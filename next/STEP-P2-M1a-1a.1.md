# STEP-P2-M1a-1a.1 — ClipboardBackend trait + DummyBackend

> **状态**：✅ 通过（trait 编译通过；8 个 DummyBackend 单测全绿）
> **执行日期**：2026-09-08　实际耗时：~10 min
> **结论**：通过，无 PLAN 偏差

---

## 1. 做了什么

按 PLAN-2 §3 M1a STEP-1a.1 行落地 `src/clipboard/mod.rs`：

- `ClipboardBackend` trait（`name` / `current_text` / `set_text` / `can_clear` 默认 true）—— 平台抽象接口
- `ClipboardError` enum（thiserror）：`NotImplemented` / `ToolMissing` / `ToolFailed` / `Io`
- `DummyBackend`（in-memory mock，`new()` / `with_text()` 两个构造器）
- `default_backend()` 工厂：返回 `Err(NotImplemented)`（平台实现 1a.2 / 1a.3 落地后填实）
- 8 个单测覆盖 trait 契约

### 1.1 文件改动

| 文件 | 改动 |
|---|---|
| `src/clipboard/mod.rs`（新，~280 行） | trait + 错误类型 + DummyBackend + factory + 8 单测；详尽 doc-comment 解释为何 text-only、为何 poll-based 而非 `watch()` async、`Send` 而非 `Sync` |
| `src/lib.rs` | `pub(crate) mod clipboard;`（仅 crate 内可见，等 1a.4 dispatcher 才决定是否 re-export） |

### 1.2 trait 设计要点

- **Text-only for M1a**：`current_text` / `set_text` 两个核心方法；`current_image` / `set_image` / `current_files` / `set_files` 等 M2a / M3a 再加。
- **Poll-based 而非 watch async**：dispatcher 拥有 500ms tick 循环，自己 hash 比对避免重复 push。NSPasteboard `changeCount` 在 macOS 上能省一次 read，但加 `watch()` 会泄漏 `NSRunLoop` / `wl_display` 跨 trait 边界，代价高于收益。
- **`Send` 而非 `Sync`**：trait 对象仅在 `spawn_local` 单 task 内被 dispatcher 持有，不会跨 task 共享。
- **`can_clear` 默认 true**：forward-compat hook，M2a+ 可能加 read-only backend。

### 1.3 测试结果

- `cargo build -p lan-mouse`：✅ 0 error / 0 warning（用 `#![allow(dead_code)]` 模块级抑制"trait/dummy 还没被 dispatcher 引用"的预期警告——等 1a.4 dispatcher 落地后这条 allow 可移除）
- `cargo test -p lan-mouse --lib clipboard`：✅ **8 passed / 0 failed**（`dummy_backend_round_trip` / `with_text_initial_state` / `set_same_text_is_noop` / `empty_string_is_distinct_from_none` / `name` / `can_clear_default_true` / `default_backend_returns_not_implemented_until_platform_files_land` / `clipboard_error_display_messages_are_stable`）
- `cargo test -p lan-mouse --lib`：✅ **116 passed / 0 failed**（基线 108 + 新 8）
- 关键单测覆盖：
  - `dummy_backend_empty_string_is_distinct_from_none` —— empty 字符串与 `None` 区分（fingerprint 短路依赖此区分）
  - `clipboard_error_display_messages_are_stable` —— 错误信息固定字串（M3b manual log grep 依赖）

## 2. 累计耗时

~10 min（trait + dummy 都很轻量；多花在 doc-comment + 测试设计上）

## 3. PLAN 偏差

**0 处**（与 PLAN §3 M1a STEP-1a.1 行 100% 一致）

## 4. 下一步

→ STEP-1a.2（macOS NSPasteboard / pbcopy-pbpaste 实现 + 单测）
→ 或按 leader 拆步指令分阶段派 STEP-1a.2 → 1a.3 → 1a.4 → 1a.5
→ commit（leader 拆 fmt-only / 真逻辑）