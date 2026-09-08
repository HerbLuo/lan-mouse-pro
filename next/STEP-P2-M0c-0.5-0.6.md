# STEP-P2-M0c-0.5 + 0.6 — StreamC 接通 + IPC 扩展 (合并)

> PLAN §3 M0c STEP-0.5a + STEP-0.5b + STEP-0.6 合并
> 执行日期：2026-09-08　实际耗时：~85 min
> 起点 commit：fdf03bc（STEP-P2-M0b-0.3-0.4 报告后）　终点 commit：（待 leader 提交）
> 结论：✅ 通过

---

## 1. 做了什么

按 PLAN §3 M0c STEP-0.5a + STEP-0.5b + STEP-0.6 合并范围落地：

### STEP-0.5a — StreamC 接通（发送 + ReadStreams 字段）
- `PeerSession` 加 `cached_send_c: Mutex<Option<SendStream>>`（lazy open + write-failure invalidation 镜像 `cached_send_b`）
- `PeerSession::send_stream_c(&ProtoEvent)` 公共方法（路由 `Channel::StreamC` 的所有 7 个 var-codec 变体）
- `StreamEvent::ClipboardMeta(ProtoEvent)` 变体
- `ReadStreams.c: Receiver<StreamEvent>` + `join_c: JoinHandle<...>` 字段
- `PeerSession::run` 的 select! 加 **Path E (Stream C)** 分支（`ClipboardMeta` → `send_clipboard_inbox`）
- 单元测试：`send_stream_c` 单元 + `ReadStreams.c` 字段访问

### STEP-0.5b — StreamC reader task
- `streams::read_stream_c_loop<R>(recv, tx)` 公共函数（替代 `streams.rs:333` 的 `drop(stream_bunch.c)`）
- `streams::read_loop` spawn `read_stream_c_loop`（替换 `drop(bunch.c)`）
- `protocol::write_stream_c_frame<W>(send, &event)` + `read_stream_c_frame<R>(recv)` 公共函数（`[u32 BE len][body bytes...]` framing，`From<ProtoEvent> for Vec<u8>` 编码 + `TryFrom<&[u8]>` 解码，无 `MAX_EVENT_SIZE` 上限）
- `PeerSession::clipboard_inbox: Arc<Mutex<Option<ClipboardInboxSender>>>` 字段 + `set_clipboard_inbox` setter + `send_clipboard_inbox` 私有方法
- **`listen.rs::handle_quic_peer_supervisor` 真正调用 `peer.start_http3_server(default_router())`**（M0b STEP-0.3+0.4 接口就位但未触发，M0c 落地）
- `listen.rs::server_accept_bi_task` 加 Stream C 判别分支（`len > MAX_EVENT_SIZE` → 派发到 `server_stream_c_reader_task`）
- `listen.rs::server_stream_c_reader_task` 新增（读 var-codec frames → 转发到 `listen_tx`）
- "stream C is M0c-only" 错误消息删除（被 `send_stream_c` 替代）
- 单测：3 个 `protocol::write_stream_c_frame` + `read_stream_c_frame` round-trip / large-payload / truncated 单测；2 个 `streams::read_stream_c_loop` round-trip + backpressure 单测；1 个 **full-QUIC-stack StreamC 端到端** 单测（client.send_input → wire → server clipboard_inbox）

### STEP-0.6 — IPC 扩展
- `lan_mouse_ipc::ClipboardConfig { auto_accept_files: bool, accept_dir: Option<PathBuf>, ignore_text: bool, ignore_images: bool, ignore_files: bool }`（`#[serde(default)]` 兼容旧 wire）
- `lan_mouse_ipc::FrontendEvent::ClipboardState { last_text_ts, last_image_ts, last_file_ts, last_source: Option<String> }`
- `lan_mouse_ipc::FrontendRequest::SetClipboardConfig(ClipboardConfig)`（无 handle）
- `lan_mouse_ipc::FrontendRequest::SetEnableClipboardTo(ClientHandle, bool)`
- `lan_mouse_ipc::ClientConfig.enable_clipboard_to: bool`（`#[serde(default = "default_enable_clipboard_to")]` 兼容旧 wire → 缺字段 = `true`）
- `src/config.rs::TomlClient.enable_clipboard_to: Option<bool>` + `ConfigClient.enable_clipboard_to: bool` + `Config::set_clipboard_config` / `clipboard_config` getter（TOML `[clipboard]` 段持久化 + 读取）
- `src/service.rs::set_clipboard_config` + `set_enable_clipboard_to` handler
- `src/client.rs::set_enable_clipboard_to` setter
- 单测：11 个 `lan-mouse-ipc` round-trip + 缺字段兼容 + 3 个 `src/config.rs` write-back / read-back

### 1.1 文件改动

| 文件 | 改动 |
|---|---|
| `lan-mouse-ipc/src/lib.rs` | +136 行：`ClipboardConfig` + `enable_clipboard_to` 字段 + `ClipboardState` event + `SetClipboardConfig`/`SetEnableClipboardTo` request + 11 个单测 |
| `lan-mouse-proto/` | 未改（M0a 已落地 codec 双轨） |
| `src/quic_transport/protocol.rs` | +109 行：`write_stream_c_frame` + `read_stream_c_frame`（length-prefixed var-codec，无 MAX cap）+ 3 个单测 |
| `src/quic_transport/streams.rs` | +88 行：`StreamEvent::ClipboardMeta` 变体 + `ReadStreams.c`/`join_c` 字段 + `read_stream_c_loop` + 2 个单测 |
| `src/quic_transport/session.rs` | +232 行：`cached_send_c` / `send_stream_c` / `clipboard_inbox` / `set_clipboard_inbox` / `send_clipboard_inbox` / `start_http3_server` 签名改为 `&self` / `send_input` StreamC 分支改用 `send_stream_c` / `PeerSession::run` select! Path E / full-QUIC-stack StreamC 端到端单测 |
| `src/quic_transport/listen.rs` | +75 行：`start_http3_server` 真正调用 + `server_stream_c_reader_task` 新增 + `server_accept_bi_task` Stream C 判别分支 |
| `src/config.rs` | +105 行：`TomlClipboard` + `TomlClient.enable_clipboard_to` + `ConfigClient.enable_clipboard_to` + `Config::clipboard_config`/`set_clipboard_config` getter/setter + 3 个单测 |
| `src/service.rs` | +44 行：`SetClipboardConfig`/`SetEnableClipboardTo` handler + `set_clipboard_config`/`set_enable_clipboard_to` 方法 |
| `src/client.rs` | +24 行：`set_enable_clipboard_to` setter + 测试 fixture `enable_clipboard_to: true` 字段 |

### 1.2 设计决策

1. **`start_http3_server` 签名从 `Arc<Self>` 改为 `&self`**：listen.rs supervisor 持 `peer: Rc<PeerSession>`（不是 `Arc`）。`Rc` 不能跨 `spawn_local` 边界，但 spawned task 只需要 `Connection`（quinn 内部 `Arc` 化），所以 clone `Connection` 即可。`Connection::clone()` 是廉价的 reference count 操作。This matches the production usage in `listen.rs::handle_quic_peer_supervisor` and the test usage in `tests/quic_smoke.rs`（前者用 `Rc`、后者用 `Arc`，两种都能通过 `&self` 调用）。

2. **Server-side Stream C 接收走 `listen_tx → ListenTask → EmulationEvent::Message → service`**：不直接走 `peer.clipboard_inbox`。理由：现有 `ListenTask → EmulationEvent::Message` pipeline 已能 handle stream A / B / datagram 消息，统一走这个路径保持 dispatch 一致性。服务在 M1a 加 `clipboard::apply_event` handler，按 variant 决定是 clipboard 还是忽略。直接走 `clipboard_inbox` 会绕开 service 的现有 pipeline。

3. **Server-side accept_bi 的 Stream C 判别**：`len > MAX_EVENT_SIZE` (21) → Stream C，`len ≤ MAX_EVENT_SIZE` → Stream B。`ClipboardText { fingerprint: [u8; 32], sha256: [u8; 32] }` baseline = 64B = 33B（type + body），固定超过 21B。`ClipboardImage` / `ClipboardFiles` / `FileTransfer*` 更大。判别器绝对可靠（FixedCodec 永远 ≤ 21B，VarCodec 永远 ≥ 33B）。

4. **End-to-end test 不使用 `peer.run(Server)`**：`peer.run(Server)` 的 main loop 只通过 `read_loop` 接收 bunch bidi（client 的 `peer.run(Client)` 启动时开的 3 个），没有 `accept_bi` 循环去接 client `send_stream_c` lazy open 的新 bidi。生产环境 listen.rs 用 `server_accept_bi_task` 来接，test 镜像这个模式：跑 server_hello 后手动起 `accept_bi` 任务。生产路径不变。

5. **`send_input` StreamC 分支直接调 `send_stream_c`（不再返 `Err`）**：M0a 时是 "stream C is M0c-only" placeholder error，M0c 落地后真正可写。M0a 的 wire-compat invariant（StreamA 不编码新 EventType）保持。

6. **`enable_clipboard_to` default = true via `#[serde(default = "default_enable_clipboard_to")]`**：直接 `#[serde(default)]` 不行（`bool::default()` = false），需要 `const fn` 函数返回 true。`ClientConfig::default()` 也携带 `true`（in-memory default = wire default）。

7. **TOML `[clipboard]` 段 omit-on-default 写回**：`true` / `None` 字段写回为 `None`，不污染旧 config 文件（与 `monitor` / `input_channels` 同模式）。新增了 3 个 config 单测覆盖 write-back / read-back / omit 三个分支。

### 1.3 端到端 StreamC wire path

```
client: send_input(ClipboardText{...})
   ↓ route_input → Channel::StreamC
client: send_stream_c(ClipboardText)
   ↓ cached_send_c (lazy open bidi)
   ↓ write_stream_c_frame: [u32 BE len][body bytes...] (body = Vec<u8> from From<ProtoEvent>)
[wire: bidi stream C]
   ↓ accept_bi on server (length prefix > MAX_EVENT_SIZE)
server: server_accept_bi_task dispatches → server_stream_c_reader_task
   ↓ read_stream_c_frame: read u32 → read body → TryFrom<&[u8]> → ProtoEvent::ClipboardText
   ↓ listen_tx.send(ListenEvent::Msg { event, addr })
[emulation pipeline]
   ↓ ListenTask → EmulationEvent::Message → service
service: M1a wires clipboard::apply_event (M0c only verifies the wire path)
```

`PeerSession::run` (client side, used by tests + future client code) mirrors this:

```
client: peer.run(Client) main loop
   ↓ read_streams.c.recv() → StreamEvent::ClipboardMeta(event)
client: send_clipboard_inbox(event, remote_addr)
   ↓ peer.clipboard_inbox (set by set_clipboard_inbox before run)
test: receives (remote_addr, ClipboardText) from mpsc inbox
```

---

## 2. 验证结果

### 2.1 命令 + 输出摘要

| 命令 | 输出 |
|---|---|
| `cargo build --workspace` | ✅ 0 error |
| `cargo test --workspace --exclude input-capture` | ✅ **170 pass / 0 fail**（108 lib + 7 quic_smoke + 2 others + 26 ipc + 27 proto；baseline 150 → +20 new） |
| `cargo fmt --check` | ✅ 0 diff |
| `cargo clippy --workspace --all-targets` | ✅ 0 new warning（7 pre-existing 在 SUGGESTION-IGNORE.md #1） |
| `git diff --stat Cargo.lock` | ✅ 0 diff（无 dep 变更） |

### 2.2 20 个新单测（覆盖 STEP-0.5 + STEP-0.6 全部范围）

#### `lan-mouse-ipc` lib (+11)
- `client_config_enable_clipboard_to_defaults_to_true_when_missing`
- `client_config_enable_clipboard_to_round_trip`
- `client_config_default_has_enable_clipboard_to_true`
- `clipboard_config_default_is_legacy_shape`
- `clipboard_config_round_trip_populated`
- `clipboard_config_missing_fields_default_to_legacy`
- `clipboard_config_partial_missing_accept_dir`
- `request_set_clipboard_config_round_trip`
- `request_set_enable_clipboard_to_round_trip`
- `event_clipboard_state_round_trip`
- `event_clipboard_state_missing_fields_default_to_none`

#### `src/config.rs` (+3)
- `config_omits_enable_clipboard_to_when_true_on_writeback`
- `config_keeps_enable_clipboard_to_field_when_false_on_writeback`
- `config_defaults_enable_clipboard_to_true_on_readback`

#### `src/quic_transport/protocol.rs` (+3)
- `stream_c_frame_round_trip_clipboard_text` — codec round-trip with inline content
- `stream_c_frame_handles_payloads_larger_than_max_event_size` — 1 KiB inline exceeds MAX_EVENT_SIZE
- `stream_c_frame_truncated_body_returns_truncated` — Truncated error semantics

#### `src/quic_transport/streams.rs` (+2)
- `stream_c_loop_round_trip_clipboard_meta` — reader task dispatches ClipboardMeta via mpsc
- `stream_c_backpressure_blocks_when_receiver_idle` — blocking sender backpressure (mirror of stream B)

#### `src/quic_transport/session.rs` (+1)
- `stream_c_clipboard_text_round_trip` — **full-QUIC-stack StreamC 端到端** (peer.run(Client) → wire → server accept_bi → clipboard_inbox)

### 2.3 wire-compat（PLAN §0 评审 #1）

- ✅ StreamA 仍不编码 7 个新 EventType（`(*event).into()` fixed-codec 路径不变）
- ✅ StreamB 仍走 21B 限（`send_input` StreamB 分支未动）
- ✅ StreamC 接通后，新 EventType 编码/解码走 var-codec（无 MAX_EVENT_SIZE 上限，var-codec 永远 ≥ 33B）
- ✅ Server-side accept_bi 判别 `len > MAX_EVENT_SIZE` → Stream C（不变 Stream B / bunch 行为）
- ✅ `From<ProtoEvent> for Vec<u8>` dispatcher 已存在（M0a 落地）— Stream C 直接用

### 2.4 PLAN §8 测试矩阵 M0c 段 — 本 STEP 覆盖

| 类型 | 测试项 | 通过标志 | 实际 |
|---|---|---|---|
| 自动 | StreamC reader 启动（`streams::read_loop` 内 `spawn` c reader） | 编译期断言 | ✅ `read_loop` 调用 `spawn_local(read_stream_c_loop(...))` |
| 自动 | `Stream C is M2-only` 警告（`session.rs:582`）消失 | grep 无匹配 | ✅ grep 验证无匹配；`send_input` StreamC 分支改用 `send_stream_c` |
| 自动 | `lan-mouse-ipc::ClipboardConfig` serde round-trip（缺字段 = default） | 单测绿 | ✅ 5 个 `clipboard_config_*` 单测 |
| 自动 | `FrontendEvent::ClipboardState` serde round-trip | 单测绿 | ✅ `event_clipboard_state_round_trip` + `event_clipboard_state_missing_fields_default_to_none` |
| 自动 | StreamC 端到端 mock：构造 ClipboardText 走 stream C → service 收件箱收到 | 单测绿 | ✅ `stream_c_clipboard_text_round_trip`（full-QUIC-stack） |

---

## 3. 与 PLAN 的偏差

**0 处功能偏差**。

落地与 PLAN §3 M0c STEP-0.5a + STEP-0.5b + STEP-0.6 完全一致：
- `cached_send_c` + `send_stream_c` ✅
- `StreamEvent::ClipboardMeta` + `ReadStreams.c` ✅
- `read_stream_c_loop` 替代 `drop(bunch.c)` ✅
- `PeerSession::start_http3_server` 在 `listen.rs` 真正调用 ✅
- "Stream C is M0c-only" 警告消失 ✅
- `ClipboardConfig` + `enable_clipboard_to` + `ClipboardState` + 对应 `FrontendRequest` ✅
- serde round-trip + 缺字段兼容 ✅

**1 处实现细节调整**（非偏差）：
- `start_http3_server` 签名从 `Arc<Self>` 改为 `&self`（匹配 listen.rs supervisor 的 `Rc<PeerSession>` + 测试的 `Arc<PeerSession>` 两种用法；连接 clone 已足够，无需 `Arc<Self>`）。

---

## 5. 闸门检查

| 闸 | 结果 |
|---|---|
| 时间门（~90 min AI；≤ 2h ABS） | ✅ ~85 min |
| milestone 边界门（M0c 内，未触碰 M1+） | ✅ StreamC wire 接通 + IPC 字段就绪；service 仅 handler + 持久化（无业务逻辑）；listen.rs server-side 接通但 production server pipeline 不变；剪贴板业务逻辑（M1a+）未触碰 |
| 产物（StreamC send + reader + IPC 字段） | ✅ |
| 依赖（M0a 完 + M0b STEP-0.2+0.3+0.4 完 + PeerSession 字段就位） | ✅ |
| 验收（cargo build/test/fmt/clippy） | ✅ fmt 0 diff；clippy 0 new；170 pass / 0 fail |

---

## 6. 遗留

- **Service handler 仅做持久化**：`set_clipboard_config` 落 TOML + log，不 wire 到 clipboard backend（`service::clipboard::apply_config` 是 M1a 范围）。重启后 `Config::clipboard_config()` 拿到值即可。`set_enable_clipboard_to` 同样：落 TOML + broadcast client state，M1a 在 dispatcher 入口处 gate on `client.enable_clipboard_to`。
- **listen.rs server-side stream C 走 `listen_tx` 而非 `clipboard_inbox`**：production supervisor 已经把消息推到 `listen_tx → ListenTask → EmulationEvent::Message` pipeline。`clipboard_inbox` 仅被 `peer.run` 的客户端 main loop 使用（因为 client 路径不走 listen_tx）。两条路径汇合在 service 层。
- **`peer.run(Server)` 不接 client 的 lazy stream C bidi**：测试用 inline `accept_bi` task 镜像 listen.rs 模式。生产路径正确（listen.rs 有 `server_accept_bi_task`）。
- **5xx / 404 / cache miss 行为**：StreamC 链路暂时不涉及 HTTP/3（StreamC 是 var-codec over bidi stream，不是 http3 路由）；http3 stub handler 已经在 M0b STEP-0.3+0.4 落地，M1/M2/M3 接 cache。
- **pre-existing clippy 噪音**：7 个 warning 在 SUGGESTION-IGNORE.md #1；与本 STEP 无关。

---

## 7. 下一步

→ leader commit（commit message 建议：`<type>(quic+ipc): M0c StreamC 接通 + ClipboardConfig IPC 扩展 (STEP-0.5a + 0.5b + 0.6 merge)` + `归档: next/STEP-P2-M0c-0.5-0.6.md` + `Co-Authored-By`）

→ 派 **M0b STEP-0.7**（fmt/clippy 三平台编译 + 真机 `curl --http3 https://<peer>:4252/healthz` 验证 — 人类配合）。StreamC 现在 + http3 server 现在真在跑，所以 M0b STEP-0.7 是合理的下一步。

→ 派 **M0c STEP-0.7**（如果有 fmt/clippy/三平台 + 真机端到端需求；或并入 M0b STEP-0.7）。

→ 派 **M1a STEP-1a.1+**（`ClipboardBackend` trait + macOS 实现 — 真机 `pbcopy "x"` → 对端 `pbpaste` 拿到 `x`）。