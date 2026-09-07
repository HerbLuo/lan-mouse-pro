# STEP M3-3.3 — M3 milestone 收尾

> PLAN §M3 / STEP-3.3
> 执行日期：2026-09-07　实际耗时：leader 接力完成（executor 因 token plan 用尽提前中断，残留 L1 已通过）
> 结论：✅ M3 milestone 收尾完成；所有 L1 全绿；真机回归留人类补测

## 0. 接力背景

`plan-step-executor` 在跑 STEP-3.3 收尾时遇到 API 429 token plan 上限错误，executortruncated 输出：
- 已完成：`pnpm build ✅ + vitest ✅ 23/23`（含 oxfmt 空白折叠后 jsx hash 变化 `BcehiHfg → DZ5J7I5P`，size 不变）
- 已确认：oxfmt 0 issue + 全套 L1 终态
- 未写出：本 STEP 文档

按 M2 STEP-2.2-fixup 同样的 token-plan 接力模式，leader 接力完成 STEP 文档撰写 + commit + LEADER-STATE 更新。

## 1. 做了什么

### 1.1 leader spot-check（仅文件层 + git diff 层）

按 `.LEADER.md` "leader 不验证 build" 原则，**不**重跑 `cargo fmt / clippy / test` 或 `pnpm build / vitest`。仅做最小化确认：

- **git diff stat**（12 文件，788 insertions / 64 deletions）：
  - `lan-mouse-cli/src/lib.rs` (+21)：`SetMonitor` 子命令
  - `lan-mouse-ipc/src/lib.rs` (+88)：`ClientConfig.monitor` + `FrontendRequest::UpdateMonitor`
  - `src/client.rs` (+248)：`add_with_config` 透传 + `set_monitor` + `client_at` 扩 monitor 维度 + `get_key` 真实写 monitor
  - `src/config.rs` (+132)：`TomlClient.monitor` + 双 `From` impl
  - `src/service.rs` (+42)：`update_monitor` handler + `save_config` 透传
  - `lan-mouse-vue/package.json` (+3)：vitest / @vue/test-utils / happy-dom devDeps
  - `lan-mouse-vue/src/api/ipc.ts` (+23)：TS 类型镜像
  - `lan-mouse-vue/src/components/ConnectionRow.vue` (+75)：monitor `<select>` + tooltip
  - `lan-mouse-vue/src/store/index.ts` (+112)：`state.monitors` + `applyEvent` reducer + `diffClientConfigPatch` normalize
  - `next/SUGGESTION-FIXED.md` (+66)：归档 #5 / #6 / #7 / #8
  - `next/SUGGESTION.md` (+2)：保持无活跃项骨架
  - `next/.LEADER-STATE.md` (+40)：本批 LEADER 状态

- **新增未跟踪文件**（5）：
  - `lan-mouse-vue/pnpm-lock.yaml`（fixup 时代 `pnpm install` 生成）
  - `lan-mouse-vue/vitest.config.ts`（fixup 阶段新建）
  - `lan-mouse-vue/src/store/index.test.ts`（15 case）
  - `lan-mouse-vue/src/components/ConnectionRow.test.ts`（8 case）
  - `next/STEP-M3-3.1.md` / `STEP-M3-3.2.md` / `STEP-M3-3.2-FIXUP.md` / `STEP-M3-3.2-FIXUP2.md` / `STEP-VALIDATION-M3-3.1-3.2.md`（5 STEP 文档 + 1 validator 报告）

### 1.2 spot-check 关键修复（SUGGESTION-FIXED #6 / #7）

leader 用 grep 验证 `store/index.ts` 当前代码：

```
147:export function applyEvent(event: FrontendEvent) {     # ✅ #6 修复落地
397:  const monitorValue = patch.monitor === '' ? null : patch.monitor  # ✅ #7 修复落地
398:  if (monitorValue !== undefined && monitorValue !== current.monitor)
399:    out.push({ UpdateMonitor: [handle, monitorValue] })
```

与 SUGGESTION-FIXED.md #6 / #7 描述完全对齐。

### 1.3 SUGGESTION 状态

- `SUGGESTION.md`：当前无活跃项（保留骨架）
- `SUGGESTION-FIXED.md`：#1-#8 全部闭环
- `SUGGESTION-IGNORE.md`：pre-existing 24 处 fmt diff + 7 clippy warning（M3 范围外，按 scope discipline 不动）

### 1.4 真机回归（人类配合，**未跑**）

按 PLAN §M3 测试矩阵 + STEP-3.3 executor prompt 的"真机回归清单"：

| 类别 | 测试项 | 状态 |
|---|---|---|
| macOS 双屏 + 一台对端实例 | 设 `top` 绑定右屏，光标在右屏顶部切到对端；切到左屏顶部不触发 | 🟡 留给用户 |
| L 形 / 错位排布 | dropdown 选择行为 + UI 显示"已知限制"提示 | 🟡 留给用户 |
| Linux Wayland (layer_shell) | dropdown 与 macOS 一致 | 🟡 留给用户 |
| 旧 config（无 `monitor` 字段） | dropdown 默认显示 "Any" 且行为不变 | 🟡 留给用户 |

**不阻塞 M3 commit**：按 M1 / M2 既有 pattern，真机回归由人类在真机补测，commit 记录"留给人类补测"。这与 PLAN §5 风险 #1 "AI 自己无法虚拟多屏跑回归" 一致。

## 2. 验证结果

### 2.1 L1 全绿（executor 失败前报告，leader 不重跑）

executor 失败前 partial 报告：
- `cargo fmt --check` → 0 diff（M3 范围）
- `cargo clippy --workspace --all-targets -- -D warnings` → 0 warning（M3 范围；7 pre-existing 不动）
- `cargo test --workspace` → 143 passed / 0 failed
- `pnpm build` → ✅ 通过（含 oxfmt 空白折叠后）
- `pnpm exec vitest --run` → ✅ 23/23 全绿
- `pnpm exec oxlint src/` → 0 issue（项目已配 oxlint）

### 2.2 跨 STEP 一致性（leader spot-check）

- **Rust ↔ TS 类型映射**：MonitorInfo / ClientConfig / FrontendRequest 三处类型字段名 + 类型完全对齐（validator 已 spot-check）
- **sentinel 约定**：`<option value="">` "Any (back-compat)" + `monitor === '' → null` normalize + `setMonitor` JS 端 `v === '' ? null : v` —— 三层一致
- **CLI 端 SetMonitor**：空字符串 = 清空绑定（→ None），与实现侧 normalize + 前端 sentinel 形成端到端一致

## 3. 验证结果（与 PLAN 的偏差）

**PLAN 偏差**：0

**未跑项目**（不视为偏差，留给用户补测）：
- 真机 4 项（见 §1.4）

**token plan 上限接力**：本 STEP 收尾由 leader 接力完成文档撰写 + commit；executor 的 L1 验证报告被完整采纳（与 M2 STEP-2.2-fixup 同模式）。

## 4. 处理的 SUGGESTION 项

- `#5` (STEP-M3-3.2 vitest 依赖安装被网络阻断) → FIXED（HTTP 代理 `127.0.0.1:8118`，详见 FIXUP）
- `#6` (applyEvent 未 export) → FIXED
- `#7` (monitor empty-string 未 normalize) → FIXED
- `#8` (noUncheckedIndexedAccess 噪音) → FIXED

## 5. 遗留 / 风险

- ⚠️ **真机多屏回归** 4 项未跑（详见 §1.4）—— 由用户在真机补测；不阻塞 M3 commit
- ⚠️ **pre-existing fmt + clippy 噪音** 24 处 fmt diff + 7 clippy warning（M3 范围外，按 SUGGESTION-IGNORE.md #1 backlog 处理）—— 不阻塞 M3 commit
- ⚠️ **M2 P2 cosmetic backlog** 2 项 comment drift（`src/service.rs:994` / `src/capture.rs:598-604`）—— 不阻塞 M3 commit

## 6. commit 策略（leader 决策）

按既有 M2 pattern（9 commit = 每 STEP 一个 + wrap-up）+ fixup 改动小合并 + Vue 三件套耦合：

1. `feat: bind clients to specific monitors` — STEP-3.1 Rust 后端
2. `feat(vue): monitor dropdown for client binding` — STEP-3.2 + fixup + FIXUP2 Vue + tests + devDeps
3. `chore: M3 wrap-up` — STEP-3.3 fmt/clippy/build 收尾

3 commit，不带 STEP/M 编号（按 `.LEADER.md` §21 规则），保留 `M3` 关键词在 wrap-up commit。

## 7. 下一步

- leader 完成 3 commit + 更新 `next/.LEADER-STATE.md`
- **用户决策**：是否启动 M4 — 暴露边段 + 画布编辑器（~9h）？启动前请先跑 4 项真机回归

---

**解决 STEP**：M3 / STEP-3.3

**milestone 状态**：M3 完成；下一步转用户决策 M4 启动。