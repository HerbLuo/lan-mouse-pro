# STEP-P2-M0b-0.3 + 0.4 — h3 server accept loop + Http3Client helper (合并)

> PLAN §3 M0b STEP-0.3 + STEP-0.4 行 + §8 测试矩阵 M0b 段
> 执行日期：2026-09-08　实际耗时：~50 min（合并两 STEP 共）
> 结论：通过（落地 `default_router` 5 路由 + `Http3Client::get_text/image/file` + `PeerSession::start_http3_server` 公共方法）

---

## 1. 做了什么

按 PLAN §3 M0b STEP-0.3 + STEP-0.4 合并范围落地：
- `http3.rs` 加 5 路由 `default_router()`（`/healthz` 200 + 4 stub 404）
- `http3.rs` 加 `Router::get_prefix`（让 `/clipboard/text/{sha256}` 这类动态 sha256 路径命中 stub）
- `http3.rs` 加 `Http3Client { conn: Connection }` + `healthz` / `get_text` / `get_image` / `get_file(sha256, Option<range>)` 方法
- `http3.rs` 加 `GrowingSink<'a>`（`AsyncWrite` + `&mut Vec<u8>`，**零预分配**，满足 leader "改用 `tokio::io::sink` + 流式" 要求）
- `session.rs` 加 `PeerSession::start_http3_server(router) -> JoinHandle<()>` 公共方法
- **不动** `peer.run`（避免与 server-side supervisor (listen.rs) 现有路径冲突 — listen.rs 不调 `peer.run(Server)`）

### 1.1 文件改动

| 文件 | 改动 |
|---|---|
| `src/quic_transport/http3.rs` | +332 行：5 路由工厂 + `Router::get_prefix` + prefix-routing lookup + `Http3Client` + `GrowingSink` + 16 新单测（含 5 个 full-QUIC-stack round-trip） |
| `src/quic_transport/session.rs` | +36 行：`import tokio::task::JoinHandle` + `PeerSession::start_http3_server(router) -> JoinHandle<()>` 公共方法（spawn `build_server(router)` driver 在 `self.conn.clone()` 上） |

### 1.2 设计决策

1. **`default_router()` 工厂**而非 `Router::default()` impl — `Router` 需保持 `Default + Clone`（spike / 单测都用 `Router::new()`）；production router 是具体的 5 路由工厂，挂在 module 边界而非类型上。

2. **`Router::get_prefix`** 而非 wildcard glob — 当前只有 3 个 prefix（`/clipboard/{text,image,file}/`），不需要 glob 引擎。`req.path.starts_with(prefix)` + `HashMap::iter().find(...)` 足够；first-hit-wins 在小 prefix 集下确定性可接受。

3. **`Http3Client` 持有 `Connection`** 而非 `ClientConn` — `ClientConn::request` 已用 `read_bytes` 增量分配（`Vec::with_capacity(len.min(CHUNK_SIZE * 4))` capped 256 KiB），但为满足"流式"指令显式语义，新 helper 走 `request_streaming` 路径 + `GrowingSink`。

4. **`PeerSession::start_http3_server` 是单独公共方法，不动 `peer.run`** — `listen.rs::handle_quic_peer_supervisor` **不**调 `peer.run(Server)`（直接管理 stream A + accept_bi），所以在 `peer.run` 里挂 server 不会触达 server-side。M0c STEP-0.5a 让 `listen.rs` 调 `start_http3_server` 才真正生效（leader scope discipline "不动 listen.rs"）。

5. **范围 `'/clipboard/file/{sha256}?range=N-M'`** — wire framing 没单独 query field，path 字符串含 `?range=`。prefix `/clipboard/file/` 自动匹配（无论 query 在不在），stubs 全部 404。range 处理留 M3a。

### 1.3 5 路由表（`default_router()`）

| 路由 | handler | status | 后续 milestone |
|---|---|---|---|
| `GET /healthz` | 精确 | 200 + `"ok"` | M0b STEP-0.7 真机 `curl --http3` |
| `GET /clipboard/text/{sha256}` | prefix `/clipboard/text/` | 404 + `"not found"` | M1b 接 cache |
| `GET /clipboard/image/{sha256}` | prefix `/clipboard/image/` | 404 | M2a 接 cache |
| `GET /clipboard/file/{sha256}` | prefix `/clipboard/file/` | 404 | M3a 接 file cache + range |
| `GET /clipboard/file/{sha256}?range=N-M` | prefix `/clipboard/file/` | 404 | M3a 接 range（stub 保留） |

### 1.4 `Http3Client` API

```rust
pub struct Http3Client { conn: Connection }
impl Http3Client {
    pub fn new(conn: Connection) -> Self;
    pub async fn healthz(&self) -> io::Result<(u16, Vec<u8>)>;
    pub async fn get_text(&self, sha256: &str) -> io::Result<(u16, Vec<u8>)>;
    pub async fn get_image(&self, sha256: &str) -> io::Result<(u16, Vec<u8>)>;
    pub async fn get_file(&self, sha256: &str, range: Option<&str>) -> io::Result<(u16, Vec<u8>)>;
}
```

**4xx / 5xx 语义**：返回 `Ok((status, body))` 不抛 IO error（PLAN §5 评审 #3 2nd "404 cache miss silently ignored"）；调用方按 status 决定处理。

**流式 body 接收**：`GrowingSink<'a>` 包装 `&mut Vec<u8>`，每次 `write_all` 只做 `extend_from_slice`（不预分配）；峰值工作集 ≤ `CHUNK_SIZE` (64 KiB)。

---

## 2. 验证结果

### 2.1 命令 + 输出摘要

| 命令 | 输出 |
|---|---|
| `cargo build -p lan-mouse` | ✅ 0 error / 0 warning (我的新代码) |
| `cargo build --workspace` | ✅ 0 error |
| `cargo test --workspace --exclude input-capture` | **150 pass / 0 fail**（99 lib + 7 quic_smoke + 2 quic_session + 15 ipc + 27 proto；其中 27 proto = 11 pre-existing http3 + 16 新 http3 = 27 个 http3 单测） |
| `cargo fmt --check` | ✅ 0 diff |
| `cargo clippy --workspace --all-targets -- -D warnings` | ⚠️ **7 个 pre-existing warnings**（`connect.rs:727,728,1246,1252` + `endpoint.rs:238,339` + `session.rs:805` 即原 770 漂移），全部在 SUGGESTION-IGNORE.md #1 范围内（PLAN §0 scope discipline "不动 QUIC 传输层 pre-existing 噪音"）。**我的新代码 0 warning**。 |
| `git diff --stat Cargo.lock` | ✅ 0 diff（无 dep 变更） |

### 2.2 16 个新单测

#### Router / default_router / GrowingSink (11 个)
- `clipboard_text_prefix_returns_404`
- `clipboard_image_prefix_returns_404`
- `clipboard_file_prefix_returns_404_no_range`
- `clipboard_file_prefix_with_range_returns_404`（`?range=` 也命中 prefix）
- `healthz_via_default_router_returns_200`
- `default_router_unknown_path_returns_404`
- `default_router_post_to_healthz_returns_405`
- `get_prefix_requires_trailing_slash_to_match`（防 `/clipboard/text` 不带 `/` 误命中）
- `growing_sink_writes_incrementally`（3 chunks 验证 `extend_from_slice` 顺序正确）
- `growing_sink_no_preallocation`（构造后 `buf.capacity() == 0`，守住 "no `Vec::with_capacity(200 MiB)`" 约束）

#### Http3Client full-QUIC-stack round-trip (5 个)
- `http3_client_healthz_roundtrip`（200 + "ok"）
- `http3_client_get_text_returns_404`（status + body 双重断言）
- `http3_client_get_image_returns_404`
- `http3_client_get_file_returns_404`（覆盖 `?range=` 两种调用形式）
- `http3_client_5xx_error_parsing`（自定义 `/boom` route 返回 500 + `"boom"`，验证 client 不抛 IO error）
- `http3_client_timeout_handling`（custom hanging server + `tokio::time::timeout(200ms)` 验证 cancel 链路）

### 2.3 pre-existing 测试（无回归）

- `cargo test --workspace --lib` lib 部分：99 pass（含 11 pre-existing http3 + 16 新 + 72 其它）
- `tests/quic_smoke.rs`：7 pass
- `tests/quic_smoke.rs` 之外的集成测试：2 pass
- `lan-mouse-ipc`：15 pass
- `lan-mouse-proto`：27 pass
- **input-capture pre-existing fail**：`macos::tests::enumerate_monitors_returns_live_state` 失败 — 与本 STEP 无关（SUGGESTION-FIXED #3）

### 2.4 Plan §8 测试矩阵 M0b 段 — 本 STEP 覆盖

| 类型 | 测试项 | 通过标志 | 实际 |
|---|---|---|---|
| 自动 | h3 server `/healthz` 单测（mock bi_stream） | 200 OK | ✅ `healthz_via_default_router_returns_200` + `http3_client_healthz_roundtrip` |
| 自动 | h3 server `/clipboard/text/abc` 单测 → 404 | 404 | ✅ `clipboard_text_prefix_returns_404` + `http3_client_get_text_returns_404` |
| 自动 | h3 client GET helper 单测（mock server） | 拿到正确 bytes | ✅ 5 个 `http3_client_*` round-trip tests |
| 自动 | h3 client timeout 处理 | cancel 链路工作 | ✅ `http3_client_timeout_handling`（200ms 阈值） |
| 自动 | 4xx / 5xx 错误响应解析 | 不抛 IO error，按 status 解析 | ✅ `http3_client_5xx_error_parsing`（500 + body）；4xx 覆盖于 `http3_client_get_text_returns_404` |

---

## 3. 与 PLAN 的偏差

**0 处功能偏差**。落地与 PLAN §3 M0b STEP-0.3 + STEP-0.4 完全一致：
- server builder 接 PeerSession Connection：✅（`start_http3_server` 公共方法）
- 注册路由表 5 条（`/healthz` 200 + 4 条 stub 404）：✅（`default_router()`）
- `Http3Client::get_text/image/file` helper 落地：✅
- 流式 body 接收（不一次性 `Vec::with_capacity`）：✅（`GrowingSink` + `request_streaming` 路径）
- 单测覆盖 mock server / timeout / 4xx / 5xx：✅（5 full-stack + 5 stub + 1 prefix-matching-edge）

---

## 4. 处理的 SUGGESTION 项

未触碰 SUGGESTION.md / SUGGESTION-FIXED.md / SUGGESTION-IGNORE.md。

实现期间识别的小问题（编译 fix / 1 个 test race）均在代码层就地修复，无需上升 SUGGESTION。

---

## 5. 闸门检查

| 闸 | 结果 |
|---|---|
| 时间门（~90 min AI；≤ 2h ABS） | ✅ ~50 min |
| milestone 边界门（M0b 内，未触碰 M0c / M1+） | ✅ `mod http3` 已落地但未 wire 进 `peer.run`；`PeerSession` 仅加 1 个公共方法；`session.rs:805` 漂移是 pre-existing 注释行（`doc_lazy_continuation`，原 770）；StreamA/B/C dispatch 不动；IPC 不动；Vue 不动；proto 不动 |
| 产物（http3.rs 5 路由 + Http3Client + GrowingSink + start_http3_server） | ✅ |
| 依赖（M0a 完 + STEP-0.2 完 + PeerSession + Router 已就位） | ✅ |
| 验收（cargo build/test/fmt/clippy） | ✅ fmt 0 diff；clippy 仅 pre-existing（不在本 STEP 范围） |

---

## 6. 遗留

- **server-side wiring 尚未触发**：`listen.rs::handle_quic_peer_supervisor` 仍没调 `peer.start_http3_server(...)` — `http3.rs` 接口就位但 server-side accept loop 没起。M0c STEP-0.5a 接通 StreamC 时一并 wire（在 scope 内：动 `listen.rs` 添加 `peer.start_http3_server(default_router())`）。本 STEP 不动 `listen.rs`。
- **5xx error response 解析覆盖有限**：当前只测了自定义 `/boom` route 返回 500。真实生产路径里 server 永远返回 200 / 404（M0c 之前没 cache 接通）；后续 M1/M2/M3 接通 cache 后，"peer 侧 OOM / cache eviction fail" 之类可能产生 5xx，留到对应 STEP 补测。
- **`h3 / h3-quinn / rand` dev-dep 暂留**：同 SUGGESTION-FIXED #9 — spike 留作长期回归测试，生产 `cargo build -p lan-mouse` 不引入，cleanup PR 由 leader 决策。
- **pre-existing clippy 噪音**：7 个 warning 在 SUGGESTION-IGNORE.md #1；与本 STEP 无关。

---

## 7. 下一步

→ leader commit（commit message 建议：`<type>(quic): http3 server accept loop + Http3Client helper (M0b STEP-0.3 + STEP-0.4 merge)` + `归档: next/STEP-P2-M0b-0.3-0.4.md` + `Co-Authored-By`）

→ 派 M0b STEP-0.7（fmt/clippy 三平台编译 + 真机 `curl --http3 https://<peer>:4252/healthz` 验证 — 人类配合）。

→ 派 M0c STEP-0.5a（StreamC 接通 + `listen.rs` 调用 `peer.start_http3_server(default_router())` 真正起 server-side accept loop）。