//! 内容级缓存 payload 编解码（issue #12）。
//!
//! 把「解析结果」（目录树 / 章内容 / 章大纲）序列化为缓存字节，命中即免
//! inflate + 清洗。自研紧凑二进制（长度前缀 + 标签字节），自带 magic +
//! schema 号，不兼容即整包丢弃重算（损坏 ≠ 判据失效，口径同 rrk-cache）。

use rrk_model::{
    Block, ChapterContent, ChapterEntry, ChapterId, ChapterKind, Inline, LinkRange, ResourceKey,
    StyleFlags, TextRange, TocNode,
};
use std::sync::Arc;

const MAGIC: &[u8; 4] = b"RRKM";
const SCHEMA: u32 = 1;

// ---------------------------------------------------------------------------
// 写端
// ---------------------------------------------------------------------------

struct W(Vec<u8>);

impl W {
    fn new() -> Self {
        Self(Vec::with_capacity(256))
    }

    fn header(&mut self) {
        self.0.extend_from_slice(MAGIC);
        self.0.extend_from_slice(&SCHEMA.to_le_bytes());
    }

    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }

    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    fn bool(&mut self, v: bool) {
        self.0.push(v as u8);
    }

    fn str(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.0.extend_from_slice(s.as_bytes());
    }

    fn opt_str(&mut self, s: &Option<String>) {
        match s {
            Some(s) => {
                self.u8(1);
                self.str(s);
            }
            None => self.u8(0),
        }
    }
}

// ---------------------------------------------------------------------------
// 读端（越界/尾部残留/版本不符 → None，整包丢弃重算）
// ---------------------------------------------------------------------------

struct R<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> R<'a> {
    fn new(d: &'a [u8]) -> Option<Self> {
        if d.len() < 8 || &d[0..4] != MAGIC || u32::from_le_bytes(d[4..8].try_into().ok()?) != SCHEMA
        {
            return None;
        }
        Some(Self { d, p: 8 })
    }

    fn u8(&mut self) -> Option<u8> {
        let v = *self.d.get(self.p)?;
        self.p += 1;
        Some(v)
    }

    fn bool(&mut self) -> Option<bool> {
        self.u8().map(|v| v != 0)
    }

    fn u32(&mut self) -> Option<u32> {
        let v = self.d.get(self.p..self.p + 4)?;
        self.p += 4;
        Some(u32::from_le_bytes(v.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        let v = self.d.get(self.p..self.p + 8)?;
        self.p += 8;
        Some(u64::from_le_bytes(v.try_into().ok()?))
    }

    fn str(&mut self) -> Option<String> {
        let len = self.u32()? as usize;
        let s = self.d.get(self.p..self.p.checked_add(len)?)?;
        self.p += len;
        String::from_utf8(s.to_vec()).ok()
    }

    fn opt_str(&mut self) -> Option<Option<String>> {
        match self.u8()? {
            0 => Some(None),
            1 => Some(Some(self.str()?)),
            _ => None,
        }
    }

    fn done(&self) -> bool {
        self.p == self.d.len()
    }
}

// ---------------------------------------------------------------------------
// 目录树
// ---------------------------------------------------------------------------

fn write_toc(w: &mut W, nodes: &[TocNode]) {
    w.u32(nodes.len() as u32);
    for n in nodes {
        w.str(&n.title);
        w.u32(n.chapter);
        match n.anchor {
            Some(a) => {
                w.u8(1);
                w.u32(a);
            }
            None => w.u8(0),
        }
        write_toc(w, &n.children);
    }
}

fn read_toc(r: &mut R) -> Option<Vec<TocNode>> {
    let n = r.u32()? as usize;
    // 防损坏数据无界分配：节点数不可能超过剩余字节数（每节点至少 13 字节）
    if n > r.d.len() {
        return None;
    }
    let mut out = Vec::with_capacity(n.min(1024));
    for _ in 0..n {
        let title = r.str()?;
        let chapter = r.u32()?;
        let anchor = match r.u8()? {
            0 => None,
            1 => Some(r.u32()?),
            _ => return None,
        };
        let children = read_toc(r)?;
        out.push(TocNode { title, chapter, anchor, children });
    }
    Some(out)
}

/// 目录树 → 缓存 payload。
pub fn encode_toc(nodes: &[TocNode]) -> Vec<u8> {
    let mut w = W::new();
    w.header();
    write_toc(&mut w, nodes);
    w.0
}

/// 缓存 payload → 目录树（损坏/版本不符回 None）。
pub fn decode_toc(bytes: &[u8]) -> Option<Vec<TocNode>> {
    let mut r = R::new(bytes)?;
    let nodes = read_toc(&mut r)?;
    r.done().then_some(nodes)
}

// ---------------------------------------------------------------------------
// 章内容（正文 + 块 + 内链）
// ---------------------------------------------------------------------------

fn write_inlines(w: &mut W, inlines: &[Inline]) {
    w.u32(inlines.len() as u32);
    for i in inlines {
        match i {
            Inline::Text { range, style } => {
                w.u8(0);
                w.u32(range.start);
                w.u32(range.end);
                w.u8(style.0);
            }
            Inline::Image { key } => {
                w.u8(1);
                w.str(&key.0);
            }
        }
    }
}

fn read_inlines(r: &mut R) -> Option<Vec<Inline>> {
    let n = r.u32()? as usize;
    if n > r.d.len() {
        return None;
    }
    let mut out = Vec::with_capacity(n.min(4096));
    for _ in 0..n {
        match r.u8()? {
            0 => {
                let start = r.u32()?;
                let end = r.u32()?;
                let style = StyleFlags(r.u8()?);
                out.push(Inline::Text { range: TextRange::new(start, end), style });
            }
            1 => out.push(Inline::Image { key: ResourceKey(r.str()?) }),
            _ => return None,
        }
    }
    Some(out)
}

fn write_blocks(w: &mut W, blocks: &[Block]) {
    w.u32(blocks.len() as u32);
    for b in blocks {
        match b {
            Block::Paragraph { inlines, indent } => {
                w.u8(0);
                w.bool(*indent);
                write_inlines(w, inlines);
            }
            Block::Heading { level, inlines } => {
                w.u8(1);
                w.u8(*level);
                write_inlines(w, inlines);
            }
            Block::Image { key, alt, size_hint } => {
                w.u8(2);
                w.str(&key.0);
                w.opt_str(alt);
                match size_hint {
                    Some((cw, ch)) => {
                        w.u8(1);
                        w.u32(*cw);
                        w.u32(*ch);
                    }
                    None => w.u8(0),
                }
            }
            Block::Quote { blocks } => {
                w.u8(3);
                write_blocks(w, blocks);
            }
            Block::Divider => w.u8(4),
        }
    }
}

fn read_blocks(r: &mut R) -> Option<Vec<Block>> {
    let n = r.u32()? as usize;
    if n > r.d.len() {
        return None;
    }
    let mut out = Vec::with_capacity(n.min(4096));
    for _ in 0..n {
        match r.u8()? {
            0 => {
                let indent = r.bool()?;
                let inlines = read_inlines(r)?;
                out.push(Block::Paragraph { inlines, indent });
            }
            1 => {
                let level = r.u8()?;
                let inlines = read_inlines(r)?;
                out.push(Block::Heading { level, inlines });
            }
            2 => {
                let key = ResourceKey(r.str()?);
                let alt = r.opt_str()?;
                let size_hint = match r.u8()? {
                    0 => None,
                    1 => Some((r.u32()?, r.u32()?)),
                    _ => return None,
                };
                out.push(Block::Image { key, alt, size_hint });
            }
            3 => out.push(Block::Quote { blocks: read_blocks(r)? }),
            4 => out.push(Block::Divider),
            _ => return None,
        }
    }
    Some(out)
}

/// 章内容 → 缓存 payload。
pub fn encode_chapter(content: &ChapterContent) -> Vec<u8> {
    let mut w = W::new();
    w.header();
    w.u32(content.id);
    w.str(&content.text);
    write_blocks(&mut w, &content.blocks);
    w.u32(content.link_ranges.len() as u32);
    for l in &content.link_ranges {
        w.u32(l.range.start);
        w.u32(l.range.end);
        w.str(&l.href);
    }
    w.0
}

/// 缓存 payload → 章内容（损坏/版本不符回 None）。
pub fn decode_chapter(bytes: &[u8]) -> Option<ChapterContent> {
    let mut r = R::new(bytes)?;
    let id: ChapterId = r.u32()?;
    let text = Arc::from(r.str()?.as_str());
    let blocks = read_blocks(&mut r)?;
    let nl = r.u32()? as usize;
    if nl > r.d.len() {
        return None;
    }
    let mut link_ranges = Vec::with_capacity(nl.min(4096));
    for _ in 0..nl {
        let start = r.u32()?;
        let end = r.u32()?;
        let href = r.str()?;
        link_ranges.push(LinkRange { range: TextRange::new(start, end), href });
    }
    r.done().then_some(ChapterContent { id, text, blocks, link_ranges })
}

// ---------------------------------------------------------------------------
// 章大纲（chapters() 条目表）
// ---------------------------------------------------------------------------

/// 章大纲 → 缓存 payload。
pub fn encode_outline(entries: &[ChapterEntry]) -> Vec<u8> {
    let mut w = W::new();
    w.header();
    w.u32(entries.len() as u32);
    for e in entries {
        w.u32(e.id);
        w.opt_str(&e.title);
        w.str(&e.href);
        w.u64(e.byte_range.0);
        w.u64(e.byte_range.1);
        w.u32(e.char_len);
        w.u8(match e.kind {
            ChapterKind::Text => 0,
            ChapterKind::ImageGallery => 1,
        });
        w.u32(e.part);
    }
    w.0
}

/// 缓存 payload → 章大纲（损坏/版本不符回 None）。
pub fn decode_outline(bytes: &[u8]) -> Option<Vec<ChapterEntry>> {
    let mut r = R::new(bytes)?;
    let n = r.u32()? as usize;
    if n > r.d.len() {
        return None;
    }
    let mut out = Vec::with_capacity(n.min(4096));
    for _ in 0..n {
        let id = r.u32()?;
        let title = r.opt_str()?;
        let href = r.str()?;
        let b0 = r.u64()?;
        let b1 = r.u64()?;
        let char_len = r.u32()?;
        let kind = match r.u8()? {
            0 => ChapterKind::Text,
            1 => ChapterKind::ImageGallery,
            _ => return None,
        };
        let part = r.u32()?;
        out.push(ChapterEntry { id, title, href, byte_range: (b0, b1), char_len, kind, part });
    }
    r.done().then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_toc() -> Vec<TocNode> {
        vec![
            TocNode {
                title: "第一章".to_string(),
                chapter: 0,
                anchor: Some(12),
                children: vec![TocNode {
                    title: "1.1 节".to_string(),
                    chapter: 1,
                    anchor: None,
                    children: Vec::new(),
                }],
            },
            TocNode { title: "第二章".to_string(), chapter: 2, anchor: None, children: Vec::new() },
        ]
    }

    fn sample_chapter() -> ChapterContent {
        let text: Arc<str> = Arc::from("正文开头，含内链与图片。第二段。");
        let blocks = vec![
            Block::Paragraph {
                inlines: vec![
                    Inline::Text { range: TextRange::new(0, 15), style: StyleFlags(1) },
                    Inline::Image { key: ResourceKey("img/a.png".to_string()) },
                ],
                indent: true,
            },
            Block::Heading { level: 2, inlines: vec![Inline::Text { range: TextRange::new(15, 21), style: StyleFlags(0) }] },
            Block::Quote { blocks: vec![Block::Divider, Block::Image { key: ResourceKey("img/b.jpg".to_string()), alt: Some("图 b".to_string()), size_hint: Some((100, 50)) }] },
        ];
        let link_ranges = vec![LinkRange { range: TextRange::new(3, 9), href: "ch2.xhtml#s1".to_string() }];
        ChapterContent { id: 7, text, blocks, link_ranges }
    }

    #[test]
    fn toc_roundtrip() {
        let toc = sample_toc();
        assert_eq!(decode_toc(&encode_toc(&toc)).unwrap(), toc);
    }

    #[test]
    fn chapter_roundtrip() {
        let c = sample_chapter();
        let back = decode_chapter(&encode_chapter(&c)).unwrap();
        assert_eq!(back.id, c.id);
        assert_eq!(&*back.text, &*c.text);
        assert_eq!(back.blocks, c.blocks);
        assert_eq!(back.link_ranges, c.link_ranges);
    }

    #[test]
    fn outline_roundtrip() {
        let entries = vec![
            ChapterEntry { id: 0, title: Some("章一".to_string()), href: "txt:0".to_string(), byte_range: (23, 107), char_len: 48, kind: ChapterKind::Text, part: 0 },
            ChapterEntry { id: 1, title: None, href: "txt:107".to_string(), byte_range: (107, 220), char_len: 37, kind: ChapterKind::Text, part: 1 },
        ];
        assert_eq!(decode_outline(&encode_outline(&entries)).unwrap(), entries);
    }

    #[test]
    fn corruption_isolated() {
        let bytes = encode_toc(&sample_toc());
        assert!(decode_toc(&bytes[..10]).is_none(), "截断应拒收");
        let mut bad = bytes.clone();
        bad[4] ^= 0xFF; // schema 篡改
        assert!(decode_toc(&bad).is_none(), "schema 不符应拒收");
        assert!(decode_chapter(&bytes).is_none(), "跨类型误读应拒收");
    }
}
