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
}

/// Backend trait — text-only for M1a.
///
/// `current_text` returns `Some(text)` if the clipboard holds UTF-8
/// text (any size, including an empty string — the empty clipboard is
/// a legitimate user state). `None` if the clipboard holds non-text
/// content (image, file list, …) or the platform read failed
/// silently. `set_text` replaces the clipboard contents; errors are
/// propagated via [`ClipboardError`].
///
/// **M1a scope**: the trait deliberately only covers text. Image and
/// file methods (`current_image` / `set_image` / `current_files` /
/// `set_files`) are added in M2a / M3a. Keeping the M1a surface small
/// matches the milestone's "small text only" gate.
///
/// **`Send` (not `Sync`)**: the trait is consumed from a single
/// `spawn_local` task on the daemon's `current_thread` runtime; the
/// dispatcher never shares the backend across tasks concurrently. `Send`
/// is required because the backend lives inside a `Box<dyn
/// ClipboardBackend>` owned by the task future.
///
/// **Why no `watch` method**: the trait is poll-based. The dispatcher
/// owns the 500 ms tick loop and calls `current_text` on every tick,
/// short-circuiting if the previous fingerprint matches the new one.
/// NSPasteboard's `changeCount` would let us skip the read entirely on
/// macOS, but adding platform-specific watcher methods would force the
/// trait to be `async` (and would leak `NSRunLoop` / `wl_display`
/// internals across the trait boundary). The hash-based approach is
/// good enough — the read is a single subprocess invocation or a tiny
/// AppKit call, both sub-millisecond on a quiescent host.
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
    }
}
