# AGENTS.md — fuyubot

## 这是什么

基于 TDLib 的 Telegram bot / user 框架：**一个宿主进程 + N 个动态库插件**。
插件是 cdylib（`.dll` / `.so` / `.dylib`），只认四个导出符号与一张 `HostApiVTable`（ABI 版本 5）。
TDLib 在**运行时**动态加载（`fuyubot-tdlib::loader`），`td_api.tl` 在**编译期**转成 Rust 类型与可校验元数据。

三个仓库（本仓只放运行时）：

| 仓库 | 放什么 |
| --- | --- |
| **Fuyu-Bot**（本仓） | 运行时全部 + `fuyubot-abi` + `fuyubot-tdlib-sys`（后两个发 crates.io） |
| FuyuBot-SDK | `fuyubot-plugin-api`、`fuyubot-sdk-script`、`fuyubot-sdk` CLI + 五种语言示例 |
| FuyuBot-Docs | 文档站（<https://catmoecircle.github.io/FuyuBot-docs/>，源在 `content/docs/**`） |

跨仓依赖只有一条、单向：SDK 通过 crates.io 依赖本仓的 `fuyubot-abi` / `fuyubot-tdlib-sys`。

## 结构

| crate | 一句话 |
| --- | --- |
| `fuyubot-cli` | 二进制 `fuyubot`：`init` / `run` / `status` / `stop` / `config` / `plugin`（clap） |
| `fuyubot-core` | Cordis 内核：`Context`（按**名字**寻址的服务表）/ `Effect` / `Plugin`（含 `inject`）/ `PluginManager`（Fiber 生命周期与依赖跟踪）。不认识 Telegram |
| `fuyubot-abi` | C ABI：`HostApiVTable`（2 个 u32 + `host_ctx` + 23 个函数指针）、`PluginHandle`、`define_plugin!`、cbindgen 头文件 |
| `fuyubot-tdlib-sys` | 绑定层：`td_api.tl` → 类型 + 元数据 + FFI 声明（无 tokio / 无网络） |
| `fuyubot-tdlib` | `loader`（四级定位）+ `client`（`td_receive` 循环、`ReceiveLoop`）+ `authorize`（授权状态机） |
| `fuyubot-event` / `service` / `tool` / `middleware` | 四个机制 crate，只依赖 `core`，彼此不依赖 |
| `fuyubot-loader` | 唯一宿主层：VTable 桥、更新管线、插件加载、注册回滚 |
| `fuyubot-simplify` | TDLib 便捷封装（发消息 / 编辑 / Markdown → entities / 命令解析）；`ops` 模块是它的 JSON 操作表，宿主 ABI v5 的 `simplify_invoke*` 就跑它 |
| `fuyubot-plugin-*` | demo / badabi 测试插件 + 三个内置插件（`/start`、`/help`、`/plugins`） |

数据流：插件的 `on/invoke/emit/middleware` → `HostApiVTable` → `HostRuntime`（事件总线 / 服务 / 工具 / 管线 / TdClient）
→ 更新走六阶段 + 双通道（`"update"` 与具体 `@type`），响应按 `@request_id` 回到发起插件。
细节都在文档站的[架构页](https://catmoecircle.github.io/FuyuBot-docs/docs/dev/architecture/)。

## 构建

`td_api.tl` 是**必需输入**：`TDLIB_TD_API_TL` → **包内 `vendor/<tdlib-version>/td_api.tl`**（随 crate 发布，crates.io 下游与 docs.rs 靠它，不联网）→ 缓存（`<缓存根>/<版本>/td_api.tl`）→ 从 Releases 下载。
缓存根顺序：`FUYUBOT_TDLIB_VENDORED_CACHE` → `$CARGO_HOME/fuyubot-tdlib-vendored` → 项目本地 `.tdlib-cache/`。
仓里的 `.cargo/config.toml` 已经把缓存指向仓内 `.tdlib-cache`（`relative = true`），本地直接敲命令即可。

```powershell
cargo build --workspace --offline     # 0 退出、0 warning
cargo test  --workspace --offline     # 341 passed / 0 failed / 12 ignored
cargo tree  --workspace --offline     # 无环
cargo fmt --all --check               # 0 diff

cargo bot                             # = fuyubot run（当前目录）
cargo package-bot                     # release：fuyubot.exe + 三个内置插件（tdjson 由 build.rs 放到 exe 同级）
```

## 改动后怎么验证

除默认自检外，按改动面补跑：

```powershell
# 真库（真 tdjson + 真 schema）：改 ffi 符号/arity、loader、tl_schema/tl_codegen、登录授权后必跑
$env:TDLIB_TDJSON_PATH = "F:\Rust\FuyuBot\.tdlib-cache\1.8.69\windows-x86_64"
cargo test -p fuyubot-tdlib --test integration --offline -- --ignored
cargo test -p fuyubot-tdlib --test sibling_dependencies --offline -- --ignored   # 必须单独跑（独占进程）

# vendored 分支（缓存命中就不联网）
cargo test  -p fuyubot-tdlib --features vendored --offline
cargo build -p fuyubot-cli   --features vendored --offline

# CLI 全流程（不需要账号）
target\debug\fuyubot.exe init --dir <项目> --name demo --api-id 1
target\debug\fuyubot.exe run --replay examples\replay\new_message.json --plugin-dir <项目>\plugins

# 文档站（改 content/docs 后）
cd F:\Rust\FuyuBot-Docs
$env:NEXT_PUBLIC_BASE_PATH='/FuyuBot-docs'; $env:NEXT_PUBLIC_SITE_URL='https://catmoecircle.github.io/FuyuBot-docs'
pnpm build        # push main 自动构建发布到 gh-pages（pages.yml）
```

真实账号登录（要真凭据，凭据只走环境变量或交互输入）：

```powershell
Remove-Item -Recurse -Force target\tdlib-login-test
$env:TG_API_ID="…"; $env:TG_API_HASH="…"; $env:TG_BOT_TOKEN="…"
cargo test -p fuyubot-tdlib --test integration --offline -- --ignored real_bot_login --nocapture
```

## 硬约束（编号是跨文件引用锚，保持不变）

1. **Rust 1.99 stable + Edition 2024**，不许 nightly feature。
2. **依赖方向**：`core` 不依赖 `tdlib`；`abi` 不依赖 `core`；`sys` 不依赖任何 fuyu crate；`tdlib` 依赖并重导出 `sys`；`loader` 是唯一同时依赖四机制 + `tdlib` + `abi` + `core` 的 crate。
3. **依赖版本只写在根 `Cargo.toml` 的 `[workspace.dependencies]`**，成员用 `x.workspace = true`。
4. **profile 只写在根 `Cargo.toml`**（`[profile.debug]` 靠 `inherits = "dev"`）。
5. **每个 crate 都要有 `README.md`**，一句话职责。
6. JSON Schema 只用 `fuyubot-tool`；动态库加载只用 `fuyubot-loader`；`cbindgen` 只是 `abi` 的 build-dependency。
7. **tdjson 运行时加载**：禁止 `#[link(name = ...)]`，符号一律走 `ffi::TdApi` + `loader::resolve_symbols`。
8. **加载优先级不要重排**：`TDLIB_TDJSON_PATH` → vendored → exe 同级目录 → Linux 带版本系统库 → 裸库名。
9. `tar`/`flate2`/`sha2` 只服务 vendored；`ureq` 是常开 build-dependency；**运行时依赖图里不许出现它们**（`cargo tree -p fuyubot-tdlib -e normal` 只有 sys / libloading / serde_json / thiserror / tokio）。
10. **`Context` 只暴露 `&self`**：注册走 `provide` 并**持有** `Effect`，不加 `&mut` 接口。
    ⚠️ **2026 P1 起它有了作用域链**：`Context::new()` 永远造**根**（68 个调用点都靠这个），
    子作用域必须显式 `child()`。查找**沿父链**走（自己这层 → 父 → …），注册**只写自己那层**；
    `isolate(name)` 把某个名字从当前作用域及以下的父链查找里摘掉。
    两个刻意的区分：`names()` 报的是「**能看见**什么」（含父级、去掉被隔离的），
    `len()` / `is_empty()` 报的是「**这一层**存了几个」。
    ✅ **2026 起 dylib 插件有各自的作用域**：`HostRuntime` 在 `begin_plugin` 时给每个 owner 建一份
    `ctx.child()`（存 `plugin_contexts`）；插件的 `provide_service` 落自己那层、要跨插件共享得用
    `provide_service_shared` 显式写根、`get_service` 从自己那层沿父链查，`rollback_owner` 连 child
    一起丢弃。⚠️ 但 `PluginManager` 传给**内置 Rust 插件** `apply` 的仍是共享的根（它们本来就是宿主
    的一部分，不是外部插件）；`isolate` 仍然只服务测试替身与宿主自己的代码。
11. **`Effect` 是 `#[must_use]`**：`let _ =` 会立刻回滚；要么存起来，要么显式 drop / rollback。
12. **四个机制 crate 只依赖 `core`，彼此不许依赖**；组合由宿主层做。
13. **与 Context 集成 = `install(ctx)` + 扩展 trait**；服务缺失时报错要带「该怎么 install」。
14. **扩展 trait 不许与 `Context` 固有方法重名**：命名服务叫 `provide_named` / `resolve`
    —— ⚠️ **2026 Cordis 化之后反过来了**：`Context` 自己有了 `provide_named` / `resolve`
    这两个固有方法（服务表现在按名字寻址），所以 `fuyubot-service` 的扩展 trait
    改叫 **`provide_service` / `get_service`**。规矩本身没变：固有方法永远优先。
    ⚠️ **P1 又占了四个名字**：`child` / `isolate` / `is_isolated` / `has_parent`。
    起新扩展方法前先 `grep` 一下 `crates/fuyubot-core/src/context.rs` 的 `pub fn`。
15. **VTable 每个函数首参是 `host: *mut c_void`，每个函数指针字段都是 `Option<...>`**。
16. **跨 ABI 字符串谁分配谁释放**：宿主不 free 插件的指针；宿主返回的是线程局部缓冲，下次调用前有效。
17. **新的宿主桥一律放 `fuyubot-loader`**。
18. 新增公开 ABI 类型要同步 `crates/fuyubot-abi/build.rs` 的 `export.include`，并真的编译一次 C 头文件。
19. **scope 是「额外定向」**：`emit` 发所有人，`emit_scoped` 发「全局 + 该归属」。
20. **更新发两条通道**：具体 `@type` 与兜底 `"update"`。
21. **过滤器里的 `event_type` 只选通道**，不参与负载匹配。
22. **插件与中间件回调必须 `C-unwind`**；`define_plugin!` 自己也要包 `catch_unwind`。
23. **一个 crate 只能展开一次 `define_plugin!`**（四个符号名是 ABI 定死的）。
24. **`td_api.tl` 的函数/构造器只认 `---functions---` / `---types---` 标记**，不做首字母启发式。
25. **`td_api.tl` 是必需输入**，拿不到就构建失败；顺序是 `TDLIB_TD_API_TL` → **包内 `vendor/<tdlib-version>/td_api.tl`** → 缓存 → 下载，缓存根顺序见上文。升 `tdlib-version` 时记得一起加 `vendor/<新版本>/`，否则下游又变回「要联网」。
26. **转换只在编译期**：`td_api.json` + `td_api_generated.rs` 来自同一份 `tl_schema` 实现。
27. **转换失败 = 构建失败**（读不了 / 解析不了 / 没分区标记都 panic）。
28. 生成器规则：类字段 `Option<Box<T>>`、认不出的类型降级 `serde_json::Value`、类 enum 变体字段不写 `pub`、函数 struct 字段写 `pub`。
29. **tdjson 只用 per-client 那套符号**；`TdExecuteFn` 是两参数；往 `TdApi` 加字段要同步 `size_of` 断言。
30. **加载动态库走 `loader::open_library`**（Windows `LOAD_WITH_ALTERED_SEARCH_PATH`），它先查库自己所在目录；插件加载也走它。
31. **`attach_tdlib` 先、`spawn_dispatch` 后**；停止位必须显式持有并 `stop()`，drop 不会停。
32. **公开 ABI 类型不许用 `macro_rules!` 生成**（cbindgen 不展开宏）。
33. cbindgen 的 `after_includes` 只放宏与注释；用到 ABI 类型的代码放 `config.trailer`。
34. **`Option<fn>` 的签名要内联写**，不能走类型别名（会退化成不完整结构体）。
35. **`fuyubot_plugin.h` 是产物**，权威副本是 `fuyubot_abi::PLUGIN_HEADER`，别签进仓库。
36. **脚本插件四条硬约定**：`plugin_start` 等 `ready`、事件带订阅号、stdio 钉 UTF-8、起不来就报错。
37. `script_plugin!` 内部走 `$crate::__macro_support::…`，不额外要求插件依赖别的 crate。
38. **`run --replay` 不接 TDLib**：回放必须离线可跑。
39. CLI 的模板与 SDK 只有一份来源（`include_str!`），不许复制第二份。
40. **`clap` 只允许出现在两个 CLI crate 里**，库 crate 不许依赖它。
41. **Bot Token / 手机号 / 两步验证密码一律不落盘**：只走 `TG_BOT_TOKEN` 或交互输入。
    `api_id` / `api_hash` 是**例外**：它们**每次冷启动**都要随 `setTdlibParameters` 发出去
    （session 有效也一样），不记住就得每次手输——`run` 交互问过之后**明文写回**
    `fuyubot.toml` 的 `[telegram]` 段（`api_hash` 不进 `Config` 树，插件 `get_config` 读不到；
    插件 `set_config` 的全量重写由 `persist_toml` 补回）。写回只发生在「这次真问过」时。
42. **TDLib 字段路径先问真库**（如 `file.id`；`user` 没有 `is_bot` / `username`）。
43. **TDLib 方法名 ≠ Bot API**：发送一律 `sendMessage` + `input_message_content`；参数名以 `td_api.tl` 为准，并留一条 `check_request` 测试。
44. **`fuyubot-sdk check` 静态读导出表**，绝不 dlopen；ABI 版本只能运行时判定。
45. **改文本用编辑器工具**，不要用 shell 读写源码（PowerShell 会把 UTF-8 当 cp936，毁中文并吞换行）。
46. **发布资产契约**：tag `TDLib-v<版本>`、资产名 `TDLib-<os>-<arch>`（cargo 口径）、内容 `.tar.gz`；加平台要同时改 `ASSETS` 与 CI matrix。
47. **实例状态是 `.fuyubot/` 三个文件**：`lock`（只用于独占）、`status`（启动后发布）、`stop`（优雅停止请求）；不要合并。
48. **构建期把 tdjson 放到可执行文件同级**（`build.rs` 的 staging），改任一侧要同步 `loader::tests`。
49. **发行形态是「下载即用」**：`fuyubot-<版本>-<OS>-<平台>.zip`（`<OS>` = windows / linux / macos，
    `<平台>` = x86_64 / aarch64），**解压出来就是本体**（zip 里不再套一层目录），
    内容是 exe + tdjson + `plugins/`；内置插件**不单独打包**，中间 tar.gz 只留在 Actions artifacts。
50. **Go 的 c-shared 插件不能卸载**：识别 `Go buildinf` 魔数并故意不卸载；同进程重复加载不可靠。
51. **三仓发布顺序**：本仓先发 `fuyubot-abi` / `fuyubot-tdlib-sys` → SDK 发 `fuyubot-plugin-api` → `fuyubot-sdk-script`；不要把 patch 写进 SDK 的 `Cargo.toml`。
52. **插件配置段是 `[plugin.<插件名>]`**（ABI v4）：插件名点分 → TOML 嵌套表，宿主把插件名当**点路径**在 `plugin` 表里下钻（`fuyubot.manage` → `plugin.fuyubot.manage`）。`get_config(section, key)` 读**任意**段（自己 + 别人）；`set_config(key, value)` **没有 section 参数**，只写调用者自己那段（靠 `owner_names` 映射判定，插件无法伪造）。`set_config` 落盘是 `toml::to_string_pretty` **全量重写**（注释会丢），写盘失败要还原内存。脚本插件侧同步入口是 `plugin_config()` / `pluginConfig()`（中间插件在 spawn 前把自己的段塞进 `config` 的 `plugin_config` 键）。
53. **依赖声明以 `plugin_manifest` 为准**：`DylibPlugin::inject()` / `soft_inject()` 先读 manifest（probe 阶段 `read_manifest` 拿到，`plugin_free_string` 归还），为空才退回 `[plugin.<名>].inject` / `.soft_inject`。别把配置当唯一来源——那会让「换台机器忘了抄」变成静默的加载顺序错误。
54. **`reconcile` 不许在 `apply` 期间递归推进**：`apply` 里注册服务会同步触发服务变更钩子；若钩子就地递归，提供者还在 `Activating`（`start` 没跑完）依赖方就被激活，软依赖「基础设施先起来」直接作废、硬依赖也会提前放人。钩子只置 `Progress::needed` 并返回，由外层 `reconcile_loop` 接着推进（`Progress::running` 是闸门，`RunningGuard` 保证 panic 也不焊死）。
55. **新增插件导出符号要三处同步**：`fuyubot-abi` 的 `SYM_*` 常量、`build.rs` 的 `TRAILER`（手写声明）、SDK 的 `sdk check` 的 `ABI_SYMBOLS`。约束 18 只管**类型**（`export.include`），不管函数声明——ABI v4 的 `plugin_manifest` / `fuyubot_abi_version` / `plugin_free_handle` 就漏在头文件外过一轮，C 作者据此**根本不知道要写**。`TRAILER` 里用到 ABI 类型的声明必须留在 trailer（`after_includes` 在所有类型定义之前，放那儿 `-fsyntax-only` 直接报错，约束 33）。
56. **`plugin_manifest` 的 `version` 取 `env!("CARGO_PKG_VERSION")`**（宏在调用点展开，所以是**插件 crate** 的版本，不是 abi 的）：它是真值，不是 `"0.0.0"` 占位。所有字符串过 `quote()` 转义（`"` / `\` / `< 0x20` → `\u00XX`），非 ASCII 原样写 UTF-8。宿主侧 `declared()` 的优先级是「manifest 该侧非空 → 用它，否则回退 `[plugin.<名>].inject`」，**覆盖而非合并、且逐侧判定**——所以插件一旦声明 `inject`，配置里那份就静默失效。
57. **`plugin_free_handle` 的签名永久冻结**：`void plugin_free_handle(PluginHandle)` 只接 handle 本身、**不碰 VTable**，宿主正是在「版本对不上、VTable 一律不可信」那一刻调它（`probe` 与 `load_with_config` 两条路径都必须调，否则 `init` 里分配的状态就漏了）。所以它不能加参数、不能改语义——改了等于让老宿主在未来的插件上乱调。没导出它的 v3 插件只能像以前一样把状态留在那儿，`load` 照样返回 `AbiMismatch`。

58. **simplify 下沉成 op JSON 分发，不是一个方法一个函数指针**（ABI v5，`docs/decisions.md` 变更 12）：
    宿主侧实现在 `fuyubot-simplify::ops`，`HostRuntime::run_simplify` 调它——与 Rust 插件用的是
    **同一个** `Simplify`，不存在第二套行为。加操作 = 往 `SYNC_OPS` / `ASYNC_OPS` 与 `dispatch`
    里加一项，**不动 ABI**。异步 op 只能走 `simplify_invoke`（宿主起工作线程跑，结果照 `@request_id`
    回投）；同步 op（`parse_text_entities` / `rich_from_markdown` / `list_ops`）走
    `simplify_invoke_result`，回调里安全，传异步 op 返回 `code:"async_op"`。
    ⚠️ op 工作线程的归属是**宿主自己**（`owner = 0`），它顺手发出的 TDLib 请求的中间响应只到
    全局订阅者——插件只该收到最后那一条 `simplifyResult`。

## 文档

公开文档只在**文档站**（`FuyuBot-Docs/content/docs/**`）；改 ABI / CLI / 行为时同步那边。
本仓 `docs/` 下是随源码分发的工程台账（HANDOFF / ROADMAP / gaps / pitfalls / decisions / tech-debt），不入库，也不再要求随每次改动更新。
