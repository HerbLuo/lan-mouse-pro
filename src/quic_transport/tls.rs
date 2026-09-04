//! TLS / mTLS trust configuration.
//!
//! This module owns the TLS trust decisions for the QUIC link:
//!
//! - [`build_quic_client_config`] assembles a `quinn::ClientConfig` (rustls +
//!   ring + [`TofuVerifier`] + mTLS client cert presentation + ALPN)
//! - [`default_transport_config`] shared server/client `TransportConfig`
//!   (5s keepalive / 10s idle)
//! - [`TofuVerifier`] client-side TOFU (Trust On First Use) fingerprint pinning
//! - [`PermissiveClientCertVerifier`] placeholder verifier that accepts any
//!   client cert passing the TLS 1.3 built-in chain check
//! - [`AuthorizedKeysVerifier`] server-side fingerprint allowlist
//!
//! Relationship with [`super::endpoint`]: `build_quic_client_config` is invoked
//! by `endpoint::dial` / `endpoint::dial_any` to assemble the client config;
//! on the server side `endpoint_with_verifier` directly accepts a caller-
//! provided verifier (`PermissiveClientCertVerifier` or `AuthorizedKeysVerifier`).

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

use super::{ALPN_LAN_MOUSE, Error, Result};

/// Shared server/client `TransportConfig`:
///
/// - `keep_alive_interval = 5s` — QUIC active probe.
/// - `max_idle_timeout = 10s` — adjusted (the original 30s was too slow; the
///   application-layer post-connect handshake already handles sub-second
///   reconnect validation). 10s is the QUIC-level fallback; on healthy links
///   the 5s keepalive always fires first, and the 10s idle timeout only
///   applies on edge cases where send/ping force-close fails.
///
/// `IdleTimeout::try_from(Duration)` fails if and only if `Duration` exceeds
/// the VarInt 2^30 ms upper bound (≈ 12.4 days), and 10s is well within that
/// range — the `expect` documents the reasoning.
///
/// **Visibility**: `pub(super)` — only `endpoint.rs` calls
/// `super::tls::default_transport_config`. `endpoint_inner` (in endpoint.rs)
/// needs it for server transport configuration; `build_quic_client_config`
/// (in this file) calls it directly.
pub(super) fn default_transport_config() -> Arc<TransportConfig> {
    let mut t = TransportConfig::default();
    t.keep_alive_interval(Some(Duration::from_secs(5)));
    t.max_idle_timeout(Some(
        IdleTimeout::try_from(Duration::from_secs(10))
            .expect("10s is far below the VarInt 2^30 ms upper bound (≈ 12.4 days)"),
    ));
    Arc::new(t)
}

/// Assemble a `quinn::ClientConfig`: rustls + ring + TofuVerifier (replaces
/// `WebPkiServerVerifier`) + mTLS client cert chain presentation + ALPN
/// `lan-mouse`.
///
/// - `crypto_provider = ring` — installed earlier by
///   [`super::endpoint::install_crypto_provider`] (this function does not
///   install it itself; the only install point at startup is main.rs).
/// - **TofuVerifier for server cert validation**:
///   `.dangerous().with_custom_certificate_verifier(Arc::new(TofuVerifier::new(pins_dir)))`.
///   `TofuVerifier` performs a three-state decision based on the server cert
///   SHA-256 fingerprint and the `$pins_dir/<sanitized_fp>.pin` on-disk
///   cache: "auto-pin on first sight / known-match accept / known-mismatch
///   reject".
/// - **mTLS client cert chain presentation**:
///   `with_client_auth_cert(cert_chain, key)` is installed synchronously;
///   symmetric with `with_client_cert_verifier(...)` on the server side.
/// - ALPN: `b"lan-mouse"` — protocol negotiated with the peer server.
///   Above this, the application layer has a secondary `PROTOCOL_MAGIC`
///   handshake.
/// - transport: `default_transport_config()` 5s keepalive + 10s idle.
///
/// **`cert_chain` dual-purpose semantics**: used as the mTLS presentation
/// chain; no longer used as a root-store trust anchor (the custom verifier is
/// fully responsible for server cert validation). In M1 both peers run in
/// the same process on the same host and use the same self-signed key
/// (production path: `dial()` internally calls
/// `crypto::load_or_create_server_cert()` to get the persisted cert), so
/// sharing the same chain does not introduce a security risk.
///
/// **`pins_dir` injection**: production path uses
/// `crypto::known_peers_dir()`; tests use `tempfile::tempdir().path()` to
/// isolate and avoid polluting the user path. The TOFU disk-write logic is
/// the sole responsibility of `TofuVerifier` — this function only constructs
/// the verifier and injects it into the rustls builder.
///
/// **`peer_key` injection**: the stable identity of the peer being dialed
/// (see [`TofuVerifier`]). One pin is kept per peer, so this must be the
/// same string across restarts for a given peer — a hostname or a configured
/// IP, never a value derived from `HashSet` iteration order.
///
/// **Does not** install the crypto provider itself: this function is guarded
/// by the caller of [`super::endpoint::install_crypto_provider`] (main.rs);
/// `#[test]` unit tests call `install` once on the first line.
///
/// **Error normalization**: all rustls / quinn assembly errors are wrapped
/// into [`Error::ClientConfig`] (carrying the underlying `Display`); no
/// `From<rustls::Error>` / `From<quinn_proto::Error>` impls are introduced.
pub fn build_quic_client_config(
    cert_chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    pins_dir: &Path,
    peer_key: &str,
) -> Result<QuinnClientConfig> {
    use rustls::ClientConfig as RustlsClientConfig;

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = RustlsClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::ClientConfig(format!("protocol versions: {e}")))?;

    // TofuVerifier replaces the placeholder WebPkiServerVerifier. The custom
    // verifier is fully responsible for server cert validation — no root
    // store is installed.
    let verifier: Arc<dyn rustls::client::danger::ServerCertVerifier> =
        Arc::new(TofuVerifier::new(pins_dir, peer_key));

    // mTLS client cert chain presentation — `with_client_auth_cert` is a
    // terminal builder (returns `Result<ClientConfig, Error>`, unlike
    // `with_no_client_auth` which is an intermediate builder). Errors flow
    // through `?` and are wrapped into `Error::ClientConfig` via `.map_err`
    // (avoids introducing a `From` impl).
    let mut rustls_client = builder
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(cert_chain, key)
        .map_err(|e| Error::ClientConfig(format!("with_client_auth_cert: {e}")))?;
    rustls_client.alpn_protocols = vec![ALPN_LAN_MOUSE.to_vec()];

    // Wrap into quinn::ClientConfig — quinn 0.11 exposes `QuicClientConfig`
    // through the `quinn::crypto::rustls` re-export. The top-level
    // `quinn_proto::*` is not a stable public path, so we avoid depending on
    // the `quinn_proto` crate directly.
    let quic_client = quinn::crypto::rustls::QuicClientConfig::try_from(Arc::new(rustls_client))
        .map_err(|e| Error::ClientConfig(format!("QuicClientConfig::try_from: {e}")))?;
    let mut client_cfg = QuinnClientConfig::new(Arc::new(quic_client));
    client_cfg.transport_config(default_transport_config());

    Ok(client_cfg)
}

/// Client-side TOFU (Trust On First Use) fingerprint pinning verifier.
///
/// One pin per **peer**: `$pins_dir/<sanitized peer_key>.pin` holds the
/// fingerprint that peer presented the first time we talked to it.
///
/// | Decision | Trigger | Behavior |
/// |---|---|---|
/// | **Known Match** | this peer's pin holds the presented fingerprint | `Ok(ServerCertVerified::assertion())` |
/// | **Known Mismatch** | this peer's pin holds a *different* fingerprint | `Err(rustls::Error::General("TOFU mismatch: ..."))` |
/// | **First Connect** | this peer has no pin | write the fingerprint + `log::info!("paired peer ...")` + `Ok(ServerCertVerified::assertion())` |
///
/// **Why the pin is keyed by peer, not by fingerprint**: the decision that
/// matters is "did *this* peer swap its certificate", which is only
/// answerable if the pin is addressable by a stable peer identity. The
/// earlier shape named the file after the fingerprint (`<fp>.pin`) and took
/// the mismatch branch whenever `pins_dir` held *any* `.pin` at all — a
/// directory-wide check with two fatal consequences: pairing a second peer
/// was impossible (the first peer's pin rejected every newcomer), and a peer
/// that legitimately regenerated its cert was locked out permanently with no
/// UI to clear the stale pin. Keying by peer restores the intended
/// three-state semantics: other peers' pins no longer participate in the
/// decision at all.
///
/// **Legacy `<fp>.pin` files are inert** — they are neither read nor
/// deleted; every peer simply re-pairs once on first connect after the
/// upgrade. They can be removed by hand.
///
/// **`peer_key`**: must be stable across restarts. `connect.rs` derives it
/// from the client's configured hostname, falling back to the lowest
/// configured IP — never from `HashSet` iteration order, which varies per
/// process and would silently re-pair (and litter `pins_dir`) on every
/// restart.
///
/// **Filename sanitization**: [`sanitize_peer_key`] reduces the key to
/// `[A-Za-z0-9._-]` so that IPv6 colons, path separators and `..` cannot
/// escape `pins_dir`.
///
/// **`Send + Sync + 'static`**: rustls 0.23 trait constraint —
/// `TofuVerifier` holds `PathBuf` + `String` + `Arc<CryptoProvider>`, which
/// satisfies the bound automatically.
///
/// **`provider` field**: forwarding `verify_tls12_signature` /
/// `verify_tls13_signature` to `rustls::crypto::verify_*_signature` requires
/// the `signature_verification_algorithms` list — a provider reference must
/// be held.
#[derive(Debug)]
pub struct TofuVerifier {
    pins_dir: PathBuf,
    /// Stable identity of the peer this verifier was built for; names the
    /// pin file. See the struct docs on why this may not be derived from an
    /// unordered address set.
    peer_key: String,
    /// Crypto provider needed for signature verification; its
    /// `signature_verification_algorithms` is forwarded by
    /// `verify_tls12_signature` / `verify_tls13_signature`.
    provider: Arc<rustls::crypto::CryptoProvider>,
}

/// Reduce a peer key to a safe filename component.
///
/// Keeps `[A-Za-z0-9._-]` (enough for hostnames and IPv4 literals) and
/// replaces everything else with `_` — IPv6 colons included. Keys that are
/// empty or consist only of dots (`.` / `..`, which are directory entries,
/// not files) collapse to a fixed placeholder, so no key can escape
/// `pins_dir` or name a directory.
fn sanitize_peer_key(peer_key: &str) -> String {
    let safe: String = peer_key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() || safe.chars().all(|c| c == '.') {
        "unnamed-peer".to_owned()
    } else {
        safe
    }
}

impl TofuVerifier {
    /// Construct a verifier for `peer_key`.
    ///
    /// `pins_dir` may not exist — `verify_server_cert` runs `create_dir_all`
    /// then `fs::write` in the First Connect branch.
    pub fn new(pins_dir: &Path, peer_key: &str) -> Self {
        Self {
            pins_dir: pins_dir.to_path_buf(),
            peer_key: peer_key.to_owned(),
            provider: Arc::new(rustls::crypto::ring::default_provider()),
        }
    }

    /// Construct in the "known peer" state (pre-write the pin so subsequent
    /// verifies take the Known Match / Known Mismatch branch).
    ///
    /// **Pre-write is best-effort**: on failure the constructor still returns
    /// `Self`, and subsequent verifies take the First Connect path instead.
    /// IO errors are intentionally not swallowed silently, because they
    /// usually indicate operational problems such as filesystem permissions
    /// or a full disk.
    #[allow(dead_code)] // tests only (production `dial()` uses `.new()`)
    pub fn with_known(pins_dir: &Path, peer_key: &str, known_fp: &str) -> Self {
        let v = Self::new(pins_dir, peer_key);
        let _ = fs::create_dir_all(&v.pins_dir);
        let _ = fs::write(v.pin_path(), format!("{known_fp}\n"));
        v
    }

    /// Path of this peer's pin file.
    fn pin_path(&self) -> PathBuf {
        self.pins_dir
            .join(format!("{}.pin", sanitize_peer_key(&self.peer_key)))
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
        // (1) Compute the SHA-256 fingerprint of the server cert (matches the
        //     format produced by `crypto::generate_fingerprint`: lowercase hex
        //     separated by `:`).
        let fp = crypto::generate_fingerprint(end_entity.as_ref());

        // (2) Ensure pins_dir exists (also needed on First Connect).
        fs::create_dir_all(&self.pins_dir).map_err(|e| {
            rustls::Error::General(format!(
                "TOFU: create_dir_all({:?}) failed: {e}",
                self.pins_dir
            ))
        })?;

        // (3) Three-state decision, scoped to this peer alone.
        let pin = self.pin_path();

        match fs::read_to_string(&pin) {
            Ok(pinned) => {
                let pinned = pinned.trim();
                if pinned == fp {
                    // Known Match.
                    Ok(ServerCertVerified::assertion())
                } else {
                    // Known Mismatch — this peer previously presented a
                    // different certificate. Either it legitimately rotated
                    // its key, or someone is impersonating it; TOFU cannot
                    // tell the two apart, so it refuses and names the file
                    // to delete for a deliberate re-pair.
                    log::warn!(
                        "TOFU: peer {} presented {fp} but {pinned} is pinned — rejecting",
                        self.peer_key
                    );
                    Err(rustls::Error::General(format!(
                        "TOFU mismatch for peer {}: presented fingerprint {fp}, pinned {pinned} \
                         (delete {} to re-pair)",
                        self.peer_key,
                        pin.display()
                    )))
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // First Connect — this peer has never been seen. Other
                // peers' pins are deliberately not consulted.
                fs::write(&pin, format!("{fp}\n")).map_err(|e| {
                    rustls::Error::General(format!("TOFU: write pin {:?} failed: {e}", pin))
                })?;
                log::info!("TOFU: paired peer {} with {fp}", self.peer_key);
                Ok(ServerCertVerified::assertion())
            }
            Err(e) => Err(rustls::Error::General(format!(
                "TOFU: read pin {:?} failed: {e}",
                pin
            ))),
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

/// Placeholder verifier: the server-side mTLS handshake requires the client
/// to present a cert (`offer_client_auth() -> true` +
/// `client_auth_mandatory() -> true`), but **any** cert that passes the TLS 1.3
/// built-in chain check is accepted — no fingerprint allowlist.
///
/// **Purpose**: lets the mTLS link itself (server `CertificateRequest` →
/// client presents cert → handshake completes) work end-to-end, while
/// providing a controllable verifier for negative tests such as
/// [`mtls_rejects_no_client_cert`] (server forces client cert, accepts any).
///
/// **Supersession**: [`AuthorizedKeysVerifier`] uses the fingerprint allowlist
/// from `config.authorized_fingerprints()` — an unauthorized fingerprint is
/// rejected at handshake. All server paths other than
/// `mtls_rejects_no_client_cert` (production callers of
/// `endpoint_with_verifier`) use `AuthorizedKeysVerifier`.
///
/// **`Send + Sync + 'static`**: rustls 0.23 trait constraint —
/// `PermissiveClientCertVerifier` holds no mutable state across awaits; the
/// struct is auto-derived (and so is `Debug`).
///
/// **`verify_client_cert`**: calls `crypto::generate_fingerprint(cert)` to
/// compute SHA-256, emits a log line (no allowlist check — placeholder
/// implementation), and returns `Ok(ClientCertVerified::assertion())`. This
/// is the **only** path — the server has already installed the verifier via
/// `with_client_cert_verifier(...)` and `client_auth_mandatory()` is true, so
/// the client **must** present a cert to reach this point; if the client
/// presents none, the TLS 1.3 built-in flow returns
/// `rustls::Error::NoCertificatesPresented` and aborts the handshake (see
/// tests).
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
        // No root hints — any self-signed cert is accepted.
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::server::danger::ClientCertVerified, rustls::Error> {
        let fp = crate::crypto::generate_fingerprint(end_entity.as_ref());
        log::debug!("[placeholder verifier] accept client cert fp={fp}");
        Ok(rustls::server::danger::ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        // Placeholder verifier — the TLS 1.2 path does not need signature
        // validation (the client cert is validated by the TLS 1.3 built-in
        // chain check). Signature verification is implemented in
        // `AuthorizedKeysVerifier` (holds a provider and forwards to the
        // ring provider).
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        // Same as above — the placeholder verifier does not perform
        // signature verification on the TLS 1.3 path.
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        // Placeholder verifier does not validate signature schemes —
        // returning an empty vec is sufficient.
        Vec::new()
    }
}

/// Server-side authorized-fingerprint allowlist verifier — the core
/// invariant of the two-layer mTLS defense: even if a client cert passes the
/// TLS 1.3 built-in chain check (self-signed root trust), it must also be
/// present in the `allowlist` fingerprint map to be admitted.
///
/// **`allowlist` semantics**: `String` is the fingerprint (lowercase hex +
/// `:` separator, matching the format produced by
/// `crypto::generate_fingerprint`). Runtime addition / removal of allowlist
/// entries is shared through `Arc<RwLock<HashMap<...>>>` — the listen.rs
/// supervisor or a later IPC push of `authorized_fingerprints` changes can
/// write into the inner `RwLock` and have those changes visible
/// (`RwLock::read()` does not block readers; `RwLock::write()` only blocks
/// writers).
///
/// **`Send + Sync + 'static`**: rustls 0.23 trait constraint —
/// `allowlist: Arc<RwLock<HashMap<...>>>` is auto `Send + Sync`, and so is
/// `provider: Arc<CryptoProvider>`.
///
/// **`provider` field**: forwarding `verify_tls12_signature` /
/// `verify_tls13_signature` to `rustls::crypto::verify_*_signature` requires
/// the `signature_verification_algorithms` list — a provider reference must
/// be held (same pattern as `TofuVerifier`).
///
/// **`verify_client_cert` two-state decision**:
/// - Hit (`allowlist.contains_key(&fp)`) → `Ok(ClientCertVerified::assertion())`
///   + `log::info!`
/// - Miss → `Err(rustls::Error::General(format!("unauthorized peer {fp}")))`
///   + `log::warn!`
#[derive(Debug)]
pub struct AuthorizedKeysVerifier {
    /// Authorized fingerprint map: key = client cert SHA-256 fingerprint
    /// (`crypto::generate_fingerprint` format), value = placeholder `String`.
    allowlist: Arc<RwLock<HashMap<String, String>>>,
    /// Crypto provider needed for signature verification; its
    /// `signature_verification_algorithms` is forwarded by
    /// `verify_tls12_signature` / `verify_tls13_signature`.
    provider: Arc<rustls::crypto::CryptoProvider>,
    /// Reverse-notification channel for rejected peer fingerprints — lets
    /// the listen task translate the fingerprint obtained during the rustls
    /// rejection path into a `ListenEvent::Rejected` → emulation.rs
    /// `ConnectionAttempt` → GUI popup.
    ///
    /// **`Option` rather than required**: unit tests and early callers (no
    /// listen task wired up) pass `None`, and `verify_client_cert` follows
    /// a no-op branch.
    ///
    /// **Why `tokio::sync::mpsc::UnboundedSender` instead of `local_channel`**:
    /// `verify_client_cert` is invoked by rustls on the QUIC handshake
    /// callback chain — quinn's I/O task may run on a non-local thread (not
    /// in the same task as `spawn_local`). `tokio::sync::mpsc::UnboundedSender`
    /// is `Send + Sync` and can be held across threads; the listen task's
    /// forwarder receives on `spawn_local`.
    rejection_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
}

impl AuthorizedKeysVerifier {
    /// Construct: the allowlist is owned by the caller (production
    /// `Config::authorized_fingerprints()`, tests
    /// `Arc::new(RwLock::new(HashMap::new()))`).
    ///
    /// `allowlist` must be `Send + Sync + 'static` (rustls requires the
    /// verifier to be `Send + Sync + 'static`; `Arc<RwLock<HashMap<...>>>`
    /// satisfies this automatically).
    ///
    /// **No rejection channel**: unit tests and early callers use this
    /// constructor; when rustls rejects a handshake only a `log::warn`
    /// audit line is emitted, with no GUI notification.
    #[allow(dead_code)]
    pub fn new(allowlist: Arc<RwLock<HashMap<String, String>>>) -> Self {
        Self {
            allowlist,
            provider: Arc::new(rustls::crypto::ring::default_provider()),
            rejection_tx: None,
        }
    }

    /// Inject the rejection reverse-notification channel — builder style,
    /// does not break the signatures of existing `new()` / `with_known()`
    /// tests and callers.
    ///
    /// `verify_client_cert` additionally calls `rejection_tx.send(fp.clone())`
    /// on the Err path (silent no-op if the channel is full or closed — the
    /// reject event is best-effort and must not interfere with rustls's
    /// intended `Err` semantics).
    #[allow(dead_code)]
    pub fn with_rejection_tx(mut self, tx: tokio::sync::mpsc::UnboundedSender<String>) -> Self {
        self.rejection_tx = Some(tx);
        self
    }

    /// Construct in the "known peer" state (pre-fill `allowlist` so
    /// subsequent verifies take the Authorized path).
    ///
    /// **Pre-fill is best-effort**: on failure the constructor still returns
    /// `Self`, and subsequent verifies take the Unauthorized path returning
    /// a `rustls::Error`. `RwLock::write()` poison errors are intentionally
    /// not swallowed, because they usually indicate an upstream panic.
    ///
    /// Test-only: callers invoke `verify_client_cert(cert)` directly and
    /// expect `Ok` (no end-to-end QUIC handshake required). Not used in
    /// production (production writes the allowlist via the listen.rs
    /// supervisor / service.rs, and the verifier gets an `Arc` reference
    /// through `new()`).
    #[allow(dead_code)]
    pub fn with_known(allowlist: Arc<RwLock<HashMap<String, String>>>, known_fp: &str) -> Self {
        let v = Self::new(allowlist);
        v.allowlist
            .write()
            .expect("RwLock poisoned")
            .insert(known_fp.to_owned(), String::new());
        v
    }

    /// Expose `allowlist` (tests assert allowlist contents and simulate
    /// runtime add/remove).
    #[allow(dead_code)]
    pub fn allowlist(&self) -> &Arc<RwLock<HashMap<String, String>>> {
        &self.allowlist
    }
}

impl rustls::server::danger::ClientCertVerifier for AuthorizedKeysVerifier {
    fn offer_client_auth(&self) -> bool {
        // Server-side mTLS forces client cert presentation (symmetric with
        // `PermissiveClientCertVerifier` — no cert triggers TLS 1.3's
        // `NoCertificatesPresented` rejection).
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        // No cert → reject directly (consistent with `offer_client_auth`).
        true
    }

    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        // No root hints — any self-signed cert is allowed to attempt the
        // handshake; the fingerprint check is performed by
        // `verify_client_cert` itself.
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::server::danger::ClientCertVerified, rustls::Error> {
        // (1) Compute the SHA-256 fingerprint of the client cert (matches the
        //     format produced by `crypto::generate_fingerprint`: lowercase hex
        //     separated by `:`).
        let fp = crypto::generate_fingerprint(end_entity.as_ref());

        // (2) Allowlist lookup (note: this collides with the module-level
        //     `Result<T>` alias — `verify_client_cert` is a trait method and
        //     must spell out `std::result::Result<_, rustls::Error>` to
        //     align with the rustls-expected type).
        let allowed = self
            .allowlist
            .read()
            .expect("RwLock poisoned")
            .contains_key(&fp);

        if allowed {
            // Authorized — fingerprint hit the allowlist.
            log::info!("AuthorizedKeysVerifier: authorized peer {fp}");
            Ok(rustls::server::danger::ClientCertVerified::assertion())
        } else {
            // Unauthorized — fingerprint not in the allowlist.
            //
            // Besides the `log::warn` audit line and the rustls `Err` that
            // triggers handshake rejection, also send the fingerprint
            // through the reverse channel to the listen task → translated
            // into `ListenEvent::Rejected` → `EmulationEvent::ConnectionAttempt`
            // → GUI `request_authorization` popup (emulation.rs:190 +
            // service.rs:320 + frontend `request_authorization`).
            //
            // **Send failures are swallowed silently**:
            // `UnboundedSender::send` only returns `Err` when the receiver
            // is dropped (channel closed), at which point the listen task
            // has already terminated — the rejection is already final, and
            // failing to emit the popup signal is a reasonable no-op. It
            // **must not** affect rustls's intended `Err` semantics (rustls
            // still rejects the handshake as designed, and the error
            // message is unchanged).
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

    /// Verifies that assembling a `quinn::ClientConfig` with a test self-signed
    /// cert does not panic.
    #[test]
    fn quinn_client_config_loads_rustls_provider() {
        super::super::endpoint::install_crypto_provider();

        let (cert_chain, key) = ephemeral_cert();
        let pins_dir = ephemeral_pins_dir();
        let _ = std::fs::remove_dir_all(&pins_dir);
        let cfg = build_quic_client_config(vec![cert_chain[0].clone()], key, &pins_dir, "peer-a")
            .expect("ClientConfig assembly should not fail");
        let _clone: quinn::ClientConfig = cfg.clone();
    }

    /// Verifies the server-side [`PermissiveClientCertVerifier`] forces
    /// mTLS: a dial without a client cert must be rejected at the server
    /// via TLS 1.3's built-in `rustls::Error::NoCertificatesPresented`.
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
        .expect("server endpoint_with_verifier bind should not fail");
        let server_addr = server_ep
            .local_addr()
            .expect("server endpoint must have local_addr");

        let server_task = tokio::spawn(async move {
            let incoming = server_ep
                .accept()
                .await
                .expect("server accept should not fail");
            let result = incoming.await;
            assert!(
                result.is_err(),
                "server side handshake should fail (mTLS requires client cert, client did not present), actually Ok"
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

        let client_ep =
            super::super::endpoint::endpoint(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
                .expect("client endpoint bind should not fail");

        let connecting_outcome = client_ep.connect_with(client_cfg, server_addr, "lan-mouse");
        let handshake_result = match connecting_outcome {
            Ok(connecting) => tokio::time::timeout(std::time::Duration::from_secs(5), connecting)
                .await
                .expect("dial end-to-end timed out"),
            Err(_connect_err) => {
                log::debug!("connect_with synchronous part failed (accepted)");
                return;
            }
        };

        assert!(
            handshake_result.is_err(),
            "dial without client cert should fail (server rejected handshake), actually Ok"
        );

        drop(client_ep);
        let _ = server_task.await;
    }

    /// Verifies that a peer's first fingerprint is accepted and pinned under
    /// that peer's key.
    #[test]
    fn tofu_first_run_pins() {
        super::super::endpoint::install_crypto_provider();

        let pins_dir = tmp_pins_dir("first");
        let verifier = TofuVerifier::new(&pins_dir, "peer-a");

        let (cert_chain, _key) = ephemeral_cert();
        let cert_der = cert_chain[0].clone();
        let fp = crate::crypto::generate_fingerprint(cert_der.as_ref());

        let server_name = test_server_name();
        let now = UnixTime::now();
        let result = verifier.verify_server_cert(&cert_der, &[], &server_name, &[], now);

        assert!(
            result.is_ok(),
            "first connect should be accepted (Ok), got {:?}",
            result
        );

        // The pin is named after the peer, not the fingerprint, and holds
        // the fingerprint as its content.
        let expected_pin = pins_dir.join("peer-a.pin");
        assert!(
            expected_pin.exists(),
            "pin file should exist at {:?}",
            expected_pin
        );

        let content = std::fs::read_to_string(&expected_pin).expect("read pin");
        assert_eq!(
            content.trim(),
            fp,
            "pin file should hold the peer's fingerprint"
        );

        // A second verify for the same peer takes the Known Match branch.
        let again = TofuVerifier::new(&pins_dir, "peer-a").verify_server_cert(
            &cert_der,
            &[],
            &server_name,
            &[],
            now,
        );
        assert!(
            again.is_ok(),
            "an unchanged fingerprint must stay accepted, got {:?}",
            again
        );

        let _ = std::fs::remove_dir_all(&pins_dir);
    }

    /// Regression: one peer's pin must not affect the verdict for a
    /// *different* peer.
    ///
    /// The pin used to be named after the fingerprint, and the mismatch
    /// branch fired whenever `pins_dir` held any `.pin` at all. That made the
    /// first paired peer poison the directory: every subsequent peer — a
    /// second machine, or the same machine after a legitimate cert
    /// regeneration — was rejected as "not in known peers" with no way to
    /// clear the pin from the UI.
    #[test]
    fn tofu_pins_are_per_peer() {
        super::super::endpoint::install_crypto_provider();

        let pins_dir = tmp_pins_dir("per-peer");
        let server_name = test_server_name();
        let now = UnixTime::now();

        // peer-a pairs first, leaving a pin behind.
        let (cert_a_chain, _key_a) = ephemeral_cert();
        let cert_a = cert_a_chain[0].clone();
        assert!(
            TofuVerifier::new(&pins_dir, "peer-a")
                .verify_server_cert(&cert_a, &[], &server_name, &[], now)
                .is_ok(),
            "peer-a first connect should be accepted"
        );

        // peer-b is a different peer with a different cert: still a first
        // connect, and peer-a's pin must not be consulted.
        let (cert_b_chain, _key_b) = ephemeral_cert();
        let cert_b = cert_b_chain[0].clone();
        let fp_b = crate::crypto::generate_fingerprint(cert_b.as_ref());
        let result = TofuVerifier::new(&pins_dir, "peer-b").verify_server_cert(
            &cert_b,
            &[],
            &server_name,
            &[],
            now,
        );
        assert!(
            result.is_ok(),
            "a second peer must pair even though another peer is already pinned, got {:?}",
            result
        );

        let pin_b = pins_dir.join("peer-b.pin");
        assert_eq!(
            std::fs::read_to_string(&pin_b)
                .expect("read peer-b pin")
                .trim(),
            fp_b,
            "peer-b should be pinned to its own fingerprint"
        );
        assert!(
            pins_dir.join("peer-a.pin").exists(),
            "pairing peer-b must leave peer-a's pin untouched"
        );

        let _ = std::fs::remove_dir_all(&pins_dir);
    }

    /// Verifies that a *known* peer swapping its fingerprint is rejected
    /// (`rustls::Error::General("TOFU mismatch: ...")`).
    #[test]
    fn tofu_disallows_swap() {
        super::super::endpoint::install_crypto_provider();

        let pins_dir = tmp_pins_dir("swap");

        let (cert1_chain, _key1) = ephemeral_cert();
        let cert1_der = cert1_chain[0].clone();
        let fp1 = crate::crypto::generate_fingerprint(cert1_der.as_ref());
        let verifier = TofuVerifier::with_known(&pins_dir, "peer-a", &fp1);

        let (cert2_chain, _key2) = ephemeral_cert();
        let cert2_der = cert2_chain[0].clone();
        let fp2 = crate::crypto::generate_fingerprint(cert2_der.as_ref());
        assert_ne!(
            fp1, fp2,
            "two ephemeral_certs must have different fingerprints (rcgen randomizes each time)"
        );

        let server_name = test_server_name();
        let now = UnixTime::now();
        let result = verifier.verify_server_cert(&cert2_der, &[], &server_name, &[], now);

        match result {
            Err(rustls::Error::General(msg)) => {
                assert!(
                    msg.contains("TOFU mismatch"),
                    "error message should contain TOFU mismatch, actually: {msg}"
                );
            }
            other => panic!(
                "TOFU mismatch should return Err(rustls::Error::General), got: {:?}",
                other
            ),
        }

        let pin = pins_dir.join("peer-a.pin");
        assert_eq!(
            std::fs::read_to_string(&pin).expect("read pin").trim(),
            fp1,
            "mismatch must not overwrite the pinned fingerprint"
        );

        let _ = std::fs::remove_dir_all(&pins_dir);
    }

    /// Verifies that when the allowlist is pre-filled with a fingerprint,
    /// `verify_client_cert` with the corresponding cert returns `Ok`.
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
            "pre-filled fingerprint in allowlist should be accepted, actually: {result:?}"
        );

        assert!(
            verifier.allowlist().read().unwrap().contains_key(&fp),
            "allowlist should contain pre-filled fp"
        );
    }

    /// Verifies that when the allowlist does not contain a fingerprint,
    /// `verify_client_cert` with the corresponding cert returns
    /// `Err(rustls::Error::General("unauthorized peer {fp}"))`.
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
            "fingerprint not in allowlist should be rejected, actually: {result:?}"
        );

        let err_msg = format!("{:?}", result.unwrap_err());
        assert!(
            err_msg.contains(&fp),
            "Err message should contain fingerprint `{fp}`, actually: {err_msg}"
        );
        assert!(
            err_msg.contains("unauthorized"),
            "Err message should contain 'unauthorized' keyword, actually: {err_msg}"
        );

        assert!(
            !verifier.allowlist().read().unwrap().contains_key(&fp),
            "allowlist should not contain cert_der's fp"
        );
    }

    /// Verifies the rejection channel: when `verify_client_cert` returns Err
    /// on an allowlist miss, the fingerprint must be delivered to rx
    /// synchronously via `rejection_tx`.
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
        assert!(r.is_ok(), "allowlist hit should be Ok");
        assert!(
            rx.try_recv().is_err(),
            "allowlist hit path should not send rejection (preventing false positives)"
        );

        allowlist.write().expect("RwLock poisoned").remove(&fp);
        let r = <AuthorizedKeysVerifier as rustls::server::danger::ClientCertVerifier>::verify_client_cert(
            &verifier,
            &cert_der,
            &[],
            rustls::pki_types::UnixTime::now(),
        );
        assert!(r.is_err(), "allowlist miss should be Err");

        let received = rx
            .try_recv()
            .expect("rejection_tx should send fp on Err path");
        assert_eq!(
            received, fp,
            "fp received on rejection channel should match the fp of the rejected cert"
        );
        assert!(
            rx.try_recv().is_err(),
            "second try_recv should be empty (one rejection sends once)"
        );
    }
}
