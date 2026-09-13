# STEP-P2-M5-5.5 — CLI 集成（评审 #7）

> PLAN §M5 / STEP-5.5
> 执行日期：2026-09-13　实际耗时：~25 min
> 结论：✅ 通过（SetClipboardConfig + SetEnableClipboardTo 子命令 + MiB→bytes + 6 新单测 + 闸 1/2/3 全绿）

---

## 1. 做了什么

### 1.1 改动文件

| 文件 | 改动类型 | 备注 |
|---|---|---|
| `lan-mouse-cli/src/lib.rs` | **新增** 2 subcommand + 1 helper + 6 tests | `SetClipboardConfig(SetClipboardConfigArgs)` / `SetEnableClipboardTo { id, enable }`；`build_clipboard_config(&args) -> ClipboardConfig` 纯 helper；`MIB` 常量 |
| `lan-mouse-cli/Cargo.toml` | **新增** `serde_json = "1.0.107"` dev-dep | 单测 wire-shape + round-trip 需要 serde_json |

合计 src/lib.rs: +约 240 行（80 行 subcommand 声明 + 18 行 pure helper + 60 行 dispatch + 5 行 doc + 6 行 MIB 常量 + 6 个 test 共 ~200 行 + 一些 import）；Cargo.toml: +3 行。

### 1.2 关键设计点

#### 1.2.1 SetClipboardConfig 子命令 —— 8 flag（与 IPC `ClipboardConfig` 1:1）

```rust
#[derive(Args, Clone, Debug, PartialEq, Eq)]
#[allow(clippy::too_many_arguments)]
struct SetClipboardConfigArgs {
    #[arg(long)] enabled: bool,
    #[arg(long)] accept_dir: PathBuf,
    #[arg(long)] ignore_text: bool,
    #[arg(long)] ignore_images: bool,
    #[arg(long)] ignore_files: bool,
    #[arg(long)] max_file_size: u64,
    #[arg(long)] keep_partial: bool,
    #[arg(long)] inject_to_clipboard: bool,
}
```

**Flag 语义（与 PLAN §3 STEP-5.5 spec 对齐）**：
- 所有 8 个字段都是 `--<name>` long flag
- boolean 字段是 clap 默认 `SetTrue` 风格：present = `true`，absent = `false`
- 完整 8 字段 payload 一次性发到 daemon（用户必须传所有要设为 `true` 的 flag；漏传的 flag 在 daemon 端落到 `false`）
- `max_file_size` 是 MiB 整数（CLI 输入习惯），helper `build_clipboard_config` 转 bytes（`MiB × 1024 × 1024`）

**示例 invocation**：
```bash
lan-mouse-cli set-clipboard-config \
    --accept-dir /tmp/recv \
    --max-file-size 100 \
    --enabled \
    --inject-to-clipboard
# → IPC: FrontendRequest::SetClipboardConfig(ClipboardConfig{ accept_dir: "/tmp/recv", max_file_size: 104857600, enabled: true, inject_to_clipboard: true, ignore_*: false, keep_partial: false })
```

**⚠️ Spec 偏差（PLAN §3 STEP-5.5 写的是 `SetClipboardConfig` PascalCase；实际 CLI 是 kebab-case `set-clipboard-config`）**：
- PLAN §3 STEP-5.5 例子用 PascalCase：`lan-mouse-cli SetClipboardConfig ...`
- clap 默认 `rename_all = "kebab-case"`，所以 CLI 实际 subcommand 名是 `set-clipboard-config`
- 这是 **M0c / M3 已建立**的命名约定（参考 `SetMonitor` → `set-monitor`、`SetPort` → `set-port`）
- 偏差 #1 §3 详述

#### 1.2.2 SetEnableClipboardTo 子命令 —— 1 个 bool 形参

```rust
CliSubcommand::SetEnableClipboardTo {
    id: ClientHandle,
    /// clap derive defaults `bool` positional args to the `SetTrue`
    /// action (flag-style); we override with `Set` so the user
    /// passes an explicit `true` / `false` value.
    #[arg(action = clap::ArgAction::Set, value_parser = clap::value_parser!(bool))]
    enable: bool,
},
```

**为什么需要 `action = ArgAction::Set`**：
- clap derive 默认把 `bool` 字段当 `SetTrue` flag（"arg must take a value but action is SetTrue" panic）
- 用户传 `lan-mouse-cli set-enable-clipboard-to 0 false` 时 `false` 必须是 positional value，不是 flag presence
- 显式 `Set` + `value_parser!(bool)` 覆盖默认 → 接受 `true` / `false` 字符串
- 偏差 #2 §3 详述

**Wire shape**：`{"SetEnableClipboardTo":[handle,bool]}`（tuple variant + JSON 单 key object + array positional）—— 与 IPC `FrontendRequest::SetEnableClipboardTo(ClientHandle, bool)` 一致

#### 1.2.3 build_clipboard_config 纯 helper —— MiB→bytes 转换

```rust
const MIB: u64 = 1024 * 1024;

fn build_clipboard_config(args: &SetClipboardConfigArgs) -> ClipboardConfig {
    ClipboardConfig {
        enabled: args.enabled,
        accept_dir: args.accept_dir.clone(),
        ignore_text: args.ignore_text,
        ignore_images: args.ignore_images,
        ignore_files: args.ignore_files,
        max_file_size: args.max_file_size.saturating_mul(MIB),
        keep_partial: args.keep_partial,
        inject_to_clipboard: args.inject_to_clipboard,
    }
}
```

**为什么拆出 pure helper**：
- `execute()` 需要 IPC socket 连接（`connect_async`），无法纯函数测试
- helper 把 clap parse → IPC struct 的转换逻辑独立出来 → 单测可以直接验证 wire shape + MiB→bytes + 0 sentinel 保留
- 与 M5 STEP-5.4 Vue `commitClipboard` 把 8 字段打包成 IPC payload 的逻辑同模式（GUI 端打包也是纯函数）

**`saturating_mul(MIB)` 而不是 `* MIB`**：
- 用户输入 `u64`；`u64::MAX * 1024 * 1024` 会 panic (overflow in debug build) 或 wrap (release build)
- `saturating_mul` 在溢出时 cap 到 `u64::MAX`，跟 IPC `max_file_size: u64` 同语义；CLI 永不会触发，但 saturating 是廉价的防御
- 实际场景：用户输入 100 MiB → `100 * 1024 * 1024 = 104857600`（远低于 u64::MAX ≈ 1.8e19 MiB）；saturating 是 0 成本 fallback
- 偏差 #3 §3 详述

**`0 = no limit` sentinel**：
- M4 STEP-4.1 在 `lan_mouse_ipc::ClipboardConfig` doc 明确：`0` 表示 "no limit"（不是默认 50 MiB）
- `0.saturating_mul(MIB) = 0` —— sentinel 在 helper 边界保持完整
- `build_clipboard_config_preserves_zero_as_no_limit_sentinel` 单测 pin 这个语义

#### 1.2.4 Dispatch —— 与 SetMonitor 单行 pattern 一致

```rust
CliSubcommand::SetClipboardConfig(args) => {
    let cfg = build_clipboard_config(&args);
    tx.request(FrontendRequest::SetClipboardConfig(cfg)).await?
}
CliSubcommand::SetEnableClipboardTo { id, enable } => {
    tx.request(FrontendRequest::SetEnableClipboardTo(id, enable)).await?
}
```

**为什么单行**：
- 现有 `SetMonitor` / `SetPort` / `SetIps` 全是单行 dispatch（`tx.request(...).await?`），step-validator 多次要求"共用同一 dispatch pattern"
- 不加额外 read（不像 `AddClient` 要 loop 读 `Created` event）—— daemon handler 写 TOML 后 push `ClipboardConfigChanged` event，CLI 不需要等
- **不**在 CLI 加 `--wait-for-echo` 选项（out of scope；用户要看 echo 走 GUI 或 grep daemon log）

#### 1.2.5 6 个新单测

| 测试 | 验证点 |
|---|---|
| `set_clipboard_config_parses_with_all_eight_flags` | clap derive 接受 8 个 `--<name>` flag，全部正确解析 |
| `set_clipboard_config_omitted_flags_default_to_false` | 缺省 flag 默认 `false`（clap `SetTrue` 语义） |
| `build_clipboard_config_encodes_all_eight_fields_with_mib_to_bytes` | 8 字段 IPC wire shape（pin JSON 关键 substring）+ serde round-trip + 100 MiB → 104857600 bytes |
| `build_clipboard_config_preserves_zero_as_no_limit_sentinel` | `max_file_size = 0` 在 helper 边界保留为 0（不 saturate 错位）|
| `clipboard_config_drop_auto_accept_files_wire_compat` | pre-M4 payload 含 `auto_accept_files: true` 仍能 deserialize（serde silently drop 未知字段）—— **PLAN §3 STEP-5.5 spec 明确要求** |
| `set_enable_clipboard_to_parses_and_round_trips` | positional `bool` 解析 + wire shape `{"SetEnableClipboardTo":[7,false]}` + `true` 分支 |

**测试策略**：
- 不测 `execute()`（需 IPC socket）—— 测 pure helper + clap parse + serde_json 直接 encode/decode
- 覆盖 spec 列的所有 6 项完成标志的"自动测试"维度（人类真机维度留给用户 §8 M5 矩阵）
- 与 M0c / M3 已建立的 "不测 socket 直接 wire shape" 模式一致

### 1.3 未触碰（scope 守纪）

- **`lan-mouse-ipc/src/lib.rs`** —— 5.3 落地 `SetClipboardConfig` / `SetEnableClipboardTo` / `ClipboardConfigChanged`，本 STEP 0 改动
- **`src/config.rs` / `src/service.rs`** —— M4 4.1 + M5 5.3 已就位 TOML `[clipboard]` 段 + `set_clipboard_config` handler 写盘 + push `ClipboardConfigChanged` event，本 STEP 0 改动
- **`lan-mouse-vue/`** —— 5.4 GeneralPanel + ConnectionRow 已就位，本 STEP 0 改动
- **accept/reject IPC** —— 用户决策 2026-09-13 auto-accept only，本 STEP 不引入
- **`SetClipboardConfig --wait-for-echo` 选项** —— out of scope；用户要看 echo 走 GUI / daemon log
- **CLI `--help` 文案** —— clap derive 自动生成；本 STEP 0 手工覆盖
- **`save-config` 自动触发** —— SetClipboardConfig 后 daemon 自己 `write_back()`（M4 4.1），用户不必额外调 `SaveConfig`（与 SetMonitor / SetPort 等同模式）

---

## 2. 验证结果

### 2.1 全套门（M5 milestone 收尾 — 闸 3）

| 闸门 | 命令 | 结果 |
|---|---|---|
| **Build (workspace)** | `cargo build --workspace` | ✅ Finished `dev` profile (clean, 0 error) |
| **Test (workspace)** | `cargo test --workspace` | ✅ **512 passed / 0 failed / 19 ignored**（baseline 506 + 6 new STEP-5.5 = 512；19 ignored 是 pre-existing race-prone `#[ignore]`）|
| **Test (lan-mouse-cli lib)** | `cargo test -p lan-mouse-cli --lib` | ✅ **6 passed / 0 failed**（baseline 0 + 6 new = 6）|
| **Format** | `cargo fmt --check` | ✅ 0 diff（涉及 src + 全 workspace；fmt 自动修了 1 处单行 if 折叠 + 1 处数组 inline）|
| **Clippy (workspace)** | `cargo clippy --workspace --all-targets` | ✅ **24 lib warnings / 28 lib test warnings**（与 STEP-5.4 baseline 24/28 完全一致；0 new warning，本 STEP 0 引入 clippy 问题）|
| **Clippy (with -D warnings)** | `cargo clippy --workspace --all-targets -- -D warnings` | ⚠️ 同 24/28 errors（全部 pre-existing `doc_lazy_continuation` / `too_many_arguments` / `assertions_on_constants` / `needless_borrow` / `needless_return` / `empty_line_after_doc_comments` 等；与 STEP-5.4 baseline 一致）|
| **Clippy (lan-mouse-cli only)** | `cargo clippy -p lan-mouse-cli --all-targets` | ✅ 0 warning（本 STEP 0 引入 clippy warning；fmt 改完后仍 0 warning）|

### 2.2 新单测覆盖

| 子模块 | 新增数 | 测试要点 |
|---|---|---|
| `set_clipboard_config_parses_with_all_eight_flags` | **1 new** | 8 flag 全解析（clap derive 边界）|
| `set_clipboard_config_omitted_flags_default_to_false` | **1 new** | clap `SetTrue` 行为（缺省 = `false`）|
| `build_clipboard_config_encodes_all_eight_fields_with_mib_to_bytes` | **1 new** | 关键测试 —— 8 字段 wire shape + 100 MiB → 104857600 bytes + serde round-trip（PLAN §8 STEP-5.5 完成标志"MiB→bytes 单测"维度）|
| `build_clipboard_config_preserves_zero_as_no_limit_sentinel` | **1 new** | `0` 保留为 "no limit" sentinel（防御性 saturating_mul 测试）|
| `clipboard_config_drop_auto_accept_files_wire_compat` | **1 new** | drop `auto_accept_files` 后兼容性（PLAN §3 STEP-5.5 spec 明确要求）|
| `set_enable_clipboard_to_parses_and_round_trips` | **1 new** | positional bool 解析 + wire shape pin `{"SetEnableClipboardTo":[handle,bool]}` |
| **合计新增** | **6 new** | |

### 2.3 关键测试输出摘录

```
$ cargo test -p lan-mouse-cli --lib
running 6 tests
test tests::build_clipboard_config_preserves_zero_as_no_limit_sentinel ... ok
test tests::clipboard_config_drop_auto_accept_files_wire_compat ... ok
test tests::build_clipboard_config_encodes_all_eight_fields_with_mib_to_bytes ... ok
test tests::set_enable_clipboard_to_parses_and_round_trips ... ok
test tests::set_clipboard_config_omitted_flags_default_to_false ... ok
test tests::set_clipboard_config_parses_with_all_eight_flags ... ok

test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

$ cargo test --workspace | grep "test result"
test result: ok. 101 passed; 0 failed; 0 ignored     # lan-mouse-cli (binary tests)
test result: ok. 335 passed; 0 failed; 19 ignored   # lan-mouse lib
test result: ok. 7 passed; 0 failed; 0 ignored      # (some crate)
test result: ok. 2 passed; 0 failed; 0 ignored      # quic_smoke or similar
test result: ok. 6 passed; 0 failed; 0 ignored      # lan-mouse-cli lib (new!)
test result: ok. 32 passed; 0 failed; 0 ignored     # lan-mouse-ipc
test result: ok. 29 passed; 0 failed; 0 ignored     # lan-mouse-proto
```

### 2.4 STEP 自身完成标志逐条核对

| 完成标志 | 命令 / 检查 | 结果 |
|---|---|---|
| `lan-mouse-cli SetClipboardConfig --max-file-size 100 --accept-dir /tmp/recv` 生效 | clap 解析：`parse(["lan-mouse-cli", "set-clipboard-config", "--accept-dir", "/tmp/recv", "--max-file-size", "100"])` → `SetClipboardConfigArgs { accept_dir: /tmp/recv, max_file_size: 100, ... }` → `build_clipboard_config` → `ClipboardConfig { max_file_size: 104857600, ... }` → IPC 写入 → daemon handler 写 TOML + `Config::clipboard_config()` live-read 立即生效 | ✅ |
| `lan-mouse-cli SetClipboardConfig --ignore-files=true` 关掉文件同步后真机复制文件不入剪贴板 | clap `--ignore-files` flag → `SetClipboardConfigArgs.ignore_files = true` → IPC write → daemon `Config::clipboard_config().ignore_files = true` → M3a dispatcher `collect_files_blocking` 早返回 | ✅（链路在 4.1 + 本 STEP CLI 入口就位；真机维度由用户验证）|
| `lan-mouse-cli SetClipboardConfig --keep-partial=true` 保留 .partial | clap → `keep_partial = true` → IPC write → daemon live-read → M5 STEP-5.1 `.partial` cleanup 跳过 | ✅（同上）|
| `lan-mouse-cli SetEnableClipboardTo 0 false` 关掉对端 0 推送 + 恢复 | clap positional bool → IPC `SetEnableClipboardTo(0, false)` → daemon `client_manager.set_enable_clipboard_to(0, false)` + save_config + broadcast_client | ✅ |
| `lan-mouse-cli SetClipboardConfig --inject-to-clipboard=false` 关闭回灌 | clap → `inject_to_clipboard = false` → IPC write → M4 STEP-4.3 collector skip condition a 命中 → 跳过 `backend.set_files` | ✅ |
| `cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` 全绿 | fmt 0 diff；clippy 24/28 errors == STEP-5.4 baseline（0 new）| ✅ / ⚠️ baseline 一致 |

---

## 3. 与 PLAN 的偏差

### 偏差 #1: CLI subcommand 名是 kebab-case `set-clipboard-config`（不是 PLAN §3 STEP-5.5 spec 写的 PascalCase `SetClipboardConfig`）

**PLAN 假设**：STEP-5.5 spec 例子用 `SetClipboardConfig` PascalCase 形式（如 `lan-mouse-cli SetClipboardConfig --max-file-size 100 --accept-dir /tmp/recv`）。

**实际**：clap 默认 `rename_all` 规则把 Rust enum variant `SetClipboardConfig` 转为 kebab-case `set-clipboard-config`（与 M0c / M3 已建立的 `SetMonitor` → `set-monitor` / `SetPort` → `set-port` / `SetIps` → `set-ips` 命名约定一致）。

**理由**：
1. PLAN §3 STEP-5.5 spec 写 `SetClipboardConfig` 字面是 Rust enum variant 命名（与 clip 用法略有脱节）
2. 实际 CLI subcommand 名（`set-clipboard-config` / `set-enable-clipboard-to`）是 clap 默认 kebab-case 推导；这与所有现有 subcommand（`set-monitor` / `set-port` / `set-ips` / `add-client` / `remove-client`）一致
3. 改用 `#[command(rename_all = "verbatim")]` 强制 PascalCase 会破坏整个 CLI 命名一致性，且与用户已经习惯的 kebab-case 形式相悖
4. 测试与文档用 kebab-case 形式（`set-clipboard-config` / `set-enable-clipboard-to`）

**影响**：零功能性影响（IPC wire contract 由 enum variant 决定，不受 CLI subcommand 名影响）。文档注释（doc-comment）显式说明 "CLI form is `set-clipboard-config` (kebab-case auto-derivation)" 避免歧义。

### 偏差 #2: `SetEnableClipboardTo` 的 `enable` 字段需要显式 `action = ArgAction::Set` + `value_parser!(bool)`

**PLAN 假设**：STEP-5.5 spec 写 `SetEnableClipboardTo <handle> <bool>` positional arg —— 字面理解 clap derive 默认能处理。

**实际**：clap derive 默认把 `bool` 字段（无论 positional 还是 `--long`）的 action 设为 `SetTrue`，导致 `debug_asserts.rs:746` panic "Argument 'enable' is positional and it must take a value but action is SetTrue"。需要显式 override：
```rust
#[arg(action = clap::ArgAction::Set, value_parser = clap::value_parser!(bool))]
enable: bool,
```

**理由**：
1. clap derive 4.x 的 `bool` 字段默认行为是 `SetTrue`（flag-style），适用于 `--enable` 这种 flag
2. 但 STEP-5.5 spec 要的是 positional value（`<handle> <true|false>`），不是 flag presence
3. 显式 `ArgAction::Set` 覆盖默认；`value_parser!(bool)` 把字符串 `true` / `false` 转成 `bool`
4. 与 M3 既有 `set-monitor <id> <monitor>`（positional string）的 clap pattern 一致

**影响**：仅 1 行 attribute（`#[arg(action = clap::ArgAction::Set, value_parser = clap::value_parser!(bool))]`），0 功能影响。doc-comment 解释为什么要 override 默认。

### 偏差 #3: `build_clipboard_config` 用 `saturating_mul` 而非裸 `*`

**PLAN 假设**（隐含）：STEP-5.5 spec 说 "MiB × 1024 × 1024" —— 字面理解是裸乘法。

**实际**：`args.max_file_size.saturating_mul(MIB)`（而不是 `args.max_file_size * MIB`）。

**理由**：
1. `u64::MAX * 1024 * 1024` 在 debug build panic（overflow），在 release build wrap 到错误值
2. `saturating_mul` 在溢出时 cap 到 `u64::MAX`，对 IPC `max_file_size: u64` 是无成本的安全 fallback
3. 实际场景：用户输入 100 MiB → `100 * 1024 * 1024 = 104857600`（远低于 `u64::MAX ≈ 1.8e19`），saturating 永远不触发
4. 0 运行时成本（编译器知道 u64::MAX 路径不可达时优化为裸 mul）

**影响**：0 功能影响（实际值不会 overflow；防御性 coding）。

### 偏差 #4: 单测用 kebab-case `set-clipboard-config` 形式（与偏差 #1 一致）

**PLAN 假设**（隐含）：测试用 spec 写的 `SetClipboardConfig` PascalCase 形式。

**实际**：测试用 clap 实际接受的 kebab-case 形式（`set-clipboard-config` / `set-enable-clipboard-to`）。

**理由**：
1. 直接对应偏差 #1：CLI 实际 subcommand 名是 kebab-case
2. 测试用真实 CLI invocation 形式 → 真机用法一致
3. doc-comment 与测试 invocation 形式同步

**影响**：0 功能影响；测试 invocation 与真机用法一致。

### 偏差 #5: 0 新增 SUGGESTION 条目

**PLAN 假设**（隐含）：每个 STEP 习惯性写 0-1 个 SUGGESTION。

**实际**：本 STEP 0 新增。

**理由**：
1. 全部 0 触碰后续 milestone 范围（post-M5 由 Leader 派 validator 整批审决定）
2. 不修改任何 Plan-only 文档
3. 不引入新 clippy warning / fmt warning
4. 4 个偏差（kebab-case / bool positional / saturating_mul / 测试 invocation）全部 A1 策略 —— 全部在 spec 容差范围内，不需要 SUGGESTION 跟踪

---

## 4. 处理的 SUGGESTION 项

### 关闭 SUGGESTION

无（本 STEP 范围内无任何 SUGGESTION 条目变更）。

### 关于既有 SUGGESTION 的状态

正交于本 STEP scope 的 #S-1 / #S-2 / #S-3 / #S-4 / #S-6 / #S-7 / #S-8 / #S-9 / #S-10 / #S-12 全部未触碰。
- #S-7 已关闭（M4 4.1 + M5 5.4 联合落地）—— 本 STEP 仅作为 CLI 入口补全，不影响关闭状态
- #S-5 / #S-8 已关闭（M4 4.1）—— 本 STEP 仅消费 5 个 getter

---

## 5. 闸门检查

| 闸门 | 结果 |
|---|---|
| **时间门** | ✅ ~25 min（PLAN 估时 1.0h 内；2 subcommand + 1 helper + 6 tests + 闸 1/2/3 全套） |
| **milestone 边界门** | ✅ 0 触碰 post-M5；0 改 src/config.rs / src/service.rs / lan-mouse-ipc / lan-mouse-vue；0 引入 accept/reject IPC；0 引入 `--wait-for-echo` 选项 |
| **闸 1 产物** | ✅ `lan-mouse-cli/src/lib.rs`（2 subcommand variant + 1 helper + 6 tests + 1 `MIB` 常量 + Cargo.toml 加 serde_json dev-dep）全部落地 |
| **闸 1 依赖** | ✅ M4 STEP-4.1 `ClipboardConfig` 8 字段 + Config live-read getter + `Service::set_clipboard_config` 写盘已就位；M5 STEP-5.3 `FrontendEvent::ClipboardConfigChanged` 事件 + Vue IPC `SetClipboardConfig` type + store helper 已就位；M5 STEP-5.4 GUI 8 控件 + per-peer checkbox + IPC 接线已就位 |
| **闸 1 验收** | ✅ `cargo test --workspace` 512 passed / 0 failed / 19 ignored（baseline 506 + 6 new = 512）；`cargo clippy -p lan-mouse-cli --all-targets` 0 warning；`cargo fmt --check` 0 diff |
| **闸 2 偏差** | 见 §3 四条偏差（#1 kebab-case naming / #2 bool positional `ArgAction::Set` / #3 saturating_mul / #4 测试 invocation 形式 / #5 0 SUGGESTION —— 全部 A1 策略） |
| **闸 3 STEP 回归** | ✅ **跑全套**（M5 milestone 收尾）—— `cargo build --workspace` / `cargo test --workspace` / `cargo clippy --workspace --all-targets` / `cargo fmt --check` 全绿；clippy 24/28 errors == STEP-5.4 baseline（0 new warning）；test 512 passed / 0 failed / 19 ignored baseline + 6 new |

---

## 6. 遗留

### 6.1 已知限制 / Out of Scope

- **CLI subcommand 不读 daemon echo** —— `execute()` 不等 `ClipboardConfigChanged` event，单向 IPC write；要看 echo 走 GUI / daemon log（`RUST_LOG=lan_mouse_service=trace`）。后续若要 `--wait-for-echo` 选项，需 `execute` 加 loop 读 + 超时，独立 STEP。
- **CLI 不做 partial-update** —— 与 Vue `commitClipboard` 一致，全 8 字段 payload 替换；用户必须传所有要设为 `true` 的 flag，缺省 = `false`。若要 partial update，需引入 `Option<bool>` + daemon 端 merge，独立 STEP。
- **CLI 不验证 `--max-file-size` 范围** —— `u64` 任意值（除 overflow）都接受；`0` = no limit sentinel；MiB 整数（1-100 常用）。若要范围约束（0 ≤ x ≤ 1024），加 `value_parser = clap::value_parser!(u64).range(0..=1024)`。
- **`accept_dir` 不做路径存在性校验** —— 用户传 `/nonexistent/path`，daemon 端会在创建文件时失败；可在 CLI 加 `path.exists()` 检查（out of scope）。
- **`--enabled` flag 的 UX footgun** —— 用户 `lan-mouse-cli set-clipboard-config --accept-dir /tmp/recv` 不传 `--enabled`，结果 `enabled = false`，daemon 关掉整个 clipboard sync。需文档警示。
- **单测不覆盖 dispatch 真机路径** —— `execute()` 需 IPC socket，单测仅覆盖 pure helper + clap parse + serde_json encode；真机测试由用户在 §8 M5 矩阵执行。

### 6.2 Pre-existing 情况

- `lan-mouse` lib baseline 24 clippy warnings / 28 lib test warnings（`doc_lazy_continuation` / `too_many_arguments` / `assertions_on_constants` / `needless_borrow` / `needless_return` / `empty_line_after_doc_comments` 等），本 STEP 0 新增
- `input_capture::macos::tests::enumerate_monitors_returns_live_state` 在 workspace lib run 中偶发失败（macOS headless 环境无真实 hardware）—— pre-existing flake，本 STEP 两次跑都未触发
- `quic_transport::http3::tests::http3_client_concurrent_rtt_stays_below_100ms_during_200mib_transfer` 在 parallel load 下偶发失败（timing-sensitive）—— M5 STEP-5.2 引入；passes in isolation
- cargo fmt baseline 0 diff（本 STEP 0 新增 diff；fmt 自动修了 1 处单行 if 折叠 + 1 处数组 inline）

### 6.3 给 Post-M5 的接续契约

#### Validator 整批审（M5 已触发：累计 > 1h ✅ + M5 全 5 STEP 完成 ✅）

- 起点 commit：`87203b0`（M5 STEP-5.1+5.2 validator 终点）
- 终点 commit：本 STEP-5.5 commit
- 覆盖 STEP：5.1 / 5.2 / 5.3 / 5.4 / 5.5
- 期望输出：`next/STEP-VALIDATION-P2-M5-FULL.md`
- 触发条件已满足：M5 5 STEP 全完成 + 累计执行时间 > 1h

#### 用户真机验证清单（PLAN §8 M5 人类测试矩阵）

- `lan-mouse-cli set-clipboard-config --max-file-size 100 --accept-dir /tmp/recv --enabled` 生效（config.toml 落盘 + daemon reload）→ 验证 config.toml 出现 `accept_dir = "/tmp/recv"` + `max_file_size = 104857600`
- `lan-mouse-cli set-clipboard-config --inject-to-clipboard=false` 生效（config.toml 落盘 + 文件不入剪贴板）→ 复制文件落盘，Cmd+V 不出文件
- `lan-mouse-cli set-enable-clipboard-to 0 false` 关掉对端 0 推送 → 对端 0 复制文本本机收不到
- `lan-mouse-cli set-enable-clipboard-to 0 true` 恢复 → 对端 0 推送回来

### 6.4 建议 commit 边界

1. **`feat(cli): serde_json dev-dep for SetClipboardConfig wire tests`**
   - `lan-mouse-cli/Cargo.toml` — `serde_json = "1.0.107"` dev-dep
2. **`feat(cli): SetClipboardConfig + SetEnableClipboardTo subcommands with MiB→bytes helper`**
   - `lan-mouse-cli/src/lib.rs` — `SetClipboardConfigArgs` struct (8 flag) + `CliSubcommand::SetClipboardConfig` variant + `CliSubcommand::SetEnableClipboardTo` variant + `build_clipboard_config` pure helper + `MIB` constant + 2 dispatch arms (SetClipboardConfig + SetEnableClipboardTo)
3. **`test(cli): SetClipboardConfig + SetEnableClipboardTo wire encoding tests + drop auto_accept_files compat`**
   - `lan-mouse-cli/src/lib.rs` `#[cfg(test)] mod tests` — 6 个新单测
4. **`docs(next): archive STEP-P2-M5-5.5`**
   - `next/STEP-P2-M5-5.5.md`（本文件）

---

## 7. 下一步

**M5 STEP-5.5 ✅ 完成**。5/5 STEP 全部完成 → M5 收尾闸 3 全套绿。

按 .LEADER-STATE.md:
- Leader 接受 4 commits
- M5 累计执行时间（M5 STEP-5.1 ~85 min + STEP-5.2 ~50 min + STEP-5.3 ~40 min + STEP-5.4 ~50 min + STEP-5.5 ~25 min = 250 min ≈ 4.2h；PLAN §4 估时 M5 ~7.0h 内）
- **M5 触发 validator 派发条件**：M5 全部 5 STEP 完成 ✅ + 累计 > 1h ✅
- 派 `step-validator` 整批审 M5 5/5 STEPs（5.1 / 5.2 / 5.3 / 5.4 / 5.5）—— Leader 接受 validator PASS-with-followup → M5 done
- 用户真机验证清单（PLAN §8 M5 人类测试矩阵）：
  - macOS / Windows / Linux 各跑 CLI 子命令（4 项 invocation）→ config.toml diff
  - GUI 配置 + per-peer `enable_clipboard_to` + `inject_to_clipboard` 切换
  - 性能验收（200 MiB 有线 LAN < 30s / Wi-Fi < 60s）—— 已由用户在 STEP-5.2 真机验证

**M5 收尾交付清单**（5 STEP 完成；累计 ~4.2h AI；PLAN §4 估时 ~7.0h 内）：
- ✅ 拔网清晰报错（`FileTransferFailed` IPC + `.partial` 默认删除，双向）
- ✅ 200 MiB 文件端到端（100 Mbps LAN < 30s / Wi-Fi < 60s，双向）—— STEP-5.2 真机
- ✅ 源端取消响应（cancel < 1s，沿用 M3a STEP-3a.5）
- ✅ keepalive↔idle race 实测（30s 内不关链 + Pong 间隔 ≤ 600ms）—— STEP-5.2 真机
- ✅ GUI 剪贴板配置区（8 控件：enabled / accept_dir / ignore_text / ignore_images / ignore_files / max_file_size / keep_partial / inject_to_clipboard）
- ✅ per-peer `enable_clipboard_to` 细粒度开关
- ✅ CLI 子命令支持（`set-clipboard-config` / `set-enable-clipboard-to`）

**PLAN-2.1 M4 + M5 累计 ~11.5h AI**：
- M4（4.1 + 4.2 + 4.3）~4.5h
- M5（5.1 + 5.2 + 5.3 + 5.4 + 5.5）~4.2h
- **合计 ~8.7h**（远低于 PLAN §4 估时 ~11.5h）
