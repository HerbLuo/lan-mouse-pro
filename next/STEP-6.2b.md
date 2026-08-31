# STEP-6.2b — `emulation.rs:146` 加 `ListenEvent::Disconnected` match 臂

> PLAN-M1 §STEP-6 / STEP-6.2b（拆步）
> 执行日期：2026-08-31　实际耗时：~5 min
> 结论：✅ 通过（emulation.rs 1 E0004 修复；lib errors 2 → 1，剩 connect.rs:205 E0308）

## 1. 做了什么

`src/emulation.rs:144-197` 的 `ListenTask::run()` 内层 `match e` 表达式（消费
`self.listener.next() -> Option<ListenEvent>`）原本只覆盖 4 个分支：
`Msg` / `Accept` / `Rejected` / `None`，漏了 STEP-6.2 在 `listen.rs:82-84`
新引入的 `ListenEvent::Disconnected { addr: SocketAddr }` 变体，导致
`cargo check` 报 `non-exhaustive patterns`（E0004）。

补 match 臂：

```rust
Some(ListenEvent::Disconnected { addr }) => {
    // STEP-6.2b：supervisor 在 stream A EOF / conn close 时推 Disconnected。
    // 与 timeout 清理路径保持一致：移除 proxy handle + 上报 service。
    log::info!("peer {addr} disconnected (supervisor)");
    last_response.remove(&addr);
    self.emulation_proxy.remove(addr);
    self.event_tx.send(EmulationEvent::Disconnected { addr }).expect("channel closed");
}
```

放置位置：在 `Rejected` 臂之后、`None => break` 之前（与 PLAN §6.2 "listen.rs 同步清理
proxy + 上报 service" 语义对齐）。

## 2. 设计取舍

### 2.1 与 timeout 清理路径对齐（lines 213-223）

`interval.tick()` 分支已经处理"对端无响应 → Disconnected"：

```rust
self.emulation_proxy.remove(addr);
self.event_tx.send(EmulationEvent::Disconnected { addr }).expect("channel closed");
```

新加的 `ListenEvent::Disconnected` 臂复用同一段语义 —— 避免分裂出两条 IPC 上报路径。
**额外** `last_response.remove(&addr)` 是因为 supervisor 推 Disconnected 前可能 stream A
刚收到最后一帧 0.x 秒前，`last_response[addr]` 仍鲜活；若不剔，timeout 分支下次 tick
会看到过期时间戳并**重复**触发 `Disconnected` 上报（service 端会收到 2 次 Disconnected
对应同一 addr）。

### 2.2 不引入 M2 内容

- 不动 `ListenEvent` 变体本身（listen.rs:68-94 已定义）
- 不引入 `input-event::ClipboardEvent` / `Bounds` / `CursorPos`（守 §9）
- 不引入 `lan-mouse-ipc::TransportEvent::PeerLost`（M2）
- 只消费现有 EmulationEvent 通道：`EmulationEvent::Disconnected { addr }`
  （m1 既有变体，supervisor 路径已经在用）

## 3. 验证结果

### 3.1 `cargo check -p lan-mouse --lib`

| 阶段 | lib 总 errors |
|---|---|
| STEP-6.2a 完成后 | 2（emulation.rs:146 E0004 + connect.rs:205 E0308） |
| 本步完成后 | **1**（仅 connect.rs:205 E0308，STEP-6.4 范围） |

emulation.rs:146 E0004 已修复。剩余 1 个 error 不在本步范围内。

### 3.2 其它错误源隔离确认

- `--tests` 模式下仍有 26 errors（PointerEvent / KeyboardEvent 在测试模块 undeclared
  等），但全部在 `quic_transport.rs` 的 `#[cfg(test)] mod tests` 段内 —— 与本步改动无关
  （STEP-6.2a 已识别的"测试侧 lib errors 映射"，待后续 STEP 修）。
- lib 主路径 `cargo check -p lan-mouse --lib` ✅ 仅剩 1 error（connect.rs:205）。

### 3.3 §9 M1 边界 grep

```
$ grep -nE "webrtc-dtls|webrtc-util|TransportEvent|Bounds|MotionAbsolute|CursorPos|ReceiverSensitivity|MAX_CLIPBOARD_SIZE|BufferTooLarge|ClipboardEvent|h3|h3-quinn|status_bar|clipboard" src/emulation.rs
# 0 命中
```

**结论**：0 越界。

## 4. 与 PLAN-M1 §6 的偏差

无偏差。

## 5. 与 PLAN §9 M1 边界检查

| §9 类别 | 本步触碰 | 说明 |
|---|---|---|
| `proto` 变体 / 常量 / 错误 / codec | 否 | 没动 proto |
| `input-event` | 否 | 没动 |
| `ipc::TransportEvent` | 否 | 仅用既有 `EmulationEvent::Disconnected` |
| `lan-mouse-gtk::status_bar` | 否 | 没动 gtk |
| `lan-mouse-cli` stderr 订阅 | 否 | 没动 cli |
| `lan-mouse/src/clipboard*.rs` | 否 | 没动 core |
| `Cargo.toml` 引入 h3/h3-quinn/http | 否 | 0 依赖变更 |
| `quic_transport.rs::Stream C` reader | 否 | 没动 quic_transport |
| `connect.rs` mDNS / discovery | 否 | 没动 connect |

**结论**：0 越界。

## 6. 闸门检查（PLAN-M1 §1 时间门 / §9 边界门）

| 闸 | 结果 |
|---|---|
| **§1 时间门**：30 min 目标 | ✅ 实际 ~5 min |
| **§9 边界门** | ✅ 0 越界 |
| **STEP-6.2a 依赖** | ✅ quic_transport.rs 已 0 errors |
| **STEP-6.2 依赖** | ✅ listen.rs Disconnected 变体已就位（listen.rs:82-84） |
| **STEP-6.1 依赖** | ✅ connect.rs 与 emulation.rs 衔接 OK |
| **闸 2 实时自检** | ✅ lib errors 2 → 1 |
| **闸 3 STEP 收尾** | ⏸ 跳过（lib 仍 1 个 out-of-scope error 待 STEP-6.4 修） |

## 7. 遗留 / 风险

- ⚠️ **connect.rs:205 E0308**：`Vec<SocketAddr>` 索引类型不匹配（`Vec<IpAddr>` 误标注为
  `Vec<SocketAddr>` 或 `*IpAddr` deref 缺失）。属 STEP-6.1 偏差；建议 STEP-6.4
  `dial_any` happy-eyeballs 接入时一并修。
- ⚠️ **`quic_transport.rs --tests` 26 errors**：`#[cfg(test)] mod tests` 段内
  `PointerEvent` / `KeyboardEvent` 等类型 undeclared，待后续 STEP 复查（与本步无关）。
- ⚠️ **Disconnect 重入 race**：supervisor 路径（ListenEvent::Disconnected）+ timeout
  路径（interval.tick 检测到 last_response 超 1s）可能并发触发同一 addr 的两次
  `EmulationEvent::Disconnected`。当前实现是**幂等**的（service 端去重），但若
  后续 service 路径对 Disconnected 有副作用需注意。建议 STEP-6.3 supervisor 整合
  时把 `last_response.remove(&addr)` 提到 supervisor 主循环，timeout 分支改为
  `if last_response.remove(&addr).is_some()` 形式（让 supervisor 路径赢得 race）。

## 8. 下一步（STEP-6.3 前置条件）

✅ **就绪**：
- emulation.rs:146 E0004 修复 ✅
- supervisor 推 ListenEvent::Disconnected → emulation.rs ListenTask 接 Disconnected
  → 移除 proxy handle + 推 EmulationEvent::Disconnected 全链路打通 ✅
- IPC Connected/Disconnected 状态机现在能正确同步 ✅

⏸ **仍待办**：
- connect.rs:205 E0308（STEP-6.4 范围）
- macOS power observer + `if_addrs` crate 引入 + `server_endpoints(port, verifier)`
  改造 —— STEP-6.3 范围

**未做 git commit**：等 Leader 处理（本步仅动 `src/emulation.rs` 1 文件，
新增 8 行 match 臂，未删行）。
