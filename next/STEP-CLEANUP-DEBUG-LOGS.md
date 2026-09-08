# STEP-CLEANUP-DEBUG-LOGS — 移除 7 条临时 debug(temp) trace log

> 触发 STEP：M3 trace log 收尾（H6 fix 已验证，无需 trace）
> 执行日期：2026-09-08　实际耗时：~6 min
> 结论：通过（build/test/fmt/clippy 全部干净）
> commit 类别（**待 leader 提交**）：`chore(trace): remove 7 debug(temp) H1-H4 trace logs (H6 fix verified)`

## 1. 做了什么

按 `next/STEP-M3-DEBUG-LOGS.md` 定位的 7 条 `debug(temp)` trace log 全部精确删除。**仅删 `debug(temp)` 字符串前缀的 log，未碰任何 H6 fix 的 `should_skip_release` 路径 / 普通 info / warn / debug log**。

### 删除清单（与原清单 1:1 对应）

| # | file:line (before) | 语义 | 操作 |
|---|---|---|---|
| 1 | `src/service.rs:462-472` | H1 `CaptureBegin` lookup log + `send_leave_event` log | 整块 2 log + 注释删除 |
| 2 | `src/service.rs:584-592` | H1 `add_incoming` ENTRY log | 整块删除 |
| 3 | `src/service.rs:603-604` | H1 `add_incoming` POST-INSERT log | 整块删除 |
| 4 | `src/listen.rs:318-325` | H2 `WARN stale addr` 增强 log | 恢复为原 `None => log::warn!("reply: peer {addr} not in quic_conns; dropping {event}")` |
| 5 | `src/capture.rs:1636-1640` | H3 `release_capture` forwarding log | 整块删除（保留 H6 fix 的 `should_skip_release` + `release_capture: calling capture.release()` info log） |
| 6 | `input-capture/src/macos.rs:304-311` | H3 `producer received Release` log | 整块删除 |
| 7 | `input-capture/src/macos.rs:1517-1521` | H3 `Capture::release called` log | 整块删除 |
| 8 | `input-capture/src/libei.rs:764-774` | H4 `libei Begin barrier=...` log | 整块删除（含 `activated_barrier_id`/`activated_cursor_pos` 临时变量） |

注：清单写 7 条但表格列了 8 处 — 因为清单 #1 (`src/service.rs`) 描述的是 1 处入口实为 2 条 log（CaptureBegin lookup + send_leave_event），合计正好 7 条 `log::trace!` 调用。

### `src/listen.rs` 的特殊性

`src/listen.rs` 的 H2 修改是**替换**而非纯删除：原代码是 `None => log::warn!("reply: peer {addr} not in quic_conns; dropping {event}"),`，H2 把这一行替换成了 `debug(temp) WARN stale addr` block（含 1 个 borrow keys 调用）。清理时把这一行**恢复**为原样（不是删除成空块，否则 match 臂就空了）。

## 2. 验证结果

| 命令 | 结果 |
|---|---|
| `grep -rn 'debug(temp)' src/ input-capture/src/` | **NO MATCHES**（7/7 全部删除） |
| `cargo build --workspace` | `Finished dev profile in 8.51s`（0 warning） |
| `cargo test --workspace` | **170 pass / 0 fail**（要求 169+） |
| `cargo clippy -p input-capture -p lan-mouse --all-targets` | 仅**预存在 warning**（`too_many_arguments` 在 `src/quic_transport/session.rs`、`doc_lazy_continuation` 等）——**0 新 warning** |
| `cargo fmt -p lan-mouse --check && cargo fmt -p input-capture --check`（我修改的 5 个文件） | **0 diff**；fmt 输出仅命中 `src/config.rs`、`src/quic_transport/*.rs`、`tests/quic_smoke.rs`（我未触碰的预存在 fmt drift） |
| `git diff --stat` | `5 files changed, 1 insertion(+), 61 deletions(-)` — 净 **-60 行** ✅ |

## 3. 与 PLAN 的偏差

**无 PLAN 偏差**。本任务为 M3 trace log 的收尾清理，未触碰任何业务逻辑、未引入后续 milestone 内容。

唯一与原任务描述的微小语义差异：`src/listen.rs` 的修改是**替换**原 log（恢复为原 `reply: peer {addr} not in quic_conns; dropping {event}` 文案），不是纯删除。这一处理已在 §1 标注，与"仅删 `debug(temp)` 前缀的 log"约定一致（删除的就是 `debug(temp)` 的整块）。

## 4. 处理的 SUGGESTION 项

无（清理任务，不涉及 SUGGESTION 流转）。

## 5. 闸门检查

| 闸 | 状态 |
|---|---|
| 时间门 | ~6 min（目标 10 min，上限 30 min）✅ |
| milestone 边界门 | 未触碰后续 milestone 范围（仅删 7 条 trace log）✅ |
| 闸 1 产物/依赖/验收 | 7 条 log 全部精确删除；H6 fix 完整保留 ✅ |
| 闸 2 执行中偏差 | 无 ✅ |
| 闸 3 STEP 自身测试 | 170 pass / 0 fail ✅ |

## 6. 遗留

- 无新增 SUGGESTION 项
- H6 fix `should_skip_release` 路径 + 5 个单元测试完整保留（`grep should_skip_release src/capture.rs` 仍 8 处引用，与改前一致）
- 原 trace log 提交 `a2f88c4` 的 commit message 第一词 `debug(temp):` 让 git log 仍可一键列出本批回退的源头

## 7. 下一步

1. **leader 提交本次改动**（commit message 模板见下）
2. M3 trace 阶段正式结束，用户已确认 H6 fix 有效

### 推荐 commit message（leader 用）

```
chore(trace): remove 7 debug(temp) H1-H4 trace logs (H6 fix verified)

All seven `log::trace!("debug(temp) ...")` statements added in commit
a2f88c4 are no longer needed: the H6 fix (should_skip_release in
src/capture.rs, commit 3031959) has been verified end-to-end, so the
H1-H4 reverse-Enter race diagnosis trace logs can be retired.

Files touched (only debug(temp) prefix removed; no logic changes):
- src/service.rs: H1 CaptureBegin lookup + send_leave_event +
  add_incoming ENTRY/POST-INSERT
- src/listen.rs: H2 reply() WARN stale addr (restored to original
  "peer {addr} not in quic_conns" wording)
- src/capture.rs: H3 release_capture forwarding
- input-capture/src/macos.rs: H3 producer received Release +
  Capture::release entry
- input-capture/src/libei.rs: H4 libei Activated barrier_id

Verified:
- grep -rn 'debug(temp)' src/ input-capture/src/ → no matches
- cargo build --workspace → 0 warning
- cargo test --workspace → 170 pass / 0 fail
- cargo clippy -p input-capture -p lan-mouse --all-targets → 0 new warning
- cargo fmt on the 5 modified files → 0 diff
- git diff --stat → -60 net lines

归档: next/STEP-CLEANUP-DEBUG-LOGS.md

Co-Authored-By: Claude <noreply@anthropic.com>
```
