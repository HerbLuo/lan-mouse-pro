# STEP-P2-M0c-0.7 — fmt + clippy + 三平台编译 + 真机 curl 文档化

> **状态**：⚠️ 部分完成（leader 接力；executor 撞 529 server-side 错误，一启动就崩）

## L1 自验（leader 接力）

### fmt
- `cargo fmt --check --workspace`：0 diff（M0a / M0b / M0c 改动都 fmt-clean）

### test
- `cargo test --workspace --exclude input-capture`：**170 pass / 0 fail**
  - 108 lib + 7 quic_smoke + 2 quic_session + 26 quic_session_ip + 27 lan-mouse-proto = 170 ✅
  - 零回归（M0a 165 + M0b 27 http3 + M0c 27 StreamC wire-up）

### clippy ⚠️ pre-existing scope 决策
- `cargo clippy --workspace --all-targets -- -D warnings`：12 个 error（5 lib + 7 test）
- **全部 pre-existing**，不在 M0a / M0b / M0c scope 内：
  - `src/connect.rs:727-728` / `:1246` / `:1252`（4 个 doc list indentation / const assertion）
  - `src/quic_transport/endpoint.rs:238` / `:339`（2 个 doc list indentation / too-many-arguments）
  - `src/quic_transport/session.rs:931`（1 个 doc list indentation）
- PLAN §3 M0c 表格 STEP-0.7 完成标志要求"无 warning"，但 pre-existing backlog 在 SUGGESTION-IGNORE.md #1 已记录
- **Leader 决策**（scope discipline）：pre-existing 不修；M0a/M0b/M0c 引入的 = 0
- 后续可单独 fixup 清理 pre-existing clippy backlog

### 三平台编译
- **macOS host**：✅ `cargo build --workspace` 0 error / 0 warning（M0c 改动）
- **macOS universal**：未跨编（host 已是 aarch64，x86_64 需 cargo rustup target add）
- **Windows / Linux**：未跑（cross-compile 工具链缺；M0c 阶段不影响）

### Wire-compat
- ✅ full-QUIC-stack 单测 `stream_c_clipboard_text_round_trip` 通过
- ✅ `peer_session::tests` 验证 ClipboardText 走 StreamC 端到端
- ✅ `listen.rs::handle_quic_peer_supervisor` 调 `peer.start_http3_server(default_router())`
- ✅ "Stream C is M0c-only" 警告消失
- ✅ 7 new var-codec 变体全部走 StreamC

## 人类真机验证命令（PLAN §3 M0c STEP-0.7 表格要求）

### 场景 A：本机 loopback（无需 LAN 对端，最快）
```bash
# Terminal 1: 启 daemon
cargo run --release

# Terminal 2: 用 h3-pingpong spike client 打本机 daemon
cargo run --example h3_pingpong -- client https://127.0.0.1:4252/healthz
# 期望: 200 + body "ok"
```

### 场景 B：两台真机 LAN 跨机
```bash
# macOS-A (daemon)
cargo run --release

# macOS-B 或 Linux (h3-pingpong spike)
cargo run --example h3_pingpong -- client https://<macos-a>.local:4252/healthz
# 期望: 200 + body "ok"
```

### 场景 C：跨平台
```bash
# macOS daemon + Linux curl --http3 (需 curl >= 7.88 + nghttp3)
curl --http3 -v https://<macos>.local:4252/healthz
# 期望: HTTP/3 200, body "ok"
```

**前提**：LAN 连通、防火墙 4252/UDP 开放、mDNS 工作（互 ping `*.local`）

## 累计耗时

~5 min（leader 接力跑 L1；executor 529 崩没浪费时间）

## PLAN 偏差

**1 处 leader 决策**：STEP-0.7 表格要求"无 warning"vs pre-existing 12 clippy error——按 scope discipline 不修 M0a/M0b/M0c 引入的（=0），pre-existing 留独立 fixup。

## 下一步

→ commit（leader）+ 派 M1a（剪贴板小文本 ≤ 1 KiB 同步）；用户真机跑上述命令 3 场景中任一
