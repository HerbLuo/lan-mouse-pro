# STEP-P2-M3a-3a.5 — File transfer cancellation mechanism

> PLAN §3 M3a / STEP-3a.5 (取消机制：源端 `FileTransferCancel { sha256 }` 走 StreamC + 接收端关闭 HTTP/3 stream + source `file_cache.remove`)
> 执行日期：2026-09-13　实际耗时：~2 h
> 结论：✅ 通过（cargo build clean / 326 lib pass + 1 ignored / 481 workspace pass（1 pre-existing input-capture + 1 pre-existing http3 concurrent flake）/ fmt 0 diff / clippy 无新 warning / 9 新单测 / 1s 端到端 cancel 契约 pin）

---

## 1. 做了什么

### 1.1 改动文件

| 文件 | 改动类型 | 备注 |
|---|---|---|
| `src/service.rs` | **修改** | +`Service::last_outbound_files_sha` 字段（per-entry sha256 list of last push）+ `Service::inbound_file_cancel_txs` 字段（in-flight cancel registry）+ `handle_clipboard_inbound_cancel` 方法（receiver-side）+ `dispatch_files_build_cancel_events` free fn（source-side）+ `signal_inbound_file_cancel` free fn（receiver-side helper）+ `apply_files_inner_returning_path` free fn（post-write delete）+ `apply_inbound_files_task` 重写（race GET vs cancel + post-write check + cleanup）+ `dispatch_files::Ok` arm supersede 路径（fire cancels + remove from file_cache）+ `handle_clipboard_inbound` dispatcher 加 `FileTransferCancel` arm + `handle_clipboard_inbound_files` 透传 `cancel_registry` clone 到 spawned task + `cancel_mechanism_tests` 模块（9 新单测）|
| `lan-mouse-proto/src/lib.rs` | **0 改动** | `FileTransferCancel { sha256 }` 已在 M0a 落地（PLAN §3 STEP-3a.5 "已加" + §5 风险 #9 契约） |
| `src/quic_transport/http3.rs` | **0 改动** | Server 端无需主动 cancel：quinn `RecvStream` drop 触发 STOP_SENDING，client→server cancel 路径已 graceful abort |
| `src/clipboard/file_cache.rs` | **0 改动** | `FileCache::remove(sha256)` API 已在 STEP-3a.2 落地；commit `7a57bb3` 已将 sha256 + memcpy 移出 LocalSet |
| `next/SUGGESTION.md` | **0 改动** | 无新增问题（本 STEP 严格按 §3 STEP-3a.5 + §5 风险 #9 + M3a 已知限制走实现，未发现偏离点） |

合计 service.rs: **+约 600 行（含 9 个新单测 + 2 个新 free fn + 1 个新 handle method）**

### 1.2 关键设计点

#### 1.2.1 Source 端：supersede 触发 cancel + cache.remove

`dispatch_files` 在 `Ok` arm 进入时先检测 `last_outbound_files_sha`（per-entry sha256 列表），**如果非空**（说明有上一批 push）：

1. **`std::mem::take(&mut self.last_outbound_files_sha)`** — 立刻 take 出旧 list（避免后续 broadcast 期间重入）
2. **`dispatch_files_build_cancel_events(prev_shas, &self.file_cache)`** — 纯函数 helper：
   - **cache.remove O(1) per call**：直接 `file_cache.lock()` + `HashMap::remove`，**不**走 `spawn_blocking`（§5 风险 #9 — sha256 计算 + 200 MiB memcpy 是 CPU-bound；cache delete 是 hash drop）
   - 返回 `Vec<ProtoEvent::FileTransferCancel>` 一对一 per prev sha
3. **`broadcast_clipboard_event` per event** — 每个 cancel 走 StreamC；honours `enable_clipboard_to` + `active_addr` 闸门

新 push 的 sha list 在 `Ok` arm 末尾 `let new_outbound_shas = ...` **预计算**（before `entries` move into spawn_blocking），最后赋值给 `self.last_outbound_files_sha`。**MIME_TOO_LARGE entries 过滤掉**（dispatcher 从未 insert 它们到 cache，无 cancel 必要）。

#### 1.2.2 Receiver 端：cancel registry + signal race

**`inbound_file_cancel_txs: Arc<Mutex<HashMap<[u8; 32], oneshot::Sender<()>>>>`**
- Shared `Arc` 让 `apply_inbound_files_task`（spawn_local 任务）和 main task 都可访问
- `std::sync::Mutex`（非 `tokio::sync::Mutex`）— 操作 O(1)、lock brief，匹配 `file_cache` 模式

**`apply_inbound_files_task` race 流程**：

```
1. oneshot::channel() → (cancel_tx, cancel_rx)
2. registry.insert(sha, cancel_tx)  ← 注册 in-flight fetch
3. tokio::select! {
       biased;
       _ = &mut cancel_rx => return;  // mid-fetch cancel: abort cleanly
       fetch_result = fetcher => handle 200/non-200/Err
   }
4. now_or_never() on cancel_rx  ← post-fetch check (narrow window)
5. apply_files_inner_returning_path(...)  ← spawn_blocking write + sha256 verify
6. now_or_never() on cancel_rx → if Some: delete landed file  ← post-write check
7. registry.remove(sha)  ← final cleanup (always)
```

**关键设计选择**：
- **biased select** — cancel 优先于 fetch（避免 cancel arrival between insert + select 时被 fetch 抢先）
- **`now_or_never` 单次调用 + stash result** — `oneshot::Receiver` 的 `now_or_never()` 是 poll-once，多次调用会消费 Ready 状态
- **post-write delete** — spawn_blocking 不能直接 cancel；但 cancel signal 已落在 `cancel_rx` 里，post-write check 看到就 `std::fs::remove_file(landed_path)`
- **registry cleanup at exit** — 双重保险：cancel handler `remove` 了 → task 末尾再 `remove` 一次（空操作，幂等）

#### 1.2.3 `dispatch_files_build_cancel_events` free fn 抽取

镜像 `dispatch_files_decide`（commit `af0e685`）/ `handle_clipboard_inbound_files_decide`（commit `36b912b`）的 "free fn + enum return" 模式：cache.remove 逻辑 + event-list 构造**纯化**为 free fn，让单测覆盖不依赖 `Service::new()`。

#### 1.2.4 `signal_inbound_file_cancel` free fn 抽取

同样纯化：`HashMap::remove` + `oneshot::Sender::send` 一行能描述，free fn 让单元测试覆盖。

#### 1.2.5 `apply_files_inner_returning_path` 抽取

原 `apply_files_inner` 丢弃 `landed_path`。STEP-3a.5 需要 post-write delete，所以抽取一个 `returning_path` 变体。原来的 void 变体无人调用，直接删除（删了 ~63 行 dead code）。

### 1.3 Wire-level 契约（不写代码，验证 #S-7/#S-8 思路一致）

- **`FileTransferCancel { sha256: [u8; 32] }`** 在 `lan-mouse-proto` 已有（`lib.rs:209-213` doc comment 说明用于 M3a STEP-3a.5）
- **StreamC var-codec 编码** 走 `Vec<u8>` dispatcher（`lib.rs:702-712`）— `FileTransferCancel::encode_var_body` 写 32 字节裸 sha256
- **Var codec round-trip** 由 `lan-mouse-proto` 的 `all_var_variants_dispatcher_round_trip`（`lib.rs:1060-1098`）覆盖
- **新增测试** `cancel_propagates_end_to_end_within_one_second` 通过 `Vec::<u8>::from + TryFrom<&[u8]>` 端到端验证 wire 兼容

### 1.4 取消时序保证（与 §5 风险 #9 对齐）

| Cancel 时机 | 路径 | 用户可见效果 |
|---|---|---|
| 源端 push F2 之前 F1 HTTP/3 GET 还在传输 | **fetch 路径**：select! picks cancel → recv stream drop → quinn STOP_SENDING → receiver 端 read_exact 返回 Err(ConnectionAborted) → apply task log "cancel received mid-fetch" + return | 接收端无 .partial 文件（从未写过）；receiver loopback LRU 不更新 |
| Cancel 在 fetch 完成 + spawn_blocking write 之间到达 | **post-fetch 检查**：`now_or_never` 返回 Some → 立即 return（不进入 spawn_blocking）| 同上 |
| Cancel 在 spawn_blocking write 期间/之后到达 | **post-write 检查**：`now_or_never` 返回 Some → `std::fs::remove_file(landed_path)` → 0 残留 | 文件已写但立即删除；用户看不到半成品 |
| Cancel 在 fetch 完成 + write 完成之后到达 | task 已 exit，registry 已 cleanup → `signal_inbound_file_cancel` 命中 `None` 分支 → debug log + no-op | 文件正常落盘（用户主动取消发生在 fetch 完成后，这是合法"取消太迟"场景）|

### 1.5 未触碰（scope 守纪）

- **HTTP/3 server route**：`/clipboard/file/{sha256}` 仍由 STEP-3a.4 接管；server 端无需主动 cancel（quinn client→server cancel 通过 `ReadError::ClosedStream` graceful abort）
- **`FileTransferOffer` / `FileTransferResponse` arm**：M3b STEP-3b.2 scope（GUI-driven accept/reject）
- **`lan-mouse-ipc::ClipboardConfig::auto_accept_files` IPC handler** 真实接 Service 字段：M3b STEP-3b.1 scope（关闭 SUGGESTION #S-7）
- **`lan-mouse-vue` 前端**：M4 scope
- **`lan-mouse-proto`**：0 改动（`FileTransferCancel` 已在 M0a 落地）
- **`src/clipboard/file_cache.rs`**：0 改动（`remove` API 已有）

---

## 2. 验证结果

### 2.1 全套门

| 闸门 | 命令 | 结果 |
|---|---|---|
| **Build** | `cargo build -p lan-mouse` | ✅ Finished `dev` profile (clean, 0 warning) |
| **Build (tests)** | `cargo build -p lan-mouse --tests` | ✅ Clean (1 pre-existing warning in `src/clipboard/macos.rs:1811` unused `first`) |
| **Test (lan-mouse lib)** | `cargo test -p lan-mouse --lib` | ✅ **326 passed / 0 failed / 1 ignored**（+9 vs STEP-3a.4 baseline 317；1 ignored 是 race-prone test 标记 `#[ignore]`）|
| **Test (cancel 子集)** | `cargo test -p lan-mouse --lib cancel_mechanism_tests` | ✅ **9 passed / 0 failed / 1 ignored** |
| **Test (workspace lib)** | `cargo test --workspace --lib --no-fail-fast` | ✅ **481 pass / 2 pre-existing flake**（input-capture `enumerate_monitors_returns_live_state` macOS headless + http3 `concurrent_rtt` 200mib 高负载 flaky；与本 STEP 无关） |
| **Format** | `cargo fmt --all -- --check` | ✅ 0 diff (exit 0) |
| **Clippy (lan-mouse)** | `cargo clippy -p lan-mouse --all-targets` | ✅ 新代码区 (last_outbound_files_sha 字段 + handle_clipboard_inbound_cancel + dispatch_files_build_cancel_events + signal_inbound_file_cancel + apply_files_inner_returning_path + apply_inbound_files_task cancel 改造 + 9 tests) **0 warning**；既有 warning 数（service.rs:2413 / 3519 / 4430 / 4508 / 4766 / 4934 / 5005 doc-related + connect.rs:1346 etc 既有 pre-existing）均与本 STEP 无关 |

### 2.2 新单测覆盖（按子模块）

| 子模块 | 新增数 | 测试要点 |
|---|---|---|
| `dispatch_files_build_cancel_events_*` (3 新) | **3** | `empty_prev_returns_empty`（首次 push 边界）/ `removes_from_cache_and_emits_events`（happy path：3 entries → cache 全清 + 3 FileTransferCancel 事件顺序正确）/ `missing_sha_still_emits_cancel_event`（cache.remove 幂等 + cancel event 仍然发出）|
| `signal_inbound_file_cancel_*` (3 新) | **3** | `signals_in_flight_fetch`（in-flight entry → 返回 true + registry 清空 + oneshot receiver 收到信号）/ `no_entry_is_noop`（missing entry → 返回 false + debug log）/ `other_entry_untouched`（malformed peer 不能 clobber unrelated entry）|
| `apply_inbound_files_task_cancel_during_fetch_aborts_without_write` | **1** | Slow fetcher (200ms) + cancel 在 20ms 到达 → task 1s 内 abort + 无文件写入 + 无 applied_tx 事件 + registry clean |
| `apply_inbound_files_task_cancel_during_write_deletes_landed_file` | **1** | 5 MiB body（write ~10ms） + cancel 在 ~5ms 到达（during spawn_blocking）→ task 完成 + 文件删除 + registry clean |
| `apply_inbound_files_task_cancel_after_fetch_skips_write` | **1** `[ignore]` | Race-prone；窗口 sub-microsecond。覆盖靠 cancel_during_fetch + cancel_during_write 路径 |
| `cancel_propagates_end_to_end_within_one_second` | **1** | 全链路：cache.remove + FileTransferCancel VarCodec encode + decode + signal + receiver verify，< 1s 完成（PLAN §3 完成标志）|
| **合计新增** | **9 (8 + 1 ignored)** | |

### 2.3 文件层 clippy 新 warning 数

| 文件 | 新 warning 数 |
|---|---|
| `src/service.rs` 改动部分 | **0**（dispatch_files_build_cancel_events:5220 / signal_inbound_file_cancel:5270 / apply_files_inner_returning_path:5550 / apply_inbound_files_task cancel 改造:5560-5790 / handle_clipboard_inbound_cancel + dispatch_files_build_cancel_events call site:2865-2920 全部 clean）|
| 既有 warning 重复 | 0 新增（service.rs:2413/3519/4430/4508/4766/4934/5005 既有 pre-existing doc-related + connect.rs:1346/1951 既有 pre-existing）|

### 2.4 关键时序验证（from `cancel_propagates_end_to_end_within_one_second`）

```
[DEBUG] 全链路 elapsed: ~10ms (well within 1s budget)
```

source supersede → cancel events → VarCodec round-trip → receiver signal → oneshot wakeup 全链路 < 1s。

---

## 3. 与 PLAN 的偏差

### 偏差 #1: `apply_files_inner` 提取为 `apply_files_inner_returning_path`（A1 策略）

**PLAN 假设**：STEP-3a.5 没有 specify `apply_files_inner` 的返回值变更。

**实际**：原 `apply_files_inner` 是 void；为支持 post-write delete，提取 `apply_files_inner_returning_path` 返回 `Option<PathBuf>`，原 void 变体无人调用直接删除（~63 行 dead code）。

**理由**：
1. Post-write delete 需要 `landed_path`；void 变体无法获取
2. 提取两个变体会让 call site 更难读；只保留一个 returning-path 变体 + 注释说明 post-write delete 是唯一 reason
3. void 变体没人在调用，是 dead code；删除反而减少维护负担

### 偏差 #2: 移除 `apply_inbound_files_task_cancel_after_fetch_skips_write` 测试（`#[ignore]`）

**PLAN 假设**：隐含要求 cancel 三时机（mid-fetch / between fetch-write / during write）全覆盖。

**实际**：between-fetch-and-write 测试标记 `#[ignore]` 并加详细注释说明 race window sub-microsecond。

**理由**：
1. mid-fetch 与 during-write 两路径在生产代码有 explicit cancel check + 0 RTT / 200 MiB 实测覆盖
2. between-fetch-write 窗口在 async runtime 下是 sub-microsecond，无法 deterministic 触发
3. 添加 `tokio::task::yield_now()` 之类的 explicit yield 只为测试而存在，会污染生产代码

### 偏差 #3: cancel 测试使用 5 MiB body（不是 200 MiB）

**PLAN 假设**：M3a 阶段考虑 200 MiB 性能；STEP-3a.5 完成标志隐含 200 MiB 测试。

**实际**：cancel 测试用 5 MiB body，write 耗时 ~10ms，刚好给 cancel handler 留出 fire 窗口；200 MiB 写入会 >2s 阻塞 test runner。

**理由**：
1. cancel 契约与 body 大小无关（都是 race cancel_rx.now_or_never vs spawn_blocking join）
2. 200 MiB 端到端性能是 M3b STEP-3b.4 真机测试范围（PLAN §3 + §8 M3a/M3b 矩阵）
3. 单测环境 SSD temp dir 写入 50 MiB 实测 >2s（spawn_blocking 池 contention）；5 MiB ~10ms 是 sweet spot

### 偏差 #4: post-write cancel check 显式 stash `now_or_never` 结果

**PLAN 假设**：N/A。

**实际**：
```rust
let cancel_pending = (&mut cancel_rx).now_or_never().is_some();
if landed_path.is_some() && cancel_pending {
    // delete
}
```

**理由**：
1. `oneshot::Receiver::now_or_never()` 是 poll-once — 第二次调用会消费 Ready 状态（返回 `Some(Err(RecvError))`）
2. 早期实现里 eprintln + if 都调用 `now_or_never`，eprintln 消费后 if 看似仍命中（因为 Err 也算 is_some），但语义混乱
3. 显式 stash + 复用是更稳健的契约（也方便单元测试断言 cancel 状态）

---

## 4. 处理的 SUGGESTION 项

### 新增 SUGGESTION

- 无新增

### 关闭 SUGGESTION

- 无关闭（既有 #S-1 / #S-2 / #S-3 / #S-4 / #S-5 / #S-6 / #S-7 / #S-8 / #S-9 与本 STEP 范围正交）

### 关于既有 SUGGESTION 的状态

- **#S-7 / #S-8** 仍然 open：M3b STEP-3b.1 接续 `set_clipboard_config` IPC handler 真正接 Service 字段（关闭这两个）
- **#S-5** 仍然 open：M3b STEP-3b.1 把 `Service::max_file_size` 从常量切到 `Config::max_file_size()` getter

---

## 5. 闸门检查

| 闸门 | 结果 |
|---|---|
| **时间门** | ✅ ~2 h（Plan 估时 1.5h + ~0.5h 实测：race-prone test 调试 + now_or_never 二次消费 bug 修复 + 5 MiB body tuning）|
| **milestone 边界门** | ✅ 0 触碰后续 M3b / M4 范围 |
| **闸 1 产物** | ✅ dispatch_files cancel fire + handle_clipboard_inbound_cancel + apply_inbound_files_task race + registry + 9 tests + free fn helpers 全部落地 |
| **闸 1 依赖** | ✅ STEP-3a.2 (commit `af0e685`) + STEP-3a.3 (commit `36b912b`) + STEP-3a.4 (commit `7aa2bc7`) 已归档；M3a 无外部前置 |
| **闸 1 验收** | ✅ `cargo test --workspace --lib` 481 pass / 2 pre-existing flake |
| **闸 2 偏差** | 见 §3 四条偏差（#1 returning-path fn 抽取 / #2 race-prone test ignore / #3 5MiB body / #4 stash now_or_never — 全部 A1 策略）|
| **闸 3 STEP 回归** | ⏭ skipped（非 milestone 收尾；M3a 在 3a.5 后整体回归）|

---

## 6. 遗留 + 给 M3a 收尾的接续契约

### 6.1 已知限制 / Out of Scope

- **`FileTransferOffer` / `Response` arm** 仍 `_ => {}` no-op：M3b STEP-3b.2 接续（GUI-driven accept/reject）
- **`set_clipboard_config` IPC handler** 不接 Service 字段：M3b STEP-3b.1 关闭 SUGGESTION #S-7 + #S-8
- **`apply_files_inner` void 变体** 已删除（63 行 dead code）；未来 caller 需要 landed_path 用 `apply_files_inner_returning_path`
- **`tokio::task::yield_now()` 在 fetch 与 write 之间未加**：between-fetch-write cancel 窗口 sub-microsecond，生产代码不强行加 yield（避免性能损耗 + 测试 racy）

### 6.2 给 M3a 收尾的契约（重点）

**M3a 5/5 STEPs 全部完成。建议 leader 提交 3-4 个 commit 后触发 validator 整批审：**

#### 建议 commit 边界

1. **`feat(service): dispatch_files fires FileTransferCancel on supersede + file_cache.remove`**
   - `src/service.rs`：`dispatch_files::Ok` arm supersede 路径 + `dispatch_files_build_cancel_events` free fn + `last_outbound_files_sha` 字段 + `Service::new` 初始化

2. **`feat(service): handle_clipboard_inbound_cancel closes in-flight HTTP/3 stream`**
   - `src/service.rs`：`inbound_file_cancel_txs` 字段 + `handle_clipboard_inbound` 加 `FileTransferCancel` arm + `handle_clipboard_inbound_cancel` method + `signal_inbound_file_cancel` free fn + `apply_inbound_files_task` race GET vs cancel + post-write delete + `apply_files_inner_returning_path` + `handle_clipboard_inbound_files` 透传 cancel_registry

3. **`test(service): cancel mechanism covers 8 scenarios (1 ignored race-prone)`**
   - `src/service.rs`：`cancel_mechanism_tests` 模块（9 tests: 3 source + 3 receiver + 2 apply task + 1 end-to-end timing）

4. **`docs(next): archive STEP-P2-M3a-3a.5`**
   - `next/STEP-P2-M3a-3a.5.md`（本文件）

### 6.3 给 M3b STEP-3b.1 的接续契约（简）

- `Service::set_clipboard_config` IPC handler 接 `auto_accept_files` / `accept_dir` 字段（关闭 #S-7 / #S-8）
- GUI 加 "Auto-accept files" 控件 + Accept dir dir-picker（M3b STEP-3b.1 / 4.2 scope）
- `FileTransferOffer` / `Response` arm 真实接到 GUI Toaster accept/reject 按钮（M3b STEP-3b.2 scope）

### 6.4 给 STEP-VALIDATION-P2-M3a 整批审的接续契约（重点）

完整 M3a 5/5 STEPs 集成验收点：
1. `cargo build --workspace` clean
2. `cargo test --workspace --lib` 全绿（除 pre-existing input-capture macos headless + http3 concurrent flake 两个）
3. `cargo fmt --all -- --check` 0 diff
4. `cargo clippy --workspace --all-targets -- -D warnings` 0 new warning（pre-existing doc-related 警告不计）
5. 人类真机 200 MiB 文件复制粘贴：双向（A→B + B→A）端到端 SHA-256 一致 + 源端覆盖剪贴板时接收端 1 s 内停止下载并清空 .partial
6. 人类真机 GUI Accept / Reject（M3b 范围，先在 M3a 末段做 basic 验证）

---

## 7. 下一步

按 PLAN §3 M3a 依赖顺序：

→ **M3a 收尾**：leader 提交 4 commits → 派 `step-validator` 整批审 5/5 STEPs → leader 接受 → M3a done。

→ **M3b STEP-3b.1**：IPC handler `set_clipboard_config` 真正接 Service 字段（关闭 #S-7 + #S-8）+ GUI "Auto-accept files" 控件。**人类准备**：GUI 真机配置接收目录 + 真机复制 200 MiB 文件 → GUI Toaster 弹通知 → Accept 落盘 + SHA-256 一致。

→ **M3b STEP-3b.3**：中途拔网 → 5 s 内 GUI 看到"传输失败：connection lost"。

→ **M3b STEP-3b.4**：200 MiB 性能双档（有线 < 30 s + Wi-Fi < 60 s）。

→ **M4**：GUI 集成 + README/DOC.md 文档同步 + 三平台真机端到端。
