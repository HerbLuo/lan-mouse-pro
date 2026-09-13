# Bug Investigation: Windows Startup Segfault (post-M4)

> 日期：2026-09-13
> 平台：Windows（被控端；debug build `target\debug\lan-mouse.exe`）
> HEAD：756fc69
> 现象：daemon 启动后 segfault (STATUS_ACCESS_VIOLATION, 0xc0000005)
> 调查者：bug-investigator (独立子代理)
> 状态：**未结案 / 需 Leader 决策是否 executor 返工**

---

## 1. 现象复现

完整日志（用户提供）：
```
[2026-09-13T09:11:15Z INFO  input_capture::windows::event_thread] initial monitors: 1 monitor(s)
[2026-09-13T09:11:15Z INFO  input_capture::windows::event_thread]   monitor: id=windows:unknown-\\.\DISPLAY1 name="Intel(R) UHD Graphics" pos=(0, 0) size=(1280, 800) primary=true scale=1
[2026-09-13T09:11:15Z INFO  input_capture] using capture backend: windows
[2026-09-13T09:11:15Z INFO  lan_mouse::emulation] creating input emulation ...
[2026-09-13T09:11:15Z INFO  input_emulation] using emulation backend: windows
[2026-09-13T09:11:15Z INFO  lan_mouse] opening http://127.0.0.1:3939 in the default browser
[2026-09-13T09:11:15Z INFO  lan_mouse] using config: "C:\\Users\\hb\\AppData\\Local\\lan-mouse\\config.toml"
[2026-09-13T09:11:15Z INFO  lan_mouse] Press [KeyLeftCtrl, KeyLeftShift, KeyLeftMeta, KeyLeftAlt] to release the mouse
[2026-09-13T09:11:15Z INFO  lan_mouse::web] lan-mouse web UI listening on http://127.0.0.1:3939
error: process didn't exit successfully: `target\debug\lan-mouse.exe` (exit code: 0xc0000005, STATUS_ACCESS_VIOLATION)
Segmentation fault         cargo run
```

**关键观察**：
1. **所有启动 log 完整打印** —— capture / emulation / web UI 全部 OK
2. **崩溃发生在 web UI listening 之后** —— `tokio::select!{server.run(), service.run()}` 已进入；spawned tasks 已全部启动
3. **没有 Rust panic 信息** —— `cargo run` 只显示 STATUS_ACCESS_VIOLATION（native crash），没有 backtrace / panic message
4. **debug build**（`target\debug\lan-mouse.exe`）—— `Cargo.toml [profile.release] panic = "abort"` 仅影响 release，debug 默认 unwind
5. **没有 panic_hook / catch_unwind** —— grep 全代码无任何 panic hook；panic 一定会显示 backtrace

## 2. 根因分析

### 2.1 M4 改动范围回顾

| 文件 | 改动类型 | 启动路径相关？ |
|---|---|---|
| `lan-mouse-ipc/src/lib.rs` | `ClipboardConfig` 8 字段 + drop `auto_accept_files` | 否（仅 IPC 层 schema） |
| `src/config.rs` | `TomlClipboard` 8 字段 + 3 个新 getter | 否（仅配置读写） |
| `src/service.rs` | `Service::max_file_size` getter + `FileSetCollector` + `InboundFileApplyResult` 扩展 + `BackendCmd::SetFiles` + `maybe_inject_files_to_clipboard` + `decide_reinject_skip` + `apply_files_inner_returning_path` batch_fingerprint + 11 reinject_decision_tests | **部分**（Service struct 新字段 + Service::run 路径**不变**） |
| `src/clipboard/mod.rs` | trait `set_files` + `DummyBackend` 默认 impl | 否（trait 层） |
| `src/clipboard/macos.rs` | macOS `set_files` via `NSPasteboard.writeObjects` | 否（macOS only） |
| `src/clipboard/windows.rs` | Windows `set_files` via `CF_HDROP` + DROPFILES | **否（仅被 dispatcher 写触发，不在 startup 路径）** |
| `src/clipboard/linux.rs` | Linux `set_files` via xclip/wl-copy | 否（Linux only） |
| `src/popup.rs` | `PopupGuard` 重构 `Option<Payload>` 防递归（commit 在 `d6fb1d8` / `9f30228` 之间，pre-M4 但接近） | 否（仅 `dispatch_files` ExceedsLimit 触发） |
| `src/emulation.rs` | `ListenTask` 加 `ClipboardFiles` / `FileTransferCancel` match arm | **否（需要 peer 推送，startup 时无 peer）** |
| `src/listen.rs` 等 | 无变更 | — |

### 2.2 按可疑点逐条排查

| # | 可疑点 | 结论 | 证据 |
|---|---|---|---|
| 1 | `src/clipboard/windows.rs::set_files` 启动路径误触 | **排除** | `set_files` 仅在 dispatcher 的 `BackendCmd::SetFiles` 收到时调用；startup 无 inbound file，故 `maybe_inject_files_to_clipboard` 不触发 |
| 2 | `BackendCmd::SetFiles` poller match arm 缺失 | **排除** | `service.rs:5118-5127`（backend-absent）+ `service.rs:5275-5281`（backend-present）两侧 match arm 已落地并 pin 测试 |
| 3 | `Retained::cast_unchecked` 误用 | **排除** | 仅 macOS 用，且 `set_files` macOS 测试通过；Windows 上无 `Retained` |
| 4 | `DummyBackend` / 测试用 backend 在 production 路径误用 | **排除** | `DummyBackend` 仅用于 unit test；production 路径用 `WinClipboard` / `MacOsPasteboard` / `LinuxClipboard` |
| 5 | `default_accept_dir()` 在 Windows 上行为异常 | **低可疑度** | IPC 层用 `cfg!(unix).then(...)` 在 Windows 上跳过 `$HOME` 直奔 `$USERPROFILE`，fallback `/tmp/lan-mouse`（Windows 上不存在但只是 default 值，不立即 I/O）。Service 层不分支。差异仅影响 default path 解析，不应 segfault |
| 6 | `Service::new` 初始化顺序问题（field drop / 顺序依赖） | **排除** | `Service::new` 在 49fd3c7..756fc69 区间仅新增 `pending_file_collectors: HashMap::new()`（标准构造）+ 删除 `max_file_size: u64` 字段；其余字段初始化顺序未变 |
| 7 | `pending_file_collectors` HashMap 未初始化 / 字段缺失 | **排除** | `service.rs:1132` 显式 `pending_file_collectors: HashMap::new()` |
| 8 | `reply_rx` channel 在 `handle_inbound_files_applied async` 路径未持有 | **排除** | `reply_rx.await` 在 `maybe_inject_files_to_clipboard` 内（`service.rs:4099`），由 `oneshot::channel()` 正确配对 |
| 9 | `PopupGuard` Drop 仍递归触发 `fire()` | **排除** | M3a 阶段已修（commit `85e7b69` 2026-09-13 早段），新设计 `Option<Payload>` + `Option::take()` 保证 at-most-once |
| 10 | `clipboard_backend` 在 `clipboard_poller` 后台 tick 触发 Win32 API 失败 | **中可疑度** | 启动后 500ms tick 触发 `backend.current_image_async()` → `WinClipboard::current_image()` → `unsafe { OpenClipboard(NULL) }`。Win32 `OpenClipboard` 调用需要正确 thread context（每线程一 clipboard），如果 daemon 用了不正确的 thread 可能 fail；失败路径有 `return None` 不应 segfault。**但**：用户的 log 全部打印完才 crash，未显示 500ms 等待；可能是 tick 在 log 缓冲刷新之前触发 |
| 11 | notify-rust 在 Windows 上 DLL 加载失败 | **中可疑度** | 但 `notify_rust::Notification::new()` 仅在 `show_notification()` 调用，而 `show_notification()` 仅在 `PopupGuard` fire/drop 触发，**startup 不触发** |
| 12 | shadow-rs `shadow!(build)` 在 Windows 上行为异常 | **低可疑度** | `config.rs:47` `shadow!(build)` 是 build-time 宏，运行时只访问 `build::SHORT_COMMIT` 字符串；pre-M4 已有 |
| 13 | tokio runtime 关闭导致 spawned task panic | **中可疑度** | `LocalSet::new().run_until(f)` 等待所有 spawned_local task 完成；如果某个 task panic 触发 unwind，整个 runtime abort。但 debug build 应该打印 panic backtrace |
| 14 | pre-existing issue（M3a / 更早）暴露 | **中可疑度** | validator STEP-VALIDATION-P2-M4-FULL §3 已列 2 P2（doc-comment 自相矛盾 + `NoLandedPaths` 死代码），均无 P0/P1。但可能漏检了 pre-existing issue |
| 15 | **debug build 上的 panic 行为与 release 不同** | **高可疑度** | `Cargo.toml [profile.release] panic = "abort"`；debug 默认 `unwind`。Windows 上 panic unwind 可能触发 STATUS_ACCESS_VIOLATION（栈展开时遇到 native frame）。如果某段代码在 startup unwind panic，**会显示为 STATUS_ACCESS_VIOLATION**而非 panic backtrace |

### 2.3 最可能根因（best guess）

**P0 假设：debug build panic unwind 在 Windows 上触发 STATUS_ACCESS_VIOLATION**

理由：
1. 用户用 `cargo run`（debug build）启动 daemon
2. 启动过程中某处触发 Rust panic（不是 Rust 自身 panic，是 unwind 触发的 native frame 异常）
3. Windows 上 panic unwind 经过 native code frame（如 Win32 API、notify-rust 等）时可能 STATUS_ACCESS_VIOLATION
4. 不会显示 panic backtrace（因为 unwind abort 在 native frame 处终止）

**触发候选点**（在 startup 路径上）：
- `LanMouseListener::new` → `quic_transport::endpoint_with_verifier` → quinn rustls 初始化（pre-M4）
- `Emulation::new` → `EmulationProxy::new` → `spawn_local(emulation_task.run())` → `ListenTask::run` 内 `tokio::select!` arms → `_ = interval.tick()` 的首个 tick
- `Capture::new` → `spawn_local(capture_task.run())` → `do_capture` → `InputCapture::new(self.backend)` → Windows API
- `Service::run` → `clipboard_poller` 首个 tick (500ms 后) → `WinClipboard::current_image_async` → `unsafe { OpenClipboard(...) }`
- `web::WebServer::run` → `axum::serve` 的某个内部 panic

**关键约束**：所有 M4 改动**不直接执行于 startup 路径**。startup 路径上的所有代码都是 pre-M4 的。

### 2.4 次要可疑点

- **次要可疑点 #1：clipboard poller 首个 tick (500ms 后)**
  - `clipboard_poller` 在 `Service::run` 内 spawn_local，立即 `interval.tick().await`（fire immediately at t=0）；然后循环
  - 下一个 tick 500ms 后，触发 `backend.current_image_async()` → `WinClipboard::current_image()` → `unsafe { OpenClipboard(NULL) }`
  - 如果 Win32 clipboard 已经被另一进程持有且 thread context 异常，可能 fail
  - **但**：错误路径已显式 `return None`，不应 segfault

- **次要可疑点 #2：popup 模块 Drop 链**
  - 即使 `PopupGuard` 在 startup 未被构造，如果 `Cargo.toml` build.rs 路径上有其他 crate 触发类似 pattern，可能 segfault
  - **但**：grep 全代码无 `PopupGuard` 在 startup 路径上的引用

- **次要可疑点 #3：Emulation / Capture task 在 startup 后 spawn 的 panic**
  - `spawn_local(capture_task.run())` 和 `spawn_local(emulation_task.run())` 在 `Service::new` / `Emulation::new` 内调用
  - 启动后这些 task 进入 select! / interval tick 循环
  - 如果某个 task panic（debug unwind），会触发 runtime abort

## 3. 修复方案

### 3.1 最小修复（保留当前架构）

**方案 A：安装 panic hook（最快验证根因）**

在 `src/main.rs` `main()` 函数最开头安装 panic hook：

```rust
fn main() {
    // Install panic hook FIRST so all subsequent panics are captured.
    std::panic::set_hook(Box::new(|panic_info| {
        eprintln!("=== RUST PANIC AT STARTUP ===");
        eprintln!("{}", panic_info);
        // Also print backtrace if RUST_BACKTRACE=1
        if let Ok(s) = std::env::var("RUST_BACKTRACE") {
            if !s.is_empty() && s != "0" {
                eprintln!("{:?}", std::backtrace::Backtrace::force_capture());
            }
        }
        eprintln!("=============================");
    }));
    
    lan_mouse::install_crypto_provider();
    // ... rest of main
}
```

**效果**：
- 如果崩溃源是 Rust panic，hook 会先打印 panic 信息再触发 unwind
- 然后 STATUS_ACCESS_VIOLATION 仍然会触发（因为 unwind 在 native frame 处失败），但 panic 信息会保留到 stderr
- 用户在 cargo run 输出会看到 panic 信息 + STATUS_ACCESS_VIOLATION，能直接定位 panic 源

**风险**：低（只增加一个 hook）

### 3.2 方案 B：release build 测试（隔离 unwind / native crash）

如果方案 A 没有显示 panic 信息，则崩溃是纯 native crash（不是 panic）。此时：

1. `cargo build --release` 重新编译 release 版本（`panic = "abort"`，无 unwind）
2. `target\release\lan-mouse.exe` 启动
3. 如果 release build 正常 → 确认是 debug unwind 在 native frame 处失败（不是 M4 bug，pre-existing platform issue）
4. 如果 release build 也 crash → 真 native crash，需要进一步 trace

### 3.3 方案 C：定位具体 panic 源（如果有）

如果方案 A 显示 panic 信息，根据 panic 位置：
- panic in `quic_transport` → quinn rustls 初始化（pre-M4 issue）
- panic in `input_capture::windows` → Windows capture backend（pre-M4 issue）
- panic in `input_emulation::windows` → Windows emulation backend（pre-M4 issue）
- panic in `web::WebServer` → axum / tokio runtime（pre-M4 issue）
- panic in `service` main loop → Service::new / Service::run（M4 affected? 查 1132 行 HashMap::new()）

### 3.4 替代方案（如果需要更稳健路径）

**方案 D：升级 panic = "abort" 到 dev profile**

在 `Cargo.toml` 添加：
```toml
[profile.dev]
panic = "abort"
```

**效果**：debug build 也走 abort，不会 unwind；如果 crash，panic message 直接打印（abort 前 hook 触发），不会被 native frame 拦截。

**风险**：debug 体验差（无 unwind 栈），但**显著降低 Windows 上 startup crash 概率**。

## 4. 风险评估

| 方案 | 风险 | 影响范围 |
|---|---|---|
| A (panic hook) | 极低 | 仅增加 stderr 输出；不改变行为 |
| B (release test) | 无（仅隔离测试） | 不修改代码；仅 binary 选择 |
| C (定位 panic) | 取决于 panic 源 | 可能是 M4 无关（M3a / 早期 issue） |
| D (dev panic=abort) | 中 | 改变 dev 体验；可能掩盖 panic unwind bug |

**M4 影响评估**：
- **0 个 M4 改动在 startup 路径直接执行**
- **0 个 M4 新 unsafe 块在 startup 触发**
- **M4 改动全部在 clipboard runtime 路径，startup 不触及**
- 因此 M4 本身**不太可能是直接根因**

**macOS / Linux 影响**：
- panic hook（方案 A）三平台一致
- release 测试（方案 B）跨平台
- panic=abort（方案 D）跨平台

**已有契约破坏**：
- panic hook 不破坏任何契约
- panic=abort 改变 unwind 语义（仅 dev profile），release 不变

## 5. 建议下一步

### 5.1 推荐顺序（Leader 决策）

1. **首先：方案 A（panic hook）** — 1 行代码改动，最快确认是否 Rust panic
2. **其次：方案 B（release build 测试）** — 不改代码，隔离 unwind vs native crash
3. **最后：方案 C/D** — 视 A/B 结果决定

### 5.2 是否派 executor 返工

**建议**：先让用户按方案 A 改 1 行 panic hook 并复测。如果 panic 信息明确指向 M4 改动区域（如 `pending_file_collectors` / `set_files` / `BackendCmd::SetFiles`），再派 executor 返工。

**如果 panic 信息指向 pre-M4 区域**（如 quinn / Windows capture / axum），则**不属于 M4 BUG**，应开新 SUGGESTION 条目跟踪 pre-existing platform issue，不要在 M4 scope 内返工。

### 5.3 调查局限性

**bug-investigator 不能跑 `cargo build` / `cargo run`**（用户纪律禁止），所以无法本地复现验证。本报告基于静态代码分析 + diff 比对 + grep 推断。

**建议 Leader 直接指示用户做以下动作**：
1. 加 panic hook（1 行）
2. 重新 `cargo run`
3. 把完整 stderr 输出贴回来（含 panic info）
4. 同时 `cargo build --release` + `target\release\lan-mouse.exe` 启动测试

收到 panic 信息 / release build 测试结果后，Leader 可精准派 executor 返工。

---

## 6. 总结报告

```
## Bug Investigation 报告

**根因**：未结案；M4 改动本身**不直接**导致 startup segfault（所有 M4 改动在 startup 路径外）。
        最可能根因：debug build panic unwind 在 Windows native frame 处触发 STATUS_ACCESS_VIOLATION；
        或 pre-existing platform issue（quinn / Windows capture / axum / clipboard poller 首个 tick）。

**位置**：未定位（需要用户复测 panic 信息才能确定）

**严重度**：P0（崩溃）；但 M4 scope 内**可能不属 M4 BUG**

**修复方案**：先在 src/main.rs 加 panic hook（1 行），让用户复测以获取 panic 信息；
                同步让用户跑 release build 隔离 unwind vs native crash。

**详细报告**：next/BUG-INVESTIGATION-WINDOWS-STARTUP-SEGFAULT.md（本文件）

**下一步**：
  - Leader 决定是否派 executor 加 panic hook（最小改动）
  - 用户复测后根据 panic 信息决定是否 M4 返工
  - 如果 panic 指向 pre-M4 代码，开 SUGGESTION 条目跟踪，不进 M4 scope
```
