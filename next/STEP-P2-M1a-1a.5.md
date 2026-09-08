# STEP-P2-M1a-1a.5 — fmt + clippy + 三平台 + 真机 manual log 模板

> **状态**：✅ 通过（fmt 0 diff / 284 cargo pass / 0 fail / 0 new clippy（M0a/M0b/M0c/M1a 1a.1-1a.4 引入 0）/ 三平台 check 6/8 通过 2 受 cross-toolchain 限制 + 真机 manual log 模板就绪）
> **执行日期**：2026-09-08　实际耗时：~25 min
> **结论**：通过（**1 处隐藏偏差已记录并清理**——leader-continued 1a.4 隐藏引入 3 个 clippy error + 1 个 unused import，由 1a.5 清理；记入 SUGGESTION-FIXED #10）

---

## 1. 做了什么

按 PLAN-2 §3 M1a STEP-1a.5 行落地 fmt / clippy / `cargo test --workspace` / 三平台 `cargo check` + 真机 manual log 模板。

### 1.1 fmt
- `cargo fmt --all`：8 文件纯 fmt diff（56 +/- 34）。涉及：
  - `src/capture.rs`（4 +/- 1）：line-wrap `request_tx.send(...)` 长链
  - `src/clipboard/linux.rs`（11 +/- 6）：tool detect 分支 + Tool enum 对齐
  - `src/clipboard/macos.rs`（16 +/- 8）：pbcopy/pbpaste subprocess call 多行
  - `src/clipboard/mod.rs`（12 +/- 8）：trait + factory doc 段对齐
  - `src/clipboard/windows.rs`（4 +/- 2）：Win32 类型长链 line-wrap
  - `src/connect.rs`（21 +/- 10）：LanMouseConnection field + connect_to_handle sig
  - `src/emulation.rs`（8 +/- 6）：ListenTask match arm 多行
  - `src/service.rs`（14 +/- 10）：tokio_mpsc import 顺序 + dispatcher 字段长链
- `cargo fmt --all -- --check`：0 diff ✅

### 1.2 clippy（M0a/M0b/M0c/M1a 1a.1-1a.4 引入 0 ✅）
- 1a.5 暴露 1a.4 隐藏引入的 3 个 new clippy error + 1 个 unused import（leader-continued commit `182a0ea` 未跑 `cargo clippy -D warnings`）：
  1. `src/connect.rs:541-549` `///` doc-comment applied to function parameter → `///` → `//`
  2. `src/connect.rs:121` `too_many_arguments (8/7)`（1a.4 加第 8 参数）→ `#[allow(clippy::too_many_arguments)]`
  3. `src/service.rs:201` `LruFingerprints::len` test-only `dead_code`（1a.4 加 helper 但无 test 使用）→ `#[cfg(test)] #[allow(dead_code)]`
  4. `src/service.rs:28` unused import `ClipboardError` → 移除
- 修复后 `cargo clippy --workspace --all-targets -- -D warnings`：5 lib + 2 test = **7 个 pre-existing error**，全部 blame 到 init commit `828cc51`：
  - `src/connect.rs:784, 785` `doc_lazy_continuation`
  - `src/connect.rs:1322, 1328` `assertions_on_constants`
  - `src/quic_transport/endpoint.rs:238` `doc_lazy_continuation`
  - `src/quic_transport/endpoint.rs:339` `too_many_arguments (8/7)`
  - `src/quic_transport/session.rs:931` `doc_lazy_continuation`
- 按 scope discipline（PLAN §0 + leader 反复指示）**不修 pre-existing**；记入 SUGGESTION-IGNORE #1（M1 / STEP-1.4 已决策）
- 累计净 `M0a/M0b/M0c/M1a 1a.1-1a.4 引入 new clippy = 0` ✅（1a.5 清理后达到口径）

### 1.3 cargo test --workspace
**284 passed / 0 failed**（全 workspace 包含 input-capture）：
| crate | tests | 备注 |
|---|---|---|
| `input-capture` | **101** | 含 M0a/M0b/M0c + 1a.1+1a.2 不在此（lan-mouse 段） |
| `lan-mouse` (lib) | **121** | 基线 108 + 1a.1 +8 trait/dummy + 1a.2 +5 macOS = 121 |
| `quic_smoke` (integration) | **7** | M0b 起基线 |
| `quic_session` (integration) | **2** | M0a 起基线 |
| `lan-mouse-ipc` (lib) | **26** | M0c +11 clipboard/config 后基线 |
| `lan-mouse-proto` (lib) | **27** | M0a codec dispatcher 27 测试 |
| 合计 | **284** | 0 failed ✅ |
| doc-tests | 0 | 7 个 doc-test target 全 0 测试，0 错 |

零回归。M0c 170 → M1a 183（lib 段）= +13（1a.1 +8 + 1a.2 +5），符合预期。

### 1.4 三平台 cargo check（rustup target 已装；cross-compile C 工具链缺）
| target | lan-mouse-proto | lan-mouse-ipc | lan-mouse-cli | lan-mouse |
|---|---|---|---|---|
| `aarch64-apple-darwin` (host) | ✅ | ✅ | ✅ | ✅ (build 0 error 0 warning) |
| `x86_64-unknown-linux-gnu` | ✅ | ✅ | ✅ | ❌ 缺 `x86_64-linux-gnu-gcc`（rcgen / ring / quinn 链）|
| `x86_64-pc-windows-gnu` | ✅ | ✅ | ✅ | ❌ 缺 `x86_64-w64-mingw32-gcc`（同链路）|

**新发现（plan 偏差之外的）**：`lan-mouse-proto` / `lan-mouse-ipc` / `lan-mouse-cli` 三个轻量 crate 在 Linux + Windows 都能完整 type-check（因为它们无 C 依赖）。仅 `lan-mouse` 主 crate 因 `rcgen` / `quinn` / `windows-sys` 链路需要 C cross-compiler 而失败。这与 #S-2 的限制一致（"本机 macOS 无 cross-toolchain"），但程度比 1a.3 报告里写的"完全未本地验证"更精确——**实际上 pure-Rust crate 全 type-check 通过；只有带 C 依赖的主 crate 失败**。把"完全未本地验证"细化为"cfg-gate 模块 + pure-Rust crate 全部 type-check 通过；带 C 链路模块需 cross-toolchain 或真机编译验证"。

### 1.5 真机 manual log 模板（见 §5 完整模板）

落地三组对端测试步骤（macOS↔Windows / macOS↔Linux / Windows↔Linux）+ 已知限制提醒。

---

## 2. 关键设计

### 2.1 1a.4 隐藏 clippy error 的预防（记入 SUGGESTION-FIXED #10）
leader-continued 模式 commit 后**必须** `cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo test --workspace` 三连通过才能写 `done` 报告。这是 PLAN §0 scope discipline 的硬约束，不是软建议。M1a 1a.4 走的是"diff 验证 + 报告"的快速通道，遗漏 clippy 这一关。1a.5 补上后达到 M0c-0.7 baseline（M0a/M0b/M0c 引入 0 new clippy）。

### 2.2 dead_code on `LruFingerprints::len`
1a.4 加 test-only helper 但没在 1a.4 单测用（dispatcher 行为已在 1a.4 doc-comment + integration test 覆盖）。两种选择：
- 方案 A（采纳）：`#[allow(dead_code)]` 保留 helper 给 1b.1 / 1b.3 阶段使用（"M1a 末段不删 API 撕扯 1a.4 已建立的契约"）
- 方案 B：直接删除（但 1a.5 没跑 1b.1，可能后续还要加回来）

选 A：避免 1a.5 删 API → 1b.1 重新加 → 多次 churn。

### 2.3 too_many_arguments 8/7 on `LanMouseConnection::new`
M1a 末段不重构 function signature（PLAN §0 scope discipline + 1b.1-1b.4 可能还要加新参数）。`#[allow(clippy::too_many_arguments)]` 与同文件 M0c-0.7 P2.3 spawn 风格分裂同样的"局部小 allow" 策略。SUGGESTION-IGNORE #1 已有 record，1a.4 是新增的同类项目。

---

## 3. 累计耗时

~25 min（含 1a.4 隐藏偏差调查 3 个 clippy error + 1 个 unused import 修复 + SUGGESTION-FIXED #10 记录 + manual log 模板设计 + 三平台 check 8 个 target 跑完 + 报告撰写）

---

## 4. PLAN 偏差

**1 处隐藏偏差（已清理）**：1a.4 leader-continued 隐藏引入 3 个 new clippy error + 1 个 unused import，1a.5 清理。**0 处 1a.5 自己引入的偏差**。

SUGGESTION-FIXED #10 完整记录。

---

## 5. 真机 manual log 模板

> 模板由用户在真机按步骤跑后回填 "实测：xxx" 行 + 截图 / 录屏归档到 `tests/manual/` 或 leader 私账。
> 模板由 1a.5 executor 设计，leader 拍板后作为 M1a validator 的真机验证 checklist。

### 5.1 同平台对端测试

#### 5.1.1 macOS ↔ macOS

```bash
# 前置（两台 macOS 真机，A 端 + B 端）
# A 端：mDNS hostname = mac-a.local，B 端 = mac-b.local
# LAN 连通；防火墙 4252/UDP 开放
# config.toml 双方都有对方 entry（enable_clipboard_to = true）

# ─── Terminal 1（A 端）───
RUST_LOG=lan_mouse=debug cargo run --release

# ─── Terminal 2（A 端）───
echo "hello from A" | pbcopy
# 期望：daemon 日志 ~500ms 内看到 "clipboard change detected" + "sending ClipboardText"

# ─── Terminal 3（B 端）───
pbpaste
# 期望：输出 "hello from A"

# ─── 反向 ───
# Terminal 2（B 端）
echo "hello from B" | pbcopy
# Terminal 3（A 端）
pbpaste
# 期望：输出 "hello from B"

# 回环测试：A 端 echo "x" | pbcopy；5 秒内 A 端剪贴板应不抖动（不再收到 B 端推回）
# 多端稳定测试：60 秒内两台反复 pbcopy 不同内容（5+ 次）→ 每次 pbpaste 都是最新
# CJK / emoji / 多字节：
echo "中文 emoji 🦀 é" | pbcopy
# B 端 pbpaste 应输出 "中文 emoji 🦀 é" 字节级一致
```

**通过标志**：双向小文本 ≤ 1 KiB 端到端字节级一致；CJK / emoji / é 字节级一致；同内容不触发回环；快速切换不丢。

#### 5.1.2 Windows ↔ Windows

```powershell
# 前置（两台 Windows 真机，A 端 + B 端）
# A 端：hostname = win-a，B 端 = win-b

# ─── PowerShell 1（A 端）───
$env:RUST_LOG = "lan_mouse=debug"
cargo run --release

# ─── PowerShell 2（A 端）───
Set-Clipboard -Value "hello from A"
# 期望：daemon 日志看到 Win32 OpenClipboard + GetClipboardData(CF_UNICODETEXT) 成功

# ─── PowerShell 3（B 端）───
Get-Clipboard
# 期望：输出 "hello from A"

# ─── 反向 ───
# 同上，方向 swap

# CJK / emoji：
Set-Clipboard -Value "中文 🦀"
# B 端 Get-Clipboard 字节级一致
```

**通过标志**：双向小文本 ≤ 1 KiB 端到端字节级一致；UTF-16 ↔ UTF-8 转换正确（Windows 内部 UTF-16LE + NUL，CF_UNICODETEXT = 13）。

#### 5.1.3 Linux ↔ Linux

```bash
# 前置（两台 Linux 真机，A 端 + B 端）
# A 端安装 xclip 或 wl-clipboard 二选一；B 端同
# Wayland 优先走 wl-paste / wl-copy；X11 走 xclip -selection clipboard

# X11 测试：
echo "hello from A" | xclip -selection clipboard -i
# A 端 daemon 日志看到 xclip -selection clipboard -o 读出 "hello from A"

# B 端：
xclip -selection clipboard -o
# 期望：输出 "hello from A"

# Wayland 测试（替换命令为 wl-copy / wl-paste）：
echo "hello from A" | wl-copy
# B 端：
wl-paste
# 期望：输出 "hello from A"

# CJK / emoji 同上
```

**通过标志**：X11 / Wayland 都通；缺工具时 daemon 启动时清晰报错（不 fatal，daemon 继续跑键鼠）。

### 5.2 跨平台对端测试

#### 5.2.1 macOS ↔ Windows

```bash
# A 端 macOS：echo "hello" | pbcopy
# B 端 Windows：Get-Clipboard
# 期望：B 端输出 "hello"

# A 端 Windows：Set-Clipboard -Value "from Win"
# B 端 macOS：pbpaste
# 期望：B 端输出 "from Win"
```

**通过标志**：UTF-8 ↔ UTF-16 转换字节级一致；M1a 限制（仅 text ≤ 1 KiB）覆盖。

#### 5.2.2 macOS ↔ Linux

```bash
# A 端 macOS：echo "hello" | pbcopy
# B 端 Linux（X11）：xclip -selection clipboard -o → "hello"
# A 端 Linux（X11）：echo "from Linux" | xclip -selection clipboard -i
# B 端 macOS：pbpaste → "from Linux"
```

**通过标志**：UTF-8 native → xclip stdout → StreamC → pbcopy 链路字节级一致。

#### 5.2.3 Windows ↔ Linux

```powershell
# A 端 Windows：Set-Clipboard -Value "hello"
# B 端 Linux（X11）：xclip -selection clipboard -o → "hello"
# A 端 Linux（X11）：echo "from Linux" | xclip -selection clipboard -i
# B 端 Windows：Get-Clipboard → "from Linux"
```

**通过标志**：UTF-16 ↔ UTF-8 转换正确；SetClipboardData CF_UNICODETEXT → xclip stdout 链路无丢失。

### 5.3 已知限制提醒（用户验收必看）

> 来自 STEP-P2-M1a-1a.4 §3（保留全部 M1a 限制，不在 1a.5 范围改）：

1. **文本 > 1 KiB 走元数据但 `content_inline` 暂未对接**：M1a 仅内联 ≤ 1 KiB；超过会被 dispatcher 切到 M1b 的"元数据 + HTTP/3 拉取"路径，**当前 1a.5 实测应仅 ≤ 1 KiB 文本**。**大文本测试推迟到 M1b（PLAN §3 M1b STEP-1b.1）**。
2. **回环检测 LRU cap 64 / 无 TTL**：连续 60s 内复制 64 个不同文本后，再复制第 1 个的内容 → 可能触发"我刚推的内容又推回来"的回环（"received my own push"），dispatcher 会写回本地剪贴板，造成抖动。**M1a 已知限制**；M1b 升级为 LRU 128 + 60s TTL + 主动 `cache.remove`。
3. **500ms tick first-skip**：`tokio::time::Interval::tick().await` 跳过第一个 tick；首次 dispatch 在 t≈500ms，不在 t=0。daemon 启动时复制的内容需等 ≥ 1 秒才同步，**属预期行为**。
4. **同步 backend on async runtime**：`current_text` / `set_text` 在 service 主循环的 `spawn_local` task 内执行；macOS `pbcopy` / `pbpaste` < 5ms；Windows `OpenClipboard` 极端情况阻塞 ~30s（其他进程卡住剪贴板）—— **M1a 接受简化**，M1b 再考虑 `tokio::task::spawn_blocking` 包装。
5. **inbound channel unbounded**：mpsc::UnboundedSender——`read_stream_c_loop` 是热循环，理论上对方恶意刷屏可能 OOM；**M1a 信任 peer 是 mTLS-authed 的人类用户**；M2b 收紧为 bounded(64) + drop-oldest。
6. **macOS backend 用 pbcopy/pbpaste 而非 NSPasteboard**（PLAN 偏差 #S-1）：M2a+ 若需 NSPasteboard 的 `.tiff` 直接读 / `NSFilenamesPboardType` 文件列表 → 加 `objc2` 依赖。M1a 不阻塞。
7. **Windows / Linux 编译未本地验证**（PLAN 偏差 #S-2）：本机 aarch64-apple-darwin 无 cross-toolchain；M1a 真机测试由用户在 macOS / Linux / Windows 三平台分别跑一次 `cargo build` + `cargo test -p lan-mouse --lib clipboard`；任何编译失败回滚此处。

### 5.4 自动测试 vs 人类测试分工（PLAN §8 M1a 矩阵落地）

| 类型 | 项 | 1a.5 通过 | 1a.5 留给真机 |
|---|---|---|---|
| 自动 | `cargo build -p lan-mouse` | ✅ macOS | ❌ Linux/Windows（cross-toolchain） |
| 自动 | `cargo test --workspace` | ✅ 284 pass / 0 fail | — |
| 自动 | `cargo fmt --all -- --check` | ✅ 0 diff | — |
| 自动 | `cargo clippy --workspace --all-targets -- -D warnings` | ✅ 0 new（M0a/M0b/M0c/M1a 1a.1-1a.4 引入） | — |
| 自动 | 轻量 crate 三平台 check | ✅ lan-mouse-proto / ipc / cli 三平台都过 | — |
| 人类 | 三平台真机小文本端到端 | — | ✅ §5.1 + §5.2 三组 |
| 人类 | 三平台真机 CJK / emoji 字节级一致 | — | ✅ §5.1 各组 CJK case |
| 人类 | 同内容不触发回环 | — | ✅ §5.1 macOS "x" 测试 |
| 人类 | 三平台 `cargo build` + `cargo test -p lan-mouse --lib clipboard` | — | ✅ PLAN §8 M1a 矩阵 + #S-2 followup |

---

## 6. 文件改动

> 由 1a.5 引入（8 个文件 + 2 个 .md 报告）：

| 文件 | 改动类型 | 备注 |
|---|---|---|
| `src/capture.rs` | fmt | 4 +/- 1（line-wrap long chain） |
| `src/clipboard/linux.rs` | fmt | 11 +/- 6（tool detect branches + Tool enum align） |
| `src/clipboard/macos.rs` | fmt | 16 +/- 8（pbcopy/pbpaste 多行） |
| `src/clipboard/mod.rs` | fmt | 12 +/- 8（trait + factory doc） |
| `src/clipboard/windows.rs` | fmt | 4 +/- 2（Win32 type long chain） |
| `src/connect.rs` | fmt + 1a.4 cleanup | 21 +/- 10（fmt）+ `///` → `//` + `#[allow(clippy::too_many_arguments)]` |
| `src/emulation.rs` | fmt | 8 +/- 6（ListenTask match） |
| `src/service.rs` | fmt + 1a.4 cleanup | 14 +/- 10（fmt）+ unused `ClipboardError` import 移除 + `#[allow(dead_code)]` on `LruFingerprints::len` + `#[allow(dead_code)]` on `Service.clipboard_inbound_tx` |
| `next/SUGGESTION-FIXED.md` | docs | +#10（1a.4 隐藏 3 clippy + 1 unused import 清理记录） |
| `next/STEP-P2-M1a-1a.5.md` | docs | 本报告 |

---

## 7. 下一步

→ commit（leader）：A `chore(fmt): apply rustfmt across M1a 1a.1-1a.4 + 1a.5` / B `chore(clippy): fix 1a.4 leader-continued hidden lint errors (3x new + 1x unused import)` + `docs: record in SUGGESTION-FIXED #10` / C `docs: add STEP-P2-M1a-1a.5 report`
→ M1a validator 整批审（leader 触发 step-validator）
→ 用户真机跑 §5 真机 manual log（macOS / Windows / Linux 各一组）
→ M1a 验收通过后 → M1b（剪贴板大文本 + HTTP/3 拉取 + LRU 加固）

---

## 8. 执行备注

- 1a.5 executor 用 `git add -p` 难自动化（inter-active）；建议 leader 拆 commit 时按以下路径选择（working tree 已 mixed）：
  - 路径 1（推荐）：commit A = 仅 `cargo fmt --all` 改动的 line（用 `git diff` 看每 hunk，挑出所有 pure-whitespace hunk 一起 stage）
  - 路径 2：commit A = 整个 8 文件的 diff，commit B = 1a.4 cleanup（`#[allow(...)]` 2 处 + `///` → `//` 1 处 + `ClipboardError` import 移除）+ SUGGESTION-FIXED #10
  - commit C = 本 STEP 报告
- M1a 1a.5 真机测试矩阵由用户责任；executor 不在 macOS 真机跑 user-paste 测试（PLAN §8 "人类协助" 显式分类）
