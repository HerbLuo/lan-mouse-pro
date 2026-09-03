use windows::Win32::Foundation::RECT;

use crate::Position;

fn is_within_dp_region(point: (i32, i32), display: &RECT) -> bool {
    [
        Position::Left,
        Position::Right,
        Position::Top,
        Position::Bottom,
    ]
    .iter()
    .all(|&pos| is_within_dp_boundary(point, display, pos))
}

fn is_within_dp_boundary(point: (i32, i32), display: &RECT, pos: Position) -> bool {
    let (x, y) = point;
    match pos {
        Position::Left => display.left <= x,
        Position::Right => display.right > x,
        Position::Top => display.top <= y,
        Position::Bottom => display.bottom > y,
    }
}

/// returns whether the given position is within the display bounds with respect to the given
/// barrier position
///
/// # Arguments
///
/// * `x`:
/// * `y`:
/// * `displays`:
/// * `pos`:
///
/// returns: bool
///
fn in_bounds(point: (i32, i32), displays: &[RECT], pos: Position) -> bool {
    displays
        .iter()
        .any(|d| is_within_dp_boundary(point, d, pos))
}

fn in_display_region(point: (i32, i32), displays: &[RECT]) -> bool {
    displays.iter().any(|d| is_within_dp_region(point, d))
}

fn moved_across_boundary(
    prev_pos: (i32, i32),
    curr_pos: (i32, i32),
    displays: &[RECT],
    pos: Position,
) -> bool {
    /* was within bounds, but is not anymore */
    in_display_region(prev_pos, displays) && !in_bounds(curr_pos, displays, pos)
}

pub(crate) fn entered_barrier(
    prev_pos: (i32, i32),
    curr_pos: (i32, i32),
    displays: &[RECT],
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

/// Returns whether `point` is on the *inside* of `pos` for every display
/// the cursor is currently over. Used by the pending-capture handshake
/// to detect the user pulling the cursor back inside the screen before
/// the remote client ACKs the Enter.
///
/// Conceptually the inverse of [`entered_barrier`]: `entered_barrier`
/// fires on the transition from "inside" to "outside", and `cursor_within`
/// answers "is it currently inside".
pub(crate) fn cursor_within(point: (i32, i32), displays: &[RECT], pos: Position) -> bool {
    in_display_region(point, displays) && in_bounds(point, displays, pos)
}

///
/// clamp point to display bounds
///
/// # Arguments
///
/// * `prev_point`: coordinates, the cursor was before entering, within bounds of a display
/// * `entry_point`: point to clamp
///
/// returns: (i32, i32), the corrected entry point
///
pub(crate) fn clamp_to_display_bounds(
    display_regions: &[RECT],
    prev_point: (i32, i32),
    point: (i32, i32),
) -> (i32, i32) {
    /* find display where movement came from */
    let display = display_regions
        .iter()
        .find(|&d| is_within_dp_region(prev_point, d))
        .unwrap();

    /* clamp to bounds (inclusive) */
    let (x, y) = point;
    let (min_x, max_x) = (display.left, display.right - 1);
    let (min_y, max_y) = (display.top, display.bottom - 1);
    (x.clamp(min_x, max_x), y.clamp(min_y, max_y))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn single_display() -> Vec<RECT> {
        vec![RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        }]
    }

    /// Inside the display on the Right side → still "within".
    #[test]
    fn cursor_within_inside_right_edge() {
        let displays = single_display();
        assert!(cursor_within((1000, 500), &displays, Position::Right));
        assert!(cursor_within((1919, 500), &displays, Position::Right));
    }

    /// Cursor pushed past the right edge → not within anymore.
    /// Mirrors `entered_barrier` going the other way: when the user
    /// pulls back from (1921, 500) to (1919, 500) the host's pending
    /// capture must cancel.
    #[test]
    fn cursor_within_outside_right_edge() {
        let displays = single_display();
        assert!(!cursor_within((1921, 500), &displays, Position::Right));
    }

    /// Cursor far outside all displays → not within (no display at all).
    #[test]
    fn cursor_within_no_display() {
        let displays = single_display();
        assert!(!cursor_within((5000, 5000), &displays, Position::Right));
    }

    /// `entered_barrier` and `cursor_within` are inverses for the
    /// just-crossed transition: crossing right edge then returning
    /// inside should flip both flags cleanly.
    #[test]
    fn enter_then_pull_back_round_trip() {
        let displays = single_display();
        // inside → outside: barrier entered
        assert_eq!(
            entered_barrier((1919, 500), (1921, 500), &displays),
            Some(Position::Right)
        );
        // outside → inside: cursor_within becomes true again
        assert!(!cursor_within((1921, 500), &displays, Position::Right));
        assert!(cursor_within((1919, 500), &displays, Position::Right));
    }
}
