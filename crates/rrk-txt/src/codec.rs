//! TXT open 索引载荷编解码（ROADMAP「章级/页级缓存持久化扩展」）。
//!
//! 载荷 = 章表（`Vec<ChapterEntry>`，含巨章逻辑块分片）。自带 magic +
//! schema 号，版本不符 / 损坏即整包丢弃重算（口径同 rrk-cache 容器）。

use rrk_model::payload::{R, W};
use rrk_model::ChapterEntry;

const MAGIC: &[u8; 4] = b"RRKT";
const SCHEMA: u32 = 1;

pub fn encode(entries: &[ChapterEntry]) -> Vec<u8> {
    let mut w = W::new(MAGIC, SCHEMA);
    w.entries(entries);
    w.into_vec()
}

pub fn decode(d: &[u8]) -> Option<Vec<ChapterEntry>> {
    let mut r = R::new(d, MAGIC, SCHEMA)?;
    let entries = r.entries()?;
    r.done().then_some(entries)
}
