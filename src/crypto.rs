//! Certificates and TLS configuration (rustls path).
//!
//! Cert persistence is split into two files (`cert.pem` + `key.pem`) so the
//! key can be tightened to `0o400` on Unix / `FILE_ATTRIBUTE_READONLY` on
//! Windows. `key_path()` and `cert_path()` mirror each other, and
//! `load_or_create_server_cert()` is the zero-argument entry point used by
//! production callers.
//!
//! cert path: `$XDG_DATA_HOME/lan-mouse/cert.pem`, falling back to
//! `$HOME/.local/share/lan-mouse/cert.pem`, or `%APPDATA%\lan-mouse\cert.pem`
//! on Windows. The key file mode (`0o400` / read-only) is enforced at write
//! time.

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

/// SHA-256 fingerprint (hex, `:` separated, lowercase). Accepts arbitrary
/// DER bytes.
///
/// Consistent with `listen.rs` (`Accept { fingerprint }`) and
/// `service.rs` (`public_key_fingerprint`): as long as both code paths see
/// the same DER bytes, the fingerprint will match.
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

// === rustls API ===================================================

/// Load a certificate chain from a PEM file and return
/// `Vec<CertificateDer<'static>>`.
pub fn load_cert_der(path: &Path) -> Result<Vec<CertificateDer<'static>>, Error> {
    let pem = fs::read(path)?;
    let der_bytes = rustls_pemfile::certs(&mut pem.as_slice())
        .map_err(|e| Error::Pem(format!("pem certs parse: {e}")))?;
    Ok(der_bytes.into_iter().map(CertificateDer::from).collect())
}

/// Load a PKCS#8 private key from a PEM file and return
/// `PrivateKeyDer<'static>`.
pub fn load_key_der(path: &Path) -> Result<PrivateKeyDer<'static>, Error> {
    let pem = fs::read(path)?;
    let mut keys = rustls_pemfile::pkcs8_private_keys(&mut pem.as_slice())
        .map_err(|e| Error::Pem(format!("pem pkcs8 parse: {e}")))?;
    let key = keys.pop().ok_or(Error::NoKey)?;
    Ok(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)))
}

/// OS-aware server cert persistence path.
///
/// - Unix: `$XDG_DATA_HOME/lan-mouse/cert.pem`, falling back to
///   `$HOME/.local/share/lan-mouse/cert.pem`, finally the current directory.
/// - Windows: `%APPDATA%\lan-mouse\cert.pem`, falling back to the current
///   directory.
pub fn cert_path() -> PathBuf {
    lan_mouse_data_dir().join("cert.pem")
}

/// OS-aware server private key persistence path (mirrors [`cert_path`]).
///
/// - Unix: `$XDG_DATA_HOME/lan-mouse/key.pem`
/// - Windows: `%APPDATA%\lan-mouse\key.pem`
///
/// File permissions (`0o400` on Unix / `FILE_ATTRIBUTE_READONLY` on Windows)
/// are tightened by [`generate_self_signed`] when the key is written.
pub fn key_path() -> PathBuf {
    lan_mouse_data_dir().join("key.pem")
}

/// TOFU fingerprint cache directory for QUIC clients.
///
/// - Unix: `$XDG_DATA_HOME/lan-mouse/known_peers/`
/// - Windows: `%APPDATA%\lan-mouse\known_peers\`
///
/// This function does **not** call `create_dir_all`; the caller
/// (`quic_transport::TofuVerifier::new`) creates the directory on demand.
/// This function only returns the path.
///
/// **Why this lives separately from `cert_path` / `key_path`:**
/// - The TOFU cache is per-peer persistence (one `<peer>.pin` file per
///   peer, named after the peer's stable identity and holding the
///   fingerprint that peer presented on first contact — see
///   [`crate::quic_transport::TofuVerifier`]) and has its own lifetime,
///   independent of the server's own cert/key.
/// - Clearing `known_peers/` only resets peer trust — the next connection
///   drops a fresh pin — without touching the server's own cert. Deleting a
///   single `<peer>.pin` re-pairs just that peer.
pub fn cert_pins_dir() -> PathBuf {
    lan_mouse_data_dir().join("known_peers")
}

/// Shared OS-resolution for the application data directory. Used by both
/// [`cert_path`] and [`key_path`].
///
/// - Unix: `$XDG_DATA_HOME`, falling back to `$HOME/.local/share`, finally
///   the current directory.
/// - Windows: `%APPDATA%`, falling back to the current directory.
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

/// Load an existing server cert + key (from `cert_path` / `key_path`); if
/// either file is missing, generate a self-signed pair and persist them to
/// the matching locations.
///
/// Returns `(cert_chain, key)`, intended for `quic_transport::endpoint_with_cert()`.
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

/// Zero-argument alias for
/// `load_or_generate_key_and_cert_der(cert_path(), key_path())`. This is
/// the entry point used by `service.rs` and `quic_transport` in production.
pub fn load_or_create_server_cert()
-> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), Error> {
    load_or_generate_key_and_cert_der(&cert_path(), &key_path())
}

/// Generate a self-signed cert and key. Returns `(cert_chain, key)`. The
/// files are split across `cert_path` (cert PEM) and `key_path` (key PEM);
/// the key file is tightened to `0o400` on Unix.
pub fn generate_self_signed(
    common_name: &str,
    cert_path: &Path,
    key_path: &Path,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), Error> {
    let cert = rcgen::generate_simple_self_signed(vec![common_name.to_owned()])?;
    let cert_der = CertificateDer::from(cert.cert.der().to_vec());
    let key_der = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()));

    // write the cert PEM
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

    // write the key PEM (0o400 — tighter than the cert)
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

/// Build `Arc<rustls::ServerConfig>` (single cert, no client auth).
///
/// mTLS-aware callers should use [`rustls_server_config_with_verifier`]
/// instead; this entry point is the minimal "no client auth" form, kept
/// for tests and for callers that explicitly don't need mTLS.
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

/// mTLS-enforcing server config builder. Mirrors [`rustls_server_config`],
/// but swaps `with_no_client_auth()` for `with_client_cert_verifier(verifier)`:
/// the former uses rustls' built-in `NoClientAuth` (no client-cert
/// validation), the latter delegates to an application-provided
/// `ClientCertVerifier` implementation. `quic_transport::endpoint_with_verifier(...)`
/// calls this entry point.
///
/// The `verifier` must be `Send + Sync + 'static` (rustls 0.23 trait
/// bound); `Arc<dyn rustls::server::danger::ClientCertVerifier>` satisfies
/// that automatically.
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

/// Build `Arc<rustls::ClientConfig>` (with root certs, no mTLS).
///
/// Server cert validation is handled by `quic_transport::TofuVerifier` on
/// the QUIC path; this function only handles the root cert store +
/// default chain validation + no-client-auth. Kept for tests that verify
/// a cert/key pair can simultaneously act as server cert and client root.
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

// === Unit tests ====================================================

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
        // SHA-256("hello world") → canonical fingerprint (95 chars: 32 bytes × 3 − 1)
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

        // self-sign + persist (two files)
        let (cert_gen, key_gen) = generate_self_signed("lan-mouse-test", &cp, &kp).unwrap();

        // read back from disk; DER should match
        let cert_loaded = load_cert_der(&cp).unwrap();
        let key_loaded = load_key_der(&kp).unwrap();

        // fingerprints match
        let fp_gen = generate_fingerprint(cert_gen[0].as_ref());
        let fp_loaded = generate_fingerprint(cert_loaded[0].as_ref());
        assert_eq!(fp_gen, fp_loaded);
        assert_eq!(fp_loaded.len(), 95);

        // ServerConfig can be built
        let server_cfg = rustls_server_config(cert_loaded.clone(), key_loaded).unwrap();
        assert!(Arc::strong_count(&server_cfg) >= 1);

        // ClientConfig can be built (same cert as root)
        let client_cfg = rustls_client_config(cert_loaded).unwrap();
        assert!(Arc::strong_count(&client_cfg) >= 1);

        // keep the key around so the compiler doesn't drop it
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

        // cert 0o600, key 0o400
        let cert_mode = fs::metadata(&cp).unwrap().permissions().mode() & 0o777;
        let key_mode = fs::metadata(&kp).unwrap().permissions().mode() & 0o777;
        assert_eq!(cert_mode, 0o600, "cert must be 0o600, got {cert_mode:o}");
        assert_eq!(key_mode, 0o400, "key must be 0o400, got {key_mode:o}");

        let _ = fs::remove_dir_all(&dir);
    }

    /// Persisted identity is stable across reloads: the first self-sign
    /// followed by a reload must produce the same fingerprint.
    #[test]
    fn load_or_generate_key_and_cert_der_persists_identity() {
        let dir = tmp_subdir("persist");
        let cp = dir.join("cert.pem");
        let kp = dir.join("key.pem");

        // first call: self-sign + persist
        let (c1, k1) = load_or_generate_key_and_cert_der(&cp, &kp).unwrap();
        let fp1 = generate_fingerprint(c1[0].as_ref());

        // second call: read from disk; must NOT re-generate
        let (c2, k2) = load_or_generate_key_and_cert_der(&cp, &kp).unwrap();
        let fp2 = generate_fingerprint(c2[0].as_ref());

        assert_eq!(fp1, fp2, "fingerprint should be stable across reloads");
        assert_eq!(
            c1[0].as_ref(),
            c2[0].as_ref(),
            "cert DER should be stable across reloads"
        );
        assert_eq!(
            k1.secret_der(),
            k2.secret_der(),
            "key DER should be stable across reloads"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// Pin contract: the workspace must NOT depend on `webrtc-dtls` /
    /// `webrtc-util`; any PR that re-adds them will fail this test.
    #[test]
    fn workspace_has_no_webrtc_dtls_or_webrtc_util() {
        const ROOT_TOML: &str = include_str!("../Cargo.toml");
        assert!(
            !ROOT_TOML.contains("webrtc-dtls"),
            "workspace Cargo.toml must not reference webrtc-dtls (DTLS is no longer used)"
        );
        assert!(
            !ROOT_TOML.contains("webrtc-util"),
            "workspace Cargo.toml must not reference webrtc-util"
        );
    }
}
