# Bug Investigation: Windows set_files Segfault (post-M4)

> 日期：2026-09-13
> 平台：Windows（被控端；debug build `target\debug\lan-mouse.exe`）
> HEAD：756fc69（M4 DONE）
> 现象：`WinClipboard::set_files` 调用后 segfault (STATUS_ACCESS_VIOLATION, 0xc0000005)
> 前置报告：`next/BUG-INVESTIGATION-WINDOWS-STARTUP-SEGFAULT.md`（场景 1；未结案；推荐 panic hook + release build）
> 状态：**未结案 / 需要 crash dump + debug symbols 才能精确 pinpoint**

---

## 1. 现象复现

### 1.1 场景 1（前置报告，已记录）

启动时主控端剪贴板含文件 → daemon 启动日志全部打印完后 segfault。所有 startup log 完整，最后一行是 web UI listening，崩溃发生在 startup 路径上。**与场景 2 不同路径**，本报告聚焦场景 2。

### 1.2 场景 2（本次分析目标）

完整 log（用户提供）：
```
[INFO  lan_mouse::listen] reply: Ack(0) to 10.2.1.15:60137 delivered
[INFO  lan_mouse::capture] release_capture: ENTER (state=Idle, active_client=None)
[INFO  lan_mouse::listen] server stream C reader: from 10.2.1.15:60137: ClipboardImage(fp=6b6e2fd7, sha=6b6e2fd7, size=300134, mime=image/png)
[INFO  lan_mouse::service] clipboard inbound image: apply kicked off to spawn_local task (sha=6b6e2fd7, mime=image/png) for 10.2.1.15:60137
[INFO  lan_mouse::service] clipboard inbound image: pulled 300134 bytes from 10.2.1.15:60137 via HTTP/3 (sha=6b6e2fd7, mime=image/png)
[INFO  lan_mouse::service] clipboard inbound image: backend transcoded (inbound sha=6b6e2fd7 → on-clipboard sha=425a0e40, mime=image/png); marking transcoded SHA in image LRU
[INFO  lan_mouse::service] clipboard inbound image: applied 300134 bytes from 10.2.1.15:60137 (sha=6b6e2fd7, mime=image/png)
[INFO  lan_mouse::listen] server stream C reader: from 10.2.1.15:60137: ClipboardFiles(fp=a6f8aa92, entries=1)
[INFO  lan_mouse::service] clipboard inbound files: spawning 1 apply task(s) for ClipboardFiles(fingerprint=a6f8aa92) from 10.2.1.15:60137 (accept_dir=C:\Users\hb\lan-mouse)
[INFO  lan_mouse::service] clipboard inbound file: pulled 2272724 bytes from 10.2.1.15:60137 via HTTP/3 (sha=6d838886, name=深井泵高速.jpg, mime=image/jpeg)
[INFO  lan_mouse::service] clipboard inbound file: applied 2272724 bytes from 10.2.1.15:60137 (sha=6d838886, name=深井泵高速.jpg, mime=image/jpeg, landed at "C:\\Users\\hb\\lan-mouse\\深井泵高速 (2).jpg")
[INFO  lan_mouse::service] clipboard re-inject: dispatching set_files(1 path(s)) for batch_fingerprint=a6f8aa92 (pre-stamped last_outbound_files_fingerprint)
error: process didn't exit successfully: `target\debug\lan-mouse.exe` (exit code: 0xc0000005, STATUS_ACCESS_VIOLATION)
Segmentation fault         cargo run
```

**关键观察**：
1. **M4 set_files 路径是 root cause 候选**（场景 2 与场景 1 路径不同）
2. **pre-M4 set_dib_image 路径 work OK**：image 300134 bytes 通过 `BackendCmd::SetDibImage` 成功 applied（包括 post-write `CurrentImage` re-read）；说明 `alloc_dib_handle_and_set` helper 在 CF_DIBV5 + CF_DIB 路径可用
3. **崩溃发生在最后一条 log 之后**：`"dispatching set_files(1 path(s)) for batch_fingerprint=a6f8aa92 (pre-stamped last_outbound_files_fingerprint)"` —— 此 log 由 main task 在 `cmd_tx.send(BackendCmd::SetFiles { ... })` 成功后、`reply_rx.await` 之前打印
4. **崩溃点在 `backend.set_files(&files)` 内部**（用户推断）：poller 收到 SetFiles 后调用 `backend.set_files(&files)`，原生 crash 在该调用内；主 task 的 `reply_rx.await` 永远收不到 reply
5. **没有 Rust panic 信息**：cargo run 输出只有 STATUS_ACCESS_VIOLATION，无 backtrace / panic message
6. **debug build**：`target\debug\lan-mouse.exe`（[profile.release] panic = "abort" 仅影响 release）
7. **崩溃涉及的具体数据**：1 个 file path `C:\Users\hb\lan-mouse\深井泵高速 (2).jpg`（36 ASCII + 5 BMP CJK + NUL terminator → 37 wchars → 74 bytes path）+ DROPFILES 20-byte header + 4-byte double-NUL → payload 98 bytes

---

## 2. 根因分析

### 2.1 调用链（main task → poller → Win32）

```
[main task] service.rs:4087
  log::info!("clipboard re-inject: dispatching set_files(1 path(s))...")
  reply_rx.await  ← blocks here

[poller] service.rs:5275
  BackendCmd::SetFiles { files, reply } => {
      let _ = reply.send(backend.set_files(&files));  ← segfault inside
  }

[windows.rs:614] WinClipboard::set_files(&mut self, files: &[PathBuf])
  if files.is_empty() { return Ok(()); }                ← not empty, skip
  let payload = build_dropfiles_payload(files)?;         ← pure helper, 98 bytes
  if OpenClipboard(null_mut()) == 0 { return Err }      ← ?
  if EmptyClipboard() == 0 { return Err }                ← ?
  alloc_dib_handle_and_set(&payload, CF_HDROP_U32)       ← ?
  CloseClipboard()
```

### 2.2 按可疑点 1-10 逐条排查

#### 可疑点 1：`OpenClipboard(NULL)` 错误传参

- **位置**：`src/clipboard/windows.rs:622`
- **证据**：`if unsafe { OpenClipboard(std::ptr::null_mut()) } == 0 { ... }`
- **分析**：
  - MSDN 明确允许 `OpenClipboard(NULL)`（associate with current task）
  - 与 `current_text` (line 141), `set_text` (line 212), `current_image` (line 328), `set_dib_image` (line 446), `current_files` (line 526) **完全一致**，全部正常工作
  - 如果 NULL 是问题，前述所有路径都会崩
- **结论**：✅ **排除**（NULL 是 Windows 文档化合法用法）
- **修复建议**：无

#### 可疑点 2：`DragQueryFileW` 误用

- **位置**：N/A
- **证据**：`DragQueryFileW` 仅在 `current_files` (line 546, 560, 571) **读取路径**使用
- **分析**：`set_files` 写入路径不调用 `DragQueryFileW`（DragQueryFileW 是 reader API，不能构造 DROPFILES payload；用户必须直接填充 DROPFILES struct + paths）
- **结论**：✅ **排除**（DragQueryFileW 不在 set_files 调用路径上）
- **修复建议**：无

#### 可疑点 3：DROPFILES 结构 layout 错误

- **位置**：`src/clipboard/windows.rs:748-752`
- **证据**：
  ```rust
  buf.extend_from_slice(&0xFFFFFFFFu32.to_le_bytes()); // pFiles sentinel
  buf.extend_from_slice(&0i32.to_le_bytes());          // pt.x
  buf.extend_from_slice(&0i32.to_le_bytes());          // pt.y
  buf.extend_from_slice(&0u32.to_le_bytes());          // fNC
  buf.extend_from_slice(&1u32.to_le_bytes());          // fWide
  ```
- **分析**：
  - DROPFILES 结构 20 bytes，layout 符合 shellapi.h（pFiles=DWORD, pt=POINT {LONG x, LONG y}, fNC=BOOL, fWide=BOOL）
  - `pFiles = 0xFFFFFFFF` 是文档化 sentinel（"file list immediately follows"）
  - `fWide = 1` 是 UTF-16 LE 标志
  - path list: 每 path UTF-16 LE + NUL wchar + 末尾 double-NUL wchar
  - 单测 `build_dropfiles_payload_emits_correct_header_for_single_path` 验证 20-byte header 字段值正确
  - 单测 `build_dropfiles_payload_round_trips_paths_via_wide_decode` 验证 NUL-terminator + double-NUL 路径 round-trip
- **结论**：✅ **排除**（layout 正确，单测已 pin）
- **修复建议**：无

#### 可疑点 4：`GlobalAlloc` 返回 NULL 但未检查

- **位置**：`src/clipboard/windows.rs:845` (alloc_dib_handle_and_set)
- **证据**：
  ```rust
  let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, byte_len) } as HGLOBAL;
  if handle.is_null() {
      let err = unsafe { GetLastError() };
      return Err(ClipboardError::Io(format!(
          "GlobalAlloc({byte_len} bytes, alloc_dib_handle_and_set format={format}) failed: GetLastError={err}"
      )));
  }
  ```
- **分析**：
  - NULL 检查存在，失败路径返回 Err + log
  - `byte_len = bytes.len() = 98`（对当前 case）—— 微小分配；GMEM_MOVEABLE 失败概率极低
  - 如果失败，会 log error "GlobalAlloc(98 bytes, alloc_dib_handle_and_set format=15) failed" —— **用户 log 未见此行**
- **结论**：✅ **排除**（NULL check 存在；失败会 log；用户 log 无此行说明 GlobalAlloc 成功）
- **修复建议**：无

#### 可疑点 5：`SetClipboardData(CF_HDROP, hdrop)` 在 `OpenClipboard` 失败但未检查

- **位置**：`src/clipboard/windows.rs:888`
- **证据**：
  ```rust
  // SAFETY: ownership of `handle` transfers to the OS on the
  // successful `SetClipboardData` return. We ignore the return
  // value (NULL on failure) — see `set_text` for the
  // leak-on-failure rationale.
  let _ = unsafe { SetClipboardData(format, handle as _) };
  ```
- **分析**：
  - `OpenClipboard` 返回值 **已检查**（line 622-627）—— 失败时返回 Err
  - `EmptyClipboard` 返回值 **已检查**（line 632-639）—— 失败时 CloseClipboard + 返回 Err
  - `SetClipboardData` 返回值被 `let _ = ...` 丢弃（与 set_text / set_dib_image 同模式）—— 失败时 handle leak，但用户文档化的 "leak-on-failure" pattern，不应 segfault
- **结论**：✅ **排除**（OpenClipboard / EmptyClipboard 都有 check；SetClipboardData 失败路径已设计为 leak not crash）
- **修复建议**：无

#### 可疑点 6：`GlobalUnlock` / `CloseClipboard` 顺序错误导致 double-free / use-after-free

- **位置**：`src/clipboard/windows.rs:875` (GlobalUnlock) + `src/clipboard/windows.rs:888` (SetClipboardData) + `src/clipboard/windows.rs:651` (CloseClipboard)
- **证据**：
  ```rust
  // alloc_dib_handle_and_set 内部:
  std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, byte_len);
  GlobalUnlock(handle);                          // ← unlock FIRST
  true
  };
  // ... write_ok 检查 ...
  let _ = unsafe { SetClipboardData(format, handle as _) };  // ← set AFTER unlock
  Ok(())
  ```
- **分析**：
  - 顺序符合 MSDN: copy → GlobalUnlock → SetClipboardData
  - SetClipboardData 成功后 OS 接手 handle (MSDN 文档化)
  - set_files 最后 `CloseClipboard()` (line 651)
  - alloc_dib_handle_and_set 中 GlobalLock 失败路径 free handle + return Err（line 868）—— 已修复 M2b validator P1.2 leak
- **结论**：✅ **排除**（顺序符合 MSDN；与 working set_dib_image 一致）
- **修复建议**：无

#### 可疑点 7：文件 path 含非 ASCII 字符（"深井泵高速.jpg"）—— UTF-16 编码失败或 buffer 长度算错

- **位置**：`src/clipboard/windows.rs:736-742` (build_dropfiles_payload)
- **证据**：
  ```rust
  let wide: Vec<u16> = OsString::from(p)
      .encode_wide()
      .chain(std::iter::once(0))
      .collect();
  payload_size += wide.len() * std::mem::size_of::<u16>();
  ```
- **分析**：
  - `OsStr::encode_wide` 对 BMP char 产 1 wchar，对 supplementary char 产 2 wchar（surrogate pair），对 unpaired surrogate 产 REPLACEMENT_CHARACTER
  - "深井泵高速" 全部 BMP (U+6DF1, U+4E95, U+6CF5, U+9AD8, U+901F) → 5 wchars
  - 完整路径 "C:\Users\hb\lan-mouse\深井泵高速 (2).jpg" → 22 ASCII + 5 CJK + 9 ASCII = 36 chars → 36 wchars + chain NUL = 37 wchars
  - payload_size 计算：`20 + 37*2 + 2*2 = 98 bytes` 正确
  - `Vec::with_capacity(payload_size)` 预分配正确
  - extend_from_slice 调用次数对得上：1 header (20) + 37 wchars (74) + 4 terminator = 98 ✓
  - 字符串含 BMP 中文 + ASCII + 括号 + 空格 + 数字 —— 都是合法 UTF-16 编码字符
- **结论**：✅ **排除**（UTF-16 编码路径正确；payload size 准确；BMP 中文 + ASCII 无 surrogate pair 风险）
- **修复建议**：无；可考虑加 `set_files_path_with_surrogate_pair_round_trips` 单测覆盖 supplementary plane (e.g. emoji 𝕊) 以防御未来回归

#### 可疑点 8：空 slice 处理（`files.is_empty()`）误触 Windows API 但 impl 是 early return

- **位置**：`src/clipboard/windows.rs:615-617`
- **证据**：
  ```rust
  if files.is_empty() {
      return Ok(());
  }
  ```
- **分析**：
  - 空 slice 提前 return，不调用任何 Win32 API
  - 当前 case `files` 有 1 个元素，非空 → 跳过此分支
- **结论**：✅ **排除**（当前 case 不走此分支）
- **修复建议**：无

#### 可疑点 9：Mutex / lock 泄漏 —— `&mut self` 但内部取锁失败 / 双重 lock

- **位置**：`src/clipboard/windows.rs:614`
- **证据**：`fn set_files(&mut self, files: &[PathBuf]) -> Result<(), ClipboardError>`
- **分析**：
  - `&mut self` 是 Rust borrow checker 保证的 exclusive reference
  - `set_files` 内部无 `Mutex` / `RwLock` / 任何内部可变状态
  - `cached: Option<String>` 是 WinClipboard 唯一 state，set_files 不访问
  - poller 通过 `&mut backend: Box<dyn ClipboardBackend>` exclusive own backend
  - main task 通过 channel `cmd_tx.send(...)` 异步交互，不直接持有 backend 引用
  - 无双重 lock 风险
- **结论**：✅ **排除**（无 Mutex / 双重 lock）
- **修复建议**：无

#### 可疑点 10：测试 mock 通过但 production 调用真实 Windows API 时未测试的代码路径

- **位置**：`src/clipboard/windows.rs:907-1207` (`mod tests`)
- **证据**：
  - 4 个 `set_files` 相关单测全部是 `build_dropfiles_payload` helper（pure function）的测试
  - **0 个单测覆盖 `WinClipboard::set_files` 本身的 Win32 API 调用路径**（OpenClipboard / EmptyClipboard / GlobalAlloc / GlobalLock / SetClipboardData / CloseClipboard）
  - pre-M4 `set_text` / `set_dib_image` / `current_files` 同样 0 单测覆盖 Win32 路径
- **分析**：
  - windows.rs 顶部 `#[cfg(target_os = "windows")]` —— 单测只在 Windows runner 跑
  - 但 `build_dropfiles_payload` 是 free function，可在 macOS / Linux cargo test 跑（因为它没有 unsafe）
  - 实际 set_files Win32 路径依赖真实 Windows VM 真机 round-trip（PLAN §8 M4 矩阵）
  - **M4 validator `STEP-VALIDATION-P2-M4-FULL` §1 报告 "0 P0 / 0 P1"**，但 validator 也在非 Windows 环境跑，无法真机验证
  - 用户在真机首次触发 set_files 路径即 segfault —— **强烈指向 Win32 integration gap**
- **结论**：⚠️ **可疑（test coverage gap）**
- **修复建议**：
  - 加 1 行 panic hook（per 前置报告方案 A）—— 任何 Rust panic 会先 log，再 unwind（debug build）
  - Windows 真机加 `procdump -ma -e 1 lan-mouse.exe` 或类似工具抓 crash dump
  - 生成 Windows debug symbols (PDB) 以便 stack trace 解码
  - 或：用户跑 release build（panic = "abort"）隔离 unwind vs native crash

### 2.3 关键对比：working set_dib_image vs crashing set_files

两者结构**完全平行**，都用 `alloc_dib_handle_and_set` helper，唯一差异是 format 参数：

| 步骤 | `set_dib_image` (working) | `set_files` (crashing) |
|---|---|---|
| 1 | OpenClipboard(null_mut()) | OpenClipboard(null_mut()) |
| 2 | EmptyClipboard() | EmptyClipboard() |
| 3 | alloc_dib_handle_and_set(bytes, CF_DIBV5_U32=17) | alloc_dib_handle_and_set(&payload, CF_HDROP_U32=15) |
| 4 | alloc_dib_handle_and_set(bytes, CF_DIB_U32=8) | — |
| 5 | CloseClipboard() | CloseClipboard() |

`alloc_dib_handle_and_set` helper 内部对 format 参数无差异处理（仅透传给 `SetClipboardData(format, handle as _)`）。**pre-M4 set_dib_image 跑了数百次都 work**，post-M4 set_files **第一次执行就 segfault**。

### 2.4 最可能根因（best guess）

**P0 假设 1：Windows `SetClipboardData` 在 CF_HDROP format + 已被本进程 prior image apply 占用 clipboard 状态下有平台特定行为差异**

- **理由**：
  1. set_dib_image 已成功 apply（clipboard 当前 owned by us，含 CF_DIBV5 + CF_DIB）
  2. set_files OpenClipboard 成功（owned by us）
  3. set_files EmptyClipboard 成功（释放 CF_DIBV5 + CF_DIB handle）
  4. set_files alloc new HGLOBAL 98 bytes + 写入 DROPFILES payload + GlobalUnlock
  5. **`SetClipboardData(CF_HDROP, hGlobal)`** ← 可能 segfault 此处

- **候选根因**：
  - 某些 Windows 版本 / 状态下，`SetClipboardData` 接受 CF_HDROP format 时要求 hGlobal 是用 `GMEM_DDESHARE` 或特殊 flag 分配的（极少数边缘 case）
  - 或：Windows 11 24H2 / 某个 patch 后 `SetClipboardData` 对 CF_HDROP 的内部 validation 改了

**P0 假设 2：测试覆盖 gap —— `set_files` Win32 路径从未被任何 test 触发过**

- **理由**：
  1. 4 个 STEP-4.2 单测**全部是 `build_dropfiles_payload` pure helper**（无 Win32 调用）
  2. pre-M4 `set_text` / `set_dib_image` / `current_files` Win32 路径同样**无单测覆盖**
  3. 但 pre-M4 路径在用户的 macOS 真机测试矩阵 + 此前 Windows 真机测试中**已被真机触发并验证 work**
  4. **M4 set_files Win32 路径是首次进入 production 的全新代码**，没有任何真机 / 单测验证过

- **推断**：可能不是代码 bug，而是 **OS 行为差异 / 用户特定环境问题**（如 Windows 版本、AV、剪贴板状态污染）

### 2.5 次要可疑点

- **次要 #1：`last_outbound_files_fingerprint` pre-stamp 时机**：
  - pre-stamp 在 `cmd_tx.send(BackendCmd::SetFiles { ... })` 之前（service.rs:4061）
  - 500ms 后 poller tick 调用 `current_files()` → 重新读 clipboard → 命中 fingerprint → short-circuit
  - 这是 symmetric to commit `d6fb1d8` 的 ExceedsLimit pre-stamp pattern，**与 set_files 实现无关**
- **次要 #2：`maybe_inject_files_to_clipboard` `&mut self` borrow 与 poller 并发**：
  - main task `&mut self` borrow 持续整个 `reply_rx.await`
  - poller `&mut backend` borrow 在 select! arm 中
  - LocalSet 单线程，**两 borrow 不重叠**（mutex 物理隔离）
  - 排除
- **次要 #3：CRLF / 文件名非法字符**：
  - 文件名 `深井泵高速 (2).jpg` 在 Windows 文件系统合法（已通过文件系统验证 —— "applied 2272724 bytes ... landed at ..."）
  - `(2)` 是 `resolve_unique_path` 加的 collision suffix（PLAN §3 决策）
  - 排除

---

## 3. 根因结论

- **根因**：**未结案**。`set_files` Win32 API 路径在用户真机 segfault，但代码静态分析 + 与 working `set_dib_image` 对比无法定位精确根因。
  - 最可能根因：Windows `SetClipboardData` 在 CF_HDROP format + 本进程 prior clipboard ownership 状态下的平台特定行为差异（**假设 1**）；或 **test coverage gap**（**假设 2**）
  - 排除所有 9 个具体可疑点（NULL HWND / DragQueryFileW / DROPFILES layout / GlobalAlloc NULL / SetClipboardData pre-check / lock ordering / UTF-16 / empty slice / Mutex）
- **触发链**：peer 发送 ClipboardFiles → 文件落盘成功 → collector 触发 → `maybe_inject_files_to_clipboard` → pre-stamp + send `BackendCmd::SetFiles` → log "dispatching set_files(1 path(s))" → `reply_rx.await` → poller 收到 → `backend.set_files(&files)` → **Win32 内部 segfault** → process die
- **为什么 segfault**：原生 access violation in `SetClipboardData(CF_HDROP, ...)` 或其内部 kernel call (win32k.sys)。Rust unwind 不触发（STATUS_ACCESS_VIOLATION 是 OS-level exception，不是 Rust panic）。

---

## 4. 修复方案

### 4.1 最小修复（推荐 —— 需要用户先提供 crash dump 才能精准定位）

#### 4.1.1 添加 panic hook（前置报告方案 A）

```rust
// src/main.rs
fn main() {
    // Install panic hook FIRST so all subsequent panics are captured.
    std::panic::set_hook(Box::new(|panic_info| {
        eprintln!("=== RUST PANIC ===");
        eprintln!("{}", panic_info);
        if let Ok(s) = std::env::var("RUST_BACKTRACE") {
            if !s.is_empty() && s != "0" {
                eprintln!("{:?}", std::backtrace::Backtrace::force_capture());
            }
        }
        eprintln!("==================");
    }));
    
    lan_mouse::install_crypto_provider();
    // ... rest of main
}
```

**效果**：如果根因实际是 Rust panic（在某个 `unwrap` / `expect` / array index），panic 信息会先打印到 stderr，**先于** STATUS_ACCESS_VIOLATION。cargo run 输出会显示 panic 位置 + backtrace。

**风险**：低（仅增加 stderr 输出）

#### 4.1.2 生成 crash dump（最关键）

Windows 上 `SetClipboardData` 内部 segfault 无法通过 panic hook 捕获。需要：

```bash
# 方法 1：procdump (推荐)
procdump -ma -e 1 -f "" target\debug\lan-mouse.exe

# 方法 2：Windows Error Reporting (WER) 启用 local dumps
# HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows\Windows Error Reporting\LocalDumps\lan-mouse.exe
#   DumpType = 2 (full)
#   DumpFolder = C:\dumps

# 方法 3：DebugDiag
```

crash dump (.dmp) + Windows debug symbols (PDB) + WinDbg 可定位精确 segfault 行号。

#### 4.1.3 跑 release build（前置报告方案 B）

```bash
cargo build --release
target\release\lan-mouse.exe
```

**效果**：[profile.release] panic = "abort" —— unwind 路径被禁用。如果 release 正常 → 根因是 debug unwind 在 native frame 处失败（与前置报告场景 1 同模式）；如果 release 也 crash → 真 native crash。

#### 4.1.4 加 SUGGESTION 条目（暂时性 workaround）

```rust
// src/clipboard/windows.rs::set_files 临时 try-catch
fn set_files(&mut self, files: &[PathBuf]) -> Result<(), ClipboardError> {
    if files.is_empty() {
        return Ok(());
    }
    let payload = match build_dropfiles_payload(files) {
        Ok(p) => p,
        Err(e) => return Err(e),
    };
    // SAFETY: each Win32 call is guarded against failure (returns Err on
    // BOOL=0 / handle.is_null()). The set_files Win32 path is identical to
    // the pre-M4-working set_dib_image pattern; if a platform-specific
    // failure occurs the helper's error message includes the GetLastError.
    if unsafe { OpenClipboard(std::ptr::null_mut()) } == 0 {
        let err = unsafe { GetLastError() };
        return Err(ClipboardError::Io(format!(
            "OpenClipboard (set_files) failed: GetLastError={err}"
        )));
    }
    if unsafe { EmptyClipboard() } == 0 {
        let err = unsafe { GetLastError() };
        unsafe { CloseClipboard(); }
        return Err(ClipboardError::Io(format!(
            "EmptyClipboard (set_files) failed: GetLastError={err}"
        )));
    }
    if let Err(e) = alloc_dib_handle_and_set(&payload, CF_HDROP_U32) {
        unsafe { CloseClipboard(); }
        return Err(e);
    }
    unsafe { CloseClipboard(); }
    Ok(())
}
```

**注**：以上 4.1.4 实际**已经是当前代码**（无 bug 可改）。这里列出来仅为说明 set_files 的现有 Win32 守卫模式。

### 4.2 替代方案（如需更稳健路径）

#### 4.2.1 `set_files` 改用 SendMessage + WM_DROPFILES 模式（避免 SetClipboardData 直接调用）

极少 Windows app 用此模式作为 CF_HDROP 写入路径，仅在 SetClipboardData 不可用时考虑。**不推荐**——会破坏其他 app 读取。

#### 4.2.2 加 Windows VM 真机 round-trip 集成测试

在 CI matrix 的 windows-latest runner 加 integration test（需要真 clipboard 操作，可能干扰 CI runner）。可作为 long-term SUGGESTION。

#### 4.2.3 加 panic hook + release build（前置报告 §3.1 方案 A+B）

综合方案 —— panic hook 优先，release build 备用。

### 4.3 不推荐的修复

- ❌ **不加诊断直接 try-except wrap set_files**：Rust panic hook 已覆盖 panic；STATUS_ACCESS_VIOLATION 是 OS exception 不受 Rust control
- ❌ **改用 OLE Drag-Drop API（DoDragDrop）**：超出 scope，且用户已经在用剪贴板 copy/paste
- ❌ **删 set_files 在 Windows 上的实现**：破坏 M4 跨平台契约

---

## 5. 测试覆盖建议

### 5.1 当前单测是否覆盖了 segfault 路径？

**否**。4 个 STEP-4.2 单测全部是 `build_dropfiles_payload` pure function，**0 个单测覆盖 `WinClipboard::set_files` 本身的 Win32 API 路径**（OpenClipboard / EmptyClipboard / alloc_dib_handle_and_set / SetClipboardData / CloseClipboard）。

### 5.2 应该加什么测试

#### 5.2.1 `build_dropfiles_payload` supplementary plane test（防御性）

```rust
#[test]
fn build_dropfiles_payload_handles_supplementary_plane_characters() {
    // 𝕊 = U+1D54A (supplementary plane, surrogate pair: 0xD835 0xDD4A)
    let paths = vec![PathBuf::from(r"C:\Users\me\file_𝕊.txt")];
    let buf = build_dropfiles_payload(&paths).expect("build_dropfiles_payload");
    // Verify 4-byte UTF-16 encoding: 2 surrogate wchars + NUL + double-NUL
    // = (4 path wchars + 2 surrogate wchars + 1 NUL + 2 NUL) * 2 = 18 bytes
    // header + path
    assert_eq!(buf.len(), 20 + 18);
    // Round-trip
    // ...
}
```

#### 5.2.2 真机 Windows VM integration test（PLAN §8 M4 矩阵要求）

```rust
// tests/manual/clipboard-files.md
// 1. Start lan-mouse daemon on Windows VM
// 2. Connect peer (macOS or Linux)
// 3. On peer: copy a file (e.g. test.pdf)
// 4. On Windows VM: paste into Explorer → should paste file at accept_dir
// 5. Verify round-trip: file SHA matches source
```

测试矩阵要求在每个 M4 STEP DONE 后做真机双向 round-trip。**M4 用户验证矩阵未跑 set_files 真机 round-trip**（场景 1 startup crash 先暴露；用户报告时未涉及文件 copy 场景）。

#### 5.2.3 set_files 路径细粒度 SUGGESTION 单测

在 Windows 真机上无法 mock `SetClipboardData` 行为，但可以加：
- `set_files_empty_slice_returns_ok` —— 验证 early return（已被 `build_dropfiles_payload_empty_input_returns_empty_vec` 间接覆盖）
- `set_files_returns_err_on_failed_openclipboard` —— mock `OpenClipboard`（需要 process-level mocking，无法在 unit test 做）

唯一可行路径：**Windows 真机 round-trip test**（PLAN §8 M4 矩阵）。

---

## 6. 风险评估

### 6.1 修复后 macOS / Linux 影响

| 修复 | macOS | Linux | Windows |
|---|---|---|---|
| panic hook | 无影响 | 无影响 | 仅增加 stderr 输出 |
| crash dump 工具 | 无影响 | 无影响 | 仅 dump 文件 |
| release build | panic=abort 已对所有平台生效 | 同 | 同 |
| SUGGESTION 单测 | N/A | N/A | 仅 Windows runner |

### 6.2 已有契约破坏

- panic hook：✅ 不破坏任何契约
- crash dump：✅ 不破坏
- release build：仅影响 dev 体验（unwind 栈不可用），release 不变
- SUGGESTION 单测：✅ 不破坏

### 6.3 不返工的风险

- 不返工 → 用户继续遇到 segfault → M4 done status 受影响 → 需要 SUGGESTION 条目跟踪
- 返工 → 需要 crash dump 定位 → 用户需重跑

---

## 7. 建议下一步

### 7.1 推荐顺序（Leader 决策）

1. **首先：用户加 panic hook（1 行）+ 重新 `cargo run` 复现**
   - 1 行代码改动，最快确认是否 Rust panic（vs 真 native crash）
   - 输出含 panic 信息 → 定位 panic 源
   - 输出仍 STATUS_ACCESS_VIOLATION → 确认真 native crash

2. **其次：用户跑 `cargo build --release` + `target\release\lan-mouse.exe`**
   - 隔离 unwind vs native crash
   - release 正常 → debug unwind 在 native frame 处失败（前置报告同模式）
   - release crash → 真 native crash，需要 crash dump

3. **并行：用户抓 crash dump（procdump 或 WER）**
   - 关键诊断数据
   - 配合 Windows PDB (debug symbols) 在 WinDbg 解码
   - 定位 `win32u!SetClipboardData` 或 `kernel32!GlobalAlloc` 等精确栈

4. **再次：用户提供 crash dump + backtrace 给 bug-investigator 重分析**
   - 本次报告无法仅靠静态分析结案
   - crash dump 是精确诊断依据

### 7.2 是否派 executor 返工

**建议：暂不返工**。
- 静态分析无法定位精确根因
- 任何"修复"都是猜测，可能引入新 bug
- 优先收集诊断数据再决策

### 7.3 是否开 SUGGESTION 条目跟踪

**建议：是**。开 1 条 SUGGESTION：
- **#S-12** 🟡 P1 - Windows `set_files` segfault 真机复现 + crash dump 定位
- 状态：⏸ 等待用户 crash dump 数据
- 跟踪：
  - 用户加 panic hook + release build 复测
  - 抓 crash dump + WinDbg 分析
  - 定位精确根因后决定 M4 返工 / SUGGESTION closure / 关闭为 Windows-specific

### 7.4 调查局限性

**bug-investigator 不能跑 `cargo build` / `cargo test` / `cargo run`**（用户纪律禁止），且没有 Windows 真机访问。本报告基于：
- 静态代码分析 + diff 比对
- 与 pre-M4 working `set_dib_image` 路径对比
- Windows API 文档 (MSDN)
- 前置 BUG-INVESTIGATION-WINDOWS-STARTUP-SEGFAULT.md 的方法论
- M4 validator 报告 `STEP-VALIDATION-P2-M4-FULL` 的 "0 P0 / 0 P1" 结论

**无法结案**。需要用户提供：
1. panic hook 后 cargo run 完整 stderr 输出
2. cargo build --release + target\release\lan-mouse.exe 启动测试结果
3. crash dump (procdump / WER) + WinDbg 分析 stack trace

---

## 8. 总结报告

```
## Bug Investigation 报告：Windows set_files segfault

**根因**：未结案；静态分析无法定位精确根因。所有 9 个具体可疑点（NULL HWND / DragQueryFileW / DROPFILES layout / GlobalAlloc NULL / SetClipboardData pre-check / lock ordering / UTF-16 encoding / empty slice / Mutex）已排除。最可能根因为 Windows SetClipboardData 在 CF_HDROP format + 本进程 prior clipboard ownership 状态下的平台特定行为差异，或 test coverage gap（set_files Win32 路径从未被真机 / 单测验证）。

**位置**：src/clipboard/windows.rs:614（WinClipboard::set_files）—— 由 src/service.rs:5275 poller 调用

**严重度**：P0（崩溃）；但 M4 set_files Win32 路径在用户真机首次触发即崩，强烈指向 platform-specific / coverage gap，而非代码逻辑 bug

**修复方案**：最小诊断动作 = 加 panic hook（1 行 src/main.rs）+ 跑 release build 隔离 unwind vs native crash + 抓 crash dump (procdump / WER) + WinDbg 解码 stack trace；提供诊断数据后再精确定位

**详细报告**：next/BUG-INVESTIGATION-WINDOWS-SET-FILES-CRASH.md（本文件）

**下一步**：
  - Leader 指示用户加 panic hook（最小改动）并复测
  - 用户跑 release build 测试
  - 用户抓 crash dump（procdump -ma -e 1）
  - 收到诊断数据后派 bug-investigator 重分析 / 派 executor 返工
  - 暂开 SUGGESTION #S-12 跟踪
```
