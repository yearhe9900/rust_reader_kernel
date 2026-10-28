#ifndef RRK_FFI_H
#define RRK_FFI_H

/* 自动生成：cbindgen crates/rrk-ffi --config cbindgen.toml；勿手改。 */

#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>

// ABI 版本号（Dart 启动校验；只增不改语义）。
// v8 = `rrk_last_error_code` / `rrk_content_cache_*`（issue #11、#12）；
// v9 = blocks JSON 改**部件流**（文本区间+图片 key，不再内嵌正文）+
//      `rrk_search_chapter` / `rrk_image_thumbnail` / `rrk_layout_cancel` /
//      `rrk_stats` / 进度与坐标 API / 排版器池（并行排版）；
// v10 = `rrk_load_headings`（issue #13：章标题块轻量出口，另 EPUB 无 TOC 书
//       open 期自动回填首个最小级标题）。
#define RRK_ABI_VERSION 10

// panic 被捕获时的错误码。
#define ERR_PANIC 2001

// 书句柄（0 = 非法）。本地定义（与 `rrk_kernel::BookHandle` 透明等价）：
// cbindgen `parse_deps=false` 看不到依赖 crate 里的别名，必须在本 crate 可见。
typedef uint64_t BookHandle;

// 检索会话句柄（0 = 非法）。
typedef uint64_t SearchId;

// BM25 索引会话句柄（0 = 非法）。
typedef uint64_t IndexId;

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

// 释放内核分配的缓冲（Dart 侧用完必调）。
void rrk_buf_free(uint8_t *ptr, uintptr_t len);

// 返回 ABI 版本号。
uint32_t rrk_abi_version(void);

// 取最近一次错误信息（UTF-8；调用方负责 `rrk_buf_free`）。
uint32_t rrk_last_error(uint8_t **out_ptr, uintptr_t *out_len);

// 取最近一次错误的**稳定错误码**（issue #11）：`RrkErrorKind::code()`
// （1001–1999，只增不改，码值表见 README）；panic = 2001；0 = 本线程尚无错误。
// 与 `rrk_last_error` 的消息来自同一次错误（设置时一并写入）。
uint32_t rrk_last_error_code(void);

// 打开一本书（UTF-8 路径）；成功返回句柄（≥1），失败返回 0。
uint64_t rrk_open(const uint8_t *path, uintptr_t path_len);

// 关闭一本书；0 = 成功。
uint32_t rrk_close(BookHandle handle);

// 书目元数据 JSON；0 = 成功（缓冲由调用方 `rrk_buf_free` 释放）。
uint32_t rrk_meta_json(BookHandle handle, uint8_t **out_ptr, uintptr_t *out_len);

// 目录 JSON；0 = 成功。
uint32_t rrk_toc_json(BookHandle handle, uint8_t **out_ptr, uintptr_t *out_len);

// 章索引 JSON；0 = 成功。
uint32_t rrk_chapters_json(BookHandle handle, uint8_t **out_ptr, uintptr_t *out_len);

// 加载一章：正文 UTF-8 缓冲 + 块结构 JSON；0 = 成功。
//
// `text_ptr/text_len` 与 `blocks_ptr/blocks_len` 都由调用方 `rrk_buf_free` 释放。
uint32_t rrk_load_chapter(BookHandle handle,
                          uint32_t chapter_id,
                          uint8_t **text_ptr,
                          uintptr_t *text_len,
                          uint8_t **blocks_ptr,
                          uintptr_t *blocks_len,
                          uint8_t **links_ptr,
                          uintptr_t *links_len);

// 封面图（无封面返回 code=2 且不产出缓冲）；0 = 成功。
uint32_t rrk_cover(BookHandle handle, uint8_t **out_ptr, uintptr_t *out_len);

// 整页栅格化（PDF 原版页，模式 B）→ RGBA 缓冲 + 尺寸。0 = 成功。
uint32_t rrk_render_page(BookHandle handle,
                         uint32_t page_index,
                         uint32_t target_width,
                         uint8_t **out_ptr,
                         uintptr_t *out_len,
                         uint32_t *out_w,
                         uint32_t *out_h);

// 书类型：0=Text 1=ImageGallery 2=Paged；非法句柄返回 u32::MAX。
uint32_t rrk_book_kind(BookHandle handle);

// 页数（仅页级格式；重排格式返回 Unsupported）。
uint32_t rrk_page_count(BookHandle handle, uint32_t *out);

// 读一页（页级）：图片页 → {kind:"image",key,mime}；文本页 → {kind:"text",text,blocks}。
uint32_t rrk_load_page_json(BookHandle handle,
                            uint32_t index,
                            uint8_t **out_ptr,
                            uintptr_t *out_len);

// 按 key 取资源原始字节（CBZ 页图 / EPUB 图片等；mime 由宿主嗅探）。
uint32_t rrk_load_resource(BookHandle handle,
                           const uint8_t *key_ptr,
                           uintptr_t key_len,
                           uint8_t **out_ptr,
                           uintptr_t *out_len);

// 配置三要素缓存（目录 + 内存上限字节；传空路径 = 关闭磁盘缓存）。
uint32_t rrk_cache_configure(const uint8_t *dir_ptr, uintptr_t dir_len, uintptr_t mem_bytes);

// 配置**内容级**缓存（issue #12：目录树 / 章文本 / 章大纲；命中免重新解析）。
//
// key 只含 `content_hash`（不随样式/视口变）：内容变了整本失效，同文件重开
// 命中；空路径 = 关闭。与布局缓存（`rrk_cache_configure`）互不干扰。
uint32_t rrk_content_cache_configure(const uint8_t *dir_ptr,
                                     uintptr_t dir_len,
                                     uintptr_t mem_bytes);

// 内容级缓存命中统计 JSON（issue #12：可观测性）：
// `{"toc":{"hits","misses"},"chapter":{...},"outline":{...}}`。
uint32_t rrk_content_cache_stats(uint8_t **out_ptr, uintptr_t *out_len);

// 汇总观测统计（第三方接入面）：
// `{"open_books","searches","indexes","layout":{"hits","misses"},"content":{…}}`。
// 排版计数为进程级累计（不随书关闭清零）；内容缓存计数见
// [`rrk_content_cache_stats`]（可清零）。
uint32_t rrk_stats(uint8_t **out_ptr, uintptr_t *out_len);

// 语义坐标 → 全书进度百分比 [0,1]（估算口径，见 `Kernel::progress_of`）。
uint32_t rrk_progress_of(BookHandle handle,
                         uint32_t chapter,
                         uint32_t byte_offset,
                         float *out_percent);

// 进度百分比 [0,1] → `(chapter, 章内字节偏移)`（`rrk_progress_of` 的逆）。
uint32_t rrk_locate(BookHandle handle, float percent, uint32_t *out_chapter, uint32_t *out_offset);

// 批量偏移映射：章文本 UTF-8 字节偏移 ↔ UTF-16 code unit 偏移。
//
// `direction`：0 = 字节→UTF-16，1 = UTF-16→字节；输入 `in_ptr` 为 `count` 个
// u32 小端，输出缓冲同样为 `count` 个 u32 小端（调用方 `rrk_buf_free`）。
uint32_t rrk_map_offsets(BookHandle handle,
                         uint32_t chapter,
                         uint32_t direction,
                         const uint32_t *in_ptr,
                         uintptr_t count,
                         uint8_t **out_ptr,
                         uintptr_t *out_len);

// 选区取文：`[start, end)` 夹到字符边界后返回章文本切片（UTF-8 缓冲）。
uint32_t rrk_slice_text(BookHandle handle,
                        uint32_t chapter,
                        uint32_t start,
                        uint32_t end,
                        uint8_t **out_ptr,
                        uintptr_t *out_len);

// 章标题块（issue #13 轻量出口）：JSON `[{"level":n,"text":"..."}]`，
// 走章内容级缓存（命中免解析）；无标题/图片章返回 `[]`。
uint32_t rrk_load_headings(BookHandle handle,
                           uint32_t chapter,
                           uint8_t **out_ptr,
                           uintptr_t *out_len);

// 排版一章 → 页 JSON（三要素缓存口径：命中免排版；miss 排完落盘 + 保护窗口）。
uint32_t rrk_pages_json(BookHandle handle,
                        uint32_t chapter_id,
                        const uint8_t *font_ptr,
                        uintptr_t font_len,
                        float font_size,
                        float line_height,
                        float para_indent_em,
                        uint32_t align,
                        float width,
                        float height,
                        float dpr,
                        uint8_t **out_ptr,
                        uintptr_t *out_len);

// 语义坐标 → 页下标（进度恢复；缓存命中免排版）。
uint32_t rrk_page_at(BookHandle handle,
                     uint32_t chapter_id,
                     uint32_t char_offset,
                     const uint8_t *font_ptr,
                     uintptr_t font_len,
                     float font_size,
                     float line_height,
                     float para_indent_em,
                     uint32_t align,
                     float width,
                     float height,
                     float dpr,
                     uint32_t *out_page);

// 页内命中测试：(页内 x, y) → 语义偏移（run 级粒度 + run 内比例）。
uint32_t rrk_hit_test(BookHandle handle,
                      uint32_t chapter_id,
                      uint32_t page_index,
                      float x,
                      float y,
                      const uint8_t *font_ptr,
                      uintptr_t font_len,
                      float font_size,
                      float line_height,
                      float para_indent_em,
                      uint32_t align,
                      float width,
                      float height,
                      float dpr,
                      uint8_t **out_ptr,
                      uintptr_t *out_len);

// 请求取消进行中的排版（尽力取消，块粒度）：`rrk_pages_json` 在块间检查并以
// `Cancelled`（1005）返回；布局未开始时的置位会在下次布局起点被消费，无副作用。
uint32_t rrk_layout_cancel(void);

// 远跳预取：预热本章布局 + 保护窗口（当前章 + 相邻章）。
uint32_t rrk_prefetch(BookHandle handle,
                      uint32_t chapter_id,
                      const uint8_t *font_ptr,
                      uintptr_t font_len,
                      float font_size,
                      float line_height,
                      float para_indent_em,
                      uint32_t align,
                      float width,
                      float height,
                      float dpr);

// 只读图片头取尺寸：成功 → `{"width","height","format","mime"}` + code 0；
// 无法识别（含空指针/畸形）→ code 2，不产出缓冲。
//
// **不解码像素**：只扫头部（PNG IHDR / JPEG SOFn / GIF / BMP / WebP chunk），
// 供宿主做行内插图测量与页尺寸表，多 MB 图片也不产生分配。
uint32_t rrk_image_dimensions(const uint8_t *data,
                              uintptr_t len,
                              uint8_t **out_ptr,
                              uintptr_t *out_len);

// BGRA 原始位图 → PNG 字节（code 0 = 成功，缓冲由调用方 `rrk_buf_free` 释放）。
//
// `src/src_len` 为**调用方持有**的位图缓冲（内核只读、不接管所有权，零拷贝入参）；
// `stride` 传 0 = 紧凑 `width*4`。逐行 BGRA→RGBA 后直接 deflate，不打整幅中间缓冲。
uint32_t rrk_encode_png_bgra(uint32_t width,
                             uint32_t height,
                             uintptr_t stride,
                             const uint8_t *src,
                             uintptr_t src_len,
                             uint8_t **out_ptr,
                             uintptr_t *out_len);

// 解码整图并等比缩到 `max_edge`（长边）→ RGBA 缓冲 + 尺寸（第三方接入面）。
//
// 原图已在 `max_edge` 内则不缩仅解码；AVIF/HEIC 等内核不解像素的格式报
// `UnsupportedFormat`（1002，宿主取原始字节自行解码）。缓冲由 `rrk_buf_free` 释放。
uint32_t rrk_image_thumbnail(const uint8_t *data,
                             uintptr_t len,
                             uint32_t max_edge,
                             uint8_t **out_ptr,
                             uintptr_t *out_len,
                             uint32_t *out_w,
                             uint32_t *out_h);

// 建立检索会话；成功返回 `search_id`（≥1），失败返回 0。
//
// `case_sensitive` 非 0 = 区分大小写（仅 ASCII 折叠）；`context_chars` = 命中片段左右
// 各取多少字节上下文（0 用默认 30）。
uint64_t rrk_search_open(BookHandle handle,
                         const uint8_t *query_ptr,
                         uintptr_t query_len,
                         uint32_t case_sensitive,
                         uint32_t context_chars);

// 拉取一批命中（**拉模式分块**）；批次 JSON 见 `search_page_json`。
//
// 扫描在**会话锁**内进行、内核锁已提前释放，故另一线程的 `rrk_search_cancel`
// 能立刻置位并让本调用在下一个检查点返回 `Cancelled`（1005）。
uint32_t rrk_search_next(SearchId search_id,
                         uint32_t max_hits,
                         uint8_t **out_ptr,
                         uintptr_t *out_len);

// 取消检索（置标志 + 摘除会话；内存随最后一个引用释放，**不留后台线程**）。
uint32_t rrk_search_cancel(SearchId search_id);

// 轻量**单章**搜索（第三方接入面）：不注册会话，同步扫完返回整页命中 JSON
// （结构与 `rrk_search_next` 的批次一致，坐标同源）。
// `max_hits = 0` 取内核默认上限（1000）。
uint32_t rrk_search_chapter(BookHandle handle,
                            uint32_t chapter,
                            const uint8_t *query_ptr,
                            uintptr_t query_len,
                            uint32_t case_sensitive,
                            uint32_t context_chars,
                            uint32_t max_hits,
                            uint8_t **out_ptr,
                            uintptr_t *out_len);

// 开一个 BM25 索引会话；成功返回 `index_id`（≥1），失败返回 0。
uint64_t rrk_bm25_open(BookHandle handle);

// 从宿主落盘的索引字节载入（跨启动复用）；成功返回 `index_id`，失败返回 0。
uint64_t rrk_bm25_load(const uint8_t *data_ptr,
                       uintptr_t data_len);

// 分组推进索引构建（每次最多 `max_chapters` 章）；返回进度 JSON。
//
// 宿主反复调用直到 `done=true`，期间可随时停手/放弃（`rrk_bm25_close`）。
uint32_t rrk_bm25_step(IndexId index_id,
                       uint32_t max_chapters,
                       uint8_t **out_ptr,
                       uintptr_t *out_len);

// 拉取序列化索引字节的分块：从 `offset` 起取至多 `max_bytes` 字节，
// `out_next` 回填下一个 offset（== 总长即取完）。**内核不写盘，落盘归宿主。**
uint32_t rrk_bm25_pull(IndexId index_id,
                       uint64_t offset,
                       uintptr_t max_bytes,
                       uint8_t **out_ptr,
                       uintptr_t *out_len,
                       uint64_t *out_next);

// BM25 查询：返回按得分降序的前 `top_k` 章 `[{"chapter","score"}]`。
uint32_t rrk_bm25_query(IndexId index_id,
                        const uint8_t *query_ptr,
                        uintptr_t query_len,
                        uint32_t top_k,
                        uint8_t **out_ptr,
                        uintptr_t *out_len);

// 关闭索引会话（释放索引内存）。
uint32_t rrk_bm25_close(IndexId index_id);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* RRK_FFI_H */
