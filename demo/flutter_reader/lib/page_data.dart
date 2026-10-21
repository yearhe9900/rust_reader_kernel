import 'package:rrk_reader/rrk_reader.dart';

/// 阅读页数据面（demo 本地视图模型，与 ReaderPage 解耦便于测试）。
class PageData {
  PageData({
    required this.index,
    required this.start,
    required this.end,
    required this.height,
    required this.lines,
    required this.runs,
  });

  final int index;
  final int start;
  final int end;
  final double height;
  final List<LineData> lines;
  final List<RunData> runs;

  static PageData fromReaderPage(ReaderPage p) => PageData(
        index: p.index,
        start: p.start,
        end: p.end,
        height: p.height,
        lines: p.lines
            .map((l) => LineData(l.start, l.end, l.height, l.paraStart))
            .toList(),
        runs: p.runs
            .map((r) =>
                RunData(r.line, r.x, r.baseline, r.fontSize, r.bold, r.italic, r.start, r.end))
            .toList(),
      );
}

class LineData {
  LineData(this.start, this.end, this.height, this.paraStart);
  final int start;
  final int end;
  final double height;
  final bool paraStart;
}

class RunData {
  RunData(this.line, this.x, this.baseline, this.fontSize, this.bold, this.italic, this.start,
      this.end);
  final int line;
  final double x;
  final double baseline;
  final double fontSize;
  final bool bold;
  final bool italic;

  /// 章文本 UTF-8 字节偏移（进度真值坐标系）。
  final int start;
  final int end;
}

/// UTF-8 字节偏移 → Dart String（UTF-16）下标。
int utf8OffsetToUtf16(String text, int byteOffset) {
  if (byteOffset <= 0) return 0;
  var bytes = 0;
  for (var i = 0; i < text.length; i++) {
    if (bytes >= byteOffset) return i;
    final c = text.codeUnitAt(i);
    if (c < 0x80) {
      bytes += 1;
    } else if (c < 0x800) {
      bytes += 2;
    } else if (c >= 0xD800 && c <= 0xDBFF) {
      bytes += 4; // 代理对
      i += 1;
    } else {
      bytes += 3;
    }
  }
  return text.length;
}

/// 取 run 文本（按 UTF-8 字节范围切片）。
String runText(String chapterText, RunData run) {
  final s = utf8OffsetToUtf16(chapterText, run.start);
  final e = utf8OffsetToUtf16(chapterText, run.end);
  return chapterText.substring(s, e.clamp(s, chapterText.length));
}
