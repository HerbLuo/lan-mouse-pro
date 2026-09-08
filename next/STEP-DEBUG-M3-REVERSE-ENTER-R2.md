# STEP-DEBUG-M3-REVERSE-ENTER-R2 — 静态代码再排查（用户跑 trace 后两边全无日志）

> 调研日期：2026-09-08
> 触发信号：用户在 commit `a2f88c4` 加上 7 条 `debug(temp)` trace log 后跑真机反向切，**反向切完全无日志**（两端都没出现 trace log）
> 调研范围：service.rs / capture.rs / libei.rs / macos.rs / emulation.rs / listen.rs（重点 H6 + 物理 barrier 安装路径）
> 与 STEP-DEBUG-M3-REVERSE-ENTER.md H1-H5 互补，本文重点 H6 + libei/install 真根因
> 用户**没有新的 trace 输出**——本次调研靠静态读代码 + 复盘 commit 历史

---

## §1 已确认事实（用户跑 trace 后看到什么）

### 1.1 用户现状报告

| 项 | 状态 |
|---|---|
| commit `a2f88c4` 7 条 `debug(temp)` trace log 已合入 | ✅ |
| `cargo build --workspace` | ✅ |
| `cargo test --workspace` | ✅ 164 pass / 0 fail |
| 跑 `RUST_LOG='lan_mouse=trace,lan_mouse_service=trace,input_capture=trace'` 主控 + 被控 | ✅ |
| 正向切（master→slave）`debug(temp)` trace 出现 | ✅ 出现 |
| 反向切（slave→master）`debug(temp)` trace | ❌ **两端都不出现** |

### 1.2 "两边都无日志"的强约束

反向切的事件链（§1.3）上**至少有一条** trace log 应该在两端某处出现。如果**全部 7 条都不出现**，意味着事件链**在最开头就断了**：

| 序号 | trace log | 位置 | 触发条件 |
|---|---|---|---|
| 1 | `debug(temp) CaptureBegin handle=...` | service.rs:466-470 | 被控 ICaptureEvent::CaptureBegin 收到 |
| 2a | `debug(temp) add_incoming ENTRY ...` | service.rs:588-592 | 被控 add_incoming 入口（应该正向切已触发过） |
| 2b | `debug(temp) add_incoming POST-INSERT ...` | service.rs:604 | 被控 add_incoming 完成 |
| 3 | `debug(temp) WARN stale addr ...` | listen.rs:322-325 | reply 找不到 peer（依赖 4） |
| 4 | `debug(temp) libei Begin barrier=...` | libei.rs:772-774 | **被控 libei 物理 barrier fire Activated** ← 链最开端 |
| 5 | `debug(temp) Capture::release called, ...` | macos.rs:1521 | 主控 macOS `Capture::release` 入口（依赖 Leave 到达） |
| 6 | `debug(temp) capture task forwarding Release ...` | capture.rs:1600-1603 | 主控 capture task release 转发（依赖 Leave 到达） |
| 7 | `debug(temp) producer received Release ...` | macos.rs:307-311 | 主控 macOS producer 收 Release（依赖 Leave 到达） |

**结论**：trace log 4（libei.rs:772-774）**没出现**= libei 物理 barrier **根本没 fire Activated**。这是 reverse 完全失败的**直接观察**。

### 1.3 期望的反向事件链（被控为主）

```
slave.user crosses slave left edge
  ↓
[被控] libei portal → Activated event (libei.rs:760)
  ↓
[被控] log::trace!("debug(temp) libei Begin barrier=...")   ← (4) **缺失**
  ↓
[被控] libei → event_tx.send((key, Begin))
  ↓
[被控] capture.rs:1206 → log::trace!("({handle}): {event:?}")
  ↓
[被控] capture.rs:1247 EnterOnly handler → log::info!("capture: EnterOnly trigger...")
  ↓
[被控] event_tx.send(ICaptureEvent::CaptureBegin(handle))
  ↓
[被控] service.rs:466 → log::trace!("debug(temp) CaptureBegin handle=...")   ← (1) **缺失**
  ↓
[被控] send_leave_event(master_addr)
  ↓
[被控] emulation.rs:236 → listener.reply(master_addr, Leave(0))
  ↓
[主控] listen.rs:reply() → log::info!("reply: Leave to {addr} delivered")
  ↓
[主控] capture.rs:857 → log::info!("releasing capture: left remote client device region")
  ↓
[主控] release_capture() → log::info!("release_capture: calling capture.release()...")
  ↓
[主控] capture.rs:1600 → log::trace!("debug(temp) capture task forwarding Release...")   ← (6) **缺失**
  ↓
[主控] macOS::Capture::release → log::trace!("debug(temp) Capture::release called...")   ← (5) **缺失**
  ↓
[主控] macOS producer → log::trace!("debug(temp) producer received Release...")   ← (7) **缺失**
```

**反向切失败 = trace log 4 缺失 = libei portal 没发 Activated 事件。**

---

## §2 libei EnterOnly barrier 当前实现状态

### 2.1 EnterOnly barrier 在哪条代码路径建立？

**`service.rs::add_incoming` (line 576-605)** — 唯一路径：

```rust
fn add_incoming(&mut self, addr: SocketAddr, pos: Position, fingerprint: String) {
    let handle = Self::ENTER_HANDLE_BEGIN + self.next_trigger_handle;
    self.next_trigger_handle += 1;
    let key = crate::capture::to_capture_pos(pos);
    let key = input_capture::BarrierKey::from_pos(key);  // {pos, monitor:None, offset:0, span:10000}
    log::trace!("debug(temp) add_incoming ENTRY ...");
    self.capture.create(handle, &key, CaptureType::EnterOnly);  // ← 唯一调用点
    self.incoming_conns.insert(addr);
    self.incoming_conn_info.insert(handle, Incoming { ... });
    log::trace!("debug(temp) add_incoming POST-INSERT ...");
}
```

`capture.create(handle, &key, CaptureType::EnterOnly)` (capture.rs:338-347) fire-and-forget 发 `CaptureRequest::Create` 给 CaptureTask。

### 2.2 CaptureTask 怎么转给 libei backend？

**`capture.rs:880-886` 在 `do_capture_session` 内**（只有这一处真正调 backend `capture.create`）：

```rust
CaptureRequest::Create(h, p, t) => {
    self.add_capture(h, &p, t);
    capture.create(h, &p).await?;  // ← key, NOT capture_type
}
```

**注意**：传给 backend 的 `capture.create(h, &p)` **不传 `t` (CaptureType)**！backend 只能拿到 `BarrierKey`，**完全没有 EnterOnly 概念的接口**。backend 不知道这个 barrier 是 EnterOnly 还是 Default。

### 2.3 libei backend 怎么装 barrier？

**`input-capture/src/libei.rs:948-958` `LibeiInputCapture::create`**：

```rust
async fn create(&mut self, key: &BarrierKey) -> Result<(), CaptureError> {
    let _ = self
        .notify_capture
        .send(LibeiNotifyEvent::Create(key.clone()))  // 把整个 BarrierKey 转发到 libei task
        .await;
    Ok(())
}
```

**libei task (`libei.rs:559-702`) 处理 `LibeiNotifyEvent::Create`**：

```rust
// libei.rs:692
LibeiNotifyEvent::Create(k) => active_clients.push(k),
```

`active_clients: Vec<BarrierKey>` 是 libei session 的"客户表"，**只在 Create/Destroy 事件里改**。

### 2.4 EnterOnly barrier 在 libei 里到底是个什么 barrier？

**关键发现：libei 的 `select_barriers` 没有任何"EnterOnly mode"概念。**

`libei.rs:277-303` 的 `select_barriers`：
- 每个 client × 每个 region 生成一条 `ICBarrier { barrier_id, position }`
- `Barrier::new(barrier_id, position)` —— ashpd 的 `Barrier` 只接受 id + position，**没有 mode 字段**
- 所有 barrier **都通过同一个 `set_pointer_barriers(session, &barriers, zone_set, ...)` 一次性提交给 portal**（libei.rs:328-335）

**所以 EnterOnly barrier 在 libei 里就是一条普通的 left-edge 物理 barrier**：
- `pos = Left`：垂直线段 `(x, y, x, y + h - 1)`（libei.rs:293）
- 装在被控屏幕**左边的全高线段**（每个 region 一条）

### 2.5 EnterOnly barrier 何时被销毁？

- `service.rs::remove_incoming` (line 635-646) 在 `update_incoming` 检测到 pos/fp 变化时调
- `service.rs::deactivate_client` (line 685-692) 在用户关闭某个 client 时调
- `service.rs::remove_client` (line 769-779) 在用户删除 client 时调
- **正向切的"反向 barrier"（EnterOnly）稳态下不会被销毁** —— `service.rs::handle_emulation_event(Disconnected)` (line 389-422) **主动保留 barrier**（注释明示"fast recovery"）

### 2.6 macOS 是否支持 EnterOnly barrier？

**不支持（也不需要）**。macOS 后端只有 Default + Grab（legacy）模式。`capture_type` 在 capture.rs:885 传给 `capture.create()` 时已经被丢弃，macOS 收不到这个区分。

macOS CGEventTap 只关心 `active_clients: HashSet<BarrierKey>` 的 `crossed()` 查询（macos.rs:155-164），EnterOnly / Default 都在这个集合里。但 macOS 不需要 EnterOnly——主控端的反向 barrier 由被控端发 Enter 后**主控的 `add_incoming` 路径**装在主控自己的 libei/macOS 上。所以**主控 macOS 不需要 EnterOnly 支持**。

### 2.7 EnterOnly barrier 真在 slave libei 上吗？

**静态读代码：是的，应该在**（slave 是 libei 后端）。

slave 收到 master 发来的 Enter(Left) 后：
1. `emulation.rs:173` 发 `EmulationEvent::ReleaseNotify` → `service.rs:439` → `capture.release()` → CaptureRequest::Release → CaptureTask → release_capture → capture.release() → libei `notify_release.notify_waiters()`（**不销毁 active_clients，只发信号**）
2. `emulation.rs:176` 发 `EmulationEvent::Entered` → `service.rs:379` → `add_incoming(addr, pos=Left, fp)` → `CaptureTask` → `capture.create(handle, &key, EnterOnly)`（line 593）
3. CaptureTask 把 Create 转给 libei `notify_capture.send(LibeiNotifyEvent::Create(key))` → libei task → `active_clients.push(key)` (libei.rs:692)
4. libei `do_capture` 取消当前 session (cancel_session.cancel())，迭代：`active_clients` 非空 → 重新 `do_capture_session` → `update_barriers(regions, active_clients, ...)` → `select_barriers` 给 slave 左 edge **每个 region 一条 ICBarrier**（包含 Default + EnterOnly）
5. `input_capture.enable(session)` → portal 激活 barrier

**到这里 slave 的 libei portal 上应该有 EnterOnly barrier 在 slave 左 edge**（按用户的 macOS master + Linux slave 几何：master.client.pos=Right, slave 左 edge = slave.client.pos=Left = master 方向）。

---

## §3 最可能的根因（按概率排序）

### 候选 1（H6，**~70%**）— `EmulationEvent::ReleaseNotify` 在被控 daemon 启动时**触发 libei session 重建，错过 EnterOnly barrier install**

#### 现象与定位

`emulation.rs:160-176` 收到 master 发来的 Enter 时**同步**做 3 件事（顺序固定）：

```rust
ProtoEvent::Enter(pos) => {
    // ...
    self.event_tx.send(EmulationEvent::ReleaseNotify).expect("...");  // ← (a)
    log::info!("emulation: sending Ack(0) to {addr}");
    self.listener.reply(addr, ProtoEvent::Ack(0)).await;              // ← (b) async
    self.event_tx.send(EmulationEvent::Entered { addr, pos, ... }).expect("...");  // ← (c)
}
```

`service.rs` 的 select! 处理这俩事件：
- 收到 (a) `ReleaseNotify` → `self.capture.release()` (line 439)
  - `Capture::release` (capture.rs:355-359) 发 `CaptureRequest::Release` → CaptureTask
  - CaptureTask 在 `do_capture_session` recv arm 收到 → `release_capture(capture).await?` (capture.rs:882)
  - `release_capture` (capture.rs:1481-1627):
    - `state` 不是 Pending（slave 未在 capture）
    - `active_client` 是 None（slave 未在 capture）
    - log "release_capture: no active_client, skipping Leave send"
    - **`self.state = State::Idle`** (line 1593-1594)
    - **`capture.release().await`** (line 1604) → libei `notify_release.notify_waiters()`
- 收到 (c) `Entered` → `add_incoming` → `CaptureRequest::Create(handle, key, EnterOnly)` → CaptureTask → `capture.create(h, &p).await` → libei `notify_capture.send(LibeiNotifyEvent::Create(key))`

#### libei 收到这两条后的真实行为

`libei.rs:608-687` 的 `do_capture` 循环：

```rust
if !active_clients.is_empty() {
    let capture_session = do_capture_session(input_capture, &mut session, &event_tx,
                                              &active_clients, &mut next_barrier_id,
                                              &notify_release, (cancel_session.clone(), cancel_update.clone()));
    let (capture_result, ()) = tokio::join!(capture_session, handle_session_update_request);
    // ...
    input_capture.disable(&session, Default::default()).await;  // 关旧 session
    session.close().await;                                       // 关旧 session
    capture_result?;
} else {
    handle_session_update_request.await;
}

if let Some(event) = capture_event_occured.take() {
    LibeiNotifyEvent::Create(k) => active_clients.push(k),
    LibeiNotifyEvent::Destroy(k) => active_clients.retain(...),
}
```

**关键 race**：slave 启动时如果 `active_clients` 已经非空（因为 slave 有 `activate_client` 装了 Default barrier），那么 do_capture 正在 active 分支。`handle_session_update_request` 和 `do_capture_session` 并行跑。

slave 收 Enter 时：
- (a) `ReleaseNotify` → libei `notify_release.notify_waiters()` → 在 do_capture_session **外层 select** 的 `_ = notify_release.notified() => { /* we are not capturing anyway, so ignore */ }` (libei.rs:814-816) arm 触发 → 静默 log → **不破坏 session**
- (c) `Entered` → add_incoming → `notify_capture.send(LibeiNotifyEvent::Create)` → `handle_session_update_request` 的 select! 捕获 → `capture_event_occured = Some(Create(k))` → `cancel_session.cancel()` → tokio::join! 等 do_capture_session 退出
- do_capture_session 退出（cancel_session arm break）
- disable + close
- active_clients.push(k) ← EnterOnly 加入 active_clients
- 下一轮 do_capture 迭代：`active_clients.is_empty()` 为 false → 重新 `do_capture_session`
- 重新 `update_barriers(regions, active_clients, ...)` → **重新装 Default + EnterOnly 两条 barrier**

**到这里看起来一切正常**。但 H6 假说的精妙之处在于：

**H6 假设被控 daemon 启动时 `active_clients`**：
1. **情况 A**：slave 没配 outgoing client（只被 master 单向控制）→ `active_clients = []` → libei 在 idle 分支。slave 收 Enter：
   - (a) `ReleaseNotify` → libei `notify_release.notify_waiters()` → **idle 分支不在 notify_release 上 select**，信号被丢弃
   - (c) `Entered` → add_incoming → `notify_capture` → idle 分支的 `capture_event.recv()` arm 触发 → `capture_event_occured = Some(Create)` → `cancel_session.cancel()` → `handle_session_update_request.await` 返回
   - `active_clients.push(k)` → 下一轮 do_capture：`active_clients` 非空 → **建新 session** → set_pointer_barriers → enable
   - ✅ EnterOnly barrier 装好了
2. **情况 B**：slave 配了 outgoing client (active=true) → `active_clients = [Default]` → libei 在 active 分支，session 已 enable，Default barrier 已装。slave 收 Enter：
   - (a) `ReleaseNotify` → 同上，忽略
   - (c) `Entered` → add_incoming → `notify_capture.send(Create)` → `handle_session_update_request` 捕获 → cancel_session → join 退出 → disable + close
   - `active_clients.push(EnterOnly)` → 下一轮 do_capture：建新 session → set_pointer_barriers（Default + EnterOnly 都在 active_clients 里）→ enable
   - ✅ EnterOnly barrier 装好了

**情况 A 和 B 静态分析都不破**。但用户报告"完全无日志"。所以问题不在 install 路径上，而在 install **之后**或 **install 的时机不对**。

#### H6 假设里被掩盖的子候选

LEADER-STATE H6 原文说"capture.release() 销毁所有 OS-level barrier"——**这在 libei 上不严格成立**（`notify_release` 不清 `active_clients`）。但有一种微妙的情况：

- **race**：slave 收 Enter 时 (a)(c) 的处理顺序可能不是顺序的。如果 `add_incoming` 收到的 handle 已经存在（reuse 旧 handle），`update_incoming` 路径会走 remove + add（service.rs:607-633）。但用户是首次收 Enter，handle 是新分配，不会触发 update_incoming。
- **真正问题**：slave 的 libei **重启 session 时 disable + close + 新建需要时间**（DBus roundtrips）。如果用户在 session 重建期间又把鼠标移到 slave 左 edge，barrier 还没装，**首次反向切换失败**——但这是 first-switch race，不是稳态 bug。

**所以 H6 的原始说法需要修正**：libei 上 capture.release() **不直接销毁 barrier**，但**会让 session 重建**。重建期间如果用户太快切回，可能错过 barrier。这能解释"首次反向切换失败"**，但不能解释"持续反向失败"**。

#### 修复方向（如果 H6 子候选命中）

**最小改动**：让被控 daemon 在 add_incoming 时**不触发 release_capture**。具体做法：
- 在 `emulation.rs:160-176` 把 `ReleaseNotify` 移除，或者
- 在 `service.rs:439` 加条件：`if self.capture.has_active_client() { self.capture.release(); }`（被控没有 active client 时 no-op）
- 但要小心：master 端如果也是同一份 binary，master 的 ReleaseNotify 必须继续 release_capture

**最稳妥**：在 `add_incoming` 路径里，**绕过 `capture.release()`**：
```rust
// emulation.rs:160-176 改成只发 Entered，不发 ReleaseNotify
// 这样被控端 add_incoming 不会触发 release_capture
```

但这样会破坏 master 端"收到 Enter 时释放 capture"的设计（master 收到 Enter 时确实在 capture，需要 release_capture）。

**折中**：判断"是否在被控路径"——被控路径的特征是 incoming_conn_info 已经有相同 addr 的 entry，或者 fingerprint 不同 / pos 相同（master 端收到 Enter 时 fingerprint 是 master 自己的，从 Entered 上看是"被控"，但 master 自己不会收到 Enter）。

更彻底的方案：**拆 add_incoming 和 capture.release**。`add_incoming` 只负责装 EnterOnly barrier，**不再调 capture.release()**。master 端 release_capture 由 macOS 的 CGEventTap → BeginPending → Ack 路径自己处理。

#### 验证方法（如果 H6 命中）

- 看被控 daemon 日志：
  - 期望看到 `debug(temp) add_incoming ENTRY ...` (forward 切已有)
  - 期望看到 `add_incoming POST-INSERT handle=H_n key=...` 
  - 期望看到 libei 装 barrier 后的 `barriers: [ICBarrier { barrier_id: N, position: (x, y, x, y+h-1) }]` 之类的 debug
  - 期望看到 `enabling session`
- 如果 add_incoming 已经执行，但 libei session 反复 disable + close，可能 release_capture 持续干扰
- **真根因验证**：在 `emulation.rs:173` 暂时**不发 ReleaseNotify**，看反向切是否 work。如果 work → H6 命中。如果不 work → H6 不命中。

#### 时间预估

- 加临时 log 验证：~10 min
- 实施修复（emulation.rs 改一行 + listen.rs 同步调整）：~15 min
- 单测 + 真机回归：~30 min
- 移除 trace log：~5 min
- **总计：~1h**

---

### 候选 2（**~20%**）— libei session **未真正 enable**，barrier 在 portal 层是 inactive

#### 现象与定位

`libei.rs:704-848` `do_capture_session`：

```rust
let (context, _conn, ei_event_stream) = connect_to_eis(input_capture, session).await?;
let (barriers, key_for_barrier_id) =
    update_barriers(input_capture, session, active_clients, next_barrier_id).await?;
log::debug!("enabling session");
input_capture.enable(session, Default::default()).await?;  // ← 必须成功
```

如果 `input_capture.enable()` 失败（DBus 错误 / portal 拒绝 / permissions），整个 `do_capture_session` 返回 Err（libei.rs:797-836），然后 capture.rs 的 `do_capture` 返回 Err，**CaptureTask 退到外层 request_rx loop**：

```rust
async fn run(mut self) {
    loop {
        if let Err(e) = self.do_capture().await {  // ← 这里返回 Err
            log::warn!("input capture exited: {e}");  // 那就有 WARN 日志
        }
        loop {
            tokio::select! {
                r = self.request_rx.recv() => match r {
                    // ...
                    CaptureRequest::Create(h, p, t) => self.add_capture(h, &p, t),  // ← 没有 capture.create !!!
                    // ...
                }
            }
        }
    }
}
```

**注意**：外层 loop 的 `CaptureRequest::Create` handler **只调 `self.add_capture`**，**不再调 `capture.create()`**！意味着 libei 的 `notify_capture` 永远不会再收到 Create 事件。

如果 `do_capture_session` 因 enable 失败而返回 Err，**所有后续 Create 请求都被吞掉**——但用户已经看到 add_incoming 第一次成功（forward 切 work），所以**首次 enable 是成功的**。第二次 enable（加 EnterOnly 后）也可能成功，但**H6 子候选会让 enable 反复被打断**。

#### 假设链路

1. slave 启动 → activate_client (Default) → libei session 启动 → enable 成功 ✅
2. master 发 Enter → slave 收 Enter → emulation.rs:173 ReleaseNotify → slave capture.release() → libei notify_release
3. libei do_capture_session 的外层 select arm `_ = notify_release.notified() => { /* we are not capturing anyway, so ignore */ }` 触发 → loop 继续
4. (parallel) emulation.rs:176 Entered → add_incoming → CaptureRequest::Create(EnterOnly) → libei notify_capture
5. libei do_capture_session 还在 outer select 阶段 → handle_session_update_request 收到 Create → cancel_session.cancel()
6. do_capture_session break → join 返回 → disable + close 旧 session
7. active_clients.push(EnterOnly)
8. 新 do_capture_session 启动 → connect_to_eis → update_barriers (装 Default + EnterOnly 两 barrier) → enable

如果第 8 步的 enable 失败，整个 do_capture_session 返回 Err → 退到外层 loop。之后所有 Create 都被吞掉。但用户报告 forward 切 work，意味着第 8 步成功。

#### 修复方向

- 在 libei.rs:797 `enable()` 失败时，log error 但 **继续**（不返回 Err）：
  ```rust
  if let Err(e) = input_capture.enable(session, Default::default()).await {
      log::error!("enable failed: {e}");  // 不返回 Err，让 session 继续
  }
  ```
- 这样 session 失败不会让 CaptureTask 退到外层 loop
- 但风险：barrier 没真的 enable，后续 Activated 永远不 fire

#### 验证方法

- 看被控 daemon 日志里 `enabling session` 之后有没有 portal 错误
- 跑 `journalctl -f` 看 xdg-desktop-portal 的日志

#### 时间预估

- 不太可能命中（H6 子候选更直接）

---

### 候选 3（**~10%**）— barrier 装到了**错的 edge**

#### 现象与定位

master 的 client config `pos = ?` 是用户配置的；如果用户配错，slave 上 EnterOnly barrier 可能装在**背离 master 的 edge**，cursor 跨过正确的 edge 时不触发 Activated。

`service.rs::add_incoming` 用 master 发来的 Enter(pos) 决定 slave 的 EnterOnly barrier 位置：

```rust
let key = crate::capture::to_capture_pos(pos);  // ipc::Position → input_capture::Position
let key = input_capture::BarrierKey::from_pos(key);  // 包装 BarrierKey
```

`emulation.rs:176` 把 `to_ipc_pos(pos)` 转 IPC 给 service.rs（slave 看到的是 `lan_mouse_ipc::Position`）。

master 端（capture.rs:1303-1307）：
```rust
let opposite_pos = to_proto_pos(key.pos.opposite());
self.conn.send(ProtoEvent::Enter(opposite_pos), handle).await
```

master.client.pos = Right → opposite_pos = Left → master 发 Enter(Left) → slave 收 Enter(Left) → slave 装 barrier 在 slave 屏幕的 **Left edge**（按用户 master 在 slave 左边的几何）。

**正确条件**：master 在 slave 左边（slave 的 Left edge 面对 master）。

如果用户配反了（master 在 slave 右边，master.client.pos = Left），master 发 Enter(Right)，slave 装 barrier 在 slave Right edge。用户的物理鼠标移到 slave 的 Left edge 时不触发 barrier。

但**用户报告 forward 切 work**，这意味着几何配对至少部分正确。forward 切 work 意味着 master 跨 master 自己的 edge 能触发 Enter 到 slave。reverse 切 fail 意味着 slave 端 EnterOnly barrier 没 fire。

#### 假设链路

- 用户 master.client.pos = Right（master 在 slave 左边）
- master 跨 master 右 edge → master 发 Enter(Left) → slave 装 barrier 在 slave 左 edge ✅
- 用户鼠标移到 slave 左 edge → 应该触发 barrier
- 但**实际不触发**？

唯一能让 barrier 不触发的情况：
- barrier 装在错的 edge（用户配错，或 libei 的 region 几何算错）
- cursor 没真的跨过 barrier（用户操作问题）

#### 修复方向

- 加 trace log：dump 装 barrier 的 position（region 的 x, y, w, h）+ 计算后的 barrier position
- 让用户跑 `RUST_LOG=input_capture=trace` 看 libei `barriers: [ICBarrier { barrier_id: N, position: ... }]`

#### 验证方法

- 在 libei.rs:327 加 log 打印装的所有 barrier 的 position 和 active_clients
- 在 libei.rs:796 后（Begin 发送前）log 装好的 barrier_id 和 key_for_barrier_id 映射
- 让用户跑反向切，看 `libei Begin barrier=X position=Y` trace 里 X 是否对应 EnterOnly barrier（key = { pos: Left, monitor: None }）

#### 时间预估

- 加 log：~10 min
- 用户跑：~10 min
- 如果命中 → 修配置 / 修算法：~30 min
- **总计：~50 min**

---

### 候选 4（**~5%**）— slave 没有 Default barrier 也没有 EnterOnly barrier，因为 libei `active_clients` 启动时为空且 Enter 没触发 do_capture 重新激活

#### 现象与定位

**一个诡异场景**：slave 启动时 `active_clients = []`，libei 在 idle 分支。master 发 Enter 到 slave：
- (a) ReleaseNotify → libei notify_release（idle 分支不消费）
- (c) Entered → add_incoming → notify_capture → idle 分支 capture_event.recv() 触发 → capture_event_occured = Some(Create) → cancel_session.cancel() (no-op)
- await 返回 → active_clients.push(EnterOnly)
- 下一轮 do_capture：active_clients 非空 → create session → update_barriers(EnterOnly barrier at slave left) → enable

这应该 work。除非 create_session 失败（DBus unavailable / portal off）。

#### 假设链路

- slave 启动 → no active_clients → idle 分支
- master 发 Enter → slave emulation 收 → service.rs add_incoming → notify_capture
- 但 libei idle 分支的 `capture_event.recv()` 没触发？

不太可能——`capture_event` 是 mpsc::channel，send 后 recv() 应该立即 ready。

#### 验证方法

- 加 trace log 在 libei.rs:691-695 处理 Create/Destroy 时打印 active_clients 长度
- 在 libei.rs:599 capture_event.recv() 触发时 log "capture event: ..."

#### 修复方向

- 如果 active_clients 真的为空时 EnterOnly barrier 没装，可能是 capture_event channel 关闭 / 满
- 几乎不可能

#### 时间预估

- 不太可能命中

---

### 候选 5（**~5%**）— capture.rs `create_captures` 在 do_capture 重启时**丢失 active_clients**

#### 现象与定位

`capture.rs:596-634` `do_capture`：

```rust
async fn do_capture(&mut self) -> Result<(), InputCaptureError> {
    let mut capture = ...;
    let initial = capture.monitors();
    self.last_monitors = initial.clone();
    let _ = self.event_tx.send(ICaptureEvent::MonitorsChanged(initial));
    let _capture_guard = ...;
    let r = self.create_captures(&mut capture).await;  // ← 把 self.captures 复制后逐个 install
    ...
}
```

```rust
async fn create_captures(&mut self, capture: &mut InputCapture) -> Result<(), CaptureError> {
    let captures = self.captures.clone();
    for (handle, pos, _type) in captures {
        tokio::select! {
            r = capture.create(handle, &pos) => r?,
            ...
        }
    }
    Ok(())
}
```

`self.captures` 是 `Vec<(CaptureHandle, BarrierKey, CaptureType)>`，**包含所有 add_capture 过的 capture**（Default + EnterOnly 都在）。

但注意 `capture.create` 是在 InputCapture 上调的（capture.rs:149-165）：
```rust
pub async fn create(&mut self, id: CaptureHandle, key: &BarrierKey) -> Result<(), CaptureError> {
    assert!(!self.id_map.contains_key(&id));
    self.id_map.insert(id, key.clone());
    if let Some(v) = self.position_map.get_mut(key) {
        v.push(id);
        Ok(())
    } else {
        self.position_map.insert(key.clone(), vec![id]);
        self.capture.create(key).await  // ← backend-specific create
    }
}
```

如果两次 create 调用同一个 key 走的是 `if let Some(v)` 分支（push id），不会重复调 backend.create。但如果 key 不同，每个不同 key 调一次 backend.create。

#### 假设链路

- do_capture 重启（disable + close + new session）→ capture (InputCapture trait object) 重新创建（通过 InputCapture::new）
- create_captures 把 self.captures 复制 → 调 capture.create(handle, &key) → InputCapture.create → 如果新 key 调 backend.create
- 对于 EnterOnly barrier（key = { Left, None, 0, 10000 }），如果跟 Default barrier key 不同（Default 也是 Left + None + 0 + 10000），它们是**同一个 key**（BarrierKey::from_pos(Left) 一样）
- 所以 **Default 和 EnterOnly 在 `position_map` 里是同一个 entry**！

**BUG 候选**：`capture.create(H_enter_only, key)` 进入 InputCapture.create：
```rust
if let Some(v) = self.position_map.get_mut(key) {
    v.push(id);
    Ok(())  // ← 不调 backend.create !!!
}
```

第一次创建（Default）：`position_map` 是空的 → `position_map.insert(key, vec![H_default])` + `self.capture.create(key)` → backend.create
第二次创建（EnterOnly）：`position_map.get_mut(key) = Some([H_default])` → `v.push(H_enter_only)` → **不调 backend.create**，直接返回

**这意味着 libei backend 的 `notify_capture.send(LibeiNotifyEvent::Create(key))` 不会被调用第二次**。

但这只影响**第二次 create**。第一次（Default）在 `activate_client` 路径已经调过（如果 slave 有 outgoing client）。**EnterOnly 在 do_capture 重启时的 create_captures 路径里会被 InputCapture.create 短路掉，不发到 libei**。

但是！在 do_capture_session 内的 `CaptureRequest::Create` handler (capture.rs:883-886)：
```rust
CaptureRequest::Create(h, p, t) => {
    self.add_capture(h, &p, t);
    capture.create(h, &p).await?;  // ← 直接调 backend，绕过 InputCapture.create 的去重
}
```

这里调的是 `capture.create`，但这是 `&mut InputCapture`，所以走的是 `InputCapture::create`。同样会被 position_map 去重。

**但 libei 的 Create 事件是从这里发的**：
```rust
async fn create(&mut self, id: CaptureHandle, key: &BarrierKey) -> Result<(), CaptureError> {
    assert!(!self.id_map.contains_key(&id));
    self.id_map.insert(id, key.clone());
    if let Some(v) = self.position_map.get_mut(key) {
        v.push(id);
        Ok(())  // ← 不调 self.capture.create
    } else {
        self.position_map.insert(key.clone(), vec![id]);
        self.capture.create(key).await  // ← 只在 key 第一次出现时调
    }
}
```

所以**两个 handle 共享一个 key 时，backend.create 只调一次**。这意味着 libei 的 `active_clients` 只 push 一次（Default），EnterOnly 的 Create 被吞掉！

**这是真 bug 候选！** 但只在 InputCapture::new 之后才出现。do_capture 重启 → InputCapture::new → position_map 空 → 第一次 create 走 backend.create → 之后 create 同 key 的不再发。

但实际上在 `do_capture_session` 的 recv arm 里：
```rust
CaptureRequest::Create(h, p, t) => {
    self.add_capture(h, &p, t);
    capture.create(h, &p).await?;  // ← h 是 EnterOnly 的新 handle（H0），p 是 EnterOnly 的 key
}
```

`capture.create(H_enter_only, key_enter_only)` → InputCapture::create (id=H_enter_only, key=key_enter_only) → if position_map contains key: push H_enter_only to vec → return early

**所以 backend 永远不被通知 EnterOnly**！

这个 bug 假设 H_enter_only 在 H_default 之后调用，且两者 share same key。

但**在 add_incoming 之前** Default 已经存在（slave.client.pos = Left + activate_client）。所以 Default 的 create 在 slave 启动时就调过 backend.create。EnterOnly 的 create 走 `position_map.get_mut` 短路，**backend 永远不知道 EnterOnly**。

**但 libei 的 active_clients 是 BarrierKey 列表，不是 handle 列表**。EnterOnly 和 Default 用同一个 key（`BarrierKey::from_pos(Left)`），所以**它们在 active_clients 里也只占一个 entry**。

也就是说**libei 只装了一条 barrier（Default 的 barrier），但这条 barrier 既响应 Default 也响应 EnterOnly**（在 capture.rs 里按 handle 路由）。

但**正向切时** Default barrier fire BeginPending → state = Pending → send Enter to master。这是 forward 切方向。当 forward 切成功后，user 实际在 master 上（因为 master 在 capture，input 转发到 slave 那边）。**这时候 slave 的 Default barrier 没在 fire**（用户不在 slave 屏幕），所以**反向切时 EnterOnly 也没在 fire**——因为它们是同一条 barrier！

**这就是真正的 H6-ish 根因**：
- **Default barrier + EnterOnly barrier 共享 key（都是 `BarrierKey::from_pos(Left)`）**
- libei 的 active_clients + position_map 把它们合并为**一条物理 barrier**
- capture.rs 按 handle 路由事件（Default 走 Default 路径，EnterOnly 走 EnterOnly 路径）
- 但 libei 只 fire 一条 barrier（不管 handle 是 Default 还是 EnterOnly）
- 这条 barrier fire 的事件被路由到**第一个 add_capture 的 handle**（Default），所以 EnterOnly handler 永远收不到 Begin 事件

**等等，这不对**。capture.rs 的 handle_capture_event 是按 (handle, event) 路由的，handle 是 libei 给的，不是 CaptureHandle。让我重新看一下。

`capture.rs:1200-1205`：
```rust
async fn handle_capture_event(
    &mut self,
    capture: &mut InputCapture,
    event: (CaptureHandle, CaptureEvent),
) -> Result<(), CaptureError> {
```

`(handle, event)` 是 `(CaptureHandle, CaptureEvent)` 来自 `capture.next()` (libei.rs:795 `event_tx.send((key.clone(), CaptureEvent::Begin))`)。

实际上 libei 给的是 `(BarrierKey, CaptureEvent)`，不是 `(CaptureHandle, ...)`。让我再看一次 InputCapture 的 next。

实际上 InputCapture 把 BarrierKey → Vec<CaptureHandle> 映射（lib.rs:325）：
```rust
let subscribers = self.position_map.get(&key).cloned().unwrap_or_default();
```

把一条 libei 事件 fan-out 给所有 share 同一个 key 的 handle。

所以当 libei fire BarrierKey{Left} 的 barrier 时：
- subscribers = [H_default, H_enter_only]（如果两个 handle 都 share 这个 key）
- 给每个 subscriber 发 `(H_default, Begin)` 和 `(H_enter_only, Begin)`
- capture.rs 收到两个事件
- 第一个 (H_default, Begin)：handle_capture_event 看到 Default 类型 → state = Pending → send Enter(opposite_pos)
- 第二个 (H_enter_only, Begin)：handle_capture_event 看到 EnterOnly 类型 → emit CaptureBegin → service.rs send_leave_event

**所以 EnterOnly handler 应该也收到 Begin**。前提是 libei 真的 fire BarrierKey{Left} 的 barrier。

OK 所以候选 5 不是真 bug。让我重新梳理。

回到 H6。LEADER-STATE H6 说的 "capture.release() 销毁所有 OS-level barrier" 在 libei 上不严格成立，但语义上有相关性：每次 master 发 Enter 到 slave，slave 都跑一遍 release_capture → capture.release() → libei notify_release。这个信号本身不销毁 barrier，但让 session 重建。

**真正的潜在 bug 候选**：如果 release_capture 在 `state` 是 Pending 时走一个特殊路径：

```rust
// capture.rs:1498-1527
if let State::Pending { handle, ref key, .. } = self.state {
    log::info!("release_capture: was in Pending for handle {handle} - cancel_pending...");
    if let Err(e) = capture.cancel_pending(key) {
        log::warn!("cancel_pending in release_capture: {e}");
    }
    self.state = State::Idle;
    let res = capture.release().await;  // ← 第一次 capture.release
    ...
    return res;
}
```

**这条分支只在 `self.state == Pending` 时走**。slave 启动时 state=Idle，forward 切 work 后 master 进入 Sending，slave 还是 Idle（slave 不 capture）。所以这条分支不触发。

除非 slave 自己 fire 了 Default barrier（slave 用户跨 slave 左 edge），slave capture.rs state 进入 Pending。这时如果有 ReleaseNotify 同时到... 不太可能，race window 很小。

OK 让我换个思路。让我看 H1（上一份报告的 H1）—— add_incoming 真的会执行吗？

正向切 work 意味着 add_incoming 被调用过（service.rs:579 add_incoming via EmulationEvent::Entered）。forward 切 work 意味着：
- master capture.rs 收到 Ack（line 786）→ start_capture → Begin → state = Sending
- master forwarding input to slave via conn.send

但正向切成功**不要求** add_incoming 成功！只要 master 把 input 转发到 slave 就行。add_incoming 在 slave 端，slave 收到 Enter 后处理 add_incoming 来装 EnterOnly barrier 用于反向触发。

但正向切的时候，slave 收到 Enter，必须处理 add_incoming 才能装 barrier。如果 add_incoming 永远失败（比如 trace log 没出现）但 slave 还是收到了 input，那说明 master 把 input 转发到 slave 的路径不依赖 add_incoming。

所以"正向切 work" 不直接证明"add_incoming 被调用"。

但用户的 trace log 2a（add_incoming ENTRY）应该出现在 forward 切。如果用户说 "正向切有日志"，但具体是哪些 log？需要明确。

用户原话："正向切（主→被）有日志，正常"。意思可能是看到正向切换正常工作的 INFO log（不是 debug(temp) trace）。比如 `releasing capture: ... entered this device` (emulation.rs:172) 和 `reply: Ack to ... delivered` (listen.rs:312)。

但 add_incoming ENTRY 是 trace level，可能用户没注意到，或者没把它当作"正向日志"。

**所以 H1（add_incoming 没被调用）仍然是可能候选**。但要看用户的真实日志才能确认。

#### 验证手段（统一）

**最便宜的验证**：让用户再跑一次 forward + reverse，把**所有 RUST_LOG=trace 的完整日志**贴上来：
- forward 时应该看到：
  - master: `capture: BeginPending` → `Enter` sent → `releasing capture: ... entered this device` (emulation.rs:172 slave side) → `reply: Ack to ... delivered` (master ack side)
  - master: `client {handle} acknowledged Enter after ...`
  - **slave: `debug(temp) add_incoming ENTRY ...`** ← 关键
  - **slave: `debug(temp) add_incoming POST-INSERT ...`** ← 关键
- reverse 时应该看到：
  - **slave: `debug(temp) libei Begin barrier=...`** ← 最关键
  - **slave: `debug(temp) CaptureBegin handle=...`** ←
  - **slave: `debug(temp) send_leave_event -> addr=...`** ←
  - master: `reply: Leave to ... delivered`
  - master: `release_capture: ...`
  - **master: `debug(temp) producer received Release ...`** ←

**如果 forward 时 add_incoming ENTRY / POST-INSERT 都出现，reverse 时 libei Begin 不出现** → H6 + libei session race 命中。
**如果 forward 时 add_incoming ENTRY 都不出现** → H1 命中（add_incoming 没被调）。

---

## §4 建议下一步

### 4.1 不跑新 trace 的备选排查清单（如果用户不想再加 log）

1. **再次确认正向切日志**：用户跑正向切时，是否看到 `[被控] debug(temp) add_incoming ENTRY ...` 这条 trace？
   - 看到 → add_incoming 走通了，问题在 libei install
   - 看不到 → H1 命中，add_incoming 没被调用（service.rs:439 ReleaseNotify 提前拦截了）

2. **被控 daemon 启动时是否配了 outgoing client**：
   - 是（active=true）→ 默认装 Default barrier + EnterOnly barrier
   - 否 → 只装 EnterOnly barrier
   - 这影响 libei session 启动时机

3. **物理屏幕布局**：master 在 slave 哪边？
   - 假设 master 在 slave 左边 → slave.client.pos = Left, master.client.pos = Right
   - 用户配错 → barrier 装错 edge

### 4.2 加 trace log 的最小方案（如果用户愿意）

**不动业务代码**，只加 3 条临时 trace log（commit 标 "debug(temp):"）：

1. **`input-capture/src/libei.rs:608`** 在 `if !active_clients.is_empty()` 之前 log 当前 `active_clients.len()` 和每个 BarrierKey 的关键字段
2. **`input-capture/src/libei.rs:699`** 在 `active_clients.push/retain` 之后 log "after push: active_clients.len() = ..."
3. **`input-capture/src/libei.rs:797`** 在 `input_capture.enable(session)` 成功之后 log "session enabled with N barriers"

让用户再跑一次，把完整 trace 贴上来。

### 4.3 派什么 sub-agent

**派 plan-step-executor 修 H6**（如果 H6 命中）：
- 改动范围：`src/emulation.rs:173`（删一行）+ `src/service.rs:439`（加条件 if has_active_client）
- 或者更彻底：让 add_incoming 路径直接装 barrier，**不依赖 release_capture**
- 单测 + 真机回归：~30 min

**不要重复跑 validator**（这是新 bug，不是 P2 backlog）。

### 4.4 时间预估

- 加临时 trace log：~10 min
- 用户跑 + 贴日志：~15 min
- 按日志派修：~30-60 min（看哪个候选命中）
- 单测 + 真机回归：~30 min
- **总计：~1.5-2h**

### 4.5 重要 rule 复述

- ✅ 不 commit（除非用户明确说"可以提交"）
- ✅ 不改代码（除临时 debug log）
- ✅ 不重复造 PLAN
- ✅ 不跑 sub-agent（按用户要求）
- ✅ 本报告就是产物

---

## §5 静态读代码 + 历史复盘总结

### 5.1 已确认的事

- **add_incoming 是装 EnterOnly barrier 的唯一路径**（service.rs:593）
- **CaptureTask 在 do_capture_session recv arm 转 Create 请求到 backend**（capture.rs:883-886）
- **libei 的 `select_barriers` 不区分 EnterOnly / Default mode**（只是 `Barrier::new(id, position)`）
- **`capture_type: CaptureType` 在 CaptureTask → InputCapture → backend 这一层完全被忽略**（capture.rs:885 只传 key）
- **libei 的 `notify_release.notify_waiters()` 不销毁 active_clients**（只发信号给 do_capture_session 的 select! arms）
- **`active_clients` 只在 `LibeiNotifyEvent::Create/Destroy` 时改**（libei.rs:692-693）

### 5.2 已排除的事（基于静态读代码）

- **macOS 不需要支持 EnterOnly**（slave 是 libei，master 的反向 barrier 由 master 自己的 add_incoming 路径装）
- **macOS producer 的 Release handler 不清 active_clients**（只清 current_key / pending_key）
- **capture.release() 在 libei 上不直接销毁 barrier**
- **release_capture 的 Pending 特殊分支对 slave 不触发**（slave 启动时 state=Idle）

### 5.3 未完全排除的事

- **H6 子候选**：libei session 在 release_capture → notify_release 触发后重建，**重建期间 barrier 临时失效**，用户快速反向切会错过
- **候选 5（InputCapture.create 短路）**：Default + EnterOnly 共享 key 时，backend.create 只调一次，但 libei 的 BarrierKey 列表里只有一条 entry；capture.rs 按 handle 路由，所以两个 handler 都收到事件——所以这个候选**应该不命中**
- **H1（add_incoming 根本没被调）**：需要用户确认 forward 切时是否看到 `debug(temp) add_incoming ENTRY` trace

### 5.4 调研期间临时改动

- ✅ 没改任何代码
- ✅ 完整读了 5 个文件（service.rs / capture.rs / libei.rs / macos.rs / emulation.rs / listen.rs）
- ✅ 跑了 grep 找 EnterOnly / reverse / position 相关引用
- ✅ 写了报告：`next/STEP-DEBUG-M3-REVERSE-ENTER-R2.md`

调研结束。leader 可：
1. 派 executor 加 §4.2 临时 debug log（commit 标"临时"）
2. 用户跑真机反向切换发日志
3. 按 §4.3 派 executor 修对应候选
