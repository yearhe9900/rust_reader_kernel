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
//! 支持的格式：PNG / JPEG / GIF / BMP / WebP（与 `image` 依赖启用的解码器集合一致），
//! 头部探测另覆盖 TIFF 与 AVIF/HEIC（ISO-BMFF `ispe`，尺寸可得但内核不解像素——
//! `render_page` 明确报 UnsupportedFormat，由宿主自行解码）。

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
    /// TIFF（头解析 + `image` 解码均支持）。
    Tiff,
    /// AVIF（头解析可得尺寸；像素解码归宿主）。
    Avif,
    /// HEIC/HEIF（头解析可得尺寸；像素解码归宿主）。
    Heic,
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
            ImageFormat::Tiff => "tiff",
            ImageFormat::Avif => "avif",
            ImageFormat::Heic => "heic",
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
            ImageFormat::Tiff => "image/tiff",
            ImageFormat::Avif => "image/avif",
            ImageFormat::Heic => "image/heic",
        }
    }

    /// 内核能否解码像素（`render_page` 模式 B 的可用性）。
    /// AVIF/HEIC 只做头解析，像素解码归宿主（模式 A 取原始字节）。
    pub fn decodable(self) -> bool {
        !matches!(self, ImageFormat::Avif | ImageFormat::Heic)
    }

    /// FFI 整数编码：0=png 1=jpeg 2=gif 3=bmp 4=webp 5=tiff 6=avif 7=heic（只增不改）。
    pub fn code(self) -> u32 {
        match self {
            ImageFormat::Png => 0,
            ImageFormat::Jpeg => 1,
            ImageFormat::Gif => 2,
            ImageFormat::Bmp => 3,
            ImageFormat::Webp => 4,
            ImageFormat::Tiff => 5,
            ImageFormat::Avif => 6,
            ImageFormat::Heic => 7,
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
        .or_else(|| tiff(bytes))
        .or_else(|| heif_family(bytes))
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

// ---------------------------------------------------------------------------
// TIFF：II/MM 魔数 → IFD0 找 ImageWidth(256)/ImageLength(257)（有界扫描，不碰像素）
// ---------------------------------------------------------------------------

fn tiff(data: &[u8]) -> Option<ImageSize> {
    let (le, ifd_off) = match data.get(0..8)? {
        [b'I', b'I', 0x2A, 0x00, a, b, c, d] => {
            (true, u32::from_le_bytes([*a, *b, *c, *d]) as usize)
        }
        [b'M', b'M', 0x00, 0x2A, a, b, c, d] => {
            (false, u32::from_be_bytes([*a, *b, *c, *d]) as usize)
        }
        _ => return None, // BigTIFF(43) 等不在范围
    };
    let u16at = |p: usize| -> Option<u16> {
        let b = data.get(p..p + 2)?;
        Some(if le { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) })
    };
    let u32at = |p: usize| -> Option<u32> {
        let b = data.get(p..p + 4)?;
        Some(if le {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        } else {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
        })
    };
    let n = u16at(ifd_off)? as usize;
    if n == 0 || n > 512 {
        return None; // 防损坏数据深扫描
    }
    let mut w = 0u32;
    let mut h = 0u32;
    for k in 0..n {
        let e = ifd_off + 2 + k * 12;
        let tag = u16at(e)?;
        if tag != 256 && tag != 257 {
            continue;
        }
        let ty = u16at(e + 2)?;
        // SHORT(3)/LONG(4) 两种常见宽高类型；值内联在 value/offset 字段
        let v = match ty {
            3 => {
                if le {
                    u16::from_le_bytes([*data.get(e + 8)?, *data.get(e + 9)?]) as u32
                } else {
                    u16::from_be_bytes([*data.get(e + 8)?, *data.get(e + 9)?]) as u32
                }
            }
            4 => u32at(e + 8)?,
            _ => continue,
        };
        if tag == 256 {
            w = v;
        } else {
            h = v;
        }
    }
    nonzero(w, h, ImageFormat::Tiff)
}

// ---------------------------------------------------------------------------
// AVIF/HEIC（ISO-BMFF）：ftyp 认家族 → 顶层 meta/iprp/ipco 里取首个 ispe
// ---------------------------------------------------------------------------

fn heif_family(data: &[u8]) -> Option<ImageSize> {
    if data.len() < 16 || &data[4..8] != b"ftyp" {
        return None;
    }
    let is_avif = matches!(&data[8..12], b"avif" | b"avis");
    let is_heic = matches!(
        &data[8..12],
        b"heic" | b"heix" | b"hevc" | b"hevx" | b"mif1" | b"msf1" | b"heim" | b"heis"
    );
    if !is_avif && !is_heic {
        return None;
    }
    let format = if is_avif { ImageFormat::Avif } else { ImageFormat::Heic };
    let meta = find_box(data, 0, data.len(), b"meta")?;
    // meta 是 FullBox：跳 4 字节 version+flags 再遍子盒
    let inner = (meta.0 + 4).min(meta.1);
    let iprp = find_box(data, inner, meta.1, b"iprp")?;
    let ipco = find_box(data, iprp.0, iprp.1, b"ipco")?;
    let ispe = find_box(data, ipco.0, ipco.1, b"ispe")?;
    // ispe 也是 FullBox：body 起 4 字节 version+flags 后是 u32 宽、u32 高
    let p = ispe.0 + 4;
    let w = u32::from_be_bytes(data.get(p..p + 4)?.try_into().ok()?);
    let h = u32::from_be_bytes(data.get(p + 4..p + 8)?.try_into().ok()?);
    nonzero(w, h, format)
}

/// 在 [from, to) 区间顺序找顶层盒，返回其 body 区间（8 字节盒头之后）。
fn find_box(data: &[u8], from: usize, to: usize, fourcc: &[u8; 4]) -> Option<(usize, usize)> {
    let mut i = from;
    let mut steps = 0usize;
    while i + 8 <= to && steps < 64 {
        steps += 1;
        let size = u32::from_be_bytes(data[i..i + 4].try_into().ok()?) as usize;
        let typ = &data[i + 4..i + 8];
        let (body, next) = match size {
            0 => (i + 8, to), // size=0 = 延伸到父盒尾
            1 => {
                let big = u64::from_be_bytes(data.get(i + 8..i + 16)?.try_into().ok()?) as usize;
                (i + 16, i + big)
            }
            s if s >= 8 => (i + 8, i + s),
            _ => return None,
        };
        if next > to || body > to {
            return None;
        }
        if typ == fourcc {
            return Some((body, next));
        }
        i = next;
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
        "tif" | "tiff" => Some(ImageFormat::Tiff),
        "avif" => Some(ImageFormat::Avif),
        "heic" | "heif" => Some(ImageFormat::Heic),
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
        RrkError::unsupported("无法识别的图片格式（支持 PNG/JPEG/GIF/BMP/WebP/TIFF/AVIF/HEIC）")
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
        assert_eq!(format_of_name("x.avif"), Some(ImageFormat::Avif));
        assert_eq!(format_of_name("x.heic"), Some(ImageFormat::Heic));
        assert_eq!(format_of_name("x.tif"), Some(ImageFormat::Tiff));
        assert_eq!(sniff(&png_header(1, 1)), Some(ImageFormat::Png));
        assert_eq!(ImageFormat::Webp.mime(), "image/webp");
        assert_eq!(ImageFormat::Bmp.code(), 3);
        assert_eq!(ImageFormat::Tiff.code(), 5);
        assert!(ImageFormat::Tiff.decodable());
        assert!(!ImageFormat::Avif.decodable(), "AVIF 像素解码归宿主");
    }

    /// TIFF 头解析：II LE 与 MM BE 两种字节序，SHORT/LONG 两种宽高类型。
    #[test]
    fn tiff_header_probe() {
        fn tiff_ifd(le: bool, tag_type: u16, w: u32, h: u32) -> Vec<u8> {
            let mut v = Vec::new();
            if le {
                v.extend_from_slice(b"II*\x00");
                v.extend_from_slice(&8u32.to_le_bytes()); // IFD 在偏移 8
                v.extend_from_slice(&2u16.to_le_bytes()); // 2 个条目
                for (tag, val) in [(256u16, w), (257, h)] {
                    v.extend_from_slice(&tag.to_le_bytes());
                    v.extend_from_slice(&tag_type.to_le_bytes());
                    v.extend_from_slice(&1u32.to_le_bytes());
                    if tag_type == 3 {
                        v.extend_from_slice(&(val as u16).to_le_bytes());
                        v.extend_from_slice(&[0, 0]);
                    } else {
                        v.extend_from_slice(&val.to_le_bytes());
                    }
                }
            } else {
                v.extend_from_slice(b"MM\x00*");
                v.extend_from_slice(&8u32.to_be_bytes());
                v.extend_from_slice(&2u16.to_be_bytes());
                for (tag, val) in [(256u16, w), (257, h)] {
                    v.extend_from_slice(&tag.to_be_bytes());
                    v.extend_from_slice(&tag_type.to_be_bytes());
                    v.extend_from_slice(&1u32.to_be_bytes());
                    if tag_type == 3 {
                        v.extend_from_slice(&(val as u16).to_be_bytes());
                        v.extend_from_slice(&[0, 0]);
                    } else {
                        v.extend_from_slice(&val.to_be_bytes());
                    }
                }
            }
            v
        }
        assert_eq!(dimensions(&tiff_ifd(true, 3, 640, 480)), Some(size(640, 480, ImageFormat::Tiff)));
        assert_eq!(dimensions(&tiff_ifd(true, 4, 1024, 768)), Some(size(1024, 768, ImageFormat::Tiff)));
        assert_eq!(dimensions(&tiff_ifd(false, 4, 33, 44)), Some(size(33, 44, ImageFormat::Tiff)));
        // BigTIFF / 截断 → None
        assert!(dimensions(b"II+\x00\x08\x00\x00\x00").is_none());
        assert!(dimensions(b"II*\x00\x08").is_none());
    }

    /// AVIF/HEIC：ftyp + meta/iprp/ipco/ispe 盒链取尺寸；坏盒不 panic。
    #[test]
    fn heif_ispe_probe() {
        fn bmff(brand: &[u8; 4], w: u32, h: u32) -> Vec<u8> {
            let mut v = Vec::new();
            // ftyp：size(4) + 'ftyp' + major brand + minor + compatible
            let ftyp_body = [brand.as_slice(), &0u32.to_be_bytes(), b"mif1avif"].concat();
            v.extend_from_slice(&((ftyp_body.len() + 8) as u32).to_be_bytes());
            v.extend_from_slice(b"ftyp");
            v.extend_from_slice(&ftyp_body);
            // ispe：FullBox + 宽高
            let mut ispe = Vec::new();
            ispe.extend_from_slice(&16u32.to_be_bytes());
            ispe.extend_from_slice(b"ispe");
            ispe.extend_from_slice(&0u32.to_be_bytes());
            ispe.extend_from_slice(&w.to_be_bytes());
            ispe.extend_from_slice(&h.to_be_bytes());
            // ipco
            let mut ipco = Vec::new();
            ipco.extend_from_slice(&((ispe.len() + 8) as u32).to_be_bytes());
            ipco.extend_from_slice(b"ipco");
            ipco.extend_from_slice(&ispe);
            // iprp
            let mut iprp = Vec::new();
            iprp.extend_from_slice(&((ipco.len() + 8) as u32).to_be_bytes());
            iprp.extend_from_slice(b"iprp");
            iprp.extend_from_slice(&ipco);
            // meta（FullBox：头后 4 字节 version+flags）
            let mut meta = Vec::new();
            meta.extend_from_slice(&((iprp.len() + 12) as u32).to_be_bytes());
            meta.extend_from_slice(b"meta");
            meta.extend_from_slice(&0u32.to_be_bytes());
            meta.extend_from_slice(&iprp);
            v.extend_from_slice(&meta);
            v
        }
        assert_eq!(dimensions(&bmff(b"avif", 800, 600)), Some(size(800, 600, ImageFormat::Avif)));
        assert_eq!(dimensions(&bmff(b"heic", 320, 240)), Some(size(320, 240, ImageFormat::Heic)));
        assert_eq!(dimensions(&bmff(b"mif1", 32, 64)), Some(size(32, 64, ImageFormat::Heic)));
        // 非 heif 家族（mp42）→ None；缺 ispe → None
        assert!(dimensions(&bmff(b"mp42", 1, 1)).is_none());
        let mut broken = bmff(b"avif", 10, 10);
        broken.truncate(20);
        assert!(dimensions(&broken).is_none());
    }

    fn size(w: u32, h: u32, format: ImageFormat) -> ImageSize {
        ImageSize { width: w, height: h, format }
    }
}
