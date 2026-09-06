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
/// and is now outside it with respect to the `pos` side. Used as the
/// per-edge detector inside [`entered_barrier`].
fn moved_across_boundary(
    prev_pos: (f64, f64),
    curr_pos: (f64, f64),
    displays: &[DisplayRect],
    pos: Position,
) -> bool {
    /* was within bounds, but is not anymore */
    in_display_region(prev_pos, displays) && !in_bounds(curr_pos, displays, pos)
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
    /// outside the union entirely. The old bbox-based code would
    /// still register this as Top (because y = 0 was bbox.ymin). The
    /// new logic correctly returns None: there is no barrier edge
    /// to attribute the crossing to; the cursor simply walked off
    /// the edge of the world into the L-shaped gap.
    ///
    /// This is the test the PLAN calls out as "L 形错位 200px:
    /// 错位区穿出时不误触相邻屏".
    #[test]
    fn l_shape_gap_strip_does_not_misfire() {
        let displays = layout_l_shaped_offset_200px();
        assert_eq!(
            entered_barrier((100.0, 0.0), (100.0, -1.0), &displays),
            None
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
}
