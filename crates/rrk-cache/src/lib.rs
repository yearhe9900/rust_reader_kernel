//! rrk-cache：缓存框架（方案 §7.3 三级缓存的 C1/C2 落地）。
//!
//! - **三要素 key**：`contentHash × styleHash × viewportHash` + 产物种类 + 分片号
//!   （改字号只失效布局、转屏只排可见页的精确失效口径）；
//! - **C1 内存层**：LRU + **保护窗口**（被保护的 key 永不淘汰；全保护则不淘汰）；
//! - **C2 磁盘层**：自研容器格式（magic + schema + key + 校验和），写入原子
//!   （临时文件 + rename）；**损坏隔离重算**（校验失败 = 缓存删除 + 返回 None，
//!   「损坏 ≠ 判据失效」口径同源）。
//!
//! 现阶段产物：结构缓存（章索引/目录）。**布局缓存（LayoutPage）的 payload 接入点
//! 已预留**——P1 排版内核产出 `LayoutPage` 后按 `CacheKind::Layout` 直接落盘，
//! 框架（key/淘汰/校验/失效）无需改动。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rrk_model::{fnv1a64, RrkResult};

/// 磁盘容器 magic + schema（schema 变更 = 全部失效重建，损坏≠判据失效）。
const MAGIC: &[u8; 4] = b"RRKC";
const SCHEMA: u32 = 1;

/// 产物种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CacheKind {
    /// 结构缓存（章索引/目录）。
    Structure,
    /// 布局缓存（LayoutPage，P1 接入）。
    Layout,
    /// 内容块（ContentChunk）。
    ContentChunk,
    /// 缩略图。
    Thumbnail,
    /// 目录树（**内容级**：key 只含 content_hash，见 [`CacheKey::content`]；issue #12）。
    Toc,
    /// 章文本（含块/内链；内容级）。
    ChapterText,
    /// 章大纲（`chapters()` 条目表；内容级；以 TXT 大纲为主，其它格式顺带）。
    TxtOutline,
    /// open 索引（EPUB：entries + toc_raw；ROADMAP 章级/页级缓存持久化，
    /// key 为内容级 [`CacheKey::content`]，命中免 open 期重建）。
    OpenEpub,
    /// open 索引（MOBI/AZW3：spans + entries + toc）。
    OpenMobi,
    /// open 索引（CBZ：页表 + 尺寸表）。
    OpenCbz,
    /// open 索引（PDF：页面表 + 章表 + 目录）。
    OpenPdf,
    /// open 索引（TXT：章表）。
    OpenTxt,
}

impl CacheKind {
    fn tag(self) -> u8 {
        match self {
            CacheKind::Structure => 1,
            CacheKind::Layout => 2,
            CacheKind::ContentChunk => 3,
            CacheKind::Thumbnail => 4,
            CacheKind::Toc => 5,
            CacheKind::ChapterText => 6,
            CacheKind::TxtOutline => 7,
            CacheKind::OpenEpub => 8,
            CacheKind::OpenMobi => 9,
            CacheKind::OpenCbz => 10,
            CacheKind::OpenPdf => 11,
            CacheKind::OpenTxt => 12,
        }
    }
}

/// 三要素缓存 key。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub content_hash: u64,
    pub style_hash: u64,
    pub viewport_hash: u64,
    pub kind: CacheKind,
    /// 分片号（巨章逻辑块 / 页码）。
    pub part: u32,
}

impl CacheKey {
    pub fn new(content_hash: u64, style_hash: u64, viewport_hash: u64, kind: CacheKind) -> Self {
        Self { content_hash, style_hash, viewport_hash, kind, part: 0 }
    }

    pub fn with_part(mut self, part: u32) -> Self {
        self.part = part;
        self
    }

    /// **内容级** key：只含 `content_hash` + 产物种类（style/viewport 恒 0）。
    ///
    /// 解析结果（目录树 / 章文本 / 章大纲）不随样式与视口变：内容 hash 变了
    /// 整本失效，与宿主原 Dart 缓存的「副本 mtime/size 判据」等价（同文件重开
    /// hash 不变，命中；文件变了 hash 变，key 全变，自然 miss）。分片号放
    /// `part`（章文本 = 章 id）。issue #12。
    pub fn content(content_hash: u64, kind: CacheKind) -> Self {
        Self {
            content_hash,
            style_hash: 0,
            viewport_hash: 0,
            kind,
            part: 0,
        }
    }

    /// 文件名（hex 平铺）。
    fn file_name(&self) -> String {
        format!(
            "{:016x}-{:016x}-{:016x}-{}-{:08x}.rrkc",
            self.content_hash,
            self.style_hash,
            self.viewport_hash,
            self.kind.tag(),
            self.part
        )
    }
}

// ---------------------------------------------------------------------------
// C1 内存层：LRU + 保护窗口
// ---------------------------------------------------------------------------

/// 内存缓存条目：载荷 + 最近访问序号（lazy-LRU）。
struct MemEntry {
    payload: Vec<u8>,
    /// get/put 时刷新的单调序号；淘汰时序号最小的未保护项最旧。
    stamp: u64,
}

/// 内存缓存（字节载荷；上层自行序列化）。
///
/// 命中/写入 **O(1)**：条目自带访问序号，只有淘汰时才线性找最旧。
/// （旧实现每次 `get` 都对 `order` 做 `retain` 全表扫，条目数千时命中热路径 O(n)。）
pub struct MemoryCache {
    map: HashMap<CacheKey, MemEntry>,
    protected: HashSet<CacheKey>,
    /// 载荷总字节上限。
    max_bytes: usize,
    used: usize,
    /// 单调访问时钟。
    clock: u64,
}

impl MemoryCache {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            map: HashMap::new(),
            protected: HashSet::new(),
            max_bytes,
            used: 0,
            clock: 0,
        }
    }

    /// 标记保护（窗口章/当前页永不淘汰；全保护则不淘汰——保护集纪律）。
    pub fn protect(&mut self, key: CacheKey) {
        self.protected.insert(key);
    }

    pub fn unprotect(&mut self, key: &CacheKey) {
        self.protected.remove(key);
    }

    pub fn get(&mut self, key: &CacheKey) -> Option<&[u8]> {
        let e = self.map.get_mut(key)?;
        self.clock += 1;
        e.stamp = self.clock;
        Some(e.payload.as_slice())
    }

    pub fn put(&mut self, key: CacheKey, payload: Vec<u8>) {
        self.clock += 1;
        let stamp = self.clock;
        if let Some(old) = self.map.remove(&key) {
            self.used -= old.payload.len();
        }
        self.used += payload.len();
        self.map.insert(key, MemEntry { payload, stamp });
        self.evict();
    }

    fn evict(&mut self) {
        while self.used > self.max_bytes {
            // 找最久未访问的未保护项；全保护则停止（纪律：全保护不淘汰）
            let victim = self
                .map
                .iter()
                .filter(|(k, _)| !self.protected.contains(k))
                .min_by_key(|(_, e)| e.stamp)
                .map(|(k, _)| *k);
            let Some(victim) = victim else { break };
            if let Some(v) = self.map.remove(&victim) {
                self.used -= v.payload.len();
            }
        }
    }

    pub fn used_bytes(&self) -> usize {
        self.used
    }

    /// 按内容哈希失效（换书/换版即清）。
    pub fn invalidate_content(&mut self, content_hash: u64) {
        let keys: Vec<CacheKey> = self
            .map
            .keys()
            .filter(|k| k.content_hash == content_hash)
            .copied()
            .collect();
        for k in keys {
            if let Some(v) = self.map.remove(&k) {
                self.used -= v.payload.len();
            }
            self.protected.remove(&k);
        }
    }
}

// ---------------------------------------------------------------------------
// C2 磁盘层
// ---------------------------------------------------------------------------

/// 磁盘缓存（每 key 一个文件；写入原子；损坏隔离）。
pub struct DiskStore {
    root: PathBuf,
}

impl DiskStore {
    pub fn open(root: impl Into<PathBuf>) -> RrkResult<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 落盘（容器头 + FNV 校验和；临时文件 + rename 原子写）。
    pub fn save(&self, key: &CacheKey, payload: &[u8]) -> RrkResult<()> {
        let mut buf = Vec::with_capacity(payload.len() + 53);
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&SCHEMA.to_le_bytes());
        buf.extend_from_slice(&key.content_hash.to_le_bytes());
        buf.extend_from_slice(&key.style_hash.to_le_bytes());
        buf.extend_from_slice(&key.viewport_hash.to_le_bytes());
        buf.push(key.kind.tag());
        buf.extend_from_slice(&key.part.to_le_bytes());
        buf.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        buf.extend_from_slice(payload);
        let sum = fnv1a64(payload);
        buf.extend_from_slice(&sum.to_le_bytes());

        let path = self.root.join(key.file_name());
        let tmp = self.root.join(format!("{}.tmp", key.file_name()));
        std::fs::write(&tmp, &buf)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// 读取；**校验失败 = 删除缓存文件并返回 None**（损坏隔离重算）。
    pub fn load(&self, key: &CacheKey) -> Option<Vec<u8>> {
        let path = self.root.join(key.file_name());
        let buf = std::fs::read(&path).ok()?;
        if !verify(&buf, key) {
            let _ = std::fs::remove_file(&path);
            return None;
        }
        let len = u64::from_le_bytes(buf[37..45].try_into().ok()?) as usize;
        Some(buf[45..45 + len].to_vec())
    }

    /// 删除单 key。
    pub fn remove(&self, key: &CacheKey) {
        let _ = std::fs::remove_file(self.root.join(key.file_name()));
    }

    /// 按内容哈希失效（遍历目录删同书缓存）。
    pub fn invalidate_content(&self, content_hash: u64) -> std::io::Result<usize> {
        let prefix = format!("{content_hash:016x}-");
        let mut removed = 0;
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                std::fs::remove_file(entry.path())?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

/// 容器校验：magic/schema/key 全对 + 长度自洽 + 校验和一致。
fn verify(buf: &[u8], key: &CacheKey) -> bool {
    if buf.len() < 53 || &buf[0..4] != MAGIC {
        return false;
    }
    if u32::from_le_bytes(buf[4..8].try_into().unwrap()) != SCHEMA {
        return false;
    }
    let u64le = |s: &[u8]| u64::from_le_bytes(s.try_into().unwrap());
    if u64le(&buf[8..16]) != key.content_hash
        || u64le(&buf[16..24]) != key.style_hash
        || u64le(&buf[24..32]) != key.viewport_hash
        || buf[32] != key.kind.tag()
        || u32::from_le_bytes(buf[33..37].try_into().unwrap()) != key.part
    {
        return false;
    }
    let len = u64le(&buf[37..45]) as usize;
    if buf.len() != 45 + len + 8 {
        return false;
    }
    let sum = u64le(&buf[45 + len..]);
    sum == fnv1a64(&buf[45..45 + len])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> CacheKey {
        CacheKey::new(1, 2, 3, CacheKind::Layout).with_part(7)
    }

    #[test]
    fn disk_roundtrip_and_corruption_isolation() {
        let dir = std::env::temp_dir().join(format!("rrk-cache-test-{}", std::process::id()));
        let store = DiskStore::open(&dir).unwrap();
        let k = key();
        store.save(&k, b"payload-bytes").unwrap();
        assert_eq!(store.load(&k).unwrap(), b"payload-bytes");

        // 翻转 payload 一个字节 → 校验失败 → 文件删除 + None
        let path = dir.join(k.file_name());
        let mut buf = std::fs::read(&path).unwrap();
        buf[45] ^= 0xFF;
        std::fs::write(&path, &buf).unwrap();
        assert!(store.load(&k).is_none());
        assert!(!path.exists(), "损坏缓存应被隔离删除");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lru_with_protection() {
        let mut c = MemoryCache::new(100);
        let k1 = CacheKey::new(1, 0, 0, CacheKind::Structure);
        let k2 = CacheKey::new(1, 0, 0, CacheKind::Layout);
        let k3 = CacheKey::new(1, 0, 0, CacheKind::ContentChunk);
        c.protect(k1); // 保护窗口
        c.put(k1, vec![0; 40]);
        c.put(k2, vec![0; 40]);
        c.put(k3, vec![0; 40]); // 超限 → 淘汰未保护的最久者 k2
        assert!(c.get(&k1).is_some(), "保护项不淘汰");
        assert!(c.get(&k2).is_none(), "未保护最久者被淘汰");
        assert!(c.get(&k3).is_some());
    }

    #[test]
    fn full_protection_no_evict() {
        let mut c = MemoryCache::new(10);
        let k = key();
        c.protect(k);
        c.put(k, vec![0; 100]);
        assert_eq!(c.used_bytes(), 100); // 全保护不淘汰
    }

    #[test]
    fn invalidate_by_content_hash() {
        let dir = std::env::temp_dir().join(format!("rrk-cache-inv-{}", std::process::id()));
        let store = DiskStore::open(&dir).unwrap();
        store.save(&key(), b"a").unwrap();
        store.save(&CacheKey::new(9, 0, 0, CacheKind::Structure), b"b").unwrap();
        let n = store.invalidate_content(1).unwrap();
        assert_eq!(n, 1);
        assert!(store.load(&key()).is_none());
        assert!(store.load(&CacheKey::new(9, 0, 0, CacheKind::Structure)).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
