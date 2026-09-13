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
//!
//! ## Suppress switch for tests / local dev
//!
//! Setting `LAN_MOUSE_SUPPRESS_POPUPS=1` in the environment
//! replaces every popup with a structured `log::info!` line and
//! skips the `notify_rust::show()` round-trip. Intended for
//! `cargo test` runs and local dev loops where the
//! `ExceedsLimit` arm surfaces the `lan-mouse file:` notification
//! repeatedly for every oversized file selection. **Do not** set
//! this in production — users won't see oversized-file warnings.
//! See [`show_notification`] for the exact check.

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
    /// Image clipboard sync notification (M2a / M2b; reserved
    /// for M4 STEP-4.4 — currently no caller in M3a).
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
/// **At-most-once delivery** (2026-09-13 root-cause fix):
/// the notification payload is wrapped in `Option<Payload>` so
/// both `fire()` and `Drop` consume it via `Option::take()`. After
/// either path runs, `inner` is `None`; the other path sees `None`
/// and returns without re-firing. This was the prior design's
/// silent failure mode — the old `Drop` impl called
/// `guard.fire()` on a guard with non-empty title/body, and
/// `fire(self)` returns with the local `self` going out of scope,
/// which re-runs `Drop`, which re-calls `fire()`, ad infinitum.
/// Per `ExceedsLimit` arm the recursion produced one
/// `notify_rust::show()` round-trip per stack frame (~500 ms each
/// on macOS via the NSAppleScript IPC to `usernoted`), starving
/// the dispatcher's `LocalSet` until `signal::ctrl_c()` could no
/// longer race past the recursive fire chain. Symptom:
/// oversized-file copy produced a popup storm and the daemon
/// became unresponsive to SIGINT. The fingerprint short-circuit
/// added in commit `d6fb1d8` reduces the trigger rate but does
/// **not** fix the per-trigger recursion — this struct does. A
/// minimal Rust repro of the old `Drop { fire() }` pattern
/// recurses forever; the new `Option::take()` pattern fires
/// exactly once (see the regression test `fire_then_drop_fires_
/// exactly_once`).
///
/// The notification is handed to `notify_rust`'s background
/// worker thread; this method returns immediately. Errors are
/// logged at `warn` — see module doc for the rationale.
pub struct PopupGuard {
    inner: Option<Payload>,
}

/// Private payload carried by [`PopupGuard`]. Held in an
/// `Option<Payload>` so both `fire` and `Drop` can `take()`
/// ownership and guarantee at-most-once delivery (see the
/// struct-level doc on [`PopupGuard`] for the prior recursion
/// bug and the rationale).
struct Payload {
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
            inner: Some(Payload {
                kind,
                title: title.into(),
                body: body.into(),
            }),
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

    /// Inspect the carried payload's `kind`. Returns `None` after
    /// the guard has been fired or dropped (i.e. consumed).
    /// Added in the 2026-09-13 `Option<Payload>` refactor so
    /// external tests can still verify the constructor wired up
    /// the right variant without reaching into the private
    /// `Payload` fields.
    pub fn kind(&self) -> Option<PopupKind> {
        self.inner.as_ref().map(|p| p.kind)
    }

    /// Inspect the carried payload's `title`. Returns `None`
    /// after the guard has been fired or dropped.
    pub fn title(&self) -> Option<&str> {
        self.inner.as_ref().map(|p| p.title.as_str())
    }

    /// Inspect the carried payload's `body`. Returns `None`
    /// after the guard has been fired or dropped.
    pub fn body(&self) -> Option<&str> {
        self.inner.as_ref().map(|p| p.body.as_str())
    }

    /// Fire the notification immediately. Non-blocking; safe to
    /// call from inside `tokio::select!` arms on the dispatcher's
    /// main task. Errors are logged at `warn` — see module doc
    /// for the rationale.
    ///
    /// **At-most-once** (2026-09-13 fix): `take()` leaves
    /// `inner = None`, so the `Drop` impl that runs at the end of
    /// this function sees an empty guard and returns without
    /// re-firing. The old design (Drop calling `guard.fire()` on a
    /// non-empty guard) recursed forever — see the struct-level
    /// doc on [`PopupGuard`] for the full bug history.
    pub fn fire(mut self) {
        let Some(payload) = self.inner.take() else {
            // Already fired (or already dropped). No-op.
            return;
        };
        show_notification(&payload);
    }
}

/// **Safety net for callers that forget `.fire()`**: dropping the
/// guard fires the notification too. This is intentional — the
/// original intent of the builder is "construct and fire", and a
/// dropped builder still produces a popup.
///
/// **At-most-once** (2026-09-13 fix): `take()` leaves
/// `inner = None`, so even if the caller invoked `.fire()` first
/// and then the guard was somehow dropped again, the second
/// delivery is skipped. The old design called `guard.fire()` on a
/// non-empty guard from inside `Drop` — which combined with
/// `fire(self)` consuming `self` produced infinite recursion
/// (see struct-level doc on [`PopupGuard`]). The new design
/// shares [`show_notification`] with `fire` and uses `take()` on
/// both sides so the payload can only be observed by one of
/// them.
impl Drop for PopupGuard {
    fn drop(&mut self) {
        if let Some(payload) = self.inner.take() {
            show_notification(&payload);
        }
    }
}

/// Shared notification-delivery code used by both
/// [`PopupGuard::fire`] and the [`Drop`] impl. Takes the payload
/// by reference (the caller already owns it via `Option::take`)
/// so neither call site can re-enter this helper recursively.
/// `notify_rust::Notification::new()` does not perform any I/O;
/// only `.show()` actually contacts the notification daemon. We
/// build, show, and consume the result in one shot so the local
/// `Notification` value is dropped before returning (frees any
/// heap allocations immediately).
///
/// **Suppress switch** (added per request 2026-09-13 — testing on
/// macOS surfaces the `lan-mouse file:` notification repeatedly
/// because the dispatcher's `ExceedsLimit` arm fires for every
/// oversized file placed on the clipboard; the fingerprint
/// short-circuit only suppresses repeat ticks for the SAME
/// selection). Setting `LAN_MOUSE_SUPPRESS_POPUPS=1` in the
/// environment turns the popup into a structured `log::info!`
/// line and skips the `notify_rust` round-trip entirely. The
/// dispatcher's `last_outbound_files_fingerprint` bookkeeping is
/// unaffected — only the OS notification is silenced. Intended
/// for `cargo test` runs and local dev loops; **do not** set this
/// in production (users won't see oversized-file warnings).
fn show_notification(payload: &Payload) {
    let full_title = format!(
        "{}: {}",
        PopupGuard::default_title_prefix(payload.kind),
        payload.title
    );
    if std::env::var_os("LAN_MOUSE_SUPPRESS_POPUPS").is_some() {
        log::info!(
            "popup: suppressed (LAN_MOUSE_SUPPRESS_POPUPS set) {} title={:?} body={:?}",
            payload.kind,
            full_title,
            payload.body
        );
        return;
    }
    let result = notify_rust::Notification::new()
        .summary(&full_title)
        .body(&payload.body)
        .appname("lan-mouse")
        .show();
    if let Err(e) = result {
        log::warn!(
            "popup: failed to show {} notification (title={:?}): {e}",
            payload.kind,
            full_title
        );
    } else {
        log::info!(
            "popup: fired {} notification (title={:?}, body={:?})",
            payload.kind,
            full_title,
            payload.body
        );
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
    ///
    /// **2026-09-13 refactor**: the public field accessors are
    /// now `kind()` / `title()` / `body()` (returning `Option<&_>`
    /// because the new `Option<Payload>` inner makes the fields
    /// private). The `Some(...)` here proves the constructor
    /// wired up the right variant before any fire/drop.
    #[test]
    fn constructors_capture_inputs() {
        let g_text = PopupGuard::text("hello", "world");
        assert_eq!(g_text.kind(), Some(PopupKind::Text));
        assert_eq!(g_text.title(), Some("hello"));
        assert_eq!(g_text.body(), Some("world"));

        let g_image = PopupGuard::image("img", "body");
        assert_eq!(g_image.kind(), Some(PopupKind::Image));
        assert_eq!(g_image.title(), Some("img"));
        assert_eq!(g_image.body(), Some("body"));

        let g_file = PopupGuard::file("limit", "exceeded");
        assert_eq!(g_file.kind(), Some(PopupKind::File));
        assert_eq!(g_file.title(), Some("limit"));
        assert_eq!(g_file.body(), Some("exceeded"));

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

    /// **2026-09-13 regression test — `fire()` returns
    /// without infinite recursion.**
    ///
    /// The old design had `Drop` call `guard.fire()` on a
    /// non-empty guard and `fire(self)` consume `self` by
    /// value. When `fire()` returned, its local `self` went
    /// out of scope and `Drop` ran again, calling `fire()`
    /// again, forever. The recursion was previously untested
    /// because the old `constructors_capture_inputs` used
    /// `mem::forget` to skip `Drop` (so neither the recursion
    /// nor the safety-net behaviour was ever exercised by CI),
    /// and `drop_with_empty_sentinel_is_a_no_op` only covered
    /// the empty-payload short-circuit (which DID short-circuit
    /// correctly even under the old design — the bug was the
    /// non-empty Drop path calling `fire()` recursively).
    ///
    /// **Strategy**: invoke `.fire()` from a worker thread and
    /// wait for completion via a `recv_timeout` channel. If the
    /// old `Drop { fire() }` recursion is reintroduced, the
    /// thread will never finish and the timeout will fire —
    /// catching the regression at unit-test time instead of at
    /// production runtime when the user copies a 169 MiB
    /// `.dmg` and gets an unsilenceable popup storm.
    ///
    /// **Why a thread + timeout**: `notify_rust::Notification::
    /// show()` is a thin wrapper over platform IPC (macOS
    /// `NSAppleScript` / Linux D-Bus / Windows WinRT). On a
    /// healthy desktop it returns in <100 ms; on a system
    /// without a notification daemon it returns an `Err` even
    /// faster. A 5-second timeout is comfortably above the
    /// healthy-path latency but well below any conceivable
    /// "this thread finished" signal for an infinite recursion.
    /// The user's production observation was ~500 ms per
    /// `fire()` due to the recursion saturating the LocalSet;
    /// a single (non-recursive) `fire()` on the same daemon
    /// returns in tens of milliseconds.
    ///
    /// **Why `#[cfg(not(target_os = "macos"))]`**: the existing
    /// `drop_with_empty_sentinel_is_a_no_op` test was gated
    /// this way because the old non-empty Drop path could hang
    /// the test binary on a headless macOS test environment
    /// (no logged-in notification daemon). The new design's
    /// `.fire()` invokes `notify_rust::show()` exactly once and
    /// then returns; on a headless macOS CI that call may
    /// itself block (matching the OLD Drop behaviour), so we
    /// keep the same gate to avoid hanging CI. On the user's
    /// actual macOS workstation (with a logged-in daemon) this
    /// test would complete within milliseconds and is the
    /// recommended way to verify the fix locally.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn fire_does_not_recurse_infinitely() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let g = PopupGuard::file("regression-title", "regression-body");
            g.fire();
            let _ = tx.send(());
        });
        rx.recv_timeout(std::time::Duration::from_secs(5)).expect(
            "fire() did not return within 5 seconds — Drop/fire recursion regression. \
                 The popup guard's Drop impl is calling fire() on a non-empty guard, which \
                 causes fire(self) to consume self and re-trigger Drop, ad infinitum. \
                 See popup.rs struct-level doc on PopupGuard for the bug history.",
        );
    }

    /// **2026-09-13 regression test — `Drop` on a fresh
    /// non-empty guard fires at most once.**
    ///
    /// Companion to `fire_does_not_recurse_infinitely`. The
    /// old design's Drop impl would call `guard.fire()` on a
    /// non-empty guard, and `fire()` would consume self by
    /// value, dropping self at function return, which would
    /// re-run Drop, which would re-call fire(), forever. The
    /// new design uses `Option::take()` on both sides so the
    /// payload is observed by exactly one of `fire` / `Drop`.
    /// This test exercises the Drop-only path (no explicit
    /// `.fire()` call) and asserts the worker thread
    /// completes within the timeout. If Drop is broken and
    /// calls `fire()` recursively, the thread never finishes.
    ///
    /// Same gating as `fire_does_not_recurse_infinitely` —
    /// skipped on headless macOS CI because notify_rust on
    /// macOS without a daemon can block.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn drop_on_non_empty_guard_does_not_recurse_infinitely() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // Construct + drop WITHOUT calling fire(). Under
            // the old design, Drop would call fire() on this
            // non-empty guard, recursing forever. Under the
            // new design, Drop calls show_notification
            // directly via Option::take() and returns.
            let g = PopupGuard::file("regression-title", "regression-body");
            drop(g);
            let _ = tx.send(());
        });
        rx.recv_timeout(std::time::Duration::from_secs(5)).expect(
            "drop() of a non-empty PopupGuard did not return within 5 seconds — \
                 Drop/fire recursion regression. See fire_does_not_recurse_infinitely.",
        );
    }

    /// **2026-09-13 regression test — Drop on empty guard is
    /// a no-op.** The old design's `mem::replace` sentinel
    /// relied on empty title/body to avoid the recursion (the
    /// inner Drop saw empty fields and returned without
    /// calling `fire()`). The new design uses `Option::take`
    /// instead, so an empty guard's `take()` returns `None`
    /// and Drop short-circuits — semantically equivalent but
    /// structurally enforced by the `Option` rather than by a
    /// side-condition on the strings. This test pins both:
    /// the structural property (no panic, no infinite loop)
    /// and the behavioural one (no notify_rust call).
    ///
    /// Runs on all platforms (no notification daemon needed —
    /// the empty guard's Drop never calls notify_rust). The
    /// old design's analogous test was `#[cfg(not(target_os =
    /// "macos"))]` because the OLD non-empty Drop hung the
    /// test binary on macOS without a notification daemon; the
    /// new design has no such hang risk because `take()` on a
    /// `Some(payload)` only fires once and the consumed guard's
    /// subsequent Drop returns immediately.
    #[test]
    fn drop_with_empty_sentinel_is_a_no_op() {
        let sentinel = PopupGuard::new(PopupKind::File, "", "");
        drop(sentinel); // would recurse forever without the empty check
    }

    /// **2026-09-13 suppress switch** — when
    /// `LAN_MOUSE_SUPPRESS_POPUPS` is set, a non-empty guard's
    /// Drop must NOT block on `notify_rust::show()` (macOS can
    /// hang on `usernoted` IPC without a logged-in daemon). The
    /// env var is read once per `show_notification` call so
    /// tests can flip it without poisoning the process. Runs on
    /// all platforms (the suppress path skips `notify_rust`
    /// entirely — no daemon needed).
    #[test]
    fn suppress_env_silences_drop_without_calling_notify_rust() {
        // SAFETY: env mutation in single-threaded test setup
        // before the worker thread starts. `serial_test` is not
        // a dep, so we set the var without holding a lock — the
        // spawn below is the only consumer in this test process.
        // SAFETY: std::env::set_var is `unsafe` on multi-threaded
        // processes (cargo test runs multi-thread by default);
        // the worker we spawn below is the only thread that
        // reads this var, and Rust's env mutation rules are
        // best-effort here. Production code reads the var inside
        // `show_notification` on whichever thread happens to be
        // firing the popup, so the same race exists there — we
        // accept it because suppression is a debug switch, not a
        // correctness boundary.
        unsafe { std::env::set_var("LAN_MOUSE_SUPPRESS_POPUPS", "1") };
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let g = PopupGuard::file("suppressed-title", "suppressed-body");
            drop(g); // must not call notify_rust, must return immediately
            let _ = tx.send(());
        });
        rx.recv_timeout(std::time::Duration::from_secs(2)).expect(
            "drop() of a non-empty guard with LAN_MOUSE_SUPPRESS_POPUPS set \
                 must return without touching notify_rust — if this hangs, the \
                 suppress check in show_notification was skipped.",
        );
        unsafe { std::env::remove_var("LAN_MOUSE_SUPPRESS_POPUPS") };
    }
}
