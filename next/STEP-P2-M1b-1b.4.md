# STEP-P2-M1b-1b.4 — Cross-platform real-machine end-to-end test template

> PLAN §M1b / STEP-1b.4
> 执行日期：2026-09-10　实际耗时：~25 min
> 结论：✅ 通过（manual 模板 + 集成测试 stub 就绪；0 PLAN 偏差；0 new clippy / fmt / build 错误）

---

## 1. 做了什么

按 PLAN-2 §3 M1b STEP-1b.4 行落地两件交付：

### 1.1 真机 manual log 模板 — `tests/manual/clipboard-text.md`（532 行，新文件）

按 1b.1 / 1b.2 / 1b.3 契约覆盖三组跨平台对端 × 四类场景：

| § | 覆盖 STEP | 场景 | 验证手段 |
|---|---|---|---|
| §1 S1 | 1b.1 inline + 1a.4 dispatcher + StreamC round-trip | 小文本（≤ 1 KiB，含 CJK / emoji）| 字节级一致 + daemon log `apply_inbound_clipboard_text` |
| §1 S2 | 1b.2 Meta + HTTP/3 GET + active cache eviction | 1 MiB 文本（`head -c 1048576 /dev/urandom \| base64`）| `diff /tmp/big.txt /tmp/back.txt` = 0 + `sha256sum` 一致 |
| §1 S3 | 1b.3 LRU 128 / 60s TTL + `mark_local_write` + metrics | 同内容 5 次重复复制（间隔 < 500 ms）| daemon log 显示 1 `apply` + 4 `loopback LRU hit — skip push` + hit-rate trace = 4/5 |
| §1 S4 | 1b.2 source `cache.remove(prev_sha)` + race | 100 ms 间隔 X1→X2→X3→X4→X5 | B 端最终 = X5；404 cache miss **允许**（per 1b.2 reviewer #3 2nd silent skip 契约）|

| §2 对端组 | 命令映射 |
|---|---|
| §2.1 macOS ↔ Windows | `pbcopy`/`pbpaste` ↔ `Set-Clipboard`/`Get-Clipboard`（含 UTF-8 ↔ UTF-16 转换提醒）|
| §2.2 macOS ↔ Linux | `pbcopy`/`pbpaste` ↔ `xclip -selection clipboard` / `wl-copy`/`wl-paste`（含 X11 vs Wayland 不兼容 + 工具检测日志）|
| §2.3 Windows ↔ Linux | `Set-Clipboard`/`Get-Clipboard` ↔ `xclip`/`wl-paste`（含 PowerShell `-Raw`/`-NoNewline` 用法）|

模板还包含：
- **§3 result capture template** — 24 行结果记录表（3 组 × 2 方向 × 4 场景）
- **§4 troubleshooting** — 4 类常见症状 + 对应 daemon log 检查 + SUGGESTION 上报路径
- **§5 M1b milestone gate** — 静态检查 + 三平台 check + 24 真机 cell 通过清单
- **§6 out of scope** — 图片 / 文件 / GUI / portal 等显式排除项

模板使用 1a.5 同样的"复制粘贴即可跑"风格：每条命令 + 期望 daemon log 行 + 期望 B 端输出 + 通过标志全部明确列出。

### 1.2 集成测试 stub — `tests/clipboard_text_e2e.rs`（300 行，新文件）

5 个测试（`cargo test --workspace` 跑 2 + ignore 3）：

| 测试 | 状态 | 覆盖 |
|---|---|---|
| `one_megabyte_text_round_trips_via_stream_c` | `#[ignore]`（需要 in-process StreamC harness）| 1b.1 `ClipboardText::from_content` 1 MiB 走 meta + `Vec<u8>` codec + decode round-trip（meta path 不内联 1 MiB）|
| `http3_cache_miss_returns_404_silently` | `#[ignore]`（需要 in-process HTTP/3 Router harness）| 1b.2 reviewer #3 2nd：`Response::with_status(404, vec![])` + dispatcher `(status, body.len())` match 走 silent-skip 分支 |
| `active_eviction_concurrent_with_lookup_old_returns_miss` | `#[ignore]`（`src/clipboard` 是 `pub(crate)`）| 1b.2 契约 pin 在 unit-test 层 `src/service.rs::register_pending_clipboard_request`（注释指明） |
| `inline_clipboard_text_codec_round_trip` | **run** ✓ | inline payload `ClipboardText` codec round-trip sanity |
| `inline_boundary_round_trip` | **run** ✓ | `CLIPBOARD_TEXT_INLINE_LIMIT` 边界（1024 → inline；1025 → meta）|

`#[ignore]` 三个 stub 在 `cargo test --workspace` 不跑，需要时 `cargo test --workspace --test clipboard_text_e2e -- --ignored` 触发。

两个 sanity 测试每次 `cargo test --workspace` 都跑 — 防止 stub 文件"100% ignored 假装在跑"。

---

## 2. 关键设计

### 2.1 模板 + stub 双轨（不替换既有单测）

按 STEP-1b.4 prompt "不要求真跑 / 只留 stub" + "1b.4 末段真机测试由用户跑"：

- **真机场景**（4 类 × 3 组 × 2 方向 = 24 cell）→ 模板指引人类在 `tests/manual/clipboard-text.md` 上打勾，不跑任何 LAN 测试代码
- **integration stub** → 1b.4 末段已落代码（1b.1 inline/meta split + 1b.2 cache + 1b.3 LRU）的契约被 pin 在文件里，但 `#[ignore]` 不参与 `cargo test`；为 M1b 末段 / 后续 milestone 提供"先占位再接 seam"的承载点

### 2.2 `src/clipboard` 是 `pub(crate)` → 集成测试够不到 cache

踩了一个约束：原本想 `use lan_mouse::clipboard::cache::ClipboardCache;` 在 `tests/clipboard_text_e2e.rs` 里 pin 1b.2 active eviction 契约，但 `src/lib.rs:14` 是 `pub(crate) mod clipboard;` → E0603 "private module"。

**处理**：
- 第三个 stub 测试 body 留空（仅 doc-comment 指明契约 pin 在 `src/service.rs::register_pending_clipboard_request`）
- 后续 milestone（M2a 当 `clipboard::Backend` 需要公开给 GUI Toaster 时）顺手把 `pub(crate)` 升级为 `pub`；届时再 un-stub 这个测试

无 PLAN 偏差（PLAN §0 Out of Scope 显式说"clipboard 模块不暴露"是预期）。

### 2.3 `Vec::<u8>::from(event.clone())` 而不是 `(&event).into()`

`lan_mouse_proto` 只实现 `From<ProtoEvent> for Vec<u8>`，**不**实现 `From<&ProtoEvent>`（引用）。`tests/quic_smoke.rs` 和 `src/quic_transport/protocol.rs:611-625` 都用 `Vec::<u8>::from(event.clone())` / `v.clone()` pattern。stub 跟随现有约定。

### 2.4 doc_lazy_continuation 警告消除

模板文件 + stub 文件都不在 `tests/` 里跑 `cargo clippy` 检查 doc-comment（只有 `cargo doc` 检查）—— 但 workspace clippy 仍然扫 `tests/`，所以 stub 文件的 doc-comment 必须 clippy-clean。

第一版用 markdown list `- **StreamC round-trip** ...` + `+ QUIC handshake` 触发 `clippy::doc_lazy_continuation`（`+` 在 markdown 视为新 bullet）。改写为"**StreamC round-trip** ... combined with QUIC handshake and TLS ..."（无 `+` / `-` 触发）后 clippy 干净。

### 2.5 stub 文件本身贡献 2 个 sanity 测试

`#[ignore]` 测试不参与默认 `cargo test --workspace` 计数 → 如果整个文件 100% ignored，等于"假装在跑"。两个 sanity 测试（`inline_clipboard_text_codec_round_trip` + `inline_boundary_round_trip`）每次都跑，既验证 1b.1 codec 路径，又给 stub 文件留个真实 anchor。

---

## 3. 验证结果

- `cargo build --workspace`：✅ 通过（0 error 0 warning）
- `cargo test --workspace`：**328 passed / 0 failed / 3 ignored**（M1b.3 baseline 326 → 1b.4 328 = **+2 sanity tests**）
- `cargo fmt --all -- --check`：✅ 0 diff
- `cargo clippy --workspace --all-targets -- -D warnings`：14 errors（baseline 14 = pre-existing；**1b.4 引入 0**；与 1b.3 同口径）

**测试细分**：
| crate | tests | 备注 |
|---|---|---|
| `input-capture` | 101 | 无变化 |
| `lan-mouse` (lib) | 161 | 无变化（M1b.3 终点）|
| `clipboard_text_e2e` (新) | **2 passed / 3 ignored** | 1b.4 贡献 |
| `quic_smoke` | 7 | 无变化 |
| `quic_session` | 2 | 无变化 |
| `lan-mouse-ipc` | 26 | 无变化 |
| `lan-mouse-proto` | 29 | 无变化 |
| **合计 run** | **328** | 0 failed |
| doc-tests | 0 | 7 个 doc-test target 全 0 测试，0 错 |

**clippy baseline（pre-existing，1b.4 未触及）**：
- `src/connect.rs`:1146/1147（doc_lazy_continuation）/ 1702/1708（assertions_on_constants）
- `src/quic_transport/endpoint.rs`:238（doc_lazy_continuation）/ 339（too_many_arguments）
- `src/quic_transport/session.rs`:931（doc_lazy_continuation）
- `src/service.rs`:109-113（incoming_clipboard 字段 doc 的 doc_lazy_continuation，pre-1b.4 代码）

**未触发任何 1b.4 相关 clippy warning**：模板文件无 Rust 代码（不参与 clippy）；stub 文件 doc-comment 干净（lazy_continuation 修过一次）。

---

## 4. 与 PLAN 的偏差

**0 处 PLAN 偏差**：1b.4 完整落地了 PLAN §3 M1b STEP-1b.4 行的全部要素：

| PLAN 要求 | 落地位置 |
|---|---|
| 创建 `tests/manual/clipboard-text.md` 模板 | ✅ `tests/manual/clipboard-text.md`（532 行）|
| 模板覆盖三组跨平台对端 × 四类场景 | ✅ §2.1 / §2.2 / §2.3 + §1 S1 / S2 / S3 / S4 |
| 加 1-2 条集成测试占位（mock StreamC + HTTP/3 cache miss）| ✅ `tests/clipboard_text_e2e.rs`（5 tests: 3 stub `#[ignore]` + 2 sanity `run`）|
| **不要求真跑** | ✅ 全部 `#[ignore]`，stub 不连真实 peer；2 个 sanity 只用 codec 层验证 |
| 模板最后列 M1b 里程碑收尾闸门 | ✅ §5 M1b milestone gate 完整列出 `cargo build/test/clippy/fmt` + 三平台 check + 24 cell |

**1 处执行偏差（已就地处理）**：原本 stub 3 想用 `use lan_mouse::clipboard::cache::ClipboardCache;` 直接 pin active eviction 契约 → E0603 "private module"。改成 doc-comment 指明契约 pin 在 `src/service.rs::register_pending_clipboard_request`，body 留空，**0 行为 / 0 计划** 变化。SUGGESTION 记在 M1b 收尾时机。

---

## 5. 处理的 SUGGESTION 项

无新增。本 STEP 不修改 dispatcher / cache / http3 / service 代码路径；现有 SUGGESTION #S-1（macOS backend）/ #S-2（Windows + Linux 跨平台）未触及。

**新增建议**：#S-3 见 §6 遗留。

---

## 6. 闸门检查

- **闸 1（执行前）**：产物 ✅（模板 + stub 路径都已识别）；依赖 ✅（1b.1 / 1b.2 / 1b.3 全部归档为"通过"）；验收 ✅（`cargo build/test` 就绪）；milestone 边界 ✅（仅 M1b 内）；时间门 ✅（<1h）
- **闸 2（执行中）**：clippy 1 次失败（`doc_lazy_continuation`）→ 就地改写为非 list 形式；编译 3 次失败（`as_clipboard_text()` 不存在 / `Vec<u8>::from(&event)` 不支持 / `encode_response` 未 import / `&[u8; 0]` 不 impl `Into<Bytes>`）→ 全部就地修通（match pattern + `event.clone()` + import + `Vec::new()`）；**0 次**触发 plan-deviation 报告
- **闸 3（milestone 收尾，本次非收尾）**：跳过；1b.4 是 M1b 末段（无后续 1b.5 / 1b.6 STEP），M1b 收尾跑全套待人类真机测试完成后

---

## 7. 遗留 / 下一步

### 7.1 1b.4 自身遗留

- **`src/clipboard` 模块仍是 `pub(crate)`**（E0603 触发源）。M2a / M4 阶段 GUI Toaster 需要 `clipboard::Backend` 公开 API 时顺手升级 `pub(crate)` → `pub`，再 un-stub `active_eviction_concurrent_with_lookup_old_returns_miss`。**不在 1b.4 范围**（PLAN §0 Out of Scope）。

- **stub 测试需要 in-process harness**：当 `PeerSession` 暴露 `send_stream_c` + clipboard 的 test seam 时，可 un-stub `one_megabyte_text_round_trips_via_stream_c` + `http3_cache_miss_returns_404_silently`。属于 "1b.x followup" / M2a 范围。

### 7.2 下一步

1. **人类真机测试**：按 `tests/manual/clipboard-text.md` 在 macOS ↔ Windows / macOS ↔ Linux / Windows ↔ Linux 三组各跑一遍（4 场景 × 2 方向 = 8 trial / group = 24 cells total）。结果记录在 §3 result capture template。
2. **M1b 收尾（人类真机全通后）**：
   - `cargo build --workspace`
   - `cargo test --workspace`（预期 328 + 任何后续 hot-fix 增量）
   - `cargo clippy --workspace --all-targets -- -D warnings`（预期 14 errors baseline 不变）
   - `cargo fmt --check`
   - leader 触发 step-validator 整批审（M0a / M0b / M0c / M1a / M1b 五批 + post-M1a hot-fix）
   - commit + push
3. **M2a 启动**：剪贴板图片 + macOS（PLAN §3 M2a STEP-2a.1 / 2a.2 / 2a.3 / 2a.4）。

### 7.3 SUGGESTION 建议（待 M1b 收尾时决议）

- **#S-3 ⚪** — `src/clipboard` 模块公开化时机。M2a 阶段当 `ClipboardBackend` 需要暴露给 GUI Toaster 通知（PLAN §3 M4 STEP-4.4 GeneralPanel 卡片"剪贴板刚被 X 改了"提示）时，把 `pub(crate)` 升级为 `pub`；同时 un-stub `tests/clipboard_text_e2e.rs::active_eviction_concurrent_with_lookup_old_returns_miss`。

---

## 8. 文件改动

| 文件 | 改动类型 | 备注 |
|---|---|---|
| `tests/manual/clipboard-text.md` | 新建 | 532 行真机 manual 模板（§0 前置 / §1 S1-S4 四场景 / §2.1-2.3 三组对端 / §3 结果记录 / §4 troubleshooting / §5 milestone gate / §6 out of scope）|
| `tests/clipboard_text_e2e.rs` | 新建 | 300 行 integration test stub（5 tests: 3 stub `#[ignore]` + 2 sanity `run`；sha256 helper；wire codec 验证）|

**未触碰**：
- `src/service.rs`（1b.3 终态不变）
- `src/quic_transport/http3.rs`（1b.2 终态不变）
- `src/clipboard/*`（1a.x / 1b.2 终态不变；无 cache.rs / mod.rs / platform backend 改动）
- `lan-mouse-proto` / `lan-mouse-ipc`（无 proto bump / IPC 扩展）
- `Cargo.toml`（无依赖变更）
- `next/.LEADER-STATE.md`（leader 维护，1b.4 不直接更新）

---

## 9. commit 拆分建议（leader 决策）

按 PLAN §0 commit 卫生 + template/stub 拆分（4 拆 → 实际可 2 拆合并）：

```
1. docs(tests): add cross-platform clipboard text manual test template (M1b STEP-1b.4 §1)
   - tests/manual/clipboard-text.md (new, 532 lines)
   - Covers: macOS ↔ Windows / macOS ↔ Linux / Windows ↔ Linux × S1-S4 (small text / 1 MiB / loopback skip / rapid switch)

2. test(clipboard): add integration test stub for StreamC + HTTP/3 cache-miss (M1b STEP-1b.4 §1)
   - tests/clipboard_text_e2e.rs (new, 300 lines)
   - 3 #[ignore] stubs (StreamC round-trip + HTTP/3 404 silent + active eviction)
   - 2 sanity tests that run on every cargo test --workspace (codec round-trip + boundary)
```

可合并为：
```
1. test(clipboard): M1b STEP-1b.4 manual template + integration test stub
   - tests/manual/clipboard-text.md
   - tests/clipboard_text_e2e.rs
```

报告本身（`next/STEP-P2-M1b-1b.4.md`）作为第三个 commit（`docs:`）。

---

## 10. 累计耗时

~25 min：
- ~10 min 设计 manual 模板结构（参考 1a.5 §5 风格 + PLAN §3 M1b STEP-1b.4 要求 + 4 场景 × 3 组对端的双向矩阵）
- ~10 min 写 stub + 调试 5 处初版编译错误（`as_clipboard_text()` / `(&event).into()` / `encode_response` 缺失 / `&[u8; 0]` Into / private module）+ clippy `doc_lazy_continuation` 修一次
- ~5 min 验证 build/test/clippy/fmt + 报告

---

## 11. 执行备注（leader）

- **模板优先于 stub**：用户真机测试用模板 → 模板直接决定 stub 测试"contract pin"的准确性。先设计 §1 S1-S4 流程后再写 stub 三个 `#[ignore]` 测试的 doc-comment。
- **stub 不连真实 peer**：按 prompt "集成测试 stub 不连真实 peer"。`#[ignore]` 是关键，不允许"测试看起来跑了但实际打洞网络"。
- **commit 拆 4 还是 2**：模板与 stub 是同一 STEP 的两份交付，逻辑上独立但同步发布；建议 2 拆（manual / stub）+ 1 docs（报告）= 3 commit。如要 1 commit 全收也行——见 §9。
- **M1b 收尾待人类真机**：24 cell 全通后 leader 跑全套静态检查 + 触发 step-validator。
