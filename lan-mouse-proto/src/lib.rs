use input_event::{Event as InputEvent, KeyboardEvent, PointerEvent};
use num_enum::{IntoPrimitive, TryFromPrimitive, TryFromPrimitiveError};
use paste::paste;
use std::{
    fmt::{Debug, Display, Formatter},
    mem::size_of,
};
use thiserror::Error;

mod codec;

use crate::codec::{FixedCodec, VarCodec};

/// defines the maximum size an encoded event can take up
/// this is currently the pointer motion event
/// type: u8, time: u32, dx: f64, dy: f64
pub const MAX_EVENT_SIZE: usize = size_of::<u8>() + size_of::<u32>() + 2 * size_of::<f64>();

/// Maximum size of a single application-layer frame on StreamA / B / C.
///
/// **PLAN-2 / M0a** (codec dual-track): the fixed-size cap is
/// [`MAX_EVENT_SIZE`] (21 bytes), but variable-length events (the new
/// `ClipboardText` / `ClipboardImage` / `ClipboardFiles` /
/// `FileTransferOffer` / `FileTransferResponse` / `FileTransferCancel` /
/// `ClipboardRequest` variants — see PLAN §1 关键设计原则) need a larger
/// frame for the inline body of `ClipboardText` (≤ 1 KiB per the
/// inline-policy boundary). 16 KiB is generous for metadata frames and
/// gives the DoS cap a concrete number without forcing a heap
/// pre-allocation on every read.
///
/// **Wire-compat** (PLAN §0 评审 #1): the larger cap only applies to
/// StreamC frames (which are dropped by old daemons' `streams.rs:291`);
/// StreamA / StreamB frames remain bounded by `MAX_EVENT_SIZE` in
/// practice (all hot-path variants fit).
pub const MAX_FRAME_SIZE: usize = 16 * 1024;

/// Maximum size of a clipboard text body carried inline in a StreamC
/// `ClipboardText` event. Larger values are represented by metadata only;
/// the receiver records a pending request and a later milestone fetches the
/// bytes through HTTP/3.
pub const CLIPBOARD_TEXT_INLINE_LIMIT: usize = 1024;

/// 8-byte protocol magic identifying a lan-mouse peer, carried in every
/// [`ProtoEvent::Hello`]. The `Hello` is exchanged right after the QUIC
/// mTLS handshake authenticates; a peer that fails to present this exact
/// magic within the handshake window is not a lan-mouse instance and
/// has its connection refused at the [`crate::quic_transport`] layer
/// (see the `client_hello` / `server_hello` exchange there). lan-mouse
/// is deliberately **not** wire-compatible with mousehop or any other
/// fork — change this magic to force a hard break against a future
/// divergence.
///
/// NOTE: kept as the brand string `LANMOUSE` (8 bytes, no `b' '`)
/// rather than the tool name `lan-mouse` (which contains a `-` outside
/// the ASCII short-id alphabet).
pub const PROTOCOL_MAGIC: [u8; 8] = *b"LANMOUSE";

/// error type for protocol violations
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// event type does not exist
    #[error("invalid event id: `{0}`")]
    InvalidEventId(#[from] TryFromPrimitiveError<EventType>),
    /// position type does not exist
    #[error("invalid event id: `{0}`")]
    InvalidPosition(#[from] TryFromPrimitiveError<Position>),
    /// frame body was truncated (var codec decode ran out of bytes,
    /// or the input length prefix did not match the body length).
    /// Distinct from `InvalidEventId` because it signals a
    /// wire-format-level problem rather than an unknown variant.
    #[error("frame body too short or truncated")]
    FrameTooShort,
    /// empty input (zero-length buffer) — no type byte to decode.
    #[error("empty input (no type byte)")]
    EmptyInput,
}

/// Position of a client
#[derive(Clone, Copy, Debug, PartialEq, Eq, TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum Position {
    Left,
    Right,
    Top,
    Bottom,
}

impl Display for Position {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let pos = match self {
            Position::Left => "left",
            Position::Right => "right",
            Position::Top => "top",
            Position::Bottom => "bottom",
        };
        write!(f, "{pos}")
    }
}

/// A single file entry inside a `ClipboardFiles` payload.
///
/// PLAN-2 / M0a: `sha256` is `[u8; 32]` (fixed-size, easy to
/// serialize, matches the existing wire convention for fingerprints /
/// content hashes). `name` and `mime` are `String` (variable-length,
/// length-prefixed on the wire by `VarCodec`).
///
/// **Wire-compat** (PLAN §0 评审 #1): FileEntry only travels on StreamC,
/// which old daemons drop silently — no wire-compat concern with
/// pre-M0a peers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    pub name: String,
    pub size: u64,
    pub mime: String,
    pub sha256: [u8; 32],
}

/// Clipboard text payload — used by M1a / M1b for cross-device text
/// sync.
///
/// `content_inline = Some(_)` for ≤ 1 KiB (carries the bytes inline);
/// `None` for larger payloads (peer must HTTP/3 GET
/// `/clipboard/text/{sha256}` to fetch).
///
/// `fingerprint` is the loop-avoidance tag (e.g. sha256 of the first 64
/// bytes + length); `sha256` is the full content hash used by the
/// HTTP/3 byte-pull endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardText {
    pub fingerprint: [u8; 32],
    pub sha256: [u8; 32],
    pub size: u64,
    pub content_inline: Option<Vec<u8>>,
}

impl ClipboardText {
    /// Build the canonical wire representation for clipboard text.
    ///
    /// The payload is kept inline only when its byte length is at most
    /// [`CLIPBOARD_TEXT_INLINE_LIMIT`]. Larger payloads intentionally drop
    /// the bytes here and retain only `sha256 + size`; the sender-side cache
    /// and receiver-side HTTP/3 pull are wired in a later milestone. Keeping
    /// this policy in one constructor prevents individual dispatch paths
    /// from accidentally sending a large body inline.
    pub fn from_content(fingerprint: [u8; 32], sha256: [u8; 32], content: Vec<u8>) -> Self {
        let size = content.len() as u64;
        let content_inline = if content.len() <= CLIPBOARD_TEXT_INLINE_LIMIT {
            Some(content)
        } else {
            None
        };
        Self {
            fingerprint,
            sha256,
            size,
            content_inline,
        }
    }

    /// Whether this event carries its content bytes inline.
    pub fn is_inline(&self) -> bool {
        self.content_inline.is_some()
    }
}

/// Clipboard image metadata — used by M2a / M2b. Bytes always go
/// through HTTP/3 GET `/clipboard/image/{sha256}`; `mime` selects the
/// decode path (PNG / JPG / BMP / `application/x-dib`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardImage {
    pub fingerprint: [u8; 32],
    pub mime: String,
    pub sha256: [u8; 32],
    pub size: u64,
}

/// Clipboard files metadata — used by M3a. Carries one
/// [`FileEntry`] per file in the OS clipboard selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardFiles {
    pub fingerprint: [u8; 32],
    pub entries: Vec<FileEntry>,
}

/// File-transfer offer (sender → receiver) — used by M3a / M3b.
/// Sent on StreamC as soon as the sender's
/// `service::clipboard_dispatcher` detects file content; receiver
/// responds with [`FileTransferResponse`] after user decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileTransferOffer {
    pub sha256: [u8; 32],
    pub name: String,
    pub size: u64,
    pub mime: String,
}

/// File-transfer accept/reject (receiver → sender) — used by M3a /
/// M3b. `accept = false` causes the sender to clean up
/// `file_cache[{sha256}]` without further action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileTransferResponse {
    pub sha256: [u8; 32],
    pub accept: bool,
}

/// File-transfer cancel (sender → receiver) — used by M3a STEP 3a.5.
/// Sent when the sender's clipboard is overwritten before the
/// transfer completes; receiver stops its HTTP/3 stream and removes
/// the `.partial` file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileTransferCancel {
    pub sha256: [u8; 32],
}

/// Clipboard content request (receiver → sender) — used by M1b
/// STEP 1b.1 to pull large text from the sender's `clipboard_cache`
/// via HTTP/3 after receiving a metadata-only `ClipboardText`
/// (size > 1 KiB, no inline).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardRequest {
    pub sha256: [u8; 32],
}

/// main lan-mouse protocol event type
#[derive(Clone, Debug, PartialEq)]
pub enum ProtoEvent {
    /// notify a client that the cursor entered its region at the given position
    /// [`ProtoEvent::Ack`] with the same serial is used for synchronization between devices
    Enter(Position),
    /// notify a client that the cursor left its region
    /// [`ProtoEvent::Ack`] with the same serial is used for synchronization between devices
    Leave(u32),
    /// acknowledge of an [`ProtoEvent::Enter`] or [`ProtoEvent::Leave`] event
    Ack(u32),
    /// Input event
    Input(InputEvent),
    /// Ping event for tracking unresponsive clients.
    /// A client has to respond with [`ProtoEvent::Pong`].
    Ping,
    /// Response to [`ProtoEvent::Ping`], true if emulation is enabled / available
    Pong(bool),
    /// Build identification for the sending peer. Sent by the
    /// connect side once after the mTLS handshake authenticates,
    /// and echoed back by the listen side in reply, so each end can
    /// display the peer's build hash and warn (soft) on mismatch.
    ///
    /// `magic` must equal [`PROTOCOL_MAGIC`]; a peer that does not
    /// present this magic within the handshake window is not a
    /// lan-mouse instance and has its connection refused at the
    /// [`crate::quic_transport`] layer. The type-level decode here
    /// still succeeds for any 8-byte magic — the connection layer
    /// is what enforces the value.
    ///
    /// `commit` is the 8-byte ASCII short commit hash from
    /// `shadow_rs`'s `SHORT_COMMIT`. Old peers that don't
    /// recognize the event type silently skip it per the
    /// forward-compat handling in the receive loop.
    Hello { magic: [u8; 8], commit: [u8; 8] },
    // === PLAN-2 / M0a — variable-length codec events (StreamC only) ===
    //
    // All seven variants below travel **only** on StreamC (per PLAN §0
    // 评审 #1 wire-compat strategy). Old daemons drop `stream_bunch.c`
    // silently (`streams.rs:291`); the new daemon never encodes these
    // events on StreamA, so pre-0.4.0 peers cannot trigger
    // `EventType::try_from(InvalidEventId)`断链.
    //
    // **Why `Clone, Debug` only** (no `Copy`): the inner payload
    // structs carry `String` / `Vec<u8>` fields and are not bitwise
    // copyable.
    /// Clipboard text payload (see [`ClipboardText`]).
    ClipboardText(ClipboardText),
    /// Clipboard image metadata (see [`ClipboardImage`]).
    ClipboardImage(ClipboardImage),
    /// Clipboard files metadata (see [`ClipboardFiles`]).
    ClipboardFiles(ClipboardFiles),
    /// File-transfer offer (see [`FileTransferOffer`]).
    FileTransferOffer(FileTransferOffer),
    /// File-transfer accept/reject (see [`FileTransferResponse`]).
    FileTransferResponse(FileTransferResponse),
    /// File-transfer cancel (see [`FileTransferCancel`]).
    FileTransferCancel(FileTransferCancel),
    /// Clipboard content request (see [`ClipboardRequest`]).
    ClipboardRequest(ClipboardRequest),
}

impl Display for ProtoEvent {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtoEvent::Enter(s) => write!(f, "Enter({s})"),
            ProtoEvent::Leave(s) => write!(f, "Leave({s})"),
            ProtoEvent::Ack(s) => write!(f, "Ack({s})"),
            ProtoEvent::Input(e) => write!(f, "{e}"),
            ProtoEvent::Ping => write!(f, "ping"),
            ProtoEvent::Pong(alive) => {
                write!(
                    f,
                    "pong: {}",
                    if *alive { "alive" } else { "not available" }
                )
            }
            ProtoEvent::Hello { magic, commit } => {
                let s = std::str::from_utf8(commit).unwrap_or("????????");
                let valid = *magic == PROTOCOL_MAGIC;
                write!(
                    f,
                    "Hello(magic={}, commit={s})",
                    if valid { "PROTOCOL_MAGIC" } else { "foreign" }
                )
            }
            ProtoEvent::ClipboardText(ct) => write!(
                f,
                "ClipboardText(fp={}, sha={}, size={}, inline={})",
                short_hex(&ct.fingerprint),
                short_hex(&ct.sha256),
                ct.size,
                if ct.content_inline.is_some() {
                    "yes"
                } else {
                    "no"
                }
            ),
            ProtoEvent::ClipboardImage(ci) => write!(
                f,
                "ClipboardImage(fp={}, sha={}, size={}, mime={})",
                short_hex(&ci.fingerprint),
                short_hex(&ci.sha256),
                ci.size,
                ci.mime
            ),
            ProtoEvent::ClipboardFiles(cf) => write!(
                f,
                "ClipboardFiles(fp={}, entries={})",
                short_hex(&cf.fingerprint),
                cf.entries.len()
            ),
            ProtoEvent::FileTransferOffer(o) => write!(
                f,
                "FileTransferOffer(sha={}, name={}, size={}, mime={})",
                short_hex(&o.sha256),
                o.name,
                o.size,
                o.mime
            ),
            ProtoEvent::FileTransferResponse(r) => write!(
                f,
                "FileTransferResponse(sha={}, accept={})",
                short_hex(&r.sha256),
                r.accept
            ),
            ProtoEvent::FileTransferCancel(c) => {
                write!(f, "FileTransferCancel(sha={})", short_hex(&c.sha256))
            }
            ProtoEvent::ClipboardRequest(r) => {
                write!(f, "ClipboardRequest(sha={})", short_hex(&r.sha256))
            }
        }
    }
}

/// Compact hex prefix for `Display` (first 4 bytes = 8 hex chars).
/// Used by `Display for ProtoEvent` so that log lines stay readable
/// while still being grep-able for a specific fingerprint.
fn short_hex(b: &[u8; 32]) -> String {
    let mut s = String::with_capacity(8);
    for byte in &b[..4] {
        s.push_str(&format!("{:02x}", byte));
    }
    s
}

#[derive(Debug, TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum EventType {
    PointerMotion,
    PointerButton,
    PointerAxis,
    PointerAxisValue120,
    KeyboardKey,
    KeyboardModifiers,
    Ping,
    Pong,
    Enter,
    Leave,
    Ack,
    Hello,
    // === PLAN-2 / M0a — variable-length codec events (StreamC only) ===
    ClipboardText,
    ClipboardImage,
    ClipboardFiles,
    FileTransferOffer,
    FileTransferResponse,
    FileTransferCancel,
    ClipboardRequest,
}

impl EventType {
    /// Is this variant a **fixed-size** codec event (encoded into
    /// `[u8; MAX_EVENT_SIZE]`)? Used by the top-level `From<ProtoEvent>
    /// for Vec<u8>` / `TryFrom<&[u8]> for ProtoEvent` dispatchers to
    /// pick the right codec path.
    ///
    /// **Invariant**: every variant listed here has a body that fits in
    /// 20 bytes (MAX_EVENT_SIZE - 1 type byte); every variant NOT
    /// listed uses the length-prefixed `VarCodec` and goes on StreamC.
    pub fn is_fixed(&self) -> bool {
        matches!(
            self,
            EventType::PointerMotion
                | EventType::PointerButton
                | EventType::PointerAxis
                | EventType::PointerAxisValue120
                | EventType::KeyboardKey
                | EventType::KeyboardModifiers
                | EventType::Ping
                | EventType::Pong
                | EventType::Enter
                | EventType::Leave
                | EventType::Ack
                | EventType::Hello
        )
    }
}

impl ProtoEvent {
    /// Construct a [`ProtoEvent::Hello`] stamped with this build's
    /// [`PROTOCOL_MAGIC`] and the given short commit hash.
    ///
    /// Used by [`crate::quic_transport::client_hello`] /
    /// [`crate::quic_transport::server_hello`] to emit the magic-bearing
    /// Hello frame on stream A — `magic` is auto-filled with `PROTOCOL_MAGIC`
    /// so callers cannot accidentally send a foreign magic on the wire.
    pub fn hello(commit: [u8; 8]) -> Self {
        ProtoEvent::Hello {
            magic: PROTOCOL_MAGIC,
            commit,
        }
    }

    fn event_type(&self) -> EventType {
        match self {
            ProtoEvent::Input(e) => match e {
                InputEvent::Pointer(p) => match p {
                    PointerEvent::Motion { .. } => EventType::PointerMotion,
                    PointerEvent::Button { .. } => EventType::PointerButton,
                    PointerEvent::Axis { .. } => EventType::PointerAxis,
                    PointerEvent::AxisDiscrete120 { .. } => EventType::PointerAxisValue120,
                },
                InputEvent::Keyboard(k) => match k {
                    KeyboardEvent::Key { .. } => EventType::KeyboardKey,
                    KeyboardEvent::Modifiers { .. } => EventType::KeyboardModifiers,
                },
            },
            ProtoEvent::Ping => EventType::Ping,
            ProtoEvent::Pong(_) => EventType::Pong,
            ProtoEvent::Enter(_) => EventType::Enter,
            ProtoEvent::Leave(_) => EventType::Leave,
            ProtoEvent::Ack(_) => EventType::Ack,
            ProtoEvent::Hello { .. } => EventType::Hello,
            ProtoEvent::ClipboardText(_) => EventType::ClipboardText,
            ProtoEvent::ClipboardImage(_) => EventType::ClipboardImage,
            ProtoEvent::ClipboardFiles(_) => EventType::ClipboardFiles,
            ProtoEvent::FileTransferOffer(_) => EventType::FileTransferOffer,
            ProtoEvent::FileTransferResponse(_) => EventType::FileTransferResponse,
            ProtoEvent::FileTransferCancel(_) => EventType::FileTransferCancel,
            ProtoEvent::ClipboardRequest(_) => EventType::ClipboardRequest,
        }
    }
}

impl TryFrom<[u8; MAX_EVENT_SIZE]> for ProtoEvent {
    type Error = ProtocolError;

    fn try_from(buf: [u8; MAX_EVENT_SIZE]) -> Result<Self, Self::Error> {
        let mut buf = &buf[..];
        let event_type = decode_u8(&mut buf)?;
        match EventType::try_from(event_type)? {
            EventType::PointerMotion => {
                Ok(Self::Input(InputEvent::Pointer(PointerEvent::Motion {
                    time: decode_u32(&mut buf)?,
                    dx: decode_f64(&mut buf)?,
                    dy: decode_f64(&mut buf)?,
                })))
            }
            EventType::PointerButton => {
                Ok(Self::Input(InputEvent::Pointer(PointerEvent::Button {
                    time: decode_u32(&mut buf)?,
                    button: decode_u32(&mut buf)?,
                    state: decode_u32(&mut buf)?,
                })))
            }
            EventType::PointerAxis => Ok(Self::Input(InputEvent::Pointer(PointerEvent::Axis {
                time: decode_u32(&mut buf)?,
                axis: decode_u8(&mut buf)?,
                value: decode_f64(&mut buf)?,
            }))),
            EventType::PointerAxisValue120 => Ok(Self::Input(InputEvent::Pointer(
                PointerEvent::AxisDiscrete120 {
                    axis: decode_u8(&mut buf)?,
                    value: decode_i32(&mut buf)?,
                },
            ))),
            EventType::KeyboardKey => Ok(Self::Input(InputEvent::Keyboard(KeyboardEvent::Key {
                time: decode_u32(&mut buf)?,
                key: decode_u32(&mut buf)?,
                state: decode_u8(&mut buf)?,
            }))),
            EventType::KeyboardModifiers => Ok(Self::Input(InputEvent::Keyboard(
                KeyboardEvent::Modifiers {
                    depressed: decode_u32(&mut buf)?,
                    latched: decode_u32(&mut buf)?,
                    locked: decode_u32(&mut buf)?,
                    group: decode_u32(&mut buf)?,
                },
            ))),
            EventType::Ping => Ok(Self::Ping),
            EventType::Pong => Ok(Self::Pong(decode_u8(&mut buf)? != 0)),
            EventType::Enter => Ok(Self::Enter(decode_u8(&mut buf)?.try_into()?)),
            EventType::Leave => Ok(Self::Leave(decode_u32(&mut buf)?)),
            EventType::Ack => Ok(Self::Ack(decode_u32(&mut buf)?)),
            EventType::Hello => {
                let mut magic = [0u8; 8];
                for b in magic.iter_mut() {
                    *b = decode_u8(&mut buf)?;
                }
                let mut commit = [0u8; 8];
                for b in commit.iter_mut() {
                    *b = decode_u8(&mut buf)?;
                }
                // Type-level decode always succeeds: any 8-byte magic
                // yields a syntactically-valid Hello. The connection
                // layer (`crate::quic_transport::client_hello` /
                // `server_hello`) is what enforces that
                // `magic == PROTOCOL_MAGIC` and rejects foreign
                // peers.
                Ok(Self::Hello { magic, commit })
            }
            // === PLAN-2 / M0a — variable-length codec events ===
            // These variants must NEVER appear in a
            // `[u8; MAX_EVENT_SIZE]` buffer — they are encoded via
            // `VarCodec` into a length-prefixed `Vec<u8>` and only
            // reach `read_frame`'s var-codec dispatcher
            // (`TryFrom<&[u8]> for ProtoEvent`). The `read_frame`
            // DoS cap (`MAX_EVENT_SIZE`) prevents a malformed peer
            // from delivering a var event with `len > MAX_EVENT_SIZE`
            // to this fixed-size decoder.
            EventType::ClipboardText
            | EventType::ClipboardImage
            | EventType::ClipboardFiles
            | EventType::FileTransferOffer
            | EventType::FileTransferResponse
            | EventType::FileTransferCancel
            | EventType::ClipboardRequest => {
                unreachable!(
                    "var-codec event delivered to fixed-size decoder; \
                     read_frame dispatcher is buggy"
                )
            }
        }
    }
}

impl From<ProtoEvent> for ([u8; MAX_EVENT_SIZE], usize) {
    fn from(event: ProtoEvent) -> Self {
        let mut buf = [0u8; MAX_EVENT_SIZE];
        let mut len = 0usize;
        {
            let mut buf = &mut buf[..];
            let buf = &mut buf;
            let len = &mut len;
            encode_u8(buf, len, event.event_type() as u8);
            match event {
                // PLAN-2 / M0a: the Input variant's body encoding goes
                // through the `FixedCodec` trait (single source of
                // truth for the byte-level encoding shared with the
                // `From<ProtoEvent> for Vec<u8>` dispatcher).
                ProtoEvent::Input(event) => {
                    let body_len = <InputEvent as FixedCodec>::encode_fixed_body(&event, buf);
                    *len += body_len;
                }
                ProtoEvent::Ping => {}
                ProtoEvent::Pong(alive) => encode_u8(buf, len, alive as u8),
                ProtoEvent::Enter(pos) => encode_u8(buf, len, pos as u8),
                ProtoEvent::Leave(serial) => encode_u32(buf, len, serial),
                ProtoEvent::Ack(serial) => encode_u32(buf, len, serial),
                ProtoEvent::Hello { magic, commit } => {
                    // magic precedes commit on the wire so the
                    // listener can short-circuit-check the magic
                    // without having to decode the commit.
                    for b in magic.iter() {
                        encode_u8(buf, len, *b);
                    }
                    for b in commit.iter() {
                        encode_u8(buf, len, *b);
                    }
                }
                // === PLAN-2 / M0a — variable-length codec events ===
                // These variants must NEVER be encoded via the
                // fixed-size `(*event).into()` path — they are
                // length-prefixed via `VarCodec` and travel only on
                // StreamC. Callers that need to send a var event
                // must use `Vec::<u8>::from(event)` instead. The
                // route_input dispatcher ensures only fixed
                // variants reach `send_input`'s `Channel::StreamA` /
                // `Channel::StreamB` arms that use this From-impl.
                v @ (ProtoEvent::ClipboardText(_)
                | ProtoEvent::ClipboardImage(_)
                | ProtoEvent::ClipboardFiles(_)
                | ProtoEvent::FileTransferOffer(_)
                | ProtoEvent::FileTransferResponse(_)
                | ProtoEvent::FileTransferCancel(_)
                | ProtoEvent::ClipboardRequest(_)) => {
                    unreachable!(
                        "var-codec event encoded via fixed-size buffer; \
                         use Vec::<u8>::from(event) instead: {v}"
                    )
                }
            }
        }
        (buf, len)
    }
}

macro_rules! decode_impl {
    ($t:ty) => {
        paste! {
            fn [<decode_ $t>](data: &mut &[u8]) -> Result<$t, ProtocolError> {
                let (int_bytes, rest) = data.split_at(size_of::<$t>());
                *data = rest;
                Ok($t::from_be_bytes(int_bytes.try_into().unwrap()))
            }
        }
    };
}

decode_impl!(u8);
decode_impl!(u32);
decode_impl!(i32);
decode_impl!(f64);

macro_rules! encode_impl {
    ($t:ty) => {
        paste! {
            fn [<encode_ $t>](buf: &mut &mut [u8], amt: &mut usize, n: $t) {
                let src = n.to_be_bytes();
                let data = std::mem::take(buf);
                let (int_bytes, rest) = data.split_at_mut(size_of::<$t>());
                int_bytes.copy_from_slice(&src);
                *amt += size_of::<$t>();
                *buf = rest
            }
        }
    };
}

encode_impl!(u8);
encode_impl!(u32);
// `encode_i32` / `encode_f64` removed (PLAN-2 / M0a: the InputEvent
// body encoding is delegated to `FixedCodec::encode_fixed_body` in
// `codec.rs`, which uses its own helpers). `decode_i32` / `decode_f64`
// below are kept for the existing `TryFrom<[u8; MAX_EVENT_SIZE]>` decoder.

// ============================================================================
//  PLAN-2 / M0a — universal dispatcher (Vec<u8> / &[u8])
// ============================================================================
//
// The fixed-size `From<ProtoEvent> for ([u8; MAX_EVENT_SIZE], usize)`
// hot path above is preserved for the existing call sites in
// `quic_transport::session` / `quic_transport::protocol` (they only
// ever feed it fixed-codec variants). For var-codec events
// (`ClipboardText` / `ClipboardImage` / `ClipboardFiles` /
// `FileTransferOffer` / `FileTransferResponse` / `FileTransferCancel` /
// `ClipboardRequest`) we provide a `Vec<u8>`-based universal
// dispatcher that allocates a vector per frame. StreamC traffic is
// low-frequency (metadata only) so the allocation cost is negligible.

impl From<ProtoEvent> for Vec<u8> {
    fn from(event: ProtoEvent) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.push(event.event_type() as u8);
        match event {
            // === Fixed-codec variants ===
            // Re-encode via the existing
            // `From<ProtoEvent> for ([u8; MAX_EVENT_SIZE], usize)`
            // impl (single source of truth for the fixed encoding)
            // and skip the type byte (already pushed above).
            fixed @ (ProtoEvent::Input(_)
            | ProtoEvent::Ping
            | ProtoEvent::Pong(_)
            | ProtoEvent::Enter(_)
            | ProtoEvent::Leave(_)
            | ProtoEvent::Ack(_)
            | ProtoEvent::Hello { .. }) => {
                let (arr, len) = <([u8; MAX_EVENT_SIZE], usize)>::from(fixed);
                // arr[0] is the type byte (same as buf[0]); skip it.
                debug_assert_eq!(arr[0], buf[0]);
                buf.extend_from_slice(&arr[1..len]);
            }
            // === Var-codec variants ===
            // Append the length-prefixed body via
            // `VarCodec::encode_var_body`. The type byte was pushed
            // above, so `encode_var_body` only writes the body.
            ProtoEvent::ClipboardText(ct) => ct.encode_var_body(&mut buf),
            ProtoEvent::ClipboardImage(ci) => ci.encode_var_body(&mut buf),
            ProtoEvent::ClipboardFiles(cf) => cf.encode_var_body(&mut buf),
            ProtoEvent::FileTransferOffer(o) => o.encode_var_body(&mut buf),
            ProtoEvent::FileTransferResponse(r) => r.encode_var_body(&mut buf),
            ProtoEvent::FileTransferCancel(c) => c.encode_var_body(&mut buf),
            ProtoEvent::ClipboardRequest(r) => r.encode_var_body(&mut buf),
        }
        buf
    }
}

/// Universal dispatcher for byte slices. Reads the type byte, then
/// routes to the fixed-codec path (`TryFrom<[u8; MAX_EVENT_SIZE]>`)
/// or the var-codec path (`VarCodec::decode_var_body`).
///
/// **Why a separate `TryFrom<&[u8]>` instead of just changing
/// `read_frame`**: the existing fixed-size `TryFrom<[u8;
/// MAX_EVENT_SIZE]>` is a public API used by `read_hello_frame` /
/// `read_frame` / `listen.rs::server_accept_bi_task` /
/// `listen.rs::server_datagram_reader_task`. Adding `TryFrom<&[u8]>`
/// alongside lets the wire-level `read_frame` use a single API for
/// both fixed and var frames (after M0a this dispatcher is the
/// canonical entry point for the wire decoder).
impl TryFrom<&[u8]> for ProtoEvent {
    type Error = ProtocolError;

    fn try_from(buf: &[u8]) -> Result<Self, Self::Error> {
        if buf.is_empty() {
            return Err(ProtocolError::EmptyInput);
        }
        let event_type = EventType::try_from(buf[0])?;
        // Skip the type byte for the body slice.
        let body = &buf[1..];
        if event_type.is_fixed() {
            // Reconstruct the full fixed-size buffer (type byte + body)
            // and reuse the fixed decoder — single source of truth for
            // the byte-level encoding. Trailing zeros are ignored by
            // the fixed decoder (each variant reads only the bytes it
            // needs). The reconstruction is critical because the
            // existing `ProtoEvent::try_from([u8; MAX_EVENT_SIZE])`
            // expects the type byte at `tmp[0]` (a 0 in `tmp[0]` would
            // silently decode as `PointerMotion`).
            let mut tmp = [0u8; MAX_EVENT_SIZE];
            tmp[0] = buf[0];
            let copy_len = body.len().min(MAX_EVENT_SIZE - 1);
            tmp[1..1 + copy_len].copy_from_slice(&body[..copy_len]);
            ProtoEvent::try_from(tmp)
        } else {
            // Var-codec dispatch. Each impl reads exactly its fields
            // and leaves the cursor pointing at any remaining bytes;
            // the trailing-bytes check below catches any extra garbage
            // after the body.
            let mut body_slice: &[u8] = body;
            let result = match event_type {
                EventType::ClipboardText => {
                    ProtoEvent::ClipboardText(ClipboardText::decode_var_body(&mut body_slice)?)
                }
                EventType::ClipboardImage => {
                    ProtoEvent::ClipboardImage(ClipboardImage::decode_var_body(&mut body_slice)?)
                }
                EventType::ClipboardFiles => {
                    ProtoEvent::ClipboardFiles(ClipboardFiles::decode_var_body(&mut body_slice)?)
                }
                EventType::FileTransferOffer => ProtoEvent::FileTransferOffer(
                    FileTransferOffer::decode_var_body(&mut body_slice)?,
                ),
                EventType::FileTransferResponse => ProtoEvent::FileTransferResponse(
                    FileTransferResponse::decode_var_body(&mut body_slice)?,
                ),
                EventType::FileTransferCancel => ProtoEvent::FileTransferCancel(
                    FileTransferCancel::decode_var_body(&mut body_slice)?,
                ),
                EventType::ClipboardRequest => ProtoEvent::ClipboardRequest(
                    ClipboardRequest::decode_var_body(&mut body_slice)?,
                ),
                // Fixed variants are handled by the `is_fixed()` branch
                // above; this arm is unreachable in practice.
                _ => unreachable!(
                    "is_fixed() returned false but event_type is fixed: {event_type:?}"
                ),
            };
            // Top-level trailing-bytes check (defends against a
            // peer that appends garbage after a complete body; the
            // per-impl check was removed because container types
            // like `FileEntry` decoded inside `Vec<FileEntry>` need
            // to leave the cursor at the next-item boundary, not at
            // a hard "empty" stop).
            if !body_slice.is_empty() {
                return Err(ProtocolError::FrameTooShort);
            }
            Ok(result)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip: encode a Hello with the protocol magic and a
    /// commit hash, then decode it. The decoder must yield the same
    /// `magic` + `commit` byte-for-byte, and the encoded length must
    /// fit in the fixed-size [`MAX_EVENT_SIZE`] buffer.
    #[test]
    fn hello_encode_decode_round_trip() {
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = ProtoEvent::Hello {
            magic: PROTOCOL_MAGIC,
            commit: *b"deadbeef",
        }
        .into();
        // 1 type byte + 8 magic + 8 commit = 17 bytes
        assert_eq!(len, 1 + size_of::<[u8; 8]>() * 2);
        assert!(len <= MAX_EVENT_SIZE);
        match buf.try_into().expect("decode") {
            ProtoEvent::Hello { magic, commit } => {
                assert_eq!(magic, PROTOCOL_MAGIC);
                assert_eq!(commit, *b"deadbeef");
            }
            other => panic!("expected Hello, got {other}"),
        }
    }

    /// Foreign / wrong magic must still decode at the type level —
    /// the connection-layer enforcement of `magic == PROTOCOL_MAGIC`
    /// is what rejects the peer (see `client_hello` / `server_hello`
    /// in `crate::quic_transport`).
    #[test]
    fn hello_wrong_magic_decodes_but_typed() {
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = ProtoEvent::Hello {
            magic: *b"WRONGMAG",
            commit: *b"deadbeef",
        }
        .into();
        assert!(len <= MAX_EVENT_SIZE);
        let decoded: ProtoEvent = buf.try_into().expect("decode");
        match decoded {
            ProtoEvent::Hello { magic, commit } => {
                assert_eq!(magic, *b"WRONGMAG");
                assert_ne!(magic, PROTOCOL_MAGIC);
                assert_eq!(commit, *b"deadbeef");
            }
            other => panic!("expected Hello, got {other}"),
        }
    }

    /// Sanity: a non-Hello event must still fit in MAX_EVENT_SIZE and
    /// round-trip (sanity for the fixed-size buffer path after Hello
    /// claims 17 of those bytes).
    #[test]
    fn ping_keeps_using_short_buffer() {
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = ProtoEvent::Ping.into();
        assert_eq!(len, 1); // type byte only
        assert!(matches!(buf.try_into().expect("decode"), ProtoEvent::Ping));
    }

    /// Sanity for the magic constant itself: it must be the exact
    /// 8-byte ASCII brand string and stay stable across versions.
    #[test]
    fn protocol_magic_is_lanmouse_ascii() {
        assert_eq!(PROTOCOL_MAGIC, *b"LANMOUSE");
        // All bytes ASCII, no embedded NUL (would terminate at
        // str::from_utf8 debug paths).
        assert!(PROTOCOL_MAGIC.iter().all(|b| b.is_ascii_graphic()));
    }

    /// `ProtoEvent::hello(commit)` must always stamp `PROTOCOL_MAGIC`
    /// on the wire regardless of the caller-supplied commit. This is
    /// the only legal way for quic_transport to build a Hello frame,
    /// so an off-by-one in the constructor would silently ship a
    /// foreign magic and break wire compatibility.
    #[test]
    fn hello_constructor_stamps_protocol_magic() {
        let event = ProtoEvent::hello(*b"deadbeef");
        match event {
            ProtoEvent::Hello { magic, commit } => {
                assert_eq!(magic, PROTOCOL_MAGIC);
                assert_eq!(commit, *b"deadbeef");
            }
            other => panic!("ProtoEvent::hello returned non-Hello: {other}"),
        }
    }

    // === PLAN-2 / M0a — top-level dispatcher tests ====================

    /// The fixed-codec `(*event).into()` hot path must still produce
    /// the **exact same bytes** as the universal `Vec<u8>` dispatcher.
    /// This is the critical wire-compat guarantee: a var-aware peer
    /// receiving a fixed event via the universal dispatcher must
    /// decode it identically to a fixed-only peer via `try_from`.
    #[test]
    fn fixed_event_bytes_match_universal_dispatcher() {
        use input_event::Event as InputEvent;
        let fixed_events = vec![
            ProtoEvent::Ping,
            ProtoEvent::Pong(true),
            ProtoEvent::Enter(Position::Left),
            ProtoEvent::Leave(42),
            ProtoEvent::Ack(99),
            ProtoEvent::Hello {
                magic: PROTOCOL_MAGIC,
                commit: *b"deadbeef",
            },
            ProtoEvent::Input(InputEvent::Pointer(PointerEvent::Motion {
                time: 1234,
                dx: 1.5,
                dy: -2.5,
            })),
            ProtoEvent::Input(InputEvent::Keyboard(KeyboardEvent::Modifiers {
                depressed: 0x01,
                latched: 0x02,
                locked: 0,
                group: 0,
            })),
        ];
        for event in fixed_events {
            let (fixed_arr, fixed_len) = <([u8; MAX_EVENT_SIZE], usize)>::from(event.clone());
            let universal: Vec<u8> = Vec::from(event.clone());
            assert_eq!(
                &fixed_arr[..fixed_len],
                &universal[..],
                "fixed path and universal Vec<u8> path must produce identical bytes for {event}"
            );
        }
    }

    /// Top-level `TryFrom<&[u8]> for ProtoEvent`: a fixed event
    /// round-trips through `From<ProtoEvent> for Vec<u8>` →
    /// `TryFrom<&[u8]>`.
    #[test]
    fn fixed_event_dispatcher_round_trip() {
        use input_event::Event as InputEvent;
        let events = vec![
            ProtoEvent::Ping,
            ProtoEvent::Pong(false),
            ProtoEvent::Enter(Position::Bottom),
            ProtoEvent::Leave(7),
            ProtoEvent::Ack(13),
            ProtoEvent::hello(*b"cafebabe"),
            ProtoEvent::Input(InputEvent::Pointer(PointerEvent::AxisDiscrete120 {
                axis: 1,
                value: -120,
            })),
        ];
        for event in events {
            let bytes: Vec<u8> = Vec::from(event.clone());
            let decoded = ProtoEvent::try_from(bytes.as_slice())
                .unwrap_or_else(|e| panic!("decode failed for {event}: {e}"));
            // Display + Debug equality — simpler than full match
            assert_eq!(
                format!("{decoded:?}"),
                format!("{event:?}"),
                "round-trip mismatch for {event}"
            );
        }
    }

    /// The canonical constructor keeps the exact 1 KiB boundary inline,
    /// while 1 KiB + 1 and larger payloads become metadata-only events.
    /// This covers the four payload sizes used by the M1b dispatcher policy.
    #[test]
    fn clipboard_text_size_policy_uses_inline_only_at_or_below_limit() {
        let cases = [
            (CLIPBOARD_TEXT_INLINE_LIMIT, true),
            (CLIPBOARD_TEXT_INLINE_LIMIT + 1, false),
            (100 * 1024, false),
            (1024 * 1024, false),
        ];

        for (size, expected_inline) in cases {
            let content = vec![0xA5; size];
            let event = ClipboardText::from_content([0x11; 32], [0x22; 32], content.clone());
            assert_eq!(event.size, size as u64);
            assert_eq!(event.is_inline(), expected_inline, "payload size: {size}");
            if expected_inline {
                assert_eq!(event.content_inline.as_deref(), Some(content.as_slice()));
            } else {
                assert!(event.content_inline.is_none(), "payload size: {size}");
            }

            let original = ProtoEvent::ClipboardText(event);
            let encoded: Vec<u8> = original.clone().into();
            let fixed_meta_len = 1 + 32 + 32 + size_of::<u64>() + 1;
            let expected_len = if expected_inline {
                fixed_meta_len + size_of::<u32>() + size
            } else {
                fixed_meta_len
            };
            assert_eq!(
                encoded.len(),
                expected_len,
                "wire payload unexpectedly retained bytes for size: {size}"
            );
            let decoded = ProtoEvent::try_from(encoded.as_slice()).expect("decode clipboard text");
            assert_eq!(
                decoded, original,
                "round-trip mismatch for payload size: {size}"
            );
        }
    }

    /// `ClipboardRequest` uses the var-codec dispatcher and preserves its
    /// 32-byte SHA-256 key on the wire. The receiver-side service uses this
    /// event in M1b.2 after registering a metadata-only text payload.
    #[test]
    fn clipboard_request_dispatcher_round_trip() {
        let original = ProtoEvent::ClipboardRequest(ClipboardRequest { sha256: [0x5A; 32] });
        let encoded: Vec<u8> = original.clone().into();
        let decoded = ProtoEvent::try_from(encoded.as_slice()).expect("decode request");
        assert_eq!(decoded, original);
    }

    /// Top-level dispatcher: a var event (ClipboardText with inline
    /// payload) round-trips through the universal `Vec<u8>` path.
    /// Pins the M0a acceptance criterion: var-codec events survive
    /// `From<ProtoEvent> for Vec<u8>` → `TryFrom<&[u8]> for ProtoEvent`.
    #[test]
    fn clipboard_text_dispatcher_round_trip() {
        let original = ProtoEvent::ClipboardText(ClipboardText {
            fingerprint: [0xab; 32],
            sha256: [0xcd; 32],
            size: 11,
            content_inline: Some(b"hello world".to_vec()),
        });
        let bytes: Vec<u8> = Vec::from(original.clone());
        let decoded = ProtoEvent::try_from(bytes.as_slice()).expect("decode");
        assert_eq!(decoded, original);
    }

    /// Top-level dispatcher: `ClipboardFiles` with multiple entries
    /// round-trips. Exercises the `read_vec_var` multi-item path
    /// through the top-level dispatcher.
    #[test]
    fn clipboard_files_dispatcher_round_trip() {
        let original = ProtoEvent::ClipboardFiles(ClipboardFiles {
            fingerprint: [0x33; 32],
            entries: vec![
                FileEntry {
                    name: "x.txt".to_string(),
                    size: 100,
                    mime: "text/plain".to_string(),
                    sha256: [0x01; 32],
                },
                FileEntry {
                    name: "y.png".to_string(),
                    size: 999,
                    mime: "image/png".to_string(),
                    sha256: [0x02; 32],
                },
            ],
        });
        let bytes: Vec<u8> = Vec::from(original.clone());
        let decoded = ProtoEvent::try_from(bytes.as_slice()).expect("decode");
        assert_eq!(decoded, original);
    }

    /// Top-level dispatcher: all 5 remaining var variants round-trip.
    #[test]
    fn all_var_variants_dispatcher_round_trip() {
        let cases = vec![
            ProtoEvent::ClipboardImage(ClipboardImage {
                fingerprint: [0x10; 32],
                mime: "image/png".to_string(),
                sha256: [0x20; 32],
                size: 4096,
            }),
            ProtoEvent::FileTransferOffer(FileTransferOffer {
                sha256: [0x99; 32],
                name: "report.pdf".to_string(),
                size: 1_234_567,
                mime: "application/pdf".to_string(),
            }),
            ProtoEvent::FileTransferResponse(FileTransferResponse {
                sha256: [0x42; 32],
                accept: true,
            }),
            ProtoEvent::FileTransferResponse(FileTransferResponse {
                sha256: [0x43; 32],
                accept: false,
            }),
            ProtoEvent::FileTransferCancel(FileTransferCancel { sha256: [0xff; 32] }),
            ProtoEvent::ClipboardRequest(ClipboardRequest { sha256: [0x55; 32] }),
            // Also the no-inline ClipboardText
            ProtoEvent::ClipboardText(ClipboardText {
                fingerprint: [0xaa; 32],
                sha256: [0xbb; 32],
                size: 999,
                content_inline: None,
            }),
        ];
        for event in cases {
            let bytes: Vec<u8> = Vec::from(event.clone());
            let decoded = ProtoEvent::try_from(bytes.as_slice())
                .unwrap_or_else(|e| panic!("decode failed for {event}: {e}"));
            assert_eq!(decoded, event, "round-trip mismatch for {event}");
        }
    }

    /// `TryFrom<&[u8]> for ProtoEvent` rejects empty input.
    #[test]
    fn empty_input_returns_error() {
        let result = ProtoEvent::try_from([].as_slice());
        assert!(matches!(result, Err(ProtocolError::EmptyInput)));
    }

    /// `TryFrom<&[u8]> for ProtoEvent` rejects an unknown event type
    /// byte (delegates to `EventType::try_from`).
    #[test]
    fn unknown_event_type_returns_error() {
        let result = ProtoEvent::try_from([0xffu8].as_slice());
        assert!(matches!(result, Err(ProtocolError::InvalidEventId(_))));
    }

    /// The top-level dispatcher catches trailing garbage after a
    /// complete var body (per-impl checks were removed for the
    /// `Vec<FileEntry>` container case).
    #[test]
    fn dispatcher_catches_trailing_garbage() {
        let original = ProtoEvent::ClipboardText(ClipboardText {
            fingerprint: [0xab; 32],
            sha256: [0xcd; 32],
            size: 0,
            content_inline: None,
        });
        let mut bytes: Vec<u8> = Vec::from(original);
        bytes.push(0xff); // stray byte
        let result = ProtoEvent::try_from(bytes.as_slice());
        assert!(
            matches!(result, Err(ProtocolError::FrameTooShort)),
            "trailing garbage must be caught by the dispatcher, got: {result:?}"
        );
    }

    /// `EventType::is_fixed` invariant: every old (M1) variant is
    /// fixed; every new (M0a) variant is var. Pins the dispatch
    /// contract.
    #[test]
    fn event_type_is_fixed_classification() {
        assert!(EventType::PointerMotion.is_fixed());
        assert!(EventType::PointerButton.is_fixed());
        assert!(EventType::PointerAxis.is_fixed());
        assert!(EventType::PointerAxisValue120.is_fixed());
        assert!(EventType::KeyboardKey.is_fixed());
        assert!(EventType::KeyboardModifiers.is_fixed());
        assert!(EventType::Ping.is_fixed());
        assert!(EventType::Pong.is_fixed());
        assert!(EventType::Enter.is_fixed());
        assert!(EventType::Leave.is_fixed());
        assert!(EventType::Ack.is_fixed());
        assert!(EventType::Hello.is_fixed());
        // Var variants:
        assert!(!EventType::ClipboardText.is_fixed());
        assert!(!EventType::ClipboardImage.is_fixed());
        assert!(!EventType::ClipboardFiles.is_fixed());
        assert!(!EventType::FileTransferOffer.is_fixed());
        assert!(!EventType::FileTransferResponse.is_fixed());
        assert!(!EventType::FileTransferCancel.is_fixed());
        assert!(!EventType::ClipboardRequest.is_fixed());
    }
}
