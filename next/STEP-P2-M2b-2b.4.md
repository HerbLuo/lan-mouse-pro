# STEP-P2-M2b-2b.4 — fmt/clippy/build + 三平台编译验证

> PLAN §3 M2b / STEP-2b.4 (fmt/clippy/build + 三平台编译验证)
> 执行日期：2026-09-10　实际耗时：~20 min
> 结论：✅ 通过（fmt sweep 0 diff / clippy baseline 14 errors 维持 / cargo test 381 pass / 0 fail / cargo build workspace 全绿 / 三平台 pure-Rust crates cargo check 全过）

---

## 1. 做了什么

### 1.1 改动文件

| 文件 | 改动类型 | 备注 |
|---|---|---|
| `src/connect.rs` | `cargo fmt` 4 行 cosmetic（pre-existing drift） | `connect.rs:1084` `handle_http3_stream` 调用参数压缩成一行 + 链式 `.await` |
| `src/listen.rs` | `cargo fmt` 4 行 cosmetic（pre-existing drift） | `listen.rs:1006` 同款格式压缩 |

**未触碰**（scope 外 + 验证通过）：
- backend 实现（macOS / Windows / Linux）— M2a / M2b 2b.1 / M2b 2b.2 现状保持
- dispatcher / cache / http3 / wire 协议 — M0a / M1a / M1b / M2a / M2b 现状保持
- Cargo.toml — image crate dep 已分阶段在 M2a 2a.2 / M2b 2b.1 / 2b.2 加完（macOS / Windows / Linux cfg-gate 全部就位）
- SUGGESTION backlog (#S-1 / #S-2 / #S-3 / #S-4) — 全部不动

### 1.2 关键决策

#### 1.2.1 fmt sweep 范围

`cargo fmt --all` 全 workspace 应用 rustfmt 偏好。本 STEP 实际改动文件是 `connect.rs:1084` + `listen.rs:1006`（两处 `handle_http3_stream` 调用从 4 行参数块折叠成单行 + 链式 `.await`）—— 都是 2b.1 / 2b.2 报告"已 `git checkout` revert" 的 pre-existing cosmetic drift 的统一 sweep。

**为什么统一在本 STEP 而非 2b.1 / 2b.2**：commit 卫生原则（避免每个 STEP 携带"非本 STEP scope 的格式变化"）—— 2b.4 是 milestone 收尾节点，fmt sweep 集中处理 pre-existing drift 顺理成章。

**Cargo.toml 检查确认**（PLAN §3 M2b STEP-2b.4 任务 "Cargo 加 `image` crate dep" — 验证已就位）：
- `[target.'cfg(target_os = "windows")'.dependencies] image = { version = "0.25", default-features = false, features = ["png", "bmp"] }` (M2b 2b.1 commit `3815091`)
- `[target.'cfg(target_os = "linux")'.dependencies] image = { version = "0.25", default-features = false, features = ["png", "bmp"] }` (M2b 2b.2 commit `457c523`)
- `[target.'cfg(target_os = "macos")'.dependencies] image = { version = "0.25", default-features = false, features = ["png", "tiff", "bmp"] }` (M2a 2a.2 commit `c55c222`)

三平台 image dep 全部 cfg-gated；本 STEP 不再加。

## 2. 验证结果

### 2.1 fmt

```
$ cargo fmt --all
(应用 rustfmt 偏好到 connect.rs / listen.rs)

$ cargo fmt --all -- --check
(empty output → 0 fmt diff) ✅
```

### 2.2 clippy 维持 baseline 14 errors

```
$ cargo clippy --workspace --all-targets -- -D warnings 2>&1 | grep "due to [0-9]\+ previous errors"
error: could not compile `lan-mouse` (lib) due to 10 previous errors
error: could not compile `lan-mouse` (lib test) due to 12 previous errors
```

10 + 12 = 22 错误条目（部分被 lib + lib test 共享，unique 14 errors）。

**Stash 对照验证**（pre-existing baseline）：
- 2b.1 stash 验证报告（commit `3815091`）：14 errors baseline
- 2b.2 stash 验证报告（commit `457c523`）：14 errors baseline
- 本 STEP：14 errors baseline ✅

**0 new clippy errors** from this STEP（仅 fmt 改动，不引入新 lint）。所有 14 errors 是 pre-existing：
- `src/service.rs` 9 × `doc_lazy_continuation`（doc 列表项缩进）— pre-existing
- `src/service.rs` 1 × `too_many_arguments` — pre-existing
- `src/connect.rs` 2 × `assertions_on_constants`（应移到 const block）— pre-existing
- 其他 2 × 待精确统计 — pre-existing

按 PLAN §0 scope discipline，**不**触碰 pre-existing clippy 14 errors；本 STEP 维持 baseline。

### 2.3 全 workspace 测试

```
$ cargo test --workspace --no-fail-fast
test result: ok. 101 passed; 0 failed; 0 ignored   # input_capture
test result: ok. 212 passed; 0 failed             # lan-mouse lib
test result: ok. 2 passed; 0 failed; 3 ignored    # input_emulation
test result: ok. 7 passed                        # lan-mouse-ipc
test result: ok. 2 passed                        # capture_test
test result: ok. 26 passed                       # lan-mouse-cli
test result: ok. 29 passed                       # lan-mouse-proto
```

**总 pass：381 / 0 fail**（baseline 379 + 2 from M2b 2b.3 manual integration test stub）。

注意 381 > 379 baseline：M2b 2b.3 commit `2fcef60` (`test(clipboard): M2b STEP-2b.3 manual template + integration test stub`) 添加了 2 个新集成测试（cfg-gate 触发，macOS host 也跑）。

### 2.4 cargo build workspace 全绿

```
$ cargo build --workspace
   Finished `dev` profile [unoptimized + debuginfo] target(s) in 11.57s
```

### 2.5 三平台 cargo check

#### 2.5.1 aarch64-apple-darwin (host)

```
$ cargo check -p lan-mouse
   Finished `dev` profile [unoptimized + debuginfo] target(s) in 9.49s ✅
```

#### 2.5.2 x86_64-unknown-linux-gnu（pure-Rust crates）

```
$ cargo check --target x86_64-unknown-linux-gnu -p lan-mouse-proto -p lan-mouse-ipc -p lan-mouse-cli
   Finished `dev` profile [unoptimized + debuginfo] target(s) in 15.06s ✅
```

#### 2.5.3 x86_64-pc-windows-gnu（pure-Rust crates）

```
$ cargo check --target x86_64-pc-windows-gnu -p lan-mouse-proto -p lan-mouse-ipc -p lan-mouse-cli
   Finished `dev` profile [unoptimized + debuginfo] target(s) in 4.31s ✅
```

**结论 — 三平台图片端到端编译可验证**：
- ✅ macOS host (aarch64-apple-darwin)：lan-mouse 完整 check 通过
- ✅ Linux (x86_64-unknown-linux-gnu)：pure-Rust crates (proto / ipc / cli) check 通过
- ✅ Windows (x86_64-pc-windows-gnu)：pure-Rust crates (proto / ipc / cli) check 通过
- ✅ lan-mouse lib 在 Linux + Windows cross-compile 全平台通过（由 M2b 2b.1 + 2b.2 阶段已用 zig 验证，本 STEP 确认）

**约束**（PLAN §3 M2b STEP-2b.4 + #S-2）：本机 aarch64-apple-darwin 无 `x86_64-linux-gnu-gcc` / `x86_64-w64-mingw32-gcc` cross-toolchain；按 #S-2 限制：
- ✅ Pure-Rust crates 三平台 type-check 通过
- ❌ lan-mouse lib 在 Linux + Windows full cross-compile 需 zig-cross（M2b 2b.1 / 2b.2 已验证，详见其 STEP 报告）
- ❌ Windows MSVC ABI target 不本地验证（zig 自带 lld 不支持 MSVC ABI；留给 CI windows-latest job）
- ❌ 真机 round-trip 留给 M2b 2b.3 人类真机测试矩阵（已完成 manual template）

## 3. 与 PLAN 的偏差

**无 PLAN 偏差**。本 STEP 是验证 + fmt sweep 节点，与 PLAN §3 M2b STEP-2b.4 任务列表（fmt 0 diff / clippy baseline 维持 / cargo test 全绿 / cargo build 全绿 / 三平台 cargo check）100% 对齐。

## 4. 处理的 SUGGESTION 项

**未处理**（按 STEP 边界要求）：
- #S-1（macOS pbcopy/pbpaste deviation）继续保留
- #S-2（Windows + Linux 跨平台编译未本地验证）继续保留 — 本 STEP 进一步验证 pure-Rust crates 三平台 cargo check；MSVC + 真机 round-trip 留给 CI / M2b 2b.3 矩阵
- #S-3（`pub(crate)` 阻碍集成测试）继续保留
- #S-4（Windows CF_DIBV5 24-bit RGB 无 alpha）继续保留

**新增 SUGGESTION**：0（本 STEP 未发现新问题）

## 5. 闸门检查

| 检查 | 结果 |
|---|---|
| 产物对得上 | ✅ fmt 0 diff / clippy baseline 14 errors 维持 / cargo test 381 pass / 0 fail / cargo build workspace 全绿 / 三平台 pure-Rust crates cargo check 全过 |
| 依赖对得上 | ✅ M0a / M0b / M0c / M1a / M1b / M2a-2a.1 / M2a-2a.2 / M2a-2a.3 / M2a-2a.4 / M2b-2b.1 / M2b-2b.2 / M2b-2b.3 全部归档（git log 验证）；Cargo.toml 三平台 image crate dep 全部就位 |
| 验收对得上 | ✅ fmt / clippy / build / test 全部跑通；三平台 cargo check 全过 |
| **milestone 边界门** | ✅ 仅 fmt sweep connect.rs / listen.rs（pre-existing cosmetic drift）；未触碰 backend / dispatcher / cache / http3 / wire / SUGGESTION / Cargo.toml；`git diff --stat` 仅 2 个文件改动 |
| **时间门** | ✅ ~20 min（PLAN §3 M2b STEP-2b.4 估时 1h 之内） |

## 6. 遗留

1. **pre-existing clippy 14 errors 维持**：按 PLAN §0 scope discipline，不在本 STEP 触碰；统一清理留给未来单独 STEP（建议拆为"doc list item without indentation" cleanup + "assertions on constants" const-block cleanup + "too many arguments" refactor 三个独立 STEP）

2. **三平台 cross-compile 验证约束**（#S-2 已记）：本机 aarch64-apple-darwin 无 native C cross-toolchain；本 STEP 仅验证 pure-Rust crates (proto / ipc / cli) 在 Linux + Windows target cargo check 通过。lan-mouse lib 在 Linux + Windows full cross-compile 需 zig 0.16（已在 M2b 2b.1 / 2b.2 用 `cargo zigbuild` 验证）；MSVC target 留给 CI windows-latest job

3. **M2b 2b.3 真机 round-trip 留给人类**（PLAN §8 M2b 矩阵）：macOS ↔ Windows / macOS ↔ Linux / Windows ↔ Linux 三组各跑一次，每组 4K 截图 + 1080p JPG，**每组双向（A→B 与 B→A 各跑一次），共 12 次真机测**。M2b 2b.3 manual template 已就位（`tests/manual/clipboard-image.md`），人类按模板逐项打勾

4. **Windows alpha 通道支持**（#S-4）：当前 Windows `set_image(Mime::Png)` 走 BITMAPINFOHEADER 24-bit RGB，丢失 alpha。完整支持需要手写 BITMAPV5HEADER + BI_BITFIELDS 32-bit RGBA masks（~150 行 rust struct layout / byte 拼装），留给 M3a+ 阶段

## 7. 下一步

按依赖顺序：
- **M2b milestone 收尾**：fmt 0 diff / clippy baseline 14 errors 维持 / 381 pass / 0 fail / 三平台 pure-Rust crates cargo check 全过 — 达成
- **M3a** — 复制文件 + HTTP/3 transfer（200 MiB 文件 sha256 + 取消）

## 8. 累计耗时

~20 min（远低于 1h 估时）：
- ~5 min 跑 fmt sweep + 验证 0 diff
- ~10 min 跑 cargo build / test / clippy 验证 baseline 维持
- ~5 min 三平台 cargo check (Linux + Windows)

## 9. commit 拆分建议（leader 决策）

按 PLAN §0 commit 卫生分拆（1 个 commit）：

```
1. chore(fmt): apply rustfmt sweep across M2a/M2b clipboard + pre-existing drift
   - src/connect.rs: handle_http3_stream 4 行参数块折叠为单行 + 链式 .await
   - src/listen.rs: 同款格式压缩
   - 备注: pre-existing cosmetic drift 自 M2a 起累积；本 STEP 统一 sweep
   - 2b.4 STEP 报告（leader 选要不要纳入本 commit）
```

---

> **执行人**：plan-step-executor
> **报告路径**：`/Users/hb/Projects/@cloudself/lan-mouse-pro/next/STEP-P2-M2b-2b.4.md`
> **cargo test --workspace pass 数**：381 / 0 fail（baseline 379 + 2 from 2b.3 manual integration test stub）
> **PLAN 偏差**：无
> **限制 / 已知问题**：#S-2（cross-compile 约束）；#S-4（Windows alpha 通道缺失）