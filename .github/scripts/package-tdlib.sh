#!/usr/bin/env bash
# 把 tdjson 打成**fuyubot 的 loader 能直接吃**的发布资产。
#
# ⚠️ 名字和格式是**契约**，不要随手改：
#   - 资产文件名必须**精确等于** `crates/fuyubot-tdlib/src/loader/vendored.rs` 里的
#     `ASSET_*` 常量——`asset_name()` 的返回值是**原样**拼进下载 URL 的，
#     所以文件名**不带扩展名**（例如 `TDLib-windows-x86_64`）。
#   - 内容必须是 **.tar.gz**：loader 的 `extract_library` 只认「裸库」和「.tar.gz」，
#     收到 .zip 会**当裸库原样写出** → 得到一个坏文件，而且**不报错**。
#   这两条由 `crates/fuyubot-tdlib/tests/release_assets.rs` 盯着（它读本 workflow 来比对）。
#
# Args: <tdjson-dir> <archive-path> <platform> <arch> <tdlib-version> <tdlib-commit> <asset-name>
set -euo pipefail

TDLIB_JSON_DIR="${1:?tdjson dir}"
ARCHIVE_PATH="${2:?archive path}"
PLATFORM="${3:?platform}"
ARCH="${4:?arch}"
TDLIB_VERSION="${5:?tdlib version}"
TDLIB_COMMIT="${6:?tdlib commit}"
ASSET_NAME="${7:?asset name（必须等于 vendored.rs 里 ASSET_* 常量的值）}"

if [[ ! -d "$TDLIB_JSON_DIR" ]]; then
  echo "error: missing tdjson dir $TDLIB_JSON_DIR" >&2
  exit 1
fi

# 归档文件名必须**就是**资产名——loader 按这个名字找它。
if [[ "$(basename "$ARCHIVE_PATH")" != "$ASSET_NAME" ]]; then
  echo "error: archive basename '$(basename "$ARCHIVE_PATH")' != asset name '$ASSET_NAME'" >&2
  echo "       loader 的 asset_name() 会把这个名字原样拼进下载 URL，两者必须一致" >&2
  exit 1
fi

LIB_NAME=""
for name in libtdjson.so libtdjson.dylib tdjson.dll; do
  if [[ -f "$TDLIB_JSON_DIR/$name" ]]; then
    LIB_NAME="$name"
    break
  fi
done
if [[ -z "$LIB_NAME" ]]; then
  echo "error: tdjson library not found in $TDLIB_JSON_DIR" >&2
  ls -la "$TDLIB_JSON_DIR" >&2 || true
  exit 1
fi

BUILD_DATE="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

# ⚠️ 必须转成 **POSIX** 形态（`/d/a/...`）——**不能**用 `cygpath -m` 的 `D:/a/...`：
#    GNU tar 把 `host:path` 当**远程归档**语法，于是 `D:/...` 会被理解成
#    「连一台叫 D 的主机」，直接失败：
#        tar (child): Cannot connect to D: resolve failed
#    Windows runner 上踩过（打 Windows 资产那一步整个挂掉）。
#    在 Linux/macOS 上没有 cygpath，也就没有冒号，这段自然跳过。
if command -v cygpath >/dev/null 2>&1; then
  ARCHIVE_PATH="$(cygpath -u "$ARCHIVE_PATH")"
fi
mkdir -p "$(dirname "$ARCHIVE_PATH")"

# 内层目录名 = 资产名。loader 用 `file_name()` 找库，所以层级本身无所谓，
# 但内外一致更好读（也方便人肉核对）。
STAGE_ROOT="$(mktemp -d)"
STAGE_DIR="$STAGE_ROOT/$ASSET_NAME"
mkdir -p "$STAGE_DIR"

# 只带 tdjson 本身（+ Windows 的导入库）。OpenSSL/zlib 是静态并进去的，没有独立运行时依赖。
cp -a "$TDLIB_JSON_DIR/$LIB_NAME" "$STAGE_DIR/"
if [[ -f "$TDLIB_JSON_DIR/tdjson.lib" ]]; then
  cp -a "$TDLIB_JSON_DIR/tdjson.lib" "$STAGE_DIR/"
fi
if [[ -f "$TDLIB_JSON_DIR/build-info.txt" ]]; then
  cp -a "$TDLIB_JSON_DIR/build-info.txt" "$STAGE_DIR/"
fi

LINKAGE="static"
if [[ -f "$STAGE_DIR/build-info.txt" ]]; then
  if grep -q '^linkage=' "$STAGE_DIR/build-info.txt" 2>/dev/null; then
    LINKAGE="$(grep -m1 '^linkage=' "$STAGE_DIR/build-info.txt" | cut -d= -f2- | tr -d '\r')"
  fi
fi

LINK_LIB_JSON="null"
if [[ -f "$STAGE_DIR/tdjson.lib" ]]; then
  LINK_LIB_JSON="\"tdjson.lib\""
fi

cat > "$STAGE_DIR/VERSION.json" <<EOF
{
  "tdlib_version": "${TDLIB_VERSION}",
  "tdlib_commit": "${TDLIB_COMMIT}",
  "asset_name": "${ASSET_NAME}",
  "platform": "${PLATFORM}",
  "arch": "${ARCH}",
  "library": "${LIB_NAME}",
  "linkage": "${LINKAGE}",
  "linkage_note": "OpenSSL/zlib are statically linked into ${LIB_NAME}",
  "import_lib": ${LINK_LIB_JSON},
  "runtime_deps": [],
  "build_date_utc": "${BUILD_DATE}",
  "consumed_by": "fuyubot (dynamically loaded by fuyubot-tdlib::loader)"
}
EOF

cat > "$STAGE_DIR/README.md" <<EOF
# TDLib binary package（给 fuyubot 用）

- **资产名**：\`${ASSET_NAME}\` —— **必须**长这样，\`fuyubot-tdlib\` 的 loader 会把它原样拼进下载 URL
- **平台 / 架构**：${PLATFORM} / ${ARCH}
- **TDLib 版本 / commit**：${TDLIB_VERSION} / ${TDLIB_COMMIT}
- **动态库**：\`${LIB_NAME}\`
- **链接方式**：${LINKAGE}（OpenSSL/zlib 静态并入，不需要额外的 libssl / libcrypto / libz）
- **构建时间 (UTC)**：${BUILD_DATE}

## fuyubot 怎么用它

\`fuyubot-tdlib\` 在**构建期**按
\`https://github.com/<repo>/releases/download/TDLib-v<版本>/<资产名>\` 取这个资产，
解包后把 \`${LIB_NAME}\` 落到缓存目录（见 \`src/loader/vendored.rs\`）。
所以它**必须是 .tar.gz 的内容、且文件名就是这个资产名**——
换成 .zip 会被当成裸库原样写出，得到一个坏文件而且**不会报错**。

## 归档布局

\`\`\`
${ASSET_NAME}/
  ${LIB_NAME}
  VERSION.json
  README.md
\`\`\`

## 备注

- Linux：\`libtdjson.so\` 内含 OpenSSL/zlib；系统 libc/libstdc++ 仍然适用。
- macOS：\`libtdjson.dylib\` 链的是静态 OpenSSL/zlib；带 \`@rpath\` id 与 ad-hoc codesign。
- Windows：\`tdjson.dll\` 用 vcpkg **static** triplet 构建；可选的 \`tdjson.lib\` 只是导入库，
  **不是**运行时依赖。
- Android：\`libtdjson.so\` 用 NDK 交叉编译（\`c++_static\` + 静态 OpenSSL），
  zlib 用系统自带的 \`libz.so\`；**不是** loader 的发布资产（loader 只认 windows/macos/linux），
  资产名因此是 \`tdjson-android-<abi>\` 而不是 \`TDLib-*\`。
EOF

# 一律 tar.gz。**不要**按扩展名分派：资产名本身不带扩展名。
tar -czf "$ARCHIVE_PATH" -C "$STAGE_ROOT" "$ASSET_NAME"

rm -rf "$STAGE_ROOT"
echo "==> packaged $ARCHIVE_PATH (asset=$ASSET_NAME, tar.gz, static-linked tdjson only)"
ls -lh "$ARCHIVE_PATH"
