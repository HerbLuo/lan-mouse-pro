//! Cross-platform clipboard backend abstraction (PLAN-2 / M1a).
//!
//! This module owns the platform abstraction for reading from / writing
//! to the local OS clipboard. It exposes:
//!
//! - [`ClipboardBackend`] — the minimal trait that platform backends
//!   implement (text-only for M1a; image / file slots land in M2a / M3a).
//! - [`ClipboardError`] — backend-specific error type (thiserror).
//! - [`DummyBackend`] — an in-memory backend used by unit tests and as a
//!   last-resort fallback if the platform integration fails to initialise.
//! - [`default_backend`] — platform-aware constructor that returns the
//!   right backend for the host OS. M1a leaves the platform impls
//!   (macOS / Linux / Windows) as `NotImplemented`; the platform files
//!   `macos.rs` / `linux.rs` / `windows.rs` are introduced in STEP-1a.2
//!   and STEP-1a.3.
//!
//! Relationship with [`crate::service`]: the dispatch loop (STEP-1a.4)
//! holds a `Box<dyn ClipboardBackend>` and polls
//! [`ClipboardBackend::current_text`] every 500 ms; on change it computes
//! the fingerprint, marks the LRU, and pushes a `ProtoEvent::ClipboardText`
//! through every active `PeerSession::send_stream_c`. The inbound path
//! (`peer.run`'s `ClipboardMeta` arm → `peer.clipboard_inbox`) calls
//! [`ClipboardBackend::set_text`] on the receiver side.
//!
//! **Why text-only for M1a**: matches the milestone's "≤ 1 KiB inline"
//! gate. Image / file methods (`current_image` / `set_image` /
//! `current_files` / `set_files`) are added in M2a / M3a — adding them
//! now would dilute the trait surface and pull in image-crate deps
//! before they are needed.
//!
//! **Why a trait, not a single OS-specific struct**: tests inject
//! [`DummyBackend`] to exercise the fingerprint / LRU / push logic
//! without standing up a real platform integration. Platform backends
//! live behind the trait so the dispatcher never knows which OS it is
// running on.

// **STEP-1a.1** — only the trait + dummy + factory are wired today.
// The dispatcher (STEP-1a.4) and the platform impls (STEP-1a.2 / 1a.3)
// will use every item below; in the meantime allow dead-code to keep
// the build warning-clean for this milestone's CI matrix.
#![allow(dead_code)]

// Platform impls (one per `#[cfg(target_os = ...)]`). Each module
// contains a single struct implementing `ClipboardBackend`. The
// `default_backend()` factory below selects the right one at compile
// time so the daemon never branches on `cfg!` at runtime.
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

/// **PLAN-2 / M1b STEP-1b.2** — content-addressed outbound clipboard
/// text cache. The dispatcher writes large (> 1 KiB) clipboard text
/// bytes keyed by sha256; the HTTP/3-lite server reads from the same
/// cache to serve `GET /clipboard/text/{sha256}` from remote peers.
///
/// See [`cache::ClipboardCache`] for the full design (capacity,
/// TTL, active eviction, concurrency).
pub mod cache;

use thiserror::Error;

/// Backend-specific error for clipboard operations.
///
/// All variants carry an owned `String` so the error type is `Send +
/// Sync + 'static` (the trait objects need this). Real platform errors
/// (subprocess non-zero exit, IO failure, missing tool) all funnel
/// through one of these variants — the dispatcher logs the message and
/// either retries or skips the current tick.
#[derive(Debug, Error)]
pub enum ClipboardError {
    /// The backend tool (Linux only: `xclip` / `wl-paste`) is missing.
    /// The platform integration itself cannot recover from this — the
    /// dispatcher logs and keeps trying (in case the user installs the
    /// tool later); on macOS / Windows this variant is unreachable
    /// because the OS API is built in.
    #[error("required clipboard tool not found: {0}")]
    ToolMissing(String),
    /// Subprocess invocation failed (non-zero exit, IO error on
    /// stdout / stderr pipe, or spawn failed). Carries the raw
    /// underlying error string for log inspection.
    #[error("clipboard tool failed: {0}")]
    ToolFailed(String),
    /// The platform-specific backend is not yet implemented for this
    /// OS. Returned by [`default_backend`] on hosts where the platform
    /// file is a stub (e.g. compiling on Linux but only the macOS impl
    /// is present in a fresh checkout).
    #[error("clipboard backend not implemented for this platform")]
    NotImplemented,
    /// Generic IO / FFI failure not covered by the variants above
    /// (e.g. broken pipe to a `pbcopy` subprocess, AppKit exception).
    #[error("clipboard backend I/O error: {0}")]
    Io(String),
    /// The operation is not supported by this backend (e.g. M2a's
    /// image methods on a backend that has not yet implemented them).
    /// The trait's default [`ClipboardBackend::set_image`] impl
    /// returns this variant so platform impls can opt-in per method.
    /// The dispatcher treats this as "skip, do not log loudly" once
    /// the milestone that needs the method lands; before that, the
    /// call site never invokes the method.
    #[error("clipboard operation not supported by this backend: {0}")]
    Unsupported(String),
}

// ============================================================================
//  M2a STEP-2a.1 — image payload types + magic-byte mime detection
// ============================================================================

/// Raw image bytes returned by [`ClipboardBackend::current_image`] and
/// accepted by [`ClipboardBackend::set_image`].
///
/// **Why carry `mime` on the read path too**: the platform clipboard
/// often advertises a single canonical encoding per paste (NSPasteboard
/// defaults to PNG; Windows `CF_DIBV5` is DIB; `xclip` honours whatever
/// MIME the user pasted). We round-trip the detected / chosen MIME
/// alongside the bytes so the dispatcher never has to re-sniff the
/// format — important because a 4 K screenshot is large enough that a
/// second magic-byte scan would dwarf the rest of the work.
///
/// **`data` semantics**: the bytes are **already normalised** to the
/// `mime` field on the read path. macOS backend (STEP-2a.2) reads
/// `.tiff` + re-encodes to PNG via the `image` crate (PLAN §3 评审 #2
/// 3rd); Windows reads `CF_DIBV5` and keeps it as
/// `"application/x-dib"` (PLAN §3 评审 #4 3rd); Linux reads whatever
/// `xclip` / `wl-paste` reports. Wire-side we always send the same
/// bytes the receiver will paste — never a transcoded approximation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageBytes {
    /// MIME type of `data` (e.g. `"image/png"`, `"image/jpeg"`,
    /// `"image/bmp"`, or `"application/x-dib"` for raw Windows DIB
    /// bytes). Matches one of [`Mime::mime_str`]'s return values.
    pub mime: String,
    /// Raw encoded image bytes (PNG / JPEG / BMP / DIB).
    pub data: Vec<u8>,
}

/// Image MIME type — used by [`ClipboardBackend::set_image`] to tell
/// the backend which encoder to use. On the read path
/// ([`ClipboardBackend::current_image`]) the backend returns a
/// [`String`] (carried in [`ImageBytes::mime`]) rather than this enum
/// to accommodate platform-specific labels like
/// `"application/x-dib"` that fall outside the PNG / JPEG / BMP
/// triple and to keep the read path forward-compatible with future
/// format additions.
///
/// **Wire-compat** (PLAN §0 评审 #4 3rd): the wire mime string is the
/// exact bytes of `mime_str()` — `lan-mouse-proto::ClipboardImage::mime`
/// already stores this as a `String` so no protocol change is needed
/// to land this enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mime {
    /// `image/png` — PNG image.
    Png,
    /// `image/jpeg` — JPEG image.
    Jpeg,
    /// `image/bmp` — Windows BMP / DIB-header file.
    Bmp,
}

/// **M2b STEP-2b.1** — wire-level MIME label for raw Windows DIB
/// (Device Independent Bitmap) bytes carried inside the
/// `ClipboardImage::mime` field on the wire.
///
/// **Why a constant rather than a [`Mime`] variant**: DIB is a
/// platform-specific binary blob (BITMAPV5HEADER + pixel data,
/// sometimes BITMAPINFOHEADER) — it does not fit the PNG / JPEG /
/// BMP image-format triad the [`Mime`] enum models. The wire layer
/// (`lan-mouse-proto::ClipboardImage::mime`) is a `String` that
/// round-trips any label verbatim, so we keep `Mime` unchanged
/// (per the STEP-2b.1 boundary "不要触碰 Mime enum") and route the
/// DIB case through a dedicated [`ClipboardBackend::set_dib_image`]
/// method instead of [`ClipboardBackend::set_image`].
///
/// **Byte-level fidelity semantics**: Windows reads `CF_DIBV5`
/// pixels losslessly (the bytes are a self-describing
/// `BITMAPV5HEADER` + RGBA pixel array); macOS / Linux receivers
/// that natively support DIB can land it losslessly, while
/// receivers without native DIB support fall back to the `image`
/// crate decode + re-encode as PNG ("视觉一致" path per PLAN §3
/// STEP-2b.1 评审 #3 3rd).
pub const MIME_DIB: &str = "application/x-dib";

impl Mime {
    /// Canonical MIME label for this variant — the bytes that travel
    /// on the wire in `ClipboardImage::mime` and that backends pass to
    /// the OS clipboard APIs.
    pub const fn mime_str(self) -> &'static str {
        match self {
            Mime::Png => "image/png",
            Mime::Jpeg => "image/jpeg",
            Mime::Bmp => "image/bmp",
        }
    }

    /// Inverse of [`Self::mime_str`]. Returns `None` for any string
    /// that is not exactly one of the three known labels — unknown
    /// labels (e.g. [`MIME_DIB`]) flow through the read path
    /// unchanged without mapping to a [`Mime`] variant.
    pub fn from_label(s: &str) -> Option<Self> {
        match s {
            "image/png" => Some(Mime::Png),
            "image/jpeg" => Some(Mime::Jpeg),
            "image/bmp" => Some(Mime::Bmp),
            _ => None,
        }
    }

    /// `true` if `s` is the canonical DIB wire label.
    /// Used by [`apply_inbound_image_bytes`](crate::service::apply_inbound_image_bytes)
    /// (STEP-2b.1) to route DIB bytes to [`ClipboardBackend::set_dib_image`]
    /// without touching the [`Mime`] enum.
    pub fn is_dib_label(s: &str) -> bool {
        s == MIME_DIB
    }
}

impl std::fmt::Display for Mime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.mime_str())
    }
}

/// Sniff the MIME type of an image byte buffer from its magic bytes.
///
/// Returns:
/// - `Some(Mime::Png)` for the 8-byte PNG signature
///   (`89 50 4E 47 0D 0A 1A 0A`).
/// - `Some(Mime::Jpeg)` for the 3-byte JFIF/EXIF JPEG signature
///   (`FF D8 FF` — every standard JPEG variant starts with this
///   triple; the 4th byte distinguishes JFIF / EXIF / … but the
///   triple alone is sufficient to identify "this is a JPEG").
/// - `Some(Mime::Bmp)` for the 2-byte BMP signature
///   (`42 4D` — the ASCII letters "BM" in little-endian file order).
/// - `None` for anything else (text, empty buffer, GIF, TIFF, WebP,
///   …). Callers use the `None` case to short-circuit "this isn't an
///   image" without surfacing an error.
///
/// **Why magic-byte detection rather than trusting an OS-provided
/// label**: the platform clipboard APIs occasionally return
/// mislabeled bytes (e.g. Preview.app on macOS advertises both `.png`
/// and `.tiff` for the same paste; Windows `CF_DIBV5` is raw DIB with
/// no MIME at all). The magic byte is the only universally-correct
/// source of truth — and it costs 2-8 byte comparisons, dwarfed by the
/// rest of the clipboard read path.
///
/// **Future format additions** (GIF `47 49 46 38`, TIFF `49 49 2A 00`,
/// WebP `52 49 46 46 … 57 45 42 50`) land in a later milestone — the
/// PLAN §3 STEP-2a.1 explicitly limits M2a to PNG / JPEG / BMP.
pub fn mime_from_magic(bytes: &[u8]) -> Option<Mime> {
    // PNG: 8-byte signature
    const PNG_MAGIC: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    // JPEG: 3-byte SOI marker (FF D8) + 4th byte (FF) marks "this is a
    // JPEG segment"; every standard JPEG stream begins with this triple.
    const JPEG_MAGIC: [u8; 3] = [0xFF, 0xD8, 0xFF];
    // BMP: 2-byte "BM" signature (ASCII letters, little-endian).
    const BMP_MAGIC: [u8; 2] = [0x42, 0x4D];

    if bytes.len() >= PNG_MAGIC.len() && bytes.starts_with(&PNG_MAGIC) {
        Some(Mime::Png)
    } else if bytes.len() >= JPEG_MAGIC.len() && bytes.starts_with(&JPEG_MAGIC) {
        Some(Mime::Jpeg)
    } else if bytes.len() >= BMP_MAGIC.len() && bytes.starts_with(&BMP_MAGIC) {
        Some(Mime::Bmp)
    } else {
        None
    }
}

/// Change event emitted by [`ClipboardBackend::watch_image`] when the
/// clipboard image content changes. Mirrors the text `TextChange`
/// pattern used by the dispatcher's polling loop.
///
/// **Why a struct wrapping `ImageBytes` rather than just
/// `ImageBytes`**: leaves room for future fields (pasteboard
/// changeCount, source app attribution) without breaking the trait
/// signature — `M2a` only ships `bytes`; later milestones can extend
/// without rewriting every backend impl.
#[derive(Clone, Debug)]
pub struct ImageChange {
    pub bytes: ImageBytes,
}

/// Backend trait — text for M1a; extended with image methods for M2a.
///
/// `current_text` returns `Some(text)` if the clipboard holds UTF-8
/// text (any size, including an empty string — the empty clipboard is
/// a legitimate user state). `None` if the clipboard holds non-text
/// content (image, file list, …) or the platform read failed
/// silently. `set_text` replaces the clipboard contents; errors are
/// propagated via [`ClipboardError`].
///
/// **M1a scope**: the trait was text-only. **M2a STEP-2a.1** adds the
/// three image methods (`current_image` / `set_image` / `watch_image`)
/// with default implementations that return `None` / `Err(Unsupported)`
/// / an empty stream — letting existing text-only backends
/// (`MacOsPasteboard` / `LinuxClipboard` / `WinClipboard`) compile
/// unchanged. Per-platform image implementations land in STEP-2a.2
/// (macOS), STEP-2b.1 (Windows), and STEP-2b.2 (Linux).
///
/// **`Send` (not `Sync`)**: the trait is consumed from a single
/// `spawn_local` task on the daemon's `current_thread` runtime; the
/// dispatcher never shares the backend across tasks concurrently. `Send`
/// is required because the backend lives inside a `Box<dyn
/// ClipboardBackend>` owned by the task future.
///
/// **Why no `watch` method for text**: the trait is poll-based. The
/// dispatcher owns the 500 ms tick loop and calls `current_text` on
/// every tick, short-circuiting if the previous fingerprint matches
/// the new one. NSPasteboard's `changeCount` would let us skip the
/// read entirely on macOS, but adding platform-specific watcher
/// methods would force the trait to be `async` (and would leak
/// `NSRunLoop` / `wl_display` internals across the trait boundary).
/// The hash-based approach is good enough — the read is a single
/// subprocess invocation or a tiny AppKit call, both sub-millisecond
/// on a quiescent host.
///
/// **Why image uses `BoxStream`**: the dispatcher may want to react
/// to image changes asynchronously (e.g. a per-peer inbox task that
/// yields when no peer is ready). The default empty stream keeps the
/// interface usable from sync code (`current_image` is still the
/// primary read path) while letting platform impls that do have an
/// event source — e.g. NSPasteboard's `NSNotificationCenter` — plug
/// in a real producer later without rewriting the trait.
pub trait ClipboardBackend: Send {
    /// Human-readable backend name (e.g. `"macos-pbcopy"`,
    /// `"linux-xclip"`, `"windows-openclip"`, `"dummy"`). Logged at
    /// daemon startup so an operator can verify which backend was
    /// selected — and crucially, that a fallback was taken when the
    /// preferred backend was unavailable.
    fn name(&self) -> &str;

    /// Read the current clipboard text.
    ///
    /// Returns:
    /// - `Some(text)` if the clipboard holds UTF-8 text (any size,
    ///   including an empty string — empty clipboard is a legitimate
    ///   user state and must propagate as `Some("")`).
    /// - `None` if the clipboard holds non-text content (image, file
    ///   list, …) or if the platform read failed silently.
    ///
    /// **Performance**: called every 500 ms by the dispatcher; the
    /// implementation should be cheap. Subprocess-based backends
    /// (`xclip` / `pbcopy`) fork a process per call — for those, a
    /// fingerprint short-circuit in the dispatcher avoids the fork on
    /// quiescent ticks.
    fn current_text(&mut self) -> Option<String>;

    /// Replace the clipboard contents with `text`.
    ///
    /// Empty `text` clears the clipboard (a legitimate user state —
    /// the receiver side observes `Some("")` from `current_text`).
    /// Errors:
    /// - [`ClipboardError::ToolMissing`] if the platform tool is not
    ///   installed (Linux only — macOS / Windows have built-in APIs).
    /// - [`ClipboardError::ToolFailed`] if the subprocess failed.
    /// - [`ClipboardError::Io`] for low-level IO / FFI errors.
    fn set_text(&mut self, text: &str) -> Result<(), ClipboardError>;

    /// Whether this backend can clear the clipboard (write empty
    /// string). All M1a backends can; the default `true` is for
    /// forward-compatibility — M2a+ may add read-only backends
    /// (e.g. an "image-only" mode that explicitly refuses text
    /// writes).
    fn can_clear(&self) -> bool {
        true
    }

    // === M2a STEP-2a.1 — image methods (default impls) ===

    /// Read the current clipboard image, if any.
    ///
    /// Returns:
    /// - `Some(ImageBytes { mime, data })` if the clipboard holds an
    ///   image that the backend can serialise. `data` is the
    ///   platform-normalised byte payload for `mime` (PNG on macOS /
    ///   Linux; `application/x-dib` or PNG on Windows per PLAN §3
    ///   评审 #4 3rd).
    /// - `None` if the clipboard holds no image (text, file list, …)
    ///   **or** if the backend does not yet implement image reads.
    ///
    /// **Default returns `None`** — the M2a platform impls
    /// (`MacOsPasteboard` in STEP-2a.2, `WinClipboard` in STEP-2b.1,
    /// `LinuxClipboard` in STEP-2b.2) override this with the
    /// platform-specific read path. The default lets the existing
    /// text-only backends satisfy the trait without churn.
    fn current_image(&mut self) -> Option<ImageBytes> {
        None
    }

    /// Replace the clipboard image with `bytes` (encoded as `mime`).
    ///
    /// The dispatcher passes raw PNG / JPEG / BMP bytes that match the
    /// requested `mime`; the backend is responsible for handing them
    /// to the OS clipboard API as-is (no re-encoding). macOS uses
    /// `setData(_:forType: .png)`; Windows uses
    /// `SetClipboardData(CF_DIBV5, dib_bytes)`; Linux shells out to
    /// `xclip -selection clipboard -t image/png -i` /
    /// `wl-copy --type image/png` with `bytes` on stdin.
    ///
    /// **Default returns
    /// `Err(ClipboardError::Unsupported(...))`** — the same rationale
    /// as [`Self::current_image`]: backends that have not yet
    /// implemented image writes compile unchanged and report a clear
    /// "not implemented" message if a caller accidentally invokes
    /// `set_image` on them (the dispatcher itself only calls
    /// `set_image` once the inbound branch has selected a backend
    /// that advertises image support).
    fn set_image(&mut self, _bytes: &[u8], _mime: Mime) -> Result<(), ClipboardError> {
        Err(ClipboardError::Unsupported(
            "image write not implemented for this backend (M2a/M2b in flight)".into(),
        ))
    }

    /// **M2b STEP-2b.1** — write raw DIB bytes
    /// (`BITMAPV5HEADER` / `BITMAPINFOHEADER` + pixel data) to the
    /// platform clipboard.
    ///
    /// DIB is the platform-native Windows clipboard image format
    /// (`CF_DIBV5`). The wire carries it under the
    /// [`MIME_DIB`] label; receivers that natively support DIB
    /// (Windows itself) land the bytes byte-for-byte (preserves
    /// alpha), while receivers without native DIB support (macOS,
    /// Linux) fall back to a lossy `image`-crate decode → PNG
    /// re-encode ("视觉一致" path per PLAN §3 评审 #3 3rd).
    ///
    /// **Why a separate method (not extending [`Self::set_image`])**:
    /// the [`Mime`] enum models PNG / JPEG / BMP only and is
    /// explicitly not extended in STEP-2b.1 ("不要触碰 Mime enum").
    /// A dedicated method keeps the [`Mime`] enum stable and lets
    /// each backend opt-in to DIB support independently of the
    /// generic image-write path.
    ///
    /// **Default returns `Err(Unsupported)`** — backends that
    /// cannot land raw DIB (e.g. `DummyBackend`) inherit the default.
    /// The macOS backend's implementation goes through an
    /// NSImage round-trip spike + `image`-crate fallback
    /// (see `src/clipboard/macos.rs::set_dib_image`); the Windows
    /// backend writes the bytes directly via `SetClipboardData(
    /// CF_DIBV5, dib_bytes)`.
    ///
    /// **Caller**:
    /// [`crate::service::apply_inbound_image_bytes`] routes here
    /// when the wire `mime` string equals [`MIME_DIB`]; all other
    /// mimes still flow through [`Self::set_image`].
    fn set_dib_image(&mut self, _bytes: &[u8]) -> Result<(), ClipboardError> {
        Err(ClipboardError::Unsupported(
            "DIB image write not implemented for this backend (M2b STEP-2b.1 in flight)".into(),
        ))
    }

    /// Stream of image changes emitted by the backend (M2a+).
    ///
    /// **Default returns an empty stream** — the polling dispatcher
    /// uses [`Self::current_image`] on its 500 ms tick and does not
    /// subscribe to this stream for backends that lack a native
    /// watcher. macOS STEP-2a.2 may override this to wrap
    /// NSPasteboard's `NSNotificationCenter` for sub-tick latency;
    /// Windows / Linux STEP-2b.1 / 2b.2 likely leave the default
    /// (no native change-notification API outside macOS).
    ///
    /// **Lifetime**: `BoxStream<'static, _>` because the default impl
    /// returns an empty stream (no borrow into `&mut self`). Platform
    /// overrides are free to return shorter-lived streams if they
    /// hold internal state, but in practice every implementation
    /// either (a) uses a channel + `select!`-friendly wrapper that
    /// owns the channel, or (b) returns an empty stream.
    fn watch_image(&mut self) -> futures::stream::BoxStream<'static, ImageChange> {
        Box::pin(futures::stream::empty())
    }
}

// ============================================================================
//  DummyBackend — in-memory mock for unit tests
// ============================================================================

/// In-memory clipboard backend used by unit tests and as a final
/// fallback. Holds a single `String` and serves it back from
/// `current_text`. Never touches the OS clipboard, never spawns a
/// subprocess, never fails.
///
/// **Why a public type (not `pub(crate)`)**: the tests in
/// `src/service.rs` (added in STEP-1a.4) construct a `DummyBackend`
/// directly. Keeping it `pub` lets the public `default_backend`
/// factory also return a `DummyBackend` as a fallback when the
/// platform integration is unavailable (e.g. running in a CI container
/// without `xclip`).
pub struct DummyBackend {
    text: Option<String>,
}

impl DummyBackend {
    /// Construct an empty dummy backend. `current_text` returns `None`
    /// until [`Self::set_text`] is invoked; this mirrors the "fresh
    /// daemon" / "clipboard not yet populated" state.
    pub fn new() -> Self {
        Self { text: None }
    }

    /// Construct a dummy backend pre-populated with `text`.
    /// `current_text` returns `Some(text)` immediately.
    pub fn with_text(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
        }
    }
}

impl Default for DummyBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl ClipboardBackend for DummyBackend {
    fn name(&self) -> &str {
        "dummy"
    }

    fn current_text(&mut self) -> Option<String> {
        self.text.clone()
    }

    fn set_text(&mut self, text: &str) -> Result<(), ClipboardError> {
        self.text = Some(text.to_string());
        Ok(())
    }
}

// ============================================================================
//  default_backend — platform-aware factory
// ============================================================================

/// Construct the right [`ClipboardBackend`] for the host OS.
///
/// Dispatches at compile time:
/// - macOS → [`macos::MacOsPasteboard`] (STEP-1a.2; wraps `pbcopy` /
///   `pbpaste` subprocesses).
/// - Linux → [`linux::LinuxClipboard`] (STEP-1a.3; wraps `xclip` or
///   `wl-paste` subprocesses).
/// - Windows → [`windows::WinClipboard`] (STEP-1a.3; wraps the
///   `OpenClipboard` / `GetClipboardData` / `SetClipboardData` Win32
///   API via `windows-sys`).
///
/// On a host where no platform file exists yet (e.g. cross-compiling
/// for Windows before STEP-1a.3 lands), the function returns
/// `Err(ClipboardError::NotImplemented)` so the dispatcher can
/// surface a clear "clipboard sync is wired but the platform backend
/// is not built yet" message instead of panicking or silently
/// dropping events.
///
/// **Why `Result` and not `Option`**: a missing platform file is an
/// error condition the caller must surface; returning
/// `Err(NotImplemented)` lets the service log + continue (other
/// features stay alive) rather than pretend the clipboard is just
/// empty.
#[cfg(target_os = "macos")]
pub fn default_backend() -> Result<Box<dyn ClipboardBackend>, ClipboardError> {
    macos::MacOsPasteboard::new().map(|b| Box::new(b) as Box<dyn ClipboardBackend>)
}

#[cfg(target_os = "linux")]
pub fn default_backend() -> Result<Box<dyn ClipboardBackend>, ClipboardError> {
    linux::LinuxClipboard::new().map(|b| Box::new(b) as Box<dyn ClipboardBackend>)
}

#[cfg(target_os = "windows")]
pub fn default_backend() -> Result<Box<dyn ClipboardBackend>, ClipboardError> {
    windows::WinClipboard::new().map(|b| Box::new(b) as Box<dyn ClipboardBackend>)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub fn default_backend() -> Result<Box<dyn ClipboardBackend>, ClipboardError> {
    // FreeBSD / other unix-likes: no platform file exists. Surfaced
    // as NotImplemented so the dispatcher logs the gap.
    Err(ClipboardError::NotImplemented)
}

// ============================================================================
//  Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip: `set_text("hello")` → `current_text` returns
    /// `Some("hello")`. Pins the most basic contract of the trait —
    /// the dispatcher depends on it for the entire M1a loop.
    #[test]
    fn dummy_backend_round_trip() {
        let mut backend = DummyBackend::new();
        assert_eq!(
            backend.current_text(),
            None,
            "fresh dummy backend should be empty"
        );
        backend.set_text("hello").expect("set_text should succeed");
        assert_eq!(backend.current_text(), Some("hello".to_string()));
    }

    /// `with_text` pre-populates the backend. Useful in test fixtures
    /// that need to start with a non-empty clipboard without going
    /// through the `set_text` step.
    #[test]
    fn dummy_backend_with_text_initial_state() {
        let mut backend = DummyBackend::with_text("seed-value");
        assert_eq!(backend.current_text(), Some("seed-value".to_string()));
    }

    /// Setting the clipboard to the same text it already holds is a
    /// no-op (not an error). The dispatcher relies on this — it calls
    /// `set_text` whenever a peer pushes, even if the local clipboard
    /// already shows the same content.
    #[test]
    fn dummy_backend_set_same_text_is_noop() {
        let mut backend = DummyBackend::with_text("hello");
        backend.set_text("hello").expect("set_text ok");
        assert_eq!(backend.current_text(), Some("hello".to_string()));
    }

    /// The empty string is a legitimate clipboard state and must
    /// round-trip as `Some("")` (not `None`). The dispatcher's
    /// fingerprint short-circuit depends on this — `Some("")` and
    /// `None` produce different fingerprints.
    #[test]
    fn dummy_backend_empty_string_is_distinct_from_none() {
        let mut backend = DummyBackend::new();
        backend.set_text("").expect("set_text ok");
        assert_eq!(
            backend.current_text(),
            Some(String::new()),
            "empty string must round-trip as Some(\"\"), not None"
        );
    }

    /// `name()` is a stable contract — the daemon startup log uses it
    /// to advertise which backend was selected.
    #[test]
    fn dummy_backend_name() {
        let backend = DummyBackend::new();
        assert_eq!(backend.name(), "dummy");
    }

    /// `can_clear` defaults to `true`. M1a backends all clear;
    /// the default only matters if M2a+ introduces read-only
    /// backends.
    #[test]
    fn dummy_backend_can_clear_default_true() {
        let backend = DummyBackend::new();
        assert!(backend.can_clear());
    }

    /// `default_backend()` contract: it must not panic, and on every
    /// host the result is either `Ok(backend)` (a real platform impl
    /// after STEP-1a.2 + 1a.3) or `Err(NotImplemented)` (stub state
    /// before all three platform files land).
    ///
    /// On non-macOS hosts the platform files for Linux + Windows
    /// still need STEP-1a.3; this test pins that the factory stays
    /// well-formed (no panic, no silently-OK result) during the gap.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    #[test]
    fn default_backend_returns_not_implemented_until_all_platform_files_land() {
        let result = default_backend();
        assert!(
            matches!(result, Err(ClipboardError::NotImplemented)),
            "default_backend on linux/windows must still return NotImplemented (STEP-1a.3 pending)"
        );
    }

    /// macOS counterpart: after STEP-1a.2 the macOS impl exists, so
    /// `default_backend` returns `Ok`. We assert the name string
    /// rather than the exact type to keep the test loosely coupled to
    /// the concrete `MacOsPasteboard` struct (which can be swapped for
    /// an `objc2`-based NSPasteboard wrapper in a future milestone
    /// without breaking this contract).
    #[cfg(target_os = "macos")]
    #[test]
    fn default_backend_returns_macos_impl_after_step_1a_2() {
        let result = default_backend();
        let backend = result.expect("default_backend must return Ok on macOS after STEP-1a.2");
        assert!(
            backend.name().starts_with("macos"),
            "default_backend on macOS should return a backend whose name starts with 'macos'; got {:?}",
            backend.name()
        );
    }

    /// `ClipboardError` variants round-trip their `Display` impl —
    /// the dispatcher logs these strings, so the messages must be
    /// stable across changes (a log-grep test in M3b's manual suite
    /// relies on the exact wording).
    #[test]
    fn clipboard_error_display_messages_are_stable() {
        assert_eq!(
            ClipboardError::NotImplemented.to_string(),
            "clipboard backend not implemented for this platform"
        );
        assert_eq!(
            ClipboardError::ToolMissing("xclip".into()).to_string(),
            "required clipboard tool not found: xclip"
        );
        assert_eq!(
            ClipboardError::ToolFailed("exit 1".into()).to_string(),
            "clipboard tool failed: exit 1"
        );
        assert_eq!(
            ClipboardError::Io("broken pipe".into()).to_string(),
            "clipboard backend I/O error: broken pipe"
        );
        assert_eq!(
            ClipboardError::Unsupported("image write".into()).to_string(),
            "clipboard operation not supported by this backend: image write"
        );
    }

    // === M2a STEP-2a.1 — image mime detection + ImageBytes round-trip ===

    /// `mime_from_magic` recognises the canonical PNG / JPEG / BMP
    /// magic byte sequences. The full PNG 8-byte signature is
    /// exercised; for JPEG we pin the first 3 bytes; for BMP the
    /// 2-byte "BM" prefix. The test appends a handful of trailing
    /// bytes to ensure the detector does not require an exact length
    /// match (it should accept "this stream starts with PNG magic"
    /// regardless of how many bytes follow).
    #[test]
    fn mime_from_magic_recognises_png_jpeg_bmp() {
        // PNG: full 8-byte signature + arbitrary trailing bytes.
        let png = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00];
        assert_eq!(mime_from_magic(&png), Some(Mime::Png));

        // JPEG: FF D8 FF (SOI + first byte of any segment marker).
        let jpeg = [0xFF, 0xD8, 0xFF, 0xE0, 0x00];
        assert_eq!(mime_from_magic(&jpeg), Some(Mime::Jpeg));
        // JPEG via Exif marker (FF D8 FF E1) — also valid.
        let jpeg_exif = [0xFF, 0xD8, 0xFF, 0xE1];
        assert_eq!(mime_from_magic(&jpeg_exif), Some(Mime::Jpeg));

        // BMP: "BM" header.
        let bmp = [0x42, 0x4D, 0x46, 0x00, 0x00, 0x00];
        assert_eq!(mime_from_magic(&bmp), Some(Mime::Bmp));
    }

    /// `mime_from_magic` returns `None` for anything that is not a
    /// known image format: empty buffer (degenerate but reachable —
    /// the dispatcher sees `current_image` returning a 0-byte buffer
    /// when the platform clipboard is in an inconsistent state),
    /// pure text, and a few common non-PNG / non-JPEG / non-BMP image
    /// headers (GIF / TIFF / WebP) that are explicitly out of scope
    /// for M2a.
    #[test]
    fn mime_from_magic_returns_none_for_unknown() {
        // Empty buffer — degenerate but a possible platform state.
        assert_eq!(mime_from_magic(&[]), None);

        // Single byte (too short for any magic).
        assert_eq!(mime_from_magic(&[0xFF]), None);

        // Two bytes that are not "BM" — fails the BMP check.
        assert_eq!(mime_from_magic(&[0x42, 0x4E]), None);

        // Three bytes that are not the JPEG triple (FF D8 FF).
        assert_eq!(mime_from_magic(&[0xFF, 0xD8, 0x00]), None);

        // Plain ASCII text — fails every check.
        assert_eq!(mime_from_magic(b"hello world"), None);

        // GIF87a / GIF89a magic (`47 49 46 38`). Not in M2a scope;
        // returns None until a future milestone adds GIF support.
        let gif = b"GIF89a";
        assert_eq!(mime_from_magic(gif), None);

        // TIFF little-endian magic (`49 49 2A 00`). Same out-of-scope
        // rationale as GIF.
        let tiff = [0x49, 0x49, 0x2A, 0x00];
        assert_eq!(mime_from_magic(&tiff), None);

        // WebP magic (`52 49 46 46 … 57 45 42 50`). Same rationale.
        let webp = b"RIFF\x00\x00\x00\x00WEBP";
        assert_eq!(mime_from_magic(webp), None);
    }

    /// `ImageBytes` is a plain data struct — verify field round-trip
    /// and equality semantics. The dispatcher uses field-equality to
    /// detect "the clipboard image has not changed since the last
    /// poll", so `PartialEq` is on the hot path.
    #[test]
    fn image_bytes_can_round_trip_through_struct() {
        let bytes = ImageBytes {
            mime: "image/png".to_string(),
            data: vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00],
        };
        // Field round-trip
        assert_eq!(bytes.mime, "image/png");
        assert_eq!(bytes.data.len(), 10);
        assert_eq!(bytes.data[0], 0x89);

        // Equality is reflexive + symmetric + transitive; we exercise
        // two-equal and one-different pairings.
        let other_equal = bytes.clone();
        assert_eq!(bytes, other_equal);

        let other_diff_mime = ImageBytes {
            mime: "image/jpeg".to_string(),
            data: bytes.data.clone(),
        };
        assert_ne!(bytes, other_diff_mime);

        let other_diff_data = ImageBytes {
            mime: bytes.mime.clone(),
            data: vec![0xFF, 0xD8, 0xFF],
        };
        assert_ne!(bytes, other_diff_data);

        // Empty data is a legitimate value (the dispatcher's fingerprint
        // short-circuit uses `is_empty()` rather than checking for
        // `None`).
        let empty = ImageBytes {
            mime: "image/png".to_string(),
            data: Vec::new(),
        };
        assert!(empty.data.is_empty());
        assert_ne!(empty, bytes, "empty data must not equal non-empty data");
    }

    /// `DummyBackend` (the test mock) inherits the trait-level default
    /// impls for image methods. `current_image` returns `None` (the
    /// dispatcher treats that as "no image on the clipboard") and
    /// `set_image` returns `Err(Unsupported)` (a clear "this mock is
    /// text-only" signal — useful for unit tests that want to assert
    /// "the dispatcher did not call `set_image` on a backend that
    /// doesn't support it").
    #[test]
    fn dummy_backend_current_image_returns_none_by_default() {
        let mut backend = DummyBackend::new();
        assert_eq!(
            backend.current_image(),
            None,
            "DummyBackend inherits the default impl (no image on clipboard)"
        );
        // Pre-populating the text side does not accidentally populate
        // an image — they are independent.
        backend.set_text("hello").expect("set_text ok");
        assert_eq!(
            backend.current_image(),
            None,
            "set_text must not be conflated with current_image"
        );
    }

    /// `DummyBackend::set_image` returns `Err(ClipboardError::Unsupported)`
    /// via the default impl. Pin the error variant (not the message
    /// text — that is asserted in `clipboard_error_display_messages_are_stable`)
    /// so a future backend impl that adds image support does not
    /// silently change the dispatcher's error-handling branch.
    #[test]
    fn dummy_backend_set_image_returns_unsupported() {
        let mut backend = DummyBackend::new();
        let png_bytes = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let result = backend.set_image(&png_bytes, Mime::Png);
        assert!(
            matches!(result, Err(ClipboardError::Unsupported(_))),
            "DummyBackend::set_image must default to Err(Unsupported); got {result:?}"
        );
    }

    /// `DummyBackend::set_dib_image` also returns
    /// `Err(ClipboardError::Unsupported)` via the trait default
    /// impl. STEP-2b.1 pins the default behaviour for non-DIB-aware
    /// backends so the dispatcher can rely on a stable error
    /// variant when the platform backend has not opted in.
    #[test]
    fn dummy_backend_set_dib_image_returns_unsupported() {
        let mut backend = DummyBackend::new();
        let result = backend.set_dib_image(&[0x00, 0x01, 0x02]);
        assert!(
            matches!(result, Err(ClipboardError::Unsupported(_))),
            "DummyBackend::set_dib_image must default to Err(Unsupported); got {result:?}"
        );
    }

    /// `MIME_DIB` is the wire-format label the dispatcher routes to
    /// [`ClipboardBackend::set_dib_image`]. Pin the exact string
    /// (`"application/x-dib"`) because `lan-mouse-proto::
    /// ClipboardImage::mime` carries these bytes verbatim and a
    /// typo would silently break cross-platform DIB passthrough.
    #[test]
    fn mime_dib_constant_is_stable() {
        assert_eq!(MIME_DIB, "application/x-dib");
        // `Mime::is_dib_label` is the routing predicate the
        // dispatcher uses — pin its truth table.
        assert!(Mime::is_dib_label("application/x-dib"));
        assert!(!Mime::is_dib_label("image/png"));
        assert!(!Mime::is_dib_label("image/jpeg"));
        assert!(!Mime::is_dib_label("image/bmp"));
        assert!(!Mime::is_dib_label(""));
        assert!(!Mime::is_dib_label("application/x-DIB"));
    }

    /// `Mime::mime_str` returns the exact wire-format label for each
    /// variant. Pin the strings because the protocol layer
    /// (`lan-mouse-proto::ClipboardImage::mime`) and the dispatcher's
    /// outbound / inbound branches both key on these exact labels —
    /// a typo here would silently break cross-device image sync.
    #[test]
    fn mime_str_returns_canonical_wire_labels() {
        assert_eq!(Mime::Png.mime_str(), "image/png");
        assert_eq!(Mime::Jpeg.mime_str(), "image/jpeg");
        assert_eq!(Mime::Bmp.mime_str(), "image/bmp");
        // Display impl must match (so the error / log formatting
        // stays consistent across the codebase).
        assert_eq!(format!("{}", Mime::Png), "image/png");
        assert_eq!(format!("{}", Mime::Jpeg), "image/jpeg");
        assert_eq!(format!("{}", Mime::Bmp), "image/bmp");
    }

    /// `Mime::from_label` round-trips each `mime_str()` value and
    /// rejects unknown labels. The dispatcher's outbound branch uses
    /// this to validate an incoming `ClipboardImage::mime` before
    /// routing it to `set_image` (an unknown label should not
    /// silently map to `None` and silently drop the push — instead
    /// the dispatcher should surface a "format not supported" log
    /// and skip the push).
    #[test]
    fn mime_from_label_round_trips_known_values() {
        assert_eq!(Mime::from_label("image/png"), Some(Mime::Png));
        assert_eq!(Mime::from_label("image/jpeg"), Some(Mime::Jpeg));
        assert_eq!(Mime::from_label("image/bmp"), Some(Mime::Bmp));

        // Unknown / out-of-scope labels (incl. DIB, GIF, TIFF, WebP)
        // must return None — never silently map to a variant.
        assert_eq!(Mime::from_label("application/x-dib"), None);
        assert_eq!(Mime::from_label("image/gif"), None);
        assert_eq!(Mime::from_label("image/tiff"), None);
        assert_eq!(Mime::from_label("image/webp"), None);
        assert_eq!(Mime::from_label(""), None);
        // Case sensitivity — `image/PNG` is NOT `image/png`. The
        // platform APIs are case-sensitive on macOS / Linux;
        // accepting mixed case here would silently mis-route.
        assert_eq!(Mime::from_label("image/PNG"), None);
    }
}
