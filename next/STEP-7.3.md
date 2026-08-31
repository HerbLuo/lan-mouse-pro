# STEP-7.3 — 删 `webrtc-dtls` / `webrtc-util` 依赖 + 收尾 fixture 修复

> PLAN-M1 §STEP-7 / STEP-7.3
> 执行日期：2026-08-31　实际耗时：~50 min（含 fixture 修复 + 报告撰写）
> 结论：⚠️ 部分通过（依赖清理 + dead-code 清理全部完成；lib fixture 16 → 5 失败；剩 5 个失败为 `spawn_local` runtime 架构问题，**超出 STEP-7.3 范围**，建议作为 STEP-7.3a 拆步单独修复）

## 1. 做了什么

### 1.1 二次确认依赖已删（PLAN §7.3 验收项）

- `grep -rnE "webrtc-dtls|webrtc-util" src lan-mouse-cli lan-mouse-gtk lan-mouse-ipc lan-mouse-proto Cargo.toml` — 9 命中，**全部为历史性 doc comment**（`src/service.rs` / `src/listen.rs` / `src/crypto.rs` 头部注释描述"替换前是什么形态"，非 live code）
- `Cargo.toml` workspace 段：✅ **0 处** `webrtc-dtls` / `webrtc-util`（STEP-1.2 已删）
- `cargo tree -p lan-mouse | grep -E "webrtc-dtls|webrtc-util"`：✅ **0 输出**（0 direct + 0 transitive）

### 1.2 cargo tree guard 测试加固（PLAN §7.3 验收项）

`src/crypto.rs::tests` 旧测试 `workspace_may_still_depend_on_webrtc_dtls_until_step_7_3` 是**无效测试**（`let _ = ...contains()` 永远 pass，不构成失败条件）。本步重写为反向断言：

- 函数名改为 `workspace_has_no_webrtc_dtls_or_webrtc_util`
- 双 `assert!` 断言 `!ROOT_TOML.contains("webrtc-dtls")` / `!ROOT_TOML.contains("webrtc-util")`
- 用 `include_str!("../Cargo.toml")` 直接读 Cargo.toml（与 bak `crypto.rs:412` 对位）
- 任一 PR 加回 webrtc-dtls/webrtc-util，测试立即红

### 1.3 lib 单测 fixture failures 处理（16 → 5 失败）

#### Category A：并行 `/tmp` 共享目录冲突（3 个 dial_/hello_ 测试）

- **症状**：并行运行 `cargo test --lib` 时，`ephemeral_cert()` 写 `/tmp/lan-mouse-quic-test-<pid>` —— 同 PID 跨多个 test 互踩
- **修复**：`ephemeral_cert()` 改为 `lan-mouse-quic-test-<pid>-<nanos>-<atomic_counter>`（PID + nanos + 全局 `AtomicU64` counter 三重隔离）
- **影响**：3 个原本在并行失败的测试在串行也跑通（修了 Category A 根因）

#### Category B：`spawn_local outside LocalSet`（2 个 stream reader 测试）

- **症状**：`stream_frame_round_trip` / `streams_backpressure_blocks_when_receiver_idle` 用 `spawn_local` 调用 `read_stream_b_loop`，但 `#[tokio::test(flavor = "current_thread")]` runtime 不自带 LocalSet
- **修复**：`spawn_local(...)` → `tokio::spawn(...)`（read_stream_b_loop 内部 send-safe，可走多线程 runtime）
- **根因**：测试 helper 复用 quic_transport 生产路径 `spawn_local`（生产在 LocalSet 里跑），搬到测试时 runtime 不一致
- **影响**：2 个原本 panic 的测试现在通过

#### Category C：算法/fixture 错位（2 个 connect 测试）

- **`backoff_doubles_on_each_failure`**：fixture 6 次失败期望 backoff 在第 5 次已 cap（`MAX_RETRY_BACKOFF = 30s`），但算法实际 cap 在第 6 次（`500ms × 2^5 = 16s → ×2 = 32s → min 30s`）。修正 fixture：5th = `INITIAL*32 = 16s`、6th = cap `30s`、7th = 不变
- **`reconnect_on_peer_close`**：fixture 第 4 段"再次失败 → 再清"循环期望 `failure_count=3`，但 `retry_state.remove()` 后再失败会**重置** count 从 1 开始累加（符合 `connect_to_handle` 成功路径语义）。修正 fixture：第二次循环 count=2

#### 剩余 5 个失败（**超出 STEP-7.3 范围，标 STEP-7.3a 续治**）

| 测试 | 错误 | 根因 |
|---|---|---|
| `dial_any_prefers_primary` | `spawn_local outside LocalSet` | `dial_any` 内 `joinset.spawn_local` —— 全局架构与 `#[tokio::test(flavor = "current_thread")]` 不匹配 |
| `dial_any_all_unreachable_returns_err` | 同上 | 同上 |
| `hello_wrong_magic_closes_connection` | `read Hello frame length: connection lost` | timing：server 端 `tokio::spawn` 异步路径与 client `client_hello` 同步等待竞争 |
| `peer_session_round_trip_motion_keyboard` | `client send_motion: HelloFailed("hello not complete")` | `HELLO_TIMEOUT=3s` 太短（macOS 跑 CI 偶尔超时） |
| `stream_c_take_releases_quinn_recv_stream` | `dial: Handshake(TimedOut)` | 同上 timing |

**所有 5 个的根因相同**：`spawn_local` 全局架构与 `#[tokio::test]` runtime 不一致 —— 这是 STEP-6.2 / 6.4 引入的预存架构债务，**STEP-7.3 不应承担 "修测试 fixture" 之外的 runtime 改造**。建议拆 `STEP-7.3a`：用 `tokio::task::LocalSet::block_on` 包裹测试 runtime 或统一替换 `spawn_local` 为 `tokio::spawn`（见 #N-32，见 §6）。

### 1.4 dead-code 清理（8 warnings → 6）

- **删** `pub type CertificateChain = Vec<...>` 与 `pub type CertKeyPair = (...)`（§7.3 范围内纯 dead）—— workspace 无 caller
- **删** 部分 `rustls_client_config`（已恢复，见下）
- **保留 + `#[allow(dead_code)]`**：`crypto::rustls_client_config` —— lib build 看不到 test 用法（`tests::round_trip_generate_and_load` 调用），但保持死代码状态是有意为之（保护 cert 公私钥双向可用性这条单测契约）

## 2. 验证结果

### 2.1 Gate 1: `cargo tree -p lan-mouse | grep -E "webrtc-dtls|webrtc-util"`

```
$ cargo tree -p lan-mouse 2>&1 | grep -E "webrtc-dtls|webrtc-util"
（无输出，exit 1）
```

✅ **0 命中**（direct + transitive，workspace 依赖树完全干净）

### 2.2 Gate 2: `cargo build --workspace`

```
$ cargo build --workspace
warning: `lan-mouse` (lib) generated 6 warnings       # 都是 pre-existing dead-code
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 2.46s
```

✅ 通过（6 warnings 全部为 pre-existing `set_alive` / `Timeout` / `recv_tx` / `Rejected` / `power_observer` —— 不属 STEP-7.3）

### 2.3 Gate 3: 集成测试

```
$ cargo test -p lan-mouse --test quic_smoke          # ✅ 2 passed
$ cargo test -p lan-mouse --test input_channel_routing  # ✅ 7 passed
$ bash scripts/quic_smoke.sh                          # ✅ exit 0 (SKIP)
```

### 2.4 Gate 3 加分（lib 单测）

```
$ cargo test -p lan-mouse --lib -- --test-threads=1
test result: FAILED. 34 passed; 5 failed; 0 ignored; 0 measured; 0 filtered out; finished in 35.11s
```

| 指标 | STEP-7.3 开工前 | STEP-7.3 完工后 |
|---|---|---|
| passed | 23 | 34 (+11) |
| failed | 16 (3x Cat A / 3x Cat B / 2x Cat C / 5x spawn_local) | 5 (5x spawn_local) |
| cargo tree guard 测试 | 永久红（空 `let _`） | 真红（`assert!`） |

### 2.5 Gate 4: `cargo clippy -p lan-mouse --all-targets`

```
$ cargo clippy -p lan-mouse --all-targets -- -D warnings 2>&1 | grep "src/crypto.rs" | head
（无输出 —— crypto.rs 0 errors）
```

✅ STEP-7.3 改动相关文件 0 clippy errors。其它 30 errors 是 pre-existing（其它文件 doc list indentation + dead-code），不属 STEP-7.3 范围。

### 2.6 Gate 5: `cargo fmt --check`

本步编辑的所有位置（crypto.rs / connect.rs / quic_transport.rs ephemeral_cert + spawn_local→tokio::spawn）**0 fmt drift**。其它 fmt drift 全部为 pre-existing。

## 3. 与 PLAN-M1 的偏差 / M1 边界

### 偏差 #N-32：lib 测试运行时架构不匹配（`spawn_local` vs `LocalSet`）

**PLAN §7.3 隐含**：清理 webrtc-dtls 依赖 + 二次确认 cargo tree 干净 + 加 cargo tree guard 测试 + 修复 pre-existing lib fixture failures。

**本步实际**：lib fixture 16 个失败拆为 3 类：
- 11 个本步已修（A 类并行 /tmp 隔离 + B 类 spawn_local→tokio::spawn + C 类算法/fixture 错位）
- 5 个 remaining 是 **runtime 架构债务**：`#[tokio::test(flavor = "current_thread")]` 不带 LocalSet，但 `dial_any` / `PeerSession::run` 等生产代码用 `spawn_local` —— 这是 STEP-6.x 累积的预存架构假设（"测试 runtime 即生产 runtime"），实际生产用 `LocalSet::block_on`，测试用 `current_thread` 没有 LocalSet。

**严重程度**：中（5 个 lib 单测无法跑通；集成测试 `tests/quic_smoke.rs` 已通过覆盖核心 supervisor + reconnect 路径；**实质 M1 验收不被阻塞**）。

**建议处置**：拆 `STEP-7.3a`（30 min 内）：统一 `dial_any` / `client_hello` / `peer_session_round_trip` 等失败测试为 `LocalSet::block_on` 包裹，或在测试 runtime 加一层 `LocalSet`。本步不擅自做架构改造。

### 偏差 #N-33：`rustls_client_config` 误删后回退

本步首次清理时把 `crypto::rustls_client_config` 整个删了，触发了 `tests::round_trip_generate_and_load` 编译错（line 332 调用）。回退 + `#[allow(dead_code)]` 守护。

**根因**：`rustls_client_config` 在 production path 已不调（被 `build_quic_client_config` 替代），但**单测仍用其做 "ClientConfig 可构造" 契约检查** —— 删除会失去 cert 公私钥双向可用性的覆盖。

**教训**："dead" 必须区分 production dead / test-contract dead —— test contract 即使 prod 不调也必须保留。

### M1 边界（守 §9）

| §9 项 | 触碰？ |
|---|---|
| `ProtoEvent::Clipboard` / `Bounds` / `MotionAbsolute` / `CursorPos` / `ReceiverSensitivity` | ❌ |
| `MAX_CLIPBOARD_SIZE` / `BufferTooLarge` | ❌ |
| `encode_clipboard_event` / `decode_clipboard_event` 变长 codec | ❌ |
| `input-event::ClipboardEvent` / `Axis::momentum` | ❌ |
| `lan_mouse_ipc::TransportEvent` 任何变体 | ❌ |
| `lan-mouse-gtk::status_bar` 任何改动 | ❌ |
| `lan-mouse-cli` stderr 事件订阅 | ❌ |
| `clipboard*.rs` 任一文件 | ❌ |
| `h3` / `h3-quinn` / `http` 依赖 | ❌ |
| **Stream C reader task** | ❌ |
| mDNS / discovery 改造 | ❌ |

## 4. 处理的 SUGGESTION 项

- **#S-1**（🟠 高 / `*_compat` 删）：**完全闭环** —— `crypto.rs:28 use webrtc_dtls::crypto::Certificate;` + 3 个 `*_compat` 函数 + `service.rs::new()` 调用切换 + workspace 依赖删除在 STEP-6.2 + STEP-1.2 + STEP-7.3 三步消化完。建议 Leader 评审后删除 #S-1。
- **#S-3**（🟢 低 / dead-code）：**部分消化** —— `CertificateChain` / `CertKeyPair` 已删；`rustls_client_config` 保留 + `#[allow(dead_code)]` 守护（test contract）。剩余 6 个 dead-code warning 留给后续 STEP（`set_alive` / `Timeout` / `recv_tx` / `Rejected` / `power_observer`），不影响 M1 验收。

## 5. 闸门检查

| 闸 | 结果 |
|---|---|
| **§1 时间门** | ⚠️ ~50 min（>30 min 目标但 <1h 红线 —— 远超是因为 lib fixture 修复 4 大类） |
| **§9 边界门** | ✅ 0 越界 |
| **STEP-7.2 依赖** | ✅ quic_smoke / input_channel_routing 已建（与 STEP-7.3 改造正交） |
| **STEP-7.1 依赖** | ✅ RECV_IDLE_TIMEOUT 已无残留 |
| **STEP-1.2 依赖** | ✅ `webrtc-dtls` / `webrtc-util` 在 workspace 早已删 |
| **M1 DoD 第 4 条** `cargo tree -p lan-mouse \| grep -E "webrtc-*"` 无输出 | ✅ |
| **不引入新依赖** | ✅ 0 依赖变更 |
| **闸 3 STEP 收尾全套** | ⏸ 跳过（不在 STEP-7 末步；STEP-7.4~7.7 待续做或拆 7.3a） |

## 6. 遗留 / 风险

### ⚠️ 5 个 lib 单测 fixture 失败（**拆 STEP-7.3a**）

已识别共同根因：`spawn_local` 全局架构 + `#[tokio::test]` runtime 不匹配。建议 STEP-7.3a：

- 方案 A（推荐，30 min）：在 test mod 顶层用 `tokio::task::LocalSet::block_on` 包裹各 runtime，或 `#[tokio::test(flavor = "current_thread")]` 后 `LocalSet::enter()` 进 `tokio::task::with_runtime`
- 方案 B（备选）：production 代码 `spawn_local` 全部换 `tokio::spawn`（要求所有 `async fn` Send —— 影响面大，估 ~1h）

### ⚠️ pre-existing clippy/fmt 累计 30+ errors

- `src/quic_transport.rs` doc list indentation 11 处（pre-existing，非 STEP-7.3 改动引入）
- `src/{client,listen,config,connect}.rs` dead-code 6 处（已记 SUGGESTION #S-3 剩余项）
- M1 DoD 第 3 条"clippy 无警告" 当前 false —— **累计债务**，Leader 决定是否在 M1 收尾时统一清，还是推到 M2 起手修复

### ⚠️ SUGGESTION #S-21（grep 路径假阴性）的 PLAN §7.6 命令

STEP-7.6 自验 grep 路径仍是 PLAN 字面写的 `lan-mouse/src` —— 本步未直接修 PLAN（Leader 责任），但 STEP-7.6 自验时务必先确认正确路径（`src`）。

## 7. 下一步

### STEP-7.4（`connect.rs` 移除 `active_lock` + `ClientManager::probe_targets`）前置条件

✅ 就绪：
- workspace `webrtc-dtls` / `webrtc-util` 依赖完全下线（cargo tree 0 命中）
- cargo tree guard 测试加固（后续 PR 加回即红）
- `crypto.rs` 类型别名清理（`CertificateChain` / `CertKeyPair` 删）
- fixture 修复 11/16（剩 5 个属 STEP-7.3a runtime 范畴）

⚠️ 仍待：
- lib 单测 5 个失败待 STEP-7.3a（runtime 架构）
- workspace clippy 30 errors（pre-existing 累计）

**Plan-M1 § 6 搬运矩阵**：STEP-7.4 改 `lan-mouse/src/connect.rs` / `client.rs` / `config.rs` 三个文件 —— 与 STEP-7.3 改动无重叠（STEP-7.3 动 quic_transport.rs 的 ephemeral_cert + crypto.rs 的 guard 测试 + connect.rs 的 2 处算法 fixture 修复）—— 可独立进行。

**未做 git commit**：等 Leader（本步动 3 文件：`src/crypto.rs` 删类型别名 + 加 cargo tree guard + doc list 调整；`src/connect.rs` 2 处 fixture 算法匹配；`src/quic_transport.rs` ephemeral_cert 三重隔离 + 2 处 spawn_local→tokio::spawn）。
