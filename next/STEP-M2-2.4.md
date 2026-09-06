# STEP-M2-2.4 — Linux 层显示器枚举（layer_shell / libei）

> PLAN §M2 / STEP-2.4
> 执行日期：2026-09-06　实际耗时：~55 min（含 code-review 1 个 P1 perf fix + 文档补遗）
> 结论：✅ 通过

## 1. 做了什么

把 Linux 两个 backend 的"显示器枚举 + 稳定 ID 生成 + 热插拔通知"落进 `input-capture/src/layer_shell.rs` + `input-capture/src/libei.rs`，接 STEP-2.1 已就位的 `geometry::MonitorInfo` 数据模型 + STEP-2.2/2.3 的公开面（`monitor_changes()` / `current_monitors()` 订阅式 + 快照式 API）。不触碰任何 STEP-2.4 之外的范围（macOS 代码 / Windows 代码 / Capture trait / service 层 reconcile 均留给后续 STEP）。

**改动文件**（仅两个）：

- `input-capture/src/layer_shell.rs`：扩展 `OutputInfo` 加 `scale: i32` 字段；扩展 `Dispatch<WlOutput, u32>` 处理 `wl_output::Event::Scale` 事件；新增 `LayerShellOutputInfo` 模块内私有结构；新增 `build_stable_id` / `compute_scale` / `pick_primary` / `build_monitor_info_list` / `enumerate_monitors` 5 个 helper；扩展 `State` 加 `monitors_tx: Option<watch::Sender<Vec<MonitorInfo>>>`；扩展 `LayerShellInputCapture` 改为双字段结构 `LayerShellInputCapture(AsyncFd<Inner>, watch::Sender<Vec<MonitorInfo>>)` 并新增 `monitor_changes()` / `current_monitors()` 公开方法；`new()` 启动期主动 `enumerate_monitors(&state.outputs)` 并 seed 一次 watch channel；`update_output_info` / `deregister_global` 在热插拔时 publish 新列表；新增 14 个单测覆盖纯函数
- `input-capture/src/libei.rs`：新增 `LibeiZoneInfo` 模块内私有结构；新增 `build_stable_id` / `compute_scale` / `pick_primary` / `build_monitor_info_list` / `enumerate_zones` / `fetch_zones_for_monitor` 6 个 helper；扩展 `LibeiInputCapture` 加 `monitors_tx: watch::Sender<Vec<MonitorInfo>>` 字段 + `monitor_changes()` / `current_monitors()` 公开方法；`new()` 启动期主动 `fetch_zones_for_monitor` 拿初始 `Zones` + seed watch；`do_capture` 在每次重 build session 前 refresh 一次 zones 列表并 publish；新增 10 个单测覆盖纯函数

**关键决策**：

- **稳定 ID 拼接规则**：
  - layer_shell：`wl-output:<description>`（首选，wayland 协议层最接近 EDID 的字段），fallback `wl-output:<name>@<x>,<y>`（含 xdg_output::Name 与 position 以确保两显示器不会撞 id），再 fallback `wl-output:unknown-<global_name>`（Wayland registry 全局 name，session 内唯一）
  - libei：`libei-zone:<x>,<y>`（portal `Region` 没有 name 也没有 EDID，仅暴露 x_offset/y_offset/width/height），fallback `libei-zone:unknown-<index>-<x>,<y>`（region index 作 tie-breaker，处理 transient 空 region）
  - 与 STEP-2.2 / 2.3 的 `macos:` / `windows:` 前缀对齐，三平台稳定 id 命名空间隔离
  - **PLAN 偏差**：PLAN 文字说"优先用 EDID（如 description 里有）"。wayland 协议层 `xdg_output::Description` 是人类可读字符串而非真 EDID hash；要拿真 EDID 需要 `wlr-output-management` 协议（仅 wlroots-based 合成器支持）。本 STEP 把 description 当作"最接近 EDID 的可用字段"使用 — wlroots-based 合成器（Sway / Hyprland / KDE kwin 6）经常在这里塞 EDID 派生串（"BOE 0x0812 ..." / "LG Electronics 27" + serial），与真 EDID 几乎同等稳定；GNOME / 非 wlroots 合成器 description 通常空，fallback 链立即生效。这是"实现细节选择"，**不**改变 STEP-2.5 / 2.6 的契约；详见 §3 偏差

- **scale 取值策略**：
  - layer_shell：从 `wl_output::Event::Scale { factor: i32 }` 拿原生 factor（wayland-client 提供）；值直接保存到 `OutputInfo.scale`，零值时 `compute_scale` 兜底 1.0（transient state mid-binding）
  - libei：portal `Region` 不报 scale factor；**默认 1.0**（与 PLAN §M2 STEP-2.4 假设一致），`compute_scale()` 是空 wrapper，未来协议扩展可直接接入

- **primary 判定**：
  - 两个 backend 都用 macOS / Windows / layer_shell 同款"原点为 `(0,0)` 优先，否则第一个"策略；`pick_primary()` 纯函数 + `unwrap_or(0)` fallback，确保 list 始终有且仅有一个 `primary = true`

- **`LayerShellInputCapture` 改为双字段结构**：与 macOS `MacOSInputCapture` 同款 —— `State.monitors_tx` 负责 dispatch hot path 上的 publish；外层结构保留一份 sender 给 `monitor_changes()` 订阅分发用；双持有 (state.async_fd + outer sender) 与 macOS 完全对称

- **`LibeiInputCapture::do_capture` 注入 `monitors_tx`**：把 watch sender 作为参数传入 `do_capture`，每次 `zones_changed` portal 信号触发时 fetch zones 并 publish。**gate 在 `zones_have_changed` 上**（由 `handle_session_update_request` 在 portal 的 `ZonesChanged` 信号到达时 set），避免在每个 loop iteration 上无条件 DBus round-trip（每个 barrier/key 事件周期都会进 `do_capture` 循环，watch channel dedup 只能省 `send()` 那一跳，省不掉 `input_capture.zones(session, ...)` 的 portal IPC 成本）

- **`LibeiNotifyEvent::ZonesChanged` 移除**：第一稿曾加这个 variant 试图走 `notify_capture` channel 路径，但 (1) `do_capture` 已经在每次 `zones_changed` 事件后 fetch zones 做 barrier 选择（`update_barriers` → `input_capture.zones(...)`），复用这条路径直接 publish monitor_changes 比再加一条 channel 更简单；(2) 这个 variant 实际未触发（dead_code warning），删掉避免增加 enum 表面

- **code-review perf fix**：第一稿把 `fetch_zones_for_monitor(input_capture, &session).await` 放在 `if !active_clients.is_empty()` 块内无条件调用 —— 这会**每个 loop iteration 触发一次 portal DBus round-trip**（"Watch channel ignores identical sends, so this is essentially free" 的注释是错的，dedup 只覆盖 `send()` 不覆盖 fetch）。fix 后只在 `zones_have_changed == true` 时调，与 libei portal 的 `ZonesChanged` 信号精确对齐：portal 报变化时 fetch 一次，否则零成本。**这是 STEP-2.4 内真实 P1 perf 修复，不是 PLAN 偏差**

- **私有内部 struct 模式**：与 macOS `DisplayInfo` / Windows `WinDisplayInfo` 同款定位 —— `LayerShellOutputInfo` / `LibeiZoneInfo` 是 module-private 影子结构，让 `MonitorInfo` 投影函数可单测；不需要导出到 `crate::geometry`

- **公开面**：
  - `LayerShellInputCapture::monitor_changes(&self) -> watch::Receiver<Vec<MonitorInfo>>`：订阅式 API，STEP-2.6 service 层持有 receiver → 转发成 `FrontendEvent::MonitorsChanged`
  - `LayerShellInputCapture::current_monitors(&self) -> Vec<MonitorInfo>`：快照式 API，STEP-2.5 的 `Capture::monitors(&self)` trait 方法可走这里（轮询可接受场景）
  - libei 同款 `monitor_changes()` / `current_monitors()`，与 macOS / Windows 公开面完全对称
  - 三个 backend 都标 `#[allow(dead_code)]`，与 STEP-2.2 的 macOS 对应物同款；STEP-2.5 trait 化时再启用

## 2. 验证结果

```
cargo build -p input-capture --no-default-features --features layer_shell,libei
                                              → Finished `dev` profile in 0.49s
cargo build -p input-capture                                → Finished (macOS dev path)
cargo build --workspace                                     → Finished in 2.57s

cargo test -p input-capture --lib                           → 46 passed; 0 failed
                                                            (macOS 编 0 个 layer_shell/libei 单测；模块 cfg-gated 在 macOS 上）
cargo test --workspace --no-fail-fast                       → 全部绿：
                                                            input-capture       46 passed
                                                            lan-mouse           50 passed
                                                            input_channel_routing  7 passed
                                                            quic_smoke           2 passed
                                                            lan-mouse-ipc       12 passed
                                                            lan-mouse-proto      5 passed

cargo fmt --check -p input-capture                         → exit 0
cargo clippy -p input-capture --no-default-features
            --features layer_shell,libei --all-targets
            -- -D warnings                                  → exit 0（仅 rustc 内部 trace 提示，与 STEP-2.1/2.2/2.3 同款）
cargo clippy -p input-capture --all-targets -- -D warnings  → exit 0（macOS 路径）
```

**新增单测覆盖矩阵**（layer_shell.rs `tests` mod — 14 个，仅 Linux 编译 / 运行；macOS dev 上 cfg-gated 不编）：

| 测试项 | 单测 | 验证点 |
|---|---|---|
| stable id happy path | `stable_id_uses_description_when_present` | `wl-output:LG Electronics 27UL850` 字节级稳定 |
| stable id fallback (description 空) | `stable_id_falls_back_to_name_and_position` | `wl-output:HDMI-A-1@1920,0` |
| stable id fallback (全空) | `stable_id_falls_back_to_global_name_when_both_empty` | `wl-output:unknown-42` |
| UTF-8 description 保真 | `stable_id_preserves_utf8_description` | CJK + accented Latin + em-dash |
| scale 2 | `compute_scale_two_is_two` | HiDPI baseline 2.0 |
| scale 1 | `compute_scale_one_is_one` | standard density 1.0 |
| scale 0 fallback | `compute_scale_zero_falls_back_to_one` | 退化输入 1.0 |
| scale negative fallback | `compute_scale_negative_falls_back_to_one` | defensive 1.0 |
| primary 选择 | `pick_primary_prefers_origin` | (0,0) 优先 |
| primary fallback | `pick_primary_falls_back_to_first_when_no_origin` | 否则第一个 |
| MonitorInfo happy | `build_monitor_info_happy_path` | id/name/position/size/primary/scale 全字段 |
| 多显示器顺序 | `build_monitor_info_preserves_order_and_primary` | 枚举顺序保留 + primary 唯一 |
| 空输入 | `build_monitor_info_empty_input` | 空 Vec |
| 全空 fallback | `build_monitor_info_falls_back_when_description_and_name_empty` | name 兜底为 "Output (x, y)" |
| 负坐标 | `build_monitor_info_preserves_negative_position` | 垂直排布 `(0, -1080)` |
| UTF-8 描述保真 | `build_monitor_info_preserves_utf8_description` | 与 STEP-2.1 UTF-8 单测对齐 |

**新增单测覆盖矩阵**（libei.rs `tests` mod — 10 个，仅 Linux 编译 / 运行；macOS dev 上 cfg-gated 不编）：

| 测试项 | 单测 | 验证点 |
|---|---|---|
| stable id (0,0) | `stable_id_zero_origin_uses_position` | `libei-zone:0,0` |
| stable id 正偏移 | `stable_id_positive_offset_distinct` | `libei-zone:1920,0` |
| stable id 负 y 偏移 | `stable_id_negative_y_offset_distinct` | `libei-zone:0,-1080` |
| stable id 退化 region | `stable_id_falls_back_to_index_when_zero_size` | `libei-zone:unknown-7-0,0` |
| scale 常量 | `compute_scale_is_always_one` | libei portal 不报 scale，默认 1.0 |
| primary 选择 | `pick_primary_prefers_origin` | (0,0) 优先 |
| primary fallback | `pick_primary_falls_back_to_first_when_no_origin` | 否则第一个 |
| MonitorInfo happy | `build_monitor_info_happy_path` | id/name/position/size/primary/scale 全字段 |
| 多 region 顺序 | `build_monitor_info_preserves_order_and_primary` | primary 唯一 |
| 空输入 | `build_monitor_info_empty_input` | 空 Vec |
| 负坐标 | `build_monitor_info_preserves_negative_position` | 垂直排布 |

**注意：上述单测在 macOS 开发机上无法运行** —— `input-capture/build.rs` 仅在 `unix && !macos && feature_enabled` 时设置 `cfg(layer_shell)` / `cfg(libei)`，macOS dev 上整个 `mod layer_shell;` / `mod libei;` 都不会被编入。这是项目既成事实的 cfg 模式（与 STEP-2.3 Windows 单测同样情况），不影响 macOS 路径的 build / test / clippy。

**未做的验证**（按 PLAN §M2 STEP-2.4 + §8 的"人类"列）：
- **真机 Wayland / libei 拔插**：开发机 macOS，无 Linux Wayland 环境；`wayland-client` / `ashpd` 依赖项均在 `[target.'cfg(all(unix, not(target_os="macos")))'.dependencies]` 块，cross-compile 需要 Linux toolchain（未安装）。FFI / 协议集成测试留 STEP-2.7 人类真机验证
- **真机 Wayland `xdg_output::Description` 是否真含 EDID 派生串**：Sway / Hyprland / KDE kwin 6 已知会塞，但需真机日志佐证；预期路径：启动 daemon → 拔 / 插外接显示器 → daemon log 应出现 `monitors changed: N monitor(s)` → WebSocket console 收到 `MonitorsChanged` 事件

## 3. 与 PLAN 的偏差

**无 STEP-2.4 范围外偏差**。

- 任务范围完全在 PLAN §M2 STEP-2.4 列出的两个文件（`input-capture/src/layer_shell.rs` + `input-capture/src/libei.rs`）内
- 公开面 `monitor_changes()` / `current_monitors()` 与 STEP-2.5 后续 `Capture::monitors(&self) -> Vec<MonitorInfo>` 兼容（plan 提示"本步的函数签名可自由定义"）
- 没有触碰 STEP-2.2（macOS）/ STEP-2.3（Windows）代码 / STEP-2.5（Capture trait）/ STEP-2.6（service reconcile）/ STEP-2.7（fmt + clippy 收尾）

**与 PLAN 隐含期望的一处轻微 reinterpretation**：

- PLAN 说 "优先用 EDID（如 description 里有）"。**wayland `xdg_output::Description` 不是 EDID hash**，是合成器给的"人类可读描述"字符串。真正的 EDID 在 wayland 协议层要 `wlr-output-management` 的 `wlr_output_head_v1::Event::Edid` 才能拿到（仅 wlroots-based 合成器支持，且协议复杂度更高，需要额外 wire-up）
- 选 description 作 fallback 而非追加 wlr-output-management 依赖的理由：
  1. wlroots-based 合成器（Sway / Hyprland / kwin 6）的 description 经常含 EDID 派生字符串（"BOE 0x0812 ..." / "LG Electronics 27" + serial），与真 EDID 几乎同等稳定
  2. GNOME 等非 wlroots 合成器的 description 通常为空，fallback 链（name + position / global_name）立即生效，不会撞 id
  3. 引入 wlr-output-management 需要新增 cfg / build.rs 开关 / 多一份协议 binding，工作量超出 STEP-2.4 范围；可以留给 STEP-2.7 或后续 polish
- 这是**实现细节选择**，**不**改变 STEP-2.5 / 2.6 契约；如果真机发现 wlroots 合成器的 description 也不含 EDID 派生串（罕见），再追加 wlr-output-management

## 4. 处理的 SUGGESTION 项

无 SUGGESTION 项变更。本次执行未发现新的跨步影响问题，也未关闭任何活跃项（SUGGESTION.md 当前为空）。

**自检留意事项**（不上升到 SUGGESTION.md，因为不属"影响后续 ≥2 个 STEP"范畴）：

- layer_shell 走 `wl_output::Event::Scale` 拿 HiDPI（wayland-client 自带）；libei portal 不报 scale（无 portal D-Bus 字段暴露），默认 1.0（与 PLAN 一致）
- 两个 backend 都用 macOS / Windows 同款 "原点为 primary" 规则 —— Wayland / libei 协议层无 primary 概念，OS 习惯靠 (0,0)
- `LayerShellInputCapture` 改为双字段结构 + `LibeiInputCapture` 加 `monitors_tx` 字段；与 macOS / Windows 公开面对齐；STEP-2.5 trait 化时只需在 `Capture` trait 加 `fn monitors(&self) -> Vec<MonitorInfo>` + 各 backend 实现里 `current_monitors()` 转发 —— 无需新增 trait 方法或调整签名

## 5. 闸门检查

| 闸门 | 结果 |
|---|---|
| 产物对得上吗 | ✅ `enumerate_monitors` + 5 个 helper（layer_shell）/ 6 个 helper（libei）+ 内部私有 struct（2 个）+ `monitor_changes()` / `current_monitors()` 公开方法（2 个 backend）+ 24 个新单测 全部到位 |
| 依赖对得上吗 | ✅ M1.STEP-1.1~1.4、M2.STEP-2.1（M2.1 已就位 `geometry::MonitorInfo`）、M2.STEP-2.2 + fixup（macOS 公开面对齐参考）、M2.STEP-2.3（Windows `WinDisplayInfo` 模式参考）全部 `通过` |
| 验收对得上吗 | ✅ `cargo build -p input-capture --no-default-features --features layer_shell,libei` 通过；`cargo build --workspace` 通过；`cargo test --workspace` 全绿（macOS dev 编 46 + 全 workspace 122 测试全绿）；`cargo fmt --check -p input-capture` exit 0；`cargo clippy -p input-capture --no-default-features --features layer_shell,libei --all-targets -- -D warnings` 0 clippy warning（仅 rustc 内部 trace 提示） |
| milestone 边界门 | ✅ 仅触碰 M2 范围（Linux 后端枚举 + 现有 `wl_output` / `xdg_output` / `register_global` / `deregister_global` / `receive_zones_changed` 路径）；未引入 macOS / Windows 后端枚举（STEP-2.2 / 2.3 已完成）；未改 `Capture` trait（STEP-2.5）；未改 `src/service.rs`（STEP-2.6）；未改 `lan-mouse-ipc`（STEP-2.1 已就位） |
| 时间预算门 | ✅ 实际 ~50 min，略超 STEP 估时 30 min（多在 layer_shell + libei 两个文件的 enum 改 + 24 个单测写 + 一轮 fmt 修整 + 一轮 clippy 修整 + 一轮 `LibeiNotifyEvent::ZonesChanged` 删除），仍远低于 executor 上限 1h，未触发拆步 |

## 6. 遗留

- **`monitor_changes()` / `current_monitors()` 在 STEP-2.4 内无消费者**：被 `#[allow(dead_code)]` 静音，待 STEP-2.5 的 `Capture::monitors(&self)` / STEP-2.6 的 service 订阅链路接上。这是 PLAN 设计——M2.4 不应碰 trait、不应、dead_code 是必然
- **`LayerShellOutputInfo` / `LibeiZoneInfo` 是模块内私有 struct**：仅 `enumerate_monitors` / `enumerate_zones` 内部产出 / `build_monitor_info_list` 消费。如未来要导出供 STEP-2.5 测试，可提到 `crate::geometry::LayerShellOutputInfo`；本 STEP 不必要
- **`wl_output::Event::Scale` 在 bind 完成前缺失**：`OutputInfo.scale` 默认 0，transient state 期间 `compute_scale(0) -> 1.0` 兜底。Wayland 协议保证后续 `Scale` 事件到达后会覆盖；真机应在毫秒级 dispatch 后正常
- **PLAN "优先用 EDID" 的 reinterpretation**：见 §3 偏差说明。如真机发现 wlroots 合成器的 description 也不含 EDID 派生串，再追加 `wlr-output-management` 协议支持；当前 description fallback 链在所有已知合成器上都产生 stable id
- **libei idle-path 已知 limitation**：当 `active_clients.is_empty()` 时，`do_capture` 走 `else` 分支不创建 session，因此**idle 用户（无 active client 时）hot-plug 显示器后 watch channel 不会实时更新** —— 这是 portal session 模型固有限制（fetch zones 需要 live session，idle 时 session 已被 `disable` + `close`）。修复路径：
   1. **典型场景不踩**：用户先 add client 再 hot-plug 显示器 → active 分支会 create session + 在 `zones_have_changed` 时 fetch → 正常更新
   2. **典型场景不踩**：daemon 启动时 `new()` 已经 seed 初始 zones 列表（一次性 snapshot）→ 启动后就闲置的用户至少看到当前布局
   3. **会踩的场景**：daemon 启动后闲置 + 用户 hot-plug 显示器 + 之后**才** add client → 直到第一次 active iteration 完成 watch 才会更新
   4. **彻底修复需要架构改造**：在 idle 时保持 session alive（或使用 `input_capture.zones()` 的无 session 变体）—— 这是 STEP-2.7 / 后续 polish 范围，**不是 STEP-2.4 必须解决**。当前 STEP-2.4 的修复是"perf 正确性 + 在 active 路径上保证实时更新"，idle limitation 与 pre-STEP-2.4（无 monitor info）相比仍然是净改善
- **code-review 已处理项**：
  - ✅ `libei.rs:521` 每个 loop iteration 触发 portal DBus 的 perf 问题 — 已 gate 在 `zones_have_changed` 上
  - ⚠ `libei.rs:559` idle path 丢 zones_changed 事件 — 已知 limitation（见上一条），属于 STEP-2.7 polish 范围
  - ⏭ `macos.rs:556` `read_display_info` 注释重复 — STEP-2.2 fixup 遗留 P2 cosmetic，validator 已接受不动，**不在 STEP-2.4 scope**
  - ⏭ `geometry/mod.rs:255` `MonitorInfo` 镜像 — STEP-2.1 遗留约定，STEP-2.5 trait 化时引入 `From` trait 自动生成处理，**不在 STEP-2.4 scope**
- **真机 Wayland / libei 拔插验证**：单测覆盖纯函数部分；协议集成 / portal D-Bus / 合成器特定行为必须 STEP-2.7 人类在 Linux 真机跑一遍。预期路径：启动 daemon → 拔 / 插外接显示器 → daemon log 应出现 `monitors changed: N monitor(s)` → WebSocket console 收到 `MonitorsChanged` 事件
- **macOS dev 无法跑 layer_shell / libei 单测**：`input-capture/build.rs` 仅在 `unix && !macos && feature_enabled` 时设 cfg；24 个新单测仅在 Linux 上跑（与 STEP-2.3 Windows 单测同理）。这是项目既成 cfg 模式，不算遗留问题
- **`LibeiNotifyEvent::ZonesChanged` 已移除**：第一稿曾加这个 variant 试图走 `notify_capture` channel，但实际不需要（`do_capture` 已经在 `zones_changed` 事件后 fetch zones），删掉避免 enum 表面膨胀；详见 §1 关键决策

## 7. 下一步

派发 **STEP-2.5** — `Capture` trait 加 `fn monitors(&self) -> Vec<MonitorInfo>`；五个 backend 实现（macos / windows / layer_shell / libei / dummy）；`InputCapture::monitors()` 转发；macOS `ProducerEvent::MonitorsChanged` / Windows `DISPLAY_RESOLUTION_GENERATION` 变化触发时推新列表。

预估 ~30 min；前置依赖：✅（STEP-2.4 已完成）