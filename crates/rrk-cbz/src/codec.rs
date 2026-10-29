//! CBZ open 索引载荷编解码（ROADMAP「章级/页级缓存持久化扩展」）。
//!
//! 载荷 = 页表（`PageRef[]`：zip 条目名 + 归一化名）+ 尺寸表
//! （`Option<(w, h)>[]`，图片头探测结果）。自带 magic + schema 号，
//! 版本不符 / 损坏即整包丢弃重算。

use crate::PageRef;
use rrk_model::payload::{R, W};

const MAGIC: &[u8; 4] = b"RRKZ";
const SCHEMA: u32 = 1;

pub fn encode(pages: &[PageRef], sizes: &[Option<(u32, u32)>]) -> Vec<u8> {
    let mut w = W::new(MAGIC, SCHEMA);
    w.u32(pages.len() as u32);
    for p in pages {
        w.str(&p.zip_name);
        w.str(&p.name);
    }
    w.u32(sizes.len() as u32);
    for s in sizes {
        match s {
            Some((a, b)) => {
                w.u8(1);
                w.u32(*a);
                w.u32(*b);
            }
            None => w.u8(0),
        }
    }
    w.into_vec()
}

pub fn decode(d: &[u8]) -> Option<(Vec<PageRef>, Vec<Option<(u32, u32)>>)> {
    let mut r = R::new(d, MAGIC, SCHEMA)?;
    let n = r.count()?;
    let mut pages = Vec::with_capacity(n);
    for _ in 0..n {
        pages.push(PageRef { zip_name: r.str()?, name: r.str()? });
    }
    let m = r.count()?;
    if m != n {
        return None;
    }
    let mut sizes = Vec::with_capacity(m);
    for _ in 0..m {
        sizes.push(match r.u8()? {
            0 => None,
            1 => Some((r.u32()?, r.u32()?)),
            _ => return None,
        });
    }
    r.done().then_some((pages, sizes))
}
