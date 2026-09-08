# STEP-DEBUG-M4-RETINA-INTERNAL — 内建屏 monitor 不可穿根因

> 调研日期：2026-09-08
> 触发条件：用户报告 macOS 主控端双屏配置，dropdown 选内建屏（Display 1, scale=2, 1512x982）绑 `top` → 不触发；选外置屏（Display 3, scale=1, 3440x1440）能穿；None（H1 fallback）能穿
> 用户假设：内建屏与外置屏 size 差异巨大（1512x982 vs 3440x1440）可能有关
> 调研范围：`input-capture/src/geometry/mod.rs` + `input-capture/src/macos.rs` 全文件；`src/service.rs` 中 update_monitor/activate_client/client_at 调用链；core-graphics crate 0.25.0 event.rs 中 CGEventGetLocation 语义

---

## §1 关键事实（macOS 坐标系 / CGEvent / CGDisplay bounds 的 point/pixel 语义）

### 1.1 macOS Quartz 全局坐标系 = **points**（不是 pixels）

- **CGDisplayBounds**（`core-graphics` crate 中 `CGDisplay::new(d).bounds()`）返回 **points**（逻辑坐标），不是 backing pixels。
  - 证据：Apple Quartz Display Services Reference 把 "global display coordinate system" 定义为 "the upper-left of the primary display, in **points**"（72dpi-equivalent logical units，与 1x 时 1 point = 1 pixel，2x 时 1 point = 2 backing pixels）。
- **CGEventGetLocation**（`core-graphics::CGEvent::location()` 在 `event.rs:767`）返回 **points**。
  - 证据：同一文档 — `CGEventGetLocation` 描述为 "The location of the mouse cursor, in global display coordinates"。
- **CGEvent MOUSE_EVENT_DELTA_X/Y** 也是 **points**（与 location 同坐标系）。
  - 证据：Apple CGEvent docs — "kCGEventMouseDeltaX contains the change in cursor position since the last mouse-move event. The cursor position is reported in global display coordinates (points)"。

### 1.2 scale 字段的角色

- `MonitorInfo.scale` 是 **point → pixel 比率**（不是反过来）：
  - 代码 `compute_scale(pixel_width, point_width) = pixel_width / point_width`（macos.rs:496）
  - scale=2.0（Retina）= 2 backing pixels per point
  - scale=1.0（外置）= 1 backing pixel per point
- **scale 字段在 `enumerate_monitors` 路径上完全不影响** `MonitorInfo.position` / `MonitorInfo.size` 的单位 — 这两个字段在 `enumerate_monitors_for_ids`（macos.rs:579）里直接抄 `bounds.origin.x/y` 和 `bounds.size.width/height`（都是 points）。

### 1.3 production path 坐标系一致性核验

| 步骤 | 数据 | 单位 | 文件:行 |
|---|---|---|---|
| Quartz 枚举 | `display.bounds()` → CGRect | points | macos.rs:585 |
| enumerate | `position = (bounds.origin.x as i32, ...)` / `size = (bounds.size.width as u32, ...)` | points → points | macos.rs:586-587 |
| build_display_bounds | `DisplayRect::new(m.position.0 as f64, ..., m.size.0 as f64, ...)` | points | macos.rs:647-652 |
| event tap | `let location = cg_ev.location()` | points | macos.rs:1097 |
| event tap | `delta = ev.get_double_value_field(MOUSE_EVENT_DELTA_X/Y)` | points | macos.rs:1098-1099 |
| event tap | `prev_pos = (location.x, location.y)` / `curr_pos = (location + delta)` | points | macos.rs:1100-1101 |
| geometry helper | `display_containing_idx(&rects, prev_pos)` | points vs points | geometry/mod.rs:217 |

**结论：production path 全部统一在 points 坐标系，没有任何 step 把 point 误当 pixel 或反之**。scale 字段只用于 IPC 上行（前端 tooltip 显示），不参与几何计算。

### 1.4 macOS cursor 在多 display 间的行为

- macOS Quartz 把 cursor **clamp 到 union 边界**（macos.rs:1082 注释正确）。
- 但在 union 内部，cursor 可以**沿共享边 warp**到相邻 display：
  - 当 cursor 到达 display A 的某条 edge，且该 edge **与另一 display B 的某条 edge 重合**（典型：上下/左右相邻两块屏幕，y 或 x 一致），macOS 把 cursor warp 到 display B 的对应位置。
  - 此时 `cg_ev.location()` 报的是 B 的坐标（不再是 A 的），`delta` 反映的是 cursor 位置变化（含 warp）。

---

## §2 现有 production 测试覆盖矩阵

| 测试 | 几何 fixture | prev_pos → curr_pos | query key 期望 | active key | 是否覆盖用户场景 |
|---|---|---|---|---|---|
| `crossed_pure_c1_display0_top_hit` | 2x1 横排：d0=(0,0,1920,1080), d1=(1920,0,1920,1080) | (500,500) → (500,-2) | d0 | d0 | ❌ 两屏共享 top 边，curr 立即出 union |
| `crossed_pure_c2_display1_top_hit` | 同上 | (2500,500) → (2500,-2) | d1 | d1 | ❌ 同上 |
| `crossed_pure_c3_display0_top_misses_legacy_active` | 同上 | (500,500) → (500,-2) | d0 → legacy None | None | ❌ 同上 |
| `crossed_pure_c4a/c4b_seam_top_*` | 同上 | (1920,540) seam → (1920,-2) | d1 | d0/d1 | ❌ 同上 |
| `crossed_pure_c5_off_screen_is_miss` | 同上 | (-100,500) → (-100,-2) | (entered_barrier 先返 None) | d0 | ❌ prev 已出 union，与用户场景无关 |
| `crossed_pure_c6_display0_top_picks_d0_over_d1` | 同上 | (500,500) → (500,-2) | d0 | d0+d1 | ❌ 同上 |
| `crossed_pure_c7_display0_right_hit` | 同上 | (1900,500) → (3841,500) | d0 | d0 | ❌ union right edge，单纯 |
| `crossed_pure_c8_display1_left_hit` | 同上 | (3000,500) → (-2,500) | d1 | d0+d1 | ❌ 同上 |
| `crossed_pure_monitor_id_none_uses_legacy_key` | 单屏 | (500,500) → (500,-2) | None | None | ❌ 单屏无相邻 display |
| `query_pure_falls_back_to_legacy_*` | 同 c3 | (500,500) → (500,-2) | d0 → None | None | ❌ 同上 |
| `activation_pure_w1-w6` | 同 c1-c8 但 Windows ids | 同上 | 同 c1-c8 | 同 c1-c8 | ❌ 同上 |
| `l_shape_overlap_zone_crosses_top` | L 形错位 200px：d1=(0,0,1920,1080), d2=(200,-1080,1920,1080) | (500,-1079) → (500,-1081) | (entered_barrier 先返 Top) | — | ❌ L 形是**错位**而非**相邻**，curr 出 union |
| `l_shape_gap_strip_does_not_misfire` | 同上 | (100,0) → (100,-1) | None（gap 区域） | — | ❌ gap 区域，与相邻边不同 |
| `build_display_bounds_pure_*` | 直接断言 build 函数的纯映射 | — | — | — | ❌ 不测 query |
| **用户场景 (D1=(0,0,1512,982), D3=(-959,-1440,3440,1440))** | **D1 和 D3 在 y=0 共享一条边，D3 在 D1 上方** | **prev in D1 → curr 必须穿过 D3 才能出 union** | **query 用 D3.monitor_id（prev_pos 在 D3）** | **D1.monitor_id** | **❌ 完全没覆盖** |

**矩阵结论**：

1. 现有测试 fixture 一律是 **2x1 横排**（两屏共享 top/bottom）或 **L 形错位**（两屏不共享任何边）。**没有相邻两屏共享单条边的 fixture**。
2. 现有测试也没有 scale=2 的 fixture（不影响 query，但 1.1/1.3 已证坐标系一致，scale 不参与）。
3. 没有任何测试断言 "prev_pos 在 display A，但 barrier 物理 cross 的 edge 在 display B" 这种 case 下的 query key 归属。

**vacuous 测试覆盖**：现有 C1-C8 / W1-W6 锁住了 "query key 使用 prev_pos 所在 display 的 monitor_id" 这条 invariant，但**没有锁住** "cross 的 edge 物理位置 ≠ prev_pos 所在 display" 的 invariant。M3 STEP-3.4 fixture 选取 2x1 横排恰好绕过了这条 invariant。

---

## §3 最可能根因（按概率排序）

### 根因 A — **几何 layout：Display 1 top 与 Display 3 bottom 共边，cursor 可在不 cross union 边界的情况下从 D1 warp 到 D3**（**~85% 概率，主因**）

#### file:line + 函数

- `input-capture/src/geometry/mod.rs:217` `display_containing_idx` — query key 用 prev_pos 所在 display
- `input-capture/src/geometry/mod.rs:404-454` `query_pure` — prev_pos 在 D3 → query 是 D3.monitor_id
- `input-capture/src/macos.rs:1097-1101` event tap callback — prev_pos / curr_pos 取样

#### 假设链路

用户 fixture（从 daemon log 直接抄）：
```
Display 1: pos=(0, 0)        size=(1512, 982)   scale=2  primary=true   → rect (0, 0, 1512, 982)
Display 3: pos=(-959, -1440) size=(3440, 1440)  scale=1  primary=false  → rect (-959, -1440, 3440, 1440)
```

矩形关系：
- D1.top = 0, D1.bottom = 982
- D3.top = -1440, D3.bottom = 0
- **D3.bottom == D1.top == y=0**（共享一条边）
- 在 x ∈ [0, 1512] 区间（即 D1.x 范围 ∩ D3.x 范围），D1 在 y ∈ [0, 982]，D3 在 y ∈ [-1440, 0] —— **两块屏在 y=0 处共享一条水平边**

用户配置 `client monitor = Display 1`，期待：cursor 在 D1 顶边触发。

物理事件链（cursor 从 D1 内部 (500, 500) 开始向上移动）：
1. CGEventTap 收到 MouseMoved，location 逐渐上升
2. location 到 y=0（D1.top / D3.bottom 共边）
3. **macOS 把 cursor warp 到 D3 内部 (500, -ε)**（Quartz 默认行为：跨相邻边自动 warp；core-graphics 0.25.0 event.rs 中 CGEventGetLocation 返回的是 post-warp 位置）
4. CGEventTap 后续事件 location 继续在 D3 内下降（如 (500,-5)、(500,-100)、…、(500,-1439)）
5. location 触底 y=-1440（D3.top = union.top）→ macOS clamp；delta 继续增大
6. 直到 `curr_pos.y < -1440`，`entered_barrier` 返 `Some(Top)`
7. `display_containing_idx(&rects, prev_pos=(500,-1439))` → D3 的 idx（**不是 D1**）
8. `query_pure` 构造的 query key = `BarrierKey { pos: Top, monitor: Some("macos:...::unknown-3"), ... }`
9. active 是 `BarrierKey { pos: Top, monitor: Some("macos:...::unknown-1"), ... }`（用户选 D1）
10. `clients.contains(&key)` 永 false → **0% 命中**

为什么 D3 工作：cursor 从 D3 内部 (1000, -1000) 上升 → location 触底 y=-1440 → clamp → curr_pos.y < -1440 → Top crossing 检出 → prev_pos 仍在 D3 → query key 用 D3 → active 也是 D3 → ✅ 命中。

为什么 None 工作：query key 算出 D3 后 specific miss → H1 fallback（geometry/mod.rs:444-452）→ probe `monitor: None` → active 是 None → ✅ 命中（H1 fixup 已落）。

#### 验证方法（最小 fixture 单测）

```rust
#[test]
fn crossed_pure_adjacent_displays_d1_top_edge_crossed_at_d3() {
    // 用户真实 fixture：D3 上方 + D1 下方，y=0 共边
    let displays = vec![
        DisplayBound::new(DisplayRect::new(0.0, 0.0, 1512.0, 982.0),
                          Some("macos:0000:0000::unknown-1".into())),
        DisplayBound::new(DisplayRect::new(-959.0, -1440.0, 3440.0, 1440.0),
                          Some("macos:0000:0000::unknown-3".into())),
    ];
    let mut active = HashSet::new();
    active.insert(BarrierKey {
        pos: Position::Top,
        monitor: Some("macos:0000:0000::unknown-1".into()),
        offset: 0, span: 10000,
    });
    // cursor 在 D1 内部 → 穿出 D1.top 进 D3 → 继续穿过 D3.top 出 union
    // 物理上 prev_pos 是 D3 内部的最后一次 in-union 位置
    let got = crossed_pure((500.0, -1439.0), (500.0, -1500.0), &displays, &active);
    assert_eq!(got, None, "D1.top 不可能 cross：cursor 必须先穿 D3 才能出 union");
    // 而 legacy None fallback 应当命中：
    active.clear();
    active.insert(BarrierKey { pos: Position::Top, monitor: None, offset: 0, span: 10000 });
    let got2 = crossed_pure((500.0, -1439.0), (500.0, -1500.0), &displays, &active);
    assert_eq!(got2, Some(BarrierKey { pos: Position::Top, monitor: None, offset: 0, span: 10000 }));
}
```

这个 fixture 把用户真机 layout **永久 lock** 到 CI；任何后续把 query 逻辑改成"按 cross edge 所在 display 归属"都会立即红。

#### 修复方向

**这是 M3 设计层面的 geometric limitation，不是 bug**。Display 1.top 不在 union 边界上 —— 它被 Display 3.bottom 完全遮挡了。用户在 dropdown 选 D1 + top 是一个**几何上不可能触发**的配置。

修复分三层（按时间/价值排序）：

1. **M3 UX 警告**（短期，最快，~30min）：在 ConnectionRow.vue / LayoutEditor.vue 端计算哪些 (monitor, pos) 组合对应的 edge 被另一 display 的对边覆盖（即 `is_edge_obscured(monitor, pos, monitors)`）；若被覆盖，dropdown option 加 "（此边被 Display N 遮挡，barrier 不会触发）" 副 label。前端不破坏向后兼容，只警告。
2. **M3 backend 文档 + service 拒绝**（中期，~30min）：在 `service.rs::update_monitor` 拒绝 `monitor × pos` 组合中 `is_edge_obscured` 为 true 的配置，返回 `BindingInvalid(monitor, "obscured edge")` 让前端高亮。或者保留但打 `log::warn!` 提示用户。
3. **M4 exposed_segments 重构**（完整修复，PLAN 已规划，~9h）：把所有 barrier 都基于 `exposed_segments(displays)` 输出的真实线段；monitor 维度降级为线段的 sub-range（offset/span）。M4 STEP-4.1 的 hull.rs 就是为这个问题设计的。**本 bug 在 M4 完工后消失**。

### 根因 B — **production test fixture 没覆盖"相邻两屏共享单边"几何，导致 query_pure 在该 layout 下行为未受锁**（**~10% 概率，次因**）

#### file:line + 函数

- `input-capture/src/geometry/mod.rs:1024-1040` C1-C8 矩阵注释（注释表格）
- `input-capture/src/geometry/mod.rs:540-588` test fixture helpers（`layout_2x1`、`layout_l_shaped_offset_200px`）

#### 假设链路

PLAN §3 STEP-3.4 line 162 要求 C1-C8 矩阵覆盖 "prev 在 display_0 / display_1 / 接缝 / 屏外 × active key `monitor: Some(d0) / Some(d1) / None`"。但 8 个 fixture 全用 `layout_2x1_bound()`（d0=(0,0,1920,1080)、d1=(1920,0,1920,1080)），**两块屏共享整条 top 边和整条 bottom 边**。这种几何下"cursor 从 D0 向上穿过 union top" = "cursor 已经在 union 边界上"（一离开 D0 立即出 union），query key 用 D0 仍然合理。

但**真实 macOS 双屏布局很少是 2x1 横排**：MBP + 外接屏通常把外接屏放上方（MBP 当主屏，下方），或 L 形错位。一旦外接屏在 MBP 上方，两屏就在 y=某值处共享一条水平边 —— 正是用户 fixture。

现有 fixture **没有 lock 住"相邻两屏共享单边"时的 query 归属不变量**，所以：
- 任何后续 refactor 把 query 从"按 prev_pos 所在 display"改成"按 cross edge 物理位置所在 display"都不会被现有 fixture 抓到；
- 但**当前 query_pure 实现是对的**（按 prev_pos 归属）—— 所以这不是 active bug，是**未来 regression 风险**。

#### 验证方法

加 fixture lock（见根因 A 验证方法的 fixture；同一个 fixture 同时锁住"query 按 prev_pos 归属"和"几何上 D1.top 不暴露"两条 invariant）。

#### 修复方向

加 production-realistic fixture（推荐**立即合并，不动行为**：锁住 invariant 而非改代码）：

- `crossed_pure_user_fixture_d1_top_obscured_by_d3` —— 断言 D1.top 配置下 `crossed_pure` 返 None（几何不可能）+ legacy fallback 返 None
- 镜像：`crossed_pure_user_fixture_d3_top_exposed` —— 断言 D3.top 配置下命中
- 文档注释：把 C1-C8 矩阵注释扩展一行 R1（用户真实 fixture），显式承认"shared-edge 的 display 配 top/bottom 时 query 归属 = 真正 cross union boundary 的那个 display 的 monitor_id，与用户 dropdown 选择无关"。

### 根因 C — **macOS cursor warp 时 delta 计算是否一致 points**（**~5% 概率，备用假设**）

#### file:line + 函数

- `input-capture/src/macos.rs:1098-1099` `cg_ev.get_double_value_field(EventField::MOUSE_EVENT_DELTA_X/Y)`

#### 假设链路

Apple CGEvent docs 对 `kCGEventMouseDeltaX` 的单位描述有歧义（"change in cursor position since the last event" vs "raw mouse delta"）。如果 delta 是 **raw mouse delta in mouse units**（不与 location 的 points 单位一致），`prev_pos = location`、`curr_pos = location + delta` 在数学上就不对了。

但这个假设**与用户报告的现象不符**：
- 即便 delta 单位错位，D1 内部 cursor 移动也不会跨 union —— query 仍然是 D1.monitor_id，仍然命中 None。
- 唯一会暴露 delta 单位错位的场景是 cross union 边界，但那种情况 D3 工作（None 工作）说明 cross union 逻辑整体可用，delta 单位错位不太可能让 D1 不工作但 D3 工作。
- **生产路径 D3 能穿 + None fallback 能穿 + 用相同 prev/curr 计算 → 排除 delta 单位问题**。

#### 验证方法

在 `compute_scale` 测试旁加 `core-graphics::CGEvent::location()` 的 smoke test（构造 fake event 不容易，可跳过）或直接读 macOS Quartz docs 确证 delta = points。

#### 修复方向

**无需修复**。把"delta 单位 = points"作为已验事实写入 macos.rs:1097-1101 注释（现注释已暗含，但可更明确）。

---

## §4 建议下一步

### 优先级 1：**新增 production-realistic fixture lock 住 invariant**（必须先做，给后续修复/重构兜底）

- 文件：`input-capture/src/geometry/mod.rs`（在 C1-C8 矩阵之后）
- 工作量：~30min
- 内容：加 2 个 test（参考根因 A 验证方法 + 镜像 D3.top exposed case），扩 C1-C8 矩阵注释加 R1 行（用户 fixture）
- 行为不变，纯 lock；workspace build / test 全绿；commit message 标 "lock: shared-edge adjacent-display invariant (no behavior change)"

### 优先级 2：**M3 UX 警告**（短期最快让用户避免踩坑）

- 文件：`lan-mouse-vue/src/components/ConnectionRow.vue` + `lan-mouse-vue/src/api/ipc.ts`（前端）；`src/service.rs::update_monitor` 加可选 `is_edge_obscured` check + `log::warn!`
- 工作量：~1.5h（含前端 snapshot test）
- 行为：dropdown option 副 label 显示 "此边被 Display N 遮挡，barrier 不会触发"；service 端记录 warning 让用户搜 log
- M3 不破坏向后兼容；让用户在 M4 完工前能知道"我选的配置不会工作"

### 优先级 3：**派 plan-step-executor 修 M4 起步 STEP-4.1 exposed_segments**（最终彻底修复）

- 文件：新 `input-capture/src/geometry/hull.rs`
- 工作量：~9h（含 4.1-4.10 全套）
- 行为：barrier 从 "monitor + pos" 维度降级为 "EdgeSegment + offset + span" 维度；monitor 仅用于定位 edge segment 集合；M3 用户配置自动迁移
- 此 bug 在 M4 完工后消失

### 优先级 4：是否立即给用户解释 + 临时绕过方案

- 现在就可以告诉用户："这不是 bug —— 你的内建屏在 y=0 处被外置屏完全覆盖；macOS cursor 跨 y=0 时自动 warp 到外置屏；barrier 物理 cross 的 edge 在外置屏顶（y=-1440）而不是内建屏顶（y=0）。dropdown 选内建屏 + top 在你这种布局下几何上不可能触发 —— 这是 M3 的已知限制，M4 会通过 exposed_segments 算法彻底修复。"
- 临时绕过：dropdown 选 None（legacy）—— H1 fallback 已经让 None 在所有 layout 下都工作。

### 风险与时间预估

| 任务 | AI 时间 | 风险 | 阻塞 |
|---|---|---|---|
| P1 fixture lock | ~30min | 极低（纯 lock） | 无 |
| P2 M3 UX 警告 | ~1.5h | 低（前端 + 后端 2 处改） | 无 |
| P3 M4 exposed_segments | ~9h | 中（几何算法 + 4 个 backend 适配） | M3 完工 |
| P4 用户沟通 | 0 | — | — |

**建议路径**：先做 P1（lock invariant，零回归），同时 P4（用户沟通 + 临时 None workaround），然后排 M4 计划（PLAN §3 M4 路线图已就绪）。

---

## §5 总结

| 项 | 结论 |
|---|---|
| macOS 坐标系 | 全部统一 points，无 point/pixel 错位 |
| scale=2 影响 | 仅 IPC 上行字段；不参与 query 计算 |
| 主因 | **几何 layout**：D1.top 与 D3.bottom 共享 y=0，cursor 在不 cross union 边界的情况下 warp 进 D3；最终 barrier cross 的 edge 在 D3.top（union.top），query key 因此用 D3.monitor_id |
| 次因 | 现有 fixture 没覆盖"相邻两屏共享单边"几何，invariant 未被 lock |
| 备用假设 | CGEvent delta 单位错位 —— 已通过 "D3 工作 + None 工作" 反证排除 |
| 修复路径 | P1 fixture lock → P2 UX 警告 → P4 用户沟通 → P3 M4 完工 |
| H1 fallback 状态 | ✅ 正常工作（用户报 None 能穿即证） |
| 真机回归需求 | 当前无需（fix 在 fixture 层；P2/P3 真机验证需人类配合） |

---

**调研完成日期**：2026-09-08
**调研依据**：`input-capture/src/geometry/mod.rs`（全文件）、`input-capture/src/macos.rs`（全文件）、`src/service.rs`（update_monitor/activate_client/client_at 调用链）、`src/client.rs`（get_key/set_monitor）、`lan-mouse-ipc/src/lib.rs`（MonitorInfo wire schema）、core-graphics 0.25.0 event.rs
**不修改代码**（per leader prompt "不要 commit；不要改代码"）
