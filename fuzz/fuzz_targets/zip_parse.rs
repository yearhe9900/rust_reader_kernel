//! Fuzz 目标：zip 中央目录解析 + 单条目读取（自研 zip 是全部格式的地基）。
#![no_main]

use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use rrk_zip::ZipArchive;

fuzz_target!(|data: &[u8]| {
    if data.len() > 256 * 1024 {
        return; // 有界：不让输入本身把内存吃爆
    }
    if let Ok(z) = ZipArchive::parse(Arc::from(data.to_vec().into_boxed_slice())) {
        // 解析成功（极罕见）：把每条目都读一遍，压 inflate 路径
        for e in z.entries() {
            let _ = z.read(e);
            let _ = z.read_string(e);
        }
        let _ = z.entry_by_suffix(".opf");
    }
});
