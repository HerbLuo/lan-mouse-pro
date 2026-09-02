//! TLS / mTLS 信任配置（STEP-2.5 / 2.6 / 2.7）。
//!
//! 本模块承担 QUIC 链路的 TLS 信任决策：
//!
//! - [`build_quic_client_config`] 装配 `quinn::ClientConfig`（rustls +
//!   ring + [`TofuVerifier`] + mTLS 出示 client cert + ALPN）
//! - [`default_transport_config`] server/client 共享的 `TransportConfig`
//!   （5s keepalive / 30s idle）
//! - [`TofuVerifier`] 客户端 TOFU（Trust On First Use）fingerprint pinning
//! - [`PermissiveClientCertVerifier`] STEP-2.5 占位 verifier（接受任意
//!   通过 TLS 1.3 内置链校验的 client cert）
//! - [`AuthorizedKeysVerifier`] STEP-2.7 server 端 fingerprint allowlist
//!
//! 与 [`super::endpoint`] 的关系：本模块的 `build_quic_client_config` 被
//! `endpoint::dial` / `endpoint::dial_any` 调用装配 client config；server
//! 端 `endpoint_with_verifier` 直接接受 caller 提供的 verifier（`PermissiveClientCertVerifier`
//! 或 `AuthorizedKeysVerifier`）。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use quinn::{ClientConfig as QuinnClientConfig, IdleTimeout, TransportConfig};
use rustls::SignatureScheme;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};

use crate::crypto;

use super::{Error, Result, ALPN_LAN_MOUSE};

/// server / client 共享的 `TransportConfig`：
///
/// - `keep_alive_interval = 5s` —— QUIC 主动探活，配合 PLAN §7 "Wi-Fi
///   切换恢复 < 1s" 预算；与 bak Step 0.1 spike 实测一致。
/// - `max_idle_timeout = 30s` —— QUIC keepalive 自带；应用层 idle 检测
///   已于 STEP-7.1 下线（原 DTLS 时代 8s 应用层 idle 探测随 STEP-6.2
///   listen.rs 重写一并消失）。对端静默不再触发本端主动关连：只有 QUIC
///   自身 30s idle 超时（且 5s keepalive 在健康链路上永远先到）才关。
///
/// `IdleTimeout::try_from(Duration)` 失败当且仅当 Duration 超 VarInt
/// 2^30 ms 上限（≈ 12.4 天），30s 远在范围内 —— `expect` 注明理由。
///
/// **可见性**：`pub(super)` —— 仅 `endpoint.rs` 通过 `super::tls::default_transport_config`
/// 调。`endpoint_inner`（在 endpoint.rs）需要它做 server transport 配置；
/// `build_quic_client_config`（本文件）直接调它。
pub(super) fn default_transport_config() -> Arc<TransportConfig> {
    let mut t = TransportConfig::default();
    t.keep_alive_interval(Some(Duration::from_secs(5)));
    t.max_idle_timeout(Some(
        IdleTimeout::try_from(Duration::from_secs(30))
            .expect("30s 远小于 VarInt 2^30 ms 上限（≈ 12.4 天）"),
    ));
    Arc::new(t)
}

/// 装配 `quinn::ClientConfig`：rustls + ring + TofuVerifier（**STEP-2.6
/// 替换 WebPkiServerVerifier**）+ mTLS 出示 client cert chain + ALPN
/// `lan-mouse`。
///
/// 当前形态（STEP-2.6）：
/// - `crypto_provider = ring` —— 由 [`super::endpoint::install_crypto_provider`]
///   早于本调用预装（本函数不主动 install，main 启动期唯一入口在 main.rs）
/// - **TofuVerifier server cert 校验**（STEP-2.6 起）：`.dangerous().with_
///   custom_certificate_verifier(Arc::new(TofuVerifier::new(pins_dir)))`
///   替代 STEP-2.5 的 `WebPkiServerVerifier` 占位 verifier；`TofuVerifier`
///   按 server cert SHA-256 fingerprint + `$pins_dir/<sanitized_fp>.pin`
///   文件系统缓存做"首次见到自动 pin / 已知 mismatch 拒绝"的三态判定（与
///   bak `mousehop/src/quic_transport.rs:1799` 路径完全对齐；#S-6 已解）
/// - **mTLS 出示 client cert chain**（STEP-2.5 起）：`with_client_auth_cert(
///   cert_chain, key)` 同步装上；与 server [`super::endpoint::endpoint_with_verifier`]
///   的 `with_client_cert_verifier(...)` 对称。`key` 字段不再是占位
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
/// **不**主动 install crypto provider：本函数被 [`super::endpoint::install_crypto_provider`]
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
#[derive(Debug)]
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
                d.filter_map(std::io::Result::ok)
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
#[derive(Debug)]
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
    ) -> std::result::Result<rustls::server::danger::ClientCertVerified, rustls::Error> {
        let fp = crate::crypto::generate_fingerprint(end_entity.as_ref());
        log::debug!("[STEP-2.5 占位 verifier] accept client cert fp={fp}");
        Ok(rustls::server::danger::ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        // STEP-2.5 占位 verifier —— TLS 1.2 路径无签名需求（client cert
        // 通过 TLS 1.3 内置链校验即可）。签名验签实现在 STEP-2.7
        // `AuthorizedKeysVerifier` 中（持 provider + 转发到 ring provider）。
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        // 同上 —— TLS 1.3 路径下 STEP-2.5 占位 verifier 不做签名验签
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        // 占位 verifier 不校验签名 schema —— 返回空 vec 即可
        Vec::new()
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
/// 自然对齐。STEP-7 / M2 把 `IncomingPeerConfig` 引入 `lan-mouse-ipc` 后，
/// 同步把本结构 + caller 一起改成 `HashMap<String, IncomingPeerConfig>`
/// （与 bak `mousehop/src/quic_transport.rs:1577-1754 AuthorizedKeysVerifier`
/// 形态完全对齐；值类型用 `IncomingPeerConfig::default()` 表示"已授权但
/// 还没填配置"）。
///
/// **`allowlist` 跨平台语义**：`String` 是 fingerprint（小写 hex + `:` 分隔，
/// 与 `crypto::generate_fingerprint` 输出格式一致）。运行时增 / 删 allowlist
/// 条目通过 `Arc<RwLock<HashMap<...>>>` 共享所有权 —— listen.rs supervisor
/// 或后续 IPC 推 `authorized_fingerprints` 变更时，可直接写本结构内的
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
/// **dead_code chain**：本结构被 [`super::endpoint::endpoint_with_verifier`] 的 verifier 参数
/// 消费 → 由 `endpoint_with_verifier` 间接消费 → 单测直接调
/// `verify_client_cert`。main-code 接入路径留 STEP-6.2 `listen.rs` supervisor
/// 整段重写时一并消化（listen.rs 当前仍调 DTLS 路径，14 errors 不在本步范围）。
#[derive(Debug)]
pub struct AuthorizedKeysVerifier {
    /// 授权指纹表：键 = client cert SHA-256 fingerprint（`crypto::generate_fingerprint` 格式），
    /// 值 = 占位 `String`（M2 接 `lan_mouse_ipc::IncomingPeerConfig::default()`）。
    allowlist: Arc<RwLock<HashMap<String, String>>>,
    /// 签名验签需要的 provider（`verify_tls12_signature` / `verify_tls13_signature`
    /// 转发到 `rustls::crypto::verify_*_signature` 时拿它的
    /// `signature_verification_algorithms`）。
    provider: Arc<rustls::crypto::CryptoProvider>,
    /// **STEP-8.2 修复**：被拒对端 fingerprint 反向通知 channel —— 把
    /// rustls 拒握路径里拿到的 fingerprint 透回 listen task，转译为
    /// `ListenEvent::Rejected` → emulation.rs 上报 `ConnectionAttempt`
    /// → GUI 弹窗。
    ///
    /// **`Option` 而非必填**：单测 + 早期 caller（无 listen task 装配时）
    /// 不接 channel 时为 `None`，`verify_client_cert` 走 no-op 分支。
    ///
    /// **为什么用 `tokio::sync::mpsc::UnboundedSender` 而非 `local_channel`**：
    /// `verify_client_cert` 由 rustls 在 QUIC 握手回调链里调用 —— quinn
    /// 的 I/O task 可能跑在非 local 线程上（与 spawn_local 不属同一 task）。
    /// `tokio::sync::mpsc::UnboundedSender` 是 `Send + Sync`，可跨线程持有；
    /// listen task 的 forwarder 在 `spawn_local` 上 recv（同 §1 `wake_rx`
    /// 模式）。
    rejection_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
}

impl AuthorizedKeysVerifier {
    /// 构造：allowlist 由 caller 持有（生产 `Config::authorized_fingerprints()`，
    /// 测试 `Arc::new(RwLock::new(HashMap::new()))`）。
    ///
    /// `allowlist` 必须 `Send + Sync + 'static`（rustls 要求 verifier
    /// `Send + Sync + 'static`；`Arc<RwLock<HashMap<...>>>` 自动满足）。
    ///
    /// **无 rejection channel**：单测 / 早期 caller 用此构造；rustls 拒握
    /// 时仅 `log::warn` 留审计线索，不通知 GUI。
    #[allow(dead_code)]
    pub fn new(allowlist: Arc<RwLock<HashMap<String, String>>>) -> Self {
        Self {
            allowlist,
            provider: Arc::new(rustls::crypto::ring::default_provider()),
            rejection_tx: None,
        }
    }

    /// **STEP-8.2 修复**：注入 rejection 反向通知 channel —— builder 模式，
    /// 不破坏既有 `new()` / `with_known()` 单测与 caller 的签名。
    ///
    /// `verify_client_cert` 在 Err 路径上额外 `rejection_tx.send(fp.clone())`
    /// （channel 满 / 关闭时静默 no-op —— reject 事件是 best-effort，不应
    /// 干扰 rustls 原本返 Err 的语义）。
    #[allow(dead_code)]
    pub fn with_rejection_tx(
        mut self,
        tx: tokio::sync::mpsc::UnboundedSender<String>,
    ) -> Self {
        self.rejection_tx = Some(tx);
        self
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
            // **STEP-8.2 修复**：除了 `log::warn` 留审计线索 + 返回 rustls
            // `Err` 触发握手拒绝，**同时**把 fingerprint 通过反向 channel
            // 通知 listen task → 转译 `ListenEvent::Rejected` →
            // `EmulationEvent::ConnectionAttempt` → GUI `request_authorization`
            // 弹窗（emulation.rs:190 + service.rs:320 + 前端 `request_authorization`）。
            //
            // **send 失败静默吞**：`UnboundedSender::send` 仅在 receiver drop
            // 时返 `Err`（channel 关闭），此时 listen task 已退出（terminate）
            // —— 拒握已是终局，发不出"弹窗"信号合理 no-op，**不**应让这影响
            // rustls 原本返 Err 的语义（rustls 仍按设计拒握，错误消息不变）。
            log::warn!("AuthorizedKeysVerifier: rejected unauthorized peer {fp}");
            if let Some(tx) = &self.rejection_tx {
                let _ = tx.send(fp.clone());
            }
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

#[cfg(test)]
mod tests {
    use rustls::client::danger::ServerCertVerifier;

    use crate::quic_transport::endpoint_with_verifier;
    use crate::quic_transport::test_helpers::{
        ephemeral_cert, ephemeral_pins_dir, test_server_name, tmp_allowlist, tmp_pins_dir,
    };

    use super::*;

    /// PLAN §2.1 验收：用测试自签 cert 装配 `quinn::ClientConfig` 不 panic。
    #[test]
    fn quinn_client_config_loads_rustls_provider() {
        super::super::endpoint::install_crypto_provider();

        let (cert_chain, key) = ephemeral_cert();
        let pins_dir = ephemeral_pins_dir();
        let _ = std::fs::remove_dir_all(&pins_dir);
        let cfg = build_quic_client_config(vec![cert_chain[0].clone()], key, &pins_dir)
            .expect("ClientConfig 装配不应失败");
        let _clone: quinn::ClientConfig = cfg.clone();
    }

    /// PLAN §2.5 验收：server 端 [`PermissiveClientCertVerifier`] 强制 mTLS
    /// dial → TLS 1.3 内置 `rustls::Error::NoCertificatesPresented` 应在
    /// server 端拒握。
    #[tokio::test]
    async fn mtls_rejects_no_client_cert() {
        use std::net::{Ipv4Addr, SocketAddrV4};
        super::super::endpoint::install_crypto_provider();

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
        let server_addr = server_ep.local_addr().expect("server endpoint 必须有 local_addr");

        let server_task = tokio::spawn(async move {
            let incoming = server_ep.accept().await.expect("server accept 不应失败");
            let result = incoming.await;
            assert!(
                result.is_err(),
                "server 端 handshake 应失败（mTLS 强制 client cert，client 未出示），实际 Ok"
            );
        });

        use rustls::ClientConfig as RustlsClientConfig;
        let (server_cert_chain, _server_key) = ephemeral_cert();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(server_cert_chain[0].clone()).expect("add root");
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let builder = RustlsClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("protocol versions");
        let mut rustls_client = builder.with_root_certificates(roots).with_no_client_auth();
        rustls_client.alpn_protocols = vec![super::super::ALPN_LAN_MOUSE.to_vec()];

        let quic_client =
            quinn::crypto::rustls::QuicClientConfig::try_from(Arc::new(rustls_client))
                .expect("QuicClientConfig try_from");
        let mut client_cfg = quinn::ClientConfig::new(Arc::new(quic_client));
        client_cfg.transport_config(super::default_transport_config());

        let client_ep = super::super::endpoint::endpoint(
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into(),
        )
        .expect("client endpoint bind 不应失败");

        let connecting_outcome = client_ep.connect_with(client_cfg, server_addr, "lan-mouse");
        let handshake_result = match connecting_outcome {
            Ok(connecting) => tokio::time::timeout(
                std::time::Duration::from_secs(5),
                connecting,
            )
            .await
            .expect("dial 端到端超时"),
            Err(_connect_err) => {
                log::debug!("connect_with 同步部分失败（接受）");
                return;
            }
        };

        assert!(
            handshake_result.is_err(),
            "无 client cert 的 dial 应失败（server 端拒握），实际 Ok"
        );

        drop(client_ep);
        let _ = server_task.await;
    }

    /// STEP-2.6 验收 (1/2)：新 fingerprint 被接受并写入 known_peers。
    #[test]
    fn tofu_first_run_pins() {
        super::super::endpoint::install_crypto_provider();

        let pins_dir = tmp_pins_dir("first");
        let verifier = TofuVerifier::new(&pins_dir);

        let (cert_chain, _key) = ephemeral_cert();
        let cert_der = cert_chain[0].clone();
        let fp = crate::crypto::generate_fingerprint(cert_der.as_ref());

        let server_name = test_server_name();
        let now = UnixTime::now();
        let result = verifier.verify_server_cert(&cert_der, &[], &server_name, &[], now);

        assert!(result.is_ok(), "first connect should be accepted (Ok), got {:?}", result);

        let expected_pin = pins_dir.join(format!("{}.pin", fp.replace(':', "_")));
        assert!(
            expected_pin.exists(),
            "pin file should exist at {:?}",
            expected_pin
        );

        let content = std::fs::read(&expected_pin).expect("read pin");
        assert_eq!(
            content, b"trusted\n",
            "pin file content should be 'trusted\\n'"
        );

        let _ = std::fs::remove_dir_all(&pins_dir);
    }

    /// STEP-2.6 验收 (2/2)：不同 fingerprint 被拒绝
    /// （`rustls::Error::General("TOFU mismatch: ...")`）。
    #[test]
    fn tofu_disallows_swap() {
        super::super::endpoint::install_crypto_provider();

        let pins_dir = tmp_pins_dir("swap");

        let (cert1_chain, _key1) = ephemeral_cert();
        let cert1_der = cert1_chain[0].clone();
        let fp1 = crate::crypto::generate_fingerprint(cert1_der.as_ref());
        let verifier = TofuVerifier::with_known(&pins_dir, &fp1);

        let (cert2_chain, _key2) = ephemeral_cert();
        let cert2_der = cert2_chain[0].clone();
        let fp2 = crate::crypto::generate_fingerprint(cert2_der.as_ref());
        assert_ne!(
            fp1, fp2,
            "两个 ephemeral_cert 必须有不同的指纹（rcgen 每次随机）"
        );

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

        let fp1_pin = pins_dir.join(format!("{}.pin", fp1.replace(':', "_")));
        assert!(
            fp1_pin.exists(),
            "mismatch 不应删除已存在的 fp1 pin 文件（pin 应保留）"
        );

        let fp2_pin = pins_dir.join(format!("{}.pin", fp2.replace(':', "_")));
        assert!(
            !fp2_pin.exists(),
            "mismatch 不应自动 pin fp2（陌生 cert 必须保持陌生）"
        );

        let _ = std::fs::remove_dir_all(&pins_dir);
    }

    /// STEP-2.7 验收 (1/2)：allowlist 预填某 fingerprint → `verify_client_cert`
    /// 用对应 cert → `Ok`。
    #[test]
    fn authorized_keys_accepts_known() {
        let allowlist = tmp_allowlist("accepts");

        let (cert_chain, _key) = ephemeral_cert();
        let cert_der = cert_chain[0].clone();

        let fp = crate::crypto::generate_fingerprint(cert_der.as_ref());
        let verifier = AuthorizedKeysVerifier::with_known(allowlist.clone(), &fp);

        let result = <AuthorizedKeysVerifier as rustls::server::danger::ClientCertVerifier>::verify_client_cert(
            &verifier,
            &cert_der,
            &[],
            rustls::pki_types::UnixTime::now(),
        );
        assert!(
            result.is_ok(),
            "allowlist 预填的 fingerprint 应被接受，实际: {result:?}"
        );

        assert!(
            verifier.allowlist().read().unwrap().contains_key(&fp),
            "allowlist 应包含预填 fp"
        );
    }

    /// STEP-2.7 验收 (2/2)：allowlist 不含某 fingerprint → `verify_client_cert`
    /// 用对应 cert → `Err(rustls::Error::General("unauthorized peer {fp}"))`。
    #[test]
    fn authorized_keys_rejects_unknown() {
        let allowlist = tmp_allowlist("rejects");

        let (cert_chain, _key) = ephemeral_cert();
        let cert_der = cert_chain[0].clone();
        let fp = crate::crypto::generate_fingerprint(cert_der.as_ref());
        let verifier = AuthorizedKeysVerifier::new(allowlist.clone());

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

        let err_msg = format!("{:?}", result.unwrap_err());
        assert!(
            err_msg.contains(&fp),
            "Err 消息应包含 fingerprint `{fp}`，实际: {err_msg}"
        );
        assert!(
            err_msg.contains("unauthorized"),
            "Err 消息应包含 'unauthorized' 关键字，实际: {err_msg}"
        );

        assert!(
            !verifier.allowlist().read().unwrap().contains_key(&fp),
            "allowlist 应不含 cert_der 的 fp"
        );
    }

    /// STEP-8.2 验收：rejection channel 接通 — 当 `verify_client_cert`
    /// 在 allowlist 未命中返 Err 时，fingerprint 必须通过 `rejection_tx`
    /// 同步送达 rx。
    #[test]
    fn rejection_channel_forwards_rejected_fingerprint() {
        let allowlist = tmp_allowlist("rejection-tx");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();

        let (cert_chain, _key) = ephemeral_cert();
        let cert_der = cert_chain[0].clone();
        let fp = crate::crypto::generate_fingerprint(cert_der.as_ref());
        let verifier = AuthorizedKeysVerifier::new(allowlist.clone()).with_rejection_tx(tx);

        allowlist
            .write()
            .expect("RwLock poisoned")
            .insert(fp.clone(), String::new());
        let r = <AuthorizedKeysVerifier as rustls::server::danger::ClientCertVerifier>::verify_client_cert(
            &verifier,
            &cert_der,
            &[],
            rustls::pki_types::UnixTime::now(),
        );
        assert!(r.is_ok(), "allowlist 命中应 Ok");
        assert!(
            rx.try_recv().is_err(),
            "allowlist 命中路径不应 send rejection（防止误报）"
        );

        allowlist.write().expect("RwLock poisoned").remove(&fp);
        let r = <AuthorizedKeysVerifier as rustls::server::danger::ClientCertVerifier>::verify_client_cert(
            &verifier,
            &cert_der,
            &[],
            rustls::pki_types::UnixTime::now(),
        );
        assert!(r.is_err(), "allowlist 不命中应 Err");

        let received = rx.try_recv().expect("rejection_tx 应在 Err 路径 send fp");
        assert_eq!(received, fp, "rejection channel 收到的 fp 应与被拒 cert 的 fp 一致");
        assert!(
            rx.try_recv().is_err(),
            "第二次 try_recv 应为空（一次拒绝只 send 一次）"
        );
    }
}