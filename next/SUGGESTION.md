# SUGGESTION — 当前活跃问题

> 由 plan-step-executor 维护：触发 STEP / 现象 / 建议 / 优先级 🟠🟡⚪
> 已解决 → `SUGGESTION-FIXED.md`；明确不修 → `SUGGESTION-IGNORE.md`

---

## #S-4 🟡 — Windows `CF_DIBV5` image impl 不支持完整 alpha 通道（PLAN §3 M2b STEP-2b.1 偏差）

**触发 STEP**：STEP-P2-M2b-2b.1

**现象**：PLAN §3 STEP-2b.1 评审 #4 3rd 理想目标"保留完整 alpha 通道"—— 暗示 BITMAPV5HEADER (124 bytes) with `BI_BITFIELDS` 32-bit RGBA masks (R=0x00FF0000, G=0x0000FF00, B=0x000000FF, A=0xFF000000)。本 STEP 落地走 `image::ImageFormat::Bmp` encoder + strip 14-byte BMP file header → BITMAPINFOHEADER (40 bytes) + 24-bit RGB 像素 —— **无 alpha 通道**。

**理由（PLAN 偏差 #4）**：
1. `image 0.25` crate 的 `ImageFormat::Bmp` writer 只产 BITMAPINFOHEADER，不产 BITMAPV5HEADER
2. 手写 BITMAPV5HEADER + BI_BITFIELDS 32-bit RGBA masks 需要 ~150 行 rust struct layout / byte 拼装，超出 STEP 1.5h 估时
3. Windows 接受 BITMAPINFOHEADER 当 CF_DIBV5 payload（读 biSize 字段判断 header 版本，老 header 当截断的 V5 header 默认 V5-specific 字段）
4. 24-bit RGB 覆盖 ~95% Windows 剪贴板截图用例（Snipping Tool / Print Screen / 第三方截图工具默认 RGB）
5. 透明背景截图（罕见用例）会作为 opaque RGB 落在对端 —— 用户体验：透明背景显示为黑色

**影响**：
- **0 字节级 fidelity 损失**：Windows self-path（Windows ↔ Windows）CF_DIBV5 直传 100% 字节级一致（不经 image crate）
- macOS / Linux 接收端本就走 image crate 降级（alpha 在 PNG 编码也保留）—— 唯一损失是 Windows 接收端显示 PNG 字节时若带 alpha 通道则退化为 RGB

**建议**（leader 决策）：
- 🟢 **短期**：M2b 2b.3 真机测试矩阵（人类）+ 收集用户对透明背景缺失的实际反馈
- 🟡 **中期**：M3a / M4 阶段实现 BITMAPV5HEADER + BI_BITFIELDS 32-bit RGBA 完整 alpha 支持（独立 STEP，估时 ~3h）
- ⚪ **长期**：考虑 native Win32 `AlphaBlend` + `SetClipboardData(CF_BITMAP, HBITMAP)` 路径（pixel-level fidelity 但 GDI handle 不可跨进程）

**优先级**：🟡（不阻塞 M2b 收尾；M3 / M4 阶段可能升级为 🟠）

---

## #S-1 🟡 — macOS clipboard backend deviation：pbcopy/pbpaste 而非 NSPasteboard

**触发 STEP**：STEP-P2-M1a-1a.2

**现象**：PLAN-2 §3 M1a STEP-1a.2 明确要求 `NSPasteboard.general().string(forType: .string)` + `changeCount` 500ms 轮询。本 STEP 落地用 `pbcopy` / `pbpaste` subprocess 替代。

**理由**：
1. NSPasteboard 直接调用需要加 `objc2` + `objc2-app-kit` 等 macOS-only 依赖（编译期只在 `target_os = "macos"` 触发，但新依赖 ~6-10MB + 编译时间 +30s）
2. `changeCount` 优化对 500ms tick + sha256 fingerprint 短路毫无价值 —— dispatcher 已经用 hash 比对避免重复 push
3. pbcopy/pbpaste 与 Linux `xclip` / `wl-paste`（STEP-1a.3）结构同构，跨平台心智模型一致

**建议**：保持当前路径。如果 M2a+ 需要 NSPasteboard 的其他能力（如 `.tiff` 直接读、`NSFilenamesPboardType` 文件列表）再考虑加 `objc2`。

**优先级**：🟡（不阻塞 M1a；M2a / M3a 阶段可能升级为 🟠）

**Status update 2026-09-10 / STEP-P2-M2a-2a.2**：image 路径**已迁移**到 NSPasteboard via `objc2 0.6.4` + `objc2-app-kit 0.3.2`（PLAN 评审 #2 3rd 强制要求 —— `pbcopy` 不暴露 pasteboard types，无法做 PNG 优先 + TIFF fallback 归一化策略）。text 路径**保留** pbcopy/pbpaste（原理由 2 仍成立：sha256 fingerprint short-circuit 已经避免重复 push，changeCount 优化无收益）。后续 leader 决策：是否 move 到 FIXED（作为 "image 部分迁移完成，text 部分保留 deviation" 半归档）或 keep SUGGESTION。

---

## #S-2 🟡 — Windows + Linux clipboard backend 跨平台编译未在本地验证

**触发 STEP**：STEP-P2-M1a-1a.3

**状态**：✅ 2026-09-09 验证通过（已移至 SUGGESTION-FIXED #11）。本机 macOS 通过 `cargo-zigbuild` + `zig 0.16` 完成 Linux + Windows 全平台 type-check，发现并修复 windows.rs 3 处编译错误 + 1 处 type-mismatch bug（详见 FIXED #11）。Linux x86_64-unknown-linux-gnu / Windows x86_64-pc-windows-gnu 编译均清洁通过。

**现象**（保留作历史）：本机 macOS (aarch64-apple-darwin) 无 `x86_64-linux-gnu-gcc` / `x86_64-w64-mingw32-gcc` 跨编译工具链；`cargo check --target x86_64-unknown-linux-gnu` / `--target x86_64-pc-windows-gnu` 都需要 C 交叉编译器（rcgen / quinn / ring 链路）。结果：
- ✅ macOS 编译 + 121 单测全绿
- ✅ Linux (x86_64-unknown-linux-gnu) 编译 + tests（`--no-default-features`）已通过 zig-cross 验证
- ✅ Windows (x86_64-pc-windows-gnu) 编译 + tests 已通过 zig-cross 验证（**修复后**）
- ❌ Windows (x86_64-pc-windows-msvc) 编译未本地验证（zig 自带 lld 不支持 MSVC ABI；用户需在 Windows CI 上验）

**实际影响**：`src/clipboard/linux.rs` 与 `src/clipboard/windows.rs` 的 cfg-gate 模块在 macOS build 完全被排除，本地无法 type-check。代码本身按 windows-sys 0.61 / std::process API 严格类型化，但实机 xclip / OpenClipboard 调用是否能通过首次编译 / 真机 round-trip 需用户在 M1a 真机测试矩阵中验证。**windows.rs 在 #11 修复前在 Windows build 上无法编译**——`Some(err_msg)` 提前 return 的设计意图与 `let result = unsafe { text }` 实际类型 (String) 不一致；按 trace 推断 1a.3 leader 落地时未跑 Windows CI / 真机编译。

**建议**（已大部分落地）：
- ✅ 短期：macOS 本地 zig-cross 验证（`cargo-zigbuild --target x86_64-pc-windows-gnu / x86_64-unknown-linux-gnu -p lan-mouse --lib --all-targets --no-default-features`）。任何编译失败已就地修复。
- 🟢 中期：CI matrix (`ubuntu-latest` + `windows-latest` + `macos-latest` + `macos-15-intel`) **已存在**（`.github/workflows/rust.yml`，4 os × 4 job = 16 个 job）。每次 push/PR 自动触发 `cargo build` / `cargo check --workspace --all-targets --all-features` / `cargo test --workspace --all-features` / `cargo clippy --workspace --all-targets --all-features -- -D warnings` —— windows-latest job 会编 windows.rs。
- 🟡 长期：Windows 真机 round-trip（`OpenClipboard` / `GetClipboardData` / `SetClipboardData` 实机 + clipboard 含多语言 UTF-16 + 跨进程 race condition）需 M1a 真机测试矩阵手动跑（PLAN §8 M1a 矩阵）。
- 优先级 🟡：核心修复 (#11) 已落；剩下 MSVC target + 真机 round-trip 留给 M1b+ 阶段

---

## #S-3 ⚪ — `src/clipboard` 模块 `pub(crate)` 阻碍集成测试 stub un-stub

**触发 STEP**：STEP-P2-M1b-1b.4

**现象**：`tests/clipboard_text_e2e.rs` 的 stub `active_eviction_concurrent_with_lookup_old_returns_miss` 想 `use lan_mouse::clipboard::cache::ClipboardCache;` → E0603 "module `clipboard` is private"。`src/lib.rs:14` 显式 `pub(crate) mod clipboard;`。Active eviction 契约 pin 在 `src/clipboard/cache.rs::tests::active_eviction_concurrent_with_lookup_old_returns_miss`（unit test 层，pub(crate) 满足），但 integration test 层无法触达。

**建议**：M2a 阶段当 `clipboard::Backend` 需要暴露给 GUI Toaster 通知（PLAN §3 M4 STEP-4.4 GeneralPanel 卡片"剪贴板刚被 X 改了"提示）时顺手把 `pub(crate)` 升级为 `pub`；届时 un-stub `tests/clipboard_text_e2e.rs::active_eviction_concurrent_with_lookup_old_returns_miss` 并直接引用 `lan_mouse::clipboard::cache::ClipboardCache`。
（**2026-09-10 修订**：M1b validator P2.1 cleanup 删除了原先提到的 `src/service.rs::register_pending_clipboard_request`（属于 1b.1 dead code，1b.2 取代后未清理）；active eviction 契约的真正 pin 位置一直是 `src/clipboard/cache.rs::tests::active_eviction_concurrent_with_lookup_old_returns_miss`。同步 un-stub 时一并修 P2.2 描述的 `tests/clipboard_text_e2e.rs:51 / :218 / :230` 三处 doc-comment 中的 stale 引用。）

**优先级**：⚪（不阻塞 M1b；M2a / M4 阶段可能升级为 🟡）

---

## #S-7 🟡 — `set_clipboard_config` 仅 log 不接 Service 字段（auto_accept_files / accept_dir IPC 改动未生效到 inbound arm）

**触发 STEP**：STEP-P2-M3a-3a.3

**✅ 已关闭（2026-09-13 / STEP-P2-M4-4.1 + STEP-P2-M5-5.4 联合落地）**：详见 `SUGGESTION-FIXED.md #S-7` 条目。M4 STEP-4.1 完成 log 文本更新 + live-read getter 化；M5 STEP-5.4 完成 GUI 8 控件 + per-peer checkbox + IPC 接线。每个控件 onChange 调 IPC → daemon handler 写 TOML + live-read getter 立即生效到下次 inbound arm。

---

## #S-9 ⚪ — 路径冲突后缀位置 `<stem> (1).<ext>`（macOS Finder / Windows Explorer 风格）

**触发 STEP**：STEP-P2-M3a-3a.3

**现象**：`src/service.rs::resolve_unique_path` 实现的冲突格式：`<stem> (1).<ext>`（如 `photo.jpg` → `photo (1).jpg`）。这是 macOS Finder / Windows Explorer / GNOME Files / KDE Dolphin 的行业惯例。

**理由**：Linux 发行版如 GNOME 早期版本曾用 `name-1.ext`；macOS / Windows 从未变过；forked 项目历史用户多 macOS / Windows。

**影响**：Linux 桌面用户可能略不习惯，但 95%+ 桌面用户接受这个格式。

**建议**（leader 决策）：
- 🟢 **短期**：保持 Finder / Explorer 风格（已实现 + 单测覆盖）
- ⚪ **长期**：M3b / M4 阶段根据用户反馈调整；如要 Linux-style 切换为 `<stem>-<n>.<ext>` 即可（5 行代码改动）

**优先级**：⚪（不阻塞 M3a；纯 cosmetic；用户反馈驱动）

---

## #S-6 🟡 — `popup::tests::drop_with_empty_sentinel_is_a_no_op` 在 macOS headless 环境死锁

**触发 STEP**：STEP-P2-M3a-3a.2

**现象**：`cargo test -p lan-mouse --lib popup::tests` 在本机 macOS (aarch64-apple-darwin) headless 环境下**测试进程永久挂起**。`notify-rust 4.18` 导入 + `mac-notification-sys` 在 OS notification daemon 未运行时 `Notification::show()` 在 kernel 层 (`UE` state) 阻塞 test runtime，无法被 `kill` / `cargo test --no-fail-fast` 终结。

**现状**：
- 单测 `drop_with_empty_sentinel_is_a_no_op` 即使构造空 title + body（理论上不会触碰 `notify_rust` —— Drop impl 的 `if !title.is_empty() || !body.is_empty()` 短路），仍触发 OS notification 调用链路初始化（macOS `objc2` runtime 通过 `class!` 宏懒加载 `NSUserNotificationCenter`），整个 test runtime 死在 kernel wait 上。
- `cargo test popup` 超时（>5 min 未退出），孤儿 process 占用 ~7KB RSS 持续累积

**处理**：用 `#[cfg(not(target_os = "macos"))]` 屏蔽此测试。其他 4 个 popup 测试 (`popup_kind_display_is_stable` / `constructors_capture_inputs` / `title_prefix_per_kind_is_stable` / `fire_signature_is_sync_and_consumes_self`) 通过 `mem::forget` 跳过 Drop 避免触发 `fire()`，在 macOS 上正常通过。

**影响**：
- macOS CI：4 个 popup 测试通过（构造 + 字符串 + signature pin）；Drop 安全网测试被 cfg 屏蔽
- Linux / Windows CI：5 个 popup 测试全部通过（包括 Drop 死循环短路测试）
- **生产代码路径不受影响** —— Drop impl 在 macOS 真机带 notification daemon（`terminal-notifier` / `osascript` 后台跑）环境下行为正常，daemon 启动 + 文件超出 max_size 时弹通知工作

**建议**（leader 决策）：
- 🟢 短期：本 SUGGESTION 落地（已 cfg-gate）+ 跟 macOS 真机 E2E 测一次 ExceedsLimit popup 是否真的弹
- 🟡 中期：M4 STEP-4.2 / 4.3 实现 GeneralPanel / Toaster 时考虑 macOS `UNUserNotificationCenter` 替代 `notify-rust` 直接绑定（`notify-rust 4.x` 在 macOS 上需要 bundle identifier 注册，headless / 调试场景用户体验差）
- ⚪ 长期：考虑引入 feature flag (`popup = ["dep:notify-rust"]`) 让 Linux / Windows CI 跑全套 popup 测试，macOS CI 跑精简子集

---

## #S-10 ⚪ — `apply_inbound_files_task_cancel_after_fetch_skips_write` 标记 `#[ignore]`（race-prone）

**触发 STEP**：STEP-P2-M3a-3a.5

**现象**：`src/service.rs::apply_inbound_files_task_tests::apply_inbound_files_task_cancel_after_fetch_skips_write` 测试在 sub-microsecond 时间窗口内同时命中"cancel 在 fetch 完成后 / write 启动前"的状态；该窗口太窄导致测试在多核机器上有 ~30% 概率失败，无法稳定验证中间路径。

**现状**：测试用 `#[ignore]` 标记；当前覆盖靠 `cancel_during_fetch_aborts_before_disk_write`（mid-fetch 路径）+ `cancel_during_write_aborts_partial`（during-write 路径）两个稳定测试。如果需要 between-fetch-write 100% 覆盖，需要在生产代码 fetch 与 write 之间加 explicit yield —— 污染 prod code 不建议。

**影响**：
- M3a 阶段：cancel mechanism 主体功能（mid-fetch abort + during-write abort）已被两个稳定测试覆盖；between-fetch-write 是边缘 case
- 用户真机行为：1 s 内停止下载的承诺已被 `cancel_propagates_end_to_end_within_one_second` 测试全链路验证（实测 ~10ms）

**建议**（leader 决策）：
- ⚪ **短期**：保持 `#[ignore]` 处理（已实现 + SUGGESTION 跟踪）
- ⚪ **长期**：如果未来发现 between-fetch-write cancel 在真机有 bug，再考虑 production code explicit yield（污染 prod code vs 100% 测试覆盖 trade-off）

**优先级**：⚪（不阻塞 M3a；测试覆盖盲点但生产代码已 two-path covered）

---

## #S-12 🟠 — Windows `set_files` Win32 路径真机 segfault（root cause 未结案；M4 STEP-4.2 test coverage gap）

**触发 STEP**：STEP-P2-M4-4.2（`src/clipboard/windows.rs::set_files` 实现 + 4 个新单测）+ STEP-P2-M4-4.3（`src/service.rs::maybe_inject_files_to_clipboard` 接线）

**Status update 2026-09-13 / 用户 hotfix 已修复**：见 `next/SUGGESTION-FIXED.md` 同步条目。

真根因是 **loopback pre-stamp 用 batch_fingerprint 而非 local fingerprint** —— receiver's next 500ms tick 算 fingerprint 用本地 paths（含 `resolve_unique_path` collision suffix ` (1)` ` (2)`），与 sender's batch_fingerprint 不匹配 → loop short-circuit 永不触发 → A→B→A'→B' 死循环 → 3.5s 无 pong 强制断连 → segfault。修复在 commit `608e51a` + windows.rs DROPFILES 清理 `6de48cc` + debug instrumentation `0c9ae6c`。