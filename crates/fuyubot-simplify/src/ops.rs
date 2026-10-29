//! simplify 的 **JSON 操作表**：ABI v5 `simplify_invoke` / `simplify_invoke_result` 的宿主侧实现。
//!
//! ## 为什么是「一张表」而不是一堆函数指针
//!
//! `fuyubot-simplify` 的价值在**语义**（发消息要等结局、超时先撤回、bot 与用户号
//! 反应走不同方法），不在方法数量。把这些语义做成 20 个 VTable 字段，ABI 每加一个
//! 方法就要升一次版本、三个仓库一起动。所以这里改成 **op JSON 分发**：
//!
//! ```json
//! {"op": "send_message", "args": {"chat_id": 123, "input_message_content": {…}}}
//! ```
//!
//! 一个入口覆盖全部操作，**加操作不动 ABI**。宿主（`fuyubot-loader`）直接调
//! [`run`]，Rust 插件也可以调它——同一份代码，行为一致。
//!
//! ## 入参的形状
//!
//! `args` 基本就是对应 TDLib 方法的参数（键名以 `td_api.tl` 为准），外加两个统一项：
//!
//! - `timeout_ms`（可选，默认 [`DEFAULT_TIMEOUT_MS`]）：整次操作共用的等待预算；
//! - 文本类字段（`text` / `caption`）可以是**字符串**（按纯文本）或
//!   `{"text":"…","parse_mode":"markdown|markdown_v2|html"}`——宿主替你走
//!   `parseTextEntities` 换成 `formattedText`。已经是 `formattedText` 就原样放过。
//!
//! 认不出的键**原样透传**给 TDLib（宿主会照 schema 警告但不拦），这样 TDLib 升版
//! 加了新参数也不必等 ABI 升版。
//!
//! ## 同步 vs 异步
//!
//! [`is_sync_op`] 判定一个 op 能不能在**回调里**跑：只有不碰网络的（`parse_text_entities`、
//! `rich_from_markdown`、`list_ops`）是同步的，其余会阻塞等 TDLib 应答，必须走
//! `simplify_invoke`（宿主在别的线程里跑）。

use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::{
    AdminRight, DEFAULT_TIMEOUT_MS, ParseMode, Result as SimplifyResult, Simplify, SimplifyError,
    Text, remaining_ms,
};

/// 操作名在 op JSON 里的键。与 `fuyubot_abi::SIMPLIFY_OP_KEY` 是同一个值。
pub const OP_KEY: &str = "op";
/// 操作入参在 op JSON 里的键。
pub const ARGS_KEY: &str = "args";

/// 一个操作的失败原因：给跨语言调用方的**可读**错误（`code` + `message`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpError {
    /// 稳定的英文代号（`"missing_field"` / `"unknown_op"` / `"timeout"` …），
    /// 调用方可以拿它分支，不用去匹配中文文案。
    pub code: &'static str,
    pub message: String,
}

impl OpError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({ "code": self.code, "message": self.message })
    }
}

impl std::fmt::Display for OpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}（{}）", self.message, self.code)
    }
}

impl std::error::Error for OpError {}

impl From<SimplifyError> for OpError {
    fn from(error: SimplifyError) -> Self {
        let code = match &error {
            SimplifyError::Timeout(_) => "timeout",
            SimplifyError::SendFailed { .. } => "send_failed",
            SimplifyError::Tdlib { .. } => "tdlib",
            SimplifyError::AlbumSize(_) => "bad_args",
            SimplifyError::EmptyForward | SimplifyError::TooManyForward(_) => "bad_args",
            _ => "simplify",
        };
        Self::new(code, error.to_string())
    }
}

impl From<fuyubot_abi::AbiError> for OpError {
    fn from(error: fuyubot_abi::AbiError) -> Self {
        Self::new("host", error.to_string())
    }
}

type OpOutcome = std::result::Result<Value, OpError>;

/// 全部**同步**操作（不碰网络，回调里安全）。
pub const SYNC_OPS: &[&str] = &["parse_text_entities", "rich_from_markdown", "list_ops"];

/// 全部**异步**操作（要等 TDLib 应答，只能走 `simplify_invoke`）。
pub const ASYNC_OPS: &[&str] = &[
    "send_message",
    "send_message_album",
    "edit_message_text",
    "edit_message_caption",
    "edit_message_media",
    "edit_message_reply_markup",
    "delete_messages",
    "answer_callback_query",
    "forward_messages",
    "react",
    "unreact",
    "add_reaction",
    "set_reactions",
    "me",
    "my_id",
    "is_bot",
    "chat_member",
    "is_chat_admin",
    "am_i_chat_admin",
    "download_file",
    "invoke",
];

/// `op` 是不是同步操作。
pub fn is_sync_op(op: &str) -> bool {
    SYNC_OPS.contains(&op)
}

/// 宿主认识的 op（同步在前）。
pub fn known_ops() -> Vec<&'static str> {
    SYNC_OPS.iter().chain(ASYNC_OPS).copied().collect()
}

/// 跑一个操作。异步 op 在**当前线程**跑（会阻塞），所以只有宿主的工作线程
/// （或 `*_then` 那类场景）该调它。
pub fn run(simplify: &Simplify, request: &Value) -> OpOutcome {
    let op = request
        .get(OP_KEY)
        .and_then(Value::as_str)
        .ok_or_else(|| OpError::new("missing_op", format!("op JSON 缺字符串字段 `{OP_KEY}`")))?;
    let args = match request.get(ARGS_KEY) {
        None | Some(Value::Null) => Map::new(),
        Some(value) => value
            .as_object()
            .cloned()
            .ok_or_else(|| OpError::new("bad_args", "`args` 必须是 JSON 对象"))?,
    };
    let outcome = dispatch(simplify, op, &args);
    if let Err(error) = &outcome
        && error.code == "unknown_op"
    {
        simplify.host().log(
            fuyubot_abi::LogLevel::Warn,
            &format!("不认识的 simplify op `{op}`，已知：{:?}", known_ops()),
        );
    }
    outcome
}

/// 把结果包成跨边界的 JSON：`{"@type":"simplifyResult","op":…,"ok":…,"result"|"error":…}`。
///
/// 成功与失败**同一个 `@type`**：调用方只看 `ok`，不用记两套形状。
pub fn to_result_json(op: &str, outcome: OpOutcome) -> Value {
    match outcome {
        Ok(result) => json!({
            "@type": fuyubot_abi::SIMPLIFY_RESULT_TYPE,
            "op": op,
            "ok": true,
            "result": result,
        }),
        Err(error) => json!({
            "@type": fuyubot_abi::SIMPLIFY_RESULT_TYPE,
            "op": op,
            "ok": false,
            "error": error.to_json(),
        }),
    }
}

// ── 分发 ──────────────────────────────────────────────────────────

fn dispatch(simplify: &Simplify, op: &str, args: &Map<String, Value>) -> OpOutcome {
    match op {
        // 同步
        "parse_text_entities" => Ok(parse_text_entities(simplify, args)?),
        "rich_from_markdown" => Ok(rich_from_markdown(args)?),
        "list_ops" => Ok(json!({ "sync": SYNC_OPS, "async": ASYNC_OPS })),
        // 异步
        "send_message" => Ok(send_message(simplify, args)?),
        "send_message_album" => Ok(send_message_album(simplify, args)?),
        "edit_message_text" => Ok(edit("editMessageText", simplify, args)?),
        "edit_message_caption" => Ok(edit("editMessageCaption", simplify, args)?),
        "edit_message_media" => Ok(edit("editMessageMedia", simplify, args)?),
        "edit_message_reply_markup" => {
            let mut args = args.clone();
            // 键盘允许显式 `null` = 移除（`replyMarkupRemoveKeyboard`），
            // 只有**缺这个键**才是调用方写漏了。
            if !args.contains_key("reply_markup") {
                args.insert(
                    "reply_markup".to_owned(),
                    json!({"@type": "replyMarkupRemoveKeyboard"}),
                );
            }
            Ok(edit("editMessageReplyMarkup", simplify, &args)?)
        }
        "delete_messages" => Ok(edit("deleteMessages", simplify, args)?),
        "answer_callback_query" => Ok(edit("answerCallbackQuery", simplify, args)?),
        "forward_messages" => Ok(forward_messages(simplify, args)?),
        "react" | "unreact" | "add_reaction" | "set_reactions" => Ok(reaction(simplify, op, args)?),
        "me" => Ok(simplify.me(timeout_of(args)).unwrap_or(Value::Null)),
        "my_id" => Ok(json!(simplify.my_id(timeout_of(args)))),
        "is_bot" => Ok(json!(simplify.is_bot(timeout_of(args)))),
        "chat_member" => Ok(simplify
            .chat_member(
                required_i64(args, "chat_id")?,
                required_i64(args, "user_id")?,
                timeout_of(args),
            )
            .unwrap_or(Value::Null)),
        "is_chat_admin" => Ok(json!(simplify.is_chat_admin(
            required_i64(args, "chat_id")?,
            required_i64(args, "user_id")?,
            admin_right_of(args)?,
            timeout_of(args),
        ))),
        "am_i_chat_admin" => Ok(json!(simplify.am_i_chat_admin(
            required_i64(args, "chat_id")?,
            admin_right_of(args)?,
            timeout_of(args),
        ))),
        "download_file" => Ok(download_file(simplify, args)?),
        "invoke" => Ok(invoke(simplify, args)?),
        _ => Err(OpError::new("unknown_op", format!("不认识的 op `{op}`"))),
    }
}

// ── 异步：发送类 ──────────────────────────────────────────────────

/// `sendMessage`：发 → 等响应拿临时消息 → 等结局（超时先撤回）。
fn send_message(simplify: &Simplify, args: &Map<String, Value>) -> OpOutcome {
    let chat_id = required_i64(args, "chat_id")?;
    let timeout_ms = timeout_of(args);
    let mut payload = passthrough(args);
    format_fields(&mut payload, simplify);

    let request = simplify
        .host()
        .td_invoke_simplified(crate::td_api::SendMessage::METHOD, &payload.to_string())?;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let pending = simplify.await_response(request, deadline, timeout_ms)?;
    let old_message_id = pending.get("id").and_then(Value::as_i64);
    done(simplify.await_outcome(chat_id, old_message_id, deadline, timeout_ms))
}

/// `sendMessageAlbum`：整组一个预算，逐项等结局，任一项失败即返回。
fn send_message_album(simplify: &Simplify, args: &Map<String, Value>) -> OpOutcome {
    let chat_id = required_i64(args, "chat_id")?;
    let timeout_ms = timeout_of(args);
    let mut payload = passthrough(args);
    format_fields(&mut payload, simplify);

    let request = simplify.host().td_invoke_simplified(
        crate::td_api::SendMessageAlbum::METHOD,
        &payload.to_string(),
    )?;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let response = simplify.await_response(request, deadline, timeout_ms)?;
    let ids: Vec<Option<i64>> = crate::pending_messages(&response)
        .iter()
        .map(|message| message.get("id").and_then(Value::as_i64))
        .collect();

    let mut messages = Vec::with_capacity(ids.len());
    for (index, old_message_id) in ids.iter().enumerate() {
        let Some(old_message_id) = *old_message_id else {
            simplify.cancel_sending(chat_id, ids[index..].iter().flatten());
            return Err(OpError::from(SimplifyError::Timeout(timeout_ms)));
        };
        messages.push(simplify.await_outcome(
            chat_id,
            Some(old_message_id),
            deadline,
            timeout_ms,
        )?);
    }
    Ok(Value::Array(messages))
}

/// 编辑 / 删除 / 回 callback 这一类：**结果就是响应**，没有发送结局那对更新。
fn edit(method: &str, simplify: &Simplify, args: &Map<String, Value>) -> OpOutcome {
    let timeout_ms = timeout_of(args);
    let mut payload = passthrough(args);
    format_fields(&mut payload, simplify);
    let request = simplify
        .host()
        .td_invoke_simplified(method, &payload.to_string())?;
    done(simplify.await_response(
        request,
        Instant::now() + Duration::from_millis(timeout_ms),
        timeout_ms,
    ))
}

/// `forwardMessages`：等响应，返回那一组（还没发完的）临时消息。
fn forward_messages(simplify: &Simplify, args: &Map<String, Value>) -> OpOutcome {
    let timeout_ms = timeout_of(args);
    let mut message_ids = args
        .get("message_ids")
        .and_then(Value::as_array)
        .map(|ids| ids.iter().filter_map(Value::as_i64).collect::<Vec<_>>())
        .ok_or_else(|| OpError::new("missing_field", "args 缺数组字段 `message_ids`"))?;
    if message_ids.is_empty() {
        return Err(OpError::new("bad_args", "转发列表是空的"));
    }
    if message_ids.len() > 100 {
        return Err(OpError::new(
            "bad_args",
            format!("一次最多转发 100 条，收到 {} 条", message_ids.len()),
        ));
    }
    message_ids.sort_unstable();
    message_ids.dedup();

    let mut payload = passthrough(args);
    set(&mut payload, "message_ids", json!(message_ids));
    let request = simplify
        .host()
        .td_invoke_simplified(crate::td_api::ForwardMessages::METHOD, &payload.to_string())?;
    let response = simplify.await_response(
        request,
        Instant::now() + Duration::from_millis(timeout_ms),
        timeout_ms,
    )?;
    Ok(Value::Array(crate::pending_messages(&response)))
}

/// 表情反应：`react` / `unreact` 自动挑方法（bot 只能整组覆盖），
/// `add_reaction` / `set_reactions` 是显式走某一条路。
fn reaction(simplify: &Simplify, op: &str, args: &Map<String, Value>) -> OpOutcome {
    let chat_id = required_i64(args, "chat_id")?;
    let message_id = required_i64(args, "message_id")?;
    let timeout_ms = timeout_of(args);
    let is_big = args.get("is_big").and_then(Value::as_bool).unwrap_or(false);
    let single = reaction_type_of(args)?;

    let (method, payload) = match op {
        "react" => {
            let is_bot = args
                .get("is_bot")
                .and_then(Value::as_bool)
                .unwrap_or_else(|| simplify.is_bot(timeout_ms));
            if is_bot {
                (
                    crate::td_api::SetMessageReactions::METHOD,
                    json!({"chat_id": chat_id, "message_id": message_id,
                           "reaction_types": [single], "is_big": is_big}),
                )
            } else {
                (
                    crate::td_api::AddMessageReaction::METHOD,
                    json!({"chat_id": chat_id, "message_id": message_id,
                           "reaction_type": single, "is_big": is_big,
                           "update_recent_reactions": true}),
                )
            }
        }
        "unreact" => {
            let is_bot = args
                .get("is_bot")
                .and_then(Value::as_bool)
                .unwrap_or_else(|| simplify.is_bot(timeout_ms));
            if is_bot {
                (
                    crate::td_api::SetMessageReactions::METHOD,
                    json!({"chat_id": chat_id, "message_id": message_id,
                           "reaction_types": [], "is_big": false}),
                )
            } else {
                (
                    crate::td_api::RemoveMessageReaction::METHOD,
                    json!({"chat_id": chat_id, "message_id": message_id,
                           "reaction_type": single}),
                )
            }
        }
        "add_reaction" => (
            crate::td_api::AddMessageReaction::METHOD,
            json!({"chat_id": chat_id, "message_id": message_id,
                   "reaction_type": single, "is_big": is_big,
                   "update_recent_reactions": true}),
        ),
        _ => {
            let reactions = args
                .get("reaction_types")
                .and_then(Value::as_array)
                .cloned()
                // 也认单数：`set_reactions` 传一个 `reaction` 就是「只留这一个」。
                .or_else(|| single.as_array().cloned())
                .unwrap_or_default();
            (
                crate::td_api::SetMessageReactions::METHOD,
                json!({"chat_id": chat_id, "message_id": message_id,
                       "reaction_types": reactions, "is_big": is_big}),
            )
        }
    };

    let request = simplify
        .host()
        .td_invoke_simplified(method, &payload.to_string())?;
    done(simplify.await_response(
        request,
        Instant::now() + Duration::from_millis(timeout_ms),
        timeout_ms,
    ))
}

/// 通用逃生舱：`args.request` 是**完整**的 TDLib 请求 JSON，发 + 等响应。
///
/// 操作表没覆盖的方法都走它——宿主这层不拦，TDLib 认不认由真库说了算。
fn invoke(simplify: &Simplify, args: &Map<String, Value>) -> OpOutcome {
    let request = args.get("request").cloned().ok_or_else(|| {
        OpError::new(
            "missing_field",
            "args 缺 `request`（完整的 TDLib 请求 JSON）",
        )
    })?;
    let timeout_ms = timeout_of(args);
    let id = simplify.host().td_invoke(&request.to_string())?;
    done(simplify.await_response(
        id,
        Instant::now() + Duration::from_millis(timeout_ms),
        timeout_ms,
    ))
}

/// `downloadFile`：发 + 等响应，再等到 `updateFile` 报下载完成，返回最终的 `file`。
fn download_file(simplify: &Simplify, args: &Map<String, Value>) -> OpOutcome {
    let file_id = required_i64(args, "file_id")?;
    let timeout_ms = timeout_of(args);
    let mut payload = passthrough(args);
    set(&mut payload, "offset", json!(0));
    set(&mut payload, "limit", json!(0));
    let request = simplify
        .host()
        .td_invoke_simplified(crate::td_api::DownloadFile::METHOD, &payload.to_string())?;
    simplify.await_response(
        request,
        Instant::now() + Duration::from_millis(timeout_ms),
        timeout_ms,
    )?;

    // `updateFile` 的 file id 在 `file.id`，不是顶层。
    let filter = json!({
        fuyubot_abi::FILTER_EVENT_TYPE_KEY: fuyubot_abi::UPDATE_EVENT,
        "file.id": file_id,
        "file.local.is_downloading_completed": true,
    })
    .to_string();
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let update = crate::payload_of(
        simplify
            .host()
            .wait_for_update(&filter, remaining_ms(deadline)),
    );
    Ok(update
        .and_then(|update| update.get("file").cloned())
        .unwrap_or(Value::Null))
}

// ── 同步 ──────────────────────────────────────────────────────────

/// `parseTextEntities`：文本 + 解析模式 → `formattedText`（同步，回调里安全）。
fn parse_text_entities(simplify: &Simplify, args: &Map<String, Value>) -> OpOutcome {
    let raw = args
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| OpError::new("missing_field", "args 缺字符串字段 `text`"))?;
    let mode = parse_mode_of(
        args.get("parse_mode")
            .and_then(Value::as_str)
            .unwrap_or("plain"),
    );
    Ok(simplify.formatted(&Text::new(raw.to_owned(), mode)))
}

/// 富消息：本地 Markdown → `inputRichMessage`（再把 `text` 那类字段过一遍解析）。
///
/// 转出来的是 `input_message_content` 的形状，可以直接塞进 `send_message` 的
/// `input_message_content`——这就把 simplify 里最复杂的那块（24 种块 / 31 种行内）
/// 交给了所有语言。
fn rich_from_markdown(args: &Map<String, Value>) -> OpOutcome {
    let markdown = args
        .get("markdown")
        .and_then(Value::as_str)
        .ok_or_else(|| OpError::new("missing_field", "args 缺字符串字段 `markdown`"))?;
    let mut rich = crate::RichMessage::from_markdown(markdown);
    if let Some(rtl) = args.get("is_rtl").and_then(Value::as_bool) {
        rich.is_rtl = rtl;
    }
    if let Some(detect) = args.get("detect_automatic_blocks").and_then(Value::as_bool) {
        rich.detect_automatic_blocks = detect;
    }
    let mut content = json!({
        "@type": "inputMessageRichText",
        "rich_message": rich.to_json(),
    });
    if let Some(caption) = args.get("caption") {
        content["caption"] = caption.clone();
    }
    Ok(content)
}

// ── 入参小工具 ────────────────────────────────────────────────────

/// `args` 原样留一份当请求体，只摘掉 `timeout_ms`（它是本层的预算，不是 TDLib 参数）。
fn passthrough(args: &Map<String, Value>) -> Value {
    let mut payload = args.clone();
    payload.remove("timeout_ms");
    Value::Object(payload)
}

/// `Simplify` 那些阻塞等待方法的返回值 → 本模块的统一错误类型。
fn done(result: SimplifyResult<Value>) -> OpOutcome {
    result.map_err(OpError::from)
}

/// 往请求体里塞一个字段（请求体一定是对象，`passthrough` 保证）。
fn set(payload: &mut Value, key: &str, value: Value) {
    if let Value::Object(map) = payload {
        map.insert(key.to_owned(), value);
    }
}

fn timeout_of(args: &Map<String, Value>) -> u64 {
    args.get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_TIMEOUT_MS)
}

fn required_i64(args: &Map<String, Value>, key: &str) -> std::result::Result<i64, OpError> {
    args.get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| OpError::new("missing_field", format!("args 缺整数字段 `{key}`")))
}

fn admin_right_of(args: &Map<String, Value>) -> std::result::Result<Option<AdminRight>, OpError> {
    match args.get("right").and_then(Value::as_str) {
        None | Some("") => Ok(None),
        Some(key) => crate::AdminRight::from_key(key)
            .map(Some)
            .ok_or_else(|| OpError::new("bad_args", format!("`right` 不是已知权限名：{key}"))),
    }
}

/// 反应类型：`{"@type":"reactionTypeEmoji","emoji":"👍"}` 原样用，
/// 纯字符串按 emoji 处理，`{"emoji":…}` / `{"custom_emoji_id":…}` 也能认。
fn reaction_type_of(args: &Map<String, Value>) -> OpOutcome {
    let Some(value) = args.get("reaction").or_else(|| args.get("reaction_type")) else {
        return Err(OpError::new(
            "missing_field",
            "args 缺 `reaction`（或 `reaction_type`）",
        ));
    };
    if let Value::Object(map) = value
        && map.contains_key("@type")
    {
        return Ok(value.clone());
    }
    if let Some(emoji) = value.as_str() {
        return Ok(json!({ "@type": "reactionTypeEmoji", "emoji": emoji }));
    }
    if let Value::Object(map) = value {
        if let Some(emoji) = map.get("emoji").and_then(Value::as_str) {
            return Ok(json!({ "@type": "reactionTypeEmoji", "emoji": emoji }));
        }
        if let Some(id) = map.get("custom_emoji_id").and_then(Value::as_i64) {
            return Ok(json!({ "@type": "reactionTypeCustomEmoji", "custom_emoji_id": id }));
        }
    }
    Err(OpError::new(
        "bad_args",
        "`reaction` 得是 emoji 字符串、{emoji}、{custom_emoji_id} 或带 @type 的 ReactionType",
    ))
}

fn parse_mode_of(mode: &str) -> ParseMode {
    match mode.to_ascii_lowercase().replace(['-', ' '], "_").as_str() {
        "markdown" | "markdown_v1" | "markdownv1" => ParseMode::Markdown,
        "markdown_v2" | "markdownv2" => ParseMode::MarkdownV2,
        "html" => ParseMode::Html,
        _ => ParseMode::Plain,
    }
}

/// 把请求体里的文本字段换成 `formattedText`（递归 `input_message_content(s)`）。
fn format_fields(node: &mut Value, simplify: &Simplify) {
    match node {
        Value::Object(map) => {
            for key in ["text", "caption"] {
                if let Some(value) = map.get_mut(key) {
                    format_text(value, simplify);
                }
            }
            if let Some(content) = map.get_mut("input_message_content") {
                format_fields(content, simplify);
            }
            if let Some(Value::Array(items)) = map.get_mut("input_message_contents") {
                for item in items.iter_mut() {
                    format_fields(item, simplify);
                }
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                format_fields(item, simplify);
            }
        }
        _ => {}
    }
}

fn format_text(value: &mut Value, simplify: &Simplify) {
    let text = match value {
        Value::String(raw) => Text::plain(raw.clone()),
        Value::Object(map) => {
            // 已经是 `formattedText`（或任何 TDLib 直接认的形状）：别动。
            if map.contains_key("@type") {
                return;
            }
            let Some(raw) = map.get("text").and_then(Value::as_str) else {
                return;
            };
            let mode = parse_mode_of(
                map.get("parse_mode")
                    .and_then(Value::as_str)
                    .unwrap_or("plain"),
            );
            Text::new(raw.to_owned(), mode)
        }
        _ => return,
    };
    *value = simplify.formatted(&text);
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyubot_abi::HostApi;
    use fuyubot_core::Context;
    use fuyubot_loader::HostRuntime;
    use std::sync::Arc;

    fn runtime() -> Arc<HostRuntime> {
        HostRuntime::new(Context::new()).expect("运行时应该建得起来")
    }

    fn simplify(rt: &Arc<HostRuntime>) -> Simplify {
        // SAFETY: rt 比 simplify 活得久。
        let host = unsafe { HostApi::from_raw(rt.vtable_ptr()) }.unwrap();
        Simplify::new(host)
    }

    /// `timeout_ms: 0` 让等待立刻超时：这些测试只验**请求怎么拼**。
    const QUICK: &str = r#"{"timeout_ms":0}"#;

    #[test]
    fn send_message_builds_a_real_send_message_and_formats_the_text() {
        let rt = runtime();
        let simplify = simplify(&rt);
        let outcome = run(
            &simplify,
            &json!({
                "op": "send_message",
                "args": {
                    "chat_id": 1,
                    "input_message_content": {
                        "@type": "inputMessageText",
                        "text": { "text": "*hi*", "parse_mode": "markdown" }
                    },
                    "timeout_ms": 0
                }
            }),
        );
        // 离线没有 TDLib，等不到响应 —— 但请求必须**已经拼对**并发出去了。
        assert!(outcome.is_err(), "{outcome:?}");
        let sent = rt.sent_requests();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0].method, "sendMessage");
        assert_eq!(sent[0].params["chat_id"], 1);
        // 文本字段经 `parseTextEntities` 换成了 `formattedText`（没接 TDLib 时退回纯文本）。
        assert_eq!(
            sent[0].params["input_message_content"]["text"]["@type"],
            "formattedText"
        );
        // `timeout_ms` 是本层的预算，不该混进 TDLib 参数里。
        assert!(
            sent[0].params.get("timeout_ms").is_none(),
            "{:?}",
            sent[0].params
        );
    }

    #[test]
    fn album_passes_every_content_through_the_formatter() {
        let rt = runtime();
        let simplify = simplify(&rt);
        let _ = run(
            &simplify,
            &json!({
                "op": "send_message_album",
                "args": {
                    "chat_id": 1,
                    "input_message_contents": [
                        { "@type": "inputMessageText", "text": { "text": "a", "parse_mode": "html" } },
                        { "@type": "inputMessagePhoto", "caption": { "text": "b" } }
                    ],
                    "timeout_ms": 0
                }
            }),
        );
        let sent = rt.sent_requests();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0].method, "sendMessageAlbum");
        let contents = sent[0].params["input_message_contents"].as_array().unwrap();
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[0]["text"]["@type"], "formattedText");
        assert_eq!(contents[1]["caption"]["@type"], "formattedText");
    }

    #[test]
    fn edit_and_delete_are_one_request_each() {
        let rt = runtime();
        let simplify = simplify(&rt);
        let _ = run(
            &simplify,
            &json!({"op":"edit_message_text","args":{"chat_id":1,"message_id":9,
                "input_message_content":{"@type":"inputMessageText","text":{"text":"x"}},
                "timeout_ms":0}}),
        );
        let _ = run(
            &simplify,
            &json!({"op":"delete_messages","args":{"chat_id":1,"message_ids":[7],"revoke":true,"timeout_ms":0}}),
        );
        let sent = rt.sent_requests();
        assert_eq!(sent.len(), 2, "{sent:?}");
        assert_eq!(sent[0].method, "editMessageText");
        assert_eq!(sent[1].method, "deleteMessages");
        assert_eq!(sent[1].params["revoke"], true);
    }

    /// 键盘那条路：`reply_markup` 可以显式给 `null`？不行——`null` 会被当成
    /// 「调用方没给」，这里补成「移除键盘」。要留键盘就直接给对象。
    #[test]
    fn edit_reply_markup_defaults_to_removing_the_keyboard() {
        let rt = runtime();
        let simplify = simplify(&rt);
        let _ = run(
            &simplify,
            &json!({"op":"edit_message_reply_markup","args":{"chat_id":1,"message_id":9,"timeout_ms":0}}),
        );
        let sent = rt.sent_requests();
        assert_eq!(
            sent[0].params["reply_markup"]["@type"],
            "replyMarkupRemoveKeyboard"
        );
    }

    #[test]
    fn sync_ops_need_no_round_trip() {
        let rt = runtime();
        let simplify = simplify(&rt);

        let parsed = run(
            &simplify,
            &json!({"op":"parse_text_entities","args":{"text":"hi"}}),
        )
        .unwrap();
        assert_eq!(parsed["@type"], "formattedText");
        assert_eq!(parsed["text"], "hi");

        let rich = run(
            &simplify,
            &json!({"op":"rich_from_markdown","args":{"markdown":"# t\n\nbody"}}),
        )
        .unwrap();
        assert_eq!(rich["@type"], "inputMessageRichText");
        let blocks = rich["rich_message"]["source"]["blocks"].as_array().unwrap();
        assert!(!blocks.is_empty(), "{rich}");

        assert!(rt.sent_requests().is_empty(), "同步 op 不该发请求");
    }

    #[test]
    fn unknown_and_malformed_ops_say_so() {
        let rt = runtime();
        let simplify = simplify(&rt);

        let error = run(&simplify, &json!({"op":"nope"})).unwrap_err();
        assert_eq!(error.code, "unknown_op");

        let error = run(&simplify, &json!({"args":{}})).unwrap_err();
        assert_eq!(error.code, "missing_op");

        let error = run(&simplify, &json!({"op":"send_message","args":{}})).unwrap_err();
        assert_eq!(error.code, "missing_field");
    }

    #[test]
    fn result_json_has_one_shape_for_ok_and_err() {
        let ok = to_result_json("send_message", Ok(json!({"id": 1})));
        assert_eq!(ok["ok"], true);
        assert_eq!(ok["op"], "send_message");
        assert_eq!(ok["result"]["id"], 1);
        assert!(ok.get("error").is_none());

        let err = to_result_json("send_message", Err(OpError::new("timeout", "等了 30 ms")));
        assert_eq!(err["ok"], false);
        assert_eq!(err["error"]["code"], "timeout");
        assert!(err.get("result").is_none());
        // 成功与失败同一个 `@type`：调用方只看 `ok`，不用记两套形状。
        assert_eq!(ok["@type"], err["@type"]);
    }

    /// 超时那条路的错误代号要稳：跨语言调用方靠它分支，不能改一次文案就坏。
    #[test]
    fn timeout_is_its_own_code() {
        let error = OpError::from(SimplifyError::Timeout(30));
        assert_eq!(error.code, "timeout");
        let error = OpError::from(SimplifyError::Tdlib {
            code: 400,
            message: "x".to_owned(),
        });
        assert_eq!(error.code, "tdlib");
    }

    #[test]
    fn every_op_in_the_table_is_either_sync_or_async() {
        for op in ASYNC_OPS {
            assert!(!is_sync_op(op), "{op} 不该同时在两张表里");
        }
        // 操作名不许撞车（撞了说明有人复制粘贴时忘了改）。
        let mut all = known_ops();
        let total = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), total, "操作表里有重名：{all:?}");
        let _ = QUICK;
    }
}
