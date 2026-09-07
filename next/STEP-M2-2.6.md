# STEP M2-2.6 — service 层 reconcile + WS 推送 + UI 高亮

> PLAN §M2 / STEP-2.6
> 执行日期：2026-09-07　实际耗时：~50 min（含 fmt + clippy 修整 + 8 单测 + 2 文件 Vue 改动 + 测试矩阵验证）
> 结论：✅ 通过

## 1. 做了什么

把"backend 显示器枚举"延伸到"前端可见"：service 订阅 backend 监控变更，转 IPC 事件，对 active client 做 reconcile，前端在 BindingInvalid 时高亮 row。

**改动文件**（5 个 + 2 个新建辅助函数）：

- `src/capture.rs`
  - 新增 `ICaptureEvent::MonitorsChanged(Vec<input_capture::MonitorInfo>)` 变体
  - CaptureTask 新增 `last_monitors: Vec<input_capture::MonitorInfo>` 字段（dedup 用）
  - 新增 `MONITOR_POLL_INTERVAL: Duration = Duration::from_secs(1)` 常量
  - `do_capture` 启动时通过 `capture.monitors()` 同步拿初始快照，emit `ICaptureEvent::MonitorsChanged`
  - `do_capture_session` 新增 `monitor_tick: tokio::time::interval(1s)`，在原 `select!` 块加一个 arm：poll → dedup → emit `MonitorsChanged`（与现有 pending_tick / watchdog ticks 并列）

- `src/service.rs`
  - 新增 `last_monitors: Option<Vec<GeometryMonitorInfo>>` 字段（`None` = 第一次观察，不触发 reconcile）
  - 新增 import：`input_capture::{BarrierKey, MonitorInfo as GeometryMonitorInfo}` + `lan_mouse_ipc::MonitorInfo as IpcMonitorInfo`
  - `handle_capture_event` 新增 `ICaptureEvent::MonitorsChanged(monitors)` arm：保存 `last_monitors` + 镜像转换 + 推 `FrontendEvent::MonitorsChanged` + （非 seed 时）调 `reconcile_monitors_changed`
  - 新增 `reconcile_monitors_changed(new_monitors, old_monitors)` 方法：snapshot active bindings → 调两个 pure helper → 对 deactivations 跑 `deactivate_client + FrontendEvent::BindingInvalid`、对 recreations 跑 `deactivate_client + activate_client`
  - 文件底部新增 3 个 pure 函数（不在 `impl Service` 内）：
    - `geometry_to_ipc_monitor_info(&GeometryMonitorInfo) -> IpcMonitorInfo` 字段级镜像转换
    - `reconcile_monitors(active, new, old) -> Vec<(handle, reason)>` 纯函数
    - `recreate_monitors(active, new, old) -> Vec<(handle, old_key, new_key)>` 纯函数
    - `monitor_geometry_changed(a, b) -> bool` 比较 helper（position / size，不含 scale）
  - 新增 `#[cfg(test)] mod reconcile_tests`，8 个单测覆盖 PLAN §M2 STEP-2.6 / §8 矩阵

- `lan-mouse-vue/src/api/ipc.ts`
  - `FrontendEvent` union 加 `MonitorsChanged: MonitorInfo[]` + `BindingInvalid: [ClientHandle, string]`
  - 新增 `MonitorInfo` interface（字段对齐 `lan_mouse_ipc::MonitorInfo`）

- `lan-mouse-vue/src/store/index.ts`
  - `Connection` 加 `invalidReason: string | null` 字段
  - `mergeClient` 在更新现有 connection 时清空 `invalidReason`（让下一次 `State` 事件能 auto-clear）
  - 新增 `state.clients.set` 时初始化 `invalidReason: null`
  - `applyEvent` 加 2 case：`MonitorsChanged`（当前 no-op，类型完备性 + 给 M3 留 hook）+ `BindingInvalid`（写入 `conn.invalidReason`，未知 handle 时 console.warn）

- `lan-mouse-vue/src/components/ConnectionRow.vue`
  - 模板 `<div class="row">` 加 `:class="{ invalid: ... }"` 和 `:title="connection.invalidReason ?? ''"`
  - 标题旁加 `<span class="invalid-badge">⚠ invalid</span>` 带 tooltip
  - `<style scoped>` 加 `.row.invalid`（红边框 + 淡红底色 + 禁用 toggle 透明度 + not-allowed cursor）+ `.invalid-badge`（红圆角徽章）

## 2. 验证结果

```
cargo build --workspace                                              → Finished in 10.40s
cargo test --workspace --no-fail-fast                                → 全部绿：
                                                                       input-capture       47 passed
                                                                       lan-mouse           58 passed  (+8 vs M2.5)
                                                                       input_channel_routing  7 passed
                                                                       quic_smoke             2 passed
                                                                       lan-mouse-ipc        12 passed
                                                                       lan-mouse-proto       5 passed

cargo clippy -p lan-mouse --lib --no-deps -- -D warnings            → 5 pre-existing errors
                                                                       （src/connect.rs:727,728 / src/quic_transport/endpoint.rs:238
                                                                        / src/quic_transport/session.rs:764
                                                                        / "function has too many arguments (8/7)" in connect.rs）
                                                                       全部与 STEP-2.6 无关，留 STEP-2.7 处理

cargo fmt --check -- src/service.rs src/capture.rs                   → exit 0（fmt-clean）

pnpm --dir lan-mouse-vue type-check                                 → exit 0
pnpm --dir lan-mouse-vue build                                       → 173ms, dist/index.html + css 11.08KB + js 83.91KB
```

**新增单测覆盖矩阵**（对应 PLAN §M2 STEP-2.6 / §8 自动测试项）：

| PLAN 要求 | 单测 | 验证点 |
|---|---|---|
| **mock 移除场景**：monitor 消失 → service 调 deactivate + 发 `BindingInvalid` | `reconcile_emits_binding_invalid_when_monitor_removed` | old 有 DP-2 / new 为空 / 1 个 bound client → 输出 1 条 `(handle=7, reason 含 "DP-2" + "disconnected")` |
| **mock 几何变化场景**：monitor size/position 改 → destroy + recreate | `recreate_emits_entry_when_monitor_geometry_changes` | DP-2 size (1920,1080) → (2560,1440) → 1 条 `(handle=11, _old_key, _new_key)` recreate；无 deactivate |
| **no-op 场景**：monitors 无变化 → 不调任何方法 | `reconcile_noop_when_monitor_list_unchanged` + `recreate_noop_when_monitor_geometry_unchanged` | old == new 时两个 helper 都返回空 Vec |
| **启动 seed 不触发 BindingInvalid** | `reconcile_noop_when_old_list_did_not_contain_monitor` | `old=[]` + `new=[DP-2]` + client bound to DP-2 → 0 deactivation |
| **M1 默认 key.monitor=None 不被影响** | `reconcile_noop_when_all_clients_have_default_key_monitor_none` | 所有 client binding 的 monitor=None → 永远 0 deactivation / 0 recreate |
| **混合 binding 状态**：None + Some 并存 | `reconcile_handles_mixed_default_and_bound_clients` | None 客户端永远不会被 deactivate；Some 客户端按规则触发 |
| **镜像转换**：`geometry::MonitorInfo` → `lan_mouse_ipc::MonitorInfo` 字段对齐 | `geometry_to_ipc_monitor_info_is_field_equivalent` | id/name/position/size/primary/scale 6 字段逐项 assert_eq |
| **UI 链路**：WS console 收到 `BindingInvalid`（编译期保证）| `pnpm type-check` exit 0 | TypeScript FrontendEvent union 含 `BindingInvalid` 变体；store applyEvent handle |

**启动 seed 验证**（架构层面，不在单测覆盖内）：
- `CaptureTask::do_capture` 启动时调 `capture.monitors()` → 同步拿初始快照 → emit `ICaptureEvent::MonitorsChanged(initial)`
- `Service::handle_capture_event` 收到时 `self.last_monitors = Some(initial)`（**不**调 reconcile）→ 仅推 `FrontendEvent::MonitorsChanged(ipc_list)`
- 下一次 1 Hz poll 检测到变化 → `last_monitors = Some(new)` + 推事件 + 调 reconcile（diff new vs old）

## 3. 与 PLAN 的偏差

**无 PLAN 偏差**（任务范围按 PLAN §M2 STEP-2.6 / §8 表格 + 涉及文件执行）。

**与 PLAN 隐含期望的 3 处 reinterpretation**：

1. **"订阅各 backend 的 `monitor_changes()` watch channel"** 改为 **"CaptureTask 内部 1 Hz poll `InputCapture::monitors()` + dedup"**：
   - 动机：后端 backend 的 `monitor_changes()` watch receiver 在 `Box<dyn Capture>` 后面取不出来（Capture trait 没暴露 `monitor_changes()` 方法，只暴露 `monitors(&self) -> Vec<MonitorInfo>` 快照），而 prompt "不要做的事" 明确禁止触碰 backend 任何代码 + 禁止修改 Capture trait 定义
   - 替代方案：给 Capture trait 加 `fn monitor_changes(&self) -> Option<watch::Receiver<Vec<MonitorInfo>>>`，需要 5 个 backend 都改 → 违反约束
   - 选定方案：CaptureTask 内部 poll 1 Hz（人类拔插显示器 ≥ 500ms 延迟远超 1s，sub-second 轮询只增加余量），dedup against `last_monitors` 避免无变化时重复 emit
   - 不影响 STEP-2.5 / 2.6 契约：`Capture::monitors()` 签名不变，前端 IPC 协议不变
   - 下一步（M3+）可考虑改 trait 暴露 watch channel 取代轮询；STEP-2.6 在 P2 cosmetic backlog 中留 placeholder

2. **geometry change → destroy + recreate barrier**：M2 `BarrierKey` 不含 geometry（只有 pos + monitor id + offset + span），所以 recompute key 永远等于 old key。但 PLAN §M2 STEP-2.6 显式要求"destroy(old_key) + create(new_key, handle)"。**按 PLAN 走**：recreate 仍触发 `deactivate_client + activate_client` round-trip（同 `update_pos` 模式），destroy/create 在 capture 层 no-op 但 round-trip 一致。
   - 注释里写明动机 + 提示 M4 offset/span 上线后会变得"真"的 recreate
   - 单测 `recreate_emits_entry_when_monitor_geometry_changes` pin 死这一行为

3. **`store` + `api/ipc.ts` 改动超出"涉及文件"列表**：prompt 列出的"涉及文件"是 `src/service.rs` + `ConnectionRow.vue`。但要让 UI 收到 BindingInvalid：
   - `api/ipc.ts` 必须把 `BindingInvalid` 加进 `FrontendEvent` union（否则 `applyEvent` switch exhaustive 检查编译报错）
   - `store/index.ts` 必须有承载 invalidReason 的字段（否则 ConnectionRow 没法 react）
   - 这两处改动已写到 `next/SUGGESTION.md` 让 Leader 复审范围解读
   - 改动总规模：api/ipc.ts 加 22 行、store/index.ts 加 42 行（含 5 行 comment），是承载新事件类型的最小必需

## 4. 处理的 SUGGESTION 项

**新增 #1**：`next/SUGGESTION.md` —
> #1 🟡 Vue store / api/ipc.ts 范围扩展（STEP-M2-2.6 必需）

无 SUGGESTION-FIXED 移动。无 IGNORE 移动。

## 5. 闸门检查

| 闸门 | 结果 |
|---|---|
| 产物对得上吗 | ✅ ICaptureEvent::MonitorsChanged + 1 Hz poll + dedup + 镜像转换 + reconcile + BindingInvalid 推送 + UI 红边框 / 徽章 / tooltip + 8 单测 全部到位 |
| 依赖对得上吗 | ✅ M1.STEP-1.1~1.4、M2.STEP-2.1~2.5 全部 `通过`；`Capture::monitors()` 已就位供 CaptureTask poll；`FrontendEvent::MonitorsChanged` / `BindingInvalid` 已在 STEP-2.1 落地 |
| 验收对得上吗 | ✅ `cargo build --workspace` 通过；`cargo test --workspace` 全绿（47 + 58 + 7 + 2 + 12 + 5 = 131 测试）；`cargo fmt --check` 服务端文件 0 diff；`pnpm type-check` + `pnpm build` 全绿 |
| milestone 边界门 | ✅ 仅触 src/service.rs + src/capture.rs (service 层 glue) + lan-mouse-vue 三个文件；未触碰 backend 5 文件；未触碰 Capture trait；未触碰 lan-mouse-proto 协议；未触碰 M1 数据模型（`BarrierKey.monitor` 已就位，`get_key` / `client_at` / `update_pos` 路径未改）；未触碰 M3 dropdown / M4 canvas |
| 时间预算门 | ✅ 实际 ~50 min（含 8 单测 + Vue 2 文件改动 + fmt + clippy 修整），略超 STEP 估时 30 min，但远低于 executor 上限 1h，未触发拆步 |
| 闸 3 milestone 收尾 | ⏸ 跳过；STEP-2.6 不是 M2 收尾步骤，fmt + clippy workspace 级清理 + 真机拔插留给 STEP-2.7 |

## 6. 遗留

- **`monitor_changes()` watch channel 轮询 vs 订阅**：当前 1 Hz poll 是绕过 `Capture` trait 限制的临时方案。M3+ 可考虑给 trait 加 `fn monitor_changes(&self) -> Option<watch::Receiver<Vec<MonitorInfo>>>` 取代轮询。已在 `MONITOR_POLL_INTERVAL` docstring 里写明动机 + 指 STEP-2.7 可做 polish
- **geometry change 触发 recreate 但 BarrierKey 相同**：M2 不影响功能（destroy/create 同 key 是 no-op），M4 offset/span 上线后这条路径会变得"真"。recreate_emits_entry_when_monitor_geometry_changes 单测 pin 死"即使 key 相等也 emit"的保守语义
- **`old_monitors == None`（first observation）→ 不触发 reconcile**：当前实现正确（`handle_capture_event` 只在 `last_monitors.replace(...)` 返回 `Some(old)` 时才调 reconcile），但意味着"启动时已有 client + 第一次 MonitorsChanged 来时 monitor 已经在列表里"不会被触发 BindingInvalid。如果用户启动 daemon 时已经拔了外接显示器，且 binding 在配置中指向该显示器，理论上应触发 deactivation — 这条边角 case 等 M3 `ClientConfig.monitor` 上线后再处理（prompt "M1 已落地的字段不要改"）
- **`scale` 不参与 geometry_changed 比较**：M2 sub-edge barrier 还没上线（plan 是 M4），scale 不影响物理 barrier rectangle。当前 `monitor_geometry_changed` 只看 `position` + `size`。M4 上线时同步扩展
- **`applyEvent` 中 `MonitorsChanged` 当前 no-op**：M3 dropdown 需要 `state.monitors: MonitorInfo[]` 字段，那是 M3.2 scope，本步只把事件类型接上不动数据流。store 端 `console.log` / `state.monitors = value` 等落地留 M3
- **clippy 5 个 pre-existing errors**：src/connect.rs:727,728 + src/quic_transport/endpoint.rs:238 + src/quic_transport/session.rs:764 + 1 个 too-many-arguments 都在 STEP-2.6 范围外，留 STEP-2.7 milestone 收尾统一 fmt/clippy 处理
- **真机三平台拔插验证**：单测覆盖 reconcile 纯逻辑；OS-specific hot-plug signal（CGDisplay reconfig / WM_DISPLAYCHANGE / wl_output register-deregister / libei ZonesChanged）的端到端流由 STEP-2.7 人类在 macOS / Windows / Linux 真机验证
- **SUGGESTION #1 (Vue store 范围)**：见 §4，待 Leader 复审

## 7. 下一步

派发 **STEP-2.7** — `cargo fmt --check` + `cargo clippy -p input-capture -p lan-mouse-ipc -p lan-mouse-vue --all-targets -- -D warnings` + 真机三平台拔插验证。

预估 ~30 min（人类配合占大头）；前置依赖：✅（STEP-2.6 已完成）
