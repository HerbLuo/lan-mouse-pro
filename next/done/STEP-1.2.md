# STEP 1.2 — 五个 backend 同步改签名

> PLAN §M1 / STEP-1.2
> 执行日期：2026-09-06　实际耗时：~25 min
> 结论：通过

## 1. 做了什么

把 STEP-1.1 的 "sig-only 兼容层" 升级为完整迁移：5 个 backend 的内部 producer-event 通道、event_rx、thread-local 状态、`Stream::Item` 现在直接以 `BarrierKey` 为键，不再做 `Position → BarrierKey` 的边界 lift。`monitor / offset / span` 全程 `None / 0 / 10000` 默认值（与 STEP-1.1 默认兼容层等价），行为零差异。

`x11.rs` 是 stub（`new()` 返回 `NotImplemented`），跳过。

### 1.1 `dummy.rs` — `DummyInputCapture::with_keys(Vec<BarrierKey>)` 注入式 schedule

- `new()` 改为 `with_keys(vec![BarrierKey::from_pos(Position::Left)])` 的薄封装；保留默认行为。
- `with_keys(Vec<BarrierKey>)`：构造时若 `keys.is_empty()` 回退到默认 single-Left（避免 modulo-by-zero）。
- `poll_next`：每次 tick 从 `keys[next_key_idx % keys.len()]` 取 key 并发出；`next_key_idx.wrapping_add(1)` 自增。
- 新增 5 个单测（详见 §1.6）。

### 1.2 `layer_shell.rs` — `Window.key: BarrierKey` + `active_positions: HashSet<BarrierKey>`

- `Window.pos: Position` → `Window.key: BarrierKey`（结构体内置）
- `Window::new` 接 `key: BarrierKey`；`width/height` 与 `Anchor` 仍从 `key.pos` 派生
- `State.active_positions: HashSet<Position>` → `HashSet<BarrierKey>`
- `LayerShellInputCapture::add_client(key)` / `delete_client(key)` / `State::add_client(key)` / `update_windows()` 全部以 `BarrierKey` 流转
- 全部 push 站点（`wl_pointer::Enter/Button/Axis/AxisValue120`、`wl_keyboard::Key/Modifiers`、`ZwpRelativePointerV1::RelativeMotion`）改为 `window.key.clone()` 直发，去掉 `BarrierKey::from_pos(window.pos)` lift
- `pending_events: VecDeque<(BarrierKey, _)>` 类型签名不变（STEP-1.1 已升级）

### 1.3 `libei.rs` — `LibeiNotifyEvent` / `event_rx` / `active_clients` / `key_for_barrier_id` 全部 `BarrierKey`

- `LibeiNotifyEvent::{Create, Destroy}` 从 `Position` 改为 `BarrierKey`（含 `String`，所以 enum 改成 `Clone, Debug` 不再 `Copy`）
- `event_rx: Receiver<(Position, _)>` → `Receiver<(BarrierKey, _)>`
- `event_tx: Sender<(Position, _)>` → `Sender<(BarrierKey, _)>`（`libei_event_handler`、`do_capture`、`do_capture_session`、`handle_ei_event` 全部）
- `active_clients: Vec<Position>` → `Vec<BarrierKey>`
- `pos_for_barrier_id: HashMap<BarrierID, Position>` → `key_for_barrier_id: HashMap<BarrierID, BarrierKey>`
- `select_barriers(zones, clients: &[BarrierKey], …)`：`pos_to_barrier(r, key.pos)` 仍只读 `pos`
- `release_capture(input_capture, session, activated, &BarrierKey)`（原 `current_pos: Position`）
- `current_key: Rc<Cell<Option<BarrierKey>>>` 替换原 `current_pos`；激活分支取出 `key.clone()` 用于 `current_key.replace()` 和 `event_tx.send((key.clone(), …))`
- `do_capture` 中 `LibeiNotifyEvent::Destroy(k) => active_clients.retain(|existing| existing != &k)`（改成引用比较，避免 `Copy` 依赖）
- `Stream::poll_next` 去掉 `BarrierKey::from_pos(pos)` lift，直接 `event_rx.poll_recv`

### 1.4 `macos.rs` — `ProducerEvent` / `event_tx` / `current_key` / `pending_key` / `active_clients` 全部 `BarrierKey`

- `ProducerEvent::{Create, Destroy, Grab, StartCapture, CancelPending}` 全部携带 `BarrierKey`（替代 `Position`）
- `event_tx: Sender<(Position, _)>` → `Sender<(BarrierKey, _)>`
- `InputCaptureState::active_clients: HashSet<Position>` → `HashSet<BarrierKey>`
- `current_pos: Option<Position>` / `pending_pos: Option<Position>` → `current_key: Option<BarrierKey>` / `pending_key: Option<BarrierKey>`
- `crossed()` 返回 `Option<BarrierKey>`：内部 `entered_barrier` 仍返回 `Option<Position>`，在边界用 `BarrierKey::from_pos(pos)` lift 后再做 `active_clients.contains(&key)` 检查（这是 STEP-1.2 唯一一处保留 lift 的地方——因为 `entered_barrier` 是几何算法，无法感知 BarrierKey）
- 整个 tap callback 的 `pending_pos` / `new_pos` / `capture_position` 变量名重命名为 `pending_key` / `new_key` / `capture_key`；`state.reset_cursor(key.pos)`（旧 `reset_cursor(current_pos)`）等只读 `pos` 字段的路径照旧
- `ProducerEvent::StartCapture(key)` 路径：`reset_cursor(key.pos)?` 然后 emit `Begin`
- `Capture::create / destroy / start_capture / cancel_pending` 改为 clone `key` 后 send
- `Stream::poll_next` 去掉 lift

### 1.5 `windows.rs` + `windows/event_thread.rs` — 全栈 `BarrierKey`

`windows.rs`：
- `event_rx: Receiver<(Position, _)>` → `Receiver<(BarrierKey, _)>`
- `event_thread.create / destroy / start_capture / cancel_pending(key: BarrierKey)`（原 `key.pos` 转发，现在直接 clone）
- `Stream::poll_next` 去掉 lift

`windows/event_thread.rs`：
- `event_tx: Sender<(Position, _)>` → `Sender<(BarrierKey, _)>`
- `start / start_routine` 参数同步改
- `ClientUpdate::{Create, Destroy, StartCapture, CancelPending}` 全部携带 `BarrierKey`
- `EVENT_TX: RefCell<Option<Sender<(Position, _)>>>` → `<Sender<(BarrierKey, _)>>`
- `blocking_send_event(key, event)` / `try_send_event(key, event)` 接 `BarrierKey`
- `CLIENTS: HashSet<Position>` → `HashSet<BarrierKey>`
- `ACTIVE_CLIENT: Cell<Option<Position>>` → `<Option<BarrierKey>>`
- `PENDING_CLIENT: Cell<Option<Position>>` → `<Option<BarrierKey>>`
- `check_client_activation`：`cursor_within(curr_pos, displays, pending_key.pos)`（只读 pos）；`entered_barrier` 返回 `Option<Position>`，在边界 lift 成 `BarrierKey::from_pos(pos)`；`PENDING_CLIENT.replace(Some(key.clone()))`；`blocking_send_event(key, BeginPending)`
- `mouse_proc` / `kybrd_proc`：`try_send_event(active_key, _)` / `try_send_event(active_client_key, _)`（变量名改 `key`/`client`，本质不变）
- `update_clients`：`CLIENTS.insert(key)` / `CLIENTS.remove(&key)` / `PENDING_CLIENT.get() != Some(key.clone())` / `ACTIVE_CLIENT.replace(Some(key.clone()))` 等所有 `Option<BarrierKey>` 比较都改成 `Some(k.clone())`

### 1.6 单测（dummy 新增 5 个）

| 测试 | 覆盖点 |
|---|---|
| `default_emits_legacy_left_key` | `new()` 与旧 `Position::Left` 行为一致 |
| `with_keys_round_robins_across_schedule` | 多 key schedule 循环发出，到尾回头 |
| `with_keys_empty_falls_back_to_default` | 空 list 回退 single-Left |
| `with_keys_preserves_monitor_offset_span` | `monitor=Some("display-A") / offset=2500 / span=5000` 全字段透传 |
| `first_event_is_begin_regardless_of_schedule` | 首事件恒为 `Begin`（旧契约保留） |

## 2. 验证结果

### 2.1 `cargo build -p input-capture`

```
Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.29s
```

无 error / 无 warning。

### 2.2 `cargo test -p input-capture --lib`

```
test result: ok. 32 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

新增 5 个 dummy 测试（27 → 32），旧 27 个（geometry + poll_next）全部保留。

### 2.3 `cargo fmt --check -p input-capture`

无 diff（已 `cargo fmt` 后）。

### 2.4 `cargo clippy -p input-capture --all-targets -- -D warnings`

```
Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.69s
```

0 warning / 0 error。

### 2.5 `cargo build --workspace`（预期部分失败）

按 STEP-1.1 + STEP-1.2 完成标志组合，workspace 故意不应编过：

```
error[E0308]: mismatched types
   --> input-capture/src/lib.rs:148:18
   --> input-capture/src/lib.rs:203:12
   --> input-capture/src/lib.rs:211:12
   --> src/capture.rs:557:44
   --> src/capture.rs:713:67
   --> src/capture.rs:720:72
   --> src/capture.rs:793:43
   --> src/capture.rs:822:56
   --> src/capture.rs:1197:60
   --> src/capture.rs:1229:56
   --> src/capture.rs:1383:52
   --> src/capture_test.rs:17:33
   --> src/capture_test.rs:18:33
   --> src/capture_test.rs:19:33
   --> src/capture_test.rs:20:33
   --> src/capture_test.rs:21:33
error: could not compile `lan-mouse` (lib) due to 13 previous errors
```

13 处错误全部在 `src/capture.rs`（9 处）和 `src/capture_test.rs`（5 处），**正是 STEP-1.3 要改的文件**。`src/service.rs` 仍 0 处错误。`input-capture/src/lib.rs` 的 3 处只是 `BarrierKey::create/start_capture/cancel_pending` 的 trait 声明行 span（被 capture.rs 调用时报告）。

## 3. 与 PLAN 的偏差

### 3.1 SUGGESTION #1 已关闭

STEP-1.1 文档化的"backend 改动应在 STEP-1.2 完成（PLAN §M1 表里 STEP-1.2 列了 6 个 backend）"——本 STEP 已落地。详见 `next/SUGGESTION-FIXED.md` #1。

### 3.2 `entered_barrier` 仍是几何原语，返回 `Position`

跨 backend 唯一保留的"边界 lift"在两处：

- macOS `InputCaptureState::crossed()`：内部仍调 `crate::geometry::entered_barrier(...)` 拿 `Position`，再 `BarrierKey::from_pos(pos)` 包成 key 喂给 `active_clients.contains(&key)`。
- Windows `check_client_activation`：`entered_barrier` 拿 `Position`，lift 成 `BarrierKey::from_pos(pos)` 再 `CLIENTS.contains(&key)`。

理由：`geometry::entered_barrier` 是 STEP-0.x / STEP-1.1 落地的纯几何算法，签名是 `(prev, curr, &[DisplayRect]) -> Option<Position>`；它的"语义"是"哪条边被跨过"，没有 monitor 概念。M2+ 让它返回 `Option<BarrierKey>`（带 monitor）会让几何模块承担 runtime 信息，违背 "geometry = 纯算法" 的分层。所以选择 backend 在调用点 lift——这是 M1 的干净边界。

### 3.3 `LibeiNotifyEvent` 改为 `Clone, Debug`（不再 `Copy`）

携带 `BarrierKey`（含 `Option<String>`）后无法 `Copy`。`do_capture` 的 `match event { LibeiNotifyEvent::Create(k) => … }` 改成 `k.clone()` / `&k` 引用比较。

## 4. 处理的 SUGGESTION 项

- **#1** (STEP-1.1 PLAN 偏差 / backend 改动时机)：已关闭 → `SUGGESTION-FIXED.md`。
- 无新增 SUGGESTION 条目。

## 5. 闸门检查（时间门 / milestone 边界门）

- **时间门**：~25 min ✅（PLAN 估时 30 min，未超时）
- **milestone 边界门**：✅（不触碰 M2 显示器枚举 / M3 绑定 UI / M4 画布。`BarrierKey.monitor / offset / span` 全程 `None / 0 / 10000` 默认值；backend 只读 `pos`；dummy 的 `with_keys` 接口只是注入 schedule 的钩子，不引入 monitor 信息）

## 6. 遗留

### 6.1 留给 STEP-1.3

`src/capture.rs`（9 处错）+ `src/capture_test.rs`（5 处错）：`CaptureTask` 持 `Vec<(CaptureHandle, BarrierKey, CaptureType)>`、`State::Pending { handle, key }`、`activate_client` 用 `client_manager.get_key(handle)` 取 key、`client_at(key)` 替换 `client_at(pos)`、`update_pos` 重算 key 后 deactivate/recreate。

### 6.2 留给 M2+

- `entered_barrier` / `crossed` 在 macOS / Windows 的边界 lift 仍是 `BarrierKey::from_pos(pos)`；M2 显示器枚举就位后，改为基于 `display_containing(prev_pos) → monitor_id + edge_idx` 计算 `BarrierKey { pos, monitor: Some(id), offset, span }`。
- layer_shell `Window::new` 接 `key: BarrierKey`，但 `width/height` / `set_margin` / `set_size` 只读 `key.pos`，未消费 `monitor / offset / span`——这是 M4 子边屏障 (STEP-4.3) 的工作。
- libei `select_barriers` 仍按 `key.pos` 遍历 zones 拿 `pos_to_barrier(r, pos)`——portal 限制下子区段不支持，M4 STEP-4.5 会显式记录这一降级。
- dummy `with_keys` 的 schedule 不带时间 / 触发逻辑，仅 round-robin——M2+ service 注入更复杂的"触发某个 key 后停 N ms 再触发下一个"由调用方封装。

## 7. 下一步

按 PLAN §M1 依赖顺序：**STEP-1.3**（`src/capture.rs` + `src/capture_test.rs` 的 BarrierKey 适配）。之后 **STEP-1.4** 跑 milestone 收尾的 fmt / clippy / 单屏 GUI 回归（人类配合）。
