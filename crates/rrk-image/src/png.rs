//! PNG：头部探测 + BGRA→PNG 编码（自研，含 CRC32）。

use std::io::Write;

use flate2::write::ZlibEncoder;
use flate2::Compression;
use rrk_model::{RrkError, RrkResult};

use crate::{nonzero, ImageFormat, ImageSize};

/// PNG 8 字节签名。
pub const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// 单幅位图像素数上限（1<<26 = 6710 万像素 ≈ 268MB RGBA），防误传巨大参数。
const MAX_PIXELS: u64 = 1 << 26;

/// 只读 IHDR 取尺寸。
pub(crate) fn probe(data: &[u8]) -> Option<ImageSize> {
    if data.len() < 24 || !data.starts_with(&PNG_SIGNATURE) {
        return None;
    }
    // 第一块必须是 IHDR：length(4) 'IHDR'(4) width(4) height(4)
    if &data[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
    let h = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
    nonzero(w, h, ImageFormat::Png)
}

// ---------------------------------------------------------------------------
// CRC32（PNG 用的 IEEE 802.3 反射多项式 0xEDB88320）
// ---------------------------------------------------------------------------

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static CRC_TABLE: [u32; 256] = crc_table();

/// 一次性 CRC32（PNG chunk 校验；对外的 `png_crc32` 便于宿主/测试对拍）。
pub fn png_crc32(bytes: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in bytes {
        c = CRC_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

/// 增量 CRC32（chunk 类型 + 数据分两段喂，避免拼接临时缓冲）。
struct Crc32(u32);

impl Crc32 {
    fn new() -> Self {
        Crc32(0xFFFF_FFFF)
    }

    fn update(&mut self, bytes: &[u8]) {
        let mut c = self.0;
        for &b in bytes {
            c = CRC_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
        }
        self.0 = c;
    }

    fn finish(&self) -> u32 {
        self.0 ^ 0xFFFF_FFFF
    }
}

fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let mut crc = Crc32::new();
    crc.update(kind);
    crc.update(data);
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc.finish().to_be_bytes());
}

// ---------------------------------------------------------------------------
// 编码
// ---------------------------------------------------------------------------

/// BGRA 原始位图 → PNG 字节。
///
/// - `width`/`height` 为像素尺寸，`stride` 为源缓冲**每行字节数**（传 0 = 紧凑 `width*4`）；
/// - `bgra` 至少需要 `stride*(height-1) + width*4` 字节（末行不必补齐 stride）；
/// - 输出为 8 位 RGBA 标准 PNG（无色 ICC/gAMA 等辅助块）；
/// - 逐行 BGRA→RGBA 转换后直接压入 deflate，峰值额外内存 = 一行 + 压缩输出。
pub fn encode_png_bgra(width: u32, height: u32, stride: usize, bgra: &[u8]) -> RrkResult<Vec<u8>> {
    if width == 0 || height == 0 {
        return Err(RrkError::invalid(format!("PNG: 宽高不可为 0（{width}×{height}）")));
    }
    let pixels = width as u64 * height as u64;
    if pixels > MAX_PIXELS {
        return Err(RrkError::new(
            rrk_model::RrkErrorKind::ResourceLimit,
            format!("PNG: 位图过大（{width}×{height} = {pixels} px，上限 {MAX_PIXELS}）"),
        ));
    }
    let row_bytes = width as usize * 4;
    let stride = if stride == 0 { row_bytes } else { stride };
    if stride < row_bytes {
        return Err(RrkError::invalid(format!(
            "PNG: stride {stride} 小于每行所需 {row_bytes} 字节"
        )));
    }
    let need = stride * (height as usize - 1) + row_bytes;
    if bgra.len() < need {
        return Err(RrkError::invalid(format!(
            "PNG: 源缓冲不足，需要 {need} 字节，实得 {} 字节",
            bgra.len()
        )));
    }

    // IHDR：宽、高、位深 8、颜色类型 6（RGBA）、压缩 0、滤波 0、隔行 0
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);

    // IDAT：每行 1 字节滤波类型（0 = None）+ RGBA 像素
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::new(6));
    let mut row = vec![0u8; 1 + row_bytes];
    for y in 0..height as usize {
        let base = y * stride;
        let src = &bgra[base..base + row_bytes];
        row[0] = 0;
        for x in 0..width as usize {
            let i = x * 4;
            let o = 1 + i;
            row[o] = src[i + 2]; // R ← B 位偏移 2
            row[o + 1] = src[i + 1]; // G
            row[o + 2] = src[i]; // B ← R 位偏移 0
            row[o + 3] = src[i + 3]; // A
        }
        enc.write_all(&row)
            .map_err(|e| RrkError::io(format!("PNG: deflate 写入失败: {e}")))?;
    }
    let idat = enc
        .finish()
        .map_err(|e| RrkError::io(format!("PNG: deflate 收尾失败: {e}")))?;

    let mut out = Vec::with_capacity(PNG_SIGNATURE.len() + 25 + idat.len() + 12);
    out.extend_from_slice(&PNG_SIGNATURE);
    write_chunk(&mut out, b"IHDR", &ihdr);
    write_chunk(&mut out, b"IDAT", &idat);
    write_chunk(&mut out, b"IEND", &[]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一张渐变 BGRA 图（含 alpha 变化，避免纯色压缩后看不出错）。
    fn gradient(w: u32, h: u32) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                v.push((x % 256) as u8); // B
                v.push((y % 256) as u8); // G
                v.push(((x + y) % 256) as u8); // R
                v.push(if (x + y) % 3 == 0 { 128 } else { 255 }); // A
            }
        }
        v
    }

    #[test]
    fn crc32_known_vector() {
        // "123456789" 的 IEEE CRC32 = 0xCBF43926
        assert_eq!(png_crc32(b"123456789"), 0xCBF4_3926);
    }

    /// 自研编码器产出必须能被标准解码器（image crate，仅测试期依赖）读回，
    /// 且 BGRA→RGBA 通道顺序正确。
    #[test]
    fn encode_roundtrip_via_standard_decoder() {
        let (w, h) = (37u32, 23u32);
        let src = gradient(w, h);
        let png = encode_png_bgra(w, h, 0, &src).unwrap();
        assert!(png.starts_with(&PNG_SIGNATURE));

        let img = image::load_from_memory(&png).expect("标准解码器应能读回").to_rgba8();
        assert_eq!((img.width(), img.height()), (w, h));
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                let p = img.get_pixel(x, y).0;
                assert_eq!(
                    p,
                    [src[i + 2], src[i + 1], src[i], src[i + 3]],
                    "像素 ({x},{y}) 通道顺序错"
                );
            }
        }
    }

    #[test]
    fn encode_roundtrip_with_padding_stride() {
        let (w, h) = (5u32, 4u32);
        let stride = w as usize * 4 + 8; // 每行尾部 8 字节垃圾
        let mut src = Vec::new();
        for y in 0..h {
            for _ in 0..w {
                src.extend_from_slice(&[10, 20, 30 + y as u8, 255]);
            }
            src.extend_from_slice(&[0xEE; 8]); // 行尾垃圾：应被 stride 跳过
        }
        let png = encode_png_bgra(w, h, stride, &src).unwrap();
        let img = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(img.get_pixel(0, 3).0, [33, 20, 10, 255], "应忽略 stride 填充");
    }

    #[test]
    fn encode_rejects_bad_args() {
        let src = gradient(2, 2);
        assert_eq!(encode_png_bgra(0, 2, 0, &src).unwrap_err().kind, rrk_model::RrkErrorKind::InvalidParam);
        assert_eq!(encode_png_bgra(2, 0, 0, &src).unwrap_err().kind, rrk_model::RrkErrorKind::InvalidParam);
        // stride 小于每行所需
        assert!(encode_png_bgra(2, 2, 4, &src).is_err());
        // 缓冲不足（末行不完整）
        assert!(encode_png_bgra(2, 2, 0, &src[..8]).is_err());
        // 末行只需 width*4：stride 补齐版缓冲刚好够
        let mut packed = Vec::new();
        packed.extend_from_slice(&src[0..8]); // 第 0 行 8 字节
        packed.extend_from_slice(&src[8..16]); // 第 1 行
        assert!(encode_png_bgra(2, 2, 0, &packed).is_ok());
    }

    #[test]
    fn encode_pixel_limit() {
        // 20000×20000 = 4e8 px > 上限，应在分配前拒绝
        let err = encode_png_bgra(20000, 20000, 0, &[]).unwrap_err();
        assert_eq!(err.kind, rrk_model::RrkErrorKind::ResourceLimit);
    }
}
