# STEP-P2-M3a-3a.3-P1A — `resolve_unique_path` path traversal fix

> Follow-up to STEP-3a.3 (P1.A from `STEP-VALIDATION-P2-M3a-3a.3.md`)
> 执行日期：2026-09-13　实际耗时：~15 min
> 结论：✅ 通过（cargo build clean / 457 pass / 1 pre-existing fail / fmt 0 diff / clippy 无新 warning / 4 新单测）

## 1. 做了什么

### 1.1 改动文件

| 文件 | 改动类型 | 备注 |
|---|---|---|
| `src/service.rs` | **修改** | `resolve_unique_path` 增加 sanitization 入口；新增 `sanitize_filename` helper + `SANITIZED_FALLBACK_NAME` 常量；4 个新单测覆盖 traversal 场景 |

合计 service.rs: **+130 行 / -6 行**

### 1.2 关键设计点

#### `resolve_unique_path` 改造

```rust
pub(crate) fn resolve_unique_path(accept_dir: &Path, name: &str) -> PathBuf {
    let name = sanitize_filename(name);   // <-- 新增: 先清洗
    let candidate = accept_dir.join(&name);
    if !candidate.exists() { return candidate; }
    let path = Path::new(&name);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(&name);
    let ext = path.extension().and_then(|s| s.to_str());
    // ... collision suffix loop + timestamp fallback (sanitized name 全程使用)
}
```

#### `sanitize_filename` helper（新建）

```rust
fn sanitize_filename(name: &str) -> String {
    use std::path::Component;
    let segments: Vec<&str> = Path::new(name)
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();
    if segments.is_empty() {
        return SANITIZED_FALLBACK_NAME.to_string();
    }
    segments.join("_")
}

const SANITIZED_FALLBACK_NAME: &str = "untitled";
```

只保留 `Component::Normal(_)` 段，过滤掉：
- `Component::ParentDir` → `..`
- `Component::CurDir` → `.`
- `Component::RootDir` → `/`
- `Component::Prefix` → Windows drive prefix `C:\`

Survivor 用 `_` 连接成一个单段文件名。空段（`name = ".."` 等）回退到 `untitled`。

### 1.3 修复策略选择

按 Leader 决策：选 **Option A**（preferred per validator 第一修复路径）。

| Option | 评估 | 选择 |
|---|---|---|
| **A** `Component::Normal` 过滤 + `_` 连接 | 优雅处理多段文件名 + 保留 stem/extension 语义；与 `Path::components()` API 自然对齐 | ✅ |
| B canonicalize + starts_with 检查 | 增加 2 次文件系统 syscall；TOCTOU 风险；性能不如 A | ❌ |
| C 拒绝（返回 error） | 影响合法含 `..` 的边缘文件名（archive 提取）；需要改 `apply_files_inner` 接 `Result` | ❌ |

### 1.4 未触碰（scope 守纪）

- 不改 `apply_files_inner` 的 return type（仍是 `Ok(PathBuf)`，sanitize 后不会是 Err）
- 不改 `apply_inbound_files_task` 签名
- 不改 PLAN-2-CLIPBOARD.md（per Leader 指示）
- 不改 SUGGESTION.md（无新遗留项）
- 不动 Cargo.toml / 其他 crate
- 不动既有 `resolve_unique_path` 单测（4 个原测试仍 pass — sanitize 对它们 transparent）

## 2. 验证结果

### 2.1 全套门

| 闸门 | 命令 | 结果 |
|---|---|---|
| **Build** | `cargo build -p lan-mouse` | ✅ Finished `dev` profile (clean) |
| **Test (lan-mouse lib)** | `cargo test -p lan-mouse --lib` | ✅ **302 passed / 0 failed** (298 旧 + 4 新 traversal tests) |
| **Test (workspace lib)** | `cargo test --workspace --lib --no-fail-fast` | ✅ **457 pass / 1 pre-existing fail** (input-capture macos 预存 flake，与本 fix 无关；原 baseline 453 + 4 新 = 457) |
| **Test (resolve_unique_path 子集)** | `cargo test -p lan-mouse --lib resolve_unique_path` | ✅ **8/8 pass** (4 原 + 4 新) |
| **Format** | `cargo fmt --all -- --check` | ✅ 0 diff (exit 0) |
| **Clippy (lan-mouse)** | `cargo clippy -p lan-mouse --all-targets` | ✅ 新代码区 (lines 5111-5200 / 8286-8350) **0 warning** |

### 2.2 新单测覆盖

| 测试 | 输入 | 期望 sanitized | 期望 resolved path |
|---|---|---|---|
| `resolve_unique_path_strips_parent_dir_traversal` | `"../private.txt"` | `"private.txt"` | `<accept_dir>/private.txt` |
| `resolve_unique_path_flattens_subdir_separator` | `"subdir/file.txt"` | `"subdir_file.txt"` | `<accept_dir>/subdir_file.txt` |
| `resolve_unique_path_strips_double_parent_dir_traversal` | `"../../etc/passwd"` | `"etc_passwd"` | `<accept_dir>/etc_passwd` |
| `resolve_unique_path_keeps_normal_name_unchanged` | `"normal.jpg"` | `"normal.jpg"` (unchanged) | `<accept_dir>/normal.jpg` |

每个 traversal 测试额外 pin `resolved.parent() == Some(accept_dir)`，断言文件**确实**落在 accept_dir 下，不会逃逸。

## 3. 与 PLAN 的偏差

无。PLAN §0 Out of Scope 不受影响；本 fix 是 M3a STEP-3a.3 接续的 P1.A 修正（per validator dispatch）。

## 4. 处理的 SUGGESTION 项

无新增 / 无关闭。

P1.A 本身是 validator 报告中的 finding，不是 SUGGESTION.md 条目；fix 完成后该 finding 已消除。

## 5. 闸门检查

| 闸门 | 结果 |
|---|---|
| **时间门** | ✅ ~15 min（Plan 估时 30 min；远低于上限） |
| **milestone 边界门** | ✅ 0 触碰后续 M3a STEP-3a.4 / 3a.5 / M3b / M4 范围 |
| **闸 1 产物** | ✅ `resolve_unique_path` 改造 + `sanitize_filename` + 常量 + 4 测试 全部落地 |
| **闸 1 依赖** | ✅ STEP-3a.3 (commit `36b912b`) 已归档为通过 |
| **闸 1 验收** | ✅ `cargo test --workspace --lib` 457 pass / 1 pre-existing input-capture fail（与本 fix 无关） |
| **闸 2 偏差** | 无 |
| **闸 3 STEP 回归** | ⏭ skipped（非 milestone 收尾；M3a 在 3a.5 后整体回归） |

## 6. 遗留

无新增遗留项。P1.A 已消除。

接续契约：STEP-3a.4 写 HTTP/3 server `/clipboard/file/{sha256}` 路由时复用 `resolve_unique_path`，**自动**继承 sanitization 保护（无需重复修复）。

## 7. 下一步

按 PLAN §3 M3a 依赖顺序：

→ **STEP-3a.4**：HTTP/3 server `/clipboard/file/{sha256}` 路由 + 流式返回 + range stub + `set_stream_priority PRIORITY_BULK` + Pong watchdog RTT < 100 ms 单测。**P1.A fix (commit `347c6b6`) 已被 server 路由自动继承**。