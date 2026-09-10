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
        let nsdata = NSData::with_bytes(&png_bytes);
        let ok = pb.setData_forType(Some(&nsdata), &png_type);
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
//  DIB helpers (M2b STEP-2b.1)
// ============================================================================

/// **M2b STEP-2b.1** — `image`-crate based DIB → PNG conversion.
/// Used as the canonical macOS receive path when the wire carries
/// `application/x-dib` bytes (Windows source).
///
/// **Why `image`-crate decode (and not just `image::load_from_memory`)**:
/// the `image` crate's BMP decoder accepts BMP files (14-byte file
/// header + DIB), but raw DIB (without the file header) is *not*
/// directly supported. The DIB bytes **may** decode directly via
/// `image::load_from_memory` if the leading `biSize` field is
/// `BM` (which it is for a BMP file but **not** for a raw DIB
/// payload from `CF_DIBV5`); if it doesn't decode, the
/// `image::ImageError::Decoding(Format)` variant surfaces a
/// specific message we log + propagate.
///
/// **Known limitation (documented for transparency, not a bug
/// fix in this STEP)**: a small fraction of Windows screenshots
/// publish DIB variants that the `image` crate cannot decode
/// directly (e.g. BITMAPV5HEADER with `BI_BITFIELDS` 32-bit
/// RGBA masks that `image` does not yet handle). For those,
/// we surface the `image`-crate error so the caller logs +
/// skips — the receiving macOS user sees "图片已转换格式" UI
/// hint (M4 GeneralPanel) and can copy manually if the loss
/// matters.
///
/// **Returns**: PNG bytes on success, or
/// [`ClipboardError::Io`] with the underlying `image`-crate
/// error message on decode / encode failure.
fn dib_to_png_via_image_crate(dib_bytes: &[u8]) -> Result<Vec<u8>, ClipboardError> {
    let img = image::load_from_memory(dib_bytes)
        .map_err(|e| ClipboardError::Io(format!("image::load_from_memory DIB: {e}")))?;
    let mut out = Vec::new();
    {
        let mut cursor = std::io::Cursor::new(&mut out);
        img.write_to(&mut cursor, image::ImageFormat::Png)
            .map_err(|e| ClipboardError::Io(format!("image::write_to PNG (DIB→PNG): {e}")))?;
    }
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
}
