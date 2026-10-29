# fuyubot-simplify

TDLib 便捷封装：把「发消息 / 相册 / 富消息 / 编辑消息 / 删除消息 / 转发 / 管理权 / 表情反应 / 连接状态 / 等更新 / 内联键盘 / 批量解析 / 下载进度」包成人话 API——**一个方法一个参数结构体 + 短字段名**（`SendMessageParams { text, media, reply_to_message_id, topic_id, reply_markup, link_preview, timeout_ms }`），调用方不拼 TDLib 的嵌套 JSON，请求经 `td_invoke*` 发出、返回的仍是 TDLib 原始 JSON。

[![](https://img.shields.io/badge/powered_by-dsh-4D6BFE?style=flat-square&logo=deepseek&logoColor=white)](https://github.com/deepseek-ai/deepseek-harness)

> ⚠️ **新代码优先 `fuyubot_abi::ResponseRouter`**（`invoke_simplified(..).await` / `wait_for_update(..).await`）。
> 本 crate 的方法是**同步阻塞**的，只适合同步上下文；事件回调里请用 `*_then`。

**每个方法都等到结果才返回**，不交回请求号：

- 发送类（`send_message` / `send_message_album` / `send_rich_message`）：等响应拿临时消息 → 等 `updateMessageSendSucceeded`（成功，给最终 `Message`）或 `updateMessageSendFailed`（→ `SimplifyError::SendFailed`）；超时先撤回未发出的消息再 `Timeout`。相册是 **N 条**结局，全成才算成。
- 其余（`edit_*` / `delete_message` / `answer_callback_query`）：结果是**响应**——`Message` / `ok` 是成功，`@type == "error"` → `SimplifyError::Tdlib`。
- ⚠️ 这些方法是**阻塞**的，而插件回调就在接收循环线程上——回调里请用 `*_then`（结果走闭包，并返回线程句柄，**`stop()` 里要 join**，否则卸载后线程还在跑库里的代码）。

## 身份、管理权、转发、反应、连接状态

这几样在 TDLib 里都「没有直觉上那个方法」，所以单独封一层：

话题（`topic_id`）不是裸 id，是 [`Topic`]：`Topic::forum(1)`（论坛群组 / 与 bot 的私聊，TDLib 的 `messageTopicForum`）、`Topic::thread(2)`（**非论坛**群组的评论串，`messageTopicThread`）、`Topic::direct_messages(..)` / `Topic::saved_messages(..)`。四者构造器与 id 类型都不同，所以**没有** `From<i64>`——逼调用方说清是哪一种。

- **管理权**：`user` 上连 `is_bot` 都没有（身份在 `user.type`），管理权在 `getChatMember` 的 `ChatMember.status` 里。`is_chat_admin(chat_id, user_id, right, timeout_ms)` / `am_i_chat_admin(chat_id, right, timeout_ms)` 直接返回 **`bool`**：`right: Option<AdminRight>` 为 `None` 只看身份（群主 / 管理员），`Some(…)` 还要求 `chatAdministratorRights` 里那一项为真（群主**拥有一切**，唯一的例外是 `IsAnonymous`——它在 creator 自己的 `is_anonymous` 上，不在 `rights` 里）。查不到（超时 / TDLib 报错 / 不是群聊）一律 `false`：不确定时说「没有」比说「有」安全。手上已经有 `ChatMember` / `updateChatMember` 时直接调纯函数 `is_admin_status(status, right)`，省一次往返。
- **转发**：`forward_messages(chat_id, from_chat_id, &ForwardParams { … })` 走 `forwardMessages`（一次一组，不是 Bot API 的 `forwardMessage`），`message_ids` 会**排序去重**（TDLib 要求严格递增、最多 100 条），返回的是还没发完的临时消息——这里只等**响应**，不等发送结局。
- **表情反应**：bot 与用户号**不是同一个方法**——用户用 `addMessageReaction`（追加一个），bot 只能用 `setMessageReactions`（整组覆盖，空列表 = 清空）。`react` / `unreact` 先 `getMe` 看 `user.type.@type == "userTypeBot"` 再挑对的那个；已经知道自己是哪种就直接调 `add_reaction` / `set_reactions`，省掉 `getMe` 那一趟。
- **连接状态**：TDLib 没有 `getConnectionState`，连接状态只以 `updateConnectionState` 的形式出现。`track_connection_state()` 订阅并**记住最新那条**（守卫 drop 即退订），之后 `is_ready()` / `connection()` / `state()` 随时可读、不发请求。订阅之前推过的更新补不回来（状态更新不重放），所以在插件 `start` 里尽早调。注意这是**连接**状态，不是**授权**状态——`connectionStateReady` 也可能还没登录。

`formattedText` 那条链路上的带标记文本（Markdown / HTML）**不自己解析**——经同步的 `td_execute` 直接问 TDLib 的 `parseTextEntities`。

发相册（`sendMessageAlbum`）返回的是 **`Messages`**（一组，不是单条）：先回一组临时消息，再**每一项各来一条** `updateMessageSendSucceeded`（`old_message_id` 就是临时消息 id）。`send_message_album_wait_success` 把这两步合成一次，结果在 `AlbumConfirmation { pending, messages, error }`——`pending` 是响应里的临时消息，`messages` 与它一一对应、同序，没等到的位置是 `Null`。

富消息（`messageRichMessage`）是另一套：TDLib 里没有 `sendRichMessage` / `editRichMessage`，也没有本地的 md → 块树转换（md/html 源由**服务端**解析且**仅 bot** 可用）。所以：

- 发送 / 编辑 = `send_rich_message` / `edit_rich_message`（底层就是 `sendMessage` / `editMessageText` + `inputMessageRichMessage`）；
- 三种源都在 `RichMessage` 上：`from_markdown`（**本地**转换，任何账号可发）、`server_markdown` / `server_html`（服务端解析，仅 bot）、`from_blocks`（直接给块树）；
- 本地转换器**对齐 Bot API 的 Rich Markdown 全表**（<https://core.telegram.org/bots/api/rich-messages>）：标题 / 段落 / 围栏代码（含 `math`）/ 引用与可展开引用 / 拉引 / 页脚 / 列表与任务项 / 表格 / 分割线 / 公式 / 折叠块 / 拼贴与幻灯片 / 媒体（按扩展名猜类型）/ 地图 / 锚点 / 按钮行 / 思考占位 / 脚注，行内粗斜删码、标记、剧透、上下标、行内公式、自定义 emoji、时间、mailto/tel/tg://user/锚点链接，以及文档里允许的那批 HTML 标签（`<u>` `<details>` `<tg-collage>` `<tg-button-row>` …）。不认的（引用式链接、`<table>` 这类 HTML 块）一律当普通文本；
- 按钮在 Markdown 里**没有语法**，所以是显式的：`RichButton::url / callback / user / copy_text` + `with_buttons` / `with_button_row` 追加 `inputPageBlockButtonRow`（只对 Blocks 源成立，md/html 源会报 `RichSourceNotBlocks`）；
- `InputPageBlock` 一共 24 个构造器，本地转换器**全都能产出**（`thinking` 只能出现在草稿消息里，结构上仍可构造）；`with_block` 留给「媒体已经上传、要传 `inputPhoto` 句柄」这类 md 表达不了的场景。分工表在 `input_page_block_coverage_is_accounted_for` 测试里，加块/删块会让它失败；
- 块级语法**按官方 Rich Markdown 表一类一例**在 `block_syntax_covers_the_official_table` 里断言（每一块都会再走一遍 `td_api::InputPageBlock` 反序列化），整段全格式的回归闸是内联的 `official_sample_fully_deserializes`；
- 行内表达式是独立的 `RichText`（31 个构造器、没有 offset/length）：Markdown 覆盖 21 个，剩下 10 个（`mention` / `hashtag` / `cashtag` / `bankCardNumber` / `tonAddress` / `botCommand` / `icon` / `button` / `diff` / `anchor`）没有 md 语法，用 `richtext::*` 拼好塞进块。31 个全部有反序列化测试。

## `ops`：同一套语义的 JSON 入口（ABI v5）

`ops` 模块把上面的语义做成**一张 op 表**：`run(&simplify, &json!({"op":"…","args":{…}}))`。

- **宿主**（`fuyubot-loader`）用它实现 ABI v5 的 `simplify_invoke` / `simplify_invoke_result`，所以
  Go / C / JS / TS / Python 插件调的是**同一份实现**——不存在「Rust 一套、别的语言另一套」。
- **Rust 插件**也能直接调它，但它更该用上面的类型化方法（有编译期检查）。
- 同步 op（`parse_text_entities` / `rich_from_markdown` / `list_ops`）不碰网络，回调里安全；
  其余要等 TDLib 应答，只能走 `simplify_invoke`。
- 加操作 = 往 `SYNC_OPS` / `ASYNC_OPS` 里加一个名字 + 一个分支，**不动 ABI**。
