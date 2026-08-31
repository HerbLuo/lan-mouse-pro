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

## #S-4 🟡 中：cert.pem + key.pem 当前合并为同一文件，STEP-2.4 必须拆开

**触发**：STEP-1.1（Leader 评审补充）

**现象**：`crypto::generate_self_signed(...)` 把 `cert.pem` + `key.pem`
内容合并落盘到同一个文件；`load_or_generate_key_and_cert_der` 也从同一
文件读。`cert_path()` 返回 `cert.pem` 单文件路径。

**PLAN §1.1 验收要求**："ls ~/.local/share/lan-mouse/key.pem  # key
分离持久（与 bak 一致）"。

**处置**：
- STEP-2.4 引入 `key_path()`，与 `cert_path()` 对应返回 `key.pem`
- `generate_self_signed` 落盘拆为：cert → `cert.pem`，key → `key.pem`
- `load_or_generate_key_and_cert_der` 签名扩为
  `(cert_path, key_path)` 或保留单参数但内部由 `(cert_path, key_path())` 推算
- key 文件 0o400 权限保持（已在 generate_self_signed 实现）

**优先级**：🟡 中（与 PLAN 验收清单对齐）
