# Rust 阅读内核总体方案：解析 · 渲染 · Flutter 封装

> 定位：把《设计-极速开书全链路》§2.4「引擎层（Rust 内核）」具体化为一个可独立演进、可被 Flutter 直接依赖的 Cargo workspace——`rust_reader_kernel`。
>
> 本文件回答六件事：①解析/渲染职责怎么分、统一数据模型长什么样；②格式适配接口怎么扩展、六种格式（TXT/EPUB/MOBI/AZW3/PDF/CBZ）的**全自研**实现；③Flutter 怎么调（API/异步/错误/资源释放）；④新老 Flutter 项目怎么低成本接入；⑤性能与内存怎么保；⑥模块划分、公开 API、封装接口、接入示例的交付形态。
>
> 术语与三份源文档逐句对齐（`SemanticPos`、`ContentChunk`、`LayoutPage`、缓存 key 三要素等）。可行性讨论见 §8，分档推进与退出条件见 §9。

---

## 0. 定位、范围与设计原则

### 0.1 内核做什么 / 不做什么

| 做（内核职责） | 不做（留给宿主/上层） |
| --- | --- |
| 格式解析、解压、编码检测、分章、内容清洗 | 书架、书目数据库、进度持久化策略（内核只存/还原 `SemanticPos` 状态） |
| 统一书籍数据模型（结构/内容/版式三层） | UI、手势、动画、合成（Flutter 呈现面） |
| 排版测量、断行、分页、渲染输出（几何/位图） | 网络书源、下载、DRM 解密 |
| 缓存（结构/内容/布局，可落盘） | 阅读设置 UI、统计上报 |
| FFI 句柄、异步任务、错误与生命周期 | 平台插件注册之外的宿主工程逻辑 |

一句话：**内核 = 计算面**。输入是书文件（路径或字节）+ 样式/视口参数，输出是结构、内容、版式与渲染产物；一切 UI 呈现留在 Flutter（《统一方案》§5 口径不变）。

**对接关系**：本项目是契约的唯一来源，MReader 等 Flutter 宿主**按本项目的 API 与数据模型对接**（对接细节见 §6.2），而不是本项目迁就宿主既有实现；宿主原有的解析/排版/缓存能力按迁移路径收敛到内核。

### 0.2 术语对齐（与三份源文档）

| 本方案 | 源文档出处 | 含义 |
| --- | --- | --- |
| `ChapterEntry` / `ChapterIndex` | 全链路 §2.3 `ChapterIndexEntry` | 章索引：标题 + 字节范围 + 字数权重 + 锚点 |
| `ContentChunk` | 全链路 §2.3 / §3.4 | 单章（或逻辑块）压缩字节块，随机读 O(1) |
| `SemanticPos(chapter, char_offset)` | 全路 §2.3 / §4.5 | 进度真值，UTF-8 字节偏移 |
| `LayoutPage` | 全链路 §2.3 / §5.1 | 不可变页对象（行 → span → 绘制指令） |
| 缓存 key = `contentHash × styleHash × viewportHash` | 全链路 §2.3 / §5.2 | 三要素失效口径 |
| 逻辑块 | 全链路 §3.3 / §5.1 | 超长章切块，独立排版/缓存 |
| 保护窗口 LRU | 全链路 §5.2 / 渲染层 §4.3 | 窗口章永不淘汰的页缓存纪律 |
| rkyv 落盘 / mmap | 全链路 §2.4 / §5.2 | 布局产物可序列化 = 冷启动免排版 |

### 0.3 设计原则（沿用五条 + 新增四条）

沿用《全链路》§0.2：关键路径零全量、UI 零重活、三级交付、预测代替等待、一切可缓存可增量可中断。内核层新增：

1. **内核无 UI 依赖**：任何 crate 不依赖 Flutter/Skia；渲染输出是几何数据或 RGBA 位图，绘制由宿主决定。
2. **一切产物可序列化**：数据模型、布局结果全部 `rkyv` 友好；跨会话落盘 mmap 即用，这是内核相对 SkParagraph 的最持久收益。
3. **一次调用 = 一个任务**：FFI 纪律——绝不逐行/逐字形跨界；批进批出，大块数据走零拷贝缓冲。
4. **全自研、代码可控**（用户拍板，硬性红线）：格式解析/解压算法/排版/渲染管线**全部自研**——MOBI 的 PalmDOC LZ77 与 HUFF/CDIC 解压、PDF 的解析与光栅化、分章/清洗/缓存一律自己实现，**禁止 pdfium / mupdf / unrar / 闭源 SDK / 预编译黑盒二进制**。第三方依赖仅限「纯 Rust、开源可审计、可 fork、许可宽松」的基础件（白名单见 §3.2），关键路径出现不可控依赖时，用自研替代而非绑定。

---

## 1. 模块划分（Cargo workspace）

```
rust_reader_kernel/
├── crates/
│   ├── rrk-model/      # 统一书籍数据模型（零 IO、零格式知识，全量 rkyv）
│   ├── rrk-format/     # FormatAdapter / BookSource trait + 嗅探注册表
│   ├── rrk-epub/       # EPUB 适配器（完整实现）
│   ├── rrk-txt/        # TXT 适配器（完整实现：编码检测 + 流式分章）
│   ├── rrk-mobi/       # MOBI/AZW3 适配器（完整实现：自研 PalmDOC/HUFF/CDIC 解压）
│   ├── rrk-pdf/        # PDF 适配器（完整实现：自研解析 + 分级光栅化）
│   ├── rrk-cbz/        # CBZ 适配器（完整实现：页级图库）
│   ├── rrk-layout/     # 排版引擎：测量/断行/BiDi/分页 → LayoutPage（纯函数）
│   ├── rrk-render/     # RenderScene / 绘制指令 / 栅格化（可选后端）
│   ├── rrk-cache/      # 三级缓存：内容块 / 结构 / 布局（rkyv + mmap）
│   ├── rrk-kernel/     # 会话编排：BookHandle、优先级任务池、取消、事件
│   └── rrk-ffi/        # C ABI + flutter_rust_bridge 绑定、句柄/错误/生命周期
├── bindings/dart/      # 生成的 Dart 绑定 → 发布包 rrk_reader
├── examples/           # 纯 Rust 基准与 CLI 校验器
└── xtask/              # 构建矩阵、产物打包、基准数据生成
```

依赖方向单向，禁止反向引用：

```
rrk-ffi ──▶ rrk-kernel ──▶ {rrk-format, rrk-layout, rrk-render, rrk-cache}
                                │                │
                                ▼                ▼
              rrk-epub / rrk-txt / rrk-mobi   rrk-model ◀──（所有 crate 只依赖它）
              / rrk-pdf / rrk-cbz
```

| crate | 职责 | 关键依赖（拟） | 备注 |
| --- | --- | --- | --- |
| `rrk-model` | `BookMeta`/`TocNode`/`Block`/`LayoutPage` 等 | `bytes`、`rkyv`、`smallvec` | 无 IO；schema 号随版本演进 |
| `rrk-format` | trait 定义、格式嗅探、注册表 | `rrk-model` | 新格式在此实现 trait 即接入 |
| `rrk-epub` | zip 随机读 + OPF/NCX/nav + XHTML→Block | `zip`/`rc-zip`、`html5ever` 或 `quick-xml` | 见 §3.3 |
| `rrk-txt` | 编码检测 + 流式分章 + 逻辑块 | `chardetng`、`encoding_rs`、`memmap2` | 见 §3.4 |
| `rrk-mobi` | PalmDB/EXTH 解析 + **自研** PalmDOC LZ77、HUFF/CDIC 解压 + KF8→Block | `chardetng`、`encoding_rs`（解压算法全自研） | 见 §3.5；AZW3 = KF8 主体 |
| `rrk-pdf` | **自研** PDF 对象/内容流解析 + 文本抽取 + 分级光栅化 | `flate2`(rust_backend)、`tiny-skia`（仅光栅原语） | 见 §3.6；禁 pdfium/mupdf |
| `rrk-cbz` | zip 图片条目 → 页索引 + 图片管线 | `zip`/`rc-zip`、`image` | 见 §3.7；CBR 不支持 |
| `rrk-layout` | 整形/断行/BiDi/分页 | `parley`（或 `cosmic-text`）、`unicode-linebreak` | 见 §4.2 选型；纯 Rust 可 fork，逃生门=自研断行 |
| `rrk-render` | 绘制指令序列化 + 可选栅格化 | `tiny-skia`（可选 `vello`） | 栅格化是可选 feature |
| `rrk-cache` | 三级缓存、key 三要素、保护窗口 LRU | `rkyv`、`memmap2`、`deflate`/`ruzstd` | 损坏隔离重算（口径同 MRSC） |
| `rrk-kernel` | 会话、任务调度（rayon + 优先级 + 取消）、事件 | `rayon`、`crossbeam` | 一次 FFI = 一个任务的执行体 |
| `rrk-ffi` | C ABI、frb 绑定、panic 边界、句柄表 | `flutter_rust_bridge` | `catch_unwind`，panic 不跨界 |

**裁剪开关**（feature）：`layout`、`raster`、`epub`、`txt`、`mobi`、`pdf`、`cbz`、`mmap`。P0 阶段可只编 `epub + txt + mobi + mmap`（重排类全齐、无排版），体积与工期都小。

---

## 2. 统一书籍数据模型（解析与渲染的分界线）

**三层模型**：结构层（目录/章索引）→ 内容层（块/行内流）→ 版式层（LayoutPage）。解析层只产出前两层；渲染层只消费第二层、产出第三层。二者之间**只有 `rrk-model` 这一个契约**，互相不感知对方实现——这就是「解析与渲染职责分离」的落点。

### 2.1 结构层

```rust
pub struct BookMeta {
    pub title: String,
    pub authors: Vec<String>,
    pub language: Option<String>,
    pub format: FormatId,            // Epub | Txt | ...
    pub content_hash: ContentHash,   // 缓存 key 的「内容」项
    pub cover: Option<ResourceKey>,
}

pub struct TocNode {
    pub title: String,
    pub chapter: ChapterId,
    pub anchor: Option<u32>,         // 章内字节偏移锚点
    pub children: Vec<TocNode>,
}

pub struct ChapterEntry {
    pub id: ChapterId,
    pub title: Option<String>,
    pub byte_range: (u64, u64),      // 源内定位（TXT 直接偏移；EPUB = 条目内范围）
    pub char_len: u32,               // 权重（估算字数），供进度与预排预算
    pub kind: ChapterKind,           // Text | ImageGallery | ...
}
```

### 2.2 内容层（统一抽象：块 + 行内流）

```rust
pub struct ChapterContent {
    pub id: ChapterId,
    pub text: bytes::Bytes,          // UTF-8 正文（mmap 切片或解压块），零复制持源
    pub blocks: Vec<Block>,
}

pub enum Block {
    Heading { level: u8, inlines: Vec<Inline> },
    Paragraph { inlines: Vec<Inline>, indent: bool },
    Image { key: ResourceKey, alt: Option<String>, size_hint: Option<(u32, u32)> },
    Quote { blocks: Vec<Block> },
    Divider,
    // Table/Ruby 等复杂结构首期降级为 Paragraph 流，接口预留变体
}

pub enum Inline {
    Text { range: TextRange, style: StyleFlags },  // range = text 内偏移，不复制字符串
    Image { key: ResourceKey },
}

pub struct SemanticPos { pub chapter: ChapterId, pub char_offset: u32 }
```

要点：

- **零拷贝硬纪律**：正文一整块 `bytes::Bytes`（mmap 或解压块），`Inline::Text` 只存 `(offset, len)` 切片引用；任何层不复制字符串。
- **偏移语义统一**：全链路 UTF-8 字节偏移（`SemanticPos.char_offset` 与 `TextRange` 同一坐标系），杜绝 UTF-8⇄UTF-16 全书转换（全链路 §2.4 数据面口径）。
- **内容清洗在适配器内离线化**：所有格式输出的 `Block` 流已经是「可直接排版」的干净结构，排版层零清洗。

### 2.3 版式层

```rust
pub struct StyleSpec {                 // styleHash 输入：任何字段变化 → 布局缓存失效
    pub font: FontSpec,                // 字体族 / 内嵌字体 key / fallback 链
    pub font_size: f32, pub line_height: f32,
    pub para_indent: f32, pub align: Align,
    pub theme: ThemeSpec,              // 色板（不影响几何的部分可单独标记）
}

pub struct Viewport { pub width: f32, pub height: f32, pub dpr: f32 }

pub struct LayoutPage {
    pub index: u32,
    pub chapter: ChapterId,
    pub start: SemanticPos, pub end: SemanticPos,
    pub lines: LineTable,              // 紧凑四列（start/end/height/para），TypedData 直传
    pub runs: Vec<TextRun>,            // 字形 run：字体、字号、span range、基线、位置
    pub images: Vec<PlacedImage>,      // 图片占位矩形 + ResourceKey
    pub height: f32,
}

pub enum RenderResult {
    Geometry(LayoutPage),              // 模式 A：几何 → Flutter 自绘
    Bitmap { width: u32, height: u32, rgba: Vec<u8> },  // 模式 B：整页栅格化
}
```

`LayoutPage` 不可变、可整体 rkyv 归档；缓存 key = `contentHash × styleHash × viewportHash`（三要素，与《全链路》§5.2 完全一致）。行表四列紧凑化与《渲染层》§4.1「TypedData 紧凑化」是同一个决定——长章 5 万行不产生 5 万小对象。

### 2.4 固定版式变体（PDF/CBZ 用）

重排类（TXT/EPUB/MOBI/AZW3）走「章节 → 分页」；固定版式类（PDF/CBZ）没有重排概念，走**页级变体**，与重排类共用 `BookMeta`/`TocNode` 与渲染出口：

```rust
pub enum BookShape { Reflowable, FixedLayout }

pub struct PageEntry {
    pub index: u32,
    pub size: (f32, f32),            // 页尺寸（pt）
    pub kind: PageKind,              // Vector（PDF）| Image（CBZ）
}

/// 固定版式书源：页级随机读，进度 = 页码
pub trait PagedSource: Send {
    fn meta(&self) -> &BookMeta;
    fn toc(&self) -> Result<Toc, RrkError>;          // PDF outline；CBZ 通常为空
    fn pages(&self) -> Result<PageIndex, RrkError>;  // O(结构)：页数 + 尺寸表，不解压
    fn load_page(&self, index: u32) -> Result<PageContent, RrkError>;   // 矢量内容/图片引用
    fn render_page(&self, index: u32, dpr: f32) -> Result<RenderResult, RrkError>;
}

pub enum PagedPos { Page(u32) }      // 固定版式进度真值；页内锚点后置
```

- 统一出口：`enum AnyBookSource { Reflowable(Box<dyn BookSource>), Paged(Box<dyn PagedSource>) }`，上层 API（`renderPage`）对两类书一视同仁，宿主无格式特判；
- `PagedPos` 与 `SemanticPos` 同为进度真值，`ReaderState` 双轨存储；
- PDF 文本层抽取结果可同时喂重排管线（`Block` 流）——同一本书支持「原版页」与「重排阅读」双模式（§3.6）。

---

## 3. 格式适配接口与六种自研实现

### 3.1 接口设计（可扩展）

```rust
/// 一个格式 = 一个适配器。注册表按嗅探置信度选择。
pub trait FormatAdapter: Send + Sync {
    fn id(&self) -> FormatId;
    /// 读头部字节 + 文件名返回置信度（0 = 不认）
    fn sniff(&self, head: &[u8], name: &str) -> u32;
    /// 打开为统一书源；src 可为路径（mmap）或内存字节
    fn open(&self, src: ContentSource) -> Result<Box<dyn BookSource>, RrkError>;
}

/// 统一书源：所有格式的输出契约（吸收 foliate-js sections.load/unload/size 思想）
pub trait BookSource: Send {
    fn meta(&self) -> &BookMeta;
    fn toc(&self) -> Result<Toc, RrkError>;
    fn chapters(&self) -> Result<ChapterIndex, RrkError>;      // O(结构) 可得
    fn load_chapter(&self, id: ChapterId) -> Result<ChapterContent, RrkError>;
    fn unload_chapter(&self, id: ChapterId);                   // 释放解压缓存
    fn load_resource(&self, key: &ResourceKey) -> Result<Resource, RrkError>;
    fn cover(&self) -> Result<Option<Resource>, RrkError>;
    fn estimate_weights(&self) -> Result<Vec<(ChapterId, u32)>, RrkError>; // 零解压权重
}
```

扩展方式：**静态注册**（`FormatRegistry::register()`，内建格式 + 第三方 crate 实现 trait 后在链接期注册）。不做 dylib 动态插件——移动端签名/加载限制下收益为负，而静态注册对「新格式接入」的成本已经只是加一个 crate。

### 3.2 所有实现的共同要求（含自研边界与依赖白名单）

- **建索引不搬内容**（全链路 §3.1）：`chapters()` 只读结构，不解压正文；
- **权重零解压**：`estimate_weights` 用压缩后大小/条目尺寸估算（《全链路》§3.1「建索引不搬内容」口径）；
- **错误可降级**：结构损坏 → 返回 `CorruptData` 并可退「纯文本流式阅读」模式（全链路 §10.1）；
- **全自研红线**（§0.3-4）：格式解析、解压算法（PalmDOC LZ77、HUFF/CDIC）、PDF 解析与光栅化、分章/清洗/缓存全部为本仓库自研代码；**禁止** pdfium、mupdf、unrar、WebKit、闭源 SDK、预编译黑盒二进制及其绑定；
- **依赖白名单**：第三方 crate 仅限「纯 Rust、开源、可审计、可 fork、许可宽松（MIT/Apache-2.0/BSD）」的基础件；C 绑定原则禁用，唯一例外是构建期从源码编译并登记审计的小型压缩原语（miniz_oxide 类）。当前白名单：`bytes`/`rkyv`/`smallvec`/`memmap2`/`flate2(rust_backend)`/`ruzstd`/`chardetng`/`encoding_rs`/`zip|rc-zip`（审计不符即自研 zip 读取，中央目录解析本就不难）/`html5ever|quick-xml`/`image`/`fast_image_resize`/`tiny-skia`/`rayon`/`crossbeam`/`unicode-linebreak`/`parley|cosmic-text`（纯 Rust 可 fork；不可控即走 §4.2 逃生门收回自研）/`swash`/`fontique`/`zeno`/`peniko`（parley 的 shaping/字形/字体库传递依赖，均纯 Rust；CFF 轮廓经自研 sfnt 封装喂 swash 光栅）。白名单变更需评审并登记。
  **2026-10-21 启用确认**：实际引入 = `encoding_rs`/`chardetng`/`flate2`/`tiny-skia`/`swash`/`image`/`parley`（+传递 `fontique`/`zeno`/`peniko`）；`rkyv` 未用（布局缓存 payload 用自研 RLAY 编解码）；`cosmic-text` 仅对照基准（dev 依赖）。

### 3.3 EPUB 完整实现（`rrk-epub`）

1. **嗅探**：`PK\x03\x04` + zip 内 `mimetype` 条目 = `application/epub+zip`。
2. **打开**：只读 zip 中央目录（一次 IO 得全部条目表）；`META-INF/container.xml` → OPF 路径。
3. **结构**：OPF → metadata（书名/作者/语言/封面声明）、manifest（条目 → 媒体类型）、spine（阅读序）；目录优先 EPUB3 `nav.xhtml`，回退 NCX。
4. **章内容**：按 spine 条目对应 `ChapterEntry`（`byte_range` = 条目内字节范围）；`load_chapter` 按需流式 inflate 单条目 → XHTML 解析 → `Block` 清洗管线：
   - 标签白名单（p/h1..h6/img/blockquote/hr/a…），其余下钻或丢弃；
   - CSS 只取影响几何的最小集（`font-size`/`line-height`/`text-align`/`text-indent`/`margin`），其余忽略；
   - 图片 → `ResourceKey`（引用 manifest 条目），尺寸提示尽量从 `width/height`/图片头解析；
   - 清洗产出干净 `Block` 流 + UTF-8 正文（`Inline` 偏移基于清洗后文本）。
5. **资源**：`load_resource` = 单条目 inflate + 媒体类型白名单（jpeg/png/gif/webp/svg 降级说明）；图片解码与降采样不在解析层做（归 `rrk-render`/宿主，见 §7.2）。
6. **封面**：OPF `cover-image` / manifest `properties=cover-image` / spine 首图三级回退。
7. **明确不支持**：DRM（`encryption.xml` 存在且非字体嵌入）→ `UnsupportedFormat` 错误码，不静默。

### 3.4 TXT 完整实现（`rrk-txt`）

1. **嗅探**：无魔数 → 扩展名 + 编码合法性打分；宁可误收不可误拒（后续编码检测兜底）。
2. **编码三级判定**：BOM → UTF-8 合法性校验 → `chardetng` 采样判定（GB18030/GBK/Big5…，前 64KB 统计），结论写入 `BookMeta` 全书复用；转码用 `encoding_rs` 流式进行，**大文件不在内存里放两份**（按块转码落 `ContentChunk`）。
3. **流式分章**：mmap/分块扫描，**只扫每行行首 N 字符**跑分章状态机（预编译正则族：`第X章/回/节/卷/部`、`Chapter N`、`序/楔子/尾声` 等，规则可配置），记录**字节偏移**不复制字符串；无任何标记的巨书按固定字符数切**逻辑块**，每块独立成 `ChapterEntry`。
4. **章内容**：段落切分（空行/全角缩进）、`\r\n` 规整、行内样式极少（`Inline::Text` 单 style）；超长单段保留为一个 `Paragraph`，由渲染层行组切片（《渲染层》§5 口径，语义偏移不动）。
5. **大文件**：`BookSource` 与 `ChapterContent` 都持 mmap 切片（`bytes::Bytes` 包 mmap region），200MB TXT 全程不整载；`unload_chapter` 丢弃转码缓存，mmap 由 OS 换页。

### 3.5 MOBI/AZW3 完整实现（`rrk-mobi`，解压算法全自研）

1. **嗅探**：PalmDB 头 + 记录 0 魔数 `BOOKMOBI`/`TEXtREAd`。
2. **结构**：PalmDB 记录表 + EXTH 元数据（书名/作者/封面偏移/封面 EXTH 201/202）；AZW3 = KF8（EXTH 125 标记），MOBI7 及以下为旧版 HTML 流。
3. **自研解压（核心，无第三方）**：
   - **PalmDOC LZ77**（压缩类型 2）：1 字节标记位 → 字面量/回溯对（dist 1..=1024、len 3..=10），自研实现小巧、可用规范样张全量对拍；
   - **HUFF/CDIC**（压缩类型 17480，KF8 常用）：按 MOBI 规范自研 HUFF 树重建 + CDIC 词典流式解码。解码慢是算法固有成本 ⇒ **导入期一次性转标准段落流**（《全链路》§3.3 口径），把解压从「每次打开」挪到「一次导入」；
   - 输出统一中间表示（HTML 流 + 资源表），后续走与 EPUB 相同的清洗管线。
4. **章内容**：HTML 流 → 复用 §3.3 的 XHTML→Block 清洗管线（同一套，不重复造）；KF8 按 `<mbp:pagebreak>` 切段（foliate-js 实证大幅改善渲染性能）；旧 MOBI 按 guide/目录链接辅助切章，无标记巨书切逻辑块。
5. **目录**：KF8 用 EXTH 121（NCX 缩略）/NCX 记录；旧 MOBI 用目录索引记录内的锚点位置。
6. **图片/封面**：记录表内图像记录（JPEG/PNG 原样引用）；封面取 EXTH 201/202。
7. **错误面**：DRM（EXTH 205/403）→ `UnsupportedFormat`；AZW1 等老格式 → 不支持；解压失败 → `CorruptData` + 纯文本降级。

### 3.6 PDF 完整实现（`rrk-pdf`，自研解析 + 分级光栅化）

**定位**：自研 PDF 是六格式中唯一的大工程（格式长尾重），按三级交付、样张集驱动；**不引入 pdfium/mupdf 兜底**是明确取舍——代码可控优先于短期覆盖率。

| 级 | 内容 | 交付形态 | 说明 |
| --- | --- | --- | --- |
| **L1 文本/结构**（先行） | xref/对象图/流过滤器（Flate/LZW/ASCII85/ASCIIHex/RunLength 自研）+ 页面内容流文本算子（Tj/TJ/T\*）+ ToUnicode/编码映射 → 文本抽取；页尺寸 + outline 书签 | **重排阅读模式**（文本 → `Block` 流走排版管线）+ 页数/目录/进度 | 「PDF 能读」的最快路径；双栏/阅读序用列检测启发式 |
| **L2 基础光栅化** | 路径填充/描边、图像（DCT=JPEG 直解、CCITT 自研）、标准 14 字体 + TrueType 字形、CTM/裁剪 | **原版页**位图（`render_page`） | 光栅原语 tiny-skia、字形光栅 Skrifa（均纯 Rust 白名单件）；管线自研 |
| **L3 长尾** | Type1/CFF 字形、透明组/混合模式、渐变/图案、JPX/JBIG2、Type3 字体、标注/表单 | 按样张集逐项补 | 未覆盖页降级 L1 文本模式并记样张进 backlog |

- **同一本书双模式**：L1 文本喂重排管线（`Block` → `LayoutPage`），L2/L3 产原版位图；宿主可在「原版 / 重排」间切换，`ReaderState` 各存进度。
- **降级纪律**：单页渲染失败不影响整书（占位 + 错误事件）；样张驱动排期，兼容性长尾不阻塞上层。
- **缓存**：位图按 `页码 × dpr × 渲染版本` 入 C1/C2（同三级纪律）；文本层抽取消耗大 → 结果落 `ContentChunk` 复用。

### 3.7 CBZ 完整实现（`rrk-cbz`，页级图库）

1. **嗅探**：`PK\x03\x04` + 条目全为图片 + 扩展名 `.cbz`。
2. **页索引**：zip 中央目录一次 IO → 图片条目按**数字自然序**（`12 < 100`）排成 `PageEntry[]`（`kind = Image`）；只记条目与压缩大小，不解压。
3. **页内容**：`load_page` = 单条目 inflate + 图片头解析尺寸；解码即降采样归图片管线（§7.2），预读 ±2 页（漫画翻页方向感知）。
4. **封面/目录**：封面 = 第一页；可选读 ComicInfo.xml 入 `TocNode`，缺失则目录为空。
5. **CBR 明确不支持**：RAR（尤其 RAR5）解压自研成本高、收益低 → `UnsupportedFormat` 错误码；接口预留，将来如需只按 RAR4 范围自研。
6. **大图库**：全书只驻页索引（KB 级），位图按需加载 + 限流（§7.2）。

### 3.8 格式支持矩阵与路线图

| 格式 | 类型 | 实现档 | 解压/渲染 | 状态 |
| --- | --- | --- | --- | --- |
| TXT | 重排 | P0 | 无 | **已落地**（§3.4） |
| EPUB | 重排 | P0 | inflate（白名单件） | **已落地**（§3.3） |
| MOBI/AZW3 | 重排 | P0 | **自研** PalmDOC/HUFF/CDIC | **已落地**（§3.5） |
| PDF | 固定版式（可重排） | P2；L1 文本模式可提前 | **自研** 解析 + 分级光栅 | **L1 + L2 已落地**（文本抽取/重排 + 基础光栅：路径/图像/字形；裁剪与 Type1/CFF 属 L3，§3.6） |
| CBZ | 固定版式 | P2 | inflate + 图片管线 | **已落地**（§3.7） |
| CBR / FB2 / DOCX 等 | — | 后置 | — | 接口预留，按需求排期 |

冷格式降级通道（WebView/foliate-js 类）属宿主集成，不进内核，也不作为格式覆盖的兜底承诺。

---

## 4. 渲染内核

### 4.1 排版是纯函数

```rust
pub fn layout(chapter: &ChapterContent, style: &StyleSpec, vp: &Viewport)
    -> Result<Vec<LayoutPage>, RrkError>;

pub fn page_at(chapter: &ChapterContent, pos: SemanticPos, style: &StyleSpec, vp: &Viewport)
    -> Result<LayoutPage, RrkError>;
```

纯函数 ⇒ 可缓存、可后台计算、可流式替换、可 rayon 并行（全链路 §5.1）。长章自动按逻辑块排版，`LayoutPage.start/end` 语义坐标保证任何页可独立定位（`page_at` 即「渲染指定页面」的内核入口）。

**字体策略（2026-10-21 拍板）**：除必须使用书内字体的书籍外，**其余全部使用系统字体**。
- **重排类**（TXT/EPUB/MOBI/AZW3、PDF 重排模式）：一律**系统字体栈**（`StyleSpec.font_family`
  默认 `Microsoft YaHei, PingFang SC, Noto Sans CJK SC, sans-serif`，平台取首个可用；
  宿主提供字体切换 = 改 styleHash → 布局缓存精确失效秒切）。书内嵌字体忽略，
  「使用书籍字体」开关属可选增强（默认关）。
- **固定版式**（PDF 原版页、CBZ）：**书内字体保真**（版式复刻语义，换字体即毁版式）；
  系统字体仅作标准 14/缺字兜底。

### 4.2 排版引擎选型

| 候选 | 组成 | 优势 | 短板 | 结论 |
| --- | --- | --- | --- | --- |
| `parley` | Fontique + Skrifa + HarfRust + ICU4X | 整形/断行/BiDi/字体回退一体，API 面向「段落 → 行 → 字形」 | 较年轻，复杂样式覆盖在演进 | **首选** |
| `cosmic-text` | rustybuzz + fontdb + swash | 成熟、字体发现全 | BiDi/复杂脚本弱于 ICU4X 系 | 备选/对照 |
| 自研断行 + swash | `unicode-linebreak` + swash 光栅 | 完全可控 | 工作量大 | 逃生门 |

两个候选都已补齐「Rust 做排版」的前提条件（全链路 §9 尾注）。**决策方式：以基准数据定**——用 50KB 单段、CJK 长章、混排样张各测「测量+分页耗时 / 结果正确性」，达标者上（§9 退出条件）。复杂 CSS EPUB 覆盖不足是已知短板，处置 = 降级通道（§3.5），不阻塞主链路。

### 4.3 两种渲染输出（「渲染指定页面」的两种消费方式）

| 模式 | 输出 | 消费方式 | 适用 |
| --- | --- | --- | --- |
| **A 几何模式**（默认） | `LayoutPage`（行表 + 字形 run + 图片矩形） | Flutter `CustomPainter`/`Canvas` 自绘；选区、高亮、link hitTest、TTS 跟随全可用 | 主力，保真可控 |
| **B 栅格化模式**（可选 feature） | `rasterize(page, dpr) -> RGBA` | `Image.memory` 直贴 | 低端机、复杂效果、预览缩略图 |

关键取舍：**几何模式是默认**，因为选区/hitTest 不能只靠位图（《渲染层》§4.3 C1 的隐藏大坑）；栅格化是降级与加速通道，不是主路径。模式 B 的位图走零拷贝缓冲直传 Dart `Uint8List`。

固定版式（PDF/CBZ）复用同一渲染出口：PDF 原版页走 `render_page` → 模式 B 位图（自研 L2 光栅，§3.6），PDF 重排模式与 CBZ 文本（ComicInfo）走模式 A；宿主 API 无格式特判。

### 4.4 分页/滚动与页窗口

- 分页模式：`LayoutPage[]` 一次分页，窗口 ±2 页预排（优先级：可见 > 相邻 > 预取）；
- 滚动模式：`LayoutPage` 高度可变（= 视口高倍数的行组块），视口 ±2 块懒排；
- 估算占位（三级交付）：内核提供 `estimate_page()`（按 `char_len` × 均值行高估行位置，语义坐标精确），宿主用于「可读」级上屏，精排页原地替换时按 `SemanticPos` 语义重锚（禁像素单跳，红线口径不变）。

### 4.5 远跳章的无感打开（目录跳转）

跳远章（如从第 3 章目录跳第 87 章）的成本**必须与距离无关**，靠五层机制保证「无感」：

1. **O(1) 定位**：章索引（字节范围/字数/锚点）常驻内存（C3），跳任意章 = 查表；TXT 直接字节偏移 seek、EPUB 直接 inflate 单条目——没有任何「扫过去」的过程。
2. **出字不等精排（三级交付）**：1 帧内给点击反馈 + 章标题 + 占位骨架；≤150ms 估算页出字（语义坐标精确、行位置估算）；≤500ms 精排页原地替换——**只排落位页 ±2 不排整章**，语义重锚、位移 < 1 行。
3. **调度插队 + 取消**：跳章瞬间目标章落位页任务提到最高优先级，原章相邻页/预测带预排全部 O(1) 丢弃（seq 作废）；快速连点目录只有最后一次跳转生效。
4. **目录预取**（最有效一招）：**打开目录面板时**（不是点击后）对可见项/悬停项调 `prefetch(chapterId, radius)`——用户浏览目录的 0.5~2 秒就是免费预取窗口，点击时解压已完成。
5. **命中即瞬开**：目标章被预取/就绪包命中 → 全链退化为取缓存；排过（同 `styleHash × viewportHash`）→ C2 布局缓存命中 = 零排版。

边界：巨章只解压落位逻辑块；章块损坏 → 纯文本流式降级；跳转风暴 → 陈旧结果作废。

### 4.6 进度保真与恢复落位（每次开书回到上次那一行）

**真值 = `SemanticPos(chapterId, charOffset)`（字符偏移），不存行号/页码/像素**——行号依赖字号与视口，一变就错；字符偏移永远指向同一个字，同设置下恢复出来自然就是同一行。

- **保存纪律**：停稳才写盘（松手/停稳后落盘）；未收敛不写（估算/占位态不写像素量）；写盘节流。`ReaderState = { chapterId, charOffset, keepScrollRemainder(像素余量), contentHash/styleHash/viewportHash 指纹 }`。
- **恢复三步**：① 章索引 O(1) 定位 → 章块就绪；② 精排行表**二分查找** `charOffset` 所在行拿行顶 y；③ 滚动位置 = `行顶 y − 像素余量`——余量还原保证视觉位置与上次一致（否则停在行中部的阅读位置会「跳上去」）。恢复过程同样走三级交付：估算页先落位、精排后语义重锚修正（不闪跳）。
- **三种情形口径**：

| 情形 | 表现 |
| --- | --- |
| 同样式同视口（关了再开） | 就是上次那一行，视觉位置一致（布局缓存命中，行表逐行相同） |
| 改字号 / 转屏 / 换设备 | 行号物理上必变，落点 = 上次读到的那个字所在行顶（「接着读」语义，追求行号一致反而是错的） |
| 内容版本变（contentHash 变） | `charOffset` 可能漂移 → 上下文指纹（偏移前后若干字符哈希）校正；失败退化为最近段落行顶 |

- **换算不外泄**：字符偏移 ↔ 行顶的换算只在内核（`pageAt(SemanticPos)`），宿主只传语义坐标，各端行为一致。

---

## 5. Flutter 封装（`rrk_reader` 包）

### 5.1 封装形态

- **技术**：`flutter_rust_bridge` v2（C ABI 在下，`rrk-ffi` 内维护句柄表 + `catch_unwind`）；Dart 侧薄 facade 包 `rrk_reader`。
- **分发**：FFI 插件布局（`flutter create --template=plugin_ffi` 结构）+ **各平台预编译产物随包**：`android/src/main/jniLibs/{arm64-v8a[,armeabi-v7a]}`、`ios/Frameworks/rrk.xcframework`、`macos/`、`windows/`、`linux/`。宿主 `flutter pub get` 后零配置。
- **不用 MethodChannel**：纯 FFI，无平台通道注册、无 Manifest/Info.plist 改动。

### 5.2 公开 Dart API（facade 签名）

```dart
// ---- 引擎与生命周期 ----
class RrkEngine {
  static Future<RrkEngine> create({EngineConfig config = const EngineConfig()});
  Future<RrkBook> openBook(String path);                 // 文件打开（mmap）
  Future<RrkBook> openBookBytes(Uint8List bytes, {String? name});
  Future<void> shutdown();                               // 释放线程池与全部句柄
}

// ---- 书：结构 / 内容 / 渲染 ----
class RrkBook {
  Future<BookMeta> meta();
  Future<List<TocNode>> toc();                            // 获取目录
  Future<List<ChapterEntry>> chapterIndex();              // 章索引（含权重）

  // 按章节读取
  Future<ChapterContent> loadChapter(int chapterId);
  Stream<ChapterContent> chapters({int from = 0});        // 流式供给，可取消
  Future<Uint8List> loadImage(String key);
  Future<Uint8List?> cover();

  // 分页读取 / 渲染指定页面
  Future<List<LayoutPage>> pagesOf(int chapterId, StyleSpec style, Viewport vp);
  Future<LayoutPage> pageAt(SemanticPos pos, StyleSpec style, Viewport vp);
  Future<RenderedPage> renderPage(PageRef page, {RenderMode mode = RenderMode.geometry});
  Future<LayoutPage> estimatePage(SemanticPos pos, StyleSpec style, Viewport vp);

  // 进度与状态（进度真值 = SemanticPos；像素偏移仅缓存）
  Future<void> saveState(ReaderState state);
  Future<ReaderState?> loadState();
  Future<void> prefetch(int chapterId, {int radius = 1}); // 预载相邻章块
  Future<void> close();                                   // 关书（句柄入池复用可选）

  Stream<EngineEvent> events();                           // 任务完成/缓存命中/进度
}
```

配套类型：`BookMeta` / `TocNode` / `ChapterEntry` / `ChapterContent`（`blocks` + 只读 `text` 视图）/ `StyleSpec` / `Viewport` / `LayoutPage`（`lines` 为 `Float32List`/`Int32List` 紧凑列，`runs` 为扁平对象）/ `PageRef` / `RenderedPage`（`LayoutPage? geometry` + `Uint8List? rgba`）。

### 5.3 异步模型

- 所有重活在 Rust 线程池（rayon，N−1 worker）；Dart 侧只见 `Future`/`Stream`，UI 线程零等待（全链路 §2.4 线程模型）。
- **一次 FFI = 一个完整任务**：`pagesOf(一章)` 一次调用返回整章页表，绝不行级跨界；首帧路径 FFI 往返 ≤ 2 次。
- `Stream` 用 NativePort 单向事件流（frb stream）；任务带 `cancelToken`，跳章/跳转即取消低优先级任务，取消代价 O(1)（丢弃未消费结果）。
- 同步小调用（`meta()` 等已缓存）标 `#[frb(sync)]`，免一次事件循环。

### 5.4 错误传递

```rust
pub enum RrkErrorKind {
    Io, UnsupportedFormat, CorruptData, InvalidParam,
    Cancelled, ResourceLimit, Internal,
}
pub struct RrkError { pub kind: RrkErrorKind, pub code: u32, pub message: String, pub context: Option<String> }
```

- FFI 层统一 `(code, message)` 出参 → Dart `RrkException`（`kind` 可编程判断）；**panic 不跨界**：`catch_unwind` 捕获 → `Internal` 错误码，绝不 abort 宿主。
- 可降级错误（图片失败、布局缓存损坏）不抛异常：走事件流 + 占位结果（口径同《全链路》§10.1「损坏 ≠ 判据失效」）。

### 5.5 资源释放

| 对象 | 释放机制 |
| --- | --- |
| `RrkEngine` | `shutdown()`：停线程池、清句柄表、drop 全部 mmap |
| `RrkBook` | `close()`：RAII drop；句柄置无效（后续调用返回 `InvalidParam`） |
| 章节/页缓存 | 内核 LRU 自管（保护窗口纪律）；`unloadChapter` 可主动释放 |
| Dart 侧兜底 | `Finalizer`：对象被 GC 且未 close 时回收对应句柄（防泄漏，不替代显式 close） |
| 进程退出 | `shutdown()` 幂等；mmap/临时缓存文件由 OS 清理，布局缓存落盘持久 |

---

## 6. 接入指南（新建 / 已有 Flutter 项目）

### 6.1 最小接入（两步）

```yaml
# pubspec.yaml
dependencies:
  rrk_reader: ^0.1.0
```

```dart
// main.dart —— 最小可运行：打开 → 目录 → 取章 → 分页 → 渲染页
final engine = await RrkEngine.create();
final book = await engine.openBook('/path/to/book.epub');

final meta = await book.meta();          // 书名/作者/封面
final toc = await book.toc();            // 目录树
final chapters = await book.chapterIndex();

const style = StyleSpec.defaults();
final vp = Viewport(width: 390, height: 844, dpr: 3);

final pages = await book.pagesOf(chapters[0].id, style, vp);   // 分页
final rendered = await book.renderPage(PageRef(pages.first));  // 渲染指定页面
// rendered.geometry → CustomPainter 自绘；rendered.rgba → Image.memory 直贴

await book.close();
await engine.shutdown();
```

### 6.2 宿主集成步骤与版本兼容（拟定值，随基准矩阵校准）

| 项 | 要求 | 说明 |
| --- | --- | --- |
| Flutter / Dart | Flutter ≥ 3.22（Dart ≥ 3.4） | 下限由 frb v2 + `Finalizer` 决定；CI 矩阵测到最新 stable |
| Android | minSdk 21；ABI：arm64-v8a 必发，armeabi-v7a 可选 | 可配 `abiFilters` 减体积；无需 Manifest 改动 |
| iOS / macOS | iOS 12+ / macOS 10.14+；xcframework 静态链接 | 无需 Info.plist 改动；bitcode 已废弃无影响 |
| Windows / Linux | Win10+ x64；Linux x86_64（glibc ≥ 2.31） | 桌面为一等目标（rust_reader_kernel 天然跨平台） |
| Web | **不支持**（首期明确排除） | WASM 后置评估 |
| 体积 | 无排版 feature ≈ 1~2MB/平台；含 layout ≈ 3~6MB；含 PDF L2 再 +0.5~1MB（估） | 全部自研/纯 Rust、无预编译 SDK；`strip + opt-level=z + LTO`；xtask 打包出报告 |

**宿主对接原则（对接方向：宿主 → 本项目）**：本项目是契约的唯一来源，MReader 等 Flutter 宿主**按本项目的 API 与数据模型对接**，内核不反向迁就宿主既有实现：

- 宿主只保留呈现面（UI/手势/动画/合成），解析、排版、缓存等计算面逐步收敛到内核；宿主既有的对应实现按迁移路径替换，不作为内核设计输入；
- 迁移期允许降级接入：只启用解析层（不用 `pagesOf`，仅 `loadChapter` 喂宿主现有排版）即 §9 P0 档形态，排版层就绪后再切换；期间缓存以 `contentHash` 为共同 key，避免双份失效口径；
- 版本升级以 semver + `RRK_ABI_VERSION` 对齐；宿主适配成本由 facade 包吸收（宿主代码只依赖 `rrk_reader` 的 Dart API，不感知原生产物与 crate 结构）。

**版本策略**：包 semver；原生产物带 `RRK_ABI_VERSION` 常量，Dart 启动时校验（不匹配 → 明确报错而非崩溃）；数据模型 schema 号独立演进（缓存损坏隔离重算）。

---

## 7. 性能与内存管理

### 7.1 大文件

- 源文件 **mmap**（TXT/EPUB 同），正文永不整载；EPUB 只读中央目录 + 按需 inflate（全链路 §3.4）；
- 分章 `ContentChunk`（zstd/deflate 独立压缩）→ 随机读 O(1)，内存与书大小无关；
- 超长章自动**逻辑块**化（默认 ~256KB/块），排版/缓存以块为粒度；
- 权重/分章只扫行首（TXT）或条目表（EPUB），关键路径零全量扫描。

### 7.2 大图片

- **解码即降采样**到目标显示尺寸（`image` + `fast_image_resize`），防 GPU 纹理超尺寸；
- 解码队列**限流 2**（全链路 §5.4），动画期只贴占位、停稳补真图（宿主策略）；
- 缓存两级：原始字节 LRU 50MB + 解码位图显式上限（设备档位）；离窗主动 evict；
- 解码失败 → 48×48 占位，不阻塞翻页；超大图（> 4096px）强制降采样 + 上限内流式解码。

### 7.3 渲染结果缓存（三级）

| 级 | 内容 | 策略 |
| --- | --- | --- |
| C1 内存 | 当前章 ±1 的 `LayoutPage` | LRU + **保护窗口**（窗口章永不淘汰；全保护不淘汰） |
| C2 磁盘 | rkyv 归档的 `LayoutPage`（key 三要素） | mmap 零反序列化加载；容量上限 + LRU；损坏隔离重算 |
| C3 源数据 | `ContentChunk` + 结构缓存（TOC/章索引） | 常驻（体积小），随书生命周期 |

- key = `contentHash × styleHash × viewportHash`：改字号只失效布局、转屏只重排可见页、旧缓存保留回切命中（《统一方案》§4.4）；
- **布局产物可落盘是内核的核心收益**（SkParagraph 排版结果不可序列化）：同书同样式同视口重开 = O(1) 取页，冷启动免排版。

### 7.4 跨语言调用开销控制

| 纪律 | 做法 |
| --- | --- |
| 批量化 | 一次 FFI = 一个任务（一章页表 / 一页渲染 / 一块字节）；禁行级/字形级跨界 |
| 零拷贝 | 大块数据（位图/图片/正文）走 `Vec<u8>` → `Uint8List` 外部内存直传；行表/页表用紧凑 TypedData 列（int/f32 数组）而非对象树 |
| 编码 | 边界统一 UTF-8 字节偏移；Dart String 只在显示局部文本时换算 |
| 往返预算 | 首帧路径 FFI ≤ 2 次；事件走单向 NativePort，不走请求-响应 |
| 同步短路 | 已缓存的小数据（meta/toc）`frb(sync)`，免事件循环 |
| panic/unsafe | `catch_unwind` 边界 + 句柄表托管；bytecheck 校验落盘缓存（可选） |

打点：`open.firstText` / `layout.cacheHit` / `ffi.calls` / `ffi.bytes` / `page.move.d` 等沿用《统一方案》§8 的 `[perf]` 事件口径，FFI 次数与拷贝字节数单独打点。

---

## 8. 可行性与风险（讨论）

### 8.1 结论先行

**可行，且与《全链路》§2.4 的四条收益判断一致**；但必须分档推进（§9），P0（解析内核）几乎无风险可立即做，P1（排版 Rust 化）以基准数据为硬门槛。理由：

1. **解析层收益确定**：zip 中央目录随机读、编码检测、流式分章、Block 清洗在 Rust（无 GC、mmap、rayon）的收益不依赖排版生态成熟度，2~5× 有把握（全链路 §2.4 预期收益）。
2. **排版层收益大但有前提**：3~10× 测量分页收益来自「并行 + 无中间对象 + 无 GC」，但前提是 parley/cosmic-text 的 CJK 断行与样式覆盖达到可用线——这一点**只能用基准与样张判定，不能拍脑袋**。
3. **布局产物可序列化是独有收益**：SkParagraph 不可序列化、跨会话即失效；rkyv 落盘 = 冷启动免排版，这是纯 Dart 路线拿不到的。
4. **Flutter 侧成本可控**：纯 FFI 插件 + 预编译产物，宿主零配置；Dart facade 保持薄，选区/hitTest 在几何模式下天然可用。

### 8.2 风险清单与规避

| # | 风险 | 严重度 | 规避 |
| --- | --- | --- | --- |
| 1 | **排版生态成熟度**（CJK 断行、复杂 CSS、字体回退边角） | 高 | 基准 + 样张验收后才进 P1；保留 `cosmic-text` 对照与自研断行逃生门；复杂书走降级通道 |
| 2 | **宿主迁移成本**（宿主既有解析/排版/缓存收敛到内核） | 中 | 对接方向已定：宿主按本项目对接（§6.2）；降级接入（P0 只用解析层）让宿主可分步切换，内核 API 不因宿主现状做特例 |
| 3 | FFI 边界纪律被滥用（行级跨界吃光收益） | 中高 | API 面天然批量化（`pagesOf`/`renderPage`）；`ffi.calls` 打点设预算红线 |
| 4 | 选区/高亮/hitTest 若走位图渲染全要自研 | 中 | 默认几何模式（§4.3）；位图仅降级/预览 |
| 5 | 双工具链（cargo × Flutter 构建矩阵、调试成本） | 中 | `xtask` 统一构建/打包；引擎 crate 纯 Rust 测试与基准先行；Dart 侧薄绑定 |
| 6 | 原生产物体积（含 ICU4X 时膨胀） | 中 | feature 裁剪（P0 无 layout）；strip + LTO；ICU4X 数据按 locale 子集化 |
| 7 | frb 大块数据传输是否真零拷贝 | 低-中 | P0 打样验证 `Vec<u8>`→`Uint8List` 传输路径；必要时外部 TypedData + 释放回调（列 P0 退出条件） |
| 8 | panic/unsafe 跨界 | 低 | `catch_unwind` + 句柄托管 + 可选 bytecheck；崩溃率进验收 |
| 9 | 平台矩阵（Android ABI/iOS 静态链接/桌面） | 低 | 预编译产物随包 + CI 矩阵；ABI 版本校验 |
| 10 | **PDF 自研兼容性长尾**（字体/透明/编码器变体多） | 中-高 | L1 文本模式先行保「能读」；L2 光栅样张集驱动逐项补；单页失败降级不阻塞整书；**不以 pdfium 兜底**（代码可控是硬性红线，覆盖率换可控是明确取舍） |
| 11 | HUFF/CDIC 解码慢（MOBI/KF8 固有成本） | 中 | 导入期一次性转标准段落流，成本挪出打开路径（《全链路》§3.3） |

### 8.3 三个待拍板项（建议先讨论）

1. **排版引擎选型**：默认押 `parley`，还是先并行 POC `cosmic-text` 对照？建议：P0 不需要拍板（不编 layout），P1 前用同一基准集测两者再定。
2. **渲染默认模式**：默认几何模式（自绘、可选区）已建议；若目标场景是「快速出货、效果优先」，可临时默认位图模式——但这会把选区/高亮推到后期补，不建议。
3. **PDF 自研分级节奏**：L1（文本/重排，保「能读」）→ L2（基础光栅）→ L3（长尾）的分级是否认可？L3 覆盖率目标与样张集需要定，避免长尾无限膨胀。

---

## 9. 分档推进与验收

### 9.1 三档（自底向上推进，每档退出条件独立把关）

> 技术切分与《统一方案》§5 同源（解析 → 排版 → 落盘），但**推进节奏由本项目自主决定**，不受宿主既有约束；宿主（MReader 等）在任一档完成后都可按 §6.2 对接。

| 档 | 内容 | 交付 | 进入条件 |
| --- | --- | --- | --- |
| **P0 解析内核** | `rrk-model/format/epub/txt/mobi/cache(mmap)/kernel/ffi` + Dart facade；`loadChapter/toc/meta/资源` 全通 | 可打开 **EPUB/TXT/MOBI/AZW3** 读内容（重排类全齐） | 立即可做 |
| **P1 排版内核** | `rrk-layout` + `pagesOf/pageAt/renderPage(几何)` + 布局缓存 | 分页读取与渲染指定页面全通 | P0 稳定 + 排版基准达标 + §8.3-1 拍板 |
| **P2 强化** | `rrk-cbz` + `rrk-pdf` 页级轨（L1 文本可提前）、rkyv 布局落盘、栅格化模式、预取/估算页、就绪包语义 | **PDF/CBZ 可读**（PDF 双模式）、冷启动免排版、低端机降级 | P1 指标达标；PDF 按 L1→L2→L3 样张验收 |

### 9.2 验收口径（沿用《统一方案》§8，内核视角裁剪）

| 指标 | 目标 | 测法 |
| --- | --- | --- |
| 打开书 → meta/toc 可用 | 冷 ≤ 100ms / 热 ≤ 20ms | `[perf]` 打点，真机 20 次 P50/P95 |
| `loadChapter`（20MB TXT 巨章） | ≤ 50ms（mmap + 命中） | 基准 |
| `pagesOf` 一章（3MB 长章） | P1 档 ≤ 200ms（空缓存） | 基准 |
| 布局缓存命中 | `layout.cacheHit` ≥ 90%（重开书） | 打点统计 |
| 目录远跳（冷章） | ≤150ms 出字（估算页）/ ≤500ms 精排就位 | 跳章打点，20 次 P50/P95（§4.5） |
| 目录远跳（预取/排过） | ≤150ms 直接精排就位 | 打点（§4.5） |
| 进度恢复（同设置） | 落点行偏差 = 0 行；余量还原误差 ≤ 2px | 随机位置关→开 20 次（§4.6） |
| 进度恢复（改字号） | 落点 = 同一句行顶（语义正确优先于行号一致） | 用例断言（§4.6） |
| FFI 往返 | 首帧路径 ≤ 2 次；`ffi.bytes` 拷贝 ≈ 0 | 打点 |
| 内存 | 200MB TXT / 大 EPUB 稳态 RSS ≤ 150MB；巨章峰值 ≤ 200MB | DevTools + RSS |
| 崩溃 | panic 跨界 = 0；损坏缓存可隔离重算 | 混沌用例（截断/损坏样张） |

判红纪律：每条指标配会变红的用例；修复后回头确认历史用例仍有判别力（防恒绿护栏，口径同源）。

### 9.3 里程碑

- **M0**：workspace 脚手架 + `rrk-model` + `rrk-txt` + FFI 打通（最小 Flutter demo：打开 TXT → 目录 → 取章）；
- **M1**：`rrk-epub` + `rrk-mobi`（自研 PalmDOC/HUFF/CDIC）+ `rrk-cache` + 完整 API + 接入文档——**重排类三格式全齐**；
- **M2**：`rrk-layout`（选型基准先行）+ 分页/渲染 API + 几何模式 demo；
- **M3**：`rrk-cbz` + `rrk-pdf` L1（文本/重排阅读模式）+ 页级 API（`PagedSource`）；
- **M4**：`rrk-pdf` L2 自研光栅 + rkyv 落盘 + 性能报告 + 预编译产物 CI + 示例工程（L3 长尾按样张集滚动推进）。

---

## 附：一页速记

- **分权**：解析（格式 → 三层数据模型的结构/内容层）与渲染（内容层 → 版式层 `LayoutPage`）只以 `rrk-model` 为契约；内核零 UI 依赖。
- **数据**：`BookMeta`/`TocNode`/`ChapterEntry`（结构）→ `Block`/`Inline`/`ChapterContent`（内容，零拷贝 UTF-8 切片）→ `LayoutPage`（版式，rkyv 可落盘）；进度真值 `SemanticPos(chapter, charOffset)`。
- **自研（硬红线）**：格式解析、MOBI 解压（PalmDOC/HUFF/CDIC）、PDF 解析与光栅化、排版管线全自研；禁 pdfium/mupdf/unrar/闭源 SDK/预编译黑盒；第三方仅限纯 Rust 可审计白名单件（§3.2）。
- **格式**：`FormatAdapter`/`BookSource` + 页级 `PagedSource`，静态注册；六格式全自研——重排类 TXT/EPUB/MOBI/AZW3（P0），固定版式 PDF（自研分级 L1 文本→L2 光栅→L3 长尾）/CBZ（P2）；CBR 明确不支持。
- **渲染**：纯函数 `layout/page_at`；默认几何模式（Flutter 自绘，选区可用），可选栅格化；固定版式走同一 `renderPage` 出口；缓存三级 + 三要素 key + 保护窗口。
- **Flutter**：frb v2 + 纯 Dart facade `rrk_reader`（FFI 插件布局 + 预编译产物随包，宿主零配置）；`openBook/toc/loadChapter/pagesOf/renderPage/close`；Future/Stream 异步、`RrkError` 分类、panic 不跨界、Finalizer 兜底。
- **性能**：mmap + 分章压缩块 + 逻辑块；图片解码即降采样 + 限流；布局 rkyv 落盘；FFI 批量化 + TypedData 零拷贝 + UTF-8 偏移 + 首帧 ≤ 2 次往返。
- **推进**：P0（TXT/EPUB/MOBI/AZW3 解析）→ P1 排版（基准达标才进）→ P2（PDF/CBZ 页级 + 落盘/栅格化）；最大不确定性是排版生态（§8.2 #1）与 PDF 自研长尾（#10），分别用基准/样张裁决。
