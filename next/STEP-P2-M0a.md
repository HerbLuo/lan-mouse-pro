# STEP-P2-M0a — ProtoEvent codec 双轨化

> **状态**：✅ 完成（leader 接力 commit；executor 撞 token plan 上限 429 撞在最后阶段）
> **撞 429 模式**：与 PLAN-1 M3.3 / M3.5 同——code 改完但 executor 没写报告 + 没 commit → leader 接力

## 范围（合并 STEP-0.0 + STEP-0.1）

按 PLAN-2 §3 M0a 表格 STEP-0.0 + 0.1 合并执行：
- **0.0**：codec.rs 双轨化骨架 + 设计文档
- **0.1**：7 个新 ProtoEvent 变体 + EventType 同步 + VarCodec 实现

## 实际改动

| 文件 | 改动 |
|---|---|
| `lan-mouse-proto/Cargo.toml` | version `0.3.0` → `0.4.0`（bump 主版本） |
| `Cargo.toml` | `lan-mouse-proto` 依赖版本同步 `0.3.0` → `0.4.0` |
| `Cargo.lock` | lock 文件同步 |
| `lan-mouse-proto/src/codec.rs`（新） | `FixedCodec` + `VarCodec` trait 定义 + dispatcher；详尽 doc-comment 解释为什么拆两个 trait（hot path zero-alloc vs 大字节变长） |
| `lan-mouse-proto/src/lib.rs` | 加 `ClipboardText` / `ClipboardImage` / `ClipboardFiles` / `FileTransferOffer` / `FileTransferResponse` / `FileTransferCancel` / `ClipboardRequest` 7 个新变体；`EventType` 同步；`From<ProtoEvent> for Vec<u8>` + `TryFrom<&[u8]> for ProtoEvent` 由 `match` 分流；**现有 `(*event).into()` 调用点不动**（Input/Ping/Pong/Enter/Leave/Ack/Hello 仍走 Fixed hot path） |
| `src/quic_transport/protocol.rs` | `route_input` 加 StreamC 分支（7 新变体路由到 Channel::StreamC）；`write_hello_frame` + `write_frame` 改 `event.clone().into()`（ProtoEvent 不再 Copy，因新变体带 `String`/`Vec<u8>`）+ 详尽 doc-comment 解释为何仍走 fixed-size |
| `src/quic_transport/session.rs` | 配套调整 + fmt |
| `input-emulation/src/macos.rs` | fmt-only（缩进调整） |
| `src/config.rs` | fmt-only |
| `src/quic_transport/streams.rs` | fmt-only |
| `src/quic_transport/tls.rs` | fmt-only |
| `tests/quic_smoke.rs` | fmt-only |

**未 commit**（用户本地修改，leader 不 commit）：
- `.claude/agents/plan-step-executor.md`
- `AGENTS.md`
- `next/.LEADER.md`

## 测试结果

- `cargo build -p lan-mouse-proto`：✅ 0 error
- `cargo build --workspace`：✅ 0 error（9.58s）
- `cargo test --workspace`：**224 pass / 0 fail**（101 + 72 + 7 + 2 + 15 + 27 + 0 doc-tests）
- `cargo clippy -p lan-mouse-proto --all-targets`：✅ 0 warning
- `cargo fmt --check` (modified files)：✅ 0 diff
- `Cargo.lock`：合理 bump（仅 lan-mouse-proto 0.3.0 → 0.4.0）

## PLAN 偏差

**0 处**（与 PLAN §3 M0a STEP-0.0+0.1 表格完全一致）

## Wire-compat 验证（PLAN §0 评审 #1）

- ✅ 新变体**全部走 StreamC**（`protocol.rs::route_input` 显式路由）
- ✅ 现有 `(*event).into()` 调用点不动（保留 hot path）
- ✅ StreamC reader 仍在 M0c STEP-0.5b 接（leader 接力 commit 时已确认 StreamC reader 还没接通——send 走 StreamC 但对端旧 daemon 没 reader → 旧 daemon 默默 drop，新 daemon 在 stream A 上键鼠互通）
- ✅ 没碰 stream A 编码新 EventType（旧 daemon 不会触发 `EventType::try_from(InvalidEventId)` 断链）

## 累计耗时

~45 min（executor 撞 429 撞在最后阶段，约 30 min 落地代码 + leader 接力 build/test/commit 约 15 min）

## 下一步

→ commit（leader 拆 fmt-only / 真逻辑 commit）→ 派 M0b STEP-0.2 h3 spike（路径 1 / 路径 2 二分叉验证 + 200 MiB 端到端 + cancel + 拔网）
