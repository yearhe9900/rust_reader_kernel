//! rrk-format：格式适配接口与嗅探注册表（方案 §3.1）。
//!
//! 一个格式 = 一个 [`FormatAdapter`]；打开后统一为 [`BookSource`] 契约。
//! 扩展方式是**静态注册**（[`FormatRegistry::register`]），不做 dylib 动态插件。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rrk_cache::CacheKey;
use rrk_model::{
    fnv1a64_update, BookMeta, ChapterContent, ChapterEntry, ChapterId, FormatId, PageEntry,
    PagedContent, Resource, ResourceKey, RrkError, RrkResult, TocNode, FNV_INIT,
};

// ---------------------------------------------------------------------------
// 内容源
// ---------------------------------------------------------------------------

/// 打开一本书的输入：路径（大文件友好）或内存字节。
#[derive(Debug, Clone)]
pub enum ContentSource {
    Path(PathBuf),
    Bytes { name: Option<String>, data: Arc<[u8]> },
}

impl ContentSource {
    pub fn from_path(path: impl Into<PathBuf>) -> Self {
        ContentSource::Path(path.into())
    }

    pub fn from_bytes(name: Option<String>, data: impl Into<Arc<[u8]>>) -> Self {
        ContentSource::Bytes { name, data: data.into() }
    }

    /// 取用于嗅探的文件名。
    pub fn name(&self) -> Option<&str> {
        match self {
            ContentSource::Path(p) => p.file_name().and_then(|s| s.to_str()),
            ContentSource::Bytes { name, .. } => name.as_deref(),
        }
    }

    /// 读入全部字节（P0 简化实现；mmap 化在 P2 落盘档接入）。
    pub fn read_all(&self) -> RrkResult<Arc<[u8]>> {
        match self {
            ContentSource::Path(p) => {
                let bytes = std::fs::read(p).map_err(|e| {
                    RrkError::io(format!("读取文件失败: {}", p.display())).with_context(e.to_string())
                })?;
                Ok(bytes.into())
            }
            ContentSource::Bytes { data, .. } => Ok(Arc::clone(data)),
        }
    }

    /// 读取前 `n` 字节做嗅探（不足则全读）。
    pub fn read_head(&self, n: usize) -> RrkResult<Vec<u8>> {
        match self {
            ContentSource::Path(p) => {
                use std::io::Read;
                let mut f = std::fs::File::open(p)?;
                let mut buf = vec![0u8; n];
                let read = f.read(&mut buf)?;
                buf.truncate(read);
                Ok(buf)
            }
            ContentSource::Bytes { data, .. } => {
                let take = n.min(data.len());
                Ok(data[..take].to_vec())
            }
        }
    }
}

impl From<&Path> for ContentSource {
    fn from(p: &Path) -> Self {
        ContentSource::Path(p.to_path_buf())
    }
}

// ---------------------------------------------------------------------------
// 内容版本指纹
// ---------------------------------------------------------------------------

/// `BookMeta::content_hash` 的**唯一来源**（所有格式统一）。
///
/// 与旧口径（只取前 1MB）的区别：**全量参与**。`content_hash` 同时是内容级
/// 缓存与 open 索引缓存的 key；只采样文件头部时，修改书后半部分的正文不会让
/// 缓存失效，重开会命中旧章表/旧章文本（用户读到旧内容）。文件此时已由
/// `read_all` 全量驻留，一次线性 FNV 扫描不引入额外内存，且保证
/// 「任意字节变动 → 指纹变」。
///
/// 长度前缀消除「内容相同但长度不同」的平凡歧义。非密码学用途：只作缓存
/// 失效键，不作完整性/防篡改校验。EPUB/CBZ 同样走本函数而**不用** zip
/// 中央目录的 CRC 列表：脏书可能不填或未更新 CRC，会让「改正文但 CRC 不变」
/// 重新漏判——全量字节是唯一能保证「任意字节变动必失效」的口径。
pub fn content_fingerprint(data: &[u8]) -> u64 {
    let h = fnv1a64_update(FNV_INIT, &(data.len() as u64).to_le_bytes());
    fnv1a64_update(h, data)
}

#[cfg(test)]
mod fingerprint_tests {
    use super::content_fingerprint;

    #[test]
    fn tail_edit_changes_fingerprint() {
        // 旧口径只取前 1MB：这里模拟「1MB 之后改一个字节」必须改变指纹。
        let mut a = vec![b'x'; 1024 * 1024 + 64];
        let h0 = content_fingerprint(&a);
        a[1024 * 1024 + 10] = b'y';
        assert_ne!(h0, content_fingerprint(&a), "文件尾改动必须让缓存失效");
    }

    #[test]
    fn length_is_part_of_fingerprint() {
        assert_ne!(content_fingerprint(b"abc"), content_fingerprint(b"abc\0"));
        assert_eq!(content_fingerprint(b""), content_fingerprint(b""));
        assert_ne!(content_fingerprint(b"a"), content_fingerprint(b"b"));
    }
}

// ---------------------------------------------------------------------------
// open 索引缓存（缓存句柄下沉 adapter）
// ---------------------------------------------------------------------------

/// open 期索引缓存：缓存句柄下沉到 adapter 的入参契约
/// （ROADMAP「章级/页级缓存持久化扩展」）。
///
/// 适配器把重建代价高的 open 索引（entries / pages / toc_raw …）按内容 key
/// （[`CacheKey::content`]）交由此缓存持久化；重开命中即载入免重建。
/// 载荷编解码归各适配器（自带 magic + schema，损坏/升级自动丢弃重算）；
/// `save` 失败不致命（降级为无缓存解析，调用方不感知）。
pub trait IndexCache: Send + Sync {
    /// 载入 open 索引字节；未命中 / 损坏返回 `None`。
    fn load(&self, key: &CacheKey) -> Option<Vec<u8>>;

    /// 写入 open 索引字节（失败静默降级）。
    fn save(&self, key: &CacheKey, payload: &[u8]);
}

/// 空缓存（Null Object）：未配置缓存时的零成本替身，load 恒未命中、save 丢弃。
pub struct NullIndexCache;

impl IndexCache for NullIndexCache {
    fn load(&self, _key: &CacheKey) -> Option<Vec<u8>> {
        None
    }
    fn save(&self, _key: &CacheKey, _payload: &[u8]) {}
}

/// 内存索引缓存（进程内字节表 + 命中/未命中计数）。
///
/// 供嵌入方无磁盘缓存场景与测试使用；生产落盘实现见内核的磁盘版。
pub struct MemIndexCache {
    map: std::sync::Mutex<std::collections::HashMap<CacheKey, Vec<u8>>>,
    hits: std::sync::atomic::AtomicU64,
    misses: std::sync::atomic::AtomicU64,
}

impl MemIndexCache {
    pub fn new() -> Self {
        Self {
            map: std::sync::Mutex::new(std::collections::HashMap::new()),
            hits: std::sync::atomic::AtomicU64::new(0),
            misses: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// 命中次数（测试/观测用）。
    pub fn hits(&self) -> u64 {
        self.hits.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 未命中次数（测试/观测用）。
    pub fn misses(&self) -> u64 {
        self.misses.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Default for MemIndexCache {
    fn default() -> Self {
        Self::new()
    }
}

impl IndexCache for MemIndexCache {
    fn load(&self, key: &CacheKey) -> Option<Vec<u8>> {
        let got = rrk_model::lock_recover(&self.map).get(key).cloned();
        if got.is_some() {
            self.hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        } else {
            self.misses.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        got
    }
    fn save(&self, key: &CacheKey, payload: &[u8]) {
        rrk_model::lock_recover(&self.map).insert(*key, payload.to_vec());
    }
}

// ---------------------------------------------------------------------------
// 适配器与书源契约
// ---------------------------------------------------------------------------

/// 一个格式 = 一个适配器。
pub trait FormatAdapter: Send + Sync {
    fn id(&self) -> FormatId;

    /// 读头部字节 + 文件名返回置信度（0 = 不认）。
    fn sniff(&self, head: &[u8], name: Option<&str>) -> u32;

    /// 打开为统一书源。
    fn open(&self, src: &ContentSource) -> RrkResult<Box<dyn BookSource>>;

    /// 打开为统一书源（带 open 索引缓存；默认回退无缓存 [`Self::open`]）。
    ///
    /// 支持缓存的适配器覆写此方法：重建代价高的 open 索引按内容 key 持久化，
    /// 重开命中即载入免重建（open 期免重建，秒开书链路的地基）。
    fn open_cached(
        &self,
        src: &ContentSource,
        cache: &dyn IndexCache,
    ) -> RrkResult<Box<dyn BookSource>> {
        let _ = cache;
        self.open(src)
    }
}

/// 统一书源：所有格式的输出契约（吸收 foliate-js `sections.load/unload/size` 思想）。
/// 线程安全（`Send + Sync`）：书源只读，供 FFI 边界共享持有。
pub trait BookSource: Send + Sync {
    fn meta(&self) -> &BookMeta;

    /// 目录树。
    fn toc(&self) -> RrkResult<Vec<TocNode>>;

    /// 章索引：O(结构) 可得，不解压正文（「建索引不搬内容」）。
    fn chapters(&self) -> RrkResult<Vec<ChapterEntry>>;

    /// 书类型（统一句柄分流依据）。
    fn kind(&self) -> rrk_model::BookKind {
        rrk_model::BookKind::of(self.meta().format)
    }

    /// 固定版式视图（CBZ/PDF 返回 `Some(self)`）；重排格式默认 `None`。
    fn as_paged(&self) -> Option<&dyn PagedSource> {
        None
    }

    /// 加载一章（解析层出口：结构层 + 内容层）。
    fn load_chapter(&self, id: ChapterId) -> RrkResult<ChapterContent>;

    /// 释放章解压缓存（可选优化，允许空实现）。
    fn unload_chapter(&self, _id: ChapterId) {}

    /// 按 key 加载资源（图片/字体）。
    fn load_resource(&self, key: &ResourceKey) -> RrkResult<Resource>;

    /// 封面。
    fn cover(&self) -> RrkResult<Option<Resource>>;

    /// 零解压权重估算（章 id → 估算字数），供进度与预排预算。
    fn estimate_weights(&self) -> RrkResult<Vec<(ChapterId, u32)>>;

    /// 整页栅格化（模式 B；仅固定版式实现——PDF L2）。默认不支持。
    fn render_page(&self, index: u32, target_width_px: u32) -> RrkResult<rrk_model::RenderResult> {
        let _ = (index, target_width_px);
        Err(RrkError::unsupported("该格式无整页栅格化能力"))
    }
}

/// 固定版式书源（PDF/CBZ，方案 §2.4）：页级随机读，进度 = 页码。
pub trait PagedSource: Send + Sync {
    fn meta(&self) -> &BookMeta;

    /// 目录树（PDF outline；CBZ 通常为空）。
    fn toc(&self) -> RrkResult<Vec<TocNode>>;

    /// 页索引：O(结构) 可得（页数 + 尺寸表），不解压。
    fn pages(&self) -> RrkResult<Vec<PageEntry>>;

    /// 读一页（L1：图片资源或抽取文本；L2 光栅在渲染层）。
    fn load_page(&self, index: u32) -> RrkResult<PagedContent>;

    fn cover(&self) -> RrkResult<Option<Resource>>;
}

// ---------------------------------------------------------------------------
// 注册表
// ---------------------------------------------------------------------------

/// 静态注册的格式注册表：按嗅探置信度选适配器。
#[derive(Default)]
pub struct FormatRegistry {
    adapters: Vec<Box<dyn FormatAdapter>>,
}

impl FormatRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个适配器（后注册优先于低置信度，不覆盖更高置信度）。
    pub fn register(&mut self, adapter: Box<dyn FormatAdapter>) {
        self.adapters.push(adapter);
    }

    /// 嗅探出置信度最高的适配器。
    pub fn detect(&self, head: &[u8], name: Option<&str>) -> Option<&dyn FormatAdapter> {
        let mut best: Option<(u32, &dyn FormatAdapter)> = None;
        for a in &self.adapters {
            let score = a.sniff(head, name);
            if score > 0 && best.map(|(s, _)| score > s).unwrap_or(true) {
                best = Some((score, a.as_ref()));
            }
        }
        best.map(|(_, a)| a)
    }

    /// 打开一本书：嗅探 → 适配器 → 统一书源（无缓存）。
    pub fn open(&self, src: &ContentSource) -> RrkResult<Box<dyn BookSource>> {
        self.open_cached(src, &NullIndexCache)
    }

    /// 打开一本书（带 open 索引缓存）：嗅探 → 适配器 → 统一书源。
    pub fn open_cached(
        &self,
        src: &ContentSource,
        cache: &dyn IndexCache,
    ) -> RrkResult<Box<dyn BookSource>> {
        let head = src.read_head(4096)?;
        let adapter = self.detect(&head, src.name()).ok_or_else(|| {
            let name = src.name().unwrap_or("<memory>").to_string();
            RrkError::unsupported(format!("无法识别的格式: {name}"))
        })?;
        adapter.open_cached(src, cache)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeAdapter;

    impl FormatAdapter for FakeAdapter {
        fn id(&self) -> FormatId {
            FormatId::Txt
        }
        fn sniff(&self, head: &[u8], name: Option<&str>) -> u32 {
            if head.starts_with(b"FAKE") {
                100
            } else if name.map(|n| n.ends_with(".txt")).unwrap_or(false) {
                10
            } else {
                0
            }
        }
        fn open(&self, _src: &ContentSource) -> RrkResult<Box<dyn BookSource>> {
            Err(RrkError::unsupported("fake"))
        }
    }

    #[test]
    fn registry_picks_highest_confidence() {
        let mut reg = FormatRegistry::new();
        reg.register(Box::new(FakeAdapter));
        assert!(reg.detect(b"FAKE123", Some("a.bin")).is_some());
        assert!(reg.detect(b"XXXX", Some("a.txt")).is_some());
        assert!(reg.detect(b"XXXX", Some("a.bin")).is_none());
    }
}
