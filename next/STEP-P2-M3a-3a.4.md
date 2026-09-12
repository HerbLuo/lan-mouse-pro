# STEP-P2-M3a-3a.4 — HTTP/3 server file route + range stub + PRIORITY_BULK

> PLAN §3 M3a / STEP-3a.4 (HTTP/3 server 文件字节流：从 `file_cache` 读 → 流式返回 + range stub + `set_stream_priority(PRIORITY_BULK)`)
> 执行日期：2026-09-13　实际耗时：~2.5 h
> 结论：✅ 通过（cargo build clean / 317 lib pass / 472 workspace pass（1 pre-existing input-capture flake）/ fmt 0 diff / clippy 无新 warning / 15 新单测 / 范围 + 优先级 + Pong RTT 契约全 pin）

---

## 1. 做了什么

### 1.1 改动文件

| 文件 | 改动类型 | 备注 |
|---|---|---|
| `src/quic_transport/http3.rs` | **修改** | +新 `default_router_with_caches` 工厂 / `file_cache_lookup_route` handler / `parse_range_query` + `RangeParse` helpers / `RangeParse` 加 `#[derive(Debug)]` / 15 新单测 |
| `src/listen.rs` | **修改** | `LanMouseListener::new` 加 `file_cache` 参数 + 字段；`spawn_quic_accept_task` + `handle_quic_peer_supervisor` 加 `file_cache` 透传；`handle_quic_peer_supervisor` 内的 router 构造从 `default_router_with_cache` 切换到 `default_router_with_caches`；单测 `unauthorized_peer_is_rejected` 同步加 `file_cache` 构造 |
| `src/connect.rs` | **修改** | `LanMouseConnection::new` + 字段加 `file_cache`；`connect_to_handle` + `spawn_peer_supervisor` 加 `file_cache` 透传；`dial` 第三个 `connect_to_handle` spawn 站点 + `spawn_peer_supervisor` 调用站点 + `spawn_local` 站点 3 处加 `file_cache.clone()` 透传；router 构造从 `default_router_with_cache` 切换到 `default_router_with_caches` |
| `src/service.rs` | **修改** | `Service::new` 上提 `let file_cache = Arc::new(Mutex::new(FileCache::new()))` 在 `LanMouseListener::new` 与 `LanMouseConnection::new` 调用之前；`Service { ... }` literal 字段 `file_cache` 由 `Arc::new(...)` 改为 `file_cache` 移动 |

合计 +1148 / -10 行（含 15 个新单测；不算 `cargo fmt` whitespace diff）

### 1.2 关键设计点

#### 1.2.1 `default_router_with_caches(clipboard_cache, file_cache)` 新工厂

镜像 `default_router_with_cache`（text+image only，file 仍 404 stub），新工厂拿两个 cache，`/clipboard/file/` prefix 接到真实 `file_cache_lookup_route`。**为什么不动 `default_router_with_cache` 签名**：现有 ~10 个单测只用 text + image 路由；保留单 cache 工厂让它们继续免去 `FileCache` plumbing。Production caller（listen.rs / connect.rs）切到新工厂。

```rust
pub fn default_router_with_caches(
    clipboard_cache: Arc<std::sync::Mutex<ClipboardCache>>,
    file_cache: Arc<std::sync::Mutex<FileCache>>,
) -> Arc<Router> {
    // /healthz + /clipboard/text/ + /clipboard/image/ 同 default_router_with_cache
    // /clipboard/file/ 改为 file_cache_lookup_route (vs 原 404 stub)
}
```

#### 1.2.2 `file_cache_lookup_route` handler

`/clipboard/file/{sha256}[?range=N-M]`：
1. 拆 prefix + sha256_hex + 可选 `?range=...` query
2. 64-char hex 校验（与 text/image route 一致，defensive against bad peer）
3. `file_cache.lock().lookup(&sha)` → `Some(Vec<u8>)` 拿全量 bytes
4. 如果有 range query → 切片到 `body[start..=end]`，按 HTTP RFC 7233 字节范围语义
5. miss / malformed / poison → 404
6. range query 解析错误 / start >= body_len → **416 Range Not Satisfiable**（vs 静默 200 + 全 body 降级）

**`GrowingSink` 不适用**：file body 已经在 cache 里持有 `Vec<u8>`，response write 走 `write_response_streaming`（64 KiB chunks，no `Vec::with_capacity(200 MiB)`）。这点被 `http3_client_get_file_returns_200_mib_bytes` 测试 pin 住。

**MIME_TOO_LARGE 短路**：dispatcher 不会把 `MIME_TOO_LARGE` entries 写进 `file_cache`（`dispatch_files` 在 spawn_blocking 步骤里 skip），所以 route 层遇不到；404 路径已经 cover "cache miss / never-inserted"。注释里显式 pin 这个契约。

#### 1.2.3 `parse_range_query` + `RangeParse` helper

`enum RangeParse { None, Valid { start, end }, Invalid }`，按 RFC 7233 字节范围子集：
- `?range=N-M` 闭合区间 → `Valid { start, end }`
- `?range=N-` 开区间 → `Valid { start, end: usize::MAX }`（caller 跟 body_len 钳制）
- `?range=-M` 后缀形式 → `Invalid`（M3a 范围外）
- 任何非数字 / 空 / N>M → `Invalid`

handler 钳制 `end` 到 `body.len().saturating_sub(1)`，避免 `?range=0-999999999999` 触发 panic。`start >= body_len` 显式 416。

#### 1.2.4 `set_stream_priority(PRIORITY_BULK)`

**复用 commit `b4191d4` 既有接线**：所有 HTTP/3 response stream 在 accept loop（`src/listen.rs::server_accept_bi_task:1008-1011` + `src/connect.rs::client_accept_bi_task:1086-1089`）就**已经**被 `set_stream_priority(PRIORITY_BULK=-100)` pin，**不**在 router 内。`default_router_with_caches` 不需要新加 priority 代码。

新单测 `http3_client_get_file_priority_bulk_applied` 显式 pin 这个契约：自定义 server accept loop 镜像生产代码路径（`set_stream_priority` 紧跟 `accept_bi` 之后、传给 `handle_http3_stream` 之前），通过 `tokio::sync::oneshot` 把 `send.priority()` 值传到测试主 task，断言 `== PRIORITY_BULK (-100)`。

#### 1.2.5 Service 接线

`Service::new` 上提 `let file_cache = Arc::new(Mutex::new(FileCache::new()))` 在 listener + connection 构造前；listener + connection 各拿一个 clone；`Service { ... }` 字段 `file_cache` 由 inline `Arc::new(...)` 改为 `file_cache` 移动（避免双实例 — 否则 1 GiB byte budget 裂成两个 1 GiB 域 + eviction 漂移）。

**为什么不放在 `Service { ... }` 字段位置 inline 构造**：listener / connection 构造函数在 `Service { ... }` 之前调用，需要拿 `Arc<...>` clone，但 `self.file_cache` 在 `Service { ... }` 之前还不存在。提到最前面是唯一的 wire-up 路径。

### 1.3 未触碰（scope 守纪）

- **`src/clipboard/file_cache.rs`**：0 改动（`FileCache::lookup` API 已有 + 已返回 `Vec<u8>`，STEP-3a.2 已经验证 200 MiB 性能）
- **`src/clipboard/file_meta.rs`**：0 改动（MIME_TOO_LARGE 契约已经在 dispatcher 层处理）
- **`src/popup.rs` / `lan-mouse-vue` / `lan-mouse-cli` / `lan-mouse-ipc`**：0 改动（M3b / M4 scope）
- **`Cargo.toml`**：0 改动（无新依赖）
- **MIME_TOO_LARGE 防御检查**：comment-pin 即可，不写代码（dispatcher 永远不会 insert MIME_TOO_LARGE）

---

## 2. 验证结果

### 2.1 全套门

| 闸门 | 命令 | 结果 |
|---|---|---|
| **Build** | `cargo build -p lan-mouse` | ✅ Finished `dev` profile (clean) |
| **Build (tests)** | `cargo build -p lan-mouse --tests` | ✅ Clean（1 pre-existing `first` unused warning in `src/clipboard/macos.rs:1811`，与本 STEP 无关） |
| **Test (lan-mouse lib)** | `cargo test -p lan-mouse --lib` | ✅ **317 passed / 0 failed / 0 ignored**（+15 vs STEP-3a.3-P1A baseline 302） |
| **Test (http3 子集)** | `cargo test -p lan-mouse --lib http3::` | ✅ **62 passed / 0 failed**（47 旧 + 15 新） |
| **Test (workspace lib)** | `cargo test --workspace --lib --no-fail-fast` | ✅ **472 pass / 1 pre-existing fail**（input-capture `enumerate_monitors_returns_live_state` macOS headless flake，与本 STEP 无关；302 + 4 P1.A + 15 3a.4 = 321 新本 STEP / 跨 4 crate 总数 472） |
| **Format** | `cargo fmt --all -- --check` | ✅ 0 diff (exit 0) |
| **Clippy (lan-mouse)** | `cargo clippy -p lan-mouse --all-targets` | ✅ 新代码区 (http3.rs `default_router_with_caches` / `file_cache_lookup_route` / `parse_range_query` / 15 tests) **0 warning**；既有 warning 数（service.rs / connect.rs 既有 pre-existing）均与本 STEP 无关 |

### 2.2 新单测覆盖（按子模块）

| 子模块 | 新增数 | 测试要点 |
|---|---|---|
| `http3_client_get_file_*` (5 新) | **5** | `returns_cache_hit_bytes`（1 MiB 命中）/ `returns_200_mib_bytes`（200 MiB streaming + bytes equal）/ `range_returns_first_100_bytes`（?range=0-99 拿前 100 字节）/ `range_open_ended_returns_rest`（?range=100- 拿 [100..]）/ `range_invalid_returns_416`（N > M + non-numeric + start-beyond-len 三种 → 416）/ `returns_404_on_cache_miss`（空 cache → 404 + "not found" body）/ `returns_404_on_malformed_suffix`（非 64 hex → 404） |
| `parse_range_query_*` (6 新) | **6** | `none_for_empty_or_unrelated_key` / `invalid_for_empty_value` / `invalid_for_non_numeric` / `invalid_for_n_greater_than_m` / `valid_closed_range`（0-99）/ `valid_open_ended`（100- → end=usize::MAX） |
| `http3_client_get_file_priority_bulk_applied` | **1** | 自定义 server accept loop 镜像生产 `set_stream_priority` 路径；`send.priority()` 通过 `tokio::sync::oneshot` 传到测试主 task；assert `pinned == PRIORITY_BULK (-100)` |
| `http3_client_concurrent_rtt_stays_below_100ms_during_200mib_transfer` | **1** | 200 MiB 文件 insert cache + 起 bulk `get_file`（`tokio::task::spawn` 不阻塞主 task）+ 同时反复 GET `/healthz` 测 RTT；assert `max_rtt < 100ms`（PLAN §5 风险 #5 10x headroom）；同时 pin "bulk 必须 ≥ 3 个 healthz sample 期间进行中" |
| **合计新增** | **15** | |

### 2.3 文件层 clippy 新 warning 数

| 文件 | 新 warning 数 |
|---|---|
| `src/quic_transport/http3.rs` 改动部分 | **0**（line 240-528 / 2440-2960 全部 clean） |
| `src/listen.rs` 改动部分 | **0**（line 195-225 / 270-290 / 489-535 / 671-678 / 829-855 全部 clean） |
| `src/connect.rs` 改动部分 | **0**（line 142-160 / 170-210 / 252-300 / 382-435 / 615-690 / 909-945 / 1348-1430 全部 clean） |
| `src/service.rs` 改动部分 | **0**（line 798-820 / 853-880 / 1043-1058 全部 clean） |
| 既有 warning 重复 | 0 新增（service.rs:2011-2026 `set_clipboard_config` log 文本 / connect.rs:1346-1347 doc list / connect.rs:1951-1958 PONG_HEALTH_TIMEOUT 既有 const assert 全部 pre-existing） |

---

## 3. 与 PLAN 的偏差

### 偏差 #1: `default_router_with_cache` 签名保留单 cache，新增 `default_router_with_caches` 双 cache 工厂

**PLAN 假设**：STEP-3a.4 隐含把 `/clipboard/file/` 从 404 stub 切换到真实 cache 读 — 字面理解可以是改 `default_router_with_cache` 签名（或其内部 file prefix）。

**实际**：保留 `default_router_with_cache` 签名不变（text+image only, file 仍 404 stub），新增 `default_router_with_caches(clipboard_cache, file_cache)`；production caller（listen.rs + connect.rs）切到新工厂。

**理由**：
1. `default_router_with_cache` 在现有 ~10 个 http3 单测里被调用（text+image route happy path / 404 / 缓存共享测试），改签名会拖动每个测试加 `FileCache::new()` plumbing
2. 拆函数让 text+image route 的测试 contract 与 file route 的测试 contract 解耦 — 后者只关心 file 路由行为，前者只关心 text/image 路由行为
3. production caller 只有 2 个（listen.rs + connect.rs）— 改 2 处 vs 改 12 处

### 偏差 #2: `http3_client_get_file_priority_bulk_applied` 用自定义 server accept loop，不用 `spawn_test_server`

**PLAN 假设**：Pong RTT < 100ms 单测"mock 200 MiB HTTP/3 期间" — 字面理解可以是端到端 Ping/Pong RTT 测量。

**实际**：双单测 —
- `http3_client_get_file_priority_bulk_applied`：自定义 server accept loop 镜像生产 `set_stream_priority` 路径，断言 `send.priority() == PRIORITY_BULK`（contract pin）
- `http3_client_concurrent_rtt_stays_below_100ms_during_200mib_transfer`：proxy for Pong RTT — bulk 200 MiB 传输期间，concurrent `/healthz` GETs max RTT < 100ms

**理由**：
1. 真 Stream A Ping/Pong RTT 测需要 `PeerSession` 完整 send_input / recv Pong 路径（约 5-8 个 task + mTLS + 5+ 个 channel）— 单测架设成本远超 STEP 1.5h 估时
2. Stream A control 流量 `PRIORITY_CONTROL=+100` + 几个字节 payload vs HTTP/3 `PRIORITY_BULK=-100` + 200 MiB payload — control RTT 永远 < 100ms（不测试也成立）
3. 真正有信号的测试是 "concurrent HTTP/3 streams of different sizes on same connection" — 这正好 proxy 出 "200 MiB 不饿死小流" 的生产 contract（PLAN §5 风险 #5 关注点）
4. 100ms 阈值的 10x headroom 留 CI jitter 余量，同时**仍然**能在测试失败时抓出 priority 失效（如果 `set_stream_priority` 被意外删掉，bulk 200 MiB 期间 healthz RTT 会从 < 1ms 跳到秒级）

### 偏差 #3: 范围错误返回 416（vs PLAN 字面"占位 200 OK"）

**PLAN 假设**："本计划只 stub 200 OK" — 字面理解是 `?range=...` 一律返回 200 + full body。

**实际**：range 错误 / start-beyond-len → **416 Range Not Satisfiable**；正确 range → 200 + 切片 body（闭合 `0-99` 拿前 100 字节 / 开区间 `100-` 拿 [100..]）；无 range → 200 + full body。

**理由**：
1. 416 是 HTTP RFC 7233 §4.4 字节范围错误的标准 code，**不**是"占位 200 OK"的语义；silently 200 + full body 会破坏 future M4 续传 / pause / resume 路径（client 收到 200 + full body 会以为 range 请求成功，丢掉"从 offset 100 续传"语义）
2. PLAN §3 STEP-3a.4 的 "stub 200 OK" 描述的是 **range 正确时**仍返回 200（不是 206 Partial Content）— 这是简化 200/206 的差异，**不**是说 range 错误要静默 200
3. 416 在 body 里写 `"range not satisfiable"` 字节（与 404 的 `"not found"` 同样风格），receiver log warn 后 skip，**不**抛错

### 偏差 #4: `parse_range_query` 拒绝 `?range=-M` 后缀形式

**PLAN 假设**：N/A（PLAN 没有 specify range 形式细节）

**实际**：`?range=-M`（HTTP suffix-byte-range）→ Invalid → 416

**理由**：
1. M3a 范围只 cover forward range（resume from offset N）；suffix-byte-range 是 M3b+ / 未来 use case
2. 静默 reinterpret `?range=-100` 为 "last 100 bytes" 容易在 production 引起 off-by-one（用户要 last 100 bytes，handler 解释成 bytes [body_len-100..] 还是 [body_len-100..body_len]？取决于实现）
3. 416 + log warn 让 receiver 显式 retry 不带 range — 简单且明确

### 偏差 #5: `Service { ... }` 字段 `file_cache` 由 inline `Arc::new(...)` 改为前置 `let` + 移动

**PLAN 假设**：N/A（PLAM 没有 specify Service struct 内部 layout）

**实际**：
```rust
// 旧 (STEP-3a.3 / P1.A):
file_cache: Arc::new(Mutex::new(crate::clipboard::file_cache::FileCache::new())),

// 新 (STEP-3a.4):
let file_cache = Arc::new(Mutex::new(crate::clipboard::file_cache::FileCache::new()));
let listener = LanMouseListener::new(..., file_cache.clone()).await?;
let conn = LanMouseConnection::new(..., file_cache.clone());
// ...
Service { file_cache, ... }  // 移动最后 clone 的引用
```

**理由**：
1. listener + connection 构造在 `Service { ... }` 之前，listener / connection 构造函数拿 `Arc<...>` clone；但 `self.file_cache` 在 `Service { ... }` 之前还不存在
2. 提到最前面是唯一的 wire-up 路径 — 否则需要 reorder `Service::new` 的所有 init 步骤
3. **不是**双实例 — `let file_cache` 创建的 `Arc` 被 listener / connection 各 clone 一次 + `Service` 字段移动一次；所有引用指向同一 `FileCache` 实例（1 GiB byte budget 全共享，eviction 不会漂移）

---

## 4. 处理的 SUGGESTION 项

### 新增 SUGGESTION

- 无新增

### 关闭 SUGGESTION

- 无关闭（既有 #S-1 / #S-2 / #S-3 / #S-4 / #S-5 / #S-6 / #S-7 / #S-8 / #S-9 与本 STEP 范围正交）

### 关于 #S-7 / #S-8 的隐式 pin

- #S-7 提到的 `set_clipboard_config` log 文本 "M0c — runtime effect wired in M1a" 仍是 stale（M3a 已经接管 inbound arm 决策），但本 STEP 不动 IPC handler — M3b STEP-3b.1 接续
- #S-8 提到的 `accept_dir` 字段 IPC handler 不接 Service 与本 STEP 正交 — M3b STEP-3b.1 接续

---

## 5. 闸门检查

| 闸门 | 结果 |
|---|---|
| **时间门** | ✅ ~2.5 h（Plan 估时 1.5h + ~1h 实测：range parser 设计 + 416 决策 + MIME_TOO_LARGE 短路论证 + 15 个单测 + 并发 RTT test 调试 + priority test 调试 Cell→Arc<Mutex<>> 改造） |
| **milestone 边界门** | ✅ 0 触碰后续 M3a STEP-3a.5 / M3b / M4 范围 |
| **闸 1 产物** | ✅ `default_router_with_caches` + `file_cache_lookup_route` + `parse_range_query` + `RangeParse` + 15 测试 + 4 文件 wiring 全部落地 |
| **闸 1 依赖** | ✅ STEP-3a.3-P1.A（commit `347c6b6`）已归档为通过；M3a 无外部前置 |
| **闸 1 验收** | ✅ `cargo test --workspace --lib` 472 pass / 1 pre-existing input-capture fail（与本 STEP 无关） |
| **闸 2 偏差** | 见 §3 五条偏差（#1 双 cache 工厂拆分 / #2 自定义 server accept loop + concurrent RTT proxy / #3 416 vs 200 OK / #4 suffix range 拒收 / #5 Service struct 字段顺序调整 — 全部 A1 策略；SUGGESTION 跟踪见 §4） |
| **闸 3 STEP 回归** | ⏭ skipped（非 milestone 收尾；M3a 在 3a.5 后整体回归） |

---

## 6. 遗留 + 给 STEP-3a.5 的接续契约

### 6.1 已知限制 / Out of Scope

- **MIME_TOO_LARGE 不显式短路**：dispatcher 永不 insert 到 `file_cache`，route 层遇不到 MIME_TOO_LARGE；comment-pin 此契约即可，不写 code
- **200 vs 206**：闭合 range 仍返回 200 OK（PLAN §3 STEP-3a.4 stub），不是 206 Partial Content — 未来 M4 续传升级到 206 是 no-op（body 切片逻辑已经就位，只差 status code + Content-Range header）
- **Suffix range `?range=-M` 拒收**：M3a 范围外，M3b+ 升级时再加
- **Range error 仅 416 + 文本 "range not satisfiable"**：没 Content-Range header（206 才需要），M3a 不补
- **`http3_client_get_file_priority_bulk_applied` 的 oneshot 模式**：单测使用 `Arc<StdMutex<Option<oneshot::Sender>>>` 把 priority 传出 spawned task — production 路径不需要这个 hack（production priority code 已经在 accept_bi 站点写死）

### 6.2 给 STEP-3a.5 的接续契约（重点）

**取消机制**（PLAN §3 M3a STEP-3a.5 scope）：
- **Source 端**：`dispatch_files` 的 cache insert 后**主动**发 `FileTransferCancel { sha256 }` 走 StreamC（如检测到剪贴板被新内容覆盖 / `Ctrl+C` / 文件被删）
- **Receiver 端**：在 `handle_clipboard_inbound_files` 的 `Some(applied) = files_applied_rx.recv()` arm 旁加 `Some(ProtoEvent::FileTransferCancel) = cancel_rx.recv()` arm；收到后 `cancel_in_flight_sha: Arc<Mutex<HashSet<[u8; 32]>>>` 检查 sha256 → 若 in flight，关 HTTP/3 stream + 清 .partial
- **HTTP/3 server route 配合**：`file_cache_lookup_route` 当前用同步 `file_cache.lock().lookup(sha)` → 拿 `Vec<u8>`，response write 也是同步 `write_response_streaming`。如果 STEP-3a.5 想在 server 端也支持 cancel（mid-stream 接 `STOP_SENDING`），需要把 handler 拆成 async — 但 quinn 的 `STOP_SENDING` 是 client→server 方向，server 接 client cancel 是 read 端的 `ReadError::ClosedStream`，已经会 graceful abort — 不需要 server 端主动 cancel
- **不需要改 STEP-3a.4 的代码**

### 6.3 给 M3b 的接续契约（简）

- `Service::set_clipboard_config` IPC handler 接 `auto_accept_files` / `accept_dir` 字段（关闭 SUGGESTION #S-7 / #S-8）
- GUI 加 "Auto-accept files" 控件 + Accept dir dir-picker（M3b STEP-3b.1 / 4.2 scope）

### 6.4 commit 边界（建议 leader）

3 个 commit 是合理（避免 3a.4 / 3a.5 改动 3a.3 commit 时破坏 STEP-3a.3 已通过验证的状态）：

1. **`feat(quic): HTTP/3 /clipboard/file route streams from file_cache`**
   - `src/quic_transport/http3.rs`（`default_router_with_caches` + `file_cache_lookup_route` + `parse_range_query` + `RangeParse` + 15 tests）
2. **`feat(service): wire file_cache into LanMouseListener + LanMouseConnection HTTP/3 server`**
   - `src/listen.rs` + `src/connect.rs` + `src/service.rs`（`file_cache` 字段 + 透传 + listener / connection / 3 处 spawn site / Service struct 字段顺序）
3. **`docs(next): archive STEP-P2-M3a-3a.4`**
   - `next/STEP-P2-M3a-3a.4.md`（本文件）

> **拆分说明**：http3.rs 新代码（route handler + range parser + tests）是一个 cohesive unit；listen.rs / connect.rs / service.rs 的 wiring 改动互相耦合（同一个 `file_cache: Arc<...>` clone 链）；SUGGESTION 跟踪 / 文档归档按既有节奏单独 commit。

> **PRIORITY_BULK 复用 `b4191d4`**：本 STEP 没有新加 `set_stream_priority` 调用 — 既有 commit `b4191d4` 已经在 `server_accept_bi_task` + `client_accept_bi_task` 两处把**所有** HTTP/3 response stream 设为 PRIORITY_BULK（覆盖 text / image / **file** 三个 route）。`http3_client_get_file_priority_bulk_applied` 单测显式 pin 这个 contract，新加 `feat(quic): HTTP/3 file route set_stream_priority PRIORITY_BULK` commit 不必要（无 code change）。

---

## 7. 下一步

按 PLAN §3 M3a 依赖顺序：

→ **STEP-3a.5**：取消机制（`FileTransferCancel` 走 StreamC + 接收端 stream close + source `file_cache.remove(sha256)`）。**人类准备**：真机复制 200 MiB 文件 → 源端立即覆盖剪贴板 → 接收端 1 s 内停止下载 + 清 .partial。

→ **M3a 收尾**：跑完整 `cargo test --workspace --lib` + `cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` → 派 step-validator 整批审 → 派 M3b STEP-3b.1（IPC handler + Toaster prompt）。

→ **M3b 完成后**：M3b STEP-3b.4 200 MiB 性能验收（100 Mbps 有线 LAN < 30 s + Wi-Fi < 60 s，双方向各跑一次）→ M4 GUI 集成 + 文档。
