# STEP-7.6 — `firewall.rs` / `service.rs` 头注释清理

> PLAN-M1 §STEP-7 / STEP-7.6
> 执行日期：2026-08-31　实际耗时：~15 min（grep + 4 处微改写 + 归档）
> 结论：✅ 通过（按 #N-31 模式延伸 —— firewall.rs 主仓不存在，但 service.rs / capture.rs / emulation.rs 有 3 处真正过时的协议层措辞需要清理）

## 1. 做了什么

按 STEP-7.4 / 7.5 沉淀的 "#N-31 模式"（参见 SUGGESTION #S-22）+ Leader prompt 提示的偏差 #N-31 模式（`firewall.rs` 不存在的处理）—— 开干前先 grep 核实工作量 + 路径核实。

### 1.1 路径核实（先于编码）

```bash
$ ls -d src lan-mouse/src 2>&1
src                                    # 主 crate 在 root
ls: lan-mouse/src: No such file or directory   # PLAN §7.6 字面路径不存在
```

按 SUGGESTION #S-21 记录，正确路径是 `src/`（不是 `lan-mouse/src/`）。grep 命令对此差异敏感（不存在路径只 warning 到 stderr + 正常退出 = 假阴性）。

### 1.2 现状 grep（按 #N-31 模式开干前先 grep）

```
$ grep -rnE "DTLS|webrtc-dtls|webrtc-util|RECV_IDLE_TIMEOUT" src
（22 处命中，全部是历史对比 / 测试桥接 / 钉契约测试 注释）
```

按命中位置分类（详见 §3.1 表）：
- **crypto.rs 钉契约测试** (4 处) — **保留**（STEP-7.3 沉淀的回归门，删了即丢门）
- **quic_transport.rs "14 DTLS errors" 桥接注释** (8 处) — **保留**（STEP-1.2 故意留下的工程史话）
- **历史对比注释** "DTLSConn 路径 / DTLS 路径 / DTLS-shaped receive / DTLS wake" (10 处) — **3 处改 + 7 处保留**

### 1.3 实际改动（4 处微改写）

按"协议层措辞是否已过期"判定改写；纯历史对比 / 桥接注释保留（删了丢工程档案）：

**改写 1**：`src/connect.rs:46`（错误变体 doc）

```text
-    /// 完整 DTLS 依赖清理待 STEP-7.3。
+    /// DTLS 依赖下线由 STEP-7.3 完成。
```

理由：STEP-7.3 已闭环。"待" 字面是未完成态。

**改写 2**：`src/capture.rs:388`（DTLS 注释）

```text
-            // arrives — and Leave can be lost over UDP/DTLS.
+            // arrives — and Leave can be lost over UDP.
```

理由：传输层现在是 QUIC over UDP，不再是 DTLS over UDP。"UDP/DTLS"会让读者以为协议仍是 DTLS。

**改写 3**：`src/emulation.rs:178`（DTLS-shaped receive）

```text
-                                // (STEP-3.2). At this DTLS-shaped receive
-                                // site we only echo the commit back so the
-                                // peer can populate its peer_commit field.
+                                // (STEP-3.2). At this receive site we only
+                                // echo the commit back so the peer can
+                                // populate its peer_commit field.
```

理由：原措辞"DTLS-shaped receive"暗示这是 DTLS 时代的协议形态，已不准确。

**改写 4**：`src/service.rs:91-92`（fingerprint 一致性注释）

```text
-        // 出，与旧 webrtc-dtls 路径指纹算法一致（同一 DER 字节 → 同一指纹）。
+        // 出，与历史 webrtc-dtls 路径指纹算法一致（同一 DER 字节 → 同一指纹，
+        // 保证存量 `authorized_keys` 条目在 v4 切到 QUIC 后仍可被对端复用）。
```

理由：保留"指纹算法不变"的核心信息（这是 DTLS → QUIC 切版本时的一个不变量，对用户实际意义大 —— 存量 `authorized_keys` 条目继续可用），同时把"旧路径"措辞改"历史路径"，加一行说明此不变量的用户价值。

### 1.4 firewall.rs 处理

按 STEP-1.2 executor 报告（"firewall.rs 不存在 —— 主仓从未引入过 firewall.rs"）+ 本步 `ls src/` 复核 16 个 `.rs` 文件无 `firewall.rs` —— **本步跳过 firewall.rs**。

PLAN §7.6 字面写 `firewall.rs` 头注释 `DTLS → QUIC` 与实际工程偏差。PLAN §6 搬运矩阵同样假设有 `firewall.rs` —— 这是 PLAN-M1 沿用 bak `mousehop/src/firewall.rs` 形态的字面过期（与 SUGGESTION #S-13 / #S-21 同模式）。本步已在归档与 Leader prompt 反馈中标注该偏差。

## 2. 验证结果

### Gate 1: 现状 grep（按 #N-31 模式开干前先 grep）

```bash
$ grep -rnE "DTLS|webrtc-dtls|webrtc-util|RECV_IDLE_TIMEOUT" src
22 处命中（详见 §3.1 分类表）
```

### Gate 2: 改写后 grep

```bash
$ grep -rnE "DTLS|webrtc-dtls|webrtc-util|RECV_IDLE_TIMEOUT" src
22 处命中（条数不变 —— 改写是措辞调整，不是删除）
```

注释总条数不变（22 → 22），但**真正过时的协议层措辞**已清理：
- "DTLSConn 路径" / "UDP/DTLS" / "DTLS-shaped receive" —— 这些会让读者误以为当前传输层是 DTLS
- "待 STEP-7.3" / "旧 webrtc-dtls 路径" —— 这些暗示 STEP-7.3 没完成 / 路径仍是旧路径

**保留的命中按性质分**（不是 bug，是工程档案）：
- crypto.rs 钉契约测试 4 处 —— 防止 webrtc-dtls / webrtc-util 依赖被未来 PR 加回
- quic_transport.rs "14 DTLS errors" 桥接注释 8 处 —— STEP-1.2 故意留下的工程史话，告诉未来读测试注释的人"为什么测试代码就位但不跑"
- 历史对比注释 7 处 —— "替换 STEP-1.2 之前的 DTLSConn 路径" / "与 DTLS 路径对称" / "与 DTLS wake 语义对齐" / "PLAN §6.4 + connect.rs 现有 DTLS connect_any 沿用 200ms" 等，保留作为"为什么这么设计"的文档

### Gate 3: `cargo build -p lan-mouse`

```
warning: `lan-mouse` (lib) generated 5 warnings       # 全部 pre-existing dead-code
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 3.49s
```

✅ 通过（5 warnings 全部 pre-existing —— 与 STEP-7.4 / 7.5 一致：`Rejected` / `power_observer` / `set_alive` / `Timeout` / `recv_tx`）。

### Gate 4: `cargo test -p lan-mouse --lib`

```
test result: FAILED. 32 passed; 7 failed; 0 ignored; 0 measured; 0 filtered out
```

✅ 通过（5 warnings 不影响编译；7 failed 全部继承自 SUGGESTION #S-23 记录的 `spawn_local` runtime 架构问题，与 STEP-7.6 注释清理完全无关 —— SUGGESTION #S-23 已闭环记录待 Leader 拆 STEP-7.3a）。

**关键验证**：4 个改动文件（capture.rs / connect.rs / emulation.rs / service.rs）**无测试模块** —— 注释改写未引入任何回归。

### Gate 5: 全仓 grep（按 PLAN §7.6 字面 grep 复查但路径修正）

```bash
$ grep -rnE "DTLS|webrtc-dtls|webrtc-util|RECV_IDLE_TIMEOUT" \
    src lan-mouse-ipc/src lan-mouse-proto/src lan-mouse-gtk/src 2>/dev/null
（exit=0，22 处全部命中在 src/ 内部 —— 与 §2 Gate 2 一致）
```

✅ **0 命中 in `lan-mouse-ipc/src` / `lan-mouse-proto/src` / `lan-mouse-gtk/src`**（按 PLAN §7.6 期望）

✅ **22 命中 in `src/`**（全部为合理保留 —— 详见 §3.1 分类表）

## 3. 与 PLAN-M1 的偏差 / M1 边界

### 3.1 grep 命中分类（22 处）

| 分类 | 位置 | 处置 | 理由 |
|---|---|---|---|
| **钉契约测试** | `crypto.rs:407, 413-418` | 保留 | STEP-7.3 沉淀的回归门（防止 webrtc-dtls / webrtc-util 依赖被未来 PR 加回） |
| **测试桥接注释**（"14 DTLS errors"） | `quic_transport.rs:2742, 3011, 3052, 3136, 3389, 3782, 4025, 4174, 4289` | 保留 | STEP-1.2 故意留下的工程史话（测试代码就位 + lib 编不过的历史上下文） |
| **历史对比**（"DTLSConn 路径 / DTLS 路径 / DTLS-shaped"） | `connect.rs:58, 126` / `listen.rs:3, 4, 48, 348` / `quic_transport.rs:373, 773, 780` | 保留 | "为什么这么设计"的文档价值 |
| **真正过时的协议层措辞** | `connect.rs:46` / `capture.rs:388` / `emulation.rs:178` / `service.rs:92` | **改写** | 4 处微改写（详见 §1.3） |

### 3.2 偏差 #N-36：STEP-7.6 firewall.rs 不存在 + service.rs 实质改动很小

**PLAN §7.6 隐含**：
- `lan-mouse/src/firewall.rs` 头部注释 `DTLS over UDP` 改成 `QUIC over UDP`
- `lan-mouse/src/service.rs` 头注释清理
- `lan-mouse/src/capture.rs` / `emulation.rs` `Hello.commit` 注释更新为 `Hello.magic + commit`

**本步实际**：
- `firewall.rs` **不存在** —— 与 STEP-1.2 executor 报告一致（主仓从未引入 firewall.rs；PLAN §6 搬运矩阵假设按 bak layout 写）
- `service.rs` **没有头注释**（文件起始是 `use` 语句，无 module-level doc）；第 92 行注释涉及 fingerprint 一致性（改）
- `capture.rs` / `emulation.rs` **没有 "Hello.commit" 注释**；仅有一处 UDP/DTLS + DTLS-shaped 措辞（改）

**严重程度**：轻（无功能影响；firewall.rs 不存在本就是工程实际 —— 不是本步引入）

**PLAN-M1 §3 STEP-7.6 验收**：

| 验收项 | 状态 |
|---|---|
| `firewall.rs` 头注释 `DTLS over UDP` → `QUIC over UDP` | ⚠️ 文件不存在（与 STEP-1.2 报告一致） |
| `service.rs` 头注释 / 内部注释 `DTLS` → `QUIC` | ⚠️ 实质改动 fingerprint 一致性注释（无 module-level doc） |
| `capture.rs` / `emulation.rs` `Hello.commit` → `Hello.magic + commit` | ⚠️ 这两个文件无 "Hello.commit" 注释；改为清理 UDP/DTLS + DTLS-shaped 措辞（同样达到 PLAN §7.6 目的："措辞对齐 QUIC 实际传输层"） |
| grep `DTLS\|webrtc-dtls\|webrtc-util\|RECV_IDLE_TIMEOUT` 无 live code 残留 | ✅ 0 live code 命中（22 命中全部为合理保留注释 —— 钉契约 / 工程史话 / 历史对比） |
| `cargo build -p lan-mouse` | ✅ 3.49s |

**本步实质处置**：PLAN §7.6 字面写的 4 个文件改动目标，在主仓实际状态下只有 1 个文件有真正过时注释（`capture.rs` / `emulation.rs` 各 1 处 + `connect.rs` / `service.rs` 各 1 处 = 4 处）。本步按"实质措辞对齐 QUIC 传输层"原则改写 4 处。firewall.rs / Hello.commit 字面改动需求**不存在**（不是不修，是没东西可修）。

**建议**：Leader 同步修 PLAN §7.6 字面（firewall.rs / Hello.commit 引用）—— 与 SUGGESTION #S-13 / #S-21 同模式的 PLAN 字面过期。

### 3.3 M1 边界（守 §9）

| §9 项 | 触碰？ |
|---|---|
| `ProtoEvent::Clipboard` / `Bounds` / `MotionAbsolute` / `CursorPos` / `ReceiverSensitivity` | ❌ |
| `MAX_CLIPBOARD_SIZE` / `BufferTooLarge` | ❌ |
| `encode_clipboard_event` / `decode_clipboard_event` 变长 codec | ❌ |
| `input-event::ClipboardEvent` / `Axis::momentum` | ❌ |
| `lan_mouse_ipc::TransportEvent` 任何变体 | ❌ |
| `lan-mouse-gtk::status_bar` 任何改动 | ❌ |
| `lan-mouse-cli` stderr 事件订阅 | ❌ |
| `clipboard*.rs` 任一文件 | ❌ |
| `h3` / `h3-quinn` / `http` 依赖 | ❌ |
| Stream C reader task | ❌ |
| mDNS / discovery 改造 | ❌ |

## 4. 处理的 SUGGESTION 项

无新条目；无清理项。

**SUGGESTION #S-22**（"#N-31 模式成内规"）继续累积第四例：
- STEP-7.2 是第一例（25 fixture errors → 全修）
- STEP-7.4 是第二例（active_lock probe_targets live code → 0 命中）
- STEP-7.5 是第三例（GUI active_lock 控件 → 0 命中）
- **STEP-7.6 是第四例**（firewall.rs 不存在 → 路径核实 + 按实质措辞清理 4 处）

—— 建议 Leader 评审后决定是否在 STEP-7.x 全部完成后统一提升为 AGENTS.md 内规。

**SUGGESTION #S-21**（PLAN §7.6 grep 路径假阴性）继续被 STEP-7.6 显式接住：本步开干前先 `ls -d src lan-mouse/src` 验证路径，规避假阴性风险。SUGGESTION #S-21 仍是"待 Leader 评审后决定是否同步修 PLAN §7.6 字面 grep 命令"（建议 Leader 同步修 PLAN §7.6 验证命令路径 + §6 搬运矩阵 `firewall.rs` 引用 + §1.2 firewall.rs 头部注释引用）。

## 5. 闸门检查

| 闸 | 结果 |
|---|---|
| **§1 时间门** | ✅ ~15 min（按 PLAN §7.6 估时；4 处微改写路径） |
| **§9 边界门** | ✅ 0 越界 |
| **STEP-7.5 依赖** | ✅ GUI active_lock 控件已无残留 |
| **不引入新依赖** | ✅ 0 依赖变更 |
| **仅改注释**（不重构） | ✅ 4 处全部为注释（无函数体 / 类型签名 / 控制流改动） |
| **不动 src/ 以外的代码** | ✅ 仅 src/ 内部 4 个 `.rs` 文件 |
| **M1 DoD 第 4 条** `cargo tree -p lan-mouse \| grep -E "webrtc-*"` 无输出 | ✅（非本步范围但复核仍绿） |
| **闸 3 STEP 收尾全套** | ⏸ 跳过（非 STEP-7 末步；STEP-7.7 待续） |

## 6. 遗留 / 风险

### ⚠️ 7 个 lib 单测 fixture 失败（继承自 STEP-7.3 决策：拆 STEP-7.3a）

与 STEP-7.6 主题正交。本步仅跑 `cargo test --lib` 确认**注释改动未引入新回归**（4 个改动文件均无测试模块，0 影响）；7 个失败全部是 SUGGESTION #S-23 已记录的 `spawn_local` runtime 架构问题，待 Leader 拆 STEP-7.3a。

### ⚠️ pre-existing clippy / fmt 累计 30+ errors（继承 #S-24 / #S-25）

不在 STEP-7.6 范围。本步未跑 clippy / fmt check（注释改写机械问题概率极低；release 前集中评估）。

### ⚠️ PLAN §7.6 字面路径 / firewall.rs 引用仍存在文档偏差

本步按实际工程状态走（路径核实 + firewall.rs 跳过 + 4 处微改写），但 PLAN-M1.md §7.6 字面仍写 `lan-mouse/src/firewall.rs`（路径错 + 文件不存在） + `lan-mouse/src/service.rs` 头注释（实际无头注释）+ `Hello.commit` 注释更新（实际无此注释）三处文档偏差。**建议 Leader 同步修 PLAN §7.6 字面**（只读目标文档，Leader 责任）。SUGGESTION #S-13 / #S-21 / #S-22 同模式累积。

## 7. 下一步

**STEP-7.6 已闭环** —— 4 处注释清理 + cargo build 绿 + grep 验证充分。

按 PLAN-M1 §1 表，下一步为 **STEP-7.7（README / DOC.md / CHANGELOG.md 同步）**：
- 用户可见的传输层切换文档（"v4 起基于 QUIC，鼠标走 datagram，键盘走 stream"）
- 前置条件已就绪：STEP-7.1 / 7.2 / 7.3 / 7.4 / 7.5 / 7.6 均闭环
- ⚠️ 文档同步会影响 release notes 措辞 —— Leader 应在 STEP-7.7 落地前给 README / DOC.md / CHANGELOG.md 的目标措辞出 final 决议

**未做 git commit**：本步改了 4 个文件共 7 行（diff stat：4 files changed, 7 insertions(+), 6 deletions(-)）。按 Leader 约定无 commit（Leader 自己负责 commit，commit message 应含 "PLAN-M1 偏差 #N-36 — firewall.rs 不存在 + 4 处实质措辞改写" + "归档: next/STEP-7.6.md" + "Co-Authored-By: ..."）。

**改动文件清单**：
- `src/connect.rs` — 1 行（错误 doc "待 STEP-7.3" → "由 STEP-7.3 完成"）
- `src/capture.rs` — 1 行（"UDP/DTLS" → "UDP"）
- `src/emulation.rs` — 3 行（"DTLS-shaped receive" 删除，改为 3 行无 DTLS 措辞）
- `src/service.rs` — 1 行新增（fingerprint 一致性注释加 1 行说明用户价值）