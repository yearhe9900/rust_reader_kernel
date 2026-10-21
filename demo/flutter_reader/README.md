# flutter_reader — Rust 阅读内核 Flutter 阅读页 demo

几何模式自绘：内核算排版（`pagesOf` → `LayoutPage` 行表 + 文本 run），Flutter 只按
`(x, baseline, fontSize)` 画字。支持：

- 逐页 PageView（点按左右翻页）
- 目录抽屉（章切换）
- 字号滑杆（改字号 → `styleHash` 变 → 重排版，缓存精确失效口径的真实演示）
- 页码进度

## 运行（一次性：生成平台 runner）

```sh
cd demo/flutter_reader
flutter create . --platforms=windows,android   # 生成平台 runner（一次性）
flutter pub get
```

## 运行（指定内核库与测试书）

```sh
flutter run -d windows --release ^
  --dart-define=RRK_LIB=D:/Workspace/personal/rust_reader_kernel/target/release/rrk_ffi.dll ^
  --dart-define=BOOK=D:/Workspace/personal/rust_reader_kernel/tmp_book/《我师兄实在太稳健了》（校对版全本）作者：言归正传.txt
```

Windows 用 `^` 续行（bash 用 `\`）。`rrk_ffi.dll` 先在仓库根 `cargo build --release -p rrk-ffi` 构建。

> **必须带 `--release`**：`flutter run` 默认构建 Debug 版，Debug 版依赖
> `MSVCP140D.dll`（VC++ Debug 运行时）——它只随 Visual Studio 安装、不进系统，
> 双击 exe 会弹「由于找不到 MSVCP140D.dll，无法继续执行代码」。
> Release 版链的是普通 `MSVCP140.dll`，任何机器都能跑（性能也对）。

## 验证（agent 可直接跑）

```sh
flutter analyze
flutter test
```
