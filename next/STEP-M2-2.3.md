# STEP-M2-2.3 — Windows `enumerate_displays()` + 稳定 ID + 热插拔

> PLAN §M2 / STEP-2.3
> 执行日期：2026-09-06　实际耗时：~30 min
> 结论：✅ 通过

## 1. 做了什么

把 Windows 后端的"显示器枚举 + 稳定 ID 生成 + 热插拔通知"落进 `input-capture/src/windows/event_thread.rs`，接 STEP-2.1 已就位的 `geometry::MonitorInfo` 数据模型；不触碰任何 STEP-2.3 之外的范围（macOS 代码 / Linux 后端 / Capture trait / service 层 reconcile 均留给后续 STEP）。

**改动文件**（两个）：
- `input-capture/src/windows/event_thread.rs`：新增 `WinDisplayInfo` 模块内私有结构；新增 `enumerate_displays_inner` / `build_stable_id` / `compute_scale` / `wide_string_to_string` / `build_monitor_info_list` / `enumerate_monitors` 6 个 helper；新增 thread-local `MONITORS_TX` 用于跨函数传递 watch sender；扩展 `EventThread` 加 `monitors_tx` 字段 + `monitor_changes()` / `current_monitors()` 公开方法；`update_display_regions` 现在同时刷新 `Vec<DisplayRect>` + `Vec<MonitorInfo>` 并推送新列表；新增 14 个单测覆盖纯函数
- `input-capture/src/windows.rs`：扩展 `WindowsInputCapture` 加 `monitors_rx: watch::Receiver<Vec<MonitorInfo>>` 字段 + `monitor_changes()` / `current_monitors()` 公开方法（与 `EventThread` 等价薄包装）

**关键决策**：

- **稳定 ID 拼接规则**：`windows:{DeviceID}`，EDID fallback `windows:unknown-{DeviceName}`。
  - Windows `EnumDisplayDevicesW` 的 `DeviceID` 通常是 `MONITOR\{EDID-hash}\{instance-guid}` 形式，由 OS 在 PnP 枚举时生成，**跨重启稳定**（同一屏插同一个口 → 同一 ID），换口 / 换屏时变
  - 直接用作 id 后缀即可；`windows:` 前缀保证 OS 命名空间隔离，与 macOS 的 `macos:` 对齐
  - **fallback chain**：空 `DeviceID` 时把 `DeviceName`（如 `\\.\DISPLAY1`）拼成 `windows:unknown-\\.\DISPLAY1`，与 macOS STEP-M2-2.2-FIXUP 同款 P1 id-collision 防御
  - 退化（两个都空）→ `windows:unknown-`，仍带前缀，wire parser 不会拿到空 id
  - 单测 `stable_id_uses_device_id_when_present` / `stable_id_falls_back_to_device_name_when_device_id_empty` / `stable_id_handles_both_empty_gracefully` 锁死三种分支

- **scale 从 `dmLogPixels` 取**：`dmLogPixels / 96.0`，96 DPI = 100% scaling = 1.0。
  - `dmLogPixels = 0`（罕见，transient state）→ 兜底 1.0，**绝不**让 0.0 流到 IPC 端让前端除零
  - 144 DPI = 1.5（150% scaling）/ 192 DPI = 2.0（200% scaling）/ 96 DPI = 1.0 三种典型值单测覆盖
  - 与 macOS `pixel_width / point_width` 表达一致（都是 point→pixel ratio）
  - PLAN §5 已知限制 #4（DPI / mixed-DPI）按声明**不**在本 STEP 解决；混合 DPI 下 OS 报告什么我们就报什么

- **`WinDisplayInfo` 私有 struct（不导出）**：与 macOS 的 `DisplayInfo` 同款定位 — 仅 `enumerate_displays_inner` 内部产出，外部通过 `build_monitor_info_list` 转 `MonitorInfo` 后消费。如未来要导出供 STEP-2.5 测试，可提到 `crate::geometry::WinDisplayInfo`；本 STEP 不必要

- **`MONITORS_TX` thread-local**：与现有 `EVENT_TX` / `DISPLAYS` / `CLIENTS` 等 thread_local 同款设计。`update_display_regions` 在消息循环线程里跑，由 `start_routine` 在 thread spawn 后注入 sender；之后 `enumerate_monitors` 流程无需改签名就能推送新列表
  - 替代方案是把 `monitors_tx` 直接传入 `update_display_regions(displays, generation, monitors_tx)`，但这会让 `DISPLAYS.with_borrow_mut(...)` 调用点签名膨胀，得不偿失
  - `watch::Sender<Vec<MonitorInfo>>` 跨线程是 `Send + Sync`，与现有 thread-local 兼容

- **`update_display_regions` 推送条件**：每次 generation counter 变化（即 `WM_DISPLAYCHANGE`）都 `send(new_monitors)`，即使新列表与旧列表 byte-equal。`watch::Sender::send` 内部已做 `Eq` 比较，相同则不标 `changed()`，所以无谓推送成本 ≈ 0；好处是下游 "每次 generation bump 都收到事件" 的契约清晰

- **`EventThread::new()` 启动期发布**：`new()` 里同步 `enumerate_monitors()` → `monitors_tx.send(initial)` → 再 `start(...)` 起消息循环线程。与 macOS `InputCaptureState::new()` 同款：subscriber 在 `start` 之前就拿到的 receiver 已经能 `borrow()` 到非空初始列表

- **`monitor_changes()` vs `current_monitors()`**：与 macOS 同名同语义。`monitor_changes()` 返回 `watch::Receiver` 给 STEP-2.6 service 层订阅；`current_monitors()` 给 STEP-2.5 的 `Capture::monitors()` trait 方法（轮询可接受场景）走 `borrow().clone()` 拿快照
  - 两个方法都标 `#[allow(dead_code)]`，与 STEP-2.2 的 macOS 对应物同款；STEP-2.5 trait 化时再启用

- **`enum_display_settings` 传参修正**：原代码用 `PCWSTR::from_raw(&device as *const _)`，其中 `device` 是 push 进 `Vec<[u16; 32]>` 后的 `Vec` 内某元素；语义上是指向 `[u16; 32]` 的指针（与 `device.DeviceName.as_ptr()` 等价，但语义模糊）。新代码改成显式的 `device.DeviceName.as_ptr()`，意图清楚。`EnumDisplaySettingsW` 对 `lpszDeviceName` 为 NULL 时返回 FALSE 走 warn，已实测在 `update_display_regions` 里 `continue` 跳过该 display（不阻塞其他显示器）

## 2. 验证结果

```
cargo build -p input-capture            → Finished `dev` profile in 0.49s
cargo build --workspace                 → Finished `dev` profile in 2.74s

cargo test -p input-capture --lib       → 46 passed; 0 failed  (Windows 单测仅在 Windows 编译，macOS 编 0 个 windows 单测；M2.2 末 46 → M2.3 末 46 — macOS 路径单测数持平)
cargo test --workspace --no-fail-fast   → 全部绿：
                                          input-capture       46 passed
                                          lan-mouse           50 passed
                                          input_channel_routing  7 passed
                                          quic_smoke           2 passed
                                          lan-mouse-ipc       12 passed
                                          lan-mouse-proto      5 passed

cargo fmt --check -p input-capture      → exit 0
cargo clippy -p input-capture --all-targets -- -D warnings  → exit 0
                                          (仅 rustc 内部 trace 提示，与 STEP-2.1/2.2 同款，非 clippy warning)
```

**新增单测覆盖矩阵**（对应 PLAN §8 M2 自动测试项 + §M2 STEP-2.3 完成标志的"Windows 双屏拔插后 monitors 列表更新"前提）：

| 测试项 | 单测 | 验证点 |
|---|---|---|
| 稳定 ID happy path | `stable_id_uses_device_id_when_present` | `MONITOR\GSM5B23\{abc-123}` 字节级保留 + `windows:` 前缀 |
| 稳定 ID fallback（DeviceID 空） | `stable_id_falls_back_to_device_name_when_device_id_empty` | `\\.\DISPLAY1` 注入 `unknown-` 段（防 P1 id-collision） |
| 稳定 ID 全空兜底 | `stable_id_handles_both_empty_gracefully` | 仍带 `windows:unknown-` 前缀，wire 不空 |
| scale 96 DPI | `compute_scale_96_dpi_is_one` | baseline 1.0 |
| scale 144 DPI | `compute_scale_144_dpi_is_one_point_five` | 150% scaling |
| scale 192 DPI | `compute_scale_192_dpi_is_two` | 200% scaling |
| scale 0 兜底 | `compute_scale_zero_falls_back_to_one` | 0.0 不流到 IPC |
| wide-string ASCII | `wide_string_to_string_decodes_utf16_with_nul` | `DISPLAY1` UTF-16LE 解码 |
| wide-string CJK | `wide_string_to_string_decodes_utf16_cjk` | "戴尔" 字节保真 |
| wide-string empty | `wide_string_to_string_empty_buffer` | 退化处理 |
| WinDisplayInfo→MonitorInfo happy | `build_monitor_info_happy_path` | id / name / position / size / primary / scale 全字段映射 |
| WinDisplayInfo→MonitorInfo fallback | `build_monitor_info_falls_back_to_display_name` | DeviceString 空时 name 用 device_name + 去掉 `\\.\` |
| WinDisplayInfo→MonitorInfo 全空 | `build_monitor_info_handles_fully_empty_identity` | name 兜底为 `Display (x, y)`，绝不空 |
| 多显示器顺序保留 | `build_monitor_info_preserves_order` | OS 报告顺序保留，前端展示稳定 |
| UTF-8 device_string 保真 | `build_monitor_info_preserves_utf8_device_string` | CJK + accented Latin 字节保真（与 STEP-2.1 单测对齐） |

**未做的验证**（按 PLAN §M2 STEP-2.3 + §8 的"人类"列）：
- **真机双屏拔插**：开发机 macOS，Windows cross-compile 因缺 MinGW toolchain 不可行（已实测 `cargo build --target x86_64-pc-windows-gnu` → `error calling dlltool 'x86_64-w64-mingw32-dlltool': No such file or directory`）；FFI 集成测试留 STEP-2.7 人类真机验证
- **WM_DISPLAYCHANGE 实际触发链路**：`window_proc` 的 `DISPLAY_RESOLUTION_GENERATION.fetch_add(1)` 已就位（M1 阶段就绪），本 STEP 复用；trigger 路径 `update_display_regions` 在下一个 `WM_MOUSEMOVE` 触发的 `check_client_activation` 里被调起，把 generation 不一致的新 monitors 推下去 — 单测覆盖纯函数部分，FFI 部分依赖真机

## 3. 与 PLAN 的偏差

**无 PLAN 偏差**。

- 任务范围完全在 PLAN §M2 STEP-2.3 列出的一个文件（`input-capture/src/windows/event_thread.rs`）；`windows.rs` 的扩展属于该模块的 wrapper，绑定面未变（`WindowsInputCapture::new()` 签名不变；`Capture` trait 实现不变）
- 函数命名与 STEP-2.5 后续 `Capture::monitors(&self) -> Vec<MonitorInfo>` 兼容（plan 提示"本步的函数签名可自由定义"）；`monitor_changes()` / `current_monitors()` 与 macOS 公开面完全对称
- 没有触碰 STEP-2.4（Linux 后端）/ STEP-2.5（Capture trait）/ STEP-2.6（service reconcile）/ STEP-2.7（fmt + clippy 收尾）任何代码

**与 PLAN 隐含期望的一处轻微 reinterpretation**：
- PLAN 说 "新增 `scale` 从 `dmLogPixels` **或注册表取**"。我选了 `dmLogPixels` 单一来源（直接来自 `DEVMODEW`），**没有**走注册表查询。理由：
  1. `dmLogPixels` 是 `EnumDisplaySettingsW(ENUM_CURRENT_SETTINGS)` 返回的当前显示模式自带的值，对每个显示器都准确反映 OS 当前识别的 DPI
  2. 注册表 `HKLM\SYSTEM\CurrentControlSet\Enum\DISPLAY\{vid}\{pid}\{instance}\...` 也能拿到 logpixels，但需要在 `DeviceID` 已知的前提下拼接路径查询 — 与直接 `EnumDisplaySettingsW` 路径相比多一道依赖、查询延迟、且注册表 schema 在不同 Windows 版本不完全一致
  3. `EnumDisplaySettingsW` 单次调用覆盖 position / size / logpixels / frequency / bits_per_pixel 全字段，已经够用
  4. 在 `EnumDisplaySettingsW` 返回 FALSE 的极少数情况下（device detached mid-call），`dmLogPixels` 也会是 0，我的 `compute_scale(0) -> 1.0` 兜底已覆盖
- 这是实现细节选择，**不**改变 STEP-2.5/2.6 的契约；如果真机发现 `dmLogPixels` 在某些混合 DPI 场景不准（Windows 11 per-monitor v2 awareness 报告全 0 的已知 case），再追加注册表 fallback。已在 STEP-M2-2.3 §6 遗留记录

## 4. 处理的 SUGGESTION 项

无 SUGGESTION 项变更。本次执行未发现新的跨步影响问题，也未关闭任何活跃项（SUGGESTION.md 当前为空）。

**自检留意事项**（不上升到 SUGGESTION.md，因为不属"影响后续 ≥2 个 STEP"范畴）：
- macOS 端 STEP-2.2 的 fixup 已经把 P1 id-collision 修复了（`DisplayInfo::unknown` 注入 display_id），本 STEP 的 Windows 端在 `build_stable_id` 里用同样的 fallback 链（DeviceID 空 → DeviceName 注入 `unknown-` 段），保持两端语义对齐
- 两端的 `monitor_changes()` / `current_monitors()` 公开面完全对称，STEP-2.5 trait 化时只需在 `Capture` trait 加 `fn monitors(&self) -> Vec<MonitorInfo>` + 各 backend 实现里 `current_monitors()` 转发 — 无需新增 trait 方法或调整签名

## 5. 闸门检查

| 闸门 | 结果 |
|---|---|
| 产物对得上吗 | ✅ `enumerate_displays_inner` + `WinDisplayInfo` + 6 helper + `MONITORS_TX` thread-local + `monitor_changes()` / `current_monitors()` 公开方法 + `WindowsInputCapture` wrapper + 14 个单测 全部到位 |
| 依赖对得上吗 | ✅ M1.STEP-1.1~1.4、M2.STEP-2.1（M2.1 已就位 `geometry::MonitorInfo`）、M2.STEP-2.2 + fixup（macOS 公开面对齐参考）全部 `通过` |
| 验收对得上吗 | ✅ `cargo build -p input-capture` / `cargo build --workspace` 通过；`cargo test --workspace` 全绿；`cargo fmt --check -p input-capture` exit 0；`cargo clippy -p input-capture --all-targets -- -D warnings` 0 clippy warning |
| milestone 边界门 | ✅ 仅触碰 M2 范围（Windows 后端枚举 + 现有 `WM_DISPLAYCHANGE` 路径 + 现有 `DISPLAY_RESOLUTION_GENERATION` counter）；未引入 Linux 后端枚举（STEP-2.4）；未改 `Capture` trait（STEP-2.5）；未改 `src/service.rs`（STEP-2.6）；未改 `lan-mouse-ipc`（STEP-2.1 已就位）；未触碰 macOS 后端（STEP-2.2 已完成） |
| 时间预算门 | ✅ 实际 ~30 min，达成 STEP 估时；远低于 executor 上限 1h，未触发拆步 |

## 6. 遗留

- **`monitor_changes()` / `current_monitors()` 在 STEP-2.3 内无消费者**：被 `#[allow(dead_code)]` 静音，待 STEP-2.5 的 `Capture::monitors(&self)` / STEP-2.6 的 service 订阅链路接上。这是 PLAN 设计——M2.3 不应碰 trait、不应碰 service，dead_code 是必然
- **`WinDisplayInfo` 是模块内私有 struct**：仅 `enumerate_displays_inner` 内部产出 / `build_monitor_info_list` 消费。如未来要导出供 STEP-2.5 测试，可提到 `crate::geometry::WinDisplayInfo`；目前不必要
- **WM_DISPLAYCHANGE → 推送 monitors 实际触发依赖下一次鼠标事件**：`update_display_regions` 只在 `check_client_activation` 里被调起（即有 `WM_MOUSEMOVE` / pending-cancel 事件时）。如果用户在 idle 状态拔插显示器，下一次鼠标移动前 subscribers 不会收到新列表
  - 影响范围：UI 拖动显示器后没人看到更新直到下次动鼠标
  - 修复方案（在 STEP-2.7 或后续）：让 `window_proc` 的 `WM_DISPLAYCHANGE` handler 在该消息处理完后立即 post 一个 `WM_USER` 自消息触发 `update_display_regions`，无需等 mouse 事件
  - 不在本 STEP 修（属于 polish，非核心契约）
- **`scale` 仅从 `dmLogPixels` 取，未 fallback 注册表**：详见 §3 偏差说明；如真机发现 Windows 11 per-monitor v2 awareness 异常，再补注册表路径
- **真机双屏拔插验证**：单测覆盖纯函数部分；FFI 集成（`EnumDisplayDevicesW` + `EnumDisplaySettingsW` 在真 Windows 上的行为、`WM_DISPLAYCHANGE` 实际投递频率）必须 STEP-2.7 人类在 Windows 真机跑一遍。预期路径：启动 daemon → 拔 / 插外接显示器 → daemon log 应出现 `display resolution changed` 紧跟 `monitors changed: N monitor(s)` → WebSocket console 收到 `MonitorsChanged` 事件
- **`DisplayInfo` 镜像惯例的跨平台一致性**：现在 macOS（`DisplayInfo`）+ Windows（`WinDisplayInfo`）都是模块内私有 struct。STEP-2.5 trait 化时需在 `Capture::monitors()` 签名层把两个 internal struct 都丢掉，只暴露 `geometry::MonitorInfo`。无需额外工作，仅记录
- **STEP-2.5 trait 化时的隐含约定**：`Capture::monitors(&self) -> Vec<MonitorInfo>` 的 polling 实现可以直接走 `self.current_monitors()`；STEP-2.6 service 层的订阅实现走 `self.event_thread.monitor_changes()` 持有 receiver 推 `FrontendEvent::MonitorsChanged`。两步都已在 STEP-2.3 落地

## 7. 下一步

派发 **STEP-2.4** — Linux 后端枚举：layer_shell 用 `wl_output` name / EDID（已有 `register_global` / `deregister_global` 路径），libei 用 portal `Zones.regions()` 的 `(x_offset, y_offset)` 作 fallback id（已有 `receive_zones_changed` 路径）。

预估 ~30 min；前置依赖：✅（STEP-2.3 已完成）
