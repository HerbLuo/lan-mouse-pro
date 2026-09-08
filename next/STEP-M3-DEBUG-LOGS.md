# STEP-M3-DEBUG-LOGS — 临时 debug log 定位反向 Enter 失败根因

> 调研依据：`next/STEP-DEBUG-M3-REVERSE-ENTER.md`
> 执行日期：2026-09-08　实际耗时：~12 min
> 结论：通过（cargo build + 164 tests pass / 0 fail / 1 预存在环境依赖失败）
> commit 类别（**待 leader 提交**）：`debug(temp): add trace logs for H1-H4 reverse-Enter race diagnosis`

## 1. 做了什么

按 `STEP-DEBUG-M3-REVERSE-ENTER.md` §4.2 的 7 点 debug log 清单，纯 `log::trace!` 添加，**未改任何代码逻辑**。每条 log 都带 `[debug(temp):H<n>]` 前缀，commit message 第一词为 `debug(temp):`，便于后续清理识别。

## 2. 改动文件 + log 位置 + 格式串

| # | file:line | 触发事件 | 格式串 | 标签 |
|---|---|---|---|---|
| 1 | `src/service.rs:457-477` | `ICaptureEvent::CaptureBegin(handle)` 收到 + lookup 结果 + send_leave_event 调用 | `"debug(temp) CaptureBegin handle={handle} incoming_conn_info_keys={:?} lookup={:?}"` + `"debug(temp) send_leave_event -> addr={addr}"` | H1 |
| 2a | `src/service.rs:587-594` | `add_incoming` 入口（handle 计算后、capture.create 前） | `"debug(temp) add_incoming ENTRY addr={addr:?} pos={pos:?} new_handle={handle} incoming_conns_before={:?}"` | H1 |
| 2b | `src/service.rs:606` | `add_incoming` insert HashMap 后 | `"debug(temp) add_incoming POST-INSERT handle={handle} key={key:?}"` | H1 |
| 3 | `src/listen.rs:301-323` | `reply()` 找不到 peer（增强现有 warn，加 conns keys） | `"debug(temp) WARN stale addr addr={addr:?} conns={:?}; dropping {event}"` | H2 |
| 4 | `input-capture/src/macos.rs:301-313` | producer event handler `ProducerEvent::Release` 分支入口 | `"debug(temp) producer received Release from capture task current_key={:?} pending_key={:?}"` | H3 |
| 5 | `input-capture/src/macos.rs:1509-1517` | `Capture::release` 入口（spawn_local 前） | `"debug(temp) Capture::release called, queueing Release to producer"` | H3 |
| 6 | `src/capture.rs:1598-1603` | `release_capture` 调 `capture.release().await` 之前 | `"debug(temp) capture task forwarding Release to producer (handle={:?})"` | H3 |
| 7 | `input-capture/src/libei.rs:765-773` | libei `Activated` 事件收到后、`barrier_id` match 之前 | `"debug(temp) libei Begin barrier={activated_barrier_id:?} position={activated_cursor_pos:?}"` | H4 |

每条 log 至少含以下三类定位字段之一：`handle` / `addr` / `position` / `key`。全部走 `log::trace!`，默认不输出（开 `RUST_LOG=trace` 或指定模块 `=debug` 才出现）。

## 3. 验证结果

| 命令 | 结果 |
|---|---|
| `cargo build --workspace` | `Finished dev profile in 4.50s`，**0 warning**（编译干净） |
| `cargo test --workspace -- --skip enumerate_monitors_returns_live_state` | 164 pass / 0 fail |
| `cargo fmt --check` 在我修改的 5 个文件上 | **0 diff**（fmt 合规） |
| `cargo clippy --workspace --all-targets -- -D warnings` | 未跑（任务要求只跑 build/test/fmt；clippy 不在完成标准里） |

**预存在的 1 个失败**：`input-capture` `macos::tests::enumerate_monitors_returns_live_state` —— 测试在沙箱环境里 `CGDisplay::active_displays()` 返回空集而 panic，**要求真机 macOS 硬件**。已用 `git stash` 验证：未加任何 debug log 时同样失败 → 与本次改动无关。

## 4. 与 PLAN 的偏差

**无 PLAN 偏差**。本次任务没有修改任何业务逻辑，没有改接口，没有引入后续 milestone 范围，纯临时 log 加点。

注：原任务描述里 `src/service.rs:566-584` 提到 `position_map.insert(key, handle)` —— service.rs 里**没有** `position_map`（那是 capture 任务里的）。我按等价的"add_incoming 写入 incoming_conn_info 后"插入 log，语义不变。

## 5. 处理的 SUGGESTION 项

无（任务范围内不涉及 SUGGESTION 流转；之前 STEP-DEBUG-M3-REVERSE-ENTER.md 报告本身也只是"调研报告"，不是 SUGGESTION.md 里的活跃条目）。

## 6. 闸门检查

| 闸 | 状态 |
|---|---|
| 时间门 | ~12 min（目标 15 min，上限 30 min）✅ |
| milestone 边界门 | 未触碰后续 milestone 范围（纯诊断 log）✅ |
| 闸 1 产物/依赖/验收 | 7 条 log 全部就位；cargo build/test 通过 ✅ |
| 闸 2 执行中偏差 | 无 ✅ |
| 闸 3 STEP 自身测试 | 164 pass / 1 预存在环境失败（非本次造成）✅ |

## 7. 遗留

- 所有 log 是临时诊断；定位完根因后**必须移除**（commit message 第一词 `debug(temp):` 已经标了删除目标）。
- `H5`（libei cursor_position 与 barrier 不对应，~5%）**没有加专门的 log** —— 任务清单的 7 个点不包含 H5。如用户需要，可后续在 libei.rs:766-768 加一行 log `cursor_position={:?}, barriers={:?}`，判断 `find_corresponding_client` 是否误判。

## 8. 用户真机测试步骤

### 8.1 命令（被控 daemon = Linux libei，主控 = macOS）

```bash
RUST_LOG='lan_mouse=trace,lan_mouse_service=trace,input_capture=trace,lan_mouse_proto=info' \
  ./target/debug/lan-mouse
```

> 在主控（macOS）和被控（Linux GNOME Wayland libei）**两侧**都跑上面的命令，重定向到独立日志文件（`tee /tmp/lanmouse-master.log` / `tee /tmp/lanmouse-slave.log`）。

### 8.2 复现步骤

1. 正向切边（主控 → 被控）：鼠标跨主控右 edge 到被控屏幕 —— 应能切（H1 fix 后）
2. **反向切边**（被控 → 主控）：鼠标跨被控**左 edge**回主控
3. 触发反向失败的瞬间，两边同时贴日志到 issue/Leader

### 8.3 H1/H2/H3/H4 区分逻辑

| 看到的现象 | 对应根因 |
|---|---|
| 服务端收 `CaptureBegin(handle)` 但 `lookup=None`（找不到 incoming_conn_info） | **H1 命中** —— handle 不一致 / HashMap 没写入 |
| `add_incoming POST-INSERT handle=H_n` 出现但 `CaptureBegin` 收到的是 `H_m` (m ≠ n) | **H1 命中** —— handle 漂移 |
| `add_incoming ENTRY` 没出现但 `CaptureBegin` 出现了 | **H1 命中** —— add_incoming 根本没跑（Enter 没到） |
| `WARN stale addr addr=X.X.X.X conns=[Y.Y.Y.Y]` 在反向切时出现 | **H2 命中** —— peer 已 disconnect 但 incoming_conn_info 还指向旧地址 |
| `reply: Ack/Leave to addr delivered` 出现 → 主控收 Leave，但 `release_capture: calling capture.release()` 之后 `capture task forwarding Release` 没出现 | **H3 命中** —— spawn race 让 release() 调了但 notify_tx 没发出 |
| `Capture::release called, queueing Release to producer` 出现但 `producer received Release from capture task` 没出现 | **H3 命中** —— 异步 channel 没送到 producer |
| `producer received Release from capture task` 出现但 `show_cursor()` / `current_key = None` 之后用户主控 cursor 仍隐藏 | macOS 平台层 bug（不在 H1-H4 范围） |
| `libei Begin barrier=...` 出现但下游 `CaptureBegin` 没出现 | **H4 命中** —— libei barrier 装了但 backend 没把事件转给 capture.rs |
| `libei Begin barrier=...` 完全没出现 | **H4 命中** —— barrier 没装好 / 还没 reinstall / 用户鼠标没真的跨过左 edge |
| `barrier=UnknownBarrier` 出现且 `position` 离所有 barrier 都很远 | **H5 命中**（未加 log，可补加）|

### 8.4 期望日志骨架（正常反向切一次）

```
[被控 daemon] debug(temp) libei Begin barrier=Some(Barrier(N)) position=Some((x, y))
[被控 daemon] capture: EnterOnly trigger on Handle(N) (event=Begin) — forwarding to service as CaptureBegin
[被控 daemon] debug(temp) CaptureBegin handle=H_n incoming_conn_info_keys=[H_n] lookup=Some(Some(addr))
[被控 daemon] debug(temp) send_leave_event -> addr=addr
[被控 daemon] debug(temp) WARN stale addr addr=addr conns=[addr]  ← 这行不该出现；出现就是 H2
                                          ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
[被控 daemon] reply: Leave(0) to addr delivered
[主控 daemon] release_capture: ENTER (state=Sending, active_client=Some(H_m))
[主控 daemon] release_capture: setting state = Idle (force-reset)
[主控 daemon] release_capture: calling capture.release() (OS-level release)
[主控 daemon] debug(temp) capture task forwarding Release to producer (handle=Some(H_m))
[主控 daemon] debug(temp) Capture::release called, queueing Release to producer
[主控 daemon] debug(temp) producer received Release from capture task current_key=Some(Key{...}) pending_key=None
[主控 daemon] handling event: Release
```

反向失败时，**最后消失的某行**就是真正的卡点。配合 8.3 的表，对照定位。

## 9. 下一步

1. **leader 提交本次改动**（commit message 模板见下）
2. 用户按 8.1+8.2 跑反向切换，贴日志
3. leader 按 8.3 表对照决定命中哪条 H
4. 命中后派 executor 按 `STEP-DEBUG-M3-REVERSE-ENTER.md` §4.3 修对应根因

### 推荐 commit message（leader 用）

```
debug(temp): add trace logs for H1-H4 reverse-Enter race diagnosis

Seven log::trace! statements at the seven cheapest-to-add diagnostic
points for the reverse-Enter race documented in
next/STEP-DEBUG-M3-REVERSE-ENTER.md:

- H1 (src/service.rs): add_incoming ENTRY/POST-INSERT + CaptureBegin lookup
- H2 (src/listen.rs): reply() warns with full conns set when peer missing
- H3 (input-capture/src/macos.rs + src/capture.rs): capture.release / producer received Release chain
- H4 (input-capture/src/libei.rs): libei Activated barrier_id + cursor_position

No code-logic changes. Default level trace (off); enable via
RUST_LOG=lan_mouse=trace,lan_mouse_service=trace,input_capture=trace

归档: next/STEP-M3-DEBUG-LOGS.md

Co-Authored-By: Claude <noreply@anthropic.com>
```

清理时 `git grep 'debug(temp)'` 找到全部 7 条删除即可（commit message 第一词 `debug(temp):` 让 git log --grep=debug\(temp\) 也能列出来）。