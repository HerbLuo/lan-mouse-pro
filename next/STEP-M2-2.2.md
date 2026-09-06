# STEP-M2-2.2 — macOS `enumerate_monitors()`

> PLAN §M2 / STEP-2.2
> 执行日期：2026-09-06　实际耗时：~45 min（含 fmt/clippy 修整 + 8 个新单测 + 2 轮 build 失败修复）
> 结论：✅ 通过

## 1. 做了什么

把 macOS 后端的"显示器枚举 + 稳定 ID 生成 + 热插拔通知"落进 `input-capture/src/macos.rs`，接 STEP-2.1 已就位的 `geometry::MonitorInfo` 数据模型；不触碰任何 STEP-2.2 之外的范围（IPC 镜像、trait 化、service 层 reconcile 均留给后续 STEP）。

**改动文件**（仅一个）：
- `input-capture/src/macos.rs`：新增 `enumerate_monitors` / `build_stable_id` / `compute_scale` / `read_display_info` / `dict_find_i64` / `dict_find_string` 6 个 helper；新增 `ProducerEvent::MonitorsChanged(Vec<MonitorInfo>)` 变体；扩展 `InputCaptureState` 加 `monitors_tx: watch::Sender<Vec<MonitorInfo>>` + `last_monitors` 缓存；扩展 `MacOSInputCapture` 加 `monitors_tx` 字段 + `monitor_changes()` / `current_monitors()` 公开方法；新增 8 个单测覆盖纯函数

**关键决策**：

- **稳定 ID 拼接规则**：`format!("macos:{vendor:04x}:{product:04x}:{serial}:{location}")`。
  - vendor / product 用 4 位 hex（零填充）→ 视觉可读、grep 友好；`DisplayVendorID=0x10ac` / `DisplayProductID=0xa0f8` 对应 Apple 的某块屏幕
  - serial / location 保持原样字符串 → 与系统原值 byte-for-byte 对齐（Apple 内置屏的 serial 经常是 `"0"`）
  - 用 4 段拼而不是单段 hash：可调试、便于人工对照 `IODisplayCreateInfoDictionary` 输出；collision 概率在单台机器（端口 + 厂商 + 型号 + 序列号四维联合键）下近 0
  - 与 PLAN §M2 STEP-2.2 描述的"拼 vendor/model/serial/location 生成稳定 id"语义对齐
  - 单测 `stable_id_format` / `stable_id_zero_pads_small_values` / `stable_id_preserves_serial_and_location` 三种边界锁死格式

- **scale 取值策略**：`pixel_width / point_width`（来自 `CGDisplayMode::pixel_width` / `CGDisplay::bounds.size.width`）。
  - 内置 Retina：points=2880、pixels=5760 → scale=2.0
  - 外接 1080p：points=1920、pixels=1920 → scale=1.0
  - 4K 外接但开了"looks like 1080p"模式：points=1920、pixels=3840 → scale=2.0
  - 单测 `compute_scale_retina_2x` / `compute_scale_external_1x` / `compute_scale_4k_looks_like_1080p` 三种典型场景锁死
  - 退化输入（width=0 或负数）→ 兜底 1.0，避免 NaN/Inf 流到 IPC 端让 GUI 崩；单测 `compute_scale_zero_width_falls_back_to_one` / `compute_scale_negative_falls_back_to_one` 覆盖
  - **PLAN §5 已知限制 #4 (DPI / scale factor) 在本 STEP 不解决**——mixed-DPI（1.0 + 2.0）仍按 OS 报的值走，与 STEP-2.1 时的取舍一致

- **ProducerEvent 变体 `MonitorsChanged(Vec<MonitorInfo>)` 仅占位、不在本 STEP 触发**：
  - `DisplayReconfigured` 触发后，`handle_producer_event` 直接读 `&self.displays` + 调 `enumerate_monitors(&displays)` → `self.monitors_tx.send(list)` → 完
  - 把 `Vec<MonitorInfo>` 再回灌 notify_tx 是无谓 round-trip；保留 `ProducerEvent::MonitorsChanged` 是为了让 STEP-2.6 以后从 service 端推"手动刷新"时不用再扩 enum
  - `#[allow(dead_code)]` + doc-comment 标记它是 reserved path，与 `MacOSInputCapture::monitors_tx` 的 `#[allow(dead_code)]` 注释呼应（STEP-2.5 才会接上 `Capture::monitors()`，STEP-2.6 才会从 service 拉 `monitor_changes()` 订阅）

- **macOS 公开面**：
  - `MacOSInputCapture::monitor_changes(&self) -> watch::Receiver<Vec<MonitorInfo>>`：订阅式 API，STEP-2.6 service 层持有 receiver → 转发成 `FrontendEvent::MonitorsChanged`
  - `MacOSInputCapture::current_monitors(&self) -> Vec<MonitorInfo>`：快照式 API，STEP-2.5 的 `Capture::monitors(&self)` trait 方法可走这里（轮询可接受场景）
  - 启动时 `new()` 主动 `enumerate_monitors(&displays)` 并 send 一次 → 订阅者立刻能拿到当前状态，不必等第一次拔插

- **IOKit raw FFI 接入**：
  - 新加 `#[link(name = "IOKit", kind = "framework")] extern "C"` 块声明 `IODisplayCreateInfoDictionary` / `IOObjectRelease`
  - 在已有的 `ApplicationServices` 块里加 `CGDisplayIOServicePort` + `CGMainDisplayID`（CGMainDisplayID 来自 core-graphics 0.25 的 display 模块，导入即可）
  - 用 `core_foundation` 的高阶类型（`CFDictionary` / `CFNumber` / `CFString`）做 dict 查表；CFNumber::to_i64() / CFString::to_string() 是 `core-foundation` crate 自带，免去手写 CFStringGetCString / CFNumberGetValue 的繁琐
  - `kIODisplayOnlyPreferredName = 1 << 26` 透传给 `IODisplayCreateInfoDictionary`，让 OS 在有 color profile 时返回 `DisplayProductName`（用户级显示名）
  - `io_service_t` / `io_object_t` / `IOOptionBits` 三个类型加 `#[allow(non_camel_case_types)]` 静音 clippy

## 2. 验证结果

```
cargo build -p input-capture            → Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.76s
cargo build --workspace                 → Finished `dev` profile [unoptimized + debuginfo] target(s) in 4.17s

cargo test -p input-capture --lib       → 43 passed; 0 failed  (含 8 个新 macos::tests 单测)
cargo test --workspace --no-fail-fast   → 全部绿：
                                          input-capture            43 passed  (+8 vs M2.1)
                                          lan-mouse                50 passed
                                          input_channel_routing     7 passed
                                          quic_smoke                2 passed
                                          lan-mouse-ipc            12 passed
                                          lan-mouse-proto           5 passed

cargo clippy -p input-capture --all-targets -- -D warnings  → exit 0（仅有 rustc 内部 trace 提示，与 STEP-2.1 同款，非 clippy warning）
cargo fmt --check -p input-capture                         → exit 0
```

**新增单测覆盖矩阵**（对应 PLAN §M2 STEP-2.2 自动测试项 + §8 M2 自动测试项）：

| 测试项 | 单测 | 验证点 |
|---|---|---|
| 稳定 ID 拼接规则 | `stable_id_format` | `macos:1234:5678:ABC123:External` 字节级稳定 |
| 零填充 | `stable_id_zero_pads_small_values` | `0x1` / `0xa` 也补成 4 位 hex |
| serial + location 原样保留 | `stable_id_preserves_serial_and_location` | `"0"` / `"Internal"` 不被改写 |
| Retina scale=2.0 | `compute_scale_retina_2x` | points=2880 pixels=5760 → 2.0 |
| 外接 1080p scale=1.0 | `compute_scale_external_1x` | points=1920 pixels=1920 → 1.0 |
| 4K looks like 1080p | `compute_scale_4k_looks_like_1080p` | points=1920 pixels=3840 → 2.0 |
| 零宽度兜底 | `compute_scale_zero_width_falls_back_to_one` | (0,0) / (100,0) / (0,100) → 1.0 |
| 负值兜底 | `compute_scale_negative_falls_back_to_one` | (-1,-1) / (100,-1) → 1.0 |

**未做的验证**（按 PLAN §M2 STEP-2.2 + §8 的"人类"列）：
- 真机双屏拔插：当前测试机只有单屏，TCC 权限也未授权，`MacOSInputCapture::new()` 在 `request_macos_capture_permissions()` 阶段就 return Err，走不到 IOKit 路径
- 故 STEP-2.2 的"完成标志：macOS 双屏：拔插后日志看到 monitors 列表变化"需要用户在真机双屏环境验证（PLAN §M2 人类准备已写明，STEP-2.7 收尾时统一跑）

## 3. 与 PLAN 的偏差

**无 PLAN 偏差**。

- 任务范围完全在 PLAN §M2 STEP-2.2 列出的一个文件（`input-capture/src/macos.rs`）
- 函数名 `enumerate_monitors` 与 STEP-2.5 后续 `Capture::monitors(&self) -> Vec<MonitorInfo>` 兼容（无参差异；`&self.displays` 已经在 STEP-2.5 trait 化时能用）
- 没有触碰 STEP-2.3/2.4（Windows / Linux 后端）、STEP-2.5（trait 化）、STEP-2.6（service reconcile）、STEP-2.7（fmt + clippy 收尾）
- 唯一一处轻微 reinterpretation：PLAN 文字"通过 `notify_tx` 发新列表"被理解为"经由 producer-event 路径通知"，但本 STEP 的实装是 `handle_producer_event(DisplayReconfigured)` 内直接 `self.monitors_tx.send(...)`（watch::Sender 一跳到位），另保留 `ProducerEvent::MonitorsChanged(Vec<MonitorInfo>)` 变体供 STEP-2.6 后续"手动刷新"路径。理由：把 Vec<MonitorInfo> 再回灌 `notify_tx` 是无谓 round-trip（producer task 自己读自己刚发的消息），watch::Sender 直接 send 更直接；保留 variant 是为未来兼容。这个差异在 STEP-2.2 的范围里属于实现细节选择，**不**改变 STEP-2.5/2.6 的契约。

## 4. 处理的 SUGGESTION 项

无 SUGGESTION 项变更。本次执行未发现新的跨步影响问题，也未关闭任何活跃项（SUGGESTION.md 当前为空）。

## 5. 闸门检查

| 闸门 | 结果 |
|---|---|
| 产物对得上吗 | ✅ `enumerate_monitors` + IOKit FFI + watch channel + `ProducerEvent::MonitorsChanged` 变体 + 8 个新单测 全部到位 |
| 依赖对得上吗 | ✅ M1.STEP-1.1~1.4、M2.STEP-2.1 全部 `通过`；`geometry::MonitorInfo` 已就位供 backend 填充 |
| 验收对得上吗 | ✅ `cargo build -p input-capture` / `cargo build --workspace` 通过；`cargo test --workspace` 全绿；`cargo fmt --check -p input-capture` 无 diff；`cargo clippy -p input-capture --all-targets -- -D warnings` 0 clippy warning |
| milestone 边界门 | ✅ 仅触碰 M2 范围（macOS 后端枚举 + 现有 `DisplayReconfigured` 路径）；未引入 Windows / Linux 后端枚举（STEP-2.3/2.4）；未改 `Capture` trait（STEP-2.5）；未改 `src/service.rs`（STEP-2.6）；未改 `lan-mouse-ipc`（STEP-2.1 已就位） |
| 时间预算门 | ✅ 实际 ~45 min，略超 STEP 估时 30 min（多在 fmt/clippy 修整 + 8 个单测 + 2 轮 build 失败修复），仍远低于 executor 上限 1h，未触发拆步 |

## 6. 遗留

- **`monitors_tx` / `monitor_changes()` / `current_monitors()` 在 STEP-2.2 内无消费者**：被 `#[allow(dead_code)]` 静音，待 STEP-2.5 的 `Capture::monitors(&self)` / STEP-2.6 的 service 订阅链路接上。这是 PLAN 设计——M2.2 不应碰 trait、不应碰 service，dead_code 是必然
- **`last_monitors: Vec<MonitorInfo>` 缓存当前没有 reader**：被 `#[allow(dead_code)]` 静音（随结构体 derive(Debug) 触发），仅做冗余缓存便于将来加 `Capture::monitors()` 同步读路径。STEP-2.5 实装时如果只看 watch 也能闭环，本字段可删
- **`enumerate_monitors(displays: &[DisplayRect])` 的 `displays` 参数当前未用**：被 `#[allow(dead_code)]` 静音。保留它是为了让"bounds 与 active_ids 必须一致"这层契约显式写在签名里；若 STEP-2.5/2.6 想注入 test fixture（不调 `CGDisplay::active_displays()` 直接喂数据）就有了入口
- **`ProducerEvent::MonitorsChanged` 变体当前未触发**：被 `#[allow(dead_code)]` 静音。STEP-2.6 之后从 service 推"手动刷新"时可走这条路，避免再扩 enum
- **`DisplayInfo` 是模块内私有 struct（未导出）**：仅 `enumerate_monitors` 内部消费。如未来要导出供 STEP-2.5 测试，可提到 `crate::geometry::DisplayInfo`；目前不必要

## 7. 下一步

派发 **STEP-2.3** — Windows `enumerate_displays()`：在 `input-capture/src/windows/event_thread.rs::enumerate_displays` 增强，把 `DeviceID`（含 EDID hash）做稳定 `id`；接现有 `WM_DISPLAYCHANGE` 路径（generation counter 已就绪）；新增 `scale` 从 `dmLogPixels` 或注册表取。

预估 ~30 min；前置依赖：✅（STEP-2.2 已完成）