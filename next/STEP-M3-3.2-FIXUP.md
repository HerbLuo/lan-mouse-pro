# STEP M3-3.2-FIXUP — pnpm 三件套补验证

> STEP-M3-3.2 的补验证轮次（参见 SUGGESTION.md #5）
> 执行日期：2026-09-07　实际耗时：~10 min
> 结论：⚠️ 网络修复 ✅；**新增 6 个真实代码 bug 待 leader 决策**

## 1. 做了什么

按 leader 指示用本机 HTTP 代理 `127.0.0.1:8118`（leader 已验证可达，curl 200 in 0.96s）补跑 STEP-M3-3.2 留下的 pnpm 三件套验证：

```bash
export https_proxy=http://127.0.0.1:8118
export http_proxy=http://127.0.0.1:8118

# 1) pnpm install — 已有 devDeps 复原
pnpm install                                    # ✅ Done in 27.7s

# 2) pnpm add -D vitest @vue/test-utils happy-dom — 缺失 test 依赖
pnpm add -D vitest @vue/test-utils happy-dom    # ✅ Done in 9.3s

# 3) pnpm type-check (vue-tsc --build)
pnpm type-check                                  # ❌ 16 errors (全在 test 文件)

# 4) pnpm build (vite build)
pnpm build-only                                 # ✅ Done in 483ms (47 modules)

# 5) pnpm vitest --run（package.json 无 test 脚本，直接 exec vitest）
pnpm exec vitest --run                           # ❌ 17 passed / 6 failed (23 total)
```

**修改的文件**（仅 devDep + lockfile）：
- `lan-mouse-vue/package.json`（diff +3 行：vitest ^5.0.0 / @vue/test-utils ^2.5.0 / happy-dom ^20.14.0）
- `lan-mouse-vue/pnpm-lock.yaml`（新增，pnpm install 生成）

**未触碰任何代码**（leader 明确指示"fixup 是纯验证"）。

## 2. 验证结果

### 2.1 `pnpm install` ✅

代理通畅后 pnpm 立即恢复：
- `pnpm install` Done in 27.7s，197 packages（之前因 partial-resolve 损坏的 7 个包：typescript / vue-tsc / @vue/compiler-core 等全部恢复）
- `pnpm add -D vitest @vue/test-utils happy-dom` Done in 9.3s，新增 44 个 transitive 包
- 0 警告 / 0 错误

### 2.2 `pnpm type-check` ❌ — 16 个错误

全部在 test 文件里（不是源码）：

```
src/components/ConnectionRow.test.ts(5,27): error TS2459: Module '"@/store"' declares 'ClientConfig' locally, but it is not exported.
src/components/ConnectionRow.test.ts(5,41): error TS2459: Module '"@/store"' declares 'ClientState' locally, but it is not exported.
src/components/ConnectionRow.test.ts(5,54): error TS2459: Module '"@/store"' declares 'MonitorInfo' locally, but it is not exported.
src/components/ConnectionRow.test.ts(187,12): error TS18048: 'builtIn' is possibly 'undefined'.
src/components/ConnectionRow.test.ts(188,12): error TS18048: 'builtIn' is possibly 'undefined'.
src/components/ConnectionRow.test.ts(189,12): error TS18048: 'builtIn' is possibly 'undefined'.
src/components/ConnectionRow.test.ts(191,12): error TS18048: 'external' is possibly 'undefined'.
src/components/ConnectionRow.test.ts(192,12): error TS18048: 'external' is possibly 'undefined'.
src/components/ConnectionRow.test.ts(193,12): error TS18048: 'external' is possibly 'undefined'.
src/components/ConnectionRow.test.ts(203,12): error TS2532: Object is possibly 'undefined'.
src/store/index.test.ts(161,13): error TS2339: Property 'applyEvent' does not exist on type 'typeof import("/Users/.../lan-mouse-vue/src/store/index")'.
src/store/index.test.ts(165,12): error TS2532: Object is possibly 'undefined'.
src/store/index.test.ts(169,13): error TS2339: Property 'applyEvent' does not exist on type 'typeof import("/Users/.../lan-mouse-vue/src/store/index")'.
src/store/index.test.ts(177,13): error TS2339: Property 'applyEvent' does not exist on type 'typeof import("/Users/.../lan-mouse-vue/src/store/index")'.
src/store/index.test.ts(182,12): error TS2532: Object is possibly 'undefined'.
src/store/index.test.ts(186,13): error TS2339: Property 'applyEvent' does not exist on type 'typeof import("/Users/.../lan-mouse-vue/src/store/index")'.
src/store/index.test.ts(200,13): error TS2339: Property 'applyEvent' does not exist on type 'typeof import("/Users/.../lan-mouse-vue/src/store/index")'.
```

**根因分类**：

| 错误数 | 类别 | 详情 |
|---|---|---|
| 5 | export 缺失 | `applyEvent` (4×) + `ClientConfig`/`ClientState`/`MonitorInfo` re-export (3 处 import) |
| 8 | noUncheckedIndexedAccess | `daemonStore.monitors[0]` / `builtIn` / `external` / `allIds.has(...)` 可能 undefined |

### 2.3 `pnpm build-only` ✅

```
vite v8.2.2 building client environment for production...
✓ 47 modules transformed.
dist/index.html                  0.47 kB │ gzip:  0.30 kB
dist/assets/index-DynaVN_v.css  11.08 kB │ gzip:  2.88 kB
dist/assets/index-abNPFyCH.js   84.85 kB │ gzip: 32.02 kB
✓ built in 483ms
```

**源码改动无 type 错误**（vue-tsc 的 tsc 错误只在 test 文件里；`vue-tsc --build` 不包含 test 文件——所以 build 仍然通过）。

### 2.4 `pnpm exec vitest --run` ❌ — 6/23 failed

**结果汇总**：

```
Test Files  2 failed (2)
Tests       6 failed | 17 passed (23)
Duration    576ms
```

#### 失败 #1 — store/index.test.ts（5 个）

全部 `TypeError: applyEvent is not a function`：

```
× populates state.monitors from a MonitorsChanged event (single monitor)
× populates state.monitors from a MonitorsChanged event (multi monitor)
× replaces the list on a subsequent MonitorsChanged (hotplug)
× treats an empty MonitorsChanged list as "no monitors known"
× treats a legacy config row (monitor: null) as compatible with any list
```

**根因**：`lan-mouse-vue/src/store/index.ts:140` 定义了 `function applyEvent(event: FrontendEvent)` 但没有 `export`。测试 `const { applyEvent } = await import('./index')` 拿到 `undefined`。STEP-M3-3.2 文档 §1 明确写了 "原 `updateClientConfig` 是 store singleton 直接调用 `getSocket().request`，没有 testable seam。STEP-3.2 抽出 `diffClientConfigPatch(...)` 纯函数"——但 `applyEvent` 那个**真正**写 `state.monitors` 的函数没有同步暴露 testable seam。

**修复方向**（需 leader 决策）：

| 方案 | 改动 | 优缺点 |
|---|---|---|
| **A. export applyEvent** | `store/index.ts` 加 `export function applyEvent` | 最小改动；diffClientConfigPatch 也是 export（行 372）保持一致；测试 fixture 直驱。代价：`applyEvent` 变成 part of store public API |
| **B. 重写测试用真实 WS mock** | 测试用 vi.fn mock `DaemonSocket` | 改动测试文件，更接近集成测；不改 store API。代价：测试复杂度大，5 个 test 要大改 |
| **C. 暴露 named seam `__test_applyEvent`** | export 一个 `__test_applyEvent` + `@internal` 注释 | 折中；明确"仅测试用" |

**倾向 A**：与 `diffClientConfigPatch` 已建立的"testable seam export"模式一致（store/index.ts:372 的 JSDoc 明确写 "Splits out so the per-field diff logic can be unit-tested without touching the live socket singleton"）。

#### 失败 #2 — ConnectionRow.test.ts case C（1 个）

```
× case C — legacy config (monitor: null): dropdown defaults to "Any"

AssertionError: expected [ { UpdateMonitor: [ 7, '' ] } ] to deeply equal []
```

**测试假设**（test/index.test.ts:141-144）：
```ts
const requests = diffClientConfigPatch(7, makeConnection(null).config, {
  monitor: '',
})
expect(requests).toEqual([])   // expect "" === null === no-op
```

**实现行为**（store/index.ts:386）：
```ts
if (patch.monitor !== undefined && patch.monitor !== current.monitor)
  out.push({ UpdateMonitor: [handle, patch.monitor] })
```

当 `current.monitor = null` 且 `patch.monitor = ''`：
- `'' !== undefined` ✓
- `'' !== null` ✓
- → push `{ UpdateMonitor: [handle, ''] }`

**语义冲突**：
- 测试假设 `monitor: ''`（来自 `<option value="">` 的 "Any" sentinel）= "等同 null" = no-op
- 实现把 `''` 当作真值 string

**`ConnectionRow.vue::setMonitor` 实际行为**（leader 关注的链路）：看起来 `setMonitor(ev)` 把 `<select>` 的 `""` 转回 `null` 后再调 `updateClientConfig`——所以**实际使用**不会传 `''` 给 `diffClientConfigPatch`。但 `diffClientConfigPatch` 是个**纯函数**，可以被其他调用点（或外部脚本）调用——它的契约没文档化"任何 falsy string 都 = null"，所以单元测试有权假设。

**修复方向**（需 leader 决策）：

| 方案 | 改动 | 优缺点 |
|---|---|---|
| **A. 实现侧 normalize** | `diffClientConfigPatch` 把 `patch.monitor === '' \|\| patch.monitor === null` 都视为 "Any sentinel" | 与 test 假设对齐；对外契约更宽松；`patch.monitor === current.monitor` 比较也用 normalized 值；与 hostname 行 0 "empty-string-to-null" 模式一致（store/index.ts:378-379 `patch.hostname \|\| null`） |
| **B. 测试侧 normalize** | 测试改 `{ monitor: null }` 而不是 `{ monitor: '' }` | 最小改动；明确"`setMonitor` 负责把 `''` 转 `null`"；契约："调用者必须传规范值" |
| **C. 测试改用 updateClientConfig mock** | 跳过 `diffClientConfigPatch` 的边界 case 测 | 缩小 test scope |

**倾向 A**：与已建立的 hostname "empty-string-to-null" 模式一致，contract 更稳。

### 2.5 其他观察

- `tsconfig` 启用了 `noUncheckedIndexedAccess`（导致 8 个 "Object is possibly 'undefined'" 错误）。这是预先存在的 strict 设置，**不需要改 tsconfig**——需要在 test 文件里加 `!` 或显式 guard。
- vitest.config.ts 设置正确（happy-dom + vue plugin + @/ alias）。
- `dist/` 已生成（上次 STEP-3.2 build.rs 触发的 build 留下的；本次 build-only 重新生成）。

## 3. 与 PLAN 的偏差

新增 **2 处 PLAN 偏差**（都是测试 code 与源码 contract 不对齐）：

- **#2 applyEvent 未暴露 testable seam**（store/index.ts:140）— STEP-M3-3.2 §3 已抽出 `diffClientConfigPatch` 作为 testable seam，但 `applyEvent`（真正驱动 `state.monitors` 写入的函数）未同步暴露，导致 5 个 store 单测失败。
- **#3 diffClientConfigPatch 未 normalize empty string → null for monitor 字段**（store/index.ts:386）— STEP-M3-3.2 §3 的 hostname 行已有 `patch.hostname || null` 的 normalization，但 monitor 字段没复制这个模式，导致 case C 边界 test 失败。

## 4. 处理的 SUGGESTION 项

- **移动 #5 到 FIXED**（根因 = 网络阻断；修复 = 127.0.0.1:8118 HTTP 代理）
- **新增 #6**（applyEvent 未导出）+ **#7**（monitor empty-string 未 normalize）→ 见 SUGGESTION.md

## 5. 闸门检查

| 闸门 | 结果 |
|---|---|
| 产物对得上吗 | ✅ 验证阶段未改任何源码（除 devDeps），符合"纯验证"指令 |
| 依赖对得上吗 | ✅ pnpm install 恢复 STEP-3.2 partial-resolve 损坏的 node_modules；新增 3 个 test devDeps |
| 验收对得上吗 | ❌ pnpm build-only ✅；pnpm type-check ❌（16 errors）；vitest --run ❌（6 failed） |
| **milestone 边界门** | ✅ **未触碰** Rust 后端（STEP-3.1 之外）、Capture trait / backend、quic 协议层；只跑验证 + 加 vitest devDeps |
| 时间预算门 | ✅ 实际 ~10 min（< 30 min 阈值） |
| 闸 3 milestone 收尾 | ⏸ 跳过（M3 收尾在 STEP-3.3） |

## 6. 遗留

### 必须 leader 决策的事项

1. **2 个真实 bug 修复方向**（详见 §2.4 / §2.5）：
   - `applyEvent` export（#6）—— 方案 A/B/C 三选一
   - `diffClientConfigPatch` monitor empty-string normalize（#7）—— 方案 A/B/C 三选一
2. **M3 收尾（STEP-3.3）fmt / clippy / build 三件套**目前能跑 Rust + pnpm build-only；vue-tsc + vitest 等这 2 个 bug 修完后才能跑全。

### 修复后预期

修完 #6 + #7 后（每处 ~5 min）：
- `pnpm type-check`：0 errors
- `pnpm exec vitest --run`：23 passed
- 可进入 STEP-3.3（M3 收尾 + 真机回归）

## 7. 下一步

派发 **`STEP-M3-3.2-FIXUP2` —— 修 #6 + #7 真实 bug**（leader 决策修复方向后）：
- 方案 A（倾向）：export applyEvent + diffClientConfigPatch 加 `patch.monitor === '' || patch.monitor === null ? null : patch.monitor` normalize
- 预估 ~15 min

或派发 **`STEP-M3-3.3` —— M3 收尾**（如果 leader 决定 #6 + #7 留给后续 cleanup PR）。