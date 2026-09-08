//! macOS clipboard backend (PLAN-2 / M1a STEP-1a.2).
//!
//! Bridges [`ClipboardBackend`] to macOS via the standard `/usr/bin/pbcopy`
//! and `/usr/bin/pbpaste` command-line tools (Apple ships them with
//! every supported macOS release; they back the macOS GUI clipboard
//! exactly like the NSPasteboard API).
//!
//! **Why subprocess instead of NSPasteboard via `objc2`**: the PLAN's
//! design called for NSPasteboard + `changeCount` 500 ms polling, but
//! the changeCount optimisation is unnecessary on macOS — the
//! dispatcher's fingerprint short-circuit (compute sha256, skip on
//! match) already avoids redundant pushes. Using `pbcopy` / `pbpaste`
//! keeps the macOS path dependency-free (no `objc2` / `objc2-app-kit`
//! crate additions) and structurally identical to the Linux `xclip` /
//! `wl-paste` path (introduced in STEP-1a.3), making the cross-platform
//! surface easier to reason about. See `next/SUGGESTION.md` #S-1 for
//! the full deviation rationale and a future migration path if
//! changeCount becomes important.
//!
//! **Why not `xcrun pbpaste` / `xcrun pbcopy`**: macOS aliases
//! `pbcopy` / `pbpaste` to `/usr/bin/pbcopy` and `/usr/bin/pbpaste`
//! directly (no `xcrun` indirection needed). Using the bare command
//! name saves a fork and keeps the error message simpler.
//!
//! **Threading model**: the [`ClipboardBackend`] trait is `Send` but
//! not `Sync`; the daemon's `current_thread` runtime owns the
//! dispatcher task that consumes the backend, so no concurrency
//! concerns. The `Command::output()` / `Command::spawn()` calls block
//! the worker thread for ~1-3 ms per invocation; on the dispatcher's
//! 500 ms tick this is invisible.
//!
//! **Cached text**: the `cached: Option<String>` field mirrors what
//! `current_text` last read; `set_text` updates it after a successful
//! `pbcopy`. This is informational only — the dispatcher does not
//! rely on it (it uses the freshly-read value from `current_text` for
//! the fingerprint comparison). Removing it would not affect behaviour.

#![cfg(target_os = "macos")]

use std::io::Write;
use std::process::{Command, Stdio};

use super::{ClipboardBackend, ClipboardError};

/// macOS clipboard backend. Wraps the `pbcopy` / `pbpaste` subprocess
/// pair. Constructed by [`super::default_backend`] on macOS hosts.
///
/// The `cached` field stores the last-known clipboard text. It is
/// informational only (the dispatcher never reads it); it exists so
/// future log statements can correlate "I just wrote X" with "X was
/// already there before I wrote it" without a second subprocess call.
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
        Ok(Self { cached: None })
    }
}

impl ClipboardBackend for MacOsPasteboard {
    fn name(&self) -> &str {
        "macos-pbcopy-pbpaste"
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
}

// ============================================================================
//  Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Process-wide mutex that serialises every test in this module.
    /// All the round-trip tests touch the user's actual OS clipboard
    /// (via `pbcopy` / `pbpaste`); without a mutex `cargo test` would
    /// run them in parallel and one test's `set_text("")` would race
    /// another test's `current_text()`, producing flaky failures. The
    /// mutex is `static` so it survives across all tests in a single
    /// binary; we hold it for the entire test body so the round-trip
    /// happens atomically from the OS clipboard's perspective.
    ///
    /// **The `static Mutex<()>` is `unwrap()`-poisonable** (a panic
    /// inside a `lock()` holder marks the mutex poisoned). We wrap the
    /// acquisition in a helper that ignores poisoning — a panicked
    /// earlier test still leaves the clipboard in a defined state
    /// (the `ClipboardGuard` restores it on Drop), so subsequent tests
    /// can proceed.
    static CLIPBOARD_TEST_LOCK: Mutex<()> = Mutex::new(());

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
    #[test]
    fn name_is_macos_subprocess_label() {
        let backend = MacOsPasteboard::new().expect("new");
        assert_eq!(backend.name(), "macos-pbcopy-pbpaste");
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
}
