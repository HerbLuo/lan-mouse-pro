# STEP-VALIDATION-P2-M0c

**Validator**: step-validator (M0c 整批审)
**日期**: 2026-09-08
**审阅范围**: 6 commit (371eee0 / 47a4e03 / 44dbd60 / 3d5ae1f / 7282756 / 86781b0)
**结论**: PASS-with-followup

## P0（阻塞，必须返工）
无

## P1（严重 bug，应该返工）
无

## P2（建议修，可押后）

### P2.1 文档漂移：`protocol.rs:96` 与 `protocol.rs:192` 仍描述 "stream C is M0c-only / M2-only" 错误返回
- **位置**: `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/quic_transport/protocol.rs:94-96` 与 `:189-192`
- **现象**: 两个 doc comment 都还描述 "until then `send_input` returns `Err(HelloFailed("stream C is M0c-only …"))`"，但 M0c 已落地，`session.rs:711` 实际代码为 `Channel::StreamC => self.send_stream_c(event).await,`，不再返回错误
- **影响**: 0（doc only；runtime 行为正确）
- **建议修复**: 把这两段 doc 改为 "StreamC is wired up in M0c STEP-0.5a+0.5b via `send_stream_c` (writer) and `read_stream_c_loop` (reader)"

### P2.2 M0b P2.3 继承：`http3.rs:744` 仍用 `tokio::spawn` 而非 `spawn_local`
- **位置**: `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/quic_transport/http3.rs:744`（P2.2 Semaphore 修复 commit `47a4e03` 引入；committer 没改 spawn 风格）
- **现象**: production `build_server` 的 per-stream task 用 `tokio::spawn`；codebase 其余（`session.rs:738`、`listen.rs:442/456/509/579/684/727/912`、`connect.rs:179/307/656/699/798/807/865/877`、`service.rs:913`）全部用 `spawn_local`
- **影响**: 当前可工作（`tokio::spawn` 在 `current_thread` runtime 也支持 `Send` future）；但风格分裂，未来切 `multi_thread` runtime 时 borrow checker 可能暴露问题
- **建议修复**: M1a 之前统一 codebase spawn 风格（M0b validator P2.3 的 followup，本批未修；不阻塞 M1a 派发）

## P3（风格 / 建议）

### P3.1 `streams.rs:302` `read_stream_c_loop` 的 `#[allow(dead_code)]` 多余
- **位置**: `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/quic_transport/streams.rs:302`
- **现象**: 函数被 `read_loop`（line 411）+ 2 个测试（lines 690, 737）调用，`dead_code` 不会触发；`#[allow(dead_code)]` 是 defensive 但实际无意义
- **建议**: 移除 `#[allow(dead_code)]`，让 compiler 守住 "public API 仍被使用"

### P3.2 `lan-mouse-ipc/src/lib.rs:178-184` `enable_clipboard_to` doc 重复
- **位置**: `/Users/hb/Projects/@cloudself/lan-mouse-pro/lan-mouse-ipc/src/lib.rs:170-186`
- **现象**: doc comment 里 `#[serde(default)]` matches the `input_channels` / `monitor` contract — backward-compatible with pre-M0c wire payloads.` 这段被写了两次
- **建议**: 删除重复段（doc string 整理）

### P3.3 listen.rs 注释 "5 路由" 笔误
- **位置**: `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/listen.rs:736`
- **现象**: 注释写 "registers 5 routes: `/healthz` 200 + 4 stubs" 但实际 1 exact + 3 prefix = 4 路由（M0b STEP-0.3+0.4 报告 P3.1 同样的笔误）
- **建议**: 改 "4 路由" 或 "1 healthz + 3 clipboard-prefix stubs"

## 验证证据

### 关键单测结果（实测）
```
cargo test --workspace --exclude input-capture：
  108 lib (lan-mouse)
  + 7 quic_smoke
  + 2 quic_session
  + 26 ipc (lan-mouse-ipc)
  + 27 proto (lan-mouse-proto)
  = 170 pass / 0 fail ✅
```

```
cargo clippy --workspace --all-targets：
  - 5 lib + 7 test warnings (5 duplicates)
  - 7 唯一 pre-existing issue locations:
    · src/connect.rs:727-728 (doc list indent)
    · src/connect.rs:1246,1252 (const assertion)
    · src/quic_transport/endpoint.rs:238 (doc list indent)
    · src/quic_transport/endpoint.rs:339 (too_many_arguments)
    · src/quic_transport/session.rs:931 (doc list indent)
  - 24 trace-filter warnings (project-wide `max_level_info` 基础设施；非新引入)
  - M0a/M0b/M0c 引入 = 0 ✅
  - 全部 pre-existing 7 个 location git blame 到 ^828cc51 (init commit, 2026-09-06)，不在 M0a/M0b/M0c scope 内
```

```
cargo fmt --check：0 diff ✅
cargo build --workspace：0 error ✅
```

### 关键单测文件 + 行号
| 测试 | 位置 | 性质 |
|---|---|---|
| `stream_c_clipboard_text_round_trip` | `src/quic_transport/session.rs:1666-1866` | **full-QUIC-stack** end-to-end（真 QUIC endpoint + 真 `PeerSession::send_input` → 真 `accept_bi` → 真 `ProtoEvent::try_from` → mpsc inbox） |
| `stream_c_frame_round_trip_clipboard_text` | `src/quic_transport/protocol.rs:1198` | codec round-trip |
| `stream_c_frame_handles_payloads_larger_than_max_event_size` | `src/quic_transport/protocol.rs:1244` | 1.2 KiB inline payload round-trip |
| `stream_c_frame_truncated_body_returns_truncated` | `src/quic_transport/protocol.rs:1298` | truncate semantics |
| `stream_c_loop_round_trip_clipboard_meta` | `src/quic_transport/streams.rs:679` | reader task unit |
| `stream_c_backpressure_blocks_when_receiver_idle` | `src/quic_transport/streams.rs:723` | backpressure contract |
| 11 × `lan-mouse-ipc` clipboard/config tests | `lan-mouse-ipc/src/lib.rs:419-622` | serde round-trip + 缺字段 compat |
| 3 × `config.rs` write-back/read-back tests | `src/config.rs:1071-1135` | TOML omit-on-default + 缺字段默认 true |

### 关键代码变更（验过不是"只编译过"）
1. **`listen.rs:748`** — `peer.start_http3_server(default_router())` 真的在 `handle_quic_peer_supervisor` 调用，server 真的起来了
2. **`listen.rs:910`** — `server_accept_bi_task` 用 length-prefix discriminator (`len > MAX_EVENT_SIZE`) 判别 Stream C
3. **`listen.rs:1012-1038`** — `server_stream_c_reader_task` 读 `read_stream_c_frame` → push `ListenEvent::Msg { event, addr }` → `listen_tx` → `ListenTask` → `EmulationEvent::Message` → service
4. **`streams.rs:411`** — `read_loop` 真正 spawn `read_stream_c_loop(bunch.c.recv, tx_c)`（替换 M0a 的 `drop(bunch.c)` 占位）
5. **`streams.rs` `StreamEvent::ClipboardMeta` variant** — 真实新增并被 reader 发出
6. **`session.rs:711`** — `Channel::StreamC => self.send_stream_c(event).await`（替换 M0a 的 `Err(stream C is M0c-only...)` placeholder）
7. **`session.rs:1175-1212`** — `PeerSession::run` 的 select! 加 Path E (stream C mpsc → `send_clipboard_inbox`) 分支
8. **`http3.rs:738-755`** — `Arc<Semaphore>` per-Connection + `acquire_owned()` 在 task 入口 + RAII release

### P2.2 Semaphore 修复质量细查
- `let semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_INFLIGHT_REQUESTS_PER_PEER));` 一次建好 per-Connection ✅
- `let semaphore = semaphore.clone();` 每次 spawn 都带 clone ✅
- `let _permit = match semaphore.acquire_owned().await { ... }` permit 绑定到 task scope；`_permit` drop 时 RAII 释放 ✅
- 32 in-flight 上限真的生效（不是 acquire 一次就放）✅
- 取消路径：`acquire_owned().await` 在 cancellation-safe 范围；permit 自动 drop，task exit ✅
- 失败路径：`write_response_streaming` 失败 / `send.finish()` 失败都不影响 permit drop（task 函数返回时 `_permit` drop） ✅

### Leader post-Leave Ack demotion (`86781b0`) 根因分析
- **race 描述正确**:
  - T0 master 发 Leave(0) → T1 master `release_capture()` → `active_client.take()` + state=Idle → T2 slave 收 Leave + 发 Ack(0) → T3 master 收 Ack(0)，`active_client.is_some()` = false，state=Idle
- **active_client.is_some() guard 仍在**（src/capture.rs:844），仍能拦住 "Pending-timeout-then-late-Ack" 旧 bug（state=Pending, timeout → Idle, late Ack 进来 → 早期 `if self.active_client.is_some()` 检查 → 没 active_client → 走 trace log，不进 `State::Sending`）
- **trace!() 而非 debug!()** 也合理：每次反向切换都会触发；debug 会污染 GUI log
- **comment 详尽**（8 行解释历史）；不引入新的 bug
- 决策方向正确

## M0c 范围对齐

| 评审要求 | 验证 |
|---|---|
| **StreamC 端到端真的工作**（不是只编译过） | ✅ `stream_c_clipboard_text_round_trip` 是真 QUIC-stack 端到端（构造 ClipboardText → `send_input` → wire → server `accept_bi` → `clipboard_inbox` 收到，断言 dbg + remote_addr 一致） |
| **IPC 扩展向后兼容** | ✅ `ClipboardConfig` 5 字段 `#[serde(default)]` + `ClientConfig.enable_clipboard_to` 用 `const fn default_enable_clipboard_to()` 返回 true；11 个 IPC 单测覆盖 缺字段/部分缺失/ partial missing；3 个 TOML write-back/read-back 单测 |
| **P2.2 Semaphore 修复质量** | ✅ `Arc<Semaphore>` 32 in-flight 上限真的生效；permit RAII release；不破坏 wire-compat；P2.3 spawn 风格分裂仍 P2 押后 |
| **leader post-Leave Ack WARN demotion 根因** | ✅ race 描述正确（T0-T3）；`active_client.is_some()` guard 仍能拦旧 bug；trace level 合理；comment 完整 |
| **scope discipline（M0a/M0b/M0c 引入 0 new clippy；pre-existing 不偷修）** | ✅ 所有 7 个 pre-existing issue blame 到 `^828cc51` (init, 2026-09-06)，不在 M0a/M0b/M0c scope 内 |

## 偏离 PLAN

| STEP | 状态 | 说明 |
|---|---|---|
| **0.5a** (StreamC send + ReadStreams) | ✅ | `cached_send_c` + `send_stream_c` + `StreamEvent::ClipboardMeta` + `ReadStreams.c/join_c` + Path E in select! — 完全符合 |
| **0.5b** (StreamC reader + http3 wire) | ✅ | `read_stream_c_loop` 替换 `drop(bunch.c)` + `listen.rs` 调 `start_http3_server(default_router())` + `server_stream_c_reader_task` + `server_accept_bi_task` StreamC 判别 — 完全符合 |
| **0.6** (IPC 扩展) | ✅ | `ClipboardConfig` + `enable_clipboard_to` + `ClipboardState` + `SetClipboardConfig` + `SetEnableClipboardTo` — 完全符合 |
| **0.7** (fmt/clippy/build + 真机 curl 文档化) | ✅ | fmt 0 diff / 170 pass / 0 fail / 0 new clippy；真机 curl 命令文档化在 STEP-P2-M0c-0.7.md（场景 A/B/C 任一） |
| **leader 直修 `86781b0`** (post-Leave Ack demote) | ✅ | scope 局部（仅 `src/capture.rs`，18 增 3 改）；不在 STEP 范围但是真实 hot-fix；根因正确 |

**0 处 PLAN 偏差**

## 偏离 REQUIREMENT

| 章节 | 状态 |
|---|---|
| §3.1 传输替换 | ✅ M0c 仅在现有 QUIC bidi 上叠加 StreamC + http3，不影响 ALPN / 鉴权 / 探活 / 既有键鼠 |
| §3.2-§3.4 剪贴板 / 文件 | ✅ M0c 仅 wire + IPC 字段，**未实现**剪贴板业务（M1a-M3b 范围）；service handler 只 persist + log，不触发后端 |
| §4 验收标准 1-5 | ✅ 全部待 M1a+ 验证；M0c 未触碰 |
| §5 多屏 | ✅ M0c 未触碰 |

**0 处 REQUIREMENT 偏离**

## 跨 STEP 一致性

| 检查项 | 状态 |
|---|---|
| **wire-compat (PLAN §0 评审 #1)** | ✅ StreamA 仍只编码 fixed-codec（Input/Ping/Pong/Hello/Ack/Leave）— `(*event).into()` 路径不变；7 个 var-codec 变体全部走 StreamC（不污染 StreamA） |
| **ALPN 不变** | ✅ `b"lan-mouse"`（M0b STEP-0.2 Path 2 决策保持） |
| **clippy scope 守恒** | ✅ M0a/M0b/M0c 引入 0 new warning；pre-existing 7 个全部 blame 到 `^828cc51` init commit |
| **fmt 一致** | ✅ workspace 0 diff |
| **Stream C var-codec wire format 跨文件一致** | ✅ `protocol.rs::write_stream_c_frame/read_stream_c_frame` ↔ `streams.rs::read_stream_c_loop` ↔ `session.rs::send_stream_c` ↔ `listen.rs::server_stream_c_reader_task` 全部用 `[u32 BE len][body bytes...]` + `Vec::<u8>::from(event)` + `ProtoEvent::try_from(&[u8])` 同一套编码 |
| **Server-side Stream C 长度判别器绝对可靠** | ✅ `ClipboardText { fingerprint: [u8;32], sha256: [u8;32] }` baseline 64B + 1B type = 65B > MAX_EVENT_SIZE=21B（FixedCodec 永远 ≤21B）；判别器 100% 准确 |
| **M0b P2.1 Cargo.toml bundle 语义退化** | ✅ 未触碰（与 M0c 无关） |
| **M0b P2.3 spawn 风格分裂** | ⚠️ 未修复（P2 押后） |
| **170 cargo pass 累计一致** | ✅ M0a 165 + M0b 27 http3 - M0b overlap - M0c new = 165 + 5 (M0b delta from 150 to 155...wait 让我重算） |

> **测试累计重算**：
> - M0a 报告：224 cargo pass（spot-check baseline）
> - M0b 报告：lib 99（+16 net new http3 单测；M0a +11 http3 = 27 http3 net）+ 7 quic_smoke + 2 quic_session + 15 ipc + 27 proto = 150 pass
> - 报告称 lib 含 11 + 16 = 27 http3 单测；lib 总 99
> - **M0c +20 新单测**：11 ipc + 3 config + 1 session (e2e) + 2 streams + 3 protocol = 20 ✅
> - **M0c 总计**：lib 99 + 20 (StreamC/clipboard) - ?? (overlap?)
> - 实测：108 lib + 7 + 2 + 26 + 27 = 170 ✅
> - 增量：lib 99 → 108 (+9)，ipc 15 → 26 (+11)，其余不变 (+0)；+9 + +11 = +20 ✅

## 总体结论

- **接受**（PASS-with-followup）
- 理由：StreamC 端到端单测是真 QUIC-stack（不是 mock）；P2.2 Semaphore 修复技术上正确；IPC 扩展向后兼容（11+3 个单测覆盖）；leader post-Leave Ack demotion 根因分析对；scope discipline 严守（0 new clippy；pre-existing 7 issue 全部 blame 到 init commit）；3 处 P3 文档漂移 + 1 处 P2.3 spawn 风格未修均不阻塞 M1a 派发

## 建议下一步

1. **M1a 派发（不阻塞）**：`ClipboardBackend` trait + macOS 实现（真机 pbcopy/pbpaste 验证）
2. **P2.3 spawn 风格统一**：M1a 之前 / 同期一并修（不阻塞，但减少未来 multi_thread runtime 切换的 borrow checker 风险）
3. **P3 文档漂移清理**：把 protocol.rs:96 / protocol.rs:192 / listen.rs:736 三处 stale doc 一次性更新
4. **人类真机 curl --http3 验证（不阻塞）**：跑 STEP-P2-M0c-0.7.md 场景 A/B/C 任一

报告已写入 next/STEP-VALIDATION-P2-M0c.md，结论：PASS-with-followup，详见报告