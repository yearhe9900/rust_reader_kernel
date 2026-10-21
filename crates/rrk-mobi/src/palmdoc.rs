//! PalmDOC LZ77 解压（自研）+ 文本记录尾部数据剔除。
//!
//! 算法口径对齐 foliate-js `decompressPalmDOC`（经海量真实书验证的 PalmDOC 变体）：
//! - `0x00` → 字面量 0x00；
//! - `0x01..=0x08` → 随后 N 字节原样拷贝；
//! - `0x09..=0x7F` → 字面量；
//! - `0x80..=0xBF` → 2 字节对：距离 = 低 14 位 >> 3（11 位，1..=2047），长度 = 低 3 位 + 3；
//! - `0xC0..=0xFF` → 「空格 + (b ^ 0x80)」两字节。

/// PalmDOC 解压。非法回溯按截断容错（脏书不整本失败）。
pub fn decompress_palmdoc(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len() * 2);
    let mut i = 0usize;
    while i < src.len() {
        let b = src[i];
        i += 1;
        match b {
            0 => out.push(0),
            1..=8 => {
                for _ in 0..b {
                    if i >= src.len() {
                        break;
                    }
                    out.push(src[i]);
                    i += 1;
                }
            }
            9..=0x7F => out.push(b),
            0x80..=0xBF => {
                if i >= src.len() {
                    break;
                }
                let pair = ((b as u16) << 8) | src[i] as u16;
                i += 1;
                let distance = ((pair & 0x3FFF) >> 3) as usize;
                let length = ((pair & 0x7) + 3) as usize;
                let distance = distance.max(1); // 距离 0 是脏数据，按 1 处理
                for _ in 0..length {
                    let idx = out.len().saturating_sub(distance);
                    let byte = out.get(idx).copied().unwrap_or(0);
                    out.push(byte);
                }
            }
            _ => {
                out.push(b' ');
                out.push(b ^ 0x80);
            }
        }
    }
    out
}

/// 从字节流末尾读变长数（foliate `getVarLenFromEnd` 同款）：高位字节为起点。
fn var_len_from_end(data: &[u8]) -> usize {
    let mut value = 0usize;
    for &byte in data.iter().rev().take(4).collect::<Vec<_>>().into_iter().rev() {
        if byte & 0x80 != 0 {
            value = 0;
        }
        value = (value << 7) | (byte & 0x7F) as usize;
    }
    value
}

/// 剔除文本记录尾部附加数据（`trailingFlags`：bit0 = 多字节重叠，bit1.. = 索引条目）。
/// 返回剔除后的切片。
pub fn remove_trailing_entries<'a>(data: &'a [u8], trailing_flags: u32) -> &'a [u8] {
    let num_entries = (trailing_flags >> 1).count_ones() as usize;
    let mut end = data.len();
    for _ in 0..num_entries {
        if end == 0 {
            break;
        }
        let length = var_len_from_end(&data[..end]);
        end = end.saturating_sub(length);
    }
    if trailing_flags & 1 != 0 && end > 0 {
        let length = ((data[end - 1] & 0b11) + 1) as usize;
        end = end.saturating_sub(length);
    }
    &data[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_and_run() {
        // 'A' 字面量 + 0x03 后跟 3 字节字面量 + 0xC1 = 空格 + 'A'
        let src = [b'A', 0x03, b'x', b'y', b'z', 0xC1];
        let out = decompress_palmdoc(&src);
        assert_eq!(out, b"Axyz A".to_vec());
    }

    #[test]
    fn backref_pair() {
        // "abc" + pair: 距离=3 长度=3 → pair = (3 << 3) | 0 = 24 = 0x00 0x18 → 编码为 0x80|0x00, 0x18
        let src = [b'a', b'b', b'c', 0x80, 0x18];
        let out = decompress_palmdoc(&src);
        assert_eq!(out, b"abcabc".to_vec());
    }

    #[test]
    fn trailing_removal() {
        // 布局：文本 + 多字节重叠(1B) + 尾条目(数据 1B + 自含变长数 0x82=长度2)
        // trailing_flags: bit0=multibyte, bit1=1 个条目 → 0b11 = 3
        let mut data = b"hello".to_vec();
        data.push(0x00); // multibyte 重叠：(0 & 3) + 1 = 1 字节
        data.push(0xDD); // 尾条目数据
        data.push(0x82); // 变长数：高位起点，值 = 2（含自身）
        let out = remove_trailing_entries(&data, 0b11);
        assert_eq!(out, b"hello".to_vec());
    }
}
