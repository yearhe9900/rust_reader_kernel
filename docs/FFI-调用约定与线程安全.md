# FFI 调用约定与线程安全（C ABI 契约）

> 适用范围：`rrk-ffi` 导出的全部符号（头文件见 [`bindings/rrk.h`](../bindings/rrk.h)）。
> 本文是宿主接入的**契约文档**：与 ABI 版本、错误码表一样，只增不改语义。

## 1. 缓冲所有权

- **字符串入参**一律 `(ptr, len)` 两元组（UTF-8，不要求 NUL 结尾）；
- **缓冲出参**一律 `(out_ptr, out_len)`：内核分配，调用方用完必须
  `rrk_buf_free(ptr, len)` 释放（JSON / 正文 / 位图 / 索引分块一视同仁）；
  **v12 起 `len` 不再参与释放**：内核在数据区前自带 8 字节 header 记录长度，
  传错/传 0/传旧值都安全（参数保留仅为不破签名与旧宿主源码兼容）。
  宿主不得自行对 `ptr` 做偏移或按 `len` 反推分配布局；
- `rrk_open_bytes` 的整本书数据同样只在调用期间读取：内核**拷贝**到自有
  `Arc<[u8]>` 后再返回，调用返回后宿主即可释放传入缓冲；
- 传入的指针只在调用期间被读取，内核不持有、不释放宿主指针；
- `rrk_buf_free(NULL, _)` 是安全的空操作；`rrk_buf_free` 可在任意线程调用。

## 2. 错误模型

- 返回码 `0` = 成功；非 `0` = **稳定错误码**（`RrkErrorKind::code`，
  1001–1999 只增不改，panic 拦截 = 2001，码值表见 README）；
- 细节消息与最近一次错误码经 `rrk_last_error()` / `rrk_last_error_code()` 读取；
- ⚠️ **二者都是 thread-local**：必须在失败调用的**同一线程**上立刻读取，
  不能跨线程传递「错误上下文」后再取；
- 软结果不算错误：如 `rrk_cover` 无封面返回 `2` 且不产出缓冲；
- **内核不锁死**（v11）：内核内部锁采用「中毒恢复」——即使某次调用内部
  panic 被 2001 拦截，引擎也会自动恢复继续服务，不会出现不可用状态；
  宿主无需（也无法）做任何解锁动作。

## 3. 句柄模型

- 句柄（`uint64_t`）从 `rrk_open` / `rrk_open_bytes` / `rrk_search_open` /
  `rrk_bm25_open` / `rrk_bm25_load` 返回，`0` 永远非法；
- 句柄**不保证跨进程**；同一进程内可自由跨线程传递；
- `rrk_close` 幂等语义：重复关闭同一句柄报 `InvalidParam`（1004），不崩溃；
- 关闭后继续使用旧句柄 → `InvalidParam`，不会段错误（句柄表校验）。
  句柄内置 **generation**（v11）：关 A 再开 B 即使复用同一内部槽位，句柄值
  也不同，旧句柄不会静默指向他书（ABA 防护）；

## 4. 线程安全

**总契约：所有导出函数都可从任意线程调用（句柄可跨线程共享）；不会出现数据
竞争或崩溃，超出的并发度由内核内部锁串行化。** 具体内情：

| 资源 | 并发行为 |
|------|---------|
| 句柄表（open/close/查询） | 内核级互斥锁串行化；**重活（载章/开书/检索扫描/排版/光栅化/磁盘 IO）已移出该锁**，互相不阻塞（v11 落实：`rrk_open`/`rrk_load_chapter`/`rrk_search_chapter` 均为「锁内取书源 → 锁外干活 → 锁内回填」三段式） |
| 排版器（parley FontContext） | **池化**（v9 引入，v12 改调度）：min(可用并行度, 4) 台，**从轮转起点 `try_lock` 找空闲槽**，全忙才排队；并行排版吞吐随池叠加，单台内互斥 |
| 布局/内容缓存 | 互斥锁保护；命中路径 O(1) |
| 检索会话 | 会话独立锁：`rrk_search_next` 在会话锁内扫描；另一线程 `rrk_search_cancel` **无需该锁即可即时置位**，扫描在下一个检查点返回 `Cancelled`（1005） |
| BM25 构建 | 任务独立锁，按 `rrk_bm25_step` 分步推进，宿主控制节奏 |
| PDF/CBZ 页渲染 | 文档级内部结构自带锁（PDF 对象缓存、字体缓存、图像解码缓存；CBZ 渲染缓存）：可多线程并发渲染不同页 |
| 排版取消 | 每次布局请求独立的取消令牌（v11）：`rrk_layout_cancel` 可从任意线程调用，置位**所有**在飞布局，只对已开始的布局生效（块粒度，以 `Cancelled`（1005）返回），不影响后续新布局；多窗口/预取只想取消单个窗口时用 `rrk_layout_cancel_book(handle)`，不误杀其它请求 |

**实践建议**：

- UI 线程做轻查询、后台线程做排版/渲染/检索混跑没有问题；
- 多章并行排版（双窗口/预取）自 v9 起随排版器池叠加吞吐；池大小
  `min(可用并行度, 4)` 惰性创建，暂不提供配置面（未来若需要会另行版本化）；
  v12 起按空闲槽调度，忙槽不再让后来请求空等；
- 错误读取（第 2 节）是唯一「必须同线程」的约束。

## 5. ABI 与版本策略

- 启动时先调 `rrk_abi_version()` 与宿主编译期常量**相等校验**（Dart 封装
  `RrkEngine.init` 已内置）；
- 策略：**同一 ABI 号内符号与语义只增不改**；删除 / 改签名 / 改输出格式
  只随 ABI bump（v9 的 blocks 部件流即属此类）。`RrkEngine.init` 是相等
  校验、不匹配直接抛错，因此 **ABI bump 时宿主封装与预编译原生库必须同时
  升级**，没有灰度窗口；磁盘缓存 payload 因自带 schema 号可自动失效重算；
- 错误码、图像格式码（`rrk_image_dimensions` 的 format）、页级 kind 码同样
  只增不改；
- 缓存 payload 自带 schema 号，内核升级自动丢弃旧缓存重算（无需宿主迁移）。

**缓存配置面（v12）**：

| 出口 | 语义 |
|------|------|
| `rrk_cache_configure(dir, mem_bytes)` | 三要素布局缓存（C1 内存 LRU + C2 磁盘）；空路径 = 关闭 |
| `rrk_content_cache_configure(dir, mem_bytes)` | 内容级缓存（目录 / 章文本 / 大纲）；空路径 = 关闭 |
| `rrk_open_index_cache_configure(dir)` | **独立**的 open 索引缓存开关 / 目录；未调用时跟随内容级缓存（旧行为），调用后不再联动；空路径 = 显式关闭 |
| `rrk_cache_set_disk_limit(max_bytes)` | 进程级磁盘配额，0 = 关闭裁剪；下次 `save` 起每个缓存目录按 mtime 裁剪到该上限。**惰性生效**，不主动扫描历史目录 |

布局 / 内容级 / open 索引三类缓存默认均不落盘：内核不擅自创建缓存目录，
落盘策略由宿主显式开启（红线）。配额出口只设上限、不会自己创建目录。

**排版参数面（v13，破坏性）**：

四个排版出口（`rrk_pages_json` / `rrk_page_at` / `rrk_hit_test` /
`rrk_prefetch`）的签名由「9 个排版参数」改为「`(style, viewport)` 两个句柄」：

| 出口 | 语义 |
|------|------|
| `rrk_style_create(font, size, lh, indent, align)` | 创建样式句柄（**0 = 失败**，错误详情仍走 `rrk_last_error`）；`rrk_style_destroy` 释放 |
| `rrk_style_set_font_family` / `_font_size` / `_line_height` / `_para_indent_em` / `_align` | 改样式字段——**加新样式字段 = 加 setter，四个出口签名不动**（这正是句柄化的目的） |
| `rrk_viewport_create(w, h, dpr)` / `rrk_viewport_destroy` | 创建/释放视口句柄 |
| `rrk_viewport_set_size` / `_set_dpr` | 改视口（窗口旋转 / 分屏，不重建句柄） |

要点：

- 句柄**不透明**（`uint64_t`），宿主不解释其内部；`0` 永远非法；
- 句柄同样带 **generation**（ABA 防护）：销毁后槽位被复用，旧句柄值不同、
  且旧句柄**不再有效**（返回错误，不是读到别人的样式）；
- 建议宿主**整本复用同一份样式/视口句柄**：改设置 = 调 setter，不要重建；
- 句柄表是进程级全局，可跨线程使用（内部互斥保护）；
- v12 及以前的四个出口签名已**移除**——宿主必须同步升级封装与原生库
  （`RrkEngine.init` 相等校验，无灰度窗口）。

## 6. 坐标系速查

| 坐标 | 定义 | 相互转换 |
|------|------|---------|
| 章文本字节偏移 | UTF-8 字节，`ChapterContent.text` 内 | 内核原生坐标（锚点/内链/搜索命中同系） |
| UTF-16 偏移 | code unit（Dart/JS/Java/Swift 字符串索引） | `rrk_map_offsets`（代理对安全） |
| 语义坐标 | `(chapter, 章内字节偏移)` | `rrk_locate` / `rrk_progress_of` 互转（估算口径，进度条用；精确定位用 `page_at`/`hit_test`） |
| 页坐标 | `(page_index, 页内 x/y, 逻辑像素)` | `rrk_hit_test` / `rrk_page_at` |
