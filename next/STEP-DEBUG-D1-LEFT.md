# D1.Left Crossing Root Cause — Independent Investigation

Date: 2026-09-08
Investigator: independent (no leader guess consumed)
Scope: macOS dual-monitor `client monitor=Display1, side=Left` does not fire.

## §1 Static Read + Chain Reasoning

### Geometry layout (user's macOS)
- **D1 (built-in Retina)**: `pos=(0, 0) size=(1512, 982)` → `x ∈ [0, 1512], y ∈ [0, 982]`
- **D3 (external)**: `pos=(-959, -1440) size=(3440, 1440)` → `x ∈ [-959, 2481], y ∈ [-1440, 0]`

D3 sits upper-left of D1, fully covering D1's horizontal range. The two displays share the y=0 seam (D1.top == D3.bottom). D1 is completely inside D3's x-range; D3 is completely above D1's y-range.

### Test scenario
- `prev_pos = (1, 491)` — inside D1 only (D3.y=491 is outside [-1440, 0])
- `curr_pos = (-1, 491)` — outside both (D1.x=-1 outside [0,1512]; D3.y=491 outside [-1440, 0])

### Chain trace through `entered_barrier`

**`entered_barrier`** (`input-capture/src/geometry/mod.rs:174-187`) iterates [Left, Right, Top, Bottom] and calls `moved_across_boundary` for each.

**`moved_across_boundary`** (`input-capture/src/geometry/mod.rs:159-167`):
```rust
in_display_region(prev_pos, displays) && !in_bounds(curr_pos, displays, pos)
```

- `in_display_region((1, 491), &rects)` → **TRUE** (inside D1)
- `in_bounds((-1, 491), &rects, Left)` (line 144-148):
  ```rust
  displays.iter().any(|d| is_within_dp_boundary(point, d, pos))
  ```
  - D1.left=0, `0 <= -1`? **FALSE**
  - D3.left=-959, `-959 <= -1`? **TRUE** → `in_bounds` returns **TRUE**
- `!in_bounds` = **FALSE** → `moved_across_boundary` for Left = **FALSE**
- Same masking happens for Right (D3.right=2481 > -1) and Top (D1.top=0 ≤ 491).
- Bottom: `in_bounds((-1,491), Bottom)` = D1.bottom=982 > 491 = TRUE, D3.bottom=0 > 491 = FALSE → result TRUE.
- All 4 sides return FALSE → `entered_barrier` returns **`None`**.

### Root mechanism
`in_bounds` checks per-axis containment against ANY display, not whether the point is **inside the union**. When one display (D3) extends much further on the relevant axis than the display the cursor is leaving (D1), the per-axis check is satisfied by the outer display, masking the real exit from the inner display.

This is a **(a) geometry-layer bug** per the task's classification: `entered_barrier` returns `None` when it should return `Some(Left)`.

## §2 Minimal Reproduction Fixture

Added 6 temporary tests at the end of `input-capture/src/geometry/mod.rs` `tests` module (lines 1610+, marked `temp_user_*`):

1. `temp_user_d1_left_crossing_should_fire_left` — `entered_barrier((1,491),(-1,491))` should be `Some(Left)`, actual `None`
2. `temp_user_d1_right_crossing_should_fire_right` — symmetric Right, same root cause (D3.right=2481 > 1513)
3. `temp_user_d3_left_crossing_works_as_baseline` — positive control: D3's exposed left edge fires correctly
4. `temp_user_query_pure_d1_left_should_hit` — end-to-end through `query_pure` with `macos:0000:0000::unknown-1` monitor id, should hit, actual miss
5. `temp_user_display_containing_idx_for_prev_pos` — pins that `display_containing_idx((1,491)) = Some(0)`, so the idx path is correct (bug is NOT here)
6. `temp_user_in_bounds_truth_table` — per-side truth table confirming `in_bounds((-1,491), Left) = TRUE` (the masking)

### Test run results (current code, no fix applied)
```
running 6 tests
test geometry::tests::temp_user_display_containing_idx_for_prev_pos ... ok
test geometry::tests::temp_user_d3_left_crossing_works_as_baseline ... ok
test geometry::tests::temp_user_d1_right_crossing_should_fire_right ... FAILED
test geometry::tests::temp_user_in_bounds_truth_table ... FAILED
test geometry::tests::temp_user_d1_left_crossing_should_fire_left ... FAILED
test geometry::tests::temp_user_query_pure_d1_left_should_hit ... FAILED

test result: FAILED. 2 passed; 4 failed; 0 ignored; 0 measured; 69 filtered out
```

Full suite baseline: **71 existing tests pass** (no regressions from the temp fixtures except the 4 expected failures above).

## §3 Root Cause — file:line

**File:** `input-capture/src/geometry/mod.rs`
**Function:** `moved_across_boundary` at **line 159-167** (delegated to `in_bounds` at **line 144-148**)

**Exact defect:** `in_bounds(point, displays, pos)` at line 144 checks "is point on the inside of `pos` for **any** display" using the per-axis `is_within_dp_boundary` predicate. This is the wrong test for "did the cursor leave the union on side `pos`". The correct test must consider whether the point is still **inside the union on that side**, which requires either:
- (preferred) full 4-axis containment in some display (`is_within_dp_region`), or
- direction-of-motion inference from `(prev_pos, curr_pos)`.

**Specific failure path for the user's case:**
- `input-capture/src/geometry/mod.rs:147` — `is_within_dp_boundary((-1,491), D3, Left)` evaluates `D3.left() <= x` = `-959 <= -1` = TRUE, satisfying `in_bounds` even though `(-1, 491)` is geometrically outside D3 (D3.y ∈ [-1440, 0] excludes y=491).

**Downstream impact:**
- `input-capture/src/geometry/mod.rs:166` — `!in_bounds(...)` returns FALSE, so `moved_across_boundary` returns FALSE for Left.
- `input-capture/src/geometry/mod.rs:186` — `entered_barrier` iterates all 4 sides, all return FALSE, function returns `None`.
- `input-capture/src/geometry/mod.rs:415` — `query_pure` early-returns `None` at `let pos = entered_barrier(...)?`.
- Result: `crossed_pure` / `activation_pure` return `None`, no BarrierKey is emitted, no log line appears on either side. This matches the user's "两边都完全无日志" report exactly.

## §4 Fix Path (minimum change)

### Recommended: replace `moved_across_boundary` with union-exit + direction inference

**Change site:** `input-capture/src/geometry/mod.rs:159-167`

**Current:**
```rust
fn moved_across_boundary(
    prev_pos: (f64, f64),
    curr_pos: (f64, f64),
    displays: &[DisplayRect],
    pos: Position,
) -> bool {
    /* was within bounds, but is not anymore */
    in_display_region(prev_pos, displays) && !in_bounds(curr_pos, displays, pos)
}
```

**Proposed:**
```rust
fn moved_across_boundary(
    prev_pos: (f64, f64),
    curr_pos: (f64, f64),
    displays: &[DisplayRect],
    pos: Position,
) -> bool {
    // Cursor must have left the union entirely.
    if !in_display_region(prev_pos, displays) || in_display_region(curr_pos, displays) {
        return false;
    }
    // Infer direction of exit from the motion vector.
    let (dx, dy) = (curr_pos.0 - prev_pos.0, curr_pos.1 - prev_pos.1);
    match pos {
        Position::Left => dx < 0.0,
        Position::Right => dx > 0.0,
        Position::Top => dy < 0.0,
        Position::Bottom => dy > 0.0,
    }
}
```

`in_bounds` (line 144-148) becomes unused and can be removed. `is_within_dp_boundary` is still used by `is_within_dp_region` and `cursor_within`, so it stays.

### Effect on existing tests
Walked through every `entered_barrier`-asserting test:

| Test | Outcome under proposed fix |
|------|----------------------------|
| `two_by_one_exit_right_from_union` | PASS (motion rightward, exits union) |
| `two_by_one_exit_left_from_union` | PASS |
| `two_by_one_exit_top` | PASS |
| `two_by_one_seam_motion_does_not_cross` | PASS (both inside union) |
| `three_by_one_*` | PASS (same shape as 2x1) |
| `uneven_height_4k_bottom_edge_from_inside_fires` | PASS |
| `l_shape_overlap_zone_crosses_top` | PASS (exits D2 upward) |
| `l_shape_exposed_top_of_upper_display_fires` | PASS (exits D2 upward) |
| **`l_shape_gap_strip_does_not_misfire`** | **FAILS** (see below) |
| `cursor_within_*` (3 tests) | PASS (`cursor_within` is unchanged) |
| `enter_then_pull_back_round_trip` | PASS |
| 4x2 `crossed_pure_c*` matrix | PASS (direction attribution matches prev_pos's display) |
| 4x2 `activation_pure_w*` matrix | PASS |

### One existing test that needs its expected value updated

**`l_shape_gap_strip_does_not_misfire`** (line 788-794) currently asserts `None` for:
- `prev=(100, 0)` (inside D1, on its top edge at y=0)
- `curr=(100, -1)` (outside D1 because y<0; outside D2 because x<200; so outside the union)
- Motion: purely upward (dy<0, dx=0)

Under the proposed fix, the cursor genuinely exits the union upward, so the result becomes `Some(Top)`. The test's PLAN-quoted rationale ("the cursor simply walked off the edge of the world into the L-shaped gap") was pinning a *side effect* of the per-axis `in_bounds` check, not a deliberate semantic. The same exit-from-union motion IS what the user's bug requires to fire. The test must be updated:

```rust
#[test]
fn l_shape_gap_strip_exits_union_top() {
    let displays = layout_l_shaped_offset_200px();
    assert_eq!(
        entered_barrier((100.0, 0.0), (100.0, -1.0), &displays),
        Some(Position::Top)
    );
}
```

The test name and docstring should also be rewritten to reflect "the gap-strip exit from D1 correctly fires Top" rather than "does not misfire into the neighboring display". The neighboring-display misfire concern (originally about bbox-based code attributing the motion to D2) is now handled by the `display_containing_idx` path in `query_pure` — when the prev is in D1, the query key is `D1.Top`, not `D2.Top`, so there is no misattribution.

### Alternative fix (less invasive but uglier)

Keep `moved_across_boundary`'s current shape, but tighten `in_bounds` to require full 4-axis containment:

```rust
fn in_bounds(point: (f64, f64), displays: &[DisplayRect], pos: Position) -> bool {
    displays.iter().any(|d| is_within_dp_region(point, d))
}
```

(Effectively `in_display_region`, so `in_bounds` would be a redundant alias.) This makes `moved_across_boundary` reduce to `in_display_region(prev) && !in_display_region(curr)` — same result as the recommended fix, but without direction inference, so `entered_barrier` would always return the first-iterated side that matches (Left) regardless of actual motion direction. The `l_shape_gap_strip_does_not_misfire` test would then fail in the same way (would return `Some(Left)` instead of `Some(Top)`), and any test that relies on per-side direction attribution would also break. The recommended fix is strictly better.

## §5 Relationship to Leader's Prior Guesses

The task explicitly asked not to consume the leader's prior reasoning. This investigation reached the root cause independently through static reading + fixture execution, and classified the bug as **(a) geometry-layer bug in `moved_across_boundary` / `in_bounds`**.

The finding is consistent with what an "in_bounds is too permissive" framing would predict, but the specific defect (per-axis `is_within_dp_boundary` against `any` display, instead of full containment) is a precise file:line attribution. The fix surface is small: one function body + one test expected value.

## §6 Deliverables

- 6 temporary `temp_user_*` tests added to `input-capture/src/geometry/mod.rs` (not committed; per task rules, would be marked `temp` if committed).
- Full test run: 71 pre-existing tests pass, 4 of 6 temp tests fail (the 2 passing are the positive control and the idx sanity check).
- Root cause: `moved_across_boundary` at `input-capture/src/geometry/mod.rs:159-167` via `in_bounds` at line 144-148.
- Fix: replace `moved_across_boundary` with `in_display_region` exit check + direction-of-motion inference; update one existing test (`l_shape_gap_strip_does_not_misfire`) to expect `Some(Top)`.
