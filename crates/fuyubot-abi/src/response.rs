//! 可 `await` 的响应 / 更新等待（缺口 10 的 async 形态）。
//!
//! `wait_for_update` 是**阻塞**原语：调用线程停下来等。要 JS `async/await` 那种语义，
//! 需要一个能挂起、之后被唤醒的 `Future`——这里就是它，**不改 C ABI 布局**：
//!
//! ```ignore
//! let router = ResponseRouter::new(host)?;      // 插件 start 里建一次，活到 stop
//!
//! // 等一条请求自己的响应
//! let (id, fut) = router.invoke_simplified("getMe", "{}")?;
//! let response = tokio::time::timeout(Duration::from_secs(5), fut).await??;
//!
//! // 等一条任意更新（async 版 wait_for_update）
//! let update = router.wait_for_update(json!({"@type": "updateNewMessage"}))?;
//! let update = tokio::time::timeout(Duration::from_secs(30), update).await??;
//! ```
//!
//! 设计要点：
//!
//! - **一个插件一个路由器**：常驻两条订阅——`"response"`（按 owner 隔离，只收本插件
//!   自己的响应）与 `"update"`（更新本来就是全局广播，所有更新等待者共用这条）。
//!   不是「每个请求/每次等待一条订阅」，省掉注册/注销与 `user_data` 生命周期问题。
//! - **先发后登记**的竞态由一个小缓冲兜住：`td_invoke*` 返回 id 之前响应理论上可能已经
//!   回来；回调找不到等待者就先塞进 `recent`，`invoke*` 拿到 id 后先查缓冲再登记。
//! - **取消 = drop future**：等待者是 `Arc`，路由器只存 `Weak`；future 一 drop，响应回来时
//!   升级失败、直接丢弃。没有 `cancel_wait` 那种「句柄要等调用返回才拿得到」的问题。
//! - **超时不内建**：用调用方运行时的 `timeout` 包一层即可（`tokio::time::timeout` 等），
//!   这样这个 crate 不需要拖 tokio。
//!
//! ## 什么时候用 / 别怎么用
//!
//! 这层是给**插件自己拥有 executor** 的场景（`tokio::spawn` / `block_on` 都行）：
//!
//! ```ignore
//! // start() 里：拿一个后台线程跑 tokio，回调里 spawn 任务，别在宿主回调里 await
//! let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
//! let handle = rt.spawn(async move { let r = fut.await; /* ... */ });
//! ```
//!
//! 仍然**不能**在宿主的事件回调里 `.await`——那个线程就是接收线程，跟阻塞版一样会自己
//! 等自己。区别是：在这里你可以把 future 交给另一个线程，回调立刻返回；阻塞版做不到。
//!
//! ## 边界
//!
//! - `ResponseRouter` 要在宿主**停止投递之后**才 drop（正常是插件 `stop` 里；`fuyubot run`
//!   的停机顺序是「停接收循环 → join → 卸载插件」，满足这个前提）。drop 时会先退订、
//!   再唤醒所有没完成的等待者（`RouterClosed`），不会留下悬挂 future。
//! - 返回的是 TDLib 的**原始响应**：`{"@type":"error",...}` 也是 `Ok`，用
//!   [`TdResponse`] 风格的判断自己看 `@type`（和宿主 `log_in_with` 一致）。
//! - 只匹配带数值 `request_id` 的响应（宿主 `deliver_response` 会给每条带 `@extra` 的
//!   响应补上这个字段）。

use std::collections::{HashMap, VecDeque};
use std::ffi::CStr;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::task::{Context, Poll, Waker};

use serde_json::Value;

use crate::{AbiError, HostApi, RESPONSE_EVENT, RequestId, SubscriptionHandle, UPDATE_EVENT};

/// 竞态缓冲的容量：只存「响应先到、等待者还没登记」的一小段时间内的响应。
const RECENT_CAPACITY: usize = 64;

/// 宿主的内置作用域服务：`invoke` 在插件自己的线程上跑时，用它把请求归属切回本插件。
const SCOPE_SERVICE: &str = "fuyubot.host.scope";

/// 一个插件的响应路由器。`Clone` 共享同一份状态（可以放进 `Arc` 分给多个任务）。
#[derive(Clone)]
pub struct ResponseRouter {
    host: HostApi,
    response_subscription: SubscriptionHandle,
    /// `"update"` 通道的常驻订阅：`wait_for_update` 的等待者共用它。
    update_subscription: SubscriptionHandle,
    /// 本插件的一个子作用域（`scope_create` 在插件 `start` 里建）。`invoke` 在
    /// 插件自己的 executor 线程上执行时，靠它把 owner 切回插件，否则请求会记成宿主
    /// （owner 0），响应按全局投递、scoped 订阅收不到。
    scope: Option<u64>,
    state: Arc<RouterState>,
}

impl ResponseRouter {
    /// 建路由器：注册**一条**带归属的 `"response"` 订阅。每个插件建一个，活到 `stop`。
    ///
    /// **在插件 `start` 里调**：这时宿主作用域栈顶是本插件，订阅才会按本插件归属；
    /// 同时用内置 scope service 建一个子作用域，供 `invoke` 在插件自己的线程上把归属
    /// 切回来（见 [`invoke`](Self::invoke)）。
    ///
    /// 若宿主没有 scope service（老宿主 / 测试替身），`scope` 为 `None`：`invoke` 仍能
    /// 用，但只能在**已经在插件作用域里的线程**（宿主回调 / `start`）上发；插件自己的
    /// executor 线程发的请求会记成 owner 0，响应投递不到 scoped 订阅。新版宿主都有它。
    pub fn new(host: HostApi) -> Result<Self, AbiError> {
        let state = Arc::new(RouterState::default());
        // 回调把 `state` 当裸指针用；`ResponseRouter` 持有 `Arc` 并在 drop 时先退订，
        // 所以只要遵守「宿主停止投递后才 drop」的契约，指针就一直有效。
        let raw = Arc::as_ptr(&state) as *mut core::ffi::c_void;

        let scope = host
            .call_service(SCOPE_SERVICE, "scope_create", "{}")
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|value| value.get("handle").and_then(Value::as_u64));

        let response_subscription = host.on(RESPONSE_EVENT, None, on_response, raw);
        if !response_subscription.is_valid() {
            if let Some(scope) = scope {
                let _ = host.call_service(
                    SCOPE_SERVICE,
                    "scope_drop",
                    &format!(r#"{{"handle":{scope}}}"#),
                );
            }
            return Err(AbiError::SubscriptionRejected);
        }
        // 更新通道用一条常驻订阅，所有 `wait_for_update` 等待者共用（不是每等一次订阅一遍）。
        // 更新是全局广播（`emit`），归属不影响能不能收到。
        let update_subscription = host.on(UPDATE_EVENT, None, on_update, raw);
        if !update_subscription.is_valid() {
            host.off(response_subscription);
            if let Some(scope) = scope {
                let _ = host.call_service(
                    SCOPE_SERVICE,
                    "scope_drop",
                    &format!(r#"{{"handle":{scope}}}"#),
                );
            }
            return Err(AbiError::SubscriptionRejected);
        }
        Ok(Self {
            host,
            response_subscription,
            update_subscription,
            scope,
            state,
        })
    }

    /// 发一条完整请求（与 `td_invoke` 相同的 JSON）并返回可 await 的 future。
    ///
    /// 发送之后、登记之前响应若已经回来，会从 `recent` 缓冲里补上，不会丢。
    pub fn invoke(&self, request_json: &str) -> Result<(RequestId, ResponseFuture), AbiError> {
        self.invoke_with(|host| host.td_invoke(request_json))
    }

    /// `td_invoke_simplified` 的 async 版。
    pub fn invoke_simplified(
        &self,
        method: &str,
        params_json: &str,
    ) -> Result<(RequestId, ResponseFuture), AbiError> {
        self.invoke_with(|host| host.td_invoke_simplified(method, params_json))
    }

    /// 等一条匹配的更新（`wait_for_update` 的 async 版，**不阻塞**）。
    ///
    /// 过滤条件与阻塞版同形：`{"event_type": "updateNewMessage", "message.chat.id": -100…}`。
    /// 只覆盖 TDLib 更新（`"update"` 通道 / 具体 `@type`）；响应请用
    /// [`invoke`](Self::invoke)，其它自定义事件目前只能走回调订阅。
    ///
    /// 等不到就永远 pending——超时用调用方运行时的 `timeout` 包一层；drop future 即取消。
    pub fn wait_for_update(&self, filter: Value) -> Result<UpdateFuture, AbiError> {
        if self.state.closed() {
            return Err(AbiError::RouterClosed);
        }
        let filter = UpdateFilter::parse(&filter)?;
        let waiter = Arc::new(Waiter::default());
        {
            let mut inner = self.state.lock();
            if self.state.closed() {
                return Err(AbiError::RouterClosed);
            }
            inner.updates.push(UpdateEntry {
                waiter: Arc::downgrade(&waiter),
                filter,
            });
        }
        Ok(UpdateFuture { waiter })
    }

    fn invoke_with(
        &self,
        send: impl FnOnce(&HostApi) -> Result<RequestId, AbiError>,
    ) -> Result<(RequestId, ResponseFuture), AbiError> {
        if self.state.closed() {
            return Err(AbiError::RouterClosed);
        }
        let waiter = Arc::new(Waiter::default());
        // 插件自己的 executor 线程不在宿主的作用域栈里：先 enter 本插件的子作用域，
        // `td_invoke*` 才会把请求记到插件名下（响应才会投到 scoped 订阅）。发送完立刻 leave。
        let entered = self.enter_scope()?;
        let outcome = send(&self.host);
        if entered {
            let _ = self.host.call_service(SCOPE_SERVICE, "scope_leave", "{}");
        }
        let id = outcome?;
        // 宿主拒绝请求时 `td_invoke*` 返回 `RequestId::INVALID`（0），不是 Err：
        // 这里不能把一个永远等不到的请求号登记成等待者。
        if !id.is_valid() {
            return Err(AbiError::RequestRejected);
        }
        // 先发后登记：这段时间回来的响应在 `recent` 里。查缓冲 + 登记在同一把锁里，
        // 回调要么看到等待者，要么把响应塞进缓冲——不存在两边都错过的窗口。
        let buffered = {
            let mut inner = self.state.lock();
            // 发送期间路由器可能被关掉（`close_and_wake` 先置位、再拿锁）：在锁里再查一次，
            // 避免插入一个没人会唤醒的等待者。
            if self.state.closed() {
                return Err(AbiError::RouterClosed);
            }
            let buffered = take_recent(&mut inner.recent, id.id);
            inner.waiters.insert(id.id, Arc::downgrade(&waiter));
            buffered
        };
        if let Some(response) = buffered {
            waiter.complete(Ok(response));
        }
        Ok((id, ResponseFuture { waiter }))
    }
}

impl ResponseRouter {
    /// 进入本插件的子作用域；没有 scope service 时返回 `false`（直接发）。
    fn enter_scope(&self) -> Result<bool, AbiError> {
        let Some(scope) = self.scope else {
            return Ok(false);
        };
        let input = format!(r#"{{"handle":{scope}}}"#);
        match self.host.call_service(SCOPE_SERVICE, "scope_enter", &input) {
            Ok(_) => Ok(true),
            Err(_) => Err(AbiError::ScopeUnavailable),
        }
    }
}

impl Drop for ResponseRouter {
    fn drop(&mut self) {
        // 只在最后一个克隆（通常就是插件自己）drop 时退订与收尾。
        if Arc::strong_count(&self.state) != 1 {
            return;
        }
        self.host.off(self.response_subscription);
        self.host.off(self.update_subscription);
        if let Some(scope) = self.scope {
            let _ = self.host.call_service(
                SCOPE_SERVICE,
                "scope_drop",
                &format!(r#"{{"handle":{scope}}}"#),
            );
        }
        self.state.close_and_wake();
    }
}

impl std::fmt::Debug for ResponseRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.state.lock();
        f.debug_struct("ResponseRouter")
            .field("response_subscription", &self.response_subscription)
            .field("update_subscription", &self.update_subscription)
            .field("waiters", &inner.waiters.len())
            .field("updates", &inner.updates.len())
            .finish()
    }
}

/// `invoke*` 返回的 future。
///
/// `Output = Result<Value, AbiError>`：只有「路由器被关闭」这类**传输层**错误才是 `Err`；
/// TDLib 的业务错误（`@type: "error"`）原样放在 `Ok` 的 JSON 里。
#[must_use = "future 不 await 也不会发出去等；drop 掉等于放弃这次等待"]
pub struct ResponseFuture {
    /// 路由器只存 `Weak` 指向它；future 一 drop，响应回来也找不到人，自然丢弃。
    waiter: Arc<Waiter>,
}

impl Future for ResponseFuture {
    type Output = Result<Value, AbiError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.waiter.lock();
        if let Some(result) = state.result.take() {
            return Poll::Ready(result);
        }
        state.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl std::fmt::Debug for ResponseFuture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponseFuture").finish_non_exhaustive()
    }
}

/// [`ResponseRouter::wait_for_update`] 返回的 future（与响应共用同一个等待状态机）。
pub type UpdateFuture = ResponseFuture;

/// 路由器内部状态。一把锁同时护住 `waiters` 与 `recent`：登记与「补缓冲」必须是原子的。
#[derive(Default)]
struct RouterState {
    inner: Mutex<RouterInner>,
    closed: std::sync::atomic::AtomicBool,
}

#[derive(Default)]
struct RouterInner {
    waiters: HashMap<u64, Weak<Waiter>>,
    recent: VecDeque<(u64, Value)>,
    /// `wait_for_update` 的等待者（按登记顺序；命中一个事件就摘掉）。
    updates: Vec<UpdateEntry>,
}

/// 一个 update 等待者 + 它的过滤条件。
struct UpdateEntry {
    waiter: Weak<Waiter>,
    filter: UpdateFilter,
}

/// `wait_for_update` 的过滤条件（async 版）。
///
/// 语义与阻塞版对齐：`event_type` 选通道（缺省 `"update"`；写具体 `@type` 也认，
/// 例如 `"updateNewMessage"`），其余键按点路径等值匹配。`"response"` 请用
/// [`ResponseRouter::invoke`]——更新订阅收不到响应。
struct UpdateFilter {
    event_type: Option<String>,
    fields: Vec<(String, Value)>,
}

impl UpdateFilter {
    fn parse(filter: &Value) -> Result<Self, AbiError> {
        let object = filter
            .as_object()
            .ok_or_else(|| AbiError::BadFilter("过滤条件必须是 JSON 对象".to_owned()))?;
        let event_type = object
            .get(crate::FILTER_EVENT_TYPE_KEY)
            .and_then(Value::as_str)
            .map(str::to_owned);
        if event_type.as_deref() == Some(RESPONSE_EVENT) {
            return Err(AbiError::UnsupportedChannel("response".to_owned()));
        }
        let fields = object
            .iter()
            .filter(|(key, _)| key.as_str() != crate::FILTER_EVENT_TYPE_KEY)
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        Ok(Self { event_type, fields })
    }

    fn matches(&self, payload: &Value) -> bool {
        if let Some(expected) = &self.event_type
            && expected != UPDATE_EVENT
            && payload.get("@type").and_then(Value::as_str) != Some(expected.as_str())
        {
            return false;
        }
        self.fields.iter().all(|(path, expected)| {
            let pointer = format!("/{}", path.replace('.', "/"));
            payload.pointer(&pointer) == Some(expected)
        })
    }
}

impl RouterState {
    fn lock(&self) -> MutexGuard<'_, RouterInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 关闭：把所有没完成的等待者叫醒（错误），之后不再接受新请求。
    fn close_and_wake(&self) {
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        let (responses, updates): (Vec<Arc<Waiter>>, Vec<Arc<Waiter>>) = {
            let mut inner = self.lock();
            let responses = inner
                .waiters
                .drain()
                .filter_map(|(_, waiter)| waiter.upgrade())
                .collect();
            let updates = inner
                .updates
                .drain(..)
                .filter_map(|entry| entry.waiter.upgrade())
                .collect();
            (responses, updates)
        };
        for waiter in responses.into_iter().chain(updates) {
            waiter.complete(Err(AbiError::RouterClosed));
        }
    }
}

#[derive(Default)]
struct Waiter {
    state: Mutex<WaiterState>,
}

#[derive(Default)]
struct WaiterState {
    result: Option<Result<Value, AbiError>>,
    waker: Option<Waker>,
}

impl Waiter {
    fn lock(&self) -> MutexGuard<'_, WaiterState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 填结果并唤醒；重复调用只保留第一个结果。
    fn complete(&self, result: Result<Value, AbiError>) {
        let waker = {
            let mut state = self.lock();
            if state.result.is_some() {
                return;
            }
            state.result = Some(result);
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

/// `"response"` 回调：按请求号找等待者；找不到就先放缓冲（竞态窗口）。
unsafe extern "C-unwind" fn on_response(
    user: *mut core::ffi::c_void,
    payload: *const core::ffi::c_char,
) -> i32 {
    if user.is_null() || payload.is_null() {
        return 0;
    }
    // SAFETY: `user` 是 `ResponseRouter::new` 里那个 `Arc<RouterState>` 的裸指针，
    // 路由器在 drop 前会退订（ABI 契约：宿主停止投递后才 drop）。
    let state = unsafe { &*(user as *const RouterState) };
    // SAFETY: 宿主保证 payload 是 NUL 结尾、只在下一次宿主调用前有效；这里立刻解析。
    let text = unsafe { CStr::from_ptr(payload) }.to_string_lossy();
    let Ok(response) = serde_json::from_str::<Value>(&text) else {
        return 0;
    };
    let Some(id) = response.get("request_id").and_then(Value::as_u64) else {
        return 0;
    };

    let waiter = {
        let mut inner = state.lock();
        match inner.waiters.remove(&id) {
            Some(waiter) => waiter.upgrade(),
            None => {
                // 响应比登记先到（或这是一个没人等的 fire-and-forget 响应）：兜住它。
                if inner.recent.len() >= RECENT_CAPACITY {
                    inner.recent.pop_front();
                }
                inner.recent.push_back((id, response.clone()));
                None
            }
        }
    };
    if let Some(waiter) = waiter {
        waiter.complete(Ok(response));
    }
    0
}

/// `"update"` 回调：把事件交给所有匹配的 `wait_for_update` 等待者（命中即摘）。
unsafe extern "C-unwind" fn on_update(
    user: *mut core::ffi::c_void,
    payload: *const core::ffi::c_char,
) -> i32 {
    if user.is_null() || payload.is_null() {
        return 0;
    }
    // SAFETY: 同 `on_response`。
    let state = unsafe { &*(user as *const RouterState) };
    // SAFETY: 宿主保证 payload 是 NUL 结尾、只在下一次宿主调用前有效；这里立刻解析。
    let text = unsafe { CStr::from_ptr(payload) }.to_string_lossy();
    let Ok(update) = serde_json::from_str::<Value>(&text) else {
        return 0;
    };

    let mut matched = Vec::new();
    {
        let mut inner = state.lock();
        inner.updates.retain(|entry| match entry.waiter.upgrade() {
            Some(waiter) if entry.filter.matches(&update) => {
                matched.push(waiter);
                false
            }
            Some(_) => true,
            // future 已经 drop：不用再留。
            None => false,
        });
    }
    for waiter in matched {
        waiter.complete(Ok(update.clone()));
    }
    0
}

fn take_recent(recent: &mut VecDeque<(u64, Value)>, id: u64) -> Option<Value> {
    let position = recent.iter().position(|(candidate, _)| *candidate == id)?;
    recent.remove(position).map(|(_, value)| value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ABI_VERSION, EventCallbackFn, HostApiVTable, ServiceHandle, ServiceVTable, ToolHandle,
    };
    use std::ffi::CString;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::task::Waker;

    // ── 假宿主：只实现 on / off / td_invoke_simplified，其余给空实现 ──────────
    //
    // 这些测试不依赖 loader（`abi` 不该反向依赖宿主层），所以自己搭一个够用的 VTable。

    /// 这些测试共用全局假宿主状态，必须串行跑（cargo test 默认并行）。
    static TEST_LOCK: Mutex<()> = Mutex::new(());
    /// channel → 订阅（路由器会同时订 `"response"` 与 `"update"` 两条）。
    static SUBSCRIPTIONS: Mutex<Vec<(u64, StoredSubscription)>> = Mutex::new(Vec::new());
    static NEXT_SUB: AtomicU64 = AtomicU64::new(1);
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    /// 1 = `td_invoke_simplified` 里立刻回调（模拟「响应比发送返回还快」的竞态）。
    static FIRE_ON_SEND: AtomicUsize = AtomicUsize::new(0);
    static UNSUBSCRIBED: AtomicUsize = AtomicUsize::new(0);

    struct StoredSubscription {
        event: String,
        callback: EventCallbackFn,
        user_data: *mut core::ffi::c_void,
    }

    // SAFETY: 测试是单线程（每个用例串行执行），裸指针只在这个进程里用。
    unsafe impl Send for StoredSubscription {}

    fn reset_fake() {
        SUBSCRIPTIONS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        UNSUBSCRIBED.store(0, Ordering::SeqCst);
    }

    unsafe extern "C" fn fake_on(
        _host: *mut core::ffi::c_void,
        event: *const core::ffi::c_char,
        _filter: *const core::ffi::c_char,
        callback: Option<EventCallbackFn>,
        user_data: *mut core::ffi::c_void,
    ) -> SubscriptionHandle {
        let Some(callback) = callback else {
            return SubscriptionHandle::INVALID;
        };
        // SAFETY: 宿主契约保证 event 是 NUL 结尾字符串。
        let event = unsafe { CStr::from_ptr(event) }
            .to_string_lossy()
            .into_owned();
        let id = NEXT_SUB.fetch_add(1, Ordering::SeqCst);
        SUBSCRIPTIONS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((
                id,
                StoredSubscription {
                    event,
                    callback,
                    user_data,
                },
            ));
        SubscriptionHandle { id }
    }

    unsafe extern "C" fn fake_off(_host: *mut core::ffi::c_void, handle: SubscriptionHandle) {
        UNSUBSCRIBED.fetch_add(1, Ordering::SeqCst);
        SUBSCRIPTIONS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(id, _)| *id != handle.id);
    }

    unsafe extern "C" fn fake_td_invoke_simplified(
        _host: *mut core::ffi::c_void,
        _method: *const core::ffi::c_char,
        _params: *const core::ffi::c_char,
    ) -> RequestId {
        let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        if FIRE_ON_SEND.load(Ordering::SeqCst) != 0 {
            fire(
                RESPONSE_EVENT,
                serde_json::json!({"@type": "ok", "request_id": id, "value": "early"}),
            );
        }
        RequestId { id }
    }

    fn fire(event: &str, payload: Value) {
        let target = {
            let subs = SUBSCRIPTIONS.lock().unwrap_or_else(|e| e.into_inner());
            subs.iter()
                .map(|(_, stored)| stored)
                .find(|stored| stored.event == event)
                .map(|stored| (stored.callback, stored.user_data))
        };
        if let Some((callback, user_data)) = target {
            let text = CString::new(payload.to_string()).unwrap();
            // SAFETY: 回调与 user_data 由 fake_on 存下；payload 在调用期间存活。
            unsafe { callback(user_data, text.as_ptr()) };
        }
    }

    // 其余字段的空实现：`HostApi::from_raw` 只校验必需的非空，不会调用它们。
    unsafe extern "C" fn noop_emit(
        _: *mut core::ffi::c_void,
        _: *const core::ffi::c_char,
        _: *const core::ffi::c_char,
    ) -> i32 {
        0
    }
    unsafe extern "C" fn noop_register_tool(
        _: *mut core::ffi::c_void,
        _: *const core::ffi::c_char,
        _: Option<crate::ToolHandlerFn>,
        _: *mut core::ffi::c_void,
    ) -> ToolHandle {
        ToolHandle::INVALID
    }
    unsafe extern "C" fn noop_call_tool(
        _: *mut core::ffi::c_void,
        _: *const core::ffi::c_char,
        _: *const core::ffi::c_char,
    ) -> RequestId {
        RequestId::INVALID
    }
    unsafe extern "C" fn noop_call_tool_result(
        _: *mut core::ffi::c_void,
        _: *const core::ffi::c_char,
        _: *const core::ffi::c_char,
    ) -> *const core::ffi::c_char {
        core::ptr::null()
    }
    unsafe extern "C" fn noop_list(_: *mut core::ffi::c_void) -> *const core::ffi::c_char {
        core::ptr::null()
    }
    unsafe extern "C" fn noop_provide(
        _: *mut core::ffi::c_void,
        _: *const core::ffi::c_char,
        _: *const ServiceVTable,
    ) -> ServiceHandle {
        ServiceHandle::INVALID
    }
    /// `provide_service_shared` 的桩：测试里两者行为一样，区别只在宿主的落点。
    unsafe extern "C" fn noop_provide_shared(
        _: *mut core::ffi::c_void,
        _: *const core::ffi::c_char,
        _: *const ServiceVTable,
    ) -> ServiceHandle {
        ServiceHandle::INVALID
    }
    unsafe extern "C" fn noop_get(
        _: *mut core::ffi::c_void,
        _: *const core::ffi::c_char,
    ) -> *const ServiceVTable {
        core::ptr::null()
    }
    unsafe extern "C" fn noop_td(
        _: *mut core::ffi::c_void,
        _: *const core::ffi::c_char,
    ) -> RequestId {
        RequestId::INVALID
    }
    unsafe extern "C" fn noop_execute(
        _: *mut core::ffi::c_void,
        _: *const core::ffi::c_char,
    ) -> *const core::ffi::c_char {
        core::ptr::null()
    }
    unsafe extern "C" fn noop_middleware(
        _: *mut core::ffi::c_void,
        _: u32,
        _: Option<crate::MiddlewareCallbackFn>,
        _: *mut core::ffi::c_void,
    ) -> crate::MiddlewareHandle {
        crate::MiddlewareHandle::INVALID
    }
    unsafe extern "C" fn noop_wait(
        _: *mut core::ffi::c_void,
        _: *const core::ffi::c_char,
        _: u64,
    ) -> crate::WaitHandle {
        crate::WaitHandle::INVALID
    }
    unsafe extern "C" fn noop_cancel(_: *mut core::ffi::c_void, _: crate::WaitHandle) {}
    unsafe extern "C" fn noop_log(_: *mut core::ffi::c_void, _: u32, _: *const core::ffi::c_char) {}
    unsafe extern "C" fn noop_get_config(
        _: *mut core::ffi::c_void,
        _: *const core::ffi::c_char,
        _: *const core::ffi::c_char,
    ) -> *const core::ffi::c_char {
        core::ptr::null()
    }
    unsafe extern "C" fn noop_set_config(
        _: *mut core::ffi::c_void,
        _: *const core::ffi::c_char,
        _: *const core::ffi::c_char,
    ) -> i32 {
        -1
    }
    unsafe extern "C" fn noop_simplify_invoke(
        _: *mut core::ffi::c_void,
        _: *const core::ffi::c_char,
    ) -> crate::RequestId {
        crate::RequestId::INVALID
    }
    unsafe extern "C" fn noop_simplify_invoke_result(
        _: *mut core::ffi::c_void,
        _: *const core::ffi::c_char,
    ) -> *const core::ffi::c_char {
        core::ptr::null()
    }

    fn fake_vtable() -> HostApiVTable {
        HostApiVTable {
            version: ABI_VERSION,
            size: core::mem::size_of::<HostApiVTable>() as u32,
            host_ctx: core::ptr::null_mut(),
            on: Some(fake_on),
            off: Some(fake_off),
            emit: Some(noop_emit),
            register_tool: Some(noop_register_tool),
            call_tool: Some(noop_call_tool),
            call_tool_result: Some(noop_call_tool_result),
            list_tools: Some(noop_list),
            list_plugins: Some(noop_list),
            provide_service: Some(noop_provide),
            get_service: Some(noop_get),
            td_invoke: Some(noop_td),
            td_invoke_simplified: Some(fake_td_invoke_simplified),
            td_execute: Some(noop_execute),
            use_middleware: Some(noop_middleware),
            wait_for_update: Some(noop_wait),
            cancel_wait: Some(noop_cancel),
            log: Some(noop_log),
            get_config: Some(noop_get_config),
            set_config: Some(noop_set_config),
            provide_service_shared: Some(noop_provide_shared),
            simplify_invoke: Some(noop_simplify_invoke),
            simplify_invoke_result: Some(noop_simplify_invoke_result),
        }
    }

    fn host() -> (HostApiVTable, HostApi) {
        let vtable = fake_vtable();
        // SAFETY: vtable 在调用期间存活；必需函数都已填。
        let api = unsafe { HostApi::from_raw(&vtable) }.unwrap();
        (vtable, api)
    }

    /// 轮询一次；用 `Waker::noop`——测试里结果要么已就绪，要么由 `fire` 手动唤醒。
    fn poll_once<F: Future + Unpin>(future: &mut F) -> Poll<F::Output> {
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        // SAFETY: `future` 是 Unpin 的（只含 Arc），投影合法。
        Pin::new(future).poll(&mut cx)
    }

    #[test]
    fn invoke_awaits_the_response_and_ignores_others() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        FIRE_ON_SEND.store(0, Ordering::SeqCst);
        reset_fake();

        let (_vtable, api) = host();
        let router = ResponseRouter::new(api).expect("注册响应订阅");
        let (id, mut future) = router.invoke_simplified("getMe", "{}").expect("发送");

        assert!(matches!(poll_once(&mut future), Poll::Pending));

        // 别人的响应：不能叫醒我们。
        fire(
            RESPONSE_EVENT,
            serde_json::json!({"@type": "ok", "request_id": id.id + 100}),
        );
        assert!(matches!(poll_once(&mut future), Poll::Pending));

        fire(
            RESPONSE_EVENT,
            serde_json::json!({"@type": "ok", "request_id": id.id, "value": "hi"}),
        );
        match poll_once(&mut future) {
            Poll::Ready(Ok(value)) => assert_eq!(value["value"], "hi"),
            other => panic!("应该拿到响应，得到 {other:?}"),
        }

        drop(future);
        drop(router);
        assert_eq!(
            UNSUBSCRIBED.load(Ordering::SeqCst),
            2,
            "路由器 drop 要退订 response + update 两条"
        );
    }

    /// 响应比 `td_invoke_simplified` 返回还快：走 `recent` 缓冲，future 第一次 poll 就 Ready。
    #[test]
    fn a_response_that_arrives_before_registration_is_buffered() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        FIRE_ON_SEND.store(1, Ordering::SeqCst);
        reset_fake();

        let (_vtable, api) = host();
        let router = ResponseRouter::new(api).expect("注册响应订阅");
        let (_id, mut future) = router.invoke_simplified("getMe", "{}").expect("发送");
        match poll_once(&mut future) {
            Poll::Ready(Ok(value)) => assert_eq!(value["value"], "early"),
            other => panic!("竞态响应应该从缓冲补上，得到 {other:?}"),
        }
        FIRE_ON_SEND.store(0, Ordering::SeqCst);
    }

    /// 路由器 drop 时，还没完成的等待者要立刻拿到 `RouterClosed`，不能永远挂着。
    #[test]
    fn dropping_the_router_wakes_pending_waiters() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        FIRE_ON_SEND.store(0, Ordering::SeqCst);
        reset_fake();

        let (_vtable, api) = host();
        let router = ResponseRouter::new(api).expect("注册响应订阅");
        let (_id, mut future) = router.invoke_simplified("getMe", "{}").expect("发送");
        let mut update = router
            .wait_for_update(serde_json::json!({"@type": "updateNewMessage"}))
            .expect("等更新");
        assert!(matches!(poll_once(&mut future), Poll::Pending));
        assert!(matches!(poll_once(&mut update), Poll::Pending));

        drop(router);
        for result in [poll_once(&mut future), poll_once(&mut update)] {
            match result {
                Poll::Ready(Err(AbiError::RouterClosed)) => {}
                other => panic!("应该以 RouterClosed 结束，得到 {other:?}"),
            }
        }
    }

    /// `wait_for_update` 的 async 版：按 `@type` / 点路径匹配，drop 即取消。
    #[test]
    fn wait_for_update_awaits_only_matching_updates() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        FIRE_ON_SEND.store(0, Ordering::SeqCst);
        reset_fake();

        let (_vtable, api) = host();
        let router = ResponseRouter::new(api).expect("注册响应订阅");
        let mut wanted = router
            .wait_for_update(serde_json::json!({
                "@type": "updateNewMessage",
                "message.chat.id": -100,
            }))
            .expect("等匹配的更新");
        let mut wrong_type = router
            .wait_for_update(serde_json::json!({"@type": "updateFile"}))
            .expect("等另一类更新");

        assert!(matches!(poll_once(&mut wanted), Poll::Pending));
        assert!(matches!(poll_once(&mut wrong_type), Poll::Pending));

        // 类型不对 / 路径不对：都不能叫醒 wanted。
        fire(
            UPDATE_EVENT,
            serde_json::json!({"@type": "updateFile", "file": {"id": 1}}),
        );
        assert!(matches!(poll_once(&mut wanted), Poll::Pending));
        assert!(matches!(poll_once(&mut wrong_type), Poll::Ready(Ok(_))));

        fire(
            UPDATE_EVENT,
            serde_json::json!({"@type": "updateNewMessage", "message": {"chat": {"id": -200}}}),
        );
        assert!(matches!(poll_once(&mut wanted), Poll::Pending), "chat 不对");

        fire(
            UPDATE_EVENT,
            serde_json::json!({"@type": "updateNewMessage", "message": {"chat": {"id": -100}}}),
        );
        match poll_once(&mut wanted) {
            Poll::Ready(Ok(value)) => assert_eq!(value["message"]["chat"]["id"], -100),
            other => panic!("应该等到匹配的更新，得到 {other:?}"),
        }
    }

    /// 真 waker + 跨线程唤醒：响应在另一个线程回调，await 的任务被唤醒。
    ///
    /// 上面几条用 `Waker::noop` 手动 poll，只能证明状态机；这条证明「宿主线程回调
    /// 能真的把插件 executor 上的任务叫醒」——也就是 `await` 能返回。
    #[tokio::test]
    async fn await_survives_a_real_waker_and_a_thread_hop() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        FIRE_ON_SEND.store(0, Ordering::SeqCst);
        reset_fake();

        let (_vtable, api) = host();
        let router = ResponseRouter::new(api).expect("注册响应订阅");
        let (id, future) = router.invoke_simplified("getMe", "{}").expect("发送");

        let responder = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(10));
            fire(
                RESPONSE_EVENT,
                serde_json::json!({"@type": "ok", "request_id": id.id, "value": "woken"}),
            );
        });

        let value = tokio::time::timeout(std::time::Duration::from_secs(2), future)
            .await
            .expect("await 超时——waker 没被调用？")
            .expect("响应应该成功");
        responder.join().unwrap();
        assert_eq!(value["value"], "woken");
    }
}
