use async_trait::async_trait;
use core::task::{Context, Poll};
use event_thread::EventThread;
use futures::Stream;
use std::pin::Pin;

use std::task::ready;
use tokio::sync::mpsc::{Receiver, channel};
use tokio::sync::watch;

use super::{BarrierKey, Capture, CaptureError, CaptureEvent};
use crate::geometry::MonitorInfo;

mod event_thread;

pub struct WindowsInputCapture {
    // M1: the event channel carries `(BarrierKey, CaptureEvent)`
    // pairs end-to-end. monitor / offset / span stay at their legacy
    // defaults (`None` / `0` / `10000`) until M2 wires monitor info
    // end-to-end.
    event_rx: Receiver<(BarrierKey, CaptureEvent)>,
    event_thread: EventThread,
    /// Held here so the public `monitor_changes()` method can hand
    /// out new receivers to upstream consumers (STEP-2.6 service
    /// layer). The actual monitor data lives inside `event_thread`
    /// (the message-loop thread is the sole writer); this receiver is
    /// just a stable view into the watch channel.
    #[allow(dead_code)] // STEP-2.5/2.6 will consume this
    monitors_rx: watch::Receiver<Vec<MonitorInfo>>,
}

#[async_trait]
impl Capture for WindowsInputCapture {
    async fn create(&mut self, key: &BarrierKey) -> Result<(), CaptureError> {
        self.event_thread.create(key.clone());
        Ok(())
    }

    async fn destroy(&mut self, key: &BarrierKey) -> Result<(), CaptureError> {
        self.event_thread.destroy(key.clone());
        Ok(())
    }

    async fn release(&mut self) -> Result<(), CaptureError> {
        self.event_thread.release_capture();
        Ok(())
    }

    /// **Pending-capture handshake**: The main thread calls this method after the peer
    /// acks Enter, promoting the pending Begin on `key` to active (cursor hidden,
    /// events consumed). Forwards to `EventThread::start_capture` through the
    /// message-loop path. Returns synchronously (the message has already been posted
    /// via PostThreadMessage and is processed asynchronously by the Windows message loop).
    fn start_capture(&mut self, key: &BarrierKey) -> Result<(), CaptureError> {
        self.event_thread.start_capture(key.clone());
        Ok(())
    }

    /// **Pending-capture handshake**: Cancels the pending Begin on `key`
    /// (network drop / 500ms timeout / release-bind pressed while pending).
    /// Forwards to `EventThread::cancel_pending`. Returns synchronously.
    fn cancel_pending(&mut self, key: &BarrierKey) -> Result<(), CaptureError> {
        self.event_thread.cancel_pending(key.clone());
        Ok(())
    }

    async fn terminate(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }
}

impl WindowsInputCapture {
    pub(crate) fn new() -> Self {
        let (event_tx, event_rx) = channel(10);
        let event_thread = EventThread::new(event_tx);
        // Take an initial receiver so we can hand out additional
        // receivers later (`subscribe` clones the channel — the
        // original receiver is what we hold onto).
        let monitors_rx = event_thread.monitor_changes();
        Self {
            event_thread,
            event_rx,
            monitors_rx,
        }
    }

    /// Subscribe to the latest monitor list. Each call returns a new
    /// receiver that sees every future update (a new entry is published
    /// on every `WM_DISPLAYCHANGE` and once during construction).
    /// STEP-2.6 service layer holds the receiver and forwards
    /// `MonitorsChanged` events to the IPC frontend.
    #[allow(dead_code)] // STEP-2.5/2.6 will consume this
    pub(crate) fn monitor_changes(&self) -> watch::Receiver<Vec<MonitorInfo>> {
        self.monitors_rx.clone()
    }

    /// Snapshot of the most recent monitor list, captured without
    /// touching the watch channel. Used by STEP-2.5's
    /// `Capture::monitors()` impl when polling is acceptable and the
    /// caller does not need a subscription.
    #[allow(dead_code)] // STEP-2.5/2.6 will consume this
    pub(crate) fn current_monitors(&self) -> Vec<MonitorInfo> {
        self.monitors_rx.borrow().clone()
    }
}

impl Stream for WindowsInputCapture {
    type Item = Result<(BarrierKey, CaptureEvent), CaptureError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // M1: events come pre-tagged with a BarrierKey from
        // `event_thread::blocking_send_event`. monitor / offset /
        // span stay at their legacy defaults until M2 wires monitor
        // info end-to-end.
        match ready!(self.event_rx.poll_recv(cx)) {
            None => Poll::Ready(None),
            Some(e) => Poll::Ready(Some(Ok(e))),
        }
    }
}
