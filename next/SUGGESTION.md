# SUGGESTION — 当前活跃问题

> 由 plan-step-executor 维护：触发 STEP / 现象 / 建议 / 优先级 🟠🟡⚪
> 已解决 → `SUGGESTION-FIXED.md`；明确不修 → `SUGGESTION-IGNORE.md`

---

## #3 — pre-existing QUIC smoke test flake `connection_survives_ten_seconds_of_silence`

- **触发 STEP**：M1 / STEP-1.3（首次观察到；与本 STEP 无关）
- **现象**：`cargo test --workspace` 时 `tests/quic_smoke.rs::connection_survives_ten_seconds_of_silence` 失败，断言 "server-side connection must remain alive after 10s silence"。`git stash` 后在 commit `828cc51 init`（STEP-1.1 之前）跑同样失败 → 确认 pre-existing。
- **建议**：单独排查 QUIC idle timeout / ping-pong 心跳链路是否在 10s 静默期被 QUIC endpoint 主动关闭。本 STEP-1.3 不修（不在 capture/service/client 三层范围内）。建议作为单独 PR 处理。
- **优先级**：🟠（QUIC 协议层健壮性，不影响 M1 milestone；但 M2+ 真机拔插测试需要稳定的 server-side connection）
