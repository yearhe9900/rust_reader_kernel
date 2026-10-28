//! Fuzz 目标：图片头解析与缩略图（尺寸头/盒链/IFD 全是手写边界算术）。
#![no_main]

use libfuzzer_sys::fuzz_target;
use rrk_image::{dimensions, thumbnail};

fuzz_target!(|data: &[u8]| {
    if data.len() > 256 * 1024 {
        return;
    }
    let _ = dimensions(data);
    // 缩略图：只喂「看起来像已知魔数」的输入（真解码成本高，压边界即可）
    if let Some(s) = dimensions(data) {
        if s.format.decodable() {
            let _ = thumbnail(data, 64);
        }
    }
});
