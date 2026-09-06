# SUGGESTION — 已解决

> 由 plan-step-executor 维护：触发 STEP / 现象 / 解决方案 / 解决 STEP
> 重新激活的项 → `SUGGESTION.md`

---

## #3 — pre-existing QUIC smoke test flake `connection_survives_ten_seconds_of_silence`

- **触发 STEP**：M1 / STEP-1.3（首次观察到；与本 STEP 无关）
- **现象**：`cargo test --workspace` 时 `tests/quic_smoke.rs::connection_survives_ten_seconds_of_silence` 失败，断言 "server-side connection must remain alive after 10s silence"。`git stash` 后在 commit `828cc51 init`（STEP-1.1 之前）跑同样失败 → 确认 pre-existing。
- **根因**：`src/quic_transport/tls.rs::default_transport_config` 把 `keep_alive_interval` 硬编码 5s；测试 fixture（`server_endpoint` helper / 两个 `dial` 调用点）把 `max_idle_timeout` 也传 5s。QUIC 协议层 idle-timeout 检查与 keep-alive PING 都在 t=5s 触发 → race。t=11s 时 server-side `closed()` 已 ready，断言挂。生产 `config.rs::quic_idle_timeout` 默认同样 5s（2026-09-04 从 10s 调下来），但被 `connect.rs::pong_health_watchdog`（1.5s 阈值）覆盖，主链路不受影响 —— 只是 QUIC 协议层兜底兜不住。
- **解决**（out-of-scope cleanup，方案 A：只动测试）：
  - `tests/quic_smoke.rs:71 / 192 / 314` 三处 `std::time::Duration::from_secs(5)` → `std::time::Duration::from_secs(30)`。30s 与该测试 doc-comment 顶部 `keep_alive_interval = 5s, max_idle_timeout = 30s` 的描述一致（注释说"we use 10 s (well below 30 s) to assert the upper bound"，实际值原本就该 ≥ 30s）。
  - 测试结果：`cargo test --workspace --test quic_smoke` → 2 passed；`connection_survives_ten_seconds_of_silence` 11.01s 完成。
  - 不动生产路径（`tls.rs` / `config.rs` / `connect.rs`）—— 留给后续 cleanup PR（与 SUGGESTION-IGNORE.md #1 同批次）。
- **解决 STEP**：out-of-scope cleanup（不在任何 PLAN STEP 内；M2 STEP-2.1 主线未被打断）

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
