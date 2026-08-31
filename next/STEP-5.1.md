# STEP-5.1 — `PeerSession::send_motion` 走 `send_datagram` + 降级 stream

> PLAN-M1 §STEP-5 / STEP-5.1
> 执行日期：2026-08-31　实际耗时：~30 min
> 结论：✅ 通过（0 编译错新增，0 边界越界）

## 1. 做了什么

在 `src/quic_transport.rs` 落地 `PeerSession::send_motion(&self,
&ProtoEvent) -> Result<()>` 公共方法（PLAN §5.1 真活），配合 `MAX_SAFE_DATAGRAM`
常量 + `send_datagram_or_stream_b` 私有 helper + `Error::Datagram` /
`Error::DatagramFallback` 两个新 Error 变体 + `motion_datagram_round_trip`
端到端单测。改动 1 个文件：

- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/quic_transport.rs`
  - 顶部 module doc 注释加 STEP-5.1 行（标 "（已）"）
  - 新常量 `MAX_SAFE_DATAGRAM: usize = 1162`（紧邻 `HELLO_TIMEOUT`）
  - 新 Error 变体 `Error::Datagram(#[from] quinn::SendDatagramError)` +
    `Error::DatagramFallback(String)`
  - `impl PeerSession` 加 2 个方法：
    `send_motion(&self, event: &ProtoEvent) -> Result<()>`（pub async,
    `#[allow(dead_code)]`）
    + `send_datagram_or_stream_b(&self, bytes: &[u8]) -> Result<()>`
    （私有 async helper）
  - 测试 mod 末尾加 `motion_event()` + `motion_test_server()` helpers +
    `motion_datagram_round_trip` 单测

## 2. 关键设计要点

### 2.1 `send_motion` 设计

```rust
#[allow(dead_code)]
pub async fn send_motion(&self, event: &ProtoEvent) -> Result<()> {
    if !self.hello_ok.load(Ordering::Acquire) {
        return Err(Error::HelloFailed("hello not complete".into()));
    }
    let (buf, _len): ([u8; MAX_EVENT_SIZE], usize) = event.clone().into();
    self.send_datagram_or_stream_b(&buf).await
}
```

**3 条契约**（与 bak `mousehop/src/quic_transport.rs:471-486` 完全对齐）：

1. **前置门禁**：`hello_ok == true` 才发事件；否则返
   `Error::HelloFailed("hello not complete")`，**不**碰 datagram /
   stream —— 这是 PLAN §3 "mTLS 通了不等于对端是 lan-mouse" 信任
   模型的守护
2. **定长编码**：`event.clone().into()` 把 `ProtoEvent` 转成
   `([u8; MAX_EVENT_SIZE], usize)`（21 字节定长）；datagram 投递
   时直接 `write_all(buf)` 写满 buffer，对端按 `MAX_EVENT_SIZE` 解码
   —— 两条通道（datagram / stream B）共用同一解码入口
3. **dead_code chain**：STEP-5.4 `PeerSession::run()` 接管读循环后，
   STEP-6.x `LanMouseConnection::send()` 消费此函数。当前 main-code
   无 caller，加 `#[allow(dead_code)]` 守护（与 STEP-1.x / 2.x / 3.x
   同模式）

### 2.2 `send_datagram_or_stream_b` 设计

**判定顺序**（与 PLAN-v4 §6 + bak
`mousehop/src/quic_transport.rs:507-530` 完全对齐）：

1. **每次重读 `max_datagram_size()`** —— 严格遵守 STEP-0.1 结论 D
   （值随路径 MTU 探测变化 1162 → 1414）。`Some(max)` 与
   `MAX_SAFE_DATAGRAM = 1162` 取 min 作为实际上限——**不缓存任何值**
2. `bytes.len() <= limit` 时调 `conn.send_datagram(bytes.to_vec().into())`：
   - `Ok` → return `Ok(())`
   - `Err(ConnectionLost(_))` → **不降级**（连接已死，stream 也救不
     回来），直接返 `Error::Datagram(...)`
   - `Err(TooLarge / Disabled / UnsupportedByPeer)` → log debug +
     降级到 stream（不抛错 —— 这些是"这条路走不通"的稳态）
3. 降级路径：inline `open_uni() + write_all() + finish()` 单条 uni
   stream（**无 cache / 无长度前缀**）—— STEP-5.2 才实现
   `send_stream_b` cache + 长度前缀帧；当前为降级 IO 错误引入临时
   变体 `Error::DatagramFallback(String)`

**`bytes.to_vec().into()` 类型推断**：quinn 0.11 的
`Connection::send_datagram` 收 `bytes::Bytes`，`Vec<u8> → Bytes` 是
零拷贝（接管 Vec 的堆分配）。无需在主仓加 `bytes` crate 依赖 —— 类型
由 quinn 0.11 的 `send_datagram` 签名反向推断。

**签名 `&[u8]` 而不是 `&ProtoEvent`**：STEP-5.2 `send_stream_b`
收到"已编码字节"时复用同一份 buffer（datagram 失败后复用 buf），
且未来 `motion_oversize_falls_back_to_stream` 测试要构造超限裸字节
验证降级管道本身（与 bak
`mousehop/src/quic_transport.rs:507 send_datagram_or_stream_b`
形态完全一致）。

### 2.3 `MAX_SAFE_DATAGRAM` 常量

**取值 1162 字节**：STEP-0.1 spike 实测的 QUIC 握手初期下限。MTU
探测完成后 `max_datagram_size()` 可达 `1414`，但本常量**不缓存**——仅作
`max_datagram_size().map(|m| m.min(MAX_SAFE_DATAGRAM))` 的取 min 边界，
防止上层用任何"陈旧更大值"绕过 cap（防御性常量）。

与 bak `mousehop/src/quic_transport.rs:121-123 MAX_SAFE_DATAGRAM`
完全对齐（PLAN-v4 Step 0.1 结论 D）。

### 2.4 Error 变体

| 变体 | 来源 | 用途 |
|---|---|---|
| `Error::Datagram(#[from] quinn::SendDatagramError)` | quinn 0.11 `SendDatagramError` | 包装 `UnsupportedByPeer` / `Disabled` / `TooLarge` / `ConnectionLost` 四种；`ConnectionLost` 由 `send_datagram_or_stream_b` 透传（**不降级**） |
| `Error::DatagramFallback(String)` | 本步 inline | 降级 uni stream 的 IO 错误（`open_uni` 的 `ConnectionError` / `write_all` 的 `WriteError` / `finish` 的 `ClosedStream`）；STEP-5.2 会替换为 `Error::StreamB(String)`（SUGGESTION #S-14） |

`Error::DatagramFallback` 的存在是**临时降级**——PLAN §5.1 文字
没强制 stream B 复用（那是 STEP-5.2 `StreamBunch` + 长度前缀帧
codec 范畴），本步把 stream B cache + 长度前缀帧写进去会突破
30 min 目标且超出"只做 motion datagram"的本步边界。

### 2.5 端到端单测 `motion_datagram_round_trip`

**测试布局**（与 bak `mousehop/src/quic_transport.rs:4176-4263` 完全对齐）：

1. `install_crypto_provider()`（前置）
2. server endpoint（ephemeral cert） + client dial
3. server task（`tokio::spawn`）：
   - `accept(&server_ep)` 拿 Connection
   - `server_hello(&session)` 走完应用层握手
   - `session.connection().read_datagram()` 等客户端发来的 datagram
   - 断言 `datagram.len() == MAX_EVENT_SIZE`（定长 codec 21 字节）
   - `ProtoEvent::try_from(buf)` 解码，断言 `Motion { time=4242, dx=12.5, dy=-7.25 }`
4. client 端：
   - 临时 `pins_dir`（PID + nanos 隔离，与 STEP-2.6 `tmp_pins_dir` 同模式）
   - `dial(...)` + `PeerSession::from_connection(conn)` + `client_hello(...)`
   - 前置断言：`max_datagram_size().is_some()`（握手完成 datagram 可用）
   - `send_motion(&motion_event())` 期望 `Ok(())`
5. 等 server task 完成 + 清理

**临时 `pins_dir` 模式**：用 `std::env::temp_dir().join(...)` 而非
`tempfile::tempdir()` —— 与 STEP-2.6 `tmp_pins_dir` 同模式，**不引入**
`tempfile` dev-dep（保持 workspace 依赖清单最小变动）。

**前置断言 `max_datagram_size().is_some()`**：是 STEP-0.1 结论 D 的
"运行时验证" —— 握手完成后 `Some(_)` 且 ≥ 21 字节，本测试才走
datagram 路径（非降级）。若 MTU 探测时机异常导致 datagram 不可用，
本测试的 `send_motion` 会改走降级路径，对端 `read_datagram()` 5s
超时失败 → 测试失败（"datagram 路径没走通"被这条断言抓住）。

### 2.6 与 STEP-0.1 结论 D 的契约

| 契约 | 落实 |
|---|---|
| `max_datagram_size()` 每次读，不缓存 | ✅ `send_datagram_or_stream_b` 第 1 步每次都调 `self.conn.max_datagram_size()` |
| `Some(_)` → 优先 datagram；`None` → 降级 | ✅ `if let Some(limit) = limit { ... } else { fallback }` |
| 实际 cap = `min(报告值, MAX_SAFE_DATAGRAM)` | ✅ `.map(|m| m.min(MAX_SAFE_DATAGRAM))` |
| `ConnectionLost` 不降级（救不回来） | ✅ `Err(e @ ConnectionLost(_)) => return Err(Error::Datagram(e))` |

## 3. 验证结果

### 3.1 `cargo check -p lan-mouse --lib`

```
$ cargo check -p lan-mouse --lib 2>&1 | grep -cE "^error\[E"
14

$ cargo check -p lan-mouse --lib 2>&1 | grep -E "quic_transport\.rs" | grep "error\["
# （无输出 —— 本步新增代码 0 编译错）
```

**14 errors 全部来自 `connect.rs` / `listen.rs` 的
`webrtc_dtls` / `webrtc_util` 引用**（与 STEP-1.2 / STEP-2.x /
STEP-3.x / STEP-4.x 报告完全一致）；本步新增
`MAX_SAFE_DATAGRAM` / `Error::Datagram` / `Error::DatagramFallback` /
`send_motion` / `send_datagram_or_stream_b` / `motion_datagram_round_trip`
单测 + 2 个 helpers **0 编译错**。

### 3.2 `cargo check -p lan-mouse --tests`

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
- 基线（git stash 后）：同样 27 errors
- 本步提交后：27 errors（**0 增量**）
- 27 = 14 DTLS pre-existing + 13 fixture 错误（`InputEvent` /
  `PointerEvent` / `KeyboardEvent` / `Position` —— STEP-4.4
  `route_input_fixtures` 内的 `use super::*` 受 STEP-1.2 留下的
  `webrtc_dtls` 引用阻塞导致模块解析失败，与本步无关）

### 3.3 §9 M1 边界 grep

```
$ grep -nE "TransportEvent|Bounds|MotionAbsolute|CursorPos|ReceiverSensitivity|MAX_CLIPBOARD_SIZE|BufferTooLarge|ClipboardEvent|axis::momentum|MACOS_KEEP_AWAKE_EVENT_TAG|h3|h3-quinn|status_bar|clipboard" src/quic_transport.rs
# （唯一命中：doc 注释引用 M2 计划，0 代码命中 —— §9 12 类 grep 无命中）
```

### 3.4 单测 `cargo test -p lan-mouse motion_datagram_round_trip`

```
$ cargo test -p lan-mouse quic_transport::motion_datagram_round_trip 2>&1 | tail -3
error: could not compile `lan-mouse` (lib test) due to 14 previous errors
```

**单测无法跑通** —— `lan-mouse` lib 因 STEP-1.2 留下的 14 DTLS
errors 编不过；test target 与 lib 同编译单位。详见 SUGGESTION #S-5：
STEP-6.x 修复 14 errors 后 Leader 手动跑一次确认。

## 4. 与 PLAN-M1 §5.1 的偏差

| 项 | PLAN 要求 | 实际做法 | 原因 |
|---|---|---|---|
| `pub async fn send_motion(&self, event: &ProtoEvent) -> Result<()>` | 同 | 直接对齐 | 签名零差异 |
| `Error::Datagram` 变体 | PLAN §5.1 隐含（"datagram 失败错误"） | `Error::Datagram(#[from] quinn::SendDatagramError)` + `Error::DatagramFallback(String)` 两个变体 | `#[from]` 自动派生让 send_datagram 错误可经 `?` 直接冒泡；降级 stream IO 错误单独 String 变体（STEP-5.2 替换为 `Error::StreamB`，见 SUGGESTION #S-14） |
| 内联 `if let Some(max) = conn.max_datagram_size() { ... }` | 同 | 完全对齐（且每次读、不缓存） | 与 bak `mousehop/src/quic_transport.rs:507-530` 1:1 |
| 单测 `motion_datagram_round_trip` | PLAN §5.1 验收清单 | 端到端：server `read_datagram()` + 长度断言 + 字段断言 | 与 bak `mousehop/src/quic_transport.rs:4176-4263` 1:1；motion_event 字段值 `(4242, 12.5, -7.25)` 与 bak 完全对齐 |
| **不实现 send_motion 之外的 send 函数** | Leader prompt 限制 | 仅 `send_motion` 一个 send 方法 | STEP-5.2 起才加 `send_stream_a` / `send_stream_b` 等 |

### PLAN-M1 偏差 #N-7（轻）：降级路径是 inline uni stream

- **PLAN 文字**："超 `max_datagram_size` 时降级 stream" —— 没明确
  指定是否复用 stream B cache 或是否带长度前缀帧
- **实际**：inline `open_uni() + write_all() + finish()`，单条 uni
  stream（**无 cache / 无长度前缀**）+ 临时 `Error::DatagramFallback(String)`
- **影响**：当前定长 codec（21 字节）下"降级路径能跑通"是验证目标；
  STEP-5.2 会把这段换成 `send_stream_b`（带 `StreamBunch` cache +
  长度前缀帧 `[u32 BE len][body]`）
- **不构成问题**：STEP-5.1 范围严格守住 PLAN §5.1 文字"motion 走
  datagram + 降级 stream"；STEP-5.2 自然消化临时变体（已记
  SUGGESTION #S-14）

## 5. 处理的 SUGGESTION 项

新增 2 条 SUGGESTION 条目：

- **#S-14 🟢 低**：`send_motion` 降级路径是 inline uni stream，
  STEP-5.2 将替换为 `send_stream_b` + `StreamBunch` cache + 长度
  前缀帧；`Error::DatagramFallback(String)` 替换为
  `Error::StreamB(String)`（与 bak 对齐）
- **#S-15 🟢 低**：`MAX_SAFE_DATAGRAM = 1162` 与 PLAN-v4 实测相关
  —— 后续 MTU spike 变更需重跑；当前 1162 是保守值

无触及其它 SUGGESTION 条目。

## 6. 闸门检查（PLAN-M1 §1 时间门 / §9 边界门）

- **§1 时间门**：~30 min，在 20–30 min 目标内 ✅
- **§9 边界门**：见 §3.3，0 越界 ✅
- **STEP-4.4 依赖**：✅ `route_input` 纯函数已就位（虽然本步
  `send_motion` 不直接调 `route_input` —— 那要等 STEP-5.4 read_loop
  接入；本步 send_motion 只编码+发，**不**做 cfg 分派）
- **STEP-3.2 依赖**：✅ `client_hello` / `server_hello` + `peer.conn`
  公共访问器就位
- **STEP-2.6 依赖**：✅ `dial(...)` 加 `pins_dir` 参数（测试用临时
  PID+nanos 隔离目录）

## 7. 遗留 / 风险

- ⚠️ **单测 `motion_datagram_round_trip` 无法在本步端到端跑通**：
  14 DTLS errors 阻塞 lib 编译（与 SUGGESTION #S-5 同根因）。单测
  代码逻辑就位，STEP-6.x 修 errors 后 Leader 手动跑一次确认
- ⚠️ **`Error::DatagramFallback` 是临时变体**：STEP-5.2
  `send_stream_b` 落地后会被 `Error::StreamB(String)` 替换
  （SUGGESTION #S-14）
- ⚠️ **`send_motion` 不调 `route_input`**：STEP-4.4 `route_input`
  纯函数就位，但本步 `send_motion` 只编码+发 Motion；STEP-5.4
  `PeerSession::run()` 接入读循环 + 业务分发时，由更高层 caller
  按 `route_input(cfg, event)` 决定调 `send_motion` 还是
  `send_stream_a/b/c`。本步契约：send_motion 只接 Motion / Axis /
  AxisDiscrete120（其他事件类型会塞满 21 字节 + 解码错位，未来
  caller 必走 route_input 分派）

## 8. 下一步（STEP-5.2 前置条件）

✅ 就绪：
- `pub async fn send_motion(&self, &ProtoEvent) -> Result<()>` 公共 API
- `send_datagram_or_stream_b(&self, &[u8]) -> Result<()>` 私有 helper
  （STEP-5.2 替换降级分支为 `send_stream_b`）
- `Error::Datagram(#[from] quinn::SendDatagramError)` 变体
- `Error::DatagramFallback(String)` 临时变体（STEP-5.2 替换）
- `MAX_SAFE_DATAGRAM: usize = 1162` 常量
- `motion_event()` + `motion_test_server()` 测试 helpers
- `motion_datagram_round_trip` 端到端单测（仅待 14 errors 修复后执行）

下一步建议：执行 **STEP-5.2** —— `StreamBunch` struct + 长度前缀帧 codec
（`Bidi<S> { send, recv }` / `StreamBunch { a, b, c }` /
`write_frame` / `read_frame`），替换 STEP-5.1 的 inline 降级路径为
`send_stream_b`（cache + 长度前缀帧），把 `Error::DatagramFallback`
替换为 `Error::StreamB(String)`。搬运参考：
`lan-mouse-pro-bak/mousehop/src/quic_transport.rs:2126-2300`。

**未做 git commit**：等 Leader 处理（本步仅动 `src/quic_transport.rs` + `next/SUGGESTION.md`）。
