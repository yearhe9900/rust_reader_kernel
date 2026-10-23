// 检索冒烟：dart run example/search_smoke.dart <rrk_ffi 动态库> <书路径> <检索词>
//
// 验证 issue #9 的两条能力：
//   1. book.search()：拉模式分块取命中（带进度、可取消）
//   2. book.buildIndex()：分步建 BM25 索引 → 拉字节 → 落盘往返（load）→ 排序查询
import 'dart:io';
import 'dart:typed_data';

import 'package:rrk_reader/rrk_reader.dart';

void main(List<String> args) {
  if (args.length < 3) {
    stderr.writeln('用法: dart run example/search_smoke.dart <库路径> <书> <检索词>');
    exit(2);
  }
  final libPath = args[0];
  final bookPath = args[1];
  final query = args[2];

  RrkEngine.init(libPath);
  final book = RrkEngine.openBook(bookPath);
  final meta = book.meta();
  final chapters = book.chapters();
  print('书名: ${meta.title}（${meta.format}，${chapters.length} 章）');

  final titleOf = <int, String>{
    for (final c in chapters) c.id: c.title ?? '#${c.id}',
  };

  // ---- 1. 全书搜索：拉模式分块 ----
  final sw = Stopwatch()..start();
  final session = book.search(query, contextChars: 24);
  var total = 0;
  var batches = 0;
  var firstShown = 0;
  SearchBatch batch;
  do {
    batch = session.next(maxHits: 20);
    batches++;
    for (final h in batch.hits) {
      if (firstShown < 5) {
        print('  #${h.chapter} @${h.offset} ${h.snippet}');
        firstShown++;
      }
      total++;
    }
  } while (!batch.done);
  sw.stop();
  print('搜索: 命中 $total 条 / $batches 批 / 扫完 ${batch.scanned}/${batch.total} 章 '
      '/ ${sw.elapsedMilliseconds} ms');
  session.cancel();

  // 已取消的会话再拉应抛错
  try {
    session.next();
    stderr.writeln('已取消的会话仍可 next，行为异常');
    exit(1);
  } on RrkException catch (e) {
    print('取消后 next: ${e.code} ${e.message}');
  }

  // ---- 2. BM25：分步建 → 拉字节 → 重新载入 → 查询 ----
  if (total == 0) {
    print('BM25: 检索词无命中，仍建索引验证链路');
  }
  final sw2 = Stopwatch()..start();
  final index = book.buildIndex();
  var progress = index.step(maxChapters: 32);
  print('  建索引首步: $progress');
  while (!progress.done) {
    progress = index.step(maxChapters: 256);
  }
  sw2.stop();
  print('BM25 建完: $progress（${sw2.elapsedMilliseconds} ms）');

  final bytes = index.pullAll(chunkBytes: 256 * 1024);
  print('  索引字节: ${bytes.length}（分块拉取，头 4 字节=${bytes.sublist(0, 4)}）');

  final hits = index.query(query, topK: 5);
  print('  BM25 查询 "$query" → ${hits.length} 章');
  for (final h in hits) {
    print('    #${h.chapter} ${titleOf[h.chapter] ?? ''} score=${h.score.toStringAsFixed(4)}');
  }
  index.close();

  // 宿主落盘 → 重新载入 → 同样的查询结果（往返一致）
  final restored = RrkBm25Index.load(Uint8List.fromList(bytes));
  final hits2 = restored.query(query, topK: 5);
  final same = hits2.length == hits.length &&
      List.generate(hits.length, (i) => hits[i].chapter == hits2[i].chapter).every((v) => v);
  print('  落盘往返后查询一致: $same');
  restored.close();

  // 空词校验
  try {
    book.search('   ');
    stderr.writeln('空检索词应报错');
    exit(1);
  } on RrkException catch (e) {
    print('空检索词: ${e.code} ${e.message}');
  }

  book.close();
  print('检索全链路 OK');
}
