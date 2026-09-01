# STEP-7.2b — 修复 Windows + tokio 1.51 上 `spawn_local` panic

> PLAN-M1 §STEP-7 收尾阶段 / 修复 panic 增量
> 执行日期：2026-09-01　实际耗时：~15 min
> 结论：通过

## 1. 做了什么

把 `quic_transport.rs` 内 **4 处** `spawn_local` 全部替换为 `spawn`，消除在 Windows + tokio 1.51 上 `#[tokio::test(flavor = "current_thread")]` + `JoinSet::spawn_local` / `tokio::task::spawn_local` 的 panic。

修改清单：

| # | 行号 (旧) | 改法 |
|---|---|---|
| 1 | 71  | `use tokio::task::{JoinHandle, JoinSet, spawn_local};` → `use tokio::task::{JoinHandle, JoinSet};`（移除 `spawn_local` import） |
| 2 | 807 | `joinset.spawn_local(async move { ... })` → `joinset.spawn(async move { ... })` |
| 3 | 854 | 同上（候选 IP 拨号分支） |
| 4 | 2054 | `let join_b = spawn_local(read_stream_b_loop(...))` → `let join_b = tokio::task::spawn(read_stream_b_loop(...))` |
| 5 | 2204 | `spawn_local(datagram_reader_task(self.clone(), tx_d))` → `tokio::task::spawn(datagram_reader_task(self.clone(), tx_d))` |

附带文档注释同步（line 779 / 1813），把"`spawn_local` 惯例"措辞改成"`spawn` 惯例"，避免误导后读代码者。

## 2. 验证结果

### 2.1 captures Send 性（每个 future 都核对）

| 行号 | 调用 | captures 类型 | Send 验证 |
|---|---|---|---|
| 807 | `JoinSet::spawn` (primary) | `ep_ref: Endpoint`、`cfg_ref: Arc<ClientConfig>`、`primary: SocketAddr` (Copy) | `quinn::Endpoint` 实现 `Send + Clone`；`Arc<ClientConfig>` Send；SocketAddr Copy |
| 854 | `JoinSet::spawn` (候选) | 同上 (addr 替换 primary) | 同 |
| 2054 | `spawn(read_stream_b_loop(bunch.b.recv, tx_b))` | `bunch.b.recv: quinn::RecvStream`、`tx_b: tokio_mpsc::Sender<StreamEvent>` | quinn `RecvStream` Send；mpsc `Sender<T: Send>` Send |
| 2204 | `spawn(datagram_reader_task(self.clone(), tx_d))` | `self.clone(): Arc<PeerSession>`、`tx_d: tokio_mpsc::Sender<StreamEvent>` | `Arc<PeerSession>` Send；mpsc `Sender` Send |

**结论**：所有 captures 都 Send，**无需重构 Rc/RefCell → Arc/Mutex**。直接换 API 即可。

### 2.2 命令 + 输出摘要

```bash
$ grep -n "spawn_local" src/quic_transport.rs
(no output)
```

```bash
$ cargo check -p lan-mouse
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 8.97s
```

- 0 errors
- 5 warnings 全是 dead-code（connect.rs `Timeout` variant、`recv_tx` field；listen.rs `Rejected` variant、`power_observer` field 等）—— 与本任务无关，已存在

```bash
$ cargo test -p lan-mouse --lib --no-run
    Finished `test` profile [unoptimized + debuginfo] target(s) in 4.37s
  Executable unittests src/lib.rs (target/debug/deps/lan_mouse-d937cb250e442c85)
```

- lib 测试可执行文件就绪，0 errors

```bash
$ cargo check --workspace
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 4.62s
```

- workspace 编译通过，0 errors（warnings 与上述一致）

## 3. 与 PLAN-M1 的偏差 / M1 边界

- **无 PLAN-M1 偏差**（纯 bug fix，沿用 quinn 0.11 + ring）
- **M1 边界** ✅：未触碰 §9 任一项（剪贴板 / h3 / ipc TransportEvent / clipboard*.rs 等）。本任务只改 spawn_local → spawn，零功能新增

## 4. 处理的 SUGGESTION 项

- 无新增 SUGGESTION（无 Leader 决策问题）
- 未触发任何 SUGGESTION 已存在条目的关闭条件

## 5. 闸门检查（PLAN-M1 § 1 时间门 / § 9 边界门）

- §1 时间门：本 STEP 实际 ~15 min（估时 5–10 min），未突破 1h 上限 — ✅
- §9 边界门：grep 当前 STEP 描述（"把所有 4 处 spawn_local 改为 spawn"）未触碰 M2 任一项 — ✅

## 6. 遗留

> ⚠️ **重要遗留（需要 Leader 决策）**：

`grep -rn spawn_local src/` 发现 **本仓还有其他文件** 仍有 `spawn_local` 调用：

| 文件 | 调用处 | 路径 |
|---|---|---|
| `src/connect.rs` | 192 / 380 / 446 | `connect_to_handle` / `spawn_peer_supervisor` / 重连拨号 |
| `src/listen.rs` | 299 / 310 / 356 | listen supervisor 主循环 |
| `src/service.rs` | 647 | 客户端定时器 / 唤醒循环 |
| `src/dns.rs` | 46 / 99 | DNS resolve task |
| `src/capture.rs` | 86 | capture task (input event capture) |
| `src/emulation.rs` | 90 / 287 | emulation task (send input event) |

用户报 Windows 上 5 个失败测试**全部**由 `quic_transport.rs` 内 `dial_any` / `PeerSession::run` / `read_loop` 触发 — 也就是本 STEP 修复的范围。但其他文件的 `spawn_local` **理论上在 Windows + tokio 1.51 上同样会 panic**（同样的 root cause），只是当前测试未覆盖。

**建议**：下一 STEP 立项统一治理（`spawn_local → spawn` 全仓 sweep），先把所有 caller 的 captures 做 Send 性 review，按需把 `Rc<...>` 改成 `Arc<...>`（参照 bak 的 `ArcConn` 模式）。Leader 决策。

## 7. 下一步

- **建议下一步**（不在本 STEP 范围）：STEP-7.2c — 全仓 `spawn_local → spawn` 治理（见 §6 遗留）
- 当前 STEP-7.2b 已提交自验清单，待用户在 **Windows** 上重跑以下命令确认 5 个失败测试全绿：

```bash
cargo test -p lan-mouse --lib -- \
  dial_any_all_unreachable_returns_err \
  dial_any_prefers_primary \
  peer_session_round_trip_motion_keyboard \
  hello_wrong_magic_closes_connection \
  stream_c_take_releases_quinn_recv_stream
```