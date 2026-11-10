//! PalmDB / PalmDOC / MOBI 头 / EXTH 解析（自研）。
//!
//! 字段偏移以 **记录 0 起点**为基准，口径对齐 foliate-js `mobi.js` 与 MobileRead MOBI wiki：
//! PalmDOC 头 16 字节 → MOBI 头（`MOBI` 魔数 @16）→ EXTH（@mobi.length+16）。
//! 压缩字段语义（规范口径，勿再误读）：**1 = 无压缩，2 = PalmDOC，17480 = HUFF/CDIC**。

use std::collections::BTreeMap;
use std::sync::Arc;

use rrk_model::{RrkError, RrkResult};
use rrk_source::ByteSource;

pub const COMPRESSION_NONE: u16 = 1;
pub const COMPRESSION_PALMDOC: u16 = 2;
pub const COMPRESSION_HUFF_CDIC: u16 = 17_480;

/// MOBI/PalmDB 容器（只读视图：不解压、不切分）。
///
/// 批次 5 深改：原先持整包 `bytes: Arc<[u8]>`（供 `record()` 切片），
/// 现改为按需读取的 `src: Arc<dyn ByteSource>`——`record()` 按偏移从源
/// 拉取，**不再整包驻留**。原始字节（含资源记录）按需读、读完即弃；
/// 稳态内存只剩 header 字段 + 装配全文（供章切片）。
pub struct MobiContainer {
    pub src: Arc<dyn ByteSource>,
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

/// MOBI 头扩展字段（@28 起）读取：短头脏书里 headerLength 未覆盖的字段
/// 按 0（缺省语义）处理，绝不越界 panic（record0 可短至 24 字节）。
fn opt32(r0: &[u8], off: usize) -> u32 {
    if off + 4 <= r0.len() { be32(r0, off) } else { 0 }
}

impl MobiContainer {
    /// 记录 [index] 的字节（**按需从源读取**，返回 owned `Vec<u8>`）。
    ///
    /// 批次 5 深改：原先返回 `&[u8]`（借自整包 `bytes`），现按偏移从
    /// `src.read_at` 拉取——**不驻留整包**，读完即弃。峰值内存 = 单条记录。
    pub fn record(&self, index: usize) -> RrkResult<Vec<u8>> {
        let start = *self
            .record_offsets
            .get(index)
            .ok_or_else(|| RrkError::corrupt(format!("MOBI: 记录越界 {index}")))? as usize;
        let end = self
            .record_offsets
            .get(index + 1)
            .map(|&x| x as usize)
            .unwrap_or(self.src.len() as usize);
        if end <= start || end as u64 > self.src.len() {
            return Err(RrkError::corrupt(format!("MOBI: 记录范围非法 {index}")));
        }
        self.src.read_at(start as u64, end - start)
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
pub fn parse(src: Arc<dyn ByteSource>) -> RrkResult<MobiContainer> {
    let total = src.len();
    if total < 100 {
        return Err(RrkError::corrupt("MOBI: 文件过小"));
    }
    // PalmDB 头（0..78）：记录数在偏移 76（按需读，不驻留整包）
    let hdr = src.read_at(0, 78)?;
    if hdr.len() < 78 {
        return Err(RrkError::corrupt("MOBI: PDB 头截断"));
    }
    let record_count = be16(&hdr, 76) as usize;
    if record_count == 0 || 78 + record_count * 8 > total as usize {
        return Err(RrkError::corrupt("MOBI: PDB 记录表越界"));
    }
    // 记录偏移表（按需读）
    let off_table = src.read_at(78, record_count * 8)?;
    if off_table.len() < record_count * 8 {
        return Err(RrkError::corrupt("MOBI: PDB 记录表截断"));
    }
    let record_offsets: Vec<u32> = (0..record_count)
        .map(|i| be32(&off_table, i * 8))
        .collect();
    let record_zero = record_offsets[0] as usize;
    if record_zero + 24 > total as usize {
        return Err(RrkError::corrupt("MOBI: 记录 0 越界"));
    }
    // 记录 0（PalmDOC/MOBI 头 + EXTH）：按需读；r0 保持 &[u8] 视图，
    // 字段提取逻辑与旧实现逐字一致（EXTH 在记录 0 内）。
    let r0_len = record_offsets
        .get(1)
        .map(|&x| (x as usize).saturating_sub(record_zero))
        .unwrap_or((total as usize).saturating_sub(record_zero))
        .max(24);
    let r0_vec = src.read_at(record_zero as u64, r0_len)?;
    if r0_vec.len() < 24 {
        return Err(RrkError::corrupt("MOBI: 记录 0 过短"));
    }
    let r0: &[u8] = &r0_vec;

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
    let text_encoding = opt32(r0, 28);
    let version = opt32(r0, 36);
    let title_offset = opt32(r0, 84) as usize;
    let title_length = opt32(r0, 88) as usize;
    let resource_start = opt32(r0, 108);
    let huffcdic = opt32(r0, 112);
    let num_huffcdic = opt32(r0, 116);
    let exth_flags = opt32(r0, 128);
    let trailing_flags = opt32(r0, 240);

    // 标题：按需读（fullNameOffset 相对记录 0，可能跨记录边界，直接从源拉取）
    let title_start = (record_zero as u64) + (title_offset as u64);
    let title_end = title_start + (title_length as u64);
    let full_name = if title_offset > 0 && title_length > 0 && title_end <= total {
        let raw = src.read_at(title_start, title_length)?;
        // 真实书 titleLength 常含尾部 NUL——不剔会把 "书名\0" 直接交给 UI
        Some(decode_text(&raw, text_encoding).trim_end_matches('\0').to_string())
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
        .map(|delta| resource_start.saturating_add(delta));

    Ok(MobiContainer {
        src,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 最小 PalmDB：1 条记录，record0 定长 `record0_len`（魔数 + 最小 MOBI 头）。
    fn mini_pdb(record0_len: usize) -> Vec<u8> {
        let record_zero = 78 + 8; // 头 78B + 记录表 1×8B
        let mut b = vec![0u8; record_zero + record0_len];
        b[60..68].copy_from_slice(b"BOOKMOBI");
        b[76..78].copy_from_slice(&1u16.to_be_bytes()); // recordCount
        b[78..82].copy_from_slice(&(record_zero as u32).to_be_bytes());
        if record0_len >= 24 {
            b[record_zero + 16..record_zero + 20].copy_from_slice(b"MOBI");
            b[record_zero + 20..record_zero + 24].copy_from_slice(&8u32.to_be_bytes()); // headerLength
        }
        b
    }

    #[test]
    fn short_record0_defaults_not_panic() {
        // 回归（审计 #1）：原实现在 record0 < 244B 时仍按偏移 240 直读 → 越界 panic。
        let c = parse(Arc::new(rrk_source::MemSource::new(Arc::from(mini_pdb(24))))).unwrap();
        assert_eq!(c.trailing_flags, 0);
        assert_eq!(c.text_encoding, 0);
        assert!(c.full_name.is_none());
    }

    #[test]
    fn record0_too_short_errors() {
        assert!(parse(Arc::new(rrk_source::MemSource::new(Arc::from(mini_pdb(16))))).is_err());
    }

    #[test]
    fn cover_record_offset_saturates() {
        // EXTH 201 封面偏移 + resource_start 双 0x80000000 → u32 加法溢出，
        // 必须饱和（debug 下裸加法直接 panic）。
        let mut b = mini_pdb(244);
        let rz = 78 + 8;
        b[rz + 108..rz + 112].copy_from_slice(&0xFFFF_FF00u32.to_be_bytes()); // resourceStart
        b[rz + 128..rz + 132].copy_from_slice(&0x40u32.to_be_bytes()); // exthFlags
        // EXTH @16+headerLength(8)=24：magic + 总长 24 + count 1 + (201,12,0x200)
        let e = rz + 24;
        b[e..e + 4].copy_from_slice(b"EXTH");
        b[e + 4..e + 8].copy_from_slice(&24u32.to_be_bytes());
        b[e + 8..e + 12].copy_from_slice(&1u32.to_be_bytes());
        b[e + 12..e + 16].copy_from_slice(&201u32.to_be_bytes());
        b[e + 16..e + 20].copy_from_slice(&12u32.to_be_bytes());
        b[e + 20..e + 24].copy_from_slice(&0x200u32.to_be_bytes());
        let c = parse(Arc::new(rrk_source::MemSource::new(Arc::from(b)))).unwrap();
        // 0xFFFFFF00 + 0x200 饱和到 0xFFFFFFFF；裸 u32 加法在 debug 构建会
        // 直接 panic、release 会回绕成 0x100（错记录号）——本断言守饱和语义。
        assert_eq!(c.cover_record, Some(0xFFFF_FFFF));
    }
}
