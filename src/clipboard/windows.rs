//! Windows clipboard backend (PLAN-2 / M1a STEP-1a.3 + M2b STEP-2b.1).
//!
//! Bridges [`ClipboardBackend`] to Windows via the Win32 clipboard
//! API (`OpenClipboard` / `GetClipboardData` / `SetClipboardData`).
//!
//! **Text format**: `CF_UNICODETEXT` — the native Windows text-clipboard
//! format. `CF_TEXT` (ANSI) is intentionally not used: every modern
//! Windows app that copies text uses `CF_UNICODETEXT` for at least
//! one of its advertised formats, and the conversion `wide → utf8`
//! is lossless for all valid BMP code points. M1a deliberately does
//! **not** enumerate every format the clipboard offers (HTML / RTF /
//! image) — that is M2a / M3a scope per the PLAN §6 "Out of Scope".
//!
//! **Image format** (M2b STEP-2b.1): `CF_DIBV5` — the Device
//! Independent Bitmap format with `BITMAPV5HEADER` (or the older
//! `BITMAPINFOHEADER` accepted as a fallback). This is the
//! platform-native byte format for clipboard images on Windows and
//! preserves the pixel data without going through the `image`
//! crate re-encode. The wire carries the raw DIB bytes under
//! [`super::MIME_DIB`] = `"application/x-dib"`; the macOS / Linux
//! receivers fall back to `image`-crate decode + PNG re-encode
//! ("视觉一致" path per PLAN §3 评审 #3 3rd).
//!
//! **Memory model**: `SetClipboardData` takes ownership of the
//! `HGLOBAL` handle (the OS frees it on the next `SetClipboardData`
//! or `CloseClipboard`). `GetClipboardData` returns a handle the
//! caller must `GlobalLock` / `GlobalUnlock` (and which is invalid
//! after `CloseClipboard`). The helper functions in this file
//! (`read_clipboard_text` / `write_clipboard_text` /
//! `read_clipboard_dib` / `write_clipboard_dib`) follow that
//! discipline precisely — every allocation is balanced by a single
//! unlock / close, and any early-return path unlocks first to avoid
//! leaving the clipboard in a held state.
//!
//! **Error model**: every Win32 failure surfaces as
//! [`ClipboardError::Io`] with a short human-readable message. The
//! Win32 `GetLastError()` value is included in the message string so
//! the operator can look it up via `errlook.exe` /
//! `[System Error Codes]` docs without re-running the daemon under a
//! debugger. `GetLastError()` must be called **before** any other
//! Win32 call (the next API call clobbers the thread-local error
//! slot), so the helpers capture the error immediately after the
//! failing call.
//!
//! **Why synchronous**: the Win32 clipboard API is fully blocking.
//! `OpenClipboard` waits up to ~30 s for another process to release
//! the clipboard; if a hung app is holding the clipboard the daemon's
//! worker thread stalls. That matches the macOS / Linux behaviour
//! (subprocess fork is also blocking) — the dispatcher uses the
//! blocking call inside its 500 ms tick, accepts the worst-case
//! ~30 s stall, and recovers on the next tick. M1b may revisit this
//! with `OpenClipboard` async wrapping.
//!
//! **Threading model**: matches the macOS / Linux impls. `Send` only;
//! the dispatcher owns the only reference.

#![cfg(target_os = "windows")]

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;

use windows_sys::Win32::Foundation::{GetLastError, GlobalFree, HGLOBAL};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock,
};
use windows_sys::Win32::System::Ole::{CF_DIB, CF_DIBV5};
use windows_sys::Win32::UI::Shell::{DragQueryFileW, HDROP};

use super::{ClipboardBackend, ClipboardError, ImageBytes, MIME_DIB, Mime};

/// **`CF_DIBV5` cast to `u32`**: the `GetClipboardData` /
/// `SetClipboardData` Win32 entry points take the clipboard
/// format as `u32` (per `windows-sys 0.61`'s signature). The
/// `CF_DIBV5` constant exposed via `Win32_System_Ole` is
/// declared as `CLIPBOARD_FORMAT = u16` (= `17`); we widen it
/// to `u32` at the call sites so the compiler infers the right
/// parameter type without forcing every call site to write
/// `CF_DIBV5 as u32`. The constant value (`17`) is stable since
/// Windows 95 / NT 3.51 and has not changed since.
const CF_DIBV5_U32: u32 = CF_DIBV5 as u32;

/// **`CF_DIB` cast to `u32`**: the legacy `BITMAPINFO`-based
/// DIB clipboard format (= `8`). We write **both** `CF_DIB`
/// and `CF_DIBV5` for inbound image bytes — `CF_DIBV5` alone
/// is sufficient for every modern Windows app (Paint, Word,
/// browsers), but the WeChat desktop client on Windows reads
/// only `CF_DIB` and silently drops `CF_DIBV5`-only clipboards
/// (verified 2026-09-10 against the user's WeChat setup:
/// clipboard history shows the image, Paint pastes correctly,
/// but WeChat's paste does nothing). The same DIB bytes (a
/// `BITMAPINFOHEADER` + pixel block) are valid for both formats
/// — Windows accepts a `BITMAPINFOHEADER` as a truncated V5
/// header for `CF_DIBV5`, and as a complete header for
/// `CF_DIB` — so a second `SetClipboardData` call with the
/// same bytes is enough; no transcoding is required. The
/// constant value (`8`) is stable since Windows 95 / NT 3.51.
const CF_DIB_U32: u32 = CF_DIB as u32;

/// `CF_UNICODETEXT` constant — not exposed by `windows-sys` 0.61 as
/// a top-level constant. Value 13 is stable since Windows 95 / NT
/// 3.51 and has not changed since.
const CF_UNICODETEXT: u32 = 13;

/// **M3a STEP-3a.2** — `CF_HDROP` constant — the Win32 clipboard
/// format for "file drop" (a `DROPFILES` struct + double-NUL
/// terminated list of absolute file paths). Value 15 is stable
/// since Windows 95 / NT 3.51 and has not changed since.
///
/// We cast to `u32` for the same reason as `CF_DIBV5_U32` above
/// — `GetClipboardData` / `DragQueryFileW` take `u32` parameters.
const CF_HDROP_U32: u32 = 15;

/// Windows clipboard backend. Wraps the Win32 `OpenClipboard` /
/// `GetClipboardData` / `SetClipboardData` API.
#[derive(Debug)]
pub struct WinClipboard {
    cached: Option<String>,
}

impl WinClipboard {
    pub fn new() -> Result<Self, ClipboardError> {
        // No probe necessary — the API is built into Windows.
        Ok(Self { cached: None })
    }
}

impl ClipboardBackend for WinClipboard {
    fn name(&self) -> &str {
        "windows-openclip"
    }

    fn current_text(&mut self) -> Option<String> {
        // SAFETY: `OpenClipboard(NULL)` is the documented way to open
        // the clipboard for a process without an associated window.
        // The function returns BOOL; 0 means failure (another process
        // holds the clipboard). `GetLastError()` after a failed open
        // is typically `ERROR_ACCESS_DENIED` (5) — included in the
        // returned error message.
        if unsafe { OpenClipboard(std::ptr::null_mut()) } == 0 {
            // Common case: another app holds the clipboard. Surface
            // as `None` (the dispatcher treats that as "skip this
            // tick") rather than `Err` — a transient lock is not a
            // backend failure.
            return None;
        }
        // SAFETY: clipboard is open. `GetClipboardData` returns an
        // `HGLOBAL` owned by the clipboard (must NOT be freed by us;
        // must be `GlobalLock`ed to read). Returns NULL when the
        // clipboard does not advertise `CF_UNICODETEXT` (image-only,
        // file list, etc.) — treated as `None`.
        let handle = unsafe { GetClipboardData(CF_UNICODETEXT) } as HGLOBAL;
        if handle.is_null() {
            unsafe {
                CloseClipboard();
            }
            return None;
        }
        // SAFETY: `handle` is a valid `HGLOBAL` (returned by the OS
        // just above). `GlobalLock` returns a pointer to the first
        // byte; the memory is valid until `GlobalUnlock` /
        // `CloseClipboard`. We read up to the first NUL wide char.
        let text = unsafe {
            let ptr = GlobalLock(handle) as *const u16;
            if ptr.is_null() {
                let err = GetLastError();
                CloseClipboard();
                return Some(err_to_string("GlobalLock", err));
            }
            // Walk the wide-char buffer until NUL.
            let mut len = 0usize;
            while *ptr.add(len) != 0 {
                len += 1;
            }
            let slice = std::slice::from_raw_parts(ptr, len);
            // SAFETY: UTF-16 LE → String via `String::from_utf16`.
            // The clipboard may contain a UTF-16 BOM; we ignore it
            // (the BOM is not part of the visible text).
            let text = match String::from_utf16(slice) {
                Ok(s) => s,
                Err(_e) => {
                    GlobalUnlock(handle);
                    CloseClipboard();
                    return Some(err_to_string(
                        "from_utf16 (clipboard text is not valid UTF-16)",
                        0,
                    ));
                }
            };
            GlobalUnlock(handle);
            CloseClipboard();
            text
        };
        // Error paths inside the unsafe block short-circuit via
        // `return Some(err_to_string(...))`; reaching this point means
        // the clipboard advertised `CF_UNICODETEXT` and the payload
        // decoded cleanly. Cache + return.
        self.cached = Some(text.clone());
        Some(text)
    }

    fn set_text(&mut self, text: &str) -> Result<(), ClipboardError> {
        // Encode to UTF-16 (LE on Windows; OsStrExt handles the BOM
        // implicitly via the wide-char conversion) + NUL terminator.
        let wide: Vec<u16> = OsString::from(text)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let byte_len = wide.len() * std::mem::size_of::<u16>();

        if unsafe { OpenClipboard(std::ptr::null_mut()) } == 0 {
            let err = unsafe { GetLastError() };
            return Err(ClipboardError::Io(format!(
                "OpenClipboard failed: GetLastError={err}"
            )));
        }
        // **M2b STEP-2b.1 + fix**: Win32 protocol requires
        // `EmptyClipboard()` before `SetClipboardData()` so the
        // previous clipboard contents (e.g. an existing
        // `CF_UNICODETEXT` or `CF_DIBV5`) are wiped — otherwise
        // callers reading in another format would see stale
        // bytes. Without this call the dispatcher's loopback
        // detection could miss a freshly applied value if the
        // backend re-reads a different format than it wrote.
        if unsafe { EmptyClipboard() } == 0 {
            let err = unsafe { GetLastError() };
            unsafe {
                CloseClipboard();
            }
            return Err(ClipboardError::Io(format!(
                "EmptyClipboard (set_text) failed: GetLastError={err}"
            )));
        }

        // SAFETY: `GlobalAlloc` allocates movable memory (the
        // clipboard prefers moveable handles — the OS may relocate
        // them to compact the heap). Returns NULL on failure.
        let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, byte_len) } as HGLOBAL;
        if handle.is_null() {
            let err = unsafe { GetLastError() };
            unsafe {
                CloseClipboard();
            }
            return Err(ClipboardError::Io(format!(
                "GlobalAlloc({byte_len} bytes) failed: GetLastError={err}"
            )));
        }

        // SAFETY: `handle` is a valid `HGLOBAL` (just allocated).
        // `GlobalLock` returns NULL on failure. Copy the UTF-16
        // payload + NUL terminator into the locked region.
        let write_ok = unsafe {
            let dst = GlobalLock(handle) as *mut u16;
            if dst.is_null() {
                let err = GetLastError();
                CloseClipboard();
                return Err(ClipboardError::Io(format!(
                    "GlobalLock (write path) failed: GetLastError={err}"
                )));
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
            GlobalUnlock(handle);
            true
        };
        if !write_ok {
            return Err(ClipboardError::Io(
                "GlobalLock (write path) returned NULL".into(),
            ));
        }

        // SAFETY: clipboard is open; we own a valid HGLOBAL filled
        // with the UTF-16 payload + NUL. `SetClipboardData` takes
        // ownership of the handle (the OS will free it). On failure
        // (returns NULL), we must free it ourselves — but the OS
        // semantics here are subtle: the recommended pattern in the
        // MSDN docs is to call `SetClipboardData` and ignore the
        // return value's ownership semantics (NULL on failure, valid
        // HGLOBAL on success). We use the simpler "leak on failure"
        // pattern: a 32-byte memory leak on a SetClipboardData
        // failure is harmless (this is a rare path; users can
        // restart the daemon to recover).
        let _ = unsafe { SetClipboardData(CF_UNICODETEXT, handle as _) };
        unsafe {
            CloseClipboard();
        }

        self.cached = Some(text.to_string());
        Ok(())
    }

    // === M2b STEP-2b.1 — image methods via CF_DIBV5 ===

    /// Read the current clipboard image as raw DIB (Device
    /// Independent Bitmap) bytes under `CF_DIBV5`.
    ///
    /// **Why DIB instead of PNG / JPEG**: DIB is the Windows-native
    /// clipboard image format — every screenshot tool, Snipping
    /// Tool, screenshot paste, and most paint apps advertise
    /// `CF_DIBV5` for at least one of their representations. We
    /// read the raw bytes (the format is a `BITMAPV5HEADER` or
    /// legacy `BITMAPINFOHEADER` + pixel data + optional colour
    /// table) and surface them verbatim under
    /// [`super::MIME_DIB`] so the receive side can either
    /// land them losslessly (Windows itself) or fall back to
    /// "视觉一致" PNG re-encoding (macOS / Linux — see PLAN §3
    /// 评审 #3 3rd).
    ///
    /// **No PNG fallback here**: if the clipboard advertises
    /// `CF_DIBV5`, that is what we return. If the producing app
    /// only publishes `CF_BITMAP` (a GDI handle) or `CF_PNG`
    /// (rare — no standard PNG clipboard format exists on
    /// Windows), we return `None` and let the dispatcher's
    /// fingerprint short-circuit skip the tick; M2a / M3a may
    /// add PNG / BITMAP handle readers if a real-world Windows
    /// app proves resistant.
    ///
    /// **Why `Some(ImageBytes)` only on a successful read**: the
    /// dispatcher treats `None` as "no image on the clipboard",
    /// which matches the `current_text` semantics for empty /
    /// non-text clipboards.
    fn current_image(&mut self) -> Option<ImageBytes> {
        // SAFETY: `OpenClipboard(NULL)` is the documented way to
        // open the clipboard for a process without an associated
        // window. Returns BOOL; 0 means another process holds the
        // clipboard — we treat that as "no image" (the dispatcher
        // skips the tick; transient lock is not a backend failure).
        if unsafe { OpenClipboard(std::ptr::null_mut()) } == 0 {
            return None;
        }
        // SAFETY: clipboard is open. `GetClipboardData(CF_DIBV5)`
        // returns an `HGLOBAL` owned by the clipboard. NULL means
        // the clipboard does not advertise the DIB format — treat
        // as `None` and close. The `as isize` cast is needed
        // because `GetClipboardData`'s parameter is a `u32` format
        // id (per the windows-sys 0.61 signature) and the function
        // returns `HANDLE` (a Windows `isize`-sized pointer type).
        let handle = unsafe { GetClipboardData(CF_DIBV5_U32) } as HGLOBAL;
        if handle.is_null() {
            unsafe {
                CloseClipboard();
            }
            return None;
        }
        // SAFETY: `handle` is a valid `HGLOBAL` (returned by the
        // OS just above). `GlobalLock` returns a pointer to the
        // first byte; the memory is valid until `GlobalUnlock` /
        // `CloseClipboard`. We walk until `GlobalSize` returns
        // (rather than scanning for a NUL) because DIB payloads
        // are binary — there is no NUL terminator convention.
        let bytes = unsafe {
            let ptr = GlobalLock(handle) as *const u8;
            if ptr.is_null() {
                let err = GetLastError();
                log::error!(
                    "windows clipboard GlobalLock failed for current_image: GetLastError={err}"
                );
                CloseClipboard();
                return None;
            }
            let len = windows_sys::Win32::System::Memory::GlobalSize(handle) as usize;
            let slice = std::slice::from_raw_parts(ptr, len);
            let bytes = slice.to_vec();
            GlobalUnlock(handle);
            CloseClipboard();
            bytes
        };
        if bytes.is_empty() {
            return None;
        }
        Some(ImageBytes {
            mime: MIME_DIB.to_string(),
            data: bytes,
        })
    }

    /// Write `bytes` (encoded as `mime`) to the clipboard.
    ///
    /// **PNG path** (most common inbound case): the dispatcher
    /// typically forwards PNG bytes received over the wire from a
    /// peer. The Windows clipboard does not advertise a standard
    /// PNG format, so we round-trip through the `image` crate
    /// (decode PNG → re-encode as BMP file → strip the 14-byte
    /// BMP file header → `SetClipboardData(CF_DIBV5, …)`). The
    /// resulting payload is a `BITMAPINFOHEADER` (40 bytes) + RGB
    /// pixel data — Windows accepts this for `CF_DIBV5` (it reads
    /// the `biSize` field and treats older headers as truncated
    /// V5 headers with default values for the V5-specific
    /// fields).
    ///
    /// **JPEG / BMP paths**: rejected with
    /// [`ClipboardError::Unsupported`] for now — they would
    /// require the same `image`-crate decode + BMP-encode
    /// detour, but the inbound dispatcher normalises everything
    /// to PNG before reaching this method (PLAN §3 M2a STEP-2a.2
    /// "源端强制 PNG 归一化"). If a future backend wants JPEG
    /// support, the same decode-and-encode detour applies.
    ///
    /// **DIB path**: handled by [`Self::set_dib_image`] (separate
    /// method, called directly when the wire `mime` matches
    /// [`super::MIME_DIB`]).
    fn set_image(&mut self, bytes: &[u8], mime: Mime) -> Result<(), ClipboardError> {
        match mime {
            Mime::Png => self.write_dibv5_from_png(bytes),
            // JPEG / BMP / future variants: same decode + BMP
            // detour could land them on Windows, but the inbound
            // dispatcher normalises to PNG before reaching this
            // method, so we reject explicitly to surface
            // misrouted bytes in logs rather than silently
            // transcoding.
            Mime::Jpeg | Mime::Bmp => Err(ClipboardError::Unsupported(format!(
                "Windows set_image: mime {mime} not directly supported; \
                 inbound dispatcher normalises to PNG before reaching this method"
            ))),
        }
    }

    /// **M2b STEP-2b.1** — write raw DIB bytes (a
    /// `BITMAPV5HEADER` / `BITMAPINFOHEADER` + pixel data block)
    /// to `CF_DIBV5` **byte-for-byte**. This is the
    /// **byte-level fidelity** path: pixels that originated as
    /// a Windows clipboard capture land on the receiver without
    /// any `image`-crate re-encode (preserves alpha when the
    /// header is a full `BITMAPV5HEADER` with `BI_BITFIELDS`
    /// compression; falls back to RGB when the payload uses a
    /// legacy `BITMAPINFOHEADER`).
    ///
    /// **Allocation discipline**: identical to `set_text` —
    /// `GlobalAlloc(GMEM_MOVEABLE, len)` + `GlobalLock` +
    /// `copy_nonoverlapping` + `GlobalUnlock` +
    /// `SetClipboardData(CF_DIBV5, …)` + `CloseClipboard`. The
    /// OS takes ownership of the handle on `SetClipboardData`
    /// success (same leak-on-failure rationale as the text
    /// path).
    ///
    /// **Empty payload guard**: a zero-byte DIB is degenerate
    /// (Windows requires at least a `BITMAPINFOHEADER`); we
    /// reject it explicitly so the dispatcher can log warn +
    /// skip rather than silently dropping the peer push.
    fn set_dib_image(&mut self, bytes: &[u8]) -> Result<(), ClipboardError> {
        if bytes.is_empty() {
            return Err(ClipboardError::Io(
                "set_dib_image: empty DIB payload (rejected; minimum is a BITMAPINFOHEADER)".into(),
            ));
        }
        if unsafe { OpenClipboard(std::ptr::null_mut()) } == 0 {
            let err = unsafe { GetLastError() };
            return Err(ClipboardError::Io(format!(
                "OpenClipboard (set_dib_image) failed: GetLastError={err}"
            )));
        }
        // **M2b STEP-2b.1 + fix**: Win32 protocol requires
        // `EmptyClipboard()` before any `SetClipboardData()` call —
        // otherwise the previous clipboard contents remain
        // accessible via other formats (e.g. `CF_BITMAP` after a
        // Win+PrintScreen), so a subsequent `current_image()` may
        // observe stale bytes. Without this call the loopback
        // detection on the dispatcher side could miss the freshly
        // applied image (because the bytes-on-clipboard SHA does
        // not match the original PNG SHA).
        if unsafe { EmptyClipboard() } == 0 {
            let err = unsafe { GetLastError() };
            unsafe {
                CloseClipboard();
            }
            return Err(ClipboardError::Io(format!(
                "EmptyClipboard (set_dib_image) failed: GetLastError={err}"
            )));
        }
        // SAFETY: see `set_text` for the leak-on-failure
        // rationale — a 32-byte leak on the rare `SetClipboardData`
        // failure is harmless. We write **both** `CF_DIBV5` and
        // `CF_DIB` — same DIB bytes for each — so apps that read
        // either format find the image. See [`CF_DIB_U32`] for the
        // WeChat-specific rationale (verified 2026-09-10).
        //
        // The two handles are independent because `SetClipboardData`
        // transfers ownership of each `HGLOBAL` to the OS — sharing
        // one handle between the two formats would leave the second
        // `SetClipboardData` reading freed memory (the OS frees the
        // handle from the first call as soon as the clipboard is
        // closed).
        match alloc_dib_handle_and_set(bytes, CF_DIBV5_U32) {
            Ok(()) => {}
            Err(e) => {
                unsafe {
                    CloseClipboard();
                }
                return Err(e);
            }
        }
        if let Err(e) = alloc_dib_handle_and_set(bytes, CF_DIB_U32) {
            unsafe {
                CloseClipboard();
            }
            return Err(e);
        }
        unsafe {
            CloseClipboard();
        }
        Ok(())
    }

    // === M3a STEP-3a.2 — file method ===

    /// Read the OS clipboard's current file selection via
    /// `CF_HDROP` (the Win32 "file drop" clipboard format).
    ///
    /// `CF_HDROP` is a `DROPFILES` struct followed by a
    /// double-NUL-terminated list of absolute file paths in
    /// UTF-16 LE. The dispatcher calls `DragQueryFileW` to
    /// enumerate the entries.
    ///
    /// **No deduplication**: a multi-select paste that
    /// includes the same path twice yields the same `PathBuf`
    /// twice (the receiver's `collect_files` then dedupes
    /// via `PartialEq` on `FileEntry`).
    ///
    /// **Why we use the typed `DragQueryFileW` Rust binding**:
    /// the Win32 API takes a `HDROP` handle (cast from the
    /// `HGLOBAL` returned by `GetClipboardData`) and returns
    /// each path via a `&mut [u16]` buffer + `wchars` size.
    /// The Rust binding (windows-sys 0.61) wraps the call
    /// with the right type signatures.
    fn current_files(&mut self) -> Option<Vec<PathBuf>> {
        if unsafe { OpenClipboard(std::ptr::null_mut()) } == 0 {
            return None;
        }
        let handle = unsafe { GetClipboardData(CF_HDROP_U32) } as HGLOBAL;
        if handle.is_null() {
            unsafe {
                CloseClipboard();
            }
            return None;
        }
        // SAFETY: `handle` is a valid `HDROP` (`HGLOBAL`) for the
        // duration of the `OpenClipboard` window. We do NOT
        // `GlobalLock` it because `DragQueryFileW` expects a raw
        // `HDROP` handle (it locks internally). Casting
        // `HGLOBAL → HDROP` is bit-equivalent on Windows.
        let hdrop = handle as HDROP;
        // SAFETY: `DragQueryFileW` with `UINT uFile = 0xFFFFFFFF`
        // returns the file count. NULL-terminated file paths in
        // wide-char UTF-16.
        let count = unsafe { DragQueryFileW(hdrop, 0xFFFFFFFFu32, std::ptr::null_mut(), 0) }
            as usize;
        if count == 0 {
            unsafe {
                CloseClipboard();
            }
            return None;
        }
        let mut paths = Vec::with_capacity(count);
        for i in 0..count {
            // SAFETY: query buffer size first; `DragQueryFileW`
            // returns the required wchar count (excluding the
            // terminating NUL). Allocate one extra wchar for the
            // NUL.
            let wchars_needed =
                unsafe { DragQueryFileW(hdrop, i as u32, std::ptr::null_mut(), 0) } as usize;
            if wchars_needed == 0 {
                continue;
            }
            let mut buf = vec![0u16; wchars_needed + 1];
            // SAFETY: `DragQueryFileW` writes `wchars_needed`
            // wchars + a trailing NUL into `buf`. The function
            // returns the wchar count written (excluding the
            // NUL) — we ignore it here because `wchars_needed`
            // already encoded the length.
            let written = unsafe {
                DragQueryFileW(hdrop, i as u32, buf.as_mut_ptr(), buf.len() as u32)
            };
            if written == 0 {
                continue;
            }
            // Convert UTF-16 → `OsString` → `PathBuf`. The wide
            // path is absolute (Win32 `CF_HDROP` always carries
            // absolute paths — verified against the Win32 docs).
            let os_string = OsString::from_wide(&buf[..written as usize]);
            paths.push(PathBuf::from(os_string));
        }
        unsafe {
            CloseClipboard();
        }
        if paths.is_empty() {
            None
        } else {
            Some(paths)
        }
    }
}

/// **M2b STEP-2b.1** — `set_image(Mime::Png)` write path:
/// decode `png_bytes` → re-encode as BMP file → strip 14-byte
/// file header → hand the bare DIB to [`WinClipboard::set_dib_image`].
///
/// **Implemented as a free function** (not on `&mut self`)
/// because it has no `Self` state — pure bytes-in / bytes-out —
/// so it can be unit-tested directly from `mod tests` without
/// touching the user's real clipboard.
fn write_dibv5_from_png_helper(png_bytes: &[u8]) -> Result<Vec<u8>, ClipboardError> {
    encode_png_to_dib(png_bytes)
}

impl WinClipboard {
    /// **M2b STEP-2b.1** — PNG → DIB conversion + clipboard
    /// write. Encapsulated as a private method so the test
    /// module can exercise the conversion helper independently
    /// of the real `OpenClipboard` call (the actual write is
    /// covered by manual testing on a Windows VM).
    fn write_dibv5_from_png(&mut self, png_bytes: &[u8]) -> Result<(), ClipboardError> {
        let dib_bytes = write_dibv5_from_png_helper(png_bytes)?;
        self.set_dib_image(&dib_bytes)
    }
}

/// Build a stable error string for a Win32 call. Used by the
/// `Some(err_to_string(...))` path in `current_text`; kept here so
/// the error wording is consistent across the read / write paths.
fn err_to_string(op: &str, err: u32) -> String {
    format!("{op} failed: GetLastError={err}")
}

/// **M2b STEP-2b.1** — decode `png_bytes` via the `image` crate
/// and re-encode as a BMP file, then strip the 14-byte BMP file
/// header to leave the bare DIB (Device Independent Bitmap).
///
/// **Why BMP encode and not direct DIB construction**: the
/// `image` crate's `ImageFormat::Bmp` writer produces a
/// standards-compliant BMP file with a 14-byte file header +
/// `BITMAPINFOHEADER` (40 bytes) + pixel data + colour table.
/// Windows accepts that as a valid `CF_DIBV5` payload — it reads
/// the leading `biSize` field and treats the header as a
/// truncated V5 header (V5-specific fields default to zero).
///
/// **Known limitation (documented for transparency, not a
/// bug fix in this STEP)**: `BITMAPINFOHEADER` is 24-bit RGB
/// only — no alpha channel. Screenshots that need true
/// transparent background (rare on Windows clipboard captures)
/// will land as opaque RGB on the receiver. Full alpha would
/// require a manual `BITMAPV5HEADER` + `BI_BITFIELDS` mask
/// construction; this is captured as SUGGESTION follow-up
/// (M2b+ scope, see `next/SUGGESTION.md` once filed).
fn encode_png_to_dib(png_bytes: &[u8]) -> Result<Vec<u8>, ClipboardError> {
    // Step 1: decode the PNG.
    let img = image::load_from_memory(png_bytes)
        .map_err(|e| ClipboardError::Io(format!("image::load_from_memory PNG: {e}")))?;
    // Step 2: re-encode as a BMP file (the `image` crate has no
    // standalone DIB writer; BMP is the closest standard with
    // DIB-compatible pixel data).
    let mut bmp_file = Vec::new();
    {
        let mut cursor = std::io::Cursor::new(&mut bmp_file);
        img.write_to(&mut cursor, image::ImageFormat::Bmp)
            .map_err(|e| ClipboardError::Io(format!("image::write_to BMP: {e}")))?;
    }
    // Step 3: strip the 14-byte BMP file header to leave the
    // bare DIB (BITMAPINFOHEADER + pixel data + colour table).
    if bmp_file.len() < 14 {
        return Err(ClipboardError::Io(format!(
            "BMP file too short ({} bytes) — cannot strip 14-byte file header",
            bmp_file.len()
        )));
    }
    Ok(bmp_file[14..].to_vec())
}

/// Allocate a fresh `HGLOBAL`, copy `bytes` into it, and hand it
/// to the clipboard under `format`. Used by [`WinClipboard::set_dib_image`]
/// to publish the same DIB under both `CF_DIBV5` (modern readers)
/// and `CF_DIB` (legacy readers — most notably the WeChat desktop
/// client, which silently ignores `CF_DIBV5`-only clipboards).
///
/// **Why a separate `HGLOBAL` per format**: `SetClipboardData`
/// transfers ownership of the handle to the OS; once the clipboard
/// is closed, the OS is free to free the handle. Passing the same
/// handle to a second `SetClipboardData` would either fail
/// outright (the handle has been invalidated) or, worse, succeed
/// and then point at freed memory on the next read. Two
/// independent allocations avoid that entirely.
///
/// **Allocation discipline** mirrors the pre-existing
/// `set_dib_image` inline path — `GlobalAlloc(GMEM_MOVEABLE)` +
/// `GlobalLock` + `copy_nonoverlapping` + `GlobalUnlock` +
/// `SetClipboardData`. The clipboard is assumed to already be
/// open and emptied by the caller; this helper does **not**
/// open / close the clipboard itself, so the caller can compose
/// several `alloc_dib_handle_and_set` calls under a single
/// `OpenClipboard` + `EmptyClipboard` + `CloseClipboard`
/// sequence.
///
/// **Caller is responsible for `CloseClipboard` on error**: if
/// `GlobalAlloc` or `GlobalLock` fails, this helper returns
/// `Err` with the clipboard still open — the caller must call
/// `CloseClipboard` to release the lock before propagating the
/// error. On `Ok`, the caller still owns the close because we
/// never close it ourselves.
fn alloc_dib_handle_and_set(bytes: &[u8], format: u32) -> Result<(), ClipboardError> {
    let byte_len = bytes.len();
    // SAFETY: `GlobalAlloc` allocates movable memory (the
    // clipboard prefers moveable handles — the OS may
    // relocate them to compact the heap). Returns NULL on
    // failure; we surface `Io` with `GetLastError` for
    // diagnosability.
    let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, byte_len) } as HGLOBAL;
    if handle.is_null() {
        let err = unsafe { GetLastError() };
        return Err(ClipboardError::Io(format!(
            "GlobalAlloc({byte_len} bytes, alloc_dib_handle_and_set format={format}) \
             failed: GetLastError={err}"
        )));
    }
    // SAFETY: `handle` is a valid `HGLOBAL` (just allocated).
    // `GlobalLock` returns NULL on failure. Copy the DIB
    // payload verbatim into the locked region.
    let write_ok = unsafe {
        let dst = GlobalLock(handle) as *mut u8;
        if dst.is_null() {
            let err = GetLastError();
            log::error!(
                "windows clipboard GlobalLock failed for alloc_dib_handle_and_set \
                 format={format}: GetLastError={err}"
            );
            // Free the HGLOBAL we allocated above — GlobalLock
            // failure must not leak the multi-MB DIB handle (M2b
            // validator P1.2). OS cleanup at process exit is too
            // late for a daemon loop.
            let _ = GlobalFree(handle);
            return Err(ClipboardError::Io(format!(
                "GlobalLock (alloc_dib_handle_and_set format={format}) failed: \
                 GetLastError={err}"
            )));
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, byte_len);
        GlobalUnlock(handle);
        true
    };
    if !write_ok {
        return Err(ClipboardError::Io(format!(
            "GlobalLock (alloc_dib_handle_and_set format={format}) returned NULL"
        )));
    }
    // SAFETY: ownership of `handle` transfers to the OS on the
    // successful `SetClipboardData` return. We ignore the return
    // value (NULL on failure) — see `set_text` for the
    // leak-on-failure rationale (a ~32-byte header leak on the
    // rare failure path is harmless).
    let _ = unsafe { SetClipboardData(format, handle as _) };
    Ok(())
}

// ============================================================================
//  Tests
// ============================================================================
//
// **Cross-platform note**: this module is `#[cfg(target_os = "windows")]`,
// so the tests only run on Windows hosts (the CI matrix runs them
// there). On macOS / Linux the entire file is excluded from the
// build — we cannot exercise the Win32 API locally.
//
// The tests pin the trait contract (`name()` / `can_clear()`) and
// the basic safety invariants of the helpers (`err_to_string`).
// Round-trip testing of `OpenClipboard` / `GetClipboardData` /
// `SetClipboardData` is performed by the human verification matrix
// in `tests/manual/clipboard-text.md` on a real Windows VM.

#[cfg(test)]
mod tests {
    use super::*;

    /// `name()` is a stable contract — the daemon startup log uses it
    /// to advertise which backend was selected. A regression here
    /// would silently break operator-facing diagnostics.
    #[test]
    fn name_is_windows_openclip_label() {
        let backend = WinClipboard::new().expect("new on Windows");
        assert_eq!(backend.name(), "windows-openclip");
    }

    /// `new()` always succeeds on Windows — the Win32 clipboard API
    /// is always available; no probe necessary.
    #[test]
    fn new_succeeds_on_windows() {
        let result = WinClipboard::new();
        assert!(
            result.is_ok(),
            "WinClipboard::new() should succeed on Windows (no probe needed); got {result:?}"
        );
    }

    /// `CF_UNICODETEXT = 13` is a stable Win32 constant. Pin it so a
    /// future windows-sys bump that re-exports the constant does not
    /// accidentally change our value (some versions had it at 12
    /// pre-NT).
    #[test]
    fn cf_unicodetext_constant_is_stable() {
        assert_eq!(CF_UNICODETEXT, 13);
    }

    /// **`CF_DIB = 8` is a stable Win32 constant**, also pinned so a
    /// future windows-sys bump that re-exports the constant does not
    /// accidentally change our value. The legacy `BITMAPINFO`-based
    /// DIB format is what we publish alongside `CF_DIBV5` so the
    /// WeChat desktop client (which reads only `CF_DIB` and
    /// silently drops `CF_DIBV5`-only clipboards) sees the image.
    #[test]
    fn cf_dib_constant_is_stable() {
        assert_eq!(CF_DIB_U32, 8);
    }

    /// `err_to_string` produces a deterministic, parseable format
    /// (`"<op> failed: GetLastError=<code>"`). The dispatcher log
    /// grep relies on the exact prefix to recognise Win32 errors.
    #[test]
    fn err_to_string_format_is_stable() {
        assert_eq!(
            err_to_string("OpenClipboard", 5),
            "OpenClipboard failed: GetLastError=5"
        );
        assert_eq!(
            err_to_string("GlobalLock", 0),
            "GlobalLock failed: GetLastError=0"
        );
    }

    /// Regression pin for **M2b validator P1.2**: the
    /// `set_dib_image` GlobalLock-failure path surfaces an
    /// `Err(ClipboardError::Io)` whose message uses this exact
    /// `"<op> failed: GetLastError=<code>"` shape. If a future
    /// refactor changes the wording, the operator's `grep
    /// "GlobalLock (set_dib_image)"` log queries silently break.
    /// (Mocking Win32 GlobalLock failure is not feasible in unit
    /// tests; this test pins the only stable contract surface
    /// for that failure path — the error string format.)
    #[test]
    fn err_to_string_format_for_set_dib_image_is_stable() {
        assert_eq!(
            err_to_string("GlobalLock (set_dib_image)", 8),
            "GlobalLock (set_dib_image) failed: GetLastError=8"
        );
    }

    // === M2b STEP-2b.1 — image helpers ===

    /// Build a small but valid PNG byte buffer (4×2 RGBA gradient)
    /// for the encode-helper tests. The PNG magic prefix pins the
    /// format so a regression in the test fixture cannot
    /// accidentally feed garbage into `image::load_from_memory`.
    fn test_png_bytes() -> Vec<u8> {
        let img = image::RgbImage::from_fn(4, 2, |x, y| {
            image::Rgb([(x * 60) as u8, (y * 60) as u8, 128])
        });
        let mut out = Vec::new();
        {
            let mut cursor = std::io::Cursor::new(&mut out);
            img.write_to(&mut cursor, image::ImageFormat::Png)
                .expect("encode test png");
        }
        // Sanity: PNG magic prefix.
        assert_eq!(
            &out[..8],
            &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A],
            "test fixture must produce PNG bytes"
        );
        out
    }

    /// `encode_png_to_dib` produces a DIB payload whose leading
    /// 40 bytes are a `BITMAPINFOHEADER`. Pin `biSize = 40` (the
    /// only header size the `image` crate's BMP encoder emits) so
    /// a regression that accidentally swapped encoders (e.g. to
    /// `Bmp` with an unexpected variant) would fail this test
    /// rather than silently corrupt the receiver's clipboard.
    #[test]
    fn encode_png_to_dib_writes_bitmapinfoheader_with_size_40() {
        let png = test_png_bytes();
        let dib = encode_png_to_dib(&png).expect("encode_png_to_dib");
        assert!(
            dib.len() >= 40,
            "DIB payload must be at least 40 bytes (BITMAPINFOHEADER), got {}",
            dib.len()
        );
        // `biSize` is the first u32 of the BITMAPINFOHEADER (little-endian on Windows).
        let bi_size = u32::from_le_bytes([dib[0], dib[1], dib[2], dib[3]]);
        assert_eq!(
            bi_size, 40,
            "leading biSize must be 40 (BITMAPINFOHEADER); got {bi_size}"
        );
        // biPlanes (bytes 12-13 LE) is always 1 for bitmaps.
        let bi_planes = u16::from_le_bytes([dib[12], dib[13]]);
        assert_eq!(bi_planes, 1, "biPlanes must be 1");
    }

    /// `encode_png_to_dib` round-trips back to the original
    /// pixel dimensions via the `image` crate. Pin the dimensions
    /// + a sample pixel so a regression in the BMP encoder (which
    /// might, e.g., swap width / height or stride-align pixel
    /// rows incorrectly) surfaces immediately.
    #[test]
    fn encode_png_to_dib_round_trips_dimensions_via_image_crate() {
        let png = test_png_bytes();
        let dib = encode_png_to_dib(&png).expect("encode_png_to_dib");
        let decoded = image::load_from_memory_with_format(&dib, image::ImageFormat::Bmp)
            .expect("image crate must be able to decode the emitted DIB");
        assert_eq!(decoded.width(), 4, "width preserved");
        assert_eq!(decoded.height(), 2, "height preserved");
    }

    /// `encode_png_to_dib` returns `Err(Io)` for malformed PNG
    /// bytes — the `image::load_from_memory` error funnel.
    #[test]
    fn encode_png_to_dib_rejects_garbage_input() {
        let garbage = b"not a png";
        let result = encode_png_to_dib(garbage);
        assert!(
            matches!(result, Err(ClipboardError::Io(_))),
            "encode_png_to_dib(garbage) must return Err(Io); got {result:?}"
        );
    }

    /// `write_dibv5_from_png_helper` is the pure-bytes form of
    /// the PNG→DIB conversion. Pin that it returns
    /// `Ok(Vec<u8>)` for a valid PNG and that the result is
    /// non-empty (the test for the exact DIB layout lives in
    /// `encode_png_to_dib_writes_bitmapinfoheader_with_size_40`
    /// above).
    #[test]
    fn write_dibv5_from_png_helper_succeeds_on_valid_png() {
        let png = test_png_bytes();
        let dib = write_dibv5_from_png_helper(&png).expect("helper must succeed");
        assert!(!dib.is_empty(), "DIB payload must not be empty");
    }

    /// `set_image(Mime::Jpeg)` is intentionally rejected because
    /// the inbound dispatcher normalises all non-PNG mimes to
    /// PNG before reaching `set_image` (PLAN §3 M2a STEP-2a.2).
    /// If a future Windows feature ever wants JPEG support, this
    /// pin forces a deliberate code change rather than a silent
    /// behaviour drift.
    #[test]
    fn set_image_jpeg_returns_unsupported() {
        let mut backend = WinClipboard::new().expect("new on Windows");
        let jpeg_bytes = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        let result = backend.set_image(&jpeg_bytes, Mime::Jpeg);
        assert!(
            matches!(result, Err(ClipboardError::Unsupported(_))),
            "set_image(Jpeg) must return Err(Unsupported); got {result:?}"
        );
    }
}
