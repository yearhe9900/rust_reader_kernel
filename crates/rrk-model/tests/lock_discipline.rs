//! 深审 C1：并发纪律的机器门禁。
//!
//! 口径（评审 §五 C1）：`unwrap`/`expect`/`panic` 的**严格红线只适用于
//! `rrk-ffi` 的 C ABI 出口**（panic 不能穿边界）；其它 crate 不启用该 clippy
//! deny，但**取锁一律走 `rrk_model::lock_recover`**——锁中毒后仍可恢复，
//! 不让一次被拦截的 panic 使引擎永久瘫痪。两条都由本文件钉死。

use std::path::{Path, PathBuf};

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        // 只扫生产源码：crates/*/tests 下是测试目标（本文件也含反模式字面量）
        if p.is_dir() {
            if p.file_name() == Some(std::ffi::OsStr::new("tests")) {
                continue;
            }
            collect_rs(&p, out);
        } else if p.extension() == Some(std::ffi::OsStr::new("rs")) {
            out.push(p);
        }
    }
}

#[test]
fn no_lock_unwrap_in_workspace_sources() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    collect_rs(&root.join("crates"), &mut files);
    assert!(files.len() >= 20, "源码扫描数量异常（解析失效？）: {}", files.len());

    let mut bad = Vec::new();
    for f in &files {
        let Ok(text) = std::fs::read_to_string(f) else {
            continue;
        };
        for (i, line) in text.lines().enumerate() {
            // 注释/文档里对本反模式的引用不算违规
            let code = line.split("//").next().unwrap_or("");
            if code.contains(".lock().unwrap()") {
                bad.push(format!("{}:{}: {}", f.display(), i + 1, line.trim()));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "生产代码取锁必须走 rrk_model::lock_recover：\n{}",
        bad.join("\n")
    );
}

#[test]
fn ffi_panic_red_line_scope_is_explicit() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let src = std::fs::read_to_string(root.join("crates/rrk-ffi/src/lib.rs"))
        .expect("读取 rrk-ffi 源码");
    assert!(
        src.contains("#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic))]"),
        "rrk-ffi 的 panic 红线声明被改动：CI 的 `-p rrk-ffi` deny 与源码声明必须同时存在"
    );
}
