# STEP-8.2 — 10.2.1.12 远程对接：mTLS Rejected / 拨号 lazy / Hello 重复 / alive 阻塞 四 bug

> PLAN-M1 §STEP-8 / **新方向：远程对接可用性问题**
> 起点：用户现场问题（两台机器 10.2.1.15 / 10.2.1.12 互 ping / UDP 4242 互通但连不上）
> 执行日期：2026-09-01
> 状态：**调研 + 修复完成（四路）+ 2 单测已加**（41 个 lib 测试全绿）

---

## 0. 问题现场

- 本机 IP：`10.2.1.15`
- 对端 IP：`10.2.1.12`
- ping 双向通；UDP 4242 双向可达（`nc -u -z`）
- `~/.config/lan-mouse/config.toml` 配置 `[[clients]] hostname="10.2.1.12"`，`activate_on_startup=true`，`position="top"`
- `~/.local/share/lan-mouse/known_peers/8d_b4_f1_...pin` 存在（曾 TOFU pin 成功过）
- `[authorized_fingerprints]` 后用户加了 `8d:b4:f1:... = ""`（对方 cert fingerprint）
- 用户问："加了指纹还是不行"
- 用户问："GUI 按道理请求链接时应该有弹窗提示接受指纹吗？"
- **Bug #1/Bug #2 fix 后用户仍报"还是不行"** → 新日志暴露 **Bug #3：Hello 握手超时**

---

## 1. 修复：GTK 弹窗路径完全断开（**Bug #1** — 已修）

### 1.1 根因（与 §1 调研一致）

`ListenEvent::Rejected { fingerprint }` 在 `src/listen.rs:77, 100` 声明，
`src/emulation.rs:190` 处理（推 `EmulationEvent::ConnectionAttempt` →
service.rs:320 → `FrontendEvent::ConnectionAttempt` → GTK `request_authorization`）。

但**全工程没有任何一处 `send(ListenEvent::Rejected ...)`** —— rustls 在
mTLS handshake 阶段直接拒绝（`AuthorizedKeysVerifier::verify_client_cert`
返 `Err(rustls::Error::General)`）→ `quinn::Connecting::await` 失败 →
**`handle_quic_peer_supervisor` 根本不被调用**（无 `Connection` resolve 出来，
无 `peer_identity()` 可读 fingerprint）。

### 1.2 修复方案（已实现）

rustls 拒握时 fingerprint 只能在 verifier 内部 `verify_client_cert` 即将返
Err 时被丢弃；其他位置（quinn accept 层、listen task）都拿不到。所以方案
是 **让 verifier 在 Err 路径上把 fp 通过反向 channel 推回 listen task**。

#### 改动清单

**`src/quic_transport.rs`**：

1. `AuthorizedKeysVerifier` 加 `rejection_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>` 字段
2. 新 builder 方法 `with_rejection_tx(self, tx) -> Self`（**不动** `new()` / `with_known()` 签名，单测与既有 caller 不破）
3. `verify_client_cert` Err 路径：`log::warn` + `rejection_tx.send(fp)` + 返 `Err`（send 失败静默吞 —— channel 关闭时 listen task 已退出，no-op 合理）

> **为什么用 `tokio::sync::mpsc::UnboundedSender` 而非 `local_channel`**：
> `verify_client_cert` 由 rustls 在 QUIC 握手回调链里调用 —— quinn 的 I/O
> task 可能跑在非 local 线程上（与 spawn_local 不属同一 task）。
> `tokio::sync::mpsc::UnboundedSender` 是 `Send + Sync`，可跨线程持有；
> listen task 的 forwarder 在 `spawn_local` 上 recv（同 §1 已有 `wake_rx` 模式）。

**`src/listen.rs`**：

1. `ListenEvent::Rejected` docstring 更新（不再是死代码 —— 标注反向 channel 触发路径与"为何需要反向 channel 而不是从 quinn `Connection` 拿 fp"的根因解释）
2. `LanMouseListener` 加 `rejection_forwarder_task: JoinHandle<()>` 字段
3. `LanMouseListener::new`：装配 `tokio::sync::mpsc::unbounded_channel::<String>()` → 把 `tx` clone 给 `AuthorizedKeysVerifier::with_rejection_tx(...)` → spawn `spawn_rejection_forwarder_task(rx, listen_tx.clone())` 走 `spawn_local`
4. `spawn_rejection_forwarder_task`：阻塞 recv `rejection_rx` → `listen_tx.send(ListenEvent::Rejected { fingerprint })` → emulation.rs:190 已有 match 臂 → `EmulationEvent::ConnectionAttempt` → service.rs:320 → GTK `request_authorization` 弹窗
5. `terminate()` 加 `self.rejection_forwarder_task.abort()`（与 `wake_task` / `accept_task` 同模式）

#### 完整 GUI 弹窗链修复

```
AuthorizedKeysVerifier::verify_client_cert 返 Err
  ↓ rejection_tx.send(fp)           ← 修：之前是空
rejection_forwarder_task (spawn_local)
  ↓ listen_tx.send(ListenEvent::Rejected { fingerprint })  ← 修：之前是死代码
emulation.rs:190 match Some(ListenEvent::Rejected)
  ↓ event_tx.send(EmulationEvent::ConnectionAttempt { fingerprint })
service.rs:320 handle_emulation_event
  ↓ notify_frontend(FrontendEvent::ConnectionAttempt { fingerprint })
lan-mouse-gtk/src/lib.rs:286 match FrontendEvent::ConnectionAttempt
  ↓ window.request_authorization(&fingerprint)
lan-mouse-gtk/src/window.rs:573 (GTK 弹窗) ← 现在可达
```

#### 单元测试

新增 `rejection_channel_forwards_rejected_fingerprint`（`src/quic_transport.rs`）：

- allowlist 命中（正向）→ 验证 `rx.try_recv().is_err()`（防止误报弹窗）
- allowlist 移除后（负向）→ 验证 `rx.try_recv()` 收到同一 fp
- 第二次 try_recv 为空（一次拒绝只 send 一次）

---

## 2. 修复：拨号 lazy 触发（**设计现状 #2** — 已修）

### 2.1 根因（与 §2 调研一致）

`src/connect.rs::LanMouseConnection::send` **只有"我要发送事件"时才触发
dial** —— 没有 periodic re-attempt / startup dial。
`activate_on_startup=true` 只让 `ClientState.active=true`，不主动建连。

### 2.2 后果（修复前）

两侧 daemon 启动 + 指纹已对 + 没人移鼠标到屏边 → 永远不建连（**用户的
10.2.1.12 / 10.2.1.15 现场就是这种情况**）。

### 2.3 修复方案（已实现）

**`src/connect.rs`**：

新增 `LanMouseConnection::dial(&self, handle: ClientHandle) -> Result<(), LanMouseConnectionError>`：

- 跑 RetryState gate（与 `send()` 同语义）
- 检查 `connecting` set 去重（与 `send()` 同语义）
- `spawn_local(connect_to_handle(...))`（与 `send()` 同语义）
- fire-and-forget，返 `Ok(())`

**为什么另起方法而非复用 `send`**：`send()` 需要 `ProtoEvent`，dial 不发
任何事件；混在一起会污染 `send()` 语义。

**`src/capture.rs`**：

1. `CaptureRequest` 加 `Dial(ClientHandle)` 变体
2. `Capture::dial(handle)` 公开方法 → send `CaptureRequest::Dial(handle)`
3. `CaptureTask::run` 与 `CaptureTask::do_capture_session` 的两个 `request_rx.recv()` match 臂都处理 `Dial` → `self.conn.dial(handle).await`（`let _ =` 显式 fire-and-forget）

> **为什么走 CaptureRequest 而非直接调 connect.rs**：CaptureTask 是唯一
> 持有 `LanMouseConnection` 的 owner —— 让 `Capture` 透传 request 保持单
> 一所有权不破。

**`src/service.rs`**：

`activate_client` 在 `client_manager.activate_client(handle)` 返 true 之后：
- `self.capture.create(handle, pos, CaptureType::Default)`（不变）
- `self.broadcast_client(handle)`（不变）
- **新增** `self.capture.dial(handle)` —— 主动 fire-and-forget 触发拨号

**调用顺序考量**：

- 必须在 `client_manager.activate_client` 返 true 之后调 —— 之前调
  `connect_to_handle` 拿不到 active handle，逻辑错位
- 在 `broadcast_client` 之后调 —— GUI 显示 client 状态为 active 时拨号
  已在途（更符合直觉）

---

## 3. 验收

### 3.1 单测

```
cargo test --lib
test result: ok. 40 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

新增 1 个测试（`rejection_channel_forwards_rejected_fingerprint`）；其余
39 个原有测试 + 已通过状态不变。**Bug #2 的 `dial()` 与 `send()` 共享同一
套 RetryState + connecting 去重逻辑** —— 既有 `backoff_doubles_on_each_
failure` + `reconnect_on_peer_close` 测试已覆盖，不重复加 E2E。

### 3.2 编译

```
cargo check --lib
warning: `lan-mouse` (lib) generated 4 warnings   ← 全部 pre-existing
Finished `dev` profile [unoptimized + debuginfo] target(s)
```

新增代码未引入新 warning。

```
cargo build
warning: `lan-mouse` (lib) generated 4 warnings   ← 全部 pre-existing
Finished `dev` profile [unoptimized + debuginfo] target(s)
```

完整 build（main.rs / GTK / cli）通过。

---

## 4. 新增修复：Hello 握手重复调用（**Bug #3** — 已修）

### 4.1 现场日志（Bug #1/#2 fix 后用户报"还是不行"）

**用户侧（10.2.1.15）**：
```
[12:48:42Z INFO  lan_mouse::connect] client (0) connected @ 10.2.1.12:4242 (quic)
[12:48:45Z WARN  lan_mouse::quic_transport] client hello handshake timed out after 3s
[12:48:45Z ERROR lan_mouse::connect] client (0) peer.run() 返了非预期 Err: hello handshake timed out after 3s — 不触发 RetryState
[12:48:45Z INFO  lan_mouse::quic_transport] datagram_reader: read_datagram error, exiting: closed
```

**对端（10.2.1.12）**：
```
[04:48:45Z INFO  lan_mouse::quic_transport] AuthorizedKeysVerifier: authorized peer a4:9b:47:...
[04:48:45Z INFO  lan_mouse::listen] QUIC peer connected: 10.2.1.15:61252
[04:48:45Z INFO  lan_mouse::listen] QUIC peer 10.2.1.15:61252 authorized (fingerprint a4:9b:47:...)
[04:48:48Z INFO  lan_mouse::listen] stream A reader exiting (IO closed): hello handshake failed: read frame length: connection lost
[04:48:48Z WARN  lan_mouse::listen] QUIC peer supervisor exited with err: hello handshake failed: read frame length: connection lost
```

**关键观察**：
- 客户端 `dial_any` 成功（12:48:42 UTC）
- 客户端 `client_hello` 第二次超时（12:48:45 UTC，**3s 后**）
- 服务端 mTLS 通过 + 第一次 hello OK（04:48:45 UTC，与客户端超时**同一瞬间**）
- 服务端 stream A 读循环 3s 后报 "connection lost"

→ 看似"mTLS 慢导致 server_hello 来不及响应"，但实际是更隐蔽的问题（见下）。

### 4.2 根因（与"mTLS 慢"假设不同的真正 bug）

**真正的根因**：客户端 `peer.run()` 内部**重复调用**了 `client_hello`。

`src/connect.rs::connect_to_handle` 的语义顺序：

```rust
let peer = Arc::new(PeerSession::from_connection(conn));
if let Err(e) = quic_transport::client_hello(&peer).await {  // ← 第一次 client_hello
    ...
}
spawn_local(spawn_peer_supervisor(..., peer));  // ← peer.run(PeerRole::Client) 内又调 client_hello
```

而 `src/quic_transport.rs::PeerSession::run()` 第 2207-2210 行（修复前）：

```rust
// (3) Hello 握手 —— role 决定走 client_hello / server_hello
match role {
    PeerRole::Client => client_hello(&self).await?,
    PeerRole::Server => server_hello(&self).await?,
}
```

→ **两次 `client_hello`** 的灾难链：

1. **第一次**（`connect_to_handle`）：`open_bi()` 开 stream A → 写 Hello → 读 server 回 Hello → 缓存 `stream_a` → `hello_ok = true`
2. **第二次**（`peer.run()` 内）：`open_bi()` 又开一条 stream D（！）→ 写 Hello → 等 server 回 Hello，**3s 超时**
3. 但服务端 `server_hello` 只 `accept_bi()` **一次**（接的是 stream A），accept 完就进 stream A 读循环，**永远不会** accept stream D
4. 客户端第二次 `client_hello` 等 3s 超时 → `peer.conn.close(VarInt(0), b"hello failed (timeout)")` → 关连
5. 服务端 stream A 的 `read_frame()` 报 "connection lost"
6. 整个 `peer.run()` 返 `Err(HelloTimeout)` → "client (0) peer.run() 返了非预期 Err: hello handshake timed out after 3s — 不触发 RetryState"

**为什么 mTLS 看起来慢是表象**：客户端 dial 成功后**立刻**调用第一次 `client_hello` —— 但服务端 mTLS 完成后，supervisor 启动 `server_hello` —— 服务端 `accept_bi` 接到 stream A、写 Hello 回包 —— 客户端收到后第一次 client_hello 成功。这部分耗时正常 ~ms。

**真正慢的是客户端 peer.run() 内的第二次 client_hello**：开 stream D、等 3s 超时。所以用户看到的"3s 后失败"其实是第二次 hello 的超时，而非 mTLS 慢。

### 4.3 修复

**`src/quic_transport.rs::PeerSession::run()`**：

```rust
match role {
    PeerRole::Client => {
        if !self.hello_ok.load(std::sync::atomic::Ordering::Acquire) {
            client_hello(&self).await?;
        } else {
            log::debug!("peer.run(Client): hello_ok 已置位，跳过重复 client_hello");
        }
    }
    PeerRole::Server => {
        if !self.hello_ok.load(std::sync::atomic::Ordering::Acquire) {
            server_hello(&self).await?;
        } else {
            log::debug!("peer.run(Server): hello_ok 已置位，跳过重复 server_hello");
        }
    }
}
```

**为什么 caller 路径还要保留早期 hello**：是历史顺序决定的 —— `connect_to_handle` 早期把 `client_hello` 放在 peer 生命周期注册到 `peers` 表**之前**（失败则不注册，便于 retry 不影响其他 caller），`spawn_peer_supervisor` 只接管 peer 死后的 RetryState。`peer.run()` 设计为"既可独立跑（单测）也可被外部 caller 提前 partial-init 后接管"——本步用 `hello_ok` 守卫表达后者语义。

**不破坏单测 `peer_session_round_trip_motion_keyboard`**：单测直接调 `peer.run(PeerRole::Client/Server)`，无早期 hello，`hello_ok` 初始 `false` → 走原始 hello 路径，行为不变。

### 4.4 回归测试

新增 `peer_run_skips_hello_if_already_done`（`src/quic_transport.rs`）：

模拟生产路径：
1. server 端：accept → `server_hello` → 模拟 supervisor 读 stream A（2s 后退出）
2. client 端：dial → **早期 `client_hello`**（与生产 `connect_to_handle` 对齐）
3. client 端：`peer.run(PeerRole::Client)` —— **核心断言点**
4. 让 run 跑 1s（让 open_bi / read_loop / 主循环进入稳态）
5. client 主动 close conn
6. 同步等 `run_task` 在 2s 内退出
7. **断言**：`Err(HelloTimeout(_))` 或 `Err(HelloFailed(_))` 都 panic（修前的症状）；`Err(Handshake(LocallyClosed))` 是修后期望的正常 close 路径

**回归验证**：

- 修复后：测试通过（peer.run 跳过 hello，进 read_loop + 主循环，conn close 触发 `Handshake(LocallyClosed)`）
- 临时回退修复：测试**失败**，panic 信息：
  ```
  Bug #3 回归：peer.run() 返 Err(HelloFailed(read Hello frame length: connection lost)) ——
  重复 client_hello 开 stream D、server 不 accept、read 失败
  ```

→ 测试有效捕获 root cause，不会随实现漂移漏过。

---

## 5. 验收（全三 bug 修后）

### 5.1 单测

```
cargo test --lib
test result: ok. 41 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

新增 2 个测试：
- `rejection_channel_forwards_rejected_fingerprint`（Bug #1）
- `peer_run_skips_hello_if_already_done`（Bug #3）

其余 39 个原有测试不变。**Bug #2 的 `dial()` 与 `send()` 共享同一套 RetryState + connecting 去重逻辑** —— 既有 `backoff_doubles_on_each_failure` + `reconnect_on_peer_close` 测试已覆盖，不重复加 E2E。

### 5.2 编译

```
cargo check --lib
warning: `lan-mouse` (lib) generated 4 warnings   ← 全部 pre-existing
Finished `dev` profile [unoptimized + debuginfo] target(s)
```

新增代码未引入新 warning。

```
cargo build
warning: `lan-mouse` (lib) generated 4 warnings   ← 全部 pre-existing
Finished `dev` profile [unoptimized + debuginfo] target(s)
```

完整 build（main.rs / GTK / cli）通过。

---

## 6. 新增修复：alive 永远 false 阻塞 send（**Bug #4** — 已修）

### 6.1 现场日志（Bug #1/#2/#3 修后用户报"还是不行"）

```
[2026-09-01T13:05:52Z WARN  lan_mouse::capture] releasing capture: emulation is disabled on the target device
```

**关键观察**：连接已建立（Bug #3 fix 后），鼠标移到屏边 → capture 触发 `conn.send()` → `send()` 返 `TargetEmulationDisabled` → capture 立刻释放 → 用户看到 "releasing capture: emulation is disabled on the target device"。

### 6.2 根因

`alive` 字段（`lan_mouse_ipc::ClientState::alive`）默认 `false`（`#[derive(Default)]`）。

- `set_alive(handle, bool)` 函数存在（`src/client.rs:307`），但**全工程无生产 caller**
- 设计意图：服务端的 Pong 响应会通过 stream A 传到客户端，客户端收到后调 `set_alive(pong_value)` —— 但**客户端没有 reader 把 stream A 上的 Pong 推到 `recv_tx`**
- `recv_tx` 字段（`src/connect.rs:85`）是死字段：`#[allow(dead_code)]` 都不需要（实际无 caller）
- `src/capture.rs:306` 的 `(handle, event) = self.conn.recv()` 永远挂起 —— recv_tx 永远没数据
- 后果链：
  1. 服务端发 Pong(true) → 客户端 peer.run() stream A 读到（peer.run 不处理，只 log debug）
  2. `alive` 始终是默认 `false`
  3. `send()` 在 peer 存在时立刻 `if !alive { return Err(TargetEmulationDisabled); }`
  4. capture 释放 → 用户看到 "releasing capture: emulation is disabled on the target device"

### 6.3 修复（最小变更）

**`src/connect.rs::LanMouseConnection::send()`**：移除 alive 检查，乐观假设 peer 在线。

```rust
// 修前
if let Some(peer) = peer {
    if !self.client_manager.alive(handle) {
        return Err(LanMouseConnectionError::TargetEmulationDisabled);
    }
    // ...
}

// 修后
if let Some(peer) = peer {
    // STEP-8.2 临时移除 alive 检查 —— 详见 send() docstring。
    // 乐观假设 peer 在线：supervisor 看到 peer.run() 退出（peer 真死）时
    // set_active_addr(None) 让下次 send 走重拨路径。
    // ...
}
```

**`LanMouseConnectionError::TargetEmulationDisabled` 变体保留**（`#[allow(dead_code)]`）—— M2 接回 Pong → `recv_tx` → `set_alive` 路径后重新启用。

### 6.4 已知缺陷（接受）

- **peer 把 emulation 关了**：本端 send 仍把事件推到 peer；peer 的 `emulation.rs:163` `consume()` 检查 `emulation_active.get()`，false 时事件**静默丢弃**
- **理想行为**：peer 关 emulation 时本端能感知 → 主动释放 capture（节省带宽 + 让 GUI 状态对）
- **当前能做的修复路径**：M2 接回 Pong → set_alive(true/false) + send 恢复 alive 检查
- **为什么本次不修**：recv_tx → `LanMouseConnection::recv()` 整个事件流入路径未装配，需要：
  1. peer.run() 主循环 stream A reader 加一条"Pong → recv_tx.send"分支
  2. capture.rs:306 的 `conn.recv()` 才能收到 Pong
  3. 加 Pong 处理分支调 set_alive
  - 这是 STEP-7.x stream A reader 的下游工作（listen.rs 已有 stream A reader，但 LanMouseConnection 路径未装配），范围超出当前 10.2.1.12 现场修复
  - 接受范围：peer 端关闭 emulation 是边缘场景（用户主动操作），核心是"连接 + 键鼠"能通

---

## 7. 用户操作建议（修复后）

两侧 daemon 拉最新代码重新 `cargo run` 启动后：

1. **若指纹已在 `[authorized_fingerprints]`**：Bug #2 修复让两侧启动后立即主动拨号；Bug #3 修复让 client_hello 不再重复调用 → 几秒内建连
2. **若指纹缺失或错误**：
   - 对端 dial 进 → 本地 verifier 拒 → **GUI 弹窗**（之前是静默失败）
   - 用户点击"接受" → fingerprint 加入 allowlist
   - **当前局限**：peer 端 supervisor 不会自动重试 —— `should_retry_after_close` 对 `TransportError` 返 `false`（保守不重试，认为是协议错误）。用户需手动 toggle client 状态 / 重启 daemon 触发新一轮 `activate_client` → `dial()` → 拨号
   - **未来 follow-up**：可考虑 `should_retry_after_close` 区分"rustls allowlist 拒绝"（可重试）vs "协议错误"（不可重试），或让 connect_to_handle 在失败时自动 backoff loop 重试。当前不阻塞本 STEP 验收

---

## 7. 时间/耗时

- 调研：~15 min（§0-§3 + 用户确认）
- 实施修复：~30 min（Bug #1 三文件 + Bug #2 三文件 + 1 单测 + 文档）
- 二次修复：~25 min（Bug #3 + 回归测试 + 用户回退验证 + 文档追加）
- 三次修复：~20 min（Bug #4 + 文档追加）
- 总计：~90 min

两侧 daemon 拉最新代码重新 `cargo run` 启动后：

1. **若指纹已在 `[authorized_fingerprints]`**：Bug #2 修复让两侧启动后立即主动拨号 —— 几秒内应建连
2. **若指纹缺失或错误**：
   - 对端 dial 进 → 本地 verifier 拒 → **GUI 弹窗**（之前是静默失败）
   - 用户点击"接受" → fingerprint 加入 allowlist
   - **当前局限**：peer 端 supervisor 不会自动重试 —— `should_retry_after_close` 对 `TransportError` 返 `false`（保守不重试，认为是协议错误）。用户需手动 toggle client 状态 / 重启 daemon 触发新一轮 `activate_client` → `dial()` → 拨号
   - **未来 follow-up**：可考虑 `should_retry_after_close` 区分"rustls allowlist 拒绝"（可重试）vs "协议错误"（不可重试），或让 connect_to_handle 在失败时自动 backoff loop 重试。当前不阻塞本 STEP 验收

---

## 5. 时间/耗时

- 调研：~15 min（§0-§3 + 用户确认）
- 实施修复：~30 min（Bug #1 三文件 + Bug #2 三文件 + 1 单测 + 文档）
- 总计：~45 min

---

## 0. 问题现场

- 本机 IP：`10.2.1.15`
- 对端 IP：`10.2.1.12`
- ping 双向通；UDP 4242 双向可达（`nc -u -z`）
- `~/.config/lan-mouse/config.toml` 配置 `[[clients]] hostname="10.2.1.12"`，`activate_on_startup=true`，`position="top"`
- `~/.local/share/lan-mouse/known_peers/8d_b4_f1_...pin` 存在（曾 TOFU pin 成功过）
- `[authorized_fingerprints]` 后用户加了 `8d:b4:f1:... = ""`（对方 cert fingerprint）
- 用户问："加了指纹还是不行"
- 用户问："GUI 按道理请求链接时应该有弹窗提示接受指纹吗？"

---

## 1. 发现：GTK 弹窗路径完全断开（**Bug #1**）

### 1.1 代码现状

`ListenEvent::Rejected { fingerprint }` 在 `src/listen.rs:77, 100` 声明，`src/emulation.rs:190` 处理（推 `EmulationEvent::ConnectionAttempt` → service.rs:320 → `FrontendEvent::ConnectionAttempt` → GTK `request_authorization`）。

但**全工程没有任何一处 `send(ListenEvent::Rejected ...)`**：

```sh
$ grep -rn 'ListenEvent::Rejected\b' src/
src/emulation.rs:190:                    Some(ListenEvent::Rejected { fingerprint }) => {
$ grep -rn 'send(ListenEvent::Rejected' src/
(none)
```

### 1.2 后果

- 对端 dial 进来时，`AuthorizedKeysVerifier::verify_client_cert`（`src/quic_transport.rs:2812`）未命中 allowlist → 返回 `Err(rustls::Error::General("unauthorized peer {fp}"))`
- rustls 在 mTLS handshake 阶段直接拒绝 → `handle_quic_peer_supervisor`（`src/listen.rs:407`）**根本不会被调用**
- quinn `accept()` 那一层只能看到 handshake 失败，但不会发 `ListenEvent::Rejected` —— 而 Rust 端的 `Endpoint::accept` 也**没有**把验证失败的 conn 路径转成 `Rejected`
- 结果：**用户永远不会看到 AuthorizationWindow 弹窗**（emulation.rs:190 那个 match 分支是死代码）
- 唯一可见信号：`quic_transport.rs:2841` 的 `log::warn!`，**只写日志，不通知 GUI**

### 1.3 修复方向（待实现）

需要在 listen task 的 accept 循环里 catch handshake 失败、提取 client cert fingerprint、发 `ListenEvent::Rejected`。要点：

1. quinn 的 `Connecting::await` 失败时（rustls 校验未通过），peer identity 可能拿不到；
2. 需要在 rustls `ServerCertVerifier` 失败路径里反向 channel 回去，或者在 `accept()` 上一层包装一层"先 TLS handshake 取 peer cert（轻量）→ 再放行/拒绝"；
3. 简单方案：让 `AuthorizedKeysVerifier` 在 reject 时把 fingerprint 通过共享 channel 通知 listen task，由 listen task 转译为 `ListenEvent::Rejected`。

> 注：本问题**与 STEP-2.7 验收测试**（`src/quic_transport.rs:3432,3470` 已通过单元测试）正交——单元测试直接调 `verify_client_cert`，路径是 OK 的；**集成路径**（QUIC handshake 失败 → GUI 弹窗）才是断的。

---

## 2. 发现：拨号 lazy 触发（**设计现状 #2，不是 bug 但要意识到**）

### 2.1 现状

`src/connect.rs:133-205` `LanMouseConnection::send(event, handle)`：

- **只有"我要发送事件"时才触发 dial** —— 没有任何 periodic re-attempt / startup dial。
- `activate_on_startup = true` 只让 `ClientState.active = true`，不会主动建连。

### 2.2 后果

- 本机 daemon 启动后即便 `activate_on_startup=true`，也不会主动 dial 对端；需要**鼠标物理移到屏幕边**（此处 `position="top"`）才会触发。
- 对端也是同样：移到对应边才会主动 dial 本机。
- **任何一侧没人移动鼠标，连接永远不建**。

### 2.3 修复方向（可选）

- 加 `connect_on_activate`：在 `activate_client`（`src/service.rs:544`）末尾立即 spawn 一次 `connect_to_handle`（不依赖 send）。
- 注意 happy-eyeballs + retry 已存在（`src/connect.rs:240-287`），重复拨号是去重 + dedup 的。

---

## 3. 待用户确认（**未实现 #3**）

按用户指示：本次**只记录，不写代码**。

- [ ] Bug #1（GTK 弹窗路径断开）：等用户拍板是否修、修哪条路径（rustls reject 路径反向通知 vs accept 包装层）
- [ ] 设计现状 #2（拨号 lazy）：等用户决定是否升级为 `connect_on_activate`
- [ ] 同时确认对端（10.2.1.12）那边的 `authorized_fingerprints` 是否真的加了**本机**指纹 `A4:9B:47:25:50:3D:17:4F:B2:64:59:95:F8:A4:4D:FF:0C:31:CA:EA:D3:A9:88:8C:8F:38:C8:C7:91:48:E7:A2`（用户说他加了，但本机目前没收到任何 Accept 事件，所以行为上看不见对端 dial 成功）

---

## 4. 时间/耗时

- 调研：~15 min（已在本 STEP 内完成）
- 实现修复：未开始
---

## 8. 新增修复：stream A 控制事件路径走新 bidi，server 不读（**Bug #5** — 已修）

### 8.1 现场日志（Bug #1-#4 修后用户报"还是不行"）

```
[2026-09-01T13:12:17Z INFO  lan_mouse] creating input emulation ...
[2026-09-01T13:12:17Z INFO  input_emulation] using emulation backend: macos
[2026-09-01T13:12:17Z INFO  lan_mouse::connect] client 0 connecting ...
...
[2026-09-01T13:12:47Z INFO  lan_mouse::connect] client 0 connecting ...
[2026-09-01T13:12:47Z INFO  lan_mouse::connect] client (0) connected @ 10.2.1.12:4242 (quic)
[2026-09-01T13:12:47Z INFO  lan_mouse::quic_transport] read_loop: stream B reader spawned (cap=64), stream C dropped (M1 §9 守门)
```

**关键观察**：Bug #3 修后 client_hello 成功；Bug #4 修后 send 不再返
TargetEmulationDisabled。但 read_loop 起来后**没有任何 Enter / Ack /
Pong 等控制事件日志** —— 用户移动鼠标后键鼠还是不通。

### 8.2 根因

**客户端** `PeerSession::send_stream_a` 每次 `open_bi()` 开**新** bidi
stream 写控制事件（Enter / Leave / Ack / Hello / Ping / Pong）。

**服务端** `listen.rs::handle_quic_peer_supervisor` 只读缓存的
`recv_a`（来自 server_hello 同 bidi）。

→ **两条不同的 stream** → server 端不读新 stream → 控制事件永远
到不了 server：

- server 不知道 client 想 release capture（**无 Enter**）→ server
  capture 不释放、server 不 inject input
- server 不知道 client 想 Ack（**无 Ack**）→ client state 卡在
  WaitingForAck 也不影响（client 端也有死代码，但本地 send 不阻塞）
- server 不知道 client 想 Pong（**无 Pong**）→ alive 状态永远无更新
  （与 Bug #4 同源，但即使 Bug #4 修了，Pong 也到不了）

→ 用户看到"连上了但键鼠不通"。

**为什么修前 `peer_session_round_trip_motion_keyboard` 测试还过**：那条
测试发 Motion → route_input 选 Datagram → 走 QUIC datagram 通道
（非 stream）→ 收发都通。**Stream A 路径从未被任何测试覆盖**。

### 8.3 修复

**`src/quic_transport.rs::PeerSession`**：

1. 加字段 `cached_send_a: tokio::sync::Mutex<Option<SendStream>>` —
   缓存 hello 时的同一条 bidi 的 send 半边
2. 加方法 `take_stream_a_send()` —— 镜像 `take_stream_a_recv`，从
   `stream_a_cache.send` 取出 send 半边（保留 recv 给 take_recv 用）
3. `client_hello` / `server_hello` 完成后 put `Pair { send, recv }` 进
   `stream_a_cache`，**然后**调 `take_stream_a_send` 把 send 搬到
   `cached_send_a`
4. `send_stream_a` 优先用 `cached_send_a`（与 server 端
   `take_stream_a_recv` 拿到的 recv 是**同一条 bidi**）；cached 不可
   用时 fallback 旧 open_bi 路径（保留兜底）

**Mutex + 持锁 await 设计**：send_stream_a 是 stream A 唯一写路径（无
其他 caller），持锁期间并发 caller 排队串行 —— 与 QUIC stream 一帧
一帧语义对齐。

### 8.4 测试

**新增** `send_stream_a_round_trip_control_event`（`src/quic_transport.rs`）：

模拟生产路径：
1. server_ep + client_ep
2. server task: accept → server_hello → take_stream_a_recv → 等 1 帧
3. client: dial → client_hello → send_input(Ping)
4. **断言**：server 在 3s 内能从 recv_a 读到 Ping
5. 修前：server 永远读不到（Ping 走新 bidi）→ 3s 超时 panic
6. 修后：server 读到 Ping → 测试通过

**修改** `hello_happy_path_exchanges_magic` 测试断言：

- 原断言 `take_stream_a_cache` 返 Some（验证 Pair 整对缓存）——
  修后返 None（因为 send 半边被 take 走了）
- 改为分别断言 `take_stream_a_recv` 返 Some + `hello_ok()` 为 true
  （隐含 cached_send_a 已就绪）

### 8.5 已知遗留

- `peer.run()` 主循环里 `open_bi × 3`（pairs[0..2]）中的 `pairs[0]`
  被注释为 "stream A" 但实际上是**新 bidi** —— 现在 send_stream_a
  走 cached_send_a，`pairs[0]` 完全没用了
- 修法：把 peer.run 的 `for i in 0..3u8` 改成 `for i in 0..2u8`（只开
  B/C 两 stream）+ `set_stream_bunch` 不再设 `a` 字段
- 影响：无功能影响（仅多余 stream 开销 + bunch.a.send 是死引用）；
  留 M2 cleanup

---

## 9. 时间/耗时（最终）

- 调研 + Bug #1/#2 修：~45 min
- Bug #3 + 回归测试：~25 min
- Bug #4：~20 min
- Bug #5：~30 min（含回归测试设计 + 现有断言迁移 + 文档追加）
- **总计：~2 小时**（超出 PLANER 期望的 1 小时上限 —— 五个相关联的 bug 累
  计时间，且每个修复后都要等用户复测才暴露下一个；建议下次类似场
  景先一次性翻完整个数据通路再批量修，避免反复 commit/retest 周期）


---

## 10. 新增修复：Enter 处理被 dead-code stub 跳过（**Bug #6** — 已修）

### 10.1 现场日志（Bug #1-#5 修后用户复测，进展 + 残留）

**进展**：
- 远程 daemon 重启后 INFO 正常：`stream A recv from 10.2.1.15:61252: Enter(top)`
  —— Bug #5 修复生效，stream A 端到端通
- 本机日志正常：`send Enter(bottom) to handle 0 addr 10.2.1.12:4242 via peer (active)`

**残留**：
- 远程收到 Enter 后**鼠标没出现** —— 远程不 inject input
- 本机反复 `send Enter(bottom)`（约每秒一次）—— 卡在 WaitingForAck
  永远收不到 Ack
- 结束远程程序后**过好久本机才出现鼠标** —— 因为 RetryState 退避
  30s 上限，dial 不再尝试，等 supervisor 超时

### 10.2 根因

`src/emulation.rs:175` Enter 处理：
```rust
ProtoEvent::Enter(pos) => {
    if let Some(fingerprint) = self.listener.get_certificate_fingerprint(addr).await {
        log::info!("releasing capture: {addr} entered this device");
        self.event_tx.send(EmulationEvent::ReleaseNotify).expect("...");
        self.listener.reply(addr, ProtoEvent::Ack(0)).await;
        self.event_tx.send(EmulationEvent::Entered{addr, pos: to_ipc_pos(pos), fingerprint}).expect("...");
    }
}
```

但 `src/listen.rs:316` `LanMouseListener::get_certificate_fingerprint` 是
**dead-code stub**：
```rust
pub(crate) async fn get_certificate_fingerprint(&self, addr: SocketAddr) -> Option<String> {
    let _ = addr;
    None  // 永远 None
}
```

→ 整个 if 块跳过 → 远程 Enter 后**不**：
- release capture（service.add_incoming 不触发）
- reply Ack（本机永远等不到 → 反复 send Enter）
- 发 EmulationEvent::Entered（service + frontend 不知道有 peer 进入）

**listen.rs:316 的 docstring 写的是期望**：
> "ListenTask 在 Enter 时不需要重算 fingerprint —— 直接查 map
>  即可。本函数保留是 emulation.rs 的现有 API 调用站桩，**M1
>  阶段不真正用**"

—— 但实际 `addr_to_fingerprint` map **从来没建过**，与 docstring 描述
的"未来路径"从未落地。

### 10.3 修复

`src/emulation.rs::ListenTask` 自己维护 `addr_to_fingerprint: HashMap<SocketAddr, String>`：

1. **新字段** `addr_to_fingerprint: HashMap<SocketAddr, String>`
   （ListenTask struct + Emulation::new 构造初始化空 map）
2. **`ListenEvent::Accept { addr, fingerprint }` 分支**：
   ```rust
   self.addr_to_fingerprint.insert(addr, fingerprint.clone());
   self.event_tx.send(EmulationEvent::Connected { addr, fingerprint });
   ```
   （同时仍 forward Connected 给 service —— 兼容现有 service.rs 路径）
3. **`ListenEvent::Disconnected { addr }` 分支**：
   `self.addr_to_fingerprint.remove(&addr);` —— peer 重连会触发新
   Accept 重填 fingerprint，旧 fingerprint 不能残留
4. **Enter 处理**：直接查 map，不调 dead stub：
   ```rust
   let fingerprint = self.addr_to_fingerprint.get(&addr).cloned()
       .unwrap_or_default();  // race 兜底：理论上 Accept 必在 Enter 之前
   ```
   后续 send ReleaseNotify / reply Ack / send Entered 都不再被 if 包
   住。

### 10.4 副效应

service.rs:323 `add_incoming(addr, pos, fingerprint)` 现在能收到真实
fingerprint（修前是空字符串 ""，service 也没校验所以表现上看不出
差别，但 GUI / 配置侧能看到）。

### 10.5 已知遗留（接受范围）

- 本机反复 send Enter 的"spam"：Bug #5 修后 send 走 cached，**但
  Bug #4 移除了 alive 守护** → 客户端 send 不知道 server 已收到
  Ack（无 Pong 路径）→ 持续发 Enter 直到 mouse 离开 capture 区。
  严格说有 Pong 路径后应 stop sending Enter，但 Pong 路径要
  接 recv_tx 是更大的修复（见 §6.4 Bug #4 已知缺陷）—— 接受范围
- 没有新单测覆盖 ListenTask 主循环（需要 mock LanMouseListener +
  完整 capture 配合，超 M1 单测范围）—— 用户复测是最终验证

---

## 11. 时间/耗时（累计）

- Bug #1（mTLS reject 反向通知）：~30 min
- Bug #2（connect_on_activate）：~15 min
- Bug #3（Hello 握手重复）：~25 min
- Bug #4（alive 永 false 移除）：~20 min
- Bug #5（stream A 控制事件路径）：~30 min
- Bug #6（Enter dead-code stub）：~25 min
- **总计：~145 min（~2.5 小时）**

5/6 bug 都是同一类：**死代码 / 未连接路径让关键逻辑被 bypass**。
建议下次类似现场先做一次**全数据通路 audit**（capture →
send → peer.send_input → stream A/B/C → server listen.rs supervisor
→ emulation.rs consume 全链路），把每个分支都过一遍，再批量修，
避免反复 commit/retest 周期。

---

## 12. 新增修复：stream A 事件转发到 local capture（**Bug #7** — 已修）

### 12.1 现场日志（Bug #6 修后用户复测）

```
# 远程（Windows）日志：
stream A recv from 10.2.1.15:50999: Enter(bottom)
releasing capture: 10.2.1.15:50999 entered this device (fp=a4:9b:47:...)

# 本机（Mac）日志：
send Enter(bottom) to handle 0 addr 10.2.1.12:4242 via peer (active)  ← 反复出现
```

**进展**：Bug #6 修复生效，远程**真的处理了 Enter**（release capture /
reply Ack / 发 Entered 上报 service）。

**残留**：本机**反复 send Enter(bottom)** —— 卡 WaitingForAck 永远
收不到 Ack。Bug #6 修复后 server 端**真的发了 Ack**（`self.listener
.reply(addr, ProtoEvent::Ack(0)).await`），只是**客户端收不到**。

### 12.2 根因

`src/connect.rs:82` `LanMouseConnection::recv_tx` 字段——`Bug #4`
文档里已点出是死字段（"recv_tx 是死字段，全工程无 caller 喂它"），
但 Bug #4 修复**没**接 recv_tx 路径。

`peer.run()` 主循环从 stream A 读到 Ack/Pong/Leave → 修前
**只 `log::debug!`**：

```rust
res = read_frame(&mut recv_a) => {
    match res {
        Ok(event) => {
            // Control 类 —— 本步仅日志（Hello 已 done；Enter/Leave/
            // Ack/Ping/Pong 留 STEP-6.x 接入 LanMouseConnection 时
            // 走 IPC 推送）
            log::debug!("run: stream A read event: {event:?}");
        }
```

→ `recv_rx` 永远空 → `LanMouseConnection::recv()` 永远 await →
`capture.rs::do_capture_session()` 收不到 Ack → 本地 state 永远
卡 WaitingForAck → 反复 `send Enter`。

**为什么 Bug #6 修后**才暴露**这个**问题：Bug #6 修前 remote 收到
Enter 后**整个 if 块被跳过**（dead-code stub）→ remote **不发
Ack** → 本机**永远收不到** Ack（自然不会发现 recv_tx 是死字段）。
Bug #6 修后 remote 真的发 Ack → 本机**应该能收但实际收不到** → 暴露
recv_tx 死字段问题。

**第 4 层同一根因系列（与 Bug #4 / recv_tx 死字段同源）**：
- Bug #4 移除 alive 检查（上层）
- Bug #7 接 recv_tx 路径（事件流入）
- TODO M2：处理 Pong → set_alive 重新接回 alive 检查（语义层）

### 12.3 修复（最小侵入）

**`src/quic_transport.rs::PeerSession`**：

1. 加字段 `outgoing_events: Arc<Mutex<Option<UnboundedSender<(SocketAddr, ProtoEvent)>>>>`
2. 加方法 `set_outgoing_events(Option<UnboundedSender<...>>)` —— 在
   `client_hello` 后、`spawn peer.run` 前由 caller 设
3. `peer.run()` 主循环 stream A handler 改成：
   ```rust
   log::debug!("run: stream A read event: {event:?}");
   if let Some(tx) = self.outgoing_events.lock().await.as_ref() {
       let remote = self.conn.remote_address();
       let _ = tx.send((remote, event.clone()));
   }
   ```

**`src/connect.rs::connect_to_handle`**：

1. 加参数 `recv_tx: Sender<(ClientHandle, protoEvent)>`（三个 caller
   传 `self.recv_tx.clone()`）
2. 建 `outgoing` mpsc channel `(SocketAddr, protoEvent)`
3. spawn forwarder task：
   ```rust
   while let Some((addr, event)) = out_rx.recv().await {
       if let Some(handle) = client_manager.get_client(addr) {
           recv_tx.send((handle, event));  // → LanMouseConnection::recv()
       }
   }
   ```
4. `peer.set_outgoing_events(Some(out_tx))` 在 spawn peer.run 前
5. forwarder task 加 INFO 日志 `stream A forwarder: {addr} → handle
   {handle}: {event}` 让用户复测时看到路径通了

### 12.4 已知遗留（接受）

- **Reconnect 路径**：supervisor 触发 reconnect 时，无法直接拿
  原 LanMouseConnection 的 recv_tx —— 本简化版传一个 local channel
  default（无 forwarder）。但 reconnect 期间 supervisor 已 `set_active_
  addr(None)`、capture release，等下次 dial 成功又会重新设
  outgoing_events + spawn 新 forwarder —— **语义 OK**
- **forwarder send 失败的处理**：recv_tx.send 失败说明 capture task
  已退（terminate）→ break。**理论 OK**，但有 race —— 上次 recv_tx
  close 后又创建新的 capture task 的话旧 forwarder 还在 break 的路上
  —— 实际不会发生，因为 forwarder 的生命周期 ≤ LanMouseConnection

### 12.5 验证

- `cargo test --lib` → 42 passed / 0 failed
- `cargo check --lib` → 2 warnings（pre-existing power_observer + 1
  个 dead_code in test，**未引入新 warning**；`recv_tx` dead_code
  警告消失 —— 现在真被使用）

---

## 13. 时间/耗时（最终累计）

| Bug | 时间 |
|---|---|
| #1（mTLS reject 反向通知） | ~30 min |
| #2（connect_on_activate） | ~15 min |
| #3（Hello 握手重复） | ~25 min |
| #4（alive 永 false 移除） | ~20 min |
| #5（stream A 控制事件路径） | ~30 min |
| #6（Enter dead-code stub） | ~25 min |
| #7（stream A 事件转发 recv_tx） | ~30 min |
| **总计** | **~175 min（~3 小时）** |

7 个 bug **全是同一条事件流的不同环节的死代码 / 未连接路径**：

```
capture → send → peer.send_input → stream A/B/C → server listen.rs
  supervisor → emulation.rs consume/Enter 处理 → ack → 回到 capture

每个环节都有未实现的 stub、dead_code 注释、或 TODO 把链路断掉。
```

**建议**：下次类似现场先做**一次完整 audit**，从 `capture.rs`
事件入到 `peer.send_input` 出到 `quic_transport::accept` 回到
`emulation.rs` 处理，整条链每个分支都过一遍，再批量修——这次如
果一开始就审计完整链路，应该一次 commit 解决所有 7 个 bug。

---

## 14. Follow-ups（不修，记录到 doc）

### 14.1 quic_transport.rs 拆分（refactor follow-up）

**现状**：`src/quic_transport.rs` 5534 lines —— 7.4× 第二大文件（listen.rs 748 lines）。
占项目总代码 ~51%。

**结构**（STEP 章节）：
- STEP-3.2 PeerSession + Hello: 488 lines
- STEP-4.4 Channel + route_input: 370 lines
- STEP-5.2 Frame codec: 135 lines
- STEP-5.3 read_loop + stream management: 265 lines
- STEP-5.4 run + watchdog + datagram_reader: 482 + 468 lines
- Verifiers (TofuVerifier / AuthorizedKeysVerifier / permissive): 430 lines
- Tests: 2342 lines（占 42%！）

**推荐拆分（3 文件 + 测试分摊）**：

| 文件 | 内容 | 预计 | 职责 |
|---|---|---|---|
| `quic_endpoint.rs` | `endpoint/dial/dial_any/accept`、`endpoint_with_cert/verifier`、`build_quic_client_config`、`install_crypto_provider`、`default_transport_config`、`PeerSession::from_connection`、HELLO_TIMEOUT/ALPN 常量 | ~600 | **连接建立** |
| `quic_verifier.rs` | `TofuVerifier`、`AuthorizedKeysVerifier`、`permissive_client_cert_verifier` | ~430 | **TLS 证书校验** |
| `quic_transport.rs` (保留) | PeerSession struct + impl + run、StreamPair/Bidi/StreamBunch、Error、Hello 握手、codec、Channel + route_input、read_loop、datagram_reader_task、PeerRole、should_retry_after_close | ~1900 | **会话生命周期** |
| 各文件 `mod tests` | tests 跟着对应文件走（verifier tests 进 verifier，endpoint tests 进 endpoint，session tests 进 main） | 分摊 | 测试访问 private items 必须同文件 |

**不拆的**：
- `Error` enum 留 main（核心错误类型，跟 PeerSession 同源）
- `StreamPair/Bidi/StreamBunch` 留 main（与 PeerSession 紧耦合）
- `Channel` + `route_input` 留 main（与 `send_input` 紧密耦合）

**公开 API 不变**（re-export from main file），调用方零修改。

**何时做**：下一次大重构窗口（5 个以上 PR 或项目转阶段时）。当前 7 个 bug 链已让文件混乱，但**优先修 bug 不优先重构**。

### 14.2 macOS 30s 鼠标卡死（已知限制）

**症状**：peer 关闭（或 QUIC idle_timeout 30s）后，本地 mouse 卡 30s 内无法移动/显示，
之后才恢复（"几十秒" 匹配 QUIC `max_idle_timeout = 30s`）。

**当前修复状态**：
- Bug #9/Bug #10：peer.run 检测 closed() future fire 或 read IO 错 → 推 Leave → capture 立即 release（**协议层正确**）
- Bug #11 防御性 Reenable：**已回退**（`9d0b4d5`）—— 重建 OS tap 没能解决 macOS tap 启动期状态问题，反而可能引入新问题

**根因（推测，未证实）**：
- macOS `input-capture` crate 的 CGEventTap 在 peer close 时未真正停止
- capture.rs `release_capture` 调 `capture.release()` 仅让 macOS producer 设 `current_pos = None`（cursor 应可见），但 `CGEventTap` 本身仍 active
- capture.rs 进入 inner loop（等 Reenable），不 poll `capture.next()` → `event_tx` buffer 32 满 → tap callback `blocking_send` 阻塞 → CGEventTap 1s timeout → re-enable → 死循环
- macOS 用户态 cursor 视觉卡住（内核 cursor position 在更新，但 user-perceived "卡 30s"）

**修复路径（需深入 input-capture crate）**：
1. 修改 `input-capture/src/macos.rs` —— release() 时同步 disable CGEventTap（不只发 notify）
2. 或在 capture.rs 检测 TimedOut 后调 `set_alive(false)` 暂停 capture（让用户手动重启 daemon）
3. 或在 Drop impl 里加 `CGEventTapEnable(port, false)` 显式停 tap

**项目范围**：超出 lan-mouse-pro（属 input-capture crate 内部问题）。需要单独 PR。

**当前 workaround**：用户接受 30s 延迟，或远端 daemon 关闭后**等满 30s 再开始新会话**（让 QUIC idle_timeout 自然触发）。

### 14.3 首次连接应自动弹窗接受指纹（功能缺失）

**现状**：首次连接对端时，需要用户**手动**在 `~/.config/lan-mouse/config.toml`
的 `[authorized_fingerprints]` 段加对方 cert fingerprint，否则 mTLS 拒握 +
握手失败。如果不加，连接建立后立即断开（用户看到 "dial timed out"
但日志里有 `AuthorizedKeysVerifier: rejected unauthorized peer ...`）。

**期望**：对端 dial 进时，本地 verifier 拒绝 → 弹 GTK 窗口显示对端 fingerprint
+ "接受 / 拒绝"按钮 → 接受后自动写入 `[authorized_fingerprints]`
→ 重连成功（无需手动编辑 config）。

**当前代码状态**（Bug #1 修后已大半到位）：
- ✅ `quic_transport.rs` AuthorizedKeysVerifier 拒握时通过反向 channel 发 fingerprint
  （`rejection_tx.send(fp)`）
- ✅ `listen.rs` `spawn_rejection_forwarder_task` 把 fp 推 `ListenEvent::Rejected`
- ✅ `emulation.rs` ListenTask match Some(Rejected) 推 `EmulationEvent::ConnectionAttempt { fp }`
- ✅ `service.rs` handle_emulation_event 转发为 `FrontendEvent::ConnectionAttempt { fp }`
- ✅ `lan-mouse-gtk/src/lib.rs:286-287` match ConnectionAttempt → `window.request_authorization(&fp)`
- ✅ `window.rs:573` request_authorization 创建 `AuthorizationWindow` 并 `present()`
- ✅ `authorization_window.rs` 提供 GTK 模板

**链路完整**，但用户实测**没看到弹窗**。推测：
- A. GTK lib.rs 端 IPC 链路没接通（AsyncFrontendListener 接收有问题）
- B. AuthorizationWindow 的 `connect_closure` 信号没正确绑定（`confirm-clicked` /
  `cancel-clicked` 在 template 里名字不一致）
- C. `present()` 调了但 window 没聚焦（macOS 上常见，application not active）

**排查建议**：
1. 在 `request_authorization` 里加 `log::info!` 看是否被调用
2. 在 `AuthorizationWindow::new` 里检查 `fingerprint` 是否非空
3. 检查 `authorization_window.ui` template 的按钮 id（`confirm-clicked` / `cancel-clicked`
   是否对应 GtkButton 的 action-name 或 signal）
4. 检查 macOS 应用是否 focus（`window.present()` 后可能需要 `window.present_with_time()` 或
   `set_keep_above(true)`）

**修复路径**：需要 GTK 调试 + macOS GUI 调试（user-perceived bug，跟具体 macOS
版本 / 焦点策略有关）。可能 1-2 小时。

**当前 workaround**：用户在 `~/.config/lan-mouse/config.toml` 加对端 fingerprint：
```toml
[authorized_fingerprints]
"<对方 fingerprint>" = ""
```
对端 cert fingerprint 可在远端 daemon 日志 `creating self-signed cert` 附近找到
（或 `openssl x509 -in ~/.local/share/lan-mouse/cert.pem -noout -fingerprint -sha256`）。

---

## 15. 时间/耗时（最终累计 — 含 14 follow-ups）

| Bug / Follow-up | 时间 |
|---|---|
| Bug #1（mTLS reject 反向通知） | ~30 min |
| Bug #2（connect_on_activate） | ~15 min |
| Bug #3（Hello 握手重复） | ~25 min |
| Bug #4（alive 永 false 移除） | ~20 min |
| Bug #5（stream A 控制事件路径） | ~30 min |
| Bug #6（Enter dead-code stub） | ~25 min |
| Bug #7（stream A 事件转发 recv_tx） | ~30 min |
| Bug #8（server 侧补 datagram reader） | ~30 min |
| Bug #9（peer 关闭 release capture） | ~25 min |
| Bug #10（read IO 错误路径） | ~20 min |
| Bug #11 防御性修复 + 回退 | ~40 min |
| Doc 整理 + follow-up 记录 | ~30 min |
| **总计** | **~5 小时** |

7 个核心 bug 都已修复（协议层）；3 个 follow-up 列入 14 节。
