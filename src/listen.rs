//! QUIC server listener —— **M1 阶段 STEP-6.3 整合 macOS wake**。
//!
//! 替换 STEP-1.2 之前 `webrtc-dtls` DTLS 路径：原 `listen.rs::read_loop`
//! 走 `webrtc_util::Conn` + `DTLSConn` + `as_any().downcast_ref::<DTLSConn>()`
//! 旧路径（10 errors）整段删除，由 quic_transport 模块的 `PeerSession` +
//! `read_frame` + `read_any_frame` 替代。
//!
//! **STEP-6.2 + STEP-6.3 supervisor 形态**：
//! 1. `LanMouseListener::new(port, cert_chain, key, authorized_keys)` 调
//!    `endpoint_with_verifier(port, cert_chain, key, AuthorizedKeysVerifier)`
//!    拿 `Endpoint`（mTLS + fingerprint allowlist 在握手期已完成）
//! 2. spawn per-listener accept task：循环 `quic_transport::accept()`，每条
//!    `Connection` spawn 一个 `handle_quic_peer_supervisor` task
//! 3. supervisor: `server_hello` → 计算 fingerprint（双层防御）→ 推
//!    `ListenEvent::Accept` → `take_stream_a_recv` 拿 stream A recv 半边
//!    → 循环 `read_frame(&mut recv_a)` 转译为 `ListenEvent::Msg`
//! 4. stream A EOF / conn close → 推 `ListenEvent::Disconnected` + 退 supervisor
//!
//! **macOS wake 路径（STEP-6.3 引入）**：
//! - macOS-only `PowerObserver` 在系统唤醒时通过 `tokio::sync::mpsc::unbounded_channel`
//!   发 `()` 给 `wake_rx`
//! - `spawn_wake_task` 后台 task 阻塞 recv `wake_rx`，收到后遍历 `quic_conns`
//!   注册表，对每条 conn 调 `peer.connection().close(0u32.into(), b"wake")`
//!   同步触发 close（不等 QUIC 30s `max_idle_timeout`）
//! - close 后 read_loop EOF → supervisor 退场 → 推 Disconnected
//! - 非 macOS 上 `PowerObserver` 不 spawn，`wake_rx = None`，`spawn_wake_task`
//!   永久 pending
//!
//! **为什么不装配三 stream（B/C）**：M1 阶段 client 端 `LanMouseConnection::send`
//! **只**在 stream A 上发控制事件（Enter / Leave / Ack / Hello / Ping / Pong
//! 走 `send_stream_a`，按键走 `send_stream_b` 但 LanMouseConnection 当前
//! 仅在 `send_input` 的 `Channel::StreamB` 分派触发），且 client 端**没有**
//! 自动开 3 条 bidi 的装配路径（`connect_to_handle` 只跑 `client_hello`，
//! stream B/C 留给 send 时按需开）。因此 server 端 supervisor 只监听
//! stream A —— listen.rs ListenTask 的现有 match 臂覆盖所有控制面事件，
//! 不依赖 stream B/C reader。
//!
//! **Stream B / C 路径留 STEP-7.x 接手**：届时 supervisor 装配 outer
//! accept_bi 循环 + 子 task 用 `read_any_frame` 解码 + 转译 `ListenEvent::Msg`。
//!
//! **port_changed / request_port_change**：M1 阶段 `Endpoint` 不支持运行
//! 期端口切换 → `Err(PortChangeUnsupported)`（与 bak 一致；后续微步再补
//! per-IP endpoint rebuild）。
//!
//! **#S-9 治理**：allowlist value 类型用 `String`（M1）；M2 接
//! `IncomingPeerConfig` 时同步改。
//!
//! **dead_code 守门**：`ArcConn` / `DTLSConn` / `VerifyPeerCertificateFn` 等
//! 老类型引用整段删除；emulation.rs `ListenTask` 通过 `ListenEvent` 流式
//! 消费 `Msg / Accept / Rejected / Disconnected` 4 变体。
//!
//! **STEP-8.2 修复 — mTLS reject 反向通知路径**：
//! `ListenEvent::Rejected { fingerprint }` 之前是死代码（`AuthorizedKeysVerifier
//! ::verify_client_cert` 在 rustls 拒握时直接 `Err(rustls::Error)`，
//! `quinn::Endpoint::accept` 不暴露被拒 cert 的 fingerprint，listen.rs
//! supervisor 永远收不到这条事件 → GUI 不弹窗 → 用户看不见"未授权对端尝试
//! 接入"的提示）。修复方案：
//! 1. `AuthorizedKeysVerifier` 加 `rejection_tx: Option<UnboundedSender<String>>`
//!    字段（`with_rejection_tx` builder 注入）
//! 2. `verify_client_cert` 在 Err 路径上 `rejection_tx.send(fp)`（best-effort）
//! 3. `LanMouseListener::new` 创 `tokio::sync::mpsc::unbounded_channel::<String>()`
//!    → 把 tx clone 给 verifier + spawn `spawn_rejection_forwarder_task` 在
//!    spawn_local 上把 `rx.recv()` 翻译成 `ListenEvent::Rejected { fingerprint }`
//!    走同一 `listen_tx`（与 Accept / Msg / Disconnected 同一通道）
//! 4. `terminate()` 把 forwarder task 也 abort（与 `wake_task` 同模式）
//!
//! 这样 `AuthorizedKeysVerifier` 拒握时 fingerprint 即时送到
//! `EmulationTask::ListenTask` 已有 match 分支（emulation.rs:190）→
//! `EmulationEvent::ConnectionAttempt` → `FrontendEvent::ConnectionAttempt` →
//! 前端 `request_authorization` 弹窗。

use futures::{Stream, StreamExt};
use lan_mouse_proto::ProtoEvent;
use local_channel::mpsc::{Receiver, Sender, channel};
use rustls::pki_types::CertificateDer;
use std::{
    cell::RefCell,
    collections::HashMap,
    net::SocketAddr,
    rc::Rc,
    sync::{Arc, RwLock},
};
use thiserror::Error;
use tokio::task::{JoinHandle, spawn_local};

use crate::crypto;
use crate::quic_transport::{self, AuthorizedKeysVerifier, PeerSession};

#[derive(Error, Debug)]
pub enum ListenerCreationError {
    #[error("port change not supported for QUIC endpoints")]
    PortChangeUnsupported,
    #[error(transparent)]
    Quic(#[from] quic_transport::Error),
}

pub(crate) enum ListenEvent {
    Msg {
        event: ProtoEvent,
        addr: SocketAddr,
    },
    Accept {
        addr: SocketAddr,
        fingerprint: String,
    },
    /// Peer 连接断开（supervisor 任一 reader task 退出 / conn close）。
    ///
    /// STEP-6.2 引入：QUIC 路径的 supervisor 在 stream A EOF / conn close 时
    /// 发本事件 → emulation.rs ListenTask 同步清理 `emulation_proxy[addr]`
    /// + 上报 service。
    Disconnected {
        addr: SocketAddr,
    },
    /// Peer 握手失败 / fingerprint 未授权（mTLS 阶段被拒）。
    ///
    /// **STEP-8.2 修复**：由 [`crate::quic_transport::AuthorizedKeysVerifier`]
    /// 通过反向 channel (`tokio::sync::mpsc::UnboundedSender<String>`) 通
    /// 知 `spawn_rejection_forwarder_task`，后者把 fingerprint 翻译为本事件
    /// 走 `listen_tx` 同一条流。
    ///
    /// **为什么需要反向 channel（而不是 rustls 拒握后从 quinn `Connection`
    /// 拿 fingerprint）**：rustls 拒握时 `quinn::Connecting::await` 直接
    /// 返 `Err(ConnectionError::TransportError(rustls::Error::General))`，
    /// 此时还没 resolve 出 `Connection`，**没有 `peer_identity()` 可读** —
    /// fingerprint 信息只在 `verify_client_cert` 调用现场被丢弃。只能在
    /// verifier 内部 `verify_client_cert` 即将返 Err 时把 fp clone 一份
    /// 发出来。
    Rejected {
        fingerprint: String,
    },
}

pub(crate) struct LanMouseListener {
    listen_rx: Receiver<ListenEvent>,
    listen_tx: Sender<ListenEvent>,
    /// QUIC accept task（M1 阶段单 endpoint 绑 `0.0.0.0:port`）。
    /// `terminate` 时 abort。
    accept_task: JoinHandle<()>,
    /// **STEP-8.2 修复**：把 `AuthorizedKeysVerifier` 的反向通知 channel
    /// (`tokio::sync::mpsc::UnboundedReceiver<String>`) 转译为
    /// `ListenEvent::Rejected` 的 forwarder task。
    ///
    /// `spawn_local` 起的 task，阻塞 recv `rejection_rx` → 收到 fp 后
    /// `listen_tx.send(ListenEvent::Rejected { fingerprint })`。**复用同
    /// 一 `listen_tx`** —— 不另开 channel，让 `emulation.rs::ListenTask`
    /// 已在的 match 臂（emulation.rs:190）天然生效。
    ///
    /// `terminate` 时 abort —— 与 `wake_task` 同模式。
    rejection_forwarder_task: JoinHandle<()>,
    /// 后台 wake 处理 task（STEP-6.3 引入，与 bak 对齐）。
    ///
    /// macOS 系统唤醒 → 强制关闭所有 QUIC peer conn（不等 QUIC 30s
    /// `max_idle_timeout`），触发 supervisor 的 read_loop EOF →
    /// `ListenEvent::Disconnected` → ListenTask 同步清理 proxy + 上报
    /// service → client 端 next `send()` 触发 `dial_any` 重连
    /// （STEP-6.4 接入）。
    ///
    /// 非 macOS 上 `wake_rx = None`，本 task 在 select 里永久 pending。
    wake_task: JoinHandle<()>,
    /// 已通过 mTLS + authorized_keys 的合法 QUIC peer 表（与 bak 对齐）。
    ///
    /// supervisor 在 Accept event 后 `insert(addr, peer.clone())`，
    /// supervisor 退出时 `remove(addr)`（drop `QuicConnGuard`）。
    ///
    /// **核心消费者是 macOS wake 路径**：`spawn_wake_task` 遍历本表，对每条
    /// conn 调 `peer.connection().close(0u32.into(), b"wake")` 同步触发
    /// close —— 不等 QUIC `max_idle_timeout` (30s)。
    ///
    /// `reply()` 也读本表查 peer 后写 control 帧到 stream A。
    ///
    /// 选 `Rc<RefCell<HashMap<...>>>` 而非 `Rc<AsyncMutex<...>>`：
    /// 注册 / 反注册 / 查表都是同步路径；`peer.send_input` 异步路径单用一次锁。
    quic_conns: Rc<RefCell<HashMap<SocketAddr, Rc<PeerSession>>>>,
    /// macOS-only: held for its `Drop` side effect (stops the
    /// CFRunLoop in the power-observer thread). The observer sends
    /// `()` into the wake channel on system-wake; the wake task
    /// drains that channel and force-closes peer conns so
    /// reconnect happens immediately after a screensaver/sleep
    /// dismissal. QUIC keepalive has taken over idle detection —
    /// see STEP-7.1.
    #[cfg(target_os = "macos")]
    power_observer: crate::macos_power::PowerObserver,
}

impl LanMouseListener {
    pub(crate) async fn new(
        port: u16,
        cert_chain: Vec<CertificateDer<'static>>,
        key: rustls::pki_types::PrivateKeyDer<'static>,
        authorized_keys: Arc<RwLock<HashMap<String, String>>>,
    ) -> Result<Self, ListenerCreationError> {
        let (listen_tx, listen_rx) = channel();

        // macOS wake → force-close-all-QUIC-peers plumbing (STEP-6.3 引入)。
        // 非 macOS 上 PowerObserver 不 spawn，wake_rx 是 None；
        // spawn_wake_task 在 wake_rx = None 分支里永久 pending。
        #[cfg(target_os = "macos")]
        let (power_observer, wake_rx) = {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<()>();
            let observer = crate::macos_power::PowerObserver::spawn(tx).await;
            (observer, Some(rx))
        };
        #[cfg(not(target_os = "macos"))]
        let wake_rx: Option<tokio::sync::mpsc::UnboundedReceiver<()>> = None;

        // QUIC peer 注册表（空初始化）。
        // `spawn_wake_task` 拿 clone 走 wake 路径，
        // `spawn_quic_accept_task` 拿 clone 走 accept + supervisor 注册路径。
        let quic_conns: Rc<RefCell<HashMap<SocketAddr, Rc<PeerSession>>>> =
            Rc::new(RefCell::new(HashMap::new()));

        let wake_task = spawn_wake_task(wake_rx, quic_conns.clone());

        // STEP-8.2 修复：装配 rejection 反向通知 channel。
        //
        // 路径：`AuthorizedKeysVerifier::verify_client_cert` Err → `tx.send(fp)`
        // → 本 forwarder task `rx.recv()` → `listen_tx.send(ListenEvent::Rejected)`
        // → emulation.rs:190 → `EmulationEvent::ConnectionAttempt` → service.rs:320
        // → `FrontendEvent::ConnectionAttempt` → 前端 `request_authorization`。
        //
        // **channel 类型**：`tokio::sync::mpsc::unbounded_channel`（与
        // §1 `wake_tx` 同模式 —— verifier 在 rustls 握手回调里 send，
        // 可能在非 local 线程上，需 Send/Sync sender；forwarder 在
        // spawn_local 上 recv）。
        let (rejection_tx, rejection_rx) =
            tokio::sync::mpsc::unbounded_channel::<String>();
        let rejection_forwarder_task = spawn_rejection_forwarder_task(rejection_rx, listen_tx.clone());

        let verifier: Arc<dyn rustls::server::danger::ClientCertVerifier> =
            Arc::new(AuthorizedKeysVerifier::new(authorized_keys).with_rejection_tx(rejection_tx));

        let addr = SocketAddr::new(
            "0.0.0.0".parse().expect("invalid ip"),
            port,
        );
        let endpoint = quic_transport::endpoint_with_verifier(addr, cert_chain, key, verifier)?;

        let accept_task = spawn_quic_accept_task(endpoint, listen_tx.clone(), quic_conns.clone());

        Ok(Self {
            listen_rx,
            listen_tx,
            accept_task,
            rejection_forwarder_task,
            wake_task,
            quic_conns,
            #[cfg(target_os = "macos")]
            power_observer,
        })
    }

    pub(crate) fn request_port_change(&mut self, _port: u16) {
        // STEP-6.3 接手：per-IP endpoint rebuild 路径
        log::warn!(
            "LanMouseListener::request_port_change is a no-op for QUIC; \
             runtime port rebind is not supported (STEP-6.2)"
        );
    }

    pub(crate) async fn port_changed(&mut self) -> Result<u16, ListenerCreationError> {
        Err(ListenerCreationError::PortChangeUnsupported)
    }

    pub(crate) async fn terminate(&mut self) {
        // STEP-6.3：terminate 改用新 task 结构清理。
        //
        // 1. abort wake task → PowerObserver Drop 关 CFRunLoop（macOS-only）。
        // 2. abort accept task → endpoint close → 所有 in-flight supervisor
        //    收到 conn close → 发 ListenEvent::Disconnected → emulation.rs
        //    ListenTask 清理 + 上报 service。
        // 3. abort rejection forwarder task（STEP-8.2）→ verifier 持有的
        //    rejection_tx sender 之后 send 会返 Err（被静默吞，与 verify_
        //    client_cert 设计一致）。
        // 4. close listen_tx → 通知所有 supervisor 的 forward_event 写入失败
        //    → 不影响 read_loop 退出（read_loop 自己的 join handle 仍 resolve）。
        self.wake_task.abort();
        self.accept_task.abort();
        self.rejection_forwarder_task.abort();
        self.listen_tx.close();
    }

    /// QUIC 路径 reply：从 `quic_conns` 表查 peer，把 control 事件写到该 peer
    /// 的 stream A。
    ///
    /// M1 简化：peer 不在线时静默 no-op（避免 emulation.rs 报错）。
    /// 走 PeerSession::send_input 通道分派：当前 `InputChannelConfig::default()`
    /// 把控制面事件映射到 `Channel::StreamA`，所以 reply 自然走 stream A。
    pub(crate) async fn reply(&self, addr: SocketAddr, event: ProtoEvent) {
        let peer = self.quic_conns.borrow().get(&addr).cloned();
        match peer {
            Some(peer) => {
                use lan_mouse_ipc::InputChannelConfig;
                match peer.send_input(&event, &InputChannelConfig::default()).await {
                    Ok(()) => {
                        if matches!(event, ProtoEvent::Ack(_) | ProtoEvent::Leave(_)) {
                            log::info!("reply: {event} to {addr} delivered");
                        }
                    }
                    Err(e) => log::warn!("reply QUIC send to {addr} failed: {e}"),
                }
            }
            None => log::warn!(
                "reply: peer {addr} not in quic_conns; dropping {event}"
            ),
        }
    }

    /// 给 ListenTask 在 Enter 事件时查 client cert fingerprint。
    ///
    /// **来源**：QUIC 路径不缓存 peer cert 单独存 —— 直接从 `Connection::peer_identity`
    /// 拿（与 supervisor 内计算 fingerprint 是同一份数据）。但 supervisor
    /// 已经把 `fingerprint: String` 放在 `ListenEvent::Accept` 里给 ListenTask
    /// 上报了 `EmulationEvent::Connected { addr, fingerprint }`，ListenTask
    /// 进 `addr_to_fingerprint` map。**所以 ListenTask 在 Enter 时不需要重
    /// 算 fingerprint** —— 直接查 map 即可。本函数保留是 emulation.rs 的
    /// 现有 API 调用站桩，**M1 阶段不真正用**（与 bak 对齐：bak 也保留这
    /// 个 no-op stub）。
    #[allow(dead_code)]
    pub(crate) async fn get_certificate_fingerprint(
        &self,
        addr: SocketAddr,
    ) -> Option<String> {
        // M1 占位：直接返 None。ListenTask 当前路径是从 Accept event 拿
        // fingerprint；本函数仅是 emulation.rs:152 调用站桩。
        let _ = addr;
        None
    }

    /// **补丁 — last_response 超时时触发对端重拨**：强制用
    /// [`crate::quic_transport::session::WAKE_CLOSE_CODE`] 关掉指定 addr
    /// 的 QUIC conn，让对端 [`crate::quic_transport::should_retry_after_close`]
    /// 看到 wake code 走 RetryState 重试路径。
    ///
    /// **使用方**：[`crate::emulation::ListenTask`] 在 5s tick 检测到
    /// `last_response[addr].elapsed() > 1s` 时调 —— 替代原"仅发
    /// `EmulationEvent::Disconnected` 不动 conn"的逻辑，那条路径下主控端
    /// QUIC conn 仍活着、supervisor 收不到 close reason、**没人重拨**。
    ///
    /// **与 wake 路径协同**：复用同一 `WAKE_CLOSE_CODE` 语义 —— 对端
    /// 不区分"系统唤醒 close"与"应用层超时 close"，统一走重试分支。
    ///
    /// **与 supervisor 路径 race**：本调用 force-close 后 slave 的
    /// `handle_quic_peer_supervisor` 也会看到 stream A EOF → 推
    /// `ListenEvent::Disconnected`，可能跟 timeout 分支自己推的
    /// `EmulationEvent::Disconnected` 重叠；service.rs
    /// [`crate::service::Service::remove_incoming`] 通过
    /// `if let Some(addr) = self.remove_incoming(addr)` 守卫住了重复
    /// remove（第二次返回 None，无副作用）。
    pub(crate) fn close_with_wake_code(&self, addr: SocketAddr) {
        let peer = self.quic_conns.borrow().get(&addr).cloned();
        match peer {
            Some(peer) => {
                log::debug!("close_with_wake_code: peer {addr} → WAKE_CLOSE_CODE (timeout path)");
                peer.connection().close(
                    crate::quic_transport::session::WAKE_CLOSE_CODE.into(),
                    b"timeout",
                );
            }
            None => {
                // peer 不在 quic_conns —— 可能已被 supervisor 路径先摘掉
                // (race: supervisor EOF 与 timeout tick 并发)。静默 no-op。
                log::trace!("close_with_wake_code: peer {addr} not in quic_conns (already gone)");
            }
        }
    }
}

impl Stream for LanMouseListener {
    type Item = ListenEvent;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.listen_rx.poll_next_unpin(cx)
    }
}

/// 给 `LanMouseListener::new()` 调的 helper：起单 endpoint QUIC accept task +
/// 每条连接的 per-peer supervisor task。
///
/// **Step 6.2 + 6.3 supervisor 形态**：
/// 1. `quic_transport::accept(&ep)` 循环拿 `Connection`
/// 2. 接受到 conn → 立即 spawn supervisor：
///    - `server_hello` 交换 PROTOCOL_MAGIC（`HelloFailed` 错误 → 退 supervisor）
///    - 计算 client cert fingerprint（peer_identity + `crypto::generate_fingerprint`）
///    - 双层防御：mTLS 已在 handshake 时校验过；supervisor 再查 allowlist
///      作为 fallback（理论上 verifier 已放行的 fp 必在 allowlist）
///    - 推 `ListenEvent::Accept { addr, fingerprint }`
///    - 注册 `quic_conns[addr] = peer.clone()` + Drop guard 反注册
///    - `take_stream_a_recv` 拿 stream A recv 半边
///    - 循环 `read_frame(&mut recv_a)` 把每帧转译为 `ListenEvent::Msg`
///    - stream A EOF / conn close → 退 `quic_conns` + 推 `ListenEvent::Disconnected`
fn spawn_quic_accept_task(
    ep: quinn::Endpoint,
    listen_tx: Sender<ListenEvent>,
    quic_conns: Rc<RefCell<HashMap<SocketAddr, Rc<PeerSession>>>>,
) -> JoinHandle<()> {
    spawn_local(async move {
        log::info!("QUIC listener listening on {ep:?}");
        loop {
            match quic_transport::accept(&ep).await {
                Ok(conn) => {
                    let peer = Rc::new(PeerSession::from_connection(conn));
                    let remote = peer.connection().remote_address();
                    log::info!("QUIC peer connected: {remote}");
                    let peer_clone = peer.clone();
                    let tx_clone = listen_tx.clone();
                    let quic_conns_for_supervisor = quic_conns.clone();
                    spawn_local(async move {
                        if let Err(e) = handle_quic_peer_supervisor(
                            peer_clone,
                            tx_clone,
                            quic_conns_for_supervisor,
                        )
                        .await
                        {
                            log::warn!("QUIC peer supervisor exited with err: {e}");
                        }
                    });
                }
                Err(e) => {
                    // QUIC accept 失败通常是 endpoint 被 close（terminate）
                    log::debug!("QUIC accept 返回：{e}");
                    break;
                }
            }
        }
    })
}

/// 后台 wake 处理 task（STEP-6.3 引入，与 bak `spawn_wake_task` 对齐）。
///
/// macOS 系统唤醒信号来时，遍历 `quic_conns` 注册表，对每条 conn 同步调
/// `peer.connection().close(0, b"wake")` —— 不等 30s `max_idle_timeout`，
/// 让 `streams.join` 立即 resolve → supervisor 发 `ListenEvent::Disconnected`
/// → ListenTask 同步清理 `emulation_proxy[addr]` + 上报 service。
///
/// **`RefCell::borrow()` 同步路径**（无 await 竞争）：
/// `quinn::Connection::close(VarInt, &[u8])` 同步，不会与 read_loop
/// 的 borrow_mut 冲突。
///
/// **不需要** clone peer —— `Rc<PeerSession>` 持有内部 `Rc<Connection>`，
/// close 直接走底层 ref count。
///
/// **error_code 0 = NO_ERROR**（graceful）；客户端 `should_retry_after_close`
/// 分类为"不重试"，但 `peers` 已被 `spawn_peer_supervisor` 摘掉（STEP-6.1），
/// 下次 `send()` 走 `should_attempt` 自然触发重拨 —— 与 DTLS wake 语义对齐。
///
/// 非 macOS 上 `wake_rx = None`，`match wake_rx.as_mut() { None => pending() }`
/// 永久挂起（不浪费 wake）。
fn spawn_wake_task(
    mut wake_rx: Option<tokio::sync::mpsc::UnboundedReceiver<()>>,
    quic_conns: Rc<RefCell<HashMap<SocketAddr, Rc<PeerSession>>>>,
) -> JoinHandle<()> {
    spawn_local(async move {
        loop {
            let wake = match wake_rx.as_mut() {
                Some(rx) => rx.recv().await,
                None => std::future::pending().await,
            };
            match wake {
                Some(()) => {
                    let q = quic_conns.borrow();
                    log::info!(
                        "mac wake: force-closing {} QUIC peer conn(s) with WAKE_CLOSE_CODE (0xCAFE) — \
                         peer supervisor will see ApplicationClosed and trigger reconnect",
                        q.len()
                    );
                    for (a, peer) in q.iter() {
                        log::debug!("post-wake close (QUIC): {a}");
                        // **补丁 — Mac wake 自动重连**：用
                        // [`crate::quic_transport::WAKE_CLOSE_CODE`] (0xCAFE)
                        // 替代默认的 0（NO_ERROR）—— 对端
                        // [`should_retry_after_close`] 看到 wake code 触发重试，
                        // 不再卡在 "graceful close → 等下次 send()" 路径
                        // （wake 后没人在动鼠标，send 不会自然来）。
                        //
                        // 与 Bug #9/#10 路径（`close(0u32, "peer closed stream")`）
                        // **不冲突**：Bug #9/#10 用 code 0 走用户/网络层 close 分支
                        // （不重试）；本 wake 路径用 0xCAFE 走 wake 分支（重试）。
                        peer.connection().close(
                            crate::quic_transport::session::WAKE_CLOSE_CODE.into(),
                            b"wake",
                        );
                    }
                }
                None => {
                    log::debug!(
                        "supervisor: wake channel closed; \
                         power observer no longer signaling"
                    );
                    wake_rx = None;
                }
            }
        }
    })
}

/// **STEP-8.2 修复**：把 `AuthorizedKeysVerifier` 的反向通知 channel
/// (`tokio::sync::mpsc::UnboundedReceiver<String>`) 转译为
/// `ListenEvent::Rejected` 的 forwarder task。
///
/// **路径**：`AuthorizedKeysVerifier::verify_client_cert` Err →
/// `tx.send(fp)`（在 verifier 内部，已在 quic_transport.rs 装配）→
/// 本 task `rx.recv()` → `listen_tx.send(ListenEvent::Rejected { fingerprint })`
/// → emulation.rs:190 → `EmulationEvent::ConnectionAttempt` →
/// service.rs:320 → `FrontendEvent::ConnectionAttempt` → 前端 `request_authorization`
/// 弹窗。
///
/// **去重**：emulation.rs:191-194 已有 2 秒去重（同一 fp 在 2 秒内只弹一
/// 次窗），避免对端 retry 时被 rustls 反复拒握导致弹窗刷屏 —— forwarder
/// 这一层不需要再 dedup，**直接转译**。
///
/// **退出路径**：`terminate()` 调 `rejection_forwarder_task.abort()` —
/// 与 `wake_task` / `accept_task` 同模式；abort 后 verifier 持有的
/// `rejection_tx` 后续 send 会返 Err（已在 verifier 内部被静默吞）。
fn spawn_rejection_forwarder_task(
    mut rejection_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    listen_tx: Sender<ListenEvent>,
) -> JoinHandle<()> {
    spawn_local(async move {
        while let Some(fp) = rejection_rx.recv().await {
            log::debug!("rejection forwarder: peer {fp} rejected by mTLS — sending ListenEvent::Rejected");
            if listen_tx
                .send(ListenEvent::Rejected { fingerprint: fp })
                .is_err()
            {
                log::debug!(
                    "rejection forwarder: listen_tx send failed (channel closed, terminating)"
                );
                break;
            }
        }
        log::debug!("rejection forwarder: rejection channel closed — exiting");
    })
}

/// 单连接的 supervisor handler。
///
/// 流程：
/// 1. `server_hello` 交换 PROTOCOL_MAGIC
/// 2. 计算 client cert fingerprint
/// 3. 发 `ListenEvent::Accept { addr, fingerprint }`
/// 4. 注册到 `quic_conns`（让 `reply()` 查 peer）+ 装 `QuicConnGuard`
/// 5. `take_stream_a_recv` 拿 stream A recv 半边
/// 6. 循环 `read_frame(&mut recv_a)` 把每帧转译为 `ListenEvent::Msg`
/// 7. stream A EOF / 致命错误 → `QuicConnGuard` Drop 自动反注册 +
///    推 `ListenEvent::Disconnected`
///
/// **为什么不调 `route_input` 做反向分派**：listen.rs supervisor 不感知
/// 发送端 cfg（receiver 端不感知 PLAN §3.1.4），把 stream → event 的物理
/// 路径转译给 ListenTask 处理；ListenTask 的现有 `match event` 已覆盖
/// 所有控制面 / input 事件。
///
/// **Stream B / C 装配留 STEP-7.x**：本步 client 端 `LanMouseConnection::send`
/// 不主动开 B/C；server 端 supervisor 只听 stream A 就够覆盖 M1 现有控制面
/// 事件流（Enter / Leave / Ack / Hello / Ping / Pong）。
#[allow(clippy::doc_lazy_continuation)]
async fn handle_quic_peer_supervisor(
    peer: Rc<PeerSession>,
    listen_tx: Sender<ListenEvent>,
    quic_conns: Rc<RefCell<HashMap<SocketAddr, Rc<PeerSession>>>>,
) -> Result<(), quic_transport::Error> {
    let addr = peer.connection().remote_address();

    // (1) server_hello
    quic_transport::server_hello(&peer).await?;

    // (2) 计算 client cert fingerprint（mTLS 已在 handshake 通过；本步只算 fp）
    //
    // quinn 0.11 `Connection::peer_identity() -> Option<Box<dyn Any>>`
    // 把 rustls 端的 `Vec<CertificateDer<'static>>` 装进 trait object —— 需
    // downcast 后取首张 cert 算 fingerprint。
    let identity = peer.connection().peer_identity();
    let certs: Option<&Vec<rustls::pki_types::CertificateDer<'static>>> = identity
        .as_ref()
        .and_then(|c| c.downcast_ref::<Vec<rustls::pki_types::CertificateDer<'static>>>());
    let fingerprint = certs
        .and_then(|c| c.first())
        .map(|cert| crypto::generate_fingerprint(cert.as_ref()))
        .ok_or_else(|| {
            quic_transport::Error::HelloFailed("no client cert presented".into())
        })?;

    // (3) 发 Accept 事件
    log::info!("QUIC peer {addr} authorized (fingerprint {fingerprint})");
    listen_tx
        .send(ListenEvent::Accept {
            addr,
            fingerprint: fingerprint.clone(),
        })
        .map_err(|_| {
            quic_transport::Error::HelloFailed("listen_tx closed (terminated)".into())
        })?;

    // (4) 注册到 QUIC peer 表（让 `reply()` 能查到）+ 装 QuicConnGuard
    //     让任何退出路径（Ok / Err / panic / wake close）都自动反注册
    //
    // QuicConnGuard Drop 在函数末尾自动触发（任何 return 路径）——
    // 与 bak `mousehop/src/listen.rs:382-386` 同模式。
    quic_conns
        .borrow_mut()
        .insert(addr, peer.clone())
        .inspect(|_old| {
            log::warn!(
                "QUIC peer {addr} already registered in quic_conns — overwriting (old peer may leak)"
            );
        });
    let _guard = QuicConnGuard {
        table: quic_conns.clone(),
        addr,
    };
    log::debug!("QUIC peer {addr} registered in quic_conns (guard active)");

    // (5) take stream A recv 半边（控制帧 reader 用）
    let mut recv_a = peer
        .take_stream_a_recv()
        .await
        .ok_or_else(|| {
            quic_transport::Error::HelloFailed("stream A not cached after server_hello".into())
        })?;

    // **STEP-8.2 修复 — Bug #8**：spawn datagram reader task。
    //
    // **背景**：`route_input` 把 Motion/Axis/AxisDiscrete120 路由到 QUIC
    // datagram 通道（不是 stream A）—— 高频指针事件为避免 stream 重传
    // 延迟走 datagram。但 server `listen.rs::handle_quic_peer_supervisor`
    // 修前**只读 cached recv_a**（来自 server_hello 的 stream A）——
    // **完全不读 datagram**。`datagram_reader_task` 只在 client 端
    // `peer.run()` 里 spawn（与 server supervisor 是两条独立路径）。
    //
    // **后果**：本机 (client) 把 motion 走 datagram 发出去 → 远程
    // (server) 端 QUIC 收到但**没人读** → 远程 capture 看不到 motion
    // → 鼠标不动。Bug #7 修后本机能正确切换到 Sending 状态开始发
    // motion，但 server 端**永远收不到**。
    //
    // **修法**：spawn 独立的 datagram reader task，循环 read_datagram
    // → ProtoEvent::try_from → 推 listen_tx（与 stream A 读循环对称）。
    //
    // **生命周期**：task 在 supervisor 退出时随 peer drop 自动结束（peer
    // 是 Rc，task 持 Arc 引用，peer drop 时 task 持有的 Arc 引用计数
    // 归零 → task 的 peer 参数析构 → read_datagram 返 Err → task 退）。
    spawn_local(server_datagram_reader_task(
        peer.clone(),
        listen_tx.clone(),
        addr,
    ));

    // (6) 循环 read_frame(recv_a) → ListenEvent::Msg
    //
    // 错误分流（与 bak `read_stream_a_loop` 对齐）：
    // - `FrameTooLarge` → fatal，返 Err
    // - `HelloFailed("decode frame...")` → warn + skip frame 续读
    // - `Truncated` / EOF → 退出循环 + 推 Disconnected
    loop {
        match quic_transport::read_frame(&mut recv_a).await {
            Ok(event) => {
                // **生产日志级别（DEBUG）**：高频路径（每个 control 事件
                // 都过这里）。INFO 仍会刷屏。Step 8.2 调试期间是 INFO，
                // 上线前调回 DEBUG —— 配合 RUST_LOG=lan_mouse::listen=debug
                // 仍能精确诊断"控制事件有没有从 client 到 server"。
                log::debug!("stream A recv from {addr}: {event}");
                if listen_tx
                    .send(ListenEvent::Msg { event, addr })
                    .is_err()
                {
                    log::debug!(
                        "QUIC supervisor: listen_tx send failed (channel closed, terminating)"
                    );
                    break;
                }
            }
            Err(quic_transport::Error::FrameTooLarge(len)) => {
                log::error!("stream A: FrameTooLarge({len}) — fatal, closing task");
                return Err(quic_transport::Error::FrameTooLarge(len));
            }
            Err(quic_transport::Error::HelloFailed(msg))
                if msg.starts_with("decode frame") =>
            {
                log::warn!("stream A: skip frame (decode error): {msg}");
                continue;
            }
            Err(quic_transport::Error::Truncated) => {
                log::info!("stream A truncated — peer closed");
                break;
            }
            Err(e) => {
                log::info!("stream A reader exiting (IO closed): {e}");
                return Err(e);
            }
        }
    }

    // (7) stream A EOF / conn close → 推 Disconnected（QuicConnGuard Drop 自动反注册）
    log::info!("QUIC peer {addr} stream A closed — sending Disconnected");
    let _ = listen_tx.send(ListenEvent::Disconnected { addr });
    Ok(())
}

/// QUIC peer 表注册 RAII guard（STEP-6.3 引入，与 bak `QuicConnGuard` 对齐）。
///
/// 构造时绑定 `(table, addr)`，Drop 时从 `table` 移除 `addr`。
/// 让 `handle_quic_peer_supervisor` 的所有退出路径（Ok / Err / panic）都能
/// 自动反注册 —— 无需在每个 `?` 早返前手动 `remove()`。
///
/// **设计动机**："peer 注册到 `quic_conns`"必须严格配对"反注册"，否则：
/// - 同一 addr 重连时 `insert()` 会覆盖旧 entry（已有 `warn!` 兜底但旧
///   peer Rc 仍 hold 旧 connection，可能延迟关闭）
/// - wake 路径遍历到僵尸 entry → close 已被回收的 conn（quinn 内部状态，
///   no-op 但日志噪音）
///
/// **不动 conn 本身**：Drop 只删 HashMap entry，不调 `conn.close()`；
/// 关闭 conn 由 read_loop 退出 / wake 路径（`peer.connection().close(...)`）
/// 触发。
struct QuicConnGuard {
    table: Rc<RefCell<HashMap<SocketAddr, Rc<PeerSession>>>>,
    addr: SocketAddr,
}

impl Drop for QuicConnGuard {
    fn drop(&mut self) {
        let removed = self.table.borrow_mut().remove(&self.addr);
        if removed.is_some() {
            log::debug!(
                "QUIC peer {} deregistered from quic_conns (guard drop)",
                self.addr
            );
        }
        // removed == None：peer 从未被注册（Accept event 之前早退），
        // 或已被 wake 路径覆盖（不可能，单线程）；静默 no-op。
    }
}

/// **STEP-8.2 修复 — Bug #8**：server 侧 datagram reader task。
///
/// 与 `quic_transport::datagram_reader_task` 同源但走 server listen
/// 路径 —— 不走 `StreamEvent::Datagram`（peer.run() 内部抽象），直接
/// 包装成 `ListenEvent::Msg { event, addr }` 推 `listen_tx`，让
/// emulation.rs ListenTask 收到（与 stream A 路径对称）。
///
/// **为什么 server 端要单独写一个**：client 端 `peer.run()` 内部 spawn
/// 的 datagram_reader_task 用 `StreamEvent::Datagram`（peer.run 主循环
/// 消费的 enum 变体）—— 但 server `listen.rs::handle_quic_peer_
/// supervisor` 不调 peer.run，有自己的 read_frame 循环。直接复用
/// client 版会让 server 路径多一层 StreamEvent → ListenEvent 转换
/// 反而绕弯。inlined 这个 server 特化版更直接。
///
/// **生命周期**：task 持 `peer: Rc<PeerSession>` + `listen_tx:
/// Sender<ListenEvent>` —— supervisor 退出时 `listen_tx` 被 close，
/// 下一次 `send` 失败 → task 退出。peer Rc 在 supervisor 退出时也
/// drop，task 持有的 Arc 引用计数归零。
async fn server_datagram_reader_task(
    peer: Rc<PeerSession>,
    listen_tx: Sender<ListenEvent>,
    addr: SocketAddr,
) {
    loop {
        match peer.connection().read_datagram().await {
            Ok(bytes) => {
                // 定长 ProtoEvent codec：bytes.len() 必须 == MAX_EVENT_SIZE
                let buf: [u8; lan_mouse_proto::MAX_EVENT_SIZE] =
                    match bytes.as_ref().try_into() {
                        Ok(b) => b,
                        Err(_) => {
                            log::warn!(
                                "server datagram_reader: datagram 长度非 MAX_EVENT_SIZE({})，skip frame",
                                lan_mouse_proto::MAX_EVENT_SIZE
                            );
                            continue;
                        }
                    };
                let event = match lan_mouse_proto::ProtoEvent::try_from(buf) {
                    Ok(e) => e,
                    Err(e) => {
                        log::warn!(
                            "server datagram_reader: ProtoEvent 解码失败，skip frame: {e}"
                        );
                        continue;
                    }
                };
                log::trace!(
                    "server datagram_reader: from {addr}: {event}"
                );
                if listen_tx
                    .send(ListenEvent::Msg { event, addr })
                    .is_err()
                {
                    log::debug!(
                        "server datagram_reader: listen_tx closed, exiting"
                    );
                    return;
                }
            }
            Err(e) => {
                log::info!(
                    "server datagram_reader: read_datagram error, exiting: {e}"
                );
                return;
            }
        }
    }
}