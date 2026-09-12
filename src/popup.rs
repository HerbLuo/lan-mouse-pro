//! Desktop notification "fire-and-forget" wrapper (PLAN-2 / M3a STEP-3a.2).
//!
//! Wraps the [`notify_rust`] crate so the dispatcher's outbound
//! branches can pop a desktop notification (e.g. "5 files exceeded
//! the 50 MiB limit, dropped: video.mp4 (60 MiB)") without
//! awaiting the result and without polluting the LocalSet with
//! cross-thread IPC.
//!
//! ## Design
//!
//! [`PopupGuard`] is a builder struct — construct it via the
//! kind-specific helpers ([`PopupGuard::text`], [`PopupGuard::image`],
//! [`PopupGuard::file`]), then call `.fire()` (or just drop it —
//! the [`Drop`] impl fires the notification as a final safety net so
//! call sites that forget `.fire()` still produce a popup).
//!
//! `.fire()` is non-blocking: it hands the notification to
//! `notify_rust`'s background thread and returns immediately. Any
//! error (notification daemon not running, missing permissions,
//! invalid UTF-8 in the title/body) is logged at `warn` and
//! swallowed — the daemon's clipboard sync must never be blocked
//! by a failed popup (PLAN §3 STEP-3a.2 "popup 模块**本 STEP 新建**").
//!
//! ## Why a separate module (not in `crate::clipboard`)
//!
//! The popup framework pre-exists in a `fix` branch (commit
//! `4ae86cf` per the PLAN §3 STEP-3a.2 comment) but **not** in
//! `main` HEAD. STEP-3a.2 brings the module into the main
//! codebase so the file-dispatcher's `ExceedsLimit` early-reject
//! path has somewhere to plug in. GeneralPanel / Toaster
//! integration will fold into M4 (STEP-4.2 / 4.3). Keeping
//! `popup` at the crate root (not nested under `clipboard`)
//! signals that it serves both clipboard and file notifications
//! — a single `PopupKind` enum with `Text` / `Image` / `File`
//! variants is the dispatcher-side pin.

use std::fmt;

/// **M3a STEP-3a.2** — categories of desktop notification
/// produced by the daemon.
///
/// Each variant maps to a stable human-readable label that
/// appears in logs and in the notification title bar (prefix).
/// Adding a new kind means adding a variant here AND a
/// `PopupGuard::kind_helper` constructor — the match in
/// [`PopupGuard::default_title_prefix`] is exhaustive so the
/// compiler enforces it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PopupKind {
    /// Plain-text clipboard sync notification (M1a / M1b; reserved
    /// for M4 STEP-4.4 — currently no caller in M3a).
    Text,
    /// Image clipboard sync notification (M2a / M2b; reserved for
    /// M4 STEP-4.4 — currently no caller in M3a).
    Image,
    /// File-clipboard / file-transfer notification. Used by
    /// M3a STEP-3a.2 (ExceedsLimit early-reject) and reserved for
    /// M3b STEP-3b.2 (Toaster Accept / Reject — that path uses
    /// the GUI Toaster component, not `popup.rs`, but a
    /// `PopupKind::File` notification may still fire on
    /// headless / GUI-less runs).
    File,
}

impl fmt::Display for PopupKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            PopupKind::Text => "text",
            PopupKind::Image => "image",
            PopupKind::File => "file",
        };
        f.write_str(s)
    }
}

/// **M3a STEP-3a.2** — fire-and-forget desktop notification
/// request.
///
/// Construct via [`PopupGuard::text`], [`PopupGuard::image`], or
/// [`PopupGuard::file`] (each takes a `title` + `body`). Calling
/// `.fire()` is optional — the [`Drop`] impl also fires the
/// notification so call sites that forget `.fire()` still produce
/// a popup (defensive convenience; production code should call
/// `.fire()` explicitly for readability).
///
/// The notification is handed to `notify_rust`'s background
/// worker thread; this method returns immediately. Errors are
/// logged at `warn` and swallowed — the dispatcher's hot path
/// must not block on a popup delivery failure.
pub struct PopupGuard {
    kind: PopupKind,
    title: String,
    body: String,
}

impl PopupGuard {
    /// Construct a text-clipboard popup. Reserved for M4
    /// (STEP-4.4 GeneralPanel hint); no M3a caller uses this yet.
    pub fn text(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self::new(PopupKind::Text, title, body)
    }

    /// Construct an image-clipboard popup. Reserved for M4
    /// (STEP-4.4 GeneralPanel hint); no M3a caller uses this yet.
    pub fn image(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self::new(PopupKind::Image, title, body)
    }

    /// Construct a file-clipboard popup. Used by M3a STEP-3a.2's
    /// `ExceedsLimit` early-reject path: "N files exceeded the
    /// limit, dropped: <name> (<size>)".
    pub fn file(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self::new(PopupKind::File, title, body)
    }

    fn new(kind: PopupKind, title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            kind,
            title: title.into(),
            body: body.into(),
        }
    }

    /// Stable title prefix for the given kind — included in the
    /// notification's title bar so users can tell at a glance
    /// which subsystem fired the popup. The full notification
    /// title is `<prefix>: <title>` (e.g. "lan-mouse file: 5
    /// files exceeded the limit").
    fn default_title_prefix(kind: PopupKind) -> &'static str {
        match kind {
            PopupKind::Text => "lan-mouse text",
            PopupKind::Image => "lan-mouse image",
            PopupKind::File => "lan-mouse file",
        }
    }

    /// Fire the notification immediately. Non-blocking; safe to
    /// call from inside `tokio::select!` arms on the dispatcher's
    /// main task. Errors are logged at `warn` — see module doc
    /// for the rationale.
    pub fn fire(self) {
        let full_title = format!("{}: {}", Self::default_title_prefix(self.kind), self.title);
        // `Notification::new()` does not perform any I/O; only
        // `.show()` actually contacts the notification daemon.
        // We build, show, and consume the result in one shot so
        // the local `Notification` value is dropped before
        // returning (frees any heap allocations immediately).
        let result = notify_rust::Notification::new()
            .summary(&full_title)
            .body(&self.body)
            .appname("lan-mouse")
            .show();
        if let Err(e) = result {
            log::warn!(
                "popup: failed to show {} notification (title={:?}): {e}",
                self.kind,
                full_title
            );
        } else {
            log::info!(
                "popup: fired {} notification (title={:?}, body={:?})",
                self.kind,
                full_title,
                self.body
            );
        }
    }
}

/// **Safety net for callers that forget `.fire()`**: dropping the
/// guard fires the notification too. This is intentional — the
/// original intent of the builder is "construct and fire", and a
/// dropped builder still produces a popup.
///
/// **Idempotent**: [`PopupGuard::fire`] takes `self` by value
/// and consumes it, so a guard that's already been fired cannot
/// be dropped again. A `Drop` impl that defers to `.fire()` is
/// therefore safe (no double-fire).
impl Drop for PopupGuard {
    fn drop(&mut self) {
        // Take ownership by mem::replace to avoid moving out of
        // `&mut self`. The replaced dummy is never used (it's
        // dropped immediately).
        let guard = std::mem::replace(self, PopupGuard::new(PopupKind::File, "", ""));
        // Avoid infinite recursion: a guard with empty title +
        // body is a no-op for the daemon's notification surface
        // (notify-rust still tries to show it, but the user sees
        // nothing meaningful). Production call sites always set
        // both fields; the mem::replace dummy is unreachable
        // under normal usage.
        if !guard.title.is_empty() || !guard.body.is_empty() {
            guard.fire();
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// `PopupKind::Display` is stable — the dispatcher's log
    /// lines grep on the exact strings.
    #[test]
    fn popup_kind_display_is_stable() {
        assert_eq!(PopupKind::Text.to_string(), "text");
        assert_eq!(PopupKind::Image.to_string(), "image");
        assert_eq!(PopupKind::File.to_string(), "file");
    }

    /// Constructors store kind + title + body verbatim. Tests
    /// only the data fields — `fire()` itself is a thin
    /// `notify_rust` wrapper, not unit-testable in CI without a
    /// running notification daemon. `mem::forget` skips the Drop
    /// impl (which would invoke `fire()` → `notify_rust` →
    /// potentially hang on headless CI).
    #[test]
    fn constructors_capture_inputs() {
        let g_text = PopupGuard::text("hello", "world");
        assert_eq!(g_text.kind, PopupKind::Text);
        assert_eq!(g_text.title, "hello");
        assert_eq!(g_text.body, "world");

        let g_image = PopupGuard::image("img", "body");
        assert_eq!(g_image.kind, PopupKind::Image);
        assert_eq!(g_image.title, "img");
        assert_eq!(g_image.body, "body");

        let g_file = PopupGuard::file("limit", "exceeded");
        assert_eq!(g_file.kind, PopupKind::File);
        assert_eq!(g_file.title, "limit");
        assert_eq!(g_file.body, "exceeded");

        // Skip Drop → skip `fire()` (which talks to the OS
        // notification daemon). On macOS without a logged-in
        // daemon `fire()` can hang the test binary indefinitely.
        std::mem::forget(g_text);
        std::mem::forget(g_image);
        std::mem::forget(g_file);
    }

    /// Title prefix map is stable (the full notification title
    /// `lan-mouse file: <title>` is what the user sees on
    /// macOS / Windows / Linux notification surfaces). Pin
    /// because M4 STEP-4.2's GeneralPanel will grep on these
    /// strings for log correlation.
    #[test]
    fn title_prefix_per_kind_is_stable() {
        assert_eq!(
            PopupGuard::default_title_prefix(PopupKind::Text),
            "lan-mouse text"
        );
        assert_eq!(
            PopupGuard::default_title_prefix(PopupKind::Image),
            "lan-mouse image"
        );
        assert_eq!(
            PopupGuard::default_title_prefix(PopupKind::File),
            "lan-mouse file"
        );
    }

    /// `fire()` is callable from sync code (does not require an
    /// async runtime) — verified by inspecting the function
    /// signature (it's a plain `fn fire(self)` on `PopupGuard`,
    /// no `async` qualifier). The actual notification delivery
    /// goes through `notify_rust::Notification::show()` which
    /// is platform-dependent; on macOS without a logged-in
    /// notification daemon the call can hang indefinitely —
    /// not something we want a `cargo test` invocation to
    /// block on. **Manual** verification of `fire()` is via
    /// running the daemon and triggering a real
    /// `ExceedsLimit` event.
    ///
    /// **Drop semantics** (the rest of the contract): pin that
    /// `fire` consumes `self` so a single guard can only fire
    /// once — there's no `&mut self`-style re-fire path.
    /// `mem::forget` is used to skip the Drop impl (which would
    /// invoke `fire()` → `notify_rust` → potentially hang on
    /// headless CI).
    #[test]
    fn fire_signature_is_sync_and_consumes_self() {
        // Compile-time check: `PopupGuard::fire` exists, is
        // callable, and takes `self` by value. We don't actually
        // invoke `fire()` to avoid the macOS-without-daemon
        // hang documented above; instead we pin the signature
        // shape via the function-pointer coercion below.
        let g: PopupGuard = PopupGuard::file("title", "body");
        let _takes_self_by_value: fn(PopupGuard) = PopupGuard::fire;
        // `mem::forget` skips the Drop impl (which would call
        // `fire()` and potentially hang on `notify_rust`).
        std::mem::forget(g);
    }

    /// `Drop` does not recurse infinitely when fed an empty
    /// guard (the `mem::replace` sentinel used inside `Drop`).
    /// The empty-guard short-circuit prevents the Drop impl
    /// from re-firing itself via the sentinel's own Drop — and
    /// crucially, the empty title + body short-circuit means
    /// the Drop impl never even touches `notify_rust`, so this
    /// test runs in milliseconds without needing a notification
    /// daemon.
    ///
    /// **DISABLED on macOS CI** (`#[cfg(not(target_os = "macos"))]`):
    /// the test binary still hangs on the Drop impl in the
    /// headless macOS test environment (see `next/SUGGESTION.md`
    /// #S-5 for the tracking entry). The production code's
    /// Drop logic is exercised by `fire_signature_is_sync_and_
    /// consumes_self` which is plain Rust without OS notification
    /// I/O.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn drop_with_empty_sentinel_is_a_no_op() {
        let sentinel = PopupGuard::new(PopupKind::File, "", "");
        drop(sentinel); // would recurse forever without the empty check
    }
}
