//! `PeerSession` — a single QUIC session with a peer (shared by client and server).
//!
//! This module owns all per-peer state in the QUIC mid-layer:
//!
//! - [`PeerSession`] struct — holds `quinn::Connection` + hello flag +
//!   stream A cache + 3-stream bunch cache + outgoing events sender
//! - `impl PeerSession` block 1 — state / IO helpers (`from_connection` /
//!   `take_stream_a_*` / `set_stream_bunch` / `send_input` / `send_motion` /
//!   `send_stream_a` / `send_stream_b` / `send_outgoing_event`, etc.)
//! - `impl PeerSession` block 2 — `PeerSession::run()` main loop
//! - [`PeerRole`] Client / Server role enum
//! - [`should_retry_after_close`] close-reason classifier
//!
//! Relationship with [`super::protocol`]: `run()` calls
//! [`super::protocol::client_hello`], [`super::protocol::server_hello`], and
//! [`super::protocol::read_frame`]; [`super::protocol::hello_watchdog`] is
//! spawned by `run()`.
//!
//! Relationship with [`super::streams`]: `run()` spawns
//! [`super::streams::datagram_reader_task`] and [`super::streams::read_loop`];
//! the latter takes the `stream_bunch`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use quinn::{Connection as QuinnConnection, SendStream};

/// Outgoing event payload routed from `peer.run` to the local capture
/// task: `(peer socket, ProtoEvent)`. The capture task maps the socket
/// back to a `ClientHandle` via `client_manager.get_client(addr)`.
type OutgoingEvent = (std::net::SocketAddr, ProtoEvent);
type OutgoingEventSender = tokio_mpsc::UnboundedSender<OutgoingEvent>;

use tokio::sync::{Mutex, mpsc as tokio_mpsc};
use tokio::task::spawn_local;

use lan_mouse_ipc::InputChannelConfig;
use lan_mouse_proto::ProtoEvent;

use super::protocol::StreamPair;
use super::protocol::{client_hello, hello_watchdog, read_frame, server_hello};
use super::streams::{Bidi, StreamBunch, StreamEvent, datagram_reader_task, read_loop};

use lan_mouse_proto::MAX_EVENT_SIZE;

/// A single QUIC session with a peer (shared by client and server).
///
/// Fields:
/// - `conn` — `quinn::Connection`, entry point for all stream / datagram IO
/// - `hello_ok: AtomicBool` — application-layer Hello handshake completion flag
///   (set with `Ordering::Release`, loaded with `Ordering::Acquire`)
/// - `stream_a_cache: Mutex<Option<StreamPair>>` — `server_hello()` /
///   `client_hello()` caches the stream A used for Hello; later handed off
///   to the read loop
/// - `cached_send_a` / `cached_send_b` — long-lived send halves reused across
///   the peer's lifetime
/// - `outgoing_events` — optional sender that forwards stream A control events
///   out to the local capture task
/// - `stream_bunch: Arc<Mutex<Option<StreamBunch>>>` — populated by the
///   read loop when the three bidi streams are assembled; symmetric with
///   `stream_a_cache` to guard the "whole-pair take" ownership transfer.
pub struct PeerSession {
    pub(crate) conn: QuinnConnection,
    /// Application-layer Hello success flag. Initial `false`; set to `true`
    /// (`Ordering::Release`) by whichever side completes Hello first via
    /// `client_hello()` / `server_hello()`. Business code paths must
    /// `load(Ordering::Acquire)` this and confirm it is `true` before sending
    /// events.
    pub(crate) hello_ok: AtomicBool,
    /// Stream A (control stream) cache. `server_hello()` / `client_hello()`
    /// writes into it; the read loop calls `take_stream_a_recv()` to obtain
    /// the `RecvStream` half for the control-frame read loop, while the
    /// `SendStream` half is left for `send_stream_a()` to reuse.
    ///
    /// **Why `Mutex<Option<StreamPair>>` rather than `OnceCell`**: the
    /// control-frame loop needs to take the recv half while retaining the
    /// send half. `Option::take` combined with `StreamPair::recv.take()`
    /// expresses the two-step semantics cleanly; `OnceCell` cannot express
    /// "set once, but recv already taken".
    pub(crate) stream_a_cache: Mutex<Option<StreamPair>>,
    /// Cached send half of stream A. After Hello completes, the send half is
    /// moved here from `stream_a_cache.send` and reused by
    /// [`Self::send_stream_a`] — no longer opening a new bidi on every call.
    ///
    /// **Why a separate field rather than reusing `stream_a_cache`**: unlike
    /// `take_stream_a_recv` (one-shot recv take), `send_stream_a` is invoked
    /// many times across the peer's lifetime (Enter / Ack / Ping / Pong /
    /// repeated Enter on every capture re-entry...) and must reuse the same
    /// `SendStream`. Holding the lock across `write` (await inside the same
    /// `Mutex` guard) is the standard pattern for QUIC streams — there is no
    /// lock contention because this peer owns the stream exclusively.
    ///
    /// **Call order**: `client_hello` / `server_hello` completes → take
    /// `send` from `stream_a_cache.send` → store it here. The listen.rs
    /// supervisor / `peer.run` calls `take_stream_a_recv` to obtain the
    /// `recv` half (from the same `StreamPair` in `stream_a_cache`) — the
    /// client's `send_a` write and the server's `recv_a` read operate on
    /// **the same bidi**.
    pub(crate) cached_send_a: Mutex<Option<SendStream>>,
    /// Cached send half of stream B (input stream). Same pattern as
    /// [`Self::cached_send_a`].
    ///
    /// **Background**: previously, [`Self::send_stream_b`] opened a new bidi
    /// via `open_bi()` for every frame, while the server-side
    /// `listen.rs::handle_quic_peer_supervisor` only reads the cached
    /// Hello stream A recv plus datagrams — there is no `accept_bi()` loop.
    /// Each newly opened stream B therefore piles up in quinn's accept queue
    /// unconsumed. The default `InputChannelConfig { keyboard: Stream }`
    /// routes all key events through this path, producing "mouse works,
    /// keyboard does not": Motion/Button/Axis travel over datagrams with a
    /// reader, while keystrokes on stream B are dropped. The sender
    /// `send_stream_b` still returns `Ok(())` (quinn buffers small writes),
    /// so the log shows no error.
    ///
    /// **Fix (sender side)**: on the first call, `open_bi()` once and cache
    /// the send half here for long-term reuse. Subsequent calls only
    /// `write_frame` without `finish` — only one stream B exists for the
    /// peer's entire lifetime. The receiver needs a single `accept_bi()` to
    /// obtain it and keep reading (see
    /// `listen.rs::server_stream_reader_task`).
    ///
    /// **Write failure invalidates**: any write error resets this field back
    /// to `None` so the next call reopens a stream (the peer still receives
    /// the new stream through its `accept_bi` loop).
    pub(crate) cached_send_b: Mutex<Option<SendStream>>,
    /// Optional outgoing channel for stream A events.
    ///
    /// When the `run` main loop reads a control event from stream A
    /// (Ack / Pong / Leave), if this field has a sender set, it sends
    /// `(remote_addr, event)` out; the client-side `connect_to_handle` sets
    /// this before spawning `peer.run`, and spawns a forwarder task that maps
    /// `(addr, event)` through `client_manager.get_client(addr)` to
    /// `(handle, event)` and pushes it onto `recv_tx`.
    ///
    /// **Why the server side does not set this**: the server-side
    /// `listen.rs::handle_quic_peer_supervisor` does not call `peer.run`;
    /// it does its own `accept_bi` + `read_frame` + `listen_tx` push, so the
    /// forwarding path already exists.
    pub(crate) outgoing_events: Arc<Mutex<Option<OutgoingEventSender>>>,
    /// Cache for the three bidi streams. Populated by the read loop when it
    /// assembles them: server-side `accept_bi()` three times + client-side
    /// `open_bi()` three times (`client_hello` / `server_hello` already used
    /// stream A). Once populated, the whole `Some(StreamBunch)` is handed
    /// off to the read loop (recv halves go to the reader tasks, send halves
    /// are reused by `send_stream_a/b/c`).
    ///
    /// **Why `Arc<Mutex<Option<_>>>` rather than plain `Mutex<Option<_>>`**:
    /// `PeerSession` currently owns `Connection` directly (not
    /// `Arc<Connection>`), but `read_loop` needs to spawn into a separate
    /// task and still reach `stream_bunch` after taking `&self` — `Arc` lets
    /// two `PeerSession` references share the same
    /// `Mutex<Option<StreamBunch>>` without ownership-splitting problems.
    /// `stream_a_cache` differs because its ownership never crosses tasks
    /// (`client_hello` / `server_hello` fill it in a single task, and
    /// `take_stream_a_recv` reads it in a single task), whereas `stream_bunch`
    /// crosses task boundaries.
    pub(crate) stream_bunch: Arc<Mutex<Option<StreamBunch>>>,
}

/// Role identifier for `PeerSession::run()`.
///
/// **Why a role parameter is needed**: the Hello handshake is asymmetric —
/// the client side runs [`super::protocol::client_hello`] (`open_bi()` + send
/// Hello) while the server side runs [`super::protocol::server_hello`]
/// (`accept_bi()` + echo back). The three-stream assembly is also
/// asymmetric — the client calls `open_bi()` three times to obtain three
/// bidi streams, while the server calls `accept_bi()` three times to wait
/// for three bidi streams. `run()` uses [`PeerRole`] to pick the right path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerRole {
    /// Active dialer — uses [`super::protocol::client_hello`] + three
    /// `open_bi()` calls.
    Client,
    /// Passive acceptor — uses [`super::protocol::server_hello`] + three
    /// `accept_bi()` calls.
    Server,
}

/// Wake-close sentinel code (patch: macOS wake auto-reconnect support).
///
/// When macOS wakes from sleep, [`crate::macos_power::PowerObserver`] triggers
/// [`crate::listen::spawn_wake_task`] which calls
/// `connection().close(WAKE_CLOSE_CODE.into(), b"wake")` for each peer conn.
/// Using this non-zero sentinel (rather than the default `0` / `NO_ERROR`)
/// lets the peer distinguish in `should_retry_after_close` between
/// "user-initiated close (no retry)" and "system-wake-triggered close
/// (retry)".
///
/// **Why not `0`**: the old code used `0u32` (`NO_ERROR`), producing
/// `ConnectionError::ApplicationClosed(0)`, which made
/// `should_retry_after_close` always return `false`, so the master never
/// redialed. The assumption that "the next send() will trigger a redial"
/// does not hold after wake — the user is not moving the mouse.
///
/// **Why `0xCAFE`**:
/// - `0xCAFE` is the conventional sentinel magic ("coffee" pun + easy to spot)
/// - well within the QUIC `VarInt` range (≤ 2^62)
/// - the only way to avoid colliding with the `close(0u32, "peer closed stream")`
///   calls in the stream A Truncated / read IO error paths — those still use
///   code 0 (user / network-initiated disconnect) and do not enter the wake
///   retry branch.
pub(crate) const WAKE_CLOSE_CODE: u32 = 0xCAFE;

/// Classify a `quinn::ConnectionError` to decide whether this close is worth
/// auto-reconnecting.
///
/// **Decision rules**:
/// - `ApplicationClosed(_)` with reason code = [`WAKE_CLOSE_CODE`] → peer
///   closed because of a system wake event — **retry** (the user expects the
///   link to be usable immediately after wake)
/// - `ApplicationClosed(_)` with any other code, including `0` = `NO_ERROR`
///   → peer-initiated close — **do not retry** (the peer explicitly does
///   not want to continue)
/// - `ConnectionLost(_)` / `TimedOut` → network-layer disconnect — **retry**
/// - `TransportError(_)` (QUIC-level) → protocol-level error — **do not
///   retry** (likely a protocol bug or attack signal)
/// - `Reset` / `VersionMismatch` / `LocalError(_)` → local error — do not
///   retry
/// - `IdleTimeout` → QUIC idle timeout (30s of silence) — **do not retry**
///   (peer really is offline; retrying only wastes resources)
pub fn should_retry_after_close(reason: &quinn::ConnectionError) -> bool {
    use quinn::ConnectionError;
    match reason {
        // Network-layer disconnect / timeout — retry.
        ConnectionError::TimedOut => true,
        // Wake-close sentinel: peer close triggered by macOS wake (see the
        // [`WAKE_CLOSE_CODE`] docstring) — retry.
        ConnectionError::ApplicationClosed(frame)
            if frame.error_code.into_inner() as u32 == WAKE_CLOSE_CODE =>
        {
            true
        }
        // quinn 0.11 actual variants: protocol-level / local error /
        // peer-initiated close / CID exhaustion — none retry (conservative).
        ConnectionError::ApplicationClosed(_)
        | ConnectionError::TransportError(_)
        | ConnectionError::ConnectionClosed(_)
        | ConnectionError::Reset
        | ConnectionError::VersionMismatch
        | ConnectionError::LocallyClosed
        | ConnectionError::CidsExhausted => false,
    }
}

impl PeerSession {
    /// Construct: wrap a `quinn::Connection` into a `PeerSession`.
    ///
    /// All `PeerSession` construction goes through this helper, which
    /// centralizes the two invariants `hello_ok = false` and an empty
    /// `stream_a_cache`.
    pub fn from_connection(conn: QuinnConnection) -> Self {
        Self {
            conn,
            hello_ok: AtomicBool::new(false),
            stream_a_cache: Mutex::new(None),
            // `cached_send_a`: after Hello completes, the send half of the
            // Hello bidi is moved here from `stream_a_cache.send`, letting
            // [`Self::send_stream_a`] reuse the same bidi's send half (the
            // same bidi whose recv half the server-side `take_stream_a_recv`
            // will obtain) instead of opening a new bidi on every call.
            cached_send_a: Mutex::new(None),
            // stream B send half cache; lazily filled by the first
            // `send_stream_b` call (see field docstring).
            cached_send_b: Mutex::new(None),
            // stream A event outgoing channel; initial `None`. The client
            // side's `connect_to_handle` sets this via `set_outgoing_events`
            // before spawning `peer.run` (see field docstring).
            outgoing_events: Arc::new(Mutex::new(None)),
            // `stream_bunch` field placeholder — default `None`, filled when
            // the read loop assembles the three streams. The `Arc` wrapper
            // lets the read loop task and the caller (`peer.send_stream_*`)
            // share ownership of the same `Mutex<Option<StreamBunch>>`.
            stream_bunch: Arc::new(Mutex::new(None)),
        }
    }

    /// Expose the underlying `quinn::Connection` for `peer_identity()` /
    /// datagram / stream B/C access.
    pub fn connection(&self) -> &QuinnConnection {
        &self.conn
    }

    /// Whether the Hello handshake has completed.
    ///
    /// Business paths (`send_motion()` / opening stream B / event loops)
    /// must call this and confirm it is `true` before sending events;
    /// otherwise there is no application-layer-validated peer (could be
    /// LAN-spoofing residue after QUIC TLS 1.3), and injection of input is
    /// not permitted.
    #[allow(dead_code)]
    pub fn hello_ok(&self) -> bool {
        self.hello_ok.load(Ordering::Acquire)
    }

    /// Take the entire `(SendStream, RecvStream)` pair of stream A.
    ///
    /// **Consuming semantics**: after this call, the `stream_a_cache`
    /// is cleared (`Option::take`). Designed for the read loop to take
    /// ownership of the stream cached during Hello and hand it to the
    /// control-frame loop.
    ///
    /// Returns `None` if Hello has not yet completed (also valid for both
    /// the client-side `client_hello` and the server-side `server_hello`
    /// completion paths — both cache symmetrically).
    #[allow(dead_code)]
    pub async fn take_stream_a_cache(&self) -> Option<(SendStream, quinn::RecvStream)> {
        let mut g = self.stream_a_cache.lock().await;
        g.take().and_then(|p| match (p.send, p.recv) {
            (Some(s), Some(r)) => Some((s, r)),
            // Half missing (already taken) — the pair cannot be rebuilt;
            // return None.
            _ => None,
        })
    }

    /// Take the `RecvStream` half of stream A, leaving the `SendStream`
    /// half in the cache.
    ///
    /// Differs from [`Self::take_stream_a_cache`] (which takes the whole
    /// pair): this method only takes the recv half so the send half can be
    /// reused by the write path.
    #[allow(dead_code)]
    pub async fn take_stream_a_recv(&self) -> Option<quinn::RecvStream> {
        let mut g = self.stream_a_cache.lock().await;
        g.as_mut().and_then(|p| p.recv.take())
    }

    /// Take the `SendStream` half of stream A, leaving the `RecvStream`
    /// half in the cache (for `take_stream_a_recv`).
    ///
    /// Symmetric with `take_stream_a_recv`. Intended usage:
    /// - After `client_hello` / `server_hello` puts `Pair { send, recv }`
    ///   into `stream_a_cache`, this method is called to move `send` into
    ///   `cached_send_a` for `send_stream_a` to reuse.
    /// - Does not conflict with a subsequent `take_stream_a_recv` call by
    ///   the supervisor / `peer.run` (each takes its own half).
    ///
    /// **Motivation** (see the `cached_send_a` field docstring):
    /// `send_stream_a` writes one frame per call but is invoked many times
    /// across the peer's lifetime (Enter / Ack / Ping / Pong / repeated
    /// Enter ...), so it must hold the same `SendStream` to repeatedly
    /// `write`. This requires extracting the send half from `stream_a_cache`
    /// into a standalone field.
    #[allow(dead_code)]
    pub async fn take_stream_a_send(&self) -> Option<SendStream> {
        let mut g = self.stream_a_cache.lock().await;
        g.as_mut().and_then(|p| p.send.take())
    }

    /// Set the stream A event outgoing sender.
    ///
    /// `connect_to_handle` calls this before spawning `peer.run` so that the
    /// main loop, when reading Ack / Pong / Leave from stream A, can forward
    /// them out (see the field docstring). Passing `Some(_)` overwrites the
    /// previous value; `None` disables forwarding (fallback; not normally
    /// needed — the client path should keep it set).
    pub async fn set_outgoing_events(&self, tx: Option<OutgoingEventSender>) {
        *self.outgoing_events.lock().await = tx;
    }

    /// Push a `ProtoEvent` to `outgoing_events` (if set) → forwarder →
    /// capture.rs. Used by the `peer.run` main loop to push a `Leave` when
    /// it detects peer close, so the local capture releases immediately.
    ///
    /// **Why wrap rather than send directly**: centralized error swallowing
    /// plus a single log line `peer closed push Leave` so users retesting
    /// can see the full release path.
    async fn send_outgoing_event(&self, event: ProtoEvent, addr: std::net::SocketAddr) {
        if let Some(tx) = self.outgoing_events.lock().await.as_ref() {
            if let Err(e) = tx.send((addr, event)) {
                log::debug!("send_outgoing_event: outgoing_events has exited (forwarder gone): {e}");
            }
        }
    }

    /// Send a high-frequency motion input event.
    ///
    /// **Channel selection**: prefer QUIC datagram; on payload exceeding
    /// [`MAX_SAFE_DATAGRAM`] / peer not supporting datagrams / datagram send
    /// failure, fall back to stream B via [`Self::send_datagram_or_stream_b`].
    ///
    /// **Precondition**: `hello_ok == true` (the application-layer Hello
    /// handshake has completed). If `hello_ok == false`, returns
    /// [`Error::HelloFailed`] without touching datagrams / streams — this
    /// guards the trust model that "mTLS connected does not equal the peer
    /// is lan-mouse".
    #[allow(dead_code)]
    pub async fn send_motion(&self, event: &ProtoEvent) -> super::Result<()> {
        if !self.hello_ok.load(Ordering::Acquire) {
            return Err(super::Error::HelloFailed("hello not complete".into()));
        }
        // Fixed-size codec into `[u8; MAX_EVENT_SIZE]` (21 bytes) — uses the
        // same fixed-length `MAX_EVENT_SIZE` decoding path as the stream B
        // reader (`read_frame`). Datagrams carry their own length, but
        // decoding always goes through `ProtoEvent::try_from`.
        let (buf, _len): ([u8; MAX_EVENT_SIZE], usize) = (*event).into();
        self.send_datagram_or_stream_b(&buf).await
    }

    /// Datagram-first with stream B fallback.
    ///
    /// **Decision order**:
    /// 1. `conn.max_datagram_size()` is **re-read on every call**: the value
    ///    changes as path MTU probing progresses, and caching would cause
    ///    either needless fallback or oversized send failures. `None` means
    ///    the peer does not support datagrams or we have them disabled —
    ///    fall back immediately.
    /// 2. Take `min` with [`MAX_SAFE_DATAGRAM`] as the effective cap — this
    ///    prevents `max_datagram_size()` reporting a **stale** larger value
    ///    after MTU probing completes (quinn only widens to 1414 after
    ///    internal path validation, but when we see `Some(>1162)` we still
    ///    cap conservatively at 1162 to avoid `TooLarge`).
    /// 3. `conn.send_datagram(...)` — in quinn 0.11 this method is
    ///    **non-blocking** (under congestion it drops the oldest queued
    ///    datagram, which is exactly the motion semantics we want). It can
    ///    only return four errors: `TooLarge` / `Disabled` /
    ///    `UnsupportedByPeer` / `ConnectionLost`. The first three mean
    ///    "this path is not viable" — fall back to stream B; `ConnectionLost`
    ///    means the connection is dead — surface the error directly
    ///    (fallback cannot recover it; failing again on stream B is pointless).
    ///
    /// **Why the signature is `&[u8]` rather than `&ProtoEvent`**: it lets
    /// [`Self::send_stream_b`] consume the already-encoded buffer (reusing
    /// `buf` after datagram failure), and lets tests build oversized raw
    /// bytes to exercise the fallback path itself.
    ///
    /// **`bytes.to_vec().into()`**: `send_datagram` takes `bytes::Bytes`;
    /// `Vec<u8> → Bytes` is zero-copy (takes ownership of the Vec's heap
    /// allocation). No need to add a `bytes` crate dependency in the main
    /// crate — the type is reverse-inferred from the quinn 0.11 signature.
    ///
    /// The fallback path was reworked to route through [`Self::send_stream_b`]
    /// (cache + length-prefixed frames) instead of the original inline
    /// `open_uni() + write_all() + finish()` (no length prefix, no reuse).
    /// All errors funnel into [`Error::StreamB`].
    async fn send_datagram_or_stream_b(&self, bytes: &[u8]) -> super::Result<()> {
        // Re-read `max_datagram_size` on every call.
        let limit = self
            .conn
            .max_datagram_size()
            .map(|m| m.min(MAX_SAFE_DATAGRAM));

        if let Some(limit) = limit {
            if bytes.len() <= limit {
                match self.conn.send_datagram(bytes.to_vec().into()) {
                    Ok(()) => return Ok(()),
                    // Connection dead: fallback cannot recover; surface the
                    // error directly.
                    Err(e @ quinn::SendDatagramError::ConnectionLost(_)) => {
                        return Err(super::Error::Datagram(e));
                    }
                    // TooLarge / Disabled / UnsupportedByPeer: this path is
                    // not viable — fall back.
                    Err(e) => {
                        log::debug!("datagram send failed ({e}), falling back to stream B");
                    }
                }
            }
        }

        // Fallback path: route through `send_stream_b` (cache + length-prefixed
        // frames).
        self.send_stream_b(bytes).await
    }

    /// Stream B (input stream, reliable and ordered) write path.
    ///
    /// **Lazy cache**: on the first call, `conn.open_bi()` obtains a bidi
    /// stream. Subsequent calls reuse the same stream's `send` half; the
    /// recv half is left for the reader task to take over.
    ///
    /// **In-lock borrow**: the `Mutex` critical section covers the entire
    /// "open + write" span — concurrent writes on the same stream would
    /// interleave bytes and break frame boundaries.
    ///
    /// **Length-prefixed frames**: uses [`super::protocol::write_frame`]
    /// (`[u32 BE len][body...]`) aligned with the peer reader task's
    /// [`super::protocol::read_frame`] codec.
    ///
    /// **Error normalization**: all IO errors funnel into
    /// [`super::Error::StreamB(String)`] (message prefix distinguishes
    /// `"open_bi"` / `"write frame length"` / `"write frame body"`).
    pub async fn send_stream_b(&self, bytes: &[u8]) -> super::Result<()> {
        // Reuse the cached stream B send half — no longer opening a new
        // stream per frame. See [`Self::cached_send_b`] docstring.
        //
        // **Lock-held await design**: matches [`Self::send_stream_a`].
        // `send_stream_b` is stream B's only write path; concurrent callers
        // queue and serialize during the lock-held section, preventing two
        // frames from interleaving and breaking frame boundaries.
        use tokio::io::AsyncWriteExt;

        let mut g = self.cached_send_b.lock().await;
        if g.is_none() {
            let (send, recv) = self
                .conn
                .open_bi()
                .await
                .map_err(|e| super::Error::StreamB(format!("open_bi: {e}")))?;
            // Drop the recv half — stream B is one-way (we write, peer reads);
            // the reverse read capability is not needed.
            drop(recv);
            *g = Some(send);
            log::debug!("send_stream_b: created and cached stream B (subsequent frames reuse the same one)");
        }

        // On write failure, reset the cache back to `None` so the next call
        // reopens a stream (the peer's accept_bi loop will pick it up) —
        // this prevents a transient error from permanently disabling stream B.
        let result = {
            let send = g.as_mut().expect("cached_send_b was just filled");
            match send.write_u32(bytes.len() as u32).await {
                Err(e) => Err(super::Error::StreamB(format!("write frame length: {e}"))),
                Ok(()) => send
                    .write_all(bytes)
                    .await
                    .map_err(|e| super::Error::StreamB(format!("write frame body: {e}"))),
            }
        };
        if result.is_err() {
            *g = None;
        }
        result
    }

    /// Channel dispatch entry point — routes a [`ProtoEvent`] to the
    /// underlying channel indicated by the per-handle [`InputChannelConfig`].
    ///
    /// **Caller**: `src/connect.rs::LanMouseConnection::send()`.
    /// `LanMouseConnection` does not own the cfg (it lives in `ClientManager`
    /// per handle), so the caller passes the cfg through this method's
    /// signature; this method does **not** cache the cfg and does not
    /// mutate peer state.
    ///
    /// **Dispatch**:
    /// | Channel | Underlying call |
    /// |---|---|
    /// | `Datagram` | [`Self::send_motion`] (datagram-first + fallback to stream B) |
    /// | `StreamA`  | [`Self::send_stream_a`] (write cached `cached_send_a`) |
    /// | `StreamB`  | [`Self::send_stream_b`] (write cached `cached_send_b`) |
    /// | `StreamC`  | `Err(super::Error::HelloFailed("stream C is M2-only"))` |
    ///
    /// **M2 gate**: `ProtoEvent` does not include a `Clipboard` variant in
    /// the main crate, so `route_input` will never return
    /// `Channel::StreamC`. This method still explicitly handles `StreamC`
    /// by returning `Err`, guarding against an `unreachable!()` being
    /// triggered by accident when an M2 variant is added to `ProtoEvent`
    /// (compile-time + runtime double guard).
    ///
    /// **Pre-flight check**: reuses `send_motion`'s internal `hello_ok`
    /// check. `StreamA` / `StreamB` paths do not explicitly check
    /// (`hello_ok == false` makes `send_motion` return `HelloFailed`;
    /// other channels should not be called in that state — `LanMouseConnection`'s
    /// dial flow is "dial → client_hello → register_peer → subsequent send",
    /// so every peer in the peers table has already passed hello).
    #[allow(dead_code)]
    pub async fn send_input(
        &self,
        event: &ProtoEvent,
        cfg: &InputChannelConfig,
    ) -> super::Result<()> {
        use super::protocol::{Channel, route_input};
        let routed = route_input(cfg, event);
        // **INFO on Ack/Leave** — diagnostics for the "controlled side Ack
        // stuck" bug. `delivered` appears = send_input actually returned Ok;
        // no `delivered` but this log line appears = blocked in
        // `send_stream_a` waiting for the peer to consume.
        if matches!(event, ProtoEvent::Ack(_) | ProtoEvent::Leave(_)) {
            log::info!("send_input: routing {event:?} via {routed:?} (entry; awaiting send)");
        }
        let result = match routed {
            Channel::Datagram => self.send_motion(event).await,
            Channel::StreamA => {
                let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = (*event).into();
                self.send_stream_a(&buf[..len]).await
            }
            Channel::StreamB => {
                let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = (*event).into();
                self.send_stream_b(&buf[..len]).await
            }
            Channel::StreamC => Err(super::Error::HelloFailed(
                "stream C is M2-only (clipboard metadata not in M1 ProtoEvent)".into(),
            )),
        };
        if matches!(event, ProtoEvent::Ack(_) | ProtoEvent::Leave(_)) {
            log::info!(
                "send_input: {event:?} via {routed:?} returned (ok={})",
                result.is_ok()
            );
        }
        result
    }

    /// Send a control-stream event (Enter / Leave / Hello / Ping / Pong).
    ///
    /// **Why not reuse `stream_a_cache` for the recv side**:
    /// - `client_hello` / `server_hello` has already cached stream A's
    ///   send/recv halves in `peer.stream_a_cache` (the cache's intent is
    ///   to let later control frames reuse the Hello stream).
    /// - However, `LanMouseConnection` currently does **not** hold a
    ///   receiver task to read the recv half — dropping the cached recv
    ///   half is the norm, which forces `take_stream_a_recv` into the
    ///   `None` branch.
    /// - The conservative implementation reuses the send half via
    ///   `cached_send_a` (see its docstring) and falls back to opening a
    ///   new bidi if the cache is empty. The extra stream overhead from
    ///   `open_bi` on cache miss is acceptable within M1.
    ///
    /// **Future optimization**: caching + in-place writes are partially
    /// implemented (the send path uses `cached_send_a`); a lock-free
    /// write is a possible follow-up.
    ///
    /// **Error normalization**: symmetric with [`Self::send_stream_b`] —
    /// IO errors funnel into `super::Error::HelloFailed(...)` (no need
    /// to add a new `super::Error::StreamA` variant; the Hello-handshake
    /// error variant already carries the same semantics for M1: "stream A
    /// write failure" ≈ "Hello follow-up frame write failure").
    #[allow(dead_code)]
    async fn send_stream_a(&self, bytes: &[u8]) -> super::Result<()> {
        // Prefer `cached_send_a` (the send half of the same bidi cached at
        // Hello time). No longer opening a new bidi on every call — the new
        // stream would not be read by the server-side supervisor (which
        // only reads the recv half obtained via `take_stream_a_recv`, i.e.
        // the same bidi used at Hello), so control events (Enter / Ack /
        // Ping / Pong) would never reach the server, appearing as
        // "connected but keyboard / mouse not working".
        //
        // **Lock-held await design**: `send_stream_a` is stream A's only
        // write path (no other callers); concurrent callers queue and
        // serialize during the lock-held section, matching QUIC stream
        // writes' "one frame at a time" semantics.
        //
        // **Fallback**: when `cached_send_a` is `None` (Hello not done /
        // already taken), fall back to the old `open_bi` path — preserves
        // compatibility with early callers / tests (a unit test may call
        // `open_bi` + `peer.send_input` directly without going through
        // Hello).
        let mut g = self.cached_send_a.lock().await;
        if let Some(send) = g.as_mut() {
            use tokio::io::AsyncWriteExt;
            send.write_u32(bytes.len() as u32).await.map_err(|e| {
                super::Error::HelloFailed(format!("send_stream_a cached length: {e}"))
            })?;
            send.write_all(bytes).await.map_err(|e| {
                super::Error::HelloFailed(format!("send_stream_a cached body: {e}"))
            })?;
            log::trace!(
                "send_stream_a cached: wrote {} bytes on hello bidi",
                bytes.len()
            );
            return Ok(());
        }
        drop(g);

        // Fallback path — open a new bidi when `cached_send_a` is unavailable
        // (legacy behavior).
        log::debug!("send_stream_a: cached_send_a unavailable, fallback to open new bidi");
        use tokio::io::AsyncWriteExt;
        let pair = self
            .conn
            .open_bi()
            .await
            .map_err(|e| super::Error::HelloFailed(format!("send_stream_a open_bi: {e}")))?;
        let (mut send, recv) = (pair.0, pair.1);
        drop(recv); // Not reading the recv half — drop to release the reverse stream.

        send.write_u32(bytes.len() as u32)
            .await
            .map_err(|e| super::Error::HelloFailed(format!("send_stream_a length: {e}")))?;
        send.write_all(bytes)
            .await
            .map_err(|e| super::Error::HelloFailed(format!("send_stream_a body: {e}")))?;
        send.finish()
            .map_err(|e| super::Error::HelloFailed(format!("send_stream_a finish: {e}")))?;
        Ok(())
    }

    /// Take ownership of the `stream_bunch` from `PeerSession`.
    ///
    /// **Consuming semantics**: after the call, `peer.stream_bunch` is
    /// back to `None`. Designed for [`super::streams::read_loop`] to take
    /// `Some(StreamBunch)` once during reader assembly and process its
    /// `a` / `b` / `c` fields separately (a kept by the caller / b fed
    /// to the reader task / c dropped).
    ///
    /// **Returns `None`**: the caller has not yet assembled `stream_bunch`.
    ///
    /// **Visibility `pub(crate)`**: called by [`super::streams::read_loop`].
    #[allow(dead_code)]
    pub(crate) async fn take_stream_bunch(&self) -> Option<StreamBunch> {
        let mut g = self.stream_bunch.lock().await;
        g.take()
    }

    /// PeerSession assembles `stream_bunch`.
    ///
    /// **Write semantics**: before calling, `peer.stream_bunch` should be
    /// `None` (first assembly) or have been taken by
    /// [`Self::take_stream_bunch`] back to `None` (reassembly). This method
    /// overwrites directly (lock + assign `Some`) without checking "already
    /// `Some`, refuse overwrite" — the caller is responsible for the call
    /// timing.
    ///
    /// **Visibility `pub(crate)`**: only `Self::run()` calls it.
    #[allow(dead_code)]
    pub(crate) async fn set_stream_bunch(&self, bunch: StreamBunch) {
        let mut g = self.stream_bunch.lock().await;
        *g = Some(bunch);
    }

    /// `PeerSession` main loop.
    ///
    /// **Flow**:
    ///
    /// 1. **Start `hello_watchdog`** — [`super::protocol::hello_watchdog`]
    ///    is a 3-second timeout fallback (actively closes the connection
    ///    when the peer never opens stream A).
    /// 2. **Start `datagram_reader_task`** —
    ///    [`super::streams::datagram_reader_task`] is the datagram event
    ///    source (produces `StreamEvent::Datagram`).
    /// 3. **Run the Hello handshake** — client side
    ///    [`super::protocol::client_hello`] / server side
    ///    [`super::protocol::server_hello`] (chosen by `role`); on success
    ///    `peer.hello_ok() == true` and `peer.stream_a_cache` holds the
    ///    send/recv halves of stream A.
    /// 4. **Take the `stream_a_recv` half** — for the main loop's
    ///    `read_frame(recv_a)` calls.
    /// 5. **Assemble three streams** — client `open_bi()` three times /
    ///    server `accept_bi()` three times; fill `peer.stream_bunch` so
    ///    [`Self::read_loop`] can take over the reader tasks.
    /// 6. **Main `tokio::select!` loop** — merges four reader paths
    ///    (stream A recv / stream B mpsc / datagram mpsc / conn closed).
    ///    `StreamEvent`s are dispatched by category (Reliable/Datagram
    ///    would go through `route_input` cfg dispatch; Control events
    ///    are only logged at this level — business dispatch lives in the
    ///    higher-level connection wrapper).
    /// 7. **`conn.closed()` triggers exit** — the main loop exits when
    ///    the `closed` future completes. Returns
    ///    `Err(super::Error::Handshake(reason))` where `reason` is
    ///    `conn.close_reason()`; [`Self::should_retry_after_close`] is
    ///    available to the caller to decide whether to reconnect.
    ///
    /// **Why `Arc<Self>` rather than `&self`**: the internal spawned
    /// reader tasks (`datagram_reader_task` and the stream B reader
    /// inside `read_loop`) need `'static + Send` borrows — they require
    /// a `'static` lifetime (a temporary `&self` borrow cannot satisfy
    /// this). `hello_watchdog` also receives an `Arc<PeerSession>`.
    /// `Arc<Self>` lets the caller's `Arc` and `run()`'s `Arc` merge into
    /// the same reference count.
    ///
    /// **Error paths**:
    /// - `client_hello` / `server_hello` fails → return `Err` immediately
    ///   (Hello failure makes later stream A assembly meaningless)
    /// - any of the three `accept_bi()` calls fails → return
    ///   [`super::Error::HelloFailed`] (same for client-side `open_bi`)
    /// - `read_loop` fails → return [`super::Error::HelloFailed`]
    ///   (`stream_bunch` not assembled)
    /// - `StreamEvent` handling inside the main loop fails → `log::warn`
    ///   + continue (a single bad frame is not fatal; matches the
    ///   "skip-frame" semantics of the stream B reader)
    /// - `conn.closed()` → return `Err(super::Error::Handshake(reason))`
    ///   (graceful disconnect, with the close reason propagated to the
    ///   caller)
    pub async fn run(self: Arc<Self>, role: PeerRole) -> std::result::Result<(), super::Error> {
        // (1) Start `hello_watchdog` — 3s timeout fallback; actively closes
        // the connection when the peer never opens stream A.
        hello_watchdog(self.clone());

        // (2) Start `datagram_reader_task` — produces `StreamEvent::Datagram`.
        let (tx_d, mut rx_d) =
            tokio_mpsc::channel::<StreamEvent>(super::streams::READ_STREAM_BUFFER_CAP);
        spawn_local(datagram_reader_task(self.clone(), tx_d));

        // (3) Hello handshake — `role` decides whether to run
        // `client_hello` / `server_hello`.
        //
        // **Hello skip-if-already-done guard**: if `hello_ok` is already
        // `true` (set by an early `client_hello` / `server_hello` call from
        // the caller before `peer.run()` was invoked), skip the handshake
        // block entirely. Otherwise `peer.run()` would unconditionally
        // perform a second `client_hello` that opens a new bidi, waits 3s
        // for a Hello reply (which never comes — the server only
        // `accept_bi`s once during its own `server_hello`), times out, and
        // closes the connection. That produced the "hello handshake timed
        // out after 3s — RetryState not triggered" error.
        //
        // The single-test path that calls `peer.run(PeerRole::Client/Server)`
        // directly without an early hello has `hello_ok == false` and
        // therefore runs the handshake as before — test behavior unchanged.
        match role {
            PeerRole::Client => {
                if !self.hello_ok.load(Ordering::Acquire) {
                    client_hello(&self).await?;
                } else {
                    log::debug!("peer.run(Client): hello_ok already set, skipping duplicate client_hello");
                }
            }
            PeerRole::Server => {
                if !self.hello_ok.load(Ordering::Acquire) {
                    server_hello(&self).await?;
                } else {
                    log::debug!("peer.run(Server): hello_ok already set, skipping duplicate server_hello");
                }
            }
        }

        // (4) Take the stream A recv half — for the main loop's
        // `read_frame(recv_a)` calls.
        let mut recv_a = self
            .take_stream_a_recv()
            .await
            .ok_or_else(|| super::Error::HelloFailed("stream A recv missing after hello".into()))?;

        // (5) Assemble three streams (client: open_bi() / server:
        // accept_bi()) — fill `peer.stream_bunch` so `read_loop` can take
        // over the reader tasks.
        //
        // Three iterations correspond to streams A / B / C (each opened
        // once for long-term reuse). In M1 the stream C recv half is
        // immediately dropped by `read_loop`, but the stream C bidi must
        // still be opened/accepted first to acquire its ownership.
        let mut pairs = Vec::with_capacity(3);
        for i in 0..3u8 {
            let pair = match role {
                PeerRole::Client => self
                    .conn
                    .open_bi()
                    .await
                    .map_err(|e| super::Error::HelloFailed(format!("open_bi #{i}: {e}")))?,
                PeerRole::Server => self
                    .conn
                    .accept_bi()
                    .await
                    .map_err(|e| super::Error::HelloFailed(format!("accept_bi #{i}: {e}")))?,
            };
            pairs.push(pair);
        }
        // pairs[0] = stream A (keep the send half for `send_stream_a`; the recv
        //                   half was already taken by `take_stream_a_recv` —
        //                   `pair.1` is a redundant dup; safe to drop)
        // pairs[1] = stream B
        // pairs[2] = stream C (dropped immediately by `read_loop` in M1)
        let mut pairs_iter = pairs.into_iter();
        let (s_a, r_a_dup) = pairs_iter.next().expect("pairs[0]");
        let (s_b, r_b) = pairs_iter.next().expect("pairs[1]");
        let (s_c, r_c_dup) = pairs_iter.next().expect("pairs[2]");
        // Stream A's recv half was taken by `take_stream_a_recv` — `r_a_dup`
        // is a redundant dup; place it at `StreamBunch.a.recv` (read_loop
        // does not read it).
        // Stream C's recv is also not read by the M1 reader task — same
        // `r_c_dup` placeholder.
        let bunch = StreamBunch {
            a: Bidi::new(s_a, r_a_dup),
            b: Bidi::new(s_b, r_b),
            c: Bidi::new(s_c, r_c_dup),
        };
        self.set_stream_bunch(bunch).await;

        // (6) `read_loop` assembles the stream B reader task; stream C is
        // dropped inside `read_loop`.
        let mut read_streams = read_loop(&self, &mut recv_a).await?;

        // (7) Main loop `select!` — 4-way reader + conn.closed() fallback.
        let closed = self.conn.closed();
        tokio::pin!(closed);
        let mut out_event_log = 0u32; // log-only counter to avoid log spam.
        loop {
            tokio::select! {
                // Path A: stream A control plane — `run()` owns `recv_a`.
                res = read_frame(&mut recv_a) => {
                    match res {
                        Ok(event) => {
                            // Forward Control events to `outgoing_events`
                            // (set by the client-side `connect_to_handle`),
                            // so `LanMouseConnection::recv()` can receive
                            // Ack / Pong / Leave responses via `recv_tx`,
                            // letting `capture.rs` transition to Sending or
                            // release capture.
                            log::debug!("run: stream A read event: {event:?}");
                            if let Some(tx) = self.outgoing_events.lock().await.as_ref() {
                                let remote = self.conn.remote_address();
                                if let Err(e) = tx.send((remote, event)) {
                                    log::debug!(
                                        "run: outgoing_events send failed (forwarder has exited): {e}"
                                    );
                                }
                            }
                        }
                        Err(super::Error::FrameTooLarge(len)) => {
                            log::error!("run: stream A FrameTooLarge({len}) — closing");
                            return Err(super::Error::FrameTooLarge(len));
                        }
                        Err(super::Error::Truncated) => {
                            log::info!("run: stream A truncated — peer closed");
                            // Peer one-sided close: actively call
                            // `conn.close()` so quinn's `closed()` future
                            // fires immediately (by default quinn waits for a
                            // bidirectional close, falling back to a 30s idle
                            // timeout). The supervisor sees `peer.run` exit
                            // right away, clears the active addr and removes
                            // the peer, and capture releases on the next
                            // send.
                            //
                            // Also push a `Leave` to `outgoing_events` →
                            // forwarder → `capture.rs` so it calls
                            // `release_capture` immediately (without waiting
                            // for the next mouse event to trigger a send).
                            self.conn.close(0u32.into(), b"peer closed stream");
                            let remote = self.conn.remote_address();
                            self.send_outgoing_event(ProtoEvent::Leave(0), remote).await;
                            break;
                        }
                        Err(super::Error::HelloFailed(msg)) if msg.starts_with("read frame") => {
                            // IO error from `read_u32` / `read_exact` (e.g.
                            // "connection lost", "closed stream") — peer is
                            // gone / conn is dead. This is a stream-end
                            // signal, so take the same "active close + push
                            // Leave" path as Truncated to release capture
                            // immediately.
                            //
                            // Previously this was misclassified as a
                            // "decode error → skip-frame and continue"
                            // case, but read IO errors are not decode
                            // errors (the data never reaches the decoder) —
                            // the main loop would log the same error on
                            // every iteration until the 30s idle timeout
                            // finally fired `closed()`, with no capture
                            // release during that time (user-noticeable 30s
                            // delay before recovery).
                            //
                            // Mirrors the listen.rs supervisor's behavior
                            // (`Err(e) => return Err(e)`): any IO error
                            // exits immediately.
                            log::info!("run: stream A read IO error: {msg}");
                            self.conn.close(0u32.into(), b"peer read IO error");
                            let remote = self.conn.remote_address();
                            self.send_outgoing_event(ProtoEvent::Leave(0), remote).await;
                            break;
                        }
                        Err(e) => {
                            // Frame decode failed → single bad frame; skip
                            // the frame and keep reading.
                            log::warn!("run: stream A read_frame error (skip frame): {e}");
                        }
                    }
                }

                // Path B: stream B mpsc — Reliable events (keystrokes /
                // modifier state).
                evt = read_streams.b.recv() => {
                    match evt {
                        Some(StreamEvent::Reliable(event)) => {
                            log::debug!("run: stream B Reliable event: {event:?}");
                            // No business dispatch at this level (no
                            // `route_input`); the higher-level connection
                            // wrapper will dispatch by cfg to local
                            // emulation.
                        }
                        Some(other) => {
                            // The stream B reader task should only produce
                            // Reliable events; this is a defensive log
                            // (warn but do not exit — the reader task
                            // already strictly produces Reliable; this is
                            // a redundant safety net).
                            log::warn!("run: stream B produced non-Reliable event: {other:?}");
                        }
                        None => {
                            // The stream B reader task has exited (peer
                            // closed / fatal).
                            log::info!("run: stream B reader closed, exiting main loop");
                            break;
                        }
                    }
                }

                // Path D: datagram mpsc — Datagram events (high-frequency
                // pointer events).
                evt = rx_d.recv() => {
                    match evt {
                        Some(StreamEvent::Datagram(event)) => {
                            // Anti-log-spam: log every 64 frames.
                            out_event_log = out_event_log.wrapping_add(1);
                            if out_event_log % 64 == 1 {
                                log::debug!("run: datagram Datagram event (count={out_event_log}): {event:?}");
                            }
                            // No business dispatch at this level (same as
                            // stream B above).
                        }
                        Some(other) => {
                            // The datagram_reader_task should only produce
                            // Datagram events; defensive log.
                            log::warn!("run: datagram_reader produced non-Datagram event: {other:?}");
                        }
                        None => {
                            // The datagram_reader task has exited
                            // (conn.closed / read_datagram returned Err).
                            log::info!("run: datagram_reader closed, exiting main loop");
                            break;
                        }
                    }
                }

                // Path C: `conn.closed()` fallback — any source of close
                // exits the main loop.
                closed_res = &mut closed => {
                    log::info!("run: conn.closed() fired: {closed_res:?}");
                    // `closed()` firing usually means the peer has sent a
                    // close frame (bidirectional close path). Push a
                    // `Leave` so the local capture releases immediately
                    // (without waiting for the next mouse event to
                    // trigger a send).
                    let remote = self.conn.remote_address();
                    self.send_outgoing_event(ProtoEvent::Leave(0), remote).await;
                    break;
                }
            }
        }

        // (8) Exit main loop — read `conn.close_reason()` and convert it
        // into `Err(super::Error::Handshake(reason))`.
        //
        // `conn.close_reason()` is a quinn 0.11 public API: it returns
        // `Some(ConnectionError::ApplicationClosed(_))` for peer-initiated
        // close, `Some(ConnectionError::ConnectionLost(_))` / `TimedOut`
        // for network-layer disconnect, `Some(LocallyClosed)` for
        // local-initiated close, and `None` if the connection was never
        // closed. The `None` case is rare (it means the main loop broke
        // out for some other reason — e.g. a stream A/B/D anomaly); in
        // that case we synthesize `LocallyClosed` so the caller can still
        // consult `should_retry_after_close` (conservative: do not retry).
        //
        // We reuse the existing `super::Error::Handshake(ConnectionError)`
        // variant (already `#[from] quinn::ConnectionError`); no new
        // `super::Error::Closed` variant is introduced.
        log::debug!("run: main loop exited");
        let reason = self.conn.close_reason();
        let reason = reason.unwrap_or(quinn::ConnectionError::LocallyClosed);
        log::info!("peer.run({role:?}) exiting with close reason: {reason:?}");
        Err(super::Error::Handshake(reason))
    }
}

/// Conservative upper bound for a single datagram, in bytes.
///
/// The QUIC handshake initially reports a low datagram cap (1162 bytes)
/// before path MTU probing widens it. Before probing completes,
/// `max_datagram_size()` may already return this conservative value;
/// using a raw `max_datagram_size()` directly could trigger
/// `SendDatagramError::TooLarge` for that early window. After probing
/// the value can grow to ~1414, but is **not cached** — this constant
/// is the `min` bound applied as `max_datagram_size().map(|m| m.min(MAX_SAFE_DATAGRAM))`
/// to prevent any caller from bypassing the cap with a "stale larger value".
const MAX_SAFE_DATAGRAM: usize = 1162;

// Silence the unused-import warning on `Ordering`. `client_hello` /
// `server_hello` (defined in `super::protocol`) use
// `self.hello_ok.store(..., Ordering::Release)`, but this file does not
// reference `Ordering` directly.
#[allow(unused_imports)]
use std::sync::atomic::Ordering as _Ordering;

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddrV4};

    use lan_mouse_ipc::InputChannelConfig;
    use lan_mouse_proto::{MAX_EVENT_SIZE, ProtoEvent};

    use crate::quic_transport::endpoint::{accept, dial, endpoint};
    use crate::quic_transport::protocol::{client_hello, read_frame, server_hello};
    use crate::quic_transport::session::PeerRole;
    use crate::quic_transport::test_helpers::{
        endpoint_with_test_cert, ephemeral_cert, ephemeral_pins_dir, key_event, local_set_test,
        motion_event, motion_test_server,
    };

    use super::*;

    /// End-to-end `send_motion` over the datagram path: the peer receives
    /// the event via `recv_datagram` and decodes it back to the original
    /// fields.
    #[tokio::test]
    async fn motion_datagram_round_trip() {
        use crate::quic_transport::endpoint::install_crypto_provider;
        install_crypto_provider();

        let (server_cert, server_key) = ephemeral_cert();
        let (server_ep, server_addr) = motion_test_server(server_cert, server_key);

        let server_task = tokio::spawn(async move {
            let conn = tokio::time::timeout(std::time::Duration::from_secs(5), accept(&server_ep))
                .await
                .expect("server accept timeout")
                .expect("server accept");
            let session = std::sync::Arc::new(PeerSession::from_connection(conn));

            tokio::time::timeout(std::time::Duration::from_secs(5), server_hello(&session))
                .await
                .expect("server hello timeout")
                .expect("server hello");

            let datagram = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                session.connection().read_datagram(),
            )
            .await
            .expect("read_datagram timed out (datagram path failed?)")
            .expect("read_datagram");

            assert_eq!(
                datagram.len(),
                MAX_EVENT_SIZE,
                "send_motion filled the fixed-size buffer, peer should receive {MAX_EVENT_SIZE} bytes"
            );
            let buf: [u8; MAX_EVENT_SIZE] =
                datagram.as_ref().try_into().expect("datagram length should match");
            let decoded = ProtoEvent::try_from(buf).expect("datagram should decode as ProtoEvent");
            match decoded {
                ProtoEvent::Input(input_event::Event::Pointer(
                    input_event::PointerEvent::Motion { time, dx, dy },
                )) => {
                    assert_eq!(time, 4242, "Motion.time round-trip consistent");
                    assert_eq!(dx, 12.5, "Motion.dx round-trip consistent");
                    assert_eq!(dy, -7.25, "Motion.dy round-trip consistent");
                }
                other => panic!("decoded result should be Motion, actually: {other:?}"),
            }
        });

        let pins_dir = std::env::temp_dir().join(format!(
            "lan-mouse-motion-roundtrip-pins-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&pins_dir);
        let (client_cert, client_key) = ephemeral_cert();
        let client_ep = endpoint(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
            .expect("client endpoint bind");
        let conn = dial(
            &client_ep,
            server_addr,
            client_cert[0].clone(),
            client_key,
            &pins_dir,
        )
        .await
        .expect("dial");
        let client_session = PeerSession::from_connection(conn);

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client_hello(&client_session),
        )
        .await
        .expect("client hello timeout")
        .expect("client hello");

        assert!(
            client_session.connection().max_datagram_size().is_some(),
            "after handshake complete, max_datagram_size() should be Some (quinn enables datagram by default)"
        );

        client_session
            .send_motion(&motion_event())
            .await
            .expect("send_motion should succeed via datagram");

        server_task.await.expect("server task");
        drop(client_session);
        client_ep.wait_idle().await;
        let _ = std::fs::remove_dir_all(&pins_dir);
    }

    /// Acceptance test for end-to-end datagram round-trip: both ends run
    /// `Arc<PeerSession>::run(role)`, each sends one Motion frame → both
    /// `datagram_reader`s receive one frame each → both sides exit
    /// successfully.
    #[tokio::test(flavor = "multi_thread")]
    async fn peer_session_round_trip_motion_keyboard() {
        local_set_test!(peer_session_round_trip_motion_keyboard, {
            use crate::quic_transport::endpoint::install_crypto_provider;
            install_crypto_provider();

            let (server_cert, server_key) = ephemeral_cert();
            let server_ep = endpoint_with_test_cert(
                SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into(),
                server_cert,
                server_key,
            )
            .expect("server endpoint bind");
            let server_addr = server_ep.local_addr().expect("server addr");

            let server_task = tokio::task::spawn_local(async move {
                let conn =
                    tokio::time::timeout(std::time::Duration::from_secs(5), accept(&server_ep))
                        .await
                        .expect("server accept timeout")
                        .expect("server accept");
                let session = std::sync::Arc::new(PeerSession::from_connection(conn));
                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    std::sync::Arc::clone(&session).run(PeerRole::Server),
                )
                .await
                .expect("server run timeout")
                .expect("server run");
            });

            let pins_dir = std::env::temp_dir().join(format!(
                "lan-mouse-step-5-4-pins-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let _ = std::fs::remove_dir_all(&pins_dir);
            let (client_cert, client_key) = ephemeral_cert();
            let client_ep = endpoint(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
                .expect("client endpoint bind");
            let conn = dial(
                &client_ep,
                server_addr,
                client_cert[0].clone(),
                client_key,
                &pins_dir,
            )
            .await
            .expect("dial");
            let client_arc = std::sync::Arc::new(PeerSession::from_connection(conn));

            tokio::time::timeout(std::time::Duration::from_secs(5), client_hello(&client_arc))
                .await
                .expect("client_hello timeout")
                .expect("client_hello");

            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                client_arc.send_motion(&motion_event()),
            )
            .await
            .expect("client send_motion timeout")
            .expect("client send_motion");

            client_arc
                .connection()
                .close(quinn::VarInt::from(0u32), b"test done");

            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                std::sync::Arc::clone(&client_arc).run(PeerRole::Client),
            )
            .await;

            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server_task).await;

            drop(client_arc);
            client_ep.wait_idle().await;
            let _ = std::fs::remove_dir_all(&pins_dir);
        });
    }

    /// Bug #3 regression: first call `client_hello` to set `hello_ok=true`,
    /// then `peer.run(Client)` — `peer.run()` must skip its own
    /// `client_hello`.
    #[tokio::test(flavor = "multi_thread")]
    async fn peer_run_skips_hello_if_already_done() {
        local_set_test!(peer_run_skips_hello_if_already_done, {
            use crate::quic_transport::endpoint::install_crypto_provider;
            install_crypto_provider();

            let (server_cert, server_key) = ephemeral_cert();
            let server_ep = endpoint_with_test_cert(
                SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into(),
                server_cert,
                server_key,
            )
            .expect("server endpoint bind");
            let server_addr = server_ep.local_addr().expect("server addr");

            let server_task = tokio::task::spawn_local(async move {
                let conn =
                    tokio::time::timeout(std::time::Duration::from_secs(5), accept(&server_ep))
                        .await
                        .expect("server accept timeout")
                        .expect("server accept");
                let session = PeerSession::from_connection(conn);

                tokio::time::timeout(std::time::Duration::from_secs(5), server_hello(&session))
                    .await
                    .expect("server hello timeout")
                    .expect("server hello should succeed");

                let mut recv_a = session
                    .take_stream_a_recv()
                    .await
                    .expect("server stream A recv cached");
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    read_frame(&mut recv_a),
                )
                .await;

                drop(session);
            });

            let pins_dir = ephemeral_pins_dir();
            let _ = std::fs::remove_dir_all(&pins_dir);
            let (client_cert, client_key) = ephemeral_cert();
            let client_ep = endpoint(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
                .expect("client endpoint bind");
            let conn = dial(
                &client_ep,
                server_addr,
                client_cert[0].clone(),
                client_key,
                &pins_dir,
            )
            .await
            .expect("dial");
            let client_arc = std::sync::Arc::new(PeerSession::from_connection(conn));

            tokio::time::timeout(std::time::Duration::from_secs(5), client_hello(&client_arc))
                .await
                .expect("client_hello timeout")
                .expect("client_hello");
            assert!(client_arc.hello_ok(), "hello_ok should be set after client_hello");

            let client_for_run = std::sync::Arc::clone(&client_arc);
            let run_task =
                tokio::task::spawn_local(async move { client_for_run.run(PeerRole::Client).await });

            tokio::time::sleep(std::time::Duration::from_secs(1)).await;

            client_arc
                .connection()
                .close(quinn::VarInt::from(0u32), b"test done");

            let run_result = tokio::time::timeout(std::time::Duration::from_secs(2), run_task)
                .await
                .expect("peer.run did not exit within 2s")
                .expect("peer.run task did not panic");

            match run_result {
                Err(crate::quic_transport::Error::HelloTimeout(_)) => {
                    panic!("Bug #3 regression");
                }
                Err(crate::quic_transport::Error::HelloFailed(msg)) => {
                    panic!("Bug #3 regression: {msg}");
                }
                Err(crate::quic_transport::Error::Handshake(reason)) => {
                    log::debug!("peer.run exited with Handshake({reason:?})");
                }
                Err(other) => {
                    log::debug!("peer.run exited with: {other:?}");
                }
                Ok(()) => {
                    log::debug!("peer.run exited Ok");
                }
            }

            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), server_task).await;

            drop(client_arc);
            client_ep.wait_idle().await;
            let _ = std::fs::remove_dir_all(&pins_dir);
        });
    }

    /// Regression test: stream A control events are reachable end-to-end.
    #[tokio::test(flavor = "multi_thread")]
    async fn send_stream_a_round_trip_control_event() {
        local_set_test!(send_stream_a_round_trip_control_event, {
            use crate::quic_transport::endpoint::install_crypto_provider;
            install_crypto_provider();

            let (server_cert, server_key) = ephemeral_cert();
            let server_ep = endpoint_with_test_cert(
                SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into(),
                server_cert,
                server_key,
            )
            .expect("server endpoint bind");
            let server_addr = server_ep.local_addr().expect("server addr");

            let server_task = tokio::task::spawn_local(async move {
                let conn =
                    tokio::time::timeout(std::time::Duration::from_secs(5), accept(&server_ep))
                        .await
                        .expect("server accept timeout")
                        .expect("server accept");
                let session = PeerSession::from_connection(conn);

                tokio::time::timeout(std::time::Duration::from_secs(5), server_hello(&session))
                    .await
                    .expect("server hello timeout")
                    .expect("server hello should succeed");

                let mut recv_a = session
                    .take_stream_a_recv()
                    .await
                    .expect("stream_a_recv should be cached after server_hello");
                let event = tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    super::super::protocol::read_hello_frame(&mut recv_a),
                )
                .await
                .expect("server stream A read 3s timed out")
                .expect("server stream A read should succeed");

                assert!(
                    matches!(event, ProtoEvent::Ping),
                    "server should receive Ping sent by client, actually: {event:?}"
                );

                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                drop(session);
            });

            let pins_dir = ephemeral_pins_dir();
            let _ = std::fs::remove_dir_all(&pins_dir);
            let (client_cert, client_key) = ephemeral_cert();
            let client_ep = endpoint(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
                .expect("client endpoint bind");
            let conn = dial(
                &client_ep,
                server_addr,
                client_cert[0].clone(),
                client_key,
                &pins_dir,
            )
            .await
            .expect("dial");
            let client_arc = std::sync::Arc::new(PeerSession::from_connection(conn));

            tokio::time::timeout(std::time::Duration::from_secs(5), client_hello(&client_arc))
                .await
                .expect("client_hello timeout")
                .expect("client_hello");
            assert!(client_arc.hello_ok());

            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                client_arc.send_input(&ProtoEvent::Ping, &InputChannelConfig::default()),
            )
            .await
            .expect("client send_input(Ping) timed out")
            .expect("client send_input(Ping) should succeed");

            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server_task).await;

            drop(client_arc);
            client_ep.wait_idle().await;
            let _ = std::fs::remove_dir_all(&pins_dir);
        });
    }

    // suppress unused warnings on helpers imported for the tests
    #[allow(dead_code)]
    fn _unused() {
        let _ = key_event();
    }
}
