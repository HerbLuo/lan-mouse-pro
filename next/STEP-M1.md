# STEP-M1.md — M1：用 QUIC 替换 webrtc-dtls + UDP

> **M 范围**：仅完成 **传输层** 从 `webrtc-dtls + UDP` 迁移到 `QUIC`。**不含任何剪贴板 / 跨设备文件同步 / h3 / HTTP/3 路径** —— 这些属 M2。
>
> **基础事实**（无需再 spike）：
>
> - 库：`quinn 0.11` + `rustls 0.23`（`ring` provider，**不要** `aws_lc_rs` —— Windows MSVC 缺 NASM）
> - `max_datagram_size` 随握手生命周期变化（1162 → 1414 字节），**不可缓存**
> - `quinn::Connection` `Send + Clone`，与 `LocalSet + spawn_local` 完全相容
>
> **搬运基线**：`lan-mouse-pro-bak/` M1 + M2 全部已 in-process smoke 全绿；约 18 个文件 ~14000 行 `cp` 即可。**真活清单 ≈ 6 项**（下面"BS 概览"中标 🔧）。

---

## 0. 范围 & 不做事项

### 0.1 In scope（M1 完成验收）

| 验收项                                                     | 源                                                           | 备注               |
| ---------------------------------------------------------- | ------------------------------------------------------------ | ------------------ | ------ |
| 鼠标 / 键盘 / Enter-Leave / Ping-Pong / Hello 握手等价工作 | `src/connect.rs`, `src/listen.rs`                            | 走新 QUIC 通道     |
| 自签证书 + 指纹白名单持久对端认证                          | `src/crypto.rs` + 新 `src/quic_transport.rs`                 | bak 已有 mTLS 实现 |
| 探活超时 8s → ≥30s                                         | 由 QUIC `keep_alive_interval` + `max_idle_timeout` 替代      | 见 BS-7.1          |
| Happy-eyeballs 支持 QUIC                                   | `dial_any()`                                                 | 见 BS-6.4          |
| 鼠标 button / 键盘 stream-or-datagram 可切换               | `lan-mouse-ipc::ChannelMode` + `route_input`                 | 见 BS-4            |
| 现有 IPC / CLI / GTK 公共 API 不变                         | `lan-mouse-ipc`, `lan-mouse-cli`, `lan-mouse-gtk/src/lib.rs` | 见 BS-7 末尾确认   |
| `cargo tree                                                | grep webrtc-dtls` **无输出**                                 | 依赖完全下线       | BS-7.3 |

### 0.2 Out of scope（推迟到 M2）

- 剪贴板 text / image / file 跨设备同步
- `h3` / `h3-quinn` / `http` 依赖引入
- `ProtoEvent::Clipboard` / `Bounds` / `MotionAbsolute` / `CursorPos` / `ReceiverSensitivity` 五变体
- 变长 codec（`encode_clipboard_event` / `decode_clipboard_event`）
- `MAX_CLIPBOARD_SIZE = 4 KiB` / `BufferTooLarge` 错误变体
- `input-event::ClipboardEvent` / `Axis::momentum` / `MACOS_KEEP_AWAKE_EVENT_TAG`
- `lan-mouse-ipc::TransportEvent`
- `lan-mouse-gtk` `status_bar`
- `lan-mouse-cli` stderr 事件订阅
- `lan-mouse/src/clipboard*.rs` 全部 8 文件

> M1 不引入 M2 任何字段，即便 bak 已经在同一文件里 —— 避免一次提交带过多改动。`SUGGESTION.md` 不再保留 M1 之前的草稿。

---

## 1. 大步骤（BS）概览

| #        | 大步骤                              | 小步数 | 估时       | 主真活                                   | 关键文件                                         |
| -------- | ----------------------------------- | ------ | ---------- | ---------------------------------------- | ------------------------------------------------ |
| BS-1     | 基础设施 & 传输抽象层               | 4      | 1h45       | 🔧 crypto 抽象                           | `crypto.rs` / `Cargo.toml` / `quic_transport.rs` |
| BS-2     | 认证 & TLS（mTLS + 信任模型）       | 7      | 3h45       | 🔧 TofuVerifier / AuthorizedKeysVerifier | `quic_transport.rs` / `listen.rs`                |
| BS-3     | 应用协议握手（PROTOCOL_MAGIC）      | 2      | 1h         | 🔧 proto + Hello.magic                   | `lan-mouse-proto` / `quic_transport.rs`          |
| BS-4     | 输入通道路由（ChannelMode）         | 6      | 2h45       | 🔧 ipc + config + route + GTK 下拉框     | `lan-mouse-ipc` / `config.rs` / `lan-mouse-gtk`  |
| BS-5     | 数据通道（Datagram + 3 Stream）     | 4      | 2h45       | ✅ 全搬运                                | `quic_transport.rs`                              |
| BS-6     | 出入站集成（connect / listen 改写） | 5      | 3h         | ✅ 全搬运                                | `connect.rs` / `listen.rs` / `service.rs`        |
| BS-7     | 收尾 & 验证（DTLS 下线 + smoke）    | 7      | 3h15       | ✅ 删依赖                                | `Cargo.toml` / `tests/` / `firewall.rs` / 文档   |
| **合计** |                                     | **35** | **~18h15** | 6 项真活                                 |                                                  |

> ⏱ 估时策略：每小步 **20–30 min** 目标值；若某小步突破 **1h**（"其它角色介入"红线），立即**就地拆步**并重新提交本 STEP-M1.md，**不要**继续推进。

---

## 2. 真活清单（M1 范围内）

仅 6 项实质性改动（其余搬运即用）。其余 14 文件 ~14000 行直接 `cp` bak → 主仓并改名 `Mousehop*` → `LanMouse*`。

| #    | 真活                                                                                          | 涉及文件                                          | 难度             |
| ---- | --------------------------------------------------------------------------------------------- | ------------------------------------------------- | ---------------- |
| TR-1 | `crypto.rs` 与 webrtc-dtls 解耦（Phase 0.2 前置）                                             | `lan-mouse/src/crypto.rs`                         | 中               |
| TR-2 | `lan-mouse-proto` 加 `PROTOCOL_MAGIC` + `ProtoEvent::Hello.magic`                             | `lan-mouse-proto/src/lib.rs`                      | 低               |
| TR-3 | `lan-mouse-ipc` 加 `ChannelMode` + `InputChannelConfig`                                       | `lan-mouse-ipc/src/lib.rs`                        | 低               |
| TR-4 | `lan-mouse/src/config.rs` 加 `input_channels` schema + 默认值回填                             | `lan-mouse/src/config.rs`                         | 低               |
| TR-5 | `lan-mouse-gtk/src/ui/client_editor.rs` 加 2 个 `ComboBoxText`                                | `lan-mouse-gtk/src/ui/client_editor.rs`           | 中               |
| TR-6 | `quic_transport.rs` 全文搬运（含 PeerSession / Endpoint / Verifier / Hello / Stream / Error） | `lan-mouse/src/quic_transport.rs`（新建 6009 行） | **大**，但纯搬运 |

---

## 3. 大步骤 · 小步骤 · 验收

---

### BS-1 基础设施 & 传输抽象层

**背景**：M1 的起点是把当前 `src/connect.rs` / `src/listen.rs` / `src/crypto.rs` 直接调用 `webrtc_dtls::*` 与 `webrtc_util::Conn` 的紧耦合剥离。本 BS 不引入 QUIC 行为，仅做"地基 + 抽象层"。

---

#### BS-1.1 `crypto.rs` 与 webrtc-dtls 解耦（Phase 0.2 前置，~30 min） 🔧 TR-1

**目标**：把 `crypto::load_or_generate_key_and_cert(cert) -> Certificate` 的返回类型从 `webrtc_dtls::crypto::Certificate` 换为 `Vec<rustls::pki_types::CertificateDer<'static>>` + `PrivateKeyDer<'static>`。后续所有步骤不再依赖 `webrtc-dtls`。

**文件**：

- `lan-mouse/src/crypto.rs`（主体重写）
- `lan-mouse/src/service.rs`（仅 `Certificate` 类型替换，~3 行）
- `lan-mouse/src/listen.rs`、`lan-mouse/src/connect.rs`（如果还引用 `Certificate` 类型，改为接受新类型）

**搬运参考**：`lan-mouse-pro-bak/mousehop/src/crypto.rs:1-469`（除 `webrtc_dtls_compat` feature 相关段，整文件复制）

**变更要点**：

- 保留 `pub fn generate_fingerprint(cert: &[u8]) -> String` 签名
- 新增 / 改写：`load_cert_der` / `load_key_der` / `load_or_create_server_cert` / `rustls_server_config` / `rustls_client_config`
- 新增 `pub fn cert_path() -> PathBuf`（统一路径解析，OS 无关）
- 删 `Error::Dtls(#[from] webrtc_dtls::Error)`
- **不引入** `webrtc_dtls_compat` feature（与 M1 下线 DTLS 的目标对齐）

**验证**：

```bash
cargo build -p lan-mouse
cargo test  -p lan-mouse crypto::tests
ls ~/.local/share/lan-mouse/cert.pem   # 首次启动生成（路径与 bak 保持一致）
ls ~/.local/share/lan-mouse/key.pem    # key 分离持久（与 bak 一致）
```

预期：单测通过；key/cert 文件首次生成、再次启动复用同一指纹。

**依赖**：无
**回退方案**：若现有 `service.rs::new()` 因类型替换破坏超过 5 处，保留 `webrtc_dtls::crypto::Certificate` 类型别名兼容 24h，下一 BS-2 步骤再彻底切。

---

#### BS-1.2 workspace 加 `quinn` + `rustls` 依赖（~15 min）

**目标**：把 `quinn 0.11` 写入 workspace `[workspace.dependencies]`，并把现有的 `rustls 0.23`（已存在于 `lan-mouse/Cargo.toml`）提升到 workspace 级共享。

**文件**：`Cargo.toml`（workspace 根）

**变更要点**（请严格按 PLAN-v4 §5 / SUGGESTION #17）：

- workspace 新增 `quinn = { version = "0.11", default-features = false, features = ["runtime-tokio", "rustls-ring", "log"] }`
- workspace 新增 `rustls = { version = "0.23", default-features = false, features = ["std", "ring"] }`
- **`lan-mouse/Cargo.toml`** 删除独立的 `webrtc-dtls 0.12.0` 与 `webrtc-util 0.11.0`；本步骤**先删除**，但是 `connect.rs` / `listen.rs` 还残留调用，编译仍会失败 —— 编译失败是预期，**不要回退**。
- **不要** 引入 `aws_lc_rs` / `rustls-aws-lc-rs`（Windows MSVC NASM 缺失，Step 0.1 已确认）。
- `firewall.rs` 头部注释 `DTLS over UDP` 改成 `QUIC over UDP`（顺手做，~30s）。

**验证**：

```bash
cargo tree -p lan-mouse | grep -E "quinn|rustls"
grep -nE "webrtc-dtls|webrtc-util" Cargo.toml lan-mouse/Cargo.toml   # 期望：无
```

预期：依赖树出现 `quinn 0.11.x`、`rustls 0.23.x`；workspace 中已无 `webrtc-dtls / webrtc-util`。`lan-mouse` 构建**预期失败**（DTLS 引用仍在 `connect.rs` / `listen.rs`）。

**依赖**：BS-1.1
**警告**：本步结束**故意让 `lan-mouse` 编译失败** —— 这是为下一步铺路。不要把这一步标注为 "完成 build"。

---

#### BS-1.3 新建 `quic_transport.rs` 骨架 + lib.rs 注册（~20 min）

**目标**：空 `PeerSession` 模块能编译挂上；不引入任何 QUIC 行为。

**文件**：

- `lan-mouse/src/quic_transport.rs`（新建）
- `lan-mouse/src/lib.rs`（加 `mod quic_transport;`）

**变更要点**：

```rust
// 仅占位，BS-1.4 起逐步填实
pub struct PeerSession { /* TODO */ }
pub enum Error { #[error("not implemented")] NotImplemented }
```

不要在这一步定义任何 `pub fn endpoint` / `pub async fn dial` —— 见 BS-1.4。

**验证**：

```bash
cargo check -p lan-mouse
cargo clippy -p lan-mouse --all-targets -- -D warnings
```

预期：**编译通过**（之前 BS-1.2 留下的 `webrtc-dtls` 调用错误本步骤还不修，因为 `quic_transport.rs` 还没替代它；本步只确保新增模块本身不引入错）。

**依赖**：BS-1.2
**注意**：本步完成意味着 `lan-mouse` 可注册 `pub mod quic_transport;`，但 `connect.rs` 仍调旧 DTLS API，编译仍失败 —— 正常。

---

#### BS-1.4 `endpoint()` —— UDP socket 包装成 quinn::Endpoint（~30 min）

**目标**：`pub fn endpoint(addr: SocketAddr) -> Result<Endpoint, Error>` 单测通过；UDP 套接字绑定 + `quinn::Endpoint::new()` + 多 CID + keepalive 配置就位。

**文件**：

- `lan-mouse/src/quic_transport.rs`
- `lan-mouse/src/listen.rs`（**最小桥接**：暂时禁用 listen 主循环调用 `listen(...)` 改为"占位返回 dummy"，留 BS-6 改造；这一步先不接）

**变更要点**：

- 实现 `pub fn endpoint(addr: SocketAddr) -> Result<Endpoint>`
- 内部用 `tokio::net::UdpSocket::bind(addr)` → `quinn::Endpoint::new(EndpointConfig, server_cfg, socket)`
- 配置 `EndpointConfig::default()`（启用 `connection_id_generator` 支持连接迁移）
- 配置 `TransportConfig`：`max_idle_timeout = Duration::from_secs(30)`（QUIC keepalive 替代 8s 应用层 idle 检测） + `keep_alive_interval = Duration::from_secs(5)`
- 用**占位** `ServerConfig` / `ClientConfig`（仅 token，无 cert） —— BS-2 填实
- `pub use quinn::Endpoint;`

**验证**：

```rust
#[tokio::test]
async fn endpoint_binds_ipv4_localhost() {
    let ep = endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
    drop(ep);
}
```

```bash
cargo test -p lan-mouse quic_transport::endpoint_binds_ipv4_localhost
```

预期：单测通过。

**依赖**：BS-1.3

---

### BS-2 认证 & TLS（mTLS + 信任模型）

**背景**：QUIC 握手复用现有自签证书 + 指纹白名单。M1 在两端都出证书（**mTLS**），客户端加 TOFU 缓存，服务端沿用 `authorized_keys` 显式 allowlist。

---

#### BS-2.1 `rustls::ClientConfig` 构造 + crypto provider（~30 min）

**目标**：从 `crypto::rustls_client_config(cert_der, key_der, None)` 拿到 `Arc<rustls::ClientConfig>` 并装载到 `quinn::ClientConfig`；进程启动时 `ring` provider 已 `install_default()`。

**文件**：

- `lan-mouse/src/quic_transport.rs`
- `lan-mouse/src/crypto.rs`（暴露 `rustls_client_config`，从 BS-1.1 已实现）
- `lan-mouse/src/main.rs`（**启动时** `rustls::crypto::ring::default_provider().install_default()`，早于任何 `ClientConfig::builder()`）

**变更要点**：

- `pub fn build_quic_client_config(cert: CertificateDer, key: PrivateKeyDer) -> Result<quinn::ClientConfig>`
- Provider 必须在 `main()` 顶层安装，否则运行期 panic
- 不带 verifier 占位（BS-2.6 TofuVerifier）

**验证**：

```rust
#[test]
fn quinn_client_config_loads_rustls_provider() {
    let cert = ...; // 测试 cert
    let cfg = build_quic_client_config(cert, key).unwrap();
    let _ = quinn::ClientConfig::new(cfg);
}
```

预期：单测通过；进程启动后 CLI / GTK / daemon 三种入口都能成功出 dial。

**依赖**：BS-1.4

---

#### BS-2.2 `dial()` 完成 QUIC TLS 握手（占位 verifier，~30 min）

**目标**：连到对端 endpoint 完成 TLS 1.3，返回 `Connection`。

**文件**：`lan-mouse/src/quic_transport.rs`

**变更要点**：

- `pub async fn dial(ep: &Endpoint, addr: SocketAddr, cert: CertificateDer, key: PrivateKeyDer) -> Result<Connection>`
- 用 `crypto::rustls_client_config(cert, key, None)` —— `None` 即"Dangerous / 不做 verifier"，**BS-2.6 替换为 TofuVerifier**
- `ep.connect_with(cfg, addr, "lan-mouse")?.await?`

**验证**：

```rust
#[tokio::test]
async fn dial_completes_handshake_against_local_listener() {
    let server_ep = endpoint(...).unwrap();
    let server_addr = server_ep.local_addr().unwrap();
    spawn_local(async move {
        let _conn = server_ep.accept().await.unwrap().await.unwrap();
    });
    let client_ep = endpoint(...).unwrap();
    let conn = dial(&client_ep, server_addr, cert, key).await.unwrap();
    assert!(conn.peer_identity().is_some());
}
```

预期：`peer_identity()` 非空。

**依赖**：BS-2.1

---

#### BS-2.3 `accept()` 接受 QUIC 连接（占位 ServerConfig，~30 min）

**目标**：服务端接受 QUIC 连接并返回原始 `Connection`。

**文件**：`lan-mouse/src/quic_transport.rs`

**变更要点**：

- `pub async fn accept(ep: &Endpoint) -> Result<Connection>`
- `endpoint()` 的 ServerConfig 占位 token（无 cert 出示）—— 满足 BS-2.5 之前 smoke 可跑即可

**验证**：

- 跑通 BS-2.2 的 in-process 测试即代表 accept 路径 OK；本步不再单独加测试。

**依赖**：BS-2.2

---

#### BS-2.4 服务端证书持久化身份（~30 min）

**目标**：`~/.local/share/lan-mouse/cert.pem` + `key.pem` 启动加载；指纹稳定。

**文件**：

- `lan-mouse/src/crypto.rs`（`load_or_create_server_cert()`，已在 BS-1.1 实现；本步补 `cert_path()` 路径计算）
- `lan-mouse/src/quic_transport.rs`（新 `pub fn endpoint_with_cert(addr, cert, key)` 调用 `crypto` 拼装 ServerConfig）
- `lan-mouse/src/service.rs`（在 `Service::new()` 用 `endpoint_with_cert(...)` 替换原 `LanMouseListener::new(...)` 中 DTLS 配置）

**变更要点**：

- 证书路径已由 `crypto::cert_path()` 在 BS-1.1 解析
- key 必须 0o400（Unix）/`...` ICACL（Windows）权限
- **server 端** + **client 端** 都出证书（mTLS：BS-2.5 启用 client 强制校验）

**验证**：

```bash
ls -la ~/.local/share/lan-mouse/{cert,key}.pem
# 二次启动
diff <(cat ~/.local/share/lan-mouse/cert.pem) <(cat /tmp/first_cert.pem)   # 期望：无 diff
```

预期：cert 稳态持久。

**依赖**：BS-2.3

---

#### BS-2.5 mTLS：dial 出示 client cert / server 强制 client cert（~30 min）

**目标**：服务端的 `rustls::ServerConfig` 装配 `client_cert_verifier`（占位 AuthorizedKeysVerifier；BS-2.7 换真）；客户端的 `rustls::ClientConfig` 出示 client cert chain。

**文件**：`lan-mouse/src/quic_transport.rs`

**变更要点**：

- 新 `pub fn endpoint_with_verifier(addr, cert, key, verifier: Arc<dyn ClientCertVerifier>) -> Result<Endpoint>`
- dial 调用栈：B2.2 `dial(...)` 加 `cert: CertificateDer` 参数
- mTLS 不通过即拒绝握手

**验证**：

```rust
#[tokio::test]
async fn mtls_rejects_no_client_cert() {
    // 用 server 没要求 client cert 的 endpoint 建连，dial 一个无证书的 client
    // 断言对端 accept 返回 Err
}
```

预期：单测通过。

**依赖**：BS-2.4

---

#### BS-2.6 客户端 `TofuVerifier`（fingerprint pinning，~45 min） 🔧 部分

**目标**：客户端 TOFU 缓存到 `$XDG_DATA_HOME/lan-mouse/known_peers/<fp>.pin`；二次连接 fingerprint 不匹配 → 拒绝。

**文件**：`lan-mouse/src/quic_transport.rs`

**搬迁参考**：`lan-mouse-pro-bak/mousehop/src/quic_transport.rs:1384-1577`（`TofuVerifier` struct + `impl rustls::client::danger::ServerCertVerifier`）

**变更要点**：

- `pub struct TofuVerifier { pins_dir: PathBuf, on_first_seen: ... }`
- `pub fn new(pins_dir: &Path) -> Self` / `pub fn with_known(pins_dir, fp)` (test helper)
- 实现 `ServerCertVerifier::verify_server_cert`：
  - SHA-256 → lowercase hex joined by `:` (即 `generate_fingerprint`)
  - 命中缓存 → `Ok(ServerCertVerified::assertion())`
  - 未命中 → `Err(rustls::Error::General(format!("untrusted peer {fp}")))`
- 第一次见到某 fingerprint → 落盘占位文件 + 日志 `paired with <fp>`

**验证**：

```rust
#[tokio::test]
async fn tofu_first_run_pins() { ... }
#[tokio::test]
async fn tofu_disallows_swap() { ... }
```

预期：两个测试通过。

**依赖**：BS-2.5

---

#### BS-2.7 服务端 `AuthorizedKeysVerifier`（显式 allowlist，~30 min） 🔧 部分

**目标**：server 端用现有 `Arc<RwLock<HashMap<String, IncomingPeerConfig>>>` 做 allowlist；未授权 fingerprint → mTLS 拒绝握手。

**文件**：

- `lan-mouse/src/quic_transport.rs`
- `lan-mouse/src/listen.rs`（注入 verifier 入 ServerConfig）

**搬运参考**：`bak/mousehop/src/quic_transport.rs:1577-1754` + `bak/mousehop/src/listen.rs`（AuthorizedKeysVerifier 注入点）

**变更要点**：

- `pub struct AuthorizedKeysVerifier { allowlist: Arc<RwLock<HashMap<String, IncomingPeerConfig>>> }`
- 实现 `ClientCertVerifier::verify_client_cert`：调用 `generate_fingerprint` → 查 allowlist
- listen.rs 用 `endpoint_with_verifier(addr, cert, key, verifier)` 替代旧 DTLS 路径

**验证**：

```bash
bash scripts/trust_neg_test.sh
```

预期：脚本里伪造 fingerprint 的对端被服务端拒握手。

**依赖**：BS-2.6

---

### BS-3 应用协议握手（PROTOCOL_MAGIC）

**背景**：QUIC TLS 完成 + 信任模型通过后，立刻在 **Stream A（控制面）** 上做 Hello 握手，魔数错即拒连（**M1 新设计**，bak 已验证）。

---

#### BS-3.1 `ProtoEvent::Hello` 加 `magic` 字段 + `PROTOCOL_MAGIC` 常量（~30 min） 🔧 TR-2

**目标**：`Hello` 在现有 `commit: [u8;8]` 之上增加 `magic: [u8;8]`，并在 `lan-mouse-proto/src/lib.rs` 顶部加 `pub const PROTOCOL_MAGIC: [u8;8] = *b"LANMOUSE";`。

**文件**：`lan-mouse-proto/src/lib.rs`

**变更要点**：

- 重新计算 `MAX_EVENT_SIZE`：原 17 字节 → 新 25 字节（17+8）。**所有用 `MAX_EVENT_SIZE` 做 buffer 长度的地方都要复核**（grep 走查：`lan-mouse-proto` + `lan-mouse` + `lan-mouse-cli` + `lan-mouse-gtk`）
- `EventType::Hello` 分支：增加 `magic` 解码；升级定长 codec
- `From<ProtoEvent>` 编码同理
- 缺省 magic 不匹配：返回 `Err(ProtocolError::HelloMagicMismatch)` 即可，不必新增 BufferTooLarge（**那是 M2 的事**）

**搬运参考**：`bak/mousehop-proto/src/lib.rs:1-697` + 注：本步只取 `Hello.magic` 段与 `PROTOCOL_MAGIC` 常量；M2 才加 `Clipboard` / 其它变体。

**验证**：

```rust
#[test]
fn hello_encode_decode_round_trip() {
    let h = ProtoEvent::Hello { magic: PROTOCOL_MAGIC, commit: *b"deadbeef" };
    let (buf, len) = h.into();
    let back: ProtoEvent = buf.try_into().unwrap();
    assert!(matches!(back, ProtoEvent::Hello { magic, commit } if magic == PROTOCOL_MAGIC && commit == *b"deadbeef"));
}

#[test]
fn hello_wrong_magic_decodes_but_typed() {
    let h = ProtoEvent::Hello { magic: *b"WRONGMAG", commit: *b"deadbeef" };
    let (buf, len) = h.into();
    let back: ProtoEvent = buf.try_into().unwrap();
    // 类型层 decode 依然成功；语义层由 BS-3.2 校验 magic
}
```

预期：两单测通过。

**依赖**：BS-2.7

---

#### BS-3.2 `client_hello` / `server_hello` 实现 + 魔数校验 + 超时（~30 min）

**目标**：建连后**第一条**双向 stream 视为 **Stream A（control）**，先做 Hello 握手；magic / commit 不匹配立即 `conn.close(VarInt(0), "hello failed")`。

**文件**：`lan-mouse/src/quic_transport.rs`

**搬运参考**：`bak/mousehop/src/quic_transport.rs:2419-2588`（整段 `client_hello` / `server_hello` 复制即可）

**变更要点**：

- `pub const HELLO_TIMEOUT: Duration = Duration::from_secs(3);`（沿用 bak）
- `pub async fn client_hello(peer: &PeerSession) -> Result<(), Error>`
- `pub async fn server_hello(peer: &PeerSession) -> Result<(), Error>`
- `pub struct PeerSession { ... hello_ok: Cell<bool> ... }` 加 `hello_ok` 字段 + 访问器
- 只有 `hello_ok == true` 才允许 `send_motion` / stream B 打开

**验证**：

```rust
#[tokio::test]
async fn hello_wrong_magic_closes_connection() {
    // server 用合法 magic；client 发错的 magic
    // 断言 server 端收到 CloseReason("hello failed")
}

#[tokio::test]
async fn hello_timeout_aborts_session() {
    // 对端不开 control stream
    // 3s 后断言 hello_ok == false
}
```

预期：两个测试通过。

**依赖**：BS-3.1

---

### BS-4 输入通道路由（ChannelMode）

**背景**：M1 保留现有"鼠标 button 走 datagram / 键盘走 stream"的**默认**，但允许用户在 config 切换。本 BS 落地 ChannelMode 的 4 类真活与一份文档。

---

#### BS-4.1 `lan-mouse-ipc` 加 `ChannelMode` + `InputChannelConfig`（~15 min） 🔧 TR-3

**目标**：新增 IPC 类型供 config 与 quic_transport 共享。

**文件**：`lan-mouse-ipc/src/lib.rs`

**变更要点**：

- `pub enum ChannelMode { Stream, Datagram }` 加 `Clone / Copy / Debug / PartialEq / Eq / Serialize / Deserialize`
- `pub struct InputChannelConfig { mouse_button: ChannelMode, keyboard: ChannelMode }` 同上 + `Default`（mouse=Datagram / keyboard=Stream）
- **不**加 `TransportEvent`（M2 才要，见 0.2）

**验证**：

```rust
#[test]
fn channel_mode_default() {
    let cfg = InputChannelConfig::default();
    assert_eq!(cfg.mouse_button, ChannelMode::Datagram);
    assert_eq!(cfg.keyboard, ChannelMode::Stream);
}
```

预期：通过。

**依赖**：BS-3.2

---

#### BS-4.2 `config.rs` 加 `input_channels` schema（~15 min） 🔧 TR-4

**目标**：per-handle 结构 `ConfigClient` 加 `input_channels: InputChannelConfig`（不再 Optional —— 内部统一 default 化）。

**文件**：`lan-mouse/src/config.rs`

**变更要点**：

- `ConfigClient` 加字段 `pub input_channels: InputChannelConfig`
- 解析时：缺省视为 `InputChannelConfig::default()`
- 写回 TOML 时：若 `input_channels == InputChannelConfig::default()` 则省略字段

**验证**：

```rust
#[test]
fn config_parses_input_channels_field() { ... }    // 见 PLAN-v4.md Step 1.7b
#[test]
fn config_defaults_when_input_channels_missing() { ... }
```

预期：两单测通过。

**依赖**：BS-4.1

---

#### BS-4.3 `config.toml` 示例更新（~15 min）

**目标**：仓库根 `config.toml` + DOC.md 示例同步。

**文件**：

- `config.toml`
- `DOC.md` 中 config 段落

**变更要点**：

- `[clients.desktop-east]` 段下加注释示例
  ```toml
  input_channels = { mouse_button = "datagram", keyboard = "stream" }
  ```
- 字段顺序保留向后兼容（缺省可省略）

**验证**：

```bash
cargo test -p lan-mouse config::tests
grep -A 1 input_channels config.toml
```

预期：单测通过；示例与新 schema 一致。

**依赖**：BS-4.2

---

#### BS-4.4 `route_input()` 纯函数 + 四个组合测试（~30 min）

**目标**：`PeerSession::route_input(&self, &ProtoEvent) -> Channel` 按 per-handle config 分派；纯函数版本 `route_input(cfg: &InputChannelConfig, event: &ProtoEvent) -> Channel` 同步暴露给单测。

**文件**：`lan-mouse/src/quic_transport.rs`

**搬运参考**：`bak/mousehop/src/quic_transport.rs:929-1004`

**变更要点**：

- `pub enum Channel { Datagram, StreamA, StreamB, StreamC }`（StreamC 是为 M2 clipboard 元数据预留，本步不开读 task）
- Motion 永远走 Datagram（即便 keyboard=Stream/mouse=Stream）
- Enter/Leave/Ack/Hello 走 StreamA
- 鼠标 button + 键盘 + Modifiers：按 `input_channels` 分派（datagram → Datagram；stream → StreamB）

**验证**：

- 四组合单测（与 PLAN-v4.md Step 1.7c 完全一致），不再赘述。

**依赖**：BS-4.3

---

#### BS-4.5 GTK `client_editor.rs` 加两个 `ComboBoxText`（~45 min） 🔧 TR-5

**目标**：peer 编辑对话框暴露 `Mouse button channel` / `Keyboard channel` 两个下拉框。

**文件**：`lan-mouse-gtk/src/ui/client_editor.rs`

**变更要点**：

- 加 `ComboBoxText`：
  - `Mouse button channel`：`Datagram (real-time)` / `Stream (reliable)`
  - `Keyboard channel`：`Stream (reliable)` / `Datagram (real-time)`
- 写入 `ClientConfig` 时序列化 `input_channels = { mouse_button = "...", keyboard = "..." }`
- 打开已有 peer 时回填下拉值；保存写回 `ClientConfig`

**搬运参考**：`bak/mousehop-gtk/src/ui/client_editor.rs`（GTK 加两下拉框段）

**验证**：手动 GUI 测试：编辑 → 切换 → 保存 → 重开确认持久。
**依赖**：BS-4.4

---

#### BS-4.6 README / DOC.md 文档更新（~15 min）

**目标**：用户能看懂两种模式取舍。

**文件**：`README.md`（英文）、如有 `README.zh-CN.md` 则同步；`DOC.md` config 段加说明。

**变更要点**：抄 `PLAN-v4.md §3.1.6` 原文。

**验证**：人工 review；grep "Stream 模式不丢操作" / "Datagram 模式丢操作"。
**依赖**：BS-4.5

---

### BS-5 数据通道（Datagram + 3 Stream 帧协议）

**背景**：所有 QUIC 应用层 IO 落地。BS-5 不动 connect.rs / listen.rs，只在 `quic_transport.rs` 内自洽能跑通 4 条并发通道（1 datagram + 3 stream）。

---

#### BS-5.1 `PeerSession::send_motion` 走 `send_datagram` + 降级 stream（~30 min）

**目标**：Motion 优先 `send_datagram`，超 `max_datagram_size` 时降级 stream。**每次读 `max_datagram_size()`，不缓存**（PLAN-v4 Step 0.1 结论 D）。

**文件**：`lan-mouse/src/quic_transport.rs`

**变更要点**：

- `pub async fn send_motion(&self, event: &ProtoEvent) -> Result<()>`
- 内联：`if let Some(max) = conn.max_datagram_size() { if bytes.len() <= max { conn.send_datagram(...)?; return; } } fallback_to_stream`

**验证**：

```rust
#[tokio::test]
async fn motion_datagram_round_trip() {
    // 两端 session，互发 Motion；断言对端 recv_datagram 收到
}
```

预期：通过。

**依赖**：BS-4.4

---

#### BS-5.2 `StreamBunch` struct + 长度前缀帧 codec（~30 min）

**目标**：定义 `StreamBunch { a, b, c }` 结构 + 帧 `write_frame` / `read_frame`，单元测试覆盖 codec 正确性。

**文件**：`lan-mouse/src/quic_transport.rs`

**搬运参考**：`bak/mousehop/src/quic_transport.rs:2126-2300`

**变更要点**：

- `pub struct Bidi<S> { send: S, recv: S }`、`pub struct StreamBunch { a, b, c }`
- 帧格式：`[u32 BE length][bytes...]`
- `pub async fn write_frame(send: &mut SendStream, event: &ProtoEvent) -> Result<(), Error>`
- `pub async fn read_frame(recv: &mut RecvStream) -> Result<ProtoEvent, Error>`
- 加错误变体 `Error::FrameTooLarge(usize)` / `Error::Truncated`

**验证**：

```rust
#[tokio::test]
async fn frame_round_trip() { ... }
#[tokio::test]
async fn frame_truncated_rejected() { ... }
```

预期：两个测试通过。

**依赖**：BS-5.1

---

#### BS-5.3 3 条 stream 独立读 task + 路由分派（~45 min）

**目标**：每条 stream 一个独立 `spawn_local` 读 task，事件经由 `local_channel::mpsc` 队列；`select!` 合并对外暴露。

**文件**：`lan-mouse/src/quic_transport.rs`

**搬运参考**：`bak/mousehop/src/quic_transport.rs:2126-2600` 整体

**变更要点**：

- `pub async fn read_loop(&self, recv_a: RecvStream) -> Result<ReadStreams, Error>` 返回 `ReadStreams { b, c }`（a 是参数）
- 派发表按 §3：`StreamBunch::route(event)` → A/B/C
- backpressure（SUGGESTION #28）：队列满时**丢最旧**的 datagram 类事件、阻塞 control/input 类事件

**验证**：

```rust
#[tokio::test]
async fn streams_are_independent() { ... }
#[tokio::test]
async fn stream_frame_round_trip() { ... }
```

预期：两个测试通过；`streams_are_independent` 显式证明 B 不被 C 阻塞。

**依赖**：BS-5.2

---

#### BS-5.4 hello_watchdog + datagram_reader + 端到端本地 IO（~30 min）

**目标**：把 `PeerSession::run()` 主干拼起来：连接建立 → 三 stream 打开 + datagram 开始 → hello_watchdog 启 → select! 主循环出事件。

**文件**：`lan-mouse/src/quic_transport.rs`

**变更要点**：

- `pub async fn run(&self) -> Result<(), Error>`：
  1. 启 hello_watchdog（3s）
  2. 启 datagram_reader task
  3. 开三 bidi（accept_a/b/c 或 open_b/c，对端视角对称）
  4. 处理 `Connection::closed()` → 触发 `should_retry_after_close`
- 这一步**不**接入 `connect.rs` / `listen.rs`：纯粹 in-process 两端打通 IO

**验证**：

```rust
#[tokio::test]
async fn peer_session_round_trip_motion_keyboard() {
    let (peer_a, peer_b) = test_two_peers().await;
    peer_a.send_motion(&motion_event()).await.unwrap();
    peer_b.send_motion(&motion_event()).await.unwrap();
    // 断言双方都从对方读到
}
```

预期：通过。

**依赖**：BS-5.3

---

### BS-6 出入站集成（connect.rs / listen.rs 改造）

**背景**：把 `connect.rs::LanMouseConnection` 与 `listen.rs::LanMouseListener` 整体切到 `PeerSession`。

---

#### BS-6.1 `connect.rs::LanMouseConnection` 持有 `Rc<PeerSession>`，`send()` 走新通道（~45 min）

**目标**：替换 `connect.rs:46-167` 整段 DTLSConn 路径为 PeerSession 路径；`send()` 调用 `peer.route_input(event)` 决定通道。

**文件**：

- `lan-mouse/src/connect.rs`
- `lan-mouse/src/quic_transport.rs`（必要时新增 `connect_to_handle` 公开函数）

**搬运参考**：`bak/mousehop/src/connect.rs:624-900`（`connect_to_handle` 整段）

**变更要点**：

- `LanMouseConnection.conns` 类型 `Rc<AsyncMutex<HashMap<SocketAddr, Rc<PeerSession>>>>`
- `send()` 改成查表拿到 `peer`，然后 `peer.send(event)` 或 `peer.send_motion(event)`
- `MousehopConnectionError` 中 `Dtls` / `Webrtc` 变体删除（已无 caller）

**验证**：

```bash
cargo build -p lan-mouse
cargo test -p lan-mouse connect::tests
```

预期：通过；旧的 connect::tests 改造不依赖 DTLS 的部分要继续跑通。

**依赖**：BS-5.4

---

#### BS-6.2 `listen.rs::read_loop` 切到 `PeerSession` + `read_any_frame`（~45 min）

**目标**：替换 `listen.rs:248-283` `read_loop` 的 DTLSConn 路径为 PeerSession。

**文件**：`lan-mouse/src/listen.rs`、`lan-mouse/src/quic_transport.rs`

**搬运参考**：`bak/mousehop/src/listen.rs:1-649`

**变更要点**：

- 新循环：调用 `peer.read_loop(recv_a, ...) -> ReadStreams { b, c }` + datagram 队列 → `tokio::select!` 合并三个流
- 删 `as_any().downcast_ref::<DTLSConn>()` 旧路径
- 类型别名 `ArcConn` 删除

**验证**：

```bash
cargo build -p lan-mouse
bash scripts/quic_smoke.sh    # 上一步先抄 bak，但这一步确认 listen 主循环跑通
```

预期：脚本退出码 0。

**依赖**：BS-6.1

---

#### BS-6.3 `listen.rs`：supervisor 清理 + macOS wake 整合（~30 min）

**目标**：保留现有 macOS 唤醒后 force-close 行为，与新 PeerSession 路径整合。

**文件**：`lan-mouse/src/listen.rs`

**搬运参考**：`bak/mousehop/src/listen.rs` supervisor 部分

**变更要点**：

- `spawn_supervisor_task` 中 `wake_rx` 触发的 close 改为 `peer.conn().close(0)`
- `terminate()` 改用新 task 结构清理
- if_watch 接口变化（listener 类型变 `Endpoint`，接入同步改）

**验证**：

- 在 macOS / Linux 各做一次手动 smoke（与 BS-6.2 共用脚本）

**依赖**：BS-6.2

---

#### BS-6.4 `dial_any()` happy-eyeballs 适配 QUIC（~30 min）

**目标**：先拨 primary IP，200ms 内不通则并发所有候选。

**文件**：

- `lan-mouse/src/quic_transport.rs`（`pub async fn dial_any(...)`）
- `lan-mouse/src/connect.rs`（`connect_any` 调用 dial_any 替换 DTLS 路径）

**变更要点**：

- `pub async fn dial_any(ep: &Endpoint, primary: SocketAddr, all: &[SocketAddr], cert, key, pins_dir, cfg) -> Result<Connection>`
- `JoinSet` 模式，与现有 `connect_any` 对称

**验证**：

```rust
#[tokio::test]
async fn dial_any_prefers_primary() { ... }
```

预期：通过。

**依赖**：BS-6.1

---

#### BS-6.5 `Connection::closed()` → 重连触发（~30 min）

**目标**：连接中断时复用现有 retry 框架自动重连；`RetryState` 接 `ConnectionEvent::Lost(handle, reason)`。

**文件**：

- `lan-mouse/src/quic_transport.rs`
- `lan-mouse/src/connect.rs`

**变更要点**：

- `PeerSession::run()` 加分支：等待 `conn.closed()` → close reason 转为 `LanMouseConnectionError::Timeout`
- 复用 `connect.rs` 现有 `RetryState` 退避
- **不**重连成功判定：连续 N 次重试（按 `RetryState` 退避上限）仍失败 → 视为"对端真离线"，IPC 推 `PeerLost`（M2 才补 `TransportEvent::PeerLost`；M1 阶段只触发本地重连 + 日志）

**验证**：

```rust
#[tokio::test]
async fn reconnect_on_peer_close() { ... }
#[tokio::test]
async fn backoff_doubles_on_each_failure() { ... }
```

预期：与 §7 重连恢复 < 2s 预算吻合。

**依赖**：BS-6.4

---

### BS-7 收尾 & 验证（DTLS 下线 + smoke）

**背景**：所有链路绿后，最后一步彻底删 `webrtc-dtls` 与应用层 idle 检测；跑端到端 smoke。

---

#### BS-7.1 移除 `RECV_IDLE_TIMEOUT`（~15 min）

**目标**：QUIC 自带 keepalive，不再需要应用层 idle 检测。

**文件**：`lan-mouse/src/listen.rs`

**变更要点**：

- 删 `const RECV_IDLE_TIMEOUT: Duration = Duration::from_secs(8);`
- 删 `read_loop` 里的 `tokio::time::timeout` 包裹
- 改为 `peer.read_any_frame(...).await` 直调

**验证**：

```bash
cargo build -p lan-mouse
# 两端连接后让对端 sleep 5s，本端不报"closing stale connection"
```

预期：5s 静默不触发关闭。

**依赖**：BS-6.5

---

#### BS-7.2 端到端 QUIC smoke 测试（~45 min）

**目标**：两实例通过 QUIC 交换基本事件。

**文件**：

- `lan-mouse/tests/quic_smoke.rs`（新建）
- `lan-mouse/tests/input_channel_routing.rs`（新建）
- `scripts/quic_smoke.sh`（新建）

**搬运参考**：`bak/mousehop/tests/quic_smoke.rs` + `bak/mousehop/tests/input_channel_routing.rs`

**变更要点**：

- 集成测试：启动一个 in-process listener + 一个 in-process connector，断言 5 个 Motion + 5 个 KeyboardKey 全部到达
- 4 组合（默认 / gaming / 全部 Stream / 混合）channel routing 测试
- shell 脚本：跑两个 `lan-mouse-cli` 进程互发

**验证**：

```bash
cargo test -p lan-mouse --test quic_smoke
cargo test -p lan-mouse --test input_channel_routing
bash scripts/quic_smoke.sh
```

预期：cargo 测试通过；脚本退出码 0。

**依赖**：BS-7.1

---

#### BS-7.3 删 `webrtc-dtls` / `webrtc-util` 依赖（~30 min）

**目标**：workspace 依赖干净。

**文件**：

- `Cargo.toml`（workspace）
- `lan-mouse/Cargo.toml`

**变更要点**：

- workspace `Cargo.toml` 删 `webrtc-dtls = "0.12.0"` 与 `webrtc-util = "0.11.0"`（BS-1.2 已经删过；本步二次确认）
- 加 `cargo tree | grep -E "webrtc-dtls|webrtc-util"` 自动 guard 测试（与 bak crypto.rs:412 一致）

**验证**：

```bash
cargo tree -p lan-mouse | grep -E "webrtc-dtls|webrtc-util"      # 期望：无输出
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

预期：依赖树干净；workspace 全部测试通过；clippy 无警告。

**依赖**：BS-7.2

---

#### BS-7.4 `connect.rs` 移除 `active_lock` + `ClientManager::probe_targets`（~30 min）

**目标**：删除 active_lock 锁定接口机制与多 IP 探测支持（bak 已删；本步搬运）。

**文件**：`lan-mouse/src/connect.rs`、`lan-mouse/src/client.rs`、`lan-mouse/src/config.rs`

**变更要点**：

- `connect.rs` 中 `active_lock` 分支删除（保留纯 happy-eyeballs 路径）
- `ClientManager` 删 `set_active_lock` / `get_active_lock` / `probe_targets` 方法（如果有）
- `config.toml` schema 删 `active_lock` 字段（如果有）

**验证**：

```bash
cargo build -p lan-mouse
cargo test -p lan-mouse connect::tests client::tests
```

预期：通过。

**依赖**：BS-7.3

---

#### BS-7.5 GUI 移除 `active_lock` 控件（~30 min）

**目标**：GTK 客户端编辑对话框删除 `active_lock` 相关下拉框。

**文件**：`lan-mouse-gtk/src/ui/client_editor.rs`

**变更要点**：删 "锁定到特定接口" 下拉框 + "探测所有接口延迟" 开关（如果有）。

**验证**：手动 GUI：打开 peer 编辑对话框，断言无 active_lock 控件。
**依赖**：BS-7.4

---

#### BS-7.6 `firewall.rs` / `service.rs` 头注释清理（~15 min）

**目标**：旧 `DTLS over UDP` → `QUIC over UDP`，grep 复查。

**文件**：

- `lan-mouse/src/firewall.rs`
- `lan-mouse/src/service.rs`
- `lan-mouse/src/capture.rs` / `emulation.rs`（`Hello.commit` 注释更新为 `Hello.magic + commit`）

**验证**：

```bash
grep -rnE "DTLS|webrtc-dtls|webrtc-util|RECV_IDLE_TIMEOUT" lan-mouse/src lan-mouse-ipc/src lan-mouse-proto/src lan-mouse-gtk/src 2>/dev/null   # 期望：无输出（除了历史 changelog / docs）
```

预期：无 live code 残留。

**依赖**：BS-7.5

---

#### BS-7.7 README / DOC.md / CHANGELOG.md 同步（~30 min）

**目标**：用户能看出 v4 起的传输层变化。

**文件**：`README.md`、`DOC.md`、`CHANGELOG.md`、如有 `README.zh-CN.md` / 翻译版

**变更要点**：

- 删 "DTLS" 段落
- 加段："v4 起传输层基于 QUIC（quinn 0.11），多宿主对端由 happy-eyeballs 自动选最快 IP，鼠标共享由 QUIC datagram 提供，键盘 / 命令由 QUIC stream 提供"
- CHANGELOG `Unreleased` 加条目

**验证**：人工 review。
**依赖**：BS-7.6

---

## 4. 完成定义（DoD）

M1 全部 35 个小步骤完成 + 下列条件全部成立：

1. `cargo build --workspace` 通过
2. `cargo test --workspace` 通过（含 `quic_smoke` + `input_channel_routing` 两个新集成测试）
3. `cargo clippy --workspace --all-targets -- -D warnings` 无警告
4. `cargo tree -p lan-mouse | grep -E "webrtc-dtls|webrtc-util"` 无输出
5. `bash scripts/quic_smoke.sh` 退出码 0
6. IPC / CLI / GTK 公共 API（`lan-mouse-ipc::*` 主要 pub enum / struct）签名保持向后兼容
7. 验收 §0.1 表格中 5 项全部 OK

---

## 5. 决策点（采纳 bak 配置）

| #   | 决策                                       | 默认                                                            |
| --- | ------------------------------------------ | --------------------------------------------------------------- |
| D1  | `PROTOCOL_MAGIC` 取什么                    | `"LANMOUSE"`（保持主仓品牌，不复用 bak 的 `"MOUSEHOP"`）        |
| D2  | `quinn` 版本                               | `0.11`（沿用 bak）                                              |
| D3  | `rustls` provider                          | `ring`（**不要** `aws_lc_rs`，Step 0.1 Windows MSVC NASM 缺失） |
| D4  | `keep_alive_interval` / `max_idle_timeout` | 5s / 30s                                                        |
| D5  | `Hello.magic` 不匹配行为                   | `conn.close(VarInt(0), "hello failed")` + warn（抄 bak）        |
| D6  | `HELLO_TIMEOUT`                            | `Duration::from_secs(3)`（抄 bak）                              |
| D7  | datagram 优先级                            | Motion 永远优先 datagram；其它按 `route_input` 分派             |
| D8  | `input-event` crate 名                     | 保持 `input-event`（不改 `mousehop-input-event`）               |
| D9  | mDNS 服务发现                              | **不引入**（bak 也没启用）                                      |
| D10 | `latency.rs` / `active_lock`               | **不引入** / 删（M1 直接下线）                                  |
| D11 | `webrtc_dtls_compat` feature               | **不引入**（M1 直接下线 DTLS，无回退需求）                      |

---

## 6. 搬运矩阵（速查）

> ✅ = `cp` 后改 crate 名字即可（不动）
> 🔧 = 主仓需要新增 / 重写

| 文件                                    | 处理  | 改动点                                                 |
| --------------------------------------- | ----- | ------------------------------------------------------ |
| `quic_transport.rs` (新建, ~6009 行)    | ✅    | `Mousehop*` → `LanMouse*`                              |
| `connect.rs`                            | 🔧    | 替换 DTLSConn 路径为 PeerSession（BS-6.1）             |
| `listen.rs`                             | 🔧    | 同上（BS-6.2/6.3）                                     |
| `crypto.rs`                             | 🔧    | BS-1.1：返回类型改 rustls                              |
| `service.rs`                            | ✅+🔧 | 适配 `Certificate` 类型替换 + if_watch 改造            |
| `client.rs`                             | 🔧    | 删 `active_lock` / `probe_targets`（BS-7.4）           |
| `config.rs`                             | 🔧    | `input_channels` 字段 + 默认值（BS-4.2）               |
| `firewall.rs`                           | 🔧    | 头部注释 `DTLS → QUIC`（BS-7.6）                       |
| `lan-mouse/src/main.rs`                 | 🔧    | `ring` provider `install_default`                      |
| `lan-mouse/src/lib.rs`                  | 🔧    | `mod quic_transport;` 注册                             |
| `lan-mouse-proto/src/lib.rs`            | 🔧    | `PROTOCOL_MAGIC` + `Hello.magic`（BS-3.1）             |
| `lan-mouse-ipc/src/lib.rs`              | 🔧    | `ChannelMode` + `InputChannelConfig`（BS-4.1）         |
| `lan-mouse-gtk/src/ui/client_editor.rs` | 🔧    | 2 个 ComboBox（BS-4.5）+ 删 active_lock 控件（BS-7.5） |
| `Cargo.toml`（workspace）               | 🔧    | `quinn 0.11`；删 `webrtc-dtls/util`（BS-1.2 + BS-7.3） |
| `config.toml`（根）                     | 🔧    | `input_channels` 示例（BS-4.3）                        |
| 文档：README / DOC.md / CHANGELOG.md    | 🔧    | BS-4.6 / BS-7.7                                        |
| `tests/quic_smoke.rs`                   | ✅    | 抄 bak（BS-7.2）                                       |
| `tests/input_channel_routing.rs`        | ✅    | 抄 bak（BS-7.2）                                       |
| `scripts/quic_smoke.sh`                 | ✅    | 抄 bak（BS-7.2）                                       |

---

## 7. 风险 & 缓解

| 风险                                              | 触发场景                           | 缓解                                                                                |
| ------------------------------------------------- | ---------------------------------- | ----------------------------------------------------------------------------------- | -------------- |
| BS-1.1 后 service.rs 多处类型替换连锁编译错误     | 同时改 `Certificate` 类型 + 调用方 | 若失败 > 5 处，BS-1.1 回退方案启用 `Certificate` 类型别名 24h                       |
| `max_datagram_size` 缓存导致 MTU 探测后仍用旧值   | 抖动手抖                           | 严格遵守 PLAN Step 0.1 结论 D，每次发送前读                                         |
| Windows MSVC `aws_lc_rs` 构建失败                 | 选了错误的 crypto provider         | 已硬编码 `ring` 唯一选项；防退化用 CI `/all-targets` 兜底                           |
| `libc` / `tokio` 版本升级连带破坏                 | 依赖新增 quinn 触发的关联升级      | 锁 version 至与 bak 一致；升级前先跑 bak `mousehop-spike/`                          |
| GTK ComboBox 与现有 client_editor 控件 ID 冲突    | 同时改两处                         | BS-4.5 单独步，先 `cargo build -p lan-mouse-gtk` 验证控件树                         |
| `cargo tree` 显示包了 `webrtc-*`（如 transitive） | 误删某 transitive 依赖             | BS-7.3 加 `cargo tree                                                               | grep` 测试固化 |
| happy-eyeballs 200ms 阈值太小被防火墙丢弃         | 某些企业网络                       | 调整为 200ms (bak 默认)；后续 milestone 评估                                        |
| Hello magic 校验失败 silent 丢连接                | 跨版本互通                         | magic 校验失败时警告日志 + IPC 推 `PeerUntrusted`（**M1 推：日志**，IPC 推送延 M2） |

---

## 8. 时间表

| 段               | BS         | 估时 | 累计       |
| ---------------- | ---------- | ---- | ---------- |
| 第 1 段          | BS-1 (1-4) | 1h45 | 1h45       |
| 第 2 段          | BS-2 (1-7) | 3h45 | 5h30       |
| 第 3 段          | BS-3 (1-2) | 1h   | 6h30       |
| 第 4 段          | BS-4 (1-6) | 2h45 | 9h15       |
| 第 5 段          | BS-5 (1-4) | 2h45 | 12h        |
| 第 6 段          | BS-6 (1-5) | 3h   | 15h        |
| 第 7 段          | BS-7 (1-7) | 3h15 | 18h15      |
| **完成 M1 累计** |            |      | **~18h15** |

> 估算基线：PLAN-v4 Phase 1（~10-12h）+ 当前主仓差异补回（crypto.rs 抽象、Hello.magic、proto 同步等），主要在 BS-1 + BS-3 上。

---

## 9. 不要做的事（M1 阶段守卫）

| 类别                | 不要做                                                                          | 推到           |
| ------------------- | ------------------------------------------------------------------------------- | -------------- |
| `proto` 变体        | `Bounds` / `MotionAbsolute` / `CursorPos` / `ReceiverSensitivity` / `Clipboard` | M2             |
| `proto` 常量        | `MAX_CLIPBOARD_SIZE`                                                            | M2             |
| `proto` 错误        | `ProtocolError::BufferTooLarge`                                                 | M2             |
| `proto` codec       | 变长 `encode_clipboard_event` / `decode_clipboard_event`                        | M2             |
| `input-event`       | `ClipboardEvent` / `Axis::momentum` / `MACOS_KEEP_AWAKE_EVENT_TAG`              | M2             |
| `ipc`               | `TransportEvent` 枚举（任何变体）                                               | M2             |
| `lan-mouse-gtk`     | `status_bar` 任何改动                                                           | M2             |
| `lan-mouse-cli`     | 任何 `stderr` 事件订阅                                                          | M2             |
| `lan-mouse/src/`    | 任何 `clipboard*.rs` 文件                                                       | M2             |
| `Cargo.toml`        | 引入 `h3` / `h3-quinn` / `http`                                                 | M2             |
| `quic_transport.rs` | 开 Stream C reader task                                                         | M2             |
| `connect.rs`        | 任何 mDNS / discovery 改造                                                      | 后续 milestone |

---

## 10. 文档纪律

- 每小步完成 → 在本文件 `STEP x.x.md` 单独落地，**不要在本文档里塞过程日志**
- 跨小步发现的小问题写 SUGGESTION.md，重大问题汇报 LEADER
- 每 BS 完成同步本表"累计时间"
- M1 全部完成后：从 SUGGESTION.md 删除 M1 期间 solved 的项
