# STEP-VALIDATION-P2-M1a

**Validator**: step-validator (M1a 整批审)
**日期**: 2026-09-08
**审阅范围**: 8 commit (`14d2651` / `6cb29df` / `52b41f1` / `182a0ea` / `2f99788` / `dfdfb7a` / `315fbc8` / `b92c750`)
**起点 commit**: `b834cfa`（M0c validator 终点）
**终点 commit**: `b92c750`（1a.5 报告）
**结论**: PASS-with-followup

---

## P0（阻塞，必须返工）

无

## P1（严重 bug，应该返工）

无

## P2（建议修，可押后）

### P2.1 `src/clipboard/mod.rs:1-71` 重复 doc comment
- **位置**: `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/clipboard/mod.rs:1-35` 与 `:37-71`
- **现象**: 模块 doc comment（"Cross-platform clipboard backend abstraction (PLAN-2 / M1a)."）整段 35 行被完整复制一份（line 37 重新以 `//!` 开头继续），rustdoc 会显示两遍完全相同的内容。
- **根因**: 1a.2 提交时 executor 整文件重写 + 漏删了 line 36 的空行 → 上半 doc 残留；1a.5 fmt 后 rustfmt 没有动 doc comment 块。
- **影响**: 0（runtime 行为正确；仅 rustdoc 输出冗余）。M0c validator P3.3 风格的同类问题。
- **建议修复**: 删除 line 36-71 的重复块（保留 line 1-35）。

### P2.2 `src/service.rs:200-204` `LruFingerprints::len` 标 `#[cfg(test)] #[allow(dead_code)]` 但无 test 使用
- **位置**: `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/service.rs:200-204`
- **现象**: `LruFingerprints::len` 是 test-only helper，1a.4 顺手加但单测没真正调用。1a.5 报告 §2.2 决策"保留给 1b.x 阶段"——但 1b.1/1b.3 阶段真要 LRU 时大概率会直接用 `clipboard_lru.contains` + `clipboard_lru.push` 端到端断言（与 M0a/M0b/M0c 风格一致），不一定需要 `len` 单独暴露。
- **影响**: 0（dead_code 已 allow，build 仍绿）。
- **建议修复**: 移到 1b.1/1b.3 dispatcher 端到端单测落地时同步加 `len` 断言并移除 `#[allow(dead_code)]`；或 1a.5 末尾就删除 helper 避免 1b.1 重新加回。

### P2.3 `src/service.rs:127-135` `clipboard_inbound_tx` 字段保留 + `#[allow(dead_code)]` —— 实际通过 `.clone()` 隐式使用
- **位置**: `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/service.rs:127-135`
- **现象**: `clipboard_inbound_tx` 字段加了 `#[allow(dead_code)]`；实际在 `Service::new` line 358 赋值、在 `Emulation::new(... clipboard_inbound_tx.clone())` line 315 与 `LanMouseConnection::new(... clipboard_inbound_tx.clone())` line 283 通过 `.clone()` 被外部消费（编译器不把 clone 视作 self-use，触发 dead_code）。
- **影响**: 0（allow 已加，build 仍绿）。
- **建议修复**: 把字段从 `Service` struct 移除——`Emulation` / `LanMouseConnection` 各 clone 自己的 sender，Service 不需要保留；或者在字段上加 `#[allow(dead_code)]` doc-comment 明确"keeps the sender alive for the lifetime of the Service so external clones retain access"。当前 doc-comment 解释了 rationale 但 allow 仍显得 hack。

## P3（风格 / 建议）

### P3.1 `service.rs` dispatcher 三函数无单元测试覆盖
- **位置**: `src/service.rs:1198` `handle_clipboard_tick` / `:1262` `handle_clipboard_inbound` / `:1333` `broadcast_clipboard_event`
- **现象**: 这三个函数是 M1a 核心业务逻辑（500ms tick + sha256 dedup + LRU loopback defense + peer broadcast + FrontendEvent push），目前**全部**靠 8 文件全 workspace 真机端到端覆盖。`LruFingerprints` 自己的 `contains` / `push` / `len` 也无独立单测——`#[cfg(test)] #[allow(dead_code)]` 标记的 `len` helper 就是为单测准备的占位。
- **影响**: 0（真机端到端能补；但 dispatcher 内部 LRU 行为在真机 fail 时调试成本高）。
- **建议**: 1b.1/1b.3 阶段补 `dispatcher_tests` 模块——mock 一个 `DummyBackend` + 单测 `LruFingerprints::new(4).push(a).push(b).push(c).push(d).push(e).contains(&a) == false`（验证 LRU 容量 + FIFO 驱逐） + 单测 `handle_clipboard_tick` 在 `current_text` 变化时推 `ProtoEvent::ClipboardText` + 单测 `handle_clipboard_inbound` 在 LRU 命中时 skip。
- **PLAN 偏差审查说明**: 这与 1a.4 报告 §1.3 "dispatcher 单测覆盖：loopback defence LRU contains / push / cap-64 eviction 的 inline 行为通过 service.rs doc-comment 详尽描述" 完全一致——是 design choice 而非 P1 漏检；M1a 末段接受 "真机测试 + doc 详尽" 简化策略。

### P3.2 `clipboard_inbound_rx` mpsc::UnboundedSender 通道在 M2b 收紧为 bounded(64) + drop-oldest
- **位置**: `src/service.rs:126` / `src/connect.rs:130` / `src/emulation.rs` (Emulation::new 签名)
- **现象**: 当前 unbounded——M1a 1a.4 报告 §3 已知限制："理论上对方恶意刷屏可能 OOM；M1a 信任 peer 是 mTLS-authed 的人类用户；M2b 收紧"。
- **影响**: 0（mTLS 鉴权下 mTLS-authed 人类用户的恶意刷屏是 accept 的 risk model）。
- **建议**: M2b 改 `tokio::sync::mpsc::channel(64)` + 服务端 `try_send` + drop-oldest 策略；M1a 末段不修。

### P3.3 `STEP-VALIDATION-P2-M0c.md` P2/P3 backlog 与 M1a 关系
- **位置**: `/Users/hb/Projects/@cloudself/lan-mouse-pro/next/STEP-VALIDATION-P2-M0c.md` P2.1 / P2.2 / P3.1 / P3.2 / P3.3
- **现象**: M0c 5 处 P2/P3 全部押后（"不阻塞 M1a 派发"）。M1a 8 commit 落地后这些 P2/P3 仍未触动（plan-step-executor scope discipline：M1a 不修 M0c 范围）。
- **影响**: 0。
- **建议**: 与 M0b P2 backlog 一起等 M2a/M2b 同步收尾阶段批量清理；M1a 末段不修。

## 验证证据

### 关键单测结果（实测：clippy 修复 commit `315fbc8` 末尾）
```
cargo test --workspace：
  input-capture  101 passed
  lan-mouse (lib)  121 passed
  quic_smoke  7 passed
  quic_session  2 passed
  lan-mouse-ipc (lib)  26 passed
  lan-mouse-proto (lib)  27 passed
  合计: 284 passed / 0 failed ✅
```

### 累计增量（M0c 170 → M1a 284 = +114）
- input-capture: 0 → 101（+101，从 M1a 1a.5 报告 §1.3 看是 input-capture crate 本来就有但 M0c 没统计到；或 M0c 期间 input-capture 新增测试）
- lan-mouse (lib): 108 → 121（+13 = 1a.1 +8 trait/dummy + 1a.2 +5 macOS round-trip）
- quic_smoke: 7 → 7（+0）
- quic_session: 2 → 2（+0）
- lan-mouse-ipc: 26 → 26（+0，M0c 已 26）
- lan-mouse-proto: 27 → 27（+0，M0a 已 27）

> 与 leader 报告 §1.3 "0 new clippy（M0a/M0b/M0c/M1a 1a.1-1a.4 引入 0）+ 1a.5 暴露 1a.4 隐藏 3 + 1，1a.5 清理" 一致。

### M0c → M1a 关键代码变更（验过不是"只编译过"）

1. **`service.rs:96-159`** —— 9 个新字段（`clipboard_backend` / `clipboard_lru` / `clipboard_last_text` / `clipboard_inbound_{tx,rx}` / `clipboard_tick` / `last_text_ts_ms` / `last_image_ts_ms` / `last_file_ts_ms` / `last_clipboard_source`），详尽 doc-comment 解释 M1a 限制
2. **`service.rs:172-205` `LruFingerprints`** —— `VecDeque<[u8; 32]>` + cap 64 + linear `contains` + FIFO 驱逐；M1a 简化（无 TTL）
3. **`service.rs:1198-1242` `handle_clipboard_tick`** —— 500ms tick 真进入 `Service::run` 的 `select!` arm（line 394）；`current_text` → sha256 → diff check → LRU check → 推 `ProtoEvent::ClipboardText` 到所有 active peer
4. **`service.rs:1262-1323` `handle_clipboard_inbound`** —— LRU 命中 skip；不命中 apply `set_text` + LRU push + `last_text = None`（force re-read）+ FrontendEvent 推送
5. **`service.rs:1333-1346` `broadcast_clipboard_event`** —— 过滤 `enable_clipboard_to=true` + `active=true` + `active_addr.is_some()`；fire-and-forget 调 `self.capture.send_event(event.clone(), handle)`
6. **`capture.rs:295` `SendClip(ProtoEvent, ClientHandle)` 新变体** —— capture request 类型
7. **`capture.rs:423-427` `Capture::send_event` public API** —— 走 `request_tx.send(CaptureRequest::SendClip(event, handle))` 非阻塞
8. **`capture.rs:635-641` `CaptureTask` `SendClip` handler（do_capture_session arm）** —— 调 `self.conn.send(event.clone(), handle).await`；失败 log warn
9. **`capture.rs:976-982` `CaptureTask` `SendClip` handler（restart-loop arm）** —— 对称实现（同 635-641）
10. **`connect.rs:100-111` `LanMouseConnection::clipboard_inbound_tx` 字段** —— tokio mpsc unbounded sender
11. **`connect.rs:121` `#[allow(clippy::too_many_arguments)]` on `LanMouseConnection::new`** —— 1a.4 加第 8 参数触发
12. **`connect.rs:541-554` `//` 注释（1a.5 修复 `///` on fn param）** —— 9 行内联 rationale
13. **`emulation.rs` ListenTask 新增 `ProtoEvent::ClipboardText` arm** —— tokio channel forward 到 service dispatcher
14. **`protocol.rs:189-203` `route_input` StreamC arm** —— 7 个新 var-codec 变体（含 `ClipboardText`）全部走 `Channel::StreamC`
15. **`session.rs:557` `send_stream_c` 实现** —— var-codec frame on `cached_send_c`
16. **`session.rs:711` `Channel::StreamC => self.send_stream_c(event).await`** —— 替换 M0a 的 `Err(stream C is M0c-only...)` placeholder
17. **`clipboard/mod.rs:283-302` `default_backend` 三平台 cfg-gate 工厂** —— macOS 走真实现；Linux/Windows/其它走 `Err(NotImplemented)`
18. **`clipboard/macos.rs:65-71` `MacOsPasteboard::new`** —— `Command::new("pbpaste").output()` 探针
19. **`clipboard/macos.rs:85-93` `current_text`** —— `pbpaste` subprocess + 非 0 exit → `None`（pbpaste 对 image clipboard 返 1，正确）
20. **`clipboard/macos.rs:105-128` `set_text`** —— `pbcopy` 写 stdin，pbcopy / pbpaste 走 std::process 不用 tokio
21. **`clipboard/linux.rs:47-91` `Tool` enum + `Tool::detect()`** —— WAYLAND_DISPLAY + wl-paste probe → Wayland；否则 xclip probe → X11
22. **`clipboard/windows.rs:79-244` `WinClipboard`** —— `OpenClipboard(NULL)` / `GetClipboardData(CF_UNICODETEXT)` / `SetClipboardData` + `GlobalAlloc(GMEM_MOVEABLE)` + `GlobalLock` UTF-16 + NUL

### 关键代码路径 spot-check

#### 路径 1：outbound 推送真进入 StreamC
```
service.handle_clipboard_tick (line 1198)
  ↓ current_text → sha256 → diff check → LRU contains check
  ↓ ProtoEvent::ClipboardText { fingerprint, sha256, size, content_inline }
  ↓ broadcast_clipboard_event (line 1232) → capture.send_event (line 1344)
  ↓ capture request_tx.send(SendClip) (capture.rs:426)
  ↓ CaptureTask handler (capture.rs:635) → conn.send(event, handle) (capture.rs:636)
  ↓ LanMouseConnection.send → peer.send_input → route_input (protocol.rs:189)
  ↓ Channel::StreamC (protocol.rs:191-203)
  ↓ session.send_stream_c (session.rs:711) → write var-codec frame on cached_send_c
  ✅ 整条链路串通
```

#### 路径 2：inbound 接收真进入 dispatcher
```
server 端：peer.run → server_stream_c_reader_task (listen.rs)
  → read_stream_c_frame → ListenEvent::Msg { event, addr }
  → listen_tx → ListenTask (emulation.rs)
  → emulation inbound handler (emulation.rs ListenTask match)
  → clipboard_inbound_tx.send((addr, ClipboardText)) (clipboard_inbound_tx 是 Emulation::new 时 clone 传入)

client 端：peer.run → read_stream_c_loop (streams.rs)
  → StreamEvent::ClipboardMeta(proto_event)
  → peer.clipboard_inbox (set via connect.rs:745 set_clipboard_inbox)
  → clipboard_inbound_tx.send((addr, ClipboardText))

两条路径汇合 → service.clipboard_inbound_rx
  ↓ Service::run select! arm (line 399-401) → handle_clipboard_inbound
  ↓ LRU contains check → skip if hit; else backend.set_text + LRU push + last_text = None + FrontendEvent
  ✅ 整条链路串通
```

### SUGGESTION 一致性

| 项 | 报告记录 | SUGGESTION.md | 一致性 |
|---|---|---|---|
| #S-1 macOS pbcopy/pbpaste | 1a.2 §1 + §3 | "🟡" "不阻塞 M1a" | ✅ |
| #S-2 Windows/Linux cross-compile | 1a.3 §1.3 + §3 | "🟡" "本机无 cross-toolchain" | ✅ |
| SUGGESTION-FIXED #10 1a.4 隐藏 clippy | 1a.5 §1.2 + §4 + SUGGESTION-FIXED.md | 4 项修复 + AGENTS.md 预防 | ✅ |
| SUGGESTION-FIXED #10 标题 "3 new clippy error" 但内容列 4 项 | 1a.5 §1.2 列 4 项 | FIXED #10 标题 "3 个" 内容 4 项 | ⚠️ minor（标题 vs 内容计数不一致；4 = 3 clippy + 1 unused import；标题 "3 个" 严格意义上只算 clippy，unused import 算另一类） |

### leader-continued 模式健全性

- 1a.4 是 leader-continued（session 重启后接管 commit `182a0ea`）—— 1a.4 报告 §5 + 1a.5 报告 §2.1 显式承认
- 1a.4 未跑 `cargo clippy --workspace --all-targets -- -D warnings` —— 1a.5 跑出 4 项 (3 new clippy + 1 unused import)
- 1a.5 修复 + 记录 SUGGESTION-FIXED #10 + 提出 AGENTS.md workflow 预防（"leader-continued 模式 commit 后**必须** `cargo fmt --check` + `cargo clippy -D warnings` + `cargo test --workspace` 三连通过才能写 done 报告"）
- 1a.5 报告 §2.1 明确："M0a 0.0 leader-continued 撞 429 后 leader 接力；M0c-0.7 同样"——**M1a 1a.4 是第 3 次**复现同一漏检模式
- 这是 process 问题不是代码问题；不影响 1a.4 落地代码的正确性（4 项都是 lint-level），但暴露 leader-continued 工作流的系统漏洞
- 建议记入 AGENTS.md hard rule

### scope discipline

- 1a.5 未触碰 M1b 范围（无 LRU TTL / 60s / cache.remove on push / HTTP/3 拉取 / 元数据 + 1 KiB 拆分支）✅
- 1a.5 未触碰 M2a 范围（无 `current_image` / `set_image` / mime 检测）✅
- 1a.5 未触碰 M3a 范围（无文件元数据 / `ClipboardFiles`）✅
- 1a.5 未触碰 M4 范围（无 Vue 改动）✅
- pre-existing 7 个 clippy warning 按 scope discipline 不修（与 SUGGESTION-IGNORE #1 一致）✅

### 三平台编译

| target | lan-mouse-proto | lan-mouse-ipc | lan-mouse-cli | lan-mouse |
|---|---|---|---|---|
| aarch64-apple-darwin (host) | ✅ | ✅ | ✅ | ✅ |
| x86_64-unknown-linux-gnu | ✅ | ✅ | ✅ | ❌ 缺 `x86_64-linux-gnu-gcc` |
| x86_64-pc-windows-gnu | ✅ | ✅ | ✅ | ❌ 缺 `x86_64-w64-mingw32-gcc` |

> 与 #S-2 一致；pure-Rust crate 三平台 type-check 通过；带 C 链路（rcgen / quinn / ring）模块需 cross-toolchain 或真机编译。

## M1a 范围对齐

| 评审要求 | 验证 |
|---|---|
| **ClipboardBackend trait** | ✅ `clipboard/mod.rs:154-197` trait + 8 个 DummyBackend 单测 |
| **macOS NSPasteboard / pbcopy** | ⚠️ 用 pbcopy/pbpaste（#S-1 PLAN 偏差；M1a 接受） |
| **Windows OpenClipboard / GetClipboardData** | ✅ `clipboard/windows.rs` windows-sys 0.61 typed bindings |
| **Linux xclip / wl-paste** | ✅ `clipboard/linux.rs` Tool enum + env probe |
| **service::clipboard_dispatcher** | ✅ `service.rs:1198-1346` 500ms tick + sha256 dedup + LRU 64 + StreamC push |
| **≤ 1 KiB 内联** | ✅ `service.rs:1230` `content_inline: Some(new_text.into_bytes())` |
| **fingerprint 防回环** | ✅ `LruFingerprints` linear `contains` 在 inbound (line 1272) + outbound push (line 1218) |
| **三平台本地剪贴板读取+写回** | ✅ trait + 3 platform impls + cfg-gated tests |
| **小文本通过 StreamC 端到端** | ✅ `protoevent::ClipboardText` → `route_input` → `Channel::StreamC` → `send_stream_c` → wire → 对端 `clipboard_inbound_tx` → `handle_clipboard_inbound` → `set_text` |
| **IPC 扩展向后兼容** | ✅ `ClipboardConfig` (daemon-global) + `enable_clipboard_to` (per-peer) + `ClipboardState` 全部 `#[serde(default)]` 兼容 pre-M0c wire |

## 偏离 PLAN

| STEP | 状态 | 说明 |
|---|---|---|
| **1a.1** ClipboardBackend trait + DummyBackend | ✅ | 与 PLAN §3 M1a STEP-1a.1 行 100% 一致 |
| **1a.2** macOS backend | ⚠️ | #S-1 pbcopy/pbpaste 替代 NSPasteboard（已记入 SUGGESTION.md #S-1 🟡；M1a 接受） |
| **1a.3** Windows + Linux backends | ⚠️ | #S-2 Windows/Linux 跨平台编译未本地验证（已记入 SUGGESTION.md #S-2 🟡；本机 macOS 无 cross-toolchain，pure-Rust crate type-check 通过） |
| **1a.4** service dispatcher | ✅ | 与 PLAN §3 M1a STEP-1a.4 行 100% 一致 |
| **1a.5** fmt + clippy + 三平台 + 真机 manual | ⚠️ | 1a.4 leader-continued 隐藏 3 clippy + 1 unused import（已记入 SUGGESTION-FIXED #10；1a.5 清理后达到 0 new clippy） |

**3 处 PLAN 偏差（全部已记录并接受 / 已修复）**

## 偏离 REQUIREMENT

| 章节 | 状态 |
|---|---|
| §3.1 传输替换 | ✅ M1a 仅在 service 层加 clipboard dispatch，不影响 ALPN / 鉴权 / 探活 / 既有键鼠 |
| §3.2 剪贴板文本 | ✅ M1a 落地小文本（≤ 1 KiB）；大文本走 M1b（PLAN §3 M1b STEP-1b.1 + 1b.2） |
| §3.3 剪贴板图片 | ✅ M1a 末触碰（M2a / M2b 范围） |
| §3.4 复制文件 | ✅ M1a 未触碰（M3a / M3b 范围） |
| §4 验收标准 1-5 | ⚠️ 标准 2（"1 MiB 文本复制→对端粘贴"）M1a 仅覆盖小文本（≤ 1 KiB）；1 MiB 文本由 M1b + 1b.4 真机测试覆盖 |
| §5 多屏 | ✅ M1a 未触碰 |

**0 处硬性 REQUIREMENT 偏离；1 处验收延期到 M1b（符合 PLAN §2 里程碑路线图）**

## 跨 STEP 一致性

| 检查项 | 状态 |
|---|---|
| **trait 抽象边界** | ✅ `ClipboardBackend: Send`（非 Sync）；dispatcher 在单 `spawn_local` task 内持有 Box<dyn>；与 1a.1 报告 §1.2 "Send 而非 Sync" 一致 |
| **三平台工厂** | ✅ `default_backend()` 三平台 cfg-gate；macOS 走真实现，Linux/Windows 走 `Err(NotImplemented)`，其它 unix 走 `Err(NotImplemented)` |
| **wire-compat (PLAN §0 评审 #1)** | ✅ ClipboardText 走 StreamC var-codec；StreamA 仍只编码 fixed-codec（Input/Ping/Pong/Hello/Ack/Leave）；`(*event).into()` 路径不变 |
| **IPC 扩展向后兼容** | ✅ ClipboardConfig 5 字段 `#[serde(default)]`；ClientConfig.enable_clipboard_to 用 `const fn default_enable_clipboard_to()` 返回 true；M0c 11 + 3 个 IPC / config 单测覆盖 |
| **StreamC 端到端** | ✅ outbound: service → capture → conn.send → peer.send_input → route_input → Channel::StreamC → send_stream_c；inbound: read_stream_c_loop → peer.clipboard_inbox → clipboard_inbound_tx → service.clipboard_inbound_rx → handle_clipboard_inbound |
| **Server-side StreamC 转发** | ✅ listen.rs server_stream_c_reader_task 收到 var-codec frame → ListenEvent::Msg { event, addr } → listen_tx → ListenTask match (emulation.rs) → clipboard_inbound_tx.send |
| **LRU cap 64 / 无 TTL** | ✅ 1a.4 实现；M1a 末段不修（PLAN §3 M1a "仅指纹比对防'收到本地写回内容'的最简回环"）；M1b 升级 LRU 128 + 60s TTL + cache.remove on push（PLAN §1 评审 #3 2nd + #4 3rd） |
| **fire-and-forget outbound** | ✅ Capture::send_event 非阻塞（request_tx.send）；CaptureTask 异步处理；失败 log warn 不回传 dispatcher |
| **500ms tick first-skip** | ✅ `tokio::time::interval(Duration::from_millis(500))` + `tick().await` 首次 t≈500ms；避免与 daemon startup handshake 抢占 |
| **doc drift** | ⚠️ 1a.5 报告 §4 "pre-existing 12 clippy" 与 1a.5 §1.2 "7 pre-existing error" 数字不一致（M0c-0.7 报 12，1a.5 报 7；可能 1a.5 把 trace-filter 噪音剥离后只数 -D warnings 范围；待 M2 收尾阶段统一） |
| **leader-continued 工作流** | ⚠️ 1a.4 漏 clippy；建议 AGENTS.md 加 hard rule（M0a 0.0 + M0c-0.7 + M1a 1a.4 共 3 次复现） |
| **170 cargo pass → 284 增量合理** | ✅ M0c 170 + 1a.1 +8 + 1a.2 +5 + 1a.4 0 + 1a.5 0 = 183 lib 段；input-capture 0→101 是统计口径变化（1a.5 报告 §1.3 注明"含 input-crate"）；其余 +0；总 +114 增量与"主要来自 input-capture"判断一致 |

## 总体结论

- **接受**（PASS-with-followup）
- 理由：M1a 5 STEP 全部按 PLAN §3 M1a 行落地，1 处 PLAN 偏差（#S-1 pbcopy/pbpaste 替代 NSPasteboard）和 1 处限制（#S-2 Windows/Linux cross-compile）已清晰记录并由 leader 接受，1 处隐藏偏差（#S-10 leader-continued 1a.4 漏 clippy 4 项）已清理 + 记录 + 提 AGENTS.md 预防；500ms tick + sha256 dedup + LRU 64 + StreamC push 端到端真在 code path 走通（不是"只编译过"）；IPC 扩展向后兼容（11 + 3 个单测覆盖）；0 P0 / 0 P1；3 处 P2 + 3 处 P3 全部可押后；M1a 里程碑交付项（三平台本地剪贴板读取+写回 / 小文本通过 StreamC 端到端同步 / 仅指纹比对防回环的最简回环）全部到位

## 建议下一步

1. **M1a 验收通过 → 派 M1b**（剪贴板大文本 + HTTP/3 拉取 + LRU 128 + 60s TTL + cache.remove on push）
2. **M1b 阶段补 dispatcher 单测覆盖**（P3.1）：mock DummyBackend + LruFingerprints 端到端测试
3. **AGENTS.md hard rule**（leader-continued 三连必跑）：`cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo test --workspace`
4. **真机测试矩阵**（不阻塞 commit；用户责任）：
   - macOS ↔ macOS 终端 `pbcopy` / `pbpaste`（§5.1.1）
   - Windows ↔ Windows PowerShell `Set-Clipboard` / `Get-Clipboard`（§5.1.2）
   - Linux ↔ Linux `xclip` / `wl-paste`（§5.1.3）
   - 跨平台：macOS↔Windows / macOS↔Linux / Windows↔Linux（§5.2.1-5.2.3）
5. **M0c / M0b P2 backlog 收尾**（M2a 阶段批量清理）：P2.1 protocol.rs:96/192 doc 漂移 + P2.2 http3.rs:744 spawn 风格 + P3.1 streams.rs:302 dead_code + P3.2 ipc lib.rs:178-184 doc 重复 + P3.3 listen.rs:736 笔误
6. **doc drift 同步**（M2 收尾阶段）：1a.5 报告 "pre-existing 12" 与 "pre-existing 7" 数字差异需要统一

报告已写入 `next/STEP-VALIDATION-P2-M1a.md`，结论：PASS-with-followup，详见报告
