# STEP 1.3 — `src/capture.rs` / `service.rs` / `client.rs` 适配 BarrierKey

> PLAN §M1 / STEP-1.3
> 执行日期：2026-09-06　实际耗时：~45 min（略超 PLAN 估时 30 min，未触发拆步阈值 1h）
> 结论：通过

## 1. 做了什么

把 STEP-1.1 + STEP-1.2 落地的 `BarrierKey` 贯穿到主 crate（`lan-mouse`）的 capture task / service / client 三层。`monitor / offset / span` 全程 `None / 0 / 10000` 默认值，行为零差异。

### 1.1 `src/capture.rs`

**核心数据结构迁移**：

| 旧 | 新 |
|---|---|
| `CaptureTask::captures: Vec<(CaptureHandle, Position, CaptureType)>` | `Vec<(CaptureHandle, BarrierKey, CaptureType)>` |
| `CaptureRequest::Create(handle, Position, type)` | `Create(handle, BarrierKey, type)` |
| `State::Pending { handle, started }` | `Pending { handle, key: BarrierKey, started }` |
| `CaptureTask::get_pos(handle) -> Position` | `get_key(handle) -> BarrierKey` |
| `CaptureTask::is_default_capture_at(Position)` | `is_default_capture_at(&BarrierKey)` |
| `CaptureTask::add_capture(handle, Position, type)` | `add_capture(handle, &BarrierKey, type)`（clone 内部） |
| `Capture::create(handle, lan_mouse_ipc::Position, type)` | `create(handle, &BarrierKey, type)`（内部 `.clone()`） |

`State::Pending` 新增 `key: BarrierKey` 字段（保留 `started: Instant`）。`BeginPending` 收到时从 `self.captures` 查 key clone 写入 state；后续 Ack / timeout / `release_capture` 路径直接读 `state.key`（PLAN §M1 STEP-1.3 "事件路由改用 key"），去掉原来 Ack 路径上每次 `self.get_pos(handle)` 的线性扫描。

**`Capture::create` public API 签名变更**：从 `Position` 改为 `&BarrierKey`。`service.rs::add_incoming` 在边界做 `BarrierKey::from_pos(to_capture_pos(pos))`，`activate_client` 直接传 `&key`（来自 `client_manager.get_key`）。

**helper 暴露 `pub(crate)`**：原 `to_capture_pos`（`lan_mouse_ipc::Position → input_capture::Position`）从私有改为 `pub(crate)`；新增 `to_ipc_pos`（反向）。`client.rs` 用 `to_ipc_pos` 做 `BarrierKey` ↔ `ClientConfig.pos` 的比较，`client.rs` 用 `to_capture_pos` 构造 `get_key` 返回值。

**`State::Pending` destructure 全部用 `ref key`**：`key` 非 Copy，`match self.state { State::Pending { ref key, .. } => ... }` 避免 `cannot move out of ...` 借用错。

**`to_capture_pos` 内部消费点简化**：`Capture::create` 现在直接拿 `&BarrierKey`，不再调 `to_capture_pos`。该 helper 仅保留给 `service.rs::add_incoming` 边界使用。

### 1.2 `src/client.rs`

**新 API**：

```rust
pub fn client_at(&self, key: &BarrierKey) -> Option<ClientHandle>  // 替换 client_at(Position)
pub(crate) fn get_key(&self, handle: ClientHandle) -> Option<BarrierKey>  // 新增
```

**删除**：`pub(crate) fn get_pos(handle) -> Option<Position>` —— 全 workspace 无调用方（`grep -rnE 'get_pos\('` 仅命中定义），由 `get_key` 取代。

**`client_at` 实现**（M1 兼容层）：内部 `to_ipc_pos(key.pos)` 后与 `c.pos` 比较。M3+ 给 `ClientConfig` 加 `monitor` 字段时，本函数扩成 `c.pos == ipc_pos && c.monitor == key.monitor` 等。

**`get_key` 实现**（M1 兼容层）：`BarrierKey::from_pos(to_capture_pos(c.pos))`，默认 `monitor=None / offset=0 / span=10000`。

### 1.3 `src/service.rs`

**`activate_client` 改造**：

```rust
let Some(key) = self.client_manager.get_key(handle) else { return; };
if let Some(other) = self.client_manager.client_at(&key) { ... }
if self.client_manager.activate_client(handle) {
    self.capture.create(handle, &key, CaptureType::Default);  // 改传 &key
    self.broadcast_client(handle);
    log::info!("activated client {handle} ({key:?})");         // 改打 key
    ...
}
```

**`add_incoming` 边界转换**（EmulationEvent 给的是 `lan_mouse_ipc::Position`）：

```rust
let key = crate::capture::to_capture_pos(pos);
let key = input_capture::BarrierKey::from_pos(key);
self.capture.create(handle, &key, CaptureType::EnterOnly);
```

**`update_pos` 未改**：现状 `set_pos → deactivate_client → activate_client` 已经隐式覆盖 "重算 key 后 deactivate/recreate"（PLAN §M1 STEP-1.3 要求）。`set_pos` 改 `c.pos`，`deactivate_client` 走 `capture.destroy(handle)`（input-capture 内部 `id_map` 按 handle 查 key 销毁），`activate_client` 通过 `get_key` 拿新 pos 的 key 重建 barrier。

### 1.4 `src/capture_test.rs` / `src/emulation_test.rs`

- `src/capture_test.rs`：5 处 `input_capture.create(N, Position::X)` → `input_capture.create(N, &BarrierKey::from_pos(Position::X))`。`emulation_test.rs` 不涉及 `BarrierKey`（用 `InputEmulation`），零改动。

## 2. 验证结果

### 2.1 `cargo build --workspace`

```
Finished `dev` profile [unoptimized + debuginfo] target(s) in 3.68s
```

0 error，0 warning。

### 2.2 `cargo test -p input-capture --lib`

```
test result: ok. 32 passed; 0 failed; 0 ignored
```

STEP-1.2 末尾 32 个测试（27 + 5 dummy）全部保留并绿。

### 2.3 `cargo test -p lan-mouse --lib`

```
test result: ok. 50 passed; 0 failed; 0 ignored
```

含 `watchdog_tests`（4 个，FIX 4 配置）+ `client_input_channels_tests`（2 个，新增字段） + `quic_transport::*` 系列 50 个。

### 2.4 `cargo test --workspace`

89 单元/集成测试全绿（input-capture 32 + lan-mouse 50 + input_channel_routing 7）。**唯一失败**：`tests/quic_smoke.rs::connection_survives_ten_seconds_of_silence` —— 已 `git stash` 验证为 **pre-existing flake**，与本 STEP 无关（不在 capture/service/client 三层）。

### 2.5 `cargo fmt --check -p lan-mouse`

本 STEP 修改文件（capture.rs / capture_test.rs / client.rs / service.rs）**0 diff**。其余文件的 fmt diff（macos.rs / config.rs / quic_transport/* / quic_smoke.rs / capture.rs 老行号 620/1036/1048/1398）均为 pre-existing，留给 STEP-1.4（milestone close）。

### 2.6 `cargo clippy -p lan-mouse --lib --all-targets`

本 STEP 修改文件**无新 warning**。`capture.rs:797` 的 `if !alive` collapsible-into-match 是 pre-existing（commit init 已存在），与本 STEP 无关。`connect.rs` 几个 `doc_lazy_continuation` / `assertions_on_constants` 也是 pre-existing。

## 3. 与 PLAN 的偏差

### 3.1 PLAN "State::Pending { handle, key }" 实为 `{ handle, key, started }`

PLAN 描述简化了 —— `started: Instant` 是 500ms 超时检测的必填字段（`PENDING_ACK_TIMEOUT` 计算依赖），**不能丢**。STEP 实现保留 `started` 并加 `key`，与 PLAN "事件路由改用 key" 语义一致。

### 3.2 `Capture::create` public API 从 `Position` 改为 `&BarrierKey`

PLAN §M1 STEP-1.3 未明确 public `Capture::create` 签名是否变更。本 STEP 决定**改**为 `&BarrierKey`，理由：
- service.rs `activate_client` 已持有 `BarrierKey`（来自 `get_key`），传 `&key` 比传 `key.pos.into()`（需要反向转换）更直接
- M3+ 当 `BarrierKey.monitor` 字段被注入时，签名已经是 key-based，无需再次 widen
- `add_incoming` 边界用 2 行 `to_capture_pos` + `BarrierKey::from_pos` 完成转换，影响半径可控

### 3.3 `client_at` 用 `to_ipc_pos(key.pos)` 比较（M1 等价旧 pos 比较）

`ClientConfig.pos: lan_mouse_ipc::Position`，`BarrierKey.pos: input_capture::Position`。两个 enum 不共享 type identity（分别由 `lan-mouse-ipc` / `input-capture` 定义），需 crate 间转换。M3 给 `ClientConfig` 加 `monitor` 字段时，`client_at` 扩成 `c.pos == ipc_pos && c.monitor == key.monitor` 即可，**无需破坏性变更**。

### 3.4 删除 `ClientManager::get_pos(handle) -> Option<Position>`

全 workspace 无调用方（`grep -rnE '\bget_pos\b'` 仅命中定义本身）。原 API 提供的"按 handle 查 pos"语义完全被 `get_key` 覆盖（`get_key(h).map(|k| to_ipc_pos(k.pos))`），保留只会增加 M3 的迁移面。

## 4. 处理的 SUGGESTION 项

无新增（无 prior SUGGESTION 条目可处理）。新增 SUGGESTION 2 条，见 `next/SUGGESTION.md`：

- **#2 — pre-existing fmt + clippy 噪音待 STEP-1.4 milestone close 统一处理**
- **#3 — pre-existing QUIC smoke test flake `connection_survives_ten_seconds_of_silence`**

## 5. 闸门检查（时间门 / milestone 边界门）

- **时间门**：~45 min ⚠️（PLAN 估时 30 min，超 50% 但 < 1h 阈值；超出原因：PLAN §3 STEP-1.3 描述里 `client_at(key)` 的"key"语义需要 crate 边界类型转换 `to_ipc_pos`，比预估多 ~1 轮 review-and-fix）
- **milestone 边界门**：✅（不触碰 M2 显示器枚举 / M3 绑定 UI / M4 画布）
  - `monitor / offset / span` 全程 `None / 0 / 10000` 默认值
  - 没引入 `ClientConfig.monitor` / `FrontendRequest::UpdateMonitor` / 前端改动
  - 没碰 `entered_barrier` / `crossed` 边界 lift（M2 范围，STEP-1.2 已记录边界）

## 6. 遗留

### 6.1 留给 STEP-1.4（milestone close）

- `cargo fmt --check` 全 workspace：pre-existing 9 处 diff 待统一处理
- `cargo clippy --workspace --all-targets -- -D warnings`：pre-existing 12 个 warning（`assertions_on_constants` / `doc_lazy_continuation` / `if !alive collapsible match` / 等）需带 `#[allow(...)]` 或重构

### 6.2 留给 M2+

- macOS `crossed()` / Windows `check_client_activation` 中 `entered_barrier` 边界 lift（`BarrierKey::from_pos(pos)`）—— STEP-1.2 §3.2 已记录，M2 显示器枚举就位后改 `entered_barrier → Option<BarrierKey>`。
- `Capture::create` 当前直接拿 `&BarrierKey`，但 `add_incoming` 仍传 `BarrierKey::from_pos(pos)`（默认 monitor=None）。M2 给 `incoming_conn_info` 注入 monitor 信息时，`add_incoming` 边界同样要传真实 key。

### 6.3 留给 M3

- `ClientManager::client_at` 需要扩成 `c.pos == ipc_pos && c.monitor == key.monitor && ...`
- `ClientManager::get_key` 需要把 `ClientConfig.monitor`（M3 新增字段）注入到返回的 `BarrierKey`
- `ClientConfig.monitor: Option<String>` 序列化字段 + 旧 config 向后兼容（缺字段 = `None`）

## 7. 下一步

按 PLAN §M1 依赖顺序：**STEP-1.4**（20m）：
- `cargo fmt --check` 全 workspace + 修 pre-existing diff
- `cargo clippy --workspace --all-targets -- -D warnings` + 处理 pre-existing warning
- 单屏 GUI 完整走一遍 config → activate → 触发 top/bottom/left/right → release（**人类配合**）

M1 milestone 完成后即可启动 M2（显示器枚举 + 热插拔）。
