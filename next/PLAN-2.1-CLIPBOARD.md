# PLAN-2.1 — 跨设备文件接收（auto-accept）+ GUI 配置 + CLI + 剪贴板回灌

> 目标：在 M3a 已落地的 `dispatch_files` / `file_cache` / HTTP/3 `/clipboard/file/{sha256}` 之上，加接收端 IPC 配置 + 拔网处理 + GUI 配置 + CLI + 接收端剪贴板回灌。承接 `next/PLAN-2-CLIPBOARD.md`（M0a / M0b / M0c / M1a / M1b / M2a / M2b / M3a 已完成）。
>
> **2026-09-13 重组**：原 M3b 8 STEPs 拆分为 **M4 3 STEPs + M5 5 STEPs**。M4 = IPC schema + 平台 `set_files` + 剪贴板回灌（4.1 / 4.2 / 4.3）；M5 = 拔网 + 端到端性能 + Vue IPC 绑定 + GUI 配置 + CLI（5.1 - 5.5）。

## 0. 范围

### In Scope

**文件接收 pipeline 收尾（从 M3a 接力）**：
- IPC `ClipboardConfig` 扩展（`enabled` / `accept_dir: PathBuf` / `ignore_*` / `max_file_size` / `keep_partial` / `inject_to_clipboard`）
- 拔网处理（HTTP/3 client stream error → `FrontendEvent::FileTransferFailed`）
- 端到端性能（200 MiB 100 Mbps LAN < 30 s + Wi-Fi < 60 s）+ cancel + 拔网
- Vue 类型 + IPC 绑定
- GeneralPanel + per-peer UI（no Toaster accept/reject — auto-accept only）
- CLI 集成（`lan-mouse-cli SetClipboardConfig` / `SetEnableClipboardTo`）
- 接收端剪贴板回灌（`handle_inbound_files_applied` 后调 `backend.set_files` + 防回环）

### Out of Scope

- 剪贴板格式协商（HTML / RTF）
- 文件断点续传（HTTP/3 `?range=` 已留接口）
- Wayland portal
- 任何前端框架切换

### 承接 PLAN-2-CLIPBOARD.md

- **wire-level 协议 / StreamC / HTTP/3 / FileEntry / file_cache / popup 模块均已在 M3a 落地**
- 本计划仅在 M3a 之上加 IPC + GUI + 拔网处理 + 剪贴板回灌
- M3a 5 STEP（3a.1 / 3a.2 / 3a.3 / 3a.4 / 3a.5）均已 PASS-with-followup 整批审

## 1. 架构概览（承接）

> 文件接收 layer 在 M3a 文件 transfer layer 之上
> - M3a：`dispatch_files` → `file_cache` → HTTP/3 `/clipboard/file` → 落盘
> - PLAN-2.1：落盘后调 `backend.set_files` → 接收端 OS 剪贴板；通过 IPC 推送 `FileTransferFailed` 给 GUI
> - IPC：`ClipboardConfig` 8 字段（`enabled` / `accept_dir` / `ignore_*` ×3 / `max_file_size` / `keep_partial` / `inject_to_clipboard`）

## 2. 路线图

| 里程碑 | AI 估时 | 状态 |
|---|---|---|
| **M4** — IPC `ClipboardConfig` + 平台 `set_files` + 剪贴板回灌 | ~4.5 h | ⏸️ 等用户验证 M3a 后启动 |
| **M5** — 拔网处理 + 端到端性能 + Vue IPC 绑定 + GUI 配置 + CLI | ~7.0 h | ⏸️ 等 M4 完成后启动 |
| **合计** | **~11.5 h** | |

> **2026-09-13 重组**：M4 聚焦"配置 + 平台底层 + 应用集成"完整闭环；M5 聚焦"网络/性能/UX/CLI"运维层

## 3. 详细步骤

### M4 — IPC `ClipboardConfig` + 平台 `set_files` + 剪贴板回灌

**目标**：M3a 已完成 200 MiB 文件端到端传输；M4 在此之上落地 **IPC `ClipboardConfig` schema**（含 `inject_to_clipboard` 字段）、**三平台 `ClipboardBackend::set_files` trait 实现**（macOS NSPasteboard / Windows CF_HDROP / Linux URI list）、**剪贴板回灌**（落盘后自动灌回本地剪贴板 + pre-stamp 防回环 + 4 类 skip condition）。M4 不涉及拔网处理、性能基准、Vue 类型 / UI、CLI —— 这些是 M5 范畴。

**AI 估时**：~4.5 h
**依赖**：M3a（已完成）

**用户决策（2026-09-13）**：
1. **Auto-accept only**（no dual-mode）—— GUI 是配置入口，**不是**交互入口；Toaster accept/reject buttons **out of scope**
2. **M4 / M5 拆分（2026-09-13 用户决策）**：原 M3b 8 STEPs 拆分为 **M4 3 STEPs + M5 5 STEPs**。拆分依据：M4 = "配置 + 平台底层 + 应用集成"完整闭环（4.1 IPC schema + 4.2 平台 `set_files` + 4.3 集成回灌），M5 = "网络 / 性能 / UX / CLI"运维层（5.1 拔网 + 5.2 性能 + 5.3 Vue IPC + 5.4 GUI + 5.5 CLI）。具体映射：`3b.1 → 4.1` / `3b.7a → 4.2` / `3b.7b → 4.3` / `3b.2 → 5.1` / `3b.3 → 5.2` / `3b.4 → 5.3` / `3b.5 → 5.4` / `3b.6 → 5.5`。重组动机：用户决策把 `set_files` trait 实现提前 —— IPC schema（4.1）+ 平台底层（4.2）+ 应用集成（4.3）作为 M4 完整闭环，使剪贴板回灌这一核心 UX 单独可达一个里程碑；M5 是其上的运维 / UX / CLI 配套
3. **2026-09-13 新增 STEP-4.2 / 4.3**（原 3b.7a / 3b.7b）：用户决策加"接收端剪贴板回灌" —— 文件落盘后自动把 path 灌回本地剪贴板，用户可直接 Cmd+V 粘贴。**2026-09-13 审阅拆步**（planer sub-agent 确认 `set_files` trait method 在 M3a 未落地 — commit `69ebd9a` 只实现了 `current_files()` + `watch_files()` 只读路径；Linux `src/clipboard/linux.rs:392-394` 明确标注 "No file-write path on Linux: M3a only needs the **read** path; `set_files` is out of scope"），原 STEP-3b.7 拆分为 4.2（trait `set_files` + macOS/Windows/Linux 三平台实现）/ 4.3（skip conditions + 防回环 + collector 接线 + `InboundFileApplyResult` 加 `error_kind` —— **GUI checkbox DOM 渲染延至 5.4**，TOML 字段在 4.1 已落地，IPC struct 扩展归 4.1）

**人类准备**：
- **配置接收目录**：在 config.toml 或 GUI 设置 `accept_dir = "/Users/me/Downloads/lan-mouse"`
- **真机回灌测试**：macOS Finder 复制文件 → 对端自动落盘 + 入剪贴板 → Cmd+V 直接粘贴（验证 pre-stamp 防回环 + 4 类 skip condition）
- **GUI 真机**：浏览器打开 GUI，`pnpm dev` 起 Vite；WebSocket console 可见

| STEP | 估时 | 任务 | 涉及文件 | 完成标志 |
|---|---|---|---|---|
| **4.1** | **1.5h** | **IPC `ClipboardConfig` 扩展 + 早拒绝 PopupGuard 串通**（drop `auto_accept_files`）：<br>**结构**：`lan_mouse_ipc::ClipboardConfig { enabled: bool, accept_dir: PathBuf, ignore_text: bool, ignore_images: bool, ignore_files: bool, max_file_size: u64, keep_partial: bool, inject_to_clipboard: bool }` —— `accept_dir` 改为**必填**（auto-accept means 始终需要 target，`Option` 移除）；`max_file_size` 默认 50 MiB、0 = 不限；`keep_partial` 默认 false（拔网后默认删除 .partial）；`inject_to_clipboard` 默认 `true`（`#[serde(default)]`）<br>**drop**：`auto_accept_files: bool`（auto-accept 是本计划唯一模式，不再需要 user toggle）<br>**保留**：`enabled: bool`（master toggle，区分"全局剪贴板监听开关"—— `enabled` 是 master on/off，与 `ignore_files: bool` per-kind ignore 是不同关注点）；`enabled = false` 时整个 dispatcher 不启，不影响键鼠<br>**wire-level 文件传输成功 + 早拒绝 PopupGuard**（沿用 M3a 已落地逻辑）：`PopupGuard::fire()` 在 ExceedsLimit / Cancel 等场景已落地；本 STEP 不改 popup.rs 主体，只把 max_file_size 默认值从 M3a 硬编码常量（`DEFAULT_MAX_FILE_SIZE = 50 MiB`）切到 `config.clipboard_config().max_file_size`，关闭 SUGGESTION #S-5 / #S-8<br>**Service 接线扩展**：扩展现有 `set_clipboard_config` handler（`src/service.rs:2155` 当前仅 `self.config.set_clipboard_config(cfg.clone())` + `config.write_back()` + log），新增把 `inject_to_clipboard` / `keep_partial` / `max_file_size` 等字段写入 `Service` 内部状态（如 `self.clipboard_inject_to_clipboard: bool` / `self.clipboard_keep_partial: bool` / `Service::max_file_size()` getter），并把 4.3 / 5.1 后续步骤所需的读取入口（`Service::clipboard_config()` 已有 getter 基础上扩展）落地。**这是新增工作，不是描述现状。**<br>**移除**：`FrontendEvent::FileTransferRequest` / `FrontendRequest::RespondFileTransfer` —— **Toaster accept/reject 取消后不需要这两个事件**<br>**TOML 段加字段**：`src/config.rs` TOML `[clipboard]` 段同步加 `inject_to_clipboard = true` 字段（默认值；用户可配）<br>**fmt/clippy/build** | `lan-mouse-ipc/src/lib.rs`（`ClipboardConfig` 8 字段 + serde round-trip）、`src/service.rs`（扩展 `set_clipboard_config` handler + 新增 `Service::max_file_size()` 等 getter）、`src/config.rs`（TOML `[clipboard]` 段加 `inject_to_clipboard` 字段） | serde round-trip 单测：缺字段 = default；`accept_dir: PathBuf` 必填语义；`max_file_size = 0` 时不限；`enabled = false` 时 dispatcher 不启；`inject_to_clipboard` 缺字段 = `true`；单测覆盖 drop `auto_accept_files` 兼容性；`src/config.rs` TOML `[clipboard]` 段 round-trip 单测（含 `inject_to_clipboard` 字段）；`set_clipboard_config` handler 写入新字段后下次 inbound 立刻生效（`Service::max_file_size()` getter 返回新值）；`cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` 全绿 |
| **4.2** | **1.5h** | **`ClipboardBackend::set_files` trait + 三平台实现**（**planer round 2 审阅拆步历史**：原 STEP-3b.7 的"前提：`set_files` 已落地"系 spec 假设错误 —— commit `69ebd9a` 仅含只读 `current_files()` + `watch_files()`，无 `set_files`；本 STEP 即补这块落地）：<br>**trait 加方法**：`src/clipboard/mod.rs` 在 `ClipboardBackend` trait 加 `fn set_files(&mut self, files: &[PathBuf])` —— **注意 `&mut self`**：与现有 `current_files` / `set_text` / `set_image` 同模式（不可变 `&self` 编译不过 —— `NSPasteboard` 写入需可变状态机、`OpenClipboard` 返回 handle 也需 mut self）；语义：把一组绝对路径灌入 OS 剪贴板（macOS NSPasteboard `NSFilenamesPboardType` / Windows `CF_HDROP` / Linux X11 `text/uri-list` 或 Wayland 同等 mime），调用方保证 paths 都已落盘且 SHA-256 校验通过；返回 `Result<()>`（失败 log warn，不 panic）<br>**macOS 实现**：`src/clipboard/macos.rs` 用 `NSPasteboard.general().clearContents()` + `writeObjects(&ns_array)` 灌入（**注意类型修正 — planer round 2 审阅**：`writeObjects` 签名是 `fn writeObjects(&self, objects: &NSArray<ProtocolObject<dyn NSPasteboardWriting>>) -> bool`，**不是** `NSArray<NSURL>`；NSURL 通过 `ProtocolObject::from_retained(nsurl)` 包装；objc2-app-kit 0.3.2 已为 NSURL 实现 `extern_conformance!(unsafe impl NSPasteboardWriting for NSURL {});`）。NSArray 构造：`NSArray::from_retained_slice(&[ProtocolObject::from_retained(NSURL::fileURLWithPath(&nsstring))])`<br>**Windows 实现**：`src/clipboard/windows.rs` 用 `OpenClipboard` + `EmptyClipboard` + `SetClipboardData(CF_HDROP, hdrop)` + `GlobalAlloc(GHND, ...)` + `GlobalLock` + `DragQueryFileW` 构造 DROPFILES 结构（DROPFILES header + 双重 null-terminated file paths；现有 windows.rs 已有 CF_DIBV5 / CF_BITMAP 写入经验可直接复用）<br>**Linux 实现**：`src/clipboard/linux.rs` —— X11 走 `xclip -selection clipboard -t text/uri-list -i`（子进程 `tokio::process`，stdin 写入 RFC 2483 URI list），Wayland 走 `wl-copy --type text/uri-list < file`；删掉"no file-write path on Linux"注释<br>**RFC 2483 URI list 格式细节**（**planer round 2 审阅补 — 参考 `src/clipboard/linux.rs:398` `wl-paste --type text/uri-list` 读取路径对称**）：`<file:///abs/path1>\r\n<file:///abs/path2>\r\n`，每行一个 URI，CRLF 结尾，多 URI 间 CRLF 分隔；以 `file://` 前缀 + 绝对路径编码；构造函数：`fn build_uri_list(paths: &[PathBuf]) -> String { paths.iter().map(|p| format!("file://{}\r\n", p.display())).collect() }`<br>**测试**：<br>• Windows 单测：mock `SetClipboardData(CF_HDROP, ...)` → 参数捕获 + DROPFILES bytes 解析验证（`DragQueryFileW` 解码对比原 paths）<br>• Linux 单测：mock `xclip` / `wl-copy` 子进程 → 拦截 `Command::spawn` 后断言 args 含 `-t text/uri-list` + stdin payload 含预期 CRLF URI list<br>• macOS 单测（**planer round 2 审阅补**：objc2 AppKit 难以纯 mock；**退化为集成式真实 pasteboard 写入断言** —— 调 `set_files` 前后比对 `NSPasteboard.general().changeCount()` 递增 + `readObjectsForClasses([NSURL.self], options: nil)` 拿到原 paths；与现有 `src/clipboard/macos.rs:1327, 1347` 真实 pasteboard 单测模式一致）<br>**fmt/clippy/build** | `src/clipboard/mod.rs`（trait 加方法）、`src/clipboard/macos.rs`、`src/clipboard/windows.rs`、`src/clipboard/linux.rs`（删 "out of scope" 注释 + 实现） | `cargo build -p lan-mouse --features <platform>` 编译通过；Windows / Linux 单测验证 mock 平台 API 被调用一次 + 参数正确；macOS 单测验证 changeCount 递增 + round-trip 读回 paths；`cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` 全绿 |
| **4.3** | **1.5h** | **接收端剪贴板回灌 + skip conditions + 防回环 + IPC 集成**（依赖 4.2）：<br>**调用点 + 时序**（**planer round 2 审阅修正 — 实际应在 `handle_inbound_files_applied`，不在 `handle_clipboard_inbound_files`**）：M3a 已落地的 `apply_inbound_files_task`（`src/service.rs:5678`）每 entry 独立 spawned；每 entry 落盘完成后通过 `InboundFileApplyResult { fingerprint, sha256, landed_path, error }` mpsc 回到主 `select!` 的 `handle_inbound_files_applied`（`src/service.rs:3762`）——**只有这里持有 `landed_path`**。<br>→ 本 STEP 真正的回灌代码插点：**collector 在 `handle_inbound_files_applied`** 累积 `HashMap<[u8; 32], Vec<(FileEntry, PathBuf)>>`（per-fingerprint 的 landed_path 列表）；当某 fingerprint 的**全部** entry 都收齐时调 `set_files`：<br>1. **`expected_entry_count` 来源**（planer 审阅补）：在 collector 启动时记一次 `cf.entries.len()`（`handle_clipboard_inbound_files_decide` 收到的 `ClipboardFiles.entries.len()`），存在 collector 的 `pending: HashMap<[u8;32], CollectorEntry>` 的 `CollectorEntry { expected: usize, received: Vec<...> }` 字段里；每收到一个 `InboundFileApplyResult` 比对 `received.len() == expected` 触发 `set_files`<br>2. **`MIME_TOO_LARGE` / `ExceedsLimit` / `Canceled` 在 `InboundFileApplyResult` 的识别**（planer 审阅补）：当前结构体只有 `success: bool` + `error_msg: Option<String>`，需扩展为 `InboundFileApplyResult { fingerprint, sha256, landed_path, error: Option<InboundFileError> }` + `enum InboundFileError { Sha256Mismatch, IoError, PartialResidue, MimeTooLarge, ExceedsLimit, Canceled }`；或**退而求其次**通过 `error_msg` 字符串前缀匹配（`"mime too large"` / `"exceeds limit"` / `"cancelled"`），由 planer 选其一<br>3. **pre-stamp 防回环**：`self.last_outbound_files_fingerprint.insert(fingerprint)` 先于 `set_files`（参考 commit `d6fb1d8` 的 ExceedsLimit arm pre-stamp 修复 — 防止本地 poller watcher 在 `set_files` 后立刻触发 `dispatch_files` 重广播给原对端形成死循环）<br>4. **skip conditions 检查**（任一命中即跳过 `set_files`）：<br>   a. `inject_to_clipboard=false` → 跳过（用户主动关）<br>   b. `last_outbound_files_fingerprint` **在 pre-stamp 之前查询**已命中（用户在本地刚复制过同 selection） → 跳过<br>   c. 任意 entry `error != None`（sha256 mismatch / IO error / `keep_partial=true` 残留 .partial / MIME_TOO_LARGE / ExceedsLimit / Canceled） → 跳过整个 batch（不注入未验证 bytes）<br>5. **set_files**：`backend.set_files(&paths)`，传 collector 累积的 `Vec<PathBuf>`（参考 4.2 trait 签名；注意 `&mut self`，通过 `service.backend_mut()` 之类取得 mut 引用）<br>**配置开关**：4.1 已加 `lan_mouse_ipc::ClipboardConfig.inject_to_clipboard: bool` 字段（`#[serde(default)]` 默认 `true`）；本 STEP 4.3 不再加 IPC 字段，复用 4.1 已落地的字段。`src/config.rs` TOML 段字段也是 4.1 落地的，本 STEP 不再加 TOML 段字段<br>**GUI checkbox 渲染延后**：本 STEP **不**改 `lan-mouse-vue/src/components/GeneralPanel.vue` —— checkbox DOM 渲染是 5.4 范畴；4.3 只完成后端 `set_files` 接线 + skip conditions + 防回环 + 单测覆盖<br>**注 ①**：`MIME_TOO_LARGE` / `ExceedsLimit` / `Canceled` 在当前 dispatch 流程中**不可达** `set_files` 调用点（`handle_clipboard_inbound_files_decide` 已过滤 `AllMimeTooLarge` / 单 entry `ExceedsLimit`，`FileTransferCancel` 早返回不发 `InboundFileApplyResult`）。保留 skip 是 forward-compat；测试矩阵的对应行验证的是 collector 在收到这些 entry 时正确跳过（即便当前 dispatch 路径不触发，仍作为防御性测项保留）<br>**fmt/clippy/build**<br>**测试**：单测覆盖 4 个 skip condition + happy path（含 collector 等待全部 entry 就绪） | `lan-mouse-ipc/src/lib.rs`（4.1 已落地的 `ClipboardConfig.inject_to_clipboard` 字段）、`src/service.rs::handle_inbound_files_applied`（collector 累积 + pre-stamp + 调 `set_files`，含 skip condition 分支；扩展 `InboundFileApplyResult` 加 error_kind）、`src/config.rs`（4.1 已落地的 TOML 段） | mock collector 验证 pre-stamp + 全部 entry 落盘后 `set_files(&[PathBuf])` 被调用一次（每路径正确）；`inject_to_clipboard=false` 时 `set_files` 不被调用；回环指纹命中时 `set_files` 不被调用；任一 entry 落盘失败时 `set_files` 不被调用；forward-compat `MIME_TOO_LARGE` entry collector 跳过 `set_files`；forward-compat `ExceedsLimit` / `Canceled` entry collector 跳过 `set_files`；`expected_entry_count` 来源单测（`cf.entries.len()` 在 collector 启动时记一次）；`cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` 全绿 |

**M4 里程碑交付**：
- IPC `ClipboardConfig` 8 字段扩展（含 `inject_to_clipboard` 字段；drop `auto_accept_files`）—— 4.1 落地
- **`ClipboardBackend::set_files` trait + macOS / Windows / Linux 三平台实现**（STEP-4.2）
- **剪贴板回灌**（STEP-4.3）：文件落盘后自动灌回本地剪贴板（`inject_to_clipboard=true` 默认开启），用户可一键 Cmd+V 粘贴，无需手动 navigate 到 `accept_dir`；pre-stamp 防回环 + 4 类 skip condition 覆盖完整
- TOML `[clipboard]` 段加 `inject_to_clipboard` 字段（4.1 落地）

**M4 已知限制**：
- **剪贴板回灌永久无 UI 提示**（mid-edit 场景下可能覆盖用户当前剪贴板内容；用户决策 2026-09-13：**M4 + M5 都不加** UI 提示，仅行为层面实现 —— 不延展到 GeneralPanel 卡片、不立后续 PLAN 补）
- 拔网处理 / 性能基准 / Vue 类型 / GUI 配置区 / CLI 集成 —— M5 范畴

---

### M5 — 拔网处理 + 端到端性能 + Vue IPC 绑定 + GUI 配置 + CLI

**目标**：M4 完成后，在已落地的 IPC `ClipboardConfig` + 平台 `set_files` + 剪贴板回灌 之上，加拔网处理（HTTP/3 stream error → IPC 推送 `FileTransferFailed`）、200 MiB 性能验收（100 Mbps LAN < 30s / Wi-Fi < 60s 双向 + cancel + 拔网 双向）、Vue 类型 + IPC 绑定、GeneralPanel + per-peer UI、CLI 集成（`SetClipboardConfig` / `SetEnableClipboardTo`）。

**AI 估时**：~7.0 h
**依赖**：M4（待启动）

**用户决策（2026-09-13）**：
1. **drop UI 端到端验收**：用户决策 2026-09-13 auto-accept only，不需要 GUI 交互验证
2. M5 5 STEPs（5.1 - 5.5）：5.1 拔网 + 5.2 端到端性能 + 5.3 Vue IPC 绑定 + 5.4 GeneralPanel + per-peer UI + 5.5 CLI 集成
3. **keepalive↔idle race 专项 + Pong 间隔 ≤ 600 ms**（承接 M0c Ping/Pong keepalive）
4. M4 + M5 step 映射关系（保留 audit trail）：`3b.1 → 4.1` / `3b.2 → 5.1` / `3b.3 → 5.2` / `3b.4 → 5.3` / `3b.5 → 5.4` / `3b.6 → 5.5` / `3b.7a → 4.2` / `3b.7b → 4.3`

**人类准备**：
- **中途拔网测试**：传输 200 MiB 过程中拔网线 / 关闭对端 Wi-Fi，观察 GUI 报错信息
- **大文件准备**：同 M3a（200 MiB 随机文件）
- **GUI 真机**：浏览器打开 GUI，`pnpm dev` 起 Vite；WebSocket console 可见

| STEP | 估时 | 任务 | 涉及文件 | 完成标志 |
|---|---|---|---|---|
| **5.1** | **1.5h** | **拔网处理：HTTP/3 客户端 stream error → IPC 推送 `FileTransferFailed`**（承接 4.1 `keep_partial` 字段 + 4.3 collector 路径）：<br>**接收端错误路径**：`apply_inbound_files_task` 内 HTTP/3 stream error（`RecvStream` 关闭、`ReadError::ConnectionLost` / `Reset` / `TimedOut`）→ 触发 `service::file_inbound_err(sha256, reason)` handler → IPC 推 `FrontendEvent::FileTransferFailed { sha256: [u8; 32], reason: String, ts_ms: u64 }`；**新增** `FrontendEvent::FileTransferFailed` 在 `lan-mouse-ipc` —— 不引入 accept/reject IPC，只补一个**失败通知**事件<br>**`.partial` 处理**：默认删除（关闭 SUGGESTION P2.3 carry-forward：本 STEP 5.1 承接，fsync between write and remove）；`keep_partial: bool` config 控制 —— `keep_partial = true` 时保留 `.partial` 文件供排查，`false` 时 `std::fs::remove_file` + log info<br>**reason 字段内容**：`"connection lost"` / `"timeout"` / `"peer cancelled"` 三类枚举 → `String`（保留扩展空间）；GUI 端展示 raw reason 即可<br>**ts_ms 来源**：使用 `std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)`（fallback 0 不会 panic）；亦可复用 `service::now_ms()` 已有 helper（如存在）<br>**fmt/clippy/build**<br>**测试**：单测覆盖三种 error 触发 → IPC event 推送 + .partial 清理 + keep_partial 路径；拔网时序端到端 < 5 s（按 M3a cancel 的 1 s 预算） | `src/service.rs`、`lan-mouse-ipc/src/lib.rs` | 单测：mock HTTP/3 stream error → IPC 收到 `FileTransferFailed`；`keep_partial = false` 时 .partial 已删；`keep_partial = true` 时 .partial 保留；`ts_ms` 单测（非 0 / 单调递增）；`cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` 全绿 |
| **5.2** | **1.5h** | **端到端性能 + 收尾（评审 #5 3rd 双档）**：200 MiB 文件传输性能分两档——**有线 100 Mbps LAN < 30 s**（理论 16 s + QUIC 加密/流控/sha256 余量）+ **Wi-Fi 实际带宽（有线 30-50 %） < 60 s**；**drop UI 端到端验收**（用户决策 2026-09-13：auto-accept only，不需要 GUI 交互验证），保留 **cancel 双向 + 拔网双向 + keepalive↔idle race 专项 + Pong 间隔 ≤ 600 ms**<br>**keepalive↔idle race 专项**：模拟 QUIC idle timer 在 200 MiB 传输完成后 5 s 内的 keepalive 行为（评审 #5 风险：传输完成后 5 s 内 idle 是否会断链）；实测：传输完成后 30 s 内连接仍 active；传输完成后 60 s 静默期不应触发 disconnect<br>**Pong 间隔 ≤ 600 ms**：M0c 引入的 Ping/Pong keepalive 间隔不超过 600 ms（保障 idle timer 在 5 s 触发前已收到 Pong）<br>**fmt/clippy/build + 三平台真机双向端到端（人类配合）** | `tests/manual/file-transfer.md` | 有线 < 30 s、Wi-Fi < 60 s；cancel 双向 + 拔网双向 均符合预期；keepalive↔idle race 实测 30 s 内不关链；Pong 实测间隔 ≤ 600 ms；fmt/clippy/build 全绿 |
| **5.3** | **1.5h** | **Vue 类型 + IPC 绑定**（drop `FileTransferRequest` 类型，保留 `FileTransferFailed`）：<br>`lan-mouse-vue/src/api/ipc.ts` 加：`ClipboardConfig` 类型（与 IPC `ClipboardConfig` 1:1 对应：enabled / accept_dir / ignore_text / ignore_images / ignore_files / max_file_size / keep_partial / **inject_to_clipboard**）+ `ClipboardState` 类型（沿用 M0c：last_text_ts / last_image_ts / last_file_ts / last_source）+ `FileTransferFailed` 类型（sha256 hex string / reason / ts_ms）<br>`lan-mouse-vue/src/store/index.ts` 维护：`state.clipboardConfig: ClipboardConfig`（从 IPC 拉初始值 + 监听 `ClipboardConfigChanged` 事件回写，含 4.1 落地的新字段；**新增** `state.lastClipboardText: string` / `state.lastClipboardAt: number` / `state.lastClipboardSource: string` 三字段，本 STEP 5.3 新增、**不是**沿用 M0c + M1b —— store 当前没有这三个字段，需在本 STEP 落地）<br>`onMounted` 监听 `ClipboardState` 事件 + `FileTransferFailed` 事件 → store 更新；新增 toast 触发（**注意**：toast 是单方向通知，不需要 actions 按钮）<br>**移除**：`FileTransferRequest` / `RespondFileTransfer` 相关类型 + store 字段（用户决策 2026-09-13）<br>**fmt/clippy/build**<br>**注**：`ClipboardConfigChanged` IPC 事件由本 STEP 5.3 **新增** 到 `lan-mouse-ipc` 的 `FrontendEvent` 枚举（不在 4.1 范围 —— 4.1 只定义配置 schema 和 `SetClipboardConfig` 请求，不动 `FrontendEvent`）；GUI 通过此事件感知后端配置变更（daemon 重启 / CLI 改动 / 对端 push 都会触发） | `lan-mouse-vue/src/api/ipc.ts`、`lan-mouse-vue/src/store/index.ts`（新增 lastClipboardText / lastClipboardAt / lastClipboardSource 字段）、`lan-mouse-vue/src/components/Toaster.vue`（仅增 FileTransferFailed 单方向通知，不扩 actions）、`lan-mouse-ipc/src/lib.rs`（新增 `FrontendEvent::ClipboardConfigChanged`） | 浏览器 console 看到状态同步；FileTransferFailed 触发 Toaster 单方向通知（无 actions）；`ClipboardConfigChanged` 事件触发 store 回写 + 现有 quicIdleTimeoutSecs 同模式事件单元测试；`cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` 全绿；`pnpm build` 0 error |
| **5.4** | **1.5h** | **GeneralPanel + per-peer 配置 + TOML 落盘**（评审 #4 改写，承接 4.1 `ClipboardConfig` + 4.3 后端接线）：<br>**GeneralPanel**：加剪贴板区块 — `enabled` checkbox（master toggle）/ `accept_dir` 文本框 + dir-picker（**必填**，无 Option 概念）/ `ignore_text` / `ignore_images` / `ignore_files` 三个 ignore checkbox / `max_file_size` number input（MiB 整数输入 → 后端转 bytes，0 = 不限）/ `keep_partial` checkbox / **`inject_to_clipboard` checkbox**（**DOM 渲染在本 STEP 5.4 落地**；4.3 只完成后端 `set_files` 接线 + skip conditions 读 4.1 落地 IPC 字段，**不**碰 Vue 文件也**不**再加 IPC 字段）；`onChange` 调 `SetClipboardConfig`（无 handle）<br>**ConnectionRow**：每个 client 行加 `enable_clipboard_to` checkbox（label "Push clipboard to this peer"）；`onChange` 调 `SetEnableClipboardTo(handle, bool)`<br>**TOML 落盘**：`src/config.rs` TOML 加 `[clipboard]` 段（**daemon-global**）：`enabled = true` / `accept_dir = "/Users/me/Downloads/lan-mouse"` / `ignore_text = false` / `ignore_images = false` / `ignore_files = false` / `max_file_size = 52428800`（bytes 整数存，UI 显示 MiB）/ `keep_partial = false` / **`inject_to_clipboard = true`**；不挂在 `[[clients]]` 下<br>**per-client TOML 段**：`[[clients]]` 加 `enable_clipboard_to = true` 字段<br>**fmt/clippy/build** | `lan-mouse-vue/src/components/GeneralPanel.vue`（剪贴板区块 + 7 个控件）、`lan-mouse-vue/src/components/ConnectionsPanel.vue`（per-peer `enable_clipboard_to` checkbox）、`src/config.rs`、`src/service.rs`（`set_clipboard_config` handler） | 改 checkbox 立即生效（关闭 SUGGESTION #S-7 + #S-8 + #S-5）；config.toml 落盘正确（顶层 `[clipboard]` + 每个 `[[clients]]` 内 `enable_clipboard_to`）；MiB → bytes 转换单测；`inject_to_clipboard` checkbox 关闭后文件不灌回剪贴板；GeneralPanel vitest snapshot 稳定；`cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` 全绿；`pnpm build` 0 error |
| **5.5** | **1.0h** | **CLI 集成（评审 #7）**：`lan-mouse-cli` 加 `SetClipboardConfig` 子命令（发 IPC → daemon 写 TOML → 回 echo）；参数 `--enabled` / `--accept-dir` / `--ignore-text` / `--ignore-images` / `--ignore-files` / `--max-file-size`（MiB 整数，CLI 转 bytes）/ `--keep-partial` / **`--inject-to-clipboard`**；`SetEnableClipboardTo <handle> <bool>` 子命令；与现有 `SetMonitor` 共用同一 dispatch pattern；单测覆盖 IPC 编码（含 drop `auto_accept_files` 后兼容性）<br>**fmt/clippy/build** | `lan-mouse-cli/src/lib.rs` | `lan-mouse-cli SetClipboardConfig --max-file-size 100 --accept-dir /tmp/recv` 生效；`lan-mouse-cli SetClipboardConfig --ignore-files=true` 关掉文件同步后真机复制文件不入剪贴板；`lan-mouse-cli SetClipboardConfig --keep-partial=true` 保留 .partial；`lan-mouse-cli SetEnableClipboardTo 0 false` 关掉对端 0 的剪贴板推送 + `SetEnableClipboardTo 0 true` 恢复；`lan-mouse-cli SetClipboardConfig --inject-to-clipboard=false` 关闭回灌；`cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` 全绿 |

**M5 里程碑交付**：
- 拔网清晰报错（`FileTransferFailed` IPC + .partial 默认删除，**双向**）
- 200 MiB 文件端到端（100 Mbps LAN < 30 s / Wi-Fi < 60 s，**双向**）
- 源端取消响应（cancel < 1 s，沿用 M3a STEP-3a.5）
- keepalive↔idle race 实测（30 s 内不关链 + Pong 间隔 ≤ 600 ms）
- GUI 剪贴板配置区（enabled / accept_dir / ignore_text / ignore_images / ignore_files / max_file_size / keep_partial / inject_to_clipboard）
- per-peer `enable_clipboard_to` 细粒度开关
- CLI 子命令支持（`SetClipboardConfig` / `SetEnableClipboardTo`）

**M5 已知限制**：
- **无 accept/reject UI**（auto-accept only；用户决策 2026-09-13）—— 用户无法在 GUI 拒绝单次文件接收；如需拒绝，关闭 `enabled` 整体开关 或 `ignore_files: bool`
- **无 clipboard state card / amber 高亮 / 回环统计显示**（用户决策 2026-09-13 auto-accept only + 不延展可观察卡片；M1b STEP-1b.3 的 `service::clipboard::metrics` 仍 log，但 UI 不展示）
- **无 README.md / DOC.md 文档章节**（用户决策 2026-09-13 不立后续 PLAN 补）
- 断点续传仅 stub（沿用 M3a；M3a `?range=` HTTP/3 接口已留）

---

## 4. 总估时汇总

| 里程碑 | AI 估时 |
|---|---|
| M4 4.1 | 1.5h |
| M4 4.2 | 1.5h |
| M4 4.3 | 1.5h |
| **M4 合计** | **~4.5 h** |
| M5 5.1 | 1.5h |
| M5 5.2 | 1.5h |
| M5 5.3 | 1.5h |
| M5 5.4 | 1.5h |
| M5 5.5 | 1.0h |
| **M5 合计** | **~7.0 h** |
| **合计** | **~11.5 h** |

> **M4 / M5 拆分原则**：原 M3b 8 STEPs（~11.5 h）拆分为 M4 3 STEPs（~4.5 h）+ M5 5 STEPs（~7.0 h）。M4 是"配置 + 平台底层 + 应用集成"完整闭环（IPC schema + 平台 set_files + 回灌）；M5 是"网络/性能/UX/CLI"运维层（拔网 + 性能 + Vue + GUI + CLI）。
> 若 STEP-5.2（端到端性能 + 收尾）实测超 1.5 h AI，按"性能双档 vs. keepalive↔idle race"二分拆为 5.2a / 5.2b。
> 若 STEP-4.2 单平台实现超 1.5 h AI（macOS / Windows / Linux 中某一特别复杂），LEADER 介入按平台拆 4.2-i / 4.2-ii / 4.2-iii。

---

## 5. 风险

> 本节合并 `PLAN-2-CLIPBOARD.md` §5 #1-#25（已采纳，落地于 M0a-M3a）+ #26（剪贴板回灌特有）+ #27-#28（set_files trait 平台实现特有）。

### 引用 PLAN-2-CLIPBOARD.md §5 #1-#25

风险 #1-#25（ProtoEvent codec 双轨化、h3 spike、StreamC 拆 a/b、Windows DIB、idle_timeout、changeCount 精度、Wayland fallback、回环 LRU、200 MiB cancel race、多端并发写、剪贴板监听权限、proto 版本号、ALPN 共存、cache 失效 race、ClipboardConfig 位置、Toaster accept/reject drop、UI 提示 drop、CLI 集成、HTTP/3-lite spike 范围、TIFF 归一化、NSImage DIB 解码、LRU metrics 信号、200 MiB 性能双档、Linux fallback、50 MiB max_file_size）已在 M0a-M3a 阶段处理完毕；详见 `next/PLAN-2-CLIPBOARD.md` §5。

### M4 + M5 新引入风险

26. **【用户决策 2026-09-13】接收端剪贴板回灌可能覆盖用户当前剪贴板内容**：STEP-4.3 在文件落盘成功后自动调 `backend.set_files(&[PathBuf])` 灌回本地剪贴板；若用户正在 mid-edit（剪贴板里是临时复制的内容如一段文本 / 一张截图），回灌会**无声覆盖**这些内容。**已审视、采纳（最终）**：**M4 + M5 均不加** UI 提示（无 toast / 无可观察卡片 / 无 GeneralPanel 提示），仅行为层面实现；用户在 GeneralPanel 可关 `inject_to_clipboard` checkbox 整体关闭回灌。mid-edit 覆盖行为由用户决策接受，不立后续 PLAN 补 GeneralPanel 卡片。
27. **`set_files` trait 三平台实现兼容 macOS `ProtocolObject`**（4.2）：4.2 涉及 objc2-app-kit 0.3.2 `ProtocolObject::from_retained(nsurl)` 类型约束，若 0.3.x → 0.4+ minor 升级破坏 `extern_conformance! unsafe impl NSPasteboardWriting for NSURL` 路径，macOS 端 set_files 会编译失败。**Mitigation**：在 Cargo.toml 把 objc2-app-kit 锁到 0.3.x；或抽 trait 隔离平台实现。
28. **Linux URI list CRLF 跨平台一致性**（4.2）：4.2 Linux 实现走 `wl-copy --type text/uri-list`，但部分 Wayland compositor（GNOME Mutter < 46）只识别 LF 不识别 CRLF，可能导致回灌失败。**Mitigation**：测试矩阵覆盖 GNOME 45/46/47 + KDE Plasma 5/6；失败则退化为"仅 X11 路径支持回灌 + Wayland 仅落盘不入剪贴板"。

---

## 6. Out of Scope

- 剪贴板格式协商（HTML / RTF / 自定义二进制）—— 后续 PLAN
- 文件断点续传 —— HTTP/3 `?range=` 接口已留；后续 PLAN
- Wayland portal 兼容 —— 需 `wlr-data-control` 协议；后续 PLAN
- 任何前端框架切换 —— 沿用现有 Vue GUI
- M4 / M5 之外的可观察卡片 / amber 高亮 / 回环统计显示 —— auto-accept only 不延展（用户决策 2026-09-13）
- **剪贴板回灌 UI 提示**（toast / GeneralPanel 卡片）—— M4 + M5 均不加（用户决策 2026-09-13：行为层面实现即可，不立后续 PLAN 补）
- M4 / M5 之外的 README.md / DOC.md 文档章节 —— 后续 PLAN

---

## 7. 执行约定

- 沿用 `PLAN-2-CLIPBOARD.md` §7 执行约定：派 `plan-step-executor` 执行 STEP-4.1 → 触发 validator（如累计 > 1h 或 M4 完成）；STEP 偏差超过 45 min AI 时 LEADER 介入重拆；跨 STEP 影响的小问题记入 `next/SUGGESTION.md`；每完成一个 milestone LEADER 提交 git（commit message 格式英文，不带 M / STEP 编号）+ 更新 `next/.LEADER-STATE.md`
- 原 M3b 8 STEPs 拆分为 **M4 3 STEPs + M5 5 STEPs**。3b.7a / 3b.7b（即现 4.2 / 4.3）是 planer round 2 审阅拆步（`set_files` trait 未在 M3a 落地 → 先补三平台实现再集成 skip conditions + IPC + GUI + TOML）
- **M4 + M5 串行执行**：M4 完成后用户介入验证剪贴板回灌（核心 UX 验收点）→ 启动 M5；M5 完成后用户再次介入验证性能 + GUI 配置 → 用户对齐下一里程碑 → leader 续约或交班

---

## 8. 测试矩阵

> 协议 / 序列化 / 类型 / 编译 / 单元测试 → 自动；平台剪贴板 API 行为、HTTP/3 端到端连通、大文件性能、回环检测的"实际场景"、GUI 交互 → 必须人类在真机手动跑。
> 自动列里每一条都必须合并前由 AI 跑通贴日志；人类列里每一条至少一次由人类在真机记录结果。

### M4 — IPC `ClipboardConfig` + 平台 `set_files` + 剪贴板回灌

| 类型 | 测试项 | 通过标志 | 对应 STEP |
|---|---|---|---|
| 自动 | `ClipboardConfig { enabled, accept_dir: PathBuf, ignore_text, ignore_images, ignore_files, max_file_size, keep_partial, inject_to_clipboard }` serde round-trip（**drop** `auto_accept_files`；**含** `inject_to_clipboard` 缺字段 = `true`） | 单测绿 | 4.1 / 4.3 |
| 自动 | `max_file_size = 0` → 不限；`accept_dir` 必填（非 Option）；`enabled = false` → dispatcher 不启 | 单测绿 | 4.1 |
| 自动 | `Config::max_file_size()` getter 单测：默认 50 MiB；TOML 读 100 MiB 后切到 100 MiB；关闭 SUGGESTION #S-5/#S-8 | 单测绿 | 4.1 |
| 自动 | `ClipboardConfig` 缺 `inject_to_clipboard` 字段 = 默认 `true` | 单测绿 | 4.3 |
| 自动 | `ClipboardBackend::set_files` trait method 编译 + dummy 实现通过 | 单测绿 | 4.2 |
| 自动 | macOS `set_files` 单测：mock `NSPasteboard.writeObjects` → 被调用一次 + 参数含预期 paths | 单测绿 | 4.2 |
| 自动 | Windows `set_files` 单测：mock `SetClipboardData(CF_HDROP, hdrop)` → 参数捕获 + DROPFILES 结构验证 | 单测绿 | 4.2 |
| 自动 | Linux `set_files` 单测：mock `xclip -selection clipboard -t text/uri-list -i` 或 `wl-copy` 子进程 → args 断言 | 单测绿 | 4.2 |
| 自动 | mock backend 接收 ClipboardFiles 后 pre-stamp + `set_files` 被调用一次 | 单测绿 | 4.3 |
| 自动 | `inject_to_clipboard=false` 时 `set_files` 不调用 | 单测绿 | 4.3 |
| 自动 | 回环指纹命中时 `set_files` 不调用（pre-stamp 前查询） | 单测绿 | 4.3 |
| 自动 | 落盘失败（sha256 mismatch / IO error / .partial 残留）时 `set_files` 不调用 | 单测绿 | 4.3 |
| 自动 | `MIME_TOO_LARGE` entry 跳过 `set_files` | 单测绿 | 4.3 |
| 自动 | `ExceedsLimit` entry 跳过 `set_files` | 单测绿 | 4.3 |
| 自动 | `Canceled` entry 跳过 `set_files` | 单测绿 | 4.3 |
| 自动 | 三平台编译通过 | CI matrix 全绿 | 4.2 |
| **人类** | macOS 真机剪贴板回灌（双向）：(a) **A→B**：A 端 Finder 复制文件 → B 端落盘后**自动**入剪贴板 → B 端 Cmd+V 直接粘贴出该文件；(b) **B→A**：反过来同样跑一次 | 录屏 / 截图（两个方向各一段） | 4.2 / 4.3 |
| **人类** | Windows 真机剪贴板回灌（双向）：(a) **A→B** + (b) **B→A** 同 macOS 验证步骤 | 同上 | 4.2 / 4.3 |
| **人类** | Linux 真机剪贴板回灌（双向）：(a) **A→B** + (b) **B→A** 同 macOS 验证步骤（X11 / Wayland 按系统走） | 同上 | 4.2 / 4.3 |
| **人类** | 关掉回灌：GeneralPanel `inject_to_clipboard = false` 后复制文件 → 落盘但**不**入剪贴板；恢复后正常回灌（**集成测项**：4.3 后端接线 + 5.4 GeneralPanel DOM 渲染双前置） | 录屏（两个状态切换各一段） | 4.3 / 5.4 |

### M5 — 拔网处理 + 端到端性能 + Vue IPC 绑定 + GUI 配置 + CLI

| 类型 | 测试项 | 通过标志 | 对应 STEP |
|---|---|---|---|
| 自动 | `FrontendEvent::FileTransferFailed { sha256, reason, ts_ms }` serde round-trip（**新增**，无 accept/reject 配对） | 单测绿 | 5.1 |
| 自动 | 拔网处理单测：mock HTTP/3 stream error → IPC 推 `FileTransferFailed` + 默认删 .partial + `keep_partial = true` 时保留 | 单测绿 | 5.1 |
| 自动 | keepalive↔idle race 单测：传输完成后 30 s 内连接仍 active；60 s 静默期不应 disconnect | 单测绿 | 5.2 |
| 自动 | Pong 间隔 ≤ 600 ms 计时单测（M0c Ping/Pong keepalive 复用） | 单测绿 | 5.2 |
| 自动 | Vue `api/ipc.ts` 类型：ClipboardConfig（含 `inject_to_clipboard`）/ ClipboardState / FileTransferFailed（**无** FileTransferRequest）/ **`ClipboardConfigChanged` 事件类型**（与 `lan-mouse-ipc` `FrontendEvent::ClipboardConfigChanged` 1:1） | 单测绿 | 5.3 |
| 自动 | Vue `store/index.ts` 单测：mock `ClipboardState` → state 更新；mock `FileTransferFailed` → toast 单方向通知（无 actions）；mock **`ClipboardConfigChanged` → `state.clipboardConfig` 回写**（含 4.1 新字段） | 单测绿 | 5.3 |
| 自动 | GeneralPanel vitest snapshot：clipboard 区块（enabled / accept_dir / ignore_text / ignore_images / ignore_files / max_file_size MiB 输入 / keep_partial / inject_to_clipboard）；ConnectionRow `enable_clipboard_to` checkbox | snapshot 稳定 | 5.4 |
| 自动 | `src/config.rs` TOML `[clipboard]` 段 round-trip 单测（含 `accept_dir` 必填 + bytes 整数存 max_file_size + `inject_to_clipboard` 字段） | 单测绿 | 5.4 |
| 自动 | MiB → bytes 转换单测（UI 输入 100 MiB → TOML 存 104857600） | 单测绿 | 5.4 |
| 自动 | `lan-mouse-cli SetClipboardConfig` IPC 编码单测（含 `--inject-to-clipboard` 参数 + drop `auto_accept_files` 兼容性） | 单测绿 | 5.5 |
| 自动 | `lan-mouse-cli SetEnableClipboardTo` IPC 编码单测 | 单测绿 | 5.5 |
| 自动 | `cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` | 无 diff / 无 warning | 5.2 |
| 自动 | `cd lan-mouse-vue && pnpm build` 产物 OK | 0 error | 5.3 |
| **人类** | macOS 真机 GUI 配置：(a) A 端 ConnectionsPanel 改 `enable_clipboard_to` → 立即生效（config.toml 落盘）；(b) B 端同样改 → 两侧独立 | 截图 + config.toml diff（两端各一份） | 5.4 |
| **人类** | Windows 真机 GUI 配置：(a) + (b) 同上 | 同上 | 5.4 |
| **人类** | Linux 真机 GUI 配置：(a) + (b) 同上 | 同上 | 5.4 |
| **人类** | GeneralPanel `max_file_size` 调到 100 MiB 后复制 60 MiB 文件（双向 A→B + B→A 各跑一次）：**不再**触发 ExceedsLimit popup，正常落盘 + sha256sum 一致 | 日志 + sha256sum | 4.1 / 5.4 |
| **人类** | GeneralPanel `enabled = false` 后复制文本：剪贴板**不**同步到对端；恢复后正常 | 日志 + pbpaste | 4.1 / 5.4 |
| **人类** | 200 MiB 性能：100 Mbps **有线** LAN 实测 < 30 s — (a) A→B + (b) B→A 各计时一次 | 秒表（两段） | 5.2 |
| **人类** | 200 MiB 性能：Wi-Fi 实测 < 60 s（**评审 #5 3rd 双档**） — (a) A→B + (b) B→A 各计时一次 | 秒表（两段） | 5.2 |
| **人类** | 200 MiB 性能：cancel 双向 — (a) A→B 源端覆盖剪贴板 → 接收端 1 s 内停止 + 清 .partial + (b) B→A 同上 | 秒表 + 文件系统 | 5.2 |
| **人类** | 200 MiB 性能：拔网双向 — (a) A→B 方向：A 发起传输，B 接收中拔网 → 5 s 内 B 端 GUI 看到 "connection lost"；`(b) B→A` 同上 | 秒表 + 错误信息 | 5.2 |
| **人类** | keepalive↔idle race：200 MiB 完成后 30 s 内连接仍 active（用 `lsof -i UDP:4252` / netstat 观察）；60 s 静默期不 disconnect | 终端 + 日志 | 5.2 |
| **人类** | `lan-mouse-cli SetClipboardConfig --max-file-size 100 --accept-dir /tmp/recv --enabled` 生效（config.toml 落盘 + daemon reload） | config.toml diff | 5.5 |
| **人类** | `lan-mouse-cli SetClipboardConfig --inject-to-clipboard=false` 生效（config.toml 落盘 + 文件不入剪贴板） | config.toml diff + 录屏 | 5.5 |
| **人类** | `lan-mouse-cli SetEnableClipboardTo 0 false` 关掉对端 0 的剪贴板推送 | 日志 | 5.5 |

### 不可自动化 / 必须人为判断的项

1. **平台剪贴板 API 行为差异**: macOS NSPasteboard / Windows CF_HDROP / Linux URI list
2. **200 MiB 真实网络性能**
3. **剪贴板回灌的副作用**: 用户 mid-edit 时收到文件会被覆盖（行为接受）
4. **macOS TCC 权限**
5. **回环检测在极端时序下是否漏检**

### 测试工具与脚本建议

- **真机回归模板**：`tests/manual/file-transfer.md`（"在 macOS 14 + Windows 11 对端下：1. 启动 daemon；2. Finder 复制 200 MiB 文件；3. 对端落盘 + 自动入剪贴板；4. 对端 Cmd+V 粘贴出该文件"），人类按模板逐项打勾
- **协议 round-trip**：`cargo test -p lan-mouse-proto` 包含所有 ProtoEvent 变体
- **HTTP/3 端到端**：`tests/http3_smoke.rs` 跑通 `/healthz` + `/clipboard/{text,image,file}/...`
- **WebSocket 事件录制**：开发期 `RUST_LOG=lan_mouse_service=trace,lan_mouse_quic_transport=trace`，console 输出存 `tests/manual/<date>-<machine>.log`
- **大文件性能**：`scripts/bench-file-transfer.sh` —— 跑 `dd` + `sha256sum` + 计时 + 错误检测，一键回归

---

> **本文档承接**：`PLAN-2-CLIPBOARD.md` §0 范围 / §1 架构概览 / §5 风险 #1-#25（已采纳，落地于 M0a-M3a）
> **本文档创建**：2026-09-13 — 用户决策拆分 PLAN（原 M3b 8 STEPs 单独承载 + 引用 M3a wire-level 依赖）
> **2026-09-13 重组**：M3b 拆分为 **M4 + M5**（用户决策把 set_files trait 实现提前至 M4；M5 承接网络/性能/UX/CLI 运维层；具体 step 映射 `3b.1→4.1` / `3b.7a→4.2` / `3b.7b→4.3` / `3b.2→5.1` / `3b.3→5.2` / `3b.4→5.3` / `3b.5→5.4` / `3b.6→5.5`）
> **下一步**：用户真机验证 M3a（`PLAN-2-CLIPBOARD.md` §7 gate）→ 启动 M4 STEP-4.1