//! rrk-search：全书检索 + BM25 索引（issue #9）。
//!
//! 两条能力，都**基于已打开的书句柄**、都遵守「搜到的 = 读到的」：
//!
//! 1. **全书搜索**（[`SearchSession`]）：拉模式分块——宿主按批 [`SearchSession::next`] 取命中，
//!    内核每次只持有一章的扁平块，跨 FFI 只传小结构；[`SearchSession::cancel`] 置原子标志，
//!    下一次检查即真停（无后台线程）。
//! 2. **BM25 索引**（[`Bm25Builder`] / [`Bm25`]）：以句柄为输入逐章投喂（可分批、可取消），
//!    产出可序列化索引字节；内核**不写盘**，落盘/缓存归宿主，重开时 [`Bm25::decode`] 交回。
//!
//! 文本真值：两者都消费 `rrk_model::flatten_blocks_spans`——与 FFI 出口
//! （`blocks_json`）**同一份正文**，保证「搜到的 = 宿主读到的」。
//!
//! 偏移坐标：命中偏移一律是 `ChapterContent.text` 的 **UTF-8 字节偏移**，与目录锚点
//! （`TocNode.anchor`）、内链（`link_ranges`）、进度（`SemanticPos`）同坐标系；
//! UTF-16 映射归宿主。

mod bm25;

pub use bm25::{Bm25, Bm25Builder, ScoredHit};

use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use rrk_format::BookSource;
use rrk_model::{
    flatten_blocks_spans, ChapterEntry, ChapterId, ChapterKind, PlainTextBlock, RrkError,
    RrkResult,
};

// ---------------------------------------------------------------------------
// 分词（CJK bigram + 西文词块）
// ---------------------------------------------------------------------------

/// CJK 判定（汉字含扩展区/兼容区、假名、谚文）。
///
/// 中日韩不用词典分词：**连续 CJK 段取 bigram**（单字段退化为 unigram），
/// 这是无词典场景下召回与精度最平衡的口径（Lucene CJKAnalyzer 同思路），
/// 且单字查询天然可命中。
pub fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF      // 平假名 / 片假名
        | 0x3400..=0x4DBF    // 汉字扩展 A
        | 0x4E00..=0x9FFF    // 汉字基本区
        | 0xF900..=0xFAFF    // 汉字兼容区
        | 0xAC00..=0xD7AF    // 谚文音节
        | 0x20000..=0x2FA1F  // 汉字扩展 B~
    )
}

/// ASCII 小写归一（**字节长度不变**，故偏移映射不受影响）。
///
/// 非 ASCII 大小写（西里尔/带变音拉丁等）不做折叠：宁可少折，不能改变偏移。
pub fn fold_ascii(s: &str) -> Cow<'_, str> {
    if s.bytes().any(|b| b.is_ascii_uppercase()) {
        Cow::Owned(s.to_ascii_lowercase())
    } else {
        Cow::Borrowed(s)
    }
}

/// 遍历词项：CJK 段出 bigram（单字出 unigram），字母/数字段出整词。
///
/// 传入文本应已做 [`fold_ascii`]（索引与查询两侧同口径）。
/// 词项是 `text` 的**借用切片**（零分配），调用方需要持有则自行 `to_string`。
pub fn for_each_term<'a>(text: &'a str, mut f: impl FnMut(&'a str)) {
    // 游标迭代（旧实现先把整段 collect 成 Vec<(usize,char)>：章级文本一次性
    // 800KB 瞬时分配；现改为 peekable 游标，零分配）。
    let mut it = text.char_indices().peekable();
    while let Some((b, c)) = it.next() {
        if is_cjk(c) {
            // 连续 CJK 段：单字出 unigram，多字相邻出 bigram（滑窗与旧口径同序）
            let first = (b, c);
            let mut prev = (b, c);
            let mut count = 1usize;
            while let Some(&(nb, nc)) = it.peek() {
                if !is_cjk(nc) {
                    break;
                }
                it.next();
                count += 1;
                f(&text[prev.0..nb + nc.len_utf8()]);
                prev = (nb, nc);
            }
            if count == 1 {
                f(&text[first.0..first.0 + first.1.len_utf8()]);
            }
        } else if c.is_alphanumeric() {
            let start = b;
            let mut end = b + c.len_utf8();
            while let Some(&(nb, nc)) = it.peek() {
                if !nc.is_alphanumeric() || is_cjk(nc) {
                    break;
                }
                it.next();
                end = nb + nc.len_utf8();
            }
            f(&text[start..end]);
        }
    }
}

/// 分词结果（测试与 CLI 用）。
pub fn tokenize(text: &str) -> Vec<String> {
    let folded = fold_ascii(text);
    let mut out = Vec::new();
    for_each_term(&folded, |t| out.push(t.to_string()));
    out
}

// ---------------------------------------------------------------------------
// 检索文本（去行内图片标记）
// ---------------------------------------------------------------------------

/// 单块检索文本：只取有源文本的片段，**去掉 `\u{FFFC}key\u{FFFC}` 图片标记**。
pub fn block_search_text<'a>(block: &'a PlainTextBlock) -> Cow<'a, str> {
    let text = block.text();
    if block.spans.iter().all(|s| s.src.is_some()) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    for s in &block.spans {
        if s.src.is_some() {
            out.push_str(&text[s.plain.start as usize..s.plain.end as usize]);
        }
    }
    Cow::Owned(out)
}

/// 整章检索文本：块间以换行分隔（避免跨块拼出伪词），无正文的块跳过。
///
/// 这是 BM25 索引的文本真值——与宿主读到的段落同源（`flatten_blocks_spans`），
/// 仅去掉图片标记这个非文本标记。
pub fn search_text(blocks: &[PlainTextBlock]) -> String {
    let mut out = String::new();
    for b in blocks {
        let t = block_search_text(b);
        if t.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&t);
    }
    out
}

// ---------------------------------------------------------------------------
// 搜索
// ---------------------------------------------------------------------------

/// 检索选项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchOptions {
    /// 区分大小写（默认否；仅 ASCII 折叠）。
    pub case_sensitive: bool,
    /// 命中片段左右各取多少**字节**上下文（近似字符数；UTF-8 边界自动对齐）。
    pub context_chars: u32,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self { case_sensitive: false, context_chars: 30 }
    }
}

/// 一条命中。
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    /// 所在章。
    pub chapter: ChapterId,
    /// 章正文（`ChapterContent.text`）中的 UTF-8 字节偏移（与锚点/内链同坐标系）。
    pub offset: u32,
    /// 命中长度（字节）。
    pub len: u32,
    /// 上下文片段（段落文本，超长时两端带 `…`）。
    pub snippet: String,
    /// 片段在所属段落文本中的起点（字节），供宿主做局部高亮。
    pub snippet_offset: u32,
}

/// 一次拉取的返回。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SearchPage {
    pub hits: Vec<SearchHit>,
    /// 已扫完的章数（含跳过的图片章）。
    pub scanned: u32,
    /// 全书章数。
    pub total: u32,
    /// 是否已扫完全书（此批之后无更多命中）。
    pub done: bool,
}

/// 当前正在扫的章（只为它持有扁平块，内存 O(1 章)）。
struct CurChapter {
    id: ChapterId,
    blocks: Vec<PlainTextBlock>,
    /// 当前块下标。
    block: usize,
    /// 当前块内已扫到的字节位置。
    offset: usize,
}

/// 检索会话：句柄级、拉模式分块、可取消。
pub struct SearchSession {
    book: Arc<dyn BookSource>,
    chapters: Vec<ChapterEntry>,
    /// 已折叠的查询串（大小写不敏感时）。
    query: String,
    opts: SearchOptions,
    cancel: Arc<AtomicBool>,
    /// 下一个要加载的章下标（`chapters` 内）。
    next_chapter: usize,
    cur: Option<CurChapter>,
    scanned: u32,
    done: bool,
}

impl SearchSession {
    /// 建立检索会话（不前扫，真正扫描发生在 `next`）。
    pub fn open(
        book: Arc<dyn BookSource>,
        query: &str,
        opts: SearchOptions,
    ) -> RrkResult<SearchSession> {
        let chapters = book.chapters()?;
        Self::with_chapters(book, chapters, query, opts)
    }

    /// 轻量单章路径（issue 无号，第三方接入面）：只扫指定章、**不注册会话**，
    /// 供 `search_chapter` 同步用完即弃——扫描/坐标/snippet 逻辑与全书会话同源。
    pub fn open_chapter(
        book: Arc<dyn BookSource>,
        chapter: rrk_model::ChapterId,
        query: &str,
        opts: SearchOptions,
    ) -> RrkResult<SearchSession> {
        let mut chapters = book.chapters()?;
        chapters.retain(|e| e.id == chapter);
        if chapters.is_empty() {
            return Err(RrkError::invalid(format!("章 id 越界: {chapter}")));
        }
        Self::with_chapters(book, chapters, query, opts)
    }

    fn with_chapters(
        book: Arc<dyn BookSource>,
        chapters: Vec<ChapterEntry>,
        query: &str,
        opts: SearchOptions,
    ) -> RrkResult<SearchSession> {
        let query = query.trim();
        if query.is_empty() {
            return Err(RrkError::invalid("检索词为空"));
        }
        let folded = if opts.case_sensitive {
            query.to_string()
        } else {
            fold_ascii(query).into_owned()
        };
        Ok(SearchSession {
            book,
            chapters,
            query: folded,
            opts,
            cancel: Arc::new(AtomicBool::new(false)),
            next_chapter: 0,
            cur: None,
            scanned: 0,
            done: false,
        })
    }

    /// 取消标志句柄（跨线程置位，下一次检查即停）。
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel)
    }

    /// 置取消标志（幂等；`next` 之后返回 `Cancelled`）。
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// 全书章数。
    pub fn total(&self) -> u32 {
        self.chapters.len() as u32
    }

    /// 已扫章数。
    pub fn scanned(&self) -> u32 {
        self.scanned
    }

    /// 是否扫完。
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// 拉取下一批命中（**拉模式**：最多 `max_hits` 条，扫够即返回）。
    pub fn next(&mut self, max_hits: u32) -> RrkResult<SearchPage> {
        let max_hits = max_hits.max(1) as usize;
        let mut hits: Vec<SearchHit> = Vec::new();

        while hits.len() < max_hits {
            if self.cancel.load(Ordering::SeqCst) {
                return Err(RrkError::new(rrk_model::RrkErrorKind::Cancelled, "检索已取消"));
            }
            if self.done {
                break;
            }
            if self.cur.is_none() && !self.advance_chapter()? {
                self.done = true;
                break;
            }
            let cur = self.cur.as_mut().expect("已确保有当前章");
            scan_block(&self.query, &self.opts, cur, max_hits - hits.len(), &mut hits);
            // 本块扫完 → 前进；整章扫完 → 释放
            if cur.block >= cur.blocks.len() {
                self.cur = None;
                self.next_chapter += 1;
                self.scanned += 1;
            }
        }

        Ok(SearchPage {
            hits,
            scanned: self.scanned,
            total: self.total(),
            done: self.done,
        })
    }

    /// 加载下一个有文本的章；没有则返回 false。
    fn advance_chapter(&mut self) -> RrkResult<bool> {
        while self.next_chapter < self.chapters.len() {
            let entry = &self.chapters[self.next_chapter];
            let (id, kind) = (entry.id, entry.kind);
            if kind != ChapterKind::Text {
                // 图片章无正文可搜，直接跳过（仍计入进度）
                self.next_chapter += 1;
                self.scanned += 1;
                continue;
            }
            let content = self.book.load_chapter(id)?;
            let blocks = flatten_blocks_spans(&content.text, &content.blocks);
            let has_text = blocks.iter().any(|b| !b.text().is_empty());
            if !has_text {
                self.next_chapter += 1;
                self.scanned += 1;
                continue;
            }
            self.cur = Some(CurChapter {
                id,
                blocks,
                block: 0,
                offset: 0,
            });
            return Ok(true);
        }
        Ok(false)
    }
}

/// 大小写不敏感查找（与 `fold_ascii` 同口径：仅 ASCII 大小写折叠，非 ASCII 字节精确比）。
/// UTF-8 安全：ASCII 字节不会出现在多字节序列内，命中位置必是字符边界。
fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    let hb = hay.as_bytes();
    let nb = needle.as_bytes();
    if nb.is_empty() {
        return Some(0);
    }
    if nb.len() > hb.len() {
        return None;
    }
    'outer: for i in 0..=(hb.len() - nb.len()) {
        for j in 0..nb.len() {
            if !hb[i + j].eq_ignore_ascii_case(&nb[j]) {
                continue 'outer;
            }
        }
        return Some(i);
    }
    None
}

/// 扫当前块，产出至多 `max` 条命中（块扫完则推进到下一块）。
///
/// **免折叠副本**：旧实现每块先把整块文本小写拷贝一份（`folded`）再扫——
/// 中文书几乎无 ASCII 大写仍是白拷一遍；现直接在块原文上扫，
/// 大小写不敏感走 `find_ci`（与折叠同口径，偏移与 plain 1:1）。
fn scan_block(
    query: &str,
    opts: &SearchOptions,
    cur: &mut CurChapter,
    max: usize,
    out: &mut Vec<SearchHit>,
) {
    let start_len = out.len();
    while out.len() - start_len < max && cur.block < cur.blocks.len() {
        let plain = cur.blocks[cur.block].text();
        if plain.is_empty() {
            next_block(cur);
            continue;
        }
        let from = cur.offset.min(plain.len());
        let rel = if opts.case_sensitive {
            plain[from..].find(query)
        } else {
            find_ci(&plain[from..], query)
        };
        match rel {
            Some(rel) => {
                let abs = from + rel;
                let end = abs + query.len();
                let block = &cur.blocks[cur.block];
                if let Some(src) = block.source_anchor(abs as u32) {
                    let (snippet, snippet_offset) =
                        make_snippet(block.text(), abs, end, opts.context_chars as usize);
                    out.push(SearchHit {
                        chapter: cur.id,
                        offset: src,
                        len: query.len() as u32,
                        snippet,
                        snippet_offset,
                    });
                }
                cur.offset = end;
            }
            None => next_block(cur),
        }
    }
}

/// 一次性章内检索（会话即用即弃；issue #14 done 语义在此实现）。
///
/// FFI 可传入**锁外**取到的书源（`Arc`）：inflate/清洗/扫描不持内核锁
/// （深审 §二.4）；内核 `Kernel::search_chapter` 与本函数同源（同一实现）。
pub fn search_chapter_once(
    book: Arc<dyn BookSource>,
    chapter: ChapterId,
    query: &str,
    opts: SearchOptions,
    max_hits: u32,
) -> RrkResult<SearchPage> {
    let mut session = SearchSession::open_chapter(book, chapter, query, opts)?;
    let cap = if max_hits == 0 { 1000 } else { max_hits as usize };
    let mut out = SearchPage::default();
    // done = 扫描确实完成（issue #14）：命中被 cap 截断时保持 false，
    // 调用方据「契约字段」得知结果不完整、可增大 maxHits 重调；
    // 真正扫完才置 true（同步 API 空结果场景同样为 true）。
    let mut finished = false;
    while out.hits.len() < cap {
        let page = session.next((cap - out.hits.len()) as u32)?;
        out.hits.extend(page.hits);
        out.scanned = page.scanned;
        out.total = page.total;
        if page.done {
            finished = true;
            break;
        }
    }
    out.hits.truncate(cap);
    out.done = finished;
    Ok(out)
}

fn next_block(cur: &mut CurChapter) {
    cur.block += 1;
    cur.offset = 0;
}

/// 取片段：左右各 `ctx` 字节，两端超界补 `…`；起点对齐 UTF-8 边界。
fn make_snippet(text: &str, start: usize, end: usize, ctx: usize) -> (String, u32) {
    let mut s = start.saturating_sub(ctx);
    while s > 0 && !text.is_char_boundary(s) {
        s -= 1;
    }
    let mut e = (end + ctx).min(text.len());
    while e < text.len() && !text.is_char_boundary(e) {
        e += 1;
    }
    let mut out = String::with_capacity(e - s + 8);
    if s > 0 {
        out.push('…');
    }
    out.push_str(&text[s..e]);
    if e < text.len() {
        out.push('…');
    }
    (out, s as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cjk_bigram_and_latin_words() {
        assert_eq!(tokenize("第一章 开始"), vec!["第一", "一章", "开始"]);
        // 单字退化为 unigram
        assert_eq!(tokenize("龙 行"), vec!["龙", "行"]);
        // 西文整词 + ASCII 折叠
        assert_eq!(tokenize("Hello World-42"), vec!["hello", "world", "42"]);
        // 中英混排不粘连
        assert_eq!(tokenize("AI时代"), vec!["ai", "时代"]);
        // 标点/空白是分隔符
        assert_eq!(tokenize("，。！？ \n\t"), Vec::<String>::new());
    }

    #[test]
    fn fold_does_not_change_len() {
        assert_eq!(fold_ascii("aBc").len(), 3);
        assert_eq!(fold_ascii("aBc"), "abc");
        assert!(matches!(fold_ascii("全部中文"), Cow::Borrowed(_)));
        // 非 ASCII 大小写不折叠（避免改变字节长度）
        assert_eq!(fold_ascii("ÄÖÜ"), "ÄÖÜ");
    }

    #[test]
    fn snippet_aligns_char_boundary() {        let t = "前文命中位置后文";
        let (s, off) = make_snippet(t, 6, 12, 3);
        assert!(t.is_char_boundary(off as usize));
        assert!(s.contains("命中"));
        // 极小上下文中不越界
        let (s2, _) = make_snippet("啊", 0, 3, 0);
        assert_eq!(s2, "啊");
    }

    /// 端到端：用假书源验证拉模式分块、进度与偏移映射。
    struct FakeBook {
        meta: rrk_model::BookMeta,
        texts: Vec<&'static str>,
    }

    impl BookSource for FakeBook {
        fn meta(&self) -> &rrk_model::BookMeta {
            &self.meta
        }
        fn toc(&self) -> RrkResult<Vec<rrk_model::TocNode>> {
            Ok(Vec::new())
        }
        fn chapters(&self) -> RrkResult<Vec<ChapterEntry>> {
            Ok(self
                .texts
                .iter()
                .enumerate()
                .map(|(i, _)| ChapterEntry {
                    id: i as ChapterId,
                    title: Some(format!("第{}章", i + 1)),
                    href: format!("mobi:{i}"),
                    byte_range: (0, 0),
                    char_len: 10,
                    kind: ChapterKind::Text,
                    part: 0,
                })
                .collect())
        }
        fn load_chapter(&self, id: ChapterId) -> RrkResult<rrk_model::ChapterContent> {
            use rrk_model::{Block, Inline, StyleFlags, TextRange};
            let t: Arc<str> = Arc::from(self.texts[id as usize]);
            let len = t.len() as u32;
            Ok(rrk_model::ChapterContent {
                id,
                text: Arc::clone(&t),
                blocks: vec![Block::Paragraph {
                    inlines: vec![Inline::Text { range: TextRange::new(0, len), style: StyleFlags(0) }],
                    indent: true,
                }],
                link_ranges: Vec::new(),
            })
        }
        fn load_resource(&self, _k: &rrk_model::ResourceKey) -> RrkResult<rrk_model::Resource> {
            Err(RrkError::unsupported("fake"))
        }
        fn cover(&self) -> RrkResult<Option<rrk_model::Resource>> {
            Ok(None)
        }
        fn estimate_weights(&self) -> RrkResult<Vec<(ChapterId, u32)>> {
            Ok(Vec::new())
        }
    }

    fn fake_book() -> Arc<dyn BookSource> {
        Arc::new(FakeBook {
            meta: rrk_model::BookMeta {
                title: "假书".into(),
                authors: vec![],
                language: Some("zh".into()),
                description: None,
                format: rrk_model::FormatId::Mobi,
                content_hash: 1,
                cover: None,
            },
            texts: vec![
                "萧炎睁开双眼，发现斗气大陆上斗气为尊。",
                "少年萧炎握紧手中玄重尺，萧家的荣耀由他扛起。",
                "没有萧炎的章节，只有路人甲在说闲话。",
            ],
        })
    }

    #[test]
    fn search_pull_mode_and_offsets() {
        let book = fake_book();
        let mut s = SearchSession::open(book, "萧炎", SearchOptions::default()).unwrap();
        assert_eq!(s.total(), 3);

        // 一批一条：分块拉取，进度随批次推进
        let p1 = s.next(1).unwrap();
        assert_eq!(p1.hits.len(), 1);
        assert_eq!(p1.hits[0].chapter, 0);
        // 偏移必须落在章正文里，且指向真实命中位置
        let text0 = "萧炎睁开双眼，发现斗气大陆上斗气为尊。";
        let off = p1.hits[0].offset as usize;
        assert_eq!(&text0[off..off + 6], "萧炎");
        assert!(p1.hits[0].snippet.contains("萧炎"));

        let p2 = s.next(1).unwrap();
        assert_eq!(p2.hits[0].chapter, 1);
        let p3 = s.next(10).unwrap();
        assert_eq!(p3.hits[0].chapter, 2);
        assert_eq!(p3.scanned, 3);
        assert!(p3.done);

        // 扫完后继续拉：空批 + done
        let p4 = s.next(10).unwrap();
        assert!(p4.hits.is_empty() && p4.done);
    }

    #[test]
    fn search_case_insensitive_ascii() {
        let book: Arc<dyn BookSource> = Arc::new(FakeBook {
            meta: rrk_model::BookMeta {
                title: "t".into(),
                authors: vec![],
                language: None,
                description: None,
                format: rrk_model::FormatId::Txt,
                content_hash: 1,
                cover: None,
            },
            texts: vec!["The Hunter and the hunted."],
        });
        let mut s = SearchSession::open(book, "HUNTER", SearchOptions::default()).unwrap();
        let p = s.next(10).unwrap();
        assert_eq!(p.hits.len(), 1);
        assert_eq!(p.hits[0].offset, 4);
    }

    #[test]
    fn search_rejects_empty_query() {
        let book = fake_book();
        assert!(SearchSession::open(book, "   ", SearchOptions::default()).is_err());
    }

    #[test]
    fn cancel_stops_scan() {
        let book = fake_book();
        let mut s = SearchSession::open(book, "萧炎", SearchOptions::default()).unwrap();
        let flag = s.cancel_flag();
        flag.store(true, Ordering::SeqCst);
        let err = s.next(10).unwrap_err();
        assert_eq!(err.kind, rrk_model::RrkErrorKind::Cancelled);
    }

    /// 检索文本去掉行内图片标记：BM25 词表里不该出现资源 key。
    #[test]
    fn search_text_strips_inline_image_markers() {
        use rrk_model::{flatten_blocks_spans, Block, Inline, ResourceKey, StyleFlags, TextRange};
        let text: Arc<str> = Arc::from("图示说明"); // 图0-3 示3-6 说6-9 明9-12
        let blocks = vec![
            Block::Paragraph {
                inlines: vec![
                    Inline::Image { key: ResourceKey("pic9.png".into()) },
                    Inline::Text { range: TextRange::new(0, 6), style: StyleFlags(0) },
                ],
                indent: false,
            },
            Block::Paragraph {
                inlines: vec![Inline::Text { range: TextRange::new(6, 12), style: StyleFlags(0) }],
                indent: false,
            },
            Block::Image { key: ResourceKey("body.png".into()), alt: None, size_hint: None },
        ];
        let traced = flatten_blocks_spans(&text, &blocks);
        let st = search_text(&traced);
        assert!(!st.contains("pic9.png"), "标记应被剥离: {st:?}");
        assert_eq!(st, "图示\n说明");
        assert!(block_search_text(&traced[2]).is_empty());
    }
}
