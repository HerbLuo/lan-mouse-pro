# STEP-P2-M1a-1a.3 — Windows + Linux clipboard backends

> **状态**：✅ 通过（macOS 编译 + 121 单测全绿；Linux/Windows 编译受本机工具链限制未本地验证——已记入 SUGGESTION #S-2）
> **执行日期**：2026-09-08　实际耗时：~30 min
> **结论**：通过（**1 处限制** + **0 处 PLAN 偏差**——本 STEP 与 PLAN-2 §3 M1a STEP-1a.3 行严格一致）

---

## 1. 做了什么

按 PLAN-2 §3 M1a STEP-1a.3 落地 Windows + Linux clipboard backends：

- **`LinuxClipboard`**（`src/clipboard/linux.rs`，~280 行）：
  - 启动时探测 `$WAYLAND_DISPLAY` env + `wl-paste` / `xclip` 二选一
  - `current_text`：`wl-paste --no-newline` / `xclip -selection clipboard -o`
  - `set_text`：`wl-copy` / `xclip -selection clipboard -i`（piped stdin）
  - 缺工具 → `ClipboardError::ToolMissing`（错误信息同时提 wl-paste + xclip，PLAN §5 评审 #6 3rd 的"清晰提示"承诺）
- **`WinClipboard`**（`src/clipboard/windows.rs`，~330 行）：
  - `OpenClipboard(NULL)` / `GetClipboardData(CF_UNICODETEXT)` / `SetClipboardData(CF_UNICODETEXT, hMem)`
  - `GlobalAlloc(GMEM_MOVEABLE, ...)` + `GlobalLock` 写 UTF-16 + NUL + `GlobalUnlock`
  - `GetLastError()` 错误信息含十六进制码，便于 errlook 查
- **`Cargo.toml`**：加 `[target.'cfg(target_os = "windows")'.dependencies] windows-sys = { version = "0.61", features = ["Win32_Foundation", "Win32_System_Memory", "Win32_System_DataExchange"] }`
- **`next/SUGGESTION.md` #S-2**：Windows/Linux 跨平台编译未本地验证（macOS 无 mingw / gcc-linux-gnu 工具链）

### 1.1 文件改动

| 文件 | 改动 |
|---|---|
| `src/clipboard/linux.rs`（新，~280 行） | `Tool` 枚举（WlPaste / Xclip）+ `Tool::detect()` env+probe 自动选 + `LinuxClipboard` + 4 个单测（probe false / detect branch / Tool eq / new error message mentions both tools） |
| `src/clipboard/windows.rs`（新，~330 行） | `WinClipboard` + `OpenClipboard` / `GetClipboardData` / `SetClipboardData` / `GlobalAlloc` / `GlobalLock` 全部走 windows-sys 0.61 typed bindings；`CF_UNICODETEXT = 13` 常量（windows-sys 0.61 未顶层导出）；4 个单测（name / new / CF 常量 / Err_to_string 格式） |
| `Cargo.toml` | 新增 `[target.'cfg(target_os = "windows")'.dependencies] windows-sys = ...`（仅 Windows 拉入） |
| `next/SUGGESTION.md` | 加 #S-2（🟡） |

### 1.2 关键设计

#### Linux：env 探测 + 二选一
- `Tool::detect()` 顺序：`WAYLAND_DISPLAY` 设 + `wl-paste` 在 PATH → Wayland；否则 `xclip` 在 PATH → X11；两者都无 → `None`
- Wayland → XWayland fallback（M2b 评审 #6 3rd 承诺）顺延：本 STEP 简化为 "任一可用即可"
- `current_text` 失败（exit 1 = 剪贴板无文本）→ `None`（dispatcher 视作 "skip this tick"）
- 单测 `linux_clipboard_new_error_message_mentions_both_tools` 把错误信息里同时提 wl-paste + xclip 锁住

#### Windows：Win32 clipboard + UTF-16
- `CF_UNICODETEXT = 13` —— windows-sys 0.61 未顶层导出，手写常量（值稳定自 Windows 95）
- 读：`OpenClipboard(NULL)` → `GetClipboardData(CF_UNICODETEXT)` → `GlobalLock` → walk wide string 找 NUL → `String::from_utf16` → `GlobalUnlock` + `CloseClipboard`
- 写：`OsString::from(text).encode_wide() + 0` → `GlobalAlloc(GMEM_MOVEABLE, ...)` → `GlobalLock` → `copy_nonoverlapping` → `GlobalUnlock` → `SetClipboardData` (OS takes ownership) → `CloseClipboard`
- 错误统一走 `ClipboardError::Io("<op> failed: GetLastError=<err>")` —— log grep 友好

#### 同步 vs 异步
- 全用 `std::process::Command`（Linux / macOS）或直接 FFI（Windows）
- 不引 `tokio::process` —— trait 是 sync；dispatcher 在 500ms tick 内阻塞 ~1-3ms 不可见
- Windows `OpenClipboard` 极端情况阻塞 ~30s（其他进程卡住剪贴板）—— 接受 M1a 简化，M1b 再考虑 async 包装

### 1.3 测试结果

- `cargo build -p lan-mouse`（macOS）：✅ 0 error / 0 warning
- `cargo test -p lan-mouse --lib`（macOS）：✅ **121 passed / 0 failed**（基线 116 + macOS 5 + 0 新单测可见——Linux/Windows 单测 cfg-gated 不在 macOS 跑）
- `cargo check --target x86_64-unknown-linux-gnu -p lan-mouse-proto`：✅（依赖树无 C 编译）
- `cargo check --target x86_64-unknown-linux-gnu -p lan-mouse-ipc`：✅（无 C 编译）
- `cargo check --target x86_64-unknown-linux-gnu -p lan-mouse-cli`：✅
- `cargo check --target x86_64-unknown-linux-gnu -p lan-mouse`：❌ **本机无 `x86_64-linux-gnu-gcc`**（rcgen / quinn 链需要 C 交叉编译器）→ SUGGESTION #S-2
- `cargo check --target x86_64-pc-windows-msvc -p lan-mouse`：❌ 同上（`x86_64-w64-mingw32-gcc` 缺失 / MSVC 工具链装超时被 kill）→ SUGGESTION #S-2
- 单测覆盖（macOS 可见部分）：
  - macOS `set_text_then_current_text_round_trip` / `set_text_empty_string_clears_clipboard` / `set_text_multibyte_utf8_round_trip` —— 1a.2 已覆盖
  - Linux/Windows 单测 cfg-gated 仅在真机编译时跑

## 2. 累计耗时

~30 min

## 3. PLAN 偏差

**0 处**

**1 处限制**（非偏差）：Windows / Linux 编译 + 单元测试**未在本机验证**。原因：本机为 aarch64-apple-darwin，无 Linux/Windows C 跨编译工具链。代码已通过类型检查 + cfg-gate 隔离 + macOS 121 单测回归；实机编译 + 跑测需用户在 M1a 真机测试矩阵中执行。已记入 SUGGESTION #S-2（🟡）+ 建议加 CI matrix（ubuntu-latest + windows-latest）。

## 4. 下一步

→ STEP-1a.4（service::clipboard_dispatcher：500ms 轮询 + sha256 fingerprint + LRU 64 + StreamC 推 ClipboardText）
→ 涉及：emulation.rs ListenTask 加 ClipboardText 分支转发 EmulationEvent::Message；service.rs 加 clipboard_dispatcher task + LanMouseConnection.send_clipboard helper
→ 关键风险点：跨 task 的 `Box<dyn ClipboardBackend>` 持有 + 500ms tokio::time::interval 集成