#!/usr/bin/env bash
# 深审 C4：本地「真书语料」回归入口。
#
# 有版权的真实书籍不入库，CI 默认跑的是 `cargo test -p rrk-epub corpus_scale`
# （确定性生成的 200 章合成大书）。本脚本供接入方 / 维护者在本地把真书
# 放进 fixtures/corpus/ 后跑端到端：open → toc → 首/中/尾章 load_chapter。
#
# 用法：bash scripts/corpus_check.sh <书1> [书2 ...]
#   CI 中若存在 fixtures/corpus/ 目录会自动调用（见 .github/workflows/ci.yml）。
set -euo pipefail

if [ "$#" -eq 0 ]; then
  echo "用法: bash scripts/corpus_check.sh <书1> [书2 ...]" >&2
  exit 2
fi

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"
cargo build --release -p rrk-inspect
bin="target/release/rrk-inspect"
[ -x "$bin.exe" ] && bin="$bin.exe"   # Windows / git-bash

for book in "$@"; do
  if [ ! -f "$book" ]; then
    echo "跳过（不是文件）: $book" >&2
    continue
  fi
  echo "=== 真书语料: $book ==="
  # 第一次只取章索引与目录（不载章），顺便解析章数
  overview="$($bin "$book" --toc --blocks 0)"
  count="$(printf '%s\n' "$overview" | sed -nE 's/^== 章索引 ==（([0-9]+) 章.*/\1/p' | head -n 1)"
  if [ -z "$count" ] || [ "$count" -lt 1 ]; then
    echo "无法解析章数，按单章书处理" >&2
    count=1
  fi
  mid=$((count / 2))
  last=$((count - 1))
  for c in 0 "$mid" "$last"; do
    printf '  load_chapter(%s) ... ' "$c"
    $bin "$book" --chapter "$c" --blocks 3 >/dev/null
    echo OK
  done
done
echo "真书语料回归 OK（$# 本）"
