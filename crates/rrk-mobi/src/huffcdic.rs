//! HUFF/CDIC 解压（自研实现，compression = 17480）。
//!
//! 结构与解码口径对齐 foliate-js `huffcdic()` 与 calibre `huffcdic.py`（两者互证）：
//! - **HUFF 记录**：`'HUFF'` + header_len(@4) + off1(@8) + off2(@12)；
//!   dict1 = 256 × u32（@off1：codelen = v & 0x1F、term = v & 0x80、maxcode = v >> 8）；
//!   table2 = 32 × (u32 mincode, u32 maxcode)（@off2，按码长 1..=32 索引）。
//! - **CDIC 记录**（紧随 HUFF 的 `num_huffcdic - 1` 条）：`'CDIC'` + header_len(@4) +
//!   total_entries(@8) + code_bits(@12)；条目区 = n × u16 偏移表 + 条目（u16 长度（低 15 位），
//!   最高位 = 1 表示已是明文，= 0 表示自身仍是一段待解码流（递归展开））。
//! - **解码**：MSB 优先取 32 位窗口 → dict1 定码长（非 term 则按 table2 mincode 升长）
//!   → 词典下标 = maxcode − 窗口高 `codelen` 位。

use rrk_model::{RrkError, RrkResult};

enum Entry {
    /// 最高位标记：明文。
    Plain(Vec<u8>),
    /// 自身仍需解码的流。
    Compressed(Vec<u8>),
    /// 展开中（环保护，同 calibre 的 None 占位）。
    Expanding,
    /// 已展开（缓存）。
    Expanded(Vec<u8>),
}

/// HUFF/CDIC 解码器（持词典与码表）。
pub struct HuffCdic {
    codelen: [u8; 256],
    term: [bool; 256],
    maxcode1: [u32; 256],
    mincode2: [u32; 33],
    maxcode2: [u32; 33],
    dict: Vec<Entry>,
}

fn be32(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn be16(b: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([b[off], b[off + 1]])
}

/// 单次 `unpack` 调用的输出预算（防解压炸弹：真实文本记录压缩后仅数 KB，
/// 即使 1000:1 脏压缩也远小于此；超限报错而非 abort）。
const MAX_UNPACK_BYTES: usize = 32 * 1024 * 1024;

/// 词典条目递归展开深度上限（链式嵌套防爆栈；真实书 ≤ 个位数层）。
const MAX_EXPAND_DEPTH: usize = 64;

/// 从 bitpos 起读 32 位（MSB 优先，越界补 0）。
fn read32(data: &[u8], bitpos: usize) -> u32 {
    let start = bitpos >> 3;
    let shift = bitpos & 7;
    let mut packed: u64 = 0;
    for i in 0..5 {
        packed = (packed << 8) | (*data.get(start + i).unwrap_or(&0) as u64);
    }
    ((packed >> (8 - shift)) & 0xFFFF_FFFF) as u32
}

impl HuffCdic {
    /// 载入 HUFF 记录。
    pub fn load_huff(record: &[u8]) -> RrkResult<Self> {
        if record.len() < 16 || &record[0..4] != b"HUFF" {
            return Err(RrkError::corrupt("MOBI: HUFF 记录非法"));
        }
        let off1 = be32(record, 8) as usize;
        let off2 = be32(record, 12) as usize;
        if off1 + 256 * 4 > record.len() || off2 + 32 * 8 > record.len() {
            return Err(RrkError::corrupt("MOBI: HUFF 码表越界"));
        }
        let mut s = HuffCdic {
            codelen: [0; 256],
            term: [false; 256],
            maxcode1: [0; 256],
            mincode2: [0; 33],
            maxcode2: [0; 33],
            dict: Vec::new(),
        };
        for i in 0..256 {
            let v = be32(record, off1 + i * 4);
            s.codelen[i] = (v & 0x1F) as u8;
            s.term[i] = v & 0x80 != 0;
            s.maxcode1[i] = v >> 8;
            if s.codelen[i] == 0 {
                return Err(RrkError::corrupt("MOBI: HUFF 码长为 0"));
            }
        }
        for len in 1..=32 {
            let base = off2 + (len - 1) * 8;
            s.mincode2[len] = be32(record, base);
            s.maxcode2[len] = be32(record, base + 4);
        }
        Ok(s)
    }

    /// 载入一条 CDIC 记录（可多次调用拼接词典）。
    pub fn load_cdic(&mut self, record: &[u8]) -> RrkResult<()> {
        if record.len() < 16 || &record[0..4] != b"CDIC" {
            return Err(RrkError::corrupt("MOBI: CDIC 记录非法"));
        }
        let header_len = be32(record, 4) as usize;
        let total_entries = be32(record, 8) as usize;
        let code_bits = be32(record, 12) as usize;
        if header_len > record.len() || code_bits > 20 {
            return Err(RrkError::corrupt("MOBI: CDIC 头非法"));
        }
        let entries = &record[header_len..];
        // 偏移表本身可能越界（code_bits/total_entries 不可信）：条目数以
        // 「偏移表实际能容纳的条数」夹紧，否则 be16(entries, j*2) 越界 panic。
        let n = (1usize << code_bits)
            .min(total_entries.saturating_sub(self.dict.len()))
            .min(entries.len() / 2);
        for j in 0..n {
            let off = be16(entries, j * 2) as usize;
            if off + 2 > entries.len() {
                return Err(RrkError::corrupt("MOBI: CDIC 条目越界"));
            }
            let x = be16(entries, off);
            let len = (x & 0x7FFF) as usize;
            let plain = x & 0x8000 != 0;
            let end = off + 2 + len;
            if end > entries.len() {
                return Err(RrkError::corrupt("MOBI: CDIC 条目数据越界"));
            }
            let data = entries[off + 2..end].to_vec();
            self.dict.push(if plain { Entry::Plain(data) } else { Entry::Compressed(data) });
        }
        Ok(())
    }

    /// 解码一段 HUFF/CDIC 压缩流（输出带预算，防解压炸弹）。
    pub fn unpack(&mut self, data: &[u8]) -> RrkResult<Vec<u8>> {
        let mut out = Vec::with_capacity(data.len() * 2);
        let mut budget = MAX_UNPACK_BYTES;
        self.unpack_into(data, &mut out, 0, &mut budget)?;
        Ok(out)
    }

    fn unpack_into(
        &mut self,
        data: &[u8],
        out: &mut Vec<u8>,
        depth: usize,
        budget: &mut usize,
    ) -> RrkResult<()> {
        if depth > MAX_EXPAND_DEPTH {
            return Err(RrkError::corrupt("MOBI: CDIC 词典嵌套过深"));
        }
        let bit_len = data.len() * 8;
        let mut pos = 0usize;
        while pos < bit_len {
            let bits = read32(data, pos);
            let top = (bits >> 24) as usize;
            let mut code_len = self.codelen[top] as usize;
            let value = if self.term[top] {
                self.maxcode1[top]
            } else {
                while code_len <= 32 && (bits >> (32 - code_len)) < self.mincode2[code_len] {
                    code_len += 1;
                }
                if code_len > 32 {
                    return Err(RrkError::corrupt("MOBI: HUFF 码长越界"));
                }
                self.maxcode2[code_len]
            };
            pos += code_len;
            if pos > bit_len {
                break;
            }
            let idx = (value as u64).wrapping_sub((bits >> (32 - code_len)) as u64) as usize;
            if idx >= self.dict.len() {
                return Err(RrkError::corrupt("MOBI: 词典下标越界"));
            }
            let chunk = self.expand(idx, depth + 1, budget)?;
            if chunk.len() > *budget {
                return Err(RrkError::corrupt("MOBI: CDIC 解压输出超限"));
            }
            *budget -= chunk.len();
            out.extend_from_slice(&chunk);
        }
        Ok(())
    }

    /// 展开词典条目（Compressed 递归解码，带缓存、环保护与深度/预算限制）。
    fn expand(&mut self, idx: usize, depth: usize, budget: &mut usize) -> RrkResult<Vec<u8>> {
        match std::mem::replace(&mut self.dict[idx], Entry::Expanding) {
            Entry::Plain(v) => {
                self.dict[idx] = Entry::Plain(v.clone());
                Ok(v)
            }
            Entry::Expanded(v) => {
                self.dict[idx] = Entry::Expanded(v.clone());
                Ok(v)
            }
            Entry::Compressed(raw) => {
                let mut out = Vec::new();
                self.unpack_into(&raw, &mut out, depth, budget)?; // 递归展开
                self.dict[idx] = Entry::Expanded(out.clone());
                Ok(out)
            }
            Entry::Expanding => Err(RrkError::corrupt("MOBI: CDIC 词典成环")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造最小 HUFF 记录：8 位定长码，码值 K → 词典下标 min(K, num_codes-1)。
    fn build_huff(num_codes: u32) -> Vec<u8> {
        let off1 = 24usize;
        let off2 = off1 + 256 * 4;
        let mut rec = vec![0u8; off2 + 32 * 8];
        rec[0..4].copy_from_slice(b"HUFF");
        rec[4..8].copy_from_slice(&24u32.to_be_bytes());
        rec[8..12].copy_from_slice(&(off1 as u32).to_be_bytes());
        rec[12..16].copy_from_slice(&(off2 as u32).to_be_bytes());
        for k in 0..256usize {
            // term=1, codelen=8, maxcode_raw = idx + K ⇒ 词典下标 = maxcode_raw − K = idx
            let idx = k.min(num_codes as usize - 1);
            let v = ((idx + k) as u32) << 8 | 0x80 | 8;
            rec[off1 + k * 4..off1 + k * 4 + 4].copy_from_slice(&v.to_be_bytes());
        }
        rec
    }

    /// 构造 CDIC 记录：条目 = (u16 len|0x8000, bytes)。
    fn build_cdic(items: &[&[u8]]) -> Vec<u8> {
        let mut entries = Vec::new();
        let table_len = items.len() * 2;
        let mut offsets = Vec::new();
        let mut body = Vec::new();
        for it in items {
            offsets.push((table_len + body.len()) as u16);
            body.extend_from_slice(&((it.len() as u16) | 0x8000).to_be_bytes());
            body.extend_from_slice(it);
        }
        for o in offsets {
            entries.extend_from_slice(&o.to_be_bytes());
        }
        entries.extend_from_slice(&body);
        let mut rec = Vec::new();
        rec.extend_from_slice(b"CDIC");
        rec.extend_from_slice(&16u32.to_be_bytes());
        rec.extend_from_slice(&(items.len() as u32).to_be_bytes());
        rec.extend_from_slice(&1u32.to_be_bytes()); // code_bits=1 → 每记录 ≤ 2 条
        rec.extend_from_slice(&entries);
        rec
    }

    /// 构造 CDIC 记录：条目 = (u16 len|0x8000, bytes)，可指定 code_bits 与明文标记。
    fn build_cdic_bits(items: &[(Vec<u8>, bool)], code_bits: u32) -> Vec<u8> {
        let mut entries = Vec::new();
        let table_len = items.len() * 2;
        let mut offsets = Vec::new();
        let mut body = Vec::new();
        for (it, plain) in items {
            offsets.push((table_len + body.len()) as u16);
            let flag = if *plain { 0x8000u16 } else { 0 };
            body.extend_from_slice(&((it.len() as u16) | flag).to_be_bytes());
            body.extend_from_slice(it);
        }
        for o in offsets {
            entries.extend_from_slice(&o.to_be_bytes());
        }
        entries.extend_from_slice(&body);
        let mut rec = Vec::new();
        rec.extend_from_slice(b"CDIC");
        rec.extend_from_slice(&16u32.to_be_bytes());
        rec.extend_from_slice(&(items.len() as u32).to_be_bytes());
        rec.extend_from_slice(&code_bits.to_be_bytes());
        rec.extend_from_slice(&entries);
        rec
    }

    #[test]
    fn decode_literals() {
        let huff = build_huff(2);
        let cdic = build_cdic(&[b"hel", b"lo"]);
        let mut d = HuffCdic::load_huff(&huff).unwrap();
        d.load_cdic(&cdic).unwrap();
        let out = d.unpack(&[0x00, 0x01]).unwrap();
        assert_eq!(out, b"hello".to_vec());
    }

    #[test]
    fn decode_recursive_entry() {
        // 条目 0 = 待解码流 [0x01]（指向条目 1），条目 1 = 明文 "lo"
        let huff = build_huff(2);
        let mut cdic = build_cdic(&[b"lo"]);
        // 手工插一条 compressed 条目到最前：重建
        let mut d = HuffCdic::load_huff(&huff).unwrap();
        // 词典顺序：0 → compressed([0x01])，1 → plain("lo")
        let mut entries = Vec::new();
        let e0: &[u8] = &[0x01];
        let e1: &[u8] = b"lo";
        let table_len = 2 * 2;
        let o0 = table_len as u16;
        let o1 = (table_len + 2 + e0.len()) as u16;
        entries.extend_from_slice(&o0.to_be_bytes());
        entries.extend_from_slice(&o1.to_be_bytes());
        entries.extend_from_slice(&(e0.len() as u16).to_be_bytes()); // 高位=0 → compressed
        entries.extend_from_slice(e0);
        entries.extend_from_slice(&((e1.len() as u16) | 0x8000).to_be_bytes());
        entries.extend_from_slice(e1);
        cdic.clear();
        cdic.extend_from_slice(b"CDIC");
        cdic.extend_from_slice(&16u32.to_be_bytes());
        cdic.extend_from_slice(&2u32.to_be_bytes());
        cdic.extend_from_slice(&1u32.to_be_bytes());
        cdic.extend_from_slice(&entries);
        d.load_cdic(&cdic).unwrap();
        let out = d.unpack(&[0x00]).unwrap();
        assert_eq!(out, b"lo".to_vec()); // 递归展开缓存
        let out2 = d.unpack(&[0x00]).unwrap();
        assert_eq!(out2, b"lo".to_vec()); // 命中缓存
    }

    #[test]
    fn cdic_offset_table_clamped() {
        // 回归（审计 #2）：code_bits 虚报每记录 256 条，但偏移表只有 2 条空间
        // ——原实现 be16(entries, j*2) 直接越界 panic；现夹紧到表容量。
        let huff = build_huff(2);
        let cdic = build_cdic_bits(
            &[(b"hel".to_vec(), true), (b"lo".to_vec(), true)],
            8,
        );
        let mut d = HuffCdic::load_huff(&huff).unwrap();
        d.load_cdic(&cdic).unwrap();
        assert_eq!(d.dict.len(), 2);
        let out = d.unpack(&[0x00, 0x01]).unwrap();
        assert_eq!(out, b"hello".to_vec());
    }

    #[test]
    fn expand_depth_limit() {
        // 回归（审计 #3b）：链式嵌套超深度上限 → 报错而非爆栈
        let depth = MAX_EXPAND_DEPTH + 8;
        let huff = build_huff((depth + 1) as u32);
        let mut items: Vec<(Vec<u8>, bool)> = (0..depth)
            .map(|i| (vec![(i + 1) as u8], false))
            .collect();
        items.push((b"x".to_vec(), true));
        let cdic = build_cdic_bits(&items, 16);
        let mut d = HuffCdic::load_huff(&huff).unwrap();
        d.load_cdic(&cdic).unwrap();
        assert!(d.unpack(&[0x00]).is_err());
    }

    #[test]
    fn unpack_output_budget() {
        // 回归（审计 #3a）：解压炸弹（单条 1KB 明文 × 3 万次引用 ≈ 32MB+）
        // 超输出预算即报错，不 allow 无限展开
        let payload = vec![b'a'; 1000];
        let huff = build_huff(1);
        let cdic = build_cdic(&[payload.as_slice()]);
        let mut d = HuffCdic::load_huff(&huff).unwrap();
        d.load_cdic(&cdic).unwrap();
        let codes = vec![0u8; MAX_UNPACK_BYTES / 1000 + 16];
        assert!(d.unpack(&codes).is_err());
    }
}
