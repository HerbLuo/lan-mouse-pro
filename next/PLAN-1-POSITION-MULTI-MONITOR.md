# PLAN-M1: 多屏友好的被控端定位

> 目标：解决"位置=上下左右"在主控端多屏时的歧义（如"top"实际只对最左屏生效）。
> 终点：**方案 C** — 后端正确地按显示器枚举并集计算暴露边段；前端用画布编辑器让用户把 client 挂到具体边段。
> 不在本计划内：M5 协议层光标 warp、M6 边沿停留手势。

## 0. 范围

### In Scope
- M0 — macOS 后端正本清源（并集语义，修 `update_bounds` 不重置）
- M1 — `BarrierKey` 数据模型重构（纯内部，无 UX 变化）
- M2 — 各 OS 显示器枚举与稳定 ID + 热插拔
- M3 — `ClientConfig.monitor` + 前端下拉框（**用户报告问题的最小修复**）
- M4 — `exposed_segments` 几何算法 + SVG 画布编辑器

### Out of Scope（后续 PLAN）
- M5 — 协议层 `CursorPos` + 接收端 `warp`（Wayland 大多数合成器不支持，需协议能力位协商）
- M6 — 边沿停留 / 双推手势（旁路廉价补丁）
- M7+ — 完整 2D 编辑器（被控端屏幕广播 + 拖拽）

## 1. 架构概览

```
┌──────────────────────────────────────────────────────────────────┐
│ input-capture::geometry                                           │
│  - DisplayRect { x, y, w, h }          (per-OS 枚举的原始矩形)    │
│  - DisplayBound { rect, monitor_id }   (per-OS 枚举的矩形+id)    │
│  - BarrierKey { pos, monitor, offset, span }   (主键)             │
│  - crossed_pure / activation_pure       (CI 可单测的纯几何查询)   │
│  - exposed_segments(Vec<DisplayRect>) -> Vec<EdgeSegment>          │
│      切分显示器并集的外轮廓为最大暴露线段                           │
└──────────────────────────────────────────────────────────────────┘
        ▲ 枚举                     ▲ 主键                       ▲ 计算
        │                          │                            │
┌───────┴────────┐         ┌───────┴────────┐         ┌─────────┴──────┐
│  macOS/Windows/│         │ InputCapture   │         │ FrontendEvent   │
│  Wayland/libei │ ───POS──>  position_map:  │ ───IPCSOCK──> LayoutChanged│
│  backends      │      +key  HashMap<BK,Vec<Handle>>  │ MonitorsChanged│
└────────────────┘         └────────────────┘         └────────────────┘
                                                          │
                                                          ▼
                                              ┌──────────────────────┐
                                              │ LayoutEditor.vue     │
                                              │  - SVG 画布：显示器   │
                                              │    按真实几何比例     │
                                              │  - 高亮 EdgeSegment   │
                                              │  - click/drag 挂client│
                                              └──────────────────────┘
```

**协议兼容**：`ProtoEvent::Enter(Position)` 仍只携带对端方向（接收端不做绝对光标定位），本计划全程**无需 bump** `lan-mouse-proto`。

---

## 2. 里程碑路线图

| 里程碑 | AI 估时 | 人类可测功能 | 依赖 | 状态 |
|---|---|---|---|---|
| **M0** macOS 正确性 | ~0.4h（实测 ~15m） | macOS 多屏边触发不再"只最左屏" | — | ✅ 已完成 |
| **M1** BarrierKey 重构 | ~1.8h | （内部）单屏行为零变化 | M0 | 待启动 |
| **M2** 显示器枚举 + 热插拔 | ~3.5h | WebSocket 收到 monitors 列表；拔插触发 `BindingInvalid` | M1 | 待启动 |
| **M3** 显示器绑定 UI | 3.5h（含 3.4-3.6 M3 落地回归 bug 修复 ~2h） | **解决用户问题**：选 monitor 即可 | M2 | 进行中（含 3.4-3.6 修复） |
| **M4** 暴露边段 + 画布 | ~9h | SVG 画布可视化拖拽挂 client；libei 后端降级 | M3 | 待启动 |
| **合计** | **~18h** | | | |

> **时间校准说明**：M0 计划 2.3h，实测 AI ~15m、人类 ~25m。整体估时按 ~1/3 系数下调，STEP 粒度按 AI 单次执行 ~30 分钟合并。

---

## 3. 详细步骤

> AI STEP 默认按 ~30 分钟单次执行量合并；超过 45 分钟的 STEP 会被 LEADER 介入重拆。
> 人类配合的 STEP 不计时、不拆分，只列"需要人做什么 / 在哪种排布下做"。

---

### M0 — macOS 后端正确性 ✅ 已完成

**AI 执行时间**：~15 分钟
**人类配合时间**：~25 分钟

**实际交付**（commit: b38cb6f, ab0e076, 2841d54）：
- ✅ `input-capture/src/geometry/mod.rs`：新建公共几何模块（DisplayRect / entered_barrier / cursor_within / clamp_to_display_bounds / 等）
- ✅ `input-capture/src/windows/display_util.rs` 删除，纯函数迁到 geometry
- ✅ `input-capture/src/macos.rs::Bounds` 改为 `Vec<DisplayRect>`；`update_bounds` 先清空再填
- ✅ `crossed()` / `compute_edge_point` 改用 `entered_barrier(prev, curr, displays)`；删 `delta` 预测
- ✅ 找不到显示器时返回 `None`，由调用方决定 warp 兜底（不塌缩到 union bbox）
- ✅ 回归：单屏行为零差异；2x1 横排双屏 `top` 从任一屏顶部都能正确触发

---

### M1 — BarrierKey 数据模型重构

**目标**：扩展数据模型加入 `monitor/offset/span`，纯内部重构，无 UX 变化。
**AI 估时**：~1.8 小时
**依赖**：M0

**人类准备**：
- 单屏端到端回归：GUI 完整走一遍 `config → activate → 触发 top/bottom/left/right → release`，与重构前像素级一致

| STEP | 估时 | 任务 | 涉及文件 | 完成标志 |
|---|---|---|---|---|
| 1.1 | 30m | `geometry::BarrierKey { pos, monitor: Option<MonitorId>, offset: u16, span: u16 }`（Default = monitor=None, offset=0, span=10000）；`InputCapture.position_map` / `id_map` 重键；`Capture::create(&BarrierKey)` / `destroy(&BarrierKey)` / `start_capture(&BarrierKey)` / `cancel_pending(&BarrierKey)`；`Stream::Item = (BarrierKey, CaptureEvent)`；`poll_next` 重写为 fan-out + 顺手修空集合 waker 不重注册的丢 poll bug | `geometry.rs`、`lib.rs` | 公共 API 签名变更；编译错误暴露所有调用点；`tokio_test::assert_pending!` 单测覆盖 waker 修复 |
| 1.2 | 30m | 五个 backend 同步改签名（macos / windows / windows::event_thread / layer_shell / libei / dummy）；内部存储 `BarrierKey` 但**目前只读 `pos` 字段**（兼容层）；dummy 改为产出对应 `BarrierKey` | 各 backend `Capture` impl | `cargo build -p input-capture` 通过；旧行为未变 |
| 1.3 | 30m | `src/capture.rs::CaptureTask` 改持 `Vec<(CaptureHandle, BarrierKey, CaptureType)>`；`State::Pending { handle, key }`；事件路由改用 `key`；`src/service.rs::activate_client` 用 `client_manager.get_key(handle)` 取 `BarrierKey`；`client_at(key)` 替换 `client_at(pos)`；`update_pos` 重算 key 后 deactivate/recreate；`monitor`/`offset`/`span` 暂固定默认 | `src/capture.rs`、`src/service.rs` | `cargo build --workspace` 通过；`src/capture_test.rs` / `src/emulation_test.rs` 全绿 |
| 1.4 | 20m | `cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings`；单屏 GUI 完整走一遍（人类配合） | — | L1 全绿 + 单屏端到端零行为差异 |

**M1 里程碑交付**：
- 全 workspace 编译通过、测试全绿
- 单屏端到端零行为差异
- 数据模型已就绪，等待 M2/M3 注入 monitor 维度的语义

---

### M2 — 显示器枚举与稳定 ID

**目标**：每个 backend 枚举出当前主机的显示器列表（含稳定 ID），并在热插拔时主动通知前端。
**AI 估时**：~3.5 小时
**依赖**：M1

**人类准备**：
- **macOS 多屏一台**：双屏拔插后 WebSocket console 看到 `MonitorsChanged`；拔插时若 active client 绑在被拔 monitor → 收到 `BindingInvalid`（UI 高亮 + tooltip）
- **Windows 多屏一台**：同上
- **Linux Wayland (layer_shell，KDE/GNOME/Sway 任一)**：拔插 `wl_output` global 后 monitors 列表更新
- **Linux GNOME Wayland (libei portal)**：改变 zones 后 monitors 列表更新

| STEP | 估时 | 任务 | 涉及文件 | 完成标志 |
|---|---|---|---|---|
| 2.1 | 30m | `geometry::MonitorInfo { id, name, position, size, primary, scale }`（带 serde derive，lowercase snake_case）；`lan-mouse-ipc::FrontendEvent::MonitorsChanged(Vec<MonitorInfo>)`；`MonitorInfo` 在 IPC 模块镜像一份；`FrontendEvent::BindingInvalid(handle, reason)`；serde round-trip 单测覆盖 UTF-8 名称、负坐标、不同 scale | `input-capture/src/geometry.rs`、`lan-mouse-ipc/src/lib.rs` | IPC 类型可序列化；老 wire（缺字段）= 空 vec，向后兼容 |
| 2.2 | 30m | macOS `enumerate_monitors()`：`CGDisplay::active_displays()` 拿 ID → `CGDisplay::new(d).bounds()` 拿矩形 → `IODisplayCreateInfoDictionary` 拼 vendor/model/serial/location 生成稳定 `id`；接现有 `DisplayReconfigured` 路径，触发后通过 `notify_tx` 发新列表 | `input-capture/src/macos.rs`（新增 fn、回调内推新事件） | macOS 双屏：拔插后日志看到 monitors 列表变化 |
| 2.3 | 30m | Windows `enumerate_displays()`：已有的 `EnumDisplayDevicesW` + `EnumDisplaySettingsW` 路径，把 `DeviceID`（含 EDID hash）做稳定 `id`；接现有 `WM_DISPLAYCHANGE` 路径（generation counter 已就绪）；新增 `scale` 从 `dmLogPixels` 或注册表取 | `input-capture/src/windows/event_thread.rs`（增强 `enumerate_displays`） | Windows 双屏：拔插后 monitors 列表更新 |
| 2.4 | 30m | layer_shell：复用现有 `OutputInfo.name` / `description`；优先用 EDID（如 description 里有），否则 `name + position` 作 fallback `id`；已有 `register_global` / `deregister_global` 路径直接复用。libei：portal `Zones.regions()` 返回的 region 没有名字；用 `x_offset, y_offset` 作 fallback `id`；接现有 `receive_zones_changed` | `input-capture/src/layer_shell.rs`、`input-capture/src/libei.rs` | Wayland / libei 拔插 monitors 列表更新 |
| 2.5 | 30m | `Capture` trait 加 `fn monitors(&self) -> Vec<MonitorInfo>`；五个 backend 实现；`InputCapture::monitors()` 转发；macOS `ProducerEvent::MonitorsChanged`、windows `DISPLAY_RESOLUTION_GENERATION` 变化触发时推新列表 | `input-capture/src/lib.rs`（trait）、各 backend | `cargo build -p input-capture` 通过 |
| 2.6 | 30m | `src/service.rs`：捕获 `input_capture` 的 monitors 变更（channel 或轮询），转 `FrontendEvent::MonitorsChanged`；启动时主动发一次；`reconcile_monitors_changed`：对每个 active client 检查 `BarrierKey.monitor` 是否仍存在，不存在则 deactivate + 发 `BindingInvalid`；存在但几何变了则重算 key 并 destroy/create barrier；单测覆盖 mock 移除场景 | `src/service.rs`、`lan-mouse-vue/src/components/ConnectionRow.vue`（高亮 + tooltip） | 单测：mock 移除 → service 调用 deactivate + 发事件；UI: WebSocket console 收到 `BindingInvalid` |
| 2.7 | 30m | `cargo fmt --check` + `cargo clippy -p input-capture -p lan-mouse-ipc -p lan-mouse-vue --all-targets -- -D warnings`；真机拔插（人类配合） | — | L2 全绿 + 三个平台拔插行为符合预期 |

**M2 里程碑交付**：
- 浏览器 console（或 WebSocket 客户端）能收到当前主机的 monitors 列表（含 id/name/position/size/scale）
- 拔插显示器后再次收到更新事件
- 拔掉的 monitor 上若有 active client，前端收到 `BindingInvalid` 并暂停 toggle
- 不影响任何现有功能

---

### M3 — 显示器绑定 UI

**目标**：用户能在 GUI 把 client 绑到具体显示器；同一 `Position` 不同显示器可绑不同 client。
**AI 估时**：~3.5 小时（含 3.4-3.6 M3 自身引入型回归 bug 修复 ~2h）
**依赖**：M2

**人类准备**：
- **macOS 双屏 + 一台对端实例**：设 `top` 绑定右屏，光标在右屏顶部应正确切到对端；切到左屏顶部不触发
- **L 形 / 错位排布**：验证 dropdown 选择行为 + UI 显示"已知限制"提示
- **Linux Wayland (layer_shell)** 后端跑一遍 dropdown，与 macOS 一致
- **旧 config（无 `monitor` 字段）**：加载后 dropdown 默认显示 "Any" 且行为不变

| STEP | 估时 | 任务 | 涉及文件 | 完成标志 |
|---|---|---|---|---|
| 3.1 | 30m | **后端链路**：`lan-mouse-ipc::ClientConfig.monitor: Option<String>`（Default 不变，缺字段 = None）；`TomlClient.monitor: Option<String>` + `ConfigClient::From<&TomlClient>` / `From<&ConfigClient>`（保存时 `None` 不写入）；`FrontendRequest::UpdateMonitor(handle, Option<String>)`；`lan-mouse-cli::SetMonitor` 子命令；`src/service.rs::update_monitor(handle, monitor)`：根据当前 `pos` 和新 `monitor` 算新 `BarrierKey`（offset/span 仍默认）；`capture.destroy(old_key)` → `capture.create(new_key, handle)`；若 active 先 deactivate 再 activate；`save_config` 落盘；反序列化测试覆盖缺字段兼容 | `lan-mouse-ipc/src/lib.rs`、`src/config.rs`、`lan-mouse-cli/src/lib.rs`、`src/service.rs` | 旧 config 仍能反序列化；新字段可选；单测：改 monitor 后 BarrierKey 实际变更 |
| 3.2 | 30m | **前端链路**：Vue `api/ipc.ts` 新增 `MonitorInfo` 类型 + `MonitorsChanged` event + `UpdateMonitor` request；Vue `store/index.ts` 维护 `state.monitors: MonitorInfo[]`，`onMounted` 监听 `MonitorsChanged`，`updateClientConfig` 加 monitor 字段 diff/send + type guard；`components/ConnectionRow.vue` 在 position `<select>` 后面加 monitor `<select>`，options = `[<Any>, ...state.monitors.map(...)]`，label tooltip 显示显示器 position/size；空选项 = "Any (back-compat)" | `lan-mouse-vue/src/api/ipc.ts`、`lan-mouse-vue/src/store/index.ts`、`lan-mouse-vue/src/components/ConnectionRow.vue` | 浏览器 console 看到 monitors 同步到 store；dropdown 渲染正确；vitest 单测 + snapshot 覆盖三种 case |
| 3.3 | 30m | `cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` + `cd lan-mouse-vue && pnpm build`；真机双屏 + L 形 + 旧 config + Linux 后端回归（人类配合） | — | **用户报告问题解决** + 所有 case 通过 |
| 3.4 | 40m | **macOS + layer_shell backend 感知 monitor（修复 #1 + 同形 bug #3）**：<br>• macOS 部分：(a) 在 `geometry/mod.rs` 新增 `pub struct DisplayBound { rect: DisplayRect, monitor_id: Option<MonitorId> }`；(b) 在 `macos.rs` 新增纯函数 `fn build_display_bounds(active_ids: &[CGDirectDisplayID], monitors: &[MonitorInfo]) -> Vec<DisplayBound>` —— **以 `enumerate_monitors`（STEP-2.2 产出的 `Vec<MonitorInfo>`）为单一信息源**：把 `monitor.position` 直接映射成 `rect.x/y`、`monitor.size` 映射成 `rect.w/h`，避免当前「`update_bounds` 走一遍 `CGDisplay::active_displays + bounds()`，`enumerate_monitors` 再走一遍 `CGDisplay::active_displays + IOKit`」的双 Quartz 调用与瞬态不一致；(c) `InputCaptureState.displays: Vec<DisplayRect>` 升级为 `Vec<DisplayBound>`；(d) `update_bounds` 退化为薄包装：`CGDisplay::active_displays()` 拿 id + 调 `enumerate_monitors_for_ids(&active_ids)`（新签名，与现有 helper 共用 IOKit 代码）→ 调纯 `build_display_bounds` 构造 `Vec<DisplayBound>`；(e) `crossed()` 内部查询逻辑抽到 `geometry/mod.rs::crossed_pure(prev, curr, displays: &[DisplayBound], active: &HashSet<BarrierKey>) -> Option<BarrierKey>`（半开约定 + `display_containing` 找 prev_pos 所在 display 的 idx → 取 `monitor_id` 构造完整 `BarrierKey` → `active.contains(&key)`），`crossed()` 退化为薄包装（`FFI` 调用 + `crossed_pure`）。<br>• layer_shell 同形 bug（与 macOS `crossed` 不同形，但同因）：`Capture::create` / `destroy` 当前的 `self.add_client(BarrierKey::from_pos(key.pos))`（layer_shell.rs:902 / 908）剥掉 `monitor / offset / span`。改为接完整 `&BarrierKey`：`add_client(key.clone())` / `delete_client(key.clone())`，并相应调整 `add_client` / `delete_client` 签名（不再做 `from_pos` 重建）。<br>• 4×2 矩阵单测在 `geometry/mod.rs`（CI 可跑，无需 FFI）。明确半开约定：`display_containing` 在 `(x, y)` 处左/上含、右/下不含；两屏接缝 `(1920.0, 540.0)` 在横排 2x1 下归到 `display_0`（左侧）。测试矩阵每个 case 写清 `(prev_pos, curr_pos, active_clients)` 三元组与断言 key：<br>  - C1: prev 在 display_0 中心 → curr 越过 top → active 含 `monitor: Some(d0.id)` 的 Top key → hit（query key = `Some(d0.id)`）<br>  - C2: prev 在 display_1 中心 → curr 越过 top → active 含 `monitor: Some(d1.id)` 的 Top key → hit<br>  - C3: prev 在 display_0 → curr 越过 top → active **不含** d0.monitor 的 Top key、只含 `monitor: None` 的 Top key → miss<br>  - C4: prev 在接缝 `(1920.0, 540.0)` → curr 越过 top → 归到 d0；active 含 `monitor: Some(d0.id)` → hit；active 含 `monitor: Some(d1.id)` → miss<br>  - C5: prev 屏外 → entered_barrier 返回 None → 直接 miss<br>  - C6: prev 在 display_0 → curr 越过 top → active 含 `monitor: Some(d0.id)` 的 Top key + `monitor: Some(d1.id)` 的 Top key → 只 d0 命中（query 是 d0）<br>  - C7: 2x1 横排左穿到右：prev 在 display_0 右边缘外侧 → curr 越过 right → active 含 d0.monitor 的 Right key → hit<br>  - C8: 镜像对照 prev 在 display_1 左边缘外侧 → curr 越过 left → active 含 d1.monitor 的 Left key → hit（d0.monitor Left 不命中）<br>• `build_display_bounds` 单测：构造 `active_ids + monitors` fixture → 断言 id 唯一、`display_containing` 拿到的 idx 对应的 monitor_id 一致、`Vec` 长度 = `active_ids.len()`<br>• layer_shell 单测：构造 `state.active_positions` 注入完整 `BarrierKey { monitor: Some("wl-output-name"), pos: Top, ... }`，确认 `add_client` 不剥字段 | `input-capture/src/geometry/mod.rs`（加 `DisplayBound` + `crossed_pure`）、`input-capture/src/macos.rs`（新增 `build_display_bounds` + `enumerate_monitors_for_ids` + 改 `update_bounds` + `crossed` 薄包装）、`input-capture/src/layer_shell.rs`（改 `Capture::create` / `destroy` + 改 `add_client` / `delete_client` 签名） | `cargo test -p input-capture::geometry` 全绿（含 4×2 矩阵 8 个 case + `build_display_bounds` 单测）；`cargo test -p input-capture::layer_shell`（cfg gate）全绿；旧 macOS 单测零回归；`Cargo.lock` 不变 |
| 3.5 | 40m | **Windows backend 感知 monitor（修复 #2）+ libei sanity 测试重构**：<br>• Windows 部分：(a) `DISPLAYS: RefCell<(Vec<DisplayRect>, i32)>` 升级为 `DISPLAYS: RefCell<(Vec<DisplayBound>, i32)>`；(b) `update_display_regions` 利用 STEP-2.3 已就绪的 `enumerate_displays_inner() -> Vec<WinDisplayInfo>` 单源（已有 `bounds` + `device_id`），**不再做 bounds 中心点匹配** —— 直接 `WinDisplayInfo.device_id` 经 `build_stable_id` 生成 `windows:...` 稳定 id，与每个 `WinDisplayInfo` 的 `bounds` 一一对应，构造 `Vec<DisplayBound>`；(c) `check_client_activation` 内部查询逻辑抽到 `geometry/mod.rs::activation_pure(prev, curr, displays: &[DisplayBound], active: &HashSet<BarrierKey>) -> Option<BarrierKey>`（与 `crossed_pure` 同形，复用同一套「display_containing + idx → monitor_id」逻辑）；`check_client_activation` 退化为薄包装；(d) 单测在 `geometry/mod.rs`（CI 可跑），无需 `MSLLHOOKSTRUCT` / `WPARAM` 构造。<br>• **dummy backend**：`dummy.rs:180-190` 已有 `with_keys_preserves_monitor_offset_span` 单测（STEP-1.2 落地），覆盖「完整 BarrierKey round-robin 不剥 monitor 字段」语义，**无需新增**。3.5 在完成标志里只引用、不重复写。<br>• **libei sanity**：现有 `select_barriers(zones: &Zones, ...)` 接收 `ashpd::desktop::input_capture::Zones`（`pub struct` 但字段全私有），`Region` 同样无 pub 构造器；测试无法注入 fixture。重构签名：`select_barriers(regions: &[(u32, u32, i32, i32)], clients: &[BarrierKey], ...) -> (Vec<ICBarrier>, HashMap<BarrierID, BarrierKey>)` 接受纯 `(width, height, x_offset, y_offset)` 元组 vec；外层 `update_barriers` 薄包装：把 `Zones.regions()` 的 `width/height/x_offset/y_offset` 提取成元组 vec，调 `select_barriers`。单测：构造 `regions = vec![(1920, 1080, 1920, 0)]`（横排 2x1 右屏）+ `clients = vec![BarrierKey { monitor: Some("region-1"), pos: Top, ... }, BarrierKey { monitor: None, pos: Top, ... }]` → 断言返回 `barriers.len() == 2`（两条都进入 barrier 列表）+ `key_for_barrier` map 含两条 key（防回归：以前 libei 之所以天然兼容只是因为它把整个 `active_clients` vec 喂给 EIS，没有「重建查询 key」这一步；现在显式加单测锁住这一行为，避免后人重构时引入与 macOS / Windows 同形 bug） | `input-capture/src/geometry/mod.rs`（加 `activation_pure`）、`input-capture/src/windows/event_thread.rs`（`DISPLAYS` 升级 + `update_display_regions` 用 `WinDisplayInfo.device_id` + `check_client_activation` 薄包装）、`input-capture/src/libei.rs`（`select_barriers` 签名重构 + `update_barriers` 薄包装） | `cargo test -p input-capture::geometry` 全绿（含 Windows `activation_pure` 6 个 case）；`cargo test -p input-capture::libei`（cfg gate）全绿；`cargo test -p input-capture::dummy` 全绿（复用 STEP-1.2 已有的 round-robin 单测，无需新增）；Windows 单测覆盖 prev_pos 在 display_0/display_1/屏外 × active key `monitor: Some / None` |
| 3.6 | 20m | `cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo test --workspace`；真机回归（人类配合） | — | L1 全绿 + 用户报告问题在真机真解决（macOS / Windows / Linux Wayland layer_shell 三平台各跑一遍） |

**M3 里程碑交付**：
- GUI 多显示器下拉框，按显示器名称展示
- 选 monitor 后实时生效（无需重启）
- 旧 config（无 `monitor` 字段）= 任意显示器，行为不变
- ⚠️ **M3 落地回归 bug（由 3.4 / 3.5 收尾修复）**：M3.1 把 `monitor` 字段写入 `active_clients` 的 BarrierKey 后，**四个 backend** 的 query / 存储路径仍按旧语义忽略 `monitor` 字段：
  - **macOS**（`input-capture/src/macos.rs:155-164` `crossed()`）：用 `BarrierKey::from_pos(pos)` 构造查询 key（monitor 钉死为 None），与 `active_clients` 里 `monitor: Some("macos:0000:0000::unknown-1")` 的 key 永远不等价，导致 100% 边缘检测 miss。
  - **Windows**（`input-capture/src/windows/event_thread.rs:437-445` `check_client_activation`）：完全相同的模式，同样 100% miss。
  - **layer_shell**（`input-capture/src/layer_shell.rs:898-911`）：`Capture::create` / `destroy` 用 `BarrierKey::from_pos(key.pos)` 重建 key 再 `add_client` / `delete_client`，剥掉 `monitor / offset / span`。layer_shell 不做 `crossed()` 风格查询（查询就是同一份 HashSet），所以 barrier 仍能触发，但用户效果是"M3 dropdown 在 layer_shell 上完全无效果"。
  - **libei**：天然不受影响（`input-capture/src/libei.rs:677-678` 直接喂整个 `active_clients` vec 给 EIS `set_pointer_barriers`，不重建查询 key），但 3.5 仍加显式单测锁住这一行为防回归。
  - 修复范围 scope guard：仅修这个回归 bug + 同形 layer_shell bug，**不扩展** M4 `exposed_segments` / `sub-edge`（offset / span）维度。

**M3 已知限制**（补丁 #4）：
- **L 形错位排布**（一个显示器在另一个显示器上方错位 N 像素）下，错位重叠区的 boundary 行为由 OS 决定，本计划不保证 boundary 一定按用户指定的 monitor 收敛。M4 画布编辑器会用**不同颜色高亮**错位区（多 monitor 覆盖同一逻辑边），但 binding 选择仍按用户在 dropdown 里指定的 monitor 为准；用户需自行理解"这一段被另一个 monitor 部分遮挡"。
- **混合 DPI**：本计划不修。若两个显示器 scale factor 不同（如 1.0 + 2.0），monitor 粒度 binding 仍按 OS 坐标计算，可能在用户感知上"位置不对"。这是独立的 bug，需后续 PLAN。
- **M3 落地回归 bug（3.4 / 3.5 收尾）**：见上方"M3 里程碑交付"第 4 条 + STEP-3.4 / 3.5。性质是 M3 自身的引入型 bug（monitor 字段加进了 BarrierKey 主键，但 macOS / Windows backend 的查询 key + layer_shell 的存储 key 仍按旧语义构造），不是 OS / 硬件限制，因此单独 STEP 修复而非列入"已知限制"长期背负。3.4 / 3.5 完工后此限制项关闭。

---

### M4 — 暴露边段 + 画布编辑器

**目标**：后端按真实几何算出暴露边段；前端用 SVG 画布可视化展示；用户点/拖 client 到具体边段。
**AI 估时**：~9 小时
**依赖**：M3

**人类准备**：
- **3 种排布**（横排 2x1 / L 形错位 200px / 上下不等高 1080p+4K）：每种排布下走一遍 click/drag 流
- **拔插一次**：验证画布重排动画顺滑无视觉跳变
- **Linux KDE Plasma 6 + layer_shell**：4.2a 真机 1px surface 可点击；offset=5000 span=5000 触发正确
- **Linux Sway / Hyprland**：4.2b 真机测试；不支持 ≤N px 的合成器自动降级（前端 slider 隐藏）
- **Linux GNOME Wayland (libei)**：画布可见但 ConnectionRow tooltip 显示"(portal 后端：monitor 粒度上限)"；offset/span slider 不渲染
- **单屏降级**：拔掉外接显示器后画布消失，ConnectionRow 仍可用

| STEP | 估时 | 任务 | 涉及文件 | 完成标志 |
|---|---|---|---|---|
| 4.1 | 30m | **exposed_segments 算法 + 单测**：`geometry/hull.rs::exposed_segments(Vec<DisplayRect>) -> Vec<EdgeSegment>` — 扫描每条 pos 边，收集在垂直方向**实际暴露**的线段（看 span 是否被邻居完全遮挡）；`EdgeSegment { pos, monitor_id, x_start, x_end }`（或 y for top/bottom）；单测覆盖 2x1、3x1、L 形错位 200px、上下不等高（双屏 1080p+4K） | 新建 `input-capture/src/geometry/hull.rs` | `cargo test -p input-capture::geometry` 全部 case 绿 |
| 4.2 | 30m | **exposed_segments 边界 case 调试**：针对 4.1 单测失败的 case 重写算法（不等高 + 部分遮挡的双行 + 旋转显示器 + 镜像排布）；每发现一个 edge case 加单测；`hull::debug_print(displays)` 函数打印 segments（用于人工对照 + snapshot test） | `input-capture/src/geometry/hull.rs` | 5 种排布（含镜像）全绿；debug_print 与手算结果一致 |
| 4.3 | 30m | **layer_shell 子边屏障（backend 逻辑）**：用 `set_margin` + `set_size` 把 1px 表面推到目标段（`Anchor::Top + margin.top + set_size(1, h*span/10000)`）；同时设置 input region 显式 1px×N 像素；macOS 在 `crossed_pure` 额外检查 prev/curr.y（或 x）是否落在 `[min + offset/10000 * extent, ... + span/10000 * extent)`；windows 改 `activation_pure` 加 range 参数；backend 单测覆盖 offset=5000, span=5000 / 0 / 10000 边界值 | `input-capture/src/macos.rs`、`input-capture/src/windows/event_thread.rs`、`input-capture/src/layer_shell.rs` | 编译期 assert 边界值；逻辑路径单测绿 |
| 4.4 | 30m | **layer_shell 子边屏障（合成器适配）**：Sway 对 sub-pixel margin 取整到 1px → 实测最小 surface 尺寸；Hyprland 在多 monitor 不同 scale 下 margin 缩放行为；如合成器实际不支持 ≤N px surface，runtime 检测并降级为全边；前端在画布上隐藏 offset/span slider（runtime detection：`segments[i].max_span == 10000` 时隐藏 slider） | `input-capture/src/layer_shell.rs` + 文档 | 文档记录每种合成器支持情况；runtime detection 标记生效 |
| 4.5 | 30m | **libei 子边屏障降级 + 文档**：发现并记录 portal `Zones.regions()` 不支持 sub-region；libei backend `Capture::create` **忽略 offset/span 参数**（仅按 monitor + pos 创建全边屏障）；ConnectionRow monitor dropdown tooltip 加 "(portal 后端：monitor 粒度上限)"；`docs/limitations.md` 补一段；libei backend 单测覆盖传入 offset/span 时实际仍为 monitor 全边 | `input-capture/src/libei.rs`、`lan-mouse-vue/src/components/ConnectionRow.vue`、新建 `docs/limitations.md` | libei 单测绿；portal 文档引用；ConnectionRow tooltip 文案 |
| 4.6 | 30m | **IPC + service**：`FrontendEvent::LayoutChanged { monitors: Vec<MonitorInfo>, segments: Vec<EdgeSegmentWire> }`（serde round-trip 单测）；`src/service.rs` 在 monitors 变更后调 `exposed_segments(monitors)` 一并发出；启动时主动发一次 | `lan-mouse-ipc/src/lib.rs`、`src/service.rs` | console 收到 `LayoutChanged` |
| 4.7 | 30m | **LayoutEditor.vue 画布基础**：按 1cm ≈ 28px 缩放画每个显示器为矩形 + 名称 + 分辨率标签 + 真实 position/size；高亮所有 `segments`（按 `pos` 着色）；单屏时 `v-if` 不渲染（降级到 ConnectionRow） | 新建 `lan-mouse-vue/src/components/LayoutEditor.vue` | 画布渲染出当前多屏布局；segments 可见；单屏降级生效 |
| 4.8 | 30m | **LayoutEditor.vue click/drag 挂 client**：点段 → 弹"挂哪个 client"或直接挂当前选中 client；hover 显示已分配 client 名称；空状态回退到 ConnectionRow 模式 | `lan-mouse-vue/src/components/LayoutEditor.vue` | 画布 click 挂 client；hover 显示分配 |
| 4.9 | 30m | **Vue store + 双向同步 + 降级 + 动画**：store 加 `state.segments: EdgeSegment[]`、`state.assignments: Map<ClientHandle, SegmentId>`；`updateClientConfig` 走 segment path（compute BarrierKey from segment）；ConnectionRow ↔ LayoutEditor 双向同步（选 ConnectionRow monitor，画布对应 segment 高亮；反之亦然）；热插拔时画布 CSS transition on `<rect>` `x`/`y`/`width`/`height`；offset/span 暂不暴露 UI（M4 暂只用 monitor 维度） | `lan-mouse-vue/src/store/index.ts`、`lan-mouse-vue/src/components/{ConnectionRow.vue, LayoutEditor.vue, ConnectionsPanel.vue}`、`lan-mouse-vue/src/styles/main.css` | 双向同步无错位；单测覆盖 ConnectionRow ↔ LayoutEditor 双向 case；transition 属性存在性可单测 |
| 4.10 | 30m | `cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` + `pnpm build`；3 种排布下走 click/drag 流 + 拔插一次（人类配合） | — | L4 全绿 + UX 顺滑 |

**M4 里程碑交付**：
- 完整可视化布局编辑器（SVG 画布）
- 用户零学习成本：看到的是真实桌面布局，点击的就是要挂的位置
- 单屏降级到 ConnectionRow，不打扰
- 热插拔实时重排

---

## 4. 总估时汇总

| 里程碑 | AI 估时 | 人类验证需求 | 状态 |
|---|---|---|---|
| M0 | 0.4h（实测 0.25h） | macOS 多屏边触发回归 + 单屏零差异 | ✅ 已完成 |
| M1 | 1.8h | 单屏零差异回归（GUI 走一遍 config → activate → 触发边） | 待启动 |
| M2 | 3.5h | 三平台拔插测试 + BindingInvalid 链路 | 待启动 |
| M3 | 3.5h（含 3.4-3.6 M3 落地回归 bug 修复 ~2h：macOS + layer_shell + Windows backend + libei sanity 测试重构） | 双屏下 dropdown 选 monitor + L 形错位已知限制验证 + Linux 后端 + 旧 config 兼容 + macOS/Windows/Linux layer_shell 真双屏下修复 100% 边缘检测 miss / dropdown 无效果 | 进行中（含 3.4-3.6 修复） |
| M4 | 9h | 3 种排布下画布交互 + KDE/Sway/Hyprland 真机 + libei 降级 + 单屏降级 + 拔插动画 | 待启动 |
| **合计** | **~18h** | | |

> **校准系数**：M0 计划 2.3h → 实测 0.4h（约 1/6）。其余里程碑按 ~1/3 系数下调（原估时 25h → 现 16h）。M3 因 3.4-3.6 收尾回归 bug 修复（4 个 backend + libei sanity + 几何纯函数化）+2h。M4 因子边屏障合成器踩坑风险高，保留较多 buffer。

**M3 完成时 = 用户报告问题解决**（核心目标达成）
**M4 完成时 = 产品形态达标**（与 ShareMouse/Deskflow 同级 UX，libei 后端除外）

---

## 5. 关键风险与不确定性

1. **真机多屏验证每步都需要你**：AI 自己无法虚拟多屏跑回归。每个 M1/M2/M3/M4 里程碑的"人类准备"是必需人力投入，不是建议。
2. **Wayland 子边屏障**用 `set_margin` 推 1px 表面，理论上可行，但不同合成器（KDE/GNOME/Sway/Hyprland）对 sub-pixel margin 处理可能差异。M4 STEP 4.3/4.4 完成后需要在每种合成器上跑回归。
3. **macOS CGEventTap 权限**：每重启系统要重新授权。M0 之后的步骤碰到你需要重新授权。
4. **DPI / scale factor**：另一个独立 bug（Windows 后端完全没有 DPI 声明，mixed-DPI 必坏）。本计划不修。
5. **L1 的"接收端光标原地不动"事实**：本计划全程假设接收端不做绝对光标 warp（M5 才会加）。如果用户在 M3 完成后报"光标跳跃感"，需要追加 M5。

---

## 6. 后续（Out of Scope，本计划结束后再立 PLAN-M2）

| 里程碑 | 内容 | 估时 |
|---|---|---|
| M5 | 协议层 `ProtoEvent::Enter` 携带 offset 比例 + 接收端 `CGWarpMouseCursorPosition` / `SetCursorPos` / `XWarpPointer`；Wayland 上需协议能力位协商 + 合成器支持差异 | ~2.5h |
| M6 | 边沿停留 / 双推手势（macos timer + windows timer；layer_shell/libei 跳过） | ~1.5h |
| M7+ | 完整 2D 编辑器（被控端屏幕广播、布局求解、拖拽冲突解决） | 12–18h |

---

## 7. 执行约定

- 每步完成后由 LEADER 评审逻辑（不验证 build），执行者自验证 build + 单测。
- 遇到跨步影响的小问题记入 `next/SUGGESTION.md`，LEADER 决定搁置/删除/合并到本计划。
- 每完成一个里程碑：LEADER 提交 git（commit message 格式 `M<n>: <milestone name>`），并把对应 STEP 草图清理。
- M2 完成后（按 .EXECUTOR.md）与用户讨论是否需要调整后续里程碑。
- 超过 1 小时未完成的步骤，LEADER 介入重拆或重规划。

---

## 8. 测试矩阵：自动 vs 人类协助

> **核心原则**：几何 / 序列化 / 类型 / 编译 / 单测 → 自动；多屏真机物理行为、热插拔时序、合成器特定 bug、人眼 UX → 必须人类在真机多屏环境下手动跑。
>
> "自动"列里每一条**都必须**在合并前由 AI 跑通并贴日志；"人类"列里每一条**至少一次**由人类在真机记录结果（截图 + 一句话结论）。

### M0 — macOS 后端正确性 ✅

| 类型 | 测试项 | 通过标志 | 对应 STEP |
|---|---|---|---|
| 自动 | `cargo build -p input-capture` | 编译通过 | 0.1 |
| 自动 | windows/display_util 拆分后既有单测 | 仍绿 | 0.1 |
| 自动 | `cargo test -p input-capture` 全绿（含新增 geometry 单测） | L 形 / 2x1 / 3x1 / 不等高 case 全过 | 0.6 |
| 自动 | `cargo fmt --check` + `cargo clippy -p input-capture --all-targets -- -D warnings` | 无 diff / 无 warning | 0.7 |
| **人类** | macOS 单屏 top/bottom/left/right 行为不变 | smoke 通过 | 0.7 |
| **人类** | macOS 双屏 2x1 横排：top 从右屏顶部触发；左屏顶部不触发 `Right` 边 | 屏幕录像 / 截图佐证 | 0.3 / 0.4 / 0.5 |
| **人类** | macOS L 形错位 200px：错位区穿出时不误触相邻屏 | 手动 + 日志 | 0.6 |

### M1 — BarrierKey 重构

| 类型 | 测试项 | 通过标志 | 对应 STEP |
|---|---|---|---|
| 自动 | `cargo build --workspace` 全绿 | 0 编译错误 | 1.1-1.3 |
| 自动 | `src/capture_test.rs` 全绿 | 与重构前行为一致 | 1.3 |
| 自动 | `src/emulation_test.rs` 全绿 | 同上 | 1.3 |
| 自动 | 新增单测：单 handle/edge 行为不变；多 handle/edge 同 `BarrierKey` 下 broadcast；空集合 poll_next 重注册 waker（`tokio_test::assert_pending!` + 后续唤醒） | 全绿 | 1.1 |
| 自动 | `cargo clippy --workspace --all-targets` | 无 warning | 1.4 |
| **人类** | 单屏 GUI 完整走一遍：config → activate → 触发 top/bottom/left/right → release | 行为与重构前像素级一致 | 1.4 |

### M2 — 显示器枚举 + 热插拔

| 类型 | 测试项 | 通过标志 | 对应 STEP |
|---|---|---|---|
| 自动 | `cargo build -p input-capture` | 编译通过 | 2.5 |
| 自动 | `MonitorInfo` serde round-trip 单测（含 UTF-8 显示器名称、负坐标、不同 scale） | 全绿 | 2.1 |
| 自动 | `lan-mouse-ipc::FrontendEvent::MonitorsChanged` 反序列化单测 | 老 wire（无该字段）= 空 vec | 2.1 |
| 自动 | `cargo clippy -p input-capture -p lan-mouse-ipc` | 无 warning | 2.5-2.6 |
| 自动 | `src/service.rs` 单测：模拟 monitors 变化 → service 转发 `MonitorsChanged`（mock input_capture） | 全绿 | 2.6 |
| 自动 | `src/service.rs` 单测：STEP 2.6 reconcile 逻辑 — 模拟 monitor 移除 → active client 被 deactivate + 发 `BindingInvalid` | 全绿 | 2.6 |
| **人类** | macOS 双屏：拔插后 WebSocket console 看到 `MonitorsChanged`；拔插时 active capture 收到 `BindingInvalid`（若 client 绑在被拔的 monitor） | console log + UI 高亮 | 2.6 / 2.7 |
| **人类** | Windows 双屏：同上 | 同上 | 2.3 / 2.7 |
| **人类** | Linux Wayland (layer_shell)：拔插 `wl_output` global 后 monitors 列表更新 | weston / KScreen 辅助 + daemon log | 2.4 / 2.7 |
| **人类** | Linux GNOME Wayland (libei portal)：改变 zones 后 monitors 列表更新 | portal 文档 + daemon log | 2.4 / 2.7 |

### M3 — 显示器绑定 UI

| 类型 | 测试项 | 通过标志 | 对应 STEP |
|---|---|---|---|
| 自动 | `cargo build --workspace` | 编译通过 | 3.1-3.2 |
| 自动 | `cargo clippy --workspace --all-targets` | 无 warning | 3.3 |
| 自动 | `cd lan-mouse-vue && pnpm build` | 产物 OK | 3.3 |
| 自动 | Vue store 单测：`onMounted` mock `MonitorsChanged` → `state.monitors` 更新；`updateClientConfig` diff 检测（重复值不发请求） | 全绿 | 3.2 |
| 自动 | ConnectionRow snapshot test（vitest）：单 monitor / 多 monitor / 旧 config 三种 case | snapshot 一致 | 3.2 |
| 自动 | `lan-mouse-ipc::ClientConfig` 反序列化测试：缺 `monitor` 字段 = `None`（向后兼容） | 全绿 | 3.1 |
| **人类** | macOS 双屏：设 `top` 绑定右屏，光标在右屏顶部切到对端正确；切到左屏顶部不触发 | 屏幕录像 | 3.3 |
| **人类** | 旧 config（无 `monitor` 字段）加载后 dropdown 默认显示 "Any" 且行为不变 | config 文件对比 + smoke | 3.1 / 3.3 |
| **人类** | Linux Wayland (layer_shell) 后端跑一遍 dropdown | 与 macOS 一致 | 3.3 |
| **人类** | L 形错位排布下 dropdown 选择行为 + UI 显示"已知限制"提示 | 截图 + 一句话结论 | 3.2（PATCH 4） |
| 自动 | `cargo test -p input-capture::geometry crossed_pure_4x2` ：8 个 case（C1-C8 见 STEP-3.4）覆盖 prev_pos 在 display_0 / display_1 / 接缝 / 屏外 × active key `monitor: Some(d0) / Some(d1) / None`；每条断言 query key 的 `monitor` 字段非硬编码 None | 全绿（≥ 8 case） | 3.4 |
| 自动 | `cargo test -p input-capture::geometry build_display_bounds_pure`：构造 `active_ids + monitors` fixture → 断言 id 唯一、`display_containing` 拿到的 idx 对应的 `monitor_id` 一致、`Vec` 长度 = `active_ids.len()` | 全绿 | 3.4 |
| 自动 | `cargo test -p input-capture::layer_shell capture_create_preserves_monitor`（cfg gate）：构造 `state.active_positions` 注入完整 `BarrierKey { monitor: Some("wl-output-name"), pos: Top, ... }`，确认 `Capture::create` 后 `state.active_positions` 内的 key 含 `monitor: Some(...)`，未被剥字段 | 全绿 | 3.4 |
| 自动 | `cargo test -p input-capture::geometry activation_pure_4x2`（Windows `check_client_activation` 纯函数版）：6 个 case 覆盖 prev_pos 在 display_0 / display_1 / 屏外 × active key `monitor: Some("win:…") / None`；断言 query key 的 `monitor` 非硬编码 None | 全绿（≥ 6 case） | 3.5 |
| 自动 | `cargo test -p input-capture::libei select_barriers_with_monitor_field`（cfg gate）：构造 `regions = vec![(1920, 1080, 1920, 0)]`（横排 2x1 右屏）+ `clients = vec![BarrierKey { monitor: Some("region-1"), pos: Top, ... }, BarrierKey { monitor: None, pos: Top, ... }]` → 断言返回 `barriers.len() == 2` 且 `key_for_barrier` map 含两条 key | 全绿 | 3.5 |
| 自动 | `cargo test -p input-capture::dummy with_keys_preserves_monitor_offset_span`（已在 STEP-1.2 落地，`dummy.rs:180-190`）：3.5 复用既有单测，无需新增 | 仍绿 | 3.5（引用） |
| 自动 | `cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo test --workspace` | 全绿 | 3.6 |
| **人类** | macOS 真双屏（**修复 3.4 真机真解决**）：dropdown 选右屏绑 `top` → cursor 移到右屏顶部 → 真切到对端（主控 `capture.rs` 日志看到 `activated client 0` + `stream C dropped` + 对端 libei 收到 `Enter`）；cursor 移到左屏顶部 → 不切；记录 daemon log + 屏幕录像 | 录屏 + daemon log 关键三行 | 3.6 |
| **人类** | Windows 真双屏（**修复 3.5 真机真解决**）：同上行为（设 `top` 绑定右屏 → cursor 在右屏顶部切换；cursor 在左屏顶部不切） | 录屏 + daemon log | 3.6 |
| **人类** | Linux Wayland (layer_shell) 真机（**修复 3.4 同形 bug 真机真解决**）：dropdown 选右屏绑 `top` → cursor 在右屏顶部 → 触发对端切换（layer_shell 用 edge barrier，行为正确 = 「dropdown 生效」） | 录屏 + daemon log | 3.6 |
| **人类** | Linux GNOME Wayland (libei) 真机：libei backend 不受影响（libei 天然兼容，但 3.5 单测已锁住）；dropdown 选择与 3.3 同 | 与 macOS dropdown 行为一致 | 3.6 |
| **人类** | 旧 config（无 `monitor` 字段）+ 单 monitor 场景：行为不变（**回归保护**，确认 3.4-3.6 没有把 "monitor=None = 任意显示器" 的语义改坏） | config 文件对比 + smoke | 3.6 |

### M4 — 暴露边段 + 画布编辑器

| 类型 | 测试项 | 通过标志 | 对应 STEP |
|---|---|---|---|
| 自动 | `cargo test -p input-capture::geometry::hull`：`exposed_segments` 2x1 / 3x1 / L 形错位 200px / 不等高 / 镜像 共 ≥ 8 个 case 全绿 | 全绿 | 4.1 |
| 自动 | `hull::debug_print(displays)` 函数：给定 L 形输入，stdout segments 与手算结果一致（用 `assert_eq!` 包 snapshot test） | snapshot 稳定 | 4.2 |
| 自动 | 五个 backend `Capture::create` 接受完整 BarrierKey 的编译期单测：offset=5000, span=5000 / 0 / 10000 边界值 | 编译 + 编译期 assert | 4.3 |
| 自动 | libei backend 单测：传入 offset=5000 span=5000 时实际 barrier 仍为 monitor 全边（offset/span 被忽略，记录日志） | 全绿 + 日志断言 | 4.5 |
| 自动 | `FrontendEvent::LayoutChanged` serde round-trip 单测 | 全绿 | 4.6 |
| 自动 | `lan-mouse-vue` 单测：`state.segments` 由 `LayoutChanged` 更新；`updateClientConfig` 走 segment 路径算 BarrierKey | 全绿 | 4.9 |
| 自动 | ConnectionRow ↔ LayoutEditor 双向同步：选 ConnectionRow 的 monitor，画布对应 segment 高亮；反之亦然 | vitest 双向 case | 4.9 |
| 自动 | 单屏降级：`monitors.length === 1` 时 LayoutEditor 组件 `v-if` 不渲染，ConnectionRow 行为不变 | vitest + screenshot diff | 4.7 / 4.9 |
| 自动 | 热插拔动画：CSS transition 在拔插前后 `<rect>` x/y/width/height 平滑（视觉自动化难，但 transition 属性存在性可单测） | vitest 断言 CSS class | 4.9 |
| 自动 | `cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` + `pnpm build` | 全绿 | 4.10 |
| **人类** | macOS 双屏 2x1 横排：画布显示 2 个矩形，segments 高亮正确（top 一段、bottom 一段、左右各一段） | 截图 + 鼠标移动录屏 | 4.7 / 4.10 |
| **人类** | macOS L 形错位 200px：画布显示错位区被分割为多段；点 segment 挂 client；hover 显示已分配 client 名 | 截图 + 操作录屏 | 4.8 / 4.9 / 4.10 |
| **人类** | macOS 双屏不等高（1080p + 4K）：上下边各分两段 | 截图 | 4.7 |
| **人类** | 拔插显示器：画布 CSS transition 顺滑，无视觉跳变 | 录屏 | 4.9 |
| **人类** | Linux KDE Plasma 6 + layer_shell：4.3 真机 1px surface 可点击；offset=5000 span=5000 触发正确 | 录屏 + 日志 | 4.3 / 4.10 |
| **人类** | Linux Sway / Hyprland：4.4 真机测试；不支持 ≤N px 的合成器自动降级（前端 slider 隐藏） | 录屏 + UI 截图 | 4.4 |
| **人类** | Linux GNOME Wayland (libei)：画布可见但 ConnectionRow tooltip 显示"(portal 后端：monitor 粒度上限)"；offset/span slider 不渲染 | 截图 | 4.5 |
| **人类** | 单屏降级：拔掉外接显示器后画布消失，ConnectionRow 仍可用 | 录屏 | 4.7 |

### 测试工具与脚本建议

- **真机多屏验证**：每台机器准备一份 `tests/manual/<os>-<layout>.md` 模板（"在 macOS 14 + 两台 Studio Display 横排下：1. 启动 daemon；2. 打开浏览器 console；3. 移动鼠标到右屏顶部..."），人类按模板逐项打勾。
- **几何算法回归**：CI 跑 `cargo test -p input-capture::geometry::hull`；本地加 `hull::debug_print` 输出 PNG（用 `plotters` crate）便于人类肉眼对照。
- **WebSocket 事件录制**：开发期开 `RUST_LOG=lan_mouse_service=trace,lan_mouse_ipc=trace`，把 console 输出存到 `tests/manual/<date>-<machine>.log`，方便回溯。
- **截图对比**：M4 画布用 Playwright + headless Chromium 跑 vitest visual snapshot（在 CI 容器里 `monitors` mock 成固定数组）。

### 不可自动化 / 必须人为判断的项

1. **错位区 boundary 行为**（M3）：OS 报的是哪个 monitor 完全由 OS 决定，AI 只能 mock。
2. **合成器对 sub-pixel surface 的接受度**（M4）：每个 Wayland 合成器都是独立软件，无通用 API 查询。
3. **拔插时序**：USB-C / DisplayPort 协商延迟因硬件而异；连续拔插 5 次看是否漏事件只有人能观察。
4. **视觉 UX**：画布动画"是否顺滑"是主观判断。
5. **TCC 权限弹窗**：macOS Accessibility / Input Monitoring 每次重启都要重授权，必须人在机器前。
6. **跨 DPI 显示器**：混合 scale factor 行为不一致（计划声明不修，但需要人来确认症状以避免误诊为本计划问题）。