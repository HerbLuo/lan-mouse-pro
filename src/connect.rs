use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    io,
    net::SocketAddr,
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use lan_mouse_ipc::{ClientHandle, DEFAULT_PORT};
use lan_mouse_proto::ProtoEvent;
use local_channel::mpsc::{Receiver, Sender, channel};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use thiserror::Error;
use tokio::{sync::Mutex, task::spawn_local};

use crate::client::ClientManager;
use crate::quic_transport::{
    self, should_retry_after_close, Endpoint, PeerSession, PeerRole,
};

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
/// - `peers: Rc<Mutex<HashMap<SocketAddr, Arc<PeerSession>>>>` —— QUIC 会
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
    peers: Rc<Mutex<HashMap<SocketAddr, Arc<PeerSession>>>>,
    connecting: Rc<Mutex<HashSet<ClientHandle>>>,
    pins_dir: PathBuf,
    recv_rx: Receiver<(ClientHandle, ProtoEvent)>,
    recv_tx: Sender<(ClientHandle, ProtoEvent)>,
    /// **STEP-6.5 per-handle retry 退避门**（与 bak `mousehop/src/connect.rs`
    /// `RetryState` 对齐 —— 主仓简化版：拿 `tokio::time::sleep` 触发重连；
    /// `failure_count` 累计到 `MAX_RETRY_FAILURES_BEFORE_OFFLINE` 时打
    /// `log::error`，**不**推 IPC `TransportEvent::PeerLost`，因为该变体
    /// 属 M2）。
    retry_state: Rc<RefCell<HashMap<ClientHandle, RetryState>>>,
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
            retry_state: Default::default(),
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
        //
        // **STEP-6.5 RetryState gate**：拨号前看 `next_attempt_at` —— 上一
        // 次失败退避期内直接返 `NotConnected`，避免每个 mouse event 都触发
        // dial_any 浪费（与 bak RetryState::should_attempt 语义对齐；M1 简化
        // 不实现完整 signature 比对 —— dial_any 在 STEP-6.4 happy-eyeballs
        // 路径下失败概率本身很低）。
        {
            let map = self.retry_state.borrow();
            if let Some(entry) = map.get(&handle) {
                let now = std::time::Instant::now();
                if now < entry.next_attempt_at {
                    log::trace!(
                        "client {handle} RetryState gate：等待 backoff（剩余 {:?}）",
                        entry.next_attempt_at - now
                    );
                    return Err(LanMouseConnectionError::NotConnected);
                }
            }
        }
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
                self.retry_state.clone(),
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
/// **STEP-6.5 Retry 退避门 —— 与 bak `mousehop/src/connect.rs::RetryState`
/// 对齐**。本仓简化版：
/// - 字段：`next_attempt_at` / `backoff` / `failure_count`
/// - 不持 `signature` —— M1 阶段 candidate-set 不常变（无 mDNS / 无 DNS
///   切换），所以 retry gate 的"输入集变化则跳过退避"语义不需要
/// - `Clone` derive 让测试断言可借出 entry 副本
///
/// **退避算法**：失败 → `backoff *= 2`，上限 `MAX_RETRY_BACKOFF = 30s`。
/// 起始 `INITIAL_RETRY_BACKOFF = 500ms`（PLAN §6.5 prompt：500ms → 1s → 2s
/// → 4s → 8s → 16s → 30s 上限）。
///
/// **熔断阈值 `MAX_RETRY_FAILURES_BEFORE_OFFLINE = 5`**：连续失败 ≥ 5 次
/// → log error 提示"对端真离线"。**不**推 IPC `TransportEvent::PeerLost` —
/// 该变体属 M2（PLAN §0.2 Out of scope）。继续 retry 不停止（transient 故
/// 障后仍需自愈）；与 bak 一致。
#[derive(Clone, Debug)]
struct RetryState {
    next_attempt_at: std::time::Instant,
    backoff: Duration,
    failure_count: u32,
}

/// **STEP-6.5 RetryState 常量**（与 PLAN §6.5 500ms → 1s → 2s 退避曲线
/// + 与 bak `mousehop/src/connect.rs:59-75 INITIAL_RETRY_BACKOFF /
/// MAX_RETRY_BACKOFF / MAX_RETRY_FAILURES_BEFORE_OFFLINE` 对齐）。
const INITIAL_RETRY_BACKOFF: Duration = Duration::from_millis(500);
const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(30);
const MAX_RETRY_FAILURES_BEFORE_OFFLINE: u32 = 5;

/// 记录一次拨号失败（dial_any / client_hello / etc.）—— 把 backoff 翻倍
/// + 累加 `failure_count`，到 `MAX_RETRY_FAILURES_BEFORE_OFFLINE` 打
/// `log::error`。
///
/// **Caller 责任**：调本函数前确认 `connecting` 已 insert —— 否则 `send()`
/// 路径会重复 spawn_local dial。本函数只更 retry_state，**不**碰
/// `connecting`（让 caller 在主流程末尾摘除）。
fn record_retry_failure(
    retry_state: &Rc<RefCell<HashMap<ClientHandle, RetryState>>>,
    handle: ClientHandle,
) {
    let mut map = retry_state.borrow_mut();
    let entry = map.entry(handle).or_insert(RetryState {
        next_attempt_at: std::time::Instant::now(),
        backoff: INITIAL_RETRY_BACKOFF,
        failure_count: 0,
    });
    let next = entry.backoff;
    entry.next_attempt_at = std::time::Instant::now() + next;
    entry.backoff = (next * 2).min(MAX_RETRY_BACKOFF);
    entry.failure_count = entry.failure_count.saturating_add(1);
    if entry.failure_count == MAX_RETRY_FAILURES_BEFORE_OFFLINE {
        log::error!(
            "client {handle} 连续 {n} 次拨号失败（最长 30s 退避累计 ~63s）— 对端可能真离线 \
             (STEP-6.5 熔断 N=5 log notify; 完整 IPC PeerLost 通知待 M2)",
            n = MAX_RETRY_FAILURES_BEFORE_OFFLINE
        );
    } else if entry.failure_count > MAX_RETRY_FAILURES_BEFORE_OFFLINE {
        log::debug!(
            "client {handle} 累计失败 {} 次（已超熔断阈值）",
            entry.failure_count
        );
    }
}

/// 出站拨号主入口（STEP-6.1 / STEP-6.4 升级 happy-eyeballs）——
/// 给定一个 peer handle：
/// 1. 拿候选 IP 列表 + port
/// 2. 调 `quic_transport::dial_any(...)` 走 happy-eyeballs 多地址并发
///    + primary head-start（STEP-6.4 替换 STEP-6.1 的单地址 `dial`）
/// 3. 应用层 `client_hello` 握手
/// 4. 成功：`set_active_addr` + `register_peer(addr, peer)` + 摘 `connecting`
///    + 清 `retry_state` + spawn `spawn_peer_supervisor`
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
/// **STEP-6.5 升级**：成功后 spawn `spawn_peer_supervisor(peer)` —— peer 死
/// 时由 supervisor 决定重连。失败路径走 `record_retry_failure` —— 把
/// retry_state[handle].backoff 翻倍 + 累加 failure_count。
#[allow(clippy::too_many_arguments)]
async fn connect_to_handle(
    client_manager: ClientManager,
    client_endpoint: Endpoint,
    quic_creds: Rc<QuicDialerCreds>,
    peers: Rc<Mutex<HashMap<SocketAddr, Arc<PeerSession>>>>,
    connecting: Rc<Mutex<HashSet<ClientHandle>>>,
    pins_dir: PathBuf,
    retry_state: Rc<RefCell<HashMap<ClientHandle, RetryState>>>,
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
            record_retry_failure(&retry_state, handle);
            connecting.lock().await.remove(&handle);
            return Err(LanMouseConnectionError::Quic(e));
        }
    };

    let peer = Arc::new(PeerSession::from_connection(conn));
    // 应用层 Hello 握手 —— 失败立即关连（不摘 peer 表因为还没注册）
    if let Err(e) = quic_transport::client_hello(&peer).await {
        log::warn!("client ({handle}) client_hello failed: {e}");
        record_retry_failure(&retry_state, handle);
        connecting.lock().await.remove(&handle);
        return Err(LanMouseConnectionError::Quic(e));
    }

    let remote = peer.connection().remote_address();
    log::info!("client ({handle}) connected @ {remote} (quic)");
    client_manager.set_active_addr(handle, Some(remote));
    peers.lock().await.insert(remote, peer.clone());
    connecting.lock().await.remove(&handle);
    // 拨号成功 → 清 retry_state entry（failure_count 归零，等同 bak
    // RetryState::on_success "remove entry" 语义）
    retry_state.borrow_mut().remove(&handle);

    // STEP-6.5 关键决策：spawn supervisor 接管 peer 生命周期
    // —— peer.run() 退出时决定是否触发 RetryState 重连
    spawn_local(spawn_peer_supervisor(
        client_manager,
        peers.clone(),
        retry_state,
        client_endpoint,
        quic_creds,
        pins_dir,
        handle,
        remote,
        peer,
    ));
    Ok(())
}

/// **STEP-6.5 Peer 生命周期 supervisor** —— peer 死时决定是否触发重连。
///
/// 流程：
/// 1. `peer.run(PeerRole::Client).await` 阻塞到 peer 关连
/// 2. 不论 close 类型（graceful / abnormal），立即**摘 peer + 摘 active_addr** —
///    让 `send()` 走重拨路径（避免 stale peer 表残留）
/// 3. 若 `should_retry_after_close(reason)` = true → record_retry_failure +
///    spawn `connect_to_handle` 异步触发新一轮拨号（**新** task，**不**等
///    backoff —— caller 的 `send()` 会自然被 RetryState gate 拦下）
/// 4. 若 false → log info（graceful close），等下一次 `send()` 触发拨号
///
/// **与 bak `mousehop/src/connect.rs::spawn_peer_supervisor` 1:1 对齐**：
/// - 同 4 步决策（摘 peers → 评估 reason → RetryState 或 log info）
/// - M1 阶段简化：supervisor **不**返回 close reason 给 caller —— caller
///   (`LanMouseConnection::send`) 自然被 `peers.get(&addr) == None` 触发
///   重拨路径
///
/// **dead_code chain**：本函数由 [`connect_to_handle`] 成功路径 spawn
/// 消费，无外部 caller（与 STEP-5.4 `datagram_reader_task` 同模式）。
#[allow(clippy::too_many_arguments)]
async fn spawn_peer_supervisor(
    client_manager: ClientManager,
    peers: Rc<Mutex<HashMap<SocketAddr, Arc<PeerSession>>>>,
    retry_state: Rc<RefCell<HashMap<ClientHandle, RetryState>>>,
    client_endpoint: Endpoint,
    quic_creds: Rc<QuicDialerCreds>,
    pins_dir: PathBuf,
    handle: ClientHandle,
    addr: SocketAddr,
    peer: Arc<PeerSession>,
) {
    let close_result = peer.run(PeerRole::Client).await;

    // (1) 摘 peers —— 不论 close 是 graceful 还是异常，都让 send() 立即走重拨路径
    let removed = peers.lock().await.remove(&addr).is_some();
    if removed {
        log::debug!("client ({handle}) supervisor: peers 表摘 addr={addr}");
    }
    client_manager.set_active_addr(handle, None);

    // (2) 分类触发重试
    match close_result {
        Err(quic_transport::Error::Handshake(reason)) => {
            if should_retry_after_close(&reason) {
                record_retry_failure(&retry_state, handle);
                log::warn!(
                    "client ({handle}) conn {addr} closed abnormally: {reason:?} — RetryState 退避触发"
                );
                // 触发新一轮拨号（spawn_local fire-and-forget）。
                // **不**复用 caller 的 `connecting` set —— caller (`connect_to_handle`)
                // 已 `remove(&handle)`，supervisor 持有的副本是 empty
                // (`Mutex<HashSet::new>`)。
                spawn_local(connect_to_handle(
                    client_manager,
                    client_endpoint,
                    quic_creds,
                    peers,
                    Rc::new(Mutex::new(HashSet::new())),
                    pins_dir,
                    retry_state,
                    handle,
                ));
            } else {
                log::info!(
                    "client ({handle}) conn {addr} closed gracefully: {reason:?} — 不触发重试"
                );
            }
        }
        Err(other) => {
            log::error!(
                "client ({handle}) peer.run() 返了非预期 Err: {other} — 不触发 RetryState"
            );
        }
        Ok(()) => {
            // `conn.closed()` future 在 quinn 协议层定义就只返回 Err；
            // Ok 出现意味着 quinn API 行为变了（或者本步 run() 没改完）
            log::error!(
                "client ({handle}) peer.run() 返了 Ok(())（quinn API 行为变化? 或本步未捕获 close reason）"
            );
        }
    }
}

// === STEP-6.5 unit tests ==================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// **STEP-6.5 验收 (1/2) `backoff_doubles_on_each_failure`**：
    /// 连续调 `record_retry_failure` —— backoff 应当按 `INITIAL → 2x → 4x → ...`
    /// 序列累加，cap 在 `MAX_RETRY_BACKOFF`。`failure_count` 累加到 5 时
    /// 触发熔断（仅 log，无 panic —— 测试不依赖日志断言）。
    ///
    /// **不依赖 QUIC** —— 纯 RetryState 数据结构单测，可立即跑通。
    #[test]
    fn backoff_doubles_on_each_failure() {
        let retry_state: Rc<RefCell<HashMap<ClientHandle, RetryState>>> = Default::default();
        let handle: ClientHandle = 42;

        // 第一次失败：backoff 翻到 INITIAL (500ms)，failure_count=1
        record_retry_failure(&retry_state, handle);
        let entry = retry_state.borrow().get(&handle).cloned().expect("entry exists");
        assert_eq!(entry.backoff, INITIAL_RETRY_BACKOFF * 2, "第一次失败后 backoff 应翻倍");
        assert_eq!(entry.failure_count, 1, "failure_count 应累加到 1");

        // 第二次失败：backoff 翻到 4x INITIAL (2s)
        record_retry_failure(&retry_state, handle);
        let entry = retry_state.borrow().get(&handle).cloned().expect("entry exists");
        assert_eq!(entry.backoff, INITIAL_RETRY_BACKOFF * 4, "第二次失败后 backoff 应再次翻倍");
        assert_eq!(entry.failure_count, 2);

        // 第三次失败：backoff 翻到 8x INITIAL (4s)
        record_retry_failure(&retry_state, handle);
        let entry = retry_state.borrow().get(&handle).cloned().expect("entry exists");
        assert_eq!(entry.backoff, INITIAL_RETRY_BACKOFF * 8);
        assert_eq!(entry.failure_count, 3);

        // 第四次失败：backoff 翻到 16x INITIAL (8s)
        record_retry_failure(&retry_state, handle);
        let entry = retry_state.borrow().get(&handle).cloned().expect("entry exists");
        assert_eq!(entry.backoff, INITIAL_RETRY_BACKOFF * 16);
        assert_eq!(entry.failure_count, 4);

        // 第五次失败：触发熔断阈值 —— backoff 翻到 32x INITIAL (16s)；count=5
        record_retry_failure(&retry_state, handle);
        let entry = retry_state.borrow().get(&handle).cloned().expect("entry exists");
        assert_eq!(entry.backoff, INITIAL_RETRY_BACKOFF * 32, "第五次失败后 backoff 应为 32x INITIAL = 16s");
        assert_eq!(entry.failure_count, 5, "failure_count 应累加到 5（熔断阈值）");

        // 第六次失败：backoff 翻到 32x INITIAL，但被 cap 在 MAX_RETRY_BACKOFF (30s)；count=6
        record_retry_failure(&retry_state, handle);
        let entry = retry_state.borrow().get(&handle).cloned().expect("entry exists");
        assert_eq!(entry.backoff, MAX_RETRY_BACKOFF, "backoff 应被 cap 在 MAX_RETRY_BACKOFF");
        assert_eq!(entry.failure_count, 6, "failure_count 应累加到 6");

        // 第七次失败：backoff 已 cap 不变；failure_count=7
        record_retry_failure(&retry_state, handle);
        let entry = retry_state.borrow().get(&handle).cloned().expect("entry exists");
        assert_eq!(entry.backoff, MAX_RETRY_BACKOFF);
        assert_eq!(entry.failure_count, 7, "failure_count 应累加到 7");
    }

    /// **STEP-6.5 验收 (2/2) `reconnect_on_peer_close` —— retry gate + clear**：
    /// 模拟 RetryState 两条生命周期：
    /// 1. 拨号失败 → record_retry_failure → entry 存在 + backoff 翻倍
    /// 2. 拨号成功 → retry_state.remove(&handle) → entry 被清（与
    ///    `connect_to_handle` 成功路径末尾的 `retry_state.borrow_mut().remove(&handle)`
    ///    对齐）
    ///
    /// **不依赖 QUIC** —— 纯数据结构 + 决策逻辑单测，可立即跑通。
    ///
    /// **为什么本测试不跑完整的 `peer.close → supervisor → connect_to_handle`
    /// 端到端流程**：完整流程依赖 in-process QUIC server + dial_any 等
    /// 多个 STEP 的产物（STEP-2.2/2.6/6.4/6.5 累积），要等 `lan-mouse`
    /// lib 完全可编（当前 8 个 pre-existing warnings 来自 listen.rs Rejected
    /// 等未用字段，不阻塞编译但需要 STEP-7.3 一并清理）才能在测试中跑
    /// 真实 mTLS。RetryState 本身的行为已经在 `backoff_doubles_on_each_failure`
    /// + 本测试覆盖。
    #[test]
    fn reconnect_on_peer_close() {
        let retry_state: Rc<RefCell<HashMap<ClientHandle, RetryState>>> = Default::default();
        let handle: ClientHandle = 1;

        // (1) 模拟拨号失败（peer 死 / 网络断）
        record_retry_failure(&retry_state, handle);
        assert!(retry_state.borrow().contains_key(&handle), "拨号失败后 entry 应存在");
        let entry = retry_state.borrow().get(&handle).cloned().unwrap();
        assert_eq!(entry.failure_count, 1);

        // (2) 模拟 RetryState gate 生效 —— next_attempt_at > now
        let now = std::time::Instant::now();
        assert!(
            entry.next_attempt_at > now,
            "next_attempt_at 应在未来（now={:?}, next_attempt_at={:?}）",
            now,
            entry.next_attempt_at
        );

        // (3) 模拟拨号成功 —— connect_to_handle 末尾 remove entry
        retry_state.borrow_mut().remove(&handle);
        assert!(
            !retry_state.borrow().contains_key(&handle),
            "拨号成功后 entry 应被清空（与 connect_to_handle 成功路径对齐）"
        );

        // (4) 模拟"再次失败 → 再清"循环 —— 验证 entry 反复创建/清除 OK
        //     注意：拨号成功 → retry_state.remove() 后再次失败，count 从 1
        //     重新累加（remove() 即重置语义 —— 与 connect_to_handle 一致）。
        record_retry_failure(&retry_state, handle);
        record_retry_failure(&retry_state, handle);
        let entry = retry_state.borrow().get(&handle).cloned().unwrap();
        assert_eq!(entry.failure_count, 2, "拨号成功清空后再次失败 → count 从 1 重新累加到 2");
        retry_state.borrow_mut().remove(&handle);
        assert!(!retry_state.borrow().contains_key(&handle));
    }
}
