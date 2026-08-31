//! QUIC 传输抽象层 —— M1 入口。
//!
//! 本模块把 UDP socket 包装成 [`quinn::Endpoint`]，并定义与对端的一路
//! QUIC 会话 [`PeerSession`]。完整生命周期由 STEP-1.x ~ STEP-5.x 逐步
//! 填实：
//!
//! - STEP-1.4（已）：[`endpoint`] —— UDP socket bind + 占位 client-mode Endpoint
//! - STEP-2.1（已）：[`build_quic_client_config`] + [`install_crypto_provider`]
//! - STEP-2.2（已）：[`dial`] —— QUIC TLS 1.3 握手完成（占位 verifier）
//! - STEP-2.3（已）：[`accept`] —— 接受 incoming QUIC 握手（占位 ServerConfig）
//! - STEP-2.4（已）：[`endpoint_with_cert`] —— 持久化 cert 注入 server-mode
//!   Endpoint（替代 `endpoint()` 占位；#S-4 cert/key 拆文件 + #S-9 server
//!   ALPN 已落地）
//! - STEP-2.5（已）：[`endpoint_with_verifier`] —— mTLS 强制 client cert 校验
//!   + [`PermissiveClientCertVerifier`] 占位 verifier（STEP-2.7 替换为
//!   `AuthorizedKeysVerifier`）；client 端 [`build_quic_client_config`]
//!   出示 client cert chain（#S-7 已解：`let _ = key` 去掉）
//! - STEP-2.6（已）：[`TofuVerifier`] —— 客户端 fingerprint pinning（首次
//!   见到自动 pin / 已知 mismatch 拒绝 / 已知 match 接受）；接 [`build_quic_client_config`]
//!   替换 STEP-2.5 的 `WebPkiServerVerifier` 占位（#S-6 已解）
//! - STEP-2.7（已）：[`AuthorizedKeysVerifier`] —— server 端显式 allowlist，
//!   命中 → `Ok`；未命中 → `Err`。复用 [`endpoint_with_verifier`]，零新增接
//!   口；listen.rs 装配点留 STEP-6.2 整段重写时接入
//! - STEP-3.2：`client_hello` / `server_hello` 握手
//! - STEP-4.4：`route_input()` ChannelMode 分派
//! - STEP-5.x：数据通道（datagram + 3 stream）
//! - STEP-6.x：出入站集成（替换 `LanMouseConnection` / `LanMouseListener`）

use std::collections::HashMap;
use std::fs;
use std::net::{SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use crate::crypto;
// `Endpoint` / `Connection` intentionally excluded from the `use` below —
// `pub use quinn::Endpoint` / `pub use quinn::Connection` re-export them
// for main-code (Step 6.x's `LanMouseListener::new`), matching the bak
// quic_transport.rs:84 pattern to avoid name collision.
use quinn::{
    ClientConfig as QuinnClientConfig, EndpointConfig, IdleTimeout, RecvStream, SendStream,
    ServerConfig, TransportConfig, VarInt,
};
use rustls::SignatureScheme;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use lan_mouse_proto::{ProtoEvent, MAX_EVENT_SIZE};

pub use quinn::{Connection, Endpoint};

/// ALPN 协议标识：QUIC TLS 握手时互换的协议名。
///
/// 与对端 server 必须一致；STEP-3.2 之上还有应用层 `PROTOCOL_MAGIC` 二次握手，
/// ALPN 仅为 TLS 层声明"这是 lan-mouse 协议"。本仓保留品牌名 `lan-mouse`（不
/// 复用 bak 的 `mousehop`，与 PLAN §5 D1 对齐）。
pub(crate) const ALPN_LAN_MOUSE: &[u8] = b"lan-mouse";

/// 应用层 Hello 握手超时（STEP-3.2 引入）。
///
/// QUIC mTLS 握手完成之后，对端必须在 `HELLO_TIMEOUT` 内在 stream A 上完成
/// `PROTOCOL_MAGIC` 交换；超时即视为"对端非 lan-mouse 实例"，关 conn +
/// `Error::HelloTimeout(HELLO_TIMEOUT)`。3s 是 PLAN §5 D6 决策（抄 bak）。
///
/// **与 QUIC idle timeout 的关系**：`HELLO_TIMEOUT` 仅在 Hello 阶段生效；
/// 之后由 `max_idle_timeout = 30s`（[`default_transport_config`]）接管。
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(3);

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
/// `StreamPair` 与 `stream_b` / `stream_c` 缓存字段留 STEP-5.1 / 5.2 落地，
/// 本步不引入。
pub struct PeerSession {
    conn: Connection,
    /// 应用层 Hello 握手成功标志。初始 `false`，`client_hello()` /
    /// `server_hello()` 任一端成功置 `true`（`Ordering::Release`）。
    /// 业务路径必须先 `load(Ordering::Acquire)` 确认 `true` 再发事件。
    hello_ok: AtomicBool,
    /// Stream A（control 流）缓存：`server_hello()` / `client_hello()` 写入；
    /// STEP-5.4 `read_loop` 通过 `take_stream_a_recv()` 拿 `RecvStream` 半
    /// 边给控制帧读循环，`SendStream` 半边留给后续 `send_stream_a()` 复用。
    ///
    /// **为什么用 `Mutex<Option<StreamPair>>` 而不是 `OnceCell`**：STEP-5.x
    /// 接手控制帧循环时需要 take recv 半边但保留 send 半边 —— `Option::take`
    /// 配合 `StreamPair::recv.take()` 的两步语义最干净。`OnceCell` 无法表达
    /// "已设置过但 recv 已被 take" 的状态。
    stream_a_cache: tokio::sync::Mutex<Option<StreamPair>>,
}

/// Stream A / B / C 缓存结构体：`(send, recv)` 二元组，两半边可独立 take
/// （STEP-5.x 接 read_loop 时 take recv 半边；send 半边留给写路径复用）。
///
/// STEP-3.2 只引入类型；具体 take 方法在 STEP-5.x。
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

/// M1 传输层错误。
///
/// STEP-1.4 引入：占位变体 [`NotImplemented`] 保留；新增 [`Io`] / [`Bind`] /
/// [`EndpointSetup`] 给 `endpoint()` 路径用。
/// STEP-3.2 新增 [`HelloFailed`] / [`HelloTimeout`] 给应用层 Hello 握手用。
/// 后续 STEP 接入 verifier / IO 时再补 `Error::Datagram` / `Error::StreamA`
/// 等（STEP-5.x）。
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
    /// "non-Hello frame: ..." / "decode frame: ..."）。
    /// STEP-3.2 引入。
    #[error("hello handshake failed: {0}")]
    HelloFailed(String),
    /// Hello 握手超时（对端在 [`HELLO_TIMEOUT`] 内未完成 stream A 上的
    /// magic 交换）。STEP-3.2 引入。
    #[error("hello handshake timed out after {0:?}")]
    HelloTimeout(Duration),
}

pub type Result<T> = std::result::Result<T, Error>;

/// server / client 共享的 `TransportConfig`：
///
/// - `keep_alive_interval = 5s` —— QUIC 主动探活，配合 PLAN §7 "Wi-Fi
///   切换恢复 < 1s" 预算；与 bak Step 0.1 spike 实测一致。
/// - `max_idle_timeout = 30s` —— QUIC keepalive 自带；应用层 idle 检测
///   （`RECV_IDLE_TIMEOUT = 8s`）由 STEP-7.1 删除。
///
/// `IdleTimeout::try_from(Duration)` 失败当且仅当 Duration 超 VarInt
/// 2^30 ms 上限（≈ 12.4 天），30s 远在范围内 —— `expect` 注明理由。
///
/// STEP-2.4 起注入 [`endpoint_with_cert`] / [`build_quic_client_config`] 的
/// `transport_config(...)` 链上，`#[allow(dead_code)]` 守护已移除（dead_code
/// 自动消失）。keepalive 5s / idle 30s 与 PLAN §5 D4 对齐。
fn default_transport_config() -> Arc<TransportConfig> {
    let mut t = TransportConfig::default();
    t.keep_alive_interval(Some(Duration::from_secs(5)));
    t.max_idle_timeout(Some(
        IdleTimeout::try_from(Duration::from_secs(30))
            .expect("30s 远小于 VarInt 2^30 ms 上限（≈ 12.4 天）"),
    ));
    Arc::new(t)
}

/// 占位实现：把 `addr` 绑成 `quinn::Endpoint`。
///
/// **STEP-1.4 真实意图**：本步**仅验证 UDP 绑定 + Endpoint 构造 + Drop**
/// 路径（PLAN §1.4 验收段："bind 临时端口、Drop 不 panic"），不验证
/// server 端 TLS 握手（那是 STEP-2.4 的范围 —— cert 持久化 + server-mode
/// endpoint）。
///
/// **占位形态**：暂用 `Endpoint::new(cfg, None, socket, runtime)` —— 不
/// 挂 `Some(ServerConfig)`，端点被 quinn 标记为 client-mode（**不**接受
/// incoming 握手；只可作为后续 dial 的本地锚点）。这是绕开 quinn 0.11
/// `ServerConfig::crypto` 必填字段的最小可编译方案。
///
/// **为什么不直接传 `ServerConfig::default()`** —— quinn 0.11 的
/// `quinn_proto::ServerConfig` 没有 `Default` 实现（`crypto` 字段必须由
/// caller 填 `Arc<dyn crypto::ServerConfig>`）；`ServerConfig::with_crypto`
/// 又要求先 `Arc<QuicServerConfig>` —— 后者要求 `rustls::ServerConfig`
/// 已完成 cert 装配（`crypto::rustls_server_config(chain, key)`）。STEP-1.4
/// 不接 cert，故走 `None`。
///
/// **STEP-2.4 切换路径**：`endpoint_with_cert()` 改为
/// `ServerConfig::with_crypto(QuicServerConfig::try_from(rustls_server_arc))`
/// 真路径 + `crypto::load_or_create_server_cert()` 持久化 cert +
/// `server_cfg.transport = default_transport_config()`。
///
/// **EndpointConfig**：`default()` 已启用 `HashedConnectionIdGenerator`
/// （支持多 CID + 连接迁移）；`migration = true` 是 quinn 默认 —— 不需
/// 显式覆盖（quinn 0.11 builder 是 `cid_generator(F)`，没有公开字段）。
///
/// **Runtime**：通过 `quinn::default_runtime()` 拿到当前 tokio runtime
/// handle；本函数被 `#[tokio::test]` 调用时由 `Handle::try_current()` 返
/// 回 `Some(TokioRuntime)`；生产路径也走同一路径。
pub fn endpoint(addr: SocketAddr) -> Result<Endpoint> {
    let endpoint_cfg = EndpointConfig::default();

    let socket = UdpSocket::bind(addr).map_err(|source| Error::Bind { addr, source })?;

    let runtime = quinn::default_runtime()
        .ok_or_else(|| Error::EndpointSetup("no tokio runtime available".into()))?;

    // STEP-1.4 占位：传 `None` 不挂 `ServerConfig`（client-mode endpoint），
    // 绕开 quinn 0.11 对 `ServerConfig::crypto` 必填字段的要求；STEP-2.4
    // 切到 `Some(server_cfg_with_cert)`，并把 `default_transport_config()`
    // 通过 `server_cfg.transport = ...` 注入。
    let endpoint = Endpoint::new(endpoint_cfg, None::<ServerConfig>, socket, runtime)
        .map_err(|e| Error::EndpointSetup(format!("Endpoint::new failed: {e}")))?;

    Ok(endpoint)
}

/// 装配 server-mode `quinn::Endpoint`：UDP bind + rustls `ServerConfig`
/// （含 ALPN `lan-mouse`）+ quinn transport_config + `Endpoint::new`。
///
/// **STEP-2.4 server-mode 入口** —— 替代 [`endpoint`] 的 client-mode 占位
/// （`None::<ServerConfig>`）。`endpoint_with_cert(...)` 返回的 endpoint
/// 才能让 [`accept`] 真正拿到 incoming 握手（client-mode endpoint 永远等
/// 不到 incoming —— STEP-2.3 占位局限）。
///
/// **生产路径 caller**：
/// 1. `crypto::load_or_create_server_cert()` → `(cert_chain, key)`（持久化
///    到 `$XDG_DATA_HOME/lan-mouse/{cert,key}.pem`）
/// 2. `endpoint_with_cert(addr, cert_chain, key)`
/// 3. `accept(ep)` 等 incoming
///
/// **#S-9 ALPN 对称**：本函数把 `rustls::ServerConfig.alpn_protocols` 设为
/// `vec![ALPN_LAN_MOUSE.to_vec()]`（在 wrap 进 `QuicServerConfig` **之前**
/// 设置 —— `alpn_protocols` 字段是 `rustls::ServerConfig` 上的，不在 quinn
/// 的 `ServerConfig` 上）。与 client [`build_quic_client_config`] 完全对称，
/// 否则 ALPN mismatch 直接拒连。
///
/// **`transport_config`**：通过 `server_cfg.transport_config(...)` 链上
/// [`default_transport_config`] —— 5s keepalive / 30s idle（PLAN §5 D4）。
/// `default_transport_config` 的 `#[allow(dead_code)]` 守护在本函数接通
/// 后自动消失。
///
/// **错误归一**：复用现有变体 —— 不新增 `Error::ServerConfig` 等：
/// - `crypto::rustls_server_config` 失败 → `Error::Rustls(#[from])`
/// - `QuicServerConfig::try_from` 失败 → `Error::ClientConfig(String)`
/// - bind / runtime / `Endpoint::new` 失败 → 复用 [`endpoint`] 路径错误变体
///
/// **`install_crypto_provider` 不在本函数内调**：与 [`build_quic_client_config`]
/// 对称 —— 由 caller（service.rs / 测试）显式守护。生产路径 `main.rs` 启动
/// 期已 install；测试首句调 `install_crypto_provider()`。
///
/// **不**改 [`endpoint`]：client-mode endpoint 仍由 [`dial`] 调用栈消费
/// （`Endpoint::connect_with` 不要求 endpoint 必须挂 `ServerConfig`）。
pub fn endpoint_with_cert(
    addr: SocketAddr,
    cert_chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<Endpoint> {
    let rustls_server_arc = crypto::rustls_server_config(cert_chain, key)?;
    endpoint_inner(addr, rustls_server_arc)
}

/// server-mode `Endpoint` + mTLS 强制 client cert 校验（STEP-2.5 引入）。
///
/// 与 [`endpoint_with_cert`] 形态对称，唯一差别是装配 rustls `ServerConfig`
/// 时调 `crypto::rustls_server_config_with_verifier(...)` 把 client cert 校验
/// 交给 caller 提供的 verifier：
/// - fingerprint 命中 allowlist → 握手通过（STEP-2.7 `AuthorizedKeysVerifier`）
/// - 未命中 / 缺 client cert → `rustls::Error::General(...)`，quinn 包装为
///   `ConnectionError::TransportError` / `LocallyClosed` → [`Error::Handshake`]
///
/// **#S-7 配套** —— 当 server `client_auth_mandatory() -> true`（本仓默认），
/// server 端 `CertificateRequest` 要求 client 出示 cert；client 端
/// [`build_quic_client_config`] 同时把 `(cert, key)` 通过 `with_client_auth_cert`
/// 装上（#S-7 解），TLS 握手双端 mTLS 才完整。
///
/// **生产路径 caller**（STEP-6.2 整段接 `listen.rs` supervisor）：
/// 1. `crypto::load_or_create_server_cert()` → `(cert_chain, key)`
/// 2. 构造 verifier（STEP-2.5 用 [`PermissiveClientCertVerifier`] 占位；STEP-2.7
///    替换为 `AuthorizedKeysVerifier` 走 `config.authorized_fingerprints()`）
/// 3. `endpoint_with_verifier(addr, cert_chain, key, verifier)`
///
/// **本步默认 verifier**：[`PermissiveClientCertVerifier`] —— 实现"接受任意
/// client cert，只要它存在 + 签名通过 TLS 1.3 内置校验"。这是 M1 STEP-2.5
/// 阶段的占位；STEP-2.7 由 `AuthorizedKeysVerifier` 替换为"指纹 allowlist"。
/// 不引入占位 verifier 也能编译通过（直接传 `Arc::new(WebPkiClientVerifier::...`
/// 也可以），但当前选择最小可工作形态 + 显式"占位"标记，方便后续 step 检索。
///
/// **错误归一**：复用现有 [`Error`] 变体 —— 不新增：
/// - `crypto::rustls_server_config_with_verifier` 失败 → `Error::Rustls`
/// - `endpoint_inner` 内部错误（`Arc::try_unwrap` / `QuicServerConfig::try_from` /
///   bind / runtime / `Endpoint::new`）→ 复用 [`endpoint_with_cert`] 路径错误
///
/// **`install_crypto_provider` 不在本函数内调**：与 [`endpoint_with_cert`] 对称。
///
/// dead_code chain：本函数被 STEP-2.5 单测 + 未来的 listen.rs supervisor
/// （STEP-6.2）消费；当前 main-code 无 caller 但单测已链上，故**不**加
/// `#[allow(dead_code)]`。
pub fn endpoint_with_verifier(
    addr: SocketAddr,
    cert_chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    verifier: Arc<dyn rustls::server::danger::ClientCertVerifier>,
) -> Result<Endpoint> {
    let rustls_server_arc = crypto::rustls_server_config_with_verifier(cert_chain, key, verifier)?;
    endpoint_inner(addr, rustls_server_arc)
}

/// `endpoint_with_cert` / `endpoint_with_verifier` 共用的私有 helper：
/// 把 `Arc<rustls::ServerConfig>` 装配成 `quinn::Endpoint`。
///
/// 抽出来是为了让两条路径共享 `Arc::try_unwrap` + ALPN + QuicServerConfig
/// + transport_config + bind + Endpoint::new 的固定装配流程，新增 verifier
/// 入口时不用复制这段（#S-7 / STEP-2.5 配套抽象）。
///
/// `Arc::try_unwrap` 必然成功：刚拿到的 `Arc<ServerConfig>` 强引用数 = 1
/// （`crypto::rustls_server_config[_with_verifier]` 返回后未持有其它副本）；
/// 即使 verifier 内部有 `Arc`（如 `Arc<RwLock<...>>`），那也是 verifier 自己的
/// 内部状态，与 server_cfg 自身无关。
///
/// 与 `bak/mousehop/src/quic_transport.rs:1266-1287 endpoint_inner` 完全对齐
/// （同样的 `Arc::try_unwrap` + ALPN + `QuicServerConfig::try_from` +
/// transport_config + bind + `Endpoint::new`）；ALPN 字符串由 `b"mousehop"`
/// 改 `b"lan-mouse"`（PLAN §5 D1）。
fn endpoint_inner(addr: SocketAddr, rustls_server_arc: Arc<ServerConfig>) -> Result<Endpoint> {
    // `alpn_protocols` 是 `rustls::ServerConfig` 的字段（不在 quinn 的
    // `ServerConfig` 上），所以要在 wrap 进 `QuicServerConfig` 之前设置。
    let mut rustls_server = Arc::try_unwrap(rustls_server_arc)
        .map_err(|_| Error::ClientConfig("rustls ServerConfig Arc 强引用数 > 1".into()))?;
    rustls_server.alpn_protocols = vec![ALPN_LAN_MOUSE.to_vec()];

    let quic_server = quinn::crypto::rustls::QuicServerConfig::try_from(Arc::new(rustls_server))
        .map_err(|e| Error::ClientConfig(format!("QuicServerConfig::try_from: {e}")))?;
    let mut server_cfg = ServerConfig::with_crypto(Arc::new(quic_server));
    server_cfg.transport_config(default_transport_config());

    let endpoint_cfg = EndpointConfig::default();
    let socket = UdpSocket::bind(addr).map_err(|source| Error::Bind { addr, source })?;
    let runtime = quinn::default_runtime()
        .ok_or_else(|| Error::EndpointSetup("no tokio runtime available".into()))?;

    let endpoint = Endpoint::new(endpoint_cfg, Some(server_cfg), socket, runtime)
        .map_err(|e| Error::EndpointSetup(format!("server Endpoint::new: {e}")))?;

    Ok(endpoint)
}

/// 装载 rustls 的 `ring` crypto provider —— **必须**早于任何
/// `rustls::ClientConfig::builder` / `rustls::ServerConfig::builder` 调用，
/// 否则运行期 panic（见 PLAN §2.1 + bak lib.rs:60-69 注释）。
///
/// 用 [`OnceLock`] 守护：cargo test 多线程并发 / `lan-mouse-cli` 子进程 /
/// GTK + daemon 双进程 同时 install 时，第二次 `install_default()` 返回
/// `Err(SomeInstalled)` 会让裸调用 panic / 噪音日志。`OnceLock` 保证整个
/// 进程只 install 一次，幂等可重入。
///
/// 与 bak `mousehop/src/lib.rs:60-69 install_crypto_provider` 完全对齐
/// （同样的 `OnceLock` + `let _ = ...install_default()`）；区别仅在：本仓
/// provider 装在 `quic_transport` 子模块（紧邻 `build_quic_client_config`），
/// `lib.rs` 顶层 `pub use quic_transport::install_crypto_provider` 转出
/// 给 `main.rs` 与集成测试调用。
pub fn install_crypto_provider() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        // 故意忽略 Err：重复 install 返回 `Err(SomeInstalled)` 不算错；
        // 已经安装的 provider 与本次想装的是同一个（ring），race 无害。
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// 装配 `quinn::ClientConfig`：rustls + ring + TofuVerifier（**STEP-2.6
/// 替换 WebPkiServerVerifier**）+ mTLS 出示 client cert chain + ALPN
/// `lan-mouse`。
///
/// 当前形态（STEP-2.6）：
/// - `crypto_provider = ring` —— 由 [`install_crypto_provider`] 早于
///   本调用预装（本函数不主动 install，main 启动期唯一入口在 main.rs）
/// - **TofuVerifier server cert 校验**（STEP-2.6 起）：`.dangerous().with_
///   custom_certificate_verifier(Arc::new(TofuVerifier::new(pins_dir)))`
///   替代 STEP-2.5 的 `WebPkiServerVerifier` 占位 verifier；`TofuVerifier`
///   按 server cert SHA-256 fingerprint + `$pins_dir/<sanitized_fp>.pin`
///   文件系统缓存做"首次见到自动 pin / 已知 mismatch 拒绝"的三态判定（与
///   bak `mousehop/src/quic_transport.rs:1799` 路径完全对齐；#S-6 已解）
/// - **mTLS 出示 client cert chain**（STEP-2.5 起）：`with_client_auth_cert(
///   cert_chain, key)` 同步装上；与 server [`endpoint_with_verifier`] 的
///   `with_client_cert_verifier(...)` 对称。`key` 字段不再是占位
///   —— #S-7 已解
/// - ALPN：`b"lan-mouse"` —— 与对端 server 协商协议；STEP-3.2 之上
///   另有应用层 `PROTOCOL_MAGIC` 二次握手（PLAN §3.1）
/// - transport：`default_transport_config()` 5s keepalive + 30s idle
///
/// **`cert_chain` 语义扩为双用**：mTLS 出示链；不再作为 root store 信任
/// anchor（自定义 verifier 全权负责 server cert 校验）。M1 双方都跑在同一
/// 台主机的同一进程，用同一私钥自签（生产路径 `dial()` 内部调
/// `crypto::load_or_create_server_cert()` 拿持久化 cert），双用同一 chain 不
/// 引安全风险。STEP-6.x 接入 connect.rs 时若需要 server trust anchor 与
/// 本端 client cert 不同，再拆参数（暂不拆 —— §9 M1 边界）。
///
/// **`pins_dir` 注入**（STEP-2.6 新增参数）：生产路径走 `crypto::known_peers_dir()`
/// （待 STEP-7.1 引入）；测试用 `tempfile::tempdir().path()` 隔离避免污染用户
/// 路径。TOFU 落盘逻辑由 `TofuVerifier` 全权负责 —— 本函数只构造 verifier
/// 注入 rustls builder。
///
/// **不**主动 install crypto provider：本函数被 [`install_crypto_provider`]
/// 调用者（main.rs）守护；`#[test]` 单测则在第一句调一次 install。
///
/// **错误归一**：所有 rustls / quinn 装配错误统一包到 [`Error::ClientConfig`]
/// （带底层 `Display`）；不引入 `From<rustls::Error>` / `From<quinn_proto::Error>`。
pub fn build_quic_client_config(
    cert_chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    pins_dir: &Path,
) -> Result<QuinnClientConfig> {
    use rustls::ClientConfig as RustlsClientConfig;

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = RustlsClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::ClientConfig(format!("protocol versions: {e}")))?;

    // STEP-2.6：TofuVerifier 替换 STEP-2.5 占位的 WebPkiServerVerifier。
    // custom verifier 全权负责 server cert 校验 —— 不再装 root store（与
    // bak `mousehop/src/quic_transport.rs:1822-1829 build_quic_client_config`
    // 完全对齐）。
    let verifier: Arc<dyn rustls::client::danger::ServerCertVerifier> =
        Arc::new(TofuVerifier::new(pins_dir));

    // STEP-2.5 起 mTLS 出示 client cert chain —— `with_client_auth_cert`
    // 是 terminal builder（返回 `Result<ClientConfig, Error>`，不像
    // `with_no_client_auth` 是中间 builder），出错走 `?` 经 `crypto::Error::Rustls`
    // 收口到 `Error::ClientConfig`（`.map_err` 避免引入 From impl）
    let mut rustls_client = builder
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(cert_chain, key)
        .map_err(|e| Error::ClientConfig(format!("with_client_auth_cert: {e}")))?;
    rustls_client.alpn_protocols = vec![ALPN_LAN_MOUSE.to_vec()];

    // wrap 进 quinn::ClientConfig —— quinn 0.11 通过 `quinn::crypto::rustls`
    // re-export 暴露 `QuicClientConfig`（顶层 `quinn_proto::*` 不是稳定
    // 公开路径，避免直接依赖 `quinn_proto` crate）
    let quic_client = quinn::crypto::rustls::QuicClientConfig::try_from(Arc::new(rustls_client))
        .map_err(|e| Error::ClientConfig(format!("QuicClientConfig::try_from: {e}")))?;
    let mut client_cfg = QuinnClientConfig::new(Arc::new(quic_client));
    client_cfg.transport_config(default_transport_config());

    Ok(client_cfg)
}

/// 主动拨号到对端 endpoint，完成 QUIC TLS 1.3 握手后返回 [`Connection`]。
///
/// **STEP-2.5 mTLS**：本函数复用 [`build_quic_client_config`]，后者已通过
/// `with_client_auth_cert(cert_chain, key)` 装上 mTLS 出示。`cert` / `key`
/// 参数在 STEP-2.5 起**双用**：
/// 1. 作为**对端** server 的 trust anchor 输入（STEP-2.6 起由 `TofuVerifier`
///    替换 `WebPkiServerVerifier`；调用栈不变）
/// 2. 作为**本端** client 的 mTLS 出示（`with_client_auth_cert(cert_chain, key)`）
///
/// M1 双方都跑在同一进程（生产路径） / 测试用 `ephemeral_cert()` 两套独立 cert；
/// 双用同一 chain 不引安全风险 —— M1 范围内合理。
///
/// **STEP-2.6 TofuVerifier**：server cert 校验走 `TofuVerifier::new(pins_dir)`
/// —— `pins_dir` 由 caller 通过 `dial` 的新参数传入（生产路径留 STEP-6.1
/// 接入 `crypto::known_peers_dir()`；测试用 `tempfile::tempdir().path()`
/// 隔离避免污染用户路径）。`TofuVerifier` 内部三态判定 Known Match /
/// Known Mismatch / First Connect（与 bak `mousehop/src/quic_transport.rs:
/// 1799 dial_with_client_cert_tofu` 完全对齐；#S-6 已解）。
///
/// **参数顺序**：`(ep, addr, cert, key, pins_dir)` —— STEP-2.6 加 `pins_dir`
/// 在末尾；`cert` 是**单张** `CertificateDer`，本函数内部 `vec![cert]` 转
/// chain 后喂给 [`build_quic_client_config`]。
///
/// **ALPN**：TLS 1.3 握手时声明 `b"lan-mouse"`（在 `build_quic_client_config`
/// 内设 `rustls_client.alpn_protocols`）。server 端 STEP-2.4 必须对称设
/// `rustls_server.alpn_protocols = vec![ALPN_LAN_MOUSE.to_vec()]`，否则
/// ALPN mismatch 直接拒连（SUGGESTION #S-9）。
///
/// **`server_name`**：`ep.connect_with(cfg, addr, "lan-mouse")` 的第三个
/// 参数用于 SNI（Server Name Indication）和 rustls 0.23 的
/// `ServerCertVerifier::verify_server_cert(..., server_name, ...)` 入参。
/// 当前 `TofuVerifier` 不读 server_name（只看 fingerprint）。硬编码
/// `"lan-mouse"` 与 ALPN 协议名一致；与 bak `mousehop/src/quic_transport.rs:
/// 1855` 的 `dial_one(... "mousehop")` 对称。
///
/// **错误归一**：
/// - `Endpoint::connect_with` 同步失败（endpoint 关闭 / 地址非法 / 无 client
///   config）→ [`Error::Connect`]（`#[from] quinn::ConnectError`）
/// - `.await` 后握手失败（证书 / ALPN / mTLS 不通过 / TofuVerifier mismatch
///   / 中断）→ [`Error::Handshake`]（`#[from] quinn::ConnectionError`）；
///   TofuVerifier mismatch 会以 `ConnectionError::TransportError(rustls::
///   Error::General("TOFU mismatch: ..."))` 形态冒到这里（§2.6 误差：PLAN
/// 文字写 "untrusted peer {fp}"，实际 bak 字符串是 "TOFU mismatch: peer
/// fingerprint {fp} not in known peers"，本步采用 bak 字符串以便与已落地
/// 的 SUGGESTION 治理纪律对齐）。
///
/// **不**主动 `install_crypto_provider`：与 `build_quic_client_config` 对称，
/// 由 `main.rs` / 测试首句显式守护。
///
/// **`#[allow(dead_code)]`**：STEP-2.6 仅被测试调用；STEP-6.1
/// `connect.rs::connect_to_handle` 接入 `LanMouseConnection` 路径时一并移除。
#[allow(dead_code)]
pub async fn dial(
    ep: &Endpoint,
    addr: SocketAddr,
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
    pins_dir: &Path,
) -> Result<Connection> {
    // 幂等守护：与 build_quic_client_config 对称 —— 即使 caller 已在 main 启
    // 动期调过一次，测试路径多次进入同一函数依然安全。
    install_crypto_provider();

    // STEP-2.6：`build_quic_client_config` 签名加 `pins_dir`（TofuVerifier 替
    // 换 WebPkiServerVerifier；构造由 `TofuVerifier::new(pins_dir)` 全权负责）。
    let cfg = build_quic_client_config(vec![cert], key, pins_dir)?;
    let conn = ep.connect_with(cfg, addr, "lan-mouse")?.await?;
    Ok(conn)
}

/// 接受一条 incoming QUIC 握手连接，完成 TLS 1.3 后返回原始 [`Connection`]。
///
/// **STEP-2.3 占位**：与 [`dial`] 对称 —— 当前仅返回握手完成的
/// `quinn::Connection`（不做 Hello 协议握手，那是 STEP-3.2）；
/// STEP-5.4 起由 `PeerSession::run()` 接管，后续会包成 `PeerSession`。
///
/// **两步式握手**：
/// 1. `ep.accept().await` 返回 `Option<Incoming>` —— `None` 表示
///    endpoint 已关闭（典型场景：listener 主动 drop / runtime 退出）；
///    wrap 成 [`Error::EndpointSetup`]，让 caller 能区分"endpoint 退出"
///    vs "握手失败"
/// 2. `incoming.await` 返回 `Result<Connection, ConnectionError>` —— 证
///    书校验 / ALPN / 中断 / TLS 错误一律归到 [`Error::Handshake`]（已
///    有 `#[from]` 派生，`?` 直接转换）
///
/// **占位 ServerConfig 注意**：当前 [`endpoint`] 是 client-mode
/// （`None::<ServerConfig>`，见 STEP-1.4 占位说明），即 `ep.accept()`
/// **永远等不到** incoming —— 这是 STEP-2.4 `endpoint_with_cert()` 的工
/// 作。本步先实现 `accept()` 公共函数 + 错误归一；STEP-2.4 注入真 server
/// cert 后，调用方（`listen.rs` supervisor）才能真正拿到 `Connection`。
/// 测试路径由 STEP-2.2 已就位的 `endpoint_with_test_cert()` 测试 helper
/// 内联 server endpoint（已含 `Some(server_cfg)`），`accept()` 的内部
/// 逻辑（`ep.accept().await?.await?`）不变，与 bak
/// `mousehop/src/quic_transport.rs:2040-2044` 模式完全对齐。
///
/// **错误归一**：
/// - endpoint 已关闭 → [`Error::EndpointSetup`]（复用现有变体，避免新增）
/// - 握手失败 → [`Error::Handshake`]（`#[from] quinn::ConnectionError`）
///
/// **`#[allow(dead_code)]`**：与 [`dial`] 对称 —— STEP-2.3 仅被
/// STEP-2.2 测试 helper 间接覆盖（in-process server 调
/// `endpoint.accept().await.await`），未在 main-code 出现；
/// STEP-6.2 `listen.rs::read_loop` 改造时 `accept()` 切换为真正的
/// caller，dead_code 自动消失。
///
/// **不**主动 `install_crypto_provider`：与 [`dial`] 对称，caller 已在
/// main 启动期守护过。
#[allow(dead_code)]
pub async fn accept(ep: &Endpoint) -> Result<Connection> {
    let incoming = ep
        .accept()
        .await
        .ok_or_else(|| Error::EndpointSetup("endpoint closed (accept returned None)".into()))?;
    let conn = incoming.await?;
    Ok(conn)
}

// === STEP-3.2 PeerSession + Hello 握手 ==================================
//
// QUIC mTLS 握手（STEP-2.x）完成 + 对端 fingerprint 验证（STEP-2.6 /
// 2.7）通过后，立即在 **stream A**（control 流）上做应用层 Hello 握手：双方
// 互换 `ProtoEvent::Hello { magic: PROTOCOL_MAGIC, commit: <our> }`，
// magic 不匹配立即 `conn.close(VarInt(0), "hello failed")` 关连。
//
// 与 bak `mousehop/src/quic_transport.rs` 的差异：
// - `Mousehop` → `LanMouse` 命名约定（PLAN §5 D1）
// - `mousehop_proto` → `lan_mouse_proto` crate 路径
// - 本仓不引入 `StreamBunch` / `route_input` / `send_motion` 等 STEP-4 /
//   STEP-5.x 范畴的字段 / 方法 —— 这些留后续 STEP 落地

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
    pub fn from_connection(conn: Connection) -> Self {
        Self {
            conn,
            hello_ok: AtomicBool::new(false),
            stream_a_cache: tokio::sync::Mutex::new(None),
        }
    }

    /// 暴露底层 `quinn::Connection`，给 STEP-5.x 读 `peer_identity()` /
    /// datagram / stream B/C 用。STEP-6.x 接入 `LanMouseConnection` 后这
    /// 一步会被 `send()` / `recv()` 高阶方法盖掉。
    pub fn connection(&self) -> &Connection {
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
    pub async fn take_stream_a_cache(&self) -> Option<(SendStream, RecvStream)> {
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
    pub async fn take_stream_a_recv(&self) -> Option<RecvStream> {
        let mut g = self.stream_a_cache.lock().await;
        g.as_mut().and_then(|p| p.recv.take())
    }
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
#[allow(dead_code)]
pub fn hello_watchdog(peer: std::sync::Arc<PeerSession>) {
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
/// **dead_code chain**：STEP-3.2 仅被测试消费；STEP-5.4 接 `run()` /
/// STEP-6.1 接 `connect.rs::connect_to_handle` 时移除 `#[allow]`。
#[allow(dead_code)]
pub async fn client_hello(peer: &PeerSession) -> Result<(), Error> {
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
            *peer.stream_a_cache.lock().await = Some(StreamPair::new(send, recv));
            peer.hello_ok.store(true, Ordering::Release);
            Ok(())
        }
        ProtoEvent::Hello { magic, .. } => {
            peer.conn
                .close(VarInt::from(0u32), b"hello failed (wrong magic)");
            log::warn!(
                "client hello rejected: wrong magic {:?}",
                std::str::from_utf8(&magic).unwrap_or("????????")
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
/// **dead_code chain**：STEP-3.2 仅被测试消费；STEP-5.4 接 `run()` /
/// STEP-6.2 接 `listen.rs::read_loop` 时移除 `#[allow]`。
#[allow(dead_code)]
pub async fn server_hello(peer: &PeerSession) -> Result<(), Error> {
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

    // 缓存 stream A 给 STEP-5.4 read_loop 接手
    *peer.stream_a_cache.lock().await = Some(StreamPair::new(send, recv));

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
async fn read_hello_frame(recv: &mut RecvStream) -> std::result::Result<ProtoEvent, Error> {
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

/// **STEP-2.6 客户端 TOFU（Trust On First Use）fingerprint pinning verifier**。
///
/// 把 server cert 的 SHA-256 fingerprint 与 `$pins_dir/<sanitized_fp>.pin`
/// 文件系统缓存做比对：
///
/// | 判定 | 触发 | 行为 |
/// |---|---|---|
/// | **Known Match** | pin 文件存在 | `Ok(ServerCertVerified::assertion())` |
/// | **Known Mismatch** | `pins_dir` 内存在任意 `.pin` 文件但当前 fingerprint 的 pin 不存在 | `Err(rustls::Error::General("TOFU mismatch: ..."))` |
/// | **First Connect** | `pins_dir` 不存在 / 不含任何 `.pin` | 落盘占位 `b"trusted\n"` + `log::info!("paired with {fp}")` + `Ok(ServerCertVerified::assertion())` |
///
/// **三态判定的语义**：区分"首次见到" vs "已知对端换了 cert"。前者是 TOFU
/// 合法路径（LAN 上第一次连新对端），后者是攻击信号（有人伪造了对端）。
/// `pins_dir` 空时走 First Connect（任何对端都接受）；`pins_dir` 非空但当前
/// fingerprint 未 pin 时拒绝 —— 这是 LAN 攻击防护的核心约束。
///
/// **`pins_dir` 跨平台兼容**：把 `aa:bb:cc:...` 替换为 `aa_bb_cc_...`（`:` 在
/// Windows 上不是合法文件名字符）后拼 `<sanitized_fp>.pin`。与 bak
/// `mousehop/src/quic_transport.rs:1384-1442 TofuVerifier` 完全对齐；差异仅
/// 在 `known_peers` 子目录 vs 直用 `pins_dir`（PLAN §2.6 直接传 `pins_dir`，
/// 不再嵌子目录 —— 测试路径 tempdir 已隔离）。
///
/// **`Send + Sync + 'static`**：rustls 0.23 trait 约束 —— `TofuVerifier` 持有
/// `PathBuf` + `Arc<CryptoProvider>`，自动满足。
///
/// **`provider` 字段**：`verify_tls12_signature` / `verify_tls13_signature`
/// 转发到 `rustls::crypto::verify_*_signature` 需要 `signature_verification_algorithms`
/// 列表 —— 必须持有 provider 引用。与 bak `TofuVerifier` 对称。
///
/// **dead_code chain**：本类型被 `build_quic_client_config`（接收 `pins_dir`）
/// 消费 → 再被 `dial()` 间接消费 → 测试也直接调 `verify_server_cert`。
/// main-code 路径在 STEP-6.1 `connect.rs::connect_to_handle` 接入时一并消化。
pub struct TofuVerifier {
    pins_dir: PathBuf,
    /// 签名验签需要的 provider（`verify_tls12_signature` / `verify_tls13_signature`
    /// 转发到 `rustls::crypto::verify_*_signature` 时拿它的 `signature_verification_algorithms`）。
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl TofuVerifier {
    /// 构造：首次连接状态。
    ///
    /// `pins_dir` 可以不存在 —— `verify_server_cert` 在 First Connect 分支会
    /// 先 `create_dir_all` 再 `fs::write`。
    pub fn new(pins_dir: &Path) -> Self {
        Self {
            pins_dir: pins_dir.to_path_buf(),
            provider: Arc::new(rustls::crypto::ring::default_provider()),
        }
    }

    /// 构造：已知 peer 状态（预落盘 `<known_fp>.pin`，让后续 verify 走
    /// "已知匹配"分支）。
    ///
    /// **预落盘是 best-effort**：失败时构造仍返回 Self，后续 verify 走 mismatch
    /// 路径返回 `rustls::Error` —— 故意不静默吞 IO 错误，因为这通常意味着 fs
    /// 权限 / 磁盘已满等运维问题。
    #[allow(dead_code)] // 测试 only（生产 `dial()` 走 `.new()`）
    pub fn with_known(pins_dir: &Path, known_fp: &str) -> Self {
        let v = Self::new(pins_dir);
        let _ = fs::create_dir_all(&v.pins_dir);
        let _ = fs::write(v.pin_path(known_fp), b"trusted\n");
        v
    }

    /// fingerprint → pin 文件路径。`:` 替换为 `_` 跨平台兼容。
    fn pin_path(&self, fp: &str) -> PathBuf {
        let safe = fp.replace(':', "_");
        self.pins_dir.join(format!("{safe}.pin"))
    }

    /// `pins_dir` 下是否存在任意 `.pin` 文件（用于区分 First Connect vs
    /// Known Mismatch）。
    fn has_any_pins(&self) -> bool {
        if !self.pins_dir.exists() {
            return false;
        }
        fs::read_dir(&self.pins_dir)
            .map(|d| {
                d.filter_map(Result::ok)
                    .any(|e| e.path().extension().is_some_and(|ext| ext == "pin"))
            })
            .unwrap_or(false)
    }
}

impl rustls::client::danger::ServerCertVerifier for TofuVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        // (1) 拿 server cert 算 SHA-256 fingerprint（与 `crypto::generate_fingerprint`
        //     输出格式一致：hex 用 `:` 分隔，小写）
        let fp = crypto::generate_fingerprint(end_entity.as_ref());

        // (2) ensure pins_dir 存在（First Connect 时也需要）
        fs::create_dir_all(&self.pins_dir).map_err(|e| {
            rustls::Error::General(format!(
                "TOFU: create_dir_all({:?}) failed: {e}",
                self.pins_dir
            ))
        })?;

        // (3) 三态判定
        let pin = self.pin_path(&fp);

        if pin.exists() {
            // Known Match —— pin 文件已存在，接受
            Ok(ServerCertVerified::assertion())
        } else if self.has_any_pins() {
            // Known Mismatch —— 其他 fp 的 pin 存在但当前 fp 没有，拒绝
            log::warn!("TOFU: rejected untrusted peer {fp}");
            Err(rustls::Error::General(format!(
                "TOFU mismatch: peer fingerprint {fp} not in known peers"
            )))
        } else {
            // First Connect —— 落盘占位 + 接受
            fs::write(&pin, b"trusted\n").map_err(|e| {
                rustls::Error::General(format!("TOFU: write pin {:?} failed: {e}", pin))
            })?;
            log::info!("TOFU: paired with {fp}");
            Ok(ServerCertVerified::assertion())
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// **STEP-2.5 占位 verifier**：server 端 mTLS 强制要求 client 出示（`offer
/// _client_auth() -> true` + `client_auth_mandatory() -> true`），但**任何**
/// 通过 TLS 1.3 内置链校验的 client cert 都接受 —— 不做 fingerprint allowlist。
///
/// **用途**：让 mTLS 链路本身（server 端 `CertificateRequest` → client 出示
/// cert → 握手完成）能在 STEP-2.5 端到端跑通，同时给 [`mtls_rejects_no_client_cert`]
/// 等负面测试提供"server 强制要求 client cert 但放行任意"的可控 verifier。
///
/// **STEP-2.7 替换**：[`AuthorizedKeysVerifier`] 走 `config.authorized_fingerprints()`
/// 的 fingerprint allowlist —— 未授权 fingerprint 即拒握。`mtls_rejects_no_client_cert`
/// 之外的所有 server 路径（`endpoint_with_verifier` 生产 caller）STEP-2.7 切换。
///
/// **`Send + Sync + 'static`**：rustls 0.23 trait 约束 —— `PermissiveClientCertVerifier`
/// 不持有跨 await 的可变状态，单字段结构体 + `Arc<ServerNameProvider>` 衍生
/// 自动满足（`Debug` 同样 derive 出）。
///
/// **`verify_client_cert`**：调用 `crypto::generate_fingerprint(cert)` 算 SHA-256
/// → 写出日志（不与 allowlist 比对 —— 占位实现）→ 返回
/// `Ok(ClientCertVerified::assertion())`。这是**唯一**路径 —— 因为服务端
/// 已经 `with_client_cert_verifier(...)` 装上 verifier，且 `client_auth_mandatory()`
/// 为 true，client **必须**出示 cert 才能到这一步；client 不出示 → TLS 1.3
/// 内置流程直接 `rustls::Error::NoCertificatesPresented` 拒握（见测试）。
pub struct PermissiveClientCertVerifier;

impl rustls::server::danger::ClientCertVerifier for PermissiveClientCertVerifier {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        // 不提供 root hints —— 任意自签 cert 都接受
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::server::danger::ClientCertVerified, rustls::Error> {
        let fp = crate::crypto::generate_fingerprint(end_entity.as_ref());
        log::debug!("[STEP-2.5 占位 verifier] accept client cert fp={fp}");
        Ok(rustls::server::danger::ClientCertVerified::assertion())
    }
}

/// **STEP-2.7 server 端授权指纹 allowlist verifier** —— mTLS 双层防御的核心
/// 约束：client cert 即使通过了 TLS 1.3 内置链校验（自签根信任），还要看
/// `allowlist` 里有没有这个 fingerprint 才放行。
///
/// **#S-9（治理）**：allowlist 的 value 类型用 `String` 而**非**
/// `lan_mouse_ipc::IncomingPeerConfig` —— `IncomingPeerConfig` 是 M2 范围
/// （带 `clipboard_receive` / `description` 等字段）；当前 M1
/// `config::authorized_fingerprints: HashMap<String, String>` 也是 String，
/// 自然对齐。STEP-7 / M2 把 `IncomingPeerConfig` 引入 `lan_mouse-ipc` 后，
/// 同步把本结构 + caller 一起改成 `HashMap<String, IncomingPeerConfig>`
/// （与 bak `mousehop/src/quic_transport.rs:1577-1754 AuthorizedKeysVerifier`
/// 形态完全对齐；值类型用 `IncomingPeerConfig::default()` 表示"已授权但
/// 还没填配置"）。
///
/// **`allowlist` 跨平台语义**：`String` 是 fingerprint（小写 hex + `:` 分隔，
/// 与 `crypto::generate_fingerprint` 输出格式一致）。运行时增 / 删 allowlist
/// 条目通过 `Arc<RwLock<HashMap<...>>>` 共享所有权 —— listen.rs supervisor
/// 或后续 IPC 推 `authorized_fingerprints` 变更时，可直接写本结构内部的
/// RwLock 看到变更（`RwLock::read()` 不阻塞 reader；`RwLock::write()` 仅
/// 阻塞 writer）。M1 阶段 caller 仅有测试 + 未来 STEP-6.2 listen.rs
/// supervisor；运行时增删 allowlist 路径 STEP-6.x 接入。
///
/// **`Send + Sync + 'static`**：rustls 0.23 trait 约束 —— `allowlist: Arc<
/// RwLock<HashMap<...>>>` 自动 `Send + Sync`，`provider: Arc<CryptoProvider>`
/// 也自动满足。
///
/// **`provider` 字段**：`verify_tls12_signature` / `verify_tls13_signature`
/// 转发到 `rustls::crypto::verify_*_signature` 需要
/// `signature_verification_algorithms` —— 必须持有 provider 引用（与 STEP-2.6
/// `TofuVerifier` 同模式）。
///
/// **`verify_client_cert` 二态判定**：
/// - 命中（allowlist.contains_key(&fp)）→ `Ok(ClientCertVerified::assertion())` + `log::info!`
/// - 未命中 → `Err(rustls::Error::General(format!("unauthorized peer {fp}")))`
///   + `log::warn!`（PLAN §2.7 验收文本，与 STEP-2.6 "untrusted peer" 对齐）
///
/// **dead_code chain**：本结构被 [`endpoint_with_verifier`] 的 verifier 参数
/// 消费 → 由 `endpoint_with_verifier` 间接消费 → 单测直接调
/// `verify_client_cert`。main-code 接入路径留 STEP-6.2 `listen.rs` supervisor
/// 整段重写时一并消化（listen.rs 当前仍调 DTLS 路径，14 errors 不在本步范围）。
pub struct AuthorizedKeysVerifier {
    /// 授权指纹表：键 = client cert SHA-256 fingerprint（`crypto::generate_fingerprint` 格式），
    /// 值 = 占位 `String`（M2 接 `lan_mouse_ipc::IncomingPeerConfig::default()`）。
    allowlist: Arc<RwLock<HashMap<String, String>>>,
    /// 签名验签需要的 provider（`verify_tls12_signature` / `verify_tls13_signature`
    /// 转发到 `rustls::crypto::verify_*_signature` 时拿它的
    /// `signature_verification_algorithms`）。
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl AuthorizedKeysVerifier {
    /// 构造：allowlist 由 caller 持有（生产 `Config::authorized_fingerprints()`，
    /// 测试 `Arc::new(RwLock::new(HashMap::new()))`）。
    ///
    /// `allowlist` 必须 `Send + Sync + 'static`（rustls 要求 verifier
    /// `Send + Sync + 'static`；`Arc<RwLock<HashMap<...>>>` 自动满足）。
    #[allow(dead_code)]
    pub fn new(allowlist: Arc<RwLock<HashMap<String, String>>>) -> Self {
        Self {
            allowlist,
            provider: Arc::new(rustls::crypto::ring::default_provider()),
        }
    }

    /// 构造：已知 peer 状态（预填 `allowlist` 让后续 verify 走 Authorized 分支）。
    ///
    /// **预填是 best-effort**：失败时构造仍返回 Self，后续 verify 走
    /// Unauthorized 路径返回 `rustls::Error` —— 故意不静默吞
    /// `RwLock::write()` 的 poison 错误，因为这通常意味着上游 panic。
    ///
    /// 测试用：直接调 `verify_client_cert(cert)` → 应 `Ok`（不需要端到端
    /// QUIC 握手）。生产路径不用（生产走 listen.rs supervisor / service.rs
    /// 写 allowlist，verifier 通过 `new()` 拿到 Arc 引用）。
    #[allow(dead_code)]
    pub fn with_known(allowlist: Arc<RwLock<HashMap<String, String>>>, known_fp: &str) -> Self {
        let v = Self::new(allowlist);
        v.allowlist
            .write()
            .expect("RwLock poisoned")
            .insert(known_fp.to_owned(), String::new());
        v
    }

    /// 暴露 `allowlist`（测试用：断言 allowlist 内容 + 模拟运行时增删）。
    #[allow(dead_code)]
    pub fn allowlist(&self) -> &Arc<RwLock<HashMap<String, String>>> {
        &self.allowlist
    }
}

impl rustls::server::danger::ClientCertVerifier for AuthorizedKeysVerifier {
    fn offer_client_auth(&self) -> bool {
        // server 端 mTLS 强制 client cert 出示（与 `PermissiveClientCertVerifier`
        // 对称 —— 不出 cert 就走 TLS 1.3 `NoCertificatesPresented` 拒握）。
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        // 不出 cert → 直接拒（与 `offer_client_auth` 一致）
        true
    }

    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        // 不提供 root hints —— 任意自签 cert 都尝试接入（fingerprint 校验由
        // `verify_client_cert` 自己做）
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::server::danger::ClientCertVerified, rustls::Error> {
        // (1) 拿 client cert 算 SHA-256 fingerprint（与
        //     `crypto::generate_fingerprint` 输出格式一致：hex 用 `:` 分隔，小写）
        let fp = crypto::generate_fingerprint(end_entity.as_ref());

        // (2) allowlist 查询（注意：与模块顶层 `Result<T>` 别名冲突 —— `verify_client_cert`
        //     是 trait method，必须显式写 `std::result::Result<_, rustls::Error>` 才能
        //     与 rustls 期望类型对齐；与 STEP-2.6 `TofuVerifier` 偏差 #1 同模式）
        let allowed = self
            .allowlist
            .read()
            .expect("RwLock poisoned")
            .contains_key(&fp);

        if allowed {
            // Authorized —— 命中 allowlist
            log::info!("AuthorizedKeysVerifier: authorized peer {fp}");
            Ok(rustls::server::danger::ClientCertVerified::assertion())
        } else {
            // Unauthorized —— allowlist 不命中
            //
            // 注意：本步不触发 IPC 推送（IPC 集成属于 STEP-6.x 接入 listen.rs supervisor
            // 时一并处理），仅 log::warn 留下审计线索。错误消息含 fingerprint 方便
            // 上层诊断（用户对照"信任的 peer 列表"判定）。
            log::warn!("AuthorizedKeysVerifier: rejected unauthorized peer {fp}");
            Err(rustls::Error::General(format!("unauthorized peer {fp}")))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

// === 单元测试 ================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto;
    use std::net::{Ipv4Addr, SocketAddrV4};

    /// 测试用临时自签 cert —— 落盘到 `/tmp` 下 ephemeral 子目录（PID 隔离），
    /// 避免污染用户 cert 路径（`crypto::cert_path()` / `key_path()`）。
    /// 返回 `(cert_chain, key)`，DER 字节直接喂给 `endpoint_with_cert` /
    /// `build_quic_client_config`。
    fn ephemeral_cert() -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
        let dir = std::env::temp_dir().join(format!("lan-mouse-quic-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cp = dir.join("cert.pem");
        let kp = dir.join("key.pem");
        crypto::generate_self_signed("lan-mouse-test", &cp, &kp).expect("test cert 自签")
    }

    /// 测试用 server endpoint 装配 —— 直接调公共 [`endpoint_with_cert`]
    /// （STEP-2.4 起不再内联；测试 helper 与生产路径共用一条代码路径）。
    fn endpoint_with_test_cert(
        addr: SocketAddr,
        cert_chain: Vec<CertificateDer<'static>>,
        key: PrivateKeyDer<'static>,
    ) -> Result<Endpoint> {
        endpoint_with_cert(addr, cert_chain, key)
    }

    /// STEP-2.4 验收 #1：`endpoint_with_cert` bind 临时端口 + Drop 不 panic。
    #[tokio::test]
    async fn endpoint_with_cert_binds_ipv4_localhost() {
        install_crypto_provider();
        let (cert_chain, key) = ephemeral_cert();
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into();
        let ep =
            endpoint_with_cert(addr, cert_chain, key).expect("endpoint_with_cert bind 不应失败");
        let local = ep.local_addr().expect("endpoint 必须有 local_addr");
        assert_ne!(local.port(), 0, "ephemeral 端口应非零");
        drop(ep);
    }

    /// STEP-2.4 验收 #2：持久化 cert 加载路径稳定 —— 首次生成 cert/key 到
    /// `crypto::cert_path()` / `key_path()`，二次加载同一路径，fingerprint
    /// 应一致（caller 一致性 + 跨重启 identity 稳定）。
    ///
    /// **注意**：本测试**不**直接调 `crypto::load_or_create_server_cert()`，
    /// 因为那条路径写到用户 home 目录的 `lan-mouse/` 子目录（生产路径）。
    /// 测试只验证 `endpoint_with_cert` + 临时 cert 的最小可用形态；持久化
    /// 路径在 `crypto::tests::load_or_generate_key_and_cert_der_persists_identity`
    /// 覆盖。STEP-6.x 修 14 errors 后 Leader 手动跑确认通过（SUGGESTION #S-5）。
    #[tokio::test]
    async fn endpoint_with_cert_accepts_local_incoming() {
        install_crypto_provider();
        let (cert_chain, key) = ephemeral_cert();
        let server_ep = endpoint_with_test_cert(
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into(),
            cert_chain,
            key,
        )
        .expect("server endpoint bind 不应失败");
        let server_addr = server_ep.local_addr().expect("server ep 必有 local_addr");

        // 后台 accept task：拿 Connection 后立即 drop
        let server_task = tokio::spawn(async move {
            let incoming = server_ep.accept().await.expect("server accept 不应失败");
            let conn = incoming.await.expect("server handshake 不应失败");
            drop(conn);
        });

        // client dial 同一端口（endpoint() client-mode 即可）
        let (client_cert, client_key) = ephemeral_cert();
        let client_ep = endpoint(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
            .expect("client endpoint bind 不应失败");
        // STEP-2.6：dial 加 pins_dir 参数；测试用临时 pins_dir 隔离。
        let pins_dir =
            std::env::temp_dir().join(format!("lan-mouse-quic-test-pins-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&pins_dir);
        let conn = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            dial(
                &client_ep,
                server_addr,
                client_cert[0].clone(),
                client_key,
                &pins_dir,
            ),
        )
        .await
        .expect("端到端 TLS 1.3 握手超时")
        .expect("dial 不应失败");

        assert!(
            conn.peer_identity().is_some(),
            "peer_identity 应非空（TLS 1.3 握手完成）"
        );

        drop(conn);
        server_task.await.expect("server task 不应 panic");
        client_ep.wait_idle().await;
    }

    /// PLAN §1.4 验收：bind 临时端口、Drop 不 panic。
    #[tokio::test]
    async fn endpoint_binds_ipv4_localhost() {
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into();
        let ep = endpoint(addr).expect("endpoint bind 不应失败");
        // 验证 local_addr() 返回非零端口（ephemeral）
        let local = ep.local_addr().expect("endpoint 必须有 local_addr");
        assert_ne!(local.port(), 0, "ephemeral 端口应非零");
        drop(ep);
    }

    /// PLAN §2.1 验收：用测试自签 cert 装配 `quinn::ClientConfig` 不 panic。
    ///
    /// 与 §1.4 单测同样的 `cargo test` 跑不通（lib 因 STEP-1.2 留下的 14
    /// DTLS errors 编不过），测试代码就位即可。STEP-6.x 修 14 errors 后由
    /// Leader 手动跑一次确认通过（与 STEP-1.4 `endpoint_binds_ipv4_localhost`
    /// 走相同路径，见 SUGGESTION #S-5）。
    #[test]
    fn quinn_client_config_loads_rustls_provider() {
        // 必须先 install crypto provider —— builder_with_provider 内部就
        // 已传 ring 不会 panic，但 build_quic_client_config 链路上的其它
        // rustls 调用（特别是 verifier 构造）仍依赖 provider 已 install
        install_crypto_provider();

        // 用 STEP-1.1 + STEP-2.4 已实现的 `crypto::generate_self_signed`
        // 拿测试 cert（落盘到 `/tmp` ephemeral，EPH 测试 helper）
        let (cert_chain, key) = ephemeral_cert();
        // STEP-2.6：`build_quic_client_config` 加 `pins_dir` 参数（TofuVerifier
        // 替换 WebPkiServerVerifier；构造由 `TofuVerifier::new(pins_dir)` 全权负责）。
        let pins_dir =
            std::env::temp_dir().join(format!("lan-mouse-quic-test-pins-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&pins_dir);
        // STEP-2.5 起：`build_quic_client_config` 收 `Vec<CertificateDer>`（`with_client_auth_cert`
        // 要求 chain 形态）—— 单张 cert 包成 `vec![cert]` 即可
        let cfg = build_quic_client_config(vec![cert_chain[0].clone()], key, &pins_dir)
            .expect("ClientConfig 装配不应失败");
        // 关键断言：构造成功 + Clone（PLAN §2.2 dial_any 多候选复用要求）
        let _clone: QuinnClientConfig = cfg.clone();
        // ALPN 已被设上 `lan-mouse`（dial 时握手会用到）
        // 注：ClientConfig 的 alpn_protocols 字段是 quinn-proto 私有的；这
        // 里只能断言构造成功，不读内部字段
    }

    /// PLAN §2.2 验收：同进程内 server endpoint + client endpoint dial，断言
    /// TLS 1.3 握手完成（`peer_identity()` 非空）。
    ///
    /// **测试布局**：
    /// 1. server 端用 `endpoint_with_test_cert()` + `ephemeral_cert()` 起
    ///    server endpoint（不污染用户 cert 路径）
    /// 2. 后台 `tokio::spawn` 跑 `accept()` 拿到 `Connection` 后立即 drop
    ///    —— STEP-2.3 `accept()` 公共函数尚不存在，但 `Endpoint::accept()`
    ///    是 quinn 原生 API（`endpoint.accept().await.await` 即拿到 Connection）
    /// 3. client 端用 `endpoint()` 绑本地 + 调 `dial(...)`，5s 兜底
    /// 4. 断言 `peer_identity().is_some()` —— TLS 1.3 走通才会到这里
    ///
    /// **与 STEP-1.4 同路径的 `cargo test` 跑不通**（lib 因 14 DTLS errors
    /// 编不过），测试代码就位即可；STEP-6.x 修复后由 Leader 手动跑一次确认
    /// 通过（SUGGESTION #S-5）。
    #[tokio::test]
    async fn dial_completes_handshake_against_local_listener() {
        install_crypto_provider();

        // (1) server endpoint —— 临时 cert，不落盘
        let (server_cert_chain, server_key) = ephemeral_cert();
        let server_ep = endpoint_with_test_cert(
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into(),
            server_cert_chain,
            server_key,
        )
        .expect("server endpoint bind 不应失败");
        let server_addr = server_ep
            .local_addr()
            .expect("server endpoint 必须有 local_addr");

        // (2) 后台 accept task：拿 Connection 后立即 drop（不消费业务）
        //     drop(conn) 触发对端 ConnectionError::LocallyClosed（quinn 0.11 正常断开）。
        let server_task = tokio::spawn(async move {
            let incoming = server_ep.accept().await.expect("server accept 不应失败");
            let conn = incoming.await.expect("server handshake 不应失败");
            drop(conn);
        });

        // (3) client endpoint + dial —— 5s 兜底防止永久挂死
        let (client_cert, client_key) = ephemeral_cert();
        let client_ep = endpoint(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
            .expect("client endpoint bind 不应失败");
        // STEP-2.6：dial 加 pins_dir 参数；测试用临时 pins_dir 隔离。
        let pins_dir =
            std::env::temp_dir().join(format!("lan-mouse-quic-test-pins-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&pins_dir);
        let conn = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            dial(
                &client_ep,
                server_addr,
                client_cert[0].clone(),
                client_key,
                &pins_dir,
            ),
        )
        .await
        .expect("端到端 TLS 1.3 握手超时")
        .expect("dial 不应失败");

        // (4) 断言 peer_identity() 非空 —— TLS 1.3 实际走通才会到这里
        assert!(
            conn.peer_identity().is_some(),
            "peer_identity 应非空（TLS 1.3 握手完成）"
        );

        // (5) 清理：drop conn → server 端 ConnectionError::LocallyClosed
        drop(conn);
        server_task.await.expect("server task 不应 panic");
        client_ep.wait_idle().await;
    }

    /// PLAN §2.5 验收：server 端 [`PermissiveClientCertVerifier`] 强制 mTLS
    /// （`offer_client_auth() = true` + `client_auth_mandatory() = true`），
    /// client 端用**无** cert 的 `rustls::ClientConfig`（`with_no_client_auth()`）
    /// dial —— TLS 1.3 内置 `rustls::Error::NoCertificatesPresented` 应在
    /// server 端拒握；quinn 包装为 `ConnectionError` → [`Error::Handshake`]。
    ///
    /// **关键测试思路**：
    /// - server 端：调 [`endpoint_with_verifier`] + `Arc::new(PermissiveClientCertVerifier)`
    ///   —— mTLS 强制但放行任意 client cert
    /// - client 端：**直接构造 `rustls::ClientConfig` + `with_no_client_auth()`**，
    ///   **不**走 [`build_quic_client_config`]（后者 mTLS 起已强制
    ///   `with_client_auth_cert`）—— 这是为什么本测试必须 inline
    ///   `QuicClientConfig::try_from(...)` 的原因
    ///
    /// **为什么不测"client 出示错 cert"**：服务端 verifier 放行任意 cert，
    /// 出示错 cert 也通过；负面测试聚焦 mTLS 强制链路本身（client 不出 cert
    /// → server 拒）。STEP-2.7 `AuthorizedKeysVerifier` 接入后，加测"client
    /// 出示 cert 但 fingerprint 不在 allowlist"（与 bak
    /// `authorized_keys_verifier_rejects_unknown_client` 对齐）。
    ///
    /// **不污染**用户 cert 路径：`ephemeral_cert()` + `endpoint_with_verifier` 公共
    /// 函数 + 临时 cert 路径。
    ///
    /// **与 STEP-1.4 同路径的 `cargo test` 跑不通**（lib 因 14 DTLS errors
    /// 编不过），测试代码就位即可；STEP-6.x 修复后由 Leader 手动跑一次确认
    /// 通过（SUGGESTION #S-5）。
    #[tokio::test]
    async fn mtls_rejects_no_client_cert() {
        install_crypto_provider();

        // (1) server endpoint + verifier（强制 client auth + 任意放行）
        let (server_cert, server_key) = ephemeral_cert();
        let verifier: Arc<dyn rustls::server::danger::ClientCertVerifier> =
            Arc::new(PermissiveClientCertVerifier);
        let server_ep = endpoint_with_verifier(
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into(),
            server_cert,
            server_key,
            verifier,
        )
        .expect("server endpoint_with_verifier bind 不应失败");
        let server_addr = server_ep
            .local_addr()
            .expect("server endpoint 必须有 local_addr");

        // (2) server task：accept 期望失败（client 不出 cert → server 拒握）
        //     拿到握手错误后吞掉，不要 panic。
        let server_task = tokio::spawn(async move {
            let incoming = server_ep.accept().await.expect("server accept 不应失败");
            // server 端 handshake 应失败（NoCertificatesPresented → ConnectionError::TransportError）
            let result = incoming.await;
            assert!(
                result.is_err(),
                "server 端 handshake 应失败（mTLS 强制 client cert，client 未出示），实际 Ok"
            );
        });

        // (3) client endpoint + **无 cert** dial
        //     —— 走 inline `QuicClientConfig` 装配：root store 用 server cert 当
        //     trust anchor（链校验能过到 `self-signed` 入口；本测试不依赖 server
        //     cert 校验失败路径 —— 关键是 client 不出 cert 让 server 在更早的
        //     `CertificateRequest` 阶段拒握）
        use rustls::ClientConfig as RustlsClientConfig;

        let (server_cert_chain, _server_key) = ephemeral_cert();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(server_cert_chain[0].clone()).expect("add root");
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let builder = RustlsClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("protocol versions");
        // 关键：**不**调 with_client_auth_cert —— client 无 cert 可出示
        let mut rustls_client = builder.with_root_certificates(roots).with_no_client_auth();
        rustls_client.alpn_protocols = vec![ALPN_LAN_MOUSE.to_vec()];

        let quic_client =
            quinn::crypto::rustls::QuicClientConfig::try_from(Arc::new(rustls_client))
                .expect("QuicClientConfig try_from");
        let mut client_cfg = QuinnClientConfig::new(Arc::new(quic_client));
        client_cfg.transport_config(default_transport_config());

        let client_ep = endpoint(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
            .expect("client endpoint bind 不应失败");

        // (4) dial —— 5s 兜底；**必须**返回 Err
        let dial_result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client_ep.connect_with(client_cfg, server_addr, "lan-mouse"),
        )
        .await
        .expect("dial 端到端超时");

        // `connect_with` 同步部分返回 `Connecting<...>`，await 后才报握手失败
        match dial_result {
            Ok(connecting) => {
                let handshake_result = connecting.await;
                assert!(
                    handshake_result.is_err(),
                    "无 client cert 的 dial 应失败（server 端拒握），实际 Ok: {:?}",
                    handshake_result.as_ref().map(|c| c.stable_id())
                );
            }
            Err(e) => {
                // 同步部分失败（罕见，例如 cert chain 解析失败）也算测试通过
                log::debug!("connect_with 同步部分失败（可接受）：{e}");
            }
        }

        // (5) 清理：drop endpoint + 等 server task 完成
        drop(client_ep);
        let _ = server_task.await;
    }

    // === STEP-2.6 TofuVerifier 单元测试 =====================================

    /// 构造 ServerName 用于 verifier 测试。localhost 在所有平台都是合法 DNS name。
    fn test_server_name() -> ServerName<'static> {
        ServerName::try_from("localhost").expect("localhost is a valid DNS name")
    }

    /// 临时 pins_dir helper（与 `ephemeral_cert()` 风格对称）。返回
    /// `(dir, owned_path)` —— `dir` 在 test 期间自动清理。
    fn tmp_pins_dir(tag: &str) -> PathBuf {
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

    /// STEP-2.6 验收 (1/2)：新 fingerprint 被接受并写入 known_peers。
    ///
    /// 直接调 `TofuVerifier::verify_server_cert()`（不经过 QUIC），断言：
    /// - 返回 `Ok(ServerCertVerified::assertion())`
    /// - `pins_dir/<sanitized_fp>.pin` 文件存在（`:` 替换为 `_` 跨平台兼容）
    /// - pin 文件内容是 `b"trusted\n"`（占位文件，作为"已知"标记）
    ///
    /// 与 bak `mousehop/src/quic_transport.rs:2966-3001 tofu_first_connect_saves_fingerprint`
    /// 对齐（PLAN §2.6 验收清单要求 `tofu_first_run_pins`）。
    #[test]
    fn tofu_first_run_pins() {
        install_crypto_provider();

        let pins_dir = tmp_pins_dir("first");
        let verifier = TofuVerifier::new(&pins_dir);

        let (cert_chain, _key) = ephemeral_cert();
        let cert_der = cert_chain[0].clone();
        let fp = crypto::generate_fingerprint(cert_der.as_ref());

        let server_name = test_server_name();
        let now = UnixTime::now();
        let result = verifier.verify_server_cert(&cert_der, &[], &server_name, &[], now);

        // (1) 接受
        assert!(
            result.is_ok(),
            "first connect should be accepted (Ok), got {:?}",
            result
        );

        // (2) pin 文件存在（文件名 sanitize：`:` → `_`）
        let expected_pin = pins_dir.join(format!("{}.pin", fp.replace(':', "_")));
        assert!(
            expected_pin.exists(),
            "pin file should exist at {:?}",
            expected_pin
        );

        // (3) pin 文件内容是 b"trusted\n"（占位标记）
        let content = std::fs::read(&expected_pin).expect("read pin");
        assert_eq!(
            content, b"trusted\n",
            "pin file content should be 'trusted\\n'"
        );

        let _ = std::fs::remove_dir_all(&pins_dir);
    }

    /// STEP-2.6 验收 (2/2)：不同 fingerprint 被拒绝
    /// （`rustls::Error::General("TOFU mismatch: ...")`）。
    ///
    /// **直接调 verifier**：用 `TofuVerifier::with_known` 预落盘 cert1 的
    /// pin → 用 cert2（完全不同 fingerprint）的 `verify_server_cert` 走
    /// Known Mismatch 分支，断言返回 `Err(rustls::Error)`，且错误消息含
    /// "TOFU mismatch"。
    ///
    /// 与 bak `mousehop/src/quic_transport.rs:3047+ tofu_mismatch_rejects_different_fingerprint`
    /// 对齐（PLAN §2.6 验收清单要求 `tofu_disallows_swap`）。
    #[test]
    fn tofu_disallows_swap() {
        install_crypto_provider();

        let pins_dir = tmp_pins_dir("swap");

        // (1) 准备 cert1 → 计算 fp1 → 用 with_known 预落盘 fp1 的 pin
        let (cert1_chain, _key1) = ephemeral_cert();
        let cert1_der = cert1_chain[0].clone();
        let fp1 = crypto::generate_fingerprint(cert1_der.as_ref());
        let verifier = TofuVerifier::with_known(&pins_dir, &fp1);

        // (2) 准备 cert2（不同 cert → 不同 fp）
        let (cert2_chain, _key2) = ephemeral_cert();
        let cert2_der = cert2_chain[0].clone();
        let fp2 = crypto::generate_fingerprint(cert2_der.as_ref());
        assert_ne!(
            fp1, fp2,
            "两个 ephemeral_cert 必须有不同的指纹（rcgen 每次随机）"
        );

        // (3) verify_server_cert 应返回 Err（Known Mismatch 分支）
        let server_name = test_server_name();
        let now = UnixTime::now();
        let result = verifier.verify_server_cert(&cert2_der, &[], &server_name, &[], now);

        match result {
            Err(rustls::Error::General(msg)) => {
                assert!(
                    msg.contains("TOFU mismatch") || msg.contains("untrusted peer"),
                    "错误消息应含 TOFU mismatch / untrusted peer，实际: {msg}"
                );
            }
            other => panic!(
                "TOFU mismatch should return Err(rustls::Error::General), got: {:?}",
                other
            ),
        }

        // (4) fp1 的 pin 文件**不应**被改写 / 删（mismatch 不动现有 pin）
        let fp1_pin = pins_dir.join(format!("{}.pin", fp1.replace(':', "_")));
        assert!(
            fp1_pin.exists(),
            "mismatch 不应删除已存在的 fp1 pin 文件（pin 应保留）"
        );

        // (5) fp2 的 pin 文件**不**应被落盘（mismatch 不自动 pin 陌生 cert）
        let fp2_pin = pins_dir.join(format!("{}.pin", fp2.replace(':', "_")));
        assert!(
            !fp2_pin.exists(),
            "mismatch 不应自动 pin fp2（陌生 cert 必须保持陌生）"
        );

        let _ = std::fs::remove_dir_all(&pins_dir);
    }

    // === STEP-2.7 AuthorizedKeysVerifier 单元测试 =============================

    /// 临时 allowlist helper（与 `tmp_pins_dir` 风格对称）。
    fn tmp_allowlist(tag: &str) -> Arc<RwLock<HashMap<String, String>>> {
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
        Arc::new(RwLock::new(HashMap::new()))
    }

    /// STEP-2.7 验收 (1/2)：allowlist 预填某 fingerprint → `verify_client_cert`
    /// 用对应 cert → `Ok`。
    ///
    /// 直接调 [`AuthorizedKeysVerifier::verify_client_cert`]（不经过 QUIC），
    /// 避免 lib 因 14 DTLS errors 编不过 —— 测试代码逻辑就位即可，
    /// STEP-6.x 修 14 errors 后 Leader 手动跑一次确认（与 `mtls_rejects_no_client_cert`
    /// / `tofu_first_run_pins` 同路径，见 SUGGESTION #S-5）。
    #[test]
    fn authorized_keys_accepts_known() {
        let allowlist = tmp_allowlist("accepts");

        // (1) 自签一个测试 cert 拿 cert_der
        let (cert_chain, _key) = ephemeral_cert();
        let cert_der = cert_chain[0].clone();

        // (2) 预计算 fp，allowlist 预填
        let fp = crypto::generate_fingerprint(cert_der.as_ref());
        let verifier = AuthorizedKeysVerifier::with_known(allowlist.clone(), &fp);

        // (3) verify_client_cert 应 Ok
        let result = <AuthorizedKeysVerifier as rustls::server::danger::ClientCertVerifier>::verify_client_cert(
            &verifier,
            &cert_der,
            &[], // intermediates（自签没有 intermediates）
            rustls::pki_types::UnixTime::now(),
        );
        assert!(
            result.is_ok(),
            "allowlist 预填的 fingerprint 应被接受，实际: {result:?}"
        );

        // (4) 二次断言：allowlist 内容确实包含预填 fp（防止"路径走通但 allowlist 空"的假阳性）
        assert!(
            verifier.allowlist().read().unwrap().contains_key(&fp),
            "allowlist 应包含预填 fp"
        );
    }

    /// STEP-2.7 验收 (2/2)：allowlist 不含某 fingerprint → `verify_client_cert`
    /// 用对应 cert → `Err(rustls::Error::General("unauthorized peer {fp}"))`。
    ///
    /// 与 `tofu_disallows_swap` 对称 —— 都是"显式校验允许未授权对端被拒"的
    /// 负面测试；`AuthorizedKeysVerifier` 与 `TofuVerifier` 形成 mTLS 双层
    /// 防御（client 端 TOFU 拒 + server 端 allowlist 拒）。
    #[test]
    fn authorized_keys_rejects_unknown() {
        let allowlist = tmp_allowlist("rejects");

        // (1) 自签一个测试 cert，allowlist **不预填**
        let (cert_chain, _key) = ephemeral_cert();
        let cert_der = cert_chain[0].clone();
        let fp = crypto::generate_fingerprint(cert_der.as_ref());
        let verifier = AuthorizedKeysVerifier::new(allowlist.clone());

        // (2) verify_client_cert 应 Err
        let result = <AuthorizedKeysVerifier as rustls::server::danger::ClientCertVerifier>::verify_client_cert(
            &verifier,
            &cert_der,
            &[],
            rustls::pki_types::UnixTime::now(),
        );
        assert!(
            result.is_err(),
            "allowlist 不含的 fingerprint 应被拒绝，实际: {result:?}"
        );

        // (3) 错误消息应含 fingerprint（便于上层诊断）
        let err_msg = format!("{:?}", result.unwrap_err());
        assert!(
            err_msg.contains(&fp),
            "Err 消息应包含 fingerprint `{fp}`，实际: {err_msg}"
        );
        assert!(
            err_msg.contains("unauthorized"),
            "Err 消息应包含 'unauthorized' 关键字，实际: {err_msg}"
        );

        // (4) 二次断言：allowlist 确实不包含 cert_der 的 fp（防"allowlist 巧合预填"的假阴性）
        assert!(
            !verifier.allowlist().read().unwrap().contains_key(&fp),
            "allowlist 应不含 cert_der 的 fp"
        );
    }

    // === STEP-3.2 Hello 握手单元测试 =========================================
    //
    // 三个核心验收测试（PLAN §4 Step 1.6 验收清单）：
    // - `hello_happy_path_exchanges_magic` —— 两端 hello_ok == true + stream
    //   A 缓存
    // - `hello_wrong_magic_closes_connection` —— server 发错 magic → client
    //   `Error::HelloFailed("wrong magic")` + server conn 关
    // - `hello_timeout_aborts_session` —— 对端不开 stream A → 3s 后
    //   `Error::HelloTimeout(HELLO_TIMEOUT)` + `hello_ok == false`
    //
    // 与 bak `mousehop/src/quic_transport.rs:3481-3773` 完全对齐；差异仅
    // 在命名 / 测试 helper（用现有 `endpoint_with_test_cert` + `ephemeral_
    // cert` 不另起新 helper）。

    /// STEP-3.2 验收 (1/3)：Happy path —— server / client 都跑
    /// `server_hello` / `client_hello`，两端 `peer.hello_ok()` 都返 `true`，
    /// 且两端 `stream_a_cache` 都有缓存。
    ///
    /// 端到端：server_ep + client dial → 两端 spawn 各自 hello task → 5s
    /// 兜底超时（HELLO_TIMEOUT=3s 留余量）。
    #[tokio::test]
    async fn hello_happy_path_exchanges_magic() {
        install_crypto_provider();

        let (server_cert_chain, server_key) = ephemeral_cert();
        let server_ep = endpoint_with_test_cert(
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into(),
            server_cert_chain,
            server_key,
        )
        .expect("server endpoint bind");
        let server_addr = server_ep.local_addr().expect("server addr");

        // (1) 后台 server task：accept + server_hello
        let server_task = tokio::spawn(async move {
            let session = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                accept(&server_ep),
            )
            .await
            .expect("server accept timeout")
            .expect("server accept");
            let session = PeerSession::from_connection(session);

            tokio::time::timeout(std::time::Duration::from_secs(5), server_hello(&session))
                .await
                .expect("server hello timeout")
                .expect("server hello should succeed");

            assert!(
                session.hello_ok(),
                "server 端 hello_ok 应为 true（server_hello 已置位）"
            );

            // server 端 stream A 应已缓存
            let cached = session.take_stream_a_cache().await;
            assert!(
                cached.is_some(),
                "server_hello 后 peer.stream_a_cache 应有缓存"
            );

            // 留出时间让 client_hello 完成 read
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            drop(session);
        });

        // (2) client：dial + client_hello
        let pins_dir =
            std::env::temp_dir().join(format!("lan-mouse-quic-test-pins-{}", std::process::id()));
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

        // (3) client 端 stream A 也应已缓存（client/server 对称缓存）
        let cached = client_session.take_stream_a_cache().await;
        assert!(
            cached.is_some(),
            "client_hello 后 peer.stream_a_cache 应已缓存 Hello 用的 stream A"
        );

        // (4) 清理
        drop(client_session);
        drop(client_ep);
        server_task.await.expect("server task");
        let _ = std::fs::remove_dir_all(&pins_dir);
    }

    /// STEP-3.2 验收 (2/3)：server 发错 magic → client `Error::HelloFailed`。
    ///
    /// 端到端构造：
    /// - server 端**不**调 `server_hello()`，而是手动 `accept_bi` + 发错
    ///   magic 的 Hello 给 client（模拟"非 lan-mouse peer"）
    /// - client 调 `client_hello()` → 读到非 `PROTOCOL_MAGIC` → 关 conn +
    ///   返 `Err(HelloFailed)`
    ///
    /// 验证：错误消息含 "wrong magic" + `hello_ok == false`。
    #[tokio::test]
    async fn hello_wrong_magic_closes_connection() {
        install_crypto_provider();

        let (server_cert_chain, server_key) = ephemeral_cert();
        let server_ep = endpoint_with_test_cert(
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into(),
            server_cert_chain,
            server_key,
        )
        .expect("server endpoint bind");
        let server_addr = server_ep.local_addr().expect("server addr");

        // (1) 后台 server task：accept + 手动 accept_bi + 发错 magic Hello
        let server_task = tokio::spawn(async move {
            let conn = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                accept(&server_ep),
            )
            .await
            .expect("server accept timeout")
            .expect("server accept");

            let (mut send, _recv) = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                conn.open_bi(),
            )
            .await
            .expect("open_bi timeout")
            .expect("open_bi");

            // 发一个错 magic 的 Hello（不是 PROTOCOL_MAGIC）
            let wrong = ProtoEvent::Hello {
                magic: *b"LAN-MOUS",
                commit: [0u8; 8],
            };
            // 走写帧 helper：长度前缀 + 17 字节 body
            write_hello_frame(&mut send, &wrong)
                .await
                .expect("server write wrong hello");
            send.finish().expect("finish");

            // 等客户端收到错 magic 后会 conn.close()；等连接自然断
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            conn.close(VarInt::from(0u32), b"test done");
            drop(conn);
        });

        // (2) client：dial + client_hello → 期望 HelloFailed
        let pins_dir =
            std::env::temp_dir().join(format!("lan-mouse-quic-test-pins-{}", std::process::id()));
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

        // (3) 断言错误是 HelloFailed + 消息含 "wrong magic"
        match &result {
            Error::HelloFailed(msg) => {
                assert!(
                    msg.contains("wrong magic"),
                    "HelloFailed 消息应含 'wrong magic'，实际：{msg}"
                );
            }
            other => panic!("错误应为 Error::HelloFailed(wrong magic...)，实际：{other:?}"),
        }

        // (4) hello_ok 应仍为 false（握手失败）
        assert!(!client_session.hello_ok(), "失败路径 hello_ok 应保持 false");

        // (5) 清理
        drop(client_session);
        drop(client_ep);
        let _ = server_task.await;
        let _ = std::fs::remove_dir_all(&pins_dir);
    }

    /// STEP-3.2 验收 (3/3)：对端不开 stream A → 3s 后
    /// `Error::HelloTimeout(HELLO_TIMEOUT)`。
    ///
    /// 端到端构造：
    /// - server 端 accept() 后**不**做任何事（不调 `server_hello` / 不
    ///   `accept_bi`）
    /// - client 调 `client_hello()` → `open_bi()` 成功后写自己的 Hello，
    ///   等 server echo → 3s 内无响应 → `Error::HelloTimeout`
    ///
    /// 验证：错误是 `Error::HelloTimeout(HELLO_TIMEOUT)` + `hello_ok` 仍
    /// 为 false。
    #[tokio::test]
    async fn hello_timeout_aborts_session() {
        install_crypto_provider();

        let (server_cert_chain, server_key) = ephemeral_cert();
        let server_ep = endpoint_with_test_cert(
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into(),
            server_cert_chain,
            server_key,
        )
        .expect("server endpoint bind");
        let server_addr = server_ep.local_addr().expect("server addr");

        // (1) 后台 server task：accept 后**不**做任何事 → 等客户端超时
        let server_task = tokio::spawn(async move {
            let conn = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                accept(&server_ep),
            )
            .await
            .expect("server accept timeout")
            .expect("server accept");

            // 等 client 端超时（3s）+ 关 conn（client_hello 错误路径内部 close）
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            drop(conn);
        });

        // (2) client：dial + client_hello → 期望 HelloTimeout(3s)
        let pins_dir =
            std::env::temp_dir().join(format!("lan-mouse-quic-test-pins-{}", std::process::id()));
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

        // 用稍大于 HELLO_TIMEOUT 的总超时（5s）兜底
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client_hello(&client_session),
        )
        .await
        .expect("client_hello 总超时不应触发（HELLO_TIMEOUT=3s 应先触发）")
        .expect_err("client_hello 应该返回 Err(HelloTimeout)");

        // (3) 断言错误是 HelloTimeout(HELLO_TIMEOUT)
        match &result {
            Error::HelloTimeout(d) => {
                assert_eq!(*d, HELLO_TIMEOUT, "HelloTimeout 应等于 HELLO_TIMEOUT (3s)");
            }
            other => panic!("错误应为 Error::HelloTimeout(HELLO_TIMEOUT)，实际：{other:?}"),
        }

        // (4) hello_ok 仍 false
        assert!(!client_session.hello_ok(), "超时路径 hello_ok 应保持 false");

        // (5) 清理
        drop(client_session);
        drop(client_ep);
        let _ = server_task.await;
        let _ = std::fs::remove_dir_all(&pins_dir);
    }
}
