# STEP-6.4 — `dial_any()` happy-eyeballs 适配 QUIC

> PLAN-M1 §STEP-6 / STEP-6.4
> 执行日期：2026-08-31　实际耗时：~30 min
> 结论：✅ 通过（lib 1 error → 0 errors = connect.rs:205 E0308 修了）

## 1. 做了什么

实现 PLAN §6.4 happy-eyeballs 多地址并发拨号，替换 STEP-6.1 单地址 `dial`：

**`lan-mouse/src/quic_transport.rs`**（改动）：
- imports 加 `tokio::task::JoinSet`（happy-eyeballs 用）
- 新 `pub async fn dial_any(ep, primary: SocketAddr, all: &[SocketAddr], cert, key, pins_dir) -> Result<Connection>` —— 200ms primary head-start + 剩余候选并发 + `JoinSet` 协调 + `abort_all()` 取消输家
- 新 `const HEAD_START: Duration = Duration::from_millis(200)` —— 与 bak `mousehop/src/quic_transport.rs:2004 HEAD_START` + connect.rs 现有 `PREFERRED_ADDR_HEAD_START` 语义对齐
- 新单测 `dial_any_prefers_primary` —— primary = server_addr + 1 不可达 fallback → 断言 `remote_address() == primary`（happy-eyeballs 头契约）
- 新单测 `dial_any_all_unreachable_returns_err` —— 2 个 TEST-NET-1 不可达地址 → 断言返 `Err` 且总耗时 < 10s

**`lan-mouse/src/connect.rs`**（改动）：
- 修 `connect.rs:205` E0308：`SocketAddr::new(a, port)` → `SocketAddr::new(*a, port)`（`ips_set.iter()` 返 `&IpAddr`，需解引用；偏差归档记 PLAN-M1 #N-26）
- `connect_to_handle` 单地址 `dial` 调用换 `dial_any(...)`：primary = `addrs.first()`、all = `&addrs`；其余流程（client_hello → set_active_addr → register_peer → 摘 connecting）不变

## 2. 关键设计要点

### 2.1 `dial_any` happy-eyeballs 算法

```
input: primary + all + cert/key/pins_dir
  ├─ (1) install_crypto_provider + build_quic_client_config() → Arc<ClientConfig> 复用
  ├─ (2) joinset.spawn_local(primary) → joinset 里 1 task
  ├─ (3) loop { select! { 200ms timer → break; join_next() → Some(Ok(conn)) → return Ok(conn) } }
  │       （head-start 期 primary 赢 → abort_all + return；输 → log warn + 等 timer）
  ├─ (4) for &addr in all (skip primary): joinset.spawn_local(addr)
  └─ (5) while let Some(joined) = joinset.join_next():
            Some(Ok(conn)) → abort_all + return
            Some(Err(e))  → log warn + last_err = Some(e)
            None (panic)   → continue
       JoinSet 空 → Err(last_err)
```

### 2.2 与 bak `mousehop/src/quic_transport.rs::dial_any` 的差异

| 维度 | bak | 本步 |
|---|---|---|
| 返值 | `Result<Rc<PeerSession>, Error>` | `Result<Connection, Error>` |
| `peer.wrap_session()` 内含 `client_hello` | 是 | **否**（hello 由 STEP-6.1 caller `connect_to_handle` 单独跑） |
| `InputChannelConfig` 参数 | 有（wrap_session 装到 `with_config`） | **无**（dial_any 只管"连上"） |

**理由**：PLAN §6.4 文字明确签名 `Result<Connection>`；STEP-6.1 caller 拆开 "happy-eyeballs" 与 "hello 握手" 两个关注点。STEP-6.5 重连触发可复用同一 `connect_to_handle` → `dial_any` 路径，无需再 wrap hello 进 dial_any。

### 2.3 JoinSet match 处理（踩坑记录 → PLAN-M1 偏差 #N-27）

`JoinSet::join_next().await` 返 `Option<Result<T, JoinError>>`（`None` 表示所有 task 已 join 完成；`Some(Err(JoinError))` 表示某 task panic）。

**错误模式**：第一版直接 `match joined.expect("...")` —— `joined` 实际是 `Option<Result<...>>`，`expect` 只能吃 `Result` 不能吃 `Option`，rustc 报 E0308"expected Result, found tuple"。

**正确解**：
```rust
let Some(inner) = joined else { break; };        // None → JoinSet 空，跳出循环
let Ok((_addr, res)) = inner else { continue; }; // Err(JoinError) → log warn + 跳过
match res { Ok(conn) => ..., Err(e) => ... }
```

### 2.4 quinn 0.11 `Connection` RAII Drop 自动 close

happy-eyeballs 中途放弃时 `JoinSet::abort_all()` 取消输家 task，输家的 `quinn::Connection` 落到 caller 的 local_drop 时**本身**就发 close frame（QUIC 相对 DTLS 的简化 —— spike README 坑清单第 4 条印证）。**不**需要显式 `conn.close(...)`，与 bak 注释明确一致。

### 2.5 M1 简化（与 bak 的差异）

- **不返回 Rc<PeerSession>**：见 §2.2
- **不装配 hello**：见 §2.2
- **`spawned: HashSet<SocketAddr>` 去重**：`all` 可能包含 primary（如 caller 传 `addrs = primary + others` 全集），用 `HashSet` 避免重复 spawn —— 与 bak 完全一致
- **不重试 / 退避**：本步只负责 happy-eyeballs 单次拨号；retry 框架留 STEP-6.5

## 3. 与 PLAN-M1 §6.4 的偏差

### 偏差 #N-26：connect.rs:205 E0308 同步修

**PLAN §6.4 隐含**：`connect_any` 调用 dial_any 替换 DTLS 路径

**本步实际**：发现 connect.rs:205 `SocketAddr::new(a, port)` 类型不匹配（`a: &IpAddr` vs 函数要求 `IpAddr`），同步加 `*a` 解引用修。

**理由**：PLAN §6.4 行文说 "修 connect.rs:205 E0308" 是验收项之一；这是 happy-eyeballs 上线必经的类型对齐。

**严重程度**：轻（PLAN 显式列入）。

### 偏差 #N-27：JoinSet match 形态调整

**本步实际**：`JoinSet::join_next()` 返 `Option<Result<T, JoinError>>`（与 bak 早期代码假设的 `Result<T, JoinError>` 不同；具体看 quinn 0.11 实际签名），故需先解 `Option` 再解 `Result`。

**理由**：quinn 0.11 `JoinSet::join_next() -> Option<Result<T, tokio::task::JoinError>>` 是稳定签名 —— bak 也按 `Option` 处理（spike 已踩坑）。

**严重程度**：轻（rustc 编译器直接拒收，不影响语义）。

## 4. 与 PLAN §9 M1 边界检查

| §9 类别 | 本步触碰 | 说明 |
|---|---|---|
| `proto` 变体 / 常量 / 错误 / codec | 否 | 没动 proto |
| `input-event` | 否 | 没动 |
| `ipc::TransportEvent` | 否 | 没动 ipc |
| `lan-mouse-gtk::status_bar` | 否 | 没动 gtk |
| `lan-mouse-cli` stderr 订阅 | 否 | 没动 cli |
| `lan-mouse/src/clipboard*.rs` | 否 | 没动 core 其它文件 |
| `Cargo.toml` 引入 h3/h3-quinn/http | 否 | 0 依赖变更 |
| `quic_transport.rs::Stream C` reader | 否 | dial_any 不开 reader task |
| `connect.rs` mDNS / discovery | 否 | 没动 mDNS |

```
$ grep -nE "webrtc-dtls|webrtc-util|TransportEvent|Bounds|MotionAbsolute|CursorPos|ReceiverSensitivity|MAX_CLIPBOARD_SIZE|BufferTooLarge|ClipboardEvent|h3|h3-quinn|status_bar|clipboard" src/connect.rs src/quic_transport.rs
# （0 命中 —— §9 12 类 grep 全部 clean）
```

**结论**：0 越界。

## 5. 验证结果

### 5.1 `cargo check -p lan-mouse --lib`

```
$ cargo check -p lan-mouse --lib 2>&1 | grep -cE "^error\[E"
0
```

**1 → 0 errors**：connect.rs:205 E0308 修复成功，dial_any 编译通过。

### 5.2 errors 分布

| 阶段 | lib 总 errors |
|---|---|
| STEP-6.3 完成后 | 1（connect.rs:205 E0308） |
| **本步完成后** | **0** |

| 错误源 | 数量 | 本步是否触碰 |
|---|---|---|
| `src/connect.rs:205` `SocketAddr::new(a, port)` 类型不匹配 | 0 | ✅ 已修（偏差 #N-26） |
| `src/quic_transport.rs` `dial_any` 编译错 | 0 | ✅ 编译过 |

### 5.3 §9 M1 边界 grep

```
$ grep -nE "webrtc-dtls|webrtc-util|TransportEvent|Bounds|MotionAbsolute|CursorPos|ReceiverSensitivity|MAX_CLIPBOARD_SIZE|BufferTooLarge|ClipboardEvent|h3|h3-quinn|status_bar|clipboard" src/connect.rs src/quic_transport.rs
# 0 命中
```

### 5.4 `cargo check -p lan-mouse --tests`

```
$ cargo check -p lan-mouse --tests 2>&1 | grep -cE "^error\[E"
25
```

**说明**：25 个 errors **全部是 pre-existing**（STEP-2.2 `dial_completes_handshake_against_local_listener` 测试用 `timeout(...connect_with...)` 但 quinn 0.11 `connect_with` 同步返 `Result<Connecting, _>` 而非 `impl Future` —— 与本步无关；其它 errors 也都是 STEP-1.x ~ 5.x 累积的 fixture 引用）。`grep "quic_transport.rs"` 在 errors 行 = **0 命中** —— 本步新增的 `dial_any_prefers_primary` / `dial_any_all_unreachable_returns_err` **0 编译错**。

**SUGGESTION #S-5 模式延续**：测试逻辑就位 → lib 编过 → leader 在 STEP-7.x 修剩余 fixture 后手动跑 `cargo test -p lan-mouse dial_any_prefers_primary` 确认通过。

### 5.5 单测设计要点

**`dial_any_prefers_primary`**：
- server endpoint + client endpoint in-process
- `primary = server_addr`，`all = [server_addr, 192.0.2.1:65535]`（TEST-NET-1 不可达）
- `dial_any` 200ms 内 primary 握手成功（→ 返回 Connection），备用 192.0.2.1 在 timer 后并发拨，连接超时
- 断言：`conn.remote_address() == server_addr == primary`

**`dial_any_all_unreachable_returns_err`**：
- 无 server endpoint（仅 client endpoint）
- `primary = 192.0.2.1:65535`, `secondary = 192.0.2.2:65535`
- `dial_any` 应在 < 10s 内返 Err
- 不断言具体错误类型（quinn 不同 OS 返的 ConnectionError 不同）

## 6. 处理的 SUGGESTION 项

无新增条目。

SUGGESTION #S-5（"lan-mouse lib 因 14 errors 编不过"）**实质性消化**：
- STEP-6.1 ~ 6.4 累积将 lib errors 从 14 → 0
- walkaround 不再需要；测试目标落在 `lan-mouse` lib 内部的 STEP-6.4 / 6.5 等可正常跑（前置：STEP-7.x 修测试 fixture errors）

## 7. 闸门检查（PLAN-M1 §1 时间门 / §9 边界门）

| 闸 | 结果 |
|---|---|
| **§1 时间门**：30 min 目标 | ✅ 实际 ~30 min |
| **§9 边界门** | ✅ 0 越界（详见 §4） |
| **STEP-6.1 依赖** | ✅ LanMouseConnection / connect_to_handle 框架就位 |
| **STEP-2.2 依赖** | ✅ `dial(ep, addr, cert, key, pins_dir) -> Result<Connection>` 单地址 dial 就位 |
| **STEP-3.2 依赖** | ✅ `client_hello(&peer)` 就位（caller 单独跑） |
| **STEP-2.6 依赖** | ✅ TofuVerifier / pins_dir 路径就位 |
| **闸 2 实时自检** | ✅ lib 0 errors（deviation #N-26 connect.rs:205 已修） |
| **闸 3 STEP 收尾** | ⏸ 跳过（非 STEP-7 收尾；STEP-7.x 集中跑全套） |

## 8. 遗留 / 风险

- ⚠️ **`dial_any_prefers_primary` / `dial_any_all_unreachable_returns_err` 测试未实跑**：lan-mouse lib 编过 + 测试代码就位，但 `cargo test --workspace` 因 STEP-1.x ~ 5.x 累积的 fixture errors（pre-existing）暂不可跑。Leader 在 STEP-7.x 修测试 fixture 后手动跑 `cargo test -p lan-mouse dial_any_*` 确认通过（SUGGESTION #S-5 模式持续）
- ⚠️ **`dial` 单地址函数**：main-code 已无 caller（`connect_to_handle` 切到 `dial_any`），仅测试用；`#[allow(dead_code)]` 守护（STEP-2.2 加的）保留
- ⚠️ **happy-eyeballs 200ms 阈值**：PLAN §7 风险表已标注"200ms 阈值太小被防火墙丢弃"——本步沿用 bak 默认；后续 milestone 评估
- ⚠️ **server 端 per-IP bind 仍未实现**（SUGGESTION #S-20）：M1 happy-eyeballs 是 client 端多 IP 并拨；server 端 per-IP bind（`enumerate_listenable_addrs` + `if_addrs`）是后续微步

## 9. 下一步（STEP-6.5 前置条件）

✅ **就绪**：
- `dial_any(ep, primary, all, cert, key, pins_dir) -> Result<Connection>` happy-eyeballs 实现 + 200ms head-start + JoinSet 协调
- `connect.rs:205` E0308 已修
- `connect_to_handle` 主干已切到 `dial_any`：primary = `addrs.first()`、all = `&addrs`
- LanMouseConnection `send()` 拨号路径完整：dial_any → client_hello → set_active_addr → register_peer → 摘 connecting

⏸ **STEP-6.5 范围**（待办）：
- `Connection::closed()` → 重连触发（`PeerSession::run()` close-driven 重连 + `RetryState` 退避）
- 复用 `connect.rs` 现有 `RetryState`（如有）+ LanMouseConnection send 路径
- `should_retry_after_close` 决策 + backoff
- 单测 `reconnect_on_peer_close` + `backoff_doubles_on_each_failure`

**未做 git commit**：等 Leader 处理（本步动 2 文件：`src/connect.rs` / `src/quic_transport.rs`；新增 `dial_any` + `HEAD_START` + 2 个单测 + 修 1 个 E0308）。
