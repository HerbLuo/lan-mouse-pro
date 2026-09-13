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
- **bump `lan-mouse-proto` 到 0.4.0**（评审 #1）：新增 7 个 `ProtoEvent` 变体（`ClipboardText` / `ClipboardImage` / `ClipboardFiles` / `ClipboardRequest` / `FileTransferOffer` / `FileTransferResponse` / `FileTransferCancel`），是主版本变更。**注**：`FileTransferOffer` / `FileTransferResponse` 已在 M0a 落地但**M3b 不再使用**（auto-accept only，不需要 offer/response 流程；用户决策 2026-09-13）—— proto 层保留作为 forward compat，未来如需询问 UI 可启用
- **wire-level 兼容策略（评审 #1）**：剪贴板 / 文件相关事件**全部**走 StreamC；旧 daemon（无 StreamC reader）的 `streams::read_loop`（参考 `streams.rs:291` 的"M2-only"预留位）已**默默 drop** `stream_bunch.c`，不会触发 stream A decode error → 新 / 旧 daemon 在 stream A 上仍能正常键鼠互通，仅剪贴板 / 文件功能单向有效（旧的收不到、新的不会推错地方）。**不**走 stream A 编码新 EventType → 不会让旧 daemon 触发 `EventType::try_from(InvalidEventId)` 断链
- 现有 `quic_transport` 的 StreamC 通道从"占位"升级为可用
- 在 `quinn::Connection` 上叠加 HTTP/3 端到端服务（h3 crate 或自实现 HTTP/3-lite，见 M0b STEP 0.2 spike），用于**大文本 / 图片字节 / 文件字节**的拉取
- 输入通道模式 `InputChannelConfig` 已在 `PLAN-1` 中引入；本计划**不**改动

**业务功能**
- 剪贴板文本同步（含大小文本；fingerprint 防回环）—— **双向：A→B 与 B→A 各跑一次才算通过**
- 剪贴板图片同步（PNG / JPG / BMP，SHA-256 校验）—— **双向：A→B 与 B→A 各跑一次才算通过**
- 复制文件同步（HTTP/3 拉取 + SHA-256 + auto-accept + 源端取消 + 拔网清晰报错）—— **双向：A→B 与 B→A 各跑一次才算通过**

**前端**
- **ClipboardConfig 放 GeneralPanel**（daemon-global，评审 #4）：剪贴板监听是 daemon 全局（一个 OS 剪贴板喂所有 peer），接收目录也是全局 → `{ enabled, accept_dir, ignore_text, ignore_images, ignore_files, max_file_size, keep_partial }` 放 GeneralPanel（**drop** `auto_accept_files` —— auto-accept 是本计划唯一模式，不需要用户决策；用户决策 2026-09-13）
- **per-peer `enable_clipboard_to: bool`**（评审 #4）：每个 ConnectionRow 加细粒度开关，控制"是否把剪贴板推给这个 peer"，与 `input_channels` per-client 配置不冲突（那是发送端路由选择）
- ConnectionsPanel / GeneralPanel：剪贴板 / 文件接收开关、最近同步状态
- **无 Toaster 接受/拒绝 UI**（用户决策 2026-09-13）：auto-accept only，GUI 是配置入口而非交互入口
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
│  │   └─ FileTransferCancel (source-side cancel)│        │
│  │ HTTP/3 (h3)       ← 新叠加                  │        │
│  │   ├─ GET  /clipboard/text/{sha256}         │        │
│  │   ├─ GET  /clipboard/image/{sha256}        │        │
│  │   ├─ GET  /clipboard/file/{sha256}         │        │
│  │   └─ GET  /clipboard/file/{sha256}?range=… │        │
│  │   (auto-accept：Files 元数据 → 立即 GET 下载；│        │
│  │    无 FileTransferOffer / Response offer-   │        │
│  │    response 流程；用户决策 2026-09-13)        │        │
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
| 文件接收流程（auto-accept only） | StreamC `ClipboardFiles` 元数据 → 接收端立即 `Http3Client::get_file` 下载；`FileTransferFailed` IPC 推 GUI 单方向通知（无 accept/reject UI；用户决策 2026-09-13） | 不需要用户决策 |
| 文件字节传输 | HTTP/3 GET | 200 MiB 不应阻塞其他流 |

**回环防御**（关键）：本端在写剪贴板时**记录** fingerprint（最近 N 条），收到对端推送时若 fingerprint 在"最近写过"集合中 → skip。

## 2. 里程碑路线图

> **时间校准**：沿用 `PLAN-1` 的 1/3 系数（30 min AI ≈ 1.5 h 人类）。
> **拆分原则**：任何里程碑 AI 估时 > 12 h 必拆。

| 里程碑 | AI 估时 | 人类可测功能 | 状态 |
|---|---|---|---|
| M0a — ProtoEvent codec 双轨化 | ~3 h | `cargo test -p lan-mouse-proto` 全绿；Input/Ping/Pong/Hello 旧路径零行为差异 | ✅ 完成 |
| M0b — HTTP/3 基础 | ~4.5 h | 真机 `curl --http3 https://<peer>/healthz` 200 | ✅ 完成 |
| M0c — StreamC 接通 + IPC 扩展 | ~4 h | StreamC reader 接通；GUI 看到 `ClipboardConfig` 字段；fmt/clippy/build 全绿 | ✅ 完成 |
| M1a — 剪贴板文本 ≤ 1 KiB | ~6 h | 复制一段短文本 → 对端剪贴板出现；**双向端到端 (A↔B)** | ✅ 完成 |
| M1b — 剪贴板文本 > 1 KiB + 防回环 | ~6 h | 复制 1 MiB 文本 → 对端粘贴成功；同端写回不再回环；**双向端到端 (A↔B)** | ✅ 完成 |
| M2a — 剪贴板图片 + macOS | ~6 h | macOS 复制截图 → 对端粘贴为 PNG / DIB 字节级一致；**双向端到端 (A↔B)** | ✅ 完成 |
| M2b — 剪贴板图片 + Windows / Linux | ~6 h | 三平台图片剪贴板端到端（macOS↔Windows 走 DIB）；**双向端到端 (A↔B)** | ✅ 完成 |
| **M3a** — 复制文件 + HTTP/3 transfer | ~9 h | 200 MiB SHA-256 一致 + 取消响应 + popup 模块 + 50 MiB 早拒绝 | ✅ 完成 |
| **M3b** — 文件接收（auto-accept）+ GUI 配置 + CLI + 剪贴板回灌 | **~11.5 h** | 100 Mbps LAN < 30 s + 拔网清晰报错 + GUI enabled/accept_dir/max_file_size/inject_to_clipboard 配置 + CLI 子命令 + **`set_files` trait 三平台落地** + **文件落盘后自动入剪贴板** | ⏸️ 用户验证 M3a 后启动 |
| **~~M4~~** | ~~5 h~~ | (合并到 M3b) | (cancelled) |
| **合计** | **~52 h** | (was ~55.5 h) | |

> **STEP 时长门**：每 STEP 控制在人类实现 1.5 h（AI ~30 min）左右；超过 3 h 必拆。
> **M3 已知范围扩大风险**：若 M3a 估时实际超 10 h，按"接收端落盘 + 200 MiB 性能"与"UI / 取消 / 拔网"二分拆为 M3a' / M3a''。
>
> **双向验收约定**：M1a / M1b / M2a / M2b / M3a / M3b **要求 A→B 与 B→A 各跑一次才算交付**（M4 已合并到 M3b 并 cancelled）。协议层 StreamC 是 bidi 的、两侧 daemon 镜像运行，理论上天然双向；但只测一侧会留下"反向静默失败"的隐性 bug（如最近 controlled→master 文本同步方向缺失）。测试矩阵（§8）的对应条目已显式拆为 (a) A→B 与 (b) B→A 两条。性能 / 拔网 / UI 验收同样在两方向各跑一次，分别记录结果。M0a / M0b / M0c 为协议 / 基础设施里程碑，不涉及端到端方向性，故 §2 主表对应行无"双向"标注。

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
| 0.6 | 0.5h | IPC 扩展（评审 #4 拆分）：<br>**daemon-global**：`lan_mouse_ipc::ClipboardConfig { auto_accept_files: bool, accept_dir: Option<PathBuf>, ignore_text: bool, ignore_images: bool, ignore_files: bool }`（`#[serde(default)]` 兼容旧 wire）；放 `lan_mouse_ipc::ClipboardConfig` 顶层，不挂在 `ClientConfig` 下；`FrontendRequest::SetClipboardConfig(ClipboardConfig)`（无 handle）<br>**per-peer**：`ClientConfig.enable_clipboard_to: bool`（`#[serde(default)]` 缺字段 = true 向后兼容）；`FrontendRequest::SetEnableClipboardTo(ClientHandle, bool)`<br>事件：`FrontendEvent::ClipboardState { last_text_ts, last_image_ts, last_file_ts, last_source: Option<String> }`（`last_source` 评审 #6 用来高亮"刚被 X 改了"）<br>**注**（2026-09-13 用户决策）：STEP-3b.1 将**替换** ClipboardConfig 结构 —— `auto_accept_files` → `enabled` + `keep_partial`；`accept_dir: Option<PathBuf>` → `accept_dir: PathBuf`（必填）；新增 `max_file_size: u64`。`ClientConfig.enable_clipboard_to` / `ClipboardState.last_source` 字段保留。M0c 阶段字段命名仅供 wire-level 兼容，**STEP-3b.1 落地后上述旧字段移除**。 | `lan-mouse-ipc/src/lib.rs` | serde round-trip 单测：ClipboardConfig 缺字段 = default；`ClientConfig` 旧 wire 加载后 `enable_clipboard_to = true` |
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
| 3a.3 | 1.5h | 接收端 inbound：收到 `ClipboardFiles` → 读 `config.clipboard_config().max_file_size`（默认 50 MiB，0 = 不限；详见 §5 风险 #25） → 逐个 `Http3Client::get_file(sha256)` → 落 `<accept_dir>/<name>`（同名加 `(1)`、`(2)` 后缀） → 重新算 sha256 校验 | `src/service.rs`、`src/quic_transport/http3.rs`（`/clipboard/file/{sha256}` 路由） | 200 MiB 文件对端落盘 + sha256sum 一致 |
| 3a.4 | 1.5h | HTTP/3 server 文件字节流：`/clipboard/file/{sha256}` 从 `file_cache` 读 → 流式返回（不一次性 `Vec::with_capacity(200 MiB)`）；range 请求支持（`?range=0-1023`，为后续断点续传留接口，本计划只 stub 200 OK） | `src/quic_transport/http3.rs` | 单测：mock 200 MiB 文件 → range=0-99 拿到前 100 字节；后续断点续传 plan 留接口 |
| 3a.5 | 1.5h | 取消机制：源端 `Ctrl+C` 或剪贴板被新内容覆盖 → 发 `FileTransferCancel { sha256 }` 走 StreamC；接收端若正在下载 → 关闭 HTTP/3 stream；源端 `file_cache` 删除对应 sha256 | `lan-mouse-proto/src/lib.rs`（已加）、`src/service.rs` | 单测：源端 cancel → 接收端在 1 s 内停止下载并清空 .partial 文件 |

**M3a 里程碑交付**：
- 200 MiB 文件传输端到端通（SHA-256 一致）
- 源端取消接收端能响应
- HTTP/3 流式传输不爆内存

**M3a 已知限制**：
- **本 STEP 假定 `enabled = true` + auto-accept**（无 GUI），用户会看到"文件无声落到 accept_dir"；M3b STEP-3b.5 加 GUI 配置入口（auto-accept 是本计划唯一模式，**不**引入询问 UI；详见用户决策 2026-09-13）
- 断点续传仅 stub；中途拔网后"清晰报错"而非"重传"在 M3b STEP-3b.2 阶段验证

---

### M3b — 文件接收（auto-accept）+ GUI 配置 + CLI

**目标**：M4 GUI 集成 + M3b 收尾**合并**到 M3b —— 落地文件接收的 auto-accept only 模式 + GUI 配置入口 + CLI 子命令 + 拔网清晰报错 + 端到端性能验证 + **接收端剪贴板回灌**（文件落盘后自动入剪贴板）。
**AI 估时**：~11.5 h
**依赖**：M3a

**用户决策（2026-09-13）**：
1. **Auto-accept only**（no dual-mode）—— GUI 是配置入口，**不是**交互入口；Toaster accept/reject buttons **out of scope**
2. M3b 拆为 8 个 STEP（3b.1 - 3b.7b）：原 M3b 4 STEPs（3b.1-3b.4）+ 原 M4 净保留 2 STEPs（4.2 → 3b.5、4.6 → 3b.6）+ drop M4 STEP-4.3/4.4/4.5（auto-accept only + 用户砍掉可观察卡片 / 文档）+ 原 M4 STEP-4.1 与 M3b STEP-3b.4 合并 + 用户 2026-09-13 新增 3b.7a/3b.7b（剪贴板回灌，planer round 2 拆步）；Toaster UI / 可观察性卡片 / 文档全部砍掉
3. **2026-09-13 新增 STEP-3b.7a / 3b.7b**：用户决策加"接收端剪贴板回灌"——文件落盘后自动把 path 灌回本地剪贴板，用户可直接 Cmd+V 粘贴。**2026-09-13 审阅拆步**（planer sub-agent 确认 `set_files` trait method 在 M3a 未落地 — commit `69ebd9a` 只实现了 `current_files()` + `watch_files()` 只读路径；Linux `src/clipboard/linux.rs:392-394` 明确标注 "No file-write path on Linux: M3a only needs the **read** path; `set_files` is out of scope"），原 STEP-3b.7 拆分为 3b.7a（trait `set_files` + macOS/Windows/Linux 三平台实现）/ 3b.7b（skip conditions + 防回环 + IPC serde + GUI checkbox + TOML 集成）

**人类准备**：
- **配置接收目录**：在 config.toml 或 GUI 设置 `accept_dir = "/Users/me/Downloads/lan-mouse"`
- **中途拔网测试**：传输 200 MiB 过程中拔网线 / 关闭对端 Wi-Fi，观察 GUI 报错信息
- **大文件准备**：同 M3a（200 MiB 随机文件）
- **GUI 真机**：浏览器打开 GUI，`pnpm dev` 起 Vite；WebSocket console 可见

| STEP | 估时 | 任务 | 涉及文件 | 完成标志 |
|---|---|---|---|---|
| **3b.1** | **1.5h** | **IPC `ClipboardConfig` 扩展 + 早拒绝 PopupGuard 串通**（drop `auto_accept_files`）：<br>**结构**：`lan_mouse_ipc::ClipboardConfig { enabled: bool, accept_dir: PathBuf, ignore_text: bool, ignore_images: bool, ignore_files: bool, max_file_size: u64, keep_partial: bool }` —— `accept_dir` 改为**必填**（auto-accept means 始终需要 target，`Option` 移除）；`max_file_size` 默认 50 MiB、0 = 不限；`keep_partial` 默认 false（拔网后默认删除 .partial）<br>**drop**：`auto_accept_files: bool`（auto-accept 是本计划唯一模式，不再需要 user toggle）<br>**保留**：`enabled: bool`（master toggle，区分"全局剪贴板监听开关"—— `enabled` 是 master on/off，与 `ignore_files: bool` per-kind ignore 是不同关注点）；`enabled = false` 时整个 dispatcher 不启，不影响键鼠<br>**wire-level 文件传输成功 + 早拒绝 PopupGuard**（沿用 M3a 已落地逻辑）：`PopupGuard::fire()` 在 ExceedsLimit / Cancel 等场景已落地；本 STEP 不改 popup.rs 主体，只把 max_file_size 默认值从 M3a 硬编码常量（`DEFAULT_MAX_FILE_SIZE = 50 MiB`）切到 `config.clipboard_config().max_file_size`，关闭 SUGGESTION #S-5 / #S-8<br>**移除**：`FrontendEvent::FileTransferRequest` / `FrontendRequest::RespondFileTransfer` —— **Toaster accept/reject 取消后不需要这两个事件** | `lan-mouse-ipc/src/lib.rs`、`src/service.rs`（`set_clipboard_config` handler 接 Service 字段 + max_file_size getter）、`src/config.rs`（TOML `[clipboard]` 段 schema） | serde round-trip 单测：缺字段 = default；`accept_dir: PathBuf` 必填语义；`max_file_size = 0` 时不限；`enabled = false` 时 dispatcher 不启；单测覆盖 drop `auto_accept_files` 兼容性 |
| **3b.2** | **1.5h** | **拔网处理：HTTP/3 客户端 stream error → IPC 推送 `FileTransferFailed`**：<br>**接收端错误路径**：`apply_inbound_files_task` 内 HTTP/3 stream error（`RecvStream` 关闭、`ReadError::ConnectionLost` / `Reset` / `TimedOut`）→ 触发 `service::file_inbound_err(sha256, reason)` handler → IPC 推 `FrontendEvent::FileTransferFailed { sha256: [u8; 32], reason: String, ts_ms: u64 }`；**新增** `FrontendEvent::FileTransferFailed` 在 `lan-mouse-ipc` —— 不引入 accept/reject IPC，只补一个**失败通知**事件<br>**`.partial` 处理**：默认删除（关闭 SUGGESTION P2.3 carry-forward：fsync between write and remove）；`keep_partial: bool` config 控制 —— `keep_partial = true` 时保留 `.partial` 文件供排查，`false` 时 `std::fs::remove_file` + log info<br>**reason 字段内容**：`"connection lost"` / `"timeout"` / `"peer cancelled"` 三类枚举 → `String`（保留扩展空间）；GUI 端展示 raw reason 即可<br>**测试**：单测覆盖三种 error 触发 → IPC event 推送 + .partial 清理 + keep_partial 路径；拔网时序端到端 < 5 s（按 M3a cancel 的 1 s 预算） | `src/service.rs`、`lan-mouse-ipc/src/lib.rs` | 单测：mock HTTP/3 stream error → IPC 收到 `FileTransferFailed`；`keep_partial = false` 时 .partial 已删；`keep_partial = true` 时 .partial 保留 |
| **3b.3** | **1.5h** | **端到端性能 + 收尾（评审 #5 3rd 双档）**：200 MiB 文件传输性能分两档——**有线 100 Mbps LAN < 30 s**（理论 16 s + QUIC 加密/流控/sha256 余量）+ **Wi-Fi 实际带宽（有线 30-50 %） < 60 s**；**drop UI 端到端验收**（用户决策 2026-09-13：auto-accept only，不需要 GUI 交互验证），保留 **cancel 双向 + 拔网双向 + keepalive↔idle race 专项 + Pong 间隔 ≤ 600 ms**<br>**keepalive↔idle race 专项**：模拟 QUIC idle timer 在 200 MiB 传输完成后 5 s 内的 keepalive 行为（评审 #5 风险：传输完成后 5 s 内 idle 是否会断链）；实测：传输完成后 30 s 内连接仍 active；传输完成后 60 s 静默期不应触发 disconnect<br>**Pong 间隔 ≤ 600 ms**：M0c 引入的 Ping/Pong keepalive 间隔不超过 600 ms（保障 idle timer 在 5 s 触发前已收到 Pong）<br>**fmt/clippy/build + 三平台真机双向端到端（人类配合）** | `tests/manual/file-transfer.md` | 有线 < 30 s、Wi-Fi < 60 s；cancel 双向 + 拔网双向 均符合预期；keepalive↔idle race 实测 30 s 内不关链；Pong 实测间隔 ≤ 600 ms；fmt/clippy/build 全绿 |
| **3b.4** | **1.5h** | **Vue 类型 + IPC 绑定**（drop `FileTransferRequest` 类型，保留 `FileTransferFailed`）：<br>`lan-mouse-vue/src/api/ipc.ts` 加：`ClipboardConfig` 类型（与 IPC `ClipboardConfig` 1:1 对应：enabled / accept_dir / ignore_text / ignore_images / ignore_files / max_file_size / keep_partial）+ `ClipboardState` 类型（沿用 M0c：last_text_ts / last_image_ts / last_file_ts / last_source）+ `FileTransferFailed` 类型（sha256 hex string / reason / ts_ms）<br>`lan-mouse-vue/src/store/index.ts` 维护：`state.clipboardConfig: ClipboardConfig`（从 IPC 拉初始值 + 监听 `ClipboardConfigChanged` 回写）+ `state.lastClipboardText` / `state.lastClipboardAt` / `state.lastClipboardSource`（沿用 M0c + M1b）<br>`onMounted` 监听 `ClipboardState` 事件 + `FileTransferFailed` 事件 → store 更新；新增 toast 触发（**注意**：toast 是单方向通知，不需要 actions 按钮）<br>**移除**：`FileTransferRequest` / `RespondFileTransfer` 相关类型 + store 字段（用户决策 2026-09-13） | `lan-mouse-vue/src/api/ipc.ts`、`lan-mouse-vue/src/store/index.ts`、`lan-mouse-vue/src/components/Toaster.vue`（仅增 FileTransferFailed 单方向通知，不扩 actions） | 浏览器 console 看到状态同步；FileTransferFailed 触发 Toaster 单方向通知（无 actions） |
| **3b.5** | **1.5h** | **GeneralPanel + per-peer 配置 + TOML 落盘**（评审 #4 改写）：<br>**GeneralPanel**：加剪贴板区块 — `enabled` checkbox（master toggle）/ `accept_dir` 文本框 + dir-picker（**必填**，无 Option 概念）/ `ignore_text` / `ignore_images` / `ignore_files` 三个 ignore checkbox / `max_file_size` number input（MiB 整数输入 → 后端转 bytes，0 = 不限）/ `keep_partial` checkbox；`onChange` 调 `SetClipboardConfig`（无 handle）<br>**ConnectionRow**：每个 client 行加 `enable_clipboard_to` checkbox（label "Push clipboard to this peer"）；`onChange` 调 `SetEnableClipboardTo(handle, bool)`<br>**TOML 落盘**：`src/config.rs` TOML 加 `[clipboard]` 段（**daemon-global**）：`enabled = true` / `accept_dir = "/Users/me/Downloads/lan-mouse"` / `ignore_text = false` / `ignore_images = false` / `ignore_files = false` / `max_file_size = 52428800`（bytes 整数存，UI 显示 MiB）/ `keep_partial = false`；不挂在 `[[clients]]` 下<br>**per-client TOML 段**：`[[clients]]` 加 `enable_clipboard_to = true` 字段 | `lan-mouse-vue/src/components/GeneralPanel.vue`、`lan-mouse-vue/src/components/ConnectionsPanel.vue`、`src/config.rs`、`src/service.rs`（`set_clipboard_config` handler） | 改 checkbox 立即生效（关闭 SUGGESTION #S-7 + #S-8 + #S-5）；config.toml 落盘正确（顶层 `[clipboard]` + 每个 `[[clients]]` 内 `enable_clipboard_to`）；MiB → bytes 转换单测 |
| **3b.6** | **~1 h** | **CLI 集成（评审 #7）**：`lan-mouse-cli` 加 `SetClipboardConfig` 子命令（与 `SetQuicIdleTimeout` 同模式：发 IPC → daemon 写 TOML → 回 echo）；参数 `--enabled` / `--accept-dir` / `--ignore-text` / `--ignore-images` / `--ignore-files` / `--max-file-size`（MiB 整数，CLI 转 bytes）/ `--keep-partial`；`SetEnableClipboardTo <handle> <bool>` 子命令；与现有 `SetMonitor` 共用同一 dispatch pattern；单测覆盖 IPC 编码（含 drop `auto_accept_files` 后兼容性） | `lan-mouse-cli/src/lib.rs` | `lan-mouse-cli SetClipboardConfig --max-file-size 100 --accept-dir /tmp/recv` 生效；`lan-mouse-cli SetEnableClipboardTo 0 false` 关掉对端 0 的剪贴板推送 |
| **3b.7a** | **~1.5 h** | **`ClipboardBackend::set_files` trait + 三平台实现**（planer 审阅拆步 — 原 STEP-3b.7 的"前提：`set_files` 已落地"系 spec 假设错误，commit `69ebd9a` 仅含只读 `current_files()` + `watch_files()`，无 `set_files`）：<br>**trait 加方法**：`src/clipboard/mod.rs` 在 `ClipboardBackend` trait 加 `fn set_files(&self, files: &[PathBuf])` —— 语义：把一组绝对路径灌入 OS 剪贴板（macOS NSPasteboard `NSFilenamesPboardType` / Windows `CF_HDROP` / Linux X11 `text/uri-list` 或 Wayland 同等 mime），调用方保证 paths 都已落盘且 SHA-256 校验通过；返回 `Result<()>`（失败 log warn，不 panic）<br>**macOS 实现**：`src/clipboard/macos.rs` 用 `NSPasteboard.general().clearContents()` + `writeObjects(&ns_array)` 灌入（**注意类型修正 — planer round 2 审阅**：`writeObjects` 签名是 `fn writeObjects(&self, objects: &NSArray<ProtocolObject<dyn NSPasteboardWriting>>) -> bool`，**不是** `NSArray<NSURL>`；NSURL 通过 `ProtocolObject::from_retained(nsurl)` 包装；objc2-app-kit 0.3.2 已为 NSURL 实现 `extern_conformance!(unsafe impl NSPasteboardWriting for NSURL {});`）。NSArray 构造：`NSArray::from_retained_slice(&[ProtocolObject::from_retained(NSURL::fileURLWithPath(&nsstring))])`<br>**Windows 实现**：`src/clipboard/windows.rs` 用 `OpenClipboard` + `EmptyClipboard` + `SetClipboardData(CF_HDROP, hdrop)` + `GlobalAlloc(GHND, ...)` + `GlobalLock` + `DragQueryFileW` 构造 DROPFILES 结构（DROPFILES header + 双重 null-terminated file paths；现有 windows.rs 已有 CF_DIBV5 / CF_BITMAP 写入经验可直接复用）<br>**Linux 实现**：`src/clipboard/linux.rs` —— X11 走 `xclip -selection clipboard -t text/uri-list -i`（子进程 `tokio::process`，stdin 写入 RFC 2483 URI list），Wayland 走 `wl-copy --type text/uri-list < file`；删掉"no file-write path on Linux"注释<br>**RFC 2483 URI list 格式细节**（**planer round 2 审阅补 — 参考 `src/clipboard/linux.rs:398` `wl-paste --type text/uri-list` 读取路径对称**）：`<file:///abs/path1>\r\n<file:///abs/path2>\r\n`，每行一个 URI，CRLF 结尾，多 URI 间 CRLF 分隔；以 `file://` 前缀 + 绝对路径编码；构造函数：`fn build_uri_list(paths: &[PathBuf]) -> String { paths.iter().map(|p| format!("file://{}\r\n", p.display())).collect() }`<br>**测试**：<br>• Windows 单测：mock `SetClipboardData(CF_HDROP, ...)` → 参数捕获 + DROPFILES bytes 解析验证（`DragQueryFileW` 解码对比原 paths）<br>• Linux 单测：mock `xclip` / `wl-copy` 子进程 → 拦截 `Command::spawn` 后断言 args 含 `-t text/uri-list` + stdin payload 含预期 CRLF URI list<br>• macOS 单测（**planer round 2 审阅补**：objc2 AppKit 难以纯 mock；**退化为集成式真实 pasteboard 写入断言** —— 调 `set_files` 前后比对 `NSPasteboard.general().changeCount()` 递增 + `readObjectsForClasses([NSURL.self], options: nil)` 拿到原 paths；与现有 `src/clipboard/macos.rs:1327, 1347` 真实 pasteboard 单测模式一致） | `src/clipboard/mod.rs`（trait 加方法）、`src/clipboard/macos.rs`、`src/clipboard/windows.rs`、`src/clipboard/linux.rs`（删 "out of scope" 注释 + 实现） | `cargo build -p lan-mouse --features <platform>` 编译通过；Windows / Linux 单测验证 mock 平台 API 被调用一次 + 参数正确；macOS 单测验证 changeCount 递增 + round-trip 读回 paths |
| **3b.7b** | **~1.5 h** | **接收端剪贴板回灌 + skip conditions + 防回环 + IPC 集成**（依赖 3b.7a）：<br>**调用点 + 时序**（**planer round 2 审阅修正 — 实际应在 `handle_inbound_files_applied`，不在 `handle_clipboard_inbound_files`**）：M3a 已落地的 `apply_inbound_files_task`（`src/service.rs:5678`）每 entry 独立 spawned；每 entry 落盘完成后通过 `InboundFileApplyResult { fingerprint, sha256, landed_path, error }` mpsc 回到主 `select!` 的 `handle_inbound_files_applied`（`src/service.rs:3762`）——**只有这里持有 `landed_path`**。<br>→ 本 STEP 真正的回灌代码插点：**collector 在 `handle_inbound_files_applied`** 累积 `HashMap<[u8; 32], Vec<(FileEntry, PathBuf)>>`（per-fingerprint 的 landed_path 列表）；当某 fingerprint 的**全部** entry 都收齐（应用层用 `expected_entry_count` 判断）时：<br>1. **pre-stamp 防回环**：`self.last_outbound_files_fingerprint.insert(fingerprint)` 先于 `set_files`（参考 commit `d6fb1d8` 的 ExceedsLimit arm pre-stamp 修复 — 防止本地 poller watcher 在 `set_files` 后立刻触发 `dispatch_files` 重广播给原对端形成死循环）<br>2. **skip conditions 检查**（任一命中即跳过 `set_files`）：<br>   a. `inject_to_clipboard=false` → 跳过（用户主动关）<br>   b. `last_outbound_files_fingerprint` **在 pre-stamp 之前查询**已命中（用户在本地刚复制过同 selection） → 跳过<br>   c. 任意 entry `error != None`（sha256 mismatch / IO error / `keep_partial=true` 残留 .partial） → 跳过整个 batch（不注入未验证 bytes）<br>   d. **forward-compat 跳过**（保留位置但当前 dispatch 不可达，详见注①）：`MIME_TOO_LARGE` / `ExceedsLimit` / `Canceled` entry → 跳过；当前 `handle_clipboard_inbound_files_decide` 已过滤 `AllMimeTooLarge` / 单 entry `ExceedsLimit`，`FileTransferCancel` 早返回不发 `InboundFileApplyResult`，所以这些 entry **当前不可达 set_files 调用点**；保留 skip 是为未来 dispatch 策略变化（per-entry 决策）兜底<br>3. **set_files**：`backend.set_files(&paths)`，传 collector 累积的 `Vec<PathBuf>`（注意：不是 `&[PathBuf]`，参考 3b.7a trait 签名）<br>**配置开关**：`lan_mouse_ipc::ClipboardConfig` 加 `inject_to_clipboard: bool` 字段（`#[serde(default)]` 默认 `true`）—— 用户可在 GUI 关掉回灌，只落盘不入剪贴板；`src/config.rs` TOML `[clipboard]` 段同步加 `inject_to_clipboard = true` 字段<br>**GUI 控件**：在 `lan-mouse-vue/src/components/GeneralPanel.vue` 的 clipboard 区块加 `inject_to_clipboard` checkbox（**只加这一个 checkbox**，不延展到 M4 砍掉的可观察卡片 —— 用户已确认不延展）<br>**注 ①**：`MIME_TOO_LARGE` / `ExceedsLimit` / `Canceled` 在当前 dispatch 流程中**不可达** `set_files` 调用点（planer round 2 审阅发现）。保留 skip 是 forward-compat；测试矩阵的对应行验证的是 collector 在收到这些 entry 时正确跳过（即便当前 dispatch 路径不触发，仍作为防御性测项保留）。<br>**测试**：单测覆盖 4 个 skip condition + happy path（含 collector 等待全部 entry 就绪）；serde round-trip 单测缺字段 = `true` | `lan-mouse-ipc/src/lib.rs`（`ClipboardConfig.inject_to_clipboard` 字段 + serde round-trip）、`src/service.rs::handle_inbound_files_applied`（collector 累积 + pre-stamp + 调 `set_files`，含 skip condition 分支）、`lan-mouse-vue/src/components/GeneralPanel.vue`（checkbox）、`src/config.rs`（TOML `[clipboard]` 段加字段） | mock collector 验证 pre-stamp + 全部 entry 落盘后 `set_files(&[PathBuf])` 被调用一次（每路径正确）；`inject_to_clipboard=false` 时 `set_files` 不被调用；回环指纹命中时 `set_files` 不被调用；任一 entry 落盘失败时 `set_files` 不被调用；forward-compat `MIME_TOO_LARGE` entry collector 跳过 `set_files`；forward-compat `ExceedsLimit` / `Canceled` entry collector 跳过 `set_files`；IPC serde round-trip：`ClipboardConfig` 缺 `inject_to_clipboard` 字段 = 默认 `true` |

**M3b 里程碑交付**：
- GUI 可配置剪贴板（enabled / accept_dir / max_file_size / ignore_* / keep_partial / inject_to_clipboard）
- per-peer `enable_clipboard_to` 细粒度开关
- 200 MiB 文件端到端（100 Mbps LAN < 30 s / Wi-Fi < 60 s，**双向**）
- 源端取消响应（cancel < 1 s，沿用 M3a STEP-3a.5）
- 拔网清晰报错（`FileTransferFailed` IPC + .partial 默认删除，**双向**）
- keepalive↔idle race 实测（30 s 内不关链 + Pong 间隔 ≤ 600 ms）
- CLI 子命令支持
- **`ClipboardBackend::set_files` trait + macOS / Windows / Linux 三平台实现**（STEP-3b.7a）
- **剪贴板回灌**（STEP-3b.7b）：文件落盘后自动灌回本地剪贴板（`inject_to_clipboard=true` 默认开启），用户可一键 Cmd+V 粘贴，无需手动 navigate 到 `accept_dir`；pre-stamp 防回环 + 4 类 skip condition 覆盖完整

**M3b 已知限制**（原 M3b + M4 已知限制并集）：
- **无 accept/reject UI**（auto-accept only；用户决策 2026-09-13）—— 用户无法在 GUI 拒绝单次文件接收；如需拒绝，关闭 `enabled` 整体开关 或 `ignore_files: bool`
- **无 clipboard state card / amber 高亮 / 回环统计显示**（M4 原 STEP-4.4 砍掉；M1b STEP-1b.3 的 `service::clipboard::metrics` 仍 log，但 UI 不展示）
- **无 README.md / DOC.md 文档章节**（M4 原 STEP-4.5 砍掉；后续 PLAN 补）
- 断点续传仅 stub（沿用 M3a；M3a `?range=` HTTP/3 接口已留）
- **STEP-3b.7b 剪贴板回灌无 UI 提示**（mid-edit 场景下可能覆盖用户当前剪贴板内容；用户决策 2026-09-13：M3b 不做提示，仅行为层面实现；M4 砍掉的可观察卡片如未来恢复可承载"剪贴板刚被远端文件替换"提示）

---

### ~~M4 — GUI 集成 + 文档~~（cancelled，已合并到 M3b）

> **用户决策 2026-09-13**：M4 取消，并入 M3b。理由：auto-accept only 模式不需要 Toaster accept/reject UI；可观察性卡片（clipboard state card / 5 s amber 高亮 / 回环跳过统计）砍掉；README/DOC.md 文档章节砍掉。详见 M3b 已知限制段。
>
> 原 M4 6 STEPs（4.1 / 4.2 / 4.3 / 4.4 / 4.5 / 4.6）→ 拆分并入新 M3b：
> - 原 STEP-4.1（Vue 类型 + IPC 绑定）→ 与新 M3b STEP-3b.4 合并（planer round 2 算式修正 — 同主题不重复编号）
> - 原 STEP-4.2（GeneralPanel + per-peer 配置 + TOML 落盘）→ 新 M3b STEP-3b.5（ClipboardConfig 字段全部更新：drop `auto_accept_files`、新增 `enabled` + `keep_partial`）
> - 原 STEP-4.3（Toaster `FileTransferRequest` 接受/拒绝）→ **DROP**（auto-accept only）
> - 原 STEP-4.4（剪贴板状态卡片 + 5 s amber 高亮 + 回环跳过统计）→ **DROP**（用户决策 2026-09-13）
> - 原 STEP-4.5（README.md / DOC.md / config.toml 文档同步）→ **DROP**（后续 PLAN 补）
> - 原 STEP-4.6（CLI 集成）→ 新 M3b STEP-3b.6
> - **2026-09-13 用户新增**接收端剪贴板回灌 → 新 M3b STEP-3b.7a（`set_files` trait + 三平台实现，planer 审阅拆步）+ STEP-3b.7b（skip conditions + 防回环 + IPC serde + GUI checkbox + TOML 集成；planer round 2 修调用点 — 在 `handle_inbound_files_applied` + collector，不在 `handle_clipboard_inbound_files`）

---

## 4. 总估时汇总

| 里程碑 | AI 估时 | 人类验证需求 | 状态 |
|---|---|---|---|
| M0a | 3h | ProtoEvent 旧路径回归（Ping/Pong/Hello 兼容） | ✅ 完成 |
| M0b | 4.5h | 真机 h3 `curl --http3` 命中 /healthz | ✅ 完成 |
| M0c | 4h | StreamC reader 接通 + IPC ClipboardConfig 字段 | ✅ 完成 |
| M1a | 6h | 三平台小文本复制粘贴 **（双向端到端 A↔B）** | ✅ 完成 |
| M1b | 6h | 1 MiB 文本端到端 + 回环检测 **（双向端到端 A↔B）** | ✅ 完成 |
| M2a | 6h | macOS 4K 截图字节级一致 **（双向端到端 A↔B）** | ✅ 完成 |
| M2b | 6h | 三平台图片互传矩阵 **（双向端到端 A↔B）** | ✅ 完成 |
| M3a | 9h | 200 MiB 文件落盘 + SHA-256 + 取消 **（双向端到端 A↔B）** | ✅ 完成 |
| M3b | 11.5h | auto-accept + GUI 配置 + 拔网清晰报错 + 性能双档 + `set_files` 三平台 + 剪贴板回灌 **（双向端到端 A↔B）** | ⏸️ 用户验证 M3a 后启动 |
| ~~M4~~ | ~~5h~~ | (合并到 M3b，cancelled) | (cancelled) |
| **合计** | **~52 h** | (was ~55.5 h，省 3.5 h 因 M3b/M4 合并去重 + drop Toaster UI / 可观察卡片 / 文档; 3b.7a/b 拆步 +2h 因 `set_files` trait 未在 M3a 落地) | |

> **校准系数**：与 `PLAN-1` 一致，30 min AI ≈ 1.5 h 人类。`image` crate 编译时间 + h3 crate 体积可能拉长 M2 / M0b/M0c，预留 ±20 % buffer。
>
> **M3b 拆分原则**：合并后 M3b 估时 ~11.5 h（贴近 12 h 边界），**已预防性拆 STEP-3b.7 为 3b.7a（trait 适配）/ 3b.7b（skip conditions + 集成）**。若 STEP-3b.3（端到端性能 + 收尾）实测超 1.5 h，按"性能双档 vs. keepalive↔idle race"二分拆为 3b.3a / 3b.3b。若 STEP-3b.7a 单平台实现超 1.5 h AI（macOS / Windows / Linux 中某一特别复杂），LEADER 介入按平台拆 3b.7a-i / 3b.7a-ii / 3b.7a-iii。
>
> **M3a 风险点（已解决）**：HTTP/3 200 MiB 流式传输 + 取消 + 拔网在 M3a STEP-3a.5 整批审后 PASS-with-followup（cancel < 1 s 端到端、quinn stream-finish race 已加 receiver-side cancel signal 兜底），无后续拆分需要。

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
5. **【评审 #5，已审视、未采纳】idle_timeout 默认 5s 关链风险**：评审担心 200 MiB 传输后 5s 即关链。**未采纳理由**：HTTP/3 stream 在 200 MiB 传输期间有持续流量，QUIC idle timer 不触发；传输完成后 daemon 回到键鼠事件，QUIC keep-alive（5s 间隔）应能保活。**未单独改 default**（不在 PLAN-2 范围）。M0c STEP 0.7 / 新 M3b STEP-3b.3 真机测试时**顺手回归**一下"200 MiB 完成后 30s 内不关链"（keepalive↔idle race 专项），异常则进 `next/SUGGESTION.md`。
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
15. **【评审 #4（2nd），已采纳】ClipboardConfig 位置 per-client vs daemon-global**：剪贴板监听是 daemon 全局（一个 OS 剪贴板喂所有 peer），接收目录也是全局。**已采纳**：ClipboardConfig { enabled, accept_dir: PathBuf, ignore_text, ignore_images, ignore_files, max_file_size, keep_partial } 移到 `GeneralPanel` 顶层（daemon-global TOML `[clipboard]` 段；M3b STEP-3b.1 落地，drop `auto_accept_files`，`accept_dir` 改为必填 PathBuf）；per-peer 只留 `ClientConfig.enable_clipboard_to: bool` 细粒度开关（"我是否要把剪贴板推给这个 peer"），与 `input_channels` per-client 配置不冲突。
16. **【评审 #5（2nd），已审视 / 因用户决策 2026-09-13 部分作废】Toaster 需要 accept/reject action**：当前 `Toaster.vue:8-15` 只支持 dismiss。**已审视**：用户决策 2026-09-13 改为 auto-accept only，**Toaster accept/reject UI drop**。原计划扩 `Toast.actions?: Array<{ label, request: FrontendRequest }>` 字段 + 模板渲染 action 按钮 的方案**不再落地**。`FileTransferOffer` / `FileTransferResponse` wire-level 事件已在 M0a 落地但 M3b 不使用（保留作为 forward compat，未来若需询问 UI 可启用）；`FrontendEvent::FileTransferRequest` / `FrontendRequest::RespondFileTransfer` IPC 事件**不**落地（drop）。
17. **【评审 #6（2nd），部分采纳 / 因用户决策 2026-09-13 部分 drop】多源 / 多端同时复制 UI 提示缺失**：REQUIREMENT 接受"最后写者赢"，但 UI 应提示用户。**部分采纳**：`ClipboardState.last_source` / `last_source_at` 字段（M0c 已落地）保留作为 IPC 数据源；5 s 内 amber 高亮 UI **drop**（M4 STEP-4.4 砍掉；用户决策 2026-09-13：可观察性卡片砍掉）。如未来需要 UI 提示，复用 `last_source` 字段即可。
18. **【评审 #7（2nd），已采纳】CLI 集成完全没提**：CLI-only 用户只能手动编辑 TOML。**已采纳**：原 M4 新增 STEP 4.6——`lan-mouse-cli::SetClipboardConfig` / `SetEnableClipboardTo <handle> <bool>` 子命令，与 `SetQuicIdleTimeout` 同模式。**重命名**：M4 合并到 M3b 后，原 STEP-4.6 → 新 M3b STEP-3b.6。

---

**第三轮评审（2026-09-06）补充：**

19. **【评审 #1（3rd），已采纳】HTTP/3-lite spike 范围不足**：原 spike 仅 30+30 行 `GET /healthz`，验不了 200 MiB 流式 + 取消 + 拔网的 race；quinn stream-finish race 在大规模数据下才会暴露。**已采纳**：M0b STEP 0.2 spike **强制跑 200 MiB 端到端含"传输中 cancel"和"传输中拔网"两个场景**；不通过则 M0b 暂停，与用户对齐方案（候选：自实现精简 HTTP/3 over 裸 stream、或放弃 HTTP/3 改用纯 length-prefix 协议）。
20. **【评审 #2（3rd），已采纳】macOS NSPasteboard TIFF-only 来源破字节级一致**：Preview.app 复制选中区域只提供 TIFF，源端 TIFF → 对端字节级一致永远不成立。**已采纳方案 A**：M2a STEP 2a.2 明确"源端强制 PNG 归一化"——`data(forType: .tiff)` 读 TIFF → `image` crate 解码 → **强制重新编码为 PNG** 再传；归一化到 PNG 后所有对端都按 PNG 处理，与 REQUIREMENT §4.3 一致。
21. **【评审 #3（3rd），已采纳】macOS NSImage DIB 解码无 codebase 依据**：NSImage 内部 surface 变换可能改变像素（虽 surface bytes 不变，但 surface → NSImage → NSPasteboard 写回时可能再变换）。**已采纳**：M2b STEP 2b.1 加 spike 测 `NSImage(data: dib_bytes) → rep → setData → 重新读出 → sha256`；**通过** = 字节级一致保真；**失败** = 降级为"视觉一致"路径（macOS 端也走 `image` crate 统一转 PNG），UI 提示"图片已转换格式"。
22. **【评审 #4（3rd），部分采纳】LRU 命中率无运行期信号**：原 plan 说"运行期监控命中率"但没 STEP 落实。**部分采纳**：M1b STEP 1b.3 加 `service::clipboard::metrics { skip_count, allow_count, last_skip_ts }`（`AtomicU64`）+ `RUST_LOG=lan_mouse_service::clipboard=trace` 时每 60 s 打印命中率 — **采纳**。M4 STEP 4.4 GeneralPanel 卡片"过去 1 小时回环跳过 N 次"显示 — **drop**（用户决策 2026-09-13：可观察性卡片砍掉）；metrics 数据仍 log，仅无 UI 展示。
23. **【评审 #5（3rd），已采纳】200 MiB 性能目标紧**：30s 留给 QUIC 加密 + 流控 + sha256 余量约 2x；Wi-Fi 实际带宽只有有线 30-50%。**已采纳**：新 M3b STEP-3b.3（合并 M3b + M4 后，原 M3b STEP 3b.4 拆 "UI 端到端" variant 后重新编号为 3b.3）拆**双档**——"100 Mbps **有线** LAN < 30 s" + "Wi-Fi < 60 s" + keepalive↔idle race 专项 + Pong 间隔 ≤ 600 ms。
24. **【评审 #6（3rd），已采纳】Linux Wayland 缺工具无 fallback**：wl-clipboard 未必预装，Wayland 用户没装时只给"清晰错误"——剪贴板功能完全不可用。**已采纳**：M2b STEP 2b.2 加探测优先级 + fallback——Wayland 缺工具 → 探测 XWayland xclip → 仍缺 → log error "请安装 wl-clipboard 或 xclip"（daemon 继续跑，键鼠不受影响）。

---

**2026-09-13 用户决策补充：**

25. **【用户决策 2026-09-13】50 MiB 默认 max_file_size 上限 + 用户可配置**：M3a STEP-3a.2 评审阶段默认 `DEFAULT_MAX_FILE_SIZE = 50 MiB` 写死为 Service 字段；M3a STEP-3a.3 接收端 inbound 读该常量，> 50 MiB 文件被 ExceedsLimit popup 拒绝。**已采纳**：
   - 新 M3b STEP-3b.1 把 `ClipboardConfig.max_file_size: u64`（默认 50 MiB；0 = 不限）字段从常量切到 `Config::max_file_size()` getter（关闭 SUGGESTION #S-5 + #S-8）
   - 新 M3b STEP-3b.5 在 GUI GeneralPanel 加 `max_file_size` number input（MiB 整数 → 后端转 bytes 整数存 TOML），用户可调至 0（不限）/ 100 MiB / 1 GiB 等任意值
   - **变更影响**：M3a 已落地的 50 MiB 早拒绝 PopupGuard 路径（`PopupKind::ExceedsLimit` → `popup.rs::fire()`）保持不变；只把上限值从 hardcoded 常量改为 config-driven
   - **验收**：M3b STEP-3b.5 改 max_file_size = 100 后，60 MiB 文件应**不再**触发 ExceedsLimit popup，正常落盘（双向 A→B + B→A 各跑一次）

---

26. **【用户决策 2026-09-13】接收端剪贴板回灌可能覆盖用户当前剪贴板内容**：STEP-3b.7b 在文件落盘成功后自动调 `backend.set_files(&[PathBuf])` 灌回本地剪贴板；若用户正在 mid-edit（剪贴板里是临时复制的内容如一段文本 / 一张截图），回灌会**无声覆盖**这些内容。**已审视、采纳**：UI 提示"剪贴板刚被远端文件替换"在 M4 砍掉的可观察卡片范畴内，本计划不承载此提示；用户在 GeneralPanel 可关 `inject_to_clipboard` checkbox 整体关闭回灌。**M3b 不做提示，仅行为层面实现**；如下游真机测试发现 mid-edit 覆盖是高频痛点，再立后续 PLAN 补 GeneralPanel 卡片。

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
- M1b / M2b 完成后与用户对齐下一里程碑；M3a 完成后**必须**用户介入验证（200 MiB 性能 + 取消语义是核心验收点）；**当前活跃里程碑 = M3b**，M3b 完成后**必须**用户介入验证（200 MiB 性能 + 拔网语义 + GUI 配置 + CLI 子命令 是核心验收点）→ 用户对齐下一里程碑 → leader 续约或交班。
- **M4 已合并到 M3b**（用户决策 2026-09-13）：M3b 完成后不存在 M4 阶段；下一里程碑由用户在 M3b 验收后决定（候选：补 M4 cancelled 的 Toaster UI / 可观察性卡片 / 文档章节；或启动后续 PLAN-M3 的断点续传 / 剪贴板历史 / Wayland portal 等）。

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
| **人类** | macOS ↔ macOS 真机对：(a) A→B：A 端 `echo hello | pbcopy`，B 端 `pbpaste` 拿到 "hello" | 录屏 / 截图 | 1a.4 / 1a.5 |
| **人类** | macOS ↔ macOS 真机对：(b) B→A：反过来同样拿（即上一行的反向，证明两侧 daemon 镜像运行） | 录屏 / 截图 | 1a.4 / 1a.5 |
| **人类** | Windows ↔ Windows 真机对：(a) A→B：A 端 PowerShell `Set-Clipboard -Value "hello"`，B 端 `Get-Clipboard` 拿到 "hello" | 截图 | 1a.3 / 1a.5 |
| **人类** | Windows ↔ Windows 真机对：(b) B→A：反过来同样拿 | 截图 | 1a.3 / 1a.5 |
| **人类** | Linux ↔ Linux 真机对：(a) A→B：A 端 `xclip -selection clipboard -i < /tmp/x`（或 `wl-copy`），B 端 `xclip -o`（或 `wl-paste`）拿到 | 录屏 | 1a.3 / 1a.5 |
| **人类** | Linux ↔ Linux 真机对：(b) B→A：反过来同样拿 | 录屏 | 1a.3 / 1a.5 |
| **人类** | macOS ↔ Windows 跨平台：(a) A→B：macOS 端 pbcopy，Windows 端 Get-Clipboard；(b) B→A：Windows 端 Set-Clipboard，macOS 端 pbpaste | 各方向各一次 + 截图 | 1a.5 |
| **人类** | macOS ↔ Linux 跨平台：(a) A→B + (b) B→A 同上 | 各方向各一次 + 录屏 | 1a.5 |
| **人类** | Windows ↔ Linux 跨平台：(a) A→B + (b) B→A 同上 | 各方向各一次 + 录屏 | 1a.5 |

### M1b — 大文本 + 防回环

| 类型 | 测试项 | 通过标志 | 对应 STEP |
|---|---|---|---|
| 自动 | `ClipboardText` 拆为"内联 + 元数据"路径单测：1 KiB / 100 KiB / 1 MiB 走不同分支 | 单测绿 | 1b.1 |
| 自动 | HTTP/3 server `/clipboard/text/{sha256}` 单测（mock cache） | 200 + 正确 bytes | 1b.2 |
| 自动 | HTTP/3 client `get_text` 单测（mock server） | 拿到正确字符串 | 1b.2 |
| 自动 | 回环 LRU 单测：本地写 "abc" → 收到对端 "abc" → skip；TTL 过期后重新同步 | 单测绿 | 1b.3 |
| 自动 | `cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` | 无 diff / 无 warning | 1b.4 |
| **人类** | macOS ↔ macOS 1 MiB 文本：(a) A→B：A 端 `head -c 1048576 /dev/urandom | base64 | pbcopy`，B 端 `pbpaste > /tmp/back.txt`；`diff` 为 0；`sha256sum` 一致 | diff 输出 + sha256 | 1b.1 / 1b.4 |
| **人类** | macOS ↔ macOS 1 MiB 文本：(b) B→A：反过来同样跑一次（验证两侧 daemon 都能作为 HTTP/3 server 暴露 `/clipboard/text/{sha256}`） | diff 输出 + sha256 | 1b.1 / 1b.4 |
| **人类** | Windows ↔ Windows 1 MiB 文本：(a) A→B + (b) B→A 各跑一次 | 同上 | 1b.4 |
| **人类** | Linux ↔ Linux 1 MiB 文本：(a) A→B + (b) B→A 各跑一次 | 同上 | 1b.4 |
| **人类** | 回环测试：(a) A 端 `pbcopy "x"` → B 端不反向推回（B 端剪贴板不抖动）；(b) 反过来 B 端 `pbcopy "y"` → A 端不抖动 | 录屏看剪贴板历史（两个方向各录一段） | 1b.3 / 1b.4 |
| **人类** | 同内容重复复制（< 1 s 内 5 次）：(a) A→B 方向 + (b) B→A 方向各跑一次，只触发一次同步 | 日志 + UI 状态 | 1b.3 / 1b.4 |

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
| **人类** | macOS ↔ macOS 真机 4K PNG：(a) A→B：A 端 `screencapture -x -t png /tmp/4k.png` → Cmd+C，B 端粘贴 → 字节级一致（`xxd | sha256sum` 对比） | sha256sum 输出 | 2a.2 / 2a.3 / 2a.4 |
| **人类** | macOS ↔ macOS 真机 4K PNG：(b) B→A：反过来同样跑一次（验证 macOS daemon 同时作为 HTTP/3 server 暴露 `/clipboard/image/{sha256}` 也能拉回字节） | sha256sum 输出 | 2a.2 / 2a.3 / 2a.4 |
| **人类** | macOS ↔ macOS 真机 1080p JPG：(a) A→B + (b) B→A 各跑一次 | 同上 | 2a.3 |
| **人类** | 图片回环：(a) A→B 复制 4K 截图后 A 端不抖动 + (b) B→A 反向复制后 B 端不抖动 | 录屏（两个方向各录一段） | 2a.4 |

### M2b — 剪贴板图片 + Windows / Linux

| 类型 | 测试项 | 通过标志 | 对应 STEP |
|---|---|---|---|
| 自动 | Windows `CF_BITMAPINFO` → PNG bytes（`image` crate）单测 | 单测绿 | 2b.1 |
| 自动 | Linux `xclip` / `wl-paste` 子进程单测（含错误处理） | 单测绿 | 2b.2 |
| 自动 | Wayland / X11 自动探测单测（`WAYLAND_DISPLAY` env / `DISPLAY` env） | 单测绿 | 2b.2 |
| 自动 | `cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` | 无 diff / 无 warning | 2b.4 |
| 自动 | 三平台编译通过 | CI matrix 全绿 | 2b.4 |
| **人类** | Windows → macOS 4K PNG（DIB 路径）：(a) A→B：Windows 端 Snipping Tool 截 4K → 复制，macOS 端粘贴字节级一致（**走 CF_DIBV5 直传 + macOS DIB 解码，mime=application/x-dib**） | `xxd | sha256sum` | 2b.1 / 2b.3 |
| **人类** | Windows → macOS 4K PNG（DIB 路径）：(b) B→A：macOS 端复制 → Windows 端粘贴字节级一致 | sha256sum 输出 | 2b.1 / 2b.3 |
| **人类** | Windows → macOS 1080p JPG：(a) A→B + (b) B→A 各跑一次 | 同上 | 2b.3 |
| **人类** | Linux → macOS 截图：(a) A→B：Linux 端 GNOME / KDE 截图 → 复制，macOS 端粘贴字节级一致 + (b) B→A：反过来 | 同上 | 2b.2 / 2b.3 |
| **人类** | Linux → Windows 截图：(a) A→B + (b) B→A 同上 | 同上 | 2b.2 / 2b.3 |
| **人类** | macOS ↔ Windows / macOS ↔ Linux / Windows ↔ Linux 三组互传 4K + 1080p JPG（共 6 组，**每组双向（A→B 与 B→A 各跑一次），共 12 次真机测**） | 12 次真机互测，**macOS↔Windows 走 DIB 字节级一致，其它组走 PNG** | 2b.3 |

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
| **人类** | macOS ↔ macOS 真机 200 MiB 文件：(a) A→B：A 端 Finder 复制 200 MiB 随机文件，B 端落盘 `/tmp/received/` → `sha256sum` 一致 | sha256sum 输出 | 3a.2 / 3a.3 / 3a.5 |
| **人类** | macOS ↔ macOS 真机 200 MiB 文件：(b) B→A：反过来同样跑一次（验证两侧 daemon 都能作为 HTTP/3 server 暴露 `/clipboard/file/{sha256}` 流式拉取） | sha256sum 输出 | 3a.2 / 3a.3 / 3a.5 |
| **人类** | 200 MiB 传输 100 Mbps LAN < 30 s（计时）：(a) A→B 方向 + (b) B→A 方向各计时一次 | 秒表 + 截图（两段） | 3a.3 / 3a.5 |
| **人类** | 源端复制文件后立即覆盖剪贴板 → 接收端不应下载（cancel 触发）：(a) A→B 方向 cancel + (b) B→A 方向 cancel 各测一次 | 日志 + 文件系统 | 3a.5 |
| **人类** | 1 KiB / 1 MiB / 200 MiB 三种大小各跑一次（**每种大小跑 A→B 与 B→A 两个方向**） | 3 种 × 2 方向 = 6 组真机 | 3a.1 / 3a.5 |

### M3b — 文件接收（auto-accept）+ GUI 配置 + CLI

| 类型 | 测试项 | 通过标志 | 对应 STEP |
|---|---|---|---|
| 自动 | `ClipboardConfig { enabled, accept_dir: PathBuf, ignore_text, ignore_images, ignore_files, max_file_size, keep_partial }` serde round-trip（**drop** `auto_accept_files`） | 单测绿 | 3b.1 |
| 自动 | `max_file_size = 0` → 不限；`accept_dir` 必填（非 Option）；`enabled = false` → dispatcher 不启 | 单测绿 | 3b.1 |
| 自动 | `Config::max_file_size()` getter 单测：默认 50 MiB；TOML 读 100 MiB 后切到 100 MiB；关闭 SUGGESTION #S-5/#S-8 | 单测绿 | 3b.1 |
| 自动 | `FrontendEvent::FileTransferFailed { sha256, reason, ts_ms }` serde round-trip（**新增**，无 accept/reject 配对） | 单测绿 | 3b.2 |
| 自动 | 拔网处理单测：mock HTTP/3 stream error → IPC 推 `FileTransferFailed` + 默认删 .partial + `keep_partial = true` 时保留 | 单测绿 | 3b.2 |
| 自动 | keepalive↔idle race 单测：传输完成后 30 s 内连接仍 active；60 s 静默期不应 disconnect | 单测绿 | 3b.3 |
| 自动 | Pong 间隔 ≤ 600 ms 计时单测（M0c Ping/Pong keepalive 复用） | 单测绿 | 3b.3 |
| 自动 | Vue `api/ipc.ts` 类型：ClipboardConfig / ClipboardState / FileTransferFailed（**无** FileTransferRequest） | 单测绿 | 3b.4 |
| 自动 | Vue `store/index.ts` 单测：mock `ClipboardState` → state 更新；mock `FileTransferFailed` → toast 单方向通知（无 actions） | 单测绿 | 3b.4 |
| 自动 | GeneralPanel vitest snapshot：clipboard 区块（enabled / accept_dir / ignore_text / ignore_images / ignore_files / max_file_size MiB 输入 / keep_partial）；ConnectionRow `enable_clipboard_to` checkbox | snapshot 稳定 | 3b.5 |
| 自动 | `src/config.rs` TOML `[clipboard]` 段 round-trip 单测（含 `accept_dir` 必填 + bytes 整数存 max_file_size） | 单测绿 | 3b.5 |
| 自动 | MiB → bytes 转换单测（UI 输入 100 MiB → TOML 存 104857600） | 单测绿 | 3b.5 |
| 自动 | `lan-mouse-cli SetClipboardConfig` IPC 编码单测（含 drop `auto_accept_files` 兼容性） | 单测绿 | 3b.6 |
| 自动 | `lan-mouse-cli SetEnableClipboardTo` IPC 编码单测 | 单测绿 | 3b.6 |
| 自动 | `ClipboardBackend::set_files` trait method 编译 + dummy 实现通过 | 单测绿 | 3b.7a |
| 自动 | macOS `set_files` 单测：mock `NSPasteboard.writeObjects` → 被调用一次 + 参数含预期 paths | 单测绿 | 3b.7a |
| 自动 | Windows `set_files` 单测：mock `SetClipboardData(CF_HDROP, hdrop)` → 参数捕获 + DROPFILES 结构验证 | 单测绿 | 3b.7a |
| 自动 | Linux `set_files` 单测：mock `xclip -selection clipboard -t text/uri-list -i` 或 `wl-copy` 子进程 → args 断言 | 单测绿 | 3b.7a |
| 自动 | mock backend 接收 ClipboardFiles 后 pre-stamp + `set_files` 被调用一次 | 单测绿 | 3b.7b |
| 自动 | `inject_to_clipboard=false` 时 `set_files` 不调用 | 单测绿 | 3b.7b |
| 自动 | 回环指纹命中时 `set_files` 不调用（pre-stamp 前查询） | 单测绿 | 3b.7b |
| 自动 | 落盘失败（sha256 mismatch / IO error / .partial 残留）时 `set_files` 不调用 | 单测绿 | 3b.7b |
| 自动 | `MIME_TOO_LARGE` entry 跳过 `set_files` | 单测绿 | 3b.7b |
| 自动 | `ExceedsLimit` entry 跳过 `set_files` | 单测绿 | 3b.7b |
| 自动 | `Canceled` entry 跳过 `set_files` | 单测绿 | 3b.7b |
| 自动 | `ClipboardConfig` serde round-trip `inject_to_clipboard` 缺字段 = true | 单测绿 | 3b.7b |
| 自动 | `cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` | 无 diff / 无 warning | 3b.3 |
| 自动 | `cd lan-mouse-vue && pnpm build` 产物 OK | 0 error | 3b.4 |
| 自动 | 三平台编译通过 | CI matrix 全绿 | 3b.3 |
| **人类** | macOS 真机 GUI 配置：(a) A 端 ConnectionsPanel 改 `enable_clipboard_to` → 立即生效（config.toml 落盘）；(b) B 端同样改 → 两侧独立 | 截图 + config.toml diff（两端各一份） | 3b.5 |
| **人类** | Windows 真机 GUI 配置：(a) + (b) 同上 | 同上 | 3b.5 |
| **人类** | Linux 真机 GUI 配置：(a) + (b) 同上 | 同上 | 3b.5 |
| **人类** | GeneralPanel `max_file_size` 调到 100 MiB 后复制 60 MiB 文件（双向 A→B + B→A 各跑一次）：**不再**触发 ExceedsLimit popup，正常落盘 + sha256sum 一致 | 日志 + sha256sum | 3b.1 / 3b.5 |
| **人类** | GeneralPanel `enabled = false` 后复制文本：剪贴板**不**同步到对端；恢复后正常 | 日志 + pbpaste | 3b.1 / 3b.5 |
| **人类** | 200 MiB 性能：100 Mbps **有线** LAN 实测 < 30 s — (a) A→B + (b) B→A 各计时一次 | 秒表（两段） | 3b.3 |
| **人类** | 200 MiB 性能：Wi-Fi 实测 < 60 s（**评审 #5 3rd 双档**） — (a) A→B + (b) B→A 各计时一次 | 秒表（两段） | 3b.3 |
| **人类** | 200 MiB 性能：cancel 双向 — (a) A→B 源端覆盖剪贴板 → 接收端 1 s 内停止 + 清 .partial + (b) B→A 同上 | 秒表 + 文件系统 | 3b.3 |
| **人类** | 200 MiB 性能：拔网双向 — (a) A→B 方向：A 发起传输，B 接收中拔网 → 5 s 内 B 端 GUI 看到 "connection lost"；`(b) B→A` 同上 | 秒表 + 错误信息 | 3b.3 |
| **人类** | keepalive↔idle race：200 MiB 完成后 30 s 内连接仍 active（用 `lsof -i UDP:4252` / netstat 观察）；60 s 静默期不 disconnect | 终端 + 日志 | 3b.3 |
| **人类** | `lan-mouse-cli SetClipboardConfig --max-file-size 100 --accept-dir /tmp/recv --enabled` 生效（config.toml 落盘 + daemon reload） | config.toml diff | 3b.6 |
| **人类** | `lan-mouse-cli SetEnableClipboardTo 0 false` 关掉对端 0 的剪贴板推送 | 日志 | 3b.6 |
| **人类** | macOS 真机剪贴板回灌（双向）：(a) **A→B**：A 端 Finder 复制文件 → B 端落盘后**自动**入剪贴板 → B 端 Cmd+V 直接粘贴出该文件；(b) **B→A**：反过来同样跑一次 | 录屏 / 截图（两个方向各一段） | 3b.7a / 3b.7b |
| **人类** | Windows 真机剪贴板回灌（双向）：(a) **A→B** + (b) **B→A** 同 macOS 验证步骤 | 同上 | 3b.7a / 3b.7b |
| **人类** | Linux 真机剪贴板回灌（双向）：(a) **A→B** + (b) **B→A** 同 macOS 验证步骤（X11 / Wayland 按系统走） | 同上 | 3b.7a / 3b.7b |
| **人类** | 关掉回灌：GeneralPanel `inject_to_clipboard = false` 后复制文件 → 落盘但**不**入剪贴板；恢复后正常回灌 | 录屏（两个状态切换各一段） | 3b.7b |

### ~~M4 — GUI 集成 + 文档~~（cancelled，已合并到 M3b）

> **drop 的验收项**（用户决策 2026-09-13）：
> - **drop** Toaster `FileTransferRequest` 通知 + Accept / Reject 按钮（auto-accept only，不需要 GUI 交互）
> - **drop** GeneralPanel 剪贴板状态卡片（最近时间 + 前 80 字符预览 + `lastClipboardSource` 字段 + 5 s amber 高亮）
> - **drop** 回环跳过统计卡片（"过去 1 小时回环跳过 N 次"）；`service::clipboard::metrics` 仍 log，但 UI 不展示
> - **drop** README.md / DOC.md 文档章节（"跨设备剪贴板"+"文件同步" 章节）
> - **drop** `auto_accept_files` 控件（auto-accept 是本计划唯一模式）

### 不可自动化 / 必须人为判断的项

1. **平台剪贴板 API 行为差异**：macOS `changeCount` 跳变、Windows `CF_BITMAPINFO` 颜色深度、Linux Wayland `wlr-data-control` 协议版本——AI 只能 mock。
2. **200 MiB 真实网络性能**：受硬盘 I/O、Wi-Fi 信号、CPU 影响，AI 单测只能给上限。
3. **GUI 体验**：GeneralPanel 配置控件的布局 / 可用性是主观判断（Toaster accept/reject UI 已 drop，**不**再评估 Toaster 体验）。
4. **跨 DPI 显示器**：mixed scale factor 下剪贴板 / 文件同步不受影响（数据不涉及屏幕坐标），但显示密度可能让 UI 错位。
5. **macOS TCC 权限**：Accessibility / Input Monitoring 重启后需重授权，必须人在机器前。
6. **剪贴板历史抖动**：回环检测在极端时序下是否漏检，只有长时间运行 + 日志观察能确认（metrics 仍 log 但 UI 不展示，依赖人工 review log）。
7. **拔网时序真实行为**：HTTP/3 stream error 在真实网络断开 / Wi-Fi 切换 / 防火墙阻断 / 进程 kill 等不同场景下的延迟差异，AI 单测只能 mock `RecvStream::ReadError::ConnectionLost`，真机需多种物理场景验证。

### 测试工具与脚本建议

- **真机回归模板**：`tests/manual/clipboard-text.md` / `clipboard-image.md` / `file-transfer.md` 模板（"在 macOS 14 + Windows 11 对端下：1. 启动 daemon；2. 终端 pbcopy 'hello'；3. 对端 pbpaste 应该是 hello"），人类按模板逐项打勾。
- **协议 round-trip**：`cargo test -p lan-mouse-proto` 包含所有 ProtoEvent 变体。
- **HTTP/3 端到端**：`tests/quic_smoke.rs` 扩展为 `tests/http3_smoke.rs`，mock 一个 server + client 跑通 `/healthz` + `/clipboard/{text,image,file}/...`。
- **WebSocket 事件录制**：开发期 `RUST_LOG=lan_mouse_service=trace,lan_mouse_quic_transport=trace`，console 输出存 `tests/manual/<date>-<machine>.log`。
- **大文件性能**：`scripts/bench-file-transfer.sh` —— 跑 `dd` + `sha256sum` + 计时 + 错误检测，一键回归。

---

> **本文档定稿时间**：2026-09-06（初版） / 2026-09-13（M3b 重写：合并 M4 + drop Toaster UI / 可观察卡片 / 文档 + 拆 6 STEPs） / 2026-09-13（M3b STEP-3b.7a / 3b.7b 新增：接收端剪贴板回灌 — planer 审阅拆步：`set_files` trait 未在 M3a 落地，需先补三平台实现再集成 skip conditions + IPC serde + GUI checkbox + TOML；`ClipboardConfig.inject_to_clipboard` 字段）
> **作者**：Claude（基于用户 `REQUIREMENT.md` + `TECHNOLOGY.md` + 现有 codebase 调查）
> **下一步**：用户真机验证 M3a → 确认 / 调整 M3b 范围 → 启动 M3b STEP-3b.1
