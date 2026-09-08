//! Geometry primitives shared by every capture backend.
//!
//! Before this module existed, the same set of pure helpers
//! (`is_within_dp_region`, `is_within_dp_boundary`, `in_bounds`,
//! `in_display_region`, `moved_across_boundary`, `entered_barrier`,
//! `cursor_within`, `clamp_to_display_bounds`) lived inside
//! `windows/display_util.rs` and were only accessible from the Windows
//! backend. macOS meanwhile implemented its own (subtly different)
//! copy in `macos::Bounds` based on a single axis-aligned bounding box
//! — a representation that cannot model real-world multi-display
//! layouts where displays overlap or sit at different heights.
//!
//! This module unifies both behind a single representation:
//!
//! - [`DisplayRect`] is an OS-agnostic rectangle (origin + size, all
//!   `f64` to preserve macOS HiDPI precision; Windows converts at the
//!   boundary from its `i32` `RECT`).
//! - The pure helpers take `&[DisplayRect]` and `(f64, f64)` cursor
//!   points so every backend speaks the same vocabulary.
//!
//! Windows keeps a thin re-export shim at `windows/display_util.rs` so
//! `event_thread.rs` can keep using its existing imports during the
//! M0 refactor.

use crate::Position;

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// An OS-agnostic display rectangle.
///
/// Uses `f64` for every component so macOS' `CGRect` (which is
/// `CGPoint` + `CGSize`, both `f64`) round-trips without precision
/// loss. Windows converts from its `i32` `RECT` at the boundary
/// (`display_util.rs` / `event_thread.rs::enumerate_displays`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl DisplayRect {
    /// Construct from origin + size.
    pub const fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y, w, h }
    }

    /// Construct from left/top/right/bottom extents (Windows `RECT`
    /// convention).
    pub fn from_ltrb(left: f64, top: f64, right: f64, bottom: f64) -> Self {
        Self {
            x: left,
            y: top,
            w: right - left,
            h: bottom - top,
        }
    }

    #[inline]
    pub const fn left(&self) -> f64 {
        self.x
    }

    #[inline]
    pub const fn top(&self) -> f64 {
        self.y
    }

    #[inline]
    pub const fn right(&self) -> f64 {
        self.x + self.w
    }

    #[inline]
    pub const fn bottom(&self) -> f64 {
        self.y + self.h
    }
}

/// A [`DisplayRect`] paired with the [`MonitorId`] that owns it.
///
/// This is the in-state representation every capture backend holds
/// once M3 lands: per-OS enumeration produces both the OS-agnostic
/// rectangle (via Quartz / Win32 EnumDisplayDevices / Wayland
/// `wl_output` / libei `Zones.regions()`) AND a stable per-monitor
/// id, and barrier detection needs both at the same time so the
/// query `BarrierKey` can be reconstructed with the right
/// `monitor` field. Splitting the two into parallel arrays would
/// invite index drift between them.
///
/// `monitor_id = None` is allowed for backends that haven't
/// completed the M2 enumeration yet (transient state, fresh bind).
/// The 4x2 matrix tests cover the "monitor=None falls through to
/// the legacy single-edge query" behavior; production callers can
/// expect this to never be `None` after the first
/// `DisplayReconfigured` / `WM_DISPLAYCHANGE` settles.
#[derive(Debug, Clone, PartialEq)]
pub struct DisplayBound {
    pub rect: DisplayRect,
    pub monitor_id: Option<MonitorId>,
}

impl DisplayBound {
    /// Construct a `DisplayBound` from origin/size and a stable id.
    pub const fn new(rect: DisplayRect, monitor_id: Option<MonitorId>) -> Self {
        Self { rect, monitor_id }
    }
}

/// Returns true when `point` is inside `display` for every barrier
/// direction. Used as the "fully on-screen" predicate, and as the
/// containment primitive that [`in_display_region`] filters by.
#[inline]
fn is_within_dp_region(point: (f64, f64), display: &DisplayRect) -> bool {
    [
        Position::Left,
        Position::Right,
        Position::Top,
        Position::Bottom,
    ]
    .iter()
    .all(|&pos| is_within_dp_boundary(point, display, pos))
}

/// Returns true when `point` is on the *inside* of `pos` for `display`.
/// This is the per-edge primitive used by both [`entered_barrier`] and
/// [`cursor_within`].
#[inline]
fn is_within_dp_boundary(point: (f64, f64), display: &DisplayRect, pos: Position) -> bool {
    let (x, y) = point;
    match pos {
        Position::Left => display.left() <= x,
        Position::Right => display.right() > x,
        Position::Top => display.top() <= y,
        Position::Bottom => display.bottom() > y,
    }
}

/// Returns true when `point` is on the inside of `pos` for **any**
/// display in `displays`. Used as the "did we leave some display on
/// this side" predicate.
fn in_bounds(point: (f64, f64), displays: &[DisplayRect], pos: Position) -> bool {
    displays
        .iter()
        .any(|d| is_within_dp_boundary(point, d, pos))
}

/// Returns true when `point` is contained by **any** display at all
/// (i.e. is inside the union of all display rectangles).
fn in_display_region(point: (f64, f64), displays: &[DisplayRect]) -> bool {
    displays.iter().any(|d| is_within_dp_region(point, d))
}

/// Returns true when the cursor was inside the union of `displays`
/// and is now outside it with respect to the `pos` side.
///
/// Detection has three parts:
/// 1. **Union exit**: `prev_pos` is inside some display and
///    `curr_pos` is outside every display. This is the primary
///    containment check — testing "did we leave the union" rather
///    than "is the curr_pos still on the inside of `pos` for some
///    display", because the per-axis `in_bounds` predicate is
///    satisfied by *any* display extending past the relevant axis
///    and therefore masks the real exit when an outer display
///    covers an inner display's edges (the original
///    STEP-DEBUG-D1-LEFT bug).
/// 2. **Direction inference**: the `(curr - prev)` motion vector
///    must point along the `pos` axis in the direction implied by
///    `pos`. Without this the function would return true for every
///    `pos` whenever the cursor exited the union, and
///    [`entered_barrier`] would always pick the first iterated
///    side (Left) regardless of actual motion. The `dx`/`dy`
///    comparison is sign-only.
/// 3. **Axis dominance**: the axis relevant to `pos` must dominate
///    the motion — i.e. `|dx| >= |dy|` for the horizontal sides
///    (Left/Right) and `|dy| >= |dx|` for the vertical sides
///    (Top/Bottom). This is what disambiguates diagonal exits
///    through a display corner (the user-reported
///    STEP-DEBUG-D1-BOTTOM bug): when the cursor exits the
///    bottom-left corner with mostly-downward motion, the sign
///    check alone would fire BOTH `Left` (dx < 0) and `Bottom`
///    (dy > 0), and `entered_barrier`'s priority list
///    `[Left, Right, Top, Bottom]` would then pick `Left` —
///    incorrectly attributing the crossing to the horizontal edge
///    that has a configured neighbor (the controlled machine on
///    the user's left), even though the cursor clearly moved
///    downward off the bottom edge. Requiring dominance breaks
///    that ambiguity: `Bottom` fires, `Left` does not.
///
/// Used as the per-edge detector inside [`entered_barrier`].
fn moved_across_boundary(
    prev_pos: (f64, f64),
    curr_pos: (f64, f64),
    displays: &[DisplayRect],
    pos: Position,
) -> bool {
    if !in_display_region(prev_pos, displays) || in_display_region(curr_pos, displays) {
        return false;
    }
    let dx = curr_pos.0 - prev_pos.0;
    let dy = curr_pos.1 - prev_pos.1;
    let adx = dx.abs();
    let ady = dy.abs();
    match pos {
        Position::Left => dx < 0.0 && adx >= ady,
        Position::Right => dx > 0.0 && adx >= ady,
        Position::Top => dy < 0.0 && ady >= adx,
        Position::Bottom => dy > 0.0 && ady >= adx,
    }
}

/// Detect a barrier crossing: returns the first [`Position`] for which
/// the cursor moved from "inside some display" to "outside on this
/// side". The result is `None` when the cursor stayed inside or
/// jumped straight across the union (which physically shouldn't
/// happen for normal mouse motion).
pub fn entered_barrier(
    prev_pos: (f64, f64),
    curr_pos: (f64, f64),
    displays: &[DisplayRect],
) -> Option<Position> {
    [
        Position::Left,
        Position::Right,
        Position::Top,
        Position::Bottom,
    ]
    .into_iter()
    .find(|&pos| moved_across_boundary(prev_pos, curr_pos, displays, pos))
}

/// Return the first display in `displays` that contains `point`.
///
/// "Contains" follows the half-open convention used by `CGPoint`
/// membership tests: the left/top edges are inclusive, the
/// right/bottom edges are exclusive. This matches what the macOS
/// event tap considers "inside the screen" — a cursor at exactly
/// `(display.right(), display.top())` is considered past the right
/// edge.
///
/// `None` when the point falls outside every display, including the
/// degenerate case of an empty display list.
pub fn display_containing(displays: &[DisplayRect], point: (f64, f64)) -> Option<&DisplayRect> {
    let (x, y) = point;
    displays
        .iter()
        .find(|d| d.left() <= x && x < d.right() && d.top() <= y && y < d.bottom())
}

/// Index-of variant of [`display_containing`]. Returns the position
/// of the first display in `displays` that contains `point` under
/// the same half-open convention. Used by [`crossed_pure`] to look
/// up the `monitor_id` that goes into the BarrierKey query.
///
/// Returns `None` when the point falls outside every display (or the
/// list is empty). Edge-seam goes to the right display (half-open:
/// left/up inclusive, right/down exclusive): `(1920.0, 540.0)` in a
/// 2x1 horizontal pair belongs to the right display (idx 1), since
/// the left display's right edge is exclusive.
pub fn display_containing_idx(displays: &[DisplayRect], point: (f64, f64)) -> Option<usize> {
    let (x, y) = point;
    displays
        .iter()
        .position(|d| d.left() <= x && x < d.right() && d.top() <= y && y < d.bottom())
}

/// Same as [`display_containing`] but for `&[DisplayBound]` slices —
/// the M3 backend representation that pairs each rectangle with a
/// per-display `monitor_id`. Mirrors the half-open convention; the
/// returned `&DisplayBound` lets the caller pull `monitor_id` out
/// without indexing again.
pub fn display_containing_bound(
    displays: &[DisplayBound],
    point: (f64, f64),
) -> Option<&DisplayBound> {
    let (x, y) = point;
    displays.iter().find(|d| {
        let r = &d.rect;
        r.left() <= x && x < r.right() && r.top() <= y && y < r.bottom()
    })
}

/// Returns whether `point` is on the *inside* of `pos` for every display
/// the cursor is currently over. Used by the pending-capture handshake
/// to detect the user pulling the cursor back inside the screen before
/// the remote client ACKs the Enter.
///
/// Conceptually the inverse of [`entered_barrier`]: `entered_barrier`
/// fires on the transition from "inside" to "outside", and `cursor_within`
/// answers "is it currently inside".
pub fn cursor_within(point: (f64, f64), displays: &[DisplayRect], pos: Position) -> bool {
    in_display_region(point, displays) && in_bounds(point, displays, pos)
}

/// Clamp `point` to the bounds of the display that contained the
/// previous cursor position. Used as the warp target on
/// pending-capture entry: after the user crosses a barrier we want
/// the cursor to be parked 1px inside the edge of the display they
/// came from (so the receiving peer sees a sensible Motion delta
/// when it promotes to active).
///
/// Returns the input point unchanged when no display matches — caller
/// must handle that case (typically by leaving the pending state
/// alone, since warping to a stale location would be worse than not
/// warping).
pub fn clamp_to_display_bounds(
    display_regions: &[DisplayRect],
    prev_point: (f64, f64),
    point: (f64, f64),
) -> (f64, f64) {
    /* find display where movement came from */
    let display = display_regions
        .iter()
        .find(|&d| is_within_dp_region(prev_point, d));

    let Some(display) = display else {
        return point;
    };

    /* clamp to bounds (inclusive) */
    let (x, y) = point;
    let (min_x, max_x) = (display.left(), display.right() - 1.0);
    let (min_y, max_y) = (display.top(), display.bottom() - 1.0);
    (x.clamp(min_x, max_x), y.clamp(min_y, max_y))
}

/// A stable identifier for a physical monitor.
///
/// Populated by M2 from per-OS enumeration (macOS `IODisplay` UUID,
/// Windows EDID hash, Wayland `wl_output` name, libei `Zones`
/// region-key). During M1 every barrier key uses `None` to preserve
/// single-monitor / no-monitor-info behavior.
pub type MonitorId = String;

/// Snapshot of one physical monitor's geometry and metadata,
/// produced by the per-OS backends' `enumerate_monitors()`.
///
/// This is the **internal domain type** used by `input-capture`
/// backends and the `Capture::monitors()` plumbing. The IPC crate
/// keeps a separate mirrored copy (`lan_mouse_ipc::MonitorInfo`)
/// because the wire schema may evolve independently from the
/// in-process one.
///
/// `position` is the display's origin in virtual-screen coordinates
/// (`(x, y)`, top-left inclusive, signed because the right / top
/// display in a horizontal pair has a negative `x` while the
/// primary is on the left). `size` is `(width, height)` in the same
/// coordinate space.
///
/// `scale` is the per-monitor HiDPI scale factor (1.0 = standard
/// density, 2.0 = Retina / 4K equivalent). It's typed as `f64`
/// because some compositors (Windows with mixed-DPI awareness,
/// macOS under per-display virtual scaling) report non-integer
/// values such as 1.25 or 1.5.
///
/// `#[serde(rename_all = "snake_case")]` makes the on-wire JSON
/// look like `{"id": ..., "name": ..., "position": [x, y],
/// "size": [w, h], "primary": ..., "scale": ...}` so the field
/// names line up with what the frontend already expects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct MonitorInfo {
    /// Stable, OS-agnostic monitor id (EDID-derived where possible,
    /// falls back to `wl_output` name / portal region key).
    pub id: MonitorId,
    /// Human-readable label (e.g. "LG UltraFine 5K"). May contain
    /// UTF-8 (some manufacturers ship CJK / accented names); UTF-8
    /// round-trip is verified by `monitor_info_round_trip_utf8_name`.
    pub name: String,
    /// Display origin in virtual-screen coordinates, signed to
    /// allow negative coordinates on the right / top monitor in a
    /// 2x1 layout. Verified by `monitor_info_round_trip_negative`.
    pub position: (i32, i32),
    /// Display size in virtual-screen coordinates.
    pub size: (u32, u32),
    /// Whether this is the OS's primary display.
    pub primary: bool,
    /// HiDPI scale factor (1.0 = standard, 2.0 = Retina). Non-
    /// integer values are permitted on platforms that report them.
    /// Verified by `monitor_info_round_trip_mixed_scale`.
    pub scale: f64,
}

/// A barrier location: which edge (`pos`), which physical monitor
/// (`monitor`), and along which sub-range (`offset`, `span`) of that
/// edge.
///
/// All fields are interpreted by the per-OS backend. `offset` and
/// `span` are in permyriad (1/10000) of the edge length: `offset =
/// 0, span = 10000` covers the full edge; `offset = 5000, span =
/// 2500` covers the central half-quarter of the edge.
///
/// `Default` produces the legacy single-edge key (`monitor = None`,
/// full edge) so existing call sites can opt into the new type
/// without changing behavior.
#[derive(Debug, Clone, Eq, Hash, PartialEq)]
pub struct BarrierKey {
    pub pos: Position,
    pub monitor: Option<MonitorId>,
    /// Sub-range start as permyriad (1/10000) of the edge length.
    pub offset: u16,
    /// Sub-range length as permyriad (1/10000) of the edge length.
    pub span: u16,
}

impl Default for BarrierKey {
    fn default() -> Self {
        Self {
            pos: Position::Left,
            monitor: None,
            offset: 0,
            span: 10000,
        }
    }
}

impl BarrierKey {
    /// Construct a full-edge [`BarrierKey`] for `pos` with no monitor
    /// information. Used by M1 backends that only carry `Position`
    /// today; will be superseded by full BarrierKey payloads in M2+.
    pub fn from_pos(pos: Position) -> Self {
        Self {
            pos,
            monitor: None,
            offset: 0,
            span: 10000,
        }
    }
}

/// Shared backend-side query: detect a barrier crossing, look up
/// the `monitor_id` for the display that contained `prev_pos`, and
/// probe `clients` for the resulting [`BarrierKey`].
///
/// This is the private helper both [`crossed_pure`] (macOS
/// STEP-3.4) and [`activation_pure`] (Windows STEP-3.5) call into.
/// Extracting the shared logic lets the two backends stay in lock-
/// step on the half-open containment rule, the seam-attribution
/// convention, and the `monitor_id == None` fallback — drift
/// between them is the precise regression the PLAN §M3 §5 calls
/// out as a "100% edge miss" bug.
///
/// `prev_pos` outside every display, or no barrier crossed, both
/// yield `None`; the caller is responsible for the post-processing
/// (e.g. deciding whether a hit should start a pending handshake
/// or set the active client).
fn query_pure(
    prev_pos: (f64, f64),
    curr_pos: (f64, f64),
    displays: &[DisplayBound],
    clients: &HashSet<BarrierKey>,
) -> Option<BarrierKey> {
    // Project to DisplayRect for the geometry primitives. The slice
    // is small (one entry per attached monitor, typically 1-4) and
    // the helper is only called once per mouse-move barrier event,
    // so the allocation cost is negligible.
    let rects: Vec<DisplayRect> = displays.iter().map(|d| d.rect).collect();
    let pos = entered_barrier(prev_pos, curr_pos, &rects)?;
    let idx = display_containing_idx(&rects, prev_pos)?;
    let key = BarrierKey {
        pos,
        monitor: displays[idx].monitor_id.clone(),
        offset: 0,
        span: 10000,
    };
    if clients.contains(&key) {
        return Some(key);
    }
    // H1 legacy fallback (STEP-M3-3.4-FIXUP): when the specific
    // `monitor: Some(...)` key misses but `clients` does carry a
    // legacy `monitor: None` entry for the same `pos`, hit that.
    //
    // Rationale: every backend's `displays` production path
    // (`build_display_bounds` on macOS, `update_display_regions`
    // on Windows) fills `monitor_id` as `Some(...)` 100% of the
    // time, so the PLAN §3 STEP-3.4 "display.monitor_id == None →
    // legacy lookup" invariant never fires in production. The real
    // legacy-config signal lives client-side: `ClientConfig.monitor
    // == None` (the default for legacy configs / dropdown not yet
    // picked). Without this fallback, a legacy client's barrier
    // trigger silently disappears (100% edge miss) after M3.4.
    //
    // Guarded by `key.monitor.is_some()` so we never pay the
    // second HashSet probe when the containing display itself has
    // no monitor id (the pre-fix fast path already handles that
    // case via the `clients.contains(&key)` hit above).
    if key.monitor.is_some() {
        let legacy_key = BarrierKey {
            monitor: None,
            ..key
        };
        if clients.contains(&legacy_key) {
            return Some(legacy_key);
        }
    }
    None
}

/// Detect a barrier crossing and look up the matching **active**
/// client key.
///
/// macOS (`crossed`) is the only backend that re-uses this entry
/// point in STEP-3.4 — the active client set and the registered
/// client set are the same `HashSet` there. The function is named
/// `crossed_pure` for symmetry with the historical event-tap side
/// (`crossed` / `entered_barrier` / `clamp_to_display_bounds`).
///
/// Combines the M0 [`entered_barrier`] position detector with the
/// per-display [`monitor_id`] lookup so the returned key has the
/// right `monitor` field. The 4x2 test matrix in the `tests` module
/// pins the contract:
///
/// - `prev` inside display `i` → query uses `displays[i].monitor_id`
/// - `prev` on the seam between two displays → goes to the display
///   that contains it under the half-open convention (right display
///   for `(1920.0, 540.0)` in a 2x1 horizontal pair)
/// - `prev` outside every display → `None` (no edge attribution)
/// - `monitor_id == None` on the containing display → fast path:
///   the query key already has `monitor: None` so the lookup is
///   identical to the legacy "monitor-agnostic" form (matches
///   active keys with `monitor: None`)
/// - specific-key miss + `monitor: None` entry in `clients` →
///   legacy fallback (STEP-M3-3.4-FIXUP, H1). The query key was
///   built with the containing display's `monitor_id` (which is
///   `Some(...)` in production) and didn't match; if `clients`
///   carries a legacy `monitor: None` entry for the same `pos`,
///   return it. This restores pre-M3.4 behavior for clients
///   whose `ClientConfig.monitor` is `None` (the default for
///   legacy configs / dropdown not yet picked).
///
/// `clients.contains(&key)` decides the final hit / miss. `offset`
/// / `span` are not parameterized in this entry point — M4
/// `activation_pure` will add them.
pub fn crossed_pure(
    prev_pos: (f64, f64),
    curr_pos: (f64, f64),
    displays: &[DisplayBound],
    clients: &HashSet<BarrierKey>,
) -> Option<BarrierKey> {
    query_pure(prev_pos, curr_pos, displays, clients)
}

/// Detect a barrier crossing and look up the matching **registered**
/// client key (the Windows `check_client_activation` analog).
///
/// Windows holds two separate sets: `CLIENTS` (everything that has
/// ever been created via `Capture::create`) and `ACTIVE_CLIENT` /
/// `PENDING_CLIENT` (the currently-bound one). The
/// pending-handshake flow runs [`cursor_within`] on its own code
/// path, so this helper deliberately only owns the "is the user
/// crossing a registered barrier" piece. The hit/miss decision is
/// identical to macOS — same half-open containment rule, same seam
/// attribution, same `monitor_id == None` legacy fallback — but
/// the function lives alongside [`crossed_pure`] so a future
/// refactor that wants to differentiate the two callers (e.g.
/// macOS checking the `active` set vs. Windows checking the
/// `clients` set) can change the semantics here without touching
/// the other backend.
///
/// The 6-case `activation_pure_4x2` matrix in the `tests` module
/// pins the Windows contract: `prev_pos` in display_0 / display_1 /
/// outside × `monitor: Some("win:...")` / `None` in the registered
/// set. Every case asserts the query `BarrierKey` carries the
/// correct `monitor` field — i.e. NOT hard-coded `None` like the
/// pre-3.5 `check_client_activation` did. The test is the
/// regression guard for the Windows "100% edge miss" bug.
pub fn activation_pure(
    prev_pos: (f64, f64),
    curr_pos: (f64, f64),
    displays: &[DisplayBound],
    clients: &HashSet<BarrierKey>,
) -> Option<BarrierKey> {
    query_pure(prev_pos, curr_pos, displays, clients)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn single_display() -> Vec<DisplayRect> {
        vec![DisplayRect::new(0.0, 0.0, 1920.0, 1080.0)]
    }

    /// 2x1 horizontal layout: two 1080p displays side by side, left
    /// at origin, right starting where left ends (no gap).
    fn layout_2x1() -> Vec<DisplayRect> {
        vec![
            DisplayRect::new(0.0, 0.0, 1920.0, 1080.0),
            DisplayRect::new(1920.0, 0.0, 1920.0, 1080.0),
        ]
    }

    /// 3x1 horizontal layout for the "no spurious middle crossing"
    /// regression: the seam between adjacent displays must not
    /// register as a barrier crossing.
    fn layout_3x1() -> Vec<DisplayRect> {
        vec![
            DisplayRect::new(0.0, 0.0, 1920.0, 1080.0),
            DisplayRect::new(1920.0, 0.0, 1920.0, 1080.0),
            DisplayRect::new(3840.0, 0.0, 1920.0, 1080.0),
        ]
    }

    /// L-shaped offset: one 1080p display below (origin), one above
    /// it but shifted 200px to the right. The top display's bottom
    /// edge spans x in [200, 2120), only partially covering the
    /// bottom display's top edge x in [0, 1920). This is the layout
    /// that exposed the union-bbox bug: prev/curr samples in the
    /// non-overlapping 200px strip must not register as a Top
    /// crossing on the upper display, because the cursor never
    /// actually entered it.
    fn layout_l_shaped_offset_200px() -> Vec<DisplayRect> {
        vec![
            DisplayRect::new(0.0, 0.0, 1920.0, 1080.0),
            DisplayRect::new(200.0, -1080.0, 1920.0, 1080.0),
        ]
    }

    /// Mixed-resolution side-by-side: 1080p on the left, 4K on the
    /// right. The 4K display is taller and starts at the same y as
    /// the 1080p, so its top and bottom edges are not aligned with
    /// the 1080p's. Tests that `display_containing` picks the right
    /// rectangle even at the seam, and that barrier detection works
    /// at the top of either display.
    fn layout_uneven_height() -> Vec<DisplayRect> {
        vec![
            DisplayRect::new(0.0, 0.0, 1920.0, 1080.0),
            DisplayRect::new(1920.0, 0.0, 3840.0, 2160.0),
        ]
    }

    // ----- single-display baselines (carried over from the old
    // windows/display_util.rs test module) ----------------------------

    /// Inside the display on the Right side → still "within".
    #[test]
    fn cursor_within_inside_right_edge() {
        let displays = single_display();
        assert!(cursor_within((1000.0, 500.0), &displays, Position::Right));
        assert!(cursor_within((1919.0, 500.0), &displays, Position::Right));
    }

    /// Cursor pushed past the right edge → not within anymore.
    /// Mirrors `entered_barrier` going the other way: when the user
    /// pulls back from (1921, 500) to (1919, 500) the host's pending
    /// capture must cancel.
    #[test]
    fn cursor_within_outside_right_edge() {
        let displays = single_display();
        assert!(!cursor_within((1921.0, 500.0), &displays, Position::Right));
    }

    /// Cursor far outside all displays → not within (no display at all).
    #[test]
    fn cursor_within_no_display() {
        let displays = single_display();
        assert!(!cursor_within((5000.0, 5000.0), &displays, Position::Right));
    }

    /// `entered_barrier` and `cursor_within` are inverses for the
    /// just-crossed transition: crossing right edge then returning
    /// inside should flip both flags cleanly.
    #[test]
    fn enter_then_pull_back_round_trip() {
        let displays = single_display();
        // inside → outside: barrier entered
        assert_eq!(
            entered_barrier((1919.0, 500.0), (1921.0, 500.0), &displays),
            Some(Position::Right)
        );
        // outside → inside: cursor_within becomes true again
        assert!(!cursor_within((1921.0, 500.0), &displays, Position::Right));
        assert!(cursor_within((1919.0, 500.0), &displays, Position::Right));
    }

    // ----- 2x1 horizontal ---------------------------------------------

    /// Crossing off the right edge of the union (D2's right edge)
    /// must register as a Right barrier.
    #[test]
    fn two_by_one_exit_right_from_union() {
        let displays = layout_2x1();
        assert_eq!(
            entered_barrier((3839.0, 500.0), (3841.0, 500.0), &displays),
            Some(Position::Right)
        );
    }

    /// Crossing off the left edge of the union (D1's left edge) must
    /// register as a Left barrier.
    #[test]
    fn two_by_one_exit_left_from_union() {
        let displays = layout_2x1();
        assert_eq!(
            entered_barrier((0.0, 500.0), (-2.0, 500.0), &displays),
            Some(Position::Left)
        );
    }

    /// Crossing off the top edge of the union fires Top regardless of
    /// which display the cursor was on — both displays share the
    /// same top edge in a 2x1 layout.
    #[test]
    fn two_by_one_exit_top() {
        let displays = layout_2x1();
        assert_eq!(
            entered_barrier((500.0, 0.0), (500.0, -2.0), &displays),
            Some(Position::Top)
        );
    }

    /// Moving the cursor across the seam between D1 and D2 must NOT
    /// register as any barrier crossing — the cursor remains inside
    /// the union the whole time.
    #[test]
    fn two_by_one_seam_motion_does_not_cross() {
        let displays = layout_2x1();
        // (1919, 500) is inside D1; (1920, 500) is inside D2.
        assert_eq!(
            entered_barrier((1919.0, 500.0), (1920.0, 500.0), &displays),
            None
        );
        // And the reverse direction.
        assert_eq!(
            entered_barrier((1920.0, 500.0), (1919.0, 500.0), &displays),
            None
        );
    }

    /// `display_containing` must pick the correct rectangle at the
    /// seam. `1920` is D1.right() (exclusive) and D2.left()
    /// (inclusive); `(1920, 500)` belongs to D2 only.
    #[test]
    fn two_by_one_display_containing_at_seam() {
        let displays = layout_2x1();
        let left = DisplayRect::new(0.0, 0.0, 1920.0, 1080.0);
        let right = DisplayRect::new(1920.0, 0.0, 1920.0, 1080.0);
        assert_eq!(display_containing(&displays, (100.0, 100.0)), Some(&left));
        assert_eq!(display_containing(&displays, (1919.0, 100.0)), Some(&left));
        assert_eq!(display_containing(&displays, (1920.0, 100.0)), Some(&right));
        assert_eq!(display_containing(&displays, (3839.0, 100.0)), Some(&right));
        // Past the right edge of D2 — outside everything.
        assert_eq!(display_containing(&displays, (3840.0, 100.0)), None);
    }

    /// `clamp_to_display_bounds` should clamp to the display that
    /// held `prev_point`, not the union. With prev on D2, a curr at
    /// (5000, 5000) (well outside everything) gets clamped to D2's
    /// inclusive bounds, NOT to D1's or some union bbox corner.
    #[test]
    fn two_by_one_clamp_picks_own_display() {
        let displays = layout_2x1();
        // prev on D2, curr well outside any display → clamped to D2.
        assert_eq!(
            clamp_to_display_bounds(&displays, (2000.0, 500.0), (5000.0, 5000.0)),
            (3839.0, 1079.0)
        );
        // prev on D1, curr well outside any display → clamped to D1.
        assert_eq!(
            clamp_to_display_bounds(&displays, (500.0, 500.0), (-5000.0, -5000.0)),
            (0.0, 0.0)
        );
    }

    // ----- 3x1 horizontal ---------------------------------------------

    /// The seam between the middle (D2) and right (D3) displays
    /// must not register as a barrier crossing either.
    #[test]
    fn three_by_one_seam_motion_does_not_cross() {
        let displays = layout_3x1();
        assert_eq!(
            entered_barrier((3839.0, 500.0), (3840.0, 500.0), &displays),
            None
        );
        assert_eq!(
            entered_barrier((3840.0, 500.0), (3839.0, 500.0), &displays),
            None
        );
    }

    /// Crossing off the rightmost display's right edge → Right.
    #[test]
    fn three_by_one_exit_right_from_rightmost() {
        let displays = layout_3x1();
        assert_eq!(
            entered_barrier((5759.0, 500.0), (5761.0, 500.0), &displays),
            Some(Position::Right)
        );
    }

    /// Crossing off the leftmost display's left edge → Left.
    #[test]
    fn three_by_one_exit_left_from_leftmost() {
        let displays = layout_3x1();
        assert_eq!(
            entered_barrier((0.0, 500.0), (-2.0, 500.0), &displays),
            Some(Position::Left)
        );
    }

    // ----- L-shaped offset (200px) ------------------------------------

    /// Crossing D2's top edge from inside the union (at y = -1080)
    /// to outside (at y = -1081) in the overlap zone x ∈ [200,
    /// 1920) fires Top — the cursor was inside a display and is now
    /// past some display's top edge.
    #[test]
    fn l_shape_overlap_zone_crosses_top() {
        let displays = layout_l_shaped_offset_200px();
        // (500, -1079) is inside D2; (500, -1081) is outside the
        // union entirely. The union's ymin is D2.top = -1080, so
        // this is a real Top crossing.
        assert_eq!(
            entered_barrier((500.0, -1079.0), (500.0, -1081.0), &displays),
            Some(Position::Top)
        );
    }

    /// In the non-overlap strip (x ∈ [0, 200), y = 0), the cursor
    /// moves from inside D1 to a point that is NOT inside D2 — it's
    /// outside the union entirely. The cursor genuinely exited the
    /// union upward, so [`entered_barrier`] fires `Top` (the
    /// direction of motion). This is the correct semantics after
    /// the STEP-DEBUG-D1-LEFT fix: the union-exit + direction-
    /// inference detector attributes the crossing to the actual
    /// side the cursor moved through, not to whatever axis happens
    /// to have a display edge nearby.
    ///
    /// Before the fix this test asserted `None` because
    /// `moved_across_boundary` checked `!in_bounds(curr, Top)`,
    /// which was satisfied (D1.top = 0 ≤ -1, so `in_bounds` for Top
    /// returned true and `!in_bounds` returned false). That
    /// assertion pinned a side effect of the per-axis check, not a
    /// deliberate semantic; under the union-exit + direction-
    /// inference contract the same `prev → curr` motion is a real
    /// upward crossing and fires `Top`.
    ///
    /// Pins the "L 形错位 200px: 错位区穿出时不误触相邻屏" rule from
    /// PLAN §M0 / §M3: the crossing is correctly attributed to D1's
    /// top edge (where the cursor came from), not to D2 (which
    /// doesn't contain `prev` or `curr`).
    #[test]
    fn l_shape_gap_strip_exits_union_top() {
        let displays = layout_l_shaped_offset_200px();
        assert_eq!(
            entered_barrier((100.0, 0.0), (100.0, -1.0), &displays),
            Some(Position::Top)
        );
    }

    /// Crossing D2's exposed top edge from inside D2 (at y = -1080,
    /// x = 2000) to outside (at y = -1081) fires Top — that part of
    /// D2's top edge is genuinely exposed because D1 doesn't cover
    /// it (D1's x stops at 1920).
    #[test]
    fn l_shape_exposed_top_of_upper_display_fires() {
        let displays = layout_l_shaped_offset_200px();
        assert_eq!(
            entered_barrier((2000.0, -1080.0), (2000.0, -1081.0), &displays),
            Some(Position::Top)
        );
    }

    /// `display_containing` must pick D2 (the upper display) for
    /// points inside it, including points that, by union-bbox
    /// arithmetic, would have been considered "inside the union but
    /// not inside D1".
    #[test]
    fn l_shape_display_containing_picks_upper() {
        let displays = layout_l_shaped_offset_200px();
        let upper = DisplayRect::new(200.0, -1080.0, 1920.0, 1080.0);
        // (500, -500) is inside D2 only (D1 is at y in [0, 1080)).
        assert_eq!(display_containing(&displays, (500.0, -500.0)), Some(&upper));
        // Past the upper display's top edge — outside everything.
        assert_eq!(display_containing(&displays, (500.0, -1081.0)), None);
    }

    // ----- mixed 1080p + 4K -------------------------------------------

    /// Crossing the 4K's bottom edge from inside (y = 2159) to
    /// outside (y = 2160) must fire Bottom.
    #[test]
    fn uneven_height_4k_bottom_edge_from_inside_fires() {
        let displays = layout_uneven_height();
        assert_eq!(
            entered_barrier((2000.0, 2159.0), (2000.0, 2161.0), &displays),
            Some(Position::Bottom)
        );
    }

    /// `display_containing` correctly distinguishes the 1080p and
    /// 4K rectangles.
    #[test]
    fn uneven_height_display_containing_picks_4k() {
        let displays = layout_uneven_height();
        let four_k = DisplayRect::new(1920.0, 0.0, 3840.0, 2160.0);
        let ten_eighty = displays[0];
        // (2000, 1500) is inside the 4K only (y > 1080).
        assert_eq!(
            display_containing(&displays, (2000.0, 1500.0)),
            Some(&four_k)
        );
        // (500, 500) is inside the 1080p only (x < 1920).
        assert_eq!(
            display_containing(&displays, (500.0, 500.0)),
            Some(&ten_eighty)
        );
        // (2000, 500) is inside the 4K only — D1's x range stops at
        // 1920, so a point at x=2000 is NOT inside D1.
        assert_eq!(
            display_containing(&displays, (2000.0, 500.0)),
            Some(&four_k)
        );
    }

    // ----- clamp_to_display_bounds fallback --------------------------

    /// When `prev_point` isn't inside any display (e.g. transient
    /// empty display list mid-reconfigure), the clamp is a no-op —
    /// it returns `point` unchanged. The caller is responsible for
    /// not warping to a stale location in that case.
    #[test]
    fn clamp_returns_input_when_prev_outside_all_displays() {
        let displays = layout_2x1();
        assert_eq!(
            clamp_to_display_bounds(&displays, (-5000.0, -5000.0), (100.0, 100.0)),
            (100.0, 100.0)
        );
    }

    // ----- BarrierKey (M1) -------------------------------------------

    /// Default must equal a full-edge, no-monitor key. Existing call
    /// sites that haven't been ported yet can rely on this to keep
    /// their behavior identical to the old `Position`-keyed maps.
    #[test]
    fn barrier_key_default_is_full_edge_no_monitor() {
        let k = BarrierKey::default();
        assert_eq!(k.pos, Position::Left);
        assert!(k.monitor.is_none());
        assert_eq!(k.offset, 0);
        assert_eq!(k.span, 10000);
    }

    /// `from_pos` matches `default()` for the Position half and keeps
    /// monitor/offset/span at their legacy defaults.
    #[test]
    fn barrier_key_from_pos_matches_legacy_defaults() {
        for &pos in &[
            Position::Left,
            Position::Right,
            Position::Top,
            Position::Bottom,
        ] {
            let k = BarrierKey::from_pos(pos);
            assert_eq!(k.pos, pos);
            assert!(k.monitor.is_none());
            assert_eq!(k.offset, 0);
            assert_eq!(k.span, 10000);
        }
    }

    /// BarrierKey must be usable as a HashMap key: equality and hash
    /// are defined for all four field combinations we care about
    /// (the `String` inside `monitor` makes it non-Copy, but it does
    /// implement Clone + Hash + Eq).
    #[test]
    fn barrier_key_eq_and_hash() {
        let a = BarrierKey {
            pos: Position::Top,
            monitor: None,
            offset: 0,
            span: 10000,
        };
        let b = BarrierKey {
            pos: Position::Top,
            monitor: Some("monitor-A".to_string()),
            offset: 2500,
            span: 5000,
        };
        let c = b.clone();
        assert_eq!(a, BarrierKey::from_pos(Position::Top));
        assert_eq!(b, c);
        assert_ne!(a, b);
    }

    // ----- MonitorInfo (M2) -------------------------------------------
    //
    // The on-wire JSON format the frontend expects is the
    // `snake_case` projection: `{"id": "...", "name": "...",
    // "position": [x, y], "size": [w, h], "primary": bool,
    // "scale": f64}`. `serde_json` round-trips are the unit-test
    // guarantee that both the backend producers and the IPC layer
    // speak the same vocabulary as the GUI.

    /// A UTF-8 monitor name (CJK + accented Latin + em-dash) must
    /// survive a JSON round-trip byte-for-byte. Display names from
    /// real hardware routinely contain non-ASCII characters — a
    /// single string carrying both scripts pins the byte-for-byte
    /// fidelity guarantee rather than splitting it across two
    /// smaller tests.
    #[test]
    fn monitor_info_round_trip_utf8_name() {
        let info = MonitorInfo {
            id: "EDID:0x1234abcd".into(),
            // CJK (戴尔 U2723QE — 左), em-dash (—, U+2014), and
            // accented Latin (é, ñ, ü) in one name. macOS reports
            // these verbatim for non-Apple vendor / model strings.
            name: "LG UltraFine 5K áéíóú ñ — 戴尔".into(),
            position: (0, 0),
            size: (2560, 1440),
            primary: true,
            scale: 1.0,
        };
        let json = serde_json::to_string(&info).unwrap();
        let back: MonitorInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(info, back);
        // Pin the wire shape so any rename_all drift is caught.
        assert!(json.contains("\"id\":\"EDID:0x1234abcd\""));
        assert!(json.contains("\"primary\":true"));
        // Explicit byte-fidelity assertion on each non-ASCII fragment.
        for needle in ["áéíóú", "ñ", "戴尔", "—"] {
            assert!(
                json.contains(needle),
                "utf-8 fragment {needle:?} lost during serde_json round-trip"
            );
        }
    }

    /// A 2x1 horizontal layout's right display has `position.x ==
    /// 1920` (positive). A vertical pair where the primary sits on
    /// the *bottom* puts the top display at `position.y == -1080`.
    /// Signed round-trip on `position` is the contract the rest of
    /// the plan relies on for "where is this thing in the union".
    #[test]
    fn monitor_info_round_trip_negative() {
        let info = MonitorInfo {
            id: "wl_output:DP-2".into(),
            name: "Top display (above primary)".into(),
            position: (-1920, -1080),
            size: (1920, 1080),
            primary: false,
            scale: 2.0,
        };
        let json = serde_json::to_string(&info).unwrap();
        let back: MonitorInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(info, back);
        assert_eq!(back.position, (-1920, -1080));
    }

    /// Mixed-DPI hosts (macOS Retina next to an external 1080p,
    /// Windows with per-monitor v2 awareness) report fractional
    /// scale factors. The f64 round-trip must keep the exact value.
    #[test]
    fn monitor_info_round_trip_mixed_scale() {
        let info = MonitorInfo {
            id: "CGDisplay:0x4271a80".into(),
            name: "Built-in Retina Display".into(),
            position: (0, 0),
            size: (1440, 900),
            primary: true,
            scale: 2.0,
        };
        let one_point_five = MonitorInfo {
            id: "wl_output:HDMI-A-1".into(),
            name: "External 1080p (125%)".into(),
            position: (1440, 0),
            size: (1920, 1080),
            primary: false,
            scale: 1.25,
        };
        for original in [info, one_point_five] {
            let json = serde_json::to_string(&original).unwrap();
            let back: MonitorInfo = serde_json::from_str(&json).unwrap();
            assert_eq!(original, back);
        }
    }

    // ----- crossed_pure 4x2 matrix (M3 STEP-3.4) -----------------------
    //
    // The four-by-two matrix the PLAN calls out for STEP-3.4
    // (C3 updated by STEP-M3-3.4-FIXUP for the H1 legacy fallback):
    //
    //   | prev_pos              | curr_pos              | active key(s)                             | expected     |
    //   |-----------------------|----------------------|--------------------------------------------|--------------|
    //   | C1: display_0 center  | top of union         | monitor: Some(d0.id), pos: Top           | hit (d0)     |
    //   | C2: display_1 center  | top of union         | monitor: Some(d1.id), pos: Top           | hit (d1)     |
    //   | C3: display_0 center  | top of union         | monitor: None, pos: Top  (legacy only)   | hit (legacy) |
    //   | C4a: seam (1920,540)  | top of union         | monitor: Some(d0.id), pos: Top           | hit (d0)     |
    //   | C4b: seam (1920,540)  | top of union         | monitor: Some(d1.id), pos: Top           | miss          |
    //   | C5: outside all       | top of union         | monitor: Some(d0.id), pos: Top           | miss          |
    //   | C6: display_0 center  | top of union         | d0.Top + d1.Top                           | hit (d0 only)|
    //   | C7: display_0 right   | right of union       | monitor: Some(d0.id), pos: Right         | hit (d0)     |
    //   | C8: display_1 left    | left of union        | monitor: Some(d1.id), pos: Left          | hit (d1)     |
    //
    // Every case asserts the query `BarrierKey` carries the correct
    // `monitor` field — i.e. NOT hard-coded `None` like the pre-3.4
    // `crossed()` did. The test is the regression guard for the M3
    // "100% edge miss" bug.

    /// 2x1 horizontal layout as `DisplayBound` (the M3 backend
    /// representation). Returns the two displays with stable ids
    /// `d0` (left) and `d1` (right) — both as `Some(...)`.
    fn layout_2x1_bound() -> Vec<DisplayBound> {
        vec![
            DisplayBound::new(
                DisplayRect::new(0.0, 0.0, 1920.0, 1080.0),
                Some("macos:d0".into()),
            ),
            DisplayBound::new(
                DisplayRect::new(1920.0, 0.0, 1920.0, 1080.0),
                Some("macos:d1".into()),
            ),
        ]
    }

    /// C1: prev in display_0 center, curr crosses the union's top.
    /// Active contains `Top @ d0.id` → hit with that key.
    #[test]
    fn crossed_pure_c1_display0_top_hit() {
        let displays = layout_2x1_bound();
        let mut active = HashSet::new();
        active.insert(BarrierKey {
            pos: Position::Top,
            monitor: Some("macos:d0".into()),
            offset: 0,
            span: 10000,
        });
        let got = crossed_pure((500.0, 500.0), (500.0, -2.0), &displays, &active);
        assert_eq!(
            got,
            Some(BarrierKey {
                pos: Position::Top,
                monitor: Some("macos:d0".into()),
                offset: 0,
                span: 10000,
            })
        );
    }

    /// C2: prev in display_1 center, curr crosses the union's top.
    /// Active contains `Top @ d1.id` → hit with that key.
    #[test]
    fn crossed_pure_c2_display1_top_hit() {
        let displays = layout_2x1_bound();
        let mut active = HashSet::new();
        active.insert(BarrierKey {
            pos: Position::Top,
            monitor: Some("macos:d1".into()),
            offset: 0,
            span: 10000,
        });
        let got = crossed_pure((2500.0, 500.0), (2500.0, -2.0), &displays, &active);
        assert_eq!(
            got,
            Some(BarrierKey {
                pos: Position::Top,
                monitor: Some("macos:d1".into()),
                offset: 0,
                span: 10000,
            })
        );
    }

    /// C3 (post-H1 / STEP-M3-3.4-FIXUP): prev in display_0 center,
    /// curr crosses the union's top. Active contains ONLY
    /// `Top @ None` (the legacy client shape — typical of legacy
    /// configs / dropdown not yet picked).
    ///
    /// Pre-H1 this case returned `None` because `query_pure`
    /// constructed a query key with `monitor: Some("macos:d0")`
    /// that never matched the legacy `monitor: None` entry — the
    /// 100% silent edge-miss bug STEP-DEBUG-M3-BARRIER-CHAIN
    /// §3 H1 documents.
    ///
    /// Post-H1 the legacy fallback in `query_pure` probes
    /// `monitor: None` after the specific-key miss and finds the
    /// hit → returns the legacy key. This restores pre-M3.4
    /// behavior for legacy-config clients.
    ///
    /// The test name's "misses" prefix is a STEP-3.4-era artifact
    /// (when the fixture asserted the buggy behavior); kept for
    /// backwards compat with the C1-C8 matrix indexing in this
    /// module's docstring. Companion test
    /// `query_pure_falls_back_to_legacy_when_active_has_monitor_none`
    /// pins the same scenario at the private-helper level.
    #[test]
    fn crossed_pure_c3_display0_top_misses_legacy_active() {
        let displays = layout_2x1_bound();
        let mut active = HashSet::new();
        // Legacy single-edge client (M3 not yet selected a monitor)
        active.insert(BarrierKey {
            pos: Position::Top,
            monitor: None,
            offset: 0,
            span: 10000,
        });
        let got = crossed_pure((500.0, 500.0), (500.0, -2.0), &displays, &active);
        assert_eq!(
            got,
            Some(BarrierKey {
                pos: Position::Top,
                monitor: None,
                offset: 0,
                span: 10000,
            })
        );
    }

    /// C4a: prev at the seam `(1920.0, 540.0)` — under the
    /// half-open convention that point belongs to display_1 (idx 1)
    /// because display_0's right edge is exclusive (`x == 1920.0`
    /// is NOT inside display_0). Crossing top from that point
    /// queries with `monitor: Some("macos:d1")`. Active contains
    /// d0's Top → miss.
    ///
    /// Mirrors PLAN §3 STEP-3.4 C4: "prev 在接缝 → 归到 d1;
    /// active 含 d0 → miss".
    #[test]
    fn crossed_pure_c4a_seam_top_queries_d1() {
        let displays = layout_2x1_bound();
        let mut active = HashSet::new();
        active.insert(BarrierKey {
            pos: Position::Top,
            monitor: Some("macos:d0".into()),
            offset: 0,
            span: 10000,
        });
        // (1920.0, 540.0) is the seam point — strictly outside d0
        // (right edge exclusive) and inside d1 (left edge inclusive).
        let got = crossed_pure((1920.0, 540.0), (1920.0, -2.0), &displays, &active);
        // d0's key won't match because query is d1; miss.
        assert_eq!(got, None);
    }

    /// C4b: same seam, active contains d1's Top → hit with d1.
    #[test]
    fn crossed_pure_c4b_seam_top_hit_with_d1() {
        let displays = layout_2x1_bound();
        let mut active = HashSet::new();
        active.insert(BarrierKey {
            pos: Position::Top,
            monitor: Some("macos:d1".into()),
            offset: 0,
            span: 10000,
        });
        let got = crossed_pure((1920.0, 540.0), (1920.0, -2.0), &displays, &active);
        assert_eq!(
            got,
            Some(BarrierKey {
                pos: Position::Top,
                monitor: Some("macos:d1".into()),
                offset: 0,
                span: 10000,
            })
        );
    }

    /// C5: prev outside every display → entered_barrier returns
    /// None → directly miss.
    #[test]
    fn crossed_pure_c5_off_screen_is_miss() {
        let displays = layout_2x1_bound();
        let mut active = HashSet::new();
        active.insert(BarrierKey {
            pos: Position::Top,
            monitor: Some("macos:d0".into()),
            offset: 0,
            span: 10000,
        });
        // (-100.0, 500.0) is outside both displays; crossing to
        // (-100.0, -2.0) does not look like a "barrier crossing" in
        // the half-open sense (prev was already outside the union).
        let got = crossed_pure((-100.0, 500.0), (-100.0, -2.0), &displays, &active);
        assert_eq!(got, None);
    }

    /// C6: prev in display_0, curr crosses top. Active contains
    /// BOTH d0.Top and d1.Top. The query is d0.Top, so only d0's
    /// key matches → hit with d0.
    #[test]
    fn crossed_pure_c6_display0_top_picks_d0_over_d1() {
        let displays = layout_2x1_bound();
        let mut active = HashSet::new();
        active.insert(BarrierKey {
            pos: Position::Top,
            monitor: Some("macos:d0".into()),
            offset: 0,
            span: 10000,
        });
        active.insert(BarrierKey {
            pos: Position::Top,
            monitor: Some("macos:d1".into()),
            offset: 0,
            span: 10000,
        });
        let got = crossed_pure((500.0, 500.0), (500.0, -2.0), &displays, &active);
        assert_eq!(
            got,
            Some(BarrierKey {
                pos: Position::Top,
                monitor: Some("macos:d0".into()),
                offset: 0,
                span: 10000,
            })
        );
    }

    /// C7: 2x1 right-cross from display_0. prev at the right edge
    /// of display_0 (x = 1919.999), curr past the union's right
    /// edge (x = 3841). Active contains `Right @ d0.id` → hit.
    #[test]
    fn crossed_pure_c7_display0_right_hit() {
        let displays = layout_2x1_bound();
        let mut active = HashSet::new();
        active.insert(BarrierKey {
            pos: Position::Right,
            monitor: Some("macos:d0".into()),
            offset: 0,
            span: 10000,
        });
        // Use a point that's clearly inside d0 (x < 1920.0) so the
        // half-open containment picks d0.
        let got = crossed_pure((1900.0, 500.0), (3841.0, 500.0), &displays, &active);
        assert_eq!(
            got,
            Some(BarrierKey {
                pos: Position::Right,
                monitor: Some("macos:d0".into()),
                offset: 0,
                span: 10000,
            })
        );
    }

    /// C8: mirror of C7. prev inside display_1 (x > 1920.0), curr
    /// crosses the union's left edge. Active contains
    /// `Left @ d1.id` → hit. Also assert that d0's Left key does
    /// NOT match (we'd hit a different display's query).
    #[test]
    fn crossed_pure_c8_display1_left_hit() {
        let displays = layout_2x1_bound();
        let mut active = HashSet::new();
        // d0's Left key — should NOT match because query is d1.
        active.insert(BarrierKey {
            pos: Position::Left,
            monitor: Some("macos:d0".into()),
            offset: 0,
            span: 10000,
        });
        // d1's Left key — the one that should match.
        active.insert(BarrierKey {
            pos: Position::Left,
            monitor: Some("macos:d1".into()),
            offset: 0,
            span: 10000,
        });
        // (3000.0, 500.0) is inside display_1; (-2.0, 500.0) is
        // outside the union on the left.
        let got = crossed_pure((3000.0, 500.0), (-2.0, 500.0), &displays, &active);
        assert_eq!(
            got,
            Some(BarrierKey {
                pos: Position::Left,
                monitor: Some("macos:d1".into()),
                offset: 0,
                span: 10000,
            })
        );
    }

    /// Belt-and-braces: `monitor_id = None` on the containing
    /// display falls through to the legacy "monitor-agnostic"
    /// lookup. Pins the contract that callers don't have to set
    /// `monitor_id` during transient state.
    #[test]
    fn crossed_pure_monitor_id_none_uses_legacy_key() {
        let displays = vec![DisplayBound::new(
            DisplayRect::new(0.0, 0.0, 1920.0, 1080.0),
            None, // monitor_id unset
        )];
        let mut active = HashSet::new();
        active.insert(BarrierKey {
            pos: Position::Top,
            monitor: None,
            offset: 0,
            span: 10000,
        });
        let got = crossed_pure((500.0, 500.0), (500.0, -2.0), &displays, &active);
        assert_eq!(
            got,
            Some(BarrierKey {
                pos: Position::Top,
                monitor: None,
                offset: 0,
                span: 10000,
            })
        );
    }

    /// Production-realistic regression guard for the H1 legacy
    /// fallback (STEP-M3-3.4-FIXUP / STEP-DEBUG-M3-BARRIER-CHAIN
    /// §3 H1).
    ///
    /// Fixture matches what the per-OS backends actually produce:
    /// - `displays` carries `Some(macos:d0)` / `Some(macos:d1)` —
    ///   exactly what `build_display_bounds` (macOS) and
    ///   `update_display_regions` (Windows) emit. The vacuous
    ///   `crossed_pure_monitor_id_none_uses_legacy_key` test
    ///   above uses `None` here, but no production backend ever
    ///   produces a `monitor_id = None` DisplayBound.
    /// - `active` contains only a legacy `monitor: None` entry —
    ///   exactly what `ClientConfig.monitor = None` writes into
    ///   the active set (the default for legacy configs / dropdown
    ///   not yet picked).
    ///
    /// Pre-H1 this returned `None`: the query key
    /// `{ pos: Top, monitor: Some("macos:d0"), ... }` never
    /// matched the legacy `monitor: None` entry, so every legacy
    /// client's barrier trigger silently disappeared (the 100%
    /// edge-miss bug). Post-H1 the legacy fallback in `query_pure`
    /// probes `monitor: None` and returns the legacy key.
    ///
    /// Calls `query_pure` directly (it's a private helper in this
    /// module) so the fallback path is exercised even if the
    /// public `crossed_pure` / `activation_pure` wrappers change.
    /// The companion tests `crossed_pure_c3` and
    /// `activation_pure_w3` pin the same behavior at the public
    /// entry-point level.
    #[test]
    fn query_pure_falls_back_to_legacy_when_active_has_monitor_none() {
        // Production-realistic fixture: both displays carry
        // `Some(macos:...)` monitor_ids, just like
        // `build_display_bounds` produces in production.
        let displays = layout_2x1_bound();
        let mut active = HashSet::new();
        // Production-realistic client: legacy config, monitor not
        // picked. This is the default `ClientConfig.monitor = None`
        // path for users on a legacy config or who haven't yet
        // selected a monitor in the dropdown.
        active.insert(BarrierKey {
            pos: Position::Top,
            monitor: None,
            offset: 0,
            span: 10000,
        });
        // Mouse moves from inside display_0 to past the union's
        // top edge. Pre-H1 this returned `None`; post-H1 it must
        // return the legacy key.
        let got = query_pure((500.0, 500.0), (500.0, -2.0), &displays, &active);
        assert_eq!(
            got,
            Some(BarrierKey {
                pos: Position::Top,
                monitor: None,
                offset: 0,
                span: 10000,
            }),
            "query_pure must fall back to legacy `monitor: None` key \
             when specific_key misses but active has a legacy entry"
        );
    }

    // ----- activation_pure 4x2 matrix (M3 STEP-3.5) -------------------
    //
    // The Windows `check_client_activation` analog. Mirrors the
    // `crossed_pure` 4x2 matrix but uses `windows:` ids instead of
    // `macos:` and a tighter 6-case selection (Windows only
    // exercises prev_pos in display_0 / display_1 / outside ×
    // active `monitor: Some / None`; the 2-display intra-set and
    // seam cases share the same code path and are already covered
    // by `crossed_pure_c6` / `c4a` / `c4b`).
    //
    // W3 updated by STEP-M3-3.4-FIXUP for the H1 legacy fallback.
    //
    //   | prev_pos              | curr_pos              | active key(s)                | expected     |
    //   |-----------------------|-----------------------|------------------------------|--------------|
    //   | W1: display_0 center  | top of union          | monitor: Some(d0.id), Top    | hit (d0)     |
    //   | W2: display_1 center  | top of union          | monitor: Some(d1.id), Top    | hit (d1)     |
    //   | W3: display_0 center  | top of union          | monitor: None, Top (legacy)  | hit (legacy) |
    //   | W4: display_0 right   | right of union        | monitor: Some(d0.id), Right  | hit (d0)     |
    //   | W5: outside all       | top of union          | monitor: Some(d0.id), Top    | miss         |
    //   | W6: display_0 center  | top of union          | monitor: None, Top + d0.Top  | hit (d0)     |
    //
    // Every case asserts the query `BarrierKey` carries the correct
    // `monitor` field — i.e. NOT hard-coded `None` like the pre-3.5
    // `check_client_activation` did. The test is the regression
    // guard for the Windows "100% edge miss" bug.

    /// 2x1 horizontal layout as `DisplayBound` with Windows-style
    /// ids (`windows:…`). Returns the two displays with stable ids
    /// `d0` (left) and `d1` (right).
    fn layout_2x1_bound_windows() -> Vec<DisplayBound> {
        vec![
            DisplayBound::new(
                DisplayRect::new(0.0, 0.0, 1920.0, 1080.0),
                Some(r"windows:MONITOR\GSM5B23\{abc-123}".into()),
            ),
            DisplayBound::new(
                DisplayRect::new(1920.0, 0.0, 1920.0, 1080.0),
                Some(r"windows:MONITOR\GSM5B23\{def-456}".into()),
            ),
        ]
    }

    /// W1: prev in display_0 center, curr crosses the union's top.
    /// Active contains `Top @ d0.id` → hit with that key.
    #[test]
    fn activation_pure_w1_display0_top_hit() {
        let displays = layout_2x1_bound_windows();
        let mut clients = HashSet::new();
        clients.insert(BarrierKey {
            pos: Position::Top,
            monitor: Some(r"windows:MONITOR\GSM5B23\{abc-123}".into()),
            offset: 0,
            span: 10000,
        });
        let got = activation_pure((500.0, 500.0), (500.0, -2.0), &displays, &clients);
        assert_eq!(
            got,
            Some(BarrierKey {
                pos: Position::Top,
                monitor: Some(r"windows:MONITOR\GSM5B23\{abc-123}".into()),
                offset: 0,
                span: 10000,
            })
        );
    }

    /// W2: prev in display_1 center, curr crosses the union's top.
    /// Active contains `Top @ d1.id` → hit with that key.
    #[test]
    fn activation_pure_w2_display1_top_hit() {
        let displays = layout_2x1_bound_windows();
        let mut clients = HashSet::new();
        clients.insert(BarrierKey {
            pos: Position::Top,
            monitor: Some(r"windows:MONITOR\GSM5B23\{def-456}".into()),
            offset: 0,
            span: 10000,
        });
        let got = activation_pure((2500.0, 500.0), (2500.0, -2.0), &displays, &clients);
        assert_eq!(
            got,
            Some(BarrierKey {
                pos: Position::Top,
                monitor: Some(r"windows:MONITOR\GSM5B23\{def-456}".into()),
                offset: 0,
                span: 10000,
            })
        );
    }

    /// W3 (post-H1 / STEP-M3-3.4-FIXUP): mirror of
    /// `crossed_pure_c3` for the Windows `activation_pure` path.
    /// Clients contains ONLY `Top @ None` (legacy). Pre-H1 this
    /// returned `None` because the Windows-side `query_pure`
    /// constructed `monitor: Some("windows:...")` that never
    /// matched the legacy entry. Post-H1 the legacy fallback hits
    /// the `monitor: None` entry → returns the legacy key.
    ///
    /// Test name kept for matrix-index compat (`activation_pure_w1-w6`
    /// in module docstring); the "misses" prefix is a STEP-3.5-era
    /// artifact.
    #[test]
    fn activation_pure_w3_display0_top_misses_legacy_clients() {
        let displays = layout_2x1_bound_windows();
        let mut clients = HashSet::new();
        // Legacy single-edge client (M3 not yet selected a monitor)
        clients.insert(BarrierKey {
            pos: Position::Top,
            monitor: None,
            offset: 0,
            span: 10000,
        });
        let got = activation_pure((500.0, 500.0), (500.0, -2.0), &displays, &clients);
        assert_eq!(
            got,
            Some(BarrierKey {
                pos: Position::Top,
                monitor: None,
                offset: 0,
                span: 10000,
            })
        );
    }

    /// W4: 2x1 right-cross from display_0. prev inside d0, curr
    /// past the union's right edge. Active contains
    /// `Right @ d0.id` → hit.
    #[test]
    fn activation_pure_w4_display0_right_hit() {
        let displays = layout_2x1_bound_windows();
        let mut clients = HashSet::new();
        clients.insert(BarrierKey {
            pos: Position::Right,
            monitor: Some(r"windows:MONITOR\GSM5B23\{abc-123}".into()),
            offset: 0,
            span: 10000,
        });
        let got = activation_pure((1900.0, 500.0), (3841.0, 500.0), &displays, &clients);
        assert_eq!(
            got,
            Some(BarrierKey {
                pos: Position::Right,
                monitor: Some(r"windows:MONITOR\GSM5B23\{abc-123}".into()),
                offset: 0,
                span: 10000,
            })
        );
    }

    /// W5: prev outside every display → entered_barrier returns
    /// None → directly miss. Mirrors `crossed_pure_c5`.
    #[test]
    fn activation_pure_w5_off_screen_is_miss() {
        let displays = layout_2x1_bound_windows();
        let mut clients = HashSet::new();
        clients.insert(BarrierKey {
            pos: Position::Top,
            monitor: Some(r"windows:MONITOR\GSM5B23\{abc-123}".into()),
            offset: 0,
            span: 10000,
        });
        let got = activation_pure((-100.0, 500.0), (-100.0, -2.0), &displays, &clients);
        assert_eq!(got, None);
    }

    /// W6: prev in display_0, curr crosses top. Active contains
    /// BOTH `monitor: None` Top (legacy) and `d0.id` Top (M3). The
    /// query is d0.Top, so only d0's key matches → hit with d0.
    /// Pins that the `monitor: Some` key always beats the legacy
    /// `monitor: None` key when both are present (the M3 dropdown
    /// "I want this monitor specifically" intent wins).
    #[test]
    fn activation_pure_w6_display0_top_picks_d0_over_none() {
        let displays = layout_2x1_bound_windows();
        let mut clients = HashSet::new();
        // Legacy single-edge client (M3 not yet selected a monitor)
        clients.insert(BarrierKey {
            pos: Position::Top,
            monitor: None,
            offset: 0,
            span: 10000,
        });
        // M3 dropdown picked d0 specifically.
        clients.insert(BarrierKey {
            pos: Position::Top,
            monitor: Some(r"windows:MONITOR\GSM5B23\{abc-123}".into()),
            offset: 0,
            span: 10000,
        });
        let got = activation_pure((500.0, 500.0), (500.0, -2.0), &displays, &clients);
        assert_eq!(
            got,
            Some(BarrierKey {
                pos: Position::Top,
                monitor: Some(r"windows:MONITOR\GSM5B23\{abc-123}".into()),
                offset: 0,
                span: 10000,
            })
        );
    }

    // ----- macOS dual-monitor D1.Left crossing regression -------------
    //
    // User-reported scenario (2026-09-08, STEP-DEBUG-D1-LEFT):
    //   D1 (built-in Retina): pos=(0,0) size=(1512,982)   primary=true
    //   D3 (external):       pos=(-959,-1440) size=(3440,1440) primary=false
    //
    // D3 sits in the upper-left of D1 (D3.x ∈ [-959, 2481] fully
    // covers D1.x ∈ [0, 1512]; D3.y ∈ [-1440, 0] sits above D1.y
    // ∈ [0, 982]). The two displays share the y=0 seam.
    //
    // Configured: client monitor = Display1, side = Left.
    //
    // Before the STEP-M3-3.4-FIXUP3 fix: cursor pushed from (1, 491)
    // to (-1, 491) — past D1's left edge — silently did NOT trigger.
    // `moved_across_boundary` checked `!in_bounds(curr, Left)`,
    // which returned false because D3's left edge (-959) sits to
    // the left of -1 (so the point was "inside" D3's left boundary
    // on the single-axis check, even though the point is not inside
    // D3 at all). Same mask applied for Right and Top. `entered_barrier`
    // iterated all four sides, all returned false, function returned
    // `None` — and `query_pure` early-returned on the `?`. 100% edge
    // miss, no log on either side.
    //
    // After the fix: `moved_across_boundary` does a union-exit
    // check (`prev` inside, `curr` outside) plus direction-of-
    // motion inference from `(curr - prev)`. The cursor's
    // leftward motion is correctly attributed to `Left`.
    //
    // These tests are the regression guard for that scenario —
    // see STEP-M3-3.4-FIXUP3 for the full investigation trace.

    /// User's exact D1/D3 layout. D1 origin (0,0), D3 origin
    /// (-959,-1440). Both rects use the reported monitor-info
    /// numbers verbatim.
    fn user_macos_dual_layout() -> Vec<DisplayRect> {
        vec![
            DisplayRect::new(0.0, 0.0, 1512.0, 982.0), // D1 (built-in)
            DisplayRect::new(-959.0, -1440.0, 3440.0, 1440.0), // D3 (external)
        ]
    }

    /// `entered_barrier` fires `Left` when the cursor exits D1's
    /// left edge in the user's macOS dual-monitor layout. The
    /// original bug was that D3's left boundary at x=-959 is
    /// far to the left of D1's left boundary at x=0, so the
    /// per-axis `in_bounds(curr, Left)` was satisfied by D3 even
    /// though `curr = (-1, 491)` is geometrically outside both
    /// displays on the left.
    #[test]
    fn d1_left_crossing_to_outer_display_fires_left() {
        let displays = user_macos_dual_layout();
        let got = entered_barrier((1.0, 491.0), (-1.0, 491.0), &displays);
        assert_eq!(
            got,
            Some(Position::Left),
            "expected Left barrier when crossing D1's left edge in \
             macOS dual layout, got {got:?}"
        );
    }

    /// Symmetric right-edge check: (1, 491) → (1513, 491) past D1's
    /// right edge. Fires `Right`. Pins the same union-exit +
    /// direction-inference fix on the right side; D3's right
    /// boundary at x=2481 similarly masks D1's right boundary at
    /// x=1512 in the pre-fix code.
    #[test]
    fn d1_right_crossing_to_outer_display_fires_right() {
        let displays = user_macos_dual_layout();
        let got = entered_barrier((1.0, 491.0), (1513.0, 491.0), &displays);
        assert_eq!(
            got,
            Some(Position::Right),
            "expected Right barrier when crossing D1's right edge \
             in macOS dual layout, got {got:?}"
        );
    }

    /// Positive control: D3's *exposed* left edge (D3.left = -959)
    /// sits in a region where no other display's left boundary
    /// extends past it, so the leftward exit fires regardless of
    /// which detector variant is in use. This is what the user
    /// reported as "configure client monitor=Display3 → works".
    ///
    /// Pins that the fix did not regress the outer-display path
    /// (the path the user said already worked).
    #[test]
    fn d3_left_crossing_fires_left_as_baseline() {
        let displays = user_macos_dual_layout();
        // (-958, -100) is inside D3 (x ∈ [-959, 2481], y ∈
        // [-1440, 0]). (-961, -100) is outside both displays on
        // the left.
        let got = entered_barrier((-958.0, -100.0), (-961.0, -100.0), &displays);
        assert_eq!(
            got,
            Some(Position::Left),
            "expected Left barrier when crossing D3's left edge in \
             macOS dual layout (positive control), got {got:?}"
        );
    }

    /// End-to-end `query_pure` with the user's macOS monitor ids.
    /// The active set contains `Left @ D1.id`. The cursor exits
    /// D1's left edge into the gap. The query hits with D1's key.
    /// Pins the full chain that was previously broken:
    /// `moved_across_boundary` → `entered_barrier` → `query_pure`.
    #[test]
    fn query_pure_d1_left_hit_in_outer_display_layout() {
        let displays = vec![
            DisplayBound::new(
                DisplayRect::new(0.0, 0.0, 1512.0, 982.0),
                Some("macos:0000:0000::unknown-1".into()),
            ),
            DisplayBound::new(
                DisplayRect::new(-959.0, -1440.0, 3440.0, 1440.0),
                Some("macos:0000:0000::unknown-3".into()),
            ),
        ];
        let mut active = HashSet::new();
        active.insert(BarrierKey {
            pos: Position::Left,
            monitor: Some("macos:0000:0000::unknown-1".into()),
            offset: 0,
            span: 10000,
        });
        let got = query_pure((1.0, 491.0), (-1.0, 491.0), &displays, &active);
        assert_eq!(
            got,
            Some(BarrierKey {
                pos: Position::Left,
                monitor: Some("macos:0000:0000::unknown-1".into()),
                offset: 0,
                span: 10000,
            }),
            "expected query_pure to hit D1.Left in macOS dual \
             layout, got {got:?}"
        );
    }

    /// `display_containing_idx` sanity: the prev_pos (1, 491) is
    /// inside D1 only (not D3, because D3's y range excludes 491).
    /// Confirms the idx path used by `query_pure` to look up
    /// `monitor_id` is sane for the user's layout.
    #[test]
    fn display_containing_idx_picks_d1_in_outer_display_layout() {
        let displays = user_macos_dual_layout();
        let idx = display_containing_idx(&displays, (1.0, 491.0));
        assert_eq!(
            idx,
            Some(0),
            "expected prev (1,491) to belong to D1 (idx 0), \
             got {idx:?}"
        );
        let idx_curr = display_containing_idx(&displays, (-1.0, 491.0));
        assert_eq!(
            idx_curr, None,
            "expected curr (-1,491) to be outside both displays, \
             got {idx_curr:?}"
        );
    }

    /// `in_bounds` per-side truth table for the user layout. Pins
    /// the per-axis masking behavior that made the pre-fix
    /// detector unreliable: `in_bounds` returns true for `Left`,
    /// `Right`, `Top`, and `Bottom` even when the point is
    /// geometrically outside every display, because each axis is
    /// checked independently against *some* display's range.
    ///
    /// For curr = (-1, 491) in the user layout:
    /// - `Left`: D3.left = -959 ≤ -1 → true (D3 satisfies).
    /// - `Right`: D3.right = 2481 > -1 → true (D3 satisfies).
    /// - `Top`: D1.top = 0 ≤ 491 → true (D1 satisfies; D3.top =
    ///   -1440 ≤ 491 also true).
    /// - `Bottom`: D1.bottom = 982 > 491 → true (D1 satisfies;
    ///   D3.bottom = 0 > 491 is false).
    ///
    /// Kept as a permanent regression: if anyone reintroduces
    /// `moved_across_boundary = in_display_region(prev) &&
    /// !in_bounds(curr, pos)` (the buggy shape), this truth
    /// table — together with `d1_left_crossing_to_outer_display_fires_left`
    /// — is the smoking gun.
    #[test]
    fn in_bounds_per_side_truth_table_for_outer_display_layout() {
        let displays = user_macos_dual_layout();
        assert!(
            in_bounds((-1.0, 491.0), &displays, Position::Left),
            "D3.left=-959 < -1, so in_bounds for Left is true \
             (this is the per-axis masking that motivated the fix)"
        );
        assert!(
            in_bounds((-1.0, 491.0), &displays, Position::Right),
            "D3.right=2481 > -1, so in_bounds for Right is true"
        );
        assert!(
            in_bounds((-1.0, 491.0), &displays, Position::Top),
            "D1.top=0 ≤ 491, so in_bounds for Top is true"
        );
        assert!(
            in_bounds((-1.0, 491.0), &displays, Position::Bottom),
            "D1.bottom=982 > 491, so in_bounds for Bottom is true"
        );
    }

    // ----- vertically stacked monitors, bottom-edge regression --------
    //
    // User-reported scenario (2026-09-08, STEP-DEBUG-D1-BOTTOM):
    //   D1 (bottom): pos=(0,0)    size=(1920,1080) primary=true
    //   D2 (top):    pos=(0,-1080) size=(1920,1080) primary=false
    //
    // Two monitors stacked vertically; primary (D1) on the bottom,
    // secondary (D2) above it on the same x range. Configured:
    // client monitor = Display1 (bottom), side = Left.
    //
    // User reported four behaviors, of which three were as expected:
    //   1. cursor leaves D1's LEFT edge → cross to controlled machine ✓
    //   2. cursor leaves D1's RIGHT edge → no crossing ✓
    //   3. cursor leaves D1's TOP edge (y=0 seam) → moves into D2,
    //      which is local, not a barrier ✓
    //   4. cursor leaves D1's BOTTOM edge → BUG: ALSO crossed to
    //      the controlled machine (should NOT cross — no neighbor on
    //      bottom).
    //
    // Pre-fix root cause: `moved_across_boundary` decided sides by
    // sign of `dx`/`dy` alone. When the cursor exited D1's bottom-
    // left corner with mostly-downward motion (e.g. (100, 1075) →
    // (99, 1081), dx=-1, dy=+6) BOTH `Left` (dx < 0) and `Bottom`
    // (dy > 0) returned true. `entered_barrier`'s priority list
    // `[Left, Right, Top, Bottom]` then picked `Left` first, the
    // query looked up `Left` in the registered clients, and found
    // the controlled machine — incorrectly attributing the diagonal
    // exit to the horizontal edge.
    //
    // Post-fix: `moved_across_boundary` requires the relevant axis
    // to dominate (`|dx| >= |dy|` for Left/Right, `|dy| >= |dx|`
    // for Top/Bottom). In the same example, `|dy|=6 > |dx|=1` so
    // `Left` returns false and `Bottom` returns true → no spurious
    // crossing.
    //
    // These tests are the regression guard for that scenario —
    // see STEP-DEBUG-D1-BOTTOM for the full investigation trace.

    /// User's exact vertical-pair layout. D1 origin (0,0), D2
    /// origin (0, -1080). Both 1080p, same x range.
    fn user_vertical_pair_layout() -> Vec<DisplayRect> {
        vec![
            DisplayRect::new(0.0, 0.0, 1920.0, 1080.0),    // D1 (bottom / primary)
            DisplayRect::new(0.0, -1080.0, 1920.0, 1080.0), // D2 (top)
        ]
    }

    /// Cursor pushed off the LEFT edge of D1 (purely horizontal,
    /// dy=0): `entered_barrier` must fire `Left`. The registered
    /// clients contain a `Left` key for D1, so this triggers a
    /// crossing to the controlled machine (positive control —
    /// already worked before the fix).
    #[test]
    fn vertical_pair_d1_left_pure_horizontal_fires_left() {
        let displays = user_vertical_pair_layout();
        let got = entered_barrier((1.0, 500.0), (-1.0, 500.0), &displays);
        assert_eq!(
            got,
            Some(Position::Left),
            "expected Left barrier when exiting D1's left edge in \
             vertical-pair layout, got {got:?}"
        );
    }

    /// Cursor pushed off the RIGHT edge of D1: no neighbor is
    /// registered on the right side, but the function itself still
    /// fires `Right` (the registered-clients lookup downstream is
    /// what suppresses the actual crossing).
    #[test]
    fn vertical_pair_d1_right_pure_horizontal_fires_right() {
        let displays = user_vertical_pair_layout();
        let got = entered_barrier((1919.0, 500.0), (1921.0, 500.0), &displays);
        assert_eq!(
            got,
            Some(Position::Right),
            "expected Right barrier when exiting D1's right edge in \
             vertical-pair layout, got {got:?}"
        );
    }

    /// Cursor pushed off the BOTTOM edge of D1 (purely vertical,
    /// dx=0): `entered_barrier` must fire `Bottom`. There's no
    /// Bottom neighbor configured, so the downstream lookup yields
    /// no crossing. This case alone was already correct before the
    /// fix — the bug only surfaced on the diagonal motion below.
    #[test]
    fn vertical_pair_d1_bottom_pure_vertical_fires_bottom() {
        let displays = user_vertical_pair_layout();
        let got = entered_barrier((500.0, 1079.0), (500.0, 1081.0), &displays);
        assert_eq!(
            got,
            Some(Position::Bottom),
            "expected Bottom barrier when exiting D1's bottom edge \
             in vertical-pair layout, got {got:?}"
        );
    }

    /// Cursor pushed off the TOP edge of D1 into D2: `curr` lands
    /// inside D2 (D2.y ∈ [-1080, 0)), so `in_display_region(curr)`
    /// is true and `entered_barrier` returns `None`. No barrier
    /// crossing — the cursor is moving between two local displays.
    #[test]
    fn vertical_pair_d1_top_into_d2_is_not_barrier() {
        let displays = user_vertical_pair_layout();
        let got = entered_barrier((500.0, 1.0), (500.0, -1.0), &displays);
        assert_eq!(
            got,
            None,
            "expected no barrier crossing when moving from D1 into \
             D2 along the y=0 seam, got {got:?}"
        );
    }

    /// THE BUG: cursor pushed diagonally down-and-slightly-left out
    /// of D1's bottom-left corner. Pre-fix this returned
    /// `Some(Left)` (because the priority list picked Left first),
    /// which then matched the controlled-machine Left key in
    /// `query_pure` and triggered an incorrect crossing. Post-fix
    /// (dominance check) this returns `Some(Bottom)` — which has no
    /// configured neighbor, so no crossing fires.
    ///
    /// dx = -1, dy = +6 → |dy| > |dx|, so Bottom dominates.
    #[test]
    fn vertical_pair_d1_bottom_left_diagonal_fires_bottom_not_left() {
        let displays = user_vertical_pair_layout();
        let got = entered_barrier((100.0, 1075.0), (99.0, 1081.0), &displays);
        assert_eq!(
            got,
            Some(Position::Bottom),
            "expected Bottom (not Left) when exiting D1's bottom-\
             left corner with mostly-downward motion — pre-fix \
             the priority list wrongly picked Left here, \
             got {got:?}"
        );
    }

    /// Mirror of the bug: cursor pushed down-and-slightly-RIGHT
    /// out of D1's bottom-right corner. Pre-fix this returned
    /// `Some(Left)` because dx < 0 still held (the sign check
    /// ignores dominance); however in this symmetric case dx > 0
    /// so the actual pre-fix bug was the Left + Bottom combo
    /// returning Left via the priority list — wait, with dx > 0
    /// pre-fix returns Right (priority) then Bottom also fires.
    /// Post-fix: dy dominates, Bottom fires, Right does not.
    /// Either way, Bottom is the right answer.
    ///
    /// dx = +1, dy = +6 → |dy| > |dx|, so Bottom dominates.
    #[test]
    fn vertical_pair_d1_bottom_right_diagonal_fires_bottom_not_right() {
        let displays = user_vertical_pair_layout();
        let got = entered_barrier((1900.0, 1075.0), (1901.0, 1081.0), &displays);
        assert_eq!(
            got,
            Some(Position::Bottom),
            "expected Bottom (not Right) when exiting D1's bottom-\
             right corner with mostly-downward motion, \
             got {got:?}"
        );
    }

    /// End-to-end `query_pure` regression for the user's bug:
    /// the active set contains ONLY `Left @ D1.id` (the controlled
    /// machine). The cursor exits D1's bottom-left corner
    /// diagonally. Pre-fix this returned the `Left` key (BUG —
    /// the user observed the wrong-side crossing). Post-fix it
    /// returns `None` (correct — no crossing because Bottom has
    /// no configured neighbor).
    #[test]
    fn query_pure_d1_bottom_left_diagonal_does_not_cross() {
        let displays = vec![
            DisplayBound::new(
                DisplayRect::new(0.0, 0.0, 1920.0, 1080.0),
                Some("macos:0000:0000::unknown-1".into()),
            ),
            DisplayBound::new(
                DisplayRect::new(0.0, -1080.0, 1920.0, 1080.0),
                Some("macos:0000:0000::unknown-2".into()),
            ),
        ];
        let mut active = HashSet::new();
        active.insert(BarrierKey {
            pos: Position::Left,
            monitor: Some("macos:0000:0000::unknown-1".into()),
            offset: 0,
            span: 10000,
        });
        let got = query_pure(
            (100.0, 1075.0),  // prev: inside D1, near bottom-left
            (99.0, 1081.0),   // curr: outside the union below-left
            &displays,
            &active,
        );
        assert_eq!(
            got,
            None,
            "expected query_pure to NOT cross when exiting D1's \
             bottom-left corner diagonally — pre-fix this returned \
             the Left key (controlled machine) and incorrectly \
             fired the crossing. got {got:?}"
        );
    }

    /// Belt-and-braces: a more strongly horizontal exit (|dx| > |dy|)
    /// should correctly attribute to `Left`, NOT to `Bottom`. Pins
    /// that the dominance check works in the *opposite* direction
    /// too — i.e. when the cursor really is moving mostly leftward
    /// through a corner exit, Left still wins.
    ///
    /// dx = -5, dy = +1 → |dx| > |dy|, so Left dominates.
    #[test]
    fn vertical_pair_d1_bottom_left_horizontal_dominant_fires_left() {
        let displays = user_vertical_pair_layout();
        let got = entered_barrier((100.0, 1079.0), (95.0, 1080.0), &displays);
        assert_eq!(
            got,
            Some(Position::Left),
            "expected Left (not Bottom) when exiting D1's bottom-\
             left corner with mostly-leftward motion, \
             got {got:?}"
        );
    }
}
