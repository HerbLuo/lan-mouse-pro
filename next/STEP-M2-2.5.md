# STEP M2-2.5 — Capture trait monitors 快照整合

> PLAN §M2 / STEP-2.5
> 执行日期：2026-09-06　实际耗时：~20 min
> 结论：✅ 通过

## 1. 做了什么

把各 backend 已有的 `current_monitors()` 快照能力接入 `Capture` trait，并补上 `InputCapture` 的公共转发面。

**改动文件**：

- `input-capture/src/lib.rs`
  - re-export `geometry::MonitorInfo`，使 `InputCapture::monitors()` 的返回类型可直接从 crate 根使用。
  - 为 `Capture` 增加快照式 `fn monitors(&self) -> Vec<MonitorInfo>`，默认返回空列表，保持可选 / synthetic backend 的兼容性。
  - 增加 `InputCapture::monitors()`，直接转发到 trait object。
  - 为现有 `OneShotCapture` 测试 backend 实现 monitor 快照，并增加转发单测。
- `input-capture/src/macos.rs`
  - `Capture for MacOSInputCapture` 的 `monitors()` 转发到既有 `current_monitors()`。
- `input-capture/src/windows.rs`
  - `Capture for WindowsInputCapture` 的 `monitors()` 转发到既有 `current_monitors()`。
- `input-capture/src/layer_shell.rs`
  - `Capture for LayerShellInputCapture` 的 `monitors()` 转发到既有 `current_monitors()`。
- `input-capture/src/libei.rs`
  - `Capture for LibeiInputCapture` 的 `monitors()` 转发到既有 `current_monitors()`。
- `input-capture/src/dummy.rs`
  - dummy backend 显式实现 `monitors()`，返回空列表，保持“无设备”语义。

macOS 的 `ProducerEvent::MonitorsChanged` / `DisplayReconfigured` 发布路径与 Windows 的 `DISPLAY_RESOLUTION_GENERATION` / `update_display_regions` 发布路径均已在 STEP-2.2/2.3 中实现，本步只验证并复用，不重写触发逻辑。Linux 两个 backend 同样只复用既有 watch channel 快照。`monitor_changes()` 订阅面保留给 STEP-2.6 service 层使用。

本步没有新增 `geometry::MonitorInfo` 与 `lan_mouse_ipc::MonitorInfo` 之间的 `From` trait；镜像转换留给 STEP-2.6 service 层处理。

## 2. 验证结果

- `cargo build -p input-capture` → 通过。
- `cargo test -p input-capture --lib` → 47 passed; 0 failed，包含 `input_capture_monitors_delegates_to_backend`。
- `cargo check -p input-capture --no-default-features --features layer_shell,libei` → exit 0。当前为 macOS 开发机；`build.rs` 按既有规则不会在 macOS 设置 Linux backend cfg，因此该命令验证的是 feature 配置可解析，非真实 Linux 编译。
- `cargo check -p input-capture --no-default-features` → exit 0，dummy-only 配置通过。
- `cargo clippy -p input-capture --all-targets -- -D warnings` → exit 0。
- `cargo fmt --check -p input-capture` → exit 0。
- Windows 没有独立 Cargo feature；Windows backend 由 `cfg(windows)` 控制。按约定未在 macOS 上进行真实 cross-compile，Windows FFI 集成验证留给目标机 / STEP-2.7。

## 3. 与 PLAN 的偏差

无 PLAN 偏差。

- trait 默认实现与 PLAN 建议的空列表语义一致；五个指定 backend 都显式覆盖或实现该方法。
- `MonitorInfo` root re-export 是为了让新增公共 `InputCapture::monitors()` 返回类型可用，不改变数据结构或 wire API。
- 未修改 backend 私有枚举函数、热插拔触发逻辑、`src/service.rs` 或 `lan-mouse-proto`。

## 4. 处理的 SUGGESTION 项

无 SUGGESTION 项变更。`next/SUGGESTION.md` 当前为空；未新增、关闭或迁移条目。

## 5. 闸门检查

| 闸门 | 结果 |
|---|---|
| 产物对得上吗 | ✅ `Capture::monitors()`、`InputCapture::monitors()`、macOS / Windows / layer_shell / libei / dummy 五个 backend 实现、转发单测全部到位 |
| 依赖对得上吗 | ✅ STEP-2.1 至 STEP-2.4 均已归档通过；各 backend 的 `current_monitors()` 已存在 |
| 验收对得上吗 | ✅ `cargo build -p input-capture`、47 个 crate 单测、Linux feature check、clippy、package fmt check 全部通过 |
| milestone 边界门 | ✅ 未触碰 STEP-2.6 service reconcile、M3/M4 或 M5+；未改 enumerate 逻辑或协议 |
| 时间预算门 | ✅ 约 20 min，低于 STEP 估时 30 min，未触发拆步 |
| 闸 3 milestone 收尾 | ⏸ 跳过；STEP-2.5 不是 M2 收尾步骤，完整 workspace 回归留给 STEP-2.7 |

## 6. 遗留

- Linux backend 在 macOS 上无法进行真实 cfg 编译；需在 Linux 环境或 CI 执行对应 feature 的真实 build/check。
- Windows backend 真实 FFI 编译与显示器拔插行为留给 Windows 目标机和 STEP-2.7 人类验证。
- `monitor_changes()` 订阅转发、IPC `MonitorInfo` 镜像转换与 reconcile 仍由 STEP-2.6 负责。
- STEP-2.2 fixup 留下的 macOS doc 注释重复问题未触碰，按 Leader 状态作为 P2 cosmetic backlog 保留。

## 7. 下一步

执行 **STEP-2.6**：service 层订阅 / 轮询 monitors 变更，转发 `FrontendEvent::MonitorsChanged`，并实现 active client 的 `BindingInvalid` / barrier reconcile。
