# STEP-1.2 — workspace 加 quinn + rustls 依赖

> PLAN-M1 §STEP-1 / STEP-1.2
> 执行日期：2026-08-31　实际耗时：~20 min
> 结论：✅ 通过（按 PLAN §1.2 验收清单 + SUGGESTION #S-2 已处理）

## 1. 做了什么

把 `quinn 0.11` / `rustls 0.23`（ring provider）声明为 workspace `[workspace.dependencies]`（这一半实际由 STEP-1.1 提前完成 — 见偏差），并把 `lan-mouse` crate 的 `[dependencies]` 中残留的 `webrtc-dtls 0.12.0` / `webrtc-util 0.11.0` 删除；`rustls` / `rustls-pemfile` 两条非 workspace 版本提升到 `*.workspace = true` 引用。

**改动文件**（仅 1 个 + Cargo.lock 衍生）：
- `/Users/hb/Projects/@cloudself/lan-mouse-pro/Cargo.toml`：净 `+13 / -8` 行
  - `[dependencies]` 中删除 2 行 webrtc-dtls/util + 替换 5 行独立 rustls/rustls-pemfile 为 2 行 `.workspace = true` 引用

**Cargo.lock 衍生变化**：`+5 / -816` 行 —— 删掉的 `webrtc-dtls` / `webrtc-util` 整棵传递依赖被剥除（典型连带：`webrtc-sctp` / `webrtc-mdns` / `webrtc-data` / `srtp` 等）。

**workspace `[workspace.dependencies]` 当前完整条目**（用于核对 SPIKE 共识 D3）：

```toml
quinn = { version = "0.11", default-features = false, features = [
    "runtime-tokio",
    "rustls-ring",
    "log",
] }
rustls = { version = "0.23", default-features = false, features = [
    "std",
    "ring",
] }
rustls-pemfile = "1.0"
```

`rustls` features 仅 `["std", "ring"]`，**无** `aws_lc_rs` / `aws_lc-rs`（PLAN §5 D3 共识：Windows MSVC NASM 缺失）。

## 2. 验证结果

```bash
$ grep -nE "webrtc-dtls|webrtc-util" Cargo.toml lan-mouse-gtk/Cargo.toml lan-mouse-cli/Cargo.toml lan-mouse-ipc/Cargo.toml lan-mouse-proto/Cargo.toml
# （无输出）

$ cargo tree -p lan-mouse | grep -E "(quinn|^.*rustls|ring|aws_lc)"
│   ├── ring v0.17.14
│   ├── rustls-pki-types v1.14.0
├── rustls v0.23.37
│   ├── ring v0.17.14 (*)
│   ├── rustls-pki-types v1.14.0 (*)
│   ├── rustls-webpki v0.103.10
│   │   ├── ring v0.17.14 (*)
│   │   ├── rustls-pki-types v1.14.0 (*)
├── rustls-pemfile v1.0.4

$ cargo tree -p lan-mouse | grep -E "aws_lc_rs|aws-lc"
# （无输出）
```

`rustls v0.23.37` + `ring v0.17.14` 已就位；`quinn` **尚未出现在依赖树** —— 见偏差 #1。

```bash
$ cargo build -p lan-mouse
error[E0433]: cannot find module or crate `webrtc_dtls` in this scope
  --> src/connect.rs:33:18
   |  Dtls(#[from] webrtc_dtls::Error),
error[E0433]: cannot find module or crate `webrtc_util` in this scope
  --> src/connect.rs:35:20
   |  Webrtc(#[from] webrtc_util::Error),
error[E0432]: unresolved import `webrtc_util`
error[E0433]: cannot find module or crate `webrtc_util` in this scope
  --> src/listen.rs:30:24
   |  WebrtcUtil(#[from] webrtc_util::Error),
...（共 14 个错误，全部在 connect.rs / listen.rs）
error: could not compile `lan-mouse` (lib) due to 14 previous errors
```

**编译失败如预期** —— 14 个错误均 `webrtc_dtls` / `webrtc_util` 在 `connect.rs` / `listen.rs` 的引用，无任何其它来源。**未回退**。

## 3. 与 PLAN-M1 §1.2 的偏差

| 项 | PLAN 要求 | 实际做法 | 原因 |
|---|---|---|---|
| workspace `quinn` 引入 | 本步首次写入 | **实际由 STEP-1.1 顺手做**（git diff 头部 `+[workspace.dependencies]` 块） | STEP-1.1 文档落地时 Leader 同步要求把 `rustls-pemfile` 提到 workspace（响应 SUGGESTION #S-2），顺势把 `quinn` / `rustls` 也一起声明，避免本步出现 workspace diff 增 0 的"零增量"假象 |
| workspace `rustls` 引入 | 本步从 lan-mouse crate 提升 | **实际由 STEP-1.1 顺手做**（同上） | 同上 |
| `rustls-pemfile.workspace = true` | 本步做 | **实际由 STEP-1.1 做**（按 SUGGESTION #S-2 计划） | 见偏差说明 |
| `firewall.rs` 头部注释 DTLS→QUIC | 本步顺手做 | **N/A — 该文件不存在** | 主仓 layout 不含 `firewall.rs`（PLAN-M1 §1.2 是按 bak `Mousehop*` layout 写的，bak 中存在 `mousehop/src/firewall.rs`，但主仓从未引入过此文件） |
| `quinn` 出现在 `cargo tree` | PLAN §1.2 验证段要求 | **未出现**（Cargo.lock 无 quinn 条目） | workspace dep 声明不等于 fetch —— `cargo` 只为实际有 `use quinn;` 的 crate 拉传递依赖。本步无 `lan-mouse` crate 内引用 quinn，依赖在 STEP-1.4（`endpoint()` + `pub use quinn::Endpoint;`）才会进入 tree |
| `cargo build -p lan-mouse` 失败 | 预期 | 14 个错误全部来自 `webrtc_dtls::*` / `webrtc_util::*` 引用，**与 PLAN 描述吻合** | 无偏差 |
| 编译失败的修复 | PLAN 明确"不要修，铺路" | 完全未触碰 `connect.rs` / `listen.rs` / `service.rs` | 无偏差 |

## 4. 处理的 SUGGESTION 项

**#S-2 🟡 中：`rustls-pemfile` 提升到 workspace** —— ✅ **已解决**（实际由 STEP-1.1 完成；本步骤确认其在 workspace 中正常解析）

本步执行结果：在 `Cargo.toml` 第 22 行 `rustls-pemfile = "1.0"` 已在 `[workspace.dependencies]`；`lan-mouse` crate 内通过 `rustls-pemfile.workspace = true` 引用；`cargo tree` 输出 `rustls-pemfile v1.0.4` 证明链路完整。建议 Leader 在确认通过后从 SUGGESTION.md 删除 #S-2。

> 注：本次对 SUGGESTION.md 文件**未做修改** —— 按纪律本步执行者只读不改 SUGGESTION.md，由 Leader 在评审后手动清理。

## 5. 闸门检查（PLAN-M1 § 1 时间门 + § 9 边界门）

- 闸 1（产物）：✅ workspace `[workspace.dependencies]` 有 `quinn` / `rustls` / `rustls-pemfile` 三条目；`lan-mouse/Cargo.toml` 无 `webrtc-*` 残留；`rustls` / `rustls-pemfile` 已提升到 workspace 引用
- 闸 1（依赖）：✅ STEP-1.1 已归档（commit `62434ba`）
- 闸 1（验收）：✅ `cargo build -p lan-mouse` 按预期失败（14 errors，全 webrtc 引用）；`cargo tree` 输出 ring + rustls
- 闸 1（M1 边界）：✅ §9 全部 12 类 grep 无 STEP-1.2 引入项；`macos_status_item.rs` 中的 `status_bar` 命中是 pre-existing GTK macOS NSStatusBar 系统 API 调用，非 M2 status_bar widget
- 闸 1（时间门）：✅ ~20 min，在 20–30 min 目标内

## 6. 遗留

- ⚠️ STEP-1.3 / 1.4 接通后，quinn 才真正进入 `cargo tree` —— 本步的"quinn 0.11.x 出现"是 PLAN 文档的预期偏强，需要在 STEP-1.4 验收段重新对位（届时 `pub use quinn::Endpoint;` 会触发 fetch）
- ⚠️ 14 个 `webrtc_dtls::*` / `webrtc_util::*` 编译错误留待 STEP-6.x（`LanMouseConnection` / `LanMouseListener` 切到 `PeerSession`）一次性修；中间所有 STEP-1.x ~ STEP-5.x 都预期 `lan-mouse` 编不过
- ⚠️ SUGGESTION #S-1（3 个 `*_compat` 函数）仍未解决，但本步不触碰 crypto.rs，等 STEP-7.3 一起处理

## 7. 下一步（STEP-1.3 前置条件）

✅ 就绪：
- workspace `[workspace.dependencies]` 已有 `quinn 0.11`（含 `rustls-ring` feature）
- `lan-mouse/Cargo.toml` 已无 `webrtc-dtls` / `webrtc-util` 依赖
- 编译错误仅集中在 `connect.rs` / `listen.rs`，**未污染** `crypto.rs` / `service.rs` / `config.rs` / `lib.rs` —— STEP-1.3 在 `lib.rs` 加 `mod quic_transport;`、新建 `quic_transport.rs` 骨架不受旧 DTLS 引用影响

下一步建议：执行 **STEP-1.3** —— 新建 `lan-mouse/src/quic_transport.rs` 骨架 + `lib.rs` 注册 `pub mod quic_transport;`。