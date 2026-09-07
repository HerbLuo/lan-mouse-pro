# Validation: M2 STEP-2.2-FIXUP

> 审阅日期：2026-09-06　审阅 STEP 范围：STEP-2.2-fixup（commit `1bfaa45`）
> 起点 commit：`35c9e32`　终点 commit：`1bfaa45`
> 前序 validator 报告：`next/STEP-VALIDATION-M2-2.1-2.2.md`（PASS-with-followup，1 P1 + 3 P2）
> 触发原因：P1 BUG 必修 + 3 P2 cosmetic 返工

## Verdict

**PASS**（带 1 个 P2 cosmetic follow-up，与 P1 修复无功能影响）

P1 必修项（id-collision guard）正确落地；P2 #1（UTF-8 覆盖）+ P2 #2（last_monitors 字段删除）+ P2 #3（不变）全部到位；新建单测全部绿、fmt + clippy + workspace test 全绿。

---

## 1. P1 修复确认（必修）

| 检查项 | 文件:行 | 期望 | 实际 | 结论 |
|---|---|---|---|---|
| `DisplayInfo::unknown(display_id)` helper 把 `display_id` 注入 `location` 字段 | `macos.rs:478-496` | `location = format!("unknown-{display_id}")` | 完全一致 | ✅ |
| early-return #1（`CGDisplayIOServicePort == 0`）改用 `DisplayInfo::unknown(display_id)` | `macos.rs:586` | `return DisplayInfo::unknown(display_id);` | 完全一致 | ✅ |
| early-return #2（`dict_ref.is_null()`）改用 `DisplayInfo::unknown(display_id)` | `macos.rs:596` | `return DisplayInfo::unknown(display_id);` | 完全一致 | ✅ |
| `enumerate_monitors` 处注释重写（与代码 id 公式一致） | `macos.rs:530-536`（原 518 行，因 12 行新增偏移到 530） | 注释说明 `read_display_info` 把 `display_id` 拼入 fallback `location`，并引用 STEP-M2-2.2-FIXUP | 完全一致 | ✅ |
| 单测 (i) fallback 形状（vendor=0/product=0/serial=""，location 含 `unknown-{display_id}`，name=None） | `macos.rs:1683-1692` | 断言完整 | 完全一致 | ✅ |
| 单测 (ii) 单 IOKit fail id 含 `display_id` | `macos.rs:1698-1708` | 断言 `id.contains("unknown-4660")` + `id == "macos:0000:0000::unknown-4660"` | 完全一致 | ✅ |
| 单测 (iii) 两个 IOKit fail id 互异 + 前缀一致 | `macos.rs:1716-1733` | `assert_ne!(id_a, id_b)` + `starts_with("macos:0000:0000:")` | 完全一致 | ✅ |
| 新单测不被 `#[ignore]` | `macos.rs:1681, 1697, 1715` | 无 ignore 标注 | 三个都是 `#[test]` | ✅ |

### P1 验证补充

- `display_id` 类型为 `CGDirectDisplayID = u32`；`format!("unknown-{display_id}")` 用 `{}` Display 走 u32 十进制输出，与单测期望 `"unknown-69671552"`（0x4271a80）、`"unknown-4660"`（0x1234）一致。
- happy path wire shape 不变：`vendor/product/serial/location` 仍由 IOKit 报告，IOKit 成功路径不进入 `DisplayInfo::unknown` 分支；只在 `service == 0` 或 `dict_ref.is_null()` 时触发。
- `DisplayInfo::default()` 在生产代码不再使用（仅一处测试 docstring 引用 `DisplayInfo::default()` 对比说明）。
- cross-platform 行为：fixup 仅改 macOS-only 代码，未触碰 Windows / Linux backend，无 cross-platform 影响。

---

## 2. P2 修复确认

### P2 #1 — UTF-8 单测覆盖 CJK + accented Latin + em-dash

| 检查项 | 文件:行 | 期望 | 实际 | 结论 |
|---|---|---|---|---|
| name 字符串含 CJK + accented Latin + em-dash 三者 | `geometry/mod.rs:745` | 同时含 `"戴尔"` / `"áéíóú ñ"` / `"—"` | `"LG UltraFine 5K áéíóú ñ — 戴尔"` | ✅ |
| docstring 与 name 匹配（声明三个全覆盖） | `geometry/mod.rs:732-737` | "CJK + accented Latin + em-dash" + 注释三个列出 | 完全一致 | ✅ |
| 4 个 `assert!(json.contains(needle))` 锁死 byte-fidelity | `geometry/mod.rs:758-763` | `["áéíóú", "ñ", "戴尔", "—"]` | 完全一致 | ✅ |

### P2 #2 — 删除 `last_monitors` 字段

| 检查项 | 文件:行 | 期望 | 实际 | 结论 |
|---|---|---|---|---|
| 字段声明删除 | `macos.rs` | 不再含 `last_monitors: Vec<MonitorInfo>` | 字段及 4 行 doc 删除（diff lines 9-19） | ✅ |
| 3 处 `self.last_monitors = ...` 赋值删除 | `macos.rs` | 不再含赋值 | `grep last_monitors` 全文件无业务引用（仅测试模块 `use super::` 文档注释提及一次） | ✅ |
| `last_monitors: Vec::new()` 初始化删除 | `macos.rs:124`（原） | struct literal 不含该字段 | 删除 | ✅ |
| `monitors_tx` 字段 doc comment 同步更新（single source of truth 语义） | `macos.rs:72-80` | 注释写明 watch channel 是 single source of truth，`current_monitors()` 走 `monitors_tx.borrow()` | 完全一致 | ✅ |
| `current_monitors()` 实现走 `monitors_tx.borrow()`（不依赖被删字段） | `macos.rs:1310-1312` | `self.monitors_tx.borrow().clone()` | 完全一致 | ✅ |
| 删除后单测仍全绿（46 passed） | `cargo test -p input-capture --lib` | 46 passed | 46 passed（+3 vs STEP-2.2） | ✅ |

注：`grep last_monitors input-capture/src/macos.rs` 仅返回 1 处引用，位于测试模块的 `DisplayInfo::unknown` doc 注释（解释历史背景），非业务引用。

### P2 #3 — validator 已接受为 P2 文档偏差，本批未触碰

- ✅ 未触碰 `enumerate_monitors(displays: &[DisplayRect])` 签名，未动 `let _ = displays;` placeholder。

---

## 3. 偏离 PLAN

- ✅ 无偏离：fixup 仅触碰 PLAN §M2 STEP-2.2 列出的同一文件（`input-capture/src/macos.rs` + `input-capture/src/geometry/mod.rs`）。
- ✅ 未触碰 STEP-2.3（Windows）/ 2.4（Linux）/ 2.5（Capture trait）/ 2.6（service.rs）/ 2.7（真机验证）。
- ✅ 未 bump `lan-mouse-proto`（与 PLAN §1 + REQUIREMENT §5 一致）。
- ✅ 未改 M1 数据模型（`BarrierKey.monitor` 仍走默认）。

---

## 4. 偏离 REQUIREMENT

- ✅ 未破坏：`REQUIREMENT.md §3.1-3.4`（QUIC + 剪贴板）未触碰；§5 多屏定位的 STEP-2.2 任务完全对齐；本批不 bump `lan-mouse-proto`。

---

## 5. BUG 清单

| 严重度 | 位置 | 现象 | 建议修复 |
|---|---|---|---|
| **P2 cosmetic** | `input-capture/src/macos.rs:556-581`（`read_display_info` 函数 doc 注释） | fixup ADD 了新 doc 注释（"Resolve the IOKit service port... Id-collision guard... SAFETY..."，18 行），但**未 DELETE**原 doc 注释（"Resolve the IOKit service port... SAFETY..."，7 行）。rustdoc 编译器会合并，但渲染后的文档读起来 jumbled：先 "Resolve the IOKit service port..." 一次 + SAFETY，再重复一次 + Id-collision guard + SAFETY。rustfmt / clippy / build 均不受影响，纯 rustdoc 排版问题。 | 删除 `macos.rs:556-563` 原 7 行 doc 注释，仅保留新增的 `macos.rs:564-581` 18 行 doc 注释。 |

### 已检查且无 BUG 的项

- `DisplayInfo::unknown` helper 内 `format!("unknown-{display_id}")` 用 `{}` Display 对 `u32` 输出十进制 —— 与单测断言 `unknown-69671552`（0x4271a80）/ `unknown-4660`（0x1234）一致，无 hex/oct/dec 误用风险。
- happy path 上 IOKit 成功时不进入 `DisplayInfo::unknown`，`location` 仍是 OS 报告字符串，wire shape 不变。
- 删除 `last_monitors` 后 `InputCaptureState` `Debug` derive 更轻（少一个字段），无新报错。
- IOKit FFI 释放路径未变（fixup 仅改 fallback 分支）；`Default` derive 保留（虽未在生产代码使用）。
- `monitors_tx` watch channel 跨线程（`InputCaptureState` producer + `MacOSInputCapture` 主线程）双持有未变，单写者安全。
- 删除 `last_monitors` 后 3 个早期路径（`new()` / `DisplayReconfigured` / `MonitorsChanged` manual）都只走 `self.monitors_tx.send(...)`，无遗漏。

---

## 6. 跨 STEP 一致性

- ✅ **STEP-2.1 ↔ STEP-2.2 ↔ fixup 数据模型一致**：`input_capture::geometry::MonitorInfo` 6 字段未变；`DisplayInfo` 仍是 STEP-2.2 内部辅助 struct，未对外暴露。
- ✅ **`lan-mouse-proto` 未 bump**：与 PLAN §1 + REQUIREMENT §5 一致。
- ✅ **`BarrierKey.monitor` 字段预留**仍走默认（M1 已落，本批未触碰）。
- ✅ **`DisplayInfo::Default` derive 保留**虽 `Default::default()` 在生产代码无 caller，但不删以保持"struct 仍可作为通用容器"的语义，且 `unknown()` 是显式首选路径。

---

## 7. 总体结论

- **接受**（PASS）
- 理由：P1 BUG（id-collision guard）必修项完全修复，所有 8 项检查项全通过；3 个新单测覆盖 (i)(ii)(iii) 三种场景且实际执行；P2 #1（UTF-8 三语言 + 4 字节断言）、P2 #2（last_monitors 字段删除 + doc comment 更新 + current_monitors 改走 monitors_tx.borrow()）全部到位；fmt + clippy + workspace 全绿；PLAN / REQUIREMENT 无偏离。唯一遗留 1 个 P2 cosmetic doc 注释重复（rustdoc 渲染 jumbled 但不影响代码行为），建议下次顺手清理。

---

## 8. 闸门检查

| 闸门 | 结果 |
|---|---|
| 产物对得上吗 | ✅ `DisplayInfo::unknown` helper + 2 个 early-return 切到 helper + 注释重写 + `last_monitors` 字段删除 + 3 个新单测 + UTF-8 单测 docstring/name/断言全更新 |
| 依赖对得上吗 | ✅ 全部基于 STEP-2.2 已落地的 `MonitorInfo` / `DisplayInfo` / `build_stable_id`；未引入新 crate / 新模块依赖 |
| 验收对得上吗 | ✅ `cargo build --workspace` / `cargo test --workspace` / `cargo fmt --check -p input-capture` / `cargo clippy -p input-capture --all-targets -- -D warnings` 全绿 |
| milestone 边界门 | ✅ 仅触碰 STEP-2.2 范围内的两个文件 + STEP 文档；未触碰 STEP-2.3/2.4/2.5/2.6/2.7 任何代码 |

---

## 9. 测试结果日志

```
cargo test -p input-capture --lib
  running 46 tests
  test result: ok. 46 passed; 0 failed; 0 ignored
  含新增：
    macos::tests::display_info_unknown_encodes_display_id_in_location ... ok
    macos::tests::stable_id_includes_display_id_when_iokit_unavailable_single ... ok
    macos::tests::stable_ids_for_two_simultaneously_failed_displays_are_unique ... ok

cargo test --workspace --no-fail-fast
  input-capture         46 passed  (+3 vs STEP-2.2)
  lan_mouse             50 passed
  input_channel_routing  7 passed
  quic_smoke            2 passed
  lan_mouse_ipc         12 passed
  lan_mouse_proto       5 passed

cargo fmt --check -p input-capture   → exit 0
cargo clippy -p input-capture --all-targets -- -D warnings → exit 0（无 clippy warning）
```

---

## 10. 建议下一步

- **接受**当前 fixup，派发 **STEP-2.3（Windows `enumerate_displays()`）**。
- P2 cosmetic doc 注释重复可下次 STEP-2.3 顺手清，也可单开 micro-cleanup PR（不动代码行为）。
- 累计执行时间建议清零（自上次 validator 审阅起 ~95 min → fixup 重审通过后 reset）。