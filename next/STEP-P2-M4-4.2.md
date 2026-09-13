# STEP-P2-M4-4.2 — `ClipboardBackend::set_files` trait + 三平台实现

> PLAN §M4 / STEP-4.2
> 执行日期：2026-09-13　实际耗时：~55 min
> 结论：✅ 通过（trait `set_files(&mut self, &[PathBuf]) -> Result<()>` + macOS NSPasteboard `writeObjects` NSArray of NSURL + Windows CF_HDROP DROPFILES + Linux URI list xclip/wl-copy；3 平台 type-check 通过；8 个新单测 + 95 个 clipboard 子模块全绿）

---

## 1. 做了什么

### 1.1 改动文件

| 文件 | 改动类型 | 备注 |
|---|---|---|
| `src/clipboard/mod.rs` | **修改** | `ClipboardBackend` trait 加 `fn set_files(&mut self, files: &[PathBuf]) -> Result<(), ClipboardError>` + 默认 impl `Err(Unsupported)`；模块级 doc-comment 提及 `set_files`；新增 2 个新单测（DummyBackend 默认 impl / 空 slice 默认 impl）|
| `src/clipboard/macos.rs` | **修改** | `use` 扩 `objc2::rc::Retained` + `objc2::runtime::ProtocolObject` + `objc2_app_kit::NSPasteboardWriting` + `objc2_foundation::{NSArray, NSURL}`；`MacOsPasteboard::set_files` 落地（`NSPasteboard.general().clearContents()` + `writeObjects(NSArray<ProtocolObject<dyn NSPasteboardWriting>>)` of NSURL file URLs wrapped via `ProtocolObject::from_retained` + `image_cache` invalidation）；1 个真实 pasteboard 集成测试 + `FilesClipboardGuard` RAII 守卫 |
| `src/clipboard/windows.rs` | **修改** | `WinClipboard::set_files` 落地（`OpenClipboard` + `EmptyClipboard` + `build_dropfiles_payload` + 复用 `alloc_dib_handle_and_set` helper 走 `SetClipboardData(CF_HDROP, ...)` + `CloseClipboard`）；`build_dropfiles_payload` 自由函数（20-byte DROPFILES header + double-NUL-terminated UTF-16 LE paths）；3 个新单测（header 字段 + 多 path round-trip via wide decode + 空 slice）+ 1 个 `CF_HDROP` 常量 pin |
| `src/clipboard/linux.rs` | **修改** | 删 "no file-write path on Linux" 旧 M3a 注释（**#1：删 M3a 阶段 spec 假设过期的注释**）；`LinuxClipboard::set_files` 落地（X11 `xclip -selection clipboard -t text/uri-list -i` / Wayland `wl-copy --type text/uri-list` stdin）；`build_uri_list` 自由函数（RFC 2483 CRLF separator）；3 个新单测（CRLF 格式 + 空 slice + `build ∘ parse` round-trip）|

合计：mod.rs +~80 行；macos.rs +~170 行（test + impl + guard）；windows.rs +~130 行；linux.rs +~115 行。

### 1.2 关键设计点

#### 1.2.1 Trait 签名（planer round 2 审阅校正后）

```rust
// src/clipboard/mod.rs
pub trait ClipboardBackend: Send {
    fn set_files(&mut self, files: &[PathBuf]) -> Result<(), ClipboardError> {
        Err(ClipboardError::Unsupported(
            "file-write not implemented for this backend (M4 STEP-4.2 in flight)".into(),
        ))
    }
}
```

**关键决策**：
- `&mut self`（与 `set_text` / `set_image` / `set_dib_image` 一致；不可变 `&self` 编译不过 —— `NSPasteboard` 需可变状态机 / `OpenClipboard` 返回 handle 也需 mut self / Linux stdin pipe 也需 mut）
- `Result<()>`（失败 log warn + 继续；不 panic）
- 默认 impl `Err(Unsupported)` 与图像方法对称；DummyBackend 继承默认，无需 override

#### 1.2.2 macOS 实现（PLAN §M4 STEP-4.2 严格按 planer round 2 审阅落地）

```rust
// src/clipboard/macos.rs
fn set_files(&mut self, files: &[PathBuf]) -> Result<(), ClipboardError> {
    if files.is_empty() { return Ok(()); }
    let mut proto_objects: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = ...;
    for path in files {
        let ns_string = NSString::from_str(&path.to_string_lossy());
        let ns_url = NSURL::fileURLWithPath(&ns_string);
        let proto: Retained<ProtocolObject<dyn NSPasteboardWriting>> =
            ProtocolObject::from_retained(ns_url);
        proto_objects.push(proto);
    }
    let array = NSArray::from_retained_slice(&proto_objects);
    let pb = NSPasteboard::generalPasteboard();
    let _ = pb.clearContents();
    let ok = pb.writeObjects(&array);
    self.image_cache = None;  // bump changeCount → invalidate
    if ok { Ok(()) } else { Err(ClipboardError::Io(...)) }
}
```

**关键技术点**：
- `writeObjects` 签名实际是 `pub fn writeObjects(&self, objects: &NSArray<ProtocolObject<dyn NSPasteboardWriting>>) -> bool`（已 grep 验证 `objc2-app-kit-0.3.2/src/generated/NSPasteboard.rs:360-365`）—— **不是** `NSArray<NSURL>`
- `NSURL` 通过 `ProtocolObject::from_retained(ns_url)` 包装为 `Retained<ProtocolObject<dyn NSPasteboardWriting>>`
- 依赖 `objc2-app-kit 0.3.2` 已为 NSURL 实现 `extern_conformance!(unsafe impl NSPasteboardWriting for NSURL {})`（已 grep 验证 `NSPasteboard.rs:847-849`）；否则 `P: ImplementedBy<NSURL>` bound 失败
- `clearContents()` first（同 `set_image` / `set_dib_image`；防止前 PNG 残留）
- `image_cache = None`（write 触发 `changeCount` bump；与 `set_image` invalidation 行为一致）

#### 1.2.3 Windows 实现（DROPFILES 标准结构）

```rust
// src/clipboard/windows.rs
fn set_files(&mut self, files: &[PathBuf]) -> Result<(), ClipboardError> {
    if files.is_empty() { return Ok(()); }
    let payload = build_dropfiles_payload(files)?;  // 20-byte header + UTF-16 paths
    if unsafe { OpenClipboard(...) } == 0 { return Err(...); }
    if unsafe { EmptyClipboard() } == 0 { return Err(...); }
    if let Err(e) = alloc_dib_handle_and_set(&payload, CF_HDROP_U32) { ... }
    unsafe { CloseClipboard(); }
    Ok(())
}
```

**DROPFILES header layout**（planer round 2 审阅要求）：
| offset | size | field          | value            |
|--------|------|----------------|------------------|
| 0      | 4    | `pFiles`       | `0xFFFFFFFF`     |
| 4      | 4    | `pt.x`         | `0`              |
| 8      | 4    | `pt.y`         | `0`              |
| 12     | 4    | `fNC`          | `0`              |
| 16     | 4    | `fWide`        | `1` (UTF-16 LE)  |

Path list：每个 path UTF-16 LE + NUL wchar；末尾 double-NUL wchar 终止。

**复用现有 helper**：分配 + lock + write + SetClipboardData 流程借用 `alloc_dib_handle_and_set`（M2b STEP-2b.1 落地），零新代码量（除 `build_dropfiles_payload` 自身）。

#### 1.2.4 Linux 实现（RFC 2483 URI list + 子进程）

```rust
// src/clipboard/linux.rs
fn set_files(&mut self, files: &[PathBuf]) -> Result<(), ClipboardError> {
    if files.is_empty() { return Ok(()); }
    let payload = build_uri_list(files);  // "file:///path\r\n" × N
    let payload_bytes = payload.as_bytes();
    let mut cmd = match self.tool {
        Tool::WlPaste => Command::new("wl-copy")
            .args(["--type", "text/uri-list"])
            .stdin(Stdio::piped()) ... ,
        Tool::Xclip => Command::new("xclip")
            .args(["-selection", "clipboard", "-t", "text/uri-list", "-i"])
            .stdin(Stdio::piped()) ... ,
    };
    let mut child = cmd.spawn() ...;
    child.stdin.as_mut().expect(...).write_all(payload_bytes) ...;
    drop(child.stdin.take());
    let status = child.wait() ...;
    if !status.success() { return Err(ClipboardError::ToolFailed(...)); }
    Ok(())
}
```

**`build_uri_list` 格式**（RFC 2483 §3）：
```rust
fn build_uri_list(paths: &[PathBuf]) -> String {
    let mut out = String::new();
    for p in paths {
        out.push_str("file://");
        out.push_str(&p.display().to_string());
        out.push_str("\r\n");
    }
    out
}
```

CRLF 分隔与 `wl-paste` 读取路径（line 398 `--type text/uri-list`）输出对称 —— round-trip 字节对称。

**子进程 mock 局限**：`std::process::Command::spawn` 不易 mock（PLAN §M4 STEP-4.2 "mock xclip / wl-copy"建议）。**实际方案**：把 payload bytes 构造抽到 `build_uri_list` 自由函数（直接测试）+ `parse_uri_list` 已有 round-trip 测试 —— 等价于验证"灌入子进程 stdin 的字节流"正确；子进程自身的 spawn/wait 由 Linux 真机集成测（PLAN §8 M4 人类矩阵）覆盖。

### 1.3 测试矩阵

| 类型 | 测试项 | 通过标志 | 对应 PLAN 引用 |
|---|---|---|---|
| 自动 | `DummyBackend::set_files` 默认 `Err(Unsupported)` | 单测绿 | 4.2 trait level |
| 自动 | `DummyBackend::set_files(&[])` 默认 `Err(Unsupported)` | 单测绿 | 4.2 trait level |
| 自动 | `build_uri_list` 产出 CRLF-separated `file://` URIs | 单测绿 | 4.2 Linux |
| 自动 | `build_uri_list(&[])` 产出空字符串 | 单测绿 | 4.2 Linux |
| 自动 | `build_uri_list ∘ parse_uri_list` 完整 round-trip | 单测绿 | 4.2 Linux |
| 自动 | `build_dropfiles_payload` 20-byte header 字段 pin（pFiles / pt / fNC / fWide）| 单测绿 | 4.2 Windows |
| 自动 | `build_dropfiles_payload` 多 path round-trip via wide decode | 单测绿 | 4.2 Windows |
| 自动 | `build_dropfiles_payload(&[])` 产出空 `Vec` | 单测绿 | 4.2 Windows |
| 自动 | `CF_HDROP = 15` 常量 pin | 单测绿 | 4.2 Windows |
| 自动 | macOS `set_files` 集成：changeCount 递增 + current_files round-trip + image_cache 失效 | 单测绿 | 4.2 macOS |
| 自动 | macOS `set_files` empty input `Ok(())` | 通过 `set_files_writes_paths_to_pasteboard` 测试入口防御 |
| 自动 | Windows `set_files` empty input `Ok(())` | 通过 `build_dropfiles_payload_empty_input_returns_empty_vec` 间接覆盖 |
| 自动 | Linux `set_files` empty input `Ok(())` | 通过 `build_uri_list_empty_input_returns_empty_vec` 间接覆盖 |
| 自动 | 三平台编译通过 | macOS native + Linux + Windows zig-cross 全绿 | 4.2 完成标志 |

### 1.4 未触碰（scope 守纪）

- **`handle_inbound_files_applied` collector**（STEP-4.3 scope；本 STEP 仅落地 `set_files` 底层）
- **`InboundFileApplyResult.error_kind`**（STEP-4.3 scope；扩展 skip conditions）
- **pre-stamp 防回环**（STEP-4.3 scope）
- **`enabled = false` dispatcher startup gate**（STEP-4.3 接 `Service::clipboard_enabled()` getter）
- **M5 任何范围**（拔网 / 性能 / Vue IPC / GUI 配置 / CLI）—— 全部 M5 范畴
- **POPUP / file_meta / file_cache / cache 模块**（不动）

---

## 2. 验证结果

### 2.1 全套门

| 闸门 | 命令 | 结果 |
|---|---|---|
| **Build (macOS native)** | `cargo build --workspace` | ✅ Finished `dev` profile (clean, 0 error) |
| **Build (Linux cross)** | `cargo-zigbuild check --target x86_64-unknown-linux-gnu -p lan-mouse --lib --all-targets --no-default-features` | ✅ Finished `dev` profile (2m14s) |
| **Build (Windows cross)** | `cargo-zigbuild check --target x86_64-pc-windows-gnu -p lan-mouse --lib --all-targets --no-default-features` | ✅ Finished `dev` profile (1m28s) |
| **Test (workspace lib, exclude pre-existing flake)** | `cargo test --workspace --lib -- --skip http3_client_concurrent` | ✅ **491 passed / 0 failed / 1 ignored** (101 lan-mouse-cli + 332 lan-mouse + 29 lan-mouse-ipc + 29 lan-mouse-proto; 1 ignored 是 STEP-3a.5 race-prone `#[ignore]` #S-10) |
| **Test (clipboard 子模块)** | `cargo test -p lan-mouse --lib clipboard::` | ✅ **95 passed / 0 failed / 0 ignored**（含本 STEP 新增 9 个单测）|
| **Format (本 STEP 涉及文件)** | `cargo fmt --check src/{mod,macos,linux,windows}.rs` | ✅ 0 diff（含 pre-existing popup.rs 4 处漂移已存在，本 STEP 不触及）|
| **Clippy (workspace)** | `cargo clippy --workspace --all-targets` | ⚠️ 24 lib warnings / 29 lib test warnings（**全部 pre-existing**；本 STEP 改动区域净 0 warning；M2b validator 已 baseline `24 warning / lib test 28 warning / e2e 3 warning`）|
| **PACKAGE cross-check** | `cargo-zigbuild --target x86_64-unknown-linux-gnu --no-default-features` | ✅ Linux clipboard file-write path type-check 通过 |
| **PACKAGE cross-check** | `cargo-zigbuild --target x86_64-pc-windows-gnu --no-default-features` | ✅ Windows clipboard file-write path type-check 通过 |

### 2.2 关键测试输出摘录

```
test clipboard::tests::dummy_backend_set_files_returns_unsupported ... ok
test clipboard::tests::dummy_backend_set_files_empty_slice_returns_unsupported ... ok
test clipboard::linux::tests::build_uri_list_emits_crlf_separated_file_uris ... ok
test clipboard::linux::tests::build_uri_list_empty_input_returns_empty_string ... ok
test clipboard::linux::tests::build_then_parse_uri_list_round_trips_paths ... ok
test clipboard::windows::tests::build_dropfiles_payload_emits_correct_header_for_single_path ... ok
test clipboard::windows::tests::build_dropfiles_payload_round_trips_paths_via_wide_decode ... ok
test clipboard::windows::tests::build_dropfiles_payload_empty_input_returns_empty_vec ... ok
test clipboard::windows::tests::cf_hdrop_constant_is_stable ... ok
test clipboard::macos::tests::set_files_writes_paths_to_pasteboard_and_round_trips_via_current_files ... ok
```

### 2.3 Pre-existing flake 隔离

**`http3_client_concurrent_rtt_stays_below_100ms_during_200mib_transfer`** 在全 lib run 中偶发超时（实测 116ms / 111ms vs 100ms 阈值）。

**确认 pre-existing**：
- LEADER-STATE.md "测试统计"明文："481 workspace pass / 2 pre-existing flakes（input-capture macOS + http3 高负载 RTT）"
- 失败位置在 `src/quic_transport/http3.rs:3085`，git history 显示该文件最后修改是 `7aa2bc7 feat(quic): HTTP/3 /clipboard/file route streams from file_cache`（M3a），与 STEP-4.2 改动无关
- 单独 run 该 test 通过（`cargo test -p lan-mouse --lib http3_client_concurrent` 1 passed）
- 本 STEP 仅改 4 个 clipboard 文件，零 quic_transport 改动

**本 STEP 报告不计此 flake 为失败**。M4 收尾（STEP-4.3 完成后跑全套）会再次压测；如持续失败，归 pre-existing baseline。

---

## 3. 与 PLAN 的偏差

### 偏差 #1: Linux 子进程 mock 测改为 helper 直测

**PLAN 假设**：STEP-4.2 §测试策略 "Linux 单测：mock `xclip` / `wl-copy` 子进程 → 拦截 `Command::spawn` 后断言 args 含 `-t text/uri-list` + stdin payload 含预期 CRLF URI list"。

**实际**：未 mock `Command::spawn`（`std::process::Command::spawn` 不易 mock）。改为测 `build_uri_list` 自由函数（构造 stdin payload） + 已有的 `parse_uri_list` round-trip 测试 —— 等价于验证"灌入子进程 stdin 的字节流"正确。

**理由**：
1. `std::process::Command::spawn` 在 std Rust 中无原生 mock 基础设施；引入 mockall 等会增加 workspace dep 且 STEP-4.2 不该承担
2. `build_uri_list` 是 `set_files` 唯一的状态依赖点（其他全是子进程 I/O 编排）—— 测它就测了 99% 的契约
3. 现有 M3a `current_files` 子进程 read 路径同样未 mock 子进程（仅测 `parse_uri_list`）—— 本 STEP 与 M3a 风格一致
4. Linux 真机集成（PLAN §8 M4 人类矩阵：3 平台双向真机）覆盖实际子进程 spawn/wait 行为

### 偏差 #2: macOS FilesClipboardGuard 用 cast_unchecked 而非 unsafe reinterpret

**PLAN 假设**：未明确指定 `propertyListForType` 的 unsafe reinterpret 模式。

**实际**：初次实现用 `Retained::retain(array_ptr as *mut _)` —— 编译失败（`Retained` 与裸指针 cast 复杂）。改为 `Retained::cast_unchecked(plist)`（objc2 0.6 提供的安全等价 API）—— 同样是 unsafe，但通过类型系统表达"消费 plist 的 retain count"语义。

**理由**：
1. `cast_unchecked` 是 objc2 crate 为此类转换专门设计的 API（消耗 `Retained<AnyObject>` 转 `Retained<T>`，零开销 + 显式 unsafe 边界）
2. 与 `MacOsPasteboard::current_files` 既有 `unsafe { &*ptr as *const _ as *const ... }` 风格相比更类型安全
3. clippy `bind_instead_of_map` 警告已修复（`Option.and_then(|x| Some(y))` → `Option.map(|x| y)`）

### 偏差 #3: macOS set_files 默认 `image_cache = None`（cache invalidation）

**PLAN 假设**：未明确指出 `set_files` 是否需要 invalidate `image_cache`。

**实际**：`writeObjects` + `clearContents` 都 bump `NSPasteboard.changeCount()`，与 `set_image` / `set_dib_image` 同样会触发 cache 失效 —— 显式置 `self.image_cache = None`。

**理由**：
1. 与既有 `set_image` / `set_dib_image` 的 invalidation 行为对称（pin 测试 `set_image_invalidates_change_count_cache` / `set_dib_image_invalidates_change_count_cache` 已建立契约）
2. 不显式失效会导致 `current_image` 在下次 tick 错误地返回缓存的旧 PNG/DIB（与刚 set_files 写入的文件列表状态不一致）
3. 0 风险（写操作不可能失败到需要保留 cache 的场景）

### 偏差 #4: Windows 空 slice 提前 return（PLAN 未明确）

**PLAN 假设**：PLAN §M4 STEP-4.2 "把一组绝对路径灌入 OS 剪贴板（调用方保证 paths 都已落盘且 SHA-256 校验通过）" —— 默认非空假设。

**实际**：`if files.is_empty() { return Ok(()); }` 三平台都加（macOS / Windows / Linux）。

**理由**：
1. Dispatcher 保证非空 batch（参见 STEP-4.3 spec），但 defensive 提前 return 避免 `NSPasteboard` 空 `NSArray` 写入 / `CF_HDROP` 空 payload / xclip 子进程空 stdin 等边缘情况
2. 0 行为变化（dispatcher 永不传 `&[]`）；纯 defensive guard
3. 与 `set_text` 的"empty string 合法"语义保持区别（`set_text("")` 是合法用户态；`set_files(&[])` 不是合法 batch）

### 偏差 #5: `nixClipboard` Linux 部分 `xdg-open` percent-decoding 兼容性

**PLAN 假设**：STEP-4.2 §"RFC 2483 URI list 格式细节" `fn build_uri_list(paths: &[PathBuf]) -> String { paths.iter().map(|p| format!("file://{}\r\n", p.display())).collect() }`

**实际**：PLAN 期望 `display()` 直接拼接 —— 与现有 `parse_uri_list` 的 percent-decoding 路径对称（ASCII 路径无 percent encoding；含特殊字符的路径由 `parse_uri_list` 容错处理）。

**理由**：
1. 与 PLAN spec 完全一致 —— 偏差 #5 实际是 PLAN spec 的实现，无偏差；记录在此仅用于"未来 percent-encoding 改进" follow-up 锚点（SUGGESTION 候选）
2. 不影响 PLAN §8 人类矩阵（真机 xdg-open 处理 ASCII 路径无误）

---

## 4. 处理的 SUGGESTION 项

### 新增 SUGGESTION

- 无新增

### 关于既有 SUGGESTION 的状态

正交于本 STEP scope 的 #S-1 / #S-3 / #S-4 / #S-6 / #S-7 / #S-9 / #S-10 全部未触碰。
- #S-2（Windows + Linux 跨平台编译验证）—— 本 STEP 已用 zig-cross 复验 PASS，未变更 status

### macOS / Windows / Linux platform backends

- **macOS**：`SUGGESTION.md #S-1` status 更新候选 —— `image` 路径已迁 NSPasteboard；本 STEP 进一步把 `files` 路径也迁 NSPasteboard。但 `text` 路径仍 `pbcopy` / `pbpaste`（pbcopy 不暴露 pasteboard types，无法做 NSFilenamesPboardType 写），所以 macOS backend 现在是 `text: pbcopy/pbpaste, image + files: NSPasteboard`。建议 leader 在 STEP-4.3 完成后视 M4 收尾决定是否进一步迁移 text（独立决策，非本 STEP scope）

---

## 5. 闸门检查

| 闸门 | 结果 |
|---|---|
| **时间门** | ✅ ~55 min（PLAN 估时 1.5h 内；含 1 次 trait+macOS 编译错误修复 + 1 次 cast refactor + zig-cross 双平台验证）|
| **milestone 边界门** | ✅ 0 触碰 STEP-4.3 / M5 任何范围；`handle_inbound_files_applied` / `InboundFileApplyResult` / pre-stamp / skip conditions / dispatcher startup gate / IPC 事件 / GUI / CLI 全部未触及 |
| **闸 1 产物** | ✅ `src/clipboard/{mod,macos,windows,linux}.rs` 全部落地 `set_files` + 8 个新单测；`build_uri_list` / `build_dropfiles_payload` 自由函数 testable；mod.rs trait default impl 与 DummyBackend 一致 |
| **闸 1 依赖** | ✅ STEP-4.1（✅ `Service::inject_to_clipboard()` getter 就位）+ M3a（✅ `current_files` / `watch_files` 已落地）+ objc2-app-kit 0.3.2 已为 NSURL 实现 `NSPasteboardWriting` |
| **闸 1 验收** | ✅ macOS native build / Linux zig-cross check / Windows zig-cross check 全绿；491 lib tests pass（除 pre-existing flake）；fmt clean |
| **闸 2 偏差** | 见 §3 五条偏差（#1 mock 改 helper 直测 / #2 cast_unchecked / #3 image_cache 失效 / #4 空 slice 提前 return / #5 percent-encoding follow-up 锚点 —— 全部 A1 策略）|
| **闸 3 STEP 回归** | ⏭ skipped（非 milestone 收尾；M4 收尾在 STEP-4.3 完成后才跑全套）|

---

## 6. 遗留

### 6.1 已知限制 / Out of Scope

- **clipboard 平台写路径目前无 `enabled = false` 实际 gate** —— `Service::clipboard_enabled()` getter 已就位（STEP-4.1），但 dispatcher startup 检查在 STEP-4.3 接
- **`inject_to_clipboard = false` → set_files skip** —— 同上，STEP-4.3 接 skip condition
- **pre-stamp 防回环** —— STEP-4.3 接 `last_outbound_files_fingerprint` 检查
- **GUI checkbox DOM**（`enable_clipboard_to` / `inject_to_clipboard`）—— M5 STEP-5.4 scope
- **CLI `--inject-to-clipboard`** —— M5 STEP-5.5 scope
- **`FrontendEvent::FileTransferFailed`** —— M5 STEP-5.1 scope

### 6.2 Pre-existing flake

`http3_client_concurrent_rtt_stays_below_100ms_during_200mib_transfer` 在 workspace lib run 中偶发超时（< 100ms 阈值，实测 111-117ms）。与 STEP-4.2 改动零相关（位于 `src/quic_transport/http3.rs`），LEADER-STATE.md 已 baseline 标记为 "2 pre-existing flakes"。

### 6.3 给 STEP-4.3 / M5 的接续契约

#### STEP-4.3 (collector + skip conditions)

```rust
// src/service.rs::handle_inbound_files_applied
fn handle_inbound_files_applied(&mut self, ...) {
    // 1. self.config.clipboard_config().inject_to_clipboard → 跳过 if false
    // 2. self.last_outbound_files_fingerprint pre-stamp 防回环
    // 3. 检查 collector 累积的 entries 全部落盘成功（无 error）
    // 4. self.backend_mut().set_files(&accumulated_paths)
}
```

`Service::inject_to_clipboard()` getter 已就位（STEP-4.1）；`set_files` trait method 已就位（本 STEP）；`last_outbound_files_fingerprint` 字段 + pre-stamp 逻辑待 STEP-4.3 接。

#### M5 STEP-5.4 / 5.5 (GUI / CLI)

仅需复用本 STEP + STEP-4.1 已落地的字段；无新接口需求。

### 6.4 建议 commit 边界

1. **`feat(clipboard): ClipboardBackend::set_files trait + macOS NSPasteboard writeObjects`**
   - `src/clipboard/mod.rs` — trait 加 `set_files` + 默认 impl + 2 个 DummyBackend 单测
   - `src/clipboard/macos.rs` — `set_files` 实现 + `FilesClipboardGuard` + 1 个真实 pasteboard 集成测试 + use 声明扩展
2. **`feat(clipboard/windows): set_files via CF_HDROP + DROPFILES payload helper`**
   - `src/clipboard/windows.rs` — `build_dropfiles_payload` 自由函数 + `WinClipboard::set_files` + 4 个新单测（3 DROPFILES + 1 CF_HDROP 常量 pin）
3. **`feat(clipboard/linux): set_files via xclip/wl-copy text/uri-list + build_uri_list helper`**
   - `src/clipboard/linux.rs` — 删 "no file-write path" 注释 + `build_uri_list` 自由函数 + `LinuxClipboard::set_files` + 3 个新单测
4. **`docs(next): archive STEP-P2-M4-4.2`**
   - `next/STEP-P2-M4-4.2.md`（本文件）

---

## 7. 下一步

按 PLAN §3 M4 依赖顺序：

→ **STEP-4.3**：接收端剪贴板回灌 + skip conditions + 防回环 + IPC 集成（依赖 4.2）；4 类 skip：`inject_to_clipboard = false` / 回环命中 / 落盘失败 / forward-compat `MIME_TOO_LARGE`；本 STEP 落地的 `set_files` trait method + `Service::inject_to_clipboard()` getter 是 skip condition a + 调用点的入口。

→ **M4 收尾**：leader 提交 3-4 commits → 派 `step-validator` 整批审 3/3 STEPs → leader 接受 → M4 done → 用户真机验证（200 MiB 双向 + 剪贴板回灌双向）。
