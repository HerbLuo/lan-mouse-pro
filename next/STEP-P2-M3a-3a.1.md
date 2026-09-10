# STEP-P2-M3a-3a.1 — 文件元数据采集 (FileEntry + collect_files)

> PLAN §3 M3a / STEP-3a.1 (FileEntry { name, size, mime, sha256 } + collect_files + 流式 sha256)
> 执行日期：2026-09-10　实际耗时：~25 min
> 结论：✅ 通过（cargo build 全绿 / cargo test --workspace 393 pass / 0 fail / cargo fmt clean / file_meta clippy 0 warning / 预存 connect.rs clippy 12 warnings 维持不增 / 12 个新单测全绿 含 200 MiB 真实写盘验证）

---

## 1. 做了什么

### 1.1 改动文件

| 文件 | 改动类型 | 备注 |
|---|---|---|
| `src/clipboard/file_meta.rs` | **新建**（371 行） | FileEntry + FileMetaError + collect_files + stream_sha256 + detect_mime + 12 个单测 |
| `src/clipboard/mod.rs` | 注册模块 + re-export | `pub mod file_meta;` + `#[allow(unused_imports)] pub use file_meta::FileEntry;`（forward-compat，等 STEP-3a.2 消费） |
| `Cargo.toml` | 加 dep feature | `[dependencies] tokio` 加 `fs` feature；`[dev-dependencies]` 加 `tempfile = "3"`（已在 Cargo.lock 间接存在，显式声明自描述） |

### 1.2 关键设计点

#### FileEntry 结构

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub name: String,
    pub size: u64,
    pub mime: String,
    pub sha256: [u8; 32],
}
```

- **`PartialEq + Eq`**：dispatcher 的去重逻辑按 sha256 比对；envelope 自身不 filter，让 service 层决定（PLAN §3 M3a 评审 #3 2nd）。

#### collect_files 行为契约

- **拒绝目录**：`FileMetaError::IsDirectory(path)` —— 第一处目录即整批中断（不递归，递归展开展后续 STEP）。
- **> 4 GiB 警告**：`should_mark_too_large(size)` → `MIME_TOO_LARGE = "application/x-too-large"` + `log::warn!`；**不计算 sha256**（接收端应该直接拒绝，省去对不会传输的文件的分钟级哈希）。
- **sha256 流式**：`tokio::fs::File::open` + 64 KiB `vec![0u8; 64*1024]` buffer + 循环 `read` + `Sha256::update(&buf[..n])`；永不分次 `Vec::with_capacity(file_size)`，符合 PLAN §3 STEP-3a.1 "不一次性 `Vec::with_capacity(file_size)`" 硬约束。

#### 辅助常量 / 函数（可单测）

```rust
pub const FOUR_GIB: u64 = 4 * 1024 * 1024 * 1024;
pub const MIME_TOO_LARGE: &str = "application/x-too-large";
pub fn should_mark_too_large(size: u64) -> bool { size > FOUR_GIB }
```

把 4 GiB 边界判定从 `collect_files` 内联抽出 → 单测可 pin 边界（`should_mark_too_large_threshold_is_4gib`）而**不**写 4 GiB 真实文件。

### 1.3 未触碰（scope 守纪）

- **dispatcher / service / cache / http3** —— STEP-3a.2 才接
- **现有 backend trait** (macos.rs / windows.rs / linux.rs) —— 0 改动
- **wire 协议** —— `lan-mouse-proto::ClipboardFiles` 已在 M0a 加好
- **P2 backlog 已押后项** (M2a/M2b P2.1-P2.6 / P3.1-P3.5) —— 0 改动
- **lan-mouse-ipc / lan-mouse-vue** —— M4 范围

---

## 2. 验证结果

### 2.1 单测覆盖（12 个，全绿）

| 测试 | 验证目标 | 实测耗时 |
|---|---|---|
| `collect_files_returns_single_file_with_correct_sha256` | 1 KiB 单一文件 → sha256 与 sha2 crate reference 一致 | < 0.1s |
| `collect_files_returns_multiple_files_with_independent_sha256` | 3 个不同文件各自 sha256 独立正确（catch 跨条目 hasher 串味） | < 0.1s |
| `collect_files_returns_error_for_directory` | 目录输入 → `FileMetaError::IsDirectory` variant | < 0.1s |
| `collect_files_with_empty_slice_returns_empty_vec` | 空切片 → 空 Vec，不触 FS | < 0.1s |
| `collect_files_missing_path_returns_io_error` | 不存在路径 → `FileMetaError::Io` variant | < 0.1s |
| `stream_sha256_1kib_correct` | 1 KiB 流式 sha256 正确（partial read 路径） | < 0.1s |
| `stream_sha256_1mib_correct` | 1 MiB 流式 sha256 正确（16 个 full read） | < 0.1s |
| **`stream_sha256_200mib_correct`** | **200 MiB 流式 sha256 正确（3200 个 read）** | **~5-8s** |
| `detect_mime_recognises_common_extensions` | 12 个扩展名（含 PNG/JPG/BMP/PDF/TXT/MD/BIN/无扩展/多扩展）映射 + 大小写不敏感 | < 0.1s |
| `should_mark_too_large_threshold_is_4gib` | 4 GiB 边界：0 / 1 KiB / 4 GiB / 4 GiB+1 / u64::MAX | < 0.1s |
| `mime_too_large_constant_is_stable` | `MIME_TOO_LARGE` 字符串稳定（接收端短路契约 pin） | < 0.1s |
| `file_meta_error_display_messages_are_stable` | 两个 error variant Display 文案稳定（dispatcher log + 后续 grep 依赖） | < 0.1s |

### 2.2 全套门（PLAN §3 完成标志）

| 闸门 | 命令 | 结果 |
|---|---|---|
| **Build** | `cargo build -p lan-mouse` | ✅ Finished `dev` profile (16.97s cold / 4.45s warm) |
| **Test (workspace)** | `cargo test --workspace` | ✅ **393 pass / 0 fail**（基线 381 + 12 个新 file_meta 测试）|
| **Test (file_meta 子集)** | `cargo test -p lan-mouse --lib clipboard::file_meta` | ✅ 12/12 pass（200 MiB 测试在内）|
| **Format** | `cargo fmt --all -- --check` | ✅ 0 diff（rustfmt auto-applied 2 处 cosmetic）|
| **Clippy (file_meta)** | `cargo clippy -p lan-mouse --all-targets` | ✅ file_meta 0 warning |
| **Clippy (workspace)** | 同上 | ⚠️ 12 warnings 维持（全部预存于 `src/connect.rs`，与本 STEP 无关；验证方法：`git stash` 后同样 12 warnings）|

---

## 3. 与 PLAN 的偏差

### 偏差 #1：tokio 加 `fs` feature

**PLAN 假设**：`tokio` 已在 workspace 直接依赖（line 55-64），`io-util` 已含，但 `tokio::fs::File::open` 需要 `fs` feature。
**实际**：Cargo.toml line 56 之前 features 列表无 `fs` → 加 `"fs"` 项。
**影响**：仅 build-time feature flag，不改 API / 不增二进制体积（tokio `fs` 在单 crate 编译时已是 no-op 链接）。

### 偏差 #2：FileEntry re-export 加 `#[allow(unused_imports)]`

**PLAN 假设**：`pub use file_meta::FileEntry;` 在 mod.rs 顶层导出，dispatcher 直接 `use crate::clipboard::FileEntry;`。
**实际**：STEP-3a.2 才消费，目前 lib build 出 `unused_imports` warning。
**处理**：加 `#[allow(unused_imports)]` + 注释说明 STEP-3a.2 才会真正引用；保持 forward-compat 公共路径稳定，避免 STEP-3a.2 落地时再次改 mod.rs。

### 偏差 #3：tempfile 加为显式 dev-dep

**PLAN 假设**：dev 单测可直接用 `tempfile`。
**实际**：`tempfile 3.27.0` 已在 Cargo.lock（被 h3 间接依赖），但作为 dev-dep 使用需要显式声明。
**处理**：加 `tempfile = "3"` 到 `[dev-dependencies]`，附注释说明 "已间接存在，显式声明自描述"。

---

## 4. 处理的 SUGGESTION 项

**无新增 SUGGESTION**。本 STEP 范围纯粹是新建模块 + 注册 + 单测；无发现需要在后续 STEP 处理的非阻塞小问题。

---

## 5. 闸门检查

| 闸门 | 结果 |
|---|---|
| **时间门** | ✅ ~25 min（< 1h 阈值；远低于 PLAN §3 M3a 估时 1.5h） |
| **milestone 边界门** | ✅ 0 触碰后续 M3a STEP-3a.2 / 3a.3 / 3a.4 / 3a.5 / M3b / M4 范围 |
| **闸 1 产物** | ✅ FileEntry / FileMetaError / collect_files / stream_sha256 + 12 测试全部落地 |
| **闸 1 依赖** | ✅ M2b 已归档（git log `0cfcab4`）；M3a 无外部前置依赖 |
| **闸 1 验收** | ✅ `cargo test --workspace` 393/0 |
| **闸 2 偏差** | 见 §3 三条偏差（feature flag + forward-compat allow + 显式 dev-dep）均已就地处理 |

---

## 6. 遗留

### 6.1 已知限制 / Out of Scope

- **递归目录展开不在本 STEP**：dispatcher 调 `collect_files` 前需自行 walk 目录（PLAN §3 STEP-3a.2 outbound 的责任）。
- **mime 检测仅按扩展名**：4 GiB 文件不计算 sha256；magic-byte sniffing（如 `infer` crate）属于 M3b / M4 forward-compat hook，本 STEP 用 extension 映射已足够（服务于"显式已知扩展"用例）。
- **不应使用一次性 `Vec::with_capacity(file_size)`**：已在 `stream_sha256` 实现 + doc 中明文 pin。

### 6.2 给 STEP-3a.2 的接续契约

- `crate::clipboard::FileEntry` —— dispatcher 直接用此类型构造 `ClipboardFiles { entries }`。
- `crate::clipboard::file_meta::collect_files(&[PathBuf]) -> Result<Vec<FileEntry>, FileMetaError>` —— service 层把 OS 剪贴板文件 URI 列表转 PathBuf 后调此函数。
- `MIME_TOO_LARGE = "application/x-too-large"` —— 接收端 dispatcher 应在 inbound branch 看到此 mime 时跳过 HTTP/3 fetch，直接 log warn。
- `FileMetaError::IsDirectory(path)` —— UI 应展示该 path 让用户重新选择非递归选项。
- `FileMetaError::Io(_)` —— 整个 batch 中止（当前 collect_files 在第一个 error 即返回）；UI 应回滚展示。

---

## 7. 下一步

按 PLAN §3 M3a 依赖顺序：

→ **STEP-3a.2**：源端 outbound（OS 剪贴板含文件 → `service::clipboard_dispatcher.file_branch` → `collect_files` → `ClipboardFiles { entries }` 走 StreamC + 文件字节暂存 `file_cache` 1 GiB LRU）。**人类准备**：macOS 真机 Finder 复制 200 MiB 文件 → 日志看到 entries 列表。

---

## 8. 建议 Commit 拆分布局（leader 执行）

按 PLAN §0 commit 卫生 + leader prompt "拆 commit: Cargo.toml deps / file_meta.rs / 单测 / 报告"：

```
<type>: <subject>

chore(deps): enable tokio fs feature + tempfile dev-dep for file_meta
  - tokio = { ..., features = ["fs", ...] } (M3a STEP-3a.1)
  - [dev-dependencies] tempfile = "3"
  归档: next/STEP-P2-M3a-3a.1.md
  Co-Authored-By: Claude Code <noreply@anthropic.com>

feat(clipboard): add file metadata collection (M3a STEP-3a.1)
  - new src/clipboard/file_meta.rs (FileEntry + FileMetaError +
    collect_files + stream_sha256 + detect_mime)
  - register pub mod file_meta; + pub use FileEntry at module root
  - 12 unit tests: 1 KiB / 1 MiB / 200 MiB sha256 + dir reject +
    oversize marker + mime map + error display
  归档: next/STEP-P2-M3a-3a.1.md
  Co-Authored-By: Claude Code <noreply@anthropic.com>

docs: archive STEP-P2-M3a-3a.1
  归档: next/STEP-P2-M3a-3a.1.md
  Co-Authored-By: Claude Code <noreply@anthropic.com>
```

> **拆分说明**：file_meta.rs 是 impl + tests 单文件，无法拆"单测"成独立 commit（除非把 tests 抽到 `tests/file_meta.rs` 集成测试，超出本 STEP scope）。把 impl + tests + mod.rs 注册合并到 commit 2 是最低成本路径；commit 1 保持纯 deps。
