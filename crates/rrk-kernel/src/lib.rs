//! rrk-kernel：会话编排层（方案 §1 的 `rrk-kernel`）。
//!
//! 职责：内核生命周期、格式注册、**句柄表**（FFI 边界的书句柄托管）、
//! **检索会话表**（全书搜索与 BM25 索引构建的句柄化托管）。
//! 任务调度（rayon 优先级池）随 P1 排版内核进入；本层先把「打开/关闭/取书源」钉牢。

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use rrk_cache::{CacheKey, CacheKind, DiskStore, MemoryCache};
use rrk_cbz::CbzAdapter;
use rrk_epub::EpubAdapter;
use rrk_format::{BookSource, ContentSource, FormatRegistry, IndexCache, PagedSource};
use rrk_mobi::MobiAdapter;
use rrk_model::{
    flatten_blocks_spans, Block, BookKind, ChapterContent, ChapterEntry, ChapterKind, Inline,
    RrkError, RrkResult, TocNode,
};
use rrk_pdf::PdfAdapter;
use rrk_search::{search_text, Bm25, Bm25Builder, SearchOptions, SearchPage, SearchSession};
use rrk_txt::TxtAdapter;

mod codec;

/// 内容级缓存命中统计（issue #12：可观测性）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContentCacheStats {
    /// 目录树命中/未命中次数。
    pub toc_hits: u64,
    pub toc_misses: u64,
    /// 章文本命中/未命中次数。
    pub chapter_hits: u64,
    pub chapter_misses: u64,
    /// 章大纲命中/未命中次数。
    pub outline_hits: u64,
    pub outline_misses: u64,
    /// open 索引（entries/pages/toc_raw）命中/未命中次数
    /// （ROADMAP 章级/页级缓存持久化：open 期免重建）。
    pub open_hits: u64,
    pub open_misses: u64,
}

/// open 索引缓存句柄（缓存句柄下沉 adapter，ROADMAP 章级/页级缓存持久化）。
///
/// 只走 C2 磁盘层：open 索引一次性使用，不进 C1 内存 LRU 抢容量；
/// 读写失败不致命（未命中/丢弃由 adapter 重建）。未配置缓存时不计数。
struct OpenIndexStore<'a> {
    store: Option<&'a DiskStore>,
    hits: std::sync::atomic::AtomicU64,
    misses: std::sync::atomic::AtomicU64,
}

impl<'a> OpenIndexStore<'a> {
    fn new(store: Option<&'a DiskStore>) -> Self {
        use std::sync::atomic::AtomicU64;
        Self {
            store,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }
}

impl IndexCache for OpenIndexStore<'_> {
    fn load(&self, key: &CacheKey) -> Option<Vec<u8>> {
        use std::sync::atomic::Ordering::Relaxed;
        let store = self.store?;
        let got = store.load(key);
        if got.is_some() {
            self.hits.fetch_add(1, Relaxed);
        } else {
            self.misses.fetch_add(1, Relaxed);
        }
        got
    }

    fn save(&self, key: &CacheKey, payload: &[u8]) {
        if let Some(s) = self.store {
            let _ = s.save(key, payload);
        }
    }
}

/// 书句柄（0 = 非法）。FFI 边界只传句柄，不传引用。
pub type BookHandle = u64;
/// 检索会话句柄（0 = 非法）。
pub type SearchId = u64;
/// BM25 索引会话句柄（0 = 非法）。
pub type IndexId = u64;

/// 内核实例：格式注册表 + 句柄表 + 检索会话表。
pub struct Kernel {
    registry: FormatRegistry,
    books: Vec<Option<Arc<dyn BookSource>>>,
    /// 检索会话：会话本体（互斥） + 取消标志（**不取会话锁即可置位**）。
    searches: HashMap<SearchId, (Arc<Mutex<SearchSession>>, Arc<AtomicBool>)>,
    indexes: HashMap<IndexId, Arc<Mutex<Bm25Job>>>,
    next_search_id: SearchId,
    next_index_id: IndexId,
    /// 内容级缓存（目录/章文本/大纲；key 只含 content_hash，见 issue #12）。
    /// 关闭时以下访问路径退化为原样解析（无缓存语义）。
    content_cache: Option<(DiskStore, MemoryCache)>,
    content_stats: ContentCacheStats,
}

impl Default for Kernel {
    fn default() -> Self {
        Self::new()
    }
}

impl Kernel {
    /// 创建内核并注册全部落地格式（EPUB/TXT/MOBI/AZW3）。
    pub fn new() -> Self {
        let mut registry = FormatRegistry::new();
        registry.register(Box::new(EpubAdapter));
        registry.register(Box::new(TxtAdapter));
        registry.register(Box::new(MobiAdapter));
        // 页级格式也进统一句柄：重排视图（页=章）+ 固定版式视图（as_paged）
        registry.register(Box::new(CbzAdapter));
        registry.register(Box::new(PdfAdapter));
        Self {
            registry,
            books: Vec::new(),
            searches: HashMap::new(),
            indexes: HashMap::new(),
            next_search_id: 0,
            next_index_id: 0,
            content_cache: None,
            content_stats: ContentCacheStats::default(),
        }
    }

    // -----------------------------------------------------------------------
    // 内容级缓存（issue #12）：目录树 / 章文本 / 章大纲
    // -----------------------------------------------------------------------

    /// 配置内容级缓存（目录 + 内存上限字节；空路径 = 关闭）。
    ///
    /// key = `content_hash`（内容级，不随样式/视口变）：内容变了整本失效，
    /// 同文件重开命中。payload 自带 schema 号，升级/损坏自动丢弃重算。
    pub fn configure_content_cache(&mut self, dir: &str, mem_bytes: usize) -> RrkResult<()> {
        self.content_cache = if dir.trim().is_empty() {
            None
        } else {
            let store = DiskStore::open(dir)?;
            Some((store, MemoryCache::new(mem_bytes.max(1 << 20))))
        };
        Ok(())
    }

    /// 内容级缓存命中统计快照。
    pub fn content_cache_stats(&self) -> ContentCacheStats {
        self.content_stats
    }

    /// 清零命中统计。
    pub fn reset_content_cache_stats(&mut self) {
        self.content_stats = ContentCacheStats::default();
    }

    /// 按内容哈希整本失效（换文件/换版即清）；返回内存+磁盘删除条数。
    pub fn invalidate_content_cache(&mut self, content_hash: u64) -> RrkResult<usize> {
        let mut removed = 0usize;
        if let Some((store, mem)) = self.content_cache.as_mut() {
            mem.invalidate_content(content_hash);
            removed += store.invalidate_content(content_hash)?;
        }
        Ok(removed)
    }

    /// 缓存读（C1 内存 → C2 磁盘；磁盘命中回填内存）。
    fn content_cache_get(&mut self, key: &CacheKey) -> Option<Vec<u8>> {
        let (store, mem) = self.content_cache.as_mut()?;
        if let Some(bytes) = mem.get(key) {
            return Some(bytes.to_vec());
        }
        let bytes = store.load(key)?;
        mem.put(*key, bytes.clone());
        Some(bytes)
    }

    /// 缓存写（C1 + C2；落盘失败不致命，降级为仅内存）。
    fn content_cache_put(&mut self, key: CacheKey, payload: Vec<u8>) {
        if let Some((store, mem)) = self.content_cache.as_mut() {
            let _ = store.save(&key, &payload);
            mem.put(key, payload);
        }
    }

    /// 目录树（内容级缓存：命中免 nav/NCX 解析与 TOC 锚点清洗）。
    pub fn toc_cached(&mut self, handle: BookHandle) -> RrkResult<Vec<TocNode>> {
        let book = self.book_arc(handle)?;
        let key = CacheKey::content(book.meta().content_hash, CacheKind::Toc);
        if let Some(bytes) = self.content_cache_get(&key) {
            if let Some(toc) = codec::decode_toc(&bytes) {
                self.content_stats.toc_hits += 1;
                return Ok(toc);
            }
        }
        self.content_stats.toc_misses += 1;
        let toc = book.toc()?;
        self.content_cache_put(key, codec::encode_toc(&toc));
        Ok(toc)
    }

    /// 章索引（内容级缓存：命中免分章扫描，主要省 TXT 大纲重建）。
    pub fn chapters_cached(&mut self, handle: BookHandle) -> RrkResult<Vec<ChapterEntry>> {
        let book = self.book_arc(handle)?;
        let key = CacheKey::content(book.meta().content_hash, CacheKind::TxtOutline);
        if let Some(bytes) = self.content_cache_get(&key) {
            if let Some(entries) = codec::decode_outline(&bytes) {
                self.content_stats.outline_hits += 1;
                return Ok(entries);
            }
        }
        self.content_stats.outline_misses += 1;
        let entries = book.chapters()?;
        self.content_cache_put(key, codec::encode_outline(&entries));
        Ok(entries)
    }

    /// 章内容（内容级缓存：命中免 inflate + 清洗 + 分块；part = 章 id）。
    pub fn load_chapter_cached(
        &mut self,
        handle: BookHandle,
        id: u32,
    ) -> RrkResult<ChapterContent> {
        let book = self.book_arc(handle)?;
        let key = CacheKey::content(book.meta().content_hash, CacheKind::ChapterText).with_part(id);
        if let Some(bytes) = self.content_cache_get(&key) {
            if let Some(content) = codec::decode_chapter(&bytes) {
                self.content_stats.chapter_hits += 1;
                return Ok(content);
            }
        }
        self.content_stats.chapter_misses += 1;
        let content = book.load_chapter(id)?;
        self.content_cache_put(key, codec::encode_chapter(&content));
        Ok(content)
    }

    // -----------------------------------------------------------------------
    // 进度 / 坐标映射 / 选区取文（第三方接入期望：进度条、UTF-16 绑定、标注）
    // -----------------------------------------------------------------------

    /// 语义坐标 → 全书进度百分比 [0, 1]。
    ///
    /// **估算口径**：按各章权重（`estimate_weights`：重排类≈字数、CBZ 页级
    /// 均分）线性插值，用于进度条与跨设备同步；精确定位请用
    /// `locate → page_at → hit_test` 链路。越界自动夹取。
    pub fn progress_of(
        &mut self,
        handle: BookHandle,
        chapter: u32,
        byte_offset: u32,
    ) -> RrkResult<f32> {
        let book = self.book_arc(handle)?;
        let weights = book.estimate_weights()?;
        if weights.is_empty() {
            return Err(RrkError::invalid("无章索引，无法计算进度"));
        }
        let total: f64 = weights.iter().map(|(_, w)| *w as f64).sum();
        if total <= 0.0 {
            // 权重全 0（CBZ 页级）：章序均分
            let pos = weights.iter().position(|(id, _)| *id == chapter).unwrap_or(0);
            return Ok((pos as f64 / weights.len() as f64).clamp(0.0, 1.0) as f32);
        }
        let mut before = 0.0f64;
        let mut w_chapter = 0.0f64;
        for (id, w) in &weights {
            if *id == chapter {
                w_chapter = *w as f64;
                break;
            }
            before += *w as f64;
        }
        let frac = self
            .chapters_cached(handle)?
            .into_iter()
            .find(|e| e.id == chapter)
            .map(|e| {
                let len = e.byte_range.1.saturating_sub(e.byte_range.0);
                if len == 0 {
                    0.0
                } else {
                    (byte_offset as f64 / len as f64).clamp(0.0, 1.0)
                }
            })
            .unwrap_or(0.0);
        Ok(((before + w_chapter * frac) / total).clamp(0.0, 1.0) as f32)
    }

    /// 进度百分比 [0, 1] → 语义坐标（`progress_of` 的逆；同为估算口径）。
    /// 返回 `(chapter, 章内字节偏移)`；建议再经 `page_at` 精确定页。
    pub fn locate(&mut self, handle: BookHandle, percent: f32) -> RrkResult<(u32, u32)> {
        let book = self.book_arc(handle)?;
        let weights = book.estimate_weights()?;
        if weights.is_empty() {
            return Err(RrkError::invalid("无章索引，无法定位进度"));
        }
        let scaled = (percent as f64).clamp(0.0, 1.0) * weights.iter().map(|(_, w)| *w as f64).sum::<f64>();
        if scaled <= 0.0 || weights.iter().all(|(_, w)| *w == 0) {
            // 全 0 权重（CBZ）：按章序均分
            let n = weights.len();
            let idx = (((percent as f64).clamp(0.0, 1.0) * n as f64) as usize).min(n - 1);
            return Ok((weights[idx].0, 0));
        }
        let mut acc = 0.0f64;
        for (id, w) in &weights {
            let w = *w as f64;
            if acc + w >= scaled {
                let frac = if w > 0.0 { ((scaled - acc) / w).clamp(0.0, 1.0) } else { 0.0 };
                let offset = self
                    .chapters_cached(handle)?
                    .into_iter()
                    .find(|e| e.id == *id)
                    .map(|e| {
                        let len = e.byte_range.1.saturating_sub(e.byte_range.0);
                        (frac * len as f64) as u32
                    })
                    .unwrap_or(0);
                return Ok((*id, offset));
            }
            acc += w;
        }
        // target == 1.0 边界：落最后一章末尾
        let last = weights[weights.len() - 1].0;
        let offset = self
            .chapters_cached(handle)?
            .into_iter()
            .find(|e| e.id == last)
            .map(|e| (e.byte_range.1 - e.byte_range.0) as u32)
            .unwrap_or(0);
        Ok((last, offset))
    }

    /// 批量偏移映射：章文本 UTF-8 字节偏移 ↔ UTF-16 code unit 偏移。
    ///
    /// `to_utf16=true`：输入字节偏移（越界夹取、非字符边界向下取整）→ UTF-16
    /// 单元偏移；`false`：输入 UTF-16 偏移 → 字节偏移（落在代理对中间时取对首）。
    /// 绑定语言（Dart/JS/Java/Swift）字符串皆 UTF-16，标注与选区必经此映射。
    pub fn map_offsets(
        &mut self,
        handle: BookHandle,
        chapter: u32,
        to_utf16: bool,
        offsets: &[u32],
    ) -> RrkResult<Vec<u32>> {
        let text = self.load_chapter_cached(handle, chapter)?.text;
        let text = &*text;
        let mut out = vec![0u32; offsets.len()];
        // 输入可无序：排序后单趟前进扫，输出按原索引回填
        let mut order: Vec<(usize, u32)> = offsets.iter().copied().enumerate().collect();
        order.sort_by_key(|&(_, o)| o);
        let mut bpos = 0usize;
        let mut uacc = 0u32;
        for (idx, raw) in order {
            if to_utf16 {
                let mut target = (raw as usize).min(text.len());
                while target > 0 && !text.is_char_boundary(target) {
                    target -= 1; // 非边界向下取整
                }
                while bpos < target {
                    let ch = text[bpos..].chars().next().unwrap();
                    bpos += ch.len_utf8();
                    uacc += ch.len_utf16() as u32;
                }
                out[idx] = uacc;
            } else {
                while uacc < raw && bpos < text.len() {
                    let ch = text[bpos..].chars().next().unwrap();
                    if uacc + ch.len_utf16() as u32 > raw {
                        break; // 代理对中间：停在对首
                    }
                    bpos += ch.len_utf8();
                    uacc += ch.len_utf16() as u32;
                }
                out[idx] = bpos as u32;
            }
        }
        Ok(out)
    }

    /// 选区取文：把 `[start, end)` 夹到字符边界后返回章文本切片（免整章拉回）。
    pub fn slice_text(
        &mut self,
        handle: BookHandle,
        chapter: u32,
        start: u32,
        end: u32,
    ) -> RrkResult<String> {
        let content = self.load_chapter_cached(handle, chapter)?;
        let text = &*content.text;
        let mut s = (start as usize).min(text.len());
        let mut e = (end as usize).min(text.len());
        while s > 0 && !text.is_char_boundary(s) {
            s -= 1; // 起点向下取整（不半个字符）
        }
        while e < text.len() && !text.is_char_boundary(e) {
            e += 1; // 终点向上取整（选中字符保持完整）
        }
        if s > e {
            std::mem::swap(&mut s, &mut e);
        }
        Ok(text[s..e].to_string())
    }

    /// 章标题块列表（issue #13 轻量出口）：`(level, 文本)`，走章内容级缓存
    /// （命中免解析）；无标题/图片章返回空表。
    pub fn chapter_headings(
        &mut self,
        handle: BookHandle,
        chapter: u32,
    ) -> RrkResult<Vec<(u8, String)>> {
        let content = self.load_chapter_cached(handle, chapter)?;
        let mut out = Vec::new();
        for b in &content.blocks {
            let Block::Heading { level, inlines } = b else { continue };
            let mut t = String::new();
            for i in inlines {
                if let Inline::Text { range, .. } = i {
                    t.push_str(range.slice(&content.text));
                }
            }
            let t = t.trim().to_string();
            if !t.is_empty() {
                out.push((*level, t));
            }
        }
        Ok(out)
    }

    /// 打开一本书（路径），返回句柄。
    pub fn open_path(&mut self, path: &str) -> RrkResult<BookHandle> {
        self.open(&ContentSource::from_path(path))
    }

    /// 打开一本书（任意内容源）。open 索引（entries/pages/toc_raw）走内容级
    /// 缓存持久化（ROADMAP 章级/页级缓存持久化：重开命中免重建）。
    pub fn open(&mut self, src: &ContentSource) -> RrkResult<BookHandle> {
        let idx = OpenIndexStore::new(self.content_cache.as_ref().map(|(s, _)| s));
        let book: Arc<dyn BookSource> = Arc::from(self.registry.open_cached(src, &idx)?);
        self.content_stats.open_hits += idx.hits.load(std::sync::atomic::Ordering::Relaxed);
        self.content_stats.open_misses += idx.misses.load(std::sync::atomic::Ordering::Relaxed);
        // 复用空槽；句柄 = 槽位 + 1（0 永远非法）
        if let Some(pos) = self.books.iter().position(|s| s.is_none()) {
            self.books[pos] = Some(book);
            Ok((pos + 1) as BookHandle)
        } else {
            self.books.push(Some(book));
            Ok(self.books.len() as BookHandle)
        }
    }

    /// 关闭一本书（幂等；非法句柄报错）。
    pub fn close(&mut self, handle: BookHandle) -> RrkResult<()> {
        let idx = handle
            .checked_sub(1)
            .ok_or_else(|| RrkError::invalid("非法句柄 0"))? as usize;
        if idx >= self.books.len() || self.books[idx].is_none() {
            return Err(RrkError::invalid(format!("句柄已关闭或越界: {handle}")));
        }
        self.books[idx] = None;
        Ok(())
    }

    /// 取书源（句柄 → 共享引用）。
    pub fn book(&self, handle: BookHandle) -> RrkResult<&Arc<dyn BookSource>> {
        let idx = handle
            .checked_sub(1)
            .ok_or_else(|| RrkError::invalid("非法句柄 0"))? as usize;
        self.books
            .get(idx)
            .and_then(|s| s.as_ref())
            .ok_or_else(|| RrkError::invalid(format!("句柄已关闭或越界: {handle}")))
    }

    /// 取书源的共享所有权（检索/索引需要**出锁后**继续扫描，不能长时间霸占内核锁）。
    pub fn book_arc(&self, handle: BookHandle) -> RrkResult<Arc<dyn BookSource>> {
        Ok(Arc::clone(self.book(handle)?))
    }

    /// 当前打开的书数。
    pub fn open_count(&self) -> usize {
        self.books.iter().filter(|s| s.is_some()).count()
    }

    /// 书类型（统一句柄分流：Text / ImageGallery / Paged）。
    pub fn kind(&self, handle: BookHandle) -> RrkResult<BookKind> {
        Ok(self.book(handle)?.kind())
    }

    /// 固定版式视图（CBZ/PDF）；重排格式返回错误。
    pub fn paged(&self, handle: BookHandle) -> RrkResult<&dyn PagedSource> {
        self.book(handle)?
            .as_paged()
            .ok_or_else(|| RrkError::unsupported("该书不是页级格式（无 PagedSource 视图）"))
    }

    // -----------------------------------------------------------------------
    // 全书检索（issue #9）
    // -----------------------------------------------------------------------

    /// 建立检索会话（不前扫；真正扫描发生在 `search_next`）。
    pub fn search_open(
        &mut self,
        handle: BookHandle,
        query: &str,
        opts: SearchOptions,
    ) -> RrkResult<SearchId> {
        let book = self.book_arc(handle)?;
        let session = SearchSession::open(book, query, opts)?;
        let cancel = session.cancel_flag();
        self.next_search_id += 1;
        let id = self.next_search_id;
        self.searches.insert(id, (Arc::new(Mutex::new(session)), cancel));
        Ok(id)
    }

    /// 取检索会话（句柄 → 会话 + 取消标志）。**内核锁在此返回后即释放**，
    /// 调用方在会话锁内做长扫描，`search_cancel` 才能真抢占。
    pub fn search_session(
        &self,
        id: SearchId,
    ) -> RrkResult<(Arc<Mutex<SearchSession>>, Arc<AtomicBool>)> {
        self.searches
            .get(&id)
            .map(|(s, c)| (Arc::clone(s), Arc::clone(c)))
            .ok_or_else(|| RrkError::invalid(format!("检索会话不存在或已取消: {id}")))
    }

    /// 取消检索：置标志 + 从表里摘除（内存随最后一个 Arc 释放，无后台线程）。
    pub fn search_cancel(&mut self, id: SearchId) -> RrkResult<()> {
        let (_, cancel) = self
            .searches
            .remove(&id)
            .ok_or_else(|| RrkError::invalid(format!("检索会话不存在或已取消: {id}")))?;
        cancel.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    /// 轻量**单章**搜索（第三方接入面）：不注册会话、同步扫完即弃，
    /// 扫描/坐标/snippet 与全书会话同源（`SearchSession::open_chapter`）。
    ///
    /// `max_hits = 0` 取默认上限 1000；命中坐标与全书搜索一致
    /// （章文本字节偏移 + `source_anchor` 源映射）。
    pub fn search_chapter(
        &mut self,
        handle: BookHandle,
        chapter: u32,
        query: &str,
        opts: SearchOptions,
        max_hits: u32,
    ) -> RrkResult<SearchPage> {
        let book = self.book_arc(handle)?;
        let mut session = SearchSession::open_chapter(book, chapter, query, opts)?;
        let cap = if max_hits == 0 { 1000 } else { max_hits as usize };
        let mut out = SearchPage::default();
        while out.hits.len() < cap {
            let page = session.next((cap - out.hits.len()) as u32)?;
            out.hits.extend(page.hits);
            out.scanned = page.scanned;
            out.total = page.total;
            if page.done {
                break;
            }
        }
        out.hits.truncate(cap);
        Ok(out)
    }

    /// 当前存活的检索会话数（生命周期自检/测试用）。
    pub fn search_count(&self) -> usize {
        self.searches.len()
    }

    // -----------------------------------------------------------------------
    // BM25 索引（issue #9）
    // -----------------------------------------------------------------------

    /// 开一个索引构建会话（分步推进，宿主可随时停）。
    pub fn bm25_open(&mut self, handle: BookHandle) -> RrkResult<IndexId> {
        let book = self.book_arc(handle)?;
        let chapters = book.chapters()?;
        self.next_index_id += 1;
        let id = self.next_index_id;
        self.indexes.insert(id, Arc::new(Mutex::new(Bm25Job::new(book, chapters))));
        Ok(id)
    }

    /// 从宿主落盘的索引字节重新载入（跨启动复用，免重建）。
    pub fn bm25_load(&mut self, bytes: &[u8]) -> RrkResult<IndexId> {
        let index = Bm25::decode(bytes)?;
        self.next_index_id += 1;
        let id = self.next_index_id;
        self.indexes.insert(id, Arc::new(Mutex::new(Bm25Job::from_index(index, bytes.to_vec()))));
        Ok(id)
    }

    /// 取索引会话（内核锁随即释放，后续分步/查询在会话锁内进行）。
    pub fn bm25_job(&self, id: IndexId) -> RrkResult<Arc<Mutex<Bm25Job>>> {
        self.indexes
            .get(&id)
            .map(Arc::clone)
            .ok_or_else(|| RrkError::invalid(format!("索引会话不存在或已关闭: {id}")))
    }

    /// 关闭索引会话。
    pub fn bm25_close(&mut self, id: IndexId) -> RrkResult<()> {
        self.indexes
            .remove(&id)
            .map(|_| ())
            .ok_or_else(|| RrkError::invalid(format!("索引会话不存在或已关闭: {id}")))
    }

    /// 当前存活的索引会话数。
    pub fn index_count(&self) -> usize {
        self.indexes.len()
    }
}

// ---------------------------------------------------------------------------
// BM25 构建会话
// ---------------------------------------------------------------------------

/// 索引构建进度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bm25Progress {
    /// 已投喂章数。
    pub built: u32,
    /// 全书章数。
    pub total: u32,
    /// 是否已建完（此时可 `pull`/`query`）。
    pub done: bool,
    /// 唯一词项数（建完才有意义）。
    pub terms: u32,
    /// 序列化索引字节数（建完才有意义；`0` = 尚未生成）。
    pub bytes: u64,
}

/// BM25 构建会话：逐章投喂（拉模式），建完缓存索引与序列化字节。
pub struct Bm25Job {
    /// 书源（从字节载入的会话为 `None`）。
    book: Option<Arc<dyn BookSource>>,
    chapters: Vec<ChapterEntry>,
    next: usize,
    builder: Bm25Builder,
    index: Option<Bm25>,
    bytes: Vec<u8>,
}

impl Bm25Job {
    fn new(book: Arc<dyn BookSource>, chapters: Vec<ChapterEntry>) -> Self {
        Self {
            book: Some(book),
            chapters,
            next: 0,
            builder: Bm25Builder::new(),
            // 空书也在首次 `step` 里走 finalize（保证 bytes 是合法的空索引字节）
            index: None,
            bytes: Vec::new(),
        }
    }

    fn from_index(index: Bm25, bytes: Vec<u8>) -> Self {
        Self {
            book: None,
            chapters: Vec::new(),
            next: 0,
            builder: Bm25Builder::new(),
            index: Some(index),
            bytes,
        }
    }

    /// 全书章数。
    pub fn total(&self) -> u32 {
        self.chapters.len() as u32
    }

    /// 已投喂章数。
    pub fn built(&self) -> u32 {
        self.next as u32
    }

    /// 是否已可查询。
    pub fn is_done(&self) -> bool {
        self.index.is_some()
    }

    /// 序列化索引字节（未建完为空）。
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// 推进最多 `max_chapters` 章（**可反复调用，宿主可随时停手**）。
    ///
    /// 图片章无正文，计入进度但不投喂。
    pub fn step(&mut self, max_chapters: u32) -> RrkResult<Bm25Progress> {
        if self.index.is_none() {
            let book = self
                .book
                .as_ref()
                .ok_or_else(|| RrkError::invalid("索引会话缺少书源"))?;
            let limit = max_chapters.max(1) as usize;
            let mut done_this_round = 0usize;
            while self.next < self.chapters.len() && done_this_round < limit {
                let entry = &self.chapters[self.next];
                let (id, kind) = (entry.id, entry.kind);
                if kind == ChapterKind::Text {
                    let content = book.load_chapter(id)?;
                    let blocks = flatten_blocks_spans(&content.text, &content.blocks);
                    let text = search_text(&blocks);
                    if !text.is_empty() {
                        self.builder.add_doc(id, &text);
                    }
                }
                self.next += 1;
                done_this_round += 1;
            }
            if self.next >= self.chapters.len() {
                self.finalize()?;
            }
        }
        Ok(self.progress())
    }

    fn finalize(&mut self) -> RrkResult<()> {
        let builder = std::mem::take(&mut self.builder);
        let index = builder.build();
        self.bytes = index.encode();
        self.index = Some(index);
        Ok(())
    }

    fn progress(&self) -> Bm25Progress {
        Bm25Progress {
            built: self.built(),
            total: self.total(),
            done: self.index.is_some(),
            terms: self.index.as_ref().map(|i| i.term_count() as u32).unwrap_or(0),
            bytes: self.bytes.len() as u64,
        }
    }

    /// 已建好的索引（未建完则报错）。
    pub fn index(&self) -> RrkResult<&Bm25> {
        self.index.as_ref().ok_or_else(|| {
            RrkError::invalid(format!(
                "索引尚未建完（{}/{} 章），请继续 step",
                self.built(),
                self.total()
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_lifecycle() {
        let mut k = Kernel::new();
        // 未知格式 → UnsupportedFormat；句柄表不被污染
        let bad = ContentSource::from_bytes(Some("x.bin".into()), &b"\x01\x02\x03\x04"[..]);
        let err = k.open(&bad).unwrap_err();
        assert_eq!(err.kind, rrk_model::RrkErrorKind::UnsupportedFormat);
        assert_eq!(k.open_count(), 0);
    }

    /// issue #12：内容级缓存端到端——首次 miss 解析，重开同文件命中磁盘缓存。
    #[test]
    fn content_cache_hit_across_reopen() {
        let dir = std::env::temp_dir()
            .join(format!("rrk-content-cache-{}-{:x}", std::process::id(), fastrand()));
        let dir_str = dir.to_string_lossy().into_owned();

        // 第一次开：toc/chapter 各一次 miss（写缓存），再读一次 hit
        {
            let mut k = Kernel::new();
            k.configure_content_cache(&dir_str, 8 * 1024 * 1024).unwrap();
            let h = k.open_path("../../fixtures/sample.epub").unwrap();
            let toc1 = k.toc_cached(h).unwrap();
            assert!(!toc1.is_empty(), "样例书应有目录");
            let c1 = k.load_chapter_cached(h, 0).unwrap();
            assert!(!c1.text.is_empty());
            let ch1 = k.chapters_cached(h).unwrap();
            assert!(!ch1.is_empty());
            // 再读一轮：全部命中且内容一致
            assert_eq!(k.toc_cached(h).unwrap(), toc1, "命中缓存的目录应一致");
            let c2 = k.load_chapter_cached(h, 0).unwrap();
            assert_eq!(&*c2.text, &*c1.text);
            assert_eq!(c2.blocks, c1.blocks);
            assert_eq!(c2.link_ranges, c1.link_ranges);
            assert_eq!(k.chapters_cached(h).unwrap(), ch1);
            let s = k.content_cache_stats();
            assert_eq!((s.toc_hits, s.toc_misses), (1, 1));
            assert_eq!((s.chapter_hits, s.chapter_misses), (1, 1));
            assert_eq!((s.outline_hits, s.outline_misses), (1, 1));
        }

        // 新内核实例（模拟重启）：同一文件重开，磁盘缓存直接命中，零 miss
        {
            let mut k = Kernel::new();
            k.configure_content_cache(&dir_str, 8 * 1024 * 1024).unwrap();
            let h = k.open_path("../../fixtures/sample.epub").unwrap();
            let toc = k.toc_cached(h).unwrap();
            assert!(!toc.is_empty());
            let s = k.content_cache_stats();
            assert_eq!((s.toc_hits, s.toc_misses), (1, 0), "重开应磁盘命中");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ROADMAP 章级/页级缓存持久化：open 索引（entries/pages/toc_raw）内容级
    /// 持久化端到端——首次 open miss 重建并落盘，新内核实例（模拟重启）重开
    /// 命中磁盘缓存，免 open 期重建。
    #[test]
    fn open_index_cache_hit_across_reopen() {
        let dir = std::env::temp_dir()
            .join(format!("rrk-open-index-{}-{:x}", std::process::id(), fastrand()));
        let dir_str = dir.to_string_lossy().into_owned();

        // 第一次开：miss 重建 + 落盘
        {
            let mut k = Kernel::new();
            k.configure_content_cache(&dir_str, 8 * 1024 * 1024).unwrap();
            let _ = k.open_path("../../fixtures/sample.epub").unwrap();
            let s = k.content_cache_stats();
            assert_eq!((s.open_hits, s.open_misses), (0, 1), "首次 open 应 miss 并落盘");
        }

        // 新内核实例（模拟重启）：重开命中磁盘 open 索引，目录/章表照常可用
        {
            let mut k = Kernel::new();
            k.configure_content_cache(&dir_str, 8 * 1024 * 1024).unwrap();
            let h = k.open_path("../../fixtures/sample.epub").unwrap();
            assert!(!k.toc_cached(h).unwrap().is_empty(), "目录应可用");
            assert!(!k.chapters_cached(h).unwrap().is_empty(), "章表应可用");
            let s = k.content_cache_stats();
            assert_eq!((s.open_hits, s.open_misses), (1, 0), "重开应 open 索引磁盘命中");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn fastrand() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64
    }

    /// 进度 API：progress_of 与 locate 互逆（估算口径），单调且越界夹取。
    #[test]
    fn progress_and_locate_roundtrip() {
        let mut k = Kernel::new();
        let h = k.open_path("../../fixtures/sample.txt").unwrap();
        let p0 = k.progress_of(h, 0, 0).unwrap();
        assert!((p0 - 0.0).abs() < 1e-6, "首页应为 0：{p0}");
        let p1 = k.progress_of(h, 1, 0).unwrap();
        assert!(p1 > 0.5, "第二章起点应过半：{p1}");
        let plast = k.progress_of(h, 1, u32::MAX).unwrap();
        assert!((plast - 1.0).abs() < 1e-6, "末章末尾应为 1：{plast}");
        // locate 逆变换：中点应落在中间章附近，且单调
        let (c_mid, _) = k.locate(h, 0.5).unwrap();
        assert!(c_mid <= 1, "50% 应落前两章：{c_mid}");
        let (c_end, _) = k.locate(h, 1.0).unwrap();
        assert_eq!(c_end, 1, "100% 应落最后一章");
        let (c0, o0) = k.locate(h, 0.0).unwrap();
        assert_eq!((c0, o0), (0, 0), "0% 应回章首");
    }

    /// UTF-16 映射：CJK + emoji（代理对）往返一致；无序批量正确回填。
    #[test]
    fn map_offsets_utf16_roundtrip() {
        let mut k = Kernel::new();
        let h = k.open_path("../../fixtures/sample.txt").unwrap();
        // 用章内字节偏移采样，映射到 UTF-16 再映射回来
        let text_len = {
            let c = k.load_chapter_cached(h, 0).unwrap();
            c.text.len() as u32
        };
        let samples = [0u32, 3, 9, text_len / 2, text_len];
        let u16s = k.map_offsets(h, 0, true, &samples).unwrap();
        assert_eq!(u16s.len(), samples.len());
        assert!(u16s.windows(2).all(|w| w[0] <= w[1]), "单调不减");
        let back = k.map_offsets(h, 0, false, &u16s).unwrap();
        // 往返应回到字符边界上的原采样点（采样点本身是边界则完全相等）
        for (orig, &b) in samples.iter().zip(back.iter()) {
            let text = { let c = k.load_chapter_cached(h, 0).unwrap(); c.text };
            let mut o = *orig as usize;
            while o > 0 && !text.is_char_boundary(o) { o -= 1; }
            assert_eq!(b as usize, o, "往返映射不一致（orig={orig}）");
        }
        // 无序输入：乱序给入也按位回填
        let messy = [samples[3], samples[0], samples[1]];
        let u = k.map_offsets(h, 0, true, &messy).unwrap();
        assert_eq!(u[1], u16s[0]);
        assert_eq!(u[2], u16s[1]);
    }

    /// 选区取文：越界夹取 + 字符边界对齐（边界从实际文本推导，不硬编码——
    /// fixtures 在不同平台 checkout 可能是 LF/CRLF，硬编码偏移在 CI 上会切在
    /// 汉字中间——这正是首次 CI 失败的原因）。
    #[test]
    fn slice_text_clamps() {
        let mut k = Kernel::new();
        let h = k.open_path("../../fixtures/sample.txt").unwrap();
        let text = { let c = k.load_chapter_cached(h, 0).unwrap(); c.text };
        // 取第 2、5 个字符的起始字节（对行尾/偏移不敏感）
        let b1 = text.char_indices().nth(1).map(|(b, _)| b).unwrap();
        let b2 = text.char_indices().nth(5).map(|(b, _)| b).unwrap();
        assert!(b1 < b2 && text.is_char_boundary(b1) && text.is_char_boundary(b2));

        let full = k.slice_text(h, 0, 0, u32::MAX).unwrap();
        assert_eq!(full, &*text, "全范围应返回整章");
        let mid = k.slice_text(h, 0, b1 as u32, b2 as u32).unwrap();
        assert_eq!(mid, &text[b1..b2]);
        // 非边界起点（b1+1 落在多字节字符中间）→ 向下取整到 b1
        let snapped = k.slice_text(h, 0, (b1 + 1) as u32, (b1 + 2) as u32).unwrap();
        let mut s = (b1 + 1) as usize;
        while s > 0 && !text.is_char_boundary(s) { s -= 1; }
        let mut e = (b1 + 2) as usize;
        while e < text.len() && !text.is_char_boundary(e) { e += 1; }
        assert_eq!(snapped, &text[s..e]);
        // 反转区间自动交换
        let swapped = k.slice_text(h, 0, b2 as u32, b1 as u32).unwrap();
        assert_eq!(swapped, &text[b1..b2]);
    }

    /// issue #13：章标题块轻量出口（sample.epub 首章含 h1）。
    #[test]
    fn chapter_headings_returns_title_blocks() {
        let mut k = Kernel::new();
        let h = k.open_path("../../fixtures/sample.epub").unwrap();
        let hs = k.chapter_headings(h, 0).unwrap();
        assert!(!hs.is_empty(), "样例 EPUB 首章应有 h1");
        assert_eq!(hs[0].0, 1, "首个应为 h1");
        assert!(hs[0].1.contains("第一章"), "h1 文本: {}", hs[0].1);
        // 图片章（CBZ）→ 空表
        let h2 = k.open_path("../../fixtures/sample.cbz").unwrap();
        assert!(k.chapter_headings(h2, 0).unwrap().is_empty());
    }

    /// 端到端：建索引 → 拉字节 → 从字节重载 → 查询结果一致。
    #[test]
    fn bm25_build_pull_reload() {
        let mut k = Kernel::new();
        let handle = k.open(&ContentSource::from_path("../../fixtures/sample.txt")).unwrap();

        let iid = k.bm25_open(handle).unwrap();
        let job = k.bm25_job(iid).unwrap();
        let progress = job.lock().unwrap().step(10_000).unwrap();
        assert!(progress.done, "应一次建完: {progress:?}");
        assert!(progress.terms > 0, "样例书应有词项");
        let bytes = job.lock().unwrap().bytes().to_vec();
        assert!(!bytes.is_empty() && bytes.starts_with(b"RRKI"));
        let before = job.lock().unwrap().index().unwrap().query("编码", 5);

        // 宿主落盘后交回内核
        let iid2 = k.bm25_load(&bytes).unwrap();
        let after = k
            .bm25_job(iid2)
            .unwrap()
            .lock()
            .unwrap()
            .index()
            .unwrap()
            .query("编码", 5);
        assert_eq!(before, after, "重载后查询结果应逐条一致");
        assert!(!before.is_empty(), "样例书应命中「编码」");

        k.bm25_close(iid).unwrap();
        k.bm25_close(iid2).unwrap();
        assert_eq!(k.index_count(), 0);
        k.close(handle).unwrap();
    }

    #[test]
    fn search_and_index_session_lifecycle() {
        let mut k = Kernel::new();
        let handle = k.open(&ContentSource::from_path("../../fixtures/sample.txt")).unwrap();
        let sid = k.search_open(handle, "的", SearchOptions::default()).unwrap();
        assert_eq!(k.search_count(), 1);
        // 取消后句柄即失效
        k.search_cancel(sid).unwrap();
        assert_eq!(k.search_count(), 0);
        assert!(k.search_session(sid).is_err());
        assert!(k.search_cancel(sid).is_err());

        let iid = k.bm25_open(handle).unwrap();
        assert_eq!(k.index_count(), 1);
        k.bm25_close(iid).unwrap();
        assert_eq!(k.index_count(), 0);
        assert!(k.bm25_job(iid).is_err());
        k.close(handle).unwrap();
    }
}
