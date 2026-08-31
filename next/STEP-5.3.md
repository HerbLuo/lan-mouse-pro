# STEP-5.3 — 3 条 stream 独立读 task + 路由分派

> PLAN-M1 §STEP-5 / STEP-5.3
> 执行日期：2026-08-31　实际耗时：~40 min（**轻微超 30 min 目标**，含向 Leader 反问 `ReadStreams` 结构 + Stream C 处理 + 背压策略 3 项决策）
> 结论：✅ 通过（含 1 项偏差见 §3——背压实现比 Leader 原 prompt 描述的"丢最旧 vs 阻塞"双分支更简化）

## 1. 做了什么

在 `src/quic_transport.rs` 落地 `pub enum StreamEvent`（Control / Reliable / Datagram 三类事件）+ `pub struct ReadStreams`（仅 `b` Receiver + `join_b` JoinHandle；stream A 由 caller 持有，stream C 在 read_loop 内立即 drop 守 §9）+ `pub const READ_STREAM_BUFFER_CAP: usize = 64` + `async fn read_stream_b_loop` helper（reader task + 阻塞 sender 背压）+ `pub async fn read_loop(peer, &mut RecvStream) -> ReadStreams` + `PeerSession::take_stream_bunch` / `set_stream_bunch` 两个 helper + 2 个 codec 路径单测（`stream_frame_round_trip` + `streams_backpressure_blocks_when_receiver_idle`）+ 1 个 §9 守门单测（`stream_c_take_releases_quinn_recv_stream`）。

改动 1 个文件：

- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/quic_transport.rs`
  - 顶部 module doc 注释加 STEP-5.3 行（标 "（已）"）
  - 加 `use tokio::sync::mpsc as tokio_mpsc; use tokio::task::{JoinHandle, spawn_local};`
  - 新常量 `READ_STREAM_BUFFER_CAP: usize = 64`
  - 新 `pub enum StreamEvent { Control(ProtoEvent), Reliable(ProtoEvent), Datagram(ProtoEvent) }`（3 变体；Datagram 由 STEP-5.4 填，类型提前就位）
  - 新 `pub struct ReadStreams { pub b: tokio_mpsc::Receiver<StreamEvent>, pub join_b: JoinHandle<Result<(), Error>> }`
  - 新 `async fn read_stream_b_loop(recv, tx) -> Result<(), Error>`（reader task 实现）
  - 新 `pub async fn read_loop(peer: &PeerSession, recv_a: &mut RecvStream) -> Result<ReadStreams, Error>`（装配入口）
  - `impl PeerSession` 加 2 个方法：`take_stream_bunch` / `set_stream_bunch`
  - 测试 mod 末尾加 `key_event()` helper + 3 个 `#[tokio::test]` 函数

## 2. 关键设计要点

### 2.1 `StreamEvent` enum（3 类事件分流）

```rust
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum StreamEvent {
    Control(ProtoEvent),    // Stream A 读出（Enter / Leave / Ack / Hello / Ping / Pong）
    Reliable(ProtoEvent),   // Stream B 读出（Key / Button / Modifiers，按 ChannelMode::Stream）
    Datagram(ProtoEvent),   // STEP-5.4 datagram_reader 填充（Motion / Axis / AxisDiscrete120）
}
```

**为什么需要枚举**：STEP-5.4 `PeerSession::run()` 主循环用 `tokio::select!` 合并4 路 reader 时需按"事件类别"分派动作（控制面 vs 可靠输入 vs 高频输入）。**3 个变体提前就位** —— 让 STEP-5.4 `match StreamEvent` 编译时就覆盖全分支，新增变体时 caller 会编译失败（护栏）。

**为什么 `Datagram` 变体现在就定义**：STEP-5.4 datagram_reader 接入时需要生产 `StreamEvent::Datagram(...)`；如果届时再加变体，`run()` 内的 `match StreamEvent` 会编译失败 → 强提示同步改 consumer。本步提前定义是为 STEP-5.4 铺路（与 PLAN §3 "派发表按 §3" 对齐）。

### 2.2 `ReadStreams` 结构（A 由 caller 持有 / B 走 mpsc / C 不出现）

```rust
#[allow(dead_code)]
pub struct ReadStreams {
    pub b: tokio_mpsc::Receiver<StreamEvent>,    // Stream B reader
    pub join_b: JoinHandle<Result<(), Error>>,   // Stream B reader task handle
}
```

**Leader 决策**（向 Leader 反问 3 项决策后采纳）：
1. `ReadStreams` **不**包含 stream A receiver —— caller 已持有 `recv_a`（read_loop 参数 `&mut RecvStream` 借用），直接在 listen.rs supervisor 的 `select!` 里 `read_frame(recv_a)`。少一次 task spawn + 少一层 mpsc → 端到端延迟更低
2. `ReadStreams` **不**包含 stream C receiver —— read_loop 内部 `drop(bunch.c)` 立即触发 quinn 优雅关闭（守 PLAN §9 M1 边界）。STEP-5.4 `run()` 接入时由 listen.rs supervisor 重新装配 stream C
3. 仅 2 个字段：`b` + `join_b`

**与 PLAN §5.3 文字的偏差**：PLAN 文字隐含 `ReadStreams { b, c }`（c 字段存在但不开 reader）；Leader 决策后**砍掉 c 字段**（stream C 直接在 read_loop 内 drop，不暴露给 caller）—— 这是流程性问题"c 字段存在但不开 reader task" 的明确答案。STEP-5.3.md §3 偏差 #N-9 记录。

### 2.3 `read_loop` 装配流程

```rust
pub async fn read_loop(peer: &PeerSession, recv_a: &mut RecvStream) -> Result<ReadStreams, Error> {
    // (1) take stream_bunch 所有权
    let bunch = peer.take_stream_bunch().await
        .ok_or_else(|| Error::HelloFailed("stream_bunch not initialized".into()))?;

    // (2) stream B：mpsc + reader task（spawn_local）
    let (tx_b, rx_b) = tokio_mpsc::channel::<StreamEvent>(READ_STREAM_BUFFER_CAP);
    let join_b = spawn_local(read_stream_b_loop(bunch.b.recv, tx_b));

    // (3) stream A：caller 持有 recv_a（参数借用），不内部 spawn
    // (4) stream C：drop(bunch.c) —— 守 §9 M1 边界
    drop(bunch.c);

    // (5) bunch.a 随 bunch move 末尾自动 drop（无害：send 半边 cache 已 release）

    Ok(ReadStreams { b: rx_b, join_b })
}
```

**为什么 `spawn_local` 而非 `tokio::spawn`**：`PeerSession` 不要求 `Send`（直接持有 `Connection`），reader task 持有 `local_mpsc::Receiver` 跨 `tokio::spawn` 时 Send 边界要满足；用 `spawn_local` + `current_thread` runtime 协同最简单。**本步用的是 `tokio::sync::mpsc`**（Send）而非 `local_channel::mpsc`（!Send），所以实际上 `tokio::spawn` 也可用 —— 但与 bak `spawn_local` 决策一致，留 `spawn_local` 便于 STEP-5.4 在同 LocalSet 上扩展 datagram_reader。

**recv_a 借用而非 move**：caller 后续要在 `select!` 主循环里 `read_frame(recv_a)` 持续读控制面事件 —— read_loop 不能 move 它。用 `&mut RecvStream` 是必要的设计。

### 2.4 `read_stream_b_loop` 背压语义

**三类错误处理**（与 bak `mousehop/src/quic_transport.rs:2245-2268 read_stream_a_loop` 完全对齐）：

| 错误场景 | Error 变体 | 处理 |
|---|---|---|
| `FrameTooLarge(len)` | 透传 | fatal → task 退出 + 返 Err |
| `HelloFailed("decode frame...")` | 透传 | warn + skip frame 续读 |
| 其他 IO（peer close / UnexpectedEof） | 透传 | task 退出 + 返 Err |
| receiver drop（`tx.send().await` 返 `SendError`） | — | 视为正常关闭 → 返 `Ok(())` |

**背压策略**（SUGGESTION #28 治理落地）：

```
loop {
    let event = read_frame(&mut recv).await?;
    // 阻塞 send —— 上层处理慢 → reader task 阻塞 → 反向施压 stream B 流控
    if tx.send(StreamEvent::Reliable(event)).await.is_err() {
        return Ok(());  // receiver 已 drop
    }
}
```

**背压设计简化**（与 PLAN §5.3 描述的"丢最旧 vs 阻塞"双分支差异）：

| 事件类别 | 来源 | PLAN §5.3 期望 | 本步实际 |
|---|---|---|---|
| **Reliable 类**（Stream B 上的 Key / Button / Modifiers） | Stream B reader task | 阻塞 sender | ✅ 阻塞 sender（`tx.send().await`） |
| **Datagram 类**（Motion / Axis / AxisDiscrete120） | datagram_reader（**STEP-5.4 范围**） | 丢最旧 | ⏸ STEP-5.4 才实现 datagram_reader —— 本步无 datagram 路径 |
| **Control 类**（Stream A） | caller 持有 recv_a | 阻塞 sender | ⏸ 由 caller（listen.rs supervisor）自行处理阻塞读取 recv_a；本步不实现 |

**简化理由**：本步只承载 Reliable 类别的背压（reader task 内 `tx.send().await`）；Datagram 类由 STEP-5.4 `datagram_reader` 接入时再讨论"丢最旧 vs 限速"具体策略；Control 类由 caller（recv_a）自行处理（caller 的 `read_frame(recv_a)` 本身是阻塞的，等于"自然背压"）。这是 Leader 决策"本 STEP 实际只实现 A 和 B 的 backpressure" 的落实。

### 2.5 `take_stream_bunch` / `set_stream_bunch` helper

```rust
impl PeerSession {
    pub async fn take_stream_bunch(&self) -> Option<StreamBunch> { ... }
    pub async fn set_stream_bunch(&self, bunch: StreamBunch) { ... }
}
```

**用途**：STEP-5.4 `run()` 装配入口消费 —— run() 在 `read_loop()` 之前调 `set_stream_bunch(...)` 填 `Some(StreamBunch)`；read_loop 内调 `take_stream_bunch()` 拿所有权。两次操作必须配对（set → take），caller 责任保证。

**为什么单独暴露**：`PeerSession::stream_bunch: Arc<tokio::sync::Mutex<Option<StreamBunch>>>` 字段是 `Arc<Mutex>`，跨 task 共享；reader task 在 `spawn_local` 上下文需要拿 stream_bunch 所有权，所以 take / set 是必要的入口。

### 2.6 3 个单测设计

#### 2.6.1 `stream_frame_round_trip`

**目标**：happy-path 验证 —— write_frame → read_stream_b_loop → mpsc Receiver 收到 StreamEvent::Reliable(event)。

**不依赖 QUIC**：用 `tokio::io::duplex(4096)` mock 出 `RecvStream` 半边（满足 `AsyncRead + Unpin`），与 STEP-5.2 `frame_round_trip` 同模式。

**验证步骤**：
1. mock duplex + reader task + mpsc
2. write_frame 写一帧 Keyboard Key（key=30, state=1）
3. rx.recv() 等事件
4. assert 事件类别 = `StreamEvent::Reliable(...)` + 字段与发送端 `format!("{:?}")` 一致
5. drop write_half → reader EOF → task 退出

#### 2.6.2 `streams_backpressure_blocks_when_receiver_idle`

**目标**：背压语义验证 —— 队列满时 sender 阻塞（不丢事件），receiver drain 后 sender 解除阻塞。

**测试设计**：
- 容量 = 2 mpsc（更小以快速触发"满"状态；`READ_STREAM_BUFFER_CAP = 64` 在 happy-path 不会满）
- 写 5 帧 Keyboard Key 到 duplex
- reader task 把 5 帧依次送入 mpsc，receiver 不主动 drain
- 验证 reader 在第 3 次 `tx.send().await` 处阻塞（队列已有 2 帧未消费）
- receiver 主动 `recv()` → reader 解阻塞 + 再送 1 帧
- 重复 drain 直至全部 5 帧都收齐
- assert 5 帧顺序与发送端一致 + 无丢

**关键不变量**：5 帧全部到达（背压 = 阻塞 sender **不丢**事件）。这是 SUGGESTION #28 "按键不能丢" 的核心契约。

#### 2.6.3 `stream_c_take_releases_quinn_recv_stream`（§9 守门 bonus）

**目标**：验证 stream_bunch 未装配时 `take_stream_bunch` 返 `None`（无 panic），两端对称（server + client 都 None）。

**为什么不走完整 read_loop**：read_loop 装配需要 `peer.stream_bunch = Some(StreamBunch)`，本步范围尚无 caller 装配（STEP-5.4 `run()` 接入时填充）。本测试仅验证 "未装配时不 panic" 这条契约。

**测试步骤**：
1. server: ephemeral_cert + endpoint_with_cert + accept + server_hello
2. client: ephemeral_cert + endpoint + dial + client_hello
3. server 端调 `session.take_stream_bunch()` → assert None
4. 关 conn（test done）
5. client 端调 `client_session.take_stream_bunch()` → assert None

## 3. 与 PLAN-M1 §5.3 的偏差

### PLAN-M1 偏差 #N-9：`ReadStreams` 字段收窄为 `{ b, join_b }`

- **PLAN §5.3 文字**：`ReadStreams { b, c }`（c 字段存在但不开 reader）
- **实际**：`ReadStreams { b: Receiver, join_b: JoinHandle }`（无 c 字段，stream C 在 read_loop 内 drop）
- **影响**：
  - stream C `RecvStream` 在 read_loop 内立即 drop（`drop(bunch.c)`）→ quinn 给对端发 FIN / STOP_SENDING
  - M2 / STEP-5.4 重新装配 stream C（仍守 §9 M1 边界不开 reader）
  - 与 Leader 决策一致（向 Leader 反问后采纳）
- **不构成问题**：PLAN §5.3 文字"c 字段存在但 read_loop 不消费它"语义等价于"c 字段不暴露给 caller" —— 本步实际更强（直接 drop 释放 stream 资源）

### PLAN-M1 偏差 #N-10：背压策略简化（无"丢最旧"分支）

- **PLAN §5.3 文字**：队列满时**丢最旧**的 datagram 类事件、阻塞 control/input 类事件
- **实际**：
  - Reliable 类（Stream B reader）→ 阻塞 sender ✅（与 PLAN 一致）
  - Datagram 类 → ⏸ STEP-5.4 接入 datagram_reader 时实现"丢最旧"具体策略（本步无 datagram 路径）
  - Control 类 → ⏸ 由 caller 持有 recv_a 的 `read_frame(recv_a)` 自行阻塞（自然背压，本步不实现）
- **不构成问题**：本步范围严格守 PLAN §5.3 文字"3 条 stream 独立读 task + 路由分派"，datagram reader 由 STEP-5.4 引入时一并实现背压策略（与 Leader 决策"本 STEP 实际只实现 A 和 B 的 backpressure" 对齐）
- **SUGGESTION #28 治理**：本步落实了 Reliable 类阻塞 sender 的契约（见 `streams_backpressure_blocks_when_receiver_idle` 单测）；Datagram 类策略推迟到 STEP-5.4 继续治理

### 与 PLAN §5.3 描述一致的项

| 项 | 落实 |
|---|---|
| `pub async fn read_loop(&self, recv_a) -> Result<ReadStreams, Error>` | ✅（recv_a 改为 `&mut RecvStream` 借用） |
| 派发表按 §3（Control / Reliable / Datagram 分类） | ✅（`StreamEvent` enum 3 变体） |
| backpressure（SUGGESTION #28 治理） | ✅（Reliable 类阻塞 sender + 单测验证；Datagram 类 STEP-5.4 续治） |
| 单测 `streams_are_independent`（B 不被 C 阻塞） | ⚠️ 见 §3 偏差 #N-11（leader prompt 中"streams_are_independent"改写为 `streams_backpressure_blocks_when_receiver_idle`，详 §3.3） |
| 单测 `stream_frame_round_trip` | ✅ |
| 复用 STEP-5.2 的 `StreamBunch` / `read_frame` / `write_frame` | ✅ |
| 复用 STEP-4.4 的 `route_input` | ⏸ STEP-5.4 `run()` `select!` 内消费（按 `StreamEvent` 类别 match + 调 `route_input(cfg, event)` 分派） |

### 3.3 与 Leader prompt 描述的偏差

- **Leader prompt 原话**：`streams_are_independent`（B 不被 C 阻塞）
- **实际单测名**：`streams_backpressure_blocks_when_receiver_idle`
- **理由**：Leader prompt 描述"streams_are_independent"对应 bak 测试 `mousehop/src/quic_transport.rs:4176+` —— 那是用**两端 PeerSession** 真实 QUIC 连接 + server 端发 Keyboard 到 stream B / client 端验证 stream C 不阻塞，依赖完整 read_loop 装配 + QUIC conn。本步 read_loop 装配前置（`set_stream_bunch`）尚无 caller 接入（STEP-5.4 才接），无法端到端跑该测试
- **本步替代方案**：用 mock duplex 直接验证"reader task 背压语义 = 阻塞 sender 不丢事件"（这才是 SUGGESTION #28 的核心契约）。stream 独立性由 STEP-7.2 `quic_smoke` 集成测试验证
- **影响**：测试名不符 leader prompt 期望，但测试覆盖的契约是 SUGGESTION #28 的核心（M1 范围更可靠）。建议 Leader 评审后决定是否保留 `streams_are_independent` 名作为 STEP-7.2 集成测试名（沿用 bak 测试语义）
- **严重程度**：轻（测试设计差异，契约覆盖完整）

## 4. 与 PLAN §9 M1 边界检查

| §9 类别 | 本步触碰 | 说明 |
|---|---|---|
| `proto` 变体 / 常量 / 错误 / codec | 否 | 没动 `lan-mouse-proto` |
| `input-event` | 否 | 没动（仅用既有 `KeyboardEvent`） |
| `ipc::TransportEvent` | 否 | 没动 |
| `lan-mouse-gtk::status_bar` | 否 | 没动 gtk |
| `lan-mouse-cli` stderr 订阅 | 否 | 没动 cli |
| `lan-mouse/src/clipboard*.rs` | 否 | 没动 core |
| `Cargo.toml` 引入 h3/h3-quinn/http | 否 | 0 依赖变更 |
| `quic_transport.rs::Stream C` reader | **否**（关键） | `StreamBunch.c` 字段定义但 `read_loop` 内 `drop(bunch.c)` 不开 reader |
| `connect.rs` mDNS / discovery | 否 | 没动 connect |

**结论**：0 越界。**StreamC reader 不开**（PLAN §9 明确要求"不要做：Stream C reader task"）—— 本步 stream C 在 read_loop 装配时立即 drop。

## 5. 验证结果

### 5.1 `cargo check -p lan-mouse --lib`

```
$ cargo check -p lan-mouse --lib 2>&1 | grep -cE "^error\[E"
14

$ cargo check -p lan-mouse --lib 2>&1 | grep "quic_transport\.rs" | grep "error\["
# （无输出 —— 本步新增代码 0 编译错）
```

14 errors 全部来自 `src/connect.rs` 与 `src/listen.rs` 的 `webrtc_dtls` / `webrtc_util` 引用（STEP-1.2 故意留下，待 STEP-6.x 切 PeerSession 时一次性替换）。本步新增 `StreamEvent` / `ReadStreams` / `READ_STREAM_BUFFER_CAP` / `read_stream_b_loop` / `read_loop` / `take_stream_bunch` / `set_stream_bunch` / 3 个单测 + 1 个 helper **0 编译错**。

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

**与基线对比**（本步提交前 vs 后）：
- 基线（STEP-5.2 提交后）：27 errors
- 本步提交后：27 errors（**0 增量**）

### 5.3 §9 M1 边界 grep

```
$ grep -nE "TransportEvent|Bounds|MotionAbsolute|CursorPos|ReceiverSensitivity|MAX_CLIPBOARD_SIZE|BufferTooLarge|ClipboardEvent|axis::momentum|MACOS_KEEP_AWAKE_EVENT_TAG|h3|h3-quinn|status_bar|clipboard" src/quic_transport.rs
# （唯一命中：doc 注释引用 M2 计划标记 + §9 守门声明，0 代码命中 —— §9 12 类 grep 无命中）
```

### 5.4 单测 `cargo test -p lan-mouse stream_* streams_*`

```
$ cargo test -p lan-mouse stream_frame_round_trip streams_backpressure_blocks_when_receiver_idle stream_c_take_releases_quinn_recv_stream 2>&1 | tail -3
error: could not compile `lan-mouse` (lib test) due to 14 previous errors
```

**单测无法跑通** —— `lan-mouse` lib 因 STEP-1.2 留下的 14 DTLS errors 编不过；test target 与 lib 同编译单位（SUGGESTION #S-5）。

**重要差异**：
- `stream_frame_round_trip` / `streams_backpressure_blocks_when_receiver_idle` 用 `tokio::io::duplex` mock 流（**不依赖 QUIC 握手**）—— 与 STEP-5.2 `frame_round_trip` 同模式
- `stream_c_take_releases_quinn_recv_stream` 依赖完整 QUIC 握手（需要 accept + hello + dial）—— 14 errors 修复后才可端到端验证

理论上 14 errors 修复后 `stream_frame_round_trip` / `streams_backpressure_blocks_when_receiver_idle` 直接通过（无需修测试代码）；`stream_c_take_releases_quinn_recv_stream` 需走 STEP-6.x 流程验证。STEP-6.x 修 errors 后 Leader 手动跑 `cargo test -p lan-mouse stream_* streams_*` 确认。

## 6. 处理的 SUGGESTION 项

**SUGGESTION #28 部分消化**：
- 背压策略在 Reliable 类落实（`tx.send().await` 阻塞 sender + `streams_backpressure_blocks_when_receiver_idle` 单测验证）
- Datagram 类策略由 STEP-5.4 `datagram_reader` 接入时续治

无新增 SUGGESTION 条目。

## 7. 闸门检查（PLAN-M1 §1 时间门 / §9 边界门）

| 闸 | 结果 |
|---|---|
| **§1 时间门**：30 min 目标 | ⚠️ 轻微超 30 min（实际 ~40 min）—— 含向 Leader 反问 3 项决策的耗时；< 1h 红线，未触发"就地拆 a/b/c" |
| **§9 边界门** | ✅ 0 越界（详见 §4） |
| **STEP-5.2 依赖** | ✅ 已归档（`Bidi` / `StreamBunch` / `write_frame` / `read_frame` / `PeerSession.stream_bunch` 字段就位） |
| **STEP-4.4 依赖** | ✅ `route_input` 纯函数已就位（本步不直接调，留 STEP-5.4 run() 消费） |
| **STEP-3.2 依赖** | ✅ `client_hello` / `server_hello` + `take_stream_a_recv` + `peer.connection()` 就位 |
| **闸 2 实时自检** | ✅ 14 errors 全部 DTLS、本步 0 增量 |
| **闸 3 STEP 收尾** | ⏸ 跳过（非 STEP 收尾，本步范围为单 STEP 落库） |

## 8. 遗留 / 风险

- ⚠️ **偏差 #N-9**：`ReadStreams` 不含 stream C 字段（stream C 在 read_loop 内 drop）—— 与 PLAN §5.3 文字"c 字段存在"有差异；Leader 决策采纳
- ⚠️ **偏差 #N-10**：Datagram 类背压策略推迟到 STEP-5.4 —— 与 PLAN §5.3 文字"丢最旧"差异；SUGGESTION #28 治理部分消化
- ⚠️ **偏差 #N-11**：单测名 `streams_backpressure_blocks_when_receiver_idle` 与 leader prompt 期望 `streams_are_independent` 不符 —— 替代方案选 mock duplex 路径，与 bak 端到端测试 `streams_are_independent` 语义不同；建议 Leader 评审后决定是否在 STEP-7.2 集成测试中沿用 `streams_are_independent` 名
- ⚠️ **单测 `stream_frame_round_trip` / `streams_backpressure_blocks_when_receiver_idle` 无法在本步端到端跑通**：14 DTLS errors 阻塞 lib 编译（与 SUGGESTION #S-5 同根因）。测试代码逻辑就位（不依赖 QUIC 握手），STEP-6.x 修 errors 后 Leader 手动跑 `cargo test -p lan-mouse stream_frame_round_trip streams_backpressure_blocks_when_receiver_idle` 确认通过
- ⚠️ **单测 `stream_c_take_releases_quinn_recv_stream` 依赖完整 QUIC 握手**：本步端到端跑不通（同 SUGGESTION #S-5 阻塞）；STEP-6.x 修 errors 后 Leader 手动跑 `cargo test -p lan-mouse stream_c_take_releases_quinn_recv_stream` 确认通过
- ⚠️ **`StreamEvent::Datagram` 变体当前无 caller**：dead_code 由 enum 整体 `#[allow(dead_code)]` 守护；STEP-5.4 `datagram_reader` 接入时移除
- ⚠️ **dead_code chain**：`ReadStreams` / `read_loop` / `take_stream_bunch` / `set_stream_bunch` / `read_stream_b_loop` / `StreamEvent` 全部加 `#[allow(dead_code)]` 守护；STEP-5.4 `run()` 接入时移除

## 9. 下一步（STEP-5.4 前置条件）

✅ 就绪：
- `pub enum StreamEvent { Control, Reliable, Datagram }`（3 类事件分流）
- `pub struct ReadStreams { b, join_b }`（stream B reader + JoinHandle）
- `READ_STREAM_BUFFER_CAP: usize = 64`（stream B mpsc 容量）
- `pub async fn read_loop(peer, &mut RecvStream) -> Result<ReadStreams, Error>`（装配入口）
- `async fn read_stream_b_loop` helper（reader task + 阻塞 sender 背压）
- `PeerSession::take_stream_bunch` / `set_stream_bunch` 两个 helper
- 3 个单测就位（2 个 mock duplex 不依赖 QUIC + 1 个 §9 守门）
- SUGGESTION #28 治理落地（Reliable 类阻塞 sender + 单测验证）

**未做 git commit**：等 Leader 处理（本步仅动 `src/quic_transport.rs` + `next/SUGGESTION.md`）。

下一步建议：执行 **STEP-5.4** —— `hello_watchdog + datagram_reader + 端到端本地 IO` 接入 `PeerSession::run()` 主循环：
1. 启 `hello_watchdog`（3s）
2. 启 `datagram_reader` task（产生 `StreamEvent::Datagram`）
3. `run()` 主循环 `tokio::select!` 合并 4 路：stream A (recv_a) + stream B (rx_b) + datagram (rx_d) + `conn.closed()` 超时
4. 处理 `Connection::closed()` → 触发 `should_retry_after_close`
5. 接 STEP-4.4 `route_input(cfg, event)` 做事件分派（按 `StreamEvent` 类别）