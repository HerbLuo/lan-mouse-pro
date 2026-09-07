# STEP M3-3.1 — 后端链路（ClientConfig.monitor + UpdateMonitor）

> PLAN §M3 / STEP-3.1
> 执行日期：2026-09-07　实际耗时：~40 min
> 结论：✅ M3.STEP-3.1 通过（12 个新单测全绿 / 0 build error / 0 新 clippy / 0 新 fmt diff）

## 1. 做了什么

按 PLAN §M3 STEP-3.1 落地后端链路：`ClientConfig.monitor` + `FrontendRequest::UpdateMonitor` + TOML `monitor` 字段 + `service.update_monitor` + CLI `SetMonitor`。**未触碰** Vue 端（M3.STEP-3.2 范围）、Capture trait / 5 backend（已在 M2 把 `monitor` 字段加入 `BarrierKey`）、quic 协议层。

### 改动文件清单

- **`lan-mouse-ipc/src/lib.rs`**:
  - `ClientConfig`: 新增 `pub monitor: Option<String>`（`#[serde(default)]`），沿用 `input_channels` 的 back-compat 范式
  - `ClientConfig::default()`: 加入 `monitor: None`
  - `FrontendRequest`: 新增 `UpdateMonitor(ClientHandle, Option<String>)` 变体
  - 新增 3 个单测（`client_config_monitor_default_when_missing` / `client_config_monitor_round_trip` / `client_config_monitor_none_serializes_as_null`）

- **`src/config.rs`**:
  - `TomlClient`: 新增 `monitor: Option<String>`（`#[serde(default)]`，缺字段 = None）
  - `ConfigClient`: 新增 `pub monitor: Option<String>`
  - `From<TomlClient> for ConfigClient`: 透传 `toml.monitor`（缺字段已为 None）
  - `From<ConfigClient> for TomlClient`: 透传 `client.monitor`（保留 None 时不写入的"省略默认值"约定）
  - 新增 4 个单测（`config_parses_monitor_field` / `config_defaults_when_monitor_missing` / `config_omits_monitor_field_when_none_on_writeback` / `config_keeps_monitor_field_when_some_on_writeback`）

- **`src/client.rs`**:
  - `ClientManager::add_with_config`: 透传 `monitor` 字段（沿用 `input_channels` 模式）
  - `ClientManager::client_at`: 同步 M2 的 BarrierKey 维度 — 同时比对 `c.pos` 和 `c.monitor`（之前只比 `pos`），保证同 pos 不同 monitor 的两个 active client 不互相 deactivate
  - `ClientManager::get_key`: 改写为直接构造 `BarrierKey { pos, monitor: c.monitor.clone(), offset: 0, span: 10000 }`（之前用 `BarrierKey::from_pos(pos)` 把 monitor 字段钉死为 None）
  - `ClientManager::set_monitor`: 新增 setter — 返回 `s.active`（沿用 `set_pos` 范式）作为"`deactivate + activate` round-trip 是否需要执行"的信号
  - 新增 5 个单测（`add_with_config_preserves_monitor` / `add_with_config_defaults_monitor_to_none` / `set_monitor_returns_active_when_value_changed` / `get_key_includes_monitor_binding` / `client_at_scopes_by_pos_and_monitor`）

- **`src/service.rs`**:
  - `FrontendRequest::UpdateMonitor` handler: `self.update_monitor(handle, monitor); self.save_config();`
  - `Service::update_monitor`: 复用 `update_pos` 的范式 — `set_monitor` 返回 true 时 `deactivate_client + activate_client` 重建 barrier；否则只 `broadcast_client`
  - `Service::save_config`: `ConfigClient` mapping 补 `monitor: c.monitor` 字段

- **`lan-mouse-cli/src/lib.rs`**:
  - 新增 `CliSubcommand::SetMonitor { id: ClientHandle, monitor: String }` 子命令
  - 约定：空字符串 `""` = 清空 binding（→ `None`）；非空字符串 = `Some(id)` 透传到 `FrontendRequest::UpdateMonitor`

## 2. 验证结果

### 2.1 `cargo build --workspace`

```
cargo build --workspace  →  Finished `dev` profile [unoptimized + debuginfo] target(s) in 12.08s
```

零编译错误。`lan-mouse-ipc` / `lan-mouse-cli` / `lan-mouse` 三个 crate 全部干净。

### 2.2 `cargo test --workspace --no-fail-fast`

```
input-capture            47 passed; 0 failed   (与 M2 持平)
lan-mouse                67 passed; 0 failed   (vs M2: 58 → 67，+9: 5 client.rs + 4 config.rs 新单测)
input_channel_routing     7 passed; 0 failed   (与 M2 持平)
quic_smoke                2 passed; 0 failed   (与 M2 持平；connection_survives_ten_seconds_of_silence 11.02s 通过)
lan-mouse-ipc            15 passed; 0 failed   (vs M2: 12 → 15，+3: client_config_monitor_* 三个单测)
lan-mouse-proto           5 passed; 0 failed   (与 M2 持平)
─────────────────────────────────────────────
合计                     143 passed; 0 failed   (vs M2: 131 → 143，+12 新单测全绿)
```

### 2.3 `cargo clippy -p lan-mouse-ipc -p lan-mouse-cli -p lan-mouse --all-targets --no-deps`

7 个 warning，**全部** pre-existing（在 SUGGESTION-IGNORE.md #1 backlog 内），**0 个新 warning**：

```
src/connect.rs:727, 728            doc_lazy_continuation (2)
src/connect.rs:1246, 1252          assertions_on_constants (2)
src/quic_transport/endpoint.rs:238 doc_lazy_continuation
src/quic_transport/endpoint.rs:339 too_many_arguments (dial_any 8/7)
src/quic_transport/session.rs:760  doc_lazy_continuation
```

与 M2 收尾（STEP-M2-2.7 §2.3）完全一致。M3 范围（service.rs / client.rs / config.rs / ipc/lib.rs / cli/lib.rs）**0 warning**。

### 2.4 `cargo fmt --check`（仅 M3 改动文件）

`rustfmt --edition 2021 --check` 直接检查 5 个改动文件 → 0 diff，唯一命中是 `src/config.rs:645`（pre-existing `WatchdogConfig` 字段对齐，属 SUGGESTION-IGNORE.md #1 backlog，与 M3 无关）。

### 2.5 测试矩阵 §M3 STEP-3.1 完成标志逐条核对

| PLAN 完成标志 | 对应验证 |
|---|---|
| 旧 config 仍能反序列化 | `config_defaults_when_monitor_missing` + `client_config_monitor_default_when_missing`（TOML + IPC 两侧都验证） ✅ |
| 新字段可选 | `TomlClient.monitor` 和 `ClientConfig.monitor` 均 `#[serde(default)]` + 单测 ✅ |
| 单测：改 monitor 后 BarrierKey 实际变更 | `get_key_includes_monitor_binding`（None → Some → Some → None 四态对照） + `client_at_scopes_by_pos_and_monitor`（3 客户端同 pos 不同 monitor 不互冲） ✅ |
| TOML 保存 None 不写入 | `config_omits_monitor_field_when_none_on_writeback` ✅ |
| TOML 保存 Some(...) 落盘 | `config_keeps_monitor_field_when_some_on_writeback` ✅ |
| CLI 子命令 | `CliSubcommand::SetMonitor` + execute 分支（空字符串 → None 约定） ✅ |

## 3. 与 PLAN 的偏差

**0 PLAN 偏差**。所有完成标志全部达成；测试矩阵逐条覆盖；未触碰 §M3 STEP-3.1 范围外内容（Vue 端 / Capture backend / 协议层均未动）。

**1 处隐含范式选择**（不算偏差，记入代码 docstring 即可）：

- `ClientManager::set_monitor` 选择 **返回 `s.active`**（沿用 `set_pos` 范式），**不**是 `set_input_channels` 范式（返回 `true` on change）。
- 原因：`update_monitor` in `service.rs` 需要做 `deactivate + activate` round-trip — 若 client 是 inactive，**绝对不能**自动 activate（用户可能只想 pre-bind 而不开启），所以需要 `s.active` 作为"是否触发 round-trip"的信号。
- `set_input_channels` 不需要 round-trip（`input_channels` 不影响 capture barrier），所以它返回 `true` on change 是合适的。
- 这两种范式共存于 `ClientManager`，新读者可能困惑；我在 `set_monitor` 的 docstring 里专门点了 "mirrors `set_pos` exactly" 并解释为什么不像 `set_input_channels`。

## 4. 处理的 SUGGESTION 项

- **未新增 SUGGESTION 条目**（无流程性问题、无 milestone 越界、无单步小问题）
- **未移动任何 SUGGESTION 条目**到 FIXED / IGNORE（FIXED.md 的 #1/#2/#3/#4 都是历史 M1 / quic_smoke flake 修复 + M2.6 Vue 范围扩展，与本 STEP 无关；IGNORE.md #1 是 pre-existing fmt/clippy backlog，本 STEP 验证确认仍未触及）
- **SUGGESTION.md 仍为 "(当前无活跃项)"**

## 5. 闸门检查

| 闸门 | 结果 |
|---|---|
| 产物对得上吗 | ✅ 5 个改动文件全部按 PLAN 完成标志落地；12 个新单测 + 3 个 IPC 单测覆盖 back-compat 矩阵 |
| 依赖对得上吗 | ✅ M0 (b38cb6f/ab0e076/2841d54) + M1 (08cb3a3) + M2 (61ff247/35c9e32/1bfaa45/3f9f560/60025ff/74949d4/63706b5/dc7e747/d08218d) 全部 `通过`；`BarrierKey { pos, monitor, offset, span }` 数据模型已在 M1 落地（M3.1 只取 `monitor` 字段，offset/span 维持默认 0/10000） |
| 验收对得上吗 | ✅ `cargo build --workspace` 干净；`cargo test --workspace` 143/0；`cargo clippy` M3 范围 0 warning；fmt M3 范围 0 diff |
| **milestone 边界门** | ✅ **未触碰** Vue（lan-mouse-vue/）、Capture trait / backend、quic 协议层、M4 exposed_segments 范围；只动 Rust 后端 + CLI |
| 时间预算门 | ✅ 实际 ~40 min（PLAN 估时 30 min；超出 10 min 主要花在 `set_monitor` 返回值范式选择 + test fixture 调整）；远低于 executor 1h 上限 |

## 6. 遗留

无遗留（不影响 M3 推进）。

**非阻塞观察**（不写 SUGGESTION，记录在 STEP 文档即可）：
- `ClientManager` 现存在两种 `set_*` 返回值范式（`set_pos` / `set_monitor` 返回 `s.active`；`set_input_channels` 返回 `true` on change）。两种范式都是正确的，docstring 已说明，但读 `client.rs` 的人需要分清场景。
- CLI `SetMonitor` 用空字符串 `""` 代表 `None`（vs `SetHost` 用的 `Option<String>` 直接 clap 表达）。CLI 范式差异，无功能性影响。

## 7. 下一步

派发 **M3.STEP-3.2 — 前端链路**：Vue `api/ipc.ts` 加 `MonitorInfo` 类型 + `MonitorsChanged` / `UpdateMonitor` 类型；Vue `store/index.ts` 维护 `state.monitors: MonitorInfo[]`，`onMounted` 监听 `MonitorsChanged`，`updateClientConfig` 加 monitor 字段 diff/send + type guard；`components/ConnectionRow.vue` 在 position `<select>` 后面加 monitor `<select>`。完成后再派 **STEP-3.3 收尾**（fmt/clippy/build + 真机回归）。

预估 STEP-3.2 ~30 min（沿用 STEP-2.6 Vue 范围扩展经验；如再次触发 SUGGESTION 范围扩展，按 leader 接受路径处理）。

## 8. 闸 1 / 闸 2 / 闸 3 状态

| 闸 | 结果 |
|---|---|
| 闸 1 产物/依赖/验收/边界 | ✅ 全部通过 |
| 闸 2 执行中偏差 | 0（1 处隐含范式选择不算 PLAN 偏差，记入 docstring） |
| 闸 3 milestone 收尾 | ⏸ 跳过（M3 收尾在 STEP-3.3） |
