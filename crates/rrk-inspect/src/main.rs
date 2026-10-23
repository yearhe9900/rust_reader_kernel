//! rrk-inspect：CLI 校验器。用法：
//!
//! ```text
//! rrk-inspect <书路径> [--toc] [--chapter N] [--blocks N] [--weights]
//!            [--search 词] [--bm25 词] [--render N --out x.png]
//! ```
//!
//! 输出：元数据 / 目录 / 章索引 / 指定章的 Block 流预览，附耗时打点。
//! `--search` 走全书搜索（拉模式分块），`--bm25` 建索引后按 BM25 排序取前 10 章。

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use rrk_format::{BookSource, ContentSource, FormatRegistry};
use rrk_model::{Block, ChapterKind, Inline};
use rrk_search::{search_text, Bm25, Bm25Builder, SearchOptions, SearchSession};
use rrk_txt::TxtAdapter;

fn build_registry() -> FormatRegistry {
    let mut reg = FormatRegistry::new();
    reg.register(Box::new(rrk_epub::EpubAdapter));
    reg.register(Box::new(TxtAdapter));
    reg.register(Box::new(rrk_mobi::MobiAdapter));
    reg.register(Box::new(rrk_cbz::CbzAdapter));
    reg.register(Box::new(rrk_pdf::PdfAdapter));
    reg
}

fn preview_inline(text: &str, inlines: &[Inline], max: usize) -> String {
    let mut out = String::new();
    for inline in inlines {
        if out.chars().count() >= max {
            break;
        }
        match inline {
            Inline::Text { range, .. } => out.push_str(range.slice(text)),
            Inline::Image { key } => out.push_str(&format!("[img:{}]", key.0)),
        }
    }
    out.chars().take(max).collect()
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .ok_or("用法: rrk-inspect <书路径> [--toc] [--chapter N] [--blocks N] [--weights]")?;
    let show_toc = args.iter().any(|a| a == "--toc");
    let show_weights = args.iter().any(|a| a == "--weights");
    let chapter: Option<u32> = args
        .windows(2)
        .find(|w| w[0] == "--chapter")
        .and_then(|w| w[1].parse().ok());
    let blocks: usize = args
        .windows(2)
        .find(|w| w[0] == "--blocks")
        .and_then(|w| w[1].parse().ok())
        .unwrap_or(20);
    let render: Option<(u32, String)> = args
        .windows(4)
        .find(|w| w[0] == "--render" && w[2] == "--out")
        .map(|w| (w[1].parse().unwrap_or(0), w[3].clone()));
    let search: Option<String> = args
        .windows(2)
        .find(|w| w[0] == "--search")
        .map(|w| w[1].clone());
    let bm25: Option<String> = args
        .windows(2)
        .find(|w| w[0] == "--bm25")
        .map(|w| w[1].clone());

    let reg = build_registry();
    let src = ContentSource::from_path(path);

    let t0 = Instant::now();
    let book: Arc<dyn BookSource> = Arc::from(reg.open(&src).map_err(|e| e.to_string())?);
    let open_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let meta = book.meta();
    println!("== 书目 ==");
    println!("标题      : {}", meta.title);
    println!("作者      : {}", if meta.authors.is_empty() { "-".into() } else { meta.authors.join(" / ") });
    println!("语言      : {}", meta.language.as_deref().unwrap_or("-"));
    let desc = meta
        .description
        .as_deref()
        .map(|d| d.chars().take(60).collect::<String>())
        .unwrap_or_else(|| "-".to_string());
    println!("简介      : {desc}");
    println!("格式      : {}", meta.format);
    println!("内容哈希  : {:016x}", meta.content_hash);
    println!("打开耗时  : {open_ms:.1} ms");

    let t1 = Instant::now();
    let chapters = book.chapters().map_err(|e| e.to_string())?;
    let toc = book.toc().map_err(|e| e.to_string())?;
    println!("\n== 章索引 ==（{} 章，解析 {:.1} ms）", chapters.len(), t1.elapsed().as_secs_f64() * 1000.0);
    for e in chapters.iter().take(12) {
        println!(
            "  #{:<4} {:<40} 字数≈{:<8} 范围={:?} part={}",
            e.id,
            e.title.as_deref().unwrap_or("（无标题）"),
            e.char_len,
            e.byte_range,
            e.part
        );
    }
    if chapters.len() > 12 {
        println!("  … 共 {} 章", chapters.len());
    }

    if show_toc {
        println!("\n== 目录 ==（{} 项）", toc.len());
        for n in toc.iter().take(40) {
            let anchor = n.anchor.map(|a| a.to_string()).unwrap_or_else(|| "-".into());
            println!("  → [{:>3}] {} @{}", n.chapter, n.title, anchor);
        }
    }

    if show_weights {
        let w = book.estimate_weights().map_err(|e| e.to_string())?;
        let total: u64 = w.iter().map(|(_, c)| *c as u64).sum();
        println!("\n== 权重 == 总字数≈{total}");
    }

    if let Some(cid) = chapter {
        let t2 = Instant::now();
        let content = book.load_chapter(cid).map_err(|e| e.to_string())?;
        println!(
            "\n== 章 #{cid} ==（{} 字节 / {} 块，加载+清洗 {:.1} ms）",
            content.text.len(),
            content.blocks.len(),
            t2.elapsed().as_secs_f64() * 1000.0
        );
        if !content.link_ranges.is_empty() {
            let l = &content.link_ranges[0];
            println!(
                "  内链: {} 条（首条 {}..{} → {}）",
                content.link_ranges.len(),
                l.range.start,
                l.range.end,
                l.href
            );
        }
        for (i, b) in content.blocks.iter().take(blocks).enumerate() {
            match b {
                Block::Heading { level, inlines } => {
                    println!("  [{i:>3}] H{level} {}", preview_inline(&content.text, inlines, 60));
                }
                Block::Paragraph { inlines, indent } => {
                    let mark = if *indent { "¶*" } else { "¶ " };
                    println!("  [{i:>3}] {mark} {}", preview_inline(&content.text, inlines, 60));
                }
                Block::Image { key, size_hint, .. } => {
                    println!("  [{i:>3}] IMG {} {:?}", key.0, size_hint);
                }
                Block::Quote { blocks } => println!("  [{i:>3}] QUOTE ({} 块)", blocks.len()),
                Block::Divider => println!("  [{i:>3}] ---", ),
            }
        }
    }

    if let Some((page, out)) = render {
        // 统一渲染出口（PDF L2 / CBZ 图页都走 BookSource::render_page）
        let t = std::time::Instant::now();
        match book.render_page(page, 900) {
            Ok(rrk_model::RenderResult::Bitmap { width, height, rgba }) => {
                save_png(&out, width, height, &rgba);
                println!(
                    "\n== 渲染 == 第 {page} 页 → {out}（{width}x{height}，{:.1} ms）",
                    t.elapsed().as_secs_f64() * 1000.0
                );
            }
            Ok(_) => println!("\n== 渲染 == 非位图结果"),
            Err(e) => println!("\n== 渲染 == 失败: {e}"),
        }
    }

    if let Some(q) = search {
        run_search(&book, &q)?;
    }

    if let Some(q) = bm25 {
        run_bm25(&book, &chapters, &q)?;
    }

    Ok(())
}

/// 全书搜索（拉模式分块；每批 50 条）。
fn run_search(book: &Arc<dyn BookSource>, query: &str) -> Result<(), String> {
    let t = Instant::now();
    let mut session =
        SearchSession::open(Arc::clone(book), query, SearchOptions::default()).map_err(|e| e.to_string())?;
    println!("\n== 搜索 == 查询 {query:?}（全书 {} 章）", session.total());
    let mut shown = 0usize;
    let mut batches = 0usize;
    loop {
        let page = session.next(50).map_err(|e| e.to_string())?;
        batches += 1;
        for h in &page.hits {
            if shown < 10 {
                println!(
                    "  #{:<4} @{:<8} {}",
                    h.chapter,
                    h.offset,
                    h.snippet.replace('\n', "⏎")
                );
            }
            shown += 1;
        }
        if page.done {
            break;
        }
    }
    println!(
        "  命中 {shown} 条 / {batches} 批 / 扫完 {} 章 / {:.1} ms",
        session.scanned(),
        t.elapsed().as_secs_f64() * 1000.0
    );
    Ok(())
}

/// BM25：建索引 + 查询排序（并校验序列化往返）。
fn run_bm25(
    book: &Arc<dyn BookSource>,
    chapters: &[rrk_model::ChapterEntry],
    query: &str,
) -> Result<(), String> {
    let t = Instant::now();
    let mut builder = Bm25Builder::new();
    for e in chapters {
        if e.kind != ChapterKind::Text {
            continue;
        }
        let content = book.load_chapter(e.id).map_err(|e| e.to_string())?;
        let text = search_text(&rrk_model::flatten_blocks_spans(&content.text, &content.blocks));
        if !text.is_empty() {
            builder.add_doc(e.id, &text);
        }
    }
    let docs = builder.doc_count();
    let index = builder.build();
    let build_ms = t.elapsed().as_secs_f64() * 1000.0;
    let bytes = index.encode();
    let t2 = Instant::now();
    let back = Bm25::decode(&bytes).map_err(|e| e.to_string())?;
    println!(
        "\n== BM25 == {docs} 章 / {} 词项 / {} 词项总量 / 索引 {} 字节（{:.1} ms 建 + {:.1} ms 往返校验）",
        index.term_count(),
        index.total_tokens(),
        bytes.len(),
        build_ms,
        t2.elapsed().as_secs_f64() * 1000.0
    );
    let hits = back.query(query, 10);
    println!("  查询 {query:?} → {} 章有分", hits.len());
    let title_of = |id: u32| {
        chapters
            .iter()
            .find(|e| e.id == id)
            .and_then(|e| e.title.clone())
            .unwrap_or_else(|| format!("#{id}"))
    };
    for h in &hits {
        println!("  #{:<4} {:<32} score={:.4}", h.chapter, title_of(h.chapter), h.score);
    }
    Ok(())
}

/// 存 PNG（自研最小编码器走 image crate）。
fn save_png(path: &str, w: u32, h: u32, rgba: &[u8]) {
    let img = image::RgbaImage::from_raw(w, h, rgba.to_vec()).expect("RGBA 尺寸自洽");
    image::DynamicImage::ImageRgba8(img)
        .save(path)
        .expect("写 PNG 失败");
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("错误: {e}");
            ExitCode::FAILURE
        }
    }
}
