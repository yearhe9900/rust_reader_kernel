// 页级格式冒烟：dart run example/paged_smoke.dart <rrk_ffi 动态库> <CBZ/PDF 路径>
//
// 验证统一句柄的页级出口：kind / pageCount / loadPage / loadResource。
import 'dart:io';

import 'package:rrk_reader/rrk_reader.dart';

void main(List<String> args) {
  if (args.length < 2) {
    stderr.writeln('用法: dart run example/paged_smoke.dart <库路径> <CBZ/PDF>');
    exit(2);
  }
  RrkEngine.init(args[0]);
  final book = RrkEngine.openBook(args[1]);
  print('书名: ${book.meta().title}');
  print('格式: ${book.meta().format}');
  print('类型: ${book.kind}（0=Text 1=ImageGallery 2=Paged）');
  final n = book.pageCount();
  print('页数: $n');
  if (n > 0) {
    final p = book.loadPage(0);
    print('第 0 页: kind=${p.kind} mime=${p.mime ?? "-"} key=${p.imageKey ?? "-"}');
    if (p.imageKey != null) {
      final bytes = book.loadResource(p.imageKey!);
      print('页图字节: ${bytes.length}');
    }
    if (p.text != null) {
      print('页文本: ${p.text!.length} 字符');
    }
  }
  book.close();
  print('页级全链路 OK');
}
