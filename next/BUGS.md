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

### 全屏截图后鼠标卡顿且无法粘贴

**根因分析**（已做两轮修复尝试，均不彻底，代码已回滚）：

1. **第一轮怀疑**：HTTP/3 大块 body 传输（master → controlled 把 3.7 MB PNG 通过 HTTP/3 response 写回）阻塞了 QUIC 连接上的 control 帧（Pong/Ack） → QUIC 流优先级修复（commit `b4191d4`）把 stream A 设为 `PRIORITY_CONTROL=+100`、HTTP/3 设为 `PRIORITY_BULK=-100`
   - **结果**：失败。用户实测截图后依然卡顿，说明根因不在 QUIC 包调度层面

2. **第二轮发现**（用户报告"小文本好/大截图卡"反向验证后）：真正的根因是 macOS 后端的 **JPEG→PNG / TIFF→PNG 转码在主控端的 LocalSet 线程同步执行**，单线程 `current_thread` runtime 下 PNG 编码 2–5s 把整条 runtime 锁死，期间：
   - `peer.run(StreamA read)` 拿不到 Pong
   - Pong-arrival forwarder 拿不到 `last_pong_at`
   - `ping_heartbeat_task` 发不出 Ping
   - `client_accept_bi_task` 接不到 HTTP/3 新 bidi
   - 1.5s Pong watchdog 超时，强制断开连接

3. **第二轮修复尝试**：commit `4313940` 给 `ClipboardBackend` 加了 `current_image_async`（返回 `Pin<Box<dyn Future + Send>>` 保 dyn 兼容），macOS override 用 `spawn_blocking` 跑 `jpeg_to_png_normalized` / `tiff_to_png_normalized`，dispatcher 的 `handle_clipboard_tick` 改调 async 版
   - **结果**：**用户实测后比之前更卡**——之前一次截图卡一下就恢复，现在每次跨越边界都卡。
   - **代码已 git revert**（commit `420221a`），回到 `b4191d4` 状态

**为什么 spawn_blocking 修复让事情变糟（仍未确认，待诊断）**：

候选假设（按可能性排序）：

- **H1：dispatch loop 被 await 锁住**——`handle_clipboard_tick().await` 是 `service::run` 主 `select!` 的一个 arm，await 期间其他 arm（capture 事件、emulation 事件、frontend 事件）全部延迟到 spawn_blocking 返回之后才被处理。截图后 2–5s 内所有 capture 事件（包括用户尝试跨边界触发的 BeginPending）都堆积，spawn_blocking 返回后才批量处理——但 BeginPending 的 500ms 超时在堆积期间就开始倒数了。
- **H2：spawn_blocking 引入了额外的调度开销**——LocalSet 单线程下，每次 cache miss 都通过 `spawn_blocking` 转发到 blocking pool；blocking pool 的线程上下文切换对单线程 runtime 有额外唤醒成本（待实测验证）。
- **H3：`current_image_async` 的 future 在某些路径上未真正 yield**——cache miss 时先做了 3 次 `read_pasteboard_bytes`（同步 IPC），再 `Box::pin(async move { spawn_blocking.await })`，这之间没有显式 yield。理论上 sync 部分很短（~1ms × 3）不影响，但需实测确认。
- **H4：被控端 daemon 没有跟着 force pull / 重启**——用户在被控端可能跑的是比主控端更老的二进制，导致两端实现不对等（不太能解释"更卡"）。

**还原现场需要的诊断信息**：

- 主控端 `RUST_LOG=lan_mouse=debug,quinn=info` 的完整日志，特别是从截图到卡顿结束这段时间内：
  - 是否出现 `clipboard: JPEG→PNG normalized ...` 信息（确认 spawn_blocking 路径确实跑了）
  - 截图前后的 Pong watchdog 日志（确认 1.5s 阈值是否仍触发）
  - 用户尝试跨边界时的 BeginPending / CancelPending 时序
- 被控端 daemon 在同一时间窗口的 `client accept_bi` / HTTP/3 GET 日志
- 被控端是否已 force pull + 重新构建（防止两边二进制不对等）

**下一步修复方向（按"代价从小到大"排序）**：

1. **最小代价：把 `handle_clipboard_tick` 拆出 dispatch loop 的 `select!`**——让它在独立 `spawn_local` task 里循环跑，主 `select!` 只看 capture/emulation/frontend 事件。这样 spawn_blocking await 不会阻塞 capture 事件处理。
2. **次小代价：throttle 跨边界的 BeginPending**——截图后短时间内（2–5s）直接屏蔽 image dispatch，只发小的恢复消息，避免堆积。
3. **结构性：把大 body transfer 走专用 QUIC 连接**（master 临时为该次 HTTP/3 GET 开一条带独立 cwnd 的连接，用完即关）——彻底隔离 bulk transfer 与 control plane。
4. **结构性：换 PNG 编码为更快的格式**（WebP / AVIF / 直接传 JPEG passthrough）——平台后端 JPEG passthrough 不需要转码。

---

**M1 修复落地（2026-09-12，`fix(svc): dispatch_image off LocalSet`）**：把 `Service::dispatch_image` 的 sha256 + cache insert 一步用 `spawn_blocking + move` 搬出 LocalSet。改动点：

- `image.data` move 进 blocking task 算 sha256，算完后 `image.data` 整体 move 进 `clipboard_cache.insert(sha, image_bytes)` —— **不再 clone 4-16 MB**。
- LocalSet 上只剩 cheap 操作：LRU / last_outbound 命中检查、cache lock、broadcast（metadata-only）—— 总耗时 < 5 ms。
- broadcast 不变（只发 `ClipboardImage { fingerprint, sha256, mime, size }` 元数据，body 走 HTTP/3 GET 拉，所以 metadata 没多占内存）。

**预期效果**：4 MB 截图的 LocalSet 占用从 ~150 ms 降到 < 5 ms；16 MB 截图（png 模式）从 ~500 ms（必触发 watchdog）降到 < 5 ms；Pong watchdog 在 dispatch 期间不会再卡死；BeginPending 不会再有"等 dispatch 完才被 service 处理"的延迟。

**为什么不直接做方案 1（整个 `handle_clipboard_tick` 拆出 select!）**：M0（`a94c249`）已经做了一半 —— poller 已经在 `spawn_local` task 里跑 `current_image_async()`，但 **poller 只生产 ImageBytes，`dispatch_image` 仍在主 `select!` 的 `image_rx.recv() => self.dispatch_image(image).await` 分支上**（`src/service.rs:1034`）。M1 改的就是这个 arm 内部。


这是被控端日志：[2026-09-10T22:11:41Z INFO input_capture::windows::event_thread] initial monitors: 1 monitor(s)
[2026-09-10T22:11:41Z INFO input_capture::windows::event_thread] monitor: id=windows:unknown-\\.\
DISPLAY1 name="Intel(R) UHD Graphics" pos=(0, 0) size=(1280, 800) primary=true scale=1
[2026-09-10T22:11:41Z INFO input_capture] using capture backend: windows
[2026-09-10T22:11:41Z INFO lan_mouse::emulation] creating input emulation ...
[2026-09-10T22:11:41Z INFO input_emulation] using emulation backend: windows
[2026-09-10T22:11:41Z INFO lan_mouse] opening http://127.0.0.1:3939 in the default browser
[2026-09-10T22:11:42Z INFO lan_mouse] using config: "C:\\Users\\hb\\AppData\\Local\\lan-mouse\\conf
ig.toml"
[2026-09-10T22:11:42Z INFO lan_mouse] Press [KeyLeftCtrl, KeyLeftShift, KeyLeftMeta, KeyLeftAlt] to
release the mouse
[2026-09-10T22:11:42Z INFO lan_mouse::web] lan-mouse web UI listening on http://127.0.0.1:3939
[2026-09-10T22:11:42Z WARN lan_mouse::service] clipboard dispatched to 0 peers (sha=e3b0c442); peer
gate filtered all clients — check `enable_clipboard_to` in TOML and that the connection is active
[2026-09-10T22:11:47Z INFO lan_mouse::quic_transport::tls] AuthorizedKeysVerifier: authorized peer
a4:9b:47:25:50:3d:17:4f:b2:64:59:95:f8:a4:4d:ff:0c:31:ca:ea:d3:a9:88:8c:8f:38:c8:c7:91:48:e7:a2
[2026-09-10T22:11:47Z INFO lan_mouse::listen] QUIC peer connected: 10.2.1.15:50247
[2026-09-10T22:11:47Z INFO lan_mouse::listen] QUIC peer 10.2.1.15:50247 authorized (fingerprint a4:
9b:47:25:50:3d:17:4f:b2:64:59:95:f8:a4:4d:ff:0c:31:ca:ea:d3:a9:88:8c:8f:38:c8:c7:91:48:e7:a2)
[2026-09-10T22:11:47Z INFO lan_mouse::listen] server accept_bi: stream C first frame length=78 (> M
AX_EVENT_SIZE=21) from 10.2.1.15:50247, reading inline + spawning stream C reader
[2026-09-10T22:11:47Z INFO lan_mouse::listen] server accept_bi: stream C first frame from 10.2.1.15
:50247: ClipboardText(fp=e3b0c442, sha=e3b0c442, size=0, inline=yes)
[2026-09-10T22:11:51Z INFO lan_mouse::emulation] releasing capture: 10.2.1.15:50247 entered this de
vice (fp=a4:9b:47:25:50:3d:17:4f:b2:64:59:95:f8:a4:4d:ff:0c:31:ca:ea:d3:a9:88:8c:8f:38:c8:c7:91:48:e
7:a2)
[2026-09-10T22:11:51Z INFO lan_mouse::emulation] emulation: sending Ack(0) to 10.2.1.15:50247 (resp
onding to master Enter)
[2026-09-10T22:11:51Z INFO lan_mouse::quic_transport::session] send_input: routing Ack(0) via Strea
mA (entry; awaiting send)
[2026-09-10T22:11:51Z INFO lan_mouse::quic_transport::session] send_input: Ack(0) via StreamA retur
ned (ok=true)
[2026-09-10T22:11:51Z INFO lan_mouse::listen] reply: Ack(0) to 10.2.1.15:50247 delivered
[2026-09-10T22:11:51Z INFO lan_mouse::capture] release_capture: ENTER (state=Idle, active_client=No
ne)
[2026-09-10T22:11:51Z INFO lan_mouse::emulation] releasing capture: 10.2.1.15:50247 entered this de
vice (fp=a4:9b:47:25:50:3d:17:4f:b2:64:59:95:f8:a4:4d:ff:0c:31:ca:ea:d3:a9:88:8c:8f:38:c8:c7:91:48:e
7:a2)
[2026-09-10T22:11:51Z INFO lan_mouse::emulation] emulation: sending Ack(0) to 10.2.1.15:50247 (resp
onding to master Enter)
[2026-09-10T22:11:51Z INFO lan_mouse::quic_transport::session] send_input: routing Ack(0) via Strea
mA (entry; awaiting send)
[2026-09-10T22:11:51Z INFO lan_mouse::quic_transport::session] send_input: Ack(0) via StreamA retur
ned (ok=true)
[2026-09-10T22:11:51Z INFO lan_mouse::listen] reply: Ack(0) to 10.2.1.15:50247 delivered
[2026-09-10T22:11:51Z INFO lan_mouse::capture] release_capture: ENTER (state=Idle, active_client=No
ne)
[2026-09-10T22:11:51Z INFO input_capture::windows::event_thread] monitors changed: 1 monitor(s)
[2026-09-10T22:11:51Z INFO input_capture::windows::event_thread] monitor: id=windows:unknown-\\.\
DISPLAY1 name="Intel(R) UHD Graphics" pos=(0, 0) size=(1280, 800) primary=true scale=1
[2026-09-10T22:11:51Z INFO lan_mouse::capture] capture: EnterOnly trigger on 9223372036854775808 (e
vent=BeginPending) — forwarding to service as CaptureBegin
[2026-09-10T22:11:51Z INFO lan_mouse::capture] releasing capture: no active client at this position
[2026-09-10T22:11:51Z INFO lan_mouse::quic_transport::session] send_input: routing Leave(0) via Str
eamA (entry; awaiting send)
[2026-09-10T22:11:51Z INFO lan_mouse::quic_transport::session] send_input: Leave(0) via StreamA ret
urned (ok=true)
[2026-09-10T22:11:51Z INFO lan_mouse::listen] reply: Leave(0) to 10.2.1.15:50247 delivered
[2026-09-10T22:11:52Z INFO lan_mouse::emulation] emulation: received Leave from 10.2.1.15:50247 — r
emoving emulation_proxy
[2026-09-10T22:11:52Z INFO lan_mouse::quic_transport::session] send_input: routing Ack(0) via Strea
mA (entry; awaiting send)
[2026-09-10T22:11:52Z INFO lan_mouse::quic_transport::session] send_input: Ack(0) via StreamA retur
ned (ok=true)
[2026-09-10T22:11:52Z INFO lan_mouse::listen] reply: Ack(0) to 10.2.1.15:50247 delivered
[2026-09-10T22:11:52Z INFO lan_mouse::capture] capture: monitor list changed (0 → 1)
[2026-09-10T22:11:55Z INFO lan_mouse::emulation] releasing capture: 10.2.1.15:50247 entered this de
vice (fp=a4:9b:47:25:50:3d:17:4f:b2:64:59:95:f8:a4:4d:ff:0c:31:ca:ea:d3:a9:88:8c:8f:38:c8:c7:91:48:e
7:a2)
[2026-09-10T22:11:55Z INFO lan_mouse::emulation] emulation: sending Ack(0) to 10.2.1.15:50247 (resp
onding to master Enter)
[2026-09-10T22:11:55Z INFO lan_mouse::quic_transport::session] send_input: routing Ack(0) via Strea
mA (entry; awaiting send)
[2026-09-10T22:11:55Z INFO lan_mouse::quic_transport::session] send_input: Ack(0) via StreamA retur
ned (ok=true)
[2026-09-10T22:11:55Z INFO lan_mouse::listen] reply: Ack(0) to 10.2.1.15:50247 delivered
[2026-09-10T22:11:55Z INFO lan_mouse::capture] release_capture: ENTER (state=Idle, active_client=No
ne)
[2026-09-10T22:11:56Z INFO lan_mouse::capture] capture: EnterOnly trigger on 9223372036854775808 (e
vent=BeginPending) — forwarding to service as CaptureBegin
[2026-09-10T22:11:56Z INFO lan_mouse::capture] releasing capture: no active client at this position
[2026-09-10T22:11:56Z INFO lan_mouse::quic_transport::session] send_input: routing Leave(0) via Str
eamA (entry; awaiting send)
[2026-09-10T22:11:56Z INFO lan_mouse::quic_transport::session] send_input: Leave(0) via StreamA ret
urned (ok=true)
[2026-09-10T22:11:56Z INFO lan_mouse::listen] reply: Leave(0) to 10.2.1.15:50247 delivered
[2026-09-10T22:11:56Z INFO lan_mouse::emulation] emulation: received Leave from 10.2.1.15:50247 — r
emoving emulation_proxy
[2026-09-10T22:11:56Z INFO lan_mouse::quic_transport::session] send_input: routing Ack(0) via Strea
mA (entry; awaiting send)
[2026-09-10T22:11:56Z INFO lan_mouse::quic_transport::session] send_input: Ack(0) via StreamA retur
ned (ok=true)
[2026-09-10T22:11:56Z INFO lan_mouse::listen] reply: Ack(0) to 10.2.1.15:50247 delivered
[2026-09-10T22:11:58Z INFO lan_mouse::listen] server stream C reader: from 10.2.1.15:50247: Clipboa
rdImage(fp=21595697, sha=21595697, size=7245, mime=image/png)
[2026-09-10T22:11:58Z INFO lan_mouse::service] clipboard inbound image: pulled 7245 bytes from 10.2
.1.15:50247 via HTTP/3 (sha=21595697, mime=image/png)
[2026-09-10T22:11:58Z INFO lan_mouse::service] clipboard inbound image: applied 7245 bytes from 10.
2.1.15:50247 (sha=21595697, mime=image/png)
[2026-09-10T22:11:59Z INFO lan_mouse::emulation] releasing capture: 10.2.1.15:50247 entered this de
vice (fp=a4:9b:47:25:50:3d:17:4f:b2:64:59:95:f8:a4:4d:ff:0c:31:ca:ea:d3:a9:88:8c:8f:38:c8:c7:91:48:e
7:a2)
[2026-09-10T22:11:59Z INFO lan_mouse::emulation] emulation: sending Ack(0) to 10.2.1.15:50247 (resp
onding to master Enter)
[2026-09-10T22:11:59Z INFO lan_mouse::quic_transport::session] send_input: routing Ack(0) via Strea
mA (entry; awaiting send)
[2026-09-10T22:11:59Z INFO lan_mouse::quic_transport::session] send_input: Ack(0) via StreamA retur
ned (ok=true)
[2026-09-10T22:11:59Z INFO lan_mouse::listen] reply: Ack(0) to 10.2.1.15:50247 delivered
[2026-09-10T22:11:59Z INFO lan_mouse::capture] release_capture: ENTER (state=Idle, active_client=No
ne)
[2026-09-10T22:12:00Z INFO lan_mouse::capture] capture: EnterOnly trigger on 9223372036854775808 (e
vent=BeginPending) — forwarding to service as CaptureBegin
[2026-09-10T22:12:00Z INFO lan_mouse::capture] releasing capture: no active client at this position
[2026-09-10T22:12:00Z INFO lan_mouse::quic_transport::session] send_input: routing Leave(0) via Str
eamA (entry; awaiting send)
[2026-09-10T22:12:00Z INFO lan_mouse::quic_transport::session] send_input: Leave(0) via StreamA ret
urned (ok=true)
[2026-09-10T22:12:00Z INFO lan_mouse::listen] reply: Leave(0) to 10.2.1.15:50247 delivered
[2026-09-10T22:12:00Z INFO lan_mouse::emulation] emulation: received Leave from 10.2.1.15:50247 — r
emoving emulation_proxy
[2026-09-10T22:12:00Z INFO lan_mouse::quic_transport::session] send_input: routing Ack(0) via Strea
mA (entry; awaiting send)
[2026-09-10T22:12:00Z INFO lan_mouse::quic_transport::session] send_input: Ack(0) via StreamA retur
ned (ok=true)
[2026-09-10T22:12:00Z INFO lan_mouse::listen] reply: Ack(0) to 10.2.1.15:50247 delivered
[2026-09-10T22:12:02Z INFO lan_mouse::emulation] releasing capture: 10.2.1.15:50247 entered this de
vice (fp=a4:9b:47:25:50:3d:17:4f:b2:64:59:95:f8:a4:4d:ff:0c:31:ca:ea:d3:a9:88:8c:8f:38:c8:c7:91:48:e
7:a2)
[2026-09-10T22:12:02Z INFO lan_mouse::emulation] emulation: sending Ack(0) to 10.2.1.15:50247 (resp
onding to master Enter)
[2026-09-10T22:12:02Z INFO lan_mouse::quic_transport::session] send_input: routing Ack(0) via Strea
mA (entry; awaiting send)
[2026-09-10T22:12:02Z INFO lan_mouse::quic_transport::session] send_input: Ack(0) via StreamA retur
ned (ok=true)
[2026-09-10T22:12:02Z INFO lan_mouse::listen] reply: Ack(0) to 10.2.1.15:50247 delivered
[2026-09-10T22:12:02Z INFO lan_mouse::capture] release_capture: ENTER (state=Idle, active_client=No
ne)
[2026-09-10T22:12:03Z INFO lan_mouse::emulation] releasing capture: 10.2.1.15:50247 entered this de
vice (fp=a4:9b:47:25:50:3d:17:4f:b2:64:59:95:f8:a4:4d:ff:0c:31:ca:ea:d3:a9:88:8c:8f:38:c8:c7:91:48:e
7:a2)
[2026-09-10T22:12:03Z INFO lan_mouse::emulation] emulation: sending Ack(0) to 10.2.1.15:50247 (resp
onding to master Enter)
[2026-09-10T22:12:03Z INFO lan_mouse::quic_transport::session] send_input: routing Ack(0) via Strea
mA (entry; awaiting send)
[2026-09-10T22:12:03Z INFO lan_mouse::quic_transport::session] send_input: Ack(0) via StreamA retur
ned (ok=true)
[2026-09-10T22:12:03Z INFO lan_mouse::listen] reply: Ack(0) to 10.2.1.15:50247 delivered
[2026-09-10T22:12:03Z INFO lan_mouse::capture] release_capture: ENTER (state=Idle, active_client=No
ne)
[2026-09-10T22:12:03Z INFO lan_mouse::capture] capture: EnterOnly trigger on 9223372036854775808 (e
vent=BeginPending) — forwarding to service as CaptureBegin
[2026-09-10T22:12:03Z INFO lan_mouse::capture] releasing capture: no active client at this position
[2026-09-10T22:12:03Z INFO lan_mouse::quic_transport::session] send_input: routing Leave(0) via Str
eamA (entry; awaiting send)
[2026-09-10T22:12:03Z INFO lan_mouse::quic_transport::session] send_input: Leave(0) via StreamA ret
urned (ok=true)
[2026-09-10T22:12:03Z INFO lan_mouse::listen] reply: Leave(0) to 10.2.1.15:50247 delivered
[2026-09-10T22:12:03Z INFO lan_mouse::emulation] emulation: received Leave from 10.2.1.15:50247 — r
emoving emulation_proxy
[2026-09-10T22:12:03Z INFO lan_mouse::quic_transport::session] send_input: routing Ack(0) via Strea
mA (entry; awaiting send)
[2026-09-10T22:12:03Z INFO lan_mouse::quic_transport::session] send_input: Ack(0) via StreamA retur
ned (ok=true)
[2026-09-10T22:12:03Z INFO lan_mouse::listen] reply: Ack(0) to 10.2.1.15:50247 delivered
[2026-09-10T22:12:10Z INFO lan_mouse::listen] server stream C reader: from 10.2.1.15:50247: Clipboa
rdImage(fp=2c37a096, sha=2c37a096, size=4264029, mime=image/png)
[2026-09-10T22:12:10Z INFO lan_mouse::emulation] releasing capture: 10.2.1.15:50247 entered this de
vice (fp=a4:9b:47:25:50:3d:17:4f:b2:64:59:95:f8:a4:4d:ff:0c:31:ca:ea:d3:a9:88:8c:8f:38:c8:c7:91:48:e
7:a2)
[2026-09-10T22:12:10Z INFO lan_mouse::emulation] emulation: sending Ack(0) to 10.2.1.15:50247 (resp
onding to master Enter)
[2026-09-10T22:12:10Z INFO lan_mouse::quic_transport::session] send_input: routing Ack(0) via Strea
mA (entry; awaiting send)
[2026-09-10T22:12:10Z INFO lan_mouse::quic_transport::session] send_input: Ack(0) via StreamA retur
ned (ok=true)
[2026-09-10T22:12:10Z INFO lan_mouse::listen] reply: Ack(0) to 10.2.1.15:50247 delivered
[2026-09-10T22:12:10Z INFO lan_mouse::emulation] releasing capture: 10.2.1.15:50247 entered this de
vice (fp=a4:9b:47:25:50:3d:17:4f:b2:64:59:95:f8:a4:4d:ff:0c:31:ca:ea:d3:a9:88:8c:8f:38:c8:c7:91:48:e
7:a2)
[2026-09-10T22:12:10Z INFO lan_mouse::emulation] emulation: sending Ack(0) to 10.2.1.15:50247 (resp
onding to master Enter)
[2026-09-10T22:12:10Z INFO lan_mouse::quic_transport::session] send_input: routing Ack(0) via Strea
mA (entry; awaiting send)
[2026-09-10T22:12:10Z INFO lan_mouse::quic_transport::session] send_input: Ack(0) via StreamA retur
ned (ok=true)
[2026-09-10T22:12:10Z INFO lan_mouse::listen] reply: Ack(0) to 10.2.1.15:50247 delivered
[2026-09-10T22:12:10Z INFO lan_mouse::capture] release_capture: ENTER (state=Idle, active_client=No
ne)
[2026-09-10T22:12:10Z WARN lan_mouse::service] clipboard inbound image: HTTP/3 GET /clipboard/image
/2c37a096da0a5dcedc3f278cb01240b97f0f26986065b8a9b30486142f5f03c6 from 10.2.1.15:50247 failed: conne
ction lost — skipping
[2026-09-10T22:12:10Z INFO lan_mouse::listen] stream A reader exiting (IO closed): hello handshake
failed: read frame length: connection lost
[2026-09-10T22:12:10Z WARN lan_mouse::listen] QUIC peer supervisor exited with err: hello handshake
failed: read frame length: connection lost
[2026-09-10T22:12:10Z INFO lan_mouse::listen] server stream reader: stream ended (10.2.1.15:50247):
hello handshake failed: read frame length: connection lost
[2026-09-10T22:12:10Z INFO lan_mouse::listen] server stream C reader: stream ended (10.2.1.15:50247
): hello handshake failed: read stream C length: connection lost
[2026-09-10T22:12:10Z INFO lan_mouse::listen] server accept_bi: exiting (conn closed): closed by pe
er: pong_health_timeout (code 51966)
[2026-09-10T22:12:10Z INFO lan_mouse::listen] server datagram_reader: read_datagram error, exiting:
closed by peer: pong_health_timeout (code 51966)
[2026-09-10T22:12:10Z INFO lan_mouse::capture] release_capture: ENTER (state=Idle, active_client=No
ne)
[2026-09-10T22:12:11Z INFO lan_mouse::quic_transport::tls] AuthorizedKeysVerifier: authorized peer
a4:9b:47:25:50:3d:17:4f:b2:64:59:95:f8:a4:4d:ff:0c:31:ca:ea:d3:a9:88:8c:8f:38:c8:c7:91:48:e7:a2
[2026-09-10T22:12:11Z INFO lan_mouse::listen] QUIC peer connected: 10.2.1.15:50247
[2026-09-10T22:12:11Z INFO lan_mouse::listen] QUIC peer 10.2.1.15:50247 authorized (fingerprint a4:
9b:47:25:50:3d:17:4f:b2:64:59:95:f8:a4:4d:ff:0c:31:ca:ea:d3:a9:88:8c:8f:38:c8:c7:91:48:e7:a2)
[2026-09-10T22:12:11Z INFO lan_mouse::listen] server accept_bi: stream C first frame length=78 (> M
AX_EVENT_SIZE=21) from 10.2.1.15:50247, reading inline + spawning stream C reader
[2026-09-10T22:12:11Z INFO lan_mouse::listen] server accept_bi: stream C first frame from 10.2.1.15
:50247: ClipboardText(fp=e3b0c442, sha=e3b0c442, size=0, inline=yes)
[2026-09-10T22:12:14Z INFO lan_mouse::emulation] releasing capture: 10.2.1.15:50247 entered this de
vice (fp=a4:9b:47:25:50:3d:17:4f:b2:64:59:95:f8:a4:4d:ff:0c:31:ca:ea:d3:a9:88:8c:8f:38:c8:c7:91:48:e
7:a2)
[2026-09-10T22:12:14Z INFO lan_mouse::emulation] emulation: sending Ack(0) to 10.2.1.15:50247 (resp
onding to master Enter)
[2026-09-10T22:12:14Z INFO lan_mouse::quic_transport::session] send_input: routing Ack(0) via Strea
mA (entry; awaiting send)
[2026-09-10T22:12:14Z INFO lan_mouse::quic_transport::session] send_input: Ack(0) via StreamA retur
ned (ok=true)
[2026-09-10T22:12:14Z INFO lan_mouse::listen] reply: Ack(0) to 10.2.1.15:50247 delivered
[2026-09-10T22:12:14Z INFO lan_mouse::capture] release_capture: ENTER (state=Idle, active_client=No
ne)
[2026-09-10T22:12:15Z INFO lan_mouse::capture] capture: EnterOnly trigger on 9223372036854775808 (e
vent=BeginPending) — forwarding to service as CaptureBegin
[2026-09-10T22:12:15Z INFO lan_mouse::capture] releasing capture: no active client at this position
[2026-09-10T22:12:15Z INFO lan_mouse::quic_transport::session] send_input: routing Leave(0) via Str
eamA (entry; awaiting send)
[2026-09-10T22:12:15Z INFO lan_mouse::quic_transport::session] send_input: Leave(0) via StreamA ret
urned (ok=true)
[2026-09-10T22:12:15Z INFO lan_mouse::listen] reply: Leave(0) to 10.2.1.15:50247 delivered
这是主控端日志：
[2026-09-10T22:11:47Z INFO input_capture::macos] initial monitors: 2 monitor(s)
[2026-09-10T22:11:47Z INFO input_capture::macos] monitor: id=macos:0000:0000::unknown-1 name="Display 1" pos=(0, 0) size=(1512, 982) primary=true scale=2
[2026-09-10T22:11:47Z INFO input_capture::macos] monitor: id=macos:0000:0000::unknown-3 name="Display 3" pos=(-233, -1080) size=(1920, 1080) primary=false scale=2
[2026-09-10T22:11:47Z INFO input_capture::macos] Enabling CGEvent tap
[2026-09-10T22:11:47Z INFO input_capture::macos] registered CGDisplay reconfiguration callback on tap thread run loop
[2026-09-10T22:11:47Z INFO input_capture] using capture backend: MacOS
[2026-09-10T22:11:47Z INFO lan_mouse::emulation] creating input emulation ...
[2026-09-10T22:11:47Z INFO input_emulation] using emulation backend: macos
[2026-09-10T22:11:47Z INFO lan_mouse] opening http://127.0.0.1:3939 in the default browser
[2026-09-10T22:11:47Z INFO lan_mouse] using config: "/Users/hb/.config/lan-mouse/config.toml"
[2026-09-10T22:11:47Z INFO lan_mouse] Press [KeyLeftCtrl, KeyLeftShift, KeyLeftMeta, KeyLeftAlt] to release the mouse
[2026-09-10T22:11:47Z INFO lan_mouse::web] lan-mouse web UI listening on http://127.0.0.1:3939
[2026-09-10T22:11:47Z INFO lan_mouse::service] activated client 2 (BarrierKey { pos: Left, monitor: Some("macos:0000:0000::unknown-1"), offset: 0, span: 10000 })
[2026-09-10T22:11:47Z WARN lan_mouse::service] clipboard dispatched to 0 peers (sha=e3b0c442); peer gate filtered all clients — check `enable_clipboard_to` in TOML and that the connection is active
[2026-09-10T22:11:47Z INFO lan_mouse::connect] client 2 connecting ...
[2026-09-10T22:11:47Z INFO lan_mouse::connect] client (2) dial_any ... (primary: 10.2.1.35:2268, candidates: 1)
[2026-09-10T22:11:47Z INFO lan_mouse::connect] client (2) connected @ 10.2.1.35:2268 (quic) — first connection
[2026-09-10T22:11:47Z INFO lan_mouse::connect] spawn_peer_supervisor: starting for handle 2 addr 10.2.1.35:2268
[2026-09-10T22:11:47Z INFO lan_mouse::quic_transport::streams] read_loop: stream B reader spawned (cap=64), stream C reader spawned (M0c STEP-0.5b)
[2026-09-10T22:11:47Z INFO lan_mouse::service] clipboard recover push: peer handle=2 just became active — pushing 0 bytes (sha=e3b0c442)
[2026-09-10T22:11:47Z INFO lan_mouse::service] clipboard recover push: dispatched to 1 peer(s) (sha=e3b0c442)
[2026-09-10T22:11:51Z INFO lan_mouse::capture] capture: BeginPending (handle=2) - awaiting Ack within 500ms
[2026-09-10T22:11:51Z INFO lan_mouse::capture] capture: CancelPending (handle=2)
[2026-09-10T22:11:51Z INFO lan_mouse::capture] capture: BeginPending (handle=2) - awaiting Ack within 500ms
[2026-09-10T22:11:51Z INFO lan_mouse::capture] client 2 acknowledged Enter after 10.254917ms
[2026-09-10T22:11:51Z INFO lan_mouse::capture] capture: pending -> active promotion (handle=2, Begin already Entered for BeginPending)
[2026-09-10T22:11:51Z INFO lan_mouse::service] entering client 2 ...
[2026-09-10T22:11:52Z INFO lan_mouse::capture] releasing capture: left remote client device region
[2026-09-10T22:11:52Z INFO lan_mouse::capture] release_capture: ENTER (state=Sending, active_client=Some(2))
[2026-09-10T22:11:52Z INFO lan_mouse::capture] release_capture: synthesizing 0 key-up events to client 2
[2026-09-10T22:11:52Z INFO lan_mouse::capture] release_capture: sending modifiers=0 to client 2
[2026-09-10T22:11:52Z INFO lan_mouse::capture] release_capture: sending Leave to client 2
[2026-09-10T22:11:52Z INFO lan_mouse::quic_transport::session] send_input: routing Leave(0) via StreamA (entry; awaiting send)
[2026-09-10T22:11:52Z INFO lan_mouse::quic_transport::session] send_input: Leave(0) via StreamA returned (ok=true)
[2026-09-10T22:11:52Z INFO lan_mouse::capture] release_capture: setting state = Idle (force-reset)
[2026-09-10T22:11:52Z INFO lan_mouse::capture] release_capture: calling capture.release() (OS-level release)
[2026-09-10T22:11:52Z INFO lan_mouse::capture] release_capture: capture.release() returned (ok=true)
[2026-09-10T22:11:52Z WARN lan_mouse::capture] capture: Input event arrived while state=Idle — dropping (host capture inactive)
[2026-09-10T22:11:55Z INFO lan_mouse::capture] capture: BeginPending (handle=2) - awaiting Ack within 500ms
[2026-09-10T22:11:55Z INFO lan_mouse::capture] client 2 acknowledged Enter after 11.4965ms
[2026-09-10T22:11:55Z INFO lan_mouse::capture] capture: pending -> active promotion (handle=2, Begin already Entered for BeginPending)
[2026-09-10T22:11:55Z INFO lan_mouse::service] entering client 2 ...
[2026-09-10T22:11:56Z INFO lan_mouse::capture] releasing capture: left remote client device region
[2026-09-10T22:11:56Z INFO lan_mouse::capture] release_capture: ENTER (state=Sending, active_client=Some(2))
[2026-09-10T22:11:56Z INFO lan_mouse::capture] release_capture: synthesizing 0 key-up events to client 2
[2026-09-10T22:11:56Z INFO lan_mouse::capture] release_capture: sending modifiers=0 to client 2
[2026-09-10T22:11:56Z INFO lan_mouse::capture] release_capture: sending Leave to client 2
[2026-09-10T22:11:56Z INFO lan_mouse::quic_transport::session] send_input: routing Leave(0) via StreamA (entry; awaiting send)
[2026-09-10T22:11:56Z INFO lan_mouse::quic_transport::session] send_input: Leave(0) via StreamA returned (ok=true)
[2026-09-10T22:11:56Z INFO lan_mouse::capture] release_capture: setting state = Idle (force-reset)
[2026-09-10T22:11:56Z INFO lan_mouse::capture] release_capture: calling capture.release() (OS-level release)
[2026-09-10T22:11:56Z INFO lan_mouse::capture] release_capture: capture.release() returned (ok=true)
[2026-09-10T22:11:58Z INFO lan_mouse::clipboard::macos] clipboard: JPEG→PNG normalized for cross-platform transfer (7477 bytes → 7245 bytes)
[2026-09-10T22:11:58Z INFO lan_mouse::service] clipboard dispatched image (7245 bytes, mime=image/png, sha=21595697) to 1 peer(s)
[2026-09-10T22:11:59Z INFO lan_mouse::capture] capture: BeginPending (handle=2) - awaiting Ack within 500ms
[2026-09-10T22:11:59Z INFO lan_mouse::capture] client 2 acknowledged Enter after 11.014ms
[2026-09-10T22:11:59Z INFO lan_mouse::capture] capture: pending -> active promotion (handle=2, Begin already Entered for BeginPending)
[2026-09-10T22:11:59Z INFO lan_mouse::service] entering client 2 ...
[2026-09-10T22:12:00Z INFO lan_mouse::capture] releasing capture: left remote client device region
[2026-09-10T22:12:00Z INFO lan_mouse::capture] release_capture: ENTER (state=Sending, active_client=Some(2))
[2026-09-10T22:12:00Z INFO lan_mouse::capture] release_capture: synthesizing 0 key-up events to client 2
[2026-09-10T22:12:00Z INFO lan_mouse::capture] release_capture: sending modifiers=0 to client 2
[2026-09-10T22:12:00Z INFO lan_mouse::capture] release_capture: sending Leave to client 2
[2026-09-10T22:12:00Z INFO lan_mouse::quic_transport::session] send_input: routing Leave(0) via StreamA (entry; awaiting send)
[2026-09-10T22:12:00Z INFO lan_mouse::quic_transport::session] send_input: Leave(0) via StreamA returned (ok=true)
[2026-09-10T22:12:00Z INFO lan_mouse::capture] release_capture: setting state = Idle (force-reset)
[2026-09-10T22:12:00Z INFO lan_mouse::capture] release_capture: calling capture.release() (OS-level release)
[2026-09-10T22:12:00Z INFO lan_mouse::capture] release_capture: capture.release() returned (ok=true)
[2026-09-10T22:12:02Z INFO lan_mouse::capture] capture: BeginPending (handle=2) - awaiting Ack within 500ms
[2026-09-10T22:12:03Z INFO lan_mouse::capture] capture: CancelPending (handle=2)
[2026-09-10T22:12:03Z INFO lan_mouse::capture] capture: BeginPending (handle=2) - awaiting Ack within 500ms
[2026-09-10T22:12:03Z INFO lan_mouse::capture] client 2 acknowledged Enter after 7.031125ms
[2026-09-10T22:12:03Z INFO lan_mouse::capture] capture: pending -> active promotion (handle=2, Begin already Entered for BeginPending)
[2026-09-10T22:12:03Z INFO lan_mouse::service] entering client 2 ...
[2026-09-10T22:12:03Z INFO lan_mouse::capture] releasing capture: left remote client device region
[2026-09-10T22:12:03Z INFO lan_mouse::capture] release_capture: ENTER (state=Sending, active_client=Some(2))
[2026-09-10T22:12:03Z INFO lan_mouse::capture] release_capture: synthesizing 0 key-up events to client 2
[2026-09-10T22:12:03Z INFO lan_mouse::capture] release_capture: sending modifiers=0 to client 2
[2026-09-10T22:12:03Z INFO lan_mouse::capture] release_capture: sending Leave to client 2
[2026-09-10T22:12:03Z INFO lan_mouse::quic_transport::session] send_input: routing Leave(0) via StreamA (entry; awaiting send)
[2026-09-10T22:12:03Z INFO lan_mouse::quic_transport::session] send_input: Leave(0) via StreamA returned (ok=true)
[2026-09-10T22:12:03Z INFO lan_mouse::capture] release_capture: setting state = Idle (force-reset)
[2026-09-10T22:12:03Z INFO lan_mouse::capture] release_capture: calling capture.release() (OS-level release)
[2026-09-10T22:12:03Z INFO lan_mouse::capture] release_capture: capture.release() returned (ok=true)
[2026-09-10T22:12:10Z INFO lan_mouse::clipboard::macos] clipboard: JPEG→PNG normalized for cross-platform transfer (1264213 bytes → 4264029 bytes)
[2026-09-10T22:12:10Z INFO lan_mouse::service] clipboard dispatched image (4264029 bytes, mime=image/png, sha=2c37a096) to 1 peer(s)
[2026-09-10T22:12:10Z INFO lan_mouse::capture] capture: BeginPending (handle=2) - awaiting Ack within 500ms
[2026-09-10T22:12:10Z INFO lan_mouse::capture] capture: CancelPending (handle=2)
[2026-09-10T22:12:10Z INFO lan_mouse::capture] capture: BeginPending (handle=2) - awaiting Ack within 500ms
[2026-09-10T22:12:10Z INFO lan_mouse::capture] capture: CancelPending (handle=2)
[2026-09-10T22:12:10Z INFO lan_mouse::capture] capture: BeginPending (handle=2) - awaiting Ack within 500ms
[2026-09-10T22:12:10Z WARN lan_mouse::connect] Pong health watchdog: peer 10.2.1.35:2268 hasn't responded in 3.350630333s (> 1.5s) — force-closing with WAKE_CLOSE_CODE + notifying capture
[2026-09-10T22:12:10Z INFO lan_mouse::quic_transport::session] run: conn.closed() fired: LocallyClosed
[2026-09-10T22:12:10Z INFO lan_mouse::quic_transport::session] peer.run(Client) exiting with close reason: LocallyClosed
[2026-09-10T22:12:10Z INFO lan_mouse::connect] spawn_peer_supervisor: peer.run() returned for handle 2 addr 10.2.1.35:2268
[2026-09-10T22:12:10Z INFO lan_mouse::connect] client (2) conn 10.2.1.35:2268 closed gracefully: LocallyClosed — no retry
[2026-09-10T22:12:10Z INFO lan_mouse::quic_transport::streams] stream B reader exiting (IO closed): hello handshake failed: read frame length: connection lost
[2026-09-10T22:12:10Z INFO lan_mouse::quic_transport::streams] stream C reader exiting (IO closed): hello handshake failed: read stream C length: connection lost
[2026-09-10T22:12:10Z INFO lan_mouse::connect] client accept_bi: exiting (conn closed): closed
[2026-09-10T22:12:10Z INFO lan_mouse::quic_transport::streams] datagram_reader: read_datagram error, exiting: closed
[2026-09-10T22:12:10Z INFO lan_mouse::capture] capture: PeerLost(handle=2) from pong health watchdog — releasing capture
[2026-09-10T22:12:10Z INFO lan_mouse::capture] releasing capture: left remote client device region
[2026-09-10T22:12:10Z INFO lan_mouse::capture] release_capture: ENTER (state=Pending { handle: 2, key: BarrierKey { pos: Left, monitor: Some("macos:0000:0000::unknown-1"), offset: 0, span: 10000 }, started: Instant { tv_sec: 715793, tv_nsec: 875329041 } }, active_client=None)
[2026-09-10T22:12:10Z INFO lan_mouse::capture] release_capture: was in Pending for handle 2 - cancel_pending (no Leave to send)
[2026-09-10T22:12:10Z WARN lan_mouse::connect] stream A forwarder: outgoing_events rx closed — forwarder exiting
[2026-09-10T22:12:10Z INFO lan_mouse::capture] capture: CancelPending (handle=2)
[2026-09-10T22:12:11Z INFO lan_mouse::capture] capture: BeginPending (handle=2) - awaiting Ack within 500ms
[2026-09-10T22:12:11Z WARN lan_mouse::capture] releasing capture: BeginPending send failed: not connected (cancelling pending, host cursor stays visible)
[2026-09-10T22:12:11Z INFO lan_mouse::connect] client 2 connecting ...
[2026-09-10T22:12:11Z INFO lan_mouse::connect] client (2) dial_any ... (primary: 10.2.1.35:2268, candidates: 1)
[2026-09-10T22:12:11Z INFO lan_mouse::capture] capture: CancelPending (handle=2)
[2026-09-10T22:12:11Z INFO lan_mouse::capture] capture: BeginPending (handle=2) - awaiting Ack within 500ms
[2026-09-10T22:12:11Z WARN lan_mouse::capture] releasing capture: BeginPending send failed: not connected (cancelling pending, host cursor stays visible)
[2026-09-10T22:12:11Z INFO lan_mouse::capture] capture: CancelPending (handle=2)
[2026-09-10T22:12:11Z INFO lan_mouse::capture] capture: BeginPending (handle=2) - awaiting Ack within 500ms
[2026-09-10T22:12:11Z WARN lan_mouse::capture] releasing capture: BeginPending send failed: not connected (cancelling pending, host cursor stays visible)
[2026-09-10T22:12:11Z INFO lan_mouse::capture] capture: CancelPending (handle=2)
[2026-09-10T22:12:11Z INFO lan_mouse::connect] client (2) connected @ 10.2.1.35:2268 (quic) — first connection
[2026-09-10T22:12:11Z INFO lan_mouse::connect] spawn_peer_supervisor: starting for handle 2 addr 10.2.1.35:2268
[2026-09-10T22:12:11Z INFO lan_mouse::quic_transport::streams] read_loop: stream B reader spawned (cap=64), stream C reader spawned (M0c STEP-0.5b)
[2026-09-10T22:12:11Z INFO lan_mouse::service] clipboard recover push: peer handle=2 just became active — pushing 0 bytes (sha=e3b0c442)
[2026-09-10T22:12:11Z INFO lan_mouse::service] clipboard recover push: dispatched to 1 peer(s) (sha=e3b0c442)
[2026-09-10T22:12:14Z INFO lan_mouse::capture] capture: BeginPending (handle=2) - awaiting Ack within 500ms
[2026-09-10T22:12:14Z INFO lan_mouse::capture] client 2 acknowledged Enter after 12.486875ms
[2026-09-10T22:12:14Z INFO lan_mouse::capture] capture: pending -> active promotion (handle=2, Begin already Entered for BeginPending)
[2026-09-10T22:12:14Z INFO lan_mouse::service] entering client 2 ...
日志中有两次截图，第一次是小图，是好的，第二次是大图，卡顿。

方案 4a（macOS JPEG passthrough 砍掉 PNG encoding 本身）是什么，我发现切换为png模式截大图后，更加卡，而且同样无法粘贴

---

### 启动顺序敏感：先 master 后 slave、鼠标未跨，第一次复制截图被控端粘贴失败

**现状**（用户 2026-09-10 实测，长期存在）：

- **触发条件**（5 个全要满足，缺一个就正常）：
  1. 先开主控端
  2. 再开被控端
  3. 鼠标**未跨越边界**（始终停在主控端）
  4. 用户**第一次**复制截图
  5. 被控端尝试**粘贴**该截图
- **症状**：被控端剪贴板看上去有内容（slave 日志明确打 `clipboard inbound image: applied 77100 bytes`），但用户实际粘贴**失败**（不出现图、不出现文件名、目标应用无反应）。
- **回避条件**：同样的截图**第二次**复制 → 粘贴成功。任何一个前提被打破（先 slave 后 master / 复制前先跨越鼠标 / 复制文本而不是截图 / 粘贴目标是纯文本框）都不重现。

**日志样本**（2026-09-10，用户实测）：

master：

```
14:32:23Z clipboard: JPEG→PNG normalized for cross-platform transfer (127751 bytes → 391350 bytes)
14:32:23Z clipboard dispatched image to 0 peers (sha=9a12bddc, mime=image/png, size=391350 bytes)
14:32:35Z clipboard: JPEG→PNG normalized for cross-platform transfer (32631 bytes → 87535 bytes)
14:32:35Z clipboard dispatched image to 0 peers (sha=d7f6be48, mime=image/png, size=87535 bytes)
14:32:41Z clipboard recover push: peer handle=2 just became active — pushing 0 bytes (sha=e3b0c442)
14:32:41Z clipboard recover push: dispatched to 1 peer(s) (sha=e3b0c442)
14:32:50Z clipboard: JPEG→PNG normalized for cross-platform transfer (29503 bytes → 77100 bytes)
14:32:50Z clipboard dispatched image (77100 bytes, mime=image/png, sha=53894182) to 1 peer(s)
14:32:50Z capture: BeginPending (handle=2) - awaiting Ack within 500ms
14:32:50Z capture: client 2 acknowledged Enter after 9.688833ms
14:32:50Z capture: pending -> active promotion (handle=2, Begin already Entered for BeginPending)
14:32:50Z service: entering client 2 ...
```

slave（时钟快约 2 秒）：

```
14:32:39Z service: clipboard inbound: applied 0 bytes from 10.2.1.15:62687 (sha=e3b0c442)
14:32:43Z capture: EnterOnly trigger on 9223372036854775808 (event=BeginPending) — forwarding to service as CaptureBegin
14:32:43Z capture: releasing capture: no active client at this position
14:32:48Z server stream C reader: from 10.2.1.15:62687: ClipboardImage(fp=53894182, sha=53894182, size=77100, mime=image/png)
14:32:48Z emulation: releasing capture: 10.2.1.15:62687 entered this device (...)
14:32:48Z emulation: emulation: sending Ack(0) to 10.2.1.15:62687 (responding to master Enter)
14:32:48Z service: clipboard inbound image: pulled 77100 bytes from 10.2.1.15:62687 via HTTP/3 (sha=53894182, mime=image/png)
14:32:48Z service: clipboard inbound image: applied 77100 bytes from 10.2.1.15:62687 (sha=53894182, mime=image/png)
14:32:48Z capture: release_capture: ENTER (state=Idle, active_client=None)
```

注意：slave 的 `applied 77100 bytes` 打出来后**没有**任何后续 overwrite 日志（既没有 `clipboard dispatched` echo，也没有 `set_text`，更没有其他 `EmptyClipboard` 调用）；用户的 paste 工具仍读不到有效数据。

**已尝试的修复（均未解决问题）**：

- `b4191d4`（流优先级 + macOS changeCount 缓存）—— 用户实测后 bug 仍在
- `0d5cd3f`（RGBA → 24-bit BI_RGB 折叠）—— 已被用户 revert（`a353255`）

**排查过的代码路径**（2026-09-10 调研，未找到根因，记录下来避免重复挖）：

1. **master outbound**：`dispatch_image`（`service.rs:2075-2176`）步骤正常——SHA 计算 / `image_lru_fingerprints` 循环检查（空）/ `last_outbound_image_sha` 比对（None）/ cache insert / `broadcast_clipboard_event` / 更新 `last_outbound_image_sha`。日志确认 77100 bytes 真推过去了（"to 1 peer(s)"）。
2. **slave inbound**：`handle_clipboard_inbound_image`（`service.rs:2332-2392`）→ `apply_inbound_clipboard_image`（`service.rs:2537-2582`）→ `apply_inbound_image_bytes` → Windows `set_image(Mime::Png)` → `write_dibv5_from_png` → `encode_png_to_dib`（`clipboard/windows.rs:646-668`，`image` crate 解 PNG → 重编 BMP → 去 14 字节文件头）→ `set_dib_image`（`clipboard/windows.rs:442-504`）走 `OpenClipboard` → `EmptyClipboard`（73ab5da 修复） → `SetClipboardData(CF_DIBV5)` + `SetClipboardData(CF_DIB)` → `CloseClipboard`。日志确认 `applied 77100 bytes`，说明 `set_dib_image` 返回 Ok。
3. **slave 侧 apply 后覆盖检查**：`apply_inbound_clipboard_image` 之后的 step 4（`notify_frontend`）只往 IPC 推 `ClipboardState`，不碰剪贴板；`EmulationEvent::ReleaseNotify` 触发的 `self.capture.release()` → `release_capture`（`capture.rs:1606-`）在 `state=Idle + active_client=None` 时走 `should_skip_release` 早返回分支，只清 watchdog 字段，不调 `capture.release()`，对剪贴板无副作用。
4. **LRU / cache 状态**：master 的 `image_lru_fingerprints` 在 outbound 路径上**不**被 mark（只有 inbound `apply_inbound_clipboard_image` 会 mark），所以第一次 `dispatch_image` 的 LRU 检查必然 miss、正常 broadcast。
5. **可能的 echo**：`apply_inbound_clipboard_image` 的 step 2.5（`current_image()` re-read 拿 post-transcode SHA）若 `OpenClipboard` 在 `CloseClipboard` 紧接其后的窗口里失败 → `current_image()` 返回 `None` → post-transcode SHA 不进 LRU → slave 下一次 tick 把 DIB echo 回 master。但这一 echo 不会让 slave 自己的剪贴板变空，paste 失败的根因不在这里。

**当前最有可能的几个候选根因**（按概率排序，待用 trace 日志区分）：

- **C1：粘贴目标读的不是 CF_DIBV5/CF_DIB**——某些 Windows 应用（老版本 Office / 部分截图工具 / WPS / 部分 IM）只读 `CF_BITMAP`（GDI handle）或不读 CF\_\*v5。如果应用方是这一类，"applied" 日志跟 paste 失败就不矛盾。
- **C2：DIB 字节格式跟应用期望不匹配**——RGBA PNG → `image` crate 32-bit BMP → BI_BITFIELDS 压缩 + bit-shift bug（0d5cd3f revert 后的已知遗留问题，见 `clipboard/windows.rs:639-645` 的"Known limitation"段）。如果两次截图 alpha 状态不同（第一次有 alpha、第二次无），可以解释"第一次失败第二次成功"。
- **C3：被控端有剪贴板管理器**（Ditto / ClipboardMaster 等）抢走 lan-mouse 写入的 DIB、换成自家格式。用户实际粘贴时读到的是管理器重写过的内容，第一次跨越因为 cache 还没预热、第二次就稳定了。

**当前 workaround**：在 master 上**再复制一次**任何内容（包括原图本身），触发 `dispatch_image` 正常路径，slave 收到新 SHA 覆盖写入 → 第二次 paste 成功。

**下一步诊断需要**（用户复现时给出）：

1. **paste 目标应用**（记事本 / Paint / 微信 / QQ / 浏览器 / 截图工具 / WPS / 其他）。不同的目标读不同的 CF\_\* 格式，能直接锁定 C1。
2. **`RUST_LOG=lan_mouse::clipboard=trace` 完整 slave 日志**（含"applied"前后 2 秒），特别是：
   - step 2.5 那条 debug 日志 `clipboard inbound image: backend transcoded (inbound sha=… → on-clipboard sha=…, mime=…)` 有没有打 → 能确认 re-read 是否成功
   - 紧接的 tick 有没有再打 `clipboard dispatched image (X bytes) to N peer(s)` → 能确认 echo 是否在发生
3. **第一次失败的截图 vs 第二次成功的截图的像素差异**（同样尺寸 / 同样窗口 / 同样有 alpha）。如果两次截图 alpha 状态一致，C2 可以排除。
4. **被控端有没有装剪贴板管理器 / 截图工具 / WPS 类软件**。装了的话 C3 概率上升。

---

### Recovery push 不处理图片分支（peer 刚连上时图片被吞）

**现状**：`handle_clipboard_recover_push`（`src/service.rs:2650-2717`）在 peer 由
inactive→active 转换时只读 `current_text()`，**完全不读 `current_image()`**：

```rust
let new_text = match backend.current_text() {
    Some(t) => t,
    None => return,
};
```

更隐蔽的是 macOS 后端的非对称语义：image-only pasteboard 下
`current_text()` 不会返回 `None`（剪贴板没文本），而是返回 `Some("")`
（空串）——见 `clipboard/macos.rs:1287` 那条测试 pin（`pbpaste` 在只有
PNG/JPEG 的 pasteboard 上 exit=0 + 0 byte stdout）。

组合起来：当 master 在 peer 连上之前**复制过图片**（用户拷了截图但
slave 还没启动），然后 slave 连上，master 会 push **0字节空文本**
（`sha=e3b0c442`）给 slave，把 slave 上原本可能有用的内容覆盖成空。
被复制的图片完全没被传过去。

复现路径：

1. master 启动，复制一张截图（slave 未启动）
2. 启动 slave，连接成功
3. master 日志看到 `clipboard recover push: pushing 0 bytes (sha=e3b0c442)`
4. slave 日志看到 `clipboard inbound: applied 0 bytes ...`
5. 用户的截图没有同步到 slave —— 用户必须**再复制一次**才会走
   `dispatch_image` 把图发过去

**期望**：recovery push 在文本分支之外补一个 image 分支，复用
`dispatch_image` 已有的 LRU + SHA + cache 写入逻辑；image-only
pasteboard 时也走 image 分支（不要被"pbpaste 返回空串"误导走文本路径）。

**当前代码状态**：

- ✅ `dispatch_image` 完整（`service.rs:2075-2176`）—— LRU / last_outbound_image_sha /
  cache insert / FrontendEvent 都已具备
- ❌ `handle_clipboard_recover_push` 只调 `current_text`，没调 `current_image`

**修复路径**：

1. `handle_clipboard_recover_push` 入口先 `current_image()`，命中就走
   `dispatch_image` 同样的广播 + cache 流程（建议直接复用 `dispatch_image`
   而不是 copy-paste，避免两份逻辑漂移）
2. 文本分支保留但加 guard：仅在 `current_image() == None` 时才考虑文本，
   避免 macOS image-only pasteboard 把 0字节空串推过去
3. 同步把 `clipboard_lru` / `image_lru_fingerprints` 的 mark 时机对齐——
   recover push 路径要走和 dispatch 一致的"先 mark 再 push"顺序，避免下一次
   tick 的 LRU 命中漏掉本次推送
4. 加单元测试：macOS image-only + recover push 应广播 `ClipboardImage`
   不是 `ClipboardText{""}`（参考 `clipboard/macos.rs:1287` 那条测试的写法）

**当前 workaround**：用户连上后**主动再复制一次**截图，触发普通
`dispatch_image` 路径，正常推送。这一 BUG 不影响日常使用（多数用户复制
截图的频率高于连/断频率），但会在"先复制后连接"的启动顺序下让首次同步
的图静默丢失。

**日志样本**（2026-09-10 用户复现，master 端）：

```
14:32:41Z clipboard recover push: peer master handle=2 just became active — pushing 0 bytes (sha=e3b0c442)
14:32:41Z clipboard recover push: dispatched to 1 peer(s) (sha=e3b0c442)
```

同一 master 当时的剪贴板实际是一张 PNG（29503→77100 bytes 的 JPEG→PNG 归一化
产物，sha=53894182），但 recover push 没把它推下去。
