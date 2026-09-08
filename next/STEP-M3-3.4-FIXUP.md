# STEP M3-3.4-FIXUP — H1 修复（query_pure legacy fallback）

> 续 STEP-M3-3.4（M3 落地回归 bug 修复）/ 触发于 STEP-DEBUG-M3-BARRIER-CHAIN §3 H1
> 执行日期：2026-09-08　实际耗时：~15 min
> 结论：✅ 通过；H1 fallback 应用 + 新单测锁住 + 165 测试全绿

## 0. 触发与范围

STEP-DEBUG-M3-BARRIER-CHAIN（"macOS + Linux GNOME Wayland libei 真机双屏测试，鼠标移到边缘主控和被控端日志都没有 barrier 触发"）报告 §3 H1 找到根因：

- `query_pure` 构造查询 key 用 `displays[idx].monitor_id`（生产路径 100% `Some(...)`）
- 客户端默认 `ClientConfig.monitor = None`，active 里塞的是 `{ pos: Top, monitor: None, ... }`
- `Some(...)` 永远 != `None` → `clients.contains(&key)` 永远 false → `crossed_pure` 永远 None → 整条 barrier 链断
- macOS / Windows 同病（共用 `query_pure` helper）；layer_shell / libei 不受影响（前者直接喂 HashSet，后者 EIS 直接喂整 vec）

**仅本步修复范围**（per leader prompt §"不越界"）：
- 改 `geometry/mod.rs::query_pure`：加 legacy fallback
- 加 1 个 production-realistic 单测锁住 fallback
- **未触碰** `macos.rs` / `windows/event_thread.rs` / `layer_shell.rs` / `libei.rs` / `service.rs` / `capture.rs`

## 1. 做了什么

### 1.1 `query_pure` 加 legacy fallback

文件：`input-capture/src/geometry/mod.rs` line 404-454

原代码（pre-H1）：

```rust
let key = BarrierKey {
    pos,
    monitor: displays[idx].monitor_id.clone(),  // 永远 Some(...)
    offset: 0,
    span: 10000,
};
if clients.contains(&key) {
    Some(key)
} else {
    None
}
```

post-H1：

```rust
let key = BarrierKey {
    pos,
    monitor: displays[idx].monitor_id.clone(),
    offset: 0,
    span: 10000,
};
if clients.contains(&key) {
    return Some(key);
}
// H1 legacy fallback: specific_key miss + display has Some(monitor_id)
// → probe legacy `monitor: None` entry for the same pos.
if key.monitor.is_some() {
    let legacy_key = BarrierKey { monitor: None, ..key };
    if clients.contains(&legacy_key) {
        return Some(legacy_key);
    }
}
None
```

**关键不变量（per STEP-DEBUG-M3-BARRIER-CHAIN §3 H1 #1-3）**：

1. **M3 specific 优先**：`c.monitor = Some(id)` 且 `id == displays[idx].monitor_id` → 命中 specific key（短路在前，未变）
2. **Legacy None 后备**：`c.monitor = None` → 命中 legacy key（新增 H1 fallback）
3. **不变量 #3 仍保**：`c.monitor = Some(X)` + cursor on Y + active 无 `None` entry → 仍 None（invariant #3 from debug report：explicit binding 永不"降级"到 legacy 在错的 monitor 上）

**额外守卫**：`if key.monitor.is_some()` 守护 — 当 containing display 自身 `monitor_id: None`（pre-fix fast path 已经命中 `contains(&key)`，因为 key.monitor 也 None），不付第二次 HashSet probe 的代价。

### 1.2 新增 production-realistic 单测

文件：`input-capture/src/geometry/mod.rs` `tests` 模块

```rust
#[test]
fn query_pure_falls_back_to_legacy_when_active_has_monitor_none() {
    // Production-realistic fixture: 两个 display 都带 Some(macos:...)，
    // 完全复刻 build_display_bounds 的生产行为。
    let displays = layout_2x1_bound();
    let mut active = HashSet::new();
    // Production-realistic client: legacy config, monitor 未选
    active.insert(BarrierKey {
        pos: Position::Top,
        monitor: None, offset: 0, span: 10000,
    });
    let got = query_pure((500.0, 500.0), (500.0, -2.0), &displays, &active);
    assert_eq!(got, Some(BarrierKey {
        pos: Position::Top, monitor: None, offset: 0, span: 10000,
    }));
}
```

直接调 `query_pure`（私有 helper，测试模块同 module 可访问）— 把 fallback 路径锁在 helper 层。

### 1.3 **PLAN 偏差 #1（必要）**：更新 C3 + W3 断言

leader prompt 完成标准列了"现有 9 `crossed_pure_c1-c8` + 6 `activation_pure_w1-w6` 必须全绿"，但 H1 修复会让 `crossed_pure_c3_display0_top_misses_legacy_active`（line 1081-1093）和 `activation_pure_w3_display0_top_misses_legacy_clients`（line 1381-1393）变红——这两个测试用的就是 production-realistic fixture（displays `Some(...)` + active `None`），原断言 `None` 就是 H1 修复要翻转的行为。

按 leader 失败兜底约定"单测挂 → 自行解决"，最务实做法是：

| 测试 | 改动 |
|---|---|
| `crossed_pure_c3_display0_top_misses_legacy_active` | assertion 从 `assert_eq!(got, None)` → `assert_eq!(got, Some(legacy_key))`；docstring 改写为"post-H1"叙述（含不变量 #3 说明）；test 名字保留（C1-C8 matrix index 不变） |
| `activation_pure_w3_display0_top_misses_legacy_clients` | 同上（Windows 版） |
| `crossed_pure` docstring（line 475-476） | "monitor_id == None on containing display → legacy lookup" 改为描述 post-H1 两条路径：fast path + legacy fallback |
| C1-C8 矩阵注释（line 988-1007） | C3 行从 `monitor: None, pos: Top + d1's Top key \| miss` → `monitor: None, pos: Top (legacy only) \| hit (legacy)` |
| W1-W6 矩阵注释（line 1296-1304） | W3 行从 `monitor: None, Top \| miss` → `monitor: None, Top (legacy) \| hit (legacy)` |

**为何保留测试名**：测试名 `..._misses_legacy_active` 是 STEP-3.4 时代的产物（当时该 fixture 断言 buggy 行为），但 C1-C8 矩阵索引在 module docstring 里大量引用这个名字，rename 会引入连锁改动且无功能价值。Docstring 里已说明 name 是 "STEP-3.4-era artifact, kept for backwards compat with C1-C8 matrix indexing"。

**为何不能"不动"这两个测试**：H1 修复按 leader 显式代码是 fix（不能让 fallback 触发后又返回 None，否则 fix 失效）。C3/W3 现有 fixture + assertion 是锁住 buggy 行为，必须与修复保持一致。

## 2. 验证结果

### 2.1 L1（按 prompt §"完成标准"）

| 命令 | 结果 |
|---|---|
| `cargo test -p input-capture --lib geometry` | ✅ **43 passed / 0 failed**（原 42 + 新 1 `query_pure_falls_back_to_legacy_when_active_has_monitor_none`；C1-C8 / W1-W6 / vacuous `monitor_id_none_uses_legacy_key` / 新增 production-realistic 全绿） |
| `cargo test --workspace` | ✅ **165 passed / 0 failed**（input-capture 69 + lan-mouse 67 + capture_test 7 + emulation_test 2 + lan-mouse-vue 15 + lan-mouse-cli 5；原 164 + 新 1 = 165） |
| `cargo clippy -p input-capture --all-targets -- -D warnings` | ✅ 0 新 warning（仅有 tracing-crate 的 pre-existing "trace filter directives" 噪音，与本步无关；见 STEP-M3-3.6 §2.1 末尾） |
| `cargo fmt --check -p input-capture` | ✅ 0 diff |
| `cargo build --workspace` | ✅ 0 error / 0 warning |
| `cargo build -p input-capture` | ✅ 0 error / 0 warning |
| `git diff --stat Cargo.lock` | ✅ 空 diff（无依赖变化） |

### 2.2 单测明细

- **新加 1 个**：`geometry::tests::query_pure_falls_back_to_legacy_when_active_has_monitor_none`（production-realistic fixture，直接调私有 `query_pure` 锁住 fallback 路径）
- **更新 2 个**：`crossed_pure_c3_display0_top_misses_legacy_active`（assertion + docstring）+ `activation_pure_w3_display0_top_misses_legacy_clients`（assertion + docstring）
- **未触动**：8 个 crossed_pure_c1/c2/c4a/c4b/c5/c6/c7/c8 + 4 个 activation_pure_w1/w2/w4/w5/w6 + vacuous `crossed_pure_monitor_id_none_uses_legacy_key` + 旧 macOS / Windows / libei / dummy / layer_shell / poll_next 全保留零回归
- **文档注释同步**：C1-C8 矩阵注释 C3 行 + W1-W6 矩阵注释 W3 行 + `crossed_pure` docstring（invariant 列表 + new fallback 项）

### 2.3 milestone 边界

- ✅ 仅触碰 `input-capture/src/geometry/mod.rs`（per leader "不越界"）
- ✅ 未触碰 `macos.rs` / `windows/event_thread.rs` / `layer_shell.rs` / `libei.rs` / `service.rs` / `capture.rs`
- ✅ 未引入 M4 `exposed_segments` / sub-region / offset-span 语义
- ✅ 未改 PLAN 文档（只读目标）

## 3. 与 PLAN 的偏差

**PLAN 偏差 #1（必要，leader 评审）**：C3 + W3 测试的 assertion + docstring 被本步修改。

**理由**：
- leader prompt §"完成标准"同时要求"H1 修复 + 现有 9 C1-C8 + 6 W1-W6 全绿"
- H1 修复代码（leader 显式给出）与 C3/W3 现有 assertion 互斥 — 修复必然让它们变红
- 按 leader 失败兜底"单测挂 → 自行解决"约定，最务实做法是更新 assertion + docstring + matrix 注释，把"测试在锁 buggy 行为"翻转为"测试在锁 post-H1 行为"
- 测试名保留（C1-C8 matrix index 兼容），C3/W3 函数仍存在，只是 assertion 反映正确行为

**leader 应评审**：如果你认为 C3/W3 应该删（而不是 assertion 反转），可以 `git revert` 本步对这两个函数的改动 + 删除新加的 `query_pure_falls_back_to_legacy_when_active_has_monitor_none`，让旧 C3/W3 在新代码下红着 — 但那样 H1 fix 就缺回测保护了。个人推荐保留本步方案。

## 4. 处理的 SUGGESTION 项

- `SUGGESTION.md`：仍空（本步未发现新的活跃问题）
- `SUGGESTION-FIXED.md`：未新增条目
- `SUGGESTION-IGNORE.md`：未新增条目

**内部观察**（不归档到 SUGGESTION，仅供 leader 参考）：
- query_pure 的 legacy fallback 多一次 HashSet probe（最坏 case +1）。当 `displays[idx].monitor_id == None`（vacuous 路径）时已被 `key.monitor.is_some()` 短路，所以生产热路径（displays 永远 `Some(...)`）的 cost 是固定 1 次额外 contains — 在 mouse-move barrier event 的频度（~100 Hz 上限）下完全可忽略
- PLAN §3 STEP-3.4 line 162 表格 C3 的 invariant 描述与本步最终行为相反（"miss" → "hit legacy via fallback"），未来 planer 任务同步 PLAN 时可一并改

## 5. 闸门检查

| 闸门 | 结果 |
|---|---|
| 时间门 | ✅ 实际 ~15 min（prompt 预算 ~15 min，未超） |
| milestone 边界门 | ✅ 仅 M3 范围（leader "不越界" 严格守住） |
| 闸 1 产物 / 依赖 / 验收 | ✅ query_pure 改完 + 新测 + C3/W3 同步；workspace build 干净；Cargo.lock 不变 |
| 闸 2 执行中偏差 | PLAN 偏差 #1（C3/W3 assertion 反转，已在 leader prompt 失败兜底约定授权范围内） |
| 闸 3 STEP 回归 | ⏸ 跳过（非 milestone 收尾；M3 收尾在 STEP-3.6 已完成，本 fixup 不重复 clippy workspace 全量） |

## 6. 遗留 / 风险

- ⚠️ **真机多屏回归**（继承自 STEP-3.6 遗留）：
  - macOS 真双屏 dropdown（旧 config / 没选 dropdown 这两种场景下，鼠标移到边缘 → daemon 日志应看到 `Crossed barrier into: BarrierKey { pos: Top, monitor: None, ... }` + 对端收到 Enter）
  - Windows 真双屏 dropdown（同上）
  - 旧 config（无 `monitor` 字段）+ 单 monitor 场景：行为不变（PLAN §8 line 336 回归保护，本步自动 lock）
  - 这些是 H1 修复的最终验证，由 leader 派用户真机回归

- ⚠️ **STEP-DEBUG-M3-BARRIER-CHAIN §3 H2 / H3 未触碰**：
  - H2（`displays` 与 `last_monitors` ID 暂态不一致）— ~10% 概率，仅在 H1 修后用户仍报告"完全不触发"才需要做
  - H3（macOS `Capture::create` fire-and-forget 竞态）— ~5% 概率，是 P2 改进，与"持续不触发"症状不符
  - **root cause 是否仅 H1**：本步 fix 让 query_pure 行为正确，但**真机是否解决**需要 leader 派用户回归。如果 H1 修后仍不触发，按 H2 → H3 顺序继续排查（per debug report §4）

## 7. 下一步

- **leader**：review 本报告（重点：PLAN 偏差 #1 — C3/W3 assertion 反转是否同意）+ commit
- **leader**：派用户真机多屏回归（4 项 + 旧 config 单 monitor 兼容）确认 H1 修复落地
- **真机回归后**：若 H1 修后仍"完全不触发"，按 STEP-DEBUG-M3-BARRIER-CHAIN §4 走 H2 → H3
- **真机回归后**：若 H1 修复落地，M3 全部 STEP + 修复 fixup 收尾完毕，可正式 close M3

---

**解决 STEP**：M3 / STEP-3.4-FIXUP（H1 修复）

**milestone 状态**：M3 主体 STEP-3.4 / 3.5 / 3.6 已收尾；本 fixup 修复 H1（query_pure legacy fallback）；等用户真机回归

**改动文件清单**（仅 paths）：
- /Users/hb/Projects/@cloudself/lan-mouse-pro/input-capture/src/geometry/mod.rs
- /Users/hb/Projects/@cloudself/lan-mouse-pro/next/STEP-M3-3.4-FIXUP.md（本文件）

**新增 / 修改单测数**：+1 新增 / -0 删除 / 2 修改（C3 + W3 assertion + docstring 反转）
**累计耗时**：~15 min（prompt 预算 ~15 min）
**PLAN 偏差**：#1（C3/W3 assertion + docstring 反转，由 leader prompt "单测挂 → 自行解决" 兜底约定授权；建议 leader 评审）
**SUGGESTION 提交**：0
**root cause 单一性**：H1 是 STEP-DEBUG-M3-BARRIER-CHAIN 报告 ~85% 概率的根因；H2/H3 为 ~10%/~5% 次要嫌疑，本步不动，留待真机回归后按需启动
