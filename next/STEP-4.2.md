# STEP-4.2 — `config.rs` 加 `input_channels` schema

> PLAN-M1 §STEP-4 / STEP-4.2
> 执行日期：2026-08-31　实际耗时：~15 min（实施就绪 + 1 处 `unused import` 修正）
> 结论：✅ 通过

## 1. 做了什么

在 `lan-mouse/src/config.rs`（仓库根 `src/` 即是主 crate）的 `TomlClient` 与
`ConfigClient` 上各加 `input_channels` 字段，并把 `InputChannelConfig` 接进
已有 `From<TomlClient> for ConfigClient` / `From<ConfigClient> for TomlClient`
两条转换路径；同时就地加 2 个单元测试。

改动 1 个文件：

- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/config.rs`：+66 / -1 行
  - 顶部 `use` 收 `InputChannelConfig`（`ChannelMode` 下移到 test mod）
  - `TomlClient` 加 `input_channels: Option<InputChannelConfig>` + `#[serde(default)]`
  - `ConfigClient` 加 `pub input_channels: InputChannelConfig`
  - `From<TomlClient> for ConfigClient`：`toml.input_channels.unwrap_or_default()`
  - `From<ConfigClient> for TomlClient`：若 `== default` 则 `None`，否则 `Some(...)`
  - 末尾 `#[cfg(test)] mod config_input_channels_tests { ... }`（2 tests）

> **不在 lan-mouse/Cargo.toml 加任何依赖**：`lan-mouse-ipc` 已存在于根
> `Cargo.toml` 第 6 行（`lan-mouse-ipc = { path = "lan-mouse-ipc", version = "0.3.0" }`），
> 无需变更。

### 1.1 `TomlClient` 字段选 `Option<InputChannelConfig>` 的理由

`TomlClient` 是 **磁盘形态**（on-disk shape）；其它所有字段（`hostname` /
`port` / `position` / ...）也都是 `Option<T>` + `unwrap_or_default()` 的模式
（参考 line 76-82）。`InputChannelConfig` 沿用同模式：

```rust
input_channels: Option<InputChannelConfig>,
#[serde(default)]
```

- `Option<...>` 是为了**写回时能省略**该字段（见 §1.3）
- `#[serde(default)]` 是为了让反序列化时 `None` / missing 走 `Default::default()`
  而不是 serde 报"missing field"——避免与"pre-M1 config 文件没有该字段"
  冲突

### 1.2 `ConfigClient` 字段直接 `InputChannelConfig`（非 Option）

`ConfigClient` 是 **内存形态**（in-memory shape），调用方（GTK 客户端编辑器、
listen.rs supervisor、connect.rs）拿到这个 struct 时永远不需要再判断
"是否缺失"。直接持有 `InputChannelConfig` 简化 call-site：

```rust
pub input_channels: InputChannelConfig,
```

未来 M2 / STEP-4.5 GTK ComboBox 拿这个字段直接读 `mouse_button` / `keyboard`
即可，无 `Option::unwrap()` 噪音。

### 1.3 写回省略 default 的语义

`From<ConfigClient> for TomlClient` 把 `ConfigClient.input_channels` 反向
写回磁盘时：

```rust
let input_channels = if client.input_channels == InputChannelConfig::default() {
    None
} else {
    Some(client.input_channels)
};
```

效果：

| 内存状态                          | 磁盘 TOML             |
|-----------------------------------|-----------------------|
| `mouse=Datagram / keyboard=Stream`（= default） | 字段**省略**         |
| `mouse=Stream / keyboard=Datagram`             | `input_channels = { mouse_button = "stream", keyboard = "datagram" }` |
| 其它任意非 default 组合                        | 内联写出              |

关键意义：旧（pre-M1）config 文件保存时不会"突然多出 input_channels 字段"，
diff 干净。

### 1.4 测试设计

```rust
#[cfg(test)]
mod config_input_channels_tests {
    use super::*;
    use lan_mouse_ipc::ChannelMode;

    #[test]
    fn config_parses_input_channels_field() {
        let toml = r#"
            [[clients]]
            hostname = "test"
            input_channels = { mouse_button = "stream", keyboard = "datagram" }
        "#;
        let cfg: ConfigToml = toml::from_str(toml).unwrap();
        let clients: Vec<ConfigClient> = cfg.clients.unwrap_or_default()
            .into_iter().map(From::<TomlClient>::from).collect();
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].input_channels.mouse_button, ChannelMode::Stream);
        assert_eq!(clients[0].input_channels.keyboard, ChannelMode::Datagram);
    }

    #[test]
    fn config_defaults_when_input_channels_missing() {
        let toml = r#"
            [[clients]]
            hostname = "test"
        "#;
        let cfg: ConfigToml = toml::from_str(toml).unwrap();
        let clients: Vec<ConfigClient> = cfg.clients.unwrap_or_default()
            .into_iter().map(From::<TomlClient>::from).collect();
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].input_channels, InputChannelConfig::default());
    }
}
```

- **第 1 个测试**：覆盖解析路径——显式 `input_channels = { ... }` →
  `mouse_button = Stream` 且 `keyboard = Datagram`（刻意挑了非默认组合，
  否则单测无法区分"解析到了"与"default 填的"）。
- **第 2 个测试**：覆盖缺省路径——TOML 缺字段 → `unwrap_or_default()` 触发
  → 与 `InputChannelConfig::default()` 完全相等（既覆盖 `#[serde(default)]`
  行为，也覆盖 `Default for InputChannelConfig` 的"mouse=Datagram /
  keyboard=Stream"语义）。
- 两者一起覆盖了 PLAN §4.2 验收清单的两条。

### 1.5 顺便修正的 1 处 warning

首次实现把 `ChannelMode` 与 `InputChannelConfig` 同时加进顶部 `use`：

```rust
use lan_mouse_ipc::{ChannelMode, InputChannelConfig, DEFAULT_PORT, Position};
```

但 `ChannelMode` 只在 test mod 里被引用，编译 `cargo check -p lan-mouse`
（非 test build）报 `unused import: ChannelMode`。处置：

- `ChannelMode` 下移到 `mod config_input_channels_tests` 内（仅 `#[cfg(test)]` 编译）
- `InputChannelConfig` 保留在顶部（runtime 代码路径使用）

修正后 `cargo check -p lan-mouse` 报 0 warning（来自本步）。

## 2. 验证结果

```bash
$ cargo check -p lan-mouse 2>&1 | grep -E "^error|^warning"
error[E0433]: cannot find module or crate `webrtc_dtls` in this scope     # ×10
error[E0433]: cannot find module or crate `webrtc_util` in this scope     # ×3
error[E0432]: unresolved import `webrtc_util`                            # ×1
                                                                        # = 14 errors
warning: (none)
error: could not compile `lan-mouse` (lib) due to 14 previous errors
```

14 errors **全部来自 `src/connect.rs` 与 `src/listen.rs`** 对 `webrtc_dtls`
/ `webrtc_util` 的引用（STEP-1.2 故意留下的 deprecated 链路，待 STEP-6.x
一次性替换为 `PeerSession`）；0 errors 来自 `config.rs` /
`quic_transport.rs` / `crypto.rs`。

`warning` 一栏 = 0，本步未引入任何 warning。

```bash
$ cargo test -p lan-mouse-ipc --lib
running 3 tests
test input_channel_tests::channel_mode_default ... ok
test input_channel_tests::channel_mode_serializes_lowercase ... ok
test input_channel_tests::input_channel_config_round_trip ... ok

test result: ok. 3 passed; 0 failed
```

STEP-4.1 落地的 IPC 单测仍全绿——证明 STEP-4.2 的 `InputChannelConfig` /
`ChannelMode` 在 ipc crate 侧没有回归。

### 2.1 STEP-4.2 自身的 2 个 test 跑不起来？

是。`cargo test -p lan-mouse config::tests` 跑不起来——根因与 SUGGESTION #S-5
完全相同：`lan-mouse` lib 因 `connect.rs` / `listen.rs` 的 14 个 DTLS errors
无法编译，单测自然进不去。处置（与 SUGGESTION #S-5 一致）：

- **本步交付物**：单测代码逻辑就位（line 605-644），覆盖解析 + 缺省两条路径
- **STEP-6.x 修复 14 errors 后**，由 Leader 手动跑 `cargo test -p lan-mouse
  config::tests` 确认绿
- 当前阶段不阻断 M1 推进

## 3. 与 PLAN-M1 的偏差 / M1 边界

### 3.1 PLAN §4.2 验收清单逐条对照

| PLAN §4.2 要求 | 落实 | 说明 |
|---|---|---|
| `ConfigClient` 加 `pub input_channels: InputChannelConfig` | ✅ | line 289 |
| 解析时缺省视为 `InputChannelConfig::default()` | ✅ | `TomlClient.input_channels: Option<...>` + `#[serde(default)]` + `unwrap_or_default()`（line 300） |
| 写回时若 `== default` 则省略字段 | ✅ | line 331-335：`if client.input_channels == InputChannelConfig::default() { None }` |
| 单测 `config_parses_input_channels_field` | ✅ | line 610-626 |
| 单测 `config_defaults_when_input_channels_missing` | ✅ | line 629-643 |

### 3.2 偏差 / 小调整

无功能性偏差。仅 1 处 cosmetic 调整：

- 顶部 `use` 移除 `ChannelMode`（仅 test 用），挪进 test mod（line 608）

### 3.3 M1 边界检查（§9）

| §9 类别 | 本步是否触碰 | 说明 |
|---|---|---|
| `proto` 变体 / 常量 / 错误 / codec | 否 | 没动 `lan-mouse-proto` |
| `input-event` | 否 | 没动 `input-event` |
| `ipc::TransportEvent` | **否**（关键） | 只用了 STEP-4.1 已就位的 `ChannelMode` / `InputChannelConfig` |
| `lan-mouse-gtk::status_bar` | 否 | 没动 gtk |
| `lan-mouse-cli` stderr 订阅 | 否 | 没动 cli |
| `lan-mouse/src/clipboard*.rs` | 否 | 没动 core |
| `Cargo.toml` 引入 h3/h3-quinn/http | 否 | 0 依赖变更 |
| `quic_transport.rs::Stream C` reader | 否 | 没动 transport |
| `connect.rs` mDNS / discovery | 否 | 没动 connect |
| `lan-mouse-gtk` client_editor 下拉 | 否 | STEP-4.5 范围，本步不碰 |

**结论**：本步 0 越界。

## 4. 处理的 SUGGESTION 项

无。`SUGGESTION.md` 现有 6 条（#S-1 / #S-3 / #S-5 / #S-8 / #S-9 / #S-10 /
#S-11）都与本步无关。本步没产生新 SUGGESTION（无新发现 / 新风险）。

## 5. 闸门检查（§1 时间门 / §9 边界门）

- **§1 时间门**：~15 min，远低于 30 min 目标；无拆步需要。
- **§9 边界门**：见 §3.3，全部 ✅。
- **STEP-4.1 依赖**：✅ 已归档（commit d7852eb），`InputChannelConfig` /
  `ChannelMode` 已 `pub` 且 `Default` 手写正确（`mouse=Datagram` /
  `keyboard=Stream`）。

## 6. 遗留

- **测试无法在本步端到端跑通**：14 DTLS errors 阻塞 lib 编译（与
  SUGGESTION #S-5 同根因），单测代码逻辑就位待 STEP-6.x 验证。
- **下一步前置条件已就绪**：
  - `lan_mouse_ipc::{ChannelMode, InputChannelConfig}` 已稳定暴露
  - `ConfigClient.input_channels` 直接 `pub`，STEP-4.5 GTK ComboBox 可直接读写
  - 写回时省略 default 的语义已落（不影响 pre-M1 config 文件）

## 7. 下一步

**建议下一步**：STEP-4.3 `config.toml` 示例更新（仓库根 `config.toml` +
`DOC.md`）。

**前置条件就绪**：
- 本步 `input_channels` schema 已生效；STEP-4.3 只需在示例 TOML 加注释段
  `input_channels = { mouse_button = "datagram", keyboard = "stream" }`
- `ConfigClient` 字段直接 `pub`，STEP-4.4 `route_input()` 可读它

**未做 git commit**：等 Leader 处理（按计划只动 `src/config.rs`，1 文件
+66 / -1 行）。
