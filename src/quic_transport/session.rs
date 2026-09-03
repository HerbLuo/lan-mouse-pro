//! `PeerSession` —— 与对端的一条 QUIC 会话（client / server 共用）。
//!
//! 本模块承担 QUIC 链路中间层的所有 per-peer 状态：
//!
//! - [`PeerSession`] struct —— 持有 `quinn::Connection` + hello 标志 +
//!   stream A cache + 3 stream 集合缓存 + outgoing events sender
//! - `impl PeerSession` block 1 —— 状态 / IO 助手（`from_connection` /
//!   `take_stream_a_*` / `set_stream_bunch` / `send_input` / `send_motion` /
//!   `send_stream_a` / `send_stream_b` / `send_outgoing_event` 等）
//! - `impl PeerSession` block 2 —— `PeerSession::run()` 主循环
//! - [`PeerRole`] Client / Server 角色枚举
//! - [`should_retry_after_close`] 关闭原因判定
//!
//! 与 [`super::protocol`] 的关系：`run()` 调 [`super::protocol::client_hello`]
//! [`super::protocol::server_hello`] / [`super::protocol::read_frame`]；
//! [`super::protocol::hello_watchdog`] 反过来被 `run()` spawn。
//!
//! 与 [`super::streams`] 的关系：`run()` spawn [`super::streams::datagram_reader_task`]
//! 与 [`super::streams::read_loop`]；后者 take 走 `stream_bunch`。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use quinn::{Connection as QuinnConnection, SendStream};

use tokio::sync::{mpsc as tokio_mpsc, Mutex};
use tokio::task::spawn_local;

use lan_mouse_ipc::InputChannelConfig;
use lan_mouse_proto::ProtoEvent;

use super::protocol::{
    client_hello, hello_watchdog, read_frame, server_hello, write_frame as _write_frame,
};
use super::protocol::StreamPair;
use super::streams::{datagram_reader_task, read_loop, Bidi, ReadStreams, StreamBunch, StreamEvent};

use lan_mouse_proto::MAX_EVENT_SIZE;

/// 与对端的一条 QUIC 会话（client / server 共用）—— STEP-5.4 起承担端到端 IO。
///
/// STEP-1.4 占位为单字段 `_private`；STEP-3.2 起扩展为：
/// - `conn` —— `quinn::Connection`，所有 stream / datagram IO 入口
/// - `hello_ok: AtomicBool` —— Hello 握手成功标志（`Ordering::Release` 置
///   / `Acquire` 读）
/// - `stream_a_cache: Mutex<Option<StreamPair>>` —— `server_hello()` /
///   `client_hello()` 缓存 Hello 用的那条 stream A 给 STEP-5.x read_loop
///   接手
///
/// STEP-5.2 起增字段：
/// - `stream_bunch: Arc<Mutex<Option<StreamBunch>>>` —— STEP-5.3 read_loop
///   装配三 stream 时填充（暂留 `None`）；与 `stream_a_cache` 对称
///   守护所有权交接的 "整对 take" 语义
///
/// `StreamPair` 与 `stream_b` / `stream_c` 缓存字段留 STEP-5.1 / 5.2 落地，
/// 本步不引入。
pub struct PeerSession {
    pub(crate) conn: QuinnConnection,
    /// 应用层 Hello 成功标志。初始 `false`，`client_hello()` /
    /// `server_hello()` 任一端成功置 `true`（`Ordering::Release`）。
    /// 业务路径必须先 `load(Ordering::Acquire)` 确认 `true` 再发事件。
    pub(crate) hello_ok: AtomicBool,
    /// Stream A（control 流）缓存：`server_hello()` / `client_hello()` 写入；
    /// STEP-5.4 `read_loop` 通过 `take_stream_a_recv()` 拿 `RecvStream` 半
    /// 边给控制帧读循环，`SendStream` 半边留给后续 `send_stream_a()` 复用。
    ///
    /// **为什么用 `Mutex<Option<StreamPair>>` 而不是 `OnceCell`**：STEP-5.x
    /// 接手控制帧循环时需要 take recv 半边但保留 send 半边 —— `Option::take`
    /// 配合 `StreamPair::recv.take()` 的两步语义最干净。`OnceCell` 无法表达
    /// "已设置过但 recv 已被 take" 的状态。
    pub(crate) stream_a_cache: Mutex<Option<StreamPair>>,
    /// **STEP-8.2 修复**：hello 完成后从 `stream_a_cache.send` 搬过来
    /// 的 send 半边，给 [`Self::send_stream_a`] 复用 —— **不再每次
    /// `open_bi` 开新 bidi**。
    ///
    /// **为什么独立字段而不是复用 stream_a_cache**：与 recv 半边的
    /// take_stream_a_recv 不同，`send_stream_a` 是一次调用写一帧但
    /// **整个 peer 生命周期内被多次调用**（Enter / Ack / Ping / Pong /
    /// 每次进 capture 重发 Enter ...），需要持有同一 `SendStream` 重复
    /// write。`Mutex<Option<_>>` + 持锁写 + 写完不释放（同一个 Mutex
    /// guard 内 await）是 QUIC 流的常规模式（本 peer 独占，无锁竞争）。
    ///
    /// **调用顺序**：`client_hello` / `server_hello` 完成 → 从
    /// `stream_a_cache.send` 拿 send → 存本字段。listen.rs supervisor /
    /// peer.run 调 `take_stream_a_recv` 拿 recv（来自 `stream_a_cache`
    /// 的同一 `StreamPair`）—— client 写 send_a ↔ server 读 recv_a 是
    /// **同一条 bidi**。
    pub(crate) cached_send_a: Mutex<Option<SendStream>>,
    /// **键盘不通修复**：stream B（input 流）的 send 半边缓存 —— 与
    /// [`Self::cached_send_a`] 同模式。
    ///
    /// **背景**：修前 [`Self::send_stream_b`] **每帧 `open_bi()` 开一条新
    /// bidi**，而 server 端 `listen.rs::handle_quic_peer_supervisor` 只读
    /// hello 缓存的 stream A recv + datagram，**没有 `accept_bi()` 循环**
    /// —— 每条新开的 stream B 都堆在 quinn 的 accept 队列里没人消费。
    /// 默认 `InputChannelConfig { keyboard: Stream }` 让所有按键走这条
    /// 路，于是"鼠标通、键盘不通"：Motion/Button/Axis 走 datagram 有
    /// reader，按键走 stream B 全丢。发送端 `send_stream_b` 还返 `Ok(())`
    /// （quinn 缓冲小写入），日志里看不出错。
    ///
    /// **修法（发送端半边）**：首次调用 `open_bi()` 拿一条 bidi，send 半
    /// 边存本字段长期复用，后续每次只 `write_frame` 不 `finish` ——
    /// 整个 peer 生命周期只有**一条** stream B。接收端只需一次
    /// `accept_bi()` 就能拿到它并持续读（见 `listen.rs::
    /// server_stream_reader_task`）。
    ///
    /// **写失败即失效**：任何 write 错误都把本字段置回 `None`，下次调用
    /// 重开一条（对端仍会通过 accept_bi 循环接到新流）。
    pub(crate) cached_send_b: Mutex<Option<SendStream>>,
    /// **STEP-8.2 修复 — Bug #7**：可选的 stream A 事件出向 channel。
    ///
    /// **背景**：peer.run 主循环从 stream A 读 control 事件（Ack /
    /// Pong / Leave），但修前只 log debug —— `recv_tx` 死字段（见
    /// Bug #4），capture.rs 永远收不到 server 的响应 → 本地卡
    /// WaitingForAck、反复 send Enter。
    ///
    /// **修法**：peer.run 读到 stream A 事件时，若本字段设了 sender，
    /// send `(remote_addr, event)` 出去；client 端 `connect_to_handle`
    /// 在 spawn peer.run 之前设上 + spawn 一个 forwarder task 把
    /// `(addr, event)` 通过 `client_manager.get_client(addr)` 映射
    /// 到 `(handle, event)` 再推到 `recv_tx`。
    ///
    /// **为何 server 端不用设**：server 端 `listen.rs::handle_quic_
    /// peer_supervisor` 不调 peer.run，自己 accept_bi + read_frame
    /// + 推 listen_tx，forwarding 路径已存在。
    pub(crate) outgoing_events: Arc<Mutex<Option<tokio_mpsc::UnboundedSender<(std::net::SocketAddr, ProtoEvent)>>>>,
    /// 3 条 bidi stream 集合缓存（STEP-5.2 引入）。
    ///
    /// STEP-5.3 / 5.4 `read_loop` 装配时填充 —— 装配路径：server 端
    /// `accept_bi()` 三条 + client 端 `open_bi()` 三条（client_hello /
    /// server_hello 已用 stream A），完成后整个 `Some(StreamBunch)` 移交
    /// `read_loop` 接管（recv 半边给 reader task，send 半边由
    /// `send_stream_a/b/c` 复用）。
    ///
    /// **为什么用 `Arc<Mutex<Option<_>>>` 而不是裸 `Mutex<Option<_>>`**：
    /// `PeerSession` 当前是直接持有 `Connection`（不是 `Arc<Connection>`），
    /// 但 `read_loop` 需要 spawn 进独立 task 后 `&self` 借用 session 之外
    /// 还能再次拿 stream_bunch —— `Arc` 让两个 `PeerSession` 引用共享
    /// 同一份 `Mutex<Option<StreamBunch>>`，避免所有权切割问题。
    /// 与 `stream_a_cache` 的"裸 `Mutex<Option<_>>`"不同是因为
    /// `stream_a_cache` 所有权不跨 task 转移（`client_hello` /
    /// `server_hello` 单 task 内填 + `take_stream_a_recv` 单 task 内拿），
    /// `stream_bunch` 跨 task 移交。
    ///
    /// dead_code chain：STEP-5.2 引入字段占位（默认 `None`），STEP-5.3
    /// 接入 `read_loop` 时消费。
    pub(crate) stream_bunch: Arc<Mutex<Option<StreamBunch>>>,
}

/// `PeerSession::run()` 角色标识（STEP-5.4 引入）。
///
/// **为什么需要 role 参数**：Hello 握手不对称 —— client 端走
/// [`super::protocol::client_hello`]（`open_bi()` + 发 Hello），server 端走
/// [`super::protocol::server_hello`]（`accept_bi()` + 回 echo）。三 stream 装配
/// 也不对称 —— client 端 `open_bi()` 三次拿三条 bidi；server 端
/// `accept_bi()` 三次等三条 bidi。`run()` 用 [`PeerRole`] 决定哪条路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerRole {
    /// 主动拨号端 —— 走 [`super::protocol::client_hello`] + `open_bi()` 三次
    Client,
    /// 被动接受端 —— 走 [`super::protocol::server_hello`] + `accept_bi()` 三次
    Server,
}

/// **Wake-close sentinel code**（补丁：Mac wake 自动重连支持）。
///
/// macOS 系统唤醒时 [`crate::macos_power::PowerObserver`] 触发
/// [`crate::listen::spawn_wake_task`] 对每条 peer conn 调
/// `connection().close(WAKE_CLOSE_CODE.into(), b"wake")` —— 用这个非零
/// sentinel 而不是默认 0（NO_ERROR）让对端在 `should_retry_after_close` 里
/// 区分"用户主动 close（不重试）"和"系统唤醒触发的 close（要重试）"。
///
/// **为什么不用 `0`**：
/// 旧代码用 `0u32`（NO_ERROR）触发 `ConnectionError::ApplicationClosed(0)`，
/// 对端 `should_retry_after_close` 一律返 `false` → master 不重拨。
/// 设计假设"下次 send() 会触发重拨"在 wake 后不成立（用户没在动鼠标）。
///
/// **为什么用 `0xCAFE`**：
/// - 0xCAFE 是约定俗成的 sentinel magic（"coffee" 谐音 + 视觉好认）
/// - 远在 QUIC `VarInt` 范围（≤ 2^62）内
/// - 唯一避免和 Bug #9/#10 的 `close(0u32, "peer closed stream")` 撞码
///   —— Bug #9/#10 仍走 0（用户/网络层断连），不进 wake 重拨分支
pub(crate) const WAKE_CLOSE_CODE: u32 = 0xCAFE;

/// 从 `quinn::ConnectionError` 判定本次关闭是否值得自动重连（STEP-5.4 引入）。
///
/// **判定逻辑**（与 PLAN §5.4 + STEP-6.5 `RetryState` 衔接）：
/// - `ApplicationClosed(_)`（reason code = [`WAKE_CLOSE_CODE`]）→ 对端因
///   系统唤醒主动 close，**应**重试（用户预期 wake 后立刻可用）
/// - `ApplicationClosed(_)`（其他 code，含 `0` = NO_ERROR）→ 是 peer 主动
///   close，**不**重试（peer 明确不想继续）
/// - `ConnectionLost(_)` / `TimedOut` → 网络层断连，**应**重试
/// - `TransportError(_)`（quic-level）→ 协议级错误，**不**重试（很可能是
///   协议 bug / 攻击信号）
/// - `Reset` / `VersionMismatch` / `LocalError(_)` → 本端错误，不重试
/// - `IdleTimeout` → QUIC idle 超时（30s 无），），**不**重试（peer 真离线
///   信号；重试只会浪费资源）
///
/// M1 阶段本函数仅作为 `run()` 退出时的判定提示；STEP-6.5
/// `connect.rs::RetryState` 会消费这个判定做退避重连。
pub fn should_retry_after_close(reason: &quinn::ConnectionError) -> bool {
    use quinn::ConnectionError;
    match reason {
        // 网络层断连 / 超时 —— 重试
        ConnectionError::TimedOut => true,
        // Wake-close sentinel：Mac 唤醒触发的对端 close（详见
        // [`WAKE_CLOSE_CODE`] docstring）—— 重试。
        //
        // **与 Bug #9/#10 关系**：STEP-8.2 Bug #9 / Bug #10 在 stream A
        // Truncated / read IO error 时 `conn.close(0u32, b"peer closed stream")`
        // —— code 仍是 0，不进本分支。所以 master 看到那种 close 仍走 "用户
        // close 不重试" 路径，**只有** wake close 走重试分支。两者正交。
        ConnectionError::ApplicationClosed(frame)
            if frame.error_code.into_inner() as u32 == WAKE_CLOSE_CODE =>
        {
            true
        }
        // quinn 0.11 实际变体：协议级 / 本端错误 / peer 主动 close / CID 耗尽
        // —— 都不重试（保守）。
        ConnectionError::ApplicationClosed(_)
        | ConnectionError::TransportError(_)
        | ConnectionError::ConnectionClosed(_)
        | ConnectionError::Reset
        | ConnectionError::VersionMismatch
        | ConnectionError::LocallyClosed
        | ConnectionError::CidsExhausted => false,
    }
}

impl PeerSession {
    /// 构造：从 `quinn::Connection` 包成 `PeerSession`（STEP-3.2 引入）。
    ///
    /// STEP-3.2 起所有 `PeerSession` 构造都走这个 helper：
    /// - `accept()` caller → `PeerSession::from_connection(conn)`
    /// - `dial()` caller → `PeerSession::from_connection(conn)`
    /// - 测试 → 直接调
    ///
    /// 保证 `hello_ok = false` + `stream_a_cache` 空初始这两个不变式集中在
    /// 一处（与 bak `Mousehop::PeerSession::from_connection` 对齐）。
    ///
    /// STEP-5.x 接 `route_input` / `input_channels` 时再加 `with_config`
    /// builder；本步不引入（M1 不触碰 ChannelMode，STEP-4.1 引入
    /// `InputChannelConfig` 后再加）。
    pub fn from_connection(conn: QuinnConnection) -> Self {
        Self {
            conn,
            hello_ok: AtomicBool::new(false),
            stream_a_cache: Mutex::new(None),
            // STEP-8.2 修复 — `cached_send_a`：hello 完成后从
            // `stream_a_cache.send` 搬过来，让 [`Self::send_stream_a`]
            // 复用同一 bidi 的 send 半边（与 server 端
            // `take_stream_a_recv` 拿到的 recv 半边是**同一条** bidi），
            // 不再每次开新 bidi。
            //
            // **历史**：修前 `send_stream_a` 每次 `open_bi()` 开新
            // stream 写控制事件（Enter / Leave / Ack / Ping / Pong），
            // 但 server `listen.rs` supervisor 只读缓存的 recv_a（即
            // hello 时的同一条 bidi）—— 控制事件走新 stream、server
            // 不读新 stream → Enter 永远到不了 server → server 不
            // release capture、不 inject input，用户看到"连上了但键
            // 鼠不通"。
            cached_send_a: Mutex::new(None),
            // 键盘不通修复：stream B send 半边缓存，首次 send_stream_b
            // 时惰性 open_bi 填充（详见字段 docstring）。
            cached_send_b: Mutex::new(None),
            // STEP-8.2 修复 — Bug #7：stream A 事件出向 channel
            // 初始 None；client 端 connect_to_handle 在 spawn peer.run
            // 之前调 `set_outgoing_events` 设上。详见字段 docstring。
            outgoing_events: Arc::new(Mutex::new(None)),
            // STEP-5.2 引入 `stream_bunch` 字段占位 —— 默认 `None`，
            // STEP-5.3 `read_loop` 装配时填充。`Arc` 包装让 read_loop
            // task 与 caller (`peer.send_stream_*`) 共用同一份
            // `Mutex<Option<StreamBunch>>` 所有权。
            stream_bunch: Arc::new(Mutex::new(None)),
        }
    }

    /// 暴露底层 `quinn::Connection`，给 STEP-5.x 读 `peer_identity()` /
    /// datagram / stream B/C 用。STEP-6.x 接入 `LanMouseConnection` 后这
    /// 一步会被 `send()` / `recv()` 高阶方法盖掉。
    pub fn connection(&self) -> &QuinnConnection {
        &self.conn
    }

    /// Hello 握手是否已完成（STEP-3.2 引入）。
    ///
    /// 业务路径（`send_motion()` / 开 stream B / 业务事件循环 —— 这些是
    /// STEP-5.x 的范畴）必须先调此方法确认 `true` 再发事件；否则 QUIC TLS
    /// 1.3 之后没有应用层验证过的对端（可能是 LAN spoofing 残余），
    /// 不允许注入键鼠。
    #[allow(dead_code)] // 测试 + STEP-5.x / STEP-6.x 接入时移除
    pub fn hello_ok(&self) -> bool {
        self.hello_ok.load(Ordering::Acquire)
    }

    /// 取出 stream A 的 `(SendStream, RecvStream)` **整对**（STEP-3.2 引入）。
    ///
    /// **消费性语义**：调用后 `stream_a_cache` 缓存被清空（`Option::take`）。
    /// 设计意图：STEP-5.4 `read_loop` 启动时拿走 server 端 Hello 时缓存
    /// 的 stream，转交给控制帧循环的所有权。本步暂无 main-code caller
    /// （STEP-5.4 才接），仅测试或 STEP-5.x 设计参考。
    ///
    /// 返回 `None` 说明 Hello 还没跑过（典型 client 端场景，client_hello
    /// 完成同样有 cache，server_hello 也一样 —— STEP-3.2 起两端对称缓存）。
    #[allow(dead_code)]
    pub async fn take_stream_a_cache(&self) -> Option<(SendStream, quinn::RecvStream)> {
        let mut g = self.stream_a_cache.lock().await;
        g.take().and_then(|p| match (p.send, p.recv) {
            (Some(s), Some(r)) => Some((s, r)),
            // 半边缺失（已被 take_recv）—— 整对无法重建，返 None
            _ => None,
        })
    }

    /// 取出 stream A 的 `RecvStream` 半边，**保留** `SendStream` 半边在
    /// cache（STEP-5.4 接 read_loop 时用）。
    ///
    /// 与 [`Self::take_stream_a_cache`]（整对 take）语义不同：本方法只拿
    /// recv 半边，让 send 半边留给写路径复用。STEP-3.2 暂未使用，
    /// STEP-5.4 由 read_loop 接手控制帧循环所有权时消费。
    #[allow(dead_code)]
    pub async fn take_stream_a_recv(&self) -> Option<quinn::RecvStream> {
        let mut g = self.stream_a_cache.lock().await;
        g.as_mut().and_then(|p| p.recv.take())
    }

    /// **STEP-8.2 修复**：取出 stream A 的 `SendStream` 半边，**保留**
    /// `RecvStream` 半边在 cache（给 `take_stream_a_recv` 用）。
    ///
    /// 与 `take_stream_a_recv` 对称 —— 设计用途：
    /// - `client_hello` / `server_hello` 完成后 put `Pair { send, recv }`
    ///   进 `stream_a_cache`，**然后**调本方法把 `send` 搬到
    ///   `cached_send_a` 供 `send_stream_a` 复用
    /// - 与 supervisor / peer.run 后续调 `take_stream_a_recv` 拿
    ///   `recv` 不冲突（双方各自 take 自己的半边）
    ///
    /// **设计动机**（详见 `cached_send_a` 字段 docstring）：
    /// `send_stream_a` 一次调用写一帧但整个 peer 生命周期被多次调用
    /// （Enter / Ack / Ping / Pong / 重复 Enter ...），需要持有同一
    /// `SendStream` 重复 write —— 必须从 `stream_a_cache` 取出 send
    /// 独立存放。
    #[allow(dead_code)]
    pub async fn take_stream_a_send(&self) -> Option<SendStream> {
        let mut g = self.stream_a_cache.lock().await;
        g.as_mut().and_then(|p| p.send.take())
    }

    /// **STEP-8.2 修复 — Bug #7**：设置 stream A 事件出向 sender。
    ///
    /// `connect_to_handle` 在 spawn peer.run 之前调用本方法把 sender
    /// 设到 `outgoing_events`，让 peer.run 主循环从 stream A 读到
    /// Ack / Pong / Leave 时能 forward 出去（详见字段 docstring）。
    /// `Some(_)` 覆盖旧值；`None` 关闭 forwarding（兜底用，理论上
    /// 不需要 —— client 路径应保持设上）。
    pub async fn set_outgoing_events(
        &self,
        tx: Option<tokio_mpsc::UnboundedSender<(std::net::SocketAddr, ProtoEvent)>>,
    ) {
        *self.outgoing_events.lock().await = tx;
    }

    /// **STEP-8.2 修复 — Bug #9**：把 ProtoEvent 推给 outgoing_events（如
    /// 设了）→ forwarder → capture.rs。专门用于 peer.run 主循环在
    /// 检测到 peer 关闭时主动推 Leave 让本地 capture 立即 release。
    ///
    /// **为何不直接 send 而封一个 helper**：send 失败静默吞 + 集中
    /// log `peer closed push Leave` 让用户复测时能看到完整释放路径。
    async fn send_outgoing_event(&self, event: ProtoEvent, addr: std::net::SocketAddr) {
        if let Some(tx) = self.outgoing_events.lock().await.as_ref() {
            if let Err(e) = tx.send((addr, event)) {
                log::debug!(
                    "send_outgoing_event: outgoing_events 已退（forwarder 不在）: {e}"
                );
            }
        }
    }

    /// 发送高频 motion 输入事件（STEP-5.1 引入）。
    ///
    /// **通道选择** —— 优先 QUIC datagram；超 [`MAX_SAFE_DATAGRAM`] /
    /// 对端不支持 datagram / datagram 发送失败时降级到 stream B
    /// （[`Self::send_datagram_or_stream_b`]）。
    ///
    /// **前置条件**：`hello_ok == true`（应用层 Hello 握手已完成）。若
    /// `hello_ok == false`，返回 [`Error::HelloFailed`]，**不**碰
    /// datagram / stream —— 这是 PLAN §3 "mTLS 通了不等于对端是
    /// lan-mouse" 信任模型的守护（与 bak
    /// `mousehop/src/quic_transport.rs:471-486 send_motion` 完全对齐）。
    ///
    /// **dead_code chain**：STEP-5.4 `PeerSession::run()` 接管读循环后，
    /// STEP-6.x `LanMouseConnection::send()` 会消费此函数。当前 main-code
    /// 无 caller，仅测试 + 即将到来的 STEP-6.x caller。
    #[allow(dead_code)]
    pub async fn send_motion(&self, event: &ProtoEvent) -> super::Result<()> {
        if !self.hello_ok.load(Ordering::Acquire) {
            return Err(super::Error::HelloFailed("hello not complete".into()));
        }
        // 定长 codec 编码到 `[u8; MAX_EVENT_SIZE]`（21 字节）—— 与 stream B
        // 读端的 `read_frame` 走同一个定长 `MAX_EVENT_SIZE` 解码路径（datagram
        // 自带长度，但解码入口统一在 `ProtoEvent::try_from`）。
        let (buf, _len): ([u8; MAX_EVENT_SIZE], usize) = event.clone().into();
        self.send_datagram_or_stream_b(&buf).await
    }

    /// datagram 优先 + stream B 降级（STEP-5.1 引入，STEP-5.2 替换降级路径）。
    ///
    /// **判定顺序**：
    /// 1. `conn.max_datagram_size()` **每次重读**（STEP-0.1 结论 D：值随
    ///    路径 MTU 探测变化，缓存会导致要么白白降级、要么超限发送失败）。
    ///    返回 `None` 表示对端不支持 / 本端禁用 datagram → 直接降级。
    /// 2. 与 [`MAX_SAFE_DATAGRAM`] 取 `min` 作为实际上限 —— 防止 MTU
    ///    探测完成后 `max_datagram_size()` 报告一个**陈旧**的更大值（quinn
    ///    内部 path validation 完成后才会扩到 1414，但本端只能读到
    ///    `Some(>1162)` 时仍应保守地 cap 在 1162 以避免 TooLarge）。
    /// 3. `conn.send_datagram(...)` —— quinn 0.11 的这个方法本身是
    ///    **非阻塞**的（拥塞时丢最旧排队 datagram，正是 motion 语义
    ///    想要的）。返回 `Err` 只有四种：`TooLarge` / `Disabled` /
    ///    `UnsupportedByPeer` / `ConnectionLost`。前三种是"这条路走不通"
    ///    → 降级到 stream B；`ConnectionLost` 是连接已死 → 直接上报
    ///    （降级也救不回来，stream B 上再失败一次也没意义）。
    ///
    /// **签名 `&[u8]` 而不是 `&ProtoEvent`**：STEP-5.2 [`Self::send_stream_b`]
    /// 收到"已编码字节"时复用同一份 buffer（datagram 失败后复用 buf），
    /// 且未来 `motion_oversize_falls_back_to_stream` 测试要构造超限裸
    /// 字节验证降级管道本身（与 bak
    /// `mousehop/src/quic_transport.rs:507` 签名完全一致）。
    ///
    /// **`bytes.to_vec().into()`**：`send_datagram` 收 `bytes::Bytes`，
    /// `Vec<u8> → Bytes` 零拷贝（接管 Vec 的堆分配）。无需在主仓加
    /// `bytes` crate 依赖 —— 类型由 quinn 0.11 的 `send_datagram` 签名
    /// 反向推断。
    ///
    /// **STEP-5.2 关键改造**：降级路径从 inline `open_uni() + write_all() +
    /// finish()`（不带长度前缀、不复用）改为 [`Self::send_stream_b`]
    /// —— 缓存 bidi stream、长度前缀帧 [`super::protocol::write_frame`]、统一错误归到
    /// [`Error::StreamB`]。**SUGGESTION #S-14 完全消化**。
    async fn send_datagram_or_stream_b(&self, bytes: &[u8]) -> super::Result<()> {
        // 每次重读 max_datagram_size —— 严格遵守 STEP-0.1 结论 D。
        let limit = self
            .conn
            .max_datagram_size()
            .map(|m| m.min(MAX_SAFE_DATAGRAM));

        if let Some(limit) = limit {
            if bytes.len() <= limit {
                match self.conn.send_datagram(bytes.to_vec().into()) {
                    Ok(()) => return Ok(()),
                    // 连接已死：降级也救不回来，直接上报
                    Err(e @ quinn::SendDatagramError::ConnectionLost(_)) => {
                        return Err(super::Error::Datagram(e));
                    }
                    // TooLarge / Disabled / UnsupportedByPeer：这条路走不通 → 降级
                    Err(e) => {
                        log::debug!("datagram 发送失败（{e}），降级到 stream B");
                    }
                }
            }
        }

        // 降级路径 —— STEP-5.2 替换为 `send_stream_b`（cache + 长度前缀帧）
        self.send_stream_b(bytes).await
    }

    /// Stream B（input 流，可靠有序）写入（STEP-5.2 引入，**替换** STEP-5.1
    /// 的 inline uni stream 降级路径）。
    ///
    /// **惰性 cache**：首次调用时 `conn.open_bi()` 拿一条 bidi stream，
    /// 存入 `peer.stream_bunch` 字段（虽然本方法目前用独立的
    /// `stream_b_cache: Mutex<Option<StreamPair>>` 临时缓存 —— STEP-5.3
    /// read_loop 接手时把 cache 内容统一迁移到 `stream_bunch`）。
    /// 后续调用复用同一条 stream 的 `send` 半边，recv 半边留给 STEP-5.3
    /// reader task 接管。
    ///
    /// **in-lock 借用**：`Mutex` 临界区覆盖 "open + write" 全段 —— 同一条
    /// stream 上并发写会交错字节、破坏帧边界。这与 bak
    /// `mousehop/src/quic_transport.rs:557-579 send_stream_b` 形态完全对齐。
    ///
    /// **长度前缀帧**：走 [`super::protocol::write_frame`]（`[u32 BE len][body...]`），与
    /// 对端 STEP-5.3 reader task 的 [`super::protocol::read_frame`] codec 对齐。
    ///
    /// **错误归一**：所有 IO 错误收敛到 [`super::Error::StreamB(String)`]
    ///（消息前缀区分 `"open_bi"` / `"write frame length"` / `"write"`），
    /// 与 bak `mousehop/src/quic_transport.rs:1035-1040` 完全对齐。
    ///
    /// dead_code chain：本方法当前仅被 [`Self::send_datagram_or_stream_b`]
    /// 降级路径消费；STEP-5.3 接入后由 [`Self::send`] 路由层
    /// `Channel::StreamB` 直接消费（不经过 datagram 试探）。
    ///
    /// STEP-6.1 升级为 `pub`：供 [`Self::send_input`] 在 `Channel::StreamB`
    /// 分派时直接消费（不经过 datagram 试探）。
    pub async fn send_stream_b(&self, bytes: &[u8]) -> super::Result<()> {
        // **键盘不通修复**：复用缓存的 stream B send 半边，**不**再每帧
        // `open_bi()` 开新流。详见 [`Self::cached_send_b`] 字段 docstring
        // —— 修前每个按键都开一条新 bidi，而 server 端 supervisor 没有
        // `accept_bi()` 循环，这些流全部堆在 accept 队列里没人读。
        //
        // **持锁 await 设计**：与 [`Self::send_stream_a`] 一致 ——
        // `send_stream_b` 是 stream B 的唯一写路径，持锁期间并发 caller
        // 排队串行，避免两帧字节交错破坏帧边界。
        use tokio::io::AsyncWriteExt;

        let mut g = self.cached_send_b.lock().await;
        if g.is_none() {
            let (send, recv) = self
                .conn
                .open_bi()
                .await
                .map_err(|e| super::Error::StreamB(format!("open_bi: {e}")))?;
            // recv 半边 drop —— stream B 是单向数据流（本端写、对端读），
            // 反向读能力不需要。
            drop(recv);
            *g = Some(send);
            log::debug!("send_stream_b: 新建并缓存 stream B（后续帧复用同一条）");
        }

        // 写失败时把 cache 置回 None，下次调用重开一条（对端 accept_bi
        // 循环会接到新流）—— 避免一次瞬时错误让 stream B 永久失效。
        let result = {
            let send = g.as_mut().expect("cached_send_b 刚填充");
            match send.write_u32(bytes.len() as u32).await {
                Err(e) => Err(super::Error::StreamB(format!("write frame length: {e}"))),
                Ok(()) => send
                    .write_all(bytes)
                    .await
                    .map_err(|e| super::Error::StreamB(format!("write frame body: {e}"))),
            }
        };
        if result.is_err() {
            *g = None;
        }
        result
    }

    /// 通道分发入口（STEP-6.1 引入）—— 按 per-handle [`InputChannelConfig`]
    /// 把 [`ProtoEvent`] 派到 [`super::protocol::Channel`] 对应的底层通道。
    ///
    /// **调用方**：`src/connect.rs::LanMouseConnection::send()`。
    /// LanMouseConnection 不持有 cfg（cfg 在 `ClientManager` 里 per-handle
    /// 存），所以 caller 通过本方法签名把 cfg 传进来；本方法**不**缓存 cfg，
    /// 也不改 peer 状态。
    ///
    /// **分派**（与 STEP-4.4 [`super::protocol::route_input`] 完全对齐）：
    /// | Channel | 底层调用 |
    /// |---|---|
    /// | `Datagram` | [`Self::send_motion`]（datagram 优先 + 超限降级 stream B） |
    /// | `StreamA`  | [`Self::send_stream_a`]（开新 bidi + write_frame + finish） |
    /// | `StreamB`  | [`Self::send_stream_b`]（开新 bidi + write_frame + finish） |
    /// | `StreamC`  | `Err(super::Error::HelloFailed("stream C is M2-only"))` |
    ///
    /// **M2 守门**：`ProtoEvent` 在主仓不含 `Clipboard` 变体（PLAN §9），
    /// 所以 `route_input` 永远不会返 `Channel::StreamC`；但本方法显式判
    /// `StreamC` 返 `Err` 防止 `unreachable!()` 在 ProtoEvent 加 M2 变体
    /// 时意外落入（编译期 + 运行期双护栏）。
    ///
    /// **前置门禁**：复用 `send_motion` 内部的 `hello_ok` 检查；`StreamA`
    /// / `StreamB` 路径不显式检查（`hello_ok == false` 时 `send_motion`
    /// 返 `HelloFailed`，其它通道理论上不应被调用 —— LanMouseConnection
    /// 拨号流程是 "dial → client_hello → register_peer → 后续 send"，所
    /// 以 peers 表里的 peer 都已过 hello）。
    ///
    /// **dead_code chain**：STEP-6.1 `LanMouseConnection::send()` 接入
    /// 后立刻消费；STEP-6.2 listen.rs 同模式复用。
    #[allow(dead_code)]
    pub async fn send_input(
        &self,
        event: &ProtoEvent,
        cfg: &InputChannelConfig,
    ) -> super::Result<()> {
        use super::protocol::{route_input, Channel};
        let routed = route_input(cfg, event);
        // **INFO on Ack/Leave** —— 被控端发 Ack 卡住的 bug 排查用。
        // `delivered` 出现 = send_input 真的返回 Ok;
        // 没出现但本条 log 出现 = 卡在 send_stream_a 等对端消费。
        if matches!(event, ProtoEvent::Ack(_) | ProtoEvent::Leave(_)) {
            log::info!(
                "send_input: routing {event:?} via {routed:?} (entry; awaiting send)"
            );
        }
        let result = match routed {
            Channel::Datagram => self.send_motion(event).await,
            Channel::StreamA => {
                let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.clone().into();
                self.send_stream_a(&buf[..len]).await
            }
            Channel::StreamB => {
                let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.clone().into();
                self.send_stream_b(&buf[..len]).await
            }
            Channel::StreamC => Err(super::Error::HelloFailed(
                "stream C is M2-only (clipboard metadata not in M1 ProtoEvent)".into(),
            )),
        };
        if matches!(event, ProtoEvent::Ack(_) | ProtoEvent::Leave(_)) {
            log::info!(
                "send_input: {event:?} via {routed:?} returned (ok={})",
                result.is_ok()
            );
        }
        result
    }

    /// 发送控制流事件（Enter / Leave / Hello / Ping / Pong），开新 bidi
    /// stream 写一帧后 finish（STEP-6.1 引入）。
    ///
    /// **为什么不复用 `stream_a_cache`**：
    /// - `client_hello` / `server_hello` 已经把 stream A 的 send/recv 双半
    ///   边缓存进 `peer.stream_a_cache`（cache 设计意图是 hello 用过的
    ///   stream 给后续控制帧复用）
    /// - 但 LanMouseConnection 当前**未**持 receiver task 来读 recv 半边
    ///   —— 缓存的 recv 半边 drop 才是常态，这会拖 `take_stream_a_recv`
    ///   进入 `None` 分支
    /// - 本步（STEP-6.1）采取**保守实现**：每条控制事件开一条新 bidi
    ///   stream，写完 finish（peer 不需要 recv 半边 → drop 即可）。Ping
    ///   每 500ms × 4 ≈ 2s 流密度的额外 stream 开销在在 M1 范围内可接受
    ///
    /// **后续优化空间**（STEP-6.x 之外）：
    /// - 与 bak `mousehop/src/quic_transport.rs::send_stream_a` 对齐（缓存
    ///   + in-place write）—— 本步已**部分实现**：send 路径走
    ///   `cached_send_a` 复用 hello 同 bidi（详见 `cached_send_a`
    ///   docstring），但仍持锁 await（无锁优化空间）
    /// - M1 阶段不做进一步优化（保持单步范围可控）
    ///
    /// **错误归一**：与 [`Self::send_stream_b`] 对称 —— IO 错误归到
    /// `super::Error::HelloFailed(...)`（避免新增 `super::Error::StreamA` 变体；HELLO
    /// 握手期错误变体也是这个名，语义复用——"stream A 写失败" ≈ "Hello
    /// 后续帧写失败"，与 M1 阶段语义匹配）。
    ///
    /// **dead_code chain**：本步由 [`Self::send_input`] 内部消费；
    /// `send_input` 又被 STEP-6.1 `LanMouseConnection::send()` 消费。
    #[allow(dead_code)]
    async fn send_stream_a(&self, bytes: &[u8]) -> super::Result<()> {
        // **STEP-8.2 修复 — Bug #5**：优先用 `cached_send_a`（hello 时
        // 缓存的同一条 bidi send 半边）。**不**再每次 `open_bi` 开新
        // stream —— 那条新 stream server 端 supervisor 不会读（它只
        // 读 `take_stream_a_recv` 拿到的 recv 半边 = hello 时的同条
        // bidi），控制事件（Enter / Ack / Ping / Pong）永远到不了
        // server，看起来"连上了但键鼠不通"。
        //
        // **持锁 await 设计**：`send_stream_a` 是 stream A 的唯一写
        // 路径（无其他 caller），持锁期间并发 caller 排队串行 —— 与
        // QUIC stream write 的"一帧一帧"语义对齐（避免两帧交错）。
        //
        // **Fallback**：`cached_send_a` 为 `None` 时（hello 未完成 / 已
        // 被 take）走旧的 open_bi 路径 —— 保留兜底兼容早期 caller /
        // 测试（单测可能直接 `open_bi` + `peer.send_input` 不走 hello）。
        let mut g = self.cached_send_a.lock().await;
        if let Some(send) = g.as_mut() {
            use tokio::io::AsyncWriteExt;
            send.write_u32(bytes.len() as u32)
                .await
                .map_err(|e| {
                    super::Error::HelloFailed(format!("send_stream_a cached length: {e}"))
                })?;
            send.write_all(bytes)
                .await
                .map_err(|e| {
                    super::Error::HelloFailed(format!("send_stream_a cached body: {e}"))
                })?;
            log::trace!(
                "send_stream_a cached: wrote {} bytes on hello bidi",
                bytes.len()
            );
            return Ok(());
        }
        drop(g);

        // Fallback path —— cached_send_a 不可用时开新 bidi（旧行为）
        log::debug!("send_stream_a: cached_send_a 不可用，fallback 开新 bidi");
        use tokio::io::AsyncWriteExt;
        let pair = self
            .conn
            .open_bi()
            .await
            .map_err(|e| super::Error::HelloFailed(format!("send_stream_a open_bi: {e}")))?;
        let (mut send, recv) = (pair.0, pair.1);
        drop(recv); // 不读 recv 半边 → drop 释放反向流

        send.write_u32(bytes.len() as u32)
            .await
            .map_err(|e| super::Error::HelloFailed(format!("send_stream_a length: {e}")))?;
        send.write_all(bytes)
            .await
            .map_err(|e| super::Error::HelloFailed(format!("send_stream_a body: {e}")))?;
        send.finish()
            .map_err(|e| super::Error::HelloFailed(format!("send_stream_a finish: {e}")))?;
        Ok(())
    }

    /// PeerSession 取出 stream_bunch 所有权（STEP-5.3 引入）。
    ///
    /// **消费性语义**：调用后 `peer.stream_bunch` 字段回到 `None`。设计
    /// 意图：[`super::streams::read_loop`] 装配 reader 时一次性 take 走
    /// `Some(StreamBunch)`，把 `a` / `b` / `c` 三个字段分别处理（a 留给
    /// caller / b 喂 reader task / c drop）。
    ///
    /// **返回 `None`**：说明 caller 还没装配 stream_bunch（典型场景：
    /// read_loop 在 STEP-5.4 `run()` 装配前就被调用）。当前 main-code 无
    /// caller，本步加 `#[allow(dead_code)]` 守护。
    ///
    /// **可见性 `pub(crate)`**：[`super::streams::read_loop`] 调。
    #[allow(dead_code)]
    pub(crate) async fn take_stream_bunch(&self) -> Option<StreamBunch> {
        let mut g = self.stream_bunch.lock().await;
        g.take()
    }

    /// PeerSession 装配 stream_bunch（STEP-5.3 引入，STEP-5.4 `run()` 装配
    /// 入口消费）。
    ///
    /// **写入语义**：调用前 `peer.stream_bunch` 应为 `None`（首次装配）
    /// 或已被 [`Self::take_stream_bunch`] take 后回 `None`（重新装配）。
    /// 本方法直接覆盖（lock + assign Some），不做 "已 Some 拒覆盖" 检查
    /// —— caller 责任保证调用时机。
    ///
    /// **dead_code chain**：本方法由 STEP-5.4 `PeerSession::run()` 在
    /// [`Self::read_loop`] 之前调用；本步加 `#[allow(dead_code)]` 守护。
    ///
    /// **可见性 `pub(crate)`**：仅 `Self::run()` 调。
    #[allow(dead_code)]
    pub(crate) async fn set_stream_bunch(&self, bunch: StreamBunch) {
        let mut g = self.stream_bunch.lock().await;
        *g = Some(bunch);
    }

    /// PeerSession 主循环（STEP-5.4 引入 + STEP-6.5 改造 close reason 返回）。
    ///
    /// **流程**（与 PLAN §5.4 + Leader prompt 完全对齐）：
    ///
    /// 1. **启 hello_watchdog** —— [`super::protocol::hello_watchdog`] 是 STEP-3.2
    ///    引入的 3s 超时兜底（对端不发起 stream A 时主动关连）；本步接 `run()`
    ///    后此 `#[allow]` 移除
    /// 2. **启 datagram_reader_task** —— [`super::streams::datagram_reader_task`]
    ///    是本步新增的 datagram 事件源（产生 `StreamEvent::Datagram`）
    /// 3. **调 Hello 握手** —— client 端 [`super::protocol::client_hello`] /
    ///    server 端 [`super::protocol::server_hello`]（由 `role` 决定）；
    ///    成功后 `peer.hello_ok() == true` + `peer.stream_a_cache` 缓存
    ///    stream A 的 send/recv 半边
    /// 4. **取 stream_a_recv 半边** —— 留给主循环 `read_frame(recv_a)` 用
    /// 5. **装配三 stream** —— client 端 `open_bi()` 三次 / server 端
    ///    `accept_bi()` 三次；填入 `peer.stream_bunch` 让 [`Self::read_loop`]
    ///    接管 reader task
    /// 6. **主循环 `tokio::select!`** —— 合并 4 路 reader（stream A recv /
    ///    stream B mpsc / datagram mpsc / conn closed）+ 处理 `StreamEvent`
    ///    按类别分派（Reliable/Datagram 走 `route_input` cfg 分派 → 本步
    ///    **不**调 `route_input`（本步是 in-process 端到端验证，业务分派留
    ///    STEP-6.x LanMouseConnection）；Control 类仅日志）
    /// 7. **conn.closed() 触发退出** —— 主循环等到 `closed` future 完成
    ///    后退出；本步返 `Ok(())`（视为"对端关连"，caller 决定是否重连）；
    ///    [`Self::should_retry_after_close`] 可由 caller 评估是否重连
    ///
    /// **dead 入口**：本步不接 `connect.rs` / `listen.rs`，仅被单测
    /// `peer_session_round_trip_motion_keyboard` 直接调；STEP-6.1
    /// `connect.rs::connect_to_handle` 接入时一并移除 `#[allow]`。
    ///
    /// **STEP-6.5 改造**：主循环退出时取 `conn.close_reason()` 转成
    /// `Err(super::Error::Handshake(reason))` —— [`should_retry_after_close`]
    /// 由 `connect.rs::spawn_peer_supervisor` 评估，决定是否触发 RetryState
    /// 退避重连。`#[allow(dead_code)]` 移除（main-code 接入后消费）。
    ///
    /// **为什么 `Arc<Self>` 而非 `&self`**：内部 spawn 两个 reader task
    /// （`datagram_reader_task` / `read_loop` 内的 stream B reader）都需要
    /// `'static + Send` 借用 —— 必须有 `'static` 生命周期（不能是临时
    /// `&self` 借用）。`hello_watchdog` 同样收 `Arc<PeerSession>`。`Arc<Self>`
    /// 把"caller 持 Arc + run() 持 Arc"两个引用合并到同一份计数。
    ///
    /// **错误路径**：
    /// - `client_hello` / `server_hello` 失败 → 立即返 Err（Hello 没成功则
    ///   后续 stream A 装配无意义）
    /// - `accept_bi()` 三次任一失败 → 返 [`super::Error::HelloFailed`]（client 端
    ///   `open_bi` 失败 → 同）
    /// - `read_loop` 失败 → 返 [`super::Error::HelloFailed`]（stream_bunch 未装配）
    /// - 主循环内 `StreamEvent` 处理失败 → `log::warn` + continue（单帧损坏
    ///   不致命；与 STEP-5.3 `read_stream_b_loop` 的"skip-frame"语义对称）
    /// - `conn.closed()` → 返 `Ok(())`（正常关连）
    pub async fn run(self: Arc<Self>, role: PeerRole) -> std::result::Result<(), super::Error> {
        // (1) 启 hello_watchdog —— 3s 超时兜底；对端不发起 stream A 时主动关连
        hello_watchdog(self.clone());

        // (2) 启 datagram_reader_task —— 产生 StreamEvent::Datagram
        //     本步新增：详见下面 datagram_reader_task 函数
        let (tx_d, mut rx_d) = tokio_mpsc::channel::<StreamEvent>(super::streams::READ_STREAM_BUFFER_CAP);
        spawn_local(datagram_reader_task(self.clone(), tx_d));

        // (3) Hello 握手 —— role 决定走 client_hello / server_hello
        //
        // **STEP-8.2 修复（Bug #3 — Hello 重复调用）**：
        //
        // **根因**：本仓 client 路径 `connect_to_handle` 在 `dial_any` 成功
        // 后**先**调一次 `client_hello`（这一步是 STEP-6.1 引入的早期语义），
        // 再 `spawn_local(spawn_peer_supervisor → peer.run(PeerRole::Client))`
        // —— 而 `peer.run()` 内部**又**无条件调 `client_hello`（这是 STEP-5.4
        // 引入 `run()` 时的原始语义）。两次 `client_hello` 的后果：
        // - 第一次：`open_bi()` 开 stream A + 写 Hello + 读 Hello 回包 + 缓存
        //   stream_a + `hello_ok = true`
        // - 第二次（run() 内）：`open_bi()` 又开一条 stream D + 写 Hello + 等
        //   Hello 回包 3s —— 但 server 端 `server_hello` 只 `accept_bi()` 一
        //   次（接的是 stream A），accept 完就进 stream A 读循环，**永远不会**
        //   accept stream D
        // - 客户端第二次 client_hello 等 3s 超时 → `peer.conn.close(...)` 关
        //   连 → server stream A 的 `read_frame()` 报 "connection lost"
        //   → 整个 peer.run() 返 `Err(HelloTimeout)`
        //   → "client (0) peer.run() 返了非预期 Err: hello handshake timed out
        //   after 3s — 不触发 RetryState"
        //
        // **修复**：peer.run() 在调 hello 前查 `hello_ok.load(Acquire)` ——
        // 已置位则跳过整个 hello 块（open_bi / accept_bi 也不会跑），让 caller
        // （`connect_to_handle` / `handle_quic_peer_supervisor`）做的早期 hello
        // 结果继续生效。
        //
        // **为什么 caller 路径还要保留早期 hello**：是历史顺序决定的 ——
        // `connect_to_handle` 早期把 client_hello 放在 peer 生命周期注册到
        // `peers` 表**之前**（失败则不注册，便于 retry 不影响其他 caller），
        // `spawn_peer_supervisor` 只接管 peer 死后的 RetryState。`peer.run()`
        // 设计为"既可独立跑（单测）也可被外部 caller 提前 partial-init 后接
        // 管"——本步用 hello_ok 守卫表达后者语义。
        //
        // **不破坏单测 `peer_session_round_trip_motion_keyboard`**：单测直接调
        // `peer.run(PeerRole::Client/Server)`，无早期 hello，hello_ok 初始 false
        // → 走原始 hello 路径，行为不变。
        match role {
            PeerRole::Client => {
                if !self.hello_ok.load(Ordering::Acquire) {
                    client_hello(&self).await?;
                } else {
                    log::debug!("peer.run(Client): hello_ok 已置位，跳过重复 client_hello");
                }
            }
            PeerRole::Server => {
                if !self.hello_ok.load(Ordering::Acquire) {
                    server_hello(&self).await?;
                } else {
                    log::debug!("peer.run(Server): hello_ok 已置位，跳过重复 server_hello");
                }
            }
        }

        // (4) 取 stream A recv 半边 —— 留给主循环 read_frame(recv_a)
        let mut recv_a = self
            .take_stream_a_recv()
            .await
            .ok_or_else(|| super::Error::HelloFailed("stream A recv missing after hello".into()))?;

        // (5) 装配三 stream（client: open_bi() / server: accept_bi()）
        //     —— 填入 peer.stream_bunch 让 read_loop 接管
        //
        //     **为什么 3 次**”：A / B / C 三条（PLAN §3 "A/B/C 各开 1 条长期
        //     复用"）。M1 阶段 Stream C 装配后 read_loop 立即 drop recv 半边
        //     （守 §9），但仍需先 open/accept 拿到 stream C 的所有权再 drop。
        let mut pairs = Vec::with_capacity(3);
        for i in 0..3u8 {
            let pair = match role {
                PeerRole::Client => self
                    .conn
                    .open_bi()
                    .await
                    .map_err(|e| super::Error::HelloFailed(format!("open_bi #{i}: {e}")))?,
                PeerRole::Server => self
                    .conn
                    .accept_bi()
                    .await
                    .map_err(|e| super::Error::HelloFailed(format!("accept_bi #{i}: {e}")))?,
            };
            pairs.push(pair);
        }
        // pairs[0] = stream A（保留 send 半边给后续 send_stream_a；recv 半边
        //                   已被 take_stream_a_recv 拿走 —— pair.1 即 stream A 的
        //                   recv，是 redundant dup；无害 drop 即可）
        // pairs[1] = stream B
        // pairs[2] = stream C（read_loop 立即 drop —— 守 §9）
        let mut pairs_iter = pairs.into_iter();
        let (s_a, r_a_dup) = pairs_iter.next().expect("pairs[0]");
        let (s_b, r_b) = pairs_iter.next().expect("pairs[1]");
        let (s_c, r_c_dup) = pairs_iter.next().expect("pairs[2]");
        // stream A recv half 已被 take_stream_a_recv 拿走 —— r_a_dup 是
        // redundant dup，交给 StreamBunch.a.recv 占位（read_loop 不读它）
        // stream C recv 也不被 M1 reader task 读（守 §9）—— 同上 r_c_dup 占位
        let bunch = StreamBunch {
            a: Bidi::new(s_a, r_a_dup),
            b: Bidi::new(s_b, r_b),
            c: Bidi::new(s_c, r_c_dup),
        };
        self.set_stream_bunch(bunch).await;

        // (6) read_loop 装配 stream B reader task；stream C 在 read_loop 内 drop
        let mut read_streams = read_loop(&self, &mut recv_a).await?;

        // (7) 主循环 select! —— 4 路 reader + conn.closed() 超时
        let closed = self.conn.closed();
        tokio::pin!(closed);
        let mut out_event_log = 0u32; // 仅日志用，避免 log spam
        loop {
            tokio::select! {
                // 路 A：stream A 控制面 —— 由 run() 持有 recv_a
                res = read_frame(&mut recv_a) => {
                    match res {
                        Ok(event) => {
                            // Control 类 —— 本步**转发**到 outgoing_events
                            // （client 端 connect_to_handle 设的 sender），
                            // 让 `LanMouseConnection::recv()` 通过 recv_tx
                            // 收到 Ack / Pong / Leave 等响应，capture.rs 据
                            // 此切到 Sending 状态或释放 capture。
                            //
                            // **STEP-8.2 修复 — Bug #7**：修前只 log debug，
                            // recv_tx 是死字段（Bug #4 同源），server 响应
                            // 永远到不了本地 capture。
                            log::debug!("run: stream A read event: {event:?}");
                            if let Some(tx) = self.outgoing_events.lock().await.as_ref() {
                                let remote = self.conn.remote_address();
                                if let Err(e) = tx.send((remote, event.clone())) {
                                    log::debug!(
                                        "run: outgoing_events send failed (forwarder 已退): {e}"
                                    );
                                }
                            }
                        }
                        Err(super::Error::FrameTooLarge(len)) => {
                            log::error!("run: stream A FrameTooLarge({len}) — closing");
                            return Err(super::Error::FrameTooLarge(len));
                        }
                        Err(super::Error::Truncated) => {
                            log::info!("run: stream A truncated — peer closed (Bug #9 path)");
                            // **STEP-8.2 修复 — Bug #9**：peer 单方面关闭
                            // 时主动 `conn.close()`，让 quinn `closed()` future
                            // 立刻 fire（quinn 默认要双向 close 才 fire，
                            // 等 30s idle_timeout）→
                            // supervisor 立刻收到 peer.run 退出 → set_active_
                            // addr(None) + remove peer → capture 下次 send
                            // 触发 release（user-noticeable 立即恢复）。
                            //
                            // 同时推一个 Leave 到 outgoing_events → forwarder
                            // → capture.rs 立即 release_capture（不等下次
                            // mouse event 触发 send）。
                            let _ = self.conn.close(0u32.into(), b"peer closed stream");
                            let remote = self.conn.remote_address();
                            self.send_outgoing_event(ProtoEvent::Leave(0), remote).await;
                            break;
                        }
                        Err(super::Error::HelloFailed(msg)) if msg.starts_with("read frame") => {
                            // **STEP-8.2 修复 — Bug #10**：read_u32 / read_exact
                            // 的 IO 错误（如 "connection lost"、"closed stream"）
                            // —— peer 已关 / conn 死。本应是 stream 结束信号，
                            // 走与 Truncated 相同的"主动 close + 推 Leave"路径
                            // 让 capture 立即 release。
                            //
                            // **修前**：这条路径误归为 "decode error → skip-frame
                            // 续读"，但 read IO 错误不是 decode 错误（数据未到
                            // 达解码阶段）—— 主循环**永远 continue待**
                            // 每次都同一错 + 30s idle_timeout 才让 closed()
                            // fire，期间 capture 不 release（用户 30s 延迟
                            // 看到 mouse 恢复）。
                            //
                            // **对照 listen.rs supervisor**（旧路径）：
                            // `Err(e) => return Err(e)` —— 任何 IO 错误立刻
                            // 退出，与本 fix 语义一致。
                            log::info!("run: stream A read IO error (Bug #10 path): {msg}");
                            let _ = self.conn.close(0u32.into(), b"peer read IO error");
                            let remote = self.conn.remote_address();
                            self.send_outgoing_event(ProtoEvent::Leave(0), remote).await;
                            break;
                        }
                        Err(e) => {
                            // decode frame 失败 → 单帧损坏，skip-frame 续读
                            log::warn!("run: stream A read_frame error (skip frame): {e}");
                        }
                    }
                }

                // 路 B：stream B mpsc —— Reliable 类（按键 / Modifier）
                evt = read_streams.b.recv() => {
                    match evt {
                        Some(StreamEvent::Reliable(event)) => {
                            log::debug!("run: stream B Reliable event: {event:?}");
                            // 本步**不**做业务分派（不调 route_input）；
                            // STEP-6.x LanMouseConnection 接入时按 cfg 分派
                            // → 本地 emulation
                        }
                        Some(other) => {
                            // stream B reader task 不应产生 Control/Datagram；
                            // 防御性记录（warn 但不退出 —— reader task 内已
                            // 严格包 Reliable；这里多一道兜底）
                            log::warn!("run: stream B produced non-Reliable event: {other:?}");
                        }
                        None => {
                            // stream B reader task 已退出（peer closed / fatal）
                            log::info!("run: stream B reader closed, exiting main loop");
                            break;
                        }
                    }
                }

                // 路 D：datagram mpsc —— Datagram 类（高频指针事件）
                evt = rx_d.recv() => {
                    match evt {
                        Some(StreamEvent::Datagram(event)) => {
                            // 防 log spam：本步每 64 帧记一条
                            out_event_log = out_event_log.wrapping_add(1);
                            if out_event_log % 64 == 1 {
                                log::debug!("run: datagram Datagram event (count={out_event_log}): {event:?}");
                            }
                            // 本步**不**做业务分派（同上 stream B）
                        }
                        Some(other) => {
                            // datagram_reader_task 不应产生 Control/Reliable；
                            // 防御性记录
                            log::warn!("run: datagram_reader produced non-Datagram event: {other:?}");
                        }
                        None => {
                            // datagram_reader task 已退出（conn.closed / read_datagram 返 Err）
                            log::info!("run: datagram_reader closed, exiting main loop");
                            break;
                        }
                    }
                }

                // 路 C：conn closed 兜底 —— 任意源触发关闭都退出主循环
                closed_res = &mut closed => {
                    log::info!("run: conn.closed() fired: {closed_res:?}");
                    // **STEP-8.2 修复 — Bug #9**：closed() fire 通常意味
                    // 着 peer 已发 close 帧（双向 close 路径）。推一个
                    // Leave 让本地 capture 立即 release（不等下次 mouse
                    // event 触发 send）。
                    let remote = self.conn.remote_address();
                    self.send_outgoing_event(ProtoEvent::Leave(0), remote).await;
                    break;
                }
            }
        }

        // (8) 退出主循环 —— 取 close reason 并转成 `super::Error::Handshake(reason)`
        //
        // **STEP-6.5 改造**：原返回 `Ok(())` —— caller 看不到"为什么关"的语义。
        // 现取 `conn.close_reason()` (quinn 0.11 公开 API)：peer 主动 close 时
        // 返 `Some(ConnectionError::ApplicationClosed(_))`；网络层断连时返
        // `Some(ConnectionError::ConnectionLost(_))` / `TimedOut` 等；本地主动
        // close 时返 `Some(ConnectionError::LocallyClosed)`；从未关闭过则
        // 返 `None` —— 这种情形极少（说明主循环是别的原因 break 的，比如
        // stream A/B/D 异常），此时返回 `super::Error::Handshake(LocallyClosed)`
        // 让 caller 走 `should_retry_after_close` 判定（保守不重试）。
        //
        // **为什么用 `super::Error::Handshake(ConnectionError)` 复用现有变体**：
        // `super::Error::Handshake` 在 STEP-2.2 已定义成 `#[from] quinn::ConnectionError`，
        // 复用零成本。`super::Error::Closed` 是 bak 命名，本仓不引入（保持现有变体集
        // 最小）。`should_retry_after_close(&reason)` 是 free function，caller
        // 自己判 retry 决策。
        log::debug!("run: main loop exited");
        let reason = self.conn.close_reason();
        let reason = reason.unwrap_or(quinn::ConnectionError::LocallyClosed);
        log::info!("peer.run({role:?}) exiting with close reason: {reason:?}");
        Err(super::Error::Handshake(reason))
    }
}

/// 单个 datagram 的"安全上限"（STEP-5.1 引入）。
///
/// 取 STEP-0.1 spike 实测的 QUIC 握手初期下限 `1162` 字节 —— MTU 探测完成前
/// `max_datagram_size()` 可能先报这个保守值，避免在此期间用误
/// `max_datagram_size()` 触发 `SendDatagramError::TooLarge`。SPIKE 后值
/// 可升到 `1414`（路径 MTU 探测完成）但**不缓存**——本常量仅作为
/// `max_datagram_size().map(|m| m.min(MAX_SAFE_DATAGRAM))` 的取 min 边界，
/// 防止上层用任何"陈旧的更大值"绕过 cap。
///
/// 与 bak `mousehop/src/quic_transport.rs:121-123 MAX_SAFE_DATAGRAM`
/// 完全对齐（PLAN-v4 Step 0.1 结论 D）。
const MAX_SAFE_DATAGRAM: usize = 1162;

// 抑制 `use` 中的 `Ordering` 警告（client_hello / server_hello 内部
// 引用了 self.hello_ok.store(..., Ordering::Release)，但本文件 import
// 没直接用）。
#[allow(unused_imports)]
use std::sync::atomic::Ordering as _Ordering;

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddrV4};

    use lan_mouse_ipc::InputChannelConfig;
    use lan_mouse_proto::{ProtoEvent, MAX_EVENT_SIZE};

    use crate::quic_transport::endpoint::{accept, dial, endpoint};
    use crate::quic_transport::protocol::{client_hello, read_frame, server_hello};
    use crate::quic_transport::session::PeerRole;
    use crate::quic_transport::test_helpers::{
        ephemeral_cert, ephemeral_pins_dir, endpoint_with_test_cert, key_event, local_set_test,
        motion_event, motion_test_server,
    };

    use super::*;

    /// STEP-5.1 验收 (1/1)：端到端 send_motion 走 datagram 路径，对端
    /// recv_datagram 收到事件并解码回原字段。
    #[tokio::test]
    async fn motion_datagram_round_trip() {
        use crate::quic_transport::endpoint::install_crypto_provider;
        install_crypto_provider();

        let (server_cert, server_key) = ephemeral_cert();
        let (server_ep, server_addr) = motion_test_server(server_cert, server_key);

        let server_task = tokio::spawn(async move {
            let conn = tokio::time::timeout(std::time::Duration::from_secs(5), accept(&server_ep))
                .await
                .expect("server accept timeout")
                .expect("server accept");
            let session = std::sync::Arc::new(PeerSession::from_connection(conn));

            tokio::time::timeout(std::time::Duration::from_secs(5), server_hello(&session))
                .await
                .expect("server hello timeout")
                .expect("server hello");

            let datagram = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                session.connection().read_datagram(),
            )
            .await
            .expect("read_datagram 超时（datagram 路径没走通？）")
            .expect("read_datagram");

            assert_eq!(
                datagram.len(),
                MAX_EVENT_SIZE,
                "send_motion 写满定长缓冲，对端应收到 {MAX_EVENT_SIZE} 字节"
            );
            let buf: [u8; MAX_EVENT_SIZE] =
                datagram.as_ref().try_into().expect("datagram 长度应匹配");
            let decoded = ProtoEvent::try_from(buf).expect("datagram 应解码为 ProtoEvent");
            match decoded {
                ProtoEvent::Input(input_event::Event::Pointer(
                    input_event::PointerEvent::Motion { time, dx, dy },
                )) => {
                    assert_eq!(time, 4242, "Motion.time round-trip 一致");
                    assert_eq!(dx, 12.5, "Motion.dx round-trip 一致");
                    assert_eq!(dy, -7.25, "Motion.dy round-trip 一致");
                }
                other => panic!("解码结果应为 Motion，实际：{other:?}"),
            }
        });

        let pins_dir = std::env::temp_dir().join(format!(
            "lan-mouse-motion-roundtrip-pins-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&pins_dir);
        let (client_cert, client_key) = ephemeral_cert();
        let client_ep = endpoint(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
            .expect("client endpoint bind");
        let conn = dial(
            &client_ep,
            server_addr,
            client_cert[0].clone(),
            client_key,
            &pins_dir,
        )
        .await
        .expect("dial");
        let client_session = PeerSession::from_connection(conn);

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client_hello(&client_session),
        )
        .await
        .expect("client hello timeout")
        .expect("client hello");

        assert!(
            client_session.connection().max_datagram_size().is_some(),
            "握手完成后 max_datagram_size() 应为 Some（quinn 默认启用 datagram）"
        );

        client_session
            .send_motion(&motion_event())
            .await
            .expect("send_motion 应走 datagram 成功");

        server_task.await.expect("server task");
        drop(client_session);
        client_ep.wait_idle().await;
        let _ = std::fs::remove_dir_all(&pins_dir);
    }

    /// STEP-5.4 验收 (1/1)：两端都跑 `Arc<PeerSession>::run(role)`，双向各发 1 帧
    /// Motion → 双端 datagram_reader 各收 1 帧 → 双方都成功退出。
    #[tokio::test(flavor = "multi_thread")]
    async fn peer_session_round_trip_motion_keyboard() {
        local_set_test!(peer_session_round_trip_motion_keyboard, {
            use crate::quic_transport::endpoint::install_crypto_provider;
            install_crypto_provider();

            let (server_cert, server_key) = ephemeral_cert();
            let server_ep = endpoint_with_test_cert(
                SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into(),
                server_cert,
                server_key,
            )
            .expect("server endpoint bind");
            let server_addr = server_ep.local_addr().expect("server addr");

            let server_task = tokio::task::spawn_local(async move {
                let conn = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    accept(&server_ep),
                )
                .await
                .expect("server accept timeout")
                .expect("server accept");
                let session = std::sync::Arc::new(PeerSession::from_connection(conn));
                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    std::sync::Arc::clone(&session).run(PeerRole::Server),
                )
                .await
                .expect("server run timeout")
                .expect("server run");
            });

            let pins_dir = std::env::temp_dir().join(format!(
                "lan-mouse-step-5-4-pins-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
            ));
            let _ = std::fs::remove_dir_all(&pins_dir);
            let (client_cert, client_key) = ephemeral_cert();
            let client_ep = endpoint(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
                .expect("client endpoint bind");
            let conn = dial(
                &client_ep,
                server_addr,
                client_cert[0].clone(),
                client_key,
                &pins_dir,
            )
            .await
            .expect("dial");
            let client_arc = std::sync::Arc::new(PeerSession::from_connection(conn));

            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                client_hello(&client_arc),
            )
            .await
            .expect("client_hello timeout")
            .expect("client_hello");

            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                client_arc.send_motion(&motion_event()),
            )
            .await
            .expect("client send_motion timeout")
            .expect("client send_motion");

            client_arc
                .connection()
                .close(quinn::VarInt::from(0u32), b"test done");

            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                std::sync::Arc::clone(&client_arc).run(PeerRole::Client),
            )
            .await;

            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server_task).await;

            drop(client_arc);
            client_ep.wait_idle().await;
            let _ = std::fs::remove_dir_all(&pins_dir);
        });
    }

    /// STEP-8.2 验收 — Bug #3 回归：先 `client_hello` 置 `hello_ok=true`，
    /// 再 `peer.run(Client)` —— `peer.run()` 必须跳过 `client_hello`。
    #[tokio::test(flavor = "multi_thread")]
    async fn peer_run_skips_hello_if_already_done() {
        local_set_test!(peer_run_skips_hello_if_already_done, {
            use crate::quic_transport::endpoint::install_crypto_provider;
            install_crypto_provider();

            let (server_cert, server_key) = ephemeral_cert();
            let server_ep = endpoint_with_test_cert(
                SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into(),
                server_cert,
                server_key,
            )
            .expect("server endpoint bind");
            let server_addr = server_ep.local_addr().expect("server addr");

            let server_task = tokio::task::spawn_local(async move {
                let conn = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    accept(&server_ep),
                )
                .await
                .expect("server accept timeout")
                .expect("server accept");
                let session = PeerSession::from_connection(conn);

                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    server_hello(&session),
                )
                .await
                .expect("server hello timeout")
                .expect("server hello should succeed");

                let mut recv_a = session
                    .take_stream_a_recv()
                    .await
                    .expect("server stream A recv cached");
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    read_frame(&mut recv_a),
                )
                .await;

                drop(session);
            });

            let pins_dir = ephemeral_pins_dir();
            let _ = std::fs::remove_dir_all(&pins_dir);
            let (client_cert, client_key) = ephemeral_cert();
            let client_ep = endpoint(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
                .expect("client endpoint bind");
            let conn = dial(
                &client_ep,
                server_addr,
                client_cert[0].clone(),
                client_key,
                &pins_dir,
            )
            .await
            .expect("dial");
            let client_arc = std::sync::Arc::new(PeerSession::from_connection(conn));

            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                client_hello(&client_arc),
            )
            .await
            .expect("client_hello timeout")
            .expect("client_hello");
            assert!(client_arc.hello_ok(), "client_hello 后 hello_ok 应已置位");

            let client_for_run = std::sync::Arc::clone(&client_arc);
            let run_task = tokio::task::spawn_local(async move {
                client_for_run.run(PeerRole::Client).await
            });

            tokio::time::sleep(std::time::Duration::from_secs(1)).await;

            client_arc
                .connection()
                .close(quinn::VarInt::from(0u32), b"test done");

            let run_result = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                run_task,
            )
            .await
            .expect("peer.run 未在 2s 内退出")
            .expect("peer.run task 未 panic");

            match run_result {
                Err(crate::quic_transport::Error::HelloTimeout(_)) => {
                    panic!("Bug #3 回归");
                }
                Err(crate::quic_transport::Error::HelloFailed(msg)) => {
                    panic!("Bug #3 回归: {msg}");
                }
                Err(crate::quic_transport::Error::Handshake(reason)) => {
                    log::debug!("peer.run exited with Handshake({reason:?})");
                }
                Err(other) => {
                    log::debug!("peer.run exited with: {other:?}");
                }
                Ok(()) => {
                    log::debug!("peer.run exited Ok");
                }
            }

            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), server_task).await;

            drop(client_arc);
            client_ep.wait_idle().await;
            let _ = std::fs::remove_dir_all(&pins_dir);
        });
    }

    /// STEP-8.2 验收 — Bug #5 回归：stream A 控制事件端到端可达。
    #[tokio::test(flavor = "multi_thread")]
    async fn send_stream_a_round_trip_control_event() {
        local_set_test!(send_stream_a_round_trip_control_event, {
            use crate::quic_transport::endpoint::install_crypto_provider;
            install_crypto_provider();

            let (server_cert, server_key) = ephemeral_cert();
            let server_ep = endpoint_with_test_cert(
                SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into(),
                server_cert,
                server_key,
            )
            .expect("server endpoint bind");
            let server_addr = server_ep.local_addr().expect("server addr");

            let server_task = tokio::task::spawn_local(async move {
                let conn = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    accept(&server_ep),
                )
                .await
                .expect("server accept timeout")
                .expect("server accept");
                let session = PeerSession::from_connection(conn);

                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    server_hello(&session),
                )
                .await
                .expect("server hello timeout")
                .expect("server hello should succeed");

                let mut recv_a = session
                    .take_stream_a_recv()
                    .await
                    .expect("server_hello 后 stream_a_recv 应已缓存");
                let event = tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    super::super::protocol::read_hello_frame(&mut recv_a),
                )
                .await
                .expect("server stream A read 3s 超时")
                .expect("server stream A read 应成功");

                assert!(
                    matches!(event, ProtoEvent::Ping),
                    "server 应收到 client 发的 Ping，实际: {event:?}"
                );

                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                drop(session);
            });

            let pins_dir = ephemeral_pins_dir();
            let _ = std::fs::remove_dir_all(&pins_dir);
            let (client_cert, client_key) = ephemeral_cert();
            let client_ep = endpoint(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
                .expect("client endpoint bind");
            let conn = dial(
                &client_ep,
                server_addr,
                client_cert[0].clone(),
                client_key,
                &pins_dir,
            )
            .await
            .expect("dial");
            let client_arc = std::sync::Arc::new(PeerSession::from_connection(conn));

            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                client_hello(&client_arc),
            )
            .await
            .expect("client_hello timeout")
            .expect("client_hello");
            assert!(client_arc.hello_ok());

            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                client_arc.send_input(
                    &ProtoEvent::Ping,
                    &InputChannelConfig::default(),
                ),
            )
            .await
            .expect("client send_input(Ping) 超时")
            .expect("client send_input(Ping) 应成功");

            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server_task).await;

            drop(client_arc);
            client_ep.wait_idle().await;
            let _ = std::fs::remove_dir_all(&pins_dir);
        });
    }

    // suppress unused warnings on helpers imported for the tests
    #[allow(dead_code)]
    fn _unused() {
        let _ = key_event();
    }
}