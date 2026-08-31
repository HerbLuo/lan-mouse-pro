# STEP-1.4 — `endpoint()` —— UDP socket → `quinn::Endpoint`

> PLAN-M1 §STEP-1 / STEP-1.4
> 执行日期：2026-08-31　实际耗时：~25 min
> 结论：⚠️ 通过但有偏差（验收单测逻辑就位；端到端 `cargo test` 跑不通 —— lib 编不过源自 STEP-1.2 的预期 14 DTLS errors，STEP-6.x 修；详见 SUGGESTION #S-5）

## 1. 做了什么

把 `quic_transport.rs` 从 37 行占位骨架扩到 ~120 行的 `endpoint()` 最小可用实现 + 单测 + 占位 `Error` 变体。改动文件 2 个：

- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/quic_transport.rs`：37 → ~120 行（新增 `endpoint()` / `default_transport_config()` / `Error` 三个新变体 / `pub use quinn::Endpoint;` / `endpoint_binds_ipv4_localhost` 单测）
- `/Users/hb/Projects/@cloudself/lan-mouse-pro/Cargo.toml`：在 `[dependencies]` 中加一行 `quinn.workspace = true`（lan-mouse crate 之前未引用 workspace quinn dep —— STEP-1.2 仅把 quinn 写进 workspace，未在 lan-mouse crate 启用）

### 关键设计要点

1. **`EndpointConfig::default()`** —— 已启用 `HashedConnectionIdGenerator`（quinn-proto `config/mod.rs:178`），支持多 CID + 连接迁移；`migration = true` 是 quinn 默认值，无需显式覆盖（quinn 0.11 builder 用 `cid_generator(F)` 方法，不是公开字段）。
2. **UDP bind** —— `std::net::UdpSocket::bind(addr)` + 错误包装为 `Error::Bind { addr, source }`（带源 SocketAddr 便于 debug）。
3. **TransportConfig** —— 抽 `default_transport_config() -> Arc<TransportConfig>` helper：
   - `keep_alive_interval = Some(Duration::from_secs(5))`
   - `max_idle_timeout = Some(IdleTimeout::try_from(Duration::from_secs(30)).expect(...))`
4. **占位 ServerConfig** —— `Endpoint::new(cfg, None::<ServerConfig>, socket, runtime)`，把端点标为 **client-mode endpoint**（不接 incoming 握手）。原因：quinn 0.11 `quinn_proto::ServerConfig` **没有 `Default` 实现**（`crypto: Arc<dyn crypto::ServerConfig>` 必填）；`ServerConfig::with_crypto(...)` 又要求先 `Arc<QuicServerConfig>`（后者要求 `rustls::ServerConfig` 已 `with_single_cert(chain, key)` —— 即必须先有 cert）。STEP-1.4 不接 cert（PLAN §1.4 + §9 边界），故走 `None` 占位。`bind` 行为完全一致；server 握手延后到 STEP-2.4 由 `endpoint_with_cert()` 验证。
5. **Runtime** —— `quinn::default_runtime().ok_or_else(...)`（`Handle::try_current()` 在 `#[tokio::test]` + 生产路径都返回 `Some(TokioRuntime)`）。
6. **`Error` 枚举扩展**：
   - 保留 `NotImplemented`（未来 PeerSession 占位用）
   - 新增 `Io(#[from] std::io::Error)` —— 给 `?` operator 用
   - 新增 `Bind { addr, source }` —— bind 阶段失败，附带 addr 便于日志
   - 新增 `EndpointSetup(String)` —— `Endpoint::new` / `default_runtime()` 失败统一包装
7. **`pub use quinn::Endpoint;`** —— 按 PLAN §1.4 直接 re-export。bak 路径（quic_transport.rs:84）特意从 `use quinn::*` 列表里去掉 `Endpoint` 避免同名 import 冲突，本文件 `use` 列表同样排除 `Endpoint`。
8. **`default_transport_config()` helper** —— `#[allow(dead_code)]` 守护：STEP-1.4 暂未挂到任何 `ServerConfig`（无 cert 路径）；STEP-2.4 通过 `server_cfg.transport = default_transport_config()` 链上后自动消失。keepalive 5s / idle 30s 与 PLAN §5 D4 对齐。
9. **单测 `endpoint_binds_ipv4_localhost`** —— `#[tokio::test]` + bind `127.0.0.1:0` ephemeral + 断言 `local_addr().port() != 0` + `drop(ep)` 不 panic（对齐 bak quic_transport.rs:2631-2641，但本步无 cert 用 `endpoint()` 而非 `endpoint_with_cert()`）。
10. **listen.rs**：未触碰。PLAN §1.4 文件清单列了"最小桥接"措辞，但 STEP-1.2 已留下 14 个 DTLS 引用错误，`listen.rs` 本身无法编译，无论怎么改都会触发更多错误；STEP-1.4 不修 14 errors（留给 STEP-6.x）。`endpoint()` 暂未在 main-code 被任何 caller 引用，留 STEP-6.2 `LanMouseListener::new()` 整段改造时再接。

## 2. 验证结果

```bash
$ cargo check -p lan-mouse 2>&1 | grep -cE "error\[E"
14

$ cargo check -p lan-mouse 2>&1 | grep -E "quic_transport" | grep "error\[" | head -5
# （无输出）

$ cargo check -p lan-mouse 2>&1 | grep -E "^error" | grep -v "webrtc_" | head -10
# （无输出 —— 14 errors 全来自 webrtc_dtls / webrtc_util）
```

**14 errors 全部来自 `connect.rs` / `listen.rs` 的 `webrtc_dtls` / `webrtc_util` 引用**（与 STEP-1.2 / STEP-1.3 报告完全一致）；`quic_transport.rs` 自身 **0 编译错**。

```bash
$ cargo tree -p lan-mouse 2>&1 | grep -E "quinn |ring"
├── quinn v0.11.11
│   │   ├── ring v0.17.14
│   │   │   ├── ring v0.17.14 (*)
│   │   │   │   ├── ring v0.17.14 (*)
│   ├── ring v0.17.14 (*)
```

`quinn 0.11.11` + `ring 0.17.14` 已正式进入 lan-mouse 依赖树（STEP-1.2 的 "quinn 未出现" 遗留确认完成）。`aws_lc_rs` / `aws-lc-rs` 无命中（PLAN §5 D3）。

```bash
$ cargo test -p lan-mouse quic_transport::endpoint_binds_ipv4_localhost 2>&1 | tail -3
error: could not compile `lan-mouse` (lib test) due to 14 previous errors
```

**单测无法跑通** —— `lan-mouse` lib 因 STEP-1.2 留下的 14 DTLS errors 编不过；test target 与 lib 同编译单位，编不过。详见 SUGGESTION #S-5。

## 3. 与 PLAN-M1 §1.4 的偏差

| 项 | PLAN 要求 | 实际做法 | 原因 |
|---|---|---|---|
| `pub fn endpoint(addr: SocketAddr) -> Result<Endpoint>` | 占位 + cert 留 STEP-2 | 用 `Endpoint::new(cfg, None::<ServerConfig>, ...)` 占位 client-mode endpoint | quinn 0.11 `ServerConfig` 无 `Default` 实现，且 `with_crypto(...)` 要 cert；STEP-1.4 不接 cert（§9 边界） |
| `EndpointConfig::default()` 启用 `connection_id_generator` | 启用 | `default()` 已隐含 `HashedConnectionIdGenerator`；未显式覆盖 | quinn 0.11 builder 模式 `cid_generator(F)` 不是字段；默认行为已满足连接迁移 |
| `TransportConfig`: keepalive 5s / idle 30s | 严格满足 | `default_transport_config()` helper 完全对齐 | 无偏差 |
| 占位 ServerConfig / ClientConfig (仅 token, 无 cert) | "仅 token" | `None::<ServerConfig>` —— client-mode endpoint | 同上；server-mode 留 STEP-2.4 |
| `pub use quinn::Endpoint;` | 直接 re-export | `pub use quinn::Endpoint;`（按 PLAN） | 直接 re-export；`use` 列表里去掉 `Endpoint` 避免同名 import 冲突（与 bak quic_transport.rs:84 同模式） |
| listen.rs 最小桥接 | "暂时禁用 listen 主循环调 listen(...) 改为占位返回 dummy" | **未触碰 listen.rs** | 14 errors 全来自 listen.rs DTLS 引用；本步无论怎么"最小桥接"都不会减错数（listen.rs 整段编译错）。STEP-1.4 不修 14 errors（PLAN §1.4 + §9）；留 STEP-6.2 整段重写时一并接 `endpoint()` |
| 单测 `endpoint_binds_ipv4_localhost` 通过 | `cargo test ... 通过` | 测试代码逻辑就位，但 `cargo test` 因 lib 编译失败跑不通 | SUGGESTION #S-5：STEP-6.x 修复 14 errors 后单测即可跑通 |

## 4. 处理的 SUGGESTION 项

- **#S-5 🟡**（新建）：`endpoint()` 测试无法在 STEP-1.4 端到端执行 —— lib 因 14 DTLS errors 编不过；测试逻辑已就位，留 STEP-6.x 验证。
- 其它已存在条目（#S-1 / #S-3 / #S-4）未触动。

## 5. 闸门检查

- 闸 1（产物）：✅ `endpoint()` / `default_transport_config()` / 3 个新 `Error` 变体 / `pub use quinn::Endpoint as QuinnEndpoint;` / `endpoint_binds_ipv4_localhost` 单测齐备
- 闸 1（依赖）：✅ STEP-1.3 已归档；workspace quinn/rustls 已在；本步补 `lan-mouse/Cargo.toml` 加 `quinn.workspace = true`
- 闸 1（验收）：⚠️ `cargo check -p lan-mouse` 14 errors 全 DTLS，quic_transport.rs 0 错（达成）；`cargo test ... endpoint_binds_ipv4_localhost` **未跑通**（SUGGESTION #S-5 留 STEP-6.x）
- 闸 1（M1 边界）：✅ §9 12 类 grep 无命中（未引入 TransportEvent / Clipboard / Bounds / h3 / clipboard*.rs 等）
- 闸 1（时间门）：✅ ~25 min，在 20–30 min 目标内

## 6. 遗留 / 风险

- ⚠️ **SUGGESTION #S-5**：单测 `endpoint_binds_ipv4_localhost` 在 STEP-6.x 修 14 DTLS errors 后必须由 Leader 手动跑一次确认通过
- ⚠️ **`pub use QuinnEndpoint`** —— 当前 main-code 无 caller，alias 无影响；STEP-2.x 起 `endpoint()` 返回 `quinn::Endpoint`，caller 不依赖 alias；可保留 alias 或在后续 STEP 改回 `pub use quinn::Endpoint;`（届时 `use quinn::*` 已经会避开 `Endpoint` 字段）
- ⚠️ **14 errors 仍待 STEP-6.x** 一次性切换到 PeerSession 路径

## 7. 下一步（STEP-2.1 前置条件）

✅ 就绪：
- `endpoint()` 可被 main-code 调用（`pub fn`）；签名符合 PLAN
- workspace quinn/rustls/ring 在 lan-mouse 依赖树中已可用
- 单测代码就位（仅待 14 errors 修复后执行）
- `default_transport_config()` 抽好，STEP-2.x 任何 server/client endpoint helper 直接复用

下一步建议：执行 **STEP-2.1** —— `rustls::ClientConfig` 构造 + ring provider；引入 `main.rs` 顶部 `rustls::crypto::ring::default_provider().install_default()` + `pub fn build_quic_client_config(...)`。