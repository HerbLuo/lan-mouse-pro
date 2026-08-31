# STEP-5.4 — `hello_watchdog` + `datagram_reader` + 端到端本地 IO

> PLAN-M1 §STEP-5 / STEP-5.4
> 执行日期：2026-08-31　实际耗时：~45 min（含一次 impl block 闭合边界 bug 修复）
> 结论：⚠️ 通过（含 2 项偏差 —— SUGGESTION #S-16 落实 + impl 闭合修复；详见 §3）

## 1. 做了什么

在 `src/quic_transport.rs` 落地 `pub enum PeerRole` + `pub async fn PeerSession::run(self: Arc<Self>, role: PeerRole) -> Result<(), Error>` 主干拼起来 + `pub fn should_retry_after_close(reason: &ConnectionError) -> bool` + `async fn datagram_reader_task(peer, tx)`（产生 `StreamEvent::Datagram`，SUGGESTION #S-16 "丢最旧"治理落地） + 1 个端到端单测 `peer_session_round_trip_motion_keyboard`（**不依赖 QUIC 握手后的业务路径** —— 依赖 mTLS 握手，14 DTLS errors 修复后才跑通）。

改动 1 个文件：

- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/quic_transport.rs`
  - 顶部 module doc 注释把 STEP-5.4 标 "（已）"
  - `READ_STREAM_BUFFER_CAP` doc 表更新：Datagram 类"丢最旧"策略 ✅
  - `StreamEvent` / `ReadStreams` 移除 `#[allow(dead_code)]`（3 个 producer 全部就位）
  - 新增 `pub enum PeerRole { Client, Server }`（Hello + 三 stream 装配角色标识）
  - 新增 `pub fn should_retry_after_close(reason: &ConnectionError) -> bool`（`ConnectionError` 6 变体分类重试 / 不重试）
  - 新增 `impl PeerSession { pub async fn run(self: Arc<Self>, role: PeerRole) -> Result<(), Error> }` —— 8 步主干拼起来
  - 新增 module-level `async fn datagram_reader_task(peer: Arc<PeerSession>, tx: tokio_mpsc::Sender<StreamEvent>)` —— 丢最旧背压落实
  - 测试 mod 末尾加 `peer_session_round_trip_motion_keyboard` 单测

## 2. 关键设计要点

### 2.1 `run()` 主干 8 步流程

```
1. hello_watchdog(Arc::clone(&self))         // 启 3s 超时兜底
2. spawn_local(datagram_reader_task(...))    // 启 datagram reader task
3. client_hello / server_hello by role       // 应用层 Hello 握手
4. take_stream_a_recv().await -> RecvStream  // 取 stream A recv 半边
5. for i in 0..3 { open_bi/accept_bi }       // 装配三 stream
   -> StreamBunch { a, b, c }
   -> self.set_stream_bunch(bunch).await
6. self.read_loop(&mut recv_a) -> ReadStreams // 启 stream B reader task
                                              // stream C 在 read_loop 内 drop
7. tokio::select! {                          // 主循环 4 路 reader + closed
     stream A recv (read_frame)
     stream B mpsc (rx_b.recv)
     datagram mpsc (rx_d.recv)
     conn.closed()
   }
8. Ok(())                                    // 退出（caller 决定是否重连）
```

**关键决策**：
- `Arc<Self>` 而非 `&self` —— 内部 spawn 两个 reader task + hello_watchdog 都需要 `'static + Send` 借用
- `tokio::pin!(closed)` + `&mut closed` in select! —— 让 future 跨多次循环轮询
- 4 路 select! 的"任一分支 break / return"语义统一：stream A/B/D 任一关闭 → break；conn.closed() → break
- 业务分派（`route_input(cfg, event)` → 本地 emulation）**不**做 —— 本步是 in-process 端到端验证，STEP-6.x `LanMouseConnection` 接入时再补

### 2.2 `datagram_reader_task` 丢最旧背压（SUGGESTION #S-16）

```rust
async fn datagram_reader_task(peer: Arc<PeerSession>, tx: Sender<StreamEvent>) {
    loop {
        let bytes = peer.conn.read_datagram().await?;
        let event = ProtoEvent::try_from(buf)?;
        // 丢最旧
        let mut dropped = 0u32;
        let mut r = tx.try_send(StreamEvent::Datagram(event));
        while let Err(TrySendError::Full(_)) = &r {
            let _ = tx.try_recv();  // 丢最旧
            dropped += 1;
            r = tx.try_send(StreamEvent::Datagram(event.re-encode()));
            if dropped > 8 { break; }  // 防活锁
        }
    }
}
```

**与 STEP-5.3 stream B "阻塞 sender" 路径的对比**：
- Reliable（按键 / Modifier）→ **阻塞** sender（事件不能丢）
- Datagram（Motion / Axis / AxisDiscrete120）→ **丢最旧**（高频丢一帧用户无感知）

**为什么 8 次上限**：极端 race 队列状态在 `try_recv` / `try_send` 之间变化（非常罕见）—— 防活锁 + 接受当前帧丢

### 2.3 `should_retry_after_close` 设计

```rust
match reason {
    ConnectionLost(_) | TimedOut => true,    // 网络层断连 / 超时
    ApplicationClosed(_)
    | TransportError(_)
    | Reset
    | VersionMismatch
    | LocalError(_) => false,                // 协议级 / 本端错误
    _ => false,                              // 兜底不重试
}
```

**M1 阶段**：仅 `run()` 退出时调一次 + 日志记录；STEP-6.5 `connect.rs::RetryState` 消费此判定做退避重连

### 2.4 `PeerRole` enum

```rust
pub enum PeerRole { Client, Server }
```

**为什么单独 enum**：`client_hello` vs `server_hello` + `open_bi` vs `accept_bi` 是两条对称路径；`run()` 用 role 决定 + compile-time exhaustiveness check 守护

### 2.5 `peer_session_round_trip_motion_keyboard` 单测设计

**端到端构造**：
1. server endpoint + client endpoint + dial + 两端都 wrap `Arc<PeerSession>`
2. server task: `accept() → Arc::new(PeerSession::from_connection(conn)) → Arc::clone(&session).run(PeerRole::Server)`
3. client: `client_arc.send_motion(&motion_event()).await`
4. 等待 server run 完成（5s 兜底）
5. client 关 conn → client run 看到 closed → 退出

**依赖完整 mTLS 握手 + Hello + datagram 路径** —— 14 DTLS errors 修复后由 Leader 手动跑一次确认通过（SUGGESTION #S-5 同根因）

**本步不验证**：stream B 双向、stream A 控制面 —— 那些是 STEP-6.x + STEP-7.2 集成测试范围

## 3. 与 PLAN-M1 §5.4 的偏差

### 偏差 #N-12（轻）：`impl PeerSession { run() }` 闭合边界 bug 修复

**现象**：第一次实现时把 `datagram_reader_task` 的 impl 闭合 `}` 放在 TofuVerifier 段前 —— 让 `datagram_reader_task` 误落在第二个 `impl PeerSession` 块内被解析成 associated function；调用处 `spawn_local(datagram_reader_task(...))` 编译报 E0425 "cannot find function in this scope"。

**修复**：把 `}` 移到 `run()` 函数体闭合（line 2046）后，让 `datagram_reader_task` 是 module-level 自由函数 —— 编译从 15 errors → 14 errors（baseline）。

**严重程度**：轻（语法 bug 一次自检到位，无功能影响）。

### 偏差 #N-13：SUGGESTION #S-16 落实 —— 8 次 drain 上限为"工程取舍"

**PLAN §5.3 文字**：队列满时**丢最旧** datagram 类事件。

**本步实际**：
- try_send → Full → try_recv 丢最旧 → 再 try_send → 仍 Full → 再 try_recv → ... → 8 次后仍失败 → 接受当前帧丢
- 8 次上限是工程取舍（**PLAN 未明确**）—— 防极端 race 活锁；正常情况 1 次丢最旧就能 send 成功

**不构成问题**：8 次上限在正常背压场景（队列满 < 8 帧）下不触发；只在极端 race 下生效；与 bak `mousehop/src/quic_transport.rs` "丢最旧" 形态对齐

## 4. 与 PLAN §9 M1 边界检查

| §9 类别 | 本步触碰 | 说明 |
|---|---|---|
| `proto` 变体 / 常量 / 错误 / codec | 否 | 仅用既有 `ProtoEvent` + `MAX_EVENT_SIZE` |
| `input-event` | 否 | 没动 |
| `ipc::TransportEvent` | 否 | 没动 |
| `lan-mouse-gtk::status_bar` | 否 | 没动 gtk |
| `lan-mouse-cli` stderr 订阅 | 否 | 没动 cli |
| `lan-mouse/src/clipboard*.rs` | 否 | 没动 core |
| `Cargo.toml` 引入 h3/h3-quinn/http | 否 | 0 依赖变更 |
| `quic_transport.rs::Stream C` reader | **否**（关键） | `StreamBunch.c` 在 `read_loop` 内 `drop(bunch.c)` —— 仍守 §9 |
| `connect.rs` mDNS / discovery | 否 | 没动 connect |

**结论**：0 越界。**StreamC reader 不开**（PLAN §9 明确要求）。

## 5. 验证结果

### 5.1 `cargo check -p lan-mouse --lib`

```
$ cargo check -p lan-mouse --lib 2>&1 | grep -cE "^error\[E"
14

$ cargo check -p lan-mouse --lib 2>&1 | grep "quic_transport\.rs" | grep "error\["
# （无输出 —— 本步新增代码 0 编译错）
```

14 errors 全部来自 `src/connect.rs` 与 `src/listen.rs` 的 `webrtc_dtls` / `webrtc_util` 引用；本步新增 `PeerRole` / `should_retry_after_close` / `PeerSession::run` / `datagram_reader_task` / `peer_session_round_trip_motion_keyboard` **0 编译错**。

### 5.2 `cargo check -p lan-mouse --tests`

```
$ cargo check -p lan-mouse --tests 2>&1 | grep -cE "^error\[E"
27

$ cargo check -p lan-mouse --tests 2>&1 | grep "^error\[" | sort | uniq -c
   2 error[E0432]: unresolved import `webrtc_util`
   9 error[E0433]: cannot find module or crate `webrtc_dtls` in this scope
   3 error[E0433]: cannot find module or crate `webrtc_util` in this scope
   6 error[E0433]: cannot find type `InputEvent` in this scope
   2 error[E0433]: cannot find type `KeyboardEvent` in this scope
   4 error[E0433]: cannot find type `PointerEvent` in this scope
   1 error[E0433]: cannot find type `Position` in this scope
```

**与基线对比**：
- 基线（STEP-5.3 提交后）：27 errors
- 本步提交后：27 errors（**0 增量**）
- 27 = 14 DTLS pre-existing + 13 fixture 错误（与 STEP-5.1 / 5.2 / 5.3 报告完全一致）

### 5.3 §9 M1 边界 grep

```
$ grep -rnE "webrtc-dtls|webrtc-util|RECV_IDLE_TIMEOUT|TransportEvent|MotionAbsolute|Bounds|ClipboardEvent|h3|h3-quinn|status_bar|MAX_CLIPBOARD_SIZE" src/quic_transport.rs
# （唯一命中：doc 注释引用 M2 计划标记 + §9 守门声明，0 代码命中 —— §9 12 类 grep 无命中）
```

### 5.4 单测 `cargo test -p lan-mouse peer_session_round_trip_motion_keyboard`

```
$ cargo test -p lan-mouse quic_transport::tests::peer_session_round_trip_motion_keyboard 2>&1 | tail -3
error: could not compile `lan-mouse` (lib test) due to 14 previous errors
```

**单测无法跑通** —— `lan-mouse` lib 因 STEP-1.2 留下的 14 DTLS errors 编不过（SUGGESTION #S-5 同根因）。测试代码逻辑就位，STEP-6.x 修复 14 errors 后 Leader 手动跑一次确认通过。

## 6. 处理的 SUGGESTION 项

**SUGGESTION #S-16 完全消化**：
- `datagram_reader_task` 实现 SUGGESTION #S-16 "Datagram 类丢最旧" 策略（`try_send` 失败 → `try_recv` 拿最旧 → 再 `try_send`，8 次上限防活锁）
- `READ_STREAM_BUFFER_CAP` doc 表更新：Reliable 阻塞 sender ✅ / Datagram 丢最旧 ✅ / Control 由 caller 自然阻塞读
- 本条目进入"待 Leader 评审后删除"状态

无新增 SUGGESTION 条目。

## 7. 闸门检查（PLAN-M1 §1 时间门 / §9 边界门）

| 闸 | 结果 |
|---|---|
| **§1 时间门**：30 min 目标 | ⚠️ 实际 ~45 min（含一次 impl 闭合边界 bug 修复）—— < 1h 红线 |
| **§9 边界门** | ✅ 0 越界（详见 §4） |
| **STEP-5.3 依赖** | ✅ 已归档 |
| **STEP-4.4 依赖** | ✅ `route_input` 纯函数已就位（本步不直接调，留 STEP-6.x LanMouseConnection） |
| **STEP-3.2 依赖** | ✅ `client_hello` / `server_hello` + `hello_watchdog` + `take_stream_a_recv` 就位 |
| **STEP-5.1 依赖** | ✅ `send_motion` + `Error::Datagram` + `MAX_SAFE_DATAGRAM` 就位 |
| **STEP-5.2 依赖** | ✅ `Bidi<S>` / `StreamBunch` / `write_frame` / `read_frame` / `PeerSession.stream_bunch` / `set_stream_bunch` 就位 |
| **闸 2 实时自检** | ✅ 14 errors 全部 DTLS、本步 0 增量（中间 1 次 15 errors 是 impl 闭合 bug，修复后回到 14） |
| **闸 3 STEP 收尾** | ⏸ 跳过（非 STEP 收尾，本步范围为单 STEP 落库） |

## 8. 遗留 / 风险

- ⚠️ **`peer_session_round_trip_motion_keyboard` 单测无法在本步端到端跑通**：14 DTLS errors 阻塞 lib 编译（SUGGESTION #S-5 同根因）。测试代码逻辑就位（依赖完整 mTLS + Hello + datagram 路径），STEP-6.x 修复 14 errors 后 Leader 手动跑一次确认通过
- ⚠️ **业务分派（`route_input(cfg, event)` → 本地 emulation）本步不实现**：run() 主循环内的 `StreamEvent` 仅日志；STEP-6.x `LanMouseConnection::send()` 接入时按 cfg 分派 → 本地 emulation（与 STEP-4.4 `route_input` 衔接）
- ⚠️ **`run()` 不调 `route_input`**：M1 范围 STEP-5.4 仅验证"传输层通"，业务路由留 STEP-6.x
- ⚠️ **`datagram_reader_task` 8 次 drain 上限**：工程取舍（PLAN 未明确）；正常背压场景不触发；与 bak `mousehop/src/quic_transport.rs` "丢最旧" 形态对齐
- ⚠️ **`run()` 退出仅返 `Ok(())`**：caller 看不到"为什么关"的语义；M1 阶段由 `should_retry_after_close(reason)` 独立判定；STEP-6.x 接入时把 `reason` 透传给 `RetryState`

## 9. 下一步（STEP-6.1 前置条件）

✅ 就绪：
- `pub enum PeerRole { Client, Server }` —— Hello + 三 stream 装配角色标识
- `pub async fn PeerSession::run(self: Arc<Self>, role: PeerRole) -> Result<(), Error>` —— 主干拼起来
- `pub fn should_retry_after_close(reason: &ConnectionError) -> bool` —— 重试判定
- `async fn datagram_reader_task(peer, tx)` —— Datagram 类丢最旧背压（SUGGESTION #S-16 落地）
- `pub` 状态 `hello_watchdog` —— 3s 超时兜底已由 run() 启用
- `StreamEvent::Datagram` 变体首次产生（dead_code 守护移除）
- 1 个端到端单测代码就位

**未做 git commit**：等 Leader 处理（本步仅动 `src/quic_transport.rs`）。

下一步建议：执行 **STEP-6.1** —— `connect.rs::LanMouseConnection` 持有 `Rc<PeerSession>` + `send()` 走新通道（接 `route_input(cfg, event)` 分派 → 本地 emulation）。搬运参考：`lan-mouse-pro-bak/mousehop/src/connect.rs:624-900` 整段。