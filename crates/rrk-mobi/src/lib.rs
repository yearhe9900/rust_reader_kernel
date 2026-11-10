//! rrk-mobi：MOBI/AZW3 适配器（方案 §3.5，完整实现）。
//!
//! - **自研解压**：PalmDOC LZ77（[`palmdoc`]）+ HUFF/CDIC（[`huffcdic`]），无第三方解压绑定；
//! - **压缩口径**（规范语义，勿误读）：1 = 无压缩、2 = PalmDOC、17480 = HUFF/CDIC；
//!   另有「声明与实际不符」的脏书兜底（长度/UTF-8 合法率双检，取更优者）；
//! - **章切分**：MOBI 正文是一整条 HTML 流，优先按 `<mbp:pagebreak>` 切段，
//!   无 pagebreak 时按 `<h1..h4>` 标题切段（foliate-js 同款思路）；
//! - **目录**：扫描 `<a filepos=...>` 锚点（MOBI6 目录页形态），filepos → 章映射；
//! - **清洗**：复用 rrk-markup 的 XHTML→Block 管线（与 EPUB 同一套）。

mod codec;
mod huffcdic;
mod palmdoc;
mod parse;

use std::sync::Arc;

use huffcdic::HuffCdic;
use palmdoc::{decompress_palmdoc, remove_trailing_entries};
use parse::{MobiContainer, COMPRESSION_HUFF_CDIC, COMPRESSION_NONE, COMPRESSION_PALMDOC};

use rrk_cache::{CacheKey, CacheKind};
use rrk_format::{BookSource, ContentSource, FormatAdapter, IndexCache, NullIndexCache};
use rrk_markup::{attr, clean_xhtml, local_name, tokenize, CleanResult, Token};
use rrk_model::{
    Block, BookMeta, ChapterContent, ChapterEntry, ChapterId, ChapterKind, FormatId,
    Resource, ResourceKey, RrkError, RrkErrorKind, RrkResult, TocNode,
};
use rrk_txt::match_heading;

/// 单章最大字节上限（超长按 pagebreak/h 标题天然分段；此为兜底逻辑块）。
const CHAPTER_LIMIT: usize = 1024 * 1024;

/// 全书解压后正文总上限（防压缩炸弹：单记录已有预算，这里是全书闸；
/// 真实书籍正文远小于此，超限即拒而非 OOM abort）。
const MAX_TEXT_BYTES: usize = 256 * 1024 * 1024;

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
        if decoded.len() > MAX_TEXT_BYTES {
            return Err(RrkError::corrupt("MOBI: 正文解压总量超限"));
        }
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

/// NCX `content src` → filepos：`text.xhtml#0000000123` 取 `#` 后纯数字，
/// 或整体纯数字（`0000000123`）；其它形态（图片/无锚）返回 None。
fn filepos_from_src(src: &str) -> Option<usize> {
    let tail = src.rsplit('#').next().unwrap_or(src).trim();
    if tail.len() >= 4 && tail.bytes().all(|b| b.is_ascii_digit()) {
        tail.parse().ok()
    } else {
        None
    }
}

/// 解析 NCX（`EXTH 121` 指向的 DTB 记录）→ (filepos, 标题, 嵌套深度)。
///
/// KF8/AZW3 的主流目录形态（设计 §3.5："KF8 用 EXTH 121/NCX 记录"）；
/// 旧实现不解析、只靠 filepos 锚点扫描，KF8 书基本无目录（深审 §四）。
/// navPoint 嵌套保留为目录树深度；条目按 **open 序**（父先于子）输出——
/// 闭合序里子节点先于父节点，直接用会把子项错挂到顶层；畸形输入丢节点不 panic。
fn parse_ncx_toc(xml: &str) -> Vec<(usize, String, usize)> {
    // (深度, 标题, filepos)：open 时入表（父先于子），close 时回填
    let mut items: Vec<(usize, String, Option<usize>)> = Vec::new();
    let mut stack: Vec<usize> = Vec::new(); // navPoint → items 下标
    let mut depth = 0usize;
    let mut in_text = 0u32;
    let mut text_buf = String::new();
    for t in tokenize(xml) {
        match t {
            Token::Open { name, attrs, .. } => match local_name(&name) {
                "navpoint" => {
                    items.push((depth, String::new(), None));
                    stack.push(items.len() - 1);
                    depth += 1;
                }
                "text" if !stack.is_empty() => {
                    in_text += 1;
                    text_buf.clear();
                }
                "content" if !stack.is_empty() => {
                    if let Some(src) = attr(&attrs, "src") {
                        if let Some(&li) = stack.last() {
                            items[li].2 = filepos_from_src(src);
                        }
                    }
                }
                _ => {}
            },
            Token::Text(s) => {
                if in_text > 0 {
                    text_buf.push_str(&s);
                }
            }
            Token::Close(name) => match local_name(&name) {
                "text" => {
                    if in_text > 0 {
                        in_text -= 1;
                        if in_text == 0 {
                            if let Some(&li) = stack.last() {
                                if items[li].1.is_empty() {
                                    items[li].1 = text_buf.trim().to_string();
                                }
                            }
                        }
                    }
                }
                "navpoint" => {
                    depth = depth.saturating_sub(1);
                    stack.pop();
                }
                _ => {}
            },
            _ => {}
        }
    }
    items
        .into_iter()
        .filter_map(|(d, t, fp)| fp.map(|fp| (fp, t, d)))
        .collect()
}

/// 按路径（顶层下标链）取嵌套节点。
fn toc_node_at<'a>(nodes: &'a mut Vec<TocNode>, path: &[usize]) -> Option<&'a mut TocNode> {
    match path.split_first() {
        None => None,
        Some((&i, rest)) => {
            let n = nodes.get_mut(i)?;
            if rest.is_empty() {
                Some(n)
            } else {
                toc_node_at(&mut n.children, rest)
            }
        }
    }
}

/// 缓存 open 索引可信校验：spans 有序不重叠且在文本内、章表与 spans 对齐、
/// 目录章号在范围内（防错配/损坏载荷）。
fn index_ok(
    spans: &[(usize, usize)],
    entries: &[ChapterEntry],
    toc: &[TocNode],
    raw_len: usize,
) -> bool {
    !spans.is_empty()
        && spans.iter().enumerate().all(|(i, &(s, e))| {
            s <= e && e <= raw_len && (i == 0 || spans[i - 1].1 <= s)
        })
        && entries.len() == spans.len()
        && entries.iter().enumerate().all(|(i, en)| {
            en.id == i as u32 && en.byte_range == (spans[i].0 as u64, spans[i].1 as u64)
        })
        && toc_ok(toc, entries.len())
}

fn toc_ok(nodes: &[TocNode], n_chapters: usize) -> bool {
    nodes.iter().all(|n| {
        (n.chapter as usize) < n_chapters && toc_ok(&n.children, n_chapters)
    })
}

impl MobiBook {
    pub fn parse(src: &ContentSource) -> RrkResult<Self> {
        Self::parse_with_cache(src, &NullIndexCache)
    }

    /// open 索引（spans + entries + toc）按内容 key 持久化——重开命中免
    /// 分段扫描、逐章清洗推导标题与 filepos 锚点扫描（open 期免重建；正文
    /// 装配 inflate 仍需逐次做，章内容切片依赖文本本体）。
    pub fn parse_with_cache(src: &ContentSource, cache: &dyn IndexCache) -> RrkResult<Self> {
        // 批次 5：内容版本指纹走流式全量哈希（峰值 1 MiB，与整包哈希同值，
        // 路径/内存源共享缓存键）。MOBI 解析需全文（PDB 结构 + 资源记录 +
        // 装配全文），故内容仍整包读入——读取与指纹统一走 to_source()/
        // SourceReader 按需读路径。
        let content_hash = src.content_fingerprint()?;
        let source = src.to_source()?;
        // 读入解析所需全部字节（SourceReader 按需读路径）。
        let mut buf = Vec::with_capacity(source.len() as usize);
        {
            let mut reader = rrk_source::SourceReader::new(source);
            std::io::Read::read_to_end(&mut reader, &mut buf)?;
        }
        let bytes = Arc::<[u8]>::from(buf);
        let container = parse::parse(bytes)?;
        let raw = assemble_text(&container)?;
        let raw_arc: Arc<[u8]> = Arc::from(raw);

        let key = CacheKey::content(content_hash, CacheKind::OpenMobi);
        let (spans, entries, toc) = match cache
            .load(&key)
            .and_then(|b| codec::decode(&b))
            .filter(|(spans, entries, toc)| index_ok(spans, entries, toc, raw_arc.len()))
        {
            Some(x) => x,
            None => {
                let spans = split_spans(&raw_arc);

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

                // 目录：优先 NCX（EXTH 121，KF8/AZW3 主流；设计 §3.5），
                // 回退 filepos 锚点扫描（MOBI6 目录页形态）；filepos → 章 id
                // 映射并回填章标题；navPoint 嵌套保留为目录树（深审 §四）。
                let ncx = container
                    .exth_uint(121)
                    .and_then(|rec| container.record(rec as usize).ok())
                    .map(|r| parse_ncx_toc(&parse::decode_text(r, container.text_encoding)))
                    .filter(|v| !v.is_empty())
                    .unwrap_or_default();
                let items: Vec<(usize, String, usize)> = if ncx.is_empty() {
                    scan_filepos_anchors(&raw_arc)
                        .into_iter()
                        .map(|(fp, t)| (fp, t, 0))
                        .collect()
                } else {
                    ncx
                };
                let mut toc: Vec<TocNode> = Vec::new();
                let mut path: Vec<usize> = Vec::new();
                for (filepos, title, depth) in items {
                    let Some(idx) = spans.iter().position(|&(s, e)| filepos >= s && filepos < e) else {
                        continue;
                    };
                    if entries[idx].title.is_none() {
                        entries[idx].title = Some(title.clone());
                    }
                    let depth = depth.min(path.len()); // 防畸形跳级
                    path.truncate(depth);
                    let node = TocNode {
                        title,
                        chapter: idx as ChapterId,
                        anchor: None,
                        children: Vec::new(),
                    };
                    if depth == 0 {
                        toc.push(node);
                        path = vec![toc.len() - 1];
                    } else if let Some(parent) = toc_node_at(&mut toc, &path[..depth]) {
                        let ci = parent.children.len();
                        parent.children.push(node);
                        path.push(ci);
                    } else {
                        toc.push(node); // 不可达兑底（parent 缺失时归顶层）
                        path = vec![toc.len() - 1];
                    }
                }
                // 目录没覆盖到的章：用推导标题回填
                for (entry, title) in entries.iter_mut().zip(titles) {
                    if entry.title.is_none() {
                        entry.title = title;
                    }
                }

                cache.save(&key, &codec::encode(&spans, &entries, &toc));
                (spans, entries, toc)
            }
        };

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
            content_hash,
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

    fn open_cached(
        &self,
        src: &ContentSource,
        cache: &dyn IndexCache,
    ) -> RrkResult<Box<dyn BookSource>> {
        Ok(Box::new(MobiBook::parse_with_cache(src, cache)?))
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

    /// fuzz 语料回归：seed.mobi 必须能完整打开（种子有效性 + 解析面不回退）。
    #[test]
    fn fuzz_seed_opens() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fuzz/seed-corpus/format_open/seed.mobi"
        );
        let Ok(bytes) = std::fs::read(path) else { return };
        let src = ContentSource::from_bytes(Some("seed.mobi".into()), bytes);
        let book = MobiAdapter.open(&src).unwrap();
        assert_eq!(book.meta().title, "RRK Seed");
        assert!(!book.chapters().unwrap().is_empty());
        assert!(book.toc().is_ok());
    }

    /// 批次 5（MOBI）：**路径源与内存源的指纹必须同值**——流式指纹
    /// （`content_fingerprint`）与整包哈希一致，否则同一本书走两条路开会
    /// 命中不同缓存（读旧内容）。
    #[test]
    fn path_and_memory_sources_share_fingerprint() {
        let seed = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fuzz/seed-corpus/format_open/seed.mobi"
        );
        let Ok(bytes) = std::fs::read(seed) else { return };
        let dir = std::env::temp_dir().join("rrk_mobi_fp");
        std::fs::create_dir_all(&dir).unwrap();
        // 唯一文件名：并行测试不抢句柄（Windows 尤甚）
        let path = dir.join(format!("t_fp_{}.mobi", std::process::id()));
        std::fs::write(&path, &bytes).unwrap();

        let path_src = ContentSource::from_path(&path);
        let mem_src =
            ContentSource::from_bytes(Some("seed.mobi".into()), Arc::<[u8]>::from(bytes.clone()));
        assert_eq!(
            path_src.content_fingerprint().unwrap(),
            mem_src.content_fingerprint().unwrap(),
            "路径源（流式哈希）与内存源（整包哈希）指纹必须一致"
        );
        assert_eq!(
            path_src.content_fingerprint().unwrap(),
            rrk_format::content_fingerprint(&bytes),
            "流式指纹须与整包指纹同值"
        );
        std::fs::remove_file(&path).ok();
    }

    /// 批次 5（MOBI）：路径开书（按需读路径）与内存开书的**内容完全一致**。
    #[test]
    fn path_and_memory_open_agree() {
        let seed = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fuzz/seed-corpus/format_open/seed.mobi"
        );
        let Ok(bytes) = std::fs::read(seed) else { return };
        let dir = std::env::temp_dir().join("rrk_mobi_path");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("t_open_{}.mobi", std::process::id()));
        std::fs::write(&path, &bytes).unwrap();

        let from_path = MobiBook::parse(&ContentSource::from_path(&path)).unwrap();
        let from_mem = MobiBook::parse(&ContentSource::from_bytes(
            Some("seed.mobi".into()),
            Arc::<[u8]>::from(bytes.clone()),
        ))
        .unwrap();

        assert_eq!(from_path.meta().content_hash, from_mem.meta().content_hash);
        assert_eq!(
            from_path.text, from_mem.text,
            "路径开书与内存开书的装配全文必须一致"
        );
        assert_eq!(from_path.entries, from_mem.entries);
        std::fs::remove_file(&path).ok();
    }

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

    /// open 索引载荷编解码回环（spans + 章表 + 目录）+ 损坏丢弃。
    #[test]
    fn open_index_codec_roundtrip() {
        let spans = vec![(0usize, 10usize), (10, 25)];
        let entries = vec![
            ChapterEntry {
                id: 0,
                title: Some("章一".into()),
                href: "mobi:0".into(),
                byte_range: (0, 10),
                char_len: 4,
                kind: ChapterKind::Text,
                part: 0,
            },
            ChapterEntry {
                id: 1,
                title: None,
                href: "mobi:1".into(),
                byte_range: (10, 25),
                char_len: 6,
                kind: ChapterKind::Text,
                part: 0,
            },
        ];
        let toc = vec![TocNode {
            title: "章二".into(),
            chapter: 1,
            anchor: None,
            children: vec![],
        }];
        let d = codec::encode(&spans, &entries, &toc);
        let (s2, e2, t2) = codec::decode(&d).unwrap();
        assert_eq!(s2, spans);
        assert_eq!(e2, entries);
        assert_eq!(t2, toc);
        assert!(codec::decode(b"garbage-not-a-payload").is_none());

        // 可信校验：对齐 → 过；越界 / 错位 → 拒
        assert!(index_ok(&spans, &entries, &toc, 25));
        assert!(!index_ok(&spans, &entries, &toc, 24), "越界应拒");
        let mut bad = entries.clone();
        bad[1].byte_range = (0, 9);
        assert!(!index_ok(&spans, &bad, &toc, 25), "章表与 spans 错位应拒");
        assert!(!index_ok(&spans, &entries, &[TocNode {
            title: "x".into(),
            chapter: 9,
            anchor: None,
            children: vec![],
        }], 25), "目录章号越界应拒");
    }

    /// NCX 解析：嵌套深度、src 两种形态（`#` 锚/纯数字）、无 filepos 丢节点。
    #[test]
    fn parse_ncx_toc_nested_and_src_forms() {
        let xml = r#"<ncx><navMap>
<navPoint><navLabel><text>Part One</text></navLabel><content src="0000000010"/></navPoint>
<navPoint><navLabel><text>Child</text></navLabel>
  <content src="text.xhtml#0000000099"/>
  <navPoint><navLabel><text>Grand</text></navLabel><content src="0000000150"/></navPoint>
</navPoint>
<navPoint><navLabel><text>NoFilepos</text></navLabel><content src="cover.jpg"/></navPoint>
</navMap></ncx>"#;
        let items = parse_ncx_toc(xml);
        assert_eq!(items.len(), 3, "无 filepos 节点丢弃: {items:?}");
        // 父先于子（open 序）；嵌套深度正确（Child 顶层，Grand 嵌在 Child 内）
        assert_eq!(items[0], (10, "Part One".to_string(), 0));
        assert_eq!(items[1], (99, "Child".to_string(), 0));
        assert_eq!(items[2], (150, "Grand".to_string(), 1));
    }

    /// EXTH 121 → NCX 目录端到端（深审 §四）：KF8 形态书打开有目录——
    /// 树形挂接、章映射、章标题回填。
    #[test]
    fn ncx_toc_from_exth121_end_to_end() {
        let text = "<p>前言</p><mbp:pagebreak/><h1>第二章</h1><p>正文二</p><mbp:pagebreak/><h1>第三章</h1><p>正文三</p>";
        let p1 = text.find("<mbp:pagebreak/>").unwrap();
        let p2 = text.rfind("<mbp:pagebreak/>").unwrap();
        let ncx = format!(
            "<ncx><navMap>\
<navPoint><navLabel><text>Part1</text></navLabel><content src=\"0000000000\"/></navPoint>\
<navPoint><navLabel><text>Ch2</text></navLabel><content src=\"text.xhtml#{p1:010}\"/>\
<navPoint><navLabel><text>Ch3</text></navLabel><content src=\"{p2:010}\"/></navPoint>\
</navPoint>\
</navMap></ncx>"
        );
        let text_b = text.as_bytes();
        let ncx_b = ncx.as_bytes();
        let r0_len = 272usize;
        let r0_off = 78 + 3 * 8; // 3 条记录表
        let r1_off = r0_off + r0_len;
        let r2_off = r1_off + text_b.len();
        let mut b = vec![0u8; 78];
        b[60..68].copy_from_slice(b"BOOKMOBI");
        b[76..78].copy_from_slice(&3u16.to_be_bytes());
        for off in [r0_off as u32, r1_off as u32, r2_off as u32] {
            b.extend_from_slice(&off.to_be_bytes());
            b.extend_from_slice(&0u32.to_be_bytes());
        }
        let rz = r0_off;
        b.resize(rz + r0_len, 0);
        // PalmDOC 头：无压缩 + 正文长度 + 1 条文本记录
        b[rz..rz + 2].copy_from_slice(&1u16.to_be_bytes());
        b[rz + 4..rz + 8].copy_from_slice(&(text_b.len() as u32).to_be_bytes());
        b[rz + 8..rz + 10].copy_from_slice(&1u16.to_be_bytes());
        b[rz + 10..rz + 12].copy_from_slice(&4096u16.to_be_bytes());
        // MOBI 头：UTF-8 + headerLength 228
        b[rz + 16..rz + 20].copy_from_slice(b"MOBI");
        b[rz + 20..rz + 24].copy_from_slice(&228u32.to_be_bytes());
        b[rz + 28..rz + 32].copy_from_slice(&65001u32.to_be_bytes());
        // EXTH：flags + 一条 (121 = NCX 记录号 2)
        b[rz + 128..rz + 132].copy_from_slice(&0x40u32.to_be_bytes());
        let e = rz + 244; // 16 + headerLength
        b[e..e + 4].copy_from_slice(b"EXTH");
        b[e + 4..e + 8].copy_from_slice(&24u32.to_be_bytes());
        b[e + 8..e + 12].copy_from_slice(&1u32.to_be_bytes());
        b[e + 12..e + 16].copy_from_slice(&121u32.to_be_bytes());
        b[e + 16..e + 20].copy_from_slice(&12u32.to_be_bytes());
        b[e + 20..e + 24].copy_from_slice(&2u32.to_be_bytes());
        // record1 = 正文，record2 = NCX
        assert_eq!(b.len(), r1_off);
        b.extend_from_slice(text_b);
        assert_eq!(b.len(), r2_off);
        b.extend_from_slice(ncx_b);

        let src = ContentSource::from_bytes(
            Some("k.epub3".to_string()),
            Arc::<[u8]>::from(b.into_boxed_slice()),
        );
        let book = MobiBook::parse(&src).unwrap();
        assert_eq!(book.toc.len(), 2, "顶层两项: {:?}", book.toc);
        assert_eq!(book.toc[0].title, "Part1");
        assert_eq!(book.toc[0].chapter, 0);
        assert_eq!(book.toc[1].title, "Ch2");
        assert_eq!(book.toc[1].chapter, 1);
        assert_eq!(book.toc[1].children.len(), 1, "嵌套子节点应挂上: {:?}", book.toc[1].children);
        assert_eq!(book.toc[1].children[0].title, "Ch3");
        assert_eq!(book.toc[1].children[0].chapter, 2);
        // NCX 标题回填章标题
        assert_eq!(book.entries[1].title.as_deref(), Some("Ch2"));
        assert_eq!(book.entries[2].title.as_deref(), Some("Ch3"));
    }
}
