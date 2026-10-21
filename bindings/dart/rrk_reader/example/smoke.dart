// 冒烟验证：dart run example/smoke.dart <rrk_ffi 动态库> <书路径>
//
// 走通 Dart → C ABI → Rust 内核全链路：打开书 → meta/toc/章索引 → 加载一章。

import 'package:rrk_reader/rrk_reader.dart';

void main(List<String> args) {
  if (args.length < 2) {
    print('用法: dart run example/smoke.dart <rrk_ffi.dll/.so/.dylib> <书路径>');
    return;
  }
  final sw = Stopwatch()..start();
  RrkEngine.init(args[0]);
  print('ABI 版本校验通过 (${kRrkAbiVersion})');

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

  book.close();
  print('关闭成功，全链路 OK（${sw.elapsedMilliseconds} ms）');
}
