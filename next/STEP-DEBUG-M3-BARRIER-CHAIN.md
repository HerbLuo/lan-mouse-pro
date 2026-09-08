# STEP-DEBUG-M3-BARRIER-CHAIN — 真机 barrier 触发链断裂根因

> 调研日期：2026-09-08
> 触发条件：用户报告 macOS（主控） + Linux GNOME Wayland libei（被控）双屏测试，鼠标移到边缘主控和被控端日志都没有 barrier 触发
> 调研起点：M3.4/3.5/3.6 修复 commit（d7d2c58 / 7ed1352 / 3afeeff / 9dcbe1b）后**仍未生效**
> 调研范围：input-capture（geometry/macOS/Windows/layer_shell/libei/dummy）、src/capture.rs、src/service.rs、src/client.rs

---

## §1 排查路径摘要（读了哪些文件 + 哪些调用链）

### 1.1 必读源文件
- `input-capture/src/geometry/mod.rs` — `DisplayBound` 数据结构 + `crossed_pure` / `activation_pure` / `query_pure` 共享 helper
- `input-capture/src/macos.rs` — `InputCaptureState` + `crossed()` + `update_bounds()` + `build_display_bounds` + `enumerate_monitors_for_ids`
- `input-capture/src/windows/event_thread.rs` — `check_client_activation` + `DISPLAYS` + `update_display_regions`
- `input-capture/src/layer_shell.rs` — `Capture::create/destroy` + `add_client` + `delete_client` + `record_active_position`
- `input-capture/src/libei.rs` — `select_barriers` + `update_barriers`
- `src/capture.rs` — `CaptureTask::do_capture_session` 整条事件链
- `src/service.rs` — `activate_client` / `update_monitor` / `FrontendRequest::UpdateMonitor` handler
- `src/client.rs` — `get_key` / `set_monitor` / `client_at`

### 1.2 触发链正向追踪
1. 前端 dropdown → WS `FrontendRequest::UpdateMonitor(handle, monitor)` →
2. `service.rs:288` handler → `service.rs:803 update_monitor` →
3. `client_manager.set_monitor(handle, monitor)` 写入 `c.monitor`（**Case A**: `Some(id)` / **Case B**: `None` legacy）→
4. `activate_client` → `client_manager.get_key`（client.rs:160）构造完整 `BarrierKey { pos, monitor: c.monitor.clone(), ... }` →
5. `capture.create(handle, &key, Default)` → `CaptureRequest::Create` channel →
6. `CaptureTask::do_capture_session` (capture.rs:883) → `capture.create(handle, &p).await` →
7. `InputCapture::create` (input-capture/src/lib.rs:148) → `self.capture.create(key).await`（macOS backend trait 方法）→
8. **macOS** `Capture::create` (macos.rs:1483) — `spawn_local { notify_tx.send(ProducerEvent::Create(key)).await }` + 立刻 `Ok(())` →
9. Producer task `handle_producer_event(Create(k))` (macos.rs:355) → `self.active_clients.insert(k)` →
10. `displays[idx].monitor_id` 由 `build_display_bounds` (macos.rs:638) 设置，**永远是 `Some(m.id.clone())`**

### 1.3 鼠标边缘事件链
11. CGEventTap callback (macos.rs:1078) — `MouseMoved` 触发
12. `prev_pos = (location.x, location.y)` (clamped) / `curr_pos = (location + delta)` (predicted)
13. `state.crossed(prev_pos, curr_pos)` →
14. `crossed_pure(prev_pos, curr_pos, &self.displays, &self.active_clients)` (geometry/mod.rs:455)
15. `query_pure` (geometry/mod.rs:404)：
    - `entered_barrier(prev, curr, &rects)` → `Some(pos)`
    - `display_containing_idx(&rects, prev_pos)` → `Some(idx)`
    - `let key = BarrierKey { pos, monitor: displays[idx].monitor_id.clone(), offset: 0, span: 10000 }`
    - `if clients.contains(&key) { Some(key) } else { None }`
16. 命中 → `BeginPending` 事件 → `conn.send(ProtoEvent::Enter)` 给被控端

### 1.4 编译 + 测试
- `cargo build --workspace` ✅ 通过
- `cargo test --workspace` ✅ 164/164 pass（input-capture 68 + lan-mouse 67 + lan-mouse-ipc 15 + lan-mouse-proto 5 + 输入路由 7 + QUIC smoke 2）
- `cargo clippy --workspace --all-targets -- -D warnings` ✅ 无 warning

---

## §2 已排除的嫌疑点

| 嫌疑 | 验证结果 | 证据 |
|---|---|---|
| 编译错（漏改某 caller / 签名错） | ❌ 排除 | `cargo build --workspace` 0 错误；`cargo test --workspace` 164/164 |
| `displays` 为空（`update_bounds` 失败） | ❌ 排除 | `InputCaptureState::new` (macos.rs:117) 同步 `update_bounds()?`，失败会让 daemon 启动失败。`create_event_tap` 起来的 event tap 还在跑 → construction 成功 → displays 必然已填 |
| `active_clients` HashSet 类型错误（`Lazy` 未初始化） | ❌ 排除 | `once_cell::unsync::Lazy<HashSet<BarrierKey>>` 首次 insert 时自动 init 空 HashSet，后续 insert/contains 正常 |
| `crossed` 没调或 `capture.next()` 没 poll | ❌ 排除 | `do_capture_session` main loop `tokio::select!` 第一个 arm 持续 poll `capture.next()`；用户看到 CGEventTap callback 正常注册（pairing 成功暗示 daemon 完全跑起来） |
| `Capture::create` 漏传 `monitor` 字段 | ❌ 排除 | 全链路 `key.clone()` 保留所有字段（service.rs:693 / capture.rs:345 / lib.rs:156 / macos.rs:1487 / producer event insert） |
| Windows backend 仍走旧 `from_pos` 路径 | ❌ 排除 | 3afeeff 已 `check_client_activation` 改成 `activation_pure` 薄包装（windows/event_thread.rs:433-453），验证了 `cargo test -p input-capture --lib geometry` 6 个 activation_pure_w1-w6 case 全绿 |
| layer_shell backend 仍走旧 `from_pos` 路径 | ❌ 排除 | d7d2c58 已 `Capture::create/destroy` 直接 `add_client(key)` / `delete_client(key)`，完整 BarrierKey 入 `active_positions`，3 个 cfg-gate 单测绿 |
| libei backend 仍有 `from_pos` 问题 | ❌ 排除 | libei 不做 query 重建（直接喂整个 `active_clients` 给 EIS `set_pointer_barriers`），3afeeff 加的 `select_barriers_with_monitor_field` 单测锁住此行为 |
| WS UpdateMonitor 链路断（Vue → daemon） | ❌ 排除 | `FrontendRequest::UpdateMonitor` handler、Vue `diffClientConfigPatch`、`updateClientConfig` 全链路 23 个 vitest case + Rust 单测全绿 |
| Frontend reconnect 后 dropdown 失去 monitor 列表 | ❌ 排除 | 7ed1352 修复，`sync_frontend` 现在 re-broadcast `last_monitors` 给新 WS 连接 |
| `displays` vs `last_monitors` IOKit 暂态失败导致 ID 不一致 | ⚠️ **可能** | 见 §3 H2，作为次要嫌疑 |

---

## §3 最可能的根因（按可能性排序）

### H1（**最可能，~85% 概率**）— legacy `monitor=None` 客户端 BarrierKey 与真实 displays 永远 mismatch

#### 现象与定位
`build_display_bounds`（macos.rs:638-657）+ Windows `update_display_regions`（windows/event_thread.rs:558-578）+ libei `select_barriers`（libei.rs:277）三个 backend 在生产路径上**100%** 把 `monitor_id` 字段塞成 `Some(m.id.clone())`：

```rust
// input-capture/src/macos.rs:638-657
fn build_display_bounds(
    active_ids: &[CGDirectDisplayID],
    monitors: &[MonitorInfo],
) -> Vec<DisplayBound> {
    let _ = active_ids;
    monitors
        .iter()
        .map(|m| {
            DisplayBound::new(
                DisplayRect::new(m.position.0 as f64, m.position.1 as f64,
                                 m.size.0 as f64, m.size.1 as f64),
                Some(m.id.clone()),  // ← 永远 Some，从不为 None
            )
        })
        .collect()
}
```

而 `query_pure`（geometry/mod.rs:404-428）构造的查询 key 永远是：

```rust
let key = BarrierKey {
    pos,
    monitor: displays[idx].monitor_id.clone(),  // ← Some("macos:...") 永远不会是 None
    offset: 0,
    span: 10000,
};
```

如果用户的 client 配置里 `c.monitor = None`（**这正是默认行为** — 没选 dropdown / 用旧 config / 刚 `add_client` 还没选），那么：

- `active_clients` 里插入的是 `{ pos: Top, monitor: None, ... }`（来自 `client.rs:160-170` 的 `get_key`，`c.monitor.clone()` 复制 None）
- `crossed_pure` 构造的 query key 是 `{ pos: Top, monitor: Some("macos:..."), ... }`
- `clients.contains(&key)` → `false`（`Option<String>::None != Option<String>::Some("...")`）
- `crossed_pure` 返回 `None` → `crossed()` 返回 `None` → 没有任何 `BeginPending` 发出
- 主控端日志**静默**（因为 `log::debug!("Crossed barrier into: {key:?}")` 在 `?` 之后，只命中才打），被控端自然收不到 Enter

#### 与 PLAN 文字的对照（**重要**）
PLAN §3 STEP-3.4 (line 162) 明确写：
> `monitor_id == None` on the containing display → legacy "monitor-agnostic" lookup; matches keys with `monitor: None`

但 **STEP-3.4 落地时这条 invariant 没真正实现**：`build_display_bounds` 永远填 `Some(...)`，所以"display 上的 monitor_id 是 None"这个前提在生产路径上**永远不会成立**。

#### 假设链路（具体输入 → 错输出）
1. 用户在 macOS 双屏下打开 GUI（dropdown 显示 "Any (back-compat)" + 两个显示器）
2. 用户授权对端指纹（pairing 成功）
3. 用户 activate client（toggle 打开，client 状态变 `active=true`）
4. **`ClientConfig.monitor` 维持 None**（用户没碰 dropdown，或 dropdown 是后来才刷出来，或用旧 config）
5. `service::activate_client` 调 `client_manager.get_key(handle)` → `BarrierKey { pos: Top, monitor: None, ... }`
6. `capture.create(handle, &key, Default)` → `CaptureRequest::Create` → `active_clients = { { pos: Top, monitor: None, ... } }`
7. `displays` 此时有 2 个 entry：`{ rect: (0,0,1920,1080), monitor_id: Some("macos:...") }` 和 `{ rect: (1920,0,1920,1080), monitor_id: Some("macos:...") }`
8. 鼠标移到右屏顶部：`CGEventType::MouseMoved` 触发回调
9. `prev_pos = (2500, 0)` (clamped) / `curr_pos = (2500, -2)` (predicted)
10. `entered_barrier` → `Some(Top)` ✓
11. `display_containing_idx` → `Some(1)` ✓
12. `key.monitor = displays[1].monitor_id.clone() = Some("macos:...")`
13. `active_clients.contains(&key)` where `key = { pos: Top, monitor: Some("macos:..."), ... }` → **false**
14. `crossed_pure` 返回 `None` → 无 `log::debug!("Crossed barrier into")` → 无 `BeginPending` → 无 Enter
15. **被控端永远收不到 ProtoEvent::Enter，鼠标不动**

#### 验证证据（单测已写已跑已通过 revert）

临时写了一个 production-realistic 单测（**已 revert**，仅用于调研）：

```rust
#[test]
fn debug_legacy_none_against_real_displays() {
    // 2x1 横排，display 都带真实 IOKit-style monitor_id（复刻 build_display_bounds 行为）
    let displays = vec![
        DisplayBound::new(DisplayRect::new(0.0, 0.0, 1920.0, 1080.0),
                          Some("macos:aaaa:1111:serial:loc1".to_string())),
        DisplayBound::new(DisplayRect::new(1920.0, 0.0, 1920.0, 1080.0),
                          Some("macos:bbbb:2222:serial:loc2".to_string())),
    ];
    // 客户端用 legacy 默认值（无 monitor 字段 / 没选 dropdown）
    let mut active = HashSet::new();
    active.insert(BarrierKey { pos: Position::Top, monitor: None, offset: 0, span: 10000 });
    // 鼠标从 display_0 顶部越界
    let got = crossed_pure((500.0, 0.0), (500.0, -2.0), &displays, &active);
    assert!(got.is_some(), "legacy None must match against real displays");
}
```

**运行结果**：`got = None` → `assert!` panic，回归被实测重现。

> 注意：现有 `crossed_pure_monitor_id_none_uses_legacy_key` 单测**通过**但**测试是 vacuous 的** — 它手动构造一个 `DisplayBound { monitor_id: None }`，但生产路径上 `build_display_bounds` 永远不产这种 `DisplayBound`。这个测试锁住的是"如果 DisplayBound 是 None 就走 legacy"的逻辑，但生产根本没触发这个分支。

#### 修复方向
**最小修复**（在 `query_pure` 加 legacy fallback）：

```rust
fn query_pure(
    prev_pos: (f64, f64),
    curr_pos: (f64, f64),
    displays: &[DisplayBound],
    clients: &HashSet<BarrierKey>,
) -> Option<BarrierKey> {
    let rects: Vec<DisplayRect> = displays.iter().map(|d| d.rect).collect();
    let pos = entered_barrier(prev_pos, curr_pos, &rects)?;
    let idx = display_containing_idx(&rects, prev_pos)?;
    // **M3 fix**: prefer the M3 monitor-specific key when present,
    // fall back to the legacy `monitor: None` key when not. The PLAN
    // §3 STEP-3.4 "monitor_id == None → legacy fallback" invariant is
    // not implementable via `build_display_bounds` (which always
    // produces `Some(id)`), so we implement it as a post-query
    // fallback here. This restores pre-M3 single-monitor behavior for
    // clients whose `c.monitor` is `None` (legacy configs / dropdown
    // not yet picked).
    let specific_key = BarrierKey {
        pos,
        monitor: displays[idx].monitor_id.clone(),
        offset: 0,
        span: 10000,
    };
    if clients.contains(&specific_key) {
        return Some(specific_key);
    }
    // Legacy fallback: any client that bound to this `pos` without a
    // specific monitor — i.e. `monitor: None`. This matches the
    // pre-M3.4 semantics where the query key was always `monitor: None`.
    let legacy_key = BarrierKey { pos, monitor: None, offset: 0, span: 10000 };
    if clients.contains(&legacy_key) {
        return Some(legacy_key);
    }
    None
}
```

**关键不变量（修复后必须守住的）**：
1. **M3 specific-monitor binding 优先**：如果 `c.monitor = Some(id)` 且 `id == displays[idx].monitor_id` → 命中 specific key（原有 M3 行为不变）
2. **Legacy None 后备**：如果 `c.monitor = None` → 命中 legacy key（恢复 pre-M3.4 行为）
3. **M3 specific binding 不命中时不**"降级"**到 legacy**：如果用户 explicitly 选了 monitor X，但鼠标在 monitor Y 上 → 仍然不命中（用户必须选对 monitor）— 这与现有 `crossed_pure_c3_display0_top_misses_legacy_active` 的语义一致
4. **多个 client 共用同 pos 时 specific 优先 legacy**：与现有 `activation_pure_w6_display0_top_picks_d0_over_none` 一致 — 不需要为这个 case 再加判断，因为 `clients.contains(&specific_key)` 短路在前

**涉及文件**：
- `input-capture/src/geometry/mod.rs`（改 `query_pure` + 加 1 个单测覆盖 production-realistic 场景）
- 可能加一个 `crossed_pure_legacy_fallback_against_real_displays` 单测（锁住 H1 修复，防回归）

**完成标志**：
- `cargo test -p input-capture --lib geometry` 新增 1 case 绿 + 既有 24 case 全绿
- `cargo build --workspace` 0 错误
- macOS 真双屏回归（人类配合）：**legacy config / 没选 dropdown** 这两种场景下，鼠标移到右屏顶 → daemon 日志看到 `Crossed barrier into: BarrierKey { pos: Top, monitor: None, ... }` + 被控端收到 Enter

---

### H2（**次要，~10% 概率**）— `displays` 与 `last_monitors` ID 暂态不一致

#### 现象与定位
macOS backend 在两个不同时刻独立调用 `enumerate_monitors_for_ids(&active_ids)`：
1. `update_bounds()`（macos.rs:194）— 喂给 `displays`
2. `enumerate_monitors()`（macos.rs:564）— 喂给 `monitors_tx` → 最终到 `FrontendEvent::MonitorsChanged` → 用户 dropdown

如果两次调用之间 `read_display_info`（macos.rs:685）对同一个 `display_id` 的 IOKit 返回结果不同（一成功一失败 / serial/location 变化），那么：
- `displays[0].monitor_id = "macos:vvvv:pppp:real_serial:location"`
- dropdown 显示 `monitors[0].id = "macos:0000:0000::unknown-{display_id}"`
- 用户点 dropdown 选中的 ID 跟 `displays` 里的 ID **不同**
- `active_clients` 里的 key 用 dropdown ID，`crossed_pure` 用 displays ID → mismatch

但这要求：
- 同一进程内 IOKit 对同一 display_id 在毫秒级间隔内返回不同结果
- 通常只有 TCC 拒绝 / IOKit 初始化未完成时才会出现
- 大多数正常启动时不会发生

#### 验证方式
让用户跑 `RUST_LOG=lan_mouse_service=trace,input_capture=trace` 重启 daemon，对比：
- daemon 启动时 `initial monitors: ...` 那段 INFO 日志
- `display reconfigured: ...` / `Updated displays: ...` 那段 DEBUG 日志
- 两个列表里的 `id=...` 字段是否 byte-for-byte 一致

如果不一致 → 加 `RUST_LOG=input_capture=debug` 重跑 + 看 `read_display_info` 的 `service == 0` / `dict_ref.is_null()` 分支是否命中。

#### 修复方向
**短期**：在 `update_bounds` 与 `enumerate_monitors` 都用同一个 `active_ids` + 缓存 IOKit 结果，强制两个路径走同一份 `MonitorInfo`（即把"single source of truth"扩展到整个 backend state，而不是每个函数独立跑一次 IOKit）。

**长期**：把 `displays` 和 `monitors_tx` 都从一个共享的 `Arc<Mutex<Vec<MonitorInfo>>>` 派生。

但这要先**确认 H1 修复后用户真机仍不工作**才需要做。

---

### H3（**~5% 概率**）— macOS `Capture::create` fire-and-forget 竞态

#### 现象与定位
macOS `Capture::create`（macos.rs:1483）：

```rust
async fn create(&mut self, key: &BarrierKey) -> Result<(), CaptureError> {
    let key = key.clone();
    let notify_tx = self.notify_tx.clone();
    tokio::task::spawn_local(async move {
        log::debug!("creating capture, {key:?}");
        let _ = notify_tx.send(ProducerEvent::Create(key)).await;
        log::debug!("done !");
    });
    Ok(())
}
```

`spawn_local` + 立刻 `Ok(())`，所以 `InputCapture::create` 返回时 `active_clients.insert(k)` **可能还没执行**。如果用户 activate 之后**立刻**（毫秒级）移动鼠标越过边缘，第一次 mouse event 时 `active_clients` 仍空 → `crossed_pure` 返回 None。

但这是个**首次穿越就失败、之后稳态成功**的 race，应该不会让用户持续报告"完全不触发"。如果用户描述的是"持续不触发"，H3 不是根因。

#### 验证方式
让用户观察：
- 启动 daemon → activate client → 等 5 秒 → 移动鼠标到边缘
- 如果 5 秒后触发 → H3 是根因
- 如果 5 秒后仍不触发 → 不是 H3

#### 修复方向
把 `Capture::create` 改成 sync barrier：等 `notify_tx.send(...).await` 完成 + producer task 确认 `active_clients.insert` 已执行后才返回。或加一个 `Capture::create_sync` 方法，让 `InputCapture::create` 等到 backend 确认。

但前提是用户报告的"持续不触发"跟 race 时序不符，所以**这是 P2 改进，不是当前 P0 修复**。

---

## §4 建议下一步

### 最小修复路径（H1 修复）
- **修改文件**：仅 `input-capture/src/geometry/mod.rs`
  - `query_pure` 加 legacy fallback（见 §3 H1 修复方向代码）
  - 新增 1 个单测 `crossed_pure_legacy_fallback_against_real_displays`（用 production-realistic fixture — 即两个 `Some(...)` monitor_id 的 DisplayBound + 1 个 `monitor: None` 的 active client — 锁住"legacy None 命中"行为）
- **不要改** `build_display_bounds`（保持现状，理由：fix 集中在 query_pure 更简单 + 不破坏 M3.4 已落的 `display_containing` 语义）
- **不要改** PLAN 文档（PLAN §3 STEP-3.4 的 invariant 文字本来就对，只是落地时漏了实现；应该改 PLAN 文档的"完成标志"行加一句"query_pure 实现 legacy fallback 单测"作为补丁说明）
- **预估时间**：15 分钟（5 分钟改代码 + 5 分钟加单测 + 5 分钟跑测试 + 验证 build）

### 派哪个 sub-agent
- **plan-step-executor**（已在 .LEADER.md 里授权）执行 H1 修复
- **step-validator**（可选）做单测回归验证

### 验证流程
1. executor 改 `geometry/mod.rs::query_pure` + 加单测
2. executor 自验证：
   - `cargo test -p input-capture --lib geometry` 全绿（既有 24 case + 新增 1 case = 25 case）
   - `cargo build --workspace` 通过
   - `cargo clippy -p input-capture --all-targets -- -D warnings` 0 warning
3. leader 接受 → commit（message 写明："fix(input-capture): restore legacy `monitor=None` barrier trigger after M3.4 monitor_id injection"）
4. **关键**：leader 提示用户**重测真机**：
   - 场景 A（legacy）：用旧 config 或刚 `add_client` 没选 dropdown → activate → 鼠标到边缘 → 应触发
   - 场景 B（M3）：dropdown 选右屏绑 top → 鼠标到右屏顶 → 应触发
   - 场景 C（M3 negative）：dropdown 选左屏绑 top → 鼠标到右屏顶 → 应**不**触发（行为正确）

### 如果 H1 修复后用户仍报告"完全不触发"
按 H2 / H3 顺序继续排查：
1. H2：让用户跑 trace log，对比两个 ID 列表
2. H3：让用户加 5 秒等待后再移动鼠标

---

## §5 重要观察（不是根因，但记录下来供未来 reference）

### 5.1 单测覆盖盲点
`crossed_pure_monitor_id_none_uses_legacy_key`（geometry/mod.rs:1262）的 fixture 是**手造**的 `DisplayBound { monitor_id: None }`，但生产路径（`build_display_bounds` / Windows `update_display_regions`）**永远不会产生**这种 DisplayBound — 它们永远填 `Some(m.id.clone())`。所以这个测试**通过了但没真正覆盖生产场景**。修复时必须新增一个"production-realistic" fixture（displays 都用 `Some(...)` monitor_id + active client 用 `monitor: None`）的单测。

### 5.2 `monitor_id` 字段语义飘移
PLAN §3 STEP-3.4 设计的"display 端 monitor_id 是 None → 走 legacy"语义，本质是把 "display 是否被稳定识别" 作为 fallback 开关。但实际生产中 IOKit 永远能给个 ID（哪怕是 fallback "macos:0000:0000::Unknown"），所以这个开关永远不触发。**正确的 fallback 开关应该在 client 端**：`c.monitor == None`（用户没指定）→ 走 legacy；`c.monitor == Some(id)`（用户指定了）→ 走 specific。这是 PLAN 文字与实现错位的根本原因。

### 5.3 用户原始报告与 PLAN §8 测试矩阵 line 336 的关系
PLAN §8 line 336 明确把"旧 config（无 `monitor` 字段）+ 单 monitor 场景：行为不变"作为**回归保护**测试项。但 STEP-3.4/3.5/3.6 的单测都没有真正覆盖这个 case — 既有的 `crossed_pure_monitor_id_none_uses_legacy_key` 是 vacuous 的（见 5.1）。**真机回归测试这道关没拦住这个 bug**，需要 executor 在修 H1 时**同步把 PLAN §8 line 336 的语义真正落到单测里**。

### 5.4 其他 backend 同形 bug 风险
- **Windows** `update_display_regions`（windows/event_thread.rs:558-578）也是 `Some(build_stable_id(...))`，同样的 legacy None 触发不到。但 macOS 修好 query_pure 后 Windows 自动跟着修好（`activation_pure` 走同一个 `query_pure` helper）
- **layer_shell** 不受影响（它不做 query，是同一份 HashSet 直接匹配，3.4 已经把 monitor 字段存进 active_positions）
- **libei** 不受影响（EIS 直接喂整个 active_clients，3.5 单测锁住）
- **dummy** 不受影响（它是 stub）

**结论**：H1 修复**一次性解决 macOS + Windows 两个 backend 的 legacy None 回归**，layer_shell / libei / dummy 不需要改。

---

## §6 调研期间临时改动

- 在 `input-capture/src/geometry/mod.rs:1471-1510` 临时加了 `debug_legacy_none_against_real_displays` 单测，验证 H1 后已 **revert**（`git diff` 应为干净）
- 没有 commit 任何代码
- 没有改 PLAN 文档
- 没有改任何 .md / .rs（除上面 revert 完的 debug 单测）

调研结束。leader 可派 plan-step-executor 执行 §4 修复路径。
