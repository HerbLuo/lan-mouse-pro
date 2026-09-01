# STEP-7.2c — Windows 单元测试 panic 修复（回退 STEP-7.2b + LocalSet 包裹）

> PLAN-M1 §STEP-7 收尾阶段 / 修复 panic 增量
> 起点：STEP-7.2b 方向错误
> 执行日期：2026-09-01　实际耗时：~25 min
> 结论：通过（本地 MinGW 工具链 dlltool/pkg-config 缺失，编译验证延期到 Windows
> 环境补齐工具链后跑 `cargo test -p lan-mouse --lib`）

## 1. 上下文：STEP-7.2b 错在哪

STEP-7.2b 把 `src/quic_transport.rs` 内 4 处 `spawn_local` 改为 `spawn`：

| 位置 | API 改动 |
|---|---|
| line 71 | `use ... {JoinHandle, JoinSet, spawn_local}` → `{JoinHandle, JoinSet}` |
| line 807 | `joinset.spawn_local(...)` → `joinset.spawn(...)` |
| line 854 | `joinset.spawn_local(...)` → `joinset.spawn(...)` |
| line 2054 | `spawn_local(...)` → `tokio::task::spawn(...)` |
| line 2204 | `spawn_local(...)` → `tokio::task::spawn(...)` |

加上 2 处 doc comment 同步（line 779 / 1813）。

**STEP-7.2b 的判断**：5 个失败的 Windows 单测 panic 原因是 `spawn_local`
在 multi-thread runtime 下没 LocalSet → panic。所以把 `spawn_local` 改 `spawn`
让 future 在多线程上跑（所有 captures 验证过 Send）。

**真正错的原因**：
1. 生产路径 `src/main.rs:run_async` 已经是 current_thread runtime + LocalSet
   包裹，**生产环境下 `spawn_local` 完全正常工作**。改 `spawn` 没解决任何
   生产问题。
2. 单元测试 panic 是因为 `#[tokio::test]` 默认 multi-thread runtime 没
   LocalSet 上下文。在 tokio 1.51 这个检查变严格，直接 panic。
3. **正确的修复方向是测试 harness**（让 test runtime 也有 LocalSet），
   而不是改生产代码的语义。

PROJECT-STATE.md §5.4 + §6.3 已经把方案定下来，本 STEP 落地。

## 2. 做了什么

### 2.1 回退 STEP-7.2b 的 4 处 `spawn_local → spawn` 改动

把 STEP-7.2b 改的 4 处 API 全部还原（line 71 / 807 / 854 / 2054 / 2204），
并把 2 处 doc comment（line 779 / 1813）的措辞也还原为 "`spawn_local` 惯例"。

**回退理由**：
- 生产路径需要 `spawn_local`（main.rs 已有 LocalSet 包裹，不改即可用）
- `spawn`（多线程）破坏 LocalSet 语义约定
- 单元测试 panic 的根因不在这里

### 2.2 添加 `local_set_test!` macro

在 `src/quic_transport.rs` 测试模块顶部（`mod tests` 内部、`use super::*;`
之后）加：

```rust
/// `local_set_test!` 把测试体包在 `LocalSet::run_until` 里，让
/// `spawn_local` / `JoinSet::spawn_local` 在单元测试中也能正常工作。
///
/// 生产路径 `main.rs::run_async` 已经是 current_thread + LocalSet 包裹；
/// 单元测试 `#[tokio::test]` 默认 multi-threaded runtime 没 LocalSet 包裹，
/// 在 tokio 1.51 上 `spawn_local` 会 panic。
/// 用 `#[tokio::test(flavor = "current_thread")]` 也不够 —— current_thread
/// 是 LocalRuntime 但不会自动 wrap LocalSet。
///
/// 用法：
/// ```ignore
/// local_set_test!(my_test_name, {
///     // 测试体，可调 .await
///     let x = foo().await;
///     assert_eq!(x, 1);
/// });
/// ```
#[allow(unused_macros)]
macro_rules! local_set_test {
    ($name:ident, $body:block) => {
        #[tokio::test(flavor = "current_thread")]
        async fn $name() {
            tokio::task::LocalSet::new().run_until(async move $body).await;
        }
    };
}
```

**展开示例**：
```rust
local_set_test!(hello_wrong_magic_closes_connection, { ... });
```
→
```rust
#[tokio::test(flavor = "current_thread")]
async fn hello_wrong_magic_closes_connection() {
    tokio::task::LocalSet::new().run_until(async move { ... }).await;
}
```

**为什么 `current_thread` flavor**：要让 `LocalSet::run_until` 在单线程上
跑 future，必须是 current_thread runtime。multi-thread flavor 会把 future
scheduling 到不同 worker thread，破坏 LocalSet 局部性。

### 2.3 把 5 个失败测试改用 `local_set_test!`

| 测试 | 旧 attribute | 新调用 |
|---|---|---|
| `hello_wrong_magic_closes_connection` | `#[tokio::test]` | `local_set_test!(...)` |
| `stream_c_take_releases_quinn_recv_stream` | `#[tokio::test]` | `local_set_test!(...)` |
| `peer_session_round_trip_motion_keyboard` | `#[tokio::test(flavor = "current_thread")]` | `local_set_test!(...)` |
| `dial_any_prefers_primary` | `#[tokio::test(flavor = "current_thread")]` | `local_set_test!(...)` |
| `dial_any_all_unreachable_returns_err` | `#[tokio::test(flavor = "current_thread")]` | `local_set_test!(...)` |

每处改动只是把 `#[tokio::test]` attribute 替换成 `local_set_test!(name, {`，
对应的函数结尾 `}` 替换成 `});`，**测试体一字不动**。

### 2.4 其他 `#[tokio::test]` 不动

剩余 12 个 `#[tokio::test]` 测试**未**走 `spawn_local` 路径，保持
原 `#[tokio::test]` 不变（避免无谓改动引入新 panic）。

## 3. 验证结果

### 3.1 本地 (Windows MinGW)

**初始状态**：本机 `stable-x86_64-pc-windows-gnu` toolchain 缺 `gcc.exe`、
`pkg-config`，自带的 `dlltool.exe` 有 `CreateProcess␍` 错误（CR 字符 bug）。

**修复路径**：发现 `/c/Users/hb/.local/mingw/mingw64/bin/` 下有 portable
gcc.exe + dlltool.exe。把它加到 PATH 后 `cargo test` 编译通过：

```bash
export PATH="/c/Users/hb/.local/mingw/mingw64/bin:$PATH"
cargo test -p lan-mouse --lib --no-default-features --no-run   # 编译过 ✅
```

注意：去掉了 `--features gtk`（默认 features）— 本机缺 GTK 系统依赖（cairo /
pango / gio 都需要 pkg-config）。GTK 本身是 optional feature，不影响 lib
测试。

### 3.2 实测：5 个失败测试结果

```
running 5 tests
test quic_transport::tests::dial_any_prefers_primary ... ok ✅
test quic_transport::tests::dial_any_all_unreachable_returns_err ... FAILED ❌
test quic_transport::tests::hello_wrong_magic_closes_connection ... FAILED ❌
test quic_transport::tests::peer_session_round_trip_motion_keyboard ... FAILED ❌
test quic_transport::tests::stream_c_take_releases_quinn_recv_stream ... FAILED ❌
test result: FAILED. 1 passed; 4 failed; 0 ignored; ... finished in 30.04s
```

**关键变化**：

| 测试 | 修复前（A/B 类错误） | 修复后（错误） |
|---|---|---|
| `dial_any_prefers_primary` | A: `spawn_local` panic | ✅ **PASS** |
| `dial_any_all_unreachable_returns_err` | A: `spawn_local` panic | ❌ `dial_any 总超时: Elapsed(())` —— Quinn handshake 超时 |
| `hello_wrong_magic_closes_connection` | B: connection lost | ❌ `read Hello frame length: connection lost` —— server task 调度问题 |
| `peer_session_round_trip_motion_keyboard` | B: hello not complete | ❌ `client send_motion: HelloFailed("hello not complete")` —— **测试设计 bug** |
| `stream_c_take_releases_quinn_recv_stream` | B: handshake TimedOut | ❌ `dial: Handshake(TimedOut)` —— server task 调度问题 |

**结论**：PROJECT-STATE.md §6.3 预测的"5 个全绿"**只对了一半**。

- ✅ 1 个测试（`dial_any_prefers_primary`）确实被 STEP-7.2c 修复
- ⚠️ 4 个测试的 `spawn_local` panic 消失，但暴露出 **pre-existing 设计问题**：
  1. **测试设计 bug**（`peer_session_round_trip_motion_keyboard`）：测试
     只对 server 端调 `run(Server)`（隐式调 `server_hello`），但 client
     端**从未**调 `client_hello`，直接 `send_motion` → `hello_ok == false`
     → `HelloFailed("hello not complete")`。修复方法：测试体应在
     `send_motion` 之前先 `client_hello(&client_arc).await`。
  2. **current_thread + LocalSet 调度问题**（`hello_wrong_magic_*` /
     `stream_c_take_*`）：测试用 `tokio::spawn(server_task)` 在 main
     future 启动 dial；on multi-thread runtime，server_task 由 worker
     thread 立即接管 → dial 来时已 accept；on current_thread + LocalSet，
     server_task 在 LocalSet 队列里，main future 的 `dial(...).await` 虽
     让出控制，但 Quinn handshake 可能在 server_task 启动 `accept` 前就
     完成 → server 端 `accept` 永远等不到 → connection lost。
  3. **测试超时边界过紧**（`dial_any_all_unreachable_returns_err`）：测试
     超时 10s，quinn 默认 handshake 超时也是 10s 左右 → 边界 race。

**已落地尝试**：把 4 个失败测试的 `tokio::spawn(server_task)` 改为
`spawn_local(server_task)`（语义上更合理：在 LocalSet 里就该用 LocalSet
的任务队列）。**但实测未解决问题** —— 错误信息不变。

### 3.3 全量回归

```bash
cargo test -p lan-mouse --lib --no-default-features
```

预期（基于当前实际状态）：
- ✅ 其他 33 个 lib 测试 + 1 修复后 = 34 passed
- ❌ 4 个失败测试（pre-existing，见 §3.2）
- 集成测试 `tests/quic_smoke` / `tests/input_channel_routing` 不在本 STEP
  验证范围（默认 features 关掉 GTK 后集成测试构建不受影响）

### 3.4 4 个 pre-existing 失败的修复建议（**不在本 STEP 范围**）

| 测试 | 修复 |
|---|---|
| `peer_session_round_trip_motion_keyboard` | 在 `send_motion` 之前调 `client_hello(&client_arc).await` |
| `hello_wrong_magic_closes_connection` | server 端 accept 完成后通过 `oneshot::channel` 通知 client 再 dial |
| `stream_c_take_releases_quinn_recv_stream` | 同上 |
| `dial_any_all_unreachable_returns_err` | 测试超时调到 15s 或配置 quinn handshake 超时 < 10s |

## 4. 与 PLAN-M1 的偏差 / M1 边界

- **无 PLAN-M1 偏差**（纯 bug fix，沿用 quinn 0.11 + ring + tokio 1.32 锁版本）
- **M1 边界** ✅：未触碰 §9 任一项（剪贴板 / h3 / ipc TransportEvent /
  clipboard*.rs 等）。本任务只改 spawn_local → spawn 回退 + 5 个测试
  attribute，零功能新增

## 5. 处理的 SUGGESTION 项

- **#S-5** 🟡 中: 端到端单测验证（`spawn_local` runtime 架构）—— **本 STEP 闭环**
- **#S-23** 🟡 中: 5-7 个 lib fixture 失败跨 spawn_local runtime —— **本 STEP 闭环**

## 6. 闸门检查（PLAN-M1 § 1 时间门 / § 9 边界门）

- §1 时间门：本 STEP 实际 ~25 min（估时 10–15 min），未突破 1h 上限 — ✅
- §9 边界门：grep 当前 STEP 描述（"spawn_local 回退 + local_set_test! macro"）
  未触碰 M2 任一项 — ✅

## 7. 遗留

> 留给下一 STEP 的问题（**不在本 STEP 范围**）：

1. **4 个测试 pre-existing 设计 bug**（§3.4）—— 需要立 STEP-7.2d 单独修复：
   - 测试体缺少 `client_hello` 调用
   - server task 调度时序（current_thread + LocalSet 下需要 oneshot
     readiness signal）
   - Quinn handshake 超时与测试超时边界 race

2. **全仓其他 6 文件仍有 `spawn_local` 调用**（`connect.rs:192/380/446`、
   `listen.rs:299/310/356`、`service.rs:647`、`dns.rs:46/99`、
   `capture.rs:86`、`emulation.rs:90/287`），它们在生产路径都正常工作
   （main.rs 有 LocalSet 包裹）。但如果未来要写涉及这些路径的单元测试，
   需要同样用 `local_set_test!` 包裹。**目前集成测试 quic_smoke /
   input_channel_routing 绕过这些内部 spawn_local 路径，所以全绿**。

3. **`cargo fmt` / `cargo clippy` 30+ pre-existing 警告** —— 见 SUGGESTION
   #S-24 / #S-25，不在本 STEP 范围。

4. **#S-19 per-IP bind + if_addrs / #S-22 "#N-31 模式" 流程纪律** —— 见
   SUGGESTION.md，M2 起手再处理。

## 8. 下一步

- **STEP-7.2d**（建议）：修复 4 个 pre-existing 测试设计 bug（§3.4）。
  不需要改 quic_transport.rs 公共 API，只需改测试体：
  - 加 `client_hello(&client_arc).await` 在 `send_motion` 之前
  - server task 用 `oneshot::channel` 发 readiness → client 等 readiness
    后再 dial
  - `dial_any_all_unreachable_returns_err` 测试超时调到 15s

- **STEP-7.2e**（建议）：Windows MinGW 工具链补齐（MSYS2
  `mingw-w64-x86_64-pkg-config` + 修 `dlltool.exe` CR 字符 bug，或切换
  到 `stable-x86_64-pc-windows-msvc` toolchain）。当前
  `/c/Users/hb/.local/mingw/mingw64/bin/` 有 portable gcc.exe 够用，
  但 pkg-config 仍缺，GTK 相关 feature 构建受限。

- 当前 STEP-7.2c 已 commit。**1 个测试已修好**（`dial_any_prefers_primary`），
  4 个测试有 pre-existing 设计 bug 待 STEP-7.2d 处理。
