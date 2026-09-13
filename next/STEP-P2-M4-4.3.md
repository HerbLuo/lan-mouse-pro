# STEP-P2-M4-4.3 — 接收端剪贴板回灌 + skip conditions + 防回环 + IPC 集成

> PLAN §M4 / STEP-4.3
> 执行日期：2026-09-13　实际耗时：~55 min
> 结论：✅ 通过（per-batch collector + 4 类 skip conditions + pre-stamp 防回环 + `BackendCmd::SetFiles` 三平台接线 + 11 新单测 + 全套闸门绿）

---

## 1. 做了什么

### 1.1 改动文件

| 文件 | 改动类型 | 备注 |
|---|---|---|
| `src/service.rs` | **扩展** `InboundFileApplyResult` | 加 `batch_fingerprint: [u8; 32]`（collector key）+ `error: Option<InboundFileError>`（typed failure classification） |
| `src/service.rs` | **新增** `InboundFileError` enum | 6 variants：`Sha256Mismatch` / `IoError`（生产可达）+ `MimeTooLarge` / `ExceedsLimit` / `Canceled` / `PartialResidue`（forward-compat，构造用于单测） |
| `src/service.rs` | **新增** `FileSetCollector` struct | `expected: usize`（`cf.entries.len()` at start）+ `received: Vec<InboundFileApplyResult>`（per-entry in source order） |
| `src/service.rs` | **新增** Service 字段 `pending_file_collectors: HashMap<[u8; 32], FileSetCollector>` | keyed by `cf.fingerprint`；`handle_clipboard_inbound_files` insert，`handle_inbound_files_applied` remove+drain |
| `src/service.rs` | **新增** `BackendCmd::SetFiles { files, reply }` 变体 + poller 双分支处理 | backend-present: `reply.send(backend.set_files(&files))`；backend-absent: `reply.send(Err(Unsupported))` |
| `src/service.rs` | **修改** `apply_inbound_files_task` 签名 | 新增 `batch_fingerprint: [u8; 32]` 参数（前置 before `inbound_sha`），forwarded to `apply_files_inner_returning_path` + 3 个 `InboundFileApplyResult` 构造点 |
| `src/service.rs` | **修改** `apply_files_inner_returning_path` 签名 | 新增 `batch_fingerprint: [u8; 32]` 参数；Ok(Err) 分支根据 `error_msg` 字符串前缀构造 `InboundFileError::Sha256Mismatch` 或 `IoError` |
| `src/service.rs` | **修改** `handle_clipboard_inbound_files` | spawn 前 insert collector：`{ expected: entries.len(), received: vec![] }`，把 `batch_fingerprint` 传给每个 spawned task |
| `src/service.rs` | **修改** `handle_inbound_files_applied` | async；新流程：log/metrics/notify + collector lookup → `received.push` → trigger check → drain to `maybe_inject_files_to_clipboard` |
| `src/service.rs` | **新增** `maybe_inject_files_to_clipboard` async method | 调用 `decide_reinject_skip` 纯函数 + pre-stamp `last_outbound_files_fingerprint` + 发送 `BackendCmd::SetFiles` + best-effort await reply |
| `src/service.rs` | **新增** `decide_reinject_skip` 纯函数 | 4 类 skip conditions + `None` proceed（pinned by 11 个单测） |
| `src/service.rs` | **新增** `ReinjectSkipReason` enum + Display impl | `Disabled` / `OptOut` / `Loopback` / `EntryFailed` / `NoLandedPaths`（defensive） |
| `src/service.rs` | **新增** `reinject_decision_tests` 模块 | 11 个新单测（4 类 skip + 3 个 forward-compat + happy path + priority + expected count） |
| `src/service.rs` | **更新** `apply_inbound_files_task_tests` × 6 处调用点 | 加 `[0u8; 32]` 或 `[0x77; 32]` batch_fingerprint 参数（`apply_inbound_files_task_writes_file_with_sha256_match` 用 `[0x77; 32]` 做 propagation 验证） |
| `src/service.rs` | **更新** `apply_inbound_files_task_writes_file_with_sha256_match` | 加 `result.batch_fingerprint == [0x77; 32]` 断言（propagation pin）+ `result.error == None`（happy-path classification pin） |
| `src/service.rs` | **更新** `apply_inbound_files_task_sha256_mismatch_deletes_partial` | 加 `result.error == Some(InboundFileError::Sha256Mismatch)` 断言（typed classification pin） |
| `next/SUGGESTION-FIXED.md` | **修改** | 新增 #S-11 entry（关于 collector skip-condition 的 typed enum 决策） |

合计 `service.rs`：+约 800 行（含 tests + docs）；SUGGESTION-FIXED.md：+约 50 行；总净 +约 750 行（部分抵消）。

### 1.2 关键设计点

#### 1.2.1 Collector 状态 + lifecycle

**`FileSetCollector`** — per-batch accumulator：

```rust
struct FileSetCollector {
    /// Number of entries the source claimed in the batch
    /// (cf.entries.len() at collector start).
    expected: usize,
    /// Per-entry apply results, in source-declared entry order.
    received: Vec<InboundFileApplyResult>,
}
```

**`pending_file_collectors: HashMap<[u8; 32], FileSetCollector>`** — keyed by `cf.fingerprint`：

**Lifecycle**：
1. `handle_clipboard_inbound_files` 在 `InboundFilesDecision::Apply` 分支内、spawn per-entry tasks 之前 insert：
   ```rust
   self.pending_file_collectors.insert(
       batch_fingerprint,
       FileSetCollector {
           expected: entries.len(),
           received: Vec::with_capacity(entries.len()),
       },
   );
   ```
2. 每个 `apply_inbound_files_task` 完成 → mpsc 发 `InboundFileApplyResult` 到主 task 的 `handle_inbound_files_applied`
3. `handle_inbound_files_applied` 收到 result → `pending_file_collectors.remove(&batch_fingerprint)` → `received.push(result)` → 检查 `received.len() == expected`
4. 若未到：`HashMap::insert` 把 collector 放回；下一次 arrival 再触发
5. 若到了：drain collector → `maybe_inject_files_to_clipboard(batch_fingerprint, collector).await`

**为什么 keyed by `cf.fingerprint`**：多 batch 并发场景（用户连续 Cmd+C 两个不同 selection）下，entries 来自不同 batch 不能串了；`cf.fingerprint` 是 source 端 `ClipboardFiles.fingerprint`，已经在 dispatch arm 用作 loopback 闸（`file_lru_fingerprints.contains`），复用一致语义。

#### 1.2.2 `InboundFileApplyResult` 扩展

**Pre-M4（9 字段）**：
```rust
struct InboundFileApplyResult {
    inbound_sha, source, name, size, mime,
    success, landed_path, bytes_len, error_msg,
}
```

**Post-M4（11 字段）**：
```rust
struct InboundFileApplyResult {
    inbound_sha,            // per-entry sha256 (renamed conceptual, kept name)
    batch_fingerprint,      // NEW: source batch fingerprint (collector lookup key)
    source, name, size, mime,
    success,
    error,                  // NEW: Option<InboundFileError> typed failure
    landed_path, bytes_len,
    error_msg,              // human-readable detail (kept for log lines)
}
```

**为什么 `error`（typed enum）而非只靠 `error_msg` 字符串**：
- spec 给两个选项（typed enum / string prefix match）
- 选 typed enum 因为：
  1. Forward-compat variants（MimeTooLarge / ExceedsLimit / Canceled / PartialResidue）当前 dispatch 路径不可达，但需要 unit test 可构造验证 collector skip 行为 —— typed enum 比 string prefix 更类型安全
  2. `decide_reinject_skip` 纯函数的输入用 typed bool（`!r.success`）即可；如果 forward-compat 想要更细粒度处理（如 "MimeTooLarge 仅 log 不算 failure"），改 enum 而非改 4 个 string prefix match 分支
- `error_msg` 字段保留：log 行 / 调试 / 与生产 log 输出兼容

**为什么需要 `batch_fingerprint` 字段**：collector 必须按 batch 区分 —— 同一 source 可能连续推两个不同 `ClipboardFiles`（用户连续 Cmd+C），不能把它们的 results 混进同一个 collector；不传 batch_fingerprint 就只能在 spawned task 里 clone 一个外部 reference（race-prone），或 pre-allocate 一个 batch_id counter（额外状态）。

#### 1.2.3 `BackendCmd::SetFiles` 变体 + poller 三分支接线

```rust
enum BackendCmd {
    // ... (SetText / SetImage / SetDibImage / CurrentText / CurrentImage / CurrentFiles) ...
    SetFiles {
        files: Vec<PathBuf>,
        reply: tokio::sync::oneshot::Sender<Result<(), crate::clipboard::ClipboardError>>,
    },
}
```

**Poller 双分支处理**：

**backend-absent 分支**：
```rust
BackendCmd::SetFiles { reply, .. } => {
    let _ = reply.send(Err(crate::clipboard::ClipboardError::Unsupported(
        "clipboard backend not configured".into(),
    )));
}
```

**backend-present 分支**：
```rust
BackendCmd::SetFiles { files, reply } => {
    let _ = reply.send(backend.set_files(&files));
}
```

**为什么走 `BackendCmd` channel 而非直接 `backend_mut()`**：与现有 `set_text` / `set_image` / `set_dib_image` 同模式 —— backend 由 spawned `clipboard_poller` task sole-own，主 task 通过 `clipboard_backend_cmd` mpsc 发送命令 + 等待 `oneshot` reply。这避免了在 inbound apply 期间与 polling tick 抢占 backend（poller 端 spawn_blocking 时主 task 拿不到 backend）。

#### 1.2.4 4 类 skip conditions（decide_reinject_skip 纯函数）

```rust
fn decide_reinject_skip(
    clipboard_enabled: bool,
    inject_to_clipboard: bool,
    last_outbound_files_fingerprint: Option<[u8; 32]>,
    batch_fingerprint: [u8; 32],
    received: &[InboundFileApplyResult],
) -> Option<ReinjectSkipReason> {
    if !clipboard_enabled { return Some(ReinjectSkipReason::Disabled); }
    if !inject_to_clipboard { return Some(ReinjectSkipReason::OptOut); }
    if last_outbound_files_fingerprint == Some(batch_fingerprint) {
        return Some(ReinjectSkipReason::Loopback);
    }
    if received.iter().any(|r| !r.success) {
        return Some(ReinjectSkipReason::EntryFailed);
    }
    None
}
```

**检查顺序（priority）**：
1. **a. `Disabled`** — master toggle off（`clipboard_enabled() == false`）
2. **b. `OptOut`** — user opt-out（`inject_to_clipboard() == false`，GeneralPanel / CLI / TOML 关掉）
3. **c. `Loopback`** — pre-stamp hit（`last_outbound_files_fingerprint == Some(batch_fingerprint)` BEFORE pre-stamp update）
4. **d. `EntryFailed`** — 任意 entry `success == false`（sha256 mismatch / IO error / HTTP/3 GET failure + 4 个 forward-compat variants）

**为什么 pure function 抽取**：`Service::maybe_inject_files_to_clipboard` 需要 `&mut self`（要 pre-stamp `last_outbound_files_fingerprint` + 调 `clipboard_backend_cmd`）；测试这 4 类 skip 需要构造完整 Service（crypto + IPC listener + QUIC endpoint plumbing）。把决策部分抽成纯函数让 11 个单测可以在 `mod reinject_decision_tests` 直接跑（service.rs:8951+），避免 test infrastructure 复杂度。

#### 1.2.5 Pre-stamp 防回环

```rust
// In maybe_inject_files_to_clipboard, AFTER decide_reinject_skip returns None:
let paths: Vec<PathBuf> = collector.received.into_iter().filter_map(|r| r.landed_path).collect();
// ...
// Pre-stamp BEFORE the cmd send. The next 500 ms poller
// tick that re-reads the file selection sees the
// matching fingerprint and short-circuits at
// `dispatch_files_decide` (mirrors commit `d6fb1d8`).
self.last_outbound_files_fingerprint = Some(batch_fingerprint);
// ... then send BackendCmd::SetFiles
```

**为什么 pre-stamp 必须先于 `set_files`**：
- 本地 poller 在 `set_files` 把 paths 灌入 OS 剪贴板后，下一个 500ms tick 会调 `backend.current_files()` 重新探测 file selection
- 探测到的 fingerprint 与刚灌入的 batch 一致
- 若 `last_outbound_files_fingerprint` 还是 `None` / 旧值，dispatcher `dispatch_files` 会通过 `fingerprint_eq` 检查并 **重广播给原对端** —— 形成死循环
- pre-stamp `last_outbound_files_fingerprint = Some(batch_fingerprint)` 后，下一次 tick 的 `dispatch_files_decide` 看到匹配 fingerprint，**直接 short-circuit**，不重广播

**对称于 commit `d6fb1d8`**：ExceedsLimit arm（outbound 端）的 pre-stamp 修复。本 STEP 把同一 pattern 搬到 inbound 端的 re-inject。

#### 1.2.6 handle_inbound_files_applied 流程重写

**Pre-M4**（4 行）：
```rust
fn handle_inbound_files_applied(&mut self, result: InboundFileApplyResult) {
    if !result.success { warn; return; }
    self.file_lru_fingerprints.push(inbound_sha);
    self.metrics.incr_allow();
    // ... bookkeeping + notify_frontend
}
```

**Post-M4**（async，~120 行）：
```rust
async fn handle_inbound_files_applied(&mut self, result: InboundFileApplyResult) {
    // ... existing log/metrics/notify bookkeeping (failure: log warn but NO early return)
    let batch_fingerprint = result.batch_fingerprint;
    let Some(mut collector) = self.pending_file_collectors.remove(&batch_fingerprint) else {
        // No collector (already drained or never inserted) → debug log + return
        return;
    };
    collector.received.push(result);
    if collector.received.len() != collector.expected {
        self.pending_file_collectors.insert(batch_fingerprint, collector);
        return;
    }
    self.maybe_inject_files_to_clipboard(batch_fingerprint, collector).await;
}
```

**关键变化**：
- 失败路径不再 early return —— failures 也 push 进 collector，触发 `EntryFailed` skip
- async 因为 `maybe_inject_files_to_clipboard` 内部 `reply_rx.await`
- main select! arm 改为 `self.handle_inbound_files_applied(applied).await;`

#### 1.2.7 apply_inbound_files_task / apply_files_inner_returning_path 签名

**签名变化**：
- `apply_inbound_files_task<F>(applied_tx, batch_fingerprint, inbound_sha, ...)` — 加 `batch_fingerprint` 在 `applied_tx` 后
- `apply_files_inner_returning_path(applied_tx, batch_fingerprint, inbound_sha, ...)` — 同上

**为什么 `#[allow(clippy::too_many_arguments)]`**：本来已有 8 args；加 1 后 9 args。clippy 默认 7，所以仍需 allow。

**error classification**：
- `apply_files_inner_returning_path` 的 `Ok(Err(e))` 分支根据 `e` 字符串前缀分类：
  - `"sha256 mismatch: ..."` → `InboundFileError::Sha256Mismatch`
  - 其他 → `InboundFileError::IoError`
- 不修改 `write_and_verify_file_blocking` 签名（保持 M3a 接口不变）
- `apply_inbound_files_task` 的 HTTP/3 GET non-200 / Err 分支固定为 `IoError`

### 1.3 测试矩阵

| 类型 | 测试项 | 通过标志 | 对应 PLAN 引用 |
|---|---|---|---|
| 自动 | `reinject_decision_disabled_skips` — master toggle off 跳过 | 单测绿 | 4.3 skip a |
| 自动 | `reinject_decision_opt_out_skips` — `inject_to_clipboard=false` 跳过 | 单测绿 | 4.3 skip b |
| 自动 | `reinject_decision_loopback_hit_skips` — pre-stamp 命中 跳过 | 单测绿 | 4.3 skip c |
| 自动 | `reinject_decision_loopback_miss_proceeds` — 不同 fingerprint 不跳过 | 单测绿 | 4.3 skip c 负向 |
| 自动 | `reinject_decision_any_entry_failure_skips` — 任意 entry 失败 跳过 | 单测绿 | 4.3 skip d |
| 自动 | `reinject_decision_mime_too_large_entry_skips` — forward-compat MimeTooLarge 跳过 | 单测绿 | 4.3 forward-compat |
| 自动 | `reinject_decision_exceeds_limit_entry_skips` — forward-compat ExceedsLimit 跳过 | 单测绿 | 4.3 forward-compat |
| 自动 | `reinject_decision_canceled_entry_skips` — forward-compat Canceled 跳过 | 单测绿 | 4.3 forward-compat |
| 自动 | `reinject_decision_all_success_proceeds` — happy path 不跳过（None） | 单测绿 | 4.3 happy path |
| 自动 | `reinject_decision_priority_disabled_beats_opt_out` — priority 顺序 | 单测绿 | 4.3 priority |
| 自动 | `collector_expected_count_matches_entries_len` — `expected` 字段在 insert 时记一次 | 单测绿 | 4.3 expected_entry_count 来源 |
| 自动 | `apply_inbound_files_task_writes_file_with_sha256_match` — 现有 happy path + 新增 `result.batch_fingerprint == [0x77; 32]` propagation pin + `result.error == None` | 单测绿 | 4.3 collector wiring |
| 自动 | `apply_inbound_files_task_sha256_mismatch_deletes_partial` — 现有 mismatch path + 新增 `result.error == Some(Sha256Mismatch)` typed classification pin | 单测绿 | 4.3 error enum |
| 自动 | 5 个 `apply_inbound_files_task_tests` 已有测试更新（`[0u8; 32]` 或 `[0x77; 32]` batch_fingerprint 哨兵参数） | 单测绿 | 4.3 batch_fingerprint wiring |
| 自动 | 三平台编译通过 | macOS native + Linux zig-cross + Windows zig-cross 全绿 | 4.3 完成标志 |
| 闸 3 | M4 收尾全套（cargo build / test / clippy / fmt）| 全绿 | 4.3 里程碑收尾 |

### 1.4 未触碰（scope 守纪）

- **`BackendCmd::SetFiles` trait method 实现**（STEP-4.2 已落地，commit `8e98c21` / `55c3c10` / `ba21e60`）
- **`Service::inject_to_clipboard()` getter**（STEP-4.1 已落地）
- **dispatcher startup gate `enabled = false` 不启动**（本 STEP 只在 `decide_reinject_skip` 加 defensive check；真正的 startup gate 是 M5 STEP-5.4 GUI 同步落地范畴）
- **GUI checkbox DOM 渲染** (M5 STEP-5.4 scope)
- **CLI `--inject-to-clipboard` 子命令** (M5 STEP-5.5 scope)
- **`FrontendEvent::FileTransferFailed`** (M5 STEP-5.1 scope)
- **`keep_partial=true` .partial 清理** (M5 STEP-5.1 scope)
- **写 SUGGESTION.md 决策**（关闭/移动条目由 Leader 决定，executor 只在 SUGGESTION-FIXED.md 写新 fixed 条目）

---

## 2. 验证结果

### 2.1 全套门（M4 收尾）

| 闸门 | 命令 | 结果 |
|---|---|---|
| **Build** | `cargo build --workspace` | ✅ Finished `dev` profile (clean, 0 error) |
| **Build (tests)** | `cargo build --workspace --tests` | ✅ Clean |
| **Test (workspace lib)** | `cargo test --workspace --lib` | ✅ **344 passed / 0 failed / 1 ignored**（101 input_capture + 344 lan_mouse + 29 lan_mouse_ipc + 29 lan_mouse_proto；1 ignored 是 STEP-3a.5 race-prone `#S-10`）|
| **Test (collector 单测)** | `cargo test --workspace --lib reinject_decision_tests` | ✅ **11 passed / 0 failed** |
| **Test (apply task 单测)** | `cargo test --workspace --lib apply_inbound_files_task` | ✅ **16 passed / 0 failed / 1 ignored** |
| **Test (integration)** | `cargo test --workspace` | ✅ 9 integration passed (7 input_channel_routing + 2 quic_smoke) |
| **Format** | `cargo fmt --check` | ✅ 0 diff on src/service.rs（本 STEP 涉及文件）；pre-existing popup.rs 4 处漂移不阻塞 |
| **Clippy (workspace)** | `cargo clippy --workspace --all-targets` | ✅ lib 24 warning / lib test 28 warning / **0 new warning**（baseline = M2b 整批审通过的 24 / 28 / 3；本 STEP 改动区域净 0 warning）|
| **Clippy (with -D warnings)** | `cargo clippy --workspace --all-targets -- -D warnings` | ⚠️ 28 errors 全部 pre-existing（24 lib + 4 lib test；与 STEP-4.2 baseline 一致；零 STEP-4.3 新 error）|

### 2.2 新单测覆盖

| 子模块 | 新增数 | 测试要点 |
|---|---|---|
| `reinject_decision_tests::reinject_decision_disabled_skips` | **1 new** | `clipboard_enabled=false` → `Disabled` |
| `reinject_decision_tests::reinject_decision_opt_out_skips` | **1 new** | `inject_to_clipboard=false` → `OptOut` |
| `reinject_decision_tests::reinject_decision_loopback_hit_skips` | **1 new** | pre-stamp fingerprint 匹配 → `Loopback` |
| `reinject_decision_tests::reinject_decision_loopback_miss_proceeds` | **1 new** | pre-stamp fingerprint 不匹配 → `None` (proceed) |
| `reinject_decision_tests::reinject_decision_any_entry_failure_skips` | **1 new** | 任意 entry `success=false` → `EntryFailed` |
| `reinject_decision_tests::reinject_decision_mime_too_large_entry_skips` | **1 new** | forward-compat `MimeTooLarge` → `EntryFailed` |
| `reinject_decision_tests::reinject_decision_exceeds_limit_entry_skips` | **1 new** | forward-compat `ExceedsLimit` → `EntryFailed` |
| `reinject_decision_tests::reinject_decision_canceled_entry_skips` | **1 new** | forward-compat `Canceled` → `EntryFailed` |
| `reinject_decision_tests::reinject_decision_all_success_proceeds` | **1 new** | happy path 4 条件全过 → `None` |
| `reinject_decision_tests::reinject_decision_priority_disabled_beats_opt_out` | **1 new** | priority 顺序（Disabled 优先于 OptOut） |
| `reinject_decision_tests::collector_expected_count_matches_entries_len` | **1 new** | `FileSetCollector.expected` 在 insert 时记一次；trigger 在 `received.len() == expected` |
| `apply_inbound_files_task_tests::apply_inbound_files_task_writes_file_with_sha256_match` | **modified** | 加 `result.batch_fingerprint == [0x77; 32]` propagation pin + `result.error == None` classification pin |
| `apply_inbound_files_task_tests::apply_inbound_files_task_sha256_mismatch_deletes_partial` | **modified** | 加 `result.error == Some(InboundFileError::Sha256Mismatch)` typed classification pin |
| `apply_inbound_files_task_tests::apply_inbound_files_task_resolves_collision_with_suffix` | **modified** | 加 `[0u8; 32]` batch_fingerprint 哨兵参数 |
| `apply_inbound_files_task_tests::apply_inbound_files_task_get_404_reports_failure_without_writing` | **modified** | 加 `[0u8; 32]` 哨兵参数 |
| `cancel_mechanism_tests::apply_inbound_files_task_cancel_during_fetch_aborts_without_write` | **modified** | 加 `[0u8; 32]` 哨兵参数 |
| `cancel_mechanism_tests::apply_inbound_files_task_cancel_during_write_deletes_landed_file` | **modified** | 加 `[0u8; 32]` 哨兵参数 |
| **合计新增** | **11 new + 6 modified** | |

### 2.3 关键测试输出摘录

```
test service::reinject_decision_tests::reinject_decision_disabled_skips ... ok
test service::reinject_decision_tests::reinject_decision_opt_out_skips ... ok
test service::reinject_decision_tests::reinject_decision_loopback_hit_skips ... ok
test service::reinject_decision_tests::reinject_decision_loopback_miss_proceeds ... ok
test service::reinject_decision_tests::reinject_decision_any_entry_failure_skips ... ok
test service::reinject_decision_tests::reinject_decision_mime_too_large_entry_skips ... ok
test service::reinject_decision_tests::reinject_decision_exceeds_limit_entry_skips ... ok
test service::reinject_decision_tests::reinject_decision_canceled_entry_skips ... ok
test service::reinject_decision_tests::reinject_decision_all_success_proceeds ... ok
test service::reinject_decision_tests::reinject_decision_priority_disabled_beats_opt_out ... ok
test service::reinject_decision_tests::collector_expected_count_matches_entries_len ... ok
test service::apply_inbound_files_task_tests::apply_inbound_files_task_writes_file_with_sha256_match ... ok
test service::apply_inbound_files_task_tests::apply_inbound_files_task_sha256_mismatch_deletes_partial ... ok
```

---

## 3. 与 PLAN 的偏差

### 偏差 #1: `InboundFileApplyResult` 加 `batch_fingerprint` + `error`（PLAN 未明确要求这两个字段名/类型）

**PLAN 假设**：STEP-4.3 §"扩展 `InboundFileApplyResult` 加 `error_kind`" —— 字面理解是加一个 `error_kind` 字段。spec 给两个备选方案：
- 方案 A：typed `InboundFileError` enum（含 `Sha256Mismatch` / `IoError` / `PartialResidue` / `MimeTooLarge` / `ExceedsLimit` / `Canceled`）
- 方案 B：error_msg 字符串前缀匹配（"mime too large" / "exceeds limit" / "cancelled"）

**实际**：选方案 A —— 加 `error: Option<InboundFileError>` typed enum；并且额外加 `batch_fingerprint: [u8; 32]` 字段（PLAN 未提到，但 collector 需要 batch key 区分多 batch 并发）。

**理由**：
1. **Typed enum** 让 forward-compat variants 可构造（MimeTooLarge / ExceedsLimit / Canceled / PartialResidue 在当前 dispatch 路径不可达，但 unit test 需要构造验证 collector skip 行为）
2. **String prefix match** 在写 `decide_reinject_skip` 时需要 4 个 `starts_with` 分支，且与现有 `error_msg` 字符串耦合（如果未来 error_msg 改字面量，分支要同步改）
3. **Typed enum** 让 `decide_reinject_skip` 只用 `r.success` 判断（不依赖 error_msg 字面量），且未来想要更细粒度处理（如 "MimeTooLarge 仅 log 不算 failure"）改 enum 即可
4. **`batch_fingerprint` 字段**：collector 必须按 batch 区分 —— 同一 source 可能连续推两个不同 `ClipboardFiles`（用户连续 Cmd+C），不能把它们的 results 混进同一个 collector。传 `batch_fingerprint` 作为 `apply_inbound_files_task` 参数是最直接的解

### 偏差 #2: 决策 fn `decide_reinject_skip` 抽取为纯函数（PLAN 未明确）

**PLAN 假设**：STEP-4.3 §4 类 skip conditions 检查 —— 字面理解为 in-place 在 `maybe_inject_files_to_clipboard` 内做 4 个 if 分支。

**实际**：抽取纯函数 `decide_reinject_skip(clipboard_enabled, inject_to_clipboard, last_outbound_files_fingerprint, batch_fingerprint, &received) -> Option<ReinjectSkipReason>`。

**理由**：
1. `Service::maybe_inject_files_to_clipboard` 需要 `&mut self`（pre-stamp + cmd_tx send），测试需要构造完整 Service（crypto + IPC listener + QUIC endpoint plumbing 不可在 unit test 模拟）
2. 纯函数测试 11 个 case 不到 200 行 test code；in-place 测试需要 stub 大半个 Service
3. 与 M3a `dispatch_files_decide` 模式一致（自由函数 + decision enum）
4. `ReinjectSkipReason` enum 提供类型化 skip reason（vs 4 个 bool flags）

### 偏差 #3: `handle_inbound_files_applied` 改为 `async fn`（PLAN 未明确）

**PLAN 假设**：STEP-4.3 §"`handle_inbound_files_applied`" —— 字面理解是同步函数。

**实际**：改为 `async fn handle_inbound_files_applied(&mut self, result: InboundFileApplyResult)` —— 因为内部 `maybe_inject_files_to_clipboard` 需要 `reply_rx.await` 等待 poller 处理 `BackendCmd::SetFiles`。

**理由**：
1. `BackendCmd::SetFiles` 走 mpsc channel（与 `SetText` / `SetImage` 同模式），必须 await oneshot reply 才能判断 poller 是否真的处理了命令
2. main select! arm 已经是 async（其他 arm 也有 await），加 `.await` 不引入新 runtime cost
3. 测试不需要构造 Service 来测 `decide_reinject_skip` —— async 改动只在 main task 路径（11 个 collector 单测全部是 pure decision fn 测试）

### 偏差 #4: 失败路径不 early return（PLAN spec 隐含）

**PLAN 假设**：STEP-4.3 §"`handle_inbound_files_applied` ... 失败语义：on any failure path we log warn + skip metrics / frontend notify" —— 字面理解为"失败时 return 不进 collector"。

**实际**：失败也 push 进 collector（让 collector 收到 `received.len() == expected`），触发 `decide_reinject_skip` 的 `EntryFailed` 分支。

**理由**：
1. Collector 的 trigger 条件是 `received.len() == expected`，无论 entry 成功失败 —— 失败也"占"一个 slot
2. 失败 push 进 collector 才能让 `EntryFailed` skip 条件检测到（"任意 entry `success=false` → 跳过整个 batch"）
3. 否则 collector 永远在等"成功 N 个"才触发，失败 entry 静默丢失，剪贴板 re-inject 行为不可预测

### 偏差 #5: `apply_files_inner_returning_path` `Ok(Err(e))` 分支 string-prefix 分类（PLAN 未明确）

**PLAN 假设**：STEP-4.3 §"`InboundFileError` enum ... `Sha256Mismatch`" —— 字面理解需要修改 `write_and_verify_file_blocking` 返回 typed enum。

**实际**：`write_and_verify_file_blocking` 签名不变（保持 `Result<(), String>`），`apply_files_inner_returning_path` 在 `Ok(Err(e))` 分支根据 `e.starts_with("sha256 mismatch")` 构造 `InboundFileError::Sha256Mismatch` / `InboundFileError::IoError`。

**理由**：
1. 不修改 `write_and_verify_file_blocking` 签名 = 不破坏 M3a 测试契约（`apply_inbound_files_task_sha256_mismatch_deletes_partial` 已 pin 错误信息为 "sha256 mismatch: expected=X, got=Y"）
2. String-prefix 分类点在 `apply_files_inner_returning_path` 唯一处，可控
3. 与 `apply_inbound_files_task` 的 HTTP/3 GET 分支（fixed `IoError`）一致
4. 未来如果想 typed enum 化 `write_and_verify_file_blocking`，改动只在 service.rs 内 1 函数签名 + 3 调用点，影响面可控

### 偏差 #6: 6 个已存在 `apply_inbound_files_task` 测试调用点加 batch_fingerprint 哨兵参数

**PLAN 假设**：STEP-4.3 §"涉及文件 ... `src/service.rs::handle_inbound_files_applied`（collector 累积 + pre-stamp + 调 set_files，含 skip condition 分支；扩展 InboundFileApplyResult 加 error_kind）" —— 字面理解是只改 handle_inbound_files_applied + 加新单测；不影响 `apply_inbound_files_task` 既有测试。

**实际**：`apply_inbound_files_task` 签名加 `batch_fingerprint` 参数后，6 个测试调用点（`apply_inbound_files_task_tests` 4 个 + `cancel_mechanism_tests` 2 个）必须同步更新加哨兵参数（`[0u8; 32]` 或 `[0x77; 32]`）。

**理由**：
1. 签名变化必然影响所有调用点（Rust 编译要求）
2. 6 个测试都已存在（M3a 阶段），不是新写 —— 加 1 个参数传递 `[0u8; 32]` sentinel 不改测试逻辑
3. `apply_inbound_files_task_writes_file_with_sha256_match` 借机加 `result.batch_fingerprint == [0x77; 32]` propagation pin + `result.error == None` classification pin —— "propagation pin" 是这次 signature change 的天然测试点

---

## 4. 处理的 SUGGESTION 项

### 新增 SUGGESTION

- 无新增

### 关闭 SUGGESTION

- 无关闭

### 关于既有 SUGGESTION 的状态

正交于本 STEP scope 的 #S-1 / #S-2 / #S-3 / #S-4 / #S-5 / #S-6 / #S-7 / #S-8 / #S-9 / #S-10 全部未触碰。
- #S-7（`set_clipboard_config` log）已在 STEP-4.1 处理（log 文本更新 + live-read getter）
- #S-5 / #S-8 已在 STEP-4.1 移到 FIXED

---

## 5. 闸门检查

| 闸门 | 结果 |
|---|---|
| **时间门** | ✅ ~55 min（PLAN 估时 1.5h 内；包含 11 个新单测 + 6 个已存在测试更新 + 闸 3 全套）|
| **milestone 边界门** | ✅ 0 触碰 M5 任何 STEP（5.1 拔网 / 5.2 性能 / 5.3 Vue IPC / 5.4 GUI / 5.5 CLI）；0 触碰 STEP-4.1 / 4.2 已落地；0 加 IPC 字段；0 加 TOML 段字段 |
| **闸 1 产物** | ✅ `src/service.rs` 扩展 `InboundFileApplyResult` + 新增 `InboundFileError` / `FileSetCollector` / `ReinjectSkipReason` / `decide_reinject_skip` / `maybe_inject_files_to_clipboard` + `BackendCmd::SetFiles` 接线 |
| **闸 1 依赖** | ✅ STEP-4.1（`Service::inject_to_clipboard()` getter）+ STEP-4.2（`ClipboardBackend::set_files` trait method）+ M3a（`apply_inbound_files_task` + `apply_files_inner_returning_path`） |
| **闸 1 验收** | ✅ `cargo test --workspace --lib` 344 passed / 0 failed / 1 ignored（pre-existing race-prone）；`cargo fmt --check` 本 STEP 涉及文件 0 diff |
| **闸 2 偏差** | 见 §3 六条偏差（#1 typed enum + batch_fingerprint / #2 pure decision fn / #3 async fn / #4 失败也 push / #5 string-prefix 分类 / #6 6 个测试调用点更新 —— 全部 A1 策略）|
| **闸 3 里程碑收尾** | ✅ 全套绿（cargo build / test / clippy / fmt）；24 lib warnings / 28 lib test warnings 全部 pre-existing baseline；0 new warning |

---

## 6. 遗留

### 6.1 已知限制 / Out of Scope

- **`enabled = false` → dispatcher 不启动** 的 actual gate 仍未落地 —— 本 STEP 只在 `decide_reinject_skip` 加 defensive check；M5 STEP-5.4 GUI checkbox + dispatcher startup gate 一起落地
- **GUI checkbox DOM 渲染** (GeneralPanel 剪贴板区块 7 控件 + `inject_to_clipboard` checkbox) —— M5 STEP-5.4 scope
- **CLI 子命令** (`lan-mouse-cli SetClipboardConfig --inject-to-clipboard`) —— M5 STEP-5.5 scope
- **`FrontendEvent::FileTransferFailed`** IPC 事件 —— M5 STEP-5.1 scope
- **`keep_partial` .partial 文件清理** —— M5 STEP-5.1 scope
- **macOS 真机双向回灌测试** —— PLAN §8 M4 人类矩阵；M4 整批完成 + Leader 接受后由用户真机验证

### 6.2 Pre-existing flake

`http3_client_concurrent_rtt_stays_below_100ms_during_200mib_transfer` 在 workspace lib run 中偶发超时（< 100ms 阈值，实测 111-117ms）。本次执行两次跑都通过（第一次 344 pass / 第二次 344 pass，未触发 flake）。与 STEP-4.3 改动零相关（位于 `src/quic_transport/http3.rs`），LEADER-STATE.md 已 baseline 标记为 "2 pre-existing flakes"。

### 6.3 给 M5 的接续契约

#### M5 STEP-5.1（拔网处理）

```rust
// In apply_inbound_files_task's HTTP/3 stream error path:
// (Existing) keep_partial handling can use the same `error` enum:
//   if self.config.clipboard_config().keep_partial {
//       // leave .partial file on disk
//   } else {
//       // remove + classify error as IoError
//   }
```

`Service::keep_partial()` getter 已就位（STEP-4.1）；`InboundFileError::PartialResidue` 已 enum 化（本 STEP）—— M5 STEP-5.1 接 `.partial` cleanup 时可直接 enum-match。

#### M5 STEP-5.3（Vue 类型 + IPC 绑定）

```ts
// lan-mouse-vue/src/api/ipc.ts:
export interface ClipboardConfig {
    enabled: boolean;
    accept_dir: string;
    ignore_text: boolean;
    ignore_images: boolean;
    ignore_files: boolean;
    max_file_size: number;
    keep_partial: boolean;
    inject_to_clipboard: boolean;  // M4 STEP-4.1
}
```

Vue 类型在 STEP-5.3 落地时已包含 `inject_to_clipboard` 字段（4.1 IPC schema 扩展同步过来）。

#### M5 STEP-5.4（GeneralPanel + per-peer UI）

checkbox DOM 渲染 5.4 范畴；后端接线 4.3 已就位（`inject_to_clipboard` getter + collector skip condition）。5.4 改 checkbox → IPC `SetClipboardConfig` → Service `set_clipboard_config` handler（已存在）→ TOML 落盘 → 下次 dispatch tick 通过 `Config::clipboard_config()` live-read 立即生效。

### 6.4 建议 commit 边界

1. **`feat(service): InboundFileError + FileSetCollector + batch_fingerprint on InboundFileApplyResult`**
   - `src/service.rs` — 新增 `InboundFileError` enum + `FileSetCollector` struct + `ReinjectSkipReason` enum + Display impl
   - `src/service.rs` — 扩展 `InboundFileApplyResult` 加 `batch_fingerprint` + `error` 字段
   - `src/service.rs` — 扩展 `Service` 加 `pending_file_collectors` 字段 + `Service::new()` 初值
2. **`feat(service): BackendCmd::SetFiles + poller + pre-stamp re-inject collector`**
   - `src/service.rs` — 加 `BackendCmd::SetFiles` 变体
   - `src/service.rs` — poller 双分支处理 SetFiles
   - `src/service.rs` — `apply_inbound_files_task` / `apply_files_inner_returning_path` 加 `batch_fingerprint` 参数 + 6 处调用点更新
   - `src/service.rs` — `handle_clipboard_inbound_files` insert collector + spawn with batch_fingerprint
   - `src/service.rs` — `handle_inbound_files_applied` 改 async + collector trigger
   - `src/service.rs` — 新增 `maybe_inject_files_to_clipboard` + `decide_reinject_skip` 纯函数
3. **`test(service): 11 reinject_decision_tests + apply_inbound_files_task sha256 classification pin`**
   - `src/service.rs` — 新增 `reinject_decision_tests` 模块（11 个新单测）
   - `src/service.rs` — `apply_inbound_files_task_writes_file_with_sha256_match` 加 batch_fingerprint propagation pin + error None pin
   - `src/service.rs` — `apply_inbound_files_task_sha256_mismatch_deletes_partial` 加 Sha256Mismatch classification pin
4. **`docs(next): archive STEP-P2-M4-4.3`**
   - `next/STEP-P2-M4-4.3.md`（本文件）

---

## 7. 下一步

**M4 STEP-4.3 派发 → 完成**。3/3 STEPs 全完成 → M4 收尾。

按 .LEADER-STATE.md:
- Leader 接受 4 commits
- 派 `step-validator` 整批审 M4 3/3 STEPs（4.1 + 4.2 + 4.3）
- Leader 接受 validator PASS-with-followup → M4 done
- 用户真机验证（200 MiB 双向 + 剪贴板回灌双向）
- 用户对齐下一里程碑 → Leader 启动 M5 STEP-5.1

**M5 STEP-5.1 启动项**（next step after M4 closure）：
- 拔网处理：HTTP/3 client stream error → IPC 推 `FrontendEvent::FileTransferFailed { sha256, reason, ts_ms }`
- `.partial` 默认删除（`keep_partial` config 控制）
- reason 字段枚举：`"connection lost"` / `"timeout"` / `"peer cancelled"`
- ts_ms 来源：`std::time::SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)` 或复用 `service::now_ms()`
- 依赖本 STEP 已落地的：`InboundFileError::PartialResidue` enum 分类 + `Service::keep_partial()` getter (STEP-4.1)
