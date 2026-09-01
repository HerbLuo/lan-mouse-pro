# STEP-7.2d — 治剩余 3 个 pre-existing 测试失败

> PLAN-M1 §STEP-7 收尾阶段 / STEP-7.2c 续治
> 起点：STEP-7.2c-3 后 23 passed / 3 failed
> 执行日期：2026-09-01　实际耗时：~30 min
> 结论：通过（38 passed / 0 failed）

## 1. 上下文：STEP-7.2c 遗留 3 个失败

STEP-7.2c / -7.2c-1 / -7.2c-2 / -7.2c-3 提交后实测结果：

| 测试 | 状态 | 根因 |
|---|---|---|
| `dial_any_prefers_primary` | ✅ PASS | STEP-7.2c 修复 |
| `peer_session_round_trip_motion_keyboard` | ✅ PASS | STEP-7.2c-2 修复 |
| `dial_any_all_unreachable_returns_err` | ❌ FAIL | Quinn 默认 `max_idle_timeout = 30s` 也是 handshake 超时，测试 15s 兜底不够 |
| `hello_wrong_magic_closes_connection` | ❌ FAIL | server 用 `conn.open_bi()` 打开**新** bi stream（写错 magic），但 client 读的是 client 自己 `open_bi()` 那条 stream —— server 必须 `accept_bi()` 接 client 的 stream 才能在它的 send 半边写错 magic。原 bug：测试代码 `open_bi()` 而不是 `accept_bi()` 与 docstring 描述不符 |
| `stream_c_take_releases_quinn_recv_stream` | ❌ FAIL | 测试**先** dial、**后** spawn server_task；但 Quinn handshake 需要 server 的 accept 已 poll 注册到 I/O driver，否则 client dial 永远 handshake TimedOut |

## 2. STEP-7.2d 做了什么

### 2.1 切到 multi_thread flavor + 加 `rt-multi-thread` feature

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

> **⚠️ multi_thread 只能解 server task 调度；剩下的 2 个测试还有自己的逻辑
> bug，单换 flavor 不够。** 见 §2.2 / §2.3。

### 2.2 修 `hello_wrong_magic_closes_connection`：`open_bi()` → `accept_bi()`

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

### 2.3 修 `stream_c_take_releases_quinn_recv_stream`：spawn server 先于 dial

**问题**：测试**先** dial、**后** `spawn_local(server_task)`。client dial
发 Initial 包给 server，但 server 的 accept 还没被 LocalSet 调度（spawn_local
任务入队后需 main future 让出一次才会被 LocalSet poll）。Quinn handshake
10s 超时 → client 拿到 `Handshake(TimedOut)`。

**修复**：把 spawn_local(server_task) 提到 dial **之前**。让 server_task 在
LocalSet 队列里先入队，main future `dial.await` 让出时 LocalSet 调度 server
accept → 注册到 Quinn I/O driver → 收到 client 的 Initial → handshake 成功。

### 2.4 修 `dial_any_all_unreachable_returns_err`：测试超时 15s → 35s

**根因**：quinn 默认 `max_idle_timeout = 30s` 同时也是 handshake 超时（见
quinn-0.11.11 `src/tests.rs:43 handshake_timeout()` 用 500ms 验证）。每条
候选 dial 等满 30s → dial_any 主 future 等最后一条 join → 总耗时 ~30s。

**修复**：测试超时调到 35s 兜底。**不**改 production transport config
（30s 是 LAN 抖动场景合理值）。docstring 同步说明超时原理。

## 3. 验证结果

### 3.1 全量 lib 测试

```bash
cargo test -p lan-mouse --lib --no-default-features -- --test-threads=1
```

```
test result: ok. 38 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 35.74s
```

**38 个 lib 测试全绿** ✅

### 3.2 单独跑 5 个原失败测试

| 测试 | STEP-7.2c | STEP-7.2d |
|---|---|---|
| `dial_any_prefers_primary` | ✅ | ✅ |
| `peer_session_round_trip_motion_keyboard` | ✅ | ✅ |
| `hello_wrong_magic_closes_connection` | ❌ | ✅（`open_bi` → `accept_bi`） |
| `stream_c_take_releases_quinn_recv_stream` | ❌ | ✅（spawn 先于 dial） |
| `dial_any_all_unreachable_returns_err` | ❌ | ✅（测试超时 35s） |

## 4. 与 PLAN-M1 的偏差 / M1 边界

- **无 PLAN-M1 偏差**（纯测试 fix，不改 quic_transport.rs 公共 API，零
  production 行为变化）
- **M1 边界** ✅：未触碰 §9 任一项。`open_bi → accept_bi` 是测试体内修复，
  与 production `client_hello` / `server_hello`（STEP-3.2）对称用法一致

## 5. 处理的 SUGGESTION 项

- **#S-5** 🟡 中: 端到端单测验证（`spawn_local` runtime 架构）—— **本 STEP 闭环**
- **#S-23** 🟡 中: 5-7 个 lib fixture 失败跨 spawn_local runtime —— **本 STEP 闭环**

## 6. 闸门检查（PLAN-M1 § 1 时间门 / § 9 边界门）

- §1 时间门：本 STEP 实际 ~30 min（估时 30–45 min），未突破 1h 上限 — ✅
- §9 边界门：grep 当前 STEP 描述（"测试 fix"），未触碰 M2 任一项 — ✅

## 7. 后续

- STEP-7.2d 已闭环 M1 测试修复。M1 代码侧 100% 完整 + lib 测试 38 全绿
- **建议下一动作**：M2 起手前 archive STEP-7.x 系列；立 STEP-8.x 系列
  准备 M2（剪贴板 / h3 / irc-bridge 等）
- **遗留（非阻塞）**：
  1. `Cargo.toml [dev-dependencies]` 加了 `tokio` 重复定义 —— 可改成
     `[dev-dependencies] tokio = { workspace = true, features = ["..."] }`
     风格（待 M2 起手时统一 sweep）
  2. clippy / fmt 30+ pre-existing warning —— 见 SUGGESTION #S-24 / #S-25

## 8. M2 启动 checklist

- [x] M1 lib 测试全绿（38/38）
- [ ] 立 PLAN-M2.md
- [ ] 决定 M2 范围：剪贴板文本 / 图片 / 文件同步？h3 over QUIC？IRC bridge？
- [ ] M2 起手统一 sweep clippy / fmt / pre-existing warnings
- [ ] 重读 SUGGESTION.md 决定哪些 active 条目进 M2
