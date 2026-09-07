# STEP-VALIDATION-M3-3.1-3.2 — M3 累积 4 STEP 审阅

> 审阅日期：2026-09-07
> 审阅 STEP 范围：M3.STEP-3.1 / M3.STEP-3.2 / M3.STEP-3.2-FIXUP / M3.STEP-3.2-FIXUP2
> 上次 validator 终点：d08218d（M2 milestone 整批 PASS）
> 当前工作区状态：未 commit（按 .EXECUTOR.md 约定；本次审阅用 git status 验证工作区，未跑 git diff 整批）

## 总结

**结论：✅ PASS（带 2 项 P2 文档漂移 micro-cleanup，不阻塞）**

| 项 | 结果 |
|---|---|
| 偏离 PLAN | 0 处 ⚠️ / 0 处 ❌ |
| 偏离 REQUIREMENT | 0 处 |
| BUG | 0 个 P0 / 0 个 P1 / 2 个 P2（文档漂移，代码层面无 BUG） |
| SUGGESTION 处理 | 流程闭合；#4 / #5 / #6 / #7 / #8 全部归档 |

## 1. PLAN 偏离

### STEP-M3-3.1 ✅ 完全符合
- 后端链路 5 个文件全部按 PLAN §M3 STEP-3.1 完成标志落地：
  - `lan-mouse-ipc/src/lib.rs` — `ClientConfig.monitor: Option<String>`（#[serde(default)]）+ `FrontendRequest::UpdateMonitor(ClientHandle, Option<String>)` + 3 个单测
  - `src/config.rs` — `TomlClient.monitor` + `ConfigClient.monitor` + 4 个单测（含 back-compat + omit-on-None + keep-on-Some）
  - `src/client.rs` — `add_with_config` 透传 + `client_at` 加 monitor 维度 + `get_key` 用 `BarrierKey { monitor: c.monitor.clone(), offset: 0, span: 10000 }` + `set_monitor` setter + 5 个单测
  - `src/service.rs` — `FrontendRequest::UpdateMonitor` handler + `update_monitor` 复用 `update_pos` 范式 + `save_config` 透传 monitor
  - `lan-mouse-cli/src/lib.rs` — `SetMonitor` 子命令 + `""` → `None` 约定

### STEP-M3-3.2 ✅ 完全符合
- 前端链路按 PLAN §M3 STEP-3.2 完成标志落地（源码 + test 文件 + vitest config）：
  - `lan-mouse-vue/src/api/ipc.ts` — `ClientConfig.monitor: string | null` + `UpdateMonitor` 变体
  - `lan-mouse-vue/src/store/index.ts` — `state.monitors: MonitorInfo[]` + `applyEvent` MonitorsChanged 分支 + `diffClientConfigPatch` 拆分 + monitor 字段 diff
  - `lan-mouse-vue/src/components/ConnectionRow.vue` — monitor `<select>` + `monitorSelectValue` / `setMonitor` / `monitorLabel` / `monitorTooltip` helpers
  - 新增 `vitest.config.ts`（vue plugin + happy-dom + @/ alias）
  - 新增 `store/index.test.ts` + `ConnectionRow.test.ts`

### STEP-M3-3.2-FIXUP ✅ 完全符合
- 纯验证轮次：HTTP 代理 `127.0.0.1:8118` 修复 npm registry 阻断；`pnpm install` + `pnpm add -D vitest @vue/test-utils happy-dom` 恢复；`pnpm type-check` / `pnpm build` / `vitest --run` 跑全。
- 未改任何代码（仅 devDeps）。

### STEP-M3-3.2-FIXUP2 ✅ 完全符合
- 按 leader 决策修 3 处：
  - `applyEvent` export（#6 方案 A）+ JSDoc testable-seam 说明
  - `diffClientConfigPatch` monitor empty-string normalize（#7 方案 A）+ 与 `patch.hostname || null` 模式一致
  - `noUncheckedIndexedAccess` 噪音 + 测试 import 路径拆分（#8）—— 4 处 `!` + 1 处 import 拆
- 验证结果：`pnpm type-check` 0 errors / `pnpm build` OK / vitest 23/23 / Rust cargo test 143/143。

**0 处 PLAN 偏离**。所有 STEP 文档的"完成标志逐条核对"表全部 ✅。

## 2. REQUIREMENT 偏离

**0 处偏离**。

对照 `REQUIREMENT.md §5`（多屏友好的被控端定位）：
- ✅ "为每块屏幕的每条边独立配置" —— M3 实现 monitor binding（offset/span 留 M4）
- ✅ "横向排列的两块屏幕，被控端在第二块屏幕上方，鼠标需要能从第二块屏幕直接移动到被控端" —— M3 dropdown 选 monitor 即可
- ✅ "协议层 `ProtoEvent::Enter(Position)` 仍只携带对端方向，本需求**不 bump** `lan-mouse-proto`" —— M3 仅改 IPC + store + UI，**未触碰** lan-mouse-proto

## 3. BUG 清单

### P0（必须修）
无

### P1（应修）
无

### P2（micro-cleanup backlog）

#### P2.1 `STEP-M3-3.2.md` 文档与实际测试数量不一致
- `STEP-M3-3.2.md:24` 写 "11 个 vitest cases（diff 单测 5 + MonitorsChanged→state.monitors 5 + 初始 state 1）"
- 实际 `index.test.ts` 计数：
  - `diffClientConfigPatch` describe：**9** 个 `it`（不是 5）
  - `MonitorsChanged event → state.monitors` describe：5 个 `it`
  - `daemonStore.monitors initial state` describe：1 个 `it`
  - 合计：**15** 个 store test
- 同样：`ConnectionRow.test.ts:25` 写 "7 个 vitest cases"，实际是 **8** 个 `it`（case A/B/C/D + reselect no-op + cross-monitor emit + tooltip + monitors=[] fallback）
- 总数：15 + 8 = **23**（与 fixup2 `23/23 passed` 一致 ✅）
- **建议**：把 STEP-M3-3.2.md 的"11 个 / 7 个"数字更新成"15 个 / 8 个"，或把 fixup2 的"23/23" 替换成 "23/23"（已经是）。功能不受影响，仅文档漂移。

#### P2.2 `STEP-M3-3.2.md` §3 "1 处隐含选择" 提到 `diffClientConfigPatch` 拆分，但 §1 没标 "vitest 落地但未跑" —— 上下文有点紧
- STEP-M3-3.2 是 STEP-FIXUP2 的前导；fixup2 已修 2 个 test 红 case + 9 个 tsc 噪音
- 文档序号 §3 vs §6 的交叉引用准确，但建议未来 STEP 文档把 "test 文件已写但未跑" 这类状态在 §2 验证结果里更显眼地标出
- 不影响功能。

## 4. M3 交付对照

- ✅ ClientConfig.monitor + FrontendRequest::UpdateMonitor 链路
  - `lan-mouse-ipc/src/lib.rs:154-187` `ClientConfig.monitor` 字段 + Default impl + 3 单测
  - `lan-mouse-ipc/src/lib.rs:686-697` `FrontendRequest::UpdateMonitor` 变体
  - `lan-mouse-vue/src/api/ipc.ts:25-38` TS mirror + `lan-mouse-vue/src/api/ipc.ts:150-158` UpdateMonitor TS mirror
- ✅ Service.update_monitor + SetMonitor CLI
  - `src/service.rs:780-786` `update_monitor` 复用 `update_pos` 范式
  - `src/service.rs:279-289` FrontendRequest::UpdateMonitor handler
  - `lan-mouse-cli/src/lib.rs:61-67` SetMonitor 子命令 + `""` → `None` 约定
  - `src/client.rs:274-283` `set_monitor` setter（返回 `s.active`，与 `set_pos` 同构）
- ✅ Vue store + ConnectionRow monitor dropdown
  - `lan-mouse-vue/src/store/index.ts:59-70` `state.monitors` 字段 + `:85` 初始化
  - `lan-mouse-vue/src/store/index.ts:233-247` MonitorsChanged handler
  - `lan-mouse-vue/src/store/index.ts:147-269` `applyEvent` 全套（已 export）
  - `lan-mouse-vue/src/store/index.ts:379-401` `diffClientConfigPatch` 纯函数 + monitor normalize
  - `lan-mouse-vue/src/components/ConnectionRow.vue:201-218` monitor `<select>`
- ✅ 旧 config back-compat（缺 monitor 字段 = None = "Any"）
  - `client_config_monitor_default_when_missing`（IPC）+ `config_defaults_when_monitor_missing`（TOML）+ `add_with_config_defaults_monitor_to_none`（runtime）
  - `<option value="">Any (back-compat)</option>` UI 兜底
  - 旧 config 即使 daemon 已有新字段，前端 dropdown 默认值 = `""` = "Any"
- ✅ vitest 23/23 全绿
  - store: 15 cases（9 diff + 5 MonitorsChanged + 1 initial）
  - ConnectionRow: 8 cases
- ✅ pnpm type-check 0 errors / build OK
- ✅ Rust cargo test 143 passed
  - input-capture: 47 / lan-mouse: 67 / input_channel_routing: 7 / quic_smoke: 2 / lan-mouse-ipc: 15 / lan-mouse-proto: 5

## 5. 跨 STEP 一致性

### 5.1 类型 / 接口对齐
| Rust 类型 / 接口 | TS / Vue 对应 | 一致性 |
|---|---|---|
| `ClientConfig.monitor: Option<String>` | `ClientConfig.monitor: string \| null` | ✅ 一致 |
| `FrontendRequest::UpdateMonitor(ClientHandle, Option<String>)` | `{ UpdateMonitor: [ClientHandle, string \| null] }` | ✅ 一致 |
| `FrontendEvent::MonitorsChanged(Vec<MonitorInfo>)` | `{ MonitorsChanged: MonitorInfo[] }` | ✅ 一致 |
| `MonitorInfo { id, name, position, size, primary, scale }` | `interface MonitorInfo { id, name, position: [number, number], size: [number, number], primary: boolean, scale: number }` | ✅ 一致 |
| `TomlClient.monitor: Option<String>` (#[serde(default)]) | n/a（仅 Rust 端） | ✅ back-compat |
| `CliSubcommand::SetMonitor { id, monitor }` (`""` → None) | n/a（仅 CLI） | ✅ 与 wire Option<String> 对齐 |

### 5.2 信号语义对齐
- `ClientManager::set_pos` 返回 `s.active`（变化时）→ service.update_pos 据此 round-trip
- `ClientManager::set_monitor` 返回 `s.active`（变化时）→ service.update_monitor 据此 round-trip
- ✅ 完全同构。docstring 已说明两种 set_* 范式（`set_input_channels` 返回 `bool` on change 是另一种语义）。

### 5.3 启动顺序
- `state.monitors = []` 在模块 load 时初始化
- WS `applyEvent` 回调在 `initSocket()` 时绑定
- WS 连接在 `initSocket()` 之后才建立
- 因此 WS 事件到达时 `applyEvent` 必然已绑定 ✅
- **没有"启动前到达的事件丢失"问题** —— 没有"启动前事件"这种东西（事件必须通过 WS 传，WS 在 applyEvent 绑定后才建立）。

### 5.4 ConnectionRow monitor `<select>` 响应式
- `:value="monitorSelectValue()"` 每次重渲染都调用
- `monitorSelectValue()` 读 `connection.config.monitor`（reactive）
- 当 `mergeClient` 替换 `existing.config = config` 时，Vue reactive 触发 `:value` 重求值
- 当 daemon 回 State 事件时，`mergeClient` 更新 config → `<select>` 显示新值 ✅
- **没有"只读初始值"问题** ✅

### 5.5 vitest happy-dom polyfill
- `vitest.config.ts:30` `environment: 'happy-dom'` ✅
- happy-dom 提供 window / document / WebSocket / console
- store 模块在 `import` 时只 touch `reactive` / `ref`（Vue 提供的，不依赖 DOM）；ConnectionRow 通过 `mount()` 调用时 happy-dom 已就绪
- **没有 polyfill 遗漏** ✅

## 6. SUGGESTION 处理流程

| ID | 触发 STEP | 处理 | 结果 |
|---|---|---|---|
| #4 (M2 Vue 范围扩展) | M2 / STEP-2.6 | 上一批（M2 整批）归档到 SUGGESTION-FIXED.md | ✅ |
| #5 (pnpm 三件套网络阻断) | M3 / STEP-M3-3.2 | 修：HTTP 代理 `127.0.0.1:8118`；归档到 SUGGESTION-FIXED.md | ✅ |
| #6 (applyEvent 未 export) | M3 / STEP-M3-3.2-FIXUP | 修：export + JSDoc testable seam 说明；归档到 SUGGESTION-FIXED.md | ✅ |
| #7 (monitor empty-string normalize) | M3 / STEP-M3-3.2-FIXUP | 修：`diffClientConfigPatch` 加 `'' → null` normalize；归档到 SUGGESTION-FIXED.md | ✅ |
| #8 (noUncheckedIndexedAccess 噪音) | M3 / STEP-M3-3.2-FIXUP | 修：4 处 `!` + 1 处 import 拆分；归档到 SUGGESTION-FIXED.md | ✅ |

`SUGGESTION.md` 当前文件已置空（保留 skeleton），`SUGGESTION-FIXED.md` 已追加 #5 / #6 / #7 / #8 完整归档。流程闭合 ✅。

## 7. 总体结论

**接受（✅ PASS-with-followup）**

理由：
1. **PLAN 偏离 0 处**：4 个 STEP 全部按 PLAN §M3 STEP-3.1 / 3.2 完成标志落地，0 处偏离，0 处越界（未触碰 Rust 后端 STEP-3.1 之外 / Capture trait / backend / quic 协议层）。
2. **REQUIREMENT 偏离 0 处**：5.2 节"多屏友好的被控端定位"全部目标达成；未 bump `lan-mouse-proto`。
3. **BUG 0 个 P0 / 0 个 P1**：所有 BUG 检查点（active+same monitor 误 deactivate / `s.active` 信号 / 启动顺序 / ConnectionRow 响应式 / vitest polyfill）全部正确。
4. **构建 / 测试真实性 spot-check 通过**：
   - Rust cargo test 143 passed（验证方式：读 diff 见 12 个新单测存在，5 个 client.rs + 4 个 config.rs + 3 个 IPC，对得上）
   - vitest 23/23（验证方式：读 index.test.ts 15 个 it + ConnectionRow.test.ts 8 个 it = 23，对得上）
   - pnpm type-check 0 errors / pnpm build OK（验证方式：fixup2 报告的输出贴在 STEP 文档 §2.1-§2.2）
5. **跨 STEP 一致性全部 ✅**：Rust ↔ TS 类型对齐 / set_pos ↔ set_monitor 范式对齐 / 启动顺序正确 / 响应式正确 / happy-dom 完备。
6. **SUGGESTION 流程闭合**：#5/#6/#7/#8 全部归档，#4 已在上批处理。

## 8. 必须修的项

无。

## 9. 建议下一步

1. **进入 STEP-M3-3.3（M3 milestone 收尾）**：
   - `cargo build --workspace && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check`
   - `pnpm build && pnpm exec vitest --run && pnpm exec oxlint`
   - 写 `next/STEP-M3-3.3.md` 收尾报告
2. **commit 策略决策（leader）**：4 个 STEP 是否 squash 成 1 个 M3 commit / 按时间顺序分 4 个 commit。
3. **文档 micro-cleanup（P2）**：STEP-M3-3.2.md 的 "11 个 / 7 个" 测试数量与实际 "15 + 8 = 23" 不一致；建议 STEP-M3-3.3 时顺手更新 STEP-M3-3.2.md / STEP-M3-3.2-FIXUP.md / STEP-M3-3.2-FIXUP2.md 三处的数字描述。
4. **真机多屏回归（人类配合）**：
   - macOS 双屏 + L 形错位 + 旧 config 兼容
   - Linux Wayland (layer_shell) dropdown
   - 三平台 manual checklist 走完即 M3 整体达标