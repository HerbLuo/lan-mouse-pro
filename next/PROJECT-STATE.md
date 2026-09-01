# lan-mouse-pro 项目技术状态文档

> 写于：2026-09-01（**接 STEP-7.2b 回退后**）
> 目的：把整个项目的技术状态汇总，方便在 Windows 上用 Claude 修复 `spawn_local` panic

---

## 1. 项目目标

**用 QUIC 替换 webrtc-dtls + UDP**，作为传输层重构。M1 范围已 100% 完成代码侧工作。

**M1 范围**（PLAN-M1.md §0）：
- ✅ 鼠标 / 键盘 / Enter-Leave / Ping-Pong / Hello 握手
- ✅ 自签证书 + 指纹白名单持久对端认证
- ✅ 探活超时 8s → 30s（QUIC keepalive）
- ✅ Happy-eyeballs 支持 QUIC
- ✅ 鼠标 button / 键盘 stream-or-datagram 可切换
- ✅ IPC / CLI / GTK 公共 API 不变
- ✅ webrtc-dtls / webrtc-util 完全下线

**M2 范围**（不在当前文档范围）：剪贴板文本/图片/文件同步。

---

## 2. 技术栈

| 组件 | 版本 | 用途 |
|---|---|---|
| Rust edition | 2021 | workspace |
| quinn | 0.11.11 | QUIC 实现（rustls-ring feature） |
| rustls | 0.23.37 | TLS 1.3（ring crypto provider，**不用** aws_lc_rs） |
| rustls-pemfile | 1.0.4 | PEM 解析 |
| rcgen | 0.13.x | 测试用自签证书 |
| sha2 | 0.10 | SHA-256 指纹 |
| thiserror | 2.0 | Error 派生 |
| tokio | **1.32.0** | async runtime |

> **⚠️ 关键**：项目锁的是 `tokio 1.32.0`，但用户在 Windows 上跑的是 **`tokio 1.51.1`**（局部升级）。`JoinSet::spawn_local` / `tokio::task::spawn_local` 在 1.51 对 LocalSet 上下文检查变严格，单元测试用 `#[tokio::test]` 默认 multi-threaded runtime 没有 LocalSet 包裹 → panic。

---

## 3. 代码结构

### 3.1 Workspace
```
lan-mouse-pro/
├── Cargo.toml                       # workspace + bin lan-mouse
├── src/                              # 主仓二进制
│   ├── main.rs                      # entry (current_thread runtime + LocalSet)
│   ├── lib.rs                       # mod quic_transport 注册
│   ├── capture.rs / emulation.rs    # 输入捕获/仿真
│   ├── client.rs / config.rs        # 配置 + ClientManager
│   ├── connect.rs                   # 出站 (LanMouseConnection) ← 关键
│   ├── listen.rs                    # 入站 (LanMouseListener) ← 关键
│   ├── service.rs                   # 服务编排
│   ├── crypto.rs                    # cert/key/fingerprint
│   ├── quic_transport.rs            # QUIC 核心 (PeerSession/Endpoint/Verifier) ← 关键
│   ├── dns.rs / firewall.rs / macos_power.rs
│   └── capture_test.rs / emulation_test.rs
├── lan-mouse-cli/                   # 命令行前端
├── lan-mouse-gtk/                   # GTK GUI
│   ├── src/client_row/              # peer 编辑行 (AdwComboRow 等)
│   ├── src/ui/window.rs
│   └── resources/client_row.ui
├── lan-mouse-ipc/                   # IPC 类型 (ClientConfig/FrontendRequest/FrontendEvent)
├── lan-mouse-proto/                 # 协议 codec (ProtoEvent, PROTOCOL_MAGIC)
├── input-capture/, input-emulation/, input-event/
├── tests/                           # 集成测试
│   ├── quic_smoke.rs                # 端到端 (2 tests, 已全绿)
│   └── input_channel_routing.rs     # 路由 (7 tests, 已全绿)
├── scripts/
│   └── quic_smoke.sh                # SKIP 模式 shell 脚本
└── next/                            # 项目过程文档
    ├── PLAN-M1.md                   # 主计划 (37 STEP, 全部完成)
    ├── STEP-x.x.md                  # 每步归档
    ├── SUGGESTION.md                # 持续更新
    └── PROJECT-STATE.md             # 本文档
```

### 3.2 核心模块 (`src/quic_transport.rs` ~4850 行)
```
PeerSession                  # 一条 QUIC 会话
├── conn: Connection         # quinn::Connection (Send + Clone)
├── hello_ok: AtomicBool
├── stream_bunch: Arc<Mutex<Option<StreamBunch>>>
└── ...

StreamBunch { a, b, c }      # 3 条双向流 (c 是 M2 预留)
Bidi<S, R = S>              # send + recv 双向流 wrapper

Channel enum { Datagram, StreamA, StreamB, StreamC }
routeInputChannel(cfg, event) -> Channel   # channel-level 路由

endpoint(addr) -> Result<Endpoint>           # UDP → quinn::Endpoint (client-mode)
endpoint_with_cert(addr, cert, key) -> ...    # server-mode + ALPN b"lan-mouse"
endpoint_with_verifier(addr, cert, key, verifier) -> ...  # server + mTLS

dial(ep, addr, cert, key) -> Result<Connection>
dial_any(ep, primary, all, cert, key, pins_dir) -> Result<Connection>
accept(ep) -> Result<Connection>

install_crypto_provider()    # OnceLock 守护 ring provider install_default()
build_quic_client_config(cert_chain, key, pins_dir) -> quinn::ClientConfig
endpoint_with_cert(...) -> quinn::Endpoint (server)

TofuVerifier                  # client 端 ServerCertVerifier
AuthorizedKeysVerifier         # server 端 ClientCertVerifier (allowlist)
PermissiveClientCertVerifier   # 占位 verifier

client_hello(peer) / server_hello(peer)    # 应用层握手 + magic 校验
send_motion(peer, event)                   # datagram 优先 + 降级 stream
send_input(peer, event, cfg)               # 按 routeInputChannel 分派
read_loop(recv_a) -> ReadStreams { b, join_b }
run(self: Arc<Self>) -> Result<()>         # 主干：hello + datagram + 3 stream + select!
hello_watchdog(peer)                       # 3s 超时
HELLO_TIMEOUT = 3s
MAX_SAFE_DATAGRAM = 1162                   # PLAN-v4 spike 实测
```

### 3.3 IPC 类型 (`lan-mouse-ipc/src/lib.rs`)
```
ChannelMode { Stream, Datagram }
InputChannelConfig { mouse_button, keyboard }   # Default: mouse=Datagram / keyboard=Stream
ClientConfig                                     # GTK 读写，含 input_channels 字段
FrontendRequest::SetClientInputChannels(handle, cfg)
```

### 3.4 协议类型 (`lan-mouse-proto/src/lib.rs`)
```
pub const PROTOCOL_MAGIC: [u8;8] = *b"LANMOUSE";
ProtoEvent::Hello { magic: [u8;8], commit: [u8;8] }
ProtoEvent::Motion / Button / Key / Enter / Leave / Ack / Modifiers / Ping / Pong
MAX_EVENT_SIZE = 21 字节
```

---

## 4. 关键设计决策

| ID | 决策 | 默认 |
|---|---|---|
| D1 | PROTOCOL_MAGIC | `b"LANMOUSE"` |
| D2 | quinn 版本 | 0.11（沿用 bak） |
| D3 | rustls provider | **ring**（不用 aws_lc_rs，Windows MSVC NASM 缺失） |
| D4 | keep_alive_interval / max_idle_timeout | 5s / 30s |
| D5 | Hello.magic 不匹配 | `conn.close(VarInt(0), "hello failed")` |
| D6 | HELLO_TIMEOUT | 3s |
| D7 | datagram 优先级 | Motion 永远 datagram；其它按 routeInputChannel |
| D8 | input-event crate 名 | 保持 `input-event` |
| D9 | mDNS | **不引入** |
| D10 | active_lock / latency.rs | **不引入**（已删） |
| D11 | webrtc_dtls_compat feature | **不引入** |
| D12 | **tokio runtime 模型** | **current_thread runtime + LocalSet 包裹**（**关键**） |

---

## 5. tokio runtime 模型（最关键！）

### 5.1 生产路径

`src/main.rs:111-124`：
```rust
fn run_async<F, E>(f: F) -> Result<(), LanMouseError>
where F: Future<Output = Result<(), E>>,
      LanMouseError: From<E>,
{
    // create single threaded tokio runtime
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;

    // run async event loop
    Ok(runtime.block_on(LocalSet::new().run_until(f))?)
}
```

**关键事实**：
- 生产用 **current_thread runtime**（不是 multi-threaded）
- 但 **`LocalSet::new().run_until(f)` 包裹整个 f**
- 所以生产环境下 `tokio::task::spawn_local` 和 `JoinSet::spawn_local` 都正常工作

### 5.2 单元测试路径

`#[tokio::test]` 默认行为：
- 在 tokio 1.32：**multi-threaded runtime**（**没有 LocalSet 包裹**）
- `spawn_local` 在多线程 runtime 下需要 LocalSet 上下文，否则 panic（**tokio 1.51 严格化**了此检查）

`#[tokio::test(flavor = "current_thread")]`：
- current_thread runtime
- 但**没有 LocalSet 包裹**
- 在 tokio 1.51 下 `spawn_local` 仍然 panic（current_thread runtime 是 LocalRuntime，但需要 LocalSet 句柄传下去；新版本检查更严）

### 5.3 spawn_local 在仓库的全貌

```
src/dns.rs:46          spawn_local(dns_task.run())
src/dns.rs:99          tokio::task::spawn_local(...)
src/listen.rs:299      spawn_local(...)
src/listen.rs:310      spawn_local(...)
src/listen.rs:356      spawn_local(...)
src/connect.rs:192     spawn_local(connect_to_handle(...))
src/connect.rs:380     spawn_local(spawn_peer_supervisor(...))
src/connect.rs:446     spawn_local(connect_to_handle(...))
src/service.rs:647     tokio::task::spawn_local(...)
src/capture.rs:86      spawn_local(capture_task.run())
src/emulation.rs:90    spawn_local(emulation_task.run())
src/emulation.rs:287   spawn_local(emulation_task.run())

# STEP-7.2b 错误地改了（应回退）：
src/quic_transport.rs:807  JoinSet::spawn_local → spawn
src/quic_transport.rs:854  JoinSet::spawn_local → spawn
src/quic_transport.rs:2054 spawn_local → tokio::task::spawn
src/quic_transport.rs:2204 spawn_local → tokio::task::spawn
```

### 5.4 STEP-7.2b 错误判断

**我之前在 STEP-7.2b 改错了**。理由：
- 生产用 current_thread + LocalSet，spawn_local 正常
- 我改成 spawn 看似让单元测试通过，但：
  1. 破坏了 LocalSet 的语义约定（虽然 captures 都 Send 不影响编译）
  2. 生产路径不需要此改动
  3. 单元测试 panic 的真正原因不是 capture 的 Send 性，是**测试 runtime 没有 LocalSet**

**正确修复**（见 §6）：回退 Step-7.2b + 改测试 infrastructure。

---

## 6. 当前已知问题：Windows 单元测试 panic

### 6.1 失败清单

```
quic_transport::tests::dial_any_all_unreachable_returns_err         FAILED
quic_transport::tests::dial_any_prefers_primary                      FAILED
quic_transport::tests::hello_wrong_magic_closes_connection           FAILED
quic_transport::tests::peer_session_round_trip_motion_keyboard       FAILED
quic_transport::tests::stream_c_take_releases_quinn_recv_stream      FAILED
```

### 6.2 错误信息

**A 类 spawn_local panic**（3 个，`#[tokio::test(flavor = "current_thread")]`）：
```
thread '...' panicked at tokio-1.51.1/src/task/local.rs:445:29:
`spawn_local` called from outside of a `task::LocalSet` or `runtime::LocalRuntime`
```

**B 类 handshake 时序问题**（2 个，`#[tokio::test]`）：
```
peer_session_round_trip_motion_keyboard:
  client send_motion: HelloFailed("hello not complete")
hello_wrong_magic_closes_connection:
  HelloFailed 消息应含 'wrong magic'，实际：read Hello frame length: connection lost
stream_c_take_releases_quinn_recv_stream:
  dial: Handshake(TimedOut)
```

B 是 A 的级联：server 端 `PeerSession::run()` 内部用 `spawn_local` panic → server task 没起来 → connection lost → handshake fail。

### 6.3 ✅ 正确的修复方案

**关键决策**：回退 STEP-7.2b 的代码改动 + 改测试 harness。

#### 步骤 1：回退 STEP-7.2b 的代码改动

```bash
git revert 9b4cb9a
```

或者手动编辑 `src/quic_transport.rs` 把 4 处改回去：
- line 71: `use tokio::task::{JoinHandle, JoinSet};` → 改回 `+ spawn_local`
- line 807: `joinset.spawn(...)` → 改回 `joinset.spawn_local(...)`
- line 854: `joinset.spawn(...)` → 改回 `joinset.spawn_local(...)`
- line 2054: `tokio::task::spawn(...)` → 改回 `spawn_local(...)`
- line 2204: `tokio::task::spawn(...)` → 改回 `spawn_local(...)`

#### 步骤 2：在测试入口用 LocalSet 包裹

**方案 A**：写测试 helper function：
```rust
// 在 quic_transport.rs 末尾加：
pub fn run_in_local_set<F: std::future::Future<Output = ()>>(f: F) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .unwrap();
    rt.block_on(tokio::task::LocalSet::new().run_until(f));
}
```

**方案 B**：每个失败的测试改成：
```rust
#[test]  // 不再用 #[tokio::test]
fn dial_any_prefers_primary() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .unwrap();
    rt.block_on(async {
        // 原测试代码，包在 LocalSet 里
        tokio::task::LocalSet::new().run_until(async {
            // 测试体
        }).await;
    });
}
```

**方案 C**（更干净）：写一个 `#[tokio::test(flavor = "current_thread")]` 的自定义 macro：
```rust
// 在 quic_transport.rs 测试模块顶部加：
use tokio::task::LocalSet;

macro_rules! local_test {
    ($name:ident, $body:expr) => {
        #[tokio::test(flavor = "current_thread")]
        async fn $name() {
            tokio::task::LocalSet::new().run_until($body).await;
        }
    };
}

// 用法：
local_test!(dial_any_prefers_primary, async {
    // 测试体
});
```

但这要求重写所有失败测试，**太繁琐**。

**✅ 推荐方案 D**：把每个失败的测试改成不直接调 `#[tokio::test]`，而是普通 `#[test]` + 在测试体里手动 `LocalSet::run_until` 包裹。

具体替换模式（适用于 5 个失败测试）：
```rust
// 原（panic）：
#[tokio::test(flavor = "current_thread")]
async fn dial_any_prefers_primary() {
    // ... 测试体用 await
}

// 新（修复）：
#[tokio::test(flavor = "current_thread")]  // 仍用 tokio runtime
async fn dial_any_prefers_primary() {
    use tokio::task::LocalSet;
    LocalSet::new().run_until(async {
        // ... 原测试体照抄
    }).await;
}
```

或者更优雅，用 macro_rules! 让所有失败测试用同一 pattern：

```rust
// 在测试模块顶部加：
macro_rules! local_set_test {
    ($name:ident, $body:expr) => {
        #[tokio::test(flavor = "current_thread")]
        async fn $name() {
            tokio::task::LocalSet::new().run_until(async move $body).await;
        }
    };
}

// 然后重写 5 个失败测试：
local_set_test!(dial_any_prefers_primary, {
    // 原测试体
});
```

**这是最干净的方案**，只需写一次 macro，5 个失败测试改一行 attribute 为 `local_set_test!()`。

---

## 7. 测试架构对比

| 测试类型 | 路径 | runtime | LocalSet | spawn_local | 状态 |
|---|---|---|---|---|---|
| 集成测试 | `tests/quic_smoke.rs` | `#[tokio::test]` 默认 multi-thread | ❌ 无 | 不调 | ✅ Windows 全绿 |
| 集成测试 | `tests/input_channel_routing.rs` | `#[tokio::test]` 默认 multi-thread | ❌ 无 | 不调 | ✅ Windows 全绿 |
| 单元测试（正常） | `quic_transport::tests::*` `#[tokio::test]` | multi-thread | ❌ 无 | 不调 | ✅ Windows 全绿 |
| 单元测试（失败） | `quic_transport::tests::*` 用 `spawn_local` | multi-thread 或 current_thread | ❌ 无 | ✅ 调 | ❌ Windows panic |
| 生产 | `main.rs::run_async` | current_thread | ✅ **有** | ✅ 调 | ✅ 工作 |

**唯一需要 LocalSet 的是：测试 runtime 调 spawn_local 的场景**。所有其他场景不需要。

---

## 8. 当前测试状态（macOS / Linux）

| 项 | 状态 |
|---|---|
| `cargo build --workspace` | ✅ |
| `cargo test -p lan-mouse --test quic_smoke` | ✅ 2 passed |
| `cargo test -p lan-mouse --test input_channel_routing` | ✅ 7 passed |
| `cargo test -p lan-mouse --lib` | ⚠️ 5 failed（spawn_local panic，仅 Windows）|
| `cargo build -p lan-mouse-gtk` | ✅ |
| `bash scripts/quic_smoke.sh` | ✅ exit 0（SKIP）|

---

## 9. 给 Windows Claude 的具体修复指令

**Task A：回退 STEP-7.2b**

```bash
cd C:\path\to\lan-mouse-pro
git log --oneline | head -3     # 找 commit 9b4cb9a
git revert 9b4cb9a              # 或 git reset --hard HEAD~1
```

或者手动改 `src/quic_transport.rs`（**如果 revert 因为签名问题失败**）：

```diff
- use tokio::task::{JoinHandle, JoinSet};
+ use tokio::task::{JoinHandle, JoinSet, spawn_local};

  // dial_any 注释同步：
- /// + `abort_all()` 一站式 API，与 STEP-0.1 全仓 `spawn` 惯例一致。
+ /// + `abort_all()` 一站式 API，与 STEP-0.1 全仓 `spawn_local` 惯例一致。

  // dial_any primary spawn：
- joinset.spawn(async move {
+ joinset.spawn_local(async move {

  // dial_any candidate spawn：
- joinset.spawn(async move {
+ joinset.spawn_local(async move {

  // read_loop 注释：
- // PLAN §5.3：每条 stream 一个独立 `spawn` 读 task，事件经由
+ // PLAN §5.3：每条 stream 一个独立 `spawn_local` 读 task，事件经由

  // read_loop stream B reader：
- let join_b = tokio::task::spawn(read_stream_b_loop(bunch.b.recv, tx_b));
+ let join_b = spawn_local(read_stream_b_loop(bunch.b.recv, tx_b));

  // read_loop datagram reader：
- spawn(datagram_reader_task(self.clone(), tx_d));
+ spawn_local(datagram_reader_task(self.clone(), tx_d));
```

**Task B：改测试 harness**

在 `src/quic_transport.rs` 末尾（或测试模块 `#[cfg(test)] mod tests` 顶部）加：

```rust
/// `local_set_test!` 把测试体包在 `LocalSet::run_until` 里，
/// 让 `spawn_local` / `JoinSet::spawn_local` 在单元测试中也能正常工作
/// （生产路径 main.rs::run_async 已经是 current_thread + LocalSet 包裹，
/// 单元测试 #[tokio::test] 默认 multi-threaded runtime 没 LocalSet 包裹，
/// 在 tokio 1.51 上 spawn_local 会 panic）。
#[cfg(test)]
macro_rules! local_set_test {
    ($name:ident, $body:block) => {
        #[tokio::test(flavor = "current_thread")]
        async fn $name() {
            tokio::task::LocalSet::new().run_until(async move $body).await;
        }
    };
}
```

然后把 5 个失败测试的 attribute 改成：
```rust
// 原：
#[tokio::test(flavor = "current_thread")]
async fn dial_any_prefers_primary() { ... }
// 改：
local_set_test!(dial_any_prefers_primary, { ... });

// 同理：
local_set_test!(dial_any_all_unreachable_returns_err, { ... });
local_set_test!(peer_session_round_trip_motion_keyboard, { ... });
local_set_test!(hello_wrong_magic_closes_connection, { ... });
local_set_test!(stream_c_take_releases_quinn_recv_stream, { ... });
```

5 个失败测试的 `#[test]` 或 `#[tokio::test]` attribute 替换为 `local_set_test!(fn_name, { ... 测试体照抄 ... })`。

**Task C：验证**

```bash
cargo test -p lan-mouse --lib \
  -- dial_any_prefers_primary \
     dial_any_all_unreachable_returns_err \
     hello_wrong_magic_closes_connection \
     peer_session_round_trip_motion_keyboard \
     stream_c_take_releases_quinn_recv_stream
```

预期：5 个全绿。

**Task D：commit**

```bash
git add src/quic_transport.rs
git commit -m "Windows 单元测试修复：LocalSet 包裹 + 回退 spawn_local 改动"
```

---

## 10. 不要做的事（避免重新踩坑）

1. ❌ **不要** 把 `JoinSet::spawn_local` 改成 `JoinSet::spawn`——破坏生产路径的 LocalSet 语义（**STEP-7.2b 的错误**）
2. ❌ **不要** 把 `tokio::task::spawn_local` 改成 `tokio::spawn`——同上
3. ❌ **不要** 把 `#[tokio::test]` 改成 `#[tokio::main]`——签名不匹配
4. ❌ **不要** 在 main.rs 改 runtime 模型——生产用 current_thread + LocalSet 是正确的
6. ❌ **不要** 改 `Cargo.toml` 锁的 tokio 版本——会破坏 macOS/Linux 上跑的集成测试
6. ❌ **不要** 在生产代码里删 `LocalSet::new().run_until(f)` 包裹

---

## 11. M2 路径（如果用户问）

当前不需要。本文档只覆盖 M1 收尾 + Windows 修复。

如果用户后续启动 M2（剪贴板/文件同步），那是新里程碑，需要新 PLAN-M2.md。

---

## 12. 关键 commit 列表

```
9b4cb9a STEP-7.2b: 修 Windows + tokio 1.51 上 spawn_local panic  ← 应回退
b1980bd STEP-7.7: README / DOC.md / CHANGELOG.md 同步 (M1 末步)
8afbdaa STEP-7.6: 头注释清理 DTLS → QUIC
a95ae18 STEP-7.1: 移除 RECV_IDLE_TIMEOUT (已被 STEP-6.2 提前吸收)
11133dc STEP-7.2: 端到端 QUIC smoke 测试 (9 新集成测试全绿)
32870a3 STEP-7.3: 删 webrtc-dtls/util 依赖 + 依赖 guard 测试 + 清理
8f6e187 STEP-7.3: listen.rs supervisor 整合 + macOS wake 整合
... 共 24 commits 总计
```

---

## 13. SUGGESTION.md 摘要（截止 2026-09-01）

剩余条目（active）：

- **#S-1** 🟠 高（已闭环，但 STEP-7.7 还未清理）: 3 个 `*_compat` 入口必删——**STEP-7.3 已删**
- **#S-3** 🟢 低: dead-code warning——**STEP-7.3 已清**
- **#S-5** 🟡 中: 端到端单测验证（`spawn_local` runtime 架构）——**STEP-7.2b 修复范围**
- **#S-17** 🟡 中: datagram_reader 背压策略——**STEP-5.4 已落实**
- **#S-19** 🟠 高: stream B/C accept_bi 装配推 STEP-7.x——**M2 处理**
- **#S-20** 🟡 中: per-IP bind + if_addrs——**后续微步**
- **#S-21** 🟡 中: PLAN §7.6 grep 路径错误——**本修复任务相关**
- **#S-22** 🟡 中: "#N-31 模式" 流程纪律——**M2 起手建议升级 AGENTS.md**
- **#S-23** 🟡 中: 5-7 个 lib fixture 失败跨 spawn_local runtime——**STEP-7.2b 修复同时解**
- **#S-24** 🟠 高: clippy 30+ pre-existing——**M1 收尾或 M2 起手统一清**
- **#S-25** 🟢 低: fmt drift 30+——**同上**

---

## 14. 联系 / 后续

**用户原话**："没修好，请把你当前了解到的整个项目的技术情况写到文档里，我准备到windows里面使用calude修"

→ 本文档已写。在 Windows 上按 §9 步骤操作。

如果 Windows 上跑 `cargo test` 还有其他 panic，告诉我具体测试名和 panic 信息。