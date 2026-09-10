# STEP P2-M1b-CLEANUP — M1b validator P2.1 dead code 清理

> 触发：M1b validator `next/STEP-VALIDATION-P2-M1b.md` §3 P2.1 / §6.1 第 1 项
> 执行日期：2026-09-10　实际耗时：~10 min
> 结论：✅ 通过

## 1. 做了什么

1b.1 (`8daaa1d`) 引入 `Service::pending_clipboard_requests: HashMap<[u8; 32], ()>` + `register_pending_clipboard_request` helper 作为 "metadata-only registered but not yet pulled" 的 stop-gap；1b.2 (`b65236a`) 取代语义为源端 `cache.remove(prev_sha)` + 接收端 HTTP/3 GET（`Http3Client::get_text`）后，这段 dead code 加上 `#[allow(dead_code)]` 注解保留到本步。本步按 validator §6.1 删除：

| 位置（删前） | 删前内容 | 删后 |
|---|---|---|
| `src/service.rs:149-159` | 字段 `pending_clipboard_requests` 及其 10 行 doc-comment + `#[allow(dead_code)]` | 移除（紧邻字段 `clipboard_last_text: Option<String>`） |
| `src/service.rs:744` | `pending_clipboard_requests: Default::default(),`（`Service::new` 初始化） | 移除（紧邻字段 `clipboard_last_text: None`） |
| `src/service.rs:2386-2399` | `#[cfg(test)] fn register_pending_clipboard_request(...)` 函数 + 4 行 doc-comment | 移除 |
| `src/service.rs:2664-2685` | `mod clipboard_tests` + `fn metadata_only_text_registers_latest_pending_request_per_hash` 测试（含 1 个测试用例、3 次 `register_pending_clipboard_request` 调用、`use super::register_pending_clipboard_request;`） | 移除 |

## 2. 验证结果

| 命令 | 结果 |
|---|---|
| `cargo build --workspace` | ✅ 0 error 0 warning |
| `cargo test --workspace` | ✅ **339 passed / 0 failed / 3 ignored**（删 1 个测试后；baseline = 340 pass，net = -1；相比 validator 报告 328 pass 增加的 12 个来自 validator 报告后 commit `36c5ce4` + `f0d9751` 引入的测试） |
| `cargo fmt --all -- --check` | ⚠️ 4 diff（**pre-existing** — 与本步无关；commit `36c5ce4` / `f0d9751` 引入；本步 0 新增 diff） |
| `cargo clippy --workspace --all-targets -- -D warnings` | ⚠️ 14 errors（**pre-existing** — 与本步无关；M1a baseline 14 个 `doc_lazy_continuation` / `assertions_on_constants` / `too_many_arguments`；本步 0 新增 clippy） |

### 2.1 grep 验证

```
$ grep -rn "pending_clipboard_requests\|register_pending_clipboard_request\|metadata_only_text_registers_latest_pending_request_per_hash" src/ tests/
（无匹配 — 仅 next/*.md 归档 / 报告文件保留历史引用）
```

`tests/clipboard_text_e2e.rs:51, :218, :230` 的 3 处 doc-comment 引用 `src/service.rs::register_pending_clipboard_request` 仍然存在，**属于 P2.2 范畴**（validator §6.2 押后到 M2a），按 scope discipline 不在本步处理。这些 doc-comment 在本步之前已是 stale（指向错的契约 pin 位置 — 实际 active eviction 契约 pin 在 `src/clipboard/cache.rs::tests::active_eviction_concurrent_with_lookup_old_returns_miss`），本步删除函数后变本步**更进一步 stale**。建议 M2a 阶段按 validator P2.2 修复时一并清理。

## 3. 与 PLAN 的偏差

无（完全按 validator §6.1 第 1 项清单执行；未触碰 §6.2 / §6.3 任何 P2 / P3 backlog；未触碰 1b.1 / 1b.2 / 1b.3 / 1b.4 任何生产代码；未触碰 dispatcher / receiver / cache / metrics / http3 逻辑）。

## 4. 处理的 SUGGESTION 项

- `#S-3` ⚪ — `src/clipboard` 模块 `pub(crate)` 阻碍集成测试 stub un-stub（SUGGESTION.md）。本步删除了 `#S-3` 中提到的 `src/service.rs::register_pending_clipboard_request`，更新 `#S-3` 现象段把契约 pin 位置改为 `src/clipboard/cache.rs::tests::active_eviction_concurrent_with_lookup_old_returns_miss`（与 validator P2.2 描述一致 — `#S-3` 的 action item 不再依赖被删的 helper）。原 `#S-3` 建议（pub(crate) → pub for M4 Toaster）保留。
- 新增 FIXED **#12** — M1b validator P2.1 dead code 清理（本步）。移入 `next/SUGGESTION-FIXED.md`。

## 5. 闸门检查（时间门 / milestone 边界门）

- **时间门**：~10 min（≤ 60 min 限制） ✅
- **milestone 边界门**：仅删 P2.1 4 处 dead code；未触碰后续 M2a / M4 范围；未触碰 1b.1 / 1b.2 / 1b.3 / 1b.4 既有代码 ✅

## 6. 遗留

- **P2.2** — `tests/clipboard_text_e2e.rs:51 / :218 / :230` doc-comment 引用 `src/service.rs::register_pending_clipboard_request`（已删除）。validator §6.2 计划 M2a 阶段处理（修改为正确 pin 位置 `src/clipboard/cache.rs::tests::active_eviction_concurrent_with_lookup_old_returns_miss`）。
- **P2.3** / **P2.4** — validator §6.2 押后到 M2a；本步未触碰。
- **P3.1-P3.6** — validator §6.3 风格 / 微优化；本步未触碰。
- **fmt / clippy pre-existing 4+14 baseline** — 与本步无关；由 `36c5ce4` / `f0d9751` 引入；待后续 cleanup PR 处理（不在 P2.1 范围）。

## 7. 下一步

- M2a 启动条件（validator §6.5）：M1b ✅ + 用户真机大文本双向通过 + P2.1 清理 ✅ → 可派 `STEP-P2-M2a-2a.1`（PLAN §3 M2a：`ClipboardBackend::current_image` / `set_image` + mime 检测）
- 同步修 P2.2 doc-comment（P2.1 引发的 further staleness）
