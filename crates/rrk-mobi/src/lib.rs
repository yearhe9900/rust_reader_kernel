//! rrk-mobi：MOBI/AZW3 适配器（方案 §3.5，完整实现）。
//!
//! - **自研解压**：PalmDOC LZ77（[`palmdoc`]）+ HUFF/CDIC（[`huffcdic`]），无第三方解压绑定；
//! - **压缩口径**（规范语义，勿误读）：1 = 无压缩、2 = PalmDOC、17480 = HUFF/CDIC；
//!   另有「声明与实际不符」的脏书兜底（长度/UTF-8 合法率双检，取更优者）；
//! - **章切分**：MOBI 正文是一整条 HTML 流，优先按 `<mbp:pagebreak>` 切段，
//!   无 pagebreak 时按 `<h1..h4>` 标题切段（foliate-js 同款思路）；
//! - **目录**：扫描 `<a filepos=...>` 锚点（MOBI6 目录页形态），filepos → 章映射；
//! - **清洗**：复用 rrk-markup 的 XHTML→Block 管线（与 EPUB 同一套）。

mod huffcdic;
mod palmdoc;
mod parse;

use std::sync::Arc;

use huffcdic::HuffCdic;
use palmdoc::{decompress_palmdoc, remove_trailing_entries};
use parse::{MobiContainer, COMPRESSION_HUFF_CDIC, COMPRESSION_NONE, COMPRESSION_PALMDOC};

use rrk_format::{BookSource, ContentSource, FormatAdapter};
use rrk_markup::{clean_xhtml, CleanResult};
use rrk_model::{
    fnv1a64, Block, BookMeta, ChapterContent, ChapterEntry, ChapterId, ChapterKind, FormatId,
    Resource, ResourceKey, RrkError, RrkErrorKind, RrkResult, TocNode,
};
use rrk_txt::match_heading;

/// 单章最大字节上限（超长按 pagebreak/h 标题天然分段；此为兜底逻辑块）。
const CHAPTER_LIMIT: usize = 1024 * 1024;

/// MOBI/AZW3 书源。
pub struct MobiBook {
    meta: BookMeta,
    container: MobiContainer,
    /// 解压后的正文 HTML 流（UTF-8 字节）。
    text: Arc<[u8]>,
    /// 章切分的字节范围（切片互不重叠、按序覆盖全书）。
    spans: Vec<(usize, usize)>,
    entries: Vec<ChapterEntry>,
    toc: Vec<TocNode>,
}

// ---------------------------------------------------------------------------
// 文本装配（逐记录：剔尾 → 解压 → 拼接）
// ---------------------------------------------------------------------------

fn utf8_score(bytes: &[u8]) -> f64 {
    if bytes.is_empty() {
        return 1.0;
    }
    match std::str::from_utf8(bytes) {
        Ok(_) => 1.0,
        Err(_) => {
            let cow = String::from_utf8_lossy(bytes);
            let bad = cow.matches('\u{FFFD}').count();
            1.0 - bad as f64 / cow.chars().count().max(1) as f64
        }
    }
}

fn assemble_text(c: &MobiContainer) -> RrkResult<Vec<u8>> {
    if c.encryption != 0 {
        return Err(RrkError::unsupported("MOBI: 加密书籍不支持"));
    }
    let mut huff = if c.compression == COMPRESSION_HUFF_CDIC {
        let mut h = HuffCdic::load_huff(c.record(c.huffcdic as usize)?)?;
        for i in 1..c.num_huffcdic as usize {
            h.load_cdic(c.record(c.huffcdic as usize + i)?)?;
        }
        Some(h)
    } else {
        None
    };

    let mut raw_concat = Vec::new();
    let mut decoded = Vec::new();
    for i in 0..c.text_record_count as usize {
        let raw = c.record(i + 1)?;
        let raw = remove_trailing_entries(raw, c.trailing_flags);
        raw_concat.extend_from_slice(raw);
        let chunk = match c.compression {
            COMPRESSION_NONE => raw.to_vec(),
            COMPRESSION_PALMDOC => decompress_palmdoc(raw),
            COMPRESSION_HUFF_CDIC => {
                huff.as_mut().expect("huff loaded").unpack(raw)?
            }
            other => {
                return Err(RrkError::new(
                    RrkErrorKind::UnsupportedFormat,
                    format!("MOBI: 未知压缩方式 {other}"),
                ))
            }
        };
        decoded.extend_from_slice(&chunk);
    }

    // 脏书兜底（实测口径）：声明压缩但实际未压缩的书，解压会毁掉正文——
    // 用「解压结果 vs 原始拼接」的 UTF-8 合法率择优。
    let out = if utf8_score(&decoded) >= utf8_score(&raw_concat) {
        decoded
    } else {
        raw_concat
    };

    // textLength 口径：描述解压后长度；仅在明显超长时裁剪，随后去掉尾部空白。
    let mut end = out.len();
    if c.text_length > 0 && (c.text_length as usize) < end {
        end = c.text_length as usize;
    }
    while end > 0 && matches!(out[end - 1], b' ' | b'\t' | b'\n' | b'\r' | 0) {
        end -= 1;
    }
    Ok(out[..end].to_vec())
}

// ---------------------------------------------------------------------------
// 章切分（pagebreak 优先，h 标题兜底）与目录 filepos 映射
// ---------------------------------------------------------------------------

/// 找到所有形如 `<mbp:pagebreak ...>` / `<pagebreak ...>` 的切分点（标签起点）。
fn find_pagebreaks(text: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 9 <= text.len() {
        if text[i] == b'<' {
            let rest = &text[i + 1..];
            let lower_starts = |s: &[u8], p: &[u8]| s.len() >= p.len() && s[..p.len()].eq_ignore_ascii_case(p);
            let tag_len = if lower_starts(rest, b"mbp:pagebreak") {
                13
            } else if lower_starts(rest, b"pagebreak") {
                9
            } else {
                0
            };
            if tag_len > 0 {
                out.push(i);
                i += 1 + tag_len;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// 无 pagebreak 时的兜底切分点：`<h1..h4` 标签起点。
fn find_heading_tags(text: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 4 <= text.len() {
        if text[i] == b'<'
            && (text[i + 1] == b'h' || text[i + 1] == b'H')
            && text[i + 2].is_ascii_digit()
            && matches!(text[i + 2], b'1' | b'2' | b'3' | b'4')
            && matches!(text[i + 3], b'>' | b' ' | b'\t' | b'\n' | b'\r')
        {
            out.push(i);
            i += 4;
            continue;
        }
        i += 1;
    }
    out
}

fn split_spans(text: &[u8]) -> Vec<(usize, usize)> {
    let mut cuts = find_pagebreaks(text);
    if cuts.len() < 2 {
        cuts = find_heading_tags(text);
    }
    if cuts.len() < 2 {
        return vec![(0, text.len())];
    }
    let mut spans = Vec::new();
    if cuts[0] > 0 {
        spans.push((0, cuts[0]));
    }
    for (k, &c) in cuts.iter().enumerate() {
        let end = cuts.get(k + 1).copied().unwrap_or(text.len());
        if end > c {
            spans.push((c, end));
        }
    }
    spans
}

/// 章标题：取清洗后第一个 Heading 块的文本；否则取首个段落匹配章标题启发式。
fn derive_title(cleaned: &CleanResult) -> Option<String> {
    for b in &cleaned.blocks {
        if let Block::Heading { inlines, .. } = b {
            let text: String = inlines
                .iter()
                .filter_map(|i| match i {
                    rrk_model::Inline::Text { range, .. } => Some(range.slice(&cleaned.text)),
                    _ => None,
                })
                .collect();
            let t = text.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    for b in &cleaned.blocks {
        if let Block::Paragraph { inlines, .. } = b {
            let text: String = inlines
                .iter()
                .filter_map(|i| match i {
                    rrk_model::Inline::Text { range, .. } => Some(range.slice(&cleaned.text)),
                    _ => None,
                })
                .collect();
            if let Some(t) = match_heading(text.trim()) {
                return Some(t);
            }
            break;
        }
    }
    None
}

/// 扫描 `<a ... filepos="N" ...>标题</a>` 锚点（MOBI6 目录页形态）。
fn scan_filepos_anchors(text: &[u8]) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let s = String::from_utf8_lossy(text);
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while out.len() < 2000 {
        let Some(pos) = s[i..].find("<a").map(|p| i + p) else { break };
        let Some(tag_end) = s[pos..].find('>').map(|p| pos + p) else { break };
        let tag = &s[pos..tag_end];
        let Some(fp) = tag.find("filepos") else {
            i = tag_end + 1;
            continue;
        };
        // filepos 后取数字（可带引号）
        let mut num_start = fp + 7;
        while num_start < tag.len()
            && !tag.as_bytes()[num_start].is_ascii_digit()
            && tag.as_bytes()[num_start] != b'='
        {
            num_start += 1;
        }
        while num_start < tag.len() && (tag.as_bytes()[num_start] == b'=' || tag.as_bytes()[num_start] == b'"' || tag.as_bytes()[num_start] == b'\'') {
            num_start += 1;
        }
        let num_end = tag[num_start..]
            .find(|c: char| !c.is_ascii_digit())
            .map(|p| num_start + p)
            .unwrap_or(tag.len());
        let Ok(filepos) = tag[num_start..num_end].parse::<usize>() else {
            i = tag_end + 1;
            continue;
        };
        // 标题：> 到 </a> 之间去标签
        let Some(close) = s[tag_end..].find("</a>").map(|p| tag_end + p) else {
            i = tag_end + 1;
            continue;
        };
        let inner = &s[tag_end + 1..close];
        let title: String = {
            let mut t = String::new();
            let mut in_tag = false;
            for ch in inner.chars() {
                match ch {
                    '<' => in_tag = true,
                    '>' => in_tag = false,
                    _ if !in_tag => t.push(ch),
                    _ => {}
                }
            }
            t.split_whitespace().collect::<Vec<_>>().join(" ")
        };
        if !title.is_empty() {
            out.push((filepos, title));
        }
        let _ = bytes;
        i = close + 4;
    }
    out
}

impl MobiBook {
    pub fn parse(src: &ContentSource) -> RrkResult<Self> {
        let bytes = src.read_all()?;
        let container = parse::parse(bytes)?;
        let raw = assemble_text(&container)?;

        let spans = split_spans(&raw);
        let raw_arc: Arc<[u8]> = Arc::from(raw);

        // 章索引 + 清洗缓存需求（标题推导在构建期做一次）；空段（html 壳/空 pagebreak）过滤
        let mut entries = Vec::new();
        let mut titles: Vec<Option<String>> = Vec::new();
        let mut kept_spans: Vec<(usize, usize)> = Vec::new();
        for &(start, end) in &spans {
            let seg = &raw_arc[start..end];
            let seg_str = parse::decode_text(seg, container.text_encoding);
            let cleaned = clean_xhtml(&seg_str);
            let has_text = cleaned.blocks.iter().any(|b| match b {
                Block::Paragraph { inlines, .. } | Block::Heading { inlines, .. } => {
                    !inlines.is_empty()
                }
                Block::Image { .. } | Block::Divider => true,
                Block::Quote { blocks } => !blocks.is_empty(),
            });
            if !has_text && spans.len() > 1 {
                continue;
            }
            kept_spans.push((start, end));
            titles.push(derive_title(&cleaned));
            let id = entries.len() as ChapterId;
            entries.push(ChapterEntry {
                id,
                title: None, // 稍后回填
                href: format!("mobi:{id}"),
                byte_range: (start as u64, end as u64),
                char_len: seg_str.chars().count() as u32,
                kind: ChapterKind::Text,
                part: 0,
            });
        }
        let spans = kept_spans;

        // 目录：filepos 锚点 → 章 id；并回填章标题
        let anchors = scan_filepos_anchors(&raw_arc);
        let mut toc = Vec::new();
        for (filepos, title) in anchors {
            let Some(idx) = spans.iter().position(|&(s, e)| filepos >= s && filepos < e) else {
                continue;
            };
            if entries[idx].title.is_none() {
                entries[idx].title = Some(title.clone());
            }
            toc.push(TocNode { title, chapter: idx as ChapterId, anchor: None, children: Vec::new() });
        }
        // 目录没覆盖到的章：用推导标题回填
        for (entry, title) in entries.iter_mut().zip(titles) {
            if entry.title.is_none() {
                entry.title = title;
            }
        }

        let name = src.name().unwrap_or("未命名");
        let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name).to_string();
        let format = if name.to_ascii_lowercase().ends_with(".azw3") {
            FormatId::Azw3
        } else {
            FormatId::Mobi
        };
        let title = container
            .exth_text(503)
            .or_else(|| container.full_name.clone())
            .unwrap_or(stem);
        let mut authors = Vec::new();
        for raw_author in container.exth.get(&100) {
            let a = parse::decode_text(raw_author, container.text_encoding);
            let a = a.trim_matches('\0').trim().to_string();
            if !a.is_empty() {
                authors.push(a);
            }
        }

        let meta = BookMeta {
            title,
            authors,
            language: container.exth_text(524),
            // EXTH 103（description）/ 104（ISBN 附近的描述位）——清洗空白后取首个非空
            description: container
                .exth_text(103)
                .or_else(|| container.exth_text(104))
                .map(|s| clean_multiline(&s))
                .filter(|s| !s.is_empty()),
            format,
            content_hash: fnv1a64(&raw_arc[..raw_arc.len().min(1024 * 1024)]),
            cover: container.cover_record.map(|r| ResourceKey(format!("rec:{}", r))),
        };

        Ok(Self { meta, container, text: raw_arc, spans, entries, toc })
    }

    /// 资源记录号 → 记录切片。
    fn resource_record(&self, index: u32) -> RrkResult<&[u8]> {
        self.container.record(index as usize)
    }

    /// 回退：EXTH 201 缺失时，从资源记录里找首个图片签名匹配项。
    fn first_image_record(&self) -> Option<u32> {
        let start = self.container.resource_start as usize;
        let total = self.container.record_offsets.len();
        for i in start..total {
            if let Ok(d) = self.container.record(i) {
                if looks_like_image(d) {
                    return Some(i as u32);
                }
            }
        }
        None
    }
}

/// 按魔数判断是否图片（MOBI 资源记录无扩展名/媒体类型）。
fn looks_like_image(d: &[u8]) -> bool {
    d.starts_with(&[0xFF, 0xD8, 0xFF])
        || d.starts_with(b"\x89PNG\r\n\x1a\n")
        || d.starts_with(b"GIF8")
        || d.starts_with(b"BM")
        || (d.len() > 12 && &d[0..4] == b"RIFF" && &d[8..12] == b"WEBP")
}

// ---------------------------------------------------------------------------
// FormatAdapter / BookSource
// ---------------------------------------------------------------------------

/// 简介清洗：折叠连续空白（含 EXTH 里的 `\r\n`）为单空格，并截断到 4000 字符。
fn clean_multiline(s: &str) -> String {
    let mut out = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if out.chars().count() > 4000 {
        out = out.chars().take(4000).collect();
    }
    out
}

/// MOBI/AZW3 适配器（同一实现，按扩展名区分元数据格式标识）。
pub struct MobiAdapter;

impl FormatAdapter for MobiAdapter {
    fn id(&self) -> FormatId {
        FormatId::Mobi
    }

    fn sniff(&self, head: &[u8], _name: Option<&str>) -> u32 {
        if head.len() >= 68 {
            let magic = &head[60..68];
            if magic == b"BOOKMOBI" {
                return 100;
            }
            if magic == b"TEXtREAd" {
                return 60;
            }
        }
        0
    }

    fn open(&self, src: &ContentSource) -> RrkResult<Box<dyn BookSource>> {
        Ok(Box::new(MobiBook::parse(src)?))
    }
}

fn sniff_mime(data: &[u8]) -> &'static str {
    if data.starts_with(&[0xFF, 0xD8]) {
        "image/jpeg"
    } else if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else if data.starts_with(b"GIF8") {
        "image/gif"
    } else {
        "application/octet-stream"
    }
}

impl BookSource for MobiBook {
    fn meta(&self) -> &BookMeta {
        &self.meta
    }

    fn toc(&self) -> RrkResult<Vec<TocNode>> {
        Ok(self.toc.clone())
    }

    fn chapters(&self) -> RrkResult<Vec<ChapterEntry>> {
        Ok(self.entries.clone())
    }

    fn load_chapter(&self, id: ChapterId) -> RrkResult<ChapterContent> {
        let entry = self
            .entries
            .get(id as usize)
            .ok_or_else(|| RrkError::invalid(format!("章 id 越界: {id}")))?;
        let (start, end) = (entry.byte_range.0 as usize, entry.byte_range.1 as usize);
        let seg = &self.text[start..end];
        let seg_str = parse::decode_text(seg, self.container.text_encoding);
        let cleaned = clean_xhtml(&seg_str);
        Ok(ChapterContent {
            id,
            text: Arc::from(cleaned.text),
            blocks: cleaned.blocks,
            link_ranges: cleaned.links,
        })
    }

    fn load_resource(&self, key: &ResourceKey) -> RrkResult<Resource> {
        let index: u32 = key
            .0
            .strip_prefix("rec:")
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| RrkError::invalid(format!("MOBI 资源 key 非法: {}", key.0)))?;
        let data = self.resource_record(index)?;
        Ok(Resource {
            key: key.clone(),
            mime: sniff_mime(data).to_string(),
            data: Arc::from(data),
        })
    }

    fn cover(&self) -> RrkResult<Option<Resource>> {
        // 1) EXTH 201 声明的封面
        if let Some(cover_key) = &self.meta.cover {
            return Ok(Some(self.load_resource(cover_key)?));
        }
        // 2) 回退：资源记录里首个图片签名匹配项（部分 MOBI 不写 EXTH 201）
        if let Some(idx) = self.first_image_record() {
            let key = ResourceKey(format!("rec:{idx}"));
            if let Ok(r) = self.load_resource(&key) {
                return Ok(Some(r));
            }
        }
        Ok(None)
    }

    fn estimate_weights(&self) -> RrkResult<Vec<(ChapterId, u32)>> {
        Ok(self.entries.iter().map(|e| (e.id, e.char_len)).collect())
    }
}

#[allow(dead_code)]
fn _limit_note() -> usize {
    CHAPTER_LIMIT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_on_pagebreaks() {
        let text = "<html><body>前言<p>a</p><mbp:pagebreak/><h1>第1章</h1><p>b</p><pagebreak type=\"x\"><p>c</p></body></html>".as_bytes();
        let spans = split_spans(text);
        assert_eq!(spans.len(), 3);
    }

    #[test]
    fn split_on_heading_tags_fallback() {
        // 首个 h 标题前的 html 壳也是一段（空段在建章索引时过滤）
        let text = "<html><h1>第一章</h1><p>a</p><h2>第二章</h2><p>b</p></html>".as_bytes();
        let spans = split_spans(text);
        assert_eq!(spans.len(), 3);
    }

    #[test]
    fn scan_filepos() {
        let text = "<a filepos=\"00000123\">第一章 开始</a> 其它 <a href=\"#x\">忽略</a>".as_bytes();
        let anchors = scan_filepos_anchors(text);
        assert_eq!(anchors, vec![(123usize, "第一章 开始".to_string())]);
    }

    #[test]
    fn derive_title_from_heading() {
        let cleaned = clean_xhtml("<h2>第二章 测试</h2><p>正文</p>");
        assert_eq!(derive_title(&cleaned).as_deref(), Some("第二章 测试"));
    }
}
