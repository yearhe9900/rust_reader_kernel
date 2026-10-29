//! PDF open 索引载荷编解码（ROADMAP「章级/页级缓存持久化扩展」）。
//!
//! 载荷 = 页面表（内容流引用 / 尺寸 / 继承资源 / 权重 / 源对象号）+ 章表 +
//! 章页区间 + 目录树 + ObjStm 合并标记。`Object` 子树（内容流引用与继承
//! 资源）做通用编解码（深度上限护栏）。自带 magic + schema 号，版本不符 /
//! 损坏即整包丢弃重算（口径同 rrk-cache 容器）。

use std::collections::BTreeMap;

use crate::objects::Object;
use crate::PageInfo;
use rrk_model::payload::{R, W};
use rrk_model::{ChapterEntry, TocNode};

const MAGIC: &[u8; 4] = b"RRKP";
const SCHEMA: u32 = 1;

/// Object 子树编解码深度上限（合法继承资源远低于此；兜底防损坏载荷递归）。
const MAX_DEPTH: u32 = 32;

/// PDF open 索引（页面表 + 章表 + 章页区间 + 目录 + ObjStm 合并标记）。
pub struct PdfIndex {
    pub pages: Vec<PageInfo>,
    pub chapters: Vec<ChapterEntry>,
    pub chapter_pages: Vec<(u32, u32)>,
    pub toc: Vec<TocNode>,
    /// 首次构建时是否合并过 ObjStm：命中重开须同样合并，否则虚拟对象解析不到。
    pub merged: bool,
}

// ---------------------------------------------------------------------------
// Object 通用编解码
// ---------------------------------------------------------------------------

fn put_obj(w: &mut W, o: &Object, depth: u32) -> Option<()> {
    if depth > MAX_DEPTH {
        return None;
    }
    match o {
        Object::Null => w.u8(0),
        Object::Bool(b) => {
            w.u8(1);
            w.bool(*b);
        }
        Object::Int(i) => {
            w.u8(2);
            w.u64(*i as u64);
        }
        Object::Real(f) => {
            w.u8(3);
            w.f64(*f);
        }
        Object::Name(s) => {
            w.u8(4);
            w.str(s);
        }
        Object::Str(b) => {
            w.u8(5);
            w.bytes(b);
        }
        Object::Arr(a) => {
            w.u8(6);
            w.u32(a.len() as u32);
            for x in a {
                put_obj(w, x, depth + 1)?;
            }
        }
        Object::Dict(d) => {
            w.u8(7);
            put_dict(w, d, depth + 1)?;
        }
        Object::Stream { dict, data } => {
            w.u8(8);
            put_dict(w, dict, depth + 1)?;
            w.bytes(data);
        }
        Object::Ref(n, g) => {
            w.u8(9);
            w.u32(*n);
            w.u32(*g as u32);
        }
    }
    Some(())
}

fn put_dict(w: &mut W, d: &BTreeMap<String, Object>, depth: u32) -> Option<()> {
    if depth > MAX_DEPTH {
        return None;
    }
    w.u32(d.len() as u32);
    for (k, v) in d {
        w.str(k);
        put_obj(w, v, depth + 1)?;
    }
    Some(())
}

fn get_obj(r: &mut R, depth: u32) -> Option<Object> {
    if depth > MAX_DEPTH {
        return None;
    }
    Some(match r.u8()? {
        0 => Object::Null,
        1 => Object::Bool(r.bool()?),
        2 => Object::Int(r.u64()? as i64),
        3 => Object::Real(r.f64()?),
        4 => Object::Name(r.str()?),
        5 => Object::Str(r.bytes()?),
        6 => {
            let n = r.count()?;
            let mut a = Vec::with_capacity(n);
            for _ in 0..n {
                a.push(get_obj(r, depth + 1)?);
            }
            Object::Arr(a)
        }
        7 => Object::Dict(get_dict(r, depth + 1)?),
        8 => {
            let dict = get_dict(r, depth + 1)?;
            let data = r.bytes()?;
            Object::Stream { dict, data }
        }
        9 => Object::Ref(r.u32()?, r.u32()? as u16),
        _ => return None,
    })
}

fn get_dict(r: &mut R, depth: u32) -> Option<BTreeMap<String, Object>> {
    let n = r.count()?;
    let mut d = BTreeMap::new();
    for _ in 0..n {
        let k = r.str()?;
        d.insert(k, get_obj(r, depth + 1)?);
    }
    Some(d)
}

// ---------------------------------------------------------------------------
// 索引载荷
// ---------------------------------------------------------------------------

/// 编码（Object 子树超深等罕见情形返回 `None` = 放弃缓存，不影响解析）。
pub fn encode(
    pages: &[PageInfo],
    chapters: &[ChapterEntry],
    chapter_pages: &[(u32, u32)],
    toc: &[TocNode],
    merged: bool,
) -> Option<Vec<u8>> {
    let mut w = W::new(MAGIC, SCHEMA);
    w.bool(merged);
    w.u32(pages.len() as u32);
    for p in pages {
        w.u32(p.content_refs.len() as u32);
        for o in &p.content_refs {
            put_obj(&mut w, o, 0)?;
        }
        w.f32(p.media.0);
        w.f32(p.media.1);
        match &p.resources {
            Some(o) => {
                w.u8(1);
                put_obj(&mut w, o, 0)?;
            }
            None => w.u8(0),
        }
        w.u32(p.weight);
        w.u32(p.obj_num);
    }
    w.entries(chapters);
    w.u32(chapter_pages.len() as u32);
    for &(s, e) in chapter_pages {
        w.u32(s);
        w.u32(e);
    }
    w.toc(toc);
    Some(w.into_vec())
}

pub fn decode(d: &[u8]) -> Option<PdfIndex> {
    let mut r = R::new(d, MAGIC, SCHEMA)?;
    let merged = r.bool()?;
    let n = r.count()?;
    let mut pages = Vec::with_capacity(n);
    for _ in 0..n {
        let m = r.count()?;
        let mut content_refs = Vec::with_capacity(m);
        for _ in 0..m {
            content_refs.push(get_obj(&mut r, 0)?);
        }
        let media = (r.f32()?, r.f32()?);
        let resources = match r.u8()? {
            0 => None,
            1 => Some(get_obj(&mut r, 0)?),
            _ => return None,
        };
        pages.push(PageInfo {
            content_refs,
            media,
            resources,
            weight: r.u32()?,
            obj_num: r.u32()?,
        });
    }
    let chapters = r.entries()?;
    let m = r.count()?;
    let mut chapter_pages = Vec::with_capacity(m);
    for _ in 0..m {
        chapter_pages.push((r.u32()?, r.u32()?));
    }
    let toc = r.toc()?;
    r.done().then_some(PdfIndex { pages, chapters, chapter_pages, toc, merged })
}
