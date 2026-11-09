# 贡献指南

感谢关注 rust_reader_kernel！提交 Issue / PR 前请阅读本指南。

## 开发环境

- Rust stable（`cargo` 可用即可）
- 可选：Dart 3.4+ / Flutter stable（涉及 `bindings/` 与 `demo/` 时）

```sh
cargo test          # 全量单测（含 200 章规模化语料回归）
cargo run --release -p rrk-inspect -- fixtures/sample.epub
bash scripts/check-abi.sh              # ABI 常量五处一致性
bash scripts/corpus_check.sh <书...>   # 本地真书端到端（可选）
```

**语料口径**：有版权的真书不入库。`crates/rrk-epub` 的 `corpus_scale` 用
确定性生成的 200 章 / 10 卷嵌套目录 / 逐章内链合成大书跑
`open → toc → 抽样 load_chapter → 偏移对拍 → open 索引缓存往返`，
挡住小样张测不出的索引重建类回归。本地想补真书，把文件放进
`fixtures/corpus/`（已 gitignore）后跑 `corpus_check.sh`；CI 检测到该目录
会自动执行，否则跳过。

## 流程

1. **Issue 优先**：行为变更/大特性先开 Issue 讨论口径，避免返工；
2. Fork + 特性分支（`feat/xxx`、`fix/xxx`）；
3. PR 前自检：
   - `cargo test` 全绿；
   - 涉及 Dart/Flutter：`dart analyze` / `flutter analyze` / `flutter test` 全绿；
   - 新增格式/功能带单测或 fixtures 样本；
4. PR 描述写清「动机 + 口径 + 验证方式」。

## 架构约束（务必先读）

以下红线来自项目设计决策，PR 不得违反：

1. **全自研红线**：格式解析/解压/排版/渲染管线自己实现；禁止引入 pdfium、mupdf、
   闭源 SDK、预编译黑盒二进制。C 绑定原则禁用（唯一例外见方案 §3.2）。
2. **依赖白名单**：第三方 crate 仅限方案 §3.2 登记的纯 Rust 件
   （MIT/Apache-2.0/BSD，可审计可 fork）。新增依赖需在 PR 中说明理由并更新白名单登记。
3. **解析/渲染分权**：格式知识不出适配器（`crates/rrk-*/`）；`rrk-model` 保持
   零 IO、零格式知识；排版是纯函数（`layout(chapter, style, viewport)`）。
4. **字体策略**：重排类一律系统字体栈；书内嵌字体仅固定版式（PDF 原版页/CBZ）保真使用。
5. **数据面不可变**：`LayoutPage` 为不可变产物，缓存 key = contentHash × styleHash ×
   viewportHash；变更缓存 payload 格式需同步升级 RLAY schema 号。
6. **panic 红线的范围**：`unwrap`/`expect`/`panic!` 的 clippy `deny` **只作用于
   `rrk-ffi`**（C ABI 出口一律 `guard`/`guard_val` + `catch_unwind`，panic 不得越界）；
   其它 crate 不启用该 deny。但全 workspace **取锁必须走
   `rrk_model::lock_recover`**，禁止 `.lock().unwrap()`——由
   `crates/rrk-model/tests/lock_discipline.rs` 扫描源码校验。
7. **ABI 常量五处同步**：`RRK_ABI_VERSION` / `kRrkAbiVersion` / `bindings/rrk.h` /
   `CHANGELOG.md` / `docs/ROADMAP.md`；`bash scripts/check-abi.sh` 是 CI 门禁，
   漏改即红（Dart `RrkEngine.init` 相等校验，没有灰度窗口）。

## 代码风格

- Rust：`cargo fmt` + `cargo clippy`（警告清零）；
- Dart/Flutter：`dart format` + `flutter analyze` 零 issue；
- 注释口径：模块级文档说明「是什么/边界在哪」，诚实标注未覆盖项（见 `rrk-pdf` 示例）。

## 提交信息

`类型: 概要`（中文或英文均可），类型取 `feat / fix / docs / chore / perf / test / refactor`。

## 行为准则

友善、就事论事；对事不对人。
