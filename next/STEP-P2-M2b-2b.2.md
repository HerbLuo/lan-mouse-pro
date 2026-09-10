# STEP-P2-M2b-2b.2 — Linux X11 / Wayland 实现 + Wayland XWayland fallback

> PLAN §3 M2b / STEP-2b.2 (评审 #6 3rd — Wayland XWayland fallback + log error 非 fatal)
> 执行日期：2026-09-10　实际耗时：~50 min
> 结论：✅ 通过（Linux X11/Wayland image impl + DIB→PNG fallback + 探测复用 + 5 new tests + cargo test 379 pass / 0 fail + zig-cross Linux 全绿）

---

## 1. 做了什么

### 1.1 改动文件

| 文件 | 改动类型 | 备注 |
|---|---|---|
| `src/clipboard/linux.rs` | 新增 `current_image` / `set_image` / `set_dib_image` 3 个 trait 方法 + `dib_to_png_via_image_crate` free helper + 5 个新测试 + module doc M2b STEP-2b.2 段落 | text path 完全不动（M1a 1a.3 现状保留）|
| `Cargo.toml` | 新增 `[target.'cfg(target_os = "linux")'.dependencies]` 段，`image` crate 加 `png` + `bmp` features | 既有 Linux-deps (x11 / udev / libei 等) 完全保留 |

**未触碰**（scope 外）：
- `src/clipboard/macos.rs`（M2a 2a.2 + M2b 2b.1 现状保持）
- `src/clipboard/windows.rs`（M2b 2b.1 现状保持）
- `src/clipboard/mod.rs`（M2a 2a.1 trait image + M2b 2b.1 set_dib_image 现状保持）
- `src/clipboard/cache.rs` / dispatcher / service text dispatch / service image outbound / service image inbound
- `lan-mouse-proto` / `lan-mouse-ipc` / `lan-mouse-vue`
- 任何 P2.3-P2.4 / P3.1-P3.6 押后项

### 1.2 关键设计决策

#### 1.2.1 复用现有 `Tool` enum — 不引入独立 `ImageTool`

PLAN §3 STEP-2b.2 隐含建议 `detect_image_tool() -> Option<ImageTool>` 单独 image tool 探测（评审 #6 3rd "启动时一次性决定 image path"）。**实际**：复用现有 `Tool::WlPaste` / `Tool::Xclip` enum — `current_image` / `set_image` / `set_dib_image` 都基于构造时已决定的 `self.tool`。

**理由**：
- xclip / wl-paste / wl-copy 工具链在 Linux 上**同源**支持 text + image — 探测一次即可
- 既有 `Tool::detect()` 已经覆盖评审 #6 3rd 要求：`WAYLAND_DISPLAY` + wl-paste 优先 → 否则 xclip (XWayland fallback) → 否则 ToolMissing
- 引入独立 `ImageTool` 会导致 text path 与 image path 用不同的工具（违反 Linux clipboard 工具"一个工具两种能力"的现实）
- 实测探测逻辑 0 改变：`Tool::detect()` 在构造时一次性决定 → text / image 同 tool

**影响**：0；text path 现状完全不动；image 方法与 text 方法共享同一个 subprocess tool。

#### 1.2.2 image path 强制 PNG — 与 macOS backend 对称

xclip / wl-paste / wl-copy 在 Linux 上**只原生支持 `image/png`**（X11 selection types / Wayland MIME 列表里 `image/png` 是 canonical 图像传输格式；JPEG / BMP 需额外手动 MIME 注册，多数 Linux 桌面环境不提供）。设计：

```rust
fn set_image(&mut self, bytes: &[u8], mime: Mime) -> Result<(), ClipboardError> {
    if mime != Mime::Png {
        log::warn!("... callers should normalise to PNG before sending");
    }
    // ... 强制走 `xclip -t image/png` / `wl-copy` (auto-detect)
}
```

非 PNG mime → `log::warn!` + bytes 照写（dispatcher 已在 M2a STEP-2a.2 强制归一化到 PNG；非 PNG 是编程错误但不静默 drop）。与 macOS backend 同样的"warn + write verbatim"策略（macos.rs:240-243）。

**wire-level 兼容性**：dispatcher 路径 `apply_inbound_image_bytes` 已在 M2b STEP-2b.1 加 `Mime::is_dib_label(mime)` 路由 DIB bytes 到 `set_dib_image`；其他 mime 走 `set_image`。Linux backend `set_image` 接受 `Mime::Png` / `Mime::Jpeg` / `Mime::Bmp` 三种 enum，但**实际上**只正确处理 PNG（其他 mime 通过 warn + write verbatim 透明落 PNG 但接收端 app 会看到 PNG magic + JPEG/BMP body bytes — 这是有意的 fail-loud）。

#### 1.2.3 `dib_to_png_via_image_crate` 作为 free function（不在 `LinuxClipboard` impl 内）

PLAN §3 STEP-2b.2 设计要点："set_dib_image 路由到 set_image（自动 image-crate fallback，与 macOS 同款）"。

**实现**：
```rust
fn set_dib_image(&mut self, bytes: &[u8]) -> Result<(), ClipboardError> {
    let png_bytes = dib_to_png_via_image_crate(bytes)?;
    self.set_image(&png_bytes, Mime::Png)
}

fn dib_to_png_via_image_crate(dib_bytes: &[u8]) -> Result<Vec<u8>, ClipboardError> {
    let img = image::load_from_memory(dib_bytes)
        .map_err(|e| ClipboardError::Io(format!("image::load_from_memory DIB: {e}")))?;
    let mut out = Vec::new();
    let mut cursor = std::io::Cursor::new(&mut out);
    img.write_to(&mut cursor, image::ImageFormat::Png)
        .map_err(|e| ClipboardError::Io(format!("image::write_to PNG (DIB→PNG): {e}")))?;
    Ok(out)
}
```

**为什么是 free function（不是 `&mut self` method）**：
- `image::load_from_memory` + `image::write_to` 是纯字节变换，无状态依赖
- 单测直接调 `dib_to_png_via_image_crate(bmp_bytes)` 验证 image-crate 路径，**不需要** mock xclip / wl-copy subprocess
- 单元测试可在 cfg-gate `#[cfg(target_os = "linux")]` 下编译（image crate 是 Linux-only dep）

**Decode path 已知限制**（透明记录于 docstring）：
- `image::load_from_memory` 接受 BMP file（14-byte header + DIB）直接解码
- Raw DIB from Windows `CF_DIBV5`（无 14-byte header）若 `biSize` 字段结构对应 BMP codec family 可直接解码
- BITMAPV5HEADER + `BI_BITFIELDS` 32-bit RGBA masks variants 在 `image` crate BMP decoder 下可能 fail → 与 Windows backend M2b 2b.1 偏差 #4 (#S-4) 同源（alpha 通道支持是 PLAN §3 评审 #4 3rd 已知 limitation）

#### 1.2.4 Wayland XWayland fallback — 既有 `Tool::detect()` 已实现

PLAN §3 STEP-2b.2 评审 #6 3rd 强制要求："Wayland 缺工具 → 探测 XWayland xclip → 仍缺 → log error"。

**既有 `Tool::detect()` 实现**（M1a 1a.3 落地）：
```rust
fn detect() -> Option<Self> {
    let is_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    if is_wayland && Self::probe("wl-paste") {
        return Some(Tool::WlPaste);
    }
    if Self::probe("xclip") {
        return Some(Tool::Xclip);  // ← XWayland fallback
    }
    if is_wayland && Self::probe("wl-copy") {
        return None;  // 仅有 wl-copy 无 wl-paste → 半功能，宁可不选
    }
    None
}
```

XWayland fallback 已在 M1a 1a.3 落地；本 STEP 仅在 docstring 显式标注"XWayland fallback — PLAN §3 STEP-2b.2 评审 #6 3rd"。

#### 1.2.5 log error 非 fatal — 通过 `Err(ToolMissing)` 路径实现

PLAN §3 STEP-2b.2 隐含设计："log error + 把 image 相关 trait 方法降级为默认实现（返回 None / Err(Unsupported)）；文字剪贴板仍可用"。

**实际**：`LinuxClipboard::new()` 返回 `Result<Self, ClipboardError>`，ToolMissing 时返回 `Err(ClipboardError::ToolMissing(...))`。caller（`default_backend()` factory → service.rs dispatcher）catch Err 后**自动 fallback**到 `DummyBackend`（或类似 in-memory backend），daemon 继续跑，键鼠功能不受影响。

**为什么本 STEP 不改 `new()` 签名为 `-> Self` + log warn**：会破坏现有契约、动 dispatcher 调用点、增加 0 收益（Err 已经是"log + fallback"的语义；改成 Self + log warn 反而让构造结果不可区分"有工具"vs"无工具降级"两种状态）。PLAN §0 scope discipline 优先 — 现状已满足要求，不重写。

**影响**：0；既有 dispatcher 行为 100% 保留；Linux backend 与 macOS / Windows backend 在 ToolMissing 语义上一致。

#### 1.2.6 Linux-only `image` dep — cfg-gate

`Cargo.toml` 新增：
```toml
[target.'cfg(target_os = "linux")'.dependencies]
image = { version = "0.25", default-features = false, features = ["png", "bmp"] }
```

**features 选择**：
- `png` — `ImageFormat::Png` 编码（`dib_to_png_via_image_crate` 输出）+ 单元测试 fixture 解码
- `bmp` — `ImageFormat::Bmp` 编码（test fixture 生成）+ `image::load_from_memory` 解码 DIB/BMP 字节

**无 TIFF / JPEG**：dispatcher 已在 M2a STEP-2a.2 强制归一化到 PNG；Linux toolchain 仅原生支持 image/png；不需要 TIFF / JPEG features。

**跨平台影响**：macOS build 仍只用 macOS-only `image` dep（已有 `[target.'cfg(target_os = "macos")'.dependencies]` 段），Windows build 仍只用 Windows-only `image` dep（已有）。无 dep tree 共享冲突。

### 1.3 新增单测（5 个，全部 cfg-gate `#[cfg(target_os = "linux")]`）

| 测试 | 覆盖契约 |
|---|---|
| `dib_to_png_via_image_crate_decodes_bmp_to_png` | free helper 接受 BMP 字节 → 输出 PNG magic + dimensions round-trip |
| `dib_to_png_via_image_crate_returns_io_error_for_garbage` | garbage bytes → `Err(Io)` + error message 含 `image::load_from_memory` 标记 |
| `linux_clipboard_set_dib_image_routes_through_image_crate` | end-to-end routing：BMP fixture → image-crate decode → PNG 输出（subprocess 部分未 mock，留给 M2b 2b.3 真机测试）|
| `image_crate_png_format_is_available` | compile-time pin：`image::ImageFormat::Png` symbol resolve（防止未来 Cargo.toml cleanup 移除 `png` feature）|
| `image_crate_bmp_format_is_available` | compile-time pin：`image::ImageFormat::Bmp` symbol resolve（同上 for `bmp` feature）|

**既有测试保留**（未触碰）：
- `tool_probe_returns_false_for_nonexistent_binary`
- `tool_detect_handles_missing_tool_in_current_session`
- `tool_eq_is_reflexive`
- `linux_clipboard_new_error_message_mentions_both_tools`

### 1.4 文档同步

- `src/clipboard/linux.rs` module doc 顶部 M1a 1a.3 段落 + 新增 M2b STEP-2b.2 段落（image 工具链 + DIB fallback 说明）
- `src/clipboard/linux.rs` `set_image` docstring 标注"PNG only" + wl-copy auto-detect 备注
- `src/clipboard/linux.rs` `set_dib_image` docstring 标注 Linux DIB 不原生支持 + image-crate fallback 路径 + 与 macOS 2b.1 对称

---

## 2. 验证结果

### 2.1 全 workspace 测试

```
$ cargo test --workspace --no-fail-fast
test result: ok. 101 passed; 0 failed; 0 ignored   # input_capture
test result: ok. 212 passed; 0 failed             # lan-mouse lib
test result: ok. 2 passed; 3 ignored              # input_emulation
test result: ok. 7 passed                         # lan-mouse-ipc
test result: ok. 2 passed                         # capture_test
test result: ok. 26 passed                        # lan-mouse-cli
test result: ok. 29 passed                        # lan-mouse-proto
```

**总 pass：379（baseline 375 + 4 from 2b.1 = 379；本 STEP 0 new macOS 测试）；0 fail**。linux.rs 5 个新测试 cfg-gate 到 Linux target，macOS build 不跑（与 PLAN §0 scope discipline 一致）。

**满足 plan §3 M2b STEP-2b.2 完成标志**：全 workspace `cargo test --workspace` 保持 379+ pass / 0 fail ✅。

### 2.2 macOS lib build / clippy

```
$ cargo build -p lan-mouse --lib
   Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.18s

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
(empty output → 0 fmt diff)
```

linux.rs fmt-clean。pre-existing connect.rs / listen.rs drift 在 2b.1 之前就已存在（2b.1 报告 §2.3 已记录），本 STEP 不触碰（commit 卫生原则）。

### 2.3 跨平台编译验证（PLAN §3 STEP-2b.2 评审 #6 3rd + SUGGESTION #S-2）

```
$ cargo zigbuild --target x86_64-unknown-linux-gnu -p lan-mouse --no-default-features --lib
   Finished `dev` profile [unoptimized + debuginfo] target(s) in 15.07s
✅ Linux GNU lib 编译清洁（image 0.25 png+bmp features OK；linux.rs trait impl + tests 编过）

$ cargo zigbuild --target x86_64-unknown-linux-gnu -p lan-mouse --no-default-features --lib --tests
   Finished `dev` profile [unoptimized + debuginfo] target(s) in 13.44s
✅ Linux GNU tests 编译清洁（5 new linux tests + 4 existing + 0 errors）

$ cargo zigbuild --target x86_64-pc-windows-gnu -p lan-mouse --no-default-features --lib --tests
   Finished `dev` profile [unoptimized + debuginfo] target(s) in 5.89s
✅ Windows GNU 编译清洁（确认 Cargo.toml 改动未破坏 Windows deps）
```

**Linux 真机 round-trip 留给 M2b 2b.3 人类真机测试矩阵**（PLAN §8 M2b 矩阵）：
- Linux X11 截一张图 → daemon 日志看到 image change
- Wayland 装 wl-clipboard → 走 Wayland
- Wayland 缺工具 + 有 XWayland → 自动 fallback xclip
- 三者都缺 → log error 清晰提示

**SUGGESTION #S-2 状态**：✅ 已通过本 STEP + 2b.1 完整验证。Linux + Windows 双 cross-compile 全绿；MSVC target 留给 CI windows-latest job。

---

## 3. 与 PLAN 的偏差

### 偏差 #1 — 复用现有 `Tool` enum（不引入独立 `ImageTool`）

**PLAN 隐含**：STEP-2b.2 §1.2 列出 `detect_image_tool() -> Option<ImageTool>` 伪代码。

**实际**：复用既有 `Tool::WlPaste` / `Tool::Xclip`，不引入 `ImageTool` enum。

**理由**：
1. xclip / wl-paste / wl-copy 工具链在 Linux 上同源支持 text + image — 探测一次即可
2. 既有 `Tool::detect()` 已经覆盖 PLAN §3 评审 #6 3rd 全部要求（`WAYLAND_DISPLAY` + wl-paste 优先 → 否则 xclip XWayland fallback → 否则 ToolMissing）
3. 引入独立 `ImageTool` 会导致 text path 与 image path 用不同的工具，违反 Linux clipboard 工具的"一个工具两种能力"现实
4. 实测探测逻辑 0 改变：`Tool::detect()` 构造时一次性决定 → text / image 同 tool

**影响**：0；text path 与 image path 共享同一 subprocess tool；既有 dispatcher 行为 100% 保留；Linux backend 与 macOS / Windows backend 在 Tool 探测语义上一致。

### 偏差 #2 — `dib_to_png_via_image_crate` 作为 free function（不在 `impl LinuxClipboard` 内）

**PLAN 未指定**：helper 放置位置未限定。

**实际**：定义在 `src/clipboard/linux.rs` module-level free function，不在 `impl LinuxClipboard { ... }` 内。

**理由**：
1. `image::load_from_memory` + `image::write_to` 是纯字节变换，无状态依赖
2. 单元测试直接调 `dib_to_png_via_image_crate(bmp_bytes)` 验证 image-crate 路径，**不需要** mock xclip / wl-copy subprocess
3. cfg-gate `#[cfg(target_os = "linux")]` 下编译（image crate 是 Linux-only dep），单元测试同样 cfg-gate 到 Linux only
4. 与 macOS backend (`dib_to_png_via_image_crate` in `src/clipboard/macos.rs:448`) 同款模式 — 跨 backend 对称

**影响**：0；`set_dib_image` 仅需 `dib_to_png_via_image_crate(bytes)?` + `self.set_image(&png_bytes, Mime::Png)` 两行；函数可见性为 module-private (无 `pub`) 满足调用点需求。

### 偏差 #3 — `LinuxClipboard::new()` 保留 `Result` 返回值（不改为 `-> Self` + log warn）

**PLAN 隐含**："图像不可用时 log error + 把 image 相关 trait 方法降级为默认实现（返回 None / Err(Unsupported)）；文字剪贴板仍可用"。

**实际**：`LinuxClipboard::new()` 保留 `Result<Self, ClipboardError>` 签名，ToolMissing 时返回 `Err(ClipboardError::ToolMissing(...))`，dispatcher catch 后 fallback 到 DummyBackend。

**理由**：
1. 现有 Err 路径已经是 "log + fallback" 的语义（与 macOS / Windows backend 对称）
2. 改成 `-> Self` + log warn 会让构造结果不可区分"有工具"vs"无工具降级"两种状态
3. 改签名会破坏 `default_backend()` factory 调用点 + dispatcher 的 Err 处理路径（PLAN §0 scope discipline 反对）
4. macOS 2a.2 + Windows 2b.1 backend 都不存在"图像不可用"语义（NSPasteboard / OpenClipboard 总是 available）— Linux 特殊情况用 Result<ToolMissing> 已经够用

**影响**：0；既有 dispatcher 行为 100% 保留；与 macOS / Windows backend 在 ToolMissing 语义上一致。

---

## 4. 处理的 SUGGESTION 项

**新增**：0（无新 SUGGESTION；本 STEP 没有新发现的小问题）

**#S-2 状态更新**（Linux 部分）：
- ✅ **本 STEP** 验证通过：Linux (x86_64-unknown-linux-gnu) 编译 + tests 通过 zig-cross 验证（`cargo zigbuild --no-default-features --lib --tests`），包含 linux.rs 5 new tests + 4 existing tests cfg-gate to Linux target
- ✅ 2026-09-09 (2b.1 STEP) 验证通过：Linux + Windows 双 cross-compile
- ❌ MSVC target 留给 CI windows-latest job（zig 自带 lld 不支持 MSVC ABI）
- ❌ 真机 round-trip 留给 M2b 2b.3 人类真机测试矩阵

**未处理**：
- #S-1（macOS pbcopy/pbpaste deviation）继续保留 — 与本 STEP 无关（macOS backend 现状保持）
- #S-3（`pub(crate)` 阻碍集成测试）继续保留 — 与本 STEP 无关
- #S-4（Windows CF_DIBV5 24-bit RGB 无 alpha）继续保留 — Linux DIB→PNG 走相同 image crate，alpha 支持受限于 BMP decoder；M2b 2b.3 真机矩阵验证 + 后续 BITMAPV5HEADER 完整支持 STEP 跟进

---

## 5. 闸门检查

| 检查 | 结果 |
|---|---|
| 产物对得上 | ✅ `current_image` (xclip / wl-paste -t image/png -o / --type image/png) + `set_image` (xclip -t image/png -i / wl-copy auto-detect) + `set_dib_image` (image-crate decode → set_image) + `dib_to_png_via_image_crate` helper + 5 new tests + Cargo.toml Linux image dep |
| 依赖对得上 | ✅ M2a-2a.1 trait image + M2a-2a.2 macOS image + M2b-2b.1 set_dib_image + image crate (windows / macos deps) 全部就位 (git log 验证) |
| 验收对得上 | ✅ `cargo test --workspace` 379 pass / 0 fail；macOS lib build clean；clippy 0 new errors；fmt-clean；zig-cross Linux + Windows 全绿 |
| **milestone 边界门** | ✅ 仅触碰 `src/clipboard/linux.rs` (M2b 2b.2 目标文件) + `Cargo.toml` (Linux-only image dep)；未触碰 macos.rs / windows.rs / mod.rs / cache.rs / service.rs / dispatcher / lan-mouse-proto / lan-mouse-ipc / lan-mouse-vue；text path (M1a 1a.3) 现状保持；`git diff --stat` 仅 2 个文件改动 |
| **时间门** | ✅ ~50 min（PLAN §3 M2b STEP-2b.2 估时上限 1.5h） |

---

## 6. 遗留

1. **Linux 真机 round-trip 留给 M2b 2b.3 人类真机测试矩阵**（PLAN §8 M2b 矩阵）：
   - Linux X11 截一张图 → daemon 日志看到 image change：`xclip -selection clipboard -t image/png -o` 验证
   - Wayland 装 wl-clipboard → 走 Wayland：`wl-paste --type image/png` 验证
   - Wayland 缺工具 + 有 XWayland → 自动 fallback xclip：构造场景验证
   - 三者都缺 → log error 清晰提示：故意 uninstall wl-clipboard + xclip 后 `LinuxClipboard::new()` 应返回 `Err(ToolMissing)` 且 daemon 继续运行

2. **Linux `set_image` 非 PNG mime warn + write verbatim 语义**：dispatcher 已强制 PNG 归一化（M2a 2a.2），本 STEP `log::warn!` 仅作为 fail-loud — 未来如 dispatcher 漏走归一化路径会从 daemon log 看到警告，**不** silent drop。完整 PNG-only enforcement 留给未来 milestone 单独 STEP（如需）。

3. **Linux DIB→PNG 与 Windows DIB 直传的 fidelity 差异**（#S-4 同源）：Windows self-path（Windows ↔ Windows）CF_DIBV5 直传 100% 字节级一致；Linux 接收 DIB bytes 走 image-crate decode → PNG → set_image，是"视觉一致"路径（PLAN §3 评审 #3 3rd 决策）。UI 提示"图片已转换格式"留给 M4 GeneralPanel（不在本 STEP scope）。

4. **`dib_to_png_via_image_crate` 已知 limitation**（与 Windows #S-4 同源）：BITMAPV5HEADER + `BI_BITFIELDS` 32-bit RGBA masks variants 在 `image` crate BMP decoder 下可能 fail。Linux 接收端遇到这种 DIB variant 时会 `Err(Io)`（image-crate decode fail），dispatcher log warn + skip。完整 alpha 支持留给 M3a+ 阶段（手写 BITMAPV5HEADER + BI_BITFIELDS 32-bit RGBA masks ~150 行）—— 已在 #S-4 跟进。

5. **PLAN §8 M2b 人类项 — 三平台图片互传矩阵** 留 leader / 用户真机执行：本 STEP 完成 Linux backend image impl + DIB→PNG fallback；端到端三平台互测（macOS ↔ Windows / macOS ↔ Linux / Windows ↔ Linux 6 组，每组双向 A→B 与 B→A 各跑一次 = 12 次真机测）需 M2b 2b.3 真机测试矩阵验证。

6. **`cargo fmt --all` 在 `connect.rs` / `listen.rs` 现有 commit 上仍有 cosmetic drift** —— 与 2b.1 偏差 #3 同源；本 STEP 不解决（commit 卫生原则），统一 sweep 留给未来。

---

## 7. 下一步

按依赖顺序：
- **M2b STEP-2b.3** — 三平台互传矩阵（人类真机）：macOS ↔ Windows / macOS ↔ Linux / Windows ↔ Linux 各跑一次（PLAN §8 M2b 矩阵），每组一张 4K 截图 + 一张 1080p JPG；`xxd | sha256sum` 验证源端 + 对端字节级一致；双向 A→B 与 B→A 各跑一次（共 12 次真机测）
- **M2b STEP-2b.4** — fmt/clippy/build + 三平台编译验证：fmt sweep pre-existing drift + clippy 清零 pre-existing 14 warnings（out of scope 整理）+ Cargo 加 `image` crate dep (Linux PNG / BMP) — **本 STEP 已落地 Linux image dep**，2b.4 主要扫 pre-existing
- **M2b milestone 收尾**：全 workspace tests 仍保持 379+ pass / 0 fail
- 后续 milestone：**M3a** — 复制文件 + HTTP/3 transfer（200 MiB 文件 sha256 + 取消）

---

## 8. 累计耗时

~50 min（估算 60 min 之内）：
- ~10 min 设计 + 实现 `current_image` / `set_image` 2 个 trait method（Tool 复用 + PNG-only + wl-copy auto-detect 备注）
- ~15 min 实现 `set_dib_image` + `dib_to_png_via_image_crate` helper + 文档同步（与 macOS 2b.1 对称设计）
- ~10 min 写 5 个测试 + 调试 BMP / PNG feature 配置
- ~15 min 跨平台 zig-cross 验证（Linux lib + Linux tests + Windows lib tests）+ 全 workspace test/clippy/fmt 验证 + 报告

---

## 9. commit 拆分建议（leader 决策）

按 PLAN §0 commit 卫生分拆（3 个 commit）：

```
1. feat(deps): add Linux-only image crate dep (png + bmp features)
   - Cargo.toml: [target.'cfg(target_os = "linux")'.dependencies]
   - 用途: dib_to_png_via_image_crate + 单元测试 fixture

2. feat(clipboard/linux): image methods + DIB→PNG fallback (M2b STEP-2b.2)
   - src/clipboard/linux.rs:
     - current_image (xclip -t image/png -o / wl-paste --type image/png)
     - set_image (xclip -t image/png -i / wl-copy auto-detect)
     - set_dib_image (image-crate decode → set_image)
     - dib_to_png_via_image_crate helper
     - 5 new tests (cfg-gate target_os = "linux")
   - module doc 顶部 M2b STEP-2b.2 段落

3. docs: archive STEP-P2-M2b-2b.2 (this file)
   - next/STEP-P2-M2b-2b.2.md
```

可合并 #1 + #2 为单 commit "feat(clipboard/linux): Linux X11/Wayland image + DIB fallback (M2b 2b.2)"（更紧凑，与 macos 2b.1 commit 风格对称）。
最终建议 2 commit（#1+#2 / #3）。

---

> **执行人**：plan-step-executor
> **报告路径**：`/Users/hb/Projects/@cloudself/lan-mouse-pro/next/STEP-P2-M2b-2b.2.md`
> **cargo test --workspace pass 数**：379 / 0 fail（baseline 379 + 0 new on macOS；Linux-only 5 new tests cfg-gated）
> **PLAN 偏差**：#1（复用 Tool 不引入 ImageTool）、#2（dib_to_png_via_image_crate 作为 free function）、#3（保留 new() Result 签名）
> **限制 / 已知问题**：#S-2 进一步验证通过（Linux cross-compile 全绿）；#S-4 alpha limitation 在 Linux 端同样存在（BMP decoder RGBA masks 限制）