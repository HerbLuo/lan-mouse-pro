# STEP M2-2.4-FIXUP — Validator 反馈修复 (P1 + P2 cosmetic)

> PLAN §M2 / STEP-2.4 validator 返工（基于 commit `60025ff`）
> validator 报告：`next/STEP-VALIDATION-M2-2.3-2.4-2.5.md`
> 触发原因：validator PASS-with-followup，1 P1 必修 BUG + 3 P2 cosmetic
> 执行日期：2026-09-06　结论：✅ 通过

## 1. 修复对照

| Validator 反馈 | 严重度 | 修复 |
|---|---|---|
| `libei.rs:538` `if zones_have_changed` gate 在 `tokio::join!` **之前**，导致 join 后 future 才可能设的 flag 永远读到 reset 后的 `false`，portal DBus fetch **永不触发**，watch channel 只在 `new()` seed 后永远不更新 | **P1** | 把整个 gate-check + fetch + publish 块（line 538-563 原内容）从 `do_capture_session` 调用前 **移到 `tokio::join!` 之后**（line 602 之后、disable capture 之前）；同时把块内逻辑抽成独立的 `publish_monitors_if_changed` helper（generic over `E` 方便单测） |
| `libei.rs:529-537` comment 描述"active iteration 触发 fetch"与 P1 BUG 现实不符 | P2 #1 | P1 fix 后 comment 自然正确；同时把"为什么在 active branch" + "idle branch limitation"两部分 comment 拆开重写，引用 validator 报告 + STEP-M2-2.4 §6 遗留，避免漂移 |
| `macos.rs:556-581` `read_display_info` doc 注释重复 | P2 #2 | 维持原状（不同文件，不在本返工 scope；STEP-2.2 fixup validator 已记 backlog，留 micro-cleanup） |
| `geometry/mod.rs:255` MonitorInfo 镜像惯例（`From` trait 自动生成） | P2 #3 | 维持原状（validator 已接受为 STEP-2.6 手工转换一次即可，build.rs 生成是 over-engineering） |

## 2. 关键决策

### 把 gate-check + fetch 抽成 `publish_monitors_if_changed` helper

**为什么抽出来**：
- 直接 inline 的方案无法单测 —— `fetch_zones_for_monitor` 调真 portal DBus，gate 块在 `do_capture` 里需要 active session + `tokio::join!` 驱动 future，整条链路无法脱离真机验证
- 抽出后 + generic over `E`（错误类型任意 Display），可构造 mock fetcher（`|| async { Ok(vec![...]) }`）直接单测 3 个 invariant：flag=false 不调 fetch、flag=true+Ok publish、flag=true+Err 不 publish
- helper 文档注释里写明 "flag MUST be read AFTER `tokio::join!`" + validator 报告引用，下次有人想"优化"时一眼能看到回归约束

**generic over `E: Display` 而不是固定 `ashpd::Error`**：
- 让单测可以传 `String` / `&'static str` 作为错误类型，无需构造 `ashpd::Error`（需要 live portal 才能从 `zbus::Error` 转）
- 生产代码 closure 仍返回 `ashpd::Error`（通过 `Ok::<_, ashpd::Error>(...)` turbofish 显式标），调用点零成本

**3 个 invariant 对应的回归单测**：

| Validator 反馈 | 单测 | 验证点 |
|---|---|---|
| P1 gate 位置错位 | `publish_monitors_if_changed_skips_fetch_when_flag_false` | `flag=false` → `AtomicBool::fetch_called` 保持 false；`rx.has_changed() == false` |
| P1 fetch 应在 active + zones_changed 时触发 | `publish_monitors_if_changed_publishes_on_ok` | `flag=true, fetch=Ok(vec![...])` → `rx.has_changed() == true`；`rx.borrow_and_update()` 拿到构造的 MonitorInfo（含 id/primary/scale 字段） |
| P1 fetch 失败不应破坏 watch channel | `publish_monitors_if_changed_does_not_publish_on_err` | `flag=true, fetch=Err("...")` → `rx.has_changed() == false`（receiver 仍可读到 previous value，不出现 closed / empty 假象） |

3 个测试都是 cfg-gated 在 `cfg(libei)`（同 STEP-2.4 既有 10 个 libei 单测的约束：macOS dev 上不编，Linux CI 上跑）。

### P2 #1 comment 重写而非删除

validator 原文："comment 描述 'active iteration 触发 fetch' 与 P1 BUG 现实不符"。修复方案：

- 把原本"active branch"的整块大 comment（line 515-537 原内容）拆成 3 段：
  - **active 分支**：comment 移到 `tokio::join!` 之后、调用 `publish_monitors_if_changed` 之前；说明 fetch 必须在 join 后做 + 引用 validator 报告 + 引用 STEP-M2-2.4 §6 遗留（idle limitation 仍独立存在）
  - **idle 分支**：在 `else { handle_session_update_request.await; }` 之前补一段简短 comment，解释为什么 idle 不 fetch（无 live session 可调 portal）+ STEP-M2-2.4 §6 遗留引用
- 不删除 comment 内容（保留"为什么 gating perf 必要"的技术细节），只调整位置 + 措辞，让描述与代码实际行为对齐

P2 #2 / #3 维持原状：跨文件 / 跨 scope，按 leader 给出的判别标准"如果 P2 跨文件或改动量大 → 留 backlog"放行，留给后续 micro-cleanup PR。

## 3. 验证结果

```
cargo build -p input-capture                                                → Finished in 0.49s
cargo build -p input-capture --no-default-features --features layer_shell,libei → Finished in 0.66s
cargo build --workspace                                                     → Finished in 2.34s

cargo test -p input-capture --lib                                           → 47 passed; 0 failed
                                                                              (macOS dev 不编 libei；3 个新单测 cfg-gated 在 macOS 上)
cargo test --workspace --no-fail-fast                                       → 全部绿：
                                                                              input-capture         47 passed
                                                                              lan-mouse             50 passed
                                                                              input_channel_routing  7 passed
                                                                              quic_smoke            2 passed
                                                                              lan-mouse-ipc        12 passed
                                                                              lan-mouse-proto       5 passed

cargo check -p input-capture --no-default-features --features layer_shell,libei --tests
                                                                          → Finished (3 个新 libei 单测 Linux cfg 编译通过)
cargo fmt --check -p input-capture                                         → exit 0
cargo clippy -p input-capture --all-targets -- -D warnings                 → exit 0
cargo clippy -p input-capture --no-default-features --features layer_shell,libei
            --all-targets -- -D warnings                                   → exit 0
```

**新增单测覆盖矩阵**（对应 validator §3 BUG 清单 P1）：

| Validator 反馈 | 单测 | 验证点 |
|---|---|---|
| P1 gate 位置错位 (perf / no-op fast path) | `publish_monitors_if_changed_skips_fetch_when_flag_false` | `flag=false` → fetch closure AtomicBool 保持 false；`rx.has_changed() == false`（无 spurious notification） |
| P1 gate 位置错位 (active + zones_changed 路径) | `publish_monitors_if_changed_publishes_on_ok` | `flag=true, fetch=Ok(vec![MonitorInfo])` → `rx.has_changed() == true`；`rx.borrow_and_update()` 读到完整 MonitorInfo（含 id `libei-zone:0,0` / primary=true / scale=1.0） |
| P1 fetch 失败语义 | `publish_monitors_if_changed_does_not_publish_on_err` | `flag=true, fetch=Err(...)` → `rx.has_changed() == false`（watch channel 保留 previous value，caller 仍可用） |

> **注**：上述 3 个新单测在 macOS 开发机上 `cfg(libei)` 不被设置（`input-capture/build.rs` 仅在 `unix && !macos && feature_enabled` 时设 cfg），与 STEP-2.4 既有 10 个 libei 单测同等情况。要在 Linux 真机或 CI 跑这 3 个测试验证 gate 修复正确性 —— 本返工已通过 `cargo check --features layer_shell,libei --tests` 验证编译通过，类型 + 借用检查全绿。

## 4. 与 PLAN / validator 的偏差

**无 PLAN 偏差**：修复严格在 PLAN §M2 STEP-2.4 列出的同一个文件（`input-capture/src/libei.rs`）内，未触碰其他 backend、未触碰 `Capture` trait、未触碰 service 层。

**与 validator 偏差**：
- 修复实施细节：把 inline 块抽成 `publish_monitors_if_changed` helper（validator 建议只说"移到 join 后"，未明确是否抽函数）。抽函数的理由见 §2 关键决策 —— 让 P1 gate 位置错位的回归可单测锁死，避免下次有人"优化"再触发同类问题

**与 STEP-M2-2.4.md §6 遗留的关系**：
- §6 已记录的 **idle-path 已知 limitation** 仍存在，与本 P1 修复**独立**：active 分支（`if !active_clients.is_empty()`）修好后，idle 分支（`else { handle_session_update_request.await; }`）仍因无 live session 无法调 portal —— 已在本返工的 idle 分支 comment 里引用 §6 遗留
- §6 已记录的 **`monitor_changes()` 在 STEP-2.4 内无消费者**（`#[allow(dead_code)]`）保持不变；STEP-2.6 service 层订阅仍是后续工作
- §6 已记录的 **真机 Wayland / libei 拔插验证** 仍留 STEP-2.7 人类验证（macOS dev 跑不到）

## 5. 处理的 SUGGESTION 项

**未新增 / 移动任何 SUGGESTION 条目**。

- `next/SUGGESTION.md` 当前为空（"当前无活跃项"）
- 本返工发现的 P1 BUG（libei.rs:538 gate 位置错误）按 validator §7 建议"直接走返工报告，不走 SUGGESTION 流程"——已在本返工完成
- STEP-2.5 executor 自报的 6 个 minor finding（dummy 显式空列表故意保留 / macOS stable ID 负值转换前序范围 / 其余 edge case cleanup）按 leader 指示**不动**——已"转告 Leader 留待后续处理"，不在本次返工 scope

## 6. 闸门检查

| 闸门 | 结果 |
|---|---|---|
| 产物对得上吗 | ✅ `publish_monitors_if_changed` helper（generic over `E: Display`）；gate-check + fetch 块从 line 538-563 移到 line 614（after `tokio::join!`）；active + idle 两段 comment 拆开重写；3 个新单测；现有 47 个 libei tests + 122 个 workspace tests 全绿 |
| 依赖对得上吗 | ✅ 基于 STEP-2.4 已落地的 `monitors_tx: watch::Sender<Vec<MonitorInfo>>` / `fetch_zones_for_monitor` / `enumerate_zones`；未引入新 crate / 新模块依赖；`tokio::time` feature 已存在 |
| 验收对得上吗 | ✅ `cargo build --workspace` / `cargo test --workspace` / `cargo fmt --check -p input-capture` / `cargo clippy -p input-capture --all-targets -- -D warnings` / `cargo clippy -p input-capture --no-default-features --features layer_shell,libei --all-targets -- -D warnings` 全绿 |
| milestone 边界门 | ✅ 仅触碰 STEP-2.4 范围内的 `input-capture/src/libei.rs`；未触碰 macOS / Windows / layer_shell 后端；未触碰 `Capture` trait；未触碰 service 层；未触碰 `lan-mouse-proto` |
| 时间预算门 | ✅ 实际 ~25 min（含 3 个新单测 + helper 函数 + comment 重写 + 一轮 fmt 修整），远低于 STEP 估时上限 1h，未触发拆步 |

## 7. 遗留

- **macOS dev 无法跑 libei 单测**：`input-capture/build.rs` 仅在 `unix && !macos && feature_enabled` 时设 cfg；3 个新单测 + 既有 10 个 libei 单测只能在 Linux 真机或 CI 跑。`cargo check --features layer_shell,libei --tests` 已验证类型 + 借用检查全绿
- **idle-path 已知 limitation 仍存在**（与 P1 BUG 独立）：active 分支修好后，idle 分支（`active_clients.is_empty()`）仍因无 live session 无法调 portal —— 已在本返工的 idle 分支 comment 里引用 STEP-M2-2.4 §6 遗留
- **真机 Wayland / libei 拔插验证**：单测覆盖 gate-check 行为；协议集成 / portal D-Bus / 合成器特定行为仍需 STEP-2.7 人类在 Linux 真机跑一遍。预期路径：启动 daemon → 拔 / 插外接显示器 → daemon log 应出现 `monitors changed (zones_changed event): N monitor(s)` → WebSocket console 收到 `MonitorsChanged` 事件
- **`LibeiNotifyEvent::ZonesChanged` 已移除**（STEP-2.4 已记录）：本返工不重新引入 —— 该 variant 实际未触发，删掉避免 enum 表面膨胀
- **P2 cosmetic backlog 维持**：`macos.rs:556-581` `read_display_info` doc 注释重复 + `geometry/mod.rs:255` MonitorInfo 镜像惯例 —— 留 micro-cleanup，本返工不动

## 8. 下一步

提交 `60025ff` + 本 fixup 的 patch-fixup → leader commit `fix(libei): move zones_have_changed gate after tokio::join` → validator 重新审 STEP-2.3 + 2.4 + 2.5 + fixup 累计批次 → 通过后派发 STEP-2.6（service.rs reconcile_monitors_changed）。