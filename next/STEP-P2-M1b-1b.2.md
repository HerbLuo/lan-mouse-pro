# STEP-P2-M1b-1b.2 — Clipboard inbound pulls metadata via HTTP/3; source evicts prev sha

> PLAN §M1b / STEP-1b.2
> 执行日期：2026-09-09 → 2026-09-10　实际耗时：~90 min（executor 撞 429 由 leader 接手收尾）
> 结论：✅ 通过（功能落地 + 测试覆盖 + clippy 与 baseline 对齐）

---

## 1. 做了什么

### 1.1 source 端 cache 升级

新增 `src/clipboard/cache.rs`（`ClipboardCache` 类型）：
- 包装 `LruCache<[u8;32], Vec<u8>>`，key = sha256（不是 fingerprint — fingerprint 是 loopback 防回环，sha256 是内容指纹）
- 容量 128（> 1 KiB 才进 cache，inline ≤ 1 KiB 的不进）
- TTL 5 min（lru 自带 stale 处理）
- `remove(sha256)` 显式接口：dispatcher 在 push 新内容前**主动**调用，让上一个 fingerprint 不要在 cache 里待到 TTL 自然过期（reviewer #3）
- Mutex-poison recovery：`into_inner()` 而非 propagate panic，保持 dispatcher "always continue" 契约

8 个 cache 单测：
- insert + lookup returns bytes
- lookup miss on empty cache
- capacity overflow evicts oldest
- distinct keys do not clobber each other
- remove() evicts only the target key
- **active eviction concurrent with lookup_old returns miss**（reviewer #3 active eviction 契约的关键 pin）
- reinsert same key does not duplicate the LRU node
- expired entries are evicted on lookup

### 1.2 source 端 dispatcher 主动 cache.remove prev

`src/service.rs` 抽 free function `evict_prev_outbound_clipboard_cache`：
- 由 dispatcher 在每次 push 新 `ClipboardText` 前调用
- 若 `last_outbound_text_sha != None`，从 cache 删掉
- trace log "clipboard cache: evicted prev outbound sha={}"

`register_pending_clipboard_request`（`#[cfg(test)]`）pin last-writer-wins 语义。

### 1.3 receiver 端 inbound Meta 拉取

`src/service.rs` `handle_clipboard_inbound` 扩展 Meta 分支：
- 收到 `ClipboardText` 时检查 `is_inline()`：
  - **Inline**：现有路径 — `apply_inbound_clipboard_text` + loopback LRU push + last_text reset + FrontendEvent push
  - **Meta**：调 `Http3Client::get_text(sha_hex)` → 拿到 `(200, body)` 走 `apply_inbound_clipboard_text`；拿到 `(status, _)` 非 200 → log warn + skip；`Err(_)` → log warn + skip
- 404 是 normal path（active eviction race，PLAN §1 评审 #3 2nd 显式要求静默）

抽 `apply_inbound_clipboard_text` 共享 helper（inline 与 HTTP/3-pulled 走同一份下游行为）。

### 1.4 HTTP/3 server `/clipboard/text/{sha256}` 路由 + client helper

`src/quic_transport/http3.rs`：
- `clipboard_text_route`（sync，std::sync::Mutex — 路由器 closure 是 `Fn(&Request) -> Response` 不能 async）
- 注册到 default_router（prefix `/clipboard/text/`）
- cache hit → 200 + bytes；miss → 404 + 空 body
- 6 个单测：prefix 路由（text/image/file 全部 404 + range variant）+ get_text client helper（cache hit / cache miss）
- `decode_hex_32` 重写避免 `chunks_exact(2)` 触发 clippy `redundant_guard` + `chunks_exact` constant chunk lint

`Http3Client::get_text(hex)` helper：调 GET，返回 `(status, body)`。

### 1.5 跨站 wiring（capture / connect / emulation / listen）

- `src/capture.rs`：ICaptureState 加 `peers` map（addr → PeerSession），dispatcher 通过它 issue HTTP/3 GET
- `src/connect.rs`：PeerSession 暴露 Http3Client 构造入口
- `src/emulation.rs`：ListenTask::ClipboardText Meta 分支转发到 service HTTP/3 拉取
- `src/listen.rs`：server accept_bi 注册 `/clipboard/text/` 前缀路由

---

## 2. 关键设计

### 2.1 reviewer #3 active eviction race 处理

```text
源端 push X（cache: X）       → 接收端未拉 X
源端 push Y（先 cache.remove(X), 再 cache: Y） → 接收端收到 Meta Y 后拉 X → 404 ✓
源端 push Y 之前若不主动 remove X：
   → cache TTL 5 min 内接收端拉 X 拿到旧内容 → 错误
```

PLAN §1 评审 #3 2nd 显式要求"源端 push 前主动 cache.remove prev fingerprint"。`evict_prev_outbound_clipboard_cache` 就是为此落地。

### 2.2 404 不当 error

`Http3Client::get_text` 把 404 surface 成 `Ok((404, Vec::new()))`。receiver 端 match `Ok((status, body))` 走 status 200 分支，其他 status 全部 log warn + skip。这样：
- wire-level race / active eviction 都走同一份 graceful 路径
- 不抛 error / 不 panic
- daemon 不死

### 2.3 路由 sync 不是 async

`Router::handle` 是 `Fn(&Request) -> Response`（sync 闭包）。cache lookup 是 `HashMap::get` + 可能的 `remove`，没有 await 必要。用 `std::sync::Mutex` 而非 `tokio::sync::Mutex`：
- 路由 hot path 不阻塞 runtime
- 不强制 `async fn` 签名

---

## 3. 验证结果

- `cargo build --workspace`：通过（0 error 0 warning — `register_pending_clipboard_request` 加 `#[cfg(test)]` 后）
- `cargo test --workspace`：**307 passed / 0 failed**（M1a 1a.5 baseline 284 → 1b.1 293 → 1b.2 307 = +14 cache + http3 单测）
- `cargo fmt --all -- --check`：0 diff
- `cargo clippy --workspace --all-targets -- -D warnings`：14 errors（baseline 同 = pre-existing；1b.2 引入 0）
- 单测覆盖：cache (8) + http3 prefix (4) + http3 client (2) + protocol stream_c variants (already 1b.1) = 14 new test pass / 0 fail

**关键测试**（pin reviewer #3 契约）：
- `clipboard::cache::tests::active_eviction_concurrent_with_lookup_old_returns_miss` —— 模拟 source push X → remove X → 接收端拉 X 必须 miss
- `quic_transport::http3::tests::clipboard_text_prefix_returns_404_no_range` / `clipboard_text_prefix_with_range_returns_404` —— 路由参数校验
- `quic_transport::http3::tests::http3_client_get_text_returns_404_on_cache_miss` / `http3_client_get_text_returns_cache_hit_bytes` —— client 404 surface

---

## 4. PLAN 偏差

- **0 处 PLAN 偏差**：1b.2 完整落地了 PLAN §3 M1b STEP-1b.2 的全部要素（dispatcher cache.remove prev + inbound Meta pull + HTTP/3 /clipboard/text/{sha256} + 404 静默 + 5 min LRU 兜底）
- **1 处执行偏差**：executor 撞 429 中断后 leader 接手收尾（修了 3 处 1b.2 引入 clippy + 1 处 cfg(test) 警告 + 1 处 match 重组）

---

## 5. 累计耗时

~90 min：
- ~70 min executor 完成大部分实现 + 测试（撞 429 时已基本完工）
- ~20 min leader 收尾（3 处 clippy fix + 1 处 cfg(test) + 1 处 match 重组 + 4 拆 commit + 报告）

---

## 6. 文件改动

| 文件 | 改动类型 | 备注 |
|---|---|---|
| `src/clipboard/cache.rs` | 新建 | ClipboardCache + 8 单测（380 行） |
| `src/clipboard/mod.rs` | 注册 | `pub mod cache;` |
| `src/service.rs` | dispatcher + inbound + helper | +290/-30（含 `evict_prev_outbound_clipboard_cache` free fn + `apply_inbound_clipboard_text` helper + register_pending cfg(test)）|
| `src/quic_transport/http3.rs` | 路由 + client helper + tests | +353（`/clipboard/text/{sha256}` 路由 + `Http3Client::get_text` + decode_hex_32 重写）|
| `src/capture.rs` | wiring | +37（ICaptureState.peers map）|
| `src/connect.rs` | wiring | +10 |
| `src/emulation.rs` | wiring | +15 |
| `src/listen.rs` | wiring | +66/-? |

---

## 7. 下一步

→ `STEP-1b.3`：回环检测加固 + 监控信号（LRU 128 + 60s TTL + `mark_local_write` + `service::clipboard::metrics` 计数 + `RUST_LOG=lan_mouse_service::clipboard=trace` 时 60s 打印命中率）

→ 1b.3 完成后 1b.4 跨平台真机端到端（macOS / Linux / Windows 各跑一遍大文本字节级一致 + 同内容不触发回环 + 不同源端快速切换）
