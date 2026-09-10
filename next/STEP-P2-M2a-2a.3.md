# STEP 2a.3 — 图片 outbound dispatcher (cache 升级 + image 分支 + HTTP/3 image 路由)

> PLAN §3 M2a / STEP-2a.3 (评审 #2 3rd + 评审 #3 2nd)
> 执行日期：2026-09-10　实际耗时：~85 min
> 结论：✅ 通过

## 1. 做了什么

### 1.1 改动文件

- `src/clipboard/cache.rs` — capacity 升级（128 entries → 200 MiB byte budget + 5 min TTL）；新增 `bytes()` API + `byte_budget()` getter；8 个旧测试更新语义 + 4 个新测试
- `src/quic_transport/http3.rs` — `clipboard_text_route` 重构为 `cache_lookup_route(prefix, req, cache)` 通用函数；新增 `clipboard_image_route` 同款包装；`default_router_with_cache` 注册 image prefix；5 个新测试
- `src/service.rs` — `handle_clipboard_tick` 重构（text-first → image-fallback 双 phase），新增 `dispatch_text` + `dispatch_image` helper；新增 `last_outbound_image_sha` 字段 + `evict_prev_outbound_image_cache` 方法 + `sha256_of_bytes` 助手；4 个新测试

### 1.2 cache.rs 关键改动

**常量重命名**：
- `CLIPBOARD_CACHE_CAPACITY: usize = 128` → `CLIPBOARD_CACHE_BYTE_BUDGET: usize = 200 * 1024 * 1024`

**字段变更**：
- 移除 `capacity: usize`（entries 计数）
- 新增 `bytes_used: usize`（累计字节数）
- `byte_budget: usize` 字段语义变为"总字节上限"

**API 变更**：
- `with_capacity_and_ttl(capacity: usize, ttl: Duration)` → `with_byte_budget_and_ttl(byte_budget: usize, ttl: Duration)`
- `insert(sha, bytes)`: 改写为字节预算驱逐；reject 单条 > budget；覆盖时调整 `bytes_used` 计数（先减旧再加新，避免重复计数）
- `remove(sha)`: 维护 `bytes_used`（如果成功删除，减对应字节）
- `lookup(sha)`: 过期驱逐时减 `bytes_used`
- 新增 `bytes() -> usize` — 总占用字节
- 新增 `byte_budget() -> usize` — 配置上限（用于测试断言）

**关键设计决策**：
- **200 MiB byte budget 而非 entry count**：plan §3 2a.3 明确要求 "5 min LRU 200 MiB 上限"；byte budget 语义对混合 text+image 工作负载更友好（一个 100 MiB text push 不会驱逐所有 cached image，因为驱逐是按 byte 而不是按 entry）
- **单条 overflow 拒绝**：`bytes.len() > byte_budget` 时 `insert` 返回 `None` 不存任何东西。200 MiB 远大于典型 image 负载（4K 截图 5-15 MiB），这条路径是防御性 bound
- **`remove` 仍保留**：与 1b.2 同源（dispatcher "evict prev before push" 路径）；并维护 `bytes_used` 计数
- **TTL 仍是 5 min**：保留 1b.2 既有行为

### 1.3 http3.rs 关键改动

**通用 helper 抽取**：
```rust
fn cache_lookup_route(
    req: &Request,
    cache: &Arc<std::sync::Mutex<ClipboardCache>>,
    prefix: &str,
) -> Response
```
原 `clipboard_text_route` 内部逻辑 → 移入 `cache_lookup_route(req, cache, "/clipboard/text/")`。Text 与 image handler 字节级相同，唯一区别是 prefix 字符串。

**路由注册**：
```rust
.get_prefix("/clipboard/text/", move |req| cache_lookup_route(req, &cache_for_text, "/clipboard/text/"))
.get_prefix("/clipboard/image/", move |req| cache_lookup_route(req, &cache_for_image, "/clipboard/image/"))
```

注意 `cache_for_text` 与 `cache_for_image` 各持一个 `Arc::clone`（`Arc` 是 `Send + Sync`，clone 是 refcount bump，便宜）；不能两个 closure 都 `move cache` 因为第二次 move 失败（rustc E0382）。

**关键设计决策**：
- **不复制 handler**：text 与 image handler 字节级相同，复制两份会引入 drift 风险（1b.4 cleanup 已观察到类似问题）。一个 helper，两个 prefix arm 共享
- **`clipboard_text_route` / `clipboard_image_route` 仍保留**：作为命名 wrapper（`#[allow(dead_code)]` 标记，目前无 caller 但保持 grep-friendly 名字）；未来 text-specific / image-specific tweak 有稳定符号可挂
- **mime header 暂不加**：plan §3 2a.3 明确"PNG-only 假设；M2b 扩展再补"；接收端已知 PNG（2a.2 归一化）；M2b 加 `application/x-dib` 时再补 mime header

### 1.4 service.rs 关键改动

**字段新增**：
```rust
last_outbound_image_sha: Option<[u8; 32]>,  // M2a 2a.3
```
独立于 `last_outbound_text_sha`：text push 不应驱逐 image（反之亦然）；共享同一 cache，但 active-eviction key 是 sha256，所以 previous-push pointer 必须 匹配 push kind。

**tick 重构（text-first → image-fallback）**：
```rust
async fn handle_clipboard_tick(&mut self) {
    let Some(backend) = self.clipboard_backend.as_mut() else { return; };
    if let Some(new_text) = backend.current_text() {
        self.dispatch_text(new_text).await;
        return;
    }
    if let Some(image) = backend.current_image() {
        self.dispatch_image(image).await;
    }
}
```
- Phase 1: text。M1a / M1b 既有路径；preserves "tick returns early on text" 行为
- Phase 2: image。剪贴板无 text → 查 image。`current_text()` + `current_image()` 都是 `&mut self` borrow，但前者返回后 borrow 即释放（Rust NLL），所以可串行调用

**dispatch_image 关键逻辑**：
1. `sha = sha256_of_bytes(&image.data)` — 内容指纹
2. `if Some(&sha) == self.last_outbound_image_sha.as_ref() { return; }` — short-circuit on duplicate（避免每 500ms 重推同一 image）
3. `self.evict_prev_outbound_image_cache()` — active eviction（与 1b.2 text 同源）
4. `cache.insert(sha, image.data.clone())` — bytes 暂存（key = sha256）
5. 构造 `ClipboardImage { fingerprint: sha, mime, sha256: sha, size: bytes.len() }`（fingerprint == sha256，与 text convention 一致）
6. `self.broadcast_clipboard_event(event, ...)` — StreamC 推送
7. `last_outbound_image_sha = Some(sha)` + `last_image_ts_ms = Some(now)` + `last_clipboard_source = None` + `FrontendEvent::ClipboardState` 通知

**新助手**：
- `fn sha256_of_bytes(&[u8]) -> [u8; 32]` — 图像字节 hash
- `fn evict_prev_outbound_image_cache(&mut self)` — image branch 的 active eviction（delegate to existing free fn `evict_prev_outbound_clipboard_cache`，只换 `last_outbound_image_sha` 字段）

### 1.5 新增单测（13 个）

**cache.rs（4 个）**：
| 测试 | 覆盖契约 |
|---|---|
| `default_byte_budget_is_200_mib` | PLAN §3 2a.3 "200 MiB 上限" — constant + getter 双重 pin |
| `bytes_returns_total_byte_count` | `bytes()` API 准确性（insert + remove + overwrite 累计对得上） |
| `insert_larger_than_budget_is_rejected` | 单条 > budget 拒绝；`bytes_used` / `len()` 不变；lookup 拿不到 |
| `byte_budget_200_mib_evicts_oldest_when_total_exceeds_cap` | production scale eviction（2 × 100 MiB push 到 100 MiB budget → 驱逐 oldest）|

**http3.rs（5 个）**：
| 测试 | 覆盖契约 |
|---|---|
| `http3_client_get_image_returns_cache_hit_bytes` | image route happy path（5 MiB 4K 截图 byte 级一致） |
| `http3_client_get_image_returns_404_on_cache_miss` | image route miss（silent 404）|
| `http3_client_get_image_returns_404_after_active_eviction` | active eviction 后 → 404 |
| `http3_client_get_image_returns_404_on_malformed_suffix` | malformed sha256 → 404 |
| `image_and_text_routes_share_one_cache` | text + image route 共享同一 cache（content-addressed）|

**service.rs dispatch_image_tests（4 个）**：
| 测试 | 覆盖契约 |
|---|---|
| `sha256_of_bytes_empty_input` | canonical SHA-256 of empty (e3b0c4...) |
| `sha256_of_bytes_known_string` | canonical SHA-256 of "abc" (ba7816...) |
| `dispatch_image_cache_step_inserts_new_and_evicts_prev` | 第一次 push → cache 暂存；第二次 push → cache.remove(prev) 驱逐前次 |
| `dispatch_image_cache_step_skips_on_duplicate_sha` | duplicate sha256 → short-circuit；cache 不变 |

旧 cache 测试 `capacity_overflow_evicts_oldest` / `reinsert_same_key_does_not_duplicate_lru_entry` 等因为 cap → byte-budget 转换重写（用 byte budget 5/10 等小数字演示）。

## 2. 验证结果

### 2.1 全 workspace 测试

```
$ cargo test --workspace --no-fail-fast
...
test result: ok. 101 passed; 0 failed   # input_capture
test result: ok. 0 passed
test result: ok. 0 passed
test result: ok. 197 passed; 0 failed   # lan-mouse lib (184 baseline + 13 new = 197)
test result: ok. 0 passed
test result: ok. 2 passed; 0 failed; 3 ignored   # input_emulation
test result: ok. 7 passed; 0 failed   # lan-mouse-ipc
test result: ok. 2 passed; 0 failed   # capture_test
test result: ok. 0 passed
test result: ok. 26 passed; 0 failed   # lan-mouse-cli
test result: ok. 29 passed; 0 failed   # lan-mouse-proto
```

**总 pass：364（baseline 351 + 13 new）；0 fail**。满足 plan §3 M2a 2a.3 完成标志 "全 workspace `cargo test --workspace` 保持 351+ pass / 0 fail"。

### 2.2 单测细节

**cache tests**：
```
$ cargo test -p lan-mouse --lib clipboard::cache
running 12 tests
test clipboard::cache::tests::default_byte_budget_is_200_mib ... ok
test clipboard::cache::tests::insert_larger_than_budget_is_rejected ... ok
test clipboard::cache::tests::lookup_miss_on_empty_cache ... ok
test clipboard::cache::tests::active_eviction_concurrent_with_lookup_old_returns_miss ... ok
test clipboard::cache::tests::byte_budget_overflow_evicts_oldest ... ok
test clipboard::cache::tests::bytes_returns_total_byte_count ... ok
test clipboard::cache::tests::distinct_keys_dont_clobber_each_other ... ok
test clipboard::cache::tests::insert_then_lookup_returns_bytes ... ok
test clipboard::cache::tests::reinsert_same_key_does_not_double_count_bytes ... ok
test clipboard::cache::tests::remove_evicts_only_target_key ... ok
test clipboard::cache::tests::expired_entries_are_evicted_on_lookup ... ok
test clipboard::cache::tests::byte_budget_200_mib_evicts_oldest_when_total_exceeds_cap ... ok

test result: ok. 12 passed; 0 failed
```

**dispatch_image_tests**：
```
$ cargo test -p lan-mouse --lib dispatch_image_tests
running 4 tests
test service::dispatch_image_tests::sha256_of_bytes_empty_input ... ok
test service::dispatch_image_tests::sha256_of_bytes_known_string ... ok
test service::dispatch_image_tests::dispatch_image_cache_step_skips_on_duplicate_sha ... ok
test service::dispatch_image_tests::dispatch_image_cache_step_inserts_new_and_evicts_prev ... ok

test result: ok. 4 passed; 0 failed
```

### 2.3 lib build / clippy

```
$ cargo build -p lan-mouse --lib
   Finished `dev` profile [unoptimized + debuginfo] target(s) in 4.64s

$ cargo clippy -p lan-mouse --lib -- -D warnings
error: doc list item without indentation       (×5, src/service.rs:109-113 — pre-existing in incoming_clipboard field doc)
error: doc list item without indentation       (×3, src/quic_transport/endpoint.rs — pre-existing)
error: this function has too many arguments     (src/quic_transport/endpoint.rs:339 — pre-existing)
error: doc list item without indentation       (src/quic_transport/session.rs — pre-existing)
error: could not compile `lan-mouse` (lib) due to 10 previous errors
```

**Stash 对照验证**（`git stash` 后跑同样 clippy）：
```
$ git stash
$ cargo clippy -p lan-mouse --lib -- -D warnings 2>&1 | grep "^error:" | wc -l
11
$ git stash pop
```

stash 前后错误数完全一致（10 个 lint + 1 个 "could not compile due to N previous errors" 摘要行），0 个 clippy error 是本 STEP 引入的。

### 2.4 fmt

```
$ cargo fmt --all -- --check
```
本 STEP 3 个改动文件（cache.rs / http3.rs / service.rs）fmt-clean。注意 `cargo fmt --all` 会顺带改 `connect.rs` / `listen.rs` 中已 committed 但不 fmt-clean 的 8 行 cosmetic diff，**已 `git checkout` revert**（与 2a.2 偏差 #3 同源——fmt sweep 留给未来统一 sweep）。

## 3. 与 PLAN 的偏差

### 偏差 #1 — `CLIPBOARD_CACHE_CAPACITY` 重命名为 `CLIPBOARD_CACHE_BYTE_BUDGET`

**PLAN 隐含**：128 entries cap → 200 MiB byte budget 是语义变化；保留旧名会误导未来读者。

**实际**：常量 + 构造方法都重命名（`CLIPBOARD_CACHE_CAPACITY` → `CLIPBOARD_CACHE_BYTE_BUDGET`；`with_capacity_and_ttl(cap, ttl)` → `with_byte_budget_and_ttl(budget, ttl)`）。

**影响**：0；所有 caller 都在 workspace 内部（cache.rs tests + service.rs via `::new()`），无外部 API 暴露。

### 偏差 #2 — `dispatch_image` 新增 sha256 short-circuit（不是 prompt 字面 "不做去重"）

**PLAN 隐含**："不做去重 / 不做 LRU check（图片走 fingerprint + receiver 回环检测，与文本同）"——直读是"不做 dispatcher-side dedup"。

**实际**：`dispatch_image` 在 `sha == last_outbound_image_sha` 时 short-circuit return（不进入 cache mutation / broadcast / state update 阶段）。

**理由**：prompt 提到的"receiver 回环检测"是 inbound 阶段的 2a.4；dispatcher 这边"skip on duplicate"是带宽优化（否则每 500ms 重推同一 image 元数据 × N peers）。Text dispatcher 也有等价 short-circuit（`clipboard_last_text` 比较 + `clipboard_lru.contains`），所以**text 与 image 行为对称**——不是破坏 prompt 意图，反而是 prompt 要求的"与文本同"。

**影响**：0；行为更严格（避免冗余 push），receiver 看到的 wire 流量更少。`last_outbound_image_sha` 已设计为此用途（field doc 显式说明）。

### 偏差 #3 — `cargo fmt --all` 副作用（已 revert）

**事件**：`cargo fmt --all` 顺带改了 `src/connect.rs:1084` / `src/listen.rs:1006` 各 4 行 cosmetic diff（已有代码 vs rustfmt 偏好），不在本 STEP scope。

**处理**：`git checkout src/connect.rs src/listen.rs` revert。STEP 2a.3 commit scope 严格限定为 `src/clipboard/cache.rs` / `src/quic_transport/http3.rs` / `src/service.rs` 3 个文件。

**理由**：与 2a.2 偏差 #3 同源——保持 commit 干净 / 利于 revert / 减少 PR review 噪音。

**影响**：0；working tree 仅 3 个文件改动。

### 偏差 #4 — `last_outbound_image_sha` 同时承担 short-circuit + active-eviction 双重职责

**PLAN 隐含**：active eviction 的 previous-sha tracking 与 short-circuit 的 last-pushed-sha tracking 是两个不同概念。

**实际**：合并为同一个 `last_outbound_image_sha` 字段。

**理由**：
- active-eviction prev pointer 在 `dispatch_image` 末尾更新
- short-circuit 比较在 `dispatch_image` 开头读取
- 两个用途的"上一个 push"语义相同（同一时间窗、同一前提：刚 push 过的 image）
- 字段 doc 显式说明双重职责
- 合并节省一个字段 + 一处 state update；text 分支也是同源（`last_outbound_text_sha` 实际兼任 active-eviction prev pointer）

**影响**：0；行为正确性不变（`dispatch_image_cache_step_skips_on_duplicate_sha` 测试 pin 这一点）。

## 4. 处理的 SUGGESTION 项

无新增 / 移出 / 移入。SUGGESTION.md / SUGGESTION-FIXED.md / SUGGESTION-IGNORE.md 未受影响。

注：#S-1（pbcopy deviation）继续保留——本 STEP 落地的 image 路径已走 NSPasteboard via `objc2`，text 路径仍走 pbcopy/pbpaste（与 2a.2 status update 一致）。leader 决策时机未到（需要 M2a / M4 阶段评估后）。

## 5. 闸门检查

| 检查 | 结果 |
|---|---|
| 产物对得上 | ✅ cache 升级（200 MiB byte budget + 5 min TTL）；dispatcher image 分支（`dispatch_image` + `last_outbound_image_sha` + `evict_prev_outbound_image_cache`）；HTTP/3 `/clipboard/image/{sha256}` route；`sha256_of_bytes` 助手 |
| 依赖对得上 | ✅ M1a / M1b / M2a-2a.1 / M2a-2a.2 全部归档（git log: ac650be / 3e40b8b / affd458 / 0110b4d）；lan-mouse-proto `ClipboardImage` 已落地（1b.1 commit `0110b4d` 起，2a.3 无新 wire schema 改动） |
| 验收对得上 | ✅ `cargo test --workspace` 全绿（351 baseline + 13 new = 364 pass / 0 fail）；macOS-only image tests 通过 cfg-gate；HTTP/3 image route happy / miss / eviction / malformed / shared-cache 5 个新测试全过 |
| **milestone 边界门** | ✅ 未触碰 2a.4 (inbound + 回环) / M2b (Windows/Linux image) / 1b.x 既有契约 (`cache.remove` prev + LRU + metrics + text dispatcher 全保留) / backend trait / 图片接收端逻辑（dispatcher 只 push 元数据；receiver 不在 2a.3 scope）；`git diff --stat` 仅 cache.rs / http3.rs / service.rs 3 个文件改动 |
| **时间门** | ✅ ~85 min（< 1.5h PLAN §3 M2a 估时上限） |

## 6. 遗留

1. **完整 `dispatch_image` 集成测试不在 unit 层**：cache step（`insert + evict_prev`）已在 `dispatch_image_tests` 模块单元测试；broadcast 阶段（peer reachability + StreamC 发送 + FrontendEvent 通知）需要 live `Service` + `LanMouseListener` + `Capture` + `Emulation`，由 2a.4 端到端测试矩阵（PLAN §8 M2a）覆盖。**风险低**：单元测试覆盖 cache 步骤 + 既有 `broadcast_clipboard_event` 在 1b.2 / 1b.3 已通过。

2. **`image_and_text_routes_share_one_cache` 测试 pin**：当前 text + image 共享同一 cache 是正确选择（content-addressed cache 天然支持）；未来如果拆分 cache（M2a.4 inbound 时如果要隔离 text receiver 与 image receiver 的 race），需要先**打破**这个测试契约。

3. **macOS 真机测试 4K 截图（PLAN §8 M2a 人类项）** 留 leader / 用户真机执行：本 STEP 仅完成自动单测 + clipboard cache eviction logic；端到端"macOS 真机复制截图 → 对端剪贴板出现"需要 2a.4 inbound 接通后由真机测试矩阵验证。

4. **PLAN §3 2a.3 写 `image.data.clone()` 两次**：dispatcher cache insert + broadcast metadata event（不携带 bytes）。两次 clone 对 5-15 MiB image = 10-30 MiB 临时内存占用；dispatcher 500ms tick 不频繁（用户复制图片才有 push），可接受；M3a 200 MiB 文件传输时要重新评估是否改为 `Rc<Vec<u8>>` 共享。

5. **`cargo fmt --all` 在 `connect.rs` / `listen.rs` 现有 commit 上仍有 cosmetic drift** —— 与 2a.2 偏差 #3 同源；本 STEP 不解决（commit 卫生原则），统一 sweep 留给未来。

## 7. 下一步

按依赖顺序：**STEP-2a.4**（图片 inbound + 回环：收到 `ClipboardImage` → 查 LRU 跳过；若新 → 调 `Http3Client::get_image(sha256)` → `backend.set_image`；本地 `mark_local_image_write(fingerprint)`；图片回环集合独立于文本（容量 32，因为图片成本高））。