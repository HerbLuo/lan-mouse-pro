//! Codec dual-track for `lan-mouse-proto` (PLAN-2 / M0a STEP-0.0+0.1).
//!
//! The protocol now has two coexisting wire codecs selected by variant:
//!
//! | Codec        | Variants                                                                                 | Wire framing                        |
//! |--------------|------------------------------------------------------------------------------------------|--------------------------------------|
//! | `FixedCodec` | `InputEvent` (and the trivial `Ping` / `Pong` / `Enter` / `Leave` / `Ack` / `Hello`)     | `[u8 type][...body...]` into `[u8; MAX_EVENT_SIZE]` |
//! | `VarCodec`   | `ClipboardText` / `ClipboardImage` / `ClipboardFiles` / `FileTransferOffer` / `FileTransferResponse` / `FileTransferCancel` / `ClipboardRequest` | `[u8 type][...length-prefixed fields...]` into `Vec<u8>` |
//!
//! The top-level dispatcher in `lib.rs` (`From<ProtoEvent> for Vec<u8>`
//! and `TryFrom<&[u8]> for ProtoEvent`) `match`-routes each variant to
//! the correct codec. The pre-existing `(*event).into()` call sites in
//! `quic_transport::session` / `quic_transport::protocol` continue to
//! use the `From<ProtoEvent> for ([u8; MAX_EVENT_SIZE], usize)` hot path
//! for `Input` / `Ping` / `Pong` / `Enter` / `Leave` / `Ack` / `Hello` —
//! no behavioural change. The new `Vec<u8>` path is reserved for the
//! clipboard / file-transfer variants on `StreamC` (PLAN §0 评审 #1
//! wire-compat strategy).
//!
//! **Why a trait split** (vs. one mega-`From`): the two codec families
//! have fundamentally different framing (`[u8; N]` vs. length-prefixed
//! `Vec<u8>`), so a single signature would either force `Vec<u8>` on the
//! hot path (extra allocation per keystroke) or force a fixed-size cap
//! on clipboard payloads (cannot grow beyond `MAX_EVENT_SIZE`).
//! Splitting by variant preserves the hot-path zero-allocation while
//! unlocking variable-size frames.

use input_event::Event as InputEvent;
use input_event::{KeyboardEvent, PointerEvent};

use crate::{
    ClipboardFiles, ClipboardImage, ClipboardRequest, ClipboardText, FileEntry, FileTransferCancel,
    FileTransferOffer, FileTransferResponse, ProtocolError,
};

/// Fixed-size codec for hot-path variants.
///
/// **Wire format**: `[u8 event_type][...body bytes...]` packed into a
/// `[u8; MAX_EVENT_SIZE]` (21 bytes) buffer. Body length is implicit
/// (each variant has a known fixed body size). The `event_type` byte
/// is written separately by the top-level `ProtoEvent` dispatcher — this
/// trait only encodes the body bytes that follow it.
///
/// **Implementor**: `InputEvent` (pointer / keyboard events). The other
/// fixed variants (`Ping` / `Pong` / `Enter` / `Leave` / `Ack` / `Hello`)
/// have such trivial bodies that they are inlined in `lib.rs` rather
/// than going through this trait — they each encode 0-16 bytes.
pub trait FixedCodec: Sized {
    /// Encode the body (excluding the type byte) into `buf`. Returns
    /// the number of bytes written. `buf.len()` must be ≥ `MAX_EVENT_SIZE - 1`.
    fn encode_fixed_body(&self, buf: &mut [u8]) -> usize;
}

/// Variable-length codec for clipboard / file-transfer events.
///
/// **Wire format**: `[u8 event_type][...length-prefixed fields...]`
/// packed into a `Vec<u8>`. Each `String` / `Vec<u8>` field is preceded
/// by a `u32 BE` length prefix; each fixed-size field
/// (`[u8; 32]` / `u64` / `u8`) is written raw (big-endian for integers).
/// The `event_type` byte is written separately by the top-level
/// `ProtoEvent` dispatcher — this trait only encodes the body that
/// follows it.
///
/// **Implementors**: `ClipboardText` / `ClipboardImage` /
/// `ClipboardFiles` / `FileTransferOffer` / `FileTransferResponse` /
/// `FileTransferCancel` / `ClipboardRequest`.
pub trait VarCodec: Sized {
    /// Encode the body (excluding the type byte) into `buf` (append).
    fn encode_var_body(&self, buf: &mut Vec<u8>);

    /// Decode the body (excluding the type byte) from `buf`. Advances
    /// the cursor to the first byte after the consumed body. The
    /// caller is responsible for verifying the cursor is empty (or for
    /// skipping any trailing bytes — `ClipboardFiles` uses this to
    /// sequentially decode `FileEntry` items).
    fn decode_var_body(buf: &mut &[u8]) -> Result<Self, ProtocolError>;
}

// ============================================================================
//  FixedCodec for InputEvent
// ============================================================================

impl FixedCodec for InputEvent {
    fn encode_fixed_body(&self, buf: &mut [u8]) -> usize {
        let mut len = 0;
        macro_rules! w_u8 {
            ($n:expr) => {
                buf[len] = $n as u8;
                len += 1;
            };
        }
        macro_rules! w_u32 {
            ($n:expr) => {
                buf[len..len + 4].copy_from_slice(&($n as u32).to_be_bytes());
                len += 4;
            };
        }
        macro_rules! w_i32 {
            ($n:expr) => {
                buf[len..len + 4].copy_from_slice(&($n as i32).to_be_bytes());
                len += 4;
            };
        }
        macro_rules! w_f64 {
            ($n:expr) => {
                buf[len..len + 8].copy_from_slice(&($n as f64).to_be_bytes());
                len += 8;
            };
        }
        match self {
            InputEvent::Pointer(p) => match p {
                PointerEvent::Motion { time, dx, dy } => {
                    w_u32!(*time);
                    w_f64!(*dx);
                    w_f64!(*dy);
                }
                PointerEvent::Button {
                    time,
                    button,
                    state,
                } => {
                    w_u32!(*time);
                    w_u32!(*button);
                    w_u32!(*state);
                }
                PointerEvent::Axis { time, axis, value } => {
                    w_u32!(*time);
                    w_u8!(*axis);
                    w_f64!(*value);
                }
                PointerEvent::AxisDiscrete120 { axis, value } => {
                    w_u8!(*axis);
                    w_i32!(*value);
                }
            },
            InputEvent::Keyboard(k) => match k {
                KeyboardEvent::Key { time, key, state } => {
                    w_u32!(*time);
                    w_u32!(*key);
                    w_u8!(*state);
                }
                KeyboardEvent::Modifiers {
                    depressed,
                    latched,
                    locked,
                    group,
                } => {
                    w_u32!(*depressed);
                    w_u32!(*latched);
                    w_u32!(*locked);
                    w_u32!(*group);
                }
            },
        }
        len
    }
}

// ============================================================================
//  VarCodec helpers (private to this module)
// ============================================================================

fn write_hash(buf: &mut Vec<u8>, h: &[u8; 32]) {
    buf.extend_from_slice(h);
}

fn read_hash(buf: &mut &[u8]) -> Result<[u8; 32], ProtocolError> {
    if buf.len() < 32 {
        return Err(ProtocolError::FrameTooShort);
    }
    let (h, rest) = buf.split_at(32);
    *buf = rest;
    Ok(h.try_into().unwrap())
}

fn write_u64_be(buf: &mut Vec<u8>, n: u64) {
    buf.extend_from_slice(&n.to_be_bytes());
}

fn read_u64_be(buf: &mut &[u8]) -> Result<u64, ProtocolError> {
    if buf.len() < 8 {
        return Err(ProtocolError::FrameTooShort);
    }
    let (n, rest) = buf.split_at(8);
    *buf = rest;
    Ok(u64::from_be_bytes(n.try_into().unwrap()))
}

fn write_bool(buf: &mut Vec<u8>, b: bool) {
    buf.push(b as u8);
}

fn read_bool(buf: &mut &[u8]) -> Result<bool, ProtocolError> {
    if buf.is_empty() {
        return Err(ProtocolError::FrameTooShort);
    }
    let (b, rest) = buf.split_at(1);
    *buf = rest;
    Ok(b[0] != 0)
}

fn write_string(buf: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    buf.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    buf.extend_from_slice(bytes);
}

fn read_string(buf: &mut &[u8]) -> Result<String, ProtocolError> {
    if buf.len() < 4 {
        return Err(ProtocolError::FrameTooShort);
    }
    let (len_bytes, rest) = buf.split_at(4);
    *buf = rest;
    let len = u32::from_be_bytes(len_bytes.try_into().unwrap()) as usize;
    if buf.len() < len {
        return Err(ProtocolError::FrameTooShort);
    }
    let (s_bytes, rest) = buf.split_at(len);
    *buf = rest;
    String::from_utf8(s_bytes.to_vec()).map_err(|_| ProtocolError::FrameTooShort)
}

fn write_bytes(buf: &mut Vec<u8>, b: &[u8]) {
    buf.extend_from_slice(&(b.len() as u32).to_be_bytes());
    buf.extend_from_slice(b);
}

fn read_bytes(buf: &mut &[u8]) -> Result<Vec<u8>, ProtocolError> {
    if buf.len() < 4 {
        return Err(ProtocolError::FrameTooShort);
    }
    let (len_bytes, rest) = buf.split_at(4);
    *buf = rest;
    let len = u32::from_be_bytes(len_bytes.try_into().unwrap()) as usize;
    if buf.len() < len {
        return Err(ProtocolError::FrameTooShort);
    }
    let (b, rest) = buf.split_at(len);
    *buf = rest;
    Ok(b.to_vec())
}

/// Write `Vec<T>` where each `T` is `VarCodec`-encoded. Layout:
/// `[u32 BE total_body_len][item_1 body][item_2 body]...`. Each item
/// is decoded independently by the reader; the length prefix lets the
/// reader slice off the body block in O(1) without parsing through all
/// items.
fn write_vec_var<T: VarCodec>(buf: &mut Vec<u8>, items: &[T]) {
    let len_pos = buf.len();
    buf.extend_from_slice(&[0u8; 4]);
    let body_start = buf.len();
    for item in items {
        item.encode_var_body(buf);
    }
    let body_len = (buf.len() - body_start) as u32;
    buf[len_pos..len_pos + 4].copy_from_slice(&body_len.to_be_bytes());
}

fn read_vec_var<T: VarCodec>(buf: &mut &[u8]) -> Result<Vec<T>, ProtocolError> {
    if buf.len() < 4 {
        return Err(ProtocolError::FrameTooShort);
    }
    let (len_bytes, rest) = buf.split_at(4);
    *buf = rest;
    let total_len = u32::from_be_bytes(len_bytes.try_into().unwrap()) as usize;
    if buf.len() < total_len {
        return Err(ProtocolError::FrameTooShort);
    }
    // Split off the body block; we decode items inside it sequentially.
    let (mut body, after) = buf.split_at(total_len);
    let mut items = Vec::new();
    while !body.is_empty() {
        let item = T::decode_var_body(&mut body)?;
        items.push(item);
    }
    *buf = after;
    Ok(items)
}

// ============================================================================
//  VarCodec impls
// ============================================================================

// ClipboardText body: [u8; 32 fingerprint][u8; 32 sha256][u64 BE size][u8 has_inline][u32 BE inline_len][inline bytes]
impl VarCodec for ClipboardText {
    fn encode_var_body(&self, buf: &mut Vec<u8>) {
        write_hash(buf, &self.fingerprint);
        write_hash(buf, &self.sha256);
        write_u64_be(buf, self.size);
        match &self.content_inline {
            Some(bytes) => {
                buf.push(1);
                write_bytes(buf, bytes);
            }
            None => buf.push(0),
        }
    }

    fn decode_var_body(buf: &mut &[u8]) -> Result<Self, ProtocolError> {
        let fingerprint = read_hash(buf)?;
        let sha256 = read_hash(buf)?;
        let size = read_u64_be(buf)?;
        let has_inline = read_bool(buf)?;
        let content_inline = if has_inline {
            Some(read_bytes(buf)?)
        } else {
            None
        };
        Ok(ClipboardText {
            fingerprint,
            sha256,
            size,
            content_inline,
        })
    }
}

// ClipboardImage body: [u8; 32 fingerprint][u32 BE mime_len][mime bytes][u8; 32 sha256][u64 BE size]
impl VarCodec for ClipboardImage {
    fn encode_var_body(&self, buf: &mut Vec<u8>) {
        write_hash(buf, &self.fingerprint);
        write_string(buf, &self.mime);
        write_hash(buf, &self.sha256);
        write_u64_be(buf, self.size);
    }

    fn decode_var_body(buf: &mut &[u8]) -> Result<Self, ProtocolError> {
        let fingerprint = read_hash(buf)?;
        let mime = read_string(buf)?;
        let sha256 = read_hash(buf)?;
        let size = read_u64_be(buf)?;
        Ok(ClipboardImage {
            fingerprint,
            mime,
            sha256,
            size,
        })
    }
}

// FileEntry body: [u32 BE name_len][name bytes][u64 BE size][u32 BE mime_len][mime bytes][u8; 32 sha256]
//
// **No trailing-bytes check** (unlike single-variant impls above):
// `FileEntry` is decoded sequentially by `read_vec_var` for
// `ClipboardFiles`, where the surrounding `total_len` prefix bounds
// the parent body. The check is enforced at the top-level dispatcher.
impl VarCodec for FileEntry {
    fn encode_var_body(&self, buf: &mut Vec<u8>) {
        write_string(buf, &self.name);
        write_u64_be(buf, self.size);
        write_string(buf, &self.mime);
        write_hash(buf, &self.sha256);
    }

    fn decode_var_body(buf: &mut &[u8]) -> Result<Self, ProtocolError> {
        let name = read_string(buf)?;
        let size = read_u64_be(buf)?;
        let mime = read_string(buf)?;
        let sha256 = read_hash(buf)?;
        Ok(FileEntry {
            name,
            size,
            mime,
            sha256,
        })
    }
}

// ClipboardFiles body: [u8; 32 fingerprint][u32 BE entries_total_len][FileEntry_1 body][FileEntry_2 body]...
impl VarCodec for ClipboardFiles {
    fn encode_var_body(&self, buf: &mut Vec<u8>) {
        write_hash(buf, &self.fingerprint);
        write_vec_var(buf, &self.entries);
    }

    fn decode_var_body(buf: &mut &[u8]) -> Result<Self, ProtocolError> {
        let fingerprint = read_hash(buf)?;
        let entries = read_vec_var::<FileEntry>(buf)?;
        Ok(ClipboardFiles {
            fingerprint,
            entries,
        })
    }
}

// FileTransferOffer body: [u8; 32 sha256][u32 BE name_len][name bytes][u64 BE size][u32 BE mime_len][mime bytes]
impl VarCodec for FileTransferOffer {
    fn encode_var_body(&self, buf: &mut Vec<u8>) {
        write_hash(buf, &self.sha256);
        write_string(buf, &self.name);
        write_u64_be(buf, self.size);
        write_string(buf, &self.mime);
    }

    fn decode_var_body(buf: &mut &[u8]) -> Result<Self, ProtocolError> {
        let sha256 = read_hash(buf)?;
        let name = read_string(buf)?;
        let size = read_u64_be(buf)?;
        let mime = read_string(buf)?;
        Ok(FileTransferOffer {
            sha256,
            name,
            size,
            mime,
        })
    }
}

// FileTransferResponse body: [u8; 32 sha256][u8 accept]
impl VarCodec for FileTransferResponse {
    fn encode_var_body(&self, buf: &mut Vec<u8>) {
        write_hash(buf, &self.sha256);
        write_bool(buf, self.accept);
    }

    fn decode_var_body(buf: &mut &[u8]) -> Result<Self, ProtocolError> {
        let sha256 = read_hash(buf)?;
        let accept = read_bool(buf)?;
        Ok(FileTransferResponse { sha256, accept })
    }
}

// FileTransferCancel body: [u8; 32 sha256]
impl VarCodec for FileTransferCancel {
    fn encode_var_body(&self, buf: &mut Vec<u8>) {
        write_hash(buf, &self.sha256);
    }

    fn decode_var_body(buf: &mut &[u8]) -> Result<Self, ProtocolError> {
        let sha256 = read_hash(buf)?;
        Ok(FileTransferCancel { sha256 })
    }
}

// ClipboardRequest body: [u8; 32 sha256]
impl VarCodec for ClipboardRequest {
    fn encode_var_body(&self, buf: &mut Vec<u8>) {
        write_hash(buf, &self.sha256);
    }

    fn decode_var_body(buf: &mut &[u8]) -> Result<Self, ProtocolError> {
        let sha256 = read_hash(buf)?;
        Ok(ClipboardRequest { sha256 })
    }
}

// ============================================================================
//  Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Trait skeleton compile-time sanity: the trait is importable and
    /// the bound resolves for `InputEvent` (FixedCodec) and all 8 var
    /// implementors. This is the "empty trait 即可" compile-time smoke
    /// test from STEP-0.0.
    #[test]
    fn trait_codecs_are_usable() {
        fn _check_fixed<T: FixedCodec>() {}
        fn _check_var<T: VarCodec>() {}
        // Fixed: only `InputEvent` for now.
        _check_fixed::<InputEvent>();
        // Var: all 8 implementors.
        _check_var::<ClipboardText>();
        _check_var::<ClipboardImage>();
        _check_var::<FileEntry>();
        _check_var::<ClipboardFiles>();
        _check_var::<FileTransferOffer>();
        _check_var::<FileTransferResponse>();
        _check_var::<FileTransferCancel>();
        _check_var::<ClipboardRequest>();
    }

    /// Var codec round-trip: a fully-populated `ClipboardText` with
    /// inline content survives encode → decode byte-equivalent.
    #[test]
    fn clipboard_text_with_inline_round_trip() {
        let fp = [0xab; 32];
        let sha = [0xcd; 32];
        let content = b"hello world".to_vec();
        let original = ClipboardText {
            fingerprint: fp,
            sha256: sha,
            size: content.len() as u64,
            content_inline: Some(content.clone()),
        };
        let mut buf = Vec::new();
        original.encode_var_body(&mut buf);
        let decoded = ClipboardText::decode_var_body(&mut &buf[..]).expect("decode");
        assert_eq!(decoded.fingerprint, fp);
        assert_eq!(decoded.sha256, sha);
        assert_eq!(decoded.size, content.len() as u64);
        assert_eq!(decoded.content_inline, Some(content));
    }

    /// Var codec round-trip: `ClipboardText` without inline payload.
    #[test]
    fn clipboard_text_no_inline_round_trip() {
        let original = ClipboardText {
            fingerprint: [1u8; 32],
            sha256: [2u8; 32],
            size: 12345,
            content_inline: None,
        };
        let mut buf = Vec::new();
        original.encode_var_body(&mut buf);
        let decoded = ClipboardText::decode_var_body(&mut &buf[..]).expect("decode");
        assert_eq!(decoded.fingerprint, original.fingerprint);
        assert_eq!(decoded.sha256, original.sha256);
        assert_eq!(decoded.size, original.size);
        assert_eq!(decoded.content_inline, None);
    }

    /// Var codec round-trip: `ClipboardImage`.
    #[test]
    fn clipboard_image_round_trip() {
        let original = ClipboardImage {
            fingerprint: [0x10; 32],
            mime: "image/png".to_string(),
            sha256: [0x20; 32],
            size: 4096,
        };
        let mut buf = Vec::new();
        original.encode_var_body(&mut buf);
        let decoded = ClipboardImage::decode_var_body(&mut &buf[..]).expect("decode");
        assert_eq!(decoded.fingerprint, original.fingerprint);
        assert_eq!(decoded.mime, original.mime);
        assert_eq!(decoded.sha256, original.sha256);
        assert_eq!(decoded.size, original.size);
    }

    /// Var codec round-trip: `FileTransferOffer`.
    #[test]
    fn file_transfer_offer_round_trip() {
        let original = FileTransferOffer {
            sha256: [0x99; 32],
            name: "report.pdf".to_string(),
            size: 1_234_567,
            mime: "application/pdf".to_string(),
        };
        let mut buf = Vec::new();
        original.encode_var_body(&mut buf);
        let decoded = FileTransferOffer::decode_var_body(&mut &buf[..]).expect("decode");
        assert_eq!(decoded.sha256, original.sha256);
        assert_eq!(decoded.name, original.name);
        assert_eq!(decoded.size, original.size);
        assert_eq!(decoded.mime, original.mime);
    }

    /// Var codec round-trip: `FileTransferResponse` (true).
    #[test]
    fn file_transfer_response_accept_round_trip() {
        let original = FileTransferResponse {
            sha256: [0x42; 32],
            accept: true,
        };
        let mut buf = Vec::new();
        original.encode_var_body(&mut buf);
        let decoded = FileTransferResponse::decode_var_body(&mut &buf[..]).expect("decode");
        assert_eq!(decoded.sha256, original.sha256);
        assert!(decoded.accept);
    }

    /// Var codec round-trip: `FileTransferResponse` (false).
    #[test]
    fn file_transfer_response_reject_round_trip() {
        let original = FileTransferResponse {
            sha256: [0x42; 32],
            accept: false,
        };
        let mut buf = Vec::new();
        original.encode_var_body(&mut buf);
        let decoded = FileTransferResponse::decode_var_body(&mut &buf[..]).expect("decode");
        assert!(!decoded.accept);
    }

    /// Var codec round-trip: `FileTransferCancel`.
    #[test]
    fn file_transfer_cancel_round_trip() {
        let original = FileTransferCancel { sha256: [0xff; 32] };
        let mut buf = Vec::new();
        original.encode_var_body(&mut buf);
        let decoded = FileTransferCancel::decode_var_body(&mut &buf[..]).expect("decode");
        assert_eq!(decoded.sha256, original.sha256);
    }

    /// Var codec round-trip: `ClipboardRequest`.
    #[test]
    fn clipboard_request_round_trip() {
        let original = ClipboardRequest { sha256: [0x55; 32] };
        let mut buf = Vec::new();
        original.encode_var_body(&mut buf);
        let decoded = ClipboardRequest::decode_var_body(&mut &buf[..]).expect("decode");
        assert_eq!(decoded.sha256, original.sha256);
    }

    /// Var codec round-trip: `FileEntry`.
    #[test]
    fn file_entry_round_trip() {
        let original = FileEntry {
            name: "data.bin".to_string(),
            size: 1024,
            mime: "application/octet-stream".to_string(),
            sha256: [0x77; 32],
        };
        let mut buf = Vec::new();
        original.encode_var_body(&mut buf);
        let decoded = FileEntry::decode_var_body(&mut &buf[..]).expect("decode");
        assert_eq!(decoded.name, original.name);
        assert_eq!(decoded.size, original.size);
        assert_eq!(decoded.mime, original.mime);
        assert_eq!(decoded.sha256, original.sha256);
    }

    /// Var codec round-trip: `ClipboardFiles` with multiple entries —
    /// exercises `read_vec_var` (multi-element sequential decode).
    #[test]
    fn clipboard_files_multi_entry_round_trip() {
        let original = ClipboardFiles {
            fingerprint: [0x33; 32],
            entries: vec![
                FileEntry {
                    name: "a.txt".to_string(),
                    size: 100,
                    mime: "text/plain".to_string(),
                    sha256: [0x01; 32],
                },
                FileEntry {
                    name: "b.png".to_string(),
                    size: 2048,
                    mime: "image/png".to_string(),
                    sha256: [0x02; 32],
                },
                FileEntry {
                    name: "c.bin".to_string(),
                    size: 999_999,
                    mime: "application/octet-stream".to_string(),
                    sha256: [0x03; 32],
                },
            ],
        };
        let mut buf = Vec::new();
        original.encode_var_body(&mut buf);
        let decoded = ClipboardFiles::decode_var_body(&mut &buf[..]).expect("decode");
        assert_eq!(decoded.fingerprint, original.fingerprint);
        assert_eq!(decoded.entries.len(), original.entries.len());
        assert_eq!(decoded.entries[0].name, "a.txt");
        assert_eq!(decoded.entries[1].name, "b.png");
        assert_eq!(decoded.entries[2].name, "c.bin");
        assert_eq!(decoded.entries[0].sha256, [0x01; 32]);
        assert_eq!(decoded.entries[1].sha256, [0x02; 32]);
        assert_eq!(decoded.entries[2].sha256, [0x03; 32]);
        assert_eq!(decoded.entries[2].size, 999_999);
    }

    /// Truncated decode: feeding a half-truncated body returns an
    /// error rather than panic.
    #[test]
    fn truncated_body_returns_error() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&[0xab; 32]); // fingerprint only — too short
        let result = ClipboardText::decode_var_body(&mut &buf[..]);
        assert!(result.is_err(), "truncated body must return Err");
    }

    /// Extra trailing bytes after a complete body are NOT detected by
    /// the per-impl `decode_var_body` (each impl leaves the cursor at
    /// the next-byte boundary). The check lives in the top-level
    /// dispatcher in `lib.rs::TryFrom<&[u8]> for ProtoEvent` —
    /// exercised by the `lib.rs::tests` block.
    ///
    /// **This test pins the contract**: `decode_var_body` returns
    /// `Ok` even with trailing bytes (the cursor points at them).
    #[test]
    fn decode_var_body_leaves_cursor_on_trailing_bytes() {
        let original = ClipboardText {
            fingerprint: [0xab; 32],
            sha256: [0xcd; 32],
            size: 0,
            content_inline: None,
        };
        let mut buf = Vec::new();
        original.encode_var_body(&mut buf);
        buf.push(0xff); // stray byte
        let mut slice = buf.as_slice();
        let decoded =
            ClipboardText::decode_var_body(&mut slice).expect("decode_var_body itself succeeds");
        assert_eq!(decoded.fingerprint, original.fingerprint);
        assert_eq!(decoded.sha256, original.sha256);
        // Cursor points at the stray byte — the top-level dispatcher
        // surfaces this as a FrameTooShort error.
        assert_eq!(slice.len(), 1);
        assert_eq!(slice[0], 0xff);
    }
}
