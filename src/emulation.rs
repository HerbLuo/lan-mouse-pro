use crate::config::local_commit;
use crate::listen::{LanMouseListener, ListenEvent, ListenerCreationError};
use futures::StreamExt;
use input_emulation::{EmulationHandle, InputEmulation, InputEmulationError};
use input_event::Event;
use lan_mouse_proto::{PROTOCOL_MAGIC, Position, ProtoEvent};
use local_channel::mpsc::{Receiver, Sender, channel};
use std::{
    cell::Cell,
    collections::HashMap,
    net::SocketAddr,
    rc::Rc,
    time::{Duration, Instant},
};
use tokio::{
    select,
    task::{JoinHandle, spawn_local},
};

/// emulation handling events received from a listener
pub(crate) struct Emulation {
    task: JoinHandle<()>,
    request_tx: Sender<EmulationRequest>,
    event_rx: Receiver<EmulationEvent>,
}

pub(crate) enum EmulationEvent {
    Connected {
        addr: SocketAddr,
        fingerprint: String,
    },
    ConnectionAttempt {
        fingerprint: String,
    },
    /// new connection
    Entered {
        /// address of the connection
        addr: SocketAddr,
        /// position of the connection
        pos: lan_mouse_ipc::Position,
        /// certificate fingerprint of the connection
        fingerprint: String,
    },
    /// connection closed
    Disconnected {
        addr: SocketAddr,
    },
    /// the port of the listener has changed
    PortChanged(Result<u16, ListenerCreationError>),
    /// emulation was disabled
    EmulationDisabled,
    /// emulation was enabled
    EmulationEnabled,
    /// capture should be released
    ReleaseNotify,
    /// peer sent us a Hello with its build commit hash. Used to
    /// populate `client_manager.peer_commit` from the listen side
    /// too — without this, peer-version visibility silently fails
    /// whenever the outgoing connection in the *other* direction is
    /// broken (one-way setups, asymmetric NAT, peer's TCP listener
    /// down). The connect-side path stays as the primary source;
    /// this is the defensive fallback.
    PeerHello {
        addr: SocketAddr,
        commit: [u8; 8],
    },
}

enum EmulationRequest {
    Reenable,
    Release(SocketAddr),
    ChangePort(u16),
    Terminate,
}

impl Emulation {
    pub(crate) fn new(
        backend: Option<input_emulation::Backend>,
        listener: LanMouseListener,
    ) -> Self {
        let emulation_proxy = EmulationProxy::new(backend);
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let emulation_task = ListenTask {
            listener,
            emulation_proxy,
            request_rx,
            event_tx,
            // **STEP-8.2 修复 — Bug #6**：addr_to_fingerprint map 初始化
            // 空，由 ListenTask 在 `ListenEvent::Accept` 分支填充、
            // `ListenEvent::Disconnected` 分支清理。详见 struct 字段
            // docstring + 旧 `get_certificate_fingerprint` stub 在
            // `listen.rs:316` 的死代码分析。
            addr_to_fingerprint: HashMap::new(),
        };
        let task = spawn_local(emulation_task.run());
        Self {
            task,
            request_tx,
            event_rx,
        }
    }

    pub(crate) fn send_leave_event(&self, addr: SocketAddr) {
        self.request_tx
            .send(EmulationRequest::Release(addr))
            .expect("channel closed");
    }

    pub(crate) fn reenable(&self) {
        self.request_tx
            .send(EmulationRequest::Reenable)
            .expect("channel closed");
    }

    pub(crate) fn request_port_change(&self, port: u16) {
        self.request_tx
            .send(EmulationRequest::ChangePort(port))
            .expect("channel closed")
    }

    pub(crate) async fn event(&mut self) -> EmulationEvent {
        self.event_rx.recv().await.expect("channel closed")
    }

    /// wait for termination
    pub(crate) async fn terminate(&mut self) {
        log::debug!("terminating emulation");
        self.request_tx
            .send(EmulationRequest::Terminate)
            .expect("channel closed");
        if let Err(e) = (&mut self.task).await {
            log::warn!("{e}");
        }
    }
}

struct ListenTask {
    listener: LanMouseListener,
    emulation_proxy: EmulationProxy,
    request_rx: Receiver<EmulationRequest>,
    event_tx: Sender<EmulationEvent>,
    /// **STEP-8.2 修复 — Bug #6**：`addr -> client cert fingerprint` map。
    ///
    /// **来源**：`ListenEvent::Accept { addr, fingerprint }` 是 supervisor
    /// 计算 client cert fingerprint 后唯一送给 ListenTask 的入口；本字段
    /// 在 Accept 分支插入、在 Disconnected 分支移除。
    ///
    /// **用途**：`ProtoEvent::Enter` 处理时查 map 拿 fingerprint（与
    /// mTLS verified 的 peer 关联）→ 进 `EmulationEvent::Entered` 上报
    /// service（service.rs:323 用来 `add_incoming` + 通知 frontend）。
    ///
    /// **修前**：`emulation.rs:152` 调 `self.listener.get_certificate_
    /// fingerprint(addr)`，但 `listen.rs:316` 该方法是 dead-code stub
    /// 直接返 None → 整个 `if let Some(fingerprint) = ...` 分支跳过 →
    /// Enter 收下后**不 release capture、不 reply Ack、不发
    /// EmulationEvent::Entered** —— 远程侧 enter 后无任何后续行为，
    /// 看起来"收到了 Enter 但没反应"。
    addr_to_fingerprint: HashMap<SocketAddr, String>,
}

impl ListenTask {
    async fn run(mut self) {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        let mut last_response = HashMap::new();
        let mut rejected_connections = HashMap::new();
        loop {
            select! {
                e = self.listener.next() => {match e {
                    Some(ListenEvent::Msg { event, addr }) => {
                        log::trace!("{event} <-<-<-<-<- {addr}");
                        last_response.insert(addr, Instant::now());
                        match event {
                            ProtoEvent::Enter(pos) => {
                                // **STEP-8.2 修复 — Bug #6**：直接查
                                // `addr_to_fingerprint` map 拿 fingerprint
                                // （不再调 `listener.get_certificate_fingerprint`
                                // —— 那是 `listen.rs:316` 的 dead-code stub，
                                // 永远返 None 让整个 Enter 处理被跳过）。
                                //
                                // **map 来源**：`ListenEvent::Accept`
                                // 分支在 mTLS + supervisor 计算 fingerprint
                                // 后 insert；`ListenEvent::Disconnected`
                                // 分支 remove。
                                //
                                // **map 缺失兜底**：Accept 在 supervisor
                                // 启动后**立刻**发，理论上 Accept 总在
                                // Enter 之前（server_hello → Accept → client
                                // 才进 capture 触发 Enter）。如果 map 缺
                                // fp（理论上不应该 —— 但 race / 测试场景
                                // 可能出现），用空字符串占位让流程不卡死
                                // （service 端 add_incoming 用 "" 也是
                                // 合法值 —— M1 不校验）。
                                let fingerprint = self.addr_to_fingerprint.get(&addr).cloned()
                                    .unwrap_or_default();
                                log::info!("releasing capture: {addr} entered this device (fp={fingerprint})");
                                self.event_tx.send(EmulationEvent::ReleaseNotify).expect("channel closed");
                                log::info!("emulation: sending Ack(0) to {addr} (responding to master Enter)");
                                self.listener.reply(addr, ProtoEvent::Ack(0)).await;
                                self.event_tx.send(EmulationEvent::Entered { addr, pos: to_ipc_pos(pos), fingerprint }).expect("channel closed");
                            }
                            ProtoEvent::Leave(_) => {
                                log::info!(
                                    "emulation: received Leave from {addr} — removing emulation_proxy"
                                );
                                self.emulation_proxy.remove(addr);
                                self.listener.reply(addr, ProtoEvent::Ack(0)).await;
                            }
                            ProtoEvent::Input(event) => self.emulation_proxy.consume(event, addr),
                            ProtoEvent::Ping => self.listener.reply(addr, ProtoEvent::Pong(self.emulation_proxy.emulation_active.get())).await,
                            // Peer's version handshake. Echo our own
                            // commit back so the peer's connect-side
                            // receive_loop populates its `peer_commit`,
                            // AND publish a PeerHello upward so our
                            // service can populate ours from the listen
                            // side too — the connect side is the primary
                            // path, but if the outbound direction is
                            // broken (one-way setup, NAT, peer's TCP
                            // listener down) the version display would
                            // otherwise silently say "unknown" while
                            // the peer is in fact happily talking to us.
                            ProtoEvent::Hello { magic: _, commit } => {
                                // The magic check happens in quic_transport.rs
                                // (STEP-3.2). At this receive site we only
                                // echo the commit back so the peer can
                                // populate its peer_commit field.
                                self.listener.reply(addr, ProtoEvent::Hello { magic: PROTOCOL_MAGIC, commit: local_commit() }).await;
                                self.event_tx.send(EmulationEvent::PeerHello { addr, commit }).expect("channel closed");
                            }
                            _ => {}
                        }
                    }
                    Some(ListenEvent::Accept { addr, fingerprint }) => {
                        // **STEP-8.2 修复 — Bug #6**：同时把 fingerprint
                        // 存进 `addr_to_fingerprint` map，供 Enter 处理
                        // 查询（替代旧的 `listener.get_certificate_
                        // fingerprint` dead-code stub）。
                        log::debug!("ListenTask: peer {addr} accepted with fingerprint {fingerprint}");
                        self.addr_to_fingerprint.insert(addr, fingerprint.clone());
                        self.event_tx.send(EmulationEvent::Connected { addr, fingerprint }).expect("channel closed");
                    }
                    Some(ListenEvent::Rejected { fingerprint }) => {
                        if rejected_connections.insert(fingerprint.clone(), Instant::now())
                            .is_none_or(|i| i.elapsed() >= Duration::from_secs(2)) {
                                self.event_tx.send(EmulationEvent::ConnectionAttempt { fingerprint }).expect("channel closed");
                            }
                    }
                    Some(ListenEvent::Disconnected { addr }) => {
                        // STEP-6.2b + STEP-6.3 合并：supervisor 在 stream A EOF /
                        // conn close 时推 Disconnected。
                        //
                        // **race 修复（STEP-6.3 Leader 评审）**：supervisor 路径
                        // 与 timeout 路径（interval.tick 检测到 last_response 超 1s）
                        // 可能并发触发同一 addr 的两次 `EmulationEvent::Disconnected`。
                        //
                        // 把 `last_response.remove(&addr)` 提到 supervisor 路径
                        // 后，timeout 路径改为 `if last_response.remove(&addr).is_some()`
                        // 形式 —— supervisor 路径赢得 race（supervisor 是 conn
                        // 真实关闭的明确信号；timeout 仅是 1s 心跳兜底）。
                        //
                        // **STEP-8.2 修复 — Bug #6**：同时清理 `addr_to_fingerprint`
                        // map（peer 重连会触发新的 Accept 重填 fingerprint，
                        // 但旧 fingerprint 不能残留）。
                        log::info!("peer {addr} disconnected (supervisor)");
                        self.addr_to_fingerprint.remove(&addr);
                        last_response.remove(&addr);
                        self.emulation_proxy.remove(addr);
                        self.event_tx.send(EmulationEvent::Disconnected { addr }).expect("channel closed");
                    }
                    None => break
                }}
                event = self.emulation_proxy.event() => {
                    self.event_tx.send(event).expect("channel closed");
                }
                request = self.request_rx.recv() => match request.expect("channel closed") {
                    // reenable emulation
                    EmulationRequest::Reenable => self.emulation_proxy.reenable(),
                    // notify the other end that we hit a barrier (should release capture)
                    EmulationRequest::Release(addr) => self.listener.reply(addr, ProtoEvent::Leave(0)).await,
                    EmulationRequest::ChangePort(port) => {
                        self.listener.request_port_change(port);
                        let result = self.listener.port_changed().await;
                        self.event_tx.send(EmulationEvent::PortChanged(result)).expect("channel closed");
                    }
                    EmulationRequest::Terminate => break,
                },
                _ = interval.tick() => {
                    // STEP-6.3 race 修复：与 supervisor 路径（ListenEvent::Disconnected
                    // 臂）的 `last_response.remove(&addr)` 配合，让 supervisor 路径
                    // 赢得 race。
                    //
                    // 如果 supervisor 已经把 addr 从 last_response 摘走（conn 真
                    // 关了）→ `last_response.remove(&addr).is_none()`，timeout
                    // 路径 no-op，不重复上报 Disconnected。
                    last_response.retain(|&addr, instant| {
                        if instant.elapsed() > Duration::from_secs(1) {
                            log::warn!("releasing keys: {addr} not responding!");
                            self.emulation_proxy.remove(addr);
                            self.event_tx.send(EmulationEvent::Disconnected { addr }).expect("channel closed");
                            false
                        } else {
                            true
                        }
                    });
                }
            }
        }
        self.listener.terminate().await;
        self.emulation_proxy.terminate().await;
    }
}

/// proxy handling the actual input emulation,
/// discarding events when it is disabled
pub(crate) struct EmulationProxy {
    emulation_active: Rc<Cell<bool>>,
    exit_requested: Rc<Cell<bool>>,
    request_tx: Sender<ProxyRequest>,
    event_rx: Receiver<EmulationEvent>,
    task: JoinHandle<()>,
}

enum ProxyRequest {
    Input(Event, SocketAddr),
    Remove(SocketAddr),
    Terminate,
    Reenable,
}

impl EmulationProxy {
    fn new(backend: Option<input_emulation::Backend>) -> Self {
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let emulation_active = Rc::new(Cell::new(false));
        let exit_requested = Rc::new(Cell::new(false));
        let emulation_task = EmulationTask {
            backend,
            exit_requested: exit_requested.clone(),
            request_rx,
            event_tx,
            handles: Default::default(),
            next_id: 0,
        };
        let task = spawn_local(emulation_task.run());
        Self {
            emulation_active,
            exit_requested,
            request_tx,
            task,
            event_rx,
        }
    }

    async fn event(&mut self) -> EmulationEvent {
        let event = self.event_rx.recv().await.expect("channel closed");
        if let EmulationEvent::EmulationEnabled = event {
            self.emulation_active.replace(true);
        }
        if let EmulationEvent::EmulationDisabled = event {
            self.emulation_active.replace(false);
        }
        event
    }

    fn consume(&self, event: Event, addr: SocketAddr) {
        // ignore events if emulation is currently disabled
        if self.emulation_active.get() {
            self.request_tx
                .send(ProxyRequest::Input(event, addr))
                .expect("channel closed");
        }
    }

    fn remove(&self, addr: SocketAddr) {
        self.request_tx
            .send(ProxyRequest::Remove(addr))
            .expect("channel closed");
    }

    fn reenable(&self) {
        self.request_tx
            .send(ProxyRequest::Reenable)
            .expect("channel closed");
    }

    async fn terminate(&mut self) {
        self.exit_requested.replace(true);
        self.request_tx
            .send(ProxyRequest::Terminate)
            .expect("channel closed");
        let _ = (&mut self.task).await;
    }
}

struct EmulationTask {
    backend: Option<input_emulation::Backend>,
    exit_requested: Rc<Cell<bool>>,
    request_rx: Receiver<ProxyRequest>,
    event_tx: Sender<EmulationEvent>,
    handles: HashMap<SocketAddr, EmulationHandle>,
    next_id: EmulationHandle,
}

impl EmulationTask {
    async fn run(mut self) {
        loop {
            if let Err(e) = self.do_emulation().await {
                log::warn!("input emulation exited: {e}");
            }
            if self.exit_requested.get() {
                break;
            }
            // wait for reenable request
            loop {
                match self.request_rx.recv().await.expect("channel closed") {
                    ProxyRequest::Reenable => break,
                    ProxyRequest::Terminate => return,
                    ProxyRequest::Input(..) => { /* emulation inactive => ignore */ }
                    ProxyRequest::Remove(..) => { /* emulation inactive => ignore */ }
                }
            }
        }
    }

    async fn do_emulation(&mut self) -> Result<(), InputEmulationError> {
        log::info!("creating input emulation ...");
        let mut emulation = tokio::select! {
            r = InputEmulation::new(self.backend) => r?,
            // allow termination event while requesting input emulation
            _ = wait_for_termination(&mut self.request_rx) => return Ok(()),
        };

        // used to send enabled and disabled events
        let _emulation_guard = DropGuard::new(
            self.event_tx.clone(),
            EmulationEvent::EmulationEnabled,
            EmulationEvent::EmulationDisabled,
        );

        // create active handles
        if let Err(e) = self.create_clients(&mut emulation).await {
            emulation.terminate().await;
            return Err(e);
        }

        let res = self.do_emulation_session(&mut emulation).await;
        // FIXME replace with async drop when stabilized
        emulation.terminate().await;
        res
    }

    async fn create_clients(
        &mut self,
        emulation: &mut InputEmulation,
    ) -> Result<(), InputEmulationError> {
        for handle in self.handles.values() {
            tokio::select! {
                _ = emulation.create(*handle) => {},
                _ = wait_for_termination(&mut self.request_rx) => return Ok(()),
            }
        }
        Ok(())
    }

    async fn do_emulation_session(
        &mut self,
        emulation: &mut InputEmulation,
    ) -> Result<(), InputEmulationError> {
        loop {
            tokio::select! {
                e = self.request_rx.recv() => match e.expect("channel closed") {
                    ProxyRequest::Input(event, addr) => {
                        let handle = match self.handles.get(&addr) {
                            Some(&handle) => handle,
                            None => {
                                let handle = self.next_id;
                                self.next_id += 1;
                                emulation.create(handle).await;
                                self.handles.insert(addr, handle);
                                handle
                            }
                        };
                        emulation.consume(event, handle).await?;
                    },
                    ProxyRequest::Remove(addr) => {
                        if let Some(handle) = self.handles.remove(&addr) {
                            emulation.destroy(handle).await;
                        }
                    }
                    ProxyRequest::Terminate => break Ok(()),
                    ProxyRequest::Reenable => continue,
                },
            }
        }
    }
}

fn to_ipc_pos(pos: Position) -> lan_mouse_ipc::Position {
    match pos {
        Position::Left => lan_mouse_ipc::Position::Left,
        Position::Right => lan_mouse_ipc::Position::Right,
        Position::Top => lan_mouse_ipc::Position::Top,
        Position::Bottom => lan_mouse_ipc::Position::Bottom,
    }
}

async fn wait_for_termination(rx: &mut Receiver<ProxyRequest>) {
    loop {
        match rx.recv().await.expect("channel closed") {
            ProxyRequest::Terminate => return,
            ProxyRequest::Input(_, _) => continue,
            ProxyRequest::Remove(_) => continue,
            ProxyRequest::Reenable => continue,
        }
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
