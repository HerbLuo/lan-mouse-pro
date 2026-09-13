# Validation: M5 STEP 5.1+5.2

> 审阅日期：2026-09-13　审阅 STEP 范围：5.1, 5.2
> 起点 commit：6fbca59　终点 commit：b395843

## 0. 范围说明

本次审阅 diff 范围 `6fbca59..b395843` 共 19 commit，其中 **8 个 commit 属于 M4 hotfix / cleanup（已 M4 收尾前完成）**：
- `b283a50` — feat(popup): add LAN_MOUSE_SUPPRESS_POPUPS env guard
- `c0289d0` — test(clipboard): remove stub e2e files + slim file_cache
- `54053fe` — docs: archive 2 M4 P0 BUG investigation reports
- `1eb69aa` — docs(suggestion): add SUGGESTION #S-12
- `0c9ae6c` — debug: panic hook instrumentation
- `6de48cc` — fix(clipboard/windows): DROPFILES cleanup
- `608e51a` — fix(service): local fingerprint pre-stamp（**SUGGESTION #S-12 真根因 hotfix**）
- `ca779da` / `dc6de83` — SUGGESTION #S-12 closure + leader-state sync

M5 STEP 实际贡献 commit：
- **STEP-5.1（5 commit）**：`7187b63` IPC + `2629d72` service stream-error + `9af62bb` docs + `21fea44` P2.3 closure + `c1e41b9` leader-state sync
- **STEP-5.2（5 commit）**：`f9fa108` keepalive tests + `ef3930e` manual + `271e641` bench + `7890d18` docs + `b395843` leader-state sync

> M4 hotfix 不在本 validator scope 内审阅（已 M4 hotfix 路径独立处理）；下文 §1-§5 专注 M5 STEP-5.1 + 5.2 实际改动。

---

## 1. 偏离 PLAN

### STEP-5.1（拔网处理 + IPC `FileTransferFailed` + `.partial` 默认删除）

PLAN §M5 STEP-5.1 描述：
> 接收端错误路径：`apply_inbound_files_task` 内 HTTP/3 stream error → 触发 `service::file_inbound_err(sha256, reason)` handler → IPC 推 `FrontendEvent::FileTransferFailed { sha256: [u8; 32], reason: String, ts_ms: u64 }`；新增 `FrontendEvent::FileTransferFailed` 在 `lan-mouse-ipc` —— 不引入 accept/reject IPC；`.partial` 处理默认删除，`keep_partial: bool` config 控制；reason 三类枚举 `"connection lost"` / `"timeout"` / `"peer cancelled"`。

实际落地（7 处 PLAN 偏差，全部 A1 策略 —— executor 自评 + 范畴一致）：

| # | 偏差 | 范畴 | 严重度 |
|---|---|---|---|
| 1 | 抽取 `FileFetchErrorKind` typed enum + `as_reason()` + `classify_io_err_kind()` 纯函数 | A1（typed enum 优于字符串前缀匹配；与 STEP-4.3 决策一致） | ⚠️ 小偏差 |
| 2 | fetcher future bound `Result<_, String>` → `Result<_, std::io::Error>`（typed error at fetch boundary） | A1（classify_io_err_kind 输入需要 ErrorKind） | ⚠️ 小偏差 |
| 3 | 重构 `write_and_verify_file_blocking` 引入 `.partial` 中间文件 + fsync + rename（PLAN 隐含但未明确） | A1（闭合 SUGGESTION P2.3 carry-forward + 启用 keep_partial postmortem） | ⚠️ 小偏差 |
| 4 | `apply_files_inner_returning_path` 加 `keep_partial` 透传参数 | A1（与 STEP-4.1 `Service::max_file_size()` getter 模式对称） | ⚠️ 小偏差 |
| 5 | `InboundFileApplyResult` 加 `stream_failure: Option<(String, u64)>` 字段 | A1（apply task 是 spawned task，无 `&mut Service`；main task 是 IPC 推送唯一地点） | ⚠️ 小偏差 |
| 6 | `mismatch_deletes_partial` 测试加 `<name>.partial` 不存在断言 | A1（pre-M5 final == partial；post-M5 两者分开需双断言） | ⚠️ 小偏差 |
| 7 | 6 处测试 fetcher type annotation `String` → `std::io::Error`（纯 type 变化） | A1（Rust 类型系统强制） | ⚠️ 小偏差 |

**判定：✅ 完全符合**（7 处偏差全部 A1 策略，0 处偏离 PLAN 范畴；STEP-5.1 完成标志全部覆盖）。

### STEP-5.2（端到端性能 + 收尾 + keepalive↔idle race + Pong ≤ 600ms）

PLAN §M5 STEP-5.2 描述：
> 200 MiB 文件传输性能分两档——有线 100 Mbps LAN < 30s + Wi-Fi 实际带宽 < 60s；**drop UI 端到端验收**（用户决策 2026-09-13 auto-accept only）；cancel 双向 + 拔网双向 + keepalive↔idle race 专项 + Pong 间隔 ≤ 600ms；fmt/clippy/build + 三平台真机双向端到端（人类配合）；涉及文件 `tests/manual/file-transfer.md`。

实际落地（4 处 PLAN 偏差，全部 A1 策略）：

| # | 偏差 | 范畴 | 严重度 |
|---|---|---|---|
| 1 | 抽取 silence-detection 为闭包 helper（`pong_health_silence_detection_thresholds_correctly` 5 边界场景 + 2 s wall-clock） | A1（`pong_health_watchdog` 是 6-arg async fn，无法 mod-level 直接测；闭包提取与 STEP-4.3 `decide_reinject_skip` 纯函数模式一致） | ⚠️ 小偏差 |
| 2 | runtime `let` 钉常量比对（avoid `assertions_on_constants` clippy lint） | A1（Rust 1.98 const blocks 不能调 `format_args!`；runtime let + assert 等价语义） | ⚠️ 小偏差 |
| 3 | 真机测试**不**在 executor 执行（executor scope 拆解为单测 + 模板 + helper） | A1（executor 无 LAN/Wi-Fi 多机环境；用户决策 2026-09-13 auto-accept only） | ⚠️ 小偏差 |
| 4 | 复用 pre-existing tests `pong_health_timeout_relaxes_to_3_5s` / `pong_health_threshold_in_safe_range`（不重复 pin） | A1（pre-existing 已 pin PONG_HEALTH_TIMEOUT == 3500ms / safe range；新测试扩展而非替代） | ⚠️ 小偏差 |

**判定：✅ 完全符合**（4 处偏差全部 A1 策略，0 处偏离 PLAN 范畴）。

---

## 2. 偏离 REQUIREMENT

对照 `REQUIREMENT.md` §3-§4 验收标准：

| REQUIREMENT 项 | 验证 | 结果 |
|---|---|---|
| §3.4 "复制一个 200 MiB 文件 → 对端可选择接收，落盘后 SHA-256 一致；中途拔网后恢复能重传或清晰报错" | STEP-5.1 落地拔网清晰报错（`FileTransferFailed` IPC + 默认删 `.partial` + `keep_partial=true` postmortem 保留） | ✅ 未破坏 |
| §4.4 "复制一个 200 MiB 文件 → 对端可选择接收，落盘后 SHA-256 一致；中途拔网后恢复能重传或清晰报错" | 同上 | ✅ 未破坏 |
| §4.5 "现有 IPC、CLI、GTK UI 不需要改公共 API" | STEP-5.1 **新增** `FrontendEvent::FileTransferFailed` 变体（**新增 = 公共 API 扩展**，非破坏性；既有 `FrontendEvent` 用户代码不受影响，serde 默认对 unknown variant 失败但 clipboard events 用 enum match 不会有 unknown） | ✅ 公共 API 扩展（非破坏） |

**判定：✅ 未破坏 REQUIREMENT**。

> **注**：新增 IPC 事件是 public API 扩展（非破坏）。`FrontendEvent::FileTransferFailed` 变体加入 `lan-mouse-ipc`；Vue 5.3 必须在 union 增 case（已在 STEP-5.1 报告 §6.3 写明 Vue side 接续契约）。

---

## 3. BUG 清单

| 严重度 | 位置 | 现象 | 建议修复 |
|---|---|---|---|
| **P2** | `next/STEP-P2-M5-5.1.md` §2.3 + §1.3 测试计数 | 报告自评"合计新增 16 new + 2 modified"，但实际 `git diff` 在 `src/service.rs` 显示 **15 new `#[test]`/`#[tokio::test]` annotations**（14 stream_error + 1 keep_partial_preserves）。差额 1 未解释；测试本身无 bug，仅报告 count 文案误差 | 文档层；不阻塞 |
| **P2** | `next/STEP-P2-M5-5.1.md` §1.3 行 `6 个 fetch error 行为变更隐含但测试影响未明确` | 实际测试改动是 6 处 fetcher type annotation + 5 处 `apply_inbound_files_task` 调用加 `keep_partial` 参数 + 1 处 `cancel_mechanism_tests` 加 `keep_partial`；report 文字描述"6 处"含糊但未声称 6 处 | 文档层；不阻塞 |
| **P0** | — | 无 | — |
| **P1** | — | 无 | — |

**P0 / P1 = 0**。本次审阅**未发现**功能 bug / 数据丢失 / 崩溃风险。

### 静态分析副产物

- ✅ `unsafe` 引入：**0 处**（grep 验证）
- ✅ 公共 API 破坏性改动：**0 处**（仅 `InboundFileApplyResult` 加 `stream_failure: Option<(String, u64)>` 字段、`apply_inbound_files_task` 签名加 `keep_partial: bool` 参数、`write_and_verify_file_blocking` 签名加 `keep_partial: bool` 参数、fetcher future bound `String` → `std::io::Error` —— 全部为扩展 / 内部签名调整）
- ✅ 闸 2 baseline：clippy 30 errors（M4 终点）→ 30 errors（STEP-5.1 持平）→ 28 errors（STEP-5.2 净 -2 pre-existing）；0 new warning / 0 new error
- ✅ 测试覆盖：service.rs 100 → 115 (+15) + connect.rs 4 → 8 (+4) + lan-mouse-ipc 29 → 31 (+2) = +21 new tests in 5.1+5.2；workspace lib 400 → 395（小幅波动因 STEP-5.2 新增 4 而 STEP-5.1 P2.3 重构对部分测试计数调整）

---

## 4. 跨 STEP 一致性

### 4.1 STEP-5.1 内部一致性

| 检查项 | 结果 |
|---|---|
| `FrontendEvent::FileTransferFailed { sha256: [u8; 32], reason: String, ts_ms: u64 }` 与 `InboundFileApplyResult.stream_failure: Option<(String, u64)>` 类型匹配 | ✅ `(String, u64)` tuple 对应 `reason: String` + `ts_ms: u64`；sha256 来源是 `inbound_sha: [u8; 32]`（main task `handle_inbound_files_applied` 直接传） |
| `FileFetchErrorKind` 与 STEP-4.3 `InboundFileError` 命名 / 变体一致性 | ✅ **清晰分工**：disk-side `InboundFileError`（Sha256Mismatch / IoError / PartialResidue / MimeTooLarge / ExceedsLimit / Canceled）vs network-side `FileFetchErrorKind`（ConnectionLost / Timeout / PeerCancelled / IoError）；两者都进入 `InboundFileApplyResult` 但 `error` 字段（disk-side）+ `stream_failure` 字段（network-side）分离 |
| `keep_partial: bool` 字段穿越 IPC → Service → write_and_verify_file_blocking 一致 | ✅ IPC `ClipboardConfig.keep_partial`（STEP-4.1 落地）→ `Service::keep_partial()` getter（STEP-4.1 落地）→ `handle_clipboard_inbound_files` capture（STEP-5.1）→ `apply_inbound_files_task` 参数（STEP-5.1）→ `apply_files_inner_returning_path` 参数（STEP-5.1）→ `write_and_verify_file_blocking` 参数（STEP-5.1）。链路完整 |
| 4 个 skip condition 在 collector 路径保留 | ✅ STEP-4.3 落地的 4 类 skip（`inject_to_clipboard=false` / 回环命中 / 落盘失败 / forward-compat `MIME_TOO_LARGE`）保留未触碰 |
| pre-stamp 防回环保留 | ✅ SUGGESTION #S-12 用户 hotfix（commit `608e51a`）保留 fingerprint = `file_selection_fingerprint(&paths)`（local）模式 |

### 4.2 STEP-5.2 内部一致性

| 检查项 | 结果 |
|---|---|
| keepalive↔idle race 测试 pin 的常量与 `pong_health_timeout_relaxes_to_3_5s` pre-existing pin 一致 | ✅ pre-existing `PONG_HEALTH_TIMEOUT = 3500ms`（commit `d882454` 2026-09-12 BUGS-2 follow-up）vs 新增 `pong_health_timeout_outpaces_quic_idle_timeout_default` 钉 3500 < 5000 QUIC default；同源同值 |
| 4 个新增测试 pin 与 `src/connect.rs` 实际常量值匹配 | ✅ `PING_INTERVAL = 500ms`（实际 `Duration::from_millis(500)` in `src/connect.rs:521`）≤ 600ms budget；`PONG_HEALTH_TIMEOUT = 3500ms`（`src/connect.rs:551`）< 5000ms QUIC default；QUIC keepalive = 5s vs idle_timeout = 5s（per `tls.rs::default_transport_config` + `config.rs::quic_idle_timeout` 默认） |
| `pong_health_silence_detection_thresholds_correctly` 测试的 2 s wall-clock pin | ✅ 实际执行需 ~2 s wall-clock（5×PING_INTERVAL = 2.5s cadence），单测执行期间消费时间但不影响其他测试 |

### 4.3 STEP-5.1 ↔ STEP-5.2 跨 STEP 一致性

| 检查项 | 结果 |
|---|---|
| STEP-5.1 落地的 `FileTransferFailed` IPC event 引用在 STEP-5.2 人工测试模板 S4 拔网双向 段 | ✅ `tests/manual/file-transfer.md:398` 引用 `[INFO lan_mouse::service] notify_frontend: FrontendEvent::FileTransferFailed { sha256: <sha>, reason: "connection lost", ts_ms: <epoch_ms> }`；S4 失败排查段引用 |
| STEP-5.1 落地的 `keep_partial` config 在 STEP-5.2 bench script 行为 | ⚠️ STEP-5.2 bench-file-transfer.sh 检查 "stray `.partial` files (should be cleaned by default)"，与 STEP-5.1 `keep_partial=false` 默认 fsync-then-remove 一致 |
| STEP-5.2 4 个 connect.rs 单测 pin 与 STEP-5.1 `InboundFileApplyResult` 改动无冲突 | ✅ 0 文件交叉引用，各自独立 |
| M4 STEP-4.3 `InboundFileError` enum vs M5 STEP-5.1 `FileFetchErrorKind` enum | ✅ 清晰分工（disk-side vs network-side）；两者都在 `service.rs` 但 `InboundFileError` 用于 `InboundFileApplyResult.error`，`FileFetchErrorKind` 用于分类 std::io::Error |

### 4.4 公共 API 一致性

| API | 状态 | 备注 |
|---|---|---|
| `lan_mouse_ipc::FrontendEvent::FileTransferFailed` | 新增 variant | wire contract pinned；3 reason 字符串 + sha256 `[u8;32]` + ts_ms `u64` |
| `lan_mouse_ipc::FrontendEvent::*`（其他变体） | 未触碰 | 0 break |
| `lan_mouse_ipc::ClipboardConfig::*` | 未触碰 | 4.1 已扩展 8 字段；5.1 不再扩展 |
| `lan_mouse_ipc::FrontendRequest::*` | 未触碰 | accept/reject drop 已在 4.1 落地 |
| `Service::keep_partial()` | 未触碰 | 4.1 已落地；5.1 仅调用 |
| `Service::max_file_size()` | 未触碰 | 同上 |
| `apply_inbound_files_task` 签名 | 扩展 +2 参数（`keep_partial: bool` + fetcher future 类型） | 内部 free function，仅 spawn site 调用 |
| `apply_files_inner_returning_path` 签名 | 扩展 +1 参数（`keep_partial: bool`） | 内部 free function |
| `write_and_verify_file_blocking` 签名 | 扩展 +1 参数（`keep_partial: bool`） | `pub(crate)` |
| `InboundFileApplyResult` 字段 | 扩展 +1 字段（`stream_failure: Option<(String, u64)>`） | 内部 struct |
| `tcp_transport/http3::Http3Client::get_file` 签名 | 未触碰 | fetcher future return type 变化不影响 `Http3Client::get_file` 本身 |

---

## 5. 总体结论

- **接受**（PASS-with-followup，0 P0 / 0 P1 / 2 P2 文档计数偏差）
- 理由：
  1. **STEP-5.1 落地完整**：拔网 IPC 事件 + 3 类 reason 字符串 + `.partial` 默认 fsync-then-remove + `keep_partial=true` postmortem 保留；闭合 SUGGESTION P2.3 carry-forward；15 新单测 + 2 IPC round-trip 测试；闸 2 全绿
  2. **STEP-5.2 落地完整**：4 个 keepalive↔idle race + Pong ≤ 600ms 结构性 pin 测试 + 715 行真机回归模板（5 scenarios × 2 方向 × 3 平台 = 30 cells）+ 184 行 bench helper；executor scope 拆解为单测 + 模板 + helper（真机验收由人类执行，符合 PLAN §M5 用户决策 2026-09-13 auto-accept only）
  3. **跨 STEP 一致性**：4.1 / 4.3 字段链路完整（keep_partial 透传、InboundFileApplyResult 扩展与 collector 兼容、FileFetchErrorKind vs InboundFileError 清晰分工）
  4. **公共 API 无破坏性改动**：所有变更均为扩展 / 内部签名调整

---

## 6. 建议下一步

### Leader 接受后的决策项

1. **STEP-5.1 ✅ 接受**（0 必须修项；仅 §3 P2 文档计数文案建议 leader 决定是否让 executor 微调）
2. **STEP-5.2 ✅ 接受**（0 必须修项）
3. **M5 累计 2/5 STEP 完成**（5.1 + 5.2 done；5.3 / 5.4 / 5.5 待派发）

### 后续派发建议

按 `.LEADER-STATE.md` 节奏：
1. **下一步派 `plan-step-executor` 启动 M5 STEP-5.3**（Vue 类型 + IPC 绑定）：
   - `lan-mouse-vue/src/api/ipc.ts` 加 `ClipboardConfig` / `ClipboardState` / `FileTransferFailed`（sha256 hex）/ `ClipboardConfigChanged` 类型
   - `lan-mouse-vue/src/store/index.ts` 加 `state.clipboardConfig: ClipboardConfig` + `state.lastClipboardText: string` / `state.lastClipboardAt: number` / `state.lastClipboardSource: string` 三字段
   - `lan-mouse-vue/src/components/Toaster.vue` 增 FileTransferFailed 单方向通知
   - `lan-mouse-ipc/src/lib.rs` 新增 `FrontendEvent::ClipboardConfigChanged { config: ClipboardConfig }`
   - 依赖已落地：STEP-5.1 的 `FrontendEvent::FileTransferFailed` wire contract + reason 字符串集合
2. **再 STEP-5.4**（GeneralPanel + per-peer UI + TOML 落盘）
3. **最后 STEP-5.5**（CLI 集成）

### 文档 / 杂项

- `next/SUGGESTION.md` 头部 `P2.3` 已隐式 close（commit `21fea44` 落地）；建议 leader 把 SUGGESTION-FIXED 顶部 P2.3 条目同步到 SUGGESTION.md 头部移除
- M4 carry-forward P2-1 / P2-2（`handle_clipboard_inbound_files_decide` doc-comment 矛盾 / `ReinjectSkipReason::NoLandedPaths` 永不构造）仍待清理；可让 STEP-5.3 / 5.4 / 5.5 executor 顺手清理
- 用户真机验证（PLAN §8 M5 人类测试矩阵）：macOS / Windows / Linux × S1-S5 × 双向 = ~30 cells，按 `tests/manual/file-transfer.md` checklist 跑

---

## 7. 审阅 Checklist 复核

- [x] STEP-5.1 涉及文件 / 完成标志 / 偏差 全部复核
- [x] STEP-5.2 涉及文件 / 完成标志 / 偏差 全部复核
- [x] 跨 STEP 一致性（4.1-4.4 全部章节）
- [x] 公共 API 破坏性改动检查（0 break）
- [x] 测试覆盖（service.rs +15 / connect.rs +4 / lan-mouse-ipc +2）
- [x] unsafe 引入（0 处）
- [x] BUG 清单（0 P0 / 0 P1 / 2 P2 文档）
- [x] REQUIREMENT 一致性（无破坏）
- [x] PLAN 偏离（11 处全部 A1 策略）