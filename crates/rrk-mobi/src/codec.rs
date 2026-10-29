//! MOBI/AZW3 open 索引载荷编解码（ROADMAP「章级/页级缓存持久化扩展」）。
//!
//! 载荷 = 章 spans（字节范围）+ 章表（`Vec<ChapterEntry>`）+ 目录树。自带
//! magic + schema 号，版本不符 / 损坏即整包丢弃重算（口径同 rrk-cache 容器）。

use rrk_model::payload::{R, W};
use rrk_model::{ChapterEntry, TocNode};

const MAGIC: &[u8; 4] = b"RRKI";
const SCHEMA: u32 = 1;

pub fn encode(spans: &[(usize, usize)], entries: &[ChapterEntry], toc: &[TocNode]) -> Vec<u8> {
    let mut w = W::new(MAGIC, SCHEMA);
    w.u32(spans.len() as u32);
    for &(s, e) in spans {
        w.u64(s as u64);
        w.u64(e as u64);
    }
    w.entries(entries);
    w.toc(toc);
    w.into_vec()
}

pub fn decode(d: &[u8]) -> Option<(Vec<(usize, usize)>, Vec<ChapterEntry>, Vec<TocNode>)> {
    let mut r = R::new(d, MAGIC, SCHEMA)?;
    let n = r.count()?;
    let mut spans = Vec::with_capacity(n);
    for _ in 0..n {
        spans.push((r.u64()? as usize, r.u64()? as usize));
    }
    let entries = r.entries()?;
    let toc = r.toc()?;
    r.done().then_some((spans, entries, toc))
}
