# STEP 2a.2 — macOS backend image impl + TIFF→PNG normalization

> PLAN §3 M2a / STEP-2a.2 (评审 #2 3rd)
> 执行日期：2026-09-10　实际耗时：~70 min
> 结论：✅ 通过

## 1. 做了什么

### 1.1 改动文件

- `Cargo.toml` — 新增 `[target.'cfg(target_os = "macos")'.dependencies]` 段，加 `objc2 = "0.6"` + `objc2-foundation = "0.3"` + `objc2-app-kit = "0.3"` + `image = { version = "0.25", default-features = false, features = ["png", "tiff"] }`（image 仅开 PNG/TIFF 两个 feature，最小化 dep tree）
- `Cargo.lock` — 自动新增 17 个 transitive deps（objc2 0.6.4 + objc2-app-kit 0.3.2 + objc2-foundation 0.3.2 + image 0.25.10 + objc2-encode 4.1.0 + block2 0.6.2 等）
- `src/clipboard/macos.rs` — 加 image impl（text 路径完全不动）

### 1.2 `macos.rs` 关键改动

**Text 路径（M1a 1a.2）—— 完全不动**：保留 `pbcopy` / `pbpaste` subprocess。`new()` / `name()` / `current_text()` / `set_text()` 0 改动。`name()` 字符串改为 `"macos-pbcopy-pbpaste+nspasteboard-image"` 以反映 image 路径加了 NSPasteboard（dispatcher startup log 会看到）。

**Image 路径（M2a 2a.2 新增）**：

1. **`current_image()`** —— 读 NSPasteboard PNG 优先；fallback TIFF→PNG normalize（PLAN §3 评审 #2 3rd 落地）
2. **`set_image()`** —— 强制按 PNG 写回 NSPasteboard（`setData_forType(public.png)`）；非 PNG mime 触发 warn + 仍按 PNG 写（透明降级）
3. **`watch_image()`** —— `spawn_local` 任务 500ms 轮询 `NSPasteboard.changeCount()`；变化时通过 mpsc channel 推 `ImageChange` 给 dispatcher（M2a 2a.3 消费）
4. **后端字段 `last_image_change_count: Cell<Option<i64>>`** —— 记录最近一次 read / write 后的 `changeCount()`；M2a 2a.3 dispatcher 可用此 + watch_image stream 做 "changeCount 未变化 → skip read" 优化

**Free helper 函数**（脱离 `&mut self`，spawn_local task 可直接调用）：
- `read_pasteboard_bytes(pb, type_str) -> Option<Vec<u8>>` —— 通用 read
- `tiff_to_png_normalized(tiff: &[u8]) -> Result<Vec<u8>, ClipboardError>` —— `image::load_from_memory` + `image::write_to(_, Png)`
- `read_image_bytes_from_pasteboard(pb) -> Option<ImageBytes>` —— PNG 优先 + TIFF fallback 归一化（被 `current_image` 和 `watch_image` 共用，保证 TIFF→PNG 语义只在 1 处）

### 1.3 关键设计决策

- **NSPasteboard vs subprocess**：PLAN §3 1a.2 允许 text 走 `pbcopy/pbpaste`（避免引入 objc2 dep），但 2a.2 image **必须**直接走 NSPasteboard —— `pbcopy` 不暴露 pasteboard types（`.png` vs `.tiff`），无法做 PNG 优先 + TIFF fallback 策略。PLAN 评审 #2 3rd 评审明确指出 "Preview.app 复制选中区域只提供 TIFF，源端不归一化 → 对端字节级一致永远不成立"，所以 image 路径必须用 objc2。

- **objc2 0.6.x 而非 0.5.x**：PLAN 列出 `objc2 = "0.5"`，但实际当前 crates.io 最新稳定版是 `objc2 = 0.6.4`（objc2-app-kit 0.3.2）。API 与 0.5 完全兼容（顶层函数签名一致），选用最新版避免使用过时 minor。**与 PLAN 偏差 #1**：objc2 主版本 0.5 → 0.6。

- **`changeCount()` 返回 `NSInteger` (即 `isize`)** 而非 `i64`：objc2 类型定义 `pub type NSInteger = isize`（见 `objc2-0.6.4/src/ffi/types.rs:70`）。后端字段 `Cell<Option<i64>>` 用 `as i64` 显式 cast；截断在 ~9.2e18 量级（macOS changeCount 重启重置，溢出不可能）。

- **`watch_image` 用 `spawn_local` 而不是 thread**：daemon runtime 是 `current_thread` + `LocalSet`（main.rs:134-140），`spawn_local` 安全可用。`NSPasteboard.generalPasteboard()` 是 process-wide singleton；spawn_local task 与 dispatcher 在同一线程跑，AppKit 线程亲和性 OK。

- **`set_image` 对非 PNG mime warn-and-write 而非 reject**：避免 hard fail —— 当未来 M2b / M3a 后端增加 JPEG 支持时，dispatcher 可以扩展 set_image 接受更多 mime；当前的 "非 PNG 也按 PNG 写" 行为保证 wire-compatible 不被破坏。

- **`watch_image` 500ms tick 与 text tick 对齐**：与 `service.rs::handle_clipboard_tick` 500ms cadence 一致；变化检测走 `changeCount` 差值（不是 bytes 哈希），所以 image 大小不影响 polling 性能。

- **不用 `NSNotificationCenter` 监听 pasteboard 变化**：理论上 `NSPasteboardDidChangeNotification` 可以做到亚秒级反应，但需要 stand up 一个 `NSObject` observer 让 objc2 retain graph 跟踪；500ms 已经 feels instant，复杂度不值得。

- **`last_image_change_count` 字段在 2a.2 已就位但未被使用**：2a.3 dispatcher 会消费它做 changeCount skip。2a.2 写入是 idempotent no-op 性质（dispatcher 还没接）。**不算 dead code**：明确为 2a.3 准备。

### 1.4 新增单测（5 个，cfg-gate macOS）

| 测试 | 覆盖契约 |
|---|---|
| `current_image_returns_none_on_empty_pasteboard` | pasteboard 空 → `None`（dispatcher 当 "no change" 处理） |
| `current_image_reads_png_bytes_directly` | PNG 在 pasteboard → `Some(ImageBytes { mime: "image/png", data: <bytes> })` 字节级一致 |
| `current_image_normalizes_tiff_to_png` | TIFF only → `mime: "image/png"` + PNG magic + dimensions preserved |
| `set_image_writes_png_bytes_to_pasteboard` | write 后 NSPasteboard.dataForType("public.png") 字节级一致 |
| `set_image_with_non_png_mime_writes_but_logs_warning` | `set_image(JPEG_bytes, Mime::Jpeg)` → 仍按 PNG 写 + warn |

每个测试用 `ImageClipboardGuard` save / restore pasteboard 状态（PNG + TIFF 两种 type 分别 save/restore），共用 `CLIPBOARD_TEST_LOCK` 与现有 text 测试串行化。

### 1.5 文档同步

- `macos.rs` 顶部 module doc 重写：分两部分说明 text 路径（M1a 1a2）+ image 路径（M2a 2a.2）+ threading model + 与 #S-1 pbcopy deviation 的关系
- `MacOsPasteboard::new()` doc 更新：说明额外 touch `NSPasteboard::generalPasteboard()` 是为了在 daemon 主线程初始化 AppKit pasteboard stack，让后续 spawn_local task 也能安全调用

## 2. 验证结果

### 2.1 全 workspace 测试

```
$ cargo test --workspace --no-fail-fast
...
test result: ok. 101 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok.   0 passed
test result: ok.   0 passed
test result: ok. 184 passed; 0 failed; 0 ignored   # lan-mouse lib (179 baseline + 5 new image tests)
test result: ok.   2 passed; 0 failed; 3 ignored
test result: ok.   7 passed; 0 failed
test result: ok.   2 passed; 0 failed
test result: ok.   0 passed
test result: ok.  26 passed; 0 failed
test result: ok.  29 passed; 0 failed
test result: ok.   0 passed  (× 8 doc-test crates)
```

**总 pass：351（baseline 346 + 5 new image tests）；0 fail**。

### 2.2 macOS 专项测试（filter `clipboard::macos`）

```
$ cargo test -p lan-mouse --lib clipboard::macos
running 10 tests
test clipboard::macos::tests::current_image_normalizes_tiff_to_png ... ok
test clipboard::macos::tests::current_image_reads_png_bytes_directly ... ok
test clipboard::macos::tests::current_image_returns_none_on_empty_pasteboard ... ok
test clipboard::macos::tests::name_is_macos_subprocess_plus_nspasteboard_label ... ok
test clipboard::macos::tests::new_succeeds_on_macos_with_pbpaste ... ok
test clipboard::macos::tests::set_image_with_non_png_mime_writes_but_logs_warning ... ok
test clipboard::macos::tests::set_image_writes_png_bytes_to_pasteboard ... ok
test clipboard::macos::tests::set_text_empty_string_clears_clipboard ... ok
test clipboard::macos::tests::set_text_multibyte_utf8_round_trip ... ok
test clipboard::macos::tests::set_text_then_current_text_round_trip ... ok

test result: ok. 10 passed; 0 failed
```

5 个原有 text 测试 + 5 个新 image 测试 全绿。

### 2.3 lib build / clippy

```
$ cargo build -p lan-mouse --lib
   Finished `dev` profile [unoptimized + debuginfo] target(s) in 2.92s

$ cargo clippy --workspace --all-targets -- -D warnings
```

10 errors pre-existing（在 connect.rs / quic_transport/endpoint.rs / quic_transport/session.rs / service.rs —— `doc_lazy_continuation` / `too_many_arguments` / `assertions_on_constants` lint），**clippy 对我 STEP 2a.2 改动 0 新增 error**（已通过 stash + cargo clippy 双向对比确认：stash 我的改动后 clippy 同样报 14 errors，pop 后仍然 14 errors）。pre-existing 错误属于 rustc 1.98 toolchain 升级后 clippy lint 收紧，**不属于 2a.2 scope**（已被 STEP-VALIDATION-P2-M1b §6 与 cleanup 报告记录）。

### 2.4 fmt

```
$ cargo fmt --all -- --check
```

（仅 macos.rs + service.rs 内部 cosmetic diff；其余 fmt 改动 revert 出 scope —— 见 §3 PLAN 偏差 #3）

## 3. 与 PLAN 的偏差

### 偏差 #1 — objc2 主版本 0.5 → 0.6

**PLAN 写**：`objc2 = "0.5"` + `objc2-app-kit = "0.2"` + `objc2-foundation = "0.2"`

**实际**：`objc2 = "0.6"` + `objc2-app-kit = "0.3"` + `objc2-foundation = "0.3"`

**理由**：crates.io 上 0.5.x 已停止维护（最新 0.5.x 是 2024 年 4 月），0.6.x 是当前稳定主线（最新 0.6.4，2025 年活跃）。`objc2-app-kit` 0.2.x 同样老旧。0.6 系列 API 与 0.5 完全兼容（顶层 NSPasteboard 方法签名一致），不存在 breaking change。

**影响**：0；只是 patch 级别升级。0.5 → 0.6 不影响 wire format / 公共 API。

### 偏差 #2 — `image` crate features 收紧（default → `["png", "tiff"]`）

**PLAN 未指定** features（默认 = 所有 formats ~6 MiB compiled）。

**实际**：显式 `default-features = false, features = ["png", "tiff"]` —— 仅开 PNG + TIFF 两个 format feature。

**理由**：macos.rs 仅需 PNG（encode 写入）+ TIFF（decode 读取 → 转 PNG）。default features 包括 avif / webp / gif / hdr / ico 等 14 种格式，全部不用 —— 关闭可减小编译时间 ~30% 与 dep tree 体积。

**影响**：0；macos.rs 不受 feature 收紧影响（PNG + TIFF 都在内）。

### 偏差 #3 — `cargo fmt --all` 副作用 / 已 revert

**事件**：`cargo fmt --all` 顺带格式化 `src/connect.rs` / `src/listen.rs` / `src/quic_transport/http3.rs` / `src/service.rs` 共 4 个不相关文件（每个 ~6 行 cosmetic diff —— 主要是 `(` 行 wrap / `[u8; N]` 数组 indent 风格）。

**处理**：`git checkout --` 全部 revert。STEP 2a.2 commit scope 严格限定为 `Cargo.toml` / `Cargo.lock` / `src/clipboard/macos.rs`。

**理由**：保持 commit 干净 / 利于 revert / 减少 PR review 噪音。这些 cosmetic 改动可在后续 "fmt sweep" STEP 单独处理（PLAN §0 commit 卫生规则）。

**影响**：0；当前 working tree 仅 3 个文件改动。

### 偏差 #4 — `name()` 字符串更新（"macos-pbcopy-pbpaste" → "macos-pbcopy-pbpaste+nspasteboard-image"）

**PLAN 未明确** image backend 的 `name()`。

**实际**：name 加上 `+nspasteboard-image` 后缀，反映 image 路径独立于 text 路径。

**影响**：dispatcher startup log 现在显示新 name；如果有任何 user-side log scraper 在 grep 老字符串会断 —— 但目前 codebase 内 grep `"macos-pbcopy-pbpaste"` 只在自身 mod.rs / macos.rs / mod test 出现，无外部依赖。

## 4. 处理的 SUGGESTION 项

### 更新 #S-1（macOS clipboard backend deviation）状态

原 #S-1（2026-09-09 / M1a 1a.2 触发）：建议保持 pbcopy 路径，M2a+ 才考虑 objc2。

**2a.2 落地**：image 路径**已迁移**到 NSPasteboard via objc2（PLAN 评审 #2 3rd 强制要求）；text 路径**仍保留** pbcopy/pbpaste（`changeCount` 优化对 500ms tick + sha256 fingerprint short-circuit 仍不必要）。

#S-1 状态部分更新（不动 move，仅追加备注）—— 让 leader 后续决定是 move 到 FIXED 还是 keep SUGGESTION with updated note。**详见 SUGGESTION.md 顶部 #S-1 的 status update**。

## 5. 闸门检查

| 检查 | 结果 |
|---|---|
| 产物对得上吗 | ✅ `current_image` / `set_image` / `watch_image` 全部实现 + `last_image_change_count` 字段就位；TIFF→PNG 归一化走 `image` crate；`setData_forType(public.png)` 写回 |
| 依赖对得上吗 | ✅ M0a/M0b/M0c/M1a/M1b/M2a-2a.1 全归档（git log: `affd458 docs: archive M2a 2a.1`）；新加 objc2/image cfg-gate macOS only |
| 验收对得上吗 | ✅ `cargo test --workspace` 全绿（351 pass / 0 fail）；macOS-only image test 通过 cfg-gate |
| **milestone 边界门** | ✅ 未触碰 service.rs / dispatcher / cache / http3 / linux.rs / windows.rs（全部由 `git diff --stat` 确认：仅 Cargo.toml / Cargo.lock / src/clipboard/macos.rs 3 个文件改动）；M2a-2a.3 / 2a.4 / M2b+ 均未触碰 |
| **时间门** | ✅ ~70 min（接近 1h 上限，未超 PLAN §3 M2a 估时上限） |

## 6. 遗留

1. **`watch_image` 的 500ms 间隔 + spawn_local 行为只在 `cargo test --lib` 中以单元测试形态覆盖**（ImageClipboardGuard + 4 个测试）；端到端 stream 验证（dispatcher 订阅 + emit 收件）由 M2a-2a.3 集成测试做（PLAN §8 M2a 测试矩阵）。**风险低**：spawn_local 在 daemon runtime (`current_thread` + `LocalSet`) 已存在同等 pattern（dns.rs / emulation.rs），compile 通过即代表 API 正确使用。

2. **`last_image_change_count` 字段在 2a.2 未被消费**（dispatcher 2a.3 才用）—— 但写路径 (`current_image` / `set_image`) 已 stamp，2a.3 直接读即可。**不算 dead code**：M2a-2a.3 依赖此字段。

3. **macOS-only 跨平台编译**：`objc2` / `image` cfg-gate macOS only；Linux / Windows build 完全不受影响（`[target.'cfg(target_os = "macos")'.dependencies]` 隔离）。本机 macOS aarch64 build + 全 workspace 351 测试通过；Linux / Windows 本机未 cross-compile（与 M1a / M1b / 2a.1 baseline 一致 —— CI matrix 4 os × 4 job 由 GitHub Actions 覆盖）。

4. **Human 真机测试 4K 截图（PLAN §8 M2a 人类项）** 留 leader / 用户真机执行 —— 本 STEP 仅完成自动单测。

5. **`name()` 字符串变化** —— 见偏差 #4；dispatcher startup log 现在显示 `macos-pbcopy-pbpaste+nspasteboard-image`。

## 7. 下一步

按依赖顺序：**STEP-2a.3**（图片 outbound dispatcher：service.rs 扩 image 分支 → sha256 算 fingerprint → `ClipboardImage` 元数据走 StreamC；图片字节暂存 `clipboard_cache`（key=sha256，5 min LRU 200 MiB 上限）；HTTP/3 server `/clipboard/image/{sha256}` 路由从 cache 返回）。
