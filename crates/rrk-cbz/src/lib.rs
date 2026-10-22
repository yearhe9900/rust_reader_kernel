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

const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "gif", "webp", "bmp", "avif"];

/// 数字自然序比较：`p12.jpg` < `p100.jpg`；同段数字按数值比，其余按字节比。
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
                Ordering::Equal => {}
                ord => return ord,
            }
        } else {
            match ca.cmp(&cb) {
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

/// 图片头解析尺寸（PNG/JPEG/GIF/BMP/WebP 尽力而为；失败返回 None）。
pub fn image_size(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() >= 24 && data.starts_with(&[0x89, b'P', b'N', b'G']) {
        // IHDR: 16..20 宽，20..24 高（大端）
        return Some((
            u32::from_be_bytes([data[16], data[17], data[18], data[19]]),
            u32::from_be_bytes([data[20], data[21], data[22], data[23]]),
        ));
    }
    if data.len() >= 4 && data.starts_with(&[0xFF, 0xD8]) {
        // JPEG：扫 SOF 标记
        let mut i = 2usize;
        while i + 9 < data.len() {
            if data[i] != 0xFF {
                i += 1;
                continue;
            }
            let marker = data[i + 1];
            let is_sof = matches!(
                marker,
                0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF
            );
            if is_sof {
                let h = u16::from_be_bytes([data[i + 5], data[i + 6]]) as u32;
                let w = u16::from_be_bytes([data[i + 7], data[i + 8]]) as u32;
                return Some((w, h));
            }
            if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
                i += 2;
            } else {
                let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
                i += 2 + len.max(2);
            }
        }
        return None;
    }
    if data.len() >= 10 && data.starts_with(b"GIF8") {
        return Some((
            u16::from_le_bytes([data[6], data[7]]) as u32,
            u16::from_le_bytes([data[8], data[9]]) as u32,
        ));
    }
    if data.len() >= 26 && data.starts_with(b"BM") {
        return Some((
            u32::from_le_bytes([data[18], data[19], data[20], data[21]]),
            i32::from_le_bytes([data[22], data[23], data[24], data[25]]).unsigned_abs(),
        ));
    }
    if data.len() >= 30 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        let four = &data[12..16];
        if four == b"VP8X" {
            let w = 1 + (data[24] as u32 | (data[25] as u32) << 8 | (data[26] as u32) << 16);
            let h = 1 + (data[27] as u32 | (data[28] as u32) << 8 | (data[29] as u32) << 16);
            return Some((w, h));
        }
        if four == b"VP8L" && data.len() >= 25 {
            let bits = u32::from_le_bytes([data[21], data[22], data[23], data[24]]);
            let w = (bits & 0x3FFF) + 1;
            let h = ((bits >> 14) & 0x3FFF) + 1;
            return Some((w, h));
        }
    }
    None
}

fn is_image_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    IMAGE_EXTS.iter().any(|e| lower.ends_with(&format!(".{e}")))
}

/// CBZ 书源。
pub struct CbzBook {
    meta: BookMeta,
    zip: Arc<ZipArchive>,
    /// 页序（按自然序排好的 zip 条目名）。
    page_names: Vec<String>,
    sizes: Vec<Option<(u32, u32)>>,
}

impl CbzBook {
    pub fn parse(src: &ContentSource) -> RrkResult<Self> {
        let bytes = src.read_all()?;
        let content_hash = fnv1a64(&bytes[..bytes.len().min(1024 * 1024)]);
        let zip = Arc::new(ZipArchive::parse(bytes)?);

        let mut page_names: Vec<String> = zip
            .entries()
            .iter()
            .filter(|e| is_image_name(&e.name) && !e.name.ends_with('/'))
            .map(|e| e.name.clone())
            .collect();
        if page_names.is_empty() {
            return Err(RrkError::corrupt("CBZ: 无图片条目"));
        }
        page_names.sort_by(|a, b| natural_cmp(a, b));

        // 尺寸只解析头 8KB（懒解压头即可定尺寸的图；JPEG 长头兜底用完整解压）
        let mut sizes = Vec::with_capacity(page_names.len());
        for name in &page_names {
            let entry = zip.entry(name).expect("entry exists");
            let size = zip
                .read(entry)
                .ok()
                .and_then(|data| image_size(&data));
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

        Ok(Self { meta, zip, page_names, sizes })
    }

    fn page_entry(&self, index: u32) -> RrkResult<&ZipEntry> {
        let name = self
            .page_names
            .get(index as usize)
            .ok_or_else(|| RrkError::invalid(format!("页越界: {index}")))?;
        self.zip
            .entry(name)
            .ok_or_else(|| RrkError::corrupt(format!("CBZ: 条目缺失 {name}")))
    }

    fn load_page_resource(&self, index: u32) -> RrkResult<Resource> {
        let entry = self.page_entry(index)?;
        let data = self.zip.read(entry)?;
        let mime = match entry.name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()) {
            Some(ref e) if e == "png" => "image/png",
            Some(ref e) if e == "gif" => "image/gif",
            Some(ref e) if e == "webp" => "image/webp",
            Some(ref e) if e == "bmp" => "image/bmp",
            _ => "image/jpeg",
        };
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
            .page_names
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

    fn toc(&self) -> RrkResult<Vec<TocNode>> {
        Ok(Vec::new())
    }

    /// 页 = 图片章（`ChapterKind::ImageGallery`），走统一管线。
    fn chapters(&self) -> RrkResult<Vec<ChapterEntry>> {
        Ok(self
            .page_names
            .iter()
            .enumerate()
            .map(|(i, name)| ChapterEntry {
                id: i as ChapterId,
                title: Some(format!("第 {} 页", i + 1)),
                href: name.clone(),
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
        Ok(self.page_names.iter().enumerate().map(|(i, _)| (i as ChapterId, 0)).collect())
    }

    /// 页 = 图片：解码 + 缩放 → RGBA 位图（CBZ 的「原版页」即图片本体）。
    fn render_page(&self, index: u32, target_width_px: u32) -> RrkResult<rrk_model::RenderResult> {
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
        Ok(rrk_model::RenderResult::Bitmap {
            width: target_w,
            height: target_h,
            rgba,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_order() {
        let mut names = vec!["p100.jpg", "p12.jpg", "p2.jpg", "p1.jpg"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(names, vec!["p1.jpg", "p2.jpg", "p12.jpg", "p100.jpg"]);
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
