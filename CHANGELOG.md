# Changelog

本项目的所有重要变更都记录在此文件。

格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

## [Unreleased]

**ABI 6**：Dart 封装（`kRrkAbiVersion = 6`）与预编译原生库必须同步升级（新增图像出口）。

### Added

- **图像工具（issue #8）**：新增 `rrk-image` crate，把宿主侧的纯字节图像算法下沉内核。
  - `rrk_image::dimensions`：**只读文件头**取尺寸 + 格式（PNG IHDR / JPEG SOFn 段链 /
    GIF / BMP `BITMAPINFOHEADER`·`BITMAPCOREHEADER` / WebP `VP8X`·`VP8 `·`VP8L`），
    不解码像素、畸形输入返回 `None`；CBZ 页尺寸表与本接口共用同一份实现（`rrk-cbz`
    原私有头解析已删除）。
  - `rrk_image::encode_png_bgra`：BGRA 位图 → PNG。逐行 BGRA→RGBA 后直接 deflate，
    **不分配整幅 RGBA 中间缓冲**（1080×1440 页位图峰值额外内存 ≈ 一行）；CRC32 自算，
    只产出字节不写盘。
  - FFI：`rrk_image_dimensions` / `rrk_encode_png_bgra`（位图缓冲零拷贝入参）；
    Dart：`RrkImage.dimensions` / `encodePngFromBgra`（含 `…Ptr` 零拷贝变体）。

## [0.1.1] - 2026-10-22

修复 GitHub issue #1–#7。**ABI 2 → 5**：Dart 封装（`kRrkAbiVersion = 5`）与预编译原生库必须同步升级。

### Added

- `BookMeta.description`：EPUB `dc:description` / MOBI EXTH 103·104（空白折叠、4000 字符截断）。
- `ChapterContent.link_ranges`：正文内链「文本区间 → href」；EPUB href 归一化为包内绝对路径并保留 `#frag`。
- `rrk_model::flatten_blocks`：块树 → 宿主段落的唯一降维口径（引用块摊平、分割线丢弃、行内图片以 `\u{FFFC}<key>\u{FFFC}` 内嵌）。
- 目录树：`TocNode.children` 保留 nav `<ol>/<li>` 与 NCX `navPoint` 嵌套；`TocNode.anchor` 惰性解析为章内字节偏移。
- 封面回退：EPUB 显式封面 → spine 首文档首图 → manifest 首个 `image/*`；MOBI EXTH 201 缺失时按图片魔数扫描资源记录。
- `BookKind`（Text / ImageGallery / Paged）+ `BookSource::as_paged`：CBZ/PDF 经统一句柄接入；
  FFI 新增 `rrk_book_kind` / `rrk_page_count` / `rrk_load_page_json` / `rrk_load_resource`。
- Dart：`BookMeta.description`、`ChapterContent.links`、`TocNode.anchor`、
  `RrkBook.kind/pageCount/loadPage/loadResource` 与 `PagedPage`。

### Fixed

- TXT：GBK 正文被 `chardetng` 在 64KB 采样窗口误判为 `windows-1252`，正文解成 `ÕâÐ©` 类拉丁乱码。
- 排版：`para_indent_em` / `align` 未被引擎消费——首行缩进在几何模式下丢失、对齐设置不生效；`align` 新增两端对齐。
- 封面：无显式封面的书 `cover()` 返回 `None`。
- 目录：层级被压平、`anchor` 恒 `None`、`fragment` 被丢弃。
- 内容出口：宿主桥与 `rrk-ffi` 重复实现「块 → 文本」映射，模型演进需多处同步。

## [0.1.0] - 2026-10-22

首个公开版本：六格式解析 → 高性能排版 → 双模式渲染 → FFI/Dart/Flutter 全链路。

### Added

- **统一数据模型**（`rrk-model`）：`BookMeta/ChapterEntry/ChapterContent(Block,Inline)/
  TocNode/LayoutPage/RenderResult`，解析与渲染分权；RLAY 布局二进制编解码（schema 校验）。
- **格式适配框架**（`rrk-format`）：`FormatAdapter` 嗅探注册表 + `BookSource`（重排）+
  `PagedSource`（页级）双契约。
- **格式适配器**：
  - `rrk-txt`：编码检测（chardetng/encoding_rs）+ 流式分章；
  - `rrk-epub`：自研 zip 随机读（`rrk-zip`）+ OPF/NCX + 清洗白名单（`rrk-markup`）；
  - `rrk-mobi`：自研 PalmDOC LZ77 + HUFF/CDIC 解压，MOBI/AZW3 全支持；
  - `rrk-pdf`：自研 L1 文本抽取/重排（对象扫描表 + ObjStm、五种流过滤器、
    文本算子、ToUnicode CMap、outline）+ L2 光栅（tiny-skia：路径/图像/CID 字形/
    裁剪/ExtGState 透明度/sh 渐变）；
  - `rrk-cbz`：数字自然序页索引、图片直出位图。
- **排版内核**（`rrk-layout`）：parley 引擎（测量/断行/BiDi/分页），纯函数布局、
  跨块分页、语义坐标单调；引擎对照基准（parley 13ms vs cosmic-text 261ms）。
- **缓存框架**（`rrk-cache`）：三要素 key、内存 LRU（保护窗口）、磁盘落盘
  （原子写 + 校验和 + 损坏隔离）；已接入 FFI 产品链路（命中免排版）。
- **内核与桥接**：`rrk-kernel` 书架/句柄/生命周期；`rrk-ffi` C ABI v2
  （open/meta/toc/chapters/load_chapter/pages_json/page_at/hit_test/prefetch/
  render_page/cache_configure）；`bindings/dart/rrk_reader` Dart 封装。
- **Flutter 阅读页 demo**（`demo/flutter_reader`）：几何自绘（PageView + run 定位）、
  字号/字体切换、进度保存恢复、PDF 原版页位图模式、Semantics 可访问性。
- **工具与工程**：`rrk-inspect` CLI 诊断工具；`fixtures/` 公开测试样本；
  GitHub Actions CI（单测 + Dart/Flutter 检查 + 三平台预编译产物）；
  性能基准报告（docs/性能-排版与解析基准.md）。

### Changed

- 排版引擎选型经基准数据终审锁定 parley（方案 §4.2）。
- 字体策略定案：重排类一律系统字体栈，书内嵌字体仅固定版式保真使用。

### Fixed

- 多行段落基线双重叠加导致坠行重叠（parley 累计坐标 → 行内基线换算）。
- PDF 对象扫描在二进制上下文误做 UTF-8 校验导致「无 Catalog」。
- CID 字体（Type0）字形取自 DescendantFonts 的 FontFile2；混合 gid 策略
  （ToUnicode→cmap 优先，CID→CIDToGIDMap 兜底）使中文上屏。

[Unreleased]: https://github.com/yearhe9900/rust_reader_kernel/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/yearhe9900/rust_reader_kernel/releases/tag/v0.1.0
