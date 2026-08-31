//! QUIC server listener —— **M1 阶段 STEP-6.2 切到 PeerSession**。
//!
//! 替换 STEP-1.2 之前 `webrtc-dtls` DTLS 路径：原 `listen.rs::read_loop`
//! 走 `webrtc_util::Conn` + `DTLSConn` + `as_any().downcast_ref::<DTLSConn>()`
//! 旧路径（10 errors）整段删除，由 quic_transport 模块的 `PeerSession` +
//! `read_frame` + `read_any_frame` 替代。
//!
//! **M1 简化 supervisor 形态**（与 bak `mousehop/src/listen.rs::handle_quic_peer_supervisor`
//! 1:1 对齐的核心流程 + 简化 stream B/C 处理）：
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
//! **为什么不装配三 stream（B/C）**：M1 阶段 client 端 `LanMouseConnection::send`
//! **只**在 stream A 上发控制事件（Enter / Leave / Ack / Hello / Ping / Pong
//! 走 `send_stream_a`，按键走 `send_stream_b` 但 LanMouseConnection 当前
//! 仅在 `send_input` 的 `Channel::StreamB` 分派触发），且 client 端**没有**
//! 自动开 3 条 bidi 的装配路径（`connect_to_handle` 只跑 `client_hello`，
//! stream B/C 留给 send 时按需开）。因此 server 端 supervisor 只监听
//! stream A —— listen.rs ListenTask 的现有 match 臂覆盖所有控制面事件，
//! 不依赖 stream B/C reader。
//!
//! **Stream B / C 路径留 STEP-6.3 接手**：届时 supervisor 装配 outer
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
    /// STEP-6.2 暂不触发（mTLS 在 quinn handshake 阶段已拒 fingerprint 不
    /// 在 allowlist 的 client；mTLS 通过后 server_hello 失败由 supervisor
    /// 退场 → 不发任何 ListenEvent）。保留变体是为 STEP-6.3 supervisor 加
    /// 二次校验或 hello 失败时仍能复用现有 match 臂。
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
    /// 已通过 mTLS + authorized_keys 的合法 QUIC peer 表（与 bak 对齐）。
    ///
    /// supervisor 在 Accept event 后 `insert(addr, peer.clone())`，
    /// supervisor 退出时 `remove(addr)`。
    ///
    /// **核心用途**：让 `reply()` 查 peer 写 control 帧到 stream A。
    ///
    /// 选 `Rc<RefCell<HashMap<...>>>` 而非 `Rc<AsyncMutex<...>>`：
    /// 注册 / 反注册 / 查表都是同步路径；`peer.send_input` 异步路径单用一次锁。
    quic_conns: Rc<RefCell<HashMap<SocketAddr, Rc<PeerSession>>>>,
}

impl LanMouseListener {
    pub(crate) async fn new(
        port: u16,
        cert_chain: Vec<CertificateDer<'static>>,
        key: rustls::pki_types::PrivateKeyDer<'static>,
        authorized_keys: Arc<RwLock<HashMap<String, String>>>,
    ) -> Result<Self, ListenerCreationError> {
        let (listen_tx, listen_rx) = channel();

        let quic_conns: Rc<RefCell<HashMap<SocketAddr, Rc<PeerSession>>>> =
            Rc::new(RefCell::new(HashMap::new()));

        let verifier: Arc<dyn rustls::server::danger::ClientCertVerifier> =
            Arc::new(AuthorizedKeysVerifier::new(authorized_keys));

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
            quic_conns,
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
        // abort accept task → endpoint close → 所有 in-flight supervisor
        // 收到 conn close → 发 ListenEvent::Disconnected → emulation.rs
        // ListenTask 清理 + 上报 service
        self.accept_task.abort();
        self.listen_tx.close();
    }

    /// QUIC 路径 reply：从 `quic_conns` 表查 peer，把 control 事件写到该 peer
    /// 的 stream A。
    ///
    /// M1 简化：peer 不在线时静默 no-op（避免 emulation.rs 报错）。
    /// 走 PeerSession::send_input 通道分派：当前 `InputChannelConfig::default()`
    /// 把控制面事件映射到 `Channel::StreamA`，所以 reply 自然走 stream A。
    pub(crate) async fn reply(&self, addr: SocketAddr, event: ProtoEvent) {
        log::trace!("reply {event} >=>=>=>=>=> {addr}");
        let peer = self.quic_conns.borrow().get(&addr).cloned();
        if let Some(peer) = peer {
            // reply 走 default cfg：control 类事件自动分派到 stream A
            use lan_mouse_ipc::InputChannelConfig;
            if let Err(e) = peer
                .send_input(&event, &InputChannelConfig::default())
                .await
            {
                log::debug!("reply QUIC send to {addr} failed: {e}");
            }
        } else {
            log::debug!("reply: peer {addr} not in quic_conns; dropping {event}");
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
}

impl Stream for LanMouseListener {
    type Item = ListenEvent;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.listen_rx.poll_next_unpin(cx)
    }
}

/// 给 `LanMouseListener::new()` 调的 helper：起单 endpoint QUIC accept task +
/// 每条连接的 per-peer supervisor task。
///
/// **Step 6.2 supervisor 形态**：
/// 1. `quic_transport::accept(&ep)` 循环拿 `Connection`
/// 2. 接受到 conn → 立即 spawn supervisor：
///    - `server_hello` 交换 PROTOCOL_MAGIC（`HelloFailed` 错误 → 退 supervisor）
///    - 计算 client cert fingerprint（peer_identity + `crypto::generate_fingerprint`）
///    - 双层防御：mTLS 已在 handshake 时校验过；supervisor 再查 allowlist
///      作为 fallback（理论上 verifier 已放行的 fp 必在 allowlist）
///    - 推 `ListenEvent::Accept { addr, fingerprint }`
///    - 注册 `quic_conns[addr] = peer.clone()`
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

/// 单连接的 supervisor handler。
///
/// 流程：
/// 1. `server_hello` 交换 PROTOCOL_MAGIC
/// 2. 计算 client cert fingerprint
/// 3. 发 `ListenEvent::Accept { addr, fingerprint }`
/// 4. 注册到 `quic_conns`（让 `reply()` 查 peer）
/// 5. `take_stream_a_recv` 拿 stream A recv 半边
/// 6. 循环 `read_frame(&mut recv_a)` 把每帧转译为 `ListenEvent::Msg`
/// 7. stream A EOF / 致命错误 → 退 `quic_conns` + 推 `ListenEvent::Disconnected`
///
/// **为什么不调 `route_input` 做反向分派**：listen.rs supervisor 不感知
/// 发送端 cfg（receiver 端不感知 PLAN §3.1.4），把 stream → event 的物理
/// 路径转译给 ListenTask 处理；ListenTask 的现有 `match event` 已覆盖
/// 所有控制面 / input 事件。
///
/// **Stream B / C 装配留 STEP-6.3**：本步 client 端 `LanMouseConnection::send`
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

    // (4) 注册到 QUIC peer 表（让 `reply()` 能查到）
    quic_conns
        .borrow_mut()
        .insert(addr, peer.clone())
        .inspect(|_old| {
            log::warn!(
                "QUIC peer {addr} already registered in quic_conns — overwriting (old peer may leak)"
            );
        });

    // (5) take stream A recv 半边（控制帧 reader 用）
    let mut recv_a = peer
        .take_stream_a_recv()
        .await
        .ok_or_else(|| {
            quic_transport::Error::HelloFailed("stream A not cached after server_hello".into())
        })?;

    // (6) 循环 read_frame(recv_a) → ListenEvent::Msg
    //
    // 错误分流（与 bak `read_stream_a_loop` 对齐）：
    // - `FrameTooLarge` → fatal，返 Err
    // - `HelloFailed("decode frame...")` → warn + skip frame 续读
    // - `Truncated` / EOF → 退出循环 + 推 Disconnected
    loop {
        match quic_transport::read_frame(&mut recv_a).await {
            Ok(event) => {
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

    // (7) stream A EOF / conn close → 推 Disconnected
    quic_conns.borrow_mut().remove(&addr);
    log::info!("QUIC peer {addr} stream A closed — sending Disconnected");
    let _ = listen_tx.send(ListenEvent::Disconnected { addr });
    Ok(())
}
