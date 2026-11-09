#!/usr/bin/env bash
# 深审 B3：ABI 常量一致性门禁。
#
# 五处人工同步的 ABI 号必须逐字相等（提交 d40f59f 就曾漏同步一次）：
#   1. crates/rrk-ffi/src/lib.rs   pub const RRK_ABI_VERSION
#   2. bindings/dart/.../rrk_reader.dart  const int kRrkAbiVersion
#   3. bindings/rrk.h              #define RRK_ABI_VERSION
#   4. CHANGELOG.md                最上方一条 `**ABI 旧 → 新**`
#   5. docs/ROADMAP.md             「已落地（截至 … / ABI N）」标题
#
# 只认当前值：CHANGELOG 的历史版本行、docs/分析-* 的历史基线、dist/ 下的
# 发布快照都不参与比对。任何一处解析不到或不等即失败（退出码 1/2）。
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

num() { sed -nE "s/$2/\1/p" "$1" | head -n 1 | tr -d '[:space:]'; }

rust="$(num crates/rrk-ffi/src/lib.rs '^pub const RRK_ABI_VERSION: u32 = ([0-9]+);.*')"
if [[ -z "$rust" ]]; then
  echo "ABI 一致性：无法解析基准常量 RRK_ABI_VERSION" >&2
  exit 2
fi

check() {
  local label="$1" value="$2"
  printf '  %-26s %s\n' "$label" "${value:-<解析失败>}"
  if [[ "$value" != "$rust" ]]; then
    echo "ABI 一致性失败：$label = '${value:-未解析}'，应为 $rust" >&2
    echo "→ 升级 ABI 时五处必须同批修改（Rust / Dart / 头文件 / CHANGELOG / ROADMAP）" >&2
    exit 1
  fi
}

echo "ABI 一致性（基准：Rust RRK_ABI_VERSION = $rust）"
printf '  %-26s %s\n' 'Rust RRK_ABI_VERSION' "$rust"
check 'Dart kRrkAbiVersion' \
  "$(num bindings/dart/rrk_reader/lib/rrk_reader.dart '^const int kRrkAbiVersion = ([0-9]+);.*')"
check 'bindings/rrk.h' \
  "$(num bindings/rrk.h '^#define RRK_ABI_VERSION ([0-9]+).*')"
check 'CHANGELOG 当前版本' \
  "$(num CHANGELOG.md '^\*\*ABI [0-9]+ [^0-9]*([0-9]+)\*\*.*')"
check 'ROADMAP 已落地标题' \
  "$(num docs/ROADMAP.md '^## .*ABI ([0-9]+).*')"
echo "ABI 一致性 OK（$rust）"
