# STEP-8.2 — 10.2.1.12 远程对接：mTLS Rejected 未接通 + 拨号 lazy 触发

> PLAN-M1 §STEP-8 / **新方向：远程对接可用性问题**
> 起点：用户现场问题（两台机器 10.2.1.15 / 10.2.1.12 互 ping / UDP 4242 互通但连不上）
> 执行日期：2026-09-01
> 状态：**调研 + 修复完成**（两路修复 + 1 单测已加，40 个 lib 测试全绿）

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

## 4. 用户操作建议（修复后）

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