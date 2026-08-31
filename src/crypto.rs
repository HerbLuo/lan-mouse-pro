//! 证书与 TLS 配置（rustls 路径）。
//!
//! STEP-1.1（PLAN-M1.md）把对 `webrtc_dtls::crypto::Certificate` 的依赖
//! 解耦：本模块只暴露 rustls 类型 —— `Vec<CertificateDer<'static>>` +
//! `PrivateKeyDer<'static>`。`Error` 枚举移除 `Dtls(webrtc_dtls::Error)` 变体。
//!
//! STEP-2.4 把 `cert.pem` + `key.pem` 拆成两个文件（#S-4 已解）；
//! `key_path()` 与 `cert_path()` 对称，`load_or_create_server_cert()` 是
//! 公共别名，内部调 `load_or_generate_key_and_cert_der(cert_path(), key_path())`。
//! 同时把"plan §1.1 别名"对齐到 `load_or_create_server_cert` 命名。
//!
//! cert 路径：与 bak 设计对齐 —— `$XDG_DATA_HOME/lan-mouse/{cert,key}.pem`，
//! 回退 `$HOME/.local/share/lan-mouse/{cert,key}.pem`，Windows 走 `%APPDATA%`。
//! key 文件 0o400（Unix）/`FILE_ATTRIBUTE_READONLY`（Windows）保持。

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, ServerConfig};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Rustls(#[from] rustls::Error),
    #[error("PEM parse: {0}")]
    Pem(String),
    #[error("no private key in PEM file")]
    NoKey,
    #[error("rcgen: {0}")]
    Rcgen(#[from] rcgen::Error),
}

/// SHA-256 指纹（hex，用 `:` 分隔），小写。接受任意 DER 字节。
///
/// 与 listen.rs（`Accept { fingerprint }`）+ service.rs（`public_key_fingerprint`）
/// 一致：两条路径只要 DER 字节相同，指纹必然一致。
pub fn generate_fingerprint(cert: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(cert);
    let bytes = hash.finalize();
    bytes
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect::<Vec<_>>()
        .join(":")
        .to_lowercase()
}

// === rustls API（Phase 1 / Phase 2 正式使用）============================

/// 从 PEM 文件加载证书链，返回 `Vec<CertificateDer<'static>>`。
pub fn load_cert_der(path: &Path) -> Result<Vec<CertificateDer<'static>>, Error> {
    let pem = fs::read(path)?;
    let der_bytes = rustls_pemfile::certs(&mut pem.as_slice())
        .map_err(|e| Error::Pem(format!("pem certs parse: {e}")))?;
    Ok(der_bytes.into_iter().map(CertificateDer::from).collect())
}

/// 从 PEM 文件加载 PKCS#8 私钥，返回 `PrivateKeyDer<'static>`。
pub fn load_key_der(path: &Path) -> Result<PrivateKeyDer<'static>, Error> {
    let pem = fs::read(path)?;
    let mut keys = rustls_pemfile::pkcs8_private_keys(&mut pem.as_slice())
        .map_err(|e| Error::Pem(format!("pem pkcs8 parse: {e}")))?;
    let key = keys.pop().ok_or(Error::NoKey)?;
    Ok(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)))
}

/// OS 感知的 server cert 持久化路径。
///
/// - Unix：`$XDG_DATA_HOME/lan-mouse/cert.pem`，回退到
///   `$HOME/.local/share/lan-mouse/cert.pem`，最后回退到当前目录
/// - Windows：`%APPDATA%\lan-mouse\cert.pem`，回退到当前目录
pub fn cert_path() -> PathBuf {
    lan_mouse_data_dir().join("cert.pem")
}

/// OS 感知的 server private key 持久化路径（与 [`cert_path`] 对称）。
///
/// - Unix：`$XDG_DATA_HOME/lan-mouse/key.pem`
/// - Windows：`%APPDATA%\lan-mouse\key.pem`
///
/// STEP-2.4 拆分 `cert.pem` + `key.pem` 后引入（#S-4）；文件权限
/// `0o400`（Unix）/`FILE_ATTRIBUTE_READONLY`（Windows）由
/// [`generate_self_signed`] 在落盘时收紧。
pub fn key_path() -> PathBuf {
    lan_mouse_data_dir().join("key.pem")
}

/// QUIC 客户端 TOFU 指纹缓存目录（STEP-6.1 引入，与 bak
/// `mousehop/src/crypto.rs:264-272 cert_pins_dir` 对齐）。
///
/// - Unix：`$XDG_DATA_HOME/lan-mouse/known_peers/`
/// - Windows：`%APPDATA%\lan-mouse\known_peers\`
///
/// **不**在此函数内 `create_dir_all` —— 由 caller（quic_transport 的
/// `TofuVerifier::new`）按需创建。本函数只返回路径。
///
/// **为什么独立于 `cert_path` / `key_path`**：
/// - TOFU 缓存是 *per-peer* 持久化（每个对端一个 `<fp>.pin` 文件），
///   与 server 自身的 cert/key 生命周期独立
/// - 用户清掉 `known_peers/` 只触发"重新信任对端"语义（首次连接会
///   落新的 pin），不会丢 server 自身的 cert
pub fn cert_pins_dir() -> PathBuf {
    lan_mouse_data_dir().join("known_peers")
}

/// 共享的"应用数据目录"OS 解析逻辑（[`cert_path`] / [`key_path`] 共用）。
///
/// - Unix：`$XDG_DATA_HOME`，回退 `$HOME/.local/share`，最后回退当前目录
/// - Windows：`%APPDATA%`，回退当前目录
fn lan_mouse_data_dir() -> PathBuf {
    #[cfg(unix)]
    {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| {
                    let mut p = PathBuf::from(h);
                    p.push(".local/share");
                    p
                })
            })
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("lan-mouse")
    }
    #[cfg(windows)]
    {
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("lan-mouse")
    }
}

/// 加载已落盘的 server cert + key（分别从 `cert_path` / `key_path` 读）；
/// 任一缺失则自签生成并落盘到对应文件。
///
/// 返回 `(cert_chain, key)`，给 `quic_transport::endpoint_with_cert()` 注入
/// 使用。STEP-2.4 起本函数只接受拆开的双路径（#S-4）；零参数 caller 一律
/// 改走 [`load_or_create_server_cert`]。
pub fn load_or_generate_key_and_cert_der(
    cert_path: &Path,
    key_path: &Path,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), Error> {
    if cert_path.exists() && key_path.exists() && cert_path.is_file() && key_path.is_file() {
        let certs = load_cert_der(cert_path)?;
        let key = load_key_der(key_path)?;
        Ok((certs, key))
    } else {
        generate_self_signed("lan-mouse", cert_path, key_path)
    }
}

/// `load_or_generate_key_and_cert_der(cert_path(), key_path())` 的零参数别名
/// （PLAN §1.1 + STEP-2.4 caller 一致性）。这是 service.rs / quic_transport
/// 生产路径的入口。
pub fn load_or_create_server_cert()
-> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), Error> {
    load_or_generate_key_and_cert_der(&cert_path(), &key_path())
}

/// 自签生成。返回 `(cert_chain, key)`。落盘拆为 `cert_path` (cert PEM) +
/// `key_path` (key PEM) 两个文件（STEP-2.4 / #S-4）；key 文件 Unix 权限 0o400。
pub fn generate_self_signed(
    common_name: &str,
    cert_path: &Path,
    key_path: &Path,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), Error> {
    let cert = rcgen::generate_simple_self_signed(vec![common_name.to_owned()])?;
    let cert_der = CertificateDer::from(cert.cert.der().to_vec());
    let key_der = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()));

    // 落 cert PEM
    if let Some(parent) = cert_path.parent() {
        fs::create_dir_all(parent)?;
    }
    {
        let f = fs::File::create(cert_path)?;
        #[cfg(unix)]
        {
            let mut perm = f.metadata()?.permissions();
            perm.set_mode(0o600);
            f.set_permissions(perm)?;
        }
        let pem = cert.cert.pem();
        let mut writer = std::io::BufWriter::new(f);
        writer.write_all(pem.as_bytes())?;
    }

    // 落 key PEM（0o400，比 cert 更紧）
    if let Some(parent) = key_path.parent() {
        fs::create_dir_all(parent)?;
    }
    {
        let f = fs::File::create(key_path)?;
        #[cfg(unix)]
        {
            let mut perm = f.metadata()?.permissions();
            perm.set_mode(0o400);
            f.set_permissions(perm)?;
        }
        let pem = cert.key_pair.serialize_pem();
        let mut writer = std::io::BufWriter::new(f);
        writer.write_all(pem.as_bytes())?;
    }

    Ok((vec![cert_der], key_der))
}

/// 构造 `Arc<rustls::ServerConfig>`（单证书，no client auth）。
///
/// STEP-2.5 引入 mTLS 后由 `rustls_server_config_with_verifier` 替代；本函数
/// 先以"no client auth"形态对外，作为 STEP-1.4 `endpoint()` 喂给
/// `quinn::ServerConfig` 的最小可用封装。
pub fn rustls_server_config(
    cert_chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<Arc<ServerConfig>, Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)?;
    Ok(Arc::new(cfg))
}

/// STEP-2.5 mTLS 强制 client cert 校验入口 —— 与 `rustls_server_config` 形态
/// 对称，唯一差别是 `with_no_client_auth()` → `with_client_cert_verifier(verifier)`：
/// 前者走 rustls 内置 `NoClientAuth`（不验证 client cert），后者走应用层提供
/// 的 `ClientCertVerifier` 实现（STEP-2.7 `AuthorizedKeysVerifier`，本步仅
/// 装配入口）。`quic_transport::endpoint_with_verifier(...)` 调本函数。
///
/// **verifier 必须 `Send + Sync + 'static`**（rustls 0.23 trait 约束）——
/// `Arc<dyn rustls::server::danger::ClientCertVerifier>` 自动满足。
///
/// 与 `bak/mousehop/src/crypto.rs:238-249 rustls_server_config_with_verifier`
/// 完全对齐；差异仅在 `/// dead_code` 注释（本仓不做 dead_code 守护 —— 单测
/// 与 `endpoint_with_verifier` 接入后自然消费）。
pub fn rustls_server_config_with_verifier(
    cert_chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    verifier: Arc<dyn rustls::server::danger::ClientCertVerifier>,
) -> Result<Arc<ServerConfig>, Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_client_cert_verifier(verifier)
        .with_single_cert(cert_chain, key)?;
    Ok(Arc::new(cfg))
}

/// 构造 `Arc<rustls::ClientConfig>`（带根证书，无 mTLS）。
///
/// server cert 校验下沉到 `quic_transport::TofuVerifier`（STEP-2.6），本函数
/// 只负责 root cert store + 默认 chain 校验 + 无 client auth。
///
/// **STEP-7.3 守护**：虽然 `build_quic_client_config` 不再调本函数（生产路径
/// 已用 TofuVerifier），但 `crypto::tests::round_trip_generate_and_load` 仍用
/// 本函数做"ClientConfig 可构造" 单测契约 —— 证明 cert/key DER 同时能作为
/// server cert 和 client root。保留 + `#[allow(dead_code)]` 让 lib build
/// 不报 dead warning（test build 会用上）。
#[allow(dead_code)]
pub fn rustls_client_config(
    root_cert_der: Vec<CertificateDer<'static>>,
) -> Result<Arc<ClientConfig>, Error> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in root_cert_der {
        roots.add(cert)?;
    }

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(cfg))
}

// === 单元测试 ================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_subdir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lan-mouse-crypto-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn fingerprint_format_is_colon_separated_hex() {
        // SHA-256("hello world") → 标准指纹（95 字符：32 bytes × 3 - 1）
        let fp = generate_fingerprint(b"hello world");
        assert_eq!(
            fp,
            "b9:4d:27:b9:93:4d:3e:08:a5:2e:52:d7:da:7d:ab:fa:c4:84:ef:e3:7a:53:80:ee:90:88:f7:ac:e2:ef:cd:e9"
        );
        assert_eq!(fp.len(), 95);
    }

    #[test]
    fn round_trip_generate_and_load() {
        let dir = tmp_subdir("rt");
        let cp = dir.join("cert.pem");
        let kp = dir.join("key.pem");

        // 自签 + 落盘（双文件）
        let (cert_gen, key_gen) = generate_self_signed("lan-mouse-test", &cp, &kp).unwrap();

        // 落盘后再读，DER 应一致
        let cert_loaded = load_cert_der(&cp).unwrap();
        let key_loaded = load_key_der(&kp).unwrap();

        // 指纹一致
        let fp_gen = generate_fingerprint(cert_gen[0].as_ref());
        let fp_loaded = generate_fingerprint(cert_loaded[0].as_ref());
        assert_eq!(fp_gen, fp_loaded);
        assert_eq!(fp_loaded.len(), 95);

        // ServerConfig 可构造
        let server_cfg = rustls_server_config(cert_loaded.clone(), key_loaded).unwrap();
        assert!(Arc::strong_count(&server_cfg) >= 1);

        // ClientConfig 可构造（同 cert 当 root）
        let client_cfg = rustls_client_config(cert_loaded).unwrap();
        assert!(Arc::strong_count(&client_cfg) >= 1);

        // key 不可丢弃
        let _ = key_gen;

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_cert_der_returns_empty_for_empty_pem() {
        let dir = tmp_subdir("empty");
        let path = dir.join("empty.pem");
        fs::write(&path, b"").unwrap();
        let certs = load_cert_der(&path).unwrap();
        assert!(certs.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_key_der_errors_when_no_key() {
        let dir = tmp_subdir("nokey");
        let path = dir.join("no_key.pem");
        fs::write(&path, b"some random bytes\n").unwrap();
        assert!(matches!(load_key_der(&path), Err(Error::NoKey)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn generated_cert_is_unix_readonly() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_subdir("perm");
        let cp = dir.join("cert.pem");
        let kp = dir.join("key.pem");

        let _ = generate_self_signed("lan-mouse-perm-test", &cp, &kp).unwrap();

        // cert 0o600，key 0o400
        let cert_mode = fs::metadata(&cp).unwrap().permissions().mode() & 0o777;
        let key_mode = fs::metadata(&kp).unwrap().permissions().mode() & 0o777;
        assert_eq!(cert_mode, 0o600, "cert 0o600, got {cert_mode:o}");
        assert_eq!(key_mode, 0o400, "key 0o400, got {key_mode:o}");

        let _ = fs::remove_dir_all(&dir);
    }

    /// STEP-2.4 验收 #1：首次落盘 → 二次加载 → 指纹一致（持久化 identity 稳定）。
    #[test]
    fn load_or_generate_key_and_cert_der_persists_identity() {
        let dir = tmp_subdir("persist");
        let cp = dir.join("cert.pem");
        let kp = dir.join("key.pem");

        // 首次：自签 + 落盘
        let (c1, k1) = load_or_generate_key_and_cert_der(&cp, &kp).unwrap();
        let fp1 = generate_fingerprint(c1[0].as_ref());

        // 二次：从磁盘读；不应该重新自签
        let (c2, k2) = load_or_generate_key_and_cert_der(&cp, &kp).unwrap();
        let fp2 = generate_fingerprint(c2[0].as_ref());

        assert_eq!(fp1, fp2, "二次加载 fingerprint 应一致");
        assert_eq!(c1[0].as_ref(), c2[0].as_ref(), "二次加载 cert DER 应一致");
        assert_eq!(k1.secret_der(), k2.secret_der(), "二次加载 key DER 应一致");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 钉契约：M1 STEP-7.3 起整个 workspace **不应** 再依赖 webrtc-dtls / webrtc-util。
    /// 任何后续 PR 加回这俩依赖时该测试立即红。
    #[test]
    fn workspace_has_no_webrtc_dtls_or_webrtc_util() {
        const ROOT_TOML: &str = include_str!("../Cargo.toml");
        assert!(
            !ROOT_TOML.contains("webrtc-dtls"),
            "workspace Cargo.toml 不应出现 webrtc-dtls（M1 STEP-7.3 已下线 DTLS）"
        );
        assert!(
            !ROOT_TOML.contains("webrtc-util"),
            "workspace Cargo.toml 不应出现 webrtc-util（M1 STEP-7.3 已下线）"
        );
    }
}
