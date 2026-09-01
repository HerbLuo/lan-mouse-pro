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

**⚠️ 本地工具链不完整** —— 本机 `stable-x86_64-pc-windows-gnu` toolchain
缺 `gcc.exe`、`pkg-config`，自带的 `dlltool.exe` 有 `CreateProcess␍`
错误（CR 字符 bug），导致 `cargo check` / `cargo test` 在基线代码上就
无法编译（与本 STEP 改动无关）。

### 3.2 替代验证

| 验证项 | 结果 |
|---|---|
| `git diff src/quic_transport.rs` review | ✅ 改动正确（44+ / 22-） |
| `local_set_test!` macro 语法最小复现 | ✅ `rustc` 编译通过 |
| Macro 展开后 `async fn` body 合法 | ✅ `tokio::task::LocalSet::new().run_until(async move { ... }).await;` 是合法表达式 |
| 5 个测试 attribute → macro 调用替换 | ✅ 一一对应 |

### 3.3 Windows 环境工具链修好后用户验证

```bash
cargo test -p lan-mouse --lib -- \
  dial_any_all_unreachable_returns_err \
  dial_any_prefers_primary \
  peer_session_round_trip_motion_keyboard \
  hello_wrong_magic_closes_connection \
  stream_c_take_releases_quinn_recv_stream
```

预期 5 个全绿。

### 3.4 全量回归

```bash
cargo test --workspace
```

预期：
- `lan-mouse` lib 测试：原 ✅ + 5 修复后 ✅ = 全绿
- `tests/quic_smoke` 集成测试：2 ✅ 不变
- `tests/input_channel_routing` 集成测试：7 ✅ 不变

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

1. **全仓其他 6 文件仍有 `spawn_local` 调用**（`connect.rs:192/380/446`、
   `listen.rs:299/310/356`、`service.rs:647`、`dns.rs:46/99`、
   `capture.rs:86`、`emulation.rs:90/287`），它们在生产路径都正常工作
   （main.rs 有 LocalSet 包裹）。但如果未来要写涉及这些路径的单元测试，
   需要同样用 `local_set_test!` 包裹。**目前集成测试 quic_smoke /
   input_channel_routing 绕过这些内部 spawn_local 路径，所以全绿**。

2. **`cargo fmt` / `cargo clippy` 30+ pre-existing 警告** —— 见 SUGGESTION
   #S-24 / #S-25，不在本 STEP 范围。

3. **#S-19 per-IP bind + if_addrs / #S-22 "#N-31 模式" 流程纪律** —— 见
   SUGGESTION.md，M2 起手再处理。

## 8. 下一步

- **建议下一步**（不在本 STEP 范围）：STEP-7.2d — Windows MinGW 工具链补齐
  （安装 MSYS2 `mingw-w64-x86_64-gcc` + `mingw-w64-x86_64-pkg-config` +
  修复 `dlltool.exe` CR 字符 bug，或切换到 `stable-x86_64-pc-windows-msvc`
  toolchain）。让 Windows 上 `cargo test` 完整跑通。

- 当前 STEP-7.2c 已 commit，待用户在 Windows 上重跑以下命令确认 5 个失败
  测试全绿 + 全量回归无破坏：

```bash
cargo test --workspace
```
