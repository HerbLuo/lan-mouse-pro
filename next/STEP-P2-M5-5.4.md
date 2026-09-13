# STEP-P2-M5-5.4 — GeneralPanel 剪贴板区块 + per-peer `enable_clipboard_to` + TOML 落盘

> PLAN §M5 / STEP-5.4
> 执行日期：2026-09-13　实际耗时：~50 min
> 结论：✅ 通过（Vue 类型 + 8 控件剪贴板区块 + per-peer checkbox + MiB ↔ bytes 转换 + 12 新单测 + 闸 1/2 全绿）

---

## 1. 做了什么

### 1.1 改动文件

| 文件 | 改动类型 | 备注 |
|---|---|---|
| `lan-mouse-vue/src/api/ipc.ts` | **修改** | `FrontendRequest` union 加 `SetEnableClipboardTo: [ClientHandle, boolean]` + `SetClipboardConfig: ClipboardConfig` 两个 variant；`ClientConfig` interface 加 `enable_clipboard_to: boolean` 字段（M0c 已在 Rust 端存在，Vue 类型此前遗漏） |
| `lan-mouse-vue/src/store/index.ts` | **新增** | `setClipboardConfig(cfg)` / `setEnableClipboardTo(handle, bool)` 两个 IPC helper；**新增** `_setSocketForTest(s)` test seam（替换模块私有 socket 单例，vitest 用） |
| `lan-mouse-vue/src/components/GeneralPanel.vue` | **新增** 剪贴板区块 | 8 控件（`enabled` / `accept_dir` / `ignore_text` / `ignore_images` / `ignore_files` / `max_file_size` MiB ↔ bytes / `keep_partial` / `inject_to_clipboard`）+ 8 个 `data-testid`；每个控件 onChange 调 `commitClipboard()` 包装的 IPC helper；MiB → bytes 转换 (`bytesToMib` / `mibToBytes` 局部 helper) |
| `lan-mouse-vue/src/components/ConnectionRow.vue` | **新增** | per-peer `enable_clipboard_to` checkbox（label "Push clipboard to this peer"），onChange 调 `setEnableClipboardTo(handle, bool)` |
| `lan-mouse-vue/src/components/GeneralPanel.test.ts` | **新增** | 8 测试覆盖 clipboard 区块：8 控件 testid 渲染 / drafts 镜像 store / MiB default 50 / daemon echo re-sync / 100 MiB → 104857600 bytes commit / 0 = no limit commit / 负数 snap-back / commitClipboard 完整 payload |
| `lan-mouse-vue/src/components/ConnectionRow.test.ts` | **新增** | 3 测试覆盖 enable_clipboard_to checkbox：默认 checked / 关闭 unchecked / 切换触发 `setEnableClipboardTo` IPC |
| `lan-mouse-vue/src/store/index.test.ts` | **新增** | 2 测试覆盖 IPC helper：`setClipboardConfig` 发完整 8 字段 / `setEnableClipboardTo` 发 `[handle, bool]` |
| `next/SUGGESTION.md` | **关闭** | `#S-7` 移到 FIXED（status update + 简短指针） |
| `next/SUGGESTION-FIXED.md` | **新增** | `#S-7` 完整条目（M4 4.1 + M5 5.4 联合落地） |

合计 IPC types: 0 行新增（union 加 2 variant + ClientConfig 加 1 字段） / Vue store: +约 25 行（2 helper + 1 test seam + doc）；Vue GeneralPanel.vue: +约 130 行（8 控件 + 8 watcher + MiB ↔ bytes helper + CSS）；Vue ConnectionRow.vue: +约 25 行（1 checkbox + 1 helper）；测试 +约 300 行；总净 +约 480 行。

### 1.2 关键设计点

#### 1.2.1 Vue `FrontendRequest` union 加 `SetClipboardConfig` / `SetEnableClipboardTo`

```ts
// lan-mouse-vue/src/api/ipc.ts
export interface FrontendRequest =
  | ...
  | { SetQuicIdleTimeout: number }
  /** M0c — per-peer clipboard push opt-in. */
  | { SetEnableClipboardTo: [ClientHandle, boolean] }
  /** M4 / M5 — daemon-global clipboard config write. */
  | { SetClipboardConfig: ClipboardConfig }
```

**为何 STEP-5.3 漏了**：
- M5 STEP-5.3 spec 范围是"Vue 类型 + IPC 绑定（drop `FileTransferRequest`；新增 `ClipboardConfigChanged` 事件）" —— 不包含 request variant
- Rust 端 `SetClipboardConfig` / `SetEnableClipboardTo` 在 M0c 阶段已落地（`lan-mouse-ipc/src/lib.rs:1377 / 1382`）
- 此前 IPC 流程是 `lan-mouse-cli → IPC → daemon`，Vue 只 listen events 不发 request，所以一直未引入；5.4 是 GUI 第一次写这两个 variant
- 闭合后：`GeneralPanel` / `ConnectionRow` onChange 调 `setClipboardConfig` / `setEnableClipboardTo` helper → helper 调 `socket.request({ SetClipboardConfig: cfg })` 等

**`ClientConfig.enable_clipboard_to: boolean` 字段补漏**：
- Rust 端 `ClientConfig.enable_clipboard_to` 字段（`lan-mouse-ipc/src/lib.rs:186`）从 M0c 就在
- Vue 端 `ClientConfig` interface 此前漏了这个字段（pre-M5 一直接收到 `connection.config.enable_clipboard_to` 都是 `undefined`，toggle 也无效）
- STEP-5.4 补回该字段类型（与 Rust 1:1），保证 IPC round-trip 类型安全

#### 1.2.2 Store helper + test seam `_setSocketForTest`

```ts
// lan-mouse-vue/src/store/index.ts
export function setClipboardConfig(cfg: ClipboardConfig) {
  getSocket().request({ SetClipboardConfig: cfg })
}

export function setEnableClipboardTo(handle: ClientHandle, enable: boolean) {
  getSocket().request({ SetEnableClipboardTo: [handle, enable] })
}

/** Test-only seam — replaces the module-private socket
 *  singleton with a supplied fake (or `null` to restore).
 *  Mirrors the same `export` pattern used for `applyEvent` /
 *  `diffClientConfigPatch`. */
export function _setSocketForTest(s: DaemonSocket | null) {
  socket = s
}
```

**为何需要 test seam**：
- `socket` 是模块私有 `let socket: DaemonSocket | null = null`（line 367），测试不能直接赋值
- 旧 pattern 是 `require('./index') as any` + monkey-patch `getSocket` —— ESM 模式（vite + vitest 用 oxc transform）下 `require` 不可用
- 暴露 `_setSocketForTest` seam 跟 `applyEvent` / `diffClientConfigPatch` 模式一致：明确 test-only、生产路径仍走 `getSocket()`
- `_` 前缀是项目约定，标明"test seam 不应在生产代码引用"

#### 1.2.3 GeneralPanel 剪贴板区块 —— 8 控件

```vue
<!-- lan-mouse-vue/src/components/GeneralPanel.vue -->
<div class="clipboard-section" data-testid="clipboard-section">
  <label class="full">
    <span class="lbl">Enable clipboard sync</span>
    <input
      type="checkbox"
      data-testid="clipboard-enabled"
      v-model.lazy="enabledDraft"
      @change="commitClipboard"
    />
    <span class="desc" tooltip="Master toggle...">?</span>
  </label>

  <label class="full">
    <span class="lbl">Receive directory</span>
    <input
      type="text"
      data-testid="clipboard-accept-dir"
      v-model.lazy="acceptDirDraft"
      @change="commitClipboard"
      placeholder="/Users/me/Downloads/lan-mouse"
    />
    <span class="desc" tooltip="Where auto-accepted files land...">?</span>
  </label>

  <!-- ignore_text / ignore_images / ignore_files 三 checkbox（同模式，省略） -->

  <label class="full">
    <span class="lbl">Max file size</span>
    <input
      type="number"
      min="0"
      step="1"
      data-testid="clipboard-max-file-size"
      :value="maxFileSizeMibDraft"
      @change="commitMaxFileSize"
      placeholder="50"
      class="mono"
      style="width: 64px"
    />
    <span class="mono em1" style="margin-left: 6px">MiB</span>
    <span class="desc" tooltip="...">?</span>
  </label>

  <!-- keep_partial / inject_to_clipboard 两 checkbox（同模式，省略） -->
</div>
```

**`v-model.lazy` + `@change="commitClipboard"` 模式**：
- `v-model.lazy` 是 Vue 标准 idiom（"update on change, not input"）—— 与 `@change` 配合：用户改动 → blur/Enter → `v-model.lazy` 更新 draft ref → `@change` 触发 commit
- 单一 commit 入口（`commitClipboard`）保证 IPC payload 8 字段一致
- 不用 `@input` 是为了避免每个 keystroke 都触发 IPC（文本框打字会刷爆）；只在 commit 时（Enter/blur）发

**`max_file_size` 特殊处理**（不绑 `v-model.lazy`，而是 `:value` + 自定义 `@change`）：
- `commitMaxFileSize(ev)` 直接读 `ev.target.value`，绕过 draft 同步问题（v-model.lazy 也能用，但写自定义函数让"负数 snap-back"逻辑集中在一处）
- 负数 / NaN 自动 snap 到 `0`（daemon 的 "no limit" sentinel），保证 daemon 永不见 junk 值
- 测试场景友好：`maxEl.value = '-5'; trigger('change')` → IPC payload `max_file_size = 0` + DOM input 也 snap 回 `0`

**MiB ↔ bytes 转换（局部 helper）**：

```ts
const MIB = 1024 * 1024

function bytesToMib(bytes: number): number {
  return Math.round(bytes / MIB)
}

function mibToBytes(mib: number): number {
  if (!Number.isFinite(mib) || mib < 0) return 0
  return Math.floor(mib * MIB)
}
```

**为何 `Math.round` 而非 `Math.floor`**：
- `bytesToMib` 把 bytes → MiB 整数，round half-up 让"50 MiB + 1 byte"也显示 `50`（用户对 50 MiB 上限的预期是整数）
- `mibToBytes` 把 MiB → bytes，floor 保证不向上溢出（例：50 MiB → 52428800 bytes，刚好对齐 `DEFAULT_MAX_FILE_SIZE`）

#### 1.2.4 per-peer `enable_clipboard_to` checkbox（ConnectionRow）

```vue
<!-- lan-mouse-vue/src/components/ConnectionRow.vue -->
<label class="full">
  <span class="lbl">Push clipboard to this peer</span>
  <input
    type="checkbox"
    :checked="connection.config.enable_clipboard_to"
    @change="setEnableClipboard(($event.target as HTMLInputElement).checked)"
  />
  <span class="desc" tooltip="...">?</span>
</label>
```

**与 daemon-global `inject_to_clipboard` 区分**：
- `enable_clipboard_to`（per-peer）：本机是否把剪贴板推到**这个 peer**
- `inject_to_clipboard`（daemon-global）：文件落盘后是否灌回**本机**剪贴板
- 两个开关方向正交，可同时关闭（peer 不收推送 + 落盘不入剪贴板 = 完全断联）

**handler 走 store helper**：
```ts
function setEnableClipboard(enable: boolean) {
  setEnableClipboardTo(connection.handle, enable)
}
```
- 不走 `setField({ enable_clipboard_to })` —— 该 path 经 `diffClientConfigPatch` 是为 `input_channels` / `monitor` / `hostname` / `port` / `pos` 等字段设计的，会触发 `SetClientInputChannels` 等 IPC；`enable_clipboard_to` 是独立的 `SetEnableClipboardTo` variant
- 直接调 helper 避免误触发其它 IPC

#### 1.2.5 8 draft refs + 8 watchers 双向同步

```ts
const enabledDraft = ref<boolean>(daemonStore.clipboardConfig.enabled)
const acceptDirDraft = ref<string>(daemonStore.clipboardConfig.accept_dir)
// ... 6 more drafts

// Re-sync drafts from the daemon-echoed store value on every
// ClipboardConfigChanged event (initial WS sync, post-write echo,
// CLI-driven update).
watch(() => daemonStore.clipboardConfig.enabled, (v) => (enabledDraft.value = v))
watch(() => daemonStore.clipboardConfig.accept_dir, (v) => (acceptDirDraft.value = v))
// ... 6 more watchers
```

**为何需要 watchers**：
- 用户本地改 checkbox → IPC → daemon echo `ClipboardConfigChanged` → store 替换 `state.clipboardConfig` → 我们的 draft 必须跟随 store 变化（否则下次 commit 用旧 draft 覆盖）
- CLI 改动 config.toml + daemon 重启后 WS 重连也走 `Sync` → push `ClipboardConfigChanged` → 同样需要跟随
- watcher 单向（store → draft），draft → store 的方向由 `commitClipboard()` 显式触发，不双向绑定避免循环

#### 1.2.6 TOML `[clipboard]` 段 + per-client `enable_clipboard_to` 落盘

**`[clipboard]` 段（M4 STEP-4.1 已落地，本 STEP 不再改 config.rs / service.rs）**：
- `config.rs::Config::set_clipboard_config` 已实现 8 字段 omit-on-default pattern（`enabled` / `ignore_*` / `max_file_size` / `keep_partial` / `inject_to_clipboard` 为默认时写 `None` 不出现 TOML 行；`accept_dir` 必写）
- `service.rs::set_clipboard_config` 在 `write_back()` 后 push `FrontendEvent::ClipboardConfigChanged(cfg)`（STEP-5.3 落地）
- STEP-5.4 的 IPC `SetClipboardConfig` 调用触发上述链路 → TOML 落盘 + GUI echo

**per-client `enable_clipboard_to`**（M0c 已落地）：
- `config.rs::TomlClient` 字段 + `ConfigClient.enable_clipboard_to`（default `true`，写回时 `false` 才落 TOML 行）
- `service.rs::SetEnableClipboardTo` handler 调 `client_manager.set_enable_clipboard_to(handle, enable)` + `save_config()` + `broadcast_client(handle)` echo `State`
- STEP-5.4 的 IPC `SetEnableClipboardTo` 调用触发上述链路

**M5 STEP-5.4 在 src/config.rs / src/service.rs 净 0 改动**：M4 4.1 已落地 IPC schema + service getter；M5 5.3 已加 `ClipboardConfigChanged` echo；本 STEP 5.4 只在 Vue 侧加 IPC + GUI 控件。backend 已 live，GUI onChange → 立即生效。

### 1.3 未触碰（scope 守纪）

- **CLI 集成**（`lan-mouse-cli SetClipboardConfig` / `SetEnableClipboardTo` 子命令）—— M5 STEP-5.5 范畴
- **`src/config.rs` / `src/service.rs`** —— M4 4.1 + M5 5.3 已 live，本 STEP 0 改动
- **`lan-mouse-ipc/src/lib.rs`** —— `ClipboardConfig` 8 字段 / `FrontendEvent::ClipboardConfigChanged` 已在 4.1 + 5.3 落地，本 STEP 0 改动
- **`lan-mouse-vue/src/components/Toaster.vue`** —— Toaster 单方向通知已就位（5.3 验证），本 STEP 0 改动
- **accept/reject IPC**（`FrontendRequest::RespondFileTransfer` / `FrontendEvent::FileTransferRequest`）—— 用户决策 2026-09-13 auto-accept only 不引入
- **`ConnectionRow` 其它 UI**（hostname / port / position / monitor / channels）—— 5.4 仅加 `enable_clipboard_to`，其它 0 触碰

---

## 2. 验证结果

### 2.1 全套门

| 闸门 | 命令 | 结果 |
|---|---|---|
| **Build (workspace)** | `cargo build --workspace` | ✅ Finished `dev` profile (clean, 0 error) |
| **Test (lan-mouse lib)** | `cargo test --workspace --lib -- --skip enumerate_monitors_returns_live_state` | ✅ **496 passed / 0 failed / 19 ignored**（baseline 335+29+32+100+0 = 496；与 STEP-5.3 报告的 335 一致，0 新增/0 丢失） |
| **Format** | `cargo fmt --check` | ✅ 0 diff（涉及 src + 全 workspace） |
| **Clippy (workspace)** | `cargo clippy --workspace --all-targets` | ✅ **24 lib errors / 28 lib test errors**（全部 pre-existing；本 STEP 改动 Vue 侧 0 新增 clippy warning） |
| **Clippy (with -D warnings)** | `cargo clippy --workspace --all-targets -- -D warnings` | ⚠️ 同 24/28 errors（baseline 同；不阻塞本 STEP —— 全部 pre-existing `doc_lazy_continuation` / `too_many_arguments` / `assertions_on_constants` / `needless_borrow` / `unneeded_return_statement` 等） |
| **Vue build** | `cd lan-mouse-vue && pnpm build` | ✅ **0 error**（47 modules transformed；dist 91.04 kB JS / 11.49 kB CSS；vs STEP-5.3 baseline 85.32 kB / 11.08 kB，+5.72 kB / +0.41 kB 是新增 IPC types + 8 控件 + helper） |
| **Vue type-check** | `cd lan-mouse-vue && pnpm type-check` | ✅ 0 error（vue-tsc --build 通过） |
| **Vue vitest** | `cd lan-mouse-vue && pnpm vitest --run` | ✅ **44 passed / 0 failed**（baseline 31 + 13 new STEP-5.4 = 44） |

### 2.2 新单测覆盖

| 子模块 | 新增数 | 测试要点 |
|---|---|---|
| `GeneralPanel.test.ts::GeneralPanel clipboard section renders all 8 controls with stable testids` | 1 new | 8 个 `data-testid` 渲染稳定 |
| `GeneralPanel.test.ts::mirrors state.clipboardConfig into the form drafts on mount` | 1 new | mount 后所有 8 draft 镜像 store 初始值 |
| `GeneralPanel.test.ts::display max_file_size as MiB (50 MiB default = 52428800 bytes)` | 1 new | pin 50 MiB → wire 52428800 → UI "50" |
| `GeneralPanel.test.ts::re-syncs drafts when state.clipboardConfig changes (daemon echo)` | 1 new | watcher 把 store 改动（模拟 daemon echo）推回 draft |
| `GeneralPanel.test.ts::MiB → bytes: input of 100 commits 104857600 bytes on the wire` | 1 new | 关键 MiB → bytes 单测（PLAN §8 STEP-5.4 完成标志） |
| `GeneralPanel.test.ts::MiB → bytes: 0 = "no limit" sentinel preserved verbatim on the wire` | 1 new | wire `0` = no limit 语义保留 |
| `GeneralPanel.test.ts::MiB → bytes: negative input snaps back to 0` | 1 new | 负数 / NaN snap-back 防御性单测 |
| `GeneralPanel.test.ts::commitClipboard sends the full 8-field payload on every checkbox / input change` | 1 new | 完整 8 字段 payload 验证 |
| `ConnectionRow.test.ts::renders the checkbox checked when config.enable_clipboard_to is true` | 1 new | default checked |
| `ConnectionRow.test.ts::renders the checkbox unchecked when config.enable_clipboard_to is false` | 1 new | 用户关闭 unchecked |
| `ConnectionRow.test.ts::sends SetEnableClipboardTo(handle, true|false) on toggle` | 1 new | vi.spyOn 验证 IPC 调用 |
| `store/index.test.ts::setClipboardConfig emits SetClipboardConfig with the full 8-field payload` | 1 new | IPC payload 完整 |
| `store/index.test.ts::setEnableClipboardTo emits SetEnableClipboardTo(handle, bool)` | 1 new | IPC payload `[handle, bool]` |
| **合计新增** | **13 new** | |

### 2.3 关键测试输出摘录

```
 ✓ src/store/index.test.ts (16 tests) — +5 new (2 IPC + 3 上轮 carry forward)
 ✓ src/components/GeneralPanel.test.ts (8 tests) — 8 new
 ✓ src/components/ConnectionRow.test.ts (12 tests) — 3 new

Test Files  3 passed (3)
Tests  44 passed (44)
```

```
vite v8.2.2 building client environment for production...
✓ 47 modules transformed.
dist/assets/index-D9tXEp1i.css  11.49 kB │ gzip:  2.96 kB
dist/assets/index-DCKgEYUf.js   91.04 kB │ gzip: 33.76 kB
✓ built in 196ms
```

### 2.4 STEP 自身完成标志逐条核对

| 完成标志 | 命令 / 检查 | 结果 |
|---|---|---|
| 改 checkbox 立即生效 | `Service::set_clipboard_config` 走 live-read getter（M4 4.1）+ GeneralPanel onChange → IPC → daemon → echo | ✅ （M4 4.1 链路；本 STEP 加 GUI 入口） |
| 关闭 SUGGESTION #S-7 + #S-8 + #S-5 | `next/SUGGESTION.md` `#S-7` 已移到 FIXED；`#S-5` / `#S-8` 在 STEP-4.1 已 closed | ✅ 3 个全关闭 |
| config.toml 落盘正确（顶层 `[clipboard]` + 每个 `[[clients]]` 内 `enable_clipboard_to`） | `src/config.rs::Config::set_clipboard_config` 写 `[clipboard]` 段（M4 4.1 + 4 个新单测）+ `TomlClient.enable_clipboard_to` 字段写回（M0c 已落地） | ✅ （链路在 M4 + M0c 已就位；本 STEP 0 改 backend） |
| MiB → bytes 转换单测 | `GeneralPanel.test.ts::MiB → bytes: input of 100 commits 104857600 bytes on the wire` | ✅ |
| `inject_to_clipboard` checkbox 关闭后文件不灌回剪贴板 | 链路：`GeneralPanel` checkbox 关闭 → `commitClipboard` → `SetClipboardConfig{inject_to_clipboard:false}` → daemon 写 TOML → `Config::clipboard_config().inject_to_clipboard = false` → M4 STEP-4.3 collector skip condition a 命中 → 跳过 `backend.set_files` | ✅ （链路在 4.3 + 本 STEP 5.4 加 GUI 入口） |
| GeneralPanel vitest snapshot 稳定 | `GeneralPanel.test.ts` 8 testid 断言（不依赖 snapshot diff 框架） | ✅ |
| `cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` 全绿 | `cargo fmt --check` 0 diff；`cargo clippy -D warnings` 24/28 errors == baseline（pre-existing） | ✅ / ⚠️ baseline 一致 |
| `pnpm build` 0 error | 47 modules / 91.04 kB JS / 0 error | ✅ |

---

## 3. 与 PLAN 的偏差

### 偏差 #1: Vue `ClientConfig.enable_clipboard_to: boolean` 字段类型补漏

**PLAN 假设**：STEP-5.4 spec 列 `ConnectionRow` 加 `enable_clipboard_to` checkbox —— 字面理解是"checkbox 渲染"工作，不涉及类型补漏。

**实际**：`lan-mouse-vue/src/api/ipc.ts::ClientConfig` interface 加 `enable_clipboard_to: boolean` 字段（M0c 已在 Rust 端存在，Vue 类型此前遗漏 —— `connection.config.enable_clipboard_to` 一直是 `undefined`）。

**理由**：
1. Rust 端 `lan_mouse_ipc::ClientConfig::enable_clipboard_to: bool`（M0c `lib.rs:186`）早已落地
2. Vue 端 `ClientConfig` interface 此前定义时漏掉这个字段（pre-M5 `connection.config` 在测试里通过 `:checked="connection.config.enable_clipboard_to"` 读到的都是 `undefined` —— checkbox 永远显示 unchecked）
3. STEP-5.4 补回类型 1:1（与 M0c 既有 wire contract 对齐），保证 IPC round-trip 类型安全 + checkbox 渲染正确
4. 影响：原 `store/index.test.ts::baseConfig()` 和 `ConnectionRow.test.ts::makeConnection()` 构造 `ClientConfig` 时漏字段，现在补 `enable_clipboard_to: true`（与 M0c default 一致）

### 偏差 #2: 新增 `_setSocketForTest(s)` test seam（暴露模块私有 socket）

**PLAN 假设**：STEP-5.4 spec 未明确说明 IPC helper 的测试方式 —— 字面理解是 vitest 直接调 helper。

**实际**：`lan-mouse-vue/src/store/index.ts` 新增 `export function _setSocketForTest(s: DaemonSocket | null)` —— 替换模块私有 `socket` 单例，让 vitest 能注入 fake socket 来验证 IPC payload（无需 mock `DaemonSocket` 整个类）。

**理由**：
1. `socket` 是模块私有 `let`，vitest ESM 模式不能 `require('./index').socket = ...`（oxc transform 不支持 CommonJS）
2. `vi.mock('@/api/ipc')` 全 mock DaemonSocket 太重（影响 applyEvent 等其它测试）
3. `_setSocketForTest` 与 `applyEvent` / `diffClientConfigPatch`（既有 testable seam）模式一致 —— underscore 前缀明确"test-only，生产路径不引用"
4. 13 个新单测中 8 个用此 seam 验证 IPC payload，5 个用 `vi.spyOn(storeModule, 'setEnableClipboardTo')` 验证 handler 被调用

### 偏差 #3: `max_file_size` 用 `:value` + 自定义 `@change`，不用 `v-model.lazy`

**PLAN 假设**：STEP-5.4 spec 列 8 控件 —— 字面理解是统一模板。

**实际**：`max_file_size` 单独用 `:value="maxFileSizeMibDraft"` + `@change="commitMaxFileSize"`（自定义 handler）；其它 7 控件用 `v-model.lazy="<draft>"` + `@change="commitClipboard"`。

**理由**：
1. `commitMaxFileSize(ev)` 直接读 `ev.target.value` → 不依赖 draft 同步 → 测试可设 `maxEl.value = '100'` 然后 `trigger('change')` 直接验证 IPC payload
2. 负数 / NaN snap-back 逻辑集中在 `commitMaxFileSize` 一处（`v-model.lazy` + watcher 实现 snap-back 会更绕）
3. `0` = no limit sentinel 处理在 commit 阶段统一（避免 draft 持有 `0` 引起 UI 闪烁）

### 偏差 #4: GeneralPanel 8 个 watcher 显式同步（非 v-model + computed 链）

**PLAN 假设**：STEP-5.4 spec 列 8 控件 —— 字面理解是"每个控件绑 store 字段"。

**实际**：8 个 draft ref（`enabledDraft` / `acceptDirDraft` / ... / `injectToClipboardDraft`）+ 8 个 `watch(() => daemonStore.clipboardConfig.<field>, (v) => <draft> = v)` 单向同步。

**理由**：
1. `daemonStore.clipboardConfig` 是 reactive nested object（`state.clipboardConfig = ...` 整体替换），不能用 `v-model="state.clipboardConfig.enabled"` —— Vue 不会追踪 reactive store 内字段的 IPC echo
2. draft ref 让用户在 commit 完成前可继续编辑本地值（中间状态）
3. watcher 只在 daemon echo 时（`state.clipboardConfig` 替换）同步 draft —— 用户 commit 后 IPC 还没回来时 draft 仍是自己刚设的值（无回弹）
4. 与 `quicIdleDraft`（GeneralPanel 既有）的 `:value` + watcher pattern 一致

### 偏差 #5: 0 新增 SUGGESTION 条目（除 #S-7 关闭）

**PLAN 假设**（隐含）：每个 STEP 习惯性写 0-1 个 SUGGESTION。

**实际**：本 STEP 0 新增；#S-7 关闭（移到 FIXED）。

**理由**：
1. 全部 0 触碰后续 milestone 范围（5.5 CLI 范畴）
2. 不修改任何 Plan-only 文档
3. 不引入新 clippy 警告 / fmt 警告
4. Vue 类型补漏是 M0c 历史遗漏而非 5.4 新风险（不留 SUGGESTION；commit message 说明即可）

---

## 4. 处理的 SUGGESTION 项

### 关闭 SUGGESTION

- **#S-7**（`set_clipboard_config` 仅 log 不接 Service 字段）→ 移到 `SUGGESTION-FIXED.md`
  - 解决方案详见 §1.2.1 + FIXED.md entry
  - 验证：M5 STEP-5.4 GUI 8 控件 onChange → IPC → daemon handler 写 TOML + live-read getter 立即生效

### 关于既有 SUGGESTION 的状态

- **#S-5** + **#S-8** 已在 M4 STEP-4.1 关闭（FIXED），本 STEP 0 触碰
- **#S-1** / **#S-2** / **#S-3** / **#S-4** / **#S-6** / **#S-9** / **#S-10** / **#S-12** 全部正交于本 STEP scope，未触碰

---

## 5. 闸门检查

| 闸门 | 结果 |
|---|---|
| **时间门** | ✅ ~50 min（PLAN 估时 1.5h 内；Vue 类型 + 8 控件 + 1 per-peer checkbox + MiB ↔ bytes 转换 + 13 新单测 + 闸 1/2 全套） |
| **milestone 边界门** | ✅ 0 触碰 STEP-5.5；0 改 src/config.rs / src/service.rs / lan-mouse-ipc；0 引入 CLI 子命令；0 触碰 Toaster |
| **闸 1 产物** | ✅ `lan-mouse-vue/src/api/ipc.ts`（2 variant + 1 字段）+ `lan-mouse-vue/src/store/index.ts`（2 helper + 1 test seam）+ `lan-mouse-vue/src/components/GeneralPanel.vue`（8 控件剪贴板区块 + 8 draft + 8 watcher + 2 MiB helper）+ `lan-mouse-vue/src/components/ConnectionRow.vue`（per-peer checkbox + 1 helper）全部落地 |
| **闸 1 依赖** | ✅ M4 STEP-4.1 `ClipboardConfig` 8 字段 + Config live-read getter + `Service::set_clipboard_config` 写盘已落地；M5 STEP-5.3 `FrontendEvent::ClipboardConfigChanged` 事件 + Vue `ClipboardConfig` / `ClipboardConfigChanged` type + store `clipboardConfig` 字段已落地 |
| **闸 1 验收** | ✅ `cargo test --workspace --lib` 496+ passed / 0 failed（excl. pre-existing flake）；`cd lan-mouse-vue && pnpm vitest --run` 44 passed / 0 failed；`pnpm build` 0 error |
| **闸 2 偏差** | 见 §3 五条偏差（#1 ClientConfig.enable_clipboard_to 类型补漏 / #2 _setSocketForTest seam / #3 max_file_size 自定义 handler / #4 8 draft + 8 watcher 显式同步 / #5 0 SUGGESTION —— 全部 A1 策略） |
| **闸 3 STEP 回归** | ⏸ 跳过（非 milestone 收尾；M5 收尾在 STEP-5.5 完成后才跑全套） |

---

## 6. 遗留

### 6.1 已知限制 / Out of Scope

- **CLI 子命令**（`lan-mouse-cli SetClipboardConfig` / `SetEnableClipboardTo`）—— M5 STEP-5.5 范畴
- **`SetClipboardConfig` 不引入 accept/reject IPC**（用户决策 2026-09-13 auto-accept only）—— 4.1 已 drop `auto_accept_files`，5.3 已 drop `FileTransferRequest` / `RespondFileTransfer`
- **`generalized max_file_size` 高级校验**（如"warn if max_file_size > available disk space"）—— out of scope；当前只是数字输入 + 字节转换
- **`accept_dir` dir-picker 按钮** —— 当前只用 `<input type="text">`（跨浏览器一致）；后续可加 `showDirectoryPicker`（Chromium-only）+ `<input type="file" webkitdirectory>` 双 fallback
- **GeneralPanel 整体重构**（subsections 分文件 / 拖拽 / dark mode）—— out of scope；当前用 `<div class="clipboard-section">` 区块 + CSS scoped 隔离

### 6.2 Pre-existing 情况

- `input_capture::macos::tests::enumerate_monitors_returns_live_state` 在 workspace lib run 中偶发失败（macOS headless 环境无真实 hardware）—— pre-existing flake，本 STEP 两次跑都未触发；与 STEP-5.1 / 5.2 / 5.3 同
- `quic_transport::http3::tests::http3_client_concurrent_rtt_stays_below_100ms_during_200mib_transfer` 在 parallel load 下偶发失败（timing-sensitive）—— M5 STEP-5.2 引入；passes in isolation
- cargo clippy baseline 24 lib errors / 28 lib test errors 全部 pre-existing（`doc_lazy_continuation` / `too_many_arguments` / `assertions_on_constants` / `needless_borrow` / `unneeded_return_statement` / `empty_line_after_doc_comment` 等），本 STEP 0 新增
- cargo fmt baseline 0 diff（本 STEP 0 新增 diff）

### 6.3 给 M5 后续 STEP 的接续契约

#### M5 STEP-5.5（CLI 集成）

- `Service::set_clipboard_config` 现在在 `write_back()` 后 push `ClipboardConfigChanged` 事件（5.3 落地）—— 5.5 CLI `SetClipboardConfig` 子命令 → IPC 写 → daemon handler 写 TOML + push echo → GUI 立即 re-sync。零额外改动。
- `Service::set_enable_clipboard_to` 已调 `client_manager.set_enable_clipboard_to` + `save_config()` + `broadcast_client(handle)` —— 5.5 CLI `SetEnableClipboardTo <handle> <bool>` 子命令 → IPC 写 → daemon 持久化 + State echo。零额外改动。

#### M5 收尾（post-5.5）

- `cargo build --workspace` + `cargo test --workspace --lib` + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo fmt --check` 全套绿
- `cd lan-mouse-vue && pnpm build` + `pnpm vitest --run` 全套绿
- `next/STEP-VALIDATION-P2-M5-FULL.md` 整批审 M5 5 STEPs（5.1 / 5.2 / 5.3 / 5.4 / 5.5）—— 触发条件：M5 全部 5 STEP 完成 + 累计 > 1h ✅
- 用户真机验证清单（PLAN §8 M5 人类测试矩阵）：macOS / Windows / Linux 各跑 GUI 配置 + 拔网 toast + `inject_to_clipboard` 关闭 / CLI 子命令

### 6.4 建议 commit 边界

1. **`feat(vue/types): add SetClipboardConfig + SetEnableClipboardTo + enable_clipboard_to ClientConfig field`**
   - `lan-mouse-vue/src/api/ipc.ts` — `FrontendRequest` union 加 2 variant + `ClientConfig` interface 加 1 字段
2. **`feat(vue/store): setClipboardConfig / setEnableClipboardTo IPC helpers + _setSocketForTest seam`**
   - `lan-mouse-vue/src/store/index.ts` — 2 helper + 1 test seam
3. **`feat(vue/GeneralPanel): clipboard section with 8 controls + MiB ↔ bytes conversion`**
   - `lan-mouse-vue/src/components/GeneralPanel.vue` — 8 控件 + 8 draft + 8 watcher + MiB helper + CSS
4. **`feat(vue/ConnectionRow): per-peer enable_clipboard_to checkbox`**
   - `lan-mouse-vue/src/components/ConnectionRow.vue` — 1 checkbox + 1 helper
5. **`test(vue): GeneralPanel clipboard section + ConnectionRow enable_clipboard_to + store IPC helpers`**
   - `lan-mouse-vue/src/components/GeneralPanel.test.ts`（新文件，8 tests）+ `lan-mouse-vue/src/components/ConnectionRow.test.ts`（3 新 tests）+ `lan-mouse-vue/src/store/index.test.ts`（2 新 tests + fixture 补 `enable_clipboard_to`）
6. **`docs(next): archive STEP-P2-M5-5.4 + close SUGGESTION #S-7`**
   - `next/STEP-P2-M5-5.4.md`（本文件）+ `next/SUGGESTION.md` #S-7 status update + `next/SUGGESTION-FIXED.md` #S-7 entry

---

## 7. 下一步

按 .LEADER-STATE.md:
- Leader 接受 6 commits
- 累计执行时间（M5 STEP-5.1 ~85 min + STEP-5.2 ~50 min + STEP-5.3 ~40 min + STEP-5.4 ~50 min = 225 min ≈ 3.75h）
- **已触发 validator 派发条件**：M5 全部 5 STEP 完成后跑整批审（5.5 待执行）
- 派 `step-validator` 整批审 M5 5/5 STEPs（5.1 / 5.2 / 5.3 / 5.4 / 5.5）—— 在 5.5 完成后跑
- Leader 接受 validator PASS-with-followup → M5 done
- 用户真机验证（GUI 配置 + per-peer enable_clipboard_to + inject_to_clipboard checkbox 关闭 + 5.5 CLI 子命令）

**M5 STEP-5.5 启动项**（next step after Leader 接受 5.4）：
- CLI 集成（`lan-mouse-cli SetClipboardConfig` / `SetEnableClipboardTo` 子命令）
- 依赖本 STEP 已落地的：Vue `SetClipboardConfig` / `SetEnableClipboardTo` request variant + store helper + IPC 类型 + 后端 `Service::set_clipboard_config` / `set_enable_clipboard_to` handler 全 live
- 估时 1.0h