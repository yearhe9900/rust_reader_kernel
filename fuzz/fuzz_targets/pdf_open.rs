//! Fuzz 目标：PDF 适配器深层路径（对象表扫描 → 章/页视图 → 载页/栅格化尝试）。
#![no_main]

use libfuzzer_sys::fuzz_target;
use rrk_format::{ContentSource, FormatRegistry};
use rrk_pdf::PdfAdapter;

fuzz_target!(|data: &[u8]| {
    if data.len() > 512 * 1024 {
        return;
    }
    let mut reg = FormatRegistry::new();
    reg.register(Box::new(PdfAdapter));
    let src = ContentSource::from_bytes(Some("fuzz.pdf".into()), data);
    let Ok(book) = reg.open(&src) else { return };
    let _ = book.meta();
    let _ = book.toc();
    let _ = book.estimate_weights();
    if let Some(paged) = book.as_paged() {
        if let Ok(pages) = paged.pages() {
            for p in pages.iter().take(4) {
                let _ = paged.load_page(p.index);
            }
            if let Some(first) = pages.first() {
                // 低分辨率栅格化（失败也必须是 Err，不许 panic/挂死）
                let _ = book.render_page(first.index, 64);
            }
        }
    }
});
