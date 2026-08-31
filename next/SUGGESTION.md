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

---

## #S-12 🟠 高：STEP-4.5 前置 IPC 链路缺失 —— GTK 下拉框无处可写（**流程性问题，已停工待决策**）

**触发**：STEP-4.5（执行前闸 1 调研阶段发现，**未写任何代码**）

**现象**：
STEP-4.5 要求"打开已有 peer 时回填下拉值；保存写回 `ClientConfig`"。但
主仓 `lan_mouse_ipc::ClientConfig`（lib.rs:132-143）**没有** `input_channels`
字段，`FrontendRequest`（lib.rs:308-343）**没有** `SetClientInputChannels`
变体。GTK 侧拿不到值也送不出值。

**关键区分**（易混淆，STEP-4.2 归档也没点破）：

| 类型 | 位置 | 有 `input_channels`？ |
|---|---|---|
| `config::ConfigClient` | `src/config.rs:277` | ✅ STEP-4.2 已加（**磁盘/内存 config**） |
| `ipc::ClientConfig` | `lan-mouse-ipc/src/lib.rs:132` | ❌ **缺** —— GTK 实际读的是这个 |

`src/client.rs:30 add_with_config()` 把 `ConfigClient` 转成 `ClientConfig`
时，`input_channels` 字段直接被丢弃（转换体里没这一行）。所以 STEP-4.2 落
的 schema 目前是"只进磁盘、不进运行时"的半条链路。

**根因**：
STEP-4.1 按 PLAN §4.1 字面只加了 `ChannelMode` + `InputChannelConfig` 两个
裸类型，PLAN 未把"IPC 传输链路"单列成小步。bak 的对应实现需要 3 件（见下），
其中 2 件落在 `lan-mouse-ipc`，1 件落在 `src/service.rs`。

**bak 对位（已验证可搬）**：
- `mousehop-ipc/src/lib.rs:453` `pub input_channels: InputChannelConfig` +
  `#[serde(default)]`（后兼容：旧 daemon 配置反序列化不丢字段）
- `mousehop-ipc/src/lib.rs:770` `FrontendRequest::SetClientInputChannels(ClientHandle, InputChannelConfig)`
- `mousehop/src/service.rs:411-413` 处理臂 → `update_input_channels(handle, cfg)`
- GTK 侧：`client_row.ui` 两个 `AdwComboRow` + `imp.rs` block/unblock 信号 +
  `window.rs:330-352` 单信号 `request-input-channels-change`（**两个下拉合并
  发一次 IPC**，避免 daemon 侧 split-brain）

**建议处置（三选一，待 Leader 定）**：
- **方案 A（推荐）**：把 STEP-4.5 拆 `4.5a`（IPC 链路 3 件 + client.rs 透传）
  / `4.5b`（GTK 两个 AdwComboRow + 回填/写回）。每子步 ≤ 35min，端到端可用。
  跨 crate 但完全落在 PLAN §0.1 "鼠标 button/键盘 stream-or-datagram 可切换"
  验收项内，**不触碰 §9 任一条**（尤其不加 `TransportEvent`）。
- **方案 B**：只做 GTK 控件（哑控件，回填恒 default、改动不落盘），IPC 留后。
  风险：交付一个用户可见但无效的控件，且 STEP-4.6 文档会描述不存在的行为。
- **方案 C**：Leader 在 PLAN-M1.md 新增小步 `STEP-4.1b`（IPC 链路），先执行
  4.1b 再回 4.5。步子最干净，但需 Leader 改 PLAN（只读文档）。

**优先级**：🟠 高（阻塞 STEP-4.5 + 影响 STEP-4.6 文档措辞，共 ≥2 STEP）

---

**STEP-4.5a 闭环（2026-08-31）—— 方案 A 已落实 4.5a 部分**：

- `lan-mouse-ipc/src/lib.rs:131-156` `ClientConfig.input_channels` + `#[serde(default)]` ✅
- `lan-mouse-ipc/src/lib.rs:341-345` `FrontendRequest::SetClientInputChannels` ✅
- `lan-mouse/src/service.rs` `update_input_channels` + 处理臂 ✅
- `lan-mouse/src/client.rs:30-50 add_with_config()` 透传 `input_channels` ✅（半条链路 bug 修复）
- `lan-mouse/src/service.rs save_config()` 反向透传 ✅（闭合 loop）
- 2 个 ipc 测试（向后兼容 + round-trip）✅
- 2 个 client 测试（透传 + setter 契约）✅ — 待 STEP-6.x 跑通

剩余：GTK 控件层（`client_row.ui` 两个 AdwComboRow + 回填/写回）→ STEP-4.5b。
本条目进入"待 Leader 评审后删除"状态（建议 4.5b 完成后一起删）。

---

## #S-13 🟡 中：PLAN §4.5 的文件名与控件类型与实际代码不符

**触发**：STEP-4.5

**现象**：
PLAN §4.5 / §2 TR-5 / §6 搬运矩阵均写 `lan-mouse-gtk/src/ui/client_editor.rs`
+ `ComboBoxText`。实际：

- **主仓与 bak 都没有** `src/ui/` 目录，也没有 `client_editor.rs`
- peer 编辑 UI 实际是 `resources/client_row.ui`（`AdwExpanderRow` 模板）
  + `src/client_row/imp.rs`（`CompositeTemplate`）+ `src/client_row.rs`
- 既有 `position` 下拉用的是 **`AdwComboRow`**（client_row.ui:58），
  bak 的两个 channel 下拉同样用 `AdwComboRow`（bak client_row.ui:98/112）

**根因**：PLAN §4.5 的文件名/控件名疑似沿用更早期草稿或凭印象所写，未与
实际 GTK 架构对位。

**建议处置**：按实际架构走 `client_row` + `AdwComboRow`——
1. 与既有 `position` 下拉风格一致（同一个 AdwExpanderRow 内的行内下拉）
2. 与 bak 参考实现 100% 对位，可直接搬运
3. `GtkComboBoxText` 在 libadwaita 场景已 deprecated，且不适合放进
   `AdwExpanderRow` 的行式布局

Leader 确认后由我在 STEP-4.5.md 记为 PLAN-M1 偏差归档；建议 Leader 同步
修正 PLAN-M1.md §4.5 / §2 TR-5 / §6 三处文件名。

**优先级**：🟡 中（不影响功能，但 PLAN 与代码不一致会误导后续 STEP-7.5
"GUI 移除 active_lock 控件"——那一步同样写的是 `client_editor.rs`）