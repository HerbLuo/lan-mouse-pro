# SUGGESTION — 永久不执行

> 由 plan-step-executor 维护：触发 STEP / 现象 / 决策理由 / 决策 STEP

---

## #1 — pre-existing fmt + clippy 噪音（与 M1 milestone 完全无关）

- **触发 STEP**：M1 / STEP-1.3 首次识别，STEP-1.4 决策
- **现象**（commit `828cc51 init` 起即存在，与本计划 M1/M2/M3/M4 任何 milestone 均无关联）：
  - **fmt diff（24 处）**：
    - `input-emulation/src/macos.rs:428, 457`（2 处注释对齐）
    - `src/config.rs:613`（1 处字段对齐）
    - `src/quic_transport/protocol.rs:576, 652, 735, 756, 803, 819`（6 处 log!/assert 多行 + Duration 缩进）
    - `src/quic_transport/session.rs:368, 501, 792, 799, 1112, 1145, 1228, 1320, 1330, 1434`（10 处 log! 多行 / doc-comment 缩进）
    - `src/quic_transport/streams.rs:650`（1 处）
    - `src/quic_transport/tls.rs:71, 790`（2 处）
    - `tests/quic_smoke.rs:189, 311`（2 处）
  - **clippy warning（7 个，`-D warnings` 下变 error）**：
    - `src/connect.rs:727, 728` `doc_lazy_continuation`
    - `src/connect.rs:1246, 1252` `assertions_on_constants`（应在 const block 中）
    - `src/quic_transport/endpoint.rs:238` `doc_lazy_continuation`
    - `src/quic_transport/endpoint.rs:339` `too_many_arguments`（8/7，`dial_any`）
    - `src/quic_transport/session.rs:760` `doc_lazy_continuation`
- **决策理由**：PLAN §0 scope discipline + Leader 指示 "不动 `quic_smoke` / `quic_transport` 等与 M1 完全无关的 pre-existing 噪音"。这些都属于：
  - **QUIC 传输层**（`quic_transport/*` + `tests/quic_smoke.rs` + `src/connect.rs`）—— 计划后续用独立 PR 处理 QUIC idle/heartbeat 健壮性（关联 SUGGESTION #3）+ 一次性 lint cleanup
  - **input-emulation macOS 后端**（`input-emulation/src/macos.rs`）—— 不属于本计划任何 milestone
  - **main crate config 层**（`src/config.rs`）—— M3 改 `ClientConfig.monitor` 时一起处理（PLAN §M3 STEP-3.1）
- **触发 fix 的建议时机**：QUIC idle timeout 调查 PR / PLAN-M2 启动前 / 任意后续 cleanup PR
- **决策 STEP**：M1 / STEP-1.4

### 当前工具链补充（2026-09-09）

本机当前 Clippy 还会在同一批 pre-existing 代码上报告：

- `src/service.rs:105-109`：`doc_lazy_continuation`（`incoming_clipboard` 生命周期列表）
- `src/connect.rs:1692,1698`：`assertions_on_constants`

这些位置均早于本次 M1b.1 改动，继续按本条永久忽略，留给独立 lint cleanup。
