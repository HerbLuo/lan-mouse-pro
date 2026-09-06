# STEP-M2-2.2-FIXUP — Validator 反馈修复 (P1 + P2)

> PLAN §M2 / STEP-2.2 validator 返工（基于 commit `35c9e32`）
> validator 报告：`next/STEP-VALIDATION-M2-2.1-2.2.md`
> 触发原因：validator PASS-with-followup，1 P1 BUG 必修 + 3 P2 cosmetic
> 执行日期：2026-09-06　结论：✅ 通过

## 1. 修复对照

| Validator 反馈 | 严重度 | 修复 |
|---|---|---|
| `macos.rs:546-587` `read_display_info` IOKit 全失败时 id 退化为 `"macos:0000:0000:"`（无 `display_id`），破坏 stable id 不变量 | **P1** | 新增 `DisplayInfo::unknown(display_id)` helper，把 `display_id` 注入 `location` 字段（`"unknown-{display_id}"`）；`read_display_info` 两个 early-return 路径全部走 `DisplayInfo::unknown(display_id)` 而非 `DisplayInfo::default()` |
| `macos.rs:518` 注释 `"still unique per display id"` 与实际 id 公式漂移（公式不含 `display_id`） | **P1（注释漂移）** | 重写注释：明确 `read_display_info` 在 fallback 路径会把 `display_id` 拼入 `location` 以保证 id 唯一；引用 STEP-M2-2.2-FIXUP 回归测试名 |
| `geometry/mod.rs:723-751` 单测 `monitor_info_round_trip_utf8_name` docstring 声称覆盖 "CJK + accented Latin"，实际只有 CJK + em-dash | P2 #1 | 测试 name 改为 `"LG UltraFine 5K áéíóú ñ — 戴尔"`（同时含 CJK、accented Latin、em-dash）；docstring 同步重写；追加 4 个 `assert!(json.contains(needle))` 锁死 byte-for-byte 字节保真 |
| `macos.rs` `last_monitors` 字段被 STEP-2.2 §6 文档声称有 `#[allow(dead_code)]`，实际无；3 处 write-only clone | P2 #2 | **删除字段**：去掉 `last_monitors` 字段、3 处 `self.last_monitors = ...` 赋值、`last_monitors: Vec::new()` 初始化；同步更新 `monitors_tx` 字段 doc comment，把"single source of truth"语义写明（`current_monitors()` 直接读 `monitors_tx.borrow()`，无需冗余缓存） |
| `macos.rs::enumerate_monitors(displays: &[DisplayRect])` 参数未用 | P2 #3（**接受不动**） | validator 已接受为 P2 文档偏差，本返工不动 |

## 2. 关键决策

### 修复方案选 (a) 而非 (b)

validator 给两个修复方向：
- (a) `DisplayInfo::unknown(display_id)` 把 `display_id` 注入 `location`
- (b) `enumerate_monitors` 拼 id 前判全零走 fallback 分支

选 (a) 的理由：
1. **关注点分离**：id 唯一性约束落在 `DisplayInfo` 构造点（数据来源）而非消费点（id 拼接），调用链更短、未来 `current_monitors()` / `monitor_changes()` 路径都自动获得正确 id
2. **避免 magic sentinel**：方案 (b) 需要判 `vendor == 0 && product == 0 && serial == "0" && location == "Unknown"`，这套 sentinel 在 macOS 内置屏上恰好匹配（"0"/"Unknown" 是 Apple 真实报告值），可能误判 fallback 路径
3. **happy path wire shape 不变**：方案 (a) 只在 IOKit 失败路径生效，IOKit 成功时 `location` 仍是 OS 报告字符串（如 `"Internal"` / `"External"`），既不影响已有 `serial + location` 拼接逻辑，也避免破坏 macOS 内置屏的真实 id
4. **回归测试可钉**：`DisplayInfo::unknown` 是纯函数，单测可同时验证 (i) 注入 display_id、(ii) 多个失败 display id 互异、(iii) happy path 拼接规则不变

### id 格式

```
happy path:    "macos:{vendor:04x}:{product:04x}:{serial}:{location}"
IOKit fail:    "macos:0000:0000::{empty_serial}:unknown-{display_id_decimal}"
                ⇒ "macos:0000:0000::unknown-4660" (display_id=0x1234)
```

末尾冒号（vendor/product 段后、`serial`/`location` 段前）保留 —— happy path 也有，wire parser 不需区分。

### P2 #2 删除 `last_monitors` 而非 `#[allow(dead_code)]`

validator 给的二选一：
- (a) 加 `#[allow(dead_code)]`
- (b) 删除字段

选 (b) 的理由：
1. watch channel 已是 single source of truth，`monitors_tx.borrow().clone()` 即可拿到当前快照
2. 字段无 reader，3 处 write 都是冗余 clone（典型 1-4 个显示器 = 每次 DisplayReconfigured 多分配 1-4 个 `MonitorInfo`）
3. 减少 InputCaptureState 字段数 = 减少 lock 内持有数据，潜在降低锁粒度
4. STEP-2.5 `Capture::monitors(&self)` 实现直接走 `self.monitor_changes().borrow().clone()`（或调 `current_monitors()`），无障碍

## 3. 验证结果

```
cargo build -p input-capture             → Finished `dev` profile in 4.13s
cargo build --workspace                  → Finished `dev` profile in 3.77s
cargo fmt --check -p input-capture       → exit 0
cargo clippy -p input-capture --all-targets -- -D warnings → exit 0
cargo test -p input-capture --lib        → 46 passed; 0 failed  (含 3 新 DisplayInfo::unknown 单测)
cargo test --workspace --no-fail-fast    → 全部绿：
                                            input-capture       46 passed  (+3 vs STEP-2.2)
                                            lan-mouse           50 passed
                                            input_channel_routing  7 passed
                                            quic_smoke           2 passed
                                            lan-mouse-ipc       12 passed
                                            lan-mouse-proto      5 passed
```

**新增单测覆盖矩阵**（对应 validator §3 BUG 清单）：

| Validator 反馈 | 单测 | 验证点 |
|---|---|---|
| P1 id 唯一性 | `display_info_unknown_encodes_display_id_in_location` | vendor/product/serial 默认 0/空；`location = "unknown-{display_id_decimal}"`；name = None |
| P1 id 唯一性（happy path） | `stable_id_includes_display_id_when_iokit_unavailable_single` | 单 display IOKit 失败时 id 含 `unknown-{display_id}` 段 |
| P1 id 唯一性（实际 P1 场景） | `stable_ids_for_two_simultaneously_failed_displays_are_unique` | 两个 display 同时 IOKit 失败 → id 互异 + 都保留 `macos:0000:0000:` 前缀 |
| P2 #1 UTF-8 覆盖 | `monitor_info_round_trip_utf8_name`（已更新） | name 含 CJK + accented Latin + em-dash，4 个 `assert!(json.contains(needle))` 锁死 |

## 4. 与 PLAN / validator 的偏差

**无 PLAN 偏差**：修复严格在 PLAN §M2 STEP-2.2 列出的同一文件（`input-capture/src/macos.rs` + `input-capture/src/geometry/mod.rs`）内。

**与 validator 偏差**：
- 修复方案选 (a) 而非 (b)（理由见 §2）
- P2 #2 选删除字段而非加 `#[allow(dead_code)]`（理由见 §2）
- 这两处选择均不破坏 validator 报告的 P1/P2 修复目标

## 5. 闸门检查

| 闸门 | 结果 |
|---|---|
| 产物对得上吗 | ✅ `DisplayInfo::unknown` + 2 个 early-return 路径切到新 helper + 注释重写 + `last_monitors` 字段删除 + 3 个新单测 + UTF-8 单测 docstring/name/断言全更新 |
| 依赖对得上吗 | ✅ 全部基于 STEP-2.2 已落地的 `MonitorInfo` / `DisplayInfo` / `build_stable_id`；未引入新 crate / 新模块依赖 |
| 验收对得上吗 | ✅ `cargo build --workspace` / `cargo test --workspace` / `cargo fmt --check -p input-capture` / `cargo clippy -p input-capture --all-targets -- -D warnings` 全绿 |
| milestone 边界门 | ✅ 仅触碰 STEP-2.2 范围内的两个文件；未触碰 STEP-2.3（Windows）/ 2.4（Linux）/ 2.5（Capture trait）/ 2.6（service.rs）任何代码 |
| 时间预算门 | ✅ executor 在 token plan 上限前已完成代码 + 单测；leader 接力跑 fmt + clippy + 验证全绿；总计 < 5 min |

## 6. 遗留

- `monitors_tx` 在 `InputCaptureState`（producer task 持有，Async Mutex 内）+ `MacOSInputCapture`（主线程持有）双持有 —— STEP-2.2 已记录
- `ProducerEvent::MonitorsChanged(Vec<MonitorInfo>)` 变体仍 `#[allow(dead_code)]` —— STEP-2.6 service 层 "手动刷新" 路径将启用
- 真机双屏拔插验证：单屏 + TCC 未授权，`MacOSInputCapture::new()` 走不到 IOKit 路径 —— 用户在 M2 STEP-2.7 收尾时真机跑

## 7. 下一步

提交 `35c9e32` + 本 fixup 的 patch-fixup → leader commit → validator 重新审 STEP-2.1 + 2.2 + fixup → 通过后派发 STEP-2.3（Windows `enumerate_displays()`）。
