//! rrk-txt：TXT 适配器（方案 §3.4，完整实现）。
//!
//! - **编码三级判定**：BOM → UTF-8 合法性校验 → chardetng 采样（GB18030/GBK/Big5…）；
//! - **流式分章**：只扫每行行首跑分章状态机（预置正则族的手写等价实现），记录字节偏移不复制字符串；
//! - **逻辑块**：巨章按段落边界切块，每块独立成 `ChapterEntry`（`part` > 0）；
//! - **段落切分**：空行分段、全角缩进识别，行内样式极少（单 `StyleFlags`）。

use std::sync::Arc;

use rrk_format::{BookSource, ContentSource, FormatAdapter};
use rrk_model::{
    fnv1a64, Block, BookMeta, ChapterContent, ChapterEntry, ChapterId, ChapterKind, FormatId,
    Inline, Resource, ResourceKey, RrkError, RrkResult, StyleFlags, TextRange, TocNode,
};

/// 单个逻辑块的最大字节上限（方案 §3.4：巨章切块，内存与章大小解耦）。
const LOGICAL_BLOCK_BYTES: usize = 512 * 1024;

/// 章标题行最大长度（超过不认为是标题）。
const HEADING_MAX_CHARS: usize = 60;

// ---------------------------------------------------------------------------
// 编码三级判定
// ---------------------------------------------------------------------------

/// 检测结果：编码名 + 是否带 BOM。
#[derive(Debug, Clone, Copy)]
pub struct EncodingGuess {
    pub label: &'static str,
    utf16le: bool,
    utf16be: bool,
    utf8: bool,
}

/// 三级判定：BOM → UTF-8 合法性 → chardetng 采样。
pub fn detect_encoding(head: &[u8]) -> EncodingGuess {
    // 1. BOM
    if head.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return EncodingGuess { label: "UTF-8(BOM)", utf16le: false, utf16be: false, utf8: true };
    }
    if head.starts_with(&[0xFF, 0xFE]) {
        return EncodingGuess { label: "UTF-16LE(BOM)", utf16le: true, utf16be: false, utf8: false };
    }
    if head.starts_with(&[0xFE, 0xFF]) {
        return EncodingGuess { label: "UTF-16BE(BOM)", utf16le: false, utf16be: true, utf8: false };
    }
    // 2. UTF-8 合法性（严格校验采样）
    if std::str::from_utf8(head).is_ok() {
        return EncodingGuess { label: "UTF-8", utf16le: false, utf16be: false, utf8: true };
    }
    // 3. chardetng 采样判定
    let mut detector = chardetng::EncodingDetector::new();
    detector.feed(head, true);
    let enc = detector.guess(None, true);
    let label: &'static str = if enc == encoding_rs::UTF_8 {
        "UTF-8(chardetng)"
    } else if enc == encoding_rs::GBK {
        "GB18030/GBK"
    } else if enc == encoding_rs::BIG5 {
        "Big5"
    } else if enc == encoding_rs::UTF_16LE {
        "UTF-16LE"
    } else if enc == encoding_rs::UTF_16BE {
        "UTF-16BE"
    } else {
        "chardetng-other"
    };
    EncodingGuess {
        label,
        utf16le: enc == encoding_rs::UTF_16LE,
        utf16be: enc == encoding_rs::UTF_16BE,
        utf8: enc == encoding_rs::UTF_8,
    }
}

fn decode_text(bytes: &[u8], guess: EncodingGuess) -> RrkResult<String> {
    let text = if guess.utf8 {
        // 去 BOM 后严格解码；非法序列退 chardetng 路径由调用方处理
        let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
        match std::str::from_utf8(body) {
            Ok(s) => s.to_string(),
            Err(_) => decode_with_charset(bytes, None),
        }
    } else if guess.utf16le {
        decode_with_charset(bytes, Some(encoding_rs::UTF_16LE))
    } else if guess.utf16be {
        decode_with_charset(bytes, Some(encoding_rs::UTF_16BE))
    } else {
        decode_with_charset(bytes, None)
    };
    Ok(text)
}

fn decode_with_charset(bytes: &[u8], forced: Option<&'static encoding_rs::Encoding>) -> String {
    let enc = forced.unwrap_or_else(|| {
        // 只采样头部 64KB 判定（禁止整书喂检测器——8MB 级会到秒级）
        let mut detector = chardetng::EncodingDetector::new();
        detector.feed(&bytes[..bytes.len().min(64 * 1024)], true);
        detector.guess(None, true)
    });
    let (cow, _encoding_used, _had_errors) = enc.decode(bytes);
    cow.into_owned()
}

// ---------------------------------------------------------------------------
// 分章状态机（行首采样，手写模式匹配，零正则依赖）
// ---------------------------------------------------------------------------

fn is_numeral(c: char) -> bool {
    matches!(c,
        '0'..='9'
        | '〇' | '零' | '一' | '二' | '三' | '四' | '五' | '六' | '七' | '八' | '九' | '十'
        | '百' | '千' | '万' | '两' | '壹' | '贰' | '叁' | '肆' | '伍' | '陆' | '柒' | '捌'
        | '玖' | '拾' | '佰' | '仟' | '萬')
}

fn is_unit(c: char) -> bool {
    matches!(c, '章' | '回' | '节' | '卷' | '部' | '篇' | '幕' | '话' | '集')
}

/// 卷/部 级标题（可作为「章」处理，P0 平铺）。
fn is_volume_unit(c: char) -> bool {
    matches!(c, '卷' | '部' | '辑')
}

const SPECIAL_TITLES: &[&str] = &[
    "序", "序章", "前言", "引子", "引言", "楔子", "尾声", "后记", "后序", "番外", "附录", "跋",
    "终章", "终卷", "开篇", "结语", "番外一", "番外二", "番外三",
];

/// 判断一行是否为章节标题；是则返回清洗后的标题。
///
/// 规则（方案 §3.4「行首采样 + 状态机」的实现口径）：
/// 1. `第X章/回/节/卷/部/篇/幕/话/集`（X 为数字或中文数字）；
/// 2. `卷/部X` 开头；
/// 3. `Chapter/Part N`（ASCII）；
/// 4. 特殊标题词（序/楔子/尾声…）；
/// 5. 行长 ≤ 60 字符，且不以句读结尾（防误判正文句）。
pub fn match_heading(line: &str) -> Option<String> {
    let trimmed = line.trim().trim_start_matches('\u{feff}');
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() > HEADING_MAX_CHARS {
        return None;
    }
    // 正文句不作为标题
    if trimmed.ends_with(['。', '！', '？', '…', '”', '』', '」', '.', '!', '?']) {
        return None;
    }

    let chars: Vec<char> = trimmed.chars().collect();

    // 1) 第X<unit>
    if chars[0] == '第' {
        let mut i = 1;
        let mut n = 0;
        while i < chars.len() && is_numeral(chars[i]) {
            i += 1;
            n += 1;
        }
        if n > 0 && i < chars.len() && is_unit(chars[i]) {
            return Some(trimmed.to_string());
        }
        return None;
    }

    // 2) 卷/部X …（至少一个数字跟在单位后）
    if is_volume_unit(chars[0]) {
        let mut i = 1;
        let mut n = 0;
        while i < chars.len() && is_numeral(chars[i]) {
            i += 1;
            n += 1;
        }
        if n > 0 {
            return Some(trimmed.to_string());
        }
        return None;
    }

    // 3) Chapter/Part N
    let lower = trimmed.to_ascii_lowercase();
    for prefix in ["chapter", "part", "section"] {
        if lower.starts_with(prefix) {
            let rest = &trimmed[prefix.len()..];
            if rest.trim_start().chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
                return Some(trimmed.to_string());
            }
        }
    }

    // 4) 特殊标题
    for t in SPECIAL_TITLES {
        if trimmed == *t {
            return Some(trimmed.to_string());
        }
        // 「楔子：xxx」「番外 · xxx」等短变体
        if trimmed.starts_with(t) {
            let rest = &trimmed[t.len()..];
            if rest.starts_with(['：', ':', ' ', '·', '—', '-']) && rest.chars().count() <= 20 {
                return Some(trimmed.to_string());
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// TXT 书源
// ---------------------------------------------------------------------------

/// TXT 书源：整书解码为 `Arc<str>`，章索引为字节范围表（O(1) 定位）。
pub struct TxtBook {
    meta: BookMeta,
    text: Arc<str>,
    entries: Vec<ChapterEntry>,
}

/// 一行的位置信息（解码后文本内的字节偏移）。
struct LineHit {
    start: usize,
    end: usize, // 不含换行
    is_heading: Option<String>,
}

fn scan_lines(text: &str) -> Vec<LineHit> {
    let mut hits = Vec::new();
    let mut start = 0usize;
    for (idx, ch) in text.char_indices() {
        if ch == '\n' {
            let mut end = idx;
            // 剔除 \r
            if end > start && text.as_bytes()[end - 1] == b'\r' {
                end -= 1;
            }
            let line = &text[start..end];
            let is_heading = match_heading(line);
            hits.push(LineHit { start, end, is_heading });
            start = idx + ch.len_utf8();
        }
    }
    if start < text.len() {
        let line = &text[start..];
        let is_heading = match_heading(line);
        hits.push(LineHit { start, end: text.len(), is_heading });
    }
    hits
}

fn count_chars(text: &str) -> u32 {
    text.chars().count() as u32
}

impl TxtBook {
    /// 解码 + 分章 + 逻辑块切分（导入期一次完成，之后打开零扫描）。
    pub fn parse(src: &ContentSource) -> RrkResult<Self> {
        let bytes = src.read_all()?;
        if bytes.len() > 512 * 1024 * 1024 {
            return Err(RrkError::new(
                rrk_model::RrkErrorKind::ResourceLimit,
                "TXT 超过 512MB 上限",
            ));
        }
        let head = &bytes[..bytes.len().min(64 * 1024)];
        let guess = detect_encoding(head);
        let text = decode_text(&bytes, guess)?;

        let title = src
            .name()
            .map(|n| n.rsplit_once('.').map(|(s, _)| s).unwrap_or(n).to_string())
            .unwrap_or_else(|| "未命名".to_string());

        let entries = build_entries(&text);

        let meta = BookMeta {
            title,
            authors: Vec::new(),
            language: None,
            format: FormatId::Txt,
            content_hash: fnv1a64(&bytes[..bytes.len().min(1024 * 1024)]),
            cover: None,
        };

        Ok(Self { meta, text: Arc::from(text), entries })
    }

    /// 章 → 正文切片（含标题行时不含标题行本体）。
    fn slice_of(&self, entry: &ChapterEntry) -> &str {
        let (s, e) = entry.byte_range;
        &self.text[s as usize..e as usize]
    }
}

/// 由扫描结果构建章索引（含巨章逻辑块切分）。
fn build_entries(text: &str) -> Vec<ChapterEntry> {
    let lines = scan_lines(text);
    let mut spans: Vec<(usize, usize, Option<String>)> = Vec::new(); // (body_start, body_end, title)

    let heading_idx: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.is_heading.is_some())
        .map(|(i, _)| i)
        .collect();

    if heading_idx.is_empty() {
        // 无任何标题：整本一个逻辑块链
        spans.push((0, text.len(), None));
    } else {
        // 开头无标题的部分（序前内容）单独成章
        if heading_idx[0] > 0 {
            let end = lines[heading_idx[0]].start;
            spans.push((0, end, None));
        }
        for (k, &li) in heading_idx.iter().enumerate() {
            let body_start = lines[li].end + 1; // 跳过标题行与换行
            let body_start = body_start.min(text.len());
            let body_end = if k + 1 < heading_idx.len() {
                lines[heading_idx[k + 1]].start
            } else {
                text.len()
            };
            if body_end < body_start {
                continue;
            }
            spans.push((body_start, body_end, lines[li].is_heading.clone()));
        }
    }

    // 逻辑块切分 + 索引构建
    // href = `txt:<起始字符偏移>`（字符偏移，与宿主历史约定一致）：
    // spans/part_spans 均按起点升序，游标增量计数即可，全书 O(n) 一次。
    let mut entries = Vec::new();
    let mut cursor = 0usize;
    let mut chars_before = 0u64;
    for (start, end, title) in spans {
        let part_spans = split_logical_blocks(text, start, end);
        let total_parts = part_spans.len() as u32;
        for (part, (s, e)) in part_spans.into_iter().enumerate() {
            chars_before += count_chars(&text[cursor..s]) as u64;
            cursor = s;
            let part = part as u32;
            let display_title = match (&title, total_parts) {
                (Some(t), 1) => Some(t.clone()),
                (Some(t), _) if part == 0 => Some(t.clone()),
                (Some(t), _) => Some(format!("{t}（续{part}）")),
                (None, _) => None,
            };
            entries.push(ChapterEntry {
                id: entries.len() as ChapterId,
                title: display_title,
                href: format!("txt:{chars_before}"),
                byte_range: (s as u64, e as u64),
                char_len: count_chars(&text[s..e]),
                kind: ChapterKind::Text,
                part,
            });
        }
    }
    entries
}

/// 巨章按段落边界切逻辑块；普通章返回单块。空体章（如卷级标题行）保留零长块。
fn split_logical_blocks(text: &str, start: usize, end: usize) -> Vec<(usize, usize)> {
    const EMPTY: Vec<(usize, usize)> = Vec::new();
    if end < start {
        return EMPTY;
    }
    if end - start <= LOGICAL_BLOCK_BYTES {
        return vec![(start, end)];
    }
    let mut spans = Vec::new();
    let mut seg_start = start;
    let mut pos = start;
    while pos < end {
        if pos - seg_start >= LOGICAL_BLOCK_BYTES {
            // 在下一个空行处切
            let mut cut = pos;
            while cut < end {
                let ch = text[cut..].chars().next().unwrap();
                if ch == '\n' {
                    cut += 1;
                    break;
                }
                cut += ch.len_utf8();
            }
            spans.push((seg_start, cut));
            seg_start = cut;
            pos = cut;
        } else {
            let ch = text[pos..].chars().next().unwrap();
            pos += ch.len_utf8();
        }
    }
    if seg_start < end {
        spans.push((seg_start, end));
    }
    spans
}

/// 行是否以句读收尾（视为段落结束，不与下一行合并）。
fn ends_paragraph(line: &str) -> bool {
    let t = line.trim_end();
    t.ends_with(['。', '！', '？', '…', '”', '’', '』', '」', '.', '!', '?'])
}

/// 章正文 → Block 流（段落切分 + 缩进识别）。
///
/// 段落边界：空行，或行尾句读（中文 TXT 常见「一行一段」）；
/// 否则视为硬折行并入同段。
fn build_blocks(body: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut para_start: Option<usize> = None;
    let mut para_end: usize = 0;
    let mut indent = false;

    let flush = |blocks: &mut Vec<Block>, s: usize, e: usize, indent: bool| {
        if e > s {
            blocks.push(Block::Paragraph {
                inlines: vec![Inline::Text {
                    range: TextRange::new(s as u32, e as u32),
                    style: StyleFlags::default(),
                }],
                indent,
            });
        }
    };

    let mut line_start = 0usize;
    let mut ends: Vec<(usize, usize)> = Vec::new(); // (start,end) 每行
    for (idx, ch) in body.char_indices() {
        if ch == '\n' {
            ends.push((line_start, idx));
            line_start = idx + 1;
        }
    }
    if line_start < body.len() {
        ends.push((line_start, body.len()));
    }

    for (ls, le) in ends {
        let line = &body[ls..le];
        let trimmed = line.trim_end();
        let content = trimmed.trim_start();
        if content.is_empty() {
            if let Some(s) = para_start.take() {
                flush(&mut blocks, s, para_end, indent);
            }
            continue;
        }
        let line_indent = trimmed.starts_with('　') || trimmed.starts_with("  ");
        if para_start.is_some() {
            // 同段续行（硬折行）：并入
            para_end = le;
            indent = indent || line_indent;
        } else {
            para_start = Some(ls + (trimmed.len() - content.len()));
            para_end = le;
            indent = line_indent;
        }
        // 行尾句读 = 段落收尾（一行一段的常见中文排版）
        if ends_paragraph(line) {
            if let Some(s) = para_start.take() {
                flush(&mut blocks, s, para_end, indent);
            }
        }
    }
    if let Some(s) = para_start.take() {
        flush(&mut blocks, s, para_end, indent);
    }
    blocks
}

// ---------------------------------------------------------------------------
// FormatAdapter / BookSource
// ---------------------------------------------------------------------------

/// TXT 适配器。
pub struct TxtAdapter;

impl FormatAdapter for TxtAdapter {
    fn id(&self) -> FormatId {
        FormatId::Txt
    }

    fn sniff(&self, head: &[u8], name: Option<&str>) -> u32 {
        // 已知非纯文本魔数直接排除，让位于对应格式（或「已识别不支持」的明确报错）
        const MAGIC_REJECT: &[&[u8]] = &[
            b"%PDF",          // PDF
            b"PK\x03\x04",    // zip 系（EPUB/CBZ）
            b"Rar!\x1a\x07",  // RAR（CBR）
            b"\x89PNG",       // PNG
            b"\xff\xd8\xff",  // JPEG
            b"GIF8",          // GIF
            b"\x1f\x8b",      // gzip
        ];
        if MAGIC_REJECT.iter().any(|m| head.starts_with(m)) {
            return 0;
        }
        if head.len() >= 60 {
            let palm = &head[60..68];
            if palm == b"BOOKMOBI" || palm == b"TEXtREAd" {
                return 0; // MOBI/AZW3
            }
        }
        // 二进制特征（大量 NUL）直接排除
        let sample = &head[..head.len().min(2048)];
        let nul = sample.iter().filter(|&&b| b == 0).count();
        if nul > sample.len() / 8 {
            return 0;
        }
        let by_name = name
            .map(|n| {
                let lower = n.to_ascii_lowercase();
                if lower.ends_with(".txt") || lower.ends_with(".text") {
                    40
                } else {
                    0
                }
            })
            .unwrap_or(0);
        // 纯文本兜底：可打印/常见字符占比高
        if by_name == 0 {
            let ok = sample.iter().filter(|&&b| b == b'\n' || b == b'\r' || b == b'\t' || (0x20..0x7F).contains(&b) || b >= 0x80).count();
            if !sample.is_empty() && ok * 100 / sample.len() > 90 {
                return 10;
            }
        }
        by_name
    }

    fn open(&self, src: &ContentSource) -> RrkResult<Box<dyn BookSource>> {
        Ok(Box::new(TxtBook::parse(src)?))
    }
}

impl BookSource for TxtBook {
    fn meta(&self) -> &BookMeta {
        &self.meta
    }

    fn toc(&self) -> RrkResult<Vec<TocNode>> {
        Ok(self
            .entries
            .iter()
            .filter(|e| e.part == 0)
            .map(|e| TocNode {
                title: e.title.clone().unwrap_or_else(|| "（无标题）".to_string()),
                chapter: e.id,
                anchor: None,
                children: Vec::new(),
            })
            .collect())
    }

    fn chapters(&self) -> RrkResult<Vec<ChapterEntry>> {
        Ok(self.entries.clone())
    }

    fn load_chapter(&self, id: ChapterId) -> RrkResult<ChapterContent> {
        let entry = self
            .entries
            .iter()
            .find(|e| e.id == id)
            .ok_or_else(|| RrkError::invalid(format!("章 id 越界: {id}")))?;
        let body = self.slice_of(entry);
        // ChapterContent.text 的偏移独立于全书文本：重建局部文本
        let text: Arc<str> = Arc::from(body);
        let blocks = build_blocks(&text);
        Ok(ChapterContent { id, text, blocks })
    }

    fn load_resource(&self, key: &ResourceKey) -> RrkResult<Resource> {
        Err(RrkError::invalid(format!("TXT 无资源: {}", key.0)))
    }

    fn cover(&self) -> RrkResult<Option<Resource>> {
        Ok(None)
    }

    fn estimate_weights(&self) -> RrkResult<Vec<(ChapterId, u32)>> {
        Ok(self.entries.iter().map(|e| (e.id, e.char_len)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heading_patterns() {
        assert!(match_heading("第一章 开始").is_some());
        assert!(match_heading("第12章").is_some());
        assert!(match_heading("第一百零一回 风起").is_some());
        assert!(match_heading("卷三 北行").is_some());
        assert!(match_heading("Chapter 3: The Door").is_some());
        assert!(match_heading("楔子").is_some());
        assert!(match_heading("尾声").is_some());
        assert!(match_heading("　　第三章 试探").is_some());
        // 正文句不误判
        assert!(match_heading("第三章的故事讲完了。").is_none());
        assert!(match_heading("他抬起头看了看天，心里想。").is_none());
        assert!(match_heading(&"很长".repeat(80)).is_none());
    }

    #[test]
    fn gbk_detection_and_decode() {
        let (gbk_bytes, _, _) = encoding_rs::GBK.encode("第一章 测试\n这是正文内容。");
        let guess = detect_encoding(&gbk_bytes);
        assert!(!guess.utf8);
        let text = decode_text(&gbk_bytes, guess).unwrap();
        assert!(text.contains("第一章"));
        assert!(text.contains("正文"));
    }

    #[test]
    fn utf8_detection() {
        let text = "第一章\n你好世界";
        let guess = detect_encoding(text.as_bytes());
        assert!(guess.utf8);
    }

    #[test]
    fn split_chapters_and_blocks() {
        let src = ContentSource::from_bytes(
            Some("t.txt".into()),
            Arc::<[u8]>::from("第1章 甲\n\n正文一。\n正文一续。\n\n第2章 乙\n\n正文二。\n".as_bytes()),
        );
        let book = TxtBook::parse(&src).unwrap();
        let chapters = book.chapters().unwrap();
        assert_eq!(chapters.len(), 2);
        assert_eq!(chapters[0].title.as_deref(), Some("第1章 甲"));
        let c0 = book.load_chapter(0).unwrap();
        assert!(c0.text.contains("正文一"));
        // 每行以句读收尾 = 一行一段
        let paras = c0.blocks.iter().filter(|b| matches!(b, Block::Paragraph { .. })).count();
        assert_eq!(paras, 2);
    }

    #[test]
    fn volume_heading_kept_as_empty_entry() {
        let src = ContentSource::from_bytes(
            Some("t.txt".into()),
            Arc::<[u8]>::from("第1卷 风起\n第1章 甲\n\n正文。\n".as_bytes()),
        );
        let book = TxtBook::parse(&src).unwrap();
        let chapters = book.chapters().unwrap();
        assert_eq!(chapters.len(), 2);
        assert_eq!(chapters[0].title.as_deref(), Some("第1卷 风起"));
        assert_eq!(chapters[0].char_len, 0);
        assert_eq!(chapters[1].title.as_deref(), Some("第1章 甲"));
    }

    #[test]
    fn chapter_href_is_txt_char_offset_of_body_start() {
        let src = ContentSource::from_bytes(
            Some("t.txt".into()),
            Arc::<[u8]>::from("第1章 甲\n\n正文。\n第2章 乙\n\n更多正文。\n".as_bytes()),
        );
        let book = TxtBook::parse(&src).unwrap();
        let chapters = book.chapters().unwrap();
        assert_eq!(chapters.len(), 2);
        let mut prev = None;
        for e in &chapters {
            let off: usize = e
                .href
                .strip_prefix("txt:")
                .expect("TXT href 前缀固定 txt:")
                .parse()
                .expect("txt: 后必须是数字");
            // 契约：数字 = 该章 body 起点（byte_range.0）在全文中的**字符**偏移。
            let expect = book.text[..e.byte_range.0 as usize].chars().count();
            assert_eq!(off, expect);
            // 且随章序严格递增（进度身份唯一）。
            if let Some(p) = prev {
                assert!(off > p, "href 字符偏移必须严格递增");
            }
            prev = Some(off);
        }
    }

    #[test]
    fn hard_wrapped_lines_merge_into_one_paragraph() {
        let src = ContentSource::from_bytes(
            Some("t.txt".into()),
            Arc::<[u8]>::from("第1章 甲\n\n这是一段被硬折行\n切成了两行的正文。\n".as_bytes()),
        );
        let book = TxtBook::parse(&src).unwrap();
        let c0 = book.load_chapter(0).unwrap();
        let paras = c0.blocks.iter().filter(|b| matches!(b, Block::Paragraph { .. })).count();
        assert_eq!(paras, 1);
    }
}
