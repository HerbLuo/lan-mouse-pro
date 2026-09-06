# STEP 1.4 — M1 milestone close：fmt + clippy + 单屏 GUI 验证

> PLAN §M1 / STEP-1.4
> 执行日期：2026-09-06　实际耗时：~20 min
> 结论：通过（M1 scope 全绿 + 非 M1 范围 noise 已 IGNORE + 单屏 GUI 留给用户）

---

## 1. 做了什么

### 1.1 AI 自动完成

按 Leader 指示做 surgical scope-discipline 修复 —— **只动 M1 改动文件**（`src/capture.rs`）：

#### 1.1.1 `src/capture.rs` — 4 处 fmt 修复

`rustfmt --edition 2021 src/capture.rs` 一键修复全部 4 处 pre-existing diff（均不在 STEP-1.3 新增逻辑范围内，属机械对齐）：

- **L620** `log::info!("watchdog disabled ...")` 多行合并为单行（适合 100 列宽）
- **L1036** `if self.watchdog.recent_crossings.len() >= self.watchdog_config.crossing_storm_threshold` 拆成单行
- **L1048** `log::warn!("watchdog[heavy]: release_capture ...")` 多行合并为单行
- **L1398** `if let State::Pending { handle, ref key, .. } = self.state` 由单行拆为多行（带 `ref` 借用语法需要续行）

> 注：L1398 在 STEP-1.3 改动的 `State::Pending` destructure 区附近，但**不是** STEP-1.3 引入的 diff——是 pre-existing 风格 + STEP-1.3 加 `ref key` 触发 rustfmt 重排。SUGGESTION #2 提前识别。

#### 1.1.2 `src/capture.rs:797` — clippy `collapsible_match` 修复

**原代码**：

```rust
ProtoEvent::Pong(alive) => {
    if !alive {
        log::info!(
            "Pong(alive=false) from handle {handle}: peer reports \
             emulation disabled (no-op, optimistic-send still in effect)"
        );
    }
}
```

**改为**：

```rust
ProtoEvent::Pong(false) => {
    log::info!(
        "Pong(alive=false) from handle {handle}: peer reports \
         emulation disabled (no-op, optimistic-send still in effect)"
    );
}
```

把 `if !alive` 内层判断合并进外层 match 模式（`Pong(false)` 直接绑定字面值），消除 `clippy::collapsible_match` warning。**语义零差异**（`alive == false` 等价 `!alive`）。

#### 1.1.3 不动非 M1 文件（scope discipline）

按 Leader 指示不动 `quic_transport/*` / `quic_smoke.rs` / `connect.rs` / `input-emulation/src/macos.rs` / `src/config.rs` 的 pre-existing 噪声（24 处 fmt diff + 7 个 clippy warning）。这些已转移到 `next/SUGGESTION-IGNORE.md` #1（理由：QUIC 健壮性单独 PR 处理 + input-emulation macOS 不在本计划 + `config.rs` 属 M3 STEP-3.1 范围）。

### 1.2 人类需完成（在 macOS 真机）

单屏 GUI 端到端回归 —— **AI 无法执行**（TCC 权限 + 物理光标验证）：

1. **单屏 config → activate**：用 `lan-mouse-cli` 或编辑 `~/.config/lan-mouse/config.toml` 把一个对端 client 绑到本机的某一边（top/bottom/left/right 任选一）；通过 GTK UI 或 IPC 触发 activate。
2. **触发 top/bottom/left/right**：在已 activate 的边上移动光标跨越边 → 期望切到对端 + 收到 Enter/Leave/Ack。重复 4 个方向。
3. **release**：触发 release-bind（默认 Shift+Esc，可改）或 service.rs 主动 release，期望光标回本机 + 无 stuck 状态。
4. **与 M0/M1 重构前对比**：以上 4 个方向的触发延迟、跨边后释放位置、键鼠状态追踪应当与重构前像素级一致；M1 改动只内部数据结构升级（`Position → BarrierKey`），UX 不变。

**通过标志**：行为与重构前一致 + 无 panic + 无 stuck modifier + 跨边后光标位置正确。

---

## 2. 验证结果

### 2.1 M1 scope fmt

```sh
$ rustfmt --edition 2021 --check src/capture.rs src/service.rs src/client.rs src/capture_test.rs src/emulation_test.rs
$ cargo fmt -p input-capture -- --check
(均无输出 = 0 diff)
```

5 个 lan-mouse M1 改动文件 + input-capture crate 全 fmt-clean。

### 2.2 M1 scope clippy

```sh
$ cargo clippy -p input-capture --all-targets -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.53s
$ cargo clippy --lib -p lan-mouse -- -D warnings 2>&1 | grep -E '\-\->'
   --> src/connect.rs:727:5
   --> src/connect.rs:728:5
   --> src/quic_transport/endpoint.rs:238:5
   --> src/quic_transport/endpoint.rs:339:1
   --> src/quic_transport/session.rs:760:9
```

**input-capture crate 0 warning**（含 backend 全栈 + 5 个新 dummy 单测）。**lan-mouse crate 0 warning in M1 文件**（capture.rs / service.rs / client.rs / capture_test.rs / emulation_test.rs 全部干净）；剩余 5 个 lib clippy error 全在非 M1 文件（connect.rs ×3 + quic_transport/endpoint.rs ×2 + quic_transport/session.rs ×1 = 7 个 lib + test warning，详见 SUGGESTION-IGNORE.md #1）。

### 2.3 `cargo build --workspace`

```
Finished `dev` profile [unoptimized + debuginfo] target(s) in 4.52s
```

0 error / 0 warning。

### 2.4 `cargo test --workspace`

```
input-capture:    32 passed; 0 failed; 0 ignored
lan-mouse lib:    50 passed; 0 failed; 0 ignored
input-event:       0 passed; 0 failed
lan-mouse-cli:     0 passed; 0 failed
lan-mouse-ipc:     0 passed; 0 failed
lan-mouse-proto:   0 passed; 0 failed
input_channel_routing: 7 passed; 0 failed; 0 ignored
quic_smoke:        1 passed; 1 failed  ← SUGGESTION #3 pre-existing flake (本 STEP 不修)
```

**M1 范围内 89 passed / 0 failed**。唯一 fail 是 STEP-1.3 SUGGESTION #3 识别的 pre-existing QUIC smoke flake（10s 静默期 server-side connection 断言），与 M1 重构无关。

### 2.5 M1 milestone delivery（AI 部分）

- [x] `cargo fmt --check` M1 范围 0 diff
- [x] `cargo clippy -p input-capture --all-targets -- -D warnings` 0 warning
- [x] `cargo clippy --lib -p lan-mouse -- -D warnings` M1 范围 0 warning
- [x] `cargo build --workspace` 0 error
- [x] `cargo test --workspace` M1 范围 89 passed
- [x] 单屏 GUI 端到端验证脚本已就位（详见 §1.2 / §3）

---

## 3. 与 PLAN 的偏差

### 3.1 完成标志 "workspace 全绿" 与 scope discipline 互斥

**PLAN/Leader 完成标志**：
- `cargo fmt --check` workspace 全绿（0 diff）
- `cargo clippy --workspace --all-targets -- -D warnings` 全绿（0 warning）

**Leader 强制流程指示**：
- "如果是 M1 范围改动文件上的 → 必修；如果是 pre-existing 与 M1 无关 → 标注 IGNORE"
- "不动 `quic_smoke` / `quic_transport` 等与 M1 完全无关的 pre-existing 噪音"

**本 STEP 决策**：遵守 scope discipline，**只修 M1 文件**，workspace-wide 全绿需要触碰 `quic_transport/*` / `connect.rs` / `input-emulation/src/macos.rs` / `src/config.rs` —— 这些不在 M1 范围。

**实际偏差**：
- M1 scope：fmt 0 diff + clippy 0 warning ✅
- Workspace scope：fmt 24 处 diff + clippy 7 个 warning ⚠️（全在非 M1 文件，已 IGNORE）

若 Leader 后续决定 workspace-wide 全绿也属于 M1 close 范围，需追加 STEP-1.4a 集中处理 QUIC/config/input-emulation 噪声；本 STEP 不擅自扩大 scope。

### 3.2 fmt 修复工具选择

未使用 `cargo fmt -p lan-mouse`（会顺便改 connect.rs / quic_transport 格式），改用 `rustfmt --edition 2021 src/capture.rs` surgical 应用 —— 严格只动 M1 文件。capture.rs 是唯一 M1 范围有 fmt diff 的文件（STEP-1.3 修改 capture.rs 时 0 新增 diff）。

---

## 4. 处理的 SUGGESTION 项

### 4.1 关闭

- **#2**（STEP-1.3 识别，M1 capture.rs pre-existing fmt + clippy 噪音）→ `SUGGESTION-FIXED.md` #2。
  - fmt 4 处 + clippy `collapsible_match` 1 个全修。

### 4.2 IGNORE（新增）

- **新增 SUGGESTION-IGNORE.md #1**：workspace 余下 24 处 fmt diff + 7 个 clippy warning 全部位于非 M1 文件（QUIC / input-emulation / config），不在 M1 close scope。详细列表见 `next/SUGGESTION-IGNORE.md`。

### 4.3 保持活跃

- **#3**（QUIC smoke flake `connection_survives_ten_seconds_of_silence`）—— 仍 active，未触碰（与 M1 无关，待独立 PR）。

---

## 5. 闸门检查（时间门 / milestone 边界门）

- **时间门**：~20 min ✅（PLAN 估时 20 min，未超时）
- **milestone 边界门**：✅
  - 不触碰 M2 显示器枚举 / M3 绑定 UI / M4 画布
  - 不引入 `monitor / offset / span` 字段语义注入（M1 全程默认值 `None / 0 / 10000`）
  - 不改公共 IPC / CLI / GUI / wire 协议

---

## 6. 遗留

### 6.1 留给人类（macOS 真机）

单屏 GUI 端到端回归（§1.2 4 步：config → activate → 触发 4 边 → release + 行为对比）。AI 无法跑（TCC 权限 + 物理光标验证）。通过标志：与重构前像素级一致。

### 6.2 留给 Leader（milestone close 决策）

1. **是否把 workspace-wide fmt + clippy 全绿纳入 M1 close 范围**：若"是"，追加 STEP-1.4a 处理 QUIC/config/input-emulation 噪声；若"否"，维持当前 scope-discipline 决策（M1 改的文件清白，其余噪声独立 PR）。
2. **Git commit**：按惯例 Leader 提交，commit message 模板 `M1: BarrierKey 数据模型重构` + `归档: next/STEP 1.1.md / 1.2.md / 1.3.md / 1.4.md`。

### 6.3 留给后续 milestone

- M2 启动时（PLAN §M2 STEP-2.1）：`SUGGESTION-IGNORE.md` #1 的 `src/config.rs:613` fmt diff + （若 M2 改 config.rs 时）一并修。
- 独立 QUIC PR（无 milestone）：处理 `quic_transport/*` + `connect.rs` 的 fmt + clippy 噪声 + SUGGESTION #3 的 10s 静默 flake。
- 独立 input-emulation PR：处理 `input-emulation/src/macos.rs` 的 2 处 fmt diff（与本计划无关）。

---

## 7. 下一步

**M1 milestone 完成**（AI 部分 ✅ / 单屏 GUI 验证 ⏳ 用户执行）。

按 PLAN §M2 依赖图：M1 → M2（显示器枚举与稳定 ID + 热插拔），需用户先完成单屏 GUI 验证 + Leader commit M1 后启动。
