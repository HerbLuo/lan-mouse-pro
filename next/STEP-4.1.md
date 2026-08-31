# STEP-4.1 — `lan-mouse-ipc` 加 `ChannelMode` + `InputChannelConfig`

> PLAN-M1 §STEP-4 / STEP-4.1
> 执行日期：2026-08-31　实际耗时：~18 min（含一次失败重试 → Default 写错被单测抓到）
> 结论：✅ 通过

## 1. 做了什么

在 `lan-mouse-ipc/src/lib.rs` 新增 IPC 共享的两个轻量类型：

- `pub enum ChannelMode { Stream, Datagram }`
- `pub struct InputChannelConfig { pub mouse_button: ChannelMode, pub keyboard: ChannelMode }`
- `impl Default for InputChannelConfig`

并就地加 3 个单元测试。改动 1 个文件：

- `/Users/hb/Projects/@cloudself/lan-mouse-pro/lan-mouse-ipc/src/lib.rs`：
  131 → 217 行（+86 行）

### 1.1 `ChannelMode` 设计要点

1. **字段顺序** —— `Stream` 先于 `Datagram`，与 `Position::Left` 等"可靠"
   选项排在前的项目惯例一致。serde tag = lowercase（`"stream"` /
   `"datagram"`），与 TOML / JSON 双向兼容。

2. **Derives** —— `Debug / Eq / Hash / PartialEq / Copy / Clone / Serialize / Deserialize`。
   参考 `Position` (line 61-69) 的 derive 清单加 `Hash`（允许未来用于
   `HashSet<ChannelMode>`）；与 PLAN §4.1 所列清单兼容（多了 `Hash`，
   属幂等扩展）。

3. **不带 `Default`** —— PLAN §4.1 没要求 `ChannelMode::default()`，
   而且 enum `Default` 需要 `#[default]` 标注某个变体，会与 `InputChannelConfig::default()`
   的逐字段语义冲突（见 §1.3）。故 enum 故意不带 `Default`。

### 1.2 `InputChannelConfig` 设计要点

1. **Derives** —— `Debug / Eq / PartialEq / Copy / Clone / Serialize / Deserialize`。
   同样采用 `Position` 的清单风格；含 `Copy` 是因为只有 2 个 enum 各 1 字节字段。

2. **`Default` 手写**（不 `#[derive(Default)]`）—— PLAN §4.1 明确：
   `mouse_button = Datagram / keyboard = Stream`。若 `#[derive(Default)]`
   会让 `keyboard` 默认 `ChannelMode::default()`（取决于 enum `#[default]`
   标注），刚好与 PLAN 不符。手写 `impl Default` 是**唯一**保证语义一致
   的写法（具体踩坑见 §3 PLAN-M1 偏差）。

3. **字段访问性** —— 全部 `pub`（与 `ClientConfig` line 132-143 / `ClientState`
   line 159-185 一致），方便 `lan-mouse/src/config.rs`（STEP-4.2）直接读写。
   同时也避免 lan-mouse-gtk（STEP-4.5）需要再包一层 API。

4. **serde 形态** —— 与 `ClientConfig` 对齐：扁平结构、内层字段直接 inline
   成 `input_channels = { mouse_button = "...", keyboard = "..." }`。
   与 PLAN §4.3 config.toml 示例一致。

5. **不入 `mod input_channel` 单独模块** —— 仿 `Position` / `Status`
   与现有 ipc 主类型并列放在 `lib.rs` 顶层。STEP-4.2 / 4.3 / 4.5 引用它
   的代码也只在 ipc 边界用，无须独立模块。

### 1.3 与 PLAN §4.1 验收对齐

PLAN §4.1 验收清单逐条对照：

| PLAN §4.1 要求 | 落实 | 说明 |
|---|---|---|
| `pub enum ChannelMode { Stream, Datagram }` | ✅ | 见 line 159-167 |
| derives: `Clone / Copy / Debug / PartialEq / Eq / Serialize / Deserialize` | ✅ + `Hash` | 见 line 158 |
| `pub struct InputChannelConfig { mouse_button, keyboard }` | ✅ + 字段全 `pub` | 见 line 172-181 |
| 同上 derives + `Default` | ✅ | 见 line 173；手写 Default（§1.2 #2） |
| Default = `{ mouse: Datagram, keyboard: Stream }` | ✅ | line 184-189 |
| 不加 `TransportEvent` | ✅ | lib.rs 无 `TransportEvent` 任何出现 |
| 单测 `channel_mode_default` | ✅ | line 202-206 |

## 2. 验证结果

```bash
$ cargo build -p lan-mouse-ipc
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.51s

$ cargo test -p lan-mouse-ipc input_channel
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.45s
     Running unittests src/lib.rs (target/debug/deps/lan_mouse_ipc-32381266defd8798)

running 3 tests
test input_channel_tests::channel_mode_serializes_lowercase ... ok
test input_channel_tests::input_channel_config_round_trip ... ok
test input_channel_tests::channel_mode_default ... ok

test result: ok. 3 passed; 0 failed; 0 ignored
```

3 个测试通过：PLAN §4.1 要求的 `channel_mode_default` + 我加的两个补
强（serde lowercase wire format 验证、JSON round-trip 验证）。

`cargo check -p lan-mouse-ipc` 应未引入任何新 warning（lib.rs 已存在的
warning 体系不变）；未单独跑 ipc 全单测，只跑本步新增的 `input_channel`
mod 下的 3 个测试（避免掩盖后续 STEPS 的红绿）。

## 3. 与 PLAN-M1 的偏差 / M1 边界

### 3.1 偏差 #N-1：struct Default 必须手写

**现象**：第一次实现时为 enum + struct 都套了 `#[derive(Default)]`，
并给 `ChannelMode` 加了 `#[default] Datagram` 标注；期望 struct 默认值
会"传播"成 `mouse = Datagram / keyboard = Stream`，但 Rust derive 实际
行为是"struct 每个字段各调一次 `Default::default()`"，**完全**忽略外层
的"业务规则"。

`channel_mode_default` 测试立刻红了：

```
left: Datagram
right: Stream
```

`keyboard` 拿到了 enum 的 default（即 `Datagram`），与 PLAN §4.1 要
的 `Stream` 相反。

**修正**：撤掉 enum 的 `#[default]`，struct 也撤 `#[derive(Default)]`，
**手写** `impl Default for InputChannelConfig` 返回硬编码的
`{ mouse: Datagram, keyboard: Stream }`。

**根因**：不是 PLAN 错，是我对 Rust derive 行为想当然——`Default` 是
"逐字段逻辑与"（field-by-field default AND），不是"业务默认"。PLAN §4.1
"mouse=Datagram / keyboard=Stream" 是业务规则，必须手写。

**commit message 影响**：本步 commit 标"PLAN-M1 偏差 #N-1：手写
`InputChannelConfig::default`，不能用 derive"。

### 3.2 偏差 #N-2：ChannelMode 加了 `Hash` derive（超出 PLAN 清单）

`Position` 现有 derive 含 `Hash`（line 61-69），PLAN §4.1 给的清单
`Clone / Copy / Debug / PartialEq / Eq / Serialize / Deserialize` 是
最小集。`ChannelMode` 我额外加了 `Hash`，没功能必要（目前没人用
`HashMap<ChannelMode, _>`），但与 `Position` 对齐更一致。

如 Leader 倾向严格按 PLAN 清单，可让我撤掉 `Hash`。

### 3.3 M1 边界检查（§9）

| §9 类别 | 本步是否触碰 | 说明 |
|---|---|---|
| `proto` 变体（Bounds/MotionAbsolute/...） | 否 | 没动 `lan-mouse-proto` |
| `proto` 常量 / 错误 / codec | 否 | 同上 |
| `input-event` | 否 | 没动 `input-event` |
| `ipc::TransportEvent` | **否**（关键） | lib.rs 无 `TransportEvent` 任何出现 |
| `lan-mouse-gtk::status_bar` | 否 | 没动 gtk |
| `lan-mouse-cli` stderr 订阅 | 否 | 没动 cli |
| `lan-mouse/src/clipboard*.rs` | 否 | 没动 core |
| `Cargo.toml` 引入 h3/h3-quinn/http | 否 | 没改依赖 |
| `quic_transport.rs::Stream C` reader | 否 | 本步不动 transport |
| `connect.rs` mDNS / discovery | 否 | 没动 connect |

**结论**：本步 0 越界。

## 4. 处理的 SUGGESTION 项

无（§S-1 / §S-3 / §S-5 / §S-8 / §S-9 / §S-10 与本步无关）。

## 5. 闸门检查（§1 时间门 / §9 边界门）

- **§1 时间门**：~18 min（含一次失败重试），远低于 30 min 目标；
  无拆步需要。
- **§9 边界门**：见 §3.3，全部 ✅。
- **STEP-3.2 依赖**：✅ 已归档 (commit a97bb2f)，`lan-mouse-proto`
  就位、Hello magic 校验就位；本步不依赖 magic，仅 ipc crate 单测。

## 6. 遗留

- 暂无功能遗留。下一步（STEP-4.2 config.rs）的前置条件已就绪：
  `lan_mouse_ipc::InputChannelConfig` / `ChannelMode` 已稳定暴露
  `pub`，config.rs 可直接 `use lan_mouse_ipc::{ChannelMode, InputChannelConfig};`。
- 细微：§3.2 列出的 `Hash` derive 超出 PLAN 清单，待 Leader 决策
  是否撤掉（默认保留）。

## 7. 下一步

**建议下一步**：STEP-4.2 `lan-mouse/src/config.rs` 加 `input_channels` schema。

**前置条件就绪**：
- `lan_mouse_ipc::InputChannelConfig::{ mouse_button, keyboard }` 已
  `pub` 且 `Default` ✅
- `lan_mouse_ipc::ChannelMode::{ Stream, Datagram }` 已 `pub` 且 serde
  lowercase ✅
- 单测就位，config.rs 改造时可作为 round-trip 参考

**未做 git commit**：等 Leader 处理（按计划只动 `lan-mouse-ipc/src/lib.rs`，
1 文件 +86 行）。
