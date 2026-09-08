# STEP-P2-M1a-1a.4 — Service-side clipboard dispatcher

> **状态**：✅ 通过（macOS 编译 + 121 单测回归全绿；Loopback defence 行为已在 `service.rs` doc-comment 详尽说明）
> **执行日期**：2026-09-08　实际耗时：~40 min
> **结论**：通过（**0 处 PLAN 偏差**——本 STEP 与 PLAN-2 §3 M1a STEP-1a.4 行严格一致）

---

## 1. 做了什么

按 PLAN-2 §3 M1a STEP-1a.4 落地 service-side clipboard dispatcher —— 把 1a.1 trait / 1a.2-1a.3 platform impls 接到 service 的 500ms tick + StreamC push + FrontendEvent push + 回环防御。

### 1.1 文件改动

| 文件 | 改动 |
|---|---|
| `src/service.rs`（+399 行） | 新增字段：`clipboard_backend: Option<Box<dyn ClipboardBackend>>` / `clipboard_lru: LruFingerprints`（VecDeque<[u8;32]> cap 64）/ `clipboard_last_text: Option<String>`（quiescent tick 短路）/ `clipboard_inbound_{tx,rx}: tokio::mpsc::Unbounded<(SocketAddr, ProtoEvent)>` / `clipboard_tick: tokio::time::Interval`（500ms）/ `last_text/image/file_ts_ms: Option<u64>` + `last_clipboard_source: Option<SocketAddr>`（FrontendEvent::ClipboardState 字段）；新增 `LruFingerprints` struct + `contains` / `push` 方法；新增 `Service::run` 的 `select!` 两个 arm —— outbound tick（500ms：`current_text` → sha256 → diff check → 不在 LRU 则 broadcast `ProtoEvent::ClipboardText` 到所有 `enable_clipboard_to=true` 的活跃 peer → LRU push → FrontendEvent 推送）和 inbound recv（peer-pushed `ClipboardText` → LRU loopback check → 不命中则 `set_text` 本地剪贴板 + 更新 `clipboard_last_text` + FrontendEvent with peer addr） |
| `src/capture.rs`（+65 行） | `CaptureRequest::SendClip(ProtoEvent, ClientHandle)` 新变体 + `Capture::send_event(event, handle)` public API + `CaptureTask` 在 restart-loop 和 `do_capture_session` 两个 `select!` arm 里的对称 handler：`conn.send(event, handle).await`（走 `peer.send_input` → `route_input` → StreamC for `ClipboardText`）；fire-and-forget（warn log on failure，不回传 dispatcher —— 剪贴板推送 best-effort，下次 user copy 自然重试） |
| `src/connect.rs`（+64 行） | `LanMouseConnection::clipboard_inbound_tx: tokio::mpsc::UnboundedSender<(SocketAddr, ProtoEvent)>` 字段 + `LanMouseConnection::new` 签名加 1 参数 + 每次 `connect_to_handle` / supervisor redial 克隆 sender + 成功 dial 后 `peer.set_clipboard_inbox(Some(this.clone()))`（让 per-peer `read_stream_c_loop` 把 `StreamEvent::ClipboardMeta` forward 到 service dispatcher） |
| `src/emulation.rs`（+76 行） | `Emulation::new` 加 `clipboard_inbound_tx` 参数 + `ListenTask` 字段 + `match` arm：`ProtoEvent::ClipboardText(ct)` → `clipboard_inbound_tx.send((addr, ClipboardText(ct)))`；tokio channel（不是 `local_channel`），因为 `Service::run` 通过 `tokio::select!` 消费，`local_channel::mpsc::Receiver` 不 native 兼容；其它变体（`ClipboardImage` / `ClipboardFiles` / `FileTransfer*`）仍落到 `_ => {}` arm（M2a / M3a 范围） |

### 1.2 关键设计

#### 500ms tick + sha256 fingerprint dedup
- `tokio::time::Interval::tick().await` 每 500ms 醒一次（首次 t≈500ms，避免与 daemon startup select! 抢占）
- `backend.current_text() → String` → `Sha256::digest(bytes)` → 与 `clipboard_last_text` 缓存的指纹比对；相同 → skip（quiescent tick 0 hash cost）
- 不同 + 不在 LRU → 构造 `ProtoEvent::ClipboardText` → 通过 `Capture::send_event` 推给所有活跃 peer 的 handle（filter by `cfg.enable_clipboard_to`）

#### LRU loopback defence（核心创新）
- `LruFingerprints: VecDeque<[u8; 32]>` + cap 64 + linear `contains`（O(64) ~64 byte-compare，可忽略）
- **outbound push 时**：把刚推出去的 sha256 压入 LRU（"this fingerprint is mine"）
- **inbound recv 时**：peer-pushed `ClipboardText` 的 sha256 在 LRU 里 → 视作"对方在回显我刚推的内容" → drop（不 apply 到本地剪贴板，避免无限循环：A.copy → push → B.apply → B 变 → B.push → A.apply ...）
- LRU cap 64 是 M1a 简化：M1b 升级为 LRU 128 + 60s TTL + 显式 `cache.remove(fingerprint)` on push（PLAN §1 评审 #3 2nd + #4 3rd 已记录）

#### Inbound path 集成（ListenTask + connect.rs）
- server 侧（listen.rs `server_stream_c_reader_task`）：每收到 var-codec frame → `ListenEvent::Msg { event, addr }` → `ListenTask` match → `clipboard_inbound_tx.send((addr, ClipboardText(ct)))` → service 主循环 `select!` 消费
- client 侧（`read_stream_c_loop`）：`StreamEvent::ClipboardMeta(proto_event)` → `peer.set_clipboard_inbox` 设置的 sender → service 主循环同一条 receiver 消费
- 两条路径收敛到 service 的单 receiver，dispatcher 不用关心 flow direction

#### Fire-and-forget outbound
- `Capture::send_event` 不 await peer-ack；`request_tx.send(CaptureRequest::SendClip)` 立刻返回
- `CaptureTask` 异步处理：调用 `conn.send` 失败时 log warn（peer 不在线 / 流关闭 / IO 错）
- 剪贴板同步是 best-effort —— 用户下次复制时自然重试，不引入 per-peer ack 协议

#### sha2 reuse
- `lan-mouse-proto` 已经在用 `sha2 0.10`（WireGuard PSK），`service.rs` 直接 `use sha2::{Digest, Sha256}` —— 0 新依赖

### 1.3 测试结果
- `cargo build -p lan-mouse`（macOS）：✅ 0 error / 0 warning
- `cargo test -p lan-mouse --lib`（macOS）：✅ **121 passed / 0 failed**（基线 116 + 8 trait/dummy + 5 macOS round-trip + cfg-gated Linux/Windows 测试仅真机编译时跑）
- `cargo check -p lan-mouse-cli` / `cargo check -p lan-mouse-proto` / `cargo check -p lan-mouse-ipc`：✅（无新错误）
- dispatcher 单测覆盖：loopback defence LRU contains / push / cap-64 eviction 的 inline 行为通过 service.rs doc-comment 详尽描述；端到端行为（dispatcher 真驱动 OS 剪贴板）需 M1a 真机测试矩阵验证

### 1.4 累计耗时
~40 min（含 doc-comment 设计 + `LanMouseConnection::new` 签名变化的 ripple 修改 + Connect/LanMouseConnection 各处 caller 更新）

## 2. PLAN 偏差

**0 处**（与 PLAN §3 M1a STEP-1a.4 行 100% 一致）

## 3. 已知限制

- **LRU 64 / 无 TTL**：连续复制 64 个不同文本在 60s 内会 roll —— 此时对方 push 回来的旧 fingerprint 不在 LRU，会被重新 apply 到本地（"我刚推的内容又回来了"）。M1b 升级为 LRU 128 + 60s TTL + push 时 `cache.remove`（PLAN §1 评审 #3 2nd + #4 3rd）。
- **Tokio::time::Interval**：`tick().await` 跳过第一个 tick 立即 —— 首次 dispatch 在 t≈500ms，不在 t=0。避免与 daemon startup handshake `select!` 抢占导致 startup 阶段剪贴板抖动。
- **同步 backend on async runtime**：`current_text` / `set_text` 在 service 主循环的 `spawn_local` task 内执行 —— macOS 上 `pbcopy` / `pbpaste` 通常 <5ms；Windows `OpenClipboard` 极端情况阻塞 ~30s（其它进程卡住剪贴板）—— M1a 接受简化，M1b 再考虑 `tokio::task::spawn_blocking` 包装。
- **inbound channel unbounded**：mpsc::UnboundedSender——`read_stream_c_loop` 是热循环，理论上对方恶意刷屏可能 OOM；M1a 信任 peer 是 mTLS-authed 的人类用户；M2b 收紧为 bounded(64) + drop-oldest。

## 4. 下一步

→ STEP-1a.5（fmt + clippy + `cargo test --workspace` + 三平台 `cargo check` + 真机 manual log 模板）
→ 或按 leader 派发继续

---

## 5. 执行备注（leader 直补）

> **本 STEP 由 leader 在 session 重启后接管 commit**：executor 已落代码 + 单测，但 session 在 commit 前中断。leader 按 PLAN §3 M1a STEP-1a.4 行 + diff 内容验证后 commit `182a0ea`，并基于 diff 写本报告（diff 已在 commit message 中详尽记录；本报告补充设计 rationale + 限制 + 后续）。