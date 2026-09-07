# Validation: M2 STEP 2.1 + 2.2

> 审阅日期：2026-09-06　审阅 STEP 范围：2.1 + 2.2
> 起点 commit：08cb3a3 (M1)　终点 commit：35c9e32
> 起点 hash 实际提交：`08cb3a3 M1: barrier key data model refactor`
> 终点 hash 实际提交：`35c9e32 feat(macos): enumerate displays with stable id and hot-plug events`
>
> 同时审阅：9298acb `fix(input-capture): adapt Windows event thread to non-Copy BarrierKey`（M1 范围内遗留；非 M2 STEP-2.1/2.2 提交，本批次不变更）

## Verdict

**PASS-with-followup**（含 1 个 P1 逻辑 BUG + 3 个 P2 文档/测试覆盖小项）

## 1. 偏离 PLAN

### STEP-2.1

- ✅ **完全符合 PLAN §M2 STEP-2.1**：数据模型 6 字段、`#[serde(rename_all="snake_case")]`、IPC 镜像、`FrontendEvent::MonitorsChanged(Vec<MonitorInfo>)` + `FrontendEvent::BindingInvalid(ClientHandle, String)`、serde round-trip 单测覆盖 UTF-8 / 负坐标 / 不同 scale / 老 wire 兼容。`scale` 选 `f64`（PLAN 未限定类型；macOS mixed-DPI 报告非整数）是 micro-偏差，已在 STEP 文档明示并补 `monitor_info_round_trip_mixed_scale` 单测覆盖 1.0/1.25/1.5/2.0/2.5。
- ✅ milestone 边界门：未触碰 macOS / Windows / Linux 后端（STEP-2.2/2.3/2.4）；未改 `Capture` trait（STEP-2.5）；未改 `src/service.rs`（STEP-2.6）。
- ✅ `serde` 加到 `input-capture/Cargo.toml` 是 PLAN 显式要求。

### STEP-2.2

- ✅ **完全符合 PLAN §M2 STEP-2.2 任务描述**：
  - `enumerate_monitors()` 用 `CGDisplay::active_displays()` 拿 ID ✅
  - `CGDisplay::new(d).bounds()` 拿矩形 ✅
  - `IODisplayCreateInfoDictionary` 拼 vendor/model/serial/location 生成稳定 `id` ✅
  - 接现有 `DisplayReconfigured` 路径 ✅
- ⚠️ **轻微 reinterpretation（PLAN-偏差，非 BUG）**：PLAN 文字"通过 `notify_tx` 发新列表"被实装为"`handle_producer_event` 内直接 `self.monitors_tx.send(...)`"（watch::Sender 一跳到位）。STEP 文档显式记录此偏差并保留 `ProducerEvent::MonitorsChanged` 变体供 STEP-2.6 "手动刷新"路径。不破坏后续 STEP-2.5/2.6 契约。判 P2 文档准确性偏差，**接受**。
- ⚠️ **`displays: &[DisplayRect]` 参数当前未用**（`#[allow(dead_code)]` 标注 + `let _ = displays;`）。STEP 文档承认这是为"未来注入 test fixture"留的入口。判 P2 cosmetic 偏差，**接受**。
- ✅ milestone 边界门：未引入 Windows / Linux 后端枚举（STEP-2.3/2.4）；未改 `Capture` trait（STEP-2.5）；未改 `src/service.rs`（STEP-2.6）；未改 `lan-mouse-ipc`（STEP-2.1 已就位）。

## 2. 偏离 REQUIREMENT

- ✅ **未破坏**：`REQUIREMENT.md` §3.1-3.4（QUIC + 剪贴板）功能未触碰；§5（多屏定位）的 STEP-2.1/2.2 任务完全对齐；本批次不 bump `lan-mouse-proto`（与 PLAN §1 一致）。

## 3. BUG 清单

| 严重度 | 位置 | 现象 | 建议修复 |
|---|---|---|---|
| **P1** | `input-capture/src/macos.rs:546-587`（`read_display_info`） | 当 `CGDisplayIOServicePort(display_id) == 0` 或 `IODisplayCreateInfoDictionary` 返回 null 时，`DisplayInfo::default()` 返回 `serial=""` / `location=""`。`enumerate_monitors` 中 `build_stable_id(0, 0, "", "")` = `"macos:0000:0000:"`（注意末尾冒号——因为 `location` 也是空串）。**同一台机器上若有两块显示器同时 IOKit 失败（罕见但可能：TCC 权限被吊销 + transient state）会产出完全相同的 id**，破坏 PLAN §M2 STEP-2.2 声明的"stable id"不变量 1。代码注释 `macos.rs:518` 声称"still unique per display id"是**错误的**——id 表达式完全不包含 `display_id`。 | 修复：`read_display_info` 返回默认 `DisplayInfo` 时把 `location` 设为 `format!("unknown-{display_id}")`（或类似），把 `display_id` 注入 id；或者在 `enumerate_monitors` 拼 id 前判 `vendor == 0 && product == 0 && serial == "0" && location == "Unknown"` 走 fallback 分支。同时修正 macos.rs:518 的注释。 |
| **P1** | `input-capture/src/macos.rs:518`（注释 vs 实际 id 公式） | 注释 "still unique per display id" 与代码 `build_stable_id(info.vendor, info.product, &info.serial, &info.location)` 不符——后者不含 `display_id`。注释与代码漂移，是 P1 BUG 的潜在后续。 | 同步注释与代码（修复见上）。 |
| **P2** | `input-capture/src/geometry/mod.rs:723-751`（`monitor_info_round_trip_utf8_name`） | 单测 docstring 声明覆盖 "CJK / accented Latin"，实际测试用例 `"戴尔 U2723QE — 左"` 只覆盖 CJK + em-dash（U+2014），不含 accented Latin（é, ñ, ü 等）。与单测 docstring 名实不符。 | 把 name 换成 `"LG UltraFine 5K áéíóú ñ"` 或追加第二个测试用例覆盖 accented Latin。 |
| **P2** | `input-capture/src/macos.rs:78-82`（`last_monitors` 字段） | STEP-M2-2.2 §6 声称"`#[allow(dead_code)]` 静音"，但实际字段声明无 `#[allow(dead_code)]` 标注。Rust `dead_code` lint 未触发（因字段有写），但**字段在 STEP-2.2 内没有任何 reader**——3 处 `self.last_monitors = monitors.clone();` 都是 write-only。每次 `DisplayReconfigured` 多一次 `Vec<MonitorInfo>` clone（典型 1-4 个显示器，代价很小，但与"无害缓存"的语义不符）。 | 二选一：(a) 在字段加 `#[allow(dead_code)]` 让意图显式；(b) 删除字段，把 watch channel 当唯一真相源（plan 文字未要求此字段）。建议 (b)：STEP-2.5 用 `monitor_changes().borrow().clone()` 即可，同步删 STEP-M2-2.2 §6 中关于 `#[allow(dead_code)]` 的文档。 |

**已检查且无 BUG 的项：**

- IOKit FFI 释放路径：`service != 0` 路径上 `IOObjectRelease(service)` 在 `IODisplayCreateInfoDictionary` 之后立刻调用；dict 通过 `CFDictionary::wrap_under_get_rule` 持有，Drop 时 `CFRelease`；`service == 0` 路径无对象可释放，提前 return OK。
- `CFNumber::wrap_under_get_rule` + 立即 `.to_i64()` / `CFString::wrap_under_get_rule` + 立即 `.to_string()`：wrap_under_get_rule 自身做 CFRetain，Drop 时 CFRelease，net 平衡，无泄漏。
- watch::Sender 跨线程：`monitors_tx` clone 在 `InputCaptureState`（producer task 持有，Async Mutex 内）和 `MacOSInputCapture`（主线程持有）之间共享；`watch::Sender` 是 `Clone + Send + Sync`，单写者（state），无锁并发安全。
- `ProducerEvent::MonitorsChanged(Vec<MonitorInfo>)` 变体：`#[allow(dead_code)]` 正确（被 match 但从未被构造）；不破坏现有 match 完整性（handle_producer_event 已有 arm）。
- `tokio::select!` / `mpsc` / `Mutex<InputCaptureState>` 锁路径未引入新的死锁/重入风险：`InputCaptureState::new()` 在 Arc 包装前同步完成，不持锁；后续 `handle_producer_event` 在 `state.lock().await` 内仅做不动锁的同步操作 + 一次 `self.monitors_tx.send()`。
- `core_foundation::CFDictionary::find(*const c_void)` API 正确：`K = V = *const c_void`（默认参数），`ToVoid<*const c_void> for *const c_void` + `FromVoid for *const c_void` 都已实现，编译通过 + 单测覆盖。
- `ProducerEvent::DisplayReconfigured` 路径内串行：先 `update_bounds()` 再 `enumerate_monitors(&self.displays)`，不并发修改 `self.displays`，单线程内顺序正确。

## 4. 跨 STEP 一致性

- ✅ **STEP-2.1 ↔ STEP-2.2 一致**：`input_capture::geometry::MonitorInfo`（STEP-2.1 落）与 `macos.rs::enumerate_monitors`（STEP-2.2 填）的 6 字段一一对应；`id` 字段类型 `String` 一致；`scale: f64` 一致；`position: (i32, i32)` / `size: (u32, u32)` 类型与 STEP-2.1 类型一致。
- ⚠️ **镜像同步约定未触发漂移**：STEP-2.1 §6 注释记录 `geometry::MonitorInfo` 与 `lan_mouse_ipc::MonitorInfo` 是手工镜像的两个独立 struct——本批次 STEP-2.2 只触碰 `geometry` 侧，未触 IPC 侧，无漂移风险。STEP-2.5 / 2.6 引入 service 层转换时需补单测覆盖。
- ✅ **`lan-mouse-proto` 未被 bump**：与 PLAN §1 协议层不变 + REQUIREMENT §5 不 bump 协议 一致。
- ✅ **`BarrierKey.monitor` 字段预留**：`M1/STEP-1.1` 已落，本 STEP 未消费（`macos.rs` 中 `from_pos(pos)` 仍走默认），与 PLAN 设计的"M2 后续 STEP 注入 monitor 维度"节奏一致。

## 5. 总体结论

- **继续（带建议）**（PASS-with-followup）
- **理由**：STEP-2.1 / STEP-2.2 的核心交付完全到位——数据模型 + IPC 镜像 + 9 个新单测 + macOS IOKit 枚举 + 稳定 ID 拼接 + 热插拔 watch channel + 纯函数测试覆盖。`cargo build --workspace` / `cargo test --workspace` / `cargo fmt --check` / `cargo clippy --all-targets -D warnings` 全绿。无 PLAN 严重偏离 / 无 REQUIREMENT 偏离 / 无 P0 崩溃 / 无 P1 数据丢失。**1 个 P1 BUG**（IOKit 全失败下的 id 碰撞风险）必须在 STEP-2.5 之前修，否则 `Capture::monitors()` 实现后第一次 enumerate 就可能产生重复 id 触发 BindingInvalid 误报或 binding 路由错乱。建议 executor 在 STEP-2.3 之前补一行修复。

## 6. 必须修的项（按优先级）

1. **❌ P1 `macos.rs::read_display_info` + `enumerate_monitors` + 注释**：IOKit 全失败路径下稳定 id 退化为非唯一值；注释 "still unique per display id" 名实不符。修复方案见 BUG 表第 1-2 行。
2. **⚠ P2 建议、不阻塞**：
   - `geometry/mod.rs::monitor_info_round_trip_utf8_name` 补充 accented Latin 覆盖
   - `macos.rs::last_monitors` 字段要么加 `#[allow(dead_code)]` 要么直接删（推荐删）
3. **✅ 已通过、可继续**：
   - STEP-2.3（Windows `enumerate_displays()`）前置依赖 ✅
   - STEP-2.4（Linux layer_shell + libei）前置依赖 ✅
   - milestone 边界门（trait 化 / service reconcile）未越界

## 7. 建议下一步

- 派 executor 修复上述 P1 后再继续 M2.STEP-2.3（Windows）或允许 P1 修复与 STEP-2.3 并行（IOKit 修复是 macOS-only，不影响 Windows STEP）。修复 diff 仍属于 STEP-2.2 范围（commit `35c9e32` 的 patch-fixup 提交，commit message 建议 `fix(macos): include display_id in stable id when IOKit unavailable`）。
- 累计执行时间自上次 validator 审阅起：~50 min（STEP-2.1 ~25 min + STEP-2.2 ~45 min，含 8 个新单测 + 2 轮 build 失败修复）。距 LEADER 触发 validator 的 "1 小时" 阈值差 10 min，建议 P1 修复完一并 close 触发下一轮 validator，避免累计跨阈值。
- LEADER-STATE 应更新 "M2.STEP-2.2 ⏳ 进行中" → "M2.STEP-2.2 ✅（带 1 个 P1 followup 派 executor）"。