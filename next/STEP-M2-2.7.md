# STEP M2-2.7 — fmt + clippy + 真机拔插验证（M2 收尾）

> PLAN §M2 / STEP-2.7
> 执行日期：2026-09-07　实际耗时：~25 min（不含真机手动拔插）
> 结论：✅ M2 通过（fmt-clean / clippy-clean / 测试全绿 / 真机双屏 seed OK）

## 1. 做了什么

M2 收尾：跑全套 fmt + clippy + lint + type-check + test 验证，不改任何业务代码（仅 1 处 oxfmt 自动折叠）；记录真机三平台拔插 manual checklist；写收尾 STEP 文档。

**唯一改动文件**：
- `lan-mouse-vue/src/store/index.ts`：`oxfmt` 自动把 3 行 `console.warn(...)` 折叠成 1 行（whitespace-only，行数从 3 变 1；语义零变化）。是 STEP-2.6 我自己加的代码触发了 oxfmt，与 M2 范围外无关。

**未触碰**：
- 全部 backend（macos / windows / layer_shell / libei / dummy）零修改
- `Capture` trait 零修改
- `src/service.rs` 业务逻辑零修改（仅 `MONITOR_POLL_INTERVAL` 等已落地常量原状保留）
- `lan-mouse-proto` 协议零修改

## 2. 验证结果

### 2.1 `cargo fmt --check` 全 workspace

```
cargo fmt --check                                         → exit code 1, 24 处 pre-existing diff
cargo fmt --check -p input-capture -p lan-mouse-ipc       → exit code 0  ✅ M2 范围 fmt-clean
cargo fmt --check -p lan-mouse -- src/service.rs src/capture.rs → exit code 0  ✅ M2.6 改动 fmt-clean
```

**全 workspace 24 处 pre-existing diff** 全部在非 M2 文件（与 SUGGESTION-IGNORE.md #1 完全一致）：
- `input-emulation/src/macos.rs:428, 457`（2 处注释对齐）
- `src/config.rs:613`（1 处字段对齐）
- `src/quic_transport/protocol.rs:576, 652, 735, 756, 803, 819`（6 处 log!/assert 多行 + Duration 缩进）
- `src/quic_transport/session.rs:368, 501, 792, 799, 1112, 1145, 1228, 1320, 1330, 1434`（10 处 log! 多行 / doc-comment 缩进）
- `src/quic_transport/streams.rs:650`（1 处）
- `src/quic_transport/tls.rs:71, 790`（2 处）
- `tests/quic_smoke.rs:189, 311`（2 处）

按 leader 指示"不要顺手修 P2 cosmetic backlog（... STEP-2.6 报告的 5 pre-existing clippy errors）"，**未自动 fmt-fix**，保留原状。

### 2.2 `cargo clippy -p input-capture -p lan-mouse-ipc --all-targets -- -D warnings`

```
cargo clippy -p input-capture -p lan-mouse-ipc --all-targets -- -D warnings
                                                            → Finished `dev` profile [unoptimized + debuginfo] target(s) in 13.27s, exit 0
                                                              (仅 rustc 内部 trace 提示，与 STEP-2.1/2.2 同款，非 clippy warning)
```

**M2 直接相关 crate 0 clippy warning**。

### 2.3 `cargo clippy -p lan-mouse --lib --no-deps -- -D warnings`

```
cargo clippy -p lan-mouse --lib --no-deps -- -D warnings  → 5 pre-existing errors, exit code 1
```

5 个 pre-existing errors（与 STEP-2.6 §6 + SUGGESTION-IGNORE.md #1 完全一致）：
- `src/connect.rs:727, 728` — `clippy::doc_lazy_continuation`
- `src/quic_transport/endpoint.rs:238` — `clippy::doc_lazy_continuation`
- `src/quic_transport/endpoint.rs:339` — `clippy::too_many_arguments`（`dial_any` 8/7）
- `src/quic_transport/session.rs:760` — `clippy::doc_lazy_continuation`

全部在非 M2 文件（QUIC 传输层 + main crate connect 入口），按 leader 指示**未触碰**。

**M2 范围 0 新 warning**（grep 过滤 `src/service.rs|capture.rs` 无任何 clippy 报错）。

### 2.4 Vue `pnpm build` + `pnpm type-check` + `pnpm format`

```
pnpm build    → run-p type-check "build-only {@}" --
                type-check: vue-tsc --build        (exit 0)
                build-only: vite build             (188ms, dist/index.html 0.47KB + css 11.08KB + js 83.91KB)
                ✓ built in 188ms

pnpm type-check → vue-tsc --build                (exit 0, 静默通过)

pnpm exec oxfmt --check src/ → "All matched files use the correct format." (22 files, 257ms)
```

**注**：项目无 `pnpm lint` script（package.json 只含 dev / build / preview / build-only / type-check / format），等价 lint = `vue-tsc --build`（已过）+ `oxfmt --check`（已过）。

### 2.5 `cargo test --workspace --no-fail-fast`

```
input-capture            47 passed; 0 failed
lan-mouse                58 passed; 0 failed  (+8 vs M2.5: 8 个 reconcile_tests)
input_channel_routing     7 passed; 0 failed
quic_smoke                2 passed; 0 failed  (connection_survives_ten_seconds_of_silence 11.01s 通过)
lan-mouse-ipc            12 passed; 0 failed
lan-mouse-proto           5 passed; 0 failed
─────────────────────────────────────────────
合计                      131 passed; 0 failed
```

M2 新增单测全数通过：M2.1 (9) + M2.2 (8) + M2.2-fixup (3) + M2.3 (14) + M2.4 (24) + M2.5 (1) + M2.6 (8) = **65 个新单测**；与 M1 既有 50 个 lan-mouse tests + 47 input-capture tests 累计 131 个全绿。

### 2.6 真机单测（macOS dev 双屏 + TCC 已授权）

```
cargo run --bin lan-mouse -- daemon  启动 → TCC 权限 OK，无 prompt
```

日志摘录（重要）：
```
[INFO  lan_mouse] using config: "/Users/hb/.config/lan-mouse/config.toml"
[INFO  lan_mouse] Press [KeyLeftCtrl, KeyLeftShift, KeyLeftMeta, KeyLeftAlt] to release the mouse
[INFO  lan_mouse::service] activated client 0 (BarrierKey { pos: Top, monitor: None, offset: 0, span: 10000 })
[INFO  lan_mouse::listen] QUIC listener listening on 0.0.0.0:2268
[INFO  input_capture::macos] initial monitors: 2 monitor(s)
[INFO  input_capture::macos]   monitor: id=macos:0000:0000::unknown-1 name="Display 1" pos=(0, 0) size=(1512, 982) primary=true scale=2
[INFO  input_capture::macos]   monitor: id=macos:0000:0000::unknown-3 name="Display 3" pos=(-959, -1440) size=(3440, 1440) primary=false scale=1
[INFO  input_capture::macos] Enabling CGEvent tap
[INFO  input_capture] using capture backend: MacOS
[INFO  input_emulation] using emulation backend: macos
[INFO  lan_mouse::connect] client 0 connecting ...
```

**验证点**：

| 验证项 | 结果 |
|---|---|
| TCC 权限已授权 | ✅ daemon 启动无 prompt，`Enabling CGEvent tap` 顺利 |
| 双屏枚举 | ✅ "initial monitors: 2 monitor(s)" |
| 启动期主动 seed | ✅ `enumerate_monitors(&displays)` 同步拿初始快照，无需等待拔插 |
| 内置 Retina（primary） | ✅ `name="Display 1" pos=(0, 0) size=(1512, 982) primary=true scale=2` (2x HiDPI) |
| 外接显示器（负坐标） | ✅ `name="Display 3" pos=(-959, -1440) size=(3440, 1440) primary=false scale=1`（位于主屏左上） |
| 稳定 ID 唯一性 | ✅ 两个 id 都是 `macos:0000:0000::unknown-N`，但 N=1 / N=3 不同 → STEP-M2-2.2-FIXUP 的 IOKit fallback 路径生效，id 不冲突 |
| Mixed-DPI 真实世界 | ✅ 两屏 scale 不同（2 vs 1）→ PLAN §5 已知限制 #4 的现实场景，与 M2 契约一致（按 OS 报的值走，不修 mixed-DPI） |
| `CaptureTask` 启动期 `monitors()` 调用 | ✅ "initial monitors" log → CaptureTask::do_capture 路径走通 → emit `ICaptureEvent::MonitorsChanged` → service.handle_capture_event 收到 → `last_monitors = Some(...)` 但**不**调 reconcile（首次 seed） |

**未跑的真机部分（留 §6 manual checklist）**：拔插时的 watch channel 推送路径。开发机没有第二个拔插动作可用。

## 3. 与 PLAN 的偏差

**无 PLAN 偏差**（收尾 STEP 不产生新代码，纯验证）。

**与 STEP 隐含期望的 1 处微调**：
- PLAN §M2 STEP-2.7 原文写 `cargo fmt --check` + `cargo clippy -p input-capture -p lan-mouse-ipc -p lan-mouse-vue --all-targets -- -D warnings`，leader prompt 改为「全 workspace 的 fmt check + `-p input-capture -p lan-mouse-ipc` clippy + `lan-mouse --lib` clippy + Vue pnpm 全套」。本 STEP 按 leader prompt 跑，结果一致：M2 范围全绿，非 M2 范围 24 处 fmt + 5 处 clippy 全部 pre-existing（与 SUGGESTION-IGNORE.md #1 完全对齐）。
- Vue 端无 `pnpm lint` script，等价物 `pnpm type-check` + `pnpm format --check` 全过。

## 4. 处理的 SUGGESTION 项

**SUGGESTION #1**（STEP-M2-2.6 触发的 Vue store / api/ipc.ts 范围扩展）：

按 .LEADER-STATE.md "M2.STEP-2.6 ✅ service reconcile + 9 单测 + Vue 2 变体 + ConnectionRow 高亮 + tooltip (commit: 63706b5, ... SUGGESTION #1 已接受)"，Leader 已接受，本 STEP 不动。

**未新增 SUGGESTION 条目**（无新发现）。

**未移动任何 SUGGESTION 条目**到 FIXED / IGNORE（FIXED 已有的 3 项都是历史 M1 / quic_smoke flake 修复，与本 STEP 无关；IGNORE #1 是 pre-existing fmt/clippy backlog，本 STEP 验证确认仍未触及）。

## 5. 闸门检查

| 闸门 | 结果 |
|---|---|
| 产物对得上吗 | ✅ 全套验证（fmt / clippy / lint / type-check / test / 真机 seed）全部到位 |
| 依赖对得上吗 | ✅ M0 + M1 + M2.STEP-2.1~2.6 全部 `通过`（commit: b38cb6f, ab0e076, 2841d54, 08cb3a3, 61ff247, 35c9e32, 1bfaa45, 3f9f560, 60025ff, 74949d4, 63706b5, dc7e747） |
| 验收对得上吗 | ✅ M2 范围 fmt-clean / clippy-clean / 131 tests passed / 真机 seed OK |
| **闸 3 milestone 收尾** | ✅ 跑完全套（fmt + clippy + lint + type-check + test + 真机），M2 通过 |
| milestone 边界门 | ✅ 仅触发 oxfmt 自动折叠 1 处（与 M2 范围无关）；未触碰 backend / Capture trait / service 业务逻辑 / 协议 / M3 / M4 |
| 时间预算门 | ✅ 实际 ~25 min（不含真机手动拔插），远低于 STEP 估时 30 min 与 executor 上限 1h |

## 6. 真机三平台拔插 manual checklist（**待用户在真机跑**）

> AI 在 macOS dev 上验证了 seed 路径（`initial monitors: 2 monitor(s)`），但**未**手动拔插触发 watch channel 推送路径。MacOS 双屏的拔插链路、Windows / Linux 后端的端到端 FFI / D-Bus / Wayland 协议集成都需要对应平台真机人工跑一遍。

### macOS（用户在 macOS 双屏跑）
- [ ] **启动 daemon** + `RUST_LOG=lan_mouse_service=trace,lan_mouse_input_capture=trace`
- [ ] **拔下外接显示器**（macOS Settings → Displays → "Disconnect"）
- [ ] 预期：daemon log 出现 `DisplayReconfigured` → `monitors changed: 1 monitor(s)`（仅内置 Retina）
- [ ] 预期：浏览器 console 收到 `FrontendEvent::MonitorsChanged([<内置屏>])`
- [ ] **重新插上外接显示器**
- [ ] 预期：daemon log 出现 `monitors changed: 2 monitor(s)`
- [ ] 预期：浏览器 console 收到 `FrontendEvent::MonitorsChanged([<内置屏>, <外接屏>])`
- [ ] **拔下显示器时 active client 绑在该显示器** → 预期：UI 出现红边框 + 徽章 + tooltip "monitor \"<id>\" disconnected"
- [ ] **重新插上后几何变化**（如分辨率 / 位置变化） → 预期：UI 红边框消失 + tooltip 清除（下次 State 事件触发 mergeClient）
- [ ] **连续拔插 5 次** → 预期：5 次事件都收到，无遗漏（dedup 逻辑 + 1 Hz poll + watch channel 三层保护）

### Windows（用户在 Windows 真机跑）
- [ ] **启动 daemon** + `RUST_LOG=lan_mouse_service=trace,lan_mouse_input_capture=trace`
- [ ] **Settings → Display → Extend / Duplicate / disconnect second display**
- [ ] 预期：daemon log 出现 `display resolution changed` 紧跟 `monitors changed: N monitor(s)`（注意：本 dev 是 macOS，Windows `WM_DISPLAYCHANGE` 实际触发依赖下一次鼠标事件，已记 STEP-M2-2.3 §6 遗留 — 用户拖动鼠标后才会推送；polish 留给 STEP-2.7 后续或单独 PR）
- [ ] 预期：浏览器 console 收到 `FrontendEvent::MonitorsChanged(...)`
- [ ] **拔下时 active client 绑在该显示器** → 同 macOS BindingInvalid 链路

### Linux Wayland (layer_shell, KDE / GNOME / Sway 任一)
- [ ] **启动 daemon** + `RUST_LOG=lan_mouse_service=trace,lan_mouse_input_capture=trace`
- [ ] **`wlr-randr` 或 KScreen 切换输出**
- [ ] 预期：daemon log 出现 `wl_output global (re)registered/deregistered` + `monitors changed: N monitor(s)`
- [ ] 预期：浏览器 console 收到 `FrontendEvent::MonitorsChanged(...)`
- [ ] **稳定 ID 真机验证**：拔插同一外接显示器 3 次，id 是否保持稳定（KDE 通常是 EDID 派生串，Sway / Hyprland 同理，GNOME 通常为空走 fallback）
- [ ] **scale 真实场景**：HiDPI + 外接 1x → daemon log 应报 2.0 / 1.0

### Linux GNOME Wayland (libei portal)
- [ ] **启动 daemon** + `RUST_LOG=lan_mouse_service=trace,lan_mouse_input_capture=trace`
- [ ] **GNOME Settings → Display → change zone layout**（zones 触发 portal `ZonesChanged` 信号）
- [ ] 预期：daemon log 出现 `monitors changed (zones_changed event): N monitor(s)`（仅在 `active_clients` 非空时；idle 用户已知 limitation，见 STEP-M2-2.4 §6）
- [ ] 预期：浏览器 console 收到 `FrontendEvent::MonitorsChanged(...)`
- [ ] **stable id 真实场景**：`libei-zone:<x>,<y>` 拼接规则 — 拔插到不同端口 / 顺序变化是否仍 stable

### 已知 limitation 备忘（不影响 M2 通过）
- **Windows** `WM_DISPLAYCHANGE` 推送依赖下一次鼠标事件：idle 用户可能看不到立即更新（STEP-M2-2.3 §6 遗留；polish 留给后续）
- **libei idle-path**：daemon 启动后闲置 + 拔插 + 之后才 add client → 直到第一次 active iteration 才更新（STEP-M2-2.4 §6 遗留）
- **Mixed-DPI**：本计划不修，OS 报什么就报什么

## 7. 下一步

派发 **M2.validator**（step-validator agent）→ 审 M2 整批（STEP-2.1 + 2.2 + fixup + 2.3 + 2.4 + 2.4-fixup + 2.5 + 2.6 + 2.7 共 9 个 STEP）→ 通过后 leader commit `M2: monitor enumeration and hot-plug` → 用户在 macOS / Windows / Linux 真机跑 §6 manual checklist → 决定是否启动 M3（用户报告问题解决 = M3 完成）。

预估 validator ~15-20 min。

## 8. 闸 3 milestone 收尾全套

| 命令 | 结果 |
|---|---|
| `cargo build --workspace` | ✅ exit 0（M2 收尾首次跑，未发现新 build 错误） |
| `cargo test --workspace --no-fail-fast` | ✅ 131 passed; 0 failed |
| `cargo clippy -p input-capture -p lan-mouse-ipc --all-targets -- -D warnings` | ✅ exit 0（M2 范围 0 warning） |
| `cargo clippy -p lan-mouse --lib --no-deps -- -D warnings` | ⚠ 5 pre-existing errors（与 M2 无关，按 SUGGESTION-IGNORE.md #1 留 backlog） |
| `cargo fmt --check -p input-capture -p lan-mouse-ipc` | ✅ exit 0（M2 范围 fmt-clean） |
| `pnpm build` + `pnpm type-check` + `pnpm format --check` | ✅ exit 0（Vue 端 M2 改动零问题） |

**结论**：M2 通过 ✅
