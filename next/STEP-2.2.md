# STEP-2.2 — `dial()` 完成 QUIC TLS 握手（占位 verifier）

> PLAN-M1 §STEP-2 / STEP-2.2
> 执行日期：2026-08-31　实际耗时：~25 min
> 结论：⚠️ 通过但有偏差（验收单测逻辑就位；端到端 `cargo test` 跑不通 —— lib
> 因 STEP-1.2 留下的 14 DTLS errors 编不过，留 STEP-6.x；详见 SUGGESTION #S-5）

## 1. 做了什么

实现 `pub async fn dial(ep, addr, cert, key) -> Result<Connection>`，
复用 STEP-2.1 `build_quic_client_config` 装配 `quinn::ClientConfig`，再
`ep.connect_with(cfg, addr, "lan-mouse")?.await?` 完成 TLS 1.3 握手。
改动 1 个文件：

- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/quic_transport.rs`：
  ~281 行 → ~395 行
  - `pub use quinn::{Connection, Endpoint};` —— 把 `Connection` 加到 re-export
    列表（与 STEP-1.4 `Endpoint` 同模式，从 `use` 列表里也排除 `Connection`
    避免同名 import 冲突）
  - `Error` 枚举新增两个变体：
    - `Connect(#[from] quinn::ConnectError)` —— `Endpoint::connect_with` 同步失败
    - `Handshake(#[from] quinn::ConnectionError)` —— `.await` 后握手失败
  - 新 `pub async fn dial(...)` —— 详见 §1.1
  - 新单测 `dial_completes_handshake_against_local_listener`（详见 §1.2）
  - 测试 helper：`ephemeral_cert()` + `endpoint_with_test_cert(...)`（详见 §1.3）
  - 模块顶部 doc comment 把 STEP-2.2 标"已"

### 1.1 `dial()` 关键设计要点

1. **签名** —— `pub async fn dial(ep: &Endpoint, addr: SocketAddr, cert: CertificateDer<'static>, key: PrivateKeyDer<'static>) -> Result<Connection>`。
   `cert` / `key` 现阶段是**对端** server 的 trust anchor（喂给
   `WebPkiServerVerifier` 的 root store）；STEP-2.5 mTLS 启用后这两个参数
   同样作为**本端 client** 出示的 cert / key —— 签名不变。

2. **实现骨架**：
   ```rust
   pub async fn dial(ep, addr, cert, key) -> Result<Connection> {
       install_crypto_provider();  // 幂等守护
       let cfg = build_quic_client_config(cert, key)?;
       let conn = ep.connect_with(cfg, addr, "lan-mouse")?.await?;
       Ok(conn)
   }
   ```
   - `install_crypto_provider()` 守护 —— 多次进入仍安全（OnceLock）
   - `?` operator 直接把 `quinn::ConnectError` 转换为 `Error::Connect`
     （`#[from]`）；把 `quinn::ConnectionError` 转换为 `Error::Handshake`
     （`#[from]`）

3. **SNI server_name `"lan-mouse"`** —— `ep.connect_with` 第三参数；
   ALPN 协议名同字面量。STEP-2.6 TofuVerifier 不读 server_name（只看
   fingerprint），硬编码无影响；与 bak `mousehop/src/quic_transport.rs:1855`
   的 `dial_one(... "mousehop")` 对称。

4. **占位 verifier** —— `dial()` 内部调 `build_quic_client_config`，
   当前形态走 `WebPkiServerVerifier::builder(roots, ring).build()` 做
   chain 校验（STEP-2.1 已就位）。STEP-2.6 改 `TofuVerifier` 时
   `dial()` 调用栈不变。

5. **`#[allow(dead_code)]`** —— STEP-2.2 仅被测试调用；STEP-6.1
   `connect.rs::connect_to_handle` 接入时一并移除。

### 1.2 单测 `dial_completes_handshake_against_local_listener` 设计

**测试布局**（与 bak `mousehop/src/quic_transport.rs:2693-2748` 完全对齐）：

1. server 端：`endpoint_with_test_cert(127.0.0.1:0, ephemeral_cert())`
   拿到 server `Endpoint` + `local_addr()`
2. 后台 `tokio::spawn` 跑 `endpoint.accept().await.await` 拿 `Connection` 后
   立即 `drop(conn)`（不消费业务；触发对端 `LocallyClosed`，quinn 0.11 正常断开）
3. client 端：`endpoint(127.0.0.1:0)` 拿 client `Endpoint`，调
   `dial(&client_ep, server_addr, client_cert[0].clone(), client_key)`
4. 5s `tokio::time::timeout` 兜底防永久挂死
5. 断言 `conn.peer_identity().is_some()` —— TLS 1.3 实际走通才会到这里
6. 清理：`drop(conn)` → 等 server task → `client_ep.wait_idle().await`

### 1.3 测试 helper `endpoint_with_test_cert`

**为什么需要**：PLAN §2.2 验收要求"同进程内 server endpoint + client endpoint
dial，断言 TLS 1.3 握手完成"。STEP-2.3 (`accept()` 公共函数) + STEP-2.4
(`endpoint_with_cert()` 公共函数) 都还没做；测试**不能**等公共 API 出现
才能写 —— inline 一个 test-only server endpoint 装配 helper。

**形态**：把 quinn ServerConfig 的最小装配（rustls::ServerConfig + ALPN +
QuicServerConfig + transport_config + Endpoint::new）内联到 `mod tests` 内
的 `fn endpoint_with_test_cert(addr, cert_chain, key) -> Result<Endpoint>`，
与 STEP-2.4 即将引入的 `endpoint_with_cert()` 公共函数**完全对称**。
STEP-2.4 上线后这个 helper 由公共函数替代。

**作用域**：`#[cfg(test)] mod tests` 内部 `fn`（非 `pub(super)`，仅本模块
测试可见），不污染公共 API。

**no client auth** —— 与 STEP-2.2 当前 `WebPkiServerVerifier` 占位形态
对齐（client 端不出 cert）。STEP-2.5 mTLS 启用后这个 helper 改为
`with_client_cert_verifier` 形态（同一文件内小改即可）。

### 1.4 `Error::Connect` / `Error::Handshake` 设计

| 变体 | 来源 | 触发场景 |
|---|---|---|
| `Connect(#[from] quinn::ConnectError)` | `Endpoint::connect_with` 同步失败 | endpoint 已关闭 / `addr` 不合法 / endpoint 无 client config |
| `Handshake(#[from] quinn::ConnectionError)` | `.await` 后握手失败 | 证书校验不通过 / ALPN 不匹配 / 中断 / STEP-2.6 TofuVerifier `untrusted peer` 错误透传 |

与 bak `mousehop/src/quic_transport.rs:1011-1014` 完全对齐（同样的
`#[from]` 派生 + Display 透传）。**不**为这两个变体单独写 `From` impl：
`#[from]` 等价于 `impl From<quinn::ConnectError> for Error`，符合 Rust
惯用法且代码最少。

## 2. 验证结果

```bash
$ cargo check -p lan-mouse 2>&1 | grep -cE "^error\[E"
14

$ cargo check -p lan-mouse 2>&1 | grep -E "src/quic_transport|src/main|src/lib|src/crypto" | grep "error\["
# （无输出）
```

14 errors 全部来自 `connect.rs` / `listen.rs` 的 `webrtc_dtls` /
`webrtc_util` 引用（与 STEP-1.2 / STEP-1.3 / STEP-1.4 / STEP-2.1 报告
完全一致）；`quic_transport.rs` / `main.rs` / `lib.rs` / `crypto.rs` 自身
**0 编译错**。

```bash
$ cargo check -p lan-mouse --tests 2>&1 | grep -cE "^error\[E"
14

$ cargo check -p lan-mouse --tests 2>&1 | grep -E "src/quic_transport" | grep "error\["
# （无输出）
```

测试 target 也 0 错来自 `dial_completes_handshake_against_local_listener`
单测 —— 测试代码逻辑就位。

```bash
$ cargo check -p lan-mouse 2>&1 | grep -E "warning\[" | grep -v "webrtc"
# （无输出）
```

无新增 warning。`build_quic_client_config` 函数体内 `let _ = key;`
还在（STEP-2.5 mTLS 才接上）；`#[allow(dead_code)]` 守护 `dial()` 不报
dead-code。

```bash
$ grep -nE "TransportEvent|Bounds|MotionAbsolute|CursorPos|ReceiverSensitivity|MAX_CLIPBOARD_SIZE|BufferTooLarge|ClipboardEvent|axis::momentum|MACOS_KEEP_AWAKE_EVENT_TAG|clipboard" src/quic_transport.rs
# （无输出 —— §9 M1 边界 12 类 grep 无命中）
```

```bash
$ cargo test -p lan-mouse --no-run 2>&1 | tail -3
error: could not compile `lan-mouse` (lib test) due to 14 previous errors
```

**单测无法跑通** —— `lan-mouse` lib 因 STEP-1.2 留下的 14 DTLS errors
编不过；test target 与 lib 同编译单位。详见 SUGGESTION #S-5。

## 3. 与 PLAN-M1 §2.2 的偏差

| 项 | PLAN 要求 | 实际做法 | 原因 |
|---|---|---|---|
| `pub async fn dial(ep, addr, cert, key) -> Result<Connection>` | 同 | 同 | 直接对齐 |
| `pub use quinn::Connection;` | re-export | `pub use quinn::{Connection, Endpoint};` | 把 Connection / Endpoint 一起列在 re-export；use 列表里排除两者避免同名 import 冲突 |
| 复用 `crypto::rustls_client_config` | "用 STEP-1.1 helper" | **未复用** —— `dial()` 内部直接调 `build_quic_client_config`（STEP-2.1 已就位） | 与 STEP-2.1 同决策：直接 rustls API 装配更直观；STEP-2.5 mTLS 后两个函数签名差异更大 |
| `ep.connect_with(cfg, addr, "lan-mouse")?.await?` | 同 | 同 | 直接对齐 |
| ALPN `b"lan-mouse"` | "用 STEP-2.1 已声明的 ALPN_LAN_MOUSE 常量" | 通过 `build_quic_client_config` 间接使用（已在 STEP-2.1 设上） | 复用而非重复；dial() 自身不操作 ALPN |
| Error 变体 | 未明确 | `Error::Connect(#[from] quinn::ConnectError)` + `Error::Handshake(#[from] quinn::ConnectionError)` | 与 bak `quic_transport.rs:1011-1014` 完全对齐 |
| 单测 `dial_completes_handshake_against_local_listener` | 验收段已给出伪代码 | 完整实现 + inline server endpoint helper | 详见 §1.2 / §1.3 |
| 测试中 server endpoint 来源 | "spawn_local 跑 server_ep.accept().await" | inline `endpoint_with_test_cert()` test helper | STEP-2.3 / 2.4 公共 server endpoint API 还没做；inline helper 是最小可执行方案 |

## 4. 处理的 SUGGESTION 项

未触动（#S-1 / #S-3 / #S-4 / #S-5 / #S-6 / #S-7 / #S-8 / #S-9 全部保留），
待 STEP-6.x 一次性清理。

## 5. 闸门检查

- 闸 1（产物）：✅ `dial()` / `pub use quinn::Connection` / `Error::Connect` /
  `Error::Handshake` / 单测 + inline test helper 齐备
- 闸 1（依赖）：✅ STEP-2.1 已归档；`build_quic_client_config` 复用成功
- 闸 1（验收）：⚠️ `cargo check -p lan-mouse` 14 errors 全 DTLS，
  quic_transport.rs 0 错（达成）；`cargo test ... dial_completes_handshake_against_local_listener`
  **未跑通**（SUGGESTION #S-5 留 STEP-6.x）
- 闸 1（M1 边界）：✅ §9 12 类 grep 无命中（未引入 TransportEvent /
  Clipboard / Bounds / h3 / clipboard*.rs / status_bar /
  Axis::momentum / MACOS_KEEP_AWAKE_EVENT_TAG 等）
- 闸 1（时间门）：✅ ~25 min，在 20–30 min 目标内

## 6. 遗留 / 风险

- ⚠️ **SUGGESTION #S-5**：`dial_completes_handshake_against_local_listener`
  在 STEP-6.x 修 14 DTLS errors 后必须由 Leader 手动跑一次确认通过
- ⚠️ **SUGGESTION #S-9**：server 端 `rustls::ServerConfig.alpn_protocols` 必须
  在 STEP-2.4 装配 server endpoint 时设 `vec![ALPN_LAN_MOUSE.to_vec()]`；
  STEP-2.4 验证清单已加（详见 SUGGESTION.md）
- ⚠️ **测试 helper `endpoint_with_test_cert`** —— 当前仅测试可见；STEP-2.4
  引入公共 `endpoint_with_cert()` 后应直接复用公共函数，删 inline helper
- ⚠️ **`#[allow(dead_code)]` 守护** `dial()` —— STEP-6.1 `connect.rs` 接入时
  一并移除
- ⚠️ **`cert` / `key` 参数语义随 STEP-2.5 变化** —— 现阶段是"对端 server 的
  trust anchor"；STEP-2.5 mTLS 后同时作为"本端 client 出示的 cert / key"
  （`with_client_auth_cert`）；签名不变。后续 STEP-2.6 / 2.7 不再改签名

## 7. 下一步（STEP-2.3 前置条件）

✅ 就绪：
- `dial(ep, addr, cert, key)` 装配 + 握手成功路径已落地
- `pub use quinn::Connection` 已 re-export
- `Error::Connect` / `Error::Handshake` 已就位
- 单测代码就位（仅待 14 errors 修复后执行）
- ALPN `b"lan-mouse"` 在 client 侧设好；server 侧 STEP-2.4 必须对称

下一步建议：执行 **STEP-2.3** —— `pub async fn accept(ep: &Endpoint) -> Result<Connection>`，
从 `ep.accept().await?.await?` 拿握手完成的 `Connection`。PLAN §2.3 验收
"跑通 STEP-2.2 的 in-process 测试即代表 accept 路径 OK；本步不再单独加
测试" —— 本步很轻量，但 `accept()` 是 STEP-2.4 `endpoint_with_cert()` + STEP-2.5
mTLS 的入口前置。
