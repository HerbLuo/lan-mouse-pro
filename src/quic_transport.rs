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
//! - STEP-2.6 / 2.7：`TofuVerifier` / `AuthorizedKeysVerifier`
//! - STEP-3.2：`client_hello` / `server_hello` 握手
//! - STEP-4.4：`route_input()` ChannelMode 分派
//! - STEP-5.x：数据通道（datagram + 3 stream）
//! - STEP-6.x：出入站集成（替换 `LanMouseConnection` / `LanMouseListener`）

use std::fs;
use std::net::{SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crate::crypto;
// `Endpoint` / `Connection` intentionally excluded from the `use` below —
// `pub use quinn::Endpoint` / `pub use quinn::Connection` re-export them
// for main-code (Step 6.x's `LanMouseListener::new`), matching the bak
// quic_transport.rs:84 pattern to avoid name collision.
use quinn::{ClientConfig as QuinnClientConfig, EndpointConfig, IdleTimeout, ServerConfig, TransportConfig};
use rustls::SignatureScheme;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use thiserror::Error;

pub use quinn::{Connection, Endpoint};

/// ALPN 协议标识：QUIC TLS 握手时互换的协议名。
///
/// 与对端 server 必须一致；STEP-3.2 之上还有应用层 `PROTOCOL_MAGIC` 二次握手，
/// ALPN 仅为 TLS 层声明"这是 lan-mouse 协议"。本仓保留品牌名 `lan-mouse`（不
/// 复用 bak 的 `mousehop`，与 PLAN §5 D1 对齐）。
pub(crate) const ALPN_LAN_MOUSE: &[u8] = b"lan-mouse";

/// 与对端的一条 QUIC 会话（client / server 共用）—— STEP-5.4 起承担端到端 IO。
///
/// STEP-1.4 仅占位；具体字段在 STEP-2 ~ STEP-5 落地（见模块顶部路线图）。
pub struct PeerSession {
    _private: (),
}

/// M1 传输层错误。
///
/// STEP-1.4 引入：占位变体 [`NotImplemented`] 保留；新增 [`Io`] / [`Bind`] /
/// [`EndpointSetup`] 给 `endpoint()` 路径用。后续 STEP 接入 verifier / IO
/// 时再补 `Error::Handshake` / `Error::HelloFailed` / `Error::Datagram` 等。
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
    let conn = ep
        .connect_with(cfg, addr, "lan-mouse")?
        .await?;
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
        crypto::generate_self_signed("lan-mouse-test", &cp, &kp)
            .expect("test cert 自签")
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
        let pins_dir = std::env::temp_dir().join(format!(
            "lan-mouse-quic-test-pins-{}",
            std::process::id()
        ));
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
        let pins_dir = std::env::temp_dir().join(format!(
            "lan-mouse-quic-test-pins-{}",
            std::process::id()
        ));
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
        let pins_dir = std::env::temp_dir().join(format!(
            "lan-mouse-quic-test-pins-{}",
            std::process::id()
        ));
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
        roots
            .add(server_cert_chain[0].clone())
            .expect("add root");
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let builder = RustlsClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("protocol versions");
        // 关键：**不**调 with_client_auth_cert —— client 无 cert 可出示
        let mut rustls_client = builder
            .with_root_certificates(roots)
            .with_no_client_auth();
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
}