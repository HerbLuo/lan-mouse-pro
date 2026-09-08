use super::{
    BarrierKey, Capture, CaptureError, CaptureEvent, Position, error::MacosCaptureCreationError,
};
use crate::geometry::{DisplayBound, DisplayRect, MonitorInfo};
use async_trait::async_trait;
use bitflags::bitflags;
use core_foundation::{
    base::{CFRelease, CFTypeRef, TCFType, kCFAllocatorDefault},
    date::CFTimeInterval,
    dictionary::CFDictionary,
    number::{CFBooleanRef, CFNumber, CFNumberRef, kCFBooleanTrue},
    runloop::{CFRunLoop, CFRunLoopSource, kCFRunLoopCommonModes},
    string::{CFString, CFStringCreateWithCString, CFStringRef, kCFStringEncodingUTF8},
};
use core_graphics::{
    base::{CGError, kCGErrorSuccess},
    display::{CGDirectDisplayID, CGDisplay, CGMainDisplayID, CGPoint},
    event::{
        CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions,
        CGEventTapPlacement, CGEventTapProxy, CGEventType, CallbackResult, EventField,
    },
    event_source::{CGEventSource, CGEventSourceStateID},
};
use futures_core::Stream;
use input_event::{
    BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event, KeyboardEvent, PointerEvent,
};
use keycode::{KeyMap, KeyMapping};
use libc::c_void;
use once_cell::unsync::Lazy;
use std::{
    collections::HashSet,
    ffi::{CString, c_char},
    pin::Pin,
    sync::{Arc, OnceLock},
    task::{Context, Poll, ready},
    thread::{self},
};
use tokio::sync::{
    Mutex,
    mpsc::{self, Receiver, Sender},
    oneshot, watch,
};

#[derive(Debug)]
struct InputCaptureState {
    /// active capture barrier keys
    active_clients: Lazy<HashSet<BarrierKey>>,
    /// the currently entered capture barrier key, if any
    current_key: Option<BarrierKey>,
    /// Intermediate pending-capture state: the mouse has crossed an edge
    /// but the main thread has not yet received the Ack.
    /// Mutually exclusive with `current_key`. Cleared on promotion to
    /// active and on cancel. While the backend is awaiting Ack it does
    /// not hide the cursor and does not warp it — the cursor stays on
    /// the host.
    pending_key: Option<BarrierKey>,
    /// Position where the cursor was when it crossed the barrier — the
    /// actual previous cursor location, not a bbox-derived corner.
    /// Used as the warp target on pending->active promotion so the
    /// peer sees a sensible Motion delta, and as the diagnostic point
    /// for debug logging.
    enter_position: Option<CGPoint>,
    /// All currently-attached displays paired with their stable
    /// [`MonitorId`]. Re-fetched on display reconfiguration; the
    /// backend holds the per-display representation rather than a
    /// precomputed bbox so it can answer per-display queries (which
    /// display contained the cursor, which display owns the edge
    /// being crossed, etc.). The M3 STEP-3.4 change upgraded the
    /// element type from `DisplayRect` to `DisplayBound` so the
    /// barrier-detection query can carry the right `monitor` field
    /// instead of always being `None`.
    displays: Vec<DisplayBound>,
    /// current state of modifier keys
    modifier_state: XMods,
    /// Latest enumerated monitor list. Updated on every
    /// `DisplayReconfigured` (and once during `new()`); a clone of the
    /// sender is held on `MacOSInputCapture` itself so the public
    /// `monitor_changes()` method can hand out new receivers to
    /// upstream consumers (STEP-2.6 service layer). The watch channel
    /// is the single source of truth — `current_monitors()` reads
    /// from `monitors_tx.borrow()` rather than from a redundant
    /// in-state cache.
    monitors_tx: watch::Sender<Vec<MonitorInfo>>,
}

#[derive(Debug)]
enum ProducerEvent {
    Release,
    Create(BarrierKey),
    Destroy(BarrierKey),
    /// Legacy macOS path used `Grab` directly when there was no pending
    /// state. Retained for compatibility; new code no longer emits it.
    #[allow(dead_code)]
    Grab(BarrierKey),
    /// Promotes a pending capture to active: the main thread calls
    /// `start_capture` after receiving the remote Ack and forwards it
    /// here. On promotion: warp the cursor, hide the cursor, emit Begin.
    StartCapture(BarrierKey),
    /// Pending cancel: triggered by main-thread `cancel_pending`,
    /// network loss, or the 500ms timeout. Emits `CancelPending` if
    /// the position is still pending.
    CancelPending(BarrierKey),
    EventTapDisabled,
    DisplayReconfigured,
    /// Reserved for STEP-2.6+ to manually trigger a refresh of the
    /// monitor list (e.g. from a service-side IPC command). The
    /// `DisplayReconfigured` path does NOT use this variant — it
    /// updates `monitors_tx` directly inside `handle_producer_event`
    /// because the producer task is the only writer on this side.
    /// Holding the variant here keeps the dispatch surface uniform so
    /// a future caller doesn't have to introduce a separate enum.
    #[allow(dead_code)]
    MonitorsChanged(Vec<MonitorInfo>),
}

impl InputCaptureState {
    fn new(
        monitors_tx: watch::Sender<Vec<MonitorInfo>>,
    ) -> Result<Self, MacosCaptureCreationError> {
        let mut res = Self {
            active_clients: Lazy::new(HashSet::new),
            current_key: None,
            pending_key: None,
            enter_position: None,
            displays: Vec::new(),
            modifier_state: Default::default(),
            monitors_tx,
        };
        res.update_bounds()?;
        // Publish the initial monitor list so subscribers can read the
        // current state without waiting for the first reconfiguration.
        let initial = enumerate_monitors(&res.displays);
        log::info!("initial monitors: {} monitor(s)", initial.len());
        for m in &initial {
            log::info!(
                "  monitor: id={} name={:?} pos={:?} size={:?} primary={} scale={}",
                m.id,
                m.name,
                m.position,
                m.size,
                m.primary,
                m.scale
            );
        }
        let _ = res.monitors_tx.send(initial);
        Ok(res)
    }

    /// Detect a barrier crossing. Thin wrapper around the pure
    /// `crossed_pure` geometry helper: the CGEvent-side work (read
    /// prev/curr, build the query key) is trivial enough to live
    /// inline; everything testable (position detection, monitor-id
    /// lookup, key match) is in `geometry::crossed_pure`.
    fn crossed(&self, prev_pos: (f64, f64), curr_pos: (f64, f64)) -> Option<BarrierKey> {
        let key = crate::geometry::crossed_pure(
            prev_pos,
            curr_pos,
            &self.displays,
            &self.active_clients,
        )?;
        log::debug!("Crossed barrier into: {key:?}");
        Some(key)
    }

    /// Re-fetch the list of active displays from Quartz and store them
    /// as `DisplayBound` entries (axis-aligned rectangle + stable
    /// `monitor_id`). Called on startup and on every
    /// `CGDisplayReconfiguration` notification.
    ///
    /// **Important**: this clears `displays` before refilling, so a
    /// transient state where Quartz returns an empty list (e.g. the
    /// user is mid-disconnect) yields an empty `displays` rather than
    /// a monotonically-growing union. The previous implementation
    /// held a single `Bounds` struct whose `xmin/xmax/ymin/ymax`
    /// could only ever expand; that made the cursor "stick" to a
    /// stale corner after unplugging a monitor.
    ///
    /// **M3 STEP-3.4 single-source-of-truth fix**: previously
    /// `update_bounds` walked `CGDisplay::active_displays + bounds()`
    /// while `enumerate_monitors` walked the same `active_displays`
    /// again + `IODisplayCreateInfoDictionary`, with no guarantee
    /// the two saw the same list mid-reconfigure. Now both paths go
    /// through `enumerate_monitors_for_ids(&active_ids)` which
    /// shares the IOKit code, and the bounds are derived from the
    /// resulting `MonitorInfo.position/size` (no double Quartz
    /// query). The pure `build_display_bounds` adapter (testable in
    /// isolation) joins the two lists by index — both come from the
    /// same `active_ids` iteration order, so the join is
    /// deterministic.
    fn update_bounds(&mut self) -> Result<(), MacosCaptureCreationError> {
        let active_ids =
            CGDisplay::active_displays().map_err(MacosCaptureCreationError::ActiveDisplays)?;
        let monitors = enumerate_monitors_for_ids(&active_ids);
        self.displays = build_display_bounds(&active_ids, &monitors);

        log::debug!("Updated displays: {0:?}", self.displays);
        Ok(())
    }

    /// start the input capture by
    ///
    /// Note: this method is no longer called. After the pending-capture
    /// refactor, promotion is handled by the tap thread via
    /// [`ProducerEvent::StartCapture`] (see `handle_producer_event` and
    /// [`InputCaptureState::compute_edge_point`] below). This method is
    /// retained as a clear reference implementation of "what to do when
    /// a capture becomes active" and to guard against regressions.
    #[allow(dead_code)]
    fn start_capture(&mut self, event: &CGEvent, position: Position) -> Result<(), CaptureError> {
        // Reference implementation of the legacy "promote to active
        // immediately" path. The current pending-capture flow goes
        // through `ProducerEvent::StartCapture` instead; this method
        // is retained as a regression guard.
        //
        // `enter_position` records the cursor's pre-event location
        // (the `prev_pos` shape used everywhere else in this file);
        // `reset_cursor` does the per-display warp lookup.
        let location = event.location();
        self.enter_position = Some(location);
        self.reset_cursor(position)
    }

    /// Pending-capture helper: computes the 1px-inside warp target for
    /// the edge identified by `pos`, **on the same display the cursor
    /// was on at `prev_pos`**.
    ///
    /// This is the geometric fix for the macOS multi-display bug the
    /// PLAN calls out: previously the backend held a single `Bounds`
    /// representing the union of all displays, and any "left" barrier
    /// crossing warped the cursor to the union's `xmin` — which on a
    /// 2x1 horizontal layout is the leftmost edge of the *left*
    /// display, even when the user was actually on the right display.
    ///
    /// Now we look up the display that contains `prev_pos`, and warp
    /// to *that display's* corresponding edge. Returning `None` means
    /// `prev_pos` wasn't on any known display (e.g. transient empty
    /// state mid-reconfigure) — in that case the caller should NOT
    /// warp to a stale union bbox, the very bug we're fixing.
    fn compute_edge_point(&self, prev_pos: (f64, f64), pos: Position) -> Option<CGPoint> {
        let edge_offset = 1.0;
        let display = crate::geometry::display_containing_bound(&self.displays, prev_pos)?;
        let mut p = CGPoint {
            x: prev_pos.0,
            y: prev_pos.1,
        };
        match pos {
            Position::Left => p.x = display.rect.left() + edge_offset,
            Position::Right => p.x = display.rect.right() - edge_offset,
            Position::Top => p.y = display.rect.top() + edge_offset,
            Position::Bottom => p.y = display.rect.bottom() - edge_offset,
        }
        Some(p)
    }

    /// Resets the cursor to the entry position for the active capture.
    ///
    /// `enter_position` holds the actual cursor location from the
    /// moment the user crossed the barrier (STEP 0.5). We compute the
    /// warp target via `compute_edge_point` so the cursor lands on
    /// the same display's inner-1px edge — not on a bbox-derived
    /// corner of the union. When the previous display is gone (e.g.
    /// mid-reconfigure) `compute_edge_point` returns `None`; we then
    /// fall back to nudging `enter_position` 1px in the direction of
    /// the active `pos`, which is at least correct in direction
    /// without ever touching the old union bbox.
    fn reset_cursor(&mut self, pos: Position) -> Result<(), CaptureError> {
        let prev = self.enter_position.expect("capture active");
        let warp = self
            .compute_edge_point((prev.x, prev.y), pos)
            .unwrap_or_else(|| {
                log::warn!(
                    "reset_cursor: no display contains prev={prev:?} pos={pos:?}; \
                     falling back to prev + 1px toward pos"
                );
                let mut fallback = prev;
                match pos {
                    Position::Left => fallback.x -= 1.0,
                    Position::Right => fallback.x += 1.0,
                    Position::Top => fallback.y -= 1.0,
                    Position::Bottom => fallback.y += 1.0,
                }
                fallback
            });
        log::trace!("Resetting cursor position to: {}, {}", warp.x, warp.y);
        CGDisplay::warp_mouse_cursor_position(warp).map_err(CaptureError::WarpCursor)
    }

    fn hide_cursor(&self) -> Result<(), CaptureError> {
        CGDisplay::hide_cursor(&CGDisplay::main()).map_err(CaptureError::CoreGraphics)
    }

    fn show_cursor(&self) -> Result<(), CaptureError> {
        CGDisplay::show_cursor(&CGDisplay::main()).map_err(CaptureError::CoreGraphics)
    }

    async fn handle_producer_event(
        &mut self,
        producer_event: ProducerEvent,
    ) -> Result<Option<(BarrierKey, CaptureEvent)>, CaptureError> {
        log::debug!("handling event: {producer_event:?}");
        match producer_event {
            ProducerEvent::Release => {
                if self.current_key.is_some() {
                    self.show_cursor()?;
                    self.current_key = None;
                }
                // Release must also clear any pending state (kept in
                // sync with the Windows semantics).
                if self.pending_key.is_some() {
                    self.pending_key = None;
                }
                Ok(None)
            }
            ProducerEvent::Grab(key) => {
                // Legacy path. New code no longer emits Grab; kept for
                // compatibility.
                if self.current_key.is_none() {
                    self.hide_cursor()?;
                    self.current_key = Some(key);
                }
                Ok(None)
            }
            ProducerEvent::StartCapture(key) => {
                // Pending -> Active promotion. Only runs when
                // pending_key matches (the user crossing a different
                // edge, or having already cancelled, becomes a no-op).
                if self.pending_key.as_ref() != Some(&key) {
                    log::trace!(
                        "StartCapture({key:?}) ignored: pending_key={:?}",
                        self.pending_key
                    );
                    return Ok(None);
                }
                self.pending_key = None;
                if self.current_key.is_none() {
                    self.hide_cursor()?;
                    self.current_key = Some(key.clone());
                    // Use enter_position (recorded earlier in the tap
                    // callback) to warp the cursor 1px inside the edge,
                    // matching the behavior of the legacy Grab path.
                    self.reset_cursor(key.pos)?;
                    // Notify the main thread: capture is now active.
                    return Ok(Some((key, CaptureEvent::Begin)));
                }
                Ok(None)
            }
            ProducerEvent::CancelPending(key) => {
                if self.pending_key.as_ref() == Some(&key) {
                    self.pending_key = None;
                    return Ok(Some((key, CaptureEvent::CancelPending)));
                }
                Ok(None)
            }
            ProducerEvent::Create(k) => {
                self.active_clients.insert(k);
                Ok(None)
            }
            ProducerEvent::Destroy(k) => {
                if let Some(current) = self.current_key.as_ref() {
                    if current == &k {
                        self.show_cursor()?;
                        self.current_key = None;
                    };
                }
                if self.pending_key.as_ref() == Some(&k) {
                    self.pending_key = None;
                }
                self.active_clients.remove(&k);
                Ok(None)
            }
            ProducerEvent::EventTapDisabled => {
                // Tap death can happen mid-capture (TCC Accessibility
                // revoked, tap-timeout, etc). Release state so we
                // don't leave the cursor hidden even if the outer
                // task only logs this error rather than propagating.
                if self.current_key.is_some() {
                    self.show_cursor()?;
                    self.current_key = None;
                }
                if self.pending_key.is_some() {
                    self.pending_key = None;
                }
                Err(CaptureError::EventTapDisabled)
            }
            ProducerEvent::DisplayReconfigured => {
                // The macOS display configuration changed — a monitor
                // was plugged in/out, the resolution changed, the
                // arrangement was rearranged, etc. Re-fetch the
                // active-display list so barrier crossings and the
                // cursor-warp on capture-start use the current
                // geometry instead of whatever was true at process
                // start.
                //
                // **STEP-M2-2.7 HOT-PLUG FIX**: refresh `self.displays`
                // for barrier tracking, but NEVER gate the
                // `monitors_tx` push on its outcome. Quartz can
                // transiently fail `CGDisplay::active_displays()` mid-
                // reconfiguration (macOS returns kIOReturnBusy / a
                // transient CoreGraphics error); if we skipped the
                // push in that case the GUI's `state.monitors` would
                // stay stale until the *next* change, and the M3
                // monitor dropdown would appear to "freeze" on the
                // old list — exactly the failure mode the macOS
                // dual-display hot-plug manual test surfaced.
                //
                // `enumerate_monitors` re-queries
                // `CGDisplay::active_displays()` independently of
                // `update_bounds`, so a transient bounds-refresh
                // failure does NOT prevent the GUI-facing snapshot
                // from being updated.
                match self.update_bounds() {
                    Ok(()) => {
                        log::info!("display reconfigured: {} display(s)", self.displays.len());
                        for d in &self.displays {
                            log::info!("  display bounds: {d:?}");
                        }
                    }
                    Err(e) => {
                        // `self.displays` was cleared at the start of
                        // `update_bounds` before the failure; until
                        // the next successful refresh barrier
                        // tracking will fall back to the per-display
                        // lookup in `compute_edge_point`, which
                        // returns None for any cursor position not
                        // on a known display. That's acceptable as a
                        // transient degradation — the GUI still
                        // gets a fresh monitor list below.
                        log::warn!(
                            "display reconfigured: refresh bounds failed ({e}); \
                             barrier tracking may be stale until next refresh"
                        );
                    }
                }
                // M2 STEP-2.2: re-enumerate monitors so subscribers
                // (the STEP-2.6 service layer) see the new list.
                // Always run, even when the bounds refresh above
                // failed — see the long-form comment above.
                let monitors = enumerate_monitors(&self.displays);
                log::info!("monitors changed: {} monitor(s)", monitors.len());
                for m in &monitors {
                    log::info!(
                        "  monitor: id={} name={:?} pos={:?} size={:?} primary={} scale={}",
                        m.id,
                        m.name,
                        m.position,
                        m.size,
                        m.primary,
                        m.scale
                    );
                }
                let _ = self.monitors_tx.send(monitors);
                Ok(None)
            }
            ProducerEvent::MonitorsChanged(monitors) => {
                // Reserved path for STEP-2.6+ to push a manually
                // refreshed list. The variant exists to keep the
                // producer-event surface uniform; today the
                // reconfiguration path updates `monitors_tx` directly
                // (see the DisplayReconfigured arm above).
                log::info!("monitors changed (manual): {} monitor(s)", monitors.len());
                let _ = self.monitors_tx.send(monitors);
                Ok(None)
            }
        }
    }
}

// ===== Monitor enumeration helpers (STEP-2.2) =====
//
// `enumerate_monitors` walks `CGDisplay::active_displays()` and
// resolves per-display info via IOKit (vendor / model / serial /
// location / preferred name). The IOKit-touching parts live in
// `read_display_info`; everything else is plain data manipulation so
// the per-monitor helper can be exercised without an IOKit query.
//
// The plan calls out two invariants for the stable `MonitorInfo::id`:
//   1. Same physical monitor plugged into the same port → same id.
//   2. Replugging into a different port → different id (port change
//      typically changes the IODisplayLocation string).
// We compose `id` from `vendor:product:serial:location` so both rules
// hold — `CGDisplaySerialNumber` alone wouldn't catch case 2.

/// Compose the stable monitor id from the IOKit info fields. Pure,
/// unit-testable. Returns a colon-separated string with hex
/// `vendor`/`product` so the human-readable parts (serial, location)
/// stay grep-able.
fn build_stable_id(vendor: u32, product: u32, serial: &str, location: &str) -> String {
    format!("macos:{vendor:04x}:{product:04x}:{serial}:{location}")
}

/// Compute the macOS `scale` (point→pixel ratio) from the current
/// display mode. Pure, unit-testable. A `point_width == 0` would
/// indicate a degenerate display and is treated as 1.0 to avoid
/// NaN / Inf propagating into the IPC layer.
fn compute_scale(pixel_width: f64, point_width: f64) -> f64 {
    if point_width > 0.0 && pixel_width > 0.0 {
        pixel_width / point_width
    } else {
        1.0
    }
}

/// Per-display info from IOKit. The raw `vendor`/`product` are
/// unsigned 32-bit IDs reported by the GPU's EDID parser; `serial` is
/// typically a decimal string from the display's EDID; `location` is
/// a free-form string like `"Internal"`, `"External"`, or `"PCI Slot
/// 1"`; `name` is the preferred human label (set when the user has
/// installed a color profile or the OS knows the model).
#[derive(Debug, Default)]
struct DisplayInfo {
    vendor: u32,
    product: u32,
    serial: String,
    location: String,
    name: Option<String>,
}

impl DisplayInfo {
    /// Build a fallback `DisplayInfo` for the case where IOKit could
    /// not be reached. The standard `Default::default()` would produce
    /// empty `serial` / `location`, which combined with `vendor =
    /// 0` / `product = 0` yields a stable id (`"macos:0000:0000:"`)
    /// that is identical across every IOKit-failed display on the
    /// same machine. To keep the stable id unique we splice the
    /// `display_id` into the `location` slot so the final id is at
    /// least display-distinguishable. See `read_display_info`.
    fn unknown(display_id: CGDirectDisplayID) -> Self {
        Self {
            vendor: 0,
            product: 0,
            serial: String::new(),
            location: format!("unknown-{display_id}"),
            name: None,
        }
    }
}

/// Enumerate every active Quartz display and produce a `MonitorInfo`
/// for each. The `displays` parameter is the up-to-date bounds list
/// (kept in sync with `self.displays` inside `update_bounds`); it's
/// currently unused because we re-fetch bounds per display via
/// `CGDisplay::bounds`, but accepting it documents that bounds and
/// the active-id list must agree and gives a future caller a hook to
/// inject test fixtures.
///
/// **M3 STEP-3.4 single-source-of-truth refactor**: this function
/// used to walk `CGDisplay::active_displays + bounds()` AND
/// `IODisplayCreateInfoDictionary` on its own. The IOKit-touching
/// work has been extracted into [`enumerate_monitors_for_ids`] so
/// both `enumerate_monitors` (used by the public
/// `Capture::monitors()` API) and `update_bounds` share the same
/// Quartz → IOKit walk. The bounds rectangle is then derived from
/// the resulting `MonitorInfo.position/size` via [`build_display_bounds`]
/// instead of doing a second `CGDisplay::bounds()` call.
#[allow(dead_code)] // accepts `displays` for documentation/future use
fn enumerate_monitors(displays: &[DisplayBound]) -> Vec<MonitorInfo> {
    let _ = displays;
    let Ok(active_ids) = CGDisplay::active_displays() else {
        log::warn!("enumerate_monitors: CGDisplay::active_displays failed");
        return Vec::new();
    };
    enumerate_monitors_for_ids(&active_ids)
}

/// Single-source-of-truth helper: given the active Quartz display
/// IDs, walk each one through IOKit to compose a stable
/// `MonitorInfo`. Pure(ish) on the FFI boundary — every per-display
/// field extraction is plain data manipulation once the OS handles
/// are in hand. `update_bounds` and the public
/// `Capture::monitors()` both go through this helper so a transient
/// Quartz state during hot-plug produces a single coherent list.
fn enumerate_monitors_for_ids(active_ids: &[CGDirectDisplayID]) -> Vec<MonitorInfo> {
    let main = unsafe { CGMainDisplayID() };

    let mut out = Vec::with_capacity(active_ids.len());
    for &display_id in active_ids {
        let display = CGDisplay::new(display_id);
        let bounds = display.bounds();
        let position = (bounds.origin.x as i32, bounds.origin.y as i32);
        let size = (bounds.size.width as u32, bounds.size.height as u32);
        let primary = display_id == main;

        // scale = pixels / points. Built-in Retina returns 2.0;
        // external 1080p returns 1.0. Fallback to 1.0 when the
        // display mode is unavailable (transient state mid-reconfigure).
        let scale = display
            .display_mode()
            .map(|mode| compute_scale(mode.pixel_width() as f64, bounds.size.width))
            .unwrap_or(1.0);

        // IOKit info dictionary — best-effort. A failure here
        // degrades to the synthetic "Display <id>" name; the
        // `read_display_info` helper bakes `display_id` into the
        // fallback `location` so the resulting stable id remains
        // unique across displays even when IOKit is unavailable
        // (TCC denied, transient state). See STEP-M2-2.2-FIXUP for the
        // id-collision regression test.
        let info = unsafe { read_display_info(display_id) };
        let id = build_stable_id(info.vendor, info.product, &info.serial, &info.location);
        let name = info
            .name
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("Display {display_id}"));

        out.push(MonitorInfo {
            id,
            name,
            position,
            size,
            primary,
            scale,
        });
    }
    out
}

/// Build the per-backend `Vec<DisplayBound>` from the active Quartz
/// IDs and the matching `Vec<MonitorInfo>`. Pure (the IOKit work
/// has already happened inside `enumerate_monitors_for_ids`), so
/// unit tests can exercise it with hand-built fixtures — no FFI.
///
/// The two lists are joined by index on the assumption that
/// `enumerate_monitors_for_ids` iterates `active_ids` in the same
/// order it received them. The Quartz API guarantees this on
/// success; the test pins the invariant.
///
/// `active_ids` is currently unused inside the function body but
/// is kept in the signature so the caller documents the
/// one-display-per-id contract and so a future length-mismatch
/// assertion can slot in here without changing the signature.
fn build_display_bounds(
    active_ids: &[CGDirectDisplayID],
    monitors: &[MonitorInfo],
) -> Vec<DisplayBound> {
    let _ = active_ids;
    monitors
        .iter()
        .map(|m| {
            DisplayBound::new(
                DisplayRect::new(
                    m.position.0 as f64,
                    m.position.1 as f64,
                    m.size.0 as f64,
                    m.size.1 as f64,
                ),
                Some(m.id.clone()),
            )
        })
        .collect()
}

/// Resolve the IOKit service port for `display_id` and read the
/// standard set of display descriptors (vendor / product / serial /
/// location / name). All fields fall back to safe defaults on
/// failure so the caller always gets a populated `DisplayInfo`.
///
/// SAFETY: calls into IOKit / CoreFoundation. The `CFDictionary`
/// returned by `IODisplayCreateInfoDictionary` is retained via
/// `CFDictionary::wrap_under_get_rule`; release happens on drop.
/// Resolve the IOKit service port for `display_id` and read the
/// standard set of display descriptors (vendor / product / serial /
/// location / name). All fields fall back to safe defaults on
/// failure so the caller always gets a populated `DisplayInfo`.
///
/// **Id-collision guard**: when IOKit cannot be reached (TCC denied,
/// transient state, etc.) we cannot recover the OS-reported vendor /
/// product / serial / location. The default-zeroed values would
/// produce a stable id like `"macos:0000:0000:"` that is identical
/// across displays — a violation of PLAN §M2 STEP-2.2's
/// stable-id uniqueness invariant. To keep the id unique we bake
/// `display_id` into the fallback `location` as `"unknown-{display_id}"`,
/// so the resulting id is always distinguishable per display.
/// The wire shape on the happy path is unchanged.
///
/// SAFETY: calls into IOKit / CoreFoundation. The `CFDictionary`
/// returned by `IODisplayCreateInfoDictionary` is retained via
/// `CFDictionary::wrap_under_get_rule`; release happens on drop.
unsafe fn read_display_info(display_id: CGDirectDisplayID) -> DisplayInfo {
    let service = CGDisplayIOServicePort(display_id);
    if service == 0 {
        log::debug!("CGDisplayIOServicePort({display_id}) returned 0; no IOKit info");
        return DisplayInfo::unknown(display_id);
    }

    let dict_ref = IODisplayCreateInfoDictionary(service, K_IO_DISPLAY_ONLY_PREFERRED_NAME);
    // Release the service port immediately — IODisplayCreateInfoDictionary
    // takes its own reference on the data it needs.
    let _ = IOObjectRelease(service);

    if dict_ref.is_null() {
        log::debug!("IODisplayCreateInfoDictionary returned null for display {display_id}");
        return DisplayInfo::unknown(display_id);
    }

    // core_foundation's CFDictionary<K, V> defaults to
    // `*const c_void` for both; we cast our CFString pointers to
    // `*const c_void` keys and read back `*const c_void` values.
    let dict: CFDictionary = unsafe { CFDictionary::wrap_under_get_rule(dict_ref as *const _) };

    let vendor_key = CFString::from_static_string("DisplayVendorID");
    let product_key = CFString::from_static_string("DisplayProductID");
    let serial_key = CFString::from_static_string("DisplaySerialNumber");
    let location_key = CFString::from_static_string("IODisplayLocation");
    let name_key = CFString::from_static_string("DisplayProductName");

    DisplayInfo {
        vendor: dict_find_i64(&dict, vendor_key.as_concrete_TypeRef() as *const c_void)
            .map(|v| v as u32)
            .unwrap_or(0),
        product: dict_find_i64(&dict, product_key.as_concrete_TypeRef() as *const c_void)
            .map(|v| v as u32)
            .unwrap_or(0),
        serial: dict_find_string(&dict, serial_key.as_concrete_TypeRef() as *const c_void)
            .unwrap_or_else(|| "0".to_string()),
        location: dict_find_string(&dict, location_key.as_concrete_TypeRef() as *const c_void)
            .unwrap_or_else(|| "Unknown".to_string()),
        name: dict_find_string(&dict, name_key.as_concrete_TypeRef() as *const c_void),
    }
}

/// Look up `key` in `dict` and try to interpret the value as an
/// `i64`. Returns `None` when the key is absent or the value is not
/// a `CFNumber`.
///
/// SAFETY: `key` must be a valid `CFTypeRef` (e.g. a CFStringRef
/// produced by `CFString::from_static_string`); the returned value
/// reference is borrowed from `dict` and only valid for `'a`.
unsafe fn dict_find_i64(dict: &CFDictionary, key: *const c_void) -> Option<i64> {
    let item_ref = dict.find(key)?;
    let value_ptr: *const c_void = *item_ref;
    if value_ptr.is_null() {
        return None;
    }
    let num = CFNumber::wrap_under_get_rule(value_ptr as CFNumberRef);
    num.to_i64()
}

/// Look up `key` in `dict` and try to interpret the value as a
/// `CFString`. Returns `None` when the key is absent or the value is
/// not a `CFString`.
///
/// SAFETY: same caveat as `dict_find_i64` — the value pointer is
/// only valid for the lifetime of the borrow from `dict`.
unsafe fn dict_find_string(dict: &CFDictionary, key: *const c_void) -> Option<String> {
    let item_ref = dict.find(key)?;
    let value_ptr: *const c_void = *item_ref;
    if value_ptr.is_null() {
        return None;
    }
    let cfstr = CFString::wrap_under_get_rule(value_ptr as CFStringRef);
    Some(cfstr.to_string())
}

// ===== /Monitor enumeration helpers =====

fn get_events(
    ev_type: &CGEventType,
    ev: &CGEvent,
    result: &mut Vec<CaptureEvent>,
    modifier_state: &mut XMods,
) -> Result<(), CaptureError> {
    fn map_pointer_event(ev: &CGEvent) -> PointerEvent {
        PointerEvent::Motion {
            time: 0,
            dx: ev.get_double_value_field(EventField::MOUSE_EVENT_DELTA_X),
            dy: ev.get_double_value_field(EventField::MOUSE_EVENT_DELTA_Y),
        }
    }

    fn map_key(ev: &CGEvent) -> Result<u32, CaptureError> {
        let code = ev.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
        match KeyMap::from_key_mapping(KeyMapping::Mac(code as u16)) {
            Ok(k) => Ok(k.evdev as u32),
            Err(()) => Err(CaptureError::KeyMapError(code)),
        }
    }

    match ev_type {
        CGEventType::KeyDown => {
            let k = map_key(ev)?;
            result.push(CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                time: 0,
                key: k,
                state: 1,
            })));
        }
        CGEventType::KeyUp => {
            let k = map_key(ev)?;
            result.push(CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                time: 0,
                key: k,
                state: 0,
            })));
        }
        CGEventType::FlagsChanged => {
            let mut depressed = XMods::empty();
            let mut mods_locked = XMods::empty();
            let cg_flags = ev.get_flags();

            if cg_flags.contains(CGEventFlags::CGEventFlagShift) {
                depressed |= XMods::ShiftMask;
            }
            if cg_flags.contains(CGEventFlags::CGEventFlagControl) {
                depressed |= XMods::ControlMask;
            }
            if cg_flags.contains(CGEventFlags::CGEventFlagAlternate) {
                depressed |= XMods::Mod1Mask;
            }
            if cg_flags.contains(CGEventFlags::CGEventFlagCommand) {
                depressed |= XMods::Mod4Mask;
            }
            if cg_flags.contains(CGEventFlags::CGEventFlagAlphaShift) {
                depressed |= XMods::LockMask;
                mods_locked |= XMods::LockMask;
            }

            // check if pressed or released
            let state = if depressed > *modifier_state { 1 } else { 0 };
            *modifier_state = depressed;

            if let Ok(key) = map_key(ev) {
                let key_event = CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                    time: 0,
                    key,
                    state,
                }));
                result.push(key_event);
            }

            let modifier_event = KeyboardEvent::Modifiers {
                depressed: depressed.bits(),
                latched: 0,
                locked: mods_locked.bits(),
                group: 0,
            };

            result.push(CaptureEvent::Input(Event::Keyboard(modifier_event)));
        }
        CGEventType::MouseMoved => {
            result.push(CaptureEvent::Input(Event::Pointer(map_pointer_event(ev))))
        }
        CGEventType::LeftMouseDragged => {
            result.push(CaptureEvent::Input(Event::Pointer(map_pointer_event(ev))))
        }
        CGEventType::RightMouseDragged => {
            result.push(CaptureEvent::Input(Event::Pointer(map_pointer_event(ev))))
        }
        CGEventType::OtherMouseDragged => {
            result.push(CaptureEvent::Input(Event::Pointer(map_pointer_event(ev))))
        }
        CGEventType::LeftMouseDown => {
            result.push(CaptureEvent::Input(Event::Pointer(PointerEvent::Button {
                time: 0,
                button: BTN_LEFT,
                state: 1,
            })))
        }
        CGEventType::LeftMouseUp => {
            result.push(CaptureEvent::Input(Event::Pointer(PointerEvent::Button {
                time: 0,
                button: BTN_LEFT,
                state: 0,
            })))
        }
        CGEventType::RightMouseDown => {
            result.push(CaptureEvent::Input(Event::Pointer(PointerEvent::Button {
                time: 0,
                button: BTN_RIGHT,
                state: 1,
            })))
        }
        CGEventType::RightMouseUp => {
            result.push(CaptureEvent::Input(Event::Pointer(PointerEvent::Button {
                time: 0,
                button: BTN_RIGHT,
                state: 0,
            })))
        }
        CGEventType::OtherMouseDown => {
            let btn_num = ev.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER);
            let button = match btn_num {
                3 => BTN_BACK,
                4 => BTN_FORWARD,
                _ => BTN_MIDDLE,
            };
            result.push(CaptureEvent::Input(Event::Pointer(PointerEvent::Button {
                time: 0,
                button,
                state: 1,
            })))
        }
        CGEventType::OtherMouseUp => {
            let btn_num = ev.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER);
            let button = match btn_num {
                3 => BTN_BACK,
                4 => BTN_FORWARD,
                _ => BTN_MIDDLE,
            };
            result.push(CaptureEvent::Input(Event::Pointer(PointerEvent::Button {
                time: 0,
                button,
                state: 0,
            })))
        }
        CGEventType::ScrollWheel => {
            // macOS CGEvent axis 1 positive = content moves up. Negate the
            // vertical axis so the wire value matches the Windows/Linux
            // convention (positive axis 0 = content moves down). The
            // horizontal axis already matches across all three platforms
            // (positive axis 1 = right), so it is passed through unchanged.
            if ev.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_IS_CONTINUOUS) != 0 {
                let v =
                    ev.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_1);
                let h =
                    ev.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_2);
                if v != 0 {
                    result.push(CaptureEvent::Input(Event::Pointer(PointerEvent::Axis {
                        time: 0,
                        axis: 0, // Vertical
                        value: -(v as f64),
                    })));
                }
                if h != 0 {
                    result.push(CaptureEvent::Input(Event::Pointer(PointerEvent::Axis {
                        time: 0,
                        axis: 1, // Horizontal
                        value: h as f64,
                    })));
                }
            } else {
                // line based scrolling
                const LINES_PER_STEP: i32 = 3;
                const V120_STEPS_PER_LINE: i32 = 120 / LINES_PER_STEP;
                let v = ev.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1);
                let h = ev.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2);
                if v != 0 {
                    result.push(CaptureEvent::Input(Event::Pointer(
                        PointerEvent::AxisDiscrete120 {
                            axis: 0, // Vertical
                            value: -(V120_STEPS_PER_LINE * v as i32),
                        },
                    )));
                }
                if h != 0 {
                    result.push(CaptureEvent::Input(Event::Pointer(
                        PointerEvent::AxisDiscrete120 {
                            axis: 1, // Horizontal
                            value: V120_STEPS_PER_LINE * h as i32,
                        },
                    )));
                }
            }
        }
        _ => (),
    }
    Ok(())
}

fn create_event_tap<'a>(
    client_state: Arc<Mutex<InputCaptureState>>,
    notify_tx: Sender<ProducerEvent>,
    event_tx: Sender<(BarrierKey, CaptureEvent)>,
) -> Result<CGEventTap<'a>, MacosCaptureCreationError> {
    // Shared slot for the tap's mach port pointer. Stored as `usize`
    // because raw pointers aren't `Send`, but the integer
    // representation is — and CGEventTapEnable is documented as
    // thread-safe. Set immediately after CGEventTap::new returns;
    // read by the callback to recover from a TapDisabledByTimeout.
    let tap_mach_port: Arc<OnceLock<usize>> = Arc::new(OnceLock::new());
    let tap_mach_port_cb = Arc::clone(&tap_mach_port);

    let cg_events_of_interest: Vec<CGEventType> = vec![
        CGEventType::LeftMouseDown,
        CGEventType::LeftMouseUp,
        CGEventType::RightMouseDown,
        CGEventType::RightMouseUp,
        CGEventType::OtherMouseDown,
        CGEventType::OtherMouseUp,
        CGEventType::MouseMoved,
        CGEventType::LeftMouseDragged,
        CGEventType::RightMouseDragged,
        CGEventType::OtherMouseDragged,
        CGEventType::ScrollWheel,
        CGEventType::KeyDown,
        CGEventType::KeyUp,
        CGEventType::FlagsChanged,
    ];

    let event_tap_callback = move |_proxy: CGEventTapProxy,
                                   event_type: CGEventType,
                                   cg_ev: &CGEvent| {
        log::trace!("Got event from tap: {event_type:?}");
        let mut state = client_state.blocking_lock();
        let mut capture_position = None;
        let mut res_events = vec![];

        if matches!(event_type, CGEventType::TapDisabledByTimeout) {
            // The kernel disables the tap when our callback runs
            // longer than ~1s on a single event — typical causes
            // are heavy load, scheduler contention, or this
            // process being briefly suspended (e.g. App Nap on a
            // long idle). It is NOT a fatal condition: Apple's
            // documented recovery is to call CGEventTapEnable
            // and resume processing. Re-enable in place and KEEP
            // existing capture state so the user doesn't see the
            // cursor pop back to the local screen mid-session.
            if let Some(&port) = tap_mach_port_cb.get() {
                log::warn!("CGEventTap disabled by timeout — re-enabling");
                unsafe {
                    CGEventTapEnable(port as *mut c_void, true);
                }
            } else {
                log::error!(
                    "CGEventTap disabled by timeout, but mach port not yet stored — cannot re-enable"
                );
            }
            return CallbackResult::Keep;
        }

        if matches!(event_type, CGEventType::TapDisabledByUserInput) {
            // Deliberate kill — secure-input mode (e.g. password
            // field), TCC Accessibility revoked mid-session, or
            // the user disabling event-monitoring. We can't
            // recover from this; drop captured state synchronously
            // and return Keep on this event. Otherwise the
            // `current_key.is_some()` branch below would drop this
            // event (and any racing callback still in flight) back
            // into `CallbackResult::Drop`, silently eating the
            // user's clicks and keypresses while the tap winds
            // down. Clear state + show the cursor here, then
            // notify the producer loop so the service can tear
            // down cleanly.
            log::error!("CGEventTap disabled by user input, releasing capture state");
            if state.current_key.is_some() {
                let _ = CGDisplay::show_cursor(&CGDisplay::main());
                state.current_key = None;
            }
            notify_tx
                .blocking_send(ProducerEvent::EventTapDisabled)
                .unwrap_or_else(|e| {
                    log::error!("Failed to send notification: {e}");
                });
            return CallbackResult::Keep;
        }

        // Are we in a client?
        if let Some(current_key) = state.current_key.clone() {
            capture_position = Some(current_key.clone());
            get_events(
                &event_type,
                cg_ev,
                &mut res_events,
                &mut state.modifier_state,
            )
            .unwrap_or_else(|e| {
                log::error!("Failed to get events: {e}");
            });

            // Keep (hidden) cursor at the edge of the screen
            if matches!(
                event_type,
                CGEventType::MouseMoved
                    | CGEventType::LeftMouseDragged
                    | CGEventType::RightMouseDragged
                    | CGEventType::OtherMouseDragged
            ) {
                state
                    .reset_cursor(current_key.pos)
                    .unwrap_or_else(|e| log::warn!("{e}"));
            }
        } else if matches!(event_type, CGEventType::MouseMoved) {
            // Cursor pre/post samples for barrier detection.
            //
            // CRITICAL, and different from the Windows backend: macOS
            // *clamps* the cursor to the display union, so
            // `cg_ev.location()` can never report a point outside a
            // display. Pushing the mouse further left at the left edge
            // keeps returning x == display.left() while only the delta
            // field grows. Windows' WH_MOUSE_LL reports the raw,
            // unclamped point, which is why `entered_barrier` there can
            // just use the hook's `pt` as `curr_pos`.
            //
            // `entered_barrier` requires `curr_pos` to be *outside* the
            // union, so on macOS `curr_pos` must be the **predicted**
            // position (location + delta), with `location` itself as
            // `prev_pos`. Deriving `prev_pos` as `location - delta`
            // instead leaves both samples clamped inside the union, so
            // no crossing is ever detected and the cursor can never
            // enter a remote client.
            let location = cg_ev.location();
            let relative_x = cg_ev.get_double_value_field(EventField::MOUSE_EVENT_DELTA_X);
            let relative_y = cg_ev.get_double_value_field(EventField::MOUSE_EVENT_DELTA_Y);
            let prev_pos = (location.x, location.y);
            let curr_pos = (location.x + relative_x, location.y + relative_y);

            // Pending-capture intermediate state: the mouse has crossed
            // an edge but the Ack has not arrived yet. Three cases:
            // 1. User pulled back inside the screen -> cancel pending.
            // 2. User crossed a different edge -> switch pending and
            //    re-record enter_position.
            // 3. Still on the same edge -> do nothing (pending stands).
            if let Some(pending_key) = state.pending_key.clone() {
                let crossed = state.crossed(prev_pos, curr_pos);
                match crossed {
                    None => {
                        // User pulled back -> cancel pending and emit
                        // CancelPending.
                        log::debug!("CANCEL pending {pending_key:?} (cursor pulled back)");
                        state.pending_key = None;
                        // Send CancelPending immediately (with the
                        // correct pending_key). Bypass the unified
                        // res_events batch because other cases below
                        // may still need to run on this same callback.
                        let _ = event_tx.blocking_send((pending_key, CaptureEvent::CancelPending));
                    }
                    Some(other_key) if other_key != pending_key => {
                        // Switched edge: cancel the old pending first,
                        // then start a new pending.
                        log::debug!("switch pending: {pending_key:?} -> {other_key:?}");
                        // Send CancelPending immediately (with the old
                        // position). Same reasoning as above: send it
                        // out-of-band rather than batching.
                        let _ = event_tx.blocking_send((pending_key, CaptureEvent::CancelPending));
                        state.pending_key = Some(other_key.clone());
                        // STEP 0.5: enter_position records the actual
                        // cursor sample from the moment of barrier
                        // crossing (prev_pos — clamped to the union by
                        // the macOS event tap). `reset_cursor` then
                        // looks up the per-display warp target via
                        // `compute_edge_point` on promotion. Storing
                        // prev_pos (rather than the warp target itself)
                        // keeps a single source of truth: if the
                        // display list changes between capture and
                        // promotion, `compute_edge_point` is re-run with
                        // the up-to-date displays on the actual active
                        // position, instead of trusting a warp point
                        // recorded against stale geometry.
                        state.enter_position = Some(CGPoint {
                            x: prev_pos.0,
                            y: prev_pos.1,
                        });
                        // The new pending goes through the unified
                        // batch (with the new position other_pos).
                        capture_position = Some(other_key);
                        res_events.push(CaptureEvent::BeginPending);
                    }
                    Some(_) => {
                        // Still on the same edge: do nothing. Subsequent
                        // moves on the same edge must not re-emit
                        // BeginPending (which would cause the main
                        // thread to handle the same pending twice).
                    }
                }
            } else if let Some(new_key) = state.crossed(prev_pos, curr_pos) {
                // Brand-new edge crossing -> enter pending. Do NOT
                // warp the cursor, do NOT hide the cursor; only record
                // enter_position for use during the promotion phase.
                log::debug!("PENDING enter {new_key:?}");
                capture_position = Some(new_key.clone());
                state.pending_key = Some(new_key);
                // STEP 0.5: enter_position records the actual
                // pre-crossing cursor sample. See the switch-edge arm
                // above for why this stays a CGPoint rather than a
                // pre-computed warp target.
                state.enter_position = Some(CGPoint {
                    x: prev_pos.0,
                    y: prev_pos.1,
                });
                res_events.push(CaptureEvent::BeginPending);
                // Intentionally NOT calling state.start_capture, and
                // intentionally NOT sending ProducerEvent::Grab —
                // promotion happens after the main thread Acks via the
                // start_capture path.
            }
        }

        if let Some(key) = capture_position {
            res_events.iter().for_each(|e| {
                // error must be ignored, since the event channel
                // may already be closed when the InputCapture instance is dropped.
                let _ = event_tx.blocking_send((key.clone(), *e));
            });
            // Returning Drop should stop the event from being processed
            // but core foundation still returns the event
            cg_ev.set_type(CGEventType::Null);
            CallbackResult::Drop
        } else {
            CallbackResult::Keep
        }
    };

    let tap = CGEventTap::new(
        CGEventTapLocation::Session,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::Default,
        cg_events_of_interest,
        event_tap_callback,
    )
    .map_err(|_| MacosCaptureCreationError::EventTapCreation)?;

    // Hand the mach port pointer to the callback so it can re-enable
    // the tap on TapDisabledByTimeout. The pointer is valid for the
    // lifetime of `tap` (which lives on the event-tap thread until
    // the run loop exits).
    let port_ptr = tap.mach_port().as_concrete_TypeRef() as usize;
    let _ = tap_mach_port.set(port_ptr);

    let tap_source: CFRunLoopSource = tap
        .mach_port()
        .create_runloop_source(0)
        .expect("Failed creating loop source");

    unsafe {
        CFRunLoop::get_current().add_source(&tap_source, kCFRunLoopCommonModes);
    }

    Ok(tap)
}

fn event_tap_thread(
    client_state: Arc<Mutex<InputCaptureState>>,
    event_tx: Sender<(BarrierKey, CaptureEvent)>,
    notify_tx: Sender<ProducerEvent>,
    ready: std::sync::mpsc::Sender<Result<CFRunLoop, MacosCaptureCreationError>>,
    exit: oneshot::Sender<()>,
) {
    // Clone now: create_event_tap consumes notify_tx into its closure.
    let display_notify_tx = notify_tx.clone();

    let _tap = match create_event_tap(client_state, notify_tx, event_tx) {
        Err(e) => {
            ready.send(Err(e)).expect("channel closed");
            return;
        }
        Ok(tap) => {
            let run_loop = CFRunLoop::get_current();
            ready.send(Ok(run_loop)).expect("channel closed");
            tap
        }
    };

    // Register a Quartz display-reconfiguration callback so the
    // capture state's bounds get refreshed when the user plugs in a
    // monitor, changes resolution, or rearranges displays. The
    // callback runs on this thread's CFRunLoop. Box-leak the sender
    // so the C side has a stable user_info pointer; reclaim it after
    // the run loop exits.
    //
    // **STEP-M2-2.7 DIAGNOSTIC**: log the CGError return value so
    // any registration failure surfaces in the daemon log instead
    // of silently dropping every hot-plug event.
    let display_user_info = Box::into_raw(Box::new(display_notify_tx)) as *mut c_void;
    let reg_err = unsafe {
        CGDisplayRegisterReconfigurationCallback(
            display_reconfiguration_callback,
            display_user_info,
        )
    };
    if reg_err != 0 {
        log::warn!(
            "CGDisplayRegisterReconfigurationCallback returned non-zero CGError={reg_err}; \
             hot-plug detection will NOT work"
        );
    } else {
        log::info!("registered CGDisplay reconfiguration callback on tap thread run loop");
    }

    log::debug!("running CFRunLoop...");
    CFRunLoop::run_current();
    log::debug!("event tap thread exiting!...");

    unsafe {
        CGDisplayRemoveReconfigurationCallback(display_reconfiguration_callback, display_user_info);
        // Reclaim the leaked sender Box so we don't leak a tokio
        // channel sender on every capture create/destroy cycle.
        drop(Box::from_raw(
            display_user_info as *mut Sender<ProducerEvent>,
        ));
    }

    let _ = exit.send(());
}

/// Quartz display-reconfiguration callback. Fires twice per change:
/// once with `kCGDisplayBeginConfigurationFlag` set (BEFORE the
/// change is applied — the bounds are still stale at this point),
/// then again afterwards with the actual change flags (Add, Remove,
/// Mode, DesktopShapeChanged, etc.). Skip the begin phase; on the
/// real notification, kick the producer task to refresh bounds.
extern "C" fn display_reconfiguration_callback(_display: u32, flags: u32, user_info: *mut c_void) {
    // **STEP-M2-2.7 HOT-PLUG DIAGNOSTIC**: log every fire of the
    // Quartz reconfiguration callback so the macOS dual-display
    // manual test can confirm whether the chain breaks at (a) macOS
    // itself not delivering the notification, (b) the run loop
    // never servicing it, or (c) the channel send failing. If you
    // see "reconfiguration begin" lines but no "reconfiguration
    // change" lines on hot-plug, the bug is upstream of us — likely
    // the daemon binary isn't running or TCC is blocking. If you
    // see "change" lines but never "DisplayReconfigured received"
    // further down, the producer task isn't draining the channel.
    if flags & (1 << 0) != 0 {
        log::debug!("display reconfiguration: begin flag set, skipping");
        return;
    }
    log::debug!("display reconfiguration: change fired, flags=0x{flags:x} display={_display}");
    if user_info.is_null() {
        log::warn!("display reconfiguration: user_info is null, ignoring");
        return;
    }
    // SAFETY: user_info is a Box::into_raw of Sender<ProducerEvent>
    // owned by `event_tap_thread`. It's valid for the lifetime of
    // that thread; the registration is removed before the box is
    // freed. The callback only fires while the run loop is running
    // on that thread, so we know the box is live here.
    let sender = unsafe { &*(user_info as *const Sender<ProducerEvent>) };
    match sender.blocking_send(ProducerEvent::DisplayReconfigured) {
        Ok(()) => log::debug!("display reconfiguration: queued DisplayReconfigured"),
        Err(e) => log::warn!("failed to notify display reconfiguration: {e}"),
    }
}

pub struct MacOSInputCapture {
    event_rx: Receiver<(BarrierKey, CaptureEvent)>,
    notify_tx: Sender<ProducerEvent>,
    run_loop: CFRunLoop,
    /// Sender for the latest monitor list. A clone is captured by the
    /// `InputCaptureState` (so the producer task can push updates when
    /// `DisplayReconfigured` fires); this handle is kept here so the
    /// public `monitor_changes()` method can hand out new receivers to
    /// upstream consumers. The field is unused inside this file today —
    /// STEP-2.5 will wire `Capture::monitors()` and STEP-2.6 will use
    /// it from the service layer.
    #[allow(dead_code)]
    monitors_tx: watch::Sender<Vec<MonitorInfo>>,
}

impl MacOSInputCapture {
    pub async fn new() -> Result<Self, MacosCaptureCreationError> {
        request_macos_capture_permissions()?;

        // Create the monitor watch channel up front so the initial
        // state (captured inside `new()`) can be published before the
        // producer task starts.
        let (monitors_tx, _) = watch::channel(Vec::new());
        let state = Arc::new(Mutex::new(InputCaptureState::new(monitors_tx.clone())?));
        let (event_tx, event_rx) = mpsc::channel(32);
        // Pending-capture close the loop: the producer task also
        // forwards events to the main thread (ProducerEvent::StartCapture
        // -> Begin; CancelPending -> CancelPending). Clone event_tx for
        // the producer task — the tap callback and the producer task
        // write concurrently to the same channel, consumed in order by
        // the downstream poll_next. Sender is Send + Sync, so multiple
        // writers are safe.
        let producer_event_tx = event_tx.clone();
        let (notify_tx, mut notify_rx) = mpsc::channel(32);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (tap_exit_tx, mut tap_exit_rx) = oneshot::channel();

        unsafe {
            configure_cf_settings()?;
        }

        log::info!("Enabling CGEvent tap");
        let event_tap_thread_state = state.clone();
        let event_tap_notify = notify_tx.clone();
        thread::spawn(move || {
            event_tap_thread(
                event_tap_thread_state,
                event_tx,
                event_tap_notify,
                ready_tx,
                tap_exit_tx,
            )
        });

        // wait for event tap creation result
        let run_loop = ready_rx.recv().expect("channel closed")?;

        let _tap_task: tokio::task::JoinHandle<()> = tokio::task::spawn_local(async move {
            loop {
                tokio::select! {
                    producer_event = notify_rx.recv() => {
                        let Some(producer_event) = producer_event else {
                            break;
                        };
                        let mut state = state.lock().await;
                        match state.handle_producer_event(producer_event).await {
                            Err(e) => log::error!("Failed to handle producer event: {e}"),
                            Ok(Some((key, ev))) => {
                                // Begin / CancelPending events emitted by the producer go through
                                // the cloned event_tx and are merged into
                                // the same downstream stream as the tap
                                // callback's events.
                                let _ = producer_event_tx.send((key, ev)).await;
                            }
                            Ok(None) => {}
                        }
                    }
                    _ = &mut tap_exit_rx => break,
                }
            }
            // show cursor
            let _ = CGDisplay::show_cursor(&CGDisplay::main());
        });

        Ok(Self {
            event_rx,
            notify_tx,
            run_loop,
            monitors_tx,
        })
    }

    /// Subscribe to the latest monitor list. Each call returns a new
    /// receiver that sees every future update (a new entry is published
    /// on every `DisplayReconfigured` and once during construction).
    /// The initial value is the list captured at startup.
    ///
    /// STEP-2.6 service layer holds the receiver and forwards
    /// `MonitorsChanged` events to the IPC frontend.
    #[allow(dead_code)] // STEP-2.5/2.6 will consume this
    pub fn monitor_changes(&self) -> watch::Receiver<Vec<MonitorInfo>> {
        self.monitors_tx.subscribe()
    }

    /// Snapshot of the most recent monitor list, captured without
    /// touching the watch channel. Used by STEP-2.5's
    /// `Capture::monitors()` impl when polling is acceptable and the
    /// caller does not need a subscription.
    #[allow(dead_code)] // STEP-2.5/2.6 will consume this
    pub fn current_monitors(&self) -> Vec<MonitorInfo> {
        self.monitors_tx.borrow().clone()
    }
}

fn request_macos_capture_permissions() -> Result<(), MacosCaptureCreationError> {
    // Call both request functions unconditionally so macOS surfaces both
    // TCC prompts on the very first launch. TCC always returns `false` the
    // first time a permission is requested (the grant only becomes visible
    // on the next process launch), so returning early on the first failure
    // would skip the second prompt and force the user through an extra
    // relaunch just to see it.
    let accessibility = request_accessibility_permission();
    let input_monitoring = request_input_monitoring_permission();

    if !accessibility {
        return Err(MacosCaptureCreationError::AccessibilityPermission);
    }
    if !input_monitoring {
        return Err(MacosCaptureCreationError::InputMonitoringPermission);
    }
    Ok(())
}

fn request_accessibility_permission() -> bool {
    // Silent check. The GUI owns the one-time user-visible prompt at
    // startup (see the web frontend's macOS TCC handling) so retries
    // triggered by clicking the "Reenable" button don't pop a fresh
    // Accessibility alert every time.
    unsafe { AXIsProcessTrusted() }
}

fn request_input_monitoring_permission() -> bool {
    // Silent check, same reasoning as above.
    unsafe { CGPreflightListenEventAccess() }
}

impl Drop for MacOSInputCapture {
    fn drop(&mut self) {
        self.run_loop.stop();
    }
}

#[async_trait]
impl Capture for MacOSInputCapture {
    async fn create(&mut self, key: &BarrierKey) -> Result<(), CaptureError> {
        // M1: forward the full BarrierKey through the producer-event
        // channel. monitor / offset / span stay at their legacy
        // defaults until M2 wires monitor info end-to-end.
        let key = key.clone();
        let notify_tx = self.notify_tx.clone();
        tokio::task::spawn_local(async move {
            log::debug!("creating capture, {key:?}");
            let _ = notify_tx.send(ProducerEvent::Create(key)).await;
            log::debug!("done !");
        });
        Ok(())
    }

    async fn destroy(&mut self, key: &BarrierKey) -> Result<(), CaptureError> {
        let key = key.clone();
        let notify_tx = self.notify_tx.clone();
        tokio::task::spawn_local(async move {
            log::debug!("destroying capture {key:?}");
            let _ = notify_tx.send(ProducerEvent::Destroy(key)).await;
            log::debug!("done !");
        });
        Ok(())
    }

    async fn release(&mut self) -> Result<(), CaptureError> {
        let notify_tx = self.notify_tx.clone();
        tokio::task::spawn_local(async move {
            log::debug!("notifying Release");
            let _ = notify_tx.send(ProducerEvent::Release).await;
        });
        Ok(())
    }

    /// Pending-capture handshake: the main thread calls this after the
    /// remote Ack for Enter to promote the pending capture at `key`
    /// to active. Returns synchronously — the message is dispatched
    /// to the producer task, and hide cursor + warp + emit Begin
    /// complete asynchronously.
    fn start_capture(&mut self, key: &BarrierKey) -> Result<(), CaptureError> {
        let key = key.clone();
        let notify_tx = self.notify_tx.clone();
        tokio::task::spawn_local(async move {
            log::debug!("notifying StartCapture({key:?})");
            let _ = notify_tx.send(ProducerEvent::StartCapture(key)).await;
        });
        Ok(())
    }

    /// Pending-capture handshake: called by the main thread on
    /// `cancel_pending`, network loss, or the 500ms timeout.
    /// Returns synchronously.
    fn cancel_pending(&mut self, key: &BarrierKey) -> Result<(), CaptureError> {
        let key = key.clone();
        let notify_tx = self.notify_tx.clone();
        tokio::task::spawn_local(async move {
            log::debug!("notifying CancelPending({key:?})");
            let _ = notify_tx.send(ProducerEvent::CancelPending(key)).await;
        });
        Ok(())
    }

    async fn terminate(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }

    fn monitors(&self) -> Vec<MonitorInfo> {
        // **STEP-M2-2.7 HOT-PLUG POLL FIX**: bypass the
        // `monitors_tx` watch-channel snapshot and re-query Quartz
        // directly via `enumerate_monitors`. The watch channel is
        // only refreshed by the `CGDisplayReconfiguration` callback
        // (or the startup seed inside `InputCaptureState::new`),
        // and the callback path silently drops every event on
        // macOS hosts where the daemon binary is not TCC-granted
        // accessibility / input-monitoring access — there is no
        // way to surface the failure short of having the user run
        // `RUST_LOG=debug` and stare at the log. That isn't an
        // acceptable failure mode for a hot-plug story the M3
        // dropdown sits on top of.
        //
        // `enumerate_monitors` is a pure read of
        // `CGDisplay::active_displays()` and does NOT need any TCC
        // permission — the 1Hz poll in `src/capture.rs:1014`
        // (`MONITOR_POLL_INTERVAL = 1s`) will pick up the fresh
        // snapshot on every tick, dedup against `self.last_monitors`
        // (in the service), and emit `MonitorsChanged` on a real
        // change. The 1-second latency is fine for the GUI
        // dropdown — the user already sees the new monitor appear
        // in macOS's own Displays panel before our event lands.
        //
        // The watch channel + callback path is still wired up (in
        // `event_tap_thread` and the `DisplayReconfigured` arm of
        // `handle_producer_event`) for any future subscription-
        // based consumer that wants sub-second latency on
        // TCC-granted hosts; the polling path above is the
        // always-on, no-TCC-required baseline.
        enumerate_monitors(&[])
    }
}

impl Stream for MacOSInputCapture {
    type Item = Result<(BarrierKey, CaptureEvent), CaptureError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // M1: the event channel already carries `(BarrierKey, _)`
        // pairs end-to-end. monitor / offset / span stay at their
        // legacy defaults (`None` / `0` / `10000`) until M2 wires
        // monitor info end-to-end.
        match ready!(self.event_rx.poll_recv(cx)) {
            None => Poll::Ready(None),
            Some(e) => Poll::Ready(Some(Ok(e))),
        }
    }
}

type CGSConnectionID = u32;

/// IOKit / IODisplay raw FFI. `IODisplayCreateInfoDictionary` is the
/// stable-monitor-info source called out by PLAN §M2 STEP-2.2 (vendor /
/// model / serial / location used to build a stable `MonitorInfo::id`).
///
/// The IOKit framework is linked here (in addition to
/// `ApplicationServices` above) because that's where the function lives;
/// `CGDisplayIOServicePort` is in ApplicationServices and resolves the
/// IOKit service port that backs a Quartz display.
#[allow(non_camel_case_types)]
type io_service_t = u32;
#[allow(non_camel_case_types)]
type io_object_t = u32;
#[allow(non_camel_case_types)]
type IOOptionBits = u32;

/// `kIODisplayOnlyPreferredName` — passes through to
/// `IODisplayCreateInfoDictionary` so we get the OS-preferred product
/// name (`DisplayProductName`) when available rather than a generic
/// vendor/model fallback.
const K_IO_DISPLAY_ONLY_PREFERRED_NAME: IOOptionBits = 1 << 26;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn CGSSetConnectionProperty(
        cid: CGSConnectionID,
        targetCID: CGSConnectionID,
        key: CFStringRef,
        value: CFBooleanRef,
    ) -> CGError;
    fn _CGSDefaultConnection() -> CGSConnectionID;
    /// Resolve an IOKit service port for the Quartz display.
    /// Returns 0 (an invalid `io_service_t`) on failure; the caller
    /// is expected to bail and treat the display as IOKit-unknown.
    fn CGDisplayIOServicePort(display: CGDirectDisplayID) -> io_service_t;
}

extern "C" {
    fn CGEventSourceSetLocalEventsSuppressionInterval(
        event_source: CGEventSource,
        seconds: CFTimeInterval,
    );
    fn CGPreflightListenEventAccess() -> bool;
    /// Re-enable an event tap that was disabled by a
    /// `kCGEventTapDisabledByTimeout` event. The Apple-documented
    /// recovery path: see Quartz Event Services Reference. The `tap`
    /// argument is a `CFMachPortRef`; we pass the raw pointer so we
    /// can store it as `usize` for cross-thread sharing.
    fn CGEventTapEnable(tap: *mut c_void, enable: bool);

    /// Register a callback invoked when the display configuration
    /// changes (monitor add/remove, resolution change, mirror,
    /// rearrange, etc). See Quartz Display Services Reference.
    fn CGDisplayRegisterReconfigurationCallback(
        callback: extern "C" fn(u32, u32, *mut c_void),
        user_info: *mut c_void,
    ) -> CGError;
    fn CGDisplayRemoveReconfigurationCallback(
        callback: extern "C" fn(u32, u32, *mut c_void),
        user_info: *mut c_void,
    ) -> CGError;
}

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    /// Returns a `CFDictionary` describing the IOKit framebuffer
    /// service backing the given display. Caller owns the returned
    /// dict (CFRelease needed; the `core_foundation` wrapper handles
    /// that for us). When `options` includes
    /// `kIODisplayOnlyPreferredName` the `DisplayProductName` entry
    /// is set when the OS has a preferred name to give (e.g. a
    /// calibration profile for a calibrated display).
    fn IODisplayCreateInfoDictionary(framebuffer: io_service_t, options: IOOptionBits)
    -> CFTypeRef;
    /// Release an IOKit object obtained via `CGDisplayIOServicePort`.
    /// Standard IOKit ownership rule: every `io_service_t` returned
    /// from a getter needs a matching `IOObjectRelease`.
    fn IOObjectRelease(object: io_object_t) -> i32;
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

unsafe fn configure_cf_settings() -> Result<(), MacosCaptureCreationError> {
    // When we warp the cursor using CGWarpMouseCursorPosition local events are suppressed for a short time
    // this leads to the cursor not flowing when crossing back from a client; setting this to 0 stops the warp
    // from working, so we set a low value by trial and error. 0.05s seems good. 0.25s is the default
    let event_source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
        .map_err(|_| MacosCaptureCreationError::EventSourceCreation)?;
    CGEventSourceSetLocalEventsSuppressionInterval(event_source, 0.05);
    // FIXME Memory Leak

    // This is a private settings that allows the cursor to be hidden while in the background.
    // It is used by Barrier and other apps.
    let key = CString::new("SetsCursorInBackground").unwrap();
    let cf_key = CFStringCreateWithCString(
        kCFAllocatorDefault,
        key.as_ptr() as *const c_char,
        kCFStringEncodingUTF8,
    );
    if CGSSetConnectionProperty(
        _CGSDefaultConnection(),
        _CGSDefaultConnection(),
        cf_key,
        kCFBooleanTrue,
    ) != kCGErrorSuccess
    {
        return Err(MacosCaptureCreationError::CGCursorProperty);
    }
    CFRelease(cf_key as *const c_void);
    Ok(())
}

// From X11/X.h
bitflags! {
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
    struct XMods: u32 {
        const ShiftMask = (1<<0);
        const LockMask = (1<<1);
        const ControlMask = (1<<2);
        const Mod1Mask = (1<<3);
        const Mod2Mask = (1<<4);
        const Mod3Mask = (1<<5);
        const Mod4Mask = (1<<6);
        const Mod5Mask = (1<<7);
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the M2 STEP-2.2 macOS monitor enumeration.
    //!
    //! The IOKit-touching helpers (`enumerate_monitors`, `read_display_info`)
    //! can't be unit-tested without a real display and TCC permissions.
    //! What we *can* test in isolation:
    //!
    //! 1. `build_stable_id` — the colon-separated hex/decimal
    //!    composition rule that distinguishes two displays on the same
    //!    machine (replug-stable, port-stable).
    //! 2. `compute_scale` — the pixel/point ratio with degenerate
    //!    inputs (zero width → fallback).
    //! 3. `DisplayInfo::unknown` — the IOKit-unavailable fallback that
    //!    keeps the stable id unique across displays by splicing the
    //!    Quartz `display_id` into the fallback `location`. Regression
    //!    guard for the STEP-M2-2.2-FIXUP P1 id-collision bug.
    //!
    //! All are pure functions with no FFI, so they exercise the
    //! implementation cheaply on every CI run.

    use super::{
        DisplayInfo, MonitorInfo, build_display_bounds, build_stable_id, compute_scale,
        enumerate_monitors,
    };

    /// Vendor + product are formatted as 4-digit hex (zero-padded)
    /// so the id stays visually scannable and grep-friendly. Serial
    /// and location come through verbatim because they are
    /// already human-readable on the OS side.
    #[test]
    fn stable_id_format() {
        let id = build_stable_id(0x1234, 0x5678, "ABC123", "External");
        assert_eq!(id, "macos:1234:5678:ABC123:External");
    }

    /// Small vendor/product values (e.g. some external displays) still
    /// zero-pad correctly so two ids differing only in the leading
    /// digit can't collide on whitespace.
    #[test]
    fn stable_id_zero_pads_small_values() {
        let id = build_stable_id(0x1, 0xa, "0", "Internal");
        assert_eq!(id, "macos:0001:000a:0:Internal");
    }

    /// `serial` and `location` are kept verbatim so non-ASCII /
    /// spaces survive the round-trip. Some manufacturers report the
    /// serial as "0" when no EDID serial is present (Apple's
    /// built-in displays fall in this bucket — they report
    /// kIODisplaySerialNumber = "0").
    #[test]
    fn stable_id_preserves_serial_and_location() {
        let id = build_stable_id(0x10ac, 0xa0f8, "0", "Internal");
        assert_eq!(id, "macos:10ac:a0f8:0:Internal");
    }

    /// Built-in Retina: 2880 points wide, 5760 device pixels →
    /// scale = 2.0. This is the only scale value Apple's MBP
    /// built-in displays report at the OS level.
    #[test]
    fn compute_scale_retina_2x() {
        assert!((compute_scale(5760.0, 2880.0) - 2.0).abs() < 1e-9);
    }

    /// External 1080p: 1920 points, 1920 pixels → scale = 1.0. The
    /// most common case for "second monitor plugged into a MacBook".
    #[test]
    fn compute_scale_external_1x() {
        assert!((compute_scale(1920.0, 1920.0) - 1.0).abs() < 1e-9);
    }

    /// A 4K external display at "looks like 1920×1080" reports 4
    /// device pixels per point (the GPU does the fractional scaling).
    /// The plan calls this out as a known limitation; the test pins
    /// the arithmetic so a future refactor doesn't accidentally
    /// round-down and lose HiDPI info.
    #[test]
    fn compute_scale_4k_looks_like_1080p() {
        assert!((compute_scale(3840.0, 1920.0) - 2.0).abs() < 1e-9);
    }

    /// Degenerate inputs (zero width) must fall back to 1.0 instead
    /// of producing NaN / Inf, which would propagate into the IPC
    /// payload and confuse the GUI.
    #[test]
    fn compute_scale_zero_width_falls_back_to_one() {
        assert_eq!(compute_scale(0.0, 0.0), 1.0);
        assert_eq!(compute_scale(100.0, 0.0), 1.0);
        assert_eq!(compute_scale(0.0, 100.0), 1.0);
    }

    /// Negative inputs are still treated as "no useful point width";
    /// falling back to 1.0 keeps the IPC layer safe even when the
    /// CGDisplayBounds returns a degenerate rectangle during
    /// transient state mid-reconfigure.
    #[test]
    fn compute_scale_negative_falls_back_to_one() {
        assert_eq!(compute_scale(-1.0, -1.0), 1.0);
        assert_eq!(compute_scale(100.0, -1.0), 1.0);
    }

    // ----- DisplayInfo IOKit-unavailable fallback (STEP-M2-2.2-FIXUP) ---
    //
    // The P1 id-collision bug was: when IOKit returns a zeroed
    // DisplayInfo (no service port, or the dict was null), the
    // `build_stable_id` formula collapsed to `"macos:0000:0000:"` for
    // every such display, so two simultaneously-failing displays
    // would share an id. The fix splices the Quartz `display_id`
    // into the fallback `location`. These tests pin the contract.

    /// The fallback `DisplayInfo` keeps vendor/product/serial at
    /// their zero-sentinel defaults but injects the `display_id`
    /// into `location` so the downstream stable id remains unique.
    /// Without this, `DisplayInfo::default()` would yield
    /// `serial="" / location=""` and the stable id would be
    /// `"macos:0000:0000:"` for every IOKit-failed display.
    #[test]
    fn display_info_unknown_encodes_display_id_in_location() {
        let info = DisplayInfo::unknown(0x4271a80);
        assert_eq!(info.vendor, 0);
        assert_eq!(info.product, 0);
        assert_eq!(info.serial, "");
        // 0x4271a80 == 69_671_552 in decimal.
        assert_eq!(info.location, "unknown-69671552");
        assert!(info.name.is_none());
    }

    /// Single-display regression: when only one display fails IOKit
    /// (the typical case — TCC denied on one service port, transient
    /// state on one), the resulting stable id still contains the
    /// `display_id` and is distinct from a successful IOKit read.
    #[test]
    fn stable_id_includes_display_id_when_iokit_unavailable_single() {
        let info = DisplayInfo::unknown(0x1234);
        let id = build_stable_id(info.vendor, info.product, &info.serial, &info.location);
        assert_eq!(id, "macos:0000:0000::unknown-4660");
        // The display_id segment MUST appear so a future regression
        // that strips it is caught here rather than in production.
        assert!(
            id.contains("unknown-4660"),
            "stable id must encode display_id when IOKit fails: {id}"
        );
    }

    /// Multi-display regression (the actual P1 scenario): when two
    /// displays simultaneously fail IOKit (rare but possible — TCC
    /// revoked at process start, transient IOKit unavailability),
    /// the produced stable ids MUST be distinct. The pre-fix code
    /// produced two identical `"macos:0000:0000:"` strings, breaking
    /// the stable-id uniqueness invariant called out in PLAN §M2
    /// STEP-2.2.
    #[test]
    fn stable_ids_for_two_simultaneously_failed_displays_are_unique() {
        let info_a = DisplayInfo::unknown(0x4271a80);
        let info_b = DisplayInfo::unknown(0x4271b00);
        let id_a = build_stable_id(
            info_a.vendor,
            info_a.product,
            &info_a.serial,
            &info_a.location,
        );
        let id_b = build_stable_id(
            info_b.vendor,
            info_b.product,
            &info_b.serial,
            &info_b.location,
        );
        assert_ne!(
            id_a, id_b,
            "two IOKit-failed displays must produce distinct stable ids (P1 regression guard)"
        );
        // Sanity: each id retains the canonical macos:vvvv:pppp prefix
        // so existing parsers / dashboards don't need updating.
        assert!(id_a.starts_with("macos:0000:0000:"));
        assert!(id_b.starts_with("macos:0000:0000:"));
    }

    // ----- STEP-M2-2.7 HOT-PLUG FIX REGRESSION GUARD ----------------------
    //
    // The macOS dual-display manual test (see STEP-M2-2.7 §6) surfaced a
    // bug: `DisplayReconfigured` in `handle_producer_event` gated the
    // `monitors_tx.send(...)` push on `update_bounds()` succeeding. When
    // `CGDisplay::active_displays()` transiently failed mid-reconfigure
    // (kIOReturnBusy / transient CG error), no `MonitorsChanged` event
    // reached the GUI and the M3 monitor dropdown froze on the stale
    // list. The fix decoupled the two: `update_bounds()` result only
    // affects barrier-tracking health; `enumerate_monitors()` is always
    // called.
    //
    // This regression guard ensures `enumerate_monitors` reads the live
    // OS state via `CGDisplay::active_displays()` directly and ignores
    // its `displays` argument. That's the structural precondition for
    // "DisplayReconfigured always pushes a fresh snapshot to
    // monitors_tx, even when `update_bounds` left `self.displays`
    // empty after a transient failure".

    use crate::geometry::{DisplayBound, DisplayRect};

    /// `enumerate_monitors` must re-query `CGDisplay::active_displays()`
    /// directly and ignore the `displays` slice — the slice is kept
    /// around for documentation / future-injection only. Two calls
    /// with different slice contents MUST return the same live
    /// snapshot. If a future refactor makes the function start
    /// reading from the slice, this test fails and the hot-plug
    /// bug would silently re-appear in production.
    #[test]
    fn enumerate_monitors_ignores_displays_slice_and_returns_live_state() {
        let from_empty = enumerate_monitors(&[]);
        // A deliberately bogus slice: one entry with negative
        // coordinates that could never be the actual OS-reported
        // bounds on a real Mac. If `enumerate_monitors` ever
        // started reading from this slice instead of the live
        // Quartz enumeration, the returned list would diverge
        // from `from_empty`.
        let from_bogus = enumerate_monitors(&[DisplayBound::new(
            DisplayRect::new(-987_654.0, -987_654.0, 1.0, 1.0),
            None,
        )]);

        assert_eq!(
            from_empty.len(),
            from_bogus.len(),
            "enumerate_monitors must not depend on the `displays` slice \
             (live `CGDisplay::active_displays()` counts must agree)"
        );

        let empty_ids: Vec<String> = from_empty.iter().map(|m| m.id.clone()).collect();
        let bogus_ids: Vec<String> = from_bogus.iter().map(|m| m.id.clone()).collect();
        assert_eq!(
            empty_ids, bogus_ids,
            "enumerate_monitors must return identical ids for identical \
             live state, regardless of the `displays` slice content"
        );

        // Sanity: the live snapshot must be non-empty (the test
        // runs on a real macOS with at least the built-in display).
        // If this fires on a headless CI runner, the test setup
        // itself is wrong — not the production code.
        assert!(
            !from_empty.is_empty(),
            "live `CGDisplay::active_displays()` returned an empty \
             snapshot on macOS — test environment is not real hardware"
        );
    }

    // ----- STEP-3.4 build_display_bounds_pure --------------------------
    //
    // Pins the contract that `build_display_bounds` is the single
    // join point between Quartz's active-display enumeration and
    // the per-OS `MonitorInfo` list. The three properties the PLAN
    // calls out:
    //
    //   1. id 唯一   — every emitted `DisplayBound` carries a
    //                 distinct `monitor_id`, derived 1:1 from the
    //                 input `MonitorInfo.id`s.
    //   2. idx 一致 — `display_containing` on the emitted rects
    //                 returns the same idx as the source
    //                 `MonitorInfo` list, so the barrier-detection
    //                 query carries the right `monitor` field.
    //   3. length   — `len(out) == active_ids.len() ==
    //                 monitors.len()`. The single-source-of-truth
    //                 refactor would silently lose displays if a
    //                 future change skipped IDs.

    use crate::geometry::display_containing_bound;
    use std::collections::HashSet;

    fn monitor(id: &str, x: i32, y: i32, w: u32, h: u32) -> MonitorInfo {
        MonitorInfo {
            id: id.to_string(),
            name: format!("Display {id}"),
            position: (x, y),
            size: (w, h),
            primary: false,
            scale: 1.0,
        }
    }

    /// Happy path: 2x1 horizontal pair. Two active IDs, two
    /// MonitorInfo entries, two DisplayBound entries. Ids are
    /// unique, indices line up with the source list, and the
    /// length matches.
    #[test]
    fn build_display_bounds_pure_2x1_layout() {
        let active_ids = vec![0xAAAA_u32, 0xBBBB_u32];
        let monitors = vec![
            monitor("macos:aaaa", 0, 0, 1920, 1080),
            monitor("macos:bbbb", 1920, 0, 1920, 1080),
        ];
        let bounds = build_display_bounds(&active_ids, &monitors);
        assert_eq!(bounds.len(), 2);
        assert_eq!(bounds.len(), active_ids.len());

        let ids: HashSet<String> = bounds.iter().filter_map(|b| b.monitor_id.clone()).collect();
        assert_eq!(ids.len(), 2, "monitor_ids must be unique");

        // Index-of via display_containing_bound lines up with the
        // source list: prev on d0 → idx 0, prev on d1 → idx 1.
        let d0 = display_containing_bound(&bounds, (500.0, 500.0)).expect("d0 contains (500, 500)");
        let d1 =
            display_containing_bound(&bounds, (2500.0, 500.0)).expect("d1 contains (2500, 500)");
        assert_eq!(d0.monitor_id.as_deref(), Some("macos:aaaa"));
        assert_eq!(d1.monitor_id.as_deref(), Some("macos:bbbb"));
    }

    /// Empty input → empty output. The caller must always be able
    /// to overwrite `self.displays` without losing old entries on
    /// the empty case.
    #[test]
    fn build_display_bounds_pure_empty_input() {
        let bounds = build_display_bounds(&[], &[]);
        assert!(bounds.is_empty());
    }

    /// Single-display regression: the function still produces one
    /// entry with the right id when there's only one active
    /// monitor (the most common case on a laptop).
    #[test]
    fn build_display_bounds_pure_single_display() {
        let active_ids = vec![0x1234_u32];
        let monitors = vec![monitor("macos:1234", 0, 0, 2880, 1800)];
        let bounds = build_display_bounds(&active_ids, &monitors);
        assert_eq!(bounds.len(), 1);
        assert_eq!(bounds[0].monitor_id.as_deref(), Some("macos:1234"));
        assert_eq!(bounds[0].rect.left(), 0.0);
        assert_eq!(bounds[0].rect.top(), 0.0);
        assert_eq!(bounds[0].rect.right(), 2880.0);
        assert_eq!(bounds[0].rect.bottom(), 1800.0);
    }

    /// 3x1 horizontal: three displays, three ids, indices preserve
    /// source order. Catches a future "off-by-one in the
    /// map-and-collect" regression.
    #[test]
    fn build_display_bounds_pure_3x1_preserves_order() {
        let active_ids = vec![1_u32, 2, 3];
        let monitors = vec![
            monitor("d-a", 0, 0, 1920, 1080),
            monitor("d-b", 1920, 0, 1920, 1080),
            monitor("d-c", 3840, 0, 1920, 1080),
        ];
        let bounds = build_display_bounds(&active_ids, &monitors);
        assert_eq!(bounds.len(), 3);
        assert_eq!(bounds[0].monitor_id.as_deref(), Some("d-a"));
        assert_eq!(bounds[1].monitor_id.as_deref(), Some("d-b"));
        assert_eq!(bounds[2].monitor_id.as_deref(), Some("d-c"));

        // Display_containing_idx lookup uses the same order: a
        // point clearly inside d-b lands on idx 1.
        let hit =
            display_containing_bound(&bounds, (2500.0, 500.0)).expect("d-b contains (2500, 500)");
        assert_eq!(hit.monitor_id.as_deref(), Some("d-b"));
    }
}
