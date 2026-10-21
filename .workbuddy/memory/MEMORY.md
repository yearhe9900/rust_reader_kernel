# rust_reader_kernel 项目长期记忆

## 项目定位
- 本仓库 = Rust 阅读内核（Cargo workspace `rust_reader_kernel`）设计与实现：解析/渲染分权、统一书籍数据模型、格式适配（EPUB/TXT 首发）、Flutter FFI 封装（`rrk_reader` 包）。
- 文档命名：`设计-*.md` / `分析-*.md`，与《设计-极速开书全链路》《设计-极速开书与渲染统一方案》《分析-渲染层性能与按需渲染改造方案》术语逐句对齐（SemanticPos/ContentChunk/LayoutPage/三要素缓存 key）。
- **文档统一存放 `docs/` 目录**（2026-10-21 用户规定）；新增文档一律放 docs/，仓库根目录不放 md。

## 关键决策（用户拍板）
- **对接方向（2026-10-21）**：不考虑 MReader 既有约束（如 RUST-09）；**本项目是契约唯一来源，MReader 等 Flutter 宿主按本项目 API 对接**。内核不反向迁就宿主实现；宿主只留呈现面，解析/排版/缓存按迁移路径收敛到内核。降级接入 = P0 只用解析层（loadChapter 喂宿主现有排版）。
- **全自研、代码可控（2026-10-21 用户拍板，硬红线）**：格式解析/解压算法（MOBI PalmDOC LZ77 + HUFF/CDIC）/PDF 解析与光栅化/排版管线全自研；禁止 pdfium/mupdf/unrar/闭源 SDK/预编译黑盒二进制。第三方仅限「纯 Rust、可审计、可 fork、许可宽松」白名单件（白名单见方案 §3.2），变更需评审登记。
- **格式支持（六格式）**：重排类 TXT/EPUB/**MOBI/AZW3（已落地）**+ 固定版式 PDF（自研分级 L1 文本重排→L2 基础光栅→L3 长尾样张驱动）/CBZ（待做）；CBR 明确不支持。固定版式走 `PagedSource` 页级变体（§2.4），PDF 支持「原版页 + 重排阅读」双模式。
- **MOBI 压缩口径（勿再误读）**：compression **1=无压缩 2=PalmDOC 17480=HUFF/CDIC**（MReader 侧曾误读为 0/1/2）。HUFF/CDIC 码表结构对齐 foliate-js/calibre；脏书兜底按 UTF-8 合法率择优。
- **开发进度**：M0–M3 已完成并提交——rrk-model/format/markup/txt/epub/mobi/kernel/ffi + rrk-inspect CLI + bindings/dart/rrk_reader（手写 dart:ffi，C ABI 契约：open/close/meta_json/toc_json/chapters_json/load_chapter/cover/last_error/buf_free/abi_version）。Dart→FFI→Rust 全链路实测通过（含 24MB MOBI/AZW3）。PDF L1/CBZ、布局落盘、Flutter 接入 demo 未做。
- 分档推进：P0 解析内核（model/format/epub/txt/mobi/cache/kernel/ffi，可立即做）→ P1 排版内核（parley 首选、cosmic-text 对照，**基准达标才进**）→ P2 pdf/cbz 页级轨 + rkyv 布局落盘 + 栅格化。
- 渲染默认**几何模式**（LayoutPage → Flutter 自绘，保选区/hitTest），栅格化为可选降级。

## 待拍板
- 排版引擎：默认押 parley 还是 P1 前并行 POC cosmic-text（建议后者，用同一基准集裁决）。
- 渲染默认模式确认（建议几何模式）。

## 环境
- 已 git 仓库（2026-10-21 初始化）；remote `origin` = https://github.com/yearhe9900/rust_reader_kernel.git，分支 `master`。`tmp_book/`（测试书）、`target/`、`Cargo.lock`、`.dart_tool/`、`pubspec.lock` 在 .gitignore。
- **GitHub CLI 已装**（gh 2.101.0，账号 yearhe9900，repo 权限；`gh auth setup-git` 已配凭证助手——推送零警告）；可 gh 管理 issue/PR/Actions。用户口径：任务批次提交后用命令行推送。
- 用户「任务批次即提交 git」惯例生效中；crates.io 走 USTC 镜像。
