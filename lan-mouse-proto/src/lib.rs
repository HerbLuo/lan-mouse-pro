use input_event::{Event as InputEvent, KeyboardEvent, PointerEvent};
use num_enum::{IntoPrimitive, TryFromPrimitive, TryFromPrimitiveError};
use paste::paste;
use std::{
    fmt::{Debug, Display, Formatter},
    mem::size_of,
};
use thiserror::Error;

/// defines the maximum size an encoded event can take up
/// this is currently the pointer motion event
/// type: u8, time: u32, dx: f64, dy: f64
pub const MAX_EVENT_SIZE: usize = size_of::<u8>() + size_of::<u32>() + 2 * size_of::<f64>();

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
}

/// Position of a client
#[derive(Clone, Copy, Debug, TryFromPrimitive, IntoPrimitive)]
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

/// main lan-mouse protocol event type
#[derive(Clone, Copy, Debug)]
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
        }
    }
}

#[derive(TryFromPrimitive, IntoPrimitive)]
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
                ProtoEvent::Input(event) => match event {
                    InputEvent::Pointer(p) => match p {
                        PointerEvent::Motion { time, dx, dy } => {
                            encode_u32(buf, len, time);
                            encode_f64(buf, len, dx);
                            encode_f64(buf, len, dy);
                        }
                        PointerEvent::Button {
                            time,
                            button,
                            state,
                        } => {
                            encode_u32(buf, len, time);
                            encode_u32(buf, len, button);
                            encode_u32(buf, len, state);
                        }
                        PointerEvent::Axis { time, axis, value } => {
                            encode_u32(buf, len, time);
                            encode_u8(buf, len, axis);
                            encode_f64(buf, len, value);
                        }
                        PointerEvent::AxisDiscrete120 { axis, value } => {
                            encode_u8(buf, len, axis);
                            encode_i32(buf, len, value);
                        }
                    },
                    InputEvent::Keyboard(k) => match k {
                        KeyboardEvent::Key { time, key, state } => {
                            encode_u32(buf, len, time);
                            encode_u32(buf, len, key);
                            encode_u8(buf, len, state);
                        }
                        KeyboardEvent::Modifiers {
                            depressed,
                            latched,
                            locked,
                            group,
                        } => {
                            encode_u32(buf, len, depressed);
                            encode_u32(buf, len, latched);
                            encode_u32(buf, len, locked);
                            encode_u32(buf, len, group);
                        }
                    },
                },
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
encode_impl!(i32);
encode_impl!(f64);

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
}
