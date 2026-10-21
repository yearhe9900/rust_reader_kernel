// rrk_reader：Rust 阅读内核的 Dart 封装（手写 dart:ffi，直绑 C ABI）。
//
// 桥接口径（方案 §5）：
// - 句柄化：书 = u64 句柄；结构化数据走 JSON 缓冲，正文走原始 UTF-8 缓冲；
// - 错误传递：函数返回错误码（0=成功），详情经 rrk_last_error 取；
// - 资源释放：内核分配的缓冲由本封装自动 free，RrkBook.close() 必调（有 Finalizer 兜底）。

import 'dart:convert';
import 'dart:ffi';
import 'dart:typed_data';

import 'package:ffi/ffi.dart';

/// ABI 版本（与 rrk-ffi 的 RRK_ABI_VERSION 对齐，启动校验）。
const int kRrkAbiVersion = 1;

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
typedef _PagesNative = Uint32 Function(Uint64, Uint32, Float, Float, Float, Float,
    Float, Pointer<Pointer<Uint8>>, Pointer<IntPtr>);
typedef _PagesDart = int Function(int, int, double, double, double, double, double,
    Pointer<Pointer<Uint8>>, Pointer<IntPtr>);
typedef _RenderNative = Uint32 Function(
    Uint64, Uint32, Uint32, Pointer<Pointer<Uint8>>, Pointer<IntPtr>, Pointer<Uint32>, Pointer<Uint32>);
typedef _RenderDart = int Function(
    int, int, int, Pointer<Pointer<Uint8>>, Pointer<IntPtr>, Pointer<Uint32>, Pointer<Uint32>);
typedef _LoadChapterNative = Uint32 Function(
    Uint64, Uint32, Pointer<Pointer<Uint8>>, Pointer<IntPtr>, Pointer<Pointer<Uint8>>, Pointer<IntPtr>);
typedef _LoadChapterDart = int Function(
    int, int, Pointer<Pointer<Uint8>>, Pointer<IntPtr>, Pointer<Pointer<Uint8>>, Pointer<IntPtr>);
typedef _BufFreeNative = Void Function(Pointer<Uint8>, IntPtr);
typedef _BufFreeDart = void Function(Pointer<Uint8>, int);
typedef _AbiNative = Uint32 Function();
typedef _AbiDart = int Function();

class _Bindings {
  _Bindings(DynamicLibrary lib)
      : open = lib.lookupFunction<_OpenNative, _OpenDart>('rrk_open'),
        close = lib.lookupFunction<_CloseNative, _CloseDart>('rrk_close'),
        metaJson = lib.lookupFunction<_QueryNative, _QueryDart>('rrk_meta_json'),
        tocJson = lib.lookupFunction<_QueryNative, _QueryDart>('rrk_toc_json'),
        chaptersJson = lib.lookupFunction<_QueryNative, _QueryDart>('rrk_chapters_json'),
        cover = lib.lookupFunction<_QueryNative, _QueryDart>('rrk_cover'),
        lastError = lib.lookupFunction<_ErrorNative, _ErrorDart>('rrk_last_error'),
        loadChapter =
            lib.lookupFunction<_LoadChapterNative, _LoadChapterDart>('rrk_load_chapter'),
        pagesJson = lib.lookupFunction<_PagesNative, _PagesDart>('rrk_pages_json'),
        renderPage = lib.lookupFunction<_RenderNative, _RenderDart>('rrk_render_page'),
        bufFree = lib.lookupFunction<_BufFreeNative, _BufFreeDart>('rrk_buf_free'),
        abiVersion = lib.lookupFunction<_AbiNative, _AbiDart>('rrk_abi_version');

  final _OpenDart open;
  final _CloseDart close;
  final _QueryDart metaJson;
  final _QueryDart tocJson;
  final _QueryDart chaptersJson;
  final _QueryDart cover;
  final _ErrorDart lastError;
  final _LoadChapterDart loadChapter;
  final _PagesDart pagesJson;
  final _RenderDart renderPage;
  final _BufFreeDart bufFree;
  final _AbiDart abiVersion;
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
  /// [libPath]：rrk_ffi 动态库路径（如 `target/release/rrk_ffi.dll`）。
  static void init(String libPath) {
    final lib = DynamicLibrary.open(libPath);
    final b = _Bindings(lib);
    final abi = b.abiVersion();
    if (abi != kRrkAbiVersion) {
      throw RrkException(1999, 'ABI 版本不匹配：内核=$abi，封装=$kRrkAbiVersion');
    }
    _b = b;
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
      return RrkException(1001, msg ?? '未知错误');
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
// 数据模型
// ---------------------------------------------------------------------------

class BookMeta {
  BookMeta._(this.title, this.authors, this.language, this.format, this.contentHash);
  final String title;
  final List<String> authors;
  final String? language;
  final String format;
  final String contentHash;

  static BookMeta _fromJson(Map<String, dynamic> j) => BookMeta._(
      j['title'] as String,
      (j['authors'] as List).cast<String>(),
      j['language'] as String?,
      j['format'] as String,
      j['contentHash'] as String);

  @override
  String toString() => 'BookMeta($title / ${authors.join(",")} / $format)';
}

class TocNode {
  TocNode._(this.title, this.chapter, this.children);
  final String title;
  final int chapter;
  final List<TocNode> children;

  static List<TocNode> _listFromJson(List<dynamic> js) => js
      .map((e) => TocNode._(e['title'] as String, e['chapter'] as int,
          _listFromJson((e['children'] as List?) ?? const [])))
      .toList();
}

class ChapterEntry {
  ChapterEntry._(this.id, this.title, this.charLen, this.byteStart, this.byteEnd, this.part);
  final int id;
  final String? title;
  final int charLen;
  final int byteStart;
  final int byteEnd;
  final int part;

  static List<ChapterEntry> _listFromJson(List<dynamic> js) => js
      .map((e) => ChapterEntry._(e['id'] as int, e['title'] as String?, e['charLen'] as int,
          e['byteStart'] as int, e['byteEnd'] as int, e['part'] as int))
      .toList();

  @override
  String toString() => 'ChapterEntry(#$id ${title ?? "（无标题）"} ≈$charLen 字)';
}

class ChapterBlock {
  ChapterBlock._(this.type, this.text, this.level, this.indent, this.key, this.alt);
  final String type; // h / p / img / quote / hr
  final String? text;
  final int? level;
  final bool? indent;
  final String? key;
  final String? alt;

  static List<ChapterBlock> _listFromJson(List<dynamic> js) => js
      .map((e) => ChapterBlock._(e['type'] as String, e['text'] as String?,
          e['level'] as int?, e['indent'] as bool?, e['key'] as String?, e['alt'] as String?))
      .toList();

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

class ChapterContent {
  ChapterContent._(this.id, this.text, this.blocks);
  final int id;
  final String text;
  final List<ChapterBlock> blocks;
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

  /// 加载一章（正文 + 块结构）。
  ChapterContent loadChapter(int chapterId) {
    _check();
    final b = _bindings;
    final textPtr = malloc<Pointer<Uint8>>();
    final textLen = malloc<IntPtr>();
    final blocksPtr = malloc<Pointer<Uint8>>();
    final blocksLen = malloc<IntPtr>();
    try {
      final code =
          b.loadChapter(_handle, chapterId, textPtr, textLen, blocksPtr, blocksLen);
      if (code != 0) _throwCode(code);
      final text = RrkEngine._takeBuffer(b, textPtr.value, textLen.value) ?? '';
      final blocksJson = RrkEngine._takeBuffer(b, blocksPtr.value, blocksLen.value) ?? '[]';
      return ChapterContent._(
          chapterId,
          text,
          ChapterBlock._listFromJson(jsonDecode(blocksJson) as List<dynamic>));
    } finally {
      malloc.free(textPtr);
      malloc.free(textLen);
      malloc.free(blocksPtr);
      malloc.free(blocksLen);
    }
  }

  /// 排版一章 → 页数组（几何模式）。
  List<ReaderPage> pagesOf(
    int chapterId, {
    double fontSize = 18.0,
    double lineHeight = 1.6,
    double width = 390.0,
    double height = 842.0,
    double dpr = 3.0,
  }) {
    _check();
    final b = _bindings;
    final outPtr = malloc<Pointer<Uint8>>();
    final outLen = malloc<IntPtr>();
    try {
      final code = b.pagesJson(_handle, chapterId, fontSize, lineHeight, width, height, dpr,
          outPtr, outLen);
      if (code != 0) _throwCode(code);
      final json = RrkEngine._takeBuffer(b, outPtr.value, outLen.value) ?? '[]';
      return (jsonDecode(json) as List<dynamic>)
          .map((e) => ReaderPage._fromJson(e as Map<String, dynamic>))
          .toList();
    } finally {
      malloc.free(outPtr);
      malloc.free(outLen);
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
