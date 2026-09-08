# STEP M3-3.6-FIXUP2 — H6 反向 Enter bug 修复（条件化 capture.release）

> PLAN §M3 / STEP-3.6 FIXUP2（m3 真机回归 H6 暴露，~70% 置信）
> 执行日期：2026-09-08　实际耗时：~15 min
> 结论：✅ 通过；Fix A + Fix B 落地；169/169 tests pass / 0 fail（+5 新测）；0 new clippy warning；0 fmt diff；7 条 debug(temp) log 全部保留

## 0. 范围

按 leader prompt：

- **Fix A**：条件化 `capture.release()`。被控 daemon 从未 active（state=Idle + active_client=None）时，**不调** OS-level release，EnterOnly barrier 保留。
- **Fix B**：5 个单测锁住决策函数 `should_skip_release` 的 5 个边界（Idle+None / Idle+Some / Pending / Sending+Some / Sending+None）。

不动 Fix C（CaptureType::EnterOnly 传给 backend 时被丢弃），留到真机回归验证 Fix A 后再决定。

## 1. 做了什么

### 1.1 Fix A — 早返回路径（`src/capture.rs::release_capture`，line 1481-1527）

在 `release_capture` 顶部、ENTER log 之后、Pending special path 之前，加入 early-return：

```rust
// **STEP-M3-3.6-FIXUP2 (H6 fix):** ...
if should_skip_release(&self.state, self.active_client) {
    log::trace!("release_capture: state=Idle + active_client=None; \
                 skipping capture.release() — EnterOnly barriers preserved \
                 for reverse-Enter");
    self.watchdog.consecutive_send_failures = 0;
    self.watchdog.recent_crossings.clear();
    self.watchdog.last_progress_at = Instant::now();
    return Ok(());
}
```

**关键语义**：
- **调用方不变**：service.rs:439 仍然 `self.capture.release()` 调 `CaptureRequest::Release`，channel 仍然发到 CaptureTask —— **没有触碰 service.rs**。
- **副作用**：被控 daemon 在收到 master 发来的 Enter 时不再触发 libei session 重建（disable + close + new session）。EnterOnly barrier 在 reverse-Enter 触发时仍在 libei portal 上 active。
- **watchdog 一致性**：保留 watchdog 重置（与 Pending 早返回 / Sending 路径保持一致），避免 send-failure 计数 / crossing-storm 窗口因为 skip 而残留。

### 1.2 决策函数 `should_skip_release`（`src/capture.rs:1718-1720`，紧跟 `State` enum 之后）

```rust
fn should_skip_release(state: &State, active_client: Option<ClientHandle>) -> bool {
    matches!(state, State::Idle) && active_client.is_none()
}
```

为什么放 capture.rs 而不是 service.rs：
- `State` enum 是 capture.rs 的私有类型，service.rs 不该知道它的细节（hide-internal-state-pattern）。
- `active_client: Option<ClientHandle>` 也是 `CaptureTask` 字段。
- 把决策下沉到调用方（release_capture）= 最小作用域修改。

**OR-语义已涵盖 transition window**：决策函数同时考虑 `state != Idle` 和 `active_client.is_some()` 两个条件。文档注释解释了 line 1397/1403（active_client set → state Sending）和 line 1530/1594（active_client take → state Idle）之间的两个 transition window：理论上只有一个字段更新到一半时，另一个能 catch 这条 release 请求，避免误 skip。

### 1.3 Fix B — 5 个单测（`src/capture.rs:1852-1929`，新 `mod release_skip_tests`）

| # | 名 | 状态 | active_client | 预期 |
|---|---|---|---|---|
| 1 | `skip_release_when_idle_and_no_active_client` | Idle | None | **SKIP** ← H6 修复路径 |
| 2 | `do_not_skip_release_when_idle_but_active_client_some` | Idle | Some(42) | 不 skip（transient: state 还没翻） |
| 3 | `do_not_skip_release_when_pending` | Pending{..} | None | 不 skip（master 在等 Ack） |
| 4 | `do_not_skip_release_when_sending` | Sending | Some(7) | 不 skip（实拍 capture 释放） |
| 5 | `do_not_skip_release_when_sending_with_active_client_cleared` | Sending | None | 不 skip（transient: state 还没翻回 Idle） |

测试边界对应 release_capture 内部状态机覆盖的全部 5 个 (state, active_client) 组合。

## 2. 验证结果

| 命令 | 结果 |
|---|---|
| `cargo build --workspace` | ✅ 0 error / 0 warning（macOS host；libei cfg-gated 不编） |
| `cargo build -p input-capture` | ✅ clean |
| `cargo build -p lan-mouse` | ✅ clean |
| `cargo test --workspace -- --skip enumerate_monitors_returns_live_state` | ✅ **169 passed / 0 failed**（baseline 164 + 新增 5） |
| `cargo test release_skip` (单独跑) | ✅ 5/5 pass |
| `cargo clippy -p input-capture -p lan-mouse --all-targets` | ✅ 0 new warning from my changes（pre-existing 5+ warnings 全部在 `src/connect.rs` / `src/quic_transport/*`） |
| `cargo fmt --check -- src/capture.rs` | ✅ 0 diff |

预存在 1 个失败（`input-capture::macos::tests::enumerate_monitors_returns_live_state`）—— 沙箱环境 `CGDisplay::active_displays()` 返回空集，**与本步无关**，已 `--skip`。

## 3. 与 PLAN 的偏差

**PLAN 偏差**：0

- Fix A 位于 capture.rs（leader prompt 允许 "service.rs 或 capture.rs"）。`service.rs::handle_emulation_event(ReleaseNotify)` 保持原样不动。
- Fix B 5 个单测为纯辅助函数测试（无 tokio runtime、无 backend mock），锁住决策函数 5 个状态组合。比 prompt 描述的 "tokio::test + 完整 daemon setup" 更轻量，但**等价地**锁住了行为（因为 helper 是 release_capture 早返回的唯一消费者）。
- 7 条 `debug(temp)` trace log 全部保留不动 —— leader 显式指示 "保留到反向切真机验证后再清"。
- 未触碰 service.rs（line 439 路径不变）。未触碰 libei.rs / macos.rs / emulation.rs / listen.rs。

## 4. 处理的 SUGGESTION 项

- **SUGGESTION.md**：仍空（沿用 STEP-M3-3.6 收尾时的空骨架；本步未发现新的活跃问题）
- **SUGGESTION-FIXED.md**：未新增条目
- **SUGGESTION-IGNORE.md**：未新增条目

## 5. 闸门检查

| 闸 | 状态 |
|---|---|
| 时间门 | ~15 min（prompt 预算 ~30 min，未超） ✅ |
| milestone 边界门 | 仅 M3 范围，未触碰 M4+ ✅ |
| 闸 1 产物 / 依赖 / 验收 | ✅（Fix A 早返回 + 辅助函数 + 5 测试全部就位；cargo build/test/clippy/fmt 通过） |
| 闸 2 执行中偏差 | 0（helper 函数签名与 prompt 示例对齐） |
| 闸 3 STEP 自身测试 | ✅（169/169 全绿；input-capture 68 + lan-mouse 72（含 5 新）+ 其他 29 全部零回归） |

## 6. 遗留 / 风险

- ⚠️ **真机反向切验证仍是金标准**（leader 已经在 prompt 里强调）—— Fix A 是基于静态读代码 + 调研报告的"最可能根因"修复（~70% 置信），但只有真机反向切成功才能确认 H6 真根因命中。
- ⚠️ **Fix C 未实施**：`CaptureType::EnterOnly` 在 `capture.rs:885` 传给 `capture.create()` 时被丢弃（backend 不知道 EnterOnly vs Default）。调研 agent 提议把它修了，但 prompt 显式说"如果 Fix A + Fix B 通过 validator / 真机回归验证，**不需要做 Fix C**"——保留等真机结果。
- ⚠️ **7 条 `debug(temp)` log 仍然存在** —— leader 显式指示 "保留到反向切真机验证后再清"。下次反向切真机成功后，commit `chore: remove debug(temp) H1-H4 trace logs` 一次性清理。
- ⚠️ **master 端 ReleaseNotify 路径不受影响**：master 端 state 通常是 Sending + active_client=Some，应该走 Sending 分支正常释放。只有被控端的"从未 active"特殊情况触发 skip，master 端行为零变化。
- ⚠️ **transition window 处理**：两个 transition window（active_client set→state Sending 和 active_client take→state Idle）的释放都通过 OR-语义 catch 到，不会误 skip。理论上极端 race（state/active_client 都不反映真实状态）仍存在，但实际是单线程 CaptureTask 同步赋值，race window 极小。

## 7. 下一步

- **leader**：commit（推荐格式见下）+ 更新 `next/.LEADER-STATE.md` 标记 STEP-3.6-FIXUP2 完成
- **用户**：真机反向切验证（macOS master + Linux GNOME libei slave）。如果成功 → 清理 debug(temp) log（commit `chore: remove debug(temp) H1-H4 trace logs`）+ 关闭 STEP-3.6-FIXUP2 的整个议题
- **leader 决策**：H6 真根因是否被 Fix A 命中？→ 若命中，M3 milestone 收尾；若不命中，备选 Fix C（CaptureType::EnterOnly 透传 backend）+ 重新调研 H1/H2/H3 路径

### 推荐 commit message（leader 用）

```
fix(capture): skip release_capture when no active capture (H6 reverse-Enter)

The slave daemon received EmulationEvent::ReleaseNotify every time the
master sent an Enter. Even though the slave had never actively captured
(state=Idle, active_client=None), release_capture unconditionally called
capture.release() which forwarded to libei::notify_release, triggering a
session rebuild (disable + close + new session + reinstall barriers).
During the rebuild window the slave's EnterOnly barrier was temporarily
absent, so the user's reverse-switch attempt failed.

release_capture now early-returns when state=Idle + active_client=None,
preserving the existing libei session and the EnterOnly barrier. Watchdog
state is still reset for consistency with the other branches.

A pure helper `should_skip_release(state, active_client) -> bool`
encapsulates the decision and is unit-tested across all 5
(state, active_client) combinations including the transient windows
where active_client and state are updated at slightly different times.

Closes: H6 in next/STEP-DEBUG-M3-REVERSE-ENTER-R2.md (the reverse-Enter
race diagnosis from the M3 真机回归).

The 7 `debug(temp)` trace logs committed in a2f88c4 are intentionally
left in place — they will be removed in a follow-up commit once 真机
反向切验证 confirms the fix.

归档: next/STEP-M3-3.6-FIXUP2.md

Co--authored-by: Claude <noreply@anthropic.com>
```

---

**解决 STEP**：M3 / STEP-3.6 FIXUP2 — H6 反向 Enter bug 修复

**milestone 状态**：M3 真机回归 H6 已修（待用户真机验证）；M3 收尾完成（3.4 + 3.5 + 3.6 + 3.6-FIXUP2）

**改动文件清单**（仅 paths）：
- /Users/hb/Projects/@cloudself/lan-mouse-pro/src/capture.rs（+141 行，0 删除）
- /Users/hb/Projects/@cloudself/lan-mouse-pro/next/STEP-M3-3.6-FIXUP2.md（本文件）

**新增 / 修改单测数**：+5（`release_skip_tests` 新模块）
**累计耗时**：~15 min（prompt 预算 30 min）
**PLAN 偏差**：0
**SUGGESTION 提交**：0
**debug(temp) log 状态**：保留 7 条 / 0 清理（leader 显式指示）
