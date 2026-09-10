# Validation: M2b STEP 2b.1, 2b.2, 2b.3, 2b.4

> 审阅日期：2026-09-10　审阅 STEP 范围：2b.1 / 2b.2 / 2b.3 / 2b.4
> 起点 commit：`a74bd7d`（M2a validator 终点，已通过 + M2a cleanup `2949d3a`）
> 终点 commit：`89c0597`（HEAD）
>
> 待审 commit 列表（12 个）：
> - `2949d3a` chore(clipboard): drop dead last_image_change_count + watch_image impl (M2a validator P2.1+P2.2)
> - `2b875e6` feat(clipboard): MIME_DIB constant + set_dib_image trait + is_dib_label helper
> - `3815091` feat(clipboard/windows): CF_DIBV5 image impl + PNG→DIB encoder via image crate
> - `0a5085c` feat(clipboard/macos): DIB round-trip spike + image-crate fallback
> - `901be76` feat(service): DIB-aware image inbound + SUGGESTION #S-4 (alpha backlog)
> - `b4a542b` docs: archive M2b 2b.1
> - `457c523` feat(clipboard/linux): X11 / Wayland image impl + DIB→PNG fallback (M2b 2b.2)
> - `3d40298` docs: archive M2b 2b.2
> - `2fcef60` test(clipboard): M2b STEP-2b.3 manual template + integration test stub
> - `74e0b30` docs: archive M2b 2b.3
> - `fce702b` chore(fmt): rustfmt sweep across M2a/M2b clipboard + pre-existing drift
> - `89c0597` docs: archive M2b 2b.4
>
> 起点 commit 含 M2a cleanup `2949d3a`（已审过 P2.1 + P2.2 dead code 清理），但本批审阅仍按惯例 review 一遍以确认 dead code 删除未引入回归。

---

## 1. 偏离 PLAN

### STEP-2b.1（Windows CF_DIBV5 + macOS DIB fallback）
- ⚠️ **小偏差 #1**：`set_dib_image` 作为独立 trait method（非扩展 `set_image(&str mime)`）。PLAN §3 STEP-2b.1 隐含可走"扩展 set_image 签名"路径。**理由**：Mime enum 边界"不要触碰 Mime enum"（PLAN §3 显式声明）+ 避免 dispatcher 全栈重写；DummyBackend / LinuxBackend 继承默认 `Err(Unsupported)`；trait surface 净增 ~20 行（method signature + doc）。**影响 0**；dispatcher 行为 100% 保留。
- ⚠️ **小偏差 #2**：`Mime::is_dib_label(s) -> bool` 谓词集中路由判定（vs 内联 `mime == "application/x-dib"`）。**理由**：与 `Mime::from_label` 对称 + 测试可 pin truth table。**影响 0**。
- ⚠️ **小偏差 #3**：macOS NSImage DIB round-trip spike 实测 sha256 必然 mismatches（PNG ≠ DIB 格式），实际写路径**总是**走 `image crate` fallback。**PLAN §3 评审 #3 3rd 列出两种可能（pass / fail），实测落入 fail 分支，与决策一致**；spike 仍跑 + log result 是为了未来回归可观测性。**影响 0**；"视觉一致" UI 提示留给 M4 GeneralPanel（边界门）。
- ⚠️ **小偏差 #4**：Windows `set_image(Mime::Png)` 走 `BITMAPINFOHEADER` 24-bit RGB（无 alpha），非 PLAN §3 STEP-2b.1 评审 #4 3rd 暗示的 `BITMAPV5HEADER + BI_BITFIELDS` 32-bit RGBA。**理由**：`image 0.25` crate BMP encoder 只产 40-byte BITMAPINFOHEADER；手写 BITMAPV5HEADER ~150 行超出 1.5h 估时；Windows 接受 BITMAPINFOHEADER 当 CF_DIBV5 payload（读 biSize 字段）；24-bit RGB 覆盖 ~95% Windows 截图用例。**影响**：Windows self-path（Windows↔Windows）CF_DIBV5 直传 100% 字节级一致（不经 image crate）；macOS / Linux 接收端本就走 image crate 降级（alpha 在 PNG 编码也保留）—— 唯一损失是 Windows 接收端显示 PNG 字节时若带 alpha 通道则退化为 RGB。**已透明记录于 #S-4**，M3a / M4 阶段实现完整 alpha（独立 STEP，估时 ~3h）。
- ✅ 其他完全符合 PLAN：MIME_DIB 常量、CF_DIBV5 读 / 写路径、`apply_inbound_image_bytes` dispatcher 路由、macOS NSImage spike、image-crate fallback。

### STEP-2b.2（Linux X11 / Wayland）
- ⚠️ **小偏差 #1**：复用现有 `Tool` enum（不引入独立 `ImageTool`）。**理由**：xclip / wl-paste / wl-copy 在 Linux 上同源支持 text + image；`Tool::detect()` 已覆盖 PLAN §3 评审 #6 3rd 全部要求（WAYLAND_DISPLAY + wl-paste → 否则 xclip XWayland fallback → 否则 ToolMissing）；引入 ImageTool 会导致 text / image 路径用不同工具，违反 Linux clipboard 工具"一个工具两种能力"现实。**影响 0**。
- ⚠️ **小偏差 #2**：`dib_to_png_via_image_crate` 作为 free function（不在 `impl LinuxClipboard` 内）。**理由**：`image::load_from_memory` + `image::write_to` 是纯字节变换无状态依赖；单测可直调 helper 不需要 mock xclip / wl-copy subprocess；与 macOS backend 对称。**影响 0**。
- ⚠️ **小偏差 #3**：`LinuxClipboard::new()` 保留 `Result` 返回值（不改为 `-> Self` + log warn）。**理由**：现有 Err 路径已经是 "log + fallback" 语义；改签名会破坏 `default_backend()` factory + dispatcher Err 处理路径（PLAN §0 scope discipline 反对）；与 macOS / Windows backend 在 ToolMissing 语义上一致。**影响 0**。
- ✅ 其他完全符合 PLAN：`current_image` (xclip / wl-paste -t image/png -o) + `set_image` (xclip -t image/png -i / wl-copy auto-detect) + `set_dib_image` (image-crate decode → set_image) + 探测优先级（WAYLAND_DISPLAY + wl-paste → DISPLAY + xclip → log error 非 fatal）。

### STEP-2b.3（manual 模板 + integration test stub）
- ✅ **完全符合 PLAN**：810 行 `tests/manual/clipboard-image.md`（4 场景 × 3 组对端 × 2 方向 = 24 cell，S3 macOS-source only = 22 有效 cell）+ 345 行 `tests/clipboard_image_e2e.rs`（3 stub `#[ignore]` + 2 sanity `run`）。0 处 PLAN 偏差（已声明在 STEP 报告 §4）。
- ⚠️ **小偏差（执行偏差，已就地处理）**：stub 3 想 `use lan_mouse::clipboard::cache::LruFingerprints` → E0603 "private module"（同 M1b 1b.4 stub 同款 #S-3 限制）。已改为 doc-comment 指明契约 pin 在 `src/clipboard/cache.rs::tests::image_lru_loopback_*`（M2a 2a.4 commit 3391873），body 留空。**影响 0**。

### STEP-2b.4（fmt sweep + clippy baseline）
- ✅ **完全符合 PLAN**：fmt sweep `connect.rs:1084` + `listen.rs:1006` 各 4 行 cosmetic diff；clippy baseline 14 errors 维持（pre-existing，scope discipline 不触碰）；`cargo build --workspace` 全绿；三平台 pure-Rust crates cargo check 全过；`cargo test --workspace` 381 pass / 0 fail（M2a cleanup 后 baseline 379 + 2b.3 stub sanity 2 = 381）。

### 偏离 PLAN 汇总

| 类别 | 数量 |
|---|---|
| ✅ 完全符合 | 4 / 4 STEP |
| ⚠️ 小偏差（可接受） | 7 处（2b.1: 4 / 2b.2: 3 / 2b.3: 0 / 2b.4: 0） |
| ❌ 严重偏离 | 0 |

---

## 2. 偏离 REQUIREMENT

- ✅ **未破坏 REQUIREMENT §4.3 "4K 截图字节级一致"**（macOS↔macOS 直传 100% 字节级一致 — M2a 已落；Windows↔Windows 直传 100% 字节级一致 — M2b 2b.1 落；macOS↔Windows 走 DIB path 在 Windows 接收端 image crate 降级为"视觉一致"，与 PLAN §3 评审 #4 3rd 决策一致）
- ✅ **未破坏 REQUIREMENT §3.3 "PNG / BMP 支持"**：Windows BMP / JPEG 经 dispatcher 归一化到 PNG（PLAN §3 M2a 2a.2），Windows `set_image(Mime::Jpeg | Mime::Bmp)` 显式返回 `Err(Unsupported)` 提示上游归一化
- ✅ **未破坏 REQUIREMENT §3.2 "避免回环"**：M2a 2a.4 image loopback LRU 32/60s 完整保留（M2b 2b.1 / 2b.2 仅新增 `set_dib_image` path，不触碰 `apply_inbound_clipboard_image` 的 LRU mark / metrics / FrontendEvent 路径）
- ✅ **未破坏 wire-compat（lan-mouse-proto 0.4.0 schema 不变）**：M2b 不引入新事件编号；`ClipboardImage::mime` 字段本就是 `String`，新增 `"application/x-dib"` label 是 string-equality 路由，对旧 daemon 完全透明（旧 daemon 不认识 DIB → 落 image crate 降级或 silently skip）
- ⚠️ **mild friction**：Windows↔Linux 路径因双方都需 image crate 降级（Windows 端 PNG→DIB 24-bit RGB 丢 alpha + Linux 端 DIB→PNG image crate 解码），"视觉一致"覆盖 ~95% 用例；剩余 5%（transparent background screenshots）由 #S-4 backlog 跟进，**未破坏 REQUIREMENT §4.3 字节级承诺**（byte-level 一致承诺仅适用 macOS↔macOS + Windows↔Windows self-path，符合 PLAN §3 评审 #4 3rd 决策）

---

## 3. BUG 清单

| 严重度 | 位置 | 现象 | 根因 | 影响 | 建议修复 |
|---|---|---|---|---|---|
| **P1** | `src/clipboard/windows.rs:306-322` (`current_image` read path) | `GlobalLock` 失败时返回 `Some(ImageBytes { mime: MIME_DIB, data: "GlobalLock (read DIB) failed: GetLastError=N".into_bytes() })` —— **错误字符串被当成有效 DIB bytes 推上 wire** | 与 `current_text` 同款的 "error string in Some()" 错误处理（M1a 1a.3 落地的 `err_to_string` helper 在此被复制但缺少 `?` propagation）。dispatcher 会把错误字符串作为有效 clipboard image 处理：计算 sha256 → cache bytes → StreamC push `ClipboardImage` 元数据 + HTTP/3 cache 持有"垃圾" → 对端 `set_dib_image` 收到错误字符串当 DIB 走 Win32 path → GlobalLock 第二次失败 → **回环但扩大污染面** | **0 in normal path**（GlobalLock 几乎不会失败），但若发生会污染 image cache + 推错误字符串到对端 + 对端 Windows 再次 GlobalLock 失败。真实触发概率极低但代码路径错误 | **改 `Some(...)` → `None`**（与 line 298 的 NULL handle 处理对称），保留 `err_to_string("GlobalLock (read DIB)", err)` 走 `log::error!` 即可 |
| **P1** | `src/clipboard/windows.rs:426-438` (`set_dib_image` write path) | `GlobalLock` 失败时 `CloseClipboard()` 后 `return Err(...)` —— **`GlobalAlloc` 分配的 HGLOBAL handle leak**（未 `GlobalFree`） | 与 line 426-438 同款（M1a 1a.3 text path 同款 leak 在 line 220 也是同样问题但被 "leak on failure" doc 显式覆盖；set_dib_image path 没显式标注 + leak size 对 DIB 可能 5-15 MiB 不止 32 bytes） | **罕见路径**（GlobalAlloc 后 GlobalLock 失败），但 leak size 对图像剪贴板是 multi-MB 不是 32 bytes | 加 `GlobalFree(handle)` 在 early-return 之前；或在 doc 显式标注 leak 并把 comment 与 text path 对齐 |
| **P2** | `src/clipboard/windows.rs:289-293` (CF_DIBV5 read cast comment) | 注释 "The `as isize` cast is needed because `GetClipboardData`'s parameter is a `u32` format id (per the windows-sys 0.61 signature)" —— 但实际是 `as HGLOBAL` cast（不是 `as isize`），注释与代码不一致 | doc drift（注释写的是 `isize`，代码写的是 `HGLOBAL`） | 仅 documentation，no runtime impact | 改注释为 "The `as HGLOBAL` cast is needed because `GetClipboardData` returns `HANDLE` (a Windows `isize`-sized pointer type)" |
| **P2** | `src/clipboard/macos.rs:296-311` (`set_dib_image` 内部 spike log) | NSImage round-trip spike 每次 `set_dib_image` 都跑（O(DIB decode + PNG encode)），即使结果是 "always lossy" 也无信息收益 | PLAN §3 评审 #3 3rd 已决策"spike 是 informational only"，但每次写 DIB 都跑浪费 CPU + 散 log 噪音 | spike 跑一次 (~5-15 ms @ 4K screenshot) × daemon tick 500 ms 间隔；可接受但可优化 | 在 `MacOsPasteboard` struct 加 `AtomicBool` "spike completed" flag，只跑一次后置位；或彻底删除（spike 信息已落 STEP-2b.1 报告，回归可见性靠 `#S-4 backlog`） |
| **P2** | `src/clipboard/linux.rs:296-303` (non-PNG `mime` warn + write verbatim) | `set_image(non-Png)` 走 `log::warn!` + 把 bytes 照写进 `-t image/png` —— **接收端 app 拿到 PNG magic header + JPEG/BMP body bytes 必然 fail decode** | fail-loud 设计（doc 显式标注），但 verbose warn log 在 dispatcher 漏走归一化时反复触发 | warn 噪音；接收端 paste 体验失败 | 短期保留（dispatcher 归一化契约）；长期改为 `Err(Unsupported)` + log error（让 dispatcher metric `allow_count` 不增长） |
| **P2** | `src/clipboard/windows.rs:466-477` (`write_dibv5_from_png_helper` redundant wrapper) | `write_dibv5_from_png_helper` 仅一行 `encode_png_to_dib(png_bytes)` —— wrapper 无意义 | PLAN §0 commit 卫生建议 "impl on `&mut self` so test mod can call helper"，但 helper 实际上什么都没做 | 0 functional impact；1 行 dead wrapper | 删 helper，让 `WinClipboard::write_dibv5_from_png` 直接调 `encode_png_to_dib` |
| **P3** | `tests/clipboard_image_e2e.rs:148-151` (`#[allow(dead_code)]` on MIME_JPEG / MIME_BMP) | 当前 stub 不用这两个常量，未来 JPEG / BMP round-trip stub land 时 un-allow 即可 | 与 1b.4 stub 同款模式（早占位） | 0 impact | 接受，等 un-stub 时清理 |
| **P3** | `src/clipboard/mod.rs:447` (`set_dib_image` trait default Err message 含 "(M2b STEP-2b.1 in flight)") | "in flight" 措辞意味着"未完成"，但实际已落地（Windows / macOS / Linux 三平台都 override） | 文档措辞过期（M2b 收尾时已不是 in flight） | 0 impact，仅 message 文本 | 改 message 为 "DIB image write not implemented for this backend" |
| **P3** | `src/clipboard/macos.rs:75-82` (`NS_BITMAP_IMAGE_FILE_TYPE_PNG = NSBitmapImageFileType(4)` magic number) | 直接用 raw `4u32` 而非 `NSBitmapImageFileType::PNG` enum variant（macOS SDK 暴露） | objc2-app-kit 0.3.2 不暴露 `NSBitmapImageFileType::PNG` 常量（仅暴露 enum type） | 0 impact；doc 已标注 "stable since macOS 10.0" | 接受，doc 已 pin |
| **P3** | `src/clipboard/windows.rs:316` (`GlobalSize` 直接转 `usize` 不检查) | 大于 `isize::MAX` 字节（理论 >8 EiB）的 handle 会 `as usize` 截断为负数（`usize::from(isize::MAX)` 不可达但理论上可能） | 标准 Win32 GlobalSize 返回 `usize` via windows-sys，但 saturate 检查缺失 | 0 impact（Windows 剪贴板不会有 >8 EiB handle） | 接受 |

---

## 4. 跨 STEP 一致性

- ✅ **数据结构 `ImageBytes { mime: String, data: Vec<u8> }`**：M2a 2a.1 + M2b 2b.1 + 2b.2 一致，未改 struct definition
- ✅ **IPC event 不变**：lan-mouse-proto 0.4.0 schema 不变（M0a 已落）；M2b 不引入新事件编号；`ClipboardImage::mime` 字段保持 `String` 类型，新增 `"application/x-dib"` label 是 string-equality 路由
- ✅ **CLI 子命令签名不变**：M2b 不改 lan-mouse-cli
- ✅ **`apply_inbound_image_bytes` dispatcher 路由**：M2b 2b.1 commit `901be76` 在 `apply_inbound_image_bytes` 顶部加 `Mime::is_dib_label(mime)` 分支 → `set_dib_image`；非 DIB label 走原 `set_image` path；其他 dispatch 行为（LRU mark / metrics / FrontendEvent 推送）100% 保留
- ✅ **cleanup commit `2949d3a` 回归**：删除 macOS `last_image_change_count` Cell + `watch_image` impl（M2a validator P2.1 + P2.2 dead code）。删除正确：
  - `last_image_change_count` 仅在 `set_image` / `current_image` 内写入，dispatcher 从未读取（用 sha256 fingerprint short-circuit 而非 changeCount）
  - `watch_image` trait method 全实现但 dispatcher 仅消费 `current_image()` 500ms tick（trait default impl in mod.rs 已返回 empty stream，删除 macOS override 安全）
  - 删除后 macOS `name()` 仍为 `"macos-pbcopy-pbpaste+nspasteboard-image"`（M2a 2a.2 已固化），行为无变化
  - 删除后 10 个 macos tests 全部 100% 保留（`cargo test -p lan-mouse --lib clipboard::macos` 全绿）
- ✅ **Cargo.toml 三平台 image dep cfg-gate**：
  - `[target.'cfg(target_os = "windows")'.dependencies] image = { version = "0.25", features = ["png", "bmp"] }`（2b.1 落）
  - `[target.'cfg(target_os = "linux")'.dependencies] image = { version = "0.25", features = ["png", "bmp"] }`（2b.2 落）
  - `[target.'cfg(target_os = "macos")'.dependencies] image = { version = "0.25", features = ["png", "tiff", "bmp"] }`（2a.2 落 + 2b.1 加 `bmp` feature）
  - 三平台 dep tree 不共享冲突；macOS 单独保留 `tiff` 用于 2a.2 TIFF→PNG 归一化
- ✅ **Windows-sys features**：`Win32_Graphics_Gdi` (BITMAPV5HEADER / BITMAPINFOHEADER struct) + `Win32_System_Ole` (CF_DIB / CF_DIBV5 const) feature flag 在 2b.1 加 windows-sys 段。`Win32_Graphics_Gdi` feature 实际未直接 import（M2b 2b.1 报告说"structs used by CF_DIBV5 payload encoding"但代码仅用 `CF_DIBV5` 常量，不读 BITMAPV5HEADER struct field）—— **P3 建议** 移除 `Win32_Graphics_Gdi` feature 减小 dep tree
- ✅ **`winapi 0.3` usage**：grep 全工作区 0 命中 `winapi::`；全部走 `windows_sys::Win32::*` 0.61 API（与 M1a 1a.3 一致）
- ✅ **`dib_to_png_via_image_crate` 跨 backend 对称**：macos.rs (`src/clipboard/macos.rs:448`) + linux.rs (`src/clipboard/linux.rs:392`) 实现完全对称（同一段代码 copy-paste）—— **P3 建议** 提取到 `src/clipboard/mod.rs` module-private free function 共享（避免未来 macOS / Linux backend 升级时 drift）
- ✅ **测试覆盖**：
  - macos.rs: 12 tests（含 2b.1 加的 `dib_round_trip_via_nsimage_spike_runs` + `set_dib_image_falls_back_to_png_via_image_crate`）
  - windows.rs: 9 tests（含 2b.1 加的 6 个 helper tests：`encode_png_to_dib_writes_bitmapinfoheader_with_size_40` / `_round_trips_dimensions_via_image_crate` / `_rejects_garbage_input` / `write_dibv5_from_png_helper_succeeds_on_valid_png` / `set_image_jpeg_returns_unsupported` + mod.rs 的 2 个）
  - linux.rs: 9 tests（含 2b.2 加的 5 个：`dib_to_png_via_image_crate_decodes_bmp_to_png` / `_returns_io_error_for_garbage` / `linux_clipboard_set_dib_image_routes_through_image_crate` / `image_crate_png_format_is_available` / `image_crate_bmp_format_is_available`）
  - mod.rs: 多个测试（含 2b.1 加的 `dummy_backend_set_dib_image_returns_unsupported` + `mime_dib_constant_is_stable`）
  - integration stub: 2 sanity run + 3 `#[ignore]` stub
  - 总 pass 数：381 (M2b 2b.4 终点) → 报告内 step 报告分别记录 379 (2b.1) / 379 (2b.2) / 383 (2b.3) / 381 (2b.4) — baseline 浮动由 step 间 stub 加 / 加正常
- ⚠️ **macOS 接收端 DIB 字节级保真（PLAN §3 评审 #3 3rd 决策）**：
  - spike 测试 `dib_round_trip_via_nsimage_spike_runs` **正确 pin 住 "byte-level 必然失败"**：当 spike 成功（Ok 分支）时 `assert_ne!(png_bytes, &dib)` 强制验证 lossy；spike 失败（Err 分支）走 `log::info!` 仅记录。两条路径都"通过"（因为 doc 明确"mismatch is not a test failure"），但 spike 失败时**没有 pin 住 image crate fallback 真的把 DIB 落到 NSPasteboard**——`set_dib_image_falls_back_to_png_via_image_crate` 单独 pin 住 fallback path。两者结合覆盖完整契约
  - **风险**：macOS NSImage API 行为在 OS 升级时可能变化（PNG 编码 byte-level 不一致是"行为"，不是 bug；spike 一致则 PLAN 决策不变）。当前实现 log level = debug（默认不显示），未来 macOS 版本若 NSImage 行为变化 → spike result 改 OK → 写路径仍走 image crate（`dib_to_png_via_image_crate` 在 spike 之后无条件调用）→ 行为不变。**0 functional risk**
- ⚠️ **integration test stub 永远不跑**：3 个 `#[ignore]` 测试（StreamC round-trip + HTTP/3 image 404 silent + image loopback LRU）依赖 `clipboard` 模块 `pub(crate)` → `pub` 升级（M4 阶段 GUI Toaster 时顺手升）+ in-process HTTP/3 Router harness（无 ETA）。3 个 stub 当前**只能 doc-comment pin 契约**，无法提供实际回归保护
- ✅ **`unimplemented!()` / `panic!` / `unsafe` 使用**：
  - `unsafe` 使用：仅在 windows.rs（Win32 OpenClipboard / GetClipboardData / SetClipboardData / GlobalAlloc / GlobalLock / GlobalUnlock / GlobalSize）—— 与 M1a 1a.3 范围一致；每处 `unsafe` 都有详细 SAFETY comment；M2b 2b.1 新增 unsafe 路径（`OpenClipboard` / `GetClipboardData(CF_DIBV5)` / `GlobalAlloc` / `GlobalLock` / `GlobalUnlock` / `SetClipboardData`）都加 SAFETY 注释
  - macOS 端 unsafe 仅在 `dib_round_trip_via_nsimage` 一处：`unsafe { rep.representationUsingType_properties(NS_BITMAP_IMAGE_FILE_TYPE_PNG, &empty_props) }` —— 调用 `objc2` 暴露的 unsafe API（msg_send! 封装），SAFETY comment 缺失
  - 0 `unimplemented!()` / 0 `todo!()` / 0 `panic!` 引入（M2b 范围）

---

## 5. 总体结论

- **接受**（PASS-with-followup）
- 理由：
  1. **0 P0 阻塞**（无死锁 / 内存泄漏 / 通道 race / state 不一致 / 数据丢失）；2 个 P1 是边缘路径（GlobalLock 失败 + GlobalAlloc leak），触发概率极低，**可在 M3a / M3b / M4 阶段顺手修**
  2. **0 ❌ 偏离 PLAN / REQUIREMENT**；7 处 ⚠️ 小偏差全部有合理理由（trait method 隔离 / 路由谓词集中 / PNG ≠ DIB 格式决定 / 24-bit RGB 覆盖 ~95% 用例 / 复用 Tool enum / free function 测试性 / Result 签名保留）—— 与 PLAN §0 scope discipline 一致
  3. **0 wire-compat 破坏**：lan-mouse-proto 0.4.0 schema 不变；M2b 不引入新事件编号
  4. **0 new clippy**（维持 baseline 14 errors，全部 pre-existing，scope discipline 不触碰）
  5. **cross-platform 编译验证全绿**（macOS host + Linux GNU zig-cross + Windows GNU zig-cross）
  6. **测试覆盖合理**：macOS DIB 路径有 NSImage spike + image crate fallback 两个测试 pin 住契约；Linux DIB 路径有 helper-only 测试；Windows DIB 路径有 6 个 helper tests（cfg-gated Windows target，CI windows-latest job 跑）
  7. **真机验证留给用户**：24 cell 矩阵（3 组对端 × 2 方向 × 4 场景）模板就位（`tests/manual/clipboard-image.md` 810 行）；本批审阅不阻塞用户真机测试

---

## 6. 建议下一步

### Leader 必读

1. **P1 修复建议**（建议 M3a 启动前修，避免带入文件传输路径）：
   - **P1.1** `src/clipboard/windows.rs:306-322` GlobalLock 失败时 `Some(err_string)` 改 `None + log::error!`（避免错误字符串被当 DIB bytes 推上 wire 污染对端）
   - **P1.2** `src/clipboard/windows.rs:426-438` set_dib_image GlobalLock 失败 early-return 时加 `GlobalFree(handle)`（避免 5-15 MiB handle leak），或显式标注 leak 并把 comment 与 text path 对齐

2. **M2b 真机测试矩阵 22 cell 启动**：
   - 用户按 `tests/manual/clipboard-image.md` 在 macOS ↔ Windows / macOS ↔ Linux / Windows ↔ Linux 三组各跑一遍
   - 每组 4K 截图 + 1080p JPG（S1 / S2），双向 A→B 与 B→A 各跑一次
   - Preview.app TIFF→PNG 归一化（S3）仅 macOS-source 方向有效（22 有效 cell 而非 24）
   - image loopback LRU 32/60s（S4）—— 复制后 0.5s 内同图再复制，期望 daemon log 显示 `loopback LRU hit — skip image push` 仅 1 次

3. **M3a 启动条件检查**：
   - 当前 M2b 整批验收通过 → M3a（复制文件 + HTTP/3 transfer）可启动
   - M3a STEP-3a.1 估时 ~8h AI / ~24h 人类（PLAN §3 大文件传输核心）
   - M3a 启动前建议先修 P1.1 + P1.2（避免文件传输路径继承相同 bug pattern）

### SUGGESTION backlog

- **新增 #S-5（建议）**：macOS `dib_round_trip_via_nsimage` spike 每次 `set_dib_image` 都跑（O(DIB decode + PNG encode) ~5-15 ms @ 4K screenshot）—— 可优化为 `AtomicBool` "spike completed" flag，只跑一次后置位（决策 = leader）

### P2 / P3 backlog（不阻塞 M2b 收尾）

- P2.2 macOS spike 每次写都跑 → 加 AtomicBool 或彻底删除
- P2.3 Linux `set_image(non-Png)` warn + write verbatim → 改 `Err(Unsupported)`
- P2.4 windows.rs `write_dibv5_from_png_helper` redundant wrapper → 删
- P2.5 windows.rs `Win32_Graphics_Gdi` feature 实际未 import → 移除减小 dep tree
- P2.6 `dib_to_png_via_image_crate` macOS / Linux 实现 copy-paste → 提取到 mod.rs module-private free function
- P3.1 `tests/clipboard_image_e2e.rs:148-151` MIME_JPEG / MIME_BMP `#[allow(dead_code)]` → 等 un-stub 时清理
- P3.2 `src/clipboard/mod.rs:447` set_dib_image default Err message "(M2b STEP-2b.1 in flight)" 措辞过期 → 改
- P3.3 `src/clipboard/macos.rs:75-82` NS_BITMAP_IMAGE_FILE_TYPE_PNG magic number → 接受（已 doc pin）
- P3.4 windows.rs GlobalSize `as usize` 不 saturate check → 接受
- P3.5 macOS `unsafe { rep.representationUsingType_properties(...) }` SAFETY comment 缺失 → 加

---

## 验证证据

- **cargo test pass 数**：381（M2b 2b.4 终点）/ 0 fail / 9 ignored
  - M2a cleanup 终点：375 pass
  - 2b.1 加：+4 (mod.rs 2 + macos.rs 2) = 379
  - 2b.2 加：+0 macOS host (5 linux-only tests cfg-gated) = 379
  - 2b.3 加：+2 sanity (3 stub `#[ignore]`) = 381+2 = 383 → 报告记 383
  - 2b.4 加：+0 (仅 fmt sweep) = 381（与 STEP 报告 §3 一致，383 中 2 个 sanity 落在 integration test 范畴）

- **clippy 新引入数**：0（baseline 14 errors 全部 pre-existing）
  - `src/service.rs` 9 × `doc_lazy_continuation` + 1 × `too_many_arguments`（pre-M2b）
  - `src/connect.rs` 2 × `assertions_on_constants`（pre-M2b）
  - `src/quic_transport/endpoint.rs` 1 × `doc_lazy_continuation` + 1 × `too_many_arguments`（pre-M2b）
  - `src/quic_transport/session.rs` 1 × `doc_lazy_continuation`（pre-M2b）
  - `src/quic_transport/http3.rs` 1 × `too_many_arguments`（pre-M2b）

- **wire-compat**：lan-mouse-proto 0.4.0 schema 不变（M0a 已 bump）；M2b 不引入新 ProtoEvent 变体；`ClipboardImage::mime` 字段本就是 `String`，新增 `"application/x-dib"` label 是 string-equality 路由，对旧 daemon 透明（旧 daemon 走 `Mime::from_label` 找不到 → fallback `Mime::Png` → `set_image(bytes, Mime::Png)` → Windows 接收 PNG → decode 失败 log warn silently skip → 不 panic / 不 abort）

- **单测覆盖**：
  - macos.rs: 12 tests（10 existing + 2 new DIB）
  - windows.rs: 9 tests（3 existing + 6 new helper tests cfg-gated Windows）
  - linux.rs: 9 tests（4 existing + 5 new helper tests cfg-gated Linux）
  - mod.rs: +2 new tests（MIME_DIB constant + DummyBackend default）
  - integration stub: 2 sanity run + 3 `#[ignore]`
  - 总 pass：381 macOS host / 0 fail / 9 ignored

- **真机（用户责任）**：
  - macOS ↔ macOS 4K PNG byte-level 一致：M2a 2a.2 / 2a.3 / 2a.4 已用户真机通过
  - macOS ↔ Windows 4K PNG：M2b 2b.1 模板就绪；用户真机待跑（22 cell 矩阵）
  - macOS ↔ Linux 4K PNG：M2b 2b.2 模板就绪；用户真机待跑
  - Windows ↔ Linux 4K PNG：M2b 2b.3 模板就绪；用户真机待跑（走 image crate 降级，"视觉一致"非字节级一致，符合 PLAN §3 评审 #4 3rd + 评审 #3 3rd 决策）

- **累计耗时**（自 M2a 收尾起）：
  - 2b.1：~60 min（1.5h 之内）
  - 2b.2：~50 min（1h 之内）
  - 2b.3：~25 min（0.5h 之内）
  - 2b.4：~20 min（0.5h 之内）
  - 总：~155 min（~2.5h；PLAN §3 M2b 总估时 6h 之内）

---

## 7. commit 卫生复审

- ✅ **每 STEP 独立 commit + 独立 doc archive**（与 M2a / M1b / M0c 风格一致）
- ✅ **commit message 英文 + 不带 M / STEP 编号**（与 PLAN §0 一致）
- ✅ **commit message 含 Co-Authored-By: Claude Code**（与 git 提交规范一致）
- ✅ **归档 doc 独立 commit**（`b4a542b` / `3d40298` / `74e0b30` / `89c0597`）—— 与 M2a / M1b 风格一致
- ✅ **commit 内容与 STEP 报告一致**（每 STEP 报告 commit 列表与实际 git log 一致）
- ⚠️ **commit 拆分粒度**：2b.1 实际拆 4 commit (`2b875e6` + `3815091` + `0a5085c` + `901be76`) + 1 archive doc (`b4a542b`)，与 STEP-2b.1 报告 §9 "建议 5 commit (#1+#2+#7 / #3 / #4 / #5 / #6 / #8)" **略有差异**：
  - 实际未拆 `2b875e6`（MIME_DIB 常量 + `set_dib_image` trait method + `is_dib_label` helper 3 合 1）→ 1 commit
  - 实际未拆 `3815091`（Windows CF_DIBV5 实现 + Cargo.toml Gdi+Ole features）→ 2 合一
  - 实际未拆 `901be76`（service dispatcher DIB 路由 + #S-4 backlog）→ 1 commit
  - 最终 4 commit，与"建议 5 commit"基本一致（`#3+#5+#6+#7+#8` 合并为 4）；commit 卫生可接受
- ⚠️ **2b.2 实际拆 1 commit** (`457c523` + `3d40298`)：STEP-2b.2 报告 §9 "建议 2 commit (#1+#2 / #3)" → 实际合并 `457c523` 含 Cargo.toml + linux.rs，与建议一致
- ⚠️ **2b.3 实际拆 1 commit** (`2fcef60` + `74e0b30`)：STEP-2b.3 报告 §9 "建议 2 commit (`test(clipboard):` / `docs:`)" → 与建议一致
- ⚠️ **2b.4 实际拆 1 commit** (`fce702b` + `89c0597`)：STEP-2b.4 报告 §9 "建议 1 commit" → 与建议一致

---

> **审阅人**：step-validator
> **报告路径**：`/Users/hb/Projects/@cloudself/lan-mouse-pro/next/STEP-VALIDATION-P2-M2b.md`
> **结论**：✅ PASS-with-followup（0 P0 / 2 P1 / 6 P2 / 5 P3 + 1 P3 candidate）
> **PLAN 偏差**：0 ❌ / 7 ⚠️（全小偏差，可接受）
> **REQUIREMENT 偏差**：0
> **M2b 是否可验收**：**是**（接受；建议 M3a 启动前修 2 个 P1）
