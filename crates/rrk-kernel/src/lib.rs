//! rrk-kernel：会话编排层（方案 §1 的 `rrk-kernel`）。
//!
//! 职责：内核生命周期、格式注册、**句柄表**（FFI 边界的书句柄托管）、
//! **检索会话表**（全书搜索与 BM25 索引构建的句柄化托管）。
//! 任务调度（rayon 优先级池）随 P1 排版内核进入；本层先把「打开/关闭/取书源」钉牢。

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use rrk_cbz::CbzAdapter;
use rrk_epub::EpubAdapter;
use rrk_format::{BookSource, ContentSource, FormatRegistry, PagedSource};
use rrk_mobi::MobiAdapter;
use rrk_model::{
    flatten_blocks_spans, BookKind, ChapterEntry, ChapterKind, RrkError, RrkResult,
};
use rrk_pdf::PdfAdapter;
use rrk_search::{search_text, Bm25, Bm25Builder, SearchOptions, SearchSession};
use rrk_txt::TxtAdapter;

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
        }
    }

    /// 打开一本书（路径），返回句柄。
    pub fn open_path(&mut self, path: &str) -> RrkResult<BookHandle> {
        self.open(&ContentSource::from_path(path))
    }

    /// 打开一本书（任意内容源）。
    pub fn open(&mut self, src: &ContentSource) -> RrkResult<BookHandle> {
        let book: Arc<dyn BookSource> = Arc::from(self.registry.open(src)?);
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
        let total = chapters.len();
        Self {
            book: Some(book),
            chapters,
            next: 0,
            builder: Bm25Builder::new(),
            index: if total == 0 { Some(Bm25Builder::new().build()) } else { None },
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
