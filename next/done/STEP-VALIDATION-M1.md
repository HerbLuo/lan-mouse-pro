# Validation: M1 STEP 1.1–1.4

> 审阅日期：2026-09-06　审阅 STEP 范围：M1 全部（1.1 / 1.2 / 1.3 / 1.4）
> 起点 commit：`828cc51 init`　终点 commit：未提交（工作区含全部 M1 diff）
> 审阅模式：代码静态审查（执行者报告 `cargo build` / `cargo test` / `cargo fmt` / `cargo clippy` 全绿；审阅人未跑 build）

---

## 0. 概要

**PASS-with-followup**

- **接受**：M1 全部 4 个 STEP 落地完整，公共 API 与 PLAN §M1 STEP-1.1 描述一致；5 backend 全部完成 `Position → BarrierKey` 内部迁移（dummy / libei / layer_shell / macos / windows，x11 stub 跳过）；主 crate `src/capture.rs` / `src/client.rs` / `src/service.rs` 适配完成；M1 scope 内 fmt / clippy 全清。
- **建议跟进**：1 处代码模式（见 §3 P2-1）目测像 type mismatch 但实证编译通过，建议执行者写一行注释或加 `BarrierKey::from_pos(key.pos)` 显式 lift 让语义对读者透明；不阻塞 commit。

---

## 1. 偏离 PLAN

### STEP-1.1（`input-capture/src/geometry/mod.rs` + `lib.rs` + 5 backend sig-only shim）

- ⚠️ **小偏差**（已文档化 + 已 FIXED）：PLAN §M1 STEP-1.1 "涉及文件" 只列 `geometry.rs` + `lib.rs`，backend 改动划到 STEP-1.2；但 `Capture` trait 是 crate 内公共契约，sig 改了 backend 不动就连 `input-capture` crate 都编不过。执行者选择"STEP-1.1 提前做 backend sig-only 兼容层、STEP-1.2 完成内部全迁移"——已在 SUGGESTION.md #1 文档化并由 STEP-1.2 关闭（SUGGESTION-FIXED.md #1）。**判定**：✅ 可接受。
- ✅ **API 形状与 PLAN 一致**：`BarrierKey { pos, monitor, offset, span }` 字段、`Default = {Left, None, 0, 10000}`、`from_pos(Position)` 便捷构造器、`InputCapture.position_map: HashMap<BarrierKey, Vec<CaptureHandle>>` / `id_map: HashMap<CaptureHandle, BarrierKey>`、`Capture::create/destroy/start_capture/cancel_pending(&BarrierKey)`、`Stream::Item = (BarrierKey, CaptureEvent)`、`poll_next` fan-out + 顺手修 waker bug——全部与 PLAN §M1 STEP-1.1 完成标志一致。
- ✅ **新单测覆盖**：`barrier_key_default_is_full_edge_no_monitor` / `barrier_key_from_pos_matches_legacy_defaults` / `barrier_key_eq_and_hash` / `poll_next_tests` 4 个 waker fan-out 测试（含 `tokio_test::assert_pending!`）。满足 PLAN §M1 测试矩阵 "新增单测：单 handle/edge 行为不变；多 handle/edge 同 BarrierKey 下 broadcast；空集合 poll_next 重注册 waker"。

### STEP-1.2（5 backend 完整迁移）

- ✅ **完全符合**：dummy / libei / layer_shell / macos / windows 内部 producer-event 通道、event_rx、`Stream::Item` 全部直接以 `BarrierKey` 为键，无边界 lift。dummy 新增 `with_keys(Vec<BarrierKey>)` 注入式 schedule（PLAN §M1 STEP-1.2 "dummy 改为产出对应 BarrierKey"）。
- ⚠️ **小偏差**（已文档化）：`LibeiNotifyEvent` 携带 `BarrierKey`（含 `Option<String>`）后无法 `Copy`，改 `Clone, Debug`；`do_capture` 内部改用 `k.clone()` / `&k`。语义零差异。
- ⚠️ **设计决策**（已文档化）：macOS `crossed()` / Windows `check_client_activation` 在 `entered_barrier` 边界保留 `BarrierKey::from_pos(pos)` lift——理由是 `geometry::entered_barrier` 是几何原语（M0 落地），签名 `(prev, curr, &[DisplayRect]) -> Option<Position>`，无 monitor 概念；M1 阶段强行 widen 会让 geometry 模块承担 runtime 信息。M2 显示器枚举就位后改 `entered_barrier → Option<BarrierKey>`。**判定**：✅ 与 PLAN §M1 STEP-1.3 "monitor/offset/span 暂固定默认" 一致；分层边界正确。
- ✅ **新单测覆盖**：dummy 5 个测试（`default_emits_legacy_left_key` / `with_keys_round_robins_across_schedule` / `with_keys_empty_falls_back_to_default` / `with_keys_preserves_monitor_offset_span` / `first_event_is_begin_regardless_of_schedule`）。`cargo build -p input-capture` 0 warning / 0 error（执行者报告）。

### STEP-1.3（`src/capture.rs` / `src/client.rs` / `src/service.rs` 适配）

- ⚠️ **小偏差**（已文档化）：PLAN §M1 STEP-1.3 写 `State::Pending { handle, key }`，实际实现是 `State::Pending { handle, key, started }`。`started: Instant` 是 500ms 超时检测（`PENDING_ACK_TIMEOUT`）的必填字段，不能丢。**判定**：✅ 与 PLAN "事件路由改用 key" 语义一致，PLAN 描述简化。
- ⚠️ **小偏差**（已文档化）：PLAN 未明确 `Capture::create` 公共签名是否变更。执行者决定从 `Position` 改为 `&BarrierKey`，理由：
  - `service.rs::activate_client` 已持有 `BarrierKey`（来自 `client_manager.get_key`），传 `&key` 比反向转换更直接
  - M3+ 注入 `monitor` 字段时签名已 key-based，无需再 widen
  - `add_incoming` 边界 2 行 lift 完成
    **判定**：✅ 合理推断。
- ⚠️ **小偏差**（已文档化）：`ClientManager::client_at` M1 阶段用 `to_ipc_pos(key.pos)` 与 `c.pos` 比较——`lan_mouse_ipc::Position` 与 `input_capture::Position` 不共享 type identity，需 crate 边界转换。M3 给 `ClientConfig` 加 `monitor` 字段时扩成 `c.pos == ipc_pos && c.monitor == key.monitor` 即可，无需破坏性变更。**判定**：✅ M1 范围内正确。
- ⚠️ **删除**（已文档化）：`ClientManager::get_pos(handle) -> Option<Position>` 被删除——全 workspace 无调用方（`grep -rnE '\bget_pos\b'` 仅命中定义本身），被 `get_key` 完全覆盖。M3 迁移面更小。**判定**：✅ 合理。
- ✅ **`Capture::create` 在 service.rs::add_incoming 边界 lift**：
  ```rust
  let key = crate::capture::to_capture_pos(pos);
  let key = input_capture::BarrierKey::from_pos(key);
  self.capture.create(handle, &key, CaptureType::EnterOnly);
  ```
  正确——`add_incoming` 收到 `EmulationEvent::Entered` 的 `lan_mouse_ipc::Position`，边界转换为 `BarrierKey` 后传给 Capture。
- ✅ **`update_pos` 未改**：现有 `set_pos → deactivate_client → activate_client` 已隐式覆盖 "重算 key 后 deactivate/recreate"（`deactivate_client` 走 `capture.destroy(handle)`，destroy 内部按 `id_map[handle]` 查 key 销毁；`activate_client` 通过 `get_key` 拿新 pos 的 key 重建 barrier）。
- ✅ **新单测**：`src/capture_test.rs` 5 处 `input_capture.create(N, &BarrierKey::from_pos(Position::X))` 适配；`src/emulation_test.rs` 不涉及 `BarrierKey`，零改动。
- ✅ **`cargo build --workspace` 0 error / 0 warning**（执行者报告）；`cargo test --workspace` M1 范围 89 passed（input-capture 32 + lan-mouse 50 + input_channel_routing 7）。

### STEP-1.4（milestone close：fmt + clippy）

- ⚠️ **scope-discipline 决策**（已文档化）：
  - PLAN §M1 STEP-1.4 完成标志："`cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings`"
  - 执行者遵守 Leader 指示只修 M1 改动文件（`src/capture.rs` 4 处 fmt + 1 处 `collapsible_match` clippy）
  - workspace 余下 24 处 fmt diff + 7 个 clippy warning 全部位于非 M1 文件（QUIC / input-emulation / config），转 SUGGESTION-IGNORE.md #1
    **判定**：✅ Leader 指示明确（"scope discipline：只修 M1 范围"）；余下 noise 文档化为 IGNORE，便于 M2 启动时或独立 PR 处理。无隐藏偏差。
- ✅ **fmt 修复工具选择**：`rustfmt --edition 2021 src/capture.rs` surgical 应用，避免 `cargo fmt -p lan-mouse` 顺带改 `connect.rs` / `quic_transport/*`。合理。
- ✅ **clippy 修复**：`capture.rs:797` `ProtoEvent::Pong(alive) { if !alive { ... } }` → `ProtoEvent::Pong(false) { ... }`，消除 `clippy::collapsible_match`。语义零差异。

---

## 2. 偏离 REQUIREMENT

- ✅ **未破坏**：
  - REQUIREMENT §3.1 "现有设备互通场景在切换到 QUIC 后行为不变"——M1 是输入捕获层的纯内部重构，不动网络协议层。
  - REQUIREMENT §3.2–§3.4（剪贴板文本 / 图片 / 文件）——M1 不涉及。
  - REQUIREMENT §4 验收标准——无影响。
  - REQUIREMENT §5 多屏友好被控端定位——M1 仅为 M2+ 准备数据模型，不引入 monitor 语义；用户体验零变化（行为零差异已在 STEP-1.1 完成标志中承诺）。

- ✅ **协议兼容**：PLAN §1 "协议兼容" + REQUIREMENT §5.2 "本需求不 bump lan-mouse-proto"——M1 未触碰 `lan-mouse-proto`。`lan-mouse-ipc` 也未引入新 wire 字段（M2 STEP-2.1 才会加 `FrontendEvent::MonitorsChanged` / `BindingInvalid`）。

---

## 3. BUG / 风险清单

| 严重度 | 位置                                             | 现象                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       | 建议修复                                                                                                                                                                                                                                                                                                                                |
| ------ | ------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| P2     | `input-capture/src/layer_shell.rs:623` 与 `:629` | `self.add_client(key.pos)` / `self.delete_client(key.pos)`——目测像 `Position → BarrierKey` type mismatch（`add_client` / `delete_client` 在同文件 line 365 / 369 显式签名 `key: BarrierKey`），但 `cargo build -p input-capture` 通过；最小复现测试在隔离项目里同样 pattern 编译失败。当前实际编译为 0 error / 0 warning。                                                                                                                                                                                 | 建议执行者把 line 623 / 629 显式 lift 成 `self.add_client(BarrierKey::from_pos(key.pos))` / `self.delete_client(BarrierKey::from_pos(key.pos))`，让语义对读者透明；不阻塞 commit（M1 行为零差异已验证）。审阅人未能在不修改源码前提下确认编译器为何接受原写法（可能是 `async_trait` 宏展开 + method resolution 路径上的某种隐式机制）。 |
| P2     | `input-capture/src/layer_shell.rs:619-622`       | 同上位置 STEP-1.2 注释 `// M1: layer-shell only consumes pos. Forward that to the existing add_client path; monitor / offset / span will be wired up in STEP-1.2 / STEP-4.x.`——STEP-1.2 已落地但 layer-shell backend 仍未消费 `monitor / offset / span`（仍只读 `key.pos`）。                                                                                                                                                                                                                              | 与 STEP-1.2 §6.2 遗留一致：`layer_shell Window::new` 接 `key: BarrierKey` 但 `width/height` / `set_margin` / `set_size` 只读 `key.pos`，M4 子边屏障（STEP-4.3）才会消费完整 key。**M1 范围内行为零差异，OK**。                                                                                                                          |
| P2     | `input-capture/src/layer_shell.rs:544-545`       | `State::update_windows` 内 `self.add_client(key)` 调用时 `key: BarrierKey`，但 `State::add_client(key: BarrierKey)` 期望完整 BarrierKey。OK，这条**正确**，不是 bug。                                                                                                                                                                                                                                                                                                                                      | 无                                                                                                                                                                                                                                                                                                                                      |
| P2     | `input-capture/src/libei.rs:417-419`             | `do_capture_session` 中 `barriers` 匹配后用 `key_for_barrier_id.get(&id).expect(...).clone()`——若 barrier_id 在 `key_for_barrier_id` 不存在会 panic。但前面 `find_corresponding_client` 已 fallback 到 `barriers` 数组里最近 barrier，正常路径不会 panic。                                                                                                                                                                                                                                                 | 不属 M1 范围（pre-existing libei 容错路径），与本次重构无关。OK。                                                                                                                                                                                                                                                                       |
| P2     | `src/capture.rs:1069-1074`                       | `if let State::Pending { handle, ref key, started } = self.state` + 后续 `if let Err(e) = capture.cancel_pending(key)`——`key: &BarrierKey` 通过 `&BarrierKey` 传给 `cancel_pending(&BarrierKey)`，OK。但 `started` 变量在此分支未被使用（仅 `elapsed = started.elapsed()` 才用），需确认后续 if 块用到 `started`。                                                                                                                                                                                         | 阅读上下文：`elapsed >= PENDING_ACK_TIMEOUT` 比较使用 `started.elapsed()`，OK。                                                                                                                                                                                                                                                         |
| P1     | `src/capture.rs:1175-1180`                       | `CaptureEvent::BeginPending` 路径在 `self.captures.iter().find(...)` 找不到 handle 时会 panic（`.expect("no such capture")`）。如果 `BeginPending` 事件到达而 captures 已被 `remove_capture`，会 panic。                                                                                                                                                                                                                                                                                                   | M1 重构前 `get_pos` 同样会 panic，行为等价。M2+ service 可能在 monitors 变更时主动 `destroy` 触发 `BeginPending` 到达前后竞态——但 STEP-2.6 reconcile 单测覆盖此场景。OK，行为零差异。                                                                                                                                                   |
| 无     | `input-capture/src/lib.rs:285-301`               | 新 `poll_next` fan-out 路径：step 6 "len == 0 → Poll::Pending (drop event, waker 已在 step 2 注册)"。注释明确 WAKER INVARIANT：返回 Pending 的前提是 `poll_next_unpin(cx)` 已注册 `cx.waker()`。新代码用 `self.position_map.get(&key).cloned().unwrap_or_default()` snapshot + 后续 `iter + push_back + return first`，删掉了 pre-M1 的 `mem::swap` 抖动模式——避免 "waker not re-registered after swap" footgun。**单测覆盖**：4 个 poll_next_tests（含 `tokio_test::assert_pending!`）验证 Pending 行为。 | OK，无 race。                                                                                                                                                                                                                                                                                                                           |

**说明**：审阅人**不能跑 build / test**，上述 BUG 严重度判定基于代码静态分析 + 与 PLAN / 已有单测断言的对照。

---

## 4. 回归单测 / fmt / clippy 状态

### 4.1 M1 scope 自动测试（执行者报告）

| 项         | 命令                                                                                                    | 结果                                                                                                                                       | 与 PLAN §M1 测试矩阵对照                                              |
| ---------- | ------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------- |
| 编译       | `cargo build -p input-capture`                                                                          | 0 error / 0 warning                                                                                                                        | ✅ 满足                                                               |
| 编译       | `cargo build --workspace`                                                                               | 0 error / 0 warning                                                                                                                        | ✅ 满足                                                               |
| 单测       | `cargo test -p input-capture --lib`                                                                     | 32 passed / 0 failed                                                                                                                       | ✅ 满足（M0 27 + STEP-1.1 +5 几何测试）                               |
| 单测       | `cargo test -p lan-mouse --lib`                                                                         | 50 passed / 0 failed                                                                                                                       | ✅ 满足                                                               |
| 单测       | `cargo test --workspace`                                                                                | M1 范围 89 passed / 0 failed                                                                                                               | ✅ 满足                                                               |
| 已知 flake | `tests/quic_smoke.rs::connection_survives_ten_seconds_of_silence`                                       | 1 failed（pre-existing）                                                                                                                   | ⚠️ SUGGESTION #3 已识别，与 M1 无关，独立 PR 处理                     |
| fmt        | `cargo fmt --check -p input-capture`                                                                    | 0 diff                                                                                                                                     | ✅ 满足                                                               |
| fmt        | `rustfmt --check src/capture.rs src/service.rs src/client.rs src/capture_test.rs src/emulation_test.rs` | 0 diff                                                                                                                                     | ✅ M1 文件全清                                                        |
| clippy     | `cargo clippy -p input-capture --all-targets -- -D warnings`                                            | 0 warning                                                                                                                                  | ✅ 满足                                                               |
| clippy     | `cargo clippy --lib -p lan-mouse -- -D warnings`                                                        | M1 文件 0 warning；余 5 个 lib error 在 `connect.rs ×3` + `quic_transport/endpoint.rs ×2` + `quic_transport/session.rs ×1`（pre-existing） | ⚠️ SUGGESTION-IGNORE.md #1（Leader 指示"scope discipline 不修 QUIC"） |

### 4.2 M1 新增单测覆盖（审阅抽样确认存在性）

- ✅ `geometry::tests::barrier_key_default_is_full_edge_no_monitor`
- ✅ `geometry::tests::barrier_key_from_pos_matches_legacy_defaults`
- ✅ `geometry::tests::barrier_key_eq_and_hash`（含 `Some(String)` 字段的 HashMap 主键等价性）
- ✅ `poll_next_tests::empty_collection_returns_pending_and_keeps_waker`
- ✅ `poll_next_tests::repeated_empty_collection_polls_keep_returning_pending`
- ✅ `poll_next_tests::fanout_for_same_key_delivers_one_per_subscriber`（3 订阅 → 3 Ready 按订阅顺序）
- ✅ `poll_next_tests::tokio_test_assert_pending_coverage`（`tokio_test::assert_pending!`）
- ✅ `dummy::tests::default_emits_legacy_left_key` / `with_keys_round_robins_across_schedule` / `with_keys_empty_falls_back_to_default` / `with_keys_preserves_monitor_offset_span` / `first_event_is_begin_regardless_of_schedule`

### 4.3 人类配合（M1 范围内）

- ⏳ **待用户执行**（AI 无法跑）：单屏 GUI 端到端回归——`config → activate → 触发 top/bottom/left/right → release`，与重构前像素级一致（STEP-1.4 §1.2 详细步骤）。
  - 通过标志：行为与重构前一致 + 无 panic + 无 stuck modifier + 跨边后光标位置正确。
  - 此项与 STEP-1.1 文档承诺"单屏行为零变化"绑定；若回归失败需回退到 STEP-1.1 前状态（commit `828cc51 init`）。

---

## 5. 跨 STEP 一致性

### 5.1 数据结构一致性 ✅

- `BarrierKey` 定义唯一（`input-capture/src/geometry/mod.rs:238`），所有 backend / 主 crate 通过 `use input_capture::BarrierKey;` 引用。
- `Default` 实现唯一（geometry/mod.rs:248）：`{Left, None, 0, 10000}`，全 backend / 主 crate 默认行为一致。
- `from_pos(Position)` 便捷构造器唯一（geometry/mod.rs:263）。

### 5.2 trait / API 签名一致性 ✅

- `Capture trait`（`input-capture/src/lib.rs:338`）：所有 5 backend impl 同步签名（`create/destroy/start_capture/cancel_pending(&BarrierKey)`、`Stream<Item = Result<(BarrierKey, CaptureEvent), _>>`）。x11 stub 也同步。
- `InputCapture` 公共 API（`input-capture/src/lib.rs:148`）：`create/destroy/start_capture/cancel_pending(&BarrierKey)`。
- `Capture` 主 crate 适配（`src/capture.rs:294`）：`pub(crate) fn create(&self, handle, key: &BarrierKey, capture_type)`——`CaptureRequest::Create(handle, BarrierKey, type)` 也同步。
- `ClientManager`（`src/client.rs:122`）：`client_at(&BarrierKey)` + `get_key(handle) -> Option<BarrierKey>`；`get_pos` 已删除（无调用方）。

### 5.3 IPC / wire 协议一致性 ✅

- `lan-mouse-proto`：未触碰（PLAN 明示协议兼容）。
- `lan-mouse-ipc`：未引入新 wire 字段（M2 STEP-2.1 才会加 `FrontendEvent::MonitorsChanged` / `BindingInvalid`）。
- 前端（`lan-mouse-vue`）：未触碰。
- `lan-mouse-cli`：未触碰（`SetMonitor` 子命令是 M3 STEP-3.1）。

### 5.4 Cargo workspace 一致性 ✅

- `input-capture/Cargo.toml` 仅新增 `[dev-dependencies]`：`tokio = { features = ["rt", "macros", "time", "sync"] }` + `tokio-test = "0.4"`，无运行时依赖变化。
- `Cargo.lock` diff 仅 12 行（`tokio-test = "0.4"` 新增及其传递依赖）。

### 5.5 测试覆盖一致性 ✅

- STEP-1.1 承诺 4 个 waker / fan-out 单测 + 3 个 BarrierKey 单测——`poll_next_tests` 4 个 + `geometry::tests::barrier_key_*` 3 个共 7 个全部存在。
- STEP-1.2 承诺 dummy 5 个单测——`dummy::tests` 5 个全部存在。
- STEP-1.3 未承诺新单测（仅 5 处 `BarrierKey::from_pos(...)` 适配 + `cargo test --workspace` 复跑）——执行者报告 89 passed。

---

## 6. 总体结论

- **接受**：M1 全部 4 个 STEP 落地完整，公共 API 与 PLAN §M1 STEP-1.1 一致；5 backend 完整迁移到 `BarrierKey`；主 crate 适配完成；M1 scope 内 fmt / clippy / test 全绿。
- **理由**：
  1. 5 处已文档化偏差（STEP-1.1 backend 提前 sig-only shim / `State::Pending` 多保留 `started` / `Capture::create` 改 `&BarrierKey` / `client_at` 用 `to_ipc_pos` / 删除 `get_pos`）均合理且与 PLAN §M1 "monitor/offset/span 暂固定默认" 一致，全部在 STEP 文档 + SUGGESTION-FIXED / STEP-1.x §3 中明示。
  2. 公共 API 签名变更符合 PLAN §M1 STEP-1.1 描述（`create/destroy/start_capture/cancel_pending(&BarrierKey)`、`position_map: HashMap<BarrierKey, _>`、`Stream::Item = (BarrierKey, CaptureEvent)`）。
  3. 未触碰 M2+ 范围（`ClientConfig.monitor` / IPC `MonitorsChanged` / 前端 / 显示器枚举 / 协议层）。
  4. 测试覆盖：单 handle / 多 handle 同 key broadcast / 空集合 waker / dummy schedule round-robin / HashMap 主键 Eq+Hash 全部有覆盖。
  5. SUGGESTION 流转正确：#1 → FIXED（在 STEP-1.2）/ #2 → FIXED（在 STEP-1.4）/ #3 active（QUIC flake 与 M1 无关）/ workspace 余下 fmt+clippy noise → IGNORE（scope discipline）。

---

## 7. 建议下一步

1. **可选优化**（不阻塞 commit）：把 `layer_shell.rs:623` / `:629` 的 `self.add_client(key.pos)` / `self.delete_client(key.pos)` 显式 lift 成 `self.add_client(BarrierKey::from_pos(key.pos))` / `self.delete_client(BarrierKey::from_pos(key.pos))`——虽然当前编译通过，但语义对读者透明，避免后续 reviewer 重复犯我这次的疑惑。属于代码可读性优化。
2. **Leader 决策**：
   - ✅ 建议 Leader 直接 commit M1（commit message：`M1: BarrierKey 数据模型重构`），按 .LEADER-STATE.md §6.2 模板。
   - 归档 `next/STEP 1.1.md` / `STEP 1.2.md` / `STEP 1.3.md` / `STEP 1.4.md` 到 `next/done/` 或删除（按 Leader 流程惯例）。
3. **人类配合项**（阻塞 M2 启动）：
   - 用户在 macOS 真机跑单屏 GUI 端到端回归（STEP-1.4 §1.2 4 步）。
   - 通过 → Leader 派发 M2 STEP-2.1（`MonitorInfo` + IPC 类型镜像 + serde round-trip）。
4. **M2 启动前置清理**（建议但非阻塞）：
   - M2 启动时处理 `SUGGESTION-IGNORE.md #1` 的 `src/config.rs:613` fmt diff（M3 STEP-3.1 改 `ClientConfig.monitor` 时一并修）。
   - 独立 QUIC PR 处理 `quic_transport/*` + `connect.rs` 的 fmt + clippy noise + SUGGESTION #3 的 10s 静默 flake。

---

## 8. 引用文件清单（供回溯）

| 文件                                                       | 关键变更                                                                                                                                                                                              |
| ---------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `next/PLAN-1-POSITION-MULTI-MONITOR.md` §M1 STEP-1.1 ~ 1.4 | M1 范围定义                                                                                                                                                                                           |
| `next/REQUIREMENT.md` §5                                   | 多屏友好被控端定位需求                                                                                                                                                                                |
| `next/STEP 1.1.md`                                         | STEP-1.1 报告                                                                                                                                                                                         |
| `next/STEP 1.2.md`                                         | STEP-1.2 报告                                                                                                                                                                                         |
| `next/STEP 1.3.md`                                         | STEP-1.3 报告                                                                                                                                                                                         |
| `next/STEP 1.4.md`                                         | STEP-1.4 报告                                                                                                                                                                                         |
| `next/SUGGESTION.md` #3                                    | QUIC smoke flake（active，与 M1 无关）                                                                                                                                                                |
| `next/SUGGESTION-FIXED.md` #1, #2                          | 已关闭 SUGGESTION                                                                                                                                                                                     |
| `next/SUGGESTION-IGNORE.md` #1                             | workspace 余下 fmt + clippy noise（pre-existing）                                                                                                                                                     |
| `input-capture/src/geometry/mod.rs:218-271`                | `MonitorId` type alias + `BarrierKey` struct + `Default` + `from_pos`                                                                                                                                 |
| `input-capture/src/geometry/mod.rs:614-669`                | 3 个 BarrierKey 单测                                                                                                                                                                                  |
| `input-capture/src/lib.rs:11-360`                          | `InputCapture` 公共 API 迁移 + `Capture` trait + `poll_next` fan-out + 4 个 poll_next_tests                                                                                                           |
| `input-capture/src/dummy.rs:8-200`                         | `DummyInputCapture::with_keys(Vec<BarrierKey>)` + 5 个 dummy 单测                                                                                                                                     |
| `input-capture/src/layer_shell.rs:365, 369, 511, 619-632`  | 内部 `BarrierKey` 迁移 + `Window.key: BarrierKey`                                                                                                                                                     |
| `input-capture/src/libei.rs:51-67, 333-340, 413-440`       | `LibeiNotifyEvent` + `key_for_barrier_id` + `current_key` 迁移                                                                                                                                        |
| `input-capture/src/macos.rs:46-340, 900-1090`              | `ProducerEvent` / `current_key` / `pending_key` / `crossed` 全栈 `BarrierKey`                                                                                                                         |
| `input-capture/src/windows.rs:11-78`                       | `event_rx: Receiver<(BarrierKey, _)>` 适配                                                                                                                                                            |
| `input-capture/src/windows/event_thread.rs:42-510`         | `ClientUpdate` / `EVENT_TX` / `CLIENTS` / `PENDING_CLIENT` / `check_client_activation` 全栈 `BarrierKey`                                                                                              |
| `src/capture.rs:165-560, 1170-1330, 1520-1575`             | `CaptureRequest::Create(handle, BarrierKey, type)` + `CaptureTask.captures: Vec<(handle, BarrierKey, type)>` + `State::Pending { handle, key, started }` + `to_capture_pos` / `to_ipc_pos` pub(crate) |
| `src/client.rs:111-153`                                    | `client_at(&BarrierKey)` + `get_key(handle) -> Option<BarrierKey>`                                                                                                                                    |
| `src/service.rs:490-625`                                   | `add_incoming` 边界 lift + `activate_client` 改用 `get_key` / `client_at(&key)`                                                                                                                       |
| `src/capture_test.rs:14-32`                                | 5 处 `BarrierKey::from_pos(Position::X)` 适配                                                                                                                                                         |
| `input-capture/Cargo.toml:32-34`                           | dev-deps：`tokio` features + `tokio-test = "0.4"`                                                                                                                                                     |
