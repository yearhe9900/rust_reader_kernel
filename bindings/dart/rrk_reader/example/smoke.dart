// 冒烟验证：dart run example/smoke.dart <rrk_ffi 动态库> <书路径>
//
// 走通 Dart → C ABI → Rust 内核全链路：打开书 → meta/toc/章索引 → 加载一章；
// 末尾另冒烟 ABI 12 的新出口（内存开书 / open 索引缓存独立开关 / 磁盘配额）。

import 'dart:io';
import 'dart:typed_data';

import 'package:rrk_reader/rrk_reader.dart';

void main(List<String> args) {
  if (args.length < 2) {
    print('用法: dart run example/smoke.dart <rrk_ffi.dll/.so/.dylib> <书路径>');
    return;
  }
  final sw = Stopwatch()..start();
  RrkEngine.init(args[0]);
  print('ABI 版本校验通过 (${kRrkAbiVersion})');

  RrkEngine.configureCache('${Directory.systemTemp.path}/rrk_smoke_cache');
  final book = RrkEngine.openBook(args[1]);
  print('打开耗时: ${sw.elapsedMilliseconds} ms');

  final meta = book.meta();
  print('书目: $meta');

  final toc = book.toc();
  print('目录: ${toc.length} 项');
  for (final n in toc.take(5)) {
    print('  → [${n.chapter}] ${n.title}');
  }

  final chapters = book.chapters();
  print('章索引: ${chapters.length} 章');
  for (final c in chapters.take(5)) {
    print('  $c');
  }

  if (chapters.isNotEmpty) {
    final id = chapters.length > 1 ? 1 : 0;
    final content = book.loadChapter(id);
    print('章 #$id: ${content.text.length} 字符 / ${content.blocks.length} 块');
    for (final b in content.blocks.take(3)) {
      final t = (b.text ?? b.key ?? '');
      print('  [${b.type}] ${t.length > 40 ? t.substring(0, 40) : t}');
    }
  }

  final cover = book.cover();
  print('封面: ${cover == null ? "无" : "${cover.length} 字节"}');

  if (chapters.isNotEmpty) {
    final pages = book.pagesOf(chapters.length > 1 ? 1 : 0, width: 390, height: 842);
    print('pagesOf: ${pages.length} 页');
    for (final pg in pages.take(2)) {
      print('  页 ${pg.index}: ${pg.lines.length} 行 / ${pg.runs.length} run '
          '(${pg.start}..${pg.end})');
    }
  }

  if (chapters.isNotEmpty) {
    final t0 = Stopwatch()..start();
    final again = book.pagesOf(chapters.length > 1 ? 1 : 0, width: 390, height: 842);
    final cachedMs = t0.elapsedMicroseconds / 1000.0;
    final page = book.pageAt(chapters.length > 1 ? 1 : 0, again.first.start, width: 390, height: 842);
    print('缓存命中 pagesOf: ${again.length} 页（${cachedMs.toStringAsFixed(1)} ms）; '
        'pageAt(start)=${page}');
  }

  book.close();

  // ABI 12 新出口冒烟：独立 open 索引缓存 / 磁盘配额 / 内存开书
  RrkEngine.configureOpenIndexCache('${Directory.systemTemp.path}/rrk_smoke_openidx');
  RrkEngine.setCacheDiskLimit(64 * 1024 * 1024);
  final bytes = File(args[1]).readAsBytesSync();
  // 沿用真实文件名（含扩展名）做嗅探：避免任意书都按 .txt 解析，
  // 同时让内存开书与路径开书的 meta/title 口径一致。
  final memName = File(args[1]).uri.pathSegments.last;
  final memBook = RrkEngine.openBookBytes(Uint8List.fromList(bytes), name: memName);
  print('内存开书: ${memBook.chapters().length} 章');
  memBook.close();
  RrkEngine.setCacheDiskLimit(0); // 复原

  print('关闭成功，全链路 OK（${sw.elapsedMilliseconds} ms）');
}
