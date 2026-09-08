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

**现象**：本机 macOS (aarch64-apple-darwin) 无 `x86_64-linux-gnu-gcc` / `x86_64-w64-mingw32-gcc` 跨编译工具链；`cargo check --target x86_64-unknown-linux-gnu` / `--target x86_64-pc-windows-gnu` 都需要 C 交叉编译器（rcgen / quinn / ring 链路）。结果：
- ✅ macOS 编译 + 121 单测全绿
- ❌ Linux (x86_64-unknown-linux-gnu) 编译未本地验证（仅 trait + cfg 守门逻辑可静态检查）
- ❌ Windows (x86_64-pc-windows-msvc / -gnu) 编译未本地验证（同上）

**实际影响**：`src/clipboard/linux.rs` 与 `src/clipboard/windows.rs` 的 cfg-gate 模块在 macOS build 完全被排除，本地无法 type-check。代码本身按 windows-sys 0.61 / std::process API 严格类型化，但实机 xclip / OpenClipboard 调用是否能通过首次编译 / 真机 round-trip 需用户在 M1a 真机测试矩阵中验证。

**建议**：
- 短期：M1a 真机测试 (PLAN §8 M1a 矩阵) 由用户在 macOS / Linux / Windows 三平台分别跑一次 `cargo build` + `cargo test -p lan-mouse --lib clipboard`；任何编译失败回滚此处
- 中期：在 CI（GitHub Actions）加 `ubuntu-latest` + `windows-latest` matrix jobs，每次 push 触发
- 优先级 🟡：当前 STEP 已通过本地验证 + 代码 review；CI 加严是 M1b+ 工程改进
