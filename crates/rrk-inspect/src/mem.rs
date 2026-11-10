//! 内存基线工具：量化「按需读取」（批次 5）相对「整包驻留」的内存收益。
//!
//! 用法：
//! ```sh
//! cargo run --release -p rrk-inspect -- --mem <书路径>
//! ```
//!
//! 输出：开书前后的**进程常驻内存**（RSS）增量，以及书的字节大小。
//! 对比口径：
//! - 旧实现：`read_all()` 整包入堆 → RSS 增量 ≈ 书大小；
//! - 新实现：只驻留中央目录 + 按需读 → RSS 增量 ≈ 目录 + 单条目。
//!
//! RSS 取自平台接口（Windows `GetProcessMemoryInfo` / Linux `/proc/self/statm`
//! / macOS `task_info`），取不到时返回 `None`（不猜、不伪造数字）。

use std::path::Path;

/// 当前进程的常驻内存（字节）。取不到时 `None`。
pub fn current_rss() -> Option<u64> {
    #[cfg(target_os = "windows")]
    {
        // Job Object 之外的简单口径：Process Memory Info
        #[repr(C)]
        struct ProcessMemoryCounters {
            cb: u32,
            page_fault_count: u32,
            peak_working_set_size: usize,
            working_set_size: usize,
            quota_peak_paged_pool_usage: usize,
            quota_paged_pool_usage: usize,
            quota_peak_non_paged_pool_usage: usize,
            quota_non_paged_pool_usage: usize,
            pagefile_usage: usize,
            peak_pagefile_usage: usize,
        }
        #[repr(C)]
        struct IoCounters {
            read_operation_count: u64,
            write_operation_count: u64,
            other_operation_count: u64,
            read_transfer_count: u64,
            write_transfer_count: u64,
            other_transfer_count: u64,
        }
        extern "system" {
            fn GetCurrentProcess() -> isize;
            // 用 `K32GetProcessMemoryInfo`（kernel32 导出，Win7+）：
            // 非 K32 版在 psapi.lib 里，会给链接器加一个额外依赖。
            fn K32GetProcessMemoryInfo(
                process: isize,
                counters: *mut ProcessMemoryCounters,
                cb: u32,
            ) -> i32;
        }
        unsafe {
            let mut c = ProcessMemoryCounters {
                cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
                page_fault_count: 0,
                peak_working_set_size: 0,
                working_set_size: 0,
                quota_peak_paged_pool_usage: 0,
                quota_paged_pool_usage: 0,
                quota_peak_non_paged_pool_usage: 0,
                quota_non_paged_pool_usage: 0,
                pagefile_usage: 0,
                peak_pagefile_usage: 0,
            };
            let ok = K32GetProcessMemoryInfo(
                GetCurrentProcess(),
                &mut c,
                std::mem::size_of::<ProcessMemoryCounters>() as u32,
            );
            if ok == 0 {
                return None;
            }
            Some(c.working_set_size as u64)
        }
    }
    #[cfg(target_os = "linux")]
    {
        let s = std::fs::read_to_string("/proc/self/statm").ok()?;
        // statm: size resident shared text lib data dt（单位：页）
        let pages: u64 = s.split_whitespace().nth(1)?.parse().ok()?;
        Some(pages * page_size())
    }
    #[cfg(target_os = "macos")]
    {
        // macOS 用 mach task_info；本工具不做 FFI 声明（避免引入额外平台代码），
        // 退化为「取不到」——不伪造数字。
        None
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        let _ = Path::new("");
        None
    }
}

/// 页大小（字节）。
fn page_size() -> u64 {
    // Linux: 通常 4 KiB；用 sysconf 更准，但 std 不暴露——
    // 4 KiB 是所有主流 Linux 发行版的默认值。
    4096
}

/// 文件大小（字节）。
pub fn file_size(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.len())
}

/// 人可读字节数。
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}
