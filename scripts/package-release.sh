#!/usr/bin/env bash
# package-release.sh — 生成 rrk_reader 可移植 release 包
#
# 用法：
#   bash scripts/package-release.sh                  # 本机(Windows) + Android 四 ABI
#   SKIP_WINDOWS=1 bash scripts/package-release.sh   # 只打 Android
#   SKIP_ANDROID=1 bash scripts/package-release.sh   # 只打 Windows
#
# 产物：
#   dist/rrk_reader-<version>/        目录（lib 各平台库 + dart 包 + MANIFEST + README）
#   dist/rrk_reader-<version>.zip     压缩包
#
# 依赖：cargo；Android 需 NDK（自动探测 ANDROID_NDK_HOME / ANDROID_SDK_ROOT / SDK 默认路径）。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

VERSION="$(grep -m1 '^version[[:space:]]*=' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/' | tr -d '\r')"
ABI="$(grep -m1 'RRK_ABI_VERSION[[:space:]]*:[[:space:]]*u32[[:space:]]*=' crates/rrk-ffi/src/lib.rs \
  | sed -E 's/.*=[[:space:]]*([0-9]+).*/\1/' | tr -d '\r')"
PKG="rrk_reader"
OUT="$ROOT/dist/${PKG}-${VERSION}"
BUILT_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

say()  { printf '\033[1;36m[release]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[release]\033[0m %s\n' "$*" >&2; }

hash256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum   >/dev/null 2>&1; then shasum -a 256 "$1" | cut -d' ' -f1
  else echo "unknown"; fi
}

say "rrk_reader v${VERSION}（ABI ${ABI}）"
rm -rf "$OUT"
mkdir -p "$OUT/lib"

# ---------------------------------------------------------------- Windows (native)
if [ "${SKIP_WINDOWS:-0}" != "1" ]; then
  say "构建 windows-x86_64 …"
  cargo build --release -p rrk-ffi
  mkdir -p "$OUT/lib/windows-x86_64"
  cp target/release/rrk_ffi.dll "$OUT/lib/windows-x86_64/"
else
  warn "SKIP_WINDOWS=1，跳过 Windows"
fi

# ---------------------------------------------------------------- Android (4 ABI)
detect_ndk() {
  if [ -n "${ANDROID_NDK_HOME:-}" ] && [ -d "${ANDROID_NDK_HOME}" ]; then
    echo "$ANDROID_NDK_HOME"; return 0
  fi
  local base
  for base in "${ANDROID_SDK_ROOT:-}" "${ANDROID_HOME:-}" \
              "$HOME/AppData/Local/Android/Sdk" "$HOME/Library/Android/sdk" "$HOME/Android/Sdk"; do
    [ -n "$base" ] && [ -d "$base/ndk" ] && {
      ls -d "$base"/ndk/* 2>/dev/null | sort -V | tail -1; return 0
    }
  done
  return 1
}

if [ "${SKIP_ANDROID:-0}" != "1" ]; then
  if NDK="$(detect_ndk)"; then
    case "$(uname -s)" in
      MINGW*|MSYS*|CYGWIN*) HOST_TAG=windows-x86_64 ;;
      Darwin)               HOST_TAG=darwin-x86_64 ;;
      *)                    HOST_TAG=linux-x86_64 ;;
    esac
    NDKBIN="$NDK/toolchains/llvm/prebuilt/$HOST_TAG/bin"
    if [ ! -d "$NDKBIN" ]; then
      warn "NDK bin 目录不存在：$NDKBIN（跳过 Android）"
    else
      say "NDK: $NDK"
      build_android() {
        local rust_triple="$1" cc_name="$2" abi_dir="$3"
        local cc="$NDKBIN/${cc_name}"
        [ -f "$cc.cmd" ] && cc="$cc.cmd"
        local env_triple cc_triple
        env_triple="$(echo "$rust_triple" | tr '[:lower:]-' '[:upper:]_')"
        cc_triple="${rust_triple//-/_}"
        say "构建 android-$abi_dir（$rust_triple）…"
        env \
          "CARGO_TARGET_${env_triple}_LINKER=$cc" \
          "CC_${cc_triple}=$cc" \
          "AR_${cc_triple}=$NDKBIN/llvm-ar" \
          cargo build --release -p rrk-ffi --target "$rust_triple"
        mkdir -p "$OUT/lib/android-$abi_dir"
        cp "target/$rust_triple/release/librrk_ffi.so" "$OUT/lib/android-$abi_dir/"
      }
      build_android aarch64-linux-android   aarch64-linux-android21-clang    arm64-v8a
      build_android armv7-linux-androideabi armv7a-linux-androideabi21-clang armeabi-v7a
      build_android i686-linux-android      i686-linux-android21-clang       x86
      build_android x86_64-linux-android    x86_64-linux-android21-clang     x86_64
    fi
  else
    warn "未找到 Android NDK（设 ANDROID_NDK_HOME 后重试；跳过 Android）"
  fi
else
  warn "SKIP_ANDROID=1，跳过 Android"
fi

# ---------------------------------------------------------------- Dart 包
say "拷贝 Dart 包 …"
mkdir -p "$OUT/dart"
cp -r bindings/dart/rrk_reader "$OUT/dart/rrk_reader"
rm -rf "$OUT/dart/rrk_reader/.dart_tool"
rm -f  "$OUT/dart/rrk_reader/pubspec.lock"

# ---------------------------------------------------------------- 集成说明
say "写集成说明 README …"
cat > "$OUT/README.md" <<'MD'
# rrk_reader 预编译 Release 包 v@VERSION@

Rust 阅读内核（`rust_reader_kernel`）的**可移植预编译产物 + Dart 封装包**。
ABI 版本：**@ABI@**（Dart 侧 `kRrkAbiVersion` 与之校验，不一致会在 `RrkEngine.init` 抛错）。

构建时间（UTC）：@BUILT_AT@

## 包内容

```
rrk_reader-@VERSION@/
├── lib/
│   ├── windows-x86_64/rrk_ffi.dll
│   ├── android-arm64-v8a/librrk_ffi.so
│   ├── android-armeabi-v7a/librrk_ffi.so
│   ├── android-x86/librrk_ffi.so
│   └── android-x86_64/librrk_ffi.so
├── dart/rrk_reader/        # Dart/Flutter 封装（path 依赖直接可用）
├── MANIFEST.json           # 各库 sha256 / 大小
└── README.md
```

## 接入 A：Flutter（Android）

1. 把对应 ABI 的 `.so` 放到 `android/app/src/main/jniLibs/<abi>/librrk_ffi.so`
   （A/B 目录名：`arm64-v8a` / `armeabi-v7a` / `x86` / `x86_64`，需放的 ABI 与
   `defaultConfig.ndk.abiFilters` 对齐）。
2. `pubspec.yaml`：
   ```yaml
   dependencies:
     rrk_reader:
       path: <本包解压目录>/dart/rrk_reader
   ```
3. 启动：
   ```dart
   import 'package:rrk_reader/rrk_reader.dart';
   RrkEngine.init();                 // 默认解析为 librrk_ffi.so，由系统链接器定位
   ```
   `.so` 会被打进 APK 的 `lib/<abi>/`，无需额外配置路径。

## 接入 B：Flutter Desktop / 纯 Dart（Windows）

1. 把 `lib/windows-x86_64/rrk_ffi.dll` 放到可执行文件同目录（或任意已知路径）。
2. `pubspec.yaml` 同上加 path 依赖。
3. 启动：
   ```dart
   RrkEngine.init(r'D:\path\to\rrk_ffi.dll');  // 显式路径
   // 或：设置环境变量 RRK_LIB 后 RrkEngine.init();
   // 或：dll 与 exe 同目录时 RrkEngine.init();  // 默认 rrk_ffi.dll
   ```

`init()` 解析顺序：显式参数 > 环境变量 `RRK_LIB` > 平台默认名
（Android `librrk_ffi.so` / Windows `rrk_ffi.dll` / macOS `librrk_ffi.dylib` / iOS 静态链接 `DynamicLibrary.process()`）。

## 接入 C：其它语言（C ABI）

`rrk_ffi` 导出标准 C ABI（`u64` 句柄 + `(ptr,len)` 缓冲），无 C++ ABI 依赖。
符号清单：

```
rrk_abi_version  rrk_open  rrk_close  rrk_last_error   rrk_buf_free
rrk_meta_json    rrk_toc_json      rrk_chapters_json  rrk_cover
rrk_load_chapter rrk_pages_json    rrk_page_at        rrk_hit_test
rrk_prefetch     rrk_cache_configure                  rrk_render_page
```

约定：函数返回 `0` = 成功，非 `0` = 错误码（`rrk_model::RrkErrorKind`，`2001` = 内部 panic）；
内核分配的缓冲由调用方 `rrk_buf_free(ptr,len)` 释放；字符串均为 UTF-8。

## 平台覆盖

- 本包含：**Windows x86_64** + **Android 四 ABI**（arm64-v8a / armeabi-v7a / x86 / x86_64）。
- Linux / macOS 由 CI（`.github/workflows/ci.yml` 的 `build-ffi` job）在对应 runner 上产出。
- iOS 需在 macOS 上用 `cargo build --release --target aarch64-apple-ios` 另编静态库/框架，
  本包不含 iOS 产物。
MD

sed -i \
  -e "s|@VERSION@|${VERSION}|g" \
  -e "s|@ABI@|${ABI}|g" \
  -e "s|@BUILT_AT@|${BUILT_AT}|g" \
  "$OUT/README.md"

# ---------------------------------------------------------------- MANIFEST
say "生成 MANIFEST.json …"
{
  printf '{\n'
  printf '  "name": "%s",\n' "$PKG"
  printf '  "version": "%s",\n' "$VERSION"
  printf '  "abiVersion": %s,\n' "$ABI"
  printf '  "builtAt": "%s",\n' "$BUILT_AT"
  printf '  "targets": {\n'
  first=1
  while IFS= read -r -d '' f; do
    rel="${f#"$OUT"/}"
    key="$(basename "$(dirname "$rel")")"
    size="$(wc -c < "$f" | tr -d '[:space:]')"
    sha="$(hash256 "$f")"
    [ "$first" -eq 1 ] || printf ',\n'
    printf '    "%s": { "lib": "%s", "bytes": %s, "sha256": "%s" }' \
      "$key" "$rel" "$size" "$sha"
    first=0
  done < <(find "$OUT/lib" -type f -print0 | sort -z)
  printf '\n  },\n'
  printf '  "dartPackage": "dart/rrk_reader"\n'
  printf '}\n'
} > "$OUT/MANIFEST.json"

# ---------------------------------------------------------------- zip
say "压缩 zip …"
ZIP="$ROOT/dist/${PKG}-${VERSION}.zip"
rm -f "$ZIP"
(
  cd "$ROOT/dist"
  if command -v zip >/dev/null 2>&1; then
    zip -qr "$ZIP" "${PKG}-${VERSION}"
  else
    tar -a -cf "$ZIP" "${PKG}-${VERSION}"    # Windows bsdtar 支持按扩展名生成 zip
  fi
)

say "完成"
say "  目录：$OUT"
say "  压缩包：$ZIP（$(wc -c < "$ZIP" | tr -d '[:space:]') bytes）"
