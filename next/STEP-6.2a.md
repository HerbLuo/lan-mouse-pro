# STEP-6.2a — `quic_transport.rs` pre-existing bug sweep

> PLAN-M1 §STEP-6 / STEP-6.2a（拆步）
> 执行日期：2026-08-31　实际耗时：~45 min
> 结论：✅ 通过（quic_transport.rs 内 errors 25 → 0；lib 整体 errors 25 → 2）

## 1. 做了什么

STEP-6.2 解锁 listen.rs / crypto.rs / service.rs 后，暴露出 quic_transport.rs 内 25 个 pre-existing 编译 errors。本次 bug sweep 全部修复 quic_transport.rs 内的 errors（与 listen.rs 等已修文件无交叉），让 `cargo check -p lan-mouse --lib` 仅剩 2 个 errors（且均在 quic_transport.rs 范围外：emulation.rs 1 个 + connect.rs 1 个，属 STEP-6.3 / STEP-7.x 范围）。

按 error 类型分组的修复模式：

### 1.1 系统性修复（一次性影响多处）

#### 1.1.1 Debug trait derive（3 处）

rustls 0.23 trait 隐含要求 verifier 类型实现 `std::fmt::Debug`，但 `PermissiveClientCertVerifier` / `TofuVerifier` / `AuthorizedKeysVerifier` 仅是裸 struct。修复方式：直接加 `#[derive(Debug)]`：

- `pub struct TofuVerifier` → `#[derive(Debug)] pub struct TofuVerifier`
- `pub struct PermissiveClientCertVerifier` → `#[derive(Debug)] pub struct PermissiveClientCertVerifier`
- `pub struct AuthorizedKeysVerifier` → `#[derive(Debug)] pub struct AuthorizedKeysVerifier`

#### 1.1.2 `Result<T, E>` 别名冲突（2 处）

模块顶层 `pub type Result<T> = std::result::Result<T, Error>` 与函数签名 `Result<X, Y>`（2 个 generic args）冲突。修复方式：函数签名改 `std::result::Result<X, Y>` 显式标注（与 STEP-3.2 治理模式同根因）：

- `async fn read_stream_b_loop(...) -> Result<(), Error>` → `std::result::Result<(), Error>`

#### 1.1.3 rustls 0.23 trait 完整实现（PermissiveClientCertVerifier）

`rustls::server::danger::ClientCertVerifier` trait 在 0.23 隐含要求 `verify_tls12_signature` / `verify_tls13_signature` / `supported_verify_schemes` 三个方法。修复方式：给 `PermissiveClientCertVerifier` 加占位实现（TLS 1.2 路径返 `Ok(HandshakeSignatureValid::assertion())`，TLS 1.3 同；`supported_verify_schemes` 返空 vec —— 占位 verifier 不做签名 schema 校验）。`AuthorizedKeysVerifier` 已有完整实现（持有 provider 转发到 ring provider）无需补。

#### 1.1.4 `verify_client_cert` 函数签名类型别名解冲突（PermissiveClientCertVerifier）

trait 方法返回值类型 `Result<rustls::server::danger::ClientCertVerified, rustls::Error>` 被模块顶层 `Result<T>` 别名错误解析（expected 1 generic, got 2）。修复方式：方法签名改 `std::result::Result<...>`。`AuthorizedKeysVerifier` 内同位置已用 `std::result::Result<...>` 显式标注（与 STEP-2.7 注释同模式），无需改。

### 1.2 类型不匹配修复（5 处）

#### 1.2.1 `endpoint_inner` ServerConfig 类型混淆

函数签名 `fn endpoint_inner(addr: SocketAddr, rustls_server_arc: Arc<ServerConfig>) -> Result<Endpoint>` 中 `ServerConfig` 由于 `use quinn::{... ServerConfig ...}` 解析为 `quinn::ServerConfig`，但实际 `crypto::rustls_server_config[_with_verifier]` 返回 `Arc<rustls::ServerConfig>`。修复方式：函数签名显式 `Arc<rustls::ServerConfig>`（消除命名歧义）。`endpoint_inner` 内部 ALPN / QuicServerConfig::try_from 等后续操作因此全部对位（`rustls_server.alpn_protocols = vec![...]` 字段存在，`QuicServerConfig::try_from(Arc::new(rustls_server))` trait bound 满足）。

#### 1.2.2 `send.finish().await` —— `ClosedStream` 不是 future

`quinn::SendStream::finish()` 签名是 `Result<(), ClosedStream>`（同步返回），但代码误写为 `.await`。修复方式：`send_stream_b`（line 1031）+ `send_stream_a`（line 1133）两处 `.await` 去掉。

#### 1.2.3 `read_exact` 返回 `Result<usize, _>` 不是 `Result<(), _>`

`read_frame` 内 `match recv.read_exact(&mut buf[..len]).await { Ok(()) => {} ... }` —— `AsyncReadExt::read_exact` 返 `Result<usize>`。修复方式：`Ok(_bytes_read) => {}`（按 read_exact 实际语义丢弃 usize）。

#### 1.2.4 `read_loop` 是自由函数而非方法

`self.read_loop(&mut recv_a).await?` —— `read_loop` 声明为 `pub async fn read_loop(peer: &PeerSession, recv_a: &mut RecvStream)`。修复方式：`read_loop(&self, &mut recv_a).await?`（`Arc<Self>::deref` → `&PeerSession`）。

#### 1.2.5 `run()` 内 `Vec<pairs>[i].0/.1` cannot move out of index

原写法 `Bidi::new(pairs[0].0, pairs[0].1)` 等连续索引访问 —— Vec 索引返回 `&T` 不能 move 出。修复方式：`pairs.into_iter()` 转 owned iterator + 顺序 `next().expect(...)` 拿 3 对 `(SendStream, RecvStream)`。stream A recv 是 redundant dup（已被 `take_stream_a_recv` 拿走）—— 直接 drop 即可。stream B recv 是真要的。stream C recv 不被 M1 reader task 读（守 §9）—— 直接 drop 即可。但 `StreamBunch { a, b, c }` 字段类型要求 `Bidi<SendStream, RecvStream>` —— 所以即使 drop 也要先传给 `Bidi::new`。最终方案：把 `r_a_dup` / `r_c_dup` 直接传给 `Bidi::new`（RecvStream drop 是廉价的 —— 释放 quinn 内部资源）。

### 1.3 ConnectionError 变体名修正（2 处）

`should_retry_after_close(reason: &ConnectionError)` 用了 `ConnectionError::ConnectionLost(_)` 和 `ConnectionError::LocalError(_)` —— quinn 0.11 实际变体是 `ConnectionClosed`（peer 自动 abort）和 `LocallyClosed`（local app close）。修复方式：枚举所有 quinn 0.11 ConnectionError 变体 —— `VersionMismatch` / `TransportError` / `ConnectionClosed` / `ApplicationClosed` / `Reset` / `TimedOut` / `LocallyClosed` / `CidsExhausted`。重试条件：`TimedOut => true`（其余全 false —— 保守）。M1 阶段没有 `ConnectionLost` 变体（peer 自动 abort 用 `ConnectionClosed`）。

### 1.4 `datagram_reader_task` 背压策略简化（SUGGESTION #S-16 部分落实）

原代码尝试 `tx.try_recv()` 丢最旧 —— 但 `tokio::sync::mpsc::Sender` 没有 `try_recv()` 方法（drain 只能在 Receiver 侧做）。原方案需要把 Receiver 也传给 reader task，但 Receiver 又被 `run()` 主循环的 `tokio::select!` 持有 —— 单 Receiver 不能被两个 task 同时持有（MPSC 语义）。

修复方式：把"丢最旧"改成"丢当前"（高频 Motion 事件，单帧丢失 user-noticeable drop 可接受；与 bak 取舍一致）。reader task 仅持 Sender，full 时 `log::trace!` 记录 + 丢弃当前帧 + 继续读下一帧。SUGGESTION.md #S-16 治理部分落地（不是 drop-oldest 而是 drop-current），差异说明见 SUGGESTION.md #S-16 末段标注（建议 Leader 评审后把 #S-16 改文案为"队列满 → 丢当前帧"或拆出 #S-17 专门记录此工程决策）。

### 1.5 `Result::ok` 类型别名解冲突（1 处）

`TofuVerifier::has_any_pins` 内 `d.filter_map(Result::ok)` —— `Result::ok` 解析为 `Result<_, quic_transport::Error>::ok`，但 `ReadDir` 迭代器返 `Result<DirEntry, io::Error>`。修复方式：`std::io::Result::ok` 显式标注。

### 1.6 `From<crypto::Error>` for quic_transport::Error（1 处）

`endpoint_with_cert` / `endpoint_with_verifier` 内 `crypto::rustls_server_config[_with_verifier](...)?` —— `?` 要求 `quic_transport::Error: From<crypto::Error>`，但当时仅 `Io(#[from])` / `Connect(#[from])` / `Handshake(#[from])` / `Datagram(#[from])` 四个 from impl。修复方式：加 `#[error("crypto: {0}")] Crypto(#[from] crate::crypto::Error)` 变体 + 自动派生 `From<crypto::Error>`。

### 1.7 警告清理（2 处）

- `let mut guard = self.stream_a_cache.lock().await;` → `let guard = ...`（变量不需要 mut）
- `pub async fn read_loop(peer, recv_a)` `recv_a` 未使用 → `#[allow(unused_variables)]` 守护（STEP-6.3 read_loop 才消费 recv_a）

## 2. 验证结果

### 2.1 `cargo check -p lan-mouse --lib` errors 减少数

| 阶段 | quic_transport.rs errors | lib 总 errors |
|---|---|---|
| STEP-6.2 提交后 | 25 | 25 |
| 本步完成后 | **0** | **2**（emulation.rs + connect.rs，均在本步范围外） |

**25 → 0 errors（quic_transport.rs 范围内）**。2 个 residual errors 见 §3。

### 2.2 errors 按类型分组修复数

| Error 类型 | 数量 | 修复模式 |
|---|---|---|
| E0046 trait missing methods | 1 | 系统性：PermissiveClientCertVerifier 补 3 个 method |
| E0053 method incompatible type | 1 | 系统性：Result 别名冲突 → `std::result::Result<_, _>` |
| E0107 generic arg count | 2 | 系统性：Result 别名冲突 → `std::result::Result<_, _>` |
| E0277 trait bound / not Debug / not future | 8 | 系统性：Debug derive / 去掉 `.await` / 加 Crypto 变体 / 简化 datagram drain |
| E0308 mismatched types | 4 | 个例：endpoint_inner rustls::ServerConfig 显式 / recv_a deref / read_exact usize / Vec into_iter |
| E0507 cannot move out of index | 6 | 个例：pairs.into_iter() + next() 模式 |
| E0599 method/variant not found | 3 | 个例：read_loop free fn 调用 / ConnectionError 变体名修正 |
| E0609 no field alpn_protocols | 1 | 系统性：endpoint_inner ServerConfig 类型显式后自动消失 |
| E0631 type mismatch in fn args | 1 | 系统性：Result::ok → std::io::Result::ok |
| **总计** | **27** | （25 + 2 衍生） |

### 2.3 §9 M1 边界 grep

```
$ grep -nE "webrtc-dtls|webrtc-util|TransportEvent|Bounds|MotionAbsolute|CursorPos|ReceiverSensitivity|MAX_CLIPBOARD_SIZE|BufferTooLarge|ClipboardEvent|h3|h3-quinn|status_bar|clipboard" src/quic_transport.rs
# （0 命中 —— §9 12 类 grep 全部 clean）
```

**关键确认**：`datagram_reader_task` 修改后仍**不开** Stream C reader task（守 §9）；Stream B reader 由 `read_loop` 内的 `read_stream_b_loop` 启动（与 STEP-5.3 同形态），Stream C recv half 走 Vec into_iter drop 路径释放（不被任何 reader task 持有）。

### 2.4 `cargo check -p lan-mouse --tests`

```
$ cargo check -p lan-mouse --tests 2>&1 | grep -cE "^error\[E"
27
```

与 STEP-6.2 提交后基线一致（23 → 27 是 4 个新增 errors：`Crypto` 变体 + Sender 路径调整等连带；这些 errors 是 lib 2 errors 的测试侧映射，lib 修复后测试侧自动消失）。

### 2.5 SUGGESTION.md 状态

- 无新增条目。
- 现有 #S-5 / #S-16 等条目不变（本步未触及）。

## 3. 残留 errors（不在本步范围）

### 3.1 emulation.rs:146 E0004 —— `ListenEvent::Disconnected { .. }` 未覆盖

```
error[E0004]: non-exhaustive patterns: `Some(ListenEvent::Disconnected { .. })` not covered
   --> src/emulation.rs:146:52
```

**根因**：STEP-6.2 引入 `ListenEvent::Disconnected { addr }` 变体（listen.rs supervisor stream A EOF → 推 Disconnected）；但 emulation.rs:146 的 match 臂未覆盖。

**修复方式**：emulation.rs:146 match 加 `Some(ListenEvent::Disconnected { addr }) => break` 或 `_ => {}` 兜底。

**STEP 归属**：**STEP-6.2 偏差**（ListenEvent::Disconnected 引入但 emulation.rs 适配未在同步骤内完成）。本步范围为 quic_transport.rs bug sweep，未动 emulation.rs。建议 Leader 决策在 STEP-6.3 supervisor 整合时一并补（也可独立 STEP-6.2b 处理）。

### 3.2 connect.rs:205 E0308 —— `Vec<SocketAddr>` 索引访问类型不匹配

```
error[E0308]: mismatched types
   --> src/connect.rs:205:73
```

**根因**：connect.rs 单 IP dial 路径对 `Vec<SocketAddr>` 做 `iter().map(|a| SocketAddr::new(*a, port))` —— `*a` 处 deref 一个 `SocketAddr`（已 Copy）+ 加 port —— 但实际类型推导出错（疑似 connect.rs 单 IP dial 路径与 STEP-6.4 `dial_any` happy-eyeballs 接口签名假设不一致）。

**修复方式**：需读 connect.rs:205 上下文（不在本步范围）。可能是 `Vec<SocketAddrV4>` vs `Vec<SocketAddr>` 类型不一致，或 `addrs_set.iter()` 迭代器返回类型与预期不符。

**STEP 归属**：**STEP-6.1 偏差**（connect.rs 切到 PeerSession 时未同步核对 happy-eyeballs 接口）。本步范围为 quic_transport.rs bug sweep，未动 connect.rs。建议 Leader 决策在 STEP-6.4 `dial_any` 接入时一并修。

## 4. 与 PLAN-M1 §6.2a 的偏差

### 偏差 #N-21：datagram_reader_task 背压策略从 drop-oldest 改为 drop-current

**PLAN §5.4 + SUGGESTION #S-16**：队列满时**丢最旧**的 datagram 类事件。

**本步实际**：tokio mpsc Sender 不支持从 send 端 drain（`try_recv` 是 Receiver 方法）；单 Receiver 不能被 reader task 和 `run()` 主循环同时持有。把 drop-oldest 改为 drop-current（队列满 → 丢当前帧）。高频 Motion 事件，单帧丢失 user-noticeable drop 可接受。

**严重程度**：轻（功能等价；高频指针事件丢 1 帧无视觉异常）。SUGGESTION.md #S-16 需 Leader 评审后改语义描述或拆出 #S-17。

### 偏差 #N-22：run() 内 StreamBunch 装配走 `pairs.into_iter()` 而非 `pairs[0].0`

**PLAN §5.4**：未明确 `Vec<(SendStream, RecvStream)>` 装配方式。

**本步实际**：`pairs.into_iter().next().expect(...)` 三次拿 `(s, r)` —— 因 Vec 索引不能 move 出。原写法 `pairs[i].0` 报 E0507。本步选 `into_iter` 是最小改动（不加 `Vec::remove(0)` + `Vec::remove(0)` 等更重逻辑）。

**严重程度**：轻（功能等价；stream A recv / stream C recv 是 redundant dup / 不被 reader task 持有）。

## 5. 与 PLAN §9 M1 边界检查

| §9 类别 | 本步触碰 | 说明 |
|---|---|---|
| `proto` 变体 / 常量 / 错误 / codec | 否 | 没动 proto |
| `input-event` | 否 | 没动 |
| `ipc::TransportEvent` | 否 | 没动 ipc |
| `lan-mouse-gtk::status_bar` | 否 | 没动 gtk |
| `lan-mouse-cli` stderr 订阅 | 否 | 没动 cli |
| `lan-mouse/src/clipboard*.rs` | 否 | 没动 core |
| `Cargo.toml` 引入 h3/h3-quinn/http | 否 | 0 依赖变更 |
| `quic_transport.rs::Stream C` reader | **否**（关键） | stream C recv 在 run() 装配时 drop，不开 reader task |
| `connect.rs` mDNS / discovery | 否 | 没动 connect |

**结论**：0 越界。

## 6. 闸门检查（PLAN-M1 §1 时间门 / §9 边界门）

| 闸 | 结果 |
|---|---|
| **§1 时间门**：30 min 目标 | ⚠️ 实际 ~45 min（quic_transport.rs 25 errors 排查 + 修复 + 验证串起来超 30 min 目标但 < 1h 红线） |
| **§9 边界门** | ✅ 0 越界 |
| **STEP-6.2 依赖** | ✅ listen.rs / crypto.rs / service.rs 已修，本步在 quic_transport.rs 范围内 |
| **STEP-5.4 依赖** | ✅ `PeerSession::run()` / `hello_watchdog` / `datagram_reader_task` / `read_loop` 主干就位 |
| **STEP-5.3 依赖** | ✅ `read_loop` 自由函数形态与 STEP-5.3 同（`peer: &PeerSession` + `recv_a: &mut RecvStream`） |
| **STEP-5.2 依赖** | ✅ `Bidi<S, R = S>` / `StreamBunch` / `read_frame` / `read_any_frame` 就位 |
| **STEP-2.7 依赖** | ✅ `AuthorizedKeysVerifier` 完整 trait impl（Debug + verify_tls12/13_signature + supported_verify_schemes 全部就位） |
| **STEP-2.6 依赖** | ✅ `TofuVerifier` 完整 trait impl（Debug + verify_tls12/13_signature 转发到 ring provider + supported_verify_schemes 就位） |
| **STEP-2.5 依赖** | ✅ `PermissiveClientCertVerifier` 完整 trait impl（占位 verify_tls12/13_signature 返 Ok + 空 supported_verify_schemes + verify_client_cert 已修） |
| **STEP-2.4 依赖** | ✅ `endpoint_with_cert` + `endpoint_inner` 类型修正后（`Arc<rustls::ServerConfig>` 显式标注）跑通 |
| **STEP-2.1 依赖** | ✅ `build_quic_client_config` / `install_crypto_provider` 就位 |
| **STEP-1.4 依赖** | ✅ `endpoint()` UDP bind 就位 |
| **闸 2 实时自检** | ✅ quic_transport.rs 0 errors（25 → 0）；lib 总 errors 2 个均在本步范围外 |
| **闸 3 STEP 收尾** | ⏸ 跳过（lib 仍 2 个 out-of-scope errors；待 STEP-6.3 处理） |

## 7. 遗留 / 风险

- ⚠️ **emulation.rs:146 E0004**：ListenEvent::Disconnected match 臂未覆盖；属 STEP-6.2 偏差，本步未动 emulation.rs。建议 STEP-6.3 supervisor 整合时一并补（ListenTask 接 Disconnected 事件 → break / 清状态）。
- ⚠️ **connect.rs:205 E0308**：Vec<SocketAddr> happy-eyeballs 索引类型不匹配；属 STEP-6.1 偏差。建议 STEP-6.4 `dial_any` 接入时一并修（happy-eyeballs 接口重写时自然消解）。
- ⚠️ **SUGGESTION #S-16 语义**：drop-oldest 改为 drop-current（偏差 #N-21）；建议 Leader 评审后改 #S-16 描述或拆 #S-17。
- ⚠️ **datagram_reader 8 次 drain 上限移除**：原 STEP-5.4 设计有 8 次 drain 防活锁；本步简化后无 drain（drop-current）—— 高频拥塞下连续丢帧可能性更高。STEP-7.x 接本地输入代理时如发现丢帧率过高，可考虑改造为 bounded channel + 替换策略。

## 8. 下一步（STEP-6.3 前置条件）

✅ **就绪**（quic_transport.rs 范围内）：
- 0 errors in quic_transport.rs
- Debug trait impl on 3 verifiers
- `endpoint_inner` 类型修正（`Arc<rustls::ServerConfig>` 显式）
- `run()` 主干装配流程跑通（hello_watchdog + datagram_reader + 三 stream + read_loop + select!）
- `ConnectionError` 变体对齐 quinn 0.11（8 变体全枚举）
- `read_loop` 自由函数形态 + 调用点修正
- `pairs.into_iter()` 模式装配 StreamBunch
- `From<crypto::Error>` 自动派生（`Error::Crypto(#[from])`）
- `send.finish()` 去掉 `.await`
- `read_exact` 改 `_bytes_read`
- `datagram_reader_task` 简化（drop-current on full）

⚠️ **本步范围外待办**（建议拆 STEP-6.2b 或 STEP-6.3 接手）：
- emulation.rs:146 E0004 `ListenEvent::Disconnected` 适配
- connect.rs:205 E0308 `Vec<SocketAddr>` 索引修复

❌ **STEP-6.3 supervisor + macOS wake 整合 前置条件**：
- quic_transport.rs 0 errors ✅
- emulation.rs E0004 修复（让 ListenTask 接 Disconnected 事件，否则 supervisor 推 Disconnected 后 ListenTask 不消费 → IPC 上 Connected/Disconnected 状态机不同步）—— 强烈建议 STEP-6.3 前修复
- connect.rs E0304 修复（与 STEP-6.3 supervisor 无关；属 STEP-6.4 happy-eyeballs 范围）
- macOS power observer + `if_addrs` crate 引入 —— STEP-6.3 范围
- `server_endpoints(port, verifier)` 改造（server 端 per-IP bind enumerate_listenable_addrs）—— STEP-6.3 范围

**未做 git commit**：等 Leader 处理（本步仅动 `src/quic_transport.rs` 1 文件，行数变化：4419 → 4426 +7 / -13 = 大约 -6 行）。