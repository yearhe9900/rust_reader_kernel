/* rrk.h — rust_reader_kernel C ABI（rrk_ffi）头文件。
 *
 * 维护方式：与 crates/rrk-ffi/src/lib.rs 的 `extern "C"` 导出手工同步（每个
 * ABI 版本随提交核对一遍）。签名规则：
 *   - 字符串入参 = (const uint8_t* ptr, uintptr_t len)，UTF-8，非 NUL 结尾；
 *   - 缓冲出参   = (uint8_t** out_ptr, uintptr_t* out_len)，内核分配，
 *     调用方用 rrk_buf_free 释放；
 *   - 返回码     = 0 成功；非 0 = RrkErrorKind 稳定码（1001–1999，见 README），
 *     详情用 rrk_last_error / rrk_last_error_code（**线程局部**：必须在失败的
 *     同一线程上读取）。
 *
 * 线程安全与调用约定详见 docs/FFI-调用约定与线程安全.md。
 */
#ifndef RRK_FFI_H
#define RRK_FFI_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ---- 基础 ---- */

/* ABI 版本（宿主启动时必须校验相等；只增不改语义）。 */
uint32_t rrk_abi_version(void);

/* 释放内核分配的缓冲（所有 out_ptr/out_len 出参都用它释放）。线程安全。 */
void rrk_buf_free(uint8_t* ptr, uintptr_t len);

/* 最近一次错误消息（UTF-8；线程局部）。 */
uint32_t rrk_last_error(uint8_t** out_ptr, uintptr_t* out_len);

/* 最近一次错误的稳定码：1001 Io / 1002 UnsupportedFormat / 1003 CorruptData /
 * 1004 InvalidParam / 1005 Cancelled / 1006 ResourceLimit / 1999 Internal /
 * 2001 panic / 0 = 本线程尚无记录。 */
uint32_t rrk_last_error_code(void);

/* ---- 书生命周期与查询 ---- */

/* 打开一本书（UTF-8 路径）；成功返回句柄（>=1），失败返回 0。 */
uint64_t rrk_open(const uint8_t* path, uintptr_t path_len);
/* 关闭一本书（幂等）。 */
uint32_t rrk_close(uint64_t handle);
/* 书目元数据 JSON。 */
uint32_t rrk_meta_json(uint64_t handle, uint8_t** out_ptr, uintptr_t* out_len);
/* 目录树 JSON（走内容级缓存）。 */
uint32_t rrk_toc_json(uint64_t handle, uint8_t** out_ptr, uintptr_t* out_len);
/* 章索引 JSON（走内容级缓存）。 */
uint32_t rrk_chapters_json(uint64_t handle, uint8_t** out_ptr, uintptr_t* out_len);
/* 加载一章：正文 UTF-8 缓冲 + 块结构 JSON + 内链 JSON（走内容级缓存）。 */
uint32_t rrk_load_chapter(uint64_t handle, uint32_t chapter_id,
                          uint8_t** text_ptr, uintptr_t* text_len,
                          uint8_t** blocks_ptr, uintptr_t* blocks_len,
                          uint8_t** links_ptr, uintptr_t* links_len);
/* 封面图字节；无封面返回 2 且不产出缓冲。 */
uint32_t rrk_cover(uint64_t handle, uint8_t** out_ptr, uintptr_t* out_len);
/* 书类型：0=Text 1=ImageGallery 2=Paged；非法句柄 u32::MAX。 */
uint32_t rrk_book_kind(uint64_t handle);
/* 页数（仅页级格式）。 */
uint32_t rrk_page_count(uint64_t handle, uint32_t* out);
/* 读一页（页级）：JSON。 */
uint32_t rrk_load_page_json(uint64_t handle, uint32_t index,
                            uint8_t** out_ptr, uintptr_t* out_len);
/* 按 key 取资源原始字节（CBZ 页图 / EPUB 图片等）。 */
uint32_t rrk_load_resource(uint64_t handle, const uint8_t* key_ptr, uintptr_t key_len,
                           uint8_t** out_ptr, uintptr_t* out_len);
/* 整页栅格化（页级格式，模式 B）→ RGBA 缓冲 + 尺寸。 */
uint32_t rrk_render_page(uint64_t handle, uint32_t page_index, uint32_t target_width,
                         uint8_t** out_ptr, uintptr_t* out_len,
                         uint32_t* out_w, uint32_t* out_h);

/* ---- 缓存 ---- */

/* 布局缓存（三要素 key：内容×样式×视口；空目录 = 关闭）。 */
uint32_t rrk_cache_configure(const uint8_t* dir_ptr, uintptr_t dir_len, uintptr_t mem_bytes);
/* 内容级缓存（目录/章文本/大纲；key 只含内容 hash；空目录 = 关闭）。 */
uint32_t rrk_content_cache_configure(const uint8_t* dir_ptr, uintptr_t dir_len,
                                     uintptr_t mem_bytes);
/* 内容级缓存命中统计 JSON：{"toc","chapter","outline"} 各 {hits,misses}。 */
uint32_t rrk_content_cache_stats(uint8_t** out_ptr, uintptr_t* out_len);

/* ---- 进度 / 坐标 / 选区 ---- */

/* 语义坐标 -> 全书进度 [0,1]（估算口径：按章权重）。 */
uint32_t rrk_progress_of(uint64_t handle, uint32_t chapter, uint32_t byte_offset,
                         float* out_percent);
/* 进度 [0,1] -> (chapter, 章内字节偏移)（估算口径）。 */
uint32_t rrk_locate(uint64_t handle, float percent,
                    uint32_t* out_chapter, uint32_t* out_offset);
/* 批量偏移映射：章文本 UTF-8 字节偏移 <-> UTF-16 code unit 偏移。
 * direction: 0 = 字节->UTF-16（非边界向下取整）；1 = UTF-16->字节（代理对中间取对首）。
 * in/out 均为 count 个小端 u32。 */
uint32_t rrk_map_offsets(uint64_t handle, uint32_t chapter, uint32_t direction,
                         const uint32_t* in_ptr, uintptr_t count,
                         uint8_t** out_ptr, uintptr_t* out_len);
/* 选区取文：[start,end) 夹到字符边界后返回章文本切片（UTF-8 缓冲）。 */
uint32_t rrk_slice_text(uint64_t handle, uint32_t chapter, uint32_t start, uint32_t end,
                        uint8_t** out_ptr, uintptr_t* out_len);

/* ---- 排版与进度（重排类） ---- */

/* style 字符串字段：font_ptr/font_len（UTF-8 字体名，可空）、font_size、line_height、
 * para_indent_em、align(0=Start 1=Center 2=End 3=Justify)、width/height/dpr（逻辑像素）。 */

/* 排版一章 -> 页 JSON（三要素缓存口径）。 */
uint32_t rrk_pages_json(uint64_t handle, uint32_t chapter_id,
                        const uint8_t* font_ptr, uintptr_t font_len,
                        float font_size, float line_height, float para_indent_em,
                        uint32_t align, float width, float height, float dpr,
                        uint8_t** out_ptr, uintptr_t* out_len);
/* 语义坐标 -> 页下标（进度恢复）。 */
uint32_t rrk_page_at(uint64_t handle, uint32_t chapter_id, uint32_t char_offset,
                     const uint8_t* font_ptr, uintptr_t font_len,
                     float font_size, float line_height, float para_indent_em,
                     uint32_t align, float width, float height, float dpr,
                     uint32_t* out_page);
/* 页内命中测试：(页内 x,y) -> 语义偏移 JSON。 */
uint32_t rrk_hit_test(uint64_t handle, uint32_t chapter_id, uint32_t page_index,
                      float x, float y,
                      const uint8_t* font_ptr, uintptr_t font_len,
                      float font_size, float line_height, float para_indent_em,
                      uint32_t align, float width, float height, float dpr,
                      uint8_t** out_ptr, uintptr_t* out_len);
/* 远跳预取：预热本章 + 相邻章布局。 */
uint32_t rrk_prefetch(uint64_t handle, uint32_t chapter_id,
                      const uint8_t* font_ptr, uintptr_t font_len,
                      float font_size, float line_height, float para_indent_em,
                      uint32_t align, float width, float height, float dpr);

/* ---- 图像工具（纯字节，与书句柄无关） ---- */

/* 只读图片头取尺寸：JSON {"width","height","format","mime"}；
 * format 码 0=png 1=jpeg 2=gif 3=bmp 4=webp 5=tiff 6=avif 7=heic（只增不改）。 */
uint32_t rrk_image_dimensions(const uint8_t* data, uintptr_t len,
                              uint8_t** out_ptr, uintptr_t* out_len);
/* BGRA 原始位图 -> PNG。stride = 每行字节数（>= width*4）。 */
uint32_t rrk_encode_png_bgra(uint32_t width, uint32_t height, uintptr_t stride,
                             const uint8_t* src, uintptr_t src_len,
                             uint8_t** out_ptr, uintptr_t* out_len);

/* ---- 全书检索（拉模式，可取消） ---- */

/* 建立检索会话；成功 handle>=0 语义的 session id（0 = 失败）。 */
uint64_t rrk_search_open(uint64_t handle, const uint8_t* query_ptr, uintptr_t query_len,
                         uint32_t case_sensitive, uint32_t context_chars);
/* 拉取一批命中（JSON）；另一线程可随时 rrk_search_cancel。 */
uint32_t rrk_search_next(uint64_t search_id, uint32_t max_hits,
                         uint8_t** out_ptr, uintptr_t* out_len);
/* 取消检索（即时置位，不留后台线程）。 */
uint32_t rrk_search_cancel(uint64_t search_id);

/* ---- BM25 索引（分步构建，宿主拉取落盘） ---- */

uint64_t rrk_bm25_open(uint64_t handle);
/* 从宿主落盘的索引字节载入（跨启动复用）。 */
uint64_t rrk_bm25_load(const uint8_t* data_ptr, uintptr_t data_len);
/* 分组推进构建；返回进度 JSON（反复调用直到 done=true）。 */
uint32_t rrk_bm25_step(uint64_t index_id, uint32_t max_chapters,
                       uint8_t** out_ptr, uintptr_t* out_len);
/* 拉取序列化索引字节的分块；out_next 回填下一 offset（==总长即取完）。 */
uint32_t rrk_bm25_pull(uint64_t index_id, uint64_t offset, uintptr_t max_bytes,
                       uint8_t** out_ptr, uintptr_t* out_len, uint64_t* out_next);
/* 索引查询（JSON）。 */
uint32_t rrk_bm25_query(uint64_t index_id, const uint8_t* query_ptr, uintptr_t query_len,
                        uint32_t top_k, uint8_t** out_ptr, uintptr_t* out_len);
uint32_t rrk_bm25_close(uint64_t index_id);

#ifdef __cplusplus
}
#endif

#endif /* RRK_FFI_H */
