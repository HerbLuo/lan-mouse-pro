# SUGGESTION — 已解决

> 由 plan-step-executor 维护：触发 STEP / 现象 / 解决方案 / 解决 STEP
> 重新激活的项 → `SUGGESTION.md`

---

## #1 — STEP-1.1 把 backend sig-only 兼容层提前到本步（PLAN 偏差）

- **触发 STEP**：M1 / STEP-1.1
- **现象**：PLAN §M1 STEP-1.1 "涉及文件" 只列 `geometry.rs`、`lib.rs`，backend 改动划到 STEP-1.2。但 `Capture` trait 是 crate 内 trait，sig 改了 backend 不动就连 `input-capture` crate 都编不过——违反 STEP-1.1 完成标志中"公共 API 签名变更 + crate 编过"的组合。
- **解决**：STEP-1.2 按 PLAN 完成 5 backend 内部完整迁移到 `BarrierKey`（dummy / libei / layer_shell / macos / windows；x11 stub 跳过）。每个 backend 的内部 producer-event 通道 / `event_rx` 通道 / thread-local 状态 / `Stream::Item` 现在都直接以 `BarrierKey` 为键，不再做"边界 lift"。`monitor / offset / span` 全程 `None / 0 / 10000` 默认值（与 STEP-1.1 默认兼容层等价）；backend 行为零差异。
- **解决 STEP**：M1 / STEP-1.2

---

## #2 — M1 milestone close：capture.rs pre-existing fmt + clippy 噪音（M1 范围）

- **触发 STEP**：M1 / STEP-1.3（识别，留给 STEP-1.4）
- **现象**：`cargo fmt --check src/capture.rs` 4 处 pre-existing diff（log! 宏多行 / Pending release_capture doc-comment 缩进）+ `cargo clippy` 1 个 pre-existing warning（`capture.rs:797` `if !alive collapsible into outer match`）。
- **解决**：
  - `cargo fmt -p lan-mouse -- --check` → surgical 应用 `rustfmt --edition 2021` 到 `src/capture.rs`（仅 M1 文件）
  - `capture.rs:797` `ProtoEvent::Pong(alive) { if !alive { … } }` → `ProtoEvent::Pong(false) { … }`（移除 `alive` binding，直接 pattern match 字面值）
- **解决 STEP**：M1 / STEP-1.4
- **未解决部分（已转移到 SUGGESTION-IGNORE.md #1）**：workspace 其余 24 处 fmt diff + 7 个 clippy warning 全部在非 M1 文件（QUIC / input-emulation / config），按 PLAN §0 scope discipline 不在 M1 close 范围内。
