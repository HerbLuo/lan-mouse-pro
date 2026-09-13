# Validation: M4 STEP 4.1+4.2+4.3

> 审阅日期：2026-09-13　审阅 STEP 范围：4.1, 4.2, 4.3
> 起点 commit：`49fd3c7`（M3a FULL validator 终点）
> 终点 commit：`058e9fc`（M4 STEP-4.3 docs 收尾 + leader state sync）

## 1. 偏离 PLAN

### STEP-4.1

- **✅ 完全符合** IPC `ClipboardConfig` 8 字段 + `accept_dir: PathBuf` 必填 + drop `auto_accept_files`
- **✅ 完全符合** `Service::max_file_size()` getter live-read（详见偏差 §3）
- **✅ 完全符合** TOML `TomlClipboard` 8 字段 + `Config::clipboard_config()` 全部 8 字段 live-read + `Config::max_file_size()` / `clipboard_enabled()` getter
- **✅ 完全符合** `Service::set_clipboard_config` log 文本更新（覆盖全部 8 字段）
- **⚠️ 小偏差 #1**：`Service::max_file_size: u64` **完全删除**（不仅加 getter，还删 field；executor §3 偏差 #2 A1 策略；与 #S-5 同源 risk）—— 比 PLAN 字面解读更激进。理由充分（SUGGESTION #S-5 根因消除），接受
- **⚠️ 小偏差 #2**：`InboundFilesDecision::AutoAcceptOff` 变体删除 + 2 个旧测试删 —— executor §3 偏差 #3 A1 策略。**与 `auto_accept_files` 删除一致**，接受
- **⚠️ 小偏差 #3**：测试模块精简 5 → 4 + 新增 6（13 net new tests）—— executor §2.2；**实际 13 new + 4 modified**，planer 估时 1.5h 内 ~45 min 完成
- **✅ 完成** 关闭 SUGGESTION #S-5 / #S-8（FIXED #S-5 / #S-8 已落地）

### STEP-4.2

- **✅ 完全符合** `ClipboardBackend::set_files(&mut self, files: &[PathBuf]) -> Result<(), ClipboardError>` trait 签名（`mod.rs:644-654`）
- **✅ 完全符合** DummyBackend 默认 impl `Err(Unsupported)`（mod.rs:644-654；2 个新单测覆盖）
- **✅ 完全符合** macOS `NSPasteboard.general().writeObjects(NSArray<ProtocolObject<dyn NSPasteboardWriting>>)` —— planer round 2 审阅要求严格落地；`ProtocolObject::from_retained(ns_url)` + `objc2-app-kit 0.3.2` 的 `extern_conformance! unsafe impl NSPasteboardWriting for NSURL` 契约已 grep 验证
- **✅ 完全符合** Windows `OpenClipboard` + `EmptyClipboard` + `SetClipboardData(CF_HDROP, hdrop)` + DROPFILES 结构（20-byte header + double-NUL-terminated UTF-16 LE）；复用 `alloc_dib_handle_and_set` helper（沿用 M2b 既有模式）
- **✅ 完全符合** Linux X11 `xclip -selection clipboard -t text/uri-list -i` / Wayland `wl-copy --type text/uri-list` + RFC 2483 CRLF URI list 构造（`build_uri_list` 自由函数 + `parse_uri_list` round-trip 测试）
- **⚠️ 小偏差 #1**（executor §3 偏差 #1）：Linux 子进程 mock 测改为 helper 直测 —— `std::process::Command::spawn` 无原生 mock 设施；测 `build_uri_list` payload + `parse_uri_list` round-trip 等价于验证 stdin 字节流正确。A1 策略
- **⚠️ 小偏差 #2**（executor §3 偏差 #2）：macOS `FilesClipboardGuard` 用 `Retained::cast_unchecked` 替代 unsafe reinterpret —— objc2 0.6 提供的安全等价 API；`Retained<AnyObject>` 转 `Retained<T>` 零开销 + 显式 unsafe 边界。A1 策略
- **⚠️ 小偏差 #3**（executor §3 偏差 #3）：macOS `set_files` 显式 `self.image_cache = None`（cache invalidation）—— 与 `set_image` / `set_dib_image` 行为对称；write bump changeCount 后 next `current_image` 必须重读。A1 策略
- **⚠️ 小偏差 #4**（executor §3 偏差 #4）：三平台 `if files.is_empty() { return Ok(()); }` 防御短路 —— 防御性 guard；dispatcher 保证非空 batch，但 edge case 防御
- **✅ 测试覆盖**：8 个新单测（2 DummyBackend + 3 macOS real pasteboard round-trip + 3 Windows DROPFILES header/payload/round-trip + 3 Linux CRLF/empty/round-trip = 11 实际新增；mod.rs 2 + linux 3 + windows 4 + macOS 1 = 10；executor §1.3 列 8 + 1 macOS integration）
- **✅ 三平台编译通过**（macOS native + Linux zig-cross + Windows zig-cross）

### STEP-4.3

- **✅ 完全符合** `InboundFileApplyResult` 扩展：`batch_fingerprint: [u8; 32]` + `error: Option<InboundFileError>` typed classification
- **✅ 完全符合** `InboundFileError` enum 6 变体：`Sha256Mismatch` / `IoError`（生产可达）+ `MimeTooLarge` / `ExceedsLimit` / `Canceled` / `PartialResidue`（forward-compat，构造用于单测）
- **✅ 完全符合** `FileSetCollector` struct：`expected: usize`（`cf.entries.len()` at start）+ `received: Vec<InboundFileApplyResult>`（per-entry in source order）
- **✅ 完全符合** `Service::pending_file_collectors: HashMap<[u8; 32], FileSetCollector>` keyed by `cf.fingerprint`
- **✅ 完全符合** `BackendCmd::SetFiles { files: Vec<PathBuf>, reply: oneshot::Sender<...> }` 变体 + poller 双分支（backend-absent: `Err(Unsupported)`；backend-present: `reply.send(backend.set_files(&files))`）
- **✅ 完全符合** `decide_reinject_skip` 纯函数 + 4 类 skip conditions（priority 顺序：Disabled > OptOut > Loopback > EntryFailed）
- **✅ 完全符合** `maybe_inject_files_to_clipboard` async method + pre-stamp `last_outbound_files_fingerprint = Some(batch_fingerprint)` **BEFORE** `BackendCmd::SetFiles` send（`src/service.rs:4061`）
- **✅ 完全符合** `ReinjectSkipReason` enum + Display impl
- **✅ 完全符合** `handle_inbound_files_applied` 改 async + collector trigger（`received.len() == expected` → `maybe_inject_files_to_clipboard`）
- **✅ 完全符合** 失败路径**不** early return（失败也 push 进 collector → `EntryFailed` skip）
- **⚠️ 小偏差 #1**（executor §3 偏差 #1）：typed enum 方案 A（vs 字符串前缀匹配方案 B）+ 额外 `batch_fingerprint` 字段 —— typed 让 forward-compat variants 可构造（unit test coverage）+ 未来可细粒度处理；`batch_fingerprint` 是 collector 必须的 batch key。A1 策略，全部接受
- **⚠️ 小偏差 #2**（executor §3 偏差 #2）：决策 fn 抽为纯函数 —— 让 11 个单测不必构造完整 Service；与 M3a `dispatch_files_decide` 模式一致。A1 策略
- **⚠️ 小偏差 #3**（executor §3 偏差 #3）：`handle_inbound_files_applied` 改 `async fn` —— `BackendCmd::SetFiles` 走 oneshot 必须 await reply；main select! arm 已 async，无新 runtime cost。A1 策略
- **⚠️ 小偏差 #4**（executor §3 偏差 #4）：失败也 push 进 collector —— 与 PLAN spec 隐含矛盾，但与 `EntryFailed` skip 条件必然要求（否则 collector 永远等不到"成功 N 个"）。A1 策略
- **⚠️ 小偏差 #5**（executor §3 偏差 #5）：`apply_files_inner_returning_path` 在 `Ok(Err(e))` 分支用 string-prefix 分类（`starts_with("sha256 mismatch")` → `Sha256Mismatch`，否则 `IoError`）—— 不修改 `write_and_verify_file_blocking` 签名，保持 M3a 接口不变。A1 策略
- **⚠️ 小偏差 #6**（executor §3 偏差 #6）：6 个 `apply_inbound_files_task` 既有测试加 `[0u8; 32]` / `[0x77; 32]` 哨兵参数 —— 签名变化的必然传播；其中 `apply_inbound_files_task_writes_file_with_sha256_match` 借机加 `batch_fingerprint == [0x77; 32]` propagation pin + `error == None` classification pin（pin 当次签名变更契约）
- **✅ 测试覆盖**：11 新 reinject_decision_tests（4 skip condition + 3 forward-compat + happy path + priority + `expected_entry_count`） + 6 modified apply_inbound_files_task 测试（哨兵参数 + typed error pin）
- **✅ 跨 STEP 接口一致**：`InboundFileApplyResult.batch_fingerprint` 在 spawn arg → apply_inbound_files_task → apply_files_inner_returning_path → InboundFileApplyResult 三层传播完整

## 2. 偏离 REQUIREMENT

- **✅ 未破坏** §3.4 文件复制需求：
  - 接收行为可配置（auto-accept 默认，master `enabled` 字段 + ignore_files per-kind filter）—— 与原方案一致
  - SHA-256 校验完整（`write_and_verify_file_blocking` 契约不变；M3a 11 个 `apply_inbound_files_task_tests` 仍 pin）
  - 源端取消响应（`handle_clipboard_inbound_cancel` 走 `apply_inbound_files_task` 的 cancel_rx；M3a STEP-3a.5 落地；未触碰）
- **✅ 未破坏** §4 验收标准：
  - 1 MiB 文本 / 4 K 截图 / 200 MiB 文件 端到端契约：剪贴板回灌（4.3）+ IPC schema 收紧（4.1）都不破坏 wire protocol；新加 IPC 字段 `inject_to_clipboard` 缺字段 = `true`（向后兼容）
  - 现有 IPC 公共 API 严格化（accept_dir 必填、auto_accept_files drop）是 PLAN §M4 决策；M5 GUI/CLI 同步落地
- **✅ 新增能力** —— 文件落盘后自动灌回本地剪贴板（`inject_to_clipboard=true` 默认开启），用户可一键 Cmd+V 粘贴；4 类 skip condition 防御性覆盖完整（pre-stamp 防回环 / `enabled=false` / `inject_to_clipboard=false` / 任意 entry 失败）

## 3. BUG 清单

| 严重度 | 位置 | 现象 | 建议修复 |
|---|---|---|---|
| P2 | `src/service.rs:5897-5901` | doc-comment 自相矛盾："the `AutoAcceptOff` variant was removed entirely" 紧接着又写 "the `InboundFilesDecision::AutoAcceptOff` variant was kept (marked `#[allow(dead_code)]` below)"。**实际**：variant 已删除，enum 仅含 `Apply` / `AllMimeTooLarge` / `Empty` 三变体。第二段错误描述，应改为 "removed entirely (matches `auto_accept_files` field removal; the `enabled` master toggle is gated at `Service::run` dispatcher startup, not here)" | 删除矛盾 doc-comment（或合并为单段）；P2 followup，不阻塞本批接受 |
| P2 | `src/service.rs:6481, 4051-4059` | `ReinjectSkipReason::NoLandedPaths` 变体定义（`#[allow(dead_code)]`）但实现中**永不构造**：defensive `if paths.is_empty()` 分支只是 `return`，没构造 `NoLandedPaths` 返回。变体不可达 = 死代码 | 二选一：(a) 移除该变体（不需要 typed defensive 路径，log warn + return 已够）；(b) 在 `paths.is_empty()` 分支显式构造并返回 `Some(NoLandedPaths)`。建议 (b) 以匹配"typed enum for forward-compat" 模式 |
| ~~P0 / P1~~ | ~~--~~ | ~~未发现崩溃 / 数据丢失 / 功能错 bug~~ | ~~--~~ |

**未触碰的 pre-existing 项**（非 M4 引入，不计入 M4 BUG）：

- `src/popup.rs` 在 M3a 阶段已修（commit `85e7b69` drop + fire() 递归修复 + commit `d6fb1d8` ExceedsLimit pre-stamp）—— pre-M4 fixes
- `src/emulation.rs` 在 M3a 阶段已扩展（commit `b8736e9` 添加 `ClipboardFiles` + `FileTransferCancel` 转发）—— pre-M4 fixes
- `lan-mouse-ipc/src/lib.rs` 的 `auto_accept_files=true` 默认翻转（commit `6d3d111`）—— pre-M4 fix
- `src/service.rs` poller Phase 3 fix（commit `9f30228`）—— pre-M4 fix
- `http3_client_concurrent_rtt_stays_below_100ms_during_200mib_transfer` flake（`src/quic_transport/http3.rs:3085`）—— LEADER-STATE.md baseline 已标记 2 pre-existing flakes
- pre-existing `src/popup.rs` 4 处 rustfmt 漂移（executor §6.2 报告）

## 4. 跨 STEP 一致性

- **✅ 完全符合** `ClipboardConfig` 8 字段在 IPC（`lan-mouse-ipc/src/lib.rs`）+ TOML（`src/config.rs::TomlClipboard`）+ Service getter（`src/service.rs::max_file_size()` / `clipboard_enabled()` / `inject_to_clipboard()` / `keep_partial()`）三层一致
- **✅ 完全符合** `ClipboardBackend::set_files` trait 签名在三平台实现（macOS `src/clipboard/macos.rs:739` / Windows `src/clipboard/windows.rs:617` / Linux `src/clipboard/linux.rs:430`）+ DummyBackend 默认 impl（`src/clipboard/mod.rs:644-654`）完全一致
- **✅ 完全符合** `InboundFileApplyResult.batch_fingerprint` 字段 + `error: Option<InboundFileError>` 类型 + `apply_inbound_files_task` / `apply_files_inner_returning_path` 调用点签名一致
- **✅ 完全符合** `BackendCmd::SetFiles { files, reply }` 变体定义（`src/service.rs:4733-4756`）+ poller 双分支（backend-absent: `src/service.rs:5118-5127`；backend-present: `src/service.rs:5275-5281`）+ main task send（`src/service.rs:4073-4078`）三方接口一致
- **✅ 完全符合** `Sender<oneshot::Result<Result<(), ClipboardError>>>` reply channel 类型在 4 处（trait default / cmd variant / poller backend-absent / backend-present / main task send）一致
- **✅ 完全符合** `Service::inject_to_clipboard()` getter（4.1 落地）→ `decide_reinject_skip` 函数参数 `inject_to_clipboard: bool` → `maybe_inject_files_to_clipboard` 内部调用（4.3 接线）三层传递
- **✅ 完全符合** pre-stamp 模式对称：outbound 端 ExceedsLimit arm（commit `d6fb1d8`）+ inbound 端 re-inject（4.3）同一 pattern
- **⚠️ 小观察**（非 M4 bug）：`ReinjectSkipReason::NoLandedPaths` 与实现路径轻微不一致（如上 P2 所述）
- **⚠️ 小观察**（非 M4 bug）：`handle_clipboard_inbound_files_decide` 函数的 doc-comment 自相矛盾（如上 P2 所述）

## 5. 总体结论

- **接受**
- 理由：
  1. M4 里程碑交付 4 项全部到位：IPC `ClipboardConfig` 8 字段扩展 + drop `auto_accept_files`（4.1）+ `ClipboardBackend::set_files` trait + macOS/Windows/Linux 三平台实现（4.2）+ 剪贴板回灌 + 4 类 skip conditions + pre-stamp 防回环（4.3）+ TOML `[clipboard]` 段加 `inject_to_clipboard`（4.1）
  2. 跨 STEP 接口一致性 ✅（IPC / TOML / Service getter / trait 签名 / BackendCmd variant / pre-stamp pattern）
  3. 现有契约无破坏：M3a `dispatch_files_decide` / `apply_inbound_files_task` / `write_and_verify_file_blocking` / `handle_clipboard_inbound_cancel` / `cancel_mechanism_tests` 全部 0 回归
  4. 0 P0 / 0 P1 / 2 P2（均为 doc-comment 一致性，不影响运行行为）
  5. 累计 ~155 min AI（45 + 55 + 55）—— PLAN 估时 4.5h 内
  6. 闸门全套绿：cargo build / test / clippy / fmt 0 new warning（baseline 24 lib + 28 lib test 全部 pre-existing）；491 lib tests pass（4.2 executor §2.1）+ 344 lib tests pass（4.3 executor §2.1）
  7. PLAN 偏差全部 A1 策略（typed enum / pure decision fn / async fn / failure-also-push / string-prefix classification at boundary / 6 test call-sites / mock→helper / cast_unchecked / image_cache invalidation / 空 slice 防御 / getter 全字段化 + field 删除 / AutoAcceptOff 删除）—— 合理性经 executor 逐条论证

## 6. 建议下一步

1. **M4 → 用户真机验证**（PLAN §8 M4 人类测试矩阵）：
   - macOS Finder 复制文件 → 对端自动落盘 + 入剪贴板 → Cmd+V 直接粘贴（双向）
   - Windows / Linux 真机同样跑双向
   - 关掉回灌：GeneralPanel `inject_to_clipboard = false`（待 5.4）→ 复制文件 → 落盘但不入剪贴板
2. **P2 follow-up**（leader 决策是否 executor 返工清理）：
   - `src/service.rs:5897-5901` doc-comment 自相矛盾清理
   - `src/service.rs:6481, 4051-4059` `NoLandedPaths` 死代码清理（移除或构造返回）
3. **M5 启动**：用户真机验证 PASS 后，leader 派 `plan-step-executor` 启动 M5 STEP-5.1（拔网处理 + `FrontendEvent::FileTransferFailed` + `.partial` 默认删除）。M5 接力点：
   - `Service::keep_partial()` getter 已就位（4.1）
   - `InboundFileError::PartialResidue` enum 变体已 enum 化（4.3）—— M5 STEP-5.1 接 `.partial` cleanup 时可直接 enum-match
   - `handle_inbound_files_applied` 失败路径已 log warn + error typed（4.3）—— M5 扩展 FileTransferFailed IPC 事件零成本
4. **M4 收尾 commit**（leader 已做）：本批 11 commit（`05035d8`, `ab40836`, `f287c67`, `4905dde`, `8e98c21`, `55c3c10`, `ba21e60`, `0b08010`, `a8db685`, `4d573dd`, `058e9fc`）+ 若干 leader-state sync + docs）—— 已落地
