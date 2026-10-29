//! TDLib 便捷封装：**只简化输入，不改变输出**。
//!
//! ⚠️ **新代码优先 `fuyubot_abi::ResponseRouter`**：本 crate 的方法是**同步阻塞**的
//! （内部 `wait_for_update`），只适合已经在同步上下文里的代码；在宿主事件回调里请用
//! `*_then`（把等待丢到新线程），或者直接用 `ResponseRouter::invoke_simplified(..).await`。
//! 这里的阻塞 API 保留给同步宿主 / 兼容层，不再是推荐的插件写法。
//!
//! 参数形状刻意跟「写 TDLib 的人已经习惯的那一套」保持一致——一个方法一个参数结构体，
//! 字段用短名字（`text` / `media` / `topic_id` / `reply_to_message_id` / `reply_markup` /
//! `link_preview` / `timeout_ms`），**不暴露 TDLib 的嵌套 JSON**：
//!
//! ```ignore
//! let message = simplify.send_message(chat_id, &SendMessageParams {
//!     text: Some(Text::markdown("*你好*")),
//!     reply_to_message_id: Some(42),
//!     ..SendMessageParams::default()
//! })?; // 等到发送结局：成功就是最终那条 Message
//! ```
//!
//! 请求本身一律经 [`HostApi::td_invoke`] / [`HostApi::td_invoke_simplified`] 发出，
//! 不绕过 ABI、也不碰 TDLib 的库；原始更新照旧全部对插件可见（这里只**额外**推简化事件）。
//!
//! ## 文本格式（Markdown / HTML）
//!
//! `formattedText` 那条链路**没有自己的 Markdown 解析器**：带标记的文本经 [`Simplify::formatted`]
//! 交给 TDLib 的 `parseTextEntities`，走的是**同步**的 [`HostApi::td_execute`]
//! （`td_json_client_execute`）。同步很关键——`parseTextEntities` 不碰服务器，TDLib 允许这么调；
//! 换成「发请求 + 等响应通道」就会把投递线程卡住（插件通常正是在更新回调里发消息）。
//! 拿不到结果就退回纯文本。
//!
//! 富消息（`messageRichMessage`）是另一回事：它的 Markdown/HTML 源**由服务端解析、且仅 bot 可用**，
//! 本地连中间态都没有。为了让用户号也能写 Markdown，本 crate 带了一份**受控子集**转换器
//! （[`markdown`]，产出 `InputPageBlock` 块树）。它只认自己文档里列出的语法，
//! 认不出的一律退回普通文本——**不是** Telegram 方言的完整实现，两者别混用。
//!
//! 富消息的行内表达式是独立体系（`RichText`，31 个构造器，**没有** offset/length）。
//! Markdown 覆盖其中 21 个（含行内 HTML 标签）；剩下 10 个（`mention` / `hashtag` /
//! `cashtag` / `bankCardNumber` / `tonAddress` / `botCommand` / `icon` / `button` /
//! `diff` / `anchor`）没有 md 语法，用 [`richtext`] 结构化拼，再塞进块里。
//!
//! ## 每个方法都等到结果才返回
//!
//! TDLib 是异步的，本 crate 把等待**收进方法里**：调用方拿到的是结果，不是请求号。
//!
//! - **发送类**（`send_message` / `send_message_album` / `send_rich_message`）：
//!   先等响应拿到临时消息，再等那条消息的**结局**——`updateMessageSendSucceeded`
//!   给最终消息，`updateMessageSendFailed` 变成 [`SimplifyError::SendFailed`]。
//!   超时则先 `deleteMessages(revoke = true)` 撤回还没发出的消息，再返回
//!   [`SimplifyError::Timeout`]。
//! - **其余方法**（`edit_*` / `delete_message` / `answer_callback_query`）：结果就是
//!   **响应**（事件类型 [`RESPONSE_EVENT`]，只发给发起请求的那个插件）——`Message`
//!   或 `ok` 是成功，`@type == "error"` 变成 [`SimplifyError::Tdlib`]。
//! - ⚠️ 它们是**阻塞**的，而插件的更新回调就跑在接收循环那个线程上——在那里等
//!   就是自己卡自己。回调里用 `*_then`：结果走闭包，不阻塞。
//!   （`*_then` 返回线程句柄，**插件要在 `stop()` 里 join 掉它**：动态库卸载之后
//!   线程还在跑库里的代码是未定义行为。）
//! - 底层出口 `wait_response` / `wait_for_update` 仍然公开：想自己接原始 JSON 就用它们。
//!   [`WaitHandle::payload_json`](fuyubot_abi::WaitHandle) 是宿主临时的线程局部缓冲，
//!   **只在下一次宿主调用之前有效**，要拿数据就立刻读（或用 `*_json` / `wait_response`，
//!   它们已经替你拷好了）。
//!
//! ## 身份、管理权、转发、反应、连接状态
//!
//! 这几样在 TDLib 里都「没有直觉上那个方法」，所以单独封一层：
//!
//! - **管理权**：`user` 上连 `is_bot` 都没有（身份在 `user.type`），管理权在
//!   `getChatMember` 的 `ChatMember.status` 里。[`Simplify::is_chat_admin`] /
//!   [`Simplify::am_i_chat_admin`] 直接返回 **`bool`**：`right: Option<AdminRight>`
//!   为 `None` 只看身份，`Some(…)` 还要求 `chatAdministratorRights` 里那一项为真；
//!   查不到（超时 / TDLib 报错 / 不是群聊）一律 `false`。
//! - **转发**：[`Simplify::forward_messages`] 走 `forwardMessages`（一次一组，
//!   不是 Bot API 的 `forwardMessage`），`message_ids` 会被排序去重。
//! - **表情反应**：bot 与用户号**不是同一个方法**——用户用 `addMessageReaction`
//!   （追加一个），bot 只能用 `setMessageReactions`（整组覆盖，空列表 = 清空）。
//!   [`Simplify::react`] / [`Simplify::unreact`] 先看 `getMe` 再挑，不用调用方操心。
//! - **连接状态**：TDLib 没有 `getConnectionState`，连接状态只以
//!   `updateConnectionState` 的形式出现。[`Simplify::track_connection_state`]
//!   订阅并记住最新那条（守卫 drop 即退订），之后 [`ConnectionTracker::is_ready`]
//!   随时可读、不发请求。
//!
//! ## 类型提示
//!
//! TDLib 的请求/更新类型（编译期从 `td_api.tl` 生成）经 [`td_api`] 重导：
//! 调用方拿到 `Value` 之后可以直接 `serde_json::from_value::<td_api::Message>(…)`，
//! 有完整的字段文档与编辑器补全，不必自己拼裸 JSON 结构。

use std::ffi::{CStr, c_char, c_void};
use std::time::{Duration, Instant};

use fuyubot_abi::{
    FILTER_EVENT_TYPE_KEY, HostApi, RESPONSE_EVENT, RequestId, SubscriptionHandle, UPDATE_EVENT,
    WaitHandle,
};
use serde_json::{Map, Value, json};

/// TDLib 生成类型（`td_api`）与可校验元数据（`schema`）的家在 `fuyubot-tdlib-sys`；
/// 这里重导 `td_api`，让只依赖本 crate 的调用方也有类型提示。
pub use fuyubot_tdlib_sys::td_api;

mod chat;
mod connection;
pub mod ops;
mod rich;

pub use ops::{OpError, is_sync_op, known_ops, run, to_result_json};

pub use chat::{
    AdminRight, ForwardParams, MAX_FORWARD_MESSAGES, Reaction, chat_member_params, forward_params,
    is_admin_status, reaction_params, remove_reaction_params, set_reactions_params,
};
pub use connection::{
    CONNECTION_UPDATE, ConnectionState, ConnectionTracker, track_connection_state,
};
pub use rich::{
    RichButton, RichButtonAction, RichButtonAlign, RichButtonStyle, RichMedia, RichMessage,
    RichMessageParams, RichSource, markdown, rich_content_json, rich_edit_params, rich_send_params,
    richtext,
};

/// 封装层的错误。
#[derive(Debug, thiserror::Error)]
pub enum SimplifyError {
    #[error(transparent)]
    Abi(#[from] fuyubot_abi::AbiError),
    #[error("参数串里含 NUL 字节")]
    BadString,
    #[error("相册只能放 2–10 项，收到 {0} 项")]
    AlbumSize(usize),
    #[error("转发列表是空的")]
    EmptyForward,
    #[error("一次最多转发 {MAX_FORWARD_MESSAGES} 条，收到 {0} 条")]
    TooManyForward(usize),
    #[error("富消息不能为空（blocks 或源文本至少要有内容）")]
    EmptyRichMessage,
    #[error("块 / 按钮行只能挂在 Blocks 源上：md / html 源由服务端解析，本地插不进块")]
    RichSourceNotBlocks,
    #[error("按钮行不能为空")]
    EmptyButtonRow,
    /// 发是发出去了，TDLib 说没发成（`updateMessageSendFailed`）。
    #[error("发送失败（TDLib {code}）：{message}")]
    SendFailed { code: i32, message: String },
    /// 方法本身被 TDLib 拒绝了（响应是 `@type == "error"`）。
    #[error("TDLib 返回错误（{code}）：{message}")]
    Tdlib { code: i32, message: String },
    /// 在 `timeout_ms` 里没等到结局。**已经尝试撤回**那条还没发出去的消息。
    #[error("等了 {0} ms 没等到结果，已尝试撤回未发出的消息")]
    Timeout(u64),
}

type Result<T> = std::result::Result<T, SimplifyError>;

/// 等待的默认上限（各参数结构体的 `timeout_ms` 初值）。
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// 一条 `@type == "error"` 的响应 → 错误（`code` / `message` 都在顶层）。
fn as_tdlib_error(error: &Value) -> SimplifyError {
    SimplifyError::Tdlib {
        code: error.get("code").and_then(Value::as_i64).unwrap_or(0) as i32,
        message: error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    }
}

/// 发送失败的更新（`updateMessageSendFailed`）→ 错误。
fn send_failed_of(update: &Value) -> Option<SimplifyError> {
    if update.get("@type").and_then(Value::as_str) != Some("updateMessageSendFailed") {
        return None;
    }
    Some(SimplifyError::SendFailed {
        code: update
            .get("error_code")
            .and_then(Value::as_i64)
            .unwrap_or(0) as i32,
        message: update
            .get("error_message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    })
}

// ── 文本 ──────────────────────────────────────────────────────────

/// 文本解析模式。
///
/// 直接映射到 TDLib 的 `TextParseMode`：带标记的文本（Markdown / MarkdownV2 / HTML）
/// 经 `parseTextEntities` 换成 `formattedText` 的 `entities`。**解析不在本 crate 里做**——
/// 没有第二套 Markdown 实现，也就不存在「我们认的语法」和「Telegram 认的语法」漂移这回事。
///
/// 代价是一次同步往返（`td_execute`，见 [`Simplify::formatted`]）：不碰服务器的方法
/// 才能这么调，所以它在事件回调里也安全。宿主没接 TDLib 时退回纯文本。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ParseMode {
    /// 纯文本：不花往返，`entities` 为空。
    #[default]
    Plain,
    Markdown,
    MarkdownV2,
    Html,
}

impl ParseMode {
    /// 转成 TDLib 的 `TextParseMode`；`Plain` 没有对应的构造器（不出请求）。
    fn to_td(self) -> Option<Value> {
        match self {
            Self::Plain => None,
            // 真 `td_api.tl`：`textParseModeMarkdown version:int32`，1 = Markdown，2 = MarkdownV2
            Self::Markdown => Some(json!({ "@type": "textParseModeMarkdown", "version": 1 })),
            Self::MarkdownV2 => Some(json!({ "@type": "textParseModeMarkdown", "version": 2 })),
            Self::Html => Some(json!({ "@type": "textParseModeHTML" })),
        }
    }
}

/// 一段文本 + 它的解析模式。
///
/// 纯文本走 [`Text::plain`]（或 `From<&str>`），带标记就走 [`Text::markdown`] /
/// [`Text::markdown_v2`] / [`Text::html`]。想显式挑模式用 [`Text::new`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Text {
    pub text: String,
    pub parse_mode: ParseMode,
}

impl Text {
    pub fn new(text: impl Into<String>, parse_mode: ParseMode) -> Self {
        Self {
            text: text.into(),
            parse_mode,
        }
    }

    pub fn plain(text: impl Into<String>) -> Self {
        Self::new(text, ParseMode::Plain)
    }

    /// Telegram 的经典 Markdown（`*bold*` / `_italic_` / `` `code` ``）。
    pub fn markdown(text: impl Into<String>) -> Self {
        Self::new(text, ParseMode::Markdown)
    }

    /// MarkdownV2（也是各家 Bot API 的默认版本）。
    ///
    /// ⚠️ **真库实测（TDLib 1.8.68，1.8.66 同结论）：粗体是 `*x*`，不是 `**x**`**。`**x**` 会被吞成
    /// `x` 而且**不给** bold entity，`__x__` 出来的是下划线。别按 CommonMark 的直觉写——
    /// 语法以 TDLib 为准（`crates/fuyubot-tdlib/tests/integration.rs` 里那条
    /// `execute_runs_synchronous_methods` 钉着这件事）。
    pub fn markdown_v2(text: impl Into<String>) -> Self {
        Self::new(text, ParseMode::MarkdownV2)
    }

    pub fn html(text: impl Into<String>) -> Self {
        Self::new(text, ParseMode::Html)
    }
}

impl From<&str> for Text {
    fn from(text: &str) -> Self {
        Self::plain(text)
    }
}

impl From<String> for Text {
    fn from(text: String) -> Self {
        Self::plain(text)
    }
}

/// 把一段文本当成**纯文本**包成 `formattedText`（不做任何解析，不花往返）。
///
/// 它也是 [`content_json`] / [`send_params`] 的默认渲染器：
/// 需要 Markdown 就传 [`Simplify::formatted`]。
pub fn plain_text(text: &Text) -> Value {
    json!({ "@type": "formattedText", "text": text.text, "entities": [] })
}

/// `parseTextEntities` 的请求 JSON（**唯一的** Markdown/HTML 解析入口）。
///
/// 真 `td_api.tl`：`parseTextEntities text:string parse_mode:TextParseMode = FormattedText`。
/// 纯文本没有对应的 `TextParseMode`，所以返回 `None`——别为它白跑一趟。
pub fn parse_text_request(text: &Text) -> Option<Value> {
    let parse_mode = text.parse_mode.to_td()?;
    Some(json!({
        "@type": td_api::ParseTextEntities::METHOD,
        "text": text.text,
        "parse_mode": parse_mode,
    }))
}

/// TDLib 对 `parseTextEntities` 的响应 → `formattedText`。
///
/// 拿不到（没接 TDLib / 超时 / TDLib 报错 / 回了个别的东西）就**退回纯文本**：
/// 一条 Markdown 写错的文本不该让整条消息发不出去。
pub fn formatted_from(text: &Text, response: Option<&Value>) -> Value {
    match response {
        Some(value) if is_formatted_text(Some(value)) => value.clone(),
        _ => plain_text(text),
    }
}

/// TDLib 真的把文本解析成了 `formattedText` 吗（没接 TDLib / 报错都是 `false`）。
pub fn is_formatted_text(response: Option<&Value>) -> bool {
    let Some(value) = response else {
        return false;
    };
    value.get("@type").and_then(Value::as_str) == Some("formattedText")
}

// ── 媒体 ──────────────────────────────────────────────────────────

/// 要发的文件（对方的 `toTdInputFile`）。
///
/// 本地路径走 `inputFileLocal`，already-uploaded 的文件 id 走 `inputFileRemote`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputFile {
    Local { path: String },
    Remote { id: String },
}

impl InputFile {
    pub fn local(path: impl Into<String>) -> Self {
        Self::Local { path: path.into() }
    }

    pub fn remote(id: impl Into<String>) -> Self {
        Self::Remote { id: id.into() }
    }

    fn to_json(&self) -> Value {
        match self {
            Self::Local { path } => json!({ "@type": "inputFileLocal", "path": path }),
            Self::Remote { id } => json!({ "@type": "inputFileRemote", "id": id }),
        }
    }
}

/// 字符串一律当成**本地路径**（最常用的那种）。
impl From<&str> for InputFile {
    fn from(path: &str) -> Self {
        Self::local(path)
    }
}

impl From<String> for InputFile {
    fn from(path: String) -> Self {
        Self::local(path)
    }
}

/// 一张缩略图 / 封面（`inputThumbnail`）。
///
/// 真 `td_api.tl`：`inputThumbnail thumbnail:InputFile width:int32 height:int32 = InputThumbnail`。
/// 宽高填 `0` 让 TDLib 自己判断。没设置（[`None`]）时**整个字段不发**——
/// 真库文档写的是「pass null to skip thumbnail uploading」，省略同样是「没传」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    pub file: InputFile,
    pub width: i32,
    pub height: i32,
}

impl Thumbnail {
    /// 一张缩略图；尺寸交给 TDLib 自己判断。
    pub fn new(file: impl Into<InputFile>) -> Self {
        Self {
            file: file.into(),
            width: 0,
            height: 0,
        }
    }

    pub fn size(self, width: i32, height: i32) -> Self {
        Self {
            width,
            height,
            ..self
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "@type": "inputThumbnail",
            "thumbnail": self.file.to_json(),
            "width": self.width,
            "height": self.height,
        })
    }
}

impl From<&str> for Thumbnail {
    fn from(path: &str) -> Self {
        Self::new(path)
    }
}

impl From<String> for Thumbnail {
    fn from(path: String) -> Self {
        Self::new(path)
    }
}

impl From<InputFile> for Thumbnail {
    fn from(file: InputFile) -> Self {
        Self::new(file)
    }
}

/// 媒体内容（对应 TDLib 的各个 `inputMessage*` 构造器）。
///
/// 尺寸/时长这些填 `0` 也没关系（TDLib 会自己读真实值），要精确控制再填。
/// `thumbnail` 同理：不给就不发这个字段（TDLib 自己抽，或者干脆没有）。
#[derive(Debug, Clone, PartialEq)]
pub enum Media {
    Photo {
        file: InputFile,
        caption: Option<Text>,
        thumbnail: Option<Thumbnail>,
        width: i32,
        height: i32,
        has_spoiler: bool,
    },
    Video {
        file: InputFile,
        caption: Option<Text>,
        thumbnail: Option<Thumbnail>,
        /// 视频封面（那一帧的画面文件）。⚠️ 秘密聊天和阅后即焚消息不支持。
        cover: Option<InputFile>,
        /// 从视频的第几秒开始播。
        start_timestamp: Option<i32>,
        duration: i32,
        width: i32,
        height: i32,
        supports_streaming: bool,
        has_spoiler: bool,
    },
    Audio {
        file: InputFile,
        caption: Option<Text>,
        /// 专辑封面（真 `inputAudio` 里叫 `album_cover_thumbnail`）。
        album_cover_thumbnail: Option<Thumbnail>,
        duration: i32,
        title: String,
        performer: String,
    },
    Document {
        file: InputFile,
        caption: Option<Text>,
        thumbnail: Option<Thumbnail>,
    },
    Animation {
        file: InputFile,
        caption: Option<Text>,
        thumbnail: Option<Thumbnail>,
        duration: i32,
        width: i32,
        height: i32,
        has_spoiler: bool,
    },
    Sticker {
        file: InputFile,
        emoji: String,
        thumbnail: Option<Thumbnail>,
        width: i32,
        height: i32,
    },
    Location {
        latitude: f64,
        longitude: f64,
    },
}

impl Media {
    /// 一张图（尺寸交给 TDLib 自己判断）。
    pub fn photo(file: impl Into<InputFile>) -> Self {
        Self::Photo {
            file: file.into(),
            caption: None,
            thumbnail: None,
            width: 0,
            height: 0,
            has_spoiler: false,
        }
    }

    /// 一段视频（尺寸/时长交给 TDLib 自己判断）。
    pub fn video(file: impl Into<InputFile>) -> Self {
        Self::Video {
            file: file.into(),
            caption: None,
            thumbnail: None,
            cover: None,
            start_timestamp: None,
            duration: 0,
            width: 0,
            height: 0,
            supports_streaming: false,
            has_spoiler: false,
        }
    }

    /// 一个文件（文档）。
    pub fn document(file: impl Into<InputFile>) -> Self {
        Self::Document {
            file: file.into(),
            caption: None,
            thumbnail: None,
        }
    }

    pub fn location(latitude: f64, longitude: f64) -> Self {
        Self::Location {
            latitude,
            longitude,
        }
    }

    /// 挂一张缩略图 / 封面（位置没有缩略图，原样返回）。
    pub fn thumbnail(self, thumbnail: impl Into<Thumbnail>) -> Self {
        let thumbnail = Some(thumbnail.into());
        match self {
            Self::Photo {
                file,
                caption,
                width,
                height,
                has_spoiler,
                ..
            } => Self::Photo {
                file,
                caption,
                thumbnail,
                width,
                height,
                has_spoiler,
            },
            Self::Video {
                file,
                caption,
                cover,
                start_timestamp,
                duration,
                width,
                height,
                supports_streaming,
                has_spoiler,
                ..
            } => Self::Video {
                file,
                caption,
                thumbnail,
                cover,
                start_timestamp,
                duration,
                width,
                height,
                supports_streaming,
                has_spoiler,
            },
            Self::Audio {
                file,
                caption,
                duration,
                title,
                performer,
                ..
            } => Self::Audio {
                file,
                caption,
                album_cover_thumbnail: thumbnail,
                duration,
                title,
                performer,
            },
            Self::Document { file, caption, .. } => Self::Document {
                file,
                caption,
                thumbnail,
            },
            Self::Animation {
                file,
                caption,
                duration,
                width,
                height,
                has_spoiler,
                ..
            } => Self::Animation {
                file,
                caption,
                thumbnail,
                duration,
                width,
                height,
                has_spoiler,
            },
            Self::Sticker {
                file,
                emoji,
                width,
                height,
                ..
            } => Self::Sticker {
                file,
                emoji,
                thumbnail,
                width,
                height,
            },
            other => other,
        }
    }

    /// 给视频挑一帧做封面，并指定从第几秒开始播（`cover` / `start_timestamp`）。
    ///
    /// 只对 [`Media::Video`] 有效，别的变体原样返回。
    /// ⚠️ 真库：秘密聊天和阅后即焚消息不支持 `cover`。
    pub fn cover(self, cover: impl Into<InputFile>, start_timestamp: i32) -> Self {
        match self {
            Self::Video {
                file,
                caption,
                thumbnail,
                duration,
                width,
                height,
                supports_streaming,
                has_spoiler,
                ..
            } => Self::Video {
                file,
                caption,
                thumbnail,
                cover: Some(cover.into()),
                start_timestamp: Some(start_timestamp),
                duration,
                width,
                height,
                supports_streaming,
                has_spoiler,
            },
            other => other,
        }
    }

    /// 剧透：发过去先被遮住，点一下才看得见。
    ///
    /// 只对图 / 视频 / 动图有效（真 `td_api.tl` 里只有这三个 `inputMessage*` 有
    /// `has_spoiler`），其余变体原样返回。
    pub fn spoiler(self, has_spoiler: bool) -> Self {
        match self {
            Self::Photo {
                file,
                caption,
                thumbnail,
                width,
                height,
                ..
            } => Self::Photo {
                file,
                caption,
                thumbnail,
                width,
                height,
                has_spoiler,
            },
            Self::Video {
                file,
                caption,
                thumbnail,
                cover,
                start_timestamp,
                duration,
                width,
                height,
                supports_streaming,
                ..
            } => Self::Video {
                file,
                caption,
                thumbnail,
                cover,
                start_timestamp,
                duration,
                width,
                height,
                supports_streaming,
                has_spoiler,
            },
            Self::Animation {
                file,
                caption,
                thumbnail,
                duration,
                width,
                height,
                ..
            } => Self::Animation {
                file,
                caption,
                thumbnail,
                duration,
                width,
                height,
                has_spoiler,
            },
            other => other,
        }
    }

    /// 宽高（图 / 视频 / 动图 / 贴纸）。填 `0` 让 TDLib 自己读真实值。
    pub fn size(self, width: i32, height: i32) -> Self {
        match self {
            Self::Photo {
                file,
                caption,
                thumbnail,
                has_spoiler,
                ..
            } => Self::Photo {
                file,
                caption,
                thumbnail,
                width,
                height,
                has_spoiler,
            },
            Self::Video {
                file,
                caption,
                thumbnail,
                cover,
                start_timestamp,
                duration,
                supports_streaming,
                has_spoiler,
                ..
            } => Self::Video {
                file,
                caption,
                thumbnail,
                cover,
                start_timestamp,
                duration,
                width,
                height,
                supports_streaming,
                has_spoiler,
            },
            Self::Animation {
                file,
                caption,
                thumbnail,
                duration,
                has_spoiler,
                ..
            } => Self::Animation {
                file,
                caption,
                thumbnail,
                duration,
                width,
                height,
                has_spoiler,
            },
            Self::Sticker {
                file,
                emoji,
                thumbnail,
                ..
            } => Self::Sticker {
                file,
                emoji,
                thumbnail,
                width,
                height,
            },
            other => other,
        }
    }

    /// 时长（秒）。图 / 文档 / 贴纸 / 位置没有时长，原样返回。
    pub fn duration(self, duration: i32) -> Self {
        match self {
            Self::Video {
                file,
                caption,
                thumbnail,
                cover,
                start_timestamp,
                width,
                height,
                supports_streaming,
                has_spoiler,
                ..
            } => Self::Video {
                file,
                caption,
                thumbnail,
                cover,
                start_timestamp,
                duration,
                width,
                height,
                supports_streaming,
                has_spoiler,
            },
            Self::Audio {
                file,
                caption,
                album_cover_thumbnail,
                title,
                performer,
                ..
            } => Self::Audio {
                file,
                caption,
                album_cover_thumbnail,
                duration,
                title,
                performer,
            },
            Self::Animation {
                file,
                caption,
                thumbnail,
                width,
                height,
                has_spoiler,
                ..
            } => Self::Animation {
                file,
                caption,
                thumbnail,
                duration,
                width,
                height,
                has_spoiler,
            },
            other => other,
        }
    }

    /// 视频按流式发（不传完就能播）。只对 [`Media::Video`] 有效。
    pub fn streaming(self, supports_streaming: bool) -> Self {
        match self {
            Self::Video {
                file,
                caption,
                thumbnail,
                cover,
                start_timestamp,
                duration,
                width,
                height,
                has_spoiler,
                ..
            } => Self::Video {
                file,
                caption,
                thumbnail,
                cover,
                start_timestamp,
                duration,
                width,
                height,
                supports_streaming,
                has_spoiler,
            },
            other => other,
        }
    }

    /// 音频的曲名。只对 [`Media::Audio`] 有效。
    pub fn title(self, title: impl Into<String>) -> Self {
        match self {
            Self::Audio {
                file,
                caption,
                album_cover_thumbnail,
                duration,
                performer,
                ..
            } => Self::Audio {
                file,
                caption,
                album_cover_thumbnail,
                duration,
                title: title.into(),
                performer,
            },
            other => other,
        }
    }

    /// 音频的演唱者。只对 [`Media::Audio`] 有效。
    pub fn performer(self, performer: impl Into<String>) -> Self {
        match self {
            Self::Audio {
                file,
                caption,
                album_cover_thumbnail,
                duration,
                title,
                ..
            } => Self::Audio {
                file,
                caption,
                album_cover_thumbnail,
                duration,
                title,
                performer: performer.into(),
            },
            other => other,
        }
    }

    /// 贴纸关联的 emoji。只对 [`Media::Sticker`] 有效。
    pub fn emoji(self, emoji: impl Into<String>) -> Self {
        match self {
            Self::Sticker {
                file,
                thumbnail,
                width,
                height,
                ..
            } => Self::Sticker {
                file,
                emoji: emoji.into(),
                thumbnail,
                width,
                height,
            },
            other => other,
        }
    }

    /// 给带标题的媒体挂一段文本（贴纸/位置没有标题，原样返回）。
    pub fn caption(self, text: impl Into<Text>) -> Self {
        let caption = Some(text.into());
        match self {
            Self::Photo {
                file,
                thumbnail,
                width,
                height,
                has_spoiler,
                ..
            } => Self::Photo {
                file,
                caption,
                thumbnail,
                width,
                height,
                has_spoiler,
            },
            Self::Video {
                file,
                thumbnail,
                cover,
                start_timestamp,
                duration,
                width,
                height,
                supports_streaming,
                has_spoiler,
                ..
            } => Self::Video {
                file,
                caption,
                thumbnail,
                cover,
                start_timestamp,
                duration,
                width,
                height,
                supports_streaming,
                has_spoiler,
            },
            Self::Audio {
                file,
                album_cover_thumbnail,
                duration,
                title,
                performer,
                ..
            } => Self::Audio {
                file,
                caption,
                album_cover_thumbnail,
                duration,
                title,
                performer,
            },
            Self::Document {
                file, thumbnail, ..
            } => Self::Document {
                file,
                caption,
                thumbnail,
            },
            Self::Animation {
                file,
                thumbnail,
                duration,
                width,
                height,
                has_spoiler,
                ..
            } => Self::Animation {
                file,
                caption,
                thumbnail,
                duration,
                width,
                height,
                has_spoiler,
            },
            other => other,
        }
    }

    fn caption_ref(&self) -> Option<&Text> {
        match self {
            Self::Photo { caption, .. }
            | Self::Video { caption, .. }
            | Self::Audio { caption, .. }
            | Self::Document { caption, .. }
            | Self::Animation { caption, .. } => caption.as_ref(),
            Self::Sticker { .. } | Self::Location { .. } => None,
        }
    }
}

/// 媒体 → `InputMessageContent`（内容类型的形状**只有这一份实现**）。
///
/// 形状照真 `td_api.tl`：
/// - `inputMessagePhoto photo:inputPhoto caption:formattedText …`，而 `inputPhoto photo:InputFile`
///   ——**`photo` 套了两层**，内层那个才是文件；
/// - `inputMessageVideo` / `inputMessageAudio` / `inputMessageDocument` / `inputMessageAnimation`
///   同一套模式（外层内容 → 中层 `inputX` → 内层 `InputFile`）；
/// - `inputMessageSticker sticker:inputSticker emoji:string`（**没有 caption**）；
/// - **caption 在内容类型里**，不是 `sendMessage` 的顶层参数。
///
/// `render` 决定文本怎么变成 `formattedText`：纯文本用 [`plain_text`]，
/// 要走 TDLib 的 Markdown 解析就传 [`Simplify::formatted`]。
pub fn content_json(media: &Media, render: &dyn Fn(&Text) -> Value) -> Value {
    let caption = |media: &Media| media.caption_ref().map(|text| render(text));
    match media {
        Media::Photo {
            file,
            thumbnail,
            width,
            height,
            has_spoiler,
            ..
        } => {
            let mut object = Map::new();
            object.insert("@type".to_owned(), json!("inputMessagePhoto"));
            let mut inner = Map::new();
            inner.insert("@type".to_owned(), json!("inputPhoto"));
            inner.insert("photo".to_owned(), file.to_json());
            put_some(
                &mut inner,
                "thumbnail",
                thumbnail.as_ref().map(Thumbnail::to_json),
            );
            inner.insert("width".to_owned(), json!(width));
            inner.insert("height".to_owned(), json!(height));
            object.insert("photo".to_owned(), Value::Object(inner));
            if let Some(caption) = caption(media) {
                object.insert("caption".to_owned(), caption);
            }
            object.insert("has_spoiler".to_owned(), json!(has_spoiler));
            Value::Object(object)
        }
        Media::Video {
            file,
            thumbnail,
            cover,
            start_timestamp,
            duration,
            width,
            height,
            supports_streaming,
            has_spoiler,
            ..
        } => {
            let mut object = Map::new();
            object.insert("@type".to_owned(), json!("inputMessageVideo"));
            let mut inner = Map::new();
            inner.insert("@type".to_owned(), json!("inputVideo"));
            inner.insert("video".to_owned(), file.to_json());
            put_some(
                &mut inner,
                "thumbnail",
                thumbnail.as_ref().map(Thumbnail::to_json),
            );
            put_some(&mut inner, "cover", cover.as_ref().map(InputFile::to_json));
            put_some(
                &mut inner,
                "start_timestamp",
                start_timestamp.map(|seconds| json!(seconds)),
            );
            inner.insert("duration".to_owned(), json!(duration));
            inner.insert("width".to_owned(), json!(width));
            inner.insert("height".to_owned(), json!(height));
            inner.insert("supports_streaming".to_owned(), json!(supports_streaming));
            object.insert("video".to_owned(), Value::Object(inner));
            if let Some(caption) = caption(media) {
                object.insert("caption".to_owned(), caption);
            }
            object.insert("has_spoiler".to_owned(), json!(has_spoiler));
            Value::Object(object)
        }
        Media::Audio {
            file,
            album_cover_thumbnail,
            duration,
            title,
            performer,
            ..
        } => {
            let mut object = Map::new();
            object.insert("@type".to_owned(), json!("inputMessageAudio"));
            let mut inner = Map::new();
            inner.insert("@type".to_owned(), json!("inputAudio"));
            inner.insert("audio".to_owned(), file.to_json());
            put_some(
                &mut inner,
                "album_cover_thumbnail",
                album_cover_thumbnail.as_ref().map(Thumbnail::to_json),
            );
            inner.insert("duration".to_owned(), json!(duration));
            inner.insert("title".to_owned(), json!(title));
            inner.insert("performer".to_owned(), json!(performer));
            object.insert("audio".to_owned(), Value::Object(inner));
            if let Some(caption) = caption(media) {
                object.insert("caption".to_owned(), caption);
            }
            Value::Object(object)
        }
        Media::Document {
            file, thumbnail, ..
        } => {
            let mut object = Map::new();
            object.insert("@type".to_owned(), json!("inputMessageDocument"));
            let mut inner = Map::new();
            inner.insert("@type".to_owned(), json!("inputDocument"));
            inner.insert("document".to_owned(), file.to_json());
            put_some(
                &mut inner,
                "thumbnail",
                thumbnail.as_ref().map(Thumbnail::to_json),
            );
            object.insert("document".to_owned(), Value::Object(inner));
            if let Some(caption) = caption(media) {
                object.insert("caption".to_owned(), caption);
            }
            Value::Object(object)
        }
        Media::Animation {
            file,
            thumbnail,
            duration,
            width,
            height,
            has_spoiler,
            ..
        } => {
            let mut object = Map::new();
            object.insert("@type".to_owned(), json!("inputMessageAnimation"));
            let mut inner = Map::new();
            inner.insert("@type".to_owned(), json!("inputAnimation"));
            inner.insert("animation".to_owned(), file.to_json());
            put_some(
                &mut inner,
                "thumbnail",
                thumbnail.as_ref().map(Thumbnail::to_json),
            );
            inner.insert("duration".to_owned(), json!(duration));
            inner.insert("width".to_owned(), json!(width));
            inner.insert("height".to_owned(), json!(height));
            object.insert("animation".to_owned(), Value::Object(inner));
            if let Some(caption) = caption(media) {
                object.insert("caption".to_owned(), caption);
            }
            object.insert("has_spoiler".to_owned(), json!(has_spoiler));
            Value::Object(object)
        }
        Media::Sticker {
            file,
            emoji,
            thumbnail,
            width,
            height,
        } => {
            let mut inner = Map::new();
            inner.insert("@type".to_owned(), json!("inputSticker"));
            inner.insert("sticker".to_owned(), file.to_json());
            put_some(
                &mut inner,
                "thumbnail",
                thumbnail.as_ref().map(Thumbnail::to_json),
            );
            inner.insert("width".to_owned(), json!(width));
            inner.insert("height".to_owned(), json!(height));
            json!({
                "@type": "inputMessageSticker",
                "sticker": Value::Object(inner),
                "emoji": emoji,
            })
        }
        Media::Location {
            latitude,
            longitude,
        } => json!({
            "@type": "inputMessageLocation",
            "location": {
                "@type": "location",
                "latitude": latitude,
                "longitude": longitude,
            },
        }),
    }
}

/// 只在 `Some` 时把键写进去。
///
/// 缩略图 / 封面这些「可空对象字段」没设置就**整个键不发**（不是发 `null`）：
/// 保持和改之前一致的行为，真库那边「没收到」就是跳过。
fn put_some(object: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        object.insert(key.to_owned(), value);
    }
}

// ── 内联键盘 ──────────────────────────────────────────────────────

/// 按钮样式（TDLib 1.8 的 `ButtonStyle`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ButtonStyle {
    #[default]
    Default,
    Primary,
    Danger,
    Success,
}

impl ButtonStyle {
    fn to_json(self) -> Value {
        let name = match self {
            Self::Default => "buttonStyleDefault",
            Self::Primary => "buttonStylePrimary",
            Self::Danger => "buttonStyleDanger",
            Self::Success => "buttonStyleSuccess",
        };
        json!({ "@type": name })
    }
}

/// 按钮的行为。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ButtonAction {
    /// 点下去回一条 callback query 给机器人（`data` 会被 base64 编码）。
    Callback { data: String },
    /// 打开链接（用户账号不能发，TDLib 会拒绝）。
    Url { url: String },
    /// 打开某个用户的主页。
    User { user_id: i64 },
    /// 打开 Web App。
    WebApp { url: String },
}

/// 一个内联键盘按钮。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyButton {
    pub text: String,
    pub action: ButtonAction,
    /// 文案前面那个自定义 emoji 的 id（`"0"` = 没有，TDLib 的约定）。
    pub emoji_id: Option<i64>,
    pub style: ButtonStyle,
}

impl ReplyButton {
    pub fn callback(text: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            action: ButtonAction::Callback { data: data.into() },
            emoji_id: None,
            style: ButtonStyle::Default,
        }
    }

    pub fn url(text: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            action: ButtonAction::Url { url: url.into() },
            emoji_id: None,
            style: ButtonStyle::Default,
        }
    }

    pub fn user(text: impl Into<String>, user_id: i64) -> Self {
        Self {
            text: text.into(),
            action: ButtonAction::User { user_id },
            emoji_id: None,
            style: ButtonStyle::Default,
        }
    }

    pub fn web_app(text: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            action: ButtonAction::WebApp { url: url.into() },
            emoji_id: None,
            style: ButtonStyle::Default,
        }
    }

    /// 挂一个自定义 emoji（按钮文案前面）。
    pub fn emoji(mut self, custom_emoji_id: i64) -> Self {
        self.emoji_id = Some(custom_emoji_id);
        self
    }

    pub fn style(mut self, style: ButtonStyle) -> Self {
        self.style = style;
        self
    }

    fn to_json(&self) -> Value {
        let kind = match &self.action {
            // 真 `td_api.tl`：`inlineKeyboardButtonTypeCallback data:bytes`（JSON 里是 base64）
            ButtonAction::Callback { data } => {
                json!({ "@type": "inlineKeyboardButtonTypeCallback", "data": base64(data) })
            }
            ButtonAction::Url { url } => {
                json!({ "@type": "inlineKeyboardButtonTypeUrl", "url": url })
            }
            ButtonAction::User { user_id } => {
                json!({ "@type": "inlineKeyboardButtonTypeUser", "user_id": user_id })
            }
            ButtonAction::WebApp { url } => {
                json!({ "@type": "inlineKeyboardButtonTypeWebApp", "url": url })
            }
        };
        json!({
            "@type": "inlineKeyboardButton",
            "text": &self.text,
            "icon_custom_emoji_id": self.emoji_id.unwrap_or(0),
            "style": self.style.to_json(),
            "type": kind,
        })
    }
}

/// 二维按钮 → `replyMarkupInlineKeyboard`；空数组就是「不要键盘」。
fn reply_markup(rows: &[Vec<ReplyButton>]) -> Option<Value> {
    if rows.iter().all(|row| row.is_empty()) {
        return None;
    }
    Some(json!({
        "@type": "replyMarkupInlineKeyboard",
        "rows": rows
            .iter()
            .map(|row| Value::Array(row.iter().map(ReplyButton::to_json).collect()))
            .collect::<Vec<_>>(),
    }))
}

/// TDLib 的 callback data 是 **base64** 编码的字节串。
fn base64(data: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = data.as_bytes();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

// ── 话题 ──────────────────────────────────────────────────────────

/// 消息落在哪个「话题」：TDLib 的 `MessageTopic`（四个构造器，各有各的 id 类型）。
///
/// 这四个**不是一回事**，选错了 TDLib 会拒绝：
///
/// | 变体 | 用在哪 | TDLib 构造器 |
/// | --- | --- | --- |
/// | [`Forum`](Self::Forum) | **论坛**群组（以及与 bot 的私聊）里的话题 | `messageTopicForum { forum_topic_id: int32 }` |
/// | [`Thread`](Self::Thread) | **非论坛**群组的评论串 | `messageTopicThread { message_thread_id: int53 }` |
/// | [`DirectMessages`](Self::DirectMessages) | 频道的「私信话题」 | `messageTopicDirectMessages` |
/// | [`SavedMessages`](Self::SavedMessages) | 收藏夹里的话题 | `messageTopicSavedMessages` |
///
/// ⚠️ 这里刻意**不给** `From<i64>`：论坛话题是 `int32`、评论串是 `int53`，
/// 一个裸数字说不清是哪一种——把它隐式转成其中任何一个，都会在另一种群里静默发错。
/// （这里以前就是固定按 `messageTopicThread` 拼的：非论坛群以外全错。）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Topic {
    /// 论坛群组 / 与 bot 的私聊里的话题（`forum_topic_id`）。
    Forum(i32),
    /// **非论坛**群组的评论串（`message_thread_id`）。
    Thread(i64),
    /// 频道私信话题（`direct_messages_chat_topic_id`）。
    DirectMessages(i64),
    /// 收藏夹话题（`saved_messages_topic_id`）。
    SavedMessages(i64),
}

impl Topic {
    /// 论坛群组（或与 bot 的私聊）里的话题。
    pub fn forum(forum_topic_id: i32) -> Self {
        Self::Forum(forum_topic_id)
    }

    /// **非论坛**群组里的评论串。
    pub fn thread(message_thread_id: i64) -> Self {
        Self::Thread(message_thread_id)
    }

    /// 频道的私信话题。
    pub fn direct_messages(direct_messages_chat_topic_id: i64) -> Self {
        Self::DirectMessages(direct_messages_chat_topic_id)
    }

    /// 收藏夹里的话题。
    pub fn saved_messages(saved_messages_topic_id: i64) -> Self {
        Self::SavedMessages(saved_messages_topic_id)
    }

    /// TDLib 的 `MessageTopic`。
    pub fn to_json(&self) -> Value {
        match self {
            Self::Forum(id) => json!({ "@type": "messageTopicForum", "forum_topic_id": id }),
            Self::Thread(id) => json!({ "@type": "messageTopicThread", "message_thread_id": id }),
            Self::DirectMessages(id) => json!({
                "@type": "messageTopicDirectMessages",
                "direct_messages_chat_topic_id": id,
            }),
            Self::SavedMessages(id) => json!({
                "@type": "messageTopicSavedMessages",
                "saved_messages_topic_id": id,
            }),
        }
    }
}

// ── 参数结构体（一个方法一个，字段用短名字） ──────────────────────

/// `sendMessage` 的参数。
///
/// `text` 与 `media` 二选一：都填就是「媒体 + 标题」，只填 `text` 就是纯文本。
#[derive(Debug, Clone)]
pub struct SendMessageParams {
    /// 文本（或媒体标题）。想发 Markdown 就 [`Text::markdown`]。
    pub text: Option<Text>,
    /// 媒体内容。填了就把 `text` 当标题用。
    pub media: Option<Media>,
    /// 回复哪条消息（TDLib 的 `reply_to`）。
    pub reply_to_message_id: Option<i64>,
    /// 发到哪个话题（TDLib 的 `topic_id`）。**不是** Bot API 的 `message_thread_id`——
    /// 论坛话题是 [`Topic::forum`]，非论坛群组的评论串才是 [`Topic::thread`]。
    pub topic_id: Option<Topic>,
    /// 内联键盘（空数组 = 不带）。
    pub reply_markup: Vec<Vec<ReplyButton>>,
    /// 链接预览：`Some(false)` = 关掉；`Some(true)` / `None` = 交给 TDLib 默认（显示）。
    pub link_preview: Option<bool>,
    /// 等最终消息时的上限（[`Simplify::send_message_wait_success`] 用）。
    pub timeout_ms: u64,
}

impl SendMessageParams {
    pub fn text(text: impl Into<Text>) -> Self {
        Self {
            text: Some(text.into()),
            ..Self::default()
        }
    }

    pub fn media(media: impl Into<Media>) -> Self {
        Self {
            media: Some(media.into()),
            ..Self::default()
        }
    }
}

impl Default for SendMessageParams {
    fn default() -> Self {
        Self {
            text: None,
            media: None,
            reply_to_message_id: None,
            topic_id: None,
            reply_markup: Vec::new(),
            link_preview: None,
            timeout_ms: 30_000,
        }
    }
}

/// `sendMessageAlbum` 的参数（2–10 项，只有音频/文档/图片/视频能组成相册）。
///
/// ⚠️ **相册没有 `reply_markup`**：真签名是
/// `sendMessageAlbum chat_id topic_id reply_to options input_message_contents`
/// ——想给相册挂键盘只能 `editMessageReplyMarkup`，不是发的时候带。
#[derive(Debug, Clone)]
pub struct SendAlbumParams {
    pub medias: Vec<Media>,
    /// 整组共用一段标题。
    pub caption: Option<Text>,
    pub reply_to_message_id: Option<i64>,
    /// 发到哪个话题（见 [`Topic`]）。
    pub topic_id: Option<Topic>,
    /// 整组**共用**的等待预算（[`Simplify::send_message_album_wait_success`] 用）：
    /// 先等 `Messages` 响应、再等 N 条 `updateMessageSendSucceeded`，全程只有这一个预算，
    /// 不是每条各等一次。
    pub timeout_ms: u64,
}

impl Default for SendAlbumParams {
    fn default() -> Self {
        Self {
            medias: Vec::new(),
            caption: None,
            reply_to_message_id: None,
            topic_id: None,
            timeout_ms: 30_000,
        }
    }
}

/// `editMessageText` 的参数（`text` 为 `None` 时只改键盘）。
#[derive(Debug, Clone)]
pub struct EditTextParams {
    pub text: Option<Text>,
    pub link_preview: Option<bool>,
    pub reply_markup: Vec<Vec<ReplyButton>>,
    /// 等 TDLib 响应的上限。
    pub timeout_ms: u64,
}

impl Default for EditTextParams {
    fn default() -> Self {
        Self {
            text: None,
            link_preview: None,
            reply_markup: Vec::new(),
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }
}

/// `editMessageCaption` 的参数。
#[derive(Debug, Clone)]
pub struct EditCaptionParams {
    pub text: Option<Text>,
    pub reply_markup: Vec<Vec<ReplyButton>>,
    pub timeout_ms: u64,
}

impl Default for EditCaptionParams {
    fn default() -> Self {
        Self {
            text: None,
            reply_markup: Vec::new(),
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }
}

/// `editMessageMedia` 的参数（`text` 是新媒体内容的标题）。
#[derive(Debug, Clone)]
pub struct EditMediaParams {
    pub text: Option<Text>,
    pub media: Option<Media>,
    pub reply_markup: Vec<Vec<ReplyButton>>,
    pub timeout_ms: u64,
}

impl Default for EditMediaParams {
    fn default() -> Self {
        Self {
            text: None,
            media: None,
            reply_markup: Vec::new(),
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }
}

// ── 请求组装 ──────────────────────────────────────────────────────

/// 组装 `sendMessage` 的参数（不含 `@type`，那是 [`HostApi::td_invoke_simplified`] 补的）。
///
/// 参数名以真 `td_api.tl` 为准（`sendMessage chat_id topic_id reply_to options
/// reply_markup input_message_content`）：
/// - 话题是 **`topic_id`**，但它是个 `MessageTopic`——**四个构造器选哪个由
///   [`Topic`] 决定**（论坛话题是 `messageTopicForum`，不是 `messageTopicThread`）；
/// - 回复是 **`reply_to`**（`inputMessageReplyToMessage`）；
/// - **没有顶层 `caption`**，它在内容类型里（见 [`content_json`]）。
///
/// 这一版修掉的是一次真缺陷：上面几条以前全按 **Bot API** 的样子拼
/// （`message_thread_id` / `reply_to_message_id` / 顶层 `caption`），TDLib 一个都不认。
/// 后来又发现 `topic_id` 固定拼 `messageTopicThread` 也是错的——那是**非论坛**群组的
/// 评论串，论坛话题得用 `messageTopicForum`，所以改成了 [`Topic`]。
/// `every_request_simplify_builds_passes_the_real_schema` 会挡住再犯。
pub fn send_params(
    chat_id: i64,
    params: &SendMessageParams,
    render: &dyn Fn(&Text) -> Value,
) -> Value {
    let mut payload = Map::new();
    payload.insert("chat_id".to_owned(), json!(chat_id));
    payload.insert(
        "input_message_content".to_owned(),
        message_content(params, render),
    );
    put_common(&mut payload, &params.topic_id, &params.reply_to_message_id);
    if let Some(markup) = reply_markup(&params.reply_markup) {
        payload.insert("reply_markup".to_owned(), markup);
    }
    Value::Object(payload)
}

/// 文本 + 媒体 → `InputMessageContent`（两者都空时按空文本处理，TDLib 会拒绝）。
fn message_content(params: &SendMessageParams, render: &dyn Fn(&Text) -> Value) -> Value {
    match &params.media {
        Some(media) => content_json(&media_with_caption(media, params.text.as_ref()), render),
        None => {
            let mut object = Map::new();
            object.insert("@type".to_owned(), json!("inputMessageText"));
            object.insert(
                "text".to_owned(),
                render(params.text.as_ref().unwrap_or(&Text::plain(""))),
            );
            if params.link_preview == Some(false) {
                object.insert(
                    "link_preview_options".to_owned(),
                    json!({ "@type": "linkPreviewOptions", "is_disabled": true }),
                );
            }
            Value::Object(object)
        }
    }
}

/// 媒体的标题：`params.text` 优先，其次是媒体自带的 `caption`。
fn media_with_caption(media: &Media, text: Option<&Text>) -> Media {
    match text {
        Some(text) => media.clone().caption(text.clone()),
        None => media.clone(),
    }
}

/// `topic_id` / `reply_to` 两个共用的可选参数。
fn put_common(payload: &mut Map<String, Value>, topic_id: &Option<Topic>, reply_to: &Option<i64>) {
    if let Some(topic) = topic_id {
        payload.insert("topic_id".to_owned(), topic.to_json());
    }
    if let Some(message) = reply_to {
        payload.insert(
            "reply_to".to_owned(),
            json!({ "@type": "inputMessageReplyToMessage", "message_id": message }),
        );
    }
}

/// 组装 `sendMessageAlbum` 的参数。
pub fn album_params(
    chat_id: i64,
    params: &SendAlbumParams,
    render: &dyn Fn(&Text) -> Value,
) -> Value {
    let contents: Vec<Value> = params
        .medias
        .iter()
        .map(|media| {
            let media = match &params.caption {
                Some(caption) => media.clone().caption(caption.clone()),
                None => media.clone(),
            };
            content_json(&media, render)
        })
        .collect();

    let mut payload = Map::new();
    payload.insert("chat_id".to_owned(), json!(chat_id));
    put_common(&mut payload, &params.topic_id, &params.reply_to_message_id);
    payload.insert("input_message_contents".to_owned(), Value::Array(contents));
    Value::Object(payload)
}

/// 三个 `editMessage*` 共用的头：`chat_id` + `message_id` + 可选的 `reply_markup`。
fn edit_params(
    chat_id: i64,
    message_id: i64,
    reply_markup_rows: &[Vec<ReplyButton>],
) -> Map<String, Value> {
    let mut payload = Map::new();
    payload.insert("chat_id".to_owned(), json!(chat_id));
    payload.insert("message_id".to_owned(), json!(message_id));
    if let Some(markup) = reply_markup(reply_markup_rows) {
        payload.insert("reply_markup".to_owned(), markup);
    }
    payload
}

/// `editMessageText` 的参数（`@type` 由宿主补）。
pub fn edit_text_params(
    chat_id: i64,
    message_id: i64,
    params: &EditTextParams,
    render: &dyn Fn(&Text) -> Value,
) -> Value {
    let mut payload = edit_params(chat_id, message_id, &params.reply_markup);
    if let Some(text) = &params.text {
        let mut content = Map::new();
        content.insert("@type".to_owned(), json!("inputMessageText"));
        content.insert("text".to_owned(), render(text));
        if params.link_preview == Some(false) {
            content.insert(
                "link_preview_options".to_owned(),
                json!({ "@type": "linkPreviewOptions", "is_disabled": true }),
            );
        }
        payload.insert("input_message_content".to_owned(), Value::Object(content));
    }
    Value::Object(payload)
}

/// `editMessageCaption` 的参数。标题在**顶层** `caption`，不在 `input_message_content` 里。
pub fn edit_caption_params(
    chat_id: i64,
    message_id: i64,
    params: &EditCaptionParams,
    render: &dyn Fn(&Text) -> Value,
) -> Value {
    let mut payload = edit_params(chat_id, message_id, &params.reply_markup);
    if let Some(text) = &params.text {
        payload.insert("caption".to_owned(), render(text));
    }
    Value::Object(payload)
}

/// `editMessageReplyMarkup` 的参数。
///
/// 与另外三个 `editMessage*` 不一样：这里的 `reply_markup` 是**必需**字段
/// （真签名 `editMessageReplyMarkup chat_id message_id reply_markup`），
/// 所以空键盘按「移除键盘」发（`replyMarkupRemoveKeyboard`），不是省略这个字段。
pub fn edit_markup_params(chat_id: i64, message_id: i64, rows: &[Vec<ReplyButton>]) -> Value {
    let mut payload = edit_params(chat_id, message_id, rows);
    payload
        .entry("reply_markup")
        .or_insert_with(|| json!({ "@type": "replyMarkupRemoveKeyboard" }));
    Value::Object(payload)
}

/// `editMessageMedia` 的参数。
pub fn edit_media_params(
    chat_id: i64,
    message_id: i64,
    params: &EditMediaParams,
    render: &dyn Fn(&Text) -> Value,
) -> Value {
    let mut payload = edit_params(chat_id, message_id, &params.reply_markup);
    if let Some(media) = &params.media {
        let media = media_with_caption(media, params.text.as_ref());
        payload.insert(
            "input_message_content".to_owned(),
            content_json(&media, render),
        );
    }
    Value::Object(payload)
}

// ── 等更新的过滤器 ────────────────────────────────────────────────

/// 要解析的实体。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntityId {
    Chat(i64),
    User(i64),
}

impl EntityId {
    fn method(&self) -> &'static str {
        match self {
            Self::Chat(_) => "getChat",
            Self::User(_) => "getUser",
        }
    }

    fn params(&self) -> Value {
        match self {
            Self::Chat(id) => json!({ "chat_id": id }),
            Self::User(id) => json!({ "user_id": id }),
        }
    }

    pub fn to_json(&self) -> Value {
        match self {
            Self::Chat(id) => json!({ "kind": "chat", "id": id }),
            Self::User(id) => json!({ "kind": "user", "id": id }),
        }
    }
}

/// 等更新的过滤器。
///
/// TDLib 的字段位置随更新类型变：`updateNewMessage` 这类把消息包在 `message` 里
/// （所以 `chat_id` 是 `message.chat_id`），而 `updateNewCallbackQuery` 这类把
/// `chat_id` 放在顶层。用 [`UpdateFilter::flat`] 切换后者。
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateFilter {
    update_type: Option<String>,
    chat_id: Option<i64>,
    sender_user_id: Option<i64>,
    message_thread_id: Option<i64>,
    flat: bool,
}

impl UpdateFilter {
    /// 按「消息包在 `message` 里」的更新类型过滤，例如 `updateNewMessage`。
    pub fn new(update_type: impl Into<String>) -> Self {
        Self {
            update_type: Some(update_type.into()),
            chat_id: None,
            sender_user_id: None,
            message_thread_id: None,
            flat: false,
        }
    }

    /// 字段在顶层的更新类型，例如 `updateNewCallbackQuery`。
    pub fn flat(update_type: impl Into<String>) -> Self {
        Self {
            flat: true,
            ..Self::new(update_type)
        }
    }

    /// 不限定类型（走兜底通道），只按下面的字段匹配。
    pub fn any() -> Self {
        Self {
            update_type: None,
            chat_id: None,
            sender_user_id: None,
            message_thread_id: None,
            flat: false,
        }
    }

    pub fn chat_id(mut self, chat_id: i64) -> Self {
        self.chat_id = Some(chat_id);
        self
    }

    pub fn sender_user_id(mut self, sender_user_id: i64) -> Self {
        self.sender_user_id = Some(sender_user_id);
        self
    }

    pub fn message_thread_id(mut self, message_thread_id: i64) -> Self {
        self.message_thread_id = Some(message_thread_id);
        self
    }

    /// 转成宿主认的过滤器 JSON。
    ///
    /// `update_type` 走的是**通道选择**（`event_type` 键），不是负载字段匹配：
    /// 宿主把每条更新同时发到具体 `@type` 与兜底 `"update"` 两条通道上。
    pub fn to_json(&self) -> Value {
        let mut object = Map::new();
        object.insert(
            FILTER_EVENT_TYPE_KEY.to_owned(),
            json!(self.update_type.as_deref().unwrap_or(UPDATE_EVENT)),
        );

        let prefix = if self.flat { "" } else { "message." };
        let mut put = |key: &str, value: Option<i64>| {
            if let Some(value) = value {
                object.insert(format!("{prefix}{key}"), json!(value));
            }
        };
        put("chat_id", self.chat_id);
        put("sender_user_id", self.sender_user_id);
        put("message_thread_id", self.message_thread_id);
        Value::Object(object)
    }
}

// ── 命令（插件最常见的那种入口）──────────────────────────────────

/// 一条 `updateNewMessage` 里的命令：`/start`、`/help@SomeBot 参数`。
///
/// 只认**文本消息**，并跳过自己发的（`is_outgoing`）——机器人不该响应自己。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub chat_id: i64,
    pub message_id: i64,
    /// 命令名：去掉 `/` 与 `@BotName` 后缀，已转小写。
    pub name: String,
    /// `/cmd` 后面那一段（去首尾空白，可能为空）。
    pub args: String,
    /// 整条消息文本（命令 + 参数）。
    pub text: String,
}

impl Command {
    /// 是这条命令吗（不分大小写，`/Start` 也算）。
    pub fn is(&self, name: &str) -> bool {
        self.name.eq_ignore_ascii_case(name)
    }
}

/// 从 `updateNewMessage` 的负载里认出一条命令；不是命令就 `None`。
///
/// 字段路径照真 `td_api.tl`：`updateNewMessage.message` →
/// `content:messageText` → `text:formattedText` → `text:string`。
pub fn command_of(update: &Value) -> Option<Command> {
    let message = update.get("message")?;
    if message
        .get("is_outgoing")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }
    let content = message.get("content")?;
    if content.get("@type").and_then(Value::as_str) != Some("messageText") {
        return None;
    }
    let text = content.get("text")?.get("text")?.as_str()?;
    let trimmed = text.trim_start();
    let token = trimmed.split_whitespace().next()?;
    // `/help@SomeBot args…` → `help`；`@` 之后是「发给哪个 bot」，不是命令的一部分。
    let name = token
        .strip_prefix('/')?
        .split('@')
        .next()
        .unwrap_or_default();
    if name.is_empty() {
        return None;
    }
    Some(Command {
        chat_id: message.get("chat_id")?.as_i64()?,
        message_id: message.get("id")?.as_i64()?,
        name: name.to_ascii_lowercase(),
        args: trimmed[token.len()..].trim().to_owned(),
        text: text.to_owned(),
    })
}

/// 一个活着的「命令订阅」：**drop 就是退订**（和 `Effect` 一个语义）。
///
/// 回调闭包由它自己持有——递给 ABI 的 `user_data` 指的就是这里那个 Box，
/// 退订之后一起释放。
pub struct CommandSubscription {
    host: HostApi,
    handle: SubscriptionHandle,
    /// 只负责**持有**回调；退订之前它的地址必须稳定。
    _handler: Box<Box<dyn Fn(&Command) + Send + Sync>>,
}

impl Drop for CommandSubscription {
    fn drop(&mut self) {
        if self.handle.is_valid() {
            self.host.off(self.handle);
        }
    }
}

/// 订阅 `updateNewMessage`，把每条命令交给 `handler`（非命令、自己发的都不打扰它）。
///
/// 插件在自己的 `start` 里调一次，把返回的守卫**存进插件状态**（drop 就退订）。
/// 宿主拒绝订阅时返回 `None`。
pub fn on_command(
    host: HostApi,
    handler: impl Fn(&Command) + Send + Sync + 'static,
) -> Option<CommandSubscription> {
    let mut owned: Box<Box<dyn Fn(&Command) + Send + Sync>> = Box::new(Box::new(handler));
    // 递给 ABI 的是**内层 Box 的地址**：它是瘦指针，外层 Box 活着它就稳定。
    let user_data = (&raw mut *owned).cast::<c_void>();
    let handle = host.on("updateNewMessage", None, command_trampoline, user_data);
    handle.is_valid().then_some(CommandSubscription {
        host,
        handle,
        _handler: owned,
    })
}

/// 命令订阅的 C 回调：把负载解成 [`Command`]，再交给闭包。
extern "C-unwind" fn command_trampoline(user_data: *mut c_void, payload: *const c_char) -> i32 {
    if user_data.is_null() || payload.is_null() {
        return -1;
    }
    // SAFETY: `user_data` 是 `on_command` 里那个 Box，订阅活多久它就活多久。
    let handler = unsafe { &*(user_data as *const Box<dyn Fn(&Command) + Send + Sync>) };
    // SAFETY: 宿主保证 `payload` 是 NUL 结尾的临时字符串。
    let text = unsafe { CStr::from_ptr(payload) }.to_string_lossy();
    let Ok(update) = serde_json::from_str::<Value>(&text) else {
        return 0;
    };
    if let Some(command) = command_of(&update) {
        handler(&command);
    }
    0
}

/// 一次实体解析的结果。
#[derive(Debug, Clone)]
pub struct EntitiesResolution {
    /// 聚合关联 id：本次最后一条发出的请求（只为日志关联）。
    pub request_id: RequestId,
    pub entities: Vec<EntityResult>,
}

impl EntitiesResolution {
    pub fn to_json(&self) -> Value {
        json!({
            "request_id": self.request_id.id,
            "entities": self.entities.iter().map(EntityResult::to_json).collect::<Vec<_>>(),
        })
    }
}

/// 一个实体的解析结果。
#[derive(Debug, Clone)]
pub struct EntityResult {
    pub id: EntityId,
    pub request: RequestId,
    /// TDLib 原始 JSON；超时或没等到时是 `Null`。
    pub value: Value,
}

impl EntityResult {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id.to_json(),
            "request_id": self.request.id,
            "value": self.value,
        })
    }
}

/// 下载结束后的状态。
#[derive(Debug, Clone)]
pub struct FileDownload {
    pub request_id: RequestId,
    pub file_id: i32,
    pub completed: bool,
    /// 最后一次 `updateFile` 的原始 JSON。
    pub last_update: Value,
}

/// TDLib 便捷入口。持有一个 [`HostApi`]（它是 `Copy` 的，随手复制）。
#[derive(Debug, Clone, Copy)]
pub struct Simplify {
    host: HostApi,
}

impl Simplify {
    pub fn new(host: HostApi) -> Self {
        Self { host }
    }

    pub fn host(&self) -> HostApi {
        self.host
    }

    /// 把一段文本交给 **TDLib 的 `parseTextEntities`** 解析成 `formattedText`。
    ///
    /// 这是 `formattedText` 链路唯一的转换入口：没有第二套解析器，
    /// 也就不存在「我们认的语法」和「Telegram 认的语法」漂移这回事。
    /// （富消息走的是另一条路：本地块树见 [`RichMessage::from_markdown`]，
    /// 服务端解析见 [`RichMessage::server_markdown`]。）
    ///
    /// 走的是**同步**的 `td_execute`（`td_json_client_execute`）：`parseTextEntities` 不碰服务器，
    /// TDLib 允许同步跑它。这一点很关键——等响应那套会卡住投递线程，
    /// 而插件通常正是在更新回调里发消息。
    ///
    /// 拿不到结果（没接 TDLib、TDLib 报错、`Plain` 模式）就退回纯文本，不报错——
    /// 一条 Markdown 写错的消息不该整条发不出去。
    pub fn formatted(&self, text: &Text) -> Value {
        let Some(request) = parse_text_request(text) else {
            return plain_text(text); // Plain：没有对应的 TextParseMode，别白跑一趟
        };
        let response = self
            .host
            .td_execute(&request.to_string())
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());
        if !is_formatted_text(response.as_ref()) {
            self.host.log(
                fuyubot_abi::LogLevel::Warn,
                &format!(
                    "宿主没接 TDLib（或解析失败），Markdown 按纯文本发：{}",
                    text.text
                ),
            );
        }
        formatted_from(text, response.as_ref())
    }

    /// 发消息并**等到发送结局**：成功返回最终的 `Message`，失败 / 超时返回错误。
    ///
    /// 时间线（TDLib 是异步的，这里把两步合成一次）：
    ///
    /// 1. `sendMessage` 的**响应**先回来，里面是还**没发出去**的临时消息；
    /// 2. 随后那条临时消息各来一条结局更新——`updateMessageSendSucceeded`
    ///    （`message` 是最终消息）或 `updateMessageSendFailed`（`error_code` /
    ///    `error_message`）。按临时消息的 `old_message_id` 精确对号，
    ///    同一会话里别人发的结局不会串进来。
    ///
    /// ⚠️ **阻塞**当前线程：在更新回调里调它就是自己卡自己（回调跑在接收循环
    /// 那个线程上，更新送不回来），等到的一定是超时——回调里请用
    /// [`send_message_then`](Self::send_message_then)。
    ///
    /// 超时会先 `deleteMessages(revoke = true)` 把那条还没发出去的消息撤回，再返回
    /// [`SimplifyError::Timeout`]。
    pub fn send_message(&self, chat_id: i64, params: &SendMessageParams) -> Result<Value> {
        let request_id = self.invoke_send_message(chat_id, params)?;
        let deadline = Instant::now() + Duration::from_millis(params.timeout_ms);

        // 1. 响应 = 临时消息（sendMessage = Message）
        let pending = self.await_response(request_id, deadline, params.timeout_ms)?;
        let old_message_id = pending.get("id").and_then(Value::as_i64);

        // 2. 这条临时消息的结局：成功 / 失败
        self.await_outcome(chat_id, old_message_id, deadline, params.timeout_ms)
    }

    /// 发一组相册（2–10 项，仅音频/文档/图片/视频可混）并**等到每一项的结局**。
    ///
    /// `sendMessageAlbum` 返回 `Messages`（一组临时消息），不是单条 `Message`：
    /// 先收响应拿那一组 id，再逐项等结局——**全部成功**才返回（`Vec<Value>`，
    /// 与 `medias` 同序的最终消息）。任一项 `updateMessageSendFailed` 就返回
    /// [`SimplifyError::SendFailed`]；超时则撤回还没发出去的那些，返回
    /// [`SimplifyError::Timeout`]。
    ///
    /// ⚠️ **阻塞**当前线程，回调里请用 [`send_message_album_then`](Self::send_message_album_then)。
    /// `timeout_ms` 是**整组共用**的一个预算，不是每项各等一次。
    pub fn send_message_album(&self, chat_id: i64, params: &SendAlbumParams) -> Result<Vec<Value>> {
        let request_id = self.invoke_send_album(chat_id, params)?;
        let deadline = Instant::now() + Duration::from_millis(params.timeout_ms);

        let response = self.await_response(request_id, deadline, params.timeout_ms)?;
        let ids: Vec<Option<i64>> = pending_messages(&response)
            .iter()
            .map(|message| message.get("id").and_then(Value::as_i64))
            .collect();

        let mut messages = Vec::with_capacity(ids.len());
        for (index, old_message_id) in ids.iter().enumerate() {
            let Some(old_message_id) = *old_message_id else {
                self.cancel_sending(chat_id, ids[index..].iter().flatten());
                return Err(SimplifyError::Timeout(params.timeout_ms));
            };
            messages.push(self.await_outcome(
                chat_id,
                Some(old_message_id),
                deadline,
                params.timeout_ms,
            )?);
        }
        Ok(messages)
    }

    // ── 发送的内部三步：发 → 等响应 → 等结局 ──────────────────────

    /// 只发请求，不等结果（`sendMessage`）。
    fn invoke_send_message(&self, chat_id: i64, params: &SendMessageParams) -> Result<RequestId> {
        let payload = send_params(chat_id, params, &|text: &Text| self.formatted(text));
        Ok(self
            .host
            .td_invoke_simplified(td_api::SendMessage::METHOD, &payload.to_string())?)
    }

    /// 只发请求，不等结果（`sendMessageAlbum`）——顺带先验项数。
    fn invoke_send_album(&self, chat_id: i64, params: &SendAlbumParams) -> Result<RequestId> {
        if !(2..=10).contains(&params.medias.len()) {
            return Err(SimplifyError::AlbumSize(params.medias.len()));
        }
        let payload = album_params(chat_id, params, &|text: &Text| self.formatted(text));
        Ok(self
            .host
            .td_invoke_simplified(td_api::SendMessageAlbum::METHOD, &payload.to_string())?)
    }

    /// 等某个请求的响应：拿到就返回，`error` 变 [`SimplifyError::Tdlib`]，
    /// 超时变 [`SimplifyError::Timeout`]。
    fn await_response(
        &self,
        request_id: RequestId,
        deadline: Instant,
        timeout_ms: u64,
    ) -> Result<Value> {
        let Some(response) = self.wait_response(request_id, remaining_ms(deadline)) else {
            return Err(SimplifyError::Timeout(timeout_ms));
        };
        match error_of(&response) {
            Some(error) => Err(as_tdlib_error(error)),
            None => Ok(response),
        }
    }

    /// 等一条临时消息的**结局**：成功给最终 `Message`，失败给错误，超时先撤回再给错误。
    ///
    /// 走**兜底的 `"update"` 通道** + `old_message_id` 精确匹配：成功和失败两条更新
    /// 都在这一条通道上，所以一次等待就能拿到「到底成了没」。
    fn await_outcome(
        &self,
        chat_id: i64,
        old_message_id: Option<i64>,
        deadline: Instant,
        timeout_ms: u64,
    ) -> Result<Value> {
        let Some(old_message_id) = old_message_id else {
            return Err(SimplifyError::Timeout(timeout_ms));
        };
        let filter = json!({
            FILTER_EVENT_TYPE_KEY: UPDATE_EVENT,
            "old_message_id": old_message_id,
        })
        .to_string();
        let Some(update) = payload_of(self.host.wait_for_update(&filter, remaining_ms(deadline)))
        else {
            // 没等到结局：消息多半还卡在发送中，撤回它
            self.cancel_sending(chat_id, std::iter::once(&old_message_id));
            return Err(SimplifyError::Timeout(timeout_ms));
        };
        match send_failed_of(&update) {
            Some(error) => Err(error),
            None => Ok(update.get("message").cloned().unwrap_or(Value::Null)),
        }
    }

    /// 撤回还没发出去的（临时）消息：`deleteMessages(revoke = true)`。
    ///
    /// 这是超时之后能做的唯一补救——发出去的撤不回，没发出去的会被取消。
    /// 只打日志，不把删除的成败当成结果返回。
    fn cancel_sending<'a>(&self, chat_id: i64, message_ids: impl IntoIterator<Item = &'a i64>) {
        let ids: Vec<i64> = message_ids.into_iter().copied().collect();
        if ids.is_empty() {
            return;
        }
        let params = json!({ "chat_id": chat_id, "message_ids": ids, "revoke": true });
        match self
            .host
            .td_invoke_simplified(td_api::DeleteMessages::METHOD, &params.to_string())
        {
            Ok(_) => self.host.log(
                fuyubot_abi::LogLevel::Warn,
                &format!("发送超时，已撤回未发出的消息 {ids:?}"),
            ),
            Err(error) => self.host.log(
                fuyubot_abi::LogLevel::Error,
                &format!("发送超时，撤回 {ids:?} 也失败了：{error}"),
            ),
        }
    }

    /// 改一条消息的文本（`editMessageText`），**等到响应**并返回改完的 `Message`。
    ///
    /// 编辑没有「发送成功 / 失败」那一对更新，它的结果就是**响应**：
    /// 成功是 `Message`，`Message is not modified` 这类是 `@type == "error"`
    /// （变成 [`SimplifyError::Tdlib`]）。**不在这里重试**——限流该等多久是调用方的事。
    ///
    /// ⚠️ **阻塞**当前线程，回调里请用 [`edit_message_text_then`](Self::edit_message_text_then)。
    pub fn edit_message_text(
        &self,
        chat_id: i64,
        message_id: i64,
        params: &EditTextParams,
    ) -> Result<Value> {
        let payload = edit_text_params(chat_id, message_id, params, &|text: &Text| {
            self.formatted(text)
        });
        let request_id = self
            .host
            .td_invoke_simplified(td_api::EditMessageText::METHOD, &payload.to_string())?;
        self.await_response(
            request_id,
            Instant::now() + Duration::from_millis(params.timeout_ms),
            params.timeout_ms,
        )
    }

    /// 改一条媒体消息的标题（`editMessageCaption`），等到响应返回改完的 `Message`。
    pub fn edit_message_caption(
        &self,
        chat_id: i64,
        message_id: i64,
        params: &EditCaptionParams,
    ) -> Result<Value> {
        let payload = edit_caption_params(chat_id, message_id, params, &|text: &Text| {
            self.formatted(text)
        });
        let request_id = self
            .host
            .td_invoke_simplified(td_api::EditMessageCaption::METHOD, &payload.to_string())?;
        self.await_response(
            request_id,
            Instant::now() + Duration::from_millis(params.timeout_ms),
            params.timeout_ms,
        )
    }

    /// 换掉一条消息的内容（`editMessageMedia`），等到响应返回改完的 `Message`。
    ///
    /// 和发消息同一套 [`Media`]：内容形状只有一份实现。
    pub fn edit_message_media(
        &self,
        chat_id: i64,
        message_id: i64,
        params: &EditMediaParams,
    ) -> Result<Value> {
        let payload = edit_media_params(chat_id, message_id, params, &|text: &Text| {
            self.formatted(text)
        });
        let request_id = self
            .host
            .td_invoke_simplified(td_api::EditMessageMedia::METHOD, &payload.to_string())?;
        self.await_response(
            request_id,
            Instant::now() + Duration::from_millis(params.timeout_ms),
            params.timeout_ms,
        )
    }

    /// 只改一条消息的键盘（`editMessageReplyMarkup`）——**相册要挂键盘就只能用这条**：
    /// `sendMessageAlbum` 的真签名里没有 `reply_markup`，发的时候带不上。
    ///
    /// 等到响应返回改完的 `Message`。空键盘 = **移除键盘**
    /// （`replyMarkupRemoveKeyboard`），不是「什么都不改」。
    pub fn edit_message_reply_markup(
        &self,
        chat_id: i64,
        message_id: i64,
        reply_markup: &[Vec<ReplyButton>],
        timeout_ms: u64,
    ) -> Result<Value> {
        let payload = edit_markup_params(chat_id, message_id, reply_markup);
        let request_id = self
            .host
            .td_invoke_simplified(td_api::EditMessageReplyMarkup::METHOD, &payload.to_string())?;
        self.await_response(
            request_id,
            Instant::now() + Duration::from_millis(timeout_ms),
            timeout_ms,
        )
    }

    /// 删消息（`deleteMessages`），`revoke = true` 表示对所有人撤回。
    ///
    /// 等到响应：成功是 `Ok`（TDLib 的 `ok`，没有内容），失败是
    /// [`SimplifyError::Tdlib`]（权限不够、消息太老之类都会在这里冒出来）。
    pub fn delete_message(
        &self,
        chat_id: i64,
        message_ids: impl IntoIterator<Item = i64>,
        revoke: bool,
        timeout_ms: u64,
    ) -> Result<()> {
        let params = json!({
            "chat_id": chat_id,
            "message_ids": message_ids.into_iter().collect::<Vec<_>>(),
            "revoke": revoke,
        });
        let request_id = self
            .host
            .td_invoke_simplified(td_api::DeleteMessages::METHOD, &params.to_string())?;
        self.await_response(
            request_id,
            Instant::now() + Duration::from_millis(timeout_ms),
            timeout_ms,
        )
        .map(|_| ())
    }

    /// 回一条 callback query（点了内联按钮之后必须回，否则客户端一直转圈）。
    ///
    /// 等到响应：成功是 `Ok`，失败是 [`SimplifyError::Tdlib`]。
    pub fn answer_callback_query(
        &self,
        callback_query_id: i64,
        text: Option<&str>,
        show_alert: bool,
        timeout_ms: u64,
    ) -> Result<()> {
        let params = json!({
            "callback_query_id": callback_query_id,
            "text": text.unwrap_or_default(),
            "show_alert": show_alert,
            "url": "",
            "cache_time": 0,
        });
        let request_id = self
            .host
            .td_invoke_simplified(td_api::AnswerCallbackQuery::METHOD, &params.to_string())?;
        self.await_response(
            request_id,
            Instant::now() + Duration::from_millis(timeout_ms),
            timeout_ms,
        )
        .map(|_| ())
    }

    // ── 异步版：不阻塞当前线程，结果走闭包 ────────────────────────
    //
    // 等待版要阻塞，而插件的更新回调**就跑在接收循环那个线程上**——在那里等
    // 就是自己卡自己，等到的一定是超时。回调里一律用 `*_then`：它们把等待挪到
    // 另一个线程上，结果由闭包接住。代价是每次调用起一个线程（有 `timeout_ms`
    // 兜着，不会一直挂着）。

    /// [`send_message`](Self::send_message) 的异步版：发完立刻返回，
    /// 结局（`Ok(最终消息)` / `Err(…)`）从另一个线程交给 `on_done`。
    pub fn send_message_then(
        &self,
        chat_id: i64,
        params: &SendMessageParams,
        on_done: impl FnOnce(Result<Value>) + Send + 'static,
    ) -> std::thread::JoinHandle<()> {
        let simplify = *self;
        let params = params.clone();
        spawn_then(move || simplify.send_message(chat_id, &params), on_done)
    }

    /// [`send_message_album`](Self::send_message_album) 的异步版。
    pub fn send_message_album_then(
        &self,
        chat_id: i64,
        params: &SendAlbumParams,
        on_done: impl FnOnce(Result<Vec<Value>>) + Send + 'static,
    ) -> std::thread::JoinHandle<()> {
        let simplify = *self;
        let params = params.clone();
        spawn_then(
            move || simplify.send_message_album(chat_id, &params),
            on_done,
        )
    }

    /// [`edit_message_text`](Self::edit_message_text) 的异步版。
    pub fn edit_message_text_then(
        &self,
        chat_id: i64,
        message_id: i64,
        params: &EditTextParams,
        on_done: impl FnOnce(Result<Value>) + Send + 'static,
    ) -> std::thread::JoinHandle<()> {
        let simplify = *self;
        let params = params.clone();
        spawn_then(
            move || simplify.edit_message_text(chat_id, message_id, &params),
            on_done,
        )
    }

    /// [`edit_message_caption`](Self::edit_message_caption) 的异步版。
    pub fn edit_message_caption_then(
        &self,
        chat_id: i64,
        message_id: i64,
        params: &EditCaptionParams,
        on_done: impl FnOnce(Result<Value>) + Send + 'static,
    ) -> std::thread::JoinHandle<()> {
        let simplify = *self;
        let params = params.clone();
        spawn_then(
            move || simplify.edit_message_caption(chat_id, message_id, &params),
            on_done,
        )
    }

    /// [`edit_message_media`](Self::edit_message_media) 的异步版。
    pub fn edit_message_media_then(
        &self,
        chat_id: i64,
        message_id: i64,
        params: &EditMediaParams,
        on_done: impl FnOnce(Result<Value>) + Send + 'static,
    ) -> std::thread::JoinHandle<()> {
        let simplify = *self;
        let params = params.clone();
        spawn_then(
            move || simplify.edit_message_media(chat_id, message_id, &params),
            on_done,
        )
    }

    /// [`edit_message_reply_markup`](Self::edit_message_reply_markup) 的异步版。
    pub fn edit_message_reply_markup_then(
        &self,
        chat_id: i64,
        message_id: i64,
        reply_markup: Vec<Vec<ReplyButton>>,
        timeout_ms: u64,
        on_done: impl FnOnce(Result<Value>) + Send + 'static,
    ) -> std::thread::JoinHandle<()> {
        let simplify = *self;
        spawn_then(
            move || {
                simplify.edit_message_reply_markup(chat_id, message_id, &reply_markup, timeout_ms)
            },
            on_done,
        )
    }

    /// [`delete_message`](Self::delete_message) 的异步版。
    pub fn delete_message_then(
        &self,
        chat_id: i64,
        message_ids: Vec<i64>,
        revoke: bool,
        timeout_ms: u64,
        on_done: impl FnOnce(Result<()>) + Send + 'static,
    ) -> std::thread::JoinHandle<()> {
        let simplify = *self;
        spawn_then(
            move || simplify.delete_message(chat_id, message_ids, revoke, timeout_ms),
            on_done,
        )
    }

    /// [`answer_callback_query`](Self::answer_callback_query) 的异步版。
    pub fn answer_callback_query_then(
        &self,
        callback_query_id: i64,
        text: Option<String>,
        show_alert: bool,
        timeout_ms: u64,
        on_done: impl FnOnce(Result<()>) + Send + 'static,
    ) -> std::thread::JoinHandle<()> {
        let simplify = *self;
        spawn_then(
            move || {
                simplify.answer_callback_query(
                    callback_query_id,
                    text.as_deref(),
                    show_alert,
                    timeout_ms,
                )
            },
            on_done,
        )
    }

    // ── 等更新 / 等响应 ────────────────────────────────────────────

    /// 等一条匹配的更新。**阻塞**当前线程，直到匹配、超时或被 `cancel_wait` 取消。
    ///
    /// ⚠️ 返回的 [`WaitHandle::payload_json`] 是宿主临时缓冲，下一次宿主调用前有效；
    /// 想省心就用 [`wait_for_update_json`](Self::wait_for_update_json)。
    pub fn wait_for_update(&self, filter: &UpdateFilter, timeout_ms: u64) -> WaitHandle {
        self.host
            .wait_for_update(&filter.to_json().to_string(), timeout_ms)
    }

    /// 同上，但顺手把负载拷成 [`Value`]。超时返回 `None`。
    pub fn wait_for_update_json(
        &self,
        filter: &UpdateFilter,
        timeout_ms: u64,
    ) -> Result<Option<Value>> {
        let filter = filter.to_json().to_string();
        let handle = self.host.wait_for_update(&filter, timeout_ms);
        Ok(payload_of(handle))
    }

    /// 等某个请求的**响应**（`"response"` 通道按 `request_id` 过滤）。
    ///
    /// 编辑类方法靠它拿改完的 `Message`；TDLib 报错时拿到的同样是响应
    /// （`@type == "error"`，`message` 里是原因）。超时返回 `None`。
    ///
    /// ⚠️ 同样**阻塞**当前线程。
    pub fn wait_response(&self, request: RequestId, timeout_ms: u64) -> Option<Value> {
        let filter = json!({
            FILTER_EVENT_TYPE_KEY: RESPONSE_EVENT,
            "request_id": request.id,
        })
        .to_string();
        payload_of(self.host.wait_for_update(&filter, timeout_ms))
    }

    /// 取消一个还没结束的等待（从另一个线程发起）。
    pub fn cancel_wait(&self, handle: WaitHandle) {
        self.host.cancel_wait(handle);
    }

    // ── 批量实体解析 ──────────────────────────────────────────────

    /// 并发发起 `getChat` / `getUser`，逐个收齐后聚合成一个事件推回去。
    ///
    /// **会阻塞**：请求是同时发出的，收集是逐个等的，每个的等待上限都是 `timeout_ms`。
    /// 聚合结果同时以 `"entities_resolved"` 事件推送（负载见 [`EntitiesResolution::to_json`]）。
    pub fn resolve_entities(
        &self,
        ids: Vec<EntityId>,
        timeout_ms: u64,
    ) -> Result<EntitiesResolution> {
        // 1. 先把请求全发出去（这一步是并发的：不等任何响应）
        let mut requests = Vec::with_capacity(ids.len());
        for id in &ids {
            let params = id.params().to_string();
            requests.push(self.host.td_invoke_simplified(id.method(), &params)?);
        }

        // 2. 逐个等响应
        let mut entities = Vec::with_capacity(ids.len());
        for (id, request) in ids.into_iter().zip(&requests) {
            entities.push(EntityResult {
                id,
                request: *request,
                value: self
                    .wait_response(*request, timeout_ms)
                    .unwrap_or(Value::Null),
            });
        }

        let resolution = EntitiesResolution {
            request_id: requests.last().copied().unwrap_or(RequestId::INVALID),
            entities,
        };
        // 3. 聚合成一个事件推回去（失败也不影响返回值）
        let _ = self
            .host
            .emit("entities_resolved", &resolution.to_json().to_string());
        Ok(resolution)
    }

    // ── 文件下载进度 ──────────────────────────────────────────────

    /// 发起下载并**阻塞**到下载完成（或超时），期间每收到一条 `updateFile` 就推一个
    /// 简化事件 `"file_progress"`。
    ///
    /// 想看实时进度又不想阻塞，就直接订阅原始的 `updateFile`——原始更新一直可见。
    pub fn download_file(
        &self,
        file_id: i32,
        priority: i32,
        timeout_ms: u64,
    ) -> Result<FileDownload> {
        let params = json!({ "file_id": file_id, "priority": priority, "offset": 0, "limit": 0 });
        let request_id = self
            .host
            .td_invoke_simplified(td_api::DownloadFile::METHOD, &params.to_string())?;

        // TDLib 的 updateFile 长这样：{"@type":"updateFile","file":{"id":..,"local":{..}}}
        // —— file id 在 `file.id`，不是顶层。
        let filter = json!({ FILTER_EVENT_TYPE_KEY: UPDATE_EVENT, "file.id": file_id }).to_string();
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut last_update = Value::Null;
        let mut completed = false;

        while Instant::now() < deadline {
            let handle = self.host.wait_for_update(&filter, remaining_ms(deadline));
            let Some(update) = payload_of(handle) else {
                break; // 超时或被取消
            };

            completed = update
                .pointer("/file/local/is_downloading_completed")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            self.emit_progress(file_id, &update, completed);
            last_update = update;
            if completed {
                break;
            }
        }

        Ok(FileDownload {
            request_id,
            file_id,
            completed,
            last_update,
        })
    }

    fn emit_progress(&self, file_id: i32, update: &Value, completed: bool) {
        let simplified = json!({
            "file_id": file_id,
            "completed": completed,
            "downloaded_size": update.pointer("/file/local/downloaded_size"),
            "expected_size": update.pointer("/file/expected_size"),
        });
        let _ = self.host.emit("file_progress", &simplified.to_string());
    }
}

/// 把一次等待挪到**另一个线程**上，结果交给 `on_done`。
///
/// 这是 `*_then` 那一套的底：等待版要阻塞，而插件回调跑在接收循环线程上，
/// 在那里等就是自己卡自己。挪走之后回调线程立刻返回，结局由闭包接住。
/// 返回的句柄**要收好**：插件是动态库，卸载之后这个线程还在跑库里的代码就是
/// 未定义行为——`stop()` 里 join 掉它们（内置插件就是这么写的）。
fn spawn_then<T>(
    work: impl FnOnce() -> T + Send + 'static,
    on_done: impl FnOnce(T) + Send + 'static,
) -> std::thread::JoinHandle<()>
where
    T: Send + 'static,
{
    // 起不了线程是 OS 资源耗尽那一档，让它 panic 好过悄悄丢掉结果。
    std::thread::spawn(move || on_done(work()))
}

/// 到 `deadline` 还剩多少毫秒（已经超了就是 0）。
///
/// 一段等待里要等多次时用它把**同一个**预算分下去：`timeout_ms` 是全程上限，
/// 不是每一次等待各给一份。
fn remaining_ms(deadline: Instant) -> u64 {
    deadline
        .saturating_duration_since(Instant::now())
        .as_millis() as u64
}

/// 响应是 `error` 吗（`@type == "error"`，`message` 里是原因）。
fn error_of(response: &Value) -> Option<&Value> {
    (response.get("@type").and_then(Value::as_str) == Some("error")).then_some(response)
}

/// 从 `Messages` 响应里取出那一组（临时）消息；不是 `Messages` 就当没有。
fn pending_messages(response: &Value) -> Vec<Value> {
    if response.get("@type").and_then(Value::as_str) != Some("messages") {
        return Vec::new();
    }
    response
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// 把 [`WaitHandle`] 的负载立刻拷成 [`Value`]；没匹配上就是 `None`。
///
/// 必须在**任何**其它宿主调用之前调用它——宿主返回的字符串是线程局部缓冲。
fn payload_of(handle: WaitHandle) -> Option<Value> {
    if !handle.did_match() || handle.payload_json.is_null() {
        return None;
    }
    // SAFETY: 宿主保证这个指针在下一次宿主调用之前有效，这里立刻读完。
    let text = unsafe { CStr::from_ptr(handle.payload_json) }.to_string_lossy();
    serde_json::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyubot_abi::{HostApi, UPDATE_EVENT as ABI_UPDATE_EVENT};
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

    /// 把等待上限压成 0：这些测试只验**请求怎么拼**，不等结果。
    ///
    /// 超时上限为 0 时 `wait_for_update` 立刻返回「没等到」，所以既不用真的等，
    /// 也不会走到「超时撤回」那一步（没拿到临时消息 id）——`sent_requests` 里
    /// 就只有被测的那一条请求。
    macro_rules! quick {
        ($params:expr) => {{
            let mut params = $params;
            params.timeout_ms = 0;
            params
        }};
    }

    /// 发一条**注定超时**的请求（离线环境没有 TDLib 回响应），只为让它留档。
    macro_rules! fire {
        ($simplify:expr, $method:ident $(, $arg:expr)*) => {
            let _ = $simplify.$method($($arg,)*);
        };
    }

    /// 七种内容**全是 `sendMessage`**，每种一个 `InputMessageContent` 形状。
    #[test]
    fn every_media_kind_becomes_one_send_message() {
        let rt = runtime();
        let simplify = simplify(&rt);

        fire!(
            simplify,
            send_message,
            7,
            &quick!(SendMessageParams::text("hi"))
        );
        fire!(
            simplify,
            send_message,
            7,
            &quick!(SendMessageParams::media(Media::photo("/tmp/a.jpg")))
        );
        fire!(
            simplify,
            send_message,
            7,
            &quick!(SendMessageParams::media(Media::Video {
                file: InputFile::remote("video-id"),
                caption: None,
                thumbnail: None,
                cover: None,
                start_timestamp: None,
                duration: 3,
                width: 640,
                height: 480,
                supports_streaming: true,
                has_spoiler: false,
            }))
        );
        fire!(
            simplify,
            send_message,
            7,
            &quick!(SendMessageParams::media(Media::Audio {
                file: "/tmp/a.mp3".into(),
                caption: None,
                album_cover_thumbnail: None,
                duration: 12,
                title: "标题".to_owned(),
                performer: "作者".to_owned(),
            }))
        );
        fire!(
            simplify,
            send_message,
            7,
            &quick!(SendMessageParams::media(Media::document("/tmp/a.pdf")))
        );
        fire!(
            simplify,
            send_message,
            7,
            &quick!(SendMessageParams::media(Media::Animation {
                file: "/tmp/a.gif".into(),
                caption: None,
                thumbnail: None,
                duration: 1,
                width: 10,
                height: 10,
                has_spoiler: false,
            }))
        );
        fire!(
            simplify,
            send_message,
            7,
            &quick!(SendMessageParams::media(Media::Sticker {
                file: "/tmp/a.webp".into(),
                emoji: "😀".to_owned(),
                thumbnail: None,
                width: 512,
                height: 512,
            }))
        );
        fire!(
            simplify,
            send_message,
            7,
            &quick!(SendMessageParams::media(Media::location(1.5, 2.5)))
        );

        let sent = rt.sent_requests();
        let methods: Vec<&str> = sent.iter().map(|request| request.method.as_str()).collect();
        // TDLib 没有 sendPhoto / sendDocument / sendLocation——那三个是 Bot API 的方法名。
        for (index, method) in methods.iter().enumerate() {
            assert_eq!(*method, "sendMessage", "第 {index} 条不是 sendMessage");
        }
        assert_eq!(methods.len(), 8);

        // 纯文本
        let text = &sent[0].params;
        assert_eq!(text["chat_id"], 7);
        assert_eq!(text["input_message_content"]["@type"], "inputMessageText");
        assert_eq!(text["input_message_content"]["text"]["text"], "hi");

        // 图片：inputMessagePhoto.photo(inputPhoto).photo(InputFile)
        assert_eq!(
            sent[1].params["input_message_content"]["photo"]["@type"],
            "inputPhoto"
        );
        assert_eq!(
            sent[1].params["input_message_content"]["photo"]["photo"]["@type"],
            "inputFileLocal"
        );
        assert_eq!(
            sent[1].params["input_message_content"]["photo"]["photo"]["path"],
            "/tmp/a.jpg"
        );
        // 远程文件走 inputFileRemote（id 而不是 path）
        assert_eq!(
            sent[2].params["input_message_content"]["video"]["video"]["@type"],
            "inputFileRemote"
        );
        assert_eq!(
            sent[2].params["input_message_content"]["video"]["video"]["id"],
            "video-id"
        );
        // 音频 / 文档 / 动图各自的构造器名
        assert_eq!(
            sent[3].params["input_message_content"]["audio"]["@type"],
            "inputAudio"
        );
        assert_eq!(
            sent[4].params["input_message_content"]["document"]["@type"],
            "inputDocument"
        );
        assert_eq!(
            sent[5].params["input_message_content"]["animation"]["@type"],
            "inputAnimation"
        );
        // 贴纸：emoji 在内容顶层，**没有 caption**
        let sticker = &sent[6].params["input_message_content"];
        assert_eq!(sticker["@type"], "inputMessageSticker");
        assert_eq!(sticker["emoji"], "😀");
        assert!(sticker.get("caption").is_none());
        // 位置：经纬度嵌在 location 里（不是顶层）
        let location = &sent[7].params["input_message_content"];
        assert_eq!(location["@type"], "inputMessageLocation");
        assert_eq!(location["location"]["latitude"], 1.5);
    }

    /// 短名字的参数（`topic_id` / `reply_to_message_id` / `reply_markup` / `link_preview`）
    /// 都要落到真 `sendMessage` 的字段名上。
    #[test]
    fn send_params_carry_topic_reply_markup_and_link_preview() {
        let rt = runtime();
        let simplify = simplify(&rt);

        fire!(
            simplify,
            send_message,
            7,
            &SendMessageParams {
                text: Some(Text::plain("hi")),
                media: None,
                reply_to_message_id: Some(9),
                topic_id: Some(Topic::forum(3)),
                reply_markup: vec![vec![
                    ReplyButton::callback("ok", "x")
                        .emoji(4242)
                        .style(ButtonStyle::Primary),
                    ReplyButton::url("打开", "https://example.com"),
                ]],
                link_preview: Some(false),
                timeout_ms: 0,
            }
        );

        let params = rt.sent_requests()[0].params.clone();
        // 真 `sendMessage` 是 `chat_id topic_id reply_to options reply_markup input_message_content`
        // 论坛话题是 messageTopicForum（不是 messageTopicThread——那是非论坛群组的评论串）
        assert_eq!(params["topic_id"]["@type"], "messageTopicForum");
        assert_eq!(params["topic_id"]["forum_topic_id"], 3);
        assert_eq!(params["reply_to"]["@type"], "inputMessageReplyToMessage");
        assert_eq!(params["reply_to"]["message_id"], 9);
        assert!(
            params.get("message_thread_id").is_none(),
            "Bot API 的名字不该出现：{params}"
        );
        assert_eq!(params["reply_markup"]["@type"], "replyMarkupInlineKeyboard");
        let ok = &params["reply_markup"]["rows"][0][0];
        assert_eq!(ok["type"]["@type"], "inlineKeyboardButtonTypeCallback");
        // callback data 是 base64（"x" → "eA=="）
        assert_eq!(ok["type"]["data"], "eA==");
        assert_eq!(ok["icon_custom_emoji_id"], 4242);
        assert_eq!(ok["style"]["@type"], "buttonStylePrimary");
        assert_eq!(
            params["reply_markup"]["rows"][0][1]["type"]["@type"],
            "inlineKeyboardButtonTypeUrl"
        );
        // 关掉预览 = inputMessageText.link_preview_options.is_disabled
        assert_eq!(
            params["input_message_content"]["link_preview_options"]["is_disabled"],
            true
        );
    }

    /// 媒体的标题：`params.text` 优先，其次媒体自带 caption；两者都走同一条解析。
    #[test]
    fn media_caption_prefers_params_text() {
        let media = Media::photo("/tmp/a.jpg").caption("<自带标题>");
        let params = SendMessageParams {
            text: Some(Text::plain("参数标题")),
            media: Some(media.clone()),
            ..SendMessageParams::default()
        };
        let content = content_json(
            &media_with_caption(params.media.as_ref().unwrap(), params.text.as_ref()),
            &plain_text,
        );
        assert_eq!(content["@type"], "inputMessagePhoto");
        assert_eq!(content["caption"]["text"], "参数标题");

        // 没给 params.text 时用媒体自带的
        let content = content_json(&media, &plain_text);
        assert_eq!(content["caption"]["text"], "<自带标题>");
    }

    /// 缩略图 / 封面：`inputThumbnail` 挂在各 `inputX` 上，**没设置就整个键不发**。
    ///
    /// 真 `td_api.tl`：`inputVideo video:InputFile thumbnail:inputThumbnail cover:InputFile
    /// start_timestamp:int32 …`；`inputAudio` 那个叫 `album_cover_thumbnail`。
    #[test]
    fn media_thumbnail_and_cover_are_omitted_when_unset() {
        // 没设置：既没有 thumbnail，也没有 cover / start_timestamp（保持改之前的行为）
        let content = content_json(&Media::video("/tmp/a.mp4"), &plain_text);
        let video = &content["video"];
        assert_eq!(video["@type"], "inputVideo");
        assert!(video.get("thumbnail").is_none(), "{video}");
        assert!(video.get("cover").is_none(), "{video}");
        assert!(video.get("start_timestamp").is_none(), "{video}");

        // 设了就发：`inputThumbnail` 也是「文件 + 宽高」两层
        let content = content_json(
            &Media::video("/tmp/a.mp4")
                .thumbnail(Thumbnail::new("/tmp/thumb.jpg").size(320, 240))
                .cover("/tmp/cover.jpg", 3),
            &plain_text,
        );
        let video = &content["video"];
        assert_eq!(video["thumbnail"]["@type"], "inputThumbnail");
        assert_eq!(video["thumbnail"]["thumbnail"]["@type"], "inputFileLocal");
        assert_eq!(video["thumbnail"]["thumbnail"]["path"], "/tmp/thumb.jpg");
        assert_eq!(video["thumbnail"]["width"], 320);
        assert_eq!(video["thumbnail"]["height"], 240);
        assert_eq!(video["cover"]["@type"], "inputFileLocal");
        assert_eq!(video["cover"]["path"], "/tmp/cover.jpg");
        assert_eq!(video["start_timestamp"], 3);

        // 音频的封面在真签名里叫 album_cover_thumbnail，不是 thumbnail
        let content = content_json(
            &Media::Audio {
                file: "/tmp/a.mp3".into(),
                caption: None,
                album_cover_thumbnail: Some(Thumbnail::new("/tmp/cover.jpg")),
                duration: 12,
                title: "标题".to_owned(),
                performer: "作者".to_owned(),
            },
            &plain_text,
        );
        assert_eq!(
            content["audio"]["album_cover_thumbnail"]["@type"],
            "inputThumbnail"
        );
        assert!(content["audio"].get("thumbnail").is_none());

        // 图 / 文档 / 动画 / 贴纸四个都能挂缩略图
        let content = content_json(
            &Media::photo("/tmp/a.jpg").thumbnail("/tmp/thumb.jpg"),
            &plain_text,
        );
        assert_eq!(content["photo"]["thumbnail"]["@type"], "inputThumbnail");
        let content = content_json(
            &Media::document("/tmp/a.pdf").thumbnail("/tmp/thumb.png"),
            &plain_text,
        );
        assert_eq!(content["document"]["thumbnail"]["@type"], "inputThumbnail");
        let content = content_json(
            &Media::Animation {
                file: "/tmp/a.gif".into(),
                caption: None,
                thumbnail: None,
                duration: 1,
                width: 10,
                height: 10,
                has_spoiler: false,
            }
            .thumbnail("/tmp/thumb.jpg"),
            &plain_text,
        );
        assert_eq!(content["animation"]["thumbnail"]["@type"], "inputThumbnail");
        let content = content_json(
            &Media::Sticker {
                file: "/tmp/a.webp".into(),
                emoji: "😀".to_owned(),
                thumbnail: Some(Thumbnail::new("/tmp/thumb.webp")),
                width: 512,
                height: 512,
            },
            &plain_text,
        );
        assert_eq!(content["sticker"]["thumbnail"]["@type"], "inputThumbnail");

        // 位置没有缩略图：挂上去原样返回
        let location = Media::location(1.5, 2.5).thumbnail("/tmp/thumb.jpg");
        assert!(matches!(location, Media::Location { .. }));

        // 字段名拿真 schema 钉住（`check_request` 只查顶层参数，不看嵌套对象）
        let schema = fuyubot_tdlib_sys::TdApiSchema::compiled();
        for (constructor, field) in [
            ("inputPhoto", "thumbnail"),
            ("inputVideo", "thumbnail"),
            ("inputVideo", "cover"),
            ("inputVideo", "start_timestamp"),
            ("inputAudio", "album_cover_thumbnail"),
            ("inputDocument", "thumbnail"),
            ("inputAnimation", "thumbnail"),
            ("inputSticker", "thumbnail"),
        ] {
            let entry = schema
                .get(constructor)
                .unwrap_or_else(|| panic!("真 td_api.tl 里没有 {constructor}"));
            assert!(
                entry.has_param(field),
                "{constructor} 里没有 {field}（真 schema 的参数：{:?}）",
                entry.params
            );
        }
        let thumbnail = schema.get("inputThumbnail").expect("inputThumbnail");
        assert_eq!(
            thumbnail.params[0].ty, "InputFile",
            "inputThumbnail 的第一个参数是文件"
        );
    }

    /// 链式设置器：`spoiler` / `size` / `duration` / `streaming` / `title` /
    /// `performer` / `emoji`。只对**真签名里有那个参数**的变体生效，其余原样返回。
    #[test]
    fn media_setters_touch_only_the_variants_that_have_the_field() {
        // 剧透：图 / 视频 / 动图有 has_spoiler
        let content = content_json(&Media::photo("/tmp/a.jpg").spoiler(true), &plain_text);
        assert_eq!(content["@type"], "inputMessagePhoto");
        assert_eq!(content["has_spoiler"], true);
        let content = content_json(&Media::video("/tmp/a.mp4").spoiler(true), &plain_text);
        assert_eq!(content["has_spoiler"], true);
        let content = content_json(
            &Media::Animation {
                file: "/tmp/a.gif".into(),
                caption: None,
                thumbnail: None,
                duration: 1,
                width: 10,
                height: 10,
                has_spoiler: false,
            }
            .spoiler(true),
            &plain_text,
        );
        assert_eq!(content["has_spoiler"], true);
        // 文档没有 has_spoiler：设了也不该冒出这个键
        let content = content_json(&Media::document("/tmp/a.pdf").spoiler(true), &plain_text);
        assert!(content.get("has_spoiler").is_none(), "{content}");

        // 尺寸 / 时长 / 流式（视频那三个都在 `inputVideo` 上）
        let content = content_json(
            &Media::video("/tmp/a.mp4")
                .size(640, 480)
                .duration(3)
                .streaming(true),
            &plain_text,
        );
        assert_eq!(content["video"]["width"], 640);
        assert_eq!(content["video"]["height"], 480);
        assert_eq!(content["video"]["duration"], 3);
        assert_eq!(content["video"]["supports_streaming"], true);

        // 音频：title / performer / duration
        let content = content_json(
            &Media::Audio {
                file: "/tmp/a.mp3".into(),
                caption: None,
                album_cover_thumbnail: None,
                duration: 0,
                title: String::new(),
                performer: String::new(),
            }
            .duration(12)
            .title("标题")
            .performer("作者"),
            &plain_text,
        );
        assert_eq!(content["audio"]["duration"], 12);
        assert_eq!(content["audio"]["title"], "标题");
        assert_eq!(content["audio"]["performer"], "作者");

        // 贴纸：emoji + 尺寸
        let content = content_json(
            &Media::Sticker {
                file: "/tmp/a.webp".into(),
                emoji: "🙂".to_owned(),
                thumbnail: None,
                width: 0,
                height: 0,
            }
            .emoji("😀")
            .size(512, 512),
            &plain_text,
        );
        assert_eq!(content["emoji"], "😀");
        assert_eq!(content["sticker"]["width"], 512);
        assert_eq!(content["sticker"]["height"], 512);

        // 设置器之间不互相抹掉：caption + thumbnail + spoiler 一起给
        let content = content_json(
            &Media::photo("/tmp/a.jpg")
                .spoiler(true)
                .thumbnail("/tmp/thumb.jpg")
                .caption("标题"),
            &plain_text,
        );
        assert_eq!(content["has_spoiler"], true);
        assert_eq!(content["photo"]["thumbnail"]["@type"], "inputThumbnail");
        assert_eq!(content["caption"]["text"], "标题");
    }

    /// Markdown / HTML 的转换**只有一条路**：交给 TDLib 的 `parseTextEntities`。
    ///
    /// 这里钉住三件事：模式到 `TextParseMode` 的映射（真 `td_api.tl` 的构造器名 + 版本号）、
    /// 请求本身过得了真 schema、以及拿不到结果时退回纯文本。
    #[test]
    fn parse_modes_map_to_the_real_tdlib_text_parse_modes() {
        let markdown = parse_text_request(&Text::markdown("*hi*")).unwrap();
        assert_eq!(markdown["@type"], "parseTextEntities");
        assert_eq!(markdown["text"], "*hi*");
        assert_eq!(markdown["parse_mode"]["@type"], "textParseModeMarkdown");
        assert_eq!(markdown["parse_mode"]["version"], 1);

        let v2 = parse_text_request(&Text::markdown_v2("**hi**")).unwrap();
        assert_eq!(v2["parse_mode"]["@type"], "textParseModeMarkdown");
        assert_eq!(v2["parse_mode"]["version"], 2);

        let html = parse_text_request(&Text::html("<b>hi</b>")).unwrap();
        assert_eq!(html["parse_mode"]["@type"], "textParseModeHTML");

        // 纯文本没有对应的 TextParseMode —— 不该白跑一趟
        assert!(parse_text_request(&Text::plain("hi")).is_none());

        let schema = fuyubot_tdlib_sys::TdApiSchema::compiled();
        for request in [markdown, v2, html] {
            let mut params = request.clone();
            params.as_object_mut().unwrap().remove("@type");
            schema
                .check_request("parseTextEntities", &params)
                .unwrap_or_else(|e| panic!("parseTextEntities 过不了真 schema：{e}\n{request}"));
        }
    }

    /// 解析结果落到 `formattedText`；拿不到就退回纯文本（Markdown 写错也不该发不出去）。
    #[test]
    fn formatted_text_comes_from_tdlib_or_falls_back_to_plain() {
        let text = Text::markdown("**hi**");
        let parsed = json!({
            "@type": "formattedText",
            "text": "hi",
            "entities": [{ "offset": 0, "length": 2, "type": { "@type": "textEntityTypeBold" } }],
        });

        // TDLib 解析成功：原样用它的结果（含 entities）
        let formatted = formatted_from(&text, Some(&parsed));
        assert_eq!(formatted, parsed);

        // 没接 TDLib / 超时：退回纯文本，原文一个字符都不丢
        let fallback = formatted_from(&text, None);
        assert_eq!(fallback["@type"], "formattedText");
        assert_eq!(fallback["text"], "**hi**");
        assert_eq!(fallback["entities"].as_array().unwrap().len(), 0);

        // TDLib 报错：同上
        let error = json!({ "@type": "error", "code": 400, "message": "Can't parse entities" });
        assert_eq!(formatted_from(&text, Some(&error))["text"], "**hi**");

        // 真的走一遍 Simplify：离线（没接 TDLib）时不 panic、按纯文本发出去
        let rt = runtime();
        let simplify = simplify(&rt);
        fire!(
            simplify,
            send_message,
            7,
            &quick!(SendMessageParams::text(Text::markdown("**hi**")))
        );
        let sent = rt.sent_requests();
        assert_eq!(
            sent[0].params["input_message_content"]["text"]["text"],
            "**hi**"
        );
        assert!(
            rt.log_lines()
                .iter()
                .any(|line| line.contains("按纯文本发")),
            "退回纯文本要留一条日志：{:?}",
            rt.log_lines()
        );
    }

    /// 编辑 / 相册 / 删除：方法名与参数名按真 `td_api.tl` 拼。
    #[test]
    fn edit_album_and_delete_build_the_real_requests() {
        let rt = runtime();
        let simplify = simplify(&rt);
        let markup = vec![vec![ReplyButton::callback("ok", "x")]];

        fire!(
            simplify,
            edit_message_text,
            7,
            9,
            &EditTextParams {
                text: Some(Text::markdown("*hi*")),
                link_preview: Some(false),
                reply_markup: markup.clone(),
                timeout_ms: 0,
            }
        );
        fire!(
            simplify,
            edit_message_caption,
            7,
            9,
            &EditCaptionParams {
                text: Some(Text::plain("新标题")),
                reply_markup: Vec::new(),
                timeout_ms: 0,
            }
        );
        fire!(
            simplify,
            edit_message_media,
            7,
            9,
            &EditMediaParams {
                text: None,
                media: Some(Media::photo("/tmp/a.jpg")),
                reply_markup: markup,
                timeout_ms: 0,
            }
        );
        fire!(
            simplify,
            send_message_album,
            7,
            &SendAlbumParams {
                medias: vec![Media::photo("/tmp/a.jpg"), Media::photo("/tmp/b.jpg")],
                caption: Some(Text::plain("一组图")),
                reply_to_message_id: Some(9),
                // 非论坛群组的评论串走 messageTopicThread（与上面的 forum 分支互补）
                topic_id: Some(Topic::thread(3)),
                timeout_ms: 0,
            }
        );
        fire!(simplify, delete_message, 7, [9, 10], true, 0);

        let sent = rt.sent_requests();
        let methods: Vec<&str> = sent.iter().map(|request| request.method.as_str()).collect();
        assert_eq!(
            methods,
            [
                "editMessageText",
                "editMessageCaption",
                "editMessageMedia",
                "sendMessageAlbum",
                "deleteMessages"
            ]
        );

        // editMessageText: chat_id message_id reply_markup input_message_content
        let text = &sent[0].params;
        assert_eq!(text["chat_id"], 7);
        assert_eq!(text["message_id"], 9);
        assert_eq!(text["reply_markup"]["@type"], "replyMarkupInlineKeyboard");
        assert_eq!(text["input_message_content"]["@type"], "inputMessageText");
        assert_eq!(
            text["input_message_content"]["link_preview_options"]["is_disabled"],
            true
        );

        // 标题在**顶层** caption（formattedText），不在 input_message_content 里
        let caption = &sent[1].params;
        assert_eq!(caption["caption"]["@type"], "formattedText");
        assert_eq!(caption["caption"]["text"], "新标题");
        assert!(caption.get("input_message_content").is_none());
        assert!(caption.get("reply_markup").is_none(), "没传就不该编一个");

        // 媒体内容与发消息同一套形状
        assert_eq!(
            sent[2].params["input_message_content"]["@type"],
            "inputMessagePhoto"
        );

        // 相册：input_message_contents 是数组，共用一段标题
        let album = &sent[3].params;
        assert_eq!(album["topic_id"]["@type"], "messageTopicThread");
        assert_eq!(album["topic_id"]["message_thread_id"], 3);
        assert_eq!(album["reply_to"]["message_id"], 9);
        let contents = album["input_message_contents"].as_array().unwrap();
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[0]["caption"]["text"], "一组图");
        assert_eq!(contents[1]["caption"]["text"], "一组图");

        // deleteMessages chat_id message_ids revoke
        let delete = &sent[4].params;
        assert_eq!(delete["message_ids"], json!([9, 10]));
        assert_eq!(delete["revoke"], true);
    }

    /// 相册的项目数有硬边界（TDLib 只认 2–10）。
    #[test]
    fn album_rejects_wrong_sizes() {
        let rt = runtime();
        let simplify = simplify(&rt);
        for count in [0usize, 1, 11] {
            let params = SendAlbumParams {
                medias: (0..count).map(|_| Media::photo("/tmp/a.jpg")).collect(),
                ..SendAlbumParams::default()
            };
            let error = simplify.send_message_album(7, &params).unwrap_err();
            assert!(matches!(error, SimplifyError::AlbumSize(n) if n == count));
        }
        assert!(
            rt.sent_requests().is_empty(),
            "不合法就不该发请求：{:?}",
            rt.sent_requests()
        );
    }

    /// 相册**发的时候带不上键盘**（真签名里没有 `reply_markup`），只能事后改：
    /// `editMessageReplyMarkup` 就是那条路，而它的 `reply_markup` 是必需字段——
    /// 空键盘按「移除键盘」发，不是省略。
    #[test]
    fn edit_reply_markup_is_how_an_album_gets_a_keyboard() {
        let rt = runtime();
        let simplify = simplify(&rt);

        fire!(
            simplify,
            edit_message_reply_markup,
            7,
            9,
            &[vec![ReplyButton::callback("ok", "x")]],
            0
        );
        fire!(simplify, edit_message_reply_markup, 7, 9, &[], 0);

        let sent = rt.sent_requests();
        assert_eq!(sent[0].method, "editMessageReplyMarkup");
        assert_eq!(sent[0].params["chat_id"], 7);
        assert_eq!(sent[0].params["message_id"], 9);
        assert_eq!(
            sent[0].params["reply_markup"]["@type"],
            "replyMarkupInlineKeyboard"
        );

        // 空键盘：真签名里 reply_markup 必填，所以发的是「移除键盘」
        assert_eq!(
            sent[1].params["reply_markup"]["@type"],
            "replyMarkupRemoveKeyboard"
        );
    }

    /// 模拟 TDLib：看到某个方法的请求就回一条响应，再逐条推更新。
    ///
    /// 顺序是 TDLib 真实的时间线：**先响应**（里面是还没发出去的临时消息），
    /// **再一条条结局更新**。
    fn tdlib_responder(
        rt: Arc<HostRuntime>,
        method: &'static str,
        response: Value,
        updates: Vec<Value>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let id = loop {
                let sent = rt.sent_requests();
                if let Some(request) = sent.iter().find(|r| r.method == method) {
                    break request.id;
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            // 让主线程先进 wait（waiter 是先注册订阅、再开始等的）
            std::thread::sleep(Duration::from_millis(60));
            let mut response = response;
            response["request_id"] = json!(id);
            rt.deliver_response(&response).unwrap();
            for update in updates {
                std::thread::sleep(Duration::from_millis(20));
                rt.publish_update(&update).unwrap();
            }
        })
    }

    /// 一条「发送成功」的更新：`old_message_id` 是临时消息 id，`message` 是最终消息。
    fn send_succeeded(old_message_id: i64, message_id: i64) -> Value {
        json!({
            "@type": "updateMessageSendSucceeded",
            "old_message_id": old_message_id,
            "message": { "@type": "message", "id": message_id, "chat_id": 7 },
        })
    }

    /// 一条「发送失败」的更新。
    fn send_failed(old_message_id: i64, code: i32, message: &str) -> Value {
        json!({
            "@type": "updateMessageSendFailed",
            "old_message_id": old_message_id,
            "error_code": code,
            "error_message": message,
            "message": { "@type": "message", "id": old_message_id, "chat_id": 7 },
        })
    }

    /// `send_message` 等到结局：响应 → 成功更新 → 返回最终 `Message`。
    #[test]
    fn send_message_returns_the_final_message() {
        let rt = runtime();
        let simplify = simplify(&rt);

        let responder = tdlib_responder(
            Arc::clone(&rt),
            "sendMessage",
            json!({ "@type": "message", "id": 11, "chat_id": 7 }),
            vec![send_succeeded(11, 111)],
        );

        let message = simplify
            .send_message(
                7,
                &SendMessageParams {
                    timeout_ms: 2_000,
                    ..SendMessageParams::text("hi")
                },
            )
            .unwrap();
        responder.join().unwrap();

        assert_eq!(message["id"], 111, "要的是最终消息，不是临时那条");
        assert_eq!(message["chat_id"], 7);
    }

    /// 发送失败：`updateMessageSendFailed` 变成 [`SimplifyError::SendFailed`]。
    #[test]
    fn send_message_reports_send_failed() {
        let rt = runtime();
        let simplify = simplify(&rt);

        let responder = tdlib_responder(
            Arc::clone(&rt),
            "sendMessage",
            json!({ "@type": "message", "id": 11, "chat_id": 7 }),
            vec![send_failed(11, 429, "Too Many Requests")],
        );

        let error = simplify
            .send_message(
                7,
                &SendMessageParams {
                    timeout_ms: 2_000,
                    ..SendMessageParams::text("hi")
                },
            )
            .unwrap_err();
        responder.join().unwrap();

        assert!(
            matches!(error, SimplifyError::SendFailed { code: 429, ref message } if message == "Too Many Requests"),
            "失败要连错误码一起带回来：{error:?}"
        );
    }

    /// 等到超时：先撤回那条还没发出去的消息，再返回 [`SimplifyError::Timeout`]。
    #[test]
    fn send_message_cancels_the_pending_message_on_timeout() {
        let rt = runtime();
        let simplify = simplify(&rt);

        // 只回响应（临时消息），不发任何结局更新
        let responder = tdlib_responder(
            Arc::clone(&rt),
            "sendMessage",
            json!({ "@type": "message", "id": 11, "chat_id": 7 }),
            Vec::new(),
        );

        let error = simplify
            .send_message(
                7,
                &SendMessageParams {
                    timeout_ms: 200,
                    ..SendMessageParams::text("hi")
                },
            )
            .unwrap_err();
        responder.join().unwrap();

        assert!(matches!(error, SimplifyError::Timeout(200)), "{error:?}");

        // 超时补救：把那条临时消息撤回来（revoke = true）
        let cancellation = rt
            .sent_requests()
            .into_iter()
            .find(|request| request.method == "deleteMessages")
            .expect("超时该撤回那条没发出去的消息");
        assert_eq!(cancellation.params["message_ids"], json!([11]));
        assert_eq!(cancellation.params["revoke"], true);
    }

    /// 相册返回的是 **`Messages`**（一组）：先收 `messages` 数组，
    /// 再按 `old_message_id` 逐条收结局；全部成功才返回。
    #[test]
    fn album_waits_for_every_item() {
        let rt = runtime();
        let simplify = simplify(&rt);

        let responder = tdlib_responder(
            Arc::clone(&rt),
            "sendMessageAlbum",
            json!({
                "@type": "messages",
                "total_count": 2,
                "messages": [
                    { "@type": "message", "id": 11, "chat_id": 7 },
                    { "@type": "message", "id": 12, "chat_id": 7 },
                ],
            }),
            vec![send_succeeded(11, 111), send_succeeded(12, 112)],
        );

        let messages = simplify
            .send_message_album(
                7,
                &SendAlbumParams {
                    medias: vec![Media::photo("/tmp/a.jpg"), Media::photo("/tmp/b.jpg")],
                    timeout_ms: 2_000,
                    ..SendAlbumParams::default()
                },
            )
            .unwrap();
        responder.join().unwrap();

        let ids: Vec<i64> = messages
            .iter()
            .map(|message| message["id"].as_i64().unwrap())
            .collect();
        assert_eq!(ids, vec![111, 112], "与 medias 同序的最终消息");
    }

    /// 相册里**任一项**失败就是失败。
    #[test]
    fn album_reports_the_first_failure() {
        let rt = runtime();
        let simplify = simplify(&rt);

        let responder = tdlib_responder(
            Arc::clone(&rt),
            "sendMessageAlbum",
            json!({
                "@type": "messages",
                "total_count": 2,
                "messages": [
                    { "@type": "message", "id": 11, "chat_id": 7 },
                    { "@type": "message", "id": 12, "chat_id": 7 },
                ],
            }),
            vec![send_succeeded(11, 111), send_failed(12, 400, "Bad file")],
        );

        let error = simplify
            .send_message_album(
                7,
                &SendAlbumParams {
                    medias: vec![Media::photo("/tmp/a.jpg"), Media::photo("/tmp/b.jpg")],
                    timeout_ms: 2_000,
                    ..SendAlbumParams::default()
                },
            )
            .unwrap_err();
        responder.join().unwrap();

        assert!(
            matches!(error, SimplifyError::SendFailed { code: 400, ref message } if message == "Bad file"),
            "{error:?}"
        );
    }

    /// 方法被 TDLib 拒了（响应是 `error`）：根本没进发送阶段。
    #[test]
    fn album_reports_the_tdlib_error() {
        let rt = runtime();
        let simplify = simplify(&rt);

        let responder = tdlib_responder(
            Arc::clone(&rt),
            "sendMessageAlbum",
            json!({ "@type": "error", "code": 400, "message": "Album is too big" }),
            Vec::new(),
        );

        let error = simplify
            .send_message_album(
                7,
                &SendAlbumParams {
                    medias: vec![Media::photo("/tmp/a.jpg"), Media::photo("/tmp/b.jpg")],
                    timeout_ms: 2_000,
                    ..SendAlbumParams::default()
                },
            )
            .unwrap_err();
        responder.join().unwrap();

        assert!(
            matches!(error, SimplifyError::Tdlib { code: 400, ref message } if message == "Album is too big"),
            "{error:?}"
        );
        assert!(
            !rt.sent_requests()
                .iter()
                .any(|request| request.method == "deleteMessages"),
            "没发出去就不该有撤回"
        );
    }

    /// 编辑没有「发送成功 / 失败」那一对更新：结果就是**响应**。
    #[test]
    fn edit_waits_for_the_response() {
        let rt = runtime();
        let simplify = simplify(&rt);

        // 成功：拿到改完的 Message
        let responder = tdlib_responder(
            Arc::clone(&rt),
            "editMessageText",
            json!({ "@type": "message", "id": 9, "chat_id": 7 }),
            Vec::new(),
        );
        let message = simplify
            .edit_message_text(
                7,
                9,
                &EditTextParams {
                    text: Some(Text::plain("改过")),
                    timeout_ms: 2_000,
                    ..EditTextParams::default()
                },
            )
            .unwrap();
        responder.join().unwrap();
        assert_eq!(message["id"], 9);
    }

    /// 编辑被 TDLib 拒了：`Message is not modified` 这类失败是**响应里的 error**。
    #[test]
    fn edit_reports_the_tdlib_error() {
        let rt = runtime();
        let simplify = simplify(&rt);

        let responder = tdlib_responder(
            Arc::clone(&rt),
            "editMessageMedia",
            json!({ "@type": "error", "code": 400, "message": "Message is not modified" }),
            Vec::new(),
        );
        let error = simplify
            .edit_message_media(
                7,
                9,
                &EditMediaParams {
                    media: Some(Media::photo("/tmp/a.jpg")),
                    timeout_ms: 2_000,
                    ..EditMediaParams::default()
                },
            )
            .unwrap_err();
        responder.join().unwrap();
        assert!(
            matches!(error, SimplifyError::Tdlib { code: 400, ref message } if message == "Message is not modified"),
            "{error:?}"
        );
    }

    /// 删除：等到响应，成功就是 `Ok(())`（TDLib 的 `ok` 没有内容）。
    #[test]
    fn delete_waits_for_the_response() {
        let rt = runtime();
        let simplify = simplify(&rt);

        let responder = tdlib_responder(
            Arc::clone(&rt),
            "deleteMessages",
            json!({ "@type": "ok" }),
            Vec::new(),
        );
        simplify.delete_message(7, [9, 10], true, 2_000).unwrap();
        responder.join().unwrap();
    }

    /// 回 callback query 失败（query 太老 / 已过期）：不再是静默的。
    #[test]
    fn answer_callback_query_reports_the_tdlib_error() {
        let rt = runtime();
        let simplify = simplify(&rt);

        let responder = tdlib_responder(
            Arc::clone(&rt),
            "answerCallbackQuery",
            json!({ "@type": "error", "code": 400, "message": "Query is too old" }),
            Vec::new(),
        );
        let error = simplify
            .answer_callback_query(555, Some("好的"), true, 2_000)
            .unwrap_err();
        responder.join().unwrap();
        assert!(
            matches!(error, SimplifyError::Tdlib { code: 400, .. }),
            "{error:?}"
        );
    }

    /// `*_then` 不阻塞：调用立刻返回，结局从另一个线程交给闭包。
    #[test]
    fn then_variants_hand_the_result_to_the_callback() {
        let rt = runtime();
        let simplify = simplify(&rt);

        let responder = tdlib_responder(
            Arc::clone(&rt),
            "sendMessage",
            json!({ "@type": "message", "id": 11, "chat_id": 7 }),
            vec![send_succeeded(11, 111)],
        );

        let (sender, receiver) = std::sync::mpsc::channel();
        let started = Instant::now();
        simplify.send_message_then(
            7,
            &SendMessageParams {
                timeout_ms: 2_000,
                ..SendMessageParams::text("hi")
            },
            move |result| {
                let _ = sender.send(result.map(|message| message["id"].as_i64().unwrap()));
            },
        );
        // 关键：调用线程没有等在这里
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "then 不该阻塞调用线程：{:?}",
            started.elapsed()
        );

        let got = receiver
            .recv_timeout(Duration::from_millis(2_000))
            .expect("结局该从另一个线程送回来");
        responder.join().unwrap();
        assert_eq!(got.unwrap(), 111);
    }

    #[test]
    fn wait_for_update_reports_timeout_and_match() {
        let rt = runtime();
        let simplify = simplify(&rt);

        // 超时：没人发更新
        let missed = simplify.wait_for_update_json(&UpdateFilter::new("updateNewMessage"), 40);
        assert!(missed.unwrap().is_none(), "没人发更新就该超时");

        // 匹配：另一个线程发一条
        let publisher = Arc::clone(&rt);
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            publisher
                .publish_update(&json!({"@type":"updateNewMessage","message":{"id":5}}))
                .unwrap();
        });
        let got = simplify
            .wait_for_update_json(&UpdateFilter::new("updateNewMessage"), 2_000)
            .unwrap();
        handle.join().unwrap();

        let got = got.expect("应该等到那条更新");
        assert_eq!(got["message"]["id"], 5);
    }

    #[test]
    fn update_filter_paths_depend_on_the_update_shape() {
        let wrapped = UpdateFilter::new("updateNewMessage")
            .chat_id(42)
            .sender_user_id(7)
            .message_thread_id(3)
            .to_json();
        assert_eq!(wrapped[FILTER_EVENT_TYPE_KEY], "updateNewMessage");
        assert_eq!(wrapped["message.chat_id"], 42);
        assert_eq!(wrapped["message.sender_user_id"], 7);
        assert_eq!(wrapped["message.message_thread_id"], 3);

        let flat = UpdateFilter::flat("updateNewCallbackQuery")
            .chat_id(42)
            .to_json();
        assert_eq!(flat[FILTER_EVENT_TYPE_KEY], "updateNewCallbackQuery");
        assert_eq!(flat["chat_id"], 42);
        assert!(flat.get("message.chat_id").is_none());

        let any = UpdateFilter::any().to_json();
        assert_eq!(any[FILTER_EVENT_TYPE_KEY], ABI_UPDATE_EVENT);
        assert_eq!(any.as_object().unwrap().len(), 1);
    }

    /// 内联键盘 + callback query 的关联。
    #[test]
    fn keyboard_and_callback_query_round_trip() {
        let rt = runtime();
        let simplify = simplify(&rt);

        fire!(
            simplify,
            send_message,
            7,
            &SendMessageParams {
                text: Some(Text::plain("选一个")),
                reply_markup: vec![vec![
                    ReplyButton::callback("A", "a"),
                    ReplyButton::callback("B", "b"),
                ]],
                timeout_ms: 0,
                ..SendMessageParams::default()
            }
        );

        // 模拟一条 callback query 更新：chat_id 在顶层，所以要 flat
        let payload = json!({
            "@type": "updateNewCallbackQuery",
            "id": 555,
            "chat_id": 7,
            "sender_user_id": 1,
        });
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let listener = {
            let seen = Arc::clone(&seen);
            rt.events().on(
                "updateNewCallbackQuery",
                Some(fuyubot_event::Filter::new().field("chat_id", 7i64)),
                move |payload| {
                    seen.lock().unwrap().push(payload.clone());
                    Ok(())
                },
            )
        };
        rt.publish_update(&payload).unwrap();
        assert_eq!(seen.lock().unwrap().len(), 1, "插件该收到回调查询");
        drop(listener);

        // 收到回调查询后回一条（离线环境拿不到响应，超时也算把请求发出去了）
        fire!(simplify, answer_callback_query, 555, Some("好的"), true, 0);

        let sent = rt.sent_requests();
        let answer = sent.last().unwrap();
        assert_eq!(answer.method, "answerCallbackQuery");
        assert_eq!(answer.params["callback_query_id"], 555);
        assert_eq!(answer.params["text"], "好的");
        assert_eq!(answer.params["show_alert"], true);

        // 发出去的那条消息确实带着键盘
        assert_eq!(
            sent[0].params["reply_markup"]["@type"],
            "replyMarkupInlineKeyboard"
        );
    }

    #[test]
    fn resolve_entities_fires_all_then_aggregates() {
        let rt = runtime();
        let simplify = simplify(&rt);
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));

        // 起一个后台线程：看到请求就逐个回响应（模拟 TDLib）
        let responder = {
            let rt = Arc::clone(&rt);
            let requests = Arc::clone(&requests);
            std::thread::spawn(move || {
                for _ in 0..8 {
                    std::thread::sleep(Duration::from_millis(10));
                    let pending: Vec<u64> = rt
                        .sent_requests()
                        .iter()
                        .filter(|request| {
                            request.method == "getChat" || request.method == "getUser"
                        })
                        .map(|request| request.id)
                        .filter(|id| !requests.lock().unwrap().contains(id))
                        .collect();
                    for id in pending {
                        requests.lock().unwrap().push(id);
                        rt.deliver_response(&json!({
                            "@type": "ok",
                            "@extra": id.to_string(),
                            "id": id,
                        }))
                        .unwrap();
                    }
                    if requests.lock().unwrap().len() >= 2 {
                        return;
                    }
                }
            })
        };

        let resolution = simplify
            .resolve_entities(vec![EntityId::Chat(7), EntityId::User(9)], 2_000)
            .unwrap();
        responder.join().unwrap();

        assert_eq!(resolution.entities.len(), 2);
        assert_eq!(resolution.entities[0].id, EntityId::Chat(7));
        assert_eq!(resolution.entities[1].id, EntityId::User(9));
        assert_eq!(resolution.entities[0].value["@type"], "ok");
        assert!(resolution.request_id.is_valid());

        let sent = rt.sent_requests();
        let methods: Vec<&str> = sent.iter().map(|request| request.method.as_str()).collect();
        assert_eq!(methods, ["getChat", "getUser"]);
    }

    #[test]
    fn download_file_reports_and_stops_at_completion() {
        let rt = runtime();
        let simplify = simplify(&rt);
        let progress = Arc::new(std::sync::Mutex::new(Vec::new()));

        let listener = {
            let progress = Arc::clone(&progress);
            rt.events().on("file_progress", None, move |payload| {
                progress.lock().unwrap().push(payload.clone());
                Ok(())
            })
        };

        let publisher = {
            let rt = Arc::clone(&rt);
            std::thread::spawn(move || {
                for (downloaded, done) in [(10u64, false), (20, false), (30, true)] {
                    std::thread::sleep(Duration::from_millis(10));
                    rt.publish_update(&json!({
                        "@type": "updateFile",
                        "file": {
                            "id": 99,
                            "expected_size": 30,
                            "local": {
                                "downloaded_size": downloaded,
                                "is_downloading_completed": done,
                            }
                        }
                    }))
                    .unwrap();
                }
            })
        };

        let download = simplify.download_file(99, 1, 2_000).unwrap();
        publisher.join().unwrap();
        drop(listener);

        assert!(download.completed, "应该等到完成");
        assert_eq!(download.file_id, 99);

        let progress = progress.lock().unwrap();
        assert_eq!(progress.len(), 3, "三条 updateFile 各推一个简化事件");
        assert_eq!(progress[0]["file_id"], 99);
        assert_eq!(progress[0]["downloaded_size"], 10);
        assert_eq!(progress[0]["completed"], false);
        assert_eq!(progress[2]["completed"], true);

        // 请求确实是 downloadFile
        let sent = rt.sent_requests();
        assert_eq!(sent[0].method, "downloadFile");
        assert_eq!(sent[0].params["file_id"], 99);
        assert_eq!(sent[0].params["priority"], 1);
    }

    /// 把 `Simplify` 真的拼出来的每一个请求，拿去和**真 `td_api.tl`** 烤出来的元数据对一遍。
    ///
    /// 这条离线就能跑（schema 是编译期烘进二进制的），而它挡的正是 gaps 缺口 11 里那半
    /// ——「方法名/参数名是按 TDLib 文档写的，没人验过」。`check_request` 会抓出
    /// 不存在的方法名、把构造器当方法调、以及**拼错的参数名**。
    ///
    /// 它验不了的是「真 Telegram 认不认这些值」（那要真账号+网络），别把这条当成端到端验收。
    #[test]
    fn every_request_simplify_builds_passes_the_real_schema() {
        let rt = runtime();
        let simplify = simplify(&rt);
        let markup = vec![vec![
            ReplyButton::callback("ok", "x").style(ButtonStyle::Success),
            ReplyButton::user("用户", 42),
            ReplyButton::web_app("应用", "https://example.com"),
        ]];

        // 尽可能多地把 Simplify 的请求拼法都走一遍（拿不到响应没关系，请求已经留档了）。
        fire!(
            simplify,
            send_message,
            7,
            &quick!(SendMessageParams::text("hi"))
        );
        fire!(
            simplify,
            send_message,
            7,
            &SendMessageParams {
                text: Some(Text::markdown_v2("*hi*")),
                media: None,
                reply_to_message_id: Some(9),
                topic_id: Some(Topic::forum(3)),
                reply_markup: markup.clone(),
                link_preview: Some(false),
                timeout_ms: 0,
            }
        );
        fire!(
            simplify,
            send_message,
            7,
            &SendMessageParams {
                text: Some(Text::plain("标题")),
                media: Some(Media::photo("/tmp/a.jpg")),
                timeout_ms: 0,
                ..SendMessageParams::default()
            }
        );
        fire!(
            simplify,
            send_message,
            7,
            &quick!(SendMessageParams::media(Media::document("/tmp/a.pdf")))
        );
        fire!(
            simplify,
            send_message,
            7,
            &quick!(SendMessageParams::media(
                Media::video("/tmp/a.mp4")
                    .caption(Text::plain("视频"))
                    // 缩略图 + 封面帧（从第 3 秒开始播）
                    .thumbnail(Thumbnail::new("/tmp/thumb.jpg").size(320, 240))
                    .cover("/tmp/cover.jpg", 3)
                    .size(640, 480)
                    .duration(3)
                    .streaming(true)
                    .spoiler(true)
            ))
        );
        fire!(
            simplify,
            send_message,
            7,
            &quick!(SendMessageParams::media(Media::Audio {
                file: "/tmp/a.mp3".into(),
                caption: None,
                album_cover_thumbnail: Some(Thumbnail::new("/tmp/cover.jpg").size(90, 90)),
                duration: 12,
                title: "标题".to_owned(),
                performer: "作者".to_owned(),
            }))
        );
        fire!(
            simplify,
            send_message,
            7,
            &quick!(SendMessageParams::media(
                Media::Animation {
                    file: "/tmp/a.gif".into(),
                    caption: None,
                    thumbnail: None,
                    duration: 1,
                    width: 10,
                    height: 10,
                    has_spoiler: false,
                }
                .thumbnail("/tmp/thumb.jpg")
            ))
        );
        fire!(
            simplify,
            send_message,
            7,
            &quick!(SendMessageParams::media(Media::Sticker {
                file: "/tmp/a.webp".into(),
                emoji: "😀".to_owned(),
                thumbnail: Some(Thumbnail::new("/tmp/thumb.webp").size(512, 512)),
                width: 512,
                height: 512,
            }))
        );
        fire!(
            simplify,
            send_message,
            7,
            &quick!(SendMessageParams::media(Media::location(1.5, 2.5)))
        );
        let _ = simplify.send_message_album(
            7,
            &SendAlbumParams {
                medias: vec![Media::photo("/tmp/a.jpg"), Media::photo("/tmp/b.jpg")],
                caption: Some(Text::plain("一组")),
                reply_to_message_id: Some(9),
                topic_id: Some(Topic::forum(3)),
                timeout_ms: 0,
            },
        );
        // 富消息：本地 md → blocks、服务端 md 源、以及编辑
        fire!(
            simplify,
            send_rich_message,
            7,
            &RichMessageParams {
                reply_to_message_id: Some(9),
                topic_id: Some(Topic::forum(3)),
                reply_markup: markup.clone(),
                clear_draft: true,
                timeout_ms: 0,
                ..RichMessageParams::markdown(
                    "# 标题\n\n**粗** 与 [链接](https://example.com)\n\n- 一\n- 二\n\n```rust\nfn main() {}\n```",
                )
            }
        );
        fire!(
            simplify,
            send_rich_message,
            7,
            &quick!(RichMessageParams::new(
                RichMessage::server_markdown("*hi*")
                    .with_media(vec![RichMedia::new("cover", Media::photo("/tmp/a.jpg"))]),
            ))
        );
        fire!(
            simplify,
            edit_rich_message,
            7,
            9,
            &quick!(RichMessageParams::markdown("改过"))
        );
        // 空富消息在本地就被挡住，不会白跑一次往返
        assert!(matches!(
            simplify.send_rich_message(7, &RichMessageParams::default()),
            Err(SimplifyError::EmptyRichMessage)
        ));
        fire!(
            simplify,
            edit_message_text,
            7,
            9,
            &EditTextParams {
                text: Some(Text::markdown_v2("*hi*")),
                link_preview: Some(false),
                reply_markup: markup.clone(),
                timeout_ms: 0,
            }
        );
        fire!(
            simplify,
            edit_message_caption,
            7,
            9,
            &EditCaptionParams {
                text: Some(Text::plain("标题")),
                reply_markup: Vec::new(),
                timeout_ms: 0,
            }
        );
        fire!(
            simplify,
            edit_message_media,
            7,
            9,
            &EditMediaParams {
                text: Some(Text::plain("新标题")),
                media: Some(Media::document("/tmp/a.pdf")),
                reply_markup: markup,
                timeout_ms: 0,
            }
        );
        fire!(
            simplify,
            edit_message_reply_markup,
            7,
            9,
            &[vec![ReplyButton::callback("ok", "x")]],
            0
        );
        // 空键盘 = 移除键盘：reply_markup 是必需字段，不能省
        fire!(simplify, edit_message_reply_markup, 7, 9, &[], 0);
        fire!(simplify, delete_message, 7, [9, 10], true, 0);
        fire!(simplify, answer_callback_query, 555, Some("好的"), true, 0);
        // 管理权 / 转发 / 表情反应：离线同样拿不到响应，但请求已经留档
        fire!(
            simplify,
            is_chat_admin,
            7,
            42,
            Some(AdminRight::DeleteMessages),
            0
        );
        fire!(simplify, am_i_chat_admin, 7, None, 0);
        fire!(
            simplify,
            forward_messages,
            7,
            8,
            &quick!(ForwardParams::new([2, 1]).topic_id(Topic::forum(3)))
        );
        fire!(simplify, react, 7, 9, &Reaction::emoji("👍"), false, 0);
        fire!(simplify, unreact, 7, 9, &Reaction::custom_emoji(5), 0);
        // bot 那条路（setMessageReactions）离线走不到（要先 getMe 才判定），
        // 它的形状由 chat::tests::new_request_shapes_pass_the_real_schema 单独守着。
        fire!(
            simplify,
            set_reactions,
            7,
            9,
            &[Reaction::emoji("👍"), Reaction::custom_emoji(1)],
            false,
            0
        );
        // 不等响应：超时也照样说明请求发出去了
        let _ = simplify.download_file(99, 1, 10);

        let schema = fuyubot_tdlib_sys::TdApiSchema::compiled();
        let sent = rt.sent_requests();
        assert!(!sent.is_empty(), "一条请求都没留档，测试没意义");

        for request in &sent {
            let mut params = request.params.clone();
            // `@extra` 是宿主注入的，不属于方法参数；`check_request` 只跳过 `@type`。
            if let Some(object) = params.as_object_mut() {
                object.remove("@extra");
            }
            schema
                .check_request(&request.method, &params)
                .unwrap_or_else(|error| {
                    panic!(
                        "Simplify 拼的 {} 过不了真 schema：{error}\n{params}",
                        request.method
                    )
                });
        }
        println!(
            "[schema] 校验了 {} 条请求，方法名与参数名全部对上",
            sent.len()
        );
    }

    /// `/start`、`/help@Bot 参数` 这类命令的解包（三个内置插件全靠它）。
    #[test]
    fn commands_are_extracted_from_new_messages() {
        let update = json!({
            "@type": "updateNewMessage",
            "message": {
                "id": 42,
                "chat_id": 7,
                "is_outgoing": false,
                "content": {
                    "@type": "messageText",
                    "text": {
                        "@type": "formattedText",
                        "text": "/Help@FuyuBot 参数 一",
                        "entities": [],
                    },
                },
            },
        });
        let command = command_of(&update).expect("该认出 /help");
        assert_eq!(command.chat_id, 7);
        assert_eq!(command.message_id, 42);
        assert_eq!(command.name, "help");
        assert_eq!(command.args, "参数 一");
        assert!(command.is("HELP"), "命令名不分大小写");
        assert!(!command.is("start"));

        // 自己发的不算（机器人不该响应自己）
        let mut outgoing = update.clone();
        outgoing["message"]["is_outgoing"] = json!(true);
        assert!(command_of(&outgoing).is_none());

        // 不带 `/` 的文本不算
        let plain = json!({
            "message": {
                "id": 1,
                "chat_id": 7,
                "content": {
                    "@type": "messageText",
                    "text": { "@type": "formattedText", "text": "你好", "entities": [] },
                },
            },
        });
        assert!(command_of(&plain).is_none());

        // 非文本消息（图片）不算
        let photo = json!({
            "message": {
                "id": 1,
                "chat_id": 7,
                "content": { "@type": "messagePhoto" },
            },
        });
        assert!(command_of(&photo).is_none());
    }

    #[test]
    fn base64_matches_the_known_vectors() {
        // TDLib 的 callback data 是 base64；用 RFC 4648 的样例校对
        assert_eq!(base64(""), "");
        assert_eq!(base64("f"), "Zg==");
        assert_eq!(base64("fo"), "Zm8=");
        assert_eq!(base64("foo"), "Zm9v");
        assert_eq!(base64("foob"), "Zm9vYg==");
        assert_eq!(base64("fooba"), "Zm9vYmE=");
        assert_eq!(base64("foobar"), "Zm9vYmFy");
    }
}
