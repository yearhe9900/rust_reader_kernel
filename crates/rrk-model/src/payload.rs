//! 缓存载荷编解码原语（自研紧凑二进制：长度前缀 + 标签字节）。
//!
//! open 索引等持久化载荷（ROADMAP「章级/页级缓存持久化扩展」）共用的写端/
//! 读端，以及 [`ChapterEntry`] / [`TocNode`] 的表级编解码。载荷自带
//! magic + schema 号，版本不符 / 越界 / 尾部残留即整包丢弃重算
//! （损坏 ≠ 判据失效，口径同 rrk-cache 容器）。

use super::{ChapterEntry, ChapterKind, TocNode};

// ---------------------------------------------------------------------------
// 写端
// ---------------------------------------------------------------------------

pub struct W(Vec<u8>);

impl W {
    /// 建写端并写容器头（magic + schema）。
    pub fn new(magic: &[u8; 4], schema: u32) -> Self {
        let mut w = Self(Vec::with_capacity(512));
        w.0.extend_from_slice(magic);
        w.0.extend_from_slice(&schema.to_le_bytes());
        w
    }

    pub fn u8(&mut self, v: u8) {
        self.0.push(v);
    }

    pub fn bool(&mut self, v: bool) {
        self.0.push(v as u8);
    }

    pub fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    pub fn f32(&mut self, v: f32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    pub fn f64(&mut self, v: f64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    pub fn str(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.0.extend_from_slice(s.as_bytes());
    }

    pub fn opt_str(&mut self, s: &Option<String>) {
        match s {
            Some(s) => {
                self.u8(1);
                self.str(s);
            }
            None => self.u8(0),
        }
    }

    pub fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
    }

    /// 章索引项。
    pub fn entry(&mut self, e: &ChapterEntry) {
        self.u32(e.id);
        self.opt_str(&e.title);
        self.str(&e.href);
        self.u64(e.byte_range.0);
        self.u64(e.byte_range.1);
        self.u32(e.char_len);
        self.u8(match e.kind {
            ChapterKind::Text => 0,
            ChapterKind::ImageGallery => 1,
        });
        self.u32(e.part);
    }

    /// 章索引表（长度前缀）。
    pub fn entries(&mut self, es: &[ChapterEntry]) {
        self.u32(es.len() as u32);
        for e in es {
            self.entry(e);
        }
    }

    /// 目录树（长度前缀 + 递归）。
    pub fn toc(&mut self, nodes: &[TocNode]) {
        self.u32(nodes.len() as u32);
        for n in nodes {
            self.str(&n.title);
            self.u32(n.chapter);
            match n.anchor {
                Some(a) => {
                    self.u8(1);
                    self.u32(a);
                }
                None => self.u8(0),
            }
            self.toc(&n.children);
        }
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }
}

// ---------------------------------------------------------------------------
// 读端（越界 / 尾部残留 / 版本不符 → None，整包丢弃重算）
// ---------------------------------------------------------------------------

pub struct R<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> R<'a> {
    /// 校验容器头（magic + schema）并建读端。
    pub fn new(d: &'a [u8], magic: &[u8; 4], schema: u32) -> Option<Self> {
        if d.len() < 8 || &d[0..4] != magic || u32::from_le_bytes(d[4..8].try_into().ok()?) != schema
        {
            return None;
        }
        Some(Self { d, p: 8 })
    }

    pub fn u8(&mut self) -> Option<u8> {
        let v = *self.d.get(self.p)?;
        self.p += 1;
        Some(v)
    }

    pub fn bool(&mut self) -> Option<bool> {
        self.u8().map(|v| v != 0)
    }

    pub fn u32(&mut self) -> Option<u32> {
        let v = self.d.get(self.p..self.p + 4)?;
        self.p += 4;
        Some(u32::from_le_bytes(v.try_into().ok()?))
    }

    pub fn u64(&mut self) -> Option<u64> {
        let v = self.d.get(self.p..self.p + 8)?;
        self.p += 8;
        Some(u64::from_le_bytes(v.try_into().ok()?))
    }

    pub fn f32(&mut self) -> Option<f32> {
        let v = self.d.get(self.p..self.p + 4)?;
        self.p += 4;
        Some(f32::from_le_bytes(v.try_into().ok()?))
    }

    pub fn f64(&mut self) -> Option<f64> {
        let v = self.d.get(self.p..self.p + 8)?;
        self.p += 8;
        Some(f64::from_le_bytes(v.try_into().ok()?))
    }

    pub fn str(&mut self) -> Option<String> {
        let len = self.u32()? as usize;
        let s = self.d.get(self.p..self.p.checked_add(len)?)?;
        self.p += len;
        String::from_utf8(s.to_vec()).ok()
    }

    pub fn opt_str(&mut self) -> Option<Option<String>> {
        match self.u8()? {
            0 => Some(None),
            1 => Some(Some(self.str()?)),
            _ => None,
        }
    }

    pub fn bytes(&mut self) -> Option<Vec<u8>> {
        let len = self.u32()? as usize;
        let b = self.d.get(self.p..self.p.checked_add(len)?)?;
        self.p += len;
        Some(b.to_vec())
    }

    /// 元素个数（带剩余字节下限护栏：每元素至少 1 字节，防损坏长度巨额预分配）。
    pub fn count(&mut self) -> Option<usize> {
        let n = self.u32()? as usize;
        if n > self.d.len() - self.p {
            return None;
        }
        Some(n)
    }

    /// 章索引项。
    pub fn entry(&mut self) -> Option<ChapterEntry> {
        Some(ChapterEntry {
            id: self.u32()?,
            title: self.opt_str()?,
            href: self.str()?,
            byte_range: (self.u64()?, self.u64()?),
            char_len: self.u32()?,
            kind: match self.u8()? {
                0 => ChapterKind::Text,
                1 => ChapterKind::ImageGallery,
                _ => return None,
            },
            part: self.u32()?,
        })
    }

    /// 章索引表。
    pub fn entries(&mut self) -> Option<Vec<ChapterEntry>> {
        let n = self.count()?;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(self.entry()?);
        }
        Some(out)
    }

    /// 目录树（递归；深度由载荷字节数天然限制）。
    pub fn toc(&mut self) -> Option<Vec<TocNode>> {
        let n = self.count()?;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(TocNode {
                title: self.str()?,
                chapter: self.u32()?,
                anchor: match self.u8()? {
                    0 => None,
                    1 => Some(self.u32()?),
                    _ => return None,
                },
                children: self.toc()?,
            });
        }
        Some(out)
    }

    /// 读尽校验（尾部残留 = 不可信，整包丢弃）。
    pub fn done(&self) -> bool {
        self.p == self.d.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entries() -> Vec<ChapterEntry> {
        vec![
            ChapterEntry {
                id: 0,
                title: Some("章一".into()),
                href: "txt:0".into(),
                byte_range: (0, 10),
                char_len: 5,
                kind: ChapterKind::Text,
                part: 0,
            },
            ChapterEntry {
                id: 1,
                title: None,
                href: "txt:5".into(),
                byte_range: (10, 25),
                char_len: 7,
                kind: ChapterKind::ImageGallery,
                part: 2,
            },
        ]
    }

    #[test]
    fn entries_toc_roundtrip() {
        let entries = sample_entries();
        let toc = vec![TocNode {
            title: "根".into(),
            chapter: 0,
            anchor: Some(3),
            children: vec![TocNode { title: "子".into(), chapter: 1, anchor: None, children: vec![] }],
        }];
        let mut w = W::new(b"TEST", 1);
        w.entries(&entries);
        w.toc(&toc);
        let d = w.into_vec();
        let mut r = R::new(&d, b"TEST", 1).unwrap();
        assert_eq!(r.entries().unwrap(), entries);
        assert_eq!(r.toc().unwrap(), toc);
        assert!(r.done());
    }

    #[test]
    fn corrupt_payload_rejected() {
        // magic / schema 不符
        assert!(R::new(b"NOPE", b"TEST", 1).is_none());
        assert!(R::new(b"TEST", b"TEST", 2).is_none());
        // 空载荷读元素表 → 越界 None
        let d = W::new(b"TEST", 1).into_vec();
        let mut r = R::new(&d, b"TEST", 1).unwrap();
        assert!(r.entries().is_none());
        // 损坏长度前缀（巨额 count）被护栏拒绝
        let mut w = W::new(b"TEST", 1);
        w.u32(u32::MAX);
        let d = w.into_vec();
        let mut r = R::new(&d, b"TEST", 1).unwrap();
        assert!(r.entries().is_none());
    }
}
