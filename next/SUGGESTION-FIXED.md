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

---

## #9 — M0b STEP-0.2 Path 1 (h3 + b"h3" ALPN) rejected; Path 2 (HTTP/3-lite over b"lan-mouse") adopted

- **触发 STEP**：M0b / STEP-0.2 spike（PLAN-2 §3 M0b STEP-0.2 行 + §5 评审 #2 second round）
- **现象**：原 PLAN 二分叉假设 "Path 1 (h3-quinn) 失败 + Path 2 (自实现 HTTP/3-lite) 落地"。Spike 在 `examples/h3_pingpong.rs` 实现 Path 1 的 ALPN-multiplexed endpoint 路径并跑 4 场景，结论：
  - **h3 本身**可工作：h3-quinn 0.0.10 与 quinn 0.11 兼容（`Cargo.toml` 已加 `h3 = "0.0.8"` + `h3-quinn = "0.0.10"` dev-dep；h3-quinn-quinn 三者实际绑定为 quinn 0.11.x），h3 的 `b"h3"` ALPN 在独立 endpoint 上 `GET /healthz` + 200 MiB 流式 + 取消 + 拔网全绿（loopback 71 MiB/s）。结论：**h3 本身不是技术债**。
  - **ALPN 共存路径失败**：当前 `quic_transport/mod.rs:31` 用 `b"lan-mouse"`；h3 标准 ALPN 是 `b"h3"`。quinn 0.11 的 TLS config 接受多个 ALPN 但 ALPN 是 **在 QUIC 握手阶段由 client 选**的——server 端要支持两者共存必须用 SO_REUSEPORT 起一个独立 UDP socket（每个 ALPN 一个），再在 `Connection` 拿到后按 `connection.alpn()` 在应用层 demux 每个 stream。这带来：① 端口分裂（4252 不能再是单端口）；② 每个 endpoint 自己一套 cert + 鉴权；③ `PeerSession::run` 选 stream 派发的 dispatch 路径要多一个 ALPN 分支（协议层结构变化）。PLAN §0 scope discipline + AGENTS.md "Scope discipline. Only implement what was requested" 明确反对引入这一层复杂度。
  - **Path 2 落地**：`src/quic_transport/http3.rs` 自实现 `[u16 method_len][method][u16 path_len][path][u32 body_len][body]` 帧 over 裸 QUIC bidi stream，ALPN 保持 `b"lan-mouse"`——纯协议层增量，零端口 / 零握手变化 / 零鉴权重做。`build_server(conn)` + `build_request_conn(conn)` 接口对齐 PLAN §3 M0b STEP-0.2 产物要求。
- **根因（与 PLAN 假设对比）**：PLAN 假设 Path 1 "失败原因是 ALPN 路由不可行"；实际 spike 证明 ALPN 路由 **可以**做，但带来"协议层分叉 + 端口分裂"的代价，违反 scope discipline。Path 1 不是"不可能"而是"不值得"——这是对 PLAN 假设的细化（无功能偏差，仅决策依据从"技术不可行"修正为"技术可行但不符合 scope discipline"）。
- **解决方案**（已落地在 commit-pending 中，路径 2 接口由 leader commit）：
  - `src/quic_transport/http3.rs`（新，~580 行）：`Request` / `Response` / `Router` / `Handler` / `CHUNK_SIZE` + 编码 / 解码（单缓冲 + 流式）+ `build_server` / `build_request_conn` / `ClientConn::request` / `request_streaming` + quinn 错误类型 → `std::io::Error` 转换 + 11 个单元测试（round-trip + 200 MiB framing + router + chunk 边界）。
  - `examples/h3_pingpong.rs`（新，~280 行）：四场景全绿——`/healthz` 200 OK / 200 MiB 字节级一致 71-72 MiB/s loopback / 取消 ~200ms 停 / 拔网 ~25ms 报 `connection lost`（均远在 1s / 5s 预算内）。
  - `src/quic_transport/mod.rs`：仅加 `pub mod http3;`（无 re-export，符合"STEP-0.3 才 wire 进 PeerSession"的纪律）。
  - `Cargo.toml`：`bytes = "1"`（生产）+ `h3 = "0.0.8"` + `h3-quinn = "0.0.10"` + `rand = "0.8"`（dev-deps for spike）。
- **Plan 偏差**：无功能偏差；Path 1 决策依据细化（见上"根因"段），与 PLAN §3 M0b STEP-0.2 行 + §5 评审 #2 second round 完全一致。
- **解决 STEP**：M0b / STEP-0.2

---

## #10 — M1a STEP-1a.4 leader-continued 隐藏引入 3 个 new clippy error（fmt / 1a.5 清理）

- **触发 STEP**：M1a / STEP-1a.4（leader-continued commit `182a0ea`；executor 1a.5 首次 `cargo clippy -- -D warnings` 才暴露）
- **现象**：commit `182a0ea` 落地后 `cargo clippy --workspace --all-targets -- -D warnings` 报 3 个新 error（之前 M0c-0.7 已确认 12 个 pre-existing 在 init commit `828cc51`，不在 M0a/M0b/M0c scope 内）：
  1. **`src/connect.rs:541-549`** — `///` doc-comment applied to function parameter. Rust 禁止在 function parameter 上写 `///`（仅允许 `//`）。`connect_to_handle` 的 `clipboard_inbound_tx: tokio::sync::mpsc::UnboundedSender<...>` 参数前的 9 行块注释用了 `///`。
  2. **`src/connect.rs:121`** — `too_many_arguments (8/7)`. `LanMouseConnection::new` 在 1a.4 加了第 8 个参数（`clipboard_inbound_tx`），使参数数从 7/7 跳到 8/7 触发 clippy。
  3. **`src/service.rs:201`** — `LruFingerprints::len` 标 `#[cfg(test)]` 但在 test target 内 `dead_code`。1a.4 顺手加了 test-only helper（doc-comment 写 "used by dispatcher unit tests"），但实际没在任何 test 内调用 → 1a.4 单测只覆盖了 dispatcher 行为，未用 `len` 断言。
  4. **附带**：`src/service.rs:28` `use crate::clipboard::{ClipboardBackend, ClipboardError, default_backend}` 里的 `ClipboardError` 实际无人使用（`ServiceError` 自己 `#[derive(Error)]` 而非 `From<ClipboardError>` 桥接），1a.4 引入但未在 `Service::new` / dispatcher 任何路径引用。
- **根因**：leader-continued 模式下 session 重启后 leader 直接接管 commit，未跑 `cargo clippy -- -D warnings`（之前 M0a executor 撞 429 后 leader 接力 commit 时跑过；M0c-0.7 也跑过）。M1a 1a.4 走的是"diff 验证 + 报告"的快速通道，遗漏 `cargo clippy -D warnings` 这一关。
- **解决方案**（M1a / STEP-1a.5）：
  - `src/connect.rs:541-549`：`///` → `//`（行注释；不是公开 API 的 doc-comment，仅内部 rationale）
  - `src/connect.rs:121`：`#[allow(clippy::too_many_arguments)]` 加在 `pub(crate) fn new` 上（与同文件 M0c-0.7 P2.3 spawn 风格分裂同样的"局部小 allow" 策略；M1a 末段不重构 function signature）
  - `src/service.rs:201`：`#[cfg(test)]` 后加 `#[allow(dead_code)]`（保留 helper 给后续 1b.1 / 1b.3 阶段使用，避免 1a.5 删 API 撕扯 1a.4 已建立的契约）
  - `src/service.rs:28`：`use` 行移除 `ClipboardError`
- **预防措施（建议记入 AGENTS.md workflow）**：leader-continued 模式 commit 后**必须** `cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo test --workspace` 三连通过才能写 `done` 报告。这是 PLAN §0 scope discipline 的硬约束，不是软建议。
- **Plan 偏差**：0 处功能性偏差（运行时行为完全正确；3 个 error 全部是 lint-level），但 1a.4 "0 new clippy" 的口径被突破 —— 文档记录为"1a.4 隐藏 3 个 lint error，1a.5 清理"
- **解决 STEP**：M1a / STEP-1a.5

