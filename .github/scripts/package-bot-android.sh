#!/usr/bin/env bash
# 把 fuyubot 交叉编译产物打成 **Android 部署包**（可执行文件，不是 apk）：
#
#   fuyubot-android-<arch>/
#     fuyubot             ← aarch64-linux-android / x86_64-linux-android 的 ELF 可执行文件
#     libtdjson.so        ← 同级：loader 的优先级 2b「可执行文件目录」靠它命中
#     plugins/*.so        ← 内置插件（/start、/help、/plugins），`init` 会自己装进项目
#     README.txt
#
# 为什么 tdjson 必须同级：Android 上 `dlopen("libtdjson.so")` 不会去搜 $PWD，
# 而 loader 的优先级 2b 是用 `/proc/self/exe` 的父目录拼出来的——二进制搬到哪都成立。
#
# Args: <target-triple> <arch> <version> <commit> <dist-dir> [tdjson-source-dir] [package-name]
#   target-triple        aarch64-linux-android / x86_64-linux-android
#   arch                 cargo 口径：aarch64 / x86_64
#   tdjson-source-dir    含 libtdjson.so 的目录（默认 .tdlib-cache/<version>/android-<arch>）
#   package-name         包目录名（默认 `fuyubot-android-<arch>`；release-bot 传带版本号的名字，
#                        这样 publish 打出来的 zip 与桌面口径一致：fuyubot-<版本>-android-<arch>.zip）
set -euo pipefail

TRIPLE="${1:?target triple}"
ARCH="${2:?arch: aarch64 | x86_64}"
VERSION="${3:?tdlib version}"
COMMIT="${4:?tdlib commit}"
DIST="${5:?dist dir}"
TDLIB_DIR="${6:-.tdlib-cache/${VERSION}/android-${ARCH}}"
PKG="${7:-fuyubot-android-${ARCH}}"

BIN="target/${TRIPLE}/release/fuyubot"
STAGE="${DIST}/${PKG}"
PLUGIN_DIR="target/${TRIPLE}/release"

[[ -f "$BIN" ]] || { echo "error: 找不到 $BIN（先 cargo build --release --target $TRIPLE）" >&2; exit 1; }

# 参数错位是最容易犯的错：把包名（第 7 个）写到了第 6 个位置，报错会变成
# 「fuyubot-…-android-aarch64 里没有 libtdjson.so」，看得人一头雾水。先点明。
if [[ ! -d "$TDLIB_DIR" ]]; then
  echo "error: 第 6 个参数应是**含 libtdjson.so 的目录**，实际传的是 '$TDLIB_DIR'（不是目录）" >&2
  echo "       用法：$0 <triple> <arch> <tdlib版本> <tdlib commit> <dist> [tdjson目录] [包名]" >&2
  exit 1
fi
[[ -f "$TDLIB_DIR/libtdjson.so" ]] || { echo "error: $TDLIB_DIR 里没有 libtdjson.so" >&2; ls -la "$TDLIB_DIR" >&2 || true; exit 1; }

rm -rf "$STAGE"
mkdir -p "$STAGE/plugins"

cp "$BIN" "$STAGE/fuyubot"
cp "$TDLIB_DIR/libtdjson.so" "$STAGE/libtdjson.so"

# 内置插件：cdylib 文件名在 Linux 上有 `lib` 前缀，包里必须是**无前缀**的
# `fuyubot_plugin_<名字>.so`（`scaffold::install_builtins` 按 `{stem}.so` 找）。
for stem in fuyubot_plugin_start fuyubot_plugin_help fuyubot_plugin_manage; do
  src="$PLUGIN_DIR/${stem}.so"
  [[ -f "$src" ]] || src="$PLUGIN_DIR/lib${stem}.so"
  [[ -f "$src" ]] || {
    echo "error: 缺内置插件产物 ${stem}.so（带 lib 前缀的也试过了，目录 $PLUGIN_DIR）" >&2
    exit 1
  }
  cp "$src" "$STAGE/plugins/${stem}.so"
done

# 用 printf 而不是 heredoc：YAML 块标量会把内容整体缩进，缩进的 `EOF` 不会终止 heredoc。
printf '%s\n' \
  "fuyubot for Android（${TRIPLE}）—— 可执行文件，不是 apk" \
  "" \
  "外置 TDLib：${VERSION}（commit ${COMMIT}，库与可执行文件同级，别删 libtdjson.so）" \
  "内置插件在 plugins/（/start、/help、/plugins），init/run 会自动装进项目" \
  "" \
  "放哪儿跑：**要可执行权限的目录**——Termux 家目录（\$HOME）或 adb 推的 /data/local/tmp。" \
  "  /sdcard 挂着 noexec，放那儿跑不起来。" \
  "" \
  "起步：" \
  "  adb push ${PKG}.tar.gz /data/local/tmp/ && adb shell" \
  "  cd /data/local/tmp && tar -xzf ${PKG}.tar.gz && cd ${PKG}" \
  "  chmod +x fuyubot" \
  "  ./fuyubot init --dir demo --name mybot --api-id <你的 api_id>" \
  "  ./fuyubot run  --config demo/fuyubot.toml" \
  "" \
  "Termux 里同样：tar -xzf … → chmod +x fuyubot → ./fuyubot init/run。" \
  "需要 API ${ANDROID_API_LEVEL:-21}+，与设备架构一致（当前 ${ARCH}）；" \
  "插件是 cdylib，必须与本体同架构。" \
  "文档：https://github.com/${GITHUB_REPOSITORY:-CatMoeCircle/Fuyu-Bot}" \
  > "$STAGE/README.txt"

chmod +x "$STAGE/fuyubot"
tar -czf "${DIST}/${PKG}.tar.gz" -C "$DIST" "$PKG"

echo "==> 校验 ${PKG}（ELF 类型 / 机器码 / 依赖）"
python3 - "$STAGE" "$ARCH" <<'PY'
import struct
import sys
from pathlib import Path

stage = Path(sys.argv[1])
arch = sys.argv[2]

bin_path = stage / "fuyubot"
data = bin_path.read_bytes()
if data[:4] != b"\x7fELF":
    raise SystemExit(f"{bin_path} 不是 ELF（Android 目标没生效？）")
if data[4] != 2 or data[5] != 1:
    raise SystemExit(f"期望 ELF64 小端，实际 class={data[4]} data={data[5]}")

e_type = struct.unpack_from("<H", data, 16)[0]
e_machine = struct.unpack_from("<H", data, 18)[0]
expected = {"aarch64": 183, "x86_64": 62}.get(arch)
if expected is None:
    raise SystemExit(f"未知 arch: {arch}")
if e_machine != expected:
    raise SystemExit(f"e_machine 不符：{e_machine}（期望 {expected}，arch={arch}）")
# 2 = EXEC，3 = DYN（PIE 可执行文件，Android 上默认就是这种）
if e_type not in (2, 3):
    raise SystemExit(f"e_type={e_type} 不是可执行文件（2=EXEC / 3=PIE-DYN）")

# PT_INTERP 必须存在：动态链接 bionic 的普通可执行文件靠它启动。
phoff = struct.unpack_from("<Q", data, 32)[0]
phentsize = struct.unpack_from("<H", data, 54)[0]
phnum = struct.unpack_from("<H", data, 56)[0]
interp = None
for i in range(phnum):
    off = phoff + i * phentsize
    p_type = struct.unpack_from("<I", data, off)[0]
    if p_type == 3:  # PT_INTERP
        p_offset = struct.unpack_from("<Q", data, off + 8)[0]
        p_filesz = struct.unpack_from("<Q", data, off + 32)[0]
        interp = data[p_offset:p_offset + p_filesz].rstrip(b"\0").decode("utf-8", "replace")
        break
if interp is None:
    raise SystemExit("没有 PT_INTERP：静态链接的二进制在 Android 上跑不起来")
if "linker" not in interp:
    raise SystemExit(f"PT_INTERP 看着不像 Android 的动态链接器：{interp}")

for required in ("libtdjson.so", "plugins/fuyubot_plugin_start.so",
                 "plugins/fuyubot_plugin_help.so", "plugins/fuyubot_plugin_manage.so"):
    if not (stage / required).is_file():
        raise SystemExit(f"包里缺 {required}")

print(f"OK ELF64 {arch} e_type={e_type} e_machine={e_machine} interp={interp}")
PY

# DT_NEEDED 只允许 bionic 那几样；出现 libssl/libcrypto/libc++_shared 说明静态合并失效，
# 它们在 Android 上**不存在**，装到设备上会直接起不来。
READELF="${ANDROID_NDK_HOME:-/nonexistent}/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-readelf"
if [[ -x "$READELF" ]]; then
  NEEDED="$("$READELF" -d "$STAGE/fuyubot" | grep -E 'NEEDED' || true)"
  echo "$NEEDED"
  if echo "$NEEDED" | grep -E 'libssl|libcrypto|libc\+\+_shared' >/dev/null; then
    echo "error: fuyubot 动态依赖了 Android 上没有的库（见上面 NEEDED）" >&2
    exit 1
  fi
else
  echo "warning: 没找到 llvm-readelf（ANDROID_NDK_HOME 没设？），跳过 DT_NEEDED 检查" >&2
fi

file "$STAGE/fuyubot" || true
ls -lhR "$STAGE"
echo "==> ${PKG}.tar.gz ready"
