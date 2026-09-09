# M1a Clipboard No-Op 真机问题调研

## 摘要

代码检查确认 500ms clipboard tick 已经接入 `Service::run` 的 `tokio::select!`，macOS `default_backend()` 也会构造 `MacOsPasteboard` 并在启动时记录 backend 选择。因此“没有任何 clipboard 日志”本身不能证明 tick 或 `pbpaste` 没有运行：当前实现对 tick 的 `None`、内容未变化、LRU 命中、没有 eligible peer 等路径全部静默，甚至成功检测到变化时也没有 `clipboard change detected` INFO 日志。

最值得优先验证的是运行时状态而非 StreamC wire codec：backend 是否真的被选中、daemon 的 `Service::run` 是否持续运行、目标 client 是否同时 `active`、`active_addr` 非空且 `enable_clipboard_to=true`。只有通过这些 gate 后才会 enqueue `CaptureRequest::SendClip`。此外发现一个架构边界：`broadcast_clipboard_event` 只遍历 `ClientManager` 中的 outgoing clients；server 侧仅有 incoming peer 时，本地复制不会由该函数推送给 incoming peer。

## 假设排序表

| # | 根因假设 | 验证手段（代码 / 命令） | 可能性 |
|---:|---|---|---|
| 1 | **实际没有看到日志是正常现象；tick 的所有成功/跳过路径均无日志** | `src/service.rs:1197-1241`：`handle_clipboard_tick` 仅在 inbound set 失败或 inbound apply 时日志；`Some`、`None`、same text、LRU hit、broadcast 完成均无日志。用 `RUST_LOG=trace` 也不会出现 change-detected 行，因为代码没有该行。 | 很高 |
| 2 | **目标 peer 被 broadcast gate 静默过滤**：`enable_clipboard_to=false`、client 非 active、`active_addr=None` | `src/service.rs:1332-1344` 逐项检查。确认 GUI/配置中的 `enable_clipboard_to`，并 grep 运行日志中的 `connected @` / `reconnected @`。用前端同步或检查配置确认该 client active；连接成功不等于 `active_addr` 在发送瞬间仍存在。 | 很高 |
| 3 | **backend 没有成功构造，或 Service 根本没有进入主 run loop** | `src/service.rs:302-310`：macOS `default_backend()` 成功会有 `clipboard backend selected: macos-pbcopy-pbpaste`，失败才有 WARN。`src/clipboard/macos.rs:63-75` 的 probe 运行 `pbpaste --help`。用户执行 `command -v pbpaste; pbpaste </dev/null >/tmp/pbpaste.out; printf 'exit=%s\\n' "$?"`。同时确认 daemon 进程未提前退出/卡在启动错误。 | 中高 |
| 4 | **`pbpaste` 每次返回 None（非零退出或非 UTF-8），而实现静默吞掉错误** | `src/clipboard/macos.rs:84-101`：`.output().ok()?`、non-zero、UTF-8 decode failure 都变成 `None`，没有日志。直接在同一用户/session 下执行 `pbpaste >/tmp/clip.out; s=$?; wc -c /tmp/clip.out; printf 'exit=%s\\n' "$s"`；再用 `pbcopy` 写入并读取。注意 GUI daemon 的 launch/session 环境可能与交互 shell 不同。 | 中 |
| 5 | **tick 没被 poll / Service::run 被其他 future 阻塞** | `src/service.rs:380-402` 明确存在 `_ = self.clipboard_tick.tick() => ...`，`Interval::interval(500ms)` 初始化于 `:358`。因此静态证据不支持“arm 缺失”；运行时需确认同一 daemon 的常规日志（连接、frontend、heartbeat）仍在流动，且进程未停在初始化。 | 中低 |
| 6 | **fingerprint/LRU 短路** | `clipboard_last_text` 初始为 `None`（`:355`），第一次成功读到文本不应被 same-text 短路；sha256 是完整 32 bytes（`:1205-1217`）。只有此前 inbound 写入同一 hash 或 daemon 曾观察过相同文本才会 LRU 命中。通过先写唯一 nonce 文本（例如 `clip-diag-$(date +%s%N)`）规避旧 hash，并查看对端结果。 | 低 |
| 7 | **Capture request 已入队但 StreamC/peer 发送失败** | `src/capture.rs:423-426` 是 fire-and-forget；实际失败 WARN 在 `src/capture.rs:976-981`。确认是否出现 `capture: send_event to handle ... failed`。`src/quic_transport/session.rs:557-587` 的 `send_stream_c` 会 `open_bi`, 写 `[u32 BE len]+var-codec body`；`src/quic_transport/streams.rs:303-325` 与 `src/listen.rs:1012-1037` 都有 StreamC reader。若没有 capture WARN，问题更早，通常是 #2。 | 中低 |
| 8 | **inbound apply 路径未接通**（outbound 已成功） | client 侧 `src/quic_transport/session.rs:1178-1192` → `send_clipboard_inbox`；server 侧 `src/listen.rs:893-910,1012-1022` → `emulation.rs:245-285` → service inbound channel。需要 trace/debug 中看到 `stream C ClipboardMeta event`、`server stream C reader: from ...`，以及接收端 `clipboard inbound: applied ...`。 | 低到中 |
| 9 | **复制发生在 server/incoming-only 一侧，代码没有向 incoming peer 广播** | `src/service.rs:1332-1344` 只遍历 `client_manager.get_client_states()`；incoming peer 由 `Emulation`/listen registry 管理，不在这个列表。若拓扑是“对端主动连入本机”，本机复制不会经过此 outbound broadcaster。验证哪一侧建立 QUIC、以及复制发生在哪一侧。 | 中（取决于拓扑） |

## 最关键 3 个发现

1. **“无 clipboard 相关日志”不是有效的故障信号。** `handle_clipboard_tick` 没有任何成功、`None`、dedup 或广播日志；即使完全正常也可能全程静默。`RUST_LOG=trace` 不能补出不存在的日志。
2. **发送前有三个静默 gate。** `enable_clipboard_to`、`state.active`、`state.active_addr.is_some()` 任一不满足，`Capture::send_event` 都不会调用。没有 `capture` WARN 只能说明请求可能从未入队，不能说明 StreamC 正常。
3. **StreamC reader/writer 静态 wiring 存在。** client `read_stream_c_loop`、server `server_stream_c_reader_task`、`PeerSession::send_stream_c` 均已实现；因此应先证明 tick/backend/broadcast gate 到达，再定位 QUIC。另有拓扑限制：当前 broadcaster 只服务 outgoing `ClientManager` peers。

## 建议的诊断步骤

### 用户最直接执行的两条命令

在 macOS 上，用与 daemon 相同的登录用户/session 执行：

```sh
command -v pbpaste; command -v pbcopy
pbcopy <<'EOF'
lan-mouse-clipboard-diag
EOF
pbpaste >/tmp/lan-mouse-pbpaste.out
s=$?
printf 'pbpaste exit=%s bytes=' "$s"
wc -c </tmp/lan-mouse-pbpaste.out
printf 'value='; tr '\\n' ' ' </tmp/lan-mouse-pbpaste.out; printf '\\n'
```

以 trace 日志启动/重启 daemon（具体启动子命令按本机安装方式替换；重点是环境变量必须传给 daemon）：

```sh
RUST_LOG=trace lan-mouse daemon 2>&1 | tee /tmp/lan-mouse-clipboard-trace.log
```

然后复制一个每次都不同的值，例如 `lan-mouse-diag-<当前时间>`，并在另一终端筛选既包含成功也包含失败/连接信号的行：

```sh
grep -Ei --line-buffered 'clipboard|send_event|send_stream_c|stream C|connected|reconnected|failed|warn|error' /tmp/lan-mouse-clipboard-trace.log
```

### 观察顺序

1. 启动后必须先找 `clipboard backend selected: macos-pbcopy-pbpaste`。若只有 unavailable WARN，收集完整 error；若两者都没有，确认实际运行的是这份二进制且 `Service::new`/`Service::run` 没有提前退出。
2. 确认同一 client 有 `connected @`，GUI/config 中 `enable_clipboard_to=true`，并且发送时 client 仍 active。若可临时加观测，最需要打印的是三个 gate 的结果，而不是先改 StreamC。
3. 对唯一文本复制后，当前版本预期仍可能没有 clipboard 日志；用 `capture: send_event ... failed` 判断是否到达 CaptureTask。出现该 WARN 才进入 QUIC/StreamC 排查。
4. 发送端若无 WARN、接收端无 `stream C ClipboardMeta event` / `server stream C reader`，再抓两端 trace 并核对连接方向。若复制发生在 incoming-only server 端，当前代码路径本来不会广播。
5. 接收端若看到 `clipboard inbound: applied ...` 但 OS 剪贴板未变，单独检查接收端 `pbcopy`/session 权限；`set_text` 失败会明确产生 WARN。

## 是否需要 leader 立即介入

需要。不是立即修改 StreamC，而是先决定真机诊断策略：优先确认 #1/#2/#3（日志可观测性、peer gate、backend/run-loop），并明确测试拓扑是否要求“incoming-only server 侧复制也能向对端推送”。如果该拓扑属于验收范围，需由 leader 单独决策是否扩展 broadcaster 的 peer 集合；这不是单纯的 macOS `pbpaste` 故障。
