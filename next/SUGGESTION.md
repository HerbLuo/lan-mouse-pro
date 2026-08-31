# SUGGESTION.md — M1 期间小问题 / 后续 todo

> 仅记录 **M1 阶段**发现 / 决定的小问题。每条带触发 STEP + 优先级 🟠🟡⚪。
> M1 全部完成后从本文档删除已解决的项。

---

## #S-1 🟠 高：3 个 `*_compat` 兼容入口必须 STEP-7.3 删除

**触发**：STEP-1.1

**现象**：
- `src/crypto.rs` 顶部有 `use webrtc_dtls::crypto::Certificate;`
- 三个 `pub(crate) fn ..._compat`：`load_certificate_compat` /
  `generate_dtls_cert_compat` / `certificate_fingerprint_compat`
- `service.rs::new()` 仍调用 `crypto::load_certificate_compat(...)` 与
  `crypto::certificate_fingerprint_compat(...)`

**根因**：
`webrtc_dtls::crypto::Certificate` 是一个由 webrtc-dtls 内部封装的复杂
类型，**无法**从 `(Vec<CertificateDer>, PrivateKeyDer)` zero-cost 构造。
完整切到 PeerSession 是 STEP-6.1/6.2 的工作，STEP-1.1 不应承担。

**建议处置**：
- STEP-2.x 接通 rustls API 后，把 `load_certificate_compat` /
  `generate_dtls_cert_compat` 在 `service.rs::new()` 中的调用替换为
  `crypto::load_or_generate_key_and_cert_der()`
- STEP-6.1 / 6.2 整段把 listen.rs / connect.rs 切到 PeerSession
- STEP-7.3 删除 3 个 `*_compat` + 顶部 `use webrtc_dtls::crypto::Certificate;`
  + `lan-mouse/Cargo.toml` 的 `webrtc-dtls` 依赖

**优先级**：🟠 高（阻碍最终依赖清理）

---

## #S-3 🟢 低：dead-code warning 9 处

**触发**：STEP-1.1

**现象**：`cargo check -p lan-mouse --lib` 报 9 个 warning：
- `load_or_generate_key_and_cert_der` / `generate_self_signed` /
  `rustls_server_config` / `rustls_client_config` / `cert_path` /
  `CertificateChain` / `CertKeyPair`
- 2 个内部 helper：未使用

**处置**：STEP-2.1（`build_quic_client_config`）+ STEP-1.4（`endpoint()` 接
`ServerConfig`）+ STEP-2.4（`load_or_create_server_cert`）陆续接通，warning
自动消失。

**优先级**：🟢 低（auto-fade）

---

## #S-5 🟡 中：`endpoint()` 测试无法在 STEP-1.4 端到端执行

**触发**：STEP-1.4

**现象**：
- `quic_transport.rs::endpoint_binds_ipv4_localhost` 编译通过（`cargo check -p lan-mouse` 报 14 errors，**0 来自 quic_transport.rs**）
- 但 `cargo test -p lan-mouse quic_transport::endpoint_binds_ipv4_localhost` 跑不起来 —— `lan-mouse` lib 因 `connect.rs` / `listen.rs` 的 `webrtc_dtls` 引用仍编不过（STEP-1.2 留下的 14 errors）

**根因**：
- PLAN §1.4 验收清单要求 `cargo test ... 通过`，但 §1.2 故意留下 14 errors 让 lib 编不过（铺路给 STEP-6.x 一次性切到 PeerSession）
- STEP-1.4 在不修 connect.rs / listen.rs 的前提下，无法独立让单测跑通
- 测试逻辑本身已对齐（`endpoint()` bind 127.0.0.1:0 → `local_addr().port() != 0` → drop）

**处置**：
- 本步把 `endpoint_binds_ipv4_localhost` 作为**单元测试逻辑就位**验收 —— 验证手段是 `cargo check -p lan-mouse` 报 0 错来自 quic_transport.rs
- STEP-6.x 修复 14 errors 后，**leader 需手动跑** `cargo test -p lan-mouse quic_transport::endpoint_binds_ipv4_localhost` 确认通过

**优先级**：🟡 中（属"验收机制与 STEP 进度解耦"的记录，无功能阻塞）

---

## #S-6 🟢 低：`build_quic_client_config` 当前仅占位 verifier（WebPkiServerVerifier），STEP-2.6 必须替换为 TofuVerifier

**触发**：STEP-2.1

**现象**：
- `build_quic_client_config(cert, key)` 用 `WebPkiServerVerifier::builder(root_with_our_cert, ring_provider).build()`
  做 server cert chain 校验 —— 等同于"信任对端的自签 cert 即放行"
- 真正信任模型应来自 `TofuVerifier`（STEP-2.6）：首次见到某 fingerprint
  落盘到 `$XDG_DATA_HOME/lan-mouse/known_peers/<fp>.pin`；二次连接
  fingerprint 不匹配即 `Err(rustls::Error::General(...))`
- 当前形态：`client 端信任对端的自签 cert` —— 任何能拿到对端 PEM 文件
  的人都可冒充（虽然局域网场景下攻击者拿到 PEM 不容易，但模型不严密）

**根因**：PLAN §2.1 "不带 verifier 占位" 本意是"无 mTLS verifier"（client
端不出示 cert 给 server），不是"无 server cert verifier"。本步用
`WebPkiServerVerifier` 走的是 chain 校验而非 mTLS，正好两边都满足
（无 client auth / 占位 server 校验）；STEP-2.6 改成
`.dangerous().with_custom_certificate_verifier(TofuVerifier::new(...))`
即可。

**处置**：
- 本步**不**修：仍按 PLAN §2.1 文字通过
- STEP-2.6 改 `build_quic_client_config` 调用
  `with_custom_certificate_verifier(Arc::new(TofuVerifier::new(pins_dir)))`
- `quinn_client_config_loads_rustls_provider` 单测继续可用：
  `WebPkiServerVerifier` vs `TofuVerifier` 都让 ClientConfig 装配成功

**优先级**：🟢 低（占位即正确，等 STEP-2.6 替换）

---

## #S-7 🟢 低：`build_quic_client_config` 接受 `key` 但未使用，留 STEP-2.5 mTLS 接上

**触发**：STEP-2.1

**现象**：函数签名 `(cert, key) -> Result<ClientConfig>` 收 `key` 但函数
体只用 `cert` 当 root；用 `let _ = key;` 抑制 unused warning。

**根因**：PLAN §2.1 验收代码样例：
```rust
pub fn build_quic_client_config(cert: CertificateDer, key: PrivateKeyDer)
  -> Result<quinn::ClientConfig>
```
签名明确收 `key` —— STEP-2.5 mTLS（client 端出示 cert）会通过
`with_client_auth_cert(cert_chain, key)` 把 `key` 接上。本步先收着避免
STEP-2.1 改签名、STEP-2.5 再改一次的两段式 churn。

**处置**：STEP-2.5 删 `let _ = key;` 加 `with_client_auth_cert(...)`。

**进度**：STEP-2.5 已 ✅ 解 —— `build_quic_client_config` 签名同时改为
`(cert_chain: Vec<CertificateDer>, key: PrivateKeyDer)`，`with_no_client_auth()`
→ `with_client_auth_cert(cert_chain, key)`，`let _ = key;` 删除。Leader 评审
后可删除本条目。

**优先级**：🟢 低（auto-fade at STEP-2.5）

---

---

## #S-8 🟢 低：`quic_transport.rs::tests` 引用 `crate::crypto::generate_self_signed` —— 跨模块依赖

**触发**：STEP-2.1

**现象**：`quinn_client_config_loads_rustls_provider` 单测从 `crypto` 模块
取测试 cert；与 `crypto::tests::round_trip_generate_and_load` 形成"crypto
→ quic_transport" 与 "quic_transport → crypto" 的双向依赖。

**根因**：STEP-2.1 验收代码样例要求单测"用 STEP-1.1 crypto::generate_self_signed
拿一个测试 cert" —— 直接引用 `crate::crypto::generate_self_signed` 是
最小改动。

**处置**：
- 单测代码本身无问题（不引入循环依赖 —— `crypto` 不引用 `quic_transport`）
- 若未来 STEP-2.6 起 TofuVerifier 单测也要 cert，提取
  `quic_transport::test_utils::self_signed_cert()` 收口（避免每个测试
  各自写一遍 `crypto::generate_self_signed` 调用）

**优先级**：🟢 低（结构整洁，无功能影响）

---

