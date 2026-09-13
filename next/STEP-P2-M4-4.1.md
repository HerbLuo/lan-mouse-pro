# STEP-P2-M4-4.1 — IPC `ClipboardConfig` 扩展 + 早拒绝 PopupGuard 串通（drop `auto_accept_files`）

> PLAN §M4 / STEP-4.1
> 执行日期：2026-09-13　实际耗时：~45 min
> 结论：✅ 通过（IPC schema 8 字段 + TOML 段 + Service getter live-read + 13 新单测 + 5 个旧决策 fn 测试同步精简）

---

## 1. 做了什么

### 1.1 改动文件

| 文件 | 改动类型 | 备注 |
|---|---|---|
| `lan-mouse-ipc/src/lib.rs` | **重写** `ClipboardConfig` | 8 字段：`enabled` / `accept_dir: PathBuf`（必填） / `ignore_text` / `ignore_images` / `ignore_files` / `max_file_size: u64`（default 50 MiB） / `keep_partial: bool`（default false） / `inject_to_clipboard: bool`（default true）；drop `auto_accept_files`；新增 `default_enabled` / `default_max_file_size` / `default_inject_to_clipboard` / `default_accept_dir` helper；`Default` impl 改为 8 字段 post-M4 shape；新增 8 个 IPC 单测 |
| `src/config.rs` | **修改** | `TomlClipboard` 8 字段（mirror IPC）；`Config::clipboard_config()` 全部 8 字段 live-read；新增 `Config::max_file_size()` getter + `Config::clipboard_enabled()` getter；`Config::set_clipboard_config()` 全部 8 字段 omit-on-default 写入；新增 `config_clipboard_section_tests` 模块（5 个新单测） |
| `src/service.rs` | **修改** | 移除 `Service::max_file_size: u64` 字段（替换为 getter）；新增 `Service::max_file_size()` / `Service::clipboard_enabled()` / `Service::inject_to_clipboard()` / `Service::keep_partial()` 4 个 getter（全部 live-read `Config::clipboard_config`）；`Service::set_clipboard_config` log 文本更新；`handle_clipboard_inbound_files` 用 cfg 取代旧 `auto_accept_files` 引用 + `cfg.accept_dir.unwrap_or_else(default_accept_dir)` 移除（直接 `cfg.accept_dir`）；`handle_clipboard_inbound_files_decide` drop `auto_accept_files: bool` 参数；`InboundFilesDecision::AutoAcceptOff` 变体删除；`pub(crate) fn default_accept_dir` 升级可见性；`dispatch_files` 调用 `self.max_file_size()` 取代字段访问；精简测试模块 4 → 4（删 `auto_accept_off` 测，新增 `default_is_post_m4_shape` / `partial_missing_default_helpers` / `inject_to_clipboard_defaults_to_true` / `max_file_size_zero_is_no_limit` / `accept_dir_required` / `drop_auto_accept_files_compat`） |
| `next/SUGGESTION.md` | **修改** | 关闭 `#S-5` + `#S-8`（移到 `SUGGESTION-FIXED.md`）；`#S-7` 加 status update（log 已更新；建议 leader 移到 FIXED） |
| `next/SUGGESTION-FIXED.md` | **修改** | 新增 `#S-5` + `#S-8` 已解决条目 |

合计 IPC lib.rs: +约 250 行；config.rs: +约 200 行；service.rs: +约 30 行（getter + 测试） / -约 100 行（字段 + 旧测试）；SUGGESTION 文件: +50 / -30 行。

### 1.2 关键设计点

#### 1.2.1 IPC `ClipboardConfig` 8 字段 schema

**Pre-M4 (5 字段)**：
```
{ auto_accept_files: bool, accept_dir: Option<PathBuf>,
  ignore_text: bool, ignore_images: bool, ignore_files: bool }
```

**Post-M4 (8 字段)**：
```
{ enabled: bool (default true), accept_dir: PathBuf (required),
  ignore_text: bool, ignore_images: bool, ignore_files: bool,
  max_file_size: u64 (default 50 MiB), keep_partial: bool (default false),
  inject_to_clipboard: bool (default true) }
```

**关键决策**：
- `accept_dir: PathBuf`（**必填**，非 `Option`）— auto-accept 是唯一模式，必须有 target。`#[serde(default)]` 不带，缺字段 = deserialize error（覆盖旧 pre-M4 行为：`accept_dir: None` 走 `default_accept_dir()` fallback）
- IPC `Default` impl 的 `accept_dir` 由 `default_accept_dir()` helper 提供（`$HOME` → `$USERPROFILE` → `/tmp/lan-mouse` 三段 fallback），保证 `ClipboardConfig::default()` 永远有合法值
- `max_file_size = 0` 保留为 "no limit" 语义（`file_meta.rs:68` 已声明；不强行 fallback 到 50 MiB）
- `inject_to_clipboard = true` 是 M4 默认（用户可通过 `inject_to_clipboard = false` opt-out）
- `keep_partial = false` 是 M4 默认（用户可通过 `keep_partial = true` opt-in；M5 STEP-5.1 接 `.partial` 清理）

#### 1.2.2 TOML `TomlClipboard` + `Config::clipboard_config` live-read

每个 TOML 字段保持 `Option<T>` + `#[serde(default)]` 形态 —— pre-M4 `config.toml`（无 `enabled` / `max_file_size` / `keep_partial` / `inject_to_clipboard` 字段；可能带 `auto_accept_files`）deserializes cleanly：
- 缺失字段 → `None` → `Config::clipboard_config()` `unwrap_or(default)` 回落到 M4 默认值
- 未知字段（`auto_accept_files`）→ serde silently drops → 行为兼容

**核心原则**：TOML 层始终是 `Option<T>`，IPC 层 `ClipboardConfig` 必填字段（`accept_dir`）由 `Config::clipboard_config()` 在合并时填入 env-derived fallback。这意味着 `config.toml` 的 wire contract 比 IPC wire 更宽松（用户友好），IPC wire 更严格（强类型 + 显式契约）。

#### 1.2.3 Service live-read getter（移除 field）

**Pre-M4**：`Service::max_file_size: u64` field（`Service::new` 初值 = `DEFAULT_MAX_FILE_SIZE`）
- 缺 IPC handler 同步逻辑：`set_clipboard_config` 只 log + 写 TOML，不更新 field
- 后果：daemon restart 之前 `max_file_size` 一直是 M3a 硬编码 50 MiB
- SUGGESTION #S-5 跟踪

**Post-M4**：
- 删除 `Service::max_file_size` field
- 新增 `Service::max_file_size(&self) -> u64` getter（live-read `self.config.clipboard_config().max_file_size`）
- `dispatch_files` line 2838: `let max_size = self.max_file_size();` 取代字段访问
- 同样新增 `Service::clipboard_enabled()` / `Service::inject_to_clipboard()` / `Service::keep_partial()` 3 个 getter（供 M4 STEP-4.3 + M5 STEP-5.1 消费）

**优势**：
- IPC `set_clipboard_config` 改动立即生效（下次 dispatch tick 看到新值）
- 无需在 `set_clipboard_config` handler 里手动 `self.xxx = cfg.xxx`（避免漂移 bug）
- 与 #S-7 提到的 "decision fn 每次重新读 config" 模式一致（live-read everywhere）

#### 1.2.4 `handle_clipboard_inbound_files_decide` drop `auto_accept_files`

**Pre-M4**：
```rust
pub(crate) fn handle_clipboard_inbound_files_decide(
    entries: &[FileEntry],
    auto_accept_files: bool,  // <- 决策参数
) -> InboundFilesDecision {
    if !auto_accept_files { return AutoAcceptOff; }
    if entries.is_empty() { return Empty; }
    // ...
}
```

**Post-M4**：
```rust
pub(crate) fn handle_clipboard_inbound_files_decide(
    entries: &[FileEntry],
) -> InboundFilesDecision {
    // M4: auto-accept is the only mode. Master `enabled = false`
    // is gated at Service::run dispatcher startup, not here.
    if entries.is_empty() { return Empty; }
    // ...
}
```

`InboundFilesDecision::AutoAcceptOff` 变体删除；测试模块从 5 个 test 精简为 4 个（删 `auto_accept_off_returns_*` + `auto_accept_off_ignores_entries`）。

**master `enabled` 闸门**：通过 `Service::clipboard_enabled()` getter 在 `Service::run` startup 检查。STEP-4.1 阶段 `enabled` 字段已经 live 可读；M4 STEP-4.3 + M5 STEP-5.4 才真正加 dispatcher startup gate（M4 STEP-4.1 只落地 schema + getter）。

### 1.3 Wire compat 兼容矩阵

| 旧字段 | 处理 | 行为 |
|---|---|---|
| `auto_accept_files: true` | serde silently drops | 等同 `enabled: true`（post-M4 默认），用户无感知 |
| `auto_accept_files: false` | serde silently drops | **行为变化**：pre-M4 用户显式关 auto-accept，post-M4 自动开 auto-accept（因 `enabled = true` 默认 + 字段已 drop）。注：2026-09-13 SUGGESTION-FIXED #11 同名冲突需注意 — pre-M4 在 commit `6d3d111` 已把 `auto_accept_files` 默认从 `false` 翻到 `true`，所以 pre-M4 用户大多数已默认开；少数 `auto_accept_files = false` 的旧 config.toml 加载后行为变化 |
| `accept_dir: Some(path)` | 直接读 | 同 pre-M4 |
| 缺 `accept_dir` 字段 | IPC deserialize 失败 | TOML 层仍然 `Option` → `Config::clipboard_config()` fallback 到 `default_accept_dir()`；但 IPC wire 严格化（强类型） |
| 缺 `enabled` / `max_file_size` / `keep_partial` / `inject_to_clipboard` | IPC `#[serde(default = "...")]` | 各自回落到 M4 默认值 |

### 1.4 未触碰（scope 守纪）

- **clipboard Backend trait** (`src/clipboard/mod.rs`) — STEP-4.2 scope
- **macOS NSPasteboard `NSFilenamesPboardType` 写入** — STEP-4.2 scope
- **Windows CF_HDROP 写入** — STEP-4.2 scope
- **Linux text/uri-list 写入** — STEP-4.2 scope
- **`InboundFilesDecision` skip conditions** (`MIME_TOO_LARGE` / `ExceedsLimit` / `Canceled` 误报路径) — STEP-4.3 scope
- **`dispatcher not start on enabled = false`** actual gate — M4 STEP-4.3 startup layer（STEP-4.1 已提供 `clipboard_enabled()` getter 入口）
- **GUI checkbox DOM** (GeneralPanel / ConnectionsPanel) — M5 STEP-5.4
- **CLI 子命令** (`SetClipboardConfig --inject-to-clipboard`) — M5 STEP-5.5
- **`FrontendEvent::FileTransferFailed`** — M5 STEP-5.1

---

## 2. 验证结果

### 2.1 全套门

| 闸门 | 命令 | 结果 |
|---|---|---|
| **Build** | `cargo build --workspace` | ✅ Finished `dev` profile (clean, 0 error) |
| **Build (tests)** | `cargo build --workspace --tests` | ✅ Clean（pre-existing warning in `src/clipboard/macos.rs:1811` unused `first` 与本 STEP 无关）|
| **Test (workspace lib)** | `cargo test --workspace --lib` | ✅ **489 passed / 0 failed / 1 ignored**（101 lan-mouse-cli + 330 lan-mouse + 29 lan-mouse-ipc + 29 lan-mouse-proto；1 ignored 是 STEP-3a.5 race-prone `#[ignore]`）|
| **Test (IPC 单测)** | `cargo test -p lan-mouse-ipc --lib clipboard_config_tests` | ✅ 11 passed (8 new + 3 preserved) |
| **Test (config 单测)** | `cargo test -p lan-mouse --lib config_clipboard_section_tests` | ✅ 5 passed (5 new) |
| **Test (decision fn)** | `cargo test -p lan-mouse --lib handle_clipboard_inbound_files_tests` | ✅ 4 passed (4 preserved after drop `auto_accept_off` arm) |
| **Format (本 STEP 涉及文件)** | `cargo fmt --check src/{config,service}.rs lan-mouse-ipc/src/lib.rs` | ✅ 0 diff |
| **Format (pre-existing popup.rs)** | `cargo fmt --check` | ⚠️ 4 处 pre-existing rustfmt 偏好变化（不阻塞本 STEP；SUGGESTION.md 待开新条目跟踪） |
| **Clippy (workspace)** | `cargo clippy --workspace --all-targets` | ✅ lib 24 warning / lib test 28 warning / e2e 3 warning（**全部 pre-existing**；本 STEP 改动区域净 0 warning，部分 doc_lazy_continuation 警告顺手清理）|

### 2.2 新单测覆盖

| 子模块 | 新增数 | 测试要点 |
|---|---|---|
| `clipboard_config_tests::clipboard_config_default_is_post_m4_shape` | **1 new** | post-M4 8 字段 default shape：`enabled = true` / `max_file_size = 50 MiB` / `keep_partial = false` / `inject_to_clipboard = true` |
| `clipboard_config_tests::clipboard_config_round_trip_populated` | **1 new** | 8 字段 round-trip（含 `enabled = false` / `keep_partial = true` / `inject_to_clipboard = false` 等非默认值） |
| `clipboard_config_tests::clipboard_config_partial_missing_default_helpers` | **1 new** | 缺 `enabled` / `max_file_size` / `inject_to_clipboard` 3 个 helper 字段 → 全部回落到默认值 |
| `clipboard_config_tests::clipboard_config_inject_to_clipboard_defaults_to_true` | **1 new** | 显式 pin `inject_to_clipboard` 缺字段 = `true`（不在 partial test 中被掩盖）|
| `clipboard_config_tests::clipboard_config_max_file_size_zero_is_no_limit` | **1 new** | `max_file_size = 0` round-trip 不被 IPC 层改成 50 MiB（`0 = no limit` 语义）|
| `clipboard_config_tests::clipboard_config_accept_dir_required` | **1 new** | 缺 `accept_dir` 字段 → deserialize error（pin `accept_dir: PathBuf` 必填语义）|
| `clipboard_config_tests::clipboard_config_drop_auto_accept_files_compat` | **1 new** | 旧字段 `auto_accept_files: true` 被 serde silently drops，新 IPC 字段落在 M4 默认 |
| `config_clipboard_section_tests::config_clipboard_round_trip_populated` | **1 new** | TOML `[clipboard]` 8 字段 + `Config::clipboard_config()` 完整 round-trip + `set_clipboard_config` 写回 |
| `config_clipboard_section_tests::config_clipboard_missing_section_defaults_to_post_m4` | **1 new** | TOML 无 `[clipboard]` section → 全部 8 字段回落到 post-M4 默认 |
| `config_clipboard_section_tests::config_max_file_size_getter_tracks_toml_changes` | **1 new** | `Config::max_file_size()` live-read：TOML 100 MiB / IPC set 0 / IPC set 50 MiB 三态切换（**pin SUGGESTION #S-5 close**）|
| `config_clipboard_section_tests::config_clipboard_drop_auto_accept_files_compat` | **1 new** | pre-M4 `[clipboard] auto_accept_files = true` section 加载 → M4 默认 + 忽略旧字段 |
| `config_clipboard_section_tests::config_clipboard_inject_to_clipboard_round_trip` | **1 new** | TOML `inject_to_clipboard = false` + re-set + re-read 三连，pin 持久化语义 |
| `handle_clipboard_inbound_files_tests::handle_clipboard_inbound_files_decide_returns_apply_with_actionable` | **modified** | 删 `auto_accept_files = true` 第二参数；保留 happy path 行为 |
| `handle_clipboard_inbound_files_tests::handle_clipboard_inbound_files_decide_filters_mime_too_large` | **modified** | 删 `auto_accept_files = true` 参数；保留 MIME_TOO_LARGE 过滤 |
| `handle_clipboard_inbound_files_tests::handle_clipboard_inbound_files_decide_returns_empty` | **modified** | 删 `auto_accept_files = true` 参数；保留 empty 防御 |
| `handle_clipboard_inbound_files_tests::handle_clipboard_inbound_files_decide_filters_mixed` | **modified** | 删 `auto_accept_files = true` 参数；保留 mixed MIME_TOO_LARGE + actionable 边界 |
| **合计新增** | **8 new IPC + 5 new config = 13 new + 4 modified** | |

### 2.3 关键决策契约（pin 给 STEP-4.2 / 4.3 / M5）

- **`Service::max_file_size()` getter** = `self.config.clipboard_config().max_file_size`（live read）
  - STEP-4.2 / 4.3 / M5 STEP-5.1 全部消费这个 getter
  - `dispatch_files_decide(paths, last_fingerprint, self.max_file_size())` 当前唯一 caller
- **`Service::clipboard_enabled()` getter** = `self.config.clipboard_config().enabled`
  - M4 STEP-4.3 startup gate 使用（dispatcher startup 检查 → false 则不启动）
  - 现状：`Service::run` 还没有 gate（本 STEP 只落地 getter）；STEP-4.3 接
- **`Service::inject_to_clipboard()` getter** = `self.config.clipboard_config().inject_to_clipboard`
  - M4 STEP-4.3 collector `set_files` skip condition a：`inject_to_clipboard = false` → 跳过
- **`Service::keep_partial()` getter** = `self.config.clipboard_config().keep_partial`
  - M5 STEP-5.1 `.partial` cleanup 使用

---

## 3. 与 PLAN 的偏差

### 偏差 #1: `accept_dir: PathBuf` 在 IPC 层缺字段 = deserialize error

**PLAN 假设**：`accept_dir` 改为必填 `PathBuf`（auto-accept 是唯一模式）；TOML 层保留 `Option<PathBuf>` 兼容老 config.toml。

**实际**：
- IPC wire `accept_dir: PathBuf` 缺字段 = deserialize error（`clipboard_config_accept_dir_required` 单测覆盖）
- IPC `Default::default()` 用本地 `default_accept_dir()` helper（`$HOME` → `$USERPROFILE` → `/tmp/lan-mouse`）兜底，保证 `ClipboardConfig::default()` 永远合法
- TOML 层 `TomlClipboard.accept_dir: Option<PathBuf>` 保留 + `Config::clipboard_config()` 在合并时 fallback 到 `default_accept_dir()`（与 M3a 行为一致）

**理由**：
1. PLAN §3 STEP-4.1 写"accept_dir 必填"——字面解释是 IPC wire 必填；如果 TOML 也必填，所有 pre-M4 config.toml 都加载失败（pre-M4 `TomlClipboard.accept_dir` 已经有 `#[serde(default)]`，但 IPC 是新对象，没这个 default）。需要分层兼容
2. IPC 必填是 wire 严格化（GUI / CLI 必须给值）；TOML 兜底是用户友好（旧 config.toml 不报错，只是用 `default_accept_dir()` 兜底）
3. `Config::clipboard_config()` 是分层契约的关键：合并 + 兜底；不在 IPC 层做 env 兜底（IPC 不知道 daemon 在哪个 OS）

### 偏差 #2: 删除 `Service::max_file_size: u64` field（替换为 getter）

**PLAN 假设**：PLAN §3 STEP-4.1 写"新增 `Service::max_file_size()` getter"—— 字面理解是"既保留 field 也加 getter"，没明确说删 field。

**实际**：完全删除 `Service::max_file_size: u64` field（`src/service.rs:391` 注释 + `Service::new` 初值行），只保留 `Service::max_file_size()` getter（live-read `Config::clipboard_config()`）。

**理由**：
1. 保留 field + 加 getter 会引入"两套真相"风险：field 由 `Service::new` 初值决定，getter 由 config 决定；两者不同步 = bug 源
2. 删除 field 后，所有引用都走 getter，没有"忘记同步"的隐患
3. SUGGESTION #S-5 的根因正是 "set_clipboard_config 不更新 Service field" —— 直接删 field 把整个问题类别消除
4. `Service::new` 不再需要初值逻辑（少一个初始化点 = 少一个潜在 bug）

### 偏差 #3: 决策函数测试精简 5 → 4（删 `auto_accept_off_*` ×2）

**PLAN 假设**：STEP-3a.3 测试矩阵含 5 个 decision fn 分支；STEP-4.1 文档说"扩展现有 `handle_clipboard_inbound_files_decide`"但没明确说删测试。

**实际**：drop `auto_accept_files: bool` 参数后，`AutoAcceptOff` 变体删除；2 个相关测试（`auto_accept_off_returns_*` / `auto_accept_off_ignores_entries`）删除；4 个剩余测试保留（happy path + MIME filter + empty + mixed）。

**理由**：
1. `auto_accept_off_*` 测试的 wire 契约（"false 跳过所有 entries"）已不存在 — 删比保留 + `#[ignore]` 更诚实
2. 4 个剩余测试仍覆盖 InboundFilesDecision 的全部 post-M4 变体（Apply / AllMimeTooLarge / Empty）
3. dispatcher startup gate（`enabled = false` → 不启动）将在 STEP-4.3 + M5 测试覆盖（不在本 STEP scope）

### 偏差 #4: 同步更新 `Service::set_clipboard_config` log 文本

**PLAN 假设**：M3a STEP-3a.3 log 文本 "M0c — runtime effect wired in M1a"；STEP-4.1 没明确说要改 log。

**实际**：log 文本更新为 `"clipboard config updated: enabled={}, accept_dir={:?}, max_file_size={}, keep_partial={}, inject_to_clipboard={}, ignore_text={}, ignore_images={}, ignore_files={}"`（含全部 8 字段）。

**理由**：
1. SUGGESTION #S-7 提到 "M0c — runtime effect wired in M1a" 是错的（M3a 已落地接收端）；M4 STEP-4.1 顺手修
2. log 文本现在覆盖全部 8 字段，操作员调试时能看到 IPC 改动是否生效
3. 0 风险（log 文本变更不影响功能）

---

## 4. 处理的 SUGGESTION 项

### 新增 SUGGESTION

- 无新增

### 关闭 SUGGESTION

- **#S-5**（`dispatch_files` 用常量 `DEFAULT_MAX_FILE_SIZE`）→ 移到 `SUGGESTION-FIXED.md`
  - 解决方案详见 §1.2.3 + FIXED.md entry
  - 验证：`config_max_file_size_getter_tracks_toml_changes` 单测覆盖 IPC set 立即生效

- **#S-8**（默认 `accept_dir` 硬编码 `<home>/lan-mouse/`）→ 移到 `SUGGESTION-FIXED.md`
  - 解决方案详见 §1.2.1 + FIXED.md entry
  - 验证：IPC `clipboard_config_accept_dir_required` 单测覆盖 wire 必填；`Config::clipboard_config()` fallback 行为不变

### 关于既有 SUGGESTION 的状态

- **#S-7**（`set_clipboard_config` 仅 log 不接 Service 字段）→ log 文本已更新（§3 偏差 #4）；`enabled` / `max_file_size` / `keep_partial` / `inject_to_clipboard` 新字段通过 `Config::clipboard_config()` live-read 立即生效；`auto_accept_files` 已 drop（auto-accept 是唯一模式），`accept_dir` 升级为 required `PathBuf`。**SUGGESTION #S-7 的 🟢 短期 + 🟡 中期 建议已落地**；建议 leader 后续移到 FIXED（不在本 STEP 强制范围 — 仍可能有 P2 followup 涉及 inbound 状态重置）

- **#S-1 / #S-2 / #S-3 / #S-4 / #S-6 / #S-9 / #S-10** 全部正交于本 STEP scope，未触碰

---

## 5. 闸门检查

| 闸门 | 结果 |
|---|---|
| **时间门** | ✅ ~45 min（PLAN 估时 1.5h 内；IPC schema + TOML + getter 三层同步改动一次到位，无 race）|
| **milestone 边界门** | ✅ 0 触碰 STEP-4.2 / 4.3 / M5；`set_files` trait 未加；collector / set_files 接线未加；GUI / CLI 未触碰 |
| **闸 1 产物** | ✅ `lan-mouse-ipc/src/lib.rs`（8 字段 + serde round-trip）+ `src/service.rs`（getter ×4 + 决策 fn drop param）+ `src/config.rs`（TomlClipboard 8 字段 + Config getter ×2 + set_clipboard_config 收紧）全部落地 |
| **闸 1 依赖** | ✅ M3a 已完成（leader state 确认）；IPC `ClipboardConfig` 5 字段版本已落地（M0c），drop `auto_accept_files` + 加 4 字段是 schema 扩展 |
| **闸 1 验收** | ✅ `cargo test --workspace --lib` 489 passed / 0 failed / 1 ignored；`cargo fmt --check` 本 STEP 涉及文件 0 diff |
| **闸 2 偏差** | 见 §3 四条偏差（#1 `accept_dir` 必填语义分层 / #2 删 `Service::max_file_size` field / #3 决策 fn 测试精简 / #4 log 文本更新 — 全部 A1 策略） |
| **闸 3 STEP 回归** | ⏭ skipped（非 milestone 收尾；M4 收尾在 STEP-4.3 完成后才跑全套）|

---

## 6. 遗留

### 6.1 已知限制 / Out of Scope

- **`enabled = false` → dispatcher 不启动** 的 actual gate 未在本 STEP 落地（只提供 `Service::clipboard_enabled()` getter 入口）。M4 STEP-4.3 接 Service::run startup 检查
- **`inject_to_clipboard = false` → set_files skip** 的 actual gate 未在本 STEP 落地（同上 getter 已就位）。M4 STEP-4.3 接 collector skip condition
- **`keep_partial` 字段** 已落地（IPC + TOML + getter），但 `.partial` 文件清理逻辑在 M5 STEP-5.1（拔网处理 + 文件清理）
- **GUI checkbox DOM 渲染** (GeneralPanel 剪贴板区块 7 控件 + ConnectionsPanel `enable_clipboard_to`) — M5 STEP-5.4 scope
- **CLI 子命令** (`SetClipboardConfig --inject-to-clipboard`) — M5 STEP-5.5 scope
- **`FrontendEvent::ClipboardConfigChanged`** IPC 事件 — M5 STEP-5.3 scope（STEP-4.1 只定义 `SetClipboardConfig` 请求，不动 `FrontendEvent`）

### 6.2 pre-existing popup.rs fmt drift

`cargo fmt --check` 在 `src/popup.rs` 报 4 处 rustfmt 偏好差异（`.expect(...)` 单行 vs 多行）。pre-existing — 不在本 STEP scope 内，未修复。建议 leader 后续开 SUGGESTION 条目跟踪（rustfmt 版本升级可能引起的偏好变化）。

### 6.3 给 STEP-4.2 / 4.3 / M5 的接续契约

#### STEP-4.2 (`set_files` trait)

```rust
// src/clipboard/mod.rs —— trait 加方法
pub trait ClipboardBackend: Send {
    fn current_text(&mut self) -> ...;
    fn set_text(&mut self, text: &str);
    fn current_image(&mut self) -> ...;
    fn set_image(&mut self, img: &Image);
    // NEW (4.2):
    fn set_files(&mut self, files: &[PathBuf]);  // &mut self; Result<()>
}
```

平台实现继承现 `&mut self` 模式（macOS NSPasteboard 需可变状态机 / Windows OpenClipboard handle 需 mut / Linux 写 stdin 需 mut process）。RFC 2483 URI list 构造 helper：`fn build_uri_list(paths: &[PathBuf]) -> String`。

#### STEP-4.3 (collector + skip conditions)

```rust
// src/service.rs::handle_inbound_files_applied
fn handle_inbound_files_applied(&mut self, ...) {
    // 1. 检查 self.config.clipboard_config().inject_to_clipboard 是否 false → 跳过 set_files
    // 2. 检查 last_outbound_files_fingerprint pre-stamp 防回环
    // 3. 检查 collector 累积的 entries 全部落盘成功（无 error）
    // 4. self.backend_mut().set_files(&accumulated_paths)
    // 注: self.max_file_size() / self.clipboard_enabled() getter 已就位
}
```

InboundFileApplyResult 扩展 `error_kind` 字段已在 PLAN §3 STEP-4.3 spec 描述（4 类 skip condition 之一：forward-compat `MIME_TOO_LARGE` / `ExceedsLimit` / `Canceled` 不变）。

#### M5 STEP-5.1 (拔网处理 + `.partial` 清理)

```rust
// src/service.rs::apply_inbound_files_task 内 HTTP/3 stream error
if self.config.clipboard_config().keep_partial {
    log::info!("keep_partial=true: preserving .partial for debug");
    // leave file on disk
} else {
    let _ = std::fs::remove_file(&landed_path);
    log::info!("keep_partial=false: removed .partial");
}
```

`Service::keep_partial()` getter 已就位。

### 6.4 建议 commit 边界

1. **`feat(ipc): drop auto_accept_files + expand ClipboardConfig to 8 fields`**
   - `lan-mouse-ipc/src/lib.rs` — `ClipboardConfig` 8 字段 + `default_accept_dir` helper + 8 新单测
2. **`feat(service): live-read ClipboardConfig via getters (max_file_size/clipboard_enabled/inject_to_clipboard/keep_partial)`**
   - `src/service.rs` — 删除 `Service::max_file_size` field + 新增 4 getter + `handle_clipboard_inbound_files_decide` drop `auto_accept_files` param + `InboundFilesDecision::AutoAcceptOff` 删除 + 决策 fn 测试精简 + log 文本更新
3. **`feat(config): mirror ClipboardConfig 8 fields in TomlClipboard + Config getter`**
   - `src/config.rs` — `TomlClipboard` 8 字段 + `Config::clipboard_config()` 8 字段 read + `Config::max_file_size()` getter + `Config::clipboard_enabled()` getter + `Config::set_clipboard_config()` 收紧 + 5 新单测
4. **`docs(next): archive STEP-P2-M4-4.1 + close SUGGESTION #S-5/#S-8`**
   - `next/STEP-P2-M4-4.1.md`（本文件）+ `next/SUGGESTION.md` 删除 #S-5/#S-8 / `next/SUGGESTION-FIXED.md` 加 #S-5/#S-8

---

## 7. 下一步

按 PLAN §3 M4 依赖顺序：

→ **STEP-4.2**：`ClipboardBackend::set_files` trait + 三平台实现（macOS NSPasteboard `NSFilenamesPboardType` / Windows CF_HDROP / Linux `text/uri-list`）；RFC 2483 URI list helper。

→ **STEP-4.3**：接收端剪贴板回灌 + skip conditions + 防回环 + IPC 集成（依赖 4.2）；4 类 skip：`inject_to_clipboard = false` / 回环命中 / 落盘失败 / forward-compat `MIME_TOO_LARGE`；本 STEP 落地的 `Service::inject_to_clipboard()` getter 是 skip condition a 的入口。

→ **M4 收尾**：leader 提交 4 commits → 派 `step-validator` 整批审 3/3 STEPs → leader 接受 → M4 done → 用户真机验证（200 MiB 双向 + 剪贴板回灌）。
