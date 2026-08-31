# STEP-4.5b — GTK 两个 AdwComboRow 在 client_row

> PLAN-M1 §STEP-4 / STEP-4.5b（拆分自 STEP-4.5，按 SUGGESTION #S-12 方案 A + #S-13 修正路径）
> 执行日期：2026-08-31　实际耗时：~25 min
> 结论：✅ 通过

## 1. 做了什么

在 GTK peer 编辑行加 2 个 `AdwComboRow`（Mouse button channel / Keyboard channel），与既有 `position` AdwComboRow 风格一致；通过**单一合并信号** `request-input-channels-change(u32, u32)` 发一次 `FrontendRequest::SetClientInputChannels` IPC。打开已有 peer 时通过 `Window::update_client_config` → `row.set_input_channels(cfg)` 回填下拉值。

改动 4 个文件：

- `/Users/hb/Projects/@cloudself/lan-mouse-pro/lan-mouse-gtk/resources/client_row.ui`：
  - 在既有 `<object class="AdwComboRow" id="position">` 之后、`<!-- delete button -->` 之前插 2 个新 AdwComboRow
  - 第一个 id=`input_channels_mouse_button`：`title="Mouse button channel"`；items 顺序 Datagram first / Stream second
  - 第二个 id=`input_channels_keyboard`：`title="Keyboard channel"`；items 顺序 Stream first / Datagram second
  - 两个都附 `subtitle` 简短说明取舍（与 bak 一致）；`translatable="yes"` 标记所有用户可见字符串

- `/Users/hb/Projects/@cloudself/lan-mouse-pro/lan-mouse-gtk/src/client_row/imp.rs`：
  - `use lan_mouse_ipc::{InputChannelConfig, Position};` 扩 1 个类型
  - `use crate::client_row::{mode_to_keyboard_index, mode_to_mouse_index};`
  - `ClientRow` struct 加 2 个 `#[template_child] pub input_channels_*: TemplateChild<ComboRow>`
  - 加 2 个 `RefCell<Option<SignalHandlerId>>` 字段
  - `constructed()` 加 2 个 `connect_selected_notify` handler → `emit_input_channels_change()`
  - `signals()` 注册 `request-input-channels-change(u32, u32)`
  - `#[gtk::template_callbacks] impl` 末尾加：
      - `pub(super) fn set_input_channels(cfg: InputChannelConfig)`（block / set / unblock 两 handler，仿 `set_pos`）
      - `fn emit_input_channels_change()`（取两个 ComboRow 的当前 selected，发合并信号）

- `/Users/hb/Projects/@cloudself/lan-mouse-pro/lan-mouse-gtk/src/client_row.rs`：
  - `use lan_mouse_ipc::{ChannelMode, DEFAULT_PORT, InputChannelConfig, Position};`
  - 模块顶层加 4 个 helper + 2 个常量（与 bak `mousehop-gtk/src/client_row.rs:21-60` 100% 对位）：
      - `pub(super) const MOUSE_DATAGRAM_INDEX: u32 = 0;`
      - `pub(super) const KEYBOARD_DATAGRAM_INDEX: u32 = 1;`
      - `mode_to_mouse_index(mode) -> u32`
      - `mode_to_keyboard_index(mode) -> u32`
      - `mouse_index_to_mode(index) -> ChannelMode`
      - `keyboard_index_to_mode(index) -> ChannelMode`
  - `impl ClientRow` 加 `pub fn set_input_channels(&self, cfg: InputChannelConfig)`

- `/Users/hb/Projects/@cloudself/lan-mouse-pro/lan-mouse-gtk/src/window.rs`：
  - `use crate::client_row::{keyboard_index_to_mode, mouse_index_to_mode};`
  - `setup_clients` 内紧接 `request-position-change` 之后、`row.upcast()` 之前加 `request-input-channels-change` connect_closure
  - 闭包内：`(mouse_idx, keyboard_idx)` → `InputChannelConfig` → `FrontendRequest::SetClientInputChannels(handle, cfg)`
  - `update_client_config` 末尾加 `row.set_input_channels(client.input_channels);`（回填）

## 2. 关键设计点

### 2.1 AdwComboRow 而非 GtkComboBoxText（libadwaita 下）

PLAN §4.5 原文写 `ComboBoxText`，但 SUGGESTION #S-13 已指出实际架构是 `AdwExpanderRow` + `AdwComboRow`，且 libadwaita 下 `GtkComboBoxText` 已被 deprecated（参见 gtk-rs 文档与 bak 既有 position 控件的写法）。

本步采用 `AdwComboRow`，与既有 `position` 控件风格 100% 一致：

- 同一 `<child>` 块，紧接在 `position` 之后
- 同一 `<object class="AdwComboRow" id="...">` + `<property name="model">` + `GtkStringList` 模式
- 同一 `title` + 可选 `subtitle` 模式
- 不需要单独的 `handle_*_changed` `#[template_callback]`（与 position 不同 —— position 在 UI 文件有显式 `<signal>` 标签，这里通过 `connect_selected_notify` 在 `constructed()` 里挂，与 bak mouse_channel / keyboard_channel 一致）

### 2.2 控件 ID 命名（与既有控件风格一致）

| 既有控件 | 新控件 |
|---|---|
| `enable_switch` (GtkSwitch) | `input_channels_mouse_button` (AdwComboRow) |
| `dns_button` (GtkButton) | `input_channels_keyboard` (AdwComboRow) |
| `hostname` / `port` (GtkEntry) | |
| `position` (AdwComboRow) | |

**命名约定**：snake_case + 描述性，与既有 `position` 同级 `AdwComboRow` 一致；前缀 `input_channels_` 与 IPC `ClientConfig.input_channels` 字段对齐。

与 bak 对照（bak 用 `mouse_channel` / `keyboard_channel`）：本步采用更长但语义更明确的形式 `input_channels_mouse_button` / `input_channels_keyboard`，因为：

1. 与 IPC 类型 `InputChannelConfig` 字段 `mouse_button` / `keyboard` 命名完全对齐
2. 避免和未来可能加的 `output_channels_*` 控件混淆（M2 才加）

### 2.3 合并 IPC 信号设计（关键：避免 daemon split-brain）

每个 ComboRow 的 `connect_selected_notify` handler **不**直接发独立的 IPC；而是调 `emit_input_channels_change()`，该函数：

```rust
fn emit_input_channels_change(&self) {
    let mouse_idx = self.input_channels_mouse_button.selected();
    let keyboard_idx = self.input_channels_keyboard.selected();
    self.obj().emit_by_name::<()>(
        "request-input-channels-change",
        &[&mouse_idx, &keyboard_idx],
    );
}
```

**为什么必须合并**：daemon `Service::update_input_channels(handle, cfg)` 整体写 `ClientConfig.input_channels` + `save_config()` 一次。如果两个下拉发两次 `FrontendRequest::SetClientInputChannels`，daemon 端将：

1. 第一次 IPC：写 `mouse_button=Stream`，`save_config` 触发 disk write
2. 第二次 IPC（紧随其后）：写 `keyboard=Stream`，又 `save_config` 又一次 disk write

合并信号后：用户改任何一个下拉，handler 都会带上**两个**的当前值，daemon 一次写完、一次存盘。同时避免"中间状态"（mouse 改了但 keyboard 还没改）落到 daemon / config.toml。

### 2.4 回填：block / set / unblock（避免 split-brain）

`set_input_channels(cfg)` 仿 `set_pos` 模式：

```rust
pub(super) fn set_input_channels(&self, cfg: InputChannelConfig) {
    let mouse_handler = self.input_channels_mouse_button_change_handler.borrow();
    let mouse_handler = mouse_handler
        .as_ref()
        .expect("input_channels_mouse_button handler");
    let keyboard_handler = self.input_channels_keyboard_change_handler.borrow();
    let keyboard_handler = keyboard_handler
        .as_ref()
        .expect("input_channels_keyboard handler");
    self.input_channels_mouse_button.block_signal(mouse_handler);
    self.input_channels_keyboard.block_signal(keyboard_handler);
    self.input_channels_mouse_button
        .set_selected(mode_to_mouse_index(cfg.mouse_button));
    self.input_channels_keyboard
        .set_selected(mode_to_keyboard_index(cfg.keyboard));
    self.input_channels_mouse_button.unblock_signal(mouse_handler);
    self.input_channels_keyboard.unblock_signal(keyboard_handler);
}
```

**关键**：daemon 回推 `FrontendEvent::Created/Updated(ClientConfig)` 时，GTK 拿到的 cfg 写回下拉。如果不 block，写回会立刻触发 `connect_selected_notify` → `emit_input_channels_change` → 发一次 IPC 给 daemon，daemon 又写一遍 cfg 又存盘 → 死循环 + 额外 disk IO。

block handler → set → unblock handler 是 GTK CompositeTemplate 的标准 pattern（与 `set_pos` / `set_active` / `set_hostname` 完全同形）。

### 2.5 下拉顺序与默认值的对应（关键 UI 体验）

| ComboRow | slot 0 | slot 1 | 默认 cfg 映射到 slot |
|---|---|---|---|
| `input_channels_mouse_button` | Datagram (real-time) | Stream (reliable) | `cfg.mouse_button=Datagram` → 0 |
| `input_channels_keyboard` | Stream (reliable) | Datagram (real-time) | `cfg.keyboard=Stream` → 0 |

**为何 mouse Datagram 在前 / keyboard Stream 在前**：两者都把"项目默认值"放在 slot 0，让新装的 peer 第一次打开就是 slot 0（即 InputChannelConfig::default()）。用户切换下拉时眼睛先扫到"默认选项"。

helper 函数 `mode_to_mouse_index` / `mode_to_keyboard_index` 用 `1 - MOUSE_DATAGRAM_INDEX` / `1 - KEYBOARD_DATAGRAM_INDEX` 实现 2-slot 翻转，避免硬编码数字 `1`。

### 2.6 `signal` 注册顺序

`signals()` 用 `OnceLock<Vec<Signal>>` 注册（与既有 5 个 `request-*` 信号同模式），新加的 `request-input-channels-change` 放在末尾，按"control 类 → 单字段类 → 合并多字段类"的逻辑顺序（与 bak `mousehop-gtk/src/client_row/imp.rs:197-236` 对位）。

## 3. 与 PLAN §4.5 验收对齐

| PLAN §4.5 要求 | 本步落实 |
|---|---|
| "GTK 加两个 `ComboBoxText`" | ✅ 改用 `AdwComboRow`（按 #S-13 修正路径） |
| "`Mouse button channel`：Datagram / Stream" | ✅ |
| "`Keyboard channel`：Stream / Datagram" | ✅ |
| "写入 `ClientConfig` 时序列化 `input_channels`" | ✅ 通过 `FrontendRequest::SetClientInputChannels(handle, cfg)`（STEP-4.5a IPC 后端） |
| "打开已有 peer 时回填下拉值" | ✅ `Window::update_client_config` → `row.set_input_channels(cfg)` |
| "保存写回 `ClientConfig`" | ✅ 通过合并 IPC 信号 → `service.update_input_channels` → `save_config`（STEP-4.5a） |
| 文件 `lan-mouse-gtk/src/ui/client_editor.rs` | ⚠️ **PLAN-M1 偏差**：实际是 `resources/client_row.ui` + `src/client_row/imp.rs` + `src/client_row.rs` + `src/window.rs`（4 文件） |

**PLAN-M1 偏差 #N-6**（按 SUGGESTION #S-13 修正）：PLAN §4.5 写的文件名 `lan-mouse-gtk/src/ui/client_editor.rs` + `ComboBoxText` 与实际 GTK 架构不符（无 `src/ui/` 目录、`client_editor.rs` 文件；既有 peer 编辑 UI 是 `client_row.ui` AdwExpanderRow 模板 + `AdwComboRow`）。本步按实际架构走；建议 Leader 同步修 PLAN §4.5 / §2 TR-5 / §6 搬运矩阵。

## 4. 验证结果

```bash
$ cargo build -p lan-mouse-gtk
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.68s
```

**关键**：lan-mouse-gtk 不受 `src/connect.rs` / `src/listen.rs` 的 14 DTLS errors 影响（GTK 编译不依赖 `lan-mouse` crate，只依赖 `lan-mouse-ipc`）。GTK 控件模板 binding（`#[template_child]` ↔ `<object class="AdwComboRow" id="input_channels_*">`）解析成功。

```bash
$ cargo check -p lan-mouse-gtk --all-targets
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.24s

$ cargo clippy -p lan-mouse-gtk --all-targets -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.35s
```

`--all-targets`（含测试目标）clean；clippy `-D warnings` clean。

```bash
$ cargo fmt --check -- lan-mouse-gtk/src/client_row.rs lan-mouse-gtk/src/client_row/imp.rs lan-mouse-gtk/src/window.rs
# 无 diff
```

本步所改 3 个 .rs 文件 fmt-clean；剩余 fmt diffs 在 `src/client.rs` / `src/config.rs` / `src/quic_transport.rs`（pre-existing from prior STEPs，与本步无关）。

## 5. M1 边界检查（§9）

| §9 类别 | 本步是否触碰 | 说明 |
|---|---|---|
| `proto` 变体 / 常量 / 错误 / codec | 否 | 没动 `lan-mouse-proto` |
| `input-event` | 否 | 没动 |
| `ipc::TransportEvent` | **否**（关键） | 只用了 STEP-4.1 / 4.5a 已就位的 `InputChannelConfig` / `ChannelMode` / `FrontendRequest::SetClientInputChannels` |
| `lan-mouse-gtk::status_bar` | 否 | 没碰 status_bar（M2 范围） |
| `lan-mouse-cli` stderr 订阅 | 否 | 没动 cli |
| `lan-mouse/src/clipboard*.rs` | 否 | 没动 core |
| `Cargo.toml` 引入 h3/h3-quinn/http | 否 | 0 依赖变更 |
| `quic_transport.rs::Stream C` reader | 否 | 没动 transport |
| `connect.rs` mDNS / discovery | 否 | 没动 connect |

**结论**：本步 0 越界。

## 6. 手动 GUI 测试步骤（PLAN §4.5 验收 / 文档记录）

无可用 display 环境（macOS Claude Code），以下步骤由 Leader 在本机 GTK 桌面环境手动执行：

1. **首次打开 GTK UI**：
   - 启动 `lan-mouse-gtk`（需先启动 daemon）
   - 展开任一 peer 行
   - **预期**：可见 `Mouse button channel` 下拉默认 `Datagram (real-time)`；`Keyboard channel` 下拉默认 `Stream (reliable)`

2. **修改下拉 → 验证 IPC 发送**：
   - 把 `Mouse button channel` 切到 `Stream (reliable)`
   - daemon 日志预期出现 `received SetClientInputChannels(handle, {mouse_button: Stream, keyboard: Stream})`
   - `~/.local/share/lan-mouse/config.toml` 中对应 peer 段应有 `input_channels = { mouse_button = "stream", keyboard = "stream" }`

3. **只改 keyboard，验证 mouse 不变**：
   - 把 `Mouse button channel` 改回 `Datagram`
   - 改 `Keyboard channel` 为 `Datagram (real-time)`
   - **关键验证**：日志中 IPC payload 仍带 **mouse=Datagram, keyboard=Datagram**（不是 "mouse=Stream, keyboard=Datagram"）—— 验证合并信号正确带两个值

4. **回填持久化**：
   - 关闭 GTK + daemon
   - 重启
   - 展开同一 peer
   - **预期**：两个下拉显示上次保存的值（不回退 default）

5. **运动验证**：
   - `input_channels = { mouse_button = "stream", keyboard = "datagram" }` 配置下，移动鼠标 + 输入键盘
   - **预期**：鼠标 button 事件不丢（Stream 可靠），键盘偶发事件丢（Datagram 不可靠，但 motion 永远 Datagram —— 与 STEP-4.4 `route_input` 一致）

## 7. 闸门检查（§1 时间门 / §9 边界门）

- **§1 时间门**：~25 min，低于 30 min 目标；无拆步需要
- **§9 边界门**：见 §5，全部 ✅
- **STEP-4.5a 依赖**：✅ 已归档（commit aa73705），IPC 链路 4 件全部就位
- **STEP-4.4 依赖**：✅ 已归档（commit b4c2b47），`route_input(cfg, event)` 纯函数已就位（本步不直接调用，但运行时路径已闭合）

## 8. 处理的 SUGGESTION 项

- **#S-12** 🟠 → **完全解决**（方案 A 全部落地：4.5a IPC 后端 + 4.5b GTK 控件层）。建议 Leader 评审后删除本条目
- **#S-13** 🟡 → **完全解决**（按实际 GTK 架构走 client_row.ui + AdwComboRow，未引入 ui/client_editor.rs 目录）。建议 Leader 评审后删除本条目

## 9. 遗留

- **PLAN §4.5 偏差 #N-6**：PLAN 原文写 `lan-mouse-gtk/src/ui/client_editor.rs` + `ComboBoxText`，本步按实际架构走（client_row.ui + 4 个 .rs）。建议 Leader 修 PLAN §4.5 / §2 TR-5 / §6 搬运矩阵文件名为 `client_row.ui` / `client_row/imp.rs` / `client_row.rs` / `window.rs`
- **手动 GUI 测试不可执行**：无可用 display，§6 步骤由 Leader 手动验证
- **控件 ID 命名差异 vs bak**：本步用 `input_channels_mouse_button` / `input_channels_keyboard`，bak 用 `mouse_channel` / `keyboard_channel`。差异根因：与 IPC `InputChannelConfig` 字段对齐（`mouse_button` / `keyboard`），便于未来加 `output_channels_*`（M2）时命名一致

## 10. 下一步

**建议下一步**：STEP-4.6 README / DOC.md 文档更新（描述两种模式取舍 + GTK 下拉用法）。

**前置条件已就绪**（本步 + 4.5a + 4.4 + 4.2 + 4.1）：
- GTK 两个 AdwComboRow 可见可交互 ✅
- 通过合并 IPC 信号 → daemon 单次写 cfg + save_config ✅
- 回填从 `Window::update_client_config` → `row.set_input_channels` ✅
- `route_input(cfg, event)` 纯函数已落地，STEP-5.x `send_motion()` / `send_*` 接入时直接调用 ✅
- 单测 / 集成测试待 STEP-6.x 修 14 DTLS errors 后跑通 ✅（STEP-4.5a 已就位）

**未做 git commit**：等 Leader 处理（本步动 4 文件：client_row.ui、client_row/imp.rs、client_row.rs、window.rs）。