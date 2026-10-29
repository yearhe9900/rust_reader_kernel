//! EPUB open 索引载荷编解码（ROADMAP「章级/页级缓存持久化扩展」）。
//!
//! 载荷 = 章表（`Vec<ChapterEntry>`）+ 目录原始树（`Vec<TocRaw>`，含 href +
//! fragment，锚点仍惰性解析）。自带 magic + schema 号，版本不符 / 损坏即
//! 整包丢弃重算（口径同 rrk-cache 容器）。

use crate::TocRaw;
use rrk_model::payload::{R, W};
use rrk_model::ChapterEntry;

const MAGIC: &[u8; 4] = b"RRKE";
const SCHEMA: u32 = 1;

pub fn encode(entries: &[ChapterEntry], toc_raw: &[TocRaw]) -> Vec<u8> {
    let mut w = W::new(MAGIC, SCHEMA);
    w.entries(entries);
    put_toc_raw(&mut w, toc_raw);
    w.into_vec()
}

pub fn decode(d: &[u8]) -> Option<(Vec<ChapterEntry>, Vec<TocRaw>)> {
    let mut r = R::new(d, MAGIC, SCHEMA)?;
    let entries = r.entries()?;
    let toc_raw = get_toc_raw(&mut r)?;
    r.done().then_some((entries, toc_raw))
}

fn put_toc_raw(w: &mut W, raws: &[TocRaw]) {
    w.u32(raws.len() as u32);
    for raw in raws {
        w.str(&raw.title);
        w.str(&raw.href);
        put_toc_raw(w, &raw.children);
    }
}

fn get_toc_raw(r: &mut R) -> Option<Vec<TocRaw>> {
    let n = r.count()?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(TocRaw {
            title: r.str()?,
            href: r.str()?,
            children: get_toc_raw(r)?,
        });
    }
    Some(out)
}
