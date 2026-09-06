# STEP 1.1 — BarrierKey 数据模型重构

> PLAN §M1 / STEP-1.1
> 执行日期：2026-09-06　实际耗时：~30 min
> 结论：通过

## 1. 做了什么

### 1.1 新增 `geometry::BarrierKey`（`input-capture/src/geometry/mod.rs`）

新增数据类型（PLAN §M1 STEP-1.1 主体）：

```rust
pub type MonitorId = String;

#[derive(Debug, Clone, Eq, Hash, PartialEq)]
pub struct BarrierKey {
    pub pos: Position,
    pub monitor: Option<MonitorId>,
    pub offset: u16,    // permyriad (1/10000) of edge length
    pub span: u16,      // permyriad (1/10000) of edge length
}
```

`Default` = `pos = Left, monitor = None, offset = 0, span = 10000`（全边 + 无 monitor，向后兼容）。
另提供 `BarrierKey::from_pos(Position)` 便捷构造器，给 M1 阶段只携带 `Position` 的 backend 升级用。

注意：`Option<String>` 使 `BarrierKey` 无法 `Copy`（`Hash + Eq + Clone` 即可，作为 `HashMap` 主键够用）。

### 1.2 改写 `InputCapture` 公共 API（`input-capture/src/lib.rs`）

- `position_map: HashMap<BarrierKey, Vec<CaptureHandle>>`（原 `HashMap<Position, _>`）
- `id_map: HashMap<CaptureHandle, BarrierKey>`（原 `HashMap<_, Position>`）
- `create(id, key: &BarrierKey)` / `destroy(id)` / `start_capture(key: &BarrierKey)` / `cancel_pending(key: &BarrierKey)`
- `Capture` trait 同步改签名：`Stream<Item = Result<(BarrierKey, CaptureEvent), _>>`
- 删除 `mem::swap` 抖动模式，改用 `subscribers.clone()` 快照 + 显式 fan-out push，附 WAKER INVARIANT 注释

### 1.3 五个 backend 同步签名（`dummy / x11 / libei / layer_shell / macos / windows`）

**PLAN 把 backend 改动放在 STEP-1.2**（"内部存储 `BarrierKey` 但**目前只读 `pos` 字段**"），但 STEP-1.1 的 `cargo build -p input-capture` 必须通过——`Capture` trait 是 crate 内公共契约，trait 改了 backend 不动就编不过。**故 STEP-1.1 仅做 sig-only 兼容层**，让 backend 仍以 `Position` 形式干活（详见 SUGGESTION #1）：

- `dummy / x11`：`create / destroy` 收 `&BarrierKey`，Stream::Item 包 `BarrierKey::default()` / `BarrierKey::from_pos(...)`
- `libei`：内部 `notify_capture` 通道仍走 `Position`，Stream::poll_recv 边界用 `BarrierKey::from_pos(pos)` lift
- `layer_shell`：`pending_events: VecDeque<(BarrierKey, CaptureEvent)>`，每个 push 站点包 `BarrierKey::from_pos(window.pos)`
- `macos`：ProducerEvent 通道保持 `Position`，Stream::poll_next 边界用 `BarrierKey::from_pos(pos)` lift
- `windows`：内部 `event_rx` 保持 `Receiver<(Position, CaptureEvent)>`，Stream::poll_next 边界 lift

### 1.4 `poll_next` 重写 + waker 单测（`input-capture/src/lib.rs::poll_next_tests`）

新 `poll_next` 的 fan-out path：

1. 排空 `pending`（已就绪事件继续 fan-out）
2. `self.capture.poll_next_unpin(cx)` 注册当前 waker 并等下一个 backend 事件
3. 处理 stream closed / inner error
4. 抓 `(key, event)`；键盘态追踪
5. **`subscribers = position_map.get(&key).cloned().unwrap_or_default()`**
6. `len == 0 → Poll::Pending`（drop event，waker 已在 step 2 注册）
7. `len == 1 → Poll::Ready((subscribers[0], event))`
8. `len >= 2 → 第一个立即 Ready，剩下 push 到 pending`

加 WAKER INVARIANT 注释：返回 Pending 的前提是 `poll_next_unpin(cx)` 已经把 `cx.waker()` 注册给 backend；任何绕过 `poll_next_unpin` 直接改 backend 的 mutating entry point（macOS `notify_tx`、Windows `PostThreadMessage`）必须自己负责 wake。

**4 个新单测**（全绿）：

- `geometry::tests::barrier_key_default_is_full_edge_no_monitor` — Default 等于 "全边 + 无 monitor"
- `geometry::tests::barrier_key_from_pos_matches_legacy_defaults` — `from_pos` 对 4 个 Position 行为一致
- `geometry::tests::barrier_key_eq_and_hash` — HashMap 主键可用（Eq + Hash + 含 `Some(String)` 字段）
- `poll_next_tests::empty_collection_returns_pending_and_keeps_waker` — 空订阅 → Pending（waker 已在 step 2 注册）
- `poll_next_tests::repeated_empty_collection_polls_keep_returning_pending` — 重复 poll 持续 Pending（waker 路径不短路）
- `poll_next_tests::fanout_for_same_key_delivers_one_per_subscriber` — 3 订阅 → 3 次 Ready 按订阅顺序
- `poll_next_tests::tokio_test_assert_pending_coverage` — 显式 `tokio_test::assert_pending!!` 覆盖 PLAN §M1 完成标志

### 1.5 Dev-dep：`tokio_test = "0.4"`（`input-capture/Cargo.toml`）

为 `tokio_test::assert_pending!` 加 dev-dep；同时 dev-dep `tokio` 显式开 `rt + macros + time + sync`。

## 2. 验证结果

### 2.1 `cargo build -p input-capture`

```
Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.17s
```

无 error / 无 warning。

### 2.2 `cargo test -p input-capture`

```
test result: ok. 27 passed; 0 failed; 0 ignored
```

新增 7 个测试（含 4 个 poll_next waker 测试 + 3 个 BarrierKey 单测），旧 20 个 geometry 测试全部保留。

### 2.3 `cargo fmt --check -p input-capture`

无 diff（已 `cargo fmt` 后）。

### 2.4 `cargo clippy -p input-capture --all-targets -- -D warnings`

```
Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.72s
```

0 warning / 0 error。

### 2.5 `cargo build --workspace`（预期部分失败）

按 STEP-1.1 完成标志 "编译错误暴露所有调用点"，workspace 故意不应编过：

```
error[E0308]: mismatched types
   --> src/capture.rs:557:44
   --> input-capture/src/lib.rs:148:18    (create)
...
error[E0308]: mismatched types
   --> src/capture_test.rs:17:33
   --> input-capture/src/lib.rs:148:18
```

**13 处错误全部在 `src/capture.rs` 与 `src/capture_test.rs`**（恰好是 STEP-1.3 要改的文件）。**`src/service.rs` 0 处错误**（service 经 `CaptureTask` 中转，未直接调本次变更的 API）。

## 3. 与 PLAN 的偏差

### 3.1 PLAN 把 5 backend 同步签名放在 STEP-1.2，但 STEP-1.1 提前做了 sig-only 兼容层

**PLAN §M1 STEP-1.1 "涉及文件" 只列 `geometry.rs`、`lib.rs`**；STEP-1.2 列 backend。但 `Capture` trait 是 crate 内 trait，sig 改了 backend 不动就编不过 `input-capture` crate（违反 STEP-1.1 完成标志中的"公共 API 签名变更 + crate 编过"组合）。

**STEP-1.1 的实际做法**：每个 backend 用 1-3 行最小改动满足新 trait：
- trait 方法签名：`(Position) → (&BarrierKey)`，方法体只读 `key.pos`
- Stream::Item：`(Position, _) → (BarrierKey, _)`，事件产生处用 `BarrierKey::from_pos(pos)` / `BarrierKey::default()` lift
- 内部 channel / pending_events 类型**保持 `Position`** 不动（layer_shell 例外，已改为 `VecDeque<(BarrierKey, _)>` 因为 push 站点多、lift 不优雅）

STEP-1.2 应按 PLAN 继续做 backend 内部 `Position → BarrierKey` 完整迁移（monitor / offset / span 注入、producer-event 通道 widen 等）。

### 3.2 `Stream::Item = (Position, CaptureEvent)` → `(BarrierKey, CaptureEvent)` 的精确边界

对内部仍以 `Position` 走 channel 的 backend（libei / macos / windows），选择**在 `Stream::poll_next` 边界 lift**，而不是把内部 channel 整体改为 `(BarrierKey, _)`。理由：
- libei `event_rx: Receiver<(Position, CaptureEvent)>` 由 capture_task（独立 `tokio::spawn`）生产，widening 涉及跨任务类型 + capture_task 内部处理逻辑
- macOS `ProducerEvent` 通道同理，且 ProducerEvent::Create/Destroy/StartCapture/CancelPending 内部全是 `Position`
- windows `event_thread` 是独立 OS 线程，PostThreadMessageW 路径 + NotifyType 枚举都按 `Position` 设计

边界 lift 是最小爆炸半径方案。STEP-1.2 真做完整迁移时再统一 widen。

## 4. 处理的 SUGGESTION 项

无（无 prior SUGGESTION 条目可处理）。

## 5. 闸门检查（时间门 / milestone 边界门）

- **时间门**：~30 min ✅（未超时）
- **milestone 边界门**：✅（不触碰 M2 显示器枚举 / M3 绑定 UI / M4 画布。`BarrierKey.monitor / offset / span` 全程 `None / 0 / 10000` 默认值；backend 只读 `pos`）

## 6. 遗留

### 6.1 留给 STEP-1.2

- 5 backend 内部 `Position → BarrierKey` 完整迁移（monitor / offset / span 注入 producer-event 通道）
- dummy backend：从"固定 `Position::Left`"改为接收外部注入的 `BarrierKey` 列表（PLAN §M1 STEP-1.2 "dummy 改为产出对应 BarrierKey"）
- `id_map.get(&id)` 现在返回 `Option<BarrierKey>`（Clone），destroy 路径多了一次克隆——可接受；若想优化可改成 `HashMap<CaptureHandle, BarrierKey>` 的 entry API

### 6.2 留给 STEP-1.3

`src/capture.rs`（13 处错）+ `src/capture_test.rs`（5 处错）：`CaptureTask` 持 `Vec<(CaptureHandle, BarrierKey, CaptureType)>`、`State::Pending { handle, key }`、`activate_client` 用 `client_manager.get_key(handle)` 取 key、`client_at(key)` 替换 `client_at(pos)`、`update_pos` 重算 key 后 deactivate/recreate。

### 6.3 已知小问题（SUGGESTION.md #1）

backend sig-only 兼容层的最小爆炸半径 vs. STEP-1.2 完整迁移的清晰度 trade-off——文档化即可。

## 7. 下一步

按 PLAN §M1 依赖顺序：**STEP-1.2**（5 backend 完整迁移到 `BarrierKey`，内部存储真实 `pos / monitor / offset / span`）。
