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

/// 书类型：统一句柄的分流依据（宿主据此选重排阅读 / 页级浏览）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BookKind {
    /// 重排文本（TXT / EPUB / MOBI / AZW3）。
    Text,
    /// 页级图片图库（CBZ）。
    ImageGallery,
    /// 固定版式页（PDF）。
    Paged,
}

impl BookKind {
    pub fn of(format: FormatId) -> Self {
        match format {
            FormatId::Cbz => BookKind::ImageGallery,
            FormatId::Pdf => BookKind::Paged,
            _ => BookKind::Text,
        }
    }

    /// FFI 整数编码：0=Text 1=ImageGallery 2=Paged。
    pub fn code(self) -> u32 {
        match self {
            BookKind::Text => 0,
            BookKind::ImageGallery => 1,
            BookKind::Paged => 2,
        }
    }
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
#[derive(Debug, Clone, PartialEq)]
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
#[derive(Debug, Clone, PartialEq)]
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

/// 正文内链区间：清洗后章文本的字节区间 → 目标 href。
///
/// 出口口径（方案 #1 轻方案 A「链接区间旁路」）：不改 `Inline` 枚举、排版内核零改动，
/// 宿主按区间把可点标记织回段落文本。
#[derive(Debug, Clone, PartialEq)]
pub struct LinkRange {
    /// 清洗后章文本中的字节区间（与 `Inline::Text` 同坐标系）。
    pub range: TextRange,
    /// 目标 href（EPUB 为包内绝对路径或 `#frag`；非 EPUB 为原文 href）。
    pub href: String,
}

/// 章内容：整章 UTF-8 正文（零拷贝持源）+ 块列表 + 内链区间。
#[derive(Debug, Clone)]
pub struct ChapterContent {
    pub id: ChapterId,
    /// 章正文（清洗后的 UTF-8 文本），`Inline::Text` 的偏移以此为坐标系。
    pub text: Arc<str>,
    pub blocks: Vec<Block>,
    /// 正文内链区间（无链接的书为空）。
    pub link_ranges: Vec<LinkRange>,
}

impl ChapterContent {
    /// 取行内文本切片。
    pub fn slice(&self, range: TextRange) -> &str {
        range.slice(&self.text)
    }
}

/// 行内图片内嵌标记（用 U+FFFC 包裹资源 key）：`<U+FFFC><key><U+FFFC>`。
///
/// 宿主按此标记把段落文本织回「文字 / 图片」片段；U+FFFC（OBJECT REPLACEMENT
/// CHARACTER）不会出现在正常正文中，故无需转义。
pub const INLINE_IMAGE_MARKER: char = '\u{FFFC}';

/// 扁平块：块树降维后的宿主可直接消费形态（引用块摊平、分割线丢弃）。
#[derive(Debug, Clone, PartialEq)]
pub enum PlainBlock {
    Paragraph { text: String, indent: bool },
    Heading { level: u8, text: String },
    Image { key: ResourceKey, alt: Option<String> },
}

impl PlainBlock {
    /// 类型标签（与 FFI `blocks_json` 的 `type` 一致：`p` / `h` / `img`）。
    pub fn kind(&self) -> &'static str {
        match self {
            PlainBlock::Paragraph { .. } => "p",
            PlainBlock::Heading { .. } => "h",
            PlainBlock::Image { .. } => "img",
        }
    }
}

/// 行内序列 → 纯文本；行内图片内嵌为 `\u{FFFC}<key>\u{FFFC}`。
pub fn inline_plain(text: &str, inlines: &[Inline]) -> String {
    let mut out = String::new();
    for i in inlines {
        match i {
            Inline::Text { range, .. } => out.push_str(range.slice(text)),
            Inline::Image { key } => {
                out.push(INLINE_IMAGE_MARKER);
                out.push_str(&key.0);
                out.push(INLINE_IMAGE_MARKER);
            }
        }
    }
    out
}

/// 扁平块内的一段：plain 文本区间 + 其来源。
///
/// 「宿主读到的段落文本」与「章正文」是两个坐标系：段落文本是行内序列的拼接
/// （含行内图片标记）。检索命中落在段落文本上，要落到章正文偏移（锚点/内链/
/// 进度同一坐标系）就必须有这张映射表。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlainSpan {
    /// 在所属扁平块 plain 文本中的字节区间。
    pub plain: TextRange,
    /// 在 `ChapterContent.text` 中的字节区间；行内图片标记为 `None`（无源文本）。
    pub src: Option<TextRange>,
}

/// 扁平块 + 来源映射（`flatten_blocks_spans` 的产出）。
#[derive(Debug, Clone, PartialEq)]
pub struct PlainTextBlock {
    pub block: PlainBlock,
    pub spans: Vec<PlainSpan>,
}

impl PlainTextBlock {
    /// 块内 plain 文本。
    pub fn text(&self) -> &str {
        match &self.block {
            PlainBlock::Paragraph { text, .. } => text,
            PlainBlock::Heading { text, .. } => text,
            PlainBlock::Image { .. } => "",
        }
    }

    /// 块内 plain 偏移 → 章正文偏移。
    ///
    /// 落在行内图片标记上（无源文本）时返回 `None`：调用方应退到该块内最近的
    /// 有源片段起点，或直接用块起点。
    pub fn to_source(&self, plain_offset: u32) -> Option<u32> {
        for s in &self.spans {
            if plain_offset >= s.plain.start && plain_offset < s.plain.end {
                return s.src.map(|src| src.start + (plain_offset - s.plain.start));
            }
        }
        // 端点（= 块尾）归到最后一段的末尾
        if plain_offset == self.text().len() as u32 {
            return self.spans.last().and_then(|s| s.src.map(|src| src.end));
        }
        None
    }

    /// 命中位置的可用源偏移：优先精确映射；退化为块内第一个有源片段起点。
    pub fn source_anchor(&self, plain_offset: u32) -> Option<u32> {
        self.to_source(plain_offset).or_else(|| {
            self.spans
                .iter()
                .filter(|s| s.plain.start <= plain_offset)
                .filter_map(|s| s.src.map(|src| src.end))
                .last()
                .or_else(|| self.spans.iter().find_map(|s| s.src.map(|src| src.start)))
        })
    }
}

/// 块树 → 扁平块流（带来源映射）：**引用块递归摊平、分割线丢弃、块级图片单列**。
///
/// 这是 [`flatten_blocks`] 的**可追溯版本**：文本内容与分块结果完全一致，
/// 额外给出「段落文本偏移 ↔ 章正文偏移」映射（检索命中回映用）。
pub fn flatten_blocks_spans(text: &str, blocks: &[Block]) -> Vec<PlainTextBlock> {
    flatten_impl(text, blocks, true)
}

/// `want_spans=false`：只产纯文本块，**不分配每块的 `Vec<PlainSpan>`**（FFI
/// `blocks_json` 等不需要来源映射的出口用——旧实现先建 spans 再整体丢弃）。
fn flatten_impl(text: &str, blocks: &[Block], want_spans: bool) -> Vec<PlainTextBlock> {
    fn walk(text: &str, blocks: &[Block], want_spans: bool, out: &mut Vec<PlainTextBlock>) {
        for b in blocks {
            match b {
                Block::Paragraph { inlines, indent } => {
                    let mut plain = String::new();
                    let spans = spans_of(text, inlines, &mut plain, want_spans);
                    out.push(PlainTextBlock {
                        block: PlainBlock::Paragraph { text: plain, indent: *indent },
                        spans,
                    });
                }
                Block::Heading { level, inlines } => {
                    let mut plain = String::new();
                    let spans = spans_of(text, inlines, &mut plain, want_spans);
                    out.push(PlainTextBlock {
                        block: PlainBlock::Heading { level: *level, text: plain },
                        spans,
                    });
                }
                Block::Image { key, alt, .. } => out.push(PlainTextBlock {
                    block: PlainBlock::Image { key: key.clone(), alt: alt.clone() },
                    spans: Vec::new(),
                }),
                Block::Quote { blocks } => walk(text, blocks, want_spans, out),
                Block::Divider => {}
            }
        }
    }

    fn spans_of(text: &str, inlines: &[Inline], plain: &mut String, want_spans: bool) -> Vec<PlainSpan> {
        if !want_spans {
            // 纯文本路径：只拼 plain（含图片标记），不建 spans
            for i in inlines {
                match i {
                    Inline::Text { range, .. } => plain.push_str(range.slice(text)),
                    Inline::Image { key } => {
                        plain.push(INLINE_IMAGE_MARKER);
                        plain.push_str(&key.0);
                        plain.push(INLINE_IMAGE_MARKER);
                    }
                }
            }
            return Vec::new();
        }
        let mut spans = Vec::with_capacity(inlines.len());
        for i in inlines {
            let start = plain.len() as u32;
            match i {
                Inline::Text { range, .. } => {
                    plain.push_str(range.slice(text));
                    spans.push(PlainSpan { plain: TextRange::new(start, plain.len() as u32), src: Some(*range) });
                }
                Inline::Image { key } => {
                    plain.push(INLINE_IMAGE_MARKER);
                    plain.push_str(&key.0);
                    plain.push(INLINE_IMAGE_MARKER);
                    spans.push(PlainSpan { plain: TextRange::new(start, plain.len() as u32), src: None });
                }
            }
        }
        spans
    }

    let mut out = Vec::with_capacity(blocks.len());
    walk(text, blocks, want_spans, &mut out);
    out
}

/// 块树 → 扁平块流：**引用块递归摊平、分割线丢弃、块级图片单列**。
///
/// 这是「内容块如何降维成宿主段落」的唯一口径（方案 #4 轻方案）：
/// FFI 出口与 FRB/其它绑定的宿主桥都应消费本函数，不再各写一份映射。
pub fn flatten_blocks(text: &str, blocks: &[Block]) -> Vec<PlainBlock> {
    // 不建 spans（旧实现先分配每块 spans 再整体丢弃）
    flatten_impl(text, blocks, false).into_iter().map(|b| b.block).collect()
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
    fnv1a64_update(FNV_INIT, bytes)
}

/// FNV 初始值（增量哈希起点）。
pub const FNV_INIT: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a 增量入口：多段输入免拼接缓冲（结果与 [`fnv1a64`] 对整段拼接同值）。
pub fn fnv1a64_update(mut hash: u64, bytes: &[u8]) -> u64 {
    const PRIME: u64 = 0x0000_0100_0000_01b3;
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

    /// #9：扁平块的「段落文本偏移 ↔ 章正文偏移」映射——
    /// 检索命中落在段落文本上，必须能回映到章正文坐标系（锚点/内链/进度同坐标）。
    #[test]
    fn plain_spans_map_back_to_source() {
        let text: Arc<str> = Arc::from("标题正文尾巴"); // 标0 题3 正6 文9 尾12 巴15
        let blocks = vec![Block::Paragraph {
            inlines: vec![
                Inline::Text { range: TextRange::new(0, 6), style: StyleFlags(0) },
                Inline::Image { key: ResourceKey("img1.png".into()) },
                Inline::Text { range: TextRange::new(6, 18), style: StyleFlags(0) },
            ],
            indent: true,
        }];
        let traced = flatten_blocks_spans(&text, &blocks);
        assert_eq!(traced.len(), 1);
        let b = &traced[0];
        assert_eq!(b.text(), "标题\u{FFFC}img1.png\u{FFFC}正文尾巴");
        assert_eq!(b.spans.len(), 3);
        // 纯文本段：1:1 映射
        assert_eq!(b.to_source(0), Some(0));
        assert_eq!(b.to_source(3), Some(3));
        // 行内图片标记：无源文本（U+FFFC 是 3 字节，故 span1 = 3+8+3 = 14）
        assert_eq!(b.spans[1].plain, TextRange::new(6, 20));
        assert_eq!(b.to_source(6), None);
        // 图片之后的文本段：源偏移从 6 起
        assert_eq!(b.to_source(20), Some(6));
        assert_eq!(b.to_source(21), Some(7));
        // 退化锚点：图片标记位置退到前一段末尾
        assert_eq!(b.source_anchor(6), Some(6));
        // 与 flatten_blocks 完全同源（唯一降维口径不被破坏）
        assert_eq!(flatten_blocks(&text, &blocks), vec![b.block.clone()]);
    }

    /// #4：块树降维为唯一口径——引用摊平、分割线丢弃、行内图片加标记。
    #[test]
    fn flatten_blocks_contract() {
        use std::sync::Arc;
        let text: Arc<str> = Arc::from("标题正文尾巴");
        let para_inlines = vec![
            Inline::Text { range: TextRange::new(0, 12), style: StyleFlags(0) },
            Inline::Image { key: ResourceKey("img1.png".into()) },
            Inline::Text { range: TextRange::new(12, 18), style: StyleFlags(0) },
        ];
        let blocks = vec![
            Block::Heading {
                level: 2,
                inlines: vec![Inline::Text { range: TextRange::new(0, 6), style: StyleFlags(0) }],
            },
            Block::Paragraph { inlines: para_inlines, indent: true },
            Block::Quote {
                blocks: vec![Block::Paragraph {
                    inlines: vec![Inline::Text {
                        range: TextRange::new(0, 6),
                        style: StyleFlags(0),
                    }],
                    indent: false,
                }],
            },
            Block::Divider,
            Block::Image {
                key: ResourceKey("cover.png".into()),
                alt: Some("封面".into()),
                size_hint: None,
            },
        ];
        let flat = flatten_blocks(&text, &blocks);
        // 引用摊平 + 分割线丢弃：h / p / p / img
        assert_eq!(
            flat.iter().map(|b| b.kind()).collect::<Vec<_>>(),
            vec!["h", "p", "p", "img"]
        );
        match &flat[1] {
            PlainBlock::Paragraph { text: t, indent } => {
                assert!(*indent);
                assert_eq!(t, "标题正文\u{FFFC}img1.png\u{FFFC}尾巴");
            }
            _ => panic!("第二块应为段落"),
        }
    }
}
