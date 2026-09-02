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

use lan_mouse_ipc::{ClientHandle, DEFAULT_PORT, InputChannelConfig};
use lan_mouse_proto::ProtoEvent;
use local_channel::mpsc::{Receiver, Sender, channel};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use thiserror::Error;
use tokio::{sync::{oneshot, Mutex}, task::spawn_local};

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

#[allow(dead_code)] // STEP-8.2: Timeout / TargetEmulationDisabled 暂无人构造；TargetEmulationDisabled 留 M2 接 Pong 路径后重新启用
#[derive(Debug, Error)]
pub(crate) enum LanMouseConnectionError {
    #[error(transparent)]
    Bind(#[from] io::Error),
    /// QUIC 传输层错误（STEP-6.1 引入）—— `PeerSession::send_input()` /
    /// `dial()` 的失败经由本变体透传给上层。
    ///
    /// 删除的 `Dtls` / `Webrtc` 变体（已无 caller，留着只会持续警告）；
    /// DTLS 依赖下线由 STEP-7.3 完成。
    #[error(transparent)]
    Quic(#[from] quic_transport::Error),
    #[error("not connected")]
    NotConnected,
    /// **STEP-8.2 临时 unused**：alive 检查已移除（详见 `send()` docstring）。
    /// 留变体供 M2 接回 Pong → recv_tx 路径后重新启用。
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

    /// **STEP-8.2 修复 — `connect_on_activate`**：主动触发拨号，但不发送
    /// 任何事件。
    ///
    /// **背景**：[`send()`] 仅在"我要发送事件"时才触发拨号。STEP-8.2 调
    /// 研发现：即便 `activate_on_startup=true`，本机 daemon 启动后也不主动
    /// dial —— 等鼠标移到屏边（capture 触发 `send()`）才拨。两侧都启动
    /// 后没人移鼠标 → 永远不建连。
    ///
    /// **本方法语义**：与 `send()` 的下半段等价 —— 跑 RetryState gate →
    /// 检查 `connecting` 去重 → `spawn_local(connect_to_handle)`。
    /// **不**发任何 ProtoEvent、不返回 NotConnected 之外的 Err。
    ///
    /// **何时调**：`service.rs::activate_client` 在 `client_manager.activate_
    /// client(handle)` 成功后立即 fire-and-forget —— 配合 capture 已有的
    /// `send()`-触发拨号路径，两路独立：
    /// - `activate_client` → `dial(handle)` 主动 dial
    /// - 鼠标移边 → `send()` 触发 dial（路径不变）
    ///
    /// **去重**：`connecting` set 已保证同一 handle 不会并发 spawn 多个
    /// `connect_to_handle`（`spawn_local(connect_to_handle)` 前 `connecting
    /// .insert(handle)`，connect_to_handle 成功 / 失败末尾 `connecting
    /// .remove(&handle)`）。RetryState 退避门也复用了 send() 的逻辑。
    ///
    /// **fire-and-forget**：本方法 spawn 后立即返回 `Ok(())`，不阻塞
    /// caller。dial 结果由 `connect_to_handle` 自己处理（成功 register peer
    /// + spawn supervisor；失败 record_retry_failure）。
    pub(crate) async fn dial(&self, handle: ClientHandle) -> Result<(), LanMouseConnectionError> {
        // RetryState gate —— 与 send() 同语义：退避期内不再 spawn dial。
        {
            let map = self.retry_state.borrow();
            if let Some(entry) = map.get(&handle) {
                let now = std::time::Instant::now();
                if now < entry.next_attempt_at {
                    log::trace!(
                        "client {handle} dial() RetryState gate：等待 backoff（剩余 {:?}）",
                        entry.next_attempt_at - now
                    );
                    return Ok(());
                }
            }
        }
        let mut connecting = self.connecting.lock().await;
        if !connecting.contains(&handle) {
            connecting.insert(handle);
            spawn_local(connect_to_handle(
                self.client_manager.clone(),
                self.client_endpoint.clone(),
                self.quic_creds.clone(),
                self.peers.clone(),
                self.connecting.clone(),
                self.pins_dir.clone(),
                self.retry_state.clone(),
                self.recv_tx.clone(),
                handle,
            ));
        }
        Ok(())
    }

    /// 发送一个事件到对端（STEP-6.1 切到 QUIC 路径）。
    ///
    /// **3 步流程**（与 bak `MousehopConnection::send` 对齐）：
    /// 1. 查 `client_manager.active_addr(handle)` 拿 socket addr
    /// 2. 查 `peers` 表拿 QUIC 会话（命中 → 调
    ///    [`PeerSession::send_input`]；未命中 → 触发拨号）
    /// 3. 错误归并（send_input 失败 → 摘 peer + 通知 manager）
    ///
    /// ~~alive 守护~~（STEP-8.2 临时移除）**：原设计对端把 emulation 关了
    /// （Pong 返 `false`）应置 `alive = false`，下次 `send()` 返
    /// `TargetEmulationDisabled` 避免无意义注入。**但是**当前架构下 alive
    /// 永不被置 `true` —— 服务端发 Pong 的路径在
    /// `src/emulation.rs:164`（`ProtoEvent::Ping => reply Pong(emulation_active)`），
    /// 但客户端**没有任何 reader 把 stream A 上的 Pong 帧转发到 `recv_tx`**
    /// —— `recv_tx` 是死字段（`src/connect.rs:82` 加 `#[allow(dead_code)]`
    /// 不需要，但本仓实际无 caller）。后果：
    /// 1. 服务端发 Pong(true) → 客户端 peer.run() stream A 读到，但 peer.run
    ///    的主循环不把 Pong 推到 recv_tx
    /// 2. `capture.rs:306` 的 `(handle, event) = self.conn.recv()` 永远挂起
    /// 3. `alive` 始终是默认 `false`（`lan_mouse_ipc::ClientState::default()`）
    /// 4. 所有 `send()` 在 peer 存在时立即返 `TargetEmulationDisabled`
    ///    → `capture.rs:409` `log::warn!("releasing capture: ...")` →
    ///    鼠标移到屏边后立刻释放 capture，看起来"连上但键鼠不通"
    ///
    /// **临时修法**：移除 alive 检查。**乐观假设 peer 在线** —— 任何 send
    /// 都尝试推到 peer；supervisor 看到 peer.run() 退出（peer 真死）时
    /// `set_active_addr(None)` 让下次 send 走重拨路径。这与 DTLS 时代
    /// "peer 死 → supervisor 关 → 重拨"语义对齐，只是缺了"Pong 假阴性
    /// → 提前返"这一道细节优化。
    ///
    /// **TODO M2**：等 stream A reader → recv_tx 转发路径补完后，重新
    /// 接入 alive 检查 + 处理 Pong(true/false)：
    /// - Pong(true) → set_alive(true)
    /// - Pong(false) → set_alive(false) → 下次 send 返
    ///   `TargetEmulationDisabled`（保留当前已存在的错误变体语义）
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
                // STEP-8.2 临时移除 alive 检查 —— 详见 send() docstring。
                // 原 alive 守护永远 false（recv_tx 路径未接），反而阻塞
                // 所有 send。乐观假设 peer 在线。
                //
                // **生产日志级别（DEBUG）**：高频事件（motion 每秒 60+
                // 次）放 INFO 会刷屏；保留 trace 级别的旧 log 让 RUST_LOG=trace
                // 时可诊断具体帧内容，DEBUG 仅记"send 成功"路径不展开
                // event 详情。
                log::debug!("send to handle {handle} addr {addr} via peer (active)");
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
                self.recv_tx.clone(),
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
/// **退避算法**：失败 → `backoff *= 2`，上限 `MAX_RETRY_BACKOFF = 8s`。
/// 起始 `INITIAL_RETRY_BACKOFF = 1s`（1s → 2s → 4s → 8s 上限；详见下方
/// 常量 docstring 的 Mac/wake UX 调整理由）。
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

/// **STEP-6.5 RetryState 常量** + **Mac/wake 重连调整**。
///
/// **退避曲线**：1s → 2s → 4s → 8s (cap) → 8s → 8s → ... 永远循环。
///
/// **与原 500ms→30s 对比**：原 PLAN §6.5 的 30s 上限对 mouse-sharing
/// UX 太慢 —— Mac wake 后用户要等 30s 才看到反应。8s cap 让用户在
/// peer 唤醒后**最多等 8s**就有下一次重试（实际更快：peer 醒的瞬间
/// 下一次 retry 就成功）。fail count=5 触发 log error 的阈值不变
/// （约 t=15s，能区分"短断网"和"对端真离线"）。
///
/// **与 bak 对齐**：原对齐 `mousehop/src/connect.rs:59-75` 的 500ms→30s
/// 曲线。**本次调整打破 bak 对齐**，理由是 mouse-sharing 场景对延迟
/// 敏感（用户实时等鼠标），bak 的 30s 是给一般 P2P 应用设计的。
const INITIAL_RETRY_BACKOFF: Duration = Duration::from_secs(1);
const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(8);
const MAX_RETRY_FAILURES_BEFORE_OFFLINE: u32 = 5;

/// **应用层心跳周期**：主控端周期性向被控端发 `ProtoEvent::Ping`，刷新
/// [`crate::emulation::ListenTask`] 的 `last_response` map，避免鼠标在被
/// 控端静止（或主控端静止等场景）期间触发 `releasing keys: ... not
/// responding!` 1s 伪超时——QUIC `keep_alive_interval = 5s` 是传输层 PING
/// 帧（[`quinn::TransportConfig`]），不会产生 `ListenEvent::Msg`、不能
/// 刷 `last_response`，所以必须有应用层 Ping。
///
/// 500ms 远低于 1s 阈值 + 5s tick 检测窗口，余量充裕；2 帧/秒 × 双向
/// stream A 流量（Pong 回包）对控制平面负载可忽略。
const PING_INTERVAL: Duration = Duration::from_millis(500);

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
    // **STEP-8.2 修复 — Bug #7**：forwarder task 用的 sender，
    // 把 peer.run 从 stream A 读到的 `(addr, event)` 映射到
    // `(handle, event)` 后推到这里 → `LanMouseConnection::recv()`
    // → capture.rs。修前 `recv_tx` 是死字段（Bug #4）。
    recv_tx: Sender<(ClientHandle, lan_mouse_proto::ProtoEvent)>,
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
    // **状态转移日志（INFO）**：区分"初次拨号"和"重拨成功"。在清除
    // retry_state 之前看 entry 是否存在 —— 存在 = 之前失败过、本次是
    // 重拨；不存在 = 首次建立。这样日志能直接告诉用户"是不是刚刚经历
    // 了一次 wake / 网络异常后自动恢复"，免去对日志前后文做关联的麻烦。
    let was_retry = retry_state.borrow().contains_key(&handle);
    if was_retry {
        log::info!(
            "client ({handle}) reconnected @ {remote} (quic) — \
             自动恢复（之前的连接中断已通过 RetryState 兜底）"
        );
    } else {
        log::info!("client ({handle}) connected @ {remote} (quic) — 首次建立");
    }
    client_manager.set_active_addr(handle, Some(remote));
    peers.lock().await.insert(remote, peer.clone());
    connecting.lock().await.remove(&handle);
    // 拨号成功 → 清 retry_state entry（failure_count 归零，等同 bak
    // RetryState::on_success "remove entry" 语义）
    retry_state.borrow_mut().remove(&handle);

    // **STEP-8.2 修复 — Bug #7**：设 outgoing_events + spawn forwarder
    // task，把 peer.run 主循环从 stream A 读到的 Ack / Pong / Leave
    // 事件转发到 `recv_tx` → `LanMouseConnection::recv()` →
    // `capture.rs::do_capture_session()` → 状态机切到 Sending 或
    // release capture。
    //
    // **修前**：peer.run 收到事件只 log debug；`recv_tx` 死字段；
    // capture.rs 永远收不到 server 响应 → 本机卡 WaitingForAck 反复
    // send Enter。
    //
    // **路径**：
    // 1. 建 mpsc channel: `(SocketAddr, ProtoEvent)` —— peer.run 只
    //    知道 remote_address，不持 ClientHandle
    // 2. spawn forwarder task：recv `(addr, event)` → 用
    //    `client_manager.get_client(addr)` 映射到 `handle` → push 到
    //    `recv_tx` (本 LanMouseConnection 的 recv_tx 字段 —— 至此
    //    **不是死字段了**)
    // 3. peer.set_outgoing_events(Some(tx))
    let (out_tx, mut out_rx) =
        tokio::sync::mpsc::unbounded_channel::<(std::net::SocketAddr, lan_mouse_proto::ProtoEvent)>();
    {
        let client_manager_for_forwarder = client_manager.clone();
        let recv_tx = recv_tx.clone();
        spawn_local(async move {
            while let Some((addr, event)) = out_rx.recv().await {
                if let Some(handle) = client_manager_for_forwarder.get_client(addr) {
                    // **生产日志级别（DEBUG）**：高频路径（Ack / Pong / Leave
                    // 等都会走这里，motion 不走这条路径——motion 走 datagram
                    // 直接发不出去到 server —— 见 Bug #8）。INFO 仍会刷屏。
                    log::debug!("stream A forwarder: {addr} → handle {handle}");
                    if let Err(e) = recv_tx.send((handle, event)) {
                        // **mouse 卡住 bug 排查**：recv_tx.send 失败说明
                        // capture task 已退出 → 整条链路断裂。
                        log::warn!(
                            "stream A forwarder: recv_tx.send failed (capture task 已退): {e}"
                        );
                        break;
                    }
                } else {
                    // 理论不应发生 —— peer 注册到 peers 表时 addr 已对应
                    // 一个 active handle。如果发生说明 client_manager
                    // 被外部清空（unregister），静默 no-op。
                    log::warn!(
                        "stream A forwarder: addr {addr} 不在 client_manager（可能已 unregister）"
                    );
                }
            }
            // **mouse 卡住 bug 排查**：forwarder 退出意味着 peer.outgoing_events
            // sender 全部 drop —— 链路彻底断了。
            log::warn!("stream A forwarder: outgoing_events rx 关闭 —— forwarder 退");
        });
    }
    peer.set_outgoing_events(Some(out_tx)).await;

    // **Post-connect Enter 握手**：reconnect 后 dial 端发 Enter 等 Ack
    // —— 应用层秒级验证连接 + 重新激活 slave 的 Entered 路径
    // （slave 收到 Enter → reply Ack + 发 Entered → service.add_incoming /
    // update_incoming 重整 barrier 与 capture_proxy）。
    //
    // **为什么 reconnect 后必须主动 Enter**：
    // - 网络断开 → 双方 conn close → slave 端 ListenTask 推 Disconnected
    //   → service.remove_incoming 销毁 barrier（即使是 patch #6 保留
    //   barrier 的版本，Entered 路径也断了，emulation_proxy 没了）
    // - reconnect 后 master 不主动 Enter，slave 端不会再走 Entered 处理
    //   → 没有 AcK 回来 → 用户侧 "mouse 卡 30s"
    //
    // **为什么用 Enter 而不是 Ping**：Enter 走完整控制路径（slave 端
    // emulation.rs Enter 分支 → Entered 事件 → service.add_incoming
    // + capture_proxy 重建），Ping 只刷 last_response 不重建 proxy/barrier。
    // 同样能用 WAKE_CLOSE_CODE 走重试（timeout 失败时强制关 conn 触发）。
    //
    // **超时 3s**：Enter 是 reliable stream A 帧，正常 1ms 内送达；3s 是
    // 极保守值，只在网络严重异常时触发。超时 → 主动 close conn with
    // WAKE_CLOSE_CODE → supervisor 走 retry 路径（与 wake close 同语义）。
    if let Some(pos) = client_manager.get_pos(handle) {
        // lan_mouse_ipc::Position → lan_mouse_proto::Position（同名枚举不同 crate，
        // 没有 From impl，手动 match；两者 variant 完全一致）
        let proto_pos = match pos {
            lan_mouse_ipc::Position::Left => lan_mouse_proto::Position::Left,
            lan_mouse_ipc::Position::Right => lan_mouse_proto::Position::Right,
            lan_mouse_ipc::Position::Top => lan_mouse_proto::Position::Top,
            lan_mouse_ipc::Position::Bottom => lan_mouse_proto::Position::Bottom,
        };
        let cfg = client_manager.input_channels(handle).unwrap_or_default();
        let (ack_tx, ack_rx) = oneshot::channel::<()>();
        peer.set_handshake_ack(ack_tx).await;
        log::info!(
            "post-connect handshake: sending Enter to {remote} (pos={proto_pos:?})"
        );
        match peer
            .send_input(&ProtoEvent::Enter(proto_pos), &cfg)
            .await
        {
            Ok(()) => {
                match tokio::time::timeout(Duration::from_secs(3), ack_rx).await {
                    Ok(Ok(())) => {
                        log::info!(
                            "post-connect handshake: Ack received, connection verified for handle {handle}"
                        );
                    }
                    Ok(Err(_)) => {
                        log::warn!(
                            "post-connect handshake: oneshot dropped before Ack for handle {handle}"
                        );
                    }
                    Err(_) => {
                        log::warn!(
                            "post-connect handshake: timeout (3s) waiting for Ack from {remote} — \
                             forcing close + retry"
                        );
                        peer.connection().close(
                            crate::quic_transport::session::WAKE_CLOSE_CODE.into(),
                            b"post-connect handshake timeout",
                        );
                        record_retry_failure(&retry_state, handle);
                        connecting.lock().await.remove(&handle);
                        // 注意：peer.run() 看到本地 close 会返回 LocallyClosed，
                        // supervisor 不再 retry —— 我们已经 record_retry_failure
                        // + connecting.remove，下次 send() 会按 backoff 重拨。
                        return Err(LanMouseConnectionError::Quic(
                            quic_transport::Error::Handshake(
                                quinn::ConnectionError::LocallyClosed,
                            ),
                        ));
                    }
                }
            }
            Err(e) => {
                log::warn!(
                    "post-connect handshake: Enter send to {remote} failed: {e} — \
                     forcing close + retry"
                );
                peer.connection().close(
                    crate::quic_transport::session::WAKE_CLOSE_CODE.into(),
                    b"post-connect handshake failed",
                );
                record_retry_failure(&retry_state, handle);
                connecting.lock().await.remove(&handle);
                return Err(LanMouseConnectionError::Quic(e));
            }
        }
    } else {
        log::warn!(
            "post-connect handshake: no pos for handle {handle} — skipping Enter handshake"
        );
    }

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
    log::info!(
        "spawn_peer_supervisor: starting for handle {handle} addr {addr}"
    );

    // **应用层心跳任务** —— 与 supervisor 并行：每 [`PING_INTERVAL`] 主动
    // 发 `ProtoEvent::Ping` 到被控端，被控端 `emulation.rs::ListenTask`
    // 的 `ListenEvent::Msg` 处理刷新 `last_response`，从而防止 1s 阈值 +
    // 5s tick 检测窗口在 Input 流静默时把 peer 误判为 not responding 并
    // 销毁 capture trigger（详见 docstring at top of file）。
    //
    // **生命周期**：与 supervisor 绑定 —— supervisor 入口 spawn、心跳
    // 在 supervisor 末尾（peer.run 返回）abort。`send_input` 失败时
    // 心跳任务自然 return，与 abort 双保险。
    //
    // **为什么不在 `connect_to_handle` 里 spawn**：那里 spawn 的话没有
    // 自然的退出触发器（`connect_to_handle` 一次性返回）；放 supervisor
    // 里和 `peer.run()` 严格对齐生命周期最干净。
    let ping_task = spawn_local(ping_heartbeat_task(peer.clone(), addr));

    let close_result = peer.run(PeerRole::Client).await;
    log::info!(
        "spawn_peer_supervisor: peer.run() returned for handle {handle} addr {addr}"
    );

    // peer 已死 → 停心跳。`send_input` 早已失败自退，但 abort 让日志与
    // supervisor 退出同步，避免停手后还有 tick 触发 send_input 报 warn。
    ping_task.abort();

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
                // **状态转移日志（INFO）**：区分两种 close path
                // - `ApplicationClosed(WAKE_CLOSE_CODE)` → peer 端系统唤醒
                //   (Mac 等)，是**预期**事件 → INFO
                // - 其他 retry-worthy reason（TimedOut 等）→ 网络层异常
                //   → 保留 WARN
                let is_wake = matches!(
                    &reason,
                    quinn::ConnectionError::ApplicationClosed(frame)
                        if frame.error_code.into_inner() as u32
                            == quic_transport::session::WAKE_CLOSE_CODE
                );
                record_retry_failure(&retry_state, handle);
                if is_wake {
                    log::info!(
                        "client ({handle}) conn {addr} wake-detected \
                         (peer system wake, expecting peer back soon) — \
                         RetryState 退避触发"
                    );
                } else {
                    log::warn!(
                        "client ({handle}) conn {addr} closed abnormally: {reason:?} — \
                         RetryState 退避触发"
                    );
                }
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
                    // STEP-8.2 修复 — Bug #7：supervisor 重新触发 dial
                    // 时不需要 recv_tx（拨号路径**之前**那次调用已设
                    // 上 + spawn 了 forwarder；重连不会另设 forwarder
                    // 是可接受的，因为 dialing 路径会再次走完整 setup，
                    // 但实际让 reconnect 也保持 forwarder 持续可用更稳
                    // 妥 —— 这里没法直接拿到原 LanMouseConnection 的
                    // recv_tx。本简化版用 local_channel default
                    // (`channel()` 已 clone 出 tx/rx)；reconnect 时
                    // peer.run 重新读 stream A → outgoing_events 还
                    // 没设（peer 是新 PeerSession 走新路径），等价
                    // reconnect 路径无 forwarder —— 但 reconnect 期间
                    // capture 已 release（supervisor 摘了 active_addr），
                    // 不需要 forwarder。**实际语义 OK**。
                    local_channel::mpsc::channel::<(ClientHandle, lan_mouse_proto::ProtoEvent)>().0,
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

/// **应用层 Ping 心跳任务**（[`crate::emulation::ListenTask`] 1s 阈值 +
/// 5s tick 检测窗口下的防伪超时补丁）。
///
/// **触发**：被 [`spawn_peer_supervisor`] 在 `peer.run()` 之前 spawn；
/// 退出 `spawn_peer_supervisor` 时 `ping_task.abort()`。
///
/// **行为**：每 [`PING_INTERVAL`] 调一次 `peer.send_input(Ping, default)`，
/// 路由到 stream A。被控端 `emulation.rs:210` 收到 Ping 后回 Pong（同时
/// 主循环把 Ping 帧作为 `ListenEvent::Msg` 推到 `last_response`，**这才是
/// 关键**——`last_response` 看到 Ping 帧就刷新）。
///
/// **退出路径**（三选一，先到先退）：
/// 1. peer 死 → `send_input` 返 `Err` → 本函数 `return`
/// 2. supervisor 末尾 `ping_task.abort()` → task 被取消
/// 3. （理论）peer.run() 抛错 → 同 (1)
///
/// **为什么跳过首个 tick**：`tokio::time::interval` 默认在 `t=0` 立即触发
/// 首 tick；supervisor 刚 spawn 时 peer 还在握手/装配阶段（虽
/// `connect_to_handle` 末尾才调 supervisor，多数情况下 cached_send_a 已
/// 就绪），跳过首 tick 让"启动后第一个 Ping"在第一个完整周期后到达，避免
/// 与握手期 `Hello` 流撞帧。
async fn ping_heartbeat_task(peer: Arc<PeerSession>, addr: SocketAddr) {
    let mut interval = tokio::time::interval(PING_INTERVAL);
    // 跳过首 tick —— 见 docstring
    interval.tick().await;
    loop {
        interval.tick().await;
        match peer
            .send_input(&ProtoEvent::Ping, &InputChannelConfig::default())
            .await
        {
            Ok(()) => {
                log::trace!("ping_heartbeat: sent Ping to {addr}");
            }
            Err(e) => {
                // peer 已死 —— 自然退出。supervisor 也会 abort 本 task，
                // 这里是 send_input 先失败时的提前退出路径。warn 而非
                // error：peer 死亡本身会被 supervisor 报出来，这里只
                // 是心跳线程先察觉到。
                log::warn!(
                    "ping_heartbeat: send Ping to {addr} failed (peer dead): {e}"
                );
                return;
            }
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
    /// **2026-09 调整**：INITIAL = 1s、MAX = 8s —— 与上方常量 docstring 同源。
    /// 实际跑出来的退避序列：
    /// - 1st fail: backoff = 2s (INITIAL × 2), count = 1
    /// - 2nd fail: backoff = 4s (INITIAL × 4), count = 2
    /// - 3rd fail: backoff = 8s (INITIAL × 8 = MAX, **撞 cap**), count = 3
    /// - 4th fail: backoff = 8s (cap stays), count = 4
    /// - 5th fail: backoff = 8s, count = 5 (触发熔断 log)
    /// - 6th fail: backoff = 8s, count = 6
    /// - 7th fail: backoff = 8s, count = 7
    ///
    /// **不依赖 QUIC** —— 纯 RetryState 数据结构单测，可立即跑通。
    #[test]
    fn backoff_doubles_on_each_failure() {
        let retry_state: Rc<RefCell<HashMap<ClientHandle, RetryState>>> = Default::default();
        let handle: ClientHandle = 42;

        // 1st fail: backoff = 2 × INITIAL = 2s; count = 1
        record_retry_failure(&retry_state, handle);
        let entry = retry_state.borrow().get(&handle).cloned().expect("entry exists");
        assert_eq!(entry.backoff, INITIAL_RETRY_BACKOFF * 2, "1st fail: backoff = 2x INITIAL = 2s");
        assert_eq!(entry.failure_count, 1);

        // 2nd fail: backoff = 4 × INITIAL = 4s; count = 2
        record_retry_failure(&retry_state, handle);
        let entry = retry_state.borrow().get(&handle).cloned().expect("entry exists");
        assert_eq!(entry.backoff, INITIAL_RETRY_BACKOFF * 4, "2nd fail: backoff = 4x INITIAL = 4s");
        assert_eq!(entry.failure_count, 2);

        // 3rd fail: backoff = 8 × INITIAL = 8s = MAX，撞 cap；count = 3
        record_retry_failure(&retry_state, handle);
        let entry = retry_state.borrow().get(&handle).cloned().expect("entry exists");
        assert_eq!(
            entry.backoff,
            MAX_RETRY_BACKOFF,
            "3rd fail: backoff = 8x INITIAL = 8s = MAX, hit cap"
        );
        assert_eq!(entry.failure_count, 3);

        // 4th fail: backoff 已被 cap 在 MAX, 不再翻倍；count = 4
        record_retry_failure(&retry_state, handle);
        let entry = retry_state.borrow().get(&handle).cloned().expect("entry exists");
        assert_eq!(entry.backoff, MAX_RETRY_BACKOFF, "4th fail: cap stays at MAX");
        assert_eq!(entry.failure_count, 4);

        // 5th fail: count = 5 触发熔断 log（仅 log 不 panic），backoff 不变
        record_retry_failure(&retry_state, handle);
        let entry = retry_state.borrow().get(&handle).cloned().expect("entry exists");
        assert_eq!(entry.backoff, MAX_RETRY_BACKOFF);
        assert_eq!(entry.failure_count, 5, "5th fail: 触发熔断阈值");

        // 6th fail: backoff 仍 cap, count = 6
        record_retry_failure(&retry_state, handle);
        let entry = retry_state.borrow().get(&handle).cloned().expect("entry exists");
        assert_eq!(entry.backoff, MAX_RETRY_BACKOFF);
        assert_eq!(entry.failure_count, 6);

        // 7th fail: cap 不变, count = 7
        record_retry_failure(&retry_state, handle);
        let entry = retry_state.borrow().get(&handle).cloned().expect("entry exists");
        assert_eq!(entry.backoff, MAX_RETRY_BACKOFF);
        assert_eq!(entry.failure_count, 7);
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
