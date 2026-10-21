//! rrk-inspect：CLI 校验器。用法：
//!
//! ```text
//! rrk-inspect <书路径> [--toc] [--chapter N] [--blocks N] [--weights]
//! ```
//!
//! 输出：元数据 / 目录 / 章索引 / 指定章的 Block 流预览，附耗时打点。

use std::process::ExitCode;
use std::time::Instant;

use rrk_format::{ContentSource, FormatAdapter, FormatRegistry};
use rrk_model::{Block, Inline};
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

    let reg = build_registry();
    let src = ContentSource::from_path(path);

    let t0 = Instant::now();
    let book = reg.open(&src).map_err(|e| e.to_string())?;
    let open_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let meta = book.meta();
    println!("== 书目 ==");
    println!("标题      : {}", meta.title);
    println!("作者      : {}", if meta.authors.is_empty() { "-".into() } else { meta.authors.join(" / ") });
    println!("语言      : {}", meta.language.as_deref().unwrap_or("-"));
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
            println!("  → [{:>3}] {}", n.chapter, n.title);
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

    Ok(())
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
