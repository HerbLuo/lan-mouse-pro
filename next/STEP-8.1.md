# STEP-8.1 — Windows M1 lib 测试 5 个 panic 修复（STEP-7.2b/c/d 合并归档）

> PLAN-M1 §STEP-7 收尾阶段 / 修复 panic 增量
> 起点：STEP-7.2b 方向错误
> 执行日期：2026-09-01　总耗时：~70 min（4 个 sub-STEP）
> 结论：通过（38 passed / 0 failed）

> **本 STEP 是 STEP-7.2b / 7.2c / 7.2c-1 / 7.2c-2 / 7.2c-3 / 7.2d 的合并归档**。
> 原 6 个文档已合并到本文件，历史 commit 链保留可追溯。

---

## 0. 总览：5 个失败 → 0 个失败

### 0.1 起点（修复前）

```
running 5 tests
thread '...' panicked at tokio-1.51.1/src/task/local.rs:445:29:
`spawn_local` called from outside of a `task::LocalSet` or `runtime::LocalRuntime`
```

5 个测试在 Windows + tokio 1.51 上 panic：

| 测试 | 错误 | 分类 |
|---|---|---|
| `dial_any_all_unreachable_returns_err` | spawn_local panic | A |
| `dial_any_prefers_primary` | spawn_local panic | A |
| `hello_wrong_magic_closes_connection` | spawn_local panic → connection lost | A→B |
| `peer_session_round_trip_motion_keyboard` | spawn_local panic → HelloFailed("hello not complete") | A→B |
| `stream_c_take_releases_quinn_recv_stream` | spawn_local panic → dial: Handshake(TimedOut) | A→B |

**A 类**（3 个，`#[tokio::test(flavor = "current_thread")]`）：直接 panic
**B 类**（2 个，`#[tokio::test]`）：server 端 `PeerSession::run()` 内部
`spawn_local` panic → server task 没起来 → connection lost / handshake fail

### 0.2 终点（修复后）

```
test result: ok. 38 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 35.74s
```

**5/5 全部修好** ✅ + **38/38 全 lib 测试全绿** ✅

---

## 1. 时间线

| 时间 | Sub-STEP | 内容 | 测试结果 |
|---|---|---|---|
| 2026-09-01 早 | STEP-7.2b | ❌ 错误方向：把 4 处 `spawn_local → spawn` | 编译过但方向错 |
| 2026-09-01 中 | STEP-7.2c | ✅ 回退 + 加 `local_set_test!` macro | 1/5 (dial_any_prefers_primary) |
| 2026-09-01 中 | STEP-7.2c-1 | 4 个失败测试 `tokio::spawn → spawn_local` | 1/5（未改善） |
| 2026-09-01 中 | STEP-7.2c-2 | 修 `peer_session_round_trip` (加 `client_hello`) + `dial_any` 超时 10s→15s | 2/5 |
| 2026-09-01 晚 | STEP-7.2c-3 | 文档更新 | — |
| 2026-09-01 晚 | **STEP-7.2d** | multi_thread flavor + `accept_bi` 修复 + spawn 顺序 + 35s 超时 | **5/5** ✅ |

最终 commit 链：
```
ce3369b STEP-7.2d: 治 3 个 pre-existing 测试失败 → 38/38 全绿
7f52c53 STEP-7.2c-3: 文档更新实测结果 23/26 passed + 3 failed
f2bd220 STEP-7.2c-2: 修 peer_session 测试 + dial_any 测试超时
9259ea0 STEP-7.2c-1: 4 个失败测试 server task 改 spawn_local + 实测结果更新
7e75b45 STEP-7.2c: 回退 STEP-7.2b spawn_local 改动 + local_set_test! macro
9b4cb9a STEP-7.2b: 修 Windows + tokio 1.51 上 spawn_local panic (应回退)
```

---

## 2. STEP-7.2b：方向错误 + 修复 STEP-7.2b

### 2.1 STEP-7.2b 改了什么

把 `src/quic_transport.rs` 内 4 处 `spawn_local` 改为 `spawn`：

| 位置 | API 改动 |
|---|---|
| line 71 | `use ... {JoinHandle, JoinSet, spawn_local}` → `{JoinHandle, JoinSet}` |
| line 807 | `joinset.spawn_local(...)` → `joinset.spawn(...)` |
| line 854 | `joinset.spawn_local(...)` → `joinset.spawn(...)` |
| line 2054 | `spawn_local(...)` → `tokio::task::spawn(...)` |
| line 2204 | `spawn_local(...)` → `tokio::task::spawn(...)` |

加上 2 处 doc comment 同步（line 779 / 1813）。

### 2.2 STEP-7.2b 的判断 vs 真正根因

**STEP-7.2b 的判断**：5 个失败的 Windows 单测 panic 原因是 `spawn_local`
在 multi-thread runtime 下没 LocalSet → panic。所以把 `spawn_local` 改 `spawn`
让 future 在多线程上跑。

**真正错的原因**：
1. 生产路径 `src/main.rs:run_async` 已经是 current_thread runtime + LocalSet
   包裹，**生产环境下 `spawn_local` 完全正常工作**。改 `spawn` 没解决任何
   生产问题。
2. 单元测试 panic 是因为 `#[tokio::test]` 默认 multi-thread runtime 没
   LocalSet 上下文。在 tokio 1.51 这个检查变严格，直接 panic。
3. **正确的修复方向是测试 harness**（让 test runtime 也有 LocalSet），
   而不是改生产代码的语义。

---

## 3. STEP-7.2c：回退 STEP-7.2b + LocalSet 包裹

### 3.1 回退 STEP-7.2b 的 4 处 `spawn_local → spawn` 改动

把 STEP-7.2b 改的 4 处 API 全部还原（line 71 / 807 / 854 / 2054 / 2204），
并把 2 处 doc comment（line 779 / 1813）的措辞也还原为 "`spawn_local` 惯例"。

**回退理由**：
- 生产路径需要 `spawn_local`（main.rs 已有 LocalSet 包裹，不改即可用）
- `spawn`（多线程）破坏 LocalSet 语义约定
- 单元测试 panic 的根因不在这里

### 3.2 添加 `local_set_test!` macro（STEP-7.2d 升级为 multi_thread flavor）

在 `src/quic_transport.rs` 测试模块顶部（`mod tests` 内部、`use super::*;`
之后）加：

```rust
/// `local_set_test!` 把测试体包在 `LocalSet::run_until` 里，让
/// `spawn_local` / `JoinSet::spawn_local` 在单元测试中也能正常工作。
///
/// 生产路径 `main.rs::run_async` 已经是 current_thread + LocalSet 包裹；
/// 单元测试 `#[tokio::test]` 默认 multi-threaded runtime 没 LocalSet 包裹，
/// 在 tokio 1.51 上 `spawn_local` 会 panic。
///
/// **flavor 选 multi_thread 而非 current_thread**（STEP-7.2d 升级）：
/// multi_thread runtime 有独立 worker pool 跑 `tokio::spawn`（Send）任务
/// （如 Quinn I/O driver / server task），LocalSet 单独跑 `spawn_local` 任
/// 务和 main future；current_thread 虽也能跑，但所有 Send 任务排在 LocalSet
/// 主 future 之后，出现 server task 还没起来 client 就 dial 完成 → handshake
/// timeout（实测 hello_wrong_magic / stream_c_take 失败）。
/// multi_thread 需要 tokio 的 `rt-multi-thread` feature —— 见 Cargo.toml
/// `[dev-dependencies]`。
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
        #[tokio::test(flavor = "multi_thread")]
        async fn $name() {
            tokio::task::LocalSet::new().run_until(async move $body).await;
        }
    };
}
```

### 3.3 把 5 个失败测试改用 `local_set_test!`

| 测试 | 旧 attribute | 新调用 |
|---|---|---|
| `hello_wrong_magic_closes_connection` | `#[tokio::test]` | `local_set_test!(...)` |
| `stream_c_take_releases_quinn_recv_stream` | `#[tokio::test]` | `local_set_test!(...)` |
| `peer_session_round_trip_motion_keyboard` | `#[tokio::test(flavor = "current_thread")]` | `local_set_test!(...)` |
| `dial_any_prefers_primary` | `#[tokio::test(flavor = "current_thread")]` | `local_set_test!(...)` |
| `dial_any_all_unreachable_returns_err` | `#[tokio::test(flavor = "current_thread")]` | `local_set_test!(...)` |

每处改动只是把 `#[tokio::test]` attribute 替换成 `local_set_test!(name, {`，
对应的函数结尾 `}` 替换成 `});`，**测试体一字不动**。

剩余 12 个 `#[tokio::test]` 测试**未**走 `spawn_local` 路径，保持原
`#[tokio::test]` 不变。

### 3.4 STEP-7.2c 阶段实测结果

```
test result: FAILED. 23 passed; 3 failed; 0 ignored; ... finished in 50.36s
```

`dial_any_prefers_primary` 通过（macro 修复让 `join_next` / `select` 不依赖
额外 task 调度问题）。

---

## 4. STEP-7.2c-1 / 7.2c-2：中间小修复

### 4.1 STEP-7.2c-1：4 个失败测试 `tokio::spawn → spawn_local`

把 4 个失败测试体里的 `let server_task = tokio::spawn(async move {...})`
改成 `let server_task = spawn_local(async move {...})`。

**目的**：既然现在测试在 LocalSet 里跑，server task 应该用 LocalSet 的任务
队列（语义上更合理）。

**实测结果**：**未改善**。4 个测试错误信息不变 —— 这 4 个测试还有自己的
逻辑 bug，单换 spawn flavor 不够。

### 4.2 STEP-7.2c-2：修 `peer_session_round_trip_motion_keyboard` + `dial_any_all_unreachable`

#### 4.2.1 `peer_session_round_trip_motion_keyboard`：补 `client_hello` + 调 close 顺序

**问题**：
- 测试只对 server 端调 `run(Server)`（隐式调 `server_hello`），但 client
  端**从未**调 `client_hello`，直接 `send_motion` → `hello_ok == false`
  → `HelloFailed("hello not complete")`
- 原顺序：await server_task → close client；server.run() 只在 conn.closed()
  fire 时退出 → 原顺序死锁

**修复**：
1. 在 `send_motion` 之前先 `client_hello(&client_arc).await`
2. 顺序改为：close client conn → await server_task
3. server_task panic 用 best-effort `.await` 包装（STEP-6.5 run() 改返
   `Err(close_reason)` 后 server.run() 退出时返 Err，原 `expect` 触发 panic）

#### 4.2.2 `dial_any_all_unreachable_returns_err`：测试超时 10s → 15s

**问题**：测试超时 10s 但 Quinn 默认 `max_idle_timeout = 30s` 也是
handshake 超时 → 测试超时先 fire。

**修复**：测试超时 15s（临时方案，STEP-7.2d 再调到 35s）。

---

## 5. STEP-7.2d：治剩余 3 个 pre-existing 测试失败

### 5.1 切到 multi_thread flavor + 加 `rt-multi-thread` feature

**问题**：current_thread + LocalSet 下 spawn_local 任务调度有 race。

**修复**：把 `local_set_test!` macro 的 `flavor = "current_thread"` 改为
`"multi_thread"`，让 Quinn I/O driver / server task 在独立 worker thread 上跑，
LocalSet 单独跑 main future + local 任务。

`Cargo.toml`:
```toml
[dev-dependencies]
rcgen = "0.13"
# STEP-7.2d: local_set_test! macro 用 multi_thread flavor 需要 rt-multi-thread
# feature —— 多 worker 让 server_task 与 main future 在不同 OS thread 上并行调度，
# 解决 current_thread + LocalSet 下 spawn_local 任务的调度 race。
tokio = { version = "1.32.0", features = ["rt-multi-thread", "time", "macros"] }
```

> **multi_thread 只能解 server task 调度；剩下的 2 个测试还有自己的逻辑
> bug，单换 flavor 不够。** 见 §5.2 / §5.3。

### 5.2 修 `hello_wrong_magic_closes_connection`：`open_bi()` → `accept_bi()`

测试 docstring 写的是 server 手动 `accept_bi` + 发错 magic，但代码误写成
`conn.open_bi()`：

```rust
// 原（错误）：
let (mut send, _recv) = tokio::time::timeout(
    std::time::Duration::from_secs(5),
    conn.open_bi(),                       // ← 打开新 stream，与 client 无关
).await
...
// 修：
let (mut send, _recv) = tokio::time::timeout(
    std::time::Duration::from_secs(5),
    conn.accept_bi(),                     // ← 接 client 开的 stream A
).await
...
```

**为什么 client_hello 一直等不到 server 的 hello**：client_hello 在自己
`open_bi()` 的那条 stream 的 recv 半边读 server 的 hello。server 必须用
`accept_bi()` 接 client 的 stream A，从 server 的 send 半边写。**server 写
错 stream 则 client 永远读不到**。原代码 server 写的是它自己 `open_bi()`
的新 stream，所以 client 的 recv 半边读到的是 stream 关闭 → connection lost
（3s HELLO_TIMEOUT 后 client 主动 conn.close()）。

### 5.3 修 `stream_c_take_releases_quinn_recv_stream`：spawn server 先于 dial

**问题**：测试**先** dial、**后** `spawn_local(server_task)`。client dial
发 Initial 包给 server，但 server 的 accept 还没被 LocalSet 调度（spawn_local
任务入队后需 main future 让出一次才会被 LocalSet poll）。Quinn handshake
10s 超时 → client 拿到 `Handshake(TimedOut)`。

**修复**：把 spawn_local(server_task) 提到 dial **之前**。让 server_task 在
LocalSet 队列里先入队，main future `dial.await` 让出时 LocalSet 调度 server
accept → 注册到 Quinn I/O driver → 收到 client 的 Initial → handshake 成功。

### 5.4 修 `dial_any_all_unreachable_returns_err`：测试超时 15s → 35s

**根因**：quinn 默认 `max_idle_timeout = 30s` 同时也是 handshake 超时（见
quinn-0.11.11 `src/tests.rs:43 handshake_timeout()` 用 500ms 验证）。每条
候选 dial 等满 30s → dial_any 主 future 等最后一条 join → 总耗时 ~30s。

**修复**：测试超时调到 35s 兜底。**不**改 production transport config
（30s 是 LAN 抖动场景合理值）。docstring 同步说明超时原理。

---

## 6. 验证结果（全量 lib 测试）

```bash
cargo test -p lan-mouse --lib --no-default-features -- --test-threads=1
```

```
test result: ok. 38 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 35.74s
```

**38 个 lib 测试全绿** ✅

| 测试 | STEP-7.2c | STEP-7.2c-2 | STEP-7.2d | 最终 |
|---|---|---|---|---|
| `dial_any_prefers_primary` | ✅ | ✅ | ✅ | ✅ |
| `peer_session_round_trip_motion_keyboard` | ❌ | ✅（加 `client_hello`） | ✅ | ✅ |
| `hello_wrong_magic_closes_connection` | ❌ | ❌ | ✅（`open_bi` → `accept_bi`） | ✅ |
| `stream_c_take_releases_quinn_recv_stream` | ❌ | ❌ | ✅（spawn 先于 dial） | ✅ |
| `dial_any_all_unreachable_returns_err` | ❌ | ❌（超时 15s） | ✅（超时 35s） | ✅ |

---

## 7. 环境修复：Windows MinGW 工具链

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

**遗留**（建议下一 STEP 处理）：
- Windows MinGW 工具链补齐（MSYS2 `mingw-w64-x86_64-pkg-config` + 修
  `dlltool.exe` CR 字符 bug，或切换到 `stable-x86_64-pc-windows-msvc`
  toolchain）

---

## 8. 与 PLAN-M1 的偏差 / M1 边界

- **无 PLAN-M1 偏差**（纯 bug fix，沿用 quinn 0.11 + ring + tokio 1.32 锁版本）
- **M1 边界** ✅：未触碰 §9 任一项（剪贴板 / h3 / ipc TransportEvent /
  clipboard*.rs 等）。本任务只改 spawn_local / local_set_test! / 测试体修复，
  零功能新增

---

## 9. 处理的 SUGGESTION 项

- **#S-5** 🟡 中: 端到端单测验证（`spawn_local` runtime 架构）—— **本 STEP 闭环**
- **#S-23** 🟡 中: 5-7 个 lib fixture 失败跨 spawn_local runtime —— **本 STEP 闭环**

---

## 10. 闸门检查（PLAN-M1 § 1 时间门 / § 9 边界门）

- §1 时间门：本 STEP 总耗时 ~70 min（ESTEP-7.2b ~15min + 7.2c ~25min + 7.2c-1/2/3 ~10min + 7.2d ~30min），分摊到各 sub-STEP 内每个 < 1h 上限 — ✅
- §9 边界门：grep 当前 STEP 描述（"测试 fix"），未触碰 M2 任一项 — ✅

---

## 11. 后续

- **STEP-8.1 已闭环 M1 测试修复**。M1 代码侧 100% 完整 + lib 测试 38 全绿
- **建议下一动作**：立 PLAN-M2.md 启动 M2（剪贴板 / 文件同步）
- **遗留（非阻塞）**：
  1. `Cargo.toml [dev-dependencies]` 加了 `tokio` 重复定义 —— 可改成
     `[dev-dependencies] tokio = { workspace = true, features = ["..."] }`
     风格（待 M2 起手时统一 sweep）
  2. clippy / fmt 30+ pre-existing warning —— 见 SUGGESTION #S-24 / #S-25
  3. Windows MinGW 工具链补齐（见 §7）

---

## 12. M2 启动 checklist

- [x] M1 lib 测试全绿（38/38）
- [x] 5 个 Windows 测试 panic 全闭环
- [ ] 立 PLAN-M2.md
- [ ] 决定 M2 范围：剪贴板文本 / 图片 / 文件同步？h3 over QUIC？IRC bridge？
- [ ] M2 起手统一 sweep clippy / fmt / pre-existing warnings
- [ ] 重读 SUGGESTION.md 决定哪些 active 条目进 M2

---

## 13. 不要做的事（避免重新踩坑）

1. ❌ **不要** 把 `JoinSet::spawn_local` 改成 `JoinSet::spawn` —— 破坏生产路径的 LocalSet 语义（STEP-7.2b 的错误）
2. ❌ **不要** 把 `tokio::task::spawn_local` 改成 `tokio::spawn` —— 同上
3. ❌ **不要** 把 `#[tokio::test]` 改成 `#[tokio::main]` —— 签名不匹配
4. ❌ **不要** 在 main.rs 改 runtime 模型 —— 生产用 current_thread + LocalSet 是正确的
5. ❌ **不要** 改 `Cargo.toml` 锁的 tokio 版本 —— 会破坏 macOS/Linux 上跑的集成测试
6. ❌ **不要** 在生产代码里删 `LocalSet::new().run_until(f)` 包裹
7. ❌ **不要** 用 `tokio::spawn` 跑需要 LocalSet 的 future —— 用 `spawn_local`
8. ❌ **不要** 测试内 server 任务用 `conn.open_bi()` 期望写到 client 开的 stream —— 必须 `accept_bi()` 接 client 的 stream
9. ❌ **不要** 测试**先** dial、**后** spawn server task —— Quinn handshake 等不到 server accept poll 注册到 I/O driver
