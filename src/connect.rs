use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    io,
    net::SocketAddr,
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use lan_mouse_ipc::{ClientHandle, DEFAULT_PORT, InputChannelConfig};
use lan_mouse_proto::ProtoEvent;
use local_channel::mpsc::{Receiver, Sender, channel};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use thiserror::Error;
use tokio::{sync::Mutex, task::spawn_local};

use crate::client::ClientManager;
use crate::quic_transport::{self, Endpoint, PeerRole, PeerSession, should_retry_after_close};

/// mTLS client cert + key presented to the peer.
///
/// Reuses the same cert/key persisted by `crypto::load_or_create_server_cert()` —
/// lan-mouse is both a client and a server (mTLS is presented on both sides), so
/// reusing one credential pair is simpler than maintaining two DER blobs.
///
/// `Rc` wrapping lets [`LanMouseConnection`] and all clones of this type share a
/// single credential set (avoiding `PrivateKeyDer::clone_key()` re-parsing the
/// DER bytes on every clone).
pub(crate) struct QuicDialerCreds {
    pub cert_chain: Vec<CertificateDer<'static>>,
    pub key: PrivateKeyDer<'static>,
}

#[allow(dead_code)]
// Timeout / TargetEmulationDisabled have no constructors today; TargetEmulationDisabled
// will be re-enabled once the Pong → recv_tx path is wired up.
#[derive(Debug, Error)]
pub(crate) enum LanMouseConnectionError {
    #[error(transparent)]
    Bind(#[from] io::Error),
    /// QUIC transport-layer error. Failures from `PeerSession::send_input()` /
    /// `dial()` are surfaced to upper layers through this variant.
    #[error(transparent)]
    Quic(#[from] quic_transport::Error),
    #[error("not connected")]
    NotConnected,
    /// Temporarily unused: the alive check was removed (see the `send()` docstring).
    /// Kept so the variant can be re-enabled once the Pong → recv_tx path is wired back in.
    #[error("emulation is disabled on the target device")]
    TargetEmulationDisabled,
    #[error("Connection timed out")]
    Timeout,
}

/// Outbound QUIC connection management.
///
/// **Architecture**:
/// - `client_endpoint: Endpoint` — single endpoint shared across peers (quinn's
///   `Endpoint: Clone`, backed by internal `Arc`). In production it binds
///   `0.0.0.0:0` (any local v4 port) once at construction time in `service.rs::new`.
/// - `quic_creds: Rc<QuicDialerCreds>` — mTLS dialer credentials, reused per-connection.
/// - `peers: Rc<Mutex<HashMap<SocketAddr, Arc<PeerSession>>>>` — QUIC session table;
///   on a hit `send()` calls `peer.send_input(&event, &cfg)` which dispatches to
///   datagram / stream A / stream B per [`crate::quic_transport::route_input`].
/// - `connecting: Rc<Mutex<HashSet<ClientHandle>>>` — in-flight dial de-duplication,
///   prevents repeated `connect_to_handle` from racing.
/// - `recv_tx: Sender<(ClientHandle, ProtoEvent)>` — sender half of the receive
///   channel consumed by `recv()`.
/// - `peer_lost_tx: Sender<ClientHandle>` — supervisor → capture signal:
///   "the Pong health watchdog decided this peer is dead; release capture
///   immediately". Receiver half lives on `Capture`, not here, so capture's
///   `select!` loop can use it without conflicting with `self.conn.recv()`.
pub(crate) struct LanMouseConnection {
    quic_creds: Rc<QuicDialerCreds>,
    client_endpoint: Endpoint,
    client_manager: ClientManager,
    peers: Rc<Mutex<HashMap<SocketAddr, Arc<PeerSession>>>>,
    connecting: Rc<Mutex<HashSet<ClientHandle>>>,
    pins_dir: PathBuf,
    recv_rx: Receiver<(ClientHandle, ProtoEvent)>,
    recv_tx: Sender<(ClientHandle, ProtoEvent)>,
    /// Per-handle retry backoff gate. Uses `tokio::time::sleep` to schedule reconnects.
    /// When `failure_count` reaches `MAX_RETRY_FAILURES_BEFORE_OFFLINE` a `log::error`
    /// is emitted; `TransportEvent::PeerLost` is **not** pushed to IPC.
    retry_state: Rc<RefCell<HashMap<ClientHandle, RetryState>>>,
    /// QUIC `max_idle_timeout` passed to every outbound dial. Frozen at
    /// service startup from [`crate::config::Config::quic_idle_timeout`].
    /// Subsequent edits via the GUI are persisted to TOML but not
    /// picked up until the daemon restarts — same "endpoint owns its
    /// transport config for the lifetime of the process" rule that
    /// applies to the server-side listener.
    idle_timeout: std::time::Duration,
    /// Sender for the supervisor → capture "peer dead" signal. Cloned
    /// into every supervisor's pong health watchdog. The receiver
    /// lives on [`crate::capture::Capture`] (not here) so it can be
    /// polled from `do_capture_session`'s `select!` without colliding
    /// with `self.conn.recv()`.
    peer_lost_tx: Sender<ClientHandle>,
}

impl LanMouseConnection {
    pub(crate) fn new(
        client_endpoint: Endpoint,
        cert_chain: Vec<CertificateDer<'static>>,
        key: PrivateKeyDer<'static>,
        pins_dir: PathBuf,
        client_manager: ClientManager,
        idle_timeout: std::time::Duration,
        peer_lost_tx: Sender<ClientHandle>,
    ) -> Self {
        let (recv_tx, recv_rx) = channel();
        let quic_creds = Rc::new(QuicDialerCreds { cert_chain, key });
        Self {
            quic_creds,
            client_endpoint,
            client_manager,
            peers: Default::default(),
            connecting: Default::default(),
            pins_dir,
            recv_rx,
            recv_tx,
            retry_state: Default::default(),
            idle_timeout,
            peer_lost_tx,
        }
    }

    pub(crate) async fn recv(&mut self) -> (ClientHandle, ProtoEvent) {
        self.recv_rx.recv().await.expect("channel closed")
    }

    /// Proactively dials a peer without sending any events.
    ///
    /// **Background**: [`send()`] only triggers a dial when there's an event to
    /// send. If both sides start their daemons and nobody moves the mouse, the
    /// edge-trigger never fires and the link is never established.
    ///
    /// **Semantics**: Equivalent to the second half of `send()` — check the
    /// RetryState gate → check `connecting` for de-duplication →
    /// `spawn_local(connect_to_handle)`. Sends no `ProtoEvent`s and returns no
    /// errors other than `NotConnected`.
    ///
    /// **When it's called**: `service.rs::activate_client` invokes it
    /// fire-and-forget immediately after `client_manager.activate_client(handle)`
    /// succeeds. The two paths stay independent:
    /// - `activate_client` → `dial(handle)` (proactive dial)
    /// - mouse hits screen edge → `send()` triggers dial (existing path)
    ///
    /// **De-duplication**: The `connecting` set guarantees that the same handle
    /// cannot spawn concurrent `connect_to_handle` tasks (`connecting.insert(handle)`
    /// is called before `spawn_local(connect_to_handle)`, and `connecting.remove(&handle)`
    /// runs at the end of `connect_to_handle` on both success and failure). The
    /// RetryState backoff gate is shared with `send()`.
    ///
    /// **Fire-and-forget**: returns `Ok(())` immediately after spawn, without
    /// blocking the caller. Dial outcomes are handled inside `connect_to_handle`
    /// (on success: register peer + spawn supervisor; on failure: `record_retry_failure`).
    pub(crate) async fn dial(&self, handle: ClientHandle) -> Result<(), LanMouseConnectionError> {
        // RetryState gate — same semantics as send(): do not spawn a dial during the
        // backoff window.
        {
            let map = self.retry_state.borrow();
            if let Some(entry) = map.get(&handle) {
                let now = std::time::Instant::now();
                if now < entry.next_attempt_at {
                    log::trace!(
                        "client {handle} dial() RetryState gate: waiting for backoff ({:?} remaining)",
                        entry.next_attempt_at - now
                    );
                    return Ok(());
                }
            }
        }
        let mut connecting = self.connecting.lock().await;
        if !connecting.contains(&handle) {
            connecting.insert(handle);
            spawn_local(connect_to_handle(
                self.client_manager.clone(),
                self.client_endpoint.clone(),
                self.quic_creds.clone(),
                self.peers.clone(),
                self.connecting.clone(),
                self.pins_dir.clone(),
                self.retry_state.clone(),
                self.recv_tx.clone(),
                handle,
                self.idle_timeout,
                self.peer_lost_tx.clone(),
            ));
        }
        Ok(())
    }

    /// Sends an event to the peer.
    ///
    /// **3-step flow**:
    /// 1. Look up the socket addr via `client_manager.active_addr(handle)`.
    /// 2. Look up the QUIC session in the `peers` table. On hit, call
    ///    [`PeerSession::send_input`]; on miss, trigger a dial.
    /// 3. Error reconciliation (on `send_input` failure, drop the peer and
    ///    notify the manager).
    ///
    /// **Alive guard (currently disabled)**: The original design set
    /// `alive = false` when the peer reported emulation disabled (Pong returns
    /// `false`), so that subsequent `send()` calls returned
    /// `TargetEmulationDisabled` to avoid meaningless injection. However, `alive`
    /// is never set to `true` in the current architecture — the server emits Pong
    /// from `src/emulation.rs:164` (`ProtoEvent::Ping => reply Pong(emulation_active)`),
    /// but no client-side reader forwards Pong frames from stream A to `recv_tx`.
    /// As a result:
    /// 1. Server sends Pong(true) → client `peer.run()` reads it from stream A,
    ///    but the main loop does not push Pong to `recv_tx`.
    /// 2. `(handle, event) = self.conn.recv()` in `capture.rs` blocks forever.
    /// 3. `alive` stays at its default `false`
    ///    (`lan_mouse_ipc::ClientState::default()`).
    /// 4. Every `send()` against an existing peer returns `TargetEmulationDisabled`
    ///    → `log::warn!("releasing capture: ...")` in `capture.rs` → capture is
    ///    released as soon as the mouse hits the screen edge, looking like a
    ///    "connected but unresponsive" link.
    ///
    /// **Temporary fix**: Remove the alive check. **Optimistically assume the
    /// peer is online** — every `send` is attempted; when the supervisor sees
    /// `peer.run()` exit (peer really dead) it calls `set_active_addr(None)` and
    /// the next `send` follows the redial path. This matches the pre-QUIC
    /// "peer dead → supervisor closes → redial" semantics, minus the
    /// "Pong false negative → return early" optimization.
    ///
    /// **TODO**: once the stream A reader → `recv_tx` forwarder is wired in,
    /// re-enable the alive check and handle Pong(true/false):
    /// - Pong(true) → `set_alive(true)`
    /// - Pong(false) → `set_alive(false)` → next `send` returns
    ///   `TargetEmulationDisabled`
    ///
    /// **M1 simplification**: All `send_input` errors are treated as transport-
    /// fatal. Protocol-level errors (e.g. `UnsupportedEvent` for clipboard) do
    /// not exist in M1; fatal/non-fatal classification will be introduced together
    /// with the reconnect trigger.
    pub(crate) async fn send(
        &self,
        event: ProtoEvent,
        handle: ClientHandle,
    ) -> Result<(), LanMouseConnectionError> {
        let event_display = format!("{event}");
        if let Some(addr) = self.client_manager.active_addr(handle) {
            let peer = {
                let peers = self.peers.lock().await;
                peers.get(&addr).cloned()
            };
            if let Some(peer) = peer {
                // The alive check has been removed (see the `send()` docstring).
                // The original guard stayed false forever because the recv_tx path
                // is not wired, blocking every send. We optimistically assume the
                // peer is online.
                //
                // Production log level is DEBUG: high-frequency events (motion fires
                // 60+ times/second) would flood at INFO. The trace-level log
                // remains available under RUST_LOG=trace for per-frame diagnosis.
                log::debug!("send to handle {handle} addr {addr} via peer (active)");
                let cfg = self
                    .client_manager
                    .input_channels(handle)
                    .unwrap_or_default();
                match peer.send_input(&event, &cfg).await {
                    Ok(()) => {
                        log::trace!("{event_display} >->->->->- {addr} (quic)");
                        return Ok(());
                    }
                    Err(e) => {
                        log::warn!("client {handle} failed to send over QUIC: {e}");
                        self.peers.lock().await.remove(&addr);
                        self.client_manager.set_active_addr(handle, None);
                        return Err(LanMouseConnectionError::Quic(e));
                    }
                }
            }
        }

        // No existing QUIC session — see whether we should trigger a dial
        // (spawn_local).
        //
        // **RetryState gate**: before dialing, check `next_attempt_at`. If we're
        // still inside the backoff window of the previous failure, return
        // `NotConnected` immediately so that every mouse event doesn't kick off a
        // wasteful `dial_any`. (M1 simplification: no full signature comparison —
        // `dial_any` under the happy-eyeballs path already has a low failure rate.)
        {
            let map = self.retry_state.borrow();
            if let Some(entry) = map.get(&handle) {
                let now = std::time::Instant::now();
                if now < entry.next_attempt_at {
                    log::trace!(
                        "client {handle} RetryState gate: waiting for backoff ({:?} remaining)",
                        entry.next_attempt_at - now
                    );
                    return Err(LanMouseConnectionError::NotConnected);
                }
            }
        }
        let mut connecting = self.connecting.lock().await;
        if !connecting.contains(&handle) {
            connecting.insert(handle);
            // The dial runs in the background. This step only covers the
            // "dial → register_peer → hello" path; the receive loop lives in
            // listen.rs and is wired in independently.
            spawn_local(connect_to_handle(
                self.client_manager.clone(),
                self.client_endpoint.clone(),
                self.quic_creds.clone(),
                self.peers.clone(),
                self.connecting.clone(),
                self.pins_dir.clone(),
                self.retry_state.clone(),
                self.recv_tx.clone(),
                handle,
                self.idle_timeout,
                self.peer_lost_tx.clone(),
            ));
        }
        Err(LanMouseConnectionError::NotConnected)
    }
}

/// Per-handle dial retry state.
///
/// **Why a free function with cloned fields (not a `&self` method)**: `send()`
/// runs `connect_to_handle` via `spawn_local`, which requires the future to be
/// `'static` — an `&self` borrow cannot survive a spawn. Every field of
/// `LanMouseConnection` is therefore cloned and passed in as parameters.
///
/// **Retry backoff gate**: a simplified version of the upstream
/// `RetryState` design:
/// - Fields: `next_attempt_at` / `backoff` / `failure_count`.
/// - No `signature` field: in M1 the candidate set is stable (no mDNS / no DNS
///   change), so the "input set changed → skip backoff" semantic is not needed.
/// - `Clone` derive lets tests take copies of entries for assertions.
///
/// **Backoff algorithm**: on failure, `backoff *= 2` with an upper cap of
/// `MAX_RETRY_BACKOFF = 8s`. The starting value is `INITIAL_RETRY_BACKOFF = 500ms`
/// (500ms → 1s → 2s → 4s → 8s cap; see the constant docstring below for the
/// Mac/wake UX rationale).
///
/// **Circuit-breaker threshold `MAX_RETRY_FAILURES_BEFORE_OFFLINE = 5`**:
/// after 5 consecutive failures, log an error indicating the peer is likely
/// offline. `TransportEvent::PeerLost` is **not** pushed to IPC. Retries do not
/// stop (transient failures still need to self-heal).
#[derive(Clone, Debug)]
struct RetryState {
    next_attempt_at: std::time::Instant,
    backoff: Duration,
    failure_count: u32,
}

/// RetryState backoff constants, tuned for Mac wake reconnect UX.
///
/// **Backoff curve**: 500ms → 1s → 2s → 4s → 4s (cap) → 4s → 4s → ...
/// repeating forever.
///
/// **Why 4s instead of the upstream 30s cap**: a 30s ceiling on the reconnect
/// interval makes the mouse-sharing UX feel sluggish — after a Mac wakes up,
/// the user has to wait up to 30s for any visible reaction. The 4s cap means
/// the user only waits up to 4s for the next retry attempt (in practice it's
/// faster: the peer wake-up itself triggers the next success). The
/// `failure_count == 5` log threshold fires earlier (around t=12s instead of
/// t=15s) which still cleanly separates "short outage" from "peer really
/// offline".
///
/// **Why 4s instead of the previous 8s (2026-09-04)**: when the controlled
/// client's network blips (Wi-Fi roam, brief switch outage, USB ethernet
/// reset), the user observes the mouse "stuck on the controlled machine" for
/// the duration of the longest retry wait. 4s halves that worst-case window
/// without meaningfully increasing dial-attempt load on a permanently-offline
/// peer (8s × N attempts and 4s × 2N attempts produce comparable background
/// traffic after the first few seconds, since both cap at their respective
/// ceiling).
// TEMPORARY: retry backoff disabled for debugging — do NOT re-enable unless
// explicitly instructed. Original values:
//   INITIAL_RETRY_BACKOFF = Duration::from_millis(500)
//   MAX_RETRY_BACKOFF     = Duration::from_secs(8)
const INITIAL_RETRY_BACKOFF: Duration = Duration::from_millis(500);
const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(4);
const MAX_RETRY_FAILURES_BEFORE_OFFLINE: u32 = 5;

/// Application-layer heartbeat cadence: the controlling side periodically sends
/// `ProtoEvent::Ping` to refresh [`crate::emulation::ListenTask`]'s
/// `last_response` map, so that a stationary mouse on either side does not trip
/// the 1s "releasing keys: ... not responding!" pseudo-timeout. QUIC's
/// `keep_alive_interval = 5s` only emits transport-layer PING frames (see
/// [`quinn::TransportConfig`]) — those do **not** produce `ListenEvent::Msg`,
/// so they cannot refresh `last_response`. An application-layer Ping is required.
///
/// 500ms sits well below the 1s threshold + 5s tick detection window, leaving
/// plenty of margin; 2 frames/s × bidirectional stream A traffic (Pong replies)
/// is negligible control-plane load.
const PING_INTERVAL: Duration = Duration::from_millis(500);

// === Pong health watchdog (master-side death detection) =====================
//
// **Why a separate watchdog**: the QUIC keepalive (5s) + idle_timeout (5s)
// stack takes ~5s on a healthy link and up to 10s on edge cases. That's the
// window during which the master keeps `State::Sending` and the user's mouse
// is "stuck on the controlled machine" while actually being captured by the
// master and dropped into a black-hole QUIC send buffer. The peer is also
// application-layer-aware — every Pong reply is a hard "alive" signal that's
// independent of the UDP-layer keepalive. Watching Pong arrivals lets us
// detect death in well under 2s.
//
// **Threshold 1.5s**: the heartbeat sends a Ping every 500ms. On a healthy
// LAN the Pong comes back in < 50ms; 1.5s is ~5× the worst realistic RTT
// (busy CPU + transient packet loss) and 3× the Ping cadence. Any value
// above `~3 × PING_INTERVAL` covers 99% of healthy networks while still
// firing within ~1s of the network actually going down. Lowering below
// `2 × PING_INTERVAL` (1s) risks false positives on a slow CPU; raising
// above `5 × PING_INTERVAL` (2.5s) loses the speed advantage over the QUIC
// idle_timeout.
//
// **Tick period 500ms**: matches the Ping cadence so the threshold check is
// never older than the previous Ping. Cheaper ticks (e.g. 100ms) wouldn't
// catch the peer any faster — the next Pong can only arrive after the next
// Ping, which is at most 500ms away.

/// How long [`pong_health_watchdog`] waits after the last Pong before
/// declaring the peer dead and force-closing the QUIC conn.
const PONG_HEALTH_TIMEOUT: Duration = Duration::from_millis(1500);

/// How often the watchdog wakes up to check `last_pong_at`. Matches
/// [`PING_INTERVAL`] so the check is never older than the previous Ping.
const PONG_HEALTH_TICK: Duration = Duration::from_millis(500);

/// Records a dial failure (`dial_any` / `client_hello` / etc.) — doubles the
/// backoff and increments `failure_count`. Emits `log::error` once
/// `MAX_RETRY_FAILURES_BEFORE_OFFLINE` is reached.
///
/// **Caller responsibility**: ensure `connecting` was inserted before calling
/// this — otherwise the `send()` path will spawn duplicate `dial_local` tasks.
/// This function only mutates `retry_state`; it does **not** touch `connecting`
/// (the caller removes it at the end of its own flow).
fn record_retry_failure(
    retry_state: &Rc<RefCell<HashMap<ClientHandle, RetryState>>>,
    handle: ClientHandle,
) {
    let mut map = retry_state.borrow_mut();
    let entry = map.entry(handle).or_insert(RetryState {
        next_attempt_at: std::time::Instant::now(),
        backoff: INITIAL_RETRY_BACKOFF,
        failure_count: 0,
    });
    let next = entry.backoff;
    entry.next_attempt_at = std::time::Instant::now() + next;
    entry.backoff = (next * 2).min(MAX_RETRY_BACKOFF);
    entry.failure_count = entry.failure_count.saturating_add(1);
    if entry.failure_count == MAX_RETRY_FAILURES_BEFORE_OFFLINE {
        log::error!(
            "client {handle} failed to dial {n} consecutive times (max 30s backoff cumulative ~63s) — peer may be truly offline \
             (circuit breaker N=5 log notify; full IPC PeerLost notification pending future implementation)",
            n = MAX_RETRY_FAILURES_BEFORE_OFFLINE
        );
    } else if entry.failure_count > MAX_RETRY_FAILURES_BEFORE_OFFLINE {
        log::debug!(
            "client {handle} cumulative failures {} times (exceeded circuit breaker threshold)",
            entry.failure_count
        );
    }
}

/// Outbound dial entry point, given a peer handle:
/// 1. Resolve candidate IP list + port.
/// 2. Call `quic_transport::dial_any(...)` for happy-eyeballs concurrent dialing
///    with a primary head-start.
/// 3. Application-layer `client_hello` handshake.
/// 4. On success: `set_active_addr` + `register_peer(addr, peer)` + remove from
///    `connecting` + clear `retry_state` + spawn `spawn_peer_supervisor`.
///
/// **Why a free function with cloned fields (not a `&self` method)**: `send()`
/// runs `connect_to_handle` via `spawn_local`, which requires the future to be
/// `'static` — an `&self` borrow cannot survive a spawn. Every field of
/// `LanMouseConnection` is therefore cloned and passed in as parameters.
///
/// **Happy-eyeballs upgrade**: concurrent dialing across all candidate addresses
/// with a 200ms primary head-start replaces single-address `dial`. The `primary`
/// is taken from `addrs.first()` — the "best IP" within the mDNS / candidate
/// list is decided by the caller (currently `HashSet` iteration order, i.e.
/// the first preferred IP when mDNS is absent); the rest are dialed in parallel.
///
/// **Supervisor on success**: spawns `spawn_peer_supervisor(peer)`. When the
/// peer dies, the supervisor decides whether to reconnect. On failure, the
/// path goes through `record_retry_failure` — doubles
/// `retry_state[handle].backoff` and increments `failure_count`.
#[allow(clippy::too_many_arguments)]
async fn connect_to_handle(
    client_manager: ClientManager,
    client_endpoint: Endpoint,
    quic_creds: Rc<QuicDialerCreds>,
    peers: Rc<Mutex<HashMap<SocketAddr, Arc<PeerSession>>>>,
    connecting: Rc<Mutex<HashSet<ClientHandle>>>,
    pins_dir: PathBuf,
    retry_state: Rc<RefCell<HashMap<ClientHandle, RetryState>>>,
    // Sender used by the stream-A forwarder task. The forwarder maps the
    // `(addr, event)` tuples read off stream A by `peer.run` into
    // `(handle, event)` and pushes them here → `LanMouseConnection::recv()`
    // → capture.rs.
    recv_tx: Sender<(ClientHandle, lan_mouse_proto::ProtoEvent)>,
    handle: ClientHandle,
    // QUIC `max_idle_timeout` passed straight to
    // [`quic_transport::dial_any`]. Frozen at supervisor startup
    // from [`crate::config::Config::quic_idle_timeout`]. Outbound
    // dials do not pick up subsequent config edits — same
    // "endpoint owns its transport config for the lifetime of the
    // process" rule that applies to the server-side listener.
    quic_idle_timeout: std::time::Duration,
    // Cloned sender for the Pong health watchdog's "peer dead" signal.
    // Multiple supervisors (one per dial) all share the same channel;
    // the receiver lives on `LanMouseConnection` and is consumed by
    // `capture.rs::do_capture_session` via [`LanMouseConnection::peer_lost`].
    peer_lost_tx: Sender<ClientHandle>,
) -> Result<(), LanMouseConnectionError> {
    log::info!("client {handle} connecting ...");
    let Some(ips_set) = client_manager.get_ips(handle) else {
        connecting.lock().await.remove(&handle);
        return Err(LanMouseConnectionError::NotConnected);
    };
    let port = client_manager.get_port(handle).unwrap_or(DEFAULT_PORT);
    // `ips_set.iter()` yields `&IpAddr`; `SocketAddr::new` takes an owned `IpAddr`,
    // so we deref with `*a`.
    let addrs: Vec<SocketAddr> = ips_set.iter().map(|a| SocketAddr::new(*a, port)).collect();

    let Some(&primary) = addrs.first() else {
        connecting.lock().await.remove(&handle);
        return Err(LanMouseConnectionError::NotConnected);
    };
    log::info!(
        "client ({handle}) dial_any ... (primary: {primary}, candidates: {})",
        addrs.len()
    );

    // TOFU pin identity. Stable across restarts and independent of which
    // candidate address wins the happy-eyeballs race — see
    // `ClientStore::peer_key`.
    let peer_key = client_manager
        .peer_key(handle)
        .unwrap_or_else(|| format!("client-{handle}"));

    let conn = match quic_transport::dial_any(
        &client_endpoint,
        primary,
        &addrs,
        quic_creds.cert_chain[0].clone(),
        quic_creds.key.clone_key(),
        &pins_dir,
        &peer_key,
        quic_idle_timeout,
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            // **Operator-facing diagnostic**: a `TimedOut` after the QUIC
            // handshake window typically means *no lan-mouse is listening on
            // the peer at all* (UDP Initial was never answered) — distinct
            // from a transport / fingerprint mismatch which surfaces as a
            // different `quinn::ConnectionError` variant. Distinguishing
            // these two failure modes is the difference between "the peer
            // daemon is down / firewall blocks UDP 4242" and "the peer's
            // cert fingerprint changed and needs re-pinning". Without this
            // hint the operator is left guessing which half of the network
            // to inspect.
            let hint = match &e {
                quic_transport::Error::Handshake(quinn::ConnectionError::TimedOut) => {
                    " (peer unreachable or not running lan-mouse — \
                     verify UDP 4242 is open and `lan-mouse` is active on the remote)"
                }
                quic_transport::Error::Handshake(quinn::ConnectionError::TransportError(_)) => {
                    " (TLS 1.3 handshake rejected — peer cert may not be in \
                     `authorized_fingerprints`, or this side's pin does not \
                     match the peer's fingerprint)"
                }
                _ => "",
            };
            log::warn!("client ({handle}) dial_any failed: {e}{hint} — **** START BACKOFF ****");
            record_retry_failure(&retry_state, handle);
            connecting.lock().await.remove(&handle);
            return Err(LanMouseConnectionError::Quic(e));
        }
    };

    let peer = Arc::new(PeerSession::from_connection(conn));
    // Application-layer Hello handshake — on failure, close the connection
    // immediately (no peer table entry to remove yet, since registration
    // hasn't happened).
    if let Err(e) = quic_transport::client_hello(&peer).await {
        log::warn!("client ({handle}) client_hello failed: {e} — **** START BACKOFF ****");
        record_retry_failure(&retry_state, handle);
        connecting.lock().await.remove(&handle);
        return Err(LanMouseConnectionError::Quic(e));
    }

    let remote = peer.connection().remote_address();
    // State-transition log (INFO): distinguish "initial dial" from
    // "successful redial". Check whether the entry exists in `retry_state`
    // before clearing it — present means a previous failure occurred and this
    // is a redial; absent means the link is being established for the first
    // time. This lets the log directly answer "did we just auto-recover from
    // a wake / network blip?", sparing readers from correlating log lines.
    let was_retry = retry_state.borrow().contains_key(&handle);
    if was_retry {
        log::info!(
            "client ({handle}) reconnected @ {remote} (quic) — \
             auto-recovered (prior disconnect handled by RetryState)"
        );
    } else {
        log::info!("client ({handle}) connected @ {remote} (quic) — first connection");
    }
    client_manager.set_active_addr(handle, Some(remote));
    peers.lock().await.insert(remote, peer.clone());
    connecting.lock().await.remove(&handle);
    // Dial succeeded → clear the retry_state entry (failure_count resets to
    // zero, mirroring the upstream `RetryState::on_success` "remove entry"
    // semantic).
    retry_state.borrow_mut().remove(&handle);

    // Set up outgoing_events + spawn a forwarder task that forwards Ack /
    // Pong / Leave events read off stream A by the `peer.run` main loop into
    // `recv_tx` → `LanMouseConnection::recv()` →
    // `capture.rs::do_capture_session()`, where the state machine transitions
    // to Sending or releases capture.
    //
    // The forwarder is needed because `peer.run` only logs incoming events at
    // debug level; without it `recv_tx` would be a dead field and capture.rs
    // would never see server responses, leaving the local state machine stuck
    // in WaitingForAck and re-sending Enter.
    //
    // **Path**:
    // 1. Create an mpsc channel of `(SocketAddr, ProtoEvent)` — `peer.run`
    //    only knows `remote_address`, not `ClientHandle`.
    // 2. Spawn the forwarder task: receive `(addr, event)` → look up the
    //    handle via `client_manager.get_client(addr)` → push to `recv_tx`
    //    (the `recv_tx` field of this `LanMouseConnection`, now actually
    //    consumed).
    // 3. `peer.set_outgoing_events(Some(tx))`.
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<(
        std::net::SocketAddr,
        lan_mouse_proto::ProtoEvent,
    )>();
    // Pong health watchdog shared state. Initialised to "now" so the
    // watchdog doesn't fire in the first 1.5s after a fresh connect
    // — the peer needs at least one round-trip (Ping → Pong) before
    // the threshold check makes sense. The forwarder updates it on
    // every Pong arrival; the watchdog reads it from
    // `pong_health_watchdog` below.
    let last_pong_at: Rc<RefCell<Instant>> = Rc::new(RefCell::new(Instant::now()));
    {
        let client_manager_for_forwarder = client_manager.clone();
        let recv_tx = recv_tx.clone();
        let last_pong_at = last_pong_at.clone();
        spawn_local(async move {
            while let Some((addr, event)) = out_rx.recv().await {
                // Pong health: refresh on every Pong arrival. The
                // Pong-frame `alive` flag is currently unused (the
                // server always responds with the current emulation
                // state), so we don't differentiate Pong(true) /
                // Pong(false) here — both are equally valid liveness
                // signals.
                if matches!(event, lan_mouse_proto::ProtoEvent::Pong(_)) {
                    *last_pong_at.borrow_mut() = Instant::now();
                }
                if let Some(handle) = client_manager_for_forwarder.get_client(addr) {
                    // Production log level is DEBUG: this is a high-frequency
                    // path (Ack / Pong / Leave all traverse it; motion does
                    // not — it goes over datagrams). INFO would flood the log.
                    log::debug!("stream A forwarder: {addr} → handle {handle}");
                    if let Err(e) = recv_tx.send((handle, event)) {
                        // A failed `recv_tx.send` means the capture task has
                        // exited → the whole link is broken.
                        log::warn!(
                            "stream A forwarder: recv_tx.send failed (capture task exited): {e}"
                        );
                        break;
                    }
                } else {
                    // Should not happen in theory — when a peer is registered
                    // in the peers table, its addr already corresponds to an
                    // active handle. If it does happen, `client_manager` was
                    // cleared externally (unregister); silently no-op.
                    log::warn!(
                        "stream A forwarder: addr {addr} not in client_manager (possibly unregistered)"
                    );
                }
            }
            // Forwarder exit means all peer.outgoing_events senders were
            // dropped — the link is fully severed.
            log::warn!("stream A forwarder: outgoing_events rx closed — forwarder exiting");
        });
    }
    peer.set_outgoing_events(Some(out_tx)).await;

    // Spawn the supervisor to take over the peer's lifecycle — when
    // `peer.run()` exits, it decides whether to trigger a RetryState reconnect.
    spawn_local(spawn_peer_supervisor(
        client_manager,
        peers.clone(),
        connecting.clone(),
        retry_state,
        client_endpoint,
        quic_creds,
        pins_dir,
        handle,
        remote,
        peer,
        quic_idle_timeout,
        peer_lost_tx,
        recv_tx.clone(),
        last_pong_at,
    ));
    Ok(())
}

/// Peer lifecycle supervisor — decides whether to reconnect when a peer dies.
///
/// Flow:
/// 1. `peer.run(PeerRole::Client).await` blocks until the peer's connection closes.
/// 2. Regardless of close type (graceful / abnormal), immediately **remove the
///    peer + clear `active_addr`** — lets `send()` follow the redial path and
///    avoids stale entries in the peer table.
/// 3. If `should_retry_after_close(reason)` is true → `record_retry_failure`
///    + spawn a fresh `connect_to_handle` task to trigger a new dial round (a
///    **new** task; we don't wait out the backoff here — the caller's `send()`
///    will be naturally gated by RetryState).
/// 4. If false → log info (graceful close) and wait for the next `send()` to
///    trigger a dial.
///
/// **M1 simplification**: the supervisor does **not** return the close reason
/// to its caller — the caller (`LanMouseConnection::send`) naturally observes
/// `peers.get(&addr) == None` and follows the redial path on its own.
#[allow(clippy::too_many_arguments)]
async fn spawn_peer_supervisor(
    client_manager: ClientManager,
    peers: Rc<Mutex<HashMap<SocketAddr, Arc<PeerSession>>>>,
    // Shared dial de-duplication set. **Must be the same `Rc` cloned from
    // `LanMouseConnection`** — see `connect_to_handle`'s spawn site. The
    // supervisor passes it to its own redial `connect_to_handle` call so
    // concurrent dials (send()/dial() retry path + supervisor-redial) are
    // collapsed by `connecting.insert(handle)` / `connecting.remove(&handle)`
    // the same way the initial dial is. Without this, the redial would
    // create a fresh empty mutex and a second `connect_to_handle` could
    // race the first — producing duplicate `PeerSession` registrations,
    // duplicate supervisors, and on the slave side a duplicate Enter that
    // drives the EnterOnly trigger → spurious Leave(0) → "mouse stuck on
    // slave after reconnect".
    connecting: Rc<Mutex<HashSet<ClientHandle>>>,
    retry_state: Rc<RefCell<HashMap<ClientHandle, RetryState>>>,
    client_endpoint: Endpoint,
    quic_creds: Rc<QuicDialerCreds>,
    pins_dir: PathBuf,
    handle: ClientHandle,
    addr: SocketAddr,
    peer: Arc<PeerSession>,
    // QUIC `max_idle_timeout` passed to the supervisor's redial
    // spawn (`connect_to_handle`). Same frozen-at-startup value as
    // on the dial side.
    idle_timeout: std::time::Duration,
    // Cloned sender for "peer dead" → capture-release. Sent by the
    // Pong health watchdog when the peer stops responding for
    // [`PONG_HEALTH_TIMEOUT`]. Forwarded to `capture.rs` via the
    // receiver half on `LanMouseConnection`.
    peer_lost_tx: Sender<ClientHandle>,
    // Stream-A forwarder channel. The redial `connect_to_handle` MUST
    // reuse the same Sender the initial dial used, otherwise its
    // forwarder would push Ack / Pong / Leave events into a throwaway
    // channel whose receiver drops on return — capture.rs would never
    // observe them and the state machine would drift (the first
    // forwarder's exit logs `recv_tx.send failed (capture task exited)`
    // with a misleading comment).
    recv_tx: Sender<(ClientHandle, lan_mouse_proto::ProtoEvent)>,
    // Shared `last_pong_at` written by the stream-A forwarder in
    // `connect_to_handle`. Read every [`PONG_HEALTH_TICK`] by the
    // watchdog below.
    last_pong_at: Rc<RefCell<Instant>>,
) {
    log::info!("spawn_peer_supervisor: starting for handle {handle} addr {addr}");

    // Application-layer heartbeat task — runs in parallel with the supervisor.
    // Every [`PING_INTERVAL`] it sends a `ProtoEvent::Ping` to the peer, whose
    // `emulation.rs::ListenTask` `ListenEvent::Msg` handler refreshes
    // `last_response`. Without this, the 1s threshold + 5s tick detection
    // window would misclassify a silent peer as "not responding" and tear down
    // the capture trigger during quiescent periods (see the PING_INTERVAL
    // docstring).
    //
    // **Lifecycle**: bound to the supervisor — spawned at supervisor entry,
    // aborted at the supervisor's tail when `peer.run` returns. If `send_input`
    // fails first, the heartbeat task exits on its own; abort provides a second
    // safety net.
    //
    // **Why not spawn it from `connect_to_handle`**: that path has no natural
    // exit trigger (`connect_to_handle` returns once). Anchoring the heartbeat
    // to `peer.run()` inside the supervisor gives the cleanest lifecycle match.
    let ping_task = spawn_local(ping_heartbeat_task(peer.clone(), addr));

    // Pong health watchdog (master-side death detection). Spawned alongside
    // the heartbeat so we close the gap where `peer.run()` hasn't yet
    // noticed the peer is dead but the application-layer Pings already
    // stopped being answered. The watchdog exits naturally on a hit
    // (returns after sending the PeerLost signal) or via the `abort()`
    // below when `peer.run()` returns first. Shared `last_pong_at` is
    // updated by the forwarder in `connect_to_handle`.
    let pong_health_task = spawn_local(pong_health_watchdog(
        peer.clone(),
        addr,
        handle,
        last_pong_at,
        PONG_HEALTH_TIMEOUT,
        peer_lost_tx.clone(),
    ));

    let close_result = peer.run(PeerRole::Client).await;
    log::info!("spawn_peer_supervisor: peer.run() returned for handle {handle} addr {addr}");

    // Peer is dead → stop the heartbeat. `send_input` will already have failed
    // and the heartbeat task exited on its own, but abort keeps the log line
    // and supervisor exit synchronized and prevents stragglers from triggering
    // a spurious `send_input` warning.
    ping_task.abort();
    // Same reasoning for the pong-health watchdog. If it already fired
    // (returned early) the abort is a no-op; if it's still waiting for the
    // next tick the abort prevents it from racing the supervisor's redial.
    pong_health_task.abort();

    // (1) Remove the peer — regardless of whether the close is graceful or
    // abnormal, let `send()` follow the redial path immediately.
    let removed = peers.lock().await.remove(&addr).is_some();
    if removed {
        log::debug!("client ({handle}) supervisor: peers table removed addr={addr}");
    }
    client_manager.set_active_addr(handle, None);

    // (2) Classify the close and decide whether to retry.
    match close_result {
        Err(quic_transport::Error::Handshake(reason)) => {
            if should_retry_after_close(&reason) {
                // State-transition log (INFO): distinguish the two close paths.
                // - `ApplicationClosed(WAKE_CLOSE_CODE)` → peer system wake
                //   (e.g. Mac), an **expected** event → INFO.
                // - Other retry-worthy reasons (e.g. TimedOut) → network
                //   anomaly → keep WARN.
                let is_wake = matches!(
                    &reason,
                    quinn::ConnectionError::ApplicationClosed(frame)
                        if frame.error_code.into_inner() as u32
                            == quic_transport::session::WAKE_CLOSE_CODE
                );
                record_retry_failure(&retry_state, handle);
                if is_wake {
                    log::info!(
                        "client ({handle}) conn {addr} wake-detected \
                         (peer system wake, expecting peer back soon) — \
                         RetryState backoff triggered — **** START BACKOFF ****"
                    );
                } else {
                    log::warn!(
                        "client ({handle}) conn {addr} closed abnormally: {reason:?} — \
                         RetryState backoff triggered — **** START BACKOFF ****"
                    );
                }
                // Trigger a new dial round (spawn_local fire-and-forget).
                // Reuse the shared `connecting` set (`LanMouseConnection`'s
                // self-connecting), the shared `recv_tx` (so the redial's
                // forwarder still publishes Ack / Pong / Leave back to
                // capture.rs), and the supervisor's own `peer_lost_tx` (so
                // the redial's watchdog can still signal capture release if
                // the new peer dies). Without sharing these, the redial
                // races the `send()`/`dial()` retry path (concurrent
                // `connect_to_handle` invocations for the same handle),
                // producing duplicate `PeerSession` registrations and
                // duplicate Enter events on the slave side — see the
                // historical "mouse stuck on slave after reconnect" bug.
                spawn_local(connect_to_handle(
                    client_manager,
                    client_endpoint,
                    quic_creds,
                    peers,
                    connecting.clone(),
                    pins_dir,
                    retry_state,
                    recv_tx.clone(),
                    handle,
                    idle_timeout,
                    peer_lost_tx.clone(),
                ));
            } else {
                log::info!(
                    "client ({handle}) conn {addr} closed gracefully: {reason:?} — no retry"
                );
            }
        }
        Err(other) => {
            log::error!(
                "client ({handle}) peer.run() returned unexpected Err: {other} — RetryState not triggered"
            );
        }
        Ok(()) => {
            // `conn.closed()` is defined by the quinn protocol layer to only
            // return Err; seeing Ok means quinn API behavior changed (or this
            // step is incomplete).
            log::error!(
                "client ({handle}) peer.run() returned Ok(()) (quinn API behavior changed? or close reason not captured)"
            );
        }
    }
}

/// Application-layer Ping heartbeat task. A workaround for the
/// "pseudo-timeout" caused by [`crate::emulation::ListenTask`]'s 1s threshold
/// + 5s tick detection window.
///
/// **Trigger**: spawned by [`spawn_peer_supervisor`] just before `peer.run()`.
/// Aborted via `ping_task.abort()` at the end of `spawn_peer_supervisor`.
///
/// **Behavior**: every [`PING_INTERVAL`], calls
/// `peer.send_input(Ping, default)`, which is routed to stream A. The peer
/// side (`emulation.rs:210`) responds with Pong, and its main loop also pushes
/// the Ping frame as `ListenEvent::Msg` into `last_response` — **that is the
/// critical bit**: `last_response` is refreshed on every Ping frame.
///
/// **Exit paths** (first to fire wins):
/// 1. Peer dies → `send_input` returns `Err` → this function `return`s.
/// 2. Supervisor tail calls `ping_task.abort()` → task is cancelled.
/// 3. (Theoretical) `peer.run()` panics → same as (1).
///
/// **Why skip the first tick**: `tokio::time::interval` fires its first tick
/// immediately at `t=0`. When the supervisor is freshly spawned the peer is
/// still in the handshake/setup phase; skipping the first tick defers "first
/// post-startup Ping" to after one full interval, avoiding frame collisions
/// with the handshake-time `Hello` stream.
async fn ping_heartbeat_task(peer: Arc<PeerSession>, addr: SocketAddr) {
    let mut interval = tokio::time::interval(PING_INTERVAL);
    // Skip the first tick — see the docstring above.
    interval.tick().await;
    loop {
        interval.tick().await;
        match peer
            .send_input(&ProtoEvent::Ping, &InputChannelConfig::default())
            .await
        {
            Ok(()) => {
                log::trace!("ping_heartbeat: sent Ping to {addr}");
            }
            Err(e) => {
                // Peer is dead — exit naturally. The supervisor will also
                // abort this task; this is the early-exit path when
                // `send_input` fails first. Warn rather than error: the peer
                // death itself will be logged by the supervisor; here we are
                // just the heartbeat thread noticing first.
                log::warn!("ping_heartbeat: send Ping to {addr} failed (peer dead): {e}");
                return;
            }
        }
    }
}

/// Pong health watchdog. Runs alongside [`ping_heartbeat_task`] inside
/// [`spawn_peer_supervisor`] and decides — purely on application-layer
/// liveness — whether the peer is dead. Fires once the gap since the
/// last Pong exceeds [`PONG_HEALTH_TIMEOUT`].
///
/// **Why this lives in the supervisor, not capture.rs**: closing the
/// QUIC conn requires a handle on the peer (`peer.connection().close(...)`),
/// which only the supervisor has. Capture only needs to know "release
/// now", which is exactly the [`ClientHandle`] the watchdog sends through
/// `peer_lost_tx`.
///
/// **Why this fires before QUIC idle_timeout**: the QUIC stack's idle
/// detection depends on transport-layer keepalive (5s) + idle_timeout
/// (5s), giving up to ~10s on a silent peer. That's the window during
/// which the master keeps the mouse captured and the user sees the
/// pointer "disappear". Pong detection collapses that window to
/// `PONG_HEALTH_TIMEOUT = 1.5s` — the master releases capture long
/// before the QUIC stack notices anything.
///
/// **Coexistence with the QUIC stack**: when the watchdog force-closes
/// the conn, `peer.run()` returns with `ConnectionError::ApplicationClosed(WAKE_CLOSE_CODE)`
/// — exactly the same reason the existing macOS wake path and the
/// server-side `last_response` timeout use. The supervisor's redial
/// branch (the `should_retry_after_close` match) already handles this
/// reason and schedules a fresh dial via RetryState, so no new logic
/// is needed on the close side.
///
/// **False positives**: 1.5s is `3 × PING_INTERVAL`, so any single
/// missed Pong triggers the watchdog. On a busy CPU this is the right
/// trade-off — the user would rather see "mouse briefly returned" than
/// "mouse stuck for 10s". False positives are bounded by the next
/// redial (≤ 500ms retry backoff + dial time, see
/// [`INITIAL_RETRY_BACKOFF`]).
///
/// **Exit paths** (first to fire wins):
/// 1. Threshold exceeded → close conn + send PeerLost + `return`.
/// 2. `pong_health_task.abort()` from the supervisor tail — the
///    peer has died from another reason (Leave, QUIC handshake
///    failure, etc.). This task drops mid-`interval.tick().await`.
/// 3. `peer.connection().close()` from the same watchdog firing in
///    duplicate — close is idempotent so a second attempt is a
///    no-op; the supervisor's abort on `peer.run() return` covers
///    cleanup.
///
/// **No tick on first iteration**: skipped via `interval.tick().await`
/// before the loop, mirroring the heartbeat task. The shared
/// `last_pong_at` is initialised to "now" in
/// [`connect_to_handle`]'s forwarder block so even if the first tick
/// fires immediately, the threshold check correctly reports
/// `elapsed() ≈ 0` rather than firing spuriously.
async fn pong_health_watchdog(
    peer: Arc<PeerSession>,
    addr: SocketAddr,
    handle: ClientHandle,
    last_pong_at: Rc<RefCell<Instant>>,
    threshold: Duration,
    peer_lost_tx: Sender<ClientHandle>,
) {
    let mut interval = tokio::time::interval(PONG_HEALTH_TICK);
    // Skip the first tick — see the docstring above.
    interval.tick().await;
    loop {
        interval.tick().await;
        let elapsed = last_pong_at.borrow().elapsed();
        if elapsed > threshold {
            log::warn!(
                "Pong health watchdog: peer {addr} hasn't responded in {elapsed:?} \
                 (> {threshold:?}) — force-closing with WAKE_CLOSE_CODE + notifying capture"
            );
            // Force-close with the wake code so the supervisor's
            // existing `should_retry_after_close` branch triggers the
            // RetryState redial — no new code path needed.
            peer.connection().close(
                quic_transport::session::WAKE_CLOSE_CODE.into(),
                b"pong_health_timeout",
            );
            // Notify capture so the master releases `State::Sending`
            // immediately rather than waiting for the user's next
            // Input event.
            if let Err(e) = peer_lost_tx.send(handle) {
                // The recv side is gone — likely the capture task has
                // exited (shutdown / panic). The peer is already
                // being closed above; just log and move on.
                log::warn!(
                    "Pong health watchdog: peer_lost_tx.send failed (capture task exited): {e}"
                );
            }
            return;
        }
    }
}

// === Unit tests ===========================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// `backoff_doubles_on_each_failure`: consecutive calls to
    /// `record_retry_failure` should accumulate backoff as
    /// `INITIAL → 2x → 4x → ...`, capping at `MAX_RETRY_BACKOFF`. The circuit
    /// breaker fires once `failure_count` reaches 5 (log only, no panic — the
    /// test does not depend on log assertions).
    ///
    /// Observed backoff sequence (INITIAL = 500ms, MAX = 8s):
    /// - 1st fail: backoff = 1s (INITIAL × 2), count = 1
    /// - 2nd fail: backoff = 2s (INITIAL × 4), count = 2
    /// - 3rd fail: backoff = 4s (INITIAL × 8), count = 3
    /// - 4th fail: backoff = 8s (INITIAL × 16 = MAX, **hits cap**), count = 4
    /// - 5th fail: backoff = 8s, count = 5 (circuit-breaker log fires)
    /// - 6th fail: backoff = 8s, count = 6
    /// - 7th fail: backoff = 8s, count = 7
    ///
    /// **No QUIC dependency** — pure RetryState data-structure unit test, runs
    /// immediately.
    #[test]
    fn backoff_doubles_on_each_failure() {
        let retry_state: Rc<RefCell<HashMap<ClientHandle, RetryState>>> = Default::default();
        let handle: ClientHandle = 42;

        // 1st fail: backoff = 2 × INITIAL = 1s; count = 1
        record_retry_failure(&retry_state, handle);
        let entry = retry_state
            .borrow()
            .get(&handle)
            .cloned()
            .expect("entry exists");
        assert_eq!(
            entry.backoff,
            INITIAL_RETRY_BACKOFF * 2,
            "1st fail: backoff = 2x INITIAL = 1s"
        );
        assert_eq!(entry.failure_count, 1);

        // 2nd fail: backoff = 4 × INITIAL = 2s; count = 2
        record_retry_failure(&retry_state, handle);
        let entry = retry_state
            .borrow()
            .get(&handle)
            .cloned()
            .expect("entry exists");
        assert_eq!(
            entry.backoff,
            INITIAL_RETRY_BACKOFF * 4,
            "2nd fail: backoff = 4x INITIAL = 2s"
        );
        assert_eq!(entry.failure_count, 2);

        // 3rd fail: backoff = 8 × INITIAL = 4s; count = 3
        record_retry_failure(&retry_state, handle);
        let entry = retry_state
            .borrow()
            .get(&handle)
            .cloned()
            .expect("entry exists");
        assert_eq!(
            entry.backoff,
            INITIAL_RETRY_BACKOFF * 8,
            "3rd fail: backoff = 8x INITIAL = 4s"
        );
        assert_eq!(entry.failure_count, 3);

        // 4th fail: backoff = 16 × INITIAL = 8s = MAX, hits cap; count = 4
        record_retry_failure(&retry_state, handle);
        let entry = retry_state
            .borrow()
            .get(&handle)
            .cloned()
            .expect("entry exists");
        assert_eq!(
            entry.backoff, MAX_RETRY_BACKOFF,
            "4th fail: backoff = 16x INITIAL = 8s = MAX, hit cap"
        );
        assert_eq!(entry.failure_count, 4);

        // 5th fail: count = 5 fires the circuit-breaker log (log only, no
        // panic); backoff unchanged.
        record_retry_failure(&retry_state, handle);
        let entry = retry_state
            .borrow()
            .get(&handle)
            .cloned()
            .expect("entry exists");
        assert_eq!(entry.backoff, MAX_RETRY_BACKOFF);
        assert_eq!(
            entry.failure_count, 5,
            "5th fail: circuit-breaker threshold reached"
        );

        // 6th fail: backoff still capped, count = 6
        record_retry_failure(&retry_state, handle);
        let entry = retry_state
            .borrow()
            .get(&handle)
            .cloned()
            .expect("entry exists");
        assert_eq!(entry.backoff, MAX_RETRY_BACKOFF);
        assert_eq!(entry.failure_count, 6);

        // 7th fail: cap holds, count = 7
        record_retry_failure(&retry_state, handle);
        let entry = retry_state
            .borrow()
            .get(&handle)
            .cloned()
            .expect("entry exists");
        assert_eq!(entry.backoff, MAX_RETRY_BACKOFF);
        assert_eq!(entry.failure_count, 7);
    }

    /// `reconnect_on_peer_close` — retry gate + clear: simulates the two
    /// lifecycle paths of RetryState:
    /// 1. Dial fails → `record_retry_failure` → entry exists with backoff
    ///    doubled.
    /// 2. Dial succeeds → `retry_state.remove(&handle)` → entry is cleared
    ///    (matches `connect_to_handle`'s `retry_state.borrow_mut().remove(&handle)`
    ///    at the end of its success path).
    ///
    /// **No QUIC dependency** — pure data-structure + decision-logic unit test,
    /// runs immediately.
    ///
    /// **Why this test does not exercise the full
    /// `peer.close → supervisor → connect_to_handle` end-to-end flow**: the
    /// full flow depends on an in-process QUIC server plus `dial_any`, which
    /// requires real mTLS. RetryState's own behavior is already covered by
    /// `backoff_doubles_on_each_failure` plus this test.
    #[test]
    fn reconnect_on_peer_close() {
        let retry_state: Rc<RefCell<HashMap<ClientHandle, RetryState>>> = Default::default();
        let handle: ClientHandle = 1;

        // (1) Simulate a dial failure (peer dead / network blip).
        record_retry_failure(&retry_state, handle);
        assert!(
            retry_state.borrow().contains_key(&handle),
            "entry should exist after a failed dial"
        );
        let entry = retry_state.borrow().get(&handle).cloned().unwrap();
        assert_eq!(entry.failure_count, 1);

        // (2) Simulate the RetryState gate taking effect — next_attempt_at > now.
        let now = std::time::Instant::now();
        assert!(
            entry.next_attempt_at > now,
            "next_attempt_at should be in the future (now={:?}, next_attempt_at={:?})",
            now,
            entry.next_attempt_at
        );

        // (3) Simulate a successful dial — connect_to_handle removes the entry
        // at the tail of its success path.
        retry_state.borrow_mut().remove(&handle);
        assert!(
            !retry_state.borrow().contains_key(&handle),
            "entry should be cleared after a successful dial (matches connect_to_handle success path)"
        );

        // (4) Simulate a "fail again → clear again" loop — verify that
        // entries can be repeatedly created and removed.
        // Note: after a successful dial clears the entry, a fresh failure
        // starts `failure_count` from 1 (the remove acts as a reset, matching
        // connect_to_handle).
        record_retry_failure(&retry_state, handle);
        record_retry_failure(&retry_state, handle);
        let entry = retry_state.borrow().get(&handle).cloned().unwrap();
        assert_eq!(
            entry.failure_count, 2,
            "after a successful dial clears the entry, a re-failure should restart count from 1 to 2"
        );
        retry_state.borrow_mut().remove(&handle);
        assert!(!retry_state.borrow().contains_key(&handle));
    }

    /// **Pong health threshold** — keeps the 1.5s default under
    /// guardrail. If a future change lowers this below `2 × PING_INTERVAL`
    /// a single missed Pong will trip the watchdog, surfacing as
    /// "mouse briefly returned during a brief pause". Going above
    /// `5 × PING_INTERVAL` loses the speed advantage over QUIC
    /// idle_timeout (5s).
    #[test]
    fn pong_health_threshold_in_safe_range() {
        const PING_INTERVAL_NS: u128 = 500_000_000; // 500ms
        const TIMEOUT_NS: u128 = 1_500_000_000; // 1.5s
        const LOWER_BOUND_NS: u128 = PING_INTERVAL_NS * 2;
        const UPPER_BOUND_NS: u128 = PING_INTERVAL_NS * 5;
        assert!(
            TIMEOUT_NS >= LOWER_BOUND_NS,
            "PONG_HEALTH_TIMEOUT ({}) should be >= 2 × PING_INTERVAL ({})",
            TIMEOUT_NS,
            LOWER_BOUND_NS
        );
        assert!(
            TIMEOUT_NS <= UPPER_BOUND_NS,
            "PONG_HEALTH_TIMEOUT ({}) should be <= 5 × PING_INTERVAL ({}) to beat QUIC idle_timeout",
            TIMEOUT_NS,
            UPPER_BOUND_NS
        );
    }
}
