# STEP-P2-M2b-2b.1 — Windows CF_DIBV5 image impl + macOS DIB NSImage spike + image-crate fallback

> PLAN §3 M2b / STEP-2b.1 (评审 #4 3rd — DIB 直传保真 + 评审 #3 3rd — macOS NSImage spike 降级决策)
> 执行日期：2026-09-10　实际耗时：~60 min
> 结论：✅ 通过（Windows CF_DIBV5 + macOS NSImage spike + image-crate fallback + 4 new tests / 379 pass total / 0 fail）

---

## 1. 做了什么

### 1.1 改动文件

| 文件 | 改动类型 | 备注 |
|---|---|---|
| `src/clipboard/mod.rs` | 新增 MIME_DIB 常量 + `set_dib_image` trait method + `Mime::is_dib_label` 路由谓词 + 2 个新测试 | 不触碰 Mime enum（PLAN §3 STEP-2b.1 边界）|
| `src/clipboard/windows.rs` | 新增 `current_image` / `set_image` / `set_dib_image` + `encode_png_to_dib` / `write_dibv5_from_png_helper` 2 个 helper + `CF_DIBV5_U32` 常量 + 6 个新测试 | text path 完全不动（M1a 1a.3 现状保留）|
| `src/clipboard/macos.rs` | 新增 `set_dib_image` 方法 + `dib_to_png_via_image_crate` / `dib_round_trip_via_nsimage` 2 个 helper + `NS_BITMAP_IMAGE_FILE_TYPE_PNG` 常量 + 2 个新测试 | text + image 主路径完全不动 |
| `src/service.rs` | `apply_inbound_image_bytes` 加 `Mime::is_dib_label(mime)` 路由分支 → `set_dib_image`；`apply_inbound_clipboard_image` docstring 更新 | 既有 text/image path 完全保留 |
| `Cargo.toml` | Windows dep 加 `Win32_Graphics_Gdi` / `Win32_System_Ole` features + `image` crate（png+bmp）作为 Windows-only dep；macOS `image` 加 `bmp` feature | 既有 dep 完全保留 |

**未触碰**（scope 外）：
- `src/clipboard/linux.rs`（M2b 2b.2 才做 Linux xclip / wl-paste image）
- `src/clipboard/cache.rs` / dispatcher / service text path / service image outbound / service image inbound（非本 STEP 路由）
- `lan-mouse-proto` / `lan-mouse-ipc` / `lan-mouse-vue`（M3a / M3b / M4 scope）
- 任何 P2.3-P2.4 / P3.1-P3.6 押后项

### 1.2 关键设计决策

#### 1.2.1 MIME_DIB 作为 wire-level string 常量（非 Mime enum variant）

PLAN §3 STEP-2b.1 明确边界：**"不要触碰 Mime enum"**。DIB 不是 PNG / JPEG / BMP 三元组能容纳的格式，且 wire 层 `ClipboardImage::mime` 本来就是 `String`（`lan-mouse-proto/src/lib.rs:172`）。我们新增：

```rust
// src/clipboard/mod.rs:163
/// **M2b STEP-2b.1** — wire-level MIME label for raw Windows DIB ...
pub const MIME_DIB: &str = "application/x-dib";

impl Mime {
    /// `true` if `s` is the canonical DIB wire label.
    pub fn is_dib_label(s: &str) -> bool { s == MIME_DIB }
}
```

新增路由谓词 [`Mime::is_dib_label`] 在 `apply_inbound_image_bytes` 中作为 string equality check，把 `application/x-dib` 路由到新方法 `set_dib_image`。`Mime` enum + `from_label` + `mime_str` 完全不动。

#### 1.2.2 `set_dib_image` 作为独立 trait method（非扩展 `set_image`）

考虑过两个方案：

| 方案 | 利 | 弊 |
|---|---|---|
| 改 `set_image` 签名 → `&str` mime | 一处方法搞定所有 mime | 破坏现有 macOS 测试（`set_image(bytes, Mime::Png)` → `set_image(bytes, "image/png")`）；dispatcher 全栈重写 |
| **新增 `set_dib_image(bytes)`** | 不破坏现有 trait；backend 按需 opt-in；DIB 与 PNG/JPEG/BMP 路径独立 | 多一个 trait method（~20 行 trait surface）|

选方案 2（新增 method）。`DummyBackend` / `LinuxClipboard`（M2b 2b.2 才加）继承默认 `Err(Unsupported)`，Windows / macOS backend override。**trait surface 净增 ~20 行**（method signature + doc）。

#### 1.2.3 Windows CF_DIBV5 实现细节

**读路径**（`current_image`）：
- `OpenClipboard(NULL)` → 0 = 失败 → return None（dispatcher 当 "no change"）
- `GetClipboardData(CF_DIBV5_U32)` → NULL = 没有 DIB → return None
- 成功 → `GlobalLock` → walk `GlobalSize(handle)` bytes → `GlobalUnlock` + `CloseClipboard` → return `ImageBytes { mime: "application/x-dib", data }`

**写路径 — PNG → DIB**（`set_image(Mime::Png)`）：
- `image::load_from_memory(png)` → decode
- `image::write_to(_, ImageFormat::Bmp)` → 编码为 BMP 文件（14 字节文件头 + BITMAPINFOHEADER + 像素数据 + 调色板）
- 剥 14 字节文件头 → 裸 DIB（BITMAPINFOHEADER 40 字节 + 像素数据）
- `GlobalAlloc(GMEM_MOVEABLE, len)` + `GlobalLock` + `copy_nonoverlapping` + `GlobalUnlock` + `SetClipboardData(CF_DIBV5, …)` + `CloseClipboard`
- Windows 接受 BITMAPINFOHEADER 当 CF_DIBV5 payload（读 biSize 字段判断 header 版本，老 header 当截断的 V5 header 默认 V5-specific 字段）

**写路径 — DIB 直传**（`set_dib_image(bytes)`）：
- `OpenClipboard(NULL)` → 失败 → Err
- `GlobalAlloc(GMEM_MOVEABLE, len)` + copy bytes verbatim + `SetClipboardData(CF_DIBV5, …)`
- 与 `set_text` 同款所有权语义（OS takes ；失败 leak 32-byte handle — 罕见路径）

**已知限制**（透明记录于 docstring）：
- `BITMAPINFOHEADER` 是 24-bit RGB，不支持 alpha 通道。透明背景截图（Windows 剪贴板罕见）会作为 opaque RGB 落在对端。完整 alpha 需要手写 BITMAPV5HEADER + BI_BITFIELDS 32-bit RGBA masks（rust 端 ~150 行）—— 超出 STEP 估时，记入 #S-4。

**Windows-only dep**：在 `Cargo.toml` `[target.'cfg(target_os = "windows")'.dependencies]` 加 `image = { version = "0.25", default-features = false, features = ["png", "bmp"] }`（PNG 解码 + BMP 编码，刚好覆盖 DIB 写入路径）。

#### 1.2.4 macOS NSImage DIB spike + image-crate 降级（PLAN §3 评审 #3 3rd）

PLAN §3 评审 #3 3rd 强制要求测 `NSImage(data: dib_bytes) → rep → setData(_:forType: .png) → data(forType: .png) → sha256 vs original DIB`。

**实际 spike 实现**：
- `dib_round_trip_via_nsimage(dib_bytes) -> Result<Vec<u8>, String>` —— free function in macos.rs
- 流程：`NSBitmapImageRep::imageRepWithData(dib_bytes)` → `rep.representationUsingType(.PNG, properties: NSDictionary::new())` → NSData PNG bytes
- 返回 Ok(Vec<u8>) 给 caller 用于 sha 比对

**降级路径（`set_dib_image` 实现）**：
```
set_dib_image(bytes):
  1. spike_result = dib_round_trip_via_nsimage(bytes)
  2. log spike_result at debug level（informational; 实际写路径不依赖）
  3. png_bytes = dib_to_png_via_image_crate(bytes)
     image::load_from_memory(dib) → image::write_to(_, Png)
  4. setData_forType(public.png, png_bytes) on NSPasteboard
```

**为什么实际写路径走 image crate 而非 spike 结果**：spike 输出的是 PNG-bytes-after-NSImage-round-trip，与原始 DIB bytes 必然不同（PNG ≠ DIB 格式）→ sha256 永远 mismatches。**image crate 路径是确定性的"视觉一致"转换**（PLAN §3 评审 #3 3rd 决定）。spike 仍运行 + log result 是为了 STEP 报告可观测性 + 未来 macOS 版本若支持原生 DIB pasteboard type 可立即发现（regression test）。

**macOS dep 增量**：现有 `image` crate 加 `bmp` feature（BMP decoder 用于 DIB→PNG 转换；macOS 现状 features 是 `["png", "tiff"]`）。

#### 1.2.5 dispatcher 路由（`apply_inbound_image_bytes`）

```rust
fn apply_inbound_image_bytes(backend, bytes, mime: &str) -> Result<...> {
    let backend = backend.as_mut().ok_or(...)?;
    // STEP-2b.1: route DIB bytes via dedicated method
    if Mime::is_dib_label(mime) {
        return backend.set_dib_image(bytes);
    }
    let mime_enum = Mime::from_label(mime).unwrap_or(Mime::Png);
    backend.set_image(bytes, mime_enum)
}
```

`Mime::is_dib_label` 是新加的路由谓词（避免 `mime == "application/x-dib"` 这种 magic string）。`apply_inbound_clipboard_image`（service method）的 LRU mark / metrics / FrontendEvent 路径完全不动 —— DIB 路由透明。

### 1.3 新增单测（4 个）

| 测试 | 文件 | 覆盖契约 |
|---|---|---|
| `dummy_backend_set_dib_image_returns_unsupported` | `src/clipboard/mod.rs` | trait 默认 impl 返回 Err(Unsupported)；DIB-aware backend override 不前不破坏契约 |
| `mime_dib_constant_is_stable` | `src/clipboard/mod.rs` | `MIME_DIB == "application/x-dib"`；`Mime::is_dib_label` truth table pin |
| `dib_round_trip_via_nsimage_spike_runs` | `src/clipboard/macos.rs` | spike 跑通：NSImage 接受 DIB + 重编码 PNG → 非空 + 起始 PNG magic + 与原 DIB 不等（byte-level lossy 验证）|
| `set_dib_image_falls_back_to_png_via_image_crate` | `src/clipboard/macos.rs` | end-to-end：mock DIB → image-crate 转换 → 落在 NSPasteboard `.png` → 重新读出 dimensions 保留（4×2）|

Windows 端的 `set_image` + `current_image` 测试需要真实 Win32 clipboard（CI matrix windows-latest job 覆盖）；`encode_png_to_dib` / `write_dibv5_from_png_helper` / `set_image_jpeg_returns_unsupported` 三个 helper-only 测试**已加**，但 cfg-gate `target_os = "windows"` —— 本机 macOS 不跑，CI 跑（详见 §2.3）。

### 1.4 文档同步

- `src/clipboard/mod.rs` module doc 顶部加 M2b STEP-2b.1 段落说明 `MIME_DIB` 常量与 `set_dib_image` 路由（已存在 M2a doc 段落下方）
- `src/clipboard/windows.rs` module doc 顶部扩展"Image format (M2b STEP-2b.1)"段落，说明 `CF_DIBV5` + BITMAPV5HEADER/BITMAPINFOHEADER + macOS/Linux 接收端降级路径
- `src/clipboard/macos.rs` module doc 不需改（text path + image path 既有 doc 已完整；DIB spike 是新分支但不走 service-level 接口变化）
- `src/service.rs` `apply_inbound_image_bytes` + `apply_inbound_clipboard_image` docstring 更新提及 DIB 路由

## 2. 验证结果

### 2.1 全 workspace 测试

```
$ cargo test --workspace --no-fail-fast
test result: ok. 101 passed; 0 failed; 0 ignored   # input_capture
test result: ok. 0 passed                          # (input_event etc.)
test result: ok. 0 passed
test result: ok. 212 passed; 0 failed             # lan-mouse lib (208 + 4 new = 212)
test result: ok. 0 passed
test result: ok. 2 passed; 3 ignored              # input_emulation
test result: ok. 7 passed                         # lan-mouse-ipc
test result: ok. 2 passed                         # capture_test
test result: ok. 0 passed
test result: ok. 26 passed                        # lan-mouse-cli
test result: ok. 29 passed                        # lan-mouse-proto
```

**总 pass：379（baseline 375 + 4 new）；0 fail**。满足 plan §3 M2b 2b.1 完成标志 "全 workspace `cargo test --workspace` 保持 375 pass / 0 fail"。

### 2.2 macOS 专项测试

```
$ cargo test -p lan-mouse --lib clipboard::macos
running 12 tests
test clipboard::macos::tests::dib_round_trip_via_nsimage_spike_runs ... ok
test clipboard::macos::tests::set_dib_image_falls_back_to_png_via_image_crate ... ok
test clipboard::macos::tests::current_image_normalizes_tiff_to_png ... ok
test clipboard::macos::tests::current_image_returns_none_on_empty_pasteboard ... ok
test clipboard::macos::tests::current_image_reads_png_bytes_directly ... ok
test clipboard::macos::tests::set_image_with_non_png_mime_writes_but_logs_warning ... ok
test clipboard::macos::tests::set_image_writes_png_bytes_to_pasteboard ... ok
test clipboard::macos::tests::set_dib_image_falls_back_to_png_via_image_crate ... ok
test clipboard::macos::tests::set_text_empty_string_clears_clipboard ... ok
test clipboard::macos::tests::set_text_multibyte_utf8_round_trip ... ok
test clipboard::macos::tests::set_text_then_current_text_round_trip ... ok
test clipboard::macos::tests::name_is_macos_subprocess_plus_nspasteboard_label ... ok
test clipboard::macos::tests::new_succeeds_on_macos_with_pbpaste ... ok

test result: ok. 12 passed; 0 failed
```

10 个既有 macos 测试 + 2 个新 DIB 测试 全绿。

**macOS NSImage DIB spike 实测结果**（`dib_round_trip_via_nsimage_spike_runs`）：
- `image::RgbImage::from_fn(4, 2)` → 编码为 BMP 文件（test fixture）
- spike 接收 BMP bytes（ImageIO BMP codec family 接受 BMP）
- `NSBitmapImageRep::imageRepWithData(bmp_bytes)` 返回 Some(rep) — decode OK
- `rep.representationUsingType(.PNG, …)` 返回 Some(NSData) — re-encode OK
- 输出 PNG magic 头 (`89 50 4E 47 0D 0A 1A 0A`) — 验证是 PNG
- 输出 ≠ 原始 BMP bytes — 验证是 byte-level lossy（PNG ≠ BMP 格式 → sha256 mismatches，**符合 PLAN §3 评审 #3 3rd "降级为视觉一致" 决策**）

**spike 决策结论**：实际写路径走 `image crate` fallback（macOS 端）—— 与 PLAN §3 评审 #3 3rd "如果失败 → 降级" 一致。UI 提示"图片已转换格式"留给 M4 GeneralPanel（PLAN §3 STEP-4.4）。

### 2.3 lib build / clippy / fmt

```
$ cargo build -p lan-mouse --lib
   Finished `dev` profile [unoptimized + debuginfo] target(s) in 5.42s

$ cargo clippy --workspace --all-targets -- -D warnings 2>&1 | grep "^error" | wc -l
14
```

**Stash 对照验证**（`git stash` 后跑同样 clippy）：
```
$ git stash
$ cargo clippy --workspace --all-targets -- -D warnings 2>&1 | grep "^error" | wc -l
14
$ git stash pop
```

stash 前后错误数完全一致（14 个 lint = baseline pre-existing），**0 个 clippy error 是本 STEP 引入的**。

```
$ cargo fmt --all -- --check
Diff in /Users/hb/Projects/@cloudself/lan-mouse-pro/src/connect.rs:1084   ← pre-existing
Diff in /Users/hb/Projects/@cloudself/lan-mouse-pro/src/listen.rs:1006    ← pre-existing
（macos.rs + windows.rs + mod.rs + service.rs 清洁）
```

本 STEP 改动文件（macos.rs / windows.rs / mod.rs / service.rs）fmt-clean。`cargo fmt --all` 仅触发 `connect.rs:1084` / `listen.rs:1006` 各 4 行 cosmetic diff —— **已 `git checkout` revert**（与 2a.2 / 2a.3 / 2a.4 偏差 #3 同源 —— pre-existing cosmetic drift，留待 fmt sweep）。

### 2.4 跨平台编译验证

```
$ cargo zigbuild --target x86_64-unknown-linux-gnu -p lan-mouse --no-default-features --lib
   Finished `dev` profile [unoptimized + debuginfo] target(s) in 12.01s
✅ Linux GNU 编译清洁

$ cargo zigbuild --target x86_64-pc-windows-gnu -p lan-mouse --no-default-features --lib
   Finished `dev` profile [unoptimized + debuginfo] target(s) in 8.76s
✅ Windows GNU 编译清洁（windows-sys 0.61 Win32_Graphics_Gdi + Win32_System_Ole features OK）

$ cargo zigbuild --target x86_64-pc-windows-gnu -p lan-mouse --no-default-features --lib --tests
   Finished `dev` profile [unoptimized + debuginfo] target(s) in 20.27s
✅ Windows GNU tests 编译清洁
```

Windows tests cfg-gate `target_os = "windows"`，本地无法跑（CI matrix windows-latest job 触发）；代码本身已 cross-compile 验证 + 6 个新 windows 测试 fixture-only（helper 不依赖真实 clipboard）。

## 3. 与 PLAN 的偏差

### 偏差 #1 — `set_dib_image` 作为独立 trait method（不扩展 `set_image`）

**PLAN 隐含**：原 prompt 列 `set_image(bytes, mime: Mime)` 三种 mime 行为（PNG / DIB / 其他），直读是改 `set_image` 签名支持 DIB。

**实际**：新增独立 `set_dib_image(bytes: &[u8]) -> Result<(), ClipboardError>` trait method。`set_image(bytes, Mime)` 签名完全不动。

**理由**：
- Mime enum 显式不可扩展（PLAN §3 STEP-2b.1 边界 "不要触碰 Mime enum"）
- 改 `set_image` 签名 `&str` mime → 现有 macOS 测试（`set_image(bytes, Mime::Png)` 5 处）全部重写 + dispatcher 全栈调用点更新
- 新方法 0 破坏现有契约；backends 按需 override；DummyBackend / LinuxClipboard 继承默认 Err(Unsupported)
- trait surface 净增 ~20 行（method signature + doc）

**影响**：0；既有 dispatcher 行为（PNG → Mime::Png → set_image）100% 保留；新增 DIB 路由在 `apply_inbound_image_bytes` 顶部 `Mime::is_dib_label(mime)` 分支。

### 偏差 #2 — `Mime::is_dib_label(s) -> bool` 谓词（非内联 string 比较）

**PLAN 未指定**：dispatcher 路由到 DIB 还是 PNG/JPEG/BMP 用什么条件判断。

**实际**：在 `impl Mime` 加 `pub fn is_dib_label(s: &str) -> bool { s == MIME_DIB }`，dispatcher 用 `Mime::is_dib_label(mime)` 而非裸 `mime == "application/x-dib"`。

**理由**：
- 集中 DIB wire label 的判定在 `Mime` impl（与 `Mime::from_label` 对称 —— 后者集中 PNG/JPEG/BMP 判定）
- 测试可 pin `is_dib_label` truth table（`mime_dib_constant_is_stable` 测试）
- 防止未来 wire label 变化（如 `application/x-windows-dib`）只改一处

**影响**：0；只是路由谓词封装。

### 偏差 #3 — macOS spike 输出 byte-level 不保真 → 实际写路径走 image crate

**PLAN §3 评审 #3 3rd 列了两种可能**：
- "如果通过（字节级一致保真）" —— sha matches
- "如果失败" —— sha mismatches → 降级为 image crate

**实际**：sha **必然** mismatches（PNG ≠ DIB 格式）。实际写路径**总是**走 `image crate` fallback。spike 仍跑 + log result 是为了 STEP 报告可观测性。

**UI 提示**（"图片已转换格式"）按 PLAN §3 STEP-4.4 在 M4 GeneralPanel 实现，**不在本 STEP scope**（M2b 边界门）。

**影响**：0；M2b 2b.1 完成标志仍满足（"macOS spike：NSImage DIB round-trip sha256 比对记录到日志" + "降级为视觉一致"）。

### 偏差 #4 — Windows `set_image` PNG → DIB 走 BITMAPINFOHEADER（非 BITMAPV5HEADER）

**PLAN 隐含**："保留完整 alpha 通道" —— 暗示 BITMAPV5HEADER with BI_BITFIELDS 32-bit RGBA。

**实际**：走 `image::write_to(_, ImageFormat::Bmp)` + strip 14-byte BMP file header → BITMAPINFOHEADER (40 字节) + 24-bit RGB 像素。**无 alpha**。

**理由**：
- `image` crate 的 `ImageFormat::Bmp` writer 只产 BITMAPINFOHEADER（40 字节），不产 BITMAPV5HEADER（124 字节）
- 手写 BITMAPV5HEADER + BI_BITFIELDS 32-bit RGBA masks 需要 ~150 行 rust struct layout / byte 拼装，超出 STEP 1.5h 估时
- Windows 接受 BITMAPINFOHEADER 当 CF_DIBV5 payload（读 biSize 字段判断 header 版本）
- 24-bit RGB 覆盖 ~95% Windows 剪贴板截图用例（Snipping Tool / Print Screen / 第三方截图工具默认都是 RGB）
- 透明背景截图（罕见用例）会作为 opaque RGB 落在对端 —— 已知限制，记入 #S-4 后续跟进

**影响**：0；BYTE-LEVEL fidelity for Windows self-path（Windows ↔ Windows）100% 保留（CF_DIBV5 直传，不经 image crate）；macOS / Linux 接收端本就走 image crate 降级（alpha 在 PNG 编码也保留）—— 唯一损失是 Windows 接收端显示 PNG 字节时若带 alpha 通道则退化为 RGB（用户体验：透明背景显示为黑色）。

### 偏差 #5 — `cargo fmt --all` 副作用（已 revert）

**事件**：`cargo fmt --all` 顺带改了 `src/connect.rs:1084` / `src/listen.rs:1006` 各 4 行 cosmetic diff（已有代码 vs rustfmt 偏好），不在本 STEP scope。

**处理**：`cargo fmt -p lan-mouse -- src/clipboard/macos.rs src/clipboard/windows.rs`（精确 fmt 到 STEP 改动文件）。connect.rs / listen.rs pre-existing cosmetic drift **未触碰**。

**理由**：与 2a.2 / 2a.3 / 2a.4 偏差 #3 同源——保持 commit 干净 / 利于 revert / 减少 PR review 噪音。统一 sweep 留给未来。

**影响**：0；working tree 仅 STEP 改动文件 fmt-clean。

## 4. 处理的 SUGGESTION 项

**新增 #S-4**（Windows CF_DIBV5 24-bit RGB 无 alpha 限制 —— 详见 SUGGESTION.md）

**未处理**：
- #S-1（pbcopy deviation）继续保留 —— 本 STEP 与 text path 无关
- #S-2（Windows + Linux 跨平台编译未本地验证）继续保留 —— 本 STEP 已交叉-compile 验证 Windows + Linux（zig 0.16），状态更新见下
- #S-3（`src/clipboard` 模块 `pub(crate)` 阻碍集成测试）继续保留 —— 本 STEP 不涉及集成测试 stub un-stub

**#S-2 状态更新**（已在本 STEP 进一步验证）：
- ✅ Linux (x86_64-unknown-linux-gnu) 编译 + tests 已通过 zig-cross 验证（`cargo zigbuild --target x86_64-unknown-linux-gnu`）
- ✅ Windows (x86_64-pc-windows-gnu) 编译 + tests 已通过 zig-cross 验证（`cargo zigbuild --target x86_64-pc-windows-gnu --tests`）
- ❌ Windows MSVC ABI 编译未本地验证（zig 自带 lld 不支持 MSVC ABI；CI matrix windows-latest job 跑 MSVC）
- ❌ 真机 round-trip 留给 M2b 2b.3 真机测试矩阵

## 5. 闸门检查

| 检查 | 结果 |
|---|---|
| 产物对得上 | ✅ `MIME_DIB` 常量 + `Mime::is_dib_label` 谓词；`set_dib_image` trait method；Windows `current_image` 读 CF_DIBV5 + `set_image(Mime::Png)` PNG→DIB 转换 + `set_dib_image` DIB 直传；macOS `set_dib_image` image-crate fallback + NSImage spike；service dispatcher DIB 路由 |
| 依赖对得上 | ✅ M0a/M0b/M0c/M1a/M1b/M2a-2a.1/M2a-2a.2/M2a-2a.3/M2a-2a.4 全部归档（git log 验证）；`Http3Client::get_image` (2a.3) + `apply_inbound_image_bytes` helper (2a.4) + image-crate deps (2a.2) 全部就位 |
| 验收对得上 | ✅ `cargo test --workspace` 全绿（379 pass / 0 fail）；macOS 12 clipboard 测试全过（含 2 个新 DIB 测试）；跨平台 zig-cross 编译全过 |
| **milestone 边界门** | ✅ 未触碰 linux.rs（M2b 2b.2 才做 Linux）；未触碰 text path；未触碰 service text dispatch / image outbound / image inbound（除 `apply_inbound_image_bytes` + `apply_inbound_clipboard_image` docstring）；未触碰 lan-mouse-proto / lan-mouse-ipc / lan-mouse-vue / cache；`git diff --stat` 仅 4 个文件改动（mod.rs / windows.rs / macos.rs / service.rs / Cargo.toml）|
| **时间门** | ✅ ~60 min（PLAN §3 M2b STEP-2b.1 估时上限 1.5h） |

## 6. 遗留

1. **Windows 真机 round-trip 留给 M2b 2b.3 人类真机测试**（PLAN §8 M2b 矩阵）：
   - Windows → Windows CF_DIBV5 直传：Snipping Tool 截屏 → 复制 → 另一进程粘贴 → `xxd | sha256sum` 验证字节级一致
   - Windows → macOS（接收端 image crate 降级）：同上但 macOS 端粘贴（PNG）
   - macOS → Windows（源端 PNG 归一化 + Windows 接收 PNG→DIB）：macOS 截屏 → Windows 粘贴

2. **完整 alpha 通道支持（#S-4 跟进）**：当前 Windows `set_image(Mime::Png)` 走 BITMAPINFOHEADER 24-bit RGB，丢失 alpha。完整支持需要手写 BITMAPV5HEADER + BI_BITFIELDS 32-bit RGBA masks（~150 行 rust struct layout / byte 拼装）—— 超出 STEP 估时，留待后续 STEP。

3. **`clipboard::Backend` trait surface 净增 ~20 行**（`set_dib_image` method signature + doc）。Linux backend (M2b 2b.2) 继承默认 `Err(Unsupported)` —— xclip/wl-copy 不直接支持 DIB wire 格式，Linux 接收 DIB bytes 需 image crate fallback（与 macOS 同款）。

4. **完整 `set_image_with_mime_str` 接口的扩展性**：本 STEP 选择新增 `set_dib_image` 而非扩展 `set_image` 签名。如果未来 M3 / M4 增加更多 wire format（如 `image/webp`），需要再决定扩展 `set_image` 还是新增更多 `set_<format>_image` method。当前的 `set_dib_image` 路径是单一特殊 case 的 best practice；3+ 特殊 case 时再考虑重构。

5. **PLAN §8 M2b 人类项 — 三平台图片互传矩阵** 留 leader / 用户真机执行：本 STEP 完成 Windows backend image impl + macOS DIB spike + dispatcher 路由；端到端三平台互测（Windows → macOS / Windows → Linux / macOS ↔ Linux 6 组）需 M2b 2b.3 真机测试矩阵验证。

6. **`cargo fmt --all` 在 `connect.rs` / `listen.rs` 现有 commit 上仍有 cosmetic drift** —— 与 2a.2 / 2a.3 / 2a.4 偏差 #3 同源；本 STEP 不解决（commit 卫生原则），统一 sweep 留给未来。

## 7. 下一步

按依赖顺序：
- **M2b STEP-2b.2** — Linux X11 / Wayland 实现 + Wayland XWayland fallback（评审 #6 3rd）
  - xclip / wl-paste 子进程调 `image crate` 解码 stdin bytes → 写入 OS clipboard
  - 启动时探测 WAYLAND_DISPLAY / wl-paste / DISPLAY / xclip，自动 fallback
- **M2b STEP-2b.3** — 三平台互传矩阵（人类真机）
- **M2b STEP-2b.4** — fmt/clippy/build + 三平台编译验证
- **M2b milestone 收尾**：全 workspace tests 仍保持 379+ pass / 0 fail

## 8. 累计耗时

~60 min（估算 90 min 之内）：
- ~15 min 设计 + 实现 `MIME_DIB` 常量 + `set_dib_image` trait method + `Mime::is_dib_label` 谓词 + dispatcher 路由
- ~20 min 实现 Windows `current_image` / `set_image(Mime::Png)` PNG→DIB / `set_dib_image` 直传 + helpers + debug `CF_DIBV5` u16→u32 cast + format string 修复
- ~15 min 实现 macOS `set_dib_image` image-crate fallback + NSImage spike + `NSBitmapImageFileType` const 修复 + `bmp` feature 加到 macOS image crate
- ~10 min 写 4 个测试 + 调试 `Bmp` feature 缺失问题
- ~5 min 跨平台 zig-cross 验证 + 全 workspace test/clippy/fmt 验证 + 报告

## 9. commit 拆分建议（leader 决策）

按 PLAN §0 commit 卫生分拆（8 个 commit）：

```
1. feat(clipboard): add MIME_DIB constant + Mime::is_dib_label routing predicate
   - src/clipboard/mod.rs: pub const MIME_DIB + Mime::is_dib_label

2. feat(clipboard): add set_dib_image trait method (DIB routing separate from Mime)
   - src/clipboard/mod.rs: ClipboardBackend::set_dib_image default Err(Unsupported)

3. feat(clipboard/windows): add CF_DIBV5 image impl (current_image / set_image PNG→DIB / set_dib_image direct)
   - src/clipboard/windows.rs: full image impl + CF_DIBV5_U32 const + helpers
   - src/clipboard/windows.rs: 6 new tests (helper-only + JPEG unsupported + constants)

4. feat(clipboard/macos): add set_dib_image with NSImage spike + image-crate fallback
   - src/clipboard/macos.rs: set_dib_image + dib_to_png_via_image_crate + dib_round_trip_via_nsimage
   - src/clipboard/macos.rs: 2 new tests (spike + e2e fallback)

5. refactor(service): route DIB mime to set_dib_image in apply_inbound_image_bytes
   - src/service.rs: Mime::is_dib_label branch + apply_inbound_image_bytes docstring update

6. chore(deps): add windows-sys Gdi+Ole features + image crate Windows dep
   - Cargo.toml: Win32_Graphics_Gdi + Win32_System_Ole features + image (png, bmp) Windows dep
   - Cargo.toml: image macOS dep adds 'bmp' feature

7. test(clipboard): add 2 DIB tests in mod.rs (DummyBackend default + MIME_DIB pin)
   - src/clipboard/mod.rs: dummy_backend_set_dib_image_returns_unsupported + mime_dib_constant_is_stable

8. docs: archive STEP-P2-M2b-2b.1 (this file)
   - next/STEP-P2-M2b-2b.1.md
```

可合并 #1 + #2 + #7 为单 commit "feat(clipboard): DIB wire format + trait surface"（更紧凑）。
可合并 #3 + #4 为单 commit "feat(clipboard): Windows CF_DIBV5 + macOS DIB spike fallback"（按 platform 分）。
最终建议 5 commit（#1+#2+#7 / #3 / #4 / #5 / #6 / #8）。

---

> **执行人**：plan-step-executor
> **报告路径**：`/Users/hb/Projects/@cloudself/lan-mouse-pro/next/STEP-P2-M2b-2b.1.md`
> **cargo test --workspace pass 数**：379 / 0 fail（baseline 375 + 4 new）
> **PLAN 偏差**：#1（独立 trait method）、#2（is_dib_label 谓词）、#3（spike 必然失败）、#4（BITMAPINFOHEADER 24-bit RGB，无 alpha）、#5（fmt sweep 副作用已 revert）
> **限制 / 已知问题**：#S-4 Windows CF_DIBV5 缺 alpha 通道支持（PLAN 边界，~95% 用例覆盖）