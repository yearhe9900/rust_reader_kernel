//! Fuzz 目标：XHTML 清洗管线（tokenize + clean_xhtml，EPUB/MOBI 共用地基）。
#![no_main]

use libfuzzer_sys::fuzz_target;
use rrk_markup::{clean_xhtml, tokenize};

fuzz_target!(|data: &[u8]| {
    if data.len() > 256 * 1024 {
        return;
    }
    // 宽容输入：任意字节按 latin-1 视作字符串（tokenize 期待 &str）
    let s: String = data.iter().map(|&b| b as char).collect();
    let _ = tokenize(&s);
    let r = clean_xhtml(&s);
    // 清洗产物的自洽性：偏移不得越界（这类断言正是 fuzz 要守住的契约）
    let len = r.text.len() as u32;
    for l in &r.links {
        assert!(l.range.start <= len && l.range.end <= len, "内链区间越界");
    }
    for b in &r.blocks {
        if let rrk_model::Block::Paragraph { inlines, .. } = b {
            for i in inlines {
                if let rrk_model::Inline::Text { range, .. } = i {
                    assert!(range.end <= len, "inline 区间越界");
                }
            }
        }
    }
});
