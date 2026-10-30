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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use rrk_kernel::{shared_registry, Kernel, OpenIndexStore};

/// 书句柄（0 = 非法）。本地定义（与 `rrk_kernel::BookHandle` 透明等价）：
/// cbindgen `parse_deps=false` 看不到依赖 crate 里的别名，必须在本 crate 可见。
pub type BookHandle = u64;
/// 检索会话句柄（0 = 非法）。
pub type SearchId = u64;
/// BM25 索引会话句柄（0 = 非法）。
pub type IndexId = u64;
use rrk_model::{
    Block, BookMeta, ChapterEntry, LinkRange, PagedContent, ResourceKey, RrkError, RrkErrorKind,
    TocNode,
};

/// ABI 版本号（Dart 启动校验；只增不改语义）。
/// v8 = `rrk_last_error_code` / `rrk_content_cache_*`（issue #11、#12）；
/// v9 = blocks JSON 改**部件流**（文本区间+图片 key，不再内嵌正文）+
///      `rrk_search_chapter` / `rrk_image_thumbnail` / `rrk_layout_cancel` /
///      `rrk_stats` / 进度与坐标 API / 排版器池（并行排版）；
/// v10 = `rrk_load_headings`（issue #13：章标题块轻量出口，另 EPUB 无 TOC 书
///       open 期自动回填首个最小级标题）；
/// v11 = `rrk_layout_cancel_book`（按书取消在飞排版；并发契约加固：per-request
///       取消令牌、句柄 generation 防 ABA、锁中毒自恢复、重活出锁）。
pub const RRK_ABI_VERSION: u32 = 11;

/// panic 被捕获时的错误码。
pub const ERR_PANIC: u32 = 2001;

static KERNEL: Mutex<Option<Kernel>> = Mutex::new(None);
/// 排版器（粗粒度共享一份；parley FontContext 重量级）。
/// 排版器池（ROADMAP：多窗口并行排版）：每台独立 parley FontContext，
/// 轮转取用——并行排版吞吐随池大小叠加；池大小 = min(可用并行度, 4)，
/// 惰性创建（每台 FontContext 数 MB，不预热）。
fn layouter_slot() -> &'static Mutex<Option<rrk_layout::Layouter>> {
    static POOL: OnceLock<Vec<Mutex<Option<rrk_layout::Layouter>>>> = OnceLock::new();
    static RR: AtomicUsize = AtomicUsize::new(0);
    let pool = POOL.get_or_init(|| {
        let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).min(4);
        (0..n).map(|_| Mutex::new(None)).collect()
    });
    &pool[RR.fetch_add(1, Ordering::Relaxed) % pool.len()]
}
/// 排版取消令牌：每次布局请求独立一个（同一请求 = 同一 `Arc`）。
///
/// 深审 §二.3：原实现是单个全局 `AtomicBool` + 布局起点 `swap(false)`
/// 消费，两个后果：布局 B 的启动会吞掉对在飞布局 A 的取消；
/// `rrk_layout_cancel` 无差别误杀所有在飞布局。改为「登记 + 独立令牌」：
/// 取消只置位登记在册的令牌，另一请求的启动不再影响别人。
type CancelToken = Arc<AtomicBool>;

/// 在飞布局登记表：(书句柄, 令牌)。
static LAYOUTS_ACTIVE: Mutex<Vec<(BookHandle, CancelToken)>> = Mutex::new(Vec::new());

/// 在飞布局登记（RAII：构造入表、drop 出表，panic 路径也不泄漏）。
/// 句柄随登记项存于 `LAYOUTS_ACTIVE`（scoped 取消按它匹配），本体只持令牌。
struct LayoutRun {
    token: CancelToken,
}

impl LayoutRun {
    fn begin(handle: BookHandle) -> Self {
        let token = CancelToken::new(AtomicBool::new(false));
        lock_recover(&LAYOUTS_ACTIVE).push((handle, token.clone()));
        Self { token }
    }
}

impl Drop for LayoutRun {
    fn drop(&mut self) {
        let mut v = lock_recover(&LAYOUTS_ACTIVE);
        v.retain(|(_, t)| !Arc::ptr_eq(t, &self.token));
    }
}
/// 排版布局缓存命中/未命中计数（`rrk_stats` 可观测性用）。
static LAYOUT_HITS: AtomicU64 = AtomicU64::new(0);
static LAYOUT_MISSES: AtomicU64 = AtomicU64::new(0);
/// 三要素缓存（C1 内存 LRU + C2 磁盘；`rrk_cache_configure` 后启用）。
static CACHE: Mutex<Option<(rrk_cache::DiskStore, rrk_cache::MemoryCache)>> = Mutex::new(None);

thread_local! {
    /// 最近一次错误：稳定错误码（0 = 尚无记录） + 消息。
    static LAST_ERROR: RefCell<(u32, String)> = RefCell::new((0, String::new()));
}

fn set_last_error(code: u32, msg: impl Into<String>) {
    LAST_ERROR.with(|e| *e.borrow_mut() = (code, msg.into()));
}

/// 锁获取（中毒恢复）：panic 被 `catch_unwind` 兤住后 Mutex 必然中毒——
/// 但 Rust 内存模型下数据结构仍一致（无 UB），恢复继续用远好于
/// 「一次 panic → 引擎永久瘫痪」（深审 §二.1；错误码表里也不该
/// 出现「锁中毒」这种宿主无法自救的内部状态）。
fn lock_recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

fn with_kernel<T>(f: impl FnOnce(&mut Kernel) -> Result<T, u32>) -> Result<T, u32> {
    let mut guard = lock_recover(&KERNEL);
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

/// 块树 → JSON（**部件流**，v9 起）：文本给章文本的字节区间、行内图片给 key，
/// 不再内嵌正文——正文只跨 FFI 一次，大章带宽减半。宿主按序拼接（图片位插
/// `\u{FFFC}key\u{FFFC}` 标记），与旧格式的 `text` 逐字节一致（rrk-model 单测钉死）：
///
/// ```json
/// [{"type":"p","indent":true,"parts":[[0,24],["img/a.png"],[24,36]]},
///  {"type":"h","level":2,"parts":[[0,10]]},
///  {"type":"img","key":"img/b.jpg","alt":null}]
/// ```
fn blocks_json(_text: &str, blocks: &[Block]) -> Vec<u8> {
    let parts = rrk_model::block_parts(blocks);
    let mut s = String::with_capacity(96 * parts.len().max(1));
    s.push('[');
    for (i, b) in parts.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        match b {
            rrk_model::PartBlock::Heading { level, parts } => {
                s.push_str("{\"type\":\"h\",\"level\":");
                let _ = write!(s, "{level}");
                s.push_str(",\"parts\":");
                write_parts(parts, &mut s);
                s.push('}');
            }
            rrk_model::PartBlock::Paragraph { indent, parts } => {
                s.push_str("{\"type\":\"p\",\"indent\":");
                s.push_str(if *indent { "true" } else { "false" });
                s.push_str(",\"parts\":");
                write_parts(parts, &mut s);
                s.push('}');
            }
            rrk_model::PartBlock::Image { key, alt } => {
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

/// 部件数组：文本 `[start,end]`；图片 `["key"]`（顺序即语义）。
fn write_parts(parts: &[rrk_model::TextPart], s: &mut String) {
    s.push('[');
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        match p {
            rrk_model::TextPart::Text { range } => {
                let _ = write!(s, "[{},{}]", range.start, range.end);
            }
            rrk_model::TextPart::Image { key } => {
                s.push('[');
                json_str(&key.0, s);
                s.push(']');
            }
        }
    }
    s.push(']');
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
        // 锁外开书三段式（§二.4）：段 1 拿 open 索引缓存句柄（锁内，微秒级）→
        // 段 2 嗅探+解析+索引重建（锁外，重活）→ 段 3 登记句柄（锁内，微秒级）。
        let store = match with_kernel(|k| Ok::<_, u32>(k.open_index_cache())) {
            Ok(s) => s,
            Err(_) => return 0,
        };
        let idx = OpenIndexStore::new(store);
        let src = rrk_format::ContentSource::from_path(&path);
        let book: Arc<dyn rrk_format::BookSource> =
            match shared_registry().open_cached(&src, &idx) {
                Ok(b) => Arc::from(b),
                Err(e) => {
                    set_kind(e);
                    return 0;
                }
            };
        match with_kernel(|k| Ok(k.register_book(book, idx.hits(), idx.misses()))) {
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
        // 锁外载章三段式（§二.4）：段 1 查缓存（锁内，微秒级）→ miss 重活
        // 出锁（inflate+清洗+分块）→ 段 3 回填缓存（锁内）→ JSON/缓冲全程出锁。
        let (book, cached, key) =
            with_kernel(|k| k.chapter_lookup(handle, chapter_id).map_err(set_kind))?;
        let content = match cached {
            Some(c) => c,
            None => {
                let content = book.load_chapter(chapter_id).map_err(set_kind)?;
                with_kernel(|k| Ok(k.chapter_store(key, &content)))?;
                content
            }
        };
        let blocks = blocks_json(&content.text, &content.blocks);
        let links = links_json(&content.link_ranges);
        write_buf(content.text.as_bytes().to_vec(), text_ptr, text_len);
        write_buf(blocks, blocks_ptr, blocks_len);
        write_buf(links, links_ptr, links_len);
        Ok(0)
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

    // ------------------------------------------------------------------
    // 深审 §二（FFI 并发契约）复现测试：修复前红，修复后转正/由新 API 回归
    // ------------------------------------------------------------------

    /// 复现 §二.1：闭包内 panic 毒化内核锁 → 修复前后续所有调用报锁中毒、
    /// 引擎永久瘫痪（catch_unwind 兜住了异常也救不回锁）。
    #[test]
    fn poison_must_not_paralyze_engine() {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = with_kernel(|_k| -> Result<(), u32> { panic!("复现：内核闭包内 panic") });
        }));
        assert!(r.is_err(), "预期 panic 被测试内 catch_unwind 看到");
        let ok = with_kernel(|_k| Ok(7u32));
        assert_eq!(ok, Ok(7u32), "锁中毒后引擎不得永久瘫痪（必须自动恢复）");
    }

    /// 回归 §二.3（转正）：另一布局的启动不得吞掉在飞布局 A 的取消——
    /// per-request 令牌下 B 的登记不影响 A（原全局 swap 复现已不存在）。
    #[test]
    fn cancel_token_not_eaten_by_other_layout_start() {
        let a = LayoutRun::begin(1);
        let _b = LayoutRun::begin(2); // B 启动（登记）
        rrk_layout_cancel(); // 宿主取消全部
        assert!(a.token.load(Ordering::Relaxed), "A 的取消不得被 B 的启动吞掉");
        assert!(_b.token.load(Ordering::Relaxed), "B 也应在取消范围内");
    }

    /// 回归 §二.3（新增 scoped API）：cancel_book 只杀目标书的布局，
    /// 不误杀其它窗口/预取。
    #[test]
    fn cancel_book_scoped_to_one_book() {
        let a = LayoutRun::begin(11);
        let b = LayoutRun::begin(22);
        assert_eq!(rrk_layout_cancel_book(11), 0);
        assert!(a.token.load(Ordering::Relaxed), "cancel_book(11) 必须取消 A");
        assert!(!b.token.load(Ordering::Relaxed), "cancel_book(11) 不得误杀他书布局");
    }

    /// 回归 §三（hit_test 精度）：定位数学纯函数——按真实 run 宽比例插值，
    /// 越界钳制正确（旧实现硬编码 20px/字、3 字节/字，西文/其它字号全错）。
    #[test]
    fn hit_test_run_offset_interpolates_by_width() {
        use rrk_model::TextRun;
        let runs = vec![
            TextRun { line: 0, x: 0.0, baseline: 0.0, font_size: 16.0, bold: false, italic: false, width: 100.0, range: (0, 30) },
            TextRun { line: 0, x: 100.0, baseline: 0.0, font_size: 16.0, bold: true, italic: false, width: 50.0, range: (30, 60) },
            TextRun { line: 1, x: 0.0, baseline: 0.0, font_size: 16.0, bold: false, italic: false, width: 100.0, range: (60, 90) },
        ];
        let line0 = (0, 60);
        // run 内中点 → 区间中点（100px 宽 / 30 字节 → 半宽 15 字节）
        assert_eq!(run_offset_at(50.0, &runs, 0, line0), 15);
        // 跨 run：x=125 在第二 run 中点 → 30 + 15 = 45
        assert_eq!(run_offset_at(125.0, &runs, 0, line0), 45);
        // x 在行首前 → 行首；x 超最右 run → 行末
        assert_eq!(run_offset_at(-5.0, &runs, 0, line0), 0);
        assert_eq!(run_offset_at(9999.0, &runs, 0, line0), 60);
        // 无宽度（v1 旧缓存）：退化为 run 起点，不 panic 不越界
        let w0 = vec![TextRun { line: 0, x: 0.0, baseline: 0.0, font_size: 16.0, bold: false, italic: false, width: 0.0, range: (0, 30) }];
        assert_eq!(run_offset_at(15.0, &w0, 0, (0, 30)), 0);
    }

    /// 回归 §二.3：布局结束（drop）后从登记表移除，不泄漏。
    #[test]
    fn layout_run_unregisters_on_drop() {
        let before = lock_recover(&LAYOUTS_ACTIVE).len();
        {
            let _r = LayoutRun::begin(33);
            assert_eq!(lock_recover(&LAYOUTS_ACTIVE).len(), before + 1);
        }
        assert_eq!(lock_recover(&LAYOUTS_ACTIVE).len(), before);
    }

    /// 写一个临时大 TXT（单章多段）。
    fn write_big_txt(tag: &str, paragraphs: usize, para_bytes: usize) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("rrk_repro_{tag}_{}.txt", std::process::id()));
        let para = "读者内核并发复现用文本".repeat(para_bytes / 33 + 1); // 11 字×3B
        let mut s = String::from("第1章 复现\n");
        for _ in 0..paragraphs {
            s.push_str(&para);
            s.push_str("\n\n");
        }
        std::fs::write(&p, s).expect("写临时书");
        p
    }

    /// 在 `done` 置位前轮询内核锁：拿到即 true（锁可用）。
    fn poll_kernel_lock_until(done: &AtomicBool) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        while !done.load(Ordering::Relaxed) && std::time::Instant::now() < deadline {
            match KERNEL.try_lock() {
                Ok(_g) => return true,
                Err(std::sync::TryLockError::Poisoned(p)) => {
                    drop(p.into_inner());
                    return true;
                }
                Err(std::sync::TryLockError::WouldBlock) => {}
            }
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
        false
    }

    /// 复现 §二.4（open）：rrk_open 重活（嗅探+解析+索引重建）期间内核锁
    /// 必须可用——契约文档已写「重活已移出该锁」，实现与文档不符。
    #[test]
    fn kernel_lock_free_during_open() {
        let path = write_big_txt("open", 150, 40_000);
        let bytes = path.to_str().unwrap().as_bytes().to_vec();
        let started = std::sync::Arc::new(AtomicBool::new(false));
        let done = std::sync::Arc::new(AtomicBool::new(false));
        let (s2, d2) = (started.clone(), done.clone());
        let t = std::thread::spawn(move || {
            s2.store(true, Ordering::Relaxed);
            let h = unsafe { rrk_open(bytes.as_ptr(), bytes.len()) };
            d2.store(true, Ordering::Relaxed);
            h
        });
        while !started.load(Ordering::Relaxed) {
            std::thread::yield_now();
        }
        // 等 opener 真正进入临界区（否则首轮 try_lock 抢在 lock() 之前 = 假绿）
        std::thread::sleep(std::time::Duration::from_millis(5));
        let got = poll_kernel_lock_until(&done);
        let h = t.join().unwrap();
        let _ = std::fs::remove_file(&path);
        assert_ne!(h, 0, "复现载体（临时大书）必须能打开");
        assert!(got, "rrk_open 重活期间内核锁被长期占用：其它 FFI 调用（含 cancel）全被阻塞");
    }

    /// 复现 §二.4（载章）：rrk_load_chapter（inflate+清洗+分块+JSON）不得全程持锁。
    #[test]
    fn kernel_lock_free_during_load_chapter() {
        let path = write_big_txt("load", 150, 40_000);
        let bytes = path.to_str().unwrap().as_bytes().to_vec();
        let h = unsafe { rrk_open(bytes.as_ptr(), bytes.len()) };
        assert_ne!(h, 0, "复现载体（临时大书）必须能打开");
        let done = std::sync::Arc::new(AtomicBool::new(false));
        let d2 = done.clone();
        let t = std::thread::spawn(move || {
            let mut codes = Vec::new();
            for _ in 0..3 {
                let (mut tp, mut tn) = (std::ptr::null_mut::<u8>(), 0usize);
                let (mut bp, mut bn) = (std::ptr::null_mut::<u8>(), 0usize);
                let (mut lp, mut ln) = (std::ptr::null_mut::<u8>(), 0usize);
                let rc = unsafe {
                    rrk_load_chapter(h, 0, &mut tp, &mut tn, &mut bp, &mut bn, &mut lp, &mut ln)
                };
                codes.push(rc);
                unsafe {
                    rrk_buf_free(tp, tn);
                    rrk_buf_free(bp, bn);
                    rrk_buf_free(lp, ln);
                }
            }
            d2.store(true, Ordering::Relaxed);
            codes
        });
        // 等载章线程进入临界区，再开始观测锁可用性
        std::thread::sleep(std::time::Duration::from_millis(10));
        let got = poll_kernel_lock_until(&done);
        let codes = t.join().unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(codes.iter().all(|&c| c == 0), "载章必须成功: {codes:?}");
        assert!(got, "rrk_load_chapter 重活期间内核锁被长期占用");
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
        let mut guard = lock_recover(&CACHE);
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
/// `{"toc":{"hits","misses"},"chapter":{…},"outline":{…},"open":{…}}`。
/// `open` = open 索引（entries/pages/toc_raw）持久化命中（ROADMAP 章级/页级缓存持久化）。
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
        let mut out = String::with_capacity(220);
        let _ = write!(
            out,
            "{{\"toc\":{{\"hits\":{},\"misses\":{}}},\"chapter\":{{\"hits\":{},\"misses\":{}}},\"outline\":{{\"hits\":{},\"misses\":{}}},\"open\":{{\"hits\":{},\"misses\":{}}}}}",
            s.toc_hits, s.toc_misses, s.chapter_hits, s.chapter_misses, s.outline_hits, s.outline_misses,
            s.open_hits, s.open_misses
        );
        write_buf(out.into_bytes(), out_ptr, out_len);
        Ok(0)
    })
}

/// 汇总观测统计（第三方接入面）：
/// `{"open_books","searches","indexes","layout":{"hits","misses"},"content":{…}}`。
/// 排版计数为进程级累计（不随书关闭清零）；内容缓存计数见
/// [`rrk_content_cache_stats`]（可清零）。
#[no_mangle]
pub unsafe extern "C" fn rrk_stats(
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
) -> u32 {
    guard(|| {
        if out_ptr.is_null() || out_len.is_null() {
            return Err(set_kind(RrkError::invalid("空指针")));
        }
        let (books, searches, indexes, content) = with_kernel(|k| {
            Ok((k.open_count(), k.search_count(), k.index_count(), k.content_cache_stats()))
        })?;
        let mut out = String::with_capacity(320);
        let _ = write!(
            out,
            "{{\"open_books\":{books},\"searches\":{searches},\"indexes\":{indexes},\"layout\":{{\"hits\":{},\"misses\":{}}},\"content\":{{\"toc\":{{\"hits\":{},\"misses\":{}}},\"chapter\":{{\"hits\":{},\"misses\":{}}},\"outline\":{{\"hits\":{},\"misses\":{}}},\"open\":{{\"hits\":{},\"misses\":{}}}}}}}",
            LAYOUT_HITS.load(Ordering::Relaxed),
            LAYOUT_MISSES.load(Ordering::Relaxed),
            content.toc_hits,
            content.toc_misses,
            content.chapter_hits,
            content.chapter_misses,
            content.outline_hits,
            content.outline_misses,
            content.open_hits,
            content.open_misses,
        );
        write_buf(out.into_bytes(), out_ptr, out_len);
        Ok(0)
    })
}

// ---------------------------------------------------------------------------
// 进度 / 坐标映射 / 选区取文（进度条、UTF-16 绑定、标注——第三方接入面）
// ---------------------------------------------------------------------------

/// 语义坐标 → 全书进度百分比 [0,1]（估算口径，见 `Kernel::progress_of`）。
#[no_mangle]
pub unsafe extern "C" fn rrk_progress_of(
    handle: BookHandle,
    chapter: u32,
    byte_offset: u32,
    out_percent: *mut f32,
) -> u32 {
    guard(|| {
        if out_percent.is_null() {
            return Err(set_kind(RrkError::invalid("空指针")));
        }
        with_kernel(|k| {
            *out_percent = k.progress_of(handle, chapter, byte_offset).map_err(set_kind)?;
            Ok(0)
        })
    })
}

/// 进度百分比 [0,1] → `(chapter, 章内字节偏移)`（`rrk_progress_of` 的逆）。
#[no_mangle]
pub unsafe extern "C" fn rrk_locate(
    handle: BookHandle,
    percent: f32,
    out_chapter: *mut u32,
    out_offset: *mut u32,
) -> u32 {
    guard(|| {
        if out_chapter.is_null() || out_offset.is_null() {
            return Err(set_kind(RrkError::invalid("空指针")));
        }
        with_kernel(|k| {
            let (c, o) = k.locate(handle, percent).map_err(set_kind)?;
            *out_chapter = c;
            *out_offset = o;
            Ok(0)
        })
    })
}

/// 批量偏移映射：章文本 UTF-8 字节偏移 ↔ UTF-16 code unit 偏移。
///
/// `direction`：0 = 字节→UTF-16，1 = UTF-16→字节；输入 `in_ptr` 为 `count` 个
/// u32 小端，输出缓冲同样为 `count` 个 u32 小端（调用方 `rrk_buf_free`）。
#[no_mangle]
pub unsafe extern "C" fn rrk_map_offsets(
    handle: BookHandle,
    chapter: u32,
    direction: u32,
    in_ptr: *const u32,
    count: usize,
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
) -> u32 {
    guard(|| {
        if out_ptr.is_null() || out_len.is_null() {
            return Err(set_kind(RrkError::invalid("空指针")));
        }
        if count == 0 {
            write_buf(Vec::new(), out_ptr, out_len);
            return Ok(0);
        }
        if in_ptr.is_null() {
            return Err(set_kind(RrkError::invalid("空指针")));
        }
        let offsets = std::slice::from_raw_parts(in_ptr, count);
        with_kernel(|k| {
            let mapped = k
                .map_offsets(handle, chapter, direction == 0, offsets)
                .map_err(set_kind)?;
            let mut bytes = Vec::with_capacity(mapped.len() * 4);
            for v in mapped {
                bytes.extend_from_slice(&v.to_le_bytes());
            }
            write_buf(bytes, out_ptr, out_len);
            Ok(0)
        })
    })
}

/// 选区取文：`[start, end)` 夹到字符边界后返回章文本切片（UTF-8 缓冲）。
#[no_mangle]
pub unsafe extern "C" fn rrk_slice_text(
    handle: BookHandle,
    chapter: u32,
    start: u32,
    end: u32,
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
) -> u32 {
    guard(|| {
        if out_ptr.is_null() || out_len.is_null() {
            return Err(set_kind(RrkError::invalid("空指针")));
        }
        with_kernel(|k| {
            let s = k.slice_text(handle, chapter, start, end).map_err(set_kind)?;
            write_buf(s.into_bytes(), out_ptr, out_len);
            Ok(0)
        })
    })
}

/// 章标题块（issue #13 轻量出口）：JSON `[{"level":n,"text":"..."}]`，
/// 走章内容级缓存（命中免解析）；无标题/图片章返回 `[]`。
#[no_mangle]
pub unsafe extern "C" fn rrk_load_headings(
    handle: BookHandle,
    chapter: u32,
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
) -> u32 {
    guard(|| {
        if out_ptr.is_null() || out_len.is_null() {
            return Err(set_kind(RrkError::invalid("空指针")));
        }
        with_kernel(|k| {
            let hs = k.chapter_headings(handle, chapter).map_err(set_kind)?;
            let mut s = String::with_capacity(64 * hs.len().max(1));
            s.push('[');
            for (i, (level, text)) in hs.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push_str("{\"level\":");
                let _ = write!(s, "{level},\"text\":");
                json_str(text, &mut s);
                s.push('}');
            }
            s.push(']');
            write_buf(s.into_bytes(), out_ptr, out_len);
            Ok(0)
        })
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
        let mut cache_guard = lock_recover(&CACHE);
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
        LAYOUT_HITS.fetch_add(1, Ordering::Relaxed);
        return Ok(pages);
    }
    LAYOUT_MISSES.fetch_add(1, Ordering::Relaxed);

    // miss：载章 + 排版（内核锁外；排版器只在 layout 期间短暂持锁；
    // 取消走 per-request 令牌——本请求登记在册，别人启动不影响它）
    let content = book.load_chapter(chapter_id).map_err(set_kind)?;
    let pages = {
        let run = LayoutRun::begin(handle);
        let mut guard = lock_recover(&layouter_slot());
        let layouter = guard.get_or_insert_with(rrk_layout::Layouter::new);
        layouter
            .layout_cancellable(&content, style, vp, &run.token)
            .map_err(set_kind)?
    };
    let payload = rrk_model::encode_pages(&pages);
    if let Some((store, mem)) = lock_recover(&CACHE).as_mut()
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

/// 页内 x → 字节偏移（纯函数，供 `rrk_hit_test` 与单测共用）。
///
/// 按 run 真实像素宽 `width` 比例插值到字节区间（旧实现硬编码 20px/字 +
/// 3 字节/字——西文与非默认字号下选区落点全错）。越界钳制：
/// x 在行首前 → 行首；x 在最右 run 之后 → 行末；y 越界由调用方钳行后传入。
fn run_offset_at(
    x: f32,
    runs: &[rrk_model::TextRun],
    hit_line: u32,
    hit_range: (u32, u32),
) -> u32 {
    let mut best = hit_range.0;
    let mut right_edge = f32::NEG_INFINITY;
    for r in runs.iter() {
        if r.line != hit_line {
            continue;
        }
        // 未知宽度（v1 旧缓存）无法判右缘：视为延伸到无穷，避免误钄行末
        let edge = if r.width > 0.0 { r.x + r.width } else { f32::INFINITY };
        right_edge = right_edge.max(edge);
        if x >= r.x {
            let len = r.range.1.saturating_sub(r.range.0);
            let off = if r.width > 0.0 {
                let ratio = ((x - r.x) / r.width).clamp(0.0, 1.0);
                (ratio * len as f32) as u32
            } else {
                0 // v1 旧缓存无宽度：退化为 run 起点
            };
            best = (r.range.0 + off.min(len)).min(hit_range.1);
        }
    }
    if x > right_edge {
        return hit_range.1; // x 在本行最右 run 之后 → 行末
    }
    best
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

        // 找目标页的行（按累计行高定位 y）：落到行间即归上一行，
        // 超出末行钳到最后一行（旧实现 y 越界静默返回 offset 0/line 0）。
        let mut y_cursor = 0.0f32;
        let mut hit_line = (page.lines.len().saturating_sub(1)) as u32;
        let mut hit_range = (0u32, 0u32);
        for (i, l) in page.lines.iter().enumerate() {
            if y < y_cursor + l.height || i + 1 == page.lines.len() {
                hit_line = i as u32;
                hit_range = (l.start, l.end);
                break;
            }
            y_cursor += l.height;
        }
        // run 内比例（纯函数抽取，单测钉住；见 run_offset_at 文档）
        let best = run_offset_at(x, &page.runs, hit_line, hit_range);
        let mut out = String::with_capacity(64);
        out.push_str(&format!(
            "{{\"offset\":{},\"line\":{},\"range\":[{},{}]}}",
            best, hit_line, hit_range.0, hit_range.1
        ));
        write_buf(out.into_bytes(), out_ptr, out_len);
        Ok(0)
    })
}

/// 请求取消进行中的排版（尽力取消，块粒度）：布局在块间检查令牌并以
/// `Cancelled`（1005）返回。只对**登记在册**的在飞布局生效，无进行中布局
/// 时置位无人可见、天然无副作用（不残留、不影响下一次布局）。
/// 取消**所有**在飞排版（尽力语义：只对已开始的布局生效，无进行中布局时
/// 无副作用）。多窗口只想取消单个窗口时用 `rrk_layout_cancel_book`。
#[no_mangle]
pub extern "C" fn rrk_layout_cancel() -> u32 {
    guard(|| {
        for (_, token) in lock_recover(&LAYOUTS_ACTIVE).iter() {
            token.store(true, Ordering::Relaxed);
        }
        Ok(0)
    })
}

/// 按书取消在飞排版（v11 增量 ABI）：只置位目标书的布局令牌，
/// 不误杀其它窗口/预取请求（`rrk_layout_cancel` 保持「取消全部」语义）。
#[no_mangle]
pub extern "C" fn rrk_layout_cancel_book(handle: BookHandle) -> u32 {
    guard(|| {
        for (h, token) in lock_recover(&LAYOUTS_ACTIVE).iter() {
            if *h == handle {
                token.store(true, Ordering::Relaxed);
            }
        }
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

/// 解码整图并等比缩到 `max_edge`（长边）→ RGBA 缓冲 + 尺寸（第三方接入面）。
///
/// 原图已在 `max_edge` 内则不缩仅解码；AVIF/HEIC 等内核不解像素的格式报
/// `UnsupportedFormat`（1002，宿主取原始字节自行解码）。缓冲由 `rrk_buf_free` 释放。
#[no_mangle]
pub unsafe extern "C" fn rrk_image_thumbnail(
    data: *const u8,
    len: usize,
    max_edge: u32,
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
    out_w: *mut u32,
    out_h: *mut u32,
) -> u32 {
    guard(|| {
        if out_ptr.is_null() || out_len.is_null() || out_w.is_null() || out_h.is_null() {
            return Err(set_kind(RrkError::invalid("空指针")));
        }
        if data.is_null() || len == 0 {
            return Err(set_kind(RrkError::invalid("空输入")));
        }
        let bytes = std::slice::from_raw_parts(data, len);
        let t = rrk_image::thumbnail(bytes, max_edge).map_err(set_kind)?;
        *out_w = t.width;
        *out_h = t.height;
        write_buf(t.rgba, out_ptr, out_len);
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
    search_id: SearchId,
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
        let mut s = lock_recover(&session);
        let page = s.next(max_hits).map_err(set_kind)?;
        write_buf(search_page_json(&page), out_ptr, out_len);
        Ok(0)
    })
}

/// 取消检索（置标志 + 摘除会话；内存随最后一个引用释放，**不留后台线程**）。
#[no_mangle]
pub extern "C" fn rrk_search_cancel(search_id: SearchId) -> u32 {
    guard(|| with_kernel(|k| k.search_cancel(search_id).map_err(set_kind)).map(|_| 0))
}

/// 轻量**单章**搜索（第三方接入面）：不注册会话，同步扫完返回整页命中 JSON
/// （结构与 `rrk_search_next` 的批次一致，坐标同源）。
/// `max_hits = 0` 取内核默认上限（1000）。
#[no_mangle]
pub unsafe extern "C" fn rrk_search_chapter(
    handle: BookHandle,
    chapter: u32,
    query_ptr: *const u8,
    query_len: usize,
    case_sensitive: u32,
    context_chars: u32,
    max_hits: u32,
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
) -> u32 {
    guard(|| {
        if out_ptr.is_null() || out_len.is_null() {
            return Err(set_kind(RrkError::invalid("空指针")));
        }
        let query = read_utf8(query_ptr, query_len, "检索词")?;
        let opts = rrk_search::SearchOptions {
            case_sensitive: case_sensitive != 0,
            context_chars: if context_chars == 0 { 30 } else { context_chars },
        };
        // 锁外检索（§二.4）：书源锁内取出（微秒级），扫描/清洗重活不持内核锁；
        // done/cap 语义与 Kernel::search_chapter 同源（rrk_search::search_chapter_once）
        let book = with_kernel(|k| k.book_arc(handle).map_err(set_kind))?;
        let page = rrk_search::search_chapter_once(book, chapter, &query, opts, max_hits)
            .map_err(set_kind)?;
        write_buf(search_page_json(&page), out_ptr, out_len);
        Ok(0)
    })
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
    index_id: IndexId,
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
        let mut j = lock_recover(&job);
        let progress = j.step(max_chapters).map_err(set_kind)?;
        write_buf(bm25_progress_json(&progress), out_ptr, out_len);
        Ok(0)
    })
}

/// 拉取序列化索引字节的分块：从 `offset` 起取至多 `max_bytes` 字节，
/// `out_next` 回填下一个 offset（== 总长即取完）。**内核不写盘，落盘归宿主。**
#[no_mangle]
pub unsafe extern "C" fn rrk_bm25_pull(
    index_id: IndexId,
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
        let j = lock_recover(&job);
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
    index_id: IndexId,
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
        let j = lock_recover(&job);
        let hits = j.index().map_err(set_kind)?.query(&query, top_k.max(1) as usize);
        write_buf(scored_json(&hits), out_ptr, out_len);
        Ok(0)
    })
}

/// 关闭索引会话（释放索引内存）。
#[no_mangle]
pub extern "C" fn rrk_bm25_close(index_id: IndexId) -> u32 {
    guard(|| with_kernel(|k| k.bm25_close(index_id).map_err(set_kind)).map(|_| 0))
}
