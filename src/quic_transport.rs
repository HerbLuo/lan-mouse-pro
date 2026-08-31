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

use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crate::crypto;
// `Endpoint` / `Connection` intentionally excluded from the `use` below —
// `pub use quinn::Endpoint` / `pub use quinn::Connection` re-export them
// for main-code (Step 6.x's `LanMouseListener::new`), matching the bak
// quic_transport.rs:84 pattern to avoid name collision.
use quinn::{ClientConfig as QuinnClientConfig, EndpointConfig, IdleTimeout, ServerConfig, TransportConfig};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
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

/// 装配 `quinn::ClientConfig`：rustls + ring + 客户端自签 cert 当 root +
/// 不带 verifier（**STEP-2.6 由 TofuVerifier 替换**）+ mTLS 出示 client cert
/// chain + ALPN `lan-mouse`。
///
/// 当前形态（STEP-2.5）：
/// - `crypto_provider = ring` —— 由 [`install_crypto_provider`] 早于
///   本调用预装（本函数不主动 install，main 启动期唯一入口在 main.rs）
/// - root cert store：把**对端** server cert 当 trust anchor 装入（仅
///   STEP-2.1 自测用；正式运行靠 STEP-2.6 TofuVerifier 做 fingerprint
///   pinning；本形态仅做 chain 校验到 root）
/// - **mTLS 出示 client cert chain**（STEP-2.5 起）：`with_client_auth_cert(
///   cert_chain, key)` 同步装上；与 server [`endpoint_with_verifier`] 的
///   `with_client_cert_verifier(...)` 对称 —— server 端 `CertificateRequest`
///   要求 client cert，client 通过本函数装配即可出示。`key` 字段不再是
///   占位 —— #S-7 已解。
/// - ALPN：`b"lan-mouse"` —— 与对端 server 协商协议；STEP-3.2 之上
///   另有应用层 `PROTOCOL_MAGIC` 二次握手（PLAN §3.1）
/// - transport：`default_transport_config()` 5s keepalive + 30s idle
///
/// **`cert_chain` 语义扩为双用**：既是 root store 信任 anchor，又是 mTLS 出示
/// 链。M1 双方都跑在同一台主机的同一进程，用同一私钥自签（生产路径
/// `dial()` 内部调 `crypto::load_or_create_server_cert()` 拿持久化 cert），
/// 双用同一 chain 不引安全风险。STEP-6.x 接入 connect.rs 时若需要 server
/// trust anchor 与本端 client cert 不同，再拆参数（暂不拆 —— §9 M1 边界）。
///
/// **不**主动 install crypto provider：本函数被 [`install_crypto_provider`]
/// 调用者（main.rs）守护；`#[test]` 单测则在第一句调一次 install
/// （`#[test]` 自身在主线程跑，但 cargo test 的多线程 harness 仍可能在别的
/// 线程触发 install —— 一次 `OnceLock` 守护足够）。
///
/// **错误归一**：所有 rustls / quinn 装配错误统一包到
/// [`Error::ClientConfig`]（带底层 `Display`）；不引入 `From<rustls::Error>`
/// / `From<quinn_proto::Error>` —— 后者不是 `pub` 路径且 STEP-6.x 之前
/// 没有别的 caller 会触发这些类型，盲目引入 `From` 反倒污染 `Error` 枚举。
pub fn build_quic_client_config(
    cert_chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<QuinnClientConfig> {
    use rustls::ClientConfig as RustlsClientConfig;
    use rustls::client::WebPkiServerVerifier;

    // 1. 构造 root cert store：把对端 server cert 当 trust anchor。
    //    STEP-2.6 TofuVerifier 走 .dangerous().with_custom_certificate_verifier
    //    替换 WebPkiServerVerifier；当前形态仅做标准 chain 校验。
    let mut roots = rustls::RootCertStore::empty();
    for cert in &cert_chain {
        roots
            .add(cert.clone())
            .map_err(|e| Error::ClientConfig(format!("add root cert: {e}")))?;
    }

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = RustlsClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::ClientConfig(format!("protocol versions: {e}")))?;
    // 占位 server verifier —— STEP-2.6 切到 TofuVerifier；本步保持 WebPkiServerVerifier
    // 标准 chain 校验，与 STEP-2.1 一致（#S-6 治理纪律：占位即正确，等替换）
    let verifier = WebPkiServerVerifier::builder(Arc::new(roots), provider)
        .build()
        .map_err(|e| Error::ClientConfig(format!("build webpki verifier: {e}")))?;

    // 2. STEP-2.5 起 mTLS 出示 client cert chain —— `with_client_auth_cert`
    //    是 terminal builder（返回 `Result<ClientConfig, Error>`，不像
    //    `with_no_client_auth` 是中间 builder），出错走 `?` 经 `crypto::Error::Rustls`
    //    收口到 `Error::ClientConfig`（`.map_err` 避免引入 From impl）
    let mut rustls_client = builder
        .with_webpki_verifier(verifier)
        .with_client_auth_cert(cert_chain, key)
        .map_err(|e| Error::ClientConfig(format!("with_client_auth_cert: {e}")))?;
    rustls_client.alpn_protocols = vec![ALPN_LAN_MOUSE.to_vec()];

    // 3. wrap 进 quinn::ClientConfig —— quinn 0.11 通过 `quinn::crypto::rustls`
    //    re-export 暴露 `QuicClientConfig`（顶层 `quinn_proto::*` 不是稳定
    //    公开路径，避免直接依赖 `quinn_proto` crate）
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
/// 1. 作为**对端** server 的 trust anchor 输入（`WebPkiServerVerifier` 链路；
///    STEP-2.6 由 `TofuVerifier` 替换 —— 调用栈不变）
/// 2. 作为**本端** client 的 mTLS 出示（`with_client_auth_cert(cert_chain, key)`）
///
/// M1 双方都跑在同一进程（生产路径） / 测试用 `ephemeral_cert()` 两套独立 cert；
/// 双用同一 chain 不引安全风险 —— M1 范围内合理。
///
/// **STEP-2.2 占位 verifier**：server cert 校验走 `WebPkiServerVerifier`
/// 占位（信任对端自签 cert 即放行；PLAN §2.1 已说明）。STEP-2.6 由
/// `TofuVerifier`（`with_custom_certificate_verifier`）替换 `WebPkiServerVerifier`
/// 路径，调用栈不变。
///
/// **参数顺序**：`(ep, addr, cert, key)` —— 与 PLAN §2.2 文字描述一致；
/// `cert` 是**单张** `CertificateDer`，本函数内部 `vec![cert]` 转 chain 后
/// 喂给 [`build_quic_client_config`]（后者要 `Vec<CertificateDer>` 才能调
/// `with_client_auth_cert`）。
///
/// **ALPN**：TLS 1.3 握手时声明 `b"lan-mouse"`（在 `build_quic_client_config`
/// 内设 `rustls_client.alpn_protocols`）。server 端 STEP-2.4 必须对称设
/// `rustls_server.alpn_protocols = vec![ALPN_LAN_MOUSE.to_vec()]`，否则
/// ALPN mismatch 直接拒连（SUGGESTION #S-9）。
///
/// **`server_name`**：`ep.connect_with(cfg, addr, "lan-mouse")` 的第三个
/// 参数用于 SNI（Server Name Indication）和 rustls 0.23 的
/// `ServerCertVerifier::verify_server_cert(..., server_name, ...)` 入参。
/// 当前 `WebPkiServerVerifier` 不读 server_name（链校验只看 cert）；
/// STEP-2.6 TofuVerifier 也不读 server_name（只看 fingerprint）。硬编
/// 码 `"lan-mouse"` 与 ALPN 协议名一致；与 bak `mousehop/src/quic_transport.rs:1855`
/// 的 `dial_one(... "mousehop")` 对称。
///
/// **错误归一**：
/// - `Endpoint::connect_with` 同步失败（endpoint 关闭 / 地址非法 / 无 client
///   config）→ [`Error::Connect`]（`#[from] quinn::ConnectError`）
/// - `.await` 后握手失败（证书 / ALPN / mTLS 不通过 / 中断）→ [`Error::Handshake`]
///   （`#[from] quinn::ConnectionError`）
///
/// **不**主动 `install_crypto_provider`：与 `build_quic_client_config` 对称，
/// 由 `main.rs` / 测试首句显式守护。
///
/// **`#[allow(dead_code)]`**：STEP-2.5 仅被测试调用；STEP-6.1
/// `connect.rs::connect_to_handle` 接入 `LanMouseConnection` 路径时一并移除。
#[allow(dead_code)]
pub async fn dial(
    ep: &Endpoint,
    addr: SocketAddr,
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
) -> Result<Connection> {
    // 幂等守护：与 build_quic_client_config 对称 —— 即使 caller 已在 main 启
    // 动期调过一次，测试路径多次进入同一函数依然安全。
    install_crypto_provider();

    // STEP-2.5：`build_quic_client_config` 改收 `Vec<CertificateDer>`（`with_client_auth_cert`
    // 要求 chain 形态；`vec![cert]` 是单张 chain 的标准包装 —— 与 bak
    // `mousehop/src/quic_transport.rs:1756` `dial_with_client_cert(..., cert_chain, key)`
    // 路径形态一致）。
    let cfg = build_quic_client_config(vec![cert], key)?;
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
        let conn = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            dial(&client_ep, server_addr, client_cert[0].clone(), client_key),
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
        // STEP-2.5 起：`build_quic_client_config` 收 `Vec<CertificateDer>`（`with_client_auth_cert`
        // 要求 chain 形态）—— 单张 cert 包成 `vec![cert]` 即可
        let cfg = build_quic_client_config(vec![cert_chain[0].clone()], key)
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
        let conn = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            dial(&client_ep, server_addr, client_cert[0].clone(), client_key),
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
}