import 'package:flutter/material.dart';

import 'page_data.dart';

/// 阅读页（几何模式自绘）：PageView 逐页消费 `LayoutPage`，
/// 每个文本 run 按 (x, baseline, fontSize) 定位——布局由内核算，Flutter 只画。
class ReadingPage extends StatelessWidget {
  const ReadingPage({
    super.key,
    required this.pages,
    required this.chapterText,
    required this.fontFamily,
    required this.controller,
    required this.onPageChanged,
    required this.onTapPrev,
    required this.onTapNext,
  });

  final List<PageData> pages;
  final String chapterText;
  /// 与排版同款字体：绘制字体必须与断行字体一致，否则切字体后行宽错位
  /// （排版按 fontFamily 断行、绘制用默认字体 → 字形宽度对不上）。
  final String fontFamily;
  final PageController controller;
  final ValueChanged<int> onPageChanged;
  final VoidCallback onTapPrev;
  final VoidCallback onTapNext;

  @override
  Widget build(BuildContext context) {
    return Stack(
      children: [
        PageView.builder(
          controller: controller,
          itemCount: pages.length,
          onPageChanged: onPageChanged,
          itemBuilder: (context, i) => Semantics(
            // 自绘文本不可被读屏检索 → 显式语义标签（几何模式的可访问性口径）
            container: true,
            label: pages[i].runs.map((r) => runText(chapterText, r)).join(),
            child: CustomPaint(
              painter: _PagePainter(pages[i], chapterText, fontFamily),
              size: Size.infinite,
            ),
          ),
        ),
        // 点按翻页热区（左 1/3 上一页，右 1/3 下一页）
        Row(
          children: [
            Expanded(
              child: GestureDetector(
                key: const Key('tap-prev'),
                onTap: onTapPrev,
                behavior: HitTestBehavior.translucent,
                child: const SizedBox.expand(),
              ),
            ),
            const Expanded(child: SizedBox.expand()),
            Expanded(
              child: GestureDetector(
                key: const Key('tap-next'),
                onTap: onTapNext,
                behavior: HitTestBehavior.translucent,
                child: const SizedBox.expand(),
              ),
            ),
          ],
        ),
      ],
    );
  }
}

class _PagePainter extends CustomPainter {
  _PagePainter(this.page, this.chapterText, this.fontFamily);

  final PageData page;
  final String chapterText;
  final String fontFamily;

  @override
  void paint(Canvas canvas, Size size) {
    for (final run in page.runs) {
      final text = runText(chapterText, run);
      if (text.isEmpty) continue;
      final painter = TextPainter(
        text: TextSpan(
          text: text,
          style: TextStyle(
            fontFamily: fontFamily.isEmpty ? null : fontFamily,
            fontSize: run.fontSize,
            fontWeight: run.bold ? FontWeight.bold : FontWeight.normal,
            fontStyle: run.italic ? FontStyle.italic : FontStyle.normal,
            color: const Color(0xFF222222),
          ),
        ),
        textDirection: TextDirection.ltr,
      )..layout();
      painter.paint(canvas, Offset(run.x, run.baseline - run.fontSize));
    }
  }

  @override
  bool shouldRepaint(_PagePainter old) =>
      old.page != page ||
      old.chapterText != chapterText ||
      old.fontFamily != fontFamily;
}
