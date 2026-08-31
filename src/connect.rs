use std::{
    collections::{HashMap, HashSet},
    io,
    net::SocketAddr,
    path::PathBuf,
    rc::Rc,
};

use lan_mouse_ipc::{ClientHandle, DEFAULT_PORT};
use lan_mouse_proto::ProtoEvent;
use local_channel::mpsc::{Receiver, Sender, channel};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use thiserror::Error;
use tokio::{sync::Mutex, task::spawn_local};

use crate::client::ClientManager;
use crate::quic_transport::{self, Endpoint, PeerSession};

/// mTLS 出示给对端的 client cert + key（STEP-6.1 引入，与 bak
/// `mousehop/src/connect.rs::QuicDialerCreds` 完全对齐）。
///
/// 复用 `crypto::load_or_create_server_cert()` 落盘的同一份 cert/key —
/// lan-mouse 既是 client 又是 server（PLAN §5 mTLS 双端出示），单端复用
/// 比维护两份 DER 字节更省事。
///
/// `Rc` 包装让 [`LanMouseConnection`] 与本类型的所有 clone 共享同一份
/// 凭证（避免 `PrivateKeyDer::clone_key()` 每次重 parse DER 字节）。
pub(crate) struct QuicDialerCreds {
    pub cert_chain: Vec<CertificateDer<'static>>,
    pub key: PrivateKeyDer<'static>,
}

#[derive(Debug, Error)]
pub(crate) enum LanMouseConnectionError {
    #[error(transparent)]
    Bind(#[from] io::Error),
    /// QUIC 传输层错误（STEP-6.1 引入）—— `PeerSession::send_input()` /
    /// `dial()` 的失败经由本变体透传给上层。
    ///
    /// 删除的 `Dtls` / `Webrtc` 变体（已无 caller，留着只会持续警告）；
    /// 完整 DTLS 依赖清理待 STEP-7.3。
    #[error(transparent)]
    Quic(#[from] quic_transport::Error),
    #[error("not connected")]
    NotConnected,
    #[error("emulation is disabled on the target device")]
    TargetEmulationDisabled,
    #[error("Connection timed out")]
    Timeout,
}

/// 出站 QUIC 连接管理（STEP-6.1 全面切到 QUIC）—— 替换 STEP-1.2 之前的
/// DTLSConn 路径。
///
/// **架构**（与 bak `mousehop/src/connect.rs::MousehopConnection` 对齐）：
/// - `client_endpoint: Endpoint` —— 单 endpoint 多 peer 复用（quinn
///   `Endpoint: Clone`，内部 `Arc`）。生产路径绑 `0.0.0.0:0`（v4 任意
///   本地端口），由 service.rs::new 构造时一次性 bind
/// - `quic_creds: Rc<QuicDialerCreds>` —— mTLS 拨号凭证，per-connection
///   复用
/// - `peers: Rc<Mutex<HashMap<SocketAddr, Rc<PeerSession>>>>` —— QUIC 会
///   话表；`send()` 查表命中后调 `peer.send_input(&event, &cfg)` 按
///   [`crate::quic_transport::route_input`] 分派到 datagram / stream A /
///   stream B
/// - `connecting: Rc<Mutex<HashSet<ClientHandle>>>` —— 拨号 in-flight 去
///   重，避免重复 `connect_to_handle` 抢占
/// - `recv_tx: Sender<(ClientHandle, ProtoEvent)>` —— 占位；未来
///   STEP-6.2 listen.rs read_loop 接入时的对称 API
pub(crate) struct LanMouseConnection {
    quic_creds: Rc<QuicDialerCreds>,
    client_endpoint: Endpoint,
    client_manager: ClientManager,
    peers: Rc<Mutex<HashMap<SocketAddr, Rc<PeerSession>>>>,
    connecting: Rc<Mutex<HashSet<ClientHandle>>>,
    pins_dir: PathBuf,
    recv_rx: Receiver<(ClientHandle, ProtoEvent)>,
    recv_tx: Sender<(ClientHandle, ProtoEvent)>,
}

impl LanMouseConnection {
    pub(crate) fn new(
        client_endpoint: Endpoint,
        cert_chain: Vec<CertificateDer<'static>>,
        key: PrivateKeyDer<'static>,
        pins_dir: PathBuf,
        client_manager: ClientManager,
    ) -> Self {
        let (recv_tx, recv_rx) = channel();
        let quic_creds = Rc::new(QuicDialerCreds { cert_chain, key });
        Self {
            quic_creds,
            client_endpoint,
            client_manager,
            peers: Default::default(),
            connecting: Default::default(),
            pins_dir,
            recv_rx,
            recv_tx,
        }
    }

    pub(crate) async fn recv(&mut self) -> (ClientHandle, ProtoEvent) {
        self.recv_rx.recv().await.expect("channel closed")
    }

    /// 发送一个事件到对端（STEP-6.1 切到 QUIC 路径）。
    ///
    /// **3 步流程**（与 bak `MousehopConnection::send` 对齐）：
    /// 1. 查 `client_manager.active_addr(handle)` 拿 socket addr
    /// 2. 查 `peers` 表拿 QUIC 会话（命中 → 调
    ///    [`PeerSession::send_input`]；未命中 → 触发拨号）
    /// 3. alive 守护 + 错误归并（send_input 失败 → 摘 peer + 通知 manager）
    ///
    /// **alive 守护**：与 DTLS 路径对称 —— 对端把 emulation 关了（ponged
    /// 返 `false`），继续注入无意义，先返 `TargetEmulationDisabled`。
    ///
    /// **M1 简化**：所有 `send_input` 错误都视为 transport fatal —
    /// protocol-level 错误（M2 clipboard 才会有 `UnsupportedEvent`）
    /// 在 M1 阶段不存在。STEP-6.5 重连触发时一并引入 fatal/non-fatal
    /// 分类。
    pub(crate) async fn send(
        &self,
        event: ProtoEvent,
        handle: ClientHandle,
    ) -> Result<(), LanMouseConnectionError> {
        let event_display = format!("{event}");
        if let Some(addr) = self.client_manager.active_addr(handle) {
            let peer = {
                let peers = self.peers.lock().await;
                peers.get(&addr).cloned()
            };
            if let Some(peer) = peer {
                if !self.client_manager.alive(handle) {
                    return Err(LanMouseConnectionError::TargetEmulationDisabled);
                }
                let cfg = self
                    .client_manager
                    .input_channels(handle)
                    .unwrap_or_default();
                match peer.send_input(&event, &cfg).await {
                    Ok(()) => {
                        log::trace!("{event_display} >->->->->- {addr} (quic)");
                        return Ok(());
                    }
                    Err(e) => {
                        log::warn!("client {handle} failed to send over QUIC: {e}");
                        self.peers.lock().await.remove(&addr);
                        self.client_manager.set_active_addr(handle, None);
                        return Err(LanMouseConnectionError::Quic(e));
                    }
                }
            }
        }

        // 没有现成 QUIC session —— 看是否要触发拨号（spawn_local）。
        let mut connecting = self.connecting.lock().await;
        if !connecting.contains(&handle) {
            connecting.insert(handle);
            // 拨号后台跑；本步只接通"成功拨号 + register_peer + hello"
            // 路径，receive_loop 留给 STEP-6.2 listen.rs read_loop 接入。
            spawn_local(connect_to_handle(
                self.client_manager.clone(),
                self.client_endpoint.clone(),
                self.quic_creds.clone(),
                self.peers.clone(),
                self.connecting.clone(),
                self.pins_dir.clone(),
                handle,
            ));
        }
        Err(LanMouseConnectionError::NotConnected)
    }
}

/// 出站拨号主入口（STEP-6.1 / STEP-6.4 升级 happy-eyeballs）——
/// 给定一个 peer handle：
/// 1. 拿候选 IP 列表 + port
/// 2. 调 `quic_transport::dial_any(...)` 走 happy-eyeballs 多地址并发
///    + primary head-start（STEP-6.4 替换 STEP-6.1 的单地址 `dial`）
/// 3. 应用层 `client_hello` 握手
/// 4. 成功：`set_active_addr` + `register_peer(addr, peer)` + 摘 `connecting`
///
/// **自由函数 vs `&self` 方法的取舍**：`send()` 通过 `spawn_local` 异步跑
/// 本函数（spawn 要求 future `'static`，`&self` borrow 不能跨 spawn），所以
/// 显式把 `LanMouseConnection` 的所有字段 clone 出来作参数 —— 与 bak
/// `mousehop/src/connect.rs::connect_to_handle` 1:1 对齐。
///
/// **STEP-6.4 升级**：happy-eyeballs 多地址并发 + 200ms primary head-start
/// 替换 STEP-6.1 单地址 `dial`（PLAN §6.4）。`primary` 取自
/// `addrs.first()` —— mDNS / 候选列表中"最优 IP"由 caller 决定（当前用
/// `HashSet` 迭代顺序，无 mDNS 时即首选 IP）；剩余候选并发拨。
///
/// **M1 简化**：bak 有完整 retry gate / 退避 / 熔断，STEP-6.5 才补。本步
/// 只负责 "成功 → 注册 + 摘 connecting；失败 → 摘 connecting + 返 Err"。
#[allow(clippy::too_many_arguments)]
async fn connect_to_handle(
    client_manager: ClientManager,
    client_endpoint: Endpoint,
    quic_creds: Rc<QuicDialerCreds>,
    peers: Rc<Mutex<HashMap<SocketAddr, Rc<PeerSession>>>>,
    connecting: Rc<Mutex<HashSet<ClientHandle>>>,
    pins_dir: PathBuf,
    handle: ClientHandle,
) -> Result<(), LanMouseConnectionError> {
    log::info!("client {handle} connecting ...");
    let Some(ips_set) = client_manager.get_ips(handle) else {
        connecting.lock().await.remove(&handle);
        return Err(LanMouseConnectionError::NotConnected);
    };
    let port = client_manager.get_port(handle).unwrap_or(DEFAULT_PORT);
    // STEP-6.4 修 connect.rs:205 E0308：`ips_set.iter()` 返 `&IpAddr`，
    // `SocketAddr::new` 收 `IpAddr`（owned），`*a` 解引用。
    let addrs: Vec<SocketAddr> = ips_set.iter().map(|a| SocketAddr::new(*a, port)).collect();

    let Some(&primary) = addrs.first() else {
        connecting.lock().await.remove(&handle);
        return Err(LanMouseConnectionError::NotConnected);
    };
    log::info!(
        "client ({handle}) dial_any ... (primary: {primary}, candidates: {})",
        addrs.len()
    );

    let conn = match quic_transport::dial_any(
        &client_endpoint,
        primary,
        &addrs,
        quic_creds.cert_chain[0].clone(),
        quic_creds.key.clone_key(),
        &pins_dir,
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            log::warn!("client ({handle}) dial_any failed: {e}");
            connecting.lock().await.remove(&handle);
            return Err(LanMouseConnectionError::Quic(e));
        }
    };

    let peer = Rc::new(PeerSession::from_connection(conn));
    // 应用层 Hello 握手 —— 失败立即关连（不摘 peer 表因为还没注册）
    if let Err(e) = quic_transport::client_hello(&peer).await {
        log::warn!("client ({handle}) client_hello failed: {e}");
        connecting.lock().await.remove(&handle);
        return Err(LanMouseConnectionError::Quic(e));
    }

    let remote = peer.connection().remote_address();
    log::info!("client ({handle}) connected @ {remote} (quic)");
    client_manager.set_active_addr(handle, Some(remote));
    peers.lock().await.insert(remote, peer);
    connecting.lock().await.remove(&handle);
    Ok(())
}
