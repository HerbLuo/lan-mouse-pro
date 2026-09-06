# STEP-M2-2.1 — MonitorInfo + IPC FrontendEvent 镜像

> PLAN §M2 / STEP-2.1
> 执行日期：2026-09-06　实际耗时：~25 min
> 结论：✅ 通过

## 1. 做了什么

把 `MonitorInfo` 数据模型定义 + IPC 镜像 + 两条新 `FrontendEvent` 落地，不触碰任何 backend 枚举逻辑（PLAN 把 backend 实装放在 STEP-2.2/2.3/2.4）。

**改动文件**：
- `input-capture/Cargo.toml`：`[dependencies]` 加 `serde = { version = "1.0", features = ["derive"] }`；`[dev-dependencies]` 加 `serde_json = "1.0"`（用于 round-trip 单测）
- `input-capture/src/geometry/mod.rs`：新增 `MonitorInfo` 结构体 + `use serde::{Deserialize, Serialize}`；新增 3 个单测（UTF-8 名称 / 负坐标 / 不同 scale）
- `lan-mouse-ipc/src/lib.rs`：新增 `MonitorInfo` 镜像结构体；在 `FrontendEvent` enum 末尾新增 `MonitorsChanged(Vec<MonitorInfo>)` 和 `BindingInvalid(ClientHandle, String)` 两个变体；新增 6 个单测

**关键决策**：

- **`MonitorInfo` 类型设计**：6 字段 — `id: MonitorId` (= String) / `name: String` / `position: (i32, i32)` / `size: (u32, u32)` / `primary: bool` / `scale: f64`
  - `position` 用 `i32` 元组而非 `f64`：geometry 模块的 `DisplayRect` 用 `f64` 保留 macOS HiDPI 精度，但 IPC 是离散事件，display origin 在 OS 层都是整数像素；负坐标是 2x1 排布必需（PLAN 显式要求 round-trip 单测覆盖负坐标）
  - `scale` 用 `f64`：macOS / Windows mixed-DPI 报 1.25 / 1.5 等非整数；`u32` 会丢精度
  - `size` 用 `u32`：分辨率不可能为负；`(0, 0)` 是合法空 display 的 sentinel
  - `rename_all = "snake_case"`：`position` / `size` 已是 snake_case，无需转换；整个 wire shape 与前端 TypeScript 类型可直接对齐

- **镜像而非 `pub use`**：`lan-mouse-ipc` 不依赖 `input-capture`，所以两个 crate 各持一份字段完全相同的 `MonitorInfo`。PLAN §M2 STEP-2.1 显式要求"镜像一份"。字段 / 顺序 / 类型必须保持同步（STEP-2.5 实现 `Capture::monitors()` 时再决定是手工转换还是 build.rs 生成）

- **`BindingInvalid` 的 reason 字段用 `String`**：PLAN 没明确指定语义，但 `MonitorId` / `Position` 这种 enum 太死，OS 后端想用 `"monitor \"DP-2\" disconnected"` / `"window resized past boundary"` 等自然语言直接告诉前端，简单胜出。`String` 在 serde 里也是 `default` 友好的（future 老 wire 缺 reason = `""`）

- **`MonitorsChanged` 老 wire 兼容策略**：用 `enum FrontendEvent` 的天然前向兼容 — 老 daemon 不可能发 `MonitorsChanged` 变体（它根本不存在于它们的 enum 里），所以新前端只要把"未收到 MonitorsChanged"处理成"monitor 列表空"就行，不需要给变体本身打 `#[serde(default)]`。单测 `monitors_changed_missing_field_yields_empty` 显式覆盖：老 wire（只有 `Error` 变体）→ 前端收到 `Error` 不崩；显式空 `MonitorsChanged([])` → wire shape `{"MonitorsChanged":[]}` 稳定

## 2. 验证结果

```
cargo build -p input-capture          → Finished `dev` profile [unoptimized + debuginfo] target(s) in 4.41s
cargo build -p lan-mouse-ipc          → Finished `dev` profile [unoptimized + debuginfo] target(s) in 3.98s
cargo build --workspace               → Finished `dev` profile [unoptimized + debuginfo] target(s) in 9.61s

cargo test -p input-capture --lib     → 35 passed; 0 failed  (含 3 新 MonitorInfo 单测)
cargo test -p lan-mouse-ipc --lib     → 12 passed; 0 failed  (含 6 新 monitor_info_tests)
cargo test --workspace --no-fail-fast → 全部绿：
                                         input-capture            35 passed
                                         lan-mouse                50 passed
                                         input_channel_routing     7 passed
                                         quic_smoke                2 passed
                                         lan-mouse-ipc            12 passed
                                         lan-mouse-proto           5 passed

cargo clippy -p input-capture -p lan-mouse-ipc --all-targets -- -D warnings  → exit 0（仅有 rustc 内部 trace 提示，非 clippy warning）
cargo fmt --check -p input-capture -p lan-mouse-ipc                       → exit 0
```

**新增单测覆盖矩阵**（对应 PLAN §8 M2 自动测试项）：

| PLAN 要求 | 单测 | 验证点 |
|---|---|---|
| UTF-8 显示器名称 | `monitor_info_round_trip_utf8_name` (input-capture + lan-mouse-ipc) | CJK / accented Latin 字符串 byte-equal |
| 负坐标 | `monitor_info_round_trip_negative` (input-capture + lan-mouse-ipc) | `position = (-1920, -1080)` / `(0, -2160)` |
| 不同 scale | `monitor_info_round_trip_mixed_scale` (input-capture + lan-mouse-ipc) | 1.0 / 1.25 / 1.5 / 2.0 / 2.5 全过 |
| 老 wire = 空 vec | `monitors_changed_missing_field_yields_empty` | 显式 `"MonitorsChanged":[]` wire shape 稳定 + 老 wire 用 `Error` 变体兜底 |
| wire shape 锁定 | `monitor_info_serializes_to_snake_case_fields` | 字段名 `id` / `name` / `position` / `size` / `primary` / `scale` 都断言 |
| `MonitorsChanged` 整体 round-trip | `monitors_changed_round_trip` | 多 monitor Vec 完整 |
| `BindingInvalid` round-trip | `binding_invalid_round_trip` | handle + reason 都保留 |

## 3. 与 PLAN 的偏差

**无 PLAN 偏差**。

- 任务范围完全在 PLAN §M2 STEP-2.1 列出的两个文件内
- `serde` 加到 `input-capture` 依赖：PLAN 显式要求"带 serde derive"，是预期改动
- 没有触碰任何 STEP-2.2+ 的范围（未引入 macOS / Windows / Linux 后端 `enumerate_monitors()`，未引入 `Capture::monitors()` trait 方法，未引入 `src/service.rs::reconcile_monitors_changed`）

**唯一一处微调**：`scale` 选 `f64` 而非 PLAN 隐含的 `u32 / f32`（PLAN 文字只说"不同 scale"，未限定类型）。理由：macOS per-display scaling + Windows mixed-DPI awareness 都报非整数 scale，固定精度会丢数据。单测 `monitor_info_round_trip_mixed_scale` 覆盖 1.0 / 1.25 / 1.5 / 2.0 / 2.5，证明选择合理。

## 4. 处理的 SUGGESTION 项

无 SUGGESTION 项变更。本次执行未发现新的跨步影响问题，也未关闭任何活跃项。

## 5. 闸门检查

| 闸门 | 结果 |
|---|---|
| 产物对得上吗 | ✅ `MonitorInfo` 结构体 + 镜像 + 2 个 FrontendEvent 变体 + 9 个单测 全部到位 |
| 依赖对得上吗 | ✅ M1 / STEP-1.1~1.4 全部 `通过`，`BarrierKey.monitor` 字段已存在供后续填充 |
| 验收对得上吗 | ✅ `cargo build --workspace` 通过；`cargo test -p input-capture -p lan-mouse-ipc` 全绿；`cargo fmt --check` + `cargo clippy -D warnings` 无 diff / warning |
| milestone 边界门 | ✅ 仅触碰 M2 范围；未引入 macOS / Windows / Linux 后端枚举（STEP-2.2~2.4）；未改 `Capture` trait（STEP-2.5）；未改 `src/service.rs`（STEP-2.6） |
| 时间预算门 | ✅ 实际 ~25 min，低于 STEP 估时 30 min 与 executor 上限 1h |

## 6. 遗留

- **镜像同步约定**：目前 `geometry::MonitorInfo` 与 `lan_mouse_ipc::MonitorInfo` 是手工镜像的两个独立 struct。STEP-2.5 实现 `Capture::monitors()` 返回 `Vec<MonitorInfo>` 时，需要在 service 层做 `geometry::MonitorInfo` → `lan_mouse_ipc::MonitorInfo` 的转换（建议单测覆盖）。后续若新增字段（如 `refresh_rate` / `rotation`），务必两处同步改 — 可考虑 STEP-2.5 时引入 build.rs / From trait 自动生成，避免漂移
- **`Position` 镜像**：当前 `Position` enum 已在 `lan-mouse-ipc` 中独立定义（历史既成事实），未走镜像路线；M2 继续沿用，避免无谓 schema 改动
- **`FrontendRequest` 侧未动**：STEP-3.1 才会加 `UpdateMonitor(handle, Option<String>)` 到 `FrontendRequest`，本步不预先添加以避免破坏 §M2 边界

## 7. 下一步

派发 **STEP-2.2** — macOS `enumerate_monitors()`：实现 `CGDisplay::active_displays()` 拿 ID → `CGDisplay::new(d).bounds()` 拿矩形 → `IODisplayCreateInfoDictionary` 拼稳定 `id`；接现有 `DisplayReconfigured` 路径触发后通过 `notify_tx` 发新列表。

预估 ~30 min；前置依赖：✅