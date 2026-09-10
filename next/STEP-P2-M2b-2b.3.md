# STEP-P2-M2b-2b.3 — Cross-platform image real-machine test template + integration test stub

> PLAN §3 M2b / STEP-2b.3（三平台互传矩阵 — 人类真机 + 集成测试 stub）
> 执行日期：2026-09-10　实际耗时：~25 min
> 结论：✅ 通过（manual 模板 + 集成测试 stub 就绪；0 PLAN 偏差；0 new clippy / fmt / build 错误）

---

## 1. 做了什么

按 PLAN §3 M2b STEP-2b.3 行落地两件交付：

### 1.1 真机 manual log 模板 — `tests/manual/clipboard-image.md`（810 行，新文件）

按 2a.1 / 2a.2 / 2a.3 / 2a.4 / 2b.1 / 2b.2 契约覆盖三组跨平台对端 × 四类场景：

| § | 覆盖 STEP | 场景 | 验证手段 |
|---|---|---|---|
| §1 S1 | 2a.3 image outbound + 2a.4 image inbound + 2b.1 DIB 直传 + 2b.2 Linux PNG path | 4K PNG（5-15 MiB 或 ImageMagick fixture 1-2 MiB）| `xxd \| sha256sum` 字节级一致 + daemon log `apply_inbound_clipboard_image` |
| §1 S2 | 2a.2 image trait + 2b.1 JPEG unsupported | 1080p JPG（约 100-500 KiB）| `sha256sum` 字节级一致；Windows 接收端 known unsupported（2b.1 边界）|
| §1 S3 | 2a.2 评审 #2 3rd macOS TIFF→PNG 归一化 | Preview.app 复制选中区域（TIFF only）→ 源端 image crate 强制重编码 PNG → 对端 PNG 字节级一致 | log 显示 `mime=image/tiff` 中间行 + `mime=image/png` 终态行；PNG magic `89 50 4E 47` 保留 |
| §1 S4 | 2a.4 image LRU 32/60s（vs text LRU 128/60s）| 同图片 0.5s 间隔重复复制 | A 端 `loopback LRU hit — skip image push` 1 次；`sending ClipboardImage` 仅 1 次；B 端 `apply_inbound_clipboard_image` 仅 1 次 |

| §2 对端组 | 命令映射 |
|---|---|
| §2.1 macOS ↔ Windows | `screencapture` / `osascript`（PNG + TIFF only）↔ `Snipping Tool` / PowerShell `[System.Windows.Forms.Clipboard]::GetImage()`；Windows ↔ Windows CF_DIBV5 直传 100% 字节级一致；Windows ↔ macOS DIB 走 image crate 降级（**视觉一致**而非字节级）|
| §2.2 macOS ↔ Linux | `screencapture` / `osascript` ↔ `xclip -t image/png` / `wl-copy` / `wl-paste --type image/png`；ImageMagick fixture 替代 `screencapture` 实现确定性 |
| §2.3 Windows ↔ Linux | `Snipping Tool` / PowerShell ↔ `xclip` / `wl-paste`；Windows→Linux 走 DIB→PNG（视觉一致）；Linux→Windows 走 PNG→DIB（视觉一致，#S-4）|

模板还包含：

- **§0.4 / §0.5 / §0.6** — daemon build / launch / 平台命令参考表 / 4 类图片 magic 检测（PNG `89 50 4E 47` / JPG `FF D8 FF` / BMP `42 4D` / DIB `28 00 00 00`）/ ImageMagick fixture 推荐（**避免 screencapture 鼠标光标 + 动态壁纸破坏字节级**）
- **§3 result capture template** — 24 cell 矩阵记录（3 组 × 2 方向 × 4 场景；S3 仅 macOS-source = 22 有效 cell）+ pass-criteria summary 表（按 cell 类型分别给出期望：byte-identical / visually consistent only / known unsupported / n/a）
- **§4 troubleshooting** — 5 类症状：剪贴板空 / 字节损坏 / Windows↔Linux 不一致（**期望行为** + magick identify + crop compare 命令）/ S3 TIFF 归一化路径缺失 / S4 LRU skip 缺失
- **§5 M2b milestone gate** — 静态检查 + 三平台 check + 24 真机 cell 通过清单
- **§6 out of scope** — 文件传输 / GUI Toaster / HTML+RTF 协商 / 多图同时 / clipboard history / Wayland portal 限制 / MSVC target

模板使用 1b.4 + 2b.1 / 2b.2 同样的"复制粘贴即可跑"风格：每条命令 + 期望 daemon log 行 + 期望 B 端输出 + 通过标志全部明确列出。模板使用 unicode 符号（— em dash / → arrow / § section sign）保持 markdown 美观（无 `cargo fmt` 影响，md 不参与 rustfmt）。

### 1.2 集成测试 stub — `tests/clipboard_image_e2e.rs`（345 行，新文件）

5 个测试（`cargo test --workspace` 跑 2 + ignore 3）：

| 测试 | 状态 | 覆盖 |
|---|---|---|
| `four_k_screenshot_metadata_round_trips_via_stream_c` | `#[ignore]`（需要 in-process StreamC harness）| 2a.3 `ClipboardImage` 8 MiB PNG → wire 始终 < 1 KiB（metadata-only，image bytes 走 HTTP/3 pull）；codec round-trip |
| `http3_image_cache_miss_returns_404_silently` | `#[ignore]`（需要 in-process HTTP/3 Router harness）| 2a.3 reviewer #3 2nd：`Response::with_status(404, vec![])` + dispatcher `(status, body.len())` match 走 silent-skip 分支 |
| `four_k_screenshot_does_not_loop_back_within_image_lru_ttl` | `#[ignore]`（`src/clipboard/cache.rs` 是 `pub(crate)`）| 2a.4 image LRU 32/60s 契约 pin 在 unit-test 层（`src/clipboard/cache.rs::tests::image_lru_loopback_*`），注释指明 |
| `clipboard_image_png_codec_round_trip` | **run** ✓ | PNG mime `ClipboardImage` codec round-trip sanity（pin var codec wire layout）|
| `clipboard_image_dib_wire_label_round_trip` | **run** ✓ | `MIME_DIB = "application/x-dib"` 字符串 verbatim 保留（pin 路由谓词 constant）+ `MIME_DIB` 常量值锁 |

`#[ignore]` 三个 stub 在 `cargo test --workspace` 不跑，需要时 `cargo test --test clipboard_image_e2e -- --ignored` 触发。已验证 `--ignored` 三个 stub 全绿（OK 0.49s）。

两个 sanity 测试每次 `cargo test --workspace` 都跑 — 防止 stub 文件"100% ignored 假装在跑"。

---

## 2. 关键设计

### 2.1 模板 + stub 双轨（不替换既有单测）

按 STEP-2b.3 prompt "本 STEP 仅创建模板 + 占位" + "1b.4 + 2b.1 / 2b.2 末段真机测试由用户跑"：

- **真机场景**（4 类 × 3 组 × 2 方向 = 24 cell；S3 macOS-source only = 22 有效 cell）→ 模板指引人类在 `tests/manual/clipboard-image.md` 上打勾，不跑任何 LAN 测试代码
- **integration stub** → 2b.3 末段已落代码（2a.1 image trait + 2a.3 image outbound + 2a.4 image inbound LRU + 2b.1 DIB + 2b.2 Linux PNG）的契约被 pin 在文件里，但 `#[ignore]` 不参与 `cargo test`；为 M2b 末段 / 后续 milestone 提供"先占位再接 seam"的承载点

### 2.2 `src/clipboard` 是 `pub(crate)` → 集成测试够不到 cache

与 1b.4 stub 同款约束（E0603 "module `clipboard` is private"）。第三个 stub 测试 body 留空（仅 doc-comment 指明契约 pin 在 `src/clipboard/cache.rs::tests::image_lru_loopback_*`）。

### 2.3 `Vec::<u8>::from(event.clone())` 而不是 `(&event).into()`

与 1b.4 stub 同款（`lan_mouse_proto` 只实现 `From<ProtoEvent> for Vec<u8>`，**不**实现 `From<&ProtoEvent>`）。stub 跟随现有约定。

### 2.4 wire payload size pin — image metadata 始终 < 1 KiB

`ClipboardImage` wire layout：`[u8; 32 fingerprint][u32 BE mime_len][mime bytes][u8; 32 sha256][u64 BE size]`（来自 `lan-mouse-proto/src/codec.rs::ClipboardImage::encode_var_body`）。**总 wire size ≈ 32 + 4 + len(mime) + 32 + 8 = 76 + len(mime) bytes** — well under 1 KiB regardless of image size。这 pin 了 2a.3 设计决策："图片字节永远走 HTTP/3 pull，绝不 inline 进 StreamC Meta"。

`four_k_screenshot_metadata_round_trips_via_stream_c` 测试断言 `wire.len() < 1024` — 防止未来有人尝试 inline image bytes 进 StreamC Meta（会瞬间打爆 StreamC 缓冲 + 阻塞 StreamA/B 键鼠事件）。

### 2.5 `MIME_PNG` / `MIME_JPEG` / `MIME_BMP` / `MIME_DIB` 常量 `#[allow(dead_code)]`

stub 文件定义 4 个 wire-level MIME label 常量（与 `src/clipboard/mod.rs::MIME_DIB` 对称）。当前只有 `MIME_PNG` + `MIME_DIB` 被 sanity 测试用到；`MIME_JPEG` + `MIME_BMP` 留作未来 un-stub 扩展（M2b+ 阶段 JPEG / BMP round-trip stub land 时使用）。`#[allow(dead_code)]` 抑制 rustc 警告但保留 doc-comment 解释为何保留。

### 2.6 rustfmt 警告消除

第一版用 `assert_eq!(ci.mime, MIME_DIB, "...")` 单行 → rustfmt 拆分 assertion arguments 时改为 multi-line block。stub 文件重新 fmt-clean。

---

## 3. 验证结果

- `cargo build --workspace --tests`：✅ 通过（0 error 0 warning；2 dead_code 警告用 `#[allow(dead_code)]` 抑制）
- `cargo test --workspace`：**383 passed / 0 failed / 9 ignored**（2b.2 baseline 381 + 2 new sanity = 383）
- `cargo test --test clipboard_image_e2e`：✅ 2 passed / 3 ignored（sanity tests）
- `cargo test --test clipboard_image_e2e -- --ignored`：✅ 3 passed / 0 ignored（stub tests 全绿 0.49s）
- `cargo fmt -p lan-mouse -- tests/clipboard_image_e2e.rs --check`：✅ 0 diff
- `cargo clippy --workspace --all-targets -- -D warnings`：**14 errors**（baseline 14 = pre-existing；**2b.3 引入 0**；与 2b.2 同口径）

**测试细分**：
| crate | tests | 备注 |
|---|---|---|
| `input-capture` | 101 | 无变化 |
| `lan-mouse` (lib) | 212 | 无变化（M2b 2b.2 终点）|
| `clipboard_text_e2e` | 2 passed / 3 ignored | 无变化（M1b 1b.4 既有）|
| `clipboard_image_e2e` (新) | **2 passed / 3 ignored** | 2b.3 贡献 |
| `input_emulation` | 2 passed / 3 ignored | 无变化 |
| `lan-mouse-ipc` | 7 | 无变化 |
| `capture_test` | 2 | 无变化 |
| `lan-mouse-cli` | 26 | 无变化 |
| `lan-mouse-proto` | 29 | 无变化 |
| **合计 run** | **383** | 0 failed |
| doc-tests | 0 | 7 个 doc-test target 全 0 测试，0 错 |

**clippy baseline（pre-existing，2b.3 未触及）**：
- `src/connect.rs`:1146/1147（doc_lazy_continuation）/ 1702/1708（assertions_on_constants）
- `src/quic_transport/endpoint.rs`:238（doc_lazy_continuation）/ 339（too_many_arguments）
- `src/quic_transport/session.rs`:931（doc_lazy_continuation）
- `src/service.rs`:109-113（incoming_clipboard 字段 doc 的 doc_lazy_continuation，pre-2b.3 代码）
- `src/quic_transport/http3.rs`：handle_http3_stream 函数 8 参数（too_many_arguments；2b.3 模板文档引用此函数，clippy 计数包含）

**未触发任何 2b.3 相关 clippy warning**：模板文件无 Rust 代码（不参与 clippy）；stub 文件 doc-comment 干净。

---

## 4. 与 PLAN 的偏差

**0 处 PLAN 偏差**：2b.3 完整落地了 PLAN §3 M2b STEP-2b.3 行的全部要素：

| PLAN 要求 | 落地位置 |
|---|---|
| 创建 `tests/manual/clipboard-image.md` 模板 | ✅ `tests/manual/clipboard-image.md`（810 行）|
| 模板覆盖三组跨平台对端 × 四类场景 | ✅ §2.1 / §2.2 / §2.3 + §1 S1 / S2 / S3 / S4 |
| 24-cell 矩阵（6 trial × 4 场景 = 24 cell）| ✅ §2 三组对端 × §1 四场景 = 24 cell matrix |
| 加 1-2 条集成测试占位（mock StreamC + HTTP/3 image pipeline）| ✅ `tests/clipboard_image_e2e.rs`（5 tests: 3 stub `#[ignore]` + 2 sanity `run`）|
| **不要求真跑** | ✅ 全部 stub `#[ignore]`；2 个 sanity 只用 codec 层验证 |
| 模板最后列 M2b 里程碑收尾闸门 | ✅ §5 M2b milestone gate 完整列出 `cargo build/test/clippy/fmt` + 三平台 check + 24 cell |

**1 处执行偏差（已就地处理）**：原本 stub 3 想 `use lan_mouse::clipboard::cache::LruFingerprints;` 直接 pin image LRU 32/60s 契约 → E0603 "private module"。改成 doc-comment 指明契约 pin 在 `src/clipboard/cache.rs::tests::image_lru_loopback_*`（M2a STEP-2a.4 commit 3391873），body 留空，**0 行为 / 0 计划** 变化。SUGGESTION #S-3 已在 SUGGESTION.md 中追踪（M4 阶段 GUI Toaster 时升级 `pub(crate)` → `pub`）。

---

## 5. 处理的 SUGGESTION 项

无新增。本 STEP 不修改 dispatcher / cache / http3 / service 代码路径；现有 SUGGESTION #S-1（macOS backend）/ #S-2（Windows + Linux 跨平台）/ #S-3（pub(crate)）/ #S-4（Windows CF_DIBV5 alpha limitation）均未触及。

**模板显式处理 #S-4**：§1 S1 / S4 / §2.1 / §2.3 / §3 / §4 明确标注"DIB → image crate 降级路径是 **视觉一致** 而非字节级一致"——按 PLAN §3 评审 #4 3rd + 评审 #3 3rd 决策，不算 fail。

---

## 6. 闸门检查

- **闸 1（执行前）**：产物 ✅（模板 + stub 路径都已识别）；依赖 ✅（2a.1 / 2a.2 / 2a.3 / 2a.4 / 2b.1 / 2b.2 全部归档为"通过"，git log 验证）；验收 ✅（`cargo build/test` 就绪）；milestone 边界 ✅（仅 M2b 内，`tests/manual/` + `tests/` 占位，不动 dispatcher / cache / http3 / backend）；时间门 ✅（~25 min < 30 min 目标）
- **闸 2（执行中）**：clippy 1 次失败（`assert_eq!` 单行被 rustfmt 拆为 multi-line）→ 就地改写为 multi-line block；dead_code 2 个警告 → `#[allow(dead_code)]` 抑制 + doc-comment 解释为何保留；编译 0 次失败；**0 次**触发 plan-deviation 报告
- **闸 3（milestone 收尾，本次非收尾）**：跳过；2b.3 是 M2b 末段（无后续 2b.4 单独 STEP 之外），M2b 收尾跑全套待人类真机测试完成后（2b.4 fmt/clippy/build + 三平台编译验证）

---

## 7. 遗留 / 下一步

### 7.1 2b.3 自身遗留

- **`src/clipboard` 模块仍是 `pub(crate)`**（E0603 触发源）。M4 阶段 GUI Toaster 需要 `clipboard::Backend` 公开 API 时顺手升级 `pub(crate)` → `pub`，再 un-stub `four_k_screenshot_does_not_loop_back_within_image_lru_ttl`。**不在 2b.3 范围**（PLAN §0 Out of Scope）。
- **stub 测试需要 in-process harness**：当 `PeerSession` 暴露 `send_stream_c` + clipboard image 的 test seam 时，可 un-stub `four_k_screenshot_metadata_round_trips_via_stream_c` + `http3_image_cache_miss_returns_404_silently`。属于 "2b.x followup" / M3a 范围。
- **Cargo.toml pre-existing fmt drift**（`src/connect.rs:1084` / `src/listen.rs:1006` 各 4 行 cosmetic diff）与 2a.2 / 2a.3 / 2a.4 / 2b.1 / 2b.2 偏差 #3 / #5 同源 —— 与本 STEP 无关；统一 sweep 留给 2b.4。

### 7.2 下一步

1. **人类真机测试**：按 `tests/manual/clipboard-image.md` 在 macOS ↔ Windows / macOS ↔ Linux / Windows ↔ Linux 三组各跑一遍（4 场景 × 2 方向 = 8 trial / group = 24 cells total；S3 macOS-source only = 22 有效 cell）。结果记录在 §3 result capture template。
2. **M2b 收尾（人类真机全通后）**：
   - `cargo build --workspace`
   - `cargo test --workspace`（预期 383 + 任何后续 hot-fix 增量）
   - `cargo clippy --workspace --all-targets -- -D warnings`（预期 14 errors baseline 不变）
   - `cargo fmt --check`（2b.4 任务：fmt sweep pre-existing connect.rs / listen.rs drift）
   - leader 触发 step-validator 整批审（M0a / M0b / M0c / M1a / M1b / M2a / M2b 七批 + post-M1a hot-fix）
   - commit + push
3. **M3a 启动**：复制文件 + HTTP/3 transfer（PLAN §3 M3a STEP-3a.1 / 3a.2 / 3a.3 / 3a.4 / 3a.5）。

---

## 8. 文件改动

| 文件 | 改动类型 | 备注 |
|---|---|---|
| `tests/manual/clipboard-image.md` | 新建 | 810 行真机 manual 模板（§0 前置 / §1 S1-S4 四场景 / §2.1-2.3 三组对端 / §3 24 行结果记录 / §4 troubleshooting / §5 milestone gate / §6 out of scope）|
| `tests/clipboard_image_e2e.rs` | 新建 | 345 行 integration test stub（5 tests: 3 stub `#[ignore]` + 2 sanity `run`；sha256 helper；wire codec 验证 + MIME 常量 pin）|

**未触碰**（scope 外）：
- `src/service.rs` / `src/clipboard/*`（2b.1 / 2b.2 终态不变）
- `src/quic_transport/*`（M0a / M0b / M0c / 2a.3 终态不变）
- `lan-mouse-proto` / `lan-mouse-ipc` / `lan-mouse-vue`（无 proto bump / IPC 扩展 / 前端变更）
- `Cargo.toml`（无依赖变更）
- `next/.LEADER-STATE.md`（leader 维护，2b.3 不直接更新）

---

## 9. commit 拆分建议（leader 决策）

按 PLAN §0 commit 卫生分拆（3 拆 → 实际可 2 拆合并）：

```
1. docs(tests): add cross-platform clipboard image manual test template (M2b STEP-2b.3 §1)
   - tests/manual/clipboard-image.md (new, 810 lines)
   - Covers: macOS ↔ Windows / macOS ↔ Linux / Windows ↔ Linux × S1-S4 (4K PNG / 1080p JPG / Preview.app TIFF→PNG / image loopback)

2. test(clipboard): add integration test stub for image StreamC + HTTP/3 cache-miss (M2b STEP-2b.3 §2)
   - tests/clipboard_image_e2e.rs (new, 345 lines)
   - 3 #[ignore] stubs (image StreamC round-trip + HTTP/3 image 404 silent + image LRU loopback)
   - 2 sanity tests that run on every cargo test --workspace (PNG codec + DIB wire label pin)
```

可合并为：
```
1. test(clipboard): M2b STEP-2b.3 manual template + integration test stub
   - tests/manual/clipboard-image.md
   - tests/clipboard_image_e2e.rs
```

报告本身（`next/STEP-P2-M2b-2b.3.md`）作为第二个 commit（`docs:`）。

最终建议 2 commit（`test(clipboard): ...` / `docs: archive STEP-P2-M2b-2b.3`）。

---

## 10. 累计耗时

~25 min：
- ~10 min 设计 manual 模板结构（参考 1b.4 + 2a.4 + 2b.1 / 2b.2 风格 + PLAN §3 M2b STEP-2b.3 要求 + 4 场景 × 3 组对端的双向矩阵）
- ~10 min 写 stub + 调试 2 处初版问题（dead_code 警告 + rustfmt multi-line assert）+ doc-comment 编辑
- ~5 min 验证 build/test/clippy/fmt + 报告

---

## 11. 执行备注（leader）

- **模板优先于 stub**：用户真机测试用模板 → 模板直接决定 stub 测试"contract pin"的准确性。先设计 §1 S1-S4 流程后再写 stub 三个 `#[ignore]` 测试的 doc-comment。
- **stub 不连真实 peer**：按 prompt "集成测试 stub 不连真实 peer"。`#[ignore]` 是关键，不允许"测试看起来跑了但实际打洞网络"。
- **commit 拆 2 还是 3**：模板与 stub 是同一 STEP 的两份交付，逻辑上独立但同步发布；建议 2 拆（`test(clipboard):` 一笔 + `docs:` 报告一笔）。
- **模板 ImageMagick fixture 推荐**：PLAN §3 隐含假设"用 screencapture 截屏"，但 `screencapture` 含鼠标光标 + 动态壁纸破坏字节级一致。模板 §0.6 显式推荐 ImageMagick `magick gradient: /tmp/4k.png` 作为 canonical byte-level fixture，保留 `screencapture` 作为 single human-judgement run per platform。
- **24-cell vs 22-effective cell**：模板矩阵标 24 cell（3 组 × 2 方向 × 4 场景），但 S3 Preview.app TIFF 归一化仅在 macOS-source 方向有效（TIFF 是 macOS-only 来源）→ 实际填写时为 22 有效 cell。模板 §2 表格 + §3 pass-criteria summary 显式标注"n/a (macOS-source only)"处理 B→A 方向的 S3 cell。
- **#S-4 alpha limitation 透明记录**：模板多处显式说明"DIB → image crate 降级路径是 **视觉一致** 而非字节级一致"，并提供 `magick identify` + `magick compare` 命令帮助人类确认"visually consistent"。不算 fail。
- **M2b 收尾待人类真机**：22 有效 cell 全通后 leader 跑全套静态检查 + 触发 step-validator + 2b.4 fmt sweep pre-existing drift。
- **#S-4 状态更新**：模板 §1 / §2 / §4 显式说明 Linux BMP decoder RGBA masks limitation —— 与 #S-4 同源，**不**新增 SUGGESTION 条目（已在 #S-4 跟踪）。

---

> **执行人**：plan-step-executor
> **报告路径**：`/Users/hb/Projects/@cloudself/lan-mouse-pro/next/STEP-P2-M2b-2b.3.md`
> **cargo test --workspace pass 数**：383 / 0 fail（baseline 381 + 2 new）
> **PLAN 偏差**：0
> **限制 / 已知问题**：模板显式处理 #S-4 alpha limitation（视觉一致而非字节级）
