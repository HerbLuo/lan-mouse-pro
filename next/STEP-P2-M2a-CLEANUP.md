# STEP P2 M2a CLEANUP — validator P2.1 + P2.2 dead code 清理

> 触发：validator 报告 `next/STEP-VALIDATION-P2-M2a.md` §3 P2.1 + P2.2
> 执行日期：2026-09-10　实际耗时：~10 min
> 起点 commit：`a74bd7d`（validator archive）→ 终点 commit：（待 leader 提交）
> 改动文件：`src/clipboard/macos.rs`（单文件，135 行删除 + 21 行新增，净减 114 行）
> 结论：✅ 通过

## 1. 做了什么

按 validator §6 §1 的 "选项 A（M2a 收尾）" 路线，删除两项 dead code：

### P2.1 — 删除 `last_image_change_count: Cell<Option<i64>>`

- 删除字段 + 字段 docstring（macos.rs:113-119 原）
- 删除 `MacOsPasteboard::new()` 中的初始化（macos.rs:156 原）
- 删除 `current_image()` 末尾的 `self.last_image_change_count.set(...)`（macos.rs:254-255 原）
- 删除 `set_image()` 成功分支里的 `self.last_image_change_count.set(...)`（macos.rs:288-289 原）
- 简化 `current_image()` 体：`let result = read_image_bytes_from_pasteboard(&pb); result` → `read_image_bytes_from_pasteboard(&pb)`（直接返回）
- 清理 `current_image` doc 中 "changeCount is checked inside `watch_image`" 段落 —— 改为说明 dispatcher 已经用 fingerprint 短路，changeCount 优化在本方法无收益（trait 语义不应改变）

### P2.2 — 删除 `watch_image()` 方法整个 impl

- 删除 `IMAGE_WATCH_TICK` 常量（500ms，macos.rs:86-92 原）—— 仅 `watch_image` 使用
- 删除 `watch_image(&mut self) -> BoxStream<'static, ImageChange>` 整个方法 + 详细 doc（macos.rs:297-366 原，约 70 行）
- 删除模块级 doc 中关于 `watch_image` 线程模型的段落（macos.rs:36-47 原）—— 改为说明 `NSPasteboard::generalPasteboard()` 的 thread-safe 保证 + `new()` 中的 autorelease pool 绑定
- 清理 image helpers doc（macos.rs:257-262 / 269-272 / 304-306 原）—— 移除 "watch_image 的 spawn_local task" / "watch_image channel" 描述；改为 "dispatcher's tick task"

### 关联清理（scope 内）

- 删除不再使用的 imports：
  - `use std::cell::Cell;`（仅 `last_image_change_count` 用）
  - `use futures::channel::mpsc;`（仅 `watch_image` 用）
  - `use futures::{SinkExt, StreamExt};`（仅 `watch_image` 用）
  - `use tokio::time::{MissedTickBehavior, interval};`（仅 `watch_image` 用）
  - `use std::time::Duration;`（仅 `IMAGE_WATCH_TICK` 用）
- 从 `use super::{...}` 中删除 `ImageChange`（仅 `watch_image` 用）
- **保留**：`ImageBytes`、`Mime`、`ClipboardBackend`、`ClipboardError`、`NSPasteboard`、`NSData`、`NSString`、`Write`、`Command`、`Stdio`（仍被 `current_text` / `set_text` / `current_image` / `set_image` / 测试使用）

### 未触碰（scope 外）

- `src/clipboard/mod.rs` 中 `ClipboardBackend::watch_image` 的 trait 默认 impl（line 401）保留 —— Linux/Windows backend 继承 default impl（empty stream）即可
- `src/clipboard/macos.rs` 中 `mod tests` 完全保留（10 个测试均不引用 dead code，grep 确认）
- `src/service.rs` / `src/quic_transport/http3.rs` / dispatcher / cache 完全未触碰

## 2. 验证结果

```
$ cargo build --workspace
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 9.70s
```

```
$ cargo test --workspace
    101 passed; 0 failed
    208 passed; 0 failed
      2 passed; 0 failed (3 ignored — macOS-only tests 跳过 by env)
      7 passed; 0 failed
      2 passed; 0 failed
     26 passed; 0 failed
     29 passed; 0 failed
    ─────────────────
    375 pass / 0 fail   ← baseline 375 维持
```

```
$ cargo fmt --all -- --check | grep "Diff in"
    Diff in src/connect.rs:1084   ← pre-existing cosmetic drift（M1a 阶段已记录，与本 STEP 无关）
    Diff in src/listen.rs:1006    ← pre-existing cosmetic drift（同源）
    （macos.rs 清洁）
```

```
$ cargo clippy --workspace --all-targets -- -D warnings | grep -c "^error"
    14   ← baseline 14 errors 维持（pre-existing，与 M1b / M2a 无关）
$ cargo clippy ... | grep "macos\.rs\|watch_image\|last_image_change"
    （无 macos.rs 相关 clippy 错误）
```

## 3. 与 PLAN 的偏差

- **无**。本 STEP 不属于 PLAN §3 任一 STEP，是 validator P2 验收后的清理任务。PLAN 文档 (`next/PLAN-2-CLIPBOARD.md`) 未提及 `last_image_change_count` 或 `watch_image` macOS impl（trait 默认 impl 是 PLAN 设计意图），所以本清理属于"实现回到 PLAN 设计的本意"而非"偏离 PLAN"。

## 4. 处理的 SUGGESTION 项

- **未处理**：本清理范围不在 `SUGGESTION.md` 当前活跃项（#S-1 / #S-2 / #S-3）内。
- **未新增**：删除 dead code 不引入新问题。

## 5. 闸门检查

| 闸门 | 状态 | 说明 |
|---|---|---|
| 闸 1 — 时间门 | ✅ | 估时 20 min，实际 ~10 min |
| 闸 1 — milestone 边界门 | ✅ | 仅触碰 `src/clipboard/macos.rs`（M2a 既有文件）；未触碰后续 M2b / M3a / M4 范围 |
| 闸 1 — 产物对得上 | ✅ | 删除项与 validator §3 P2.1 / P2.2 描述完全对应 |
| 闸 1 — 依赖对得上 | ✅ | 不依赖其他 STEP；M2a 4 STEP 既有契约（current_image / set_image / macOS 测试）全部保留 |
| 闸 2 — 执行中 | ✅ | 无偏差；fmt 微调（删常量后的多余空行）已修正 |
| 闸 3 — STEP 回归 | ✅ | 375 pass baseline 维持；无 macos.rs 相关 clippy 新增 |

## 6. 遗留

- validator §3 P2.3 / P2.4 + §4 P3.1-P3.6 全部押后到 M4 cleanup 阶段（leader 决策）
- pre-existing 14 clippy errors（与 M2a 无关，留待统一处理）
- pre-existing `connect.rs:1084` / `listen.rs:1006` fmt drift（与 M2a 无关，留待 fmt sweep）

## 7. 下一步

- Leader 评审本 STEP + 决定是否 commit（commit message 模板见下）
- 启动 M2b（Windows CF_DIBV5 + Linux X11/Wayland）前无需其他前置
- M2a 真机 4K 截图 / TIFF→PNG / 1080p JPG / 图片回环矩阵仍为用户责任（PLAN §8）

## 8. 建议 commit message

```
chore(clipboard): drop dead last_image_change_count + watch_image impl (M2a validator P2.1+P2.2)

M2a validator flagged two dead-code items in src/clipboard/macos.rs:

- P2.1 `last_image_change_count: Cell<Option<i64>>` — written by set_image
  + current_image but never read (dispatcher uses sha256 fingerprint
  short-circuit, not changeCount)
- P2.2 `watch_image()` method — full impl but never consumed by
  dispatcher (which only uses current_image() 500ms tick); trait default
  impl in src/clipboard/mod.rs already returns empty stream, so removing
  the macOS override is safe

Cleanup:
- Remove last_image_change_count field, writes, init, doc
- Remove watch_image method + IMAGE_WATCH_TICK constant + unused imports
  (Cell, mpsc, SinkExt, StreamExt, MissedTickBehavior, interval, Duration,
   ImageChange)
- Update doc comments that referenced watch_image / changeCount polling
- Net: -114 lines (135 deletions, 21 insertions) in macos.rs

Scope: src/clipboard/macos.rs only. ClipboardBackend trait default impl
in mod.rs preserved. mod tests preserved (10 macos tests unchanged).
No wire-format / IPC / public API changes.

验证:
- cargo build --workspace: clean
- cargo test --workspace: 375 pass / 0 fail (baseline maintained)
- cargo fmt --all -- --check: macos.rs clean (connect.rs/listen.rs
  pre-existing drift untouched)
- cargo clippy --workspace --all-targets -- -D warnings: 14 errors
  (baseline; no new, no macos.rs-related)

归档: next/STEP-P2-M2a-CLEANUP.md
```

---

> **执行人**：plan-step-executor
> **报告路径**：`/Users/hb/Projects/@cloudself/lan-mouse-pro/next/STEP-P2-M2a-CLEANUP.md`