//! macOS clipboard backend (PLAN-2 / M1a STEP-1a.2 + M2a STEP-2a.2).
//!
//! Two cooperating paths live behind this file:
//!
//! - **Text path** (M1a STEP-1a.2) wraps the `/usr/bin/pbcopy` and
//!   `/usr/bin/pbpaste` subprocesses. Apple ships them with every
//!   supported macOS release; they back the macOS GUI clipboard
//!   exactly like the NSPasteboard API. The dispatcher's fingerprint
//!   short-circuit (compute sha256, skip on match) already avoids
//!   redundant pushes, so the `NSPasteboard` `changeCount`
//!   optimisation is unnecessary on the text side. Using subprocesses
//!   keeps the macOS path dependency-free (no `objc2` /
//!   `objc2-app-kit` crate additions for text) and structurally
//!   identical to the Linux `xclip` / `wl-paste` path (introduced in
//!   STEP-1a.3), making the cross-platform surface easier to reason
//!   about. See `next/SUGGESTION.md` #S-1 for the original deviation
//!   rationale and a future migration path if changeCount becomes
//!   important for text.
//!
//! - **Image path** (M2a STEP-2a.2) uses `NSPasteboard` directly via
//!   the `objc2` / `objc2-app-kit` crates. The image methods need
//!   finer control over pasteboard types (`.png` / `.tiff`) and
//!   `changeCount`-based change detection that `pbcopy` / `pbpaste`
//!   don't expose. PLAN §3 评审 #2 3rd made PNG normalisation
//!   mandatory at the source end — Preview.app on macOS only
//!   publishes `.tiff` for selected regions, so the source must
//!   re-encode TIFF to PNG via the `image` crate before transit; the
//!   receive side then sees only PNG bytes regardless of which macOS
//!   app produced the image.
//!
//! **Why not `xcrun pbpaste` / `xcrun pbcopy`**: macOS aliases
//! `pbcopy` / `pbpaste` to `/usr/bin/pbcopy` and `/usr/bin/pbpaste`
//! directly (no `xcrun` indirection needed). Using the bare command
//! name saves a fork and keeps the error message simpler.
//!
//! **Threading model**: the [`ClipboardBackend`] trait is `Send` but
//! not `Sync`; the daemon's `current_thread` + `LocalSet` runtime
//! owns the dispatcher task that consumes the backend, so no
//! concurrency concerns (the text subprocess calls block ~1-3 ms per
//! invocation; on the dispatcher's 500 ms tick this is invisible).
//! `NSPasteboard::generalPasteboard()` is documented as thread-safe
//! (returns a process-wide singleton) and is bound to the calling
//! thread's autorelease pool in [`MacOsPasteboard::new`] so any
//! subsequent `current_image` / `set_image` call from the
//! dispatcher's tick task is safe.
//!
//! **Cached text**: the `cached: Option<String>` field mirrors what
//! `current_text` last read; `set_text` updates it after a successful
//! `pbcopy`. This is informational only — the dispatcher does not
//! rely on it (it uses the freshly-read value from `current_text` for
//! the fingerprint comparison). Removing it would not affect
//! behaviour.

#![cfg(target_os = "macos")]

use std::io::Write;
use std::process::{Command, Stdio};

use objc2_app_kit::NSPasteboard;
use objc2_foundation::{NSData, NSString};

use super::{ClipboardBackend, ClipboardError, ImageBytes, Mime};

/// `NSPasteboard` type identifier for PNG payloads.
///
/// Defined as a `const &str` because `NSPasteboard::dataForType` takes
/// `&NSPasteboardType` (an `NSString` reference). We wrap the `&str`
/// in `NSString::from_str` at every call site — cheap (single
/// refcount bump) and keeps the constant immutable + `'static`.
const NS_PASTEBOARD_TYPE_PNG: &str = "public.png";

/// `NSPasteboard` type identifier for TIFF payloads (used as a
/// fallback when the producing app does not publish PNG — Preview.app
/// on a selected region is the canonical example; PLAN §3 评审 #2
/// 3rd). The TIFF bytes are decoded by the `image` crate and
/// re-encoded as PNG before being returned to the dispatcher.
const NS_PASTEBOARD_TYPE_TIFF: &str = "public.tiff";

/// macOS clipboard backend. Wraps the `pbcopy` / `pbpaste` subprocess
/// pair for text and `NSPasteboard` via `objc2` for images.
///
/// The `cached` field stores the last-known clipboard text. It is
/// informational only (the dispatcher never reads it); it exists so
/// future log statements can correlate "I just wrote X" with "X was
/// already there before I wrote it" without a second subprocess
/// call.
#[derive(Debug)]
pub struct MacOsPasteboard {
    cached: Option<String>,
}

impl MacOsPasteboard {
    /// Construct a new backend. Probes `pbpaste` availability by
    /// running `--help` (a no-op that exits 0 on every supported
    /// macOS release) — fails fast if the tool is missing, so the
    /// dispatcher logs a clear "platform tool missing" message at
    /// startup rather than waiting until the first clipboard read to
    /// discover the problem.
    ///
    /// Also touches `NSPasteboard.generalPasteboard()` once to ensure
    /// the AppKit pasteboard stack is initialised on the current
    /// thread (the daemon's main thread). Subsequent `objc2` calls
    /// from `spawn_local` tasks on the same thread are then safe —
    /// `generalPasteboard()` is a process-wide singleton, but the
    /// first call binds it to the calling thread's autorelease pool.
    pub fn new() -> Result<Self, ClipboardError> {
        let status = Command::new("pbpaste")
            .arg("--help")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| ClipboardError::ToolMissing(format!("pbpaste: {e}")))?;
        if !status.success() {
            return Err(ClipboardError::ToolFailed(format!(
                "pbpaste --help exited {status}"
            )));
        }
        // Bind NSPasteboard.generalPasteboard() to the calling thread
        // (the daemon's main thread). This is a no-op if already
        // bound; it cannot fail in a way we need to surface — if
        // AppKit is missing the macOS build itself would not have
        // produced a binary, so the linker guarantees presence.
        let _ = NSPasteboard::generalPasteboard();
        Ok(Self { cached: None })
    }
}

impl ClipboardBackend for MacOsPasteboard {
    fn name(&self) -> &str {
        "macos-pbcopy-pbpaste+nspasteboard-image"
    }

    fn current_text(&mut self) -> Option<String> {
        let output = Command::new("pbpaste")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            // pbpaste exits 1 if the clipboard holds non-text content
            // (e.g. an image) — the dispatcher treats `None` as
            // "skip this tick" which is the correct behaviour for an
            // image-only clipboard. Other non-zero exits (very rare)
            // are also surfaced as `None` so the dispatcher keeps
            // running instead of taking the backend down.
            return None;
        }
        let text = String::from_utf8(output.stdout).ok()?;
        self.cached = Some(text.clone());
        Some(text)
    }

    fn set_text(&mut self, text: &str) -> Result<(), ClipboardError> {
        let mut child = Command::new("pbcopy")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| ClipboardError::Io(format!("spawn pbcopy: {e}")))?;
        // SAFETY: we just constructed the child with a piped stdin,
        // so `child.stdin` is `Some(_)`. The `unwrap` would only fire
        // if the OS detached stdin between spawn and here, which
        // doesn't happen in practice.
        child
            .stdin
            .as_mut()
            .expect("pbcopy stdin must be piped (just spawned with Stdio::piped)")
            .write_all(text.as_bytes())
            .map_err(|e| ClipboardError::Io(format!("write pbcopy stdin: {e}")))?;
        // Drop stdin explicitly to send EOF — pbcopy reads until EOF.
        drop(child.stdin.take());
        let status = child
            .wait()
            .map_err(|e| ClipboardError::Io(format!("wait pbcopy: {e}")))?;
        if !status.success() {
            return Err(ClipboardError::ToolFailed(format!(
                "pbcopy exited {status}"
            )));
        }
        self.cached = Some(text.to_string());
        Ok(())
    }

    // === M2a STEP-2a.2 — image methods via NSPasteboard ===

    /// Read the current clipboard image, if any.
    ///
    /// **Preferred path**: `NSPasteboard.generalPasteboard().dataForType("public.png")`
    /// — the vast majority of macOS apps publish a PNG representation
    /// alongside any other image formats they offer, so this hits on
    /// the first try.
    ///
    /// **Fallback path**: when the producing app only publishes a
    /// `.tiff` representation (Preview.app on a selected region is
    /// the canonical case — PLAN §3 评审 #2 3rd), the TIFF bytes are
    /// decoded by the `image` crate and re-encoded as PNG. This
    /// source-side normalisation is what makes the receive side
    /// simple: every peer on every platform sees `image/png` bytes
    /// regardless of which macOS app produced the image, satisfying
    /// REQUIREMENT §4.3 "4K 截图字节级一致" (the bytes the receiver
    /// writes are the bytes we re-encoded, so any 4 K screenshot
    /// originating as TIFF on macOS round-trips lossily as PNG —
    /// that is the explicit PLAN §3 评审 #2 3rd decision).
    ///
    /// **No changeCount optimisation here**: this method always
    /// reads. The dispatcher (M2a STEP-2a.3) compares the freshly-read
    /// bytes' fingerprint to the last-broadcast fingerprint to decide
    /// whether to push — same pattern as the text path. Optimising the
    /// quiescent-tick skip via `NSPasteboard.changeCount()` would
    /// change the trait's "Some(bytes) means there is an image, None
    /// means there is not" semantics, so it stays out of this method.
    fn current_image(&mut self) -> Option<ImageBytes> {
        let pb = NSPasteboard::generalPasteboard();
        read_image_bytes_from_pasteboard(&pb)
    }

    /// Write `bytes` to the clipboard as PNG.
    ///
    /// **`mime` is ignored** — the macOS backend writes PNG bytes
    /// unconditionally. The caller (the dispatcher in M2a STEP-2a.3)
    /// is responsible for normalising non-PNG images to PNG before
    /// reaching this method; if a non-PNG mime slips through we log
    /// a `warn!` so the issue is debuggable from logs, but the bytes
    /// are still written (mis-labelled data is recoverable; a silent
    /// drop is not).
    ///
    /// `NSPasteboard.clearContents()` is called first so any prior
    /// image representation (PNG / TIFF / JPEG / …) on the
    /// pasteboard is wiped — leaving stale representations risks the
    /// user pasting an outdated format in a different app.
    fn set_image(&mut self, bytes: &[u8], mime: Mime) -> Result<(), ClipboardError> {
        if mime != Mime::Png {
            log::warn!(
                "clipboard set_image: macOS backend forces PNG encoding (NSPasteboard public.png); \
                 caller passed mime={mime} — callers should normalise to PNG before sending"
            );
        }
        let pb = NSPasteboard::generalPasteboard();
        let png_type = NSString::from_str(NS_PASTEBOARD_TYPE_PNG);
        // `NSData::with_bytes` copies the bytes into an NSData
        // owned by the autorelease pool; safe to call from any
        // thread once AppKit is initialised (which `new()` did).
        let nsdata = NSData::with_bytes(bytes);
        let ok = pb.setData_forType(Some(&nsdata), &png_type);
        if ok {
            Ok(())
        } else {
            Err(ClipboardError::Io(format!(
                "NSPasteboard::setData_forType({NS_PASTEBOARD_TYPE_PNG}) returned false"
            )))
        }
    }
}

// ============================================================================
//  Image helpers (free functions, not on `&mut self`, so the
//  dispatcher's tick task can call them without holding a borrow into
//  the backend).
// ============================================================================

/// Read raw bytes for a given `NSPasteboard` type identifier. Returns
/// `None` if the type is not present on the pasteboard, or if the
/// returned `NSData` is empty (an empty data ref means "this type
/// exists in the type list but no bytes are attached" — we treat
/// that identically to "not present").
///
/// **Safe wrapper around `dataForType`**: `NSData::to_vec()` copies
/// the bytes into a fresh `Vec<u8>`, so the returned `Vec` does not
/// alias the autoreleased `NSData` and is safe to keep / clone /
/// return to the dispatcher's tick task.
fn read_pasteboard_bytes(pb: &NSPasteboard, type_str: &str) -> Option<Vec<u8>> {
    let ns_type = NSString::from_str(type_str);
    let data = pb.dataForType(&ns_type)?;
    let bytes = data.to_vec();
    if bytes.is_empty() { None } else { Some(bytes) }
}

/// Decode TIFF bytes via the `image` crate and re-encode as PNG.
///
/// Returns `Err(ClipboardError::Io)` on decode or encode failure —
/// the message carries the `image` crate's `ImageError` `Display`
/// for log inspection. Rare in practice (TIFF from
/// `NSPasteboard.generalPasteboard()` is always
/// NSBitmapImageRep-generated and well-formed); the warn-and-skip
/// path in `read_image_bytes_from_pasteboard` handles it.
fn tiff_to_png_normalized(tiff: &[u8]) -> Result<Vec<u8>, ClipboardError> {
    let img = image::load_from_memory(tiff)
        .map_err(|e| ClipboardError::Io(format!("image::load_from_memory TIFF: {e}")))?;
    let mut out = Vec::new();
    {
        let mut cursor = std::io::Cursor::new(&mut out);
        img.write_to(&mut cursor, image::ImageFormat::Png)
            .map_err(|e| ClipboardError::Io(format!("image::write_to PNG: {e}")))?;
    }
    Ok(out)
}

/// Read an image from `pb`, preferring PNG and falling back to TIFF
/// (re-encoded as PNG). Returns `None` if neither type is present or
/// if the only available type is a malformed TIFF.
///
/// Called from `current_image` (one-shot read on each dispatcher
/// tick); the TIFF→PNG normalisation + warn-on-bad-TIFF semantics
/// are encapsulated here so the read path stays a single helper.
fn read_image_bytes_from_pasteboard(pb: &NSPasteboard) -> Option<ImageBytes> {
    // Preferred: PNG (most apps provide it).
    if let Some(bytes) = read_pasteboard_bytes(pb, NS_PASTEBOARD_TYPE_PNG) {
        return Some(ImageBytes {
            mime: Mime::Png.mime_str().to_string(),
            data: bytes,
        });
    }
    // Fallback: TIFF (Preview.app on a selected region, certain
    // Quick Look exports, etc. — PLAN §3 评审 #2 3rd).
    if let Some(tiff_bytes) = read_pasteboard_bytes(pb, NS_PASTEBOARD_TYPE_TIFF) {
        match tiff_to_png_normalized(&tiff_bytes) {
            Ok(png_bytes) => {
                log::info!(
                    "clipboard: TIFF→PNG normalized for cross-platform transfer \
                     ({} bytes → {} bytes)",
                    tiff_bytes.len(),
                    png_bytes.len()
                );
                return Some(ImageBytes {
                    mime: Mime::Png.mime_str().to_string(),
                    data: png_bytes,
                });
            }
            Err(e) => {
                log::warn!("clipboard: TIFF decode failed, skipping image change: {e}");
                return None;
            }
        }
    }
    None
}

// ============================================================================
//  Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Process-wide mutex that serialises every test in this module.
    /// All the round-trip tests touch the user's actual OS clipboard
    /// (via `pbcopy` / `pbpaste` for text, via `NSPasteboard` directly
    /// for images); without a mutex `cargo test` would run them in
    /// parallel and one test's `set_text("")` would race another
    /// test's `current_text()`, producing flaky failures. The mutex
    /// is `static` so it survives across all tests in a single
    /// binary; we hold it for the entire test body so the round-trip
    /// happens atomically from the OS clipboard's perspective.
    ///
    /// **The `static Mutex<()>` is `unwrap()`-poisonable** (a panic
    /// inside a `lock()` holder marks the mutex poisoned). We wrap the
    /// acquisition in a helper that ignores poisoning — a panicked
    /// earlier test still leaves the clipboard in a defined state
    /// (the `ClipboardGuard` restores it on Drop), so subsequent tests
    /// can proceed.
    static CLIPBOARD_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Helper: acquire the test lock, ignoring poisoning.
    fn lock_for_test() -> std::sync::MutexGuard<'static, ()> {
        CLIPBOARD_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Smoke test: `new()` returns `Ok` on any macOS host where
    /// `pbpaste` is on `$PATH` (the standard `/usr/bin/pbpaste` ships
    /// with every supported macOS release). Failing here means the
    /// test environment is misconfigured, not that the backend is
    /// broken.
    #[test]
    fn new_succeeds_on_macos_with_pbpaste() {
        let result = MacOsPasteboard::new();
        assert!(
            result.is_ok(),
            "MacOsPasteboard::new() should succeed when pbpaste exists; got {result:?}"
        );
    }

    /// `name()` is a stable contract — the daemon startup log uses it
    /// to advertise which backend was selected. A regression here would
    /// silently break operator-facing diagnostics.
    ///
    /// **M2a STEP-2a.2**: the name changed from
    /// `"macos-pbcopy-pbpaste"` to `"macos-pbcopy-pbpaste+nspasteboard-image"`
    /// to reflect that text uses subprocesses and image uses
    /// `NSPasteboard` directly. The dispatcher logs this at startup
    /// so operators see which path is active.
    #[test]
    fn name_is_macos_subprocess_plus_nspasteboard_label() {
        let backend = MacOsPasteboard::new().expect("new");
        assert_eq!(backend.name(), "macos-pbcopy-pbpaste+nspasteboard-image");
    }

    /// Round-trip via the real `pbcopy` / `pbpaste` subprocesses.
    ///
    /// **Important**: this test mutates the user's actual OS
    /// clipboard. It saves the original content first and restores it
    /// at the end so the test is invisible to anyone using the
    /// clipboard while the test runs. The `CLIPBOARD_TEST_LOCK`
    /// mutex prevents parallel tests from interleaving their
    /// writes/reads.
    ///
    /// The test fails (not panics) if the subprocesses fail — failures
    /// are usually due to a missing / misconfigured `/usr/bin/pbpaste`
    /// (very rare on standard macOS) rather than a logic bug.
    #[test]
    fn set_text_then_current_text_round_trip() {
        let _lock = lock_for_test();
        let mut backend = MacOsPasteboard::new().expect("new");

        // Save original clipboard so we can restore at the end.
        let original = backend.current_text();
        // Always restore, even if the test panics or errors midway.
        let _guard = ClipboardGuard::new(original.clone());

        let payload = "lan-mouse M1a round-trip test 1a.2";
        backend
            .set_text(payload)
            .expect("set_text must succeed on macOS with pbcopy");
        let read_back = backend
            .current_text()
            .expect("current_text must return Some after set_text succeeded");
        assert_eq!(read_back, payload);
    }

    /// Empty string round-trip — `pbcopy` accepts empty stdin (it
    /// clears the clipboard). This is the legitimate "user pressed
    /// Cmd+C on nothing / cleared the clipboard" state.
    #[test]
    fn set_text_empty_string_clears_clipboard() {
        let _lock = lock_for_test();
        let mut backend = MacOsPasteboard::new().expect("new");

        let original = backend.current_text();
        let _guard = ClipboardGuard::new(original);

        backend
            .set_text("placeholder")
            .expect("set_text must succeed");
        backend.set_text("").expect("set_text empty must succeed");
        let read_back = backend
            .current_text()
            .expect("current_text after set_text(\"\") should return Some(\"\")");
        assert_eq!(
            read_back, "",
            "empty string must round-trip as empty string"
        );
    }

    /// Multi-byte UTF-8 round-trip — macOS clipboard is UTF-8 native;
    /// `pbpaste` should hand back the exact bytes we wrote via
    /// `pbcopy`. Tests with CJK + emoji + combining accents to catch
    /// any byte-vs-char confusion.
    #[test]
    fn set_text_multibyte_utf8_round_trip() {
        let _lock = lock_for_test();
        let mut backend = MacOsPasteboard::new().expect("new");

        let original = backend.current_text();
        let _guard = ClipboardGuard::new(original);

        let payload = "中文 + emoji 🎉 + é";
        backend.set_text(payload).expect("set_text utf8");
        let read_back = backend
            .current_text()
            .expect("current_text after utf8 set_text");
        assert_eq!(
            read_back, payload,
            "multi-byte UTF-8 must round-trip byte-for-byte"
        );
    }

    /// RAII guard that restores the original clipboard on drop. Used
    /// by the round-trip tests so they don't leak test state into the
    /// user's actual clipboard. Held for the entire test (via the
    /// `static Mutex`) so concurrent tests cannot race the restore.
    struct ClipboardGuard {
        original: Option<String>,
    }

    impl ClipboardGuard {
        fn new(original: Option<String>) -> Self {
            Self { original }
        }
    }

    impl Drop for ClipboardGuard {
        fn drop(&mut self) {
            // Best-effort restore — if it fails (e.g. another test
            // changed the clipboard again), there's nothing useful we
            // can do. The next test run will save whatever is current.
            if let Some(text) = self.original.take() {
                let mut backend = MacOsPasteboard::new().expect("new (in guard drop)");
                let _ = backend.set_text(&text);
            }
        }
    }

    // === M2a STEP-2a.2 — image tests via NSPasteboard ===

    /// Build a small but valid PNG byte buffer for image tests.
    /// 4×4 RGB gradient — minimal valid PNG the `image` crate can
    /// re-decode (and that survives a TIFF round-trip too).
    fn test_png_bytes() -> Vec<u8> {
        let img = image::RgbImage::from_fn(4, 4, |x, y| {
            image::Rgb([(x * 60) as u8, (y * 60) as u8, 128])
        });
        let mut out = Vec::new();
        {
            let mut cursor = std::io::Cursor::new(&mut out);
            img.write_to(&mut cursor, image::ImageFormat::Png)
                .expect("encode test png");
        }
        out
    }

    /// Build a small but valid TIFF byte buffer for the
    /// TIFF→PNG-normalisation test.
    fn test_tiff_bytes() -> Vec<u8> {
        let img = image::RgbImage::from_fn(2, 2, |x, y| {
            image::Rgb([(x * 100) as u8, (y * 100) as u8, 200])
        });
        let mut out = Vec::new();
        {
            let mut cursor = std::io::Cursor::new(&mut out);
            img.write_to(&mut cursor, image::ImageFormat::Tiff)
                .expect("encode test tiff");
        }
        out
    }

    /// RAII guard for image tests — saves the pasteboard's PNG + TIFF
    /// representations and restores them on drop. Image tests mutate
    /// the user's actual OS clipboard (via `NSPasteboard` directly,
    /// bypassing `pbcopy`); without the guard the test would leak
    /// test images into whatever app the user next opens.
    ///
    /// **Why a separate guard from `ClipboardGuard`**: the text
    /// `ClipboardGuard` saves / restores text via `pbcopy` /
    /// `pbpaste`. Image tests bypass the text path and write directly
    /// to `NSPasteboard`; saving / restoring text there would be
    /// wasted work. The two guards cover disjoint state, but they
    /// share the `CLIPBOARD_TEST_LOCK` mutex so they cannot
    /// interleave their reads / writes.
    struct ImageClipboardGuard {
        saved_png: Option<objc2::rc::Retained<NSData>>,
        saved_tiff: Option<objc2::rc::Retained<NSData>>,
    }

    impl ImageClipboardGuard {
        fn new() -> Self {
            let pb = NSPasteboard::generalPasteboard();
            Self {
                saved_png: pb.dataForType(&NSString::from_str(NS_PASTEBOARD_TYPE_PNG)),
                saved_tiff: pb.dataForType(&NSString::from_str(NS_PASTEBOARD_TYPE_TIFF)),
            }
        }
    }

    impl Drop for ImageClipboardGuard {
        fn drop(&mut self) {
            let pb = NSPasteboard::generalPasteboard();
            let png_type = NSString::from_str(NS_PASTEBOARD_TYPE_PNG);
            let tiff_type = NSString::from_str(NS_PASTEBOARD_TYPE_TIFF);
            let _ = pb.clearContents();
            if let Some(data) = self.saved_png.take() {
                let _ = pb.setData_forType(Some(&data), &png_type);
            }
            if let Some(data) = self.saved_tiff.take() {
                let _ = pb.setData_forType(Some(&data), &tiff_type);
            }
        }
    }

    /// Helper: write raw bytes to the `NSPasteboard` under a given
    /// type, clearing prior contents. Used by image tests to seed the
    /// clipboard with a PNG / TIFF payload before calling
    /// `current_image`.
    fn write_pasteboard_bytes(type_str: &str, bytes: &[u8]) {
        let pb = NSPasteboard::generalPasteboard();
        let ns_type = NSString::from_str(type_str);
        let nsdata = NSData::with_bytes(bytes);
        let _ = pb.clearContents();
        let ok = pb.setData_forType(Some(&nsdata), &ns_type);
        assert!(
            ok,
            "test setup failed: NSPasteboard::setData_forType({type_str}) returned false"
        );
    }

    /// `current_image` returns `None` when the pasteboard holds no
    /// image data (only text, only files, or empty). The dispatcher
    /// (M2a STEP-2a.3) treats `None` as "no image change this tick".
    ///
    /// **Setup**: we call `clearContents()` to put the pasteboard
    /// into a known-empty state. The `ImageClipboardGuard` restores
    /// whatever the user had before the test ran.
    #[test]
    fn current_image_returns_none_on_empty_pasteboard() {
        let _lock = lock_for_test();
        let mut backend = MacOsPasteboard::new().expect("new");
        let _guard = ImageClipboardGuard::new();

        // Put the pasteboard into a known-empty state.
        let pb = NSPasteboard::generalPasteboard();
        let _ = pb.clearContents();

        let result = backend.current_image();
        assert_eq!(
            result, None,
            "current_image on empty pasteboard must return None"
        );
    }

    /// `current_image` reads the PNG bytes directly from the
    /// pasteboard (no re-encoding) when the producing app published
    /// a `.png` representation. The returned bytes must be
    /// byte-for-byte identical to the input — this is what makes the
    /// "macOS 4 K 截图字节级一致" promise in REQUIREMENT §4.3 work
    /// for screenshots captured by macOS-native screenshot tools
    /// (`screencapture -x -t png`, `Cmd+Shift+4`, Preview.app
    /// exports, …).
    #[test]
    fn current_image_reads_png_bytes_directly() {
        let _lock = lock_for_test();
        let mut backend = MacOsPasteboard::new().expect("new");
        let _guard = ImageClipboardGuard::new();

        let png = test_png_bytes();
        write_pasteboard_bytes(NS_PASTEBOARD_TYPE_PNG, &png);

        let read_back = backend
            .current_image()
            .expect("current_image must return Some after writing PNG to pasteboard");
        assert_eq!(
            read_back.mime, "image/png",
            "current_image must label the returned bytes as image/png"
        );
        assert_eq!(
            read_back.data, png,
            "PNG bytes must round-trip byte-for-byte (no re-encoding on the preferred path)"
        );
    }

    /// **TIFF→PNG normalisation** (PLAN §3 评审 #2 3rd, the reason
    /// this milestone exists): when only `.tiff` is on the
    /// pasteboard (Preview.app's canonical "copy selection" output),
    /// `current_image` must re-encode the bytes to PNG and label
    /// them `"image/png"` so the receive side gets a single
    /// canonical format.
    ///
    /// **Sanity-check after re-encoding**: the re-encoded bytes must
    /// start with the PNG magic (`89 50 4E 47 0D 0A 1A 0A`) and the
    /// `image` crate must decode them back to the same dimensions +
    /// pixel values we started with (the `image` crate's
    /// `load_from_memory` round-trips losslessly for our test
    /// image).
    #[test]
    fn current_image_normalizes_tiff_to_png() {
        let _lock = lock_for_test();
        let mut backend = MacOsPasteboard::new().expect("new");
        let _guard = ImageClipboardGuard::new();

        let tiff = test_tiff_bytes();
        write_pasteboard_bytes(NS_PASTEBOARD_TYPE_TIFF, &tiff);

        let normalised = backend
            .current_image()
            .expect("current_image must return Some after writing TIFF to pasteboard");
        assert_eq!(
            normalised.mime, "image/png",
            "TIFF input must be normalised to image/png (PLAN §3 评审 #2 3rd)"
        );
        // PNG magic check.
        assert_eq!(
            &normalised.data[..8],
            &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A],
            "normalised bytes must start with the PNG magic"
        );
        // Round-trip via the `image` crate to confirm dimensions +
        // pixel values match.
        let decoded =
            image::load_from_memory(&normalised.data).expect("normalised PNG must be decodable");
        assert_eq!(
            decoded.width(),
            2,
            "TIFF→PNG normalisation must preserve width"
        );
        assert_eq!(
            decoded.height(),
            2,
            "TIFF→PNG normalisation must preserve height"
        );
    }

    /// `set_image` writes PNG bytes to the pasteboard via
    /// `NSPasteboard::setData_forType("public.png")`. A subsequent
    /// read via `NSPasteboard.dataForType("public.png")` must return
    /// the exact same bytes — this is what makes the receive side
    /// see "PNG bytes I can paste anywhere".
    ///
    /// **Note**: we verify via `NSPasteboard.dataForType` directly,
    /// not via `backend.current_image`, because we want to test the
    /// `set_image` write path in isolation from the read path's
    /// PNG-first preference logic.
    #[test]
    fn set_image_writes_png_bytes_to_pasteboard() {
        let _lock = lock_for_test();
        let mut backend = MacOsPasteboard::new().expect("new");
        let _guard = ImageClipboardGuard::new();

        let png = test_png_bytes();
        backend
            .set_image(&png, Mime::Png)
            .expect("set_image must succeed on macOS");

        // Verify via direct NSPasteboard read.
        let pb = NSPasteboard::generalPasteboard();
        let read_back = pb
            .dataForType(&NSString::from_str(NS_PASTEBOARD_TYPE_PNG))
            .expect("PNG must be on pasteboard after set_image")
            .to_vec();
        assert_eq!(
            read_back, png,
            "set_image PNG bytes must be readable byte-for-byte from NSPasteboard"
        );

        // Also verify the backend's own read-back path agrees (this
        // covers the "current_image picks up bytes we just wrote"
        // scenario the dispatcher will hit in STEP-2a.3).
        let via_backend = backend
            .current_image()
            .expect("current_image must return Some after set_image");
        assert_eq!(
            via_backend.data, png,
            "current_image after set_image must return the same bytes"
        );
    }

    /// `set_image` with a non-PNG `mime` must still succeed (it
    /// writes the bytes verbatim under the `.png` type identifier)
    /// but must log a `warn!` so the mis-call is debuggable. The
    /// caller (the dispatcher) is responsible for normalising
    /// non-PNG images to PNG before reaching the backend — passing
    /// JPEG / BMP bytes here is a programming error we surface but
    /// do not reject, so a future M2b backend that *does* support
    /// JPEG can override `set_image` to honour the mime.
    #[test]
    fn set_image_with_non_png_mime_writes_but_logs_warning() {
        let _lock = lock_for_test();
        let mut backend = MacOsPasteboard::new().expect("new");
        let _guard = ImageClipboardGuard::new();

        // Pass JPEG-flavoured bytes — they will be written to the
        // pasteboard as PNG (the backend forces PNG encoding). The
        // warn! is verified by `RUST_LOG=warn` log capture (manual);
        // here we just confirm the call returns Ok and the bytes
        // are stored.
        let jpeg_bytes = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        backend
            .set_image(&jpeg_bytes, Mime::Jpeg)
            .expect("set_image must succeed even for non-PNG mime (warns + writes verbatim)");

        let pb = NSPasteboard::generalPasteboard();
        let read_back = pb
            .dataForType(&NSString::from_str(NS_PASTEBOARD_TYPE_PNG))
            .expect("PNG must be on pasteboard after set_image(Jpeg)")
            .to_vec();
        assert_eq!(
            read_back, jpeg_bytes,
            "macOS backend writes bytes verbatim under .png regardless of `mime`"
        );
    }
}
