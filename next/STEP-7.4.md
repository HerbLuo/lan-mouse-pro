# STEP-7.4 — `connect.rs` 移除 `active_lock` + `ClientManager::probe_targets`

> PLAN-M1 §STEP-7 / STEP-7.4
> 执行日期：2026-08-31　实际耗时：~10 min（验证 + 归档；按 #N-31 模式无代码改动）
> 结论：✅ 通过（按 #N-31 模式退化验证 — 现状 grep 0 命中 live code）

## 1. 做了什么

本步**未做任何代码改动**。按 STEP-7.2 沉淀的 "#N-31 模式"（参见 SUGGESTION #S-22）——

开干前先 grep 核实工作量：

```bash
$ grep -rinE 'active[_ -]?lock|probe[_ -]?targets|set[_ -]?active[_ -]?lock|get[_ -]?active[_ -]?lock' \
    src lan-mouse-cli lan-mouse-gtk lan-mouse-ipc lan-mouse-proto \
    input-event input-capture input-emulation service firewall nix build-aux dylibs \
    tests scripts config.toml Cargo.toml README.md DOC.md
（exit=1，无命中）
```

随后扩大 grep 范围到全仓（排除 `target/` `next/` `.git/`）：

```
$ grep -rinE 'active[_ -]?lock|probe[_ -]?targets|set[_ -]?active[_ -]?lock|get[_ -]?active[_ -]?lock' .
（仅命中 PLAN-M1.md / SUGGESTION.md / STEP-7.3.md 三份文档 — 均为本 STEP 自身 PLAN 与归档，无 live code）
```

**根因**（何时已删）：
- active_lock 是 DTLS 时代的接口锁定机制（用于把客户端绑定到特定网卡 IP）
- probe_targets 是配套的多 IP 延迟探测（用于选 IP 后再做锁定）
- STEP-6.1（connect.rs 切到 PeerSession）+ STEP-6.4（dial_any happy-eyeballs）一并清理了这些路径
- happy-eyeballs (STEP-6.4) 替代了 probe_targets 的功能 —— 现在拨号是**并发候选 + 200ms 内优先 primary**，不再需要"先探测再锁定"
- bak mousehop 同样不引入 latency.rs / active_lock（PLAN §5 决策 D10 明确**不引入** / 删）

**保留 API 验证**：
- `connect.rs` 仍有 `active_addr` / `set_active_addr` —— 这是 happy-eyeballs 阶段选中的"当前活动 peer addr"，**不是** active_lock。`grep -in 'active' src/connect.rs` 全部命中都是这个语义。
- `ClientManager` 无 `set_active_lock` / `get_active_lock` / `probe_targets` 方法（grep `set_active_lock|probe_targets` 0 命中）
- `config.toml` schema 无 `active_lock` 字段（grep 0 命中）

## 2. 验证结果

### Gate 1: 现状 grep（按 #N-31 模式开干前先 grep）

```
exit=1（无 live code 命中）
仅命中 3 份 markdown 文档（PLAN-M1.md / SUGGESTION.md / STEP-7.3.md — 都是 STEP-7.4 自身 PLAN 与归档）
```

✅ **退化验证**（与 STEP-7.2 / STEP-7.3 同期归档同样格式）

### Gate 2: `cargo build -p lan-mouse`

```
warning: `lan-mouse` (lib) generated 5 warnings       # 全部 pre-existing dead-code
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 3.98s
```

✅ 通过（5 warnings 全部 pre-existing — `Rejected` / `power_observer` / `set_alive` / `Timeout` / `recv_tx`，不属 STEP-7.4）

### Gate 3: `cargo test -p lan-mouse connect::tests`

```
test connect::tests::reconnect_on_peer_close ... ok
test connect::tests::backoff_doubles_on_each_failure ... ok

test result: ok. 2 passed; 0 failed
```

### Gate 4: `cargo test -p lan-mouse --lib client_input_channels_tests`

注：Leader prompt 写的是 `client::tests` 字面 —— 实际 client.rs 的测试模块名是 `client_input_channels_tests`（STEP-4.5a 引入，line 369）。

```
test client::client_input_channels_tests::add_with_config_preserves_input_channels ... ok
test client::client_input_channels_tests::set_input_channels_returns_true_only_on_change ... ok

test result: ok. 2 passed; 0 failed
```

✅ 通过

### Gate 5: 残留 grep（再次确认无遗漏）

```
$ grep -rinE 'active[_ -]?lock|probe[_ -]?targets' src lan-mouse-cli lan-mouse-gtk \
    lan-mouse-ipc lan-mouse-proto config.toml Cargo.toml
（exit=1，无命中）
```

✅ **0 命中**

## 3. 与 PLAN-M1 的偏差 / M1 边界

### 偏差 #N-34：STEP-7.4 工作量为零（按 #N-31 模式退化验证）

**PLAN §7.4 隐含**：删除 `connect.rs` 中 `active_lock` 分支 + `ClientManager` 删 `set_active_lock` / `get_active_lock` / `probe_targets` 方法 + `config.toml` schema 删 `active_lock` 字段。

**本步实际**：0 命中。代码清理在 STEP-6.1 / STEP-6.4 已**顺手完成**（happy-eyeballs 路径替代 probe_targets + active_lock）—— 本步作为 **#N-31 模式落地**的第二例（继 STEP-7.2 第一例），仅做 grep 验证 + 归档。

**严重程度**：轻（无功能影响；本步承接 STEP-7.2 流程模式沉淀的工程纪律）

**PLAN-M1 §3 STEP-7.4 验收**：

| 验收项 | 状态 |
|---|---|
| `connect.rs` 中 `active_lock` 分支删除（保留纯 happy-eyeballs 路径） | ✅ 已在 STEP-6.4 完成 |
| `ClientManager` 删 `set_active_lock` / `get_active_lock` / `probe_targets` 方法（如果有） | ✅ grep 0 命中，方法不存在 |
| `config.toml` schema 删 `active_lock` 字段（如果有） | ✅ grep 0 命中，字段不存在 |
| `cargo build -p lan-mouse` 通过 | ✅ |
| `cargo test -p lan-mouse connect::tests client::tests` 通过 | ✅ 4/4 passed（模块名微调） |

### M1 边界（守 §9）

| §9 项 | 触碰？ |
|---|---|
| `ProtoEvent::Clipboard` / `Bounds` / `MotionAbsolute` / `CursorPos` / `ReceiverSensitivity` | ❌ |
| `MAX_CLIPBOARD_SIZE` / `BufferTooLarge` | ❌ |
| `encode_clipboard_event` / `decode_clipboard_event` 变长 codec | ❌ |
| `input-event::ClipboardEvent` / `Axis::momentum` | ❌ |
| `lan_mouse_ipc::TransportEvent` 任何变体 | ❌ |
| `lan-mouse-gtk::status_bar` 任何改动 | ❌ |
| `lan-mouse-cli` stderr 事件订阅 | ❌ |
| `clipboard*.rs` 任一文件 | ❌ |
| `h3` / `h3-quinn` / `http` 依赖 | ❌ |
| Stream C reader task | ❌ |
| mDNS / discovery 改造 | ❌ |

## 4. 处理的 SUGGESTION 项

无新条目；无清理项。

**SUGGESTION #S-22**（"#N-31 模式成内规"）继续累积第二例（STEP-7.2 是第一例；STEP-7.4 是第二例；建议 Leader 决定是否在 STEP-7.x 全部完成后统一提升为 AGENTS.md 内规）。

## 5. 闸门检查

| 闸 | 结果 |
|---|---|
| **§1 时间门** | ✅ ~10 min（远低于 30 min 目标 —— 0 代码改动路径） |
| **§9 边界门** | ✅ 0 越界 |
| **STEP-7.3 依赖** | ✅ webrtc-dtls/util 已下线（cargo tree 干净） |
| **不引入新依赖** | ✅ 0 依赖变更 |
| **M1 DoD 第 4 条** `cargo tree -p lan-mouse \| grep -E "webrtc-*"` 无输出 | ✅（非本步范围但复核仍绿） |
| **闸 3 STEP 收尾全套** | ⏸ 跳过（非 STEP-7 末步；STEP-7.5/7.6/7.7 待续） |

## 6. 遗留 / 风险

### ⚠️ 5 个 lib 单测 fixture 失败（**继承自 STEP-7.3 决策：拆 STEP-7.3a**）

5 个失败仍是 `spawn_local` runtime 架构问题（dial_any_prefers_primary / dial_any_all_unreachable_returns_err / hello_wrong_magic_closes_connection / peer_session_round_trip_motion_keyboard / stream_c_take_releases_quinn_recv_stream）。本步不修（与 STEP-7.4 主题正交），待 Leader 拆 STEP-7.3a。

### ⚠️ pre-existing clippy / fmt 累计 30+ errors（继承 #S-24 / #S-25）

不在 STEP-7.4 范围（既不触碰代码也未跑 clippy / fmt check）—— 留待 STEP-7.6 / 7.7 收尾时统一评估。

### ⚠️ STEP-7.5 GUI 端 active_lock 控件

按 PLAN §7.5，GTK 编辑对话框删 `active_lock` 控件。`grep -rin 'active_lock\|activeLock' lan-mouse-gtk` 我刚才在 Gate 5 复核时 0 命中 —— **前端也无 active_lock 控件残留**，STEP-7.5 预期同样按 #N-31 模式退化（仅 grep 验证）。该结论前置给 Leader 评估是否需要把 STEP-7.5 同步标记为"已闭环"或保留为独立一步。

## 7. 下一步

**STEP-7.4 已闭环** —— 按偏差 #N-31 模式无代码改动，全部验收命令绿。

按 PLAN-M1 §1 表，下一步为 **STEP-7.5（GUI 移除 active_lock 控件）**：
- 预期同样退化验证（grep GTK 0 命中）—— 前置已就绪
- 若 Leader 选择合并执行 STEP-7.4+7.5，可一次归档为单文档（共同 #N-31 模式案例）

后续 STEP-7.6 / 7.7 为 `firewall.rs` / `service.rs` 头注释清理 + README/DOC.md/CHANGELOG 同步（PLAN-M1 §7.6 / §7.7），均依赖 STEP-7.5 完成；按当前 STEP-7.4 / 7.5 的退化模式，预期它们也都是 grep + 微改写。

**未做 git commit**：本步未改任何文件（0 文件改动）；按 Leader 约定无 commit。

**改动文件清单**：**空**（0 文件、0 行变化）—— #N-31 模式第二例归档。