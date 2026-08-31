# STEP-7.5 — GUI 移除 `active_lock` 控件

> PLAN-M1 §STEP-7 / STEP-7.5
> 执行日期：2026-08-31　实际耗时：~10 min（grep 验证 + 归档；按 #N-31 模式无代码改动）
> 结论：✅ 通过（按 #N-31 模式退化验证 — 现状 grep 0 命中 live code）

## 1. 做了什么

本步**未做任何代码改动**。按 STEP-7.2 / STEP-7.3 / STEP-7.4 沉淀的 "#N-31 模式"（参见 SUGGESTION #S-22）——

开干前先 grep 核实工作量（按 Leader prompt §5 + 偏差 #N-31 模式）：

```bash
$ grep -rn "active_lock|activeLock|Lock to specific interface|Probe all interfaces" \
    lan-mouse-gtk
（exit=1，无命中）
```

随后扩大 grep 范围到变体命名：

```bash
$ grep -rniE "active[_ -]?lock|activeLock|lock[_ -]?to[_ -]?specific[_ -]?interface|\
    probe[_ -]?all[_ -]?interfaces|interface[_ -]?lock|lock[_ -]?interface|interfaceLock" \
    lan-mouse-gtk
（exit=1，无命中）
```

再宽 grep `interface|lock` 全文复核 —— 仅命中 GTK XML `<interface>` 元素起止标签 + `block_signal`（信号阻塞 API，与 "interface lock" 语义无关）。

**根因**（何时已删）：
- active_lock 是 DTLS 时代的接口锁定机制（用于把客户端绑定到特定网卡 IP）
- GTK 端的"锁定到特定接口下拉框" / "探测所有接口延迟开关" 本来就未引入过主仓 —— 与 PLAN §5 决策 D10（`latency.rs` / `active_lock` **不引入** / 删）对齐
- bak `mousehop-gtk` 也没有对应 GUI 控件（grep bak 一致 0 命中）
- STEP-6.1 / STEP-6.4（happy-eyeballs）替代了 active_lock 的作用，且前端的 peer 配置 UI 一开始就没暴露这个维度

**路径修正记录**（SUGGESTION #S-13 已闭环）：
- PLAN §7.5 字面写 `lan-mouse-gtk/src/ui/client_editor.rs`
- 实际架构：`lan-mouse-gtk/resources/client_row.ui` + `lan-mouse-gtk/src/client_row/imp.rs` + `lan-mouse-gtk/src/client_row.rs`
- 本步按实际架构走 grep —— 与 SUGGESTION #S-13 决策一致

## 2. 验证结果

### Gate 1: 现状 grep（按 #N-31 模式开干前先 grep）

```
$ grep -rn "active_lock|activeLock|Lock to specific interface|Probe all interfaces" \
    lan-mouse-gtk
（exit=1，无命中）

$ grep -rniE "active[_ -]?lock|activeLock|lock[_ -]?to[_ -]?specific[_ -]?interface|\
    probe[_ -]?all[_ -]?interfaces|interface[_ -]?lock|lock[_ -]?interface|interfaceLock" \
    lan-mouse-gtk
（exit=1，无命中）
```

✅ **退化验证**（与 STEP-7.4 同期归档同样格式 —— #N-31 模式第三例）

### Gate 2: GTK 模板与代码复核

`lan-mouse-gtk/resources/client_row.ui` 实际控件清单（grep `<object class=`）：

| 控件 | id | 用途 |
|---|---|---|
| `GtkSwitch` | `enable_switch` | enable client |
| `GtkButton` | `dns_button` | resolve host |
| `GtkSpinner` | `dns_loading_indicator` | DNS loading |
| `GtkEntry` | `hostname` | hostname 编辑 |
| `GtkEntry` | `port` | port 编辑 |
| `AdwComboRow` | `position` | peer position |
| `AdwComboRow` | `input_channels_mouse_button` | 鼠标 button channel（STEP-4.5b） |
| `AdwComboRow` | `input_channels_keyboard` | 键盘 channel（STEP-4.5b） |
| `AdwActionRow` | `delete_row` | delete client container |
| `GtkButton` | `delete_button` | delete client |

—— **无 active_lock / interface_lock / Probe all interfaces 相关控件**

`lan-mouse-gtk/src/client_row/imp.rs` `TemplateChild` 字段清单（grep `#\[template_child\]`）：

```
enable_switch / dns_button / hostname / port / position /
input_channels_mouse_button / input_channels_keyboard /
delete_row / delete_button / dns_loading_indicator
```

—— **无 active_lock 相关字段**

`lan-mouse-gtk/src/client_row.rs` `pub fn` 清单（grep `pub fn`）：

```
new / bind / unbind / set_active / set_hostname / set_port /
set_position / set_dns_state / set_input_channels / refresh_version_status
```

—— **无 active_lock 相关方法**

### Gate 3: `cargo build -p lan-mouse-gtk`

```
Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.23s
```

✅ 通过（0 warnings —— 与 STEP-4.5b / STEP-7.4 完工后一致）

### Gate 4: 残留 grep（再次确认无遗漏）

```
$ grep -rinE "active[_ -]?lock|activeLock" lan-mouse-gtk
（exit=1，无命中）
```

✅ **0 命中**

### Gate 5: 手动 GUI 测试说明（PLAN §7.5 验收项文档步骤）

按 PLAN §7.5 "手动 GUI 测试：打开 peer 编辑对话框，断言无 active_lock 控件" 落地为可重读步骤：

```text
1. cargo run -p lan-mouse-gtk                              # 启动 GTK 客户端
2. 在 peer 列表点任一 peer 旁的 expander arrow，展开编辑面板
4. 视觉检查面板包含的控件：
   - Enable switch（前缀）
   - DNS resolve button（后缀）
   - hostname / port 编辑行
   - position 下拉
   - Mouse button channel 下拉
   - Keyboard channel 下拉
   - delete this client 行
5. 断言：无 "Lock to specific interface" 下拉框；无 "Probe all interfaces" 开关
```

—— 实操验证需要桌面环境（macOS / Linux X11），STEP-7.5 主体是 grep 静态验证；手动步骤已纳入文档供 release 前最终目视复核。

## 3. 与 PLAN-M1 的偏差 / M1 边界

### 偏差 #N-35：STEP-7.5 工作量为零（按 #N-31 模式退化验证）

**PLAN §7.5 隐含**：GTK 编辑对话框删除 `active_lock` 相关下拉框 + "探测所有接口延迟"开关。

**本步实际**：0 命中。GUI 控件本就不存在 —— 与 PLAN §5 决策 D10（`active_lock` **不引入**）一致；GTK 端的 peer 配置 UI 从未暴露此维度。

**严重程度**：轻（无功能影响；本步承接 #N-31 模式累计的第三例工程纪律）

**PLAN-M1 §3 STEP-7.5 验收**：

| 验收项 | 状态 |
|---|---|
| GTK 客户端编辑对话框删除 `active_lock` 相关下拉框 | ✅ 不存在（grep 0 命中） |
| 删 "锁定到特定接口" 下拉框 | ✅ 不存在（grep 0 命中） |
| 删 "探测所有接口延迟" 开关 | ✅ 不存在（grep 0 命中） |
| `cargo build -p lan-mouse-gtk` 通过 | ✅ |
| 手动 GUI 测试：打开 peer 编辑对话框，断言无 active_lock 控件 | ✅ Gate 5 已纳入文档步骤 |

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

**SUGGESTION #S-22**（"#N-31 模式成内规"）继续累积第三例：
- STEP-7.2 是第一例（25 fixture errors → 全修）
- STEP-7.4 是第二例（active_lock probe_targets live code → 0 命中）
- STEP-7.5 是第三例（GUI active_lock 控件 → 0 命中）

—— 建议 Leader 评审后决定是否在 STEP-7.x 全部完成后统一提升为 AGENTS.md 内规。

## 5. 闸门检查

| 闸 | 结果 |
|---|---|
| **§1 时间门** | ✅ ~10 min（远低于 30 min 目标 —— 0 代码改动路径） |
| **§9 边界门** | ✅ 0 越界 |
| **STEP-7.4 依赖** | ✅ connect.rs `active_lock` 已无残留 |
| **不引入新依赖** | ✅ 0 依赖变更 |
| **不重构**（仅验证） | ✅ 0 行改动 |
| **不动 src/**（不在 GTK 范围违反） | ✅ GTK 范围内 0 行改动 |
| **闸 3 STEP 收尾全套** | ⏸ 跳过（非 STEP-7 末步；STEP-7.6 / 7.7 待续） |

## 6. 遗留 / 风险

### ⚠️ 手动 GUI 目视复核未实操

Gate 5 仅把"打开 peer 编辑对话框"步骤写成文档 —— 实际打开需要桌面环境，STEP-7.5 主体是 grep 静态验证。release 前需 Leader 在带桌面的 macOS / Linux 跑一次目视确认（本步 grep 已经从静态面 100% 证明控件不存在；目视只是双保险）。

### ⚠️ 5 个 lib 单测 fixture 失败（继承自 STEP-7.3 决策：拆 STEP-7.3a）

不在 STEP-7.5 范围（与 GUI 控件删除正交），待 Leader 拆 STEP-7.3a。

### ⚠️ pre-existing clippy / fmt 累计 30+ errors（继承 #S-24 / #S-25）

不在 STEP-7.5 范围，留待 STEP-7.6 / 7.7 收尾时统一评估。

### ⚠️ PLAN §7.5 字面路径 `lan-mouse-gtk/src/ui/client_editor.rs` 仍存在文档偏差

SUGGESTION #S-13 已闭环（建议 Leader 评审后删除），但 PLAN §7.5 / §2 TR-5 / §6 搬运矩阵三处仍写 `client_editor.rs`（plan 文档只读目标，Leader 责任）。本步按实际架构走 grep 不受影响。

## 7. 下一步

**STEP-7.5 已闭环** —— 按 #N-31 模式无代码改动，全部验收命令绿。

按 PLAN-M1 §1 表，下一步为 **STEP-7.6（`firewall.rs` / `service.rs` 头注释清理）**：
- 预期为微改写（`DTLS over UDP` → `QUIC over UDP` 等历史注释更新）
- 前置条件已就绪：STEP-7.4 + 7.5 均闭环；GTK / connect.rs / client.rs / config.toml 均已无 active_lock 残留
- ⚠️ SUGGESTION #S-21 提示 STEP-7.6 验收命令路径 PLAN 字面写 `lan-mouse/src`（不存在），实际是 `src` —— 执行时务必先 `ls -d <path>` 确认路径再下"无残留"结论

**未做 git commit**：本步未改任何文件（0 文件改动）；按 Leader 约定无 commit。

**改动文件清单**：**空**（0 文件、0 行变化）—— #N-31 模式第三例归档。