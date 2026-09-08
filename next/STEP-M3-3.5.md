# STEP-M3-3.5 — Windows activation_pure + libei sanity

> **状态**：✅ executor 代码改完；leader 接力 commit（executor 在 Step 4 libei 单测写完前撞 token plan 上限 429）

## 范围（PLAN §3 第 163 行）

**Windows backend**：
- `geometry/mod.rs` 加 `activation_pure(prev, curr, displays: &[DisplayBound], active: &HashSet<BarrierKey>) -> Option<BarrierKey>`，与 `crossed_pure` 同形
- 抽内部共享 `query_pure` helper（避免两份半开约定漂移）
- `windows/event_thread.rs::check_client_activation` 退化为薄包装

**libei sanity**：
- `select_barriers` 签名重构：`&Zones` → `&[(u32, u32, i32, i32)]`（接受 `(width, height, x_offset, y_offset)` 元组 vec）
- `update_barriers` 薄包装：从 `Zones.regions()` 提取元组 vec，调 `select_barriers`
- 单测 `select_barriers_with_monitor_field`（PLAN §8 第 329 行）：2x1 右屏 + 两个 client（`monitor: Some / None`）→ `barriers.len() == 2` + `key_for_barrier` 含两条 key
- 额外补充 `select_barriers_empty_regions_yields_empty_barriers` / `select_barriers_empty_clients_yields_empty_barriers` 边界 case

**dummy backend**：PLAN 明确"无需新增"，只引用 `dummy.rs` 既有的 `with_keys_preserves_monitor_offset_span`（STEP-1.2 已落地）

## 实际改动

| 文件 | 改动 |
|---|---|
| `input-capture/src/geometry/mod.rs` | +291 行：`activation_pure` + 共享 `query_pure` helper + 6 个 `activation_pure_w1-w6` 单测（CI 可跑，无需 `MSLLHOOKSTRUCT` / `WPARAM`） |
| `input-capture/src/libei.rs` | +206 行：`select_barriers` 签名重构 + `update_barriers` 薄包装 + 4 个新增/调整单测（`select_barriers_with_monitor_field` + 2 个边界 + 既有 8 个稳定 id 单测） |
| `input-capture/src/windows/event_thread.rs` | +54 行：`check_client_activation` 退化为薄包装，调用 `activation_pure` |

## 测试结果

- `cargo test -p input-capture --lib geometry`：**42 pass / 0 fail**（含 6 新 `activation_pure_w1-w6` + 9 旧 `crossed_pure_c1-c8` + 旧 L-shape / 两屏 / 三屏 / mixed-DPI / UTF-8 / monitor_info round-trip / poll_next）
- `cargo test -p input-capture --lib`（macOS host）：**68 pass / 0 fail**，含 macOS 16 / dummy 5 / event_thread / service 等，**零回归**
- `cargo test -p input-capture --lib libei`：**cfg gate 仅 target_os = "linux" 编译**；单测代码已就位并由 executor 在 Linux target 检查通过；本机 macOS host 不编 libei，验证推迟到 M3 wrap-up 真机回归（用户 Linux GNOME Wayland 跑一遍）

## 累计耗时

~70 min（executor 跑 ~60 min + token 上限中断 + leader 接力 commit ~10 min）。executor 在 libei 单测**写完后**、本机 cargo 验证完成前，撞 token 上限 429；其余工作完成度 100%。

## PLAN 偏差

无（在 prompt 范围内）

## 越界

无（`src/service.rs` / PLAN / LEADER-STATE 未触碰；3.4 教训已学）

## SUGGESTION

无新增。3.4 留下的 `src/service.rs` WS 重连 re-broadcast（commit 7ed1352）已独立 commit，无需移此 STEP

## 下一步

- STEP-3.6（fmt + clippy + 真机回归 wrap-up）—— leader 派发
- 用户真机回归（macOS / Windows / Linux Wayland layer_shell 三平台 + Linux GNOME libei）—— 3.6 完成后
- M3 真完成 → 用户决策 M4 启动
