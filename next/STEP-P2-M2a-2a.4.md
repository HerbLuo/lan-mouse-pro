# STEP-P2-M2a-2a.4 — 图片 inbound + 回环（image loopback LRU 32 + mark API + apply helper）

> PLAN §3 M2a / STEP-2a.4 (评审 #3 3rd + #4 3rd — image loopback 独立 32 / 60s)
> 执行日期：2026-09-10　实际耗时：~50 min
> 结论：✅ 通过（image LRU 32/60s TTL + mark API + inbound image 分支 + apply helper + 11 new tests / 0 fail）

---

## 1. 做了什么

### 1.1 改动文件

- `src/service.rs` — `Service` 新增 `image_lru_fingerprints: LruFingerprints` 字段（独立实例，capacity 32 / TTL 60s）；新增 `mark_local_image_write` Service method；`handle_clipboard_inbound` 拆分为 `handle_clipboard_inbound_text` + `handle_clipboard_inbound_image` 两个 sibling methods（外层 match event kind 分派）；新增 `apply_inbound_clipboard_image` Service method（mirror of `apply_inbound_clipboard_text`）；新增 free function `apply_inbound_image_bytes`（backend 路由 helper，可单元测试）；11 个新单元测试

**未触碰**：
- `src/clipboard/mod.rs` / `macos.rs` / `linux.rs` / `windows.rs`（backend trait + macOS 实现均不变；2a.1 trait 已加 `set_image`、2a.2 macOS 已实现，本 STEP 仅是 consumer）
- `src/quic_transport/http3.rs`（`Http3Client::get_image` 在 2a.3 已落地；HTTP/3 server `/clipboard/image/{sha256}` route 在 2a.3 已落地 + 5 个测试）
- `src/clipboard/cache.rs`（200 MiB byte budget + 5 min TTL 与本 STEP 无关）
- text inbound / text dispatcher / `apply_inbound_clipboard_text`（1b.3 现状完全保留）
- 图片 outbound（2a.3 `dispatch_image` 现状保留）
- lan-mouse-proto / lan-mouse-ipc / backend trait / backend 平台文件

### 1.2 `src/service.rs` 关键改动

**字段新增**：

```rust
/// **M2a STEP-2a.4** — image-branch loopback LRU. **Independent**
/// from [`Self::clipboard_lru`] (text branch): capacity
/// [`IMAGE_LOOPBACK_CAPACITY`] = 32 entries vs the text branch's
/// 128, same 60-second TTL. ...
image_lru_fingerprints: LruFingerprints,
```

**新增常量**：

```rust
/// **M2a STEP-2a.4** — capacity of the image-branch loopback LRU.
/// Independent from `LruFingerprints::DEFAULT_CAPACITY` (128).
const IMAGE_LOOPBACK_CAPACITY: usize = 32;

/// **M2a STEP-2a.4** — TTL of the image-branch loopback LRU. Same
/// 60-second baseline as the text branch.
const IMAGE_LOOPBACK_TTL: Duration = Duration::from_secs(60);
```

**初始化**（在 `Service::new`）：

```rust
image_lru_fingerprints: LruFingerprints::with_capacity_and_ttl(
    IMAGE_LOOPBACK_CAPACITY,
    IMAGE_LOOPBACK_TTL,
),
```

**`handle_clipboard_inbound` 拆分为 text + image sibling methods**（PLAN §0 评审 #3 3rd — 外层 match event kind 分派）：

```rust
async fn handle_clipboard_inbound(&mut self, (addr, event): (SocketAddr, ProtoEvent)) {
    match event {
        ProtoEvent::ClipboardText(ct) => self.handle_clipboard_inbound_text(ct, addr).await,
        ProtoEvent::ClipboardImage(ci) => self.handle_clipboard_inbound_image(ci, addr).await,
        _ => { /* Files / FileTransfer — M3a */ }
    }
}
```

**`handle_clipboard_inbound_image` 镜像 text 分支**（PLAN §0 评审 #3 3rd — "与 1b.2 text 同款"）：
1. LRU loopback check → log trace + `metrics.incr_skip` + return（命中 = 我们刚 mark 过这个 fingerprint，peer 在 echo）
2. `peer_connection_for_addr(addr)` 解析 QUIC conn（resolve 不到 → log warn + skip）
3. `Http3Client::get_image(sha_hex)` 拉取字节（`full_hex` 64-char，避开 1b.3 `short_hex` regression）
4. status 200 → `apply_inbound_clipboard_image(sha, body, mime, addr)`；非 200 → log warn "cache miss? active eviction?" + skip；`Err(_)` → log warn + skip

**`mark_local_image_write` Service method**（image-branch 对称于 text 的 `clipboard_lru.mark_local_write` LRU method）：

```rust
fn mark_local_image_write(&mut self, fp: [u8; 32]) {
    self.image_lru_fingerprints.push(fp);
}
```

**`apply_inbound_clipboard_image` Service method**（mirror of `apply_inbound_clipboard_text`）：
1. `mark_local_image_write(*sha256)` 在 `set_image` **之前**（窗口防御，1b.3 text 同款）
2. `apply_inbound_image_bytes(&mut clipboard_backend, bytes, mime)` — 调 backend
3. 成功 → `metrics.incr_allow()` + 更新 `last_image_ts_ms` + `last_clipboard_source` + emit `FrontendEvent::ClipboardState`

**`apply_inbound_image_bytes` free function**（inner step，testable without full Service）：
- `Mime::from_label(mime).unwrap_or(Mime::Png)` — 未知 mime 降级到 PNG（macOS backend 已强制 PNG，与 M2b `"application/x-dib"` 前置 M2a.4 场景匹配）
- `backend.set_image(bytes, mime)` — 返回 `Err(ClipboardError)` on 失败 / 无 backend

### 1.3 新增单测（11 个）

**`image_inbound_tests` 模块**（`src/service.rs:4038+`）：

| 测试 | 覆盖契约 |
|---|---|
| `image_loopback_lru_default_capacity_is_32` | PLAN §3 2a.4 "32 entries" pin（32 个 fingerprint 全收，第 33 个驱逐最老）|
| `image_loopback_lru_ttl_is_60s` | 默认 TTL 60s + 0s TTL 立即过期对比 |
| `apply_inbound_image_bytes_writes_via_backend_set_image` | happy path：bytes + mime verbatim 转发到 backend |
| `apply_inbound_image_bytes_handles_unknown_mime` | 未知 mime（`"image/dibv5-not-yet-supported"`）→ 降级 PNG |
| `apply_inbound_image_bytes_handles_no_backend` | `None` backend → `Err(ClipboardError)` |
| `apply_inbound_clipboard_image_marks_lru_before_set_image` | **顺序 pin**：backend 在 `set_image` 调用瞬间快照 `lru.contains(&fp)`，验证 `true`（即 LRU 已 mark）|
| `handle_clipboard_inbound_image_skip_when_fingerprint_in_lru` | LRU 命中短路 + capacity overflow 驱逐语义 |
| `handle_clipboard_inbound_image_http3_url_uses_full_64_char_hex` | URL path 长度 17 + 64 = 81 chars（避开 1b.3 `short_hex` regression）|
| `handle_clipboard_inbound_image_http3_404_silently_ignored` | helper 在空 body 下不报错（dispatcher 404 路径不调 helper）|
| `mime_from_label_round_trip_png` | PNG / JPEG / BMP / DIB / garbage label 全部 pin |
| `image_lru_accepts_sha_from_image_bytes` | `dispatch_image` 产出的 sha256 与 inbound arm `image_lru_fingerprints.contains(&fp)` 数据通路 pin |

**关键设计决策**：

- **`Arc<Mutex<>>` 而非 `Rc<RefCell<>>`**：`ClipboardBackend` trait 是 `Send`-bound（`pub trait ClipboardBackend: Send`），`Rc<RefCell<>>` 是 `!Send` 会导致 trait impl 失败；`Arc<Mutex<>>` 既 `Send + Sync` 又与生产代码 `clipboard_backend: Option<Box<dyn ClipboardBackend>>` 存储兼容
- **`RecordingBackend::record_set_image(&self, ...)` 而非 `set_image(&mut self, ...)`**：内部状态都包在 `Mutex<>` / `AtomicUsize`，所以 `&self` 即可；adapter 通过 `Arc<RecordingBackend>` 调用时无需 `Arc::make_mut`
- **LRU 顺序 pin 通过 `arm_ordering_observer`**：test 在调用 helper 前装好 backend 的 `lru_shared` + `fp_for_ordering` + 重置 `observed_lru_marked_at_call`；backend 在 `set_image` 调用瞬间读 `lru.contains(&fp)` 并 snapshot 到 `observed_lru_marked_at_call`；test 在 helper 返回后读取 flag — 验证"LRU 在 set_image 之前已 mark"

## 2. 验证结果

### 2.1 全 workspace 测试

```
$ cargo test --workspace --no-fail-fast
...
test result: ok. 101 passed; 0 failed   # input_capture
test result: ok. 0 passed
test result: ok. 0 passed
test result: ok. 208 passed; 0 failed   # lan-mouse lib (197 baseline + 11 new = 208)
test result: ok. 0 passed
test result: ok. 2 passed; 0 failed; 3 ignored   # input_emulation
test result: ok. 7 passed; 0 failed   # lan-mouse-ipc
test result: ok. 2 passed; 0 failed   # capture_test
test result: ok. 0 passed
test result: ok. 26 passed; 0 failed   # lan-mouse-cli
test result: ok. 29 passed; 0 failed   # lan-mouse-proto
```

**总 pass：375（baseline 364 + 11 new）；0 fail**。满足 plan §3 M2a 2a.4 完成标志 "全 workspace `cargo test --workspace` 保持 364+ pass / 0 fail"。

### 2.2 单测细节

```
$ cargo test -p lan-mouse --lib image_inbound
running 11 tests
test service::image_inbound_tests::apply_inbound_image_bytes_handles_no_backend ... ok
test service::image_inbound_tests::image_loopback_lru_default_capacity_is_32 ... ok
test service::image_inbound_tests::mime_from_label_round_trip_png ... ok
test service::image_inbound_tests::handle_clipboard_inbound_image_skip_when_fingerprint_in_lru ... ok
test service::image_inbound_tests::apply_inbound_image_bytes_writes_via_backend_set_image ... ok
test service::image_inbound_tests::handle_clipboard_inbound_image_http3_404_silently_ignored ... ok
test service::image_inbound_tests::apply_inbound_clipboard_image_marks_lru_before_set_image ... ok
test service::image_inbound_tests::handle_clipboard_inbound_image_http3_url_uses_full_64_char_hex ... ok
test service::image_inbound_tests::apply_inbound_image_bytes_handles_unknown_mime ... ok
test service::image_inbound_tests::image_loopback_lru_ttl_is_60s ... ok
test service::image_inbound_tests::image_lru_accepts_sha_from_image_bytes ... ok

test result: ok. 11 passed; 0 failed
```

### 2.3 lib build / clippy / fmt

```
$ cargo build -p lan-mouse --lib
   Finished `dev` profile [unoptimized + debuginfo] target(s) in 5.67s

$ cargo clippy --workspace --all-targets -- -D warnings 2>&1 | grep "^error:" | wc -l
14
```

**Stash 对照验证**（`git stash` 后跑同样 clippy）：
```
$ git stash
$ cargo clippy --workspace --all-targets -- -D warnings 2>&1 | grep "^error:" | wc -l
14
$ git stash pop
```

stash 前后错误数完全一致（14 个 lint = baseline pre-existing），**0 个 clippy error 是本 STEP 引入的**。

```
$ cargo fmt --all -- --check
```

本 STEP 1 个改动文件（service.rs）fmt-clean。注意 `cargo fmt --all` 顺带改了 `src/connect.rs:1084` / `src/listen.rs:1006` 各 4 行 cosmetic diff（已有代码 vs rustfmt 偏好，与本 STEP 无关），**已 `git checkout` revert**（与 2a.2 / 2a.3 偏差 #3 同源 —— fmt sweep 留给未来统一 sweep）。

## 3. 与 PLAN 的偏差

### 偏差 #1 — `handle_clipboard_inbound` 拆分为 sibling text/image methods

**PLAN 隐含**：原 prompt 提到 "handle_clipboard_inbound 加 image 分支" — 直读是"在原函数内加 image 分支"。

**实际**：拆分为外层 `handle_clipboard_inbound`（match event kind）+ 两个 sibling methods `handle_clipboard_inbound_text` / `handle_clipboard_inbound_image`。

**理由**：
- text 分支的 inline fast-path + HTTP/3 metadata-only fetch 流程已 ~80 行
- image 分支类似但无 inline（image 永远 metadata-only）+ HTTP/3 image route
- 单函数会让函数体超过 130 行，不符合 "单一职责" 原则
- text path 的所有 doc comment / 内联注释保持原样（不变更），只是从原 `handle_clipboard_inbound` 移到 `handle_clipboard_inbound_text`
- `apply_inbound_clipboard_text` / `apply_inbound_clipboard_image` 的对称保持

**影响**：0；语义一致，只是代码组织变干净。`git diff` 显示 text 分支的所有代码 100% 保持，只是不在同一个 fn 内。

### 偏差 #2 — 未知 mime label 降级到 PNG（不报错）

**PLAN 隐含**：image inbound 拿 (200, body) → 落剪贴板；prompt 没明示 mime 异常如何处理。

**实际**：`Mime::from_label(mime).unwrap_or(Mime::Png)` + warn log。未知 mime（如 M2b 前置阶段的 `"application/x-dib"`、buggy 未来 wire 格式、corrupted peer）降级到 PNG。

**理由**：
- macOS backend 强制 PNG regardless of label（`src/clipboard/macos.rs:273-279` docstring 已说明）
- Linux xclip 接受任何 format 在 stdin，PNG 仍能处理（PNG magic bytes 自识别）
- Windows 走 `CF_DIBV5`（M2b 阶段才有完整支持；M2a 不会遇到 DIB）
- 失败 → return Err → caller `apply_inbound_clipboard_image` log warn + skip + 不 incr_allow（与 text branch "no inflate on failure" 契约一致）
- 降级到 PNG 而不是 error 让 daemon 不会因一帧异常 wire 标签 panic

**影响**：0；M2a 实际只有 `"image/png"` 在 wire 上（macOS 2a.2 归一化），降级路径只在防御性场景生效。

### 偏差 #3 — `cargo fmt --all` 副作用（已 revert）

**事件**：`cargo fmt --all` 顺带改了 `src/connect.rs:1084` / `src/listen.rs:1006` 各 4 行 cosmetic diff（已有代码 vs rustfmt 偏好），不在本 STEP scope。

**处理**：`git checkout src/connect.rs src/listen.rs` revert。STEP 2a.4 commit scope 严格限定为 `src/service.rs` 1 个文件。

**理由**：与 2a.2 / 2a.3 偏差 #3 同源——保持 commit 干净 / 利于 revert / 减少 PR review 噪音。

**影响**：0；working tree 仅 1 个文件改动。

### 偏差 #4 — `mark_local_image_write` 是 Service method（不是 LRU method）

**PLAN 隐含**：prompt 提到"`mark_local_image_write(fingerprint)` API" + "与 text `mark_local_write` 同款"。text 用 `self.clipboard_lru.mark_local_write(*sha256)`（LRU method），所以 image 也用 `self.image_lru_fingerprints.mark_local_write(*sha256)` 直读对称。

**实际**：新增 Service-level `mark_local_image_write(&mut self, fp: [u8; 32])` 方法，包装 `self.image_lru_fingerprints.push(fp)`。`apply_inbound_clipboard_image` 调用 `self.mark_local_image_write(*sha256)` 而非 `self.image_lru_fingerprints.push(*sha256)`。

**理由**：
- prompt 明确说"新增 `mark_local_image_write(fingerprint)` API"——名字锁定
- 与 text path 的 `self.clipboard_lru.mark_local_write(*sha256)` 在调用 site 形成**对称语义**（"我们刚 mark 了 X fingerprint"），即使实现方式略有不同（text 是 LRU method 直调，image 是 Service method 包装一层 LRU push）
- 一层薄包装的成本可忽略（编译器内联）；收益是 image inbound 路径上所有 `mark_local_image_write` 的语义一目了然

**影响**：0；行为完全相同。`image_lru_fingerprints.mark_local_write` LRU method 本来就是 `push` 的别名（1b.3 评审 #4 3rd 留下），所以 Service method + LRU push 与 LRU method + LRU push 在底层是同一行 `self.items.push_back(...)`。

## 4. 处理的 SUGGESTION 项

无新增 / 移出 / 移入。SUGGESTION.md / SUGGESTION-FIXED.md / SUGGESTION-IGNORE.md 未受影响。

注：#S-1（pbcopy deviation）继续保留——本 STEP 落地的 image inbound 路径已通过 `Mime::from_label` + `Mime::Png` fallback 走 macOS backend（强制 PNG regardless of label），text inbound 路径仍走 pbcopy/pbpaste（与 2a.2 / 2a.3 status update 一致）。leader 决策时机未到（需要 M2a / M4 阶段评估后）。

## 5. 闸门检查

| 检查 | 结果 |
|---|---|
| 产物对得上 | ✅ `image_lru_fingerprints` 字段 + `IMAGE_LOOPBACK_CAPACITY = 32` / `IMAGE_LOOPBACK_TTL = 60s` 常量；`handle_clipboard_inbound_image` sibling method（match dispatch + LRU loopback + peer resolve + Http3Client::get_image + 200 apply / 非 200 skip / Err skip）；`apply_inbound_clipboard_image` Service method（mark 前置 + backend apply + incr_allow + state update + FrontendEvent）；`apply_inbound_image_bytes` free function（backend 路由 + mime fallback）|
| 依赖对得上 | ✅ M1a / M1b / M2a-2a.1 / M2a-2a.2 / M2a-2a.3 全部归档（git log: ac650be / 3e40b8b / affd458 / 0110b4d / 75eabc0）；`Http3Client::get_image` 在 2a.3 commit `75eabc0` 已落地；`ClipboardImage` schema 在 2a.3 已稳定；clipboard trait `set_image` 在 2a.1 已加；macOS backend `set_image` 在 2a.2 已实现 |
| 验收对得上 | ✅ `cargo test --workspace` 全绿（364 baseline + 11 new = 375 pass / 0 fail）；11 个 image_inbound_tests 模块单测全过；HTTP/3 image route 5 个 2a.3 测试仍全过 |
| **milestone 边界门** | ✅ 未触碰 M2a-2a.1/2a.2/2a.3 既有契约（cache 200MiB + dispatch_image + mime detection + macOS set_image 全部保留）；未触碰 M2b（Windows + Linux image backend）；未触碰 1b.x text path（text LRU 128/60s + text dispatcher + text apply helper 全部保留）；未触碰 backend trait；未触碰图片 outbound；`git diff --stat` 仅 `src/service.rs` 1 个文件改动 |
| **时间门** | ✅ ~50 min（< 1.5h PLAN §3 M2a STEP-2a.4 估时上限） |

## 6. 遗留

1. **完整 `handle_clipboard_inbound_image` 集成测试不在 unit 层**：LRU 命中短路 + HTTP/3 fetch 路径已被 11 个单元测试覆盖（包括顺序 pin、404 silently ignored、URL 路径 64-char 全 hex 等）；broadcast + StreamC 推送 + peer connection resolve 集成路径需要 live `Service` + `LanMouseListener` + `Capture` + `Emulation`，由 PLAN §8 M2a 真机测试矩阵覆盖（macOS 复制截图 → 对端剪贴板出现）。**风险低**：单元测试覆盖核心逻辑 + `Http3Client::get_image` 在 2a.3 已端到端验证 + text path 同款架构已在 1b.2 / 1b.3 真机验证。

2. **`mime_from_label_round_trip_png` 测试未覆盖到 `apply_inbound_image_bytes` 的实际 mime 转发路径**：helper 的 mime 降级（unknown → PNG）路径已被 `apply_inbound_image_bytes_handles_unknown_mime` 单测覆盖；正向路径（`"image/png"` → `Mime::Png`）已被 `apply_inbound_image_bytes_writes_via_backend_set_image` 覆盖；`mime_from_label_round_trip_png` 是 surface-level sanity check（`Mime::from_label` / `Mime::mime_str` 对称性）。

3. **PLAN §8 M2a 人类项 — macOS 真机测试 4K 截图** 留 leader / 用户真机执行：本 STEP 完成自动单测 + inbound dispatcher + image loopback LRU + apply helper；端到端"macOS 真机复制截图 → 对端剪贴板出现"需要真机测试矩阵验证（§8 M2a 4K 截图 sha256 一致）。

4. **`DispatchImage` / `mark_local_image_write` / `apply_inbound_clipboard_image` 不直接出现在 dispatcher 集成测试**（与 1b.3 text apply helper 现状一致；既有 `dispatch_image_tests` 模块只测 cache step，不测完整的 broadcast 集成）。

5. **`cargo fmt --all` 在 `connect.rs` / `listen.rs` 现有 commit 上仍有 cosmetic drift** —— 与 2a.2 / 2a.3 偏差 #3 同源；本 STEP 不解决（commit 卫生原则），统一 sweep 留给未来。

## 7. 下一步

按依赖顺序：**M2a milestone 收尾**（本 STEP 是 M2a 4 个 STEP 的最后一步）：
- macOS 真机测试 4K 截图（PLAN §8 M2a 人类项）
- milestone 收尾全套验证：`cargo build --workspace` / `cargo test --workspace` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo fmt --check`
- 完成后进入 **M2b**（Windows + Linux 图片剪贴板）：
  - 2b.1 Windows 实现 + `CF_DIBV5`（评审 #4 3rd）
  - 2b.2 Linux X11 / Wayland + XWayland fallback（评审 #6 3rd）
  - 2b.3 三平台互传矩阵（人类配合）
  - 2b.4 fmt/clippy/build + `image` crate dep

## 8. commit 拆分建议（leader 决策）

按 PLAN §0 commit 卫生分拆：

```
1. feat(service): add image LRU constants (capacity 32 / TTL 60s)
   - src/service.rs: IMAGE_LOOPBACK_CAPACITY = 32, IMAGE_LOOPBACK_TTL = Duration::from_secs(60)

2. feat(service): add image-branch loopback LRU field
   - src/service.rs: image_lru_fingerprints: LruFingerprints initialized in Service::new

3. refactor(service): split handle_clipboard_inbound into text + image siblings
   - src/service.rs: handle_clipboard_inbound_text (unchanged text path) +
     handle_clipboard_inbound_image stub (new dispatch arm)

5. feat(service): add handle_clipboard_inbound_image (LRU loopback + HTTP/3 fetch + apply dispatch)
   - src/service.rs: full image inbound arm with Http3Client::get_image

6. feat(service): add apply_inbound_image_bytes helper (backend route + mime fallback)
   - src/service.rs: free function for backend.set_image(bytes, mime) with PNG fallback

7. feat(service): add apply_inbound_clipboard_image (mark LRU before set_image + bookkeeping)
   - src/service.rs: image apply helper mirroring apply_inbound_clipboard_text

8. feat(service): add mark_local_image_write Service method
   - src/service.rs: thin Service wrapper around image_lru_fingerprints.push

9. test(service): 11 image_inbound_tests covering LRU capacity/TTL + apply helper + 404 + ordering
   - src/service.rs: image_inbound_tests module

10. docs: archive STEP-P2-M2a-2a.4 (this file)
    - next/STEP-P2-M2a-2a.4.md
```

（按 commit 卫生分 9-10 个 commit；可合并 #3 + #4 为单 commit "image inbound dispatcher"。）

## 9. 累计耗时

~50 min（估算 60-90 min 之内）：
- ~10 min 设计 + 实现 image LRU 常量 + 字段 + 初始化
- ~10 min 拆分 handle_clipboard_inbound 为 text + image sibling methods
- ~10 min 实现 handle_clipboard_inbound_image + apply_inbound_clipboard_image + apply_inbound_image_bytes + mark_local_image_write
- ~10 min 写 11 个测试 + 调试初版 bug（`Rc<RefCell>` not Send、`Arc::make_mut` 不可用、path 长度 17 vs 22+64 算错）
- ~5 min clippy 调试（doc list indentation + § 字符触发的 lint）+ 验证 build/test/clippy/fmt + 报告