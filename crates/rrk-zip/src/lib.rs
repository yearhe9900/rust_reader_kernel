//! 自研 ZIP 随机读（方案 §3.2「自研可控」红线：zip 中央目录解析自己实现，
//! 仅 inflate 原语用白名单件 flate2(rust_backend/miniz_oxide)）。
//!
//! 只读：EOCD → 中央目录条目表 → 按需取单条目数据（stored/deflate）。
//! 不整包解压；Zip64 / 加密条目明确报错。

use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;

use rrk_model::{RrkError, RrkResult};
use rrk_source::{ByteSource, SourceReader};

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

/// zip 归档（**只驻留中央目录**，条目数据按需从源读取）。
///
/// 深审 B1 ② / 批次 5：旧实现持整包 `Arc<[u8]>`——2GB CBZ 直接 OOM。
/// 现在：
/// - **中央目录常驻**（几 MB 级：条目名 + 偏移 + 大小）；
/// - **条目数据按需读**：读一条解压一条，读完即弃（峰值内存 = 单条目）。
///
/// 代价：条目读取多一次 `read_at` 系统调用（页缓存命中时是内存拷贝，
/// 与旧实现的切片拷贝同量级），换来「整包不驻留」。
pub struct ZipArchive {
    src: Arc<dyn ByteSource>,
    entries: Vec<ZipEntry>,
    /// 条目名 → 下标（解析中央目录时一次建好；重名保留首个，与旧线性 find 同序）。
    by_name: HashMap<Box<str>, u32>,
}

fn u16le(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

fn u32le(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

impl ZipArchive {
    /// 从**内存**字节构造（整包已在内存；条目仍按需读）。
    ///
    /// 与 [`ZipArchive::open`] 的唯一区别是源类型：这里用 `MemSource`。
    pub fn parse(data: Arc<[u8]>) -> RrkResult<Self> {
        let src: Arc<dyn ByteSource> = Arc::new(rrk_source::MemSource::new(data));
        Self::open(src)
    }

    /// 从**按需读取**的源构造：只驻留中央目录，条目数据按需读。
    ///
    /// IO 语义（批次 5）：
    /// 1. 读尾部 64KB+22 找 EOCD（1 次读）；
    /// 2. 读中央目录区（1 次读，几 MB 级）；
    /// 3. 之后**不再读整包**——条目数据在 `read` 时按偏移拉取。
    pub fn open(src: Arc<dyn ByteSource>) -> RrkResult<Self> {
        let total_len = src.len();
        if total_len < 22 {
            return Err(RrkError::corrupt("zip 太小"));
        }
        // 1) 尾部窗口：找 EOCD
        let tail_len = total_len.min(65_557 + 22);
        let tail_off = total_len - tail_len;
        let tail = src.read_at(tail_off, tail_len as usize)?;
        if tail.len() < 22 {
            return Err(RrkError::corrupt("zip 太小"));
        }
        // 找 EOCD（尾部窗口内从后往前扫）
        let search_start = tail.len() - 22;
        let mut eocd = None;
        let mut i = tail.len() - 22;
        loop {
            if u32le(&tail, i) == SIG_EOCD {
                eocd = Some(i);
                break;
            }
            if i == search_start {
                break;
            }
            i -= 1;
        }
        let eocd = eocd.ok_or_else(|| RrkError::corrupt("zip: 未找到 EOCD"))?;

        let total = u16le(&tail, eocd + 10) as usize;
        let cd_size = u32le(&tail, eocd + 12) as usize;
        let cd_off = u32le(&tail, eocd + 16) as usize;
        if cd_off == 0xFFFF_FFFF || cd_size == 0xFFFF_FFFF {
            return Err(RrkError::unsupported("zip: Zip64 不在 P0 范围"));
        }
        if (cd_off as u64) + (cd_size as u64) > total_len {
            return Err(RrkError::corrupt("zip: 中央目录越界"));
        }

        // 2) 读中央目录区（一次）
        let cd = src.read_at(cd_off as u64, cd_size)?;
        let mut entries = Vec::with_capacity(total);
        let mut by_name = HashMap::with_capacity(total);
        let mut p = 0usize;
        while p + 46 <= cd.len() {
            if u32le(&cd, p) != SIG_CENTRAL {
                return Err(RrkError::corrupt("zip: 中央目录签名错误"));
            }
            let flags = u16le(&cd, p + 8);
            let method = u16le(&cd, p + 10);
            let crc32 = u32le(&cd, p + 16);
            let comp_size = u32le(&cd, p + 20) as u64;
            let uncomp_size = u32le(&cd, p + 24) as u64;
            let name_len = u16le(&cd, p + 28) as usize;
            let extra_len = u16le(&cd, p + 30) as usize;
            let comment_len = u16le(&cd, p + 32) as usize;
            let local_offset = u32le(&cd, p + 42) as u64;

            let name_start = p + 46;
            let name_end = name_start + name_len;
            if name_end > cd.len() {
                return Err(RrkError::corrupt("zip: 条目名越界"));
            }
            let name = String::from_utf8_lossy(&cd[name_start..name_end]).into_owned();

            if comp_size == 0xFFFF_FFFF || uncomp_size == 0xFFFF_FFFF {
                return Err(RrkError::unsupported(format!("zip: Zip64 条目 {name}")));
            }
            if flags & 0x1 != 0 {
                return Err(RrkError::unsupported(format!("zip: 加密条目 {name}")));
            }

            by_name.entry(Box::from(name.as_str())).or_insert(entries.len() as u32);
            entries.push(ZipEntry { name, method, comp_size, uncomp_size, crc32, local_offset });
            p = name_end + extra_len + comment_len;
        }

        Ok(Self { src, entries, by_name })
    }

    pub fn entries(&self) -> &[ZipEntry] {
        &self.entries
    }

    /// 按精确名取条目（O(1)：名字下标在解析中央目录时已建好）。
    pub fn entry(&self, name: &str) -> Option<&ZipEntry> {
        self.by_name.get(name).map(|&i| &self.entries[i as usize])
    }

    /// 按后缀模糊匹配（OPF/NCX 定位辅助；仅 open 期调用）。
    pub fn entry_by_suffix(&self, suffix: &str) -> Option<&ZipEntry> {
        self.entries.iter().find(|e| e.name.ends_with(suffix))
    }

    /// 条目压缩数据的**字节区间**（本地头校验后）：`(起点, 长度)`。
    ///
    /// 只算偏移，不读数据——读发生在真正解压时（`read_vec` / `read_prefix`）。
    fn raw_range(&self, entry: &ZipEntry) -> RrkResult<(u64, u64)> {
        let lo = entry.local_offset;
        // 本地头 30 字节（含文件名/扩展字段长度）
        let head = self.src.read_at(lo, 30)?;
        if head.len() < 30 || u32le(&head, 0) != SIG_LOCAL {
            return Err(RrkError::corrupt(format!("zip: 本地头损坏 {}", entry.name)));
        }
        let name_len = u16le(&head, 26) as u64;
        let extra_len = u16le(&head, 28) as u64;
        let data_start = lo + 30 + name_len + extra_len;
        let data_end = data_start + entry.comp_size;
        if data_end > self.src.len() {
            return Err(RrkError::corrupt(format!("zip: 条目数据越界 {}", entry.name)));
        }
        Ok((data_start, entry.comp_size))
    }

    /// 单条目解压输出上限（防御：伪造 uncomp_size 直接当预分配大小 /
    /// deflate 炸弹——fuzz zip_parse 以 1.7KB 输入诱导 OOM 实锤；
    /// 真实电子书单条目远小于此）。
    const MAX_ENTRY_BYTES: u64 = 512 * 1024 * 1024;

    /// 读取并按需 inflate 单个条目（`Vec` 所有权；deflate 路径零二次拷贝）。
    pub fn read_vec(&self, entry: &ZipEntry) -> RrkResult<Vec<u8>> {
        let (start, len) = self.raw_range(entry)?;
        match entry.method {
            0 => {
                // stored：整条读入（输出就是条目本身，无二次拷贝）
                let raw = self.src.read_at(start, len as usize)?;
                if (raw.len() as u64) < len {
                    return Err(RrkError::corrupt(format!("zip: 条目数据截断 {}", entry.name)));
                }
                Ok(raw)
            }
            8 => {
                // 预分配按流大小推导（deflate 压缩比上界 ~1032:1），
                // 不信头部虚报的 uncomp_size——否则 1KB 文件也能申请 512MB
                let plausible = (entry.comp_size.saturating_mul(1032) + 64 * 1024)
                    .min(Self::MAX_ENTRY_BYTES);
                let mut out =
                    Vec::with_capacity(entry.uncomp_size.min(plausible) as usize);
                // 流式解压：源侧按块拉取，峰值内存 = 块缓冲 + 输出，
                // 不再把整条压缩数据读进内存（批次 5）。
                let mut reader = SourceReader::new(Arc::clone(&self.src));
                reader.seek_to(start);
                flate2::read::DeflateDecoder::new(Read::take(reader, len))
                    .take(Self::MAX_ENTRY_BYTES + 1)
                    .read_to_end(&mut out)
                    .map_err(|e| RrkError::corrupt(format!("zip: inflate 失败 {}", entry.name)).with_context(e.to_string()))?;
                if out.len() as u64 > Self::MAX_ENTRY_BYTES {
                    return Err(RrkError::unsupported(format!(
                        "zip: 条目超限（>512MB）{}",
                        entry.name
                    )));
                }
                Ok(out)
            }
            m => Err(RrkError::unsupported(format!("zip: 压缩方法 {m} 不支持（{}）", entry.name))),
        }
    }

    /// 读取并按需 inflate 单个条目（共享缓冲）。
    pub fn read(&self, entry: &ZipEntry) -> RrkResult<Arc<[u8]>> {
        Ok(self.read_vec(entry)?.into())
    }

    /// 限流读条目头部：最多解压/拷贝 `max` 字节（开书探头场景——图片尺寸等
    /// 免全量 inflate，大图条目开书不再整本解压）。
    pub fn read_prefix(&self, entry: &ZipEntry, max: usize) -> RrkResult<Vec<u8>> {
        let (start, len) = self.raw_range(entry)?;
        match entry.method {
            0 => {
                // stored：只读前 max 字节（不必读整条）
                let want = (len as usize).min(max);
                self.src.read_at(start, want)
            }
            8 => {
                // deflate：**流式**解压，只读需要的那部分输出——
                // 源侧用 SourceReader 按块拉取，峰值内存 = 块 + 输出上限，
                // 不再把整条压缩数据读进内存。
                let mut reader = SourceReader::new(Arc::clone(&self.src));
                reader.seek_to(start);
                let mut out = Vec::with_capacity((entry.uncomp_size.min(max as u64)) as usize);
                // 注意：`Vec::take`（固有方法）会遮蔽 `Read::take`，必须完全限定
                flate2::read::DeflateDecoder::new(Read::take(reader, len))
                    .take(max as u64)
                    .read_to_end(&mut out)
                    .map_err(|e| RrkError::corrupt(format!("zip: inflate 失败 {}", entry.name)).with_context(e.to_string()))?;
                Ok(out)
            }
            m => Err(RrkError::unsupported(format!("zip: 压缩方法 {m} 不支持（{}）", entry.name))),
        }
    }

    /// 读取文本条目（UTF-8 宽容解码；UTF-8 正文零拷贝直接进 `String`）。
    pub fn read_string(&self, entry: &ZipEntry) -> RrkResult<String> {
        let bytes = self.read_vec(entry)?;
        let text = match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(e) => {
                // XHTML 偶发 GBK/ISO-8859-1；宽容转 UTF-8
                let (cow, _, _) = encoding_rs::WINDOWS_1252.decode(e.as_bytes());
                cow.into_owned()
            }
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

    /// 批次 5（深审 B1 ②）：从**文件源**打开 zip，只驻留中央目录，
    /// 条目数据按需读——「读 1 个条目」的总读取量应远小于整包。
    #[test]
    fn file_source_reads_entries_on_demand() {
        let dir = std::env::temp_dir().join("rrk_zip_demand");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("book.zip");

        // 造一个有体积的 zip：3 个大条目（各 256KB 随机数据 → deflate 压不动）
        let payload: Vec<u8> = (0..256 * 1024u32).map(|i| (i % 251) as u8).collect();
        let zip_bytes = build_zip(&[("big1.bin", &payload), ("big2.bin", &payload), ("big3.bin", &payload)]);
        std::fs::write(&path, &zip_bytes).unwrap();

        let src: Arc<dyn rrk_source::ByteSource> =
            Arc::new(rrk_source::FileSource::open(&path).unwrap());
        let z = ZipArchive::open(src).unwrap();
        assert_eq!(z.entries().len(), 3);

        // 读一条：内容正确
        let e = z.entry("big2.bin").unwrap();
        let data = z.read(e).unwrap();
        assert_eq!(data.len(), payload.len());
        assert_eq!(&*data, &payload[..]);

        // 读全部：内容都正确
        for name in ["big1.bin", "big2.bin", "big3.bin"] {
            let e = z.entry(name).unwrap();
            assert_eq!(&*z.read(e).unwrap(), &payload[..]);
        }
        std::fs::remove_file(&path).ok();
    }

    /// 批次 5：**读多少字节可被计数**——证明「开书 + 读 1 条」不整包读。
    ///
    /// 用计数源包住真实字节：记录每次 read_at 的请求字节数。
    /// 断言「总请求量 << 整包大小」，即中央目录 + 目标条目即可。
    #[test]
    fn on_demand_reads_do_not_pull_whole_archive() {
        struct Counting {
            inner: Arc<dyn rrk_source::ByteSource>,
            requested: std::sync::atomic::AtomicU64,
        }
        impl rrk_source::ByteSource for Counting {
            fn len(&self) -> u64 {
                self.inner.len()
            }
            fn read_at(&self, off: u64, len: usize) -> rrk_result_alias::RrkResult<Vec<u8>> {
                self.requested
                    .fetch_add(len as u64, std::sync::atomic::Ordering::Relaxed);
                self.inner.read_at(off, len)
            }
        }
        // 局部别名，避免顶层 import 改名
        mod rrk_result_alias {
            pub type RrkResult<T> = rrk_model::RrkResult<T>;
        }

        // 用**不可压缩**的伪随机数据（LCG）：若数据可压，整包会很小，
        // 「按需读」的收益就测不出来。
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let payload: Vec<u8> = (0..256 * 1024u32)
            .map(|_| {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (state >> 33) as u8
            })
            .collect();
        let zip_bytes = build_zip(&[
            ("a.bin", &payload),
            ("b.bin", &payload),
            ("c.bin", &payload),
            ("d.bin", &payload),
        ]);
        let total = zip_bytes.len() as u64;
        let base: Arc<dyn rrk_source::ByteSource> = Arc::new(rrk_source::MemSource::new(Arc::from(
            zip_bytes.into_boxed_slice(),
        )));
        let counting = Arc::new(Counting {
            inner: base,
            requested: std::sync::atomic::AtomicU64::new(0),
        });
        let src: Arc<dyn rrk_source::ByteSource> = counting.clone();
        let z = ZipArchive::open(src).unwrap();
        let e = z.entry("b.bin").unwrap();
        let got = z.read(e).unwrap();
        assert_eq!(got.len(), payload.len());

        let requested = counting.requested.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            requested * 2 < total,
            "开书 + 读 1 条的请求量 {requested} 应远小于整包 {total}"
        );
    }

    /// fuzz 回归（zip_parse OOM 实锤）：头部虚报 uncomp_size（3GB）不得
    /// 付出分配代价——预分配按流大小推导，输出上限 512MB 兑底
    ///（炸弹真解压时也拒绝）。
    #[test]
    fn lying_uncomp_size_does_not_overalloc() {
        let mut zip_bytes = build_zip(&[("a.txt", b"hello zip")]);
        // 局部头：comp_size @18、uncomp_size @22；中央目录（单条目）：uncomp @dir+24
        let comp = u32::from_le_bytes(zip_bytes[18..22].try_into().unwrap());
        let name_len = u16::from_le_bytes(zip_bytes[26..28].try_into().unwrap());
        let dir = 30 + name_len as usize + comp as usize;
        let fake = 3_000_000_000u32; // < 0xFFFF_FFFF（Zip64 拒收线）
        zip_bytes[22..26].copy_from_slice(&fake.to_le_bytes());
        zip_bytes[dir + 24..dir + 28].copy_from_slice(&fake.to_le_bytes());

        let z = ZipArchive::parse(Arc::from(zip_bytes.into_boxed_slice())).unwrap();
        let e = z.entry("a.txt").unwrap();
        assert_eq!(e.uncomp_size, fake as u64, "头应保留虚报值");
        let t = std::time::Instant::now();
        let data = z.read(e).unwrap(); // 真实 9 字节：小内存快速成功
        assert_eq!(data.as_ref(), b"hello zip");
        assert!(t.elapsed().as_secs_f64() < 1.0, "不得为虚报尺寸付出分配/耗时代价");
    }
}
