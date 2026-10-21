//! PalmDB / PalmDOC / MOBI 头 / EXTH 解析（自研）。
//!
//! 字段偏移以 **记录 0 起点**为基准，口径对齐 foliate-js `mobi.js` 与 MobileRead MOBI wiki：
//! PalmDOC 头 16 字节 → MOBI 头（`MOBI` 魔数 @16）→ EXTH（@mobi.length+16）。
//! 压缩字段语义（规范口径，勿再误读）：**1 = 无压缩，2 = PalmDOC，17480 = HUFF/CDIC**。

use std::collections::BTreeMap;
use std::sync::Arc;

use rrk_model::{RrkError, RrkResult};

pub const COMPRESSION_NONE: u16 = 1;
pub const COMPRESSION_PALMDOC: u16 = 2;
pub const COMPRESSION_HUFF_CDIC: u16 = 17_480;

/// MOBI/PalmDB 容器（只读视图：不解压、不切分）。
pub struct MobiContainer {
    pub bytes: Arc<[u8]>,
    pub compression: u16,
    pub text_length: u32,
    pub text_record_count: u16,
    pub record_size: u16,
    pub encryption: u16,
    pub text_encoding: u32,
    pub version: u32,
    pub record_offsets: Vec<u32>,
    pub record_zero: usize,
    /// 全书名（MOBI 头 fullNameOffset/Length）。
    pub full_name: Option<String>,
    pub resource_start: u32,
    pub huffcdic: u32,
    pub num_huffcdic: u32,
    pub trailing_flags: u32,
    /// EXTH 原始值（type → bytes）。
    pub exth: BTreeMap<u32, Vec<u8>>,
    /// 封面资源记录号（resource_start + EXTH 201）。
    pub cover_record: Option<u32>,
}

fn be16(b: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([b[off], b[off + 1]])
}

fn be32(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

impl MobiContainer {
    /// 记录 [index] 的字节切片。
    pub fn record(&self, index: usize) -> RrkResult<&[u8]> {
        let start = *self
            .record_offsets
            .get(index)
            .ok_or_else(|| RrkError::corrupt(format!("MOBI: 记录越界 {index}")))? as usize;
        let end = self
            .record_offsets
            .get(index + 1)
            .map(|&x| x as usize)
            .unwrap_or(self.bytes.len());
        if end <= start || end > self.bytes.len() {
            return Err(RrkError::corrupt(format!("MOBI: 记录范围非法 {index}")));
        }
        Ok(&self.bytes[start..end])
    }

    /// EXTH 文本值（UTF-8 优先，失败退 latin1）。
    pub fn exth_text(&self, ty: u32) -> Option<String> {
        let raw = self.exth.get(&ty)?;
        Some(match std::str::from_utf8(raw) {
            Ok(s) => s.trim_matches('\0').to_string(),
            Err(_) => raw.iter().map(|&b| b as char).collect(),
        })
    }

    /// EXTH 数值类型（如 201 封面偏移）。
    pub fn exth_uint(&self, ty: u32) -> Option<u32> {
        let raw = self.exth.get(&ty)?;
        match raw.len() {
            4 => Some(be32(raw, 0)),
            2 => Some(be16(raw, 0) as u32),
            1 => Some(raw[0] as u32),
            n if n > 4 => Some(be32(raw, n - 4)),
            _ => None,
        }
    }
}

/// 解析 PalmDB 记录表 + 头结构；`magic` 已由嗅探把关，这里做结构校验。
pub fn parse(bytes: Arc<[u8]>) -> RrkResult<MobiContainer> {
    if bytes.len() < 100 {
        return Err(RrkError::corrupt("MOBI: 文件过小"));
    }
    let record_count = be16(&bytes, 76) as usize;
    if record_count == 0 || 78 + record_count * 8 > bytes.len() {
        return Err(RrkError::corrupt("MOBI: PDB 记录表越界"));
    }
    let record_offsets: Vec<u32> = (0..record_count)
        .map(|i| be32(&bytes, 78 + i * 8))
        .collect();
    let record_zero = record_offsets[0] as usize;
    if record_zero + 24 > bytes.len() {
        return Err(RrkError::corrupt("MOBI: 记录 0 越界"));
    }
    let r0 = &bytes[record_zero..];

    let compression = be16(r0, 0);
    let text_length = be32(r0, 4);
    let text_record_count = be16(r0, 8);
    let record_size = be16(r0, 10);
    let encryption = be16(r0, 12);

    let has_mobi = r0.len() >= 20 && &r0[16..20] == b"MOBI";
    if !has_mobi {
        return Err(RrkError::corrupt("MOBI: 缺少 MOBI 头"));
    }
    let header_length = be32(r0, 20) as usize;
    let text_encoding = be32(r0, 28);
    let version = be32(r0, 36);
    let title_offset = be32(r0, 84) as usize;
    let title_length = be32(r0, 88) as usize;
    let resource_start = be32(r0, 108);
    let huffcdic = be32(r0, 112);
    let num_huffcdic = be32(r0, 116);
    let exth_flags = be32(r0, 128);
    let trailing_flags = be32(r0, 240);

    let full_name = if title_offset > 0 && title_length > 0 && record_zero + title_offset + title_length <= bytes.len() {
        let raw = &bytes[record_zero + title_offset..record_zero + title_offset + title_length];
        Some(decode_text(raw, text_encoding))
    } else {
        None
    };

    let mut exth = BTreeMap::new();
    if exth_flags & 0x40 != 0 {
        let exth_at = 16 + header_length;
        if exth_at + 12 <= r0.len() && &r0[exth_at..exth_at + 4] == b"EXTH" {
            let count = be32(r0, exth_at + 8) as usize;
            let mut p = exth_at + 12;
            for _ in 0..count {
                if p + 8 > r0.len() {
                    break;
                }
                let ty = be32(r0, p);
                let len = be32(r0, p + 4) as usize;
                if len < 8 || p + len > r0.len() {
                    break;
                }
                exth.insert(ty, r0[p + 8..p + len].to_vec());
                p += len;
            }
        }
    }

    // 封面：EXTH 201 偏移 + resourceStart（foliate 同款；0xFFFFFFFF 视为无）
    let cover_record = exth
        .get(&201)
        .and_then(|raw| {
            (raw.len() == 4).then(|| be32(raw, 0)).filter(|&v| v != 0xFFFF_FFFF)
        })
        .map(|delta| resource_start + delta);

    Ok(MobiContainer {
        bytes,
        compression,
        text_length,
        text_record_count,
        record_size: if record_size == 0 { 4096 } else { record_size },
        encryption,
        text_encoding,
        version,
        record_offsets,
        record_zero,
        full_name,
        resource_start,
        huffcdic,
        num_huffcdic,
        trailing_flags,
        exth,
        cover_record,
    })
}

/// 按 MOBI 头 textEncoding 解码（65001=UTF-8，其余 CP1252 近似；绝不抛）。
pub fn decode_text(bytes: &[u8], encoding: u32) -> String {
    if encoding == 65001 {
        match std::str::from_utf8(bytes) {
            Ok(s) => s.to_string(),
            Err(_) => {
                let (cow, _, _) = encoding_rs::UTF_8.decode(bytes);
                cow.into_owned()
            }
        }
    } else {
        let (cow, _, _) = encoding_rs::WINDOWS_1252.decode(bytes);
        cow.into_owned()
    }
}
