//! rrk-ffi：C ABI 边界（方案 §5，Dart/Flutter 调用面）。
//!
//! 纪律（方案 §0.3-3 / §5.3）：
//! - **句柄化**：跨边界只传 `u64` 句柄与 `(ptr, len)` 缓冲，不传引用；
//! - **panic 不跨界**：全部 `catch_unwind`，内部错误转错误码；
//! - **错误传递**：函数返回 0 = 成功，非 0 = `RrkErrorKind` 错误码（+2001 = panic），
//!   详情走 `rrk_last_error`（线程局部）；
//! - **缓冲所有权**：内核分配、Dart 用完必须 `rrk_buf_free` 释放。
//!
//! 结构化数据（meta/toc/章节/块）以 JSON 字符串缓冲传递（自研转义，零新增依赖）；
//! 大块正文走原始 UTF-8 缓冲（零拷贝语义）。

use std::cell::RefCell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Mutex;

use rrk_kernel::{BookHandle, Kernel};
use rrk_model::{Block, BookMeta, ChapterContent, ChapterEntry, Inline, RrkError, RrkErrorKind, TocNode};

/// ABI 版本号（Dart 启动校验；只增不改语义）。
pub const RRK_ABI_VERSION: u32 = 3;

/// panic 被捕获时的错误码。
pub const ERR_PANIC: u32 = 2001;

static KERNEL: Mutex<Option<Kernel>> = Mutex::new(None);
/// 排版器（粗粒度共享一份；parley FontContext 重量级）。
static LAYOUTER: Mutex<Option<rrk_layout::Layouter>> = Mutex::new(None);
/// 三要素缓存（C1 内存 LRU + C2 磁盘；`rrk_cache_configure` 后启用）。
static CACHE: Mutex<Option<(rrk_cache::DiskStore, rrk_cache::MemoryCache)>> = Mutex::new(None);

thread_local! {
    static LAST_ERROR: RefCell<String> = RefCell::new(String::new());
}

fn set_last_error(msg: impl Into<String>) {
    LAST_ERROR.with(|e| *e.borrow_mut() = msg.into());
}

fn with_kernel<T>(f: impl FnOnce(&mut Kernel) -> Result<T, u32>) -> Result<T, u32> {
    let mut guard = KERNEL
        .lock()
        .map_err(|_| set_err(RrkError::io("内核锁中毒"), RrkErrorKind::Internal.code()))?;
    let kernel = guard.get_or_insert_with(Kernel::new);
    f(kernel)
}

fn set_err(e: RrkError, code: u32) -> u32 {
    set_last_error(e.to_string());
    code
}

fn set_kind(e: RrkError) -> u32 {
    let code = e.kind.code();
    set_err(e, code)
}

/// 统一 panic 兜底：任何 panic → 2001 + 线程局部错误。
fn guard(f: impl FnOnce() -> Result<u32, u32>) -> u32 {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r.unwrap_or_else(|code| code),
        Err(_) => {
            set_last_error("内部 panic（已拦截）");
            ERR_PANIC
        }
    }
}

// ---------------------------------------------------------------------------
// 缓冲管理
// ---------------------------------------------------------------------------

fn alloc_buf(bytes: Vec<u8>) -> (*mut u8, usize) {
    let boxed = bytes.into_boxed_slice();
    let len = boxed.len();
    let ptr = Box::into_raw(boxed) as *mut u8;
    (ptr, len)
}

unsafe fn write_buf(bytes: Vec<u8>, out_ptr: *mut *mut u8, out_len: *mut usize) {
    let (p, n) = alloc_buf(bytes);
    *out_ptr = p;
    *out_len = n;
}

/// 释放内核分配的缓冲（Dart 侧用完必调）。
#[no_mangle]
pub unsafe extern "C" fn rrk_buf_free(ptr: *mut u8, len: usize) {
    if ptr.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)));
    }));
}

/// 返回 ABI 版本号。
#[no_mangle]
pub extern "C" fn rrk_abi_version() -> u32 {
    RRK_ABI_VERSION
}

/// 取最近一次错误信息（UTF-8；调用方负责 `rrk_buf_free`）。
#[no_mangle]
pub unsafe extern "C" fn rrk_last_error(out_ptr: *mut *mut u8, out_len: *mut usize) -> u32 {
    guard(|| {
        if out_ptr.is_null() || out_len.is_null() {
            return Err(set_kind(RrkError::invalid("空指针")));
        }
        let msg = LAST_ERROR.with(|e| e.borrow().clone());
        write_buf(msg.into_bytes(), out_ptr, out_len);
        Ok(0)
    })
}

// ---------------------------------------------------------------------------
// JSON 序列化（自研转义，零依赖）
// ---------------------------------------------------------------------------

fn esc(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
}

fn json_str(s: &str, out: &mut String) {
    out.push('"');
    esc(s, out);
    out.push('"');
}

fn meta_json(meta: &BookMeta) -> Vec<u8> {
    let mut s = String::with_capacity(256);
    s.push_str("{\"title\":");
    json_str(&meta.title, &mut s);
    s.push_str(",\"authors\":[");
    for (i, a) in meta.authors.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        json_str(a, &mut s);
    }
    s.push_str("],\"language\":");
    match &meta.language {
        Some(l) => json_str(l, &mut s),
        None => s.push_str("null"),
    }
    s.push_str(",\"format\":");
    json_str(&meta.format.to_string(), &mut s);
    s.push_str(&format!(",\"contentHash\":\"{:016x}\"}}", meta.content_hash));
    s.into_bytes()
}

fn toc_json(nodes: &[TocNode]) -> Vec<u8> {
    let mut s = String::with_capacity(256 * nodes.len().max(1));
    s.push('[');
    for (i, n) in nodes.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str("{\"title\":");
        json_str(&n.title, &mut s);
        s.push_str(&format!(",\"chapter\":{},\"children\":", n.chapter));
        s.push_str(&String::from_utf8_lossy(&toc_json(&n.children)));
        s.push('}');
    }
    s.push(']');
    s.into_bytes()
}

fn chapters_json(entries: &[ChapterEntry]) -> Vec<u8> {
    let mut s = String::with_capacity(256 * entries.len().max(1));
    s.push('[');
    for (i, e) in entries.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            "{{\"id\":{},\"title\":",
            e.id
        ));
        match &e.title {
            Some(t) => json_str(t, &mut s),
            None => s.push_str("null"),
        }
        s.push_str(",\"href\":");
        json_str(&e.href, &mut s);
        s.push_str(&format!(
            ",\"charLen\":{},\"byteStart\":{},\"byteEnd\":{},\"part\":{},\"kind\":\"{}\"}}",
            e.char_len,
            e.byte_range.0,
            e.byte_range.1,
            e.part,
            match e.kind {
                rrk_model::ChapterKind::Text => "text",
                rrk_model::ChapterKind::ImageGallery => "image",
            }
        ));
    }
    s.push(']');
    s.into_bytes()
}

fn inline_text(text: &str, inlines: &[Inline]) -> String {
    let mut out = String::new();
    for i in inlines {
        if let Inline::Text { range, .. } = i {
            out.push_str(range.slice(text));
        }
    }
    out
}

fn blocks_json(text: &str, blocks: &[Block]) -> Vec<u8> {
    let mut s = String::with_capacity(256 * blocks.len().max(1));
    s.push('[');
    let mut first = true;
    for b in blocks {
        let (ty, extra): (&str, String) = match b {
            Block::Heading { level, inlines } => {
                ("h", format!(",\"level\":{level},\"text\":{}", {
                    let mut t = String::new();
                    json_str(&inline_text(text, inlines), &mut t);
                    t
                }))
            }
            Block::Paragraph { inlines, indent } => {
                ("p", format!(",\"indent\":{},\"text\":{}", indent, {
                    let mut t = String::new();
                    json_str(&inline_text(text, inlines), &mut t);
                    t
                }))
            }
            Block::Image { key, alt, .. } => {
                ("img", format!(",\"key\":{},\"alt\":{}", {
                    let mut k = String::new();
                    json_str(&key.0, &mut k);
                    k
                }, {
                    match alt {
                        Some(a) => {
                            let mut x = String::new();
                            json_str(a, &mut x);
                            x
                        }
                        None => "null".to_string(),
                    }
                }))
            }
            Block::Quote { blocks } => ("quote", format!(",\"count\":{}", blocks.len())),
            Block::Divider => ("hr", String::new()),
        };
        if !first {
            s.push(',');
        }
        first = false;
        s.push_str("{\"type\":");
        json_str(ty, &mut s);
        s.push_str(&extra);
        s.push('}');
    }
    s.push(']');
    s.into_bytes()
}

// ---------------------------------------------------------------------------
// 书生命周期与查询
// ---------------------------------------------------------------------------

/// 打开一本书（UTF-8 路径）；成功返回句柄（≥1），失败返回 0。
#[no_mangle]
pub unsafe extern "C" fn rrk_open(path: *const u8, path_len: usize) -> u64 {
    match catch_unwind(AssertUnwindSafe(|| {
        if path.is_null() {
            set_last_error("路径为空指针");
            return 0;
        }
        let bytes = std::slice::from_raw_parts(path, path_len);
        let path = match std::str::from_utf8(bytes) {
            Ok(s) => s.to_string(),
            Err(e) => {
                set_last_error(format!("路径非 UTF-8: {e}"));
                return 0;
            }
        };
        match with_kernel(|k| k.open_path(&path).map_err(set_kind)) {
            Ok(h) => h,
            Err(_) => 0,
        }
    })) {
        Ok(h) => h,
        Err(_) => {
            set_last_error("内部 panic（已拦截）");
            0
        }
    }
}

/// 关闭一本书；0 = 成功。
#[no_mangle]
pub extern "C" fn rrk_close(handle: BookHandle) -> u32 {
    guard(|| with_kernel(|k| k.close(handle).map_err(set_kind)).map(|_| 0))
}

/// 书目元数据 JSON；0 = 成功（缓冲由调用方 `rrk_buf_free` 释放）。
#[no_mangle]
pub unsafe extern "C" fn rrk_meta_json(
    handle: BookHandle,
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
) -> u32 {
    guard(|| {
        if out_ptr.is_null() || out_len.is_null() {
            return Err(set_kind(RrkError::invalid("空指针")));
        }
        with_kernel(|k| {
            let book = k.book(handle).map_err(set_kind)?;
            let json = meta_json(book.meta());
            write_buf(json, out_ptr, out_len);
            Ok(0)
        })
    })
}

/// 目录 JSON；0 = 成功。
#[no_mangle]
pub unsafe extern "C" fn rrk_toc_json(
    handle: BookHandle,
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
) -> u32 {
    guard(|| {
        if out_ptr.is_null() || out_len.is_null() {
            return Err(set_kind(RrkError::invalid("空指针")));
        }
        with_kernel(|k| {
            let book = k.book(handle).map_err(set_kind)?;
            let toc = book.toc().map_err(set_kind)?;
            write_buf(toc_json(&toc), out_ptr, out_len);
            Ok(0)
        })
    })
}

/// 章索引 JSON；0 = 成功。
#[no_mangle]
pub unsafe extern "C" fn rrk_chapters_json(
    handle: BookHandle,
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
) -> u32 {
    guard(|| {
        if out_ptr.is_null() || out_len.is_null() {
            return Err(set_kind(RrkError::invalid("空指针")));
        }
        with_kernel(|k| {
            let book = k.book(handle).map_err(set_kind)?;
            let chapters = book.chapters().map_err(set_kind)?;
            write_buf(chapters_json(&chapters), out_ptr, out_len);
            Ok(0)
        })
    })
}

/// 加载一章：正文 UTF-8 缓冲 + 块结构 JSON；0 = 成功。
///
/// `text_ptr/text_len` 与 `blocks_ptr/blocks_len` 都由调用方 `rrk_buf_free` 释放。
#[no_mangle]
pub unsafe extern "C" fn rrk_load_chapter(
    handle: BookHandle,
    chapter_id: u32,
    text_ptr: *mut *mut u8,
    text_len: *mut usize,
    blocks_ptr: *mut *mut u8,
    blocks_len: *mut usize,
) -> u32 {
    guard(|| {
        if text_ptr.is_null() || text_len.is_null() || blocks_ptr.is_null() || blocks_len.is_null() {
            return Err(set_kind(RrkError::invalid("空指针")));
        }
        with_kernel(|k| {
            let book = k.book(handle).map_err(set_kind)?;
            let content = book.load_chapter(chapter_id).map_err(set_kind)?;
            let blocks = blocks_json(&content.text, &content.blocks);
            write_buf(content.text.as_bytes().to_vec(), text_ptr, text_len);
            write_buf(blocks, blocks_ptr, blocks_len);
            Ok(0)
        })
    })
}

/// 封面图（无封面返回 code=2 且不产出缓冲）；0 = 成功。
#[no_mangle]
pub unsafe extern "C" fn rrk_cover(
    handle: BookHandle,
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
) -> u32 {
    guard(|| {
        if out_ptr.is_null() || out_len.is_null() {
            return Err(set_kind(RrkError::invalid("空指针")));
        }
        with_kernel(|k| {
            let book = k.book(handle).map_err(set_kind)?;
            match book.cover().map_err(set_kind)? {
                Some(res) => {
                    write_buf(res.data.to_vec(), out_ptr, out_len);
                    Ok(0)
                }
                None => {
                    *out_ptr = std::ptr::null_mut();
                    *out_len = 0;
                    Ok(2) // 无封面（软结果，非错误）
                }
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_version_and_errors() {
        assert_eq!(rrk_abi_version(), RRK_ABI_VERSION);
        // 打开不存在的文件 → 句柄 0 + 错误可读
        let code = unsafe { rrk_open(b"nope.txt\0".as_ptr(), 8) };
        assert_eq!(code, 0);
        let mut p: *mut u8 = std::ptr::null_mut();
        let mut n = 0usize;
        let rc = unsafe { rrk_last_error(&mut p, &mut n) };
        assert_eq!(rc, 0);
        assert!(n > 0);
        unsafe { rrk_buf_free(p, n) };
    }

    #[test]
    fn json_escape() {
        let mut s = String::new();
        json_str("a\"b\\c\n", &mut s);
        assert_eq!(s, "\"a\\\"b\\\\c\\n\"");
    }
}


// ---------------------------------------------------------------------------
// 排版出口（P1）：pagesOf / renderPage
// ---------------------------------------------------------------------------

fn pages_to_json(pages: &[rrk_model::LayoutPage]) -> Vec<u8> {
    let mut s = String::with_capacity(1024 * pages.len().max(1));
    s.push('[');
    for (i, p) in pages.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            "{{\"index\":{},\"start\":{},\"end\":{},\"height\":{},\"lines\":[",
            p.index, p.start.char_offset, p.end.char_offset, p.height
        ));
        for (j, l) in p.lines.iter().enumerate() {
            if j > 0 {
                s.push(',');
            }
            s.push_str(&format!(
                "{{\"s\":{},\"e\":{},\"h\":{},\"p\":{}}}",
                l.start, l.end, l.height, l.para_start
            ));
        }
        s.push_str("],\"runs\":[");
        for (j, r) in p.runs.iter().enumerate() {
            if j > 0 {
                s.push(',');
            }
            s.push_str(&format!(
                "{{\"l\":{},\"x\":{},\"y\":{},\"fs\":{},\"b\":{},\"i\":{},\"s\":{},\"e\":{}}}",
                r.line, r.x, r.baseline, r.font_size, r.bold, r.italic, r.range.0, r.range.1
            ));
        }
        s.push_str("]}");
    }
    s.push(']');
    s.into_bytes()
}


/// 整页栅格化（PDF 原版页，模式 B）→ RGBA 缓冲 + 尺寸。0 = 成功。
#[no_mangle]
pub unsafe extern "C" fn rrk_render_page(
    handle: BookHandle,
    page_index: u32,
    target_width: u32,
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
    out_w: *mut u32,
    out_h: *mut u32,
) -> u32 {
    guard(|| {
        if out_ptr.is_null() || out_len.is_null() || out_w.is_null() || out_h.is_null() {
            return Err(set_kind(RrkError::invalid("空指针")));
        }
        with_kernel(|k| {
            let book = k.book(handle).map_err(set_kind)?;
            match book.render_page(page_index, target_width).map_err(set_kind)? {
                rrk_model::RenderResult::Bitmap { width, height, rgba } => {
                    *out_w = width;
                    *out_h = height;
                    write_buf(rgba, out_ptr, out_len);
                    Ok(0)
                }
                _ => Err(set_kind(RrkError::unsupported("非位图渲染结果"))),
            }
        })
    })
}


// ---------------------------------------------------------------------------
// A 批：缓存接线 + 进度/命中/预取
// ---------------------------------------------------------------------------

fn style_of(
    font_family: &str,
    font_size: f32,
    line_height: f32,
    para_indent_em: f32,
    align: u32,
) -> rrk_model::StyleSpec {
    let mut st = rrk_model::StyleSpec::default();
    if !font_family.is_empty() {
        st.font_family = font_family.to_string();
    }
    st.font_size = font_size;
    st.line_height = line_height;
    st.para_indent_em = para_indent_em.max(0.0);
    st.align = align_of(align);
    st
}

/// FFI `align` 整数 → 模型枚举（0=Start 1=Center 2=End 3=Justify，越界按 Start）。
fn align_of(code: u32) -> rrk_model::Align {
    use rrk_model::Align;
    match code {
        1 => Align::Center,
        2 => Align::End,
        3 => Align::Justify,
        _ => Align::Start,
    }
}

fn cache_key(
    content_hash: u64,
    style: &rrk_model::StyleSpec,
    vp: &rrk_model::Viewport,
    chapter: u32,
) -> rrk_cache::CacheKey {
    rrk_cache::CacheKey::new(
        content_hash,
        rrk_model::style_hash(style),
        rrk_model::viewport_hash(vp),
        rrk_cache::CacheKind::Layout,
    )
    .with_part(chapter)
}

/// 配置三要素缓存（目录 + 内存上限字节；传空路径 = 关闭磁盘缓存）。
#[no_mangle]
pub unsafe extern "C" fn rrk_cache_configure(
    dir_ptr: *const u8,
    dir_len: usize,
    mem_bytes: usize,
) -> u32 {
    guard(|| {
        let dir = if dir_ptr.is_null() || dir_len == 0 {
            String::new()
        } else {
            match std::str::from_utf8(std::slice::from_raw_parts(dir_ptr, dir_len)) {
                Ok(s) => s.to_string(),
                Err(e) => return Err(set_kind(RrkError::invalid(format!("缓存目录非 UTF-8: {e}")))),
            }
        };
        let mut guard = CACHE.lock().map_err(|_| {
            set_err(RrkError::io("缓存锁中毒"), RrkErrorKind::Internal.code())
        })?;
        *guard = if dir.is_empty() {
            None
        } else {
            let store = rrk_cache::DiskStore::open(&dir).map_err(set_kind)?;
            Some((store, rrk_cache::MemoryCache::new(mem_bytes.max(1 << 20))))
        };
        Ok(0)
    })
}

/// 排版一章 → 页 JSON（三要素缓存口径：命中免排版；miss 排完落盘 + 保护窗口）。
#[no_mangle]
pub unsafe extern "C" fn rrk_pages_json(
    handle: BookHandle,
    chapter_id: u32,
    font_ptr: *const u8,
    font_len: usize,
    font_size: f32,
    line_height: f32,
    para_indent_em: f32,
    align: u32,
    width: f32,
    height: f32,
    dpr: f32,
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
) -> u32 {
    guard(|| {
        if out_ptr.is_null() || out_len.is_null() {
            return Err(set_kind(RrkError::invalid("空指针")));
        }
        let font_family = if font_ptr.is_null() || font_len == 0 {
            String::new()
        } else {
            match std::str::from_utf8(std::slice::from_raw_parts(font_ptr, font_len)) {
                Ok(s) => s.to_string(),
                Err(e) => return Err(set_kind(RrkError::invalid(format!("字体名非 UTF-8: {e}")))),
            }
        };
        with_kernel(|k| {
            let book = k.book(handle).map_err(set_kind)?;
            let style = style_of(&font_family, font_size, line_height, para_indent_em, align);
            let vp = rrk_model::Viewport { width, height, dpr };
            let key = cache_key(book.meta().content_hash, &style, &vp, chapter_id);

            // C1 → C2 → 排版
            let pages = {
                let mut cache_guard = CACHE.lock().map_err(|_| {
                    set_err(RrkError::io("缓存锁中毒"), RrkErrorKind::Internal.code())
                })?;
                if let Some((store, mem)) = cache_guard.as_mut() {
                    if let Some(bytes) = mem.get(&key) {
                        if let Some(pages) = rrk_model::decode_pages(bytes) {
                            Some(pages)
                        } else {
                            None
                        }
                    } else if let Some(bytes) = store.load(&key) {
                        let pages = rrk_model::decode_pages(&bytes);
                        if let Some(p) = &pages {
                            mem.protect(key);
                            mem.put(key, bytes);
                        }
                        pages
                    } else {
                        None
                    }
                } else {
                    None
                }
            };

            let pages = match pages {
                Some(p) => p,
                None => {
                    let content = book.load_chapter(chapter_id).map_err(set_kind)?;
                    let mut guard = LAYOUTER
                        .lock()
                        .map_err(|_| set_err(RrkError::io("排版器锁中毒"), RrkErrorKind::Internal.code()))?;
                    let layouter = guard.get_or_insert_with(rrk_layout::Layouter::new);
                    let pages = layouter.layout(&content, &style, &vp);
                    let payload = rrk_model::encode_pages(&pages);
                    if let Some((store, mem)) = CACHE
                        .lock()
                        .map_err(|_| set_err(RrkError::io("缓存锁中毒"), RrkErrorKind::Internal.code()))?
                        .as_mut()
                    {
                        let _ = store.save(&key, &payload);
                        mem.protect(key);
                        mem.put(key, payload);
                    }
                    pages
                }
            };

            let json = pages_to_json(&pages);
            write_buf(json, out_ptr, out_len);
            Ok(0)
        })
    })
}

/// 语义坐标 → 页下标（进度恢复；缓存命中免排版）。
#[no_mangle]
pub unsafe extern "C" fn rrk_page_at(
    handle: BookHandle,
    chapter_id: u32,
    char_offset: u32,
    font_ptr: *const u8,
    font_len: usize,
    font_size: f32,
    line_height: f32,
    para_indent_em: f32,
    align: u32,
    width: f32,
    height: f32,
    dpr: f32,
    out_page: *mut u32,
) -> u32 {
    guard(|| {
        if out_page.is_null() {
            return Err(set_kind(RrkError::invalid("空指针")));
        }
        // 复用 pages 链路（含缓存）
        let mut p: *mut u8 = std::ptr::null_mut();
        let mut n = 0usize;
        let rc = rrk_pages_json(
            handle, chapter_id, font_ptr, font_len, font_size, line_height, para_indent_em, align, width, height, dpr,
            &mut p, &mut n,
        );
        if rc != 0 {
            return Err(rc);
        }
        let json = String::from_utf8_lossy(std::slice::from_raw_parts(p, n)).into_owned();
        rrk_buf_free(p, n);
        let pages = rrk_layout::page_at(&[], rrk_model::SemanticPos { chapter: chapter_id, char_offset });
        let _ = pages;
        // 轻量扫描 JSON 的 start 字段（避免引 serde）：粗解析即可
        let mut best = 0u32;
        let mut best_start = 0u32;
        let mut first = true;
        for seg in json.split("\"index\":").skip(1) {
            let idx: u32 = seg.split(',').next().and_then(|v| v.parse().ok()).unwrap_or(0);
            let start: u32 = seg
                .split("\"start\":")
                .nth(1)
                .and_then(|v| v.split(',').next())
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let end: u32 = seg
                .split("\"end\":")
                .nth(1)
                .and_then(|v| v.split(',').next())
                .and_then(|v| v.parse().ok())
                .unwrap_or(u32::MAX);
            if first || (char_offset >= start && char_offset < end) {
                best = idx;
                best_start = start;
                first = false;
                if char_offset >= start && char_offset < end {
                    break;
                }
            }
            let _ = best_start;
        }
        *out_page = best;
        Ok(0)
    })
}

/// 页内命中测试：(页内 x, y) → 语义偏移（run 级粒度 + run 内比例）。
#[no_mangle]
pub unsafe extern "C" fn rrk_hit_test(
    handle: BookHandle,
    chapter_id: u32,
    page_index: u32,
    x: f32,
    y: f32,
    font_ptr: *const u8,
    font_len: usize,
    font_size: f32,
    line_height: f32,
    para_indent_em: f32,
    align: u32,
    width: f32,
    height: f32,
    dpr: f32,
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
) -> u32 {
    guard(|| {
        if out_ptr.is_null() || out_len.is_null() {
            return Err(set_kind(RrkError::invalid("空指针")));
        }
        let mut p: *mut u8 = std::ptr::null_mut();
        let mut n = 0usize;
        let rc = rrk_pages_json(
            handle, chapter_id, font_ptr, font_len, font_size, line_height, para_indent_em, align, width, height, dpr,
            &mut p, &mut n,
        );
        if rc != 0 {
            return Err(rc);
        }
        let json = String::from_utf8_lossy(std::slice::from_raw_parts(p, n)).into_owned();
        rrk_buf_free(p, n);

        // 找目标页的行（按累计行高定位 y）与 run（按 x 与比例推 offset）
        let page_seg = match json.split("\"index\":").nth((page_index + 1) as usize) {
            Some(s) => s,
            None => return Err(set_kind(RrkError::invalid("页越界"))),
        };
        let page_head = page_seg.split("\"runs\":[").next().unwrap_or("");
        let mut y_cursor = 0.0f32;
        let mut hit_line = 0u32;
        let mut hit_range = (0u32, 0u32);
        for (i, lseg) in page_head.split("\"s\":").skip(1).enumerate() {
            let ls: u32 = lseg.split(',').next().and_then(|v| v.parse().ok()).unwrap_or(0);
            let le: u32 = lseg
                .split("\"e\":")
                .nth(1)
                .and_then(|v| v.split(',').next())
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let lh: f32 = lseg
                .split("\"h\":")
                .nth(1)
                .and_then(|v| v.split(',').next())
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0);
            if y < y_cursor + lh {
                hit_line = i as u32;
                hit_range = (ls, le);
                break;
            }
            y_cursor += lh;
        }
        // run 内比例（x 与 run 起点/下一 run 起点）
        let runs_part = page_seg.split("\"runs\":[").nth(1).unwrap_or("");
        let mut best = hit_range.0;
        for rseg in runs_part.split("{\"l\":") {
            let rl: u32 = rseg.split(',').next().and_then(|v| v.parse().ok()).unwrap_or(u32::MAX);
            if rl != hit_line {
                continue;
            }
            let rx: f32 = rseg
                .split("\"x\":")
                .nth(1)
                .and_then(|v| v.split(',').next())
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0);
            let rs: u32 = rseg
                .split("\"s\":")
                .nth(1)
                .and_then(|v| v.split(',').next())
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let re: u32 = rseg
                .split("\"e\":")
                .nth(1)
                .and_then(|v| v.split(',').next())
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            if x >= rx {
                best = rs + (((x - rx) as u32 / 20) * 3).min(re.saturating_sub(rs));
            }
        }
        let mut out = String::with_capacity(64);
        out.push_str(&format!(
            "{{\"offset\":{},\"line\":{},\"range\":[{},{}]}}",
            best, hit_line, hit_range.0, hit_range.1
        ));
        write_buf(out.into_bytes(), out_ptr, out_len);
        Ok(0)
    })
}

/// 远跳预取：预热本章布局 + 保护窗口（当前章 + 相邻章）。
#[no_mangle]
pub unsafe extern "C" fn rrk_prefetch(
    handle: BookHandle,
    chapter_id: u32,
    font_ptr: *const u8,
    font_len: usize,
    font_size: f32,
    line_height: f32,
    para_indent_em: f32,
    align: u32,
    width: f32,
    height: f32,
    dpr: f32,
) -> u32 {
    guard(|| {
        // 当前章与相邻章各排一遍（走缓存链路自动落盘/保护）
        for target in [chapter_id.saturating_sub(1), chapter_id, chapter_id + 1] {
            let mut p: *mut u8 = std::ptr::null_mut();
            let mut n = 0usize;
            let rc = rrk_pages_json(
                handle, target, font_ptr, font_len, font_size, line_height, para_indent_em, align, width, height, dpr,
                &mut p, &mut n,
            );
            if rc == 0 {
                rrk_buf_free(p, n);
            }
        }
        Ok(0)
    })
}
