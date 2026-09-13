# SUGGESTION — 已解决

> 由 plan-step-executor 维护：触发 STEP / 现象 / 解决方案 / 解决 STEP
> 重新激活的项 → `SUGGESTION.md`

---

## #12 — M1b validator P2.1 dead code 清理（`pending_clipboard_requests` + `register_pending_clipboard_request`）

- **触发 STEP**：M1b validator `next/STEP-VALIDATION-P2-M1b.md` §3 P2.1 / §6.1 第 1 项
- **现象**：1b.1 (`8daaa1d`) 引入 `Service::pending_clipboard_requests: HashMap<[u8; 32], ()>` 字段 + `#[cfg(test)] fn register_pending_clipboard_request(...)` helper 作为 "metadata-only registered but not yet pulled" 的 stop-gap；1b.2 (`b65236a`) 取代语义为源端 `cache.remove(prev_sha)` + 接收端 HTTP/3 GET（`Http3Client::get_text`）后，4 处 dead code 加上 `#[allow(dead_code)]` 注解保留到本步。
- **解决**：
  - `src/service.rs:149-159` 字段 + doc-comment + `#[allow(dead_code)]` → 删
  - `src/service.rs:744` `pending_clipboard_requests: Default::default(),` → 删
  - `src/service.rs:2386-2399` `#[cfg(test)] fn register_pending_clipboard_request(...)` 函数 + doc-comment → 删
  - `src/service.rs:2664-2685` `mod clipboard_tests` + `fn metadata_only_text_registers_latest_pending_request_per_hash`（含 3 次 helper 调用 + `use super::register_pending_clipboard_request;`） → 删
  - `cargo test --workspace`：**339 passed / 0 failed / 3 ignored**（baseline 340 - 1 = 339，净删 1 个测试）；`cargo build --workspace`：0 error 0 warning；fmt / clippy baseline（4 + 14 pre-existing）维持不变。
- **解决 STEP**：`next/STEP-P2-M1b-CLEANUP.md`

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

---

## #11 — Windows clipboard backend (`src/clipboard/windows.rs`) 在 Windows build 上无法编译（zig-cross 验证暴露 + 4 处修复）

- **触发 STEP**：SUGGESTION #S-2（STEP-P2-M1a-1a.3 衍生）；2026-09-09 macOS 本地 `cargo-zigbuild --target x86_64-pc-windows-gnu` 验证暴露
- **现象**：`cargo zigbuild --target x86_64-pc-windows-gnu -p lan-mouse --lib --all-targets --no-default-features` 报 4 处编译错误 + 3 处 warning：
  1. **error[E0432] `windows_sys::Win32::Foundation::CloseClipboard` unresolved** — `CloseClipboard` 在 windows-sys 0.61 中位于 `Win32::System::DataExchange`，不是 `Win32::Foundation`。1a.3 leader 落地时按旧版 windows-sys 路径写错。
  2. **error[E0308] mismatched types @ `src/clipboard/windows.rs:149`** — `let text = match result { Some(s) => s, None => return None };` 期望 `result: Option<String>`，但 `let result = unsafe { ... text }` 实际为 `String`（unsafe 块的尾表达式是 `text: String`，而非 `Option<String>`；块内的 `return Some(...)` 是从外层函数 return，不贡献块的值）。这是 M1a STEP-1a.3 leader 报告 §1.2 "Some(Ok) / Some(Err) shape" 注释与代码脱节的产物 —— 注释声称有 `Option<Result<String, String>>`，但实际重构时已展平为 `Option<String>`，match 没跟着改。
  3. **error[E0308] mismatched types @ `src/clipboard/windows.rs:150`** — 同根因（`None` 模式在 `String` 上不合法）。
  4. **warning: unused import `OsStringExt`** — `OsStringExt::encode_wide` 实际是 `OsStrExt` 上的方法；OsStrExt 已 import，OsStringExt 是冗余。
  5. **warning: unused import `HWND`** — windows.rs 全文件未使用 `HWND`（仅 `HGLOBAL` 用于 handle）。
  6. **warning: unused import `EmptyClipboard`** — set_text / current_text 均未调用 `EmptyClipboard`（"clear clipboard" 走 `SetClipboardData("")` 路径，未用 `EmptyClipboard` 单独 API）。
  7. **warning: `Err_to_string` non_snake_case** — helper 函数命名违反 Rust 命名约定。
- **根因**：1a.3 leader 落地 windows.rs 时**仅在 macOS 本机静态 review + 静态 cfg-gate 检查**，未跑 Windows 真机编译也未通过 CI 验证。windows.rs 是 `#[cfg(target_os = "windows")]` 守门，macOS 完全不编，所以 4 个 error + 3 个 warning 全部躲过本地 + M1a 1a.5 leader 复核（1a.5 clippy 检查 macOS host 也跳过 windows.rs）。GitHub Actions CI matrix 含 windows-latest job，但当前 main 分支无 recent push 触发；并且即使触发，`cargo build` 在 windows-latest 上也会立刻爆这 4 个 error —— 即 1a.3 落地后任何 CI run 都应已红。推测：M1a 1a.3 完成后 windows-latest job 没真正跑过 / 或 main 后续 commit 修了但未删 windows.rs 旧路径（看了 git log，1a.3 之后到 b92c750 之间 8 个 commit 全是 macOS-side 工作，windows-latest job 实际未触发）。
- **解决方案**（本轮 leader 验证 + 修复）：
  - `src/clipboard/windows.rs:51-55` — 修正 `CloseClipboard` import 位置：`Win32::Foundation::{CloseClipboard, ...}` → 拆分 `Win32::Foundation::{GetLastError, HGLOBAL}` + `Win32::System::DataExchange::{CloseClipboard, ...}`。同时移除 `HWND` / `EmptyClipboard` / `OsStringExt` 3 个 unused import。
  - `src/clipboard/windows.rs:113-152` — 删掉 buggy 的 `match result { Some(s) => s, None => return None };`，直接把 unsafe 块的尾表达式 `text: String` 作为 `let text = unsafe { ... };` 的值。同时把 GlobalLock / UTF-16 decode 两条错误路径的 `return Some(Err_to_string(...))` 改为 `return None` —— 与 trait 契约 "platform read failed silently → None" 一致（之前 `Some(err_msg)` 会把 Win32 错误信息泄漏到 dispatcher 的 clipboard-text 通道作为"剪贴板内容"，是双 bug：编译错 + 语义错）。
  - `src/clipboard/windows.rs:229-313` — `Err_to_string` 从 module-level 移到 `#[cfg(test)]`，重命名为 `err_to_string`（snake_case）。删除 `_force_keep_Err_to_string` workaround（不再需要 —— production code 已不用 `err_to_string`）。测试 `err_to_string_format_is_stable` 仍 pin 格式给未来 debug-log 用。
- **预防措施**（AGENTS.md 待补 hard rule）：**任何 cfg-gated 平台模块落地后必须跑三平台编译验证**：
  1. macOS host：`cargo build -p lan-mouse` + `cargo test -p lan-mouse --lib`（host 平台全编）
  2. Linux cross：`cargo zigbuild --target x86_64-unknown-linux-gnu -p lan-mouse --lib --all-targets --no-default-features`（依赖 zig 0.14+，zig cross 替 cc-rs 编 ring/rcgen/zlib 的 C 链路；macOS 不需要 gcc cross-toolchain）
  3. Windows cross：同上 + `--target x86_64-pc-windows-gnu`（zig 自带 lld 不支持 MSVC ABI，--gnu target 是 macOS 上唯一可验路径；MSVC target 留给 CI windows-latest job）
  三平台全绿后才能写 `done` 报告。这与 FIXED #10 "leader-continued 后必须 fmt + clippy + test 三连" 同级硬约束 —— 但 #10 是 lint-level（错过只是 -D warnings 报错），#11 是 type-level（错过直接 E0308 / E0432 编不过，CI 红）。
- **CI matrix 现状**：`.github/workflows/rust.yml` 已含 `ubuntu-latest` + `windows-latest` + `macos-latest` + `macos-15-intel` × `build` / `check` / `clippy` / `test` 4 job = 16 job。本机 zig-cross 验证后，windows.rs 在 windows-latest CI job 上也应绿（前提是 CI 真的跑了；如果 #11 修复 push 后 windows-latest job 还红，根因大概率是 input-capture/input-emulation 的 libei/layer-shell 在 Windows 上有 cfg-gated stub —— 那是 M1a scope 之外，留给 M1b+）。
- **Plan 偏差**：1 处隐性偏差（windows.rs 未本地验证落地，是 1a.3 的隐性 scope gap；本轮就地把 gap 补上）。0 处功能性偏差（修复后 windows.rs 在 Windows + Linux 双 cross-compile + macOS native 均绿）。
- **解决 STEP**：out-of-scope cleanup（M1a / STEP-P2-M1a-1a.3 follow-up；不在任何 PLAN STEP 内，仅响应 SUGGESTION #S-2 的本地验证诉求）

---

## #S-5 — `dispatch_files` 用常量 `DEFAULT_MAX_FILE_SIZE = 50 MiB`（IPC schema 扩展后切到 `Config::max_file_size()` getter）

- **触发 STEP**：STEP-P2-M3a-3a.2
- **现象**：`src/service.rs::DEFAULT_MAX_FILE_SIZE = 50 * 1024 * 1024`（PLAN §3 STEP-3a.2 评审 #25 默认值）写死在 `Service::new` 字段 `max_file_size` 中。`lan_mouse_ipc::ClipboardConfig.max_file_size` 字段 M3b STEP-3b.1 才落地，`Config::max_file_size()` getter 同步落地。
- **解决方案**（M4 STEP-4.1 落地）：
  - `src/service.rs:391-394` — 移除 `Service::max_file_size: u64` 字段；doc-comment 改为指向新的 [`Service::max_file_size`] getter
  - `src/service.rs:1141-1147` — `Service::new` 不再初始化 `max_file_size` 字段（已删除）
  - `src/service.rs:2197-2204` — 新增 `Service::max_file_size(&self) -> u64` getter，live-read `self.config.clipboard_config().max_file_size`（`Config` 内部 fallback 到 `DEFAULT_MAX_FILE_SIZE` 当 TOML 缺字段）
  - `src/service.rs:2838` — `dispatch_files` 调用 `dispatch_files_decide(paths, last_fingerprint, self.max_file_size())`（之前是 `self.max_file_size` 字段访问）
  - `src/config.rs:38` — `pub(crate) use crate::service::DEFAULT_MAX_FILE_SIZE`（re-export）
  - `src/config.rs:896` — `Config::clipboard_config()` `max_file_size: cb.max_file_size.unwrap_or(DEFAULT_MAX_FILE_SIZE)` fallback
  - `src/config.rs:909-917` — 新增 `Config::max_file_size()` getter（live read）
  - `src/config.rs:951-955` — `Config::set_clipboard_config()` omit-on-default pattern（用户用 50 MiB 时不写入 TOML）
  - `src/config.rs:1327-1365` — 新增 `config_max_file_size_getter_tracks_toml_changes` 单测覆盖 getter 在 IPC 改动下立即生效
  - `lan-mouse-ipc/src/lib.rs` — `ClipboardConfig.max_file_size: u64` 加 `#[serde(default = "default_max_file_size")]`（50 MiB）
- **结果**：`cargo test --workspace --lib` 全绿；`set_clipboard_config` 写入新 `max_file_size` 后下次 inbound / 下一个 dispatch tick 立刻生效（live read，无需 daemon restart）
- **解决 STEP**：M4 / STEP-P2-M4-4.1

---

## #S-8 — 默认 `accept_dir` 硬编码 `<home>/lan-mouse/`（IPC schema 收紧后由 `Config::clipboard_config()` 兜底）

- **触发 STEP**：STEP-P2-M3a-3a.3
- **现象**：`src/service.rs::default_accept_dir()` 在用户没配置 `ClipboardConfig::accept_dir` 时 fallback 到 `<$HOME 或 $USERPROFILE>/lan-mouse/`。`lan-mouse-ipc::ClipboardConfig.accept_dir: Option<PathBuf>` 字段已存在，但 `set_clipboard_config` IPC handler 不接 Service 字段（参考 #S-7）。
- **解决方案**（M4 STEP-4.1 落地）：
  - `lan-mouse-ipc/src/lib.rs` — `ClipboardConfig::accept_dir` 从 `Option<PathBuf>` 改为 **required `PathBuf`**（auto-accept 是唯一模式，必须有 target）；IPC `Default` impl 用本地 `default_accept_dir()` helper（`$HOME` → `$USERPROFILE` → `/tmp/lan-mouse` 链）
  - `lan-mouse-ipc/src/lib.rs` — 新增单测 `clipboard_config_accept_dir_required`（缺 `accept_dir` 字段 = deserialize error）+ `clipboard_config_partial_missing_default_helpers`（`accept_dir` 必须显式提供）
  - `src/service.rs:4540` — `default_accept_dir()` 升级为 `pub(crate)`（之前 `fn` private）
  - `src/config.rs:24` — `pub(crate) use crate::service::default_accept_dir`（re-export）
  - `src/config.rs:887-889` — `Config::clipboard_config()` 在 TOML 缺 `accept_dir` 时 fallback 到 `default_accept_dir()`（保持 M3a 的 env-based fallback 语义）
  - `src/config.rs:947-949` — `Config::set_clipboard_config()` 把 `accept_dir` 写入 TOML（`Some(cfg.accept_dir)` — 不走 omit-on-default，因为 required 字段必须持久化）
- **结果**：行为兼容 pre-M4（缺 TOML 字段 → fallback 到 `<home>/lan-mouse/`），IPC schema 收紧（`accept_dir` 必填 wire），M5 STEP-5.4 加 GUI textbox + dir-picker 时用户可显式覆盖
- **解决 STEP**：M4 / STEP-P2-M4-4.1


---

## #S-12 — Windows `set_files` 真机 segfault（loopback pre-stamp mismatch → 死循环 → 3.5s 无 pong → 强制断连 → STATUS_ACCESS_VIOLATION）

- **触发 STEP**：STEP-P2-M4-4.2（`src/clipboard/windows.rs::set_files`）+ STEP-P2-M4-4.3（`src/service.rs::maybe_inject_files_to_clipboard` + pre-stamp）
- **现象**（用户 2026-09-13 真机 Windows 被控端）：
  1. **场景 1**：daemon 启动 → 主控端初始剪贴板含文件 → daemon 立即 segfault (STATUS_ACCESS_VIOLATION, 0xc0000005)
  2. **场景 2**：daemon 启动 → 复制文件 → 文件落盘成功 → `clipboard re-inject: dispatching set_files(1 path(s)) ... (pre-stamped last_outbound_files_fingerprint)` 这条 log 之后 segfault
- **调研**（leader sub-agent 派 2 个独立 bug-investigator）：
  - `next/BUG-INVESTIGATION-WINDOWS-STARTUP-SEGFAULT.md`（场景 1 静态分析；14 候选点排除）
  - `next/BUG-INVESTIGATION-WINDOWS-SET-FILES-CRASH.md`（场景 2 静态分析；9 候选点排除；test coverage gap 是 best guess）
  - 用户跑 release build 也崩 → 确认是 native crash（非 debug unwind）
- **真根因**（用户 2026-09-13 hotfix 发现）：
  - **loopback pre-stamp 用 `batch_fingerprint`（sender 算的）而非 `file_selection_fingerprint(&paths)`（receiver 本地算的）**
  - M3a STEP-3a.4 / 3a.5 的 `dispatch_files` 在 `src/service.rs:2946 / 3106` 一直用 **local fingerprint**（基于本地 paths），所以 outbound loopback short-circuit 一直工作
  - M4 STEP-4.3 的 `maybe_inject_files_to_clipboard` 错误地复用了 envelope 的 `batch_fingerprint`（sender 算的）做 pre-stamp
  - Receiver 的 next 500ms tick 算 fingerprint 用本地 paths（含 `resolve_unique_path` collision suffix ` (1)` ` (2)` 当 sender basename 已存在 accept_dir）—— 与 sender's batch_fingerprint 不匹配
  - Loop short-circuit 在 `dispatch_files_decide` 永不触发 → A → B → A' → B' → A'' 死循环
  - 3.5s 无 pong → pong watchdog 强制断连 → STATUS_ACCESS_VIOLATION 触底崩溃
- **解决方案**（用户 hotfix，3 commit）：
  - `0c9ae6c debug` — `src/main.rs` 加 debug instrumentation（panic hook + lifecycle trace）+20 行（用户调试用）
  - `6de48cc fix` — `src/clipboard/windows.rs::set_files` DROPFILES payload 清理 / `alloc_dib_handle_and_set` helper 重构 / +41/-20 行（user fix 的 windows-specific 部分 —— 静态分析未抓出的 Win32 path bug；与 #S-12 主根因 loopback 无关，但是 set_files 真机第一次入 production 时的 Win32 path 配套修复）
  - `608e51a fix(service): stamp re-inject fingerprint with local paths, not batch_fingerprint` — `src/service.rs::maybe_inject_files_to_clipboard` 把 pre-stamp 从 `last_outbound_files_fingerprint.insert(batch_fingerprint)` 改为 `last_outbound_files_fingerprint.insert(file_selection_fingerprint(&paths))`；+19/-1 行
- **结果**：用户 2026-09-13 18:30 真机确认 "现在不崩了，已经解决了"；Windows 剪贴板回灌 100% 通路
- **测试覆盖盲点教训**（**SUGGESTION #S-12 后续预防建议**）：
  - M4 STEP-4.2 的 4 个新单测全部是 `build_dropfiles_payload` pure helper，无任何 set_files Win32 API 路径测试；M4 STEP-4.3 的 11 个 reinject_decision_tests 全部是 `decide_reinject_skip` 纯函数测试，**0 个测试覆盖 `maybe_inject_files_to_clipboard` 实际调 `backend.set_files` 的 integration 路径**
  - 建议 M5 / post-M5 hotfix 阶段加 `maybe_inject_files_to_clipboard_e2e_loopback_short_circuits_after_one_round_trip` 测试（mock backend + 模拟 batch_fingerprint vs local fingerprint 不匹配场景）
- **Plan 偏差**：无 M4 STEP-4.3 偏差条目；这是 4.3 / 4.2 协同引入的真机 integration bug（4.3 loopback pre-stamp fingerprint 选择 + 4.2 set_files Win32 真机首次入 production 的 Win32 path 副作用），单 STEP 内静态分析无法精准定位
- **解决 STEP**：M4 hotfix（用户 2026-09-13 18:12 - 18:30 直接修复；不在 PLAN STEP 内；M5 STEP-5.1+ 之后无需重做）

---

## #P2.3 (M3a validator carry-forward) — `write_and_verify_file_blocking` post-write fsync before remove / rename

- **触发 STEP**：STEP-P2-M3a-3a.2（initial落地）+ M3a validator `next/STEP-VALIDATION-P2-M3a-FULL.md` P2.3 carry-forward 标记
- **现象**：`src/service.rs::write_and_verify_file_blocking` 之前 `std::fs::write` + sha256 verify + `std::fs::remove_file` 无 `sync_all` 在 write 后；power loss 在 OS page-cache flush 前可能撕裂文件。image branch 已用 `f.sync_all().await`（M2a/M2b 修过 pre-fix state）；file branch 漏同步。
- **严重度**：P2 (validator) — design smell；low risk (daemon runs on desktop, not DB); 极端 edge case。
- **解决方案**（M5 STEP-5.1 落地，commit `2629d72`）：
  - 重构 `write_and_verify_file_blocking`：
    1. **写入 `<name>.partial` 中间文件**（`File::create + write_all + sync_all()` —— fsync-then-rename 模式闭合 P2.3）
    2. sha256 verify against partial bytes
    3. 验证通过：`rename <name>.partial → final landed path`（atomic on POSIX / NTFS）
    4. 验证失败：remove `<name>.partial`（`keep_partial=false`）或保留（`keep_partial=true`，postmortem 用）
  - 新 `keep_partial: bool` 参数透传（从 4.1 IPC `ClipboardConfig.keep_partial` 字段 → 4.3 `Service::keep_partial()` getter → 5.1 接进 `write_and_verify_file_blocking`）
  - 14 新单测 + 1 扩展测试覆盖：
    - `write_and_verify_file_blocking_keep_partial_preserves_on_mismatch`（新增）
    - 现有 `write_and_verify_file_blocking_mismatch_deletes_partial` 加 `<name>.partial` 路径断言
- **结果**：fsync-then-rename 模式同时闭合 validator P2.3（fsync 缺失）+ 提供 atomic replace + 默认 .partial 删除（除非用户开 keep_partial）
- **解决 STEP**：M5 / STEP-P2-M5-5.1
