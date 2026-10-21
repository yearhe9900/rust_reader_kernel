//! 排版基准（方案 §4.2「基准先行」）：
//!
//! ```text
//! cargo run --release -p rrk-layout --example bench_layout -- <书路径> [--cache-dir DIR]
//! ```
//!
//! 口径：取全书最大章，手机视口（390×842@3x）下测
//! ① 冷排版（空缓存）② 编码落盘 ③ 缓存命中（解码免排版），对照 P1 目标
//! 「pagesOf 一章（3MB 长章）≤ 200ms」。

use std::time::Instant;

use rrk_format::{BookSource, ContentSource, FormatRegistry};
use rrk_layout::Layouter;
use rrk_model::{style_hash, viewport_hash, StyleSpec, Viewport};
use rrk_txt::TxtAdapter;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = args.first() else {
        eprintln!("用法: bench_layout <书路径> [--cache-dir DIR]");
        std::process::exit(2);
    };
    let cache_dir = args
        .windows(2)
        .find(|w| w[0] == "--cache-dir")
        .map(|w| w[1].clone());

    let mut reg = FormatRegistry::new();
    reg.register(Box::new(TxtAdapter));

    let t0 = Instant::now();
    let book = reg
        .open(&ContentSource::from_path(path))
        .expect("打开书失败");
    let open_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let chapters = book.chapters().expect("章索引");
    let biggest = chapters
        .iter()
        .max_by_key(|c| c.char_len)
        .expect("有章");
    println!("书: {}", book.meta().title);
    println!("打开: {open_ms:.1} ms；最大章 #{}（{} 字节）", biggest.id, biggest.char_len);

    let t1 = Instant::now();
    let content = book.load_chapter(biggest.id).expect("载章");
    let load_ms = t1.elapsed().as_secs_f64() * 1000.0;
    println!("载章: {:.1} ms（{} 字节 / {} 块）", load_ms, content.text.len(), content.blocks.len());

    let style = StyleSpec::default();
    let vp = Viewport { width: 390.0, height: 842.0, dpr: 3.0 };

    let mut layouter = Layouter::new();
    let t2 = Instant::now();
    let pages = layouter.layout(&content, &style, &vp);
    let layout_ms = t2.elapsed().as_secs_f64() * 1000.0;

    let total_lines: usize = pages.iter().map(|p| p.lines.len()).sum();
    let total_runs: usize = pages.iter().map(|p| p.runs.len()).sum();
    println!(
        "冷排版: {:.1} ms（{} 页 / {} 行 / {} run）",
        layout_ms,
        pages.len(),
        total_lines,
        total_runs
    );

    // 目标判定（方案 §9.2：pagesOf 3MB 长章 ≤ 200ms）
    let target = 200.0;
    println!(
        "P1 目标判定（≤ {target:.0} ms）: {}",
        if layout_ms <= target { "✅ 达标" } else { "⚠️ 超标" }
    );

    // 编码 + 落盘 + 命中
    let t3 = Instant::now();
    let payload = rrk_model::encode_pages(&pages);
    let encode_ms = t3.elapsed().as_secs_f64() * 1000.0;

    if let Some(dir) = cache_dir {
        let store = rrk_cache::DiskStore::open(&dir).expect("缓存目录");
        let key = rrk_cache::CacheKey::new(
            book.meta().content_hash,
            style_hash(&style),
            viewport_hash(&vp),
            rrk_cache::CacheKind::Layout,
        )
        .with_part(biggest.id);

        let t4 = Instant::now();
        store.save(&key, &payload).expect("落盘");
        let save_ms = t4.elapsed().as_secs_f64() * 1000.0;

        let t5 = Instant::now();
        let loaded = store.load(&key).expect("缓存命中");
        let back = rrk_model::decode_pages(&loaded).expect("解码");
        let hit_ms = t5.elapsed().as_secs_f64() * 1000.0;

        println!(
            "缓存链路: 编码 {:.1} ms（{} KB）/ 落盘 {:.1} ms / 命中+解码 {:.1} ms（{} 页一致: {}）",
            encode_ms,
            payload.len() / 1024,
            save_ms,
            hit_ms,
            back.len(),
            back.len() == pages.len()
        );
        println!(
            "免排版口径: 命中路径 ≈ {:.1} ms vs 冷排版 {:.1} ms（提速 {:.0}%）",
            hit_ms,
            layout_ms,
            (1.0 - hit_ms / layout_ms) * 100.0
        );
    } else {
        println!("缓存链路: 编码 {:.1} ms（{} KB）（--cache-dir 可测落盘/命中）", encode_ms, payload.len() / 1024);
    }
}
