//! 检索基准（bench 盲区补全，CI 护栏用）：
//!
//! ```text
//! cargo run --release -p rrk-search --example bench_search [书路径]
//! ```
//!
//! 口径：默认 `../../fixtures/sample.txt`，测 ① 全书拉模式搜索（拉到扫完）
//! ② 单章搜索 ③ BM25 分步构建到完成。断言为宽松上限（正常值的 40 倍以上），
//! 只抓灾难性回归；精细调优以本地多次测量为准。

use std::sync::Arc;
use std::time::Instant;

use rrk_format::{BookSource, ContentSource, FormatRegistry};
use rrk_search::{SearchOptions, SearchSession};
use rrk_txt::TxtAdapter;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = args
        .first()
        .cloned()
        // cargo run 从仓库根执行（CI 同）；也可显式传书路径
        .unwrap_or_else(|| "fixtures/sample.txt".to_string());

    let mut reg = FormatRegistry::new();
    reg.register(Box::new(TxtAdapter));
    let t0 = Instant::now();
    let book: Arc<dyn BookSource> = Arc::from(reg.open(&ContentSource::from_path(&path)).expect("打开书"));
    println!(
        "书: {}（打开 {:.1} ms）",
        book.meta().title,
        t0.elapsed().as_secs_f64() * 1000.0
    );

    let opts = SearchOptions { case_sensitive: false, context_chars: 30 };

    // ① 全书搜索（拉到扫完）
    let mut session = SearchSession::open(Arc::clone(&book), "编码", opts).expect("开检索");
    let t1 = Instant::now();
    let mut total_hits = 0usize;
    loop {
        let page = session.next(64).expect("拉批");
        total_hits += page.hits.len();
        if page.done {
            break;
        }
    }
    let search_ms = t1.elapsed().as_secs_f64() * 1000.0;
    println!("全书搜索: {search_ms:.1} ms（{total_hits} 命中）");

    // ② 单章搜索
    let chapters = book.chapters().expect("章索引");
    let first = chapters.first().expect("有章").id;
    let t2 = Instant::now();
    let mut s2 = SearchSession::open_chapter(Arc::clone(&book), first, "编码", opts).expect("单章检索");
    let mut n2 = 0usize;
    loop {
        let page = s2.next(64).expect("拉批");
        n2 += page.hits.len();
        if page.done {
            break;
        }
    }
    println!(
        "单章搜索: {:.1} ms（{n2} 命中）",
        t2.elapsed().as_secs_f64() * 1000.0
    );

    // ③ BM25 构建（一次跑完）
    let t3 = Instant::now();
    let mut builder = rrk_search::Bm25Builder::new();
    for ch in book.chapters().expect("章索引") {
        if ch.kind != rrk_model::ChapterKind::Text {
            continue;
        }
        let content = book.load_chapter(ch.id).expect("载章");
        let blocks = rrk_model::flatten_blocks_spans(&content.text, &content.blocks);
        let text = rrk_search::search_text(&blocks);
        if !text.is_empty() {
            builder.add_doc(ch.id, &text);
        }
    }
    let index = builder.build();
    let bytes = index.encode();
    let build_ms = t3.elapsed().as_secs_f64() * 1000.0;
    println!("BM25 构建: {build_ms:.1} ms（{} 词项 / {} KB）", index.term_count(), bytes.len() / 1024);

    // 宽松护栏（正常值 ~几十 ms；40 倍以上余量）
    let budget_ms = 4000.0;
    assert!(search_ms < budget_ms, "全书搜索超预算：{search_ms:.1} ms");
    assert!(build_ms < budget_ms, "BM25 构建超预算：{build_ms:.1} ms");
    println!("护栏通过（预算 {budget_ms:.0} ms）");
}
