import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:flutter/material.dart';
import 'package:rrk_reader/rrk_reader.dart';

import 'page_data.dart';
import 'reading_page.dart';

/// Flutter 阅读页 demo。
///
/// 能力：逐页阅读（几何自绘）/ 目录 / 字号字体切换（styleHash 精确失效）/
/// 进度保存恢复（SemanticPos 真值）/ PDF 原版页位图模式（renderPage）/
/// 三要素布局缓存（命中免排版）。
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

/// 系统字体族预设（字体策略：重排类一律系统字体）。
const kFontOptions = <String, String>{
  '默认(雅黑)': 'Microsoft YaHei',
  '宋体': 'SimSun',
  '黑体': 'SimHei',
  '楷体': 'KaiTi',
  '仿宋': 'FangSong',
};

class ReaderHomePage extends StatefulWidget {
  const ReaderHomePage({super.key});

  @override
  State<ReaderHomePage> createState() => _ReaderHomePageState();
}

class _ReaderHomePageState extends State<ReaderHomePage> with WidgetsBindingObserver {
  static const _libPath = String.fromEnvironment('RRK_LIB');
  static const _bookPath = String.fromEnvironment('BOOK');

  RrkBook? _book;
  String? _error;
  List<ChapterEntry> _chapters = [];
  int _chapterId = 0;
  List<PageData> _pages = [];
  String _chapterText = '';
  double _fontSize = 18;
  String _fontFamily = kFontOptions.values.first;
  int _pageIndex = 0;
  bool _isPdf = false;
  final _controller = PageController();

  File get _progressFile =>
      File('${Directory.systemTemp.path}/rrk_reader_progress.json');

  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addObserver(this);
    _boot();
  }

  @override
  void dispose() {
    WidgetsBinding.instance.removeObserver(this);
    _saveProgress();
    super.dispose();
  }

  @override
  void didChangeAppLifecycleState(AppLifecycleState state) {
    if (state == AppLifecycleState.paused || state == AppLifecycleState.hidden) {
      _saveProgress(); // 停稳才写（§4.6 口径）
    }
  }

  void _boot() {
    try {
      if (_libPath.isEmpty || _bookPath.isEmpty) {
        setState(() => _error = '缺少 --dart-define=RRK_LIB / BOOK（见 README）');
        return;
      }
      RrkEngine.init(_libPath);
      RrkEngine.configureCache(
          '${Directory.systemTemp.path}/rrk_reader_cache');
      _book = RrkEngine.openBook(_bookPath);
      _isPdf = _book!.meta().format == 'PDF';
      _chapters = _book!.chapters();
      if (_chapters.isNotEmpty) {
        _chapterId = _chapters.first.id;
        _restoreProgress();
      }
    } catch (e) {
      setState(() => _error = '$e');
    }
  }

  /// 进度恢复：真值 (chapterId, charOffset) → pageAt → 跳页（§4.6）。
  void _restoreProgress() {
    try {
      if (_progressFile.existsSync()) {
        final m =
            jsonDecode(_progressFile.readAsStringSync()) as Map<String, dynamic>;
        if (m['book'] == _bookPath) {
          _chapterId = m['chapter'] as int;
          final offset = m['offset'] as int;
          _reloadPages();
          final media = MediaQuery.of(context);
          final page = _book!.pageAt(_chapterId, offset,
              fontFamily: _fontFamily,
              fontSize: _fontSize,
              width: media.size.width,
              height: media.size.height,
              dpr: media.devicePixelRatio);
          if (_controller.hasClients) _controller.jumpToPage(page);
          setState(() => _pageIndex = page);
          return;
        }
      }
    } catch (_) {
      // 损坏进度 = 静默回开头（损坏≠判据失效）
    }
    _reloadPages();
  }

  void _saveProgress() {
    final book = _book;
    if (book == null || _pages.isEmpty) return;
    final offset = _pages[_pageIndex.clamp(0, _pages.length - 1)].start;
    _progressFile.writeAsStringSync(jsonEncode({
      'book': _bookPath,
      'chapter': _chapterId,
      'offset': offset,
    }));
  }

  void _reloadPages() {
    final book = _book;
    if (book == null) return;
    final media = MediaQuery.of(context);
    if (_isPdf) {
      setState(() {});
      return;
    }
    final content = book.loadChapter(_chapterId);
    final pages = book.pagesOf(
      _chapterId,
      fontFamily: _fontFamily,
      fontSize: _fontSize,
      width: media.size.width,
      height: media.size.height,
      dpr: media.devicePixelRatio,
    );
    // 远跳预取：当前 + 相邻章（保护窗口）
    book.prefetch(_chapterId,
        fontFamily: _fontFamily,
        fontSize: _fontSize,
        width: media.size.width,
        height: media.size.height,
        dpr: media.devicePixelRatio);
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
    if (_pages.isEmpty) return;
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
              _isPdf
                  ? '${_pageIndex + 1} 页'
                  : '${_pageIndex + 1}/${_pages.length}',
              key: const Key('page-progress'),
            ),
          ),
          if (!_isPdf)
            DropdownButton<String>(
              value: _fontFamily,
              underline: const SizedBox.shrink(),
              items: [
                for (final e in kFontOptions.entries)
                  DropdownMenuItem(value: e.value, child: Text(e.key)),
              ],
              onChanged: (v) {
                if (v != null) {
                  setState(() => _fontFamily = v);
                  _reloadPages();
                }
              },
            ),
          IconButton(icon: const Icon(Icons.toc), onPressed: _showToc),
        ],
        bottom: _isPdf
            ? null
            : PreferredSize(
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
      body: _isPdf ? _buildPdfView(book) : _buildReflowView(),
    );
  }

  Widget _buildReflowView() {
    return _pages.isEmpty
        ? const Center(child: Text('（空章）'))
        : ReadingPage(
            pages: _pages,
            chapterText: _chapterText,
            fontFamily: _fontFamily,
            controller: _controller,
            onPageChanged: (i) {
              setState(() => _pageIndex = i);
              _saveProgress();
            },
            onTapPrev: () => _goRelative(-1),
            onTapNext: () => _goRelative(1),
          );
  }

  /// PDF 原版页模式：renderPage 位图翻页（模式 B）。
  Widget _buildPdfView(RrkBook book) {
    // PDF 章的 byte_range 是页区间（0, 页数）→ 末页 = 各章 end 的最大值
    final pageCount =
        _chapters.map((c) => c.byteEnd).fold<int>(0, (a, b) => a > b ? a : b);
    return PageView.builder(
      controller: _controller,
      itemCount: pageCount,
      onPageChanged: (i) {
        setState(() => _pageIndex = i);
        _saveProgress();
      },
      itemBuilder: (context, i) {
        final (rgba, w, h) = book.renderPage(i, targetWidth: 900);
        return InteractiveViewer(
          child: Image.memory(
            Uint8List.fromList(rgba),
            gaplessPlayback: true,
            fit: BoxFit.contain,
            key: ValueKey('pdf-page-$i-$w-$h'),
          ),
        );
      },
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
                _book?.prefetch(_chapterId);
              },
            ),
        ],
      ),
    );
  }
}
