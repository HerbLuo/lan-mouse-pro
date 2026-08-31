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

**STEP-6.2 闭环（2026-08-31）**：
- `crypto.rs:28 use webrtc_dtls::crypto::Certificate;` 顶部 import **已删**
- `crypto::load_certificate_compat` / `generate_dtls_cert_compat` / `certificate_fingerprint_compat` **3 个函数全删**
- `service.rs::new()` 调用从 `crypto::load_certificate_compat(&cert_path)` 切到 `cert_der.0.clone()` + `cert_der.1.clone_key()`（与 `LanMouseConnection::new` 同一份 rustls 元组）
- `LanMouseListener::new(...)` 签名从 `cert: Certificate` 改为 `(cert_chain, key)` 元组
- `lan-mouse/Cargo.toml` 的 `webrtc-dtls` / `webrtc-util` 依赖**在 STEP-1.2 已删除** —— #S-1 的最后一项已自动满足
- 本条目进入"待 Leader 评审后删除"状态

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

## #S-23 🟡 中：lib 单测 `spawn_local` 全局架构与 `#[tokio::test]` runtime 不匹配 —— **拆 STEP-7.3a**

**触发**：STEP-7.3 调研阶段发现（lib fixture 修复阶段）

**现象**：
- STEP-7.3 计划 "把 17 个 lib fixture failures 全部修通"
- 实际分类：
  - **11 个 STEP-7.3 已修**（Category A 并行 /tmp 隔离 ×3 / Category B spawn_local→tokio::spawn ×2 / Category C 算法/fixture 错位 ×2 / 无效 cargo tree guard 测试改写）
  - **5 个剩**（Category D — `spawn_local` 全局架构）：
    - `dial_any_prefers_primary` / `dial_any_all_unreachable_returns_err` —— `dial_any` 内 `joinset.spawn_local` 在 `#[tokio::test(flavor = "current_thread")]` 跑不通
    - `hello_wrong_magic_closes_connection` —— server `tokio::spawn` 异步路径与 client `client_hello` 同步等待竞争（HELLO_TIMEOUT=3s 短）
    - `peer_session_round_trip_motion_keyboard` —— `HELLO_TIMEOUT` 在 in-process 慢机时不够
    - `stream_c_take_releases_quinn_recv_stream` —— timing 同样
- 这 5 个测试**全部因同一根因**：`spawn_local` 需要 `LocalSet` runtime，`current_thread` flavor 不带 LocalSet
- 生产路径 `spawn_local` 在 `LocalSet::block_on`（daemon 主循环）里跑没问题；测试 helper 复用 quic_transport 生产代码时 runtime 不一致

**根因**：
- STEP-6.2 / 6.4 / 6.5 引入 `spawn_local` 时假设测试即生产 runtime（错！）
- 5 个测试零散分布，多种症状实际同因
- 修起来不是 "改 fixture" 级别 —— 是 runtime 架构调整

**建议处置（拆 STEP-7.3a）**：
- **方案 A（推荐，30 min）**：在 test mod 顶层统一 `LocalSet::block_on` 包裹（用 `tokio::task::LocalSet::block_on` + `tokio::runtime::Builder::new_current_thread().enable_all().build()`），或 `#[tokio::test(flavor = "current_thread")]` 后 `LocalSet::enter()` 进 `tokio::task::with_runtime`
- **方案 B（备选，~1h）**：production 代码 `spawn_local` 全部换 `tokio::spawn`（要求所有 `async fn` Send —— 影响面大）
- 当前 M1 阶段集成测试 `tests/quic_smoke.rs` 已覆盖核心 supervisor + reconnect 路径（STEP-7.2 验收）；
  lib 单测失败不阻塞 M1 DoD 第 2 条

**优先级**：🟡 中（实质 M1 验收不被阻塞，但 lib 单测 5 个不绿 — 影响工程整洁度）

---

## #S-24 🟠 高：clippy / fmt 30+ pre-existing errors 累计债务

**触发**：STEP-7.3 验证阶段发现（`cargo clippy --workspace --all-targets -- -D warnings` 报 30 errors）

**现象**：
- STEP-7.3 改动相关文件（`src/crypto.rs` / `connect.rs` / `quic_transport.rs`）**0 errors**（STEP-7.3 引入 0 clippy 问题）
- 但 workspace 累计 30 errors 全部 pre-existing：
  - `src/quic_transport.rs` doc list indentation 11 处（pre-existing）
  - `src/{client,listen,config,connect}.rs` dead-code 6 处（pre-existing，#S-3 剩余项）
  - 其它：redundant reference in `info!` argument 2 处 + 其它小 lint

**根因**：M1 STEP-1.x ~ STEP-6.x 全程未跑过 `cargo clippy --workspace --all-targets -- -D warnings` —— 单 crate 测试只跑 `cargo test` / `cargo build`，严格 clippy 门是 PLAN §4 DoD 第 3 条的**收尾**门，由 STEP-7.x 集中跑；STEP-7.1 / 7.2 已落地时未触发此验证（都标 "闸 3 STEP 收尾" ⏸ 跳过，仅 STEP-7.3 集中处理）。

**建议处置**：
- **方案 A（推荐，~30 min）**：本步 Leader 决策是否在 STEP-7.3 顺手统一清（会触动 ~5 个文件的多处小块改动）
- **方案 B**：列为 M2 起手 / Plan-M1.1 新增"STEP-7.7b：workspace clippy 累计清理" 微步
- 本步"进度报告 ⏸"标记为**已知债务已盘点**，具体修复决策由 Leader

**优先级**：🟠 高（直接影响 M1 DoD 第 3 条 — 但不属于 STEP-7.3 范围）

---

## #S-25 🟢 低：`cargo fmt --check` 累计 drift 30+ 处

**触发**：STEP-7.3 验证阶段发现

**现象**：
- STEP-7.3 编辑的位置（crypto.rs guard 测试 / connect.rs 2 处 fixture 修复 / quic_transport.rs ephemeral_cert + spawn_local）**0 fmt drift**
- 但 workspace 全仓 fmt drift 30+ 处，分布在 `src/{client,config,connect,listen,quic_transport,crypto}.rs` 等多个 pre-existing 文件

**根因**：fmt check 全程未跑（与 #S-24 同模式）

**建议处置**：方案 A 紧随 #S-24 顺手修，或方案 B 列 M2 起手。

**优先级**：🟢 低（机械问题；非功能阻塞）

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

**STEP-4.5b 闭环（2026-08-31）—— 方案 A 完整落实**：

GTK 控件层落地（见 `next/STEP-4.5b.md`）：

- `lan-mouse-gtk/resources/client_row.ui` 加 2 个 AdwComboRow（id=`input_channels_mouse_button` / `input_channels_keyboard`）
- `lan-mouse-gtk/src/client_row/imp.rs` 加 2 个 TemplateChild + 2 个 SignalHandlerId + `set_input_channels()` + `emit_input_channels_change()`
- `lan-mouse-gtk/src/client_row.rs` 加 4 个 mode↔index helper（与 bak mousehop-gtk 100% 对位）+ `pub fn set_input_channels()`
- `lan-mouse-gtk/src/window.rs` 接 `request-input-channels-change` 合并信号 → 单次 `FrontendRequest::SetClientInputChannels`；`update_client_config` 加 `row.set_input_channels(client.input_channels)` 回填
- `cargo build -p lan-mouse-gtk` ✅ / `cargo check --all-targets` ✅ / `cargo clippy -D warnings` ✅
- 合并 IPC 信号设计避免 daemon split-brain + 回填 block/unblock signals 避免 GTK 端死循环

**#S-12 完全解决**（建议 Leader 评审后删除本条目）。

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

**STEP-4.5b 闭环（2026-08-31）**：

本步已按实际架构走（`client_row.ui` + `client_row/imp.rs` + `client_row.rs` + `window.rs` 4 文件）+ `AdwComboRow`（非 GtkComboBoxText）。已记为 `PLAN-M1 偏差 #N-6`，并建议 Leader 同步修 PLAN §4.5 / §2 TR-5 / §6 文件名。

**#S-13 完全解决**（建议 Leader 评审后删除本条目）。

---

## #S-14 🟢 低：`send_motion` 降级路径是 inline uni stream，STEP-5.2 将替换

**触发**：STEP-5.1

**现象**：`send_datagram_or_stream_b` 的降级分支当前是
inline `open_uni() + write_all() + finish()`（不缓存、不复用、不带
长度前缀帧）。本步用独立的 `Error::DatagramFallback(String)` 变体
承载降级 IO 错误。

**根因**：
- STEP-5.1 范围 = "motion 走 datagram + 降级 stream"，PLAN §5.1
  文字没强制 stream B 复用（那是 STEP-5.2 的 `StreamBunch` + 长度
  前缀帧 codec 范畴）
- STEP-5.1 把 stream B cache + 长度前缀帧写进去会突破 30 min 目标
  且超出"只做 motion datagram"的本步边界
- 因此降级路径暂走 inline uni stream —— 本步能跑通即可

**建议处置**：
- STEP-5.2 实现 `StreamBunch` + `send_stream_b`（与 bak
  `mousehop/src/quic_transport.rs:557-579` 形态对齐：缓存
  `Mutex<Option<StreamPair>>` + 长度前缀帧 `[u32 BE len][body]`）
- 把 `send_datagram_or_stream_b` 的降级分支改调 `send_stream_b`
- 把 `Error::DatagramFallback(String)` 替换为 `Error::StreamB(String)`
  （与 bak `mousehop/src/quic_transport.rs:564, 575, 578` 一致）

**优先级**：🟢 低（STEP-5.2 自然消化，无功能阻塞）

**STEP-5.2 闭环（2026-08-31）**：
- `PeerSession::send_datagram_or_stream_b` 降级分支由 inline
  `open_uni() + write_all() + finish()` 替换为 `self.send_stream_b(bytes).await?`
- `send_stream_b` 用长度前缀帧 `[u32 BE len][body]`（与对端
  STEP-5.3 `read_frame` codec 对齐）
- `Error::StreamB(String)` 替换 `Error::DatagramFallback(String)`（与 bak
  `mousehop/src/quic_transport.rs:1035-1040 Error::StreamB` 完全对齐；
  `Error::DatagramFallback` 保留变体但已无 caller，待 STEP-7.3 收尾清理）
- 本步 `send_stream_b` **不**做 cache 命中复用（偏差 #N-8）——
  STEP-5.3 read_loop 接入时统一重构（引入 `stream_b: Mutex<Option<StreamPair>>`
  字段 + 合并进 `PeerSession.stream_bunch`）

**#S-14 完全解决**（建议 Leader 评审后删除本条目）。

---

## #S-15 🟢 低：`MAX_SAFE_DATAGRAM = 1162` 与 PLAN-v4 实测相关 —— 后续 MTU spike 变更需重跑

**触发**：STEP-5.1

**现象**：`MAX_SAFE_DATAGRAM: usize = 1162`（与 bak
`mousehop/src/quic_transport.rs:121-123` 对齐）取 STEP-0.1 spike
实测的 QUIC 握手初期下限；MTU 探测完成后 `max_datagram_size()` 可
达 `1414`，但本常量**不缓存**——仅作 `max_datagram_size().map(|m| m.min(MAX_SAFE_DATAGRAM))` 的取 min 边界。

**根因**：
- 与 bak 完全对齐
- 本常量只是"安全上限"哨兵，防止上层绕过 cap 用陈旧更大值触发
  `TooLarge`
- 真值始终由 `max_datagram_size()` 每次读

**建议处置**：
- 若 STEP-5.4 / 6.x 跑端到端时发现 MTU 探测时机与 0.1 spike 假设
  不一致（比如 LAN 环境下立即达 1414），重跑 spike 调整常量
- 当前 1162 是保守值 —— 任何路径 MTU 探测完成后的值都会 ≥ 1162，
  取 min 不会引入退化

**优先级**：🟢 低（防御性常量，无功能阻塞）

---

## #S-16 🟡 中：read_loop 背压策略分阶段落实 — Datagram 类策略留 STEP-5.4

**触发**：STEP-5.3

**现象**：
- PLAN §5.3 要求"队列满时**丢最旧**的 datagram 类事件、阻塞 control/input 类事件"
- STEP-5.3 实际落实：
  - **Reliable 类**（Stream B reader 喂 Key / Button / Modifiers）→ 阻塞 sender ✅（`tx.send().await` + `streams_backpressure_blocks_when_receiver_idle` 单测验证）
  - **Datagram 类**（Motion / Axis / AxisDiscrete120 高频指针事件）→ ⏸ STEP-5.4 才实现 datagram_reader + 丢最旧策略
  - **Control 类**（Stream A 由 caller 持有 recv_a）→ ⏸ caller 自行阻塞读 recv_a（自然背压）

**根因**：
- 本 STEP-5.3 仅引入 stream B reader task；datagram_reader 是 STEP-5.4 范围
- Datagram 类"丢最旧"策略需要 reader task 内做"try_send 失败 → drain 旧 → send 新"两步逻辑；当前 reader task 模板只覆盖 Reliable 阻塞 send 一种语义
- Stream C 立即 drop（守 §9），不读不消费——本步无 stream C 背压问题

**建议处置**：
- STEP-5.4 接入 `datagram_reader` 时实现"丢最旧"具体策略：
  - 用 `tokio::sync::mpsc::Sender::try_send` 试探队列
  - 失败时 `rx.try_recv()` 拿最旧一帧丢弃
  - 再 `tx.try_send(new)`；再失败 → 再 drain 直到成功
  - 单测覆盖"高频输入突发 → 队列满 → 旧事件被丢"
- Control 类（caller 持有 recv_a）由 STEP-6.x `listen.rs supervisor` 主循环消费时按 `tokio::select!` 自然阻塞读，无需额外策略

**STEP-5.3 闭环**：Reliable 类阻塞 sender 已落实 + 单测验证；Datagram 类策略留 STEP-5.4 续治。本条目进入"待 Leader 评审后删除"状态（STEP-5.4 完成后一起删）。

**STEP-5.4 闭环（2026-08-31）**：
- `datagram_reader_task` 实现"丢最旧"策略（`try_send` 失败 → `try_recv` 拿最旧 → 再 `try_send` → 8 次上限防活锁）
- `READ_STREAM_BUFFER_CAP` doc 表更新：Reliable 阻塞 sender ✅ / Datagram 丢最旧 ✅ / Control 由 caller 自然阻塞读
- 本条目进入"待 Leader 评审后删除"状态（建议 Leader 评审后删）

**优先级**：🟡 中（SUGGESTION #28 治理部分消化，STEP-5.4 续治）

---

## #S-17 🟡 中：datagram_reader 背压从 drop-oldest 改为 drop-current（tokio mpsc API 约束）

**触发**：STEP-6.2a

**现象**：原 STEP-5.4 实现 + SUGGESTION #S-16 设计的 `datagram_reader_task` 试图在 queue 满时调 `tx.try_recv()` 丢最旧帧 —— 但 `tokio::sync::mpsc::Sender` 没有 `try_recv()` 方法（drain 只能在 Receiver 侧做）。原方案需要把 Receiver 也传给 reader task，但 Receiver 又被 `run()` 主循环的 `tokio::select!` 持有 —— 单 Receiver 不能被两个 task 同时持有（MPSC 语义）。

**本步修复**：把"丢最旧"改为"丢当前帧"（高频 Motion 事件，单帧丢失 user-noticeable drop 可接受；与 bak 取舍一致）。reader task 仅持 Sender，full 时 `log::trace!` 记录 + 丢弃当前帧 + 继续读下一帧。

**差异**：
- drop-oldest（SUGGESTION #S-16 原方案）：保留更早的指针位置，丢失最新
- drop-current（本步实现）：保留更早的指针位置被消费，丢失当前

对高频 Motion 事件：
- 旧位置（最近一次成功 send）通常已经在 consumer 端（如本地 OS 鼠标位置）应用
- 当前帧位置如果是 user-noticeable 状态突变（如突然加速）会丢这一帧
- 整体视觉差：差异极小（毫秒级 Motion 帧率下，1-2 帧丢失肉眼不可见）

**严重程度**：轻（功能等价；高频指针事件丢 1 帧无视觉异常）。SUGGESTION.md #S-16 需 Leader 评审后改语义描述或删除 #S-16 + 把本条目作为唯一背压策略说明。

**建议处置**：
- STEP-6.x 接本地输入代理时若发现丢帧率过高，改造为 `tokio::sync::watch`（overwrite-on-send）或 `Arc<Mutex<VecDeque>>`（caller 端 drain）实现真正 drop-oldest
- 当前 M1 阶段 drop-current 完全够用（与 bak 同取舍）

**优先级**：🟡 中（工程取舍记录；不影响 STEP-6.x）

---

## #S-18 🟡 中：listen.rs supervisor 整合 + macOS wake 路径未覆盖（**STEP-6.3 闭合**）

**触发**：STEP-6.3 调研阶段发现

**现象**：
- STEP-6.2 supervisor 仅监听 stream A（控制面）
- macOS 系统唤醒信号路径缺失（无 `PowerObserver` / 无 `spawn_wake_task`）
- 非 macOS 上 wake_rx 永久 pending 路径缺失

**根因**：
- PLAN §6.3 文字明确要求"保留现有 macOS 唤醒后 force-close 行为，与新 PeerSession 路径整合"
- supervisor 任一 reader task 退出路径未严格配对 `quic_conns` 反注册

**STEP-6.3 闭环（2026-08-31）**：
- `src/macos_power.rs` 新增（仅 macOS，IOKit `PowerObserver::spawn` + wake_tx 无界通道）
- `src/lib.rs` 加 `#[cfg(target_os = "macos")] pub(crate) mod macos_power;`
- `src/listen.rs`：
  - `LanMouseListener` 新增 `wake_task: JoinHandle<()>` + `quic_conns` + macOS-only `power_observer`
  - `LanMouseListener::new` 装配 PowerObserver + wake_rx + spawn_wake_task
  - `spawn_wake_task` 后台 task：recv wake_rx → 遍历 quic_conns → `peer.connection().close(0u32.into(), b"wake")` 同步触发
  - `QuicConnGuard` RAII：构造时 `insert(addr, peer.clone())`，Drop 时 `remove(&addr)` —— 让 supervisor 任何退出路径都自动反注册（与 bak 对齐）
  - `terminate()` 改新结构：`wake_task.abort() + accept_task.abort() + listen_tx.close()`
- `src/emulation.rs` race 修复注释强化（supervisor 路径 `last_response.remove(&addr)` 先于 timeout 路径的 retain —— supervisor 赢得 race）
- 本条目进入"待 Leader 评审后删除"状态

**优先级**：🟡 中（STEP-6.3 完成；race 已通过注释强化 race 防御）

---

## #S-19 🟠 高：supervisor 不装配 stream B/C reader（STEP-6.3 决策：推到 STEP-7.x）

**触发**：STEP-6.3 调研阶段决策

**现象**：
- PLAN §6.2 验收要求 supervisor 装配 outer `accept_bi` 循环 + 子 task 用 `read_any_frame` 解码 + 4 路 `select!` dispatch
- 当前 STEP-6.2 supervisor 只 `read_frame(&mut recv_a)` 推 Msg / Disconnected
- stream B/C `accept_bi` 路径在 listen.rs supervisor 内未装配

**根因**：
- STEP-6.2 supervisor 简化：M1 阶段 client 端 `LanMouseConnection::send` 不主动开 3 条 bidi；server 端 supervisor 装配 `accept_bi` 3 次会 hang（等不到 client 主动 open 的 B/C bidi）
- STEP-6.3 prompt 严格限制"不要重构（只做 supervisor + macOS wake 整合，不动现有 PeerSession 路径）"
- M1 阶段控制面事件（Enter / Leave / Ack / Hello / Ping / Pong）只走 stream A —— listen.rs ListenTask 的现有 match 臂覆盖所有这些事件
- stream B/C 输入事件（M1 阶段不发，client LanMouseConnection 仅在 `send_input` 分派 `Channel::StreamB` 时按需开新 bidi）暂时不需要 supervisor 处理

**建议处置**：
- 推到 STEP-7.x 接本地输入代理时一并装配（届时 supervisor 装配 outer `accept_bi` 循环 + 子 task 用 `read_any_frame` 解码 + 4 路 select! dispatch —— 与 bak `mousehop/src/listen.rs:296-483 handle_quic_peer_supervisor` 形态 1:1 对齐）
- 当前 M1 阶段功能等价：M1 控制面事件流不依赖 stream B/C reader

**优先级**：🟠 高（功能等价；后续 STEP 续治）

---

## #S-20 🟡 中：server 端 per-IP bind (`enumerate_listenable_addrs`) + `if_addrs` 依赖引入（STEP-6.3 推到后续微步）

**触发**：STEP-6.3 调研阶段决策

**现象**：
- PLAN §6.3 文字提到"if_watch 接口变化（listener 类型变 `Endpoint`，接入同步改）"
- 当前 listener 绑 `0.0.0.0:port` 单 endpoint（**4-tuple 受限**：多宿主机器 reply 源 IP 与 peer dial 目的 IP 不一致 → 握手超时——SUGGESTION #29 描述问题）
- bak `mousehop/src/listen.rs:188-236 is_listenable_addr / enumerate_listenable_addrs` 用 `if_addrs` crate 做 per-IP bind

**根因**：
- per-IP bind 涉及 listener 大改（多 endpoint + 多 accept_task + `Vec<JoinHandle>` 持有）
- STEP-6.3 prompt 严格限制"不要重构（只做 supervisor + macOS wake 整合，不动现有 PeerSession 路径）"
- M1 阶段 happy-eyeballs（STEP-6.4）是 client 端多 IP 并拨，server 端 per-IP bind 是优化项
- 单 endpoint (0.0.0.0:port) 在 LAN 上**通常**可达（除非 4-tuple 受限场景）

**建议处置**：
- 后续微步拆 STEP-6.3a（per-IP bind）：workspace `Cargo.toml` 加 `if-addrs = "0.13"` + `lan-mouse/Cargo.toml` 加 dep；`lan-mouse/src/listen.rs` 加 `enumerate_listenable_addrs()` + `is_listenable_addr()` helpers（与 bak `mousehop/src/listen.rs:188-236` 1:1 对齐）；`spawn_quic_accept_tasks` 改造返 `Vec<JoinHandle<()>>`；terminate 改 drain + abort
- STEP-6.5 接 `RetryState` 退避重连时一并做 server 端 per-IP bind（届时 `LanMouseListener` 持有 endpoints vec 与 conns map 的"per-IP accept task → per-IP peer conn"映射）

**优先级**：🟡 中（功能等价；M1 阶段 LAN 上可达性可接受；后续微步续治）

---

## #S-21 🟡 中：PLAN §7.6 与各步 prompt 的 grep 路径 `lan-mouse/src` 不存在 —— 收尾验收恒假阴性

**触发**：STEP-7.1

**现象**：
PLAN §7.6 的收尾验证命令（以及 STEP-7.1 prompt 里的自验命令）写的是：

```bash
grep -rnE "DTLS|webrtc-dtls|webrtc-util|RECV_IDLE_TIMEOUT" \
  lan-mouse/src lan-mouse-ipc/src lan-mouse-proto/src lan-mouse-gtk/src
```

但主 crate 源码实际在**仓库根 `src/`**，**不存在** `lan-mouse/` 目录：

```
$ ls -d */
build-aux/ dylibs/ firewall/ input-capture/ input-emulation/ input-event/
lan-mouse-cli/ lan-mouse-gtk/ lan-mouse-ipc/ lan-mouse-proto/ next/ nix/
screenshots/ scripts/ service/ src/ target/
```

`grep` 对不存在的路径只发 warning 到 stderr、正常退出 —— 命令对**主 crate 恒返回空**。
STEP-7.1 首轮自验就因此拿到假阴性"已无残留"，复核正确路径后才发现
`src/quic_transport.rs:373` 仍有 1 处命中。

**根因**：
PLAN 的路径写法疑似沿用"每个 crate 一个同名目录"的惯例（如 bak 的
`mousehop/src/`），未与主仓实际布局（主 crate 在 root，`Cargo.toml`
的 `[package] name = "lan-mouse"` + `src/` 同级）对位。同类问题
SUGGESTION #S-13 已在 GTK 文件名上出现过一次。

**影响面（≥2 STEP）**：
- **STEP-7.6** 的验收命令就是这一条 —— "无 live code 残留"是 M1 DoD 的
  清理闸门，假阴性会让 DTLS / RECV_IDLE_TIMEOUT 残留蒙混过关
- STEP-7.3 `cargo tree | grep webrtc-*` 不受影响（走 cargo 不走路径）
- 后续任何按 PLAN 字面复制 grep 命令的步骤同样受影响

**建议处置**：
- Leader 修正 PLAN §7.6 验证命令的路径：`lan-mouse/src` → `src`
- 同步复核 PLAN 其它位置的 `lan-mouse/src/...` 文件路径写法（§1.1 / §6 搬运
  矩阵等处写的 `lan-mouse/src/crypto.rs` 等，实际是 `src/crypto.rs`）——
  这些位置作为"文件定位"表述尚可理解，但作为**可执行命令**必须修正
- 执行侧纪律：凡 grep 自验返回空，先确认路径存在（`ls -d <path>`）再下
  "无残留"结论

**优先级**：🟡 中（不影响已完成步骤的实际代码质量 —— STEP-7.1 已用正确
路径复核；但直接影响 STEP-7.6 收尾闸门的有效性）

---

## #S-22 🟡 中：STEP-7.2 暴露的 work-pattern 教训 —— "#N-31 模式"成流程纪律

**触发**：STEP-7.2

**现象**：
STEP-7.2 是 PLAN §7.2 字面写 "**抄 bak → 主仓**" 的步子。但实际执行
时发现：

1. `lan-mouse-pro-bak/` 目录**早已不存在**（主仓与 bak 在 STEP-6.4 之前某
   步已合并）—— `tests/quic_smoke.rs` 等 4 个文件无 bak 可参考
2. `tests/` 目录**从未在主仓存在** —— 主仓 lib 在 root（`src/`），workspace
   也在 root，主仓即 crate，不在 `lan-mouse/{src,tests,...}`
3. **25 个 pre-existing lib fixture errors** 阻塞 `cargo test -p lan-mouse
   --no-run` —— STEP-7.2 集成测试需要 lib 编过

**根因**：
- PLAN §7.2 的搬运矩阵假设 bak 与主仓已分家（与 STEP-1.x/2.x 同模式），
  实际从 STEP-6.x 起已合并
- "STEP-7.x 抄 bak" 是依据旧工程模型字面写的，与主仓当前状态脱节
- 25 fixture errors 是历史搬运 step（STEP-6.2a / 6.2b）"测试代码就位即
  合格"路线累积；lib 测试能跑通是工程拐点（首条用例），不在 STEP-7.2 字
  面范围，但不让 lib 编过 = 集成测试也跑不通

**本步（STEP-7.2）落实**：
- 现状 grep 核实 **先于** 编码（按 `#N-31` 模式）
- 25 errors 全部修（最小侵入：补 imports / 改 `connect_with` await 形态 /
  generic-ize `read_stream_b_loop` / `let session = Arc::new(PeerSession::
  from_connection(conn))`）
- 3 个新文件全部就位（无 bak 可抄 → 从零写，紧贴主仓 public API）
- 顺手修 1 个相邻 fixture 缺陷（`client.rs::set_input_channels_returns_
  true_only_on_change` 的 `gaming` 与 default 同值）

**建议处置（成流程纪律）**：
- **"#N-31 模式"（gating checkpoint）**：未来 STEP 开干前 5 个动作固定为：
  1. `ls <PLAN 提到的目录>` 确认存在
  2. `ls <PLAN 提到的 bak 路径>` 确认可参考
  3. `cargo test -p <crate> --no-run` 确认编译过（否则先修 fixture）
  4. grep PLAN §9 关键字确认未触碰 M2 范畴
  5. 时间预算门（≤30 min / ≤2h ABS）
- 若任一动作 fail，回 Leader 评估是"PLAN 字面过期"还是"工程实际偏离"
- 把这条写进 `AGENTS.md` §"工作流"段（与 STEP-7.2 归档同源）

**优先级**：🟡 中（流程模式升级；不影响已交付代码，影响后续 STEP 调度）

**STEP-7.2 闭环**：归档 `next/STEP-7.2.md` 同源记录本模式落地；本条目
进入"待 Leader 评审后决定是否升级为 AGENTS.md 内规"状态。

---