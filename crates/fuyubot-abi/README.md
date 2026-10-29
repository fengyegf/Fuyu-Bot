# fuyubot-abi

宿主与插件之间唯一的稳定边界：C ABI 定义、插件导出宏与可 `.await` 的响应路由（ABI 版本 5，发布于 [crates.io](https://crates.io/crates/fuyubot-abi)）。

[![](https://img.shields.io/badge/powered_by-dsh-4D6BFE?style=flat-square&logo=deepseek&logoColor=white)](https://github.com/deepseek-ai/deepseek-harness)

## 里面有什么

- **`HostApiVTable` / `ServiceVTable`** —— `#[repr(C)]` 函数指针表：每个函数首参是 `host: *mut c_void`，每个字段都是 `Option<...>`。
- **插件必须导出的七个符号** —— v3 四件套 `plugin_init` / `plugin_start` / `plugin_stop` / `plugin_free_string`，加 v4 的 `plugin_manifest` / `fuyubot_abi_version` / `plugin_free_handle`。
- **插件侧门面** —— `HostApi` 门面与 `define_plugin!` 宏（自带 `catch_unwind`；一个 crate 只能展开一次）。
- **Rust 侧等待原语** —— `ResponseRouter` / `ResponseFuture` / `UpdateFuture`，`.await` 响应与更新，不改 C 布局。
- **跨语言的 simplify 入口（v5）** —— `simplify_invoke(op_json) -> RequestId`（异步，结果走 `"response"` 通道）与 `simplify_invoke_result(op_json) -> JSON`（同步，只收不碰网络的操作）。操作以 **op JSON 分发**（`{"op":"send_message","args":{…}}`），加操作**不动 ABI**。

C 头文件 `fuyubot_plugin.h` 是构建产物，权威副本是 `fuyubot_abi::PLUGIN_HEADER`；跨 ABI 字符串谁分配谁释放（宿主返回的是线程局部缓冲）。

## 相关

- [插件开发](https://catmoecircle.github.io/FuyuBot-docs/docs/plugin/)
- [仓库](https://github.com/CatMoeCircle/Fuyu-Bot)
