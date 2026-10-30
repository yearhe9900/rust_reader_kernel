import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:flutter_reader/page_data.dart';
import 'package:flutter_reader/reading_page.dart';

void main() {
  const text = '第一章 测试\n这是正文内容。';

  PageData samplePage() => PageData(
        index: 0,
        start: 0,
        end: text.length * 3,
        height: 842,
        lines: [
          LineData(0, 18, 28, true),
          LineData(18, text.length * 3, 28, false),
        ],
        runs: [
          RunData(0, 0, 22, 24, true, false, 0, 18),
          RunData(1, 0, 50, 18, false, false, 18, text.length * 3),
        ],
      );

  testWidgets('阅读页渲染 run 文本并支持点按翻页', (tester) async {
    var prev = 0;
    var next = 0;
    await tester.pumpWidget(MaterialApp(
      home: Scaffold(
        body: ReadingPage(
          pages: [samplePage()],
          chapterText: text,
          fontFamily: '',
          controller: PageController(),
          onPageChanged: (_) {},
          onTapPrev: () => prev++,
          onTapNext: () => next++,
        ),
      ),
    ));

    // 自绘文本经 Semantics 暴露（可访问性口径）
    expect(find.bySemanticsLabel(RegExp('第一章')), findsOneWidget);
    expect(find.byType(CustomPaint), findsWidgets);

    // run 文本切片正确（Canvas 自绘不可被 finder 检索，走语义/纯函数双验）
    expect(runText(text, RunData(0, 0, 22, 24, true, false, 0, 18)), '第一章 测试\n这');

    // 点按热区
    await tester.tap(find.byKey(const Key('tap-next')));
    await tester.tap(find.byKey(const Key('tap-prev')));
    await tester.pump();
    expect(next, 1);
    expect(prev, 1);
  });

  test('UTF-8 偏移换算', () {
    const s = '甲a乙';
    // 甲=3B, a=1B, 乙=3B
    expect(utf8OffsetToUtf16(s, 0), 0);
    expect(utf8OffsetToUtf16(s, 3), 1);
    expect(utf8OffsetToUtf16(s, 4), 2);
    expect(utf8OffsetToUtf16(s, 7), 3);
  });
}
