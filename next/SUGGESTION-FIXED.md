# SUGGESTION — 已解决

> 由 plan-step-executor 维护：触发 STEP / 现象 / 解决方案 / 解决 STEP
> 重新激活的项 → `SUGGESTION.md`

---

## #3 — pre-existing QUIC smoke test flake `connection_survives_ten_seconds_of_silence`

- **触发 STEP**：M1 / STEP-1.3（首次观察到；与本 STEP 无关）
- **现象**：`cargo test --workspace` 时 `tests/quic_smoke.rs::connection_survives_ten_seconds_of_silence` 失败，断言 "server-side connection must remain alive after 10s silence"。`git stash` 后在 commit `828cc51 init`（STEP-1.1 之前）跑同样失败 → 确认 pre-existing。
- **根因**：`src/quic_transport/tls.rs::default_transport_config` 把 `keep_alive_interval` 硬编码 5s；测试 fixture（`server_endpoint` helper / 两个 `dial` 调用点）把 `max_idle_timeout` 也传 5s。QUIC 协议层 idle-timeout 检查与 keep-alive PING 都在 t=5s 触发 → race。t=11s 时 server-side `closed()` 已 ready，断言挂。生产 `config.rs::quic_idle_timeout` 默认同样 5s（2026-09-04 从 10s 调下来），但被 `connect.rs::pong_health_watchdog`（1.5s 阈值）覆盖，主链路不受影响 —— 只是 QUIC 协议层兜底兜不住。
- **解决**（out-of-scope cleanup，方案 A：只动测试）：
  - `tests/quic_smoke.rs:71 / 192 / 314` 三处 `std::time::Duration::from_secs(5)` → `std::time::Duration::from_secs(30)`。30s 与该测试 doc-comment 顶部 `keep_alive_interval = 5s, max_idle_timeout = 30s` 的描述一致（注释说"we use 10 s (well below 30 s) to assert the upper bound"，实际值原本就该 ≥ 30s）。
  - 测试结果：`cargo test --workspace --test quic_smoke` → 2 passed；`connection_survives_ten_seconds_of_silence` 11.01s 完成。
  - 不动生产路径（`tls.rs` / `config.rs` / `connect.rs`）—— 留给后续 cleanup PR（与 SUGGESTION-IGNORE.md #1 同批次）。
- **解决 STEP**：out-of-scope cleanup（不在任何 PLAN STEP 内；M2 STEP-2.1 主线未被打断）

---

## #1 — STEP-1.1 把 backend sig-only 兼容层提前到本步（PLAN 偏差）

- **触发 STEP**：M1 / STEP-1.1
- **现象**：PLAN §M1 STEP-1.1 "涉及文件" 只列 `geometry.rs`、`lib.rs`，backend 改动划到 STEP-1.2。但 `Capture` trait 是 crate 内 trait，sig 改了 backend 不动就连 `input-capture` crate 都编不过——违反 STEP-1.1 完成标志中"公共 API 签名变更 + crate 编过"的组合。
- **解决**：STEP-1.2 按 PLAN 完成 5 backend 内部完整迁移到 `BarrierKey`（dummy / libei / layer_shell / macos / windows；x11 stub 跳过）。每个 backend 的内部 producer-event 通道 / `event_rx` 通道 / thread-local 状态 / `Stream::Item` 现在都直接以 `BarrierKey` 为键，不再做"边界 lift"。`monitor / offset / span` 全程 `None / 0 / 10000` 默认值（与 STEP-1.1 默认兼容层等价）；backend 行为零差异。
- **解决 STEP**：M1 / STEP-1.2

---

## #2 — M1 milestone close：capture.rs pre-existing fmt + clippy 噪音（M1 范围）

- **触发 STEP**：M1 / STEP-1.3（识别，留给 STEP-1.4）
- **现象**：`cargo fmt --check src/capture.rs` 4 处 pre-existing diff（log! 宏多行 / Pending release_capture doc-comment 缩进）+ `cargo clippy` 1 个 pre-existing warning（`capture.rs:797` `if !alive collapsible into outer match`）。
- **解决**：
  - `cargo fmt -p lan-mouse -- --check` → surgical 应用 `rustfmt --edition 2021` 到 `src/capture.rs`（仅 M1 文件）
  - `capture.rs:797` `ProtoEvent::Pong(alive) { if !alive { … } }` → `ProtoEvent::Pong(false) { … }`（移除 `alive` binding，直接 pattern match 字面值）
- **解决 STEP**：M1 / STEP-1.4
- **未解决部分（已转移到 SUGGESTION-IGNORE.md #1）**：workspace 其余 24 处 fmt diff + 7 个 clippy warning 全部在非 M1 文件（QUIC / input-emulation / config），按 PLAN §0 scope discipline 不在 M1 close 范围内。

---

## #5 — STEP-M3-3.2 vitest 依赖安装被网络阻断

- **触发 STEP**：M3 / STEP-M3-3.2
- **现象**：`pnpm add -D vitest @vue/test-utils happy-dom` 在 `lan-mouse-vue/` 内两次尝试均失败：
  - 第一次 `add -D` 在 partial-apply 后报 `ERR_PNPM_META_FETCH_FAIL`，package.json 未被改写，但 node_modules 目录里 7 个包（typescript / vue-tsc / @vue/compiler-core 等）被 pnpm 的 partial-resolve 阶段删除。
  - 第二次 `pnpm install` 同样报 `ERR_PNPM_META_FETCH_FAIL`，无法重建。
  - 直接 `curl https://registry.npmjs.org/vitest` 验证：`Resolving timed out after 5002ms`，npm registry 在本会话内不可达。
- **根因**：用户机器直接出网受限（直连 npm registry 5s 超时）；非 pnpm / 仓库配置问题。
- **解决方案**：
  - leader 验证本机有 HTTP 代理 `127.0.0.1:8118` 可达（curl 200 in 0.96s）。
  - executor 跑：
    ```bash
    export https_proxy=http://127.0.0.1:8118
    export http_proxy=http://127.0.0.1:8118
    pnpm install                                                       # Done in 27.7s
    pnpm add -D vitest @vue/test-utils happy-dom                       # Done in 9.3s
    ```
  - 0 warnings / 0 errors / 3 新增 test devDeps (`vitest ^5.0.0` / `@vue/test-utils ^2.5.0` / `happy-dom ^20.14.0`)
- **副作用（FIXUP 新发现）**：网络问题修了之后，vitest / vue-tsc 才有机会跑 → 暴露出 2 个 STEP-M3-3.2 留下的真 bug（`applyEvent` 未 export + `diffClientConfigPatch` 未 normalize monitor empty-string）。详见 `next/STEP-M3-3.2-FIXUP.md` 与 `next/SUGGESTION.md` #6 / #7。
- **解决 STEP**：M3 / STEP-M3-3.2-FIXUP

---

## #6 — STEP-M3-3.2-FIXUP `applyEvent` 未暴露 testable seam

- **触发 STEP**：M3 / STEP-M3-3.2-FIXUP
- **现象**：`lan-mouse-vue/src/store/index.ts:140` 定义了 `function applyEvent(event: FrontendEvent)` 但没有 `export`。vitest 的 5 个 MonitorsChanged → state.monitors case 用 `const { applyEvent } = await import('./index')` 拿到的全是 `undefined`，全部 `TypeError: applyEvent is not a function`。
- **根因**：STEP-M3-3.2 §3 抽出 `diffClientConfigPatch` 作为 testable seam 暴露了 `export`，但 `applyEvent`（真正驱动 `state.monitors` 写入的 reducer）漏了。STEP-M3-3.2 当时的 `updateClientConfig` wrapper 测试策略只覆盖了 `diffClientConfigPatch` 的纯函数维度，遗漏了 `applyEvent` 的事件维度。
- **解决**（方案 A，leader 指示）：
  - `lan-mouse-vue/src/store/index.ts:140` — 加 `export` 关键字 + JSDoc 标注"testable seam"，与同文件 `diffClientConfigPatch`（line 372）的 seam 模式保持一致。
  - 名称保留 camelCase `applyEvent`（不按 Rust 习惯改 snake_case），与 Vue 端 camelCase 公共 API 约定一致。
  - vitest 5 个 MonitorsChanged case 全绿。
- **解决 STEP**：M3 / STEP-M3-3.2-FIXUP2

## #7 — STEP-M3-3.2-FIXUP `diffClientConfigPatch` 未 normalize `monitor: ''` → `null`

- **触发 STEP**：M3 / STEP-M3-3.2-FIXUP
- **现象**：ConnectionRow.test.ts case C 失败：
  ```
  AssertionError: expected [ { UpdateMonitor: [ 7, '' ] } ] to deeply equal []
  ```
  - 测试假设 `diffClientConfigPatch(7, { monitor: null }, { monitor: '' })` → `[]`（`<option value="">` 的 "Any" sentinel 等同 null = no-op）
  - 实现 `store/index.ts:386` `if (patch.monitor !== undefined && patch.monitor !== current.monitor) out.push(...)` 把 `''` 当成真值 string，输出 `[{ UpdateMonitor: [7, ''] }]`
- **根因**：同函数 `hostname` 行已有 `patch.hostname || null` 的 normalize 模式（line 378-379），但 `monitor` 行没有复制这个约定。`ConnectionRow::setMonitor` 实际在调 `updateClientConfig` 前会把 `''` 转回 `null`，所以 **实际链路** 不会传 `''` —— 但 `diffClientConfigPatch` 是个 public 纯函数（已 export），单元测试有权假设边界规范化是它的契约。
- **解决**（方案 A，leader 指示 — 实现侧 normalize）：
  - `lan-mouse-vue/src/store/index.ts:393-399` — 加 `const monitorValue = patch.monitor === '' ? null : patch.monitor` 然后 diff 比较与 out.push 都用 normalized 值。
  - 与 `patch.hostname || null` 模式一致，对外契约更宽松（调用方传 `''` / `null` 都视为 "Any"）。
- **解决 STEP**：M3 / STEP-M3-3.2-FIXUP2

## #8 — STEP-M3-3.2-FIXUP vitest `noUncheckedIndexedAccess` 噪音

- **触发 STEP**：M3 / STEP-M3-3.2-FIXUP
- **现象**：`pnpm type-check` 报 9 个 `Object is possibly 'undefined'` 错误，集中在 test 文件里（tsconfig 启用了 `noUncheckedIndexedAccess`，strict mode）：
  - `ConnectionRow.test.ts:186, 190` — `opts[1]` / `opts[2]`（tooltip 测试）
  - `ConnectionRow.test.ts:203` — `findAll('option')[0].text()`（legacy 单选项 fallback）
  - `store/index.test.ts:165, 182` — `daemonStore.monitors[0].id`（hotplug / single monitor 测试）
  - 加上 ConnectionRow.test.ts:5 `ClientConfig`/`ClientState`/`MonitorInfo` 3 个 type-re-export 错（也归到这一步处理）
- **根因**：strict mode 下 array index access 返回 `T | undefined`。测试 fixture 已通过 `toHaveLength()` 或前序断言保证了索引存在，但 TS 不做跨语句的 narrowing。
- **解决**（与 #6 / #7 合并到本 fixup，避免 STEP 数量膨胀）：
  - `ConnectionRow.test.ts:186, 190, 203` — `opts[1]` / `opts[2]` / `opts[0]` 加 `!`（非空断言）
  - `ConnectionRow.test.ts:203` — 同时把 `findAll('option')` 提取到 `const opts = sel.findAll('option')` 局部变量，让 `.toHaveLength(1)` 和 `opts[0]` 类型一致
  - `ConnectionRow.test.ts:5` — 拆 type import：`Connection` 仍从 `@/store` import（它在那里 export）；`ClientConfig` / `ClientState` / `MonitorInfo` 改从 `@/api/ipc` import（这才是它们的真正定义地）
  - `store/index.test.ts:165, 182` — `daemonStore.monitors[0]` 加 `!`
  - **没** 关 `noUncheckedIndexedAccess`（它是项目已有 strict 设置，关掉会污染所有源码文件）
- **解决 STEP**：M3 / STEP-M3-3.2-FIXUP2

## #4 — STEP-2.6 Vue store / api/ipc.ts 范围扩展（leader 接受）

- **触发 STEP**：M2 / STEP-2.6
- **现象**：PLAN §M2 STEP-2.6 要求 ConnectionRow.vue 在收到 `BindingInvalid` 时高亮 + tooltip。prompt "不要做的事" 列出"不要触碰 M3 / M4 范围"括号里包含 "Vue store"。但要 surface `BindingInvalid` 到 ConnectionRow，**最少**需要：
  - `lan-mouse-vue/src/api/ipc.ts`：把 `MonitorsChanged` + `BindingInvalid` 加入 `FrontendEvent` union（否则 TypeScript 把这两个变体当 never 类型，`applyEvent` switch 必须 fallback，否则编译报 exhaustive 检查错误）
  - `lan-mouse-vue/src/store/index.ts`：`Connection` 加 `invalidReason: string | null` 字段 + `applyEvent` 2 个新 case 处理（`MonitorsChanged` 当前 no-op、`BindingInvalid` 写入 invalidReason）
- **解决**（leader 决策：接受范围扩展）：
  - 范围扩展是 STEP-2.6 必需的最少 Vue 承载层，不在 M3 dropdown / M4 canvas scope
  - 替代方案（ConnectionRow 直接持有 WS 订阅）会重复 socket 状态机 + 复杂度高 + 不符合现有 store 单例模式，不推荐
  - commit `63706b5` 已落地：9 处新文件 / 47 行 ConnectionRow.vue / 25 行 api/ipc.ts / 57 行 store/index.ts 全部到位
  - 备注：SUGGESTION.md 头部规则要求"已解决 → SUGGESTION-FIXED.md"，本次归档由 leader 显式触发（之前 commit 时口头接受但漏了正式归档，导致 STEP-2.7 executor 看到 SUGGESTION.md #1 仍在没主动动 —— 这是 leader 失误）
- **解决 STEP**：M2 / STEP-2.6 + M2 收尾归档
