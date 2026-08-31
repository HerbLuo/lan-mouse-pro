# STEP-7.1 — 移除 `RECV_IDLE_TIMEOUT`

> PLAN-M1 §STEP-7 / STEP-7.1
> 执行日期：2026-08-31　实际耗时：~15 min
> 结论：✅ 通过（但**本步实际工作量远小于 PLAN 预期** —— 见 §3 偏差 #N-31）

## 1. 做了什么

**核心发现（闸 1 调研阶段）**：PLAN §7.1 列的 3 项变更要点中，**前 2 项在 STEP-6.2 已随 DTLS `read_loop` 整体重写一并消失**，第 3 项本就已是现状。本步实际只清理了 1 处**陈述过时的 doc 注释**。

| PLAN §7.1 变更要点 | 本步实际状态 |
|---|---|
| 删 `const RECV_IDLE_TIMEOUT: Duration = Duration::from_secs(8);` | **已不存在** —— 全仓 0 命中（含 git history `-S` 检索：该标识符在 FORK 后从未出现在 `src/` 下） |
| 删 `read_loop` 里的 `tokio::time::timeout` 包裹 | **已不存在** —— STEP-6.2 把 `listen.rs` 从 DTLS `read_loop` 整体切到 PeerSession supervisor 时，旧循环连同 timeout 包裹一起被删 |
| 改为 `peer.read_any_frame(...).await` 直调 | **已是现状** —— `listen.rs:479` 是裸 `quic_transport::read_frame(&mut recv_a).await`，无任何 timeout 包裹 |

**唯一改动**：`src/quic_transport.rs:372-373` doc 注释。原文用**将来时**写 "应用层 idle 检测（`RECV_IDLE_TIMEOUT = 8s`）**由 STEP-7.1 删除**" —— 该句是 STEP-1.4 写 `default_transport_config()` 时留的前瞻标记。本步改为完成时陈述，并补上"对端静默不再触发本端主动关连"的语义说明（即 PLAN §7.1 的验收意图）。

## 2. 关键判断：为什么 `read_frame` 直调是安全的

STEP-7.1 的实质是**把"连接何时算死"的判定权从应用层交还给 QUIC 传输层**。三条支撑：

1. **两端都装配了 keepalive**：`default_transport_config()`（`keep_alive_interval = 5s` / `max_idle_timeout = 30s`）被注入 server 端（`:562`）、client 端（`:672`）、dial_any 路径（`:3176`）三处 —— 无遗漏路径退回 quinn 默认（quinn 默认 `keep_alive_interval = None`，那才会让长静默链路被对端 idle 超时切断）。
2. **健康链路上 5s keepalive 永远先于 30s idle 到达**：应用层静默（对端 sleep 5s / GC / 用户不动鼠标）不再产生任何关连压力 —— PING frame 由 quinn 自动发，与应用层是否有数据无关。
3. **`read_frame` 的阻塞是"背压"而非"泄漏"**：supervisor 阻塞在 `read_frame(&mut recv_a).await` 上等价于阻塞在 `RecvStream` 上；连接真死时 quinn 会让该 future 返 `Err`（`ConnectionLost` / `TimedOut`），走 `listen.rs:505` 的 `Err(e)` 臂退出 + `QuicConnGuard` Drop 反注册。**不存在"永久 hang 无人回收"路径**。

**副作用（正面）**：删掉 8s 应用层 idle 后，PLAN §0.1 "探活超时 8s → ≥30s" 验收项达成 —— 这也正是 REQUIREMENT §1 表格里 "2s idle 即断 → 对端暂停/GC/休眠 → 鼠标卡顿数秒" 那条痛点的闭环。

## 3. 与 PLAN-M1 §7.1 的偏差

### 偏差 #N-31：STEP-7.1 的功能性工作已被 STEP-6.2 提前吸收

**PLAN §7.1 预期**：本步是一次独立的删除动作（删 const + 删 timeout 包裹 + 改直调，~15 min）。

**本步实际**：前 2 项在 STEP-6.2（`listen.rs` 切 PeerSession supervisor）时已随旧 DTLS `read_loop` 整体消失，第 3 项本就是重写后的形态。本步退化为 1 处 doc 注释订正。

**理由**：PLAN §7.1 写作时假设 `listen.rs` 的 DTLS `read_loop` 会**存活到 STEP-7**，只在收尾时才摘掉 idle 检测。实际 STEP-6.2 是"整段替换"而非"增量改造" —— 旧循环体（含 `RECV_IDLE_TIMEOUT` 包裹）被整体删除，新循环从 bak 形态搬运，天然不带应用层 idle。

**严重程度**：轻（功能目标已达成，且达成得更早；无返工、无残留）。**但需 Leader 注意**：这类"后置清理步骤被前置重写吸收"的模式在 STEP-7.3（删 webrtc-dtls 依赖，STEP-1.2 已删过）已出现过一次，STEP-7.4 / 7.5 / 7.6 可能同样已部分完成 —— 建议每步开工前先跑现状 grep 再决定工作量。

### 观察 #O-1：Leader prompt 与 PLAN 中的 grep 路径均指向不存在的目录

PLAN §7.6 与本步 prompt 给的自验命令都是：

```
grep -rn "RECV_IDLE_TIMEOUT" lan-mouse/src lan-mouse-ipc/src ...
```

但主 crate 源码实际在**仓库根 `src/`**，不存在 `lan-mouse/src/`。该命令对主 crate 恒返回空 —— 是**假阴性**，不是"已清理干净"的证据。本步已改用 `src ...` 正确路径复核（见 §4）。已记入 SUGGESTION #S-21，建议 Leader 修正 PLAN §7.6 的验证命令，否则 STEP-7.6 的收尾 grep 会以假阴性通过。

## 4. 验证结果

### 4.1 `cargo build -p lan-mouse`

```
$ cargo build -p lan-mouse
warning: `lan-mouse` (lib) generated 8 warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 7.78s
```

✅ 通过。8 warnings 全为 pre-existing dead-code（`CertKeyPair` / `ListenEvent::Rejected` / `power_observer` 等），0 来自本步改动（本步只改注释）。

### 4.2 grep 自验（**正确路径**）

```
$ grep -rn "RECV_IDLE_TIMEOUT" src lan-mouse-ipc/src lan-mouse-proto/src lan-mouse-gtk/src lan-mouse-cli/src
（无输出，exit 1）

$ grep -rn "RECV_IDLE_TIMEOUT" --include="*.rs" --include="*.toml" .   # 全仓，排除 target/
（无输出，exit 1）

$ grep -rniE "closing stale|stale connection|idle_since|last_recv_at" src
（无输出，exit 1）
```

✅ 全仓 0 残留（源码 + 配置）。

### 4.3 `listen.rs` 无 timeout 包裹

```
$ grep -n "timeout" src/listen.rs
24  //!   同步触发 close（不等 QUIC 30s `max_idle_timeout`）
114 /// `max_idle_timeout`），触发 supervisor 的 read_loop EOF →
128 /// close —— 不等 QUIC `max_idle_timeout` (30s)。
335 /// `peer.connection().close(0, b"wake")` —— 不等 30s `max_idle_timeout`，
```

4 处命中**全部是 doc 注释**且全部关于 **macOS wake 强制 close 路径**（STEP-6.3）—— 语义是"唤醒后立即关连、不等 QUIC 30s 超时"，与应用层 idle 检测无关，**应予保留**。0 处 `tokio::time::timeout` 代码命中。

### 4.4 keepalive 前置条件复核（STEP-7.1 立论基础）

```
$ grep -n "default_transport_config()" src/quic_transport.rs
562:    server_cfg.transport_config(default_transport_config());     # server 端
672:    client_cfg.transport_config(default_transport_config());     # client 端
3176:       client_cfg.transport_config(default_transport_config()); # dial_any 路径
```

✅ 三条建连路径全部装配 `keep_alive_interval = 5s` + `max_idle_timeout = 30s`，无路径退回 quinn 默认（`keep_alive_interval = None`）。

### 4.5 手动 smoke 步骤（无双机环境，文档化留 STEP-7.2）

PLAN §7.1 验收："两端连接后让对端 sleep 5s，本端不报 closing stale connection"。本地无双机环境，记录可执行步骤：

```
1. 机器 A：RUST_LOG=lan_mouse=debug cargo run -- daemon
2. 机器 B：配置 A 为 peer + 指纹互信，RUST_LOG=lan_mouse=debug cargo run -- daemon
3. 待日志出现 client_hello / server_hello 成功（hello_ok = true）
4. 两端均停止一切鼠标/键盘输入，静默 ≥ 10s（覆盖原 8s 阈值）
5. 期望：
   - 两端日志均无 "stale" / "Disconnected" / "stream A truncated"
   - QUIC PING frame 每 5s 自动发（RUST_LOG=quinn=trace 可见）
   - 连接在 30s+ 静默后仍存活（继续动鼠标可立即恢复，无重连日志）
6. 反证：连接确实会在真断线时关 —— 拔网线 30s 后应见 TimedOut → supervisor 退出 → RetryState 退避重连
```

**判据**：步骤 5 无 close 日志 = STEP-7.1 达成；步骤 6 有 TimedOut = 未把连接生命周期判定一并删坏。
端到端自动化覆盖留 STEP-7.2 `tests/quic_smoke.rs`。

## 5. 处理的 SUGGESTION 项

- **新增 #S-21**（🟡 中）：PLAN §7.6 / 各步 prompt 的 grep 路径 `lan-mouse/src` 不存在，恒假阴性 —— 建议 Leader 修正为 `src`
- 无消化条目（本步未触及既有条目根因）

## 6. 闸门检查（PLAN-M1 §1 时间门 / §9 边界门）

| 闸 | 结果 |
|---|---|
| **§1 时间门**：15 min 估时 | ✅ 实际 ~15 min（调研占绝大部分；改动仅 1 处注释） |
| **§9 边界门** | ✅ 0 越界 —— 未动 proto / ipc / gtk / cli / clipboard / Cargo.toml；未开 Stream C reader |
| **STEP-6.5 依赖** | ✅ 已归档通过（`PeerSession::run()` 返 close reason + RetryState 退避就位） |
| **STEP-1.4 依赖**（keepalive 5s / idle 30s） | ✅ 三条建连路径全装配（见 §4.4） |
| **不重构约束** | ✅ 仅改 1 处 doc 注释，0 行逻辑变更 |
| **不引入新依赖** | ✅ 0 依赖变更 |
| **闸 3 STEP 收尾全套回归** | ⏸ 跳过（非 STEP-7 末步；STEP-7.3 集中跑 `--workspace` 全套） |

## 7. 遗留 / 风险

- ⚠️ **双机 smoke 未实跑**：§4.5 步骤已文档化，需 Leader 在有双机环境时执行，或等 STEP-7.2 `tests/quic_smoke.rs` 自动化覆盖
- ⚠️ **PLAN §7.6 收尾 grep 命令路径错误**（#S-21）：若不修正，STEP-7.6 的 "无 live code 残留" 验收会以假阴性通过 —— 该步是 M1 DoD 的清理闸门，建议 Leader 优先处理
- ⚠️ **STEP-7.4 / 7.5 可能同样已被前置步骤部分吸收**（偏差 #N-31 同模式）：建议各步开工前先跑现状 grep 核实真实工作量，避免"按 PLAN 字面找不到目标代码"的困惑

## 8. 下一步（STEP-7.2 前置条件）

✅ 就绪：
- 应用层 idle 检测全仓 0 残留；连接生命周期判定完全交由 QUIC（5s keepalive / 30s idle）
- `listen.rs` supervisor 循环为裸 `read_frame` 直调，错误分流（FrameTooLarge fatal / decode warn+skip / Truncated break）完整
- `cargo build -p lan-mouse` 通过

⏸ **STEP-7.2 需注意**：
- `tests/quic_smoke.rs` 应显式覆盖"静默 ≥10s 连接存活"用例，把 §4.5 手动步骤自动化（这是 STEP-7.1 唯一未自动验证的验收点）
- SUGGESTION #S-5 的 25 pre-existing fixture errors 仍可能阻塞 `cargo test -p lan-mouse` —— STEP-7.2 需先处理

**未做 git commit**：等 Leader 处理（本步动 1 文件：`src/quic_transport.rs`，仅 doc 注释 2 行 → 4 行）。
