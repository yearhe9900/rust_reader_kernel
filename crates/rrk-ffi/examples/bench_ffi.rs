//! FFI 出口基准（bench 盲区补全，CI 护栏用）：
//!
//! ```text
//! cargo run --release -p rrk-ffi --example bench_ffi
//! ```
//!
//! 口径：经 C ABI 公开符号实测两条最热出口——
//! ① `rrk_open` + `rrk_load_chapter`（含内容级缓存命中/未命中两轮 + 部件流
//!    blocks JSON 体量）② `rrk_render_page`（PDF 光栅化）。
//! 断言为宽松上限，只抓灾难性回归。

use std::time::Instant;

/// 构造调用：把字符串转成 FFI 需要的 (ptr, len)。
fn with_str<T>(s: &str, f: impl FnOnce(*const u8, usize) -> T) -> T {
    f(s.as_ptr(), s.len())
}

fn main() {
    let dir = std::path::Path::new("target/bench-ffi-cache");
    let _ = std::fs::create_dir_all(dir);
    let dir_s = dir.to_string_lossy();

    unsafe {
        // ABI 校验
        let abi = rrk_ffi::rrk_abi_version();
        println!("ABI: {abi}");

        // 内容级缓存（测两轮：首轮 miss、次轮 hit）
        with_str(&dir_s, |p, l| {
            assert_eq!(rrk_ffi::rrk_content_cache_configure(p, l, 16 << 20), 0);
        });

        // ① TXT：open + load_chapter ×2（第二轮命中内容级缓存）
        let h = with_str("fixtures/sample.txt", |p, l| rrk_ffi::rrk_open(p, l));
        assert!(h != 0, "打开 sample.txt 失败");
        for round in 1..=2 {
            let (mut tp, mut tn, mut bp, mut bn, mut lp, mut ln) =
                (std::ptr::null_mut(), 0usize, std::ptr::null_mut(), 0usize, std::ptr::null_mut(), 0usize);
            let t = Instant::now();
            let rc = rrk_ffi::rrk_load_chapter(h, 0, &mut tp, &mut tn, &mut bp, &mut bn, &mut lp, &mut ln);
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            assert_eq!(rc, 0, "load_chapter 失败: {rc}");
            println!(
                "load_chapter 第{round}轮: {ms:.2} ms（正文 {tn} B + blocks {bn} B + links {ln} B）"
            );
            rrk_ffi::rrk_buf_free(bp, bn);
            rrk_ffi::rrk_buf_free(lp, ln);
            rrk_ffi::rrk_buf_free(tp, tn);
        }
        // 宽松护栏（正常值 ~0.1ms 级；预算 1000ms 只抓灾难性回归）
        let t = Instant::now();
        let mut tp = std::ptr::null_mut();
        let mut tn = 0usize;
        let mut bp = std::ptr::null_mut();
        let mut bn = 0usize;
        let mut lp = std::ptr::null_mut();
        let mut ln = 0usize;
        assert_eq!(rrk_ffi::rrk_load_chapter(h, 0, &mut tp, &mut tn, &mut bp, &mut bn, &mut lp, &mut ln), 0);
        let cached_ms = t.elapsed().as_secs_f64() * 1000.0;
        rrk_ffi::rrk_buf_free(bp, bn);
        rrk_ffi::rrk_buf_free(lp, ln);
        rrk_ffi::rrk_buf_free(tp, tn);
        assert!(cached_ms < 1000.0, "缓存命中超预算：{cached_ms:.1} ms");
        rrk_ffi::rrk_close(h);

        // ② PDF 光栅化（模式 B）
        let h2 = with_str("fixtures/sample.pdf", |p, l| rrk_ffi::rrk_open(p, l));
        if h2 != 0 {
            let (mut op, mut on, mut w, mut hh) =
                (std::ptr::null_mut(), 0usize, 0u32, 0u32);
            let t2 = Instant::now();
            let rc = rrk_ffi::rrk_render_page(h2, 0, 256, &mut op, &mut on, &mut w, &mut hh);
            let ms2 = t2.elapsed().as_secs_f64() * 1000.0;
            if rc == 0 {
                println!("render_page(256px): {ms2:.1} ms（{w}x{hh}，{on} B）");
                assert!(ms2 < 5000.0, "光栅化超预算：{ms2:.1} ms");
                rrk_ffi::rrk_buf_free(op, on);
            } else {
                println!("render_page 不适用（rc={rc}，跳过）");
            }
            rrk_ffi::rrk_close(h2);
        }

        // 统计出口冒烟
        let (mut sp, mut sn) = (std::ptr::null_mut(), 0usize);
        assert_eq!(rrk_ffi::rrk_stats(&mut sp, &mut sn), 0);
        let stats = String::from_utf8_lossy(std::slice::from_raw_parts(sp, sn)).into_owned();
        rrk_ffi::rrk_buf_free(sp, sn);
        println!("stats: {stats}");
    }
    let _ = std::fs::remove_dir_all(dir);
    println!("护栏通过");
}
