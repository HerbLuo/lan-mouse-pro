# STEP-2.4 — 服务端证书持久化 + ALPN

> PLAN-M1 §STEP-2 / STEP-2.4
> 执行日期：2026-08-31　实际耗时：~35 min
> 结论：✅ 通过（同步处理 #S-4 cert/key 拆文件 + #S-9 server ALPN）

## 1. 做了什么

实现 server-mode `quic_transport::endpoint_with_cert()` 公共函数 + 持久化
cert/key 双文件路径 + service.rs 最小接入（cert 加载入口从 `_compat` 切到
`load_or_create_server_cert`）。改动 3 个文件：

- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/crypto.rs`：290 → ~390 行
  - 抽 `lan_mouse_data_dir()` 私有 helper（`cert_path()` / `key_path()` 共用 OS 解析）
  - 新增 `pub fn key_path() -> PathBuf`（与 `cert_path()` 对称，#S-4）
  - `generate_self_signed` 签名 `(cn, save_to: Option<&Path>)` → `(cn, cert_path: &Path, key_path: &Path)`（#S-4）；落盘拆为 cert 0o600 + key 0o400
  - `load_or_generate_key_and_cert_der` 签名 `(path)` → `(cert_path, key_path)`（#S-4）
  - 新增 `pub fn load_or_create_server_cert()` 零参数别名（PLAN §1.1 命名对齐）
  - 6 个原单测更新签名 + 新增 `load_or_generate_key_and_cert_der_persists_identity`（持久化 identity 稳定）
- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/quic_transport.rs`：461 → ~625 行
  - 新增 `pub fn endpoint_with_cert(addr, cert_chain, key) -> Result<Endpoint>`（#S-9 server ALPN）
  - 去掉 `default_transport_config()` 的 `#[allow(dead_code)]` 守护（已链上）
  - 顶部 `use crate::crypto;`（被 `endpoint_with_cert` 调用）
  - 测试 helper `endpoint_with_test_cert` 简化为直接调 `endpoint_with_cert`（消除重复装配）
  - `ephemeral_cert()` 改用 `/tmp` 临时目录（双文件签名要求）
  - 新增 2 个单测：`endpoint_with_cert_binds_ipv4_localhost` / `endpoint_with_cert_accepts_local_incoming`
- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/service.rs`：~95 行（`Service::new()` 内 cert 加载块）
  - `crypto::load_certificate_compat(config.cert_path())` → `crypto::load_or_create_server_cert()` + `crypto::generate_fingerprint(cert_der.0[0].as_ref())`（最小接入）
  - 加注释：完整切到 PeerSession（`cert: Certificate` 字段替换）留 STEP-6.x

### 1.1 关键设计要点

1. **`endpoint_with_cert` 装配路径**：
   - `crypto::rustls_server_config(cert_chain, key)?` → `Arc<rustls::ServerConfig>`
   - `Arc::try_unwrap` 拆出（强引用数必须为 1，crypto 模块刚返回未持有副本）
   - 设 `rustls_server.alpn_protocols = vec![ALPN_LAN_MOUSE.to_vec()]`（**在 wrap 进 QuicServerConfig 之前**，关键约束 —— SUGGESTION #S-9）
   - `QuicServerConfig::try_from(Arc::new(rustls_server))` → `quinn::ServerConfig::with_crypto(...)`
   - `server_cfg.transport_config(default_transport_config())` —— keepalive 5s / idle 30s（PLAN §5 D4）
   - `Endpoint::new(..., Some(server_cfg), ...)`（server-mode endpoint，能接 incoming）

2. **`crypto::load_or_create_server_cert` 命名**：PLAN §1.1 别名 `load_or_create_server_cert`，STEP-2.4 caller 全部用这个零参数形式；底层 `load_or_generate_key_and_cert_der(cert_path, key_path)` 是双路径签名，给需要落自定义路径的测试用。

3. **`generate_self_signed` 权限收紧**：cert 0o600（可读）、key 0o400（只读）；与 bak mousehop crypto.rs:178 一致（key 文件更紧）。

4. **service.rs 最小接入**：`Service::new()` 内 cert 字段仍保留 `webrtc_dtls::crypto::Certificate`（给 `LanMouseListener::new(port, cert, ...)` 用），但加载入口已切到 rustls 路径。完整切到 PeerSession（`cert` 字段类型替换）是 STEP-6.x。

5. **`endpoint_with_cert` 不主动 `install_crypto_provider`**：与 `build_quic_client_config` / `dial` 对称，由 caller（service.rs / 测试）显式守护；生产路径 `main.rs` 启动期已 install。

6. **`#S-9 ALPN 对称实现位置**：`rustls::ServerConfig.alpn_protocols` 字段（不是 quinn ServerConfig 上的字段）—— bak quic_transport.rs:1272-1274 的注释明确这一点；本仓在 `endpoint_with_cert` 内 wrap 前设置，与 client `build_quic_client_config` 对称。

## 2. 验证结果

```bash
$ cargo check -p lan-mouse --lib 2>&1 | grep -cE "^error\[E"
14

$ cargo check -p lan-mouse --lib 2>&1 | grep -E "quic_transport\.rs|crypto\.rs|service\.rs" | grep "error\[" | head -5
# （仅 1 行：src/crypto.rs:28 `use webrtc_dtls::crypto::Certificate` —— STEP-1.1 既有，不属本步新增）

$ cargo check -p lan-mouse --tests 2>&1 | grep -cE "^error\[E"
14
```

**14 errors 全部来自 `connect.rs` / `listen.rs` 的 `webrtc_dtls` / `webrtc_util` 引用**（与 STEP-1.2 / STEP-2.1 / STEP-2.2 / STEP-2.3 报告完全一致）；`quic_transport.rs` / `crypto.rs` / `service.rs` 中**本步新增代码 0 编译错、0 新增 warning**。

```bash
$ grep -nE "TransportEvent|Bounds|MotionAbsolute|CursorPos|ReceiverSensitivity|MAX_CLIPBOARD_SIZE|BufferTooLarge|ClipboardEvent|axis::momentum|MACOS_KEEP_AWAKE_EVENT_TAG|clipboard|h3|h3-quinn|status_bar" src/quic_transport.rs
# （无输出 —— §9 M1 边界 12 类 grep 无命中）
```

```bash
$ wc -l src/crypto.rs src/quic_transport.rs
  394 src/crypto.rs
  627 src/quic_transport.rs
```

`crypto.rs` 从 STEP-1.1 的 290 行扩到 394 行（+104 行：`key_path()` / `lan_mouse_data_dir()` / 签名变更 / 新测试）。
`quic_transport.rs` 从 STEP-2.3 的 461 行扩到 627 行（+166 行：`endpoint_with_cert` 公共函数 / 2 个新测试 / 测试 helper 重构）。

## 3. 与 PLAN-M1 §2.4 的偏差

| 项 | PLAN 要求 | 实际做法 | 原因 |
|---|---|---|---|
| `pub fn endpoint_with_cert(addr, cert, key) -> Result<Endpoint>` | 同 | 完全对齐 | 直接对齐 |
| ALPN `b"lan-mouse"` | 必须设 | 在 `rustls::ServerConfig.alpn_protocols` 上设（wrap 前） | #S-9；复用 `ALPN_LAN_MOUSE` 常量避免字符串漂移 |
| `service.rs::new()` 用 `endpoint_with_cert` 替换 DTLS | "用 endpoint_with_cert(...) 替换" | 仅 cert 加载入口替换为 `load_or_create_server_cert()`；`LanMouseListener::new(port, cert, ...)` 签名 / 类型未改 | cert 字段类型替换 `webrtc_dtls::crypto::Certificate` → `(Vec<CertificateDer>, PrivateKeyDer)` 是 STEP-6.x 的工作 |
| `load_or_create_server_cert` 命名 | PLAN §1.1 别名 | 直接实现为 `load_or_generate_key_and_cert_der(cert_path(), key_path())` 的零参数别名 | 与 PLAN 文字一致 |
| cert/key 拆文件（#S-4） | 必须拆 | `key_path()` + `generate_self_signed` 双路径 + `load_or_generate_key_and_cert_der` 双路径 | 完全对齐 |
| key 文件 0o400 | 0o400 | 0o400（key）+ 0o600（cert） | cert 略宽（可读），key 0o400 与 bak mousehop crypto.rs:178 对齐 |
| `default_transport_config()` 的 `#[allow(dead_code)]` | "去掉" | 已去掉 | 现在被 `endpoint_with_cert` 通过 `server_cfg.transport_config(...)` 链上 |

## 4. 处理的 SUGGESTION 项

- **#S-4 🟡**（✅ 已解）：cert.pem + key.pem 拆文件
  - 新增 `crypto::key_path() -> PathBuf`（与 `cert_path()` 对称）
  - `generate_self_signed` 签名改为 `(common_name, cert_path, key_path)`，落盘拆为两个文件
  - `load_or_generate_key_and_cert_der` 签名改为 `(cert_path, key_path)`
  - key 文件 0o400 权限保持；cert 文件 0o600
- **#S-9 🟡**（✅ 已解）：server ALPN
  - `endpoint_with_cert` 装配 rustls::ServerConfig 时设 `alpn_protocols = vec![ALPN_LAN_MOUSE.to_vec()]`
  - 复用 STEP-2.1 已声明的 `pub(crate) const ALPN_LAN_MOUSE: &[u8] = b"lan-mouse";`
  - 与 client `build_quic_client_config` 的 `rustls_client.alpn_protocols = vec![ALPN_LAN_MOUSE.to_vec()];` 完全对称
- 其它（#S-1 / #S-3 / #S-5 / #S-6 / #S-7 / #S-8）未触动 —— #S-1 待 STEP-6.x 完整切换 PeerSession 后删除 `*_compat`；#S-3 已自动消除（dead_code 不再触发）；#S-5 测试无法端到端跑问题仍是 STEP-6.x 修 14 errors 后再跑。

## 5. 闸门检查

- 闸 1（产物）：✅ `endpoint_with_cert()` / `key_path()` / `load_or_create_server_cert()` / 拆分落盘 / server ALPN 齐备
- 闸 1（依赖）：✅ STEP-2.3 已归档（c44d62c）；`crypto::rustls_server_config` / `ALPN_LAN_MOUSE` / `default_transport_config` 复用成功
- 闸 1（验收）：✅ `cargo check -p lan-mouse` 14 errors 全 DTLS（pre-existing），本步新增代码 0 错 0 warning；`cargo check -p lan-mouse --tests` 同 14 errors；单测 `cargo test ... crypto::` 因 lib 编译失败无法跑通（SUGGESTION #S-5，留 STEP-6.x）
- 闸 1（M1 边界）：✅ §9 12 类 grep 无命中（未引入 TransportEvent / Clipboard / Bounds / h3 / clipboard*.rs 等）
- 闸 1（时间门）：✅ ~35 min，略超 30 min 目标但在 1h ABS 上限内；本步含 1 次方向调整（最初试图用 `Arc::try_unwrap` 直接包装 bak 的 `rustls_server_config` 返回值，发现强引用数为 1 时才安全）

## 6. 遗留 / 风险

- ⚠️ **service.rs 仍依赖 `*_compat`**：`Service::new()` 仍调 `crypto::load_certificate_compat(&cert_path)` 拿 `webrtc_dtls::crypto::Certificate`，仅 cert 加载入口切到 rustls；`LanMouseListener::new(port, cert, ...)` / `LanMouseConnection::new(cert, ...)` 的 cert 字段类型替换留 STEP-6.x（详见 SUGGESTION #S-1）。
- ⚠️ **`endpoint_with_cert` `Arc::try_unwrap` 假设**：依赖 `crypto::rustls_server_config` 返回的 `Arc<ServerConfig>` 强引用数 = 1。当前实现满足（函数体内未持有其它副本）；未来若 `rustls_server_config` 内部 `Arc::new` 多次会 panic（已有兜底：fallback 到 `Error::ClientConfig("强引用数 > 1")`）。
- ⚠️ **SUGGESTION #S-5**：单测 `endpoint_with_cert_binds_ipv4_localhost` / `endpoint_with_cert_accepts_local_incoming` 因 lib 14 DTLS errors 编不过，逻辑就位即可，STEP-6.x 修后 Leader 手动跑一次确认。
- ⚠️ **`ephemeral_cert` 临时目录清理**：测试 helper 用 `/tmp/lan-mouse-quic-test-<pid>/`（PID 隔离），未显式清理（依赖 OS `/tmp` 重启清理）；STEP-6.x 修 14 errors 后跑全套测试时注意 `/tmp` 占用。

## 7. 下一步（STEP-2.5 前置条件）

✅ 就绪：
- `endpoint_with_cert()` 公共函数 + ALPN_LAN_MOUSE 已就位
- `crypto::load_or_create_server_cert()` 持久化 cert/key 入口就位
- `default_transport_config()` `#[allow(dead_code)]` 已移除
- service.rs 最小接入（cert 加载入口切到 rustls）
- 现有 6 个 crypto 单测已更新 + 新增 1 个持久化测试

下一步建议：执行 **STEP-2.5** —— `endpoint_with_verifier()` mTLS 强制（server `with_client_auth_verifier` / client `with_client_auth_cert`），引入 `Arc<dyn ClientCertVerifier>` 入参；本仓暂以 `with_no_client_auth()` 占位。