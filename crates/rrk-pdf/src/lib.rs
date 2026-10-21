//! rrk-pdf：PDF 适配器（方案 §3.6，**L1 完整实现**：文本抽取 / 重排阅读模式）。
//!
//! 自研链路：对象扫描表 + ObjStm 合并 → 页面树 → 内容流文本算子抽取（ToUnicode 解码）
//! → 重排阅读（文本 → Block）+ `PagedSource` 页级视图。
//!
//! L1 诚实边界（方案 §3.6 分级口径）：
//! - 抽取顺序 = 内容流顺序；复杂双栏/绝对定位重排是 L3 长尾，样张驱动补；
//! - 无 ToUnicode 的 CJK 字体出占位符（`\u{FFFD}`），走降级口径不整书失败；
//! - 扫描版 PDF（纯图页）文本为空——预期行为，L2 光栅后可读图；
//! - L2 光栅（原版页位图）不在 L1 范围。

mod content;
mod objects;
mod raster;

use std::sync::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

use content::extract_text;
use objects::{Object, PdfDocument};

use rrk_format::{BookSource, ContentSource, FormatAdapter, PagedSource};
use rrk_model::{
    fnv1a64, Block, BookMeta, ChapterContent, ChapterEntry, ChapterId, ChapterKind, FormatId,
    Inline, PageEntry, PageKind, PagedContent, RenderResult, Resource, RrkError, RrkResult,
    TextRange, TocNode,
};

/// 页面信息（结构层，O(结构) 可得）。
struct PageInfo {
    content_refs: Vec<Object>,
    media: (f32, f32),
    resources: Option<Object>,
    /// 内容流压缩字节和（权重代理）。
    weight: u32,
    /// 源对象号（outline → 页映射用）。
    obj_num: u32,
}

/// PDF 书源。
pub struct PdfBook {
    meta: BookMeta,
    doc: PdfDocument,
    pages: Vec<PageInfo>,
    page_texts: Mutex<HashMap<u32, String>>,
    chapters: Vec<ChapterEntry>,
    /// 章 → 页区间 [start, end)。
    chapter_pages: Vec<(u32, u32)>,
    toc: Vec<TocNode>,
}

impl PdfBook {
    pub fn parse(src: &ContentSource) -> RrkResult<Self> {
        let bytes = src.read_all()?;
        let content_hash = fnv1a64(&bytes[..bytes.len().min(1024 * 1024)]);
        let doc = PdfDocument::open(bytes)?;

        // 目录对象：扫对象表找 /Type/Catalog；找不到则合并 ObjStm 再找
        let catalog = find_catalog(&doc).or_else(|| {
            doc.merge_objstms();
            find_catalog(&doc)
        });
        let catalog = catalog.ok_or_else(|| RrkError::corrupt("PDF: 无 Catalog"))?;

        // 页面树
        let mut pages = Vec::new();
        if let Some(pref) = catalog.get("Pages").cloned() {
            let num = match &pref {
                Object::Ref(n, _) => *n,
                _ => 0,
            };
            walk_pages(&doc, num, &doc.resolve(&pref), None, None, &mut pages);
        }
        if pages.is_empty() {
            return Err(RrkError::corrupt("PDF: 页面树为空"));
        }
        let page_index_of: HashMap<u32, u32> = pages
            .iter()
            .enumerate()
            .map(|(i, p)| (p.obj_num, i as u32))
            .collect();

        // 章：outline 顶层项切页区间；无 outline → 全书一章
        let (toc, chapter_pages, chapters_meta) =
            build_outline(&doc, &catalog, &page_index_of, pages.len());
        let chapter_pages = if chapter_pages.is_empty() {
            vec![(0, pages.len() as u32)]
        } else {
            chapter_pages
        };
        let mut chapters = Vec::new();
        for (i, &(start, end)) in chapter_pages.iter().enumerate() {
            let weight: u32 = pages[start as usize..end as usize]
                .iter()
                .map(|p| p.weight)
                .sum::<u32>()
                / 2;
            let title = chapters_meta.get(i).cloned().flatten();
            chapters.push(ChapterEntry {
                id: i as ChapterId,
                title,
                byte_range: (start as u64, end as u64),
                char_len: weight,
                kind: ChapterKind::Text,
                part: 0,
            });
        }

        let title = toc
            .first()
            .map(|n| n.title.clone())
            .filter(|t| !t.starts_with("第 "))
            .or_else(|| {
                src.name().map(|n| n.rsplit_once('.').map(|(s, _)| s).unwrap_or(n).to_string())
            })
            .unwrap_or_else(|| "未命名".to_string());

        let meta = BookMeta {
            title,
            authors: Vec::new(),
            language: None,
            format: FormatId::Pdf,
            content_hash,
            cover: None,
        };

        Ok(Self {
            meta,
            doc,
            pages,
            page_texts: Mutex::new(HashMap::new()),
            chapters,
            chapter_pages,
            toc,
        })
    }

    /// 抽取单页文本（惰性 + 缓存）。
    fn page_text(&self, index: u32) -> String {
        if let Some(t) = self.page_texts.lock().unwrap().get(&index) {
            return t.clone();
        }
        let Some(page) = self.pages.get(index as usize) else {
            return String::new();
        };
        let mut content = Vec::new();
        for r in &page.content_refs {
            if let Ok(data) = self.doc.stream_data(r) {
                content.extend_from_slice(&data);
                content.push(b'\n');
            }
        }
        let text = extract_text(&self.doc, &content, page.resources.as_ref());
        self.page_texts.lock().unwrap().insert(index, text.clone());
        text
    }

    /// 章 = 页区间文本合并 → 段落块。
    fn chapter_content(&self, id: ChapterId) -> RrkResult<ChapterContent> {
        let &(start, end) = self
            .chapter_pages
            .get(id as usize)
            .ok_or_else(|| RrkError::invalid(format!("章 id 越界: {id}")))?;
        let mut text = String::new();
        for p in start..end {
            let t = self.page_text(p);
            let t = t.trim();
            if t.is_empty() {
                continue;
            }
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(t);
        }
        let blocks = paragraphs_to_blocks(&text);
        Ok(ChapterContent { id, text: Arc::from(text), blocks })
    }

    /// L2 光栅：渲染原版页为 RGBA 位图（方案 §3.6 模式 B）。
    ///
    /// [target_width_px] 为输出像素宽；高按页比例推。失败返回错误（上层降级 L1）。
    pub fn render_page(&self, index: u32, target_width_px: u32) -> RrkResult<RenderResult> {
        let page = self
            .pages
            .get(index as usize)
            .ok_or_else(|| RrkError::invalid(format!("页越界: {index}")))?;
        let (w_pt, h_pt) = (page.media.0.max(1.0), page.media.1.max(1.0));
        let (rgba, width, height) = raster::render_page(
            &self.doc,
            &page.content_refs,
            page.resources.clone(),
            w_pt,
            h_pt,
            target_width_px,
        )?;
        Ok(RenderResult::Bitmap { width, height, rgba })
    }
}

/// 文本 → 段落 Block（空行分段；每段单 Inline 切片）。
fn paragraphs_to_blocks(text: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut segs: Vec<(usize, usize)> = Vec::new();
    for para in text.split("\n\n") {
        if para.trim().is_empty() {
            continue;
        }
        let start = para.as_ptr() as usize - text.as_ptr() as usize;
        let trimmed = para.trim();
        let lead = para.len() - para.trim_start().len();
        segs.push((start + lead, start + lead + trimmed.len()));
    }
    for (s, e) in segs {
        let trimmed = &text[s..e];
        blocks.push(Block::Paragraph {
            inlines: vec![Inline::Text {
                range: TextRange::new(s as u32, e as u32),
                style: Default::default(),
            }],
            indent: trimmed.starts_with('　') || trimmed.starts_with("  "),
        });
    }
    blocks
}

fn find_catalog(doc: &PdfDocument) -> Option<Object> {
    for &num in doc.object_nums() {
        if let Ok(obj) = doc.get(num) {
            if let Some(d) = obj.as_dict() {
                if d.get("Type").and_then(|t| t.as_name()) == Some("Catalog") {
                    return Some(obj);
                }
            }
        }
    }
    None
}

fn walk_pages(
    doc: &PdfDocument,
    obj_num: u32,
    node: &Object,
    inh_res: Option<Object>,
    inh_media: Option<Object>,
    out: &mut Vec<PageInfo>,
) {
    let Some(d) = node.as_dict() else { return };
    let res = d.get("Resources").cloned().or(inh_res);
    let media = d.get("MediaBox").cloned().or(inh_media);
    let ty = d.get("Type").and_then(|t| t.as_name()).unwrap_or("");
    if ty == "Pages" || d.contains_key("Kids") {
        if let Some(kids) = d.get("Kids").and_then(|k| k.as_array()) {
            for kid in kids {
                let num = match kid {
                    Object::Ref(n, _) => *n,
                    _ => 0,
                };
                walk_pages(doc, num, &doc.resolve(kid), res.clone(), media.clone(), out);
            }
        }
        return;
    }
    if ty == "Page" || d.contains_key("Contents") {
        let content_refs = match d.get("Contents") {
            Some(Object::Arr(a)) => a.clone(),
            Some(other) => vec![other.clone()],
            None => Vec::new(),
        };
        let mb = media
            .as_ref()
            .map(|m| doc.resolve(m))
            .and_then(|m| m.as_array().map(|a| a.to_vec()))
            .unwrap_or_default();
        let f = |o: Option<&Object>| -> f32 {
            match o {
                Some(Object::Int(i)) => *i as f32,
                Some(Object::Real(f)) => *f as f32,
                _ => 0.0,
            }
        };
        let (w, h) = if mb.len() >= 4 {
            (f(mb.get(2)) - f(mb.get(0)), f(mb.get(3)) - f(mb.get(1)))
        } else {
            (0.0, 0.0)
        };
        let weight = content_refs
            .iter()
            .filter_map(|r| match doc.resolve(r) {
                Object::Stream { data, .. } => Some(data.len() as u32),
                _ => None,
            })
            .sum();
        out.push(PageInfo {
            content_refs,
            media: (w.max(0.0), h.max(0.0)),
            resources: res,
            weight,
            obj_num,
        });
    }
}

/// outline 树 → （目录树、章页区间、章标题表）；顶层项成章。
fn build_outline(
    doc: &PdfDocument,
    catalog: &Object,
    page_index_of: &HashMap<u32, u32>,
    page_count: usize,
) -> (Vec<TocNode>, Vec<(u32, u32)>, Vec<Option<String>>) {
    let Some(outlines_ref) = catalog.get("Outlines") else {
        return (Vec::new(), Vec::new(), Vec::new());
    };
    let outlines = doc.resolve(outlines_ref);
    let Some(first) = outlines.as_dict().and_then(|d| d.get("First").cloned()) else {
        return (Vec::new(), Vec::new(), Vec::new());
    };

    let dest_page = |item: &Object| -> Option<u32> {
        let d = item.as_dict()?;
        let dest = match d.get("Dest") {
            Some(x) => doc.resolve(x),
            None => doc.resolve(
                d.get("A")
                    .and_then(|a| a.as_dict().and_then(|a| a.get("D")))
                    .unwrap_or(&Object::Null),
            ),
        };
        match dest {
            Object::Arr(a) => a
                .first()
                .and_then(|r| match doc.resolve(r) {
                    Object::Ref(n, _) => Some(n),
                    _ => None,
                })
                .and_then(|n| page_index_of.get(&n).copied()),
            _ => None,
        }
    };

    let mut items: Vec<(u32, String)> = Vec::new();
    let mut item = doc.resolve(&first);
    let mut guard = 0;
    while guard < 10_000 {
        guard += 1;
        let Some(d) = item.as_dict() else { break };
        let title = d
            .get("Title")
            .and_then(|t| t.as_str())
            .map(decode_pdf_text)
            .unwrap_or_default();
        if let Some(p) = dest_page(&item) {
            items.push((p, title));
        }
        match d.get("Next") {
            Some(n) => item = doc.resolve(n),
            None => break,
        }
    }

    items.sort_by_key(|&(p, _)| p);
    let mut toc = Vec::new();
    let mut ranges = Vec::new();
    let mut titles = Vec::new();
    for (i, &(p, ref title)) in items.iter().enumerate() {
        let end = items
            .get(i + 1)
            .map(|&(np, _)| np)
            .unwrap_or(page_count as u32)
            .max(p + 1)
            .min(page_count as u32);
        ranges.push((p, end));
        titles.push(if title.is_empty() { None } else { Some(title.clone()) });
        toc.push(TocNode {
            title: if title.is_empty() { format!("第 {} 页", p + 1) } else { title.clone() },
            chapter: i as ChapterId,
            anchor: None,
            children: Vec::new(),
        });
    }
    (toc, ranges, titles)
}

fn decode_pdf_text(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xFE, 0xFF]) {
        let units: Vec<u16> = bytes[2..]
            .chunks(2)
            .map(|c| u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    bytes.iter().map(|&b| b as char).collect()
}

// ---------------------------------------------------------------------------
// FormatAdapter / BookSource / PagedSource
// ---------------------------------------------------------------------------

/// PDF 适配器。
pub struct PdfAdapter;

impl FormatAdapter for PdfAdapter {
    fn id(&self) -> FormatId {
        FormatId::Pdf
    }

    fn sniff(&self, head: &[u8], _name: Option<&str>) -> u32 {
        if head.starts_with(b"%PDF-") {
            100
        } else {
            0
        }
    }

    fn open(&self, src: &ContentSource) -> RrkResult<Box<dyn BookSource>> {
        Ok(Box::new(PdfBook::parse(src)?))
    }
}

impl BookSource for PdfBook {
    fn meta(&self) -> &BookMeta {
        &self.meta
    }

    fn toc(&self) -> RrkResult<Vec<TocNode>> {
        Ok(self.toc.clone())
    }

    fn chapters(&self) -> RrkResult<Vec<ChapterEntry>> {
        Ok(self.chapters.clone())
    }

    fn load_chapter(&self, id: ChapterId) -> RrkResult<ChapterContent> {
        self.chapter_content(id)
    }

    fn load_resource(&self, key: &rrk_model::ResourceKey) -> RrkResult<Resource> {
        Err(RrkError::invalid(format!("PDF L1 无独立资源: {}", key.0)))
    }

    fn cover(&self) -> RrkResult<Option<Resource>> {
        Ok(None) // L2 光栅后可截首页做封面
    }

    fn estimate_weights(&self) -> RrkResult<Vec<(ChapterId, u32)>> {
        Ok(self.chapters.iter().map(|c| (c.id, c.char_len)).collect())
    }
}

impl PagedSource for PdfBook {
    fn meta(&self) -> &BookMeta {
        &self.meta
    }

    fn toc(&self) -> RrkResult<Vec<TocNode>> {
        Ok(self.toc.clone())
    }

    fn pages(&self) -> RrkResult<Vec<PageEntry>> {
        Ok(self
            .pages
            .iter()
            .enumerate()
            .map(|(i, p)| PageEntry {
                index: i as u32,
                size: p.media,
                kind: PageKind::Vector,
            })
            .collect())
    }

    fn load_page(&self, index: u32) -> RrkResult<PagedContent> {
        let text = self.page_text(index);
        let blocks = paragraphs_to_blocks(&text);
        Ok(PagedContent::Text(ChapterContent {
            id: index,
            text: Arc::from(text),
            blocks,
        }))
    }

    fn cover(&self) -> RrkResult<Option<Resource>> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paragraphs_split() {
        let text = "第一段第一行。\n\n第二段。\n\n\n第三段。";
        let blocks = paragraphs_to_blocks(text);
        assert_eq!(blocks.len(), 3);
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;

    /// L2 冒烟：最小 PDF（白底 + 黑色矩形）→ 位图非空白。
    #[test]
    fn render_minimal_page() {
        let src = b"%PDF-1.4
1 0 obj
<< /Type /Catalog /Pages 2 0 R >>
endobj
2 0 obj
<< /Type /Pages /Kids [3 0 R] /Count 1 >>
endobj
3 0 obj
<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R >>
endobj
4 0 obj
<< /Length 44 >>
stream
0 0 0 rg 20 20 100 50 re f 1 0 0 rg
endstream
endobj
";
        let doc = PdfDocument::open(std::sync::Arc::from(&src[..])).unwrap();
        let content_refs = vec![Object::Ref(4, 0)];
        let (rgba, w, h) = raster::render_page(&doc, &content_refs, None, 200.0, 200.0, 200).unwrap();
        assert_eq!((w, h), (200, 200));
        // 应有非白像素（黑矩形/红彩状态至少其一）
        let non_white = rgba.chunks(4).filter(|p| p[0] < 250 || p[1] < 250 || p[2] < 250).count();
        assert!(non_white > 500, "应画出矩形，非白像素={non_white}");
    }
}
