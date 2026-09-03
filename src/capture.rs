use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    rc::Rc,
    time::{Duration, Instant},
};

/// **Pending-capture 握手超时**：主线程发了 `ProtoEvent::Enter` 后等对
/// 端 `Ack` 的最大时长。LAN 内 Ack 通常 <50ms；500ms 给 10× 缓冲，超时
/// 后取消 pending（鼠标留在主机）。
///
/// **为什么不阻塞等更长**：用户感受优先 —— 移过边沿 0.5s 还不见鼠标切
/// 走就已经觉得"卡了"，再长直接归类为故障。可作为后续可配置项（M2）。
const PENDING_ACK_TIMEOUT: Duration = Duration::from_millis(500);

/// Pending 超时检测 tick 周期。100ms 给超时判定最多 100ms 抖动（worst
/// case：500ms 整边界 → 实际 500–600ms 才触发取消）。
const PENDING_TICK_INTERVAL: Duration = Duration::from_millis(100);

// === FIX 4 — Watchdog 自愈 ====================================================
//
// **背景**：1+2 修复覆盖了"`release_capture` 后 State::Sending 残留"和"迟
// 到 Ack 切错状态"两类 bug；剩余失败场景（网络抖动后 `active_client` 与
// backend 状态错位、用户反复跨边但都走重试 gate、连续 `send_input` 失败
// 但 release 路径竞态未触发）需要主动检测 + 自愈。本节加 watchdog。
//
// **设计原则**：
// - **不改协议**：纯本地状态自检，不新增 ProtoEvent。
// - **分层检测**：light (200ms) 跑无 IO 的 ghost-active 检测，heavy (3s)
//   跑带状态聚合的 storm / failures / no-progress 检测，分摊开销。
// - **可关闭**：环境变量 `LAN_MOUSE_WATCHDOG=off` 一键关，方便调试。
// - **可观测**：命中后 WARN 一条；触发恢复后 INFO 记一条结果。
//
// **不做**的事：
// - 不修协议层（无新增 ProtoEvent 字段）。
// - 不强依赖具体后端（OS-agnostic，只调 `InputCapture::release`）。
// - 不替代 release-bind —— 用户主动按键永远是最高优先级。

/// Watchdog light 检测周期。无 IO 状态自检。
const WATCHDOG_LIGHT_INTERVAL: Duration = Duration::from_millis(200);

/// Watchdog heavy 检测周期。带状态聚合（crossings 计数、send_failures 累
/// 计、no-progress 时间窗），允许大窗口。
const WATCHDOG_HEAVY_INTERVAL: Duration = Duration::from_secs(3);

/// 连续 `conn.send` 失败次数阈值；`State::Sending` 期间累计到该值 → 强制
/// release。选 3 是因为 LAN 内连续 3 次失败已经能区分"瞬时网络抖动"与"对
/// 端真挂"，前者 1~2 次就恢复，后者需 3+ 次后才好真正判定。
const WATCHDOG_SEND_FAILURES_THRESHOLD: u32 = 3;

/// "跨边风暴"窗口大小。窗口内跨边次数 ≥ 阈值即触发。
const WATCHDOG_CROSSING_WINDOW: Duration = Duration::from_secs(5);

/// 窗口内允许的最大"未成功推进"的跨边次数。5s 内 5 次意味着用户每次跨过去
/// 都在反复 retry 都失败 —— 大概率是状态机冻住了。轻量场景（用户正常来回
/// 切）下不会触达：5s 内最多 1~2 次有效跨边。
const WATCHDOG_CROSSING_STORM_THRESHOLD: usize = 5;

/// "无进度"超时：从最近一次成功状态推进（Pending→Sending、send_input ok
/// 等）算起，超此时长仍停留在非 Idle 状态 → 强制 release。8s 远大于正常
/// LAN jitter（<10ms），也不会让用户卡超过这个时间。
const WATCHDOG_NO_PROGRESS_TIMEOUT: Duration = Duration::from_secs(8);

use futures::StreamExt;
use input_capture::{
    CaptureError, CaptureEvent, CaptureHandle, InputCapture, InputCaptureError, Position,
};
use input_event::{Event, KeyboardEvent, scancode};
use lan_mouse_proto::ProtoEvent;
use local_channel::mpsc::{Receiver, Sender, channel};
use tokio::task::{JoinHandle, spawn_local};
use tokio_util::sync::CancellationToken;

use crate::connect::LanMouseConnection;
use lan_mouse_ipc::ClientHandle;

pub(crate) struct Capture {
    cancellation_token: CancellationToken,
    request_tx: Sender<CaptureRequest>,
    task: JoinHandle<()>,
    event_rx: Receiver<ICaptureEvent>,
}

pub(crate) enum ICaptureEvent {
    /// a client was entered
    CaptureBegin(CaptureHandle),
    /// capture disabled
    CaptureDisabled,
    /// capture disabled
    CaptureEnabled,
    /// A (new) client was entered.
    /// In contrast to [`ICaptureEvent::CaptureBegin`] this
    /// event is only triggered when the capture was
    /// explicitly released in the meantime by
    /// either the remote client leaving its device region,
    /// a new device entering the screen or the release bind.
    ClientEntered(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureType {
    /// a normal input capture
    Default,
    /// A capture only interested in [`CaptureEvent::Begin`] events.
    /// The capture is released immediately, if there is no
    /// Default capture at the same position.
    EnterOnly,
}

#[derive(Clone, Debug)]
enum CaptureRequest {
    /// capture must release the mouse
    Release,
    /// add a capture client
    Create(CaptureHandle, Position, CaptureType),
    /// destory a capture client
    Destroy(CaptureHandle),
    /// reenable input capture
    Reenable,
    /// set release bind
    SetReleaseBind(Vec<scancode::Linux>),
    /// **STEP-8.2 修复 — `connect_on_activate`**：主动触发拨号但不发送
    /// 任何事件。`service.rs::activate_client` 在 client 激活后立即 fire-
    /// and-forget 发这条请求 → `CaptureTask` 调用 `conn.dial(handle)` →
    /// `connect_to_handle` 后台 spawn。详见 connect.rs::dial docstring。
    Dial(ClientHandle),
}

impl Capture {
    pub(crate) fn new(
        backend: Option<input_capture::Backend>,
        conn: LanMouseConnection,
        release_bind: Vec<scancode::Linux>,
    ) -> Self {
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let cancellation_token = CancellationToken::new();
        let capture_task = CaptureTask {
            active_client: None,
            backend,
            cancellation_token: cancellation_token.clone(),
            captures: Default::default(),
            conn,
            event_tx,
            request_rx,
            release_bind: Rc::new(RefCell::new(release_bind)),
            state: Default::default(),
            release_bind_prev: false,
            // FIX 4：watchdog 自愈状态初始化。
            watchdog: WatchdogState::new(),
        };
        let task = spawn_local(capture_task.run());
        Self {
            cancellation_token,
            request_tx,
            task,
            event_rx,
        }
    }

    pub(crate) fn reenable(&self) {
        self.request_tx
            .send(CaptureRequest::Reenable)
            .expect("channel closed");
    }

    pub(crate) async fn terminate(&mut self) {
        self.cancellation_token.cancel();
        log::debug!("terminating capture");
        if let Err(e) = (&mut self.task).await {
            log::warn!("{e}");
        }
    }

    pub(crate) fn create(
        &self,
        handle: CaptureHandle,
        pos: lan_mouse_ipc::Position,
        capture_type: CaptureType,
    ) {
        let pos = to_capture_pos(pos);
        self.request_tx
            .send(CaptureRequest::Create(handle, pos, capture_type))
            .expect("channel closed");
    }

    pub(crate) fn destroy(&self, handle: CaptureHandle) {
        self.request_tx
            .send(CaptureRequest::Destroy(handle))
            .expect("channel closed");
    }

    pub(crate) fn release(&self) {
        self.request_tx
            .send(CaptureRequest::Release)
            .expect("channel closed");
    }

    pub(crate) async fn event(&mut self) -> ICaptureEvent {
        self.event_rx.recv().await.expect("channel closed")
    }

    pub(crate) fn set_release_bind(&mut self, bind: Vec<scancode::Linux>) {
        let _ = self.request_tx.send(CaptureRequest::SetReleaseBind(bind));
    }

    /// **STEP-8.2 修复 — `connect_on_activate`**：主动触发对端的拨号，但不
    /// 发送任何事件。
    ///
    /// **为什么需要这条路径**：`service.rs::activate_client` 在 client 激活
    /// 时调它 —— 即便没人移鼠标到屏边，也能立即 spawn 一次拨号尝试。解决
    /// "两侧 daemon 启动 + 指纹已授权 + 没人移鼠标 → 永远不建连"的鸡生蛋
    /// 问题。
    ///
    /// **fire-and-forget**：本方法 send `CaptureRequest::Dial` 后立即返回。
    /// `CaptureTask` 在两个 `select!` 臂（`run()` 重启循环 + `do_capture_
    /// session()` 主循环）的任一臂收到 `Dial(handle)` 时调
    /// `self.conn.dial(handle)`（fire-and-forget spawn `connect_to_handle`）。
    ///
    /// **失败模式**：send `request_tx` 失败仅在 task 已退出时发生（terminate
    /// 已触发），**不**是用户可见的失败 —— 此后 activate_client 也不再有
    /// 意义。静默 no-op。
    pub(crate) fn dial(&self, handle: ClientHandle) {
        let _ = self.request_tx.send(CaptureRequest::Dial(handle));
    }
}

/// debounce a statement `$st`, i.e. the statement is executed only if the
/// time since the previous execution is at least `$dur`.
/// `$prev` is used to keep track of this timestamp
macro_rules! debounce {
    ($prev:ident, $dur:expr, $st:stmt) => {
        let exec = match $prev.get() {
            None => true,
            Some(instant) if instant.elapsed() > $dur => true,
            _ => false,
        };
        if exec {
            $prev.replace(Some(Instant::now()));
            $st
        }
    };
}

struct CaptureTask {
    active_client: Option<CaptureHandle>,
    backend: Option<input_capture::Backend>,
    cancellation_token: CancellationToken,
    captures: Vec<(CaptureHandle, Position, CaptureType)>,
    conn: LanMouseConnection,
    event_tx: Sender<ICaptureEvent>,
    release_bind: Rc<RefCell<Vec<scancode::Linux>>>,
    request_rx: Receiver<CaptureRequest>,
    state: State,
    /// 上一次 `keys_pressed(release_bind)` 检测结果 —— 用于在 false→true
    /// 跳变时记一条 INFO(mouse 卡住 bug 复现时只关心按下那一瞬间,持续
    /// 按下时不刷屏)。
    release_bind_prev: bool,
    /// **FIX 4 — Watchdog 自愈状态**：见下方 `WatchdogState` docstring。
    watchdog: WatchdogState,
}

/// **FIX 4 — Watchdog 自愈状态**：
///
/// 用于在 `do_capture_session` 主循环被动 select 不到异常时主动检测一
/// 类无法被现有事件流捕获的"状态机冻住"场景。具体三类 detection 需要
/// 不同的状态：
///
/// 1. **Ghost-active**：`state == State::Sending` 但 `active_client ==
///    None`（修复 1 没覆盖的镜像 bug —— `active_client.take()` 之后忘了
///    重置 state）。light tick 检测。
/// 2. **Send-failure storm**：连续 `conn.send` 失败次数累计。input 事
///    件发送失败时累加，发送成功或离开 Sending 时清零。heavy tick 检测。
/// 3. **Crossing storm**：滑动窗口 [`WATCHDOG_CROSSING_WINDOW`] 内的
///    `BeginPending` 时间戳。`do_capture_session` 进入 `BeginPending`
///    handler 时 push。heavy tick 清理过期条目 + 检测风暴。
/// 4. **No-progress**：`last_progress_at` 上次"状态成功推进"时间戳。包
///    含 Pending→Sending 提升、`send_input` ok、`release_capture` 完
///    成。heavy tick 检测距此时长超 [`WATCHDOG_NO_PROGRESS_TIMEOUT`]
///    且 state 非 Idle → 强 release。
///
/// **reset 触发点**：
/// - `release_capture` 末尾：清零 `consecutive_send_failures`、刷新
///   `last_progress_at = now`、清 `recent_crossings`。
/// - `handle_capture_event::Input(e)` State::Sending Ok 时：清零 + 刷新。
/// - `Begin` 命中 pending→active 分支时：刷新 `last_progress_at`。
struct WatchdogState {
    /// `State::Sending` 期间连续 `conn.send` 失败次数。
    consecutive_send_failures: u32,
    /// 滑动窗口 `BeginPending` 时间戳。push 在 `handle_capture_event` 入
    /// `BeginPending` 分支处；heavy tick 清理 [`WATCHDOG_CROSSING_WINDOW`]
    /// 之前的旧条目。
    recent_crossings: VecDeque<Instant>,
    /// 上次"状态推进成功"时间戳。`release_capture` 完成 / Pending→Sending
    /// 提升 / `send_input` ok 三处刷新。
    last_progress_at: Instant,
}

impl WatchdogState {
    fn new() -> Self {
        Self {
            consecutive_send_failures: 0,
            recent_crossings: VecDeque::new(),
            // 初值 = `do_capture_session` 启动时间；后续三处刷新点保持
            // 最新。
            last_progress_at: Instant::now(),
        }
    }
}

impl CaptureTask {
    fn add_capture(&mut self, handle: CaptureHandle, pos: Position, capture_type: CaptureType) {
        self.captures.push((handle, pos, capture_type));
    }

    fn remove_capture(&mut self, handle: CaptureHandle) {
        self.captures.retain(|&(h, ..)| handle != h);
    }

    fn is_default_capture_at(&self, pos: Position) -> bool {
        self.captures
            .iter()
            .any(|&(_, p, t)| p == pos && t == CaptureType::Default)
    }

    fn get_pos(&self, handle: CaptureHandle) -> Position {
        self.captures
            .iter()
            .find(|(h, ..)| *h == handle)
            .expect("no such capture")
            .1
    }

    fn get_type(&self, handle: CaptureHandle) -> CaptureType {
        self.captures
            .iter()
            .find(|(h, ..)| *h == handle)
            .expect("no such capture")
            .2
    }

    async fn run(mut self) {
        loop {
            if let Err(e) = self.do_capture().await {
                log::warn!("input capture exited: {e}");
            }
            loop {
                tokio::select! {
                    r = self.request_rx.recv() => match r.expect("channel closed") {
                        CaptureRequest::Reenable => break,
                        CaptureRequest::Create(h, p, t) => self.add_capture(h, p, t),
                        CaptureRequest::Destroy(h) => self.remove_capture(h),
                        CaptureRequest::Release => { /* nothing to do */ }
                        CaptureRequest::SetReleaseBind(bind) => {
                            self.release_bind.borrow_mut().clone_from(&bind);
                        }
                        // STEP-8.2 修复：do_capture 退出循环期间（重启期
                        // 间）来的 dial 请求也要立即转发 —— 等下次 capture
                        // 起来再处理就太晚了（peer daemon 可能已退出
                        // retry 退避窗口）。fire-and-forget 调 conn.dial。
                        CaptureRequest::Dial(handle) => {
                            let _ = self.conn.dial(handle).await;
                        }
                    },
                    _ = self.cancellation_token.cancelled() => return,
                }
            }
        }
    }

    async fn do_capture(&mut self) -> Result<(), InputCaptureError> {
        /* allow cancelling capture request */
        let mut capture = tokio::select! {
            r = InputCapture::new(self.backend) => r?,
            _ = self.cancellation_token.cancelled() => return Ok(()),
        };

        let _capture_guard = DropGuard::new(
            self.event_tx.clone(),
            ICaptureEvent::CaptureEnabled,
            ICaptureEvent::CaptureDisabled,
        );

        /* create barriers for active clients */
        let r = self.create_captures(&mut capture).await;
        if let Err(e) = r {
            capture.terminate().await?;
            return Err(e.into());
        }

        let r = self.do_capture_session(&mut capture).await;

        // FIXME replace with async drop when stabilized
        capture.terminate().await?;

        r
    }

    async fn create_captures(&mut self, capture: &mut InputCapture) -> Result<(), CaptureError> {
        let captures = self.captures.clone();
        for (handle, pos, _type) in captures {
            tokio::select! {
                r = capture.create(handle, pos) => r?,
                _ = self.cancellation_token.cancelled() => return Ok(()),
            }
        }
        Ok(())
    }

    async fn do_capture_session(
        &mut self,
        capture: &mut InputCapture,
    ) -> Result<(), InputCaptureError> {
        // **Pending 超时 tick**：100ms 周期检查 `State::Pending` 的
        // `started.elapsed()`，超过 500ms 主动 cancel_pending。
        // `MissedTickBehavior::Skip` 防止累积延迟（back-pressure）。
        let mut pending_tick = tokio::time::interval(PENDING_TICK_INTERVAL);
        pending_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // 跳过初始立即触发的 tick —— session 启动时还没 pending 可查。
        pending_tick.tick().await;

        // **FIX 4 — Watchdog ticks**：
        //
        // - `watchdog_light_tick` (200ms) —— 无 IO 的 ghost-active 检测。
        //   频次高但成本低，命中后立即 force-reset。
        // - `watchdog_heavy_tick` (3s) —— 跨边窗口清理 + send-failures
        //   累计 + no-progress 超时，频次低但需要状态聚合。
        //
        // **环境变量关停**：`LAN_MOUSE_WATCHDOG=off` 时两个 tick 都不
        // 创建，loop 永远不命中这两个分支 —— 等价于禁用。开发调试时用。
        let watchdog_enabled = std::env::var("LAN_MOUSE_WATCHDOG")
            .map(|v| v.to_ascii_lowercase() != "off")
            .unwrap_or(true);
        let (mut watchdog_light_tick, mut watchdog_heavy_tick) = if watchdog_enabled {
            let mut light = tokio::time::interval(WATCHDOG_LIGHT_INTERVAL);
            light.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // 跳过初始立即 tick，避免启动瞬时 false-positive
            // （CaptureTask 内字段刚初始化）。
            light.tick().await;
            let mut heavy = tokio::time::interval(WATCHDOG_HEAVY_INTERVAL);
            heavy.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            heavy.tick().await;
            (Some(light), Some(heavy))
        } else {
            log::info!("watchdog disabled by LAN_MOUSE_WATCHDOG=off");
            (None, None)
        };

        loop {
            tokio::select! {
                event = capture.next() => match event {
                    Some(event) => self.handle_capture_event(capture, event?).await?,
                    None => return Ok(()),
                },
                (handle, event) = self.conn.recv() => {
                    if let Some(active) = self.active_client {
                        if handle != active {
                            // we only care about events coming from the client we are currently connected to
                            // only `Ack` and `Leave` are relevant
                            continue
                        }
                    }

                    match event {
                        // **Pending-capture 握手 Ack**：
                        // - 当前是 Pending 且 handle 匹配 → 让后端提升
                        //   pending → active，然后等后端 emit Begin →
                        //   在 handle_capture_event 里切到 Sending。
                        // - 当前是 Idle（libei 异步路径或 Begin 已先到）
                        //   → 走老逻辑，直接转 Sending。
                        // - handle 不匹配 → ignore（Ack 跟我们无关）。
                        ProtoEvent::Ack(_) => {
                            match self.state {
                                State::Pending { handle: pending_h, started } => {
                                    if pending_h == handle {
                                        log::info!(
                                            "client {handle} acknowledged Enter after {:?}",
                                            started.elapsed()
                                        );
                                        // 让 Windows / macOS 后端把 pending
                                        // 提升为 active；后端会在自己的消息
                                        // 循环里 emit Begin，主线程收到 Begin
                                        // 时切 Sending（见 handle_capture_event）。
                                        // 注：start_capture 是 idempotent，如
                                        // 果期间被 cancel（用户回退）则 no-op。
                                        if let Err(e) =
                                            capture.start_capture(self.get_pos(handle))
                                        {
                                            log::warn!(
                                                "start_capture after Ack failed: {e} \
                                                 (cancelling pending)"
                                            );
                                            let _ =
                                                capture.cancel_pending(self.get_pos(handle));
                                            self.state = State::Idle;
                                        }
                                        // 保持 Pending，等后端 Begin 切 Sending。
                                    } else {
                                        log::trace!(
                                            "Ack for handle {handle} ignored: pending handle is {pending_h:?}"
                                        );
                                    }
                                }
                                _ => {
                                    log::info!("client {handle} acknowledged the connection!");
                                    self.state = State::Sending;
                                }
                            }
                        }
                        // client disconnected
                        ProtoEvent::Leave(_) => {
                            log::info!("releasing capture: left remote client device region");
                            self.release_capture(capture).await?;
                        },
                        _ => {}
                    }
                },
                e = self.request_rx.recv() => match e.expect("channel closed") {
                    CaptureRequest::Reenable => { /* already active */ },
                    CaptureRequest::Release => self.release_capture(capture).await?,
                    CaptureRequest::Create(h, p, t) => {
                        self.add_capture(h, p, t);
                        capture.create(h, p).await?;
                    }
                    CaptureRequest::Destroy(h) => {
                        self.remove_capture(h);
                        capture.destroy(h).await?;
                    }
                    CaptureRequest::SetReleaseBind(bind) => {
                        self.release_bind.borrow_mut().clone_from(&bind);
                    }
                    // STEP-8.2 修复：service.rs::activate_client 触发的主
                    // 动拨号。fire-and-forget 调 conn.dial —— RetryState
                    // gate + connecting set 去重由 connect.rs 内部保证。
                    CaptureRequest::Dial(handle) => {
                        let _ = self.conn.dial(handle).await;
                    }
                },
                // **Pending 超时 tick**：Pending 状态下超过 500ms 没收到
                // Ack → 主动取消 pending，鼠标留在主机。
                _ = pending_tick.tick() => {
                    if let State::Pending { handle, started } = self.state {
                        let elapsed = started.elapsed();
                        if elapsed >= PENDING_ACK_TIMEOUT {
                            log::warn!(
                                "capture: BeginPending timed out after {elapsed:?} for handle {handle} \
                                 - cancelling (host cursor stays visible)"
                            );
                            if let Err(e) =
                                capture.cancel_pending(self.get_pos(handle))
                            {
                                log::warn!("cancel_pending on timeout: {e}");
                            }
                            self.state = State::Idle;
                            // 注意：timeout 分支不需要走 capture.release()。
                            // pending 状态下后端没真正激活，ACTIVE_CLIENT
                            // 本来就没设；cancel_pending 已经把后端 PENDING
                            // 清掉。release_bind / Leave 等路径才需要 release。
                        }
                    }
                },
                // **FIX 4 — Watchdog light tick**：ghost-active 检测。
                //
                // **触发条件**：`state == State::Sending` 且
                // `active_client.is_none()` —— 状态机说"我们在 active"但
                // 实际上没有任何 active client。这是修复 1 没覆盖的镜像
                // bug（`active_client.take()` 之后忘了 `state = Idle`）。
                //
                // **修复**：把 state 强制回 Idle、调 `capture.release()`。
                // 即使本分支命中频繁，因为命中后立即 reset state，下次
                // tick 不会再次触发。
                //
                // **不做**：不重建 peer、不主动 dial —— 仅本地状态对齐，
                // 让下一轮 `BeginPending` 自然从 Idle 开始。
                _ = async {
                    if let Some(t) = watchdog_light_tick.as_mut() {
                        t.tick().await;
                    } else {
                        // 永不 resolve（旁路分支）
                        std::future::pending::<()>().await
                    }
                } => {
                    self.run_watchdog_light(capture).await;
                }
                // **FIX 4 — Watchdog heavy tick**：跨边窗口清理 +
                // send-failures 累计 + no-progress 超时。
                _ = async {
                    if let Some(t) = watchdog_heavy_tick.as_mut() {
                        t.tick().await;
                    } else {
                        std::future::pending::<()>().await
                    }
                } => {
                    self.run_watchdog_heavy(capture).await;
                }
                _ = self.cancellation_token.cancelled() => break,
            }
        }
        Ok(())
    }

    // === FIX 4 — Watchdog 检测函数 ============================================

    /// **Watchdog light check**：无 IO 检测，仅 ghost-active 一种条件。
    ///
    /// 调频 [`WATCHDOG_LIGHT_INTERVAL`] = 200ms。设计原则是"便宜到随时
    /// 可跑"——只看 `self.state` 和 `self.active_client` 两个字段，不触
    /// 任何 peer / send / select。
    ///
    /// **ghost-active**：原逻辑 `release_capture` 的 Pending 分支显式置
    /// `state = Idle`，但 Sending 分支只 `active_client.take()` 即发送
    /// Leave + 调 `capture.release()`，**没有同步置 state**（这是修复
    /// 1 的内容；但当前 fix 4 单独 ship 时此路径仍残留）。watchdog 在
    /// 此处兜底。
    ///
    /// **命中后**：state 强制回 Idle，`capture.release()` 兜底（清 OS
    /// 层 ACTIVE_CLIENT），刷新 `last_progress_at = now` 让 heavy tick
    /// 不再视为 no-progress。
    async fn run_watchdog_light(
        &mut self,
        capture: &mut InputCapture,
    ) {
        // 仅在 state == Sending 且 active_client == None 时命中。
        // 不打 Sending 但 active_client 还有 / Idle 状态：前者是正常
        // active 状态，后者本来就该 idle。
        if matches!(self.state, State::Sending) && self.active_client.is_none() {
            log::warn!(
                "watchdog[light]: ghost-active detected (State::Sending + active_client=None) \
                 -- force-resetting to Idle"
            );
            self.state = State::Idle;
            // best-effort: 这一步让 OS 层真释放 ACTIVE_CLIENT，避免
            // hook 还在吞事件但 state 已经说 idle。
            if let Err(e) = capture.release().await {
                log::warn!("watchdog[light]: capture.release() failed: {e}");
            }
            // 重置 watchdog 状态：刚刚"恢复"一次，从 now 起算。
            self.watchdog.consecutive_send_failures = 0;
            self.watchdog.last_progress_at = Instant::now();
        }
    }

    /// **Watchdog heavy check**：跨边窗口清理 + send-failures 累计 +
    /// no-progress 超时三种检测。调频 [`WATCHDOG_HEAVY_INTERVAL`] = 3s。
    ///
    /// **三类检测独立判断**：
    /// 1. **Crossing storm**：清理 [`WATCHDOG_CROSSING_WINDOW`] 之前的旧
    ///    条目，再看 `recent_crossings.len()` 是否 ≥
    ///    [`WATCHDOG_CROSSING_STORM_THRESHOLD`] 且当前 Idle + 无 active。
    ///    → 用户反复跨过去但都失败，状态机卡住。强制 release 让下一轮
    ///    Begin 从 Idle 自然开始。
    /// 2. **Send-failure storm**：`State::Sending` 期间累计连续 send 失
    ///    败达 [`WATCHDOG_SEND_FAILURES_THRESHOLD`] → 强制 release。
    ///    与 `release_capture` 的"send 失败 → release"路径互补，处理
    ///    `conn.send` 在 retry gate 内返 NotConnected 但 release 路径竞
    ///    态未触发的场景。
    /// 3. **No-progress**：距 `last_progress_at` 超
    ///    [`WATCHDOG_NO_PROGRESS_TIMEOUT`] 且 state 非 Idle → 状态机
    ///    "冻住"，强制 release。
    ///
    /// **顺序**：先做 (1)，再做 (2)，最后 (3)。每一项独立判断、独立记日志。
    /// 一次 tick 命中多个时全部触发（各自 `release_capture` 是幂等的）。
    async fn run_watchdog_heavy(
        &mut self,
        capture: &mut InputCapture,
    ) {
        let now = Instant::now();

        // ── (1) Crossing storm ────────────────────────────────────────────
        // 清理过期 entries（早于窗口的）。
        while let Some(&front) = self.watchdog.recent_crossings.front() {
            if now - front > WATCHDOG_CROSSING_WINDOW {
                self.watchdog.recent_crossings.pop_front();
            } else {
                break;
            }
        }
        if self.watchdog.recent_crossings.len() >= WATCHDOG_CROSSING_STORM_THRESHOLD
            && matches!(self.state, State::Idle)
            && self.active_client.is_none()
        {
            log::warn!(
                "watchdog[heavy]: crossing storm ({} in {:?}) without state advance \
                 -- force release",
                self.watchdog.recent_crossings.len(),
                WATCHDOG_CROSSING_WINDOW,
            );
            if let Err(e) = self.release_capture(capture).await {
                log::warn!("watchdog[heavy]: release_capture during storm clear failed: {e}");
            }
            self.watchdog.recent_crossings.clear();
            self.watchdog.last_progress_at = now;
            // 跳过后续两个检测：本轮已释放，状态已 reset。
            return;
        }

        // ── (2) Send-failure storm ────────────────────────────────────────
        if matches!(self.state, State::Sending)
            && self.watchdog.consecutive_send_failures >= WATCHDOG_SEND_FAILURES_THRESHOLD
        {
            log::warn!(
                "watchdog[heavy]: {} consecutive send-input failures in Sending \
                 -- force release",
                self.watchdog.consecutive_send_failures,
            );
            self.watchdog.consecutive_send_failures = 0;
            if let Err(e) = self.release_capture(capture).await {
                log::warn!("watchdog[heavy]: release_capture during send-failure clear failed: {e}");
            }
            self.watchdog.last_progress_at = now;
            return;
        }

        // ── (3) No-progress ────────────────────────────────────────────────
        // 仅在非 Idle 状态检查（Idle 本来就没"进展"可言）。
        if !matches!(self.state, State::Idle)
            && now - self.watchdog.last_progress_at > WATCHDOG_NO_PROGRESS_TIMEOUT
        {
            log::warn!(
                "watchdog[heavy]: no progress for >{:?} in state {:?} \
                 -- force release",
                WATCHDOG_NO_PROGRESS_TIMEOUT,
                self.state,
            );
            if let Err(e) = self.release_capture(capture).await {
                log::warn!("watchdog[heavy]: release_capture during no-progress clear failed: {e}");
            }
            self.watchdog.last_progress_at = now;
        }
    }

    async fn handle_capture_event(
        &mut self,
        capture: &mut InputCapture,
        event: (CaptureHandle, CaptureEvent),
    ) -> Result<(), CaptureError> {
        let (handle, event) = event;
        log::trace!("({handle}): {event:?}");

        let pressed = capture.keys_pressed(&self.release_bind.borrow());
        // 持续按住 release-bind 时不刷屏,只在按下那一瞬间记一条 INFO,
        // 便于诊断 "鼠标卡住但 release-bind 已按下" 这类状态错乱 bug。
        if pressed && !self.release_bind_prev {
            log::info!("release_bind detected as PRESSED (rising edge)");
        }
        self.release_bind_prev = pressed;

        if pressed {
            log::info!("releasing capture: release-bind pressed");
            return self.release_capture(capture).await;
        }

        // enter only capture (for incoming connections)
        //
        // **Pending-capture 兼容（bc5507f 修复 #2）**：commit `bc5507f` 让
        // Windows / macOS 后端对所有 client 统一走 pending-capture 握手，
        // 边沿跨越一律发 `BeginPending` + 设 `PENDING_CLIENT`，由主线程
        // 等对端 Ack 后调 `start_capture` 提升到 active 才发 `Begin`。
        //
        // **问题**：EnterOnly 是单向 trigger（不发 Enter，只让 service 给
        // 对端发 Leave），根本不会进入 `State::Pending` 也不会等 Ack，所以
        // 后端对它也只发 `BeginPending`、永远不会发 `Begin`。原"event==
        // Begin 才发 CaptureBegin"的逻辑对 EnterOnly 永远漏报 → service
        // 收不到 trigger 信号 → emulation.send_leave_event 不被调 → 对端
        // 收不到 `ProtoEvent::Leave(0)` → 主控端 capture 永远不释放 →
        // 鼠标卡在被控端回不到主机（即用户报的 BUG）。
        //
        // **修复**：对 EnterOnly capture，Begin 和 BeginPending 都当 trigger
        // 处理，都向上层发 `CaptureBegin`（service 用来调 send_leave_event）。
        // 不改变 EnterOnly 的语义（不真正捕获光标、不发 Enter、不进入
        // State::Pending），只让 trigger 信号正确到达 service 层。
        if self.get_type(handle) == CaptureType::EnterOnly {
            if event == CaptureEvent::Begin || event == CaptureEvent::BeginPending {
                log::info!(
                    "capture: EnterOnly trigger on {handle:?} (event={event:?}) \
                     — forwarding to service as CaptureBegin"
                );
                self.event_tx
                    .send(ICaptureEvent::CaptureBegin(handle))
                    .expect("channel closed");
            }
            // if there is no active outgoing connection at the current capture,
            // we release the capture
            if !self.is_default_capture_at(self.get_pos(handle)) {
                log::info!("releasing capture: no active client at this position");
                capture.release().await?;
            }
            // we dont care about events from incoming handles except for releasing the capture
            return Ok(());
        }

        // 通知 service 层有 hook 事件。CaptureBegin 对 Begin 发（libei /
        // 其他异步后端的"立即进入捕获"路径），对 BeginPending 不发（pending
        // 路径在 `match` 分支里走自己的握手，不在这里 trigger）。
        if event == CaptureEvent::Begin {
            self.event_tx
                .send(ICaptureEvent::CaptureBegin(handle))
                .expect("channel closed");
        }

        // ── 按 event 变体分支 ──────────────────────────────────────────────
        match event {
            // **Pending-capture 握手入口**：Windows / macOS 后端检测到鼠
            // 标跨过边沿 → 设 pending（光标仍可见）→ 发 BeginPending。
            // 我们发 Enter 给对端，等 Ack。Send 失败直接取消 pending。
            CaptureEvent::BeginPending => {
                self.state = State::Pending {
                    handle,
                    started: Instant::now(),
                };
                log::info!(
                    "capture: BeginPending (handle={handle:?}) - awaiting Ack within {PENDING_ACK_TIMEOUT:?}"
                );
                // **FIX 4**：watchdog 跨边风暴检测 —— 每次 BeginPending
                // 都记一次。heavy tick 在 5s 窗口内看到 ≥ 5 次且 state
                // 仍 Idle → 判定状态机卡住，强制 release 后让下一轮自
                // 然从头来。
                self.watchdog.recent_crossings.push_back(Instant::now());
                let opposite_pos = to_proto_pos(self.get_pos(handle).opposite());
                if let Err(e) = self.conn.send(ProtoEvent::Enter(opposite_pos), handle).await {
                    log::warn!(
                        "releasing capture: BeginPending send failed: {e} \
                         (cancelling pending, host cursor stays visible)"
                    );
                    // send 失败 → 取消 pending。注意：主线程发的 cancel
                    // 是异步消息到 Windows thread；即便 cancel_pending
                    // 失败，500ms tick 也会兜底清掉 PENDING_CLIENT。
                    if let Err(e) = capture.cancel_pending(self.get_pos(handle)) {
                        log::warn!("cancel_pending after send failure: {e}");
                    }
                    self.state = State::Idle;
                    // **FIX 4**：BeginPending 内的 send 失败也计入
                    // consecutive failures，让 watchdog heavy 把
                    // "反复 Begin→send 失败"识别为跨边风暴前的征兆。
                    self.watchdog.consecutive_send_failures = self
                        .watchdog
                        .consecutive_send_failures
                        .saturating_add(1);
                    // 不要走 capture.release()：pending 状态后端本来就没
                    // 激活，无 Leave 要发。
                    return Ok(());
                }
                // send 成功 → 重置失败计数 + 记录推进（Pending 已建
                // 立，下一步等 Ack 是预期路径不视作 no-progress）。
                self.watchdog.consecutive_send_failures = 0;
                self.watchdog.last_progress_at = Instant::now();
                Ok(())
            }

            // **Pending-capture 取消**：用户把鼠标移回屏幕内、Windows
            // 后端自动取消，或 cancel_pending 失败后的兜底。我们已不在
            // 等 Ack；只把状态归 Idle。如果当前不是 Pending（已切换或
            // 还在 Idle）也不报错 —— CancelPending 可能重复到达。
            CaptureEvent::CancelPending => {
                log::info!("capture: CancelPending (handle={handle:?})");
                if let Err(e) = capture.cancel_pending(self.get_pos(handle)) {
                    log::warn!("cancel_pending: {e}");
                }
                if let State::Pending { handle: h, .. } = self.state {
                    if h == handle {
                        self.state = State::Idle;
                    }
                }
                Ok(())
            }

            // **普通 Begin**：libei 等异步后端的路径（compositor 已经捕
            // 获了 cursor，我们只是被通知）。行为与旧版几乎一致：
            // 设 Sending 状态 + 发 Enter。
            //
            // **为什么有 BeginPending 还要保留 Begin 路径**：libei 模
            // 型是 compositor 端先捕获，主线程从 `Activated` 信号才知
            // 道。Begin 直接对应"已经在捕获中"。Windows / macOS 的
            // BeginPending 后 Ack 到达 → capture.start_capture → 后端
            // 主动 emit Begin（见 event_thread.rs::update_clients 的
            // StartCapture 分支）。
            CaptureEvent::Begin => {
                // Begin 的两种来源：
                // 1. **Pending → Active 提升**（Windows / macOS 路径）：
                //    BeginPending → Enter → Ack → capture.start_capture → 后端
                //    主动 emit Begin。这种情况下 Enter **已经为 BeginPending
                //    发过**，这里**不**能再发（重复 Enter 会让对端重复
                //    ReleaseNotify + add_incoming，状态错乱）。
                // 2. **libei 异步路径**：compositor 已捕获，backend 第一次
                //    emit Begin，没有前置 Enter，需要现在发。
                //
                // 区分依据：`State::Pending { handle, .. }` 且 handle 匹配
                // → 路径 1；其他（Idle / Sending）→ 路径 2。
                match self.state {
                    State::Pending { handle: pending_h, .. } if pending_h == handle => {
                        log::info!(
                            "capture: pending -> active promotion (handle={handle:?}, \
                             Begin already Entered for BeginPending)"
                        );
                        if Some(handle) != self.active_client {
                            self.active_client.replace(handle);
                            self.event_tx
                                .send(ICaptureEvent::ClientEntered(handle))
                                .expect("channel closed");
                        }
                        // **不**发 Enter —— 见上方路径 1 说明。
                        self.state = State::Sending;
                        // **FIX 4**：Pending → Sending 是 happy path 主
                        // 推进点之一；刷新 last_progress_at 让 heavy
                        // tick 不把它当 no-progress。
                        self.watchdog.last_progress_at = Instant::now();
                        self.watchdog.consecutive_send_failures = 0;
                        Ok(())
                    }
                    _ => {
                        if Some(handle) != self.active_client {
                            log::info!("capture: new client entered (handle={handle:?})");
                            self.active_client.replace(handle);
                            self.event_tx
                                .send(ICaptureEvent::ClientEntered(handle))
                                .expect("channel closed");
                        }
                        self.state = State::Sending;
                        let opposite_pos = to_proto_pos(self.get_pos(handle).opposite());
                        if let Err(e) = self.conn.send(ProtoEvent::Enter(opposite_pos), handle).await {
                            const DUR: Duration = Duration::from_millis(500);
                            debounce!(PREV_LOG, DUR, log::warn!("releasing capture: {e}"));
                            log::warn!("releasing capture: send failed: {e}");
                            self.watchdog.consecutive_send_failures = self
                                .watchdog
                                .consecutive_send_failures
                                .saturating_add(1);
                            capture.release().await?;
                        } else {
                            // **FIX 4**：Begin 路径（同进程内第 2 次
                            // Enter）发送成功也算一次进度推进。
                            self.watchdog.consecutive_send_failures = 0;
                            self.watchdog.last_progress_at = Instant::now();
                        }
                        Ok(())
                    }
                }
            }

            // **普通输入事件**：仅在 Sending 状态转发。Pending / Idle 时
            // 是"鼠标还没真切走"，不应当出现在这里（如果出现，说明后端
            // 在不该吞事件时吞了 —— 记 warning 便于诊断，正常 drop）。
            CaptureEvent::Input(e) => match self.state {
                State::Sending => {
                    if let Err(err) = self.conn.send(ProtoEvent::Input(e), handle).await {
                        const DUR: Duration = Duration::from_millis(500);
                        debounce!(PREV_LOG, DUR, log::warn!("releasing capture: {err}"));
                        log::warn!("releasing capture: send failed: {err}");
                        // **FIX 4**：连续 send 失败累加；heavy tick
                        // 在 ≥ 3 次时强制 release（即使本路径 release
                        // 已调），做兜底。
                        self.watchdog.consecutive_send_failures = self
                            .watchdog
                            .consecutive_send_failures
                            .saturating_add(1);
                        capture.release().await?;
                    } else {
                        // **FIX 4**：成功 send 视为"还在干活"——清零失
                        // 败计数、刷新 last_progress_at 让 no-progress
                        // 不会误触发。
                        self.watchdog.consecutive_send_failures = 0;
                        self.watchdog.last_progress_at = Instant::now();
                    }
                    Ok(())
                }
                State::Pending { .. } | State::Idle => {
                    log::warn!(
                        "capture: Input event arrived while state={:?} — dropping (host capture inactive)",
                        self.state
                    );
                    Ok(())
                }
            },
        }
    }

    async fn release_capture(&mut self, capture: &mut InputCapture) -> Result<(), CaptureError> {
        log::info!(
            "release_capture: ENTER (state={:?}, active_client={:?})",
            self.state,
            self.active_client,
        );

        // **Pending-capture 特殊路径**：如果当前还在等 Ack，宿主端还没
        // 真正捕获过鼠标（active_client = None，pressed_keys 仍可能为
        // 空），不需要发 Leave / key-ups / modifiers=0 —— 因为从没发过
        // 被 Ack 的 Enter。仅通知后端清 PENDING_CLIENT。
        //
        // **触发场景**：
        // 1. release-bind 在 pending 状态被按下
        // 2. service.rs 主动 release（用户操作）
        // 3. capture::Capture 的 Drop / destroy
        if let State::Pending { handle, .. } = self.state {
            log::info!(
                "release_capture: was in Pending for handle {handle} - \
                 cancel_pending (no Leave to send)"
            );
            if let Err(e) = capture.cancel_pending(self.get_pos(handle)) {
                log::warn!("cancel_pending in release_capture: {e}");
            }
            self.state = State::Idle;
            // 后端 Windows 消息循环顺带清 PENDING（`RequestType::Release`
            // 分支 belt-and-suspenders 已加），无需再调 capture.release()。
            // 但为 macOS 兜底，这里也调一次 —— 后端 `release()` 在
            // pending 状态下是 no-op（macOS pending_pos 不依赖 ACTIVE
            // 状态，已被 StartCapture/CancelPending 异步处理过）。
            let res = capture.release().await;
            // **FIX 4**：Pending 早返回路径也要清 watchdog（避免
            // 用户在 pending 期间按 release-bind → 立刻又跨过去 →
            // recent_crossings 含旧条目被误判为风暴）。
            self.watchdog.consecutive_send_failures = 0;
            self.watchdog.recent_crossings.clear();
            self.watchdog.last_progress_at = Instant::now();
            return res;
        }

        // If we have an active client, notify them we're leaving
        if let Some(handle) = self.active_client.take() {
            let held_keys = capture.take_pressed_keys();
            log::info!(
                "release_capture: synthesizing {} key-up events to client {handle}",
                held_keys.len()
            );
            // Synthesize key-up events for every key still held in the
            // capture's pressed_keys set BEFORE sending Leave. Without
            // this, pressing the release-bind chord (typically all four
            // modifiers) leaves the peer with phantom held modifiers:
            // the down events were forwarded while capture was active,
            // but the matching up events arrive after the local tap
            // flips to passthrough and never reach the peer. The peer
            // then runs every subsequent keystroke through those held
            // mods until its watchdog times out (1+ s) or our Leave
            // arrives — and Leave can be lost over UDP.
            for key in held_keys {
                let key_up = ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                    time: 0,
                    key: key as u32,
                    state: 0,
                }));
                if let Err(e) = self.conn.send(key_up, handle).await {
                    log::warn!("failed to send key-up to client {handle}: {e}");
                }
            }
            // Reset the modifier mask too. The peer's input-emulation
            // layer keeps a separate XKB-style modifier state that's
            // updated by KeyboardEvent::Modifiers, distinct from the
            // pressed_keys set drained above. Without this, an
            // already-locked CapsLock would survive the release.
            log::info!("release_capture: sending modifiers=0 to client {handle}");
            let mods_zero = ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Modifiers {
                depressed: 0,
                latched: 0,
                locked: 0,
                group: 0,
            }));
            if let Err(e) = self.conn.send(mods_zero, handle).await {
                log::warn!("failed to reset modifiers on client {handle}: {e}");
            }

            log::info!("release_capture: sending Leave to client {handle}");
            if let Err(e) = self.conn.send(ProtoEvent::Leave(0), handle).await {
                log::warn!("failed to send Leave to client {handle}: {e}");
            }
        } else {
            log::info!("release_capture: no active_client, skipping Leave send");
        }
        log::info!("release_capture: calling capture.release() (OS-level release)");
        let res = capture.release().await;
        log::info!("release_capture: capture.release() returned (ok={})", res.is_ok());

        // **FIX 4**：release_capture 完成 → 重置 watchdog 状态。
        //
        // - 清零 `consecutive_send_failures`：下次 BeginPending / Input
        //   重新计数。
        // - 清空 `recent_crossings`：跨边风暴的滑动窗口从 now 起重新计。
        // - 刷新 `last_progress_at`：刚释放完成视作一次进度（no-progress
        //   超时从 now 重新算）。
        //
        // 任何分支（Pending 早返回 / Sending 路径 / leave/send-failure）
        // 走完 release 都重置 —— watchdog 的"已恢复"哨兵。
        self.watchdog.consecutive_send_failures = 0;
        self.watchdog.recent_crossings.clear();
        self.watchdog.last_progress_at = Instant::now();

        res
    }
}

thread_local! {
    static PREV_LOG: Cell<Option<Instant>> = const { Cell::new(None) };
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Idle,
    /// Mouse crossed barrier, [`ProtoEvent::Enter`] sent, waiting for
    /// the client's [`ProtoEvent::Ack`]. Capture is NOT yet active on the
    /// backend (the host cursor is still visible). After 500ms without
    /// Ack the [`crate::capture::CaptureTask`] cancels and goes back to
    /// [`State::Idle`].
    Pending {
        handle: CaptureHandle,
        started: Instant,
    },
    /// Capture is active (backend has promoted pending → active on Ack,
    /// or backend is libei-style async). Input events are forwarded to
    /// the remote.
    Sending,
}

fn to_capture_pos(pos: lan_mouse_ipc::Position) -> input_capture::Position {
    match pos {
        lan_mouse_ipc::Position::Left => input_capture::Position::Left,
        lan_mouse_ipc::Position::Right => input_capture::Position::Right,
        lan_mouse_ipc::Position::Top => input_capture::Position::Top,
        lan_mouse_ipc::Position::Bottom => input_capture::Position::Bottom,
    }
}

fn to_proto_pos(pos: input_capture::Position) -> lan_mouse_proto::Position {
    match pos {
        input_capture::Position::Left => lan_mouse_proto::Position::Left,
        input_capture::Position::Right => lan_mouse_proto::Position::Right,
        input_capture::Position::Top => lan_mouse_proto::Position::Top,
        input_capture::Position::Bottom => lan_mouse_proto::Position::Bottom,
    }
}

struct DropGuard<T> {
    tx: Sender<T>,
    on_drop: Option<T>,
}

impl<T> DropGuard<T> {
    fn new(tx: Sender<T>, on_new: T, on_drop: T) -> Self {
        tx.send(on_new).expect("channel closed");
        let on_drop = Some(on_drop);
        Self { tx, on_drop }
    }
}

impl<T> Drop for DropGuard<T> {
    fn drop(&mut self) {
        self.tx
            .send(self.on_drop.take().expect("item"))
            .expect("channel closed");
    }
}
