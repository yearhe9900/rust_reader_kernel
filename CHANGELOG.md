# Changelog

本项目的所有重要变更都记录在此文件。

格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

## [Unreleased]

### Added

- **issue #13（无 TOC 书的章标题）**：
  1. **首选落地**：EPUB 无 nav/NCX 时，open 期用「限流前缀（64KB）+ 同一条
     清洗管线」提取正文首个**最小级**标题回填 `ChapterEntry.title`（不全量
     inflate；前缀内无标题回 None 由宿主回落占位）；有目录的书不受影响
     （backfill 只填空缺）。
  2. **轻量出口**：`rrk_load_headings`（JSON `[{"level","text"}]`）+
     `Kernel::chapter_headings` + Dart `loadHeadings`——走章内容级缓存，命中
     免解析；宿主的临时 `_resolvePlaceholderTitles` 逐章扫描可换到这里。
  3. **h 块排除**：v9 部件流已按 `type` 区分块型，宿主直接过滤 `type=="h"`
     即可（无需内核删块——块流保持完整，偏移坐标系不变）。
  - ABI 9 → 10；配套测试：TOC-less 回填/无标题回 None/有目录不覆盖/kernel
    标题块出口（含 CBZ 图片章空表）。

## [0.2.0] - 2026-10-28

**ABI 7 → 9**：本版含两轮破坏性变更（v8 = 稳定错误码/内容级缓存，v9 = blocks
部件流），Dart 封装（`kRrkAbiVersion = 9`）与预编译原生库必须同步升级。

### Changed

- **ABI 8 → 9（宿主必须同步升级原生库与 Dart 封装）**：
  1. **blocks JSON 改部件流**：`rrk_load_chapter` / `rrk_load_page_json` 的
     blocks 不再内嵌正文——文本给章文本的字节区间 `[start,end]`、行内图片给
     `["key"]`，宿主按序拼接（图片位插 `\u{FFFC}key\u{FFFC}`），与旧格式
     逐字节一致（rrk-model `block_parts_reconstruct_flatten_text` 单测钉死）；
     大章跨 FFI 带宽减半（实测样张 blocks 46B）；Dart 绑定已按字节区间重建。
  2. **排版器池**：`min(可用并行度, 4)` 台 parley FontContext 轮转取用，
     并行排版吞吐随池叠加（线程安全矩阵已更新）。

### Added

- **PDF 渲染深化**：图像 XObject 文档级解码缓存（同一图跨页/跨 Do 只
  inflate+解码一次，96MB 上限整表回收）；裁剪相交限定在路径包围盒内
  （免全页逐字节 min）。
- **cbindgen 接入**：`cbindgen.toml` + `bindings/rrk.h` 改为生成产物
  （文档注释从 Rust 源带入）；CI 新增 `cbindgen --verify` 漂移校验与
  `gcc -fsyntax-only` 头文件语法检查。
- **Release 自动化**：`.github/workflows/release.yml`——打 tag（v*）自动
  三平台构建 + SHA256SUMS + 附加到 GitHub Release（签名仍需证书托管，见 ROADMAP）。
- **FFI 出口基准 bench_ffi**（bench 盲区）：经 C ABI 实测载章/部件流体量/
  缓存命中/PDF 光栅化/stats，已挂 CI bench job。
- **Release GPG 签名**：release.yml 支持对 `SHA256SUMS.txt` 做 GPG 分离签名
  （仓库 Secrets 配 `GPG_PRIVATE_KEY`/`GPG_PASSPHRASE`，可选 `GPG_KEY_ID`；
  未配置自动跳过）；README 新增「Release 产物与验证」（宿主验证三步）。

- **清单剩余项批次（第三方接入面收尾）**：
  1. **轻量单章搜索**：`SearchSession::open_chapter`（与全书会话同源的扫描/坐标/
     snippet）+ `Kernel::search_chapter`（同步扫完不注册会话）+
     `rrk_search_chapter` + Dart `searchChapter`（`maxHits=0` 默认上限 1000）。
  2. **图片缩略图一条龙**：`rrk_image::thumbnail`（解码 → 等比缩 max_edge → RGBA；
     AVIF/HEIC 明确 1002、损坏 1003）+ `rrk_image_thumbnail` + Dart
     `RrkImage.thumbnail`；`image` 依赖转正式（与 workspace 同 feature 集）。
  3. **排版可取消**：`Layouter::layout_cancellable`（块粒度检查，Cancelled=1005）+
     `rrk_layout_cancel`（起点消费陈旧置位，无进行中布局时调用无副作用）+
     Dart `RrkEngine.cancelLayout`（任意线程可调）。
  4. **观测 API**：`rrk_stats`（open_books/searches/indexes/布局缓存 hits·misses/
     内容缓存计数）+ Dart `RrkEngine.statsJson`。
  5. **ROADMAP 文档**（docs/ROADMAP.md）：明确不做清单（DRM/PDF 表单签名/固定版式
     EPUB/Media Overlays/TTS 分句/内核落盘/闭源依赖）及理由、待办、语义承诺。
  6. **Bench 盲区补全 + CI 护栏**：`bench_markup`（1MB 合成 XHTML：tokenize 3.7ms /
     clean 7.8ms 实测）与 `bench_search`（全书/单章搜索 + BM25 构建）两个基准
     example，宽松预算断言；CI 新增 `bench` job（含 P1 排版判据）。
  7. **Fuzz（cargo-fuzz）**：`fuzz/` 独立包、5 个目标（zip_parse / format_open /
     markup_clean / image_probe / pdf_open，均带输入上界与契约断言）；CI 新增
     `fuzz` job（nightly + cargo-fuzz，每目标 90s，崩溃样例上传 artifact）。

- **第三方接入面批次（ABI 8 追加，符号只增）**：
  1. **进度 API**：`rrk_progress_of` / `rrk_locate`（kernel 同步提供）——按章权重的
     估算口径，供进度条与跨设备同步；Dart `progressOf/locateByPercent`。
  2. **UTF-16 偏移映射**：`rrk_map_offsets`（批量、单趟、代理对安全，方向可选）——
     Dart/JS/Java/Swift 字符串索引皆 UTF-16，标注/选区必经；Dart `mapOffsets`。
  3. **选区取文**：`rrk_slice_text`（夹到字符边界，免整章拉回）；Dart `sliceText`。
  4. **C 头文件 `bindings/rrk.h`**（38 个导出手工同步）与
     **《FFI 调用约定与线程安全》契约文档**（缓冲所有权 / 错误模型 / 线程安全
     矩阵 / 坐标系速查），README 文档索引已挂。
  5. **图片格式缺口修复（CBZ 白名单 vs 解码能力不对齐）**：`image` 启用纯 Rust
     `tiff` 解码 feature；`rrk-image` 头部探测新增 TIFF（IFD0）与 AVIF/HEIC
     （ISO-BMFF `ispe`）——这三类页尺寸不再为 0；`render_page` 对内核不可解码的
     AVIF/HEIC 明确报 UnsupportedFormat（1002，宿主回落模式 A 取原始字节），
     不再混入 CorruptData；`ImageFormat` 新增 Tiff/Avif/Heic（码值 5/6/7 只增）。
  - 验证：cargo test --workspace 119 通过；dart analyze 无问题；release DLL +
    Dart 端到端：进度往返、UTF-16 映射代理对往返、选区夹取、命中统计均正确。

- **稳定错误码透出 FFI（issue #11，ABI 7 → 8）**：新增 `rrk_last_error_code() -> u32`，
  返回与 `rrk_last_error()` 消息同源的 `RrkErrorKind::code()`（1001–1999 只增不改，
  panic=2001，0=尚无记录）；Dart 封装同步（`_lastError` 不再硬编码 1001），
  码值表写入 README。宿主可按码映射文案，不再对消息做字符串匹配。
- **内容级解析结果缓存（issue #12，ABI 8）**：`rrk-cache` 新增内容级 key
  （`CacheKey::content(content_hash, kind)`，kind = Toc / ChapterText / TxtOutline，
  style/viewport 恒 0）；`rrk-kernel` 新增 `toc_cached / chapters_cached /
  load_chapter_cached`（payload 为自研紧凑二进制，magic+schema 自校验，损坏自动
  重算）与命中统计（`content_cache_stats`）；FFI 暴露
  `rrk_content_cache_configure` / `rrk_content_cache_stats`，`rrk_toc_json` /
  `rrk_chapters_json` / `rrk_load_chapter` 自动走缓存——命中即免 inflate+清洗+分块，
  同文件重开命中磁盘、内容 hash 变了整本失效（与宿主原 Dart 三层缓存的
  mtime/size 判据等价，宿主侧 `structureCache/txtOutlineCache/chapterTextCache`
  可正式下线）。
- **全链路性能批次（`perf(core)` 6105ab1）**：FFI 锁范围收敛（排版/光栅/载章出锁）、
  page_at/hit_test/prefetch 直连结构化数据免整章 JSON、CBZ 开书限流读头、
  EPUB 章清洗 LRU、清洗层零拷贝（`Token<'a>`）、搜索免折叠副本、BM25 扁平 CSR、
  PDF 字体跨页缓存等（详见提交说明）。

### Fixed

- **CBZ 页表口径与宿主基线对齐（issue #10）**：`CbzBook::parse` 的页收集改为
  归一化 → 过滤 → 去重 → 自然序，四条对齐宿主 `CbzSource`：
  1. **条目名先归一化**（`\`→`/`、去 `./` 与前导 `/`、压 `//`、处理 `..`、去尾 `/`），
     过滤与 `chapters()` 的 `href` 都用归一化名；
  2. **过滤垃圾条目**：`__MACOSX/`（任意层级，Finder 压缩的 CBZ 必带
     `__MACOSX/._page1.jpg`）与 basename 以 `.` 开头的隐藏文件；
  3. **扩展名白名单补全**（`png,jpeg,jpg,jpe,gif,bmp,webp,apng,tif,tiff,avif,heic`）——
     原白名单缺 `.tif`/`.heic`/`.jpe`/`.apng`，这类页此前被内核**整页丢弃**（用户可见回归）；
  4. **大小写撞名去重**（`Page1.jpg` 与 `page1.jpg` 只收 zip 物理序首件），
     `natural_cmp` 改大小写不敏感 + 数值相同则前导零少者在前（`p7.jpg` < `p007.jpg`）。
  - 新增 `mime_of_name`：`tif/tiff → image/tiff`、`avif → image/avif`、`heic → image/heic`
    （此前一律回退 `image/jpeg`）。宿主侧适配层的临时过滤/重排可简化为纯透传。
  - 7 项 fixture 用例（自建最小 stored zip，不引新依赖）：白名单 / 垃圾过滤 / 去重 /
    排序 / 归一化 / 端到端页表。

## [0.1.2] - 2026-10-23

**ABI 6 → 7**：Dart 封装（`kRrkAbiVersion = 7`）与预编译原生库必须同步升级
（6 = 图像出口，7 = 检索/索引出口）。

### Added

- **检索与索引（issue #9）**：新增 `rrk-search` crate + 内核会话表 + FFI/Dart 出口。
  - **全书搜索**（`SearchSession`）：句柄级会话、**拉模式分块**（宿主按批取命中，
    进度随批返回 `scanned/total`）、`cancel` 置原子标志即真停（无后台线程、内存随
    引用释放）；每次只持有一章的扁平块，跨 FFI 只传批次小结构。
  - **BM25 索引**（`Bm25Builder`/`Bm25`）：以句柄为输入逐章投喂（`step` 分批、可随时
    放弃），产出**自描述序列化字节**（magic `RRKI` + LEB128 varint postings），
    内核不写盘；`Bm25::decode`/`rrk_bm25_load` 支持宿主落盘后跨启动复用。
  - **文本真值**：两者统一消费 `flatten_blocks_spans`（与 FFI `blocks_json` 同源），
    保证「搜到的 = 读到的」；`rrk-search::search_text` 去掉行内图片标记后喂索引。
  - **偏移坐标系**：命中偏移 = `ChapterContent.text` 的 UTF-8 字节偏移，与目录锚点
    (`TocNode.anchor`)、内链 (`link_ranges`)、进度 (`SemanticPos`) 同一坐标系；
    `rrk_model::PlainTextBlock::to_source` 负责「段落文本偏移 → 章正文偏移」回映。
  - **分词**：CJK 连续段取 bigram（单字退化为 unigram）、西文/数字按词块，
    仅做 ASCII 大小写折叠（不改变字节长度，故偏移映射不受影响）。
  - FFI：`rrk_search_open/next/cancel`、`rrk_bm25_open/load/step/pull/query/close`；
    Dart：`RrkBook.search()`/`buildIndex()`、`RrkSearchSession`、`RrkBm25Index`
    （含 `pullAll` 分块拉取与 `load` 往返）。
  - CLI：`rrk-inspect --search <词>` / `--bm25 <词>`。

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
