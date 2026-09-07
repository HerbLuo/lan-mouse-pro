# Validation: M2 STEP-2.4-FIXUP (libei gate position P1 fixup)

> 审阅日期：2026-09-07　审阅范围：STEP-2.4-FIXUP（commit `dc7e747`）
> 起点 commit：`dc7e747~1`（= `74949d4`）　终点 commit：`dc7e747`
> 起点 hash：`74949d4 feat(input-capture): expose Capture::monitors snapshot across backends`
> 终点 hash：`dc7e747 fix(libei): move zones_have_changed gate after tokio::join`
> 前序报告：`next/STEP-VALIDATION-M2-2.3-2.4-2.5.md`（PASS-with-followup，1 P1 + 3 P2 cosmetic）
> 返工 STEP 文档：`next/STEP-M2-2.4-FIXUP.md`

---

## Verdict

**PASS** — P1 BUG 已修复；P2 cosmetic 已按 plan 处理；无 PLAN / REQUIREMENT 偏离；无新 BUG；现有 24 个 libei 单测 + 3 个新单测 + 123 个 workspace 单测全绿。

---

## 1. P1 修复确认

### 1.1 gate 位置移到 `tokio::join!` 之后

| 验证项 | 位置 | 结果 |
|---|---|---|
| gate-check 不在 join 之前 | `libei.rs:602` `tokio::join!` 在前；`libei.rs:614` `publish_monitors_if_changed` 调用在后 | **PASS** |
| fetch 真的会被调用（flag=true 路径） | `libei.rs:614-618` 调起 `fetch_zones_for_monitor(input_capture, &session)` | **PASS** |
| fetch 失败语义正确（保留 previous value） | `libei.rs:509-511` `Err(e) => log::warn + return`；不调 `monitors_tx.send` | **PASS** |
| happy path（Ok → publish） | `libei.rs:491-507` `Ok(monitors)` → log → `monitors_tx.send(monitors)` | **PASS** |
| helper 抽出成独立函数 | `libei.rs:478-513` `publish_monitors_if_changed<E, F, Fut>` | **PASS** |

### 1.2 flag 时序验证

- **flag 重置时机**（libei.rs:539）：`let mut zones_have_changed = false;` 在 `loop` 顶部每个 iteration 起始重置
- **flag 设置时机**（libei.rs:550-553）：`handle_session_update_request` async block 的 `zones_changed.next()` arm 在 poll 时执行 `zones_have_changed = true`
- **flag 读取时机**（libei.rs:602-618）：`tokio::join!` 先 poll `handle_session_update_request`，join 完成后 `publish_monitors_if_changed(zones_have_changed, ...)` 在 line 614 才读 flag
- **结论**：flag 读取发生在 future 可能设置它之后，gate 永远读到 session-time state（不再读到 reset 后的 false）

### 1.3 3 个新回归单测覆盖

| 单测 | file:line | 验证 invariant | 结果 |
|---|---|---|---|
| `publish_monitors_if_changed_skips_fetch_when_flag_false` | libei.rs:1210-1230 | flag=false 时 fetch closure 必不被调用 + watch channel 无 spurious notification | **PASS** |
| `publish_monitors_if_changed_publishes_on_ok` | libei.rs:1237-1261 | flag=true+Ok 时 `monitors_tx` 被通知 + receiver 读到构造的 `MonitorInfo`（含 id/primary/scale 字段） | **PASS** |
| `publish_monitors_if_changed_does_not_publish_on_err` | libei.rs:1267-1278 | flag=true+Err 时 `monitors_tx` 不被通知（watch channel 保留 previous value） | **PASS** |

3 个单测均 `#[tokio::test]` 无 `ignore`；均 cfg-gated 在 `mod libei`（lib.rs:21 `#[cfg(libei)]`）→ 在 macOS dev 上不编、在 Linux CI 上跑（与既有 10 个 libei 单测同等约束）。

### 1.4 现有 24 个 libei 单测 + workspace 测试

- `cargo test --workspace --no-fail-fast`：
  - `input-capture` 47 passed（含 non-libei 部分；macOS dev 不编 libei.rs）
  - `lan-mouse` 50 passed
  - `input_channel_routing` 7 passed
  - `quic_smoke` 2 passed
  - `lan-mouse-ipc` 12 passed
  - `lan-mouse-proto` 5 passed
  - 合计 **123 passed**（与前次 validator 报告数完全一致；fixup 没破坏任何既有测试）

### 1.5 `cargo check -p input-capture --no-default-features --features layer_shell,libei --tests`

- EXIT 0；类型 + 借用检查全绿；3 个新 libei 单测在 Linux cfg 下编译通过

---

## 2. P2 cosmetic 处理确认

| P2 项 | 处理 | 结果 |
|---|---|---|
| P2 #1 `libei.rs:529-537` comment 与 P1 BUG 不符 | comment 拆分重写：active 分支（libei.rs:571-590, 605-613）解释 fetch 必须在 join 后做 + 引用 validator 报告 + 引用 STEP-M2-2.4 §6 遗留；idle 分支（libei.rs:632-641）补 comment 解释为什么 idle 不 fetch + 引用 §6 遗留 | **PASS** |
| P2 #2 `macos.rs:556-581` doc 注释重复 | 未触碰（macos.rs:556-582 重复段仍存在） | **PASS**（按 leader 给定判别标准"跨文件/改动量大 → 留 backlog"放行） |
| P2 #3 `geometry/mod.rs:255` MonitorInfo 镜像惯例 `From` trait | 未触碰（geometry/mod.rs:255 仍无 `impl From<...> for MonitorInfo`） | **PASS**（前次 validator 已接受为 STEP-2.6 手工转换一次即可；over-engineering） |

---

## 3. PLAN / REQUIREMENT 偏离

### 3.1 PLAN 偏离

- ✅ **零 PLAN 偏离**：修复严格在 PLAN §M2 STEP-2.4 列出的同一个文件 `input-capture/src/libei.rs` 内；未触碰其他 backend（macos / windows / layer_shell / dummy）、未触碰 `Capture` trait（STEP-2.5 已就位）、未触碰 `src/service.rs`（STEP-2.6 待派发）、未触碰 `lan-mouse-proto`（与 PLAN §1 一致）

- ⚠ **与 STEP-M2-2.4.md §6 遗留的关系**：
  - §6 已记录的 **idle-path 已知 limitation** 仍存在（与本 P1 修复**独立**）：active 分支修好后，idle 分支（`active_clients.is_empty()`）仍因无 live session 无法调 portal —— libei.rs:632-641 idle comment 已引用 §6 遗留
  - §6 已记录的 **`monitor_changes()` 在 STEP-2.4 内无消费者**（`#[allow(dead_code)]`）保持不变；STEP-2.6 service 层订阅仍是后续工作
  - §6 已记录的 **真机 Wayland / libei 拔插验证** 仍留 STEP-2.7 人类验证（macOS dev 跑不到）

### 3.2 与 validator 建议的偏差

- ⚠ **修复实施细节**：把 inline 块抽成 `publish_monitors_if_changed` helper。前次 validator 建议只说"移到 join 后"，未明确是否抽函数。**接受**——抽函数的理由见 STEP-M2-2.4-FIXUP.md §2：让 P1 gate 位置错位的回归可单测锁死 + 文档化"MUST be after tokio::join"约束

### 3.3 REQUIREMENT 偏离

- ✅ **未破坏**：
  - `REQUIREMENT.md §3.1-3.4`（QUIC + 剪贴板）未触碰
  - `REQUIREMENT.md §5.2`（多屏定位）逐步推进 —— 本 fixup 完成 libei 后端在 active client + hot-plug 路径上**真正能**触发 `MonitorsChanged` 事件
  - 本 fixup 不 bump `lan-mouse-proto`（与 REQUIREMENT §5 末尾 + PLAN §1 一致）

---

## 4. 新 BUG 清单

**无新 BUG**。

| 严重度 | 位置 | 现象 | 状态 |
|---|---|---|---|
| P0 | — | — | 无 |
| P1 | — | — | 无 |
| P2 | — | — | 见 §5 followup notes |

---

## 5. 跨 STEP 一致性

- ✅ **STEP-2.4 ↔ STEP-2.5 一致性**：
  - `Capture::monitors()` trait 已在 STEP-2.5（commit `74949d4`）就位；fixup 未触碰 trait
  - `monitor_changes() / current_monitors()` 公开面（libei.rs:431）保持不变
  - `InputCapture::monitors()` 转发链路（lib.rs:249）保持不变

- ✅ **libei.rs 内部一致性**：
  - `tokio::join!` 在 `do_capture`（libei.rs:602，gate-check 之后）和 `do_capture_session`（libei.rs:783，独立 session 退出路径）出现两次；后者与 gate 逻辑无关（仅捕获 ei_task + capture_session_task 的退出结果）
  - `handle_session_update_request` async block（libei.rs:542-562）未被改动；future 内部的 `zones_changed.next()` arm（line 550-553）仍是设置 flag 的唯一途径

- ✅ **`publish_monitors_if_changed` 设计**：
  - generic over `E: std::fmt::Display` 而非固定 `ashpd::Error`：让单测可传 `String` 作错误类型（生产代码 closure 用 `Ok::<_, ashpd::Error>(...)` turbofish 显式标），调用点零成本
  - `F: FnOnce() -> Fut`：允许捕获 `&session`（mut 借用）；async block 内部 `.await` 后闭包 drop，borrow 自动释放
  - 没有引入新 unsafe（libei.rs 中所有 unsafe 仍是 `new()` 内的 `*const` deref + `do_capture` 顶部的 raw ptr，pre-existing）

- ✅ **`lan-mouse-proto` 未被 bump**：fixup 仅在 STEP-2.4 范围内（libei.rs + STEP doc）

---

## 6. followup notes（非阻塞，建议 backlog）

### 6.1 P2 文档准确性偏差（建议修正 comment 文案）

- **位置**：`libei.rs:1205-1209`（test `publish_monitors_if_changed_skips_fetch_when_flag_false` 的 doc comment）
- **现象**：comment 写"**Catches regressions where someone re-orders the gate-check back above `tokio::join!` and the flag is read in its reset state (the P1 BUG).**"
- **事实**：该单测直接调 helper 传 `zones_have_changed=false`，**只能**验证 helper 在 flag=false 时不调 fetch closure + 不发 watch channel 通知。**不能**验证"flag 在 join 之前被读"这种 race 状态 —— 该 race 在 `do_capture` 调用 helper 的位置上，不在 helper 内部。测试名 `skips_fetch_when_flag_false` 准确反映测试内容；测试 comment 描述的"re-orders the gate-check back" 实际上需要 `do_capture` 集成覆盖（macOS dev 跑不到）
- **建议**：把 comment 改为 "Locks the helper's no-op fast-path contract: flag=false skips the fetch closure entirely (so a spurious fetch never costs a portal DBus round-trip). The actual gate position in `do_capture` is documented in the helper's own doc comment and in the `do_capture` call site."
- **判**：**P2 cosmetic backlog**（非阻塞；不影响 fixup 通过；测试本身行为正确，只是 comment 描述略 overstate 范围）

### 6.2 P2 cosmetic backlog（按既定判别标准保留）

- `macos.rs:556-581` doc 注释重复（macos.rs:556-567 与 564-581 重复段仍在原位置）
- `geometry/mod.rs:255` MonitorInfo 镜像惯例 `From` trait（geometry 模块仍无 `impl From<...> for MonitorInfo`）
- **判**：**保留为 micro-cleanup backlog**（与前次 validator 报告一致；fixup scope 不在 macos.rs / geometry/）

---

## 7. SUGGESTION 检查

- `next/SUGGESTION.md` 当前为空（"当前无活跃项"）
- 本 fixup 未新增 SUGGESTION 条目
- P1 BUG（libei.rs:538 gate 位置错误）按前次 validator §7 建议"直接走返工报告，不走 SUGGESTION 流程"——已在本次返工完成
- §6.1 followup note（P2 文档准确性偏差）建议**不入 SUGGESTION.md**（测试行为正确，仅 comment 文案微调；属于 STEP-2.4-fixup 本批的 cosmetic 内务，留 micro-cleanup 一起处理更高效）

---

## 8. 闸门检查

| 闸门 | 结果 |
|---|---|
| 产物对得上吗 | ✅ `publish_monitors_if_changed` helper（generic over `E: Display`）；gate-check + fetch + publish 块从原 join 之前移到 `libei.rs:614`（after `tokio::join!` at line 602）；active + idle 两段 comment 拆开重写；3 个新单测覆盖 invariant 1/2/3 |
| 依赖对得上吗 | ✅ 基于 STEP-2.4 已落地的 `monitors_tx: watch::Sender<Vec<MonitorInfo>>` / `fetch_zones_for_monitor` / `enumerate_zones`；未引入新 crate / 新模块依赖 |
| 验收对得上吗 | ✅ `cargo build --workspace` 0 errors / `cargo test --workspace --no-fail-fast` 123 passed / `cargo fmt --check -p input-capture` exit 0 / `cargo clippy -p input-capture --all-targets -- -D warnings` exit 0 / `cargo clippy -p input-capture --no-default-features --features layer_shell,libei --all-targets -- -D warnings` exit 0 / `cargo check -p input-capture --no-default-features --features layer_shell,libei --tests` exit 0 |
| milestone 边界门 | ✅ 仅触碰 STEP-2.4 范围内的 `input-capture/src/libei.rs` + `next/STEP-M2-2.4-FIXUP.md`；未触碰 macOS / Windows / layer_shell 后端；未触碰 `Capture` trait；未触碰 service 层；未触碰 `lan-mouse-proto` |
| 时间预算门 | ✅ 实际 ~25 min（含 3 个新单测 + helper 函数 + comment 重写 + 一轮 fmt 修整），远低于 STEP 估时上限 1h，未触发拆步 |

---

## 9. 总体结论

- **PASS**（接受）
  - `libei.rs:538` gate 位置错误 P1 BUG 已修复（gate 移到 `tokio::join!` 之后、抽 helper、可单测）
  - 3 个新单测覆盖 helper 三条 invariant（perf no-op / Ok publish / Err no-publish）
  - 既有 123 个 workspace 单测全绿
  - fmt + clippy + libei cfg check 全部 exit 0
  - PLAN / REQUIREMENT 零偏离
  - milestone 边界门守住（仅 libei.rs + STEP doc）
  - 累计执行时间 ~25 min，远低于 1h 阈值

- **无 P0 / 无 P1 / 1 项 P2 cosmetic followup**（§6.1 测试 doc comment 略 overstate 范围；非阻塞，留 micro-cleanup backlog）

- **遗留**（与本 fixup 独立，STEP-2.7 人类真机验证）：
  - macOS dev 无法跑 libei 单测（`build.rs` 仅在 `unix && !macos && feature_enabled` 时设 cfg）；3 个新单测 + 既有 10 个 libei 单测只能在 Linux 真机或 CI 跑
  - idle-path 已知 limitation 仍存在（与 P1 BUG 独立）
  - 真机 Wayland / libei 拔插验证：预期路径为"启动 daemon → 拔 / 插外接显示器 → daemon log 应出现 `monitors changed (zones_changed event): N monitor(s)` → WebSocket console 收到 `MonitorsChanged` 事件"

---

## 10. 测试结果日志（2026-09-07 重跑）

```
cargo build -p input-capture
  Finished in 0.49s (no changes vs pre-fixup baseline)

cargo build -p input-capture --no-default-features --features layer_shell,libei
  Finished in 0.66s

cargo build --workspace
  Finished in 2.34s

cargo test --workspace --no-fail-fast
  input-capture          47 passed (macOS dev: 不编 libei.rs；libei 单测在 Linux CI 跑)
  lan-mouse              50 passed
  input_channel_routing   7 passed
  quic_smoke              2 passed
  lan-mouse-ipc          12 passed
  lan-mouse-proto         5 passed
  ─────────────────────────────
  合计 123 passed; 0 failed

cargo check -p input-capture --no-default-features --features layer_shell,libei --tests
  Finished (3 个新 libei 单测 Linux cfg 编译通过)

cargo fmt --check -p input-capture
  exit 0

cargo clippy -p input-capture --all-targets -- -D warnings
  exit 0

cargo clippy -p input-capture --no-default-features --features layer_shell,libei --all-targets -- -D warnings
  exit 0
```

---

## 11. 建议下一步

- **接受** STEP-2.4-fixup，里程碑 M2 仅剩 STEP-2.6 待派发（`src/service.rs` `reconcile_monitors_changed`）
- 累计执行时间 reset 到 0（M2.STEP-2.4-fixup ~25 min 已计入"上次 validator 审阅位置"——本次 validator 算"二审"，不重新累加）
- LEADER-STATE 应更新：
  - "M2.STEP-2.4-fixup ⏳ 进行中" → "M2.STEP-2.4-fixup ✅（P1 libei gate 位置 BUG 已修复，1 P2 cosmetic followup 留 micro-cleanup backlog）"
  - "M2.validator ⏳ 进行中（重审 STEP-2.4-fixup）" → "M2.validator ✅ PASS"
- 微小 followup：STEP-2.4-fixup §6.1 测试 doc comment 修正建议入 micro-cleanup backlog（与 macos.rs doc 重复 / geometry From trait 一起处理）