# Validation: M2 STEP 2.3 + 2.4 + 2.5

> 审阅日期：2026-09-06　审阅 STEP 范围：2.3 + 2.4 + 2.5（自上次 fixup validator 以来的累计）
> 起点 commit：`1bfaa45`　终点 commit：`74949d4`
> 起点 hash：`1bfaa45 fix(macos): include display_id in stable id when IOKit unavailable`
> 终点 hash：`74949d4 feat(input-capture): expose Capture::monitors snapshot across backends`

---

## Verdict

**⚠ 返工**（1 个 P1 必修 BUG + 多个 P2 cosmetic followup）

---

## 1. 偏离 PLAN

### STEP-2.3（Windows `enumerate_displays()`）

- ✅ **完全符合 PLAN §M2 STEP-2.3**：
  - 用 `EnumDisplayDevicesW` + `EnumDisplaySettingsW` 路径 ✓
  - `DeviceID` 做稳定 id（带 `windows:` 前缀）✓
  - 接现有 `WM_DISPLAYCHANGE` 路径（`DISPLAY_RESOLUTION_GENERATION` counter）✓
  - `scale` 从 `dmLogPixels` 取（PLAN 文字"或注册表"，实装选 `dmLogPixels` 单源，详见 STEP §3）✓
  - 镜像 macOS 公开面 `monitor_changes()` / `current_monitors()` ✓
- ⚠ **PLAN 轻微 reinterpretation**：PLAN 说 "从 `dmLogPixels` **或注册表取**"，实装只走 `dmLogPixels` 单源。理由记录在 STEP-M2-2.3 §3（注册表查询路径多一道依赖、查询延迟、schema 不一致）。判 P2 文档准确性偏差，**接受**。
- ✅ milestone 边界门：未触碰 macOS / Linux 后端（STEP-2.2 / 2.4 已完成）；未改 `Capture` trait（STEP-2.5 已就位）；未改 `src/service.rs`（STEP-2.6）；未改 `lan-mouse-ipc`（STEP-2.1 已就位）。

### STEP-2.4（Linux layer_shell + libei）

- ✅ **layer_shell 完全符合 PLAN**：
  - 复用 `wl_output::Event::Scale` 拿 scale ✓
  - 优先 `xdg_output::Description`（作为 EDID-like），fallback `name@<x>,<y>`，再 fallback `global_name` ✓
  - 接现有 `register_global` / `deregister_global` 路径 ✓（通过 `Dispatch<WlOutput, u32>` 处理 `wl_output::Event::Done` → `update_output_info` publish 新列表）
- ✅ **libei 命名空间 + 字段选择符合 PLAN**：用 `x_offset,y_offset` 作 id（`libei-zone:` 前缀）✓；portal 不报 scale，默认 1.0 ✓
- ⚠ **PLAN 偏差（reinterpretation）**：PLAN 说 "优先用 EDID（如 description 里有）"。**wayland `xdg_output::Description` 不是 EDID hash**，是合成器给的人类可读字符串。真正的 EDID 需要 `wlr-output-management` 协议，超 STEP-2.4 scope。STEP §3 已记录，**接受**。
- ❌ **P1 BUG（必修，详见 §3）**：`libei.rs::do_capture` 中 `if zones_have_changed` gate 在**错误位置**——在 `handle_session_update_request` future 被 poll **之前**读取，导致 gate 永远为 `false`，portal DBus round-trip **永不触发**（详见 §3 BUG #1）。这破坏 STEP-2.4 对 libei 后端的核心契约——hot-plug 后 watch channel 更新。executor 报告称 "已 gate 在 `zones_have_changed` 上" 的 P1 perf fix 实装有 timing 错误，需要返工。
- ✅ milestone 边界门：未触碰 macOS / Windows / Capture trait / service.rs / lan-mouse-ipc。

### STEP-2.5（Capture trait monitors）

- ✅ **完全符合 PLAN §M2 STEP-2.5**：
  - `Capture` trait 加 `fn monitors(&self) -> Vec<MonitorInfo>`，默认返回空 ✓
  - 五个 backend 显式实现（macos / windows / layer_shell / libei / dummy）✓
  - `InputCapture::monitors()` 转发到 trait object ✓
  - 镜像导出 `pub use geometry::MonitorInfo` ✓
- ✅ milestone 边界门：未触碰各 backend 私有枚举函数（STEP-2.2/2.3/2.4 已就位）；未改 hot-plug 触发逻辑；未改 `src/service.rs`（STEP-2.6）；未 bump `lan-mouse-proto`。

---

## 2. 偏离 REQUIREMENT

- ✅ **未破坏**：
  - `REQUIREMENT.md §3.1-3.4`（QUIC + 剪贴板）未触碰。
  - `REQUIREMENT.md §5.2`（多屏定位）逐步推进——本批三个 STEP 完成了显示器枚举 + 稳定 ID + Capture trait 整合；binding UI 留给 M3 STEP-3.x。
  - 本批不 bump `lan-mouse-proto`（与 REQUIREMENT §5 末尾 + PLAN §1 一致）。
- ⚠ **libei hot-plug 实际不工作**（详见 §3 BUG #1）：在 active client + hot-plug 路径上 watch channel 不更新——这意味着 STEP-2.6 的 service 层不会收到 `MonitorsChanged` 事件，UI 不会刷新。该路径对最终用户体验（拔插显示器）有功能影响，属 BUG 而非 REQUIREMENT 偏离。

---

## 3. BUG 清单

| 严重度 | 位置 | 现象 | 建议修复 |
|---|---|---|---|
| **P1** | `input-capture/src/libei.rs:538`（`if zones_have_changed`） | **gate 位置错误**：check 发生在 `handle_session_update_request` future 被 poll **之前**。`zones_have_changed` 在循环顶部（line 483）被重置为 `false`，直到 `tokio::join!(capture_session, handle_session_update_request)` 在 line 575 才真正开始 poll 这个 future 并可能在 line 496 设 `zones_have_changed = true`。但 gate check 在 line 538 早已读完当时的 `false` 值。结果：libei backend 在 active client + hot-plug 路径上**永不**触发 portal DBus fetch + `monitors_tx.send(...)`，watch channel 只在 `new()` 启动期 seed 一次，之后永远不更新。这破坏了 STEP-2.4 对 libei 的核心契约。 | 把 `if zones_have_changed { fetch_zones_for_monitor(...) }` 整块从 line 538-563 **移到 `tokio::join!` 之后**（line 575 之后 + line 593 `if let Some(event) = capture_event_occured.take()` 之前）。在 join 完成后 future 已经运行完毕，`zones_have_changed` 才是正确的值。comment 里 "Documented in STEP-M2-2.4 §6 遗留" 提到的 idle limitation 仍存在（idle 分支 `else` 走 `handle_session_update_request.await`，但不 fetch），与本 BUG 独立。 |
| P2 | `input-capture/src/libei.rs:529-537`（comment 与代码不符） | comment 描述 "Idle users who hot-plug monitors before activating a client will see the watch channel update on the next active iteration (when a fresh session is created and the fetch runs again)"，但 fetch 实际**永不在 active iteration 触发**（因 P1 BUG）。 | 修 P1 后此 comment 自然正确。如 P1 不修，comment 至少需加一句"但因 gate 位置错误实际不触发，详见 validator 报告" |
| P2 | `input-capture/src/macos.rs:556-581`（`read_display_info` doc 注释重复） | STEP-2.2 fixup validator 已记入 P2 cosmetic backlog，确认未在本批触碰。**保持原状**——不在本 STEP scope 内清理。 | 留给后续 micro-cleanup PR |
| P2 | `input-capture/src/geometry/mod.rs:255`（MonitorInfo 镜像惯例） | STEP-2.1 §6 已记录"镜像同步约定 + STEP-2.5 时引入 `From` trait 自动生成处理"；本 STEP-2.5 没有引入 `From` trait。判**接受**——STEP-2.6 service 层只需手工转换一次（6 字段字段同序），build.rs 生成是 over-engineering。 | 维持手工镜像 + STEP-2.6 service 层手工转换；或后续 polish 引入 build.rs |

### 4 项 code-review finding 验证

| Finding | 状态 |
|---|---|
| #1 (P1 perf fix) `libei.rs` gate 在 `zones_have_changed` | ❌ **fix 写进了 diff，但 gate 位置错误导致 fetch 永不触发**。见上表 P1 BUG |
| #2 (P1 idle limitation) 是否在 STEP-2.4.md §6 显式记录 | ✅ 已记录（line 158-162 "libei idle-path 已知 limitation"） |
| #3 (P2 macOS doc 注释重复) 是否未触碰 | ✅ 未触碰（`git diff 1bfaa45..HEAD -- input-capture/src/macos.rs` 仅 5 行，全部是 `monitors()` trait impl） |
| #4 (Tech debt `From` trait) 是否未触碰 | ✅ 未触碰（geometry / lan-mouse-ipc 均不在本批 diff） |

### 已检查且无 BUG 的项

- **STEP-2.3 Windows `enumerate_displays_inner`**：`EnumDisplayDevicesW` + `EnumDisplaySettingsW` 路径用 `device.DeviceName.as_ptr()`（替代之前的 `&device`）传 `lpszDeviceName`，FFI 调用语义清晰。`DISPLAY_DEVICE_PRIMARY_DEVICE` flag 正确用于 primary 判定。
- **STEP-2.3 Windows `wide_string_to_string`**：处理 UTF-16LE 解码 + nul-terminator + 退化（empty buffer）三种 case；CJK 单测覆盖（"戴尔"）。
- **STEP-2.3 Windows `build_stable_id`**：fallback 链（DeviceID → DeviceName → 全空兜底）三段单测覆盖，与 macOS `DisplayInfo::unknown` fallback 对齐。
- **STEP-2.4 layer_shell `update_output_info`**：`has_xdg_info == true` 时 publish monitors；这是 Wayland 协议保证在 `Done` 事件到达后两个 info 都 ready 的时点，filter 半填字段避免 `position = (0, 0)` 假 primary。正确。
- **STEP-2.4 layer_shell `deregister_global`**：global remove 后 publish 一次；正确。
- **STEP-2.5 五个 backend `monitors()` 类型一致性**：全部返回 `Vec<MonitorInfo>`（dummy 显式覆盖 `vec![]`，其他四个委托 `current_monitors()`）；trait 默认实现 `vec![]`；`InputCapture::monitors()` 转发 `self.capture.monitors()`。
- **STEP-2.5 lib.rs `MonitorInfo` re-export**：`pub use geometry::MonitorInfo` 仅暴露类型，不引入新 crate 依赖。
- **STEP-2.5 单测 `input_capture_monitors_delegates_to_backend`**：`OneShotCapture` 返回 1 个 `id="test:monitor"` 的 fixture；`InputCapture::monitors()` 转发后能拿到该 fixture。覆盖路径完整。

---

## 4. 跨 STEP 一致性

- ✅ **STEP-2.1 ↔ STEP-2.3 ↔ STEP-2.4 ↔ STEP-2.5 数据模型一致**：
  - `geometry::MonitorInfo` 6 字段（id / name / position / size / primary / scale）未变
  - 三个 OS 后端的稳定 id 前缀互不冲突：`macos:` / `windows:` / `wl-output:` / `libei-zone:`
  - `scale: f64` 一致（macOS Retina = 2.0，Windows 150% = 1.5，layer_shell HiDPI = 2.0，libei 默认 = 1.0）
- ✅ **STEP-2.2 ↔ STEP-2.3 ↔ STEP-2.4 公开面对称**：
  - 四个 backend 都暴露 `monitor_changes() -> watch::Receiver<Vec<MonitorInfo>>` + `current_monitors() -> Vec<MonitorInfo>`
  - 签名、生命周期、所有权语义一致
- ✅ **STEP-2.5 `Capture::monitors()` 跨 backend 一致**：
  - 全部 5 backend 显式实现 `fn monitors(&self) -> Vec<MonitorInfo>`
  - 4 个委托 `current_monitors()`（零成本），dummy 显式 `vec![]`
  - `InputCapture::monitors()` 转发链路完整
- ✅ **`lan-mouse-proto` 未被 bump**：与 PLAN §1 + REQUIREMENT §5 一致
- ✅ **`BarrierKey.monitor` 字段预留**：M1 已落，本批未消费（各 backend `from_pos(pos)` 仍走默认），与 PLAN "M2 后续 STEP 注入 monitor 维度"节奏一致

---

## 5. STEP-2.4 idle path 评估（leader 重点关注）

### 当前实现状态

STEP-2.4 §6 已显式记录两个独立的 limitation：

1. **idle path limitation（已 documented）**：`active_clients.is_empty()` 时 `do_capture` 走 `else` 分支只 await `handle_session_update_request`，不创建 session，所以**无法**在 idle 路径上 fetch zones。文档列了 4 种典型场景的不踩 / 会踩情况，结论是 "**不是 STEP-2.4 必须解决**，当前 STEP-2.4 的修复是 'perf 正确性 + 在 active 路径上保证实时更新'"。

2. **active path limitation（未 documented — 这是 §3 BUG #1）**：executor 声称 active 路径上修复了 perf 问题（gate 在 `zones_have_changed`），但 gate 位置错误（line 538 在 line 575 之前），**实际 active 路径上 fetch 也永不触发**。这是新增 BUG，不在 executor 已记录的 scope 里。

### 是否需要在 STEP-2.6 处理？

- **active path 部分**：**应当**。STEP-2.6 service 层订阅 `monitor_changes()` 推 `FrontendEvent::MonitorsChanged`。如果 P1 BUG 不修，STEP-2.6 永远收不到 libei 后端的 hot-plug 事件，整个 M2 milestone 在 Linux GNOME Wayland 上 functional broken。
- **idle path 部分**：**不必**。STEP-2.6 service 层不解决 architectural 问题（idle 时保持 session alive）；按 STEP-2.4 §6 现状保持即可。但 STEP-2.7 收尾的人类真机验证应能观测到 idle limitation 与 P1 BUG 的复合效果：拔插后若不 activate 任何 client，watch channel 不更新；activate 后**第一次** iteration 内 watch 仍不更新（因为 P1 BUG）；需要等第二次 active iteration（zones_changed 事件在此 iteration 触发）才会更新。

### 建议修复方案（executor 返工指引）

把 libei.rs:538-563 的 `if zones_have_changed { fetch_zones_for_monitor(...) }` 整块**移到 line 575 之后**：

```rust
            let (capture_result, ()) = tokio::join!(capture_session, handle_session_update_request);
            log::debug!("capture session + session_update task done!");

            // STEP-2.4 P1 fix: check zones_have_changed AFTER the join
            if zones_have_changed {
                match fetch_zones_for_monitor(input_capture, &session).await {
                    ...
                    let _ = monitors_tx.send(monitors);
                }
            }

            // disable capture
            ...
```

这样 join 完成后 `zones_have_changed` 正确反映 session 期间是否收过 `ZonesChanged` 事件。同时建议：
1. 把 `if zones_have_changed` 改名为 `if session_changed_zones` 或加 comment 说明 "checked AFTER tokio::join! to reflect session-time changes"
2. 在 idle `else` 分支后也加 comment："idle branch: zones_have_changed set during await but fetch requires session; dropped at next loop reset (documented limitation)"
3. STEP-M2-2.4.md §6 加一条新遗留："active path P1 BUG（line 538 gate 位置错误）已 fix；原 §6 idle limitation 仍存在"

---

## 6. 跨 backend monitors() 类型验证

| Backend | `Capture::monitors(&self) -> Vec<MonitorInfo>` 实现 | 委托 / 直接 |
| --- | --- | --- |
| `MacOSInputCapture` (`macos.rs:1420`) | `self.current_monitors()` | 委托 |
| `WindowsInputCapture` (`windows.rs:70`) | `self.current_monitors()` | 委托 |
| `LayerShellInputCapture` (`layer_shell.rs:923`) | `self.current_monitors()` | 委托 |
| `LibeiInputCapture` (`libei.rs:878`) | `self.current_monitors()` | 委托 |
| `DummyInputCapture` (`dummy.rs:79`) | `vec![]` | 直接（显式覆盖） |
| `Capture` trait 默认实现 (`lib.rs:368`) | `vec![]` | 默认 |
| `OneShotCapture` 测试 backend (`lib.rs:505`) | 显式 fixture（`id="test:monitor"`） | 测试 fixture |
| `InputCapture::monitors()` 转发 (`lib.rs:249`) | `self.capture.monitors()` | 转发 |

- ✅ **类型完全一致**：全部 `Vec<MonitorInfo>`（无 backend 私有类型泄漏）
- ✅ **dummy backend 显式覆盖**（非用 trait 默认），符合 PLAN §M2 STEP-2.5 "五个 backend 实现"
- ✅ **转发链完整**：`InputCapture` → `Box<dyn Capture>` → 各 backend `monitors()` → `current_monitors()` → `watch::Sender::borrow().clone()`

---

## 7. SUGGESTION 检查

- **当前 SUGGESTION.md**：空（`# 当前无活跃项`）
- **本次执行未产生新项**：
  - ✅ macOS doc 注释重复：已在 STEP-2.2 fixup validator 报告中记录 P2 cosmetic backlog，不在本批 scope
  - ✅ `From` trait 自动生成：STEP-2.5 决策"不引入"，STEP-2.6 service 层手工转换一次即可
- **新增项建议（因 P1 BUG 触发）**：
  - 应在 SUGGESTION.md 加一条 P1 必修项指向 `libei.rs:538` gate 位置错误，让 Leader / executor 在返工时一目了然。**这条不是"影响后续 ≥2 个 STEP"**，而是 "本 STEP 内必修 + 影响 STEP-2.6 全部功能"，所以**直接走返工报告，不走 SUGGESTION 流程**

---

## 8. 总体结论

- **返工**（PASS-with-followup — **不**接受当前 commit 状态）
- **理由**：
  - STEP-2.3（Windows）：完整 + 正确 + 与 PLAN 对齐
  - STEP-2.4（Linux）：layer_shell 完整 + 正确；libei **P1 BUG**——`zones_have_changed` gate 位置错误导致 fetch 永不触发，破坏 hot-plug 核心契约。executor 误以为 "已 gate 在 zones_have_changed" = perf fix 完成，实际 fix 不可用
  - STEP-2.5（Capture trait）：完整 + 正确 + 跨 backend 一致
  - 累计：本批共 3 commit，1 commit (60025ff STEP-2.4) 含 P1 必修 BUG，1 commit (74949d4 STEP-2.5) 无 BUG，1 commit (3f9f560 STEP-2.3) 无 BUG
- **NO P0（崩溃/数据丢失）**

---

## 9. 必须修的项（按优先级）

1. **❌ P1 `libei.rs:538` gate 位置错误**：
   - 现象：active 路径 + hot-plug 触发 portal DBus fetch **永不**执行
   - 影响：M2 milestone 在 Linux GNOME Wayland 上 functional broken；STEP-2.6 service 层订阅 `monitor_changes()` 收不到 libei 事件
   - 修复：把 `if zones_have_changed { fetch_zones_for_monitor(...) }` 整块移到 `tokio::join!` 之后（详见 §5 修复方案）
   - 测试：建议在 macOS dev cfg-gated 单测中 mock 一个 portal helper 验证 fetch_zones_for_monitor 被调用（当前仅纯函数单测，无法验证运行时 gate）；Linux 真机验证留 STEP-2.7
   - 建议 executor 单开 commit `fix(libei): move zones_have_changed gate after tokio::join` 返工

2. **⚠ P2 cosmetic（不阻塞）**：
   - `macos.rs:556-581` `read_display_info` doc 注释重复——保持原状，micro-cleanup backlog 留后续
   - `libei.rs:529-537` comment 描述 "active iteration 触发 fetch" 与 P1 BUG 现实不符；P1 修复后自然正确

3. **✅ 已通过**：
   - STEP-2.3 全部交付（Windows 端完整）
   - STEP-2.5 全部交付（Capture trait 整合完整）
   - milestone 边界门（未触碰 STEP-2.6 service.rs / lan-mouse-proto / lan-mouse-vue）

---

## 10. 闸门检查

| 闸门 | 结果 |
|---|---|
| 产物对得上吗 | ⚠ STEP-2.3 完整；STEP-2.4 libei 部分缺（gate 错位）；STEP-2.5 完整 |
| 依赖对得上吗 | ✅ STEP-2.1 (MonitorInfo), STEP-2.2 (macOS 公开面对齐), STEP-2.3 (Windows WinDisplayInfo 模式) 全部通过 |
| 验收对得上吗 | ✅ `cargo build --workspace` 通过；`cargo test --workspace --no-fail-fast` 全绿（47+50+7+2+12+5=123 tests）；`cargo clippy -p input-capture --all-targets -- -D warnings` exit 0；`cargo clippy -p input-capture --no-default-features --features layer_shell,libei --all-targets -- -D warnings` exit 0；`cargo fmt --check -p input-capture` exit 0 |
| milestone 边界门 | ✅ 仅触碰 M2 范围（Windows / Linux / Capture trait）；未触碰 STEP-2.6 service reconcile、M3/M4/M5+；未改 lan-mouse-proto |
| 时间预算门 | ✅ STEP-2.3 ~30 min + STEP-2.4 ~55 min + STEP-2.5 ~20 min = ~105 min（略超 1h 阈值，符合触发 validator 的约定） |

---

## 11. 测试结果日志

```
cargo test -p input-capture --lib
  running 47 tests
  test result: ok. 47 passed; 0 failed; 0 ignored
  含 STEP-2.5 新增：
    poll_next_tests::input_capture_monitors_delegates_to_backend ... ok

cargo test --workspace --no-fail-fast
  input-capture         47 passed
  lan-mouse             50 passed
  input_channel_routing  7 passed
  quic_smoke            2 passed
  lan-mouse-ipc         12 passed
  lan-mouse-proto       5 passed

cargo build --workspace                                 → Finished in 4.24s
cargo fmt --check -p input-capture                      → exit 0
cargo clippy -p input-capture --all-targets -- -D warnings → exit 0
cargo clippy -p input-capture --no-default-features
            --features layer_shell,libei --all-targets
            -- -D warnings                              → exit 0
```

> **注**：`cargo clippy --workspace --all-targets -- -D warnings` 报 5-7 个 error（`src/connect.rs`, `src/quic_transport/endpoint.rs`, `src/quic_transport/session.rs` 中的 `assertions_on_constants` / `doc_list_item_without_indentation` / `too_many_arguments`），全部是 pre-existing（commit `6a95a0e port change` / `828cc51 init` 引入），**与本批无关**。前次 validator 报告（STEP-VALIDATION-M2-2.1-2.2.md §2）也仅跑 `cargo clippy -p input-capture -p lan-mouse-ipc`，scope 一致。本批保持同样 scope 的 clippy 验证。

---

## 12. 建议下一步

- **派 executor 返工**修复 P1 BUG（libei gate 位置错位），单开 commit `fix(libei): move zones_have_changed gate after tokio::join`。
- 返工后无需再跑 validator（M2 batch 仍在累积）；待 STEP-2.6 提交后一并 validator 审 M2.STEP-2.3-2.6 累计批次。
- 累计执行时间建议在 P1 返工通过后 reset 到 0；如 STEP-2.6 完成时累计 > 1h，再次触发 validator。
- LEADER-STATE 应更新：
  - "M2.STEP-2.5 ⏳ 进行中" → "M2.STEP-2.5 ✅（P1 libei gate 位置 BUG 待返工）"
  - 累计执行时间追加 ~20 min（STEP-2.5），保持上次 validator reset 后的累计 ~105 min
  - 加 "[P1 BUG] libei.rs:538 zones_have_changed gate 位置错误，需 executor 返工" 到"待办 / 决策项"