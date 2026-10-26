//! rrk-cbz：CBZ 页级图库（方案 §3.7，完整实现）。
//!
//! - **页索引**：zip 中央目录一次 IO → 图片条目按**数字自然序**（`12 < 100`）排成
//!   `PageEntry[]`，只记条目与压缩大小，不解压；
//! - **页内容**：`load_page` = 单条目 inflate + 图片头解析尺寸；解码即降采样归宿主/渲染层；
//! - **双视图**：`PagedSource`（页级原生接口）+ `BookSource`（页 = 图片章，走统一管线）；
//! - **封面** = 第一页；**CBR 明确不支持**（RAR 解压成本高，方案 §3.7 口径）。

use std::sync::Arc;

use rrk_format::{BookSource, ContentSource, FormatAdapter, PagedSource};
use rrk_model::{
    fnv1a64, Block, BookMeta, ChapterContent, ChapterEntry, ChapterId, ChapterKind, FormatId,
    PageEntry, PageKind, PagedContent, Resource, ResourceKey, RrkError, RrkResult, TocNode,
};
use rrk_zip::{ZipArchive, ZipEntry};

/// 图片扩展名白名单（与宿主基线 `CbzSource` 对齐，见 issue #10）。
///
/// `png/jpg/jpeg/jpe/gif/bmp/webp/apng/tif/tiff/avif/heic`。其中 `avif`/`heic`/`tif`
/// 能进页表、能出原始字节，但 L2 光栅化仍需对应解码器（当前 `image` 依赖未启用 tiff）。
const IMAGE_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "jpe", "gif", "bmp", "webp", "apng", "tif", "tiff", "avif", "heic",
];

/// 数字自然序比较：`p12.jpg` < `p100.jpg`；同段数字按数值比。
///
/// 与宿主 `compareNatural` 同序（issue #10-4）：
/// - **大小写不敏感**（否则 `Page10.jpg` 会排到 `page2.jpg` 前）；
/// - 数值相等时**前导零少者在前**（`p7.jpg` < `p007.jpg`）。
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (ab, bb) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0usize, 0usize);
    while i < ab.len() && j < bb.len() {
        let (ca, cb) = (ab[i], bb[j]);
        if ca.is_ascii_digit() && cb.is_ascii_digit() {
            let (sa, sb) = (i, j);
            while i < ab.len() && ab[i].is_ascii_digit() {
                i += 1;
            }
            while j < bb.len() && bb[j].is_ascii_digit() {
                j += 1;
            }
            let na: u64 = a[sa..i].trim_start_matches('0').parse().unwrap_or(0);
            let nb: u64 = b[sb..j].trim_start_matches('0').parse().unwrap_or(0);
            match na.cmp(&nb) {
                Ordering::Equal => {
                    // 数值相等 → 前导零少者在前（位数少者在前）
                    match (i - sa).cmp(&(j - sb)) {
                        Ordering::Equal => {}
                        ord => return ord,
                    }
                }
                ord => return ord,
            }
        } else {
            // 大小写不敏感比较（只折叠 ASCII；非 ASCII 按原字节）
            let (la, lb) = (ca.to_ascii_lowercase(), cb.to_ascii_lowercase());
            match la.cmp(&lb) {
                Ordering::Equal => {
                    i += 1;
                    j += 1;
                }
                ord => return ord,
            }
        }
    }
    (ab.len() - i).cmp(&(bb.len() - j))
}

/// 条目名归一化：`\`→`/`、去前导 `/`、压 `//`、去 `./` 与 `.`、处理 `..`、去尾 `/`。
///
/// 空名（只有目录标记）返回 `None`。
fn normalize_entry_name(raw: &str) -> Option<String> {
    let normalized = raw.replace('\\', "/");
    let mut out: Vec<&str> = Vec::new();
    for seg in normalized.split('/') {
        match seg {
            "" | "." => continue,
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    if out.is_empty() {
        return None;
    }
    Some(out.join("/"))
}

/// 垃圾条目判定（与宿主基线一致）：
/// - 任意层级出现 `__MACOSX`（Finder 压缩的 CBZ 必带 `__MACOSX/._page1.jpg`）；
/// - basename 以 `.` 开头（`.DS_Store` / `._xxx` / 隐藏文件）。
fn is_junk_path(name: &str) -> bool {
    if name
        .split('/')
        .any(|seg| seg.eq_ignore_ascii_case("__macosx"))
    {
        return true;
    }
    let base = name.rsplit('/').next().unwrap_or("");
    base.starts_with('.')
}

/// 图片名判定（按 **basename** 的扩展名匹配白名单）。
fn is_image_name(name: &str) -> bool {
    let base = name.rsplit('/').next().unwrap_or("");
    match base.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => IMAGE_EXTS.contains(&ext.to_ascii_lowercase().as_str()),
        _ => false,
    }
}

/// 扩展名 → MIME（含 `image` 解码器未覆盖的格式：宿主拿原始字节自行解码/降级）。
fn mime_of_name(name: &str) -> &'static str {
    let base = name.rsplit('/').next().unwrap_or("");
    match base.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "png" | "apng" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "avif" => "image/avif",
        "heic" => "image/heic",
        _ => "image/jpeg",
    }
}

/// 收集页表：归一化 → 过滤（垃圾 + 白名单）→ 大小写撞名去重 → 自然序排序。
///
/// 与宿主 `CbzSource` 基线对齐（issue #10）：
/// 1. 条目名先归一化（`\`→`/`、去 `./` 与前导 `/`、压 `//`、去尾 `/`）；
/// 2. 过滤 `__MACOSX/` 前缀与 `.` 开头的隐藏文件；
/// 3. 扩展名白名单补全（12 种）；
/// 4. **小写键去重**（zip 物理序只收第一件，`Page1.jpg` 与 `page1.jpg` 不会成为两页）；
/// 5. `natural_cmp` 大小写不敏感 + 前导零少者在前（与宿主 `compareNatural` 同序）。
fn collect_pages(entries: &[ZipEntry]) -> Vec<PageRef> {
    let mut pages: Vec<PageRef> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for e in entries {
        if e.name.ends_with('/') {
            continue; // 目录条目
        }
        let Some(name) = normalize_entry_name(&e.name) else { continue };
        if is_junk_path(&name) || !is_image_name(&name) {
            continue;
        }
        // 大小写撞名去重：小写键（首件胜出，按 zip 物理序）
        if !seen.insert(name.to_ascii_lowercase()) {
            continue;
        }
        pages.push(PageRef { zip_name: e.name.clone(), name });
    }
    pages.sort_by(|a, b| natural_cmp(&a.name, &b.name));
    pages
}

/// 图片头解析尺寸（**已下沉**到 `rrk_image::dimensions`，此处仅保留 `(w, h)` 便捷壳）。
///
/// 唯一口径在内核 `rrk-image`：CBZ 页尺寸表与宿主正文插图测量共用同一份实现，
/// 避免「两处各写一份头解析、格式支持不一致」。
pub fn image_size(data: &[u8]) -> Option<(u32, u32)> {
    rrk_image::dimensions(data).map(|s| (s.width, s.height))
}

/// 页条目：`zip_name` 用于取数据，`name` 是归一化名（过滤/排序/href 口径）。
#[derive(Debug, Clone)]
struct PageRef {
    zip_name: String,
    name: String,
}

/// CBZ 书源。
pub struct CbzBook {
    meta: BookMeta,
    zip: Arc<ZipArchive>,
    /// 页序（按自然序排好）。
    pages: Vec<PageRef>,
    sizes: Vec<Option<(u32, u32)>>,
    /// 最近渲染页缓存（(页, 目标宽) → 位图；保留 2 档：翻回 + resize 后回看）。
    render_cache: std::sync::Mutex<Vec<Rendered>>,
}

/// 渲染缓存条目：(页, 目标宽, 宽, 高, RGBA)。
type Rendered = (u32, u32, u32, u32, Vec<u8>);

/// 渲染缓存容量（档位少：页位图大，只防最近动作的重复 inflate+解码）。
const RENDER_CACHE_SLOTS: usize = 2;

impl CbzBook {
    pub fn parse(src: &ContentSource) -> RrkResult<Self> {
        let bytes = src.read_all()?;
        let content_hash = fnv1a64(&bytes[..bytes.len().min(1024 * 1024)]);
        let zip = Arc::new(ZipArchive::parse(bytes)?);

        let pages = collect_pages(zip.entries());
        if pages.is_empty() {
            return Err(RrkError::corrupt("CBZ: 无图片条目"));
        }

        // 尺寸只限流读条目前缀（64KB 内不解全图；头内定不出尺寸的
        // 如超长 EXIF JPEG 才退全量解压）——开书不再逐页整图 inflate。
        const SIZE_PROBE: usize = 64 * 1024;
        let mut sizes = Vec::with_capacity(pages.len());
        for p in &pages {
            let size = zip.entry(&p.zip_name).and_then(|entry| {
                let head = zip.read_prefix(entry, SIZE_PROBE).ok()?;
                image_size(&head)
                    .or_else(|| zip.read(entry).ok().and_then(|d| image_size(&d)))
            });
            sizes.push(size);
        }

        let stem = src
            .name()
            .map(|n| n.rsplit_once('.').map(|(s, _)| s).unwrap_or(n).to_string())
            .unwrap_or_else(|| "未命名".to_string());
        let meta = BookMeta {
            title: stem,
            authors: Vec::new(),
            language: None,
            description: None,
            format: FormatId::Cbz,
            content_hash,
            cover: Some(ResourceKey("page:0".to_string())),
        };

        Ok(Self { meta, zip, pages, sizes, render_cache: std::sync::Mutex::new(Vec::new()) })
    }

    fn page_ref(&self, index: u32) -> RrkResult<&PageRef> {
        self.pages
            .get(index as usize)
            .ok_or_else(|| RrkError::invalid(format!("页越界: {index}")))
    }

    fn page_entry(&self, index: u32) -> RrkResult<&ZipEntry> {
        let p = self.page_ref(index)?;
        self.zip
            .entry(&p.zip_name)
            .ok_or_else(|| RrkError::corrupt(format!("CBZ: 条目缺失 {}", p.zip_name)))
    }

    fn load_page_resource(&self, index: u32) -> RrkResult<Resource> {
        let entry = self.page_entry(index)?;
        let data = self.zip.read(entry)?;
        let mime = mime_of_name(&self.page_ref(index)?.name);
        Ok(Resource {
            key: ResourceKey(format!("page:{index}")),
            mime: mime.to_string(),
            data,
        })
    }
}

/// CBZ 适配器。
pub struct CbzAdapter;

impl FormatAdapter for CbzAdapter {
    fn id(&self) -> FormatId {
        FormatId::Cbz
    }

    fn sniff(&self, head: &[u8], name: Option<&str>) -> u32 {
        if !head.starts_with(b"PK\x03\x04") {
            return 0;
        }
        if name.map(|n| n.to_ascii_lowercase().ends_with(".cbz")).unwrap_or(false) {
            return 100;
        }
        0
    }

    fn open(&self, src: &ContentSource) -> RrkResult<Box<dyn BookSource>> {
        Ok(Box::new(CbzBook::parse(src)?))
    }
}

impl PagedSource for CbzBook {
    fn meta(&self) -> &BookMeta {
        &self.meta
    }

    fn toc(&self) -> RrkResult<Vec<TocNode>> {
        Ok(Vec::new())
    }

    fn pages(&self) -> RrkResult<Vec<PageEntry>> {
        Ok(self
            .pages
            .iter()
            .enumerate()
            .map(|(i, _)| PageEntry {
                index: i as u32,
                size: self.sizes[i].map(|(w, h)| (w as f32, h as f32)).unwrap_or((0.0, 0.0)),
                kind: PageKind::Image,
            })
            .collect())
    }

    fn load_page(&self, index: u32) -> RrkResult<PagedContent> {
        Ok(PagedContent::Image(self.load_page_resource(index)?))
    }

    fn cover(&self) -> RrkResult<Option<Resource>> {
        Ok(Some(self.load_page_resource(0)?))
    }
}

impl BookSource for CbzBook {
    fn meta(&self) -> &BookMeta {
        &self.meta
    }

    fn as_paged(&self) -> Option<&dyn PagedSource> {
        Some(self)
    }

    fn toc(&self) -> RrkResult<Vec<TocNode>> {
        Ok(Vec::new())
    }

    /// 页 = 图片章（`ChapterKind::ImageGallery`），走统一管线。
    fn chapters(&self) -> RrkResult<Vec<ChapterEntry>> {
        Ok(self
            .pages
            .iter()
            .enumerate()
            .map(|(i, p)| ChapterEntry {
                id: i as ChapterId,
                title: Some(format!("第 {} 页", i + 1)),
                href: p.name.clone(),
                byte_range: (i as u64, (i + 1) as u64),
                char_len: 0,
                kind: ChapterKind::ImageGallery,
                part: 0,
            })
            .collect())
    }

    fn load_chapter(&self, id: ChapterId) -> RrkResult<ChapterContent> {
        // 图片章：正文为空，唯一块指向页资源
        let _ = self.page_entry(id)?;
        Ok(ChapterContent {
            id,
            text: Arc::from(""),
            blocks: vec![Block::Image {
                key: ResourceKey(format!("page:{id}")),
                alt: None,
                size_hint: self.sizes.get(id as usize).copied().flatten(),
            }],
            link_ranges: Vec::new(),
        })
    }

    fn load_resource(&self, key: &ResourceKey) -> RrkResult<Resource> {
        let index: u32 = key
            .0
            .strip_prefix("page:")
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| RrkError::invalid(format!("CBZ 资源 key 非法: {}", key.0)))?;
        self.load_page_resource(index)
    }

    fn cover(&self) -> RrkResult<Option<Resource>> {
        Ok(Some(self.load_page_resource(0)?))
    }

    fn estimate_weights(&self) -> RrkResult<Vec<(ChapterId, u32)>> {
        Ok(self.pages.iter().enumerate().map(|(i, _)| (i as ChapterId, 0)).collect())
    }

    /// 页 = 图片：优先走渲染缓存（免重复 inflate + 解码 + 缩放），未命中再解码并入缓存。
    fn render_page(&self, index: u32, target_width_px: u32) -> RrkResult<rrk_model::RenderResult> {
        {
            let cache = self
                .render_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some((_, _, w, h, rgba)) = cache
                .iter()
                .find(|(i, tw, _, _, _)| *i == index && *tw == target_width_px)
            {
                return Ok(rrk_model::RenderResult::Bitmap {
                    width: *w,
                    height: *h,
                    rgba: rgba.clone(),
                });
            }
        }

        let res = self.load_page_resource(index)?;
        let img = image::load_from_memory(&res.data)
            .map_err(|e| RrkError::corrupt(format!("CBZ 图片解码失败: {e}")))?;
        let w = img.width();
        let h = img.height();
        let target_w = target_width_px.max(1);
        let target_h = ((h as u64 * target_w as u64) / w.max(1) as u64).max(1) as u32;
        let resized = if target_w != w {
            img.resize_exact(target_w, target_h, image::imageops::FilterType::Triangle)
        } else {
            img
        };
        let rgba = resized.to_rgba8().into_raw();
        {
            let mut cache = self
                .render_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if cache.len() >= RENDER_CACHE_SLOTS {
                cache.remove(0);
            }
            cache.push((index, target_width_px, target_w, target_h, rgba.clone()));
        }
        Ok(rrk_model::RenderResult::Bitmap { width: target_w, height: target_h, rgba })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个最小 **stored**（不压缩）zip：本地头 + 数据 + 中央目录 + EOCD。
    /// 只为页表口径用例服务，不引 zip 编码依赖。
    fn build_cbz(files: &[&str]) -> Vec<u8> {
        const SIG_LOCAL: u32 = 0x0403_4b50;
        const SIG_CENTRAL: u32 = 0x0201_4b50;
        const SIG_EOCD: u32 = 0x0605_4b50;
        let mut out = Vec::new();
        let mut central = Vec::new();
        let mut count = 0u16;
        for name in files {
            // 用 24B 的 PNG 头当"图片"，让尺寸解析也能跑通
            let mut data = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
            data.extend_from_slice(&[0, 0, 0, 13]);
            data.extend_from_slice(b"IHDR");
            data.extend_from_slice(&8u32.to_be_bytes());
            data.extend_from_slice(&8u32.to_be_bytes());
            data.extend_from_slice(&[8, 6, 0, 0, 0]);
            // 追加名字的长度字节，保证不同条目大小不同（CRC 不校验）
            data.extend_from_slice(name.as_bytes());

            let offset = out.len() as u32;
            let crc = 0u32; // rrk-zip 不校验 CRC
            let len = data.len() as u32;
            let nlen = name.len() as u16;
            out.extend_from_slice(&SIG_LOCAL.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes()); // version
            out.extend_from_slice(&0u16.to_le_bytes()); // flags
            out.extend_from_slice(&0u16.to_le_bytes()); // stored
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&nlen.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // extra
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&data);

            central.extend_from_slice(&SIG_CENTRAL.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&len.to_le_bytes());
            central.extend_from_slice(&len.to_le_bytes());
            central.extend_from_slice(&nlen.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes()); // extra
            central.extend_from_slice(&0u16.to_le_bytes()); // comment
            central.extend_from_slice(&0u16.to_le_bytes()); // disk
            central.extend_from_slice(&0u16.to_le_bytes()); // internal
            central.extend_from_slice(&0u32.to_le_bytes()); // external
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
            count += 1;
        }
        let cd_off = out.len() as u32;
        out.extend_from_slice(&central);
        let cd_size = out.len() as u32 - cd_off;
        out.extend_from_slice(&SIG_EOCD.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_off.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    /// 从 zip 字节打开 CBZ 书源。
    fn open_bytes(bytes: Vec<u8>) -> CbzBook {
        let src = ContentSource::from_bytes(Some("t.cbz".to_string()), bytes);
        CbzBook::parse(&src).expect("CBZ 解析")
    }

    fn page_names_of(book: &CbzBook) -> Vec<String> {
        book.pages.iter().map(|p| p.name.clone()).collect()
    }

    #[test]
    fn natural_order() {
        let mut names = vec!["p100.jpg", "p12.jpg", "p2.jpg", "p1.jpg"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(names, vec!["p1.jpg", "p2.jpg", "p12.jpg", "p100.jpg"]);
    }

    #[test]
    fn natural_order_case_insensitive_and_leading_zeros() {
        // #10-4：大小写不敏感 + 前导零少者在前（对齐宿主 compareNatural）
        //
        // 关键对：`Page10.jpg` vs `page2.jpg`——按**字节**比时 'P'(0x50) < 'p'(0x70)，
        // Page10 会错排到 page2 前；折叠大小写后按数值 10 > 2，page2 应在前（宿主口径）。
        // 关键对：`p7.jpg` vs `p007.jpg`——数值同为 7，前导零少者在前。
        let mut names = vec!["Page10.jpg", "page2.jpg", "p7.jpg", "p007.jpg", "P3.jpg"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            names,
            vec!["P3.jpg", "p7.jpg", "p007.jpg", "page2.jpg", "Page10.jpg"]
        );
    }

    #[test]
    fn normalize_name_rules() {
        assert_eq!(normalize_entry_name(r".\pages\page1.jpg"), Some("pages/page1.jpg".to_string()));
        assert_eq!(normalize_entry_name("./pages/page1.jpg"), Some("pages/page1.jpg".to_string()));
        assert_eq!(normalize_entry_name("pages//page1.jpg"), Some("pages/page1.jpg".to_string()));
        assert_eq!(normalize_entry_name("pages/page1.jpg"), Some("pages/page1.jpg".to_string()));
        assert_eq!(normalize_entry_name("/pages/page1.jpg"), Some("pages/page1.jpg".to_string()));
        assert_eq!(normalize_entry_name("pages/page1.jpg/"), Some("pages/page1.jpg".to_string()));
        assert_eq!(normalize_entry_name("./"), None);
        assert_eq!(normalize_entry_name("/"), None);
    }

    #[test]
    fn image_ext_whitelist() {
        // #10-1：白名单补全（12 种）
        for ext in ["png", "jpg", "jpeg", "jpe", "gif", "bmp", "webp", "apng", "tif", "tiff", "avif", "heic"] {
            assert!(is_image_name(&format!("a.{ext}")), "{ext} 应在白名单");
            assert!(is_image_name(&format!("pages/a.{ext}")), "带目录的 {ext} 应在白名单");
        }
        for ext in ["txt", "xml", "pdf", "docx", ""] {
            assert!(!is_image_name(&format!("a.{ext}")), "{ext} 不应在白名单");
        }
        assert!(!is_image_name("pages"), "无扩展名不在白名单");
        assert!(!is_image_name("pages/page."), "空扩展名不在白名单");
    }

    #[test]
    fn junk_paths_filtered() {
        // #10-2：__MACOSX 前缀与隐藏文件过滤
        //
        // issue 口径是「`__macosx/` 前缀 + **basename** 以 `.` 开头」，故隐藏**目录**里的
        // 普通图片（`.well-known/image.jpg`）不在过滤范围——按字面实现，避免与宿主基线漂移。
        for p in [
            "__MACOSX/._page1.jpg",
            "__macosx/sub/page.jpg", // 任意层级
            ".DS_Store",
            "._page1.jpg",
            "pages/.hidden.png",
        ] {
            assert!(is_junk_path(p), "{p} 应被过滤");
        }
        for p in [
            "pages/page1.jpg",
            "page2.png",
            "a/b/c.jpg",
            ".page1.jpg.bak/x.png", // 目录段以 `.` 开头但 basename 不是 → 保留
            "pages/.well-known/image.jpg", // 隐藏目录里的普通图片 → 保留
        ] {
            assert!(!is_junk_path(p), "{p} 不应被过滤");
        }
    }

    #[test]
    fn page_table_aligns_with_host_baseline() {
        // #10 端到端：白名单 + 垃圾过滤 + 归一化 + 去重 + 排序
        let bytes = build_cbz(&[
            "cover.png",           // 应保留
            "__MACOSX/._cover.png", // 垃圾 → 滤
            ".DS_Store",           // 垃圾 → 滤
            "notes.txt",           // 非白名单 → 滤
            "./pages//page1.png",  // 归一化 → pages/page1.png
            "pages/page2.jpg",     // 应保留
            "page1.png",           // 与归一化后 pages/page1.png 不同名，应保留
            "pages/PAGE1.PNG",     // 与 pages/page1.png 撞名 → 滤（首件胜出）
            "p7.png",              // 自然序用例
            "p10.png",             // 自然序用例
            "p007.png",            // 与 p7.png 数值相同 → 前导零少者在前
        ]);
        let book = open_bytes(bytes);
        let names = page_names_of(&book);
        assert_eq!(
            names,
            vec![
                "cover.png",
                "p7.png",
                "p007.png",
                "p10.png",
                "page1.png",
                "pages/page1.png",
                "pages/page2.jpg",
            ],
            "页序/过滤口径不符"
        );
        // 尺寸应从 PNG 头读出（8×8）
        assert_eq!(book.sizes[0], Some((8, 8)));
        // href 用归一化名
        let chapters = book.chapters().unwrap();
        assert_eq!(chapters[5].href, "pages/page1.png");
        // 资源仍按 zip 原名取回（zip_name ≠ name 的场景）
        let res = book.load_resource(&ResourceKey("page:5".to_string())).unwrap();
        // fixture 里每条数据 = 29B PNG 头 + 原始条目名字节（见 build_cbz）
        assert_eq!(res.data.len(), 29 + "./pages//page1.png".len());
    }

    #[test]
    fn png_and_gif_size() {
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend_from_slice(&[0, 0, 0, 13]);
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&640u32.to_be_bytes());
        png.extend_from_slice(&480u32.to_be_bytes());
        png.extend_from_slice(&[0; 8]);
        assert_eq!(image_size(&png), Some((640, 480)));

        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&10u16.to_le_bytes());
        gif.extend_from_slice(&20u16.to_le_bytes());
        gif.extend_from_slice(&[0; 4]);
        assert_eq!(image_size(&gif), Some((10, 20)));
    }
}
