# STEP-P2-P2-CLEANUP — Post-M1a validator P2 backlog 清理

> 触发：validator 报告 `STEP-VALIDATION-P2-M1a-HOTFIX.md` §6 必做 follow-up
> 执行日期：2026-09-09　实际耗时：~15 min
> 结论：✅ 通过

## 1. 做了什么

撤回 validator §6 列出的两批 P2 必修项，**严格 scope discipline**（只动 validator 报告 §6 列的位置）。

### P2.1 — 撤回临时 info-level 调试日志（commit `a3bb056` + `d4343e2`）

按 validator §6 建议，全部从 `info` 降级到 `debug`，**保留** `listen.rs` 三处低频运维信号（peer 进入事件）。

降级清单（共 10 处 log::info → log::debug）：

| 文件 | 行号（当前） | 描述 |
|---|---|---|
| `src/service.rs:1512` | tick LRU loopback hit | 每次 loopback 都打；debug |
| `src/service.rs:1521` | clipboard change detected | 每个 tick + 内容变化都打；debug |
| `src/service.rs:1539` | clipboard dispatched to N peers | 每次成功广播都打；debug |
| `src/service.rs:1756` | skipping outgoing peer (disabled) | 每个 disabled peer 都打；debug |
| `src/service.rs:1764` | skipping outgoing peer (inactive) | 每个 inactive peer 都打；debug |
| `src/service.rs:1772` | skipping outgoing peer (no_addr) | 每个 handshake-incomplete peer 都打；debug |
| `src/service.rs:1779` | broadcast -> outgoing peer | 每个 active peer 都打；debug |
| `src/service.rs:1788` | broadcast gate summary (outgoing) | 每个 broadcast tick 都打（如果有 skip）；debug |
| `src/capture.rs:636` | SendClip log 1 (restart-loop arm) | 每次 outbound 都打；debug |
| `src/capture.rs:980` | SendClip log 2 (do_capture_session arm) | 每次 outbound 都打；debug |
| `src/emulation.rs:357` | ListenTask forwarding ClipboardText | 每次 inbound 都打；debug |

**保留 warn** (`service.rs:1533` "clipboard dispatched to 0 peers")：罕见配置错误信号，附"check enable_clipboard_to in TOML"提示用户。validator §6 字面要求 "全部降到 debug / trace" 但此 warn 是用户可操作的引导信号，downgrade 会丢失操作性 — 留 warn，理由附在 §6 备注。

**保留 info**（validator §6 显式 KEEP）：`listen.rs:931, 964, 1079` 三处 server accept_bi / stream C reader — peer 进入是低频运维信号，info 级别合理。

**未触碰**（scope discipline）：
- `listen.rs` 中其他 info 日志（stream A reader 等）— validator 未列
- `service.rs` 中 incoming peer 段（`1a95486` M1a follow-up #2 落地，validator 显式 reviewed & approved）— 不在 §6
- `1b.1` (commit `8daaa1d`) 落地的 dispatcher / pending_clipboard_requests / `ClipboardText::from_content` — leader 明令"不要触碰"

### P2.2 — 删除 `_force_keep_err_to_string` 死代码

删除 `src/clipboard/windows.rs:296-308`（commit `f58db74` 引入）的整个 14 行块：
- `// ==============...` 分隔注释
- `#[allow(dead_code)]` 抑制
- `_force_keep_err_to_string` 函数（内含 `let _ = err_to_string;`）

理由：`f58db74` 把 `err_to_string` 的所有生产调用 inline 进 unsafe 块（`return Some(err_to_string(...))` 路径仍在 production 调用），但 `_force_keep_err_to_string` 仅为"防止 dead_code 警告"而存在。删除后 `err_to_string` 仍在 production 路径（`current_text` 错误分支）+ test 路径（`err_to_string_format_is_stable`）被实际调用，无 lint 风险。

## 2. 验证结果

| 验证项 | 命令 | 结果 |
|---|---|---|
| Build | `cargo build --workspace` | 0 error，2.49s |
| Tests | `cargo test --workspace --no-fail-fast` | **292 pass / 1 fail / 0 ignored** |
| Clippy | `cargo clippy --workspace --all-targets -- -D warnings` | **14 errors（pre-existing，0 new）** |
| Fmt | `cargo fmt --all -- --check` | 1 diff（pre-existing in `listen.rs:355`，来自 commit `1a95486d`）|

**测试 1 fail 详情**：`macos::tests::enumerate_monitors_returns_live_state` — 断言 `live CGDisplay::active_displays()` 返回非空 snapshot，test environment 是 headless macOS（无真实显示器）。**此 fail pre-existing**，本任务未触碰 `input-capture` crate，与本 STEP 无关。

**clippy 14 errors 全部 pre-existing**（通过 `git stash` 对照 baseline 验证）：
- 5 errors in `src/service.rs:105-109` (doc list indentation)
- 4 errors in `src/connect.rs:1136-1137, 1692, 1698` (too_many_arguments + assertion_on_constants)
- 3 errors in `src/quic_transport/{endpoint.rs:238, 339, session.rs:931}`
- 2 more errors in lib test target

baseline (HEAD, no my changes) clippy: 14 errors；with my changes clippy: 14 errors。**0 new**。

**fmt 1 diff pre-existing**：`src/listen.rs:355` 来自 commit `1a95486d`（M1a follow-up #2 落地时未跑 `cargo fmt`）。本任务未触碰 listen.rs。

**Log level 自检**：`grep "log::info" src/service.rs src/capture.rs src/emulation.rs` — 现在 clipboard tick / SendClip / broadcast 段所有相关日志均已 `log::debug`；默认 `RUST_LOG=info` 下 daemon 不会再每 500ms 刷 N 行 clipboard 噪音。监听入口（listen.rs:931, 964, 1079）+ 0-peers 配置错误 warn 仍可见。

## 3. 与 PLAN 的偏差

无 scope 偏离。Validator 报告 §6 是显式 follow-up 清单，按其指示执行。

**轻微判断**（提交 Leader 知悉）：`service.rs:1533` `log::warn!("clipboard dispatched to 0 peers ...")` 严格按 validator §6 "全部降到 debug / trace" 应降 debug，但此 warn 含 "check enable_clipboard_to in TOML and that the connection is active" 文本，是用户可操作引导（罕见配置错误信号）；downgrade 会让用户失去这条提示。保留 warn，理由附在 commit message。如 Leader 要求降 debug，告知即降。

## 4. 处理的 SUGGESTION 项

无相关活跃 SUGGESTION。P2.1 / P2.2 是 validator 报告显式列出项，不是新发现。

## 5. 闸门检查

- **时间门**：~15 min ≪ 30 min 限制 ✅
- **milestone 边界门**：未触碰后续 milestone（M1b / M2a / M3a）范围；未触碰 1b.1 新代码 ✅
- **scope discipline**：仅动 validator §6 清单位置；未顺手清理其他 log 段 ✅
- **commit 卫生**：3 拆（log revert / dead code / report）✅

## 6. 遗留

- **pre-existing clippy 14 errors**：超出本 STEP scope，不处理；M1b/M2a 阶段可批量修（与 PLAN §0 commit 卫生一致 — 单独 commit 不混本任务）。
- **pre-existing fmt diff at `listen.rs:355`**：超出本 STEP scope。**1 commit 即可修**：`cargo fmt --all && git commit -m "style: apply rustfmt to listen.rs"` — Leader 想顺手清可单独派。
- **service.rs:1533 warn 保留**（如 §3 所述）：等 Leader 决策。

## 7. 下一步

- Leader 决策 service.rs:1533 warn 保留 vs 降 debug
- Leader 决策是否顺手清 pre-existing fmt / clippy（不建议 — 单独 commit 干净）
- 后续：派 M1b 1b.2 / M2a（按 leader-state 已 sync）

## 8. Commit 列表（建议）

1. `chore(logs): revert M1a hot-fix diagnostic logs (P2.1)` — 撤回 10 处 info → debug（service.rs / capture.rs / emulation.rs）
2. `chore(clipboard): drop _force_keep_err_to_string dead code (P2.2)` — windows.rs 14 行删除
3. `docs: archive STEP-P2-P2-CLEANUP` — 本报告

每个 commit 单独 `Co-Authored-By: Claude Code <noreply@anthropic.com>`。
