use std::{
    cell::{Cell, RefCell},
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
                                    // **补丁（修复 2）**：迟到的 Ack 不再盲目切 Sending。
                                    //
                                    // **bug**：Master 端 500ms Pending 超时
                                    // 取消后（state 已 Idle），Slave 这边才
                                    // 把 Ack 发到。命中 `_` 分支 → 直接把
                                    // State::Sending 设回去。后端 OS 层
                                    // 没激活、`active_client` 是空，结果是
                                    // State 跟 backend 状态错位 —— 用户感
                                    // 受为"鼠标抖一下又卡住"。
                                    //
                                    // **修法**：只有当本地仍有 `active_client`
                                    // 时才接受 Ack 进 Sending；否则日志告警
                                    // + 保持 Idle，让下一轮 BeginPending 自
                                    // 然覆盖。
                                    if self.active_client.is_some() {
                                        log::info!("client {handle} acknowledged the connection!");
                                        self.state = State::Sending;
                                    } else {
                                        log::warn!(
                                            "client {handle} acknowledged -- but no active_client \
                                             (stale Ack after timeout, ignoring)"
                                        );
                                    }
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
                _ = self.cancellation_token.cancelled() => break,
            }
        }
        Ok(())
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
                    // 不要走 capture.release()：pending 状态后端本来就没
                    // 激活，无 Leave 要发。
                    return Ok(());
                }
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
                            capture.release().await?;
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
                        capture.release().await?;
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
            return capture.release().await;
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
        // **补丁（修复 1）**：`release_capture` 末尾强制 `state = State::Idle`。
        //
        // **bug**：原逻辑 Pending 分支显式置 Idle（行 670），Sending 分支
        // 只 `active_client.take()` + 发 Leave + 调 `capture.release()`
        // —— State 字段整次调用下来都不归位。下一次发过来的 motion 落到
        // `State::Sending` 分支但 `active_client` 已是 None → `conn.send`
        // 返 NotConnected → 反复 release_capture 但 State 仍未变，UI
        // 体感"鼠标抖一下又不动了"。
        //
        // **修法**：与 Pending 分支对齐 —— 在调 `capture.release()` 之前
        // 置 Idle，状态机和后端 OS 状态保持一致。
        log::info!("release_capture: setting state = Idle (force-reset)");
        self.state = State::Idle;

        log::info!("release_capture: calling capture.release() (OS-level release)");
        let res = capture.release().await;
        log::info!("release_capture: capture.release() returned (ok={})", res.is_ok());
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
