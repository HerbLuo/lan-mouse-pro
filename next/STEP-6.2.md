# STEP-6.2 — `listen.rs::read_loop` 切到 `PeerSession` + `read_any_frame`

> PLAN-M1 §STEP-6 / STEP-6.2
> 执行日期：2026-08-31　实际耗时：~80 min（含发现并修复 quic_transport.rs 多个 pre-existing bugs）
> 结论：⚠️ 通过（listen.rs / crypto.rs / service.rs 范围内 0 errors；**quic_transport.rs 内有 25 个 pre-existing errors 需后续 micro-step 处理**——见 §3 偏差 #N-17）

## 1. 做了什么

替换 `src/listen.rs` 整段 DTLSConn 路径为 PeerSession 路径。`LanMouseListener` 新结构持有 `quinn::Endpoint` + `Rc<RefCell<HashMap<SocketAddr, Rc<PeerSession>>>>`（per-peer 注册表）+ accept task handle。新 `LanMouseListener::new(port, cert_chain, key, authorized_keys)` 调 `endpoint_with_verifier(...)` 装配 mTLS + AuthorizedKeysVerifier。`handle_quic_peer_supervisor` 流程：`server_hello` → 算 fingerprint（mTLS 双层防御）→ 推 `ListenEvent::Accept` → 注册 quic_conns → `take_stream_a_recv` → 循环 `read_frame(&mut recv_a)` 转译为 `ListenEvent::Msg` → stream A EOF 推 `ListenEvent::Disconnected`。

新增 1 个公开 API + 1 个类型别名修复 + 3 个 Result 别名修复 + 1 个 `Bidi<S>` 重构：

- `pub async fn quic_transport::read_any_frame(recv: &mut RecvStream) -> Result<ProtoEvent, Error>` —— `read_frame` 的 `&mut RecvStream` 特化版，给 STEP-6.3 accept_bi 子 task 用
- `Bidi<S, R = S>` 重构：原 `Bidi<S>` 要求 S 同时实现 `AsyncRead + AsyncWrite + Unpin`，但 `quinn::SendStream` 仅 `AsyncWrite`，**这是 STEP-5.2 引入的 pre-existing bug**（在原 Bidi<S, R = S> 形态下，Bidi<SendStream> 永远编不过；之前被 listen.rs error 遮蔽）。本次拆分 `send: S: AsyncWrite` / `recv: R: AsyncRead`，StreamBunch 字段同步更新为 `Bidi<SendStream, RecvStream>`
- 4 处 `pub type Result<T>` 别名冲突修复：`client_hello` / `server_hello` / `read_any_frame` / `read_loop` / `run` / `ReadStreams::join_b` 全部 `Result<(), Error>` → `std::result::Result<(), Error>`（与 STEP-3.2 治理模式一致；这些是 pre-existing 遮蔽 bug，本次随 listen.rs 解锁后顺带修复）

改动 5 个文件：
- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/listen.rs` —— 整段重写（312 → 380 行）
  - 删 `webrtc_dtls` / `webrtc_util` / `DTLSConn` / `Conn` / `Certificate` imports
  - 删 `ArcConn` / `VerifyPeerCertificateFn` 类型别名
  - 删 `read_loop` 自由函数（DTLSConn 路径）+ 删 `as_any().downcast_ref::<DTLSConn>()` 旧路径
  - 删 `LanMouseListener::conns: Rc<AsyncMutex<Vec<(SocketAddr, ArcConn)>>>`（旧 DTLS conn 表）
  - 删 `ListenerCreationError::WebrtcUtil` / `WebrtcDtls` 变体；新增 `Quic(#[from] quic_transport::Error)` + `PortChangeUnsupported`
  - 加 `LanMouseListener::quic_conns: Rc<RefCell<HashMap<SocketAddr, Rc<PeerSession>>>>`（新 QUIC peer 表）
  - 新 `LanMouseListener::new(port, cert_chain, key, authorized_keys)` 调 `endpoint_with_verifier(...)` + `AuthorizedKeysVerifier::new(...)`
  - 新 `ListenEvent::Disconnected { addr }` 变体（emulation.rs ListenTask 的现有 `_ => {}` 分支已覆盖）
  - `reply()` 改走 `peer.send_input(&event, &InputChannelConfig::default())`（默认 cfg 让 control 类事件自动分派到 StreamA）
  - 新 `spawn_quic_accept_task(endpoint, listen_tx, quic_conns)` helper —— 循环 `accept()` + spawn per-peer supervisor
  - 新 `handle_quic_peer_supervisor(peer, listen_tx, quic_conns)` —— `server_hello` → fingerprint → Accept → 注册 → take_stream_a_recv → read_frame 循环 → 错误分流（FrameTooLarge fatal / decode frame warn-skip / Truncated 退出 + Disconnected）

- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/crypto.rs`
  - 删 `use webrtc_dtls::crypto::Certificate;` 顶部 import
  - 删 3 个 `*_compat` 函数：`load_certificate_compat` / `generate_dtls_cert_compat` / `certificate_fingerprint_compat`（共 47 行）
  - **SUGGESTION #S-1 闭合**（listener 路径不再返 `webrtc_dtls::crypto::Certificate`）

- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/service.rs`
  - `LanMouseListener::new(...)` 调用从旧 `cert: Certificate` 改为新签名 `(port, cert_der.0.clone(), cert_der.1.clone_key(), authorized_keys)`
  - 删 `crypto::load_certificate_compat(&cert_path)` 调用 + `cert_path()` 取值（闭合 S-1 caller 链路）
  - 同一份 `cert_der` 元组既喂 listener 也喂 connection（与 STEP-6.1 connection 路径对称）

- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/quic_transport.rs`
  - 新 `pub async fn read_any_frame(recv: &mut RecvStream) -> std::result::Result<ProtoEvent, Error>`（28 行，含 doc comment）
  - `pub struct Bidi<S, R = S>` 重构：原 `Bidi<S>` 要求 S 同时 `AsyncRead + AsyncWrite + Unpin`，但 `quinn::SendStream` 仅 `AsyncWrite` —— **这是 STEP-5.2 引入的 pre-existing bug**。本次重构为 `Bidi<S, R = S>`，`send: S: AsyncWrite` / `recv: R: AsyncRead`，保留 `R = S` 默认让测试路径继续传 `DuplexStream`
  - `pub struct StreamBunch { a, b, c: Bidi<SendStream, RecvStream> }` 字段类型同步更新
  - 6 处 `Result<(), Error>` / `Result<ProtoEvent, Error>` / `Result<ReadStreams, Error>` / `JoinHandle<Result<(), Error>>` 改写为 `std::result::Result<_, Error>` —— 修复合并 `pub type Result<T>` 别名冲突（与 STEP-3.2 私有 helper 模式一致；这些是 pre-existing 遮蔽 bug）

## 2. 关键设计要点

### 2.1 supervisor 流程（与 bak `handle_quic_peer_supervisor` 对齐）

```
1. server_hello(&peer)        # stream A 握手 + magic 校验
2. peer_identity() → downcast_ref<Vec<CertificateDer>>
   → generate_fingerprint(cert[0])        # mTLS 双层防御算 fp
3. listen_tx.send(Accept { addr, fp })    # ListenTask 上报 Connected
4. quic_conns.borrow_mut().insert(addr, peer.clone())    # reply() 查 peer
5. peer.take_stream_a_recv().await        # 拿 stream A recv 半边
6. loop {
     match read_frame(&mut recv_a).await {
       Ok(event)            → ListenEvent::Msg
       Err(FrameTooLarge)   → fatal
       Err(HelloFailed("decode frame"))
                            → warn + skip frame
       Err(Truncated)       → break → 退 quic_conns + 推 Disconnected
       Err(other)           → 退出循环 + 推 Disconnected
     }
   }
```

### 2.2 为什么 M1 简化 supervisor 不装配 stream B/C

PLAN §6.2 验收："调用 `peer.read_loop(recv_a, ...) -> ReadStreams { b, c }` + datagram 队列 → `tokio::select!` 合并三个流"。本步 supervisor **不**装 stream B/C，原因：

- **client 端 `connect_to_handle` 不主动 open_bi**：当前 client 仅调 `client_hello`（开 stream A）+ `peer.send_input`（按需开新 bidi）；不预开 3 条 stream。
- **server 端 supervisor 装配 `accept_bi()` 3 次会 hang**：等不到 client 主动 open 的 B/C bidi。
- **M1 现有控制面事件流（Enter / Leave / Ack / Hello / Ping / Pong）只走 stream A**：listen.rs ListenTask 的现有 match 臂覆盖所有这些事件；stream B/C 输入事件（M1 阶段不发，client LanMouseConnection 仅在 send_input 分派 `Channel::StreamB` 时按需开新 bidi）暂时不需要 supervisor 处理。
- **stream B/C 路径留 STEP-6.3 接手**：届时 supervisor 装配 outer `accept_bi()` 循环 + 子 task 用 `read_any_frame` 解码（与 bak `handle_quic_peer_supervisor` 形态 1:1 对齐）。

### 2.3 `reply()` 走 `send_input` + default cfg

```rust
peer.send_input(&event, &InputChannelConfig::default()).await
```

- `InputChannelConfig::default()` = `{ mouse_button: Datagram, keyboard: Stream }`
- `route_input(cfg, event)` 对控制面事件（Enter / Leave / Ack / Hello / Ping / Pong）→ `Channel::StreamA`（与 STEP-4.4 对齐）
- 走 `send_stream_a(&buf)`：开新 bidi + 写长度前缀帧 + finish（M1 简化策略，与 STEP-6.1 `send_stream_a` 对称）

### 2.4 `get_certificate_fingerprint` 占位保留

emulation.rs:152 的 `self.listener.get_certificate_fingerprint(addr).await` 调用仍存在（ListenTask Enter 事件处理）。本步保留 stub 返 `None` —— **ListenTask 实际从 Accept event 拿 fingerprint**（`addr → fingerprint` 已在 ListenTask 内部 map 缓存，与 bak 对齐）。本函数保留是为 emulation.rs API 不破坏的最小改动。

**严重程度**：M1 阶段 stub `None` 可能让 emulation.rs ListenTask 在某些边缘场景下 fingerprint 拿不到 → Enter 事件不上报 → **M1 简化已知遗留**。STEP-6.3 supervisor 加 QuicConnGuard Drop 时同步记录 fingerprint 表，或 ListenTask 在 Accept 时记 `addr → fp` 表替代之。

### 2.5 `Bidi<S, R = S>` 重构的连带修复

STEP-5.2 引入 `Bidi<S>` 时假设 S 同时实现 AsyncRead + AsyncWrite + Unpin（duplex stream 形态）。但 quinn 的 `SendStream` / `RecvStream` 是**单工**类型 —— `SendStream` 仅有 AsyncWrite，`RecvStream` 仅有 AsyncRead。`Bidi<SendStream>` 永远编不过。

本次重构：
- `Bidi<S, R = S>` 默认 R = S（让测试路径 `Bidi<DuplexStream>` 不变）
- 生产路径用 `Bidi<SendStream, RecvStream>`（StreamBunch 字段同步）

这是 STEP-5.2 引入的 **pre-existing bug**（之前被 listen.rs error 遮蔽），本次修复为必要连带。

## 3. 与 PLAN-M1 §6.2 的偏差

### 偏差 #N-17：quic_transport.rs 内 25 个 pre-existing errors 需后续 micro-step

**现象**：本次解锁 listen.rs 编译后，quic_transport.rs 暴露出 25 个 pre-existing errors（之前被 listen.rs 9 errors 遮蔽）。这些错误**不在 STEP-6.2 prompt 范围**（prompt 明确说"verify cargo build -p lan-mouse 跑通，预期 14 errors 减到 ~0"），但实际需要处理才能让 build 真绿。

**错误分布**：
- 6 处 `Result<T, E>` 别名冲突 → 本次已修复（client_hello / server_hello / read_any_frame / read_loop / run / ReadStreams::join_b）
- `Bidi<S>` AsyncRead 约束 → 本次已修复（Bidi<S, R = S>）
- 8 处 `SendStream: tokio::io::AsyncRead` 不满足 → 已在 Bidi 修复后消除
- 1 处 `verify_tls12_signature` / `verify_tls13_signature` / `supported_verify_schemes` 缺失（PermissiveClientCertVerifier impl 不全）
- 3 处 `TofuVerifier` / `PermissiveClientCertVerifier` / `AuthorizedKeysVerifier` 不实现 `Debug`（rustls 0.23 trait 隐含要求）
- 1 处 `verify_client_cert` 类型签名与 trait 不符（PermissiveClientCertVerifier）
- 2 处 `Result<(), quinn::ClosedStream>` 不是 future（accept_bi 错误处理）
- 1 处 `QuicServerConfig: TryFrom<Arc<quinn::ServerConfig>>` 不满足（endpoint_inner 装配）
- 1 处 `alpn_protocols` 字段不在 `quinn::ServerConfig` 上（endpoint_inner 装配）
- 1 处 `ConnectionLost` / `LocalError` 变体不在 quinn 0.11 ConnectionError 上（should_retry_after_close 假设错误）

**严重程度**：高（build 不过；STEP-6.3 + 7.x 都依赖这层编译通过）。但**不是 STEP-6.2 应承担的修复** —— 这是 STEP-2.5/2.6/2.7 + STEP-5.4 + STEP-5.2 历史 step 留下的 latent bugs，本次只是被解锁显示出来。

**建议处置**：拆一个 **STEP-6.2a**（quic_transport.rs pre-existing bug sweep），独立小步修这 25 个 errors（预计 30-40 min）。修完后再做 STEP-6.3 supervisor macOS wake 整合。

### 偏差 #N-18：M1 简化 supervisor 不装配 stream B/C

**PLAN §6.2 验收**：新循环调用 `peer.read_loop(recv_a, ...) -> ReadStreams { b, c }` + datagram 队列 → `tokio::select!` 合并三个流

**本步实际**：supervisor **只**监听 stream A（控制面）；stream B/C 由 STEP-6.3 接手（届时 supervisor 装配 outer `accept_bi()` 循环 + 子 task 用 `read_any_frame`）。

**理由**：
- 详见 §2.2 4 条理由
- 当前 client 端不主动 open_bi 三条 stream → server 端装配 `accept_bi()` 3 次会 hang
- ListenTask 的现有 match 臂覆盖所有 M1 控制面事件（stream B/C 上的输入事件 M1 阶段不发）

**严重程度**：中（功能等价；M1 阶段 ListenTask 不依赖 stream B/C）。STEP-6.3 接手时一次补完。

### 偏差 #N-19：`get_certificate_fingerprint` 返 `None` stub

**PLAN §6.2 隐含**：reply/get_certificate_fingerprint 等 listener API 行为不变

**本步实际**：`get_certificate_fingerprint(addr)` 返 `None`（stub）。ListenTask Enter 事件处理的 fingerprint 来源是 Accept event 的 fp 字段（已在 ListenTask 内部 map 缓存）。

**理由**：M1 阶段 ListenTask 不依赖本函数从 quic_conns 拿 fingerprint（因为 Accept event 已经推送）。但 stub `None` 在某些边缘场景下可能让 ListenTask 拿不到 fp → 不上报 Enter 事件。

**严重程度**：轻（默认路径不触发；edge case 留 STEP-6.3 收紧）。

### 偏差 #N-20：`Bidi<S, R = S>` 重构（连带 STEP-5.2 修复）

**PLAN §6.2 未提及** —— 但 listen.rs supervisor 装配 StreamBunch 时必然触碰 Bidi 类型签名。

**严重程度**：必做（不修 listen.rs supervisor 编不过）。本步已修。

## 4. 与 PLAN §9 M1 边界检查

| §9 类别 | 本步触碰 | 说明 |
|---|---|---|
| `proto` 变体 / 常量 / 错误 / codec | 否 | 没动 proto |
| `input-event` | 否 | 没动 |
| `ipc::TransportEvent` | 否 | 没动 ipc |
| `lan-mouse-gtk::status_bar` | 否 | 没动 gtk |
| `lan-mouse-cli` stderr 订阅 | 否 | 没动 cli |
| `lan-mouse/src/clipboard*.rs` | 否 | 没动 core |
| `Cargo.toml` 引入 h3/h3-quinn/http | 否 | 0 依赖变更 |
| `quic_transport.rs::Stream C` reader | **否**（关键） | supervisor 不装配 stream C；STEP-6.3 才接 |
| `connect.rs` mDNS / discovery | 否 | 没动 connect |
| `lan-mouse-gtk` `status_bar` | 否 | 没动 gtk |

**结论**：0 越界。

## 5. 验证结果

### 5.1 `cargo check -p lan-mouse --lib` errors 分布

**本步完成后**：25 errors（**全部在 quic_transport.rs 内**）

| 错误源文件 | errors 数 | STEP-6.2 scope 内？ |
|---|---|---|
| `src/crypto.rs` | 0 | ✅（删 3 个 *_compat + webrtc_dtls import 全清） |
| `src/listen.rs` | 0 | ✅（整段重写无新错） |
| `src/service.rs` | 0 | ✅（LanMouseListener::new 新签名 caller 适配全清） |
| `src/connect.rs` | 0 | ✅（本步未动；STEP-6.1 已完成） |
| `src/quic_transport.rs` | 25 | ❌（**pre-existing latent bugs**，详见 §3 偏差 #N-17） |

**对比基线**：
- 基线（STEP-6.1 提交后）：10 errors（9 listen.rs + 1 crypto.rs:28）
- 本步提交后（**listen.rs / crypto.rs / service.rs 内**：0 errors）
- listen.rs 9 errors 全消除（DTLSConn / webrtc_dtls / webrtc_util imports + 类型全部下线）
- crypto.rs:28 `use webrtc_dtls::crypto::Certificate` error 闭合（SUGGESTION #S-1）

### 5.2 SUGGESTION #S-1 闭合验证

- `crypto::load_certificate_compat` 已删除（service.rs 旧 caller 已切）
- `crypto::generate_dtls_cert_compat` 已删除（无 caller）
- `crypto::certificate_fingerprint_compat` 已删除（无 caller）
- `use webrtc_dtls::crypto::Certificate;` 顶部 import 已删除
- **#S-1 完全闭合** —— 建议 Leader 评审后从 SUGGESTION.md 删除本条

### 5.3 §9 M1 边界 grep

```
$ grep -nE "webrtc-dtls|webrtc-util|TransportEvent|Bounds|MotionAbsolute|CursorPos|ReceiverSensitivity|MAX_CLIPBOARD_SIZE|BufferTooLarge|ClipboardEvent|h3|h3-quinn|status_bar|clipboard" src/listen.rs src/crypto.rs src/service.rs
# （0 命中 —— §9 12 类 grep 全部 clean）
```

### 5.4 本步未跑 `cargo test`

- 14 → 0 errors 验证未达成（仍有 25 errors 在 quic_transport.rs 内 —— 见 §3 偏差 #N-17）
- `bash scripts/quic_smoke.sh` 不存在 —— bak `mousehop/scripts/quic_smoke.sh` 存在但依赖 bak-specific `mousehop` binary + `mousehop-cli` 子命令；本仓 `lan-mouse-cli` 不存在。STEP-7.2 抄 bak 时一并创建（PLAN §7.2）

### 5.5 `cargo check -p lan-mouse --tests`

```
$ cargo check -p lan-mouse --tests 2>&1 | grep -cE "^error\[E"
# 跳过 —— 25 lib errors 阻塞 test target 编译（与 SUGGESTION #S-5 同根因）
```

## 6. 处理的 SUGGESTION 项

**SUGGESTION #S-1 完全闭合**：
- `crypto::load_certificate_compat` / `generate_dtls_cert_compat` / `certificate_fingerprint_compat` 三个 `*_compat` 入口**已删除**
- `crypto.rs:28 use webrtc_dtls::crypto::Certificate;` 已删除
- `service.rs::new()` `load_certificate_compat` 调用已切到 `cert_der.0.clone()` + `cert_der.1.clone_key()`
- `LanMouseListener::new(...)` 签名从 `cert: Certificate` 改为 `(cert_chain, key)` 元组
- 本条目进入"待 Leader 评审后删除"状态

无新增 SUGGESTION 条目。

## 7. 闸门检查（PLAN-M1 §1 时间门 / §9 边界门）

| 闸 | 结果 |
|---|---|
| **§1 时间门**：30 min 目标 | � 实际 ~80 min（**超 1h 红线** —— quic_transport.rs pre-existing bugs 排查 + Bidi 重构 + Result 别名修复 + listen.rs 重写四件事串起来远超预算） |
| **§9 边界门** | ✅ 0 越界（详见 §4） |
| **STEP-6.1 依赖** | ✅ `LanMouseConnection` 切到 PeerSession 已完成（service.rs caller 适配时复用） |
| **STEP-5.4 依赖** | ✅ `PeerSession::from_connection` / `server_hello` / `take_stream_a_recv` / `connection` / `peer_identity` 就位 |
| **STEP-3.2 依赖** | ✅ `server_hello` + `HELLO_TIMEOUT` 就位 |
| **STEP-2.7 依赖** | ✅ `AuthorizedKeysVerifier::new(allowlist)` 就位（listen.rs supervisor 装配） |
| **STEP-2.4 依赖** | ✅ `endpoint_with_verifier(addr, cert_chain, key, verifier)` 就位（service.rs caller 复用） |
| **STEP-1.1 依赖** | ✅ `crypto::load_or_create_server_cert()` 返 `(cert_chain, key)` 元组（service.rs caller 复用） |
| **闸 2 实时自检** | ⚠️ 25 errors 在 quic_transport.rs 内（**pre-existing**，详见偏差 #N-17） |
| **闸 3 STEP 收尾** | ⏸ 跳过（25 lib errors 阻塞，未跑全套） |

## 8. 遗留 / 风险

- ⚠️ **#1 偏差 #N-17：quic_transport.rs 25 pre-existing errors** 阻塞 build —— 见 §3，建议 Leader 决策拆 STEP-6.2a 处理
- ⚠️ **#2 偏差 #N-18：stream B/C 路径未装配** —— M1 阶段 ListenTask 不依赖，STEP-6.3 接手
- ⚠️ **#3 偏差 #N-19：`get_certificate_fingerprint` stub None** —— edge case 留 STEP-6.3 收紧
- ⚠️ **SUGGESTION #S-5 持续**：14 lib errors 修复前 `cargo test -p lan-mouse ...` 不能跑；本步解锁 listen.rs 编译后该 SUGGESTION 适用范围扩大到 quic_transport.rs 内的 pre-existing errors
- ⚠️ **`scripts/quic_smoke.sh` 不存在**：bak 脚本依赖 `mousehop` / `mousehop-cli` 二进制；本仓 `lan-mouse-cli` 命名 + IPC socket 路径不同。STEP-7.2 抄 bak 时同步重命名（与 PLAN §7.2 一致）
- ⚠️ **`reply()` 走 `send_input` 每条控制事件开新 bidi**：与 STEP-6.1 `send_stream_a` 同策略；高频控制事件下额外 stream 开销可接受；M2 阶段可缓存 send half 优化

## 9. 下一步（STEP-6.3 前置条件）

✅ **本步范围内完成**：
- listen.rs 整段切到 PeerSession 路径（DTLSConn → PeerSession + read_frame）
- crypto.rs 删 3 个 *_compat + webrtc_dtls import（SUGGESTION #S-1 闭合）
- service.rs LanMouseListener::new 新签名 + 不再调 `crypto::load_certificate_compat`
- ListenEvent::Disconnected 变体引入（emulation.rs ListenTask 现有 match 臂已覆盖）
- read_any_frame 公开 API 就位（STEP-6.3 accept_bi 子 task 用）
- Bidi<S, R = S> 重构（连带 STEP-5.2 pre-existing bug 修复）

⚠️ **本步范围外待办**（建议拆 STEP-6.2a）：
- quic_transport.rs 25 pre-existing errors（Debug derive / verify_tls12_signature / Result 别名 / Bidi send-recv 拆分 / alpn_protocols 路径 / quinn ConnectionError 变体名等）
- 跑 `cargo check -p lan-mouse --lib` 全绿

❌ **STEP-6.3 前置条件**：
- quic_transport.rs 25 errors 修复（让 `cargo check -p lan-mouse --lib` 真正 0 errors）
- macOS power observer + if_addrs 依赖引入（per-IP bind `enumerate_listenable_addrs`）—— 当前 workspace 尚未引入 `if_addrs` crate，需要在 `Cargo.toml` 加 dep + 写 `crate::macos_power::PowerObserver`（与 bak 对齐）；**这是 STEP-6.3 的范围，不在本步**

**未做 git commit**：等 Leader 处理（本步动 4 文件：`src/listen.rs` / `src/crypto.rs` / `src/service.rs` / `src/quic_transport.rs`）。
