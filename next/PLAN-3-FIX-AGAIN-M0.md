# PLAN-3-FIX-AGAIN — 把 BUGS-2 地基搬回 twice_fix

> 目的：把 `feature/bulk-conn-quic-isolation` 上**真正起作用的修复**和**地基性修复**搬回当前 `twice_fix` 分支，让 BUGS-2（截图后鼠标丢帧 / 连接被 watchdog 关闭）的根因排查能在一个稳定基线上做。
>
> 关联：[next/BUGS.md](BUGS.md)（BUGS-2 详细日志 + 历史修复记录），[next/STEP-FIX-BUGS-2-runtime-block-panic.md](STEP-FIX-BUGS-2-runtime-block-panic.md)（本里程碑的前置 panic 修复）。
>
> 状态：**M0 已完成**（地基已就位）；M1（`dispatch_image` 重活搬出 LocalSet）为下一里程碑。

---

## 1. 为什么需要这个 PLAN

`twice_fix` 在 96ef883 起步，比 `main` 还落后 6 个 commit。这 6 个 commit 是 2026-09-11 那批"把 polling / inbound apply / inbound HTTP/3 GET 从 main `select!` 拆走"的根本修复——没有它们，`feature/bulk-conn-quic-isolation` 上的 `b1cc3a0`（multi_thread runtime）和 `7959586`（`apply_inbound_image_task` spawn_blocking）所依赖的代码结构（独立 `clipboard_poller` task、`apply_inbound_image_task` 函数）根本**不存在**。这就是为什么用户在 `feature/bulk-conn-quic-isolation` 上测了一轮觉得"基本没有修复"——其中一部分修复受地基缺失拖累而失效。

本次 M0 把地基（main 上已就位的 6 个 commit）+ 症状缓解（`f928401` watchdog relax、`ea5a3b8` idle_timeout fix）全部搬过来，让 M1 有一个干净的工作基线。

---

## 2. M0 搬了哪些 commit

按落地顺序：

| 序号 | commit 来源 | 内容 | 落地方式 |
|------|------------|------|---------|
| ①  | `a94c249` | move clipboard polling into spawn_local task | ff-merge from main |
| ②  | `fd5733d` | poller cmd arm + inbound helpers async paths | ff-merge from main |
| ③  | `8de4219` | move inbound image apply to spawn_local task | ff-merge from main |
| ④  | `fc38296` | move inbound HTTP/3 GET into spawn_local task | ff-merge from main |
| ⑤  | `8df5ec2` | LRU mark after apply + GET timing log | ff-merge from main |
| ⑥  | `257c939` | 3 条 clipboard-flow 日志提 info | ff-merge from main |
| ⑦  | `f928401` | Pong watchdog 1.5s → 3.5s + 不级联关 bulk conn | 手动（cherry-pick 撞上 fd5733d 的 supervisor tail 重写） |
| ⑧  | `ea5a3b8` | KEEPALIVE 5s→2s / max_idle_timeout 5s→30s | 直接应用，diff 不冲突 |

落地后的 `twice_fix` HEAD：
```
3b19a7a fix(quic): bulk conn idle_timeout too short — keepalive 5s→2s, idle 5s→30s
d882454 fix(quic): relax Pong watchdog 1.5s → 3.5s (BUGS-2 follow-up)
257c939 chore(logs): promote 3 clipboard-flow debug lines to info for live diagnosis
8df5ec2 fix(service): code-review follow-up — LRU mark after apply, GET timing log, GET-failure test
fc38296 fix(service): move inbound HTTP/3 GET into spawn_local task (mouse frame drop on controlled — round 2)
8de4219 fix(service): move inbound image apply to spawn_local task (mouse frame drop on controlled)
fd5733d fix(service): route poller cmd arm + inbound helpers through async paths (2026-09-10 code-review follow-up)
a94c249 fix(service): move clipboard polling into spawn_local task (2026-09-10 screenshot root cause)
96ef883 docs
7e78428 docs
c8a87f8 docs(known-bugs): record 2026-09-10 screenshot mouse-stuck bug + failed fix attempts
420221a Revert "fix(clipboard/master): route macOS JPEG/TIFF→PNG normalisation through spawn_blocking (2026-09-10 screenshot bug)"
4313940 fix(clipboard/master): route macOS JPEG/TIFF→PNG normalisation through spawn_blocking (2026-09-10 screenshot bug)
b4191d4 fix(quic/clipboard): stream priorities + macos changeCount image cache (2026-09-10 screenshot bug)
... 更早的 ...
```

注意：
- `b4191d4`（流优先级 + macOS changeCount 缓存）通过 ff-merge 已经隐式带上（在 main 的历史里）。
- `b1cc3a0`（multi_thread runtime）、`7959586`（`apply_inbound_image_task` spawn_blocking）、所有 bulk conn routing 系列——本次**不拿**。

---

## 3. 为什么不拿 `b1cc3a0` 和 `7959586`

| 不拿 | 理由 |
|------|------|
| `b1cc3a0`（multi_thread） | 用户实测没改善根因（`dispatch_image` 的 sha256/clone 仍在 LocalSet）；引入 peer_lost_tx 类型换 tokio mpsc、last_pong_at 类型换 Arc<Mutex>、ping/pong 改 tokio::spawn 的复杂性合并冲突面广；症状缓解已经由 `f928401` 完成。等 M1 决定是否走"主任务零重活"路线时再评估。 |
| `7959586`（`apply_inbound_image_task` spawn_blocking） | 修的是**被控端入站 apply**（`set_image`/`set_dib_image` FFI），不是主控端出站 dispatch（`dispatch_image`）。用户 bug 在主控端截图方向，这条修了**没用**。`8de4219 + fc38296` 已经把入站 HTTP/3 GET + apply 都搬出主 `select!`，是它的弱化版，对当前 bug 够用。 |
| bulk conn routing 系列（`a017718` 等 12 个 commit） | 新功能不是 bug 修复，独立 PLAN 单独 STEP。 |

---

## 4. 验证

### 4.1 单测
- `cargo build --workspace`：✅ 0 error
- `cargo test --workspace --lib`：✅ **249 + 26 + 29 + 101 + ... 全过**（含 `clipboard_*`, `quic_transport::*`, `connect::*`）
- 新增 4 个回归测试全过：
  - `connect::tests::pong_health_timeout_relaxes_to_3_5s`
  - `connect::tests::pong_health_threshold_in_safe_range`（bounds 已更新到 `[5×PING, 30×PING]`）
  - `quic_transport::tls::tests::default_transport_config_keepalive_tighter_than_idle`（pin 2s / 30s + 不变量 `KEEPALIVE*2 < MAX_IDLE_TIMEOUT`）
  - `quic_transport::tls::tests::default_transport_config_supports_long_idle_bulk_conn_use_case`（≥ 20s 兜底）

### 4.2 真机验证（用户）

用 `RUST_LOG=lan_mouse=debug` 在主控端跑以下脚本：

1. **复现原 bug**：截一张全屏截图 → 立刻在主控端跨越边界 → 看 `[A] capture BeginPending` 与 `[C] stream A forwarder: Pong` 日志是否连续（之前是断 200–250 ms 的洞）。预期：连续 / 间隔明显变小。
2. **watchdog 不再误触**：连续截 3–5 张大图 → 检查 `[B] Pong health watchdog triggers = 0`。
3. **idle timeout 不再死 bulk conn**：空闲 30+ 秒不动 → 检查连接仍然 alive（之前 5s/5s combo 会在 10s 时死掉）。

记录到 `next/BUGS.md` 里"Bug #2（截图鼠标丢帧）"条目末尾。

---

## 5. M1 计划（下一里程碑）

如果 M0 真机验证后症状仍然残留，下一个要做的是：

**`STEP-FIX-BUGS-2-dispatch-image-off-localset`** — `Service::dispatch_image` 的 sha256 + cache clone 搬出 LocalSet：

```rust
async fn dispatch_image(&mut self, image: crate::clipboard::ImageBytes) {
    // 1) sha256 → spawn_blocking（保留 Vec<u8>，future 完事再 .await）
    let data = image.data;
    let sha = tokio::task::spawn_blocking(move || sha256_of_bytes(&data))
        .await
        .expect("sha256 task panicked");

    // 2) LRU + last_outbound_image_sha 比对（cheap）
    if self.image_lru_fingerprints.contains(&sha) { return; }
    if Some(&sha) == self.last_outbound_image_sha.as_ref() { return; }

    self.evict_prev_outbound_image_cache().await;

    // 3) cache insert 同时 move 原 bytes 进去（避免 clone 4–16 MB）
    let mut guard = self.clipboard_cache.lock().await;
    guard.insert(sha, image.data);
    drop(guard);

    // ... broadcast / frontend notify 不动 ...
}
```

预期 16 MB 截图从 ~250 ms LocalSet 占用降到 ~10 ms。配套加 `dispatch_image_does_not_starve_local_set_during_16mib_sha` 回归测试（仿 `apply_inbound_image_task_does_not_starve_local_set_during_slow_set_image` 写法）。

**M1 不做的**：
- 把 `dispatch_image` 整体搬出主 `select!`（需要重写 `Service::run` 主 select + 一堆现有测试，先看 M1 的小改是否够）
- macOS `current_image_async` 加 JPEG/TIFF passthrough（牵到下游 Windows/Linux 后端 mime 处理，独立 STEP）

---

## 6. 跟之前回答的关系

上一条回答里的"修法 1（dispatch_image 的 sha256/clone 搬 spawn_blocking）"现在变成 M1 的精确内容——在 M0 地基就位后，这是收尾的最后一步。

---

## 7. 落地记录

- 2026-09-12 M0 完成。`twice_fix` HEAD: `3b19a7a`（包含 ff-merge main 的 6 个 commit + 2 个手动应用 commit）
- 实机验证：待用户
