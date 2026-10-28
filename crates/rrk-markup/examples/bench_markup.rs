//! 清洗层基准（bench 盲区补全，CI 护栏用）：
//!
//! ```text
//! cargo run --release -p rrk-markup --example bench_markup [--size-kb N] [--iters N]
//! ```
//!
//! 口径：合成 ~1MB 真实形态 XHTML（段落/标题/粗斜体/链接/实体/图片混排），
//! 测 ① `tokenize` ② `clean_xhtml` 两条热路径。断言为**宽松上限**（CI 共享
//! runner 抖动大，阈值取正常值的 40 倍以上）——只抓灾难性回归（意外 O(n²)、
//! 无限循环式退化），精细调优仍以本地多次测量为准。

use std::time::Instant;

use rrk_markup::{clean_xhtml, tokenize};

/// 合成 XHTML：每节 = 标题 + 两段（含粗体/斜体/链接/实体/图片/换行）。
fn synth_xhtml(target_bytes: usize) -> String {
    let para = "<p>　　<strong>重点</strong>段落：The quick brown fox jumps over the lazy dog。\
实体示例 &amp; &lt;tag&gt; &#20320;&#22909;，<em>斜体</em>穿插\
<a href=\"chapter2.xhtml#sec1\">内链跳转</a>，末尾\
<img src=\"images/fig.png\" alt=\"图 1\" width=\"320\" height=\"200\"/>。</p>\n";
    let head = "<h2 id=\"sec1\">第 2 节 标题 Heading</h2>\n";
    let mut out = String::with_capacity(target_bytes + 4096);
    out.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<html><body>\n");
    while out.len() < target_bytes {
        out.push_str(head);
        out.push_str(para);
        out.push_str(para);
    }
    out.push_str("</body></html>\n");
    out
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str, default: usize| -> usize {
        args.windows(2)
            .find(|w| w[0] == name)
            .map(|w| w[1].parse().unwrap_or(default))
            .unwrap_or(default)
    };
    let size_kb = opt("--size-kb", 1024);
    let iters = opt("--iters", 5);

    let html = synth_xhtml(size_kb * 1024);
    println!(
        "bench_markup: 输入 {} 字节（目标 {size_kb} KB），各跑 {iters} 次取末次",
        html.len()
    );

    // tokenize
    let mut last_tok = 0.0f64;
    for _ in 0..iters {
        let t = Instant::now();
        let tokens = tokenize(&html);
        last_tok = t.elapsed().as_secs_f64() * 1000.0;
        assert!(!tokens.is_empty());
    }
    println!("tokenize: {last_tok:.1} ms");

    // clean_xhtml（含 tokenize）
    let mut last_clean = 0.0f64;
    let mut out_blocks = 0usize;
    for _ in 0..iters {
        let t = Instant::now();
        let r = clean_xhtml(&html);
        last_clean = t.elapsed().as_secs_f64() * 1000.0;
        out_blocks = r.blocks.len();
    }
    println!("clean_xhtml: {last_clean:.1} ms（{out_blocks} 块）");

    // 宽松护栏：正常值约几~几十 ms；40 倍以上余量，只抓灾难性回归
    let budget_ms = (size_kb as f64) * 2.0; // 1MB → 2000ms
    assert!(
        last_tok < budget_ms,
        "tokenize 超预算：{last_tok:.1} ms > {budget_ms:.0} ms（疑似 O(n²) 退化）"
    );
    assert!(
        last_clean < budget_ms,
        "clean_xhtml 超预算：{last_clean:.1} ms > {budget_ms:.0} ms（疑似 O(n²) 退化）"
    );
    println!("护栏通过（预算 {budget_ms:.0} ms）");
}
