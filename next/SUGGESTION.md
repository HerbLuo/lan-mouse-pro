# SUGGESTION — 当前活跃问题

> 由 plan-step-executor 维护：触发 STEP / 现象 / 建议 / 优先级 🟠🟡⚪
> 已解决 → `SUGGESTION-FIXED.md`；明确不修 → `SUGGESTION-IGNORE.md`

---

（本文件当前无活跃项。M3-3.2-FIXUP 暴露的 #6 / #7 / #8 已修复并归档到 SUGGESTION-FIXED.md；详见 STEP-M3-3.2-FIXUP2 报告。）

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

**现象**：`tests/clipboard_text_e2e.rs` 的 stub `active_eviction_concurrent_with_lookup_old_returns_miss` 想 `use lan_mouse::clipboard::cache::ClipboardCache;` → E0603 "module `clipboard` is private"。`src/lib.rs:14` 显式 `pub(crate) mod clipboard;`。Active eviction 契约 pin 在 `src/service.rs::register_pending_clipboard_request`（unit test 层，pub(crate) 满足），但 integration test 层无法触达。

**建议**：M2a 阶段当 `clipboard::Backend` 需要暴露给 GUI Toaster 通知（PLAN §3 M4 STEP-4.4 GeneralPanel 卡片"剪贴板刚被 X 改了"提示）时顺手把 `pub(crate)` 升级为 `pub`；届时 un-stub `tests/clipboard_text_e2e.rs::active_eviction_concurrent_with_lookup_old_returns_miss` 并把 `src/service.rs::register_pending_clipboard_request` 的 body 搬过去。

**优先级**：⚪（不阻塞 M1b；M2a / M4 阶段可能升级为 🟡）
