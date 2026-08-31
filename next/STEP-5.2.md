# STEP-5.2 — `StreamBunch` struct + 长度前缀帧 codec

> PLAN-M1 §STEP-5 / STEP-5.2
> 执行日期：2026-08-31　实际耗时：~40 min（**轻微超 30 min 目标**，4 子步串行写完，未拆 a/b/c）
> 结论：✅ 通过（含 1 项偏差见 §3——与 PLAN §5.2 描述的 "cache 命中复用" 略有形态差异）

## 1. 做了什么

在 `src/quic_transport.rs` 落地 `Bidi<S>` + `StreamBunch { a, b, c }` 结构 +
通用长度前缀帧 codec（`write_frame` / `read_frame`，generic `AsyncWrite` /
`AsyncRead`）+ 3 个 Error 变体（`StreamB(String)` 替换 `DatagramFallback`、
`FrameTooLarge(usize)`、`Truncated`）+ `send_motion` 降级路径替换为
`send_stream_b`（cache + 长度前缀帧，SUGGESTION #S-14 治理落地）+ 2 个 codec
单测（`frame_round_trip` + `frame_truncated_rejected`）。

改动 1 个文件：

- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/quic_transport.rs`
  - 顶部 module doc 注释把 STEP-5.2 标 "（已）" + 加本步摘要
  - 新增 `pub struct Bidi<S> { send: S, recv: S }`（generic `S: AsyncRead +
    AsyncWrite + Unpin`）+ `impl Bidi { pub fn new(send: S, recv: S) -> Self }`
  - 新增 `pub struct StreamBunch { pub a/b/c: Bidi<SendStream> }`（三个字段
    公有，方便 STEP-5.3 read_loop 接管）
  - 新增 `pub struct PeerSession` 字段
    `stream_bunch: Arc<tokio::sync::Mutex<Option<StreamBunch>>>`（默认
    `None`，STEP-5.3 装配）
  - 新增 3 个 Error 变体：`Error::StreamB(String)` / `Error::FrameTooLarge(usize)`
    / `Error::Truncated`
  - 新增 `pub async fn write_frame<W>(send: &mut W, event: &ProtoEvent)
    -> std::result::Result<(), Error>` + `pub async fn read_frame<R>(recv:
    &mut R) -> std::result::Result<ProtoEvent, Error>`（generic
    `AsyncWrite`/`AsyncRead` + `Unpin`）
  - 新增 `impl PeerSession::send_stream_b(&self, bytes: &[u8]) -> Result<()>` 私有 helper
  - 改造 `impl PeerSession::send_datagram_or_stream_b`：降级分支替换为
    `self.send_stream_b(bytes).await?`
  - 测试 mod 末尾加 `frame_round_trip` + `frame_truncated_rejected` 单测

## 2. 关键设计要点

### 2.1 `Bidi<S>` 设计

```rust
#[allow(dead_code)]
pub struct Bidi<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    pub send: S,
    pub recv: S,
}
```

**Generic `S` 而非固定 `SendStream`**：生产路径 `S = SendStream`（quinn 0.11
双向 stream 的写半边），单测可以传 `tokio::io::DuplexStream`（满足
`AsyncRead + AsyncWrite + Unpin`）跑 codec 路径 —— 本步 `frame_round_trip`
/ `frame_truncated_rejected` 即用此模式。生产路径不受影响：`SendStream`
本身已实现 `AsyncRead` + `AsyncWrite` + `Unpin` 三 trait。

**为什么不是 `StreamPair` 升级版（保留 `Option<SendStream>` 包装）**：
主仓 `StreamPair`（STEP-3.2 引入）的"`Option<>`包装"语义是"recv 半边可
独立 take"，由 `PeerSession.stream_a_cache: Mutex<Option<StreamPair>>` 持有。
`Bidi<S>` 不做"半边 take"语义，**接收半边所有权管理交给上层
`StreamBunch` + `PeerSession.stream_bunch`** —— 上层可以用更灵活的
方式管理（take 一对 / take 整 bunch / 任意组合）。

### 2.2 `StreamBunch` 设计

```rust
#[allow(dead_code)]
pub struct StreamBunch {
    pub a: Bidi<SendStream>,
    pub b: Bidi<SendStream>,
    pub c: Bidi<SendStream>,
}
```

**3 个字段对应 3 条 bidi stream**（PLAN §3 "A/B/C 各开 1 条长期复用"）：
- `a` —— Stream A（control，Enter/Leave/Ack/Hello/Ping/Pong）
- `b` —— Stream B（input，鼠标按键/键盘按键/键盘 Modifier，按 STEP-4.4
  `route_input` 分派）
- `c` —— Stream C（clipboard meta，M2 预留；本步**不开** reader task，
  守 PLAN §9 M1 边界）

**字段公有**：方便 STEP-5.3 read_loop 接管时按 `streams.b.send /
streams.b.recv` 直接拿。`#[allow(dead_code)]` 守护（与 STEP-3.2 `StreamPair`
同模式）。

### 2.3 `write_frame` / `read_frame` codec

**帧格式**：`[u32 BE length][bytes...]`（4 字节 BE 长度前缀 + body）

**为什么 generic `W` / `R` 而非固定 `SendStream` / `RecvStream`**：
- 生产路径 `W = SendStream` / `R = RecvStream`（quinn 0.11 stream 半边）
- 单测路径 `W = R = tokio::io::DuplexStream`（满足相同 trait bound）

**两个函数均用 `std::result::Result<_, Error>` 显式标注**：模块顶层
`pub type Result<T> = std::result::Result<T, Error>;` 是 1-arg 别名；
`Result<ProtoEvent, Error>` 是 2-arg 形态触发 E0107。**与 STEP-3.2 偏差 #1
同模式解决**（详见 STEP-3.2.md §1.3.6 偏差 #1）。

**`read_frame` 错误归一**（与 STEP-3.2 `read_hello_frame` "全归 HelloFailed"
区分）：
| 错误场景 | Error 变体 | 含义 |
|---|---|---|
| 长度字段读 IO 失败 | `HelloFailed("read frame length: ...")` | 流已断 |
| `len > MAX_EVENT_SIZE` | **`FrameTooLarge(len)`** | DoS 防护，fatal |
| `read_exact` 收到 `UnexpectedEof` | **`Truncated`** | 对端半途关流，fatal |
| `read_exact` 其他 IO 错 | `HelloFailed("read frame body: ...")` | 流已断 |
| `ProtoEvent::try_from` 失败 | `HelloFailed("decode frame: ...")` | codec 解码失败（**可** skip-frame 续读） |

**为什么 `UnexpectedEof` 单独分流为 `Truncated`**：M1 范围 STEP-5.3 read_loop
需要区分 "fatal 立即关 conn"（`FrameTooLarge` / `Truncated`）vs "单帧损坏
可 skip-frame 续读"（`HelloFailed("decode frame: ...")`）。`Truncated` 与
`FrameTooLarge` 都是 fatal 信号 —— `Truncated` 表示对端可能在帧中半途关流
（peer 崩溃 / 攻击），reader task 应整体退出。

### 2.4 `send_stream_b` 实现（SUGGESTION #S-14 治理落地）

**签名**：`async fn send_stream_b(&self, bytes: &[u8]) -> Result<()>`（私有，
与 `send_datagram_or_stream_b` 对称）

**实现步骤**：
1. `self.conn.open_bi().await` 拿新一条 bidi stream（**每条都新建**——
   本步不缓存，详见 §3 偏差）
2. `drop(pair.1)` 释放 recv 半边（本步接收端 STEP-5.3 才接管 stream B reader，
   本步测试不需要 reader）
3. `send.write_u32(bytes.len() as u32).await` 写 4 字节长度前缀
4. `send.write_all(bytes).await` 写 body
5. `send.finish().await` 优雅关闭（quinn `finish` ≠ `close`，只发 FIN 让对端
   `read_to_end` 立即返回 EOF）
6. 错误归一：`open_bi` 失败 → `Error::StreamB("open_bi: ...")`，写失败 →
   `Error::StreamB("write frame length/body/finish: ...")`，与 bak
   `mousehop/src/quic_transport.rs:1035-1040` 完全对齐

**关键改造点**：`send_datagram_or_stream_b` 的降级分支由 inline
`open_uni() + write_all() + finish()` + `Error::DatagramFallback(String)` 替
换为 `self.send_stream_b(bytes).await?` —— `Error::DatagramFallback` 不再
被调用（保留变体为兼容测试 / 历史栈但已无 caller），SUGGESTION #S-14 完全
消化。

### 2.5 `Error::StreamB` / `Error::FrameTooLarge` / `Error::Truncated`

- **`Error::StreamB(String)`** —— 替换 STEP-5.1 `Error::DatagramFallback(String)`。
  消息前缀区分阶段（`"open_bi: ..."` / `"write frame length: ..."` /
  `"write frame body: ..."` / `"finish: ..."`），与 bak `Error::StreamB`
  完全对齐
- **`Error::FrameTooLarge(usize)`** —— 透传 `read_frame` 读到的超限长度；
  与 bak 变体形态一致（PLAN §5.2 验收清单要求）
- **`Error::Truncated`** —— `read_exact` 收到 `UnexpectedEof` 时返回；与
  `FrameTooLarge` 同为 fatal 信号，但语义聚焦"对端半途关流"

### 2.6 PeerSession 新增字段 `stream_bunch`

```rust
stream_bunch: Arc<tokio::sync::Mutex<Option<StreamBunch>>>,
```

**为什么 `Arc<Mutex<Option<_>>>` 而不是裸 `Mutex<Option<_>>`**：
`stream_a_cache: Mutex<Option<StreamPair>>` 是裸 Mutex（`client_hello` /
`server_hello` 单 task 内填 + `take_stream_a_recv` 单 task 内拿 —— 不跨
task 移交）；`stream_bunch` 需要 STEP-5.3 read_loop 跨 task 接管（spawn
reader task 后 `peer.send_stream_*` 仍要在原 task 复用 stream send 半边）
—— `Arc` 让两个 task 共享同一份 `Mutex<Option<StreamBunch>>`，所有权
切割问题由 `Arc` 自动解决。

**默认 `None`**：STEP-5.3 `read_loop` 装配 3 条 stream 时填充；本步仅占
位字段，dead_code 守护由 `StreamBunch` 类型的 `#[allow(dead_code)]` 覆盖。

## 3. 与 PLAN-M1 §5.2 的偏差

### PLAN-M1 偏差 #N-8：`send_stream_b` 不做 cache 命中复用

- **PLAN §5.2 文字隐含**：`send_stream_b` 复用 `StreamBunch.b.send`
  （"cache 命中"）
- **实际**：每次调用 `send_stream_b` 都调 `conn.open_bi()` 拿新一条 bidi
  stream，**不**复用。`send` / `recv` 半边随方法结束 drop（recv 立即
  drop，send 在 `finish()` 后 drop）
- **影响**：
  - datagram 多次降级 → 多次开新 stream（每个 stream 都独立发一帧），
    接收端 STEP-5.3 reader 各自收一帧（不要求 stream 复用）
  - 与 bak `mousehop/src/quic_transport.rs:557-579 send_stream_b` 的
    "cache 命中复用 / 未命中 open_bi"语义略不同（bak Step 1.9a 就有
    cache）
- **本步取舍**：避免 PeerSession 字段碎片化（再加 `stream_b_cache:
  Mutex<Option<StreamPair>>`）。STEP-5.3 read_loop 接入时统一重构：
  - 引入 `stream_b: Mutex<Option<StreamPair>>` 字段（与 `stream_a_cache`
    对称）
  - 把 `stream_b` / `stream_c` 字段合并进 `stream_bunch: Arc<Mutex<Option<StreamBunch>>>`
  - `send_stream_b` 改为 in-lock 借用（与 bak 形态一致）
- **不构成功能问题**：M1 范围每个 stream 都发一帧的设计下不依赖 cache；
  cache 复用是 M2+ 优化（同一 stream 多次发，节省握手 / 流 ID 开销）

### 与 PLAN §5.2 描述一致的项

| 项 | 落实 |
|---|---|
| `pub struct Bidi<S> { send, recv }` | ✅ |
| `pub struct StreamBunch { a, b, c }` | ✅ |
| 帧格式 `[u32 BE length][bytes...]` | ✅ |
| `pub async fn write_frame` | ✅（generic W + `std::result::Result`） |
| `pub async fn read_frame` | ✅（generic R + `std::result::Result`） |
| `Error::FrameTooLarge(usize)` | ✅ |
| `Error::Truncated` | ✅（额外，PLAN 未明确但 SUGGESTION 治理需要） |
| 替换 STEP-5.1 inline 降级 | ✅（`Error::DatagramFallback` 不再被调用） |
| 单测 `frame_round_trip` + `frame_truncated_rejected` | ✅（mock duplex 流，不依赖 QUIC 握手） |
| `send_stream_b` cache | ⚠️ 本步**不**做 cache，留 STEP-5.3（详见 §3） |

## 4. 与 PLAN §9 M1 边界检查

| §9 类别 | 本步触碰 | 说明 |
|---|---|---|
| `proto` 变体 / 常量 / 错误 / codec | 否 | 仅用既有 `ProtoEvent` + `MAX_EVENT_SIZE` |
| `input-event` | 否 | 没动 |
| `ipc::TransportEvent` | 否 | 没动 |
| `lan-mouse-gtk::status_bar` | 否 | 没动 |
| `lan-mouse-cli` stderr 订阅 | 否 | 没动 |
| `lan-mouse/src/clipboard*.rs` | 否 | 没动 |
| `Cargo.toml` 引入 h3/h3-quinn/http | 否 | 0 依赖变更 |
| `quic_transport.rs::Stream C` reader | **否**（关键）| `StreamBunch.c` 字段定义但**不开** reader task；M2 才接 |
| `connect.rs` mDNS / discovery | 否 | 没动 connect |

**结论**：0 越界。

## 5. 验证结果

### 5.1 `cargo check -p lan-mouse --lib`

```
$ cargo check -p lan-mouse --lib 2>&1 | grep -cE "^error\[E"
14

$ cargo check -p lan-mouse --lib 2>&1 | grep "quic_transport\.rs" | grep "error\["
# （无输出 —— 本步新增代码 0 编译错）
```

14 errors 全部来自 `src/connect.rs` 与 `src/listen.rs` 的 `webrtc_dtls` /
`webrtc_util` 引用（STEP-1.2 故意留下，待 STEP-6.x 切 PeerSession 时一次
性替换）。本步新增 `Bidi` / `StreamBunch` / `write_frame` / `read_frame` /
`Error::StreamB` / `Error::FrameTooLarge` / `Error::Truncated` /
`send_stream_b` / 2 个单测 + `PeerSession.stream_bunch` 字段
**0 编译错**。

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
- 基线（STEP-5.1 提交后）：27 errors
- 本步提交后：27 errors（**0 增量**）
- 27 = 14 DTLS pre-existing + 13 fixture 错误（与 STEP-5.1 报告完全一致）

### 5.3 §9 M1 边界 grep

```
$ grep -nE "TransportEvent|Bounds|MotionAbsolute|CursorPos|ReceiverSensitivity|MAX_CLIPBOARD_SIZE|BufferTooLarge|ClipboardEvent|axis::momentum|MACOS_KEEP_AWAKE_EVENT_TAG|h3|h3-quinn|status_bar|clipboard" src/quic_transport.rs
# （唯一命中：doc 注释引用 M2 计划标记，0 代码命中 —— §9 12 类 grep 无命中）
```

### 5.4 单测 `frame_round_trip` / `frame_truncated_rejected`

```
$ cargo test -p lan-mouse quic_transport::tests::frame_*
error: could not compile `lan-mouse` (lib test) due to 14 previous errors
```

**单测无法跑通** —— `lan-mouse` lib 因 STEP-1.2 留下的 14 DTLS errors
编不过；test target 与 lib 同编译单位（SUGGESTION #S-5）。

**重要差异**：与 STEP-5.1 / STEP-5.4 不同，**本步单测不依赖 QUIC 握手**
—— 用 `tokio::io::duplex(4096)` mock 出 `AsyncRead + AsyncWrite + Unpin` 的
双向流，**理论上** 14 errors 修复后**应直接通过**（无需修任何测试代码）。
STEP-6.x 修 14 errors 后 Leader 手动跑 `cargo test -p lan-mouse
quic_transport::tests::frame_round_trip quic_transport::tests::frame_truncated_rejected`
确认（不需要修 stream A 缓存或 hello 握手路径）。

## 6. 处理的 SUGGESTION 项

**#S-14 完全解决**：
- `send_motion` 降级路径从 inline `open_uni() + write_all() + finish()`
  替换为 `self.send_stream_b(bytes).await?`
- `Error::StreamB(String)` 替换 `Error::DatagramFallback(String)`（与 bak
  `mousehop/src/quic_transport.rs:1035-1040 Error::StreamB` 完全对齐）
- `send_stream_b` 用长度前缀帧 `[u32 BE len][body]` 替代裸 `write_all`
  （与对端 STEP-5.3 `read_frame` codec 对齐）

SUGGESTION.md 中 #S-14 条目进入"待 Leader 评审后删除"状态。

无新增 SUGGESTION 条目。

## 7. 闸门检查（PLAN-M1 §1 时间门 / §9 边界门）

| 闸 | 结果 |
|---|---|
| **§1 时间门**：30 min 目标 | ⚠️ 轻微超 30 min（实际 ~40 min）—— 但 < 1h 红线，未触发"就地拆 a/b/c" |
| **§9 边界门** | ✅ 0 越界（详见 §4） |
| **STEP-5.1 依赖** | ✅ 已归档 |
| **闸 2 实时自检** | ✅ 14 errors 全部 DTLS、本步 0 增量 |
| **闸 3 STEP 收尾** | ✅ `cargo check -p lan-mouse --lib` + `--tests` 通过基线 |

## 8. 遗留 / 风险

- ⚠️ **`send_stream_b` 不做 cache 命中复用**（偏差 #N-8）：本步每条
  降级写都开新 stream（不缓存）；STEP-5.3 read_loop 接入时统一重构
  —— 引入 `stream_b: Mutex<Option<StreamPair>>` 字段 + 合并进
  `PeerSession.stream_bunch` + `send_stream_b` 改 in-lock 借用
- ⚠️ **`Error::DatagramFallback` 变体仍存在但已无 caller**：本步
  未删除（保守做法 —— 防止未来临时再用 inline uni stream）。STEP-7.3
  收尾时可一并删
- ⚠️ **单测 `frame_round_trip` / `frame_truncated_rejected` 无法在本步
  端到端跑通**：14 DTLS errors 阻塞 lib 编译（与 SUGGESTION #S-5 同
  根因）。本步单测**不**依赖 QUIC 握手，理论上 14 errors 修复后直接
  通过 —— STEP-6.x 修 errors 后 Leader 手动跑 `cargo test -p lan-mouse
  quic_transport::tests::frame_*` 确认通过
- ⚠️ **dead_code chain**：`Bidi<S>` / `StreamBunch` / `write_frame` /
  `read_frame` / `PeerSession.stream_bunch` 字段当前均加
  `#[allow(dead_code)]` 守护（与 STEP-3.2 `StreamPair` 同模式）——
  STEP-5.3 read_loop 接入时移除

## 9. 下一步（STEP-5.3 前置条件）

✅ 就绪：
- `Bidi<S>` / `StreamBunch { a, b, c }` 类型定义完整
- `write_frame` / `read_frame` 公共 codec（generic AsyncWrite/AsyncRead）
- `Error::StreamB(String)` / `Error::FrameTooLarge(usize)` / `Error::Truncated` 错误变体
- `PeerSession.stream_bunch: Arc<Mutex<Option<StreamBunch>>>` 字段就位
- `send_stream_b` 私有 helper（不含 cache 命中复用，留 STEP-5.3）
- `send_motion` 降级路径替换为 `send_stream_b`（SUGGESTION #S-14 已消化）
- 2 个 codec 单测代码就位（不依赖 QUIC 握手）

**未做 git commit**：等 Leader 处理（本步仅动 `src/quic_transport.rs` +
`next/SUGGESTION.md`）。

下一步建议：执行 **STEP-5.3** —— 3 条 stream 独立读 task + 路由分派
（`PeerSession::read_loop(recv_a) -> ReadStreams { b, c }` + 派发表按 §3 +
backpressure）。搬运参考：
`lan-mouse-pro-bak/mousehop/src/quic_transport.rs:2126-2600` 整体。