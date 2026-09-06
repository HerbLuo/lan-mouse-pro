# PLAN-M2: QUIC 传输就绪 + 跨设备剪贴板 / 文件同步

> 目标：
> 1. 把现有 `webrtc-dtls + UDP` 传输层替换为 QUIC（已基本完成于当前 codebase，需要补 HTTP/3 字节拉取通道）。
> 2. 在 QUIC 之上**新增**剪贴板文本 / 图片 / 文件三类跨设备同步。
>
> 终点：用户在一台机器复制一段文本 / 一张截图 / 一个文件，在另一台已配对设备上能粘贴或落盘；传输层走 QUIC，200 MiB 文件 SHA-256 校验通过。
>
> 不在本计划内：
> - 剪贴板格式协商（HTML / RTF 等）—— 首版仅 text + image + file。
> - 文件断点续传（首版一次性成功；拔网后清晰报错而非"卡死"）。
> - CRDT / 最终一致性 —— 仍按"最后一个写入者赢"，靠 fingerprint 比对避免回环。
> - **多屏友好的被控端定位**（已在 `PLAN-1-POSITION-MULTI-MONITOR.md` 中）。
> - **Vue 前端**（本 fork 已从上游 GTK 迁到 Vue / Vite；本计划不涉及框架切换）。

## 0. 范围

### In Scope

**协议 / 传输层**
- **bump `lan-mouse-proto` 到 0.4.0**（评审 #1）：新增 7 个 `ProtoEvent` 变体（`ClipboardText` / `ClipboardImage` / `ClipboardFiles` / `ClipboardRequest` / `FileTransferOffer` / `FileTransferResponse` / `FileTransferCancel`），是主版本变更
- **wire-level 兼容策略（评审 #1）**：剪贴板 / 文件相关事件**全部**走 StreamC；旧 daemon（无 StreamC reader）的 `streams::read_loop`（参考 `streams.rs:291` 的"M2-only"预留位）已**默默 drop** `stream_bunch.c`，不会触发 stream A decode error → 新 / 旧 daemon 在 stream A 上仍能正常键鼠互通，仅剪贴板 / 文件功能单向有效（旧的收不到、新的不会推错地方）。**不**走 stream A 编码新 EventType → 不会让旧 daemon 触发 `EventType::try_from(InvalidEventId)` 断链
- 现有 `quic_transport` 的 StreamC 通道从"占位"升级为可用
- 在 `quinn::Connection` 上叠加 HTTP/3 端到端服务（h3 crate 或自实现 HTTP/3-lite，见 M0b STEP 0.2 spike），用于**大文本 / 图片字节 / 文件字节**的拉取
- 输入通道模式 `InputChannelConfig` 已在 `PLAN-1` 中引入；本计划**不**改动

**业务功能**
- 剪贴板文本同步（含大小文本；fingerprint 防回环）
- 剪贴板图片同步（PNG / JPG / BMP，SHA-256 校验）
- 复制文件同步（HTTP/3 拉取 + SHA-256 + 询问 / 自动接收 + 源端取消）

**前端**
- **ClipboardConfig 放 GeneralPanel**（daemon-global，评审 #4）：剪贴板监听是 daemon 全局（一个 OS 剪贴板喂所有 peer），接收目录也是全局 → `{ auto_accept_files, accept_dir, ignore_text, ignore_images, ignore_files }` 放 GeneralPanel
- **per-peer `enable_clipboard_to: bool`**（评审 #4）：每个 ConnectionRow 加细粒度开关，控制"是否把剪贴板推给这个 peer"，与 `input_channels` per-client 配置不冲突（那是发送端路由选择）
- ConnectionsPanel / GeneralPanel：剪贴板 / 文件接收开关、最近同步状态
- Toaster：文件接收请求通知（接受 / 拒绝）
- 接收目录配置

### Out of Scope（后续 PLAN）

- 剪贴板格式协商（HTML / RTF / 自定义二进制）
- 文件断点续传（HTTP/3 `?range=` 已留接口，本计划不实现）
- Wayland portal 兼容（libei 后端在 `PLAN-1` 已知有限制，clipboard 走 X11 / wl-clipboard 工具）
- 任何前端框架切换（沿用现有 Vue GUI，不再维护 GTK 上游路径）

## 1. 架构概览

```
┌────────────────── lan-mouse daemon ──────────────────────┐
│                                                           │
│  ┌────────── 平台剪贴板监听 ──────────┐                   │
│  │  ClipboardBackend (trait)         │  ← macOS: NSPasteboard
│  │  ├─ OsxPasteboard                 │  ← Windows: OpenClipboard
│  │  ├─ WinClipboard                  │  ← Linux: xclip / wl-paste
│  │  └─ X11OrWlpPasteClipboard        │                   │
│  └────────────────────────────────────┘                   │
│         │ 变化                                           │
│         ▼                                                │
│  ┌──── service::clipboard_dispatcher ────┐                │
│  │  1. 计算 fingerprint (sha256 / first_n)│                │
│  │  2. 反向查表：fingerprint 是本地刚写? │                │
│  │     → 是 → skip（防回环）             │                │
│  │  3. 按大小分派：                     │                │
│  │     ≤ 1 KiB → StreamC 内联           │                │
│  │     > 1 KiB → 元数据 + HTTP/3 拉取   │                │
│  └───────────────────────────────────────┘                │
│         │                                                │
│         ▼                                                │
│  ┌──────── quic_transport::PeerSession ──────────┐        │
│  │ StreamA (control) ← 现有                    │        │
│  │ StreamB (input)   ← 现有                    │        │
│  │ StreamC (meta)    ← 新接通                  │        │
│  │   ├─ ClipboardText  /  Image  /  Files 元数据 │        │
│  │   └─ FileTransferOffer / Cancel / Request   │        │
│  │ HTTP/3 (h3)       ← 新叠加                  │        │
│  │   ├─ GET  /clipboard/text/{sha256}         │        │
│  │   ├─ GET  /clipboard/image/{sha256}        │        │
│  │   ├─ GET  /clipboard/file/{sha256}         │        │
│  │   └─ GET  /clipboard/file/{sha256}?range=… │        │
│  └──────────────────────────────────────────────┘        │
└──────────────────────────────────────────────────────────┘
         │
         ▼ QUIC (UDP/4252)
┌────────────────── 对端 lan-mouse daemon ──────────────────┐
│  镜像同样三层；按 fingerprint / SHA-256 决定落剪贴板 / 落盘 │
└───────────────────────────────────────────────────────────┘
```

**关键设计原则**（沿用 `TECHNOLOGY.md` §3）：

| 内容 | 通道 | 理由 |
|---|---|---|
| 剪贴板元数据（≤ 1 KiB 文本内联 / 大文本 sha256 / 图片 sha256 / 文件元数据） | StreamC（bidi） | 与键鼠解耦，避免阻塞 |
| 大文本 / 图片 / 文件字节 | HTTP/3 GET 拉取 | 标准请求/响应 + 可断点续传接口 |
| 文件接收询问 | StreamC `FileTransferOffer` → 接收端 IPC → 用户 → StreamC `FileTransferResponse` | 必须等用户决策 |
| 文件字节传输 | HTTP/3 GET | 200 MiB 不应阻塞其他流 |

**回环防御**（关键）：本端在写剪贴板时**记录** fingerprint（最近 N 条），收到对端推送时若 fingerprint 在"最近写过"集合中 → skip。

## 2. 里程碑路线图

> **时间校准**：沿用 `PLAN-1` 的 1/3 系数（30 min AI ≈ 1.5 h 人类）。
> **拆分原则**：任何里程碑 AI 估时 > 12 h 必拆。

| 里程碑 | AI 估时 | 人类可测功能 | 状态 |
|---|---|---|---|
| **M0a** — ProtoEvent codec 双轨化 | ~3 h | `cargo test -p lan-mouse-proto` 全绿；Input/Ping/Pong/Hello 旧路径零行为差异 | 待启动 |
| **M0b** — HTTP/3 基础 | ~4.5 h | 真机 `curl --http3 https://<peer>/healthz` 200 | M0a |
| **M0c** — StreamC 接通 + IPC 扩展 | ~4 h | StreamC reader 接通；GUI 看到 `ClipboardConfig` 字段；fmt/clippy/build 全绿 | M0b |
| **M1a** — 剪贴板文本（小文本 ≤ 1 KiB） | ~6 h | 复制一段短文本 → 对端剪贴板出现 | M0c |
| **M1b** — 剪贴板文本（大文本 + 防回环） | ~6 h | 复制 1 MiB 文本 → 对端粘贴成功；同端写回不再回环 | M1a |
| **M2a** — 剪贴板图片 + macOS | ~6 h | macOS 复制截图 → 对端粘贴为 PNG / DIB 字节级一致 | M1b |
| **M2b** — 剪贴板图片 + Windows / Linux | ~6 h | 三平台图片剪贴板端到端（macOS↔Windows 走 DIB） | M2a |
| **M3a** — 复制文件 + HTTP/3 transfer | ~8 h | 复制 200 MiB 文件 → 对端落盘 SHA-256 一致 | M2b |
| **M3b** — 文件接收 UI + 取消 + 中途拔网 | ~6 h | GUI 询问 / 自动接收；源端取消；拔网清晰报错 | M3a |
| **M4** — GUI 集成 + 文档 | ~5 h | GeneralPanel 剪贴板状态、Toaster 通知、配置落盘 | M3b |
| **合计** | **~54.5 h** | | |

> **STEP 时长门**：每 STEP 控制在人类实现 1.5 h（AI ~30 min）左右；超过 3 h 必拆。
> **M3 已知范围扩大风险**：若 M3a 估时实际超 10 h，按"接收端落盘 + 200 MiB 性能"与"UI / 取消 / 拔网"二分拆为 M3a' / M3a''。

## 3. 详细步骤

---

### M0a / M0b / M0c — ProtoEvent + HTTP/3 基础（拆分）

> **拆分原因**（评审 #1-#3 后）：单 milestone 内塞 9 个 STEP 估时超 12 h，按规则拆为三个可独立验证的子里程碑。
>
> | 子里程碑 | 估时 | 依赖 | 人类可测 |
> |---|---|---|---|
> | **M0a** ProtoEvent codec 双轨化 | ~3 h | — | `cargo test -p lan-mouse-proto` 全绿；Input/Ping/Pong/Hello 旧路径零行为差异 |
> | **M0b** HTTP/3 基础（spike + server + client） | ~4.5 h | M0a | 真机 `curl --http3 https://<peer>/healthz` 200 |
> | **M0c** StreamC 接通 + IPC 扩展 | ~4 h | M0b | StreamC reader 接通；GUI 看到 `ClipboardConfig` 字段；fmt/clippy/build 全绿 |
>
> 三段合计 ~11.5 h（仍在 12 h 边界内）。

**人类准备**：
- **两台真机（macOS + Windows 或 macOS + Linux）**：LAN 连通；防火墙 4252/UDP 开放；mDNS 工作（互 ping `*.local`）
- **curl / Python 脚本**：用 `--http3` 或 `aioquic` 测端到端 `GET /healthz`
- 详细步骤：见 M0a/M0b/M0c 各 STEP 完成后 LEADER 反馈

| STEP | 估时 | 任务 | 涉及文件 | 完成标志 |
|---|---|---|---|---|
| **0.0** | **1.5h** | **ProtoEvent codec 双轨化设计文档**（评审 #1）：先在 `lan-mouse-proto/src/codec.rs` 拆出两个 trait：`FixedCodec for InputEvent`（`[u8; 21]` 定长，最大 payload = `MAX_EVENT_SIZE`）+ `VarCodec for ClipboardText/Image/Files/FileTransfer*`（`Vec<u8>` 变长，length-prefixed）；`ProtoEvent` 顶层的 `From<ProtoEvent> for Vec<u8>` / `TryFrom<&[u8]> for ProtoEvent` 由 `match` 按 variant 分流（Input 走 Fixed、其余走 Var）；**不动**现有 `(*event).into()` 调用点的 `([u8; MAX_EVENT_SIZE], usize)` 路径（保留给 Input + Ping/Pong/Hello 的 hot path），新 `Vec<u8>` 路径专给剪贴板/文件用。设计文档 + trait 骨架 + 编译期单测（空 trait 即可） | `lan-mouse-proto/src/codec.rs`（新文件）、`lan-mouse-proto/src/lib.rs`（文档） | 文档说明两个 codec 的边界 + 调用点；`cargo build -p lan-mouse-proto` 通过 |
| 0.1 | 1.5h | ProtoEvent 加 `ClipboardText { fingerprint, sha256, size, content_inline: Option<Vec<u8>> }` / `ClipboardImage { fingerprint, mime, sha256, size }` / `ClipboardFiles { fingerprint, entries: Vec<FileEntry> }` / `FileTransferOffer { sha256, name, size, mime }` / `FileTransferResponse { sha256, accept: bool }` / `FileTransferCancel { sha256 }` / `ClipboardRequest { sha256 }`；更新 `EventType`（新增变体）；`encode_var` / `decode_var` 走 `Vec<u8>` + `length-prefix`；`encode_fixed` / `decode_fixed` 维持现状；**所有现有 `(*event).into()` 调用点不动**（Input/Ping/Pong/Hello 仍走 Fixed），只在 `read_frame` / `write_frame` 的 dispatcher 里 `match` variant 选 codec | `lan-mouse-proto/src/lib.rs`、`lan-mouse-proto/src/codec.rs` | `cargo test -p lan-mouse-proto` 全绿（含新增 round-trip + 旧 Ping/Pong/Hello 仍兼容 + InputEvent round-trip 字节级一致） |
| 0.2 | 1.5h | **h3 / h3-quinn spike + ALPN 路线图（评审 #2 + 评审 #1 3rd）**：写 `examples/h3_pingpong.rs`（独立 binary，**不**进 `lan-mouse` crate 主体）—— server 监听 bi_stream 回 200 OK，client GET `/healthz`；先 `cargo add h3 h3-quinn` 试 0.0.x 哪个 minor 与 quinn 0.11 兼容（用 `cargo tree` 验证 quinn 版本）。**同时**验证 ALPN 路线：当前 ALPN = `b"lan-mouse"`（`quic_transport/mod.rs:31`），h3 标准 ALPN 是 `b"h3"`；quinn 0.11 同一连接不支持双 ALPN，意味着 HTTP/3 要么换端口（与"单 4252/UDP"目标冲突）、要么自实现 HTTP/3-lite。**spike 必须二分叉**：<br>• **路径 1（h3 路线）**：尝试 h3-quinn 用 `b"h3"` ALPN 独立 endpoint + SO_REUSEPORT 共端口；预期**失败**（ALPN 在握手阶段由 client 选、server 端无法按 client 区分）<br>• **路径 2（HTTP/3-lite 路线）**：自实现简易 request/response 协议 over 裸 QUIC bidi stream（`[u16 method_len][method][u16 path_len][path][u32 body_len][body]`，够 `GET /clipboard/{text,image,file}/{sha256}` 拉字节用），ALPN 仍是 `b"lan-mouse"`<br>**spike 必跑 200 MiB 端到端（评审 #1 3rd）**：不仅 `/healthz`，还要跑 200 MiB random file 拉取 + 传输中 cancel + 传输中拔网，验证 quinn stream-finish race 在大规模数据下不暴露；**不通过**则 M0b 暂停，spike 输出失败原因到 `next/SUGGESTION-FIXED.md`，与用户对齐方案（候选：自实现精简 HTTP/3 over 裸 stream、或放弃 HTTP/3 改用纯 length-prefix 协议）<br>`quic_transport::http3` 模块骨架 + `build_server(conn) / build_request_conn(conn)` 接口（实现走路径 2） | `Cargo.toml`（spike 时临时）、`examples/h3_pingpong.rs`（spike，新）、`src/quic_transport/http3.rs`（新） | spike 三场景全绿：200 MiB 字节级一致、cancel 后 1 s 内停止、拔网后 5 s 内 `connection lost` 报错；`cargo build -p lan-mouse` 编译通过（路径 2 编译） |
| 0.3 | 1.5h | h3 server accept loop：复用 `PeerSession` 的 QUIC `Connection`，`h3::server::Connection` 接 bi_stream；注册路由表 `GET /healthz` → 200 + "ok"；`GET /clipboard/{kind}/{sha256}` / `GET /clipboard/file/{sha256}?range=` 暂时 404（后续 M1 / M2 / M3 填）；启动时只在"作为 server（accept 端）"时挂 server，client 端不挂 | `src/quic_transport/http3.rs` | 单测：mock 一个 bi_stream + req → /healthz 返回 200；/clipboard/text/abc 返回 404 |
| 0.4 | 1.5h | h3 client helper：`Http3Client::get_text(sha256) -> Vec<u8>` / `get_image(sha256) -> Vec<u8>` / `get_file(sha256, range) -> Vec<u8>`；限速：单文件 200 MiB（避免一次性 `Vec::with_capacity(200 MiB)`，改用 `tokio::io::sink` + 流式）；connection 复用同一条 QUIC `Connection` | `src/quic_transport/http3.rs` | 单测：mock server → client GET 拿到正确字节 |
| **0.5a** | **1.5h** | **StreamC 接通（发送 + ReadStreams）**（评审 #3 拆分）：a) `PeerSession` 加 `cached_send_c: Mutex<Option<SendStream>>` + `send_stream_c` 方法；b) `StreamEvent` 加 `ClipboardMeta(ProtoEvent)` 变体；c) `ReadStreams` 加 `c: Receiver<StreamEvent>` 字段；d) `PeerSession::run` 的 select! 加 `ClipboardMeta` 分支（暂只 log 收 + dispatch 给 service）；**不做** reader task | `src/quic_transport/streams.rs`、`src/quic_transport/session.rs` | `cargo build -p lan-mouse` 通过；`cargo test` 全绿；`Stream C is M2-only` 警告（`session.rs:582`）仍存（**预期**：reader 任务 0.5b 才接） |
| **0.5b** | **1.0h** | **StreamC 接通（reader task）**：a) `streams::read_loop` 启动 `read_stream_c_loop`（替代当前 `streams.rs:333` 的 `drop(stream_bunch.c)`）；b) `protocol::route_input`（应为 `route_clipboard`）增 StreamC 分支（**注意**：现有 `route_input` 只覆盖 Input 事件，需新增 `route_clipboard`）；c) 接收 `ClipboardMeta(ProtoEvent)` 转发到 service 收件箱 | `src/quic_transport/streams.rs`、`src/quic_transport/protocol.rs` | `Stream C is M2-only` 警告消失；StreamC 端到端可见（mock test：构造 ClipboardText 走 stream C → service 收到） |
| 0.6 | 0.5h | IPC 扩展（评审 #4 拆分）：<br>**daemon-global**：`lan_mouse_ipc::ClipboardConfig { auto_accept_files: bool, accept_dir: Option<PathBuf>, ignore_text: bool, ignore_images: bool, ignore_files: bool }`（`#[serde(default)]` 兼容旧 wire）；放 `lan_mouse_ipc::ClipboardConfig` 顶层，不挂在 `ClientConfig` 下；`FrontendRequest::SetClipboardConfig(ClipboardConfig)`（无 handle）<br>**per-peer**：`ClientConfig.enable_clipboard_to: bool`（`#[serde(default)]` 缺字段 = true 向后兼容）；`FrontendRequest::SetEnableClipboardTo(ClientHandle, bool)`<br>事件：`FrontendEvent::ClipboardState { last_text_ts, last_image_ts, last_file_ts, last_source: Option<String> }`（`last_source` 评审 #6 用来高亮"刚被 X 改了"） | `lan-mouse-ipc/src/lib.rs` | serde round-trip 单测：ClipboardConfig 缺字段 = default；`ClientConfig` 旧 wire 加载后 `enable_clipboard_to = true` |
| 0.7 | 1.0h | `cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` + 三平台编译（macOS / Windows / Linux）；真机 `curl --http3` 命中 `https://<peer>:4252/healthz`（人类配合） | — | L0 全绿 + 端到端 h3 路由可达 |

**M0a / M0b / M0c 里程碑交付**：
- ProtoEvent 支持新变体，向后兼容
- h3 server / client 在 QUIC `Connection` 上跑通
- StreamC reader 接通
- `curl --http3 https://<peer>:4252/.../healthz` 返回 200
- IPC 类型扩展就绪（GUI 尚未接）

---

### M1a — 剪贴板文本（小文本 ≤ 1 KiB）

**目标**：本机剪贴板变化 → 通过 StreamC 推 `<= 1 KiB` 文本 → 对端落剪贴板。**仅小文本**，不涉及 HTTP/3。
**AI 估时**：~6 h
**依赖**：M0a/M0b/M0c

**人类准备**：
- **macOS 真机一对**：终端跑 `echo hello | pbcopy` / `pbpaste`
- **Windows 真机一对**：PowerShell `Set-Clipboard -Value "hello"` / `Get-Clipboard`
- **Linux 真机一对**（X11 或 Wayland）：`xclip -selection clipboard` / `wl-paste` 任一

| STEP | 估时 | 任务 | 涉及文件 | 完成标志 |
|---|---|---|---|---|
| 1a.1 | 1.5h | `ClipboardBackend` trait：`fn current_text() -> Option<String>` / `fn set_text(&str)` / `fn watch() -> BoxStream<TextChange>`；platform 抽象接口 | 新建 `src/clipboard/mod.rs` | trait + dummy 实现（测试用） |
| 1a.2 | 1.5h | macOS 实现：NSPasteboard `string(forType: .string)` / `setString(_:forType:)` / `NSPasteboardGeneral.changeCount` 轮询（500 ms tick） | `src/clipboard/macos.rs`（新） | 终端 `pbcopy "x"` → daemon 日志看到 "clipboard change detected" |
| 1a.3 | 1.5h | Windows + Linux stub：Windows OpenClipboard / GetClipboardData / SetClipboardData；Linux 调 `xclip` / `wl-paste` 子进程（`tokio::process`）；本 STEP 仅 Linux X11 路径，Wayland 后续 M2 阶段做 | `src/clipboard/windows.rs`、`src/clipboard/linux.rs`（新） | Windows 端 Set-Clipboard 触发；Linux 端 xclip 路径触发 |
| 1a.4 | 1.5h | `service::clipboard_dispatcher`：backend 推 stream → 算 fingerprint（sha256 + 前 8 字节）→ 查"最近写过"集合（LRU 64 条）→ 推 StreamC（≤ 1 KiB 走 `ClipboardText { sha256, content_inline: Vec<u8> }` 元数据 + 内联字节；本 STEP 只走内联） | `src/service.rs`、`src/quic_transport/protocol.rs`（`route_clipboard` 改派发到 StreamC） | 复制 hello → 对端剪贴板出现 hello（macOS ↔ macOS 真机） |
| 1a.5 | 0.5h | fmt/clippy/build + 跨平台编译；macOS / Windows / Linux 真机各跑一遍小文本同步（人类配合） | — | L1a 全绿 + 三平台小文本端到端通 |

**M1a 里程碑交付**：
- 三平台本地剪贴板读取 + 写回
- 小文本通过 StreamC 端到端同步
- 仅指纹比对防"收到本地写回内容"的最简回环

**M1a 已知限制**：
- 文本 > 1 KiB 走元数据但 `content_inline` 字段暂未对接（等 M1b）
- 回环检测基于 fingerprint LRU，可能在快速同内容复制时漏检（极端 case）

---

### M1b — 剪贴板文本（大文本 > 1 KiB + 防回环加固）

**目标**：超过 1 KiB 的文本走"StreamC 元数据 + HTTP/3 拉取"；加固回环检测。
**AI 估时**：~6 h
**依赖**：M1a

**人类准备**：
- **生成大文本**：`head -c 1048576 /dev/urandom | base64 > /tmp/big.txt`（~1 MiB）
- **复制粘贴验证**：源端 `pbcopy < /tmp/big.txt` / `pbpaste > /tmp/back.txt`；`diff` 应为 0
- **回环测试**：在本端修改 → 不应触发对端再次推回

| STEP | 估时 | 任务 | 涉及文件 | 完成标志 |
|---|---|---|---|---|
| 1b.1 | 1.5h | StreamC `ClipboardText` 拆为"元数据 + 内联可选"：`<= 1 KiB` 走内联；`> 1 KiB` 只发 `sha256 + size` + 接收端 `ClipboardRequest { sha256 }` 触发 HTTP/3 GET | `lan-mouse-proto/src/lib.rs`（已加 `ClipboardRequest`） | 单测：1 KiB / 100 KiB / 1 MiB 走不同路径 |
| 1b.2 | 1.5h | 接收端 `service::clipboard_inbound` + 源端 cache 失效（评审 #3）：<br>**源端 push 前**：`service::clipboard_dispatcher` 发送 `ClipboardText` 元数据前，**主动**从 `clipboard_cache` 删除上一个 fingerprint（`cache.remove(prev_fingerprint)`），**不**仅依赖 LRU TTL；避免"旧 X 已 5min 过期、新 Y 推过来、接收端 5min 内拉 X 拿到旧内容"<br>**接收端收到 `ClipboardText`**：若有 `inline` → `backend.set_text`；若仅 `sha256` → 调 `Http3Client::get_text(sha256)` → `backend.set_text`<br>**HTTP/3 server `/clipboard/text/{sha256}` 路由**：从 `clipboard_cache`（key = sha256，5 min LRU 兜底）读 bytes；返回 404 时接收端**静默忽略**（log warn "cache miss"）—— 含义是"用户复制新内容前那次错过了"，不打断接收端 | `src/service.rs`、`src/quic_transport/http3.rs` | 单测：mock 源端 push X → 删旧 → push Y；接收端拉 X 拿 404、拉 Y 拿到正确内容；单测 404 静默不抛错 |
| 1b.3 | 1.5h | 回环检测加固 + 监控信号（评审 #4 3rd）：`service::clipboard_outbound` 写本地剪贴板前先 `mark_local_write(fingerprint)`，push LRU；收到对端推来内容 → 查 LRU → 命中则 skip；LRU 容量 128，TTL 60 s。**新增运行期信号**：`service::clipboard::metrics { skip_count: AtomicU64, allow_count: AtomicU64, last_skip_ts: AtomicU64 }`；每次 skip / allow 计数 +1；`RUST_LOG=lan_mouse_service::clipboard=trace` 时每 60 s 打印一次命中率（`skip/(skip+allow)`）；M4 STEP 4.4 进一步 UI 展示 | `src/service.rs` | 单测：本地写入 fingerprint "abc" → 收到对端 "abc" → 不重复 set_text；TTL 过期后可重新同步；单测覆盖 metrics 计数 |
| 1b.4 | 1.5h | 跨平台端到端：macOS ↔ Windows / macOS ↔ Linux / Windows ↔ Linux 三组各跑一遍（人类配合）：小文本 / 大文本 / 同一内容重复复制 / 不同源端快速切换 | `tests/manual/clipboard-text.md`（新模板） | 三组真机均通过：1 MiB 文本粘贴字节级一致；重复内容不触发回环 |

**M1b 里程碑交付**：
- 大文本走 HTTP/3 拉取，1 MiB 文本端到端字节级一致
- 回环检测在 128 条 LRU + 60 s TTL 内可靠
- 三平台互相同步

---

### M2a — 剪贴板图片基础设施 + macOS

**目标**：图片剪贴板读取 / 写回；SHA-256 指纹；图片字节走 HTTP/3。
**AI 估时**：~6 h
**依赖**：M1b

**人类准备**：
- **macOS 真机一对**：截图工具（Cmd+Shift+4 截屏 / `screencapture` 命令行）
- **4K 截图准备**：`screencapture -x -t png /tmp/4k.png`（约 5-15 MiB）

| STEP | 估时 | 任务 | 涉及文件 | 完成标志 |
|---|---|---|---|---|
| 2a.1 | 1.5h | `ClipboardBackend` 加图片方法：`fn current_image() -> Option<ImageBytes>` / `fn set_image(bytes: &[u8], mime: Mime)` / `fn watch_image() -> BoxStream<ImageChange>`；`ImageBytes { mime: String, data: Vec<u8> }`；mime 检测（PNG magic `89 50 4E 47` / JPG `FF D8 FF` / BMP `42 4D`） | `src/clipboard/mod.rs` | mime 检测单测覆盖 3 种格式 + 非图片返回 None |
| 2a.2 | 1.5h | macOS 实现 + 源端强制 PNG 归一化（评审 #2 3rd）：**优先**直接 `NSPasteboardGeneral.data(forType: .png)` 读 PNG bytes（绝大多数 macOS 应用都提供）；**fallback 路径**：`data(forType: .tiff)` 读 TIFF → 用 `image` crate 解码 → **强制重新编码为 PNG** 再传（Reviewer 2nd #2 指出 Preview.app 复制选中区域只提供 TIFF，源端不归一化 → 对端字节级一致永远不成立；归一化到 PNG 后**所有对端**都按 PNG 处理，与 REQUIREMENT §4.3 一致）；写入 `setData(_:forType: .png)` 把 PNG bytes 写回；changeCount 轮询扩展到图片；`image` crate 解码失败的 TIFF（罕见损坏）记 warn + skip | `src/clipboard/macos.rs` | 截一张图 → daemon 日志看到 image change；`Preview.app` 复制选中区域 → 日志看到 "TIFF→PNG 归一化" |
| 2a.3 | 1.5h | 图片 outbound：`service::clipboard_dispatcher` 扩展图片分支：算 sha256（内容指纹）→ `ClipboardImage { fingerprint, mime, sha256, size }` 元数据走 StreamC；图片字节暂存本地 `clipboard_cache`（key = sha256，5 min LRU 200 MiB 上限）；HTTP/3 server `/clipboard/image/{sha256}` 从 cache 返回 | `src/service.rs`、`src/quic_transport/http3.rs` | macOS 真机复制截图 → 对端剪贴板出现 |
| 2a.4 | 1.5h | 图片 inbound + 回环：收到 `ClipboardImage` → 查 LRU 跳过；若新 → 调 `Http3Client::get_image(sha256)` → `backend.set_image`；本地 `mark_local_image_write(fingerprint)`；图片回环集合独立于文本（容量 32，因为图片成本高） | `src/service.rs` | 单测：mock 4K 截图（5 MiB）→ 接收端落剪贴板字节级一致；同 fingerprint 不重复写 |

**M2a 里程碑交付**：
- macOS 图片剪贴板端到端（4K 截图字节级一致）
- 图片走 StreamC 元数据 + HTTP/3 字节
- 图片回环检测独立

---

### M2b — 剪贴板图片 + Windows / Linux

**目标**：Windows / Linux 图片剪贴板读取 / 写回。
**AI 估时**：~6 h
**依赖**：M2a

**人类准备**：
- **Windows 真机一对**：Snipping Tool / `Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.Clipboard]::GetImage()`
- **Linux 真机一对**：GNOME / KDE 截图工具；`wl-paste --type image/png` / `xclip -selection clipboard -t image/png -o`

| STEP | 估时 | 任务 | 涉及文件 | 完成标志 |
|---|---|---|---|---|
| 2b.1 | 1.5h | Windows 实现（**评审 #4 + 评审 #3 3rd 调整**）：**优先**用 `CF_DIBV5`（保留完整 alpha 通道 + 原始 BITMAPV5HEADER 字节），不用 `image` crate 编码；写入路径 `SetClipboardData(CF_DIBV5, dib_bytes)`；读取路径 `GetClipboardData(CF_DIBV5)` → 直接把 DIB bytes 当成"伪 PNG 容器"在 StreamC 元数据标 `mime = "application/x-dib"`，接收端若支持直接落剪贴板（macOS NSImage 支持 DIB 解码）/ 不支持则转 PNG（用 `image` crate 解码 DIB 后再编码为 PNG，**有损但 fallback**）。Wire 上 mime 字段决定走哪条。**评审 #3 3rd 验证**（macOS 端）：spike 测 `NSImage(data: dib_bytes) → rep → setData(_:forType: .png)` 写回 NSPasteboard → `data(forType: .png)` 重新读出 → sha256 与原始 DIB 不一致（NSImage 内部 surface 变换导致字节级不保真）→ 失败则降级为"视觉一致"路径（macOS 端也走 `image` crate 统一转 PNG），UI 提示"图片已转换格式" | `src/clipboard/windows.rs` | Windows 截一张图 → daemon 日志看到 image change；macOS spike：NSImage DIB round-trip sha256 比对记录到日志（**通过** = 字节级一致保真；**失败** = 降级为视觉一致） |
| 2b.2 | 1.5h | Linux X11 / Wayland 实现 + Wayland XWayland fallback（评审 #6 3rd）：<br>**X11**：`xclip -selection clipboard -t image/png -o` / `-i`（子进程调 `tokio::process`）<br>**Wayland**：`wl-paste --type image/png` / `wl-copy`<br>**探测 + fallback**（启动时一次性决定）：1) 探测 `WAYLAND_DISPLAY` env → 优先 Wayland；2) 探测 `wl-paste` 是否在 `$PATH` → 有则走 Wayland；3) 缺工具 → 探测 `DISPLAY` env + `xclip` → 走 XWayland xclip 路径（**评审 #6 3rd**：避免剪贴板功能完全不可用）；4) 两者都缺 → log error "剪贴板不可用：请安装 wl-clipboard 或 xclip"（**不**是 fatal 错，daemon 继续跑，键鼠功能不受影响）<br>运行时再轮询（图片剪贴板 change 频繁）按启动决策走 | `src/clipboard/linux.rs` | Linux X11 截一张图 → daemon 日志看到 image change；Wayland 装 wl-clipboard → 走 Wayland；Wayland 缺工具 + 有 XWayland → 自动 fallback xclip；三者都缺 → log error 清晰提示 |
| 2b.3 | 1.5h | 三平台互传矩阵：macOS ↔ Windows / macOS ↔ Linux / Windows ↔ Linux 各跑一次（人类配合），每组一张 4K 截图 + 一张 1080p JPG；`xxd | sha256sum` 验证源端 + 对端字节级一致 | `tests/manual/clipboard-image.md`（新模板） | 6 组真机互测：sha256 一致 |
| 2b.4 | 1.0h | fmt/clippy/build；Cargo 加 `image` crate dep（PNG / JPG 解码） | `Cargo.toml` | L2b 全绿 + 三平台图片端到端 |

**M2b 里程碑交付**：
- 三平台图片剪贴板同步
- 4K 截图 + 1080p JPG 字节级一致
- 图片回环检测覆盖三平台

---

### M3a — 复制文件 + HTTP/3 transfer

**目标**：源端复制文件 → 元数据走 StreamC + 字节走 HTTP/3 → 接收端落盘 + SHA-256 校验；不涉及 UI。
**AI 估时**：~8 h
**依赖**：M2b

**人类准备**：
- **大文件准备**：`dd if=/dev/urandom of=/tmp/big.bin bs=1M count=200`（200 MiB 随机数）
- **不同大小文件**：1 KiB / 1 MiB / 200 MiB 各一份
- **接收目录**：源端 / 接收端都准备 `/tmp/received/`（清空）

| STEP | 估时 | 任务 | 涉及文件 | 完成标志 |
|---|---|---|---|---|
| 3a.1 | 1.5h | 文件元数据采集：`FileEntry { name: String, size: u64, mime: String, sha256: [u8; 32] }`；`fn collect_files(paths: &[PathBuf]) -> Result<Vec<FileEntry>>`（拒绝目录、单个 > 4 GiB 警告）；sha256 流式计算（`tokio::io::AsyncReadExt` + `sha2::Sha256::update`） | 新建 `src/clipboard/file_meta.rs` | 单测：1 KiB / 1 MiB / 200 MiB 文件 sha256 正确（`sha256sum` 对比） |
| 3a.2 | 1.5h | 源端 outbound：OS 剪贴板含"文件"（macOS NSFilenamesPboardType / Windows CF_HDROP / Linux text/uri-list）→ `service::clipboard_dispatcher.file_branch` 调 `collect_files` → `ClipboardFiles { fingerprint, entries }` 走 StreamC；文件字节暂存 `file_cache`（key = sha256，1 GiB LRU） | `src/service.rs`、`src/clipboard/file_meta.rs` | macOS Finder 复制 200 MiB 文件 → 日志看到 entries 列表 |
| 3a.3 | 1.5h | 接收端 inbound（暂不询问）：收到 `ClipboardFiles` → 假定 `auto_accept_files = true`（M3b 改 UI） → 逐个 `Http3Client::get_file(sha256)` → 落 `<accept_dir>/<name>`（同名加 `(1)`、`(2)` 后缀） → 重新算 sha256 校验 | `src/service.rs`、`src/quic_transport/http3.rs`（`/clipboard/file/{sha256}` 路由） | 200 MiB 文件对端落盘 + sha256sum 一致 |
| 3a.4 | 1.5h | HTTP/3 server 文件字节流：`/clipboard/file/{sha256}` 从 `file_cache` 读 → 流式返回（不一次性 `Vec::with_capacity(200 MiB)`）；range 请求支持（`?range=0-1023`，为后续断点续传留接口，本计划只 stub 200 OK） | `src/quic_transport/http3.rs` | 单测：mock 200 MiB 文件 → range=0-99 拿到前 100 字节；后续断点续传 plan 留接口 |
| 3a.5 | 1.5h | 取消机制：源端 `Ctrl+C` 或剪贴板被新内容覆盖 → 发 `FileTransferCancel { sha256 }` 走 StreamC；接收端若正在下载 → 关闭 HTTP/3 stream；源端 `file_cache` 删除对应 sha256 | `lan-mouse-proto/src/lib.rs`（已加）、`src/service.rs` | 单测：源端 cancel → 接收端在 1 s 内停止下载并清空 .partial 文件 |

**M3a 里程碑交付**：
- 200 MiB 文件传输端到端通（SHA-256 一致）
- 源端取消接收端能响应
- HTTP/3 流式传输不爆内存

**M3a 已知限制**：
- **本 STEP 默认 auto_accept = true**（无 UI），用户会看到"文件无声落到 /tmp/received/"；M3b 加 UI 询问
- 断点续传仅 stub；中途拔网后"清晰报错"而非"重传"在 M3b 阶段验证

---

### M3b — 文件接收 UI + 取消 + 中途拔网

**目标**：GUI 询问 / 自动接收开关；Toaster 通知；中途拔网清晰报错。
**AI 估时**：~6 h
**依赖**：M3a

**人类准备**：
- **配置接收目录**：在 config.toml 或 GUI 设置 `accept_dir = "/Users/me/Downloads/lan-mouse"`
- **中途拔网测试**：传输 200 MiB 过程中拔网线 / 关闭对端 Wi-Fi，观察错误信息
- **大文件准备**：同 M3a

| STEP | 估时 | 任务 | 涉及文件 | 完成标志 |
|---|---|---|---|---|
| 3b.1 | 1.5h | IPC 扩 `ClipboardConfig`：`auto_accept_files: bool` / `accept_dir: Option<PathBuf>` / `ignore_files: bool`（快速 disable 文件同步）；`FrontendEvent::FileTransferRequest { sha256, name, size, mime, source: String }`；`FrontendRequest::RespondFileTransfer { sha256, accept: bool, save_dir: Option<PathBuf> }` | `lan-mouse-ipc/src/lib.rs` | serde round-trip 单测 |
| 3b.2 | 1.5h | 接收端询问流程 + Toaster actions（评审 #5）：收到 `ClipboardFiles` → 不直接下载 → 发 `FileTransferRequest` 走 IPC；前端 **Toaster 扩 `actions?` 字段**（已在 `api/ipc.ts:44` 可扩展）—— `actions?: Array<{ label: string, request: FrontendRequest }>`；Toaster 模板渲染 action 按钮（不破坏现有 `dismissToast` 流程）→ 用户点 [Accept] / [Reject] → 发 `RespondFileTransfer { sha256, accept, save_dir }` IPC；daemon 收到后：accept 走 `Http3Client::get_file` 下载；reject 发 `FileTransferCancel` 通知源端清理 | `src/service.rs`、`src/web.rs`（IPC 推送）、`lan-mouse-vue/src/components/Toaster.vue`（actions 模板）、`lan-mouse-vue/src/api/ipc.ts`（`Toast.actions?` 类型） | 复制 200 MiB 文件 → GUI 弹通知带 [Accept] [Reject] → 点击 accept 落盘 + SHA-256 一致；点 reject 源端收到 cancel |
| 3b.3 | 1.5h | 中途拔网处理：HTTP/3 客户端 stream 错误 → `service::file_inbound_err` → IPC 推 `FrontendEvent::FileTransferFailed { sha256, reason: String }` → 落 `.partial` 保留还是删除？（默认删除，留 .partial 风险）；用户 config `keep_partial: bool` 控制 | `src/service.rs` | 拔网后 5 s 内 GUI 看到"传输失败：connection lost"；重启后未保留 .partial |
| 3b.4 | 1.5h | 端到端性能 + 收尾（评审 #5 3rd 双档）：200 MiB 文件传输性能分两档——**有线 100 Mbps LAN < 30 s**（理论 16 s + QUIC 加密/流控/sha256 余量）+ **Wi-Fi 实际带宽（有线 30-50 %） < 60 s**；UI 端到端 + 取消 + 拔网三种 case（人类配合） | `tests/manual/file-transfer.md` | 有线 < 30 s、Wi-Fi < 60 s；三种 case 均符合预期；fmt/clippy/build 全绿 |

**M3b 里程碑交付**：
- GUI 询问 / 自动接收双模式
- Toaster 通知 + 接受 / 拒绝
- 拔网清晰报错（不卡死）
- 200 MiB 在 100 Mbps LAN < 30 s

---

### M4 — GUI 集成 + 文档

**目标**：ConnectionsPanel / GeneralPanel 配置剪贴板；Toaster 通知已就位；clipboard 状态显示；README / DOC.md 文档更新。
**AI 估时**：~5 h
**依赖**：M3b

**人类准备**：
- **浏览器打开 GUI**：`pnpm dev` 起 Vite；WebSocket console 可见
- **三平台真机** + 已配对 QUIC 链路（同 M0a/M0b/M0c 准备）

| STEP | 估时 | 任务 | 涉及文件 | 完成标志 |
|---|---|---|---|---|
| 4.1 | 1.5h | Vue 类型 + IPC 绑定：`api/ipc.ts` 加 `ClipboardConfig` / `ClipboardState` / `FileTransferRequest` / `FileTransferFailed`；`store/index.ts` 维护 `state.clipboardConfig: ClipboardConfig` / `state.lastClipboardText` / `state.lastClipboardAt`；`onMounted` 监听 `ClipboardState` 事件 | `lan-mouse-vue/src/api/ipc.ts`、`lan-mouse-vue/src/store/index.ts` | 浏览器 console 看到状态同步 |
| 4.2 | 1.5h | **ClipboardConfig 移到 GeneralPanel + per-peer 开关**（评审 #4 改写）：<br>**GeneralPanel**：加剪贴板区块 — `auto_accept_files` checkbox / `accept_dir` 文本框 / `ignore_text` / `ignore_images` / `ignore_files` 三个 ignore checkbox；`onChange` 调 `SetClipboardConfig`（无 handle）；`src/config.rs` TOML 加 `[clipboard]` 段（**daemon-global**），不挂在 `[[clients]]` 下<br>**ConnectionRow**：每个 client 行加 `enable_clipboard_to` checkbox（label "Push clipboard to this peer"）；`onChange` 调 `SetEnableClipboardTo(handle, bool)`；`[[clients]]` TOML 段加 `enable_clipboard_to = true` 字段 | `lan-mouse-vue/src/components/GeneralPanel.vue`、`lan-mouse-vue/src/components/ConnectionsPanel.vue`、`src/config.rs` | 改 checkbox 立即生效；config.toml 落盘正确（顶层 `[clipboard]` + 每个 `[[clients]]` 内 `enable_clipboard_to`） |
| 4.3 | 1.0h | Toaster 扩 `FileTransferRequest`：`name` / `size` / `source` / [Accept] [Reject] 按钮；点击 → `RespondFileTransfer` IPC | `lan-mouse-vue/src/components/Toaster.vue` | 文件传输通知正确弹 + 点击 accept 落盘 |
| 4.4 | 1.0h | GeneralPanel 剪贴板状态卡片 + 多源提示 + 回环跳过统计（评审 #6 + 评审 #4 3rd）：`state.lastClipboardText` / `state.lastClipboardAt` 渲染最近同步时间（相对时间）+ 文本预览（前 80 字符）；**新增** `state.lastClipboardSource: Option<String>`（peer hostname / fingerprint）+ `state.lastClipboardSourceAt: Option<DateTime>` —— UI 显示"上次来源"字段（与 BindingInvalid 提示同模式）；**新增** 5 s 高亮提示：当 `ClipboardState.last_source_at` 与 `now()` 差 ≤ 5 s 且 `last_source != local` 时，UI 高亮一条 amber 条"剪贴板刚被 <peer> 改了"（避免用户把对端推过来的内容误以为是本端自己复制的）；**新增** 回环跳过统计卡片——显示"过去 1 小时回环跳过 N 次"（数据来自 M1b STEP 1b.3 的 `service::clipboard::metrics`） | `lan-mouse-vue/src/components/GeneralPanel.vue`、`lan-mouse-vue/src/store/index.ts` | 复制文本后 1 s 内 UI 显示；对端覆盖时 5 s 高亮淡出；回环跳过分时计数显示正确 |
| 4.5 | 1.0h | 文档：`README.md` / `DOC.md` 加章节"跨设备剪贴板"+"文件同步"；`config.toml` 注释更新；fmt/clippy/build + pnpm build + 三平台真机端到端（人类配合） | `README.md`、`DOC.md`、`config.toml` | L4 全绿 + 三平台 GUI 端到端 |
| **4.6** | — | **CLI 集成（评审 #7）**：`lan-mouse-cli` 加 `SetClipboardConfig` 子命令（与 `SetQuicIdleTimeout` 同模式：发 IPC → daemon 写 TOML → 回 echo）；`SetEnableClipboardTo <handle> <bool>` 子命令；与现有 `SetMonitor` 共用同一 dispatch pattern；单测覆盖 IPC 编码 | `lan-mouse-cli/src/lib.rs` | `lan-mouse-cli SetClipboardConfig --auto-accept-files true --accept-dir /tmp/recv` 生效；`lan-mouse-cli SetEnableClipboardTo 0 false` 关掉对端 0 的剪贴板推送 |

**M4 里程碑交付**：
- GUI 完整可配置 + 可观察
- README / DOC.md 同步
- 所有 milestone 文档化

---

## 4. 总估时汇总

| 里程碑 | AI 估时 | 人类验证需求 | 状态 |
|---|---|---|---|
| M0a | 3h | ProtoEvent 旧路径回归（Ping/Pong/Hello 兼容） | 待启动 |
| M0b | 4.5h | 真机 h3 `curl --http3` 命中 /healthz | 待启动 |
| M0c | 4h | StreamC reader 接通 + IPC ClipboardConfig 字段 | 待启动 |
| M1a | 6h | 三平台小文本复制粘贴 | 待启动 |
| M1b | 6h | 1 MiB 文本端到端 + 回环检测 | 待启动 |
| M2a | 6h | macOS 4K 截图字节级一致 | 待启动 |
| M2b | 6h | 三平台图片互传矩阵 | 待启动 |
| M3a | 8h | 200 MiB 文件落盘 + SHA-256 + 取消 | 待启动 |
| M3b | 6h | GUI 询问 + 拔网清晰报错 | 待启动 |
| M4 | 5h | 三平台 GUI 端到端 + 文档同步 | 待启动 |
| **合计** | **~51 h** | | |

> **校准系数**：与 `PLAN-1` 一致，30 min AI ≈ 1.5 h 人类。`image` crate 编译时间 + h3 crate 体积可能拉长 M2 / M0b/M0c，预留 ±20 % buffer。
>
> **M3 风险点**：HTTP/3 200 MiB 流式传输 + 取消 + 拔网可能踩 quinn/h3 上游的 stream-finish race condition。若 M3a 超 10 h，按"传输核心 vs. UI / 取消"二分拆为 M3a' / M3a''。

---

## 5. 关键风险与不确定性

> 本节合并了**两轮外部评审报告**（2026-09-06）的 12 条风险与本计划作者的 8 条原始风险；**第一轮 #1-#4 已采纳并落到对应 STEP，#5 已审视未采纳**；**第二轮 #1-#7 全部采纳**并落到对应 STEP。理由附在条目下。

1. **【评审 #1，已采纳】ProtoEvent 变长 codec 双轨化设计**：现有 `From<ProtoEvent> for ([u8; 21], usize)` 是定长 codec；评审指出若直接把 `ProtoEvent` 整体改 `Vec<u8>` 编码，会牵扯 `session.rs:396/574/578`、`protocol.rs:454/512` 等 5-6 个调用点。**已采纳**：M0a 新增 **STEP 0.0**（1.5h）做 codec 双轨化设计——`FixedCodec for InputEvent` + `VarCodec for Clipboard*/FileTransfer*`，顶层 `From` / `TryFrom` 按 variant `match` 分流；现有 `(*event).into()` 调用点全部保留 Input/Ping/Pong/Hello 的 Fixed 路径，只在 dispatcher 增 1 处 `match`。**预期节省返工 3-5h**。
2. **【评审 #2，已采纳】h3 兼容性 + spike 先行**：Cargo.lock 当前**没有** h3 任何痕迹（已 grep 验证），无法"锁版本到已锁的 minor"。**已采纳**：M0b **STEP 0.2** 改为先做 `examples/h3_pingpong.rs` minimal demo（30+30 行），用 `cargo tree` 验证 quinn 0.11 兼容版本，再正式入 workspace deps。**spike 失败** → M0b 暂停，与用户对齐方案（候选：自实现简化 HTTP/3 over QUIC bidi stream）。
3. **【评审 #3，已采纳】StreamC 接通拆 a/b**：评审指出 0.5 实有 6 个子改动（`cached_send_c` / `send_stream_c` / `StreamEvent::ClipboardMeta` / `ReadStreams.c` / `PeerSession::run` select! 分支 / `read_stream_c_loop` / `route_clipboard`），原 1.5h 偏紧。**已采纳**：拆为 **0.5a（1.5h，加 send + ReadStreams）** + **0.5b（1.0h，加 reader + 路由）**；若 0.5a 超 1.5h，LEADER 进一步拆 0.5a 为 0.5a-i / 0.5a-ii。
4. **【评审 #4，已采纳】Windows 图片字节级一致 vs. REQUIREMENT §4.3**：评审指出 `CF_BITMAPINFO` → PNG 走 `image` crate 有损，**会破坏 REQUIREMENT §4.3 "4K 截图字节级一致"承诺**。**已采纳方案**：
   - Windows 端 `SetClipboardData(CF_DIBV5, dib_bytes)` 直接传 DIB 字节，**不**经 `image` crate 重编码；
   - StreamC 元数据 `mime = "application/x-dib"`；
   - 接收端（macOS / Linux）若原生支持 DIB 解码（macOS NSImage 支持）→ 字节级一致落剪贴板；不支持则 fallback 到 `image` crate 解码再编码 PNG（**此时声明降级为"视觉一致"**，落到 logs/UI 提示用户"图片已转换格式"）。
   - **用户验收**（M2b STEP 2b.3 人类测试）：macOS↔Windows 走 DIB 路径，sha256sum 应一致；其它平台走 PNG，sha256sum 一致。
5. **【评审 #5，已审视、未采纳】idle_timeout 默认 5s 关链风险**：评审担心 200 MiB 传输后 5s 即关链。**未采纳理由**：HTTP/3 stream 在 200 MiB 传输期间有持续流量，QUIC idle timer 不触发；传输完成后 daemon 回到键鼠事件，QUIC keep-alive（5s 间隔）应能保活。**未单独改 default**（不在 PLAN-2 范围）。M0c STEP 0.7 / M3b STEP 3b.4 真机测试时**顺手回归**一下"200 MiB 完成后 30s 内不关链"，异常则进 `next/SUGGESTION.md`。
6. **macOS NSPasteboard changeCount 轮询精度**：500 ms tick 是合理估值，但快速复制两次（< 500 ms）可能漏。改进：收到 `changeCount` 变化后立即 dispatch，不等下一 tick。
7. **Linux Wayland 剪贴板**：wl-clipboard 工具未必预装；M2b 启动时探测，缺工具给清晰错误（"install wl-clipboard"）。
8. **回环 LRU 容量与 TTL**：128 条 / 60 s 是估值。极端 case（用户 1 分钟内 200 条不同内容复制）会失效。运行期监控命中率。
9. **200 MiB HTTP/3 流式传输 + 取消的 race**：源端发 `FileTransferCancel` 时，接收端 HTTP/3 stream 可能已收到 99 % 字节。需要明确"cancel 早于 stream finish 视为已取消"语义。
10. **多屏 / 多端同时复制**：两个对端同时推不同内容，最后一个写者赢（REQUIREMENT 明确接受）。但 UI 上应提示"对端 X 在 Y 时间改了剪贴板"。
11. **剪贴板监听权限**：
    - macOS：Accessibility / Input Monitoring（提示需重启授权）
    - Windows：UAC 不影响剪贴板读取
    - Linux Wayland portal：需 `wlr-data-control` 协议版本 ≥ 2

---

**第二轮评审（2026-09-06）补充：**

12. **【评审 #1（2nd），已采纳】`lan-mouse-proto` 版本号 + wire-level 兼容策略**：原计划假设"不 bump proto"，但新增 7 个 EventType 变体是主版本变更。**已采纳**：bump 到 `0.4.0`；剪贴板 / 文件事件**全部走 StreamC**（旧 daemon 的 `read_loop` 已默默 drop `stream_bunch.c` 见 `streams.rs:291`，不会触发 stream A 的 `EventType::try_from(InvalidEventId)` 断链）→ 新旧 daemon 在 stream A 仍能正常键鼠互通，仅剪贴板 / 文件功能单向有效。**不**走 stream A 编码新 EventType。
13. **【评审 #2（2nd），已采纳】ALPN 与 h3 共存方案缺位**：当前 ALPN = `b"lan-mouse"`，h3 标准 ALPN = `b"h3"`；quinn 0.11 同一连接不支持双 ALPN。**已采纳**：M0b STEP 0.2 spike 强制**二分叉**——路径 1（h3-quinn + 双 ALPN endpoint）预期失败（ALPN 在握手阶段由 client 选、server 无法按 client 区分）；路径 2（自实现 HTTP/3-lite over 裸 QUIC bidi stream，`[u16 method][u16 path][u32 body]` 简帧，够 `GET /clipboard/{text,image,file}/{sha256}` 拉字节用，ALPN 仍 `b"lan-mouse"`）落地。spike 失败 → M0b 暂停与用户对齐。
14. **【评审 #3（2nd），已采纳】clipboard_cache 失效 push/pull race**：源端 push 新 fingerprint 前**主动**从 cache 删除旧 fingerprint（不依赖 LRU TTL），避免"旧 X 已过期、新 Y 推过来、接收端拉到旧 X"；接收端 GET 收到 404 静默忽略（log warn），不抛错。已落到 M1b STEP 1b.2。
15. **【评审 #4（2nd），已采纳】ClipboardConfig 位置 per-client vs daemon-global**：剪贴板监听是 daemon 全局（一个 OS 剪贴板喂所有 peer），接收目录也是全局。**已采纳**：ClipboardConfig { auto_accept_files, accept_dir, ignore_text, ignore_images, ignore_files } 移到 `GeneralPanel` 顶层（daemon-global TOML `[clipboard]` 段）；per-peer 只留 `ClientConfig.enable_clipboard_to: bool` 细粒度开关（"我是否要把剪贴板推给这个 peer"），与 `input_channels` per-client 配置不冲突。
16. **【评审 #5（2nd），已采纳】Toaster 需要 accept/reject action**：当前 `Toaster.vue:8-15` 只支持 dismiss。**已采纳**：扩 `Toast.actions?: Array<{ label, request: FrontendRequest }>` 字段，模板渲染 action 按钮（不破坏 dismiss 流程）。已落到 M3b STEP 3b.2。
17. **【评审 #6（2nd），已采纳】多源 / 多端同时复制 UI 提示缺失**：REQUIREMENT 接受"最后写者赢"，但 UI 应提示用户。**已采纳**：M4 STEP 4.4 追加 `ClipboardState.last_source` / `last_source_at` 字段，5 s 内 amber 高亮"剪贴板刚被 <peer> 改了"（与 BindingInvalid 提示同模式）。
18. **【评审 #7（2nd），已采纳】CLI 集成完全没提**：CLI-only 用户只能手动编辑 TOML。**已采纳**：M4 新增 **STEP 4.6**——`lan-mouse-cli::SetClipboardConfig` / `SetEnableClipboardTo <handle> <bool>` 子命令，与 `SetQuicIdleTimeout` 同模式。

---

**第三轮评审（2026-09-06）补充：**

19. **【评审 #1（3rd），已采纳】HTTP/3-lite spike 范围不足**：原 spike 仅 30+30 行 `GET /healthz`，验不了 200 MiB 流式 + 取消 + 拔网的 race；quinn stream-finish race 在大规模数据下才会暴露。**已采纳**：M0b STEP 0.2 spike **强制跑 200 MiB 端到端含"传输中 cancel"和"传输中拔网"两个场景**；不通过则 M0b 暂停，与用户对齐方案（候选：自实现精简 HTTP/3 over 裸 stream、或放弃 HTTP/3 改用纯 length-prefix 协议）。
20. **【评审 #2（3rd），已采纳】macOS NSPasteboard TIFF-only 来源破字节级一致**：Preview.app 复制选中区域只提供 TIFF，源端 TIFF → 对端字节级一致永远不成立。**已采纳方案 A**：M2a STEP 2a.2 明确"源端强制 PNG 归一化"——`data(forType: .tiff)` 读 TIFF → `image` crate 解码 → **强制重新编码为 PNG** 再传；归一化到 PNG 后所有对端都按 PNG 处理，与 REQUIREMENT §4.3 一致。
21. **【评审 #3（3rd），已采纳】macOS NSImage DIB 解码无 codebase 依据**：NSImage 内部 surface 变换可能改变像素（虽 surface bytes 不变，但 surface → NSImage → NSPasteboard 写回时可能再变换）。**已采纳**：M2b STEP 2b.1 加 spike 测 `NSImage(data: dib_bytes) → rep → setData → 重新读出 → sha256`；**通过** = 字节级一致保真；**失败** = 降级为"视觉一致"路径（macOS 端也走 `image` crate 统一转 PNG），UI 提示"图片已转换格式"。
22. **【评审 #4（3rd），已采纳】LRU 命中率无运行期信号**：原 plan 说"运行期监控命中率"但没 STEP 落实。**已采纳**：M1b STEP 1b.3 加 `service::clipboard::metrics { skip_count, allow_count, last_skip_ts }`（`AtomicU64`）；`RUST_LOG=lan_mouse_service::clipboard=trace` 时每 60 s 打印命中率；M4 STEP 4.4 GeneralPanel 卡片显示"过去 1 小时回环跳过 N 次"。
23. **【评审 #5（3rd），已采纳】200 MiB 性能目标紧**：30s 留给 QUIC 加密 + 流控 + sha256 余量约 2x；Wi-Fi 实际带宽只有有线 30-50%。**已采纳**：M3b STEP 3b.4 拆**双档**——"100 Mbps **有线** LAN < 30 s" + "Wi-Fi < 60 s"。
24. **【评审 #6（3rd），已采纳】Linux Wayland 缺工具无 fallback**：wl-clipboard 未必预装，Wayland 用户没装时只给"清晰错误"——剪贴板功能完全不可用。**已采纳**：M2b STEP 2b.2 加探测优先级 + fallback——Wayland 缺工具 → 探测 XWayland xclip → 仍缺 → log error "请安装 wl-clipboard 或 xclip"（daemon 继续跑，键鼠不受影响）。

---

## 6. 后续（Out of Scope，本计划结束后再立 PLAN-M3）

| 内容 | 估时 | 备注 |
|---|---|---|
| 剪贴板格式协商（HTML / RTF / 自定义二进制） | ~4 h | 需 `ClipboardType` 枚举 + mime 协商 |
| 文件断点续传 | ~6 h | HTTP/3 `?range=` 接口已在 M3a stub；需要 source 端持久化 + target 端断点恢复 |
| 剪贴板历史 / 跨设备最近 N 条 | ~4 h | 需新增 `ClipboardHistory` 元数据事件 + UI 列表 |
| Wayland portal 兼容 | ~3 h | 需 `wlr-data-control` 协议 + 适配 layer_shell |

---

## 7. 执行约定

- 沿用 `PLAN-1` 的 STEP 流程：派发 `plan-step-executor` → LEADER 统计累计时间 → 触发 `step-validator`（累计 > 1 h 或 milestone 结束）→ 接受 / 返工 → commit。
- STEP 偏差超过 45 min AI 时，LEADER 介入重拆。
- 跨 STEP 影响的小问题记入 `next/SUGGESTION.md`，由执行者决定 FIXED / IGNORE。
- 每完成一个 milestone：LEADER 提交 git（commit message 格式英文，不带 M / STEP 编号），更新 `next/.LEADER-STATE.md`。
- M1b / M2b / M3b 完成后与用户对齐下一里程碑；M3a 完成后**必须**用户介入验证（200 MiB 性能 + 取消语义是核心验收点）。

---

## 8. 测试矩阵：自动 vs 人类协助

> **核心原则**：协议 / 序列化 / 类型 / 编译 / 单元测试 → 自动；平台剪贴板 API 行为、HTTP/3 端到端连通、大文件性能、回环检测的"实际场景"、GUI 交互 → 必须人类在真机手动跑。
>
> "自动"列里每一条**都必须**在合并前由 AI 跑通并贴日志；"人类"列里每一条**至少一次**由人类在真机记录结果（截图 / 录屏 / 一句话结论）。

### M0a / M0b / M0c — ProtoEvent + HTTP/3 基础

| 类型 | 测试项 | 通过标志 | 对应 STEP |
|---|---|---|---|
| 自动 | ProtoEvent 新变体 encode/decode round-trip（含变长路径） | 全绿 | 0.1 |
| 自动 | 旧 Ping / Pong / Hello 仍兼容（不变长） | 单测绿 | 0.1 |
| 自动 | `cargo build -p lan-mouse --features http3` 编译通过 | 0 error | 0.2 |
| 自动 | h3 server `/healthz` 单测（mock bi_stream） | 200 OK | 0.3 |
| 自动 | h3 server `/clipboard/text/abc` 单测 → 404 | 404 | 0.3 |
| 自动 | h3 client GET helper 单测（mock server） | 拿到正确 bytes | 0.4 |
| 自动 | StreamC reader 启动（`streams::read_loop` 内 `spawn` c reader） | 编译期断言 | 0.5 |
| 自动 | `Stream C is M2-only` 警告（`session.rs:582`）消失 | grep 无匹配 | 0.5 |
| 自动 | `lan-mouse-ipc::ClipboardConfig` serde round-trip（缺字段 = default） | 单测绿 | 0.6 |
| 自动 | `FrontendEvent::ClipboardState` serde round-trip | 单测绿 | 0.6 |
| 自动 | `cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` | 无 diff / 无 warning | 0.7 |
| 自动 | macOS / Windows / Linux 三平台编译通过 | CI matrix 全绿 | 0.7 |
| **人类** | macOS 真机：`curl --http3 https://<peer>:4252/.../healthz` 返回 200 + "ok" | curl 输出 + 截图 | 0.7 |
| **人类** | Windows 真机：同上 | 同上 | 0.7 |
| **人类** | Linux 真机：同上 | 同上 | 0.7 |

### M1a — 剪贴板文本（小文本）

| 类型 | 测试项 | 通过标志 | 对应 STEP |
|---|---|---|---|
| 自动 | `ClipboardBackend` trait + dummy 实现编译 | 0 error | 1a.1 |
| 自动 | macOS backend：changeCount 轮询 + `NSPasteboard.general().string(forType: .string)` 单测（注入 mock） | 单测绿 | 1a.2 |
| 自动 | Windows backend：OpenClipboard / GetClipboardData / SetClipboardData 单测（`windows-sys` mock） | 单测绿 | 1a.3 |
| 自动 | Linux backend：`xclip` / `wl-paste` 子进程单测（`tokio::process` mock + 错误处理：缺工具时清晰报错） | 单测绿 | 1a.3 |
| 自动 | `service::clipboard_dispatcher` 小文本走 StreamC 派发单测 | 单测绿 | 1a.4 |
| 自动 | `cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` | 无 diff / 无 warning | 1a.5 |
| 自动 | 三平台编译通过 | CI matrix 全绿 | 1a.5 |
| **人类** | macOS ↔ macOS 真机对：终端 `echo hello | pbcopy` → 对端 `pbpaste` 拿到 "hello" | 录屏 / 截图 | 1a.4 / 1a.5 |
| **人类** | Windows ↔ Windows 真机对：PowerShell `Set-Clipboard -Value "hello"` → 对端 `Get-Clipboard` 拿到 "hello" | 截图 | 1a.3 / 1a.5 |
| **人类** | Linux ↔ Linux 真机对：`xclip -selection clipboard` / `wl-paste` 互传 | 录屏 | 1a.3 / 1a.5 |
| **人类** | macOS ↔ Windows / macOS ↔ Linux / Windows ↔ Linux 跨平台互传小文本 | 3 组真机各跑一次 | 1a.5 |

### M1b — 大文本 + 防回环

| 类型 | 测试项 | 通过标志 | 对应 STEP |
|---|---|---|---|
| 自动 | `ClipboardText` 拆为"内联 + 元数据"路径单测：1 KiB / 100 KiB / 1 MiB 走不同分支 | 单测绿 | 1b.1 |
| 自动 | HTTP/3 server `/clipboard/text/{sha256}` 单测（mock cache） | 200 + 正确 bytes | 1b.2 |
| 自动 | HTTP/3 client `get_text` 单测（mock server） | 拿到正确字符串 | 1b.2 |
| 自动 | 回环 LRU 单测：本地写 "abc" → 收到对端 "abc" → skip；TTL 过期后重新同步 | 单测绿 | 1b.3 |
| 自动 | `cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` | 无 diff / 无 warning | 1b.4 |
| **人类** | macOS ↔ macOS 1 MiB 文本：`head -c 1048576 /dev/urandom | base64 | pbcopy` → 对端 `pbpaste > /tmp/back.txt`；`diff` 为 0；`sha256sum` 一致 | diff 输出 + sha256 | 1b.1 / 1b.4 |
| **人类** | Windows ↔ Windows 1 MiB 文本同上 | 同上 | 1b.4 |
| **人类** | Linux ↔ Linux 1 MiB 文本同上 | 同上 | 1b.4 |
| **人类** | 回环测试：本端 `pbcopy "x"` → 不应触发对端再推回（本端剪贴板不抖动） | 录屏看剪贴板历史 | 1b.3 / 1b.4 |
| **人类** | 同内容重复复制（< 1 s 内 5 次）：只触发一次同步 | 日志 + UI 状态 | 1b.3 / 1b.4 |

### M2a — 剪贴板图片 + macOS

| 类型 | 测试项 | 通过标志 | 对应 STEP |
|---|---|---|---|
| 自动 | mime 检测单测：PNG `89 50 4E 47` / JPG `FF D8 FF` / BMP `42 4D` / 非图片 | 单测绿 | 2a.1 |
| 自动 | `ClipboardBackend::current_image` / `set_image` 单测（mock） | 单测绿 | 2a.1 |
| 自动 | macOS `NSPasteboard.general().data(forType: .png)` 单测 | 单测绿 | 2a.2 |
| 自动 | 图片 outbound sha256 计算 + StreamC 派发单测 | 单测绿 | 2a.3 |
| 自动 | HTTP/3 server `/clipboard/image/{sha256}` 单测 | 200 + bytes | 2a.3 |
| 自动 | 图片回环 LRU 单测（同 fingerprint skip） | 单测绿 | 2a.4 |
| 自动 | `cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` | 无 diff / 无 warning | 2a.4 |
| **人类** | macOS 真机：`screencapture -x -t png /tmp/4k.png`（4K）→ Finder 复制 / Cmd+C → 对端粘贴为 PNG 字节级一致 | `xxd | sha256sum` 对比 | 2a.2 / 2a.3 / 2a.4 |
| **人类** | macOS 1080p JPG 截图同上 | 同上 | 2a.3 |
| **人类** | 图片回环：复制 4K 截图后本端不抖动（不再触发对端重推） | 录屏 | 2a.4 |

### M2b — 剪贴板图片 + Windows / Linux

| 类型 | 测试项 | 通过标志 | 对应 STEP |
|---|---|---|---|
| 自动 | Windows `CF_BITMAPINFO` → PNG bytes（`image` crate）单测 | 单测绿 | 2b.1 |
| 自动 | Linux `xclip` / `wl-paste` 子进程单测（含错误处理） | 单测绿 | 2b.2 |
| 自动 | Wayland / X11 自动探测单测（`WAYLAND_DISPLAY` env / `DISPLAY` env） | 单测绿 | 2b.2 |
| 自动 | `cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` | 无 diff / 无 warning | 2b.4 |
| 自动 | 三平台编译通过 | CI matrix 全绿 | 2b.4 |
| **人类** | Windows 真机：Snipping Tool 截 4K → 复制 → 对端（macOS）粘贴字节级一致（**走 CF_DIBV5 直传 + macOS DIB 解码，mime=application/x-dib**） | `xxd | sha256sum` | 2b.1 / 2b.3 |
| **人类** | Windows 截 1080p JPG → 对端（macOS）粘贴字节级一致 | 同上 | 2b.3 |
| **人类** | Linux 真机：GNOME / KDE 截图 → 对端（macOS / Windows）粘贴字节级一致 | 同上 | 2b.2 / 2b.3 |
| **人类** | macOS ↔ Windows / macOS ↔ Linux / Windows ↔ Linux 三组互传 4K + 1080p JPG（共 6 组） | 6 组真机互测，**macOS↔Windows 走 DIB 字节级一致，其它组走 PNG** | 2b.3 |

### M3a — 复制文件 + HTTP/3 transfer

| 类型 | 测试项 | 通过标志 | 对应 STEP |
|---|---|---|---|
| 自动 | `collect_files` 单测：1 KiB / 1 MiB / 200 MiB sha256 正确 | sha256sum 对比 | 3a.1 |
| 自动 | 单文件 > 4 GiB 警告 + 拒绝目录 | 单测绿 | 3a.1 |
| 自动 | `ClipboardFiles` outbound 单测（mock platform clipboard 含文件 URI） | 单测绿 | 3a.2 |
| 自动 | HTTP/3 server `/clipboard/file/{sha256}` 流式返回单测 | 200 + 字节流 | 3a.4 |
| 自动 | HTTP/3 server range 请求 stub 单测（`?range=0-99`） | 200 + 前 100 字节 | 3a.4 |
| 自动 | HTTP/3 client `get_file` 流式下载单测（不一次性分配 200 MiB） | 单测绿（profile 内存 < 50 MiB） | 3a.3 |
| 自动 | `FileTransferCancel` 取消流程单测：源端 cancel → 接收端 1 s 内停止 + 清 .partial | 单测绿 | 3a.5 |
| 自动 | `cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` | 无 diff / 无 warning | 3a.5 |
| **人类** | macOS 真机：Finder 复制 200 MiB 随机文件 → 对端（macOS）落盘 `/tmp/received/` → `sha256sum` 一致 | sha256sum 输出 | 3a.2 / 3a.3 / 3a.5 |
| **人类** | 200 MiB 传输 100 Mbps LAN < 30 s（计时） | 秒表 + 截图 | 3a.3 / 3a.5 |
| **人类** | 源端复制文件后立即覆盖剪贴板 → 接收端不应下载（cancel 触发） | 日志 + 文件系统 | 3a.5 |
| **人类** | 1 KiB / 1 MiB / 200 MiB 三种大小各跑一次 | 3 组真机 | 3a.1 / 3a.5 |

### M3b — 文件接收 UI + 取消 + 中途拔网

| 类型 | 测试项 | 通过标志 | 对应 STEP |
|---|---|---|---|
| 自动 | `ClipboardConfig { auto_accept_files, accept_dir, ignore_files }` serde round-trip | 单测绿 | 3b.1 |
| 自动 | `FileTransferRequest` / `RespondFileTransfer` IPC 单测 | 单测绿 | 3b.1 |
| 自动 | 接收端询问流程单测：mock IPC 收到 `FileTransferRequest` → 等响应 → accept / reject | 单测绿 | 3b.2 |
| 自动 | 拔网处理单测：mock HTTP/3 stream error → IPC 推 `FileTransferFailed` + 清 .partial | 单测绿 | 3b.3 |
| 自动 | `cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` | 无 diff / 无 warning | 3b.4 |
| **人类** | 三平台真机 GUI：复制 200 MiB 文件 → Toaster 弹通知 → Accept 落盘 + SHA-256 一致 | 录屏 + sha256sum | 3b.2 / 3b.4 |
| **人类** | 复制 200 MiB 文件 → Toaster 弹通知 → Reject → 源端收到 cancel + 不再下载 | 日志 | 3b.2 |
| **人类** | 复制 200 MiB 文件 → 接收中拔网线 / 关对端 Wi-Fi → 5 s 内 GUI 看到"传输失败：connection lost" | 录屏 + 错误信息 | 3b.3 / 3b.4 |
| **人类** | 200 MiB 性能：100 Mbps **有线** LAN 实测 < 30 s | 秒表 | 3b.4 |
| **人类** | 200 MiB 性能：Wi-Fi 实测 < 60 s（**评审 #5 3rd 双档**） | 秒表 | 3b.4 |

### M4 — GUI 集成 + 文档

| 类型 | 测试项 | 通过标志 | 对应 STEP |
|---|---|---|---|
| 自动 | Vue `api/ipc.ts` / `store/index.ts` 单测：mock `ClipboardState` → state 更新 | 单测绿 | 4.1 |
| 自动 | ConnectionsPanel vitest snapshot：clipboard 区块（auto_accept_files / accept_dir 文本框） | snapshot 稳定 | 4.2 |
| 自动 | Toaster vitest：`FileTransferRequest` 通知 + Accept / Reject 按钮 | snapshot 稳定 | 4.3 |
| 自动 | `src/config.rs` TOML `clipboard` 段 round-trip 单测 | 单测绿 | 4.2 |
| 自动 | `cd lan-mouse-vue && pnpm build` 产物 OK | 0 error | 4.5 |
| 自动 | `cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` | 无 diff / 无 warning | 4.5 |
| 自动 | 三平台编译通过 | CI matrix 全绿 | 4.5 |
| **人类** | macOS 真机：浏览器打开 GUI（`pnpm dev`）→ ConnectionsPanel 改 `Auto-accept files` → 立即生效（config.toml 落盘） | 截图 + config.toml diff | 4.2 / 4.5 |
| **人类** | Windows 真机：同上 | 同上 | 4.5 |
| **人类** | Linux 真机：同上 | 同上 | 4.5 |
| **人类** | GeneralPanel 剪贴板状态卡片：复制文本后 1 s 内 UI 显示最近时间 + 前 80 字符预览 | 录屏 | 4.4 |
| **人类** | Toaster 文件传输通知：三平台各跑一次（accept + reject） | 录屏 | 4.3 / 4.5 |
| **人类** | README.md / DOC.md 阅读一遍，确认"跨设备剪贴板"+"文件同步"章节描述准确 | 自审 | 4.5 |

### 不可自动化 / 必须人为判断的项

1. **平台剪贴板 API 行为差异**：macOS `changeCount` 跳变、Windows `CF_BITMAPINFO` 颜色深度、Linux Wayland `wlr-data-control` 协议版本——AI 只能 mock。
2. **200 MiB 真实网络性能**：受硬盘 I/O、Wi-Fi 信号、CPU 影响，AI 单测只能给上限。
3. **GUI 体验**：Toaster 通知的位置 / 时长 / 关闭动画是否顺滑是主观判断。
4. **跨 DPI 显示器**：mixed scale factor 下剪贴板 / 文件同步不受影响（数据不涉及屏幕坐标），但显示密度可能让 UI 错位。
5. **macOS TCC 权限**：Accessibility / Input Monitoring 重启后需重授权，必须人在机器前。
6. **剪贴板历史抖动**：回环检测在极端时序下是否漏检，只有长时间运行 + 日志观察能确认。

### 测试工具与脚本建议

- **真机回归模板**：`tests/manual/clipboard-text.md` / `clipboard-image.md` / `file-transfer.md` 模板（"在 macOS 14 + Windows 11 对端下：1. 启动 daemon；2. 终端 pbcopy 'hello'；3. 对端 pbpaste 应该是 hello"），人类按模板逐项打勾。
- **协议 round-trip**：`cargo test -p lan-mouse-proto` 包含所有 ProtoEvent 变体。
- **HTTP/3 端到端**：`tests/quic_smoke.rs` 扩展为 `tests/http3_smoke.rs`，mock 一个 server + client 跑通 `/healthz` + `/clipboard/{text,image,file}/...`。
- **WebSocket 事件录制**：开发期 `RUST_LOG=lan_mouse_service=trace,lan_mouse_quic_transport=trace`，console 输出存 `tests/manual/<date>-<machine>.log`。
- **大文件性能**：`scripts/bench-file-transfer.sh` —— 跑 `dd` + `sha256sum` + 计时 + 错误检测，一键回归。

---

> **本文档定稿时间**：2026-09-06
> **作者**：Claude（基于用户 `REQUIREMENT.md` + `TECHNOLOGY.md` + 现有 codebase 调查）
> **下一步**：用户审核本计划 → 确认 / 调整 → 启动 M0a
