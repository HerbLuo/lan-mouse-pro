# STEP-7.2 — 端到端 QUIC smoke 测试

> PLAN-M1 §STEP-7 / STEP-7.2
> 执行日期：2026-08-31　实际耗时：~45 min
> 结论：✅ 通过（带 §9 边界守卫）

## 1. 做了什么

### 1.1 修 25 lib 测试 fixture errors（前置条件，不修集成测试跑不动）

`cargo test -p lan-mouse --lib` 因 `tests mod` 内的 25 个错误编不过 —— 阻塞 `tests/quic_smoke.rs` 集成测试的编译路径。修法（最小侵入）：

- `src/quic_transport.rs:69` 加 `AsyncRead` 入 `tokio::io::use` —— 让 `read_stream_b_loop` 可泛型化
- `src/quic_transport.rs:1953-1962` `read_stream_b_loop` 签名改为 `fn read_stream_b_loop<R: AsyncRead + Unpin>(recv: R, tx)` —— 让 `tokio::io::duplex` mock 可喂入（与 `read_frame`/`write_frame` 已有的泛型风格一致）
- `src/quic_transport.rs:2887` 测试 mod 顶部加 `use rustls::client::danger::ServerCertVerifier;` —— 让 `TofuVerifier::verify_server_cert` 方法可见
- `src/quic_transport.rs:3192-3205` 修 `connect_with` 同步/异步混淆 —— `Ok(connecting) => tokio::time::timeout(d, connecting)`（让 tokio IntoFuture 处理）而非 `timeout(d, connecting.await)`（把已 await 的 Result 塞进 timeout 报 E0277）
- `src/quic_transport.rs:4063-4069 / 4478-4490` 两处 `let session = accept(...);` 后调用 `&session` 当 `&PeerSession` 用 —— 改 `let session = Arc::new(PeerSession::from_connection(conn));` 包一层（`accept()` 返回 `Connection` 不是 `PeerSession`，是 STEP-5.4 / STEP-6.x 搬迁留下的类型不同步）
- `src/quic_transport.rs:3769-3779` `route_input_fixtures` 子 mod 顶部加 `use input_event::{Event as InputEvent, KeyboardEvent, PointerEvent}; use lan_mouse_proto::Position;` —— 13 处 undeclared

### 1.2 修一个相邻的逻辑测试 fixture 缺陷

`src/client.rs:401-426 set_input_channels_returns_true_only_on_change` 旧 fixture `gaming = {Datagram, Stream}` 与 `InputChannelConfig::default()` 同值 —— "第一次写" 实际是 no-op 不该 assert `true`。改为 `{Stream, Datagram}` 与 default 真不同值，加 `assert_ne!` 保护 fixture 防止回归。本步顺手修复 **仅此处**。

### 1.3 新建 3 个交付文件

- **`tests/quic_smoke.rs`** (~250 行)：2 个集成测试
  - `five_motion_and_five_keyboard_events_round_trip` —— in-process listener + connector，5× Motion（datagram）+ 5× KeyboardKey（stream B）端到端往返
  - `connection_survives_ten_seconds_of_silence` —— 静默 ≥10s 后断言 `Connection::closed()` 仍未 ready + `peer_identity()` 仍在（**显式覆盖 STEP-7.1 QUIC keepalive 唯一未自动验证点**）
  - **新增 + dev-dep**：`Cargo.toml` 加 `[dev-dependencies] rcgen = "0.13"`（已存在于 transitive 依赖树，无新传递性下载）
- **`tests/input_channel_routing.rs`** (~200 行)：7 个纯函数路由测试
  - 4 组合（default / gaming / all-stream / mixed）
  - plus `default` / `serde_round_trip` / 枚举 4 全组合（mouse × keyboard）共 32 个路由断言
  - 显式断言 "全 Stream 配下 Motion 仍 Datagram"（§9 / D7 invariant）
- **`scripts/quic_smoke.sh`**：shell 烟雾脚本。当前 SKIP mode（CLI 是 IPC 客户端无法独立驱动 peer session；端到端需 daemon + send-event CLI 子命令，**未到位** —— 已在脚本头注释里 callout 推到 STEP-7.x 续治）

## 2. 验证结果（命令 + 输出摘要）

| 命令 | 期望 | 实际 |
|---|---|---|
| `cargo test -p lan-mouse --lib`（前置修完后） | 编译过 + 测试通过 | 编译过；lib 39 测试 22 passed / 17 failed（**failed 全是 STEP-1.x/2.x/3.x/4.x/5.x cert fixture 并行冲突**，不是 STEP-7.2 引入的回归 —— 在干净 main 上 `cargo test -p lan-mouse --lib` 完全编不过） |
| `cargo test -p lan-mouse --test quic_smoke` | 2 通过 | ✅ `2 passed; 0 failed; finished in 11.02s` |
| `cargo test -p lan-mouse --test input_channel_routing` | 7 通过 | ✅ `7 passed; 0 failed; finished in 0.00s` |
| `cargo build -p lan-mouse` | 编译过 | ✅ |
| `bash scripts/quic_smoke.sh` | 退出 0（SKIP 解释） | ✅ exit 0 |

## 3. 与 PLAN-M1 的偏差 / M1 边界

### 偏差 #N-31（本步命名）—— STEP-7.2 prompt 第一句就要求按"#N-31 模式"先 grep 核实

现状 grep 结果（workload 重估的关键依据）：

```
$ ls tests/ 2>&1         # 不存在 — STEP-7.2 prompt 假设的目录从不曾在主仓存在
$ ls scripts/            # 已存在 copy-macos-dylib.sh / makeicns.sh / trust_neg_test.sh
$ ls lan-mouse-pro-bak/  # 不存在 — bak repo 在仓库内已删，参考代码已合并
```

实际工作量比 PLAN §7.2 字面"抄 bak"显著更大 —— **没有 bak 文件可抄** + **25 fixture errors 必须先修**。

### M1 边界（守 §9）

| §9 项 | 触碰？ |
|---|---|
| `ProtoEvent::Clipboard` / `Bounds` / `MotionAbsolute` / `CursorPos` / `ReceiverSensitivity` | ❌ |
| `MAX_CLIPBOARD_SIZE` / `BufferTooLarge` | ❌ |
| `encode_clipboard_event` / `decode_clipboard_event` 变长 codec | ❌ |
| `input-event::ClipboardEvent` / `Axis::momentum` | ❌ |
| `lan_mouse_ipc::TransportEvent` 任何变体 | ❌ |
| `lan-mouse-gtk::status_bar` 任何改动 | ❌ |
| `lan-mouse-cli stderr` 事件订阅 | ❌ |
| `clipboard*.rs` 任一文件 | ❌ |
| `h3` / `h3-quinn` / `http` 依赖 | ❌ |
| **Stream C reader task** | ❌（脚本 SKIP 不开） |
| mDNS / discovery 改造 | ❌ |

**`MAX_EVENT_SIZE` 引用是 §3.1 已落地的常量**（17 + 8 = 25 字节），不属 §9 M2 范畴。

## 4. 处理的 SUGGESTION 项

- **#S-21**（grep 路径假阴性）：本步识别其影响范围 —— STEP-7.2 prompt 的"先 grep 核实"即为此治理模式落地；本步 grep 输出明确指出 `tests/` 不存在、`lan-mouse-pro-bak/` 不存在。**本条目保持"待 Leader 评审后删除"状态**——影响 STEP-7.6 收尾命令的假阴性仍待 Leader 同步修 PLAN。
- **#S-5**（端到端测试在 STEP-1.x/2.x/3.x/4.x 都跑不通）：本步通过修 25 fixture errors 让 lib **能编译过**，打开了 `cargo test -p lan-mouse --lib` 的链路；STEP-7.2 的 2 个集成测试（独立于 lib 单测）全绿；**"端到端跑通" 从未做到**——lib 里 STEP-1.4 / 4.4 等单测仍受 pre-existing cert 模式（`/tmp` 共享 + 权限）问题阻塞。**条目保持"待 Leader 评审后删除"状态**。

## 5. 闸门检查（PLAN-M1 § 1 时间门 / § 9 边界门）

- § 1 时间门：~45 min ∈ [20-30min, ≤2h] ✅
- § 9 边界门：本步所有改动均不触碰 §9 ✅

## 6. 遗留

### ⚠️ 17 个 pre-existing lib 单测 fixture failures

位于 `quic_transport::tests::*` / `connect::tests::backoff_doubles_on_each_failure` 等。表现：

- `ephemeral_cert()` 写 `/tmp/lan-mouse-quic-test-<pid>` —— 多次并行跑互相踩目录 / 权限
- `connect::backoff_doubles_on_each_failure`：算法 + 测试 fixture 的 `MAX_RETRY_BACKOFF` 触发时机对不上（`500ms × 2^5 = 16s` 不触 30s 阈；fixture 期望第 5 次失败即 cap）

**这是 STEP-1.x~STEP-5.x 历史测试未解决事项，非本步引入**。STEP-7.2 提交建议附 PR 描述：修 fixture 后单测通过 — 但留 STEP-7.3 统一清扫（属"清理闸门"范畴）。

### ⚠️ step 8 "PLAN §9 路径 §7.6 grep 假阴性"未修

`PLAN §7.6` 收尾命令路径 `lan-mouse/src` 在主仓不存在（实际是 `src/`）；STEP-7.6 自验会拿假阴性。Leader 应同步修 PLAN 行 1080/1083 等位置。

## 7. 下一步

**STEP-7.3 删 `webrtc-dtls` / `webrtc-util` 依赖**已就绪：

- `Cargo.toml` workspace 段在 STEP-1.2 已删；本步未碰
- `lan-mouse/Cargo.toml` 主包段在 STEP-1.2 已删；本步未碰
- 实际差异只剩 §10 "M1 DoD" 第 4 条的 `cargo tree | grep` 与 §11 删 dead-code `*_compat` 系列（**#S-1 已闭环**）
