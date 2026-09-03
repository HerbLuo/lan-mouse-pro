//! QUIC 传输抽象层 —— M1 入口。
//!
//! 本模块把 UDP socket 包装成 [`quinn::Endpoint`]，并定义与对端的一路
//! QUIC 会话 [`PeerSession`]。完整生命周期由 STEP-1.x ~ STEP-8.x 逐步
//! 填实。
//!
//! 本文件（`mod.rs`）只承担 **公共表面 + 跨模块错误类型**：
//!
//! - [`Error`] / [`Result`] —— 所有子模块共用的传输层错误类型
//! - [`ALPN_LAN_MOUSE`] —— ALPN 协议名（`endpoint.rs` 和 `tls.rs` 都用）
//! - `pub use` 重导出 —— 把 5 个子模块（`endpoint` / `tls` / `protocol` /
//!   `streams` / `session`）的公共 API 拍平到 `quic_transport::xxx` 路径
//!   下，保持外部 caller (`connect.rs` / `listen.rs` / `service.rs` /
//!   `lib.rs` / `tests/quic_smoke.rs`) 完全不需要改
//!
//! STEP 演进历史见各子模块的 docstring。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;

use lan_mouse_proto::{MAX_EVENT_SIZE, ProtoEvent};

pub use quinn::{Connection, Endpoint};

pub(crate) const ALPN_LAN_MOUSE: &[u8] = b"lan-mouse";

pub mod endpoint;
pub mod protocol;
pub mod session;
pub mod streams;
pub mod tls;

// 重导出各子模块的公共 API —— 让外部 caller 写 `quic_transport::xxx` 即可，
// 不需要直接访问子模块。外部 API 与拆分前完全一致。
pub use crate::quic_transport::{
    endpoint::{
        accept, dial, dial_any, endpoint, endpoint_with_cert, endpoint_with_verifier,
        install_crypto_provider,
    },
    protocol::{
        Channel, HELLO_TIMEOUT, client_hello, read_any_frame, read_frame, route_input, server_hello,
    },
    session::{PeerRole, PeerSession, should_retry_after_close},
    streams::StreamBunch,
    tls::{
        AuthorizedKeysVerifier, PermissiveClientCertVerifier, TofuVerifier,
        build_quic_client_config,
    },
};

/// M1 传输层错误。
///
/// STEP-1.4 引入：占位变体 [`Error::NotImplemented`] 保留；新增
/// [`Error::Io`] / [`Error::Bind`] / [`Error::EndpointSetup`] 给 `endpoint()`
/// 路径用。
/// STEP-3.2 新增 [`Error::HelloFailed`] / [`Error::HelloTimeout`] 给应用层
/// Hello 握手用。
/// STEP-5.1 新增 [`Error::Datagram`] / [`Error::DatagramFallback`]；
/// STEP-5.2 新增 [`Error::StreamB`]（替换 [`Error::DatagramFallback`]，
/// SUGGESTION #S-14 治理落地）+ [`Error::FrameTooLarge`] /
/// [`Error::Truncated`]（codec 边界守护）。
///
/// **位置**：`mod.rs`（跨模块错误类型）—— 所有子模块都 `use super::Error`。
/// `#[from]` 派生绑定的 `quinn::{ConnectError, ConnectionError,
/// SendDatagramError}` 与 `crate::crypto::Error` 都通过 `super::Error` 透明
/// 转换。
#[derive(Debug, Error)]
pub enum Error {
    #[error("not implemented (STEP-1.3 占位)")]
    NotImplemented,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("bind {addr} failed: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    #[error("endpoint setup failed: {0}")]
    EndpointSetup(String),
    #[error("rustls / quic client config failed: {0}")]
    ClientConfig(String),
    /// `Endpoint::connect_with(...)` 同步失败 —— endpoint 关闭 / 远端地址非法 /
    /// 当前 endpoint 未配 client config（PLAN §2.2）。
    #[error("connect_with failed: {0}")]
    Connect(#[from] quinn::ConnectError),
    /// QUIC TLS 1.3 握手失败 —— 证书校验不通过 / ALPN 不匹配 / 中断等
    /// （PLAN §2.2）。`ConnectionError` 含 LocallyClosed / RemoteClosed /
    /// TransportError / ApplicationClosed 等子类；STEP-2.6 TofuVerifier 替
    /// 换占位 verifier 后，`rustls::Error::General("untrusted peer ...")`
    /// 会以 `ConnectionError::TransportError(...)` 形态冒到这里。
    #[error("handshake failed: {0}")]
    Handshake(#[from] quinn::ConnectionError),
    /// 应用层 Hello 握手失败：magic 不匹配 / 协议层错误 / 解码失败 /
    /// 收到非 Hello 帧等。消息含具体原因（"wrong magic: ..." /
    /// "non-Hello message: / " decode frame: ..."）。
    /// STEP-3.2 引入。
    #[error("hello handshake failed: {0}")]
    HelloFailed(String),
    /// Hello 握手超时（对端在 [`HELLO_TIMEOUT`] 内未完成 stream A 上的
    /// magic 交换）。STEP-3.2 引入。
    #[error("hello handshake timed out after {0:?}")]
    HelloTimeout(Duration),
    /// QUIC datagram 发送失败（STEP-5.1 引入）。
    ///
    /// 包装 [`quinn::SendDatagramError`] —— 包含 `UnsupportedByPeer` /
    /// `Disabled` / `TooLarge` / `ConnectionLost` 四种。**`ConnectionLost`
    /// 是连接已死，降级到 stream 也救不回来**，调用方需要据此决策是否上报
    /// `Error::Handshake`（TODO M2 接入 connect.rs 时细化）；其他三种
    /// 是"这条路走不通"，由 `send_datagram_or_stream_b` 内部兜底到
    /// stream B 路径，不冒到这里。
    #[error("datagram send failed: {0}")]
    Datagram(#[from] quinn::SendDatagramError),
    /// 降级到 stream uni 时的 IO 错误（STEP-5.1 引入，**临时**）。
    ///
    /// STEP-5.1 的降级路径是 inline `open_uni() + write_all() + finish()`，
    /// 不复用 STEP-5.2 才定义的 stream B cache + 长度前缀帧。本变体仅
    /// 承载降级 IO 错误（含 `open_uni` 的 `ConnectionError` /
    /// `write_all` 的 `WriteError` / `finish` 的 `ClosedStream`），STEP-5.2
    /// 落地后会被 [`Error::StreamB`] 替换（与 bak
    /// `mousehop/src/quic_transport.rs:564 Error::StreamB(format!("open_bi: {e}"))`
    /// 形态对齐）。
    #[error("datagram fallback stream io error: {0}")]
    DatagramFallback(String),
    /// Stream B（input 流）建立或写入失败（STEP-5.2 引入，**替换**
    /// [`Error::DatagramFallback`]，SUGGESTION #S-14 治理落地）。
    ///
    /// 消息前缀区分两个阶段（`"open_bi: ..."` / `"write frame length: ..."` /
    /// `"write: ..."`）—— 底层类型不同（`ConnectionError` vs `WriteError`），
    /// 收敛成 `String` 避免为一条降级路径加两个两个版本体；与 bak
    /// `mousehop/src/quic_transport.rs:1035-1040 Error::StreamB` 完全对齐。
    #[error("stream B: {0}")]
    StreamB(String),
    /// 帧长度字段超过 [`MAX_EVENT_SIZE`] 上限（[`read_frame`] 专用，STEP-5.2
    /// 引入）。
    ///
    /// 攻击者控制长度前缀字段时会诱使 `read_exact(&mut buf[..len])` 读
    /// 非常多字节（DoS 攻击向量）；本变体让 `read_frame` 在读到超限长度
    /// 时立即返回错误，避免 OOM / 慢速读。消息含超限的 `len` 值方便
    /// 上层诊断。
    ///
    /// 与 bak `mousehop/src/quic_transport.rs:1063-1071 Error::FrameTooLarge`
    /// 完全对齐（PLAN §5.2 验收清单要求）。
    #[error("frame too large: {0} bytes (max {MAX_EVENT_SIZE})")]
    FrameTooLarge(usize),
    /// 帧 body 在 [`read_frame`] 内被截断（STEP-5.2 引入）。
    ///
    /// 当 `read_exact` 因为流提前关闭（quinn `UnexpectedEof` / `ClosedStream`）
    /// 而读到 < `len` 字节时返回 —— 与解码失败（`Error::HelloFailed`）和
    /// 长度字段超限（[`Error::FrameTooLarge`]）**语义区分**：本变体表示
    /// "对端在帧内半途关流"（可能是恶意 / 也可能是 peer 崩溃），是
    /// fatal —— read_loop 看到本错误应关 conn + 整体退出，不做
    /// "skip frame" 续读（与 bak `frame_truncated_rejected` 测试一致）。
    #[error("frame body truncated")]
    Truncated,
    /// crypto.rs 错误透传 —— 主要由 `crypto::rustls_server_config` /
    /// `rustls_server_config_with_verifier` 失败冒上来（证书解析 / 链构
    /// 建失败 / rcgen 自签 cert 失败等）。STEP-6.2 引入。
    #[error("crypto: {0}")]
    Crypto(#[from] crate::crypto::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

// 抑制 `use` 中的 `Arc` / `ProtoEvent` 警告 —— 本文件顶层不再直接引用，但
// 保留 import 占位方便后续修改。
#[allow(unused_imports)]
use {Arc as _Arc, ProtoEvent as _ProtoEvent};

#[cfg(test)]
pub(crate) mod test_helpers {
    //! 跨子模块测试 helpers —— 5 个子模块的 `mod tests` 通过
    //! `use crate::quic_transport::test_helpers::*;` 共享本模块的辅助函数与宏。

    use std::sync::atomic::{AtomicU64, Ordering};

    use quinn::Endpoint;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
    use std::net::SocketAddr;
    use std::path::PathBuf;

    use crate::crypto;

    /// `local_set_test!` 把测试体包在 `LocalSet::run_until` 里，让
    /// `spawn_local` / `JoinSet::spawn_local` 在单元测试中也能用。
    ///
    /// **为什么 multi_thread flavor**：multi_thread runtime 让 Quinn I/O
    /// driver / server task 等 `Send` future 在独立 worker thread 跑，LocalSet
    /// 单独跑 main future + `spawn_local` 任务，避免 current_thread 下所有
    /// `Send` 任务排在 main future 之后（server task 还没起来 client 就 dial
    /// 完成 → handshake timeout）。需要 tokio `rt-multi-thread` feature。
    /// Run `$body` inside a fresh `LocalSet`, awaiting it inline.
    ///
    /// The caller is expected to be inside a `#[tokio::test]` `async fn`.
    /// The macro emits a single expression statement (no inner fn), so it
    /// does not produce `unnameable_test_items` / `dead_code` warnings.
    macro_rules! local_set_test {
        ($name:ident, $body:block) => {
            tokio::task::LocalSet::new()
                .run_until(async move $body)
                .await
        };
    }

    /// 测试用临时自签 cert —— 落盘到 `/tmp` 下 ephemeral 子目录（PID + nanos
    /// + 全局 counter 三重隔离），避免污染用户 cert 路径（`crypto::cert_path()`
    ///   / `key_path()`），并让并行跑的多个 test 互不踩同一目录。
    ///   返回 `(cert_chain, key)`，DER 字节直接喂给 `endpoint_with_cert` /
    ///   `build_quic_client_config`。
    pub(crate) fn ephemeral_cert() -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "lan-mouse-quic-test-{}-{}-{}",
            std::process::id(),
            nanos,
            n
        ));
        let cp = dir.join("cert.pem");
        let kp = dir.join("key.pem");
        crypto::generate_self_signed("lan-mouse-test", &cp, &kp).expect("test cert 自签")
    }

    /// 测试用临时 TOFU pins 目录 —— 与 `ephemeral_cert()` 同三重隔离（PID +
    /// nanos + counter）。
    pub(crate) fn ephemeral_pins_dir() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "lan-mouse-quic-pins-{}-{}-{}",
            std::process::id(),
            nanos,
            n
        ))
    }

    /// 测试用 server endpoint 装配 —— 直接调公共 `endpoint_with_cert`
    /// （STEP-2.4 起不再内联；测试 helper 与生产路径共用一条代码路径）。
    pub(crate) fn endpoint_with_test_cert(
        addr: SocketAddr,
        cert_chain: Vec<CertificateDer<'static>>,
        key: PrivateKeyDer<'static>,
    ) -> crate::quic_transport::Result<Endpoint> {
        crate::quic_transport::endpoint_with_cert(addr, cert_chain, key)
    }

    /// 构造 ServerName 用于 verifier 测试。localhost 在所有平台都是合法 DNS name。
    pub(crate) fn test_server_name() -> ServerName<'static> {
        ServerName::try_from("localhost").expect("localhost is a valid DNS name")
    }

    /// 临时 pins_dir helper（与 `ephemeral_cert()` 风格对称）。返回
    /// `(dir, owned_path)` —— `dir` 在 test 期间自动清理。
    pub(crate) fn tmp_pins_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lan-mouse-tofu-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create pins dir");
        dir
    }

    /// 临时 allowlist helper（与 `tmp_pins_dir` 风格对称）。
    pub(crate) fn tmp_allowlist(
        tag: &str,
    ) -> std::sync::Arc<std::sync::RwLock<std::collections::HashMap<String, String>>> {
        let dir = std::env::temp_dir().join(format!(
            "lan-mouse-allowlist-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create allowlist dir");
        std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()))
    }

    /// 测试用 Motion 事件（STEP-5.1 引入）。
    pub(crate) fn motion_event() -> lan_mouse_proto::ProtoEvent {
        lan_mouse_proto::ProtoEvent::Input(input_event::Event::Pointer(
            input_event::PointerEvent::Motion {
                time: 4242,
                dx: 12.5,
                dy: -7.25,
            },
        ))
    }

    /// 测试用 server endpoint 装配 helper（STEP-5.1 引入）。
    pub(crate) fn motion_test_server(
        cert: Vec<CertificateDer<'static>>,
        key: PrivateKeyDer<'static>,
    ) -> (Endpoint, SocketAddr) {
        let ep = crate::quic_transport::endpoint_with_cert(
            std::net::SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, 0).into(),
            cert,
            key,
        )
        .expect("server endpoint bind");
        let addr = ep.local_addr().expect("server addr");
        (ep, addr)
    }

    /// 测试用键盘按键事件（STEP-5.3 引入）。
    pub(crate) fn key_event() -> lan_mouse_proto::ProtoEvent {
        lan_mouse_proto::ProtoEvent::Input(input_event::Event::Keyboard(
            input_event::KeyboardEvent::Key {
                time: 0,
                key: 30,  // 'a'
                state: 1, // press
            },
        ))
    }

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    // 把宏重导出到 crate 范围，让子模块 `use crate::quic_transport::test_helpers::local_set_test;`
    // 后能调 `local_set_test!(name, { body })`。
    pub(crate) use local_set_test;
}
