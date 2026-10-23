//! rrk-image：纯字节级图像工具（issue #8）。
//!
//! 两个宿主侧的纯字节算法，下沉到内核统一实现：
//!
//! 1. [`dimensions`]：**只读文件头**取图片尺寸 + 格式，不分配像素缓冲、不做解码。
//!    用于正文插图的行内布局测量（CBZ 页尺寸表、EPUB 内嵌图尺寸）。畸形输入一律
//!    返回 `None`（不 panic、不越界）。
//! 2. [`encode_png_bgra`]：BGRA 原始位图 → PNG 字节（PDF/CBZ 页光栅位图的出口）。
//!    **逐行**转换 BGRA→RGBA 后直接喂 deflate，不分配整幅 RGBA 中间缓冲；
//!    1080×1440 页位图（≈6MB）峰值额外内存只有一行（≈4KB）。
//!
//! 口径约束（方案 §3.2 白名单 + issue #8）：
//! - 只产出字节、**不写盘**（落盘归宿主）；
//! - PNG 编码自研（约 120 行 + 自算 CRC32），不引 `image` 的编码器：既减重，也避免
//!   「解码器把整幅 RGBA 拷一遍」的峰值。`image` 仅作 dev-dependency 用于单测往返校验。
//!
//! 支持的格式：PNG / JPEG / GIF / BMP / WebP（与 `image` 依赖启用的解码器集合一致）。

mod png;

pub use png::{encode_png_bgra, png_crc32};

use rrk_model::{RrkError, RrkResult};

/// 已支持的图片格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageFormat {
    Png,
    Jpeg,
    Gif,
    Bmp,
    Webp,
}

impl ImageFormat {
    /// 小写格式名（FFI/JSON 出口用）。
    pub fn name(self) -> &'static str {
        match self {
            ImageFormat::Png => "png",
            ImageFormat::Jpeg => "jpeg",
            ImageFormat::Gif => "gif",
            ImageFormat::Bmp => "bmp",
            ImageFormat::Webp => "webp",
        }
    }

    /// MIME 类型。
    pub fn mime(self) -> &'static str {
        match self {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::Gif => "image/gif",
            ImageFormat::Bmp => "image/bmp",
            ImageFormat::Webp => "image/webp",
        }
    }

    /// FFI 整数编码：0=png 1=jpeg 2=gif 3=bmp 4=webp（只增不改）。
    pub fn code(self) -> u32 {
        match self {
            ImageFormat::Png => 0,
            ImageFormat::Jpeg => 1,
            ImageFormat::Gif => 2,
            ImageFormat::Bmp => 3,
            ImageFormat::Webp => 4,
        }
    }
}

/// 头部探测结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageSize {
    pub width: u32,
    pub height: u32,
    pub format: ImageFormat,
}

/// 只读文件头解析尺寸；失败/不支持返回 `None`。
///
/// 时间复杂度 O(头部段数)：JPEG 会按段链跳过 APPn/DQT/DHT 直到第一个 SOFn，
/// 其余格式都是定长读取。
pub fn dimensions(bytes: &[u8]) -> Option<ImageSize> {
    png::probe(bytes)
        .or_else(|| jpeg(bytes))
        .or_else(|| gif(bytes))
        .or_else(|| bmp(bytes))
        .or_else(|| webp(bytes))
}

fn nonzero(width: u32, height: u32, format: ImageFormat) -> Option<ImageSize> {
    if width == 0 || height == 0 {
        return None;
    }
    Some(ImageSize { width, height, format })
}

// ---------------------------------------------------------------------------
// JPEG：按段链跳过，直到 SOFn 取尺寸
// ---------------------------------------------------------------------------

fn jpeg(data: &[u8]) -> Option<ImageSize> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    let mut i = 2usize;
    while i + 4 <= data.len() {
        if data[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = data[i + 1];
        // 填充字节 / 无长度的独立标记
        if marker == 0xFF {
            i += 1;
            continue;
        }
        if marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            i += 2;
            continue;
        }
        if marker == 0xD9 {
            break; // EOI：没有 SOF，放弃
        }
        let seg_len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
        if seg_len < 2 {
            i += 2;
            continue;
        }
        let is_sof = matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF);
        if is_sof {
            // SOF：FFCn(2) len(2) precision(1) height(2) width(2)
            if i + 9 > data.len() {
                return None;
            }
            let h = u16::from_be_bytes([data[i + 5], data[i + 6]]) as u32;
            let w = u16::from_be_bytes([data[i + 7], data[i + 8]]) as u32;
            return nonzero(w, h, ImageFormat::Jpeg);
        }
        i += 2 + seg_len;
    }
    None
}

// ---------------------------------------------------------------------------
// GIF
// ---------------------------------------------------------------------------

fn gif(data: &[u8]) -> Option<ImageSize> {
    if data.len() < 10 || !data.starts_with(b"GIF8") {
        return None;
    }
    let w = u16::from_le_bytes([data[6], data[7]]) as u32;
    let h = u16::from_le_bytes([data[8], data[9]]) as u32;
    nonzero(w, h, ImageFormat::Gif)
}

// ---------------------------------------------------------------------------
// BMP（兼容 BITMAPINFOHEADER 与 OS/2 BITMAPCOREHEADER）
// ---------------------------------------------------------------------------

fn bmp(data: &[u8]) -> Option<ImageSize> {
    if data.len() < 26 || !data.starts_with(b"BM") {
        return None;
    }
    let dib = u32::from_le_bytes([data[14], data[15], data[16], data[17]]);
    if dib == 12 {
        // BITMAPCOREHEADER：u16 宽高
        let w = u16::from_le_bytes([data[18], data[19]]) as u32;
        let h = u16::from_le_bytes([data[20], data[21]]) as u32;
        return nonzero(w, h, ImageFormat::Bmp);
    }
    // BITMAPINFOHEADER 起：i32 宽高（高度为负 = 自上而下）
    let w = i32::from_le_bytes([data[18], data[19], data[20], data[21]]);
    let h = i32::from_le_bytes([data[22], data[23], data[24], data[25]]);
    nonzero(w.unsigned_abs(), h.unsigned_abs(), ImageFormat::Bmp)
}

// ---------------------------------------------------------------------------
// WebP：RIFF 容器内按 chunk 走，取 VP8X / VP8 / VP8L
// ---------------------------------------------------------------------------

fn webp(data: &[u8]) -> Option<ImageSize> {
    if data.len() < 20 || !data.starts_with(b"RIFF") || &data[8..12] != b"WEBP" {
        return None;
    }
    let mut i = 12usize;
    while i + 8 <= data.len() {
        let four = &data[i..i + 4];
        let size = u32::from_le_bytes([data[i + 4], data[i + 5], data[i + 6], data[i + 7]]) as usize;
        let body = i + 8;
        let avail = data.len().saturating_sub(body);
        let n = size.min(avail);
        match four {
            b"VP8X" if n >= 10 => {
                let w = 1 + (data[body + 4] as u32
                    | (data[body + 5] as u32) << 8
                    | (data[body + 6] as u32) << 16);
                let h = 1 + (data[body + 7] as u32
                    | (data[body + 8] as u32) << 8
                    | (data[body + 9] as u32) << 16);
                return nonzero(w, h, ImageFormat::Webp);
            }
            b"VP8 " if n >= 10 => {
                // 关键帧起始码 9D 01 2A，之后 14 位宽 / 14 位高
                if data[body + 3] == 0x9D && data[body + 4] == 0x01 && data[body + 5] == 0x2A {
                    let w = (u16::from_le_bytes([data[body + 6], data[body + 7]]) & 0x3FFF) as u32;
                    let h = (u16::from_le_bytes([data[body + 8], data[body + 9]]) & 0x3FFF) as u32;
                    return nonzero(w, h, ImageFormat::Webp);
                }
            }
            b"VP8L" if n >= 5 && data[body] == 0x2F => {
                let bits = u32::from_le_bytes([
                    data[body + 1],
                    data[body + 2],
                    data[body + 3],
                    data[body + 4],
                ]);
                let w = (bits & 0x3FFF) + 1;
                let h = ((bits >> 14) & 0x3FFF) + 1;
                return nonzero(w, h, ImageFormat::Webp);
            }
            _ => {}
        }
        // chunk 体按偶数字节对齐
        i = body + size + (size & 1);
    }
    None
}

/// 按 MIME/扩展名归一到 [`ImageFormat`]（宿主按资源名分流时用）。
pub fn format_of_name(name: &str) -> Option<ImageFormat> {
    let lower = name.rsplit('.').next()?.to_ascii_lowercase();
    match lower.as_str() {
        "png" => Some(ImageFormat::Png),
        "jpg" | "jpeg" | "jpe" | "jfif" => Some(ImageFormat::Jpeg),
        "gif" => Some(ImageFormat::Gif),
        "bmp" | "dib" => Some(ImageFormat::Bmp),
        "webp" => Some(ImageFormat::Webp),
        _ => None,
    }
}

/// 是否是可识别的位图字节（魔数判定）。
pub fn sniff(bytes: &[u8]) -> Option<ImageFormat> {
    dimensions(bytes).map(|s| s.format)
}

/// 校验 + 取出尺寸，失败给中文原因（FFI 出口用）。
pub fn dimensions_strict(bytes: &[u8]) -> RrkResult<ImageSize> {
    dimensions(bytes).ok_or_else(|| {
        RrkError::unsupported("无法识别的图片格式（支持 PNG/JPEG/GIF/BMP/WebP）")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_header(w: u32, h: u32) -> Vec<u8> {
        let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        v.extend_from_slice(&13u32.to_be_bytes());
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&[8, 6, 0, 0, 0]);
        v
    }

    #[test]
    fn png_dimensions() {
        let d = dimensions(&png_header(640, 480)).unwrap();
        assert_eq!((d.width, d.height, d.format), (640, 480, ImageFormat::Png));
        // 0 尺寸视为畸形
        assert!(dimensions(&png_header(0, 480)).is_none());
        // 截断
        assert!(dimensions(&png_header(640, 480)[..20]).is_none());
    }

    #[test]
    fn jpeg_skips_app_segments() {
        // SOI + APP0(JFIF) + APP1(EXIF 大段) + SOF0
        let mut v = vec![0xFF, 0xD8];
        v.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x10]);
        v.extend_from_slice(&[0u8; 14]);
        v.extend_from_slice(&[0xFF, 0xE1, 0x00, 0x20]);
        v.extend_from_slice(&[0u8; 30]);
        v.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        v.extend_from_slice(&720u16.to_be_bytes());
        v.extend_from_slice(&1280u16.to_be_bytes());
        v.extend_from_slice(&[0u8; 8]);
        let d = dimensions(&v).unwrap();
        assert_eq!((d.width, d.height, d.format), (1280, 720, ImageFormat::Jpeg));
    }

    #[test]
    fn jpeg_progressive_and_truncated() {
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xC2, 0x00, 0x11, 0x08];
        v.extend_from_slice(&100u16.to_be_bytes());
        v.extend_from_slice(&200u16.to_be_bytes());
        assert_eq!(dimensions(&v).unwrap().width, 200);
        // 只有 SOI：放弃
        assert!(dimensions(&[0xFF, 0xD8]).is_none());
        // 段长越界不 panic
        let bad = vec![0xFF, 0xD8, 0xFF, 0xE0, 0xFF, 0xFF, 0x00];
        assert!(dimensions(&bad).is_none());
    }

    #[test]
    fn gif_and_bmp() {
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&10u16.to_le_bytes());
        gif.extend_from_slice(&20u16.to_le_bytes());
        gif.extend_from_slice(&[0; 4]);
        assert_eq!(dimensions(&gif).unwrap(), ImageSize { width: 10, height: 20, format: ImageFormat::Gif });

        let mut bmp = b"BM".to_vec();
        bmp.extend_from_slice(&[0u8; 12]); // 文件头剩余
        bmp.extend_from_slice(&40u32.to_le_bytes()); // DIB 头大小
        bmp.extend_from_slice(&64i32.to_le_bytes());
        bmp.extend_from_slice(&(-32i32).to_le_bytes()); // 自上而下
        bmp.extend_from_slice(&[0u8; 8]);
        let d = dimensions(&bmp).unwrap();
        assert_eq!((d.width, d.height), (64, 32));
    }

    #[test]
    fn webp_variants() {
        // VP8X
        let mut vp8x = b"RIFF".to_vec();
        vp8x.extend_from_slice(&30u32.to_le_bytes());
        vp8x.extend_from_slice(b"WEBP");
        vp8x.extend_from_slice(b"VP8X");
        vp8x.extend_from_slice(&10u32.to_le_bytes());
        vp8x.extend_from_slice(&[0u8; 4]);
        vp8x.extend_from_slice(&[0x3F, 0x02, 0x00]); // 宽 576-1
        vp8x.extend_from_slice(&[0x1F, 0x01, 0x00]); // 高 288-1
        let d = dimensions(&vp8x).unwrap();
        assert_eq!((d.width, d.height, d.format), (576, 288, ImageFormat::Webp));

        // VP8L：14 位宽高（各减 1）
        let mut vp8l = b"RIFF".to_vec();
        vp8l.extend_from_slice(&21u32.to_le_bytes());
        vp8l.extend_from_slice(b"WEBP");
        vp8l.extend_from_slice(b"VP8L");
        vp8l.extend_from_slice(&5u32.to_le_bytes());
        vp8l.push(0x2F);
        let bits: u32 = (99u32 - 1) | ((49u32 - 1) << 14);
        vp8l.extend_from_slice(&bits.to_le_bytes());
        let d = dimensions(&vp8l).unwrap();
        assert_eq!((d.width, d.height), (99, 49));

        // VP8（有损关键帧）
        let mut vp8 = b"RIFF".to_vec();
        vp8.extend_from_slice(&30u32.to_le_bytes());
        vp8.extend_from_slice(b"WEBP");
        vp8.extend_from_slice(b"VP8 ");
        vp8.extend_from_slice(&10u32.to_le_bytes());
        vp8.extend_from_slice(&[0x10, 0x02, 0x00, 0x9D, 0x01, 0x2A]);
        vp8.extend_from_slice(&320u16.to_le_bytes());
        vp8.extend_from_slice(&240u16.to_le_bytes());
        let d = dimensions(&vp8).unwrap();
        assert_eq!((d.width, d.height), (320, 240));
    }

    #[test]
    fn garbage_is_none_not_panic() {
        assert!(dimensions(b"").is_none());
        assert!(dimensions(b"not an image").is_none());
        // 各格式魔数打头但被截断
        for head in [
            &b"\x89PNG"[..],
            &b"GIF8"[..],
            &b"BM"[..],
            &b"RIFF\x00\x00\x00\x00WEBP"[..],
            &b"\xFF\xD8"[..],
        ] {
            assert!(dimensions(head).is_none());
        }
        // 随机字节不 panic
        let junk: Vec<u8> = (0..4096u32).map(|i| (i.wrapping_mul(37) & 0xFF) as u8).collect();
        let _ = dimensions(&junk);
    }

    #[test]
    fn name_and_sniff() {
        assert_eq!(format_of_name("a/b/PIC.JPEG"), Some(ImageFormat::Jpeg));
        assert_eq!(format_of_name("x.avif"), None);
        assert_eq!(sniff(&png_header(1, 1)), Some(ImageFormat::Png));
        assert_eq!(ImageFormat::Webp.mime(), "image/webp");
        assert_eq!(ImageFormat::Bmp.code(), 3);
    }
}
