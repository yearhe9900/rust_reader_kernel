//! 自研 ZIP 随机读（方案 §3.2「自研可控」红线：zip 中央目录解析自己实现，
//! 仅 inflate 原语用白名单件 flate2(rust_backend/miniz_oxide)）。
//!
//! 只读：EOCD → 中央目录条目表 → 按需取单条目数据（stored/deflate）。
//! 不整包解压；Zip64 / 加密条目明确报错。

use std::io::Read;
use std::sync::Arc;

use rrk_model::{RrkError, RrkResult};

const SIG_EOCD: u32 = 0x0605_4b50;
const SIG_CENTRAL: u32 = 0x0201_4b50;
const SIG_LOCAL: u32 = 0x0403_4b50;

/// zip 条目（只存索引信息，不解压）。
#[derive(Debug, Clone)]
pub struct ZipEntry {
    pub name: String,
    /// 0 = stored, 8 = deflate。
    pub method: u16,
    pub comp_size: u64,
    pub uncomp_size: u64,
    pub crc32: u32,
    pub local_offset: u64,
}

/// zip 归档（持源字节，条目按需 inflate）。
pub struct ZipArchive {
    data: Arc<[u8]>,
    entries: Vec<ZipEntry>,
}

fn u16le(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

fn u32le(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

impl ZipArchive {
    /// 解析中央目录（一次 IO 语义：只扫尾部 EOCD + 目录区）。
    pub fn parse(data: Arc<[u8]>) -> RrkResult<Self> {
        if data.len() < 22 {
            return Err(RrkError::corrupt("zip 太小"));
        }
        // 找 EOCD（尾部 64KB+22 内）
        let search_start = data.len().saturating_sub(65_557);
        let mut eocd = None;
        let mut i = data.len() - 22;
        loop {
            if u32le(&data, i) == SIG_EOCD {
                eocd = Some(i);
                break;
            }
            if i == search_start {
                break;
            }
            i -= 1;
        }
        let eocd = eocd.ok_or_else(|| RrkError::corrupt("zip: 未找到 EOCD"))?;

        let total = u16le(&data, eocd + 10) as usize;
        let cd_size = u32le(&data, eocd + 12) as usize;
        let cd_off = u32le(&data, eocd + 16) as usize;
        if cd_off == 0xFFFF_FFFF || cd_size == 0xFFFF_FFFF {
            return Err(RrkError::unsupported("zip: Zip64 不在 P0 范围"));
        }
        if cd_off + cd_size > data.len() {
            return Err(RrkError::corrupt("zip: 中央目录越界"));
        }

        let mut entries = Vec::with_capacity(total);
        let mut p = cd_off;
        while p + 46 <= cd_off + cd_size {
            if u32le(&data, p) != SIG_CENTRAL {
                return Err(RrkError::corrupt("zip: 中央目录签名错误"));
            }
            let flags = u16le(&data, p + 8);
            let method = u16le(&data, p + 10);
            let crc32 = u32le(&data, p + 16);
            let comp_size = u32le(&data, p + 20) as u64;
            let uncomp_size = u32le(&data, p + 24) as u64;
            let name_len = u16le(&data, p + 28) as usize;
            let extra_len = u16le(&data, p + 30) as usize;
            let comment_len = u16le(&data, p + 32) as usize;
            let local_offset = u32le(&data, p + 42) as u64;

            let name_start = p + 46;
            let name_end = name_start + name_len;
            if name_end > data.len() {
                return Err(RrkError::corrupt("zip: 条目名越界"));
            }
            let name = String::from_utf8_lossy(&data[name_start..name_end]).into_owned();

            if comp_size == 0xFFFF_FFFF || uncomp_size == 0xFFFF_FFFF {
                return Err(RrkError::unsupported(format!("zip: Zip64 条目 {name}")));
            }
            if flags & 0x1 != 0 {
                return Err(RrkError::unsupported(format!("zip: 加密条目 {name}")));
            }

            entries.push(ZipEntry { name, method, comp_size, uncomp_size, crc32, local_offset });
            p = name_end + extra_len + comment_len;
        }

        Ok(Self { data, entries })
    }

    pub fn entries(&self) -> &[ZipEntry] {
        &self.entries
    }

    /// 按精确名取条目。
    pub fn entry(&self, name: &str) -> Option<&ZipEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// 按后缀模糊匹配（OPF/NCX 定位辅助）。
    pub fn entry_by_suffix(&self, suffix: &str) -> Option<&ZipEntry> {
        self.entries.iter().find(|e| e.name.ends_with(suffix))
    }

    /// 读取并按需 inflate 单个条目。
    pub fn read(&self, entry: &ZipEntry) -> RrkResult<Arc<[u8]>> {
        let lo = entry.local_offset as usize;
        if lo + 30 > self.data.len() || u32le(&self.data, lo) != SIG_LOCAL {
            return Err(RrkError::corrupt(format!("zip: 本地头损坏 {}", entry.name)));
        }
        let name_len = u16le(&self.data, lo + 26) as usize;
        let extra_len = u16le(&self.data, lo + 28) as usize;
        let data_start = lo + 30 + name_len + extra_len;
        let data_end = data_start + entry.comp_size as usize;
        if data_end > self.data.len() {
            return Err(RrkError::corrupt(format!("zip: 条目数据越界 {}", entry.name)));
        }
        let raw = &self.data[data_start..data_end];

        match entry.method {
            0 => Ok(Arc::from(raw)),
            8 => {
                let mut out = Vec::with_capacity(entry.uncomp_size as usize);
                flate2::read::DeflateDecoder::new(raw)
                    .read_to_end(&mut out)
                    .map_err(|e| RrkError::corrupt(format!("zip: inflate 失败 {}", entry.name)).with_context(e.to_string()))?;
                Ok(out.into())
            }
            m => Err(RrkError::unsupported(format!("zip: 压缩方法 {m} 不支持（{}）", entry.name))),
        }
    }

    /// 读取文本条目（UTF-8 宽容解码）。
    pub fn read_string(&self, entry: &ZipEntry) -> RrkResult<String> {
        let bytes = self.read(entry)?;
        let text = if let Ok(s) = std::str::from_utf8(&bytes) {
            s.to_string()
        } else {
            // XHTML 偶发 GBK/ISO-8859-1；宽容转 UTF-8
            let (cow, _, _) = encoding_rs::WINDOWS_1252.decode(&bytes);
            cow.into_owned()
        };
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    //! 用 flate2 的 deflate 编码在内存里造一个最小 zip 做回环测试。

    use super::*;
    use std::io::Write;

    fn build_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, content) in files {
            let offset = out.len() as u32;
            let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            enc.write_all(content).unwrap();
            let comp = enc.finish().unwrap();
            out.extend_from_slice(&SIG_LOCAL.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes()); // version
            out.extend_from_slice(&0u16.to_le_bytes()); // flags
            out.extend_from_slice(&8u16.to_le_bytes()); // deflate
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes()); // crc (不校验)
            out.extend_from_slice(&(comp.len() as u32).to_le_bytes());
            out.extend_from_slice(&(content.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&comp);

            central.extend_from_slice(&SIG_CENTRAL.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&8u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&(comp.len() as u32).to_le_bytes());
            central.extend_from_slice(&(content.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let cd_offset = out.len() as u32;
        out.extend_from_slice(&central);
        let cd_size = out.len() as u32 - cd_offset;
        out.extend_from_slice(&SIG_EOCD.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    #[test]
    fn roundtrip_deflate_entries() {
        let zip_bytes = build_zip(&[("a.txt", b"hello zip"), ("dir/b.bin", &[1u8, 2, 3, 4])]);
        let z = ZipArchive::parse(Arc::from(zip_bytes.into_boxed_slice())).unwrap();
        assert_eq!(z.entries().len(), 2);
        let a = z.entry("a.txt").unwrap();
        assert_eq!(z.read(a).unwrap().as_ref(), b"hello zip");
        let b = z.entry("dir/b.bin").unwrap();
        assert_eq!(z.read(b).unwrap().as_ref(), &[1u8, 2, 3, 4]);
    }
}
