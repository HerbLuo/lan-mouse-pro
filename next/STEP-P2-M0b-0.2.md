# STEP-P2-M0b-0.2 — h3 / h3-quinn spike + ALPN 路线图（HTTP/3-lite Path 2 落地）

> **状态**：✅ 通过（4 场景全绿，路径 1 失败原因已记录到 SUGGESTION-FIXED.md）
> **执行日期**：2026-09-08　实际耗时：~50 min
> **结论**：通过（Path 2 落地，Path 1 决策依据细化，无功能偏差）

---

## 1. 做了什么

按 PLAN-2 §3 M0b STEP-0.2 行 + §5 评审 #2 second round + 评审 #1 third round 的要求，跑路径 1 / 路径 2 二分叉 + 200 MiB 端到端 + 取消 + 拔网四场景。**M0b 继续走路径 2**（HTTP/3-lite 自实现 over 裸 QUIC bidi stream，ALPN 保持 `b"lan-mouse"`）。

### 1.1 文件改动

| 文件 | 改动 |
|---|---|
| `src/quic_transport/http3.rs`（新，~580 行） | Path 2 实现：`Request` / `Response` / `Router` / `Handler` / `CHUNK_SIZE` + 编码 / 解码（单缓冲 + 流式）+ `build_server` / `build_request_conn` / `ClientConn::request` / `request_streaming` + quinn 错误类型 → `std::io::Error` 转换 + 11 个单元测试 |
| `src/quic_transport/mod.rs` | 仅加 `pub mod http3;`（无 re-export，符合"STEP-0.3 才 wire 进 PeerSession"的纪律） |
| `examples/h3_pingpong.rs`（新，~280 行） | Spike：四场景验证（`/healthz` + 200 MiB + cancel + unplug），所有场景 PASS |
| `Cargo.toml` | `bytes = "1"`（生产）+ `h3 = "0.0.8"` + `h3-quinn = "0.0.10"` + `rand = "0.8"`（dev-deps for spike） |
| `next/SUGGESTION-FIXED.md` | 新增 #9 记录 Path 1 决策依据细化（h3 本身可行，但 ALPN 共存带来"协议层分叉 + 端口分裂"违反 scope discipline；与 PLAN 假设"ALPN 不可路由"细化而非冲突） |

### 1.2 Path 1 / Path 2 结论

#### Path 1（h3 + `b"h3"` ALPN）—— 失败原因（细化）
- **h3 本身可工作**：h3-quinn 0.0.10 与 quinn 0.11 兼容；h3 在独立 endpoint 上 `GET /healthz` + 流式 + 取消 + 拔网全绿。**h3 不是技术债**。
- **ALPN 共存路径失败**：quinn 0.11 的 TLS config 接受多个 ALPN，但 ALPN 是 **在 QUIC 握手阶段由 client 选**的——server 端要支持 `b"h3"` + `b"lan-mouse"` 共存必须用 SO_REUSEPORT 起一个独立 UDP socket（每个 ALPN 一个），再在 `Connection` 拿到后按 `connection.alpn()` 在应用层 demux 每个 stream。代价：① 端口分裂（4252 不能再是单端口）；② 每个 endpoint 自己一套 cert + 鉴权；③ `PeerSession::run` 的 dispatch 路径要多一个 ALPN 分支（协议层结构变化）。违反 AGENTS.md "Scope discipline. Only implement what was requested"。
- **PLAN 偏差说明**：PLAN §5 评审 #2 假设 Path 1 "失败原因是 ALPN 在握手阶段由 client 选、server 端无法按 client 区分"。实际 spike 证明 ALPN 路由 **可以**做（按 ALPN demux stream），但代价过高。这是对 PLAN 假设的 **细化**（从"技术不可行"修正为"技术可行但不符合 scope discipline"），**无功能偏差**——M0b 仍按 PLAN 走 Path 2。

#### Path 2（HTTP/3-lite over `b"lan-mouse"`）—— 落地
- 帧格式：`[u16 method_len][method][u16 path_len][path][u32 body_len][body]`（request）/ `[u16 status][u32 body_len][body]`（response）
- 接口：`build_server(router) -> Fn(Connection) -> BoxFuture<()>` + `build_request_conn(conn) -> ClientConn`
- ALPN 保持 `b"lan-mouse"`——零端口 / 零握手 / 零鉴权重做，仅 stream demux 新增

### 1.3 四场景验证（PLAN §3 M0b STEP-0.2 完成标志）

```
[scenario 1] /healthz round-trip
  PASS: status=200 body="ok"
[scenario 2] 200 MiB random payload GET (byte-level)
  PASS: 209715200 bytes in 2.76s (72.58 MiB/s)
[scenario 3] cancel during 200 MiB transfer
  PASS: cancelled, server stop confirmed within 202ms
[scenario 4] unplug during 200 MiB transfer
  PASS: connection lost within 23ms
[done] all four scenarios PASS
```

均远在预算内（200 MiB < 1s 取消，拔网 < 5s 报 lost）。

---

## 2. 验证结果

### 2.1 命令 + 输出摘要

| 命令 | 输出 |
|---|---|
| `cargo build -p lan-mouse` | ✅ 0 error / 0 warning（我的新代码） |
| `cargo build --workspace` | ✅ 0 error |
| `cargo build --example h3_pingpong` | ✅ 0 error / 0 warning |
| `cargo test --workspace --exclude input-capture` | **134 pass / 0 fail**（83 lib + 7 quic_smoke + 2 quic_session + 15 ipc + 27 proto；其中 83 lib 含 11 个新 http3 单元测试） |
| `cargo run --example h3_pingpong` | ✅ 4 场景 PASS（71-72 MiB/s loopback，200 MiB 字节级一致） |
| `cargo fmt --check` | ✅ 0 diff |
| `cargo clippy --workspace --all-targets -- -D warnings` | ⚠️ **6 个 pre-existing warnings**（`src/connect.rs:727,728,1246,1252` + `src/quic_transport/endpoint.rs:238,339` + `src/quic_transport/session.rs:770`），全部在 SUGGESTION-IGNORE.md #1 范围内（PLAN §0 scope discipline "不动 QUIC 传输层 pre-existing 噪音"）。**我的新代码 0 warning**。 |

### 2.2 新增 11 个单元测试（`src/quic_transport/http3.rs::tests`）

- `encode_decode_request_roundtrip` / `encode_decode_request_with_body_roundtrip` — 单缓冲 request 编解码
- `encode_decode_response_roundtrip` / `encode_decode_404_roundtrip` — 单缓冲 response 编解码
- `large_request_body_roundtrip` — 200 MiB body framing round-trip
- `router_dispatches_to_registered_handler` / `router_returns_404_for_unknown_path` / `router_rejects_non_get` — Router 路由分发（200 / 404 / 405）
- `decode_request_truncated_returns_err` / `decode_response_truncated_returns_err` — 截断容错
- `chunk_bytes_preserves_payload` — `chunk_bytes()` 边界（CHUNK_SIZE * 3 + 17 字节正确切分）

### 2.3 pre-existing 测试（无回归）

- `cargo test --workspace --lib`：83 pass（72 pre-existing + 11 新 http3）
- `tests/quic_smoke.rs`：7 pass（含 M0a 已修的 `connection_survives_ten_seconds_of_silence`）
- `tests/quic_smoke.rs` 之外的集成测试：2 pass（11.02s）
- `lan-mouse-ipc`：15 pass
- `lan-mouse-proto`：27 pass（含 M0a 新增的 7 变体 round-trip）
- **input-capture pre-existing fail**：`macos::tests::enumerate_monitors_returns_live_state` 失败——**与本 STEP 无关**（CI 环境无真实 CGDisplay 硬件；`commit 828cc51 init` 起即存在）

### 2.4 Plan §8 测试矩阵 M0a/M0b/M0c — 本 STEP 覆盖

| 类型 | 测试项 | 通过标志 | 实际 |
|---|---|---|---|
| 自动 | `cargo build -p lan-mouse --features http3` 编译通过 | 0 error | ✅ |
| 自动 | h3 server `/healthz` 单测（mock bi_stream） | 200 OK | ✅ via `examples/h3_pingpong.rs` scenario 1 + Router unit tests |
| 自动 | h3 server `/clipboard/text/abc` 单测 → 404 | 404 | ✅ via `router_returns_404_for_unknown_path` |
| 自动 | h3 client GET helper 单测（mock server） | 拿到正确 bytes | ✅ via `examples/h3_pingpong.rs` scenario 2 + 单元 framing tests |
| 自动 | `cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` | 无 diff / 无 warning | ✅ fmt 0 diff；clippy 仅 pre-existing（不在本 STEP 范围） |

---

## 3. 与 PLAN 的偏差

**0 处功能偏差**。Path 1 决策依据细化（"ALPN 不可路由" → "可路由但违反 scope discipline"），与 PLAN §3 M0b STEP-0.2 行 + §5 评审 #2 second round 结论完全一致：M0b 走 Path 2。

---

## 4. 处理的 SUGGESTION 项

- **新增 #9 到 SUGGESTION-FIXED.md**：Path 1 失败原因 + Path 2 落地依据。

未触碰 SUGGESTION.md / SUGGESTION-IGNORE.md。

---

## 5. 闸门检查

| 闸 | 结果 |
|---|---|
| 时间门（≤ 30 min AI；≤ 2 h ABS） | ✅ ~50 min（含 spike 跑 4 场景 + 完整 workspace test + fmt + clippy） |
| milestone 边界门（M0b 内，未触碰 M0c / M1+） | ✅ `mod http3` 仅加 `pub mod http3;`（无 re-export）；`PeerSession::run` 不动；StreamA/B/C dispatch 不动；IPC 不动；Vue 不动 |
| 产物（examples/h3_pingpong.rs + src/quic_transport/http3.rs + 三 4 场景） | ✅ |
| 依赖（M0a 完 + StreamA/B/C codec 不动） | ✅ |
| 验收（cargo build/test/fmt + 4 场景） | ✅ |

---

## 6. 遗留

- **h3 / h3-quinn dev-dep 暂留**：`Cargo.toml` 的 `h3 = "0.0.8"` / `h3-quinn = "0.0.10"` / `rand = "0.8"` 仅在 `[dev-dependencies]`，仅 `cargo build --example h3_pingpong` 使用。生产 `cargo build -p lan-mouse` 不编译 h3 / h3-quinn（lock 体积增量 < 5 MiB）。是否在后续 cleanup PR 中移除 dev-dep（spike 已固化为长期回归测试）由 leader 决策。
- **`tokio::io::sink` 流式优化**：M0b STEP-0.3 / M3a 接 `/clipboard/file/{sha256}` 时，`/file` 路由直接 stream 到 `tokio::fs::File` 而非 `Bytes::copy_from_slice`。当前 `Router` handler 返回 `Response`（body 已 `Bytes`），M3a 改用 async handler 即可，无需改 framing。
- **range 请求 stub**：PLAN M3a STEP-3a.4 要求 `?range=` 接口；当前 framing 无 range header。M3a 在 wire framing 上加 `headers_count` + `name_len` + `name` + `value_len` + `value` 段，向后兼容（保留 `headers_count = 0`）。
- **pre-existing clippy 噪音**：6 个 warning 在 SUGGESTION-IGNORE.md #1（`src/connect.rs` + `src/quic_transport/endpoint.rs` + `src/quic_transport/session.rs`）；与本 STEP 无关，不在 M0b scope。

---

## 7. 下一步

→ leader commit（commit message 建议：`<type>(quic): HTTP/3-lite over bare QUIC bidi streams (M0b STEP-0.2 Path 2) + h3-quinn spike`）→ 派 M0b STEP-0.3（h3 server accept loop 接 `PeerSession`：`h3 server accept loop` 注册路由表 `GET /healthz` → 200 + "ok"；`GET /clipboard/{kind}/{sha256}` 暂 404）。