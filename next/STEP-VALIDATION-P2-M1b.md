# Validation: M1b STEP 1b.1 + 1b.2 + 1b.3 + 1b.4

> 审阅日期：2026-09-10　审阅 STEP 范围：1b.1 / 1b.2 / 1b.3 / 1b.4（M1b 整批）
> 起点 commit：`d0ce5a9`　终点 commit：`HEAD`（`395a05b`）
> 起点 / 终点 diff：`23 files changed, 3891 insertions(+), 156 deletions(-)`
> 待审 commit（9）：
>   - `8daaa1d` feat(clipboard): split text payloads by size — 1b.1
>   - `560d40e` feat(clipboard): dedicated ClipboardCache with active eviction + tests — 1b.2 (cache)
>   - `b65236a` feat(service): dispatcher evicts prev sha + inbound Meta pulls via HTTP/3 — 1b.2 (dispatcher)
>   - `7abb275` feat(quic): /clipboard/text/{sha256} route + Http3Client::get_text helper — 1b.2 (route)
>   - `5d2f82a` feat(service): wire Http3Client hookup across capture / connect / emulation / listen — 1b.2 (wiring)
>   - `1b85d11` feat(service): harden clipboard loopback LRU + add runtime metrics + hit-rate log task — 1b.3
>   - `d13808d` docs(tests): add cross-platform clipboard text manual test template — 1b.4 (template)
>   - `f471475` test(clipboard): add integration test stub for StreamC + HTTP/3 cache-miss — 1b.4 (stub)
>   - `395a05b` docs: archive M1b 1b.4 + record SUGGESTION #S-3 — leader 收尾 commit
>
> 实际耗时：~3h AI（1b.1 55min + 1b.2 90min + 1b.3 50min + 1b.4 25min）
> 结论：✅ **PASS-with-followup**（0 P0 / 0 P1 / 4 P2 / 6 P3）

---

## 1. 偏离 PLAN

### STEP-1b.1
- ✅ 完全符合 PLAN §3 M1b STEP-1b.1：4 档 inline 边界（1024 / 1025 / 100KiB / 1MiB）单测覆盖，StreamC 路由回归覆盖 7 个 var-codec 变体（ClipboardText/Image/Files/FileTransferOffer/Response/Cancel/ClipboardRequest），不破坏既有 `content_inline: Option<Vec<u8>>` wire 契约。
- ⚠️ **执行偏差（已就地处理）**：executor 实现略超出"只登记 metadata 不发 HTTP/3"边界 — 把 `pending_clipboard_requests` 字段引入了 `Service` struct（line 159），后续 1b.2 必须清理。但 1b.2 落地时**未**就地清理，而是保留 `#[allow(dead_code)]` 字段 + `#[cfg(test)]` helper + 在 `SUGGESTION.md` 留 `#S-3` → 累计技术债，详见 §3 P2.1。

### STEP-1b.2
- ✅ 完全符合 PLAN §3 M1b STEP-1b.2：
  - ✅ 源端 push 前主动 `cache.remove(prev_sha)`（`evict_prev_outbound_clipboard_cache` free fn + `Service::evict_prev_outbound_clipboard_cache` 包装，tick + recover_push 两条路径都调用）
  - ✅ 接收端 inbound Meta 拉取（`handle_clipboard_inbound` async + `Http3Client::get_text` + `apply_inbound_clipboard_text` 共享 helper）
  - ✅ HTTP/3 server `/clipboard/text/{sha256}` 路由（`clipboard_text_route` + `default_router_with_cache`，sync `std::sync::Mutex` 满足 `Fn(&Request) -> Response` 签名约束）
  - ✅ 404 静默（receiver 走 `Ok((status, body))` match → 非 200 → log warn + skip）
  - ✅ 5 min LRU 兜底（`CLIPBOARD_CACHE_TTL = Duration::from_secs(5 * 60)`，lazy eviction on lookup）
- ✅ reviewer #3 active eviction 契约 pin 在 `clipboard::cache::tests::active_eviction_concurrent_with_lookup_old_returns_miss`（line 344-370 of `src/clipboard/cache.rs`）+ `quic_transport::http3::tests::http3_client_get_text_returns_404_after_active_eviction`（line 7abb275 +120）。

### STEP-1b.3
- ✅ 完全符合 PLAN §3 M1b STEP-1b.3：
  - ✅ LRU 容量 64 → **128**（`LruFingerprints::DEFAULT_CAPACITY = 128`，test `default_capacity_is_128` pin 129 触发 eviction）
  - ✅ TTL 60 s（`LruFingerprints::DEFAULT_TTL = Duration::from_secs(60)`，test `default_ttl_is_60s` pin vs 0s LRU）
  - ✅ `mark_local_write` API（line 350 of service.rs，`push` 的语义化别名，test `push_and_mark_local_write_share_lru_state` pin 共享底层 deque）
  - ✅ `service::clipboard::metrics { skip_count: AtomicU64, allow_count: AtomicU64, last_skip_ts: AtomicU64 }`（line 384-415 of service.rs，Ordering::Relaxed，5 metrics tests 全绿）
  - ✅ `apply_inbound_clipboard_text` **mark 前置** line 1997 `self.clipboard_lru.mark_local_write(*sha256);` 在 line 2003 `backend.set_text(&text)` **之前**调用 — reviewer #4 3rd race 关闭
  - ✅ `spawn_hit_rate_log_task` 每 60s 打印（0/0 跳过），`RUST_LOG=lan_mouse::service::clipboard=trace` 激活（line 502-535 of service.rs）
  - ⚠️ **执行偏差**：task hint 写 `lan_mouse_service::clipboard`，实际落地 `lan_mouse::service::clipboard`（Rust module path 语法）— 已在 docstring 注明，可接受。

### STEP-1b.4
- ✅ 完全符合 PLAN §3 M1b STEP-1b.4：cross-platform 真机模板 532 行（§1 4 场景 × §2 3 对端 × §3 24-cell 记录表 × §4 troubleshooting × §5 milestone gate × §6 out-of-scope）+ 集成测试 stub 5 个（2 sanity `run` + 3 `#[ignore]`）。
- ⚠️ **执行偏差（已就地处理）**：原本 stub `active_eviction_concurrent_with_lookup_old_returns_miss` 想 `use lan_mouse::clipboard::cache::ClipboardCache;` 在 `tests/` 直接 pin 1b.2 active eviction 契约 → E0603 "private module"（`src/clipboard` 是 `pub(crate)`）。**处理**：body 留空 + 注释指明契约 pin 在 `src/service.rs::register_pending_clipboard_request`（实际应改为 `src/clipboard/cache.rs::active_eviction_concurrent_with_lookup_old_returns_miss`，stub 注释位置错位）— 见 P2.2。

---

## 2. 偏离 REQUIREMENT

- ✅ 未破坏：wire-compat 完整保持。`lan-mouse-proto::ProtoEvent::ClipboardText` struct 字段未改；`from_content` / `is_inline` 是新 method 不影响既有编码路径；var-codec dispatcher 走 `Vec<u8>` + length prefix，不影响旧 daemon 在 stream A 上的键鼠互通。
- ✅ 满足 REQUIREMENT §3.2 文本同步（大文本 ≥ 1 KiB 通过 Meta + HTTP/3 拉取端到端通路就绪）；§3.2 防回环在 1b.3 加固（128 条 LRU + 60s TTL + mark 前置）。
- ⚠️ REQUIREMENT §4.2 验收"复制 1 MiB 文本 → 对端粘贴成功，无丢失、无重复" — 自动测试覆盖到 codec 边界 + 1MiB inline 走 meta + 404 静默 + active eviction + LRU TTL。真机双向（A→B 与 B→A）**由用户在 1b.4 模板驱动下手动验证**，用户 2026-09-09 已验证小文本双向通过；大文本真机双向待 1b.4 模板驱动。

---

## 3. BUG 清单

| 严重度 | 位置 | 现象 | 根因 | 影响 | 建议修复 |
|---|---|---|---|---|---|
| P2.1 | `src/service.rs:159` + `:367` | `pending_clipboard_requests: HashMap<[u8; 32], ()>` 字段与 `register_pending_clipboard_request` helper 是 1b.1 引入的 stop-gap（用于 "metadata-only registered but not yet pulled"），1b.2 取代语义为直接 HTTP/3 GET 后**未就地清理**，保留 `#[allow(dead_code)]` + `#[cfg(test)]` helper | 1b.1 → 1b.2 接力时 executor 撞 429 → leader 接手只补 3 clippy + 1 cfg(test) + 1 match 重组，**未**顺手清掉 dead state | 技术债：增加 struct 字段（不致命但 mental load）；未来若 1b.3/1b.4 改 hashmap 行为（比如改用 BTreeMap）会同时牵动 dead code 路径 | 1b.5 收尾（或 M2a 启动前）删字段 + helper + unit test `metadata_only_text_registers_latest_pending_request_per_hash`；同步更新 `next/SUGGESTION.md` 把 `#S-3` 拆为两个：原 `#S-3`（pub(crate)→pub，clipboard GUI Toaster 需要） + 新增 `pub(crate) cleanup` |
| P2.2 | `tests/clipboard_text_e2e.rs:218-219` | stub `active_eviction_concurrent_with_lookup_old_returns_miss` 的 doc-comment 指明"contract pin at `src/service.rs::register_pending_clipboard_request`"，但**实际契约 pin** 是在 `src/clipboard/cache.rs::tests::active_eviction_concurrent_with_lookup_old_returns_miss`（line 344-370）— 注释位置错位 | stub 写时 executor 没仔细校对 pin 位置；line 231-232 注释里也写了 "commit 7abb275, M1b STEP-1b.2" 但正确位置在 `560d40e` commit 的 `src/clipboard/cache.rs` | 文档误导：未来 reviewer / 维护者按注释找契约 pin 会找不到（service.rs 里的 test pin 的是 pending metadata last-writer-wins 语义，不是 active eviction 语义） | stub 注释改为 "contract pin at `src/clipboard/cache.rs::tests::active_eviction_concurrent_with_lookup_old_returns_miss`（commit 560d40e, M1b STEP-1b.2）" |
| P2.3 | `src/service.rs:1936-1952` + `src/clipboard/cache.rs:182` | receiver `handle_clipboard_inbound` 在 HTTP/3 `Ok((200, body))` 时**不校验 body 是否为空**直接 `apply_inbound_clipboard_text(&ct.sha256, &body, addr)`，会 `set_text("")` 清空本地剪贴板 | 契约层面没 pin "200 + empty body" 应如何处理；当前 router 实现下 200 只来自 `cache.lookup` 返回 `Some(bytes)`，但 helper `Http3Client::get_text` 没约束，**future-proof 风险**：若 router 增加"200 + empty sentinel"或 helper 被复用时，receiver 静默清空本地剪贴板 | 当前代码不可达（cache lookup 不会返回空 body），但契约薄弱，未来重构可能暴露 | 在 `Http3Client::get_text` 里加 `if body.is_empty() && status == 200 { return Err(...) }` 或 `Ok((200, body))` 后 receiver match 加 `200 if body.is_empty() => warn + skip` arm |
| P2.4 | `src/service.rs:1842-1847` + `:1864-1868`（recover_push 路径） | `if push_was_metadata_only` 块只在 push 字节到 cache 后 `self.last_outbound_text_sha = Some(sha);` 才更新 — 但 line 1837 `evict_prev_outbound_clipboard_cache` 已经**在 broadcast 前**调用，且该 helper 用 `last_outbound_text_sha` 决定 evict 什么 — **顺序正确**但有 race：tick 路径（line 1820-1867）与 recover_push 路径（line 1842-1876）代码块几乎逐行重复，未来修一处忘另一处的概率高 | 1b.2 落地时两个 push 路径（tick + recover_push）独立改动，没抽公共 helper；mark 与 update 间隔可能被未来 refactor 拆开 | 当前无 bug（两条路径都对），但 drift risk | 抽 `fn push_clipboard_to_peers(...)` helper 统一：构造 event → evict prev → broadcast → cache insert → update last_outbound_text_sha → frontend notify |
| P3.1 | `src/service.rs:1933` | `let sha_hex = short_hex(&ct.sha256);` — 1b.2 router 接受大写 / 小写 hex（`is_ascii_hexdigit` + `decode_hex_32` 同时接受），但 `short_hex` 输出小写。理论上工作正常，但若 `clipboard_text_route` 的接收方与发送方 hex 大小写约定不一致，路由会 404 | 无明确文档约定 hex case；`Http3Client::get_text` 接收 `&str`，内部 `format!("/clipboard/text/{sha_hex}")` 全小写 | 当前单测覆盖小写；大写 case 没测 | 加单测：`http3_client_get_text_uppercase_hex_works` |
| P3.2 | `src/quic_transport/http3.rs:262-269` | `default_router_with_cache` 的 closure `move \|req: &Request| { clipboard_text_route(req, &cache) }` 把 `cache` 的 `Arc` 捕获进每个 request — 闭包对 `cache` 是 `move` 语义，没问题；但如果未来其他 prefix route 也想 capture state，可能出现 `Arc` 闭包 trait bound 限制 | 当前实现可行 | 当前无 bug | 文档化 router closure 是 `Fn(&Request) -> Response + Send + Sync`，未来加 state-capturing prefix route 时需 `Arc<dyn Fn(...)>` 或 `Box<dyn Fn(...)>` 转换 |
| P3.3 | `src/service.rs:502-535` | `spawn_hit_rate_log_task` 用 `tokio::task::spawn_local`，但 `Service::new` 是 sync fn（**不是 async**）— `spawn_local` 在多线程 runtime 上 panic | 当前 daemon 用 current_thread runtime + LocalSet，所以 OK；但若未来 M4 GUI 集成把 daemon runtime 切到 multi_thread（Vite dev server 集成），`spawn_local` 会 panic | 当前无 bug（runtime 是 current_thread） | 把 task 改 `async fn` + `tokio::spawn` 或在 `Service::new` 注释里明确"requires current_thread runtime" |
| P3.4 | `src/clipboard/cache.rs:101-107` | `VecDeque<[u8; 32]>` 与 `HashMap<[u8; 32], CacheEntry>` 双数据结构保持，注释承认"don't bother walking the deque"；但若 `remove` 高频触发，`lru` deque 会囤积 dead entries 直到 `capacity overflow` 才被 pop — 在 active eviction 频繁 + 容量宽松场景下，deque 长度可超 `capacity` | 当前 capacity 128 + active eviction 频繁时 deque 可短时超过 128 → 后续 insert 触发的 `pop_front` 一次性清掉 | 当前 `lookup` 路径不受影响（只读 HashMap），但 deque 内存可累积 | `remove` 时也同步清理 `lru`（O(n) 一次） |
| P3.5 | `src/clipboard/cache.rs:152-166` | `insert` 后 capacity overflow 驱逐循环用 `while self.entries.len() > self.capacity`，**只**在 insert 时触发；`remove` 不触发（设计正确）— 但如果同时有并发 `insert` + `remove`，`entries.len()` 在 lock 内是安全的 | 当前用 `std::sync::Mutex` + 无并发线程（dispatcher + http3 server 都是 spawn_local），无 race | 当前无 bug | 文档化"single-threaded usage only"，或改用 `tokio::sync::Mutex` 匹配多线程场景 |
| P3.6 | `src/service.rs:153` + `:2367` | `register_pending_clipboard_request` 函数注释说"registered the SHA-256 without issuing a request. Only the latest hash is retained" — 这描述的是 1b.1 行为，但 1b.2 取代语义后该函数变成"internal test helper for a vestigial field"；注释与现实脱节 | 与 P2.1 同根 | 阅读 service.rs 时困惑 | P2.1 修复时同步 |

---

## 4. 跨 STEP 一致性

### 4.1 接口一致性（1b.1 ↔ 1b.2 ↔ 1b.3）

| 接口 | 1b.1 引入 | 1b.2 扩展 | 1b.3 加固 | 一致性 |
|---|---|---|---|---|
| `ClipboardText` struct | 不变（既有字段） | — | — | ✅ wire-compat |
| `ClipboardText::from_content` | ✅ new | — | — | ✅ |
| `ClipboardText::is_inline` | ✅ new | — | — | ✅ |
| `LruFingerprints::push` | 既有 | — | 不变（loopback 路径用） | ✅ |
| `LruFingerprints::mark_local_write` | — | — | ✅ new（`push` 别名） | ✅ |
| `ClipboardCache::insert/lookup/remove` | — | ✅ new | — | ✅ |
| `evict_prev_outbound_clipboard_cache` (free fn) | — | ✅ new | — | ✅ |
| `Service::evict_prev_outbound_clipboard_cache` (wrapper) | — | ✅ new | — | ✅ |
| `Service::handle_clipboard_inbound` | sync | **async** | 不变（仍 async） | ✅ — 1b.2 改 async 已更新 select! arm（line 802）|
| `apply_inbound_clipboard_text` | 不存在 | ✅ new | line 1997 mark 前置 | ✅ |
| `Http3Client::get_text` | — | ✅ new（返回 `Ok((status, body))`） | — | ✅ |
| `default_router_with_cache` | — | ✅ new | — | ✅ |
| `ClipboardMetrics` + `incr_skip` / `incr_allow` / `snapshot` | — | — | ✅ new | ✅ |
| `spawn_hit_rate_log_task` | — | — | ✅ new | ✅ |

### 4.2 公共 API 破坏性改动

- ✅ **0 公共 API 破坏**：
  - `lan-mouse-proto`：ProtoEvent enum 不变，ClipboardText struct 字段不变，只新增 1 个常量（`CLIPBOARD_TEXT_INLINE_LIMIT`）+ 1 个 impl（`from_content` / `is_inline`），向后兼容
  - `lan-mouse-ipc`：未触碰
  - `lan-mouse-vue`：未触碰
  - `lan-mouse`（二进制 CLI）：未触碰
  - `lan-mouse` lib 内部：`LruFingerprints::contains` 从 `&self` → `&mut self`（**breaking** for crate-internal callers），但所有 caller 都是 `&mut Service` 上下文，签名变更无影响

### 4.3 测试覆盖

| STEP | 期望单测 | 实际新增 | 通过率 |
|---|---|---|---|
| 1b.1 | 1 KiB / 100 KiB / 1 MiB 边界 + StreamC 路由 7 变体 | 4 + 1 = **5 new** | 100% |
| 1b.2 | cache (8) + http3 (6) | 8 + 6 = **14 new** | 100% |
| 1b.3 | LRU + metrics + hit-rate + log task | 9 + 5 + 4 + 1 = **19 new** | 100% |
| 1b.4 | 1-2 sanity + 3 stub | 2 sanity + 3 ignored = **5 added** | 100% (run) / N/A (ignored) |
| **合计** | — | **+40 new pass + 3 ignored stub** | 328 total pass / 0 fail |

### 4.4 reviewer #3 / #4 3rd 契约 pin

| 契约 | pin 位置 | 状态 |
|---|---|---|
| reviewer #3 active eviction（源端 push 前 cache.remove prev） | `clipboard::cache::tests::active_eviction_concurrent_with_lookup_old_returns_miss` + `quic_transport::http3::tests::http3_client_get_text_returns_404_after_active_eviction` + `http3_client_get_text_returns_404_on_cache_miss` | ✅ 双层 pin |
| reviewer #4 3rd loopback LRU 128 + 60s TTL | `service::lru_fingerprints_tests::default_capacity_is_128` + `default_ttl_is_60s` + `contains_returns_false_after_ttl_expires` + `ttl_expired_fingerprint_can_be_remarked_and_resyncs` | ✅ |
| reviewer #4 3rd `mark_local_write` 前置 | `service::lru_fingerprints_tests::mark_local_write_then_contains_returns_true` + `push_and_mark_local_write_share_lru_state`（API 形状） + 实际顺序由 `src/service.rs:1997` 在 `set_text` 之前调用（无 unit test pin 顺序本身 — 见 P3.7） | ⚠️ API 形状 pin ✅；运行时顺序仅由 code review 保险 — 见 P3.7 |
| 404 静默 | `quic_transport::http3::tests::http3_client_get_text_returns_404_on_cache_miss`（http3 层）+ `tests/clipboard_text_e2e.rs::http3_cache_miss_returns_404_silently`（stub，未来 un-ignore 时补 receiver 端 match 测试） | ✅ http3 层；receiver match 由 code review 保险 |
| metrics 计数 | `service::clipboard_metrics_tests::*` (5) + `hit_rate_tests::*` (4) | ✅ |

### 4.5 与 M1a hot-fix 的接力

- ✅ M1a hot-fix `#S-2 P2.1 P2.2 backlog`（`e0cae34` `chore(clipboard): drop _force_keep_err_to_string dead code` + `46f5e20` `chore(logs): revert M1a hot-fix diagnostic logs`）已在 `d0ce5a9` 之前合并并审过，本次 M1b 起点不再涉及。
- ✅ M1a follow-up #1（active_addr late-bind + incoming peer broadcast）由 1b.2 在 `peer_connection_for_addr` 路径里同时覆盖 incoming + outgoing 两表（line 1768-1788 of service.rs）。

---

## 5. 总体结论

- **结论**：✅ **PASS-with-followup**
- **理由**：
  1. **0 P0 / 0 P1**：所有 reviewer #3 / #4 3rd 契约都 pin 在单测里且全绿；wire-compat 完全保持；1b.1 → 1b.2 → 1b.3 → 1b.4 跨 STEP 接口一致；M1b 整批未触碰 M2a/M2b/M3a/M3b/M4 范围；M0c 的 `start_http3_server(default_router)` 被 1b.2 替换为 `start_http3_server(default_router_with_cache)` 正确衔接。
  2. **328 cargo test pass / 0 fail / 3 ignored stub**：与 1b.4 报告一致；3 ignored 是设计意图（in-process harness 未就绪）。
  3. **14 clippy error 全部 pre-existing**：M1a baseline 14 个 doc_lazy_continuation / assertions_on_constants / too_many_arguments，1b.1-1b.4 引入 0；fmt 0 diff。
  4. **真机小文本双向已通过**（2026-09-09 hot-fix `1a95486` + `8a44bdf` 之后）；大文本真机双向待 1b.4 模板驱动用户跑 24-cell（不阻塞 validator 接受 — 模板就绪）。

---

## 6. 建议下一步

### 6.1 必须做（M1b 收尾，可由 leader 派 1b.5 或纳入 M2a STEP-2a.0 清理）

1. **P2.1** 删 `src/service.rs:159` `pending_clipboard_requests` 字段 + `:367` struct doc + `:2367` `register_pending_clipboard_request` 函数 + `:2597-2620` test module `metadata_only_text_registers_latest_pending_request_per_hash`。同步 `next/SUGGESTION.md` 把 `#S-3` 拆为：原 `#S-3`（`src/clipboard` `pub(crate)` → `pub` for M4 Toaster） + 新增 `#S-4`（dead state cleanup）。

### 6.2 建议修（押后到 M2a 或 M2a 末尾）

2. **P2.2** 修 `tests/clipboard_text_e2e.rs:218-219` + `:231-232` 的 stub doc-comment，把契约 pin 位置从 `src/service.rs::register_pending_clipboard_request` 改为 `src/clipboard/cache.rs::tests::active_eviction_concurrent_with_lookup_old_returns_miss`。
3. **P2.3** `Http3Client::get_text` 或 receiver `handle_clipboard_inbound` 加 "200 + empty body" 静默处理（防 latent 风险）。
4. **P2.4** 抽 `Service::push_clipboard_to_peers` 公共 helper 统一 tick + recover_push 两条路径，避免 drift。

### 6.3 风格 / 微优化（M2a-M4 任何窗口期顺手）

5. **P3.1-P3.6** 见 §3 表。

### 6.4 M1b 收尾动作（leader 接管）

6. 用户按 `tests/manual/clipboard-text.md` 模板跑 24-cell 真机双向矩阵（macOS ↔ Windows / macOS ↔ Linux / Windows ↔ Linux × S1-S4 场景）。
7. 用户真机全通后 leader 跑全套静态检查 + commit（任何 M1b 后续 hot-fix 或 P2 backlog 清理） + 触发 next validator（M2a 启动前可跳过 — M1b 收尾默认通过）。

### 6.5 M2a 启动条件

8. M1b ✅ + 用户真机大文本双向通过 + P2.1 清理 → 派 STEP-P2-M2a-2a.1（PLAN §3 M2a：`ClipboardBackend::current_image` / `set_image` + mime 检测）。

---

## 7. 验证证据汇总

### 7.1 cargo build --workspace
✅ 通过（0 error 0 warning）
`Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.50s`

### 7.2 cargo test --workspace
✅ **328 passed / 0 failed / 3 ignored**
- `input_capture`: 101 pass / 0 fail
- `lan_mouse` (lib): 161 pass / 0 fail
- `clipboard_text_e2e`: 2 pass / 0 fail / **3 ignored**（设计意图）
- `quic_smoke`: 7 pass / 0 fail
- `quic_session`: 2 pass / 0 fail
- `lan-mouse-ipc`: 26 pass / 0 fail
- `lan-mouse-proto`: 29 pass / 0 fail
- doc-tests: 7 个 target × 0 test（设计意图 — clippy-clean，code 注释已覆盖）

### 7.3 cargo fmt --all -- --check
✅ 0 diff（已自动修正 5 处 line wrapping — 1b.3 报告记载）

### 7.4 cargo clippy --workspace --all-targets -- -D warnings
**14 errors（全部 pre-existing，1b.1-1b.4 引入 0）**：
- `src/connect.rs:1146/1147` doc_lazy_continuation
- `src/connect.rs:1702/1708` assertions_on_constants
- `src/quic_transport/endpoint.rs:238` doc_lazy_continuation
- `src/quic_transport/endpoint.rs:339` too_many_arguments
- `src/quic_transport/session.rs:931` doc_lazy_continuation
- `src/service.rs:109-113` doc_lazy_continuation（5 个 consecutive bullet，与 M1a baseline 位置一致）

### 7.5 关键契约单测验证

```
✓ clipboard::cache::tests::active_eviction_concurrent_with_lookup_old_returns_miss
✓ quic_transport::http3::tests::http3_client_get_text_returns_cache_hit_bytes
✓ quic_transport::http3::tests::http3_client_get_text_returns_404_on_cache_miss
✓ quic_transport::http3::tests::http3_client_get_text_returns_404_on_malformed_suffix
✓ quic_transport::http3::tests::http3_client_get_text_returns_404_after_active_eviction
✓ service::lru_fingerprints_tests::default_capacity_is_128
✓ service::lru_fingerprints_tests::default_ttl_is_60s
✓ service::clipboard_metrics_tests::* (5 tests)
✓ service::hit_rate_tests::* (4 tests)
✓ service::hit_rate_log_task_tests::spawn_hit_rate_log_task_returns_live_handle
```

### 7.6 wire-compat

- ✅ ProtoEvent enum 不变
- ✅ ClipboardText struct 字段不变（`fingerprint` / `sha256` / `size` / `content_inline: Option<Vec<u8>>`）
- ✅ 仅新增 `CLIPBOARD_TEXT_INLINE_LIMIT = 1024` 常量 + `from_content` / `is_inline` 方法（向后兼容扩展）
- ✅ Var-codec dispatcher 走既有 `Vec<u8>` + length prefix 路径
- ✅ 旧 daemon 在 stream A 上的键鼠互通不变；剪贴板 / 文件功能单向有效（旧 daemon 不发 StreamC，新 daemon 不会推错地方）

### 7.7 真机验证

- ✅ 用户 2026-09-09 已验证 **小文本双向**（A→B + B→A）通过（M1a 5 STEP + 10 post-M1a hot-fix 后）
- ⏳ 大文本双向待用户按 `tests/manual/clipboard-text.md` 24-cell 模板跑（不阻塞 M1b validator 接受）
