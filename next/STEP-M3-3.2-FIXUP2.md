# STEP M3-3.2-FIXUP2 — 修 STEP-3.2 暴露的 2 个 bug + type-check 噪音

> 续 STEP-M3-3.2-FIXUP（已修好网络 + devDeps）；本轮修 vitest 红 6 个 case 暴露的源码 bug
> PLAN §M3 / STEP-3.2 的 fixup 收尾
> 执行日期：2026-09-07　实际耗时：~15 min
> 结论：✅ 通过 — type-check 0 / build 通过 / vitest 23/23 / Rust 143/143 全绿

## 1. 做了什么

按 leader 决策（prompt 顶部已给方向，避免反复问询）修 3 处：

### 1.1 `lan-mouse-vue/src/store/index.ts:140` — `applyEvent` 加 export（#6 方案 A）

源码改动（JSDoc + `export` 关键字）：

```ts
/** Dispatch a `FrontendEvent` into the singleton reactive store.
 *
 *  Exported as a **testable seam** so vitest can drive the
 *  `MonitorsChanged → state.monitors` branch (and any future
 *  state-only events) directly, without spinning up a
 *  `DaemonSocket` mock. Mirrors the same seam pattern used for
 *  `diffClientConfigPatch` at line 372. */
export function applyEvent(event: FrontendEvent) { … }
```

- 命名保留 camelCase（Vue 端约定），不改 snake_case
- 与同文件 line 372 `diffClientConfigPatch` 的 "testable seam" 模式一致
- 5 个 vitest MonitorsChanged case 立刻能从 import 拿到真函数

### 1.2 `lan-mouse-vue/src/store/index.ts:386` — monitor empty-string normalize（#7 方案 A）

源码改动（行 393-399，加 normalize + 用 normalized 值做 diff / push）：

```ts
// Normalize the empty-string sentinel (the `<option value="">`
// "Any (back-compat)" entry in ConnectionRow) to `null` so it's
// diffed against the canonical "unbound" form — mirrors the
// `patch.hostname || null` pattern above.
const monitorValue = patch.monitor === '' ? null : patch.monitor
if (monitorValue !== undefined && monitorValue !== current.monitor)
  out.push({ UpdateMonitor: [handle, monitorValue] })
```

- 与 `patch.hostname || null`（line 378-379）模式一致
- 对外契约更宽松：调用方传 `''` 或 `null` 都视为 "Any"，避免再次踩到边界
- ConnectionRow.test.ts case C `diffClientConfigPatch(7, { monitor: null }, { monitor: '' })` 现在返回 `[]`

### 1.3 `noUncheckedIndexedAccess` 噪音 + type-re-export 错（#8 + 顺带清理）

tsx `noUncheckedIndexedAccess` strict 设置是项目已有的，不需要关。涉及 3 个文件：

| 文件:行 | 改动 |
|---|---|
| `ConnectionRow.test.ts:5` | 拆 type import：`Connection` 仍从 `@/store` import（它在那里 export）；`ClientConfig` / `ClientState` / `MonitorInfo` 改从 `@/api/ipc` import（它们在那定义） |
| `ConnectionRow.test.ts:186, 190` | `opts[1]` → `opts[1]!`，`opts[2]` → `opts[2]!` |
| `ConnectionRow.test.ts:203` | 把 `findAll('option')` 提取到 `const opts`，让 `.toHaveLength(1)` 和 `opts[0]!` 共享同一类型 |
| `store/index.test.ts:165, 182` | `daemonStore.monitors[0]` → `daemonStore.monitors[0]!` |

**`ConnectionRow.test.ts:5` 的 type-re-export 错** 不在 leader 列出的 #6/#7/#8 三项里——它是 STEP-M3-3.2-FIXUP 报告的 16 errors 中 "export 缺失" 类别的剩余 3 个（leader 把这 3 个归入 #6 "export 缺失" 大类，但单独看它们不是 `applyEvent` 也不是 monitor）。修法选了"最小改动"路径（修测试文件 import 路径），而不是从 `@/store` re-export 三个 type（后者会把 store 模块跟 ipc 模块类型绑死，违反现有分层）。

## 2. 验证结果（带代理 `127.0.0.1:8118`）

### 2.1 `pnpm type-check` ✅ 0 errors

```
> vue-tsc --build
(no output → exit 0)
```

之前 STEP-M3-3.2-FIXUP 报的 16 errors 全部归零。

### 2.2 `pnpm build` ✅

```
> run-p type-check "build-only {@}" --

> vue-tsc --build                       # ← 0 errors (see 2.1)
> vite build                            # ← vite v8.2.2

✓ 47 modules transformed.
dist/index.html                  0.47 kB │ gzip:  0.30 kB
dist/assets/index-DynaVN_v.css  11.08 kB │ gzip:  2.88 kB
dist/assets/index-BcehiHfg.js   84.86 kB │ gzip: 32.03 kB
✓ built in 179ms
```

注：与 STEP-M3-3.2-FIXUP 的 dist js hash 不同（`index-BcehiHfg.js` vs `index-abNPFyCH.js`）—— 因为我加了 `MonitorInfo` / `FrontendRequest` 两个 type import 到 store/index.ts（虽然 type-only 会被 tsc 擦除，但 V8 仍可能调整 bundling order；hash 变化合理）。

### 2.3 `pnpm exec vitest --run` ✅ 23/23 passed

```
 RUN  v5.0.0 /Users/hb/Projects/@cloudself/lan-mouse-pro/lan-mouse-vue

 Test Files  2 passed (2)
      Tests  23 passed (23)
   Start at  21:00:26
   Duration  564ms
```

对照之前 STEP-M3-3.2-FIXUP 的 `6 failed | 17 passed (23 total)` —— 6 个红 case 全部转绿：
- 5 个 store MonitorsChanged case（#6 修 applyEvent export）
- 1 个 ConnectionRow case C（#7 修 monitor empty-string normalize）

### 2.4 Rust 回归 ✅ `cargo test --workspace`

```
running 47 tests → test result: ok. 47 passed; 0 failed
running 67 tests → test result: ok. 67 passed; 0 failed
running 7  tests → test result: ok. 7 passed; 0 failed
running 2  tests → test result: ok. 2 passed; 0 failed  (← quic_smoke，11.01s)
running 15 tests → test result: ok. 15 passed; 0 failed
running 5  tests → test result: ok. 5 passed; 0 failed
```

合计 **143 passed / 0 failed**（与 STEP-M3-3.2-FIXUP 的 143 一致；本 fixup 仅 Vue 端，Rust 不变）。

## 3. 与 PLAN 的偏差

无新增偏差。`applyEvent` export 与 PLAN §M3 STEP-3.2 §3 已建立的 "testable seam export" 模式同构（`diffClientConfigPatch` 也是 export）；monitor normalize 是实现侧的契约澄清，与 PLAN §M3 STEP-3.1 描述的 "empty string = null sentinel" 语义对齐。

## 4. 处理的 SUGGESTION 项

| ID | 标题 | 处理 |
|---|---|---|
| #6 | applyEvent 未暴露 testable seam | 移到 `SUGGESTION-FIXED.md`（export + JSDoc） |
| #7 | monitor empty-string 未 normalize | 移到 `SUGGESTION-FIXED.md`（normalize 一行） |
| #8 | noUncheckedIndexedAccess 噪音 + 顺带的 type-re-export | 移到 `SUGGESTION-FIXED.md`（4 处 `!` + 1 处 import 拆分） |

`SUGGESTION.md` 当前文件已置空（保留 skeleton）；待 M3 收尾（STEP-3.3）跑全 `cargo test --workspace` + `pnpm build` 后若再发现新项再 append。

## 5. 闸门检查

| 闸门 | 结果 |
|---|---|
| 产物对得上吗 | ✅ 3 处改动（store/index.ts:140, store/index.ts:386, 2 个 test 文件的 strict-noise）；与 leader 决策一致 |
| 依赖对得上吗 | ✅ 没动 package.json / pnpm-lock.yaml |
| 验收对得上吗 | ✅ pnpm type-check 0 errors；pnpm build 通过；vitest 23/23；cargo test --workspace 143/143 |
| **milestone 边界门** | ✅ **未触碰** Rust 后端 / Capture trait / backend / quic 协议层；只动 Vue 端源码 + 测试 |
| 时间预算门 | ✅ 实际 ~15 min（< 30 min 阈值） |
| 闸 3 milestone 收尾 | ⏸ 仍跳过（M3 收尾在 STEP-3.3） |

## 6. 遗留

无新增遗留。M3 收尾（STEP-3.3）需要做的事（`cargo fmt --check` / `cargo clippy -D warnings` / `pnpm oxlint` / `pnpm build`）现在可以跑全了。

## 7. 下一步

派发 **STEP-M3-3.3** — M3 milestone 收尾：
- 跑 `cargo build --workspace && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check`
- 跑 `pnpm build && pnpm exec vitest --run && pnpm exec oxlint`
- 写 `next/STEP-M3-3.3.md` 收尾报告 + 把本 STEP-M3-3.1 / STEP-M3-3.2 / STEP-M3-3.2-FIXUP / STEP-M3-3.2-FIXUP2 的累积改动一并归档
- leader 决策是否合并 commit（一次性 squash 4 个 STEP 还是按时间顺序分 4 个 commit）
