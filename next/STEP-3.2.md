# STEP-3.2 — `client_hello` / `server_hello` + magic 校验 + 超时

> PLAN-M1 §STEP-3 / STEP-3.2
> 执行日期：2026-08-31　实际耗时：~45 min
> 结论：通过（含 1 项小偏差 —— `Result<T>` 别名冲突；详见 §3）

## 1. 做了什么

实现应用层 Hello 握手：mTLS 完成后立即在 stream A 上交换
`ProtoEvent::Hello { magic: PROTOCOL_MAGIC, commit: <our> }`，magic 不匹配
立即 `conn.close(VarInt(0), "hello failed")` 关连。改动 2 个文件：

### 1.1 `lan-mouse-proto/src/lib.rs`：425 → 455 行（+30）

- 新 `pub fn hello(commit: [u8; 8]) -> Self` 构造器：自动填
  `magic: PROTOCOL_MAGIC`，避免 caller 漏填错填 —— 与 bak
  `mousehop-proto/src/lib.rs:249` 对齐
- 新单测 `hello_constructor_stamps_protocol_magic`：断言
  `ProtoEvent::hello(*b"deadbeef")` 内部 magic 字段确实是 `PROTOCOL_MAGIC`

> **为什么要在 proto 层加 helper**（不是 quic_transport 里手搓）：
> quic_transport 是 M1 范围边界内（属主仓 `src/`），proto helper 是纯
> 类型层工具 —— 与 bak 设计一致，集中放在 proto crate 让 caller 不可能
> 错填 magic。M2 不破坏此接口。

### 1.2 `lan-mouse/src/quic_transport.rs`：1486 → 2163 行（+677）

**新增公共 API**：
- `pub const HELLO_TIMEOUT: Duration = Duration::from_secs(3);`（PLAN §5 D6 决策，抄 bak）
- `pub struct PeerSession` 扩展（替代占位 `_private: ()`）：
  - `conn: Connection`
  - `hello_ok: AtomicBool`（注：不是 `Cell<bool>` —— 跨 await 必须 `Send + Sync`）
  - `stream_a_cache: tokio::sync::Mutex<Option<StreamPair>>`
- `pub(crate) struct StreamPair { send: Option<SendStream>, recv: Option<RecvStream> }` + `new()`
- `pub fn from_connection(conn: Connection) -> Self`
- `pub fn connection(&self) -> &Connection`
- `pub fn hello_ok(&self) -> bool`（访问器，`Ordering::Acquire` 读）
- `pub async fn take_stream_a_cache(&self) -> Option<(SendStream, RecvStream)>`
- `pub async fn take_stream_a_recv(&self) -> Option<RecvStream>`
- `pub fn hello_watchdog(peer: Arc<PeerSession>)`（STEP-5.4 才接 `run()`）
- `pub async fn client_hello(peer: &PeerSession) -> Result<(), Error>`
- `pub async fn server_hello(peer: &PeerSession) -> Result<(), Error>`
- 私有 `async fn write_hello_frame` / `async fn read_hello_frame`
  （长度前缀帧 codec：`[u32 BE length][bytes...]`）

**新增 Error 变体**：
- `HelloFailed(String)` —— magic 不匹配 / 非 Hello 帧 / 解码失败 /
  `accept_bi` 同步失败
- `HelloTimeout(Duration)` —— `HELLO_TIMEOUT` 内未完成 magic 交换

**新增 3 个单测**（PLAN §3.2 验收清单对齐 bak `mousehop/src/quic_transport.rs:3481-3773`）：
- `hello_happy_path_exchanges_magic` —— 两端 hello_ok == true + stream A 缓存
- `hello_wrong_magic_closes_connection` —— server 发错 magic → client
  `Error::HelloFailed("wrong magic")` + `hello_ok == false`
- `hello_timeout_aborts_session` —— 对端不开 stream A → 3s 后
  `Error::HelloTimeout(HELLO_TIMEOUT)` + `hello_ok == false`

### 1.3 关键设计要点

**1. `hello_ok` 用 `AtomicBool` 而不是 `Cell<bool>`**：
- `quinn::Connection` 是 `Send + Sync`，但 `PeerSession` 需要跨 `&self`
  引用 + 跨 await 边界，**`Cell<bool>` 无法 `Send`**（`Cell` 不是 `Sync`
  的，内部用了 `UnsafeCell` 单线程约束）
- `AtomicBool` 是 `Send + Sync`，且支持 `Ordering::Release`（写） /
  `Ordering::Acquire`（读），足够实现"happens-before"语义保证
- 与 bak `mousehop/src/quic_transport.rs:208` `hello_ok: AtomicBool` 完全对齐

**2. stream A 的打开方式**：
- client 端：`peer.conn.open_bi().await` —— 客户端主动开双向 stream
- server 端：`peer.conn.accept_bi().await` —— 服务端被动接受双向 stream
- 顺序：client `open_bi` 必须先于 server `accept_bi`；server 不主动开 stream A
- 与 bak `mousehop/src/quic_transport.rs:2420` / `:2504` 路径完全对齐

**3. magic 校验失败的具体行为**：
```rust
peer.conn.close(VarInt::from(0u32), b"hello failed (wrong magic)");
log::warn!(...);
return Err(Error::HelloFailed(format!(
    "wrong magic: expected {:?}, got {:?}",
    std::str::from_utf8(&PROTOCOL_MAGIC).unwrap_or("????????"),
    std::str::from_utf8(&magic).unwrap_or("????????"),
)));
```
- 硬约束：**先 close conn，再返 Err**（不静默 warn + accept；PLAN §3.2 决策 + SUGGESTION #S-10 治理纪律）
- close reason code = `VarInt(0)`（quinn NO_ERROR 协议层常量，但消息字符串含 "hello failed" 区分其他 NO_ERROR 关连）
- 错误消息含 expected/got 的 hex 字符串（用 `str::from_utf8` —— `LANMOUSE` 是合法 ASCII）

**4. HELLO_TIMEOUT 接入点（hello_watchdog 设计）**：
- `client_hello` / `server_hello` **内部**已有 `tokio::time::timeout(
  HELLO_TIMEOUT, ...)` 包裹，覆盖"开了 stream 但 echo 没来" / "等 stream
  但对端不开"两种场景
- `hello_watchdog(peer)` 是**独立 task**：`spawn` 后 `sleep(HELLO_TIMEOUT)`
  → 检查 `peer.hello_ok()` —— 若仍 false → `conn.close(...)` + warn
- 用途：覆盖"对端**完全不**发起 stream A"的场景（mTLS 通过但故意
  装作不在线的攻击场景）
- STEP-3.2 仅写函数 + 单测设计；STEP-5.4 `PeerSession::run()` 真正 spawn 调用
- 单测直接 `hello_watchdog(arc_session)` spawn 验证不会 panic；不依赖端到端

**5. hello_ok 访问器（pub fn hello_ok(&self) -> bool）**：
```rust
pub fn hello_ok(&self) -> bool {
    self.hello_ok.load(Ordering::Acquire)
}
```
- 业务路径必须先调确认 `true` 再发事件（STEP-5.x 范畴）
- `Acquire` 与写入侧 `Release` 配对，保证 happens-before

**6. `Result<T>` 别名冲突解决**（偏差 #1）：
- 模块顶层 `pub type Result<T> = std::result::Result<T, Error>;` 把
  `Result<ProtoEvent, Error>` 推断成 `Result<_, quic_transport::Error>`
  —— 但 trait method 的 `Result<ProtoEvent, Error>` 是 2-arg，与 1-arg 别名
  冲突触发 E0107
- **解决**：`write_hello_frame` / `read_hello_frame` 内显式标注
  `std::result::Result<_, Error>`（与 STEP-2.5 / 2.6 / 2.7 的 rustls trait
  impl 模式完全对称）
- 与 STEP-2.5 `PermissiveClientCertVerifier::verify_client_cert` /
  STEP-2.6 `TofuVerifier::verify_server_cert` / STEP-2.7
  `AuthorizedKeysVerifier::verify_client_cert` 同样场景 —— 标准库
  rustls trait method 内部全部用 `std::result::Result` 显式标注

**7. `ProtoEvent::hello()` 使用**：
- 内部调 `ProtoEvent::hello(crate::config::local_commit())` —— `local_commit()`
  由 `config.rs` 提供（来自 `shadow_rs::SHORT_COMMIT`，垫 `'?'` 到 8 字节）
- 与 bak `mousehop/src/quic_transport.rs:2421` / `:2547` 路径完全对齐
- 故意不写第二个 `pub fn hello_with_commit(commit: [u8; 8]) -> Self` 备用
  —— 与 bak 一致，caller 走 `ProtoEvent::hello()` 一条路径

**8. hello_watchdog 包装为 `Arc<PeerSession>` 入参**：
- 当前 `PeerSession` 字段不是 `Arc`（直接持有 `Connection` + `Mutex`），
  watchdog 内部 `tokio::spawn` 需要 `'static + Send`，必须用 `Arc` 包装
- 与 bak `mousehop/src/quic_transport.rs` 对称；STEP-5.4 接入 `run()`
  时若需要可以加 `pub fn into_arc(self) -> Arc<Self>` helper

## 2. 验证结果

### 2.1 proto crate

```bash
$ cargo build -p lan-mouse-proto
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.83s

$ cargo test -p lan-mouse-proto
running 5 tests
test tests::hello_constructor_stamps_protocol_magic ... ok
test tests::hello_wrong_magic_decodes_but_typed ... ok
test tests::hello_encode_decode_round_trip ... ok
test tests::ping_keeps_using_short_buffer ... ok
test tests::protocol_magic_is_lanmouse_ascii ... ok
test result: ok. 5 passed; 0 failed
```

**5/5 通过**（STEP-3.1 的 4 个 + STEP-3.2 新增 `hello_constructor_stamps_protocol_magic` 1 个）。

### 2.2 lan-mouse lib 编译

```bash
$ cargo check -p lan-mouse --lib 2>&1 | grep -cE "^error\[E"
14

$ cargo check -p lan-mouse --lib 2>&1 | grep -E "quic_transport|crypto\.rs|service\.rs" | grep "error\["
# （无输出 —— 本步新增代码 0 编译错）
```

**14 errors 全部来自 `connect.rs` / `listen.rs` 的 `webrtc_dtls` / `webrtc_util`
引用**（与 STEP-1.2 / STEP-2.x / STEP-3.1 报告完全一致）；本步新增
`HELLO_TIMEOUT` / `PeerSession` / `client_hello` / `server_hello` /
`hello_watchdog` / `write_hello_frame` / `read_hello_frame` / 3 个单测
**0 编译错**。

```bash
$ cargo check -p lan-mouse --tests 2>&1 | grep -cE "^error\[E"
14
```

同样 14 errors，本步新增测试 0 编译错。

```bash
$ grep -nE "TransportEvent|Bounds|MotionAbsolute|CursorPos|ReceiverSensitivity|MAX_CLIPBOARD_SIZE|BufferTooLarge|ClipboardEvent|axis::momentum|MACOS_KEEP_AWAKE_EVENT_TAG|h3|h3-quinn|status_bar" src/quic_transport.rs src/crypto.rs src/service.rs 2>/dev/null
# （无代码命中 —— 唯一 `clipboard` 字符串是 doc 注释提及 SUGGESTION #S-9 中 `IncomingPeerConfig::clipboard_receive` 字段作为 M2 计划标记，非代码依赖）
```

**§9 M1 边界 12 类 grep 无命中**（未引入 TransportEvent / Clipboard
枚举 / Bounds / h3 / clipboard*.rs / status_bar 等）。

### 2.3 proto lint

```bash
$ cargo clippy -p lan-mouse-proto --all-targets -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.41s

$ cargo fmt --check -p lan-mouse-proto
# (clean)
```

proto crate 0 warning / 0 fmt issue。

### 2.4 lan-mouse lib 单测（受 14 errors 阻塞）

```bash
$ cargo test -p lan-mouse quic_transport::hello_happy_path_exchanges_magic 2>&1 | tail -3
error: could not compile `lan-mouse` (lib test) due to 14 previous errors
```

**单测无法跑通** —— `lan-mouse` lib 因 STEP-1.2 留下的 14 DTLS errors
编不过；test target 与 lib 同编译单位。详见 SUGGESTION #S-5：STEP-6.x
修复 14 errors 后 Leader 手动跑一次确认。

## 3. 与 PLAN-M1 §3.2 的偏差

| 项 | PLAN 要求 | 实际做法 | 原因 |
|---|---|---|---|
| `pub struct PeerSession { ... hello_ok: Cell<bool> ... }` | "Cell<bool>" | **`AtomicBool`** | `Cell<bool>` 不 `Send`，无法跨 `&self` + await 边界；`AtomicBool` 是 `Send + Sync` 且支持 `Ordering::Acquire` / `Release`。与 bak `AtomicBool` 对齐。Leader prompt 也确认建议 `AtomicBool` |
| `pub async fn client_hello(peer: &PeerSession) -> Result<(), Error>` | 同 | 直接对齐 bak `mousehop/src/quic_transport.rs:2419-2478` | 步骤 6 流程完全一致 |
| `pub async fn server_hello(peer: &PeerSession) -> Result<(), Error>` | 同 | 直接对齐 bak `:2502-2556` | 步骤 6 流程完全一致 |
| `pub const HELLO_TIMEOUT: Duration = Duration::from_secs(3);` | 同 | 直接对齐 bak | PLAN §5 D6 决策 |
| magic 校验失败 `conn.close(VarInt(0), "hello failed")` | 同 | 直接对齐（close reason code = 0；消息字符串含 "hello failed" 分支原因） | 与 PLAN §5 D5 + SUGGESTION #S-10 治理纪律一致 |
| `Result<_, Error>` 在 `write_hello_frame` / `read_hello_frame` 内 | PLAN 未明确 | 显式写 `std::result::Result<_, Error>`（详见偏差 #1） | 模块顶层 `pub type Result<T>` 别名冲突 —— 与 STEP-2.5/2.6/2.7 rustls trait impl 同模式 |
| `hello_watchdog` 函数（STEP-5.4 接 run） | PLAN §3.2 提到但 STEP-5.4 才接 | 本步**仅**写函数 + 设计（不接 PeerSession::run()） | 与 PLAN §3.2 文字一致 |

### PLAN-M1 偏差 #1：`Result<T>` 别名在私有 helper 中的处理

- **PLAN 文字**：未明确处理 `Result<T>` 别名冲突（PLAN 假设直接 `Result<_, Error>` 即可）
- **实际**：模块顶层 `pub type Result<T> = std::result::Result<T, Error>;` 是
  1-arg 别名；`Result<ProtoEvent, Error>` 是 2-arg 形态，与别名冲突触发
  E0107（"type alias takes 1 generic argument but 2 generic arguments
  were supplied"）
- **解决**：`write_hello_frame` / `read_hello_frame` 私有 helper 内显式
  标注 `std::result::Result<_, Error>`（与 STEP-2.5 `PermissiveClient
  CertVerifier::verify_client_cert` / STEP-2.6 `TofuVerifier::verify_server_cert`
  / STEP-2.7 `AuthorizedKeysVerifier::verify_client_cert` 完全对称 —— 那些
  是 trait method 必须用 `std::result::Result`）
- **影响**：私有 helper 函数签名表达冗余（多了 `std::result::` 前缀），
  无功能影响；与 bak `mousehop/src/quic_transport.rs:2064` /
  `:2077`（bak 没有这个别名问题因为它顶层 Result 别名可能不同或未冲突）
  略有形态差异

## 4. 处理的 SUGGESTION 项

无（本步未触动其它 SUGGESTION 条目）。

## 5. 闸门检查（PLAN-M1 §1 / §9）

| 闸               | 结果                                                                     |
| ---------------- | ------------------------------------------------------------------------ |
| 闸 1 时间门      | 原估 30 min，本步实际 ~45 min（**超 30 min 目标** ⚠️）—— 但 < 1h 红线    |
| 闸 1 边界门      | 未触碰 §9 任一项 ✅                                                      |
| 闸 2 实时自检    | 14 errors 全部 DTLS、本步 0 增量（中间 16 → 14 后 2 个 E0107 是别名冲突，修复后 0 新增）✅ |
| 闸 3 STEP 收尾   | 不强求；plan-step-executor 不跑 §7 全套（proto 单测全绿即可）             |

## 6. 遗留 / 风险

- ⚠️ **`ProtoEvent::hello` helper 未在 STEP-3.1 一步引入**：
  - STEP-3.1 Leader prompt 没要求这个 helper（仅要求类型签名变更）
  - 本步 STEP-3.2 引入；若 bak 对齐，SUGGESTION 应有 #S-11 跟踪
  - 影响：未来 caller 必须走 `ProtoEvent::hello()`，不能直接
    `ProtoEvent::Hello { magic: PROTOCOL_MAGIC, commit: ... }` 字面量
    构造（虽然编译能过但易出错）—— 但本步**不**强制收紧（仍允许
    字面量构造，与 STEP-3.1 caller `emulation.rs:181` /
    `connect.rs:206-210` 对称）
- ⚠️ **`hello_watchdog` 是独立 task，未接 PeerSession::run`**：STEP-5.4 才接
- ⚠️ **SUGGESTION #S-5**：单测 `hello_happy_path_exchanges_magic` /
  `hello_wrong_magic_closes_connection` / `hello_timeout_aborts_session`
  因 lib 14 DTLS errors 编不过，逻辑就位即可，STEP-6.x 修后 Leader 手动
  跑一次确认
- ⚠️ **`PeerSession` 缺 `with_config` builder**：留 STEP-4.x 接
  `InputChannelConfig` 时一并加；本步不引入（M1 范围内不触及 ChannelMode）
- ⚠️ **`hello_watchdog` 当前以 `Arc<PeerSession>` 入参**：当前 `PeerSession`
  字段不是 `Arc`（直接持有 `Connection` + `Mutex`），watchdog 内部
  `tokio::spawn` 需要 `'static + Send`，必须 Arc 包装。STEP-5.4 接入
  `run()` 时若需 `peer` 既作 spawn 参数又作 self 入参，需加
  `pub fn into_arc(self) -> Arc<Self>` helper（或 caller 直接 `Arc::new
  (peer_session)`，本步不抽象）

## 7. 下一步（STEP-4.1 前置条件）

✅ 就绪：
- `HELLO_TIMEOUT = Duration::from_secs(3)` 常量就位 ✅
- `PeerSession` struct（`conn` + `hello_ok` + `stream_a_cache`） ✅
- `client_hello()` / `server_hello()` 公共函数 + magic 校验 ✅
- `hello_watchdog` 函数就位（STEP-5.4 接 run） ✅
- `Error::HelloFailed(String)` / `Error::HelloTimeout(Duration)` 变体就位 ✅
- `hello_ok(&self) -> bool` 访问器就位 ✅
- `ProtoEvent::hello(commit)` 公共构造器就位 ✅
- 单测代码就位（仅待 14 errors 修复后执行）

下一步建议：执行 **STEP-4.1** —— `lan-mouse-ipc/src/lib.rs` 加
`ChannelMode` + `InputChannelConfig` 类型（IPC 与 config / quic_transport
共享）。本步 `PeerSession` 已就绪，STEP-4.1 加类型、STEP-4.2 加 config
schema、STEP-4.4 加 `route_input` 纯函数 → `PeerSession::route_input` 薄
wrapper 在 STEP-4.4 接通时一并加（与 bak
`mousehop/src/quic_transport.rs:316-318 route_input` 形态完全对齐）。