# STEP-P2-M1b-1b.3 — Loopback detection hardening + monitoring signals

> PLAN §M1b / STEP-1b.3
> 执行日期：2026-09-10　实际耗时：~50 min
> 结论：✅ 通过（loopback LRU 128/60s TTL + mark_local_write + ClipboardMetrics + hit-rate log task + 19 new tests / 0 fail）

---

## 1. 做了什么

### 1.1 `LruFingerprints` 升级 (reviewer #4 3rd)

| 维度 | M1a (前) | M1b.3 (后) |
|---|---|---|
| 容量 | 64 | **128** |
| TTL | 无穷 | **60 s** |
| 数据结构 | `VecDeque<[u8; 32]>` | `VecDeque<(Instant, [u8; 32])>` |
| `contains` 签名 | `&self` | `&mut self`（lazy TTL eviction）|
| 新增 API | — | `mark_local_write(fp)`（语义化别名）|

TTL 60 s 在 `contains` 调用时**懒清理**：从队首向前驱逐过期条目（`now.duration_since(ts) >= ttl`），然后再走 `iter().any(...)` 线性查找。O(128) 字节比较保持原 M1a 的同等开销。

**新增常量** `LruFingerprints::DEFAULT_CAPACITY = 128` / `DEFAULT_TTL = 60s`，用 `Self::new()` 直接走默认值；测试用 `with_capacity_and_ttl(...)` 注入小 TTL（10 ms + 20 ms sleep）。

### 1.2 `mark_local_write` 在 inbound apply 前置

`Service::apply_inbound_clipboard_text` 改动：

```rust
// M1a 顺序 (push 在 set_text 之后):
backend.set_text(&text)?;
self.clipboard_lru.push(*sha256);  // ← 太晚

// M1b.3 顺序 (mark 在 set_text 之前):
self.clipboard_lru.mark_local_write(*sha256);  // ← 提前
backend.set_text(&text)?;
```

**为什么前置**：如果 platform clipboard backend（macOS NSPasteboard / Linux xclip）在 `set_text` 期间触发 changeCount 回调（dispatcher 500 ms tick 也可能撞上），tick 的 `contains` 会先看到 LRU hit → skip re-broadcast。后置顺序则会暴露一个 "set_text 完 → push 进 LRU 前" 的窗口，**这个窗口内 tick 会把我们刚 apply 的内容再次广播**，破坏回环防御。

`set_text` 失败时仍保留 LRU mark（"intended to write"）；代价是下一次同 fingerprint 的 inbound 会无害 skip。

### 1.3 `service::clipboard::metrics` (reviewer #4 3rd)

新增 top-level `pub struct ClipboardMetrics` 在 `src/service.rs`：

```rust
pub struct ClipboardMetrics {
    skip_count: AtomicU64,
    allow_count: AtomicU64,
    last_skip_ts: AtomicU64,
}
```

API:
- `incr_skip(unix_now_ms: u64)` — skip +1 + stamp `last_skip_ts`
- `incr_allow()` — allow +1（**不动** `last_skip_ts`）
- `snapshot() -> ClipboardMetricsSnapshot { skip, allow, last_skip_ts }`

helper `ClipboardMetricsSnapshot::hit_rate() -> Option<f64>`：
- 0 / 0 → `None`（hit-rate log task 用此跳过噪声）
- skip / (skip + allow) → `f64` (e.g. 3 / 45 = 0.0666…)

**Ordering::Relaxed**：三个计数器是独立 monoid，snapshot 不要求跨计数器一致性（hit-rate log 是 debug 信号，不需要 snapshot 严格同步）。`Mutex` 会成为 500 ms tick 的负担，原子化消除锁。

### 1.4 dispatcher 接线

| 位置 | 改动 |
|---|---|
| `Service::new` (line 691) | 初始化 `metrics: Arc::new(ClipboardMetrics::new())` + `spawn_hit_rate_log_task(metrics.clone())` |
| `Service { metrics: ... }` (line 244) | 新增字段 `metrics: Arc<ClipboardMetrics>` |
| `handle_clipboard_inbound` skip 路径 (line 1908) | `self.metrics.incr_skip(unix_now_ms())` |
| `apply_inbound_clipboard_text` apply 路径 (line 1989) | `self.metrics.incr_allow()`（set_text 成功**之后**才计数，失败不膨胀指标）|

### 1.5 hit-rate log task

`pub fn spawn_hit_rate_log_task(metrics: Arc<ClipboardMetrics>) -> JoinHandle<()>`：

- `tokio::task::spawn_local`（daemon 用 `current_thread` runtime + `LocalSet`，`tokio::spawn` 走 multi-thread 与本任务不兼容）
- `tokio::time::interval(Duration::from_secs(60))`，**跳过首次立即 tick**（避免 t=0 时就打印）
- 每次 tick：
  - `let snap = metrics.snapshot();`
  - `if let Some(rate) = snap.hit_rate() { log::trace!(target: "lan_mouse::service::clipboard", "..."); }`
  - 0 / 0 时静默（避免 fresh daemon 的噪声）
- log format: `clipboard hit rate: skip={} allow={} rate={:.1}% last_skip_ts={}`

**RUST_LOG 用法**：
```text
RUST_LOG=lan_mouse::service::clipboard=trace
```

不修改 env_logger filter——`log::trace!` 本身就受 RUST_LOG filter 控制。

**生命周期**：JoinHandle 不被持有；task 跟随 daemon runtime 终结（与 `capture_task` / `emulation_task` 同模式）。

### 1.6 测试覆盖（19 new tests, 0 fail）

四个 `#[cfg(test)] mod`：

| 模块 | 测试数 | 覆盖 |
|---|---|---|
| `lru_fingerprints_tests` | 9 | new() 空 / mark + contains round-trip / push 与 mark_local_write 共享状态 / TTL 过期后 contains 返回 false / TTL 过期后可 re-mark / capacity overflow evict oldest / **DEFAULT_CAPACITY = 128** pin / **DEFAULT_TTL = 60s** pin / distinct keys 不串 |
| `clipboard_metrics_tests` | 5 | 默认全 0 / incr_skip 同时增 skip_count + 更新 last_skip_ts / incr_allow 只动 allow_count（不污染 skip + last_skip_ts）/ 混合 skip + allow 独立累加 / snapshot 是 Copy（前后两次独立）|
| `hit_rate_tests` | 4 | 0/0 → None / 3/45 → 6.67% / 7/0 → 100% / 0/7 → 0% |
| `hit_rate_log_task_tests` | 1 | spawn 返回 live JoinHandle + abort 后 finished；包在 `LocalSet` 里（`spawn_local` panic 防御）|

**关键 pin**（reviewer #4 3rd）:
- `default_capacity_is_128` — 显式插入 128 条 distinct fingerprint，验证 129 条触发 eviction
- `default_ttl_is_60s` — 通过 `with_capacity_and_ttl(0s)` vs `new()`（60s）的对比，验证 default TTL 不为 0
- `contains_returns_false_after_ttl_expires` — 10ms TTL + 20ms sleep，lazy eviction 驱逐过期项
- `ttl_expired_fingerprint_can_be_remarked_and_resyncs` — 60s 后再同步（不在本 STEP 测；语义由 TTL 后 contains 返回 false + 重新 mark 推入）

---

## 2. 验证结果

- `cargo build --workspace`：✅ 通过
- `cargo test --workspace`：**326 passed / 0 failed**（M1b.2 baseline 307 → 1b.3 326 = **+19 new tests**）
- `cargo fmt --all -- --check`：✅ 0 diff（fmt 已自动修正 5 处 line wrapping）
- `cargo clippy --workspace --all-targets -- -D warnings`：14 errors（baseline 14 = pre-existing；**1b.3 引入 0**）

**clippy baseline（pre-existing，1b.3 未触及）**：
- `src/connect.rs`:1146/1147（doc_lazy_continuation）/ 1702/1708（assertions_on_constants）
- `src/quic_transport/endpoint.rs`:238（doc_lazy_continuation）/ 339（too_many_arguments）
- `src/quic_transport/session.rs`:931（doc_lazy_continuation）
- `src/service.rs`:109-113（incoming_clipboard field doc 的 doc_lazy_continuation，pre-1b.3 代码）

**未触发任何 1b.3 相关 clippy warning**：LRU / metrics / hit-rate 全部 clippy 干净。

---

## 3. 与 PLAN 的偏差

- **0 处 PLAN 偏差**：1b.3 完整落地了 PLAN §3 M1b STEP-1b.3 的全部要素：
  - ✅ `service::clipboard_outbound` 写本地剪贴板前 `mark_local_write`（即 `apply_inbound_clipboard_text` 前置 LRU mark）
  - ✅ inbound 收到对端 → 查 LRU → 命中 skip + 计数
  - ✅ LRU 容量 128、TTL 60 s
  - ✅ `service::clipboard::metrics { skip_count, allow_count, last_skip_ts }`（AtomicU64）
  - ✅ 每次 skip / allow 计数 +1
  - ✅ `RUST_LOG=lan_mouse::service::clipboard=trace` 时 60 s 打印命中率（0/0 跳过）

- **1 处执行偏差**：task hint 写的是 `lan_mouse_service::clipboard`（无 module separator），实际用 `lan_mouse::service::clipboard`（Rust 标准 module path syntax）。已在 docstring 注明。

- **1 处 API 重命名**：task 说"`mark_local_write(fingerprint)` API"，同时现有 LRU 已有 `push`。我**保留两者**（`mark_local_write` 是 `push` 的语义化别名），理由：
  - outbound tick 用 `push`（"我观察到本地有这 fingerprint"）
  - inbound apply 用 `mark_local_write`（"我刚把 fingerprint 写到本地"）
  - 两者落到同一 `VecDeque`（test `push_and_mark_local_write_share_lru_state` pin）

---

## 4. 处理的 SUGGESTION 项

无新增。本 STEP 不修改 dispatcher 之外的代码路径；现有 SUGGESTION #S-1（macOS backend）/#S-2（Windows + Linux 跨平台）未触及。

---

## 5. 闸门检查

- **闸 1（执行前）**：产物 ✅（LRU + metrics + log task + dispatcher wire）; 依赖 ✅（1b.2 clipboard_cache 已落）; 验收 ✅（cargo build/test 配置就绪）; milestone 边界 ✅（仅 M1b 内）; 时间门 ✅（<1h）
- **闸 2（执行中）**：build 一次失败（test 0ms TTL 太紧 → 改 10ms TTL + 20ms sleep）; spawn_local 测试一次失败（未包 LocalSet → 加 LocalSet::new().run_until）; 均就地修通
- **闸 3（milestone 收尾，本次非收尾）**：跳过；1b.3 是 M1b 内部 step，M1b 收尾跑全套（待 1b.4 完成后）

---

## 6. 遗留 / 下一步

- **下一步 STEP-1b.4**（PLAN §3 M1b）：跨平台真机端到端 — macOS ↔ Windows / macOS ↔ Linux / Windows ↔ Linux 三组各跑一遍（小文本 / 大文本 / 同内容重复复制 / 不同源端快速切换）。**人类配合**。完成后 M1b milestone 收尾：
  - `cargo build --workspace`
  - `cargo test --workspace`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `cargo fmt --check`
  - commit + push + 更新 `next/.LEADER-STATE.md`

- **M4 STEP-4.4**（PLAN §3 M4）将进一步把 `service::clipboard::metrics` 暴露到 GUI（`state.lastSkipTs` + "过去 1 小时回环跳过 N 次"卡片），不在本 STEP 范围。

---

## 7. 文件改动

| 文件 | 改动类型 | 备注 |
|---|---|---|
| `src/service.rs` | 修改 + 新增 | LRU 升级（+90/-30）+ ClipboardMetrics 新增（+135）+ dispatcher wire（+15）+ 4 个新测试模块（+320）|

**未触碰**：
- `src/clipboard/mod.rs`（metrics 不属于 platform backend 模块，避免污染 trait surface）
- `src/clipboard/cache.rs`（ClipboardCache 是 content cache，与 loopback LRU 是不同抽象）
- `src/quic_transport/*`（1b.2 已落，本 STEP 不动 wire）
- `lan-mouse-ipc/*`（GUI 集成推迟到 M4）
- dispatcher 内 inline / meta 分流逻辑（1b.1 范围，**本 STEP 不动**）

---

## 8. commit 拆分建议（leader 决策）

按 PLAN §0 commit 卫生分拆：

```
1. feat(service): harden loopback LRU to 128 entries + 60s TTL
   - src/service.rs: LruFingerprints capacity 64→128, add 60s TTL with lazy eviction, mark_local_write API

2. feat(service): add clipboard runtime metrics (skip / allow counters + last_skip_ts)
   - src/service.rs: ClipboardMetrics struct + snapshot + hit_rate helper + 5 metrics tests + 4 hit_rate tests

3. feat(service): wire metrics into dispatcher (skip / apply paths) + mark LRU before set_text
   - src/service.rs: handle_clipboard_inbound skip path → incr_skip; apply_inbound_clipboard_text → mark_local_write前置 + incr_allow

4. feat(service): spawn 60s hit-rate log task (RUST_LOG=lan_mouse::service::clipboard=trace)
   - src/service.rs: spawn_hit_rate_log_task + LocalSet test

5. docs: archive STEP-P2-M1b-1b.3 (this file)
   - next/STEP-P2-M1b-1b.3.md
```

---

## 9. 累计耗时

~50 min（估算 60-75 min 之内）：
- ~15 min 设计 + 实现 LRU 升级 + mark_local_write
- ~10 min 实现 ClipboardMetrics + spawn task
- ~10 min dispatcher wire（mark 前置 / incr_skip / incr_allow）
- ~10 min 写 19 个测试 + 调试 2 处初版 bug（0ms TTL + spawn_local 不在 LocalSet）
- ~5 min 验证 build/test/clippy/fmt + 报告
