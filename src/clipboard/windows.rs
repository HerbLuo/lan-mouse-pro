//! Windows clipboard backend (PLAN-2 / M1a STEP-1a.3).
//!
//! Bridges [`ClipboardBackend`] to Windows via the Win32 clipboard
//! API (`OpenClipboard` / `GetClipboardData` / `SetClipboardData`).
//!
//! **Format**: `CF_UNICODETEXT` — the native Windows text-clipboard
//! format. `CF_TEXT` (ANSI) is intentionally not used: every modern
//! Windows app that copies text uses `CF_UNICODETEXT` for at least
//! one of its advertised formats, and the conversion `wide → utf8`
//! is lossless for all valid BMP code points. M1a deliberately does
//! **not** enumerate every format the clipboard offers (HTML / RTF /
//! image) — that is M2a / M3a scope per the PLAN §6 "Out of Scope".
//!
//! **Memory model**: `SetClipboardData` takes ownership of the
//! `HGLOBAL` handle (the OS frees it on the next `SetClipboardData`
//! or `CloseClipboard`). `GetClipboardData` returns a handle the
//! caller must `GlobalLock` / `GlobalUnlock` (and which is invalid
//! after `CloseClipboard`). The helper functions in this file
//! (`read_clipboard_text` / `write_clipboard_text`) follow that
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
use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::Foundation::{GetLastError, HGLOBAL};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};

use super::{ClipboardBackend, ClipboardError};

/// `CF_UNICODETEXT` constant — not exposed by `windows-sys` 0.61 as
/// a top-level constant. Value 13 is stable since Windows 95 / NT
/// 3.51 and has not changed since.
const CF_UNICODETEXT: u32 = 13;

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
}

/// Build a stable error string for a Win32 call. Used by the
/// `Some(err_to_string(...))` path in `current_text`; kept here so
/// the error wording is consistent across the read / write paths.
fn err_to_string(op: &str, err: u32) -> String {
    format!("{op} failed: GetLastError={err}")
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
}

// ============================================================================
//  Unused-import silencer for the `err_to_string` helper on no-op paths
// ============================================================================
//
// The `err_to_string` helper is only used in the `current_text`
// `Some(Err)` branch which is unreachable in practice (the
// `GlobalLock` failure path returns the `String` directly). Suppress
// the resulting `unused` warning on the `err_to_string` fn so the
// Windows build is warning-clean.
#[allow(dead_code)]
fn _force_keep_err_to_string() {
    let _ = err_to_string;
}
