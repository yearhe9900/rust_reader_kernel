//! rrk-layout：排版内核（方案 §4.1/§4.2，parley 引擎）。
//!
//! - **纯函数口径**：`layout(content, style, viewport) -> Vec<LayoutPage>`——同一输入
//!   同一输出，可缓存、可并行、可中断；
//! - **块级排版 + 跨块分页**：段落/标题逐块排出行序列，页按视口高度消费行
//!   （长段落可跨页，短段落不拆断）；
//! - **坐标口径**：`LayoutPage` 内 y 为页内坐标；run 的文本范围为章文本 UTF-8 字节偏移
//!   （进度真值坐标系）；
//! - **图片**：块级图片按尺寸提示缩放占位（解码即降采样归渲染层）。
//!
//! 引擎选型口径（方案 §4.2）：parley 先行 + 真实样张基准（`examples/bench_layout`），
//! cosmic-text 对照与逃生门（自研断行）留档。

use rrk_model::{
    Align, Block, ChapterContent, ChapterId, Inline, LayoutPage, LineEntry, PlacedImage,
    SemanticPos, StyleSpec, TextRun, Viewport,
};

use parley::{
    Alignment, AlignmentOptions, FontContext, FontStack, FontStyle, FontWeight, LayoutContext,
    LineHeight, PositionedLayoutItem, StyleProperty,
};

/// 排版器：持字体库与排版 scratch（方案 §4.1「粗粒度共享」，勿逐页新建）。
pub struct Layouter {
    font_cx: FontContext,
    layout_cx: LayoutContext,
}

impl Default for Layouter {
    fn default() -> Self {
        Self::new()
    }
}

impl Layouter {
    pub fn new() -> Self {
        Self { font_cx: FontContext::new(), layout_cx: LayoutContext::new() }
    }

    /// 整章排版：Block 流 → 行序列 → 按视口分页。
    pub fn layout(
        &mut self,
        content: &ChapterContent,
        style: &StyleSpec,
        vp: &Viewport,
    ) -> Vec<LayoutPage> {
        let width = vp.width.max(1.0);
        let mut flat: Vec<FlatLine> = Vec::new();

        for block in &content.blocks {
            match block {
                Block::Paragraph { inlines, indent } => {
                    self.layout_inlines(content, inlines, style, false, *indent, width, &mut flat);
                }
                Block::Heading { level, inlines } => {
                    let mut s = style.clone();
                    s.font_size *= heading_scale(*level);
                    self.layout_inlines(content, inlines, &s, true, false, width, &mut flat);
                }
                Block::Image { key, size_hint, .. } => {
                    let (w, h) = fit_image(*size_hint, vp, style);
                    flat.push(FlatLine::Image {
                        key: key.clone(),
                        w,
                        h,
                        slot: h + style.font_size * 0.5,
                    });
                }
                Block::Quote { blocks } => {
                    for b in blocks {
                        if let Block::Paragraph { inlines, .. } = b {
                            self.layout_inlines(content, inlines, style, false, true, width, &mut flat);
                        }
                    }
                }
                Block::Divider => {
                    flat.push(FlatLine::Gap { height: style.font_size * 1.2 });
                }
            }
        }

        paginate(flat, content.id, vp.height.max(1.0))
    }

    /// 一个块（段落/标题）→ 行序列。
    fn layout_inlines(
        &mut self,
        content: &ChapterContent,
        inlines: &[Inline],
        style: &StyleSpec,
        is_heading: bool,
        indent: bool,
        width: f32,
        out: &mut Vec<FlatLine>,
    ) {
        // 重建块局部文本 + 样式 span（章文本偏移 = base_offset + 局部偏移）
        let mut text = String::new();
        let mut spans: Vec<(usize, usize, bool, bool)> = Vec::new();
        let mut base_offset: Option<u32> = None;
        for inline in inlines {
            if let Inline::Text { range, style: flags } = inline {
                base_offset = Some(base_offset.map_or(range.start, |b: u32| b.min(range.start)));
                let start = text.len();
                text.push_str(range.slice(&content.text));
                spans.push((start, text.len(), flags.is_bold(), flags.is_italic()));
            }
        }
        let Some(base_offset) = base_offset else { return };
        if text.trim().is_empty() {
            return;
        }

        // 首行缩进：前置全角空格（CJK 语义等价 text-indent: 2em）
        let prefix = if indent { "　　" } else { "" };
        let full = format!("{prefix}{text}");
        let shift = prefix.len() as u32;

        let mut builder = self.layout_cx.ranged_builder(&mut self.font_cx, &full, 1.0, true);
        builder.push_default(StyleProperty::FontSize(style.font_size));
        if !style.font_family.is_empty() {
            builder.push_default(StyleProperty::FontStack(FontStack::from(style.font_family.as_str())));
        }
        builder.push_default(StyleProperty::LineHeight(LineHeight::FontSizeRelative(style.line_height)));
        if is_heading {
            builder.push_default(StyleProperty::FontWeight(FontWeight::new(700.0)));
        }
        for &(s, e, bold, italic) in &spans {
            if bold {
                builder.push(StyleProperty::FontWeight(FontWeight::new(700.0)), s..e);
            }
            if italic {
                builder.push(StyleProperty::FontStyle(FontStyle::Italic), s..e);
            }
        }
        let mut layout = builder.build(&full);
        layout.break_all_lines(Some(width));
        layout.align(
            Some(width),
            match style.align {
                Align::Start => Alignment::Start,
                Align::Center => Alignment::Middle,
                Align::End => Alignment::End,
            },
            AlignmentOptions::default(),
        );

        let gap_after = style.font_size * 0.35;
        for line in layout.lines() {
            let metrics = *line.metrics();
            let text_range = line.text_range();
            let mut runs = Vec::new();
            for item in line.items() {
                if let PositionedLayoutItem::GlyphRun(gr) = item {
                    let run = gr.run();
                    let range = run.text_range();
                    runs.push(FlatRun {
                        x: gr.offset(),
                        baseline: metrics.baseline,
                        font_size: run.font_size(),
                        start: base_offset + (range.start as u32).saturating_sub(shift),
                        end: base_offset + (range.end as u32).saturating_sub(shift),
                    });
                }
            }
            out.push(FlatLine::Text {
                start: base_offset + (text_range.start as u32).saturating_sub(shift),
                end: base_offset + (text_range.end as u32).saturating_sub(shift),
                height: metrics.line_height.max(metrics.max_coord - metrics.min_coord),
                para_start: text_range.start == 0,
                runs,
                gap_after: 0.0,
            });
        }
        if let Some(FlatLine::Text { gap_after: g, .. }) = out.last_mut() {
            *g = gap_after;
        }
    }
}

fn heading_scale(level: u8) -> f32 {
    match level {
        1 => 1.6,
        2 => 1.4,
        3 => 1.2,
        _ => 1.1,
    }
}

fn fit_image(size_hint: Option<(u32, u32)>, vp: &Viewport, style: &StyleSpec) -> (f32, f32) {
    let max_w = (vp.width - style.font_size * 2.0).max(1.0);
    let (iw, ih) = size_hint.map(|(w, h)| (w as f32, h as f32)).unwrap_or((max_w, max_w * 0.6));
    if iw <= 0.0 || ih <= 0.0 {
        return (max_w, style.font_size * 6.0);
    }
    let scale = (max_w / iw).min(1.0);
    (iw * scale, ih * scale)
}

/// 行的中间形态（跨块扁平化后统一分页）。
enum FlatLine {
    Text {
        start: u32,
        end: u32,
        height: f32,
        para_start: bool,
        runs: Vec<FlatRun>,
        gap_after: f32,
    },
    Image {
        key: rrk_model::ResourceKey,
        w: f32,
        h: f32,
        /// 占位总高（含段距）。
        slot: f32,
    },
    Gap {
        height: f32,
    },
}

struct FlatRun {
    x: f32,
    baseline: f32,
    font_size: f32,
    start: u32,
    end: u32,
}

/// 扁平行流 → 页（视口高切割）。
fn paginate(flat: Vec<FlatLine>, chapter: ChapterId, page_height: f32) -> Vec<LayoutPage> {
    let mut pages: Vec<LayoutPage> = Vec::new();
    let mut cur = new_page(0, chapter, page_height);
    let mut y = 0.0f32;

    for line in flat {
        let slot = match &line {
            FlatLine::Text { height, gap_after, .. } => *height + *gap_after,
            FlatLine::Image { slot, .. } => *slot,
            FlatLine::Gap { height } => *height,
        };
        if y + slot > page_height && !cur.lines.is_empty() {
            let next = new_page(pages.len() as u32 + 1, chapter, page_height);
            pages.push(std::mem::replace(&mut cur, next));
            y = 0.0;
        }
        match line {
            FlatLine::Text { start, end, height, para_start, runs, gap_after: _ } => {
                let line_idx = cur.lines.len() as u32;
                cur.lines.push(LineEntry {
                    start,
                    end,
                    height,
                    para_start,
                });
                for r in runs {
                    cur.runs.push(TextRun {
                        line: line_idx,
                        x: r.x,
                        baseline: y + r.baseline,
                        font_size: r.font_size,
                        bold: false,
                        italic: false,
                        range: (r.start, r.end),
                    });
                }
                cur.end = SemanticPos { chapter, char_offset: end };
                y += height;
            }
            FlatLine::Image { key, w, h, slot } => {
                cur.images.push(PlacedImage { key, x: 0.0, y, w, h });
                y += slot;
            }
            FlatLine::Gap { height } => y += height,
        }
    }
    if !cur.lines.is_empty() || !cur.images.is_empty() {
        pages.push(cur);
    }
    // 回填每页 start（按行表首行）
    for page in &mut pages {
        if let Some(first) = page.lines.first() {
            page.start = SemanticPos { chapter, char_offset: first.start };
        }
    }
    pages
}

fn new_page(index: u32, chapter: ChapterId, page_height: f32) -> LayoutPage {
    LayoutPage {
        index,
        chapter,
        start: SemanticPos { chapter, char_offset: 0 },
        end: SemanticPos { chapter, char_offset: 0 },
        lines: Vec::new(),
        runs: Vec::new(),
        images: Vec::new(),
        height: page_height,
    }
}

/// `page_at`：定位语义坐标所在页下标（缓存命中免排版；未命中回 0 兜底）。
pub fn page_at(pages: &[LayoutPage], pos: SemanticPos) -> Option<usize> {
    if pages.is_empty() {
        return None;
    }
    Some(
        pages
            .binary_search_by(|p| {
                if pos.char_offset < p.start.char_offset {
                    std::cmp::Ordering::Greater
                } else if pos.char_offset >= p.end.char_offset {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .unwrap_or(0)
            .min(pages.len() - 1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rrk_model::{Inline, StyleFlags, TextRange};

    fn chapter() -> ChapterContent {
        let long = "这是用于验证排版引擎分页与行切割能力的长文本内容，会折成多行并跨页。".repeat(20);
        let text: std::sync::Arc<str> = format!("{long}\n\n第二段比较短。").as_str().into();
        let blocks = vec![
            Block::Paragraph {
                inlines: vec![Inline::Text {
                    range: TextRange::new(0, long.len() as u32),
                    style: StyleFlags::default(),
                }],
                indent: true,
            },
            Block::Paragraph {
                inlines: vec![Inline::Text {
                    range: TextRange::new(long.len() as u32, text.len() as u32),
                    style: StyleFlags::default(),
                }],
                indent: true,
            },
        ];
        ChapterContent { id: 0, text, blocks }
    }

    #[test]
    fn layout_paginates() {
        let mut l = Layouter::new();
        let c = chapter();
        let style = StyleSpec::default();
        let vp = Viewport { width: 390.0, height: 400.0, dpr: 3.0 };
        let pages = l.layout(&c, &style, &vp);
        assert!(pages.len() > 1, "长文本应跨页");
        for w in pages.windows(2) {
            assert!(w[0].end.char_offset <= w[1].end.char_offset, "语义坐标单调");
        }
        assert!(pages[0].lines.iter().all(|li| li.height > 0.0));
        assert!(!pages[0].runs.is_empty());
        let idx = page_at(&pages, pages[1].start).unwrap();
        assert!(idx >= 1);
    }

    #[test]
    fn cache_roundtrip_preserves_layout() {
        let mut l = Layouter::new();
        let c = chapter();
        let style = StyleSpec::default();
        let vp = Viewport { width: 390.0, height: 400.0, dpr: 3.0 };
        let pages = l.layout(&c, &style, &vp);
        let bytes = rrk_model::encode_pages(&pages);
        let back = rrk_model::decode_pages(&bytes).unwrap();
        assert_eq!(pages, back);
    }
}
