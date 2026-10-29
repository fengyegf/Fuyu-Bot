//! vendored 原生库的资产映射与校验。
//!
//! 这个文件**同时**被 `build.rs`（`#[path]` 引入）和库本体编译，
//! 所以它只能依赖 std 和可选依赖，不得引用 crate 内其他模块。
//!
//! 缓存根、下载、`release_url` 这些**两个构建脚本共用**的设施不在这里——
//! 它们住在 `../fuyubot-tdlib-sys/src/build_support.rs`（含 `td_api.tl` 那半）。

use std::path::{Path, PathBuf};

/// Releases 资产名。
///
/// **命名约定**：`TDLib-<os>-<arch>`，其中 `arch` 一律用 **cargo `CARGO_CFG_TARGET_ARCH` 的口径**
/// （`x86_64` / `aarch64`）——别再混用 `arm64`。
/// 踩过一次：macOS 写成 `TDLib-macos-arm64`、Linux 写成 `TDLib-aarch64-linux`
/// （一个用 `arm64` 一个用 `aarch64`，顺序还一个是 os-arch 一个是 arch-os），读起来分不清。
///
/// 资产名会被 [`asset_name`] **原样**拼进下载 URL，所以改这里必须同步改 CI（AGENTS 约束 46）。
pub const ASSET_LINUX_X86_64: &str = "TDLib-linux-x86_64";
pub const ASSET_LINUX_AARCH64: &str = "TDLib-linux-aarch64";
pub const ASSET_MACOS_AARCH64: &str = "TDLib-macos-aarch64";
pub const ASSET_WINDOWS_X86_64: &str = "TDLib-windows-x86_64";
pub const ASSET_WINDOWS_AARCH64: &str = "TDLib-windows-aarch64";

/// `(os, arch, 资产名)`——**加/减平台只动这一张表**：
/// [`asset_name`] 与报错里的「支持哪些」都从它来，不会各自漂。
///
/// `crates/fuyubot-tdlib/tests/release_assets.rs` 会把表里的名字与
/// `.github/workflows/build-tdlib.yml` 的 matrix 对起来——**表里有、CI 不建的组合会让它红**，
/// 于是「CI 会不会产出来」与「loader 能不能下到」永远是同一件事。
///
/// 目前**故意不覆盖** `macos/x86_64`（Intel Mac）：CI 没有那个 runner，
/// 加了表反而会在构建期变成一个含糊的 404。真要支持得先给 CI 加平台。
///
/// 同理**故意不覆盖** `android/*`：Android 的 tdjson 由 `.github/workflows/build-tdlib.yml`
/// 的 `build-android` job 以 `tdjson-android-<abi>` 的形式产出（workflow artifact），
/// 再由 `build-framework-android` job 塞进 `.tdlib-cache/<版本>/android-<arch>/` 供
/// `build.rs` 拷到可执行文件同级——**不走 Releases 的自动下载**（那张表一旦加进来，
/// 就等于承诺 `TDLib-android-*` 一定躺在 Release 上，而现在 Android 是临时产物）。
/// 所以 Android **有** [`library_filename`]（加载要用），但 `asset_name` 是 `None`：
/// 在 Android 上开 `--features vendored` 会在构建期明确报「没有对应的 Releases 资产」。
pub const ASSETS: &[(&str, &str, &str)] = &[
    ("linux", "x86_64", ASSET_LINUX_X86_64),
    ("linux", "aarch64", ASSET_LINUX_AARCH64),
    ("macos", "aarch64", ASSET_MACOS_AARCH64),
    ("windows", "x86_64", ASSET_WINDOWS_X86_64),
    ("windows", "aarch64", ASSET_WINDOWS_AARCH64),
];

/// 平台动态库文件名；不支持的平台返回 `None`。
///
/// Android 也是 ELF，库名与 Linux 一致；`std::env::consts::OS` 在 Android 上是 `"android"`
/// （**不是** `"linux"`），所以必须单列一条——漏了它 loader 会在优先级 4 报
/// 「未知平台 android，没有约定的库文件名」。
pub fn library_filename(os: &str) -> Option<&'static str> {
    match os {
        "windows" => Some("tdjson.dll"),
        "macos" => Some("libtdjson.dylib"),
        "linux" | "android" => Some("libtdjson.so"),
        _ => None,
    }
}

/// `(os, arch)` → Releases 资产名；没有这个组合就是 `None`
/// （构建期会报错并列出**支持的组合**，不用去翻代码）。
pub fn asset_name(os: &str, arch: &str) -> Option<&'static str> {
    ASSETS
        .iter()
        .find(|(o, a, _)| *o == os && *a == arch)
        .map(|(_, _, name)| *name)
}

/// 报错时用：把支持的组合列出来（形如 `linux-x86_64 / linux-aarch64 / …`）。
pub fn supported_targets() -> String {
    ASSETS
        .iter()
        .map(|(os, arch, _)| format!("{os}-{arch}"))
        .collect::<Vec<_>>()
        .join(" / ")
}

/// 文件名是否像某个平台的 tdjson。
pub fn is_library_file(name: &str, os: &str) -> bool {
    match os {
        "windows" => name.starts_with("tdjson") && name.ends_with(".dll"),
        "macos" => name.starts_with("libtdjson") && name.contains(".dylib"),
        _ => name.starts_with("libtdjson") && name.contains(".so"),
    }
}

/// 某个版本 + 平台在缓存里的目录（**相对缓存根**）：`<version>/<os>-<arch>`。
pub fn relative_library_dir(version: &str, os: &str, arch: &str) -> PathBuf {
    Path::new(version).join(format!("{os}-{arch}"))
}

/// 缓存里该平台库文件的**相对**路径（相对缓存根）。
pub fn relative_library(version: &str, os: &str, arch: &str) -> PathBuf {
    relative_library_dir(version, os, arch).join(library_filename(os).unwrap_or("tdjson"))
}

/// 构建产物目录（放可执行文件的地方）从 `OUT_DIR` 推出来：
/// `<target>/<profile>/build/<crate>-<hash>/out` 往上三级。
///
/// cargo 没有「把文件放到产物旁边」的机制，只能这么算。形状对不上就返回 `None`
/// ——**宁可不拷，也不往一个猜出来的目录里写**。（纯路径运算 + 一次 `is_dir`，所以能单测。）
pub fn binary_dir_from(out_dir: &Path) -> Option<PathBuf> {
    let mut ancestors = out_dir.ancestors();
    ancestors.next()?; // out
    ancestors.next()?; // <crate>-<hash>
    if ancestors.next()?.file_name()? != "build" {
        return None;
    }
    let profile = ancestors.next()?;
    profile.is_dir().then(|| profile.to_path_buf())
}

/// `source` 里要拷到二进制旁边的动态库文件。
///
/// - 文件来源：文件名等于 `lib_name` 才要（`TDLIB_TDJSON_PATH` 直接指文件时就是这种）；
/// - 目录来源：**必须**含有 `lib_name`，连同**兄弟动态库**一起返回（`tdjson.dll` 可能 import
///   `libssl-3-x64.dll` 之类，pitfalls 坑 44）；没有 `lib_name` 的目录整个跳过，免得把别处的 DLL 搬过来。
///
/// 只看扩展名、不猜依赖名字：真机上的依赖集随 TDLib 的构建方式变（静态并进去时一个都没有）。
pub fn stageable_library_files(source: &Path, lib_name: &str) -> Vec<PathBuf> {
    if source.is_file() {
        return source
            .file_name()
            .is_some_and(|name| name == lib_name)
            .then(|| vec![source.to_path_buf()])
            .unwrap_or_default();
    }
    let Ok(entries) = std::fs::read_dir(source) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    let mut has_library = false;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == lib_name {
            has_library = true;
            found.push(entry.path());
        } else if name.ends_with(".dll") || name.ends_with(".dylib") || name.contains(".so") {
            found.push(entry.path());
        }
    }
    if has_library { found } else { Vec::new() }
}

/// SHA256 十六进制小写。
#[cfg(feature = "vendored")]
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// 从下载到的字节里取出裸库文件。
///
/// 支持两种资产形态：裸动态库，和 `.tar.gz`（取其中第一个匹配平台的库文件）。
#[cfg(feature = "vendored")]
pub fn extract_library(bytes: &[u8], os: &str) -> Result<Vec<u8>, String> {
    use std::io::Read;

    if !bytes.starts_with(&[0x1f, 0x8b]) {
        return Ok(bytes.to_vec());
    }

    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    let entries = archive
        .entries()
        .map_err(|e| format!("读取 tar.gz 失败: {e}"))?;
    let mut fallback: Option<Vec<u8>> = None;
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("读取 tar.gz 条目失败: {e}"))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let name = entry
            .path()
            .map(|p| {
                p.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            })
            .unwrap_or_default();
        let mut buf = Vec::new();
        entry
            .read_to_end(&mut buf)
            .map_err(|e| format!("解压条目 {name} 失败: {e}"))?;
        if is_library_file(&name, os) {
            return Ok(buf);
        }
        fallback.get_or_insert(buf);
    }
    fallback.ok_or_else(|| "tar.gz 里没有任何普通文件".to_owned())
}

/// 解析 `libtdjson.so.<major>[.<minor>[.<patch>]]`，缺失的部分补 0。
pub fn parse_versioned_library_name(name: &str) -> Option<(u32, u32, u32)> {
    let rest = name.strip_prefix("libtdjson.so.")?;
    let mut parts = rest.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().map_or(Some(0), |p| p.parse().ok())?;
    let patch = parts.next().map_or(Some(0), |p| p.parse().ok())?;
    // 多于三段的不认（例如 libtdjson.so.1.8.66.debug）
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// `"1.8.66"` → `(1, 8, 66)`；解析失败返回 `None`。
pub fn parse_version(version: &str) -> Option<(u32, u32, u32)> {
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().map_or(Some(0), |p| p.parse().ok())?;
    let patch = parts.next().map_or(Some(0), |p| p.parse().ok())?;
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用的临时目录（**不用 `%TEMP%`**，见 AGENTS 的测试规矩）。
    fn temp(name: &str) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp/fuyubot-tdlib")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建测试临时目录失败");
        dir
    }

    #[test]
    fn asset_and_library_names() {
        assert_eq!(asset_name("linux", "x86_64"), Some("TDLib-linux-x86_64"));
        assert_eq!(asset_name("linux", "aarch64"), Some("TDLib-linux-aarch64"));
        assert_eq!(asset_name("macos", "aarch64"), Some("TDLib-macos-aarch64"));
        assert_eq!(
            asset_name("windows", "x86_64"),
            Some("TDLib-windows-x86_64")
        );
        assert_eq!(
            asset_name("windows", "aarch64"),
            Some("TDLib-windows-aarch64")
        );

        // 命名约定：`TDLib-<os>-<arch>`，arch 一律 cargo 口径（`aarch64`，不是 `arm64`）。
        for &(os, arch, name) in ASSETS {
            assert_eq!(asset_name(os, arch), Some(name), "{os}/{arch} 查不到");
            assert!(name.starts_with("TDLib-"), "{name} 前缀不对");
            assert!(
                name.ends_with(arch),
                "资产名必须以 cargo 的 arch 结尾：{name} vs {arch}"
            );
            assert!(
                !name.contains("arm64"),
                "{name} 混用了 `arm64`——统一用 cargo 的 `aarch64`"
            );
        }
        assert_eq!(
            ASSETS.len(),
            5,
            "表里有几行就要和 CI 的 matrix 一样多（守卫测试会逐行比对）"
        );

        // **故意不支持**的组合必须是 None：构建期会报「目前支持：…」，
        // 而不是让人以为能下、结果 404。
        assert_eq!(
            asset_name("macos", "x86_64"),
            None,
            "Intel Mac：CI 没有对应 runner，别往表里加"
        );
        assert_eq!(asset_name("windows", "x86"), None);
        assert_eq!(asset_name("freebsd", "x86_64"), None);

        // 报错里能列出支持的组合，且不多列。
        let list = supported_targets();
        for &(os, arch, _) in ASSETS {
            assert!(
                list.contains(&format!("{os}-{arch}")),
                "{list} 少了 {os}-{arch}"
            );
        }
        assert!(
            !list.contains("macos-x86_64"),
            "{list} 不该包含没支持的组合"
        );

        assert_eq!(library_filename("windows"), Some("tdjson.dll"));
        assert_eq!(library_filename("macos"), Some("libtdjson.dylib"));
        assert_eq!(library_filename("linux"), Some("libtdjson.so"));
        // Android：库名与 Linux 相同（ELF），但 `std::env::consts::OS` 是 `"android"`，
        // 不单列一条就会在优先级 4 变成「未知平台」。
        assert_eq!(library_filename("android"), Some("libtdjson.so"));
        // 而**故意没有** Releases 资产：Android 的 tdjson 由 CI 的 workflow artifact 喂给
        // 框架构建 job（见 ASSETS 的注释），所以 `--features vendored` 在 Android 上会明确报错。
        assert_eq!(asset_name("android", "aarch64"), None);
        assert_eq!(library_filename("freebsd"), None);
    }

    #[test]
    fn cache_layout_is_version_scoped() {
        // 相对路径按 `<version>/<os>-<arch>` 分层：查缓存与落盘用的都是这一份
        // （`build_support::find_cached` / `store_download`），布局漂了缓存就失效。
        assert_eq!(
            relative_library_dir("1.8.66", "windows", "x86_64"),
            Path::new("1.8.66/windows-x86_64")
        );
        assert_eq!(
            relative_library("1.8.66", "linux", "x86_64"),
            Path::new("1.8.66/linux-x86_64/libtdjson.so")
        );
    }

    #[test]
    fn versioned_library_names() {
        assert_eq!(
            parse_versioned_library_name("libtdjson.so.1.8.66"),
            Some((1, 8, 66))
        );
        assert_eq!(
            parse_versioned_library_name("libtdjson.so.1.8"),
            Some((1, 8, 0))
        );
        assert_eq!(
            parse_versioned_library_name("libtdjson.so.1"),
            Some((1, 0, 0))
        );
        assert_eq!(parse_versioned_library_name("libtdjson.so"), None);
        assert_eq!(parse_versioned_library_name("libtdjson.so.1.8.66.1"), None);
        assert_eq!(parse_version("1.8.66"), Some((1, 8, 66)));
        assert_eq!(parse_version("2"), Some((2, 0, 0)));
    }

    #[cfg(feature = "vendored")]
    #[test]
    fn sha256_and_extract() {
        // 空输入的 SHA256 是众所周知的常量。
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );

        // 裸库：原样返回。
        let raw = b"\x7fELF fake tdjson";
        assert_eq!(extract_library(raw, "linux").unwrap(), raw);

        // tar.gz：挑出匹配平台的库文件，忽略其它文件。
        let mut tar_bytes = Vec::new();
        {
            let enc = flate2::write::GzEncoder::new(&mut tar_bytes, flate2::Compression::fast());
            let mut builder = tar::Builder::new(enc);
            let files: [(&str, &[u8]); 2] = [
                ("README.md", b"not a library"),
                ("libtdjson.so.1.8.66", b"real library bytes"),
            ];
            for (path, data) in files {
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append_data(&mut header, path, data).unwrap();
            }
            builder.into_inner().unwrap().finish().unwrap();
        }
        assert_eq!(
            extract_library(&tar_bytes, "linux").unwrap(),
            b"real library bytes"
        );
    }

    /// 「把库放到二进制旁边」这一半的路径推导：形状要认，形状不对要**拒绝**。
    ///
    /// 代价写反了就是往一个猜出来的目录里搬 28MB（甚至搬到仓库里去），所以这里的断言
    /// 两个方向都要有。
    #[test]
    fn the_binary_directory_is_derived_from_out_dir() {
        let root = temp("binary-dir");
        let profile = root.join("debug");
        let out_dir = profile
            .join("build")
            .join("fuyubot-tdlib-0123456789abcdef")
            .join("out");
        std::fs::create_dir_all(&out_dir).unwrap();
        assert_eq!(binary_dir_from(&out_dir), Some(profile.clone()));

        // `--target <triple>` 也是这样：`<目标目录>/<triple>/<profile>/build/.../out`
        let triple_profile = root.join("x86_64-pc-windows-msvc").join("release");
        let triple_out = triple_profile
            .join("build")
            .join("fuyubot-tdlib-0123456789abcdef")
            .join("out");
        std::fs::create_dir_all(&triple_out).unwrap();
        assert_eq!(binary_dir_from(&triple_out), Some(triple_profile.clone()));

        // 形状不对（没有 `build` 那一段）= 宁可什么都不拷。
        let weird = root.join("a").join("b").join("c").join("out");
        std::fs::create_dir_all(&weird).unwrap();
        assert_eq!(binary_dir_from(&weird), None);
        // profile 目录还不存在时也不能认（真到不了，但别默默往上一级写）。
        let missing = root.join("nope").join("build").join("c").join("out");
        assert_eq!(binary_dir_from(&missing), None);
    }

    /// 目录来源要把**兄弟动态库**一起带上（pitfalls 坑 44），没有 tdjson 的目录则整个跳过。
    #[test]
    fn stageable_files_follow_the_dynamic_library_shapes() {
        let dir = temp("stageable-dir");
        for name in [
            "tdjson.dll",
            "libssl-3-x64.dll",
            "libcrypto-3.dll",
            "tdjson.lib",
            "build-info.txt",
        ] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        let mut names: Vec<String> = stageable_library_files(&dir, "tdjson.dll")
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        // `tdjson.dll` + 兄弟 dll；导入库 `.lib` 与说明文件不是运行时依赖。
        assert_eq!(names, ["libcrypto-3.dll", "libssl-3-x64.dll", "tdjson.dll"]);

        // 目录里没有约定的库名 → 一个都不拷（免得把别处的 DLL 搬过来）。
        assert!(stageable_library_files(&dir, "libtdjson.so").is_empty());

        // 文件来源：名字对上才要。
        assert_eq!(
            stageable_library_files(&dir.join("tdjson.dll"), "tdjson.dll"),
            vec![dir.join("tdjson.dll")]
        );
        assert!(stageable_library_files(&dir.join("tdjson.lib"), "tdjson.dll").is_empty());
        assert!(stageable_library_files(&dir.join("不存在.dll"), "tdjson.dll").is_empty());
    }
}
