//! Stream-task assembly layer.
//!
//! This module orchestrates the read tasks for the three QUIC streams (A / B / C)
//! and the datagram channel:
//!
//! - [`Bidi`] / [`StreamBunch`] — ownership wrappers for the three streams
//! - [`StreamEvent`] — mpsc queue event type (distinguishes Control / Reliable / Datagram)
//! - [`ReadStreams`] — return value of `read_loop` (stream B receiver + reader task handle)
//! - [`read_stream_b_loop`] — stream B reader task (Reliable uses a blocking sender for backpressure)
//! - [`read_loop`] — wires up the three streams + spawns the stream B reader
//! - [`datagram_reader_task`] — datagram event loop (drop-oldest backpressure)
//! - [`READ_STREAM_BUFFER_CAP`] — mpsc capacity = 64
//!
//! Relationship with [`super::protocol`]: the stream B reader calls
//! `protocol::read_frame` to decode.
//! Relationship with [`super::session`]: both [`datagram_reader_task`] and
//! [`read_loop`] consume `Arc<PeerSession>` (these two tasks are spawned inside
//! `peer.run`).

use std::sync::Arc;

use quinn::{RecvStream, SendStream};
use tokio::io::AsyncRead;
use tokio::sync::mpsc as tokio_mpsc;
use tokio::task::{JoinHandle, spawn_local};

use lan_mouse_proto::ProtoEvent;

use super::Error;
use super::protocol::read_frame;
use super::session::PeerSession;

/// mpsc channel capacity used by the stream B reader task.
///
/// The capacity is `64` — enough to buffer ~50ms @ 1000Hz bursts of
/// high-frequency input, while not wasting memory (each `StreamEvent`
/// is < 256B → 64 entries < 16KB).
///
/// **Backpressure strategy** (SUGGESTION #28 governance):
///
/// | Event category | Origin | Policy when the queue is full |
/// |---|---|---|
/// | **Control** (Enter / Leave / Ack / Hello / Ping / Pong on Stream A) | Stream A | **Block the sender** (Stream A reader task is managed by the listen.rs supervisor itself; not implemented here) |
/// | **Input Reliable** (Key / Button / Modifiers on Stream B, when the channel is configured as Stream) | Stream B | **Block the sender** (`tx.send().await`) — mouse and keyboard button events must not be dropped |
/// | **Input Datagram** (Motion / Axis / AxisDiscrete120 and other high-frequency events) | Datagram | **Drop-oldest** (when the queue is full, `try_recv` the oldest frame and drop it, then `try_send` the new frame) |
///
/// Reliable uses a blocking sender and Datagram uses drop-oldest.
/// **Control is managed by the caller (listen.rs supervisor)** — it holds
/// `recv_a` and reads `read_frame` inside `select!`, which naturally blocks,
/// effectively pushing the backpressure back to the peer.
pub(crate) const READ_STREAM_BUFFER_CAP: usize = 64;

/// Event type sent by the read tasks into the mpsc queue.
///
/// **Why an enum** (rather than a bare `ProtoEvent`):
/// `PeerSession::run()` uses `tokio::select!` to merge four readers
/// (datagram / stream A / stream B / stream C). The enum distinguishes
/// "this is a control-plane event" from "this needs IPC push / dispatch".
/// In M1, control-plane events (Enter / Leave / Ack / Hello / Ping / Pong)
/// do **not** enter IPC ([`lan_mouse_ipc::TransportEvent`] is M2); the enum
/// lets `run()` dispatch by category — `Control` writes back hello_ok /
/// channel config / logs, `Reliable` routes through `route_input` to local
/// emulation, and `Datagram` is forwarded directly.
///
/// **The three variants** (PLAN §5.3 dispatch table):
/// - **`Control(ProtoEvent)`** — control frames read from Stream A
///   (Enter / Leave / Ack / Hello / Ping / Pong / Hello echo, etc.)
/// - **`Reliable(ProtoEvent)`** — reliable input events read from Stream B
///   (mouse buttons / keyboard keys / keyboard modifiers; routed to Stream B
///   when `route_input` is configured with `ChannelMode::Stream`)
/// - **`Datagram(ProtoEvent)`** — events read from the QUIC datagram channel
///   (Motion / Axis / AxisDiscrete120, plus Button/Key/Modifiers when configured
///   for the Datagram channel). The variant is reserved up front so that the
///   `match` in `run()` is exhaustive before the datagram reader is wired up.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Control frame read from Stream A.
    Control(ProtoEvent),
    /// Reliable input event read from Stream B (button / modifier).
    Reliable(ProtoEvent),
    /// High-frequency event read from the QUIC datagram channel.
    Datagram(ProtoEvent),
}

/// Ownership wrapper for a single bidirectional stream.
///
/// **Rationale for the abstraction**: `SendStream` / `RecvStream` come from
/// quinn 0.11, and a single bidi stream always yields a paired result
/// (`open_bi() -> (SendStream, RecvStream)`). Bundling them into a
/// `Bidi<S, R>` type lets the upper layers (`StreamBunch` /
/// `PeerSession.stream_bunch`) take the whole pair at once, so stream-level
/// lifecycle management lives in one place.
///
/// **Why generic `S: AsyncRead + AsyncWrite + Unpin` instead of a fixed
/// `SendStream`**: tests (e.g. `frame_round_trip`, which exercises the codec
/// round-trip with a mock stream) and the production path (a real quinn
/// stream) share the same `write_frame` / `read_frame` codec.
/// `SendStream` already implements `AsyncRead` + `AsyncWrite` + `Unpin`, so
/// the generic bound does not restrict the production path.
///
/// **Lifetime / Send boundary**: `Bidi<SendStream>` is not shared across
/// `await` in the main crate (the `Arc<tokio::sync::Mutex<Option<...>>>`
/// around `PeerSession.stream_bunch` already guards it). The generic `S`
/// lets callers use local types like `tokio::io::DuplexStream` or `Vec<u8>`
/// in tests, which keeps tests flexible.
///
/// Aligned in shape with `StreamPair` from the bak
/// `mousehop/src/quic_transport.rs` (same semantics — a send / recv pair),
/// but the type abstraction here is lighter: the bak `StreamPair` wraps
/// `SendStream` in `Option<...>` to support "take the recv half" semantics,
/// whereas `Bidi` here holds a bare `S` (the recv-half take is managed by
/// the `StreamBunch` + `PeerSession.stream_bunch` pair).
#[allow(dead_code)]
pub struct Bidi<S, R = S>
where
    S: tokio::io::AsyncWrite + Unpin,
    R: tokio::io::AsyncRead + Unpin,
{
    pub send: S,
    pub recv: R,
}

impl<S, R> Bidi<S, R>
where
    S: tokio::io::AsyncWrite + Unpin,
    R: tokio::io::AsyncRead + Unpin,
{
    /// Constructor: wraps the `(SendStream, RecvStream)` pair returned by
    /// quinn's `open_bi()` / `accept_bi()` into a `Bidi`. In production
    /// `S = SendStream` and `R = RecvStream`; tests may pass a
    /// `tokio::io::DuplexStream` (same type, 2-arg default).
    pub fn new(send: S, recv: R) -> Self {
        Self { send, recv }
    }
}

/// Ownership bundle for the three bidirectional streams.
///
/// **`a`** — the control stream (Hello / Enter / Leave / Ack / Ping / Pong).
/// During the Hello phase, `client_hello()` / `server_hello()` cache the
/// pair here; `read_loop` later takes ownership through
/// `PeerSession.stream_bunch`.
///
/// **`b`** — the input stream (mouse buttons / keyboard keys / keyboard
/// modifiers, routed by `route_input`). Reused by the `send_motion` fallback
/// path, which upgraded the inline uni stream to a bidi cache + length prefix.
///
/// **`c`** — the clipboard meta stream (reserved for M2). The field is
/// present but no reader task is started — per PLAN §9, the M1 boundary
/// forbids opening a Stream C reader task.
#[allow(dead_code)]
pub struct StreamBunch {
    /// Stream A (control, reliable, in-order).
    pub a: Bidi<SendStream, RecvStream>,
    /// Stream B (input, reliable, in-order).
    pub b: Bidi<SendStream, RecvStream>,
    /// Stream C (clipboard meta, reserved for M2; no reader task in M1).
    pub c: Bidi<SendStream, RecvStream>,
}

/// Return value of [`PeerSession::read_loop`] — consumed by the `select!`
/// main loop in `PeerSession::run()`.
///
/// **Field semantics**:
/// - **`b`** — mpsc `Receiver` for events read from Stream B. Reliable input
///   events (keys / modifiers) flow through this receiver to the upper
///   emulation / dispatch layer when `route_input` is configured with
///   `ChannelMode::Stream`.
/// - **`join_b`** — `JoinHandle<Result<(), Error>>` for the Stream B reader
///   task. The caller may `.await` it to observe the reader task exiting.
///   Awaiting is not required here (the reader task runs in parallel with
///   the `select!` main loop).
///
/// **Why Stream A is not in this struct**: the caller already holds
/// `recv_a` (a `read_loop` parameter), and simply calls
/// `read_frame(&mut recv_a)` from the listen.rs supervisor's `select!`.
/// There is no need for `read_loop` to wrap it in another mpsc layer.
///
/// **Why Stream C is not in this struct**: `read_loop` immediately drops
/// the Stream C `RecvStream` internally (honoring the PLAN §9 M1 boundary)
/// — no reader task is started, so nothing is returned to the caller.
///
/// **`Clone` is not implemented**: `tokio::sync::mpsc::Receiver` cannot be
/// cloned (the semantics forbid it — there must be a single consumer).
///
/// **`Debug` is not implemented**: `ReadStreams` currently only contains
/// a `Receiver` and a `JoinHandle`, both of which already implement
/// `Debug`, so a derive would work; if fields like `RecvStream` are added
/// later, a manual `impl` will be required.
pub struct ReadStreams {
    /// mpsc receiver for events read from Stream B (Reliable category).
    pub b: tokio_mpsc::Receiver<StreamEvent>,
    /// `JoinHandle` for the Stream B reader task.
    pub join_b: JoinHandle<std::result::Result<(), Error>>,
}

/// Stream B reader task.
///
/// **Responsibilities**: read `stream_bunch.b.recv` in a loop, call
/// `read_frame`, decode into a `ProtoEvent`, wrap it as
/// `StreamEvent::Reliable(...)`, and send it through the mpsc queue via
/// `tx.send().await`.
///
/// **Three categories of error handling**:
/// - `Error::FrameTooLarge(len)` → fatal: an attacker-controlled length
///   field or a corrupted wire. The task is unrecoverable; it returns
///   `Err` so the caller's `join_b` observes the failure.
/// - `Error::HelloFailed(msg)` when `msg.starts_with("decode frame")` →
///   codec decode failure (a single corrupted frame): log a `warn!` and
///   skip the current frame, continuing the loop without exiting the task.
/// - Other IO errors (peer close / reset / `Error::Truncated`) → the task
///   exits and returns `Err`.
///
/// **Backpressure**: `tx.send(event).await` blocks until the receiver is
/// ready — when the upper `select!` loop is slow or the receiver doesn't
/// drain in time, the reader awaits at `send`, pushing backpressure all
/// the way to stream B's quinn flow control. This is the concrete
/// implementation of "block the sender for control / input reliable"
/// (SUGGESTION #28).
///
/// **Receiver-drop exit**: when the caller drops `ReadStreams.b`
/// (the receiver), `tx.send().await` returns `Err(SendError)`, the task
/// exits cleanly and returns `Ok(())` (treated as a graceful shutdown).
#[allow(dead_code)]
async fn read_stream_b_loop<R>(
    mut recv: R,
    tx: tokio_mpsc::Sender<StreamEvent>,
) -> std::result::Result<(), Error>
where
    R: AsyncRead + Unpin,
{
    loop {
        match read_frame(&mut recv).await {
            Ok(event) => {
                // Reliable send blocks — backpressure: slow caller -> slow reader.
                if tx.send(StreamEvent::Reliable(event)).await.is_err() {
                    // Receiver was dropped (caller shut down read_loop); exit cleanly.
                    log::info!("stream B reader: receiver dropped, exiting cleanly");
                    return Ok(());
                }
            }
            Err(Error::FrameTooLarge(len)) => {
                log::error!("stream B: FrameTooLarge({len}) — fatal, closing task");
                return Err(Error::FrameTooLarge(len));
            }
            Err(Error::HelloFailed(msg)) if msg.starts_with("decode frame") => {
                log::warn!("stream B: skip frame (decode error): {msg}");
                continue;
            }
            Err(e) => {
                log::info!("stream B reader exiting (IO closed): {e}");
                return Err(e);
            }
        }
    }
}

/// `PeerSession::read_loop` — wires up the readers for the three streams.
///
/// **Responsibilities**: spawn one independent reader task (stream B).
/// Stream A is held by the caller (borrowed via `&mut RecvStream`), and
/// stream C is dropped immediately (honoring the §9 M1 boundary).
/// Returns [`ReadStreams`] to the `select!` main loop in
/// `PeerSession::run()`.
///
/// **Flow**:
/// 1. **Take ownership of `stream_bunch`** (`Option::take()` consumes the
///    `Some(...)`); the caller has already populated the `StreamBunch`.
/// 2. **Stream A is held by the caller** — `recv_a: &mut RecvStream` is a
///    parameter borrow, so no reader is spawned inside `read_loop`. The
///    caller (the listen.rs supervisor) reads `read_frame(recv_a)` from
///    its own `select!`.
/// 3. **Stream B**: `tx_b = mpsc::channel(READ_STREAM_BUFFER_CAP)`,
///    `spawn_local(read_stream_b_loop(stream_bunch.b.recv, tx_b))` returns
///    `JoinHandle<Result<(), Error>>`.
/// 4. **Stream C**: `drop(stream_bunch.c)` triggers a graceful quinn
///    shutdown (honoring §9 M1 — no reader task).
/// 5. **Return** [`ReadStreams { b: rx_b, join_b }`].
///
/// **Why Stream A is held by the caller** (rather than spawned inside
/// `read_loop`):
/// - The listen.rs supervisor's `select!` already holds `recv_a` (from
///   `server_hello`'s `take_stream_a_recv()`); there's no need for
///   `read_loop` to add another mpsc layer.
/// - One fewer task spawn and one fewer mpsc channel → lower end-to-end
///   latency.
/// - Stream A is the control stream, with no symmetric "join" requirement
///   (it lives with the supervisor for the entire session).
///
/// **Why Stream C is dropped immediately**: PLAN §9 (M1 boundary) forbids
/// opening a Stream C reader task. Stream C is reserved for M2 clipboard
/// metadata. Taking the `RecvStream` ownership and dropping it immediately
/// lets quinn send FIN / STOP_SENDING to the peer so that the peer's
/// write half on stream C is not left blocked.
///
/// **Loop backpressure**: Stream B mpsc capacity is
/// [`READ_STREAM_BUFFER_CAP`] = 64; the blocking sender implements
/// backpressure for reliable input events (see that constant's docs).
///
/// **`stream_bunch` ownership semantics**: calling
/// [`PeerSession::take_stream_bunch`] extracts the `StreamBunch` from the
/// `Option<StreamBunch>` and leaves `peer.stream_bunch` as `None`.
///
/// **Error path**: this function does not actively return `Err` (the
/// assembly itself cannot fail). If assembly fails (e.g. `stream_bunch`
/// was never set), it returns [`Error::HelloFailed`] with the message
/// `"stream_bunch not initialized"` for the caller to handle.
///
/// **`bunch.a` handling**: `stream_bunch.a` (the cached `Bidi<SendStream>`
/// for stream A) is dropped together with the bunch on move. This is
/// harmless: the caller already took the recv half via
/// `take_stream_a_recv`, and the `take_stream_bunch` here consumes the
/// remaining half.
#[allow(dead_code)]
#[allow(unused_variables)] // recv_a is reserved for a future stream A reader integration
pub async fn read_loop(
    peer: &PeerSession,
    recv_a: &mut RecvStream,
) -> std::result::Result<ReadStreams, Error> {
    // (1) Take ownership of stream_bunch — single take, leaves the field as None.
    let bunch = peer
        .take_stream_bunch()
        .await
        .ok_or_else(|| Error::HelloFailed("stream_bunch not initialized".into()))?;

    // (2) Stream B assembly: mpsc + reader task.
    let (tx_b, rx_b) = tokio_mpsc::channel::<StreamEvent>(READ_STREAM_BUFFER_CAP);
    let join_b = spawn_local(read_stream_b_loop(bunch.b.recv, tx_b));

    // (3) Stream A is held by the caller (parameter borrow), no internal spawn.
    //     Reduces task count and removes an mpsc layer.

    // (4) Stream C: dropped immediately — honors the PLAN §9 M1 boundary.
    drop(bunch.c);

    // (5) `bunch.a` (Stream A's cached `Bidi<SendStream>`) is dropped
    //     automatically at the end of the bunch move. Harmless: the caller
    //     already took the recv half via `take_stream_a_recv`, and the
    //     `bunch.a.send` half is released as the bunch drops.

    log::info!(
        "read_loop: stream B reader spawned (cap={READ_STREAM_BUFFER_CAP}), \
         stream C dropped (M1 §9 boundary)"
    );

    Ok(ReadStreams { b: rx_b, join_b })
}

/// Datagram event reader task.
///
/// **Responsibilities**: loop on `read_datagram()`, parse into a
/// `ProtoEvent` (fixed-size codec), wrap it as `StreamEvent::Datagram`,
/// and forward it through the mpsc queue to the main loop.
///
/// **Backpressure strategy (SUGGESTION #S-16) — drop the oldest frame**:
///
/// When the queue is full, `tx.try_send` fails. The current M1
/// implementation drops the incoming frame instead — high-frequency pointer
/// increments are imperceptible when a single frame is lost, in contrast
/// with stream B's "button events must not be dropped" requirement
/// (SUGGESTION #28's two-path design).
///
/// **Why Motion / Axis / AxisDiscrete120 use drop-oldest**: a single dropped
/// high-frequency pointer increment is not user-visible (contrast with
/// stream B's "buttons must not be dropped").
///
/// **Task-exit conditions**:
/// - `read_datagram` returns `Err` (peer closed / connection dead) → exit.
/// - mpsc `tx` is dropped (the main loop exits, `rx_d` is dropped) →
///   `Closed(_)` variant on `try_send` → exit (treated as a clean shutdown).
/// - Parse failure (`ProtoEvent::try_from`) → `log::warn` + continue (a
///   single corrupted frame is not fatal; symmetric with stream B's
///   skip-frame semantics).
///
/// **Visibility `pub(crate)`**: this function is consumed by
/// [`super::session::PeerSession::run`] (becoming `'static` after spawn).
/// `pub(crate)` lets `session.rs` spawn it.
pub(crate) async fn datagram_reader_task(
    peer: Arc<PeerSession>,
    tx: tokio_mpsc::Sender<StreamEvent>,
) {
    loop {
        match peer.conn.read_datagram().await {
            Ok(bytes) => {
                // Fixed-size codec: ProtoEvent::try_from consumes [u8; MAX_EVENT_SIZE];
                // bytes.len() must equal MAX_EVENT_SIZE.
                let buf: [u8; lan_mouse_proto::MAX_EVENT_SIZE] = match bytes.as_ref().try_into() {
                    Ok(b) => b,
                    Err(_) => {
                        log::warn!(
                            "datagram_reader: datagram length != MAX_EVENT_SIZE ({}), skip frame",
                            lan_mouse_proto::MAX_EVENT_SIZE
                        );
                        continue;
                    }
                };
                let event = match ProtoEvent::try_from(buf) {
                    Ok(e) => e,
                    Err(e) => {
                        log::warn!("datagram_reader: ProtoEvent decode failed, skip frame: {e}");
                        continue;
                    }
                };

                // SUGGESTION #S-16 backpressure: queue full -> drop the current frame.
                //
                // tokio mpsc Sender cannot drain from the send side; true drop-oldest
                // semantics must live on the Receiver side. The simplified M1 policy
                // drops the incoming frame when the caller is slow, trading off user-
                // noticeable drops of high-frequency Motion events for simplicity.
                match tx.try_send(StreamEvent::Datagram(event)) {
                    Ok(()) => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        // Queue full -> drop the current frame (high-frequency pointer
                        // events; a single lost frame is imperceptible).
                        log::trace!("datagram_reader: queue full, dropping current frame");
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                        // Main loop has exited (rx_d was dropped); exit cleanly.
                        log::info!("datagram_reader: mpsc receiver dropped, exiting");
                        return;
                    }
                }
            }
            Err(e) => {
                // Peer closed / connection dead — exit the task.
                log::info!("datagram_reader: read_datagram error, exiting: {e}");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddrV4};

    use tokio::io::AsyncWriteExt;

    use lan_mouse_proto::ProtoEvent;

    use crate::quic_transport::endpoint::{accept, dial, endpoint};
    use crate::quic_transport::session::PeerSession;
    use crate::quic_transport::test_helpers::{
        ephemeral_cert, key_event, local_set_test, motion_event, motion_test_server,
    };

    use super::*;

    /// Codec round-trip ——
    /// `write_frame(send, &event)` → `read_frame(&mut recv)` recovers the same event.
    #[tokio::test]
    async fn frame_round_trip() {
        let (mut write_half, mut read_half) = tokio::io::duplex(4096);

        let events = vec![
            ProtoEvent::Ping,
            ProtoEvent::hello([0xab; 8]),
            motion_event(),
        ];
        let events_clone = events.clone();
        let writer = tokio::spawn(async move {
            for event in &events_clone {
                super::super::protocol::write_frame(&mut write_half, event)
                    .await
                    .expect("write_frame should succeed");
            }
        });

        for expected in &events {
            let got = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                super::super::protocol::read_frame(&mut read_half),
            )
            .await
            .expect("read_frame timeout")
            .expect("read_frame should succeed");
            let expected_dbg = format!("{expected:?}");
            let got_dbg = format!("{got:?}");
            assert_eq!(
                got_dbg, expected_dbg,
                "events should match after codec round-trip: expected {expected_dbg}, got {got_dbg}"
            );
        }

        writer.await.expect("writer task");
    }

    /// Body truncation — `read_frame` must return
    /// [`super::super::Error::Truncated`].
    #[tokio::test]
    async fn frame_truncated_rejected() {
        let (mut write_half, mut read_half) = tokio::io::duplex(4096);

        let writer = tokio::spawn(async move {
            write_half.write_u32(17).await.expect("write length prefix");
            write_half
                .write_all(&[0u8; 8])
                .await
                .expect("write truncated body");
            drop(write_half);
        });

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            super::super::protocol::read_frame(&mut read_half),
        )
        .await
        .expect("read_frame should not hit the overall timeout");

        match result {
            Err(crate::quic_transport::Error::Truncated) => {}
            Err(other) => panic!("error should be Error::Truncated, got: {other:?}"),
            Ok(event) => {
                panic!("read_frame on a truncated frame should not succeed, decoded: {event:?}")
            }
        }

        writer.await.expect("writer task");
    }

    /// Stream B reader task + mpsc queue round-trip.
    #[tokio::test]
    async fn stream_frame_round_trip() {
        let (mut write_half, read_half) = tokio::io::duplex(4096);

        let (tx, mut rx) = tokio_mpsc::channel::<StreamEvent>(READ_STREAM_BUFFER_CAP);
        let join_b = tokio::spawn(read_stream_b_loop(read_half, tx));

        let event = key_event();
        let event_dbg = format!("{event:?}");
        super::super::protocol::write_frame(&mut write_half, &event)
            .await
            .expect("write_frame should succeed");

        let received = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("mpsc recv timed out")
            .expect("mpsc recv q succeeded");

        match received {
            StreamEvent::Reliable(got) => {
                let got_dbg = format!("{got:?}");
                assert_eq!(
                    got_dbg, event_dbg,
                    "stream B reader should forward the same event written via write_frame"
                );
            }
            other => panic!("event category should be Reliable, got: {other:?}"),
        }

        drop(write_half);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), join_b).await;
    }

    /// Stream B reader task's backpressure semantics: when the receiver is idle,
    /// the sender must block rather than drop frames.
    #[tokio::test]
    async fn streams_backpressure_blocks_when_receiver_idle() {
        let (mut write_half, read_half) = tokio::io::duplex(4096);

        let (tx, mut rx) = tokio_mpsc::channel::<StreamEvent>(2);
        let join_b = tokio::spawn(read_stream_b_loop(read_half, tx));

        let events: Vec<ProtoEvent> = (0..5).map(|_| key_event()).collect();
        let events_dbg: Vec<String> = events.iter().map(|e| format!("{e:?}")).collect();
        for event in &events {
            super::super::protocol::write_frame(&mut write_half, event)
                .await
                .expect("write_frame should succeed");
        }

        let mut got: Vec<String> = Vec::with_capacity(events.len());
        for _ in 0..events.len() {
            let received = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("drain timed out")
                .expect("drain recv q succeeded");
            match received {
                StreamEvent::Reliable(got_event) => {
                    got.push(format!("{got_event:?}"));
                }
                other => panic!("event category should be Reliable, got: {other:?}"),
            }
        }

        assert_eq!(
            got, events_dbg,
            "after 5 frames round-trip, order and content should match \
             (backpressure = blocking sender, no frames lost)"
        );

        drop(write_half);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), join_b).await;
    }

    /// Stream C handling — honors the §9 M1 boundary (no reader task).
    #[tokio::test(flavor = "multi_thread")]
    async fn stream_c_take_releases_quinn_recv_stream() {
        local_set_test!(stream_c_take_releases_quinn_recv_stream, {
            use crate::quic_transport::endpoint::install_crypto_provider;
            use crate::quic_transport::protocol::{client_hello, server_hello};
            install_crypto_provider();

            let (server_cert, server_key) = ephemeral_cert();
            let (server_ep, server_addr) = motion_test_server(server_cert, server_key);

            let server_session_fut = tokio::task::spawn_local(async move {
                let conn =
                    tokio::time::timeout(std::time::Duration::from_secs(5), accept(&server_ep))
                        .await
                        .expect("server accept timeout")
                        .expect("server accept");
                let session = std::sync::Arc::new(PeerSession::from_connection(conn));

                tokio::time::timeout(std::time::Duration::from_secs(5), server_hello(&session))
                    .await
                    .expect("server hello timeout")
                    .expect("server hello");

                tokio::time::sleep(std::time::Duration::from_millis(200)).await;

                let bunch = session.take_stream_bunch().await;
                assert!(
                    bunch.is_none(),
                    "stream_bunch should be None (not yet assembled)"
                );

                session
                    .connection()
                    .close(quinn::VarInt::from(0u32), b"test done");
                session
            });

            let pins_dir = std::env::temp_dir().join(format!(
                "lan-mouse-stream-c-pins-{}-{}",
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

            let client_bunch = client_session.take_stream_bunch().await;
            assert!(
                client_bunch.is_none(),
                "client-side stream_bunch should also be None (symmetric with server side)"
            );

            let _server_session = server_session_fut.await.expect("server task");
            drop(client_session);
            client_ep.wait_idle().await;
            let _ = std::fs::remove_dir_all(&pins_dir);
        });
    }
}
