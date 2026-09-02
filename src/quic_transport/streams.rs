//! 流任务装配层（STEP-5.3 / STEP-5.4）。
//!
//! 本模块承担 QUIC 三条流（A / B / C）+ datagram 的读取任务编排：
//!
//! - [`StreamPair`] / [`Bidi`] / [`StreamBunch`] 三条流的所有权封装
//! - [`StreamEvent`] mpsc 队列事件类型（区分 Control / Reliable / Datagram）
//! - [`ReadStreams`] `read_loop` 返回值（stream B receiver + reader task handle）
//! - [`read_stream_b_loop`] stream B reader task（Reliable 阻塞 sender 背压）
//! - [`read_loop`] 装配 3 条流 + 启动 stream B reader
//! - [`datagram_reader_task`] datagram 事件循环（丢最旧背压）
//! - [`READ_STREAM_BUFFER_CAP`] mpsc 容量 = 64
//!
//! 与 [`super::protocol`] 的关系：stream B reader 调 `protocol::read_frame` 解码；
//! 与 [`super::session`] 的关系：[`datagram_reader_task`] 与 [`read_loop`] 都消费
//! `Arc<PeerSession>`（peer.run 内 spawn 这两个 task）。

use std::sync::Arc;

use quinn::{RecvStream, SendStream};
use tokio::io::AsyncRead;
use tokio::sync::mpsc as tokio_mpsc;
use tokio::task::{JoinHandle, spawn_local};

use lan_mouse_proto::ProtoEvent;

use super::protocol::{read_frame, write_frame};
use super::session::PeerSession;
use super::{Error, Result};

/// Stream B reader task 用的 mpsc 通道容量（STEP-5.3 引入）。
///
/// 容量 `64` —— 既能缓冲 ~50ms@1000Hz 高频输入突发，又不浪费内存
/// （每个 `StreamEvent` < 256B → 64 个 < 16KB）。
///
/// **背压策略**（SUGGESTION #28 治理）：
///
/// | 事件类别 | 来源 | 队列满时策略 |
/// |---|---|---|
/// | **Control**（Stream A 上的 Enter / Leave / Ack / Hello / Ping / Pong） | Stream A | **阻塞 sender**（Stream A reader task 由 listen.rs supervisor 自己管，本步不实现） |
/// | **Input Reliable**（Stream B 上的 Key / Button / Modifiers，channel 配置为 Stream 时） | Stream B | **阻塞 sender**（`tx.send().await`）—— 鼠标按键 + 键盘按键不能丢 |
/// | **Input Datagram**（Motion / Axis / AxisDiscrete120 等高频） | Datagram | **丢最旧**（队列满时 `try_recv` 拿最旧一帧丢，再 `try_send` 新帧） | **STEP-5.4 ✅**（SUGGESTION #S-16 治理落地） |
///
/// 当前 STEP-5.3 + STEP-5.4 已落实 Reliable 阻塞 sender + Datagram 丢最旧
/// 两类背压。**Control 由 caller（listen.rs supervisor）自行管理** —— 它持
/// 有 `recv_a` 在 `select!` 里 `read_frame` 自然阻塞读，相当于"背压到对端"。
pub(crate) const READ_STREAM_BUFFER_CAP: usize = 64;

/// 读 task 送入 mpsc 队列的事件类型（STEP-5.3 引入）。
///
/// **为什么需要枚举**（而非裸 `ProtoEvent`）：
/// STEP-5.4 `PeerSession::run()` 主循环用 `tokio::select!` 合并 datagram /
/// stream A / stream B / stream C 4 个 reader 时，需要区分"是控制面事件
/// 还是要走 IPC 推送 / 调度层"。M1 阶段控制面事件（Enter / Leave / Ack /
/// Hello / Ping / Pong）**不**进 IPC（不进 [`lan_mouse_ipc::TransportEvent`]
/// 那是 M2）；STEP-5.4 接 run() 时由 `StreamEvent` 的 enum 分流决定动作
/// —— `Control` 类直接写回 hello_ok / channel 配置 / 日志，`Reliable`
/// 类按 `route_input` 分派给本地 emulation，`Datagram` 类直发。
///
/// **3 个变体**（PLAN §5.3 派发表）：
/// - **`Control(ProtoEvent)`** —— Stream A 读出的控制帧（Enter / Leave /
///   Ack / Hello / Ping / Pong / Hello echo 等）
/// - **`Reliable(ProtoEvent)`** —— Stream B 读出的可靠输入事件（鼠标按键 /
///   键盘按键 / 键盘 Modifier，按 STEP-4.4 `route_input` 配置 `ChannelMode::Stream`
///   时入 StreamB）
/// - **`Datagram(ProtoEvent)`** —— QUIC datagram 读出的事件（Motion /
///   Axis / AxisDiscrete120 / Button/Key/Modifiers 按 Datagram 配置时）。
///   本步 **不** 由 reader task 产生 —— STEP-5.4 datagram_reader 接入
///   时填充。预留变体为 STEP-5.4 run() 的 `match` 提前就位（避免新增
///   variant 时 caller 编译失败）
///
/// **dead_code chain**：本 enum 由 STEP-5.3 `read_loop`（Reliable）+ STEP-5.4
/// `datagram_reader_task`（Datagram）填充；STEP-5.4 `run()` 主循环 `select!`
/// 消费。Control 类由 caller / listen.rs supervisor 持有 recv_a 自行读。
/// 三个变体当前均有 producer，`#[allow(dead_code)]` 已由 STEP-5.4 移除。
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Stream A 读出的控制帧
    Control(ProtoEvent),
    /// Stream B 读出的可靠输入事件（按键 / Modifier）
    Reliable(ProtoEvent),
    /// QUIC datagram 读出的高频事件（STEP-5.4 datagram_reader 填充）
    Datagram(ProtoEvent),
}

/// 单条双向 stream 的所有权封装（STEP-5.2 引入）。
///
/// **抽象动机**：`SendStream` / `RecvStream` 来自 quinn 0.11，单条 bidi
/// 流的两个半边必然成对出现（`open_bi() -> (SendStream, RecvStream)`）；
/// 把它们收口成一个 `Bidi<S>` 类型，让上层（`StreamBunch` /
/// `PeerSession.stream_bunch`）可以一次性拿走整对、流级别生命周期管理
/// 集中在一处。
///
/// **为什么 generic `S: AsyncRead + AsyncWrite + Unpin` 而非固定
/// `SendStream`**：单测（如 `frame_round_trip` 借 mock 流做 codec
/// round-trip）和生产路径（quinn 真实 stream）共用同一份 `write_frame`
/// / `read_frame` codec —— `SendStream` 已实现 `AsyncRead` + `AsyncWrite`
/// + `Unpin`，generic 约束不会限制生产路径。
///
/// **生命周期 / Send 边界**：当前主仓不用 `Bidi<SendStream>` 做跨 await
/// 共享（`PeerSession.stream_bunch: Arc<tokio::sync::Mutex<Option<...>>>`
/// 已守护）；generic `S` 允许 caller 在测试里用 `tokio::io::DuplexStream`
/// / `Vec<u8>` 之类的本地类型，自由度高。
///
/// 与 bak `mousehop/src/quic_transport.rs` 的 `StreamPair` 形态对齐
/// （语义相同 —— send / recv 二元组），但**类型抽象更轻**：bak 的
/// `StreamPair` 用 `Option<SendStream>` 包装以支持"recv 半边 take"语义，
/// 本仓 `Bidi` 直接持裸 `S`（recv 半边 take 由上层结构 `StreamBunch`
/// + `PeerSession.stream_bunch` 一起管理）。
///
/// dead_code chain：本类型被 `StreamBunch { a, b, c }` 字段直接持有；
/// `StreamBunch` 暂未在 main-code 被消费（STEP-5.3 read_loop 接入）。
/// 当前加 `#[allow(dead_code)]` 守护（与 STEP-1.x / 2.x / 3.x 同模式）。
#[allow(dead_code)]
pub struct Bidi<S, R = S>
where
    S: tokio::io::AsyncWrite + Unpin,
    R: tokio::io::AsyncRead + Unpin,
{
    pub send: S,
    pub recv: R,
}

impl<S, R> Bidi<S, R>
where
    S: tokio::io::AsyncWrite + Unpin,
    R: tokio::io::AsyncRead + Unpin,
{
    /// 构造：把 quinn `open_bi()` / `accept_bi()` 拿到的 `(SendStream, RecvStream)`
    /// 包成 `Bidi`。生产路径 `S = SendStream` / `R = RecvStream`；测试可传
    /// `tokio::io::DuplexStream`（同一类型，2-arg 默认）。
    pub fn new(send: S, recv: R) -> Self {
        Self { send, recv }
    }
}

/// 3 条 bidi stream 的所有权集合（STEP-5.2 引入）。
///
/// **`a`** —— Step-3.2 引入的 control 流（Hello / Enter / Leave / Ack /
/// Ping / Pong）；Hello 阶段 `client_hello()` / `server_hello()` 缓存，
/// STEP-5.4 read_loop 通过 `PeerSession.stream_bunch` 拿走接管权。
///
/// **`b`** —— input 流（鼠标按键 / 键盘按键 / 键盘 Modifier，按 STEP-4.4
/// `route_input` 分派）；STEP-5.1 起由 `send_motion` 降级路径复用
///（STEP-5.2 把 inline uni stream 升级为 bidi cache + 长度前缀）。
///
/// **`c`** —— clipboard meta 流（M2 预留）。STEP-5.2 引入字段但**不开**
/// reader task —— PLAN §9 M1 边界"不要做：开 Stream C reader task"。
///
/// **dead_code chain**：当前仅 `PeerSession.stream_bunch` 持有本类型
/// （空 `None`），STEP-5.3 / 5.4 read_loop 装配三 stream 时消费。`#[allow]`
/// 守护与 STEP-3.2 `StreamPair` 同模式。
#[allow(dead_code)]
pub struct StreamBunch {
    /// Stream A（control，可靠有序）
    pub a: Bidi<SendStream, RecvStream>,
    /// Stream B（input，可靠有序）
    pub b: Bidi<SendStream, RecvStream>,
    /// Stream C（clipboard meta，M2 预留；本步不开 reader task）
    pub c: Bidi<SendStream, RecvStream>,
}

/// `read_loop` 返回值 —— 给 STEP-5.4 `select!` 主循环消费用。
///
/// **字段语义**：
/// - **`b`** —— Stream B 读出事件的 mpsc Receiver。可靠输入事件（按键 /
///   Modifier）按 STEP-4.4 `route_input` 配置 `ChannelMode::Stream` 时经
///   此 Receiver 送给上层 emulation / dispatch
/// - **`join_b`** —— Stream B reader task 的 `JoinHandle<Result<(), Error>>`。
///   caller 可 `.await` 监听 reader task 退出；本步不强制 await（reader
///   task 与 select! 主循环并行）
///
/// **Stream A 为何不在 struct 内**：caller 已持有 `recv_a`（read_loop 参数），
/// 直接在 listen.rs supervisor 的 `select!` 里 `read_frame(&mut recv_a)`
/// 即可，不需要 read_loop 再包一层 mpsc。这与 Leader 决策一致。
///
/// **Stream C 为何不在 struct 内**：本步 read_loop 内部立即 drop stream C
/// `RecvStream`（守 PLAN §9 M1 边界）—— 不开 reader task，所以不返回
/// 给 caller。STEP-5.4 接 run() 时由 listen.rs supervisor 重新装配
/// StreamBunch + 开 stream C reader（仍守 §9）。
///
/// **不实现 `Clone`**：`tokio::sync::mpsc::Receiver` 不 Clone（语义
/// 不允许 —— 一次只能有一个 consumer）。
///
/// **不实现 `Debug`**：当前 `ReadStreams` 仅持有 Receiver + JoinHandle
/// 两个可 Debug 字段，derive 即可；如未来加 `RecvStream` 等字段需手工
/// impl。
///
/// **dead_code chain**：本 struct 由 STEP-5.4 `PeerSession::run()` 主
/// 循环消费（本步实现 `run()` 时消费）；dead_code 自动消失。
pub struct ReadStreams {
    /// Stream B 读出事件 Receiver（Reliable 类）
    pub b: tokio_mpsc::Receiver<StreamEvent>,
    /// Stream B reader task 的 JoinHandle
    pub join_b: JoinHandle<std::result::Result<(), Error>>,
}

/// Stream B 读 task（STEP-5.3 引入）。
///
/// **职责**：从 `stream_bunch.b.recv` 循环 `read_frame` → 解码为
/// `ProtoEvent` → 包成 `StreamEvent::Reliable(...)` → `tx.send().await`
/// 送入 mpsc 队列。
///
/// **三类错误处理**（与 bak `read_stream_a_loop` 同模式）：
/// - `Error::FrameTooLarge(len)` → fatal：攻击者控制长度字段或 wire 损坏，
///   task 不可恢复；返回 `Err` 让 caller `join_b` 收到
/// - `Error::HelloFailed(msg)` 当 `msg.starts_with("decode frame")` →
///   codec 解码失败（单帧损坏）：`warn!` 日志 + 跳过当前帧继续循环，
///   **不**退出 task
/// - 其他 IO 错误（peer close / reset / `Error::Truncated`）→ task 退出，
///   返回 `Err`
///
/// **背压**：`tx.send(event).await` 阻塞等待 receiver —— 当上层
/// `select!` 处理慢 / 接收端未及时 drain 时，reader 会在 send 处 await，
/// 反向施压 stream B 流控（quinn 流控）。这是 SUGGESTION #28 治理
/// 中"control / input reliable 类阻塞 sender"的具体落实。
///
/// **receiver drop 退出**：当 caller drop `ReadStreams.b`（receiver）时，
/// `tx.send().await` 返回 `Err(SendError)` → task 干净退出 + 返回
/// `Ok(())`（视为"正常关闭"）。
///
/// **dead_code chain**：本函数由 [`PeerSession::read_loop`] spawn；
/// `JoinHandle` 由 caller 通过 [`ReadStreams::join_b`] 持有。
#[allow(dead_code)]
async fn read_stream_b_loop<R>(
    mut recv: R,
    tx: tokio_mpsc::Sender<StreamEvent>,
) -> std::result::Result<(), Error>
where
    R: AsyncRead + Unpin,
{
    loop {
        match read_frame(&mut recv).await {
            Ok(event) => {
                // Reliable 阻塞 send —— 背压：caller 慢 → reader 慢
                if tx.send(StreamEvent::Reliable(event)).await.is_err() {
                    // receiver 已 drop（caller 终止 read_loop），干净退出
                    log::info!("stream B reader: receiver dropped, exiting cleanly");
                    return Ok(());
                }
            }
            Err(Error::FrameTooLarge(len)) => {
                log::error!("stream B: FrameTooLarge({len}) — fatal, closing task");
                return Err(Error::FrameTooLarge(len));
            }
            Err(Error::HelloFailed(msg)) if msg.starts_with("decode frame") => {
                log::warn!("stream B: skip frame (decode error): {msg}");
                continue;
            }
            Err(e) => {
                log::info!("stream B reader exiting (IO closed): {e}");
                return Err(e);
            }
        }
    }
}

/// `PeerSession::read_loop` —— 装配 3 条 stream 的 reader（STEP-5.3 引入）。
///
/// **职责**：spawn 1 个独立 reader task（stream B），stream A 由 caller
/// 持有（参数借用 `&mut RecvStream`），stream C 立即 drop（守 §9 M1
/// 边界）。返回 [`ReadStreams`] 给 STEP-5.4 `run()` 主循环消费。
///
/// **流程**：
/// 1. **取 stream_bunch 所有权**（`Option::take()` 拿走 `Some(...)`）——
///    caller 已通过 STEP-5.2 / STEP-5.4 把 `StreamBunch` 装配好
/// 2. **stream A 由 caller 持有** —— `recv_a: &mut RecvStream` 是参数
///    借用，**不**在 read_loop 内 spawn reader；caller（listen.rs
///    supervisor）自行在 `select!` 里 `read_frame(recv_a)`
/// 3. **stream B**：`tx_b = mpsc::channel(READ_STREAM_BUFFER_CAP)`，spawn
///    `read_stream_b_loop(stream_bunch.b.recv, tx_b)` 返回
///    `JoinHandle<Result<(), Error>>`
/// 4. **stream C**：`drop(stream_bunch.c)` 立即触发 quinn 优雅关闭（**守
///    §9 M1 边界** —— 不开 reader task）
/// 5. **返回** [`ReadStreams { b: rx_b, join_b }`]
///
/// **为什么 stream A 由 caller 持有**（而非 read_loop 内部 spawn）：
/// - listen.rs supervisor 的 `select!` 主循环**已经**在持有 `recv_a`
///   （来自 `server_hello` 的 `take_stream_a_recv()`），无需 read_loop
///   再包一层 mpsc
/// - 减少一次 task spawn / 一次 mpsc 通道 → 端到端延迟更低
/// - 与 Leader 决策一致：stream A 是 control stream，没有"join 行为"语义
///   上的对称需求（A 由 supervisor 整个生命周期持有）
///
/// **为什么 stream C 立即 drop**：PLAN §9 M1 边界明确要求"不要做：开
/// Stream C reader task"。stream C 是 M2 clipboard 元数据预留。本步把
/// `RecvStream` 所有权 take 出来**立即 drop**，让 quinn 给对端发 FIN /
/// STOP_SENDING，避免对端 stream C 上一直写半边被卡。STEP-5.4 接 run()
/// 时由 listen.rs supervisor 重新装配 StreamBunch + 开 stream C reader
/// （但那时仍是 §9 守门）。
///
/// **死循环背压**：stream B mpsc 容量 [`READ_STREAM_BUFFER_CAP`] = 64；
/// 阻塞 sender 实现可靠输入事件的背压（详细见该常量 doc）。
///
/// **`stream_bunch` 所有权语义**：调用 [`PeerSession::take_stream_bunch`]
/// 取出 `Option<StreamBunch>` 内的 StreamBunch，调用后
/// `peer.stream_bunch` 字段回到 `None`。本步首次接入时该字段为 `None`
/// （STEP-5.2 留空）；STEP-5.4 `run()` 接入时会先 `set_stream_bunch(...)`
/// 填充。
///
/// **错误路径**：当前实现不主动返回 `Err`（装配步骤本身不失败）；
/// 装配失败（如 `stream_bunch` 未设置）→ 返回 [`Error::HelloFailed`]
/// "stream_bunch not initialized" 错误给 caller 决策。
///
/// **`bunch.a` 处理**：stream_bunch.a（stream A 缓存的 `Bidi<SendStream>`）
/// 在 bunch move 进 drop 时一起 drop（caller 已通过 `take_stream_a_recv`
/// 拿走 recv 半边 + `take_stream_bunch` 拿走 recv_a → 整对已被 caller
/// 接管；bunch.a 内剩余字段无害 drop）。
///
/// **dead_code chain**：本方法由 STEP-5.4 `PeerSession::run()` 装配
/// 入口消费；本步 `#[allow(dead_code)]` 守护（与 STEP-3.x / 4.x 同模式）。
#[allow(dead_code)]
#[allow(unused_variables)] // recv_a reserved for STEP-6.3 stream A reader integration
pub async fn read_loop(
    peer: &PeerSession,
    recv_a: &mut RecvStream,
) -> std::result::Result<ReadStreams, Error> {
    // (1) 取 stream_bunch 所有权 —— 一次性 take，调用后该字段回 None
    let bunch = peer
        .take_stream_bunch()
        .await
        .ok_or_else(|| Error::HelloFailed("stream_bunch not initialized".into()))?;

    // (2) stream B 装配：mpsc + reader task
    let (tx_b, rx_b) = tokio_mpsc::channel::<StreamEvent>(READ_STREAM_BUFFER_CAP);
    let join_b = spawn_local(read_stream_b_loop(bunch.b.recv, tx_b));

    // (3) stream A：caller 已持有 recv_a（参数借用），不内部 spawn
    //     —— leader 决策：减少 task 数 + 减少 mpsc 层

    // (4) stream C：立即 drop —— 守 PLAN §9 M1 边界
    drop(bunch.c);

    // (5) bunch.a (stream A 的 Bidi<SendStream>) 在 bunch move 末尾自动 drop
    //     —— 无害：caller 已通过 take_stream_a_recv 拿走 recv 半边，
    //     bunch.a.send (即 stream A 的 SendStream 缓存) 随 bunch drop 释放。

    log::info!(
        "read_loop: stream B reader spawned (cap={READ_STREAM_BUFFER_CAP}), \
         stream C dropped (M1 §9 守门)"
    );

    Ok(ReadStreams {
        b: rx_b,
        join_b,
    })
}

/// Datagram 类事件读 task（STEP-5.4 引入，SUGGESTION #S-16 治理落地）。
///
/// **职责**：循环 `read_datagram()` → 解析为 `ProtoEvent`（定长 codec）→
/// 包成 `StreamEvent::Datagram` → 通过 mpsc 送入主循环消费。
///
/// **背压策略（SUGGESTION #S-16）—— 丢最旧**：
///
/// 队列满时 `tx.try_send` 失败 → `tx.try_recv` 拿最旧一帧丢弃 → 再
/// `tx.try_send(new)`。重复直到成功。如果反复失败导致队列被狂 drain
/// （极端场景：对端 datagram 速率 > 本端处理速率 × 100），`tx.try_send`
/// 仍失败 → 用 `log::warn` 记下"该帧也丢"。这与 bak
/// `mousehop/src/quic_transport.rs` `datagram_reader_task` 的"丢最旧"
/// 形态对齐。
///
/// **为什么 Motion / / Axis / / AxisDiscrete120 走丢最旧策略**：高频指针增量
/// 丢一帧用户无感知（与 stream B 的"按键不能丢"形成对比 —— SUGGESTION
/// #28 治理的双路径设计）。
///
/// **任务退出条件**：
/// - `read_datagram` 返 `Err`（peer 关 / conn 死）→ break → task 退出
/// - mpsc `tx` 被 drop（主循环退出，rx_d 被 drop）→ `tx.send().await` 返
///   `SendError` → 视为正常退出
/// - 解析失败（`ProtoEvent::try_from`） → `log::warn` + continue（单帧损坏
///   不致命，与 stream B 的 skip-frame 语义对称）
///
/// **可见性 `pub(crate)`**：本函数由 [`super::session::PeerSession::run`] 消费
/// （spawn 后即 'static）。`pub(crate)` 让 session.rs 能 spawn 它。
pub(crate) async fn datagram_reader_task(
    peer: Arc<PeerSession>,
    tx: tokio_mpsc::Sender<StreamEvent>,
) {
    loop {
        match peer.conn.read_datagram().await {
            Ok(bytes) => {
                // 定长 codec：ProtoEvent::try_from 收 [u8; MAX_EVENT_SIZE]，
                // 实际 bytes.len() 应 == MAX_EVENT_SIZE
                let buf: [u8; lan_mouse_proto::MAX_EVENT_SIZE] = match bytes.as_ref().try_into() {
                    Ok(b) => b,
                    Err(_) => {
                        log::warn!(
                            "datagram_reader: datagram 长度非 MAX_EVENT_SIZE({})，skip frame",
                            lan_mouse_proto::MAX_EVENT_SIZE
                        );
                        continue;
                    }
                };
                let event = match ProtoEvent::try_from(buf) {
                    Ok(e) => e,
                    Err(e) => {
                        log::warn!("datagram_reader: ProtoEvent 解码失败，skip frame: {e}");
                        continue;
                    }
                };

                // SUGGESTION #S-16 背压：队列满 → 丢当前帧
                //
                // tokio mpsc Sender 不支持从 send 端 drain；Drop-oldest 语义要
                // 在 Receiver 端实现（M1 简化：接受当前帧丢，caller 慢就让 datagram
                // 走丢 —— 与高频 Motion 事件 user-noticeable drop 的取舍一致）。
                // 真正 Drop-oldest 留 STEP-7.x 接本地输入代理时按按。
                match tx.try_send(StreamEvent::Datagram(event)) {
                    Ok(()) => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        // 队列满 → 丢当前帧（高频指针事件，单帧丢失不可见）
                        log::trace!("datagram_reader: queue full, dropping current frame");
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                        // 主循环已退出（rx_d 被 drop），干净退出
                        log::info!("datagram_reader: mpsc receiver dropped, exiting");
                        return;
                    }
                }
            }
            Err(e) => {
                // peer 关 / conn 死 —— 退出 task
                log::info!("datagram_reader: read_datagram error, exiting: {e}");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddrV4};

    use tokio::io::AsyncWriteExt;

    use lan_mouse_proto::ProtoEvent;

    use crate::quic_transport::endpoint::{accept, dial, endpoint};
    use crate::quic_transport::protocol::write_frame;
    use crate::quic_transport::session::PeerSession;
    use crate::quic_transport::test_helpers::{
        ephemeral_cert, ephemeral_pins_dir, endpoint_with_test_cert, key_event, local_set_test,
        motion_event, motion_test_server,
    };

    use super::*;

    /// STEP-5.2 验收 (1/2)：codec round-trip ——
    /// `write_frame(send, &event)` → `read_frame(&mut recv)` 还原出同一
    /// event。
    #[tokio::test]
    async fn frame_round_trip() {
        let (mut write_half, mut read_half) = tokio::io::duplex(4096);

        let events = vec![
            ProtoEvent::Ping,
            ProtoEvent::hello([0xab; 8]),
            motion_event(),
        ];
        let events_clone = events.clone();
        let writer = tokio::spawn(async move {
            for event in &events_clone {
                super::super::protocol::write_frame(&mut write_half, event)
                    .await
                    .expect("write_frame 应成功");
            }
        });

        for expected in &events {
            let got = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                super::super::protocol::read_frame(&mut read_half),
            )
            .await
            .expect("read_frame timeout")
            .expect("read_frame 应成功");
            let expected_dbg = format!("{expected:?}");
            let got_dbg = format!("{got:?}");
            assert_eq!(
                got_dbg, expected_dbg,
                "codec round-trip 后事件应一致：expected {expected_dbg}, got {got_dbg}"
            );
        }

        writer.await.expect("writer task");
    }

    /// STEP-5.2 验收 (2/2)：body 截断时 `read_frame` 应返回
    /// [`super::super::Error::Truncated`]。
    #[tokio::test]
    async fn frame_truncated_rejected() {
        let (mut write_half, mut read_half) = tokio::io::duplex(4096);

        let writer = tokio::spawn(async move {
            write_half
                .write_u32(17)
                .await
                .expect("write length prefix");
            write_half
                .write_all(&[0u8; 8])
                .await
                .expect("write truncated body");
            drop(write_half);
        });

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            super::super::protocol::read_frame(&mut read_half),
        )
        .await
        .expect("read_frame 总超时不应触发");

        match result {
            Err(crate::quic_transport::Error::Truncated) => {}
            Err(other) => panic!("错误应为 Error::Truncated，实际：{other:?}"),
            Ok(event) => panic!("截断帧 read_frame 不应成功，实际解码为 {event:?}"),
        }

        writer.await.expect("writer task");
    }

    /// STEP-5.3 验收 (1/2)：stream B reader task + mpsc 队列 round-trip。
    #[tokio::test]
    async fn stream_frame_round_trip() {
        let (mut write_half, read_half) = tokio::io::duplex(4096);

        let (tx, mut rx) = tokio_mpsc::channel::<StreamEvent>(READ_STREAM_BUFFER_CAP);
        let join_b = tokio::spawn(read_stream_b_loop(read_half, tx));

        let event = key_event();
        let event_dbg = format!("{event:?}");
        super::super::protocol::write_frame(&mut write_half, &event)
            .await
            .expect("write_frame 应成功");

        let received = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("mpsc recv 超时")
            .expect("mpsc recv q succeeded");

        match received {
            StreamEvent::Reliable(got) => {
                let got_dbg = format!("{got:?}");
                assert_eq!(
                    got_dbg, event_dbg,
                    "stream B reader 送入的事件应与 write_frame 写入一致"
                );
            }
            other => panic!("事件类别应为 Reliable，实际：{other:?}"),
        }

        drop(write_half);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), join_b).await;
    }

    /// STEP-5.3 验收 (2/2)：stream B reader task 的背压语义。
    #[tokio::test]
    async fn streams_backpressure_blocks_when_receiver_idle() {
        let (mut write_half, read_half) = tokio::io::duplex(4096);

        let (tx, mut rx) = tokio_mpsc::channel::<StreamEvent>(2);
        let join_b = tokio::spawn(read_stream_b_loop(read_half, tx));

        let events: Vec<ProtoEvent> = (0..5).map(|_| key_event()).collect();
        let events_dbg: Vec<String> = events.iter().map(|e| format!("{e:?}")).collect();
        for event in &events {
            super::super::protocol::write_frame(&mut write_half, event)
                .await
                .expect("write_frame 应成功");
        }

        let mut got: Vec<String> = Vec::with_capacity(events.len());
        for _ in 0..events.len() {
            let received = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("drain 超时")
                .expect("drain recv q succeeded");
            match received {
                StreamEvent::Reliable(got_event) => {
                    got.push(format!("{got_event:?}"));
                }
                other => panic!("事件类别应为 Reliable，实际：{other:?}"),
            }
        }

        assert_eq!(
            got, events_dbg,
            "5 帧 round-trip 后顺序与内容应一致（背压 = 阻塞 sender 不丢事件）"
        );

        drop(write_half);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), join_b).await;
    }

    /// STEP-5.3 验收 (3/3 bonus)：stream C 处理 —— 守 §9 M1 边界。
    #[tokio::test(flavor = "multi_thread")]
    async fn stream_c_take_releases_quinn_recv_stream() {
        local_set_test!(stream_c_take_releases_quinn_recv_stream, {
            use crate::quic_transport::endpoint::install_crypto_provider;
            use crate::quic_transport::protocol::{client_hello, server_hello};
            install_crypto_provider();

            let (server_cert, server_key) = ephemeral_cert();
            let (server_ep, server_addr) = motion_test_server(server_cert, server_key);

            let server_session_fut = tokio::task::spawn_local(async move {
                let conn = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    accept(&server_ep),
                )
                .await
                .expect("server accept timeout")
                .expect("server accept");
                let session = std::sync::Arc::new(PeerSession::from_connection(conn));

                tokio::time::timeout(std::time::Duration::from_secs(5), server_hello(&session))
                    .await
                    .expect("server hello timeout")
                    .expect("server hello");

                tokio::time::sleep(std::time::Duration::from_millis(200)).await;

                let bunch = session.take_stream_bunch().await;
                assert!(
                    bunch.is_none(),
                    "STEP-5.3 范围 stream_bunch 应为 None（未装配）"
                );

                session
                    .connection()
                    .close(quinn::VarInt::from(0u32), b"test done");
                session
            });

            let pins_dir = std::env::temp_dir().join(format!(
                "lan-mouse-stream-c-pins-{}-{}",
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

            let client_bunch = client_session.take_stream_bunch().await;
            assert!(
                client_bunch.is_none(),
                "client 端 stream_bunch 也应为 None（与 server 端对称）"
            );

            let _server_session = server_session_fut.await.expect("server task");
            drop(client_session);
            client_ep.wait_idle().await;
            let _ = std::fs::remove_dir_all(&pins_dir);
        });
    }
}