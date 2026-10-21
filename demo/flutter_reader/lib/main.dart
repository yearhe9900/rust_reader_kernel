import 'package:flutter/material.dart';
import 'package:rrk_reader/rrk_reader.dart';

import 'page_data.dart';
import 'reading_page.dart';

/// Flutter 阅读页 demo。
///
/// 运行（平台 runner 生成与运行命令见 README.md）：
/// ```sh
/// flutter run \
///   --dart-define=RRK_LIB=D:/Workspace/personal/rust_reader_kernel/target/release/rrk_ffi.dll \
///   --dart-define=BOOK=D:/Workspace/personal/rust_reader_kernel/tmp_book/xxx.txt
/// ```
void main() {
  runApp(const ReaderApp());
}

class ReaderApp extends StatelessWidget {
  const ReaderApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'rrk_reader demo',
      theme: ThemeData(colorSchemeSeed: Colors.indigo, useMaterial3: true),
      home: const ReaderHomePage(),
    );
  }
}

class ReaderHomePage extends StatefulWidget {
  const ReaderHomePage({super.key});

  @override
  State<ReaderHomePage> createState() => _ReaderHomePageState();
}

class _ReaderHomePageState extends State<ReaderHomePage> {
  static const _libPath = String.fromEnvironment('RRK_LIB');
  static const _bookPath = String.fromEnvironment('BOOK');

  RrkBook? _book;
  String? _error;
  List<ChapterEntry> _chapters = [];
  int _chapterId = 0;
  List<PageData> _pages = [];
  String _chapterText = '';
  double _fontSize = 18;
  int _pageIndex = 0;
  final _controller = PageController();

  @override
  void initState() {
    super.initState();
    _boot();
  }

  void _boot() {
    try {
      if (_libPath.isEmpty || _bookPath.isEmpty) {
        setState(() => _error = '缺少 --dart-define=RRK_LIB / BOOK（见 README）');
        return;
      }
      RrkEngine.init(_libPath);
      _book = RrkEngine.openBook(_bookPath);
      _chapters = _book!.chapters();
      if (_chapters.isNotEmpty) {
        _chapterId = _chapters.first.id;
        _reloadPages();
      }
    } catch (e) {
      setState(() => _error = '$e');
    }
  }

  void _reloadPages() {
    final book = _book;
    if (book == null) return;
    final media = MediaQuery.of(context);
    final content = book.loadChapter(_chapterId);
    final pages = book.pagesOf(
      _chapterId,
      fontSize: _fontSize,
      width: media.size.width,
      height: media.size.height,
      dpr: media.devicePixelRatio,
    );
    setState(() {
      _chapterText = content.text;
      _pages = pages.map(PageData.fromReaderPage).toList();
    });
    if (_controller.hasClients) {
      _controller.jumpToPage(0);
    }
    setState(() => _pageIndex = 0);
  }

  void _goRelative(int delta) {
    final next = (_pageIndex + delta).clamp(0, _pages.length - 1);
    if (next != _pageIndex) {
      _controller.animateToPage(next,
          duration: const Duration(milliseconds: 180), curve: Curves.easeOut);
    }
  }

  @override
  Widget build(BuildContext context) {
    if (_error != null) {
      return Scaffold(body: Center(child: Text(_error!)));
    }
    final book = _book;
    if (book == null) {
      return const Scaffold(body: Center(child: CircularProgressIndicator()));
    }
    return Scaffold(
      appBar: AppBar(
        title: Text(book.meta().title, overflow: TextOverflow.ellipsis),
        actions: [
          Center(
            child: Text(
              _pages.isEmpty ? '' : '${_pageIndex + 1}/${_pages.length}',
              key: const Key('page-progress'),
            ),
          ),
          IconButton(
            icon: const Icon(Icons.toc),
            onPressed: _showToc,
          ),
        ],
        bottom: PreferredSize(
          preferredSize: const Size.fromHeight(24),
          child: Slider(
            key: const Key('font-slider'),
            value: _fontSize,
            min: 14,
            max: 28,
            divisions: 14,
            onChanged: (v) => setState(() => _fontSize = v),
            onChangeEnd: (_) => _reloadPages(),
          ),
        ),
      ),
      body: _pages.isEmpty
          ? const Center(child: Text('（空章）'))
          : ReadingPage(
              pages: _pages,
              chapterText: _chapterText,
              controller: _controller,
              onPageChanged: (i) => setState(() => _pageIndex = i),
              onTapPrev: () => _goRelative(-1),
              onTapNext: () => _goRelative(1),
            ),
    );
  }

  void _showToc() {
    showModalBottomSheet<void>(
      context: context,
      builder: (context) => ListView(
        children: [
          for (final c in _chapters)
            ListTile(
              title: Text(c.title ?? '第 ${c.id} 章'),
              selected: c.id == _chapterId,
              onTap: () {
                Navigator.pop(context);
                setState(() => _chapterId = c.id);
                _reloadPages();
              },
            ),
        ],
      ),
    );
  }
}
