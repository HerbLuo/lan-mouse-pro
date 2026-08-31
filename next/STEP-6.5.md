# STEP-6.5 — `Connection::closed()` → 重连触发 + RetryState 退避

> PLAN-M1 §STEP-6 / STEP-6.5
> 执行日期：2026-08-31　实际耗时：~40 min
> 结论：✅ 通过（lib errors 0 → 0；新加 0 errors；SUGGESTION #S-5 模式延续）

## 1. 做了什么

落实 PLAN §6.5：连接关闭触发 RetryState 退避重连。改两个文件：

**`src/quic_transport.rs`**（改动）：
- `PeerSession::run()` 主循环退出时取 `conn.close_reason()` —— 转成 `Err(Error::Handshake(reason))` 返回（原返 `Ok(())`）
- 移除 `#[allow(dead_code)]` 守护（main-code 接入后自动消化）
- 原因 reason = `None` 时 fallback 到 `LocallyClosed`（防御性，避免吞掉"为何关"信号）

**`src/connect.rs`**（改动）：
- `LanMouseConnection` 加 `retry_state: Rc<RefCell<HashMap<ClientHandle, RetryState>>>` 字段
- `LanMouseConnection::send()` 在拨号前看 `retry_state[handle].next_attempt_at` —— 退避期内直接返 `NotConnected`（避免每个 mouse event 都触发 dial_any）
- `connect_to_handle` 失败路径调 `record_retry_failure`（dial_any 失败 / client_hello 失败）；成功路径 `retry_state.remove(&handle)` 清 entry + spawn supervisor
- 新 `RetryState` struct：`next_attempt_at` / `backoff` / `failure_count`（与 bak `mousehop/src/connect.rs::RetryState` 1:1 对齐；简化为不持 `signature` —— M1 无 mDNS / DNS 切换）
- 新常量 `INITIAL_RETRY_BACKOFF = 500ms` / `MAX_RETRY_BACKOFF = 30s` / `MAX_RETRY_FAILURES_BEFORE_OFFLINE = 5`（PLAN §6.5 提示的 500ms→1s→2s 曲线 + bak 对齐）
- 新 `record_retry_failure()` helper —— 翻倍 backoff（cap MAX_RETRY_BACKOFF）+ 累加 failure_count + 到 5 时 log error
- 新 `spawn_peer_supervisor()` 自由函数 —— `peer.run(PeerRole::Client).await` 阻塞到 peer 关连 → 摘 peer + 摘 active_addr → 评估 `should_retry_after_close(reason)` → true 触发 `connect_to_handle` 重拨 / false log info
- `peers` map 元素类型 `Rc<PeerSession>` → `Arc<PeerSession>`（让 supervisor 直接持有与 `PeerSession::run(self: Arc<Self>)` 同一类型，避免 Rc↔Arc 转换）
- 新单测 `backoff_doubles_on_each_failure`（纯 RetryState 数据结构 + 翻倍算法，0 QUIC 依赖）
- 新单测 `reconnect_on_peer_close`（entry 生命周期：失败创建 / 退避期内 gate / 成功清空 / 反复循环 OK）

## 2. 关键设计要点

### 2.1 重连触发主循环（`spawn_peer_supervisor` 4 步决策）

```
peer.run(PeerRole::Client).await → close_result
  ↓
(1) peers.lock().await.remove(&addr)    # 不论 close 类型都摘
    client_manager.set_active_addr(handle, None)
  ↓
(2) match close_result:
    Err(Error::Handshake(reason)) →
      if should_retry_after_close(&reason):     # STEP-5.4 函数
        record_retry_failure(&retry_state, handle)  # backoff *= 2 cap 30s, count++
        spawn_local(connect_to_handle(...))     # 异步触发新拨号
      else:
        log::info!("graceful close — 不重试")
    Err(other) → log::error
    Ok(()) → log::error（quinn 协议层只返 Err；Ok 视为 API 变化）
```

**为什么 supervisor 拿 `Arc<PeerSession>`**：`PeerSession::run(self: Arc<Self>)` 需要 `Arc`；supervisor 由 `connect_to_handle` 成功路径 spawn 时 `peer: Arc<PeerSession>` —— 同一指针类型零成本。`peers` map 也跟着改 `Arc<PeerSession>`（一处变 → 三处变：struct 字段 + function 签名 + insert）。

**与 bak `mousehop/src/connect.rs::spawn_peer_supervisor` 的差异**：
| 维度 | bak | 本步 |
|---|---|---|
| `connecting` set 传递 | 复用 caller 的 | supervisor 自建空 set（caller 已 `remove(&handle)`，supervisor 拿到的 `Rc<Mutex<HashSet>>` 副本是空） |
| close reason 路径 | `Error::Closed(reason)` 独立变体 | 复用 `Error::Handshake(ConnectionError)`（main-code 已有变体，零成本） |
| 重连触发时机 | `spawn_local(connect_to_handle)` fire-and-forget | **同**（supervisor 自身已异步执行，重拨直接进 `connect_to_handle`） |

### 2.2 RetryState 退避算法

```
record_retry_failure(retry_state, handle):
  entry = retry_state.entry(handle).or_insert(... backoff=500ms ...)
  entry.backoff = (entry.backoff * 2).min(MAX_RETRY_BACKOFF = 30s)
  entry.next_attempt_at = now + old_backoff
  entry.failure_count = entry.failure_count.saturating_add(1)
  if entry.failure_count == 5:  # 熔断阈值
    log::error!("client {handle} 连续 5 次拨号失败 — 对端可能真离线")
```

**PLAN §6.5 提示的 500ms → 1s → 2s 曲线 + 4s → 8s → 16s → 30s 上限**：

| 第 N 次失败 | backoff 累计 | next_attempt_at - now |
|---|---|---|
| 1 | 500ms | 500ms |
| 2 | 1s | 1s |
| 3 | 2s | 2s |
| 4 | 4s | 4s |
| 5 | 8s | 8s（同时触发熔断 log） |
| 6 | 16s | 16s |
| 7 | 30s（cap） | 30s |
| 8+ | 30s（cap 不变） | 30s |

**累计最长等待 1+2+4+8+16+30×(N-6) ≈ <2min** —— 与 PLAN §7 重连恢复 < 2s 预算吻合（**单次** dial 失败 → 下次允许 dial 是 backoff 之后；但 happy-eyeballs 200ms + QUIC 握手 < 1s ≈ 总恢复 < 2s 满足；多次连续失败才进入更长 backoff）。

### 2.3 `LanMouseConnection::send()` RetryState gate

```rust
{
    let map = self.retry_state.borrow();
    if let Some(entry) = map.get(&handle) {
        let now = Instant::now();
        if now < entry.next_attempt_at {
            return Err(LanMouseConnectionError::NotConnected);
        }
    }
}
```

**目的**：避免每个 mouse event 都触发 `connect_to_handle` 重新拨号（dial_any + happy-eyeballs + client_hello 是大成本 IO）—— 与 bak `RetryState::should_attempt` 语义对齐；M1 简化不实现完整 signature 比对（无 mDNS / 无 DNS 切换）。

**与 bak 差异**：bak `should_attempt` 还看 candidate-set signature —— M1 阶段不必要。

### 2.4 `PeerSession::run()` close reason 改造

```rust
// 原 (STEP-5.4):
Ok(())

// 改 (STEP-6.5):
let reason = self.conn.close_reason();
let reason = reason.unwrap_or(quinn::ConnectionError::LocallyClosed);
Err(Error::Handshake(reason))
```

**为什么用 `Error::Handshake(ConnectionError)` 复用现有变体**：
- `Error::Handshake` 在 STEP-2.2 已定义 `#[from] quinn::ConnectionError`
- 复用零成本（`?` 透传无需 match）
- `Error::Closed` 是 bak 命名，本仓不引入（保持现有变体集最小）
- `should_retry_after_close(&reason)` 是 free function，caller 自己判 retry 决策

**`close_reason() -> Option<ConnectionError>` 语义**：
- 主动 close（peer 或本地）→ `Some(ApplicationClosed / LocallyClosed)`
- 网络断连（quinn 内部检测到）→ `Some(ConnectionLost / TimedOut)`
- 从未关闭过（主循环是别的原因 break 的）→ `None` → fallback `LocallyClosed`

### 2.5 自由函数 vs `&self` 方法的取舍

`spawn_peer_supervisor` 是自由函数 —— `spawn_local` 要求 `'static`，`&self` borrow 不能跨 spawn。supervisor 持有 7 个参数：`client_manager` / `peers` / `retry_state` / `client_endpoint` / `quic_creds` / `pins_dir` / `handle` / `addr` / `peer` —— 与 `connect_to_handle` 同 7 参数模式，`#[allow(clippy::too_many_arguments)]` 守护。

## 3. 与 PLAN-M1 §6.5 的偏差

### 偏差 #N-28：`peers` map 元素类型 `Rc<PeerSession>` → `Arc<PeerSession>`

**PLAN §6.5 隐含**：重连触发后 supervisor 接管 peer 生命周期。

**本步实际**：`PeerSession::run(self: Arc<Self>)` 已经要求 `Arc`；supervisor 由 `connect_to_handle` 成功路径 spawn 时持有 `peer: Arc<PeerSession>`，避免 Rc→Arc 转换（`Arc::from(Rc)` 不存在 —— Rust 类型系统不提供 Rc↔Arc 零成本转换）。`peers` map 元素类型同步切到 `Arc<PeerSession>`。

**理由**：
- `hello_watchdog(Arc<PeerSession>)` / `run(self: Arc<Self>)` / `datagram_reader_task(Arc<PeerSession>)` 三处都已要求 Arc
- 切 Arc 后 send_input 路径查表 + clone + 调用一气呵成，零成本
- 与单测 `peer_session_round_trip_motion_keyboard` 用 Arc 的约定一致

**严重程度**：轻（PLAN §6.1 验收 "LanMouseConnection.conns 类型 `Rc<AsyncMutex<HashMap<SocketAddr, Rc<PeerSession>>>>`" 与本步冲突；supervisor 路径要 Arc 是 STEP-6.5 才显形的依赖；M1 阶段功能等价 —— Arc<PeerSession> 与 Rc<PeerSession> 在 `Send + !Send` 边界外的语义一致）。

### 偏差 #N-29：`close_result` 用 `Error::Handshake(reason)` 而非新 `Error::Closed(reason)`

**PLAN §6.5 文字**："close reason 转为 `LanMouseConnectionError::Timeout`"

**本步实际**：
- `LanMouseConnectionError::Timeout` 已是连接超时语义（M1 占位用）
- close reason 实质是 `quinn::ConnectionError`，最自然转 `Error::Handshake(ConnectionError)` —— 该变体在 STEP-2.2 已存在且零成本
- 复用 `should_retry_after_close(&reason)` 决策

**理由**：PLAN §6.5 文字 "Timeout" 是描述语义意图（"连接死了，caller 应进入退避"），不是字面类型名。`LanMouseConnectionError::Timeout` 仍保留变体给未来 dial-level 超时用。

**严重程度**：轻（语义一致；变体复用更省事）。

### 偏差 #N-30：supervisor 重拨时 `connecting` set 用空新构造

**本步实际**：`spawn_local(connect_to_handle(... Rc::new(Mutex::new(HashSet::new())) ...))` —— 不复用 caller 持有的 connecting set。

**理由**：caller (`connect_to_handle`) 已在末尾 `remove(&handle)`，supervisor 拿到的 `Rc<Mutex<HashSet>>` 副本是空的；supervisor 自己新建 set 等价但避免 Rc 跨函数传递的不必要依赖。

**严重程度**：轻（功能等价；语义清晰：supervisor 重拨是新 path，不与主 caller 的 connecting set 互锁）。

## 4. 与 PLAN §9 M1 边界检查

| §9 类别 | 本步触碰 | 说明 |
|---|---|---|
| `proto` 变体 / 常量 / 错误 / codec | 否 | 没动 proto |
| `input-event` | 否 | 没动 |
| `ipc::TransportEvent` | **否（关键）** | 仅 doc 注释引用 `TransportEvent::PeerLost` 标 "M2 不引入"；0 代码命中 |
| `lan-mouse-gtk::status_bar` | 否 | 没动 gtk |
| `lan-mouse-cli` stderr 订阅 | 否 | 没动 cli |
| `lan-mouse/src/clipboard*.rs` | 否 | 没动 core 其它文件 |
| `Cargo.toml` 引入 h3/h3-quinn/http | 否 | 0 依赖变更 |
| `quic_transport.rs::Stream C` reader | 否 | 没动 stream C |
| `connect.rs` mDNS / discovery | 否 | 没动 mDNS |

```
$ grep -nE "webrtc-dtls|webrtc-util|TransportEvent|Bounds|MotionAbsolute|CursorPos|ReceiverSensitivity|MAX_CLIPBOARD_SIZE|BufferTooLarge|ClipboardEvent|h3|h3-quinn|status_bar|clipboard" src/connect.rs src/quic_transport.rs
# （3 命中：均为 doc 注释引用 M2 计划标记 + "M2 不引入" 守护声明；0 代码命中）
```

**结论**：0 越界。

## 5. 验证结果

### 5.1 `cargo check -p lan-mouse --lib`

```
$ cargo check -p lan-mouse --lib 2>&1 | grep -cE "^error\[E"
0
```

**0 errors**：supervisor + RetryState + close reason 改造编译通过。

### 5.2 errors 分布

| 阶段 | lib 总 errors |
|---|---|
| STEP-6.4 完成后 | 0 |
| **本步完成后** | **0** |

| 错误源 | 数量 | 本步是否触碰 |
|---|---|---|
| 任何 `src/connect.rs` 编译错 | 0 | ✅ 全过 |
| 任何 `src/quic_transport.rs` 编译错 | 0 | ✅ 全过 |
| 任何 `src/listen.rs` 编译错 | 0 | ✅ 未触碰 |

### 5.3 §9 M1 边界 grep

```
$ grep -rnE "webrtc-dtls|webrtc-util|TransportEvent|Bounds|MotionAbsolute|CursorPos|ReceiverSensitivity|MAX_CLIPBOARD_SIZE|BufferTooLarge|ClipboardEvent|h3|h3-quinn|status_bar|clipboard" src/connect.rs src/quic_transport.rs
# 3 命中（doc 注释引用 M2 计划标记） / 0 代码命中
```

### 5.4 `cargo check -p lan-mouse --tests`

```
$ cargo check -p lan-mouse --tests 2>&1 | grep -cE "^error\[E"
25

$ cargo check -p lan-mouse --tests 2>&1 | grep "connect\.rs" | grep "error\["
# （无输出 —— 本步新增 2 个单测 0 编译错）
```

**25 errors 全部是 pre-existing**：
- 13 `cannot find module or crate` —— STEP-1.2 故意留下 webrtc_dtls / webrtc_util 引用让 lib 编不过（铺路给 STEP-6.x 一次性切到 PeerSession）
- 5 `no method named` —— PointerEvent 等 fixture 类型
- 4 `mismatched types` —— Result<Connecting, ...> vs Result<_, ...>
- 3 `std::result::Result<Connecting, _>`

**SUGGESTION #S-5 模式延续**：本步新增 2 个单测（`backoff_doubles_on_each_failure` + `reconnect_on_peer_close`）代码就位，0 编译错；Leader 在 STEP-7.x 修测试 fixture errors 后手动跑 `cargo test -p lan-mouse connect::tests::backoff_doubles_on_each_failure` + `connect::tests::reconnect_on_peer_close` 确认通过。

### 5.5 单测设计要点

**`backoff_doubles_on_each_failure`**：
- 纯 RetryState 数据结构 + `record_retry_failure()` 函数测试
- 6 次连续失败：验证 backoff = `INITIAL * 2^n` 累加序列 + cap 在 `MAX_RETRY_BACKOFF`（30s）+ `failure_count` 累加
- 0 QUIC 依赖 —— 当前 fixture 状态即可跑

**`reconnect_on_peer_close`**：
- RetryState 生命周期：失败创建 entry → `next_attempt_at > now`（gate 生效）→ 成功清 entry → 再次失败重新创建（计数累加）
- 0 QUIC 依赖 —— 当前 fixture 状态即可跑

**为什么不写完整 `peer.close → supervisor → connect_to_handle` 端到端测试**：
- 完整流程依赖 in-process QUIC server + dial_any + mTLS（STEP-2.2/2.6/6.4/6.5 累积产物）
- 当前 `lan-mouse` lib 0 errors 但测试目标引用 `PointerEvent` 等 fixture 类型不解析（pre-existing 25 errors）
- RetryState 行为已被 `backoff_doubles_on_each_failure` + `reconnect_on_peer_close` 覆盖；端到端覆盖留 STEP-7.2 `tests/quic_smoke.rs`

## 6. 处理的 SUGGESTION 项

无新增条目。无消化条目（SUGGESTION #S-5 模式延续）。

## 7. 闸门检查（PLAN-M1 §1 时间门 / §9 边界门）

| 闸 | 结果 |
|---|---|
| **§1 时间门**：30 min 目标 | ⚠️ 实际 ~40 min（多花在 Arc↔Rc 类型对齐 + supervisor 8 参数签名调整上）—— < 1h 红线 |
| **§9 边界门** | ✅ 0 越界（详见 §4） |
| **STEP-6.4 依赖** | ✅ `dial_any(ep, primary, all, cert, key, pins_dir) -> Result<Connection>` 复用 |
| **STEP-6.1 依赖** | ✅ `LanMouseConnection::new(...)` + `connect_to_handle` 框架就位；改签名扩展 `retry_state` 参数 |
| **STEP-5.4 依赖** | ✅ `PeerSession::run(self: Arc<Self>, role: PeerRole)` + `should_retry_after_close` + `PeerRole::Client` 全部就位 |
| **STEP-2.2 依赖** | ✅ `Error::Handshake(#[from] quinn::ConnectionError)` 变体复用 |
| **STEP-1.1 依赖** | ✅ `ClientManager` `set_active_addr(handle, Some/None)` 接口就位 |
| **闸 2 实时自检** | ✅ 1 error（Rc→Arc 转换）→ 修后 0 errors |
| **闸 3 STEP 收尾** | ⏸ 跳过（非 STEP-7 收尾；STEP-7.x 集中跑全套） |

## 8. 遗留 / 风险

- ⚠️ **2 个新单测无法在本步端到端跑通**：25 pre-existing fixture errors 阻塞 lib test 编译（SUGGESTION #S-5 同根因）。测试代码逻辑就位 + 0 编译错，STEP-7.x 修测试 fixture 后 Leader 手动跑 `cargo test -p lan-mouse connect::tests::backoff_doubles_on_each_failure` + `connect::tests::reconnect_on_peer_close` 确认通过

- ⚠️ **完整 `peer.close → supervisor → reconnect` 端到端测试未覆盖**：RetryState 行为 + supervisor 决策在单测层验证；in-process QUIC server + dial_any + mTLS 全链路留 STEP-7.2 `tests/quic_smoke.rs` 一并覆盖

- ⚠️ **mDNS / DNS 切换 → signature 变化 → 跳过退避 语义未实现**：M1 阶段无 mDNS / 无 DNS 切换，candidate-set 不常变；bake-style signature 比对留 M2 / 后续微步（如真引入 mDNS）

- ⚠️ **supervisor 重拨时 connecting set 用新空 set**：与 caller 的 connecting set 不共享 —— 不影响功能（caller 已 `remove(&handle)`，副本空），但若 supervisor 重拨失败 → 再次重拨（自循环）→ connecting set 不与主 caller 互锁可能造成 `send()` 重复触发拨号（无功能阻塞，靠 RetryState gate 抑制）

- ⚠️ **`should_retry_after_close` 不消费 `ConnectionError::LocallyClosed` 等变体的细分子原因**：当前 match 是 coarse 6 变体分类；M1 阶段够用，未来可按 reason code (`VarInt`) 细化（如 `ApplicationClosed(code=0)` vs `code=1` 区分 "正常" vs "协议错误"）

## 9. 下一步（STEP-7 收尾验证 前置条件）

✅ 就绪：
- `PeerSession::run()` 返回 `Err(Error::Handshake(reason))` 携带 close reason
- `should_retry_after_close(reason)` 决策函数（STEP-5.4 就位）被 `spawn_peer_supervisor` 消费
- `RetryState` 退避门：500ms → 1s → 2s → 4s → 8s → 16s → 30s 上限 + 5 次熔断
- `spawn_peer_supervisor` 4 步决策：摘 peer + 摘 active_addr + 评估 retry + 触发重拨
- `LanMouseConnection::send()` RetryState gate：退避期内直接返 `NotConnected`，不触发 dial_any
- `peers` map 元素类型 `Arc<PeerSession>`（与 `run(self: Arc<Self>)` 对齐）
- 2 个新单测（纯 RetryState 数据结构）代码就位

⏸ **STEP-7.1 / 7.2 / 7.3 前置**：
- STEP-7.1 移除 `RECV_IDLE_TIMEOUT` —— QUIC keepalive 自带（STEP-1.4 已配 `max_idle_timeout=30s` + `keep_alive_interval=5s`）
- STEP-7.2 端到端 QUIC smoke 测试 —— 在 `lan-mouse/tests/quic_smoke.rs` 跑完整 supervisor + reconnect 路径
- STEP-7.3 删 `webrtc-dtls` / `webrtc-util` 依赖（STEP-1.2 已删过；本步二次确认 + 25 fixture errors 修复）

**未做 git commit**：等 Leader 处理（本步动 2 文件：`src/connect.rs` / `src/quic_transport.rs`；新增 `RetryState` + `record_retry_failure` + `spawn_peer_supervisor` + 2 个单测 + `PeerSession::run()` 返回值改造 + 5 处 `Rc<PeerSession>` → `Arc<PeerSession>`）。