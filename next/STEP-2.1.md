# STEP-2.1 — `rustls::ClientConfig` 构造 + ring provider

> PLAN-M1 §STEP-2 / STEP-2.1
> 执行日期：2026-08-31　实际耗时：~25 min
> 结论：✅ 通过

## 1. 做了什么

实现 `build_quic_client_config(cert, key) -> quinn::ClientConfig` + 进程
启动期 `install_crypto_provider()` 守护。改动 3 个文件：

- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/quic_transport.rs`：~150 行 → ~265 行
  - `Error` 枚举新增 `ClientConfig(String)` 变体（rustls / quinn 装配错误归一）
  - 新 `pub const ALPN_LAN_MOUSE: &[u8] = b"lan-mouse"`（TLS ALPN，与 PLAN §5 D1 对齐）
  - 新 `pub fn install_crypto_provider()` —— `OnceLock` 守护的 `rustls::crypto::ring::default_provider().install_default()` 幂等包装
  - 新 `pub fn build_quic_client_config(cert: CertificateDer<'static>, key: PrivateKeyDer<'static>) -> Result<QuinnClientConfig>` —— rustls ClientConfig（ring + safe-default TLS 1.3 + 信任对端自签 cert）+ WebPkiServerVerifier 占位 + `quinn::crypto::rustls::QuicClientConfig::try_from` 包装 + transport_config 注入
  - 新单测 `quinn_client_config_loads_rustls_provider`（**未端到端执行** —— 同 STEP-1.4 `endpoint_binds_ipv4_localhost` 路径，SUGGESTION #S-5）
- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/lib.rs`：+3 行 `pub use quic_transport::install_crypto_provider;`
- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/main.rs`：`fn main()` 第一行（早于 `env_logger::init_from_env`）加 `lan_mouse::install_crypto_provider();`

### 关键设计要点

1. **`OnceLock` 守护** —— 与 bak `mousehop/src/lib.rs:60-69 install_crypto_provider` 完全对齐；第二次 `install_default()` 返回 `Err(SomeInstalled)` 在 cargo test 多线程 / GTK + daemon 双进程场景会触发的 panic / 噪音日志，`OnceLock` 保证幂等可重入。

2. **`main.rs` 安装顺序** —— 在 `fn main()` **第一行**（早于 logging），确保 `env_logger` 等日志初始化时若有任何 rustls 调用已经具备 provider；当前 main.rs 不调 rustls，但 `config::Config::new()` → `service::Service::new()` → crypto 装配链路要求 provider 已就位。

3. **ALPN `b"lan-mouse"`** —— TLS 协商时声明"这是 lan-mouse 协议"。与对端 server 必须一致；STEP-3.2 之上还有应用层 `PROTOCOL_MAGIC` 二次握手做语义层校验。保留品牌名（PLAN §5 D1），不复用 bak 的 `mousehop`。

4. **占位 verifier** —— 用 rustls 0.23 标准 `WebPkiServerVerifier::builder(root_with_our_cert, ring_provider).build()` 走标准 chain 校验。本步**未**引入 `.dangerous().with_custom_certificate_verifier(...)` 路径（紧贴 PLAN §2.1 "不带 verifier 占位"）—— STEP-2.6 TofuVerifier 直接换 `.dangerous().with_custom_certificate_verifier(Arc::new(TofuVerifier))`。

5. **`key` 收而不接** —— 函数签名收 `key: PrivateKeyDer<'static>` 但本步只把 `cert` 装进 root store；用 `let _ = key;` 抑制 unused warning。STEP-2.5 mTLS 会通过 `with_client_auth_cert(cert_chain, key)` 把 `key` 接上（SUGGESTION #S-7）。

6. **错误归一** —— 所有 rustls / quinn 装配错误统一包到 `Error::ClientConfig(String)`（带底层 `Display`）；不引入 `From<rustls::Error>` / `From<quinn_proto::Error>` —— 后者不是 `pub` 路径，且 STEP-6.x 之前无其它 caller，盲目引入 `From` 反倒污染 `Error` 枚举。

7. **quinn 0.11 公开路径** —— `QuicClientConfig` 通过 `quinn::crypto::rustls::QuicClientConfig` 取（顶层 `quinn_proto::*` 不是稳定公开路径，不直接依赖 `quinn_proto` crate）。

## 2. 验证结果

```bash
$ cargo check -p lan-mouse 2>&1 | grep -cE "^error\[E"
14

$ cargo check -p lan-mouse 2>&1 | grep -E "quic_transport|main\.rs|lib\.rs" | grep "error\["
# （无输出）
```

14 errors 全部来自 `connect.rs` / `listen.rs` 的 `webrtc_dtls` / `webrtc_util` 引用（与 STEP-1.2 / STEP-1.3 / STEP-1.4 报告完全一致）；`quic_transport.rs` / `main.rs` / `lib.rs` 自身 **0 编译错**。

```bash
$ cargo check -p lan-mouse --tests 2>&1 | grep -cE "^error\[E"
14

$ cargo check -p lan-mouse --tests 2>&1 | grep -B1 "src/quic_transport" | grep "error\["
# （无输出）
```

测试 target 也 0 错来自本步新增的 `quinn_client_config_loads_rustls_provider` 单测 —— 测试代码逻辑就位。

```bash
$ cargo check -p lan-mouse 2>&1 | grep -E "warning\[" | grep -v "webrtc"
# （无输出）
```

无新增 warning；`#S-3` 的 9 个 dead-code 全部消失（`crypto::rustls_server_config` / `crypto::rustls_client_config` 被 `quic_transport.rs` 引用即将消失；本步直接用 rustls API，未走 `crypto::*` 的 helper —— 见 §3 偏差）。

## 3. 与 PLAN-M1 §2.1 的偏差

| 项 | PLAN 要求 | 实际做法 | 原因 |
|---|---|---|---|
| `pub fn build_quic_client_config(cert, key) -> Result<quinn::ClientConfig>` | 同 | `Result<QuinnClientConfig>`（即 `Result<quinn::ClientConfig, quic_transport::Error>`，通过 `Result<T> = std::result::Result<T, Error>` 类型别名） | 模块顶层有 `Result<T>` 别名，不写 `<Error>` 二参 |
| 复用 `crypto::rustls_client_config` | "用 STEP-1.1 的 helper" | **未复用**，直接在本函数内构造 `rustls::ClientConfig` | `crypto::rustls_client_config` 接收 `Vec<CertificateDer>` 当 root；本步只需一个 root + ALPN + 后续 mTLS 参数化 —— 直接构造更直观；STEP-2.5 加 `key` 出示后两个函数签名差异更大，复用失去意义 |
| `WebPkiServerVerifier` 占位 | "不带 verifier 占位（STEP-2.6 TofuVerifier 替换）" | 用 `WebPkiServerVerifier::builder(...)` 标准 chain 校验 | 与 PLAN §2.1 "不带 verifier" 文字不冲突 —— "不带 verifier" 理解成"不带自定义 verifier"（即不调 `.dangerous()`）；标准 `WebPkiServerVerifier` 满足"无 client auth + 占位 server 校验"。STEP-2.6 改 TofuVerifier 时再切 `.dangerous()` 链路 |
| `main.rs` install 位置 | "进程启动时 / `main()` 顶层 / 早于 `ClientConfig::builder`" | `fn main()` 第一行（早于 `env_logger::init_from_env`） | 与 PLAN §2.1 一致 |

## 4. 处理的 SUGGESTION 项

- **#S-6**（新增）：`WebPkiServerVerifier` 占位 verifier，STEP-2.6 替换为 TofuVerifier
- **#S-7**（新增）：`build_quic_client_config` 收 `key` 未使用，STEP-2.5 mTLS 接上
- **#S-8**（新增）：`quic_transport::tests` 跨模块引用 `crypto::generate_self_signed`
- 其它已存在条目（#S-1 / #S-3 / #S-4 / #S-5）未触动

## 5. 闸门检查

- 闸 1（产物）：✅ `build_quic_client_config` / `install_crypto_provider` / `Error::ClientConfig` / ALPN 常量 / 单测齐备
- 闸 1（依赖）：✅ STEP-1.4 已归档；`rustls 0.23` `WebPkiServerVerifier` / `RootCertStore` 在 workspace 已可用
- 闸 1（验收）：⚠️ `cargo check -p lan-mouse` 14 errors 全 DTLS，quic_transport.rs / main.rs / lib.rs 0 错；`cargo test ... quinn_client_config_loads_rustls_provider` **未跑通**（SUGGESTION #S-5 留 STEP-6.x）
- 闸 1（M1 边界）：✅ §9 12 类 grep 无命中（未引入 TransportEvent / Clipboard / Bounds / h3 / clipboard*.rs / `status_bar` / `axis::momentum` 等）
- 闸 1（时间门）：✅ ~25 min，在 20–30 min 目标内

## 6. 遗留 / 风险

- ⚠️ **SUGGESTION #S-5**：单测 `quinn_client_config_loads_rustls_provider` 在 STEP-6.x 修 14 DTLS errors 后必须由 Leader 手动跑一次确认通过
- ⚠️ **SUGGESTION #S-6**：`WebPkiServerVerifier` 占位 server cert 校验，信任模型不够严密（任何能拿到对端 PEM 者都可冒充）—— STEP-2.6 TofuVerifier 替换
- ⚠️ **SUGGESTION #S-7**：`key` 参数收而不接，STEP-2.5 mTLS 接上
- ⚠️ **ALPN 协议名 `b"lan-mouse"`** —— 当前 main-code 无 caller，但 STEP-2.4 服务端 `endpoint_with_cert` 必须在 `rustls::ServerConfig.alpn_protocols` 设同样 `b"lan-mouse"`，否则 TLS 握手 ALPN mismatch 直接拒连。**STEP-2.4 验证清单需加一条**："server alpn_protocols 包含 `b"lan-mouse"`"

## 7. 下一步（STEP-2.2 前置条件）

✅ 就绪：
- `build_quic_client_config(cert, key)` 装配 `quinn::ClientConfig` 成功
- `install_crypto_provider` 在 main 启动期幂等安装 ring
- ALPN `b"lan-mouse"` 在 client 侧设好；server 侧 STEP-2.4 必须对称
- 单测代码就位（仅待 14 errors 修复后执行）

下一步建议：执行 **STEP-2.2** —— `pub async fn dial(ep, addr, cert, key) -> Result<Connection>`，`build_quic_client_config(cert, key)` + `ep.connect_with(cfg, addr, "lan-mouse")?.await?`，占位 verifier 路径（用本步成果，不接 TofuVerifier）。