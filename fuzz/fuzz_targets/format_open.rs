//! Fuzz 目标：五格式适配器打开任意字节（嗅探 + 解析管线统一入口）。
#![no_main]

use libfuzzer_sys::fuzz_target;
use rrk_format::{ContentSource, FormatRegistry};

fuzz_target!(|data: &[u8]| {
    if data.len() > 512 * 1024 {
        return;
    }
    let mut reg = FormatRegistry::new();
    reg.register(Box::new(rrk_epub::EpubAdapter));
    reg.register(Box::new(rrk_txt::TxtAdapter));
    reg.register(Box::new(rrk_mobi::MobiAdapter));
    reg.register(Box::new(rrk_cbz::CbzAdapter));
    reg.register(Box::new(rrk_pdf::PdfAdapter));
    let src = ContentSource::from_bytes(Some("fuzz.bin".into()), data);
    if let Ok(book) = reg.open(&src) {
        // 打开成功：把查询面也压一遍（这些同样吃畸形元数据）
        let _ = book.meta();
        let _ = book.chapters();
        let _ = book.toc();
        let _ = book.estimate_weights();
    }
});
