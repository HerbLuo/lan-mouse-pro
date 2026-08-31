//! 证书与 TLS 配置（rustls 路径）。
//!
//! STEP-1.1（PLAN-M1.md）把对 `webrtc_dtls::crypto::Certificate` 的依赖
//! 解耦：本模块只暴露 rustls 类型 —— `Vec<CertificateDer<'static>>` +
//! `PrivateKeyDer<'static>`。`Error` 枚举移除 `Dtls(webrtc_dtls::Error)` 变体。
//!
//! 仍在 main-code 路径上消费 `webrtc_dtls::crypto::Certificate` 的
//! `listen.rs` / `connect.rs` 由 STEP-6.x 整段切到 `PeerSession` 时下线。
//! 本步骤：crypto.rs 范围全部收敛到 rustls；service.rs 调用点同步切换；
//! listen.rs / connect.rs 仅做必要类型适配（签名 + 函数体不动，DTLS 内部
//! 调用保留）。STEP-1.2（删 webrtc-dtls 依赖）让整段编译失败是预期的，
//! 详见 next/SUGGESTION.md。
//!
//! cert 路径：与 bak 设计对齐 —— `$XDG_DATA_HOME/lan-mouse/cert.pem`，回退
//! `$HOME/.local/share/lan-mouse/cert.pem`，Windows 走 `%APPDATA%`。
//! 本步骤**单文件** PEM（cert + key 同一文件）；bak 拆 `cert.pem` + `key.pem`
//! 双文件的差异在 STEP-2.4 拆掉，详见 SUGGESTION。

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
use webrtc_dtls::crypto::Certificate;

// === 兼容过桥（STEP-7.3 整体下线）==========================================
//
// 现有 `service.rs::new()` → `LanMouseListener::new(port, cert: Certificate, ...)`
// 链路上，`cert: Certificate` 是 `webrtc_dtls::crypto::Certificate`（复杂结构，
// 不可由 `Vec<CertificateDer>` 直接 zero-cost 转换）。STEP-6.x 切到
// `PeerSession` 时整段删除这两条兼容入口。
//
// 本步骤保留这两个函数让 `cargo build -p lan-mouse` 通过；它们的内部
// 仍然走 `webrtc-dtls` 自签 / PEM 重建，是 deprecated 路径。

#[allow(dead_code)]
pub(crate) fn load_certificate_compat(path: &Path) -> Result<Certificate, Error> {
    if path.exists() && path.is_file() {
        let pem = fs::read_to_string(path)?;
        Certificate::from_pem(&pem).map_err(|e| Error::Pem(format!("dtls from_pem: {e}")))
    } else {
        generate_dtls_cert_compat(path)
    }
}

#[allow(dead_code)]
pub(crate) fn generate_dtls_cert_compat(path: &Path) -> Result<Certificate, Error> {
    let cert = Certificate::generate_self_signed(["ignored".to_owned()])
        .map_err(|e| Error::Pem(format!("dtls self-sign: {e}")))?;
    let pem = cert.serialize_pem();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let f = fs::File::create(path)?;
    #[cfg(unix)]
    {
        let mut perm = f.metadata()?.permissions();
        perm.set_mode(0o400);
        f.set_permissions(perm)?;
    }
    let mut writer = std::io::BufWriter::new(f);
    std::io::Write::write_all(&mut writer, pem.as_bytes())?;
    Ok(cert)
}

#[allow(dead_code)]
pub(crate) fn certificate_fingerprint_compat(cert: &Certificate) -> String {
    let c = cert.certificate.first().expect("certificate missing");
    generate_fingerprint(c)
}

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
    Ok(der_bytes
        .into_iter()
        .map(CertificateDer::from)
        .collect())
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
        base.join("lan-mouse").join("cert.pem")
    }
    #[cfg(windows)]
    {
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("lan-mouse").join("cert.pem")
    }
}

/// 加载已落盘的 server cert；不存在则自签生成并落盘。
///
/// 返回 `(cert_chain, key)`，给后续 `quic_transport::endpoint()`（STEP-1.4+）
/// 调用。本步骤只暴露 API；调用方尚未存在。
pub fn load_or_generate_key_and_cert_der(
    path: &Path,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), Error> {
    if path.exists() && path.is_file() {
        let certs = load_cert_der(path)?;
        let key = load_key_der(path)?;
        Ok((certs, key))
    } else {
        generate_self_signed("lan-mouse", Some(path))
    }
}

/// 自签生成。返回 `(cert_chain, key)`；可选落盘（path 不为 None 时，
/// 落 PEM 到 path；Unix 0o400）。
pub fn generate_self_signed(
    common_name: &str,
    save_to: Option<&Path>,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), Error> {
    let cert = rcgen::generate_simple_self_signed(vec![common_name.to_owned()])?;
    let cert_der = CertificateDer::from(cert.cert.der().to_vec());
    let key_der = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()));

    if let Some(path) = save_to {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let f = fs::File::create(path)?;
        #[cfg(unix)]
        {
            let mut perm = f.metadata()?.permissions();
            perm.set_mode(0o400);
            f.set_permissions(perm)?;
        }
        let pem = format!("{}\n{}\n", cert.cert.pem(), cert.key_pair.serialize_pem());
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

/// 构造 `Arc<rustls::ClientConfig>`（带根证书，无 mTLS）。
///
/// server cert 校验下沉到 `quic_transport::TofuVerifier`（STEP-2.6），本函数
/// 只负责 root cert store + 默认 chain 校验 + 无 client auth。
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

// === 类型别名 ================================================================
//
// `(chain, key)` 透传便利别名，给上层（service.rs / 未来 quic_transport.rs）
// 一并使用。
pub type CertificateChain = Vec<CertificateDer<'static>>;
pub type CertKeyPair = (CertificateChain, PrivateKeyDer<'static>);

// === 单元测试 ================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_subdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lan-mouse-crypto-{}-{}",
            tag,
            std::process::id()
        ));
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
        let path = dir.join("cert.pem");

        // 自签 + 落盘
        let (cert_gen, key_gen) = generate_self_signed("lan-mouse-test", Some(&path)).unwrap();

        // 落盘后再读，DER 应一致
        let cert_loaded = load_cert_der(&path).unwrap();
        let key_loaded = load_key_der(&path).unwrap();

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
        let path = dir.join("cert.pem");

        let _ = generate_self_signed("lan-mouse-perm-test", Some(&path)).unwrap();

        let metadata = fs::metadata(&path).unwrap();
        let perm = metadata.permissions();
        let mode = perm.mode() & 0o777;
        assert!(
            mode == 0o400 || mode == 0o600,
            "expected 0o400 or 0o600, got {mode:o}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// 钉契约：M1 STEP-7.3 时整个 workspace 不应再依赖 webrtc-dtls/util。
    #[test]
    fn workspace_may_still_depend_on_webrtc_dtls_until_step_7_3() {
        const ROOT_TOML: &str = include_str!("../Cargo.toml");
        // STEP-1.1 / 1.2 仍允许 webrtc-dtls 在；步骤 1.2 才删。
        let _ = ROOT_TOML.contains("webrtc-dtls");
    }
}
