//! UDP bind → `quinn::Endpoint`, dial / accept / install_crypto_provider.
//!
//! This module provides both entry points of the QUIC link:
//!
//! - [`endpoint`] placeholder client-mode endpoint (superseded in most cases by
//!   [`endpoint_with_cert`] / [`endpoint_with_verifier`])
//! - [`endpoint_with_cert`] / [`endpoint_with_verifier`] server-mode
//!   endpoint assembly
//! - [`dial`] / [`dial_any`] active dial (with happy-eyeballs)
//! - [`accept`] accept incoming handshakes
//! - [`install_crypto_provider`] one-shot install of the rustls `ring` provider
//! - [`endpoint_inner`] private helper shared by both server-mode paths
//! - [`HEAD_START`] 200ms primary head-start for happy-eyeballs
//!
//! Relationship with [`super::tls`]: `build_quic_client_config` assembles the
//! client TLS configuration consumed by `dial` / `dial_any`; `default_transport_config`
//! provides keepalive + idle settings.

use std::net::{SocketAddr, UdpSocket};
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use quinn::{EndpointConfig, ServerConfig};

use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use tokio::task::JoinSet;

use crate::crypto;

use super::tls;
use super::{ALPN_LAN_MOUSE, Connection, Endpoint, Error, Result};

/// Placeholder implementation: bind `addr` into a `quinn::Endpoint`.
///
/// This function is the minimal compile-passing shape used to validate the UDP
/// bind + Endpoint construction + Drop path. It does **not** attach a
/// `ServerConfig`, so quinn marks the endpoint as client-mode (it never accepts
/// incoming handshakes; it can only serve as the local anchor for subsequent
/// dials).
///
/// **Why not pass `ServerConfig::default()` directly** — quinn 0.11's
/// `quinn_proto::ServerConfig` has no `Default` impl (the `crypto` field must
/// be filled by the caller with `Arc<dyn crypto::ServerConfig>`), and
/// `ServerConfig::with_crypto` requires an existing `Arc<QuicServerConfig>`,
/// which in turn requires a fully assembled `rustls::ServerConfig` (via
/// `crypto::rustls_server_config(chain, key)`). This function does not deal
/// with certs, so it passes `None`.
///
/// **EndpointConfig**: `default()` already enables `HashedConnectionIdGenerator`
/// (supporting multiple CIDs + connection migration); `migration = true` is
/// the quinn default and needs no explicit override (the quinn 0.11 builder is
/// `cid_generator(F)`, with no public field).
///
/// **Runtime**: obtains the current tokio runtime handle via
/// `quinn::default_runtime()`. When this function is called from a
/// `#[tokio::test]`, `Handle::try_current()` returns `Some(TokioRuntime)`;
/// the production path follows the same route.
pub fn endpoint(addr: SocketAddr) -> Result<Endpoint> {
    let endpoint_cfg = EndpointConfig::default();

    let socket = UdpSocket::bind(addr).map_err(|source| Error::Bind { addr, source })?;

    let runtime = quinn::default_runtime()
        .ok_or_else(|| Error::EndpointSetup("no tokio runtime available".into()))?;

    // Pass `None` (client-mode endpoint), working around quinn 0.11's requirement
    // that `ServerConfig::crypto` be set. `endpoint_with_cert` uses the real
    // `Some(server_cfg_with_cert)` path and injects `default_transport_config()`
    // via `server_cfg.transport = ...`.
    let endpoint = Endpoint::new(endpoint_cfg, None::<ServerConfig>, socket, runtime)
        .map_err(|e| Error::EndpointSetup(format!("Endpoint::new failed: {e}")))?;

    Ok(endpoint)
}

/// Assemble a server-mode `quinn::Endpoint`: UDP bind + rustls `ServerConfig`
/// (with ALPN `lan-mouse`) + quinn transport_config + `Endpoint::new`.
///
/// The endpoint returned by `endpoint_with_cert(...)` is what allows [`accept`]
/// to actually receive incoming handshakes (a client-mode endpoint never sees
/// incoming connections).
///
/// **Production path caller**:
/// 1. `crypto::load_or_create_server_cert()` → `(cert_chain, key)` (persisted
///    to `$XDG_DATA_HOME/lan-mouse/{cert,key}.pem`)
/// 2. `endpoint_with_cert(addr, cert_chain, key)`
/// 3. `accept(ep)` waits for incoming connections
///
/// **ALPN symmetry**: this function sets `rustls::ServerConfig.alpn_protocols`
/// to `vec![ALPN_LAN_MOUSE.to_vec()]` — **before** wrapping into
/// `QuicServerConfig` (the `alpn_protocols` field belongs to
/// `rustls::ServerConfig`, not to quinn's `ServerConfig`). It is fully symmetric
/// with the client [`super::tls::build_quic_client_config`]; otherwise an ALPN
/// mismatch will reject the connection outright.
///
/// **`transport_config`**: chained onto the config via
/// `server_cfg.transport_config(...)` with [`super::tls::default_transport_config`]
/// — 5s keepalive / 30s idle.
///
/// **Error normalization**: reuses existing variants — no new ones added:
/// - `crypto::rustls_server_config` failure → `Error::Crypto(#[from])`
/// - `QuicServerConfig::try_from` failure → `Error::ClientConfig(String)`
/// - bind / runtime / `Endpoint::new` failures → reuse the [`endpoint`] error variants
///
/// **`install_crypto_provider` is not called inside this function**: it is
/// symmetric with [`super::tls::build_quic_client_config`] — the caller
/// (`service.rs` / tests) is responsible for invoking it. The production path
/// already installs it at startup in `main.rs`; tests call
/// `install_crypto_provider()` as their first line.
///
/// [`endpoint`] is **not** changed: the client-mode endpoint is still consumed
/// by the [`dial`] call stack (`Endpoint::connect_with` does not require the
/// endpoint to have a `ServerConfig` attached).
pub fn endpoint_with_cert(
    addr: SocketAddr,
    cert_chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<Endpoint> {
    let rustls_server_arc = crypto::rustls_server_config(cert_chain, key)?;
    endpoint_inner(addr, rustls_server_arc)
}

/// Server-mode `Endpoint` with mandatory mTLS client cert verification.
///
/// Mirrors the shape of [`endpoint_with_cert`]; the only difference is that
/// when assembling the rustls `ServerConfig`, it calls
/// `crypto::rustls_server_config_with_verifier(...)` to delegate client cert
/// verification to a caller-provided verifier:
/// - fingerprint in allowlist → handshake succeeds
/// - not in allowlist / missing client cert → `rustls::Error::General(...)`,
///   wrapped by quinn as `ConnectionError::TransportError` / `LocallyClosed`
///   → [`Error::Handshake`]
///
/// **mTLS pairing**: when the server `client_auth_mandatory() -> true` (the
/// default in this repo), the server side's `CertificateRequest` requires the
/// client to present a cert. The client side
/// [`super::tls::build_quic_client_config`] must simultaneously attach
/// `(cert, key)` via `with_client_auth_cert` so that both ends of the TLS
/// handshake have a complete mTLS.
///
/// **Default verifier**: [`super::tls::PermissiveClientCertVerifier`] — it
/// accepts any client cert, provided it exists and its signature passes the
/// TLS 1.3 built-in validation. This is the minimal working shape; a stricter
/// fingerprint-allowlist verifier can be substituted in the same position.
///
/// **Error normalization**: reuses existing [`Error`] variants — no new ones:
/// - `crypto::rustls_server_config_with_verifier` failure → `Error::Crypto`
/// - `endpoint_inner` internal errors (`Arc::try_unwrap` / `QuicServerConfig::try_from` /
///   bind / runtime / `Endpoint::new`) → reuse the [`endpoint_with_cert`] error variants
///
/// **`install_crypto_provider` is not called inside this function**: symmetric
/// with [`endpoint_with_cert`].
pub fn endpoint_with_verifier(
    addr: SocketAddr,
    cert_chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    verifier: Arc<dyn rustls::server::danger::ClientCertVerifier>,
) -> Result<Endpoint> {
    let rustls_server_arc = crypto::rustls_server_config_with_verifier(cert_chain, key, verifier)?;
    endpoint_inner(addr, rustls_server_arc)
}

/// Private helper shared by `endpoint_with_cert` / `endpoint_with_verifier`:
/// assemble a `quinn::Endpoint` from an `Arc<rustls::ServerConfig>`.
///
/// Extracted so that both paths share the fixed assembly flow of
/// `Arc::try_unwrap` + ALPN + `QuicServerConfig` + transport_config + bind +
/// `Endpoint::new`, avoiding duplication when adding new verifier entry points.
///
/// `Arc::try_unwrap` is guaranteed to succeed: the freshly returned
/// `Arc<ServerConfig>` has a strong count of 1 (no other copy is held after
/// `crypto::rustls_server_config[_with_verifier]` returns). Even if the verifier
/// internally holds an `Arc` (e.g. `Arc<RwLock<...>>`), that is its own
/// internal state and unrelated to the server_cfg.
fn endpoint_inner(
    addr: SocketAddr,
    rustls_server_arc: Arc<rustls::ServerConfig>,
) -> Result<Endpoint> {
    // `alpn_protocols` is a field on `rustls::ServerConfig` (not on quinn's
    // `ServerConfig`), so it must be set before wrapping into `QuicServerConfig`.
    let mut rustls_server = Arc::try_unwrap(rustls_server_arc)
        .map_err(|_| Error::ClientConfig("rustls ServerConfig Arc strong count > 1".into()))?;
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

/// Install rustls' `ring` crypto provider — **must** be called before any
/// `rustls::ClientConfig::builder` / `rustls::ServerConfig::builder` call,
/// otherwise the runtime will panic.
///
/// Guarded by [`OnceLock`]: under multi-threaded `cargo test` runs, the
/// `lan-mouse-cli` subprocess, or CLI + daemon concurrent installs, a second
/// `install_default()` call returns `Err(SomeInstalled)` and would cause
/// panic / noisy logs from a bare call. `OnceLock` guarantees the install
/// runs at most once per process, making the function idempotent and reentrant.
pub fn install_crypto_provider() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        // Intentionally ignore Err: a repeated install returning
        // `Err(SomeInstalled)` is not an error — the already-installed provider
        // is the same one (ring) we are trying to install, so the race is harmless.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Actively dial the peer endpoint and return a [`Connection`] after the QUIC
/// TLS 1.3 handshake completes.
///
/// **mTLS**: this function reuses [`super::tls::build_quic_client_config`],
/// which has already attached the mTLS presentation via
/// `with_client_auth_cert(cert_chain, key)`. The `cert` / `key` parameters
/// serve two purposes:
/// 1. As the trust anchor input for the **remote** server
/// 2. As the **local** client's mTLS presentation
///   (`with_client_auth_cert(cert_chain, key)`)
///
/// The server cert validation runs through `TofuVerifier::new(pins_dir)`,
/// where `pins_dir` is provided by the caller. `TofuVerifier` internally
/// tri-states between Known Match / Known Mismatch / First Connect.
///
/// **Parameter order**: `(ep, addr, cert, key, pins_dir)` — `pins_dir` is
/// appended last; `cert` is a **single** `CertificateDer` that this function
/// wraps into a chain with `vec![cert]` before feeding
/// [`super::tls::build_quic_client_config`].
///
/// **ALPN**: the TLS 1.3 handshake advertises `b"lan-mouse"` (set via
/// `rustls_client.alpn_protocols` inside `build_quic_client_config`). The
/// server side must symmetrically set
/// `rustls_server.alpn_protocols = vec![ALPN_LAN_MOUSE.to_vec()]`, otherwise
/// an ALPN mismatch will reject the connection outright.
///
/// **`server_name`**: the third argument of `ep.connect_with(cfg, addr, "lan-mouse")`
/// is used for SNI (Server Name Indication) and as the `server_name` parameter
/// of rustls 0.23's `ServerCertVerifier::verify_server_cert(..., server_name, ...)`.
/// The current `TofuVerifier` does not read `server_name` (it inspects
/// fingerprint only). The hardcoded `"lan-mouse"` matches the ALPN protocol
/// name.
///
/// **Error normalization**:
/// - `Endpoint::connect_with` synchronous failure (endpoint closed / invalid
///   address / no client config) → [`Error::Connect`]
///   (`#[from] quinn::ConnectError`)
/// - Post-`.await` handshake failure (cert / ALPN / mTLS rejection /
///   `TofuVerifier` mismatch / interruption) → [`Error::Handshake`]
///   (`#[from] quinn::ConnectionError`); `TofuVerifier` mismatches surface
///   here as `ConnectionError::TransportError(rustls::Error::General("TOFU
///   mismatch: ..."))`
///
/// **Does not** proactively call `install_crypto_provider`: symmetric with
/// `build_quic_client_config`; `main.rs` / tests are responsible for guarding
/// it as their first line.
#[allow(dead_code)]
pub async fn dial(
    ep: &Endpoint,
    addr: SocketAddr,
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
    pins_dir: &Path,
) -> Result<Connection> {
    // Idempotent guard: symmetric with build_quic_client_config — even if the
    // caller already invoked it once during main startup, repeatedly entering
    // this function from the test path remains safe.
    install_crypto_provider();

    // `build_quic_client_config` takes `pins_dir` (TofuVerifier replaces
    // WebPkiServerVerifier; construction is fully owned by `TofuVerifier::new(pins_dir)`).
    let cfg = tls::build_quic_client_config(vec![cert], key, pins_dir)?;
    let conn = ep.connect_with(cfg, addr, "lan-mouse")?.await?;
    Ok(conn)
}

/// Happy-eyeballs dial — 200ms primary head-start + concurrent dialing of
/// remaining candidates; the first QUIC TLS 1.3 handshake to succeed wins,
/// and the raw [`Connection`] is returned.
///
/// **happy-eyeballs algorithm** (RFC 8305 simplified):
/// 1. Construct an `Arc<ClientConfig>` once (`build_quic_client_config` +
///    cert/key/pins_dir injection; `ClientConfig: Clone` is reused for every
///    candidate — avoiding reparsing `PrivateKeyDer::clone_key()` per
///    candidate).
/// 2. **`primary` head-starts alone** — spawn a task to dial the primary;
///    `tokio::select!` races the 200ms timer against `joinset.join_next()`:
///    - win within 200ms (primary handshake succeeds) → immediately
///      `abort_all()` + return
///    - primary loses before the timer fires → log warn + wait for timer
/// 3. **head-start ends → dial remaining candidates together** — spawn tasks
///    for every address in `all` except the primary.
/// 4. **First successful task** → `abort_all()` + return Connection.
/// 5. All dials fail → return the **last** error.
///
/// Returns a [`Connection`] rather than a wrapped session — `dial_any` only
/// handles "connected"; routing configuration and higher-level handshakes
/// are the caller's responsibility.
///
/// **Why 200ms**: LAN round trips are typically fast enough for the primary
/// handshake to complete within 200ms; on timeout the concurrent dialing
/// fallback handles LAN multi-homed latency drift.
///
/// quinn's `Connection` implements `Drop` to close automatically (a
/// simplification versus DTLS), so losers aborted here are closed via RAII
/// without needing an explicit `conn.close(...)`.
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

    // (1) Construct ClientConfig once, reuse for every dial.
    let cfg = tls::build_quic_client_config(vec![cert], key, pins_dir)?;

    // (2) JoinSet collects (SocketAddr, Result<Connection, Error>).
    let mut joinset: JoinSet<(SocketAddr, Result<Connection>)> = JoinSet::new();
    let mut spawned: std::collections::HashSet<SocketAddr> = std::collections::HashSet::new();

    // (3) Spawn the primary head-start task.
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

    // (4) Primary head-start race: win within 200ms → return immediately;
    //     lose → log warn + wait for timer.
    {
        let head_start = tokio::time::sleep(HEAD_START);
        tokio::pin!(head_start);
        loop {
            tokio::select! {
                _ = &mut head_start => break,
                joined = joinset.join_next() => {
                    let Some(inner) = joined else { break; };
                    let Ok((_addr, res)) = inner else {
                        log::warn!("dial_any: JoinSet task panic (during head-start)");
                        continue;
                    };
                    match res {
                        Ok(conn) => {
                            joinset.abort_all();
                            return Ok(conn);
                        }
                        Err(e) => {
                            log::warn!("dial_any: dial {_addr} failed (during head-start): {e}");
                        }
                    }
                }
            }
        }
    }

    // (5) Primary did not win during head-start → dial remaining candidates together.
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
                log::warn!("dial_any: dial {_addr} failed: {e}");
                last_err = Some(e);
            }
        }
    }

    Err(last_err.expect("JoinSet should join at least one task"))
}

/// The 200ms head-start reserved for the primary in happy-eyeballs
/// (RFC 8305 simplified; mirrors the semantics of connect.rs's existing
/// `PREFERRED_ADDR_HEAD_START`).
const HEAD_START: Duration = Duration::from_millis(200);

/// Accept an incoming QUIC handshake connection and return the raw
/// [`Connection`] after the TLS 1.3 handshake completes.
///
/// **Two-step handshake**:
/// 1. `ep.accept().await` returns `Option<Incoming>` — `None` indicates the
///    endpoint has been closed (typical scenarios: the listener is dropped /
///    the runtime exits). This is wrapped as [`Error::EndpointSetup`] so the
///    caller can distinguish "endpoint exited" from "handshake failed".
/// 2. `incoming.await` returns `Result<Connection, ConnectionError>` — cert
///    validation / ALPN / interruption / TLS errors are all normalized to
///    [`Error::Handshake`] (already has the `#[from]` derive, so `?` converts
///    directly).
///
/// Note: a client-mode endpoint (built via [`endpoint`] with `None::<ServerConfig>`)
/// will never receive an incoming connection — only server-mode endpoints
/// built via `endpoint_with_cert()` / `endpoint_with_verifier()` can. The test
/// path uses the in-process server helper `endpoint_with_test_cert()`
/// (already equipped with `Some(server_cfg)`); the `accept()` body itself
/// (`ep.accept().await?.await?`) is unchanged across both paths.
///
/// **Error normalization**:
/// - endpoint closed → [`Error::EndpointSetup`] (reuses an existing variant;
///   no new variants added)
/// - handshake failure → [`Error::Handshake`] (`#[from] quinn::ConnectionError`)
///
/// **Do not drive a long-lived accept loop with this function.** It collapses
/// "the endpoint is gone" and "this one handshake failed" into a single `Err`,
/// so a loop that `break`s on `Err` tears the listener down on the first
/// unauthorized / mismatched / aborted dial. `listen.rs::spawn_quic_accept_task`
/// therefore calls `ep.accept()` directly and only treats `None` as fatal.
/// This helper is for one-shot accepts (tests, single-connection flows).
///
/// **Does not** proactively call `install_crypto_provider`: symmetric with
/// [`dial`]; the caller is responsible for guarding it during main startup.
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
        endpoint_with_test_cert, ephemeral_cert, ephemeral_pins_dir,
    };

    use super::*;

    /// `endpoint_with_cert` binds a temporary port and `Drop` does not panic.
    #[tokio::test]
    async fn endpoint_with_cert_binds_ipv4_localhost() {
        install_crypto_provider();
        let (cert_chain, key) = ephemeral_cert();
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into();
        let ep =
            endpoint_with_cert(addr, cert_chain, key).expect("endpoint_with_cert bind should not fail");
        let local = ep.local_addr().expect("endpoint must have local_addr");
        assert_ne!(local.port(), 0, "ephly port should be non-zero");
        drop(ep);
    }

    /// `endpoint_with_cert` accepts an incoming connection and a client `dial`
    /// completes the TLS 1.3 handshake.
    #[tokio::test]
    async fn endpoint_with_cert_accepts_local_incoming() {
        install_crypto_provider();
        let (cert_chain, key) = ephemeral_cert();
        let server_ep = endpoint_with_test_cert(
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into(),
            cert_chain,
            key,
        )
        .expect("server endpoint bind should not fail");
        let server_addr = server_ep.local_addr().expect("server ep must have local_addr");

        let server_task = tokio::spawn(async move {
            let incoming = server_ep.accept().await.expect("server accept should not fail");
            let conn = incoming.await.expect("server handshake should not fail");
            drop(conn);
        });

        let (client_cert, client_key) = ephemeral_cert();
        let client_ep = endpoint(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
            .expect("client endpoint bind should not fail");
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
        .expect("end-to-end TLS 1.3 handshake timed out")
        .expect("dial should not fail");

        assert!(
            conn.peer_identity().is_some(),
            "peer_identity should not be empty (TLS 1.3 handshake complete)"
        );

        drop(conn);
        server_task.await.expect("server task should not panic");
        client_ep.wait_idle().await;
    }

    /// `endpoint` binds a temporary port and `Drop` does not panic.
    #[tokio::test]
    async fn endpoint_binds_ipv4_localhost() {
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into();
        let ep = endpoint(addr).expect("endpoint bind should not fail");
        let local = ep.local_addr().expect("endpoint must have local_addr");
        assert_ne!(local.port(), 0, "ephly port should be non-zero");
        drop(ep);
    }

    /// In-process server endpoint + client endpoint dial, asserting the
    /// TLS 1.3 handshake completes (`peer_identity()` is non-empty).
    #[tokio::test]
    async fn dial_completes_handshake_against_local_listener() {
        install_crypto_provider();

        let (server_cert_chain, server_key) = ephemeral_cert();
        let server_ep = endpoint_with_test_cert(
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into(),
            server_cert_chain,
            server_key,
        )
        .expect("server endpoint bind should not fail");
        let server_addr = server_ep
            .local_addr()
            .expect("server endpoint must have local_addr");

        let server_task = tokio::spawn(async move {
            let incoming = server_ep.accept().await.expect("server accept should not fail");
            let conn = incoming.await.expect("server handshake should not fail");
            drop(conn);
        });

        let (client_cert, client_key) = ephemeral_cert();
        let client_ep = endpoint(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
            .expect("client endpoint bind should not fail");
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
        .expect("end-to-end TLS 1.3 handshake timed out")
        .expect("dial should not fail");

        assert!(
            conn.peer_identity().is_some(),
            "peer_identity should not be empty (TLS 1.3 handshake complete)"
        );

        drop(conn);
        server_task.await.expect("server task should not panic");
        client_ep.wait_idle().await;
    }

    /// `dial_any` selects the primary when primary is the server_addr.
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
                let _conn =
                    tokio::time::timeout(std::time::Duration::from_secs(5), accept(&server_ep))
                        .await
                        .expect("server accept timeout")
                        .expect("server accept");
            });

            let pins_dir = ephemeral_pins_dir();
            let _ = std::fs::remove_dir_all(&pins_dir);
            let (client_cert, client_key) = ephemeral_cert();
            let client_ep = endpoint(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
                .expect("client endpoint bind");

            let unreachable =
                SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 65535);
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
            .expect("dial_any total timeout")
            .expect("dial_any should succeed (primary wins)");

            assert_eq!(
                conn.remote_address(),
                server_addr,
                "dial_any should select primary (i.e., server_addr), not fallback unreachable address"
            );

            conn.close(quinn::VarInt::from(0u32), b"test done");
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), server_task).await;
            drop(client_ep.wait_idle());
            let _ = std::fs::remove_dir_all(&pins_dir);
        };
        tokio::task::LocalSet::new().run_until(fut).await;
    }

    /// `dial_any` returns Err when every candidate is unreachable.
    #[tokio::test(flavor = "multi_thread")]
    async fn dial_any_all_unreachable_returns_err() {
        let fut = async {
            install_crypto_provider();

            let client_ep = endpoint(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
                .expect("client endpoint bind");

            let pins_dir = ephemeral_pins_dir();
            let _ = std::fs::remove_dir_all(&pins_dir);
            let (client_cert, client_key) = ephemeral_cert();

            let primary = SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 65535);
            let secondary =
                SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2)), 65535);
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
            .expect("dial_any total timeout (should return Err within <35s)");

            assert!(
                result.is_err(),
                "dial_any should return Err when all candidates are unreachable, actually returned: {result:?}"
            );

            drop(client_ep);
            let _ = std::fs::remove_dir_all(&pins_dir);
        };
        tokio::task::LocalSet::new().run_until(fut).await;
    }
}
