#!/usr/bin/env bash
# TDLib for **Android**（tdjson）—— 官方 CMake 流程 + NDK 交叉编译。
#
# 与桌面脚本（build-tdlib-linux.sh / -macos.sh）的两处根本差别：
#
# 1. **必须先做一次 host 构建**。TDLib 的生成源码（`td/generate/auto/**`）不在仓库里
#    （`.gitignore` 里有 `**/auto/`），而 `prepare_cross_compiling` 目标被
#    `if (NOT CMAKE_CROSSCOMPILING)` 包着——交叉编译时它根本不存在。所以这里先用
#    主机编译器把生成器跑一遍，把源码落到 `$TDLIB_SRC/td/generate/auto`，再做 Android 构建。
#    ⇒ host 侧需要 gperf + 一个 C++ 编译器（CI 的 apt 装了 g++）。
#
# 2. **Android 没有系统 OpenSSL**。这里自己用 NDK 编译一份**静态** OpenSSL
#    （`no-shared`），zlib 用 NDK sysroot 里的 `libz.so`（Android 系统自带，不算额外运行依赖）。
#    这与桌面的「静态并入 OpenSSL/zlib」目标一致：tdjson 只留一个文件。
#
# Args: <tdlib-src-dir> <output-dir> <abi: arm64-v8a | armeabi-v7a | x86 | x86_64>
#
# Env:
#   ANDROID_NDK_HOME / ANDROID_NDK_ROOT   NDK 根目录（必填，二选一）
#   ANDROID_API_LEVEL                     默认 21（TDLib 要求 ≥ 21）
#   OPENSSL_VERSION                       默认 3.5.4（见下面 OPENSSL_SHA256）
#   OPENSSL_CACHE_DIR                     源码/产物缓存目录（默认 $OUT_DIR/openssl-src）
#   OPENSSL_JOBS                          OpenSSL 的 make 并发度，默认 **1**（并行会抢
#                                         `.d.tmp`，见第 2) 步里的说明）
set -euo pipefail

TDLIB_SRC="${1:?tdlib source dir}"
OUT_DIR="${2:?output dir}"
ABI="${3:?abi: arm64-v8a | armeabi-v7a | x86 | x86_64}"

API_LEVEL="${ANDROID_API_LEVEL:-21}"
OPENSSL_VERSION="${OPENSSL_VERSION:-3.5.4}"

# 版本 → ABI 口径（注意 `TRIPLE` 用的是 NDK sysroot 的目录名，和 cargo 的 arch 不是一回事）。
case "$ABI" in
  arm64-v8a)   OPENSSL_TARGET=android-arm64;  TRIPLE=aarch64-linux-android;  CARCH=aarch64 ;;
  armeabi-v7a) OPENSSL_TARGET=android-arm;    TRIPLE=arm-linux-androideabi;  CARCH=arm     ;;
  x86)         OPENSSL_TARGET=android-x86;    TRIPLE=i686-linux-android;     CARCH=x86     ;;
  x86_64)      OPENSSL_TARGET=android-x86_64; TRIPLE=x86_64-linux-android;   CARCH=x86_64  ;;
  *)
    echo "error: unsupported Android ABI: $ABI (支持 arm64-v8a / armeabi-v7a / x86 / x86_64)" >&2
    exit 1
    ;;
esac

NDK="${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-}}"
if [[ -z "$NDK" || ! -f "$NDK/build/cmake/android.toolchain.cmake" ]]; then
  echo "error: 找不到 Android NDK（ANDROID_NDK_HOME / ANDROID_NDK_ROOT 未指向有效 NDK）" >&2
  echo "       需要 \$NDK/build/cmake/android.toolchain.cmake" >&2
  [[ -n "$NDK" ]] && ls -la "$NDK" >&2 || true
  exit 1
fi
NDK="$(cd "$NDK" && pwd)"

# OpenSSL 的 15-android.conf 认这几个变量名（不同版本认的不一样，索性都导出）。
export ANDROID_NDK_ROOT="$NDK"
export ANDROID_NDK_HOME="$NDK"
export ANDROID_NDK="$NDK"

case "$(uname -s)" in
  Linux)  HOST_TAG=linux-x86_64 ;;
  Darwin) HOST_TAG=darwin-x86_64 ;;
  *)
    echo "error: 只支持在 Linux / macOS runner 上交叉编译 Android（当前 $(uname -s)）" >&2
    exit 1
    ;;
esac
TOOLCHAIN="$NDK/toolchains/llvm/prebuilt/$HOST_TAG"
SYSROOT="$TOOLCHAIN/sysroot"
if [[ ! -d "$TOOLCHAIN" ]]; then
  echo "error: NDK 里没有 host 工具链 $TOOLCHAIN（Windows 版 NDK 不能用于本脚本）" >&2
  exit 1
fi

NDK_REVISION="$(sed -n 's/^Pkg\.Revision *= *//p' "$NDK/source.properties" 2>/dev/null | tr -d '\r' || true)"

# macOS 上没有 nproc；CI 只有 Linux，但脚本别在 Darwin 上莫名挂掉。
JOBS="$( (command -v nproc >/dev/null 2>&1 && nproc) || sysctl -n hw.ncpu 2>/dev/null || echo 2 )"

echo "==> Android tdjson: abi=$ABI api=$API_LEVEL ndk=${NDK_REVISION:-unknown} ($NDK)"
echo "==> Official build doc: https://tdlib.github.io/td/build.html"
echo "==> Extra: static OpenSSL (built by this script) + NDK sysroot zlib"

mkdir -p "$OUT_DIR/tdjson"

# ---------------------------------------------------------------------------
# 1) host 侧先把生成源码跑出来（td/generate/auto/**）
# ---------------------------------------------------------------------------
GENERATED_MARKER="$TDLIB_SRC/td/generate/auto/td/telegram/td_api.h"
if [[ -f "$GENERATED_MARKER" ]]; then
  echo "==> 生成源码已存在，跳过 host 生成：$GENERATED_MARKER"
else
  echo "==> host 构建：生成源码（prepare_cross_compiling）"
  command -v gperf >/dev/null 2>&1 || { echo "error: 缺 gperf（TDLib 生成器要用）" >&2; exit 1; }
  HOST_BUILD="${TDLIB_SRC}/build-android-host"
  rm -rf "$HOST_BUILD"
  mkdir -p "$HOST_BUILD"
  (
    cd "$HOST_BUILD"
    cmake -DCMAKE_BUILD_TYPE=Release -DTD_ENABLE_JNI=OFF "$TDLIB_SRC"
    cmake --build . --target prepare_cross_compiling -j "$JOBS"
  )
  [[ -f "$GENERATED_MARKER" ]] || {
    echo "error: host 生成阶段没产出 $GENERATED_MARKER" >&2
    find "$TDLIB_SRC/td/generate" -maxdepth 3 -name 'auto' -o -maxdepth 3 -name 'td_api*' >&2 || true
    exit 1
  }
fi

# ---------------------------------------------------------------------------
# 2) 静态 OpenSSL（每个 ABI 一份，缓存在 $OUT_DIR/openssl/<abi>）
# ---------------------------------------------------------------------------
OPENSSL_PREFIX="$OUT_DIR/openssl/$ABI"
if [[ -f "$OPENSSL_PREFIX/lib/libcrypto.a" && -f "$OPENSSL_PREFIX/lib/libssl.a" ]]; then
  echo "==> 复用已有 OpenSSL：$OPENSSL_PREFIX"
else
  echo "==> 编译静态 OpenSSL $OPENSSL_VERSION ($OPENSSL_TARGET)"
  CACHE="${OPENSSL_CACHE_DIR:-$OUT_DIR/openssl-src}"
  mkdir -p "$CACHE"
  TARBALL="$CACHE/openssl-$OPENSSL_VERSION.tar.gz"
  if [[ ! -s "$TARBALL" ]]; then
    curl -fsSL --retry 3 -o "$TARBALL.part" \
      "https://www.openssl.org/source/openssl-$OPENSSL_VERSION.tar.gz"
    mv "$TARBALL.part" "$TARBALL"
  fi
  # 官方随包发布的 sha256 侧车（拿不到就只警告：离线复用时缓存里已有源码）。
  SHA_URL="https://www.openssl.org/source/openssl-$OPENSSL_VERSION.tar.gz.sha256"
  if EXPECTED="$(curl -fsSL --max-time 30 "$SHA_URL" 2>/dev/null | awk '{print $1}' | tr -d '\r')" \
     && [[ -n "$EXPECTED" ]]; then
    ACTUAL="$(sha256sum "$TARBALL" | awk '{print $1}')"
    if [[ "$ACTUAL" != "$EXPECTED" ]]; then
      echo "error: OpenSSL 源码校验失败 $TARBALL" >&2
      echo "       expected=$EXPECTED actual=$ACTUAL" >&2
      exit 1
    fi
    echo "==> OpenSSL sha256 OK: $ACTUAL"
  else
    echo "warning: 拿不到 $SHA_URL，跳过 OpenSSL 源码校验" >&2
  fi

  SRC_DIR="$CACHE/openssl-$OPENSSL_VERSION"
  rm -rf "$SRC_DIR"
  tar -xzf "$TARBALL" -C "$CACHE"
  (
    cd "$SRC_DIR"
    PATH="$TOOLCHAIN/bin:$PATH" ./Configure "$OPENSSL_TARGET" \
      "-D__ANDROID_API__=$API_LEVEL" \
      no-shared no-tests no-apps no-docs \
      --prefix="$OPENSSL_PREFIX" --openssldir="$OPENSSL_PREFIX/ssl"

    # ⚠️ OpenSSL 的 make 有三个坑，都踩过：
    #
    # 1. **自更新 Makefile**：第一次 make 会重新生成 Makefile，然后要求
    #    「Please run the same make command again」。所以下面用重试循环，别当失败。
    # 2. **并行会抢 `.d.tmp`**：Makefile 里有些对象**同时挂在两个库里**
    #    （`make -n` 里同一个 `-o X.o` 出现两次），`make -j` 下两条配方会同时写/搬
    #    同一个 `X.d.tmp`，报 `mv: cannot stat 'X.d.tmp'`。16 路并行几乎必挂，
    #    这不是偶发。所以 OpenSSL 默认**串行**编（几分钟）；赶时间就传 OPENSSL_JOBS。
    # 3. `-j1` 之后仍有极小概率需要第二次 make（生成的头文件比对象新）。重试兜底。
    OPENSSL_JOBS="${OPENSSL_JOBS:-1}"
    echo "==> OpenSSL make -j$OPENSSL_JOBS（并行会抢 .d.tmp，默认串行）"
    attempt=0
    until PATH="$TOOLCHAIN/bin:$PATH" make -j "$OPENSSL_JOBS" install_sw; do
      attempt=$((attempt + 1))
      if [[ "$attempt" -ge 3 ]]; then
        echo "error: OpenSSL 构建失败（已重跑 $attempt 次）" >&2
        exit 1
      fi
      echo "==> OpenSSL make 需要重跑（第 $attempt 次）"
    done
  )
fi

[[ -f "$OPENSSL_PREFIX/lib/libcrypto.a" ]] || { echo "error: libcrypto.a 没编出来" >&2; exit 1; }

# ---------------------------------------------------------------------------
# 3) 交叉编译 TDLib（tdjson）
# ---------------------------------------------------------------------------
BUILD_DIR="${TDLIB_SRC}/build-android-${ABI}"
rm -rf "$BUILD_DIR"
mkdir -p "$BUILD_DIR"

CMAKE_ARGS=(
  -DCMAKE_BUILD_TYPE=Release
  -DCMAKE_TOOLCHAIN_FILE="$NDK/build/cmake/android.toolchain.cmake"
  -DANDROID_ABI="$ABI"
  -DANDROID_PLATFORM="android-$API_LEVEL"
  # c++_static：把 libc++ 并进 tdjson，宿主侧只需要一个 .so（和桌面的「单文件」目标一致）。
  -DANDROID_STL=c++_static
  # Android 15 起要求 16KB 页对齐（NDK r27+ 的开关）。
  -DANDROID_SUPPORT_FLEXIBLE_PAGE_SIZES=ON
  -DBUILD_SHARED_LIBS=ON
  -DTD_ENABLE_JNI=OFF
  -DOPENSSL_ROOT_DIR="$OPENSSL_PREFIX"
  -DOPENSSL_INCLUDE_DIR="$OPENSSL_PREFIX/include"
  -DOPENSSL_SSL_LIBRARY="$OPENSSL_PREFIX/lib/libssl.a"
  -DOPENSSL_CRYPTO_LIBRARY="$OPENSSL_PREFIX/lib/libcrypto.a"
  -DOPENSSL_USE_STATIC_LIBS=TRUE
)

# zlib 用 NDK sysroot 里按 API 分层的 stub（Android 系统自带 libz.so）。
ZLIB_LIB="$(find "$SYSROOT/usr/lib/$TRIPLE" -maxdepth 2 -name 'libz.so' 2>/dev/null | sort | head -n1 || true)"
if [[ -n "$ZLIB_LIB" ]]; then
  echo "==> zlib: $ZLIB_LIB"
  CMAKE_ARGS+=(-DZLIB_LIBRARY="$ZLIB_LIB" -DZLIB_INCLUDE_DIR="$SYSROOT/usr/include")
else
  echo "warning: sysroot 里没找到 $TRIPLE 的 libz.so，交给 find_package(ZLIB) 自己找" >&2
fi

echo "==> cmake (android.toolchain.cmake, abi=$ABI)"
cmake "${CMAKE_ARGS[@]}" "$TDLIB_SRC" -B "$BUILD_DIR"
echo "==> cmake --build . --target tdjson"
cmake --build "$BUILD_DIR" --target tdjson -j "$JOBS"

# ---------------------------------------------------------------------------
# 4) 收集产物
# ---------------------------------------------------------------------------
MAIN=""
while IFS= read -r -d '' f; do
  if [[ ! -L "$f" ]]; then
    MAIN="$f"
    break
  fi
done < <(find "$BUILD_DIR" -type f -name 'libtdjson.so*' -print0 2>/dev/null | sort -z)
if [[ -z "$MAIN" ]]; then
  echo "error: 没产出 libtdjson.so" >&2
  find "$BUILD_DIR" -type f -name '*tdjson*' >&2 || true
  exit 1
fi
cp -aL "$MAIN" "$OUT_DIR/tdjson/libtdjson.so"
"$TOOLCHAIN/bin/llvm-strip" --strip-unneeded "$OUT_DIR/tdjson/libtdjson.so"

LIB="$OUT_DIR/tdjson/libtdjson.so"
echo "==> llvm-readelf -h"
"$TOOLCHAIN/bin/llvm-readelf" -h "$LIB" | grep -E 'Class|Machine|Type' || true

# OpenSSL/zlib 必须是**静态**并进去的：出现 libssl/libcrypto 的 DT_NEEDED 就说明并失败了。
echo "==> DT_NEEDED（不允许出现 libssl / libcrypto / libc++_shared）"
NEEDED="$("$TOOLCHAIN/bin/llvm-readelf" -d "$LIB" | grep -E 'NEEDED' || true)"
echo "$NEEDED"
if echo "$NEEDED" | grep -E 'libssl|libcrypto|libc\+\+_shared' >/dev/null; then
  echo "error: tdjson 仍然动态依赖 openssl / libc++_shared（静态并入失败）" >&2
  exit 1
fi

{
  echo "platform=android"
  echo "arch=${CARCH}"
  echo "abi=${ABI}"
  echo "api_level=${API_LEVEL}"
  echo "ndk_revision=${NDK_REVISION:-unknown}"
  echo "openssl_version=${OPENSSL_VERSION}"
  echo "doc=https://tdlib.github.io/td/build.html"
  echo "official_flow=host-generate+cmake(ndk toolchain)+tdjson"
  echo "linkage=static"
  echo "openssl_lib=${OPENSSL_PREFIX}/lib/libcrypto.a"
  echo "zlib_note=libz.so comes from the Android system (NDK sysroot stub)"
  echo "note=OpenSSL merged into libtdjson.so (static), libc++ merged via c++_static"
} > "$OUT_DIR/tdjson/build-info.txt"

file "$LIB" || true
ls -lh "$LIB"
echo "==> Android $ABI tdjson ready"
