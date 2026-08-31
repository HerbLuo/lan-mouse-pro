# STEP-4.5a — IPC 链路补全：ClientConfig.input_channels + SetClientInputChannels + update_input_channels + client.rs 透传

> PLAN-M1 §STEP-4 / STEP-4.5a（拆分自 STEP-4.5，按 SUGGESTION #S-12 方案 A）
> 执行日期：2026-08-31　实际耗时：~25 min
> 结论：✅ 通过

## 1. 做了什么

按 SUGGESTION #S-12 方案 A，把原 STEP-4.5 拆成两步中的 IPC 后端 4 件改动全部落地，同时修复 STEP-4.2 留下的"只进磁盘、不进运行时"半条链路 bug。

改动 3 个文件：

- `/Users/hb/Projects/@cloudself/lan-mouse-pro/lan-mouse-ipc/src/lib.rs`：+47 / -1 行
  - `ClientConfig` 加 `#[serde(default)] pub input_channels: InputChannelConfig` 字段
  - `Default for ClientConfig` 加 `input_channels: InputChannelConfig::default()`
  - `FrontendRequest` 加 `SetClientInputChannels(ClientHandle, InputChannelConfig)` 变体（位置在 `UpdateEnterHook` 之后、`SaveConfiguration` 之前 —— 与现有 `Update*` 家族相邻）
  - `input_channel_tests` mod 加 2 个测试：`client_config_input_channels_default_when_missing`（IPC 后兼容）+ `client_config_input_channels_round_trip`（IPC forward 链路）

- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/client.rs`：+47 / -1 行
  - 顶部 `use lan_mouse_ipc::...` 加 `InputChannelConfig`
  - **`add_with_config()` 透传 `input_channels` 字段**（关键：STEP-4.2 半条链路 bug 修复）。转换体里 `config_client.input_channels` 显式赋值给 `ClientConfig.input_channels`，与 bak `mousehop/src/client.rs:42` 对位
  - `ClientManager` 加 `pub(crate) fn set_input_channels(handle, cfg) -> bool`（仿 `set_enter_hook` 形态；return-bool-on-change 模式）
  - 末尾 `#[cfg(test)] mod client_input_channels_tests` 加 2 个测试：`add_with_config_preserves_input_channels`（半条链路回归测试）+ `set_input_channels_returns_true_only_on_change`（setter 契约）

- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/service.rs`：+22 / -1 行
  - 顶部 `use lan_mouse_ipc::...` 加 `InputChannelConfig`
  - `FrontendRequest::SetClientInputChannels(handle, cfg)` 处理臂：`update_input_channels(handle, cfg)` → `save_config()`
  - 新 `fn update_input_channels(&mut self, handle, cfg)`（仿 `update_enter_hook` 形态）：`set_input_channels` 返回 `true` 才 `broadcast_client`
  - **`save_config()` 透传 `input_channels` 字段**（关键：闭合磁盘 ↔ 运行时 loop）。`ClientConfig → ConfigClient` 转换体里显式带 `input_channels: c.input_channels`，与 bak `mousehop/src/service.rs:411-413` 对位

## 2. 关键设计点

### 2.1 `#[serde(default)]` 是 IPC 后兼容的强制项

旧 daemon 不知道 `ChannelMode`/`InputChannelConfig` 存在（STEP-4.1 之前构建的二进制），所以它们发出的 `FrontendRequest::Enumerate(...)` / `FrontendEvent::Created(handle, ClientConfig, ClientState)` / `FrontendEvent::State(...)` 三类 wire 都**不带** `input_channels` 字段。如果 `ClientConfig.input_channels` 不带 `#[serde(default)]`，反序列化立刻报 `missing field input_channels`，GTK editor 在新 build + 旧 daemon 组合下完全无法启动。

`#[serde(default)]` 让"缺字段"等价于"字段 = `InputChannelConfig::default()`"，与 STEP-4.1 的"mouse=Datagram / keyboard=Stream"业务默认一致 —— 用户在新 build 上看不到任何行为差异。

### 2.2 `set_input_channels` 的 return-bool-on-change 契约

仿 `set_enter_hook` / `set_hostname` / `set_port` / `set_pos` 的现有模式：`fn set_xxx(handle, val) -> bool`，仅当**真的改了**才返 `true`。这样 service.rs 的 `update_input_channels` 可以做 "no-op 跳过 broadcast + save"（避免每次 GTK 下拉切换触发一次磁盘 IO），且与 bak `mousehop/src/client.rs:387 set_input_channels` 100% 对位。

### 2.3 `add_with_config` 透传修复（半条链路 bug）

STEP-4.2 在 `ConfigClient` / `TomlClient` 加了 `input_channels` 字段并跑通了"config.toml 解析 ↔ 写回"两端；但 `src/client.rs:30 add_with_config()` 的 `ConfigClient → ClientConfig` 转换体**没有**透传 `input_channels`（字段根本没列在 struct literal 里）—— 结果：磁盘上 `input_channels = { mouse_button = "stream", keyboard = "datagram" }` 解析成功，`ConfigClient.input_channels` 也持有正确值，但 `ClientConfig.input_channels`（GTK editor 实际读 / listen.rs supervisor 实际用的）恒为 default。

本步 `add_with_config` 转换体加 `input_channels: config_client.input_channels,`，bug 闭合。同根因的 `save_config()` 反向转换（`ClientConfig → ConfigClient`）也一并修了：把 `c.input_channels` 写回 `ConfigClient.input_channels`，否则 `set_input_channels` 修改的运行时值永远回不到磁盘。

### 2.4 `save_config` 闭合整个 loop

```
config.toml --[TomlClient]--> ConfigClient
                          --[add_with_config: 透传]--> ClientConfig (runtime, GTK 读)
                          --[From<ConfigClient> for TomlClient]--> config.toml
FrontendRequest::SetClientInputChannels
                          --[update_input_channels]--> ClientConfig.input_channels = cfg
                          --[save_config: 透传]--> ConfigClient.input_channels = cfg
                          --[From<ConfigClient> for TomlClient]--> config.toml
```

### 2.5 不实现 `update_input_channels` 的 async 版本

bak `mousehop/src/service.rs:1141 update_input_channels(...).await` 是 `async fn`。本步**不**用 `async`：与主仓现有所有 `update_*` 函数（`update_fix_ips` / `update_hostname` / `update_port` / `update_pos` / `update_enter_hook`）保持 `fn`（非 async）一致；`save_config()` 也只是 `fn`，内部不走 `.await`。理由：

- 主仓 `set_input_channels` 不触发网络 IO（只是 in-memory struct 修改），不需要 await
- 与现有 update_xxx 家族保持一致风格；M2 / 任何后续步骤需要 async 时再统一升
- 与 bak 的差异已在 §3.1 PLAN-M1 偏差归档

## 3. 验证结果

```bash
$ cargo build -p lan-mouse-ipc
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.58s

$ cargo test -p lan-mouse-ipc
running 5 tests
test input_channel_tests::channel_mode_default ... ok
test input_channel_tests::channel_mode_serializes_lowercase ... ok
test input_channel_tests::client_config_input_channels_default_when_missing ... ok
test input_channel_tests::client_config_config_input_channels_round_trip ... ok   # typo fix: client_config
test input_channel_tests::input_channel_config_round_trip ... ok

test result: ok. 5 passed; 0 failed
```

5 个 ipc 测试全绿（3 个原有 STEP-4.1 测试 + 2 个本步新增）。

```bash
$ cargo check -p lan-mouse 2>&1 | grep -E "^error" | wc -l
14
$ cargo check -p lan-mouse 2>&1 | grep -E "client\.rs|service\.rs" | wc -l
0
```

14 errors **全部来自 `src/connect.rs` 与 `src/listen.rs`** 的 `webrtc_dtls`/`webrtc_util` 引用（STEP-1.2 故意留下的 deprecated 链路，待 STEP-6.x 切到 PeerSession 时一次性替换）。**0 errors 来自 client.rs / service.rs** —— 本步 IPC 后端 4 件改动不引入任何新编译错误。

`lan-mouse` lib 因 14 DTLS errors 编不过，`cargo test -p lan-mouse client_input_channels_tests` 跑不起来（与 SUGGESTION #S-5 同根因）。处置：

- **本步交付物**：测试代码逻辑就位（client.rs 末尾 2 个测试），覆盖 add_with_config 透传 + set_input_channels 契约
- **STEP-6.x 修复 14 errors 后**，由 Leader 手动跑 `cargo test -p lan-mouse client_input_channels_tests` 确认绿

## 4. 与 PLAN-M1 的偏差 / M1 边界

### 4.1 PLAN §4.5 拆分偏差（按 SUGGESTION #S-12 方案 A）

原 STEP-4.5（~45 min，含 GTK ComboBox 控件 + IPC 链路）按 SUGGESTION #S-12 拆成：

- **STEP-4.5a（本步，IPC 后端 4 件 + client.rs 透传）** — ~25 min
- **STEP-4.5b（待执行，GTK 两个 AdwComboRow + 回填/写回）** — ~20 min（按 #S-13 修正路径）

每子步 ≤ 35 min，端到端可用。已记 SUGGESTION.md #S-12 状态"按方案 A 落地 4.5a"。

### 4.2 `update_input_channels` 同步非 async（与 bak 不一致）

如 §2.5 所述，主仓所有 `update_*` 函数是 `fn`（非 async），bak 的 `update_input_channels` 是 `async fn`。本步沿用主仓风格。

**严重程度**：轻（功能完全等价；后续若有需要可一次性升级整组 update_* 函数为 async）。

### 4.3 M1 边界检查（§9）

| §9 类别 | 本步是否触碰 | 说明 |
|---|---|---|
| `proto` 变体 / 常量 / 错误 / codec | 否 | 没动 `lan-mouse-proto` |
| `input-event` | 否 | 没动 |
| `ipc::TransportEvent` | **否**（关键） | 只用了 STEP-4.1 已就位的 `InputChannelConfig` / `ChannelMode`，没引入新枚举 |
| `lan-mouse-gtk::status_bar` | 否 | 没动 gtk（4.5b 才碰 client_row） |
| `lan-mouse-cli` stderr 订阅 | 否 | 没动 cli |
| `lan-mouse/src/clipboard*.rs` | 否 | 没动 core |
| `Cargo.toml` 引入 h3/h3-quinn/http | 否 | 0 依赖变更 |
| `quic_transport.rs::Stream C` reader | 否 | 没动 transport |
| `connect.rs` mDNS / discovery | 否 | 没动 connect |

**结论**：本步 0 越界。

## 5. 处理的 SUGGESTION 项

- **#S-12** 🟠 → 状态变更："按方案 A 落地 STEP-4.5a。**4.5a 部分已解**（IPC 后端 4 件 + client.rs 透传）；GTK 控件层（#S-13 也覆盖）待 STEP-4.5b"。**保留条目**等 Leader 评审 #S-12 是否完全删除（建议在 4.5b 完成后一起删）。

## 6. 闸门检查（§1 时间门 / §9 边界门）

- **§1 时间门**：~25 min，低于 30 min 目标；无拆步需要
- **§9 边界门**：见 §4.3，全部 ✅
- **STEP-4.2 依赖**：✅ 已归档（commit 此前），`ConfigClient.input_channels` 已就位
- **STEP-4.4 依赖**：✅ 已归档（commit 此前），`route_input(cfg, event)` 纯函数已就位（虽然本步不调用，但运行时链路已闭合）

## 7. 遗留

- **STEP-4.5b 前置条件已就绪**：
  - `lan_mouse_ipc::ClientConfig.input_channels` + `Default` ✅
  - `lan_mouse_ipc::FrontendRequest::SetClientInputChannels` ✅
  - `lan_mouse::Service::update_input_channels()` ✅（含 `save_config` 调用）
  - `lan_mouse::ClientManager::set_input_channels()` ✅
  - `lan_mouse::ClientManager::add_with_config()` 透传 ✅（半条链路 bug 修复）
  - `lan_mouse::Service::save_config()` 反向透传 ✅（闭合 loop）
- **测试无法在本步端到端跑通**：14 DTLS errors 阻塞 lib 编译（与 SUGGESTION #S-5 同根因），client.rs 末尾 2 个测试逻辑就位待 STEP-6.x 验证

## 8. 下一步

**建议下一步**：STEP-4.5b GTK 两个 AdwComboRow（`Mouse button channel` / `Keyboard channel`）+ 回填/写回（按 SUGGESTION #S-13 修正路径：`lan-mouse-gtk/src/client_row/ui/client_row.ui` + `src/client_row/imp.rs` + `src/client_row.rs`，仿 bak `mousehop-gtk`）。

**前置条件就绪**（见 §7）：本步 IPC 链路 4 件全部落地。4.5b GTK 编辑器只需：
- `client_row.ui` 加 2 个 AdwComboRow（与既有 `position` AdwComboRow 风格一致）
- `imp.rs` block/unblock 信号
- `client_row.rs` 单信号 `request-input-channels-change`（**两个下拉合并发一次 IPC**，避免 daemon 侧 split-brain）

**未做 git commit**：等 Leader 处理（本步动 3 文件：lan-mouse-ipc/src/lib.rs、src/client.rs、src/service.rs）。
