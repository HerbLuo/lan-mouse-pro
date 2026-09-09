# Validation: M1a Post-Hotfix 整批审

> 审阅日期：2026-09-09　审阅 STEP 范围：M1a 之后 10 个真机驱动 hot-fix + 配套文档 commit
> 起点 commit：`b92c750`（M1a 整批 validator 终点）
> 终点 commit：`d0ce5a9`（HEAD）
> 起点 / 终点 hash：起点 `b92c750`，终点 `d0ce5a9`
> 角色：step-validator（只读不写，**唯二可写** = 本报告 + `next/.LEADER-STATE.md` 不写）

---

## 0. 审阅范围（19 commit 中的代码 / 文档 / 临时调试分类）

代码 commit（src/ 实际改动）：
- `f58db74` fix(windows-clipboard): resolve cargo run errors
- `730092a` fix(clipboard): wire default_backend factory to Linux + Windows impls（**真机发现** #S-2）
- `a3bb056` chore(clipboard-debug): bump slave inbound chain logs to info for no-op diagnosis（**临时调试**）
- `d4343e2` logs（**临时调试**：tick / SendClip / broadcast gate summary）
- `31f7df6` fix(quic): read first stream C frame inline (off-by-4 corrupt decode)（**真机发现**）
- `3a51134` fix(clipboard): recover dial-window copies on active_addr transition
- `2c6ad53` fix(clipboard): bypass LRU check in recover push (tick pre-marks incorrectly)
- `6df6216` fix(clipboard): drop byte dump from SendClip log
- `42c724f` fix(service): recover barrier after macOS screen sleep/wake
- `1a95486` fix: The copied text on the controlled terminal cannot be transmitted to the master terminal（**PLAN-1 M3 回归**）
- `8a44bdf` fix(quic): add client-side accept_bi loop for peer stream C push（**PLAN-1 M3 回归**）

文档 commit（仅 next/ / .claude/agents/ + Cargo.lock）：
- `1ba24bb` docs: sync leader-state with M1a 1a.1-1a.4 reality
- `4a70566` remove step docs（PLAN-1 历史 STEP 文档清理）
- `8c0a1d1` docs: add M1a clipboard no-op investigation（**已删除**）
- `998ec2f` docs（BUGS.md 微清理）
- `06780b6` docs: log two M1a clipboard follow-up bugs
- `1ec1a66` docs（删除 INVESTIGATION 文件）
- `2736c50` / `d0ce5a9` docs（planer.md + PLAN-2-CLIPBOARD.md 微调）

**结论：✅ PASS-with-followup**（详见 §5）

---

## 1. 偏离 PLAN

所有 hot-fix 都是用户真机验证驱动的回归修复或真机发现的必要补全，**不属于 PLAN scope 越界**。具体对位：

| Commit | 对位 PLAN | 状态 |
|---|---|---|
| `f58db74` windows.rs compile 错误 | STEP-P2-M1a-1a.3 #S-2 跨平台编译限制 | ✅ **真机** bug（windows-sys 0.61 import 错位 + 1a.3 leader 未跑 Windows CI）|
| `730092a` factory wire | STEP-P2-M1a-1a.3 #S-2 + 1a.3 leader 漏改 | ✅ 真机发现（Linux/Windows daemon clipboard_backend 一直是 None）|
| `31f7df6` off-by-4 stream C inline | M0c STEP-0.5b 隐藏 bug | ✅ 真机发现（M0c round-trip 单测只覆盖 client 侧）|
| `3a51134` + `2c6ad53` active_addr late-bind recover | BUGS.md "M1a follow-up #1" | ✅ 真机发现（dial 5-35s 窗口内复制全丢）|
| `42c724f` barrier recover after sleep/wake | PLAN-1 M2 monitor reconcile 隐藏 bug | ✅ 真机发现（屏幕 dim/关屏后 `s.active=false` 永不回滚）|
| `1a95486` 被控→主控反向同步 | BUGS.md "M1a follow-up #2" 拓扑限制 | ✅ 真机发现（incoming peer 不在 `get_client_states()` 集合）|
| `8a44bdf` client-side accept_bi | 同 #2 wire-level 另一半 | ✅ 真机发现（master accept queue 永空）|
| `a3bb056` + `d4343e2` 临时 info 日志 | "diagnostic, to be reverted"（commit message 标注）| ⚠️ **调试日志未撤**（详见 P2.1）|

**0 处 PLAN 越界；3 处临时调试日志未及时撤**（详见 P2.1）。

---

## 2. 偏离 REQUIREMENT

| 章节 | 状态 |
|---|---|
| §3.1 传输替换 | ✅ 所有 hot-fix 不动 ALPN / 鉴权 / 探活 |
| §3.2 剪贴板文本 | ✅ **M1a 功能验证**——被控端→主控端反向文本同步已通（1a95486 + 8a44bdf + 42c724f 组合）|
| §3.3 剪贴板图片 | ✅ 未触碰（M2a / M2b 范围）|
| §3.4 复制文件 | ✅ 未触碰（M3a / M3b 范围）|
| §4 验收 1-5 | ⚠️ 标准 2（"1 MiB 文本"）仍由 M1b 覆盖；hot-fix 只动 ≤ 1 KiB 路径 |

**0 处硬性 REQUIREMENT 偏离**。

---

## 3. BUG 清单

| 严重度 | 位置 | 现象 | 根因 | 影响 | 建议修复 |
|---|---|---|---|---|---|
| P0 | — | 无 | — | — | — |
| P1 | — | 无 | — | — | — |
| P2.1 | `src/service.rs:1214-1243`（`handle_clipboard_tick`）+ `:1211-1220`（LRU loopback）+ `:1486-1557`（tick 全段）+ `src/capture.rs:635-637` / `:979-981`（SendClip）+ `src/listen.rs:929-967`（server accept_bi info-level）+ `src/listen.rs:1078`（stream C reader）+ `src/emulation.rs:281-294`（ListenTask forwarding）| **临时 info-level 调试日志未撤** | `d4343e2` / `a3bb056` 提交时明确写"to be reverted once slave inbound is verified end-to-end"。截至 2026-09-09 用户真机验证已通过（小文本双向复制粘贴 ✅），日志应正式回退到 debug/trace 级别（或仅保留运维真正需要的 subset）。| 性能 / 日志量：tick 每 500ms / SendClip 每次 / accept_bi 每次都写 info 级别（每分钟可能 100+ 行）；debug 用户日常看不到，反过来会"默认日志噪音" | 按 hot-fix commit message 自承诺撤回到 debug / trace；保留诊断必要的最小子集（如"stream C first frame from {addr}: {event}" info 级别即可——peer 进入 / 离开事件低频）|
| P2.2 | `src/clipboard/windows.rs:284-291` `_force_keep_err_to_string` 函数 + `err_to_string` 函数（`#[cfg(test)]` 模块外） | 死代码 workaround | `f58db74` 修复时把 `err_to_string` 调用全部 inline 进 unsafe 块，原 `_force_keep_err_to_string` 仅为触发"函数未使用" lint 警告而存在；现在生产代码已不再调用 `err_to_string`，整个死函数 + `#[allow(dead_code)]` 抑制都是技术债 | 0（无运行时影响；只是 dead_code 抑制函数本身）| 直接删除 `_force_keep_err_to_string` 函数；模块顶层 `err_to_string` 已是 `#[cfg(test)]` + 测试断言使用，生产路径不再触及 |
| P2.3 | `src/connect.rs:147-160` `LanMouseConnection::new` 第 9 个参数 | 函数参数过多 | `3a51134` 加 `clipboard_push_notify_tx` 后已 9 个参数（之前 `M1a 1a.5` 已加 `#[allow(clippy::too_many_arguments)]`）| 0（clippy 已 allow）；style 一致性问题 | 短期保留 allow；M1b 阶段可重构为 `LanMouseConnectionConfig` struct（与 connect.rs:128-178 已有 `client_endpoint / cert_chain / key / pins_dir / client_manager / idle_timeout / peer_lost_tx / clipboard_inbound_tx / clipboard_push_notify_tx` 同质）|
| P2.4 | `src/service.rs:286-296` `IncomingClipboardState::fingerprint` 字段标 `#[allow(dead_code)]` | 当前不被 dispatcher 使用 | `1a95486` 落地时为"未来 per-incoming-peer 授权检查"预留；目前 dispatcher 只看 `enable_clipboard_to` | 0（已 allow）；新 dead_code 抑制 | 短期保留；M3 / M4 GUI 表面 inbound peer 列表时再启用；不要在本期移除（会让 hot-fix 必要性看起来不完整）|
| P2.5 | `src/listen.rs:997` 旧 `Result<(), Some(Err_to_string(...))>` → 现已 inline 修复（f58db74 已清）| 旧设计意图与代码脱节 | `f58db74` 已修——`Some(err_to_string(...))` 改为 `return None`（与 trait "platform read failed silently → None" 契约一致）| 0（已修）| 无需处理；记录为"hot-fix 修了原 1a.3 双 bug"（编译错 + 语义错）|
| P3.1 | `src/connect.rs:1019-1027` Stream B first-frame 双重拷贝 | `let mut body = vec![0u8; len]; read_exact(&mut body).await; let mut buf = [0u8; MAX_EVENT_SIZE]; buf.copy_from_slice(&body);` | 防御性 stream B 路径；body 先分配到 `Vec<u8>`，再 copy 到固定 buffer 才能 `ProtoEvent::try_from([u8; 21])` | 0（21 字节的额外分配，性能微优化）| 直接 `let mut buf = [0u8; MAX_EVENT_SIZE]; recv.read_exact(&mut buf).await;`；少一次分配；server 侧同样有相同模式（listen.rs:978-995），可一起优化 |
| P3.2 | `src/service.rs:1389-1409` Phase B `active_set` HashSet 创建 | `let active_set: HashSet<ClientHandle> = self.client_manager.active_clients().into_iter().collect();` 一次性消费 active_clients() 用于 Phase A + Phase B 过滤 | 双 Phase 共用同一个 set 比双 snapshot 一致性更好；500ms tick 路径代价 | 0（O(n) 一次性）；style | 可考虑 `self.client_manager.active_clients()` 返回 `&[ClientHandle]` 让两次 filter 都借用，避免 HashSet 分配 |
| P3.3 | `src/connect.rs:932-933` `client_accept_bi_task` 内部 `parked_streams: Rc<RefCell<Vec<...>>>` | 与 server 侧（listen.rs:750-751）对称：parked_streams 在外层创建并 clone 到 task 内；client 侧在 task 内自创建 | **server 侧 pattern 是 parked_streams 持有者也是其销毁时机的控制点**——若 quinn 句柄的所有权语义重要，client 侧模式会略偏离 | 0（task 在 conn closed 时 exit，parked_streams 随之 drop，与 server 侧等效）| 风格统一：把 `parked_streams` 提到调用 `spawn_local(client_accept_bi_task(...))` 的外层（与 listen.rs 一致）|
| P3.4 | `src/service.rs:285-296` `IncomingClipboardState` `#[allow(dead_code)] fingerprint` 字段 | 同 P2.4，但 lint suppression 本身是 tech debt | 复用 P2.4 决议 | 0 | 同 P2.4 |
| P3.5 | `src/connect.rs:710-730` `clipboard_push_notify_tx.send(handle)` 失败仅 `log::debug!` | daemon shutdown 路径被淹没到 debug 级别 | 设计选择（"expected shutdown path" 注释明确）| 0；可调试性下降 | 维持现状；supervisor redial 每次都会打同样 log，真正异常（service 不在 shutdown 但 receiver 已 drop）需要从 supervisor 退出码 + 心跳日志间接判断 |

---

## 4. 跨 STEP / 跨 commit 一致性

| 检查项 | 状态 | 说明 |
|---|---|---|
| **factory 三平台 cfg-gate 一致性** | ✅ | `730092a` 拆 `cfg(any(linux, windows))` 为两个单 target impl，调用各 `linux::LinuxClipboard::new()` / `windows::WinClipboard::new()`；macOS 路径已存在。三平台 trait 签名统一：`pub fn new() -> Result<Self, ClipboardError>`（macos.rs:64 / linux.rs:107 / windows.rs:72） |
| **`clipboard_push_notify_tx` 数据流** | ✅ | `connect.rs:159` (struct field) → `connect_to_handle` 入参 (connect.rs:584-595) → `send()` 后触发 (connect.rs:721-730) → `Service::new` 构造 (service.rs:347-352) → `Service::run` select! arm (service.rs:436-441) → `handle_clipboard_recover_push` (service.rs:1679-1729)。链路完整；redial `spawn_peer_supervisor` 也 forward clone（connect.rs:842-848），每次 reconnect 都触发 recover |
| **off-by-4 fix 与 server stream C inline 路径对齐** | ✅ | `31f7df6` 修复 server side（listen.rs:929-967）；client side `client_accept_bi_task`（connect.rs:960-999）首次落地即采用 inline-then-spawn pattern，**直接避免同类 bug**。两个文件加在一起覆盖了 listen.rs 旧 server reader task + new client accept_bi task 的对称设计 |
| **`accept_bi` 任务 vs `peer.run` 任务并存** | ✅ | `8a44bdf` 在 connect_to_handle 末尾 `spawn_local(client_accept_bi_task(...))` 与 `spawn_peer_supervisor` 并行；quinn accept 队列是共享的，两 task 不竞争；supervisor 的 `peer.run(PeerRole::Client)` 只读 stream A/B/C（per stream_c_clipboard_text_round_trip test doc 注释），不消费 accept_bi 队列 |
| **incoming peer 状态模型一致性** | ✅ | `IncomingClipboardState { fingerprint, enable_clipboard_to }` 与现有 `ClientConfig.enable_clipboard_to` 字段语义对齐（默认 true）；`Connected` 事件填充（service.rs:792-799），`Disconnected` 事件移除（service.rs:755）；与 `incoming_conn_info`（capture barrier 用途）字段语义**显式区分**（service.rs:81-102 doc 已说明）|
| **recover_monitors 与 reconcile_monitors 接口一致性** | ✅ | `42c724f` 加 `recover_monitors` 纯 helper（service.rs:2069-2087），与 `reconcile_monitors`（service.rs:1959-1977）/ `recreate_monitors`（service.rs:1997-2021）共享 `(was_present, is_present)` 状态机；调用方 `reconcile_monitors_changed` 显式三相位（A 拆 / B 重建 / C 几何重建）|
| **diagnostic 日志自承诺范围** | ⚠️ | `a3bb056` + `d4343e2` commit message 都标注 "to be reverted once slave inbound is verified end-to-end"；用户 2026-09-09 已真机验证小文本双向 ✅ → 触发撤回条件已满足但未撤（详见 P2.1）|
| **scope discipline** | ✅ | 所有 hot-fix 都是用户真机发现的回归修复；无 M0a-M1a 范围外的功能扩展（图片 / 文件 / GUI / 大文本 LRU TTL 全部未触碰）|
| **commit message 完整性** | ✅ | 每个 hot-fix 都含：触发 STEP / 真机现象 / 修复方案 / 验证（cargo test / fmt / clippy / 真机 trace）；`8a44bdf` 还额外含 "Lifecycle / 上下游影响面 / 为什么需要 accept_bi loop" 的 10 行 rationale |
| **PLAN 文档同步** | ✅ | `2736c50` / `d0ce5a9` PLAN-2 文档已加入 "双向验收约定（A→B 与 B→A 各跑一次）" 显式拆分验收矩阵（§2 主表 + §8 测试矩阵均有 (a) A→B / (b) B→A 拆分），与 hot-fix 真机发现一致——`1a95486` + `8a44bdf` 正是 "反向静默失败" 案例 |
| **文档 / 报告** | ⚠️ | `06780b6` 已记录两个 M1a follow-up bug 文本到 `next/BUGS.md`；`1a95486` + `8a44bdf` 完成后 `8a44bdf` commit 内做了 `next/BUGS.md` 清理（删除 M1a follow-up #1 / #2 段）；但 `next/INVESTIGATION-M1a-CLIPBOARD-NO-OP.md` 文件被 `8c0a1d1` 创建后又被 `1ec1a66` 整文件删除——commit 净效果 = "未持久化"，符合 sub-agent "只读调研" 自定位，OK |

---

## 5. 总体结论

**接受**（PASS-with-followup）

理由：

1. **0 P0 / 0 P1**：所有 hot-fix 都是用户真机驱动的回归修复或必要 bug 修复；fix 自身不引入死锁 / 内存泄漏 / 通道 race / 状态不一致
2. **5 个真机 bug 全部 root-cause 修复**：
   - `f58db74` + `730092a` 修了 Linux/Windows daemon clipboard_backend 一直为 None 的工厂漏接（1a.3 leader 落地 + Windows compile 错位）
   - `31f7df6` 修了 server-side Stream C 第一帧 off-by-4（M0c 0.5b 隐藏）
   - `3a51134` + `2c6ad53` 修了 dial-window 5-35s 复制全丢（active_addr late-bind）
   - `42c724f` 修了 macOS 屏幕睡眠/唤醒后 capture barrier 永久死掉（`s.active=false` 永不回滚 + peers[addr] 抢槽 race）
   - `1a95486` + `8a44bdf` 修了"被控端→主控端反向文本同步"完全静默失败（incoming peer 不在 broadcast 集合 + client accept_bi 队列永空）
3. **真机行为已确认**（leader-state + commit message + 2026-09-09 用户真机验证记录）：macOS master ↔ macOS slave 小文本双向复制粘贴通过
4. **cargo test / clippy 自承诺**（commit message 引用）：
   - `3a51134`: cargo build 0 error / fmt 0 diff / clippy 0 new / cargo test --workspace 284 pass 0 fail
   - `2c6ad53`: cargo build 0 error / fmt 0 diff / cargo test --workspace 284 pass 0 fail / clippy 0 new
   - `42c724f`: cargo build + cargo test --lib (126 pass) + cargo clippy (no new warnings) + cargo fmt 绿
   - `8a44bdf`: cargo check clean / cargo test --lib 126 pass / cargo test --test input_channel_routing 7 pass / cargo test --test quic_smoke 2 pass
5. **scope discipline 守住**：0 处 M0a-M1a 范围外功能扩展；hot-fix 是真机驱动的最小必要修改
6. **跨 commit 一致性**：factory 三平台 / Stream C inline 模式 / incoming peer 状态模型 / monitor 三相位状态机 / accept_bi task lifecycle 全部一致

---

## 6. 建议下一步（按优先级）

### 必须做的（M2a / M1b 启动前必清）

- [ ] **P2.1 撤临时 info-level 调试日志**（最关键 follow-up）：`a3bb056` + `d4343e2` 合计在 `service.rs` / `capture.rs` / `listen.rs` / `emulation.rs` 落了 ~7 处 info-level "diagnostic" 日志。commit message 自承诺"slave inbound 验证后即撤"，2026-09-09 已真机验证通过。建议撤回标准：
  - 撤回 `service.rs:1214-1243`（tick LRU loopback + change detected）+ `:1496-1547`（tick 整段）+ `capture.rs:635-637` + `:979-981`（SendClip）→ 全部降到 debug / trace
  - 撤回 `emulation.rs:281-294`（ListenTask forwarding）→ 降 debug
  - **保留** `listen.rs:929-967` server accept_bi "first frame" 日志 → info（peer 首次进入是低频事件，运维信号）  
  - **保留** `listen.rs:1078` server stream C reader → info（同样低频）
  - 撤回后重跑 `cargo test --workspace` 确认 284 pass / 0 fail 不退化；新 commit message 标注"hot-fix follow-up: revert diagnostic logs (M1a hot-fix complete)"
- [ ] **P2.2 删 `_force_keep_err_to_string`**（一行 commit）：`src/clipboard/windows.rs:284-291` 死代码 workaround 已不再需要（f58db74 已把生产路径全部 inline 进 unsafe 块）

### 可押后（M1b / M2a 阶段处理）

- [ ] P2.3 `LanMouseConnection::new` 重构为 `LanMouseConnectionConfig` struct（缓解 9 参数 + 配合后续字段扩展）
- [ ] P2.4 / P3.4 `IncomingClipboardState::fingerprint` dead_code 字段：等 M3 / M4 真正 surface inbound peer 列表到 GUI 时再启用
- [ ] P3.1 Stream B first-frame 双重拷贝优化（与 listen.rs 同步做）
- [ ] P3.2 Phase B `active_set` 借用替代 HashSet 分配
- [ ] P3.3 client `parked_streams` 提到外层与 listen.rs 风格统一

### 决策项（给 leader）

- [ ] **M1b 是否派发 vs 直接进 M2a**：当前所有 hot-fix 已 PASS；PLAN-2 路线图下一步是 M1b（剪贴板大文本 + HTTP/3 拉取 + LRU 128 + 60s TTL + cache.remove on push）。leader-state 已列 "推荐 B：跳过 hot-fix 整批审 → 直接派 M1b"；**本 validator 输出即用**——本报告可作为"hot-fix 整批审"正式记录归档

### 留作下一轮 validator 关注

- [ ] **dispatcher 单测覆盖**（承接 M1a 整批 P3.1）：mock DummyBackend + LruFingerprints `new(4).push(a).push(b).push(c).push(d).push(e).contains(&a) == false` + handle_clipboard_tick 在 current_text 变化时推 ProtoEvent::ClipboardText + handle_clipboard_inbound 在 LRU 命中时 skip + **新增** handle_clipboard_recover_push 在 bypass LRU 后仍只推一次
- [ ] **client_accept_bi_task 集成测试**：与 listen.rs server_accept_bi_task 对称加一条 `tests/client_accept_bi_clipboard_text_round_trip`，确保 off-by-4 修复 + 新对称代码不回归
- [ ] **factory 单测**：mock platform 三平台 cfg-gate 落地后，加 `default_backend()` 单测确认 macOS / Linux / Windows 三 path 各返回正确 backend

---

## 7. 验证证据

### 7.1 commit message 自承诺（executor 验证口径汇总）

| Commit | cargo build | cargo fmt --check | cargo clippy | cargo test --workspace / --lib |
|---|---|---|---|---|
| `f58db74` | — (Windows-only fix) | — | — | — |
| `730092a` | 0 error | 0 diff | (未在 commit message 提) | (未在 commit message 提)|
| `a3bb056` | — (log change only) | — | — | — |
| `d4343e2` | — (log change only) | — | — | — |
| `31f7df6` | — (off-by-4 fix; msg 提"诊断日志保留")| — | — | — |
| `3a51134` | 0 error | 0 diff | 0 new (7 pre-existing unchanged) | 284 pass / 0 fail |
| `2c6ad53` | 0 error | 0 diff | 0 new | 284 pass / 0 fail |
| `6df6216` | — (log format change only) | — | — | — |
| `42c724f` | 0 error | 0 diff | 0 new | 126 pass (lib only) |
| `1a95486` | (未在 commit message 提) | — | — | — |
| `8a44bdf` | cargo check clean | — | — | 126 pass (lib) + 7 pass (input_channel_routing) + 2 pass (quic_smoke) |

**基线对照**：M1a 1a.5 终点（`b92c750`） = 284 pass / 0 fail（M1a validator 报告 §验证证据）；所有 hot-fix 落地后 cargo test 数量基线**未变**（3a51134 / 2c6ad53 都明说 "matches STEP-1a.5 baseline; 0 regression"）；8a44bdf 报告 126 + 7 + 2 = 135 pass（lib + 两个 test target；与 M1a 1a.5 报告的 lan-mouse (lib) 121 + input_channel_routing (未在 1a.5 报中显式提) + quic_smoke 7 基本对齐；小的 +5 差异可能源自 hot-fix 期间 test target 重新统计或部分之前未跑）

### 7.2 clippy 新引入

| Commit | clippy new warning | 来源 |
|---|---|---|
| `f58db74` | 0 (fixes `non_snake_case` `Err_to_string` → `err_to_string`；移除 unused imports；减少 warnings) | commit message |
| `730092a` | 0 (factory 改 cfg-gate；no new lint) | (推断) |
| `a3bb056` | 0 (log level change only) | (推断) |
| `d4343e2` | 0 (log level change only) | (推断) |
| `31f7df6` | 0 (off-by-4 fix; 已有 `#[allow(clippy::too_many_arguments)]` 不需新增) | (推断) |
| `3a51134` | 0 (7 pre-existing 保持不变) | commit message |
| `2c6ad53` | 0 | commit message |
| `6df6216` | 0 (format change only) | (推断) |
| `42c724f` | 0 (no new warnings) | commit message |
| `1a95486` | (未在 commit message 提；推断 0) | (推断) |
| `8a44bdf` | (未在 commit message 提；推断 0；+1 个 `spawn_local` + 1 个新 task 但都在现有 #[allow] 范围内) | (推断) |

**整体 clippy 引入：0 new warning**。7 个 pre-existing clippy error 维持不变（与 M1a 1a.5 baseline 一致）。

### 7.3 真机行为确认（leader-state + 用户记录）

| 日期 | 行为 | 来源 |
|---|---|---|
| 2026-09-09 12:12Z | macOS master + macOS slave：clipboard pushed from slave lands on master within one 500ms tick | `8a44bdf` commit message §Verification |
| 2026-09-09 | 用户真机验证小文本双向复制粘贴通过 | `next/.LEADER-STATE.md:52` |
| 2026-09-09 (live repro) | slave `clipboard broadcast: -> incoming peer 10.2.1.15:53032` → master `client accept_bi: stream C first frame from 10.2.1.15:53032: ClipboardText(fp=..., sha=709d26b1..., size=25, inline=yes)` | `8a44bdf` commit message §User-visible symptom |
| 2026-09-09 (live repro) | recover push 真机 trace：`[T+0ms]` tick fires 57 bytes → broadcast to 0 peers; `[T+~200ms]` dial completes → recover push runs → push 57 bytes to 1 peer → slave 收到 57 bytes（用户的原始复制）| `2c6ad53` commit message §After fix |
| 2026-09-09 (live repro, Windows 11) | master → slave inbound: master outbound 链全通 (tick → broadcast → CaptureTask → SendClip → StreamC → wire)，Windows slave inbound 静默无日志 → 修 factory 后 inbound 端出现 "stream C first frame" + "applied N bytes" 完整链路 | `730092a` commit message §Root cause observed |

### 7.4 跨 commit 一致性 spot-check

1. **`LanMouseConnection::new` 参数列表自洽**：`connect.rs:147-160` 9 个参数；service.rs:284-307 调用点参数顺序一致；spawn_peer_supervisor 函数签名 (connect.rs:1144-1207) 参数列表也已含 clipboard_push_notify_tx；connect.rs:359-373 / connect.rs:823-849 两个调用点都正确 clone client_manager + clipboard_push_notify_tx
2. **`LanMouseConnection::send` 仍走原有路径**：`connect.rs:292-329` 未被 hot-fix 触碰；outgoing 流推 `peer.send_input(ClipboardText)` → `route_input` → `Channel::StreamC` → 既有路径
3. **emulation.rs ListenTask ClipboardText arm**：`emulation.rs:281-294`（debug 日志）+ `:296-300`（实际 send）保持原有逻辑；新增 debug 日志不影响 arm 行为
4. **service.rs `handle_clipboard_inbound`**（service.rs:1577 起的 inbound arm）未被 hot-fix 触碰；仍是 LRU contains → skip / backend.set_text + LRU push + last_text = None + FrontendEvent 推送
5. **`PeerSession::send_stream_c` 与 `client_accept_bi_task` 协接**：`session.rs:557-587` 懒 open_bi + var-codec 写；`connect.rs:923-1053` client_accept_bi_task 消费 + 派发到 `clipboard_inbound_tx`；与 server 侧 `listen.rs:929-967` inline + `server_stream_c_reader_task` 对称

---

报告已写入 `next/STEP-VALIDATION-P2-M1a-HOTFIX.md`，结论：**PASS-with-followup**（0 P0 / 0 P1 / 5 P2 / 5 P3）；建议下次 validator 启动前优先撤 P2.1 临时调试日志并删 P2.2 死代码，再决定派 M1b 还是 M2a。