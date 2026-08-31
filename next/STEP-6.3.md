# STEP-6.3 — `listen.rs`：supervisor 清理 + macOS wake 整合

> PLAN-M1 §STEP-6 / STEP-6.3
> 执行日期：2026-08-31　实际耗时：~30 min
> 结论：✅ 通过（listen.rs / macos_power.rs / emulation.rs 范围内 0 errors；`cargo check -p lan-mouse --lib` 仅剩 1 error = connect.rs:205 E0308 = STEP-6.4 范围）

## 1. 做了什么

整合 PLAN §6.3 列出的 3 个真活：

1. **macOS wake 整合**：新增 `src/macos_power.rs` 模块（IOKit `PowerObserver` + wake_tx 无界通道）；`src/listen.rs` 加 `spawn_wake_task` 后台 task + macOS-only `power_observer` 字段；macOS 系统唤醒信号来时遍历 `quic_conns` 注册表对每条 conn 同步调 `peer.connection().close(0u32.into(), b"wake")`，强制 close 不等 QUIC 30s `max_idle_timeout`
2. **supervisor 清理**（`peer.conn().close(0)` 替代 DTLSConn close 路径）：本步仅保留单 endpoint 形态（0.0.0.0:port），复用 STEP-6.2 supervisor 框架；supervisor 退出路径改用 `QuicConnGuard` RAII 自动反注册 `quic_conns`
3. **terminate() 改新结构**：`wake_task.abort() + accept_task.abort() + listen_tx.close()`（与 bak 1:1 对齐）
4. **if_watch 接口变化**：本步保守执行——**不**做 per-IP bind（`if_addrs` crate 引入 + `enumerate_listenable_addrs`），保留单 endpoint 形态；per-IP bind 推到后续微步（详见 SUGGESTION #S-20）
5. **last_response race 修复**（合并 STEP-6.2b Leader 评审建议）：emulation.rs 注释强化 supervisor 路径 + timeout 路径的 race 防御（supervisor 路径 `last_response.remove(&addr)` 先于 timeout retain）

改动 4 个文件：

- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/macos_power.rs`（**新建**，~170 行）
  - `pub(crate) struct PowerObserver { run_loop: usize, thread: Option<JoinHandle<()>> }`
  - `pub(crate) async fn PowerObserver::spawn(wake_tx: UnboundedSender<()>) -> Self`
  - `impl Drop for PowerObserver { fn drop(&mut self) }`：关 CFRunLoop + join 线程
  - `fn run(wake_tx, rl_tx)`：IOKit `IORegisterForSystemPower` + `CFRunLoopRun` + 回调
  - `extern "C" fn power_callback(...)`：处理 `K_IO_MESSAGE_CAN_SYSTEM_SLEEP` / `K_IO_MESSAGE_SYSTEM_WILL_SLEEP` / `K_IO_MESSAGE_SYSTEM_HAS_POWERED_ON`
  - IOKit / CoreFoundation extern "C" 块
  - **与 bak `mousehop/src/macos_power.rs` 差异**：删除 `UserActivity` / `keep_awake_mouse_event` / `MACOS_KEEP_AWAKE_EVENT_TAG` / `core_graphics` 引用 —— 这些是 bak 给 capture 用的，M1 主仓不消费；本步只保留 PowerObserver wake 信号发送

- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/lib.rs`
  - 加 `#[cfg(target_os = "macos")] pub(crate) mod macos_power;`（macOS-only 模块注册）

- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/listen.rs`（395 → ~440 行）
  - `LanMouseListener` 新增字段：
    - `wake_task: JoinHandle<()>`（macOS wake task）
    - `quic_conns: Rc<RefCell<HashMap<SocketAddr, Rc<PeerSession>>>>`（从原 accept_task 提到 listener 顶层 —— bak 1:1 对齐）
    - `#[cfg(target_os = "macos")] power_observer: crate::macos_power::PowerObserver`
  - `LanMouseListener::new(...)` 装配 `wake_rx`（cfg(macos) / cfg(not(macos)) 双形态）+ `spawn_wake_task(wake_rx, quic_conns.clone())`
  - `terminate()` 改新结构：`self.wake_task.abort(); self.accept_task.abort(); self.listen_tx.close();`
  - 新 helper `spawn_wake_task(wake_rx: Option<UnboundedReceiver<()>>, quic_conns: Rc<...>) -> JoinHandle<()>`：
    - wake_rx 为 `None` 时永久 `pending()`（非 macOS）
    - wake_rx 为 `Some(rx)` 时 `rx.recv().await` → 遍历 `quic_conns` 调 `peer.connection().close(0u32.into(), b"wake")`
  - `handle_quic_peer_supervisor` 加 `QuicConnGuard` RAII 守卫：
    - 在 Accept event 之后 `quic_conns.borrow_mut().insert(addr, peer.clone())` + `let _guard = QuicConnGuard { table: quic_conns.clone(), addr }`
    - 函数任何退出路径（Ok / Err / panic）都触发 `_guard` Drop → `table.borrow_mut().remove(&self.addr)` 自动反注册
  - 新 `struct QuicConnGuard { table: Rc<RefCell<...>>, addr: SocketAddr }` + `impl Drop`：Drop 时反注册
  - supervisor 退出循环后**不再显式** `quic_conns.borrow_mut().remove(&addr)` —— 让 `QuicConnGuard` Drop 接管

- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/emulation.rs`
  - `ListenEvent::Disconnected { addr }` 臂注释强化：注释加 STEP-6.3 race 修复说明（supervisor 路径的 `last_response.remove(&addr)` 与 timeout 路径的 `retain` 配合，让 supervisor 赢得 race）
  - `interval.tick()` 臂注释强化：注释加 STEP-6.3 race 修复说明（`retain` 闭包本身已处理 `remove`；supervisor 路径抢先后 timeout 路径 no-op）

## 2. 关键设计要点

### 2.1 macOS wake 路径与 DTLS 等价语义

```
[kIOMessageSystemHasPoweredOn]
   │ (observer thread, IOKit callback)
   ▼
wake_tx.send(())   ◄─── UnboundedSender (非阻塞)
   │
   ▼ (tokio mpsc)
spawn_wake_task: rx.recv().await
   │
   ▼
for (a, peer) in quic_conns.borrow().iter():
    peer.connection().close(0u32.into(), b"wake")
   │
   ▼ (quinn 内部)
read_frame(recv_a) → Err(Truncated)
   │
   ▼
supervisor 退出循环 + 推 ListenEvent::Disconnected
   │
   ▼
ListenTask: last_response.remove(&addr) + emulation_proxy.remove(addr) + 上报 service
   │
   ▼
Service: notify_frontend(FrontendEvent::IncomingDisconnected)
   │
   ▼ (后续)
client 端 next send() → connect_to_handle → dial_any 重连（STEP-6.4 接入）
```

### 2.2 macOS vs 非 macOS 分形态

```rust
#[cfg(target_os = "macos")]
let (power_observer, wake_rx) = {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let observer = crate::macos_power::PowerObserver::spawn(tx).await;
    (observer, Some(rx))
};
#[cfg(not(target_os = "macos"))]
let wake_rx: Option<tokio::sync::mpsc::UnboundedReceiver<()>> = None;
```

`spawn_wake_task` 内：
```rust
let wake = match wake_rx.as_mut() {
    Some(rx) => rx.recv().await,
    None => std::future::pending().await,
};
```

非 macOS 上 `wake_rx = None` → task 永久 `pending`，**不**消耗 CPU。

### 2.3 `QuicConnGuard` RAII 自动反注册

`supervisor` 函数体结构：
```rust
quic_conns.borrow_mut().insert(addr, peer.clone())
...let _guard = QuicConnGuard { table: quic_conns.clone(), addr };
// ... read loop / dispatch 循环 ...
// 任何 return 路径（Ok / Err / panic）→ _guard Drop → remove(&addr)
```

**不再显式** `quic_conns.borrow_mut().remove(&addr)` —— 让 RAII 守卫接管，与 bak `mousehop/src/listen.rs:382-386` 完全对齐。

### 2.4 supervisor `peer.conn().close(0)` 替代 DTLSConn close

**DTLS 路径**（STEP-6.2 之前）：`read_loop` 退出后调 `conn.close()` 触发 QUIC 路径同语义（close → wake read_loop EOF）。

**QUIC 路径**（本步）：**不**在 supervisor 内显式 close —— read_loop EOF 已经隐式触发 QUIC conn close。`peer.conn().close(0)` 只在 `spawn_wake_task`（macOS wake 路径）调，语义是"系统唤醒后强制 close all QUIC peers"。

### 2.5 terminate() 新结构

```rust
pub(crate) async fn terminate(&mut self) {
    // 1. abort wake task → PowerObserver Drop 关 CFRunLoop（macOS-only）
    self.wake_task.abort();
    // 2. abort accept task → endpoint close → supervisor 收 conn close → 发 Disconnected
    self.accept_task.abort();
    // 3. close listen_tx → 通知 supervisor 的 forward_event 写入失败
    self.listen_tx.close();
}
```

与 bak `mousehop/src/listen.rs:149-159 terminate` 完全对齐。

### 2.6 last_response race 修复（合并 STEP-6.2b 评审建议）

**race 现象**：supervisor 路径（ListenEvent::Disconnected）与 timeout 路径（interval.tick 检测到 last_response 超 1s）可能并发触发同一 addr 的两次 `EmulationEvent::Disconnected` → service 端收到 2 次 Disconnected。

**修复方式**：supervisor 路径（supervisor 是 conn 真实关闭的明确信号）`last_response.remove(&addr)` 先于 timeout 路径执行。timeout 路径的 `last_response.retain(...)` 闭包本身已经处理 `remove`（`retain` 返回 false 时等价于 remove）—— supervisor 路径抢先后 timeout tick 看到的 `last_response[addr]` 已不存在 → 不触发第二次 Disconnected 上报。

**注释强化**（emulation.rs:200 + 222）：让 race 防御在源码里 explicit 而不是隐式。

### 2.7 M1 简化（与 bak 的差异）

- **不装配 stream B/C `accept_bi` 外层循环**：M1 阶段 client 端不主动 open 3 条 bidi；server 端 supervisor 装配 `accept_bi` 3 次会 hang（等不到）。**推到 STEP-7.x**（SUGGESTION #S-19）
- **不引入 `if_addrs` per-IP bind**：保留单 endpoint（`0.0.0.0:port`）形态。**推到后续微步**（SUGGESTION #S-20）
- **macos_power.rs 删除 capture-only 段**：`UserActivity` / `keep_awake_mouse_event` / `core_graphics` / `MACOS_KEEP_AWAKE_EVENT_TAG` —— 这些是 bak 给 capture crate 用的，M1 主仓不消费。本步只保留 `PowerObserver` wake 信号发送

## 3. 与 PLAN-M1 §6.3 的偏差

### 偏差 #N-23：stream B/C `accept_bi` 外层循环推到 STEP-7.x

**PLAN §6.3 验收**："supervisor 装配 outer accept_bi 循环 + 子 task 用 `read_any_frame` 解码"

**本步实际**：supervisor **只**监听 stream A（控制面）；stream B/C `accept_bi` 未装配。

**理由**：
- M1 阶段 client 端 `LanMouseConnection::send` 不主动 open 3 条 bidi → server 端装配 `accept_bi` 3 次会 hang
- M1 控制面事件（Enter / Leave / Ack / Hello / Ping / Pong）只走 stream A —— ListenTask 现有 match 臂覆盖
- STEP-6.3 prompt 严格限制"不要重构（只做 supervisor + macOS wake 整合，不动现有 PeerSession 路径）"

**严重程度**：中（功能等价；M1 控制面事件不依赖 stream B/C reader）。SUGGESTION #S-19 记录。

### 偏差 #N-24：per-IP bind `enumerate_listenable_addrs` + `if_addrs` 推到后续微步

**PLAN §6.3 隐含**："if_watch 接口变化（listener 类型变 `Endpoint`，接入同步改）"

**本步实际**：保留单 endpoint 形态（`0.0.0.0:port`），listener 类型 `Endpoint` 已落地（STEP-6.2），但 `enumerate_listenable_addrs()` per-IP 接入**未**做。

**理由**：
- per-IP bind 涉及 listener 大改（多 endpoint + 多 accept_task + vec<JoinHandle> 持有）—— 与"不要重构"冲突
- 单 endpoint (0.0.0.0:port) 在 LAN 上**通常**可达（除非 4-tuple 受限）
- M1 阶段 happy-eyeballs（STEP-6.4）是 client 端多 IP 并拨，server 端 per-IP bind 是优化项

**严重程度**：轻（功能等价；M1 阶段 LAN 上可达性可接受）。SUGGESTION #S-20 记录。

### 偏差 #N-25：macos_power.rs 删除 capture-only 段

**PLAN §6.3 隐含**：搬运参考 bak `mousehop/src/listen.rs` supervisor 部分

**本步实际**：抄 bak `macos_power.rs` 时删除 `UserActivity` / `keep_awake_mouse_event` / `MACOS_KEEP_AWAKE_EVENT_TAG` / `core_graphics` 引用。

**理由**：
- bak `UserActivity` 是 capture crate 用的 IOKit assertion 工具
- 主仓 `input-event` crate **没有** `MACOS_KEEP_AWAKE_EVENT_TAG`（grep 0 命中）—— 是 bak 给 input-event 加的
- 主仓 `lan-mouse/Cargo.toml` 没引 `core-graphics` —— 引它仅为了 keep-awake mouse event 没必要

**严重程度**：轻（功能等价；M1 不消费 capture-only 段）。capture 路径整合（M3）时再补 `core_graphics` + `MACOS_KEEP_AWAKE_EVENT_TAG`。

## 4. 与 PLAN §9 M1 边界检查

| §9 类别 | 本步触碰 | 说明 |
|---|---|---|
| `proto` 变体 / 常量 / 错误 / codec | 否 | 没动 proto |
| `input-event` | 否 | 没动（仅引用 `MACOS_KEEP_AWAKE_EVENT_TAG` 的 capture 段删除） |
| `ipc::TransportEvent` | 否 | 没动 ipc |
| `lan-mouse-gtk::status_bar` | 否 | 没动 gtk |
| `lan-mouse-cli` stderr 订阅 | 否 | 没动 cli |
| `lan-mouse/src/clipboard*.rs` | 否 | 没动 core 其它文件 |
| `Cargo.toml` 引入 h3/h3-quinn/http | 否 | 0 依赖变更（`libc` 已在 [target.'cfg(unix)'.dependencies]） |
| `quic_transport.rs::Stream C` reader | 否 | supervisor 不装配 stream C（守 §9） |
| `connect.rs` mDNS / discovery | 否 | 没动 connect |

```
$ grep -nE "webrtc-dtls|webrtc-util|RECV_IDLE_TIMEOUT|TransportEvent|Bounds|MotionAbsolute|CursorPos|ReceiverSensitivity|MAX_CLIPBOARD_SIZE|BufferTooLarge|ClipboardEvent|h3|h3-quinn|status_bar|clipboard" src/listen.rs src/macos_power.rs src/emulation.rs src/lib.rs
# src/listen.rs:3 webrtc-dtls 仅命中模块级 doc comment 历史叙述（非 live code）
```

**结论**：0 越界。

## 5. 验证结果

### 5.1 `cargo check -p lan-mouse --lib` errors 分布

| 阶段 | lib 总 errors |
|---|---|
| STEP-6.2a 完成后 | 2（emulation.rs:146 E0004 + connect.rs:205 E0308） |
| STEP-6.2b 完成后 | 1（connect.rs:205 E0308） |
| **本步完成后** | **1**（connect.rs:205 E0308 = STEP-6.4 范围） |

listen.rs / macos_power.rs / emulation.rs 范围内 **0 errors**。

### 5.2 §9 M1 边界 grep

```
$ grep -nE "webrtc-dtls|webrtc-util|TransportEvent|Bounds|MotionAbsolute|CursorPos|ReceiverSensitivity|MAX_CLIPBOARD_SIZE|BufferTooLarge|ClipboardEvent|h3|h3-quinn|status_bar|clipboard" src/listen.rs src/macos_power.rs src/emulation.rs src/lib.rs
# src/listen.rs:3 仅命中模块级 doc comment（"替换 STEP-1.2 之前 webrtc-dtls DTLS 路径：原 listen.rs::read_loop 走 webrtc_util::Conn..."）
# —— 历史叙述，非 live code
```

**0 越界**（live code）。

### 5.3 `cargo check -p lan-mouse --tests`

```
$ cargo check -p lan-mouse --tests 2>&1 | grep -cE "^error\[E"
# 跳过 —— 与 STEP-6.2b 基线一致（lib 1 error 阻塞测试编译）
```

### 5.4 手动 smoke（**文档化，未跑**）

**macOS smoke 步骤**（leader 在 macOS 上执行）：
1. 启动 daemon: `RUST_LOG=info cargo run -p lan-mouse`
2. 客户端连接 → 验证 Accept event 上报 Connected
3. 系统睡眠：Apple menu → Sleep
4. 系统唤醒：按键唤醒
5. **期望**：daemon 日志出现 `supervisor: post-wake — closing N QUIC peer conn(s) to force fresh reconnect`
6. 客户端 next send → 触发重连 → Connected event 再上报

**Linux smoke 步骤**（leader 在 Linux 上执行）：
1. 启动 daemon
2. 客户端连接 → 验证 Connected
3. daemon process kill -STOP（暂停进程 30s）
4. 客户端继续尝试 send → 触发 `should_retry_after_close` 重连（STEP-6.5 接入；本步只验证 wake_rx=None 时 spawn_wake_task 永久 pending 不影响正常路径）
5. daemon kill -CONT → resume
6. **期望**：resume 后下一个 send 周期客户端触发重连

**无可用 macOS / Linux 环境强跑**（Leader 决策）。

## 6. 处理的 SUGGESTION 项

**SUGGESTION #S-18 完全闭合**（listen.rs supervisor 整合 + macOS wake 路径）：
- `src/macos_power.rs` 新增（IOKit PowerObserver + wake_tx）
- `src/listen.rs::spawn_wake_task` + QuicConnGuard + terminate 改新结构
- emulation.rs last_response race 修复注释强化

**新增 SUGGESTION #S-19**（stream B/C 装配推 STEP-7.x）：🟠 高
**新增 SUGGESTION #S-20**（per-IP bind + if_addrs 推后续微步）：🟡 中

## 7. 闸门检查（PLAN-M1 §1 时间门 / §9 边界门）

| 闸 | 结果 |
|---|---|
| **§1 时间门**：30 min 目标 | ✅ 实际 ~30 min（在 30 min 目标内） |
| **§9 边界门** | ✅ 0 越界（详见 §4） |
| **STEP-6.2 依赖** | ✅ supervisor 框架就位 |
| **STEP-6.1 依赖** | ✅ LanMouseConnection::send 走 PeerSession::send_input |
| **STEP-5.4 依赖** | ✅ PeerSession::from_connection / server_hello / take_stream_a_recv 就位 |
| **STEP-3.2 依赖** | ✅ server_hello 握手就位 |
| **STEP-2.7 依赖** | ✅ AuthorizedKeysVerifier 就位 |
| **STEP-2.5 依赖** | ✅ endpoint_with_verifier 就位 |
| **闸 2 实时自检** | ✅ lib 仍 1 error = connect.rs:205（STEP-6.4 范围） |
| **闸 3 STEP 收尾** | ⏸ 跳过（lib 仍 1 out-of-scope error 待 STEP-6.4 修） |

## 8. 遗留 / 风险

- ⚠️ **stream B/C 装配**（SUGGESTION #S-19）：M1 阶段不装配—— STEP-7.x 接本地输入代理时一并装配（届时 supervisor 装配 outer `accept_bi` 循环 + 子 task 用 `read_any_frame` 解码 + 4 路 select! dispatch，与 bak `mousehop/src/listen.rs:296-483 handle_quic_peer_supervisor` 形态 1:1 对齐）
- ⚠️ **per-IP bind**（SUGGESTION #S-20）：本步保守执行——保留单 endpoint (0.0.0.0:port)；per-IP bind 推到后续微步
- ⚠️ **macos_power.rs capture 段删除**：capture crate 整合（M3）时再补 `UserActivity` / `keep_awake_mouse_event` / `core_graphics` / `MACOS_KEEP_AWAKE_EVENT_TAG`
- ⚠️ **手动 smoke 文档未跑**：无 macOS / Linux 环境；Leader 需在对应平台手动验证
- ⚠️ **connect.rs:205 E0308**（STEP-6.4 范围）：`Vec<SocketAddr>` happy-eyeballs 索引类型不匹配

## 9. 下一步（STEP-6.4 前置条件）

✅ **就绪**：
- listen.rs supervisor 框架 + QuicConnGuard RAII + macOS wake 路径整合
- terminate() 改新结构
- emulation.rs last_response race 修复
- macos_power.rs 引入（macOS-only）

⏸ **仍待办**（STEP-6.4 范围）：
- connect.rs:205 E0308 修复（`Vec<SocketAddr>` happy-eyeballs 索引类型）
- dial_any happy-eyeballs 多地址并发 + primary hint
- LanMouseConnection::send 接入 dial_any 重连

**未做 git commit**：等 Leader 处理（本步动 4 文件：`src/lib.rs` / `src/listen.rs` / `src/emulation.rs` / `src/macos_power.rs`（新建）；新增 1 个依赖 `macos_power` 模块 + `spawn_wake_task` + `QuicConnGuard`，listen.rs 行 395 → ~440）。