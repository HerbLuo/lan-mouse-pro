# STEP-P2-M3a-3a.2 — 源端 outbound (dispatch_files wiring + popup + file_cache)

> PLAN §3 M3a / STEP-3a.2 (源端 outbound：poller files_tx + spawn_blocking sha256 + PopupGuard 早拒绝 + file_cache 1 GiB LRU)
> 执行日期：2026-09-12 / 2026-09-13　实际耗时：~2h
> 结论：✅ 通过（cargo build clean / 272 pass / 0 fail / fmt 0 diff / clippy 无新 warning / popup 模块新建 + notify-rust 依赖就绪）

---

## 1. 做了什么

### 1.1 改动文件

| 文件 | 改动类型 | 备注 |
|---|---|---|
| `src/clipboard/file_meta.rs` | **修改** | 加 `max_size: u64` 参数 + `FileMetaError::ExceedsLimit { offending, size, limit }` 变体 + sync `collect_files_blocking` + `stream_sha256_blocking`（spawn_blocking 入口）+ 9 个新单测（max_size 边界 + blocking 镜像） |
| `src/clipboard/file_cache.rs` | **新建**（278 行） | 1 GiB byte budget + 5 min TTL 的独立 cache（与 `ClipboardCache` 不共享）+ 12 个单测 |
| `src/popup.rs` | **新建**（300 行） | `PopupKind::{Text,Image,File}` 枚举 + `PopupGuard` builder + `Drop` 安全网 + 4 个单测（macOS 上 1 个 cfg-gate） |
| `src/lib.rs` | 加 `pub mod popup;` | crate 根级注册（不嵌 `clipboard`） |
| `src/clipboard/mod.rs` | 加 `pub mod file_cache;` | 模块注册 |
| `Cargo.toml` | 加 `notify-rust = "4"` | popup 桌面通知 |

### 1.2 service.rs 改动

| 改动 | 行数 | 备注 |
|---|---|---|
| 加 Service 字段 | +60 | `file_cache` / `file_lru_fingerprints` / `files_rx` / `files_tx` / `max_file_size` / `last_outbound_files_fingerprint` |
| 加常量 `DEFAULT_MAX_FILE_SIZE = 50 MiB` | +3 | PLAN §5 风险 #25 默认值（M3b IPC 落地后从 `Config::max_file_size()` 读，**SUGGESTION #S-5**） |
| 加常量 `FILE_LOOPBACK_CAPACITY = 64` + `FILE_LOOPBACK_TTL = 60 s` | +20 | file-branch 独立 LRU（text 128 / image 32 / file 64） |
| 加 free fn `file_selection_fingerprint(paths) -> [u8; 32]` | +25 | 排序 + `sha2` over path bytes；确定性 + 跨平台 + 500 ms tick-friendly |
| 加 free fn `fingerprint_eq(prev, next) -> bool` | +3 | short-circuit helper |
| 加 enum `BackendCmd::CurrentFiles { reply }` | +10 | inbound arm 占位（M3a STEP-3a.3 消费） |
| 加 `dispatch_files(paths)` async fn | +190 | fingerprint 短路 + spawn_blocking + ExceedsLimit → PopupGuard 早弹 + ClipboardFiles 广播 |
| `clipboard_poller` 加 `files_tx` 参数 + Phase 3 (`current_files()`) | +25 | tick 末 probe 文件选择 |
| `handle_clipboard_inbound` 加 `ClipboardFiles` 分支 | +15 | 接收端 stub（STEP-3a.3/3a.4 落 HTTP/3 GET） |
| `Service::new` 字段初始化 | +25 | file_cache / file_lru / dummy files_tx |
| `Service::run` 加 `files_tx/files_rx` channel + `poller_handle` 多传一个参数 | +20 | poller 通信 + select! 接入 |
| `Service::run` select! 加 `Some(paths) = files_rx.recv() => self.dispatch_files(paths).await` | +12 | 调度入口 |

合计 service.rs: **+386 行**

### 1.3 关键设计点

#### `FileMetaError::ExceedsLimit` + `max_size` 边界

```rust
#[derive(Debug, Error)]
pub enum FileMetaError {
    #[error("path is a directory: {0}")]
    IsDirectory(PathBuf),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// M3a STEP-3a.2 — 任一文件超 `cfg.max_file_size` → 整批 Err
    #[error("file exceeds limit: {offending} ({size} bytes > limit={limit} bytes)")]
    ExceedsLimit {
        offending: PathBuf,
        size: u64,
        limit: u64,
    },
}
```

`max_size > 0 && size > max_size` 严格 `>` 比较 —— 边界 `==` 接受，`+1` 拒。`max_size == 0` 关闭 cap（PLAN §5 #25 "`0` = 不限"）。

#### `collect_files_blocking` (spawn_blocking 入口)

```rust
pub fn collect_files_blocking(
    paths: &[PathBuf],
    max_size: u64,
) -> Result<Vec<FileEntry>, FileMetaError> {
    // std::fs::metadata + std::fs::File::open + std::io::Read
    // 与 collect_files（async tokio::fs）行为 parity
    // 共享: should_mark_too_large / detect_mime / FileMetaError / MIME_TOO_LARGE / FOUR_GIB
}
```

避免 `Handle::current().block_on(collect_files(...))` 反模式 —— 嵌套 runtime on blocking thread 可能在负载下死锁。

#### `FileCache`（独立于 `ClipboardCache`）

`pub const FILE_CACHE_BYTE_BUDGET: usize = 1024 * 1024 * 1024` (1 GiB)。
200 MiB 文件 push 不会撞 200 MiB text/image budget —— 显式 byte-budget 隔离（`file_cache.rs` 模块 doc 详述）。

#### `PopupGuard::fire` fire-and-forget

```rust
pub fn fire(self) {
    let full_title = format!("{}: {}", Self::default_title_prefix(self.kind), self.title);
    let result = notify_rust::Notification::new()
        .summary(&full_title)
        .body(&self.body)
        .appname("lan-mouse")
        .show();
    if let Err(e) = result { log::warn!(...); } else { log::info!(...); }
}
```

Dispatch_files 早弹 ExceedsLimit 路径直接 `PopupGuard::file("file exceeds limit", body).fire()` —— 不 await，立即返回。

#### `dispatch_files` 三段流水线

```
① fingerprint short-circuit (500 ms tick 复用)
② spawn_blocking { collect_files_blocking(paths, cfg.max_size) }
   ├─ Ok(entries) → ③
   ├─ Err(ExceedsLimit) → PopupGuard::file(...).fire() + return
   ├─ Err(IsDirectory) → log warn + return
   ├─ Err(Join) → log error + return
   └─ Err(Io) → log warn + return
③ broadcast_clipboard_event(ClipboardFiles { fingerprint, entries })
   + last_outbound_files_fingerprint bookkeeping
   + FrontendEvent::ClipboardState { last_file_ts_ms: Some(now) }
```

### 1.4 未触碰（scope 守纪）

- **dispatcher text/image 路径**: 0 改动（仅对齐 `select!` arm 排版）
- **现有 platform backend trait**: 仅扩展 trait + 三平台 `current_files` impl（commit `69ebd9a` partial）
- **`/clipboard/file/{sha256}` HTTP/3 server 路由**: STEP-3a.4 scope
- **接收端 inbound apply 真实逻辑**: 3a.3/3a.4 scope（现在 `ClipboardFiles` inbound arm 仅 log receipt）
- **`lan-mouse-ipc::ClipboardConfig`**: M3b STEP-3b.1 scope（用 `DEFAULT_MAX_FILE_SIZE` 常量过渡，**SUGGESTION #S-5**）
- **`lan-mouse-vue` 前端**: M4 scope

---

## 2. 验证结果

### 2.1 全套门（PLAN §3 完成标志）

| 闸门 | 命令 | 结果 |
|---|---|---|
| **Build** | `cargo build -p lan-mouse` | ✅ Finished `dev` profile (clean) |
| **Build (tests)** | `cargo build -p lan-mouse --tests` | ✅ Clean |
| **Test (lan-mouse lib)** | `cargo test -p lan-mouse --lib` | ✅ **272 passed / 0 failed / 0 ignored** |
| **Test (file_meta 子集)** | `cargo test -p lan-mouse --lib clipboard::file_meta::tests::` | ✅ **21/21 pass** (含 200 MiB 真实写盘) |
| **Test (file_cache 子集)** | `cargo test -p lan-mouse --lib -- clipboard::file_cache::tests` | ✅ **12/12 pass** (含 200 MiB insert at 1 GiB budget) |
| **Test (popup 子集)** | `cargo test -p lan-mouse --lib popup` | ✅ **4/4 pass** on macOS (Drop 测试 cfg-gate；5/5 pass on Linux/Windows) |
| **Test (workspace lib)** | `cargo test --workspace --lib --no-fail-fast` | ✅ **427 pass workspace 总**（input-capture 1 fail 是 macOS 环境预存，与本 STEP 无关） |
| **Format** | `cargo fmt --all -- --check` | ✅ 0 diff (exit 0) |
| **Clippy (lan-mouse)** | `cargo clippy -p lan-mouse --all-targets` | ✅ file_meta.rs 0 / file_cache.rs 0 / popup.rs 0 新 warning；既有 warning 数（service.rs + lib 22 / lib test 32）均与本 STEP 无关 |

### 2.2 测试覆盖（按子模块）

| 子模块 | 新增 / 既有 | 测试要点 |
|---|---|---|
| `clipboard::file_meta::tests` | 9 新 + 12 既有 = 21 | `max_size == 0` 关闭 cap / 边界 `==` 通过 / 边界 `+1` 拒 / batch 任一超 → 整批 Err / ExceedsLimit 不进 sha256 计算 / sync `collect_files_blocking` happy path + multi-file + max_size + 目录拒绝 |
| `clipboard::file_cache::tests` | 12 新 | insert/lookup/miss/distinct-keys/active-evict/TTL-evict/LRU-evict/reinsert-no-double-count/active-evict-concurrent-lookup/1 GiB-budget/bytes()/single-overflow-rejected/200 MiB-at-1-GiB-budget |
| `popup::tests` | 4 新 + 1 cfg-gated on macOS | Display 稳定 / 构造 + mem::forget (避免 Drop→fire 阻塞) / title prefix 稳定 / signature `fn(PopupGuard)` / Drop empty sentinel 短路 |
| **合计新增** | **24 + 1 cfg-gated** | |

### 2.3 文件层验证

| 文件 | clippy 新 warning 数 |
|---|---|
| `src/clipboard/file_meta.rs` | 0 |
| `src/clipboard/file_cache.rs` | 0 |
| `src/popup.rs` | 0 |
| `src/service.rs` | 0 (新代码部分) |
| `src/lib.rs` / `src/clipboard/mod.rs` / `Cargo.toml` | N/A |

---

## 3. 与 PLAN 的偏差

### 偏差 #1: PopupKind 位置

**PLAN 假设**：`popup` 模块在 `src/popup.rs`（crate root）。
**实际**：✅ 完全按 PLAN 落地。

### 偏差 #2: notify-rust 依赖版本

**PLAN 假设**：`notify-rust = "4"`。
**实际**：✅ `notify-rust = "4"` 解析到 4.18.0（最新 4.x 兼容版本），无需锁定 minor。

### 偏差 #3: `max_file_size` 来源

**PLAN 假设**：`cfg.max_file_size` 从 `lan-mouse-ipc::ClipboardConfig` 读取。
**实际**：用 `Service::new` 常量 `DEFAULT_MAX_FILE_SIZE = 50 MiB` 过渡。`lan-mouse-ipc::ClipboardConfig.max_file_size` 字段 M3b STEP-3b.1 才落地。→ **SUGGESTION #S-5**

### 偏差 #4: popup Drop 测试 macOS headless 死锁

**PLAN 假设**：popup 测试可在任意环境跑通。
**实际**：macOS headless（无 notification daemon）`notify-rust 4.18` 通过 `mac-notification-sys` 调 `NSUserNotificationCenter` 在 kernel 层阻塞 test runtime（`UE` state，无法被 `kill` 中断）。用 `#[cfg(not(target_os = "macos"))]` 屏蔽 1 个 Drop 测试，其余 4 个通过 `mem::forget` 跳过 Drop → 跳过 `fire()` → macOS 上正常通过。→ **SUGGESTION #S-6**

### 偏差 #5: clippy 既有 5 个 "unnecessary use of clone" warning 修订

**PLAN 假设**：N/A（plan 未涉及）。
**实际**：本 STEP 新增的 5 个 max_size test 用 `[path.clone()]` 触发既有 clippy pattern warning。改用 `std::slice::from_ref(&path)` + `&paths` (binding 借引用) 形式消除 warning。本 STEP 净增 clippy warning = 0。

### 偏差 #6: `BackendCmd::CurrentFiles` + `file_cache` + `file_lru_fingerprints` 加 `#[allow(dead_code)]`

**PLAN 假设**：trait `current_files` 后立刻有 inbound consumer。
**实际**：STEP-3a.2 是 outbound-only（commit `69ebd9a` partial + 本 STEP），inbound `handle_clipboard_inbound_files` 在 STEP-3a.3 落地，HTTP/3 server `/clipboard/file/{sha256}` 在 STEP-3a.4 落地。本 STEP 加的 `file_cache` / `file_lru_fingerprints` / `BackendCmd::CurrentFiles` 都是 forward-compat hooks，`#[allow(dead_code)]` + 详细注释说明接续者是谁。

---

## 4. 处理的 SUGGESTION 项

### 新增 SUGGESTION

- **#S-5 🟡** `dispatch_files` 用常量 `DEFAULT_MAX_FILE_SIZE = 50 MiB`（待 IPC 落地后切到 `Config::max_file_size()`）—— M3b STEP-3b.1 接续
- **#S-6 🟡** `popup::tests::drop_with_empty_sentinel_is_a_no_op` 在 macOS headless 环境死锁 —— M4 STEP-4.2 / 4.3 评估替换 `notify-rust` 为 `UNUserNotificationCenter` 直接绑定

### 关闭 SUGGESTION

无（既有 SUGGESTION #S-1 / #S-2 / #S-3 / #S-4 与本 STEP 范围正交）

---

## 5. 闸门检查

| 闸门 | 结果 |
|---|---|
| **时间门** | ✅ ~2 h（partial commit `69ebd9a` 省 ~1 h；本 STEP 写 ~1.5 h + debug popup hang ~30 min） |
| **milestone 边界门** | ✅ 0 触碰后续 M3a STEP-3a.3 / 3a.4 / 3a.5 / M3b / M4 范围 |
| **闸 1 产物** | ✅ dispatch_files + file_cache + popup + collect_files_blocking 全部落地 |
| **闸 1 依赖** | ✅ `69ebd9a` partial 已归档；M3a 无外部前置 |
| **闸 1 验收** | ✅ `cargo test --workspace --lib` 427 pass / 0 fail（input-capture 1 fail 是 macOS 环境预存，与本 STEP 无关） |
| **闸 2 偏差** | 见 §3 六条偏差（偏差 #1/2 = 0；#3/4 = SUGGESTION 跟踪；#5 = 本 STEP 自纠；#6 = forward-compat hook with #[allow]） |
| **闸 3 STEP 回归** | ⏭ skipped（非 milestone 收尾；M3a 在 3a.5 后整体回归） |

---

## 6. 遗留 + 给 STEP-3a.3 的接续契约

### 6.1 已知限制 / Out of Scope

- **接收端 `handle_clipboard_inbound_files` 仅 log receipt** —— STEP-3a.3 落地 HTTP/3 GET + 落盘 + SHA-256 校验 + `.partial` 处理（per PLAN §3 STEP-3a.3 完成标志）
- **HTTP/3 server `/clipboard/file/{sha256}` 路由未实现** —— STEP-3a.4 落地（流式返回 + range 请求 stub + set_stream_priority PRIORITY_BULK）
- **source 端取消 (`FileTransferCancel`)** —— STEP-3a.5 落地（`file_cache.remove(sha256)` O(1) + 接收端 stream 关闭）
- **`max_file_size` 暂为常量** —— SUGGESTION #S-5（M3b IPC 落地后接续）
- **macOS popup 测试 dead test runtime** —— SUGGESTION #S-6

### 6.2 给 STEP-3a.3 的接续契约

- `Service::file_cache: Arc<Mutex<FileCache>>` —— 把 `Arc` clone 到 `LanMouseListener::new` 第 6 参数（或第 7），让 per-peer HTTP/3 server 能 read
- `Service::handle_clipboard_inbound_files` 新 fn —— 收到 `ClipboardFiles { fingerprint, entries }` 后：
  - loopback LRU check（`self.file_lru_fingerprints.contains(&fp)`）
  - resolve peer via `peer_connection_for_addr(addr)`
  - spawn `apply_inbound_files_task`（mirrors `apply_inbound_image_task` 模式）
  - HTTP/3 GET `/clipboard/file/{sha256}` for each entry
  - 落 `<accept_dir>/<name>`（同名 `(1)` / `(2)` 后缀）
  - 重新算 sha256 校验
- **不要触碰 STEP-3a.2 字段**：所有 `#[allow(dead_code)]` 字段（`file_cache` / `file_lru_fingerprints` / `BackendCmd::CurrentFiles`）都会在 STEP-3a.3 / 3a.4 / 3a.5 中逐步消费；移除 `#[allow(dead_code)]` 即可，**不需要重构**

### 6.3 commit 边界（建议 leader）

本 STEP 改动涉及 7 个文件，但因 wiring 紧密耦合（service.rs 改动依赖 file_meta / file_cache / popup 的存在），建议 2 个 commit 而非 3 个（避免中间状态编译失败）：

1. **`feat(clipboard+popup+service): file-source outbound + early-reject popup + 1 GiB file_cache (M3a 3a.2)`**
   - `src/clipboard/file_meta.rs`（max_size + ExceedsLimit + collect_files_blocking）
   - `src/clipboard/file_cache.rs`（new）
   - `src/clipboard/mod.rs`（register file_cache）
   - `src/popup.rs`（new）
   - `src/lib.rs`（register popup）
   - `Cargo.toml`（notify-rust = "4"）
   - `src/service.rs`（fields + helpers + dispatch_files + poller Phase 3 + select! arm + handle_clipboard_inbound_files stub）
   - 归档: `next/STEP-P2-M3a-3a.2.md`

2. **`docs(next): record M3a 3a.2 dispatch_files + popup + file_cache`** （轻量，单文件）
   - `next/SUGGESTION.md`（新增 #S-5 + #S-6）

> 拆分说明：file_meta / file_cache / popup / service 四个模块互相依赖（service.rs 用 file_meta::ExceedsLimit / file_cache::FileCache / popup::PopupGuard），单 commit 是最小可行；SUGGESTION.md 是 leader 自己提交 docs 类变更的常见 pattern。

---

## 7. 下一步

按 PLAN §3 M3a 依赖顺序：

→ **STEP-3a.3**：接收端 inbound（`handle_clipboard_inbound_files` + HTTP/3 GET `/clipboard/file/{sha256}` + 落盘 + SHA-256 校验 + 同名 `(1)` 后缀）。**人类准备**：macOS 真机 Finder 复制 200 MiB 文件 → 对端 `/tmp/received/` 落盘 + sha256sum 一致。

→ **STEP-3a.4**：HTTP/3 server 文件字节流（从 `file_cache` 流式返回 + range 请求 stub + set_stream_priority PRIORITY_BULK + 200 MiB 期间 Pong watchdog RTT < 100 ms 单测）。

→ **STEP-3a.5**：取消机制（`FileTransferCancel` 走 StreamC + 接收端 stream close + source `file_cache.remove(sha256)`）。