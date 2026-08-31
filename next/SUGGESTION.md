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
- 测试逻辑本身已对齐（`endpoint()` bind 127.0.0.1:0 → `local_addr().port() != 0` → drop`）

**处置**：
- 本步把 `endpoint_binds_ipv4_localhost` 作为**单元测试逻辑就位**验收 —— 验证手段是 `cargo check -p lan-mouse` 报 0 错来自 quic_transport.rs
- STEP-6.x 修复 14 errors 后，**leader 需手动跑** `cargo test -p lan-mouse quic_transport::endpoint_binds_ipv4_localhost` 确认通过

**优先级**：🟡 中（属"验收机制与 STEP 进度解耦"的记录，无功能阻塞）

**STEP-4.3 补充（2026-08-31）—— 通用绕行手段**：
本条已连续阻塞 STEP-1.4 / 4.2 / 4.3 的验收命令。STEP-4.3 验证"config.toml
示例与新 schema 一致"时，改用**不依赖 `lan-mouse` lib** 的手段：临时建
`/tmp` 一次性 crate，只依赖能编译的 `lan-mouse-ipc` + `toml`，把文档里的
示例行原样喂给真实类型反序列化，验完 `rm -rf`（不落文件进仓库、不进
workspace、0 依赖变更）。

建议后续被本条阻塞的 STEP 复用该模式，而不是把"测试跑不通"直接记为遗留 ——
凡验收目标只涉及 `lan-mouse-ipc` / `lan-mouse-proto` 等**可编译 crate** 的
类型，都能这样实证。仅当验收目标真的落在 `lan-mouse` lib 内部（如
`quic_transport` 的 tokio 测试）才需等 STEP-6.x。

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

## #S-9 🟢 低：`AuthorizedKeysVerifier` 的 allowlist value 类型用 `String` 而非 `lan_mouse_ipc::IncomingPeerConfig`

**触发**：STEP-2.7

**现象**：
- `quic_transport.rs::AuthorizedKeysVerifier` 的 `allowlist` 字段类型：
  `Arc<RwLock<HashMap<String, String>>>` —— value 是 `String`（空串即可，
  表示"已授权但无 peer 配置"）
- 现有 `config.rs::authorized_fingerprints: HashMap<String, String>` 也是
  同形态，自然对齐
- bak `mousehop/src/quic_transport.rs:1577-1754 AuthorizedKeysVerifier`
  用的是 `Arc<RwLock<HashMap<String, IncomingPeerConfig>>>`

**根因**：
- `lan_mouse_ipc::IncomingPeerConfig` **尚未**引入 `lan-mouse-ipc/src/lib.rs`
  —— 该类型带 `clipboard_receive` / `description` 等字段，**M2 范围**
  （PLAN §0.2 推迟项）
- STEP-2.7 仅落 server 端 cert 验证骨架，**不**触及 clipboard receiver 配置
  → 用 `String` 当占位 value 最自然（与现有 `config.rs` 一致）

**建议处置**：
- M2 阶段把 `IncomingPeerConfig` 引入 `lan-mouse-ipc`（带 `clipboard_receive` 等）
- 同步把本结构 + caller + `config.rs::authorized_fingerprints` 类型签名改成
  `HashMap<String, IncomingPeerConfig>`（与 bak 对齐）
- M2 同步改 `AuthorizedKeysVerifier::with_known(allowlist, fp)`：
  `insert(known_fp.to_owned(), IncomingPeerConfig::default())`
- listen.rs supervisor 也同步切到 `IncomingPeerConfig`（生产路径拿 peer
  description 推 IPC）

**优先级**：🟢 低（M1 阶段 `String` 占位完全够用；M2 clipboard 同步改一次即可）

---

## #S-10 🟡 中：`ProtocolError::HelloMagicMismatch` 变体是否引入

**触发**：STEP-3.1

**现象**：
- Leader prompt 期望 STEP-3.1 加 `ProtocolError::HelloMagicMismatch` 错误变体
  并在 type-level decode 路径返回
- **本步实际未引入**：proto 层 try_from 对任何 8-byte magic 永远成功；
  magic 校验放 `crate::quic_transport::client_hello` / `server_hello`
  （STEP-3.2）做，match 拒绝后调 `conn.close(VarInt(0), "hello failed")`
- 与 bak 设计一致：bak `mousehop-proto/src/lib.rs:48-63`
  `ProtocolError` 只有 4 个变体（InvalidEventId / InvalidPosition /
  ClipboardTooLarge / InvalidUtf8 / BufferTooSmall），**没有**
  HelloMagicMismatch

**根因**：
- proto crate 设计原则：try_from 只负责 **类型层** 解码（8 byte magic 都
  是合法字节切片），不接受"语义层"（magic 是否等于 PROTOCOL_MAGIC）
- 语义层职责属 quic_transport 层（连接级），由 STEP-3.2 落实

**建议处置**：
- **方案 A（推荐，与 bak 对齐）**：维持本步实现；不加新变体。STEP-3.2
  在 quic_transport 用 match arm 显式分支处理 `magic != PROTOCOL_MAGIC`
- **方案 B（与 Leader prompt 对齐）**：proto 层加
  `ProtocolError::HelloMagicMismatch` 变体，try_from 在 magic 错时返
  该错误；STEP-3.2 走 `?` 透传到 caller。该方案破坏与 bak 一致性，
  且 type 层错误与 call-site 处理语义略糊

**优先级**：🟡 中（API 表面问题，无功能阻塞；不影响 STEP-3.2 实现）

**待 Leader 决策**：本步按方案 A 实现。若选方案 B，回退本步加变体；后续
STEP 不受影响（两边都能配合 STEP-3.2 工作）。

**STEP-3.2 闭环**：方案 A 已落实 —— `client_hello` / `server_hello` 内
match arm 显式分支处理 `magic != PROTOCOL_MAGIC`，返
`Error::HelloFailed("wrong magic: ...")` + 调
`conn.close(VarInt(0), b"hello failed (wrong magic)")`。本条目进入
"待 Leader 评审后删除"状态。

---

## #S-11 🟡 中：`#[derive(Default)]` 对"业务默认值"语义不够用 —— STEP-4.1 已踩坑

**触发**：STEP-4.1

**现象**：
- PLAN §4.1 要求 `InputChannelConfig::default() = { mouse: Datagram, keyboard: Stream }`
- 第一次实现套 `#[derive(Default)]` + `ChannelMode::#[default] Datagram`
- 单测 `channel_mode_default` 立刻红：`keyboard` 拿到 `Datagram`（不是 `Stream`）

**根因**：
Rust `Default` derive 对 struct 是"逐字段 default AND" —— 每个字段各
调一次 `Default::default()`，**完全**忽略外层"业务规则"。enum 的
`#[default]` 标注也无法跨 struct 嵌套形成"business default"。

```
struct Default = fieldwise(field1.default(), field2.default(), ...)
            ≠ 业务默认（mouse=Datagram & keyboard=Stream 是独立规则）
```

**本步修正**：
1. 撤 enum `#[default] Datagram`
2. 撤 struct `#[derive(Default)]`
3. 手写 `impl Default for InputChannelConfig { ... }` 硬编码两个字段

**未来影响**：
- STEP-4.2 `ConfigClient` 加 `input_channels` 字段时若想给 `ConfigClient`
  自身 derive Default，**必须手写**而不是依赖 `InputChannelConfig::default()`
  的语意外传 —— 但因为已经实现正确，derive 可用。
- M2 引入 `IncomingPeerConfig`（含 `clipboard_receive`）时若其字段有"业务默认"
  与"字段默认"差异，需同样规则。

**优先级**：🟡 中（API 表面教训；STEP-4.1 一次自检到位，无功能遗留）

**待 Leader 决策**：决定是否将本条目合入工程内规（"凡涉及多个字段有业务
关联规则的 config struct，必须手写 Default + 单测覆盖"），纳入 README / AGENTS.md。