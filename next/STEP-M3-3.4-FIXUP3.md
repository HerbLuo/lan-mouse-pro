# STEP M3-3.4-FIXUP3 — `moved_across_boundary` 设计 bug 修复

> PLAN §M3 / STEP-3.4 (FIXUP3)
> 执行日期：2026-09-08　实际耗时：~25 min
> 结论：通过

## 1. 做了什么

修复 `input-capture/src/geometry/mod.rs::moved_across_boundary` 的设计 bug——它原本用 `in_bounds(curr, displays, pos)`（per-axis + any display）检查"curr 是否仍在某 display 的 pos 边内侧"，导致当外层 display 覆盖内层 display 的边时永远返回 true，让 `entered_barrier` 假阳性退化返回 `None`（用户报告的"D1.Left 完全不触发"现象）。

### 改动 1 — `moved_across_boundary` (line 156-196)

从 `in_display_region(prev) && !in_bounds(curr, pos)` 改成 **union-exit + 方向推断**：

```rust
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
    match pos {
        Position::Left => dx < 0.0,
        Position::Right => dx > 0.0,
        Position::Top => dy < 0.0,
        Position::Bottom => dy > 0.0,
    }
}
```

两步语义：
1. **union exit**：prev 在 union 内 + curr 在 union 外 → 否则 false（保证 cursor 真的"出去了"）。
2. **方向推断**：根据 `(curr - prev)` 的符号匹配 pos 的隐含方向 → 否则 false（保证 attribution 到正确的 side）。

`in_bounds` (line 144-148) 保留不动——它仍被 `cursor_within` (line 277) 调用，是 pending-capture handshake 路径的反向谓词。`entered_barrier` (line 203-) 一字未动：现在它按 `[Left, Right, Top, Bottom]` 顺序找第一个返回 true 的 pos，因为每个 pos 的方向签名不同，对单一轴运动（所有现有测试都是单一轴）只会匹配一个 pos；tie case（dx=dy≠0）在现有测试中不存在。

### 改动 2 — `l_shape_gap_strip_does_not_misfire` 测试 (line 825-852)

旧期望 `None`（因为 per-axis `in_bounds(curr, Top)` 假阳性返回 true → `moved_across_boundary` 假阴性返回 false）。新期望 `Some(Position::Top)`：gap strip 的 `(100, 0)` → `(100, -1)` 真的是一次向上穿出 union 的运动，方向推断正确归到 `Top`。测试名改为 `l_shape_gap_strip_exits_union_top`，注释从"pinning a side effect"改为讲正确的 union-exit + 方向推断语义。

### 改动 3 — 6 个 temp 临时测试改为永久回归测试 (line 1670-1852)

| 临时名 | 永久名 |
|---|---|
| `temp_user_d1_left_crossing_should_fire_left` | `d1_left_crossing_to_outer_display_fires_left` |
| `temp_user_d1_right_crossing_should_fire_right` | `d1_right_crossing_to_outer_display_fires_right` |
| `temp_user_d3_left_crossing_works_as_baseline` | `d3_left_crossing_fires_left_as_baseline` |
| `temp_user_query_pure_d1_left_should_hit` | `query_pure_d1_left_hit_in_outer_display_layout` |
| `temp_user_display_containing_idx_for_prev_pos` | `display_containing_idx_picks_d1_in_outer_display_layout` |
| `temp_user_in_bounds_truth_table` | `in_bounds_per_side_truth_table_for_outer_display_layout` |

每个测试的 docstring 重写：去掉 "this is the bug / current code" 框架，改为"original bug was … after the fix … this test pins the regression"。模块顶部 `// ----- TEMP: ... -----` 改为 `// ----- macOS dual-monitor D1.Left crossing regression -----`，加上 `STEP-M3-3.4-FIXUP3` 引用。

### 改动 4 — `in_bounds` 真理表修正

原 temp 测试断言 `!in_bounds(...Bottom) == true`（即 in_bounds 返回 false），但 `D1.bottom = 982 > 491` 满足 `is_within_dp_boundary` → `in_bounds` 实际返回 **true**。原测试的 docstring 把这条描述错了。改后断言改成 `in_bounds(...Bottom) == true`，匹配真实行为（4 个方向 in_bounds 全部 true——这正是 per-axis 检查的设计缺陷的另一个佐证）。

## 2. 验证结果

| 命令 | 结果 |
|---|---|
| `cargo test -p input-capture --lib` | **75 passed, 0 failed** (改前 71 passed / 4 failed) |
| `cargo test --workspace` | **176 passed, 0 failed** 跨所有 crate |
| `cargo clippy -p input-capture --all-targets -- -D warnings` | exit 0，无 lint warning |
| `cargo fmt --check -p input-capture` | 0 diff |
| `git diff --stat Cargo.lock` | 无 diff（Cargo.lock 不变） |

测试覆盖关键 case：
- D1.Left / D1.Right 用户的 macOS dual layout 现在正确 fire（之前 None）
- D3.Left baseline（positive control）继续 fire
- L 形 gap strip `(100, 0) → (100, -1)` 现在正确归到 Top
- 2x1 / 3x1 / 4K 不等高的所有 existing test 继续过

## 3. 与 PLAN 的偏差

无架构偏差。改动完全在 STEP-DEBUG-D1-LEFT §4 + §5 调研报告框定的"最小修复"范围内：

- `moved_across_boundary` 改写策略 = 调研报告 §4 "Recommended: replace `moved_across_boundary` with union-exit + direction inference"，与 leader 在 STEP prompt 提供的版本（"保留 `in_bounds` 但改语义为 prev 所在 display"）相比更激进——leader 的版本会让 `entered_barrier` 在 union-exit 时永远返回 Left。调研报告指出了这一点，本 STEP 选了"重写 + 方向推断"路径。
- `entered_barrier` 未改动——调研报告说"根据 (dx, dy) 推断 pos"，但通过把方向推断下沉到 `moved_across_boundary`，`entered_barrier` 自然保持 `[Left, Right, Top, Bottom]` 迭代顺序即可，不需要重写函数体。
- `in_bounds` 保留——调研报告 §4 提到 "becomes unused and can be removed"，但实际 `cursor_within` 仍调用它（line 277），不能删。调研报告的"unused"判断是基于它只被 `moved_across_boundary` 调用的假设，不准确；本 STEP 保留它。

无 SUGGESTION 触发（这是直接的 bug 修复，不是 scope-creep）。

## 4. 处理的 SUGGESTION 项

无。

## 5. 闸门检查

- **时间门**：~25 min < 30 min target。
- **milestone 边界门**：未触碰 macos.rs / windows/event_thread.rs / layer_shell.rs / libei.rs / src/service.rs / src/capture.rs。仅 `input-capture/src/geometry/mod.rs`。**M3 milestone 范围未溢出**（修复的是 M3.4 / M3.5 引入的几何层 bug；不涉及 M4 exposed_segments / sub-edge）。
- **范围文件**：仅 `input-capture/src/geometry/mod.rs` 改动（257 lines / +244 net new code 主要是测试 + docstring；-13 是把 vacuous test 改成真实断言 + 改 moved_across_boundary 函数体）。

## 6. 遗留

- **真机回归待 leader**：本 STEP 修了 CI 层全部 unit test，但用户报告的真机现象（"cursor 推到 (-1, 491) 完全无日志"）需要 leader 在自己的 macOS dual layout 上跑一遍才能 100% 确认。STEP 调研报告 §3 给出了精确 trace，理论上修好；但真机环境涉及 CGEventTap / 显示器事件实际发出的 `monitor_id` 等 CI 跑不到的维度。
- **`entered_barrier` tie 行为**：调研 prompt 说"相等时按 [Top, Bottom, Left, Right] 优先级（保留原行为）"，但现有代码是 `[Left, Right, Top, Bottom]`。现有测试无 tie case，两个顺序都过。我保留了 `[Left, Right, Top, Bottom]`（真实"原行为"），prompt 里的"原行为"描述不准确。如 leader 想 tie 改 Top-first，单行调整即可。

## 7. 下一步

建议 leader:
1. 真机回归（macOS dual-monitor 推 cursor 过 D1 左/右边）—— 这才是 user-facing 验证。
2. 确认无回归后 commit。建议 commit message:
   ```
   fix(input-capture): replace per-axis moved_across_boundary with union-exit + direction inference
   
   The per-axis in_bounds predicate is satisfied by any display extending
   past the relevant axis, which masks inner-display exits when an outer
   display covers them (the macOS dual-monitor D1.Left crossing bug).
   The new detector does a proper union-exit check (prev in, curr out)
   and infers the exit direction from the (curr - prev) motion vector.
   
   归档: next/STEP-M3-3.4-FIXUP3.md
   ```
3. leader commit 后，本 STEP 报告归到 `next/done/`。
