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
use std::fmt::Write as _;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Mutex;

use rrk_kernel::{BookHandle, Kernel};
use rrk_model::{
    Block, BookMeta, ChapterEntry, LinkRange, PagedContent, ResourceKey, RrkError, RrkErrorKind,
    TocNode,
};

/// ABI 版本号（Dart 启动校验；只增不改语义）。
/// v8 = 新增 `rrk_last_error_code` / `rrk_content_cache_*`（issue #11、#12）。
pub const RRK_ABI_VERSION: u32 = 8;

/// panic 被捕获时的错误码。
pub const ERR_PANIC: u32 = 2001;

static KERNEL: Mutex<Option<Kernel>> = Mutex::new(None);
/// 排版器（粗粒度共享一份；parley FontContext 重量级）。
static LAYOUTER: Mutex<Option<rrk_layout::Layouter>> = Mutex::new(None);
/// 三要素缓存（C1 内存 LRU + C2 磁盘；`rrk_cache_configure` 后启用）。
static CACHE: Mutex<Option<(rrk_cache::DiskStore, rrk_cache::MemoryCache)>> = Mutex::new(None);

thread_local! {
    /// 最近一次错误：稳定错误码（0 = 尚无记录） + 消息。
    static LAST_ERROR: RefCell<(u32, String)> = RefCell::new((0, String::new()));
}

fn set_last_error(code: u32, msg: impl Into<String>) {
    LAST_ERROR.with(|e| *e.borrow_mut() = (code, msg.into()));
}

fn with_kernel<T>(f: impl FnOnce(&mut Kernel) -> Result<T, u32>) -> Result<T, u32> {
    let mut guard = KERNEL
        .lock()
        .map_err(|_| set_err(RrkError::io("内核锁中毒"), RrkErrorKind::Internal.code()))?;
    let kernel = guard.get_or_insert_with(Kernel::new);
    f(kernel)
}

fn set_err(e: RrkError, code: u32) -> u32 {
    set_last_error(code, e.to_string());
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
            set_last_error(ERR_PANIC, "内部 panic（已拦截）");
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
        let msg = LAST_ERROR.with(|e| e.borrow().1.clone());
        write_buf(msg.into_bytes(), out_ptr, out_len);
        Ok(0)
    })
}

/// 取最近一次错误的**稳定错误码**（issue #11）：`RrkErrorKind::code()`
/// （1001–1999，只增不改，码值表见 README）；panic = 2001；0 = 本线程尚无错误。
/// 与 `rrk_last_error` 的消息来自同一次错误（设置时一并写入）。
#[no_mangle]
pub extern "C" fn rrk_last_error_code() -> u32 {
    LAST_ERROR.with(|e| e.borrow().0)
}

// ---------------------------------------------------------------------------
// JSON 序列化（自研转义，零依赖）
// ---------------------------------------------------------------------------

/// JSON 字符串体转义：按「下一个需转义字符」批量拷安全段（旧实现逐字符
/// push，长文本慢且碎；控制字符仍逐个生成 \u00XX）。
fn esc(s: &str, out: &mut String) {
    let mut start = 0usize;
    for (pos, c) in s.char_indices() {
        let repl: &str = match c {
            '"' => "\\\"",
            '\\' => "\\\\",
            '\n' => "\\n",
            '\r' => "\\r",
            '\t' => "\\t",
            c if (c as u32) < 0x20 => {
                if pos > start {
                    out.push_str(&s[start..pos]);
                }
                let _ = write!(out, "\\u{:04x}", c as u32);
                start = pos + c.len_utf8();
                continue;
            }
            _ => continue,
        };
        if pos > start {
            out.push_str(&s[start..pos]);
        }
        out.push_str(repl);
        start = pos + c.len_utf8();
    }
    if start < s.len() {
        out.push_str(&s[start..]);
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
    s.push_str(",\"description\":");
    match &meta.description {
        Some(d) => json_str(d, &mut s),
        None => s.push_str("null"),
    }
    s.push_str(",\"format\":");
    json_str(&meta.format.to_string(), &mut s);
    s.push_str(",\"contentHash\":\"");
    let _ = write!(s, "{:016x}\"}}", meta.content_hash);
    s.into_bytes()
}

/// 内链区间 → JSON（`[{"start","end","href"}]`）。
fn links_json(links: &[LinkRange]) -> Vec<u8> {
    let mut s = String::with_capacity(32 * links.len().max(1));
    s.push('[');
    for (i, l) in links.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!("{{\"start\":{},\"end\":{},\"href\":", l.range.start, l.range.end));
        json_str(&l.href, &mut s);
        s.push('}');
    }
    s.push(']');
    s.into_bytes()
}

/// 页内容 → JSON（图片页给 key/mime，文本页给 text/blocks）。
fn paged_json(content: &PagedContent) -> Vec<u8> {
    match content {
        PagedContent::Image(res) => {
            let mut s = String::with_capacity(128);
            s.push_str("{\"kind\":\"image\",\"key\":");
            json_str(&res.key.0, &mut s);
            s.push_str(",\"mime\":");
            json_str(&res.mime, &mut s);
            s.push('}');
            s.into_bytes()
        }
        PagedContent::Text(c) => {
            let blocks = blocks_json(&c.text, &c.blocks);
            let mut s = String::with_capacity(256 + c.text.len());
            s.push_str("{\"kind\":\"text\",\"text\":");
            json_str(&c.text, &mut s);
            s.push_str(",\"blocks\":");
            s.push_str(&String::from_utf8_lossy(&blocks));
            s.push('}');
            s.into_bytes()
        }
    }
}

fn toc_json(nodes: &[TocNode]) -> Vec<u8> {
    let mut s = String::with_capacity(256 * nodes.len().max(1));
    write_toc(nodes, &mut s);
    s.into_bytes()
}

/// 递归写目录 JSON（旧实现每层先序列化到独立 Vec 再 from_utf8_lossy 拷进父串：
/// 整棵子树被复制一次；现直写同一 String）。
fn write_toc(nodes: &[TocNode], s: &mut String) {
    s.push('[');
    for (i, n) in nodes.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str("{\"title\":");
        json_str(&n.title, s);
        s.push_str(",\"chapter\":");
        let _ = write!(s, "{}", n.chapter);
        s.push_str(",\"anchor\":");
        match n.anchor {
            Some(a) => {
                let _ = write!(s, "{a}");
            }
            None => s.push_str("null"),
        }
        s.push_str(",\"children\":");
        write_toc(&n.children, s);
        s.push('}');
    }
    s.push(']');
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

/// 块树 → JSON（**唯一降维口径**：`rrk_model::flatten_blocks`，引用块摊平、
/// 分割线丢弃、行内图片以 `\u{FFFC}key\u{FFFC}` 内嵌）。
fn blocks_json(text: &str, blocks: &[Block]) -> Vec<u8> {
    use rrk_model::PlainBlock;
    let flat = rrk_model::flatten_blocks(text, blocks);
    let mut s = String::with_capacity(256 * flat.len().max(1));
    s.push('[');
    for (i, b) in flat.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        match b {
            PlainBlock::Heading { level, text: t } => {
                s.push_str("{\"type\":\"h\",\"level\":");
                s.push_str(&level.to_string());
                s.push_str(",\"text\":");
                json_str(t, &mut s);
                s.push('}');
            }
            PlainBlock::Paragraph { text: t, indent } => {
                s.push_str("{\"type\":\"p\",\"indent\":");
                s.push_str(if *indent { "true" } else { "false" });
                s.push_str(",\"text\":");
                json_str(t, &mut s);
                s.push('}');
            }
            PlainBlock::Image { key, alt } => {
                s.push_str("{\"type\":\"img\",\"key\":");
                json_str(&key.0, &mut s);
                s.push_str(",\"alt\":");
                match alt {
                    Some(a) => json_str(a, &mut s),
                    None => s.push_str("null"),
                }
                s.push('}');
            }
        }
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
            set_last_error(RrkErrorKind::InvalidParam.code(), "路径为空指针");
            return 0;
        }
        let bytes = std::slice::from_raw_parts(path, path_len);
        let path = match std::str::from_utf8(bytes) {
            Ok(s) => s.to_string(),
            Err(e) => {
                set_last_error(RrkErrorKind::InvalidParam.code(), format!("路径非 UTF-8: {e}"));
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
            set_last_error(ERR_PANIC, "内部 panic（已拦截）");
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
        let book = with_kernel(|k| k.book_arc(handle).map_err(set_kind))?;
        let json = meta_json(book.meta());
        write_buf(json, out_ptr, out_len);
        Ok(0)
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
            let toc = k.toc_cached(handle).map_err(set_kind)?;
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
            let chapters = k.chapters_cached(handle).map_err(set_kind)?;
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
    links_ptr: *mut *mut u8,
    links_len: *mut usize,
) -> u32 {
    guard(|| {
        if text_ptr.is_null()
            || text_len.is_null()
            || blocks_ptr.is_null()
            || blocks_len.is_null()
            || links_ptr.is_null()
            || links_len.is_null()
        {
            return Err(set_kind(RrkError::invalid("空指针")));
        }
        with_kernel(|k| {
            // 内容级缓存（issue #12）：命中免 inflate + 清洗 + 分块
            let content = k.load_chapter_cached(handle, chapter_id).map_err(set_kind)?;
            let blocks = blocks_json(&content.text, &content.blocks);
            let links = links_json(&content.link_ranges);
            write_buf(content.text.as_bytes().to_vec(), text_ptr, text_len);
            write_buf(blocks, blocks_ptr, blocks_len);
            write_buf(links, links_ptr, links_len);
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
        let book = with_kernel(|k| k.book_arc(handle).map_err(set_kind))?;
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #4 对拍：blocks_json 与 flatten_blocks 同源——引用摊平、分割线不出现。
    #[test]
    fn blocks_json_uses_flatten_contract() {
        use rrk_model::{Inline, StyleFlags, TextRange};
        let text: std::sync::Arc<str> = std::sync::Arc::from("引用正文");
        let blocks = vec![
            Block::Quote {
                blocks: vec![Block::Paragraph {
                    inlines: vec![Inline::Text {
                        range: TextRange::new(0, 12),
                        style: StyleFlags(0),
                    }],
                    indent: false,
                }],
            },
            Block::Divider,
        ];
        let json = String::from_utf8(blocks_json(&text, &blocks)).unwrap();
        assert!(json.contains("\"type\":\"p\""), "引用应摊平成段落: {json}");
        assert!(!json.contains("quote") && !json.contains("\"hr\""), "不应残留结构标记: {json}");
    }

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

    /// issue #11：`rrk_last_error_code` 与消息同源，返回稳定错误码。
    #[test]
    fn last_error_code_stable() {
        // 尚无记录 → 0（或上一个测试留下的码，但必是合法码）
        let c0 = rrk_last_error_code();
        assert!(c0 == 0 || (1001..=2001).contains(&c0), "非法码: {c0}");
        // 打开不存在的文件 → 非 0 稳定码（Io/InvalidParam 之一）
        let h = unsafe { rrk_open(b"definitely-missing.txt\0".as_ptr(), 23) };
        assert_eq!(h, 0);
        let c1 = rrk_last_error_code();
        assert!(c1 != 0, "失败后应有错误码");
        assert!((1001..=1999).contains(&c1), "应是 RrkErrorKind 稳定码: {c1}");
        // 空指针参数 → InvalidParam（1004）
        let mut n = 0usize;
        let rc = unsafe { rrk_meta_json(0, std::ptr::null_mut(), &mut n) };
        assert!(rc != 0);
        assert_eq!(rrk_last_error_code(), 1004, "空指针应为 InvalidParam");
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
        // write! 直写 String，免 format! 临时 String（每页/每行/每 run 一次）
        let _ = write!(
            s,
            "{{\"index\":{},\"start\":{},\"end\":{},\"height\":{},\"lines\":[",
            p.index, p.start.char_offset, p.end.char_offset, p.height
        );
        for (j, l) in p.lines.iter().enumerate() {
            if j > 0 {
                s.push(',');
            }
            let _ = write!(s, "{{\"s\":{},\"e\":{},\"h\":{},\"p\":{}}}", l.start, l.end, l.height, l.para_start);
        }
        s.push_str("],\"runs\":[");
        for (j, r) in p.runs.iter().enumerate() {
            if j > 0 {
                s.push(',');
            }
            let _ = write!(
                s,
                "{{\"l\":{},\"x\":{},\"y\":{},\"fs\":{},\"b\":{},\"i\":{},\"s\":{},\"e\":{}}}",
                r.line, r.x, r.baseline, r.font_size, r.bold, r.italic, r.range.0, r.range.1
            );
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
        let book = with_kernel(|k| k.book_arc(handle).map_err(set_kind))?;
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
}


/// 书类型：0=Text 1=ImageGallery 2=Paged；非法句柄返回 u32::MAX。
#[no_mangle]
pub unsafe extern "C" fn rrk_book_kind(handle: BookHandle) -> u32 {
    match with_kernel(|k| k.kind(handle).map_err(set_kind).map(|b| b.code())) {
        Ok(c) => c,
        Err(_) => u32::MAX,
    }
}

/// 页数（仅页级格式；重排格式返回 Unsupported）。
#[no_mangle]
pub unsafe extern "C" fn rrk_page_count(handle: BookHandle, out: *mut u32) -> u32 {
    guard(|| {
        if out.is_null() {
            return Err(set_kind(RrkError::invalid("空指针")));
        }
        let book = with_kernel(|k| k.book_arc(handle).map_err(set_kind))?;
        let paged = book
            .as_paged()
            .ok_or_else(|| RrkError::unsupported("该书不是页级格式（无 PagedSource 视图）"))
            .map_err(set_kind)?;
        let pages = paged.pages().map_err(set_kind)?;
        *out = pages.len() as u32;
        Ok(0)
    })
}

/// 读一页（页级）：图片页 → {kind:"image",key,mime}；文本页 → {kind:"text",text,blocks}。
#[no_mangle]
pub unsafe extern "C" fn rrk_load_page_json(
    handle: BookHandle,
    index: u32,
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
) -> u32 {
    guard(|| {
        if out_ptr.is_null() || out_len.is_null() {
            return Err(set_kind(RrkError::invalid("空指针")));
        }
        let book = with_kernel(|k| k.book_arc(handle).map_err(set_kind))?;
        let paged = book
            .as_paged()
            .ok_or_else(|| RrkError::unsupported("该书不是页级格式（无 PagedSource 视图）"))
            .map_err(set_kind)?;
        let content = paged.load_page(index).map_err(set_kind)?;
        write_buf(paged_json(&content), out_ptr, out_len);
        Ok(0)
    })
}

/// 按 key 取资源原始字节（CBZ 页图 / EPUB 图片等；mime 由宿主嗅探）。
#[no_mangle]
pub unsafe extern "C" fn rrk_load_resource(
    handle: BookHandle,
    key_ptr: *const u8,
    key_len: usize,
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
) -> u32 {
    guard(|| {
        if out_ptr.is_null() || out_len.is_null() {
            return Err(set_kind(RrkError::invalid("空指针")));
        }
        let key = if key_ptr.is_null() || key_len == 0 {
            String::new()
        } else {
            match std::str::from_utf8(std::slice::from_raw_parts(key_ptr, key_len)) {
                Ok(s) => s.to_string(),
                Err(e) => return Err(set_kind(RrkError::invalid(format!("资源 key 非 UTF-8: {e}")))),
            }
        };
        let book = with_kernel(|k| k.book_arc(handle).map_err(set_kind))?;
        let r = book
            .load_resource(&ResourceKey(key))
            .map_err(set_kind)?;
        write_buf(r.data.to_vec(), out_ptr, out_len);
        Ok(0)
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

/// 配置**内容级**缓存（issue #12：目录树 / 章文本 / 章大纲；命中免重新解析）。
///
/// key 只含 `content_hash`（不随样式/视口变）：内容变了整本失效，同文件重开
/// 命中；空路径 = 关闭。与布局缓存（`rrk_cache_configure`）互不干扰。
#[no_mangle]
pub unsafe extern "C" fn rrk_content_cache_configure(
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
        with_kernel(|k| {
            k.configure_content_cache(&dir, mem_bytes).map_err(set_kind)?;
            Ok(0)
        })
    })
}

/// 内容级缓存命中统计 JSON（issue #12：可观测性）：
/// `{"toc":{"hits","misses"},"chapter":{...},"outline":{...}}`。
#[no_mangle]
pub unsafe extern "C" fn rrk_content_cache_stats(
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
) -> u32 {
    guard(|| {
        if out_ptr.is_null() || out_len.is_null() {
            return Err(set_kind(RrkError::invalid("空指针")));
        }
        let s = with_kernel(|k| Ok(k.content_cache_stats()))?;
        let mut out = String::with_capacity(160);
        let _ = write!(
            out,
            "{{\"toc\":{{\"hits\":{},\"misses\":{}}},\"chapter\":{{\"hits\":{},\"misses\":{}}},\"outline\":{{\"hits\":{},\"misses\":{}}}}}",
            s.toc_hits, s.toc_misses, s.chapter_hits, s.chapter_misses, s.outline_hits, s.outline_misses
        );
        write_buf(out.into_bytes(), out_ptr, out_len);
        Ok(0)
    })
}

/// 从 FFI 原始参数构造样式（字体名非 UTF-8 → Invalid 参数错误）。
unsafe fn style_from_raw(
    font_ptr: *const u8,
    font_len: usize,
    font_size: f32,
    line_height: f32,
    para_indent_em: f32,
    align: u32,
) -> Result<rrk_model::StyleSpec, u32> {
    let font_family = if font_ptr.is_null() || font_len == 0 {
        String::new()
    } else {
        match std::str::from_utf8(std::slice::from_raw_parts(font_ptr, font_len)) {
            Ok(s) => s.to_string(),
            Err(e) => return Err(set_kind(RrkError::invalid(format!("字体名非 UTF-8: {e}")))),
        }
    };
    Ok(style_of(&font_family, font_size, line_height, para_indent_em, align))
}

/// 排版一章 → 结构化页（三要素缓存口径：C1 内存 → C2 磁盘 → 排版落盘 + 保护窗口）。
///
/// **锁纪律**：先取 `Arc<dyn BookSource>` 出内核锁再干活——载章（解压/清洗）、排版、
/// 磁盘缓存 IO 全在内核锁外进行，任何其它 FFI 调用不再被排版/光栅化阻塞。
/// `pages_json` / `page_at` / `hit_test` / `prefetch` 共用本函数，直连结构化数据。
fn layout_pages_cached(
    handle: BookHandle,
    chapter_id: u32,
    style: &rrk_model::StyleSpec,
    vp: &rrk_model::Viewport,
) -> Result<Vec<rrk_model::LayoutPage>, u32> {
    let book = with_kernel(|k| k.book_arc(handle).map_err(set_kind))?;
    let key = cache_key(book.meta().content_hash, style, vp, chapter_id);

    // C1 内存 → C2 磁盘（命中免排版）
    let cached = {
        let mut cache_guard = CACHE.lock().map_err(|_| {
            set_err(RrkError::io("缓存锁中毒"), RrkErrorKind::Internal.code())
        })?;
        match cache_guard.as_mut() {
            Some((store, mem)) => {
                if let Some(bytes) = mem.get(&key) {
                    rrk_model::decode_pages(bytes)
                } else if let Some(bytes) = store.load(&key) {
                    let pages = rrk_model::decode_pages(&bytes);
                    if pages.is_some() {
                        mem.protect(key);
                        mem.put(key, bytes);
                    }
                    pages
                } else {
                    None
                }
            }
            None => None,
        }
    };
    if let Some(pages) = cached {
        return Ok(pages);
    }

    // miss：载章 + 排版（内核锁外；排版器只在 layout 期间短暂持锁）
    let content = book.load_chapter(chapter_id).map_err(set_kind)?;
    let pages = {
        let mut guard = LAYOUTER
            .lock()
            .map_err(|_| set_err(RrkError::io("排版器锁中毒"), RrkErrorKind::Internal.code()))?;
        let layouter = guard.get_or_insert_with(rrk_layout::Layouter::new);
        layouter.layout(&content, style, vp)
    };
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
    Ok(pages)
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
        let style = style_from_raw(font_ptr, font_len, font_size, line_height, para_indent_em, align)?;
        let vp = rrk_model::Viewport { width, height, dpr };
        let pages = layout_pages_cached(handle, chapter_id, &style, &vp)?;
        write_buf(pages_to_json(&pages), out_ptr, out_len);
        Ok(0)
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
        // 复用同一排版链路（含缓存），直连结构化数据——不再整章 JSON 序列化后再回扫
        let style = style_from_raw(font_ptr, font_len, font_size, line_height, para_indent_em, align)?;
        let vp = rrk_model::Viewport { width, height, dpr };
        let pages = layout_pages_cached(handle, chapter_id, &style, &vp)?;
        let idx =
            rrk_layout::page_at(&pages, rrk_model::SemanticPos { chapter: chapter_id, char_offset })
                .unwrap_or(0);
        *out_page = idx as u32;
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
        let style = style_from_raw(font_ptr, font_len, font_size, line_height, para_indent_em, align)?;
        let vp = rrk_model::Viewport { width, height, dpr };
        let pages = layout_pages_cached(handle, chapter_id, &style, &vp)?;
        let page = pages
            .get(page_index as usize)
            .ok_or_else(|| set_kind(RrkError::invalid("页越界")))?;

        // 找目标页的行（按累计行高定位 y）与 run（按 x 与比例推 offset）
        let mut y_cursor = 0.0f32;
        let mut hit_line = 0u32;
        let mut hit_range = (0u32, 0u32);
        for (i, l) in page.lines.iter().enumerate() {
            if y < y_cursor + l.height {
                hit_line = i as u32;
                hit_range = (l.start, l.end);
                break;
            }
            y_cursor += l.height;
        }
        // run 内比例（x 与 run 起点，按平均字宽外推；钳制在 run 范围内）
        let mut best = hit_range.0;
        for r in page.runs.iter() {
            if r.line != hit_line {
                continue;
            }
            if x >= r.x {
                best = r.range.0
                    + (((x - r.x) as u32 / 20) * 3).min(r.range.1.saturating_sub(r.range.0));
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
        // 当前章与相邻章各排一遍（走缓存链路自动落盘/保护；只排版，不产 JSON、不持内核锁）
        let style = style_from_raw(font_ptr, font_len, font_size, line_height, para_indent_em, align)?;
        let vp = rrk_model::Viewport { width, height, dpr };
        for target in [chapter_id.saturating_sub(1), chapter_id, chapter_id + 1] {
            let _ = layout_pages_cached(handle, target, &style, &vp);
        }
        Ok(0)
    })
}

// ---------------------------------------------------------------------------
// 图像工具（issue #8）：纯字节能力，与书句柄无关
// ---------------------------------------------------------------------------

/// 只读图片头取尺寸：成功 → `{"width","height","format","mime"}` + code 0；
/// 无法识别（含空指针/畸形）→ code 2，不产出缓冲。
///
/// **不解码像素**：只扫头部（PNG IHDR / JPEG SOFn / GIF / BMP / WebP chunk），
/// 供宿主做行内插图测量与页尺寸表，多 MB 图片也不产生分配。
#[no_mangle]
pub unsafe extern "C" fn rrk_image_dimensions(
    data: *const u8,
    len: usize,
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
) -> u32 {
    guard(|| {
        if out_ptr.is_null() || out_len.is_null() {
            return Err(set_kind(RrkError::invalid("空指针")));
        }
        *out_ptr = std::ptr::null_mut();
        *out_len = 0;
        if data.is_null() || len == 0 {
            return Ok(2); // 空输入 = 无尺寸（软结果）
        }
        let bytes = std::slice::from_raw_parts(data, len);
        match rrk_image::dimensions(bytes) {
            Some(s) => {
                let mut json = String::with_capacity(96);
                json.push_str(&format!("{{\"width\":{},\"height\":{},\"format\":", s.width, s.height));
                json_str(s.format.name(), &mut json);
                json.push_str(",\"mime\":");
                json_str(s.format.mime(), &mut json);
                json.push('}');
                write_buf(json.into_bytes(), out_ptr, out_len);
                Ok(0)
            }
            None => Ok(2),
        }
    })
}

/// BGRA 原始位图 → PNG 字节（code 0 = 成功，缓冲由调用方 `rrk_buf_free` 释放）。
///
/// `src/src_len` 为**调用方持有**的位图缓冲（内核只读、不接管所有权，零拷贝入参）；
/// `stride` 传 0 = 紧凑 `width*4`。逐行 BGRA→RGBA 后直接 deflate，不打整幅中间缓冲。
#[no_mangle]
pub unsafe extern "C" fn rrk_encode_png_bgra(
    width: u32,
    height: u32,
    stride: usize,
    src: *const u8,
    src_len: usize,
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
) -> u32 {
    guard(|| {
        if out_ptr.is_null() || out_len.is_null() {
            return Err(set_kind(RrkError::invalid("空指针")));
        }
        *out_ptr = std::ptr::null_mut();
        *out_len = 0;
        if src.is_null() || src_len == 0 {
            return Err(set_kind(RrkError::invalid("位图缓冲为空")));
        }
        let pixels = std::slice::from_raw_parts(src, src_len);
        let png = rrk_image::encode_png_bgra(width, height, stride, pixels).map_err(set_kind)?;
        write_buf(png, out_ptr, out_len);
        Ok(0)
    })
}

// ---------------------------------------------------------------------------
// 检索（issue #9）：全书搜索（拉模式分块 + 可取消）与 BM25 索引
// ---------------------------------------------------------------------------

/// 读调用方传入的 UTF-8 串（空指针/零长 → 空串）。
unsafe fn read_utf8(ptr: *const u8, len: usize, what: &str) -> Result<String, u32> {
    if ptr.is_null() || len == 0 {
        return Ok(String::new());
    }
    match std::str::from_utf8(std::slice::from_raw_parts(ptr, len)) {
        Ok(s) => Ok(s.to_string()),
        Err(e) => Err(set_kind(RrkError::invalid(format!("{what}非 UTF-8: {e}")))),
    }
}

/// 检索批次 → JSON：
/// `{"hits":[{"chapter","offset","len","snippet","snippetOffset"}],"scanned","total","done"}`
fn search_page_json(page: &rrk_search::SearchPage) -> Vec<u8> {
    let mut s = String::with_capacity(96 * page.hits.len().max(1) + 64);
    s.push_str("{\"hits\":[");
    for (i, h) in page.hits.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            "{{\"chapter\":{},\"offset\":{},\"len\":{},\"snippet\":",
            h.chapter, h.offset, h.len
        ));
        json_str(&h.snippet, &mut s);
        s.push_str(&format!(",\"snippetOffset\":{}}}", h.snippet_offset));
    }
    s.push_str(&format!(
        "],\"scanned\":{},\"total\":{},\"done\":{}}}",
        page.scanned,
        page.total,
        if page.done { "true" } else { "false" }
    ));
    s.into_bytes()
}

/// 索引进度 → JSON：`{"built","total","done","terms","bytes"}`
fn bm25_progress_json(p: &rrk_kernel::Bm25Progress) -> Vec<u8> {
    format!(
        "{{\"built\":{},\"total\":{},\"done\":{},\"terms\":{},\"bytes\":{}}}",
        p.built,
        p.total,
        if p.done { "true" } else { "false" },
        p.terms,
        p.bytes
    )
    .into_bytes()
}

/// BM25 命中 → JSON：`[{"chapter","score"}]`
fn scored_json(hits: &[rrk_search::ScoredHit]) -> Vec<u8> {
    let mut s = String::with_capacity(32 * hits.len().max(1) + 2);
    s.push('[');
    for (i, h) in hits.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!("{{\"chapter\":{},\"score\":{:.4}}}", h.chapter, h.score));
    }
    s.push(']');
    s.into_bytes()
}

/// 建立检索会话；成功返回 `search_id`（≥1），失败返回 0。
///
/// `case_sensitive` 非 0 = 区分大小写（仅 ASCII 折叠）；`context_chars` = 命中片段左右
/// 各取多少字节上下文（0 用默认 30）。
#[no_mangle]
pub unsafe extern "C" fn rrk_search_open(
    handle: BookHandle,
    query_ptr: *const u8,
    query_len: usize,
    case_sensitive: u32,
    context_chars: u32,
) -> u64 {
    match catch_unwind(AssertUnwindSafe(|| {
        let query = match read_utf8(query_ptr, query_len, "检索词") {
            Ok(q) => q,
            Err(_) => return 0,
        };
        let opts = rrk_search::SearchOptions {
            case_sensitive: case_sensitive != 0,
            context_chars: if context_chars == 0 { 30 } else { context_chars },
        };
        with_kernel(|k| k.search_open(handle, &query, opts).map_err(set_kind)).unwrap_or(0)
    })) {
        Ok(id) => id,
        Err(_) => {
            set_last_error(ERR_PANIC, "内部 panic（已拦截）");
            0
        }
    }
}

/// 拉取一批命中（**拉模式分块**）；批次 JSON 见 `search_page_json`。
///
/// 扫描在**会话锁**内进行、内核锁已提前释放，故另一线程的 `rrk_search_cancel`
/// 能立刻置位并让本调用在下一个检查点返回 `Cancelled`（1005）。
#[no_mangle]
pub unsafe extern "C" fn rrk_search_next(
    search_id: rrk_kernel::SearchId,
    max_hits: u32,
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
) -> u32 {
    guard(|| {
        if out_ptr.is_null() || out_len.is_null() {
            return Err(set_kind(RrkError::invalid("空指针")));
        }
        *out_ptr = std::ptr::null_mut();
        *out_len = 0;
        let (session, _cancel) =
            with_kernel(|k| k.search_session(search_id).map_err(set_kind))?;
        let mut s = session.lock().map_err(|_| {
            set_err(RrkError::io("检索会话锁中毒"), RrkErrorKind::Internal.code())
        })?;
        let page = s.next(max_hits).map_err(set_kind)?;
        write_buf(search_page_json(&page), out_ptr, out_len);
        Ok(0)
    })
}

/// 取消检索（置标志 + 摘除会话；内存随最后一个引用释放，**不留后台线程**）。
#[no_mangle]
pub extern "C" fn rrk_search_cancel(search_id: rrk_kernel::SearchId) -> u32 {
    guard(|| with_kernel(|k| k.search_cancel(search_id).map_err(set_kind)).map(|_| 0))
}

/// 开一个 BM25 索引会话；成功返回 `index_id`（≥1），失败返回 0。
#[no_mangle]
pub extern "C" fn rrk_bm25_open(handle: BookHandle) -> u64 {
    match catch_unwind(AssertUnwindSafe(|| {
        with_kernel(|k| k.bm25_open(handle).map_err(set_kind)).unwrap_or(0)
    })) {
        Ok(id) => id,
        Err(_) => {
            set_last_error(ERR_PANIC, "内部 panic（已拦截）");
            0
        }
    }
}

/// 从宿主落盘的索引字节载入（跨启动复用）；成功返回 `index_id`，失败返回 0。
#[no_mangle]
pub unsafe extern "C" fn rrk_bm25_load(data_ptr: *const u8, data_len: usize) -> u64 {
    match catch_unwind(AssertUnwindSafe(|| {
        if data_ptr.is_null() || data_len == 0 {
            set_last_error(RrkErrorKind::InvalidParam.code(), "索引字节为空");
            return 0;
        }
        let bytes = std::slice::from_raw_parts(data_ptr, data_len);
        with_kernel(|k| k.bm25_load(bytes).map_err(set_kind)).unwrap_or(0)
    })) {
        Ok(id) => id,
        Err(_) => {
            set_last_error(ERR_PANIC, "内部 panic（已拦截）");
            0
        }
    }
}

/// 分组推进索引构建（每次最多 `max_chapters` 章）；返回进度 JSON。
///
/// 宿主反复调用直到 `done=true`，期间可随时停手/放弃（`rrk_bm25_close`）。
#[no_mangle]
pub unsafe extern "C" fn rrk_bm25_step(
    index_id: rrk_kernel::IndexId,
    max_chapters: u32,
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
) -> u32 {
    guard(|| {
        if out_ptr.is_null() || out_len.is_null() {
            return Err(set_kind(RrkError::invalid("空指针")));
        }
        *out_ptr = std::ptr::null_mut();
        *out_len = 0;
        let job = with_kernel(|k| k.bm25_job(index_id).map_err(set_kind))?;
        let mut j = job
            .lock()
            .map_err(|_| set_err(RrkError::io("索引会话锁中毒"), RrkErrorKind::Internal.code()))?;
        let progress = j.step(max_chapters).map_err(set_kind)?;
        write_buf(bm25_progress_json(&progress), out_ptr, out_len);
        Ok(0)
    })
}

/// 拉取序列化索引字节的分块：从 `offset` 起取至多 `max_bytes` 字节，
/// `out_next` 回填下一个 offset（== 总长即取完）。**内核不写盘，落盘归宿主。**
#[no_mangle]
pub unsafe extern "C" fn rrk_bm25_pull(
    index_id: rrk_kernel::IndexId,
    offset: u64,
    max_bytes: usize,
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
    out_next: *mut u64,
) -> u32 {
    guard(|| {
        if out_ptr.is_null() || out_len.is_null() || out_next.is_null() {
            return Err(set_kind(RrkError::invalid("空指针")));
        }
        *out_ptr = std::ptr::null_mut();
        *out_len = 0;
        let job = with_kernel(|k| k.bm25_job(index_id).map_err(set_kind))?;
        let j = job
            .lock()
            .map_err(|_| set_err(RrkError::io("索引会话锁中毒"), RrkErrorKind::Internal.code()))?;
        j.index().map_err(set_kind)?; // 未建完则报错（中文原因带进度）
        let bytes = j.bytes();
        let start = (offset as usize).min(bytes.len());
        let end = start.saturating_add(max_bytes.max(1)).min(bytes.len());
        *out_next = end as u64;
        write_buf(bytes[start..end].to_vec(), out_ptr, out_len);
        Ok(0)
    })
}

/// BM25 查询：返回按得分降序的前 `top_k` 章 `[{"chapter","score"}]`。
#[no_mangle]
pub unsafe extern "C" fn rrk_bm25_query(
    index_id: rrk_kernel::IndexId,
    query_ptr: *const u8,
    query_len: usize,
    top_k: u32,
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
) -> u32 {
    guard(|| {
        if out_ptr.is_null() || out_len.is_null() {
            return Err(set_kind(RrkError::invalid("空指针")));
        }
        *out_ptr = std::ptr::null_mut();
        *out_len = 0;
        let query = read_utf8(query_ptr, query_len, "查询词")?;
        if query.trim().is_empty() {
            return Err(set_kind(RrkError::invalid("查询词为空")));
        }
        let job = with_kernel(|k| k.bm25_job(index_id).map_err(set_kind))?;
        let j = job
            .lock()
            .map_err(|_| set_err(RrkError::io("索引会话锁中毒"), RrkErrorKind::Internal.code()))?;
        let hits = j.index().map_err(set_kind)?.query(&query, top_k.max(1) as usize);
        write_buf(scored_json(&hits), out_ptr, out_len);
        Ok(0)
    })
}

/// 关闭索引会话（释放索引内存）。
#[no_mangle]
pub extern "C" fn rrk_bm25_close(index_id: rrk_kernel::IndexId) -> u32 {
    guard(|| with_kernel(|k| k.bm25_close(index_id).map_err(set_kind)).map(|_| 0))
}
