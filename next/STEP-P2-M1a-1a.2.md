# STEP-P2-M1a-1a.2 — macOS clipboard backend + Linux/Windows stubs

> **状态**：✅ 通过（macOS 实现 + Linux/Windows 占位模块 + 5 个新增单测全绿）
> **执行日期**：2026-09-08　实际耗时：~20 min
> **结论**：通过（**1 处 PLAN 偏差**：pbcopy/pbpaste 替代 NSPasteboard——已记入 SUGGESTION #S-1）

---

## 1. 做了什么

按 PLAN-2 §3 M1a STEP-1a.2 落地 macOS clipboard backend：
- **`MacOsPasteboard`**（`src/clipboard/macos.rs`）：`pbcopy` / `pbpaste` subprocess 封装（PLAN 偏差见 §3）
- **Linux / Windows 占位模块**（`src/clipboard/linux.rs` / `windows.rs`）：`#[cfg(target_os = ...)]` 守门 + 仅返回 `NotImplemented`，STEP-1a.3 替换为真实实现
- **`default_backend()`** 工厂三平台分发：`#[cfg(target_os = "macos")]` → `MacOsPasteboard::new()`；`#[cfg(any(target_os = "linux", target_os = "windows"))]` → `NotImplemented`
- **`next/SUGGESTION.md` #S-1**：PLAN 偏差记录（🟡 优先级，不阻塞 M1a）

### 1.1 文件改动

| 文件 | 改动 |
|---|---|
| `src/clipboard/macos.rs`（新，~250 行） | `MacOsPasteboard` + `#[derive(Debug)]`；`new()` 探针 `pbpaste --help`；`current_text` 读 + `set_text` 写（pbcopy via piped stdin）；5 个单测（4 round-trip + 1 name + 1 constructor） |
| `src/clipboard/linux.rs`（新，~35 行） | `LinuxClipboard` stub（STEP-1a.3 替换） |
| `src/clipboard/windows.rs`（新，~35 行） | `WinClipboard` stub（STEP-1a.3 替换） |
| `src/clipboard/mod.rs` | `default_backend()` 改为 `#[cfg(...)]` 三平台分发；`#![allow(dead_code)]` 移除；`tests::default_backend_returns_not_implemented...` 拆为 macOS / linux-windows 两个 cfg-gated 测试 |
| `next/SUGGESTION.md` | 加 #S-1（PLAN 偏差：pbcopy/pbpaste 而非 NSPasteboard；🟡） |

### 1.2 关键设计

#### macOS：pbcopy/pbpaste subprocess
- `current_text()`：`Command::new("pbpaste").output()` → `String::from_utf8(stdout).ok()`；非非 0 exit → `None`（pbpaste 对 image clipboard 返回 1，dispatcher 把 `None` 当作 "skip this tick"——正确）
- `set_text()`：`Command::new("pbcopy").stdin(Stdio::piped()).spawn()` → `write_all(text.as_bytes())` → `drop(stdin)` (EOF) → `wait()`
- `cached: Option<String>`：信息性缓存；dispatcher 不用，未来 log 可对比 "I just wrote X" vs "X was already there"
- `#[derive(Debug)]`：测试需要 `{:?}` 格式化；shadow_rs crate 限制需要 derive

#### Linux/Windows stubs
- 仅暴露 `pub struct LinuxClipboard;` / `pub struct WinClipboard;`（cfg 守门）
- 实现 `ClipboardBackend` 但 `current_text → None` / `set_text → Err(NotImplemented)`
- 让 `cargo build -p lan-mouse` 在所有 3 平台同时通过（CI matrix 要求）

#### `default_backend()` 工厂
- `#[cfg(target_os = "macos")]` → 构造 `MacOsPasteboard`（动态分发）
- `#[cfg(any(target_os = "linux", target_os = "windows"))]` → `Err(NotImplemented)`（STEP-1a.3 改）
- `#[cfg(not(any(target_os, ...)))]`（其它 unix）→ `Err(NotImplemented)`

### 1.3 并行测试修复（pbcopy/pbpaste 真实 OS 剪贴板）

`pbcopy` / `pbpaste` 改用户**真实** OS 剪贴板；`cargo test` 默认并行跑多个 round-trip 测试会互相覆盖写入，导致 flaky fail。

**修复**：`static CLIPBOARD_TEST_LOCK: Mutex<()> = Mutex::new(())` —— 每个 round-trip 测试 `let _lock = lock_for_test()` 拿到全局互斥，把整段 set→read 串行化；`lock_for_test()` 用 `unwrap_or_else(|e| e.into_inner())` 容忍中毒（panic-in-guard 仍可继续）。

加 `ClipboardGuard` RAII：`Drop` 时恢复原始剪贴板内容 —— 防止 round-trip 测试留下脏数据给真实 macOS 用户。

### 1.4 测试结果

- `cargo build -p lan-mouse`：✅ 0 error / 0 warning（移除模块级 `#![allow(dead_code)]` —— 现在 trait / dummy / macOS / stubs / factory 都有人用）
- `cargo test -p lan-mouse --lib clipboard`：✅ **11 passed / 0 failed**（8 trait/dummy + 5 macOS + 1 cfg-gated default_backend）
- `cargo test -p lan-mouse --lib`：✅ **121 passed / 0 failed**（基线 116 + macOS 5）
- 关键单测：
  - `set_text_then_current_text_round_trip` —— 真实 pbcopy/pbpaste round-trip（`lan-mouse M1a round-trip test 1a.2`）
  - `set_text_empty_string_clears_clipboard` —— 空字符串清空剪贴板（empty ≠ None 的 dispatcher 依赖）
  - `set_text_multibyte_utf8_round_trip` —— CJK + emoji + é 字节级一致
  - `default_backend_returns_macos_impl_after_step_1a_2` —— 工厂返回 macOS 实现（替代旧的 "永远 NotImplemented" 测试）

## 2. 累计耗时

~20 min

## 3. PLAN 偏差

**1 处**：#S-1 macOS 用 pbcopy/pbpaste 而非 NSPasteboard + changeCount。理由：
1. NSPasteboard 直调需加 `objc2` + `objc2-app-kit` macOS-only 依赖（+6-10MB + +30s 编译）
2. `changeCount` 优化对 500ms tick + sha256 fingerprint 短路无价值
3. pbcopy/pbpaste 与 Linux xclip/wl-paste（1a.3）结构同构，跨平台心智模型一致

迁移路径：M2a+ 若需 NSPasteboard 的 `.tiff` 直接读 / `NSFilenamesPboardType` 文件列表 → 加 `objc2`。SUGGESTION #S-1 记录。

## 4. 下一步

→ STEP-1a.3（Windows `windows-sys` + Linux `tokio::process` xclip/wl-paste 替换占位 stub；Linux Wayland 探测 fallback 顺延 M2b）
→ 或按 leader 派发继续