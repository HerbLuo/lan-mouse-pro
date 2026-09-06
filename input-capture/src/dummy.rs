use std::f64::consts::PI;
use std::pin::Pin;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use async_trait::async_trait;
use futures_core::Stream;
use input_event::PointerEvent;
use tokio::time::{self, Instant, Interval};

use super::{BarrierKey, Capture, CaptureError, CaptureEvent, Position};

pub struct DummyInputCapture {
    start: Option<Instant>,
    interval: Interval,
    offset: (i32, i32),
    /// Round-robin schedule of barrier keys to emit events under. The
    /// M1 default is a single `BarrierKey::from_pos(Position::Left)`
    /// entry — tests / M2+ service injection can replace this with a
    /// richer schedule via [`DummyInputCapture::with_keys`].
    keys: Vec<BarrierKey>,
    /// Index of the next key to emit under (modulo `keys.len()`).
    next_key_idx: usize,
}

impl DummyInputCapture {
    pub fn new() -> Self {
        Self::with_keys(vec![BarrierKey::from_pos(Position::Left)])
    }

    /// Build a dummy backend that cycles through `keys` on each poll,
    /// emitting one Motion / Begin event per poll under the next key
    /// in round-robin order. Used by tests / the M2+ service to
    /// inject arbitrary `BarrierKey` schedules (including ones with
    /// `monitor` / `offset` / `span` populated) without depending on
    /// a real OS backend.
    pub fn with_keys(keys: Vec<BarrierKey>) -> Self {
        // Empty input is meaningless for a round-robin backend —
        // fall back to the legacy single-Left schedule so the
        // backend always has at least one key to emit under.
        let keys = if keys.is_empty() {
            vec![BarrierKey::from_pos(Position::Left)]
        } else {
            keys
        };
        Self {
            start: None,
            interval: time::interval(Duration::from_millis(1)),
            offset: (0, 0),
            next_key_idx: 0,
            keys,
        }
    }
}

impl Default for DummyInputCapture {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Capture for DummyInputCapture {
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

const FREQUENCY_HZ: f64 = 1.0;
const RADIUS: f64 = 100.0;

impl Stream for DummyInputCapture {
    type Item = Result<(BarrierKey, CaptureEvent), CaptureError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let current = ready!(self.interval.poll_tick(cx));
        let event = match self.start {
            None => {
                self.start.replace(current);
                CaptureEvent::Begin
            }
            Some(start) => {
                let elapsed = start.elapsed();
                let elapsed_sec_f64 = elapsed.as_secs_f64();
                let second_fraction = elapsed_sec_f64 - elapsed_sec_f64 as u64 as f64;
                let radians = second_fraction * 2. * PI * FREQUENCY_HZ;
                let offset = (radians.cos() * RADIUS * 2., (radians * 2.).sin() * RADIUS);
                let offset = (offset.0 as i32, offset.1 as i32);
                let relative_motion = (offset.0 - self.offset.0, offset.1 - self.offset.1);
                self.offset = offset;
                let (dx, dy) = (relative_motion.0 as f64, relative_motion.1 as f64);
                CaptureEvent::Input(input_event::Event::Pointer(PointerEvent::Motion {
                    time: 0,
                    dx,
                    dy,
                }))
            }
        };
        // M1: dummy cycles through its injected key schedule. With
        // the default `new()` constructor that's a single
        // `BarrierKey::from_pos(Position::Left)`; tests / service
        // injection can swap in a richer list via `with_keys`.
        let key = self.keys[self.next_key_idx % self.keys.len()].clone();
        self.next_key_idx = self.next_key_idx.wrapping_add(1);
        Poll::Ready(Some(Ok((key, event))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CaptureEvent;
    use futures::StreamExt;

    /// `new()` produces a backend that emits every event under the
    /// legacy default `BarrierKey::from_pos(Position::Left)` — i.e.
    /// the same `(Position::Left, _)` shape callers have always
    /// expected, just lifted into the new `BarrierKey` envelope.
    #[tokio::test]
    async fn default_emits_legacy_left_key() {
        let mut cap = DummyInputCapture::new();
        let first = cap.next().await.expect("first event").expect("ok");
        let (key, _event) = first;
        assert_eq!(key, BarrierKey::from_pos(Position::Left));
    }

    /// `with_keys` round-robins across the supplied schedule, in
    /// order. Verifies that the externally injected list is honored
    /// rather than silently replaced by the default.
    #[tokio::test]
    async fn with_keys_round_robins_across_schedule() {
        let schedule = vec![
            BarrierKey::from_pos(Position::Left),
            BarrierKey::from_pos(Position::Right),
            BarrierKey::from_pos(Position::Top),
        ];
        let mut cap = DummyInputCapture::with_keys(schedule.clone());
        let mut seen = Vec::new();
        for _ in 0..schedule.len() {
            let (key, _event) = cap.next().await.expect("event").expect("ok");
            seen.push(key);
        }
        assert_eq!(seen, schedule);
        // Wrap-around: after `schedule.len()` emissions the next
        // entry is again the first one.
        let (key, _) = cap.next().await.expect("event").expect("ok");
        assert_eq!(key, schedule[0]);
    }

    /// `with_keys` with an empty list falls back to the legacy
    /// single-Left schedule so the backend never has zero keys to
    /// emit under (which would be a division-by-zero modulo).
    #[tokio::test]
    async fn with_keys_empty_falls_back_to_default() {
        let mut cap = DummyInputCapture::with_keys(vec![]);
        let (key, _) = cap.next().await.expect("event").expect("ok");
        assert_eq!(key, BarrierKey::from_pos(Position::Left));
    }

    /// `with_keys` preserves full BarrierKey payloads (not just the
    /// `Position` half). Confirms monitor / offset / span round-trip
    /// through the schedule.
    #[tokio::test]
    async fn with_keys_preserves_monitor_offset_span() {
        let k = BarrierKey {
            pos: Position::Bottom,
            monitor: Some("display-A".to_string()),
            offset: 2500,
            span: 5000,
        };
        let mut cap = DummyInputCapture::with_keys(vec![k.clone()]);
        let (got, _) = cap.next().await.expect("event").expect("ok");
        assert_eq!(got, k);
    }

    /// The first event from a freshly-constructed dummy is always
    /// `Begin` (not a Motion), regardless of how the schedule was
    /// populated. Carried forward from the original dummy behavior
    /// — keeps the existing fan-out tests in `poll_next_tests`
    /// working unchanged.
    #[tokio::test]
    async fn first_event_is_begin_regardless_of_schedule() {
        let schedule = vec![BarrierKey::from_pos(Position::Left)];
        let mut cap = DummyInputCapture::with_keys(schedule);
        let (_, event) = cap.next().await.expect("event").expect("ok");
        assert!(matches!(event, CaptureEvent::Begin));
    }
}
