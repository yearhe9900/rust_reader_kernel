//! rrk-model：统一书籍数据模型（方案 §2）。
//!
//! 三层模型：结构层（`BookMeta`/`TocNode`/`ChapterEntry`）→ 内容层（`Block`/`Inline`/`ChapterContent`）
//! → 版式层（`LayoutPage`，P1 排版内核产出，本 crate 预留位置）。
//! 解析层只产出前两层；渲染层只消费内容层、产出版式层；二者之间只有本 crate 一个契约。
//!
//! 数据面硬纪律（方案 §2.2）：
//! - 零拷贝：正文持 `Arc<str>`，`Inline::Text` 只存 `(offset, len)` 切片引用；
//! - 偏移语义统一：全链路 UTF-8 **字节偏移**（`SemanticPos.char_offset` 与 `TextRange` 同坐标系）。

use std::fmt;
use std::sync::Arc;

pub mod layout;
pub use layout::{
    decode_pages, encode_pages, style_hash, viewport_hash, Align, LayoutPage, LineEntry,
    PlacedImage, RenderResult, StyleSpec, TextRun, Viewport,
};

// ---------------------------------------------------------------------------
// 标识与基础类型
// ---------------------------------------------------------------------------

/// 章节 ID（书内稳定序号，按阅读顺序 0..n）。
pub type ChapterId = u32;

/// 格式标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FormatId {
    Epub,
    Txt,
    Mobi,
    Azw3,
    Pdf,
    Cbz,
}

impl fmt::Display for FormatId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            FormatId::Epub => "EPUB",
            FormatId::Txt => "TXT",
            FormatId::Mobi => "MOBI",
            FormatId::Azw3 => "AZW3",
            FormatId::Pdf => "PDF",
            FormatId::Cbz => "CBZ",
        };
        f.write_str(s)
    }
}

/// 资源引用（图片/字体等），正文与资源解耦，按需加载。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResourceKey(pub String);

/// 二进制资源。
#[derive(Debug, Clone)]
pub struct Resource {
    pub key: ResourceKey,
    /// MIME 类型，如 `image/jpeg`。
    pub mime: String,
    pub data: Arc<[u8]>,
}

/// 内容哈希（缓存 key 三要素之「内容」项）。
pub type ContentHash = u64;

// ---------------------------------------------------------------------------
// 结构层
// ---------------------------------------------------------------------------

/// 书目元数据。
#[derive(Debug, Clone)]
pub struct BookMeta {
    pub title: String,
    pub authors: Vec<String>,
    pub language: Option<String>,
    /// 简介（EPUB `dc:description` / MOBI EXTH 103·104）；取不到为 `None`（兜底文案属宿主职责）。
    pub description: Option<String>,
    pub format: FormatId,
    pub content_hash: ContentHash,
    pub cover: Option<ResourceKey>,
}

/// 目录节点（树形；`anchor` 为章内字节偏移锚点）。
#[derive(Debug, Clone)]
pub struct TocNode {
    pub title: String,
    pub chapter: ChapterId,
    pub anchor: Option<u32>,
    pub children: Vec<TocNode>,
}

/// 章节种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChapterKind {
    /// 重排类文本章。
    Text,
    /// 图片章（漫画/图册）。
    ImageGallery,
}

/// 章索引项：标题 + 稳定定位串 + 字节范围 + 字数权重 + 锚点。
#[derive(Debug, Clone)]
pub struct ChapterEntry {
    pub id: ChapterId,
    pub title: Option<String>,
    /// 稳定定位串（宿主进度/同步的章节身份键，同一本书内恒定、重开不变）。
    ///
    /// 口径按格式：EPUB = 包内路径（如 `OEBPS/text/ch1.xhtml`）；TXT =
    /// `txt:<起始字符偏移>`（与宿主历史约定一致，**字符**偏移而非字节）；
    /// MOBI/AZW3 = `mobi:<章序号>`；CBZ = 页（图片条目）名；PDF = `pdf:<章序号>`。
    pub href: String,
    /// 源内字节范围（TXT = 文件内偏移；EPUB = 条目内范围 `(0, len)`）。
    pub byte_range: (u64, u64),
    /// 权重：估算字数（进度与预排预算用）。
    pub char_len: u32,
    pub kind: ChapterKind,
    /// 逻辑块分片号（巨章按方案 §3.4 切块时 > 0，普通章为 0）。
    pub part: u32,
}

/// 全书章索引。
pub type ChapterIndex = Vec<ChapterEntry>;

// ---------------------------------------------------------------------------
// 内容层
// ---------------------------------------------------------------------------

/// UTF-8 字节区间（引用 `ChapterContent.text` 切片，不复制字符串）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextRange {
    pub start: u32,
    pub end: u32,
}

impl TextRange {
    pub fn new(start: u32, end: u32) -> Self {
        debug_assert!(start <= end);
        Self { start, end }
    }

    /// 从正文取切片。
    pub fn slice<'a>(&self, text: &'a str) -> &'a str {
        &text[self.start as usize..self.end as usize]
    }
}

/// 行内样式标志（轻量：加粗/斜体；复杂 CSS 最小集在解析层规整）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StyleFlags(pub u8);

impl StyleFlags {
    pub const BOLD: u8 = 1;
    pub const ITALIC: u8 = 2;

    pub fn is_bold(self) -> bool {
        self.0 & Self::BOLD != 0
    }

    pub fn is_italic(self) -> bool {
        self.0 & Self::ITALIC != 0
    }

    pub fn union(self, other: StyleFlags) -> StyleFlags {
        StyleFlags(self.0 | other.0)
    }
}

/// 行内元素。
#[derive(Debug, Clone, PartialEq)]
pub enum Inline {
    /// 文本切片引用（零拷贝）。
    Text { range: TextRange, style: StyleFlags },
    Image { key: ResourceKey },
}

/// 块级元素（所有格式的统一出口；排版层不感知格式）。
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Heading {
        level: u8,
        inlines: Vec<Inline>,
    },
    Paragraph {
        inlines: Vec<Inline>,
        /// 首行缩进（CJK 习惯）。
        indent: bool,
    },
    Image {
        key: ResourceKey,
        alt: Option<String>,
        size_hint: Option<(u32, u32)>,
    },
    Quote {
        blocks: Vec<Block>,
    },
    Divider,
}

/// 章内容：整章 UTF-8 正文（零拷贝持源）+ 块列表。
#[derive(Debug, Clone)]
pub struct ChapterContent {
    pub id: ChapterId,
    /// 章正文（清洗后的 UTF-8 文本），`Inline::Text` 的偏移以此为坐标系。
    pub text: Arc<str>,
    pub blocks: Vec<Block>,
}

impl ChapterContent {
    /// 取行内文本切片。
    pub fn slice(&self, range: TextRange) -> &str {
        range.slice(&self.text)
    }
}

// ---------------------------------------------------------------------------
// 进度坐标
// ---------------------------------------------------------------------------

/// 进度真值：(章, UTF-8 字节偏移)。与视口/字号无关（方案 §2.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemanticPos {
    pub chapter: ChapterId,
    pub char_offset: u32,
}

// ---------------------------------------------------------------------------
// 固定版式（PDF/CBZ，方案 §2.4 页级变体）
// ---------------------------------------------------------------------------

/// 页种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageKind {
    /// 矢量页（PDF）。
    Vector,
    /// 图片页（CBZ）。
    Image,
}

/// 页索引项：O(结构) 可得，不解压。
#[derive(Debug, Clone)]
pub struct PageEntry {
    pub index: u32,
    /// 页尺寸（pt）。
    pub size: (f32, f32),
    pub kind: PageKind,
}

/// 页内容（L1 口径：图片页给资源，矢量页给抽取文本）。
#[derive(Debug, Clone)]
pub enum PagedContent {
    /// 图片页。
    Image(Resource),
    /// 文本页（L1 抽取结果；L2 光栅位图在渲染层落）。
    Text(ChapterContent),
}

/// 固定版式进度真值（页码；页内锚点后置）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PagedPos {
    pub page: u32,
}

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

/// 错误分类（FFI 层映射为错误码 → Dart `RrkException`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RrkErrorKind {
    /// IO 失败。
    Io,
    /// 格式不支持（含 DRM、CBR 等明确拒绝）。
    UnsupportedFormat,
    /// 数据损坏（结构解析失败）。
    CorruptData,
    /// 参数非法。
    InvalidParam,
    /// 任务被取消。
    Cancelled,
    /// 资源超限（内存/大小上限）。
    ResourceLimit,
    /// 内部错误（panic 边界兜底）。
    Internal,
}

impl RrkErrorKind {
    /// 稳定错误码（FFI 边界用，只增不改）。
    pub fn code(self) -> u32 {
        match self {
            RrkErrorKind::Io => 1001,
            RrkErrorKind::UnsupportedFormat => 1002,
            RrkErrorKind::CorruptData => 1003,
            RrkErrorKind::InvalidParam => 1004,
            RrkErrorKind::Cancelled => 1005,
            RrkErrorKind::ResourceLimit => 1006,
            RrkErrorKind::Internal => 1999,
        }
    }
}

/// 统一错误。
#[derive(Debug, Clone)]
pub struct RrkError {
    pub kind: RrkErrorKind,
    pub message: String,
    pub context: Option<String>,
}

impl RrkError {
    pub fn new(kind: RrkErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into(), context: None }
    }

    pub fn with_context(mut self, ctx: impl Into<String>) -> Self {
        self.context = Some(ctx.into());
        self
    }

    pub fn io(message: impl Into<String>) -> Self {
        Self::new(RrkErrorKind::Io, message)
    }

    pub fn corrupt(message: impl Into<String>) -> Self {
        Self::new(RrkErrorKind::CorruptData, message)
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(RrkErrorKind::UnsupportedFormat, message)
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(RrkErrorKind::InvalidParam, message)
    }
}

impl fmt::Display for RrkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.kind.code(), self.message)?;
        if let Some(ctx) = &self.context {
            write!(f, " ({ctx})")?;
        }
        Ok(())
    }
}

impl std::error::Error for RrkError {}

impl From<std::io::Error> for RrkError {
    fn from(e: std::io::Error) -> Self {
        Self::io(e.to_string())
    }
}

pub type RrkResult<T> = Result<T, RrkError>;

// ---------------------------------------------------------------------------
// 内容哈希（FNV-1a，缓存 key 用；非密码学用途）
// ---------------------------------------------------------------------------

/// FNV-1a 64 位哈希，供 `content_hash` 与指纹使用。
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_range_slice_is_zero_copy() {
        let text: Arc<str> = "hello 世界".into();
        let range = TextRange::new(6, 12); // "世界" 的 UTF-8 字节范围
        assert_eq!(range.slice(&text), "世界");
    }

    #[test]
    fn error_codes_are_stable() {
        assert_eq!(RrkErrorKind::UnsupportedFormat.code(), 1002);
    }

    #[test]
    fn fnv1a_known_values() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_ne!(fnv1a64(b"a"), fnv1a64(b"b"));
    }
}
