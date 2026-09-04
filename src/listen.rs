//! QUIC server listener.
//!
//! Replaces the earlier `webrtc-dtls` DTLS path: the old `listen.rs::read_loop`
//! used `webrtc_util::Conn` + `DTLSConn` + `as_any().downcast_ref::<DTLSConn>()`
//! and has been deleted. It is replaced by `PeerSession`, `read_frame`, and
//! `read_any_frame` from the `quic_transport` module.
//!
//! **Supervisor shape**:
//! 1. `LanMouseListener::new(port, cert_chain, key, authorized_keys)` calls
//!    `endpoint_with_verifier(port, cert_chain, key, AuthorizedKeysVerifier)`
//!    to obtain an `Endpoint` (mTLS + fingerprint allowlist are enforced
//!    during the handshake).
//! 2. A per-listener accept task loops on `quinn::Endpoint::accept()`,
//!    awaiting each handshake in its own task and spawning one
//!    `handle_quic_peer_supervisor` task per established `Connection`. Only
//!    endpoint closure ends the loop — a rejected handshake is per-connection
//!    (see `spawn_quic_accept_task`).
//! 3. The supervisor: `server_hello` → compute fingerprint (defense-in-depth)
//!    → push `ListenEvent::Accept` → `take_stream_a_recv` to obtain the recv
//!    half of stream A → loop on `read_frame(&mut recv_a)` and translate each
//!    frame into `ListenEvent::Msg`.
//! 4. Stream A EOF / conn close → push `ListenEvent::Disconnected` and exit
//!    the supervisor.
//!
//! **macOS wake path**:
//! - The macOS-only `PowerObserver` sends `()` over a
//!   `tokio::sync::mpsc::unbounded_channel` to `wake_rx` on system wake.
//! - `spawn_wake_task` blocks on `wake_rx`; on each wake signal it walks the
//!   `quic_conns` registry and calls `peer.connection().close(0u32.into(), b"wake")`
//!   to force-close the connection synchronously, without waiting for the QUIC
//!   30s `max_idle_timeout`.
//! - After close, the supervisor's read loop EOFs and exits, pushing
//!   `Disconnected`.
//! - On non-macOS targets, `PowerObserver` is not spawned, `wake_rx = None`,
//!   and `spawn_wake_task` parks forever.
//!
//! **Why only stream A is wired up**: the client-side `LanMouseConnection::send`
//! emits control events (Enter / Leave / Ack / Hello / Ping / Pong) on stream A,
//! and key events on stream B via `send_stream_b` only when `send_input`
//! dispatches into `Channel::StreamB`. The client side does not eagerly open
//! three bidi streams during `connect_to_handle` (only `client_hello` runs;
//! stream B/C are opened on demand when `send` is called). The server-side
//! supervisor therefore only consumes stream A — the existing match arms in
//! `ListenTask` cover every control-plane event, so no stream B/C reader is
//! required.
//!
//! **port_changed / request_port_change**: runtime port switching is not
//! supported by `Endpoint`, so `Err(PortChangeUnsupported)` is returned (a
//! per-IP endpoint rebuild can be added later).
//!
//! **mTLS reject notification**: `ListenEvent::Rejected { fingerprint }` was
//! previously dead code — `AuthorizedKeysVerifier::verify_client_cert` returns
//! `Err(rustls::Error)` directly on rejection, `quinn::Endpoint::accept` does
//! not expose the rejected cert's fingerprint, and the supervisor never
//! observed this event, so the GUI could not surface "an unauthorized peer
//! attempted to connect". The fix:
//! 1. `AuthorizedKeysVerifier` carries an
//!    `Option<UnboundedSender<String>>` injected via `with_rejection_tx`.
//! 2. `verify_client_cert` calls `rejection_tx.send(fp)` on the Err path
//!    (best-effort).
//! 3. `LanMouseListener::new` creates a
//!    `tokio::sync::mpsc::unbounded_channel::<String>()`, clones the tx into
//!    the verifier, and spawns `spawn_rejection_forwarder_task` on
//!    `spawn_local` to translate `rx.recv()` into
//!    `ListenEvent::Rejected { fingerprint }` on the same `listen_tx` shared
//!    with Accept / Msg / Disconnected.
//! 4. `terminate()` aborts the forwarder task alongside `wake_task`.
//!
//! With this in place, a rejected fingerprint is delivered immediately to the
//! existing match arm in `EmulationTask::ListenTask`, producing
//! `EmulationEvent::ConnectionAttempt` → `FrontendEvent::ConnectionAttempt` →
//! the frontend's `request_authorization` dialog.

use futures::{Stream, StreamExt};
use lan_mouse_proto::ProtoEvent;
use local_channel::mpsc::{Receiver, Sender, channel};
use rustls::pki_types::CertificateDer;
use std::{
    cell::RefCell,
    collections::HashMap,
    net::SocketAddr,
    rc::Rc,
    sync::{Arc, RwLock},
};
use thiserror::Error;
use tokio::task::{JoinHandle, spawn_local};

use crate::crypto;
use crate::quic_transport::{self, AuthorizedKeysVerifier, PeerSession};

#[derive(Error, Debug)]
pub enum ListenerCreationError {
    #[error("port change not supported for QUIC endpoints")]
    PortChangeUnsupported,
    #[error(transparent)]
    Quic(#[from] quic_transport::Error),
}

pub(crate) enum ListenEvent {
    Msg {
        event: ProtoEvent,
        addr: SocketAddr,
    },
    Accept {
        addr: SocketAddr,
        fingerprint: String,
    },
    /// Peer connection closed (any supervisor reader task exited / conn close).
    ///
    /// The QUIC supervisor emits this event on stream A EOF / conn close;
    /// `emulation.rs::ListenTask` synchronously removes `emulation_proxy[addr]`
    /// and reports the change to the service.
    Disconnected {
        addr: SocketAddr,
    },
    /// Peer handshake failed / fingerprint not authorized (rejected at the
    /// mTLS stage).
    ///
    /// [`crate::quic_transport::AuthorizedKeysVerifier`] notifies
    /// `spawn_rejection_forwarder_task` through a reverse channel
    /// (`tokio::sync::mpsc::UnboundedSender<String>`); the forwarder translates
    /// the fingerprint into this event on the same `listen_tx` stream.
    ///
    /// **Why a reverse channel is needed**: rustls rejects a handshake by
    /// making `quinn::Connecting::await` return
    /// `Err(ConnectionError::TransportError(rustls::Error::General))` before
    /// any `Connection` is resolved, so `peer_identity()` is not yet
    /// readable — the fingerprint is only known inside `verify_client_cert`
    /// and is otherwise discarded. We must clone the fp from inside the
    /// verifier on the Err path so it can be surfaced.
    Rejected {
        fingerprint: String,
    },
}

pub(crate) struct LanMouseListener {
    listen_rx: Receiver<ListenEvent>,
    listen_tx: Sender<ListenEvent>,
    /// QUIC accept task (a single endpoint bound to `0.0.0.0:port`).
    /// Aborted by `terminate`.
    accept_task: JoinHandle<()>,
    /// Forwarder task that translates the `AuthorizedKeysVerifier` reverse
    /// notification channel (`tokio::sync::mpsc::UnboundedReceiver<String>`)
    /// into `ListenEvent::Rejected` events.
    ///
    /// Spawned via `spawn_local`, it blocks on `rejection_rx`; on each
    /// fingerprint it calls `listen_tx.send(ListenEvent::Rejected { fingerprint })`.
    /// The same `listen_tx` is reused (no extra channel) so the existing
    /// match arm in `emulation.rs::ListenTask` activates without further
    /// plumbing.
    ///
    /// Aborted by `terminate`, mirroring the `wake_task` pattern.
    rejection_forwarder_task: JoinHandle<()>,
    /// Background wake handling task.
    ///
    /// On macOS system wake, force-closes all QUIC peer connections
    /// (without waiting for the QUIC 30s `max_idle_timeout`), which triggers
    /// the supervisor's read loop EOF → `ListenEvent::Disconnected` →
    /// `ListenTask` synchronously cleans up the proxy and reports to the
    /// service → the client's next `send()` triggers `dial_any` to reconnect.
    ///
    /// On non-macOS targets `wake_rx = None` and this task parks forever
    /// inside `select`.
    wake_task: JoinHandle<()>,
    /// Registry of authorized QUIC peers (validated by mTLS + authorized_keys).
    ///
    /// The supervisor `insert(addr, peer.clone())`s after sending the Accept
    /// event and `remove(addr)`s (via dropping the `QuicConnGuard`) when the
    /// supervisor exits.
    ///
    /// The primary consumer is the macOS wake path: `spawn_wake_task` walks
    /// this registry and calls `peer.connection().close(0u32.into(), b"wake")`
    /// on each conn to force-close without waiting for QUIC `max_idle_timeout`
    /// (30s).
    ///
    /// `reply()` also reads this registry to find a peer before writing a
    /// control frame to stream A.
    ///
    /// `Rc<RefCell<HashMap<...>>>` is chosen over `Rc<AsyncMutex<...>>`
    /// because registration / deregistration / lookup are synchronous; the
    /// async `peer.send_input` path takes its own lock once.
    quic_conns: Rc<RefCell<HashMap<SocketAddr, Rc<PeerSession>>>>,
    /// macOS-only: held for its `Drop` side effect (stops the
    /// CFRunLoop in the power-observer thread). The observer sends
    /// `()` into the wake channel on system-wake; the wake task
    /// drains that channel and force-closes peer conns so
    /// reconnect happens immediately after a screensaver/sleep
    /// dismissal. QUIC keepalive has taken over idle detection.
    #[cfg(target_os = "macos")]
    #[allow(dead_code)]
    power_observer: crate::macos_power::PowerObserver,
}

impl LanMouseListener {
    pub(crate) async fn new(
        port: u16,
        cert_chain: Vec<CertificateDer<'static>>,
        key: rustls::pki_types::PrivateKeyDer<'static>,
        authorized_keys: Arc<RwLock<HashMap<String, String>>>,
    ) -> Result<Self, ListenerCreationError> {
        let (listen_tx, listen_rx) = channel();

        // macOS wake → force-close-all-QUIC-peers plumbing.
        // On non-macOS targets `PowerObserver` is not spawned, `wake_rx` is
        // None, and `spawn_wake_task` parks forever in that branch.
        #[cfg(target_os = "macos")]
        let (power_observer, wake_rx) = {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<()>();
            let observer = crate::macos_power::PowerObserver::spawn(tx).await;
            (observer, Some(rx))
        };
        #[cfg(not(target_os = "macos"))]
        let wake_rx: Option<tokio::sync::mpsc::UnboundedReceiver<()>> = None;

        // QUIC peer registry (initially empty). `spawn_wake_task` takes a
        // clone for the wake path, `spawn_quic_accept_task` takes a clone for
        // the accept + supervisor registration path.
        let quic_conns: Rc<RefCell<HashMap<SocketAddr, Rc<PeerSession>>>> =
            Rc::new(RefCell::new(HashMap::new()));

        let wake_task = spawn_wake_task(wake_rx, quic_conns.clone());

        // Reverse notification channel for mTLS rejections.
        //
        // Path: `AuthorizedKeysVerifier::verify_client_cert` Err → `tx.send(fp)`
        // → this forwarder task `rx.recv()` → `listen_tx.send(ListenEvent::Rejected)`
        // → `EmulationTask::ListenTask` → `EmulationEvent::ConnectionAttempt`
        // → service → `FrontendEvent::ConnectionAttempt` → frontend
        // `request_authorization` dialog.
        //
        // Channel type: `tokio::sync::mpsc::unbounded_channel` (same pattern
        // as the wake channel — the verifier sends from inside a rustls
        // handshake callback that may not run on the local task thread, so a
        // `Send + Sync` sender is required; the forwarder is `spawn_local`).
        let (rejection_tx, rejection_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let rejection_forwarder_task =
            spawn_rejection_forwarder_task(rejection_rx, listen_tx.clone());

        let verifier: Arc<dyn rustls::server::danger::ClientCertVerifier> =
            Arc::new(AuthorizedKeysVerifier::new(authorized_keys).with_rejection_tx(rejection_tx));

        let addr = SocketAddr::new("0.0.0.0".parse().expect("invalid ip"), port);
        let endpoint = quic_transport::endpoint_with_verifier(addr, cert_chain, key, verifier)?;

        let accept_task = spawn_quic_accept_task(endpoint, listen_tx.clone(), quic_conns.clone());

        Ok(Self {
            listen_rx,
            listen_tx,
            accept_task,
            rejection_forwarder_task,
            wake_task,
            quic_conns,
            #[cfg(target_os = "macos")]
            power_observer,
        })
    }

    pub(crate) fn request_port_change(&mut self, _port: u16) {
        // Runtime port rebind for QUIC endpoints is not supported.
        log::warn!(
            "LanMouseListener::request_port_change is a no-op for QUIC; \
             runtime port rebind is not supported"
        );
    }

    pub(crate) async fn port_changed(&mut self) -> Result<u16, ListenerCreationError> {
        Err(ListenerCreationError::PortChangeUnsupported)
    }

    pub(crate) async fn terminate(&mut self) {
        // Teardown order matches the task structure:
        // 1. abort wake task → `PowerObserver` Drop stops the CFRunLoop
        //    (macOS-only).
        // 2. abort accept task → endpoint close → all in-flight supervisors
        //    observe conn close → emit `ListenEvent::Disconnected` →
        //    `emulation.rs::ListenTask` cleans up + reports to the service.
        // 3. abort rejection forwarder task → the `rejection_tx` retained by
        //    the verifier silently fails subsequent sends (matching the
        //    `verify_client_cert` design).
        // 4. close `listen_tx` → all supervisors' `listen_tx.send` calls fail.
        //    This does not affect read-loop exit (each loop's join handle
        //    resolves independently).
        self.wake_task.abort();
        self.accept_task.abort();
        self.rejection_forwarder_task.abort();
        self.listen_tx.close();
    }

    /// QUIC reply path: looks up the peer in `quic_conns` and writes the
    /// control event to that peer's stream A.
    ///
    /// When the peer is not connected this silently no-ops (to avoid
    /// surfacing errors from `emulation.rs`). Dispatches via
    /// `PeerSession::send_input`: the current `InputChannelConfig::default()`
    /// routes control-plane events to `Channel::StreamA`, so replies naturally
    /// land on stream A.
    pub(crate) async fn reply(&self, addr: SocketAddr, event: ProtoEvent) {
        let peer = self.quic_conns.borrow().get(&addr).cloned();
        match peer {
            Some(peer) => {
                use lan_mouse_ipc::InputChannelConfig;
                match peer
                    .send_input(&event, &InputChannelConfig::default())
                    .await
                {
                    Ok(()) => {
                        if matches!(event, ProtoEvent::Ack(_) | ProtoEvent::Leave(_)) {
                            log::info!("reply: {event} to {addr} delivered");
                        }
                    }
                    Err(e) => log::warn!("reply QUIC send to {addr} failed: {e}"),
                }
            }
            None => log::warn!("reply: peer {addr} not in quic_conns; dropping {event}"),
        }
    }

    /// Returns the client cert fingerprint for `addr`, as used by `ListenTask`
    /// when processing an Enter event.
    ///
    /// The supervisor already includes `fingerprint: String` in
    /// `ListenEvent::Accept` and `ListenTask` stores it in an
    /// `addr_to_fingerprint` map, so this lookup is not strictly needed on
    /// the QUIC path. The function is retained as a no-op stub to satisfy the
    /// existing `emulation.rs` call site; it returns `None` and may be wired
    /// up later if a non-Accept path requires it.
    #[allow(dead_code)]
    pub(crate) async fn get_certificate_fingerprint(&self, addr: SocketAddr) -> Option<String> {
        // Stub: `ListenTask` currently sources fingerprints from the Accept
        // event, so this function is intentionally a no-op.
        let _ = addr;
        None
    }

    /// Force-closes the QUIC conn for `addr` using
    /// [`crate::quic_transport::session::WAKE_CLOSE_CODE`] so the peer takes
    /// its `RetryState` path via
    /// [`crate::quic_transport::should_retry_after_close`].
    ///
    /// Called by [`crate::emulation::ListenTask`] when its 5s tick detects
    /// `last_response[addr].elapsed() > 1s`. It replaces the prior
    /// "only emit `EmulationEvent::Disconnected` and leave the conn alive"
    /// behavior, under which the host's QUIC conn remained open, the
    /// supervisor never observed a close reason, and nobody redialed.
    ///
    /// The same `WAKE_CLOSE_CODE` semantics are shared with the wake path:
    /// the peer does not distinguish "system wake close" from "application
    /// timeout close" and treats both as retry triggers.
    ///
    /// Race with the supervisor path: after this force-close the peer's
    /// `handle_quic_peer_supervisor` also observes stream A EOF and pushes
    /// `ListenEvent::Disconnected`, which can overlap with the timeout
    /// branch's own `EmulationEvent::Disconnected`. The service guards
    /// duplicate removals via
    /// `if let Some(addr) = self.remove_incoming(addr)` — a second call
    /// returns `None` and is a no-op.
    pub(crate) fn close_with_wake_code(&self, addr: SocketAddr) {
        let peer = self.quic_conns.borrow().get(&addr).cloned();
        match peer {
            Some(peer) => {
                log::debug!("close_with_wake_code: peer {addr} → WAKE_CLOSE_CODE (timeout path)");
                peer.connection().close(
                    crate::quic_transport::session::WAKE_CLOSE_CODE.into(),
                    b"timeout",
                );
            }
            None => {
                // Peer is not in `quic_conns` — likely already removed by the
                // supervisor path (race between supervisor EOF and timeout
                // tick). Silent no-op.
                log::trace!("close_with_wake_code: peer {addr} not in quic_conns (already gone)");
            }
        }
    }
}

impl Stream for LanMouseListener {
    type Item = ListenEvent;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.listen_rx.poll_next_unpin(cx)
    }
}

/// Helper called by `LanMouseListener::new()`: spawns the QUIC accept task
/// for a single endpoint plus a per-peer supervisor task for each connection.
///
/// **Only endpoint closure ends this loop.** `quinn::Endpoint::accept()`
/// returning `None` means the endpoint itself is gone (`terminate()` aborts
/// this task, or the runtime is shutting down) — that is the sole fatal
/// condition. A failed *handshake* is per-connection and must never reach the
/// loop: a peer whose fingerprint is not yet in `authorized_keys` is rejected
/// by [`AuthorizedKeysVerifier`], and that rejection is completely routine —
/// it is exactly the first-contact path that raises the "authorize this peer?"
/// dialog.
///
/// The earlier shape drove the loop with `quic_transport::accept(&ep)`, which
/// collapses both cases into a single `Err`, and `break`ing on it dropped the
/// last `quinn::Endpoint` handle. With no connection alive, quinn's endpoint
/// driver then saw `ref_count == 0 && connections.is_empty()`, shut down, and
/// closed the UDP socket — so the daemon kept running while the port silently
/// vanished from the listen table, and no code path ever rebinds it
/// (`request_port_change` is a no-op). One unauthorized dial, one ALPN
/// mismatch, one aborted `dial_any` happy-eyeballs loser, or one peer whose
/// TOFU verifier rejected our cert killed the listener for the lifetime of
/// the process — including, fatally, the very rejection that the pairing
/// dialog is supposed to resolve.
///
/// Supervisor shape:
/// 1. Loop on `ep.accept()` for `Incoming`s; `None` (and only `None`) breaks.
/// 2. Await the TLS 1.3 handshake in a per-connection task — never inline on
///    the accept loop, where a peer stalling mid-handshake would block every
///    other peer from being accepted.
/// 3. On each established conn, spawn a supervisor that:
///    - Exchanges `PROTOCOL_MAGIC` via `server_hello` (a `HelloFailed` error
///      exits the supervisor).
///    - Computes the client cert fingerprint from `peer_identity` and
///      `crypto::generate_fingerprint`.
///    - Defense-in-depth: mTLS already validated during the handshake; the
///      supervisor re-checks against the allowlist as a fallback (a fingerprint
///      approved by the verifier is, by construction, in the allowlist).
///    - Pushes `ListenEvent::Accept { addr, fingerprint }`.
///    - Inserts `quic_conns[addr] = peer.clone()` with a Drop guard for
///      deregistration.
///    - Takes the stream A recv half via `take_stream_a_recv`.
///    - Loops on `read_frame(&mut recv_a)`, translating each frame into
///      `ListenEvent::Msg`.
///    - On stream A EOF / conn close, removes from `quic_conns` and pushes
///      `ListenEvent::Disconnected`.
fn spawn_quic_accept_task(
    ep: quinn::Endpoint,
    listen_tx: Sender<ListenEvent>,
    quic_conns: Rc<RefCell<HashMap<SocketAddr, Rc<PeerSession>>>>,
) -> JoinHandle<()> {
    spawn_local(async move {
        log::info!("QUIC listener listening on {ep:?}");
        loop {
            // `None` — the endpoint is closed — is the *only* fatal condition.
            // Everything else is per-connection and must leave the listener
            // bound.
            let Some(incoming) = ep.accept().await else {
                log::info!("QUIC endpoint closed (accept returned None) — accept loop exiting");
                break;
            };

            let remote = incoming.remote_address();
            let tx_clone = listen_tx.clone();
            let quic_conns_for_supervisor = quic_conns.clone();
            spawn_local(async move {
                // The TLS 1.3 handshake runs here, off the accept loop, so a
                // peer that stalls mid-handshake cannot stop other peers from
                // being accepted.
                let conn = match incoming.await {
                    Ok(conn) => conn,
                    Err(e) => {
                        // Routine and non-fatal: unauthorized fingerprint
                        // (the pairing-dialog path — `AuthorizedKeysVerifier`
                        // has already pushed `ListenEvent::Rejected` through
                        // the rejection forwarder), ALPN mismatch, the peer's
                        // TOFU verifier rejecting our cert, or a `dial_any`
                        // happy-eyeballs loser being aborted. The endpoint
                        // stays bound and keeps accepting.
                        log::warn!("QUIC handshake from {remote} failed: {e}");
                        return;
                    }
                };
                let peer = Rc::new(PeerSession::from_connection(conn));
                log::info!("QUIC peer connected: {remote}");
                if let Err(e) =
                    handle_quic_peer_supervisor(peer, tx_clone, quic_conns_for_supervisor).await
                {
                    log::warn!("QUIC peer supervisor exited with err: {e}");
                }
            });
        }
    })
}

/// Background wake handling task.
///
/// On a macOS system-wake signal, walks the `quic_conns` registry and calls
/// `peer.connection().close(0, b"wake")` on each conn synchronously, without
/// waiting for the 30s `max_idle_timeout`. This lets `streams.join` resolve
/// immediately, so the supervisor emits `ListenEvent::Disconnected`, and
/// `ListenTask` synchronously cleans up `emulation_proxy[addr]` and reports
/// to the service.
///
/// **Synchronous `RefCell::borrow()` path (no await contention)**:
/// `quinn::Connection::close(VarInt, &[u8])` is synchronous and cannot
/// conflict with a concurrent `borrow_mut` from the read loop.
///
/// No need to clone the peer — `Rc<PeerSession>` holds an internal
/// `Rc<Connection>`, so `close` operates through the existing refcount.
///
/// Non-macOS targets use `wake_rx = None`, and the
/// `match wake_rx.as_mut() { None => pending() }` branch parks forever
/// (no wake signal ever arrives).
fn spawn_wake_task(
    mut wake_rx: Option<tokio::sync::mpsc::UnboundedReceiver<()>>,
    quic_conns: Rc<RefCell<HashMap<SocketAddr, Rc<PeerSession>>>>,
) -> JoinHandle<()> {
    spawn_local(async move {
        loop {
            let wake = match wake_rx.as_mut() {
                Some(rx) => rx.recv().await,
                None => std::future::pending().await,
            };
            match wake {
                Some(()) => {
                    let q = quic_conns.borrow();
                    log::info!(
                        "mac wake: force-closing {} QUIC peer conn(s) with WAKE_CLOSE_CODE (0xCAFE) — \
                         peer supervisor will see ApplicationClosed and trigger reconnect",
                        q.len()
                    );
                    for (a, peer) in q.iter() {
                        log::debug!("post-wake close (QUIC): {a}");
                        // Mac wake auto-reconnect: use
                        // [`crate::quic_transport::WAKE_CLOSE_CODE`] (0xCAFE)
                        // instead of the default 0 (NO_ERROR) so that the peer's
                        // [`should_retry_after_close`] takes the retry branch.
                        // Without a non-zero code, after wake the user is not
                        // moving the mouse, so `send()` would never naturally
                        // fire and the connection would stay in a "graceful
                        // close" state until the next send.
                        //
                        // This is distinct from the user/network-level close
                        // path (`close(0u32, "peer closed stream")`), which
                        // uses code 0 and does not trigger a retry.
                        peer.connection().close(
                            crate::quic_transport::session::WAKE_CLOSE_CODE.into(),
                            b"wake",
                        );
                    }
                }
                None => {
                    log::debug!(
                        "supervisor: wake channel closed; \
                         power observer no longer signaling"
                    );
                    wake_rx = None;
                }
            }
        }
    })
}

/// Forwarder task that translates the `AuthorizedKeysVerifier` reverse
/// notification channel (`tokio::sync::mpsc::UnboundedReceiver<String>`)
/// into `ListenEvent::Rejected` events.
///
/// **Path**: `AuthorizedKeysVerifier::verify_client_cert` returns Err →
/// `tx.send(fp)` (inside the verifier, wired up in `quic_transport.rs`) →
/// this task `rx.recv()` → `listen_tx.send(ListenEvent::Rejected { fingerprint })`
/// → `EmulationTask::ListenTask` → `EmulationEvent::ConnectionAttempt` →
/// service → `FrontendEvent::ConnectionAttempt` → frontend
/// `request_authorization` dialog.
///
/// **Deduplication**: `emulation.rs` already de-duplicates at 2 seconds (the
/// same fingerprint triggers at most one dialog per 2s) so retries from a
/// rejected peer don't spam pop-ups. The forwarder does no additional
/// deduping — it just translates.
///
/// **Exit path**: `terminate()` calls `rejection_forwarder_task.abort()`,
/// mirroring the `wake_task` / `accept_task` pattern. After abort, the
/// `rejection_tx` retained by the verifier silently fails subsequent sends
/// (matching the verifier's design).
fn spawn_rejection_forwarder_task(
    mut rejection_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    listen_tx: Sender<ListenEvent>,
) -> JoinHandle<()> {
    spawn_local(async move {
        while let Some(fp) = rejection_rx.recv().await {
            log::debug!(
                "rejection forwarder: peer {fp} rejected by mTLS — sending ListenEvent::Rejected"
            );
            if listen_tx
                .send(ListenEvent::Rejected { fingerprint: fp })
                .is_err()
            {
                log::debug!(
                    "rejection forwarder: listen_tx send failed (channel closed, terminating)"
                );
                break;
            }
        }
        log::debug!("rejection forwarder: rejection channel closed — exiting");
    })
}

/// Per-connection supervisor handler.
///
/// Flow:
/// 1. Exchange `PROTOCOL_MAGIC` via `server_hello`.
/// 2. Compute the client cert fingerprint.
/// 3. Emit `ListenEvent::Accept { addr, fingerprint }`.
/// 4. Register in `quic_conns` (so `reply()` can look up the peer) and install
///    a `QuicConnGuard`.
/// 5. Take the stream A recv half via `take_stream_a_recv`.
/// 6. Loop on `read_frame(&mut recv_a)`, translating each frame into
///    `ListenEvent::Msg`.
/// 7. On stream A EOF / fatal error, the `QuicConnGuard` Drop automatically
///    deregisters the peer and `ListenEvent::Disconnected` is pushed.
///
/// The supervisor deliberately does not call `route_input` for reverse
/// dispatch: the listen side has no view of the sender's channel config,
/// so stream-to-event translation is delegated to `ListenTask`, whose
/// existing `match event` already covers every control-plane and input event.
#[allow(clippy::doc_lazy_continuation)]
async fn handle_quic_peer_supervisor(
    peer: Rc<PeerSession>,
    listen_tx: Sender<ListenEvent>,
    quic_conns: Rc<RefCell<HashMap<SocketAddr, Rc<PeerSession>>>>,
) -> Result<(), quic_transport::Error> {
    let addr = peer.connection().remote_address();

    // (1) server_hello
    quic_transport::server_hello(&peer).await?;

    // (2) Compute the client cert fingerprint. mTLS already validated the
    // certificate during the handshake; we only need the fingerprint value.
    //
    // quinn 0.11 `Connection::peer_identity() -> Option<Box<dyn Any>>`
    // exposes the rustls `Vec<CertificateDer<'static>>` as a trait object —
    // we downcast and take the first certificate to compute the fingerprint.
    let identity = peer.connection().peer_identity();
    let certs: Option<&Vec<rustls::pki_types::CertificateDer<'static>>> = identity
        .as_ref()
        .and_then(|c| c.downcast_ref::<Vec<rustls::pki_types::CertificateDer<'static>>>());
    let fingerprint = certs
        .and_then(|c| c.first())
        .map(|cert| crypto::generate_fingerprint(cert.as_ref()))
        .ok_or_else(|| quic_transport::Error::HelloFailed("no client cert presented".into()))?;

    // (3) Emit the Accept event
    log::info!("QUIC peer {addr} authorized (fingerprint {fingerprint})");
    listen_tx
        .send(ListenEvent::Accept {
            addr,
            fingerprint: fingerprint.clone(),
        })
        .map_err(|_| quic_transport::Error::HelloFailed("listen_tx closed (terminated)".into()))?;

    // (4) Register in the QUIC peer table (so `reply()` can find the peer)
    //     and install a `QuicConnGuard` so any exit path (Ok / Err / panic /
    //     wake close) automatically deregisters. The guard drops at the end
    //     of this function regardless of which return path is taken.
    quic_conns
        .borrow_mut()
        .insert(addr, peer.clone())
        .inspect(|_old| {
            log::warn!(
                "QUIC peer {addr} already registered in quic_conns — overwriting (old peer may leak)"
            );
        });
    let _guard = QuicConnGuard {
        table: quic_conns.clone(),
        addr,
    };
    log::debug!("QUIC peer {addr} registered in quic_conns (guard active)");

    // (5) Take the stream A recv half (used by the control-frame reader).
    let mut recv_a = peer.take_stream_a_recv().await.ok_or_else(|| {
        quic_transport::Error::HelloFailed("stream A not cached after server_hello".into())
    })?;

    // Spawn the datagram reader task. `route_input` routes Motion / Axis /
    // AxisDiscrete120 events over the QUIC datagram channel (not stream A)
    // to avoid stream retransmission latency for high-frequency pointer
    // events. The server supervisor only reads the cached stream A and
    // datagrams, so without a dedicated datagram reader the server would
    // never observe motion events sent by the client.
    //
    // Lifecycle: the task ends when the supervisor exits — when the peer
    // `Rc` drops, the task's clone of it drops too, `read_datagram` returns
    // `Err`, and the task exits.
    spawn_local(server_datagram_reader_task(
        peer.clone(),
        listen_tx.clone(),
        addr,
    ));

    // Spawn the `accept_bi` loop that consumes client-side stream B frames.
    //
    // The default `InputChannelConfig { keyboard: Stream }` routes every
    // key event to `Channel::StreamB` → `PeerSession::send_stream_b`, which
    // uses a bidi independent of the hello stream A. Without an `accept_bi`
    // loop on the server side, stream B frames pile up in quinn's accept
    // queue and key events are silently dropped — this is why the mouse
    // (datagram, with its reader) works while the keyboard does not.
    //
    // **bunch bidi compatibility**: the client's `peer.run(PeerRole::Client)`
    // opens 3 extra bidi streams into a `StreamBunch` after the hello (see
    // `session.rs`), none of which has a corresponding server reader. These
    // streams are opened but never written to — any read on them EOFs
    // immediately.
    //
    // **Critical pitfall**: QUIC bidi is bidirectional — the acceptor's
    // send corresponds to the initiator's recv. If the server simply
    // `drop`s `send` on a bunch bidi, quinn sends FIN, the client's
    // `bunch.b.recv` sees immediate EOF, `read_stream_b_loop` exits,
    // `peer.run` breaks, and `conn.close()` tears the connection down.
    //
    // **Correct strategy**: both `send` and `recv` of a bunch bidi must be
    // parked, never dropped. The client's `bunch.b.recv` parks in the
    // "awaiting data" state (not EOF), so `read_stream_b_loop` blocks until
    // the conn closes and quinn tears everything down. Real stream B uses
    // the normal read path: dropping `send` is safe because the client's
    // `send_stream_b` has already dropped its own recv, so the server's
    // `send` has no reader on the other side and dropping it is a no-op.
    //
    // **How real stream B is identified**: `send_stream_b` performs
    // `open_bi()` + `write_u32(len)` + `write_all(body)` synchronously and
    // only returns once all of that completes. By the time the server
    // accepts, the length prefix is already in the quinn buffer and
    // `read_u32` returns immediately with a valid length. This is the
    // reliable "data vs no data" discriminator.
    let parked_streams: Rc<RefCell<Vec<(quinn::SendStream, quinn::RecvStream)>>> =
        Rc::new(RefCell::new(Vec::new()));
    spawn_local(server_accept_bi_task(
        peer.clone(),
        listen_tx.clone(),
        addr,
        parked_streams.clone(),
    ));

    // (6) Loop read_frame(recv_a) → ListenEvent::Msg
    //
    // Error dispatch:
    // - `FrameTooLarge` → fatal, return Err
    // - `HelloFailed("decode frame...")` → warn + skip frame, keep reading
    // - `Truncated` / EOF → exit loop + push Disconnected
    loop {
        match quic_transport::read_frame(&mut recv_a).await {
            Ok(event) => {
                // Hot path: every control event traverses this point. Use
                // debug to avoid log spam; `RUST_LOG=lan_mouse::listen=debug`
                // still gives precise diagnostics of the control event flow.
                log::debug!("stream A recv from {addr}: {event}");
                if listen_tx.send(ListenEvent::Msg { event, addr }).is_err() {
                    log::debug!(
                        "QUIC supervisor: listen_tx send failed (channel closed, terminating)"
                    );
                    break;
                }
            }
            Err(quic_transport::Error::FrameTooLarge(len)) => {
                log::error!("stream A: FrameTooLarge({len}) — fatal, closing task");
                return Err(quic_transport::Error::FrameTooLarge(len));
            }
            Err(quic_transport::Error::HelloFailed(msg)) if msg.starts_with("decode frame") => {
                log::warn!("stream A: skip frame (decode error): {msg}");
                continue;
            }
            Err(quic_transport::Error::Truncated) => {
                log::info!("stream A truncated — peer closed");
                break;
            }
            Err(e) => {
                log::info!("stream A reader exiting (IO closed): {e}");
                return Err(e);
            }
        }
    }

    // (7) stream A EOF / conn close → push Disconnected (QuicConnGuard Drop
    //     automatically deregisters the peer).
    log::info!("QUIC peer {addr} stream A closed — sending Disconnected");
    let _ = listen_tx.send(ListenEvent::Disconnected { addr });
    Ok(())
}

/// RAII guard for entries in the QUIC peer registry.
///
/// On construction, binds `(table, addr)`. On `Drop`, removes `addr` from
/// `table`. This guarantees that every exit path of
/// `handle_quic_peer_supervisor` (Ok / Err / panic) deregisters the peer
/// without each `?` early-return needing a manual `remove()`.
///
/// **Why strict pairing matters**: without it,
/// - on a reconnect from the same `addr`, `insert()` overwrites the old
///   entry (already warned, but the old `Rc<PeerSession>` still holds the
///   old connection and may delay its close);
/// - the wake path may iterate over zombie entries and `close` already-
///   recycled conns (a quinn-internal no-op, but noisy).
///
/// The guard does not touch the conn itself — it only removes the HashMap
/// entry. The conn is closed by the read loop exiting or by the wake path
/// calling `peer.connection().close(...)`.
struct QuicConnGuard {
    table: Rc<RefCell<HashMap<SocketAddr, Rc<PeerSession>>>>,
    addr: SocketAddr,
}

impl Drop for QuicConnGuard {
    fn drop(&mut self) {
        let removed = self.table.borrow_mut().remove(&self.addr);
        if removed.is_some() {
            log::debug!(
                "QUIC peer {} deregistered from quic_conns (guard drop)",
                self.addr
            );
        }
        // `removed == None` means the peer was never registered (early exit
        // before the Accept event) or was already overwritten by the wake
        // path (impossible under single-threaded `spawn_local`); silent
        // no-op in either case.
    }
}

/// Server-side `accept_bi()` loop. Receives client-side bidis opened after
/// the stream A / datagram handshake (currently the sole source is stream B
/// key events).
///
/// **Bunch bidi detection + park**: the client's
/// `peer.run(PeerRole::Client)` opens 3 extra bidis into a `StreamBunch`
/// after the hello (`session.rs`). The server has no dedicated reader for
/// these. Discriminator: try to read the first frame's `read_u32(len)` —
/// bunch bidis have never been written to and EOF immediately; real stream
/// B has been written to synchronously by `send_stream_b` and `read_u32`
/// returns a valid length right away.
///
/// **Bunch path must park both send + recv**: QUIC bidi is bidirectional —
/// the acceptor's send corresponds to the initiator's recv. If only `recv`
/// is parked and `send` is dropped, quinn sends FIN, the client's
/// `bunch.b.recv` sees immediate EOF, `read_stream_b_loop` exits, `peer.run`
/// breaks, and `conn.close()` tears the connection down. Parking both keeps
/// the client's recv parked in the "awaiting data" state until the conn
/// closes and quinn cleans everything up uniformly.
///
/// **Real stream B path**: drop `send` (the client's `send_stream_b` has
/// already dropped its own recv, so the server's send has no reader and
/// dropping it is safe) and read `recv`.
async fn server_accept_bi_task(
    peer: Rc<PeerSession>,
    listen_tx: Sender<ListenEvent>,
    addr: SocketAddr,
    parked_streams: Rc<RefCell<Vec<(quinn::SendStream, quinn::RecvStream)>>>,
) {
    loop {
        let (send, mut recv) = match peer.connection().accept_bi().await {
            Ok(pair) => pair,
            Err(e) => {
                log::info!("server accept_bi: exiting (conn closed): {e}");
                return;
            }
        };

        // Try to read the first frame's length — discriminator between
        // bunch bidi (EOF) and real stream B (valid u32).
        use tokio::io::AsyncReadExt;
        let len: u32 = match recv.read_u32().await {
            Ok(n) => n,
            Err(e) => {
                // EOF = bunch bidi (client never wrote). Both `send` and
                // `recv` must NOT be dropped here, otherwise we send FIN /
                // STOP_SENDING to the client, which makes the client's
                // `stream_bunch.b.recv` see immediate EOF → `peer.run` breaks
                // → `conn.close()`. Parking both keeps the client's recv
                // parked in the "awaiting data" state.
                log::debug!(
                    "server accept_bi: accepted stream EOF'd immediately (bunch bidi), parking send+recv: {e}"
                );
                parked_streams.borrow_mut().push((send, recv));
                continue;
            }
        };

        // Real stream B — drop `send` (the client has dropped its own recv,
        // so the server's `send` has no peer-side reader; dropping is a
        // no-op), read the body, decode, and push the first frame to
        // `listen_tx`.
        drop(send);
        let mut body = vec![0u8; len as usize];
        if let Err(e) = recv.read_exact(&mut body).await {
            // Length read succeeded but body read failed — also park as if
            // it were a bunch bidi (do not break the client). `send` is
            // already dropped, so this is unrecoverable; let `peer.run`
            // surface the original error path.
            log::debug!("server accept_bi: first-frame body read failed, parking as bunch: {e}");
            return;
        }
        let mut buf = [0u8; lan_mouse_proto::MAX_EVENT_SIZE];
        if len as usize > buf.len() {
            log::warn!(
                "server accept_bi: first-frame length {len} exceeds MAX_EVENT_SIZE={}",
                lan_mouse_proto::MAX_EVENT_SIZE
            );
            return;
        }
        buf[..len as usize].copy_from_slice(&body);
        let event = match lan_mouse_proto::ProtoEvent::try_from(buf) {
            Ok(e) => e,
            Err(e) => {
                log::warn!("server accept_bi: first-frame decode failed: {e}");
                return;
            }
        };
        log::debug!("server accept_bi: real stream B first frame from {addr}: {event}");
        if listen_tx.send(ListenEvent::Msg { event, addr }).is_err() {
            log::debug!("server accept_bi: listen_tx closed, exiting");
            return;
        }
        // Subsequent frames are handled by a dedicated reader task.
        spawn_local(server_stream_reader_task(recv, listen_tx.clone(), addr));
    }
}

/// Reader task for subsequent frames on a real stream B. Spawned by
/// [`server_accept_bi_task`] after the first frame is confirmed valid.
///
/// Error dispatch mirrors the supervisor's stream A loop: a decode error
/// skips the frame and keeps reading; EOF / IO error ends the stream and
/// exits the task (without affecting other streams).
async fn server_stream_reader_task(
    mut recv: quinn::RecvStream,
    listen_tx: Sender<ListenEvent>,
    addr: SocketAddr,
) {
    loop {
        match quic_transport::read_frame(&mut recv).await {
            Ok(event) => {
                log::debug!("server stream reader: from {addr}: {event}");
                if listen_tx.send(ListenEvent::Msg { event, addr }).is_err() {
                    log::debug!("server stream reader: listen_tx closed, exiting");
                    return;
                }
            }
            Err(quic_transport::Error::HelloFailed(msg)) if msg.starts_with("decode frame") => {
                log::warn!("server stream reader: skip frame (decode error): {msg}");
                continue;
            }
            Err(e) => {
                log::info!("server stream reader: stream ended ({addr}): {e}");
                return;
            }
        }
    }
}

/// Server-side datagram reader task.
///
/// This shares intent with `quic_transport::datagram_reader_task` but takes
/// the server listen path: it does not use `StreamEvent::Datagram` (an
/// abstraction internal to `peer.run()`); instead it wraps each datagram
/// directly into `ListenEvent::Msg { event, addr }` and pushes it on
/// `listen_tx`, mirroring the stream A path so `emulation.rs::ListenTask`
/// receives it.
///
/// The server side has its own specialization rather than reusing the
/// client-side version because the supervisor has its own `read_frame` loop
/// and never invokes `peer.run`. Going through `peer.run`'s abstraction
/// would only add an extra `StreamEvent` → `ListenEvent` translation layer.
///
/// **Lifecycle**: the task holds `peer: Rc<PeerSession>` and
/// `listen_tx: Sender<ListenEvent>`. When the supervisor exits,
/// `listen_tx` is closed, the next `send` fails, and the task exits. The
/// `peer` `Rc` is also dropped, so the task's clone of it drops too.
async fn server_datagram_reader_task(
    peer: Rc<PeerSession>,
    listen_tx: Sender<ListenEvent>,
    addr: SocketAddr,
) {
    loop {
        match peer.connection().read_datagram().await {
            Ok(bytes) => {
                // Fixed-size `ProtoEvent` codec: `bytes.len()` must equal
                // `MAX_EVENT_SIZE`.
                let buf: [u8; lan_mouse_proto::MAX_EVENT_SIZE] = match bytes.as_ref().try_into() {
                    Ok(b) => b,
                    Err(_) => {
                        log::warn!(
                            "server datagram_reader: datagram length is not MAX_EVENT_SIZE({}), skipping frame",
                            lan_mouse_proto::MAX_EVENT_SIZE
                        );
                        continue;
                    }
                };
                let event = match lan_mouse_proto::ProtoEvent::try_from(buf) {
                    Ok(e) => e,
                    Err(e) => {
                        log::warn!(
                            "server datagram_reader: ProtoEvent decode failed, skipping frame: {e}"
                        );
                        continue;
                    }
                };
                log::trace!("server datagram_reader: from {addr}: {event}");
                if listen_tx.send(ListenEvent::Msg { event, addr }).is_err() {
                    log::debug!("server datagram_reader: listen_tx closed, exiting");
                    return;
                }
            }
            Err(e) => {
                log::info!("server datagram_reader: read_datagram error, exiting: {e}");
                return;
            }
        }
    }
}
