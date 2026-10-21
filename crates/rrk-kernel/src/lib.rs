//! rrk-kernel：会话编排层（方案 §1 的 `rrk-kernel`）。
//!
//! 职责：内核生命周期、格式注册、**句柄表**（FFI 边界的书句柄托管）。
//! 任务调度（rayon 优先级池）随 P1 排版内核进入；本层先把「打开/关闭/取书源」钉牢。

use std::sync::Arc;

use rrk_epub::EpubAdapter;
use rrk_format::{BookSource, ContentSource, FormatRegistry};
use rrk_mobi::MobiAdapter;
use rrk_model::{RrkError, RrkResult};
use rrk_txt::TxtAdapter;

/// 书句柄（0 = 非法）。FFI 边界只传句柄，不传引用。
pub type BookHandle = u64;

/// 内核实例：格式注册表 + 句柄表。
pub struct Kernel {
    registry: FormatRegistry,
    books: Vec<Option<Arc<dyn BookSource>>>,
}

impl Default for Kernel {
    fn default() -> Self {
        Self::new()
    }
}

impl Kernel {
    /// 创建内核并注册全部落地格式（EPUB/TXT/MOBI/AZW3）。
    pub fn new() -> Self {
        let mut registry = FormatRegistry::new();
        registry.register(Box::new(EpubAdapter));
        registry.register(Box::new(TxtAdapter));
        registry.register(Box::new(MobiAdapter));
        Self { registry, books: Vec::new() }
    }

    /// 打开一本书（路径），返回句柄。
    pub fn open_path(&mut self, path: &str) -> RrkResult<BookHandle> {
        self.open(&ContentSource::from_path(path))
    }

    /// 打开一本书（任意内容源）。
    pub fn open(&mut self, src: &ContentSource) -> RrkResult<BookHandle> {
        let book: Arc<dyn BookSource> = Arc::from(self.registry.open(src)?);
        // 复用空槽；句柄 = 槽位 + 1（0 永远非法）
        if let Some(pos) = self.books.iter().position(|s| s.is_none()) {
            self.books[pos] = Some(book);
            Ok((pos + 1) as BookHandle)
        } else {
            self.books.push(Some(book));
            Ok(self.books.len() as BookHandle)
        }
    }

    /// 关闭一本书（幂等；非法句柄报错）。
    pub fn close(&mut self, handle: BookHandle) -> RrkResult<()> {
        let idx = handle
            .checked_sub(1)
            .ok_or_else(|| RrkError::invalid("非法句柄 0"))? as usize;
        if idx >= self.books.len() || self.books[idx].is_none() {
            return Err(RrkError::invalid(format!("句柄已关闭或越界: {handle}")));
        }
        self.books[idx] = None;
        Ok(())
    }

    /// 取书源（句柄 → 共享引用）。
    pub fn book(&self, handle: BookHandle) -> RrkResult<&Arc<dyn BookSource>> {
        let idx = handle
            .checked_sub(1)
            .ok_or_else(|| RrkError::invalid("非法句柄 0"))? as usize;
        self.books
            .get(idx)
            .and_then(|s| s.as_ref())
            .ok_or_else(|| RrkError::invalid(format!("句柄已关闭或越界: {handle}")))
    }

    /// 当前打开的书数。
    pub fn open_count(&self) -> usize {
        self.books.iter().filter(|s| s.is_some()).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_lifecycle() {
        let mut k = Kernel::new();
        // 未知格式 → UnsupportedFormat；句柄表不被污染
        let bad = ContentSource::from_bytes(Some("x.bin".into()), &b"\x01\x02\x03\x04"[..]);
        let err = k.open(&bad).unwrap_err();
        assert_eq!(err.kind, rrk_model::RrkErrorKind::UnsupportedFormat);
        assert_eq!(k.open_count(), 0);
    }
}
