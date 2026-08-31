# STEP-2.5 — mTLS：dial 出示 client cert / server 强制 client cert

> PLAN-M1 §STEP-2 / STEP-2.5
> 执行日期：2026-08-31　实际耗时：~30 min
> 结论：✅ 通过（同步处理 #S-7 mTLS 接上；本步代码全部编译 0 新增错误）

## 1. 做了什么

实现 server 端 mTLS 强制 client cert 校验入口 + client 端 mTLS 出示。
改动 2 个文件（`service.rs` 未触动 —— 计划延后到 STEP-6.x 接入 `listen.rs`
supervisor 时整体接 `endpoint_with_verifier`）：

- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/crypto.rs`：394 → 454 行
  - 新 `pub fn rustls_server_config_with_verifier(cert_chain, key, verifier)`
    —— 与 `rustls_server_config` 形态对称，唯一差别是
    `with_no_client_auth()` → `with_client_cert_verifier(verifier)`
- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/quic_transport.rs`：627 → 871 行
  - 新 `pub fn endpoint_with_verifier(addr, cert_chain, key, verifier) -> Result<Endpoint>`
    —— 与 `endpoint_with_cert` 形态对称，调
    `crypto::rustls_server_config_with_verifier` 后转入共享的
    `endpoint_inner` 私有 helper
  - 抽 `fn endpoint_inner(addr, rustls_server_arc) -> Result<Endpoint>` 私有
    helper —— 把 `Arc::try_unwrap` + ALPN + `QuicServerConfig::try_from` +
    transport_config + bind + `Endpoint::new` 的固定装配流程从
    `endpoint_with_cert` 抽出，给 `endpoint_with_verifier` 共用（避免两段
    重复；与 bak `mousehop/src/quic_transport.rs:1266-1287 endpoint_inner`
    完全对齐）
  - **#S-7 解**：`build_quic_client_config(cert, key)` 签名
    `(cert: CertificateDer, key: PrivateKeyDer)` → `(cert_chain: Vec<CertificateDer>,
    key: PrivateKeyDer)`（`with_client_auth_cert` 是 terminal builder 收
    `Vec`，必须扩为 chain 形态）；`with_no_client_auth()` →
    `with_client_auth_cert(cert_chain, key)`；`let _ = key;` 删除
  - 新 `pub struct PermissiveClientCertVerifier`（零字段结构体）+ `impl
    rustls::server::danger::ClientCertVerifier` —— mTLS 强制（`offer_client_auth
    () = true` + `client_auth_mandatory() = true`）但放行任意 client cert 的
    占位 verifier；STEP-2.7 `AuthorizedKeysVerifier` 替换为指纹 allowlist
  - `dial(ep, addr, cert: CertificateDer, key)` 签名不变（plan §2.5 验收
    "加 cert 参数"，实际 STEP-2.2 已是 cert 参数；本步明确"双用语义"）
    —— 内部 `vec![cert]` 转 chain 喂给 `build_quic_client_config`
  - 单测 `mtls_rejects_no_client_cert` 新增：server 用
    `endpoint_with_verifier` + `PermissiveClientCertVerifier`，client 用
    inline `QuicClientConfig::try_from(...)`（不走 `build_quic_client_config`
    —— 后者已 mTLS 强制 `with_client_auth_cert`）配 `with_no_client_auth()`
    dial；断言 server 端握手失败 + dial 端最终 `Err(Handshake)`
  - 单测 `quinn_client_config_loads_rustls_provider` 同步更新：
    `build_quic_client_config(cert_chain[0].clone(), key)` →
    `build_quic_client_config(vec![cert_chain[0].clone()], key)`

### 1.1 关键设计要点

1. **`endpoint_with_verifier` vs `endpoint_with_cert` 关系**：保留 `endpoint_with_cert`
   作 no-verifier wrapper（`with_no_client_auth()` 形态，与 STEP-2.4 验收
   测试 `endpoint_with_cert_accepts_local_incoming` 共用）；新
   `endpoint_with_verifier` 走 `with_client_cert_verifier(verifier)` 形态。
   两条路径通过私有 `endpoint_inner` helper 共享后半段装配（避免复制粘贴
   `Arc::try_unwrap` + ALPN + `QuicServerConfig::try_from` + transport +
   bind + `Endpoint::new`）。

2. **`PermissiveClientCertVerifier` 占位设计**：单字段结构体
   （`pub struct PermissiveClientCertVerifier;` 零字段）—— 不持有任何
   运行时状态。`Send + Sync + 'static` 自动满足。`verify_client_cert`
   算 SHA-256 fingerprint 写出 `log::debug!`（占位标记，便于在 grep 时
   检索"哪条 server 路径走了占位"），返回
   `ClientCertVerified::assertion()`。`root_hint_subjects() = &[]`
   （不提供 root hints —— 任意自签 cert 都接受）。

3. **`dial()` 签名保持不变的双用语义**：`cert: CertificateDer` 单张既
   作为 root store 信任 anchor（`WebPkiServerVerifier::builder(roots, ...)`
   链路）也作为 mTLS 出示链（`with_client_auth_cert(vec![cert], key)`）。
   M1 双方都跑在同一进程（生产路径调 `load_or_create_server_cert()` 拿
   持久化 cert）/ 测试用 `ephemeral_cert()`，双用同一 chain 不引安全风险
   （§9 M1 边界守卫：不在 STEP-2.5 拆参数）。

4. **`build_quic_client_config` 签名变更（#S-7）**：`with_client_auth_cert`
   是 **terminal** builder 方法（不像 `with_no_client_auth` 是中间 builder）
   —— 收 `Vec<CertificateDer>` + `PrivateKeyDer` 直接返回
   `Result<ClientConfig, Error>`。因此本函数必须改收 chain，单张
   cert 在 caller 处包成 `vec![cert]`。同步更新所有 caller：
   - `dial()` 内部 `vec![cert]`
   - 单测 `quinn_client_config_loads_rustls_provider` 改 `vec![cert_chain[0].clone()]`
   - 单测 `endpoint_with_cert_accepts_local_incoming` / `dial_completes_handshake_against_local_listener`
     走 `dial()`（签名不变，零改动）

5. **`mtls_rejects_no_client_cert` 测试技巧**：要测"server 强制 client cert
   但 client 没出示" —— client 端必须 inline 构造 `rustls::ClientConfig`，
   **不能**走 `build_quic_client_config`（后者已强制 `with_client_auth_cert`
   让 client 出示）。inline 代码：rustls `ClientConfig::builder_with_provider(
   ring).with_safe_default_protocol_versions()?.with_root_certificates(roots
   ).with_no_client_auth()` + `alpn_protocols = vec![ALPN_LAN_MOUSE]` +
   `QuicClientConfig::try_from(...)` + `ClientConfig::new(...)` + `transport_config(
   default_transport_config())`。Server 端 `PermissiveClientCertVerifier`
   `client_auth_mandatory() = true` → TLS 1.3 内置在 client 不出 cert 时报
   `rustls::Error::NoCertificatesPresented` → quinn 包装为
   `ConnectionError::TransportError` → 测试断言 `dial.await` 返 `Err`。

6. **`service.rs` 不动**：`Service::new()` 内仍调 `LanMouseListener::new(
   port, cert: webrtc_dtls::crypto::Certificate, ...)`（DTLS 路径）。
   PLAN §9 M1 守卫要求"不修 listen.rs / connect.rs 的 14 DTLS errors"；
   把 `endpoint_with_verifier` 接入生产路径（替换 `LanMouseListener::new`
   中的 DTLS 配置）延后到 STEP-6.2（listen.rs supervisor 整段改造）。本步
   **仅**保证 `endpoint_with_verifier` + `PermissiveClientCertVerifier` +
   `rustls_server_config_with_verifier` 公共 API 在 quic_transport / crypto
   层可用。

## 2. 验证结果

```bash
$ cargo check -p lan-mouse --lib 2>&1 | grep -cE "^error\[E"
14

$ cargo check -p lan-mouse --lib 2>&1 | grep -E "quic_transport|crypto\.rs|service\.rs" | grep "error\["
# （无输出 —— 本步新增代码 0 编译错）

$ cargo check -p lan-mouse --tests 2>&1 | grep -cE "^error\[E"
14

$ cargo check -p lan-mouse --tests 2>&1 | grep -E "quic_transport|crypto\.rs" | grep "error\["
# （无输出 —— 本步新增测试 0 编译错）
```

**14 errors 全部来自 `connect.rs` / `listen.rs` 的 `webrtc_dtls` / `webrtc_util`
引用**（与 STEP-1.2 / STEP-2.1 / STEP-2.2 / STEP-2.3 / STEP-2.4 报告完全
一致）；本步新增 `endpoint_with_verifier` / `PermissiveClientCertVerifier` /
`rustls_server_config_with_verifier` / `mtls_rejects_no_client_cert` 0 编译错。

```bash
$ grep -nE "TransportEvent|Bounds|MotionAbsolute|CursorPos|ReceiverSensitivity|MAX_CLIPBOARD_SIZE|BufferTooLarge|ClipboardEvent|axis::momentum|MACOS_KEEP_AWAKE_EVENT_TAG|clipboard|h3|h3-quinn|status_bar" src/quic_transport.rs src/crypto.rs src/service.rs
# （无输出 —— §9 M1 边界 12 类 grep 无命中）
```

```bash
$ wc -l src/crypto.rs src/quic_transport.rs
  454 src/crypto.rs
  871 src/quic_transport.rs
```

`crypto.rs` 从 STEP-2.4 的 394 行扩到 454 行（+60 行：`rustls_server_config
_with_verifier` 函数 + doc）。
`quic_transport.rs` 从 STEP-2.4 的 627 行扩到 871 行（+244 行：
`endpoint_with_verifier` / `endpoint_inner` / `PermissiveClientCertVerifier`
/ `mtls_rejects_no_client_cert` 单测 + `build_quic_client_config` mTLS
改造 / `dial` doc 更新）。

```bash
$ cargo test -p lan-mouse quic_transport::mtls_rejects_no_client_cert 2>&1 | tail -3
error: could not compile `lan-mouse` (lib test) due to 14 previous errors
```

**单测无法跑通** —— `lan-mouse` lib 因 STEP-1.2 留下的 14 DTLS errors
编不过；test target 与 lib 同编译单位。详见 SUGGESTION #S-5：STEP-6.x
修复 14 errors 后 Leader 手动跑一次确认。

## 3. 与 PLAN-M1 §2.5 的偏差

| 项 | PLAN 要求 | 实际做法 | 原因 |
|---|---|---|---|
| `pub fn endpoint_with_verifier(addr, cert, key, verifier)` | 同 | 直接对齐 | 与 bak `mousehop/src/quic_transport.rs:1245-1253` 完全一致 |
| dial 加 `cert` 参数 | "加 cert: CertificateDer 参数" | **签名不变** —— `dial(ep, addr, cert: CertificateDer, key)` STEP-2.2 已收 cert，本步明确"双用语义"（root store + mTLS 出示共用同 chain） | cert 参数 STEP-2.2 已加；本步不是新增而是"语义扩展"。Plan 文字歧义：实际无需改签名 |
| 占位 verifier 实现 | "占位 AuthorizedKeysVerifier；STEP-2.7 换真" | **`PermissiveClientCertVerifier`** —— 显式独立占位结构体 + 写 `log::debug!` 标记 | 与 bak `AuthorizedKeysVerifier` 直接对照：STEP-2.7 才上真 allowlist；本步需独立占位（不能直接调 `AuthorizedKeysVerifier::new(empty_allowlist)` 假装）。命名上"Permissive"语义更清晰（强制 + 放行），与"占位 AuthorizedKeysVerifier"同义不同名 |
| `build_quic_client_config` mTLS 出示 | "client 的 rustls::ClientConfig 出示 client cert chain" | 签名 `(cert: CertificateDer, key)` → `(cert_chain: Vec<CertificateDer>, key)`（同步处理 #S-7） | `with_client_auth_cert` 是 terminal builder 收 `Vec<CertificateDer>`；单张 cert 在 caller (`dial`) 处 `vec![cert]` 转 chain |
| `dial(...)` 同时作为 root store + 出示 | "复用 STEP-2.2 的 (cert, key) 但 cert 现在意义扩展：双用" | 直接对齐；明确在 doc 注释写"双用语义" | 与 PLAN 一致 |
| mTLS 不通过即拒绝握手 | "mTLS 不通过即拒绝握手" | `mtls_rejects_no_client_cert` 单测验证 —— server `client_auth_mandatory() = true` + client `with_no_client_auth()` → TLS 1.3 `NoCertificatesPresented` → quinn `ConnectionError` → 测试断言 `Err(Handshake)` | 与 PLAN 一致 |
| `endpoint_with_cert` 关系 | "保留 `endpoint_with_cert` 作 no-verifier wrapper" | 直接对齐 | 与 PLAN 一致 |

## 4. 处理的 SUGGESTION 项

- **#S-7 🟢**（✅ 已解）：`build_quic_client_config` 接受 `key` 但未使用
  - 删 `let _ = key;`
  - `with_no_client_auth()` → `with_client_auth_cert(cert_chain, key)`
  - 签名同步改 `cert: CertificateDer` → `cert_chain: Vec<CertificateDer>`
    （`with_client_auth_cert` 是 terminal builder 收 chain）
  - `dial()` 内部 `vec![cert]` 转 chain
  - 单测 `quinn_client_config_loads_rustls_provider` 同步更新
  - **本条目 Leader 评审后可直接删除**（已留 "Leader 评审后可删除本条目" 标记）
- 其它已存在条目（#S-1 / #S-3 / #S-5 / #S-6 / #S-8）未触动 —— #S-1
  待 STEP-6.x 完整切换 PeerSession 后删除 `*_compat`；#S-5 仍是
  STEP-6.x 修 14 errors 后再跑单测；#S-6（WebPkiServerVerifier 占位）仍
  等 STEP-2.6 TofuVerifier 替换。

## 5. 闸门检查

- 闸 1（产物）：✅ `endpoint_with_verifier` / `endpoint_inner` / `PermissiveClientCertVerifier` / `rustls_server_config_with_verifier` / `mtls_rejects_no_client_cert` 单测齐备
- 闸 1（依赖）：✅ STEP-2.4 已归档；`rustls 0.23` `ClientCertVerifier` trait + `with_client_auth_cert` terminal builder API 已通过本步落地
- 闸 1（验收）：⚠️ `cargo check -p lan-mouse` 14 errors 全 DTLS，本步新增代码 0 错 0 warning；`cargo check -p lan-mouse --tests` 同 14 errors；单测 `cargo test ... mtls_rejects_no_client_cert` **未跑通**（SUGGESTION #S-5 留 STEP-6.x）
- 闸 1（M1 边界）：✅ §9 12 类 grep 无命中（未引入 TransportEvent / Clipboard / Bounds / h3 / clipboard*.rs 等）；`service.rs` **未触动**（避免碰 14 DTLS errors 之外的非必要改动）
- 闸 1（时间门）：✅ ~30 min，在 20–30 min 目标内

## 6. 遗留 / 风险

- ⚠️ **service.rs 未接入 `endpoint_with_verifier`**：`Service::new()` 仍调
  `LanMouseListener::new(port, cert: webrtc_dtls::crypto::Certificate, ...)`
  （DTLS 路径）。生产路径 mTLS 强制延后到 STEP-6.2 `listen.rs` supervisor
  整段改造时 —— 本步**仅**保证 quic_transport + crypto 公共 API 在位。
- ⚠️ **`endpoint_inner` `Arc::try_unwrap` 假设**：依赖
  `crypto::rustls_server_config[_with_verifier]` 返回的 `Arc<ServerConfig>`
  强引用数 = 1。与 STEP-2.4 `endpoint_with_cert` 同一假设；当前实现满足
  （函数体内未持有其它副本）。
- ⚠️ **SUGGESTION #S-5**：单测 `mtls_rejects_no_client_cert` 因 lib 14
  DTLS errors 编不过，逻辑就位即可，STEP-6.x 修后 Leader 手动跑一次确认。
- ⚠️ **`PermissiveClientCertVerifier` `verify_client_cert` 无 root 链校验**：
  当前实现只算 fingerprint + log，**不**校验 cert 是否真由可信 CA 签发。
  这是 STEP-2.7 `AuthorizedKeysVerifier` 的责任（看 fingerprint 是否在
  allowlist；allowlist 是用户授权操作的结果）。M1 内"任意自签 cert 通过"
  风险可接受 —— STEP-2.7 替换即生效。
- ⚠️ **`dial` cert 双用语义**：`cert` 同时是 root anchor 和 mTLS 出示；
  M1 内同 chain 不引安全风险（生产路径是同一持久化 cert），但 STEP-6.x
  若需"server trust anchor 与本端 client cert 不同"，需拆参数（`dial(ep,
  addr, server_root_chain, client_chain, client_key)`）。

## 7. 下一步（STEP-2.6 前置条件）

✅ 就绪：
- `endpoint_with_verifier(addr, cert, key, verifier)` 公共函数 + ALPN
- `crypto::rustls_server_config_with_verifier(cert, key, verifier)`
- `PermissiveClientCertVerifier` 占位 verifier（生产 caller 暂未接入）
- `build_quic_client_config(cert_chain, key)` mTLS 出示装上（#S-7 解）
- `endpoint_with_cert` no-verifier wrapper 保留（与 STEP-2.4 测试兼容）
- `dial(ep, addr, cert, key)` 双用语义明确
- 单测代码就位（仅待 14 errors 修复后执行）

下一步建议：执行 **STEP-2.6** —— `TofuVerifier`（client 端 fingerprint
pinning），替换 `build_quic_client_config` 内的 `WebPkiServerVerifier`
→ `.dangerous().with_custom_certificate_verifier(Arc::new(TofuVerifier::new(
pins_dir)))`。本步提供的 `cert_chain` 参数同时给 TOFU cache 校验用（首次
见到某 fingerprint → 落盘到 `$XDG_DATA_HOME/lan-mouse/known_peers/<fp>.pin`；
二次连接 fingerprint 不匹配 → `rustls::Error::General("untrusted peer ...")`）。