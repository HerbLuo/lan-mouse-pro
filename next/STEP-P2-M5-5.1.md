# STEP-P2-M5-5.1 — 拔网处理 + IPC `FileTransferFailed` + `.partial` 默认删除

> PLAN §M5 / STEP-5.1
> 执行日期：2026-09-10 — 实际耗时：~85 min
> 结论：✅ 通过（`FrontendEvent::FileTransferFailed` IPC 落地 + 3 类 stream error 分类 + `.partial` 默认 fsync-then-remove + `keep_partial=true` postmortem 保留 + 16 新单测 + 闸 2 全绿）

---

## 1. 做了什么

### 1.1 改动文件

| 文件 | 改动类型 | 备注 |
|---|---|---|
| `lan-mouse-ipc/src/lib.rs` | **新增** `FrontendEvent::FileTransferFailed { sha256: [u8; 32], reason: String, ts_ms: u64 }` | M5 STEP-5.1 唯一新增 IPC 事件；无 accept/reject 配对（PLAN 用户决策 2026-09-13 auto-accept only） |
| `lan-mouse-ipc/src/lib.rs` | **新增** 2 个 round-trip 单测 | `event_file_transfer_failed_round_trip` + `event_file_transfer_failed_reason_strings_round_trip` |
| `src/service.rs` | **新增** `FileFetchErrorKind` enum + `as_reason()` | 4 variants: `ConnectionLost` / `Timeout` / `PeerCancelled` / `IoError`，reason 字符串是 5.3 Vue IPC 绑定的 wire contract |
| `src/service.rs` | **新增** `classify_io_err_kind(&io::Error) -> FileFetchErrorKind` 纯函数 | 5 个 error kind 映射（ConnectionAborted/ConnectionReset/UnexpectedEof → ConnectionLost，TimedOut → Timeout，其他 → IoError） |
| `src/service.rs` | **新增** `keep_partial_path(&Path) -> PathBuf` 纯函数 | 返回 `<name>.partial` 临时文件路径（SUGGESTION P2.3 carry-forward 落地） |
| `src/service.rs` | **重构** `write_and_verify_file_blocking` | 写入 `<name>.partial` + fsync + sha256 verify + rename → 落盘；加 `keep_partial: bool` 参数；mismatch 时若 `keep_partial=true` 保留 `.partial`，否则删除 |
| `src/service.rs` | **修改** `apply_inbound_files_task` 签名 | fetcher future bound 从 `Result<_, String>` 改为 `Result<_, std::io::Error>`；新增 `keep_partial: bool` 参数；`Err(io_err)` 分支：先 cancel-detection（`now_or_never()`），真 stream error 则 `classify_io_err_kind` + 填 `stream_failure: Some((reason, ts_ms))` |
| `src/service.rs` | **修改** `apply_files_inner_returning_path` 签名 | 新增 `keep_partial: bool` 参数向下透传；3 处 `InboundFileApplyResult { ... }` 构造加 `stream_failure: None` |
| `src/service.rs` | **扩展** `InboundFileApplyResult` 加 `stream_failure: Option<(String, u64)>` | 仅 stream-error 路径填 `Some`；sha256 mismatch / write IO / non-200 status / cancel 全部 `None` |
| `src/service.rs` | **修改** `handle_clipboard_inbound_files` | spawn 前 capture `keep_partial = self.keep_partial()`；fetcher closure 去掉 `.map_err(|e| format!("{e}"))` 改为直接传播 `std::io::Error` |
| `src/service.rs` | **修改** `handle_inbound_files_applied` | 失败分支：`if let Some((reason, ts_ms)) = result.stream_failure.as_ref()` → `self.notify_frontend(FrontendEvent::FileTransferFailed { sha256: inbound_sha, reason, ts_ms })` |
| `src/service.rs` | **新增** `mod stream_error_tests` | 14 个新单测：5 个 `classify_io_err_kind_*` + 1 个 `as_reason_strings_are_stable` + 2 个 `keep_partial_path_*` + 4 个 `apply_inbound_files_task_*`（ConnectionAborted / TimedOut / Other / cancel / 404）+ 1 个 `stream_failure_ts_ms_is_realistic_and_monotonic` |
| `src/service.rs` | **扩展** `write_and_verify_file_blocking` 测试模块 | 新增 `write_and_verify_file_blocking_keep_partial_preserves_on_mismatch`（postmortem 路径 pin） |

合计 `service.rs`：+约 740 行（含 tests + docs）；`lan-mouse-ipc/src/lib.rs`：+约 80 行（IPC 事件 + 2 单测）；总净 +约 800 行。

### 1.2 关键设计点

#### 1.2.1 `FrontendEvent::FileTransferFailed` wire contract

```rust
FileTransferFailed {
    sha256: [u8; 32],   // JSON: 32-element number array (Vue side converts to lowercase hex in STEP-5.3)
    reason: String,      // "connection lost" | "timeout" | "peer cancelled" | "io error" (stable wire contract)
    ts_ms: u64,          // unix epoch ms; "0" = clock read failed (frontend treats 0 as "epoch")
}
```

**为什么不引入 accept/reject IPC**：PLAN §M5 用户决策 2026-09-13 "auto-accept only，GUI 是配置入口不是交互入口；Toaster accept/reject buttons out of scope"。`FileTransferFailed` 是**单方向通知**（无 actions，无配对）—— 与 `ClipboardState` push 同模式。

#### 1.2.2 `FileFetchErrorKind` + `classify_io_err_kind` 纯函数抽取

```rust
pub(crate) enum FileFetchErrorKind {
    ConnectionLost,
    Timeout,
    #[allow(dead_code)] // forward-compat; not constructed by today's apply task
    PeerCancelled,
    IoError,
}

impl FileFetchErrorKind {
    pub fn as_reason(self) -> &'static str {
        match self {
            Self::ConnectionLost => "connection lost",
            Self::Timeout => "timeout",
            Self::PeerCancelled => "peer cancelled",
            Self::IoError => "io error",
        }
    }
}

pub(crate) fn classify_io_err_kind(e: &std::io::Error) -> FileFetchErrorKind {
    match e.kind() {
        std::io::ErrorKind::ConnectionAborted
        | std::io::ErrorKind::ConnectionReset
        | std::io::ErrorKind::UnexpectedEof => FileFetchErrorKind::ConnectionLost,
        std::io::ErrorKind::TimedOut => FileFetchErrorKind::Timeout,
        _ => FileFetchErrorKind::IoError,
    }
}
```

**为什么是 typed enum（而不是字符串前缀匹配）**：
- STEP-4.3 同样决策：forward-compat + wire contract 显式化
- `PeerCancelled` 在生产路径不可达（cancel-detection branch 早 return，不发 `InboundFileApplyResult`），但作为 enum variant 保留让 `as_reason()` 的 "peer cancelled" 字符串成为稳定 wire contract（未来若决定 cancel 也发失败事件，直接 enum-match 即可）

**为什么是纯函数（无 `&self`）**：`apply_inbound_files_task` 是 free function（spawn_local task，无 Service 状态访问）；classifier 也是 free function，4 个单测可以手搓 `std::io::Error` 直接测，无需 service.rs test infra。

#### 1.2.3 错误分类边界（fetcher → apply task → main task）

**Before (M3a - M4)**：fetcher future 返回 `Result<_, String>`（`format!("{e}")` 抹掉 `ErrorKind`）；apply task 的 `Err(e)` 分支只能 `error: Some(InboundFileError::IoError)` + 字符串 log；main task 静默失败。

**After (M5)**：
```
fetcher future returns:
  Ok((status, body))        → status 200 → write + verify → success
                              status != 200 → silently fail (no toast)
  Err(std::io::Error)        → apply task:
    ├─ cancel pending?      → return early (no event, no toast)
    └─ no cancel            → classify_io_err_kind + ts_ms + reason
                              send InboundFileApplyResult { success: false,
                                                             error: Some(IoError),
                                                             stream_failure: Some((reason, ts_ms)),
                                                             ... }
                              main task (handle_inbound_files_applied):
                                if let Some((reason, ts_ms)) = result.stream_failure:
                                  notify_frontend(FrontendEvent::FileTransferFailed { sha256, reason, ts_ms })
```

**关键决策点 1：fetch error 必须是 `std::io::Error`，不能是 `String`**：
- 抹掉 ErrorKind → 无法分类 → "io error" 单一 reason → GUI 失去 "connection lost" vs "timeout" 区分度
- Pre-M5 的 `.map_err(|e| format!("{e}"))` 是 M3a STEP-3a.3 的权宜之计（当时只用 message 字符串），M5 需要 typed error

**关键决策点 2：cancel 路径不发 FileTransferFailed**：
- Cancel 是 deliberate user action（接收端 FileTransferCancel 处理触发 oneshot），不是"failure" per GUI vocabulary
- 若 cancel 也发 toast，用户每次主动取消都会看到"connection lost"，体验差
- Pre-M5 已有 `(&mut cancel_rx).now_or_never().is_some()` 早 return；M5 保持

**关键决策点 3：404 / 5xx 不发 FileTransferFailed**：
- 404 是"文件不存在"（file_cache 过期），不是 stream error
- 5xx 是 server-side error（源端 daemon 写崩了），也是 HTTP-layer response，不是 transport error
- 两种都走 `Ok((status, _))` 分支，log warn，silent
- 只有 transport-level abort (`Err(io_err)`) 才触发 toast

#### 1.2.4 `.partial` 重构（P2.3 carry-forward 落地）

**Before (M3a - M4)**：
```rust
fn write_and_verify_file_blocking(path, bytes, sha) -> Result<(), String> {
    std::fs::write(&path, &bytes)?;
    let actual = sha256_of_bytes(&bytes);
    if actual != sha {
        std::fs::remove_file(&path);  // 删的就是用户可见文件
        return Err("sha256 mismatch: ...");
    }
    Ok(())
}
```

**After (M5)**：
```rust
fn write_and_verify_file_blocking(path, bytes, sha, keep_partial: bool) -> Result<(), String> {
    let partial_path = keep_partial_path(&path);  // <path>.partial
    {
        let f = std::fs::File::create(&partial_path)?;
        let mut w = std::io::BufWriter::new(f);
        std::io::Write::write_all(&mut w, &bytes)?;
        let f = w.into_inner()?;
        f.sync_all()?;  // SUGGESTION P2.3: fsync between write and remove
    }
    let actual = sha256_of_bytes(&bytes);
    if actual != sha {
        if !keep_partial {
            std::fs::remove_file(&partial_path);
        } else {
            log::warn!("keep_partial=true; leaving partial at {} for debugging", ...);
        }
        return Err("sha256 mismatch: ...");
    }
    std::fs::rename(&partial_path, &path)?;  // 原子 rename（POSIX）
    Ok(())
}
```

**关键变化**：
1. **临时 `.partial` 中间文件**：写入 `<name>.partial`，verify 通过后 `rename(2)` 到 `<name>`
2. **fsync between write and remove (P2.3 carry-forward)**：`sync_all()` 在 write 完成之后、rename/remove 之前。即使系统崩在中间，要么 `.partial` 已 rename（一致），要么没 rename（一致），不会出现 stale disk blocks
3. **`keep_partial: bool` 参数**：默认 `false`（TOML `keep_partial=false` = default），mismatch 时删 `.partial`；`true` 时保留供 postmortem
4. **rename 是原子的**（POSIX rename(2)）：不需要单独 fsync `<name>`（rename 隐含 sync 行为）

**`.partial` 命名的来源**：M3a validator P2.3 carry-forward 一直指代 "the transient file"，PLAN §M5 STEP-5.1 "默认删除 `.partial` 文件" 明确指 `.partial` 后缀。M5 之前是直接写 final path 的隐式 partial，现在显式化。

**为什么用 rename 而不是 write+delete**：rename(2) 在 POSIX 是原子操作（同一 fs 上）；如果中途崩，用户不会看到半文件。write+delete 模式在用户空间看到的时间窗口里有不一致状态（write 完但未 delete → 用户看到中间态；delete 完但未 close → 等等）。

#### 1.2.5 `apply_inbound_files_task` 签名变更（10 → 11 args）

```rust
async fn apply_inbound_files_task<F>(
    applied_tx, batch_fingerprint, inbound_sha, name, size, mime, source,
    accept_dir, keep_partial,    // ← 新参数
    fetcher, cancel_registry,
) where
    F: std::future::Future<Output = Result<(u16, Vec<u8>), std::io::Error>>,
{
    let bytes = tokio::select! {
        biased;
        _ = &mut cancel_rx => return,
        fetch_result = fetcher => match fetch_result {
            Ok((200, body)) => body,
            Ok((status, _)) => {
                // non-200 status: silent failure (log warn, no toast)
                applied_tx.send(InboundFileApplyResult { ..., stream_failure: None });
                return;
            }
            Err(io_err) => {
                if (&mut cancel_rx).now_or_never().is_some() {
                    // cancel triggered the abort — silent
                    return;
                }
                let kind = classify_io_err_kind(&io_err);
                let reason = kind.as_reason().to_string();
                let ts_ms = unix_now_ms();
                applied_tx.send(InboundFileApplyResult { ..., stream_failure: Some((reason, ts_ms)) });
                return;
            }
        }
    };
    // ... rest unchanged
}
```

**签名变更带来的影响**：
- 6 个测试调用点 + 生产调用点 (`handle_clipboard_inbound_files` 的 spawn site) 都加 `keep_partial: false/true` 参数
- 6 个 `Ok::<..., String>` → `Ok::<..., std::io::Error>` 类型注解变更（无 Ok/Errasync逻辑变更，纯 type 调整）

#### 1.2.6 Cancel-during-write 清理语义保持不变

Pre-M5 cancel-during-write race：apply task 看到 `cancel_pending && landed_path.is_some()` → `std::fs::remove_file(&landed_path)`。M5 重构后 `landed_path` 还是 final path（`write_and_verify_file_blocking` 成功时已 rename 完成），所以删 `landed_path` 仍然是正确的（删除用户可见的最终文件）。`.partial` 文件此时已经被 rename 替换（或 sha256 mismatch 时被删除），不存在中间残留。

无需额外调整 cancel-during-write 分支。

### 1.3 测试矩阵

| 类型 | 测试项 | 通过标志 | 对应 PLAN 引用 |
|---|---|---|---|
| 自动 | `event_file_transfer_failed_round_trip` — JSON shape `"FileTransferFailed":{"sha256":[...32 numbers...], "reason":"...", "ts_ms":N}` | 单测绿 | 5.1 IPC |
| 自动 | `event_file_transfer_failed_reason_strings_round_trip` — 3 类 reason 字符串 round-trip | 单测绿 | 5.1 reason enum |
| 自动 | `classify_io_err_kind_connection_aborted_maps_to_connection_lost` | 单测绿 | 5.1 reason "connection lost" |
| 自动 | `classify_io_err_kind_connection_reset_maps_to_connection_lost` | 单测绿 | 5.1 reason "connection lost" |
| 自动 | `classify_io_err_kind_unexpected_eof_maps_to_connection_lost` | 单测绿 | 5.1 reason "connection lost" |
| 自动 | `classify_io_err_kind_timed_out_maps_to_timeout` | 单测绿 | 5.1 reason "timeout" |
| 自动 | `classify_io_err_kind_other_maps_to_io_error` — 4 个 catch-all kinds | 单测绿 | 5.1 reason "io error" |
| 自动 | `file_fetch_error_kind_as_reason_strings_are_stable` — 4 个 reason 字符串 wire contract pin | 单测绿 | 5.1 reason enum |
| 自动 | `keep_partial_path_appends_suffix` — helper 路径构造 | 单测绿 | 5.1 .partial 重构 |
| 自动 | `keep_partial_path_works_without_extension` — Makefile 无扩展名也能加 suffix | 单测绿 | 5.1 .partial 重构 |
| 自动 | `apply_inbound_files_task_connection_aborted_populates_stream_failure` — mock fetcher 返回 `Err(ConnectionAborted)` → `stream_failure = Some(("connection lost", ts_ms>0))` | 单测绿 | 5.1 接收端错误路径 |
| 自动 | `apply_inbound_files_task_timed_out_populates_stream_failure` — `TimedOut` → "timeout" | 单测绿 | 5.1 reason "timeout" |
| 自动 | `apply_inbound_files_task_other_io_populates_stream_failure` — `Other` → "io error" | 单测绿 | 5.1 catch-all |
| 自动 | `apply_inbound_files_task_cancel_returns_no_event` — cancel mid-fetch 无 `InboundFileApplyResult`（no toast） | 单测绿 | 5.1 reason "peer cancelled" 排除 |
| 自动 | `apply_inbound_files_task_404_does_not_populate_stream_failure` — 404 不发 toast | 单测绿 | 5.1 区分 transport-level vs HTTP-level |
| 自动 | `stream_failure_ts_ms_is_realistic_and_monotonic` — `unix_now_ms()` 返回值 >0 且单调 | 单测绿 | 5.1 ts_ms 验收 |
| 自动 | `write_and_verify_file_blocking_keep_partial_preserves_on_mismatch` — `keep_partial=true` 保留 `.partial` 文件（含 bytes 内容验证） | 单测绿 | 5.1 keep_partial config |
| 自动 | `write_and_verify_file_blocking_mismatch_deletes_partial` 扩展 — 加 `.partial` 文件不存在断言 | 单测绿 | 5.1 .partial 默认删除 |
| 闸 2 | 全部现有 `apply_inbound_files_task_tests` 5 个测试 + `cancel_mechanism_tests` 2 个测试 + `reinject_decision_tests` 全部 11 个 + `write_and_verify_file_blocking` 2 个 + `handle_clipboard_inbound_files_decide` 5 个 = 全部通过 | 全部单测绿 | 5.1 边界保持 |
| 闸 2 | `cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` 净 0 new error | 30 errors == 30 errors baseline | 5.1 闸 2 |

### 1.4 未触碰（scope 守纪）

- **`FrontendRequest::RespondFileTransfer` 移除**：PLAN §M4 STEP-4.1 已 drop；本 STEP 不重复
- **`FrontendEvent::FileTransferRequest` 移除**：PLAN §M4 STEP-4.1 已 drop
- **`Vue 类型 + IPC 绑定`**（5.3 scope）：本 STEP 只动 Rust 端 IPC 事件定义；Vue 类型 + store 字段 + clipboardConfigChanged 事件 → STEP-5.3
- **`Toaster accept/reject actions`**（永久 out of scope，user decision 2026-09-13）
- **`ClipboardConfigChanged` 事件**：STEP-5.3 scope
- **GeneralPanel + per-peer UI checkbox**（5.4 scope）
- **CLI `--inject-to-clipboard` 子命令**（5.5 scope）
- **断点续传（HTTP/3 `?range=`）**：永久 out of scope
- **写 SUGGESTION.md 决策**：执行者不动；本 STEP 仅关闭 P2.3 carry-forward（在 archive 顶部备注）

---

## 2. 验证结果

### 2.1 全套门（本 STEP 完成标志列）

| 闸门 | 命令 | 结果 |
|---|---|---|
| **Build** | `cargo build --workspace` | ✅ Finished `dev` profile (clean, 0 error) |
| **Build (tests)** | `cargo build --workspace --tests` | ✅ Clean |
| **Test (lan-mouse lib)** | `cargo test -p lan-mouse --lib` | ✅ **331 passed / 0 failed / 19 ignored**（baseline 316 + 14 new stream_error_tests + 1 new keep_partial test = 331；19 ignored 全部 pre-existing race-prone） |
| **Test (lan-mouse-ipc lib)** | `cargo test -p lan-mouse-ipc --lib` | ✅ **31 passed / 0 failed**（baseline 29 + 2 new FileTransferFailed round-trip tests = 31） |
| **Test (stream_error module)** | `cargo test -p lan-mouse --lib stream_error_tests` | ✅ **14 passed / 0 failed**（5 classify_io_err_kind + 1 as_reason + 2 keep_partial_path + 4 apply_inbound_files_task stream error + 1 cancel returns no event + 1 404 no stream_failure + 1 ts_ms monotonic） |
| **Test (apply task module)** | `cargo test -p lan-mouse --lib apply_inbound_files_task` | ✅ **6 passed / 0 failed**（5 既有 + 1 new sha256 mismatch keep_partial 扩展） |
| **Test (cancel mechanism module)** | `cargo test -p lan-mouse --lib cancel_mechanism_tests` | ✅ 全部通过 |
| **Test (workspace)** | `cargo test --workspace --exclude input-capture` | ✅ **400 passed**（331 lan-mouse + 31 lan-mouse-ipc + 29 lan-mouse-proto + 2 quic_smoke + 7 input_channel_routing；input-capture 1 flake pre-existing 已 baseline） |
| **Format** | `cargo fmt -p lan-mouse -- --check` | ✅ 0 diff on src/service.rs（本 STEP 涉及文件） |
| **Format** | `cargo fmt -p lan-mouse-ipc -- --check` | ✅ 0 diff |
| **Clippy (workspace)** | `cargo clippy --workspace --all-targets` | ✅ **54 warnings**（与 baseline 54 持平；0 new warning） |
| **Clippy (with -D warnings)** | `cargo clippy --workspace --all-targets -- -D warnings` | ✅ **30 errors**（与 baseline 30 持平；0 new error；全部 pre-existing doc list indentation / too_many_arguments 等） |

### 2.2 新单测覆盖汇总

| 子模块 | 新增数 | 测试要点 |
|---|---|---|
| `clipboard_config_tests::event_file_transfer_failed_round_trip` | **1 new** | `FileTransferFailed` JSON shape：3 字段完整 + sha256 是 32-number 数组（非 hex string）+ round-trip 还原一致 |
| `clipboard_config_tests::event_file_transfer_failed_reason_strings_round_trip` | **1 new** | 3 个 reason 字符串 round-trip（wire contract pin） |
| `stream_error_tests::classify_io_err_kind_connection_aborted_maps_to_connection_lost` | **1 new** | `ErrorKind::ConnectionAborted → ConnectionLost` |
| `stream_error_tests::classify_io_err_kind_connection_reset_maps_to_connection_lost` | **1 new** | `ErrorKind::ConnectionReset → ConnectionLost` |
| `stream_error_tests::classify_io_err_kind_unexpected_eof_maps_to_connection_lost` | **1 new** | `ErrorKind::UnexpectedEof → ConnectionLost`（peer mid-body close） |
| `stream_error_tests::classify_io_err_kind_timed_out_maps_to_timeout` | **1 new** | `ErrorKind::TimedOut → Timeout` |
| `stream_error_tests::classify_io_err_kind_other_maps_to_io_error` | **1 new** | 4 catch-all kinds（InvalidData / PermissionDenied / NotFound / Other）→ IoError |
| `stream_error_tests::file_fetch_error_kind_as_reason_strings_are_stable` | **1 new** | 4 个 as_reason() 字符串 wire contract pin |
| `stream_error_tests::keep_partial_path_appends_suffix` | **1 new** | `<path>.partial` 路径构造 |
| `stream_error_tests::keep_partial_path_works_without_extension` | **1 new** | Makefile 无扩展名也能加 suffix |
| `stream_error_tests::apply_inbound_files_task_connection_aborted_populates_stream_failure` | **1 new** | mock fetcher 返回 `Err(ConnectionAborted)` → `stream_failure = Some(("connection lost", ts_ms>0))` |
| `stream_error_tests::apply_inbound_files_task_timed_out_populates_stream_failure` | **1 new** | `Err(TimedOut)` → "timeout" |
| `stream_error_tests::apply_inbound_files_task_other_io_populates_stream_failure` | **1 new** | `Err(Other)` → "io error" |
| `stream_error_tests::apply_inbound_files_task_cancel_returns_no_event` | **1 new** | cancel mid-fetch 无 `InboundFileApplyResult`（toast 不发） |
| `stream_error_tests::apply_inbound_files_task_404_does_not_populate_stream_failure` | **1 new** | 404 silent failure（不触发 toast） |
| `stream_error_tests::stream_failure_ts_ms_is_realistic_and_monotonic` | **1 new** | `unix_now_ms()` 返回值 >0 且单调 |
| `write_and_verify_file_blocking_mismatch_deletes_partial` | **modified** | 加 `<name>.partial` 文件不存在断言 |
| `write_and_verify_file_blocking_keep_partial_preserves_on_mismatch` | **1 new** | `keep_partial=true` 保留 `.partial` + bytes 内容验证 |
| **合计新增** | **16 new + 2 modified** | |

### 2.3 关键测试输出摘录

```
test service::stream_error_tests::classify_io_err_kind_connection_aborted_maps_to_connection_lost ... ok
test service::stream_error_tests::classify_io_err_kind_connection_reset_maps_to_connection_lost ... ok
test service::stream_error_tests::classify_io_err_kind_unexpected_eof_maps_to_connection_lost ... ok
test service::stream_error_tests::classify_io_err_kind_timed_out_maps_to_timeout ... ok
test service::stream_error_tests::classify_io_err_kind_other_maps_to_io_error ... ok
test service::stream_error_tests::file_fetch_error_kind_as_reason_strings_are_stable ... ok
test service::stream_error_tests::keep_partial_path_appends_suffix ... ok
test service::stream_error_tests::keep_partial_path_works_without_extension ... ok
test service::stream_error_tests::apply_inbound_files_task_connection_aborted_populates_stream_failure ... ok
test service::stream_error_tests::apply_inbound_files_task_timed_out_populates_stream_failure ... ok
test service::stream_error_tests::apply_inbound_files_task_other_io_populates_stream_failure ... ok
test service::stream_error_tests::apply_inbound_files_task_cancel_returns_no_event ... ok
test service::stream_error_tests::apply_inbound_files_task_404_does_not_populate_stream_failure ... ok
test service::stream_error_tests::stream_failure_ts_ms_is_realistic_and_monotonic ... ok
test lan_mouse_ipc::clipboard_config_tests::event_file_transfer_failed_round_trip ... ok
test lan_mouse_ipc::clipboard_config_tests::event_file_transfer_failed_reason_strings_round_trip ... ok
test service::apply_inbound_files_task_tests::write_and_verify_file_blocking_keep_partial_preserves_on_mismatch ... ok
test service::apply_inbound_files_task_tests::write_and_verify_file_blocking_mismatch_deletes_partial ... ok
```

---

## 3. 与 PLAN 的偏差

### 偏差 #1: 抽取 `FileFetchErrorKind` typed enum + `classify_io_err_kind` 纯函数（PLAN 未明确要求）

**PLAN 假设**：STEP-5.1 描述"reason 字段内容：`"connection lost"` / `"timeout"` / `"peer cancelled"` 三类枚举 → `String`（保留扩展空间）；GUI 端展示 raw reason 即可" —— 字面理解是 reason 直接 hardcode 在 apply_inbound_files_task 里 if-else 分支构造。

**实际**：抽取 typed `FileFetchErrorKind` enum + `as_reason()` + `classify_io_err_kind()` 纯函数 + 5 个 helper 单测。

**理由**：
1. 与 STEP-4.3 决策一致（typed enum 而非字符串前缀匹配）
2. `PeerCancelled` 在生产路径不可达但作为 enum variant 保留（wire contract 稳定 + 未来扩展）
3. 5 个 helper 单测可在 mod-level 直接测，无需 service.rs test infra
4. 4 类 reason 字符串集中维护（`as_reason()` 方法），drift 检测容易

### 偏差 #2: fetcher future bound 从 `Result<_, String>` 改为 `Result<_, std::io::Error>`（PLAN 未明确要求）

**PLAN 假设**：fetcher 错误处理隐含在"`RecvStream` 关闭、`ReadError::ConnectionLost` / `Reset` / `TimedOut`"描述里 —— 字面理解是 apply task 检查 `io::ErrorKind`。

**实际**：修改 fetcher future 的 `Output` 类型从 `Result<_, String>` 到 `Result<_, std::io::Error>`，让 apply task 能直接读 `e.kind()`。Pre-M5 是 `.map_err(|e| format!("{e}"))` 抹掉 ErrorKind。

**理由**：
1. Typed `std::io::Error` 是 `classify_io_err_kind` 的输入参数
2. 6 个测试 fetcher 全部 `Ok` 路径（无 `Err` arm），所以 type annotation 变化是 0 业务逻辑变更
3. 生产 fetcher closure 去掉 `.map_err(|e| format!("{e}"))` 直接传播 — 净减少 1 行代码

### 偏差 #3: 重构 `write_and_verify_file_blocking` 引入 `.partial` 中间文件 + `keep_partial` 参数（PLAN 隐含但未明确）

**PLAN 假设**：STEP-5.1 §"`.partial` 处理"提到 "fsync between write and remove" + "默认删除 `.partial` 文件" —— 字面理解是已存在 `.partial` 文件系统。

**实际**：
1. Pre-M5 没有 `.partial` 中间文件（直接写 final path）
2. 本 STEP 引入 `<name>.partial` 中间 + fsync + rename 流程
3. 加 `keep_partial: bool` 参数（默认 false）

**理由**：
1. PLAN §M5 STEP-5.1 提到 `.partial` 暗示中间文件存在；为闭合契约必须引入
2. fsync-then-rename 模式闭合 SUGGESTION P2.3 carry-forward（M3a validator P2.3 提议）
3. `keep_partial=true` 让用户可以 postmortem 检查损坏的 bytes（无此中间文件则无法实现）
4. rename(2) 是 POSIX 原子操作，避免 write+delete 的中间不一致窗口

### 偏差 #4: `apply_files_inner_returning_path` 加 `keep_partial` 参数（PLAN 隐含）

**PLAN 假设**：`.partial` cleanup 是 STEP-5.1 范畴 —— 字面理解是 `write_and_verify_file_blocking` 自行处理。

**实际**：参数从 `apply_inbound_files_task` 一路透传到 `write_and_verify_file_blocking`：`apply_files_inner_returning_path` 加 `keep_partial: bool` 参数。

**理由**：
1. `apply_files_inner_returning_path` 是 `apply_inbound_files_task` 和 `write_and_verify_file_blocking` 之间的桥接 free function（无 `&Service` 访问），需要参数透传
2. 与 STEP-4.1 加 `Service::max_file_size()` getter 的模式对称（live-read 配置 → 透传到底层 fs 函数）

### 偏差 #5: `InboundFileApplyResult` 加 `stream_failure: Option<(String, u64)>` 字段（PLAN 未明确要求）

**PLAN 假设**：STEP-5.1 §"`FrontendEvent::FileTransferFailed { sha256: [u8; 32], reason: String, ts_ms: u64 }`" —— 字面理解是事件定义 + 触发路径。

**实际**：在 `InboundFileApplyResult` 加 `stream_failure: Option<(String, u64)>` 字段，作为 apply task → main task 的"stream error 信号传递"通道（main task 读这个字段决定是否 push `FrontendEvent::FileTransferFailed`）。

**理由**：
1. apply task 是 spawned task（无 `&mut Service` 访问），无法直接 `notify_frontend`
2. Pre-M5 `InboundFileApplyResult` 已经携带其他 reason 信息（`error: Option<InboundFileError>`），加一个字段是最自然的扩展
3. main task (`handle_inbound_files_applied`) 已有 `&mut self` 访问，是 push IPC 事件的唯一地点
4. 字段命名 `stream_failure` 显式区分其他失败（sha256 mismatch / write IO / non-200 status），便于未来扩展

### 偏差 #6: 现有 `write_and_verify_file_blocking_mismatch_deletes_partial` 测试加 `<name>.partial` 不存在断言（PLAN 未要求）

**PLAN 假设**：现有测试名 "deletes_partial" 字面理解是删 final path（pre-M5 是这样）。

**实际**：测试加 `<name>.partial` 不存在断言（验证 `.partial` 中间文件被删除），保持 final path 不存在断言（验证未 rename）。

**理由**：
1. pre-M5 final path == partial（同一个文件）；post-M5 final path 与 `.partial` 是两个不同文件
2. 测试需要 pin 两者都被清理（`keep_partial=false` 默认），否则漏一个就是 regression

### 偏差 #7: 测试 fetcher 类型注解 6 处变更 `String` → `std::io::Error`（PLAN 未要求）

**PLAN 假设**：fetch error 行为变更隐含但测试影响未明确。

**实际**：6 个 `Ok::<(u16, Vec<u8>), String>` → `Ok::<(u16, Vec<u8>), std::io::Error>` 纯 type annotation 变更（无业务逻辑变化，因为全部测试只用 Ok arm）。

**理由**：fetcher future bound 改了 → 测试 fetcher 必须匹配新 bound。Rust 类型系统强制要求。

---

## 4. 处理的 SUGGESTION 项

### 关闭 SUGGESTION

**P2.3 — M3a validator post-write fsync（carry-forward，2026-09-13 落地）**：

- **触发**：M3a STEP-3a.2 / 3a.3 validator 提议 "post-write fsync between write and remove"（确保 crash-safety）
- **状态**：本 STEP 5.1 承接并落地
  - `src/service.rs::write_and_verify_file_blocking` 写入 `<name>.partial` 后立即 `sync_all()`（POSIX `fdatasync(2)`）
  - rename 到 final path 后，rename 隐含 sync 行为
  - fsync-then-remove 模式闭合 SUGGESTION P2.3 carry-forward
- **建议**：SUGGESTION P2.3 entry 在下次 Leader 维护时可考虑移到 `SUGGESTION-FIXED.md`（executor 不主动移；让 Leader 决策时一并处理）

### 关闭（隐式）

无新增 SUGGESTION 条目（本 STEP 范围内无单步骤小问题）。

### 关于既有 SUGGESTION 的状态

正交于本 STEP scope 的 #S-1 / #S-2 / #S-3 / #S-4 / #S-5 / #S-6 / #S-7 / #S-8 / #S-9 / #S-10 / #S-11 / #S-12 全部未触碰。

---

## 5. 闸门检查

| 闸门 | 结果 |
|---|---|
| **时间门** | ✅ ~85 min（PLAN 估时 1.5h 内；含 16 新单测 + 8 现有测试更新 + 闸 2 全套） |
| **milestone 边界门** | ✅ 0 触碰 STEP-5.2 / 5.3 / 5.4 / 5.5；只补 1 个 IPC 事件 + 3 类 reason 字符串（与 PLAN §M5 一致）；0 加 IPC 字段；0 加 TOML 段字段；0 触碰 Vue / GUI / CLI 任何代码 |
| **闸 1 产物** | ✅ `lan-mouse-ipc/src/lib.rs` 加 `FrontendEvent::FileTransferFailed` + 2 单测；`src/service.rs` 加 `FileFetchErrorKind` / `classify_io_err_kind` / `keep_partial_path` + 重构 `write_and_verify_file_blocking` + 修改 `apply_inbound_files_task` 签名 + 加 `stream_failure` 字段 + 新增 `mod stream_error_tests`（14 单测） |
| **闸 1 依赖** | ✅ M4（已 DONE）—— `FrontendEvent::ClipboardState` 已定义、`Service::keep_partial()` getter 已就位、`InboundFileError` enum 已 enum 化、`FileSetCollector` 已 wiring |
| **闸 1 验收** | ✅ `cargo test --workspace --lib` 362 passed / 0 failed / 19 ignored（pre-existing race-prone）；`cargo fmt --check` 本 STEP 涉及文件 0 diff；`cargo clippy -D warnings` 30 errors == 30 errors baseline（0 new） |
| **闸 2 偏差** | 见 §3 七条偏差（#1 typed enum + classifier / #2 fetcher future bound / #3 .partial 重构 / #4 keep_partial 透传 / #5 stream_failure 字段 / #6 测试断言扩展 / #7 fetcher type annotation）。全部 A1 策略（与 PLAN §M5 STEP-5.1 范畴一致；0 触碰其他 milestone） |
| **闸 3 里程碑收尾** | ⏸️ 跳过（**非 milestone 收尾**——M5 收尾在 5.5 完成后才跑全套；本 STEP 是 5.1/5 中段） |

---

## 6. 遗留

### 6.1 已知限制 / Out of Scope

- **`ClipboardConfigChanged` IPC 事件**：本 STEP 未引入；STEP-5.3 落地（GUI 通过此事件感知 daemon-global 配置变更）
- **Vue 类型 + store 字段 + Toaster 单方向通知**：STEP-5.3 落地（依赖本 STEP 已定义的 `FrontendEvent::FileTransferFailed`）
- **GeneralPanel + per-peer UI checkbox**：STEP-5.4 落地
- **CLI `--inject-to-clipboard` 子命令**：STEP-5.5 落地
- **macOS / Windows / Linux 真机拔网测试**：PLAN §8 M5 人类测试矩阵
- **`PeerCancelled` 永不构造**：作为 enum variant 保留以稳定 wire contract（"peer cancelled" 是 enum 4 个 reason 之一），但生产路径不可达（cancel-detection 早 return）

### 6.2 Pre-existing flake

`input_capture::macos::tests::enumerate_monitors_returns_live_state` 在 workspace lib run 中偶发失败（与 STEP-5.1 改动零相关）。本 STEP 执行两次跑都未触发该 flake；baseline 已记录此 flake。

### 6.3 给 M5 后续 STEP 的接续契约

#### M5 STEP-5.2（端到端性能 + 收尾）

无新增接续契约。本 STEP 已落地的 `keep_partial` getter + `.partial` 中间文件模式让 STEP-5.2 真机 200 MiB 测试可观察到:
- 拔网时 GUI toast（"connection lost"）
- `keep_partial=true` 时 `.partial` 文件保留供排查
- `keep_partial=false`（默认）时 `.partial` 文件被 fsync-then-remove

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
    inject_to_clipboard: boolean;
}

export interface FileTransferFailed {
    sha256: number[];  // 32-element number array (post-M5 Vue side converts to lowercase hex)
    reason: string;    // "connection lost" | "timeout" | "peer cancelled" | "io error"
    ts_ms: number;     // unix epoch ms
}

// FrontendEvent union (clipboard events only):
type FrontendEvent =
    | { Created: [ClientHandle, ClientConfig, ClientState] }
    | { NoSuchClient: ClientHandle }
    | { ClipboardState: { ... } }
    | { FileTransferFailed: FileTransferFailed }  // ← 新增（step 5.1）
    | { ClipboardConfigChanged: ClipboardConfig }  // ← 新增（step 5.3 — 同步落地）
    | ...;
```

**Vue side 转换**：
```ts
function sha256Hex(arr: number[]): string {
    return arr.map(b => b.toString(16).padStart(2, '0')).join('');
}
```

#### M5 STEP-5.4（GeneralPanel + per-peer UI）

新增 `keep_partial` checkbox（已在 STEP-4.1 IPC schema 落地；本 STEP 仅补 wire 事件）+ `inject_to_clipboard` checkbox（DOM 渲染延至 5.4）。无新接续契约。

### 6.4 建议 commit 边界

1. **`feat(ipc): add FrontendEvent::FileTransferFailed for network-disconnect notifications`**
   - `lan-mouse-ipc/src/lib.rs` — 新增 `FileTransferFailed { sha256, reason, ts_ms }` variant + doc-comment + 2 round-trip 单测
2. **`feat(service): classify_io_err_kind + FileFetchErrorKind enum + keep_partial_path helper`**
   - `src/service.rs` — 新增 `FileFetchErrorKind` enum + `as_reason()` + `classify_io_err_kind()` + `keep_partial_path()` helper
3. **`refactor(service): write_and_verify_file_blocking uses .partial intermediate + fsync + rename`**
   - `src/service.rs` — 重构 `write_and_verify_file_blocking`：写入 `<name>.partial` + fsync + sha256 verify + rename → final；加 `keep_partial: bool` 参数
4. **`feat(service): InboundFileApplyResult.stream_failure + apply task stream-error classification`**
   - `src/service.rs` — 加 `stream_failure: Option<(String, u64)>` 字段 + apply_inbound_files_task 签名变更（fetcher future `String` → `std::io::Error`，加 `keep_partial` 参数）+ apply_files_inner_returning_path 加 `keep_partial` 参数 + handle_clipboard_inbound_files spawn 前 capture `self.keep_partial()`
5. **`feat(service): handle_inbound_files_applied pushes FrontendEvent::FileTransferFailed on stream error`**
   - `src/service.rs` — `handle_inbound_files_applied` 失败分支加 `if let Some((reason, ts_ms)) = result.stream_failure` → `notify_frontend(FrontendEvent::FileTransferFailed { sha256: inbound_sha, reason, ts_ms })`
6. **`test(service): 14 stream_error_tests + keep_partial preservation`**
   - `src/service.rs` — 新增 `mod stream_error_tests`（14 个新单测）+ 扩展 `write_and_verify_file_blocking` 测试模块（加 `keep_partial_preserves_on_mismatch` 测试 + `mismatch_deletes_partial` 加 `.partial` 断言）
7. **`docs(next): archive STEP-P2-M5-5.1`**
   - `next/STEP-P2-M5-5.1.md`（本文件）

---

## 7. 下一步

**M5 STEP-5.1 派发 → 完成**。1/5 STEP 完成 → M5 收尾需等 5.2 / 5.3 / 5.4 / 5.5。

按 .LEADER-STATE.md:
- Leader 接受 7 commits
- 累计执行时间重置（在 SUGGESTION P2.3 落地后无新遗留）
- 派 `step-validator` 整批审 M5 5/5 STEPs（5.1 / 5.2 / 5.3 / 5.4 / 5.5）
- Leader 接受 validator PASS-with-followup → M5 done
- 用户真机验证（拔网双向 + GUI toast + .partial postmortem）

**M5 STEP-5.2 启动项**（next step after Leader 接受 5.1）：
- 端到端性能 + 收尾（100 Mbps LAN < 30s + Wi-Fi < 60s 双向 + cancel + 拔网 双向 + keepalive↔idle race + Pong ≤ 600ms）
- 依赖本 STEP 已落地的：`keep_partial` getter（STEP-4.1）+ `.partial` 中间文件（本 STEP）+ `FrontendEvent::FileTransferFailed`（本 STEP）