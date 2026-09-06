//! Application-layer wire protocol — Hello handshake + length-prefixed
//! frame codec + per-channel routing.
//!
//! This module owns the application-layer protocol on top of the QUIC link:
//!
//! - [`HELLO_TIMEOUT`] — application-layer Hello handshake timeout (3s)
//! - [`StreamPair`] — `(send, recv)` tuple cache for streams A / B / C
//!   (during the Hello window)
//! - [`Channel`] enum — the 4 channel variants
//!   (Datagram / StreamA / StreamB / StreamC)
//! - [`route_input`] — pure function that dispatches a `ProtoEvent` to the
//!   corresponding [`Channel`] based on per-handle [`InputChannelConfig`]
//! - [`client_hello`] / [`server_hello`] — application-layer Hello handshake
//! - [`write_hello_frame`] / [`read_hello_frame`] — Hello-specific frame codec
//! - [`write_frame`] / [`read_frame`] / [`read_any_frame`] — generic
//!   length-prefixed frame codec
//!
//! Relationship with [`super::session`]: both `client_hello` and
//! `server_hello` take a `&PeerSession` argument, cache the peer's stream A
//! into `peer.stream_a_cache`, then call `set_cached_send_a` to reuse the
//! send half.

use std::sync::atomic::Ordering;
use std::time::Duration;

use quinn::{RecvStream, SendStream, VarInt};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use lan_mouse_ipc::{ChannelMode, InputChannelConfig};
use lan_mouse_proto::{MAX_EVENT_SIZE, ProtoEvent};

use super::Error;
use super::session::PeerSession;

/// Application-layer Hello handshake timeout.
///
/// After the QUIC mTLS handshake completes, the peer must complete the
/// `PROTOCOL_MAGIC` exchange on stream A within `HELLO_TIMEOUT`; if it does
/// not, the connection is treated as a non-lan-mouse peer and is closed
/// with `Error::HelloTimeout(HELLO_TIMEOUT)`. The 3s value matches the
/// upstream reference implementation.
///
/// **Relationship with QUIC idle timeout**: `HELLO_TIMEOUT` only applies
/// during the Hello phase; afterwards `max_idle_timeout = 30s`
/// (see [`super::tls::default_transport_config`]) takes over.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(3);

/// Stream A / B / C cache: a `(send, recv)` tuple whose two halves can be
/// taken independently (the read loop takes the recv half; the send half
/// is left for the write path to reuse).
///
/// **Visibility `pub(crate)`**: held by the
/// [`super::session::PeerSession::stream_a_cache`] field and populated by
/// [`client_hello`] / [`server_hello`].
pub(crate) struct StreamPair {
    pub(crate) send: Option<SendStream>,
    pub(crate) recv: Option<RecvStream>,
}

impl StreamPair {
    pub(crate) fn new(send: SendStream, recv: RecvStream) -> Self {
        Self {
            send: Some(send),
            recv: Some(recv),
        }
    }
}

/// The 4 QUIC channel variants.
///
/// **Datagram** — QUIC unreliable datagrams: lowest latency, packets may
/// be dropped. `Motion` / `Axis` / `AxisDiscrete120` always travel on this
/// channel; mouse buttons travel on it when
/// `mouse_button = Datagram`; keyboard travels on it when
/// `keyboard = Datagram`.
///
/// **StreamA** — QUIC reliable bidi stream used as the control channel.
/// Low-frequency, must-arrive. `Enter` / `Leave` / `Ack` / `Hello` /
/// `Ping` / `Pong` always travel on this channel regardless of the
/// per-handle config. The Hello handshake itself runs on StreamA.
///
/// **StreamB** — QUIC reliable bidi stream used as the input channel.
/// Mouse buttons travel on it when `mouse_button = Stream`; keyboard
/// travels on it when `keyboard = Stream` (`Modifiers` follows the
/// keyboard config as well; see the implementation notes on
/// `route_input`).
///
/// **StreamC** — QUIC reliable bidi stream reserved for clipboard meta.
/// Currently no events are routed to StreamC (every existing event maps
/// to the first three variants); once M2 introduces
/// `ProtoEvent::Clipboard` / `Input(ClipboardEvent)` an additional
/// branch will be added.
///
/// **Derives**: `Debug / Clone / Copy / PartialEq / Eq`. `Copy` is sound
/// because all four variants are zero-sized. `Hash` is intentionally
/// omitted — the routing table uses `match`, not a `HashMap` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Datagram,
    StreamA,
    StreamB,
    StreamC,
}

/// Pure function that dispatches a `ProtoEvent` to the corresponding
/// [`Channel`] based on the per-handle [`InputChannelConfig`].
///
/// **Routing table** (must stay in lock-step with the `config.toml`
/// documentation, which states "Motion always travels on Datagram and is
/// unaffected by this setting"):
///
/// | `ProtoEvent` | `Channel` | Trigger |
/// |---|---|---|
/// | `Input(Pointer::Motion)` | `Datagram` | **Always**, independent of cfg |
/// | `Input(Pointer::Axis)` | `Datagram` | **Always** — high-frequency scroll delta |
/// | `Input(Pointer::AxisDiscrete120)` | `Datagram` | **Always** — discrete scroll tick |
/// | `Input(Pointer::Button)` | `Datagram` or `StreamB` | per `cfg.mouse_button` |
/// | `Input(Keyboard::Key)` | `Datagram` or `StreamB` | per `cfg.keyboard` |
/// | `Input(Keyboard::Modifiers)` | `Datagram` or `StreamB` | per `cfg.keyboard` (**critical**: must share the channel with `Key` so the modifier bitmask and key events stay in sync) |
/// | `Enter` / `Leave` / `Ack` / `Hello` / `Ping` / `Pong` | `StreamA` | **Always** — control flow |
/// | (M2 scope, not yet emitted) `Clipboard` etc. | `StreamC` | added when M2 introduces the variant |
///
/// **Why Motion / Axis / AxisDiscrete120 are always Datagram**: these
/// high-frequency inputs should not pay the Stream retransmission cost
/// for a single lost frame. `Axis` is the touchpad scroll delta;
/// `AxisDiscrete120` is a single mouse scroll-wheel tick (120 == one
/// detent); like `Motion` they are incremental streams and so they ride
/// the Datagram channel.
///
/// **Why Modifiers follows the keyboard config**: in lan-mouse the
/// modifier state is effectively a compressed view of the key state —
/// routing Modifiers on a different channel than Key would allow a
/// "Modifier already delivered on Datagram while Key is still queued on
/// Stream B" race. The config's `input_channels` section only exposes
/// `mouse_button` and `keyboard`, so having Modifiers follow `keyboard`
/// is the natural contract.
///
/// **Why Channel::StreamC has no routing rule**: M1 does not introduce
/// `ProtoEvent::Clipboard` / `Input(ClipboardEvent)`, and the upstream
/// `ProtoEvent` enum does not contain those variants either. The match
/// is exhaustive, so no `_ => unreachable!()` arm is required — every one
/// of the current eight `ProtoEvent` variants is listed explicitly, and
/// any M2 variant will produce a compile error reminding us to add the
/// missing arm.
#[allow(dead_code)]
pub fn route_input(cfg: &InputChannelConfig, event: &ProtoEvent) -> Channel {
    use input_event::{Event as InputEvent, KeyboardEvent, PointerEvent};

    match event {
        // (1) High-frequency pointer deltas — always Datagram
        //     (Motion / Axis / AxisDiscrete120)
        ProtoEvent::Input(InputEvent::Pointer(
            PointerEvent::Motion { .. }
            | PointerEvent::Axis { .. }
            | PointerEvent::AxisDiscrete120 { .. },
        )) => Channel::Datagram,

        // (2) Mouse buttons — dispatched per cfg.mouse_button
        ProtoEvent::Input(InputEvent::Pointer(PointerEvent::Button { .. })) => {
            match cfg.mouse_button {
                ChannelMode::Datagram => Channel::Datagram,
                ChannelMode::Stream => Channel::StreamB,
            }
        }

        // (3) Keyboard keys / Modifiers — dispatched per cfg.keyboard
        //     (must share the channel to avoid modifier/key ordering skew)
        ProtoEvent::Input(InputEvent::Keyboard(KeyboardEvent::Key { .. }))
        | ProtoEvent::Input(InputEvent::Keyboard(KeyboardEvent::Modifiers { .. })) => {
            match cfg.keyboard {
                ChannelMode::Datagram => Channel::Datagram,
                ChannelMode::Stream => Channel::StreamB,
            }
        }

        // (4) Control flow — always StreamA
        ProtoEvent::Enter(_)
        | ProtoEvent::Leave(_)
        | ProtoEvent::Ack(_)
        | ProtoEvent::Hello { .. }
        | ProtoEvent::Ping
        | ProtoEvent::Pong(_) => Channel::StreamA,
    }
}

/// Client-side Hello handshake.
///
/// 1. `peer.conn.open_bi().await` opens stream A (the control stream).
/// 2. Send `ProtoEvent::hello(local_commit())` to the peer.
/// 3. Wait for the peer's echo `ProtoEvent::Hello` (within `HELLO_TIMEOUT`).
/// 4. Verify that the echoed magic equals `PROTOCOL_MAGIC`:
///    - match → cache stream A into `peer.stream_a_cache` and set
///      `hello_ok = true`;
///    - mismatch → `conn.close(VarInt(0), "hello failed (wrong magic)")`
///      and return `Err(HelloFailed("wrong magic: ..."))`.
/// 5. Timeout → `conn.close(VarInt(0), "hello failed (timeout)")` and
///    return `Err(HelloTimeout(HELLO_TIMEOUT))`.
///
/// **Caching stream A**: `client_hello` and `server_hello` cache
/// symmetrically — the control plane reads and writes all happen on this
/// one stream. The read loop later takes ownership of the recv half via
/// `take_stream_a_recv()`, while the send half stays around for
/// `send_stream_a()` to reuse, avoiding opening a second stream A.
///
/// **Error unification**: every magic / decode / timeout failure
/// collapses into [`Error::HelloFailed`] or [`Error::HelloTimeout`].
/// `conn.close(...)` is always invoked first so the peer's `accept_bi()`
/// / `open_bi()` fails immediately with
/// `ConnectionError::LocallyClosed` and no zombie connection is left
/// behind.
#[allow(dead_code)]
pub async fn client_hello(peer: &PeerSession) -> std::result::Result<(), Error> {
    let (mut send, mut recv) = peer.conn.open_bi().await.map_err(Error::Handshake)?;
    let outgoing = ProtoEvent::hello(crate::config::local_commit());

    let exchange = async {
        write_hello_frame(&mut send, &outgoing).await?;
        read_hello_frame(&mut recv).await
    };
    let response = match tokio::time::timeout(HELLO_TIMEOUT, exchange).await {
        Ok(Ok(event)) => event,
        Ok(Err(e)) => return Err(e),
        Err(_elapsed) => {
            peer.conn
                .close(VarInt::from(0u32), b"hello failed (timeout)");
            log::warn!("client hello handshake timed out after {HELLO_TIMEOUT:?}");
            return Err(Error::HelloTimeout(HELLO_TIMEOUT));
        }
    };

    match response {
        ProtoEvent::Hello { magic, .. } if magic == lan_mouse_proto::PROTOCOL_MAGIC => {
            // **Ordering**: first put the pair into `stream_a_cache`, then
            // `take_stream_a_send` it back out and stash it in
            // `cached_send_a` for `send_stream_a` to reuse. The later
            // `take_stream_a_recv()` from the supervisor / peer.run does
            // not conflict because the two halves can be taken
            // independently.
            *peer.stream_a_cache.lock().await = Some(StreamPair::new(send, recv));
            let send_a = peer
                .take_stream_a_send()
                .await
                .expect("stream_a_cache just put Some(Pair { send: Some, recv: Some }) — take_stream_a_send must return Some");
            *peer.cached_send_a.lock().await = Some(send_a);
            peer.hello_ok.store(true, Ordering::Release);
            Ok(())
        }
        ProtoEvent::Hello { magic, .. } => {
            peer.conn
                .close(VarInt::from(0u32), b"hello failed (wrong magic)");
            log::warn!(
                "client hello rejected: wrong magic {:?}",
                std::str::from_utf8(&magic).unwrap_or("?????????")
            );
            Err(Error::HelloFailed(format!(
                "wrong magic: expected {:?}, got {:?}",
                std::str::from_utf8(&lan_mouse_proto::PROTOCOL_MAGIC).unwrap_or("????????"),
                std::str::from_utf8(&magic).unwrap_or("????????"),
            )))
        }
        other => {
            peer.conn
                .close(VarInt::from(0u32), b"hello failed (non-hello response)");
            log::warn!("client hello rejected: non-Hello response: {other}");
            Err(Error::HelloFailed(format!(
                "non-Hello response on stream A: {other}"
            )))
        }
    }
}

/// Server-side Hello handshake.
///
/// Mirrors `client_hello`:
/// 1. `peer.conn.accept_bi().await` waits for stream A (which the client
///    opens proactively with `open_bi`).
/// 2. Read the Hello frame sent by the client.
/// 3. Verify that `magic == PROTOCOL_MAGIC` (mismatch → close + `Err`).
/// 4. Echo our own Hello back to the client.
/// 5. Cache stream A into `peer.stream_a_cache` and set `hello_ok = true`.
///
/// **Failure semantics**: a synchronous failure on `open_bi` /
/// `accept_bi` returns `Err(HelloFailed)`; a timeout on
/// `read_hello_frame` returns `Err(HelloTimeout)`. Every failure path
/// invokes `conn.close(...)` before returning the error.
#[allow(dead_code)]
pub async fn server_hello(peer: &PeerSession) -> std::result::Result<(), Error> {
    let (mut send, mut recv) = peer
        .conn
        .accept_bi()
        .await
        .map_err(|e| Error::HelloFailed(format!("accept_bi: {e}")))?;

    let hello = match tokio::time::timeout(HELLO_TIMEOUT, read_hello_frame(&mut recv)).await {
        Ok(Ok(event)) => event,
        Ok(Err(e)) => return Err(e),
        Err(_elapsed) => {
            peer.conn
                .close(VarInt::from(0u32), b"hello failed (timeout)");
            log::warn!("server hello handshake timed out after {HELLO_TIMEOUT:?}");
            return Err(Error::HelloTimeout(HELLO_TIMEOUT));
        }
    };

    match &hello {
        ProtoEvent::Hello { magic, .. } if *magic == lan_mouse_proto::PROTOCOL_MAGIC => {}
        ProtoEvent::Hello { magic, .. } => {
            peer.conn
                .close(VarInt::from(0u32), b"hello failed (wrong magic)");
            log::warn!(
                "server hello rejected: wrong magic {:?}",
                std::str::from_utf8(magic).unwrap_or("????????")
            );
            return Err(Error::HelloFailed(format!(
                "wrong magic: expected {:?}, got {:?}",
                std::str::from_utf8(&lan_mouse_proto::PROTOCOL_MAGIC).unwrap_or("????????"),
                std::str::from_utf8(magic).unwrap_or("????????"),
            )));
        }
        other => {
            peer.conn
                .close(VarInt::from(0u32), b"hello failed (non-hello frame)");
            log::warn!("server hello rejected: non-Hello frame: {other}");
            return Err(Error::HelloFailed(format!(
                "non-Hello frame on stream A: {other}"
            )));
        }
    }

    // Echo our own Hello back to the client.
    let outgoing = ProtoEvent::hello(crate::config::local_commit());
    write_hello_frame(&mut send, &outgoing).await?;

    // **Bidirectional symmetry**: server-side and client-side `send_stream_a`
    // share the same bidi stream — server's recv sees what the client
    // sent, and server's send reaches the client's recv. So the server
    // supervisor reading `recv_a` will receive the client's
    // Enter / Ack / Pong / etc. control events, while the server's
    // `send_stream_a` (re)uses the cached send half for its own
    // outbound control traffic.
    *peer.stream_a_cache.lock().await = Some(StreamPair::new(send, recv));
    let send_a = peer
        .take_stream_a_send()
        .await
        .expect("stream_a_cache just put Some(Pair { send: Some, recv: Some }) — take_stream_a_send must return Some");
    *peer.cached_send_a.lock().await = Some(send_a);

    peer.hello_ok.store(true, Ordering::Release);
    Ok(())
}

/// Encode a `ProtoEvent` as a **length-prefixed frame** and write it to
/// the stream.
///
/// Frame layout: `[u32 BE length][bytes...]`. The Hello-specific codec
/// shares the same on-wire format as the generic `write_frame`; the
/// separate Hello path exists to avoid an import cycle while the generic
/// codec is being brought up.
///
/// **Error propagation**: write I/O errors are passed through as
/// `Error::HelloFailed("write Hello frame: ...")`. The
/// `ProtoEvent::try_from` / `.into()` conversion cannot fail (fixed-size
/// codec, Hello is only 17 bytes), so there is no decode error path.
async fn write_hello_frame(
    send: &mut SendStream,
    event: &ProtoEvent,
) -> std::result::Result<(), Error> {
    let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = (*event).into();
    send.write_u32(len as u32)
        .await
        .map_err(|e| Error::HelloFailed(format!("write Hello frame length: {e}")))?;
    send.write_all(&buf[..len])
        .await
        .map_err(|e| Error::HelloFailed(format!("write Hello frame body: {e}")))?;
    Ok(())
}

/// Read a **length-prefixed frame** from the stream and decode it into a
/// `ProtoEvent`.
///
/// Frame layout: `[u32 BE length][bytes...]`. Read the BE `u32` length,
/// reject `len > MAX_EVENT_SIZE` (DoS guard: an attacker who controls
/// the length field could otherwise coax `read_exact` into reading a
/// huge number of bytes), then `read_exact(&mut buf[..len])`, then
/// `ProtoEvent::try_from(buf)`.
///
/// **Error propagation**:
/// - `read_exact` I/O error → `Error::HelloFailed("read Hello frame: ...")`
/// - `ProtoEvent::try_from` failure →
///   `Error::HelloFailed("decode Hello frame: ...")`
///
/// **Visibility `pub(crate)`**: used by the unit test
/// `send_stream_a_round_trip_control_event` (in `session.rs`) which
/// reads a Ping frame off the server-side `recv_a`.
pub(crate) async fn read_hello_frame(
    recv: &mut RecvStream,
) -> std::result::Result<ProtoEvent, Error> {
    let len = recv
        .read_u32()
        .await
        .map_err(|e| Error::HelloFailed(format!("read Hello frame length: {e}")))?
        as usize;
    if len > MAX_EVENT_SIZE {
        return Err(Error::HelloFailed(format!(
            "Hello frame too large: {len} bytes (max {MAX_EVENT_SIZE})"
        )));
    }
    let mut buf = [0u8; MAX_EVENT_SIZE];
    recv.read_exact(&mut buf[..len])
        .await
        .map_err(|e| Error::HelloFailed(format!("read Hello frame body ({len} bytes): {e}")))?;
    ProtoEvent::try_from(buf).map_err(|e| Error::HelloFailed(format!("decode Hello frame: {e}")))
}

/// Encode a `ProtoEvent` as a **length-prefixed frame** and write it to
/// any `AsyncWrite` stream.
///
/// Frame layout: `[u32 BE length][bytes...]`
///
/// 1. `From<ProtoEvent> for ([u8; MAX_EVENT_SIZE], usize)` encodes into a
///    fixed-size buffer and returns `(buf, len)` — `buf` is
///    zero-padded past `len`.
/// 2. `write_u32(len as u32).await` writes the 4-byte BE length prefix.
/// 3. `write_all(&buf[..len]).await` writes the `len` payload bytes.
///
/// **Why `MAX_EVENT_SIZE` as the buffer cap?** All `ProtoEvent` variants
/// in `lan-mouse-proto` use a fixed-size codec and encode to ≤ 21 bytes;
/// the trailing zeros in `buf` are ignored by `ProtoEvent::try_from`
/// (which only inspects the first `len` bytes). When M2 introduces a
/// variable-length codec (e.g. clipboard payloads) a dedicated
/// `MAX_FRAME_SIZE` constant will replace this.
///
/// **Generic `W: AsyncWrite + Unpin`**: production path uses
/// `W = SendStream` (the write half of a quinn 0.11 bidi stream); unit
/// tests can plug in local types like `tokio::io::DuplexStream` or
/// `Vec<u8>` to exercise the codec.
///
/// **Error propagation**: write I/O errors collapse into
/// [`Error::HelloFailed`]. Failure to write the length field means the
/// stream is gone, mirroring the read side's [`Error::Truncated`].
///
/// **Visibility `pub(crate)`**: called by internal helpers such as
/// [`super::session::PeerSession::send_stream_a`]; the external test
/// `frame_round_trip` reaches it through `use super::*`.
#[allow(dead_code)]
pub(crate) async fn write_frame<W>(
    send: &mut W,
    event: &ProtoEvent,
) -> std::result::Result<(), Error>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = (*event).into();
    send.write_u32(len as u32)
        .await
        .map_err(|e| Error::HelloFailed(format!("write frame length: {e}")))?;
    send.write_all(&buf[..len])
        .await
        .map_err(|e| Error::HelloFailed(format!("write frame body: {e}")))?;
    Ok(())
}

/// Read a **length-prefixed frame** from any `AsyncRead` stream and
/// decode it into a `ProtoEvent`.
///
/// Frame layout: `[u32 BE length][bytes...]`
///
/// 1. `read_u32().await` reads the 4-byte BE length prefix → `len`.
/// 2. `len > MAX_EVENT_SIZE` → `Err([`Error::FrameTooLarge`]`(len))` —
///    DoS guard.
/// 3. `read_exact(&mut buf[..len]).await` reads the `len` payload bytes.
/// 4. `ProtoEvent::try_from(buf)` decodes the payload.
///
/// **Error unification** (distinguished from the Hello-specific
/// `read_hello_frame`):
/// - `FrameTooLarge(usize)` — passed through as-is; a dedicated variant
///   so the reader task can fatally close the connection on it.
/// - `Truncated` — `read_exact` failed because the peer half-closed
///   the stream (quinn reports `UnexpectedEof` / `ClosedStream`).
///   Fatal; the reader does **not** try to skip past the bad frame and
///   keep reading.
/// - `HelloFailed(msg)` — failure to read the length field or to decode
///   the body via `ProtoEvent::try_from`.
///
/// **Why the buffer tail is not trimmed?** `ProtoEvent::try_from` is
/// declared as `fn try_from([u8; MAX_EVENT_SIZE]) -> Result<Self, _>`,
/// so passing `&buf[..len]` does not compile. `read_exact` only writes
/// into the front of the buffer (the tail stays zero), which matches
/// the fixed-size codec's assumption that decoding depends only on the
/// effective field length, not on trailing zeros.
///
/// **Generic `R: AsyncRead + Unpin`**: mirrors [`write_frame`] —
/// production uses `R = RecvStream`; tests can use
/// `tokio::io::DuplexStream`.
///
/// **Visibility `pub`**: called by internal helpers like
/// [`super::streams::read_stream_b_loop`] and also by the `listen.rs`
/// supervisor and other modules, so it must stay `pub`.
#[allow(dead_code)]
pub async fn read_frame<R>(recv: &mut R) -> std::result::Result<ProtoEvent, Error>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let len = recv
        .read_u32()
        .await
        .map_err(|e| Error::HelloFailed(format!("read frame length: {e}")))? as usize;
    if len > MAX_EVENT_SIZE {
        return Err(Error::FrameTooLarge(len));
    }
    let mut buf = [0u8; MAX_EVENT_SIZE];
    match recv.read_exact(&mut buf[..len]).await {
        Ok(_bytes_read) => {}
        // Peer half-closed the stream mid-frame → Truncated (distinct from a
        // decode-side HelloFailed failure).
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Err(Error::Truncated);
        }
        Err(e) => {
            return Err(Error::HelloFailed(format!(
                "read frame body ({len} bytes): {e}"
            )));
        }
    }
    ProtoEvent::try_from(buf).map_err(|e| Error::HelloFailed(format!("decode frame: {e}")))
}

/// Public alias for the single-frame reader specialised to `RecvStream`.
///
/// **Difference from [`read_frame`]**: the signature is fixed to
/// `&mut RecvStream`, so callers in `listen.rs` (the supervisor's
/// `accept_bi` sub-tasks) don't need to thread a turbofish type
/// parameter through. `quinn::RecvStream` already implements
/// `tokio::io::AsyncRead + Unpin`, so all of [`read_frame`]'s logic is
/// reused verbatim.
///
/// **Use case**: after the server's `accept_bi()` accepts a stream B / C
/// bidi opened by the client, the sub-task loops over
/// `read_any_frame(&mut recv)` to decode frames and translate them into
/// `ListenEvent::Msg`.
#[allow(dead_code)]
pub async fn read_any_frame(recv: &mut RecvStream) -> std::result::Result<ProtoEvent, Error> {
    read_frame(recv).await
}

/// Watchdog for the application-layer Hello handshake.
///
/// **Purpose**: a successful QUIC mTLS handshake does **not** guarantee
/// that the peer is actually a lan-mouse instance — a peer could pass
/// mTLS (self-signed root trust + fingerprint allowlist) and still
/// deliberately never open stream A, leaving `client_hello()` /
/// `server_hello()` parked forever on `open_bi()` / `accept_bi()`. The
/// `HELLO_TIMEOUT` watchdog provides a non-blocking fallback:
///
/// 1. Spawn a tokio task that sleeps `HELLO_TIMEOUT`.
/// 2. Check `peer.hello_ok()` — if it is `true` (Hello already
///    completed) the task exits silently.
/// 3. Otherwise actively `conn.close(VarInt(0), "hello timeout")` to
///    tear down the connection, plus a `log::warn`. The peer's
///    `client_hello()` / `server_hello()` will then fail immediately
///    with `ConnectionError::LocallyClosed` from their
///    `accept_bi().await` / `open_bi().await`.
///
/// **Does not** block `client_hello` / `server_hello` themselves — those
/// two functions already wrap their inner work in
/// `tokio::time::timeout(HELLO_TIMEOUT, ...)` (see the implementations
/// below); the watchdog is the fallback for the case where the peer
/// never even *starts* the Hello.
///
/// **Visibility `pub(crate)`**: invoked by
/// [`super::session::PeerSession::run`].
#[allow(dead_code)]
pub(crate) fn hello_watchdog(peer: std::sync::Arc<PeerSession>) {
    use std::sync::atomic::Ordering;
    tokio::spawn(async move {
        tokio::time::sleep(HELLO_TIMEOUT).await;
        if !peer.hello_ok.load(Ordering::Acquire) {
            log::warn!("hello watchdog: hello_ok not set within {HELLO_TIMEOUT:?}, proactively closing connection");
            peer.conn
                .close(VarInt::from(0u32), b"hello timeout (watchdog)");
        }
    });
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddrV4};

    use lan_mouse_ipc::{ChannelMode, InputChannelConfig};
    use lan_mouse_proto::ProtoEvent;

    use crate::quic_transport::endpoint::accept;
    use crate::quic_transport::endpoint::dial;
    use crate::quic_transport::endpoint::endpoint;
    use crate::quic_transport::session::PeerSession;
    use crate::quic_transport::test_helpers::{
        endpoint_with_test_cert, ephemeral_cert, ephemeral_pins_dir, local_set_test,
    };

    use super::*;

    /// Hello handshake acceptance test (1/3): happy path — both sides run
    /// `server_hello` / `client_hello`, both `peer.hello_ok()` return
    /// `true`, and both `stream_a_cache` instances have a cached pair.
    #[tokio::test]
    async fn hello_happy_path_exchanges_magic() {
        crate::quic_transport::endpoint::install_crypto_provider();

        let (server_cert_chain, server_key) = ephemeral_cert();
        let server_ep = endpoint_with_test_cert(
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into(),
            server_cert_chain,
            server_key,
        )
        .expect("server endpoint bind");
        let server_addr = server_ep.local_addr().expect("server addr");

        let server_task = tokio::spawn(async move {
            let conn = tokio::time::timeout(std::time::Duration::from_secs(5), accept(&server_ep))
                .await
                .expect("server accept timeout")
                .expect("server accept");
            let session = PeerSession::from_connection(conn);

            tokio::time::timeout(std::time::Duration::from_secs(5), server_hello(&session))
                .await
                .expect("server hello timeout")
                .expect("server hello should succeed");

            assert!(
                session.hello_ok(),
                "server side hello_ok should be true (server_hello has been set)"
            );
            assert!(
                session.take_stream_a_recv().await.is_some(),
                "peer.stream_a_cache.recv should be cached after server_hello"
            );

            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            drop(session);
        });

        let pins_dir = ephemeral_pins_dir();
        let _ = std::fs::remove_dir_all(&pins_dir);
        let (client_cert_chain, client_key) = ephemeral_cert();
        let client_ep = endpoint(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
            .expect("client endpoint bind");
        let conn = dial(
            &client_ep,
            server_addr,
            client_cert_chain[0].clone(),
            client_key,
            &pins_dir,
                        std::time::Duration::from_secs(5),
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
        .expect("client hello should succeed");

        assert!(
            client_session.hello_ok(),
            "client side hello_ok should be true (client_hello has been set)"
        );
        assert!(
            client_session.take_stream_a_recv().await.is_some(),
            "peer.stream_a_cache.recv should be cached after client_hello"
        );

        drop(client_session);
        drop(client_ep);
        server_task.await.expect("server task");
        let _ = std::fs::remove_dir_all(&pins_dir);
    }

    /// Hello handshake acceptance test (2/3): server sends the wrong magic
    /// → client receives `Error::HelloFailed`.
    #[tokio::test(flavor = "multi_thread")]
    async fn hello_wrong_magic_closes_connection() {
        local_set_test!(hello_wrong_magic_closes_connection, {
            crate::quic_transport::endpoint::install_crypto_provider();

            let (server_cert_chain, server_key) = ephemeral_cert();
            let server_ep = endpoint_with_test_cert(
                SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into(),
                server_cert_chain,
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

                let (mut send, _recv) =
                    tokio::time::timeout(std::time::Duration::from_secs(5), conn.accept_bi())
                        .await
                        .expect("accept_bi timeout")
                        .expect("accept_bi");

                let wrong = ProtoEvent::Hello {
                    magic: *b"LAN-MOUS",
                    commit: [0u8; 8],
                };
                super::write_hello_frame(&mut send, &wrong)
                    .await
                    .expect("server write wrong hello");
                send.finish().expect("finish");

                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                conn.close(VarInt::from(0u32), b"test done");
                drop(conn);
            });

            let pins_dir = ephemeral_pins_dir();
            let _ = std::fs::remove_dir_all(&pins_dir);
            let (client_cert_chain, client_key) = ephemeral_cert();
            let client_ep = endpoint(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
                .expect("client endpoint bind");
            let conn = dial(
                &client_ep,
                server_addr,
                client_cert_chain[0].clone(),
                client_key,
                &pins_dir,
                            std::time::Duration::from_secs(5),
            )
            .await
            .expect("dial");
            let client_session = PeerSession::from_connection(conn);

            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                client_hello(&client_session),
            )
            .await
            .expect("client hello timeout (5s fallback)")
            .expect_err("client_hello should return Err(HelloFailed)");

            match &result {
                crate::quic_transport::Error::HelloFailed(msg) => {
                    assert!(
                        msg.contains("wrong magic"),
                        "HelloFailed message should contain 'wrong magic', actually: {msg}"
                    );
                }
                other => panic!("error should be Error::HelloFailed(wrong magic...), actually: {other:?}"),
            }

            assert!(!client_session.hello_ok(), "hello_ok should remain false on failure path");

            drop(client_session);
            drop(client_ep);
            let _ = server_task.await;
            let _ = std::fs::remove_dir_all(&pins_dir);
        });
    }

    /// Hello handshake acceptance test (3/3): peer never opens stream A →
    /// after 3s the client gets `Error::HelloTimeout(HELLO_TIMEOUT)`.
    #[tokio::test]
    async fn hello_timeout_aborts_session() {
        crate::quic_transport::endpoint::install_crypto_provider();

        let (server_cert_chain, server_key) = ephemeral_cert();
        let server_ep = endpoint_with_test_cert(
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into(),
            server_cert_chain,
            server_key,
        )
        .expect("server endpoint bind");
        let server_addr = server_ep.local_addr().expect("server addr");

        let server_task = tokio::spawn(async move {
            let conn = tokio::time::timeout(std::time::Duration::from_secs(10), accept(&server_ep))
                .await
                .expect("server accept timeout")
                .expect("server accept");
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            drop(conn);
        });

        let pins_dir = ephemeral_pins_dir();
        let _ = std::fs::remove_dir_all(&pins_dir);
        let (client_cert_chain, client_key) = ephemeral_cert();
        let client_ep = endpoint(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
            .expect("client endpoint bind");
        let conn = dial(
            &client_ep,
            server_addr,
            client_cert_chain[0].clone(),
            client_key,
            &pins_dir,
                        std::time::Duration::from_secs(5),
        )
        .await
        .expect("dial");
        let client_session = PeerSession::from_connection(conn);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client_hello(&client_session),
        )
        .await
        .expect("client_hello total timeout should not be triggered (HELLO_TIMEOUT=3s should trigger first)")
        .expect_err("client_hello should return Err(HelloTimeout)");

        match &result {
            crate::quic_transport::Error::HelloTimeout(d) => {
                assert_eq!(*d, HELLO_TIMEOUT, "HelloTimeout should equal HELLO_TIMEOUT (3s)");
            }
            other => panic!("error should be Error::HelloTimeout(HELLO_TIMEOUT), actually: {other:?}"),
        }

        assert!(!client_session.hello_ok(), "hello_ok should remain false on timeout path");

        drop(client_session);
        drop(client_ep);
        let _ = server_task.await;
        let _ = std::fs::remove_dir_all(&pins_dir);
    }

    // === route_input pure-function unit tests ================================

    mod route_input_fixtures {
        use super::*;
        use input_event::{Event as InputEvent, KeyboardEvent, PointerEvent};
        use lan_mouse_proto::Position;

        pub(super) fn motion() -> ProtoEvent {
            ProtoEvent::Input(InputEvent::Pointer(PointerEvent::Motion {
                time: 0,
                dx: 1.0,
                dy: 2.0,
            }))
        }

        pub(super) fn axis() -> ProtoEvent {
            ProtoEvent::Input(InputEvent::Pointer(PointerEvent::Axis {
                time: 0,
                axis: 0,
                value: 1.0,
            }))
        }

        pub(super) fn axis_discrete() -> ProtoEvent {
            ProtoEvent::Input(InputEvent::Pointer(PointerEvent::AxisDiscrete120 {
                axis: 0,
                value: 120,
            }))
        }

        pub(super) fn button() -> ProtoEvent {
            ProtoEvent::Input(InputEvent::Pointer(PointerEvent::Button {
                time: 0,
                button: 0x110,
                state: 1,
            }))
        }

        pub(super) fn key() -> ProtoEvent {
            ProtoEvent::Input(InputEvent::Keyboard(KeyboardEvent::Key {
                time: 0,
                key: 30,
                state: 1,
            }))
        }

        pub(super) fn modifiers() -> ProtoEvent {
            ProtoEvent::Input(InputEvent::Keyboard(KeyboardEvent::Modifiers {
                depressed: 0x01 | 0x02,
                latched: 0,
                locked: 0,
                group: 0,
            }))
        }

        pub(super) fn enter() -> ProtoEvent {
            ProtoEvent::Enter(Position::Left)
        }

        pub(super) fn leave() -> ProtoEvent {
            ProtoEvent::Leave(42)
        }

        pub(super) fn ack() -> ProtoEvent {
            ProtoEvent::Ack(42)
        }

        pub(super) fn hello() -> ProtoEvent {
            ProtoEvent::hello(*b"deadbeef")
        }

        pub(super) fn ping() -> ProtoEvent {
            ProtoEvent::Ping
        }

        pub(super) fn pong() -> ProtoEvent {
            ProtoEvent::Pong(true)
        }
    }

    #[test]
    fn route_input_default_motion_datagram_keyboard_stream() {
        use route_input_fixtures::*;
        let cfg = InputChannelConfig::default();
        assert_eq!(cfg.mouse_button, ChannelMode::Datagram);
        assert_eq!(cfg.keyboard, ChannelMode::Stream);

        assert_eq!(route_input(&cfg, &motion()), Channel::Datagram);
        assert_eq!(route_input(&cfg, &axis()), Channel::Datagram);
        assert_eq!(route_input(&cfg, &axis_discrete()), Channel::Datagram);
        assert_eq!(route_input(&cfg, &button()), Channel::Datagram);

        assert_eq!(route_input(&cfg, &key()), Channel::StreamB);
        assert_eq!(route_input(&cfg, &modifiers()), Channel::StreamB);

        assert_eq!(route_input(&cfg, &enter()), Channel::StreamA);
        assert_eq!(route_input(&cfg, &leave()), Channel::StreamA);
        assert_eq!(route_input(&cfg, &ack()), Channel::StreamA);
        assert_eq!(route_input(&cfg, &hello()), Channel::StreamA);
        assert_eq!(route_input(&cfg, &ping()), Channel::StreamA);
        assert_eq!(route_input(&cfg, &pong()), Channel::StreamA);
    }

    #[test]
    fn route_input_all_stream_motion_still_datagram() {
        use route_input_fixtures::*;
        let cfg = InputChannelConfig {
            mouse_button: ChannelMode::Stream,
            keyboard: ChannelMode::Stream,
        };

        assert_eq!(
            route_input(&cfg, &motion()),
            Channel::Datagram,
            "Motion always goes through Datagram, independent of cfg.mouse_button"
        );
        assert_eq!(route_input(&cfg, &axis()), Channel::Datagram);
        assert_eq!(route_input(&cfg, &axis_discrete()), Channel::Datagram);
        assert_eq!(route_input(&cfg, &button()), Channel::StreamB);
        assert_eq!(route_input(&cfg, &key()), Channel::StreamB);
        assert_eq!(route_input(&cfg, &modifiers()), Channel::StreamB);
        assert_eq!(route_input(&cfg, &enter()), Channel::StreamA);
        assert_eq!(route_input(&cfg, &ack()), Channel::StreamA);
    }

    #[test]
    fn route_input_all_datagram_everything_datagram() {
        use route_input_fixtures::*;
        let cfg = InputChannelConfig {
            mouse_button: ChannelMode::Datagram,
            keyboard: ChannelMode::Datagram,
        };

        assert_eq!(route_input(&cfg, &motion()), Channel::Datagram);
        assert_eq!(route_input(&cfg, &axis()), Channel::Datagram);
        assert_eq!(route_input(&cfg, &axis_discrete()), Channel::Datagram);
        assert_eq!(route_input(&cfg, &button()), Channel::Datagram);
        assert_eq!(route_input(&cfg, &key()), Channel::Datagram);
        assert_eq!(route_input(&cfg, &modifiers()), Channel::Datagram);

        assert_eq!(route_input(&cfg, &enter()), Channel::StreamA);
        assert_eq!(route_input(&cfg, &leave()), Channel::StreamA);
        assert_eq!(route_input(&cfg, &ack()), Channel::StreamA);
        assert_eq!(route_input(&cfg, &hello()), Channel::StreamA);
        assert_eq!(route_input(&cfg, &ping()), Channel::StreamA);
        assert_eq!(route_input(&cfg, &pong()), Channel::StreamA);
    }

    #[test]
    fn route_input_mixed_mouse_stream_keyboard_datagram() {
        use route_input_fixtures::*;
        let cfg = InputChannelConfig {
            mouse_button: ChannelMode::Stream,
            keyboard: ChannelMode::Datagram,
        };

        assert_eq!(route_input(&cfg, &motion()), Channel::Datagram);
        assert_eq!(route_input(&cfg, &axis()), Channel::Datagram);
        assert_eq!(route_input(&cfg, &axis_discrete()), Channel::Datagram);

        assert_eq!(route_input(&cfg, &button()), Channel::StreamB);

        assert_eq!(route_input(&cfg, &key()), Channel::Datagram);
        assert_eq!(
            route_input(&cfg, &modifiers()),
            Channel::Datagram,
            "Modifier must use the same channel as Key (avoid modifier/key cross-channel timing skew)"
        );

        assert_eq!(route_input(&cfg, &enter()), Channel::StreamA);
        assert_eq!(route_input(&cfg, &leave()), Channel::StreamA);
        assert_eq!(route_input(&cfg, &ack()), Channel::StreamA);
        assert_eq!(route_input(&cfg, &hello()), Channel::StreamA);
        assert_eq!(route_input(&cfg, &ping()), Channel::StreamA);
        assert_eq!(route_input(&cfg, &pong()), Channel::StreamA);
    }
}
