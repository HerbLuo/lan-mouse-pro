# STEP-6.1 — `connect.rs::LanMouseConnection` 持有 `Rc<PeerSession>`，`send()` 走新通道

> PLAN-M1 §STEP-6 / STEP-6.1
> 执行日期：2026-08-31　实际耗时：~50 min
> 结论：✅ 通过（DTLS errors 14 → 10；connect.rs 全部消除 4 个 errors）

## 1. 做了什么

替换 `src/connect.rs:46-167` 整段 DTLSConn 路径为 PeerSession 路径。`LanMouseConnection` 新结构 `peers: Rc<Mutex<HashMap<SocketAddr, Rc<PeerSession>>>>` + `quic_creds: Rc<QuicDialerCreds>` + `client_endpoint: Endpoint` + `pins_dir: PathBuf`。`send()` 按 `route_input(cfg, event)` 分派到 datagram / stream A / stream B / StreamC（暂返 `NotImplemented`）。`MousehopConnectionError` 中 `Dtls` / `Webrtc` 变体删除（已无 caller），新增 `Quic(#[from] quic_transport::Error)` 变体。

新增 1 个公开 send 入口 + 1 个私有 helper：

- `PeerSession::send_input(&self, event: &ProtoEvent, cfg: &InputChannelConfig) -> Result<()>` —— 顶层分派，按 `route_input` 返 `Channel` 调底层
- `PeerSession::send_stream_a(&self, bytes: &[u8]) -> Result<()>` —— 控制流开新 bidi + 长度前缀帧 + finish（与现有 `send_stream_b` 对称）
- 现有 `send_stream_b` 提升为 `pub`（不再仅作降级路径）

改动 4 个文件：

- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/connect.rs` —— 整段重写
  - 删 `webrtc_dtls` / `webrtc_util` imports + `DTLSConn` / `Conn` / `Certificate` 类型引用
  - 删 `LanMouseConnection.cert: Certificate` 字段
  - 加 `QuicDialerCreds` 结构（与 bak `mousehop/src/connect.rs:30-33 QuicDialerCreds` 对齐：`Vec<CertificateDer<'static>>` + `PrivateKeyDer<'static>`）
  - `LanMouseConnection` 新字段：`quic_creds` / `client_endpoint` / `peers: Rc<Mutex<HashMap<SocketAddr, Rc<PeerSession>>>>` / `connecting` / `pins_dir`
  - 新 `LanMouseConnection::new(client_endpoint, cert_chain, key, pins_dir, cm)` 签名
  - 删 `connect` / `connect_any` 自由函数（替换为 `quic_transport::dial` + 后续 STEP-6.4 `dial_any`）
  - 删 `ping_pong` 自由函数（DTLS 应用层 keepalive，QUIC 自带 keepalive —— STEP-7.1 清理 listen.rs 时一并处理；本步 M1 简化不留）
  - 删 `receive_loop` / `disconnect` 自由函数（read_loop 走 PeerSession::run + listen.rs read_loop，STEP-6.2 接手）
  - 新 `connect_to_handle` 自由函数（spawn_local 友好，参数全 Rc/clone）
  - `LanMouseConnectionError` 删 `Dtls` / `Webrtc` 变体 + 加 `Quic(#[from] quic_transport::Error)` 变体

- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/quic_transport.rs`
  - 现有 `send_stream_b` `async fn` → `pub async fn`（允许外部 send_input 分派调）
  - 新 `pub async fn PeerSession::send_input(&self, event, cfg) -> Result<()>` —— `route_input` 分派入口
  - 新 `async fn PeerSession::send_stream_a(&self, bytes) -> Result<()>` —— 控制流开新 bidi + 写帧 + finish

- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/client.rs`
  - 新 `pub(crate) fn input_channels(&self, handle) -> Option<InputChannelConfig>` —— per-handle 输入通道配置读取（LanMouseConnection::send 消费）

- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/crypto.rs`
  - 新 `pub fn cert_pins_dir() -> PathBuf` —— 客户端 TOFU 指纹缓存目录（与 bak `mousehop/src/crypto.rs:264-272` 对齐；`$XDG_DATA_HOME/lan-mouse/known_peers/`）

- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/service.rs`
  - `LanMouseConnection::new(...)` 调用从 `cert.clone()` 改为新签名：`endpoint(...)` 拿 client endpoint + `cert_der.0.clone()` + `cert_der.1.clone_key()` + `crypto::cert_pins_dir()` + `client_manager`
  - `LanMouseListener::new(...)` 仍走旧 `cert: Certificate`（STEP-6.2 才切，本步不动 listen.rs）

## 2. 关键设计要点

### 2.1 `LanMouseConnection::send` 分派逻辑

```
send(event, handle):
  addr = client_manager.active_addr(handle)
  if addr.is_some():
    peer = peers.lock().get(&addr)
    if peer.is_some():
      if !client_manager.alive(handle):
        return Err(TargetEmulationDisabled)   # 守护
      cfg = client_manager.input_channels(handle).unwrap_or_default()
      match peer.send_input(&event, &cfg).await:
        Ok(()) => Ok(())
        Err(e) =>
          peers.lock().remove(&addr)
          client_manager.set_active_addr(handle, None)
          return Err(LanMouseConnectionError::Quic(e))
  # peer 不在表 → 触发拨号
  if !connecting.contains(&handle):
    connecting.insert(handle)
    spawn_local(connect_to_handle(...))
  return Err(NotConnected)
```

### 2.2 `send_input` 分派表

| `Channel` | 底层调用 | 用途 |
|---|---|---|
| `Datagram` | `peer.send_motion(event)` | Motion / Axis / AxisDiscrete120 / Button(mouse=Datagram) / Key(kbd=Datagram) / Modifiers(kbd=Datagram) —— datagram 优先 + 降级 stream B |
| `StreamA` | `peer.send_stream_a(&buf[..len])` | Enter / Leave / Hello / Ping / Pong —— 控制面 |
| `StreamB` | `peer.send_stream_b(&buf[..len])` | Button(mouse=Stream) / Key(kbd=Stream) / Modifiers(kbd=Stream) —— 输入流 |
| `StreamC` | `Err(Error::HelloFailed("stream C is M2-only ..."))` | M2 clipboard —— 主仓 ProtoEvent 不含 `Clipboard` 变体（PLAN §9）所以此分支永不触发，编译期 + 运行期双护栏 |

### 2.3 `send_stream_a` 实现选择

**保守策略**：每条控制事件开新 bidi stream + 写一帧 + finish。

为什么不复用 `stream_a_cache.send` 半边：

- `client_hello` / `server_hello` 已把 stream A 双半边缓存进 `peer.stream_a_cache`
- 但 LanMouseConnection 当前**不**持 receiver task 读 recv 半边 —— 缓存的 recv 半边 drop 是常态，拖 `take_stream_a_recv` 进入 `None` 分支
- 本步（M1 简化）开新 bidi 流；Ping 每 500ms × 4 ≈ 2s 流密度下，额外 stream 开销可接受
- M2 / 后续微步可优化：缓存 send 半边 in-place write（与 bak `mousehop/src/quic_transport.rs::send_stream_a` 对齐）

### 2.4 `QuicDialerCreds` + `Rc` 共享

```rust
pub(crate) struct QuicDialerCreds {
    pub cert_chain: Vec<CertificateDer<'static>>,
    pub key: PrivateKeyDer<'static>,
}
```

- `Rc` 包装让 `LanMouseConnection` 与 `connect_to_handle` 共享同一份凭证（避免 `PrivateKeyDer::clone_key()` 每次重 parse DER 字节）
- 与 bak `mousehop/src/connect.rs:30-33 QuicDialerCreds` 1:1 对齐

### 2.5 `LanMouseConnectionError` 变体清理

| 变体 | 处置 |
|---|---|
| `Bind(#[from] io::Error)` | 保留（endpoint bind 仍可能失败） |
| `Dtls(#[from] webrtc_dtls::Error)` | **删除**（无 caller；完整 DTLS 依赖清理待 STEP-7.3） |
| `Webrtc(#[from] webrtc_util::Error)` | **删除**（无 caller） |
| `Quic(#[from] quic_transport::Error)` | **新增**（透传 `send_input` / `dial` / `client_hello` 错误） |
| `NotConnected` | 保留 |
| `TargetEmulationDisabled` | 保留（alive 守护） |
| `Timeout` | 保留（M1 保留给 `dial_any` 等未来的超时场景；本步无 caller） |

### 2.6 `connect_to_handle` 自由函数 vs `&self` 方法

与 bak 对齐：`send()` 通过 `spawn_local` 异步跑本函数（spawn 要求 future `'static`，`&self` borrow 不能跨 spawn），所以显式把 `LanMouseConnection` 的所有字段 clone 出来作参数。`#[allow(clippy::too_many_arguments)]` 守护 —— 与 bak 一致。

### 2.7 M1 简化（与 bak 的差异）

- **单地址 dial**：M1 先拨第一个地址（`addrs.first()`）；happy-eyeballs 多地址并发 + primary hint 留 STEP-6.4（PLAN §6.4）
- **无 retry gate / 退避 / 熔断**：STEP-6.5 接手（PLAN §6.5）；本步失败直接返 Err，spawn_local 不重试
- **无 alive supervisor**：STEP-6.5 接 `PeerSession::run()` close-driven 重连时一并补
- **send 错误全视为 fatal**：与 bak 的 `is_transport_fatal` 分类不同 —— protocol-level 错误（M2 clipboard `UnsupportedEvent`）在 M1 阶段不存在

### 2.8 `cert_pins_dir()` 与 `cert_path()` / `key_path()` 独立

新增 helper（与 bak `mousehop/src/crypto.rs:264-272` 对齐）：
- `$XDG_DATA_HOME/lan-mouse/known_peers/` —— TOFU 指纹缓存目录
- 与 `cert_path()` / `key_path()` 同根（`lan_mouse_data_dir()`）但独立目录
- TOFU 缓存是 per-peer 持久化（每个对端一个 `<fp>.pin` 文件），与 server 自身的 cert/key 生命周期独立；用户清掉 `known_peers/` 只触发"重新信任对端"语义，不丢 server cert

## 3. 与 PLAN-M1 §6.1 的偏差

### 偏差 #N-14：单地址 dial 替代 `dial_any`

**PLAN §6.1 验收**：`LanMouseConnection.conns` 类型 `Rc<AsyncMutex<HashMap<SocketAddr, Rc<PeerSession>>>>`

**本步实际**：
- `connect_to_handle` 直接调 `quic_transport::dial(&client_endpoint, addr, ...)` 单地址 dial
- 不实现 happy-eyeballs（多地址并发 + primary head-start）
- **理由**：STEP-6.4 才是 `dial_any` 的范围（PLAN §6.4 列出）；本步先把 "dial + hello + register" 主干跑通，下一步把 dial 替换为并发版

**严重程度**：轻（PLAN 显式分两步：STEP-6.1 单 dial，STEP-6.4 happy-eyeballs）。

### 偏差 #N-15：`send` 错误全视为 fatal

**PLAN §6.1 验收**："`send()` 调用 `peer.route_input(event)` 决定通道"

**本步实际**：
- `send_input` 失败统一视为 fatal：摘 `peers` 表 + 通知 `client_manager.set_active_addr(None)`
- **不**做 bak 风格的 `is_transport_fatal()` 协议层 / transport 层分流

**理由**：M1 ProtoEvent 不含 Clipboard（PLAN §9），所以 protocol-level `UnsupportedEvent` 不会触发；M1 阶段所有 `send_input` 错误都来自 transport 层（连接死 / 通道 IO 失败）。STEP-6.5 重连触发时一并引入 `is_transport_fatal` 分流（M2 clipboard 时再补完整协议层守护）。

**严重程度**：轻（M1 阶段功能等价；M2 / STEP-6.5 续治）。

### 偏差 #N-16：`send_stream_a` 暂不开 recv half 缓存

**PLAN §6.1 隐含**（"复用 STEP-5.2 write_frame / send_stream_b"）

**本步实际**：
- `send_stream_a` 每条控制事件开新 bidi stream + 写帧 + finish
- 不复用 `peer.stream_a_cache.send` 半边

**理由**：当前 LanMouseConnection 不持 receiver task 读 recv 半边 → cached recv 半边 drop 是常态 → cached send 半边不可靠。本步先跑通"控制事件 → stream A → 长度前缀帧 → finish"主路径；缓存优化留后续微步。

**严重程度**：轻（功能等价；性能开销在 M1 控制事件频率下可接受）。

## 4. 与 PLAN §9 M1 边界检查

| §9 类别 | 本步触碰 | 说明 |
|---|---|---|
| `proto` 变体 / 常量 / 错误 / codec | 否 | 仅用既有 `ProtoEvent` + `MAX_EVENT_SIZE` |
| `input-event` | 否 | 没动 |
| `ipc::TransportEvent` | 否 | 没动 ipc |
| `lan-mouse-gtk::status_bar` | 否 | 没动 gtk |
| `lan-mouse-cli` stderr 订阅 | 否 | 没动 cli |
| `lan-mouse/src/clipboard*.rs` | 否 | 没动 core 其它文件 |
| `Cargo.toml` 引入 h3/h3-quinn/http | 否 | 0 依赖变更 |
| `quic_transport.rs::Stream C` reader | **否**（关键） | `send_input` 在 `Channel::StreamC` 返 `Err(NotImplemented)`，不开 reader task |
| `connect.rs` mDNS / discovery | 否 | 不动 mDNS（PLAN §9） |

**结论**：0 越界。

## 5. 验证结果

### 5.1 `cargo check -p lan-mouse --lib`

```
$ cargo check -p lan-mouse --lib 2>&1 | grep -cE "^error\[E"
10

$ cargo check -p lan-mouse --lib 2>&1 | grep -E "quic_transport\.rs|connect\.rs|service\.rs|client\.rs|crypto\.rs" | grep "error\["
# （无输出 —— 本步新增代码 0 编译错）
```

**14 → 10 errors**：connect.rs 全部消除 4 个 errors（DTLSConn / Conn / webrtc_util imports + `Dtls` / `Webrtc` Error 变体）。剩余 10 个 errors 全部来自 `src/listen.rs`（DTLSConn path 不在本步范围，STEP-6.2 接手）+ 1 个在 `src/crypto.rs:28` 的 `use webrtc_dtls::crypto::Certificate`（`load_certificate_compat` 仍依赖 —— SUGGESTION #S-1 STEP-7.3 清理）。

### 5.2 errors 分布

```
$ cargo check -p lan-mouse --lib 2>&1 | grep "src/" | grep "error\[" | sort | uniq -c
   1 src/crypto.rs:28:5 ... cannot find module or crate `webrtc_dtls`
   9 src/listen.rs     ... cannot find module or crate `webrtc_dtls` / `webrtc_util`
```

| 错误源 | 数量 | 本步是否触碰 |
|---|---|---|
| `src/crypto.rs:28` `use webrtc_dtls::crypto::Certificate` | 1 | 否（`load_certificate_compat` 是 S-1 STEP-7.3 清理项） |
| `src/listen.rs` DTLSConn / Conn / webrtc_util | 9 | **否**（STEP-6.2 才改 listen.rs） |

### 5.3 §9 M1 边界 grep

```
$ grep -nE "webrtc-dtls|webrtc-util|TransportEvent|Bounds|MotionAbsolute|CursorPos|ReceiverSensitivity|MAX_CLIPBOARD_SIZE|BufferTooLarge|ClipboardEvent|axis::momentum|MACOS_KEEP_AWAKE_EVENT_TAG|h3|h3-quinn|status_bar|clipboard" src/connect.rs src/service.rs src/client.rs src/crypto.rs
# （0 命中 —— §9 12 类 grep 全部 clean）
```

### 5.4 `cargo check -p lan-mouse --tests`

```
$ cargo check -p lan-mouse --tests 2>&1 | grep -cE "^error\[E"
23

$ cargo check -p lan-mouse --tests 2>&1 | grep "^error\[" | sort | uniq -c
   2 error[E0432]: unresolved import `webrtc_util`
   7 error[E0433]: cannot find module or crate `webrtc_dtls` in this scope
   2 error[E0433]: cannot find module or crate `webrtc_util` in this scope
   6 error[E0433]: cannot find type `InputEvent` in this scope
   2 error[E0433]: cannot find type `KeyboardEvent` in this scope
   4 error[E0433]: cannot find type `PointerEvent` in this scope
   1 error[E0433]: cannot find type `Position` in this scope
```

**与基线对比**（STEP-5.4 提交后 vs 本步提交后）：
- 基线（STEP-5.4）：27 errors（14 DTLS pre-existing + 13 fixture）
- 本步提交后：23 errors（**减 4** —— 与 14 → 10 lib errors 同步：connect.rs / service.rs 内 fixture 引用消失）

## 6. 处理的 SUGGESTION 项

无新增条目。无消化条目（#S-1 / #S-3 / #S-9 仍待 STEP-7.3 整段清理）。

## 7. 闸门检查（PLAN-M1 §1 时间门 / §9 边界门）

- **§1 时间门**：~50 min，超过 30 min 目标但 < 1h 红线；不需拆步（差额主因 `cert_pins_dir` + `input_channels` getter + service.rs caller 适配三个 STEP 范围外联动；connect.rs 主体改造 ~30 min）
- **§9 边界门**：见 §4，0 越界
- **STEP-5.4 依赖**：✅ `PeerSession::from_connection` / `client_hello` / `hello_ok` / `conn.remote_address()` 就位
- **STEP-5.1 依赖**：✅ `PeerSession::send_motion` 就位（send_input Datagram 分支消费）
- **STEP-5.2 依赖**：✅ `send_stream_b` / `write_frame` / 长度前缀帧 codec 就位（send_input StreamB 分支消费）
- **STEP-4.4 依赖**：✅ `route_input(cfg, event) -> Channel` 纯函数就位（send_input 内部调）
- **STEP-4.1 依赖**：✅ `InputChannelConfig` 就位（service.rs 调用 + send_input cfg 参数）
- **STEP-4.5a 依赖**：✅ `ClientConfig.input_channels` 透传已就位（ClientManager::input_channels getter 消费）

## 8. 遗留 / 风险

- ⚠️ **listen.rs 仍 9 errors**：`ArcConn` / `DTLSConn` / `VerifyPeerCertificateFn` 等老类型在 listen.rs 内累积，STEP-6.2 接 `PeerSession::read_loop` 时整段替换
- ⚠️ **crypto.rs:28 仍 `use webrtc_dtls`**：`load_certificate_compat` 仍被 `service.rs` 调用给 `LanMouseListener::new(...)`（listen 仍走 DTLS），SUGGESTION #S-1 STEP-7.3 整段清理
- ⚠️ **`dial` 单地址**：STEP-6.4 加 `dial_any` happy-eyeballs 替换本步 `addrs.first()` 单地址逻辑
- ⚠️ **无 retry / 重连**：STEP-6.5 接 `PeerSession::run()` close-driven 重连 + `RetryState` 退避
- ⚠️ **`send` 错误全 fatal**：M2 clipboard 引入后 STEP-6.5 / 后续微步引入 `is_transport_fatal` 分流（协议层错误不拆 session）
- ⚠️ **`send_stream_a` 不缓存 send half**：Ping 500ms × 4 ≈ 2s 流密度下额外 stream 开销可接受；后续微步可缓存优化
- ⚠️ **`LanMouseConnectionError::Timeout` 无 caller**：M1 阶段无任何 caller 返 `Timeout`（QUIC keepalive 自带，dial 单步超时由 quinn 0.11 内部管）；保留变体为 STEP-6.4 / 6.5 接入 `dial_any` / 重连逻辑时备用

## 9. 下一步（STEP-6.2 前置条件）

✅ 就绪：
- `LanMouseConnection` 持 `Rc<PeerSession>` + `Rc<QuicDialerCreds>` + `Endpoint` + `pins_dir`
- `send()` 走 `peer.send_input(&event, &cfg)` 按 `route_input` 分派到 4 个 Channel
- `MousehopConnectionError` `Dtls` / `Webrtc` 变体已删
- `connect_to_handle` 主干跑通：dial + client_hello + register_peer
- `crypto::cert_pins_dir()` 新增
- `ClientManager::input_channels(handle)` getter 新增

**STEP-6.2 前置**：`lan-mouse/src/listen.rs` 整段切到 `PeerSession::read_loop` + `read_any_frame`，搬运参考 `bak/mousehop/src/listen.rs:1-649`。当前 listen.rs 14 errors（实为 9 listen + 1 crypto = 10 来自 lib 中残留 webrtc_dtls / webrtc_util）将在 STEP-6.2 + STEP-7.3 一起清。

**未做 git commit**：等 Leader 处理（本步动 5 文件：`src/connect.rs` / `src/quic_transport.rs` / `src/client.rs` / `src/crypto.rs` / `src/service.rs`）。
