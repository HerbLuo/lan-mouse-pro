# STEP M3-3.6 — M3 milestone 收尾 + validator P2 backlog 清理

> PLAN §M3 / STEP-3.6
> 执行日期：2026-09-08　实际耗时：~15 min
> 结论：✅ 通过；L1 全绿（cargo fmt / clippy / test）；3 个 P2 micro-cleanup 全部落地

## 0. 范围

- **L1 收尾**（PLAN §3 第 164 行）：`cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo test --workspace`
- **P2 backlog 清理**（validator STEP-VALIDATION-M3-3.4-3.5 报的 3 个 micro-cleanup，全部 leader 显式指示"3.6 顺手清"）：
  - P2.1：`display_containing_idx` docstring 标题 + C4a test comment 对齐 d1 归属
  - P2.2：libei.rs `pos_to_barrier` dead code
  - P2.3：macos.rs `enumerate_monitors(&[DisplayBound])` 误签名 → 无参

## 1. 做了什么

### 1.1 P2.1 — docstring 标题 + C4 test comment 同步（`input-capture/src/geometry/mod.rs`）

- `display_containing_idx` 文档注释（line 213-216 原内容）：原标题「Mirrors the "edge-seam goes to the left display" convention」与说明文字「belongs to the right display (idx 1)」自相矛盾。
- 改为：「Edge-seam goes to the right display (half-open: left/up inclusive, right/down exclusive)」（与 PLAN §3 第 162 行 3.4 表格 C4 已更新的「归到 display_1（右侧，与半开约定一致）」完全对齐）。
- C4a test comment（line 1095-1106 原内容）：删掉"Wait — actually …"自相矛盾段（"PLAN's C4 wording ("seam goes to d0") reflects an alternative convention"），因为 PLAN §3 第 162 行的 C4 文字已被 planer 任务同步成 d1 归属；旧 comment 引用的"PLAN 写 d0"已不存在。改为：「Mirrors PLAN §3 STEP-3.4 C4: "prev 在接缝 → 归到 d1; active 含 d0 → miss"」。

### 1.2 P2.2 — 删除 libei.rs `pos_to_barrier` dead code（`input-capture/src/libei.rs:82-91`）

- 3.5 把 `pos_to_barrier` 逻辑内联到 `select_barriers` 后，原函数零 caller（`grep -rn pos_to_barrier` 仅返回定义本身）。
- 删整块 10 行函数定义 + 上方 doc comment（line 81-91）。`Region` import 保留——`regions_to_tuples` / `LibeiZoneInfo::from_region` / `Region` zvariant handle 仍需。
- macOS host 不编 libei（cfg gate `target_os != macos`），验证由 macOS 全 L1 通过间接确认（libei module 内的任何语法错误会让 `cargo build --workspace` 失败）。

### 1.3 P2.3 — `enumerate_monitors` 无参签名（`input-capture/src/macos.rs`）

- 函数签名改回 `fn enumerate_monitors() -> Vec<MonitorInfo>`（移除 `&[DisplayBound]` 参数 + `let _ = displays;`）。
- 文档注释移到函数定义上方（替代原 `#[allow(dead_code)]` 的"占位"说明），说明为何用无参签名 + 永远重查 live Quartz 状态。
- 4 处 caller 同步更新（去掉 `&[]` / `&self.displays` / `&res.displays` 占位参数）：
  - line 133（initial seed）: `enumerate_monitors(&res.displays)` → `enumerate_monitors()`
  - line 439（DisplayReconfigured handler）: `enumerate_monitors(&self.displays)` → `enumerate_monitors()`
  - line 1577（`Capture::monitors()` impl）: `enumerate_monitors(&[])` → `enumerate_monitors()`
- 1 处测试同步重写：
  - 测试名：`enumerate_monitors_ignores_displays_slice_and_returns_live_state` → `enumerate_monitors_returns_live_state`（去掉"ignores_displays_slice"维度）
  - 测试逻辑：原"two slice variants return same snapshot" 已无意义（无 slice 参数），改为"two back-to-back calls return identical ids + live snapshot non-empty"，等价地锁住"始终读 live Quartz，不读 `self.displays`"这个核心契约。
  - 删除 `use crate::geometry::{DisplayBound, DisplayRect};` import（测试不再构造 DisplayBound）。

## 2. 验证结果

### 2.1 L1（按 STEP 自身要求）

| 命令 | 结果 |
|---|---|
| `cargo build --workspace` | ✅ 0 error / 0 warning（macOS host；libei cfg-gated 不编） |
| `cargo build -p input-capture` | ✅ 0 error / 0 warning |
| `cargo check -p input-capture --target x86_64-pc-windows-gnu --features layer_shell` | ✅ Windows 跨编译 0 error / 0 warning（libei.rs 不在 Windows target 编译路径内，本步未触及） |
| `cargo test --workspace` | ✅ **164 passed / 0 failed**（input-capture 68 + lan-mouse 67 + capture_test 7 + emulation_test 2 + lan-mouse-vue 15 + lan-mouse-cli 5） |
| `cargo test -p input-capture --lib` | ✅ 68 pass / 0 fail（含 `macos::tests::enumerate_monitors_returns_live_state` + 4 `build_display_bounds_pure_*` + 9 `crossed_pure_*` + 6 `activation_pure_*` + 旧 macOS / Windows / dummy / geometry / poll_next 全部零回归） |
| `cargo clippy -p input-capture --all-targets` | ✅ 0 warning（M3 范围；trace filter warnings 是 tracing crate pre-existing 噪音，与本步无关） |
| `cargo clippy --workspace --all-targets -- -D warnings` | ⚠️ **7 pre-existing errors**（全部在 `src/connect.rs` + `src/quic_transport/*`，与本步无关；详见 LEADER-STATE.md "P2 cosmetic / tech debt backlog"） |
| `cargo fmt --check` (modified files) | ✅ 0 diff（`geometry/mod.rs` / `libei.rs` / `macos.rs` 三件全 fmt-clean；workspace 24 处 pre-existing diff 全部在非 M3 文件——`src/connect.rs` / `src/quic_transport/*` / `input-emulation/src/macos.rs` / `tests/quic_smoke.rs` / `src/config.rs`） |

### 2.2 单测新增 / 修改

- 重命名 + 重写：`macos::tests::enumerate_monitors_returns_live_state`（替代旧 `enumerate_monitors_ignores_displays_slice_and_returns_live_state`）
- 零新单测、零删除单测（test 数量 -0/+1 重命名等价）

### 2.3 Cargo.lock 不变

`git diff --stat Cargo.lock` → 空 diff（修改纯源码，无依赖变化）。

### 2.4 milestone 边界

- ✅ 仅触碰 M3 范围内文件（`geometry/mod.rs` / `libei.rs` / `macos.rs`，全部 M3 修复 STEP 已落地的文件）
- ✅ 未触碰 M4 sub-region / offset-span / exposed_segments（PLAN §0 Out of Scope）
- ✅ 未触碰 `src/service.rs` / `src/capture.rs`（无 PLAN §3 范围内的副作用）
- ✅ PLAN / LEADER-STATE 仅读，无修改（leader 接力 commit 时同步更新）

## 3. 与 PLAN 的偏差

**PLAN 偏差**：0

- P2.1 改 docstring 标题 + 删 C4a test comment 中"PLAN's C4 wording"过时引用 —— leader 在 prompt 中显式指示"3.6 顺手清"，且 PLAN §3 第 162 行文字已被 planer 任务同步为 d1 归属，新 docstring 与 PLAN 一致
- P2.2 删 10 行 dead code —— leader 显式指示"删除 `pos_to_barrier` 函数（line 82-91 整块）"
- P2.3 改函数签名 + 同步 4 caller + 1 test —— leader 显式指示每处细节

## 4. 处理的 SUGGESTION 项

- **SUGGESTION.md**：仍空（沿用 STEP-M3-3.4 收尾时的空骨架；本步未发现新的活跃问题）
- **SUGGESTION-FIXED.md**：未新增条目（3 个 P2 是 validator backlog 项，已在 LEADER-STATE.md "P2 cosmetic / tech debt backlog" 区域跟踪，不走 SUGGESTION 流程）
- **SUGGESTION-IGNORE.md**：未新增条目

## 5. 闸门检查

- **时间门**：~15 min（prompt 预算 ~20 min，未超） ✅
- **milestone 边界门**：仅 M3 范围，未触碰 M4+ ✅
- **闸 1 产物 / 依赖 / 验收**：✅（3 个 P2 目标 + L1 三命令全部通过）
- **闸 2 执行中偏差**：0（所有改动在 leader prompt 显式批准范围内）
- **闸 3 STEP 回归**：✅（M3 milestone 收尾 L1 全绿；输入捕获 68 + 主仓 67 + 其它 29 全保留零回归）

## 6. 遗留 / 风险

- ⚠️ **真机多屏回归 4 项**（PLAN §8 第 332-336 行）—— macOS 真双屏 dropdown、Windows 真双屏 dropdown、Linux Wayland (layer_shell) 真机、Linux GNOME Wayland (libei) 真机。3.6 milestone 收尾统一由用户在真机补测；本步 L1 单测 + Windows 跨编译已覆盖代码层契约。

- ⚠️ **libei 单测本机未跑**（`target_os = "linux"` cfg gate）—— 与 STEP-3.4/3.5 同样的限制：libei module 整个文件 cfg-gated 不在 macOS 编译路径。本步只删了 dead code（`pos_to_barrier` 零 caller 已 grep 确认），无逻辑变化；删除后 `libei.rs` 行数从 `1011+` 减到 `1001-`，后续 Linux 真机编译时只会更短不会更复杂。

- ⚠️ **pre-existing fmt + clippy 噪音**（SUGGESTION-IGNORE.md #1）：24 处 fmt diff + 7 clippy error 全部在非 M3 文件（`src/connect.rs` / `src/quic_transport/*` / `input-emulation/src/macos.rs`），按 scope discipline 不动。

## 7. 下一步

- **leader**：commit + 更新 `next/.LEADER-STATE.md` 标记 STEP-3.6 完成 + P2 backlog 清空
- **用户**：真机回归 4 项（macOS / Windows / Linux Wayland layer_shell / Linux GNOME libei）+ 旧 config 单 monitor 兼容回归
- **用户决策**：M3 真完成 → 是否启动 M4（暴露边段 + 画布编辑器，~9h）

---

**解决 STEP**：M3 / STEP-3.6 + validator P2 backlog 3 项（#P2.1 docstring / #P2.2 dead code / #P2.3 误签名）

**milestone 状态**：M3 收尾完成（3.4 + 3.5 + 3.6 三步 + P2 backlog 全清）；等用户真机回归 + M4 启动决策

**改动文件清单**（仅 paths）：
- /Users/hb/Projects/@cloudself/lan-mouse-pro/input-capture/src/geometry/mod.rs
- /Users/hb/Projects/@cloudself/lan-mouse-pro/input-capture/src/libei.rs
- /Users/hb/Projects/@cloudself/lan-mouse-pro/input-capture/src/macos.rs
- /Users/hb/Projects/@cloudself/lan-mouse-pro/next/STEP-M3-3.6.md（本文件）

**新增 / 修改单测数**：0 新增；1 改名（`enumerate_monitors_ignores_displays_slice_and_returns_live_state` → `enumerate_monitors_returns_live_state`）
**累计耗时**：~15 min（prompt 预算 20 min）
**PLAN 偏差**：0
**SUGGESTION 提交**：0
**validator P2 backlog 清理**：3 / 3 完成
