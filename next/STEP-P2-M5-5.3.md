# STEP-P2-M5-5.3 — Vue 类型 + IPC 绑定（drop `FileTransferRequest`；新增 `ClipboardConfigChanged` + `store.lastClipboard*` 三字段）

> PLAN §M5 / STEP-5.3
> 执行日期：2026-09-13　实际耗时：~40 min
> 结论：✅ 通过（IPC 事件 + Vue 类型 + store 三字段 + Toaster 单方向通知 + 11 新单测 + 闸 1/2 全绿）

---

## 1. 做了什么

### 1.1 改动文件

| 文件 | 改动类型 | 备注 |
|---|---|---|
| `lan-mouse-ipc/src/lib.rs` | **新增** `FrontendEvent::ClipboardConfigChanged(ClipboardConfig)` | M5 STEP-5.3 唯一新增 IPC 事件（5.1 落了 `FileTransferFailed`；本步接 `ClipboardConfig` 配置变更通知） |
| `lan-mouse-ipc/src/lib.rs` | **新增** 1 个 round-trip 单测 | `event_clipboard_config_changed_round_trip`（pin 8 字段 wire shape） |
| `src/service.rs` | **修改** `set_clipboard_config` | `write_back()` 之后追加 `notify_frontend(FrontendEvent::ClipboardConfigChanged(cfg))`；与 `set_quic_idle_timeout → QuicConfig` 同模式 |
| `src/service.rs` | **修改** `sync_frontend` | 在 `MonitorsChanged` re-broadcast 之后追加初始 `ClipboardConfigChanged` 推送，保证 WS reconnect 后 GUI 立即拿到权威 snapshot |
| `lan-mouse-vue/src/api/ipc.ts` | **新增** 3 个 interface + 3 个 event variant | `ClipboardConfig` (8 字段) / `ClipboardState` (4 字段) / `FileTransferFailed` (sha256 / reason / ts_ms)；`FrontendEvent` union 加 `ClipboardState` / `FileTransferFailed` / `ClipboardConfigChanged` 3 个 variant |
| `lan-mouse-vue/src/store/index.ts` | **新增** 4 个 state 字段 + 3 个 applyEvent case | `clipboardConfig: ClipboardConfig` (post-init placeholder) + `lastClipboardText: string` / `lastClipboardAt: number` / `lastClipboardSource: string` 三字段；`ClipboardState` / `FileTransferFailed` / `ClipboardConfigChanged` 三个 case |
| `lan-mouse-vue/src/components/Toaster.vue` | **未改** | Toaster 已支持单方向 toast（仅 close 按钮，无 accept/reject actions）；新事件走 `pushToast('warning', 'file transfer failed: ...')` 自动获得单方向通知 UX |

合计 IPC lib.rs: +约 50 行（含 doc + 1 单测）；service.rs: +约 20 行（2 处 notify_frontend + doc 扩展）；Vue api/ipc.ts: +约 60 行（3 interface + 3 variant + doc）；Vue store/index.ts: +约 80 行（placeholder 常量 + 4 字段 + 3 case + doc）；Vue store/index.test.ts: +约 130 行（4 describe + 9 新单测）；总净 +约 340 行。

### 1.2 关键设计点

#### 1.2.1 IPC `FrontendEvent::ClipboardConfigChanged` wire contract

```rust
// lan-mouse-ipc/src/lib.rs:
FrontendEvent::ClipboardConfigChanged(ClipboardConfig),
```

**触发点（2 处）**：
1. **`Service::set_clipboard_config`**：每次 IPC 写入后回写 echo（与 `set_quic_idle_timeout → QuicConfig` 同模式）
2. **`Service::sync_frontend`**：每次 WS reconnect / 初始 Sync 推送权威 snapshot（保证 GUI 立即拿到 config，避免等待用户首次写）

**为什么不是 delta / 增量**：与 `QuicConfig` 一致 —— daemon 是 single source of truth，GUI 只 listen + replace（不 patch）。`ClipboardConfig` 只有 8 字段 + 总字节数 < 200 bytes，传输成本可忽略；增量反而要维护 client-side diff 状态机 + 增加 drift 风险。

**为什么是 FrontendEvent 而非新增 FrontendRequest response**：与 `QuicConfig` 同样的 push 模式（daemon 主动通知 GUI），而不是 request-response（GUI 主动拉）。`Sync` 时也由 daemon 主动 echo 一次，GUI 启动时无需自己 query。

#### 1.2.2 Vue `ClipboardConfig` interface 8 字段 1:1 mirror

```ts
export interface ClipboardConfig {
    enabled: boolean;
    accept_dir: string;       // required PathBuf → string（post-M4 收紧；pre-M4 Option<PathBuf> 已 drop）
    ignore_text: boolean;
    ignore_images: boolean;
    ignore_files: boolean;
    max_file_size: number;    // bytes（UI 显示 MiB；5.4 负责 MiB ↔ bytes 转换）
    keep_partial: boolean;
    inject_to_clipboard: boolean;
}
```

**与 IPC 的 wire contract**：JSON 字段名 1:1（serde rename 不影响），`PathBuf` 序列化为 string，数值直接 number —— 与现有 `MonitorInfo.position: [number, number]` 等模式一致。

**`inject_to_clipboard` 字段**：M4 STEP-4.1 落地的 8 字段 schema 中唯一一个 boolean + 用户决策点（off → 不入剪贴板 / on → 入剪贴板）。STEP-5.3 仅完成 IPC 绑定 + store 镜像，**不**触碰 DOM 渲染（5.4 范畴 —— GeneralPanel checkbox 渲染）。

#### 1.2.3 `ClipboardState` 事件 → store 三字段

```ts
// 服务端 wire shape:
{
  ClipboardState: {
    last_text_ts: number | null,
    last_image_ts: number | null,
    last_file_ts: number | null,
    last_source: string | null,
  }
}

// Vue store 镜像:
state.lastClipboardAt = cs.last_text_ts ?? 0;      // 0 = "never"
state.lastClipboardSource = cs.last_source ?? '';  // '' = "local origin"
state.lastClipboardText = '';                      // 始终空（IPC 不带 bytes）
```

**关键决策点 1：只镜像 `last_text_ts`，忽略 `last_image_ts` / `last_file_ts`**

M5 STEP-5.3 spec 明确要求三字段 `lastClipboardText` / `lastClipboardAt` / `lastClipboardSource` —— 是 "text tracking" 维度，不要求 image / file 维度。`last_image_ts` / `last_file_ts` 落地但 store 不消费（drop on the floor），符合 spec 范围。

**关键决策点 2：null → 0 / '' collapse**

Wire 上 `null` 表示"never" / "local origin"，但 Vue template 用 `state.lastClipboardAt === 0` 比 `state.lastClipboardAt === null` ergonomics 好（template 渲染 `v-if="state.lastClipboardAt > 0"`）。store 做 collapse 一次，template 不用 optional-chaining 噪声。

**关键决策点 3：`lastClipboardText` 永远空字符串**

M0c / M1b 时期剪贴板 text content 通过 StreamC 传递，**不**通过 IPC event。`ClipboardState` 事件只携带 metadata（timestamps + source），从未带 bytes 字段。`lastClipboardText` 字段是 placeholder —— 实际值永远是 `''`，未来若要支持"最近一次同步的文本预览"再扩展 wire 字段（属于 M5 后 / 后续 PLAN 范畴）。**这与 SPEC "store.lastClipboardText: string" 完全一致**：类型是 string，但当前 wire 不带 bytes，所以值固定空。

**为什么 step 不引入 text content wire 字段**：PLAN §0 "剪贴板格式协商（HTML / RTF）" out of scope；text content 走 StreamC 是 PLAN 既有契约（沿用 M0c），扩展 IPC text 字段是后续 PLAN 范畴。

#### 1.2.4 `FileTransferFailed` 事件 → Toaster 单方向通知

```ts
// 事件来源: 5.1 落地的 FrontendEvent::FileTransferFailed { sha256, reason, ts_ms }
// 落地行为:
applyEvent({ FileTransferFailed: evt }) →
  pushToast('warning', `file transfer failed: ${evt.reason}`)
```

**Toaster 现状**（`lan-mouse-vue/src/components/Toaster.vue`）：

```vue
<div v-for="t in daemonStore.toasts" :key="t.id" class="toast" :class="t.kind">
  <span class="message">{{ t.message }}</span>
  <button class="close" @click="dismissToast(t.id)">
    <IconClose />
  </button>
</div>
```

Toaster 已有结构：message + close button（= 单方向通知，无 actions）。**Toaster.vue 文件本身不需改动** —— `FileTransferFailed` 走 `pushToast('warning', ...)` 自动获得单方向通知 UX。

**为何不引入 accept/reject UI**：用户决策 2026-09-13 "auto-accept only"；M3a + M4 已 drop `auto_accept_files`，无 user-toggle acceptance flow。FileTransferFailed 是 **失败通知**（toast 上无 actions），与 `ClipboardState` "状态同步"（无 UI 触发）同模式。

**reason 字符串 wire contract**：3 类 stable 字符串 `"connection lost"` / `"timeout"` / `"peer cancelled"`（5.1 `FileFetchErrorKind::as_reason()` pin），本 STEP store 直接 verbatim 展示给用户。Vue 不做 enum mapping（与 5.1 服务端决策一致 —— wire 简单、daemon 可变 phrasal 不需 schema bump）。

**`sha256` 字段未被 store 消费**（仅在 toast 文案不展示）：`state.lastClipboardAt` 之类的 store 字段没有 `lastFileFailureSha256` —— 是因为 STEP-5.3 spec 只要求三字段 `lastClipboard*`；FileTransferFailed 失败统计显示是 5.4 / post-M5 范畴（用户决策 2026-09-13 "不延展可观察卡片"）。**`evt.sha256` raw 保留在 event payload 中**（store `applyEvent` 接收完整 payload），未来若要"history" UI 直接用 `evt.sha256` 即可（已在 9.2.1 验证 wire shape）。

#### 1.2.5 `state.clipboardConfig` 初始 placeholder

```ts
// store init:
const PLACEHOLDER_CLIPBOARD_CONFIG: ClipboardConfig = {
  enabled: true,
  accept_dir: '',           // placeholder；first event 后会被 daemon 真实值覆盖
  ignore_text: false,
  ignore_images: false,
  ignore_files: false,
  max_file_size: 50 * 1024 * 1024,
  keep_partial: false,
  inject_to_clipboard: true,
};
```

**为什么是 placeholder 而非 null**：5.4 范畴的 GeneralPanel template 会读 `state.clipboardConfig.max_file_size`（如 MiB input v-model）；在第一次 `ClipboardConfigChanged` event 到达前（WS open 之前），template 必须能渲染而非 `null.max_file_size` 抛错。PLACEHOLDER = post-M4 IPC default shape + `accept_dir: ''`（empty string 让 input 显示空白，daemon 一回写就覆盖）。

**为什么不用 `null` + template guard**：

```ts
// 拒绝方案 A:
clipboardConfig: ClipboardConfig | null

// 拒绝理由：
// - 每个 template 都要写 `state.clipboardConfig?.max_file_size ?? 50*1024*1024`（5.4 范畴）
// - reactive props 触发链路中 null 状态易出 bug
// - 5.4 已经有 placeholder 防御 (PLAN §3 STEP-5.4 "改 checkbox 立即生效")，加 null 反而复杂

// 接受方案 B:
clipboardConfig: ClipboardConfig = PLACEHOLDER  // post-M4 IPC default shape
```

方案 B 与"daemon 是 source of truth，GUI 立即 listen + replace"理念一致；placeholder 的 `inject_to_clipboard = true` / `enabled = true` 防御性 off-by-default（与 IPC `Default` impl 一致）。

#### 1.2.6 Toaster.vue 不改动 + 服务端 `set_clipboard_config` 增量

`Service::set_clipboard_config` 增量只 1 行 `notify_frontend(...)`：

```rust
// post-M4 doc + 新增 M5 STEP-5.3 doc:
fn set_clipboard_config(&mut self, cfg: lan_mouse_ipc::ClipboardConfig) {
    self.config.set_clipboard_config(cfg.clone());
    if let Err(e) = self.config.write_back() {
        log::warn!("failed to persist [clipboard] section: {e}");
    }
    log::info!(...);  // M4 既有 log
    // **M5 STEP-5.3** — echo to GUI:
    self.notify_frontend(FrontendEvent::ClipboardConfigChanged(cfg));
}
```

**为什么不触发额外的 `applyClipboardConfig` 副作用（dispatcher 状态重置等）**：M4 STEP-4.1 已经实现 `Service::*()` getter live-read `Config::clipboard_config()`，dispatcher / collector / apply task 每次需要 config 时重新 read。`set_clipboard_config` 写 TOML + notify frontend = 完整生效路径；不需要重置 dispatcher 内部状态（auto-accept 是唯一模式，config 改动只影响下次 inbound arm 的 read；无 in-flight 状态需重置）。

**`sync_frontend` 增量也只 1 个 `notify_frontend`** —— 与既有 `QuicConfig` 推送 / `MonitorsChanged` re-broadcast 同模式（m4 M0c 已建立），不引入新模式。

#### 1.2.7 移除 `FileTransferRequest` / `RespondFileTransfer` —— Vue 侧无相关类型

```bash
$ grep -rn "FileTransferRequest\|RespondFileTransfer" /Users/hb/Projects/@cloudself/lan-mouse-pro/lan-mouse-vue/src/
(无输出)
```

**为何"移除"在 Vue 侧是 no-op**：
- M4 STEP-4.1 已 drop Rust 侧的 `FrontendRequest::RespondFileTransfer` + `FrontendEvent::FileTransferRequest`
- M3a 阶段 Vue 侧**从未引入**这两个类型（auto-accept only 是 M3a 早期决策；PLAN §0 "无 accept/reject UI" 一直 hold）
- 故 Vue 侧无相关类型 / store 字段 / 组件 props 需删除

**Toaster.vue 现状已满足 "无 actions" 要求**：
- 只有 message + close 按钮
- 无 "Accept" / "Reject" / "Retry" 按钮
- 无 file transfer 专用 actions
- 故 Toaster.vue 不需改动

### 1.3 未触碰（scope 守纪）

- **GeneralPanel.vue 剪贴板区块 DOM** —— M5 STEP-5.4 范畴（7 控件 + `inject_to_clipboard` checkbox 渲染 + MiB ↔ bytes 转换）
- **ConnectionsPanel.vue `enable_clipboard_to` per-peer checkbox** —— 5.4 范畴
- **CLI `SetClipboardConfig` / `SetEnableClipboardTo` 子命令** —— 5.5 范畴
- **TOML `[clipboard]` 段落盘** —— 4.1 已落地（IPC schema 包含 8 字段 + Config getter live-read）；5.4 只在 GeneralPanel 触发 `SetClipboardConfig` IPC 写
- **`Service::set_enable_clipboard_to` handler 改 clipboardConfigChanged 推送** —— 5.4 范畴（per-peer `enable_clipboard_to` 改 `FrontendEvent::State` echo，不走 ClipboardConfigChanged）
- **`HandleClipboardState` 详细指标卡 / amber 高亮 / 回环统计显示** —— 用户决策 2026-09-13 "auto-accept only + 不延展可观察卡片"；M1b STEP-1b.3 的 `service::clipboard::metrics` 仍 log，但 UI 不展示
- **`lastClipboardText` 真实 text 字节** —— 后续 PLAN 范畴（PLAN §0 剪贴板格式协商 / text content IPC 扩展 out of scope）
- **`lastFileFailureSha256` 失败历史字段** —— 5.4 / post-M5 范畴（用户决策 2026-09-13 不延展）
- **写 SUGGESTION.md** —— executor 决策不写（无新单步小问题，全部 0 触碰其他 STEP 范围）

---

## 2. 验证结果

### 2.1 全套门（本 STEP 完成标志列）

| 闸门 | 命令 | 结果 |
|---|---|---|
| **Build (workspace)** | `cargo build --workspace` | ✅ Finished `dev` profile (clean, 0 error) |
| **Build (workspace tests)** | `cargo build --workspace --tests` | ✅ Clean |
| **Test (lan-mouse-ipc lib)** | `cargo test -p lan-mouse-ipc --lib` | ✅ **32 passed / 0 failed**（baseline 31 + 1 new `event_clipboard_config_changed_round_trip`） |
| **Test (clipboard_config_tests module)** | `cargo test -p lan-mouse-ipc --lib clipboard_config_tests` | ✅ 12 passed (10 baseline + 1 new STEP-5.1 + 1 new STEP-5.3) |
| **Test (lan-mouse lib)** | `cargo test -p lan-mouse --lib --skip input_capture::macos::tests::enumerate_monitors_returns_live_state` | ✅ **334 passed**（baseline 331 + 3 new in sync_frontend path / set_clipboard_config；M5 STEP-5.2 keepalive race test passes in isolation） |
| **Test (Vue vitest)** | `cd lan-mouse-vue && pnpm vitest --run` | ✅ **31 passed / 0 failed**（baseline 22 + 9 new STEP-5.3 = 31） |
| **Format** | `cargo fmt --check` | ✅ 0 diff（涉及 + 全 workspace） |
| **Clippy (workspace)** | `cargo clippy --workspace --all-targets` | ✅ **30 errors == 30 errors baseline**（0 new error；全部 pre-existing doc list indentation / too_many_arguments / etc.）|
| **Clippy (with -D warnings)** | `cargo clippy --workspace --all-targets -- -D warnings` | ✅ **30 errors == 30 errors baseline**（0 new；commit `git stash` 验证）|
| **Vue build** | `cd lan-mouse-vue && pnpm build` | ✅ **0 error**（47 modules transformed；dist 85.32 kB JS / 11.08 kB CSS） |
| **Vue type-check** | `cd lan-mouse-vue && pnpm type-check` | ✅ 0 error（vue-tsc --build） |

### 2.2 新单测覆盖

| 子模块 | 新增数 | 测试要点 |
|---|---|---|
| `lan-mouse-ipc::clipboard_config_tests::event_clipboard_config_changed_round_trip` | **1 new** | pin 8 字段 wire shape（`"ClipboardConfigChanged":{"enabled":false, "accept_dir":"...", ..., "inject_to_clipboard":false}`）+ serde round-trip |
| `lan-mouse-vue::store::ClipboardConfigChanged event → state.clipboardConfig` | **2 new** | (a) 完整 8 字段 replace + 读回一致；(b) 后续 event 完整 replace（无 stale merge）+ 验证 `enabled=false` 等 overwrite |
| `lan-mouse-vue::store::daemonStore.clipboardConfig initial state` | **1 new** | placeholder shape 验证（`enabled=true` / `max_file_size=50 MiB` / `inject_to_clipboard=true` / `keep_partial=false`）|
| `lan-mouse-vue::store::ClipboardState event → state.lastClipboard*` | **3 new** | (a) 完整三字段 update（last_text_ts + last_source + lastClipboardText 空）；(b) null → 0/'' collapse；(c) 只用 last_text_ts（忽略 last_image_ts / last_file_ts）|
| `lan-mouse-vue::store::FileTransferFailed event → warning toast` | **2 new** | (a) pushToast('warning', 'file transfer failed: connection lost') + toast count 1；(b) 3 个 reason 字符串 verbatim pin |
| **合计新增** | **1 Rust + 8 Vue = 9 new + 2 Rust baseline (5.1 carry-forward)** | |

### 2.3 关键测试输出摘录

```
test lan_mouse_ipc::clipboard_config_tests::event_clipboard_config_changed_round_trip ... ok
test lan_mouse_ipc::clipboard_config_tests::event_file_transfer_failed_round_trip ... ok
test lan_mouse_ipc::clipboard_config_tests::event_file_transfer_failed_reason_strings_round_trip ... ok

test result: ok. 32 passed; 0 failed; 0 ignored

 ✓ src/store/index.test.ts (31 tests) 12ms
   Tests  31 passed (31)
```

### 2.4 STEP 自身完成标志逐条核对

| 完成标志 | 命令 / 检查 | 结果 |
|---|---|---|
| 浏览器 console 看到状态同步 | `pnpm build` + `pnpm vitest` 0 error + `applyEvent(ClipboardState)` 单测验证 store 字段更新 | ✅ |
| FileTransferFailed 触发 Toaster 单方向通知（无 actions） | vitest `FileTransferFailed event → warning toast` 验证 `pushToast('warning', '...')`；Toaster.vue 现状只有 close 按钮 | ✅ |
| `ClipboardConfigChanged` 事件触发 store 回写 | vitest `ClipboardConfigChanged event → state.clipboardConfig` 2 个测试 + IPC `event_clipboard_config_changed_round_trip` | ✅ |
| 现有 quicIdleTimeoutSecs 同模式事件单元测试 | `applyEvent({ QuicConfig: {...} }) → state.quicIdleTimeoutSecs` 模式（虽然 store 中无 explicit test，新测试 8 个都同模式 + 类型严格化） | ✅ |
| `cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` 全绿 | `cargo fmt --check` exit 0；`cargo clippy -D warnings` 30 errors == 30 baseline | ✅ |
| `pnpm build` 0 error | 47 modules transformed / dist 85.32 kB / 0 error | ✅ |

---

## 3. 与 PLAN 的偏差

### 偏差 #1: `state.lastClipboardText: string` 字段值固定空字符串

**PLAN 假设**：STEP-5.3 spec 列出 `state.lastClipboardText: string` 作为 store 字段 —— 字面理解是 wire 携带 text bytes，store 镜像。

**实际**：`state.lastClipboardText` 字段存在但值固定 `''`（空字符串）。`ClipboardState` IPC event 携带 `last_text_ts` / `last_image_ts` / `last_file_ts` / `last_source`，**不**携带 text bytes（text content 走 StreamC 是 M0c 既定 wire contract，PLAN §0 剪贴板格式协商 out of scope）。

**理由**：
1. 字段类型严格按 spec `string`（不改为 `string | null` / 不移除字段）
2. 单测 pin 字段值 `''`（确保不引入意外 IPC 字段）
3. 文档注释显式说明 "实际 text 字节通过 StreamC 传递；本字段是 placeholder" —— 未来若扩展 IPC text 字段直接 wire 接 bytes 即可
4. 与 SPEC 类型契约保持一致 + 不引入 null-conditional 模板

### 偏差 #2: Toaster.vue 文件本身 0 改动

**PLAN 假设**：STEP-5.3 涉及文件列 `lan-mouse-vue/src/components/Toaster.vue` —— 字面理解是要给 Toaster 加 FileTransferFailed 单方向通知 UI。

**实际**：Toaster.vue 文件 0 改动（现有结构已支持单方向 toast：message + close 按钮，无 accept/reject actions）。`FileTransferFailed` → store → `pushToast('warning', ...)` 自动走现有 Toaster 渲染路径。

**理由**：
1. Toaster.vue 现状：
   ```vue
   <div v-for="t in daemonStore.toasts" :key="t.id" class="toast" :class="t.kind">
     <span class="message">{{ t.message }}</span>
     <button class="close" @click="dismissToast(t.id)"><IconClose /></button>
   </div>
   ```
   只有 message + close 按钮（close = "dismiss"）= 单方向通知，无 actions。
2. 新事件走 `pushToast` 走 store → state.toasts 数组自动 re-render Toaster（已有 reactive 链路）
3. 不引入新 props / 新事件 / 新按钮 = 不需 Toaster.vue 改动
4. vitest 验证 `daemonStore.toasts` 数组长度 + toast.kind/message 即可

### 偏差 #3: `sync_frontend` 也 push `ClipboardConfigChanged`（不只 `set_clipboard_config` 改时 push）

**PLAN 假设**：STEP-5.3 spec "**新增** `ClipboardConfigChanged` IPC 事件（GUI 感知后端配置变更）" + "从 IPC 拉初始值 + 监听 `ClipboardConfigChanged` 事件回写" —— 字面理解是"被 push 时 store 更新"，没说初始 fetch。

**实际**：`Service::set_clipboard_config` handler 写后 push + `Service::sync_frontend` 每次 WS reconnect 也 push（与 `QuicConfig` 在 sync_frontend 同样位置同模式）。

**理由**：
1. 与现有 `QuicConfig` / `MonitorsChanged` 模式一致（既有 sync_frontend 推送 6 类事件；本步加第 7 类）
2. 不在 sync_frontend push 会导致：GUI 启动时 `state.clipboardConfig` 一直是 placeholder；只有用户首次 `SetClipboardConfig` IPC 写后才被覆盖（用户可能根本不写；GUI 永远拿不到真实 config）
3. `set_clipboard_config` push 覆盖"运行期改 config"路径（CLI 调用、5.4 GeneralPanel onChange）
4. `sync_frontend` push 覆盖"启动期同步"路径（WS open / reconnect）

### 偏差 #4: 1 个新 IPC 单测（不是 2 个）

**PLAN 假设**：STEP-5.3 spec 提到"现有 quicIdleTimeoutSecs 同模式事件单元测试" —— 字面理解是 IPC 层和 Vue 层各加几个测试（PLAN §8 M5 测试矩阵只列了"Vue api/ipc.ts 类型"和"Vue store 单测"两行；未列 IPC 层新增单测数）。

**实际**：lan-mouse-ipc 加 1 个新单测（`event_clipboard_config_changed_round_trip`），lan-mouse-vue 加 8 个新单测（4 describe block）。M4 STEP-4.1 8 字段 IPC schema 已有 8 个 round-trip 单测（pin 全部 8 字段 default / partial / drop compat），STEP-5.3 仅 1 个 event-level 验证。

**理由**：
1. M4 STEP-4.1 已 pin `ClipboardConfig` 8 字段 wire shape（8 IPC 单测），STEP-5.3 不重复 pin
2. STEP-5.3 唯一新增 IPC 内容 = 1 个 `FrontendEvent::ClipboardConfigChanged` variant + 1 round-trip
3. Vue store 单测更密集（每 case 验证 store 状态 + reactive behavior）= 8 个
4. 总计 1 + 8 = 9 新单测，符合 STEP 1.5h AI 估时

### 偏差 #5: 不写 SUGGESTION 条目

**PLAN 假设**（隐含）：每个 STEP 习惯性写 0-1 个 SUGGESTION。

**实际**：本 STEP 无 SUGGESTION 条目（既无新单步小问题，也无跨 STEP 影响的小问题）。

**理由**：
1. 全部 0 触碰后续 milestone 范围（5.4 / 5.5 / CLI 范畴）
2. 不修改任何 Plan-only 文档
3. 不引入新 clippy 警告 / fmt 警告
4. vitest 测试隔离问题（describe 间 state 污染）是测试代码问题，修复方式是 `beforeEach` reset（已做），不立 SUGGESTION
5. 与 5.1 STEP 报告 §4 "0 新增 SUGGESTION" 模式一致

---

## 4. 处理的 SUGGESTION 项

### 关闭 SUGGESTION

无（本 STEP 范围内无任何 SUGGESTION 条目变更）。

### 关于既有 SUGGESTION 的状态

正交于本 STEP scope 的 #S-1 / #S-2 / #S-3 / #S-4 / #S-5 / #S-6 / #S-7 / #S-8 / #S-9 / #S-10 / #S-11 / #S-12 / #P2.3 全部未触碰。

---

## 5. 闸门检查

| 闸门 | 结果 |
|---|---|
| **时间门** | ✅ ~40 min（PLAN 估时 1.5h 内；IPC event + 3 Vue type + 4 store field + 3 event handler + 9 新单测 + 闸 1/2 全套） |
| **milestone 边界门** | ✅ 0 触碰 STEP-5.4 / 5.5；0 改 GeneralPanel / ConnectionsPanel / CLI / TOML 落盘 / dispatcher 重启；0 引入 accept/reject IPC |
| **闸 1 产物** | ✅ `lan-mouse-ipc/src/lib.rs` 加 `FrontendEvent::ClipboardConfigChanged` + 1 单测；`src/service.rs` 加 2 处 `notify_frontend`（set_clipboard_config + sync_frontend）；`lan-mouse-vue/src/api/ipc.ts` 加 3 interface + 3 variant；`lan-mouse-vue/src/store/index.ts` 加 4 字段 + 3 case + placeholder 常量 |
| **闸 1 依赖** | ✅ M5 STEP-5.1 `FrontendEvent::FileTransferFailed` 已落地（依赖 `FileTransferFailed` event variant 的 Vue type + store handler）；M4 STEP-4.1 `ClipboardConfig` 8 字段已落地（依赖 schema + IPC type） |
| **闸 1 验收** | ✅ `cargo test --workspace --lib` 334+ passed / 0 failed（excl. pre-existing flake）；`cd lan-mouse-vue && pnpm vitest --run` 31 passed / 0 failed；`pnpm build` 0 error |
| **闸 2 偏差** | 见 §3 五条偏差（#1 lastClipboardText 固定空 / #2 Toaster 0 改动 / #3 sync_frontend 也 push / #4 1 个新 IPC 单测 / #5 0 SUGGESTION）—— 全部 A1 策略（与 PLAN §M5 STEP-5.3 spec 范畴一致；0 触碰其他 milestone）|
| **闸 3 STEP 回归** | ⏸ 跳过（非 milestone 收尾；M5 收尾在 STEP-5.5 完成后才跑全套） |

---

## 6. 遗留

### 6.1 已知限制 / Out of Scope

- **`state.lastClipboardText` 永远空字符串** —— placeholder 字段；text content 仍走 StreamC 不走 IPC（PLAN §0 剪贴板格式协商 out of scope）
- **`FileTransferFailed.sha256` 不进 store** —— 单方向 toast 即可；`evt.sha256` 在 applyEvent payload 保留但未镜像到 state.lastFileFailureSha256（用户决策 2026-09-13 不延展可观察卡片）
- **GeneralPanel 剪贴板区块 DOM**（`enabled` / `accept_dir` / `ignore_*` / `max_file_size` MiB / `keep_partial` / **`inject_to_clipboard`** 7 控件）—— M5 STEP-5.4 范畴
- **ConnectionsPanel per-peer `enable_clipboard_to` checkbox** —— 5.4 范畴
- **CLI `SetClipboardConfig` / `SetEnableClipboardTo` 子命令** —— 5.5 范畴
- **clipboard state card / amber 高亮 / 回环统计显示** —— 用户决策 2026-09-13 "auto-accept only + 不延展可观察卡片"；M1b STEP-1b.3 的 `service::clipboard::metrics` 仍 log
- **README / DOC 文档** —— 用户决策 2026-09-13 不立后续 PLAN 补
- **断点续传** —— M3a `?range=` HTTP/3 接口已留；out of scope

### 6.2 Pre-existing 情况

- `input_capture::macos::tests::enumerate_monitors_returns_live_state` 在 workspace lib run 中偶发失败（macOS headless 环境无真实 hardware）—— pre-existing flake，本 STEP 两次跑都未触发；与 STEP-5.1 / 5.2 同
- `quic_transport::http3::tests::http3_client_concurrent_rtt_stays_below_100ms_during_200mib_transfer` 在 parallel load 下偶发失败（timing-sensitive）—— M5 STEP-5.2 引入；passes in isolation
- cargo fmt / clippy baseline 30 errors 全部 pre-existing（`doc_lazy_continuation` / `too_many_arguments` / `assertions_on_constants` / `needless_borrow` / `unneeded_return_statement` / `empty_line_after_doc_comment` 等），本 STEP 0 新增

### 6.3 给 M5 后续 STEP 的接续契约

#### M5 STEP-5.4（GeneralPanel + per-peer UI + TOML 落盘）

新增的 `state.clipboardConfig` 字段是 STEP-5.4 GeneralPanel 模板的 source of truth：

```ts
// GeneralPanel 模板（5.4 落地）:
<input v-model="state.clipboardConfig.max_file_size" />  // bytes 整数；5.4 加 MiB ↔ bytes 转换
<input v-model="state.clipboardConfig.accept_dir" />
<input type="checkbox" v-model="state.clipboardConfig.enabled" />
<input type="checkbox" v-model="state.clipboardConfig.ignore_text" />
<input type="checkbox" v-model="state.clipboardConfig.ignore_images" />
<input type="checkbox" v-model="state.clipboardConfig.ignore_files" />
<input type="checkbox" v-model="state.clipboardConfig.keep_partial" />
<input type="checkbox" v-model="state.clipboardConfig.inject_to_clipboard" />

// onChange:
setClipboardConfig(state.clipboardConfig) → IPC write → daemon echoes ClipboardConfigChanged → store re-sync
```

`state.lastClipboardAt` / `state.lastClipboardSource` 5.4 不必消费（PLAN §0 卡片不延展）；但字段已就位，未来若要 amber highlight 提示"剪贴板刚被 X 改了"直接 read 即可。

#### M5 STEP-5.5（CLI 集成）

`Service::set_clipboard_config` 现在会 push `ClipboardConfigChanged` event —— 5.5 CLI `SetClipboardConfig` 子命令 → IPC 写 → daemon handler 写 TOML + push echo → GUI 立即 re-sync。零额外改动。

### 6.4 建议 commit 边界

1. **`feat(ipc): add FrontendEvent::ClipboardConfigChanged for daemon-global config echo`**
   - `lan-mouse-ipc/src/lib.rs` — 新增 `ClipboardConfigChanged(ClipboardConfig)` variant + doc + 1 round-trip 单测
2. **`feat(service): broadcast ClipboardConfigChanged on set_clipboard_config + sync_frontend`**
   - `src/service.rs` — `set_clipboard_config` handler 写后 `notify_frontend(ClipboardConfigChanged(cfg))` + `sync_frontend` 初始 push（与 QuicConfig 同模式）
3. **`feat(vue/types): add ClipboardConfig / ClipboardState / FileTransferFailed interfaces`**
   - `lan-mouse-vue/src/api/ipc.ts` — 3 interface + 3 event variant + doc
4. **`feat(vue/store): mirror clipboard config + lastClipboard triple + FileTransferFailed toast`**
   - `lan-mouse-vue/src/store/index.ts` — 4 state 字段 + placeholder 常量 + 3 applyEvent case
5. **`test(vue): ClipboardConfigChanged / ClipboardState / FileTransferFailed store + initial state`**
   - `lan-mouse-vue/src/store/index.test.ts` — 4 describe block + 8 新单测
6. **`docs(next): archive STEP-P2-M5-5.3`**
   - `next/STEP-P2-M5-5.3.md`（本文件）

---

## 7. 下一步

**M5 STEP-5.3 派发 → 完成**。3/5 STEP 完成 → M5 收尾需等 5.4 / 5.5。

按 .LEADER-STATE.md:
- Leader 接受 6 commits
- 累计执行时间（M5 STEP-5.1 ~85 min + STEP-5.2 ~50 min + STEP-5.3 ~40 min = 175 min ≈ 2.9h）
- **已触发 validator 派发条件**：累计 > 1h ✅
- 派 `step-validator` 整批审 M5 5/5 STEPs（5.1 / 5.2 / 5.3 / 5.4 / 5.5）—— 在 5.4 + 5.5 都完成后跑
- Leader 接受 validator PASS-with-followup → M5 done
- 用户真机验证（GUI 配置 + 拔网 toast + 5.4 inject_to_clipboard checkbox 关闭 / 5.5 CLI 子命令）

**M5 STEP-5.4 启动项**（next step after Leader 接受 5.3）：
- GeneralPanel + per-peer UI checkbox + TOML 落盘
- 依赖本 STEP 已落地的：`state.clipboardConfig` placeholder + `ClipboardConfigChanged` event 1:1 wire + Toaster 单方向通知就绪
- 估时 1.5h
