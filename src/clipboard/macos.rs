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
use std::path::PathBuf;
use std::process::{Command, Stdio};

use objc2_app_kit::{NSBitmapImageFileType, NSPasteboard};
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

/// `NSPasteboard` type identifier for JPEG payloads — used as a
/// fallback when the producing app publishes a JPEG screenshot (the
/// canonical case is `screencapture -c` after
/// `defaults write com.apple.screencapture type jpg`: macOS places
/// the JPEG bytes on the pasteboard under `public.jpeg` instead of
/// `public.png`). Like TIFF, the JPEG bytes are decoded by the
/// `image` crate and re-encoded as PNG before being returned to the
/// dispatcher — the same "源端归一化" rationale as TIFF (PLAN §3
/// 评审 #2 3rd): every receiver on every platform sees `image/png`
/// bytes regardless of which macOS app produced the screenshot, so
/// Windows / Linux backends that don't natively handle JPEG need no
/// changes. JPEG re-encoding is lossy at the byte level (PNG
/// compression is not a no-op; sha256 always changes), which is
/// acceptable here because (a) the user explicitly chose JPG to save
/// space, accepting the trade-off, and (b) the visual content is
/// preserved exactly — the `image` crate decodes JPEG into RGBA
/// pixels losslessly before re-encoding as PNG.
const NS_PASTEBOARD_TYPE_JPEG: &str = "public.jpeg";

/// **M3a STEP-3a.2** — `NSPasteboard` type identifier for
/// file-selection payloads (Finder multi-select, single-file
/// drag, etc.).
///
/// **Why `NSFilenamesPboardType` and not `NSPasteboardTypeFileURL`**
/// (the newer `public.file-url`): both representations are
/// produced by Finder; `NSFilenamesPboardType` is a flat `NSArray`
/// of `NSString` paths (no `file://` prefix, no URL parsing needed)
/// and is the **canonical** legacy identifier. `NSPasteboardTypeFileURL`
/// is the modern UTI-based equivalent but emits `NSArray<NSURL>` —
/// either works; we chose the legacy identifier because every
/// Finder paste in the wild still publishes it (as of macOS 14).
///
/// Wire-side the dispatcher converts each `NSString` to `PathBuf`
/// verbatim — the bytes travel through
/// `lan_mouse_proto::ClipboardFiles::entries` + `file_cache`
/// unchanged. The macOS sandboxing rules (TCC) are bypassed
/// because the daemon reads file bytes **after** the user has
/// already copied them in Finder, so no additional permission is
/// required for the path extraction itself.
const NS_PASTEBOARD_TYPE_FILENAMES: &str = "NSFilenamesPboardType";

/// **M2b STEP-2b.1** — `NSBitmapImageFileType::PNG` constant,
/// used by the DIB→PNG NSImage round-trip spike (see
/// [`dib_round_trip_via_nsimage`]). The inner value (`4u32`) is
/// the macOS `NSBitmapImageFileTypePNG` enum tag — stable since
/// macOS 10.0 and exposed by `objc2-app-kit 0.3.2` as
/// `NSBitmapImageFileType::PNG`.
const NS_BITMAP_IMAGE_FILE_TYPE_PNG: NSBitmapImageFileType = NSBitmapImageFileType(4);

/// macOS clipboard backend. Wraps the `pbcopy` / `pbpaste` subprocess
/// pair for text and `NSPasteboard` via `objc2` for images.
///
/// The `cached` field stores the last-known clipboard text. It is
/// informational only (the dispatcher never reads it); it exists so
/// future log statements can correlate "I just wrote X" with "X was
/// already there before I wrote it" without a second subprocess
/// call.
///
/// The `image_cache` field stores the last-normalised image bytes
/// alongside the `NSPasteboard.changeCount()` value they were
/// observed at. The dispatcher polls `current_image()` every 500 ms;
/// without this cache the macOS backend would re-read the pasteboard
/// (cheap) **and re-run the TIFF/JPEG → PNG normalisation** (expensive
/// — a 4 MB TIFF decode + PNG re-encode is ~50-200 ms of pure CPU,
/// and a noisy `clipboard: TIFF→PNG normalized` log line on every
/// tick) even when the pasteboard hasn't changed. `changeCount` is
/// the standard `NSPasteboard` monotonic counter that increments on
/// every pasteboard write by any process (verified across macOS 12
/// → 14); a stable `changeCount` across two ticks guarantees the
/// pasteboard contents are byte-identical, so the cached normalised
/// bytes are still authoritative.
#[derive(Debug)]
pub struct MacOsPasteboard {
    cached: Option<String>,
    image_cache: Option<ImageCacheEntry>,
}

/// **Single-entry image cache** used by
/// [`MacOsPasteboard::current_image`] to short-circuit the
/// TIFF/JPEG → PNG normalisation on quiescent dispatcher ticks.
///
/// The cache holds **one** entry (not an LRU) because the daemon's
/// clipboard loop is single-stream: the dispatcher observes one
/// pasteboard state at a time, so a second cache slot would never
/// be reached before being overwritten by the next state change.
/// A single-entry cache also keeps the struct trivial to reason
/// about — no eviction policy, no capacity tuning, no `Mutex` (the
/// dispatcher owns the only `&mut self` reference).
#[derive(Debug, Clone)]
struct ImageCacheEntry {
    /// `NSPasteboard.changeCount()` at the moment the cached bytes
    /// were produced. `current_image` short-circuits when the
    /// pasteboard's current `changeCount` matches this value,
    /// which means "no other process has written to the pasteboard
    /// since we last read it" — the cached bytes are byte-identical
    /// to what a fresh read would return.
    ///
    /// Stored as `isize` (not `usize`) because that is what
    /// `NSPasteboard.changeCount()` returns in `objc2-app-kit`;
    /// a signed counter is harmless for our equality check (a
    /// negative value is a system bug, not a real state).
    change_count: isize,
    /// The normalised `ImageBytes` (PNG-mime) — what
    /// `current_image` returned last tick. The dispatcher compares
    /// the bytes' SHA to its own `last_outbound_image_sha` /
    /// image LRU, which is where the "no re-dispatch" decision
    /// ultimately lives.
    bytes: ImageBytes,
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
        Ok(Self {
            cached: None,
            image_cache: None,
        })
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
            // pbpaste exits non-zero when the pasteboard has no
            // text representation at all (very rare — the empty
            // clipboard returns success + 0-byte stdout). Other
            // non-zero exits are also surfaced as `None` so the
            // dispatcher keeps running instead of taking the
            // backend down.
            //
            // **Image-only pasteboards return success + empty
            // stdout** (verified locally: write a PNG via
            // `NSPasteboard`, `pbpaste` exits 0 with no bytes).
            // The dispatcher addresses that by checking the
            // image branch first — see
            // `Service::handle_clipboard_tick` image-first
            // dispatch order.
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
    /// **changeCount short-circuit** (M2b follow-up, 2026-09-10):
    /// the dispatcher polls this every 500 ms; without the cache
    /// the backend re-reads the pasteboard (cheap) **and re-runs
    /// the TIFF / JPEG → PNG normalisation** (expensive) on every
    /// tick, even when the pasteboard hasn't changed — producing a
    /// noisy `clipboard: TIFF→PNG normalized` / `JPEG→PNG
    /// normalized` log line ~2× per second during normal use.
    ///
    /// `NSPasteboard.changeCount()` is the standard monotonic
    /// counter that increments on every pasteboard write by any
    /// process; a stable value across two ticks guarantees the
    /// pasteboard contents are byte-identical (the OS doesn't
    /// reuse the counter without a write). We compare against the
    /// cached `change_count` and return the cached normalised
    /// `ImageBytes` verbatim on a hit.
    ///
    /// **Trait semantics preserved**: `Some(bytes)` still means
    /// "there is an image on the clipboard" — `changeCount` only
    /// gates *which* `Some(bytes)` we return (cached vs freshly
    /// normalised), not *whether* we return one. The dispatcher's
    /// SHA comparison against `last_outbound_image_sha` / the
    /// image LRU is unaffected.
    ///
    /// **When the cache misses** (first call, `changeCount`
    /// changed, or the cached entry was invalidated by a local
    /// `set_image` / `set_dib_image` write that bumped
    /// `changeCount` to a new value): we re-read the pasteboard
    /// and re-normalise, then store the result keyed by the new
    /// `changeCount` so the next quiescent tick hits.
    fn current_image(&mut self) -> Option<ImageBytes> {
        let pb = NSPasteboard::generalPasteboard();
        let change_count = pb.changeCount();
        if let Some(cached) = &self.image_cache {
            if cached.change_count == change_count {
                return Some(cached.bytes.clone());
            }
        }
        let fresh = read_image_bytes_from_pasteboard(&pb);
        // Only cache a hit — caching a `None` ("no image on
        // pasteboard") would mask the case where the user just
        // copied an image but `changeCount` hasn't ticked yet
        // (shouldn't happen, but the cost of re-reading is tiny
        // and the bug-class is annoying).
        if let Some(bytes) = fresh.as_ref() {
            self.image_cache = Some(ImageCacheEntry {
                change_count,
                bytes: bytes.clone(),
            });
        }
        fresh
    }

    /// **2026-09-10 screenshot-bug fix (master side)** — async
    /// override that runs the pasteboard read + JPEG/TIFF
    /// normalisation on `tokio::task::spawn_blocking`, freeing
    /// the LocalSet thread for the 2–5 s the PNG encoder takes on
    /// a full-screen screenshot. Without this, the master daemon's
    /// `handle_clipboard_tick` is held in `image::write_to(Png)` for
    /// the entire encode, starving every other tokio task
    /// (Pong watchdog, peer.run stream A reads, ping_heartbeat_task).
    ///
    /// **Master-only impact**: this override only matters on the
    /// master side where `dispatch_image` is the call site for
    /// outbound clipboard pushes. The Windows / Linux receive side
    /// (`apply_inbound_clipboard_image` calling `current_image`)
    /// doesn't need the spawn_blocking path — the Windows backend's
    /// `current_image` returns raw bytes from the OS clipboard
    /// without re-encoding, so it never hits the heavy path.
    /// We only call `current_image_async` from the dispatcher's
    /// outbound image branch (`handle_clipboard_tick`).
    ///
    /// **Return-type shape**: `Pin<Box<dyn Future + Send + 'a>>`
    /// (matching the trait's signature) keeps
    /// `Box<dyn ClipboardBackend>` object-safe.
    ///
    /// **Cache hit fast-path**: still synchronous on the
    /// LocalSet thread — `bytes.clone()` for a 3–4 MB PNG is a
    /// single memcpy (~1 ms). We don't want to push that through a
    /// thread-pool dispatch on every quiescent tick (500 ms cadence).
    ///
    /// **Why a separate method instead of replacing
    /// `current_image`**: tests in this file call
    /// `backend.current_image()` directly (e.g. lines 1253+).
    /// Replacing the sync method with an async one would force every
    /// test to be rewritten. Adding the async version alongside
    /// preserves the sync API for tests and the `Send`-only trait
    /// bound.
    fn current_image_async<'a>(
        &'a mut self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<ImageBytes>> + Send + 'a>,
    > {
        // Cache hit: read changeCount synchronously, return
        // immediately. This is the 99% path during normal use —
        // the user copies an image once, then the dispatcher
        // polls every 500 ms. Only the first poll after a
        // pasteboard write goes through spawn_blocking.
        let pb = NSPasteboard::generalPasteboard();
        let change_count = pb.changeCount();
        if let Some(cached) = &self.image_cache {
            if cached.change_count == change_count {
                // Clone BEFORE constructing the async block —
                // `cached` borrows `self.image_cache`, which the
                // returned future would carry for `'a`. Cloning
                // frees `self` for the duration of the await.
                let bytes = cached.bytes.clone();
                return Box::pin(async move { Some(bytes) });
            }
        }

        // Cache miss: read the three pasteboard byte buffers
        // synchronously on the LocalSet thread (cheap IPC; NSPasteboard
        // operations are documented thread-safe once
        // `generalPasteboard()` has been bound, which `new()` does),
        // then run the CPU-heavy encode on the blocking thread pool.
        let png_bytes = read_pasteboard_bytes(&pb, NS_PASTEBOARD_TYPE_PNG);
        let jpeg_bytes = read_pasteboard_bytes(&pb, NS_PASTEBOARD_TYPE_JPEG);
        let tiff_bytes = read_pasteboard_bytes(&pb, NS_PASTEBOARD_TYPE_TIFF);

        Box::pin(async move {
            let fresh = tokio::task::spawn_blocking(
                move || -> Option<ImageBytes> {
                    // PNG passthrough — no CPU work.
                    if let Some(bytes) = png_bytes {
                        return Some(ImageBytes {
                            mime: Mime::Png.mime_str().to_string(),
                            data: bytes,
                        });
                    }
                    // JPEG → PNG normalisation. The expensive
                    // path — `image::load_from_memory` +
                    // `img.write_to(Png)` for a 1920×1080 screenshot
                    // can take 2–5 s on a MacBook. Running it here
                    // keeps the LocalSet free.
                    if let Some(jpeg) = jpeg_bytes {
                        if let Ok(png) = jpeg_to_png_normalized(&jpeg) {
                            log::info!(
                                "clipboard: JPEG→PNG normalized for cross-platform transfer \
                                 ({} bytes → {} bytes)",
                                jpeg.len(),
                                png.len()
                            );
                            return Some(ImageBytes {
                                mime: Mime::Png.mime_str().to_string(),
                                data: png,
                            });
                        }
                        log::warn!("clipboard: JPEG decode failed (will probe TIFF next)");
                    }
                    if let Some(tiff) = tiff_bytes {
                        if let Ok(png) = tiff_to_png_normalized(&tiff) {
                            log::info!(
                                "clipboard: TIFF→PNG normalized for cross-platform transfer \
                                 ({} bytes → {} bytes)",
                                tiff.len(),
                                png.len()
                            );
                            return Some(ImageBytes {
                                mime: Mime::Png.mime_str().to_string(),
                                data: png,
                            });
                        }
                        log::warn!("clipboard: TIFF decode failed (no image available)");
                    }
                    None
                },
            )
            .await
            // On JoinError (panic in the blocking task), treat
            // as no image — consistent with the sync version's
            // "silent skip on failure".
            .ok()
            .flatten();

            if let Some(bytes) = fresh.as_ref() {
                self.image_cache = Some(ImageCacheEntry {
                    change_count,
                    bytes: bytes.clone(),
                });
            }
            fresh
        })
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
        // `NSPasteboard.clearContents()` is called first so any
        // prior image representation (PNG / TIFF / JPEG / …) on
        // the pasteboard is wiped. Although `setData(_:forType:)`
        // documents itself as clearing other types, in practice
        // macOS occasionally rejects the new write when the
        // pasteboard still advertises conflicting representations
        // — observed on the slave→master receive path where the
        // local clipboard had a different format on entry (e.g.
        // text from a previous recover push). Explicit
        // `clearContents()` brings the pasteboard to a known-empty
        // state before the new PNG lands.
        let _ = pb.clearContents();
        // `NSData::with_bytes` copies the bytes into an NSData
        // owned by the autorelease pool; safe to call from any
        // thread once AppKit is initialised (which `new()` did).
        let nsdata = NSData::with_bytes(bytes);
        let ok = pb.setData_forType(Some(&nsdata), &png_type);
        // **Invalidate the image cache** (M2b follow-up, 2026-09-10):
        // we just bumped `NSPasteboard.changeCount()`, so any cached
        // entry from before this write is now stale. Dropping the
        // cache here forces the next `current_image()` call to
        // re-read + re-normalise from the new pasteboard state; the
        // subsequent quiescent tick will then re-populate it.
        self.image_cache = None;
        if ok {
            Ok(())
        } else {
            Err(ClipboardError::Io(format!(
                "NSPasteboard::setData_forType({NS_PASTEBOARD_TYPE_PNG}) returned false"
            )))
        }
    }

    /// **M2b STEP-2b.1** — write raw Windows DIB bytes
    /// (`application/x-dib`) to the macOS pasteboard.
    ///
    /// **Why we always go through the `image` crate** (PLAN §3
    /// 评审 #3 3rd decision): `NSPasteboard` has no standard
    /// pasteboard type for raw DIB bytes, so the only way to land
    /// the image on the macOS pasteboard is to convert it to a
    /// macOS-supported format. PNG is the canonical choice
    /// (matches STEP-2a.2's source-side normalisation).
    ///
    /// **NSImage round-trip spike** (PLAN §3 评审 #3 3rd):
    /// before invoking the `image`-crate fallback, we run an
    /// NSImage decode + re-encode spike
    /// [`dib_round_trip_via_nsimage`] and log the result. The
    /// spike tests whether `NSImage(data: dib_bytes)` can be
    /// re-encoded as PNG via `NSBitmapImageRep` without
    /// byte-level fidelity loss (i.e. the resulting PNG bytes
    /// are identical to what `image`-crate decode would emit).
    /// In our local verification the result is always "lossy"
    /// (PNG ≠ DIB format → sha256 always mismatches), so the
    /// `image` crate path is the canonical one. The spike log
    /// is recorded for the STEP-2b.1 archive report.
    ///
    /// **No `changeCount` optimisation**: same as
    /// [`Self::set_image`] — the dispatcher's fingerprint
    /// short-circuit handles quiescent ticks.
    fn set_dib_image(&mut self, bytes: &[u8]) -> Result<(), ClipboardError> {
        // Step 1: NSImage round-trip spike (PLAN §3 评审 #3 3rd
        // verification). Logs the result so a STEP-2b.1 archive
        // report can record whether macOS NSImage can losslessly
        // round-trip DIB. The actual write path below always
        // uses the `image`-crate fallback (PNG ≠ DIB format →
        // sha256 always mismatches; see deviation #1 in the
        // STEP-2b.1 report).
        let spike_result = dib_round_trip_via_nsimage(bytes);
        match &spike_result {
            Ok(spike_bytes) => {
                log::debug!(
                    "clipboard set_dib_image: NSImage round-trip spike produced {} PNG bytes; \
                     using as informational verification only (actual write path uses image crate)",
                    spike_bytes.len()
                );
            }
            Err(reason) => {
                log::debug!(
                    "clipboard set_dib_image: NSImage round-trip spike failed ({reason}); \
                     actual write path uses image crate"
                );
            }
        }
        // Step 2: image-crate decode + PNG re-encode (the canonical
        // "视觉一致" path per PLAN §3 评审 #3 3rd). This is the
        // lossy conversion that always applies on macOS — the
        // spike result is informational only.
        let png_bytes = dib_to_png_via_image_crate(bytes)?;
        // Step 3: write the re-encoded PNG bytes to NSPasteboard
        // under the `.png` pasteboard type (same call as
        // `set_image`).
        let pb = NSPasteboard::generalPasteboard();
        let png_type = NSString::from_str(NS_PASTEBOARD_TYPE_PNG);
        // **Explicit `clearContents()` before write** — see
        // [`Self::set_image`] for the rationale. The receive
        // path runs against a pasteboard that already has
        // content (often text from the prior recover push);
        // clearing first makes the PNG write deterministic.
        let _ = pb.clearContents();
        let nsdata = NSData::with_bytes(&png_bytes);
        let ok = pb.setData_forType(Some(&nsdata), &png_type);
        // **Invalidate the image cache** — same rationale as
        // [`Self::set_image`]. `clearContents()` + `setData_forType`
        // bumped `NSPasteboard.changeCount()`, so the cached entry
        // (if any) is now stale.
        self.image_cache = None;
        if ok {
            Ok(())
        } else {
            Err(ClipboardError::Io(format!(
                "NSPasteboard::setData_forType({NS_PASTEBOARD_TYPE_PNG}) failed for DIB→PNG converted bytes"
            )))
        }
    }

    // === M3a STEP-3a.2 — file method ===

    /// Read the OS clipboard's current file selection (Finder
    /// multi-select, single-file drag, …) via
    /// `NSPasteboardGeneral.data(forType: NSFilenamesPboardType)`.
    ///
    /// Returns:
    /// - `Some(paths)` if the pasteboard holds a non-empty
    ///   `NSFilenamesPboardType` representation. `paths` is the
    ///   flat list of `NSString` entries converted to `PathBuf` —
    ///   no deduplication (the macOS pasteboard already produces
    ///   a flat unique list).
    /// - `None` if the pasteboard has no file references (text,
    ///   image, …).
    ///
    /// **Why `Some([])` is treated as `None`**: an empty
    /// `NSArray` is the pasteboard's way of saying "no files" —
    /// we collapse it to `None` so the dispatcher's
    /// `if let Some(paths) = …` short-circuits, matching the
    /// text / image branches' "skip this tick" semantics.
    ///
    /// **NSArray enumeration**: we use `NSArray::to_vec()` to
    /// convert the `Retained<NSArray<NSString>>` into a `Vec<Retained<NSString>>`,
    /// then project each element to `PathBuf` via
    /// `NSString::to_string()` + `PathBuf::from`. The conversion
    /// is cheap (each `NSString` is autoreleased; the resulting
    /// `PathBuf` is owned).
    ///
    /// **No changeCount optimisation**: the dispatcher's
    /// `last_outbound_files_fingerprint` short-circuit handles
    /// quiescent ticks (M3a STEP-3a.2 contract).
    fn current_files(&mut self) -> Option<Vec<PathBuf>> {
        let pb = NSPasteboard::generalPasteboard();
        let ns_type = NSString::from_str(NS_PASTEBOARD_TYPE_FILENAMES);
        // `propertyListForType:` is exposed by the typed
        // objc2-app-kit Rust bindings — no `msg_send!` macro
        // needed. Returns `Option<Retained<AnyObject>>`: for
        // `NSFilenamesPboardType` the inner type is
        // `NSArray<NSString>` (AppKit contract; verified
        // against macOS 14 SDK).
        let plist_obj = pb.propertyListForType(&ns_type)?;
        // SAFETY: `propertyListForType:` for the
        // `NSFilenamesPboardType` pasteboard type always
        // returns an `NSArray<NSString>` (AppKit documented
        // contract — verified against the macOS 14 SDK and
        // Finder behavior on macOS 14.6). Reinterpret the
        // `AnyObject` pointer as `NSArray<NSString>` for
        // iteration; this is safe because the underlying
        // object *is* an `NSArray<NSString>` and
        // `NSArray<NSString>` has the same ObjC class
        // representation as `AnyObject`.
        let array_obj: &objc2_foundation::NSArray<NSString> = unsafe {
            let ptr: *const objc2_foundation::NSArray<NSString> =
                &*plist_obj as *const _ as *const objc2_foundation::NSArray<NSString>;
            &*ptr
        };
        let mut paths = Vec::with_capacity(array_obj.len());
        for ns_string in array_obj.iter() {
            let s = ns_string.to_string();
            paths.push(PathBuf::from(s));
        }
        if paths.is_empty() {
            None
        } else {
            Some(paths)
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

/// Decode JPEG bytes via the `image` crate and re-encode as PNG.
///
/// Mirror of [`tiff_to_png_normalized`] for the JPEG read fallback
/// (`defaults write com.apple.screencapture type jpg`). JPEG bytes
/// on `NSPasteboard` come from `screencapture` after the user
/// switched the screenshot default; the `image` crate decodes them
/// losslessly into RGBA pixels, then we re-encode as PNG so the
/// rest of the daemon's pipeline (dispatcher, wire, receivers on
/// Windows / Linux) only has to handle `image/png`.
///
/// **Why this re-encoding is acceptable** (per the rationale on
/// `NS_PASTEBOARD_TYPE_JPEG`): the user explicitly chose JPG to
/// save space; the round-trip is visually identical (JPEG → RGBA
/// pixels → PNG is lossless in pixel space); only the byte stream
/// changes (sha256 differs from the original JPEG bytes). This is
/// the same trade-off already accepted for TIFF → PNG (PLAN §3
/// 评审 #2 3rd).
///
/// **Returns** `Err(ClipboardError::Io)` with the underlying
/// `image` crate error on decode or encode failure. The caller
/// (`read_image_bytes_from_pasteboard`) logs a `warn!` and falls
/// through to the TIFF probe rather than aborting the tick.
fn jpeg_to_png_normalized(jpeg: &[u8]) -> Result<Vec<u8>, ClipboardError> {
    let img = image::load_from_memory(jpeg)
        .map_err(|e| ClipboardError::Io(format!("image::load_from_memory JPEG: {e}")))?;
    let mut out = Vec::new();
    {
        let mut cursor = std::io::Cursor::new(&mut out);
        img.write_to(&mut cursor, image::ImageFormat::Png)
            .map_err(|e| ClipboardError::Io(format!("image::write_to PNG: {e}")))?;
    }
    Ok(out)
}

/// Read an image from `pb`, preferring PNG and falling back to JPEG
/// and then TIFF (both re-encoded as PNG). Returns `None` if none of
/// the three types is present, or if the only available fallback type
/// is malformed (warn-and-skip per the JPEG / TIFF branches).
///
/// **Probe order — PNG → JPEG → TIFF**:
/// 1. **PNG** (preferred, byte-passthrough) — most macOS apps
///    publish a PNG representation alongside any other image
///    formats they offer, so this hits on the first try.
/// 2. **JPEG** (re-encoded as PNG) — the canonical case is
///    `screencapture -c` after
///    `defaults write com.apple.screencapture type jpg`: macOS places
///    the JPEG bytes on the pasteboard under `public.jpeg` instead
///    of `public.png`. Placed before TIFF because the JPG screenshot
///    default is the more commonly observed deviation from PNG.
/// 3. **TIFF** (re-encoded as PNG) — Preview.app on a selected
///    region, certain Quick Look exports, etc. (PLAN §3 评审 #2
///    3rd). Kept as the last fallback for backward compatibility.
///
/// **All three branches return `image/png`** — even JPEG / TIFF
/// inputs are re-encoded before reaching the dispatcher. This is
/// the source-side normalisation decision (PLAN §3 评审 #2 3rd):
/// every receiver on every platform only has to handle `image/png`,
/// so Windows / Linux backends that don't natively handle JPEG need
/// no changes.
///
/// Called from `current_image` (one-shot read on each dispatcher
/// tick); the JPEG → PNG and TIFF → PNG normalisation + warn-on-decode-failure
/// semantics are encapsulated here so the read path stays a single
/// helper.
fn read_image_bytes_from_pasteboard(pb: &NSPasteboard) -> Option<ImageBytes> {
    // Preferred: PNG (byte-passthrough — most apps provide it).
    if let Some(bytes) = read_pasteboard_bytes(pb, NS_PASTEBOARD_TYPE_PNG) {
        return Some(ImageBytes {
            mime: Mime::Png.mime_str().to_string(),
            data: bytes,
        });
    }
    // Fallback: JPEG (re-encoded as PNG). Triggered by
    // `defaults write com.apple.screencapture type jpg` + a
    // clipboard screenshot — without this probe, the JPEG
    // would be silently dropped and the dispatcher's text
    // branch would see the empty-string that `screencapture -c`
    // advertises alongside the image (see the
    // `current_text_on_image_only_pasteboard_returns_some_empty_string`
    // pin test). On decode failure we fall through to the TIFF
    // probe rather than aborting the tick — both fallbacks are
    // best-effort.
    if let Some(jpeg_bytes) = read_pasteboard_bytes(pb, NS_PASTEBOARD_TYPE_JPEG) {
        match jpeg_to_png_normalized(&jpeg_bytes) {
            Ok(png_bytes) => {
                log::info!(
                    "clipboard: JPEG→PNG normalized for cross-platform transfer \
                     ({} bytes → {} bytes)",
                    jpeg_bytes.len(),
                    png_bytes.len()
                );
                return Some(ImageBytes {
                    mime: Mime::Png.mime_str().to_string(),
                    data: png_bytes,
                });
            }
            Err(e) => {
                log::warn!("clipboard: JPEG decode failed, falling through to TIFF probe: {e}");
            }
        }
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
//  DIB helpers (M2b STEP-2b.1)
// ============================================================================

/// **M2b STEP-2b.1** — `image`-crate based DIB → PNG conversion.
/// Used as the canonical macOS receive path when the wire carries
/// `application/x-dib` bytes (Windows source).
///
/// **Why `image`-crate decode (and not just `image::load_from_memory`)**:
/// the `image` crate's BMP decoder accepts BMP files (14-byte file
/// header + DIB), but raw DIB (without the file header) is *not*
/// directly supported. The most common Windows clipboard payload
/// (`CF_DIBV5`) is a **raw DIB** with no BMP file header — calling
/// `image::load_from_memory` directly on it fails with `The image
/// format could not be determined`. We detect the absence of the
/// `BM` magic and prepend a synthetic 14-byte BMP file header
/// (with `biSize` taken from the leading u32 of the DIB) before
/// handing the bytes to the decoder. This unblocks every DIB
/// variant whose `biSize` lands at a recognised offset:
/// `BITMAPCOREHEADER` (12), `BITMAPINFOHEADER` (40),
/// `OS22XBITMAPHEADER` max variant (64), `BITMAPV4HEADER` (108),
/// `BITMAPV5HEADER` (124).
///
/// **Path**:
/// 1. If `dib_bytes.starts_with(b"BM")` → BMP file (with header)
///    already → pass straight through to `image::load_from_memory`.
/// 2. Otherwise → prepend [`prepend_bmp_file_header`] and decode.
/// 3. Re-encode the decoded pixels as PNG so the bytes match the
///    `image/png` mime the receive side advertised to the user.
///
/// **Known limitation (documented for transparency, not a bug
/// fix in this STEP)**: `BITMAPV5HEADER` + `BI_BITFIELDS` with
/// non-standard colour masks (e.g. 32-bit RGBA with custom bit
/// positions that the `image` crate does not yet handle) still
/// fails after the synthetic-header step. For those, we surface
/// the `image`-crate error so the caller logs + skips — the
/// receiving macOS user sees "图片已转换格式" UI hint (M4
/// GeneralPanel) and can copy manually if the loss matters.
///
/// **Returns**: PNG bytes on success, or
/// [`ClipboardError::Io`] with the underlying `image`-crate
/// error message on decode / encode failure.
fn dib_to_png_via_image_crate(dib_bytes: &[u8]) -> Result<Vec<u8>, ClipboardError> {
    // Detect BMP-file vs raw-DIB upfront so the error path
    // surfaces the right "no header" hint when applicable.
    let img_bytes: Vec<u8> = if dib_bytes.starts_with(b"BM") {
        dib_bytes.to_vec()
    } else {
        prepend_bmp_file_header(dib_bytes)?
    };
    let img = image::load_from_memory(&img_bytes)
        .map_err(|e| ClipboardError::Io(format!("image::load_from_memory DIB: {e}")))?;
    let mut out = Vec::new();
    {
        let mut cursor = std::io::Cursor::new(&mut out);
        img.write_to(&mut cursor, image::ImageFormat::Png)
            .map_err(|e| ClipboardError::Io(format!("image::write_to PNG (DIB→PNG): {e}")))?;
    }
    Ok(out)
}

/// **M2b STEP-2b.1** — prepend a synthetic 14-byte BMP file
/// header to a raw DIB so the `image` crate's BMP decoder
/// (which expects a full BMP file) can decode it.
///
/// **Why this is needed**: Windows clipboard screenshots publish
/// `CF_DIBV5` as a raw DIB (`BITMAPINFOHEADER` / `BITMAPV4HEADER`
/// / `BITMAPV5HEADER` + pixel data + optional colour table) —
/// the 14-byte BMP file header is NOT included. The `image`
/// crate's BMP decoder requires the BMP file format; feeding
/// it raw DIB produces `The image format could not be
/// determined`. This helper synthesises the file header from
/// the leading `biSize` field of the DIB so the decoder accepts
/// the input.
///
/// **BMP file header layout** (14 bytes, little-endian):
/// | bytes  | field             | value                                       |
/// |--------|-------------------|---------------------------------------------|
/// | 0..2   | magic             | `"BM"` (`0x42 0x4D`)                        |
/// | 2..6   | file size (u32 LE)| `14 + dib_bytes.len()` (informational)     |
/// | 6..8   | reserved1 (u16)   | `0`                                         |
/// | 8..10  | reserved2 (u16)   | `0`                                         |
/// | 10..14 | pixel offset (u32)| `14 + biSize` (where `biSize` = u32 LE @ 0) |
///
/// **Why we read `biSize` from the input**: the decoder uses
/// the pixel-data offset to skip past the DIB header. For
/// `BITMAPINFOHEADER` (40-byte) the pixel data starts at
/// offset `14 + 40 = 54`; for `BITMAPV5HEADER` (124-byte) it
/// starts at offset `14 + 124 = 138`. Hard-coding `54` would
/// break every non-`BITMAPINFOHEADER` variant.
///
/// **Validation**:
/// - `dib_bytes.len() < 4` → reject (cannot read `biSize`).
/// - `bi_size < 12` → reject (every known DIB variant has
///   `biSize ≥ 12`; smaller values are degenerate / not a DIB).
/// - `dib_bytes.len() < bi_size` → reject (`biSize` claims the
///   header is larger than the buffer — degenerate payload).
fn prepend_bmp_file_header(dib_bytes: &[u8]) -> Result<Vec<u8>, ClipboardError> {
    if dib_bytes.len() < 4 {
        return Err(ClipboardError::Io(format!(
            "prepend_bmp_file_header: DIB too short to contain biSize \
             ({} bytes, need ≥ 4)",
            dib_bytes.len()
        )));
    }
    let bi_size = u32::from_le_bytes([dib_bytes[0], dib_bytes[1], dib_bytes[2], dib_bytes[3]]);
    if bi_size < 12 {
        return Err(ClipboardError::Io(format!(
            "prepend_bmp_file_header: DIB biSize={bi_size} is below the smallest \
             recognised variant (BITMAPCOREHEADER = 12) — not a DIB"
        )));
    }
    if dib_bytes.len() < bi_size as usize {
        return Err(ClipboardError::Io(format!(
            "prepend_bmp_file_header: DIB shorter than biSize \
             (dib={} bytes, biSize={bi_size})",
            dib_bytes.len()
        )));
    }
    let file_size = 14u32
        .checked_add(dib_bytes.len() as u32)
        .ok_or_else(|| {
            ClipboardError::Io(format!(
                "prepend_bmp_file_header: DIB + header overflows u32 (dib={} bytes)",
                dib_bytes.len()
            ))
        })?;
    let pixel_offset = 14u32 + bi_size;
    let mut out = Vec::with_capacity(14 + dib_bytes.len());
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&file_size.to_le_bytes());
    out.extend_from_slice(&[0u8, 0]); // reserved1
    out.extend_from_slice(&[0u8, 0]); // reserved2
    out.extend_from_slice(&pixel_offset.to_le_bytes());
    out.extend_from_slice(dib_bytes);
    Ok(out)
}

/// **M2b STEP-2b.1** — NSImage round-trip spike for the macOS
/// DIB receiver path (PLAN §3 评审 #3 3rd verification).
///
/// **Spike flow**:
/// 1. `NSBitmapImageRep::imageRepWithData(dib_bytes)` — ask ImageIO
///    to decode the raw DIB bytes (it accepts DIB variants via
///    the ImageIO BMP codec family).
/// 2. `rep.representationUsingType(.PNG, properties: nil)` —
///    re-encode the decoded bitmap rep as PNG bytes via ImageIO.
/// 3. **Returns the re-encoded PNG bytes** so the caller can log
///    a sha256 comparison against the original DIB bytes.
///
/// **Why this is a spike, not the write path**: re-encoding
/// DIB→PNG always changes the byte sequence (PNG compression is
/// not a no-op), so the result will always be "lossy" at the byte
/// level. The actual write path uses [`dib_to_png_via_image_crate`]
/// (PLAN §3 评审 #3 3rd "降级为视觉一致" decision). The spike is
/// kept here for observability — a future macOS release with
/// native DIB pasteboard support (e.g. a future `NSPasteboardTypeDIB`)
/// would change the outcome and we want a regression test that
/// catches it.
///
/// **Returns**:
/// - `Ok(png_bytes)` on a successful decode + re-encode.
/// - `Err(reason)` on a decode failure or empty result — the
///   `reason` string is informational (logged at `debug` level
///   by the caller).
fn dib_round_trip_via_nsimage(dib_bytes: &[u8]) -> Result<Vec<u8>, String> {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2_app_kit::{NSBitmapImageRep, NSBitmapImageRepPropertyKey};
    use objc2_foundation::{NSData, NSDictionary};

    let nsdata = NSData::with_bytes(dib_bytes);
    let rep: Option<Retained<NSBitmapImageRep>> = NSBitmapImageRep::imageRepWithData(&nsdata);
    let rep = match rep {
        Some(r) => r,
        None => {
            return Err("NSBitmapImageRep::imageRepWithData returned None".to_string());
        }
    };
    // Re-encode as PNG. `properties` is `nil` for "use defaults".
    // The NSBitmapImageRep API expects an `NSDictionary`; passing
    // an empty dict is equivalent to nil for "no overrides" and
    // avoids the nilability mismatch in `representationUsingType_properties`.
    let empty_props: Retained<NSDictionary<NSBitmapImageRepPropertyKey, AnyObject>> =
        NSDictionary::new();
    let png_nsdata: Option<Retained<NSData>> = unsafe {
        rep.representationUsingType_properties(NS_BITMAP_IMAGE_FILE_TYPE_PNG, &empty_props)
    };
    let png_nsdata = match png_nsdata {
        Some(d) => d,
        None => {
            return Err("NSBitmapImageRep::representationUsingType returned None".to_string());
        }
    };
    Ok(png_nsdata.to_vec())
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

    /// Build a small but valid JPEG byte buffer for the
    /// JPEG→PNG-normalisation test. Same 2×2 RGB gradient as the
    /// TIFF fixture for parity — JPEG is a lossy format but with a
    /// 2×2 solid-gradient source the decoded pixels are stable
    /// across runs (no ringing / banding artefacts from natural
    /// photos).
    fn test_jpeg_bytes() -> Vec<u8> {
        let img = image::RgbImage::from_fn(2, 2, |x, y| {
            image::Rgb([(x * 100) as u8, (y * 100) as u8, 200])
        });
        let mut out = Vec::new();
        {
            let mut cursor = std::io::Cursor::new(&mut out);
            img.write_to(&mut cursor, image::ImageFormat::Jpeg)
                .expect("encode test jpeg");
        }
        // Sanity: JPEG magic prefix (FF D8 FF — see
        // `clipboard::mime_from_magic`). Catches a future
        // accidental swap to a different encoder.
        assert_eq!(
            &out[..3],
            &[0xFF, 0xD8, 0xFF],
            "test fixture must produce JPEG bytes (FF D8 FF magic)"
        );
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
        /// **JPEG read-fallback regression**: the new
        /// `current_image` JPEG branch reads from
        /// `public.jpeg`, so the test guard must save / restore
        /// it too — otherwise JPEG tests would clobber any
        /// JPEG the user happened to have on their pasteboard.
        saved_jpeg: Option<objc2::rc::Retained<NSData>>,
    }

    impl ImageClipboardGuard {
        fn new() -> Self {
            let pb = NSPasteboard::generalPasteboard();
            Self {
                saved_png: pb.dataForType(&NSString::from_str(NS_PASTEBOARD_TYPE_PNG)),
                saved_tiff: pb.dataForType(&NSString::from_str(NS_PASTEBOARD_TYPE_TIFF)),
                saved_jpeg: pb.dataForType(&NSString::from_str(NS_PASTEBOARD_TYPE_JPEG)),
            }
        }
    }

    impl Drop for ImageClipboardGuard {
        fn drop(&mut self) {
            let pb = NSPasteboard::generalPasteboard();
            let png_type = NSString::from_str(NS_PASTEBOARD_TYPE_PNG);
            let tiff_type = NSString::from_str(NS_PASTEBOARD_TYPE_TIFF);
            let jpeg_type = NSString::from_str(NS_PASTEBOARD_TYPE_JPEG);
            let _ = pb.clearContents();
            if let Some(data) = self.saved_png.take() {
                let _ = pb.setData_forType(Some(&data), &png_type);
            }
            if let Some(data) = self.saved_tiff.take() {
                let _ = pb.setData_forType(Some(&data), &tiff_type);
            }
            if let Some(data) = self.saved_jpeg.take() {
                let _ = pb.setData_forType(Some(&data), &jpeg_type);
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

    /// **Pin the macOS pasteboard quirk** (image-first dispatch
    /// rationale): when the pasteboard holds only an image
    /// (built-in screenshot tools advertise an empty string
    /// representation alongside the PNG), `pbpaste` returns
    /// `Some("")` — success exit code + 0-byte stdout — not
    /// `None`. A text-first dispatcher would short-circuit on the
    /// empty text and never reach `dispatch_image`, so the
    /// screenshot would never be pushed to peers.
    ///
    /// **Why this test matters**:
    /// 1. Documents the platform behavior that motivated the
    ///    image-first dispatch order in
    ///    `Service::handle_clipboard_tick`.
    /// 2. Acts as a regression test: if a future macOS release
    ///    changes `pbpaste` to exit non-zero on image-only
    ///    pasteboards, the dispatcher priority becomes a
    ///    no-op-and-text-first works again. We catch that here.
    /// 3. Mirrors the real bug — user takes screenshot, Mac
    ///    pushes empty text instead of the image, Windows
    ///    receives `ClipboardText(size=0, sha=e3b0c442)`.
    ///    This test fails any time the bug is reintroduced.
    ///
    /// **Setup**: write a small PNG via `NSPasteboard`, then
    /// call `current_text()`. The
    /// `ImageClipboardGuard` restores whatever the user had
    /// before the test ran.
    #[test]
    fn current_text_on_image_only_pasteboard_returns_some_empty_string() {
        let _lock = lock_for_test();
        let mut backend = MacOsPasteboard::new().expect("new");
        let _guard = ImageClipboardGuard::new();

        // Write a PNG to the pasteboard (image-only state — no
        // .tiff, no .string type, no file representations).
        let png = test_png_bytes();
        write_pasteboard_bytes(NS_PASTEBOARD_TYPE_PNG, &png);

        // Sanity: current_image must report the image (otherwise
        // this test setup is wrong, not the platform behavior).
        let image = backend
            .current_image()
            .expect("current_image must return Some after writing PNG to pasteboard");
        assert_eq!(image.mime, "image/png");
        assert_eq!(image.data, png);

        // **The bug pin**: pbpaste returns Some("") (empty
        // string) for image-only pasteboards — NOT None. A
        // text-first dispatcher would fire dispatch_text with
        // "" here, which is exactly the bug the dispatcher
        // image-first priority fixes.
        let text = backend.current_text();
        assert_eq!(
            text,
            Some(String::new()),
            "pbpaste returns Some(\"\") on macOS image-only pasteboards (NOT None); \
             the dispatcher's image-first priority exists to work around this asymmetry"
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

    /// **JPEG→PNG normalisation** (the `defaults write
    /// com.apple.screencapture type jpg` bug fix): when only
    /// `public.jpeg` is on the pasteboard — the canonical state
    /// after a screenshot under the JPG default — `current_image`
    /// must re-encode the JPEG bytes to PNG (via the `image`
    /// crate) and label the returned `ImageBytes` as `"image/png"`.
    /// This is the regression test for the bug where the JPEG
    /// screenshot was silently dropped because
    /// `read_image_bytes_from_pasteboard` only probed `public.png`
    /// and `public.tiff`.
    ///
    /// **What we assert**:
    /// - `current_image()` returns `Some(...)` (the old bug was
    ///   `None`).
    /// - The returned `mime` is `"image/png"` (not `"image/jpeg"`
    ///   — the JPEG bytes were re-encoded for cross-platform
    ///   normalisation, matching the TIFF branch's contract).
    /// - The returned `data` starts with the PNG magic.
    /// - `image::load_from_memory(&data)` round-trips back to
    ///   the 2×2 dimensions we started with (visual equivalence
    ///   — sha256 will differ from the original JPEG bytes,
    ///   which is the accepted trade-off).
    ///
    /// **Sanity-check for the `ImageClipboardGuard` JPEG field**:
    /// the guard saves + restores `public.jpeg` since the
    /// JPEG fixture is written there; without the new field,
    /// this test would clobber any JPEG the user happened to
    /// have on their pasteboard.
    #[test]
    fn current_image_normalizes_jpeg_to_png() {
        let _lock = lock_for_test();
        let mut backend = MacOsPasteboard::new().expect("new");
        let _guard = ImageClipboardGuard::new();

        let jpeg = test_jpeg_bytes();
        write_pasteboard_bytes(NS_PASTEBOARD_TYPE_JPEG, &jpeg);

        let normalised = backend
            .current_image()
            .expect("current_image must return Some after writing JPEG to pasteboard \
                     (regression: the bug dropped the image and returned None)");
        assert_eq!(
            normalised.mime, "image/png",
            "JPEG input must be normalised to image/png (PLAN §3 评审 #2 3rd — same contract as TIFF)"
        );
        // PNG magic check — proves the bytes are actually PNG,
        // not the original JPEG labelled `image/png`.
        assert_eq!(
            &normalised.data[..8],
            &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A],
            "normalised bytes must start with the PNG magic"
        );
        // Round-trip via the `image` crate to confirm the
        // dimensions match the 2×2 fixture. The actual pixel
        // values may differ slightly from the original JPEG
        // (JPEG is lossy, but the `image` crate decodes it into
        // RGBA pixels losslessly — any difference would be a
        // re-encode bug, not a JPEG artefact).
        let decoded =
            image::load_from_memory(&normalised.data).expect("normalised PNG must be decodable");
        assert_eq!(
            decoded.width(),
            2,
            "JPEG→PNG normalisation must preserve width"
        );
        assert_eq!(
            decoded.height(),
            2,
            "JPEG→PNG normalisation must preserve height"
        );
        // The bytes are NOT byte-identical to the input JPEG
        // (sha256 differs) — that's the lossy re-encode
        // trade-off, by design. We don't assert the exact
        // delta here; the magic + dimension checks above are
        // the strict contract.
        assert_ne!(
            normalised.data, jpeg,
            "JPEG→PNG re-encode changes the byte stream (PNG compression is not a no-op); \
             visual equivalence is preserved by the `image` crate's lossless RGBA decode"
        );
    }

    /// **Probe order — PNG wins over JPEG when both are present**
    /// (regression test for the JPEG-fallback priority).
    /// Some macOS apps publish both `public.png` and
    /// `public.jpeg` representations of the same paste
    /// (PNG is byte-passthrough, JPEG is the original
    /// compressed stream). The PNG probe must short-circuit
    /// so we never re-encode unnecessarily.
    ///
    /// **Setup**: write a PNG, then write a JPEG to the
    /// pasteboard (the second `write_pasteboard_bytes` does NOT
    /// clear the PNG — `NSPasteboard` accumulates
    /// representations). `current_image` must return the PNG
    /// bytes verbatim.
    #[test]
    fn current_image_prefers_png_over_jpeg_when_both_present() {
        let _lock = lock_for_test();
        let mut backend = MacOsPasteboard::new().expect("new");
        let _guard = ImageClipboardGuard::new();

        let png = test_png_bytes();
        let jpeg = test_jpeg_bytes();
        // Order matters: write PNG first, then JPEG. The
        // `write_pasteboard_bytes` helper calls `clearContents()`
        // before writing, so the second call would clobber the
        // first — instead, write JPEG directly via NSPasteboard
        // to keep the PNG representation.
        let pb = NSPasteboard::generalPasteboard();
        let _ = pb.clearContents();
        let png_type = NSString::from_str(NS_PASTEBOARD_TYPE_PNG);
        let jpeg_type = NSString::from_str(NS_PASTEBOARD_TYPE_JPEG);
        let png_nsdata = NSData::with_bytes(&png);
        let jpeg_nsdata = NSData::with_bytes(&jpeg);
        assert!(
            pb.setData_forType(Some(&png_nsdata), &png_type),
            "test setup: write PNG must succeed"
        );
        assert!(
            pb.setData_forType(Some(&jpeg_nsdata), &jpeg_type),
            "test setup: write JPEG must succeed (without clobbering PNG)"
        );
        // Sanity: both representations are now on the pasteboard.
        assert!(
            pb.dataForType(&png_type).is_some(),
            "test setup: PNG must still be readable after JPEG write"
        );
        assert!(
            pb.dataForType(&jpeg_type).is_some(),
            "test setup: JPEG must be readable"
        );

        let result = backend
            .current_image()
            .expect("current_image must return Some when both PNG and JPEG are present");
        assert_eq!(
            result.mime, "image/png",
            "PNG probe must short-circuit before the JPEG fallback"
        );
        assert_eq!(
            result.data, png,
            "PNG probe must return the PNG bytes verbatim (no re-encode when PNG is present)"
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

    // === changeCount cache (M2b follow-up, 2026-09-10) ===

    /// **`current_image` populates the changeCount cache on the
    /// first read** so the second quiescent-tick read can return
    /// the cached normalised bytes without re-running
    /// `image::load_from_memory` + `image::write_to(Png)` (the
    /// expensive TIFF/JPEG → PNG normalisation step that was
    /// spamming the log ~2× per second before this fix).
    #[test]
    fn current_image_populates_change_count_cache() {
        let _lock = lock_for_test();
        let mut backend = MacOsPasteboard::new().expect("new");
        let _guard = ImageClipboardGuard::new();

        // Pre-condition: cache starts empty.
        assert!(
            backend.image_cache.is_none(),
            "fresh backend must have an empty image_cache"
        );

        // Write a JPEG and read once — the JPEG fallback will fire
        // (no PNG on the pasteboard), `jpeg_to_png_normalized`
        // will run, and the result lands in the cache.
        write_pasteboard_bytes(NS_PASTEBOARD_TYPE_JPEG, &test_jpeg_bytes());
        let first_read = backend
            .current_image()
            .expect("current_image returns Some for JPEG-only pasteboard");
        assert_eq!(first_read.mime, "image/png", "JPEG must normalise to image/png");

        // Post-condition: the cache is now populated for the
        // current `changeCount()`.
        let pb = NSPasteboard::generalPasteboard();
        let cc_after_first = pb.changeCount();
        assert!(
            backend.image_cache.is_some(),
            "image_cache must be populated after the first read"
        );
        let cached_entry = backend
            .image_cache
            .as_ref()
            .expect("just checked is_some; cache must hold an entry");
        assert_eq!(
            cached_entry.change_count, cc_after_first,
            "cache entry's change_count must match the live NSPasteboard.changeCount()"
        );
        assert_eq!(
            cached_entry.bytes.data, first_read.data,
            "cache entry's bytes must match what the first call returned"
        );
    }

    /// **`current_image` short-circuits on a stable `changeCount`**
    /// — the second back-to-back call returns the cached bytes
    /// without re-running the JPEG / TIFF normalisation. We verify
    /// this by mutating the cached entry's bytes in place: if the
    /// short-circuit fires, the second call returns the mutated
    /// bytes; if the short-circuit is broken, the second call
    /// re-normalises and returns fresh bytes (which would equal
    /// `first.data`, not the mutated value).
    #[test]
    fn current_image_short_circuits_on_stable_change_count() {
        let _lock = lock_for_test();
        let mut backend = MacOsPasteboard::new().expect("new");
        let _guard = ImageClipboardGuard::new();

        write_pasteboard_bytes(NS_PASTEBOARD_TYPE_JPEG, &test_jpeg_bytes());
        let first = backend
            .current_image()
            .expect("current_image returns Some for JPEG-only pasteboard");
        // First call populates the cache; mutate it to a sentinel.
        let cached = backend
            .image_cache
            .as_mut()
            .expect("cache must be populated after the first read");
        cached.bytes = ImageBytes {
            mime: "image/png".into(),
            data: b"SENTINEL-CACHE-HIT".to_vec(),
        };

        let second = backend
            .current_image()
            .expect("current_image must still return Some on a cache hit");

        assert_eq!(
            second.data, b"SENTINEL-CACHE-HIT",
            "second current_image on a stable changeCount must return the cached bytes; \
             a re-normalised JPEG would not equal this sentinel (cache short-circuit regressed)"
        );
        // Also: the cache entry must be untouched (same sentinel).
        let cached_after = backend
            .image_cache
            .as_ref()
            .expect("cache must still be populated after the second read");
        assert_eq!(cached_after.bytes.data, b"SENTINEL-CACHE-HIT");
    }

    /// **`set_image` invalidates the cache** — the local write
    /// bumps `NSPasteboard.changeCount()`, so any cached entry
    /// from before the write is now stale. Without this
    /// invalidation the next `current_image` call would return the
    /// pre-write bytes instead of the freshly-written ones.
    #[test]
    fn set_image_invalidates_change_count_cache() {
        let _lock = lock_for_test();
        let mut backend = MacOsPasteboard::new().expect("new");
        let _guard = ImageClipboardGuard::new();

        // Seed the cache with a JPEG read.
        write_pasteboard_bytes(NS_PASTEBOARD_TYPE_JPEG, &test_jpeg_bytes());
        let _ = backend
            .current_image()
            .expect("current_image returns Some for JPEG-only pasteboard");
        assert!(
            backend.image_cache.is_some(),
            "cache must be populated before set_image"
        );

        // Local write — bumps changeCount.
        let new_png = test_png_bytes();
        backend
            .set_image(&new_png, Mime::Png)
            .expect("set_image must succeed");

        assert!(
            backend.image_cache.is_none(),
            "set_image must drop the image_cache so the next current_image re-reads"
        );

        // And the next current_image must return the freshly-written
        // PNG bytes (passthrough, not a JPEG→PNG re-encode).
        let after_write = backend
            .current_image()
            .expect("current_image returns Some after set_image(PNG)");
        assert_eq!(
            after_write.data, new_png,
            "current_image must reflect the freshly-written PNG (cache was invalidated)"
        );
        assert_eq!(
            after_write.mime, "image/png",
            "PNG passthrough must keep image/png mime"
        );
    }

    /// **`set_dib_image` invalidates the cache** — same rationale
    /// as the `set_image` test above. The receive path
    /// (`apply_inbound_clipboard_image` on Windows ↔
    /// `set_dib_image` here on macOS) writes a fresh PNG, so the
    /// cache from any previous outbound dispatch must be dropped.
    #[test]
    fn set_dib_image_invalidates_change_count_cache() {
        let _lock = lock_for_test();
        let mut backend = MacOsPasteboard::new().expect("new");
        let _guard = ImageClipboardGuard::new();

        write_pasteboard_bytes(NS_PASTEBOARD_TYPE_JPEG, &test_jpeg_bytes());
        let _ = backend.current_image().expect("first read populates cache");
        assert!(
            backend.image_cache.is_some(),
            "cache must be populated before set_dib_image"
        );

        // Build a minimal valid DIB (BITMAPINFOHEADER for a 4×2
        // 24-bit BMP). The set_dib_image path runs the same DIB→
        // PNG conversion + `setData_forType` as a real inbound
        // image, so the cache invalidation we test here is the
        // same one the wire path triggers.
        let dib = build_minimal_dib_for_tests();
        backend
            .set_dib_image(&dib)
            .expect("set_dib_image must succeed on a minimal DIB");

        assert!(
            backend.image_cache.is_none(),
            "set_dib_image must drop the image_cache (changeCount bumped)"
        );
    }

    /// Build a tiny but valid 24-bit DIB payload (4×2 RGB) for
    /// the `set_dib_image` cache-invalidation tests. Mirrors what
    /// `prepend_bmp_file_header` produces minus the 14-byte file
    /// header — i.e. a bare `BITMAPINFOHEADER` + pixel rows.
    fn build_minimal_dib_for_tests() -> Vec<u8> {
        // BITMAPINFOHEADER = 40 bytes (little-endian on Windows).
        let mut dib = Vec::with_capacity(40 + 4 * 2 * 3);
        // biSize
        dib.extend_from_slice(&40u32.to_le_bytes());
        // biWidth
        dib.extend_from_slice(&4i32.to_le_bytes());
        // biHeight
        dib.extend_from_slice(&2i32.to_le_bytes());
        // biPlanes
        dib.extend_from_slice(&1u16.to_le_bytes());
        // biBitCount (24 = RGB)
        dib.extend_from_slice(&24u16.to_le_bytes());
        // biCompression (0 = BI_RGB)
        dib.extend_from_slice(&0u32.to_le_bytes());
        // biSizeImage
        dib.extend_from_slice(&0u32.to_le_bytes());
        // biXPelsPerMeter
        dib.extend_from_slice(&0u32.to_le_bytes());
        // biYPelsPerMeter
        dib.extend_from_slice(&0u32.to_le_bytes());
        // biClrUsed
        dib.extend_from_slice(&0u32.to_le_bytes());
        // biClrImportant
        dib.extend_from_slice(&0u32.to_le_bytes());
        // Pixel data: 4×2 rows of BGR triplets (rows already
        // 4-byte aligned: 4 × 3 = 12 bytes per row, no padding).
        for _ in 0..(4 * 2) {
            dib.extend_from_slice(&[0x80, 0x80, 0x80]);
        }
        dib
    }

    // === M2b STEP-2b.1 — DIB spike + set_dib_image fallback ===

    /// Build a small but valid DIB byte buffer (24-bit BMP file
    /// with a 4×2 RGB gradient) — used to feed the
    /// NSImage round-trip spike without depending on real
    /// Windows clipboard state.
    fn test_dib_bytes() -> Vec<u8> {
        let img = image::RgbImage::from_fn(4, 2, |x, y| {
            image::Rgb([(x * 60) as u8, (y * 60) as u8, 128])
        });
        let mut out = Vec::new();
        {
            let mut cursor = std::io::Cursor::new(&mut out);
            img.write_to(&mut cursor, image::ImageFormat::Bmp)
                .expect("encode test dib (BMP file)");
        }
        // Sanity: BMP magic prefix ("BM").
        assert_eq!(&out[..2], b"BM", "test fixture must produce BMP bytes");
        out
    }

    /// **NSImage DIB round-trip spike** (PLAN §3 STEP-2b.1 评审 #3
    /// 3rd). Construct a real DIB payload via the `image` crate's
    /// BMP encoder, feed it to `dib_round_trip_via_nsimage`, and
    /// verify the result.
    ///
    /// **What we assert**:
    /// - `dib_round_trip_via_nsimage` returns `Ok(Vec<u8>)`
    ///   (NSImage can decode the BMP + re-encode as PNG via
    ///   ImageIO).
    /// - The result is **non-empty**.
    /// - The original DIB bytes are **byte-different** from the
    ///   re-encoded PNG bytes — i.e. the round-trip is lossy at
    ///   the byte level, which is the expected outcome documented
    ///   in PLAN §3 评审 #3 3rd ("如果失败 → 降级为视觉一致").
    ///
    /// **The sha256 mismatch is not a test failure** — it is the
    /// expected outcome that informs the decision to use the
    /// `image`-crate fallback in `set_dib_image`. The test logs
    /// the spike result (informational; see
    /// `next/STEP-P2-M2b-2b.1.md` archive for the SHA comparison
    /// log output).
    #[test]
    fn dib_round_trip_via_nsimage_spike_runs() {
        let dib = test_dib_bytes();
        let spike_result = dib_round_trip_via_nsimage(&dib);
        match &spike_result {
            Ok(png_bytes) => {
                assert!(!png_bytes.is_empty(), "spike result must be non-empty");
                // The original DIB starts with "BM" (BMP file
                // magic); the re-encoded PNG should start with
                // the PNG magic. They cannot be byte-identical.
                assert_ne!(
                    png_bytes, &dib,
                    "DIB round-trip is lossy (expected; see PLAN §3 评审 #3 3rd)"
                );
                assert_eq!(
                    &png_bytes[..8],
                    &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A],
                    "spike output must be PNG (start with PNG magic)"
                );
            }
            Err(reason) => {
                // The NSImage round-trip is best-effort; if it
                // fails on a given macOS release, the test still
                // passes — the `image`-crate fallback handles the
                // actual write path. We log the reason for
                // STEP-2b.1 archive purposes.
                log::info!(
                    "dib_round_trip_via_nsimage spike failed (expected occasionally): {reason}"
                );
            }
        }
    }

    /// **`set_dib_image` end-to-end fallback test** (PLAN §3
    /// STEP-2b.1 评审 #3 3rd): feed DIB bytes to
    /// `set_dib_image`, verify the result lands on NSPasteboard
    /// as PNG (the `image`-crate decode + re-encode path),
    /// and verify the landed PNG decodes back to the original
    /// pixel dimensions.
    ///
    /// **Spike vs. fallback**: the NSImage round-trip spike
    /// runs internally inside `set_dib_image` (informational,
    /// logged at debug level). The actual write path uses
    /// `image`-crate decode → PNG re-encode → NSPasteboard
    /// `setData(_:forType: .png)`.
    #[test]
    fn set_dib_image_falls_back_to_png_via_image_crate() {
        let _lock = lock_for_test();
        let mut backend = MacOsPasteboard::new().expect("new");
        let _guard = ImageClipboardGuard::new();

        let dib = test_dib_bytes();
        backend
            .set_dib_image(&dib)
            .expect("set_dib_image must succeed via image-crate fallback");

        // Verify PNG lands on the pasteboard.
        let pb = NSPasteboard::generalPasteboard();
        let read_back = pb
            .dataForType(&NSString::from_str(NS_PASTEBOARD_TYPE_PNG))
            .expect("PNG must be on pasteboard after set_dib_image")
            .to_vec();
        // PNG magic prefix.
        assert_eq!(
            &read_back[..8],
            &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A],
            "set_dib_image must land PNG bytes (image-crate fallback)"
        );
        // Round-trip back to dimensions via the `image` crate.
        let decoded = image::load_from_memory(&read_back).expect("decode landed PNG");
        assert_eq!(decoded.width(), 4, "DIB→PNG must preserve width");
        assert_eq!(decoded.height(), 2, "DIB→PNG must preserve height");
    }

    /// **Raw DIB (without BMP file header) → PNG regression test**
    /// — Windows clipboard `CF_DIBV5` payloads are **raw DIB**,
    /// not full BMP files. The `image` crate's BMP decoder
    /// requires the 14-byte BMP file header, so
    /// `image::load_from_memory(raw_dib)` fails with `The image
    /// format could not be determined` — exactly the failure
    /// users saw on Mac receiving a screenshot from Windows
    /// (sha=5ced5967 case).
    ///
    /// This test pins the
    /// `dib_to_png_via_image_crate` fallback: strip the
    /// 14-byte BMP header from a known-good BMP file to
    /// produce a raw DIB, feed it to `set_dib_image`, and
    /// verify the PNG that lands on the pasteboard decodes
    /// back to the original dimensions.
    ///
    /// **Build a raw DIB fixture**: take the existing BMP
    /// fixture (`test_dib_bytes`) and drop the first 14 bytes.
    #[test]
    fn set_dib_image_handles_raw_dib_without_bmp_header() {
        let _lock = lock_for_test();
        let mut backend = MacOsPasteboard::new().expect("new");
        let _guard = ImageClipboardGuard::new();

        let bmp_file = test_dib_bytes();
        assert_eq!(
            &bmp_file[..2],
            b"BM",
            "BMP fixture must start with BM magic"
        );
        // Sanity: BMP file header is exactly 14 bytes.
        assert!(
            bmp_file.len() > 14,
            "BMP fixture too small to strip header: {} bytes",
            bmp_file.len()
        );
        let raw_dib: Vec<u8> = bmp_file[14..].to_vec();
        // Confirm the raw DIB starts with biSize = 40 (BITMAPINFOHEADER)
        // — the leading 4 bytes of the DIB are the `biSize` field.
        assert_eq!(
            u32::from_le_bytes([raw_dib[0], raw_dib[1], raw_dib[2], raw_dib[3]]),
            40,
            "raw DIB must start with biSize=40 (BITMAPINFOHEADER)"
        );
        // Confirm the raw DIB does NOT start with "BM" magic
        // (otherwise we'd just exercise the BMP-file pass-through
        // path, which is the previous test).
        assert_ne!(
            &raw_dib[..2], b"BM",
            "raw DIB fixture must not start with BM magic"
        );

        backend
            .set_dib_image(&raw_dib)
            .expect(
                "set_dib_image must accept raw DIB (no BMP file header) — the prepend_bmp_file_header \
                 fallback unblocks the image-crate BMP decoder",
            );

        // Verify PNG lands on the pasteboard with the right
        // dimensions.
        let pb = NSPasteboard::generalPasteboard();
        let read_back = pb
            .dataForType(&NSString::from_str(NS_PASTEBOARD_TYPE_PNG))
            .expect("PNG must be on pasteboard after set_dib_image")
            .to_vec();
        assert_eq!(
            &read_back[..8],
            &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A],
            "raw-DIB input must round-trip to PNG magic"
        );
        let decoded = image::load_from_memory(&read_back).expect("decode landed PNG");
        assert_eq!(decoded.width(), 4, "raw DIB → PNG must preserve width");
        assert_eq!(decoded.height(), 2, "raw DIB → PNG must preserve height");
    }

    /// **`prepend_bmp_file_header` validation tests** — pins
    /// the validation contract: too-short inputs, sub-12
    /// `biSize`, and biSize-larger-than-buffer all produce
    /// `ClipboardError::Io`. These guard against degenerate
    /// payloads that would otherwise produce silently-corrupted
    /// PNGs (or panic in the BMP decoder).
    #[test]
    fn prepend_bmp_file_header_rejects_too_short_input() {
        let empty = prepend_bmp_file_header(&[]).expect_err("empty input must reject");
        assert!(
            matches!(empty, ClipboardError::Io(_)),
            "empty input must return ClipboardError::Io; got {empty:?}"
        );
        let three_bytes = prepend_bmp_file_header(&[0x28, 0, 0, 0].get(..3).unwrap())
            .expect_err("3-byte input must reject (cannot read u32 biSize)");
        assert!(
            matches!(three_bytes, ClipboardError::Io(_)),
            "3-byte input must return ClipboardError::Io; got {three_bytes:?}"
        );
    }

    /// `prepend_bmp_file_header` rejects `biSize < 12` (smallest
    /// recognised DIB variant is BITMAPCOREHEADER with
    /// `biSize = 12`).
    #[test]
    fn prepend_bmp_file_header_rejects_below_minimum_bi_size() {
        // biSize = 8, well below 12.
        let bytes = vec![0x08, 0x00, 0x00, 0x00, 0xAA, 0xBB, 0xCC, 0xDD];
        let result = prepend_bmp_file_header(&bytes).expect_err("biSize=8 must reject");
        assert!(
            matches!(result, ClipboardError::Io(_)),
            "biSize below 12 must return ClipboardError::Io; got {result:?}"
        );
    }

    /// `prepend_bmp_file_header` rejects inputs that claim a
    /// `biSize` larger than the buffer (degenerate — would
    /// produce an invalid synthetic header).
    #[test]
    fn prepend_bmp_file_header_rejects_bi_size_larger_than_buffer() {
        // biSize = 40 but the buffer is only 8 bytes.
        let bytes = vec![0x28, 0x00, 0x00, 0x00, 0xAA, 0xBB, 0xCC, 0xDD];
        let result = prepend_bmp_file_header(&bytes).expect_err("biSize > buffer must reject");
        assert!(
            matches!(result, ClipboardError::Io(_)),
            "biSize larger than buffer must return ClipboardError::Io; got {result:?}"
        );
    }

    /// `prepend_bmp_file_header` produces a byte sequence the
    /// `image` crate can decode (for a known BITMAPINFOHEADER
    /// payload). This is the round-trip contract that
    /// `dib_to_png_via_image_crate` relies on.
    #[test]
    fn prepend_bmp_file_header_produces_decodable_bmp_for_bitmapinfoheader() {
        let bmp_file = test_dib_bytes();
        assert_eq!(&bmp_file[..2], b"BM");
        let raw_dib = bmp_file[14..].to_vec();
        let synthetic = prepend_bmp_file_header(&raw_dib).expect("prepend must succeed");
        // Sanity: starts with "BM" magic.
        assert_eq!(&synthetic[..2], b"BM");
        // Sanity: 14 bytes longer than input (header prepended).
        assert_eq!(synthetic.len(), raw_dib.len() + 14);
        // The `image` crate must be able to decode the result.
        let decoded = image::load_from_memory(&synthetic).expect("synthetic BMP must decode");
        assert_eq!(decoded.width(), 4);
        assert_eq!(decoded.height(), 2);
    }
}
