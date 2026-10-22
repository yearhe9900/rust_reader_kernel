# Changelog

本项目的所有重要变更都记录在此文件。

格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

## [Unreleased]

（暂无）

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
