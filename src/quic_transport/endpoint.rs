//! UDP bind → `quinn::Endpoint`，dial / accept / install_crypto_provider。
//!
//! 本模块承担 QUIC 链路的两端入口：
//!
//! - [`endpoint`] 占位 client-mode endpoint（STEP-1.4 早期占位，
//!   STEP-2.4 后基本被 [`endpoint_with_cert`] / [`endpoint_with_verifier`]
//!   替代）
//! - [`endpoint_with_cert`] / [`endpoint_with_verifier`] server-mode
//!   endpoint 装配
//! - [`dial`] / [`dial_any`] 主动拨号（含 happy-eyeballs）
//! - [`accept`] 接受 incoming 握手
//! - [`install_crypto_provider`] rustls `ring` provider 一次性安装
//! - [`endpoint_inner`] 两条 server-mode 路径的私有 helper
//! - [`HEAD_START`] happy-eyeballs 200ms primary head-start
//!
//! 与 [`super::tls`] 的关系：`build_quic_client_config` 装配 client TLS
//! 配置被 `dial` / `dial_any` 调用；`default_transport_config` 提供 keepalive
//! + idle 设置。

use std::net::{SocketAddr, UdpSocket};
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use quinn::{EndpointConfig, ServerConfig};

use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use tokio::task::JoinSet;

use crate::crypto;

use super::tls;
use super::{Error, Result, ALPN_LAN_MOUSE, Connection, Endpoint};

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
/// 的 `ServerConfig` 上）。与 client [`super::tls::build_quic_client_config`]
/// 完全对称，否则 ALPN mismatch 直接拒连。
///
/// **`transport_config`**：通过 `server_cfg.transport_config(...)` 链上
/// [`super::tls::default_transport_config`] —— 5s keepalive / 30s idle
/// （PLAN §5 D4）。`default_transport_config` 的 `#[allow(dead_code)]` 守
/// 护在本函数接通后自动消失。
///
/// **错误归一**：复用现有变体 —— 不新增 `Error::ServerConfig` 等：
/// - `crypto::rustls_server_config` 失败 → `Error::Crypto(#[from])`
/// - `QuicServerConfig::try_from` 失败 → `Error::ClientConfig(String)`
/// - bind / runtime / `Endpoint::new` 失败 → 复用 [`endpoint`] 路径错误变体
///
/// **`install_crypto_provider` 不在本函数内调**：与
/// [`super::tls::build_quic_client_config`] 对称 —— 由 caller（service.rs /
/// 测试）显式守护。生产路径 `main.rs` 启动期已 install；测试首句调
/// `install_crypto_provider()`。
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
/// - fingerprint 命中 allowlist → 握手通过（STEP-2.7
///   [`super::tls::AuthorizedKeysVerifier`]）
/// - 未命中 / 缺 client cert → `rustls::Error::General(...)`，quinn 包装为
///   `ConnectionError::TransportError` / `LocallyClosed` → [`Error::Handshake`]
///
/// **#S-7 配套** —— 当 server `client_auth_mandatory() -> true`（本仓默认），
/// server 端 `CertificateRequest` 要求 client 出示 cert；client 端
/// [`super::tls::build_quic_client_config`] 同时把 `(cert, key)` 通过
/// `with_client_auth_cert` 装上（#S-7 解），TLS 握手双端 mTLS 才完整。
///
/// **生产路径 caller**（STEP-6.2 整段接 `listen.rs` supervisor）：
/// 1. `crypto::load_or_create_server_cert()` → `(cert_chain, key)`
/// 2. 构造 verifier（STEP-2.5 用 [`super::tls::PermissiveClientCertVerifier`]
///    占位；STEP-2.7 替换为 `AuthorizedKeysVerifier` 走
///    `config.authorized_fingerprints()`）
/// 3. `endpoint_with_verifier(addr, cert_chain, key, verifier)`
///
/// **本步默认 verifier**：[`super::tls::PermissiveClientCertVerifier`] ——
/// 实现"接受任意 client cert，只要它存在 + 签名通过 TLS 1.3 内置校验"。
/// 这是 M1 STEP-2.5 阶段的占位；STEP-2.7 由 `AuthorizedKeysVerifier` 替换为
/// "指纹 allowlist"。不引入占位 verifier 也能编译通过（直接传
/// `Arc::new(WebPkiClientVerifier::...` 也可以），但当前选择最小可工作形态
/// + 显式"占位"标记，方便后续 step 检索。
///
/// **错误归一**：复用现有 [`Error`] 变体 —— 不新增：
/// - `crypto::rustls_server_config_with_verifier` 失败 → `Error::Crypto`
/// - `endpoint_inner` 内部错误（`Arc::try_unwrap` / `QuicServerConfig::try_from` /
///   bind / runtime / `Endpoint::new`）→ 复用 [`endpoint_with_cert`] 路径错误
///
/// **`install_crypto_provider` 不在本函数内调**：与 [`endpoint_with_cert`]
/// 对称。
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
fn endpoint_inner(
    addr: SocketAddr,
    rustls_server_arc: Arc<rustls::ServerConfig>,
) -> Result<Endpoint> {
    // `alpn_protocols` 是 `rustls::ServerConfig` 的字段（不在 quinn 的
    // `ServerConfig` 上），所以要在 wrap 进 `QuicServerConfig` 之前设置。
    let mut rustls_server = Arc::try_unwrap(rustls_server_arc)
        .map_err(|_| Error::ClientConfig("rustls ServerConfig Arc 强引用数 > 1".into()))?;
    rustls_server.alpn_protocols = vec![ALPN_LAN_MOUSE.to_vec()];

    let quic_server = quinn::crypto::rustls::QuicServerConfig::try_from(Arc::new(rustls_server))
        .map_err(|e| Error::ClientConfig(format!("QuicServerConfig::try_from: {e}")))?;
    let mut server_cfg = ServerConfig::with_crypto(Arc::new(quic_server));
    server_cfg.transport_config(tls::default_transport_config());

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
/// CLI 与 daemon 同时 install 时，第二次 `install_default()` 返回
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

/// 主动拨号到对端 endpoint，完成 QUIC TLS 1.3 握手后返回 [`Connection`]。
///
/// **STEP-2.5 mTLS**：本函数复用 [`super::tls::build_quic_client_config`]，
/// 后者已通过 `with_client_auth_cert(cert_chain, key)` 装上 mTLS 出示。
/// `cert` / `key` 参数在 STEP-2.5 起**双用**：
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
/// chain 后喂给 [`super::tls::build_quic_client_config`]。
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
    let cfg = tls::build_quic_client_config(vec![cert], key, pins_dir)?;
    let conn = ep.connect_with(cfg, addr, "lan-mouse")?.await?;
    Ok(conn)
}

/// Happy-eyeballs 拨号（STEP-6.4 引入）—— 200ms primary head-start + 其余
/// 候选并发拨；首个 QUIC TLS 1.3 握手成功者赢，返回原始 [`Connection`]。
///
/// **happy-eyeballs 算法**（RFC 8305 简化版，PLAN §6.4）：
/// 1. 一次性构造 `Arc<ClientConfig>`（`build_quic_client_config` + cert/key/
///    pins_dir 注入；`ClientConfig: Clone` 复用给每条候选 —— 避免
///    `PrivateKeyDer::clone_key()` 每候选重 parse）
/// 2. **`primary` 单独 head start** —— spawn 一条 task 拨 primary；`tokio::select!`
///    race 200ms timer vs `joinset.join_next()`：
///    - 200ms 内赢（primary 握手成功）→ 立即 `abort_all()` + 返回
///    - 200ms 内 primary 失败（输在 timer 之前）→ log warn + 等 timer 触发
/// 3. **head-start 结束 → 剩余候选一齐拨** —— spawn task 给 `all` 中除
///    primary 外的所有地址
/// 4. **首个成功 task** → `abort_all()` + 返回 Connection
/// 5. 全部 dial 失败 → 返**最后**一个错误（与 bak `Mousehop::dial_any`
///    "覆盖最新错误"语义对齐；SUGGESTION #S-21 治理落地）
///
/// **与 bak `mousehop/src/quic_transport.rs:1930 dial_any` 的差异**：
/// - 返回 [`Connection`] 而**非** `Rc<PeerSession>` —— STEP-6.1 caller
///   `connect_to_handle` 自己包 `PeerSession` + 跑 `client_hello`（拆开
///   "happy-eyeballs" 与 "hello 握手"两个关注点；STEP-6.5 重连时 hello
///   可复用同一路径）。PLAN §6.4 文字明确签名 `Result<Connection>`，本步
///   与之对齐
/// - 不带 `InputChannelConfig` 参数 —— `dial_any` 只管"连上"，路由配置
///   与 hello 是 caller 责任（与 STEP-6.1 拆分一致）
///
/// **为什么 200ms**：PLAN §6.4 + connect.rs 现有 DTLS `connect_any` 沿用
/// 同一常量（见 PLAN §7 风险表"happy-eyeballs 200ms 阈值太小被防火墙丢
/// 弃" —— bak 默认；本步落地 bak 取舍）。LAN 内 200ms 通常够 primary
/// 握手完成；超时则并发拨兜底 LAN 多宿主延迟漂移
///
/// **`JoinSet` vs `Vec<SpawnLocal>`**：JoinSet 提供 `join_next().await`
/// + `abort_all()` 一站式 API，与 STEP-0.1 全仓 `spawn_local` 惯例一致。
/// quinn `Connection` 实现 `Drop` 自动 close（QUIC 相对 DTLS 的简化），
/// 输家被 abort 时 RAII 自动关连，**不**需要显式 `conn.close(...)`。
///
/// **`#[allow(dead_code)]`**：STEP-6.4 仅被 `connect.rs::connect_to_handle`
/// 接入；dead_code 自动消失。
#[allow(dead_code)]
pub async fn dial_any(
    ep: &Endpoint,
    primary: SocketAddr,
    all: &[SocketAddr],
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
    pins_dir: &Path,
) -> Result<Connection> {
    install_crypto_provider();

    // (1) 一次性构造 ClientConfig，复用给每条 dial
    let cfg = tls::build_quic_client_config(vec![cert], key, pins_dir)?;

    // (2) JoinSet 收集 (SocketAddr, Result<Connection, Error>)
    let mut joinset: JoinSet<(SocketAddr, Result<Connection>)> = JoinSet::new();
    let mut spawned: std::collections::HashSet<SocketAddr> = std::collections::HashSet::new();

    // (3) primary 单独 head start spawn
    {
        let cfg_ref = cfg.clone();
        let ep_ref = ep.clone();
        joinset.spawn_local(async move {
            let res = ep_ref.connect_with(cfg_ref, primary, "lan-mouse");
            match res {
                Ok(connecting) => match connecting.await {
                    Ok(conn) => (primary, Ok(conn)),
                    Err(e) => (primary, Err(Error::Handshake(e))),
                },
                Err(e) => (primary, Err(Error::Connect(e))),
            }
        });
        spawned.insert(primary);
    }

    // (4) primary head start race：200ms 内赢 → 立即返回；输 → log warn + 等 timer
    {
        let head_start = tokio::time::sleep(HEAD_START);
        tokio::pin!(head_start);
        loop {
            tokio::select! {
                _ = &mut head_start => break,
                joined = joinset.join_next() => {
                    let Some(inner) = joined else { break; };
                    let Ok((_addr, res)) = inner else {
                        log::warn!("dial_any: JoinSet task panic（head-start 期）");
                        continue;
                    };
                    match res {
                        Ok(conn) => {
                            joinset.abort_all();
                            return Ok(conn);
                        }
                        Err(e) => {
                            log::warn!("dial_any: dial {_addr} 失败（head-start 期）：{e}");
                        }
                    }
                }
            }
        }
    }

    // (5) head-start 内 primary 没赢 → 剩余候选一齐拨
    for &addr in all {
        if spawned.contains(&addr) {
            continue;
        }
        let cfg_ref = cfg.clone();
        let ep_ref = ep.clone();
        joinset.spawn_local(async move {
            let res = ep_ref.connect_with(cfg_ref, addr, "lan-mouse");
            match res {
                Ok(connecting) => match connecting.await {
                    Ok(conn) => (addr, Ok(conn)),
                    Err(e) => (addr, Err(Error::Handshake(e))),
                },
                Err(e) => (addr, Err(Error::Connect(e))),
            }
        });
        spawned.insert(addr);
    }

    // (6) wait for any to win
    let mut last_err: Option<Error> = None;
    while let Some(joined) = joinset.join_next().await {
        let Ok((_addr, res)) = joined else {
            log::warn!("dial_any: JoinSet task panic");
            continue;
        };
        match res {
            Ok(conn) => {
                joinset.abort_all();
                return Ok(conn);
            }
            Err(e) => {
                log::warn!("dial_any: dial {_addr} 失败：{e}");
                last_err = Some(e);
            }
        }
    }

    Err(last_err.expect("JoinSet 至少应 join 一个 task"))
}

/// happy-eyeballs 给 primary 单独留的 200ms head start（RFC 8305 简化版 /
/// connect.rs 现有 `PREFERRED_ADDR_HEAD_START` 语义）。
///
/// 与 bak `mousehop/src/quic_transport.rs:2004 HEAD_START` 完全对齐。
const HEAD_START: Duration = Duration::from_millis(200);

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

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

    use crate::quic_transport::test_helpers::{
        ephemeral_cert, ephemeral_pins_dir, endpoint_with_test_cert,
    };

    use super::*;

    /// STEP-2.4 验收 #1：`endpoint_with_cert` bind 临时端口 + Drop 不 panic。
    #[tokio::test]
    async fn endpoint_with_cert_binds_ipv4_localhost() {
        install_crypto_provider();
        let (cert_chain, key) = ephemeral_cert();
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into();
        let ep = endpoint_with_cert(addr, cert_chain, key).expect("endpoint_with_cert bind 不应失败");
        let local = ep.local_addr().expect("endpoint 必须有 local_addr");
        assert_ne!(local.port(), 0, "ephly 端口应非零");
        drop(ep);
    }

    /// STEP-2.4 验收 #2：`endpoint_with_cert` 接 incoming 连接 + client dial
    /// 完成 TLS 1.3 握手。
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

        let server_task = tokio::spawn(async move {
            let incoming = server_ep.accept().await.expect("server accept 不应失败");
            let conn = incoming.await.expect("server handshake 不应失败");
            drop(conn);
        });

        let (client_cert, client_key) = ephemeral_cert();
        let client_ep = endpoint(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
            .expect("client endpoint bind 不应失败");
        let pins_dir = ephemeral_pins_dir();
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
        let local = ep.local_addr().expect("endpoint 必须有 local_addr");
        assert_ne!(local.port(), 0, "ephly 端口应非零");
        drop(ep);
    }

    /// PLAN §2.2 验收：同进程内 server endpoint + client endpoint dial，断言
    /// TLS 1.3 握手完成（`peer_identity()` 非空）。
    #[tokio::test]
    async fn dial_completes_handshake_against_local_listener() {
        install_crypto_provider();

        let (server_cert_chain, server_key) = ephemeral_cert();
        let server_ep = endpoint_with_test_cert(
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into(),
            server_cert_chain,
            server_key,
        )
        .expect("server endpoint bind 不应失败");
        let server_addr = server_ep.local_addr().expect("server endpoint 必须有 local_addr");

        let server_task = tokio::spawn(async move {
            let incoming = server_ep.accept().await.expect("server accept 不应失败");
            let conn = incoming.await.expect("server handshake 不应失败");
            drop(conn);
        });

        let (client_cert, client_key) = ephemeral_cert();
        let client_ep = endpoint(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
            .expect("client endpoint bind 不应失败");
        let pins_dir = ephemeral_pins_dir();
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

    /// STEP-6.4 验收 (1/2)：dial_any primary 选 primary 即 server_addr。
    #[tokio::test(flavor = "multi_thread")]
    async fn dial_any_prefers_primary() {
        let fut = async {
            install_crypto_provider();

            let (server_cert, server_key) = ephemeral_cert();
            let server_ep = endpoint_with_test_cert(
                SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into(),
                server_cert,
                server_key,
            )
            .expect("server endpoint bind");
            let server_addr = server_ep.local_addr().expect("server addr");

            let server_task = tokio::spawn(async move {
                let _conn = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    accept(&server_ep),
                )
                .await
                .expect("server accept timeout")
                .expect("server accept");
            });

            let pins_dir = ephemeral_pins_dir();
            let _ = std::fs::remove_dir_all(&pins_dir);
            let (client_cert, client_key) = ephemeral_cert();
            let client_ep = endpoint(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
                .expect("client endpoint bind");

            let unreachable = SocketAddr::new(
                std::net::IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)),
                65535,
            );
            let all = vec![server_addr, unreachable];

            let conn = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                dial_any(
                    &client_ep,
                    server_addr,
                    &all,
                    client_cert[0].clone(),
                    client_key,
                    &pins_dir,
                ),
            )
            .await
            .expect("dial_any 总超时")
            .expect("dial_any 应成功（primary 赢）");

            assert_eq!(
                conn.remote_address(),
                server_addr,
                "dial_any 应选 primary（即 server_addr），而非 fallback 不可达地址"
            );

            conn.close(quinn::VarInt::from(0u32), b"test done");
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), server_task).await;
            drop(client_ep.wait_idle());
            let _ = std::fs::remove_dir_all(&pins_dir);
        };
        tokio::task::LocalSet::new().run_until(fut).await;
    }

    /// STEP-6.4 验收 (2/2)：dial_any 全部候选不可达 → 返 Err。
    #[tokio::test(flavor = "multi_thread")]
    async fn dial_any_all_unreachable_returns_err() {
        let fut = async {
            install_crypto_provider();

            let client_ep = endpoint(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
                .expect("client endpoint bind");

            let pins_dir = ephemeral_pins_dir();
            let _ = std::fs::remove_dir_all(&pins_dir);
            let (client_cert, client_key) = ephemeral_cert();

            let primary = SocketAddr::new(
                std::net::IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)),
                65535,
            );
            let secondary = SocketAddr::new(
                std::net::IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2)),
                65535,
            );
            let all = vec![primary, secondary];

            let result = tokio::time::timeout(
                std::time::Duration::from_secs(35),
                dial_any(
                    &client_ep,
                    primary,
                    &all,
                    client_cert[0].clone(),
                    client_key,
                    &pins_dir,
                ),
            )
            .await
            .expect("dial_any 总超时（应 < 35s 内返 Err）");

            assert!(
                result.is_err(),
                "全部候选不可达时 dial_any 应返 Err，实际返：{result:?}"
            );

            drop(client_ep);
            let _ = std::fs::remove_dir_all(&pins_dir);
        };
        tokio::task::LocalSet::new().run_until(fut).await;
    }
}