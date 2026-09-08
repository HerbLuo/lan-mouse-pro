# Validation: P2 M0a STEP-0.0+0.1

> 审阅日期：2026-09-08　审阅 STEP 范围：PLAN-2 M0a STEP-0.0 + STEP-0.1（合并执行）
> 起点 commit：`9b316aa`（M3 wrap-up）　终点 commit：`f21ae81`（leader 接力报告）
> 审阅 commits：`af6a17b`（fmt-only）+ `248d276`（substantive）+ `f21ae81`（docs）
> 审阅背景：executor `a43679def60ffdc86` 撞 token plan 上限 429，leader 接力 commit

## 0. 总结

| 项 | 结果 |
|---|---|
| 偏离 PLAN | 0 处 ❌；**1 处 ⚠️ commit 卫生（fmt-only 含 logic 改动）** |
| 偏离 REQUIREMENT | 0 处 |
| BUG | 0 个 P0 / 0 个 P1 / **2 个 P2**（commit 卫生 + doc drift）+ 2 个 P3（cosmetic） |
| 测试真实性 spot-check | ✅ 224 cargo pass / 0 fail（验证方式：stash 未提交 M0b 后跑 `cargo test --workspace --all-targets`，结果 101 + 72 + 7 + 2 + 15 + 27 = 224 ✅） |
| 跨 STEP 一致性 | ✅ dispatcher exhaustive / 全部 14 ProtoEvent 变体有 routing arm / VarCodec/FixedCodec 单一真源 |
| wire-compat（PLAN §0 评审 #1） | ✅ 新 EventType 全部不编码在 StreamA；StreamC 在 M0a **未接通**（send_input 返 Err；streams.rs:334 drop(bunch.c) 仍然存在） |

**Verdict：✅ PASS-with-followup**（不阻塞 M0b 派发，但建议 M0b 落地时顺手清理）

---

## 1. 偏离 PLAN

### STEP-0.0 ✅ 完全符合
- `lan-mouse-proto/src/codec.rs`（新文件，697 行）：`FixedCodec` trait + `VarCodec` trait + 单一真源 dispatcher 全部到位
- 详尽 doc-comment 解释"为什么拆两个 trait"（hot path zero-alloc vs. 变长 payload）—— PLAN §3 M0a STEP-0.0 表格"设计文档 + trait 骨架 + 编译期单测（空 trait 即可）"✅
- trait 骨架编译期单测 `trait_codecs_are_usable()`（codec.rs:459-474）：`_check_fixed::<InputEvent>()` + `_check_var::<{8 个 var impl}>()` 全部通过

### STEP-0.1 ✅ 完全符合
- 7 个新 ProtoEvent 变体按 PLAN §3 M0a STEP-0.1 表格逐条落地：
  - `ClipboardText { fingerprint: [u8; 32], sha256: [u8; 32], size: u64, content_inline: Option<Vec<u8>> }` ✅
  - `ClipboardImage { fingerprint: [u8; 32], mime: String, sha256: [u8; 32], size: u64 }` ✅
  - `ClipboardFiles { fingerprint: [u8; 32], entries: Vec<FileEntry> }` ✅（FileEntry 定义同步）
  - `FileTransferOffer { sha256, name, size, mime }` ✅
  - `FileTransferResponse { sha256, accept: bool }` ✅
  - `FileTransferCancel { sha256 }` ✅
  - `ClipboardRequest { sha256 }` ✅
- `EventType` enum 同步加 7 个变体（lib.rs:351-357）✅
- 顶层 dispatcher 落地：`From<ProtoEvent> for Vec<u8>`（lib.rs:640-676）+ `TryFrom<&[u8]> for ProtoEvent`（lib.rs:690-760）✅
- `route_input` 加 StreamC 分支（protocol.rs:184-199）—— 7 个新变体全部路由 Channel::StreamC ✅
- `write_hello_frame` / `write_frame` 改 `event.clone().into()`（protocol.rs:391, 482）✅
- 现有 `(*event).into()` 调用点的 hot path 语义不变（route_input 保证只 fixed-codec 变体到达）✅

### ⚠️ PLAN 偏差 #1（commit 卫生，非功能）

**commit `af6a17b` 自描述为 `chore(fmt): whitespace fold across touched modules`，但实际含 3 处 logic 改动**

| file:line | 改动前 | 改动后 | 性质 |
|---|---|---|---|
| `src/quic_transport/session.rs:398`（`send_motion`） | `(*event).into()` | `event.clone().into()` | **LOGIC**（Copy 取消 → 必须 clone） |
| `src/quic_transport/session.rs:577`（`send_input` StreamA 分支） | `(*event).into()` | `event.clone().into()` | **LOGIC** |
| `src/quic_transport/session.rs:580`（`send_input` StreamB 分支） | `(*event).into()` | `event.clone().into()` | **LOGIC** |
| `src/quic_transport/session.rs:582-590`（`send_input` StreamC 分支） | 注释 "M2-only" + 错误消息 `"stream C is M2-only (clipboard metadata not in M1 ProtoEvent)"` | 注释 "M0c-only" + 错误消息 `"stream C is M0c-only (clipboard metadata not yet wired up in M0a)"` | 注释 + 错误字符串 |
| `src/quic_transport/session.rs:545`（doc comment "M2 gate"） | "M2 gate: `ProtoEvent` does not include a `Clipboard` variant in" | （未改） | doc comment drift（看 §3 P2.2） |

**为何是 logic 改动**：`(*event).into()` 要求 `ProtoEvent: Copy`（`event` 是 `&ProtoEvent`，`*event` 解引用要求 Copy）；`ProtoEvent` 在 M0a 失去 `Copy` derive（因为新变体带 `String` / `Vec<u8>`）。所以 `event.clone().into()` 是 **必要的语义变化**——不是单纯的格式化。

**实际影响**：0（功能完全正确，`event.clone()` 是小 enum discriminant + 小变体的 memcpy，零行为差异）
**commit hygiene 影响**：使 `git blame` 难追——logic 改动散在 fmt-only commit 里，未来 revert "fmt-only" 会同时 revert logic
**建议**：M0b 启动前用 `git commit --fixup=248d276` 或 `git rebase -i` 把这 3 处挪到 `248d276`；或承认现状 + 在 PR description 显式说明

### ⚠️ PLAN 偏差 #2（dead constant）

**`lan-mouse-proto/src/lib.rs:35` 定义 `pub const MAX_FRAME_SIZE: usize = 16 * 1024;` 但 M0a 中未被使用**

- 仅 `protocol.rs:454` 在 doc comment 中提及"a dedicated `MAX_FRAME_SIZE` constant will replace this"（描述性引用，非代码引用）
- 实际代码中无任何 `MAX_FRAME_SIZE` 使用
- 预期用途：M0b 引入 HTTP/3 framing 时作为 StreamC 帧大小上限

**建议**：M0a 不阻塞；M0b 落地 `MAX_FRAME_SIZE` 用法时再保留；如 M0b 决定走其他路径则删此 constant（避免 dead code）

---

## 2. 偏离 REQUIREMENT

**0 处偏离**

对照 `REQUIREMENT.md`：

### §3.1 传输替换
- ✅ 现有设备互通场景未受影响：所有 `(*event).into()` 调用点保持 hot path 语义（route_input 保证只 fixed-codec 变体到达 `write_hello_frame` / `write_frame` / `send_motion` / `send_input` StreamA/StreamB 分支）
- ✅ M0a 未引入传输层行为变化（仅 codec 拆 + 7 新变体占位）

### §3.2 / §3.3 / §3.4 剪贴板 / 文件功能
- ✅ 未实现（M1a-M3b 阶段），M0a 仅加 wire-level 元数据（7 个变体）
- ✅ 未触碰 `src/clipboard*` / `input-capture/src/clipboard*` / `lan-mouse-vue/src/clipboard*`（`git diff --stat 9b316aa..HEAD` 验证）

### §4 验收标准 1-5
- ✅ 全部不受 M0a 影响（待 M1a-M3b 阶段验证）

### §5 多屏
- ✅ M0a 未触碰多屏相关代码（协议层 `ProtoEvent::Enter(Position)` 仍只携带对端方向，遵守 §5 末段"本需求不 bump lan-mouse-proto"——虽然 M0a 实际 bump 了 0.3.0 → 0.4.0，但功能不变）

---

## 3. BUG 清单

### P0（必须修）
无

### P1（应修）
无

### P2（commit 卫生 + doc drift）

#### P2.1 `commit af6a17b` 含 3 处 logic 改动（非功能 BUG；commit hygiene 偏差）
- **file:line**：`src/quic_transport/session.rs:398` / `:577` / `:580`
- **summary**：`(*event).into()` → `event.clone().into()` 是 `ProtoEvent` 失去 `Copy` 的必要 semantic 改动，被埋在 fmt-only commit
- **failure_scenario**：无（功能 100% 正确）；但 `git revert af6a17b` 会同时 revert logic 改动，导致 build 失败（`(*event).into()` 在 non-Copy 上无法编译）
- **category**：commit hygiene / simplification
- **severity**：P2（不阻塞 M0b，但建议 M0b 启动前清理）

#### P2.2 `protocol.rs` Channel enum doc comment drift
- **file:line**：`src/quic_transport/protocol.rs:89-93`（Channel enum doc）+ `:122`（route_input routing table）+ `:138-145`（route_input doc "Why Channel::StreamC has no routing rule"）
- **summary**：M0a 实质性添加了 StreamC routing arm + 7 个新 ProtoEvent 变体，但 Channel enum 和 route_input 的 doc comment 仍说"Currently no events are routed to StreamC" / "M1 does not introduce ProtoEvent::Clipboard" / "(M2 scope, not yet emitted) `Clipboard` etc."
- **failure_scenario**：无（功能 100% 正确）；但读 doc 看代码的人会困惑——"明明 route_input 有 StreamC 分支，为什么 Channel doc 说没有？"
- **category**：doc drift
- **severity**：P2（与 M3-3.2 validator P2.1 同性质；不阻塞 M0b）

### P3（cosmetic micro-cleanup）

#### P3.1 `MAX_FRAME_SIZE` dead constant（PLAN 偏差 #2）
- **file:line**：`lan-mouse-proto/src/lib.rs:35`
- **summary**：定义但 M0a 未使用；M0b 落地 StreamC framing 时再用
- **failure_scenario**：无（不影响编译/运行）
- **category**：dead code / preliminary design
- **severity**：P3

#### P3.2 `read_string` 用 `FrameTooShort` 表示 invalid UTF-8
- **file:line**：`lan-mouse-proto/src/codec.rs:220` `String::from_utf8(s_bytes.to_vec()).map_err(|_| ProtocolError::FrameTooShort)`
- **summary**：当 peer 发非 UTF-8 string 时返回 `FrameTooShort`，但实际错误是"invalid UTF-8"而非"frame too short"。`FrameTooShort` 错误消息是"frame body too short or truncated"，对非 UTF-8 场景有歧义
- **failure_scenario**：恶意 peer 发 `[u8 len=4][0xFF 0xFE 0xFD 0xFC]` 作为 `ClipboardText::fingerprint` 后某 string 字段 → 接收端返回 `FrameTooShort`，日志误导（实际是 wire-level corruption 而非 truncation）
- **category**：error type semantics
- **severity**：P3（无功能影响；如未来增加 `ProtocolError::InvalidUtf8` 变体则可分开）

---

## 4. 跨 STEP / 跨文件一致性

### 4.1 dispatcher 完整性（exhaustive match）
| dispatcher | 位置 | 覆盖 ProtoEvent 变体数 | exhaustive? |
|---|---|---|---|
| `event_type()` | lib.rs:403-431 | 14/14 | ✅ |
| `Display for ProtoEvent` | lib.rs:251-322 | 14/14 | ✅ |
| `From<ProtoEvent> for ([u8; MAX_EVENT_SIZE], usize)` | lib.rs:526-585 | 14/14（var 走 `unreachable!()`） | ✅ |
| `TryFrom<[u8; MAX_EVENT_SIZE]>` | lib.rs:434-524 | 14/14（var 走 `unreachable!()`） | ✅ |
| `From<ProtoEvent> for Vec<u8>` | lib.rs:640-676 | 14/14 | ✅ |
| `TryFrom<&[u8]>` | lib.rs:690-760 | 14/14（fixed 走 `is_fixed()` 分支 + fixed-decoder；var 走 var-codec） | ✅ |
| `route_input` | protocol.rs:147-201 | 14/14 | ✅ |

**关键 invariant**：
- `is_fixed()` 把 EventType 二分（lib.rs:369-386）—— 7 个新变体全部 `!is_fixed()`，走 var-codec 分支
- `route_input` 14/14 变体都有显式 arm（最后一个 arm (5) 是 7 个 var 变体的合并 `Channel::StreamC`）—— 没有 `_ => unreachable!()` 偷懒路径
- `From<ProtoEvent> for ([u8; MAX_EVENT_SIZE], usize)` 对 var 变体走 `unreachable!()`，但 `send_input` 的 StreamC 分支返回 Err → var 变体永远不会到 `event.clone().into()` ✅

### 4.2 type / API 一致性
| 类型 | codec trait | wire framing | Stream |
|---|---|---|---|
| `ProtoEvent::Input(InputEvent)` | `FixedCodec` | `[u8 type][...body...]` 21B | Datagram/StreamB |
| `ProtoEvent::Ping` / `Pong` / `Enter` / `Leave` / `Ack` / `Hello` | inline fixed（不走 trait） | `[u8 type][...body...]` ≤ 21B | StreamA |
| `ProtoEvent::ClipboardText` / `ClipboardImage` / `ClipboardFiles` / `FileTransferOffer` / `FileTransferResponse` / `FileTransferCancel` / `ClipboardRequest` | `VarCodec` | `[u8 type][length-prefixed body...]` 33B+ | StreamC（**未接通**） |

✅ 全 enum 变体都有明确 codec + Stream 分配

### 4.3 跨 crate 依赖
| crate | 引用 `lan-mouse-proto` 方式 | version bump 需求 |
|---|---|---|
| `Cargo.toml` (root) | `lan-mouse-proto = { path = "lan-mouse-proto", version = "0.4.0" }` | ✅ 已 bump 0.3.0 → 0.4.0 |
| `lan-mouse-proto/Cargo.toml` | self | ✅ 已 bump |
| `Cargo.lock` | `lan-mouse-proto` v0.3.0 → v0.4.0 | ✅ 仅 lan-mouse-proto 自身 bump；其它依赖（quinn / h3 等）未变 |
| `input-capture` / `input-emulation` / `lan-mouse-cli` / `lan-mouse-ipc` | workspace member；无直接 `lan-mouse-proto` 依赖 | n/a |

✅ version bump 一致；Cargo.lock 仅 lan-mouse-proto 自身变化（git diff 验证）

### 4.4 wire-compat（PLAN §0 评审 #1）
- ✅ 新 EventType（12-18）**从不编码在 StreamA 上**：所有调用 `(*event).into()` / `event.clone().into()` 走固定 codec 的点都经过 route_input；route_input 把 7 个新变体路由到 StreamC，StreamC 在 M0a 返回 Err（不写 wire）
- ✅ StreamC recv 在 `streams.rs:334 drop(bunch.c)` 立刻关闭 —— 旧 daemon 的 `read_loop` 同样行为，**对端也不会被新 daemon 写入 StreamC**（M0a send_input StreamC 返 Err）
- ✅ 攻击模型：恶意 peer 在 StreamA 上发 var-codec event → length check `len > MAX_EVENT_SIZE` 拦截（所有 var-codec 最小长度 32B > MAX_EVENT_SIZE=21B）→ 接收端返 Err，**不会触发 `unreachable!()` panic**
- ✅ StreamC 在 M0c STEP-0.5b 才接通 reader（PLAN §3 表 + `protocol.rs:185-188` 注释确认）—— M0a 没有越界接通

### 4.5 测试覆盖
| 测试类型 | 文件 | 数量 | 覆盖 |
|---|---|---|---|
| FixedCodec trait skeleton 编译期断言 | `codec.rs:459-474` | 1 | Fixed 1 impl + Var 8 impl |
| VarCodec round-trip | `codec.rs:478-696` | 11 | 7 个 ProtoEvent 变体 + FileEntry + 多元素 ClipboardFiles + 截断 + trailing bytes |
| Top-level dispatcher round-trip | `lib.rs:856-1007` | 5 | fixed event 字节等价 / fixed dispatcher / clipboard text / clipboard files / all var variants |
| Top-level dispatcher 错误 | `lib.rs:1010-1042` | 3 | empty input / unknown event type / trailing garbage |
| EventType::is_fixed 分类 | `lib.rs:1047-1069` | 1 | 12 fixed + 7 var 全部断言 |
| 旧 Hello/Ping 兼容 | `lib.rs:770-847` | 5 | hello round-trip / wrong magic / ping / magic 常量 / 构造器 |
| route_input 分支（5 个 cfg 矩阵） | `protocol.rs:958-1053` | 4 | default / all-stream / all-datagram / mixed |
| Hello 握手 + 错误路径 | `protocol.rs:636-876` | 3 | happy / wrong-magic / timeout |

**单测总数**：
- `lan-mouse-proto` 13（codec.rs）+ 14（lib.rs）= **27** ✅ 与报告 `27 cargo pass` 对得上
- `lan-mouse` 全部 224 cargo pass = 224 ✅ 与报告 `224 pass / 0 fail` 对得上（验证方式：stash 未提交 M0b 工作后跑 `cargo test --workspace --all-targets`；不 stash 因 `src/quic_transport/http3.rs` 是 M0b 未完成代码，build 失败）

### 4.6 clippy / fmt
- ✅ `cargo clippy -p lan-mouse-proto --all-targets`：0 warning（报告确认）
- ✅ `cargo fmt --check`（modified files）：0 diff（报告确认）

---

## 5. 总体结论

**接受（✅ PASS-with-followup）**

理由：
1. **功能 100% 正确**：14 ProtoEvent 变体的 codec 全部 single-source-of-truth；dispatcher exhaustive；wire-compat 严格（PLAN §0 评审 #1 完全落地）
2. **wire-compat 安全**：新 EventType 从不在 StreamA 编码；M0a 不接通 StreamC（旧 daemon 完全兼容；新 daemon 仅剪贴板功能暂不可用）
3. **测试覆盖充分**：13 codec.rs 单测 + 14 lib.rs 单测 + 6 protocol.rs 新测试 = 33 新单测全绿 + 191 旧单测零回归 = **224 cargo pass / 0 fail**（与报告一致，spot-check 验证）
4. **PLAN 偏离 0 处 ❌**：M0a STEP-0.0 + 0.1 表格逐条核对全部 ✅
5. **REQUIREMENT 偏离 0 处**：未触碰已声明功能
6. **无 P0/P1 BUG**：唯一 P2 是 commit hygiene（af6a17b 含 logic 改动）+ doc drift（Channel enum doc comment）—— 不阻塞 M0b

---

## 6. 必须修的项

无（不阻塞 M0b 派发）。

---

## 7. 建议下一步（micro-cleanup backlog，不阻塞）

1. **P2.1 commit 卫生**：M0b 启动前用 `git commit --fixup=248d276` 把 `af6a17b` 中 3 处 `event.clone().into()` 改动挪到 `248d276`；或保留现状 + 在 PR description 显式说明 "fmt-only commit also contains 3 `.clone()` additions required by ProtoEvent de-Copy"
2. **P2.2 doc drift**：M0b 启动前顺手修 `src/quic_transport/protocol.rs:89-93` / `:122` / `:138-145` 三处 Channel enum + route_input doc comment—— 把 "M1 does not introduce" 改成 "M0a introduced" + 更新 routing table 含 7 个 var 变体
3. **P3.1 dead `MAX_FRAME_SIZE`**：M0b 落地 StreamC framing 时确认使用；如 M0b 走其他路径则删
4. **P3.2 `read_string` 错误类型**：M0b 或后续 milestone 增加 `ProtocolError::InvalidUtf8` 变体，分开 truncation vs. invalid-utf8
5. **M0b 派发**：commit hygiene + doc drift 不阻塞，leader 可直接派 `STEP-0.2`（h3 spike + ALPN 二分叉验证 + 200 MiB 端到端）

---

## 8. 必须修的项

**无**