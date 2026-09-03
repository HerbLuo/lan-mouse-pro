//! QUIC transport abstraction layer.
//!
//! This module wraps a UDP socket into a [`quinn::Endpoint`] and defines a
//! QUIC session [`PeerSession`] with a remote peer.
//!
//! This file (`mod.rs`) provides only the **public surface + cross-module
//! error type**:
//!
//! - [`Error`] / [`Result`] — transport-layer error types shared by all
//!   submodules.
//! - [`ALPN_LAN_MOUSE`] — ALPN protocol name (used by both `endpoint.rs`
//!   and `tls.rs`).
//! - `pub use` re-exports — flatten the public API of the five submodules
//!   (`endpoint` / `tls` / `protocol` / `streams` / `session`) under the
//!   `quic_transport::xxx` path so external callers (`connect.rs` /
//!   `listen.rs` / `service.rs` / `lib.rs` / `tests/quic_smoke.rs`) need
//!   no changes.
//!
//! See the docstrings in each submodule for additional documentation.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;

use lan_mouse_proto::{MAX_EVENT_SIZE, ProtoEvent};

pub use quinn::{Connection, Endpoint};

pub(crate) const ALPN_LAN_MOUSE: &[u8] = b"lan-mouse";

pub mod endpoint;
pub mod protocol;
pub mod session;
pub mod streams;
pub mod tls;

// Re-export the public API of each submodule so external callers can write
// `quic_transport::xxx` without accessing the submodules directly.
// The external API is identical to before the split.
pub use crate::quic_transport::{
    endpoint::{
        accept, dial, dial_any, endpoint, endpoint_with_cert, endpoint_with_verifier,
        install_crypto_provider,
    },
    protocol::{
        Channel, HELLO_TIMEOUT, client_hello, read_any_frame, read_frame, route_input, server_hello,
    },
    session::{PeerRole, PeerSession, should_retry_after_close},
    streams::StreamBunch,
    tls::{
        AuthorizedKeysVerifier, PermissiveClientCertVerifier, TofuVerifier,
        build_quic_client_config,
    },
};

/// Transport-layer error type.
///
/// Location: `mod.rs` (cross-module error type). Every submodule uses
/// `use super::Error`. The `quinn::{ConnectError, ConnectionError,
/// SendDatagramError}` and `crate::crypto::Error` types bound via
/// `#[from]` are transparently converted through `super::Error`.
#[derive(Debug, Error)]
pub enum Error {
    #[error("not implemented (placeholder)")]
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
    /// `Endpoint::connect_with(...)` failed synchronously — the endpoint is
    /// closed, the remote address is invalid, or the current endpoint has
    /// no client config.
    #[error("connect_with failed: {0}")]
    Connect(#[from] quinn::ConnectError),
    /// QUIC TLS 1.3 handshake failed — certificate validation did not pass,
    /// the ALPN did not match, the connection was aborted, etc.
    /// `ConnectionError` contains sub-variants such as `LocallyClosed`,
    /// `RemoteClosed`, `TransportError`, and `ApplicationClosed`. After the
    /// `TofuVerifier` replaced the placeholder verifier, errors such as
    /// `rustls::Error::General("untrusted peer ...")` surface here as
    /// `ConnectionError::TransportError(...)`.
    #[error("handshake failed: {0}")]
    Handshake(#[from] quinn::ConnectionError),
    /// Application-layer Hello handshake failed: magic mismatch, protocol
    /// error, decode failure, or a non-Hello frame was received.
    /// The message includes a specific reason ("wrong magic: ..." /
    /// "non-Hello message: ..." / "decode frame: ...").
    #[error("hello handshake failed: {0}")]
    HelloFailed(String),
    /// Hello handshake timed out (the remote peer did not complete the
    /// magic exchange on stream A within [`HELLO_TIMEOUT`]).
    #[error("hello handshake timed out after {0:?}")]
    HelloTimeout(Duration),
    /// QUIC datagram send failed.
    ///
    /// Wraps [`quinn::SendDatagramError`] — contains `UnsupportedByPeer`,
    /// `Disabled`, `TooLarge`, and `ConnectionLost`. `ConnectionLost`
    /// indicates the connection is already dead and falling back to a
    /// stream cannot save it; callers must decide whether to report this
    /// as `Error::Handshake` (TODO: refine when integrating into
    /// `connect.rs` in the next stage). The other three variants
    /// indicate that this path is unavailable; `send_datagram_or_stream_b`
    /// falls back to the stream B path internally and does not surface
    /// them here.
    #[error("datagram send failed: {0}")]
    Datagram(#[from] quinn::SendDatagramError),
    /// IO error when falling back to a stream uni. **Temporary.**
    ///
    /// The fallback path opens a uni stream inline, writes the frame, and
    /// finishes the stream — it does not reuse the stream B cache +
    /// length-prefix framing. This variant carries only fallback IO
    /// errors (including `ConnectionError` from `open_uni`,
    /// `WriteError` from `write_all`, and `ClosedStream` from `finish`).
    /// It will be replaced by [`Error::StreamB`] once the stream B cache
    /// lands (aligned in shape with bak
    /// `mousehop/src/quic_transport.rs:564
    /// Error::StreamB(format!("open_bi: {e}"))`).
    #[error("datagram fallback stream io error: {0}")]
    DatagramFallback(String),
    /// Stream B (input stream) setup or write failed.
    ///
    /// The message prefix distinguishes the two stages (`"open_bi: ..."` /
    /// `"write frame length: ..."` / `"write: ..."`) — the underlying
    /// types differ (`ConnectionError` vs `WriteError`), so they are
    /// collapsed into a `String` to avoid adding two more variants for a
    /// single fallback path; aligned in shape with bak
    /// `mousehop/src/quic_transport.rs:1035-1040 Error::StreamB`.
    #[error("stream B: {0}")]
    StreamB(String),
    /// Frame length field exceeds [`MAX_EVENT_SIZE`] (used by [`read_frame`]).
    ///
    /// An attacker who controls the length-prefix field can induce
    /// `read_exact(&mut buf[..len])` to read a huge number of bytes (a
    /// DoS vector). This variant lets `read_frame` return an error
    /// immediately when an over-limit length is read, avoiding OOM or
    /// slow reads. The message includes the offending `len` value to
    /// aid diagnosis.
    ///
    /// Aligned in shape with bak
    /// `mousehop/src/quic_transport.rs:1063-1071 Error::FrameTooLarge`.
    #[error("frame too large: {0} bytes (max {MAX_EVENT_SIZE})")]
    FrameTooLarge(usize),
    /// Frame body was truncated inside [`read_frame`].
    ///
    /// Returned when `read_exact` reads fewer than `len` bytes because
    /// the stream was closed early (quinn `UnexpectedEof` /
    /// `ClosedStream`). This is semantically distinct from a decode
    /// failure (`Error::HelloFailed`) and an over-limit length field
    /// ([`Error::FrameTooLarge`]): this variant indicates that the
    /// remote peer closed the stream mid-frame (possibly malicious or
    /// possibly a peer crash) and is fatal — `read_loop` should close
    /// the connection and exit on this error, not skip the frame and
    /// continue reading (consistent with the bak
    /// `frame_truncated_rejected` test).
    #[error("frame body truncated")]
    Truncated,
    /// crypto.rs error pass-through — primarily surfaced by failures in
    /// `crypto::rustls_server_config` or
    /// `rustls_server_config_with_verifier` (certificate parsing, chain
    /// construction, or `rcgen` self-signed cert failures, etc.).
    #[error("crypto: {0}")]
    Crypto(#[from] crate::crypto::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

// Suppress `Arc` / `ProtoEvent` warnings in the `use` statement —
// these are not used directly at the top of this file, but the
// imports remain for convenience when future changes are made.
#[allow(unused_imports)]
use {Arc as _Arc, ProtoEvent as _ProtoEvent};

#[cfg(test)]
pub(crate) mod test_helpers {
    //! Cross-submodule test helpers. The `mod tests` blocks of the five
    //! submodules share this module's helper functions and macros via
    //! `use crate::quic_transport::test_helpers::*;`.

    use std::sync::atomic::{AtomicU64, Ordering};

    use quinn::Endpoint;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
    use std::net::SocketAddr;
    use std::path::PathBuf;

    use crate::crypto;

    /// Run `$body` inside a fresh `LocalSet`, awaiting it inline.
    ///
    /// `local_set_test!` wraps the test body in `LocalSet::run_until` so
    /// that `spawn_local` / `JoinSet::spawn_local` work in unit tests.
    ///
    /// The caller is expected to be inside a `#[tokio::test]` `async fn`.
    /// The macro emits a single expression statement (no inner fn), so
    /// it does not produce `unnameable_test_items` / `dead_code`
    /// warnings.
    ///
    /// **Why the multi-thread flavor**: a multi-thread runtime lets
    /// `Send` futures such as the Quinn I/O driver and server task run
    /// on independent worker threads. The `LocalSet` runs the main
    /// future and `spawn_local` tasks separately, preventing the
    /// situation in the `current_thread` flavor where all `Send` tasks
    /// are queued behind the main future (the client dials and
    /// completes before the server task starts up, leading to a
    /// handshake timeout). Requires the tokio `rt-multi-thread`
    /// feature.
    macro_rules! local_set_test {
        ($name:ident, $body:block) => {
            tokio::task::LocalSet::new()
                .run_until(async move $body)
                .await
        };
    }

    /// Temporary self-signed cert for tests — written to an ephemeral
    /// subdirectory under `/tmp` (triple-isolated by PID + nanos + a
    /// global counter) to avoid polluting the user cert path
    /// (`crypto::cert_path()` / `key_path()`) and to keep multiple
    /// parallel tests from sharing a directory.
    /// Returns `(cert_chain, key)`; the DER bytes are fed directly into
    /// `endpoint_with_cert` / `build_quic_client_config`.
    pub(crate) fn ephemeral_cert() -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "lan-mouse-quic-test-{}-{}-{}",
            std::process::id(),
            nanos,
            n
        ));
        let cp = dir.join("cert.pem");
        let kp = dir.join("key.pem");
        crypto::generate_self_signed("lan-mouse-test", &cp, &kp).expect("self-sign test cert")
    }

    /// Temporary TOFU pins directory for tests — triple-isolated (PID +
    /// nanos + counter), same as `ephemeral_cert()`.
    pub(crate) fn ephemeral_pins_dir() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "lan-mouse-quic-pins-{}-{}-{}",
            std::process::id(),
            nanos,
            n
        ))
    }

    /// Test server endpoint assembly — directly calls the public
    /// `endpoint_with_cert`. The test helper shares the production code
    /// path.
    pub(crate) fn endpoint_with_test_cert(
        addr: SocketAddr,
        cert_chain: Vec<CertificateDer<'static>>,
        key: PrivateKeyDer<'static>,
    ) -> crate::quic_transport::Result<Endpoint> {
        crate::quic_transport::endpoint_with_cert(addr, cert_chain, key)
    }

    /// Build a `ServerName` for verifier tests. `localhost` is a valid DNS
    /// name on all platforms.
    pub(crate) fn test_server_name() -> ServerName<'static> {
        ServerName::try_from("localhost").expect("localhost is a valid DNS name")
    }

    /// Temporary `pins_dir` helper (symmetric in style with
    /// `ephemeral_cert()`). Returns `(dir, owned_path)` — `dir` is
    /// automatically cleaned up during the test.
    pub(crate) fn tmp_pins_dir(tag: &str) -> PathBuf {
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

    /// Temporary allowlist helper (symmetric in style with `tmp_pins_dir`).
    pub(crate) fn tmp_allowlist(
        tag: &str,
    ) -> std::sync::Arc<std::sync::RwLock<std::collections::HashMap<String, String>>> {
        let dir = std::env::temp_dir().join(format!(
            "lan-mouse-allowlist-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create allowlist dir");
        std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()))
    }

    /// Test motion event.
    pub(crate) fn motion_event() -> lan_mouse_proto::ProtoEvent {
        lan_mouse_proto::ProtoEvent::Input(input_event::Event::Pointer(
            input_event::PointerEvent::Motion {
                time: 4242,
                dx: 12.5,
                dy: -7.25,
            },
        ))
    }

    /// Test server endpoint assembly helper.
    pub(crate) fn motion_test_server(
        cert: Vec<CertificateDer<'static>>,
        key: PrivateKeyDer<'static>,
    ) -> (Endpoint, SocketAddr) {
        let ep = crate::quic_transport::endpoint_with_cert(
            std::net::SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, 0).into(),
            cert,
            key,
        )
        .expect("server endpoint bind");
        let addr = ep.local_addr().expect("server addr");
        (ep, addr)
    }

    /// Test keyboard key event.
    pub(crate) fn key_event() -> lan_mouse_proto::ProtoEvent {
        lan_mouse_proto::ProtoEvent::Input(input_event::Event::Keyboard(
            input_event::KeyboardEvent::Key {
                time: 0,
                key: 30,  // 'a'
                state: 1, // press
            },
        ))
    }

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    // Re-export the macro at the crate scope so that submodule tests can
    // invoke `local_set_test!(name, { body })` after
    // `use crate::quic_transport::test_helpers::local_set_test;`.
    pub(crate) use local_set_test;
}
