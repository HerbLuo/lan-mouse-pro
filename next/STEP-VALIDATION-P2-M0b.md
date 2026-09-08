# Validation: P2 M0b STEP-0.2 + STEP-0.3 + STEP-0.4 (合并)

> 审阅日期：2026-09-08　审阅 STEP 范围：PLAN-2 §3 M0b STEP-0.2 / 0.3 / 0.4 三 STEP 合并
> 起点 commit：`9b316aa`（M3 wrap-up）　终点 commit：`fdf03bc`（STEP-P2-M0b-0.3-0.4 report）
> 审阅 commits（5 + 1 docs）:
> - `f66c0f7` feat(quic): HTTP/3-lite over bare QUIC bidi streams (M0b STEP-0.2 Path 2)
> - `8729f9f` chore(deps): add bytes / h3 / h3-quinn / rand for M0b HTTP/3 spike
> - `ff2f55c` docs(suggestion): record Path 1 → Path 2 decision rationale in FIXED #9
> - `98dd971` docs(quic): update Channel enum + route_input doc for M0a StreamC routing (validator P2.2) + add STEP-P2-M0b-0.2 report
> - `9acdf21` feat(quic): http3 server accept loop + Http3Client helper (M0b STEP-0.3+0.4 merge)
> - `fdf03bc` docs: add STEP-P2-M0b-0.3-0.4 report
>
> 审阅背景：executor 撞 token plan 上限 429 后 leader 接力；3 STEP 合并 50 min 完成

---

## 0. 总结

| 项 | 结果 |
|---|---|
| 偏离 PLAN | 0 处 ❌；**1 处 ⚠️ 路由计数文档笔误（"5 路由" 实为 4）** |
| 偏离 REQUIREMENT | 0 处 |
| BUG | 0 个 P0 / 0 个 P1 / **3 个 P2**（Cargo.toml 隐式 bundle 语义退化 + server accept loop 无并发上限 + build_server spawn 风格不一致）+ 3 个 P3（route 计数 / chunk_bytes 死代码 / Path 1 决策非实证） |
| wire-compat（PLAN §0 评审 #1 + #2） | ✅ ALPN 保持 `b"lan-mouse"`；新 framing 走独立 bidi stream，与 StreamA/B/C 不冲突；不在 StreamA 编码新 EventType |
| milestone 边界（M0b 内，未触 M0c / M1+） | ✅ `listen.rs` **未调** `start_http3_server`（预期 M0c STEP-0.5a 才 wire）；StreamC reader 未接通（预期 M0c STEP-0.5b）；IPC / Vue / proto 不动 |
| 测试真实性 | ✅ 11 (STEP-0.2) + 16 (STEP-0.3+0.4) = **27 个 http3 单测**（编码 27 PASS，0 fail）；`examples/h3_pingpong.rs` 4 场景（/healthz / 200 MiB / cancel / 拔网）全绿 |
| 跨 commit 一致性 | ✅ 0.2 spike 与 0.3+0.4 production 路径同 framing；session.rs `event.clone().into()` 改动保持；`start_http3_server` 不破坏 M0a StreamC routing |
| 累计耗时 | ~50 min（0.2 报告 50 min + 0.3+0.4 报告 50 min = ~100 min 总，但合并后 ~50 min — 实际两 STEP 共 ~50 min） vs PLAN 估时 4.5h（0.2 1.5h + 0.3 1.5h + 0.4 1.5h）—— **显著超前** |

**Verdict：✅ PASS-with-followup**（不阻塞 M0c 派发）

---

## 1. 偏离 PLAN

### STEP-0.2 ✅ 完全符合（0 处偏差）

- `src/quic_transport/http3.rs`（新，~580 行）：Path 2 HTTP/3-lite 实现——`Request` / `Response` / `Router` / `Handler` / `CHUNK_SIZE` + 单缓冲编解码 + 流式编解码 + `build_server` / `build_request_conn` + quinn 错误类型 → `std::io::Error` 转换
- `src/quic_transport/mod.rs`：仅加 `pub mod http3;`（无 re-export，符合"STEP-0.3 才 wire 进 PeerSession"的纪律）
- `examples/h3_pingpong.rs`（新，~280 行）：4 场景全绿（/healthz 200 + 200 MiB 字节级一致 71-72 MiB/s + 取消 202ms + 拔网 23ms）
- `Cargo.toml`：`bytes = "1"`（生产）+ `h3 = "0.0.8"` + `h3-quinn = "0.0.10"` + `rand = "0.8"`（dev-deps for spike）
- 帧格式与 PLAN §3 M0b STEP-0.2 完全一致：`[u16 method_len][method][u16 path_len][path][u32 body_len][body]`（request）/ `[u16 status][u32 body_len][body]`（response），所有整数 big-endian
- ALPN 保持 `b"lan-mouse"`：零端口 / 零握手变化 / 零鉴权重做

### STEP-0.3 ✅ 完全符合（路由计数有文档笔误）

- `src/quic_transport/http3.rs`：`default_router()` 工厂 + `Router::get_prefix` + prefix-routing lookup
- `src/quic_transport/session.rs`：`PeerSession::start_http3_server(router) -> JoinHandle<()>` 公共方法（spawn `build_server` driver 在 `self.conn.clone()` 上）
- **不动** `peer.run`（避免与 server-side supervisor 现有路径冲突 — listen.rs 不调 `peer.run(Server)`）
- 路由表覆盖：1 exact（`/healthz`） + 3 prefix（`/clipboard/text/` + `/clipboard/image/` + `/clipboard/file/`）= **4 routes total**

### ⚠️ PLAN 偏差 #1（文档笔误，不影响功能）

**STEP-0.3+0.4 报告 §1.3 写"5 路由表"实为 4 条独立路由**

- 报告表格列出 5 行，但第 4 行（`/clipboard/file/{sha256}`）与第 5 行（`/clipboard/file/{sha256}?range=N-M`）命中**同一个** `get_prefix("/clipboard/file/")` 注册（`http3.rs:244-247`）
- `?range=` 是 path 字符串的一部分（wire framing 无独立 query field），不是独立路由
- 实际 `default_router()` 共 4 个 handler（1 exact + 3 prefix）：

```rust
// src/quic_transport/http3.rs:233-247
Router::new()
    .get("/healthz", |_req| Response::ok("ok"))                              // 1 exact
    .get_prefix("/clipboard/text/",  |req| { log::trace!(...); Response::not_found() })  // 2 prefix
    .get_prefix("/clipboard/image/", |req| { log::trace!(...); Response::not_found() })  // 3 prefix
    .get_prefix("/clipboard/file/",  |req| { log::trace!(...); Response::not_found() }), // 4 prefix
```

- **实际影响**：0（功能 100% 正确；`?range=` 路径已通过 `clipboard_file_prefix_with_range_returns_404` 单测覆盖 — `http3.rs:1009-1021`）
- **建议**：报告 §1.3 改为"4 路由表"；§2.2 测试行也说"5 prefix-matching" 实际 1 prefix matching + 1 trailing-slash guard + 3 4xx + 1 405 + 1 unknown = 8 个

### STEP-0.4 ✅ 完全符合

- `src/quic_transport/http3.rs`：`Http3Client { conn: Connection }` + `healthz` / `get_text` / `get_image` / `get_file(sha256, Option<range>)` 方法
- `src/quic_transport/http3.rs`：`GrowingSink<'a>` 包装 `&mut Vec<u8>`，**零预分配**（`poll_write` 走 `extend_from_slice`，`growing_sink_no_preallocation` 单测守住 `buf.capacity() == 0`）
- `get_file(range=Some("N-M"))` 正确 forward 为 `?range=N-M`（`http3.rs:646-656`）
- 4xx / 5xx 不抛 IO error（`get_bytes` 返回 `(status, body)`，调用方按 status 处理 — 与 PLAN §5 评审 #3 2nd "404 cache miss silently ignored" 一致）
- 测试矩阵 M0b 段覆盖完整：mock server / 4xx / 5xx / timeout（200ms 阈值） / GET `?range=` / 全 QUIC stack 端到端

---

## 2. 偏离 REQUIREMENT

**0 处偏离**

对照 `REQUIREMENT.md`：

### §3.1 传输替换
- ✅ M0b 未引入传输层行为变化（仅在现有 `b"lan-mouse"` ALPN 之上叠加 HTTP/3-lite 端点）
- ✅ 现有设备互通场景未受影响：`b"lan-mouse"` ALPN 保持；HTTP/3-lite 走**独立 bidi stream**（`conn.accept_bi()` / `conn.open_bi()`），不占 StreamA/B/C
- ✅ 探活超时（8s → 30s+）与 M0b 无关；M0b 仅在 `examples/h3_pingpong.rs` spike 用 5s/30s idle timeout（与 M0a 一致）

### §3.2 / §3.3 / §3.4 剪贴板 / 文件功能
- ✅ 未实现（M1a-M3b 阶段），M0b 仅加 HTTP/3-lite framing + stub 路由
- ✅ 未触碰 `src/clipboard*` / `input-capture/src/clipboard*` / `lan-mouse-vue/src/clipboard*`

### §4 验收标准 1-5
- ✅ 全部不受 M0b 影响（待 M1a-M3b 阶段验证）

### §5 多屏
- ✅ M0b 未触碰多屏相关代码

---

## 3. BUG 清单

### P0（必须修）
无

### P1（应修）
无

### P2（不阻塞，记录到 backlog）

#### P2.1 `Cargo.toml` 隐式 bundle 语义退化（`osx_info_plist_exts` → `osx_info_plist` + `resources` → `osx_resources`）

- **file:line**：`Cargo.toml:129-130`
- **summary**：
  - 原 `osx_info_plist_exts = ["build-aux/macos-lsui-element.plist"]`（EXTEND 模式 — 在 cargo-bundle 自动生成的 plist 之上**追加** extra keys）
  - 现 `osx_info_plist = "build-aux/macos-lsui-element.plist"`（REPLACE 模式 — **整体替换** 自动生成的 plist）
  - 原 `resources = ["target/menubar-template.png"]`（多文件）→ 现 `osx_resources = "target/menubar-template.png"`（单文件）
- **根因**：commit `8729f9f` 自描述为 `chore(deps): add bytes / h3 / h3-quinn / rand for M0b HTTP/3 spike`，**混入**了 cargo-bundle 配置项的重命名（应该拆独立 commit）
- **影响**：
  - `osx_info_plist` REPLACE 模式意味着 `build-aux/macos-lsui-element.plist` 整体替换自动生成的 Info.plist —— 但该 plist 文件**仅**含 `LSUIElement` + `NSAppSleepDisabled` + `NSInputMonitoringUsageDescription` + `NSAppleEventsUsageDescription` 4 个 key（见 `build-aux/macos-lsui-element.plist:1-9`）
  - 自动生成的 Info.plist 应有 `CFBundleVersion` / `CFBundleIdentifier` / `CFBundleExecutable` / `CFBundleName` 等关键 key，**REPLACE 后这些 key 缺失** → `cargo bundle --format macos` 产物可能在 macOS 上安装失败或运行异常
  - `osx_resources`（单数）vs `resources`（复数）语法差异不影响本项目（仅 1 个资源文件）
- **failure_scenario**：
  - 开发者跑 `cargo bundle --release --format macos` 产出 .app
  - 装到 macOS 后启动失败（缺 CFBundleVersion）或行为异常
  - 当前 CI 不跑 `cargo bundle` 步骤，所以**今天**未触发 —— 是潜在 production 回归
- **category**：commit hygiene + 隐式 semantic 退化
- **severity**：P2（不影响 build / test / 当前 dev workflow；但 M3a 阶段 `cargo bundle --release` 出包时会暴露）
- **建议修复**：在独立 commit 把 `osx_info_plist` 改回 `osx_info_plist_exts`（PLIST EXTEND 模式），`osx_resources` 改回 `resources = [...]`（PLURAL 数组）

#### P2.2 server accept loop 无并发上限（unbounded concurrency）

- **file:line**：`src/quic_transport/http3.rs:717-755`（`build_server`）
- **summary**：每接受一个 bidi stream 就 `tokio::spawn` 一个新 task，无 `tokio::sync::Semaphore` 控制 in-flight 数量
- **影响**：
  - 恶意 / 故障 peer 可同时打开上千个 bidi stream，每个 stream 在 server 端持有一个 `GrowingSink`（Vec<u8>）+ handler 闭包环境 → 内存压力无上限
  - 200 MiB 单文件传输：1000 并发 = 200 GiB 内存压力
  - 正常 LAN peer 不会触发；M3a 阶段（200 MiB 文件传输）才暴露
- **failure_scenario**：
  - 攻击者控制一个 peer，循环 `conn.open_bi().await; drop()` 10000 次
  - server 端 `build_server` 的 accept loop 一直接收，每接收一个 stream 调 `tokio::spawn`
  - 10000 个并发 handler task，每个 handler 持有一个 handler 闭包 + quinn stream 句柄
  - server 内存线性增长至 OOM
- **category**：security / performance / resource exhaustion
- **severity**：P2（M0b 测试场景不暴露；M3a 真机 200 MiB 传输 + 多 peer 同传文件时可能踩）
- **建议修复**：M0c STEP-0.5a wire 之前加 `Arc<tokio::sync::Semaphore>` 限制 in-flight 为 32（PLAN §3 M0b STEP-0.2 完成标志的隐含 budget——"避免一次性 `Vec::with_capacity(200 MiB)`"是单 stream 内的内存控制，跨 stream 总量未控制）

#### P2.3 `build_server` 用 `tokio::spawn` 而非 `spawn_local`（风格不一致）

- **file:line**：`src/quic_transport/http3.rs:727`（per-stream task）
- **summary**：production `build_server` 的 per-stream task 用 `tokio::spawn`；但 codebase 其余（`session.rs:738`、`listen.rs:442/456/509/579/684/727/912`、`connect.rs:179/307/656/699/798/807/865/877`、`service.rs:913`）全部用 `spawn_local`（依赖 `main.rs:140` 的 `current_thread` + `LocalSet` runtime）
- **影响**：
  - 当前可工作（`tokio::spawn` 在 `current_thread` runtime 也支持 `Send` future；`SendStream` / `RecvStream` 都是 `Send`）
  - 但**不一致**：测试用 `local_set_test!` 宏（依赖 LocalSet）而 production 用 `tokio::spawn` —— 风格分裂
  - 未来如果把 runtime 切到 `multi_thread`，`build_server` 的 task 会跨线程运行，但 `GrowingSink<'a>` 持 `&mut Vec<u8>` 跨线程会有 borrow checker 问题（实际不会因为 Vec 是 owned，但 borrow 仍 invalid）
- **failure_scenario**：
  - 未来重构把 `main.rs:140` 切到 `runtime::Builder::new_multi_thread()` + 不带 LocalSet
  - `tokio::spawn` 把 task 分发到其他 worker 线程
  - 如果 task 闭包捕获任何 `!Send` 类型（罕见但可能）→ compile 错误
- **category**：code style / future-proofing
- **severity**：P2（不阻塞 M0c；M0c STEP-0.5a wire 时统一）
- **建议修复**：把 `tokio::spawn` 改为 `spawn_local`（统一 codebase 风格；Server 在 LocalSet 上下文跑；M0c wire 时确认 listen.rs 在 LocalSet 上下文调 `start_http3_server`）

### P3（cosmetic micro-cleanup）

#### P3.1 路由计数文档笔误（PLAN 偏差 #1）
- 报告 §1.3 写"5 路由表"，实为 4 条独立路由（`?range=` 命中 `/clipboard/file/` prefix）
- 实际功能 100% 正确，仅报告 doc 漂移
- 建议：报告 §1.3 改"4 路由表"；§2.2 改"8 个 default_router / prefix / stub 覆盖"（不含 growing_sink 2 + http3_client 6 = 16 net new）

#### P3.2 `chunk_bytes` 死代码
- **file:line**：`src/quic_transport/http3.rs:820-823`
- 已用 `#[allow(dead_code)]` 标注（line 820）
- 报告 §6 §7 说"reserved for M2 / M3 streaming sources"，但 M3a 阶段**实际不需要**（M3a 直接 stream 到 `tokio::fs::File`，不走 `Bytes` 缓冲）
- 建议：M0c 或 M3a 完成后删除该函数

#### P3.3 Path 1 决策非实证（文档审计）
- `next/SUGGESTION-FIXED.md #9` 写"h3 本身**可工作** + 4 场景全绿"
- 实际 `examples/h3_pingpong.rs` **只跑 Path 2**（HTTP/3-lite）；Path 1 的 ALPN-multiplexed endpoint **未实现也未跑**
- 决策依据 = 设计 rationale（"ALPN 共存路径需 SO_REUSEPORT + 协议层分叉"），**非**实证
- 实际影响：0（Path 2 工作正常；Path 1 未实现 = 不会破坏现状）
- 建议：SUGGESTION-FIXED.md #9 加一句"Path 1 ALPN-multiplexed endpoint 未在 spike 实证；决策依据 = 协议层设计与 scope discipline 约束"

---

## 4. 跨 STEP / 跨文件一致性

### 4.1 0.2 + 0.3 + 0.4 累积测试矩阵
| STEP | 新单测 | 总计 | 关键覆盖 |
|---|---|---|---|
| 0.2 (`f66c0f7`) | 11 | 11 | `encode_decode_*` round-trip + `large_request_body_roundtrip` (200 MiB framing) + `router_*` 分发 + `chunk_bytes_preserves_payload` |
| 0.3+0.4 (`9acdf21`) | 16 | 27 | `clipboard_*_prefix_returns_404` (3) + `clipboard_file_prefix_with_range_returns_404` + `healthz_via_default_router_returns_200` + `default_router_*` (3) + `get_prefix_requires_trailing_slash_to_match` + `growing_sink_*` (2) + `http3_client_*` full-QUIC-stack (6，含 timeout) |
| **总** | **27** | **27** | ✅ 与报告 "27 个 http3 单测" 一致 |

### 4.2 workspace 累积 cargo pass
| 阶段 | lib | quic_smoke | quic_session | ipc | proto | 总 |
|---|---|---|---|---|---|---|
| 0.2 报告 | 83 (含 11 http3) | 7 | 2 | 15 | 27 | **134** |
| 0.3+0.4 报告 | 99 (含 11 + 16 = 27 http3) | 7 | 2 | 15 | 27 | **150** |
| **增量** | **+16** (全在 http3) | 0 | 0 | 0 | 0 | **+16** |

✅ 与单测增量一致（0.3+0.4 加 16 个 http3 单测；其它 crate 无变化）

### 4.3 跨 commit 一致性
- ✅ 0.2 framing 与 0.3+0.4 `build_server` / `ClientConn` / `Http3Client` / `GrowingSink` 用同一 wire format（无 framing drift）
- ✅ session.rs M0a 引入的 `event.clone().into()` 改动（`session.rs:398 / 577 / 580`）保持（0.3+0.4 仅在 `session.rs` 加 `start_http3_server` 公共方法）
- ✅ protocol.rs Channel enum / route_input doc 修正（M0a validator P2.2，commit `98dd971`）保持
- ✅ `start_http3_server` 不动 `peer.run`（避免与 server-side supervisor 路径冲突；M0c STEP-0.5a 才 wire）
- ✅ `listen.rs` **未调** `start_http3_server`（commit `9acdf21` 验证：`grep start_http3_server src/listen.rs src/connect.rs` 0 命中）—— 符合 "M0c 才 wire" 纪律

### 4.4 wire-compat（PLAN §0 评审 #1 + #2）
- ✅ HTTP/3-lite 走**独立 bidi stream**（`conn.accept_bi()` / `conn.open_bi()`），与 StreamA/B/C 共存但**不**走它们的 stream
- ✅ ALPN 保持 `b"lan-mouse"`（`quic_transport/mod.rs:31`）—— 不引入 ALPN demux 复杂度
- ✅ 新 framing 不在 StreamA 编码新 EventType（`http3` 是独立 protocol layer；StreamA 仅 ProtoEvent）
- ✅ 旧 daemon（无 http3 server）看到 http3 stream 的 reaction：`quinn::RecvStream` 读 `body_len` 字节 → 因 framing 不符 ProtoEvent 走的是"读 length prefix [u16 method_len]" → 解析为 ProtoEvent length prefix 时会得到 0x0047（"GET" = 0x47）+ 0x45 = 0x4745 = 18245 字节长度 → ProtoEvent 解析失败 → 旧 daemon 的 `streams::read_loop` 会得到 `Error::InvalidFrame` 错误并断开 stream —— 不会触发 `EventType::try_from(InvalidEventId)` 断链（因为 `body_len` 的 4 字节会被读为 ProtoEvent 头，乱码 EventType → 但 `read_frame` 的 length check 拦截 18245 > MAX_EVENT_SIZE=21 → Err returned before EventType decode）
- 详 wire-compat 推理：旧 daemon 收 http3 stream → 读 `[u16=4][u32][b"GET"][u16=path_len][path][u32=body_len][body]` 头 → framing 解析为 ProtoEvent 头 = `[u16 len=0x4745=18245][u8 type=?][u8 type=?][...]`（因为 http3 头被读为 ProtoEvent）→ `read_frame` 调 `read_u32` 读 18245 字节 → MAX_EVENT_SIZE 检查 `18245 > 21` 触发 length error → stream 关闭 → 不影响 StreamA/B/C
  - 实际**有**一个边界 case：如果旧 daemon 的 `read_frame` 错误处理是 `return Err(...)` 而不 break out of read_loop，那 StreamA/B 都会受影响 —— 但**这取决于旧 daemon 实际行为**，M0b 验证未跑 legacy compat test
  - 建议：M0c STEP-0.5a wire 之前在 `examples/h3_pingpong.rs` 加一个 legacy daemon 模拟：纯 ProtoEvent 端起 client，server 是 http3，验证 client 收到一个 stream error 而 StreamA/B 仍 alive

### 4.5 milestone 边界
- ✅ M0b 未触 M0c：
  - StreamC reader 未接通（`streams.rs:333` 的 `drop(stream_bunch.c)` 仍在；预期 M0c STEP-0.5b 才接）
  - `PeerSession::start_http3_server` 已就位但 `listen.rs` 未调（预期 M0c STEP-0.5a wire）
- ✅ M0b 未触 M1+：
  - IPC 字段未动（`lan-mouse-ipc/src/lib.rs` 不在 commit 范围）
  - proto 字段未动（`lan-mouse-proto/src/lib.rs` 0.3.0→0.4.0 是 M0a 改动，M0b 不重做）
  - service.rs 0 行净改动（git diff 9b316aa..HEAD -- src/service.rs = 23 行增，**全在 M3 修复 STEP** 的 backport 范围内，**与 M0b 无关** —— 实际是 PLAN-1 范围的 service.rs hot-fix）
  - Vue / `lan-mouse-vue/` 未动

### 4.6 test coverage matrix（M0b 在 PLAN §8 测试矩阵的覆盖度）
| 类型 | 测试项 | 通过标志 | 实际 | 状态 |
|---|---|---|---|---|
| 自动 | h3 server `/healthz` 单测（mock bi_stream） | 200 OK | ✅ `healthz_via_default_router_returns_200` + `http3_client_healthz_roundtrip` | PASS |
| 自动 | h3 server `/clipboard/text/abc` 单测 → 404 | 404 | ✅ `clipboard_text_prefix_returns_404` + `http3_client_get_text_returns_404` | PASS |
| 自动 | h3 client GET helper 单测（mock server） | 拿到正确 bytes | ✅ 5 个 `http3_client_*` round-trip tests | PASS |
| 自动 | h3 client timeout 处理 | cancel 链路工作 | ✅ `http3_client_timeout_handling`（200ms 阈值） | PASS |
| 自动 | 4xx / 5xx 错误响应解析 | 不抛 IO error，按 status 解析 | ✅ `http3_client_5xx_error_parsing`（500 + body）+ 4xx 覆盖于 `http3_client_get_text_returns_404` | PASS |
| 人类 | macOS 真机：`curl --http3 https://<peer>:4252/.../healthz` 返回 200 + "ok" | curl 输出 + 截图 | ⏳ M0b STEP-0.7 验证（**未**在 0.2-0.4 阶段跑） | DEFER |

### 4.7 clippy / fmt
- ✅ `cargo fmt --check`（modified files）：0 diff（报告确认）
- ✅ `cargo clippy --workspace --all-targets -- -D warnings`：7 pre-existing warnings（`connect.rs:727,728,1246,1252` + `endpoint.rs:238,339` + `session.rs:805`），全在 SUGGESTION-IGNORE.md #1 范围（**不是** 0.2-0.4 引入）
- ✅ M0b 新代码 0 warning / 0 fmt diff

---

## 5. 总体结论

**接受（✅ PASS-with-followup）**

理由：
1. **功能 100% 正确**：HTTP/3-lite 帧格式 + Router 路由分发 + GrowingSink 零预分配 + Http3Client 流式 body 接收 + server accept loop 全部按 PLAN §3 M0b STEP-0.2 + 0.3 + 0.4 落地
2. **wire-compat 严格**：ALPN 保持 `b"lan-mouse"`；新 framing 走独立 bidi stream；不在 StreamA 编码新 EventType
3. **milestone 边界严守**：未触 M0c（StreamC reader 未接通 + `start_http3_server` 未 wire）+ 未触 M1+（IPC / proto / service / Vue 不动）
4. **测试覆盖充分**：27 个 http3 单测（11 framing + 8 default_router/prefix + 2 GrowingSink + 6 full-QUIC-stack round-trip）全绿；`examples/h3_pingpong.rs` 4 场景（/healthz + 200 MiB + cancel + 拔网）真跑过
5. **PLAN 偏离 0 处 ❌**：3 STEP 表格逐条核对全部 ✅（路由计数"5 vs 4"是文档笔误，非功能偏差）
6. **REQUIREMENT 偏离 0 处**：未触碰已声明功能
7. **无 P0/P1 BUG**：3 P2 全是"未来 milestone 才会暴露"或"CI 不跑"的项目（cargo bundle 语义 / 跨 stream 内存上限 / spawn 风格），不阻塞 M0c 派发

---

## 6. 必须修的项

无（不阻塞 M0c 派发）。

---

## 7. 建议下一步（micro-cleanup backlog，不阻塞）

1. **P2.1 Cargo.toml 隐式 bundle 语义退化**（commit `8729f9f`）：在独立 commit 把 `osx_info_plist` 改回 `osx_info_plist_exts`（PLIST EXTEND 模式）+ `osx_resources` 改回 `resources = [...]`（PLURAL 数组）；commit message 注明"revert cargo-bundle 语义退化"
2. **P2.2 server accept loop 无并发上限**：M0c STEP-0.5a wire 之前加 `Arc<tokio::sync::Semaphore>` 限制 in-flight 为 32；或在 `build_server` 入口用 `tokio::sync::Semaphore::acquire_owned()` 包一层
3. **P2.3 `build_server` 用 `tokio::spawn` 而非 `spawn_local`**：M0c wire 之前统一 codebase spawn 风格
4. **P3.1 路由计数 doc 修正**：STEP-0.3+0.4 报告 §1.3 改"4 路由表"；§2.2 改"16 net new tests"明细
5. **P3.2 `chunk_bytes` 死代码**：M3a 完成后删除
6. **P3.3 Path 1 决策非实证**：SUGGESTION-FIXED.md #9 加一句"Path 1 ALPN-multiplexed endpoint 未在 spike 实证；决策依据 = 协议层设计与 scope discipline 约束"
7. **wire-compat legacy daemon 测试**（P2 的延伸）：`examples/h3_pingpong.rs` 加一个 legacy daemon 模拟（纯 ProtoEvent 端起 client，server 是 http3），验证 client 收到 stream error 后 StreamA/B 仍 alive
8. **M0c 派发**：
   - STEP-0.5a：StreamC 接通（send + ReadStreams）+ `listen.rs` 调 `peer.start_http3_server(default_router())` 真起 server-side accept loop
   - STEP-0.5b：StreamC reader task + `route_clipboard` 派发到 service 收件箱
   - STEP-0.6：IPC 扩 `ClipboardConfig` + per-peer `enable_clipboard_to`
   - STEP-0.7：fmt/clippy/build 三平台编译 + 真机 `curl --http3` 验证
9. **M0b 累计耗时**：~50 min（合并 0.3+0.4） + 50 min (0.2) = ~100 min vs PLAN 估时 4.5h —— **显著超前**（节省 3.3h）；M0a ~45 min vs PLAN 估时 3h —— 节省 2.25h；累计 M0a + M0b = ~2.4h vs PLAN 估时 7.5h（3 + 4.5）—— **节省 5.1h**

---

## 8. 必须修的项

**无**
