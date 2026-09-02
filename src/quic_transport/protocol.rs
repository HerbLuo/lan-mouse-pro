//! 应用层 wire protocol（STEP-3.2 / 4.4 / 5.2）—— Hello 握手 + 长度前缀帧
//! codec + 通道路由分派。
//!
//! 本模块承担 QUIC 链路之上的应用层协议：
//!
//! - [`HELLO_TIMEOUT`] 应用层 Hello 握手超时（3s）
//! - [`StreamPair`] stream A / B / C 的 `(send, recv)` 二元组缓存（hello 期）
//! - [`Channel`] enum 4 类通道（Datagram / StreamA / StreamB / StreamC）
//! - [`route_input`] 按 per-handle `InputChannelConfig` 把 ProtoEvent 分派
//!   到对应 `Channel` 的纯函数
//! - [`client_hello`] / [`server_hello`] 应用层 Hello 握手
//! - [`write_hello_frame`] / [`read_hello_frame`] hello 专用帧 codec
//! - [`write_frame`] / [`read_frame`] / [`read_any_frame`] 通用长度前缀帧 codec
//!
//! 与 [`super::session`] 的关系：`client_hello` / `server_hello` 都接
//! `&PeerSession` 参数，把对端的 stream A 缓存进 `peer.stream_a_cache` 后
//! 调 `set_cached_send_a` 复用。

use std::sync::atomic::Ordering;
use std::time::Duration;

use quinn::{RecvStream, SendStream, VarInt};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use lan_mouse_ipc::{ChannelMode, InputChannelConfig};
use lan_mouse_proto::{ProtoEvent, MAX_EVENT_SIZE};

use super::session::PeerSession;
use super::{Error, Result, ALPN_LAN_MOUSE};

/// 应用层 Hello 握手超时（STEP-3.2 引入）。
///
/// QUIC mTLS 握手完成之后，对端必须在 `HELLO_TIMEOUT` 内在 stream A 上完成
/// `PROTOCOL_MAGIC` 交换；超时即视为"对端非 lan-mouse 实例"，关 conn +
/// `Error::HelloTimeout(HELLO_TIMEOUT)`。3s 是 PLAN §5 D6 决策（抄 bak）。
///
/// **与 QUIC idle timeout 的关系**：`HELLO_TIMEOUT` 仅在 Hello 阶段生效；
/// 之后由 `max_idle_timeout = 30s`（[`super::tls::default_transport_config`]）接管。
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(3);

/// Stream A / B / C 缓存结构体：`(send, recv)` 二元组，两半边可独立 take
/// （STEP-5.x 接 read_loop 时 take recv 半边；send 半边留给写路径复用）。
///
/// STEP-3.2 只引入类型；具体 take 方法在 STEP-5.x。
///
/// **可见性 `pub(crate)`**：被 [`super::session::PeerSession::stream_a_cache`]
/// 字段持有，同时 [`client_hello`] / [`server_hello`] 装配它。
pub(crate) struct StreamPair {
    pub(crate) send: Option<SendStream>,
    pub(crate) recv: Option<RecvStream>,
}

impl StreamPair {
    pub(crate) fn new(send: SendStream, recv: RecvStream) -> Self {
        Self {
            send: Some(send),
            recv: Some(recv),
        }
    }
}

/// 4 类 QUIC 通道（STEP-4.4 引入）。
///
/// **Datagram** —— QUIC unreliable datagram，最低延迟、可能丢包。
/// Motion / Axis / AxisDiscrete120 恒定走本通道；鼠标按键在
/// `mouse_button = Datagram` 配置下走本通道；键盘在 `keyboard = Datagram`
/// 配置下走本通道。
///
/// **StreamA** —— QUIC reliable bidi stream，control 通道。低频、必到；
/// Enter / Leave / Ack / Hello / Ping / Pong 一律走本通道，与 per-handle
/// 配置无关。Step-3.2 hello 握手即建立在 StreamA 上。
///
/// **StreamB** —— QUIC reliable bidi stream，input 通道。鼠标按键在
/// `mouse_button = Stream` 配置下走本通道；键盘在 `keyboard = Stream` 配置
/// 下走本通道（`Modifiers` 也走 keyboard 配置；见 `route_input` 实现注释）。
///
/// **StreamC** —— QUIC reliable bidi stream，clipboard meta 通道（M2 预留）。
/// 本步不分配任何事件给 StreamC（路由表里全部事件已落到前 3 个变体）；M2 接入
/// `ProtoEvent::Clipboard` / `Input(ClipboardEvent)` 时再加分支。
///
/// **derives**：`Debug / Clone / Copy / PartialEq / Eq` —— 与 bak
/// `mousehop/src/quic_transport.rs:873` 一致；`Copy` 因为只有 4 个 0-字节变体。
/// 不带 `Hash`（路由表用 `match`，不存 HashMap key）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Datagram,
    StreamA,
    StreamB,
    StreamC,
}

/// 把 `ProtoEvent` 按 per-handle [`InputChannelConfig`] 分派到 [`Channel`] 的
/// **纯函数**（STEP-4.4 引入）。
///
/// **分派表**（与 STEP-4.3 写进 `config.toml` 注释的"Motion 永远走 Datagram
/// 不受此设置影响"完全一致 —— 文档与实现必须同步）：
///
/// | `ProtoEvent` | `Channel` | 触发条件 |
/// |---|---|---|
/// | `Input(Pointer::Motion)` | `Datagram` | **恒定**，与 cfg 无关 |
/// | `Input(Pointer::Axis)` | `Datagram` | **恒定**，高频 scroll 增量 |
/// | `Input(Pointer::AxisDiscrete120)` | `Datagram` | **恒定**，离散 scroll tick |
/// | `Input(Pointer::Button)` | `Datagram` 或 `StreamB` | 按 `cfg.mouse_button` |
/// | `Input(Keyboard::Key)` | `Datagram` 或 `StreamB` | 按 `cfg.keyboard` |
/// | `Input(Keyboard::Modifiers)` | `Datagram` 或 `StreamB` | 按 `cfg.keyboard`（**关键**：与 Key 走同一通道，避免 modifier 状态与按键解耦丢同步） |
/// | `Enter` / `Leave` / `Ack` / `Hello` / `Ping` / `Pong` | `StreamA` | **恒定**，control 流 |
/// | （M2 范围，本步不出现）`Clipboard` 等 | `StreamC` | M2 引入 ProtoEvent 变体时再补 |
///
/// **为什么 Motion / Axis / AxisDiscrete120 恒定 Datagram**：高频、丢一帧无
/// 感的输入不应承担 Stream 重传延迟。`Axis` 是 touchpad scroll 增量、
/// `AxisDiscrete120` 是鼠标 scroll wheel 单 tick（120 = 一格），与 Motion
/// 同属"流流加量"，也走 Datagram（与 bak
/// `mousehop/src/quic_transport.rs:934-936` 完全对齐）。
///
/// **为什么 Modifiers 跟 keyboard 配置走**：lan-mouse 的 modifier 状态本质
/// 是"键状态的压缩视图"——单独把 Modifiers 配成与 Key 不同通道，会出现
/// "Modifier 已 Datagram 投递 + Key 仍在 Stream B 队列" 的时序错位。STEP-4.3
/// 文档中"input_channels"只暴露 `mouse_button` / `keyboard` 两字段，`Modifier`
/// 跟随 `keyboard` 是最自然的契约。
///
/// **为什么不暴露 Channel::StreamC 的路由**：M1 不引入 `ProtoEvent::Clipboard` /
/// `Input(ClipboardEvent)`（PLAN §9 边界）；主仓 `ProtoEvent` 也不含这些变体。
/// match 全覆盖 → 编译期 `_ => unreachable!()` 也不必要 —— 当前 ProtoEvent
/// 8 个变体都已显式列出，新增 M2 变体时编译器会报错提醒补 match arm。
///
/// **dead_code chain**：STEP-4.4 单测消费 + STEP-5.1 `send_motion()` +
/// STEP-5.4 `PeerSession::run()` 调用栈消费；目前 main-code 无 caller，
/// 加 `#[allow(dead_code)]` 守护（与 STEP-1.x / 2.x / 3.x 同模式）。
#[allow(dead_code)]
pub fn route_input(cfg: &InputChannelConfig, event: &ProtoEvent) -> Channel {
    use input_event::{Event as InputEvent, KeyboardEvent, PointerEvent};

    match event {
        // (1) 高频指针增量 → 恒定 Datagram（Motion / Axis / AxisDiscrete120）
        ProtoEvent::Input(InputEvent::Pointer(
            PointerEvent::Motion { .. }
            | PointerEvent::Axis { .. }
            | PointerEvent::AxisDiscrete120 { .. },
        )) => Channel::Datagram,

        // (2) 鼠标按键 → 按 cfg.mouse_button 分派
        ProtoEvent::Input(InputEvent::Pointer(PointerEvent::Button { .. })) => match cfg.mouse_button
        {
            ChannelMode::Datagram => Channel::Datagram,
            ChannelMode::Stream => Channel::StreamB,
        },

        // (3) 键盘按键 / Modifiers → 按 cfg.keyboard 分派（同一通道，避免
        //     modifier / key 时序错位）
        ProtoEvent::Input(InputEvent::Keyboard(KeyboardEvent::Key { .. }))
        | ProtoEvent::Input(InputEvent::Keyboard(KeyboardEvent::Modifiers { .. })) => {
            match cfg.keyboard {
                ChannelMode::Datagram => Channel::Datagram,
                ChannelMode::Stream => Channel::StreamB,
            }
        }

        // (4) Control 流（PLAN §3 "Stream A — control"））→ 恒定 StreamA
        ProtoEvent::Enter(_)
        | ProtoEvent::Leave(_)
        | ProtoEvent::Ack(_)
        | ProtoEvent::Hello { .. }
        | ProtoEvent::Ping
        | ProtoEvent::Pong(_) => Channel::StreamA,
    }
}

/// 客户端 Hello 握手（STEP-3.2 引入）。
///
/// 1. `peer.conn.open_bi().await` 开 stream A（control 流）
/// 2. 发 `ProtoEvent::hello(local_commit())` 给对端
/// 3. 等对端 echo `ProtoEvent::Hello` 回包（`HELLO_TIMEOUT` 内）
/// 4. 校验 echo magic == `PROTOCOL_MAGIC`：
///    - 匹配 → 缓存 stream A 到 `peer.stream_a_cache` + 置 `hello_ok = true`
///    - 不匹配 → `conn.close(VarInt(0), "hello failed (wrong magic)")` + 返
///      `Err(HelloFailed("wrong magic: ..."))`
/// 5. 超时 → `conn.close(VarInt(0), "hello failed (timeout)")` + 返
///    `Err(HelloTimeout(HELLO_TIMEOUT))`
///
/// **缓存 stream A**：client_hello 与 server_hello 对称缓存（与 bak
/// `mousehop/src/quic_transport.rs:2452` Step 1.9a 行为对齐）—— 控制面
/// 读写都在这条 stream 上，STEP-5.4 read_loop 接手所有权时通过
/// `take_stream_a_recv()` 拿 recv 半边，send 半边留给 `send_stream_a()`
/// 复用（避免重开第二条 stream A 破坏 PLAN §3 "A/B/C 各开 1 条长期复用"）。
///
/// **错误归一**：所有 magic / 解码 / 超时失败统一归到 [`Error::HelloFailed`]
/// / [`Error::HelloTimeout`]；`conn.close(...)` 一定先调，确保对端
/// `accept_bi()` / `open_bi()` 立即以 `ConnectionError::LocallyClosed` 失
/// 败退出，不留 zombie conn。
///
/// **dead_code chain**：STEP-3.2 仅被测试消费；STEP-5.4 接 `run()`
/// STEP-6.1 接 `connect.rs::connect_to_handle` 时移除 `#[allow]`。
#[allow(dead_code)]
pub async fn client_hello(peer: &PeerSession) -> std::result::Result<(), Error> {
    let (mut send, mut recv) = peer.conn.open_bi().await.map_err(Error::Handshake)?;
    let outgoing = ProtoEvent::hello(crate::config::local_commit());

    let exchange = async {
        write_hello_frame(&mut send, &outgoing).await?;
        read_hello_frame(&mut recv).await
    };
    let response = match tokio::time::timeout(HELLO_TIMEOUT, exchange).await {
        Ok(Ok(event)) => event,
        Ok(Err(e)) => return Err(e),
        Err(_elapsed) => {
            peer.conn
                .close(VarInt::from(0u32), b"hello failed (timeout)");
            log::warn!("client hello handshake timed out after {HELLO_TIMEOUT:?}");
            return Err(Error::HelloTimeout(HELLO_TIMEOUT));
        }
    };

    match response {
        ProtoEvent::Hello { magic, .. } if magic == lan_mouse_proto::PROTOCOL_MAGIC => {
            // **STEP-8.2 修复**：缓存 send 半边到 `cached_send_a` 供后续
            // `send_stream_a` 复用 —— 详见 cached_send_a 字段 docstring 与
            // `send_stream_a` docstring。
            //
            // **顺序**：先 put 进 stream_a_cache（Pair 形式），再
            // take_stream_a_send 拿出来存 cached_send_a —— 与
            // supervisor / peer.run 后续调 take_stream_a_recv 不冲突
            // （send / recv 各自独立 take）。
            *peer.stream_a_cache.lock().await = Some(StreamPair::new(send, recv));
            let send_a = peer
                .take_stream_a_send()
                .await
                .expect("stream_a_cache just put Some(Pair { send: Some, recv: Some }) — take_stream_a_send must return Some");
            *peer.cached_send_a.lock().await = Some(send_a);
            peer.hello_ok.store(true, Ordering::Release);
            Ok(())
        }
        ProtoEvent::Hello { magic, .. } => {
            peer.conn
                .close(VarInt::from(0u32), b"hello failed (wrong magic)");
            log::warn!(
                "client hello rejected: wrong magic {:?}",
                std::str::from_utf8(&magic).unwrap_or("?????????")
            );
            Err(Error::HelloFailed(format!(
                "wrong magic: expected {:?}, got {:?}",
                std::str::from_utf8(&lan_mouse_proto::PROTOCOL_MAGIC).unwrap_or("????????"),
                std::str::from_utf8(&magic).unwrap_or("????????"),
            )))
        }
        other => {
            peer.conn
                .close(VarInt::from(0u32), b"hello failed (non-hello response)");
            log::warn!("client hello rejected: non-Hello response: {other}");
            Err(Error::HelloFailed(format!(
                "non-Hello response on stream A: {other}"
            )))
        }
    }
}

/// 服务端 Hello 握手（STEP-3.2 引入）。
///
/// 流程与 `client_hello` 对称：
/// 1. `peer.conn.accept_bi().await` 等 stream A（client 主动 `open_bi`）
/// 2. 读 client 发来的 Hello
/// 3. 校验 magic == `PROTOCOL_MAGIC`（不匹配 → close + Err）
/// 4. echo 自己 Hello 给 client
/// 5. 缓存 stream A 到 `peer.stream_a_cache` + 置 `hello_ok = true`
///
/// **失败语义**：`open_bi` / `accept_bi` 同步失败 → `Err(HelloFailed)`；
/// `read_hello_frame` 超时 → `Err(HelloTimeout)`。所有失败路径先
/// `conn.close(...)` 再返 Err。
///
/// **dead_code chain**：STEP-3.2 仅被测试消费；STEP-5.4 接 `run()`
/// STEP-6.2 接 `listen.rs::read_loop` 时移除 `#[allow]`。
#[allow(dead_code)]
pub async fn server_hello(peer: &PeerSession) -> std::result::Result<(), Error> {
    let (mut send, mut recv) = peer
        .conn
        .accept_bi()
        .await
        .map_err(|e| Error::HelloFailed(format!("accept_bi: {e}")))?;

    let hello = match tokio::time::timeout(HELLO_TIMEOUT, read_hello_frame(&mut recv)).await {
        Ok(Ok(event)) => event,
        Ok(Err(e)) => return Err(e),
        Err(_elapsed) => {
            peer.conn
                .close(VarInt::from(0u32), b"hello failed (timeout)");
            log::warn!("server hello handshake timed out after {HELLO_TIMEOUT:?}");
            return Err(Error::HelloTimeout(HELLO_TIMEOUT));
        }
    };

    match &hello {
        ProtoEvent::Hello { magic, .. } if *magic == lan_mouse_proto::PROTOCOL_MAGIC => {}
        ProtoEvent::Hello { magic, .. } => {
            peer.conn
                .close(VarInt::from(0u32), b"hello failed (wrong magic)");
            log::warn!(
                "server hello rejected: wrong magic {:?}",
                std::str::from_utf8(magic).unwrap_or("????????")
            );
            return Err(Error::HelloFailed(format!(
                "wrong magic: expected {:?}, got {:?}",
                std::str::from_utf8(&lan_mouse_proto::PROTOCOL_MAGIC).unwrap_or("????????"),
                std::str::from_utf8(magic).unwrap_or("????????"),
            )));
        }
        other => {
            peer.conn
                .close(VarInt::from(0u32), b"hello failed (non-hello frame)");
            log::warn!("server hello rejected: non-Hello frame: {other}");
            return Err(Error::HelloFailed(format!(
                "non-Hello frame on stream A: {other}"
            )));
        }
    }

    // echo 自己 Hello
    let outgoing = ProtoEvent::hello(crate::config::local_commit());
    write_hello_frame(&mut send, &outgoing).await?;

    // **STEP-8.2 修复**：缓存 send 半边到 `cached_send_a` 供后续
    // `send_stream_a` 复用 —— 详见 client_hello 镜像注释 + cached_send_a
    // 字段 docstring。`send_stream_a` 走 cached 与 client 走 cached 是
    // **同一条 bidi**（server 的 recv ↔ client 的 send / client 的 recv
    // ↔ server 的 send），server 端 supervisor 读 recv_a 即可读到
    // client 的 Enter / Ack / Pong 等控制事件。
    *peer.stream_a_cache.lock().await = Some(StreamPair::new(send, recv));
    let send_a = peer
        .take_stream_a_send()
        .await
        .expect("stream_a_cache just put Some(Pair { send: Some, recv: Some }) — take_stream_a_send must return Some");
    *peer.cached_send_a.lock().await = Some(send_a);

    peer.hello_ok.store(true, Ordering::Release);
    Ok(())
}

/// 把 `ProtoEvent` 编码成**长度前缀帧**写到 stream（STEP-3.2 引入）。
///
/// 帧格式：`[u32 BE length][bytes...]`（与 STEP-5.2 `write_frame` 共用
/// codec；本步只引入 `hello` 专用路径，避免与 STEP-5.x `write_frame` 一
/// 起 import 造成循环）。
///
/// **失败传播**：写 IO 错误透传为 `Error::HelloFailed("write Hello frame:
/// ...")`。`ProtoEvent::try_from` / `.into()` 不可能失败（定长 codec +
/// Hello 只有 17 字节），无解码错误路径。
async fn write_hello_frame(send: &mut SendStream, event: &ProtoEvent) -> std::result::Result<(), Error> {
    let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.clone().into();
    send.write_u32(len as u32)
        .await
        .map_err(|e| Error::HelloFailed(format!("write Hello frame length: {e}")))?;
    send.write_all(&buf[..len])
        .await
        .map_err(|e| Error::HelloFailed(format!("write Hello frame body: {e}")))?;
    Ok(())
}

/// 从 stream 读**长度前缀帧**并解码为 `ProtoEvent`（STEP-3.2 引入）。
///
/// 帧格式：`[u32 BE length][bytes...]`。先读 `u32 BE len` → 校验
/// `len <= MAX_EVENT_SIZE`（防 DoS：攻击者控制长度字段会诱使
/// `read_exact` 读非常多字节）→ `read_exact(&mut buf[..len])` →
/// `ProtoEvent::try_from(buf)`。
///
/// **失败传播**：
/// - `read_exact` IO 错误 → `Error::HelloFailed("read Hello frame: ...")`
/// - `ProtoEvent::try_from` 失败 → `Error::HelloFailed("decode Hello frame: ...")`
///
/// **可见性 `pub(crate)`**：单测 `send_stream_a_round_trip_control_event`（session.rs）
/// 走 server 端 recv_a 读一帧 Ping 时调它。
pub(crate) async fn read_hello_frame(recv: &mut RecvStream) -> std::result::Result<ProtoEvent, Error> {
    let len = recv
        .read_u32()
        .await
        .map_err(|e| Error::HelloFailed(format!("read Hello frame length: {e}")))?
        as usize;
    if len > MAX_EVENT_SIZE {
        return Err(Error::HelloFailed(format!(
            "Hello frame too large: {len} bytes (max {MAX_EVENT_SIZE})"
        )));
    }
    let mut buf = [0u8; MAX_EVENT_SIZE];
    recv.read_exact(&mut buf[..len])
        .await
        .map_err(|e| Error::HelloFailed(format!("read Hello frame body ({len} bytes): {e}")))?;
    ProtoEvent::try_from(buf).map_err(|e| Error::HelloFailed(format!("decode Hello frame: {e}")))
}

/// 把 `ProtoEvent` 编码成**长度前缀帧**写到任意 `AsyncWrite` 流（STEP-5.2
/// 引入）。
///
/// 帧格式：`[u32 BE length][bytes...]`
///
/// 1. `From<ProtoEvent> for ([u8; MAX_EVENT_SIZE], usize)` 编码到定长
///    buffer，返回 `(buf, len)` —— `buf` 后部 0 填充
/// 2. `write_u32(len as u32).await` 写 4 字节长度前缀（BE 字节序）
/// 3. `write_all(&buf[..len).await` 写 `len` 个有效字节
///
/// **为什么用 `MAX_EVENT_SIZE` 作为 buffer 上限？** —— 当前
/// `lan-mouse-proto` 所有 `ProtoEvent` 变体都是定长 codec，编码后长度 ≤
/// 21 字节；buffer 后部 0 填充不影响 `ProtoEvent::try_from` 解码（解码时
/// 只看前 `len` 字节）。M2 引入变长 codec（剪贴板大负载）时另设
/// `MAX_FRAME_SIZE` 常量替换。
///
/// **generic `W: AsyncWrite + Unpin`**：生产路径 `W = SendStream`（quinn
/// 0.11 双向 stream 的写半边）；单测可以传 `tokio::io::DuplexStream`
/// / `Vec<u8>` 等本地类型跑 codec 路径。
///
/// **失败传播**：写 IO 错误归 [`Error::HelloFailed`]（保留 STEP-3.2 的
/// 错误语义独立于 codec）—— 长度字段写失败 = 流已断，与 read 端的
/// [`Error::Truncated`] 对称（不同变体承载不同语义）。
///
/// dead_code chain：STEP-5.3 独立读 task 写入时消费；STEP-5.4
/// `PeerSession::run()` 接入时消费；STEP-6.x `LanMouseConnection::send()`
/// 经 `route_input()` 分派后消费（事件 → write_frame）。
///
/// **可见性 `pub(crate)`**：[`super::session::PeerSession::send_stream_a`]
/// 等内部方法调它；外部 test (`frame_round_trip`) 走 `use super::*`。
#[allow(dead_code)]
pub(crate) async fn write_frame<W>(send: &mut W, event: &ProtoEvent) -> std::result::Result<(), Error>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.clone().into();
    send.write_u32(len as u32)
        .await
        .map_err(|e| Error::HelloFailed(format!("write frame length: {e}")))?;
    send.write_all(&buf[..len])
        .await
        .map_err(|e| Error::HelloFailed(format!("write frame body: {e}")))?;
    Ok(())
}

/// 从任意 `AsyncRead` 流读**长度前缀帧**并解码为 `ProtoEvent`（STEP-5.2
/// 引入）。
///
/// 帧格式：`[u32 BE length][bytes...]`
///
/// 1. `read_u32().await` 读 4 字节长度前缀（BE 字节序）→ `len`
/// 2. `len > MAX_EVENT_SIZE` → `Err([`Error::FrameTooLarge`]`(len))`（防 DoS）
/// 3. `read_exact(&mut buf[..len]).await` 读 `len` 个有效字节
/// 4. `ProtoEvent::try_from(buf)` 解码
///
/// **错误归一**（与 STEP-3.2 `read_hello_frame` 区分）：
/// - `FrameTooLarge(usize)` —— 透传，专属变体（reader task 据此 fatal 关 conn）
/// - `Truncated` —— `read_exact` 失败（quinn `UnexpectedEof` /
///   `ClosedStream` 表示对端半途关流）→ fatal（不 skip-frame 续读）
/// - `HelloFailed(msg)` —— 长度字段读失败 / `ProtoEvent::try_from` 失败
///   → 保留 STEP-3.2 语义独立于 codec
///
/// **为什么 buffer 后部不裁剪？** —— `ProtoEvent::try_from` 的签名是
/// `fn try_from([u8; MAX_EVENT_SIZE]) -> Result<Self, _>`，传
/// `&buf[..len]` 编译不过。`read_exact` 只写 buffer 前部（后部 0 不变），
/// 符合 `ProtoEvent` 定长 codec 假设（解码只看有效字段长度，不依赖尾部 0）。
///
/// **generic `R: AsyncRead + Unpin`**：与 [`write_frame`] 对称——生产
/// `R = RecvStream` / 单测 `tokio::io::DuplexStream`。
///
/// dead_code chain：STEP-5.3 独立读 task 读取时消费（stream A / B / C
/// reader）；STEP-5.4 `read_loop` 接手后消费；STEP-6.x
/// `listen.rs::read_loop` 接入时消费。
///
/// **可见性 `pub`**：被 [`super::streams::read_stream_b_loop`] 内部调，
/// 也被外部 `listen.rs` supervisor 等模块使用 —— 必须保持 `pub`。
#[allow(dead_code)]
pub async fn read_frame<R>(recv: &mut R) -> std::result::Result<ProtoEvent, Error>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let len = recv
        .read_u32()
        .await
        .map_err(|e| Error::HelloFailed(format!("read frame length: {e}")))? as usize;
    if len > MAX_EVENT_SIZE {
        return Err(Error::FrameTooLarge(len));
    }
    let mut buf = [0u8; MAX_EVENT_SIZE];
    match recv.read_exact(&mut buf[..len]).await {
        Ok(_bytes_read) => {}
        // 对端半途关流 → 截断（区别于"解码失败 HelloFailed"）
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Err(Error::Truncated);
        }
        Err(e) => {
            return Err(Error::HelloFailed(format!(
                "read frame body ({len} bytes): {e}"
            )));
        }
    }
    ProtoEvent::try_from(buf).map_err(|e| Error::HelloFailed(format!("decode frame: {e}")))
}

/// 单帧读取的公开别名（STEP-6.2 引入）。
///
/// **与 [`read_frame`] 区别**：类型签名固定为 `&mut RecvStream`，让
/// `listen.rs` supervisor 的 accept_bi 子 task 不需要带泛型参数；
/// `quinn::RecvStream` 实现 `tokio::io::AsyncRead + Unpin`，可直接复用
/// [`read_frame`] 的所有逻辑。
///
/// **使用场景**：server 端 `accept_bi()` 接 client 主动开出的 stream B/C
/// bidi 后，子 task 循环调 `read_any_frame(&mut recv)` 解码帧 +
/// 转译为 `ListenEvent::Msg`。
///
/// 与 bak `mousehop/src/quic_transport.rs:2301 read_any_frame` 形态对齐；
/// 本仓 `read_frame` 是泛型 + 本函数是 `RecvStream` 特化版（避免每次
/// 调用点重复标注 `<RecvStream>`）。
///
/// **dead_code chain**：本函数由 STEP-6.2 `listen.rs::spawn_quic_accept_tasks`
/// 的子 task 消费；main-code 接入后自然消化。
#[allow(dead_code)]
pub async fn read_any_frame(recv: &mut RecvStream) -> std::result::Result<ProtoEvent, Error> {
    read_frame(recv).await
}

/// 应用层 Hello 握手超时 watchdog（STEP-3.2 引入，STEP-5.4 接入 run()）。
///
/// **目的**：QUIC mTLS 通了不等于对端是 lan-mouse —— 一个对端可能过了
/// mTLS（自签根信任 + fingerprint allowlist）但故意不开 stream A，导致
/// `client_hello()` / `server_hello()` 永远挂在 `open_bi()` /
/// `accept_bi()`。`HELLO_TIMEOUT` watchdog 在不阻塞主流程的前提下做兜底：
///
/// 1. spawn 一个 tokio task，sleep `HELLO_TIMEOUT`
/// 2. 检查 `peer.hello_ok()` —— 若为 `true`（Hello 已成功）则安静退出
/// 3. 若仍为 `false` —— 主动 `conn.close(VarInt(0), "hello timeout")` 关
///    连 + `log::warn`，让对端 `client_hello()` / `server_hello()` 的
///    `accept_bi().await` / `open_bi().await` 立即以
///    `ConnectionError::LocallyClosed` 失败退出
///
/// **不**阻塞 `client_hello` / `server_hello` 自身 —— 那两个函数内部已有
/// `tokio::time::timeout(HELLO_TIMEOUT, ...)` 包裹（见下文实现），watchdog
/// 是"对端不发起 stream"这种**完全不开始 hello**场景的兜底。
///
/// **dead_code chain**：STEP-3.2 仅写函数 + 单测（直接 spawn 调用验证）；
/// STEP-5.4 `PeerSession::run()` 启 hello_watchdog 后此 `#[allow]` 移除。
///
/// **可见性 `pub(crate)`**：被 [`super::session::PeerSession::run`] 调。
#[allow(dead_code)]
pub(crate) fn hello_watchdog(peer: std::sync::Arc<PeerSession>) {
    use std::sync::atomic::Ordering;
    tokio::spawn(async move {
        tokio::time::sleep(HELLO_TIMEOUT).await;
        if !peer.hello_ok.load(Ordering::Acquire) {
            log::warn!(
                "hello watchdog: hello_ok 未在 {HELLO_TIMEOUT:?} 内置位，主动关闭连接"
            );
            peer.conn
                .close(VarInt::from(0u32), b"hello timeout (watchdog)");
        }
    });
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddrV4};

    use lan_mouse_ipc::{ChannelMode, InputChannelConfig};
    use lan_mouse_proto::{ProtoEvent, MAX_EVENT_SIZE};

    use crate::quic_transport::endpoint::accept;
    use crate::quic_transport::endpoint::dial;
    use crate::quic_transport::endpoint::endpoint;
    use crate::quic_transport::session::PeerSession;
    use crate::quic_transport::test_helpers::{
        ephemeral_cert, ephemeral_pins_dir, endpoint_with_test_cert, local_set_test,
    };

    use super::*;

    /// STEP-3.2 验收 (1/3)：Happy path —— 两端都跑 `server_hello` /
    /// `client_hello`, 两端 `peer.hello_ok()` 都返 `true`，且两端
    /// `stream_a_cache` 都有缓存。
    #[tokio::test]
    async fn hello_happy_path_exchanges_magic() {
        crate::quic_transport::endpoint::install_crypto_provider();

        let (server_cert_chain, server_key) = ephemeral_cert();
        let server_ep = endpoint_with_test_cert(
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into(),
            server_cert_chain,
            server_key,
        )
        .expect("server endpoint bind");
        let server_addr = server_ep.local_addr().expect("server addr");

        let server_task = tokio::spawn(async move {
            let conn = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                accept(&server_ep),
            )
            .await
            .expect("server accept timeout")
            .expect("server accept");
            let session = PeerSession::from_connection(conn);

            tokio::time::timeout(std::time::Duration::from_secs(5), server_hello(&session))
                .await
                .expect("server hello timeout")
                .expect("server hello should succeed");

            assert!(
                session.hello_ok(),
                "server 端 hello_ok 应为 true（server_hello 已置位）"
            );
            assert!(
                session.take_stream_a_recv().await.is_some(),
                "server_hello 后 peer.stream_a_cache.recv 应已缓存"
            );

            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            drop(session);
        });

        let pins_dir = ephemeral_pins_dir();
        let _ = std::fs::remove_dir_all(&pins_dir);
        let (client_cert_chain, client_key) = ephemeral_cert();
        let client_ep = endpoint(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
            .expect("client endpoint bind");
        let conn = dial(
            &client_ep,
            server_addr,
            client_cert_chain[0].clone(),
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
        .expect("client hello should succeed");

        assert!(
            client_session.hello_ok(),
            "client 端 hello_ok 应为 true（client_hello 已置位）"
        );
        assert!(
            client_session.take_stream_a_recv().await.is_some(),
            "client_hello 后 peer.stream_a_cache.recv 应已缓存"
        );

        drop(client_session);
        drop(client_ep);
        server_task.await.expect("server task");
        let _ = std::fs::remove_dir_all(&pins_dir);
    }

    /// STEP-3.2 验收 (2/3)：server 发错 magic → client `Error::HelloFailed`。
    #[tokio::test(flavor = "multi_thread")]
    async fn hello_wrong_magic_closes_connection() {
        local_set_test!(hello_wrong_magic_closes_connection, {
            crate::quic_transport::endpoint::install_crypto_provider();

            let (server_cert_chain, server_key) = ephemeral_cert();
            let server_ep = endpoint_with_test_cert(
                SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into(),
                server_cert_chain,
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

                let (mut send, _recv) = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    conn.accept_bi(),
                )
                .await
                .expect("accept_bi timeout")
                .expect("accept_bi");

                let wrong = ProtoEvent::Hello {
                    magic: *b"LAN-MOUS",
                    commit: [0u8; 8],
                };
                super::write_hello_frame(&mut send, &wrong)
                    .await
                    .expect("server write wrong hello");
                send.finish().expect("finish");

                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                conn.close(VarInt::from(0u32), b"test done");
                drop(conn);
            });

            let pins_dir = ephemeral_pins_dir();
            let _ = std::fs::remove_dir_all(&pins_dir);
            let (client_cert_chain, client_key) = ephemeral_cert();
            let client_ep = endpoint(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
                .expect("client endpoint bind");
            let conn = dial(
                &client_ep,
                server_addr,
                client_cert_chain[0].clone(),
                client_key,
                &pins_dir,
            )
            .await
            .expect("dial");
            let client_session = PeerSession::from_connection(conn);

            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                client_hello(&client_session),
            )
            .await
            .expect("client hello timeout (5s 兜底)")
            .expect_err("client_hello 应该返回 Err(HelloFailed)");

            match &result {
                crate::quic_transport::Error::HelloFailed(msg) => {
                    assert!(
                        msg.contains("wrong magic"),
                        "HelloFailed 消息应含 'wrong magic'，实际：{msg}"
                    );
                }
                other => panic!("错误应为 Error::HelloFailed(wrong magic...)，实际：{other:?}"),
            }

            assert!(!client_session.hello_ok(), "失败路径 hello_ok 应保持 false");

            drop(client_session);
            drop(client_ep);
            let _ = server_task.await;
            let _ = std::fs::remove_dir_all(&pins_dir);
        });
    }

    /// STEP-3.2 验收 (3/3)：对端不开 stream A → 3s 后
    /// `Error::HelloTimeout(HELLO_TIMEOUT)`。
    #[tokio::test]
    async fn hello_timeout_aborts_session() {
        crate::quic_transport::endpoint::install_crypto_provider();

        let (server_cert_chain, server_key) = ephemeral_cert();
        let server_ep = endpoint_with_test_cert(
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into(),
            server_cert_chain,
            server_key,
        )
        .expect("server endpoint bind");
        let server_addr = server_ep.local_addr().expect("server addr");

        let server_task = tokio::spawn(async move {
            let conn = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                accept(&server_ep),
            )
            .await
            .expect("server accept timeout")
            .expect("server accept");
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            drop(conn);
        });

        let pins_dir = ephemeral_pins_dir();
        let _ = std::fs::remove_dir_all(&pins_dir);
        let (client_cert_chain, client_key) = ephemeral_cert();
        let client_ep = endpoint(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
            .expect("client endpoint bind");
        let conn = dial(
            &client_ep,
            server_addr,
            client_cert_chain[0].clone(),
            client_key,
            &pins_dir,
        )
        .await
        .expect("dial");
        let client_session = PeerSession::from_connection(conn);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client_hello(&client_session),
        )
        .await
        .expect("client_hello 总超时不应触发（HELLO_TIMEOUT=3s 应先触发）")
        .expect_err("client_hello 应该返回 Err(HelloTimeout)");

        match &result {
            crate::quic_transport::Error::HelloTimeout(d) => {
                assert_eq!(*d, HELLO_TIMEOUT, "HelloTimeout 应等于 HELLO_TIMEOUT (3s)");
            }
            other => panic!("错误应为 Error::HelloTimeout(HELLO_TIMEOUT)，实际：{other:?}"),
        }

        assert!(!client_session.hello_ok(), "超时路径 hello_ok 应保持 false");

        drop(client_session);
        drop(client_ep);
        let _ = server_task.await;
        let _ = std::fs::remove_dir_all(&pins_dir);
    }

    // === STEP-4.4 route_input 纯函数 单元测试 =================================

    mod route_input_fixtures {
        use super::*;
        use input_event::{
            Event as InputEvent, KeyboardEvent, PointerEvent,
        };
        use lan_mouse_proto::Position;

        pub(super) fn motion() -> ProtoEvent {
            ProtoEvent::Input(InputEvent::Pointer(PointerEvent::Motion {
                time: 0,
                dx: 1.0,
                dy: 2.0,
            }))
        }

        pub(super) fn axis() -> ProtoEvent {
            ProtoEvent::Input(InputEvent::Pointer(PointerEvent::Axis {
                time: 0,
                axis: 0,
                value: 1.0,
            }))
        }

        pub(super) fn axis_discrete() -> ProtoEvent {
            ProtoEvent::Input(InputEvent::Pointer(PointerEvent::AxisDiscrete120 {
                axis: 0,
                value: 120,
            }))
        }

        pub(super) fn button() -> ProtoEvent {
            ProtoEvent::Input(InputEvent::Pointer(PointerEvent::Button {
                time: 0,
                button: 0x110,
                state: 1,
            }))
        }

        pub(super) fn key() -> ProtoEvent {
            ProtoEvent::Input(InputEvent::Keyboard(KeyboardEvent::Key {
                time: 0,
                key: 30,
                state: 1,
            }))
        }

        pub(super) fn modifiers() -> ProtoEvent {
            ProtoEvent::Input(InputEvent::Keyboard(KeyboardEvent::Modifiers {
                depressed: 0x01 | 0x02,
                latched: 0,
                locked: 0,
                group: 0,
            }))
        }

        pub(super) fn enter() -> ProtoEvent {
            ProtoEvent::Enter(Position::Left)
        }

        pub(super) fn leave() -> ProtoEvent {
            ProtoEvent::Leave(42)
        }

        pub(super) fn ack() -> ProtoEvent {
            ProtoEvent::Ack(42)
        }

        pub(super) fn hello() -> ProtoEvent {
            ProtoEvent::hello(*b"deadbeef")
        }

        pub(super) fn ping() -> ProtoEvent {
            ProtoEvent::Ping
        }

        pub(super) fn pong() -> ProtoEvent {
            ProtoEvent::Pong(true)
        }
    }

    #[test]
    fn route_input_default_motion_datagram_keyboard_stream() {
        use route_input_fixtures::*;
        let cfg = InputChannelConfig::default();
        assert_eq!(cfg.mouse_button, ChannelMode::Datagram);
        assert_eq!(cfg.keyboard, ChannelMode::Stream);

        assert_eq!(route_input(&cfg, &motion()), Channel::Datagram);
        assert_eq!(route_input(&cfg, &axis()), Channel::Datagram);
        assert_eq!(route_input(&cfg, &axis_discrete()), Channel::Datagram);
        assert_eq!(route_input(&cfg, &button()), Channel::Datagram);

        assert_eq!(route_input(&cfg, &key()), Channel::StreamB);
        assert_eq!(route_input(&cfg, &modifiers()), Channel::StreamB);

        assert_eq!(route_input(&cfg, &enter()), Channel::StreamA);
        assert_eq!(route_input(&cfg, &leave()), Channel::StreamA);
        assert_eq!(route_input(&cfg, &ack()), Channel::StreamA);
        assert_eq!(route_input(&cfg, &hello()), Channel::StreamA);
        assert_eq!(route_input(&cfg, &ping()), Channel::StreamA);
        assert_eq!(route_input(&cfg, &pong()), Channel::StreamA);
    }

    #[test]
    fn route_input_all_stream_motion_still_datagram() {
        use route_input_fixtures::*;
        let cfg = InputChannelConfig {
            mouse_button: ChannelMode::Stream,
            keyboard: ChannelMode::Stream,
        };

        assert_eq!(
            route_input(&cfg, &motion()),
            Channel::Datagram,
            "Motion 永远走 Datagram，不受 cfg.mouse_button 影响"
        );
        assert_eq!(route_input(&cfg, &axis()), Channel::Datagram);
        assert_eq!(route_input(&cfg, &axis_discrete()), Channel::Datagram);
        assert_eq!(route_input(&cfg, &button()), Channel::StreamB);
        assert_eq!(route_input(&cfg, &key()), Channel::StreamB);
        assert_eq!(route_input(&cfg, &modifiers()), Channel::StreamB);
        assert_eq!(route_input(&cfg, &enter()), Channel::StreamA);
        assert_eq!(route_input(&cfg, &ack()), Channel::StreamA);
    }

    #[test]
    fn route_input_all_datagram_everything_datagram() {
        use route_input_fixtures::*;
        let cfg = InputChannelConfig {
            mouse_button: ChannelMode::Datagram,
            keyboard: ChannelMode::Datagram,
        };

        assert_eq!(route_input(&cfg, &motion()), Channel::Datagram);
        assert_eq!(route_input(&cfg, &axis()), Channel::Datagram);
        assert_eq!(route_input(&cfg, &axis_discrete()), Channel::Datagram);
        assert_eq!(route_input(&cfg, &button()), Channel::Datagram);
        assert_eq!(route_input(&cfg, &key()), Channel::Datagram);
        assert_eq!(route_input(&cfg, &modifiers()), Channel::Datagram);

        assert_eq!(route_input(&cfg, &enter()), Channel::StreamA);
        assert_eq!(route_input(&cfg, &leave()), Channel::StreamA);
        assert_eq!(route_input(&cfg, &ack()), Channel::StreamA);
        assert_eq!(route_input(&cfg, &hello()), Channel::StreamA);
        assert_eq!(route_input(&cfg, &ping()), Channel::StreamA);
        assert_eq!(route_input(&cfg, &pong()), Channel::StreamA);
    }

    #[test]
    fn route_input_mixed_mouse_stream_keyboard_datagram() {
        use route_input_fixtures::*;
        let cfg = InputChannelConfig {
            mouse_button: ChannelMode::Stream,
            keyboard: ChannelMode::Datagram,
        };

        assert_eq!(route_input(&cfg, &motion()), Channel::Datagram);
        assert_eq!(route_input(&cfg, &axis()), Channel::Datagram);
        assert_eq!(route_input(&cfg, &axis_discrete()), Channel::Datagram);

        assert_eq!(route_input(&cfg, &button()), Channel::StreamB);

        assert_eq!(route_input(&cfg, &key()), Channel::Datagram);
        assert_eq!(
            route_input(&cfg, &modifiers()),
            Channel::Datagram,
            "Modifier 必须跟 Key 同通道（避免 modifier / key 跨通道时序错位）"
        );

        assert_eq!(route_input(&cfg, &enter()), Channel::StreamA);
        assert_eq!(route_input(&cfg, &leave()), Channel::StreamA);
        assert_eq!(route_input(&cfg, &ack()), Channel::StreamA);
        assert_eq!(route_input(&cfg, &hello()), Channel::StreamA);
        assert_eq!(route_input(&cfg, &ping()), Channel::StreamA);
        assert_eq!(route_input(&cfg, &pong()), Channel::StreamA);
    }
}