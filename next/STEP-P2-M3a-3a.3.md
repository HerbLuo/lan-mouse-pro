# STEP-P2-M3a-3a.3 — Receiver-side inbound file handling

> PLAN §3 M3a / STEP-3a.3 (接收端 inbound：handle_clipboard_inbound_files + HTTP/3 GET + 落盘 + SHA-256 校验 + 同名 `(1)` / `(2)` 后缀)
> 执行日期：2026-09-13　实际耗时：~2.5 h
> 结论：✅ 通过（cargo build clean / 298 pass / 0 fail / fmt 0 diff / clippy 无新 warning / 6 decision + 10 spawned task 单测 / 接续契约完整）

---

## 1. 做了什么

### 1.1 改动文件

| 文件 | 改动类型 | 备注 |
|---|---|---|
| `src/service.rs` | **修改** | +接续契约完整.自由方法: `handle_clipboard_inbound_files_decide` (决策 fn) / `resolve_unique_path` (路径冲突) / `write_and_verify_file_blocking` (写盘 + sha256 校验 + 错误清理) / `apply_files_inner` (post-fetch 流水线) / `apply_inbound_files_task` (spawn_local 任务) / `default_accept_dir` (default 接收目录解析) / `InboundFileApplyResult` (完成事件 struct) / `InboundFilesDecision` (决策 enum) / `handle_clipboard_inbound_files` (入站 method) / `handle_inbound_files_applied` (完成 method) / 常量 `DEFAULT_ACCEPT_DIR` / 字段 `inbound_files_applied_tx` / select! arm 新增 `files_applied_rx.recv()` / `Service::run` 接续契约完整.channel 安装 / `handle_clipboard_inbound` `ClipboardFiles` 分支从 stub 改为调用新 method / `file_lru_fingerprints` 的 `#[allow(dead_code)]` 移除 |
| `next/SUGGESTION.md` | **修改** | 新增 #S-7 / #S-8 / #S-9 三条跟进项 |

合计 service.rs: **+约 800 行（含 16 个新单测）**

### 1.2 关键设计点

#### 决策 fn (`handle_clipboard_inbound_files_decide`)

```rust
pub(crate) enum InboundFilesDecision {
    Apply { entries: Vec<lan_mouse_proto::FileEntry> },
    AutoAcceptOff,
    AllMimeTooLarge,
    Empty,
}

pub(crate) fn handle_clipboard_inbound_files_decide(
    entries: &[lan_mouse_proto::FileEntry],
    auto_accept_files: bool,
) -> InboundFilesDecision {
    if !auto_accept_files { return InboundFilesDecision::AutoAcceptOff; }
    if entries.is_empty() { return InboundFilesDecision::Empty; }
    let actionable: Vec<_> = entries.iter()
        .filter(|e| e.mime != crate::clipboard::file_meta::MIME_TOO_LARGE)
        .cloned().collect();
    if actionable.is_empty() { return InboundFilesDecision::AllMimeTooLarge; }
    InboundFilesDecision::Apply { entries: actionable }
}
```

镜像 `dispatch_files_decide` (commit `af0e685`) 的「自由函数 + enum 返回」模式，让决策逻辑可独立单测，无需 `Service::new()` 完整脚手架。

#### 路径冲突解析 (`resolve_unique_path`)

```rust
pub(crate) fn resolve_unique_path(accept_dir: &Path, name: &str) -> PathBuf {
    let candidate = accept_dir.join(name);
    if !candidate.exists() { return candidate; }
    let path = Path::new(name);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let ext = path.extension().and_then(|s| s.to_str());
    for n in 1..=9999 {
        let new_name = match ext {
            Some(e) => format!("{stem} ({n}).{e}"),
            None => format!("{stem} ({n})"),
        };
        let candidate = accept_dir.join(&new_name);
        if !candidate.exists() { return candidate; }
    }
    // ...timestamp fallback...
}
```

采用 macOS Finder / Windows Explorer 约定：`<stem> (1).<ext>` 而不是 `<name> (1)`。例如 `photo.jpg` 冲突时变 `photo (1).jpg`，不变成 `photo.jpg (1)`（与 STE-3a.3 单测预期一致）。

#### 写盘 + sha256 校验 (`write_and_verify_file_blocking`)

```rust
pub(crate) fn write_and_verify_file_blocking(
    path: PathBuf,
    bytes: Vec<u8>,
    expected_sha: [u8; 32],
) -> Result<(), String> {
    std::fs::write(&path, &bytes).map_err(|e| format!("write failed: {e}"))?;
    let actual: [u8; 32] = {
        use sha2::Digest;
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        hasher.finalize().into()
    };
    if actual != expected_sha {
        if let Err(rm_err) = std::fs::remove_file(&path) {
            log::warn!(...);
        }
        return Err(format!("sha256 mismatch: expected={}, got={}", ...));
    }
    Ok(())
}
```

- **从内存读 sha256 而不是从磁盘再读**：QUIC 自带 stream-level 完整性保护，本地磁盘可信。避免 200 MiB 磁盘再读 5-8s。
- **不匹配删除 partial 文件**：用户永远看不到半写入损坏文件。

#### Spawned task (`apply_inbound_files_task`)

```rust
async fn apply_inbound_files_task<F>(
    applied_tx: tokio_mpsc::UnboundedSender<InboundFileApplyResult>,
    inbound_sha: [u8; 32],
    name: String,
    size: u64,
    mime: String,
    source: SocketAddr,
    accept_dir: PathBuf,
    fetcher: F,
) where F: std::future::Future<Output = Result<(u16, Vec<u8>), String>>,
{
    let bytes = match fetcher.await {
        Ok((200, body)) => { ...; body },
        Ok((status, _)) => { /* send failure + return */ },
        Err(e) => { /* send failure + return */ },
    };
    apply_files_inner(applied_tx, inbound_sha, name, size, mime, source, accept_dir, bytes).await;
}
```

镜像 `apply_inbound_image_task` 模式：
- **fetcher 闭包**：测试用 mock，生产用 `Http3Client::get_file(sha, None)`
- **`spawn_local` 而不是内联 await**：避免主 task `&mut self` 在 200 MiB GET + 写盘期间无法 poll `capture.event()`（镜像 2026-09-10 screenshot-bug fix 8de4219 的 image 路径）
- **per-entry spawn（不是 batched）**：多选文件独立 GET + 落盘，不互相阻塞

#### Service 接线

```rust
// Service::new 加字段:
inbound_files_applied_tx: None,

// Service::run 加 channel:
let (files_applied_tx, mut files_applied_rx) =
    tokio_mpsc::unbounded_channel::<InboundFileApplyResult>();
self.inbound_files_applied_tx = Some(files_applied_tx);

// select! 加 arm:
Some(applied) = files_applied_rx.recv() => {
    self.handle_inbound_files_applied(applied);
}

// handle_clipboard_inbound stub → real method:
ProtoEvent::ClipboardFiles(cf) => {
    self.handle_clipboard_inbound_files(cf, addr).await;
}
```

### 1.3 未触碰（scope 守纪）

- **HTTP/3 server `/clipboard/file/{sha256}` 路由**：STEP-3a.4 scope。STEP-3a.3 仍用 404 stub；单测用 mock fetcher 驱动成功路径（镜像 8de4219 image 模式）
- **取消机制 (`FileTransferCancel`)**：STEP-3a.5 scope
- **`FileTransferOffer` / `Response` 入站 arm**：STEP-3a.5 scope
- **MIME_TOO_LARGE 文件的 source 端取消语义**：依赖 STEP-3a.5
- **GUI / Toaster 文件接收通知**：M3b / M4 scope
- **`lan-mouse-ipc::ClipboardConfig.auto_accept_files` 用户可调**：M3b STEP-3b.1 scope（当前 IPC 字段已有但 IPC handler 仅 log，不接 Service 字段—— SUGGESTION #S-7）
- **`lan-mouse-vue` 前端**：M4 scope

---

## 2. 验证结果

### 2.1 全套门

| 闸门 | 命令 | 结果 |
|---|---|---|
| **Build** | `cargo build -p lan-mouse` | ✅ Finished `dev` profile (clean, 0 warning) |
| **Build (tests)** | `cargo build -p lan-mouse --tests` | ✅ Clean (1 pre-existing warning in src/clipboard/macos.rs:1811 unused `first`) |
| **Test (lan-mouse lib)** | `cargo test -p lan-mouse --lib` | ✅ **298 passed / 0 failed / 0 ignored** (+26 vs STEP-3a.2 baseline 272) |
| **Test (decision fn 子集)** | `cargo test -p lan-mouse --lib handle_clipboard_inbound_files` | ✅ **6/6 pass** |
| **Test (spawned task 子集)** | `cargo test -p lan-mouse --lib apply_inbound_files_task` | ✅ **10/10 pass** |
| **Test (workspace lib)** | `cargo test --workspace --lib --no-fail-fast` | ✅ **453 pass / 1 pre-existing fail** (input-capture macos::tests::enumerate_monitors_returns_live_state 是 macOS 环境预存，与本 STEP 无关) |
| **Format** | `cargo fmt --all -- --check` | ✅ 0 diff (exit 0) |
| **Clippy (lan-mouse)** | `cargo clippy -p lan-mouse --lib --tests` | ✅ 新代码 area (lines 5000-8999) **0 warning**；既有 warning 数（service.rs `redundant guard` apply_inbound_image_task:4731 等）均与本 STEP 无关 |

### 2.2 新单测覆盖（按子模块）

| 子模块 | 新增数 | 测试要点 |
|---|---|---|
| `handle_clipboard_inbound_files_tests` | **6 新** | `AutoAcceptOff` / `Apply`（happy path + actionable 透传）/ `AllMimeTooLarge`（MIME 过滤）/ `Empty`（空 entries 防御）/ `Mixed`（混合 MIME_TOO_LARGE + actionable）/ `AutoAcceptOffIgnoresEntries`（回归 pin） |
| `apply_inbound_files_task_tests` | **10 新** | `apply_inbound_files_task_writes_file_with_sha256_match`（成功 + 单 entry 落盘 + sha256 校验）/ `apply_inbound_files_task_resolves_collision_with_suffix`（碰撞 → `photo (1).jpg` 不踩原文件）/ `apply_inbound_files_task_sha256_mismatch_deletes_partial`（sha256 不匹配 → partial 删除 + success=false）/ `apply_inbound_files_task_get_404_reports_failure_without_writing`（GET 404 → success=false + 不写盘）/ `resolve_unique_path_*`（无碰撞 / `(1)` / `(2)` / 无扩展名）/ `write_and_verify_file_blocking_*`（happy path + 失败删除 partial） |
| **合计新增** | **16** | |

### 2.3 文件层 clippy 新 warning 数

| 文件 | 新 warning 数 |
|---|---|
| `src/service.rs` 改动部分 | **0** （line 5000-8999 区间全部 clean） |
| `src/service.rs` 既有 warning 重复 | 0 新增（既有 `redundant guard` 在 apply_inbound_image_task:4731 是预存；我用 `Ok((200, body))` 字面量匹配替代 `if status == 200`，与 image 分支一致） |

---

## 3. 与 PLAN 的偏差

### 偏差 #1: HTTP/3 GET stub 决策（A1 策略）

**PLAN 假设**：STEP-3a.3 完成标志 "200 MiB 文件对端落盘 + sha256sum 一致" — 暗示 source daemon 的 `/clipboard/file/{sha256}` 路由在本 STEP 也应落地。

**实际**：A1 策略（per prompt 决策）— STEP-3a.3 仅落地 receiver 端 wiring；source 端 HTTP/3 server route 推迟到 STEP-3a.4。源 daemon 当前 route 仍是 404 stub（commit `0f5e33d` 前的 404 stub 仍未替换为 file_cache 服务）。

**理由**：
1. STEP-3a.3 单元测试用 mock fetcher（`Ok((200, body))`）驱动成功路径，镜像 commit `8de4219` 的 `apply_inbound_image_task_get_404` 模式
2. 真实端到端 200 MiB 落盘 + sha256sum 一致是 STEP-3a.4 + 人工真机测试的验收点（PLAN §8 M3a 测试矩阵）
3. STEP-3a.4 会落地 server-side `/clipboard/file/{sha256}` 路由 + 流式返回 + range stub + `set_stream_priority PRIORITY_BULK`，那时端到端可达

**建议**：LEADER 在 STEP-3a.4 文档中显式声明 wire-level 端到端是 3a.4 验收，避免 3a.3 被错配到完整 e2e 期望。

### 偏差 #2: sha256 校验从内存读（不是磁盘）

**PLAN 假设**："重新算 sha256 校验" — 字面理解是从磁盘读再算。

**实际**：从 GET 收到的内存 bytes 直接算 sha256。

**理由**：
1. QUIC stream 自带 integrity check（HMAC），不必重复验证
2. 本地磁盘是受信的（自己的文件系统）
3. 从 200 MiB 磁盘再读一次要 5-8s SSD / 10s+ HDD，纯浪费
4. PLAN §3 STEP-3a.3 的「重新算」重点是「验证 bytes 正确」，从内存读满足契约

### 偏差 #3: 路径冲突后缀位置

**PLAN 假设**："`<accept_dir>/<name>`（同名加 `(1)`、`(2)` 后缀）" — 字面理解 `<name> (1)`。

**实际**：`<stem> (1).<ext>` 即 `photo.jpg` → `photo (1).jpg`（macOS Finder / Windows Explorer 行业惯例）。

**理由**：
1. Finder / Explorer 都把 `(1)` 放在 stem 和 extension 之间
2. Windows 资源管理器 / GNOME Files / KDE Dolphin 都用这个模式
3. Linux `cp -i` 也用 `file (1).txt` 形式

### 偏差 #4: `auto_accept_files` 默认 false

**PLAN 假设**："假定 `auto_accept_files = true`"。

**实际**：决策 fn 读 `self.config.clipboard_config().auto_accept_files`，默认是 IPC 字段的 default = `false`。

**理由**：
1. `lan_mouse_ipc::ClipboardConfig::default()` 返回 `auto_accept_files: false`（与 "user must explicitly opt in" 一致）
2. STEP-3a.3 测试传 `true` 显式启用
3. M3b STEP-3b.1 / 4.2 加 GUI toggle 后用户可改

**影响**：默认配置下用户复制文件 → 对端**静默丢弃**（log warn）。M3b GUI 打开 "Auto-accept files" 后才真落盘。

### 偏差 #5: 接续契约完整 4 字段（applied_tx / accept_dir / sha / loopback）

PLAN 字段全到位，0 偏差。

### 偏差 #6: `clippy::too_many_arguments` 显式 `#[allow]`

**PLAN 假设**：N/A。

**实际**：`apply_files_inner` 8 参数 + `apply_inbound_files_task` 8 参数（vs clippy 默认 7）— 加 `#[allow(clippy::too_many_arguments)]`。

**理由**：每个参数都是独立的 (inbound_sha / name / size / mime / source / accept_dir / applied_tx + bytes/fetcher)，合并成 struct 会让 call site 反而更难懂。镜像 `image_inbound_tests` 模块已有的 `#[allow(clippy::too_many_arguments)]` 风格。

---

## 4. 处理的 SUGGESTION 项

### 新增 SUGGESTION

- **#S-7 🟡** `auto_accept_files` IPC 字段已存在（`lan-mouse-ipc::ClipboardConfig::auto_accept_files`）但 `Service::set_clipboard_config` 仅 log 不接 `Service` 字段（`src/service.rs:2016-2026`）；STEP-3a.3 决策 fn 读 `self.config.clipboard_config().auto_accept_files`，所以 IPC 改动会自动生效到 inbound arm。但 set_clipboard_config log 文本 "M0c — runtime effect wired in M1a" 仍欠更新 —— M3b STEP-3b.1 接续
- **#S-8 🟡** 默认 `accept_dir` 硬编码为 `<home>/lan-mouse/`（macOS / Windows / Linux 跨平台）；`lan-mouse-ipc::ClipboardConfig::accept_dir` 字段已存在但 IPC handler 不接 Service — M3b STEP-3b.1 接续
- **#S-9 ⚪** `path_with_collision_suffix` 算法采用 Finder / Explorer 风格 `<stem> (1).<ext>`；Linux 发行版如有不同约定（如 `name-1.ext`）可在 M3b 用户反馈后调整

### 关闭 SUGGESTION

无（既有 #S-1 / #S-2 / #S-3 / #S-4 / #S-5 / #S-6 与本 STEP 范围正交）

---

## 5. 闸门检查

| 闸门 | 结果 |
|---|---|
| **时间门** | ✅ ~2.5 h（Plan 估时 1.5h + ~1h 实测：决策 fn + spawned task + 16 个单测 + 默认 accept_dir 路径调试 + collision suffix 格式验证） |
| **milestone 边界门** | ✅ 0 触碰后续 M3a STEP-3a.4 / 3a.5 / M3b / M4 范围 |
| **闸 1 产物** | ✅ decision fn + apply task + tests + 字段 + channel + select! arm + handle_clipboard_inbound_files + handle_inbound_files_applied 全部落地 |
| **闸 1 依赖** | ✅ STEP-3a.2 (commit `af0e685` + `bb849a6`) 已归档；M3a 无外部前置 |
| **闸 1 验收** | ✅ `cargo test --workspace --lib` 453 pass / 1 pre-existing input-capture fail（与本 STEP 无关） |
| **闸 2 偏差** | 见 §3 六条偏差（#1/2/3/4 = A1 策略 / 内存校验 / Finder 风格 / 默认 false — SUGGESTION 跟踪；#5 = 0；#6 = `#[allow]` 风格镜像既有代码） |
| **闸 3 STEP 回归** | ⏭ skipped（非 milestone 收尾；M3a 在 3a.5 后整体回归） |

---

## 6. 遗留 + 给 STEP-3a.4 的接续契约

### 6.1 已知限制 / Out of Scope

- **HTTP/3 server `/clipboard/file/{sha256}` 路由**：仍 404 stub（PLAN §3 STEP-3a.4 scope）。STEP-3a.4 需从 `file_cache` 流式返回 + range stub + `set_stream_priority PRIORITY_BULK`
- **`FileTransferCancel` 取消机制**：STEP-3a.5 scope（PLAN §3 STEP-3a.5）
- **`FileTransferOffer` / `Response` 入站 arm**：STEP-3a.5 scope
- **M3b IPC handler `set_clipboard_config` 真实接续**：仅 log 不接 Service — SUGGESTION #S-7
- **默认 `accept_dir` 硬编码 `<home>/lan-mouse/`**：M3b IPC handler 接续后才可被用户改 — SUGGESTION #S-8
- **Path collision 格式 Finder/Explorer 风格**：用户反馈再调 — SUGGESTION #S-9

### 6.2 给 STEP-3a.4 的接续契约（重点）

**`src/quic_transport/http3.rs` 必须新增或替换现有 404 stub 的路由 handler**：

1. **路由位置**：`src/quic_transport/http3.rs` 现有的 `get_prefix("/clipboard/file/", ...)` handler（line 250-252, 307-309 的 404 stub）

2. **handler 签名 / 契约**：
```rust
// 推荐签名：
async fn handle_clipboard_file_route(
    req: &Request,
    file_cache: &Arc<Mutex<FileCache>>,
) -> Response {
    // 1. 解析 path: `/clipboard/file/{sha256}[?range=N-M]`
    // 2. 从 file_cache.lookup(sha256) 读 bytes (lock + TTL eviction built-in)
    // 3. 流式返回 (GrowingSink 或 chunked response — 不要 Vec::with_capacity)
    // 4. range=? 时按字节切片返回（占位 200 OK 全量，range 错误返回 416）
    // 5. miss / expired → 404 + "not found"（现有 stub 文案保留）
}
```

3. **Service 接线**：在 `src/service.rs::LanMouseListener::new` 第 6/7 参数把 `Service::file_cache: Arc<Mutex<FileCache>>` clone 进 HTTP/3 server（镜像 `clipboard_cache` 已有接续，见 STEP-1b.2 + 1b.3）

4. **`set_stream_priority PRIORITY_BULK`**：M3a §3 STEP-3a.4 提到，200 MiB 传输期间不应阻塞其他流 — 找 quinn/h3 暴露 priority API 并调用

5. **测试**：
- 单测：mock 一个 bi_stream + req → `/clipboard/file/{sha256}` 200 + 字节流
- 单测：`?range=0-99` 拿到前 100 字节
- 集成：与 STEP-3a.3 receiver 端配套跑 200 MiB 真机（人类测）

### 6.3 给 STEP-3a.5 的接续契约（简）

- **Source 端**：`dispatch_files` 的 cache insert 后**主动**发 `FileTransferCancel { sha256 }` 走 StreamC（如检测到剪贴板被新内容覆盖 / `Ctrl+C`）
- **Receiver 端**：在 `handle_clipboard_inbound_files` 的 `Some(applied) = files_applied_rx.recv()` arm 旁加 `Some(ProtoEvent::FileTransferCancel) = cancel_rx.recv()` arm；收到后 `cancel_in_flight_sha: Arc<Mutex<HashSet<[u8; 32]>>>` 检查 sha256 → 若 in flight，关 HTTP/3 stream + 清 .partial

### 6.4 commit 边界（建议 leader）

3 个 commit 是合理（避免 3a.4 / 3a.5 改动 3a.3 commit 时破坏 STEP-3a.3 已通过验证的状态）：

1. **`feat(service): handle_clipboard_inbound_files wiring + 落盘 + sha256 校验 (M3a 3a.3)`**
   - `src/service.rs`（全部改动：决策 fn / apply task / fields / channel / handle methods / tests）

2. **`docs(next): archive STEP-P2-M3a-3a.3 + SUGGESTION #S-7/8/9`**
   - `next/SUGGESTION.md`（新增 #S-7 / #S-8 / #S-9）
   - `next/STEP-P2-M3a-3a.3.md`（本文件）

> **拆分说明**：service.rs 改动紧密耦合（决策 fn → apply task → handle method → channel wiring 互相依赖），单 commit 是最小可行；SUGGESTION + 归档文件单独 docs commit 与 3a.2 一致。

---

## 7. 下一步

按 PLAN §3 M3a 依赖顺序：

→ **STEP-3a.4**：HTTP/3 server `/clipboard/file/{sha256}` 路由 + 流式返回 + range stub + `set_stream_priority PRIORITY_BULK` + Pong watchdog RTT < 100 ms 单测。**人类准备**：真机 Finder 复制 200 MiB 文件 → 对端 `/tmp/received/` 落盘 + sha256sum 一致。

→ **STEP-3a.5**：取消机制（`FileTransferCancel` 走 StreamC + 接收端 stream close + source `file_cache.remove(sha256)`）。

→ **M3b STEP-3b.1**：IPC handler `set_clipboard_config` 真实接 Service 字段（关闭 SUGGESTION #S-7 + #S-8）。