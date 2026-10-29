# rust_reader_kernel

**Rust 阅读内核**：面向阅读器应用的跨平台内容内核——统一解析、统一数据模型、
高性能排版与双模式渲染，经 C ABI / Dart FFI 对接 Flutter 等任意宿主。

> 一句话：把「一本书」变成「可排版、可渲染、可缓存、可定位」的标准数据面，
> 宿主只关心呈现。

## 功能特性

### 格式支持

| 格式 | 类型 | 状态 | 说明 |
| --- | --- | --- | --- |
| TXT | 重排 | ✅ | 编码检测（chardetng）+ 流式分章 |
| EPUB | 重排 | ✅ | OPF/NCX 目录、清洗白名单、资源管线 |
| MOBI / AZW3 | 重排 | ✅ | **自研** PalmDOC LZ77 + HUFF/CDIC 解压 |
| PDF | 固定版式 + 重排 | ✅ | **自研** L1 文本抽取/重排 + L2 光栅（路径/图像/CID 字形/裁剪/透明度/渐变） |
| CBZ | 页级图库 | ✅ | 数字自然序页索引、图片直出 |

### 核心能力

- **统一数据模型**（`rrk-model`）：`BookMeta / ChapterEntry / ChapterContent(Block/Inline) /
  TocNode / LayoutPage`——解析与渲染严格分权，格式知识不出适配器；
- **排版内核**（`rrk-layout`，parley 引擎）：测量/断行/BiDi/分页，纯函数口径
  `layout(chapter, style, viewport) -> Vec<LayoutPage>`，同一输入同一输出；
- **双渲染模式**：几何模式（`LayoutPage` 行表 + 文本 run → 宿主自绘，选区/hitTest 可用）
  与整页栅格化（PDF 原版页 / CBZ 图页 → RGBA 位图）；
- **三要素布局缓存**（`rrk-cache`）：`contentHash × styleHash × viewportHash` 精确失效，
  内存 LRU（保护窗口）+ 磁盘落盘（校验和 + 损坏隔离），实测命中提速 ~93%；
- **进度真值**：`(chapterId, charOffset)` 语义坐标，与视口/字号无关，换设备/换字号秒恢复；
- **字体策略**：重排类一律系统字体栈（切换 = styleHash 变 = 缓存精确失效），
  书内嵌字体仅固定版式保真使用。

### 工程红线

全自研、代码可控：格式解析/解压/排版/渲染管线自己实现，**禁止 pdfium / mupdf /
闭源 SDK / 预编译黑盒二进制**；第三方依赖仅限纯 Rust 白名单（见方案 §3.2，
parley / tiny-skia / swash / image 等，均可审计可 fork）。

## 快速开始

### 环境要求

- Rust stable（1.80+）
- 可选：Dart 3.4+（FFI 封装）、Flutter stable（demo）

### 构建与测试

```sh
cargo build --release            # 全 workspace
cargo test                       # 28 套单测
cargo run --release -p rrk-inspect -- fixtures/sample.epub
```

### 预编译 Release 包（供其它项目快速接入）

```sh
bash scripts/package-release.sh            # Windows + Android 四 ABI
SKIP_ANDROID=1 bash scripts/package-release.sh   # 只打 Windows
```

产物在 `dist/rrk_reader-<version>/`（含各平台 `rrk_ffi` 动态库、`dart/rrk_reader`
封装包、`MANIFEST.json` 校验和、接入说明 `README.md`）及同名 `.zip`。发布到 GitHub：

```sh
gh release create v0.1.2 dist/rrk_reader-0.1.2.zip --generate-notes
```

### Release 产物与验证

打 tag（`v*`）后 GitHub Actions（`.github/workflows/release.yml`）自动构建三平台
`rrk_ffi` 动态库，Release 附件包含：

- `rrk_ffi.dll` / `librrk_ffi.so` / `librrk_ffi.dylib`（release 构建）
- `rrk.h`（cbindgen 生成的 C 头文件，与源码同 commit）
- `SHA256SUMS.txt`（各产物 SHA-256）
- `SHA256SUMS.txt.sig`（GPG 分离签名，**未配置密钥时自动跳过**）

宿主侧验证完整性：

```sh
sha256sum -c SHA256SUMS.txt                    # 1. 校验产物未被篡改
gpg --import docs/keys/hzp9900-maintainer.asc  # 2. 导入维护者公钥（仓库内，一次性）
gpg --verify SHA256SUMS.txt.sig SHA256SUMS.txt # 3. 验证校验和可信
```

维护者配置（一次性）：生成 GPG 密钥后，将 `gpg --armor --export-secret-keys
YOUR_ID` 的输出存入仓库 Settings → Secrets 的 `GPG_PRIVATE_KEY`，口令存
`GPG_PASSPHRASE`（多把密钥时用 `GPG_KEY_ID` 指定签名钥）；公钥发布到 keyserver
或 GitHub Profile 供宿主导入。平台证书签名（Apple Developer / Azure Trusted
Signing）的选型与成本见 [docs/ROADMAP.md](docs/ROADMAP.md)。

### 命令行工具（rrk-inspect）

解析诊断工具：书目 / 章索引 / 目录 / 章内容 / 页渲染。

```sh
rrk-inspect <书路径> [--toc] [--chapter N] [--blocks M] [--render N --out page.png]
rrk-inspect <书路径> --search 词      # 全书搜索（拉模式分块）
rrk-inspect <书路径> --bm25 词        # BM25 建索引 + 排序
```

### Dart / FFI

```sh
cargo build --release -p rrk-ffi      # 产出 rrk_ffi.dll / librrk_ffi.so / .dylib
cd bindings/dart/rrk_reader
dart pub get && dart analyze
dart run example/smoke.dart <ffi库路径> <书路径>
```

API 概览：`RrkEngine.init/openBook` → `meta()/toc()/chapters()/loadChapter()`
→ `pagesOf()/pageAt()/hitTest()/prefetch()`（排版与进度）→ `renderPage()`（整页位图）
→ `search()/buildIndex()`（全书检索与 BM25 索引）→ `RrkImage.*`（尺寸头解析 / BGRA→PNG）
→ `configureContentCache()/contentCacheStatsJson()`（内容级缓存，见下）
→ `progressOf()/locateByPercent()/mapOffsets()/sliceText()`（进度与坐标，见下）
→ `loadHeadings()`（章标题块，无 TOC 书的章名回填，issue #13）。

C 接入方请直接用头文件 [`bindings/rrk.h`](bindings/rrk.h)；线程安全与调用约定见
[`docs/FFI-调用约定与线程安全.md`](docs/FFI-调用约定与线程安全.md)。

#### 进度与坐标（`rrk_progress_of` / `rrk_locate` / `rrk_map_offsets` / `rrk_slice_text`）

- **进度**：`progressOf(chapter, byteOffset) -> [0,1]` 与其逆 `locateByPercent`——
  按章权重估算的口径，专供进度条与跨设备同步；精确定位仍走 `pageAt/hitTest`。
- **UTF-16 映射**：`mapOffsets(chapter, offsets, {toUtf16})`——Dart/JS/Java/Swift
  字符串索引皆 UTF-16 code unit，标注/选区必经；代理对安全，批量单趟。
- **选区取文**：`sliceText(chapter, start, end)`——夹到字符边界后返回切片，
  免为取一段选区拉回整章。

#### 稳定错误码（ABI 契约，只增不改）

任何入口失败后，除返回码（非 0）外，可用 `rrk_last_error_code()` 取**稳定错误码**，
配合 `rrk_last_error()` 的中文/调试消息——宿主应按码值映射用户文案，勿对消息做
字符串匹配（消息可随调试需要变更）。

| 码值 | 枚举（`rrk_model::RrkErrorKind`） | 含义 |
|------|----------------------------------|------|
| 1001 | `Io` | IO 失败（文件不存在/不可读等） |
| 1002 | `UnsupportedFormat` | 格式不支持（含 DRM、CBR 明确拒绝） |
| 1003 | `CorruptData` | 数据损坏（结构解析失败） |
| 1004 | `InvalidParam` | 参数非法（空指针/越界/非 UTF-8） |
| 1005 | `Cancelled` | 任务被取消（检索取消等） |
| 1006 | `ResourceLimit` | 资源超限（内存/大小上限） |
| 1999 | `Internal` | 内部错误（含锁中毒等兜底） |
| 2001 | （`ERR_PANIC` 常量） | 边界 panic 被拦截 |
| 0 | — | 本线程尚无错误记录 |

#### 内容级缓存（issue #12 + open 索引持久化）

与布局缓存（`rrk_cache_configure`，三要素 key）并行的另一层：**内容级 key**
（只含 `content_hash`），命中即免重新解析：

- **访问器口径（issue #12）**：kind = toc / chapter_text / txt_outline，命中免
  inflate + 清洗 + 分块；
- **open 索引口径（ROADMAP 章级/页级缓存持久化）**：kind = open_epub / open_mobi /
  open_cbz / open_pdf / open_txt，open 期重建的索引（entries / pages / toc_raw）
  持久化，重开命中免重建（免分章扫描、nav/NCX 解析、页面树遍历、逐页尺寸探测等）。

接入面：

- C ABI：`rrk_content_cache_configure(dir, mem_bytes)`（空目录 = 关闭）；
  `rrk_content_cache_stats()` → `{"toc","chapter","outline","open"}` 各 `{hits,misses}`。
- Dart：`RrkEngine.configureContentCache(dir)` / `contentCacheStatsJson()`。
- `rrk_open`（自动走 open 索引缓存）与 `rrk_toc_json` / `rrk_chapters_json` /
  `rrk_load_chapter`（访问器缓存）自动生效，无需额外调用。
- **失效口径**：内容 hash 变了整本失效（与宿主原 Dart 缓存的「副本 mtime/size
  判据」等价）；payload 自带 schema 号，内核升级/损坏自动丢弃重算。

### 接入选型：C ABI（`rrk-ffi`）还是 FRB 直连

- **推荐 C ABI**：跨语言稳定边界，预编译动态库直接加载，宿主无需 Rust 工具链；
  数据出口为 JSON（`meta_json` / `blocks_json` / `link_ranges`）。
- **FRB 直连 `rrk-*` crate**：仅在需要零 JSON 序列化时选用。注意「块 → 宿主段落」
  唯一降维口径是 `rrk_model::flatten_blocks`（引用块摊平、分割线丢弃、行内图片以
  `\u{FFFC}<key>\u{FFFC}` 标记内嵌），**不要在宿主侧重写映射**，否则模型演进两处漂移。

### Flutter demo（阅读页）

```sh
cd demo/flutter_reader
flutter create . --platforms=windows,android   # 一次性生成平台 runner
flutter run -d windows --release \
  --dart-define=RRK_LIB=<ffi库绝对路径> \
  --dart-define=BOOK=<书路径>
```

> Windows 下请带 `--release`（Debug 版依赖 VC++ 调试运行时）。详见
> [demo/flutter_reader/README.md](demo/flutter_reader/README.md)。

## 性能摘要

release 构建、Windows x64 实测（完整数据见
[docs/性能-排版与解析基准.md](docs/性能-排版与解析基准.md)）：

| 指标 | 实测 |
| --- | --- |
| 打开 24MB MOBI（1509 章） | 394 ms |
| 排版 87KB 长章（67 页） | 23.9 ms（parley） |
| 布局缓存命中 | 1.6 ms（提速 ~93%） |
| PDF 原版页渲染 @900px | 15–23 ms/页 |
| 全书搜索（7.1MB EPUB / 1152 章，命中 23750） | 204 ms（1188 批跨 FFI） |
| BM25 建索引（同上，380544 词项） | 915 ms，序列化 9.5MB |

## 目录结构

```
rust_reader_kernel/
├── crates/                  # Cargo workspace
│   ├── rrk-model/           # 统一数据模型（零 IO、零格式知识）
│   ├── rrk-format/          # FormatAdapter / BookSource / PagedSource + 嗅探注册表
│   ├── rrk-markup/          # XHTML/HTML 清洗（标签白名单）
│   ├── rrk-zip/             # zip 随机读（EPUB/CBZ 共用）
│   ├── rrk-txt/ rrk-epub/ rrk-mobi/ rrk-cbz/ rrk-pdf/   # 各格式适配器
│   ├── rrk-layout/          # 排版内核（parley）+ 引擎对照基准
│   ├── rrk-search/          # 全书检索（拉模式分块）+ BM25 索引
│   ├── rrk-image/           # 图片尺寸头解析 + BGRA→PNG 编码
│   ├── rrk-cache/           # 三要素缓存（内存 LRU + 磁盘落盘）
│   ├── rrk-kernel/          # 书架/句柄/生命周期
│   ├── rrk-ffi/             # C ABI（Dart/Flutter/任意宿主）
│   └── rrk-inspect/         # CLI 诊断工具
├── bindings/dart/rrk_reader # Dart FFI 封装包
├── demo/flutter_reader/     # Flutter 阅读页 demo（几何自绘 + PDF 原版页）
├── fixtures/                # 公开测试样本（txt/epub/cbz/pdf）
├── docs/                    # 设计方案与性能基准
└── .github/workflows/ci.yml # CI：单测 + Dart/Flutter 检查 + 三平台产物
```

## 文档索引

| 文档 | 内容 |
| --- | --- |
| [设计-Rust阅读内核总体方案](docs/设计-Rust阅读内核总体方案.md) | 总体架构、数据模型、格式适配、缓存/排版/渲染设计 |
| [设计-极速开书全链路](docs/设计-极速开书全链路.md) | 首屏性能与开书链路 |
| [设计-极速开书与渲染统一方案](docs/设计-极速开书与渲染统一方案.md) | 解析/渲染统一数据面 |
| [分析-渲染层性能与按需渲染改造方案](docs/分析-渲染层性能与按需渲染改造方案.md) | 按需渲染分析 |
| [设计-检索与BM25索引](docs/设计-检索与BM25索引.md) | 全书搜索会话、偏移映射、BM25 索引格式 |
| [FFI 调用约定与线程安全](docs/FFI-调用约定与线程安全.md) | C ABI 契约：缓冲所有权、错误模型、线程安全矩阵、坐标系速查（配头文件 `bindings/rrk.h`） |
| [ROADMAP：范围与明确不做](docs/ROADMAP.md) | 能力边界与理由、待办清单、对接方可依赖的语义承诺 |
| [性能-排版与解析基准](docs/性能-排版与解析基准.md) | 实测数据与验收判据 |
| [CHANGELOG.md](CHANGELOG.md) | 版本变更记录 |

## 贡献

欢迎 Issue 与 PR。请阅读 [CONTRIBUTING.md](CONTRIBUTING.md)（流程、测试要求、
架构约束——**自研红线与依赖白名单**务必先看）。

## 许可证

本项目采用 **MIT OR Apache-2.0** 双许可证（Rust 生态惯例），任选其一。

- [LICENSE-MIT](LICENSE-MIT)
- [LICENSE-APACHE](LICENSE-APACHE)
