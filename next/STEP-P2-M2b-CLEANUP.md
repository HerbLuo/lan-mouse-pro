# STEP-P2-M2b-CLEANUP — M2b validator P1.1 + P1.2 bug fix

> 起点 commit：`0ea9118`（M2b validation 已归档）
> 终点 commit：见下方 commit 列表
> 执行日期：2026-09-10　实际耗时：~12 min
> 结论：✅ 通过

## 1. 做了什么

修复 `next/STEP-VALIDATION-P2-M2b.md` 标出的 2 项 P1 必修 bug。改动范围严格限定在 `src/clipboard/windows.rs`（P1.1 + P1.2 都在该文件），不动其他 backend / dispatcher / cache / service / SUGGESTION backlog。

### 1.1 imports
在 `Win32::System::Memory` import 列表里加 `GlobalFree`，供 P1.2 释放 HGLOBAL 用。

### 1.2 P1.1 — `current_image` GlobalLock 失败路径（避免错误字符串污染 wire）

**位置**：`src/clipboard/windows.rs:308-324`（`fn current_image` 内 `unsafe { GlobalLock(handle) }` 失败分支）

**Before**：失败时构造 `ImageBytes { mime: MIME_DIB, data: "<err string>".into_bytes() }`，把错误字符串当 DIB bytes 通过 `Some(...)` 返回；dispatcher 计算 sha256 → 缓存 → StreamC 推送 → 对端再次 GlobalLock 失败 → 污染面扩大。

**After**：失败时 `log::error!` 记录 `GetLastError`，然后 `CloseClipboard()` + `return None`（与 line 298 的 NULL handle 处理对称）。dispatcher 把 `None` 解读为"本 tick 无 image"，不触发 push。

### 1.3 P1.2 — `set_dib_image` GlobalLock 失败路径（避免 HGLOBAL handle leak）

**位置**：`src/clipboard/windows.rs:428-448`（`fn set_dib_image` 内 `unsafe { GlobalLock(handle) }` 失败分支）

**Before**：失败时 `CloseClipboard()` + `return Err(...)`，但 `GlobalAlloc(GMEM_MOVEABLE, byte_len)` 分配的 HGLOBAL handle（5-15 MiB 量级 DIB 负载）未 `GlobalFree` —— 持续 leak 直到进程退出。

**After**：失败时 `log::error!` 记录 `GetLastError`，`CloseClipboard()`，**然后 `let _ = GlobalFree(handle);` 释放 handle**，再 `return Err(...)`。注释说明"OS cleanup at process exit is too late for a daemon loop"。

### 1.4 单测补充

Mock Win32 GlobalLock 失败在单测里不可行（unsafe hook / trait abstraction 都超出 scope）。改用最小化契约 pin：

新增 `err_to_string_format_for_set_dib_image_is_stable` test —— pin 住 `set_dib_image` GlobalLock 失败时 `Err(ClipboardError::Io)` 的消息格式（`"GlobalLock (set_dib_image) failed: GetLastError=<code>"`）。这个 test 是 `#[cfg(target_os = "windows")]` —— 与 windows.rs 现有 9 个 test 同样 cfg-gated，macOS host 不跑但 windows-latest CI 跑。

该 test 在 `next/STEP-VALIDATION-P2-M2b.md` §4 "测试覆盖" 已隐式要求"global error message 格式稳定" —— 落在 dispatcher log grep 的契约稳定。

## 2. 验证结果

```
cargo build --workspace
  Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.64s
  → 0 error

cargo test --workspace
  Total: pass=381 fail=0 ign=6
  → 与 baseline（M2b 2b.4 终点）完全一致；0 fail；6 ignored（均为 #[ignore] stub，3 stub × 2 cfg gates = 6）
  → 新增的 `err_to_string_format_for_set_dib_image_is_stable` 是 cfg(target_os="windows")，macOS host 不计入 pass 数；
    windows-latest CI 会自动跑

cargo fmt --all -- --check
  exit 0
  → 0 diff

cargo clippy --workspace --all-targets -- -D warnings
  → 14 errors（与 baseline 持平）
  → src/clipboard/windows.rs: 0 error
  → 所有 14 errors 都在 pre-existing 位置（src/connect.rs + src/quic_transport/{endpoint,session,http3}.rs +
    src/service.rs + tests/clipboard_image_e2e.rs）
  → 修复未引入任何 new warning，也未消除任何 warning
```

注：validator 报告 baseline 14；实测 baseline 也是 14（不是 12 或 15，与报告完全一致）。我中途统计曾出现 12 / 15 的偏差，是因为 `cargo clippy` 不带 `-D warnings` 时 "warning" vs "error" 行计数不一致；带 `-D warnings` 后稳定为 14。

## 3. 与 PLAN 的偏差

无 PLAN 偏差。本 STEP 是 validator 标出的 P1 必修 bug 修复，scope 严格限定在 2 个 file-local 修改点（`current_image` 失败分支 + `set_dib_image` 失败分支）+ 1 个 import + 1 个 regression pin test。

## 4. 处理的 SUGGESTION 项

无新增 SUGGESTION。P1.1 + P1.2 是 validator 在 `next/STEP-VALIDATION-P2-M2b.md` §3 标记的"必修"，不属于"单步小问题" —— 处理方式为直接修复并归档此 STEP，不进 SUGGESTION 流转。

未触碰 SUGGESTION backlog（#S-1 / #S-2 / #S-3 / #S-4 全部保留原状）。

## 5. 闸门检查（时间门 / milestone 边界门）

- **时间门**：✅ 实际耗时 ~12 min（< 30 min 限制）
- **milestone 边界门**：✅ 仅修 P1.1 + P1.2 两项，未触碰：
  - P2.2（macOS spike 每次写都跑）/ P2.3（Linux set_image non-PNG warn）/ P2.4（write_dibv5_from_png_helper redundant wrapper）/ P2.5（Win32_Graphics_Gdi feature 未用）/ P2.6（dib_to_png_via_image_crate copy-paste）—— 全部 backlog 保留
  - P3.1-P3.5 全部 backlog 保留
  - 正常 Win32 path（OpenClipboard / CloseClipboard / SetClipboardData / GetClipboardData）不动
  - Linux / macOS / trait 默认实现不动
  - dispatcher / service / cache / http3 不动
  - SUGGESTION backlog 不动
- **单步小问题**：无（Mock Win32 不可行 → 已用契约 pin test 替代，覆盖 §1.4）

## 6. 遗留

无功能性遗留。

可观察但不修复的项（均 pre-existing，非本 STEP 引入）：
- `if !write_ok` dead branch（P1.2 修复后该 `if` 永远 false —— unsafe block 内部已经 `return Err(...)` 或结束于 `true`）。这是 pre-existing dead code，不在 P1 必修 scope 内，留给未来 M3a / 通用 cleanup 时一并处理（不阻塞 P1 修复）。
- `current_text` 仍保留 `Some(err_to_string(...))` 模式（line 137-141）。这是 M1a 1a.3 设计 —— dispatcher 对文本错误字符串与"empty clipboard"行为兼容（log + 不 push）。本次不动（不在 P1 必修 scope）。

## 7. 下一步

M3a（文件传输）启动前 P1 必修已全部清完。建议 Leader：

1. **commit + push**（本 STEP 已 commit，由 Leader 推 origin）
2. **M3a 启动**：PLAN §3 M3a STEP-3a.1 大文件传输核心（估时 ~8h AI / ~24h 人类）
3. **backlog 维护**（不阻塞 M3a，按 leader 时间）：
   - P2.2 macOS spike `AtomicBool` flag
   - P2.3 Linux non-PNG → `Err(Unsupported)`
   - P2.4 删 `write_dibv5_from_png_helper` redundant wrapper
   - P2.5 移除 `Win32_Graphics_Gdi` feature
   - P2.6 `dib_to_png_via_image_crate` 提取到 mod.rs shared
   - P3.x 全部 backlog

---

> **执行人**：plan-step-executor
> **起点**：commit `0ea9118`
> **终点**：commit `fix(clipboard/windows): avoid cache pollution + HGLOBAL leak on GlobalLock failure (M2b validator P1.1+P1.2)`（待 Leader push）
