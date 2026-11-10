//! rrk-source：内容源的**按需读取**抽象（深审 B1 ② / 路线表第 10 项）。
//!
//! 旧实现的形状是 `ContentSource::read_all() -> Arc<[u8]>`：整本书进堆。
//! 这在 2GB CBZ 上直接 OOM——**痛点是 `std::fs::read` 全量入堆，不是 Zip64**
//! （Zip64 门槛是 >4GB 或 >65535 条目，两件事别捆一起估工）。
//!
//! 这里引入 [`ByteSource`]：只承诺「按偏移读一段」+「总长度」，
//! 于是 zip 中央目录、pdf xref 这类**索引**可以常驻内存，而**正文/图片**
//! 按需读、读完即弃：
//!
//! - [`FileSource`]：路径源，`seek + read` 定位读，**不整包入堆**；
//! - [`MemSource`]：内存源（`rrk_open_bytes` / 已解密内容），切片零拷贝。
//!
//! ## 为什么不用 mmap
//!
//! mmap 是 OS 级映射，收益在「省一次拷贝」，代价是：Windows 上映射文件
//! 不能被另一进程截断写、映射页要驻留、以及额外的平台差异代码。
//! 本抽象**先做到「不整包驻留」**（真正的内存收益），mmap 留作后续可选
//! 实现——届时只要给 [`ByteSource`] 加一个 mmap 实现即可，上层不动。
//!
//! ## 与内容指纹的关系
//!
//! 缓存失效键需要「任意字节变动 → 指纹变」。这要求**读到全部字节**，
//! 但**不要求全部驻留**：[`fingerprint_of`] 按块顺序读、增量哈希，
//! 峰值内存 = 一个块（默认 1 MiB），与文件大小无关。

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rrk_model::{fnv1a64_update, RrkError, RrkResult, FNV_INIT};

/// 内容源：按需读取的字节序列。
///
/// 契约：
/// - `len()` 返回总字节数；
/// - `read_at(off, len)` 读 `[off, off+len)`；越界按「读到 EOF」处理
///   （返回**至多** `len` 字节，不报错）——与 `Read::read` 语义一致，
///   便于直接喂给 inflate 这类流式解码器；
/// - 实现必须**线程安全**（`Send + Sync`）：同一源可被多线程并发读
///   （如 EPUB 的章加载 + CBZ 的图片读并行）。
pub trait ByteSource: Send + Sync {
    /// 总字节数。
    fn len(&self) -> u64;

    /// 从 `off` 起读至多 `len` 字节（越界截断，不报错）。
    fn read_at(&self, off: u64, len: usize) -> RrkResult<Vec<u8>>;

    /// 读全部字节（少数确需整包的场景用；优先用 [`fingerprint_of`] 增量哈希）。
    fn read_all(&self) -> RrkResult<Vec<u8>> {
        let n = self.len();
        let n = usize::try_from(n).map_err(|_| {
            RrkError::unsupported("内容源过大：单次读入超出地址空间").with_context(format!("{n} 字节"))
        })?;
        self.read_at(0, n)
    }
}

/// 路径源：`seek + read` 定位读，**整包不驻留**。
///
/// 内部用 `Mutex<File>`：一次读只持锁做一次 seek+read（毫秒级 IO），
/// 换来实现简单 + 跨平台一致（Windows 上多句柄读同一文件也安全）。
/// 并发瓶颈可接受：读的是页缓存，且 zip 条目读取本就零星。
pub struct FileSource {
    path: PathBuf,
    file: Mutex<File>,
    len: u64,
}

impl FileSource {
    /// 打开路径源（只读）。
    pub fn open(path: impl AsRef<Path>) -> RrkResult<Self> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path).map_err(|e| {
            RrkError::io(format!("读取文件失败: {}", path.display())).with_context(e.to_string())
        })?;
        let len = file
            .metadata()
            .map_err(|e| RrkError::io("读取文件元信息失败").with_context(e.to_string()))?
            .len();
        Ok(Self { path, file: Mutex::new(file), len })
    }

    /// 源路径（供日志/诊断）。
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl ByteSource for FileSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, off: u64, len: usize) -> RrkResult<Vec<u8>> {
        if len == 0 || off >= self.len {
            return Ok(Vec::new());
        }
        // 越界截断到 EOF（与 Read::read 语义一致）
        let want = (self.len - off).min(len as u64) as usize;
        let mut file = match self.file.lock() {
            Ok(g) => g,
            // 锁中毒（曾有 panic）：文件描述符状态仍一致，恢复继续用
            Err(p) => p.into_inner(),
        };
        file.seek(SeekFrom::Start(off)).map_err(|e| {
            RrkError::io(format!("定位失败: {}", self.path.display())).with_context(e.to_string())
        })?;
        let mut buf = vec![0u8; want];
        let mut filled = 0usize;
        while filled < want {
            match file.read(&mut buf[filled..]) {
                Ok(0) => break, // EOF
                Ok(n) => filled += n,
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    return Err(RrkError::io(format!("读取失败: {}", self.path.display()))
                        .with_context(e.to_string()))
                }
            }
        }
        buf.truncate(filled);
        Ok(buf)
    }
}

/// 内存源：整包已在内存（`rrk_open_bytes` / 解密后内容），切片零拷贝。
pub struct MemSource {
    data: Arc<[u8]>,
}

impl MemSource {
    pub fn new(data: Arc<[u8]>) -> Self {
        Self { data }
    }
}

impl ByteSource for MemSource {
    fn len(&self) -> u64 {
        self.data.len() as u64
    }

    fn read_at(&self, off: u64, len: usize) -> RrkResult<Vec<u8>> {
        if len == 0 || off >= self.data.len() as u64 {
            return Ok(Vec::new());
        }
        let start = off as usize;
        let end = (start + len).min(self.data.len());
        Ok(self.data[start..end].to_vec())
    }
}

/// 顺序流式读取器：把 [`ByteSource`] 适配成 [`std::io::Read`]。
///
/// 用于流式解码（inflate / pdf 内容流）：解码器按需拉取，
/// 峰值内存 = 解码器缓冲 + 本读取器的块缓冲（**与源大小无关**）。
pub struct SourceReader {
    src: Arc<dyn ByteSource>,
    pos: u64,
    /// 单次拉取块大小（默认 64 KiB）。
    chunk: usize,
}

impl SourceReader {
    pub fn new(src: Arc<dyn ByteSource>) -> Self {
        Self { src, pos: 0, chunk: 64 * 1024 }
    }

    /// 设定拉取块大小（0 或过小 → 退回 64 KiB）。
    pub fn with_chunk(mut self, chunk: usize) -> Self {
        self.chunk = if chunk == 0 { 64 * 1024 } else { chunk };
        self
    }

    /// 当前读位置。
    pub fn position(&self) -> u64 {
        self.pos
    }

    /// 跳到指定偏移（读器复位）。
    pub fn seek_to(&mut self, off: u64) {
        self.pos = off;
    }
}

impl Read for SourceReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let want = buf.len().min(self.chunk);
        let data = self
            .src
            .read_at(self.pos, want)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        if data.is_empty() {
            return Ok(0); // EOF
        }
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        self.pos += n as u64;
        Ok(n)
    }
}

/// 内容指纹：按块**顺序读 + 增量哈希**，峰值内存 = 一个块。
///
/// 与 `rrk_format::content_fingerprint(&[u8])` **同值**（同一 FNV-1a 序列
/// + 同一长度前缀），因此：
/// - 路径源可以「不整包驻留」也算出与内存源一致的指纹；
/// - 缓存键在两种源之间一致（同一本书走路径开书与走内存开书，命中同一缓存）。
///
/// 块大小 1 MiB：IO 次数与内存占用折中（1 MiB 缓冲，顺序读几乎全命中页缓存）。
pub fn fingerprint_of(src: &dyn ByteSource) -> RrkResult<u64> {
    const CHUNK: usize = 1024 * 1024;
    let total = src.len();
    let mut h = fnv1a64_update(FNV_INIT, &total.to_le_bytes());
    let mut off = 0u64;
    while off < total {
        let want = ((total - off) as usize).min(CHUNK);
        let block = src.read_at(off, want).map_err(|e| {
            RrkError::io("流式哈希读取失败").with_context(e.to_string())
        })?;
        if block.is_empty() {
            break; // 源提前 EOF（截断文件）：按已读部分收尾
        }
        h = fnv1a64_update(h, &block);
        off += block.len() as u64;
    }
    Ok(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem(data: &[u8]) -> MemSource {
        MemSource::new(Arc::from(data.to_vec().into_boxed_slice()))
    }

    #[test]
    fn mem_source_reads_ranges() {
        let s = mem(b"0123456789");
        assert_eq!(s.len(), 10);
        assert_eq!(s.read_at(0, 3).unwrap(), b"012");
        assert_eq!(s.read_at(7, 10).unwrap(), b"789"); // 越界截断
        assert_eq!(s.read_at(10, 5).unwrap(), b""); // EOF
        assert_eq!(s.read_at(0, 0).unwrap(), b"");
    }

    #[test]
    fn file_source_matches_mem_source() {
        let dir = std::env::temp_dir().join("rrk_source_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.bin");
        let data: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();

        let fs = FileSource::open(&path).unwrap();
        let ms = mem(&data);
        assert_eq!(fs.len(), ms.len());
        for (off, len) in [(0usize, 10usize), (100, 500), (4990, 100), (0, 5000)] {
            assert_eq!(
                fs.read_at(off as u64, len).unwrap(),
                ms.read_at(off as u64, len).unwrap(),
                "off={off} len={len} 路径源与内存源应一致"
            );
        }
        std::fs::remove_file(&path).ok();
    }

    /// 流式指纹必须与「整包哈希」同值——这是路径源与内存源共享缓存键的前提。
    #[test]
    fn streaming_fingerprint_equals_batch_fingerprint() {
        let dir = std::env::temp_dir().join("rrk_source_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fp.bin");
        // 跨块边界（1 MiB 块）测试：确保分块拼接与整包一致
        let data: Vec<u8> = (0..(3 * 1024 * 1024u32)).map(|i| (i % 97) as u8).collect();
        std::fs::write(&path, &data).unwrap();

        let batch = rrk_model::fnv1a64_update(
            rrk_model::fnv1a64_update(rrk_model::FNV_INIT, &(data.len() as u64).to_le_bytes()),
            &data,
        );
        let fs = FileSource::open(&path).unwrap();
        assert_eq!(fingerprint_of(&fs).unwrap(), batch, "流式指纹须与整包一致");
        assert_eq!(fingerprint_of(&mem(&data)).unwrap(), batch);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn fingerprint_changes_with_any_byte() {
        let a: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let mut b = a.clone();
        b[777] ^= 0x01;
        assert_ne!(fingerprint_of(&mem(&a)).unwrap(), fingerprint_of(&mem(&b)).unwrap());
    }

    #[test]
    fn source_reader_streams_through_source() {
        let data: Vec<u8> = (0..10000u32).map(|i| (i % 251) as u8).collect();
        let src: Arc<dyn ByteSource> = Arc::new(mem(&data));
        let mut r = SourceReader::new(src).with_chunk(97); // 故意用非整除块长
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(out, data, "顺序流式读须还原全部字节");
    }
}
