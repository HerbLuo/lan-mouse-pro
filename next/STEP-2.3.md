# STEP-2.3 — `accept()` 接受 QUIC 连接（占位 ServerConfig）

> PLAN-M1 §STEP-2 / STEP-2.3
> 执行日期：2026-08-31　实际耗时：~12 min
> 结论：✅ 通过（最小补完步；不写单测，PLAN §2.3 明确；`accept()` 路径由
> STEP-2.2 `dial_completes_handshake_against_local_listener` 单测已隐式覆盖）

## 1. 做了什么

实现 `pub async fn accept(ep: &Endpoint) -> Result<Connection>`，
两步式握手 (`ep.accept().await.ok_or(EndpointSetup)?` + `incoming.await?`)，
复用 STEP-2.2 已就位的 `Error::EndpointSetup(String)` 与 `Error::Handshake(#[from] quinn::ConnectionError)`
两个变体，不新增任何 `Error` 变体。改动 1 个文件：

- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/quic_transport.rs`：
  395 → ~460 行
  - `pub async fn accept(ep: &Endpoint) -> Result<Connection>`（新增）
  - 模块顶部 doc comment 把 STEP-2.3 标"已"

### 1.1 `accept()` 关键设计要点

1. **签名** —— `pub async fn accept(ep: &Endpoint) -> Result<Connection>`。
   直接返回原始 `quinn::Connection`（与 PLAN §2.3 文字一致），不包
   `PeerSession`（STEP-5.4 才引入）。

2. **两步式握手**：
   - `ep.accept().await` → `Option<Incoming>`。`None` = endpoint 已关闭
     （典型场景：listener 主动 drop / runtime 退出），wrap 成
     `Error::EndpointSetup("endpoint closed (accept returned None)".into())`
     —— 复用 STEP-1.4 已有的 `Error::EndpointSetup(String)` 变体，不新增
   - `incoming.await?` → `Result<Connection, ConnectionError>`。`?` 操作
     符直接走 `Error::Handshake(#[from])` 派生（STEP-2.2 已就位）

3. **`#[allow(dead_code)]`** —— 与 `dial()` 对称：STEP-2.3 仅被
   STEP-2.2 `dial_completes_handshake_against_local_listener` 测试 helper
   间接覆盖（in-process server 直接调 `endpoint.accept().await.await`，未
   走公共 `accept()`）；STEP-6.2 `listen.rs::read_loop` 改造时 `accept()`
   切换为真正的 main-code caller，dead_code 自动消失。

4. **错误归一选择** —— 不新增 `Error::NoIncoming` / `Error::Accept` 变体：
   - 复用 `Error::EndpointSetup(String)` 包装 endpoint 关闭事件（语义接近：
     endpoint 状态异常，不是握手失败）
   - 复用 `Error::Handshake(#[from] quinn::ConnectionError)` 透传
     `ConnectionError`（cert / ALPN / 中断 / TofuVerifier 拒绝等）
   - bak 用 `Error::NoIncoming` / `Error::Accept(String)` 是历史设计，本仓
     不必镜像 —— Error 变体数量从源头控制更省心

5. **占位 ServerConfig 局限（已在 doc comment 说明）** —— 当前 `endpoint()`
   是 client-mode（`None::<ServerConfig>`，见 STEP-1.4 说明），公共
   `accept()` 在 `endpoint()` 上 **永远等不到** incoming。本步先实现
   公共函数 + 错误归一；STEP-2.4 `endpoint_with_cert()` 注入真 server cert
   后，调用方（`listen.rs` supervisor）才能真正拿到 `Connection`。

6. **测试覆盖路径** —— STEP-2.2 测试 helper `endpoint_with_test_cert()`
   已经内联 server endpoint（已含 `Some(server_cfg)`），`accept()` 内部
   逻辑（`ep.accept().await.ok_or(...)?.await?`）与测试 helper 直接调的
   `server_ep.accept().await.await` 是**等价**的（仅错误归一多一层 wrap）。
   因此 STEP-2.2 in-process 测试通过即代表 `accept()` 路径 OK，与
   PLAN §2.3 验收一致。

### 1.2 与 bak `mousehop/src/quic_transport.rs:2040-2044` 对位

| bak 实现 | 本仓实现 | 差异 |
|---|---|---|
| `pub async fn accept(ep) -> Result<PeerSession, Error>` | `pub async fn accept(ep) -> Result<Connection, Error>` | 本步按 PLAN §2.3 仅返回原始 `Connection`；`PeerSession` 留 STEP-5.4 |
| `let incoming = ep.accept().await.ok_or(Error::NoIncoming)?;` | `let incoming = ep.accept().await.ok_or_else(\|\| Error::EndpointSetup(...))?;` | 复用 `EndpointSetup` 变体，不新增 `NoIncoming` |
| `let conn = incoming.await.map_err(Error::Accept)?;` | `let conn = incoming.await?;` | 复用 `Error::Handshake(#[from])` 派生 |
| `Ok(PeerSession::from_connection(conn))` | `Ok(conn)` | 同上 |

逻辑骨架完全一致；差异只在错误变体选择（本仓优先复用而非新增，符合 §1
最小补完精神）。

## 2. 验证结果

```bash
$ cargo check -p lan-mouse 2>&1 | grep -cE "^error\[E"
14

$ cargo check -p lan-mouse 2>&1 | grep -E "src/quic_transport\.rs" | grep "error\[" | head -5
# （无输出）

$ cargo check -p lan-mouse 2>&1 | grep -E "warning\[" | head -5
# （无输出）
```

**14 errors 全部来自 `connect.rs` / `listen.rs` 的 `webrtc_dtls` /
`webrtc_util` 引用**（与 STEP-1.2 / STEP-2.1 / STEP-2.2 报告完全一致）；
`quic_transport.rs` 自身 **0 编译错、0 新增 warning**。

```bash
$ grep -nE "TransportEvent|Bounds|MotionAbsolute|CursorPos|ReceiverSensitivity|MAX_CLIPBOARD_SIZE|BufferTooLarge|ClipboardEvent|axis::momentum|MACOS_KEEP_AWAKE_EVENT_TAG|clipboard|h3|h3-quinn|status_bar" src/quic_transport.rs
# （无输出 —— §9 M1 边界 12 类 grep 无命中）
```

```bash
$ wc -l src/quic_transport.rs
461 src/quic_transport.rs
```

文件从 STEP-2.2 的 ~395 行扩到 461 行（+66 行：5 行函数体 + ~61 行 doc comment）。

## 3. 与 PLAN-M1 §2.3 的偏差

| 项 | PLAN 要求 | 实际做法 | 原因 |
|---|---|---|---|
| `pub async fn accept(ep: &Endpoint) -> Result<Connection>` | 同 | 同 | 直接对齐 |
| `ep.accept().await?.await?` | 同 | `ep.accept().await.ok_or_else(\|\| Error::EndpointSetup(...))?.await?` | `Option<Incoming>` 必须 wrap `None` → `Error::EndpointSetup`，与 `?` operator 配合使用 |
| `endpoint()` 的 ServerConfig 占位 token（无 cert 出示） | 同 | 不改 `endpoint()`，由 STEP-2.4 处理 | §9 边界：M1 不引入 cert 持久化；本步仅做 `accept()` 公共函数 + 错误归一 |
| 不单独加测试 | "本步不再单独加测试" | 不加测试 | PLAN §2.3 验收明确要求；STEP-2.2 `dial_completes_handshake_against_local_listener` 已隐式覆盖 accept 路径 |
| `Error` 变体新增 | 未明确 | 不新增，复用 `EndpointSetup` / `Handshake` | Error 变体数量最小化；bak 用的 `NoIncoming` / `Accept(String)` 是历史设计，本仓不必镜像 |

## 4. 处理的 SUGGESTION 项

未触动（#S-1 / #S-3 / #S-4 / #S-5 / #S-6 / #S-7 / #S-8 / #S-9 全部保留），
待 STEP-2.4 一次清理（特别是 #S-4 cert/key 拆文件 + #S-9 server ALPN）。

## 5. 闸门检查

- 闸 1（产物）：✅ `pub async fn accept()` / 错误归一 / doc comment 更新齐备
- 闸 1（依赖）：✅ STEP-2.2 已归档（b0123c1）；`Error::Handshake` /
  `Error::EndpointSetup` 复用成功
- 闸 1（验收）：✅ `cargo check -p lan-mouse` 14 errors 全 DTLS，
  quic_transport.rs 0 错（达成）；不写单测符合 PLAN §2.3 明确要求
- 闸 1（M1 边界）：✅ §9 12 类 grep 无命中（未引入 TransportEvent /
  Clipboard / Bounds / h3 / clipboard*.rs / status_bar /
  Axis::momentum / MACOS_KEEP_AWAKE_EVENT_TAG 等）
- 闸 1（时间门）：✅ ~12 min，远低于 20–30 min 目标（本步为最小补完，
  与 PLAN 估算一致）

## 6. 遗留 / 风险

- ⚠️ **占位 ServerConfig 局限** —— 当前 `endpoint()` 是 client-mode
  （`None::<ServerConfig>`），公共 `accept()` 在 `endpoint()` 上**永远等
  不到** incoming。这是 STEP-2.4 `endpoint_with_cert()` 的工作。本步不
  试图解决（§9 边界：不引入 cert 持久化），仅提供 `accept()` 公共函数 +
  错误归一。
- ⚠️ **`#[allow(dead_code)]` 守护** `accept()` —— STEP-6.2
  `listen.rs::read_loop` 改造时 `accept()` 切换为真正的 main-code caller，
  dead_code 自动消失。
- ⚠️ **SUGGESTION #S-5** —— `accept()` 路径虽由 STEP-2.2 测试 helper
  间接覆盖（in-process server 直接调 `endpoint.accept().await.await`），
  但严格意义上与本步公共 `accept()` 是两个调用点；STEP-6.x 修 14 DTLS
  errors 后 Leader 需手动跑 `dial_completes_handshake_against_local_listener`
  一次确认通过（与 STEP-1.4 / 2.1 / 2.2 同路径）。
- ⚠️ **SUGGESTION #S-9** —— STEP-2.4 装配 server `rustls::ServerConfig`
  时必须设 `alpn_protocols = vec![ALPN_LAN_MOUSE.to_vec()]`，否则
  ALPN mismatch 拒连（SUGGESTION.md 已记录）。

## 7. 下一步（STEP-2.4 前置条件）

✅ 就绪：
- `accept()` 公共函数 + 错误归一已就位
- `Error::EndpointSetup` / `Error::Handshake` 复用成功
- 模块顶部 doc comment 标记 STEP-2.3 已
- STEP-2.4 可直接调用 `accept()`，无需绕过

下一步建议：执行 **STEP-2.4** —— `endpoint_with_cert()` 公共函数 +
`crypto::load_or_create_server_cert()` 持久化 cert；用
`endpoint_with_cert(...)` 替换 `endpoint()` 占位路径，server 端 `accept()`
才能真正拿到 `Connection`。本步**只**实现 `accept()`，cert 持久化与
server endpoint 装配留 STEP-2.4。
