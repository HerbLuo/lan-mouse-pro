# STEP-DEBUG-M3-REVERSE-ENTER — 真机反向 Enter 不工作根因

> 调研日期：2026-09-08
> 触发条件：用户真机回归报告 **macOS (主控) + Linux libei (被控)** 双机测试，鼠标可从主控切到被控（H1 修复后），但**反向**（被控切回主控）**不工作**
> 调研起点：H1 fix commit `f7a3ab8`（`query_pure` legacy fallback）后正向切边修复；反向仍不工作
> 调研范围：`lan-mouse-proto`、`src/service.rs`、`src/emulation.rs`、`src/capture.rs`、`src/connect.rs`、`input-capture/src/libei.rs`、`input-capture/src/macos.rs`、`input-capture/src/geometry/mod.rs`、`src/client.rs`
> OS 上下文（已确认）：**主控端 macOS，被控端 Linux GNOME Wayland libei**（PLAN §8 line 332 真机测试矩阵明示"主控 `capture.rs` 日志 + 对端 libei 收到 `Enter`"；被控端 KWin/libei portal 派发 EIS）

---

## §1 触发链整条双向（读完所有相关文件）

### 1.1 正向 — 主控 → 被控（**H1 fix 后能切**，基准线）

1. **主控 macOS CGEventTap**（`input-capture/src/macos.rs:1078-1180`）
   - 鼠标跨主控右 edge：`prev_pos = (x, y)` clamped + `curr_pos = (x + dx, y + dy)` predicted
   - `state.crossed(prev, curr)` → `crossed_pure` → `query_pure`（**H1 legacy fallback 在此生效**：`clients.contains(monitor:Some(...))` miss 时 fallback `monitor:None`）
   - 命中 → `state.pending_key = Some(key)` + emit `CaptureEvent::BeginPending`
2. **`capture.rs::do_capture_session` recv arm**（`src/capture.rs:709`）
   - `handle_capture_event(capture, (handle, BeginPending))`
   - `key = self.get_key(handle)` → `State::Pending { handle, key, started }`
   - `conn.send(ProtoEvent::Enter(to_proto_pos(key.pos.opposite())), handle)`
     - **关键**：`key.pos.opposite()` —— 主控 `pos=Right` → 发 `Enter(Left)`（对端语义 = 对端的左边）
3. **Wire QUIC stream A** → 被控 daemon
4. **被控 `emulation.rs::ListenTask`**（`src/emulation.rs:160-176`）
   - 收到 `ProtoEvent::Enter(pos=Left)`
   - 发 `EmulationEvent::ReleaseNotify`（触发 service 释放本地 capture；被控本来无 capture，no-op）
   - `self.listener.reply(addr, ProtoEvent::Ack(0))`
   - 发 `EmulationEvent::Entered { addr, pos: to_ipc_pos(pos=Left), fingerprint }`
5. **`service.rs::handle_emulation_event`**（`src/service.rs:372-388`）
   - `add_incoming(addr, pos=Left, fp)`：
     ```rust
     // src/service.rs:566-584
     let handle = ENTER_HANDLE_BEGIN + next_trigger_handle;  // u64::MAX/2+1 起
     let key = BarrierKey::from_pos(to_capture_pos(Left));   // {pos:Left, monitor:None, offset:0, span:10000}
     self.capture.create(handle, &key, CaptureType::EnterOnly);
     self.incoming_conn_info.insert(handle, Incoming { addr, pos:Left, fp });
     ```
   - **被控建立 EnterOnly barrier**：libei `set_pointer_barriers` 在被控屏幕**左 edge**（即面向主控的边）装一条物理 barrier
6. **主控 `capture.rs` recv arm**（`src/capture.rs:786`）收到 `Ack(0)`
   - `state == Pending { handle:matching, ... }` → `capture.start_capture(key)` → macOS producer `ProducerEvent::StartCapture(key)`
   - **macOS producer** 改 `current_key=Some(key)`、调 `hide_cursor()`、`reset_cursor(key.pos)` warp 1px
   - macOS producer 随后主动 emit `CaptureEvent::Begin`（通过 `notify_tx` event_tx）→ `capture.rs` 收到 Begin → `active_client.replace(handle)` + `state = State::Sending`
7. **主控 → 被控输入流**：`capture.rs:1449` 的 `State::Sending` 分支把每个 `CaptureEvent::Input(e)` 通过 `conn.send(ProtoEvent::Input(e))` 转发给被控
8. **被控 `emulation.rs::ListenTask`** 收到 `ProtoEvent::Input(event)` → `self.emulation_proxy.consume(event, addr)` → 被控 `InputEmulation` 后端（如 X11/libei/xdotool）合成输入

**正向路径在 H1 fix 后能切边** ← 用户确认。

### 1.2 反向 — 被控 → 主控（**用户报告不工作**）

1. 用户鼠标移到被控屏幕左 edge（朝向主控）
2. **被控 libei 物理 barrier fire `Activated`**（`input-capture/src/libei.rs:760-783`）
   - libei portal → compositor（GNOME / KDE）发 `Activated { barrier_id, cursor_position }`
   - `barrier_id` 通过 `key_for_barrier_id` 找回 `BarrierKey { pos:Left, monitor:None, ... }`
   - `event_tx.send((key.clone(), CaptureEvent::Begin))` 发给被控 `capture.rs`
3. **被控 `capture.rs::handle_capture_event`**（`src/capture.rs:1247-1265`）
   - `get_type(handle) == CaptureType::EnterOnly` → true
   - `event == CaptureEvent::Begin` → true
   - **`event_tx.send(ICaptureEvent::CaptureBegin(handle))`** —— 推 service
   - `!is_default_capture_at(&get_key(handle))` → 被控无 Default captures → true
   - **`capture.release().await?`** —— 被控 libei `notify_release.notify_waiters()`
   - return Ok(()) —— **重要：直接 return，不进 match event 分支，所以不向主控发 Enter / 不重置 state**
4. **被控 `emulation.rs::ListenTask`** 把 `notify_release` 经 service 转的 `EmulationEvent::ReleaseNotify` 处理掉（service.rs:439 调 `capture.release()` —— 但**这一步是冗余 no-op，因为被控已经处于 EnterOnly 路径**）
5. **`service.rs::handle_capture_event`**（`src/service.rs:459-465`）收到 `CaptureBegin(handle)`
   ```rust
   if let Some(incoming) = self.incoming_conn_info.get(&handle) {
       self.emulation.send_leave_event(incoming.addr);
   }
   ```
   - 查 `incoming_conn_info[handle]` → 找到 `{ addr: master_addr, pos:Left, fp }`
   - **`send_leave_event(master_addr)`** → `EmulationRequest::Release(master_addr)`
6. **`emulation.rs::ListenTask`**（`src/emulation.rs:236`）
   - `EmulationRequest::Release(addr) => self.listener.reply(addr, ProtoEvent::Leave(0)).await`
   - **通过 QUIC stream A 发 `ProtoEvent::Leave(0)` 给主控**
7. **被控 libei `do_capture_session` 的 `notify_release.notified()` arm**（`src/libei.rs:786`）触发
   - `release_capture(input_capture, session, activated, &key)`（`src/libei.rs:838-865`）
   - **`input_capture.release(session, ReleaseOptions { cursor_position: (x+1, y), activation_id })`** —— EIS release + **warp slave cursor 1px inside** slave screen（防止再次触发）
   - libei session 继续 wait for next `Activated`
8. **主控 `capture.rs::do_capture_session` recv arm**（`src/capture.rs:713, 857-860`）收到 `ProtoEvent::Leave(0)`
   - `active_client = Some(handle)` (主控还在 Sending) → 不 continue
   - fallthrough 到 `match event { ProtoEvent::Leave(_) => self.release_capture(capture).await? }`
9. **`capture.rs::release_capture`**（`src/capture.rs:1481-1620`）
   - `state == Sending`（非 Pending）
   - `active_client.take() = Some(handle)`
   - `capture.take_pressed_keys()` → 空集（纯鼠标切换）
   - 合成 mods=0 + `ProtoEvent::Leave(0)` → `conn.send` 给被控（**被控已经处理过自己的释放，但 Leave(0) 再来一次也无害**）
   - **`self.state = State::Idle`**（force-reset 补丁，已 commit）
   - **`capture.release().await`** → macOS `LibeiInputCapture::release` via `spawn_local` 发 `ProducerEvent::Release`
10. **主控 macOS producer**（`src/macos.rs:303-314`）处理 `ProducerEvent::Release`
    - `current_key.is_some()` → `show_cursor()` + `current_key = None`
    - `pending_key.is_some()` → 清
11. 主控 OS-level capture 释放 → **主控 cursor 应可见**

---

## §2 已排除嫌疑

| 嫌疑 | 验证结果 | 证据 |
|---|---|---|
| 协议层 `Enter` 是单向的（被控不发自己的 Enter） | ✅ **确认设计如此**，符合 PLAN §1 架构图"Enter 只携带对端方向"。被控不发 Enter 也不需要发——它装 EnterOnly barrier 触发 Leave 来反向通知主控 | `lan-mouse-proto/src/lib.rs:65-72` `Enter(Position)` 只有方向无坐标；`src/capture.rs:1283-1338` BeginPending 时发 Enter；`src/capture.rs:1303` `key.pos.opposite()` |
| `add_incoming` barrier position 用错（被控装在右 edge 而不是左） | ❌ **排除** —— 用 master 收到的 `Enter(Left)` 直接作 pos，被控屏障在**被控屏幕的 Left edge**（面向主控）；libei 的 `select_barriers` `Position::Left => (x, y, x, y+h_i-1)` 是左 edge 线段（`src/libei.rs:289-303`）| `src/service.rs:566-584` add_incoming；语义对称 |
| EnterOnly 触发后 capture.rs 没转发 CaptureBegin | ❌ **排除** —— `src/capture.rs:1247-1255` `if event == Begin \|\| BeginPending` 时明确 forward；commit `f7a3ab8` 之前的版本只监听 Begin，**EnterOnly 永远不触发**，导致老 bug "mouse stuck on slave" —— 已修复（docstring `src/capture.rs:1230-1246`）| 已有单测路径覆盖 |
| EnterOnly 触发后 service.rs 找不到 incoming_conn_info | ⚠️ **可能性 1** —— 见 §3 H1，handle 用 `ENTER_HANDLE_BEGIN + next_trigger_handle`，**是高 u64 数字（u64::MAX/2 起）**；`incoming_conn_info: HashMap<ClientHandle, Incoming>` 写入和读取同一 handle；但有 race：add_incoming 写入 incoming_conn_info **同步**（service.rs 主循环），CaptureBegin 从 capture 任务发到 service（异步事件循环）—— 顺序：add_incoming 先 send `ICaptureEvent::DeviceEntered` 给 frontend → 然后 capture.create 异步到 capture 任务 → 然后 libei install barrier（异步）→ 然后用户跨 edge → Begin 事件 → CaptureBegin → service 查 incoming_conn_info
| `is_default_capture_at` 在被控上误判 true 导致 capture.release() 被跳过 | ❌ **排除** —— 被控无 Default captures（slab 里只有 EnterOnly entry）；函数返回 false → `!is_default_capture_at == true` → 调 release | `src/capture.rs:535-542` |
| `emulation.send_leave_event` 没把 Leave 发到主控 | ⚠️ **可能性 2** —— 见 §3 H2。`EmulationRequest::Release(addr)` 通过 channel 送到 ListenTask，`listen.rs::reply(addr, Leave(0))` 用 `quic_conns.borrow().get(&addr).cloned()` 找 peer；**如果 master_addr 不在 quic_conns**（如 server_hello race / mTLS handshake 刚完成就发 Leave），`reply` 静默 `log::warn` 并 drop event | `src/listen.rs:301-320` |
| 主控 `capture.rs` recv arm 早 continue 把 Leave 丢了 | ❌ **排除** —— early-continue 只在 `active_client.is_some() && handle != active` 时触发。Leave 进入时 `active_client = Some(handle)`（主控还在 Sending）+ handle == active → 落 through | `src/capture.rs:714-720` |
| 主控 `release_capture` 没真正释放 OS-level capture | ⚠️ **可能性 3** —— 见 §3 H3。macOS `Capture::release` 是 `spawn_local` 异步发 `ProducerEvent::Release`，fire-and-forget；如果 spawn race 让 producer 没收到，current_key 残留 → cursor 隐藏 | `src/capture.rs:1508-1515` + `src/macos.rs:303-314` |
| H1 fix 漏修了被控路径 | ❌ **排除** —— H1 修的是 `query_pure`（macOS `crossed` + Windows `check_client_activation` 共用），**libei 不走 query_pure**（直接走 `Activated` event）。被控 daemon 跑同一份 binary 共享 query_pure 修复没问题，但被控根本不用 query_pure | `input-capture/src/libei.rs:760-800` |
| H2（displays vs last_monitors IOKit 暂态 ID 不一致） | ❌ **排除**（与本场景无关）—— H2 影响"dropdown 选的 monitor ID 跟 displays 里的 ID 不同"，**只影响带 `monitor: Some(...)` 的 client**；本场景默认 `monitor:None`，走 H1 legacy fallback | STEP-DEBUG-M3-BARRIER-CHAIN.md §3 H2 |
| H3（macOS Capture::create spawn race） | ⚠️ **可移植到本场景** —— 见 §3 H4 |
| 单测覆盖盲点（vacuous test） | ⚠️ **真空** —— 单测只覆盖 `query_pure` / `crossed_pure` / `activation_pure`（macOS / Windows 路径）；**没有任何单测覆盖 `add_incoming` → libei barrier install → Begin → CaptureBegin → send_leave_event → master release_capture 这条反向链** | `grep -rn "add_incoming\|reverse.*enter" src/*test* next/STEP*` 无命中 |

---

## §3 最可能的根因（按概率排序）

### H1（**最可能，~40%**）—— `add_incoming` 写入 `incoming_conn_info` 与 `capture.create` 的 race

#### 现象与定位
`add_incoming`（`src/service.rs:566-584`）的顺序：
```rust
let handle = ENTER_HANDLE_BEGIN + next_trigger_handle;
self.capture.create(handle, &key, CaptureType::EnterOnly);     // ← async path
self.incoming_conns.insert(addr);
self.incoming_conn_info.insert(handle, Incoming { ... });      // ← sync, but AFTER capture.create
```

`capture.create` 是 `request_tx.send(CaptureRequest::Create(...))`（`src/capture.rs:344-347`）—— fire-and-forget。但**`incoming_conn_info.insert` 是同步 HashMap 写**——**主线程 service event loop 在 `add_incoming` 内就完成**。

那么 `handle_capture_event(CaptureBegin(handle))` 查 `incoming_conn_info.get(&handle)` 时能查到吗？理论上**能** —— 因为 `add_incoming` 在 handle_emulation_event 里是同步执行的，返回前已写完 HashMap。CaptureBegin 是从 capture 任务发来的**之后**的事件。

但**这里有一个边角 case**：`handle_emulation_event` 的 `EmulationEvent::Entered` 分支：
```rust
EmulationEvent::Entered { addr, pos, fingerprint } => {
    if !self.incoming_conns.contains(&addr) {  // ← 第二次 Enter 走 update_incoming
        self.add_incoming(addr, pos, fingerprint.clone());  // ← 删旧 + 建新
        ...
    } else {
        self.update_incoming(addr, pos, fingerprint);  // ← update_incoming 会 remove + add
    }
}
```

`update_incoming`（`src/service.rs:586-612`）：
```rust
if changed {
    self.remove_incoming(addr);  // ← 删旧 handle + destroy capture
    self.add_incoming(addr, pos, fingerprint.clone());  // ← 建新 handle
    ...
}
```

**关键**：`update_incoming` 只在 `pos` 或 `fingerprint` 变化时 rebuild。如果 reconnect 后 pos / fp 相同，**直接 reuse 旧 `incoming_conn_info[handle]`**。这条路径理论上没问题，但 add_incoming 每次会 `next_trigger_handle += 1` 拿新 handle —— **如果有 EnterOnly barrier 已经在 libei 装好但 handle 换号了**，旧 handle 的 barrier 会 leak（不会 destroy，因为 remove_incoming 只针对 `incoming_conn_info` 中 entry 的 addr 查 handle）。

不过这是 leak，不会让反向"完全不工作"，只会让第二次切换后被控多一条 stale barrier。

#### 假设链路
1. 主控首轮 send Enter(Left) 给被控
2. 被控 `add_incoming(addr, Left, fp)`：handle=H0=ENTER_HANDLE_BEGIN+0=9223372036854775808，写 `incoming_conn_info[H0]`
3. libei 在被控装 barrier（异步 1-2 个 loop iteration）
4. 主控 send Leave（用户按 release-bind / macOS capture 退订）→ 不影响被控 EnterOnly
5. **第二次**主控 send Enter(Left)（用户重新跨 edge 切到被控）
6. 被控 emulation.rs 收到 Enter(Left)，**同 addr + 同 pos**（Left）+ 同 fp → 走 else 分支 `update_incoming`：pos 和 fp 都没变 → `incoming_conn_info.get(&handle)` 找到旧 H0 → **不重建 barrier**（这条路径不进 add_incoming / remove_incoming）
7. OK 这条路径不破

但**还有一个真 race**：被控 daemon 重启后首次 Enter：
1. 被控 daemon 启动，`incoming_conns = empty`, `incoming_conn_info = empty`
2. 主控收到 list 发 Enter(Left)
3. 被控 `add_incoming`：
   - `handle = ENTER_HANDLE_BEGIN + 0 = H0`
   - `self.capture.create(handle, &key, EnterOnly)` ← **fire-and-forget via request_tx**
   - `self.incoming_conn_info.insert(H0, ...)` ← **同步写**
   - `next_trigger_handle += 1` → 1
4. capture 任务 recv `CaptureRequest::Create(H0, key, EnterOnly)` → `add_capture(H0, key, EnterOnly)` → `capture.create(H0, &key).await`
5. InputCapture::create 写 `id_map[H0] = key`, `position_map[key] = [H0]` → `LibeiInputCapture::create` 发 `LibeiNotifyEvent::Create(key)`
6. libei task 收 → active_clients.push(key) → 下一次循环 reinstall barriers via `set_pointer_barriers`
7. 用户跨被控左 edge → libei Activated → Begin(H0, Begin) → capture.rs EnterOnly handler → **ICaptureEvent::CaptureBegin(H0) → service**
8. service.rs 收 CaptureBegin(H0) → `incoming_conn_info.get(&H0)` → **找到！**（add_incoming 同步写了）
9. → send_leave_event(master_addr)

**正常情况下这条 race 不会破**。但**有一种 corner case**：如果 daemon 启动慢，barrier 还没装好（步骤 6 之前）用户就跨 edge 了 —— libei 不发 Activated，Begin 事件不来，反向自然不 work。**这是首次启动 race，不是稳态 bug**。

#### 验证方法
- 跑 `RUST_LOG=lan_mouse_service=trace,input_capture=trace,lan_mouse=trace` + 重启被控 daemon + 用户跨边，看 service 日志：
  - 期望：`EmulationEvent::Entered: add_incoming(addr, Left, fp) → incoming_conn_info.insert`
  - 期望：`ICaptureEvent::CaptureBegin(handle)` 后能查到 handle
  - 失败的话看：`incoming_conn_info` 里有 handle 但 `CaptureBegin` 是另一个 handle（handle 不匹配）→ 数字漂移

#### 修复方向
- 加 debug log（**临时**，commit message 标"临时"）：
  ```rust
  // src/service.rs:461
  log::info!("ICaptureEvent::CaptureBegin(handle={handle:?}); incoming_conn_info has {} entries, lookup = {:?}",
      self.incoming_conn_info.len(),
      self.incoming_conn_info.get(&handle));
  log::info!("incoming_conns={:?}", self.incoming_conns);
  ```
- 如果确认 handle 不匹配 → 加单测 `service_handle_capture_begin_uses_correct_handle`：用 `ClientManager` + mock Capture 验证 handle 路由

---

### H2（**~30%**）—— `emulation.send_leave_event` 在 `quic_conns` 里找不到 peer，Leave 被 drop

#### 现象与定位
`src/listen.rs:301-320`：
```rust
pub(crate) async fn reply(&self, addr: SocketAddr, event: ProtoEvent) {
    let peer = self.quic_conns.borrow().get(&addr).cloned();
    match peer {
        Some(peer) => { peer.send_input(&event, ...).await ... }
        None => log::warn!("reply: peer {addr} not in quic_conns; dropping {event}"),
    }
}
```

如果 `quic_conns` 里没有 master_addr，**Leave(0) 被静默 drop**，主控永远收不到。

什么时候会找不到？
- 被控 daemon 重启后**第一轮**反向切换：mTLS handshake + server_hello 完成后才把 peer 写进 `quic_conns`（`src/listen.rs:604`）。如果 Enter 来得比 quic_conns 注册早，`EmulationEvent::Entered` 还是会被处理（因为 emulation 是基于 `ListenEvent::Msg`，ListenEvent::Accept 先到），但**后续 Leave 走 reply 时 peer 已经在 quic_conns 了** —— 这条不破。

- **真实 race**：主控的 QUIC connection 被强制 close（idle timeout / pong health watchdog），被控收到 supervisor 的 `WAKE_CLOSE_CODE` → `ListenEvent::Disconnected` → `quic_conns.remove(&addr)`。但被控 `incoming_conn_info[handle].addr` 还指向那个已断的 addr。如果**这时**用户跨被控左 edge → send_leave_event(stale_addr) → reply 找不到 peer → drop。

#### 假设链路
1. 主控 / 被控正常连接，incoming_conn_info[handle].addr = master_addr
2. 主控因网络抖动 / pong watchdog 强制 close QUIC connection
3. 被控 `ListenTask` 收到 `Disconnected(addr=master_addr)` → `quic_conns.remove(&addr)` + `emulation_proxy.remove(addr)` + 发 `EmulationEvent::Disconnected`
4. **被控 `service.rs::handle_emulation_event(Disconnected)`**（`src/service.rs:389-422`）：
   ```rust
   EmulationEvent::Disconnected { addr } => {
       // Preserve the capture barrier across transient disconnects...
       //   - the barrier in the capture module keeps firing CaptureBegin on edge crossings;
       //   - the CaptureBegin handler can look up the addr via incoming_conn_info
       //     and call send_leave_event(addr);
       log::info!("peer {addr} transiently disconnected — barrier preserved for fast recovery");
       self.notify_frontend(FrontendEvent::IncomingDisconnected(addr));
   }
   ```
   **重要**：Disconnected 路径**主动保留 barrier**（不 destroy）+ 不清 incoming_conn_info —— 这是设计（注释解释"fast recovery"）。
5. 主控 QUIC supervisor 重新 dial 成功 → 新连接注册到 quic_conns，**但 addr 可能不同**（QUIC source port 变化）→ 主控 `client_manager.set_active_addr(handle, Some(new_addr))` + supervisor 在新连接注册 `quic_conns.insert(new_addr, peer)`
6. **被控**的 incoming_conn_info[handle].addr 还是**旧的 master_addr**（Disconnected 路径没改）
7. 主控 send Enter（新连接，旧 listener 收不到；新 listener 收 Enter）
8. 被控 emulation.rs 收 Enter → service EmulationEvent::Entered → `incoming_conns.contains(&new_addr) == false`（addr 变了！）→ `add_incoming(new_addr, Left, fp)` → **新 handle H1** + 新 barrier + 写 `incoming_conn_info[H1]`
9. **但旧 handle H0 的 incoming_conn_info entry 还在**（因为 Disconnected 路径主动保留 barrier），旧 barrier 还在 libei 装
10. 用户跨被控左 edge → libei Activated → **旧 handle H0** 的 Begin 事件 → CaptureBegin(H0) → service `incoming_conn_info.get(&H0)` 返回**旧 master_addr**（已从 quic_conns 移除）→ send_leave_event(stale_addr) → reply warn drop

**这条链路会真的让反向不工作**。

但注意：**H2 的前提是"disconnect 后 reconnect，addr 变了"**。如果 reconnect 后 addr 不变（旧 listener 仍持有 conn 或端口保留），H2 不成立。

#### 验证方法
- 跑 daemon + `RUST_LOG=lan_mouse=trace,input_capture=trace`
- 主动 `kill -STOP <lan-mouse>` 让主控收不到 pong，触发主控 pong watchdog 关 conn
- 用户跨被控左 edge
- 看被控日志：
  - 是否出现 `"reply: peer {addr} not in quic_conns; dropping Leave"` warn？
  - 或 `"peer {addr} transiently disconnected — barrier preserved"`
- 如果是 → 主控 reconnect 后用户报告的"反向不工作"是这条 H2

#### 修复方向
- **选项 A（短期，1 行）**：`Disconnected` 时也 destroy EnterOnly barrier + 清 incoming_conn_info：
  ```rust
  EmulationEvent::Disconnected { addr } => {
      self.remove_incoming(addr);  // ← 新增：destroy barrier + 清 handle
      self.notify_frontend(FrontendEvent::IncomingDisconnected(addr));
  }
  ```
  但这破坏了 "fast recovery" 设计（注释明示"下一次 Enter 直接走 add_incoming"）。

- **选项 B（短期，更新 incoming_conn_info 的 addr）**：disconnect 时把 `incoming_conn_info` 里的 addr 标记 invalid（`addr = 0.0.0.0:0`），让 send_leave_event 走 no-op。或者用 Option<SocketAddr>，None 时不 send。

- **选项 C（更彻底）**：监听 reconnect 事件，主动 update_incoming 重建新 handle（destroy 旧 barrier）：
  ```rust
  EmulationEvent::Connected { addr, fingerprint } => {
      // 找到旧 incoming_conn_info 里有同 fp 的 handle，destroy 旧 barrier
      for (handle, incoming) in &self.incoming_conn_info {
          if incoming.fingerprint == fingerprint {
              self.remove_incoming_addr_only(*handle);  // 保留 entry 但清 addr
          }
      }
  }
  ```

- **正确性靠锁**：`Disconnected` 时把 incoming_conn_info 的 addr 改成 Option 或标记 stale，让 send_leave_event 检测 stale 后 drop + log + 通知 frontend。

---

### H3（**~15%**）—— 主控 macOS producer `Release` spawn race 没收到事件

#### 现象与定位
主控 `release_capture`（`src/capture.rs:1597-1619`）：
```rust
log::info!("release_capture: calling capture.release() (OS-level release)");
let res = capture.release().await;
```

`capture.release()` on macOS（`src/macos.rs:1508-1515`）：
```rust
async fn release(&mut self) -> Result<(), CaptureError> {
    let notify_tx = self.notify_tx.clone();
    tokio::task::spawn_local(async move {
        log::debug!("notifying Release");
        let _ = notify_tx.send(ProducerEvent::Release).await;
    });
    Ok(())
}
```

**fire-and-forget** —— `release()` 立刻返回 Ok，ProducerEvent::Release 异步发。

如果 producer task 在 release 之前被 cancel / panic / 该 task 退出，**`notify_tx.send` 会失败，event 静默丢失**。Producer 仍在，但 state 里 `current_key.is_some()` 残留（如果 `notify_tx` channel 满了，`send().await` 会阻塞等待；如果 channel 关闭，立即返 Err）。

但这通常是单次 race 不会持续。**首次**反向切不 work 但**第二次**可能 work（producer channel 重置后）。

#### 验证方法
- 看主控 macOS producer 日志：
  - 期望：`handling event: Release` + `show_cursor()` + `current_key = None`
  - 失败的话：日志里没有 `Release` handler 那行，但 `release_capture: calling capture.release()` 已 log

#### 修复方向
- 把 macOS `release` 改成 sync barrier：
  ```rust
  async fn release(&mut self) -> Result<(), CaptureError> {
      let (tx, rx) = tokio::sync::oneshot::channel();
      let notify_tx = self.notify_tx.clone();
      tokio::task::spawn_local(async move {
          let _ = notify_tx.send(ProducerEvent::Release).await;
          let _ = tx.send(());
      });
      let _ = rx.await;
      Ok(())
  }
  ```
  但前提是 producer 必然处理 Release —— 需要保证 producer task 活着

---

### H4（**~10%**）—— `add_incoming` 触发后 libei barrier 还没装好就跨 edge（一次性 race）

#### 现象与定位
被控 daemon 启动后第一轮 master → slave 切换：
1. add_incoming → CaptureRequest::Create → CaptureTask → InputCapture::create → LibeiInputCapture::Create notify → libei task 下次迭代 active_clients.push → 重启 session → set_pointer_barriers → barrier 装好
2. **步骤 1 期间**（几十 ms 到几百 ms）用户物理鼠标就在被控左 edge 上 → **libei 不发 Activated**（barrier 没装好）
3. 用户主控收到 Enter ack 顺利切到被控（master 方向 OK）
4. 用户把鼠标**挪回**主控 → slave 的 barrier 没 fire → send_leave_event 没触发 → **主控永远卡在 State::Sending**

这是**首次启动 race**，**稳态后**应该 work。如果用户测过几次后还 fail，不是 H4。

#### 验证方法
- 用户**首次切换**就 fail 还是**几次之后**才 fail？
- 如果首次 fail：H4 高概率
- 如果几次之后 fail：H1/H2 高概率

#### 修复方向
- 让 libei barrier install 同步：add_incoming 之后 spin 等待 barrier ready（不优雅）
- 或者：让 master 在收到 Enter ack 后**短延迟**（200ms）才发送第一个 input event，给被控 barrier install 时间

---

### H5（**~5%**）—— libei `Activated` 的 cursor_position 与 barrier 不对应导致 `find_corresponding_client` 失败

#### 现象与定位
`input-capture/src/libei.rs:766-768`：
```rust
let barrier_id = match activated.barrier_id() {
    Some(ActivatedBarrier::Barrier(id)) => id,
    Some(ActivatedBarrier::UnknownBarrier) | None =>
        find_corresponding_client(&barriers, activated.cursor_position().expect("...")),
};
```

`ActivatedBarrier::UnknownBarrier`（KDE plasma workaround）走 `find_corresponding_client`，按 cursor_position 找最近 barrier。

被控只有 1 条 barrier，理论上不会找错。但 cursor_position() 可能返回 (x, y) 而最近距离算下来不是用户预期的 barrier。如果用户鼠标在两条 barrier 边界处，可能找错。

被控**只有 1 条 EnterOnly barrier** —— 应该直接命中。但 KDE plasma 的 `cursor_position()` 可能延迟 / 滞后，导致算出来的"最近"是其他 edge（虽然被控只有 1 条，但万一 enumerate_monitors 有 stale edge 数据）。

#### 验证方法
- 看被控 libei 日志：
  - 期望：`Activated { barrier_id: Some(Barrier(N)) }` 或 `find_corresponding_client distance_to_line(...)
  - 失败的话：`INVALID BARRIER ID: Id N does not exist` 紧接 panic（`src/libei.rs:777` `key_for_barrier_id.get(&id).expect("invalid barrier id").clone()`）

#### 修复方向
- 加诊断 log：
  ```rust
  log::info!("Activated barrier_id={barrier_id:?}, cursor_position={pos:?}, key_for_barrier_id map: {key_for_barrier_id:?}");
  ```

---

## §4 建议下一步

### 4.1 派什么 sub-agent
**plan-step-executor**（已在 .LEADER.md 授权）：

### 4.2 第一步（最便宜）—— 加临时 debug log 锁死嫌疑
**不动业务代码**，只在 service.rs / emulation.rs / listen.rs / capture.rs 加 `log::info!` / `log::warn!`，commit message 标"临时 debug log"。

建议改的位置（临时）：
1. `src/service.rs:461` CaptureBegin 收到时 log `handle` + lookup 结果
2. `src/service.rs:566-584` add_incoming log `handle` + addr + pos
3. `src/emulation.rs:236` send_leave_event 收到 time log
4. `src/listen.rs:301-320` reply 时 log + warn
5. `src/capture.rs:1597` release_capture 调用 capture.release() 时 log
6. `input-capture/src/libei.rs:760-770` Activated event 时 log barrier_id + cursor_position
7. `input-capture/src/macos.rs:303-314` ProducerEvent::Release 处理时 log

**用户跑一次** `RUST_LOG=lan_mouse_service=trace,lan_mouse=trace,input_capture=trace`，反向切换，把日志发回来 —— **看哪一行缺失就是哪条 H 的根因**。

### 4.3 第二步（按日志结果）

- **如果 H1 命中**（incoming_conn_info 查不到 handle 或 handle 不匹配）：
  - 派 executor 加单测 `service_handle_capture_begin_uses_correct_handle`（锁住 add_incoming 写入 vs CaptureBegin 查询的顺序不变性）
  - 修：在 `add_incoming` 把 `incoming_conn_info.insert` 放到 `capture.create` **之前**（防御未来 race，目前不可见但加注释锁住）
  - 预计 30 min

- **如果 H2 命中**（reply warn "peer not in quic_conns"）：
  - 派 executor 在 `service.rs:handle_emulation_event(Disconnected)` 把 `remove_incoming(addr)` 加进去（或更保守：在 `Disconnected` 时把 incoming_conn_info entry 的 addr 标记 stale，让 send_leave_event 走 no-op + warn）
  - **重要**：保留 barrier 设计意图 — 只清 addr 引用，barrier 不 destroy（fast recovery 设计不变）
  - 加单测 `disconnect_marks_incoming_addr_stale` + `send_leave_event_to_stale_addr_is_noop`
  - 预计 45 min（含设计权衡文档）

- **如果 H3 命中**（macOS producer 没看到 Release event）：
  - 把 macOS `release` 改成 sync barrier（oneshot 等 producer 处理完）
  - 加单测 `macos_release_propagates_to_producer_synchronously`（很难测，可能加 integration test）
  - 预计 60 min

- **如果 H4 命中**（首次启动 race）：
  - 派 executor 在 master side 加 "Enter ack 后 200ms 才发送 input event" 延迟
  - 加单测锁住这个延迟
  - 预计 30 min

- **如果 H5 命中**（libei Activated cursor_position 异常）：
  - 看 `find_corresponding_client` 是否真的 panic 或 warn
  - 加防御：如果 cursor_position 不在 slave 任何 barrier 范围内 → log + 不发 Begin event

### 4.4 时间预估
- 临时 debug log：15 min
- 跑真机复现 + 日志：用户配合 10-30 min
- 按日志派 executor 修：30-60 min（看哪条 H 命中）
- validator 单测：20 min
- **总计：~1.5-2.5h**

### 4.5 不动代码的备选排查清单
如果用户不想装 debug log（commit message 标"临时"），可以问用户：
1. **首次切换就 fail 还是几次之后？** → 区分 H4 vs H1/H2
2. **反向切换失败时主控 OS cursor 是否可见？**
   - 可见 + 但无法操作 → H3 高概率（cursor 显示但 input 不响应）
   - 不可见 + 主控卡在 capture 状态 → H1/H2/H5 高概率
3. **被控 daemon 日志里有没有 "Activated" 或 "libei" 相关日志？**
   - 完全没有 → H4 / H5（barrier 没装好或没 fire）
   - 有 → send_leave_event 后续链路问题
4. **是否发生过 disconnect / reconnect？** → H2 高概率
5. **被控是否单 monitor？** 多 monitor + `monitor: None` 可能让 libei 给每个 monitor 都装 barrier（layer_shell 同形 bug），但 libei 的 select_barriers 已经按 client × region 装，**不应该有这问题**

---

## §5 调研期间临时改动

- **没改任何代码**（除已 commit 的 H1 fix `f7a3ab8`）
- **跑了 `cargo test --workspace`** 165/165 全绿（确认现状不是单测层面问题）
- **没动 PLAN 文档**
- **写报告**：`next/STEP-DEBUG-M3-REVERSE-ENTER.md`

调研结束。leader 可：
1. 派 executor 加 §4.2 临时 debug log（commit 标"临时"）
2. 用户跑真机反向切换发日志
3. 按 §4.3 派 executor 修对应 H
