# STEP 2a.1 — ClipboardBackend 加图片方法

> PLAN §3 M2a / STEP-2a.1
> 执行日期：2026-09-10　实际耗时：~30 min
> 结论：✅ 通过

## 1. 做了什么

### 1.1 改动文件
- `src/clipboard/mod.rs` — 新增 image 类型 + mime 检测 + trait 默认实现 + 5 个新单测 + 1 个 display-message 单测更新

### 1.2 新增的公开类型与函数（全部 `pub`）
- `ImageBytes { mime: String, data: Vec<u8> }` — read 路径返回 / write 路径接收的图像字节 + MIME 字符串
- `Mime` enum（`Png` / `Jpeg` / `Bmp`）+ `mime_str() -> &'static str` + `from_label(&str) -> Option<Self>` + `Display` impl
- `ImageChange { bytes: ImageBytes }` — `watch_image` 流的事件
- `mime_from_magic(bytes: &[u8]) -> Option<Mime>` — PNG 8-byte / JPEG 3-byte / BMP 2-byte magic 检测

### 1.3 新增的 error 变体
- `ClipboardError::Unsupported(String)` — trait 默认 `set_image` 用，message 为 `"clipboard operation not supported by this backend: {0}"`

### 1.4 扩展的 trait 方法（全部带默认实现，不破坏现有 backend）
```rust
fn current_image(&mut self) -> Option<ImageBytes> { None }
fn set_image(&mut self, _bytes: &[u8], _mime: Mime) -> Result<(), ClipboardError> {
    Err(ClipboardError::Unsupported(
        "image write not implemented for this backend (M2a/M2b in flight)".into(),
    ))
}
fn watch_image(&mut self) -> futures::stream::BoxStream<'static, ImageChange> {
    Box::pin(futures::stream::empty())
}
```

### 1.5 关键设计决策
- **`BoxStream` 复用 `futures` crate（已在依赖中）** — `futures::stream::BoxStream<'static, _>` + `futures::stream::empty()`，不引入新 dep（`tokio-stream` 仅 `lan-mouse-ipc` 使用，避免破坏 M2a 阶段的最小变更原则）
- **`Mime` 走 enum + `from_label`**，read 路径用 `String` —— 兼容 `application/x-dib` / `image/gif` / 未来 format（PLAN §3 M2a 限定 PNG/JPG/BMP，但 read 路径 forward-compatible）
- **默认 `set_image` 返回 `Unsupported` 而非 panic** —— dispatch / test 任何路径错误调用都能优雅失败
- **保留所有现有 platform 实现不变**（`macos.rs` / `linux.rs` / `windows.rs`）—— 现有 backend 自动继承默认实现，无需修改

### 1.6 新增单测（5 个 + 1 个扩展）
- `mime_from_magic_recognises_png_jpeg_bmp` — PNG/JPEG/BMP magic 识别
- `mime_from_magic_returns_none_for_unknown` — 空 buffer / 文本 / GIF / TIFF / WebP / 单 byte / 错误前缀都返回 None
- `image_bytes_can_round_trip_through_struct` — ImageBytes 字段 round-trip + PartialEq + 空 data 边界
- `dummy_backend_current_image_returns_none_by_default` — DummyBackend 继承默认实现 + text/image 互不干扰
- `dummy_backend_set_image_returns_unsupported` — DummyBackend::set_image 默认返回 Err(Unsupported)
- `mime_str_returns_canonical_wire_labels` — mime_str() + Display 稳定性
- `mime_from_label_round_trips_known_values` — round-trip + 大小写敏感
- 更新 `clipboard_error_display_messages_are_stable` 增加 Unsupported 行

## 2. 验证结果

### 2.1 全 workspace 测试

```
$ cargo test --workspace --no-fail-fast
...
test result: ok. 101 passed
test result: ok. 0 passed
test result: ok. 0 passed
test result: ok. 179 passed    # lan-mouse lib (172 baseline + 7 new)
test result: ok. 0 passed
test result: ok. 2 passed
test result: ok. 7 passed
test result: ok. 2 passed
test result: ok. 0 passed
test result: ok. 26 passed
test result: ok. 29 passed
...
```

总 pass：346（baseline 339 + 7 new）；**0 fail**。

### 2.2 lib build
```
$ cargo build -p lan-mouse --lib
Finished `dev` profile [unoptimized + debuginfo] target(s) in 2.80s
```

### 2.3 fmt / clippy
- `cargo fmt --check src/clipboard/mod.rs` —— **0 diff**（本步新增代码格式清洁）
- `cargo clippy --workspace --all-targets -- -D warnings` —— mod.rs **0 新增 lint**（10 pre-existing `doc_lazy_continuation` / 1 pre-existing `too_many_arguments` 跨文件，与本步无关；详见 STEP-VALIDATION-P2-M1b §6 与 cleanup 报告）

## 3. 与 PLAN 的偏差

无（完全按 PLAN §3 M2a STEP-2a.1 任务描述执行）。

可选偏差：**`watch_image` 默认返回 `BoxStream<'static, _>` 而非 `BoxStream<'a, _>`** —— PLAN 表格简写省略了 lifetime；本步选 `'static` 是为了让默认 empty-stream 实现不依赖 `&mut self` 的 borrow，平台 impl 后续可自由 override。

## 4. 处理的 SUGGESTION 项

无新增 / 移出 / 移入。SUGGESTION.md / SUGGESTION-FIXED.md 未受影响。

注：本步执行期间，并行 STEP-P2-M1b-CLEANUP（commit `503bd8a`）同时落地，删除了 `src/service.rs` 的 `pending_clipboard_requests` dead code（P2.1 backlog 收尾），未与本步产生文件冲突。

## 5. 闸门检查

| 检查 | 结果 |
|---|---|
| 产物对得上吗 | ✅ `ImageBytes` / `Mime` / `ImageChange` / `mime_from_magic` / `ClipboardError::Unsupported` / 3 个 trait image 方法 全部就位 |
| 依赖对得上吗 | ✅ 不依赖任何未完成 STEP（仅依赖已归档的 M0a/M0b/M0c + M1a/M1b） |
| 验收对得上吗 | ✅ `cargo test --workspace` 全绿；mime 检测单测覆盖 PNG/JPG/BMP + 非图片返回 None |
| milestone 边界门 | ✅ 未触碰 2a.2（macOS backend 实现）/ 2a.3（outbound dispatcher）/ 2a.4（inbound 回环）等后续 STEP 范围；macos.rs / linux.rs / windows.rs 未改 |
| 时间门 | ✅ ~30 min（远低于 1h 阈值） |

## 6. 遗留

1. **跨平台编译验证仅本机 macOS**：本机 macOS aarch64 build + 121 单测全绿。Linux / Windows target 本机未验（与 M1a baseline 一致 —— 跨平台验证留 CI / `cargo-zigbuild`）。
2. **`BoxStream` lifetime 选择**：选了 `'static`（简单 + 默认空流不依赖 borrow），平台 impl 可在 STEP-2a.2 / 2b.1 / 2b.2 override 为更短的 lifetime if needed。
3. **未实现 `text_image` 复合类型**：M2a STEP-2a.1 仅 image；M2b/M3a 阶段不增加（PLAN §0 Out of Scope）。

## 7. 下一步

按依赖顺序：**STEP-2a.2**（macOS backend image 实现 + TIFF→PNG 归一化，PLAN §3 评审 #2 3rd）。
