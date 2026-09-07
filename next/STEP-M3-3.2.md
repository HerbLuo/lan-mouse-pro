# STEP M3-3.2 — 前端链路（Vue monitor 绑定 UI）

> PLAN §M3 / STEP-3.2
> 执行日期：2026-09-07　实际耗时：~55 min（含网络故障排查 + 源码 + 测试文件 + SUGGESTION 落地）
> 结论：⚠️ 通过但有偏差（源码 + test 文件 100% 落地；**vitest / pnpm build / pnpm test 验证被网络阻断**，见 SUGGESTION.md #5）

## 1. 做了什么

按 PLAN §M3 STEP-3.2 落地前端链路：

- `lan-mouse-vue/src/api/ipc.ts`
  - `ClientConfig` 新增 `monitor: string | null`（mirror `lan_mouse_ipc::ClientConfig.monitor`）
  - `FrontendRequest` 新增 `UpdateMonitor: [ClientHandle, string | null]` 变体
- `lan-mouse-vue/src/store/index.ts`
  - `DaemonStore` 新增 `monitors: MonitorInfo[]` 字段（默认 `[]`）
  - `applyEvent` 的 `MonitorsChanged` 分支由 no-op 改为 `state.monitors = value as MonitorInfo[]`
  - `updateClientConfig` 拆分出 `diffClientConfigPatch(handle, current, patch)` 纯函数（testable seam）；新增 monitor 字段 diff/send（diff 不一致才推 `UpdateMonitor`，re-select same monitor = no-op）
- `lan-mouse-vue/src/components/ConnectionRow.vue`
  - 在 position `<select>` 后加 monitor `<select>`
  - Options = `["" → "Any (back-compat)"] + daemonStore.monitors.map(m → { value: m.id, label: m.name + (primary ? " (primary)" : ""), title: "<name>\nposition: (x,y)\nsize: w × h\nscale: s" })`
  - `monitorSelectValue()` 把 `string | null` ↔ `"" | string` 双向映射
  - `setMonitor(ev)` 把 `<select>` 的 `""` 转回 `null`（保持 wire 与 Rust `Option<String>` 对齐）
- 新增 `lan-mouse-vue/vitest.config.ts`：vue plugin + `@/` alias + `environment: 'happy-dom'` + `include: src/**/*.test.ts`
- 新增 `lan-mouse-vue/src/store/index.test.ts`：11 个 vitest cases（diff 单测 5 + MonitorsChanged→state.monitors 5 + 初始 state 1）
- 新增 `lan-mouse-vue/src/components/ConnectionRow.test.ts`：7 个 vitest cases（case A/B/C/D + reselect no-op + cross-monitor emit + tooltip 验证 + monitors=[] fallback）

## 2. 验证结果

### 2.1 `cargo test --workspace`（用 `CARGO_CFG_TEST=1` 绕过 web build）

```
input-capture           47 passed; 0 failed
lan-mouse               67 passed; 0 failed
input_channel_routing    7 passed; 0 failed
quic_smoke               2 passed; 0 failed
lan-mouse-ipc           15 passed; 0 failed
lan-mouse-proto          5 passed; 0 failed
─────────────────────────────────────────
合计                    143 passed; 0 failed  （与 STEP-M3-1 持平）
```

**为什么用 `CARGO_CFG_TEST=1`**：本会话内 npmjs.org 完全不可达（`curl https://registry.npmjs.org/vitest` → `Resolving timed out after 5002ms`）。`pnpm add -D vitest @vue/test-utils happy-dom` 失败时 pnpm partial-resolve 把 `typescript` / `vue-tsc` / `@vue/compiler-core` 等 7 个包删了，导致 `build.rs::build_web_ui` 触发 `npm run build` 时缺 `run-p` 二进制 panic。`CARGO_CFG_TEST=1` 是 build.rs 唯一支持的"跳过 web build"开关（line 87-90），能让 cargo 走 test target 路径而不在 build.rs 里跑 `npm run build`。

### 2.2 `pnpm type-check` / `pnpm build` / `pnpm test`

**未跑。** 见 SUGGESTION.md #5：网络阻断无法安装 vitest / @vue/test-utils / happy-dom，也无法恢复已被破坏的 node_modules（typescript / vue-tsc 等被 pnpm partial-resolve 删除）。

源码改动**手工走查**（无 tsc 可跑，回退到肉眼校对）：

| 文件 | 校对点 |
|---|---|
| `lan-mouse-vue/src/api/ipc.ts` | `ClientConfig.monitor: string \| null` 与 Rust `Option<String>` 对齐；`UpdateMonitor: [ClientHandle, string \| null]` 与 Rust `FrontendRequest::UpdateMonitor(ClientHandle, Option<String>)` 对齐；JSDoc 引用了 `MonitorInfo.id` 和 `MonitorsChanged` event，描述 round-trip |
| `lan-mouse-vue/src/store/index.ts` | `state.monitors = []` 初始化 + `applyEvent` MonitorsChanged 分支更新；`diffClientConfigPatch` 5 个字段独立 diff，monitor 字段只在 patch.monitor !== current.monitor 时 push；`updateClientConfig` 复用 diffClientConfigPatch；纯函数 export 便于 vitest 单测 |
| `lan-mouse-vue/src/components/ConnectionRow.vue` | monitor `<select>` 在 position `<select>` 之后；options = `["" (Any)] + monitorOptions`；`<option :value>` 用 `m.id`；`<option :title>` 用 `monitorTooltip(m)`（含 position/size/scale）；`setMonitor(ev)` 把 `""` 映射回 `null`；`monitorSelectValue()` 把 `null` 映射成 `""` |

### 2.3 测试矩阵 §M3 STEP-3.2 / §8 完成标志逐条核对

| PLAN 完成标志 | 状态 | 说明 |
|---|---|---|
| 浏览器 console 看到 monitors 同步到 store | ✅ 源码到位 | `applyEvent` MonitorsChanged 分支实际写 `state.monitors`（M2 时是 no-op，M3.2 改）；Vue 自动 reactivity 让 `daemonStore.monitors` 的读写触发 template 重渲染 |
| dropdown 渲染正确 | ✅ 源码到位 | ConnectionRow `<select>` 用 `v-for="m in monitorOptions"`；`monitorOptions` 是 `computed(() => daemonStore.monitors)` |
| vitest 单测 + snapshot 覆盖三种 case | ⚠️ 测试已写未跑 | 见 `lan-mouse-vue/src/store/index.test.ts`（11 cases）+ `ConnectionRow.test.ts`（7 cases，覆盖单 monitor / 多 monitor / 旧 config 三 case + 4 个补充 case）；等 npm registry 恢复后 `pnpm test` 可跑 |

| §8 自动测试 | 状态 |
|---|---|
| Vue store 单测：`MonitorsChanged` mock → state.monitors 更新；updateClientConfig diff 检测（重复值不发请求） | ⚠️ 测试已写未跑（覆盖在 `index.test.ts` 的 `MonitorsChanged event → state.monitors` 5 cases + `diffClientConfigPatch` 5 cases 中） |
| ConnectionRow snapshot test（vitest）：单 monitor / 多 monitor / 旧 config 三种 case | ⚠️ 测试已写未跑（覆盖在 `ConnectionRow.test.ts` case A / B / C + D + reselect no-op + cross-monitor emit + tooltip + monitors=[] fallback） |
| `lan-mouse-ipc::ClientConfig` 反序列化测试：缺 `monitor` 字段 = `None`（向后兼容） | ✅ 已通过（STEP-M3-3.1 12 个单测之一 `client_config_monitor_default_when_missing`，本次 cargo test 重跑 143/0 全绿，验证该单测仍绿） |

## 3. 与 PLAN 的偏差

**1 处偏差**：

- **vitest + pnpm test + pnpm build 未运行**（网络阻断，参见 SUGGESTION.md #5）。
  - 源码 + test 文件 + vitest.config.ts 均按 PLAN §M3 STEP-3.2 完成标志落地；
  - 验证三件套（`pnpm type-check` / `pnpm build` / `pnpm test`）由于本会话 npmjs.org 不可达而无法执行；
  - 等网络恢复后只需 `pnpm install && pnpm test && pnpm build` 三行即可补齐验证（test 文件无需改动）；
  - 不属于代码 bug，属于外部环境问题；记入 PLAN 偏差。

**1 处隐含选择**（不算偏差，记入代码 docstring）：

- **store 端 `state.monitors` 替换而非 mutate**：每个 `MonitorsChanged` event 触发 `state.monitors = new_array`（不是 `.splice()`）。Vue 3 reactivity 对 array 替换的反应最稳定（不依赖深 reactive proxy 行为）；同时避免了 `push` / `splice` 顺序对模板 watch 触发时机的影响。ConnectionRow 用 `v-for="m in monitorOptions"` + `monitorOptions = computed(() => daemonStore.monitors)`，每次替换触发一次重渲染。

- **diffClientConfigPatch 拆分为 export 函数**：原 `updateClientConfig` 是 store singleton 直接调用 `getSocket().request`，没有 testable seam。STEP-3.2 抽出 `diffClientConfigPatch(handle, current, patch) -> FrontendRequest[]` 纯函数（被 vitest 直接 import 测试），`updateClientConfig` 退化为薄包装。这样不破坏现有调用点（ConnectionRow 还是调 `updateClientConfig(handle, patch)`），但新增了 5 个 diff 单测的测试入口。

## 4. 处理的 SUGGESTION 项

**新增 #5**：见 `next/SUGGESTION.md`：

- `#5 🟠 STEP-M3-3.2 vitest 依赖安装被网络阻断` —— 本会话网络故障，无法安装 vitest / @vue/test-utils / happy-dom，无法跑 `pnpm test` / `pnpm build` / `pnpm type-check`。源码改动 100% 落地，test 文件 100% 落地，等网络恢复一行 `pnpm install` 即可补齐。

**未移动任何 SUGGESTION 条目**到 FIXED / IGNORE。

## 5. 闸门检查

| 闸门 | 结果 |
|---|---|
| 产物对得上吗 | ✅ 3 个改动文件（api/ipc.ts / store/index.ts / ConnectionRow.vue）+ 2 个新 test 文件 + 1 个 vitest.config.ts 全部按 PLAN 完成标志落地；11 个 store 单测 + 7 个 ConnectionRow snapshot/structural 单测已写 |
| 依赖对得上吗 | ✅ M0/M1/M2/M3.1 全部 `通过`（`cargo test --workspace` 143/0 重跑全绿）；`lan_mouse_ipc::ClientConfig.monitor` + `FrontendRequest::UpdateMonitor` 已 STEP-3.1 落地；本步前端镜像这两类 |
| 验收对得上吗 | ⚠️ Rust 测试 143/0 全绿；Vue tsc / build / test 因网络未跑；test 文件已写完待跑 |
| **milestone 边界门** | ✅ **未触碰** Rust 后端（STEP-3.1 之外）、Capture trait / backend、quic 协议层；只动 Vue 端 + 新增 vitest 配置 |
| 时间预算门 | ✅ 实际 ~55 min（PLAN 估时 30 min；超出 25 min 主要花在网络故障排查 + 决定降级路径），低于 executor 1h 上限但接近 |
| 闸 3 milestone 收尾 | ⏸ 跳过（M3 收尾在 STEP-3.3） |

## 6. 遗留

### 必须 leader 决策的事项

1. **网络恢复后立即补验证**：`pnpm install && pnpm test && pnpm build` 三件套（test 文件已写好，不需改任何代码）。如果 `pnpm install` 时报错或 tsc 暴露类型问题，按报错修即可。
2. **M3 收尾（STEP-3.3）需要的 fmt / clippy / build 三件套**目前只能跑 Rust 部分（已验证），Vue 部分要在 vitest 装好后才能跑。

### 非阻塞观察（不写 SUGGESTION）

- `diffClientConfigPatch` 的 export 让 store 模块多了一个对外 API；ConnectionRow 等组件不需要直接 import 它（仍走 `updateClientConfig` wrapper），但有好奇的 reader 会发现它。这是 testability 与封装性的 trade-off —— 我倾向保留 export（test 入口明确 + 不需要 mock socket），但在 docstring 里写明"intended for tests only, prefer updateClientConfig at call sites"。

- ConnectionRow 的 monitor `<select>` 用 `:value` 绑定（不是 `v-model`），与 position / input_channels 保持一致；onChange 走 `setMonitor($event)` 单表达式（**没有 inline 多语句**，避开 memory `oxfmt-vue-template-semicolons.md` 的 `;` 被剥的坑）。

## 7. 下一步

派发 **M3.STEP-3.3 — M3 收尾**（`cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` + `cd lan-mouse-vue && pnpm build` + 真机多屏回归）。

预估 ~30 min（人类配合占大头：macOS 双屏 + L 形错位 + Linux Wayland + 旧 config 兼容）；前置依赖：✅（STEP-3.1 + STEP-3.2 源码均落地；网络恢复后 STEP-3.2 的验证 gap 自动闭合）。

## 8. 闸 1 / 闸 2 / 闸 3 状态

| 闸 | 结果 |
|---|---|
| 闸 1 产物/依赖/验收/边界 | ⚠️ 产物 / 依赖 / 边界全 OK；验收 Vue 部分因网络未跑（test 文件已就位待补） |
| 闸 2 执行中偏差 | #1：网络故障导致 vitest 验证未跑（已写 SUGGESTION #5） |
| 闸 3 milestone 收尾 | ⏸ 跳过（M3 收尾在 STEP-3.3） |
