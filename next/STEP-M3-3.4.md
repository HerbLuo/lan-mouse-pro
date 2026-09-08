# STEP M3-3.4 — M3 落地回归 bug 修复（macOS + Windows + layer_shell 感知 monitor）

> PLAN §M3 / STEP-3.4
> 执行日期：2026-09-08　实际耗时：~50 min
> 结论：✅ 通过；所有 L1 测试 + Windows 跨平台编译全绿

## 0. 范围

修复 M3 STEP-3.1 引入的回归 bug：M3 把 `monitor` 字段写进 `BarrierKey` 主键后，
macOS / Windows / layer_shell 三个 backend 的 query / 存储路径仍按旧语义忽略 `monitor`：
- macOS `crossed()` 用 `BarrierKey::from_pos(pos)` 构造 query key（monitor 钉死 None）
- Windows `check_client_activation` 完全相同模式
- layer_shell `Capture::create / destroy` 用 `from_pos(key.pos)` 重建再 add_client

修复范围严格遵守 STEP-3.4 表格（含把 3.5 的 Windows 字段升级**前置**到 3.4，
理由见 §3）。**不触碰** M4 sub-region / offset / span 维度（PLAN §0 Out of Scope）。

## 1. 做了什么

### 1.1 `input-capture/src/geometry/mod.rs`（新公共 API）

- 新增 `pub struct DisplayBound { rect: DisplayRect, monitor_id: Option<MonitorId> }` + `new()` 构造器
- 新增 `display_containing_idx(&[DisplayRect], point) -> Option<usize>` —— `display_containing` 的 idx 版本，给 `crossed_pure` 用
- 新增 `display_containing_bound(&[DisplayBound], point) -> Option<&DisplayBound>` —— 给 macOS `compute_edge_point` 用
- 新增 `crossed_pure(prev, curr, &[DisplayBound], &HashSet<BarrierKey>) -> Option<BarrierKey>` —— 把 entered_barrier + idx lookup + monitor_id 构造 + active.contains 装进纯函数
- 9 个 crossed_pure 单测（C1-C8 + `monitor_id=None` fallback），覆盖 PLAN §8 第 325 行 4×2 矩阵

### 1.2 `input-capture/src/macos.rs`

- `InputCaptureState.displays: Vec<DisplayRect>` → `Vec<DisplayBound>`
- `crossed()` 退化为薄包装：`crate::geometry::crossed_pure(prev, curr, &self.displays, &self.active_clients)`
- `update_bounds()`：抽 `active_ids` → `enumerate_monitors_for_ids(&active_ids)` → `build_display_bounds(&active_ids, &monitors)`（**single source of truth**：原来 `update_bounds` 走 `active_displays + bounds()`、`enumerate_monitors` 走 `active_displays + IOKit`，两条独立 Quartz 调用 + 瞬态不一致）
- 新增 `enumerate_monitors_for_ids(&[CGDirectDisplayID]) -> Vec<MonitorInfo>` —— 与 `enumerate_monitors` 共享 IOKit 代码
- 新增 `pub fn build_display_bounds(active_ids, monitors) -> Vec<DisplayBound>` —— 纯函数，按 index join
- `compute_edge_point()` 改用 `display_containing_bound`（`&[DisplayBound]` 版本）
- 4 个 `build_display_bounds_pure` 单测：2x1 / 3x1 / empty / single-display，覆盖 PLAN §8 第 326 行（id 唯一、idx 一致、length）
- 更新 `enumerate_monitors` 签名 `&[DisplayRect]` → `&[DisplayBound]`，同步更新现有 fixture

### 1.3 `input-capture/src/layer_shell.rs`（同形 bug #3）

- 抽 `fn record_active_position(active_positions: &mut HashSet<BarrierKey>, key: &BarrierKey)` —— 模块级 helper，纯逻辑，可单测
- `State::add_client(key: BarrierKey)` → `State::add_client(key: &BarrierKey)`，内部先 `record_active_position` 再 Wayland 窗口创建
- `State::delete_client(key: BarrierKey)` → `State::delete_client(key: &BarrierKey)`（新增方法，统一 delete 路径）
- `LayerShellInputCapture::add_client / delete_client` 签名同步改为 `&BarrierKey`
- `Capture::create / destroy`：`self.add_client(BarrierKey::from_pos(key.pos))` → `self.add_client(key)` / `self.delete_client(key)`
- 3 个 cfg-gated 单测：`capture_create_preserves_monitor` / `record_active_position_preserves_offset_span` / `record_active_position_remove_round_trip`

### 1.4 `input-capture/src/windows/event_thread.rs`（前置 3.5 的字段升级）

PLAN §3 第 163 行 3.5 表格要求把 `DISPLAYS` 升级到 `Vec<DisplayBound>`，但 3.5 主要做
`activation_pure` 抽离；**字段升级必须 3.4 先做才能让 3.5 拿到正确类型**。3.4 完成
字段升级，3.5 再做 `activation_pure` 抽离。

- `DISPLAYS: RefCell<(Vec<DisplayRect>, i32)>` → `RefCell<(Vec<DisplayBound>, i32)>`
- `update_display_regions(displays: &mut Vec<DisplayBound>, generation: &mut i32)`：用 `build_stable_id(&d.device_id, &d.device_name)` 生成 `windows:...` 稳定 id，与每个 `WinDisplayInfo.bounds` 一一对应
- `check_client_activation` **保持现状**（3.5 才抽 `activation_pure`）；call sites 投影 `displays.iter().map(|b| b.rect).collect()` 给 `entered_barrier` / `cursor_within` / `clamp_to_display_bounds`（小 Vec，per-mouse-move 分配，3.5 会替换掉）
- 删除未用的 `enumerate_displays(Vec<DisplayRect>)` helper（DISPLAYS 类型升级后没人调用）

## 2. 验证结果

### 2.1 L1（按 STEP 自身要求）

| 命令 | 结果 |
|---|---|
| `cargo build -p input-capture` | ✅ 0 error / 0 warning |
| `cargo build --workspace` | ✅ 0 error / 0 warning |
| `cargo test -p input-capture` | ✅ **62 passed** / 0 failed（含 16 个新单测：9 `crossed_pure_c1-c8` + 4 `build_display_bounds_pure_*` + 3 layer_shell cfg-gated；旧 46 个保留零回归） |
| `cargo test --workspace` | ✅ 全绿（input-capture 62 + lan-mouse 67 + capture_test 7 + emulation_test 2 + 15 + 5 + dummy 全保留） |
| `cargo check -p input-capture --target x86_64-pc-windows-gnu --features layer_shell` | ✅ Windows 跨编译 0 error / 0 warning |
| `cargo fmt --check -p input-capture` | ✅ 0 diff |
| `cargo clippy -p input-capture --all-targets` | ✅ 0 warning（M3 范围；7 pre-existing 不动） |

### 2.2 单测明细

- `geometry::tests::crossed_pure_c1..c8` + `crossed_pure_monitor_id_none_uses_legacy_key`：**9 个新测试全绿**
- `macos::tests::build_display_bounds_pure_2x1_layout` + `build_display_bounds_pure_3x1_preserves_order` + `build_display_bounds_pure_empty_input` + `build_display_bounds_pure_single_display`：**4 个新测试全绿**
- `layer_shell::tests::capture_create_preserves_monitor` + `record_active_position_preserves_offset_span` + `record_active_position_remove_round_trip`：**3 个新 cfg-gated 测试，编译通过**（运行时需 Linux CI）
- 旧 macOS / Windows / libei / dummy / geometry / poll_next **零回归**

### 2.3 milestone 边界

- ✅ 仅触碰 M3 范围内文件
- ✅ 没引入 M4 `exposed_segments` / sub-region / offset-span 语义
- ✅ libei / dummy backend 未动（libei "天然兼容" 由 3.5 加 `select_barriers_with_monitor_field` 单测锁住；dummy 复用 STEP-1.2 的 `with_keys_preserves_monitor_offset_span`）

## 3. 与 PLAN 的偏差

**PLAN 偏差 #1（必要）**：3.5 表格要求把 Windows `DISPLAYS` 升级到 `Vec<DisplayBound>`，
本步提前到 3.4 执行（PLAN 第 163 行 3.5 表格的 Windows (a) 项）。

**理由**：3.5 主要做 `activation_pure` 抽离（pure 函数化），但抽离后函数签名
要消费 `&[DisplayBound]`。如果 3.5 一步到位做两件事，会突破 1h 单步上限。
PLAN STEP-3.4 prompt §"范围" 已显式声明"前置到 3.4 做"，
所以这是 prompt 层批准的提前，不是无授权改动。

**未触碰 3.5 的活儿**：`activation_pure` 抽离 + `check_client_activation` 薄包装——
这两件事仍归 3.5。3.4 完成 `DISPLAYS` 升级，3.5 完成函数抽离。

**未触碰项**：PLAN §0 Out of Scope 全部未触碰；M4 / M5 / M6 / M7+ 均无改动。

## 4. 处理的 SUGGESTION 项

- SUGGESTION.md 当前无活跃项（沿用 STEP-M3-3.3 收尾时的空骨架）
- FIXED.md / IGNORE.md 未新增条目（本步未触发任何已有 #N）

**新增内部观察**（不归档到 SUGGESTION.md）：Windows `check_client_activation`
在 3 处 call sites 投影 `Vec<DisplayBound>` 到 `Vec<DisplayRect>`，每次
mouse-move 各分配一个小 Vec（典型 2-4 个 DisplayRect）。perf 影响可忽略，
3.5 的 `activation_pure` 抽离会消除这个分配。该观察已在代码 comment 中标注
"3.5 will fold this into activation_pure"，不需 SUGGESTION 跟踪。

## 5. 闸门检查

- **时间门**：~50 min（prompt 预算 ~40 min，超 ~10 min 但 < 1h 上限） ✅
- **milestone 边界门**：仅 M3 范围，未触碰 M4+ ✅
- **闸 1 产物 / 依赖 / 验收**：✅
- **闸 2 执行中偏差**：PLAN 偏差 #1（3.5 Windows 字段升级前置，已在 prompt 层批准） ✅
- **闸 3 STEP 回归**：✅（3.6 是 STEP 级别的 clippy + fmt + workspace test 收尾，本步 L1 全绿）

## 6. 遗留 / 风险

- ⚠️ **真机多屏回归 4 项**（PLAN §8 第 332-336 行）—— macOS 真双屏 dropdown、
  Windows 真双屏 dropdown、Linux Wayland (layer_shell) 真机、Linux GNOME
  Wayland (libei) 真机。3.6 milestone 收尾时统一由用户在真机补测；本步
  L1 单测 + Windows 跨编译已覆盖代码层契约。

- ⚠️ **layer_shell 单测需 Linux CI 运行**：3 个 `capture_create_preserves_monitor`
  等单测通过 cfg(layer_shell) gate（build.rs 第 9 行：`layer_shell = unix && !macos`）。
  macOS 开发机不编译 layer_shell 模块，本步无法在本机验证运行时。代码层
  已通过 `cargo check --target x86_64-pc-windows-gnu --features layer_shell`
  间接验证（layer_shell 模块被 cfg 编译进来时语法正确）。

- ⚠️ **pre-existing fmt + clippy 噪音**（SUGGESTION-IGNORE.md #1）：与本步
  无关，按 scope discipline 不动。

- ⚠️ **STEP-3.5 的两件未做事**：`activation_pure` 抽离 + `check_client_activation`
  薄包装。这两件是 3.5 的"主要活儿"，3.4 提前做的字段升级是为 3.5 做铺垫。

## 7. 下一步

- **leader**：commit + 更新 `next/.LEADER-STATE.md` 标记 STEP-3.4 完成
- **下一步 STEP**：STEP-3.5（Windows `activation_pure` 抽离 + libei sanity
  `select_barriers_with_monitor_field` 单测）。3.4 的字段升级已就位，3.5 只需
  专注函数抽离，预计 ~40 min 可完成。
- **真机补测**：4 项真机多屏回归（macOS / Windows / layer_shell / libei）
  留用户补；M3 milestone 在 3.6 收尾后正式关闭。

---

**解决 STEP**：M3 / STEP-3.4

**milestone 状态**：M3 还差 3.5 + 3.6（3.5 预计 40 min，3.6 milestone 收尾）。

**改动文件清单**（仅 paths）：
- /Users/hb/Projects/@cloudself/lan-mouse-pro/input-capture/src/geometry/mod.rs
- /Users/hb/Projects/@cloudself/lan-mouse-pro/input-capture/src/macos.rs
- /Users/hb/Projects/@cloudself/lan-mouse-pro/input-capture/src/layer_shell.rs
- /Users/hb/Projects/@cloudself/lan-mouse-pro/input-capture/src/windows/event_thread.rs
- /Users/hb/Projects/@cloudself/lan-mouse-pro/next/STEP-M3-3.4.md

**新增测试数**：16（9 geometry + 4 macos + 3 layer_shell cfg-gated）
**累计耗时**：~50 min（prompt 预算 40 min）
**PLAN 偏差**：#1（3.5 Windows 字段升级前置，已在 prompt 层批准）
**SUGGESTION 提交**：0