### 首次连接应自动弹窗接受指纹（功能缺失）

**现状**：首次连接对端时，需要用户**手动**在 `~/.config/lan-mouse/config.toml`
的 `[authorized_fingerprints]` 段加对方 cert fingerprint，否则 mTLS 拒握 +
握手失败。如果不加，连接建立后立即断开（用户看到 "dial timed out"
但日志里有 `AuthorizedKeysVerifier: rejected unauthorized peer ...`）。

**期望**：对端 dial 进时，本地 verifier 拒绝 → 弹 GTK 窗口显示对端 fingerprint

- "接受 / 拒绝"按钮 → 接受后自动写入 `[authorized_fingerprints]`
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

### 启动时未跨鼠标边界，复制文本无法传到被控端（M1a follow-up #1）

**现状**：`service.rs::broadcast_clipboard_event` 的 gate 要求
`active_addr=Some(addr)` 才发送。active_addr 在 QUIC dial 成功 + Hello
握手完成后才被 `connect.rs:652` 设置。daemon 启动到 dial 成功之间
约 5-35s 窗口（含 backoff），**窗口内所有复制都丢**——`tick` 在跑、
`change detected` 日志有，但 `dispatched to 0 peers (no active_addr)`。
鼠标跨边界只是触发 dial 的方式之一，本质是"active_addr 还没 ready"。

**期望**：active_addr 从 `None → Some(addr)` 时，**自动把当前本地
剪贴板内容推一次给该 peer**，把窗口内 missed copies recover 掉。

**修复点**：在 `set_active_addr(handle, Some(addr))` 调用之后，触发
一次 `broadcast_clipboard_event(current_clipboard_content)`。需要复用
LRU 防回环。

**影响面**：`service.rs` + `connect.rs` 两文件，单 STEP。
**AI 估时**：~1.5h。
**对应 commit**：M1a follow-up 1a.6。

---

### 被控端复制文本不能传到主控端 — incoming peer 不在 broadcast 列表（M1a follow-up #2）

**现状**：`broadcast_clipboard_event` 只遍历
`client_manager.get_client_states()`——这个集合只包含 **outgoing
clients**。被控端是 incoming-only server，主控对被控而言是 incoming
peer，不在这个集合里。结果被控端 tick 在跑、`change detected` 有，
但永远 `dispatched to 0 peers`——slave 上复制的内容**永远推不出去**。
（INVESTIGATION-M1a-CLIPBOARD-NO-OP 假设 #9）

**期望**：`broadcast_clipboard_event` 也遍历 incoming peer（来自
`ListenEvent::Accept { addr, fingerprint }`）；对每个 active incoming
peer 也 push ClipboardText。

**修复点**：
- 给 incoming peer 引入状态表（active / enable_clipboard_to）
- 让 `broadcast_clipboard_event` 合并 outgoing + incoming 两个集合
- per-peer `enable_clipboard_to` 对 incoming 暂时默认 true（MVP，
  GUI 配置支持留后续）

**影响面**：`service.rs` / `emulation.rs` / `listen.rs` / `connect.rs`
4 文件，需扩展 per-peer 状态模型。
**AI 估时**：~2.5h。
**对应 commit**：M1a follow-up 1a.7。

---
