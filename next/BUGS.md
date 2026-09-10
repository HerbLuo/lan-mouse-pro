### 全屏截图后鼠标卡顿 / 连接被强制关闭（2026-09-10，未解决）

**现状（用户实测确认）**：

- **触发条件**：用户在被控端（这里指 macOS 主控 + Windows 被控的拓扑）全屏截图 → 主控端 `dispatch_image` 把 1–4 MB PNG 通过 ClipboardImage 推到被控端 → 鼠标跨边界时被卡住几秒；Pong watchdog 1.5s 超时 → 连接被强制关闭
- **对照实验**（用户实测，可重复）：
  - 复制**小文本** → 鼠标正常工作
  - 复制**全屏截图**（1.1 MB JPEG → 3.3–3.7 MB PNG）→ 鼠标卡住
- **结论**：触发点严格绑定到 PNG 编码路径（即 macOS 的 `screencapture -c` 后默认 JPEG，需要 `image::write_to(Png)` 转码）

**根因分析**（已做两轮修复尝试，均不彻底）：

1. **第一轮怀疑**：HTTP/3 大块 body 传输（master → controlled 把 3.7 MB PNG 通过 HTTP/3 response 写回）阻塞了 QUIC 连接上的 control 帧（Pong/Ack） → QUIC 流优先级修复（commit `b4191d4`）把 stream A 设为 `PRIORITY_CONTROL=+100`、HTTP/3 设为 `PRIORITY_BULK=-100`
   - **结果**：失败。用户实测截图后依然卡顿，说明根因不在 QUIC 包调度层面

2. **第二轮发现**（用户报告"小文本好/大截图卡"反向验证后）：真正的根因是 macOS 后端的 **JPEG→PNG / TIFF→PNG 转码在主控端的 LocalSet 线程同步执行**，单线程 `current_thread` runtime 下 PNG 编码 2–5s 把整条 runtime 锁死，期间：
   - `peer.run(StreamA read)` 拿不到 Pong
   - Pong-arrival forwarder 拿不到 `last_pong_at`
   - `ping_heartbeat_task` 发不出 Ping
   - `client_accept_bi_task` 接不到 HTTP/3 新 bidi
   - 1.5s Pong watchdog 超时，强制断开连接

3. **第二轮修复尝试**：commit `4313940` 给 `ClipboardBackend` 加了 `current_image_async`（返回 `Pin<Box<dyn Future + Send>>` 保 dyn 兼容），macOS override 用 `spawn_blocking` 跑 `jpeg_to_png_normalized` / `tiff_to_png_normalized`，dispatcher 的 `handle_clipboard_tick` 改调 async 版
   - **结果**：**用户实测后比之前更卡**——之前一次截图卡一下就恢复，现在每次跨越边界都卡。
   - **代码已 git revert**（commit `420221a`），回到 `b4191d4` 状态

**为什么 spawn_blocking 修复让事情变糟（仍未确认，待诊断）**：

候选假设（按可能性排序）：

- **H1：dispatch loop 被 await 锁住**——`handle_clipboard_tick().await` 是 `service::run` 主 select! 的一个 arm，await 期间其他 arm（capture 事件、emulation 事件、frontend 事件）全部延迟到 spawn_blocking 返回之后才被处理。截图后 2–5s 内所有 capture 事件（包括用户尝试跨边界触发的 BeginPending）都堆积，spawn_blocking 返回后才批量处理——但 BeginPending 的 500ms 超时在堆积期间就开始倒数了。
- **H2：spawn_blocking 引入了额外的调度开销**——LocalSet 单线程下，每次 cache miss 都通过 `spawn_blocking` 转发到 blocking pool；blocking pool 的线程上下文切换对单线程 runtime 有额外唤醒成本（待实测验证）
- **H3：`current_image_async` 的 future 在某些路径上未真正 yield**——cache miss 时先做了 3 次 `read_pasteboard_bytes`（同步 IPC），再 `Box::pin(async move { spawn_blocking.await })`，这之间没有显式 yield。理论上 sync 部分很短（~1ms × 3）不影响，但需实测确认
- **H4：被控端 daemon 没有跟着 force pull / 重启**——用户在被控端可能跑的是比主控端更老的二进制，导致两端实现不对等（不太能解释"更卡"）

**还原现场需要的诊断信息（用户在被控端测一次，把日志贴出来）**：

- 主控端 `RUST_LOG=lan_mouse=debug,quinn=info` 的完整日志，特别是从截图到卡顿结束这段时间内：
  - 是否出现 `clipboard: JPEG→PNG normalized ...` 信息（确认 spawn_blocking 路径确实跑了）
  - 截图前后的 Pong watchdog 日志（确认 1.5s 阈值是否仍触发）
  - 用户尝试跨边界时的 BeginPending / CancelPending 时序
- 被控端 daemon 在同一时间窗口的 `client accept_bi` / HTTP/3 GET 日志
- 被控端是否已 force pull + 重新构建（防止两边二进制不对等）

**下一步修复方向（按"代价从小到大"排序）**：

1. **最小代价：dispatch loop 把 `handle_clipboard_tick` 拆出 select!**——让它在自己的 task 里循环跑（`tokio::spawn_local`），主 select! 只看 capture/emulation/frontend 事件。这样 spawn_blocking await 不会阻塞 capture 事件处理
2. **次小代价：throttle 跨边界的 BeginPending 时，绕过 clip board tick 整条流水线**——截图后短时间内（2–5s）直接屏蔽 image dispatch，只发小的恢复消息
4. **结构性：把大 body transfer 走专用 QUIC 连接**（master 临时为该次 HTTP/3 GET 开一条带独立 cwnd 的连接，用完即关）——彻底隔离 bulk transfer 与 control plane
5. **结构性：换 PNG 编码为更快的格式**（WebP / AVIF / 直接传 JPEG）——平台后端 JPEG passthrough 不需要转码

**当前 git 状态**（备忘）：

```
b4191d4  stream priorities + macos changeCount image cache (2026-09-10)
420221a  Revert "fix(clipboard/master): route macOS JPEG/TIFF→PNG normalisation through spawn_blocking"
a353255  revert(commit 0d5cd3f): drop RGBA→24-bit BI_RGB collapse + regression test
```

主控端 + 被控端现在都应运行 `b4191d4`（或 `420221a`）的代码，bug 未修复。

**相关 commit 详情**：

- `b4191d4`：流优先级修复（防御层，保留）
- `4313940`（已 revert）：spawn_blocking 修复（让事情变糟）
- `d97f3bd`（之前已 revert）：全量 spawn_blocking 修复（含 `apply_inbound_clipboard_image` async 重构）