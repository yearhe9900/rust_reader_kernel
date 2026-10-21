//! 引擎对照基准（方案 §4.2 终审）：同一章同一视口，parley vs cosmic-text。
//!
//! ```text
//! cargo run --release -p rrk-layout --example bench_compare -- <书路径>
//! ```

use std::time::Instant;

use rrk_format::{ContentSource, FormatRegistry};
use rrk_layout::Layouter;
use rrk_model::{StyleSpec, Viewport};
use rrk_txt::TxtAdapter;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = args.first() else {
        eprintln!("用法: bench_compare <书路径>");
        std::process::exit(2);
    };
    let mut reg = FormatRegistry::new();
    reg.register(Box::new(TxtAdapter));
    let book = reg.open(&ContentSource::from_path(path)).expect("打开失败");
    let chapters = book.chapters().expect("章索引");
    let biggest = chapters.iter().max_by_key(|c| c.char_len).expect("有章");
    let content = book.load_chapter(biggest.id).expect("载章");
    println!(
        "章 #{}：{} 字节 / {} 块（视口 390x842，字号 18，行高 1.6）",
        biggest.id,
        content.text.len(),
        content.blocks.len()
    );

    // 纯文本（对照口径：逐段拼接）
    let mut plain = String::new();
    for b in &content.blocks {
        if let rrk_model::Block::Paragraph { inlines, .. } = b {
            for i in inlines {
                if let rrk_model::Inline::Text { range, .. } = i {
                    plain.push_str(range.slice(&content.text));
                }
            }
            plain.push('\n');
        }
    }

    // ---- parley（rrk-layout 全链路）----
    let style = StyleSpec::default();
    let vp = Viewport { width: 390.0, height: 842.0, dpr: 3.0 };
    let mut layouter = Layouter::new();
    let t0 = Instant::now();
    let pages = layouter.layout(&content, &style, &vp);
    let parley_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let parley_lines: usize = pages.iter().map(|p| p.lines.len()).sum();

    // ---- cosmic-text ----
    let t1 = Instant::now();
    let mut fs = cosmic_text::FontSystem::new();
    let mut buffer = cosmic_text::Buffer::new(
        &mut fs,
        cosmic_text::Metrics::new(style.font_size, style.font_size * style.line_height),
    );
    buffer.set_text(&mut fs, &plain, cosmic_text::Attrs::new(), cosmic_text::Shaping::Advanced);
    buffer.set_size(&mut fs, Some(vp.width), Some(f32::MAX));
    buffer.shape_until_scroll(&mut fs, false);
    // 分页：累计行高切页（对照口径）
    let mut cosmic_pages = 1usize;
    let mut y = 0.0f32;
    let mut cosmic_lines = 0usize;
    for line in buffer.lines.iter() {
        let h = match line.layout_opt() {
            Some(ls) => ls
                .iter()
                .map(|w| w.line_height_opt.unwrap_or(style.font_size * style.line_height))
                .fold(0.0f32, f32::max),
            None => style.font_size * style.line_height,
        };
        if y + h > vp.height {
            cosmic_pages += 1;
            y = 0.0;
        }
        y += h;
        cosmic_lines += 1;
    }
    let cosmic_ms = t1.elapsed().as_secs_f64() * 1000.0;

    println!("parley    ：{parley_ms:7.1} ms / {} 页 / {parley_lines} 行", pages.len());
    println!("cosmic-txt：{cosmic_ms:7.1} ms / {cosmic_pages} 页 / {cosmic_lines} 行");
    println!(
        "结论口径：以 pagesOf ≤200ms 判据与页/行一致性裁决（页数差 ≤1% 视为等价，取快者）"
    );
}
