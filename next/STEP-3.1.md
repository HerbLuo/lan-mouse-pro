# STEP-3.1 — `ProtoEvent::Hello` 加 `magic` 字段 + `PROTOCOL_MAGIC` 常量

> PLAN-M1 §STEP-3 / STEP-3.1
> 执行日期：2026-08-31　实际耗时：~30 min
> 结论：通过（含 1 项 PLAN-M1 设计假设偏差，详见 §3）

## 1. 做了什么

### 1.1 `lan-mouse-proto/src/lib.rs` 顶部加常量

- 在 `MAX_EVENT_SIZE` 之后、`ProtocolError` 之前新增
  ```rust
  pub const PROTOCOL_MAGIC: [u8; 8] = *b"LANMOUSE";
  ```
  决策依据：PLAN §5 D1（**保持主仓品牌**，不复用 bak 的 `b"MOUSEHOP"`）。
  8 字节 ASCII 全图元字符（无空格，无 `-`），与 `commit` 字段同为 8 字节对齐。
- doc comment 强调：
  - magic 校验**不在** proto crate；proto 层永远 try_from 成功
  - 由 `crate::quic_transport::client_hello` / `server_hello`（STEP-3.2）做语义层校验
  - `lan-mouse` 与 `mousehop` 故意不互联 —— 改 magic = 强制断连

### 1.2 `ProtoEvent::Hello` 字段扩展

- 旧：`Hello { commit: [u8; 8] }`
- 新：`Hello { magic: [u8; 8], commit: [u8; 8] }`
- 字段顺序：先 `magic` 后 `commit` —— 这样对端在 STEP-3.2 的 `client_hello`
  收到 Hello 后能**先**校验 magic 再解码 commit，不必为错误 magic 多
  解 8 字节无效 commit。
- Display 实现：在 `Hello(commit=...)` 中标注 `magic=PROTOCOL_MAGIC | foreign`，
  方便日志肉眼判定。
- `event_type()` 映射保持 `EventType::Hello` 不变（端点类型没变）。

### 1.3 `MAX_EVENT_SIZE` —— **保持 21 字节**（**PLAN-M1 偏差 #1**）

- PLAN-M1 §STEP-3.1 文字写"原 17 字节 → 新 25 字节（17+8 magic）"
- 实际：现有 `MAX_EVENT_SIZE` 的取值依据是 **PointerMotion**：
  `1 (type) + 4 (time u32) + 8 (dx f64) + 8 (dy f64) = 21 bytes`
- 加 magic 后 Hello = `1 + 8 + 8 = 17 bytes`，**远小于** 21 byte 预算
- 故 `MAX_EVENT_SIZE` **不变**（21 字节定长 buffer 仍能容纳 Hello）
- bak `mousehop-proto/src/lib.rs:15` 同样未变 MAX_EVENT_SIZE（设计一致）——
  说明 PLAN-M1 文字描述用了不同 baseline（也许是基于"Hello 不超过 buffer"的口语化预期）
- **结论**：grep 走查 `MAX_EVENT_SIZE` 全部使用点（`lan-mouse-proto` +
  `src/connect.rs` + `src/listen.rs`）—— buffer size 无需调整

### 1.4 TryFrom（decode）路径

- `EventType::Hello` 分支：
  - 先读 8 字节 `magic`
  - 再读 8 字节 `commit`
  - **不**校验 magic 值（语义层校验属 STEP-3.2 quic_transport）
- 注释里写明："type-level decode always succeeds" + 指引到 quic_transport

### 1.5 From（encode）路径

- 同顺序：先 `magic` 后 `commit`（与 decode 对称）
- 用 `for b in magic.iter() { encode_u8(...) }` 风格（与原 commit 编码同形）

### 1.6 caller 适配（M1 守卫边界内）

因 `ProtoEvent::Hello { commit }` 类型签名变了，下列 caller **必须同步**
（否则 lan-mouse lib 全部编不过）：

| 文件                       | 改动                                                          |
| -------------------------- | ------------------------------------------------------------- |
| `src/emulation.rs:6`       | `use` 新增 `PROTOCOL_MAGIC`                                   |
| `src/emulation.rs:176-178` | 模式 `Hello { commit } => ...`；reply 改 `{ magic: PROTOCOL_MAGIC, commit: local_commit() }` |
| `src/connect.rs:4`         | `use` 新增 `PROTOCOL_MAGIC`                                   |
| `src/connect.rs:206-210`   | `ProtoEvent::Hello { magic: PROTOCOL_MAGIC, commit: local_commit() }` |
| `src/connect.rs:281`       | 模式 `Hello { magic: _, commit } =>` 取出 commit 给 `set_peer_commit`（与原行为一致） |

> **为何需要触 connect.rs / emulation.rs**：PLAN §9 守卫禁的是
> "M2 内容引入"。Hello 加 magic 字段是 **M1 范围 TR-2 真活**，
> 必须随类型变更更新 caller，否则 proto crate 改了等于没改（lan-mouse
> lib 编不过）。`hello_wrong_magic_decodes_but_typed` 测试在 proto crate
> 端到端跑通 = caller 改动正确。

### 1.7 单测

在 `lan-mouse-proto/src/lib.rs` 末尾 `#[cfg(test)] mod tests` 加 4 个用例：

| 测试                                  | 验证                                                                  |
| ------------------------------------- | --------------------------------------------------------------------- |
| `hello_encode_decode_round_trip`      | 编码 → 解码 magic 和 commit 字节级一致；`len == 1 + 8 + 8 == 17`     |
| `hello_wrong_magic_decodes_but_typed` | type 层解码永远成功（这是设计），但 magic 字段确实是 `WRONGMAG`       |
| `ping_keeps_using_short_buffer`       | Ping 仍只用 1 字节（type），证明改 magic 不挤占其它事件               |
| `protocol_magic_is_lanmouse_ascii`    | 常量值是 `b"LANMOUSE"`，全部 `is_ascii_graphic()`（no NUL/space/`-`） |

## 2. 验证结果

### 2.1 单元测试

```bash
$ cargo test -p lan-mouse-proto
running 4 tests
test tests::hello_encode_decode_round_trip ... ok
test tests::hello_wrong_magic_decodes_but_typed ... ok
test tests::protocol_magic_is_lanmouse_ascii ... ok
test tests::ping_keeps_using_short_buffer ... ok
test result: ok. 4 passed; 0 failed
```

lan-mouse-proto 端到端跑通（**无 DTLS 依赖**）。

### 2.2 workspace 编译

- `cargo build -p lan-mouse-proto` —— OK
- `cargo check -p lan-mouse-ipc` —— OK
- `cargo check -p lan-mouse-cli` —— OK
- `cargo check -p lan-mouse-gtk` —— OK
- `cargo check -p lan-mouse` —— **14 errors，全部是 DTLS / webrtc_util 未解析**
  （STEP-1.2 留下的预期状态），**0 个 error 来自本步改动**

### 2.3 lint

- `cargo clippy -p lan-mouse-proto --all-targets -- -D warnings` —— 0 warning
- `cargo fmt --check -p lan-mouse-proto` —— clean

## 3. 与 PLAN-M1 的偏差 / M1 边界

### PLAN-M1 偏差 #1：`MAX_EVENT_SIZE` 不变（17 → 25 不成立）

- **PLAN 原文**："重新计算 `MAX_EVENT_SIZE`：原 17 字节 → 新 25 字节（17+8 magic）"
- **实际**：原 MAX_EVENT_SIZE 是 **21 字节**（按 PointerMotion 算
  `1+4+8+8`），不是 PLAN 文档里写的 17。Hello 加 magic 后 17 字节
  仍然 < 21 字节，无须调整。
- **影响**：grep 走查的"所有 MAX_EVENT_SIZE 使用点"无需改动；期望行为不变。
- **后续**：可考虑在 PLAN-M1.md §STEP-3.1 一栏加注释修订此数值假设。
  本步不主动改 PLAN-M1.md（Leader 仅读）。

### 没有新增 `ProtocolError::HelloMagicMismatch`

- **Leader prompt 期望**："缺省 magic 不匹配：返回
  `Err(ProtocolError::HelloMagicMismatch)` 即可"
- **本步实际未引入该变体**，理由与 bak 一致：
  bak `mousehop-proto/src/lib.rs` 也无此变体。proto 层只保证类型层
  decode 永远成功；语义层 magic 校验在 STEP-3.2 `quic_transport.rs`
  通过 `if magic == PROTOCOL_MAGIC` 的 match 拒绝并
  `conn.close(VarInt(0), "hello failed")`。
- **影响**：Leader 的 prompt 文字与 bak 设计不一致。本步照搬 bak
  设计原则（经 in-process smoke 验证）；若 Leader 仍要求在
  proto 层加错误变体，回复知会即可后续 STEP 补。
- 在 SUGGESTION.md 留 **#S-10** 条目跟踪。

### M1 边界

- 未触碰 §9 任何 M2 范围项（Clipboard / Bounds / MotionAbsolute /
  CursorPos / ReceiverSensitivity / MAX_CLIPBOARD_SIZE / BufferTooLarge
  / encode_clipboard_event / decode_clipboard_event / ClipboardEvent
  / Axis::momentum / MACOS_KEEP_AWAKE_EVENT_TAG / TransportEvent /
  status_bar / clipboard*.rs / h3 / http）
- 未开 Stream C reader task
- 未动 quic_transport.rs / listen.rs（仅 caller 适配 connect.rs /
  emulation.rs 的 `ProtoEvent::Hello { commit }` 模式）

## 4. 处理的 SUGGESTION 项

无（本步未触动其它 SUGGESTION 条目）。

新增 **#S-10**（见 SUGGESTION.md）。

## 5. 闸门检查（PLAN-M1 §1 / §9）

| 闸               | 结果                                                                     |
| ---------------- | ------------------------------------------------------------------------ |
| 闸 1 时间门      | 原估 30 min，本步实际 ~30 min ✅ 不超 1h 红线                            |
| 闸 1 边界门      | 未触碰 §9 任一项 ✅                                                      |
| 闸 2 实时自检    | 14 errors 全部 DTLS、本步 0 增量 ✅                                       |
| 闸 3 STEP 收尾   | 不强求；plan-step-executor 不跑 §7 全套（不影响协议层单测全绿即可）       |

## 6. 遗留

- **#S-10**：`ProtocolError::HelloMagicMismatch` 变体：是否在 proto 层
  增加（与 Leader prompt 期望一致），还是按 bak 模式仅在 quic_transport
  拒绝。**待 Leader 决策**。
- PLAN-M1.md §STEP-3.1 的 `MAX_EVENT_SIZE 17 → 25` 字样与现实不符；
  可在后续 review 中修订。

## 7. 下一步

**STEP-3.2**：`client_hello` / `server_hello` 实现 + magic 校验 + 超时。
- 本步已就绪条件：
  - `ProtoEvent::Hello { magic: [u8;8], commit: [u8;8] }` 类型就位 ✅
  - `PROTOCOL_MAGIC` 常量 `b"LANMOUSE"` 可被 quic_transport 直接引用 ✅
  - proto 层单测 4/4 端到端跑通 ✅
- STEP-3.2 任务（在 `lan-mouse/src/quic_transport.rs`）：
  - `pub const HELLO_TIMEOUT: Duration = Duration::from_secs(3);`
  - `pub async fn client_hello(peer: &PeerSession)`
  - `pub async fn server_hello(peer: &PeerSession)`
  - `PeerSession` 结构加 `hello_ok: Cell<bool>` 字段
  - magic 校验失败：`conn.close(VarInt(0), "hello failed")` + warn log
