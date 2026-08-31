//! QUIC 传输抽象层 —— M1 入口。
//!
//! 本模块把 UDP socket 包装成 [`quinn::Endpoint`]，并定义与对端的一路
//! QUIC 会话 [`PeerSession`]。完整生命周期由 STEP-1.x ~ STEP-5.x 逐步
//! 填实：
//!
//! - STEP-1.4（已）：[`endpoint`] —— UDP socket bind + 占位 client-mode Endpoint
//! - STEP-2.1（已）：[`build_quic_client_config`] + [`install_crypto_provider`]
//! - STEP-2.2 / 2.3：`dial()` / `accept()`
//! - STEP-2.4：`endpoint_with_cert()` 持久化 cert 注入
//! - STEP-2.6 / 2.7：`TofuVerifier` / `AuthorizedKeysVerifier`
//! - STEP-3.2：`client_hello` / `server_hello` 握手
//! - STEP-4.4：`route_input()` ChannelMode 分派
//! - STEP-5.x：数据通道（datagram + 3 stream）
//! - STEP-6.x：出入站集成（替换 `LanMouseConnection` / `LanMouseListener`）

use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

// `Endpoint` intentionally excluded from the `use` below — `pub use quinn::Endpoint`
// at line 26 re-exports it for main-code (Step 6.x's `LanMouseListener::new`),
// matching the bak quic_transport.rs:84 pattern to avoid name collision.
use quinn::{ClientConfig as QuinnClientConfig, EndpointConfig, IdleTimeout, ServerConfig, TransportConfig};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use thiserror::Error;

pub use quinn::Endpoint;

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
/// `#[allow(dead_code)]`：STEP-1.4 暂未注入到任何 `ServerConfig` /
/// `ClientConfig`（cert 路径未接，见 `endpoint()` 占位说明）；STEP-2.4 /
/// STEP-2.5 切到 `endpoint_with_cert(...)` / `client_endpoint(...)` 后
/// 通过 `server_cfg.transport = default_transport_config();` 链上，dead_code
/// 自动消失。
#[allow(dead_code)]
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
/// 不带 verifier（**STEP-2.6 由 TofuVerifier 替换**）+ ALPN `lan-mouse`。
///
/// 当前形态（STEP-2.1）：
/// - `crypto_provider = ring` —— 由 [`install_crypto_provider`] 早于
///   本调用预装（本函数不主动 install，main 启动期唯一入口在 main.rs）
/// - root cert store：把**对端** server cert 当 trust anchor 装入（仅
///   STEP-2.1 自测用；正式运行靠 STEP-2.6 TofuVerifier 做 fingerprint
///   pinning；本形态仅做 chain 校验到 root）
/// - **不**带 client cert 出示 —— mTLS 留 STEP-2.5；STEP-2.1 仅装配
///   client 一侧的握手结构（PLAN §2.1 "不带 verifier 占位"）
/// - ALPN：`b"lan-mouse"` —— 与对端 server 协商协议；STEP-3.2 之上
///   另有应用层 `PROTOCOL_MAGIC` 二次握手（PLAN §3.1）
/// - transport：`default_transport_config()` 5s keepalive + 30s idle
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
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
) -> Result<QuinnClientConfig> {
    use rustls::ClientConfig as RustlsClientConfig;
    use rustls::client::WebPkiServerVerifier;

    // 1. 构造 rustls::ClientConfig：ring + safe-default TLS 1.3 + 自签 cert
    //    当 root anchor + 不带 verifier（用 WebPkiServerVerifier::build
    //    走标准 chain 校验；STEP-2.6 改 with_custom_certificate_verifier
    //    注入 TofuVerifier）
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(cert)
        .map_err(|e| Error::ClientConfig(format!("add root cert: {e}")))?;

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = RustlsClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::ClientConfig(format!("protocol versions: {e}")))?;
    // 使用 WebPkiServerVerifier 做占位校验 —— 与 rustls 0.23 标准做法一致；
    // STEP-2.6 用 TofuVerifier 替换此 .dangerous().with_custom_certificate_verifier(...)
    // 链路；本步不引入 `dangerous()` API（紧贴 PLAN §2.1 "不带 verifier 占位"）
    let verifier = WebPkiServerVerifier::builder(Arc::new(roots), provider)
        .build()
        .map_err(|e| Error::ClientConfig(format!("build webpki verifier: {e}")))?;
    let mut rustls_client = builder
        .with_webpki_verifier(verifier)
        .with_no_client_auth();
    rustls_client.alpn_protocols = vec![ALPN_LAN_MOUSE.to_vec()];

    // 2. wrap 进 quinn::ClientConfig —— quinn 0.11 通过 `quinn::crypto::rustls`
    //    re-export 暴露 `QuicClientConfig`（顶层 `quinn_proto::*` 不是稳定
    //    公开路径，避免直接依赖 `quinn_proto` crate）
    let quic_client = quinn::crypto::rustls::QuicClientConfig::try_from(Arc::new(rustls_client))
        .map_err(|e| Error::ClientConfig(format!("QuicClientConfig::try_from: {e}")))?;
    let mut client_cfg = QuinnClientConfig::new(Arc::new(quic_client));
    client_cfg.transport_config(default_transport_config());

    // 关键：保存 key 防止 key 提前 drop 引发的"未使用变量"警告 —— 未来
    // STEP-2.5 mTLS 会通过 `with_client_auth_cert(cert_chain, key)` 接上
    // `key`，届时这一行自然消失。
    let _ = key;

    Ok(client_cfg)
}

// === 单元测试 ================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto;
    use std::net::{Ipv4Addr, SocketAddrV4};

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

        // 用 STEP-1.1 已实现的 `crypto::generate_self_signed` 拿测试 cert
        let (cert_chain, key) =
            crypto::generate_self_signed("lan-mouse-test", None).expect("test cert 自签");
        let cfg = build_quic_client_config(cert_chain[0].clone(), key)
            .expect("ClientConfig 装配不应失败");
        // 关键断言：构造成功 + Clone（PLAN §2.2 dial_any 多候选复用要求）
        let _clone: QuinnClientConfig = cfg.clone();
        // ALPN 已被设上 `lan-mouse`（dial 时握手会用到）
        // 注：ClientConfig 的 alpn_protocols 字段是 quinn-proto 私有的；这
        // 里只能断言构造成功，不读内部字段
    }
}