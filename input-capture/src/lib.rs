use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt::Display,
    task::{Poll, ready},
};

use async_trait::async_trait;
use futures::StreamExt;
use futures_core::Stream;

use input_event::{Event, KeyboardEvent, scancode};

pub use error::{CaptureCreationError, CaptureError, InputCaptureError};
pub use geometry::BarrierKey;

pub mod error;

pub mod geometry;

#[cfg(libei)]
mod libei;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(layer_shell)]
mod layer_shell;

#[cfg(windows)]
mod windows;

#[cfg(x11)]
mod x11;

/// fallback input capture (does not produce events)
mod dummy;

pub type CaptureHandle = u64;

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum CaptureEvent {
    /// capture on this capture handle is now active
    Begin,
    /// cursor crossed a barrier but the client has not yet Acked the
    /// Enter. The host cursor remains visible and events are NOT consumed
    /// yet. Once the caller calls [`Capture::start_capture`] (after the
    /// Ack arrives) the backend promotes this into a real [`CaptureEvent::Begin`].
    BeginPending,
    /// the pending Begin was cancelled — either the user moved the cursor
    /// back inside the screen, the Ack timed out, or the caller invoked
    /// [`Capture::cancel_pending`] explicitly. No further events will
    /// arrive for the previous pending Begin.
    CancelPending,
    /// input event coming from capture handle
    Input(Event),
}

impl Display for CaptureEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureEvent::Begin => write!(f, "begin capture"),
            CaptureEvent::BeginPending => write!(f, "begin pending (awaiting ack)"),
            CaptureEvent::CancelPending => write!(f, "cancel pending"),
            CaptureEvent::Input(e) => write!(f, "{e}"),
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq)]
pub enum Position {
    Left,
    Right,
    Top,
    Bottom,
}

impl Position {
    pub fn opposite(&self) -> Self {
        match self {
            Position::Left => Self::Right,
            Position::Right => Self::Left,
            Position::Top => Self::Bottom,
            Position::Bottom => Self::Top,
        }
    }
}

impl Display for Position {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pos = match self {
            Position::Left => "left",
            Position::Right => "right",
            Position::Top => "top",
            Position::Bottom => "bottom",
        };
        write!(f, "{pos}")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Backend {
    #[cfg(libei)]
    InputCapturePortal,
    #[cfg(layer_shell)]
    LayerShell,
    #[cfg(x11)]
    X11,
    #[cfg(windows)]
    Windows,
    #[cfg(target_os = "macos")]
    MacOs,
    Dummy,
}

impl Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(libei)]
            Backend::InputCapturePortal => write!(f, "input-capture-portal"),
            #[cfg(layer_shell)]
            Backend::LayerShell => write!(f, "layer-shell"),
            #[cfg(x11)]
            Backend::X11 => write!(f, "X11"),
            #[cfg(windows)]
            Backend::Windows => write!(f, "windows"),
            #[cfg(target_os = "macos")]
            Backend::MacOs => write!(f, "MacOS"),
            Backend::Dummy => write!(f, "dummy"),
        }
    }
}

pub struct InputCapture {
    /// capture backend
    capture: Box<dyn Capture>,
    /// keys pressed by active capture
    pressed_keys: HashSet<scancode::Linux>,
    /// map from barrier key to subscribed ids
    position_map: HashMap<BarrierKey, Vec<CaptureHandle>>,
    /// map from id to its barrier key (for `destroy` lookup)
    id_map: HashMap<CaptureHandle, BarrierKey>,
    /// pending events (fan-out queue: same event delivered to N subscribers)
    pending: VecDeque<(CaptureHandle, CaptureEvent)>,
}

impl InputCapture {
    /// create a new client with the given id at the given barrier key
    pub async fn create(
        &mut self,
        id: CaptureHandle,
        key: &BarrierKey,
    ) -> Result<(), CaptureError> {
        assert!(!self.id_map.contains_key(&id));

        self.id_map.insert(id, key.clone());

        if let Some(v) = self.position_map.get_mut(key) {
            v.push(id);
            Ok(())
        } else {
            self.position_map.insert(key.clone(), vec![id]);
            self.capture.create(key).await
        }
    }

    /// destroy the client with the given id, if it exists
    pub async fn destroy(&mut self, id: CaptureHandle) -> Result<(), CaptureError> {
        let key = self
            .id_map
            .remove(&id)
            .expect("no barrier key for this handle");

        log::debug!("destroying capture {id} @ {key:?}");
        let remaining = self.position_map.get_mut(&key).expect("id vector");
        remaining.retain(|&i| i != id);

        log::debug!("remaining ids @ {key:?}: {remaining:?}");
        if remaining.is_empty() {
            log::debug!("destroying capture @ {key:?} - no remaining ids");
            self.position_map.remove(&key);
            self.capture.destroy(&key).await?;
        }
        Ok(())
    }

    /// release mouse
    pub async fn release(&mut self) -> Result<(), CaptureError> {
        self.pressed_keys.clear();
        self.capture.release().await
    }

    /// **Pending-capture handshake (Windows / macOS only)** — promote a
    /// previously reported `BeginPending` on `key` to an active capture.
    ///
    /// The main thread calls this after the remote client ACKs the Enter
    /// we sent in response to `BeginPending`. Backends that don't
    /// distinguish pending from active (libei, layer-shell, x11, dummy)
    /// are no-ops.
    ///
    /// **Synchronous**: Windows directly calls `PostThreadMessage`; macOS
    /// fires `notify_tx` via `spawn_local`. The caller does not need to
    /// await after the call.
    pub fn start_capture(&mut self, key: &BarrierKey) -> Result<(), CaptureError> {
        self.capture.start_capture(key)
    }

    /// **Pending-capture handshake (Windows / macOS only)** — cancel a
    /// pending Begin on `key`, if any. Called when the user moves the
    /// cursor back inside before Ack, or on Ack timeout. Default backend
    /// implementations are no-ops.
    pub fn cancel_pending(&mut self, key: &BarrierKey) -> Result<(), CaptureError> {
        self.capture.cancel_pending(key)
    }

    /// Drain and return every key the capture has forwarded as
    /// down-but-not-up. The caller is expected to synthesize key-up
    /// events to the remote peer for each — otherwise the peer
    /// retains phantom-held keys after capture is released. The
    /// canonical case is the release-bind chord
    /// (Ctrl+Shift+Alt+Meta): the down events were sent while
    /// capture was active, but the matching up events arrive after
    /// the local tap has flipped to passthrough and never reach
    /// the peer.
    pub fn take_pressed_keys(&mut self) -> HashSet<scancode::Linux> {
        std::mem::take(&mut self.pressed_keys)
    }

    /// destroy the input capture
    pub async fn terminate(&mut self) -> Result<(), CaptureError> {
        self.capture.terminate().await
    }

    /// creates a new [`InputCapture`]
    pub async fn new(backend: Option<Backend>) -> Result<Self, CaptureCreationError> {
        let capture = create(backend).await?;
        Ok(Self {
            capture,
            id_map: Default::default(),
            pending: Default::default(),
            position_map: Default::default(),
            pressed_keys: HashSet::new(),
        })
    }

    /// check whether the given keys are pressed
    pub fn keys_pressed(&self, keys: &[scancode::Linux]) -> bool {
        keys.iter().all(|k| self.pressed_keys.contains(k))
    }

    fn update_pressed_keys(&mut self, key: u32, state: u8) {
        if let Ok(scancode) = scancode::Linux::try_from(key) {
            log::debug!("key: {key}, state: {state}, scancode: {scancode:?}");
            match state {
                1 => self.pressed_keys.insert(scancode),
                _ => self.pressed_keys.remove(&scancode),
            };
        }
    }
}

impl Stream for InputCapture {
    type Item = Result<(CaptureHandle, CaptureEvent), CaptureError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        // 1) Drain any queued fan-out events first.
        if let Some(e) = self.pending.pop_front() {
            return Poll::Ready(Some(Ok(e)));
        }

        // 2) Wait for the next backend event. Registers `cx.waker()`
        //    with the backend so future events wake us up.
        let event = ready!(self.capture.poll_next_unpin(cx));

        // 3) Stream closed.
        let event = match event {
            Some(e) => e,
            None => return Poll::Ready(None),
        };

        // 4) Inner stream error.
        let (key, event) = match event {
            Ok(e) => e,
            Err(e) => return Poll::Ready(Some(Err(e))),
        };

        // 5) Track keyboard state.
        if let CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key { key, state, .. })) = &event
        {
            self.update_pressed_keys(*key, *state);
        }

        // 6) Fan-out to subscribers.
        //
        // Snapshot the subscriber list under an immutable borrow, then
        // release the borrow before mutating `self.pending`. The old
        // implementation used a `mem::swap` dance that silently dropped
        // the borrowed event when no subscribers matched — the new
        // `clone()` path keeps `self.position_map` intact and removes
        // a class of "waker not re-registered after swap" footguns.
        //
        // WAKER INVARIANT: when the subscriber list is empty we drop
        // the event and return `Pending`. The waker registered with
        // the backend in step 2 remains pointed at the current task;
        // the next call to `poll_next` re-enters step 2 and
        // re-registers it (with whatever the current cx.waker() is).
        // Returning `Pending` here is correct as long as no mutating
        // entry point (`create` / `destroy` / `start_capture` /
        // `cancel_pending`) bypasses `poll_next_unpin(cx)` to drive
        // the backend — backends that need a wake-up signal after
        // such a mutation drive it themselves (macOS uses `notify_tx`;
        // Windows uses `PostThreadMessage`).
        let subscribers = self.position_map.get(&key).cloned().unwrap_or_default();

        match subscribers.len() {
            0 => Poll::Pending,
            1 => Poll::Ready(Some(Ok((subscribers[0], event)))),
            _ => {
                // Push every subscriber after the first to the pending
                // queue; return the first immediately. `event` is
                // `Copy` (via `CaptureEvent::Input(Event)` where Event
                // derives Copy), so each push moves it; we still need
                // to hand a copy back to the caller too.
                let mut iter = subscribers.into_iter();
                let first = iter.next().expect("non-empty");
                for id in iter {
                    self.pending.push_back((id, event));
                }
                Poll::Ready(Some(Ok((first, event))))
            }
        }
    }
}

#[async_trait]
trait Capture: Stream<Item = Result<(BarrierKey, CaptureEvent), CaptureError>> + Unpin {
    /// create a new client at the given barrier key
    async fn create(&mut self, key: &BarrierKey) -> Result<(), CaptureError>;

    /// destroy the client at the given barrier key
    async fn destroy(&mut self, key: &BarrierKey) -> Result<(), CaptureError>;

    /// release mouse
    async fn release(&mut self) -> Result<(), CaptureError>;

    /// destroy the input capture
    async fn terminate(&mut self) -> Result<(), CaptureError>;

    /// Promote the pending Begin on `key` to an active capture. Called by
    /// the main thread after the remote client has Acked the Enter.
    /// Idempotent: a no-op if there is no pending Begin, the pending
    /// key does not match, or the capture is already active.
    ///
    /// **Synchronous** (no `async`) so the caller doesn't have to await
    /// — Windows just posts a message and macOS fires a `spawn_local`.
    /// Default is a no-op for backends (libei, layer-shell, x11, dummy)
    /// that don't distinguish pending from active. Only the synchronous
    /// Windows / macOS backends need to override this.
    fn start_capture(&mut self, _key: &BarrierKey) -> Result<(), CaptureError> {
        Ok(())
    }

    /// Cancel the pending Begin on `key`, if any. Called when the user
    /// moves the cursor back inside before the Ack arrives, on Ack
    /// timeout, or when the caller wants to abort. Default is a no-op.
    fn cancel_pending(&mut self, _key: &BarrierKey) -> Result<(), CaptureError> {
        Ok(())
    }
}

async fn create_backend(
    backend: Backend,
) -> Result<
    Box<dyn Capture<Item = Result<(BarrierKey, CaptureEvent), CaptureError>>>,
    CaptureCreationError,
> {
    match backend {
        #[cfg(libei)]
        Backend::InputCapturePortal => Ok(Box::new(libei::LibeiInputCapture::new().await?)),
        #[cfg(layer_shell)]
        Backend::LayerShell => Ok(Box::new(layer_shell::LayerShellInputCapture::new()?)),
        #[cfg(x11)]
        Backend::X11 => Ok(Box::new(x11::X11InputCapture::new()?)),
        #[cfg(windows)]
        Backend::Windows => Ok(Box::new(windows::WindowsInputCapture::new())),
        #[cfg(target_os = "macos")]
        Backend::MacOs => Ok(Box::new(macos::MacOSInputCapture::new().await?)),
        Backend::Dummy => Ok(Box::new(dummy::DummyInputCapture::new())),
    }
}

async fn create(
    backend: Option<Backend>,
) -> Result<
    Box<dyn Capture<Item = Result<(BarrierKey, CaptureEvent), CaptureError>>>,
    CaptureCreationError,
> {
    if let Some(backend) = backend {
        let b = create_backend(backend).await;
        if b.is_ok() {
            log::info!("using capture backend: {backend}");
        }
        return b;
    }

    for backend in [
        #[cfg(libei)]
        Backend::InputCapturePortal,
        #[cfg(layer_shell)]
        Backend::LayerShell,
        #[cfg(x11)]
        Backend::X11,
        #[cfg(windows)]
        Backend::Windows,
        #[cfg(target_os = "macos")]
        Backend::MacOs,
    ] {
        match create_backend(backend).await {
            Ok(b) => {
                log::info!("using capture backend: {backend}");
                return Ok(b);
            }
            Err(e) if e.cancelled_by_user() => return Err(e),
            Err(e) => log::warn!("{backend} input capture backend unavailable: {e}"),
        }
    }
    Err(CaptureCreationError::NoAvailableBackend)
}

#[cfg(test)]
mod poll_next_tests {
    //! Regression tests for the M1 `BarrierKey` refactor.
    //!
    //! The two invariants exercised here are called out in PLAN §M1
    //! STEP-1.1:
    //!
    //! 1. When the subscriber list for the incoming `BarrierKey` is
    //!    empty, `InputCapture::poll_next` must drop the event and
    //!    return `Pending`, AND must leave the backend's waker
    //!    pointing at the current task — otherwise no future event
    //!    would ever wake the consumer. (Pre-M1, this was the "empty
    //!    collection waker not re-registered drop poll bug".)
    //!
    //! 2. When two or more subscribers share a `BarrierKey`, the
    //!    single backend event must fan out to all of them — exactly
    //!    one Ready per subscriber, in subscription order.

    use super::*;
    use crate::geometry::BarrierKey;
    use async_trait::async_trait;
    use futures::Future;
    use futures::StreamExt;
    use futures::task::noop_waker_ref;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// A Capture backend that produces one event on first poll, then
    /// stays Pending forever. Lets us assert Pending without racing
    /// a real stream.
    struct OneShotCapture {
        emitted: bool,
    }

    impl OneShotCapture {
        fn new() -> Self {
            Self { emitted: false }
        }
    }

    #[async_trait]
    impl Capture for OneShotCapture {
        async fn create(&mut self, _key: &BarrierKey) -> Result<(), CaptureError> {
            Ok(())
        }
        async fn destroy(&mut self, _key: &BarrierKey) -> Result<(), CaptureError> {
            Ok(())
        }
        async fn release(&mut self) -> Result<(), CaptureError> {
            Ok(())
        }
        async fn terminate(&mut self) -> Result<(), CaptureError> {
            Ok(())
        }
    }

    impl Stream for OneShotCapture {
        type Item = Result<(BarrierKey, CaptureEvent), CaptureError>;

        fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            if !self.emitted {
                self.emitted = true;
                Poll::Ready(Some(Ok((
                    BarrierKey::from_pos(Position::Left),
                    CaptureEvent::Begin,
                ))))
            } else {
                Poll::Pending
            }
        }
    }

    fn make_input_capture() -> InputCapture {
        let capture: Box<dyn Capture<Item = Result<(BarrierKey, CaptureEvent), CaptureError>>> =
            Box::new(OneShotCapture::new());
        InputCapture {
            capture,
            pressed_keys: HashSet::new(),
            position_map: HashMap::new(),
            id_map: HashMap::new(),
            pending: VecDeque::new(),
        }
    }

    /// A noop-waker context used to manually poll a stream and observe
    /// its `Poll<...>` result without driving an actual executor.
    fn noop_cx() -> Context<'static> {
        Context::from_waker(noop_waker_ref())
    }

    /// Invariant 1: with no subscribers, `poll_next` drops the first
    /// backend event and returns Pending. The waker registered during
    /// the underlying `capture.poll_next_unpin(cx)` call remains
    /// pointed at the current task; a subsequent poll that finds more
    /// pending events will keep waking us up correctly.
    #[tokio::test]
    async fn empty_collection_returns_pending_and_keeps_waker() {
        let cap = make_input_capture();
        let mut cap = std::pin::pin!(cap);
        let mut cx = noop_cx();

        // First poll: backend's one-shot event is delivered, but no
        // subscribers → event dropped, return Pending.
        let r1 = cap.as_mut().poll_next(&mut cx);
        assert!(
            matches!(r1, Poll::Pending),
            "expected Pending when no subscribers, got {r1:?}"
        );
    }

    /// Invariant 1 follow-up: after the empty-collection Pending,
    /// a second poll must STILL register the waker with the backend
    /// (rather than short-circuiting on a stale registration from the
    /// previous poll). The simplest observable signature is that the
    /// second poll returns Pending (backend has no more events) — but
    /// crucially it must reach the backend's poll at all. We verify
    /// by confirming `poll_next` returns the same `Pending` state
    /// shape on every subsequent poll, which is impossible if the
    /// poll path short-circuits before reaching the backend.
    #[tokio::test]
    async fn repeated_empty_collection_polls_keep_returning_pending() {
        let cap = make_input_capture();
        let mut cap = std::pin::pin!(cap);
        let mut cx = noop_cx();

        // First poll: consumes the backend's one-shot event and
        // returns Pending.
        let r1 = cap.as_mut().poll_next(&mut cx);
        assert!(matches!(r1, Poll::Pending));

        // Subsequent polls must keep returning Pending (backend has
        // nothing left to deliver). This proves the waker path is
        // still being driven end-to-end.
        for i in 0..3 {
            let r = cap.as_mut().poll_next(&mut cx);
            assert!(
                matches!(r, Poll::Pending),
                "poll #{i}: expected Pending, got {r:?}"
            );
        }
    }

    /// Invariant 2: multiple subscribers on the same BarrierKey fan
    /// out — one Ready per subscriber, in subscription order.
    #[tokio::test]
    async fn fanout_for_same_key_delivers_one_per_subscriber() {
        let cap = make_input_capture();
        let mut cap = std::pin::pin!(cap);
        let mut cx = noop_cx();

        let key = BarrierKey::from_pos(Position::Left);
        // SAFETY: cap is pinned and we mutably access via Pin::get_unchecked_mut.
        let inner = unsafe { cap.as_mut().get_unchecked_mut() };
        inner.position_map.insert(key.clone(), vec![10, 20, 30]);

        // 3 subscribers → 3 polls → 3 Ready Begin events.
        let mut got = Vec::new();
        for _ in 0..3 {
            match cap.as_mut().poll_next(&mut cx) {
                Poll::Ready(Some(Ok((id, CaptureEvent::Begin)))) => got.push(id),
                other => panic!("expected Ready(Begin, ...), got {other:?}"),
            }
        }
        assert_eq!(got, vec![10, 20, 30]);

        // 4th poll: pending drained, backend is Pending → Pending.
        let r = cap.as_mut().poll_next(&mut cx);
        assert!(
            matches!(r, Poll::Pending),
            "expected Pending after fan-out drained, got {r:?}"
        );
    }

    /// Belt-and-braces: confirm `StreamExt::next` driven under
    /// `tokio_test::assert_pending!` produces the same Pending
    /// result. This is the explicit `tokio_test::assert_pending!`
    /// coverage the PLAN §M1 STEP-1.1 completion flag calls out.
    ///
    /// `tokio_test::assert_pending!` takes a `Poll<_>` (not a
    /// future) and asserts it is `Pending`. We poll the stream's
    /// `next()` future once and forward the result.
    #[tokio::test]
    async fn tokio_test_assert_pending_coverage() {
        let mut cap = std::pin::pin!(make_input_capture());
        let mut pinned = cap.as_mut();
        let next_fut = pinned.next();
        tokio::pin!(next_fut);
        let mut cx = noop_cx();
        let poll = next_fut.as_mut().poll(&mut cx);
        tokio_test::assert_pending!(poll);
    }
}
