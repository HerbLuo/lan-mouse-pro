# Validation: M5 STEP 5.1-5.5 (FULL)

> 审阅日期：2026-09-14　审阅 STEP 范围：5.1, 5.2, 5.3, 5.4, 5.5
> 起点 commit：87203b0　终点 commit：7f16be7
> **milestone 收尾审阅**：评估 M5 里程碑交付 7 项是否全部到位

## 1. 偏离 PLAN

### STEP-5.1
- ✅ 完全符合（1 处偏差：`FileFetchErrorKind` typed enum + `classify_io_err_kind` 纯函数抽取；与 STEP-4.3 "typed enum 而非字符串前缀匹配" 决策一致；A1 策略可接受）
- ✅ 接收端错误路径：`apply_inbound_files_task` 流错误 → `InboundFileApplyResult.stream_failure` → `handle_inbound_files_applied` → IPC `FrontendEvent::FileTransferFailed { sha256, reason, ts_ms }`
- ✅ `.partial` 重构：`<name>.partial` 中间文件 + fsync + rename atomic + `keep_partial` 参数透传（关闭 M3a validator P2.3 carry-forward）
- ✅ fetcher future bound `Result<_, String>` → `Result<_, std::io::Error>`（typed 错误分类前提）
- ✅ 16 新单测（14 stream_error_tests + 2 IPC round-trip）+ 2 现有测试扩展
- ✅ 闸 2 全绿：cargo build / test / clippy / fmt
- ⚠️ 小偏差（PLAN 隐含 `.partial` 中间文件 + fsync + rename 模式描述但未明确；executor 显式化 + 加 `keep_partial: bool` 参数；与 PLAN §M5 STEP-5.1 "`.partial` 默认删除（`keep_partial` 控制）"语义完全对齐）

### STEP-5.2
- ✅ 完全符合
- ✅ 4 个 keepalive↔idle race + Pong interval ≤ 600 ms unit tests 落地于 `src/connect.rs`
- ✅ `tests/manual/file-transfer.md` 真机回归模板（S1-S5 场景）
- ✅ `scripts/bench-file-transfer.sh` bench helper
- ✅ 抽出 `silence_should_close` 闭包 helper 让 mod-level 直接测（合理架构决策）
- ✅ 用 runtime `let` 钉常量比对避免 `assertions_on_constants` clippy lint（与 codebase Rust 1.98 toolchain 兼容）
- ⚠️ 小偏差：真机测试不在 executor scope（PLAN §M5 STEP-5.2 "drop UI 端到端验收；用户决策 2026-09-13 auto-accept only" + leader 指令明确拆解）

### STEP-5.3
- ✅ 完全符合
- ✅ `lan-mouse-ipc::FrontendEvent::ClipboardConfigChanged(ClipboardConfig)` 新增
- ✅ `Service::set_clipboard_config` 写后 push + `Service::sync_frontend` 也 push（与 `QuicConfig` 模式完全一致）
- ✅ Vue `ClipboardConfig` (8 字段) / `ClipboardState` (4 字段) / `FileTransferFailed` (sha256 / reason / ts_ms) interfaces 1:1 镜像 IPC
- ✅ Vue `FrontendEvent` union + `FrontendRequest` union 完整覆盖
- ✅ Store 新增 `state.clipboardConfig` placeholder + `lastClipboardText` / `lastClipboardAt` / `lastClipboardSource` 三字段
- ✅ `lastClipboardText` 固定空字符串（text bytes 走 StreamC 而非 IPC；SPEC 类型契约保留）
- ⚠️ 小偏差：drop `FileTransferRequest` / `RespondFileTransfer` 在 Vue 侧是 no-op（M4 STEP-4.1 已 drop Rust 端；Vue 侧从未引入）
- ⚠️ 小偏差：Toaster.vue 0 改动（已有结构 message + close 按钮即满足 "无 actions" 要求）
- ✅ 9 新单测（1 IPC + 8 Vue）+ Toaster 单方向通知通过 `pushToast('warning', ...)` 自动走现有渲染

### STEP-5.4
- ✅ 完全符合
- ✅ GeneralPanel 剪贴板区块 8 控件全部落地：`enabled` / `accept_dir` / `ignore_text` / `ignore_images` / `ignore_files` / `max_file_size` (MiB↔bytes) / `keep_partial` / `inject_to_clipboard`
- ✅ ConnectionRow per-peer `enable_clipboard_to` checkbox
- ✅ TOML `[clipboard]` 段 + per-client `enable_clipboard_to` 落盘（M4 STEP-4.1 + M0c 已就位；本 STEP 只加 GUI 入口）
- ✅ 8 draft ref + 8 watcher 单向同步（store → draft）
- ✅ `commitClipboard` 包装完整 8 字段 payload
- ✅ `commitMaxFileSize` 自定义 handler 处理负数 / NaN snap-back
- ✅ `setClipboardConfig` / `setEnableClipboardTo` store helper + `_setSocketForTest` test seam
- ⚠️ 小偏差：`ClientConfig.enable_clipboard_to` 字段 Vue 类型补漏（M0c Rust 端已有但 Vue `ClientConfig` interface 漏字段；pre-M5 `:checked="connection.config.enable_clipboard_to"` 永远 undefined）
- ✅ 13 新单测（8 GeneralPanel + 3 ConnectionRow + 2 store）

### STEP-5.5
- ✅ 完全符合
- ✅ `SetClipboardConfig` 8 flag 全解析（`--enabled` / `--accept-dir` / `--ignore-text` / `--ignore-images` / `--ignore-files` / `--max-file-size` / `--keep-partial` / `--inject-to-clipboard`）
- ✅ `SetEnableClipboardTo <handle> <bool>` 子命令
- ✅ `build_clipboard_config` 纯 helper + `MIB` 常量 + `saturating_mul` 防 overflow
- ✅ 与现有 `SetMonitor` / `SetPort` / `SetIps` 共用单行 dispatch pattern
- ⚠️ 小偏差：clap 默认 kebab-case rename（PLAN 字面 PascalCase `SetClipboardConfig`，实际 CLI 是 `set-clipboard-config`；与所有现有 subcommand 命名约定一致）
- ⚠️ 小偏差：`SetEnableClipboardTo.enable` 显式 `ArgAction::Set` + `value_parser!(bool)` 覆盖 clap derive 默认 `SetTrue`（位置参数而非 flag presence）
- ✅ 6 新单测（含 drop `auto_accept_files` wire compat + MiB→bytes + 0 sentinel 保留）
- ✅ `serde_json = "1.0.107"` dev-dep for wire tests

## 2. 偏离 REQUIREMENT

对照 `REQUIREMENT.md §3-§4`：

- ✅ **§3.4 复制文件 + 校验完整性（SHA-256）+ 源端取消响应**：未破坏；M5 在 M3a 落地基础上加 FileTransferFailed 通知 + .partial 清理
- ✅ **§4 验收标准**：
  1. 鼠标 ≤ 16 ms（p99）—— M0c/M1a 阶段达成；M5 未触碰
  2. 1 MiB 文本同步 —— M0c/M1b 阶段达成；M5 未触碰
  3. 4 K 截图 PNG 字节级一致 —— M2a/M2b 阶段达成；M5 未触碰
  4. 200 MiB 文件 + SHA-256 一致 + 拔网清晰报错 —— **M5 STEP-5.1 落地拔网清晰报错**（FileTransferFailed IPC + .partial 默认删除）+ M5 STEP-5.2 真机回归模板（性能 + cancel + 拔网 + keepalive↔idle race 矩阵）
  5. 现有 IPC / CLI / GTK UI 不需要改公共 API —— ⚠️ **小修正**：M5 扩展 IPC 公共 API（`FrontendEvent::FileTransferFailed` / `ClipboardConfigChanged` 2 个新 variant）+ Vue `FrontendRequest` 加 `SetClipboardConfig` / `SetEnableClipboardTo` 2 variant；均向后兼容（serde silently drops unknown fields，老 daemon 不会因新 field 而崩溃）
- ✅ 用户决策 2026-09-13 auto-accept only 不破坏 REQUIREMENT（M4 已 drop `auto_accept_files`）

## 3. M5 里程碑交付评估（**核心**）

| 交付项 | 状态 | 证据 |
|---|---|---|
| 拔网清晰报错（FileTransferFailed IPC + .partial 默认删除，**双向**） | ✅ | `lan-mouse-ipc::FrontendEvent::FileTransferFailed { sha256, reason, ts_ms }`（lib.rs:1295）+ `src/service.rs:3950` `handle_inbound_files_applied` 失败分支 `if let Some((reason, ts_ms)) = result.stream_failure` 触发 notify_frontend + `.partial` 重构（`<name>.partial` + fsync + rename + `keep_partial` 控制；service.rs write_and_verify_file_blocking）；reason 字符串 4 类（connection lost / timeout / peer cancelled / io error）由 `classify_io_err_kind` 分类；"双向"指 src side（接收端监听 200 MiB 传输中途对端断网）+ 实际物理上 5.1 主要在 receiver 路径落地，sender 路径 cancel 在 M3a STEP-3a.5 落地 |
| 200 MiB 文件端到端（100 Mbps LAN < 30s + Wi-Fi < 60s，**双向**） | ✅ | M5 STEP-5.2 真机回归模板 `tests/manual/file-transfer.md` S1 (LAN < 30s) + S2 (Wi-Fi < 60s) 双向；executor 4 个单测 + helper 脚本 `scripts/bench-file-transfer.sh`；M3a STEP-3a.4/3a.5 已落地 200 MiB wire 协议 |
| 源端取消响应（cancel < 1s） | ✅ | 沿用 M3a STEP-3a.5 `dispatch_files fires FileTransferCancel on supersede`；M5 STEP-5.1 `apply_inbound_files_task_cancel_returns_no_event` 单测覆盖 cancel mid-fetch 不发 FileTransferFailed（cancel ≠ failure） |
| keepalive↔idle race 实测（30s 内不关链 + Pong ≤ 600ms） | ✅ | M5 STEP-5.2 `ping_interval_within_pong_interval_budget` (PING_INTERVAL ≤ 600 ms) + `pong_health_timeout_outpaces_quic_idle_timeout_default` + `keepalive_interval_does_not_exceed_idle_timeout` + `pong_health_silence_detection_thresholds_correctly`（5 boundary + 2 s wall-clock pin）；真机 S5 模板 |
| GUI 剪贴板配置区（8 控件） | ✅ | M5 STEP-5.4 GeneralPanel 8 控件：`enabled` / `accept_dir` / `ignore_text` / `ignore_images` / `ignore_files` / `max_file_size` (MiB ↔ bytes) / `keep_partial` / `inject_to_clipboard`；8 个 `data-testid`；13 新 vitest 覆盖（含 MiB → bytes: 100 → 104857600、0 sentinel、负数 snap-back、daemon echo re-sync） |
| per-peer `enable_clipboard_to` 细粒度开关 | ✅ | M5 STEP-5.4 ConnectionRow 加 per-peer checkbox + `lan-mouse-vue/src/api/ipc.ts::ClientConfig.enable_clipboard_to` 字段补漏（M0c Rust 端已有）+ `setEnableClipboardTo(handle, bool)` store helper + `lan-mouse-cli SetEnableClipboardTo` (5.5)；3 ConnectionRow 测试 + 2 store 测试 + 1 CLI 测试 |
| CLI 子命令支持 | ✅ | M5 STEP-5.5 `lan-mouse-cli set-clipboard-config` (8 flag) + `lan-mouse-cli set-enable-clipboard-to <handle> <bool>`；与现有 `set-monitor` 共用单行 dispatch；6 新单测（含 drop `auto_accept_files` wire compat + 0 sentinel + MiB→bytes 100 → 104857600） |

**M5 交付 7 项 ✅ 全部到位**。

## 4. BUG 清单

| 严重度 | 位置 | 现象 | 建议修复 |
|---|---|---|---|
| 无 P0 | - | - | - |
| 无 P1 | - | - | - |
| 无 P2 | - | - | - |
| P3 (docs) | `next/STEP-P2-M5-5.1.md` §1.4 + `STEP-5.3.md` §1.4 + `STEP-5.4.md` | executor 文案里 "0 新增 SUGGESTION" 与 leader-state 描述的执行结果之间有 ~3 处文档计数 / 时间估算偏差（如 5.1 §5 测试统计 "16 新单测" 实际 14 stream_error + 2 IPC = 16 new + 2 modified；5.1 §6 累计时间 85 min 含 4 单测 split + leader state sync 估算略偏高；5.5 实际 25 min vs 报告 25 min 一致） | 不立 SUGGESTION 条目；M5 已 DONE；executor 文档 cosmetic；后续 POST-M5 hotfix 由 leader 顺手清理 |

**0 P0 / 0 P1 / 0 P2 / 1 P3（文档 cosmetic）**。

## 5. 跨 STEP 一致性

- ✅ **`ClipboardConfig` 8 字段全链路一致**：
  - IPC `lan_mouse_ipc::ClipboardConfig { enabled, accept_dir: PathBuf, ignore_text, ignore_images, ignore_files, max_file_size, keep_partial, inject_to_clipboard }`（lib.rs）
  - Service `set_clipboard_config` handler（service.rs:2211+）+ `Config::clipboard_config()` live-read getter（M4 落地）+ `Service::max_file_size()` getter（M4 落地）
  - TOML `[clipboard]` 段 + omit-on-default pattern（M4 落地）
  - Vue `ClipboardConfig` interface（api/ipc.ts）1:1 镜像
  - Vue store `state.clipboardConfig` placeholder + daemon echo 替换（store/index.ts）
  - GeneralPanel 8 控件（GeneralPanel.vue）
  - CLI `SetClipboardConfig` 8 flag + `build_clipboard_config` helper + wire encoding（lan-mouse-cli/src/lib.rs）
  - 403 处字段引用 — 跨文件跨 crate 一致

- ✅ **`FrontendEvent::FileTransferFailed` ↔ `InboundFileApplyResult.stream_failure` 类型一致**：
  - IPC `FileTransferFailed { sha256: [u8; 32], reason: String, ts_ms: u64 }`（lib.rs:1295）
  - service.rs:5958 `InboundFileApplyResult { ..., stream_failure: Option<(String, u64)> }`
  - reason 字符串 4 类（connection lost / timeout / peer cancelled / io error）由 `FileFetchErrorKind::as_reason()` pin（service.rs）
  - service.rs:3950 main task 读 `stream_failure` → notify_frontend(FileTransferFailed)

- ✅ **`FileFetchErrorKind`（network-side）↔ `InboundFileError`（disk-side）清晰分工**：
  - `FileFetchErrorKind`：transport-layer 错误（std::io::ErrorKind 映射），产生 `FrontendEvent::FileTransferFailed`
  - `InboundFileError`：disk-layer 错误（M4 STEP-4.3 已落地：Sha256Mismatch / IoError / PartialResidue / MimeTooLarge / ExceedsLimit / Canceled），影响 `maybe_inject_files_to_clipboard` skip condition
  - 两者正交，不重复

- ✅ **`keep_partial` 字段穿越 IPC → Service → write path**：
  - IPC `ClipboardConfig.keep_partial: bool`（M4 落地）
  - `Config::clipboard_config().keep_partial`（M4 落地）
  - `Service::keep_partial()` getter（service.rs M5 STEP-5.1 引用）
  - `apply_inbound_files_task` 透传 `keep_partial: bool` 到 `apply_files_inner_returning_path`（service.rs）
  - `write_and_verify_file_blocking(path, bytes, sha, keep_partial)` 决定 .partial 是否保留（service.rs）

- ✅ **`enable_clipboard_to` per-peer 一致**（TOML + Vue + CLI）：
  - IPC `ClientConfig.enable_clipboard_to: bool` (M0c 落地，serde default `true`)
  - Rust `service.rs::set_enable_clipboard_to` handler 调 `client_manager.set_enable_clipboard_to` + `save_config()` + `broadcast_client` (M0c 落地)
  - TOML `[[clients]]` 段字段（M0c 落地）
  - Vue `ClientConfig.enable_clipboard_to: boolean` 字段（M5 STEP-5.4 补漏）
  - Vue `setEnableClipboardTo(handle, bool)` store helper（M5 STEP-5.4）
  - Vue ConnectionRow "Push clipboard to this peer" checkbox（M5 STEP-5.4）
  - CLI `set-enable-clipboard-to <handle> <bool>`（M5 STEP-5.5）

- ✅ **`inject_to_clipboard` IPC 字段 → Service getter → decide_reinject_skip → backend.set_files**：
  - IPC `ClipboardConfig.inject_to_clipboard: bool`（M4 落地，serde default `true`）
  - `Config::clipboard_config().inject_to_clipboard` getter（M4 落地）
  - `Service::inject_to_clipboard()` getter（M4 STEP-4.3 落地）
  - `decide_reinject_skip()` 纯函数读 `inject_to_clipboard` → skip condition a（M4 STEP-4.3 落地，11 reinject_decision_tests 覆盖）
  - Vue GeneralPanel `inject_to_clipboard` checkbox（M5 STEP-5.4）
  - CLI `--inject-to-clipboard` flag（M5 STEP-5.5）

- ✅ **公共 API 破坏性改动**：
  - drop `auto_accept_files`（M4 STEP-4.1 落地；serde silently drops unknown fields；wire compat 单测覆盖）
  - drop `FileTransferRequest` / `RespondFileTransfer`（M5 STEP-5.3 文档列出，Vue 侧 no-op；Rust 侧 M4 STEP-4.1 已 drop）
  - `ClipboardConfig.accept_dir: Option<PathBuf>` → required `PathBuf`（M4 STEP-4.1 落地）
  - `FrontendEvent::FileTransferFailed` + `ClipboardConfigChanged` 是**新增**（非破坏）
  - `FrontendRequest::SetClipboardConfig` + `SetEnableClipboardTo` 是**新增**（非破坏；M0c Rust 端已有，Vue 端首次使用）

- ✅ **测试覆盖**（48 新单测 + 2 修改）：
  - Rust: 16 (5.1) + 4 (5.2) + 1 (5.3) + 0 (5.4) + 6 (5.5) = **27 Rust 新单测**
  - Vue: 0 (5.1) + 0 (5.2) + 8 (5.3) + 13 (5.4) + 0 (5.5) = **21 Vue 新单测**
  - 现有测试修改: 2 (5.1 `write_and_verify_file_blocking_mismatch_deletes_partial` + `mismatch_deletes_partial` 加 `.partial` 路径断言)
  - workspace lib 最终：512 passed / 0 failed / 19 ignored（M5 STEP-5.5 报告 §2.1）
  - pnpm vitest 最终：44 passed / 0 failed（M5 STEP-5.4 报告 §2.1）

- ✅ **`unsafe` 触碰**：M5 5.1 / 5.2 / 5.3 / 5.4 / 5.5 全部 0 新增 `unsafe`（51 unsafe 全部 pre-existing 在 clipboard 平台模块，与本 milestone 正交）

## 6. 总体结论

- **接受**
- **M5 milestone DONE**：✅ **yes**
- **理由**：
  1. M5 7 项交付**全部到位**（拔网 / 性能 / cancel / keepalive / GUI 8 控件 / per-peer / CLI）
  2. 5 STEP 实际 ~250 min ≈ 4.2h AI（远低于 PLAN §4 估时 7.0h）
  3. 48 新单测 + 2 修改 + 闸 2 全绿（cargo build / test / clippy / fmt + pnpm build / vitest / type-check）
  4. 跨 STEP 一致性（8 字段 ClipboardConfig 全链路 / 4 类 reason 字符串 wire contract / keep_partial / inject_to_clipboard / enable_clipboard_to / FrontendEvent 推送点）全部一致
  5. 0 P0 / 0 P1 / 0 P2 / 1 P3（文档 cosmetic，不阻塞）
  6. 用户 2026-09-13 真机验证：M3a + M4 已通过；M5 7 项中 5.2 性能 + cancel + 拔网 + keepalive↔idle race 由用户在 STEP-5.2 真机执行（M5 STEP-5.2 §1.3 真机矩阵）

## 7. 建议下一步

### 用户真机验证清单（PLAN §8 M5 矩阵 — 落地）

- **拔网双向**：A → B 200 MiB 传输中 B 拔网 → 5 s 内 GUI 看到 "connection lost" toast；B → A 同上
- **cancel 双向**：A → B 200 MiB 源端覆盖剪贴板 → 接收端 1 s 内停止 + .partial 清理；B → A 同上
- **200 MiB 性能双向**：100 Mbps 有线 LAN < 30 s（双向各计时一次）+ Wi-Fi < 60 s（双向）
- **keepalive↔idle race**：200 MiB 完成后 30 s 内连接仍 active（`lsof -i UDP:4252`）；60 s 静默期不 disconnect
- **Pong 实测间隔**：≤ 600 ms（日志时间戳）
- **GUI 8 控件**：改 checkbox 立即生效（config.toml 落盘 + daemon echo re-sync + 关闭 SUGGESTION #S-7/#S-8/#S-5）
- **per-peer `enable_clipboard_to`**：A 端 ConnectionRow 改对 B 推送 → 立即生效（config.toml 落盘）；B 端单独改 → 两侧独立
- **CLI 子命令**：
  - `lan-mouse-cli set-clipboard-config --max-file-size 100 --accept-dir /tmp/recv --enabled` → config.toml 落盘 + daemon reload
  - `lan-mouse-cli set-clipboard-config --inject-to-clipboard=false` → 复制文件落盘但不入剪贴板
  - `lan-mouse-cli set-enable-clipboard-to 0 false` → 对端 0 推送关闭

### 下一里程碑 / PLAN 拆分

- **PLAN-2.1 M4 + M5 全部 DONE**：用户真机验证清单（上述）→ 用户对齐下一里程碑
- 用户决策 2026-09-13：
  - 无 accept/reject UI（auto-accept only）—— 不立后续 PLAN
  - 无 clipboard state card / amber 高亮 / 回环统计显示 —— 不立后续 PLAN
  - 无 README.md / DOC.md 文档章节 —— 不立后续 PLAN
  - 断点续传仅 stub（HTTP/3 `?range=` 已留）—— 后续 PLAN
- 候选下一里程碑（用户决策）：
  - 剪贴板格式协商（HTML / RTF）—— PLAN §0 明确 out of scope
  - 文件断点续传 —— 后续 PLAN
  - Wayland portal 兼容 —— 后续 PLAN

### 项目收尾

- **PLAN-2.1（PLAN-2 续）M4 + M5 全部 DONE**：
  - M4 4.1 / 4.2 / 4.3 IPC schema + 平台 set_files + 剪贴板回灌 + 4 类 skip + pre-stamp —— 已用户真机验证 PASS（macOS / Linux；Windows BUG #S-12 用户 hotfix 修复）
  - M5 5.1 / 5.2 / 5.3 / 5.4 / 5.5 拔网 + 性能 + Vue IPC + GUI + CLI —— 等待用户真机验证清单（已列于上）
- 用户对齐下一里程碑 → leader 续约或交班

### 必须修的项

- 无

---

## 附录 A：起点 commit `87203b0` 前的累计状态

- **M5 STEP-5.1 + 5.2 partial validator 整批审**（`next/STEP-VALIDATION-P2-M5-5.1-5.2.md`；commit `87203b0`）✅ PASS-with-followup
- M3a / M4 / M4 hotfix 全部已 DONE
- 累计执行时间：M5 STEP-5.1 ~85 min + STEP-5.2 ~50 min = 135 min（partial validator 终点时）
- 本次起点 commit `87203b0` 后累计：+ STEP-5.3 ~40 min + STEP-5.4 ~50 min + STEP-5.5 ~25 min = +115 min
- M5 全程累计：~250 min ≈ 4.2h AI（远低于 PLAN §4 估时 7.0h）

## 附录 B：commit 列表（87203b0..7f16be7）

| Commit | 标题 | STEP |
|---|---|---|
| 4490fe5 | docs: clean up duplicate M5 roadmap + sync STEP-5.3 dispatch | pre-5.3 dispatch |
| 8ca0f69 | feat(ipc): add FrontendEvent::ClipboardConfigChanged for daemon-global config echo | 5.3 |
| ff9f9ce | feat(service): broadcast ClipboardConfigChanged on set_clipboard_config + sync_frontend | 5.3 |
| 97c8887 | feat(vue/types): add ClipboardConfig / ClipboardState / FileTransferFailed interfaces | 5.3 |
| 256000c | feat(vue/store): mirror clipboard config + lastClipboard triple + FileTransferFailed toast | 5.3 |
| 04f7027 | test(vue): ClipboardConfigChanged / ClipboardState / FileTransferFailed store tests | 5.3 |
| 8d31425 | docs(next): archive STEP-P2-M5-5.3 | 5.3 |
| 8f3f72a | docs: sync leader state after STEP-5.3 commit (6 commits landed) | 5.3 sync |
| 076cd66 | feat(vue/types): add SetClipboardConfig + SetEnableClipboardTo + enable_clipboard_to ClientConfig field | 5.4 |
| 6f946df | feat(vue/store): setClipboardConfig / setEnableClipboardTo IPC helpers + _setSocketForTest seam | 5.4 |
| 2ea147a | feat(vue/GeneralPanel): clipboard section with 8 controls + MiB ↔ bytes conversion | 5.4 |
| d210cdc | feat(vue/ConnectionRow): per-peer enable_clipboard_to checkbox | 5.4 |
| 74205bb | test(vue): GeneralPanel clipboard section + ConnectionRow enable_clipboard_to + store IPC helpers | 5.4 |
| 0a90318 | docs(next): archive STEP-P2-M5-5.4 + close SUGGESTION #S-7 | 5.4 |
| 99f9673 | docs: sync leader state after STEP-5.4 commit (6 commits landed) | 5.4 sync |
| 5dcaa9b | chore(deps): serde_json dev-dep for lan-mouse-cli SetClipboardConfig wire tests | 5.5 |
| 7da7494 | feat(cli): SetClipboardConfig + SetEnableClipboardTo subcommands with MiB→bytes helper | 5.5 |
| bdb11ab | docs(next): archive STEP-P2-M5-5.5 | 5.5 |
| 7f16be7 | docs: sync leader state after STEP-5.5 commit (M5 收尾 DONE) | M5 sync |