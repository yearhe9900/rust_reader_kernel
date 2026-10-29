// rrk_reader：Rust 阅读内核的 Dart 封装（手写 dart:ffi，直绑 C ABI）。
//
// 桥接口径（方案 §5）：
// - 句柄化：书 = u64 句柄；结构化数据走 JSON 缓冲，正文走原始 UTF-8 缓冲；
// - 错误传递：函数返回错误码（0=成功），详情经 rrk_last_error 取；
// - 资源释放：内核分配的缓冲由本封装自动 free，RrkBook.close() 必调（有 Finalizer 兜底）。

import 'dart:convert';
import 'dart:ffi';
import 'dart:io' show Platform;
import 'dart:typed_data';

import 'package:ffi/ffi.dart';

/// ABI 版本（与 rrk-ffi 的 RRK_ABI_VERSION 对齐，启动校验）。
/// v8 = rrk_last_error_code / rrk_content_cache_*（issue #11、#12）；
/// v9 = blocks JSON 改部件流（文本区间+图片 key，不再内嵌正文）等接入面扩充；
/// v10 = rrk_load_headings（issue #13：章标题块轻量出口）。
const int kRrkAbiVersion = 10;

/// 内核错误（错误码语义见 rrk_model::RrkErrorKind）。
class RrkException implements Exception {
  RrkException(this.code, this.message);
  final int code;
  final String message;
  @override
  String toString() => 'RrkException($code): $message';
}

// ---------------------------------------------------------------------------
// C ABI 绑定
// ---------------------------------------------------------------------------

typedef _OpenNative = Uint64 Function(Pointer<Utf8>, IntPtr);
typedef _OpenDart = int Function(Pointer<Utf8>, int);
typedef _CloseNative = Uint32 Function(Uint64);
typedef _CloseDart = int Function(int);
typedef _QueryNative = Uint32 Function(Uint64, Pointer<Pointer<Uint8>>, Pointer<IntPtr>);
typedef _QueryDart = int Function(int, Pointer<Pointer<Uint8>>, Pointer<IntPtr>);
typedef _ErrorNative = Uint32 Function(Pointer<Pointer<Uint8>>, Pointer<IntPtr>);
typedef _ErrorDart = int Function(Pointer<Pointer<Uint8>>, Pointer<IntPtr>);
typedef _ErrorCodeNative = Uint32 Function();
typedef _ErrorCodeDart = int Function();
typedef _ProgressNative = Uint32 Function(Uint64, Uint32, Uint32, Pointer<Float>);
typedef _ProgressDart = int Function(int, int, int, Pointer<Float>);
typedef _LocateNative = Uint32 Function(Uint64, Float, Pointer<Uint32>, Pointer<Uint32>);
typedef _LocateDart = int Function(int, double, Pointer<Uint32>, Pointer<Uint32>);
typedef _MapOffsetsNative = Uint32 Function(
    Uint64, Uint32, Uint32, Pointer<Uint32>, IntPtr, Pointer<Pointer<Uint8>>, Pointer<IntPtr>);
typedef _MapOffsetsDart = int Function(
    int, int, int, Pointer<Uint32>, int, Pointer<Pointer<Uint8>>, Pointer<IntPtr>);
typedef _SliceNative = Uint32 Function(
    Uint64, Uint32, Uint32, Uint32, Pointer<Pointer<Uint8>>, Pointer<IntPtr>);
typedef _SliceDart = int Function(
    int, int, int, int, Pointer<Pointer<Uint8>>, Pointer<IntPtr>);
typedef _HeadingsNative = Uint32 Function(
    Uint64, Uint32, Pointer<Pointer<Uint8>>, Pointer<IntPtr>);
typedef _HeadingsDart = int Function(
    int, int, Pointer<Pointer<Uint8>>, Pointer<IntPtr>);
typedef _ThumbNative = Uint32 Function(Pointer<Uint8>, IntPtr, Uint32,
    Pointer<Pointer<Uint8>>, Pointer<IntPtr>, Pointer<Uint32>, Pointer<Uint32>);
typedef _ThumbDart = int Function(Pointer<Uint8>, int, int,
    Pointer<Pointer<Uint8>>, Pointer<IntPtr>, Pointer<Uint32>, Pointer<Uint32>);
typedef _SearchChapterNative = Uint32 Function(Uint64, Uint32, Pointer<Utf8>, IntPtr,
    Uint32, Uint32, Uint32, Pointer<Pointer<Uint8>>, Pointer<IntPtr>);
typedef _SearchChapterDart = int Function(int, int, Pointer<Utf8>, int, int, int, int,
    Pointer<Pointer<Uint8>>, Pointer<IntPtr>);
typedef _PagesNative = Uint32 Function(Uint64, Uint32, Pointer<Utf8>, IntPtr, Float, Float,
    Float, Uint32, Float, Float, Float, Pointer<Pointer<Uint8>>, Pointer<IntPtr>);
typedef _PagesDart = int Function(int, int, Pointer<Utf8>, int, double, double, double, int,
    double, double, double, Pointer<Pointer<Uint8>>, Pointer<IntPtr>);
typedef _PageAtNative = Uint32 Function(Uint64, Uint32, Uint32, Pointer<Utf8>, IntPtr, Float,
    Float, Float, Uint32, Float, Float, Float, Pointer<Uint32>);
typedef _PageAtDart = int Function(int, int, int, Pointer<Utf8>, int, double, double, double,
    int, double, double, double, Pointer<Uint32>);
typedef _HitNative = Uint32 Function(Uint64, Uint32, Uint32, Float, Float, Pointer<Utf8>,
    IntPtr, Float, Float, Float, Uint32, Float, Float, Float, Pointer<Pointer<Uint8>>, Pointer<IntPtr>);
typedef _HitDart = int Function(int, int, int, double, double, Pointer<Utf8>, int, double,
    double, double, int, double, double, double, Pointer<Pointer<Uint8>>, Pointer<IntPtr>);
typedef _PrefetchNative = Uint32 Function(Uint64, Uint32, Pointer<Utf8>, IntPtr, Float, Float,
    Float, Uint32, Float, Float, Float);
typedef _PrefetchDart = int Function(int, int, Pointer<Utf8>, int, double, double, double,
    int, double, double, double);
typedef _CacheCfgNative = Uint32 Function(Pointer<Utf8>, IntPtr, IntPtr);
typedef _CacheCfgDart = int Function(Pointer<Utf8>, int, int);
typedef _RenderNative = Uint32 Function(
    Uint64, Uint32, Uint32, Pointer<Pointer<Uint8>>, Pointer<IntPtr>, Pointer<Uint32>, Pointer<Uint32>);
typedef _RenderDart = int Function(
    int, int, int, Pointer<Pointer<Uint8>>, Pointer<IntPtr>, Pointer<Uint32>, Pointer<Uint32>);
typedef _LoadChapterNative = Uint32 Function(
    Uint64, Uint32, Pointer<Pointer<Uint8>>, Pointer<IntPtr>, Pointer<Pointer<Uint8>>, Pointer<IntPtr>,
    Pointer<Pointer<Uint8>>, Pointer<IntPtr>);
typedef _LoadChapterDart = int Function(
    int, int, Pointer<Pointer<Uint8>>, Pointer<IntPtr>, Pointer<Pointer<Uint8>>, Pointer<IntPtr>,
    Pointer<Pointer<Uint8>>, Pointer<IntPtr>);
typedef _BufFreeNative = Void Function(Pointer<Uint8>, IntPtr);
typedef _BufFreeDart = void Function(Pointer<Uint8>, int);
typedef _AbiNative = Uint32 Function();
typedef _AbiDart = int Function();
typedef _BookKindNative = Uint32 Function(Uint64);
typedef _BookKindDart = int Function(int);
typedef _PageCountNative = Uint32 Function(Uint64, Pointer<Uint32>);
typedef _PageCountDart = int Function(int, Pointer<Uint32>);
typedef _LoadPageNative = Uint32 Function(
    Uint64, Uint32, Pointer<Pointer<Uint8>>, Pointer<IntPtr>);
typedef _LoadPageDart = int Function(
    int, int, Pointer<Pointer<Uint8>>, Pointer<IntPtr>);
typedef _LoadResourceNative = Uint32 Function(
    Uint64, Pointer<Utf8>, IntPtr, Pointer<Pointer<Uint8>>, Pointer<IntPtr>);
typedef _LoadResourceDart = int Function(
    int, Pointer<Utf8>, int, Pointer<Pointer<Uint8>>, Pointer<IntPtr>);
typedef _ImageDimNative = Uint32 Function(
    Pointer<Uint8>, IntPtr, Pointer<Pointer<Uint8>>, Pointer<IntPtr>);
typedef _ImageDimDart = int Function(
    Pointer<Uint8>, int, Pointer<Pointer<Uint8>>, Pointer<IntPtr>);
typedef _PngBgraNative = Uint32 Function(Uint32, Uint32, IntPtr, Pointer<Uint8>, IntPtr,
    Pointer<Pointer<Uint8>>, Pointer<IntPtr>);
typedef _PngBgraDart = int Function(int, int, int, Pointer<Uint8>, int, Pointer<Pointer<Uint8>>,
    Pointer<IntPtr>);
typedef _SearchOpenNative = Uint64 Function(Uint64, Pointer<Utf8>, IntPtr, Uint32, Uint32);
typedef _SearchOpenDart = int Function(int, Pointer<Utf8>, int, int, int);
typedef _SearchNextNative = Uint32 Function(Uint64, Uint32, Pointer<Pointer<Uint8>>, Pointer<IntPtr>);
typedef _SearchNextDart = int Function(int, int, Pointer<Pointer<Uint8>>, Pointer<IntPtr>);
typedef _IdNative = Uint32 Function(Uint64);
typedef _IdDart = int Function(int);
typedef _HandleToIdNative = Uint64 Function(Uint64);
typedef _HandleToIdDart = int Function(int);
typedef _Bm25LoadNative = Uint64 Function(Pointer<Uint8>, IntPtr);
typedef _Bm25LoadDart = int Function(Pointer<Uint8>, int);
typedef _StepNative = Uint32 Function(Uint64, Uint32, Pointer<Pointer<Uint8>>, Pointer<IntPtr>);
typedef _StepDart = int Function(int, int, Pointer<Pointer<Uint8>>, Pointer<IntPtr>);
typedef _PullNative = Uint32 Function(
    Uint64, Uint64, IntPtr, Pointer<Pointer<Uint8>>, Pointer<IntPtr>, Pointer<Uint64>);
typedef _PullDart = int Function(
    int, int, int, Pointer<Pointer<Uint8>>, Pointer<IntPtr>, Pointer<Uint64>);
typedef _Bm25QueryNative = Uint32 Function(
    Uint64, Pointer<Utf8>, IntPtr, Uint32, Pointer<Pointer<Uint8>>, Pointer<IntPtr>);
typedef _Bm25QueryDart = int Function(
    int, Pointer<Utf8>, int, int, Pointer<Pointer<Uint8>>, Pointer<IntPtr>);

class _Bindings {
  _Bindings(DynamicLibrary lib)
      : open = lib.lookupFunction<_OpenNative, _OpenDart>('rrk_open'),
        close = lib.lookupFunction<_CloseNative, _CloseDart>('rrk_close'),
        metaJson = lib.lookupFunction<_QueryNative, _QueryDart>('rrk_meta_json'),
        tocJson = lib.lookupFunction<_QueryNative, _QueryDart>('rrk_toc_json'),
        chaptersJson = lib.lookupFunction<_QueryNative, _QueryDart>('rrk_chapters_json'),
        cover = lib.lookupFunction<_QueryNative, _QueryDart>('rrk_cover'),
        lastError = lib.lookupFunction<_ErrorNative, _ErrorDart>('rrk_last_error'),
        lastErrorCode = lib.lookupFunction<_ErrorCodeNative, _ErrorCodeDart>('rrk_last_error_code'),
        contentCacheConfigure =
            lib.lookupFunction<_CacheCfgNative, _CacheCfgDart>('rrk_content_cache_configure'),
        contentCacheStats =
            lib.lookupFunction<_ErrorNative, _ErrorDart>('rrk_content_cache_stats'),
        progressOf =
            lib.lookupFunction<_ProgressNative, _ProgressDart>('rrk_progress_of'),
        locate = lib.lookupFunction<_LocateNative, _LocateDart>('rrk_locate'),
        mapOffsets =
            lib.lookupFunction<_MapOffsetsNative, _MapOffsetsDart>('rrk_map_offsets'),
        sliceText = lib.lookupFunction<_SliceNative, _SliceDart>('rrk_slice_text'),
        loadHeadings =
            lib.lookupFunction<_HeadingsNative, _HeadingsDart>('rrk_load_headings'),
        imageThumbnail =
            lib.lookupFunction<_ThumbNative, _ThumbDart>('rrk_image_thumbnail'),
        searchChapter = lib.lookupFunction<_SearchChapterNative, _SearchChapterDart>(
            'rrk_search_chapter'),
        layoutCancel =
            lib.lookupFunction<_ErrorCodeNative, _ErrorCodeDart>('rrk_layout_cancel'),
        stats = lib.lookupFunction<_ErrorNative, _ErrorDart>('rrk_stats'),
        loadChapter =
            lib.lookupFunction<_LoadChapterNative, _LoadChapterDart>('rrk_load_chapter'),
        pagesJson = lib.lookupFunction<_PagesNative, _PagesDart>('rrk_pages_json'),
        pageAt = lib.lookupFunction<_PageAtNative, _PageAtDart>('rrk_page_at'),
        hitTest = lib.lookupFunction<_HitNative, _HitDart>('rrk_hit_test'),
        prefetch = lib.lookupFunction<_PrefetchNative, _PrefetchDart>('rrk_prefetch'),
        cacheConfigure =
            lib.lookupFunction<_CacheCfgNative, _CacheCfgDart>('rrk_cache_configure'),
        renderPage = lib.lookupFunction<_RenderNative, _RenderDart>('rrk_render_page'),
        bufFree = lib.lookupFunction<_BufFreeNative, _BufFreeDart>('rrk_buf_free'),
        abiVersion = lib.lookupFunction<_AbiNative, _AbiDart>('rrk_abi_version'),
        bookKind = lib.lookupFunction<_BookKindNative, _BookKindDart>('rrk_book_kind'),
        pageCount = lib.lookupFunction<_PageCountNative, _PageCountDart>('rrk_page_count'),
        loadPage = lib.lookupFunction<_LoadPageNative, _LoadPageDart>('rrk_load_page_json'),
        loadResource =
            lib.lookupFunction<_LoadResourceNative, _LoadResourceDart>('rrk_load_resource'),
        imageDimensions =
            lib.lookupFunction<_ImageDimNative, _ImageDimDart>('rrk_image_dimensions'),
        encodePngBgra =
            lib.lookupFunction<_PngBgraNative, _PngBgraDart>('rrk_encode_png_bgra'),
        searchOpen = lib.lookupFunction<_SearchOpenNative, _SearchOpenDart>('rrk_search_open'),
        searchNext = lib.lookupFunction<_SearchNextNative, _SearchNextDart>('rrk_search_next'),
        searchCancel = lib.lookupFunction<_IdNative, _IdDart>('rrk_search_cancel'),
        bm25Open = lib.lookupFunction<_HandleToIdNative, _HandleToIdDart>('rrk_bm25_open'),
        bm25Load = lib.lookupFunction<_Bm25LoadNative, _Bm25LoadDart>('rrk_bm25_load'),
        bm25Step = lib.lookupFunction<_StepNative, _StepDart>('rrk_bm25_step'),
        bm25Pull = lib.lookupFunction<_PullNative, _PullDart>('rrk_bm25_pull'),
        bm25Query = lib.lookupFunction<_Bm25QueryNative, _Bm25QueryDart>('rrk_bm25_query'),
        bm25Close = lib.lookupFunction<_IdNative, _IdDart>('rrk_bm25_close');

  final _OpenDart open;
  final _CloseDart close;
  final _QueryDart metaJson;
  final _QueryDart tocJson;
  final _QueryDart chaptersJson;
  final _QueryDart cover;
  final _ErrorDart lastError;
  final _ErrorCodeDart lastErrorCode;
  final _CacheCfgDart contentCacheConfigure;
  final _ErrorDart contentCacheStats;
  final _ProgressDart progressOf;
  final _LocateDart locate;
  final _MapOffsetsDart mapOffsets;
  final _SliceDart sliceText;
  final _HeadingsDart loadHeadings;
  final _ThumbDart imageThumbnail;
  final _SearchChapterDart searchChapter;
  final _ErrorCodeDart layoutCancel;
  final _ErrorDart stats;
  final _LoadChapterDart loadChapter;
  final _PagesDart pagesJson;
  final _PageAtDart pageAt;
  final _HitDart hitTest;
  final _PrefetchDart prefetch;
  final _CacheCfgDart cacheConfigure;
  final _RenderDart renderPage;
  final _BufFreeDart bufFree;
  final _AbiDart abiVersion;
  final _BookKindDart bookKind;
  final _PageCountDart pageCount;
  final _LoadPageDart loadPage;
  final _LoadResourceDart loadResource;
  final _ImageDimDart imageDimensions;
  final _PngBgraDart encodePngBgra;
  final _SearchOpenDart searchOpen;
  final _SearchNextDart searchNext;
  final _IdDart searchCancel;
  final _HandleToIdDart bm25Open;
  final _Bm25LoadDart bm25Load;
  final _StepDart bm25Step;
  final _PullDart bm25Pull;
  final _Bm25QueryDart bm25Query;
  final _IdDart bm25Close;
}

_Bindings? _b;

_Bindings get _bindings {
  final b = _b;
  if (b == null) {
    throw StateError('RrkEngine.init(libPath) 尚未调用');
  }
  return b;
}

// ---------------------------------------------------------------------------
// 引擎
// ---------------------------------------------------------------------------

class RrkEngine {
  /// 加载原生内核库并校验 ABI 版本。
  ///
  /// [libPath] 省略时按下列顺序自动解析（便于宿主零配置接入）：
  /// 1. 显式参数 [libPath]；
  /// 2. 环境变量 `RRK_LIB`；
  /// 3. 平台默认名：Android `librrk_ffi.so`（jniLibs/APK 提供，系统链接器定位）、
  ///    Windows `rrk_ffi.dll`、macOS `librrk_ffi.dylib`、其它 `librrk_ffi.so`；
  /// 4. iOS：静态链接进宿主，走 `DynamicLibrary.process()`。
  static void init([String? libPath]) {
    final DynamicLibrary lib;
    final explicit = libPath ?? Platform.environment['RRK_LIB'];
    if (explicit != null && explicit.isNotEmpty) {
      lib = DynamicLibrary.open(explicit);
    } else if (Platform.isIOS) {
      lib = DynamicLibrary.process();
    } else {
      lib = DynamicLibrary.open(_defaultLibName());
    }
    final b = _Bindings(lib);
    final abi = b.abiVersion();
    if (abi != kRrkAbiVersion) {
      throw RrkException(1999, 'ABI 版本不匹配：内核=$abi，封装=$kRrkAbiVersion');
    }
    _b = b;
  }

  /// 未显式指定库路径时的平台默认库名。
  static String _defaultLibName() {
    if (Platform.isAndroid) return 'librrk_ffi.so';
    if (Platform.isMacOS) return 'librrk_ffi.dylib';
    if (Platform.isWindows) return 'rrk_ffi.dll';
    return 'librrk_ffi.so';
  }

  /// 汇总观测统计（原始 JSON）：open_books / searches / indexes /
  /// layout{hits,misses} / content{toc,chapter,outline}。埋点用。
  static String statsJson() {
    final b = _bindings;
    final outPtr = malloc<Pointer<Uint8>>();
    final outLen = malloc<IntPtr>();
    try {
      final code = b.stats(outPtr, outLen);
      if (code != 0) throw _lastError(b);
      return _takeBuffer(b, outPtr.value, outLen.value) ?? '{}';
    } finally {
      malloc.free(outPtr);
      malloc.free(outLen);
    }
  }

  /// 请求取消进行中的排版（尽力取消，块粒度）：正在跑的 `pagesOf` 以 1005
  /// 返回；没有进行中布局时调用无副作用。可从任意线程调用。
  static void cancelLayout() {
    _bindings.layoutCancel();
  }

  /// 配置三要素缓存（目录 + 内存上限 MB）。
  static void configureCache(String dir, {int memBytes = 64 * 1024 * 1024}) {
    final b = _bindings;
    final p = dir.toNativeUtf8();
    try {
      final code = b.cacheConfigure(p, utf8.encode(dir).length, memBytes);
      if (code != 0) throw _lastError(b);
    } finally {
      malloc.free(p);
    }
  }

  /// 配置内容级缓存（目录树/章文本/章大纲；key 只含内容 hash，命中免重新解析）。
  /// 空路径 = 关闭。issue #12。
  static void configureContentCache(String dir,
      {int memBytes = 32 * 1024 * 1024}) {
    final b = _bindings;
    final p = dir.toNativeUtf8();
    try {
      final code =
          b.contentCacheConfigure(p, utf8.encode(dir).length, memBytes);
      if (code != 0) throw _lastError(b);
    } finally {
      malloc.free(p);
    }
  }

  /// 内容级缓存命中统计（原始 JSON：toc/chapter/outline/open 各 hits/misses）。
  /// issue #12 + open 索引持久化（ROADMAP 章级/页级缓存持久化）。
  static String contentCacheStatsJson() {
    final b = _bindings;
    final outPtr = malloc<Pointer<Uint8>>();
    final outLen = malloc<IntPtr>();
    try {
      final code = b.contentCacheStats(outPtr, outLen);
      if (code != 0) throw _lastError(b);
      return _takeBuffer(b, outPtr.value, outLen.value) ?? '{}';
    } finally {
      malloc.free(outPtr);
      malloc.free(outLen);
    }
  }

  /// 打开一本书（文件路径）。
  static RrkBook openBook(String path) {
    final b = _bindings;
    final pathPtr = path.toNativeUtf8();
    try {
      final handle = b.open(pathPtr, utf8.encode(path).length);
      if (handle == 0) {
        throw _lastError(b);
      }
      return RrkBook._(handle);
    } finally {
      malloc.free(pathPtr);
    }
  }

  static RrkException _lastError(_Bindings b) {
    final outPtr = malloc<Pointer<Uint8>>();
    final outLen = malloc<IntPtr>();
    try {
      b.lastError(outPtr, outLen);
      final msg = _takeBuffer(b, outPtr.value, outLen.value);
      // 稳定错误码（issue #11）：与消息同源；老库无此符号时退回 1001
      var code = 1001;
      try {
        code = b.lastErrorCode();
      } catch (_) {}
      if (code == 0) code = 1001;
      return RrkException(code, msg ?? '未知错误');
    } finally {
      malloc.free(outPtr);
      malloc.free(outLen);
    }
  }

  /// 取走内核缓冲 → Dart 字符串，并释放缓冲。
  static String? _takeBuffer(_Bindings b, Pointer<Uint8> ptr, int len) {
    if (ptr == nullptr || len == 0) return null;
    final bytes = Uint8List.fromList(ptr.asTypedList(len));
    b.bufFree(ptr, len);
    return utf8.decode(bytes);
  }

  /// 取走内核缓冲 → Dart 字节，并释放缓冲。
  static Uint8List? _takeBytes(_Bindings b, Pointer<Uint8> ptr, int len) {
    if (ptr == nullptr || len == 0) return null;
    final bytes = Uint8List.fromList(ptr.asTypedList(len));
    b.bufFree(ptr, len);
    return bytes;
  }
}

// ---------------------------------------------------------------------------
// 图像工具（与书句柄无关的纯字节能力）
// ---------------------------------------------------------------------------

/// 图片头部尺寸（只读文件头，未解码像素）。
class RrkImageSize {
  RrkImageSize._(this.width, this.height, this.format, this.mime);
  final int width;
  final int height;

  /// `png` / `jpeg` / `gif` / `bmp` / `webp`。
  final String format;

  /// 如 `image/png`。
  final String mime;

  @override
  String toString() => 'RrkImageSize(${width}x$height $format)';
}

/// 图像工具：尺寸头解析 + BGRA→PNG 编码。
///
/// 两者都不落盘（落盘归宿主）、都不解码/不做色彩管理，只做字节级工作。
class RrkImage {
  /// 解码整图并等比缩到 [maxEdge]（长边）以内 → `(rgba, width, height)`（RGBA8）。
  ///
  /// 原图已在限内则不缩仅解码；AVIF/HEIC 等内核不解像素的格式抛
  /// 1002（宿主取原始字节自行解码），无法识别同样 1002、损坏 1003。
  /// 书架/封面缩略图一条龙；页缩略图请用 `renderPage(targetWidth)`。
  static (Uint8List, int, int) thumbnail(Uint8List bytes, {int maxEdge = 320}) {
    if (bytes.isEmpty) throw RrkException(1004, '图片缓冲为空');
    final b = _bindings;
    final src = malloc<Uint8>(bytes.length);
    final outPtr = malloc<Pointer<Uint8>>();
    final outLen = malloc<IntPtr>();
    final w = malloc<Uint32>();
    final h = malloc<Uint32>();
    try {
      src.asTypedList(bytes.length).setAll(0, bytes);
      final code =
          b.imageThumbnail(src, bytes.length, maxEdge, outPtr, outLen, w, h);
      if (code != 0) {
        throw RrkException(code, RrkEngine._lastError(b).message);
      }
      final rgba =
          RrkEngine._takeBytes(b, outPtr.value, outLen.value) ?? Uint8List(0);
      return (rgba, w.value, h.value);
    } finally {
      malloc.free(src);
      malloc.free(outPtr);
      malloc.free(outLen);
      malloc.free(w);
      malloc.free(h);
    }
  }

  /// 只读图片头取尺寸；无法识别的格式/畸形字节返回 null。
  ///
  /// 只扫头部（PNG IHDR / JPEG SOFn / GIF / BMP / WebP chunk），不分配像素缓冲。
  /// 便捷版：内部把 [bytes] 拷贝一次到原生内存，需零拷贝请用 [dimensionsPtr]。
  static RrkImageSize? dimensions(Uint8List bytes) {
    if (bytes.isEmpty) return null;
    final src = malloc<Uint8>(bytes.length);
    try {
      src.asTypedList(bytes.length).setAll(0, bytes);
      return dimensionsPtr(src, bytes.length);
    } finally {
      malloc.free(src);
    }
  }

  /// 同上，直接读调用方持有的原生缓冲（**零拷贝**）。
  static RrkImageSize? dimensionsPtr(Pointer<Uint8> data, int len) {
    if (data == nullptr || len == 0) return null;
    final b = _bindings;
    final outPtr = malloc<Pointer<Uint8>>();
    final outLen = malloc<IntPtr>();
    try {
      final code = b.imageDimensions(data, len, outPtr, outLen);
      if (code == 2) return null; // 无法识别（软结果，非错误）
      if (code != 0) {
        throw RrkException(code, RrkEngine._lastError(b).message);
      }
      final json = RrkEngine._takeBuffer(b, outPtr.value, outLen.value);
      if (json == null) return null;
      final m = jsonDecode(json) as Map<String, dynamic>;
      return RrkImageSize._(m['width'] as int, m['height'] as int,
          m['format'] as String, m['mime'] as String);
    } finally {
      malloc.free(outPtr);
      malloc.free(outLen);
    }
  }

  /// BGRA 原始位图 → PNG 字节（8 位 RGBA 标准 PNG）。
  ///
  /// [stride] 为源缓冲每行字节数（0 = 紧凑 `width*4`）。
  /// 便捷版：内部拷贝一次（页位图可达数 MB，宿主已有原生缓冲请用 [encodePngFromBgraPtr]）。
  static Uint8List encodePngFromBgra(Uint8List bgra,
      {required int width, required int height, int stride = 0}) {
    if (bgra.isEmpty) {
      throw RrkException(1004, '位图缓冲为空');
    }
    final src = malloc<Uint8>(bgra.length);
    try {
      src.asTypedList(bgra.length).setAll(0, bgra);
      return encodePngFromBgraPtr(src, bgra.length,
          width: width, height: height, stride: stride);
    } finally {
      malloc.free(src);
    }
  }

  /// 同上，直接读调用方持有的原生缓冲（**零拷贝入参**，内核只读不接管所有权）。
  static Uint8List encodePngFromBgraPtr(Pointer<Uint8> src, int srcLen,
      {required int width, required int height, int stride = 0}) {
    if (src == nullptr || srcLen == 0) {
      throw RrkException(1004, '位图缓冲为空');
    }
    final b = _bindings;
    final outPtr = malloc<Pointer<Uint8>>();
    final outLen = malloc<IntPtr>();
    try {
      final code =
          b.encodePngBgra(width, height, stride, src, srcLen, outPtr, outLen);
      if (code != 0) {
        throw RrkException(code, RrkEngine._lastError(b).message);
      }
      return RrkEngine._takeBytes(b, outPtr.value, outLen.value) ?? Uint8List(0);
    } finally {
      malloc.free(outPtr);
      malloc.free(outLen);
    }
  }
}

// ---------------------------------------------------------------------------
// 数据模型
// ---------------------------------------------------------------------------

class BookMeta {
  BookMeta._(this.title, this.authors, this.language, this.description,
      this.format, this.contentHash);
  final String title;
  final List<String> authors;
  final String? language;

  /// 简介（EPUB `dc:description` / MOBI EXTH 103·104）；缺失为 null。
  final String? description;
  final String format;
  final String contentHash;

  static BookMeta _fromJson(Map<String, dynamic> j) => BookMeta._(
      j['title'] as String,
      (j['authors'] as List).cast<String>(),
      j['language'] as String?,
      j['description'] as String?,
      j['format'] as String,
      j['contentHash'] as String);

  @override
  String toString() => 'BookMeta($title / ${authors.join(",")} / $format)';
}

class TocNode {
  TocNode._(this.title, this.chapter, this.anchor, this.children);
  final String title;
  final int chapter;

  /// 章内字节偏移锚点（目标元素起点）；无 fragment/无法解析时为 null。
  final int? anchor;
  final List<TocNode> children;

  static List<TocNode> _listFromJson(List<dynamic> js) => js
      .map((e) => TocNode._(e['title'] as String, e['chapter'] as int,
          e['anchor'] as int?, _listFromJson((e['children'] as List?) ?? const [])))
      .toList();
}

class ChapterEntry {
  ChapterEntry._(this.id, this.title, this.href, this.charLen, this.byteStart, this.byteEnd, this.part);
  final int id;
  final String? title;

  /// 稳定定位串（进度/同步的章节身份键）：EPUB = 包内路径、TXT = `txt:<字符偏移>`、
  /// MOBI = `mobi:<序号>`、CBZ = 页名、PDF = `pdf:<序号>`。
  final String href;
  final int charLen;
  final int byteStart;
  final int byteEnd;
  final int part;

  static List<ChapterEntry> _listFromJson(List<dynamic> js) => js
      .map((e) => ChapterEntry._(e['id'] as int, e['title'] as String?,
          e['href'] as String? ?? '', e['charLen'] as int,
          e['byteStart'] as int, e['byteEnd'] as int, e['part'] as int))
      .toList();

  @override
  String toString() => 'ChapterEntry(#$id ${title ?? "（无标题）"} $href ≈$charLen 字)';
}

class ChapterBlock {
  ChapterBlock._(this.type, this.text, this.level, this.indent, this.key, this.alt);
  final String type; // h / p / img / quote / hr

  /// 块文本（v9：由部件流按序重建——文本切自章文本字节、图片插
  /// `\u{FFFC}key\u{FFFC}` 标记，与旧格式内嵌 text 逐字节一致）。
  final String? text;
  final int? level;
  final bool? indent;
  final String? key;
  final String? alt;

  /// [js] 为 blocks JSON 数组；[text] 为同源正文的 UTF-8 字节（部件按区间切）。
  static List<ChapterBlock> _listFromJson(List<dynamic> js, Uint8List text) => js.map((e) {
        final parts = e['parts'] as List?;
        String? t;
        if (parts != null) {
          final sb = StringBuffer();
          for (final p in parts) {
            if (p is List && p.length == 2) {
              final s = p[0] as int;
              final en = p[1] as int;
              if (en > s && s >= 0 && en <= text.length) {
                sb.write(utf8.decode(text.sublist(s, en)));
              }
            } else if (p is List && p.length == 1 && p[0] is String) {
              sb.write('\u{FFFC}${p[0]}\u{FFFC}');
            }
          }
          t = sb.toString();
        }
        return ChapterBlock._(
            e['type'] as String,
            t,
            e['level'] as int?,
            e['indent'] as bool?,
            e['key'] as String?,
            e['alt'] as String?);
      }).toList();

  @override
  String toString() => 'ChapterBlock($type ${text ?? key ?? ""})';
}

/// 版式行（行表条目）。
class ReaderLine {
  ReaderLine._(this.start, this.end, this.height, this.paraStart);
  final int start;
  final int end;
  final double height;
  final bool paraStart;
}

/// 文本 run（字体/字号/位置；文本范围 = 章文本 UTF-8 字节偏移）。
class ReaderRun {
  ReaderRun._(this.line, this.x, this.baseline, this.fontSize, this.bold, this.italic,
      this.start, this.end);
  final int line;
  final double x;
  final double baseline;
  final double fontSize;
  final bool bold;
  final bool italic;
  final int start;
  final int end;
}

/// 版式页（几何模式数据面 → Flutter 自绘）。
class ReaderPage {
  ReaderPage._(this.index, this.start, this.end, this.height, this.lines, this.runs);
  final int index;
  final int start;
  final int end;
  final double height;
  final List<ReaderLine> lines;
  final List<ReaderRun> runs;

  static ReaderPage _fromJson(Map<String, dynamic> j) => ReaderPage._(
      j['index'] as int,
      j['start'] as int,
      j['end'] as int,
      (j['height'] as num).toDouble(),
      (j['lines'] as List)
          .map((e) => ReaderLine._(e['s'] as int, e['e'] as int, (e['h'] as num).toDouble(),
              e['p'] as bool))
          .toList(),
      (j['runs'] as List)
          .map((e) => ReaderRun._(e['l'] as int, (e['x'] as num).toDouble(),
              (e['y'] as num).toDouble(), (e['fs'] as num).toDouble(), e['b'] as bool,
              e['i'] as bool, e['s'] as int, e['e'] as int))
          .toList());
}

/// 正文内链区间：清洗后章文本字节区间 → 目标 href（EPUB 为包内路径或 `#frag`）。
class ChapterLink {
  ChapterLink._(this.start, this.end, this.href);
  final int start;
  final int end;
  final String href;
}

class ChapterContent {
  ChapterContent._(this.id, this.text, this.blocks, this.links);
  final int id;
  final String text;
  final List<ChapterBlock> blocks;

  /// 正文内链区间（无链接的书为空）。
  final List<ChapterLink> links;
}

/// 页级格式（CBZ / PDF）的一页。
class PagedPage {
  PagedPage._(this.kind, this.text, this.blocks, this.imageKey, this.mime);

  /// `image`（页图）或 `text`（L1 抽取文本）。
  final String kind;
  final String? text;
  final List<ChapterBlock> blocks;

  /// 图片页的资源 key（配合 `loadResource` 取字节）。
  final String? imageKey;
  final String? mime;

  static PagedPage _fromJson(Map<String, dynamic> j) {
    final text = j['text'] as String?;
    return PagedPage._(
      j['kind'] as String,
      text,
      ChapterBlock._listFromJson(
          (j['blocks'] as List?) ?? const [], utf8.encode(text ?? '')),
      j['key'] as String?,
      j['mime'] as String?,
    );
  }
}

/// 检索命中：定位信息（章 + 章内偏移）与上下文片段。
class SearchHit {
  SearchHit._(this.chapter, this.offset, this.len, this.snippet, this.snippetOffset);

  /// 所在章。
  final int chapter;

  /// 章正文（`loadChapter().text`）中的 **UTF-8 字节偏移**，与目录锚点/内链同坐标系。
  final int offset;

  /// 命中长度（字节）。
  final int len;

  /// 上下文片段（超长时两端带 `…`）。
  final String snippet;

  /// 片段在所属段落文本中的起点（字节），供局部高亮。
  final int snippetOffset;

  @override
  String toString() => 'SearchHit(#$chapter @$offset "$snippet")';
}

/// 一批检索结果（拉模式分块）。
class SearchBatch {
  SearchBatch._(this.hits, this.scanned, this.total, this.done);

  final List<SearchHit> hits;

  /// 已扫完的章数（含跳过的图片章）。
  final int scanned;

  /// 全书章数。
  final int total;

  /// 是否已扫完全书（此批之后没有更多命中）。
  final bool done;

  /// 进度 0..1。
  double get progress => total == 0 ? 1.0 : (scanned / total).clamp(0.0, 1.0);

  @override
  String toString() => 'SearchBatch(${hits.length} 条, $scanned/$total, done=$done)';
}

/// BM25 索引构建进度。
class Bm25Progress {
  Bm25Progress._(this.built, this.total, this.done, this.terms, this.bytes);

  /// 已投喂章数。
  final int built;

  /// 全书章数。
  final int total;

  /// 是否建完（建完后才能 `pull` / `query`）。
  final bool done;

  /// 唯一词项数。
  final int terms;

  /// 序列化索引字节数（未建完为 0）。
  final int bytes;

  double get ratio => total == 0 ? 1.0 : (built / total).clamp(0.0, 1.0);

  @override
  String toString() => 'Bm25Progress($built/$total, done=$done, $terms 词项, $bytes 字节)';
}

/// BM25 排序命中（章级）。
class Bm25Hit {
  Bm25Hit._(this.chapter, this.score);
  final int chapter;
  final double score;

  @override
  String toString() => 'Bm25Hit(#$chapter ${score.toStringAsFixed(4)})';
}

// ---------------------------------------------------------------------------
// 书
// ---------------------------------------------------------------------------

class RrkBook {
  RrkBook._(this._handle) {
    _finalizer.attach(this, _handle, detach: this);
  }

  static final Finalizer<int> _finalizer = Finalizer((handle) {
    final b = _b;
    if (b != null && handle != 0) {
      b.close(handle); // 兜底：GC 时未 close 的句柄交还内核
    }
  });

  int _handle;
  bool _closed = false;

  int get handle => _handle;

  void _check() {
    if (_closed || _handle == 0) {
      throw RrkException(1004, '书已关闭');
    }
  }

  Never _throwCode(int code) {
    final e = RrkEngine._lastError(_bindings);
    throw RrkException(code, e.message);
  }

  /// 书目元数据。
  BookMeta meta() {
    _check();
    final b = _bindings;
    final outPtr = malloc<Pointer<Uint8>>();
    final outLen = malloc<IntPtr>();
    try {
      final code = b.metaJson(_handle, outPtr, outLen);
      if (code != 0) _throwCode(code);
      final json = RrkEngine._takeBuffer(b, outPtr.value, outLen.value);
      return BookMeta._fromJson(jsonDecode(json!) as Map<String, dynamic>);
    } finally {
      malloc.free(outPtr);
      malloc.free(outLen);
    }
  }

  /// 目录树。
  List<TocNode> toc() {
    _check();
    final b = _bindings;
    final outPtr = malloc<Pointer<Uint8>>();
    final outLen = malloc<IntPtr>();
    try {
      final code = b.tocJson(_handle, outPtr, outLen);
      if (code != 0) _throwCode(code);
      final json = RrkEngine._takeBuffer(b, outPtr.value, outLen.value);
      return TocNode._listFromJson(jsonDecode(json!) as List<dynamic>);
    } finally {
      malloc.free(outPtr);
      malloc.free(outLen);
    }
  }

  /// 章索引。
  List<ChapterEntry> chapters() {
    _check();
    final b = _bindings;
    final outPtr = malloc<Pointer<Uint8>>();
    final outLen = malloc<IntPtr>();
    try {
      final code = b.chaptersJson(_handle, outPtr, outLen);
      if (code != 0) _throwCode(code);
      final json = RrkEngine._takeBuffer(b, outPtr.value, outLen.value);
      return ChapterEntry._listFromJson(jsonDecode(json!) as List<dynamic>);
    } finally {
      malloc.free(outPtr);
      malloc.free(outLen);
    }
  }

  /// 书类型：0=Text 1=ImageGallery 2=Paged（-1 = 非法句柄）。
  int get kind => _bindings.bookKind(_handle);

  /// 页数（仅 CBZ/PDF；重排格式抛错）。
  int pageCount() {
    _check();
    final out = malloc<Uint32>();
    try {
      final code = _bindings.pageCount(_handle, out);
      if (code != 0) _throwCode(code);
      return out.value;
    } finally {
      malloc.free(out);
    }
  }

  /// 读一页（CBZ 图片页 / PDF 文本页）。
  PagedPage loadPage(int index) {
    _check();
    final b = _bindings;
    final outPtr = malloc<Pointer<Uint8>>();
    final outLen = malloc<IntPtr>();
    try {
      final code = b.loadPage(_handle, index, outPtr, outLen);
      if (code != 0) _throwCode(code);
      final json = RrkEngine._takeBuffer(b, outPtr.value, outLen.value) ?? '{}';
      return PagedPage._fromJson(jsonDecode(json) as Map<String, dynamic>);
    } finally {
      malloc.free(outPtr);
      malloc.free(outLen);
    }
  }

  /// 按 key 取资源原始字节（图片页等）。
  Uint8List loadResource(String key) {
    _check();
    final b = _bindings;
    final keyPtr = key.toNativeUtf8();
    final outPtr = malloc<Pointer<Uint8>>();
    final outLen = malloc<IntPtr>();
    try {
      final code =
          b.loadResource(_handle, keyPtr, utf8.encode(key).length, outPtr, outLen);
      if (code != 0) _throwCode(code);
      return RrkEngine._takeBytes(b, outPtr.value, outLen.value) ?? Uint8List(0);
    } finally {
      malloc.free(keyPtr);
      malloc.free(outPtr);
      malloc.free(outLen);
    }
  }

  /// 加载一章（正文 + 块结构）。
  ChapterContent loadChapter(int chapterId) {
    _check();
    final b = _bindings;
    final textPtr = malloc<Pointer<Uint8>>();
    final textLen = malloc<IntPtr>();
    final blocksPtr = malloc<Pointer<Uint8>>();
    final blocksLen = malloc<IntPtr>();
    final linksPtr = malloc<Pointer<Uint8>>();
    final linksLen = malloc<IntPtr>();
    try {
      final code = b.loadChapter(
          _handle, chapterId, textPtr, textLen, blocksPtr, blocksLen, linksPtr, linksLen);
      if (code != 0) _throwCode(code);
      // 正文保留原始 UTF-8 字节：blocks 部件流按字节区间切片（v9）
      final textBytes =
          RrkEngine._takeBytes(b, textPtr.value, textLen.value) ?? Uint8List(0);
      final text = utf8.decode(textBytes);
      final blocksJson = RrkEngine._takeBuffer(b, blocksPtr.value, blocksLen.value) ?? '[]';
      final linksJson = RrkEngine._takeBuffer(b, linksPtr.value, linksLen.value) ?? '[]';
      return ChapterContent._(
          chapterId,
          text,
          ChapterBlock._listFromJson(jsonDecode(blocksJson) as List<dynamic>, textBytes),
          (jsonDecode(linksJson) as List<dynamic>)
              .map((e) => ChapterLink._(
                  e['start'] as int, e['end'] as int, e['href'] as String))
              .toList());
    } finally {
      malloc.free(textPtr);
      malloc.free(textLen);
      malloc.free(blocksPtr);
      malloc.free(blocksLen);
      malloc.free(linksPtr);
      malloc.free(linksLen);
    }
  }

  /// 排版一章 → 页数组（几何模式）。
  List<ReaderPage> pagesOf(
    int chapterId, {
    String fontFamily = '',
    double fontSize = 18.0,
    double lineHeight = 1.6,
    double paraIndentEm = 2.0,
    int align = 0,
    double width = 390.0,
    double height = 842.0,
    double dpr = 3.0,
  }) {
    _check();
    final b = _bindings;
    final outPtr = malloc<Pointer<Uint8>>();
    final outLen = malloc<IntPtr>();
    final fontPtr = fontFamily.toNativeUtf8();
    try {
      final code = b.pagesJson(_handle, chapterId, fontPtr, utf8.encode(fontFamily).length, fontSize,
          lineHeight, paraIndentEm, align, width, height, dpr, outPtr, outLen);
      if (code != 0) _throwCode(code);
      final json = RrkEngine._takeBuffer(b, outPtr.value, outLen.value) ?? '[]';
      return (jsonDecode(json) as List<dynamic>)
          .map((e) => ReaderPage._fromJson(e as Map<String, dynamic>))
          .toList();
    } finally {
      malloc.free(fontPtr);
      malloc.free(outPtr);
      malloc.free(outLen);
    }
  }

  /// 语义偏移 → 页下标（进度恢复；缓存命中免排版）。
  int pageAt(int chapterId, int charOffset,
      {String fontFamily = '',
      double fontSize = 18.0,
      double lineHeight = 1.6,
      double paraIndentEm = 2.0,
      int align = 0,
      double width = 390.0,
      double height = 842.0,
      double dpr = 3.0}) {
    _check();
    final b = _bindings;
    final fontPtr = fontFamily.toNativeUtf8();
    final out = malloc<Uint32>();
    try {
      final code = b.pageAt(_handle, chapterId, charOffset, fontPtr, utf8.encode(fontFamily).length,
          fontSize, lineHeight, paraIndentEm, align, width, height, dpr, out);
      if (code != 0) _throwCode(code);
      return out.value;
    } finally {
      malloc.free(fontPtr);
      malloc.free(out);
    }
  }

  /// 语义坐标 → 全书进度 [0,1]（估算口径：按章权重，进度条/跨设备同步用）。
  double progressOf(int chapterId, int byteOffset) {
    _check();
    final out = malloc<Float>();
    try {
      final code = _bindings.progressOf(_handle, chapterId, byteOffset, out);
      if (code != 0) _throwCode(code);
      return out.value;
    } finally {
      malloc.free(out);
    }
  }

  /// 进度 [0,1] → (chapterId, 章内字节偏移)；建议再经 pageAt 精确定页。
  (int, int) locateByPercent(double percent) {
    _check();
    final c = malloc<Uint32>();
    final o = malloc<Uint32>();
    try {
      final code = _bindings.locate(_handle, percent, c, o);
      if (code != 0) _throwCode(code);
      return (c.value, o.value);
    } finally {
      malloc.free(c);
      malloc.free(o);
    }
  }

  /// 批量偏移映射：章文本 UTF-8 字节偏移 ↔ UTF-16 code unit 偏移
  /// （Dart 字符串即 UTF-16，标注/选区必经）。非边界自动夹取（详见内核文档）。
  List<int> mapOffsets(int chapterId, List<int> offsets,
      {bool toUtf16 = true}) {
    _check();
    if (offsets.isEmpty) return const [];
    final b = _bindings;
    final inPtr = malloc<Uint32>(offsets.length);
    final outPtr = malloc<Pointer<Uint8>>();
    final outLen = malloc<IntPtr>();
    try {
      for (var i = 0; i < offsets.length; i++) {
        inPtr[i] = offsets[i];
      }
      final code = b.mapOffsets(_handle, chapterId, toUtf16 ? 0 : 1, inPtr,
          offsets.length, outPtr, outLen);
      if (code != 0) _throwCode(code);
      final bytes = RrkEngine._takeBytes(b, outPtr.value, outLen.value) ??
          Uint8List(0);
      final bd = ByteData.sublistView(bytes);
      return [
        for (var i = 0; i + 4 <= bytes.length; i += 4)
          bd.getUint32(i, Endian.little)
      ];
    } finally {
      malloc.free(inPtr);
      malloc.free(outPtr);
      malloc.free(outLen);
    }
  }

  /// 选区取文：`[start, end)` 夹到字符边界后返回章文本切片（免整章拉回）。
  String sliceText(int chapterId, int start, int end) {
    _check();
    final b = _bindings;
    final outPtr = malloc<Pointer<Uint8>>();
    final outLen = malloc<IntPtr>();
    try {
      final code = b.sliceText(_handle, chapterId, start, end, outPtr, outLen);
      if (code != 0) _throwCode(code);
      return RrkEngine._takeBuffer(b, outPtr.value, outLen.value) ?? '';
    } finally {
      malloc.free(outPtr);
      malloc.free(outLen);
    }
  }

  /// 章标题块（issue #13）：`(level, 文本)` 列表，走章内容级缓存
  /// （命中免解析）；无标题/图片章返回空表。无 TOC 书的章名回填用它。
  List<(int, String)> loadHeadings(int chapterId) {
    _check();
    final b = _bindings;
    final outPtr = malloc<Pointer<Uint8>>();
    final outLen = malloc<IntPtr>();
    try {
      final code = b.loadHeadings(_handle, chapterId, outPtr, outLen);
      if (code != 0) _throwCode(code);
      final json = RrkEngine._takeBuffer(b, outPtr.value, outLen.value) ?? '[]';
      return ((jsonDecode(json) as List)
          .map((e) => (e['level'] as int, e['text'] as String))
          .toList());
    } finally {
      malloc.free(outPtr);
      malloc.free(outLen);
    }
  }

  /// 页内命中测试（页内 x,y → 语义偏移，run 级粒度）。
  (int offset, int line) hitTest(int chapterId, int pageIndex, double x, double y,
      {String fontFamily = '',
      double fontSize = 18.0,
      double lineHeight = 1.6,
      double paraIndentEm = 2.0,
      int align = 0,
      double width = 390.0,
      double height = 842.0,
      double dpr = 3.0}) {
    _check();
    final b = _bindings;
    final fontPtr = fontFamily.toNativeUtf8();
    final outPtr = malloc<Pointer<Uint8>>();
    final outLen = malloc<IntPtr>();
    try {
      final code = b.hitTest(_handle, chapterId, pageIndex, x, y, fontPtr, utf8.encode(fontFamily).length,
          fontSize, lineHeight, paraIndentEm, align, width, height, dpr, outPtr, outLen);
      if (code != 0) _throwCode(code);
      final json = RrkEngine._takeBuffer(b, outPtr.value, outLen.value) ?? '{}';
      final m = jsonDecode(json) as Map<String, dynamic>;
      return (m['offset'] as int, m['line'] as int);
    } finally {
      malloc.free(fontPtr);
      malloc.free(outPtr);
      malloc.free(outLen);
    }
  }

  /// 远跳预取：预热本章 + 相邻章（走缓存链路）。
  void prefetch(int chapterId,
      {String fontFamily = '',
      double fontSize = 18.0,
      double lineHeight = 1.6,
      double paraIndentEm = 2.0,
      int align = 0,
      double width = 390.0,
      double height = 842.0,
      double dpr = 3.0}) {
    _check();
    final b = _bindings;
    final fontPtr = fontFamily.toNativeUtf8();
    try {
      final code = b.prefetch(_handle, chapterId, fontPtr, utf8.encode(fontFamily).length, fontSize,
          lineHeight, paraIndentEm, align, width, height, dpr);
      if (code != 0) _throwCode(code);
    } finally {
      malloc.free(fontPtr);
    }
  }

  /// 整页栅格化（PDF 原版页）→ RGBA + 尺寸。
  (Uint8List, int, int) renderPage(int pageIndex, {int targetWidth = 900}) {
    _check();
    final b = _bindings;
    final outPtr = malloc<Pointer<Uint8>>();
    final outLen = malloc<IntPtr>();
    final outW = malloc<Uint32>();
    final outH = malloc<Uint32>();
    try {
      final code = b.renderPage(_handle, pageIndex, targetWidth, outPtr, outLen, outW, outH);
      if (code != 0) _throwCode(code);
      final bytes = RrkEngine._takeBytes(b, outPtr.value, outLen.value) ?? Uint8List(0);
      return (bytes, outW.value, outH.value);
    } finally {
      malloc.free(outPtr);
      malloc.free(outLen);
      malloc.free(outW);
      malloc.free(outH);
    }
  }

  /// 封面图字节（无封面返回 null）。
  Uint8List? cover() {
    _check();
    final b = _bindings;
    final outPtr = malloc<Pointer<Uint8>>();
    final outLen = malloc<IntPtr>();
    try {
      final code = b.cover(_handle, outPtr, outLen);
      if (code != 0 && code != 2) _throwCode(code);
      return RrkEngine._takeBytes(b, outPtr.value, outLen.value);
    } finally {
      malloc.free(outPtr);
      malloc.free(outLen);
    }
  }

  /// 建立全书检索会话（拉模式分块、可取消）。
  ///
  /// 检索基于「宿主读到的段落文本」同源数据，命中偏移是章正文 UTF-8 字节偏移。
  RrkSearchSession search(String query,
      {bool caseSensitive = false, int contextChars = 30}) {
    _check();
    final b = _bindings;
    final q = query.toNativeUtf8();
    try {
      final id = b.searchOpen(
          _handle, q, utf8.encode(query).length, caseSensitive ? 1 : 0, contextChars);
      if (id == 0) throw RrkException(1001, RrkEngine._lastError(b).message);
      return RrkSearchSession._(id);
    } finally {
      malloc.free(q);
    }
  }

  /// 轻量**单章**搜索：不建会话，同步扫完返回整页命中（坐标与全书搜索同源）。
  /// [maxHits] = 0 取内核默认上限（1000）。
  SearchBatch searchChapter(int chapterId, String query,
      {bool caseSensitive = false, int contextChars = 30, int maxHits = 0}) {
    _check();
    final b = _bindings;
    final q = query.toNativeUtf8();
    final outPtr = malloc<Pointer<Uint8>>();
    final outLen = malloc<IntPtr>();
    try {
      final code = b.searchChapter(_handle, chapterId, q, utf8.encode(query).length,
          caseSensitive ? 1 : 0, contextChars, maxHits, outPtr, outLen);
      if (code != 0) _throwCode(code);
      final json = RrkEngine._takeBuffer(b, outPtr.value, outLen.value) ?? '{}';
      final m = jsonDecode(json) as Map<String, dynamic>;
      final hits = ((m['hits'] as List?) ?? const [])
          .map((e) => SearchHit._(
              e['chapter'] as int,
              e['offset'] as int,
              e['len'] as int,
              e['snippet'] as String,
              e['snippetOffset'] as int))
          .toList();
      return SearchBatch._(
          hits, m['scanned'] as int, m['total'] as int, m['done'] as bool);
    } finally {
      malloc.free(q);
      malloc.free(outPtr);
      malloc.free(outLen);
    }
  }

  /// 开一个 BM25 索引构建会话（分步推进，可随时 `close` 放弃）。
  RrkBm25Index buildIndex() {
    _check();
    final b = _bindings;
    final id = b.bm25Open(_handle);
    if (id == 0) throw RrkException(1001, RrkEngine._lastError(b).message);
    return RrkBm25Index._(id);
  }

  /// 关闭（幂等；Finalizer 兜底但请显式调用）。
  void close() {
    if (_closed) return;
    _closed = true;
    final handle = _handle;
    _handle = 0;
    _finalizer.detach(this);
    final b = _b;
    if (b != null && handle != 0) {
      final code = b.close(handle);
      if (code != 0) {
        throw RrkException(code, RrkEngine._lastError(b).message);
      }
    }
  }
}

// ---------------------------------------------------------------------------
// 检索会话
// ---------------------------------------------------------------------------

/// 全书检索会话（句柄级）。
///
/// 拉模式：反复 [next] 取批，[SearchBatch.done] 为 true 即扫完；
/// 中途放弃请 [cancel]（真停，不留后台线程）。
class RrkSearchSession {
  RrkSearchSession._(this._id) {
    _finalizer.attach(this, _id, detach: this);
  }

  static final Finalizer<int> _finalizer = Finalizer((id) {
    final b = _b;
    if (b != null && id != 0) b.searchCancel(id);
  });

  int _id;
  bool _closed = false;

  int get id => _id;
  bool get isClosed => _closed;

  void _check() {
    if (_closed || _id == 0) {
      throw RrkException(1004, '检索会话已取消');
    }
  }

  /// 拉取下一批命中（[maxHits] 条以内）。
  SearchBatch next({int maxHits = 50}) {
    _check();
    final b = _bindings;
    final outPtr = malloc<Pointer<Uint8>>();
    final outLen = malloc<IntPtr>();
    try {
      final code = b.searchNext(_id, maxHits, outPtr, outLen);
      if (code != 0) {
        throw RrkException(code, RrkEngine._lastError(b).message);
      }
      final json = RrkEngine._takeBuffer(b, outPtr.value, outLen.value) ?? '{}';
      final m = jsonDecode(json) as Map<String, dynamic>;
      final hits = ((m['hits'] as List?) ?? const [])
          .map((e) => SearchHit._(e['chapter'] as int, e['offset'] as int, e['len'] as int,
              e['snippet'] as String, e['snippetOffset'] as int))
          .toList();
      return SearchBatch._(hits, m['scanned'] as int, m['total'] as int, m['done'] as bool);
    } finally {
      malloc.free(outPtr);
      malloc.free(outLen);
    }
  }

  /// 取消检索并释放内核侧会话（幂等）。
  void cancel() {
    if (_closed) return;
    _closed = true;
    final id = _id;
    _id = 0;
    _finalizer.detach(this);
    final b = _b;
    if (b != null && id != 0) {
      final code = b.searchCancel(id);
      if (code != 0) {
        throw RrkException(code, RrkEngine._lastError(b).message);
      }
    }
  }
}

// ---------------------------------------------------------------------------
// BM25 索引
// ---------------------------------------------------------------------------

/// BM25 索引会话：分步构建 → 拉取字节（宿主落盘）→ 查询排序。
class RrkBm25Index {
  RrkBm25Index._(this._id) {
    _finalizer.attach(this, _id, detach: this);
  }

  static final Finalizer<int> _finalizer = Finalizer((id) {
    final b = _b;
    if (b != null && id != 0) b.bm25Close(id);
  });

  /// 从宿主落盘的索引字节载入（跨启动复用，免重建）。
  static RrkBm25Index load(Uint8List bytes) {
    if (bytes.isEmpty) throw RrkException(1004, '索引字节为空');
    final b = _bindings;
    final src = malloc<Uint8>(bytes.length);
    try {
      src.asTypedList(bytes.length).setAll(0, bytes);
      final id = b.bm25Load(src, bytes.length);
      if (id == 0) throw RrkException(1001, RrkEngine._lastError(b).message);
      return RrkBm25Index._(id);
    } finally {
      malloc.free(src);
    }
  }

  int _id;
  bool _closed = false;

  int get id => _id;
  bool get isClosed => _closed;

  void _check() {
    if (_closed || _id == 0) {
      throw RrkException(1004, '索引会话已关闭');
    }
  }

  /// 推进构建（最多 [maxChapters] 章），返回进度；反复调用直到 `done`。
  Bm25Progress step({int maxChapters = 50}) {
    _check();
    final b = _bindings;
    final outPtr = malloc<Pointer<Uint8>>();
    final outLen = malloc<IntPtr>();
    try {
      final code = b.bm25Step(_id, maxChapters, outPtr, outLen);
      if (code != 0) throw RrkException(code, RrkEngine._lastError(b).message);
      final json = RrkEngine._takeBuffer(b, outPtr.value, outLen.value) ?? '{}';
      final m = jsonDecode(json) as Map<String, dynamic>;
      return Bm25Progress._(m['built'] as int, m['total'] as int, m['done'] as bool,
          m['terms'] as int, m['bytes'] as int);
    } finally {
      malloc.free(outPtr);
      malloc.free(outLen);
    }
  }

  /// 一步建完（阻塞；宿主想分批/可中断请自己循环 [step]）。
  Bm25Progress buildAll({int chunkChapters = 200}) {
    var p = step(maxChapters: chunkChapters);
    while (!p.done) {
      p = step(maxChapters: chunkChapters);
    }
    return p;
  }

  /// 拉取序列化索引的一个分块：返回 (字节, 下一个 offset)。
  (Uint8List, int) pullChunk({required int offset, int maxBytes = 1 << 20}) {
    _check();
    final b = _bindings;
    final outPtr = malloc<Pointer<Uint8>>();
    final outLen = malloc<IntPtr>();
    final outNext = malloc<Uint64>();
    try {
      final code = b.bm25Pull(_id, offset, maxBytes, outPtr, outLen, outNext);
      if (code != 0) throw RrkException(code, RrkEngine._lastError(b).message);
      final bytes = RrkEngine._takeBytes(b, outPtr.value, outLen.value) ?? Uint8List(0);
      return (bytes, outNext.value);
    } finally {
      malloc.free(outPtr);
      malloc.free(outLen);
      malloc.free(outNext);
    }
  }

  /// 一次取完整份索引字节（内部按 [chunkBytes] 分块拉取，避免单次大块跨边界）。
  Uint8List pullAll({int chunkBytes = 1 << 20}) {
    final builder = BytesBuilder(copy: false);
    var offset = 0;
    while (true) {
      final (chunk, next) = pullChunk(offset: offset, maxBytes: chunkBytes);
      if (chunk.isEmpty) break;
      builder.add(chunk);
      offset = next;
    }
    return builder.toBytes();
  }

  /// 查询：按 BM25 得分降序返回前 [topK] 章。
  List<Bm25Hit> query(String q, {int topK = 10}) {
    _check();
    final b = _bindings;
    final qp = q.toNativeUtf8();
    final outPtr = malloc<Pointer<Uint8>>();
    final outLen = malloc<IntPtr>();
    try {
      final code = b.bm25Query(_id, qp, utf8.encode(q).length, topK, outPtr, outLen);
      if (code != 0) throw RrkException(code, RrkEngine._lastError(b).message);
      final json = RrkEngine._takeBuffer(b, outPtr.value, outLen.value) ?? '[]';
      return (jsonDecode(json) as List<dynamic>)
          .map((e) => Bm25Hit._(e['chapter'] as int, (e['score'] as num).toDouble()))
          .toList();
    } finally {
      malloc.free(qp);
      malloc.free(outPtr);
      malloc.free(outLen);
    }
  }

  /// 关闭索引会话（释放内存；幂等）。
  void close() {
    if (_closed) return;
    _closed = true;
    final id = _id;
    _id = 0;
    _finalizer.detach(this);
    final b = _b;
    if (b != null && id != 0) {
      final code = b.bm25Close(id);
      if (code != 0) {
        throw RrkException(code, RrkEngine._lastError(b).message);
      }
    }
  }
}
