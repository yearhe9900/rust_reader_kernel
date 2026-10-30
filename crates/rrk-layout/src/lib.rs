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

use std::borrow::Cow;

use rrk_model::{
    Align, Block, ChapterContent, ChapterId, Inline, LayoutPage, LineEntry, PlacedImage,
    RrkError, RrkErrorKind, RrkResult, SemanticPos, StyleSpec, TextRun, Viewport,
};

use parley::{
    Alignment, AlignmentOptions, FontContext, FontStack, FontStyle, FontWeight, LayoutContext,
    LineHeight, PositionedLayoutItem, StyleProperty,
};

/// 排版器：持字体库与排版 scratch（方案 §4.1「粗粒度共享」，勿逐页新建）。
pub struct Layouter {
    font_cx: FontContext,
    layout_cx: LayoutContext,
    /// 块局部文本缓冲（跨块复用，免每块零容量起步的堆分配）。
    text_buf: String,
    /// 块局部样式 span 缓冲。
    spans_buf: Vec<(usize, usize, bool, bool)>,
}

impl Default for Layouter {
    fn default() -> Self {
        Self::new()
    }
}

impl Layouter {
    pub fn new() -> Self {
        Self {
            font_cx: FontContext::new(),
            layout_cx: LayoutContext::new(),
            text_buf: String::new(),
            spans_buf: Vec::new(),
        }
    }

    /// 整章排版：Block 流 → 行序列 → 按视口分页（不可取消的便捷入口）。
    pub fn layout(
        &mut self,
        content: &ChapterContent,
        style: &StyleSpec,
        vp: &Viewport,
    ) -> Vec<LayoutPage> {
        const NEVER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        self.layout_cancellable(content, style, vp, &NEVER)
            .expect("无取消标志时 layout_cancellable 不会失败")
    }

    /// 整章排版（**可取消**，第三方接入面）：块间检查取消标志；
    /// 已取消 → `Err(Cancelled=1005)`，不留半成品页表。
    ///
    /// 取消粒度为块（百万字长章的一个段落内不响应），延迟上界 = 单块 shaping 时间。
    pub fn layout_cancellable(
        &mut self,
        content: &ChapterContent,
        style: &StyleSpec,
        vp: &Viewport,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> RrkResult<Vec<LayoutPage>> {
        let width = vp.width.max(1.0);
        let mut flat: Vec<FlatLine> = Vec::new();

        for block in &content.blocks {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(RrkError::new(RrkErrorKind::Cancelled, "排版已取消"));
            }
            match block {
                Block::Paragraph { inlines, indent } => {
                    self.layout_inlines(content, inlines, style, false, *indent, width, 1.0, &mut flat);
                }
                Block::Heading { level, inlines } => {
                    // 只缩放字号（旧实现 clone 整个 StyleSpec：每次标题一次字体族 String 分配）
                    self.layout_inlines(
                        content,
                        inlines,
                        style,
                        true,
                        false,
                        width,
                        heading_scale(*level),
                        &mut flat,
                    );
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
                            self.layout_inlines(content, inlines, style, false, true, width, 1.0, &mut flat);
                        }
                    }
                }
                Block::Divider => {
                    flat.push(FlatLine::Gap { height: style.font_size * 1.2 });
                }
            }
        }

        Ok(paginate(flat, content.id, vp.height.max(1.0)))
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
        scale: f32,
        out: &mut Vec<FlatLine>,
    ) {
        // 复用排版器 scratch（解构出字段借用，免每块 2 次零容量堆分配）
        let Self { font_cx, layout_cx, text_buf, spans_buf, .. } = self;
        let text = text_buf;
        text.clear();
        spans_buf.clear();
        let mut base_offset: Option<u32> = None;
        for inline in inlines {
            if let Inline::Text { range, style: flags } = inline {
                base_offset = Some(base_offset.map_or(range.start, |b: u32| b.min(range.start)));
                let start = text.len();
                text.push_str(range.slice(&content.text));
                spans_buf.push((start, text.len(), flags.is_bold(), flags.is_italic()));
            }
        }
        let Some(base_offset) = base_offset else { return };
        if text.trim().is_empty() {
            return;
        }
        let font_size = style.font_size * scale;

        // 首行缩进：em → 前缀空格（全角 1em / 半角 en space 0.5em），
        // 让断行宽度真实扣除缩进；同时记下缩进像素宽，供 run.x 补偿（见下）。
        let (prefix, indent_px) = if indent {
            indent_metrics(style.para_indent_em, font_size)
        } else {
            (String::new(), 0.0)
        };
        // 无缩进时零拷贝借用块文本（旧实现无条件整段 format! 拷贝）
        let full: Cow<'_, str> = if prefix.is_empty() {
            Cow::Borrowed(text.as_str())
        } else {
            Cow::Owned(format!("{prefix}{text}"))
        };
        let shift = prefix.len() as u32;

        let mut builder = layout_cx.ranged_builder(font_cx, &full, 1.0, true);
        builder.push_default(StyleProperty::FontSize(font_size));
        if !style.font_family.is_empty() {
            builder.push_default(StyleProperty::FontStack(FontStack::from(style.font_family.as_str())));
        }
        builder.push_default(StyleProperty::LineHeight(LineHeight::FontSizeRelative(style.line_height)));
        if is_heading {
            builder.push_default(StyleProperty::FontWeight(FontWeight::new(700.0)));
        }
        for &(s, e, bold, italic) in spans_buf.iter() {
            // span 偏移基于块文本，而 builder 文本是 `full`（prefix + text）：
            // 必须右移 prefix 字节数（旧实现未移位，缩进段落的粗体/斜体
            // 会错位到前缀区域内）。
            let (s, e) = (s + shift as usize, e + shift as usize);
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
                Align::Justify => Alignment::Justified,
            },
            AlignmentOptions::default(),
        );

        let gap_after = font_size * 0.35;
        for line in layout.lines() {
            let metrics = *line.metrics();
            let text_range = line.text_range();
            let mut runs = Vec::with_capacity(2);
            for item in line.items() {
                if let PositionedLayoutItem::GlyphRun(gr) = item {
                    let run = gr.run();
                    let range = run.text_range();
                    // 段落首行的首个 run 含缩进前缀，其 `offset()` 从 0 起算，
                    // 而导出的文本范围已扣掉前缀字节量；这里把 x 补到前缀宽度，
                    // 宿主「range → x」才能画在缩进后的位置（否则缩进在几何模式下丢失）。
                    let at_para_head = indent_px > 0.0 && text_range.start == 0 && range.start == 0;
                    let x = if at_para_head { gr.offset() + indent_px } else { gr.offset() };
                    let (bold_mask, italic_mask) =
                        FlatRun::style_masks(range.start, range.end, spans_buf);
                    runs.push(FlatRun {
                        x,
                        // parley 的 metrics.baseline 是布局坐标系（累计）——
                        // 换算成「行内基线」（baseline - 行顶），分页时再加页内行光标 y
                        baseline: metrics.baseline - metrics.min_coord,
                        font_size: run.font_size(),
                        start: base_offset + (range.start as u32).saturating_sub(shift),
                        end: base_offset + (range.end as u32).saturating_sub(shift),
                        width: run.advance(),
                        bold_mask,
                        italic_mask,
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

/// em → 首行缩进前缀（全角 1em / en space 0.5em）与对应像素宽度。
fn indent_metrics(em: f32, font_size: f32) -> (String, f32) {
    let em = em.clamp(0.0, 8.0);
    let whole = em.floor() as usize;
    let half = (em - whole as f32) >= 0.5;
    let mut prefix = "　".repeat(whole);
    if half {
        prefix.push('\u{2002}'); // EN SPACE ≈ 0.5em
    }
    let used_em = whole as f32 + if half { 0.5 } else { 0.0 };
    (prefix, used_em * font_size)
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
    /// run 像素宽度（parley advance），供 hit_test 按比例定位光标。
    width: f32,
    /// 逐字节样式覆盖：真值当且仅当该字节落在某个粗体/斜体 span 内
    /// （span 输入已知；parley 的 Run 不暴露样式，故用覆盖而非读回）。
    bold_mask: Vec<bool>,
    italic_mask: Vec<bool>,
}

impl FlatRun {
    /// 按 builder 坐标下的 run 文本范围，从 span 表构造逐字节样式覆盖。
    /// 掩码与 run 字节序列一一对应；run 落在 span 边界内时取覆盖，
    /// 跨界部分按相交字节判定（UTF-8 多字节字符只要被 span 覆盖到即算）。
    fn style_masks(
        run_start: usize,
        run_end: usize,
        spans: &[(usize, usize, bool, bool)],
    ) -> (Vec<bool>, Vec<bool>) {
        let len = run_end.saturating_sub(run_start);
        let mut bold = vec![false; len];
        let mut italic = vec![false; len];
        for &(s, e, b, i) in spans {
            let lo = s.max(run_start);
            let hi = e.min(run_end);
            if lo >= hi {
                continue;
            }
            for k in (lo - run_start)..(hi - run_start) {
                bold[k] |= b;
                italic[k] |= i;
            }
        }
        (bold, italic)
    }
}

/// 扁平行流 → 页（视口高切割）。
fn paginate(flat: Vec<FlatLine>, chapter: ChapterId, page_height: f32) -> Vec<LayoutPage> {
    let mut pages: Vec<LayoutPage> = Vec::new();
    let mut cur = new_page(0, chapter, page_height);
    let mut y = 0.0f32;
    // 已放置文本的末尾字节偏移：新页 start/end 初始化为它——纯图片/间隙页也
    // 保持 start/end 单调不退（`page_at` 二分查找的前提；旧实现纯图片页
    // start/end 恒 0，会破坏单调性导致定位错页）。
    let mut cursor = 0u32;

    for line in flat {
        let slot = match &line {
            FlatLine::Text { height, gap_after, .. } => *height + *gap_after,
            FlatLine::Image { slot, .. } => *slot,
            FlatLine::Gap { height } => *height,
        };
        if y + slot > page_height && !cur.lines.is_empty() {
            let mut next = new_page(pages.len() as u32 + 1, chapter, page_height);
            next.start = SemanticPos { chapter, char_offset: cursor };
            next.end = SemanticPos { chapter, char_offset: cursor };
            pages.push(std::mem::replace(&mut cur, next));
            y = 0.0;
        }
        match line {
            FlatLine::Text { start, end, height, para_start, runs, gap_after: _ } => {
                if cur.lines.is_empty() {
                    cur.start = SemanticPos { chapter, char_offset: start };
                }
                let line_idx = cur.lines.len() as u32;
                cur.lines.push(LineEntry {
                    start,
                    end,
                    height,
                    para_start,
                });
                for r in runs {
                    // 掩码与 run 字节序列同长；整 run 任一字节带样式即标记
                    // （parley 通常按样式边界切 run，run 内混样式极少见）。
                    let bold = r.bold_mask.iter().any(|&b| b);
                    let italic = r.italic_mask.iter().any(|&i| i);
                    cur.runs.push(TextRun {
                        line: line_idx,
                        x: r.x,
                        baseline: y + r.baseline,
                        font_size: r.font_size,
                        bold,
                        italic,
                        range: (r.start, r.end),
                        width: r.width,
                    });
                }
                cur.end = SemanticPos { chapter, char_offset: end };
                cursor = end;
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

/// `page_at`：定位语义坐标所在页下标（缓存命中免排版）。
///
/// 二分未命中时（如落在章尾之后、纯图片空区间处）回退到
/// 「`start <= pos` 的最后一页」，全都大于 pos 才回 0。
pub fn page_at(pages: &[LayoutPage], pos: SemanticPos) -> Option<usize> {
    if pages.is_empty() {
        return None;
    }
    match pages
        .binary_search_by(|p| {
            if pos.char_offset < p.start.char_offset {
                std::cmp::Ordering::Greater
            } else if pos.char_offset >= p.end.char_offset {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        }) {
        Ok(i) => Some(i),
        Err(_) => {
            let n = pages.partition_point(|p| p.start.char_offset <= pos.char_offset);
            Some(if n == 0 { 0 } else { n - 1 })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rrk_model::{Inline, StyleFlags, TextRange};

    pub(crate) fn chapter() -> ChapterContent {
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
        ChapterContent { id: 0, text, blocks, link_ranges: Vec::new() }
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

    /// 回归（深审 §三）：Bold/Italic 样式必须从 FlatRun 传导到导出的 TextRun，
    /// 否则宿主按 flag 渲染字重失效，且与 parley 按 bold 计量不一致 → 字宽错位。
    #[test]
    fn runs_propagate_bold_italic() {
        let mut l = Layouter::new();
        let text: std::sync::Arc<str> = "普通 bold 体 italic 体 收尾".into();
        // 偏移必须落在 UTF-8 char 边界上
        let b0 = "普通 ".len(); // 粗体段起点
        let b1 = b0 + "bold 体 ".len(); // 粗体段终点 / 斜体段起点
        let i1 = b1 + "italic 体 ".len(); // 斜体段终点
        assert!(text.is_char_boundary(b0) && text.is_char_boundary(b1) && text.is_char_boundary(i1));
        let blocks = vec![Block::Paragraph {
            inlines: vec![
                Inline::Text { range: TextRange::new(0, b0 as u32), style: StyleFlags::default() },
                Inline::Text {
                    range: TextRange::new(b0 as u32, b1 as u32),
                    style: StyleFlags(StyleFlags::BOLD),
                },
                Inline::Text {
                    range: TextRange::new(b1 as u32, i1 as u32),
                    style: StyleFlags(StyleFlags::ITALIC),
                },
                Inline::Text { range: TextRange::new(i1 as u32, text.len() as u32), style: StyleFlags::default() },
            ],
            indent: false,
        }];
        let c = ChapterContent { id: 0, text, blocks, link_ranges: Vec::new() };
        let vp = Viewport { width: 390.0, height: 400.0, dpr: 3.0 };
        let pages = l.layout(&c, &StyleSpec::default(), &vp);
        let runs: Vec<_> = pages.iter().flat_map(|p| p.runs.iter()).collect();
        assert!(runs.iter().any(|r| r.bold), "应存在粗体 run");
        assert!(runs.iter().any(|r| r.italic), "应存在斜体 run");
        assert!(runs.iter().any(|r| !r.bold && !r.italic), "应存在正文 run");
        // 每个 run 宽度非负；非空 run 应有正宽（供 hit_test 定位）
        assert!(runs.iter().all(|r| r.width >= 0.0));
        assert!(runs.iter().any(|r| r.width > 0.0));
    }

    /// 可取消排版：预置标志 → Cancelled（1005）；清零后正常。
    #[test]
    fn layout_cancellable_trip_and_clear() {
        let mut l = Layouter::new();
        let c = chapter();
        let style = StyleSpec::default();
        let vp = Viewport { width: 390.0, height: 400.0, dpr: 3.0 };
        let cancel = std::sync::atomic::AtomicBool::new(true);
        let err = l
            .layout_cancellable(&c, &style, &vp, &cancel)
            .expect_err("已置取消标志应报 Cancelled");
        assert_eq!(err.kind, RrkErrorKind::Cancelled);
        cancel.store(false, std::sync::atomic::Ordering::Relaxed);
        let pages = l.layout_cancellable(&c, &style, &vp, &cancel).unwrap();
        assert!(!pages.is_empty(), "取消清除后应正常排版");
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

    /// #6：para_indent_em 必须真影响首行起点（默认 2em 与 0 可测差异）。
    #[test]
    fn para_indent_em_shifts_first_line() {
        let mut l = Layouter::new();
        let c = chapter();
        let vp = Viewport { width: 390.0, height: 842.0, dpr: 3.0 };
        let mut s0 = StyleSpec::default();
        s0.para_indent_em = 0.0;
        let s2 = StyleSpec::default(); // 2.0
        let p0 = l.layout(&c, &s0, &vp);
        let p2 = l.layout(&c, &s2, &vp);
        let x0 = p0[0].runs.iter().find(|r| r.line == 0).map(|r| r.x).unwrap_or(-1.0);
        let x2 = p2[0].runs.iter().find(|r| r.line == 0).map(|r| r.x).unwrap_or(-1.0);
        assert!(x2 > x0 + 1.0, "2em 缩进应让首行起点右移：x0={x0}, x2={x2}");
        // 2em × 18px = 36px（字宽近似，容差放宽）
        assert!((x2 - x0 - 36.0).abs() < 6.0, "缩进宽度应≈2em：{x2} - {x0}");
    }

    /// #6：justify 只改行内间距，不应改变断行结构。
    #[test]
    fn align_justify_keeps_breaks() {
        let mut l = Layouter::new();
        let c = chapter();
        let vp = Viewport { width: 390.0, height: 842.0, dpr: 3.0 };
        let s = StyleSpec::default();
        let mut j = StyleSpec::default();
        j.align = Align::Justify;
        let ps = l.layout(&c, &s, &vp);
        let pj = l.layout(&c, &j, &vp);
        assert_eq!(ps.len(), pj.len());
        assert_eq!(ps[0].lines.len(), pj[0].lines.len());
    }

    #[test]
    fn indent_metrics_maps_em() {
        let (p2, w2) = indent_metrics(2.0, 18.0);
        assert_eq!(p2, "　　");
        assert!((w2 - 36.0).abs() < 0.01);
        let (p0, w0) = indent_metrics(0.0, 18.0);
        assert!(p0.is_empty());
        assert_eq!(w0, 0.0);
        let (p15, _) = indent_metrics(1.5, 18.0);
        assert_eq!(p15, "　\u{2002}");
    }
}

#[cfg(test)]
mod baseline_tests {
    use super::tests::chapter;
    use super::*;

    /// 回归：多行段落的 run 基线必须「页内单调递增且间距≈行高」——
    /// 基线若重复叠加布局累计值，第二行起逐行下坠压行（2026-10-21 真机截图 bug）。
    #[test]
    fn run_baselines_do_not_drift() {
        let mut l = Layouter::new();
        let c = chapter();
        let style = StyleSpec::default();
        let vp = Viewport { width: 390.0, height: 842.0, dpr: 3.0 };
        let pages = l.layout(&c, &style, &vp);
        for p in &pages {
            let mut prev = -1.0f32; // 页内单调（跨页 y 会归零）
            for r in &p.runs {
                if (r.baseline - prev).abs() < 1e-3 {
                    continue;
                }
                assert!(r.baseline > prev, "基线必须单调递增");
                if prev >= 0.0 {
                    let gap = r.baseline - prev;
                    assert!(gap < style.font_size * 4.0, "相邻 run 基线间距异常: {gap}");
                }
                prev = r.baseline;
            }
        }
    }
}
