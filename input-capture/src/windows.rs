use async_trait::async_trait;
use core::task::{Context, Poll};
use event_thread::EventThread;
use futures::Stream;
use std::pin::Pin;

use std::task::ready;
use tokio::sync::mpsc::{Receiver, channel};

use super::{BarrierKey, Capture, CaptureError, CaptureEvent};

mod event_thread;

pub struct WindowsInputCapture {
    // M1: the event channel carries `(BarrierKey, CaptureEvent)`
    // pairs end-to-end. monitor / offset / span stay at their legacy
    // defaults (`None` / `0` / `10000`) until M2 wires monitor info
    // end-to-end.
    event_rx: Receiver<(BarrierKey, CaptureEvent)>,
    event_thread: EventThread,
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
        Self {
            event_thread,
            event_rx,
        }
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
