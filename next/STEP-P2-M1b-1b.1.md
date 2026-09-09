# STEP-P2-M1b-1b.1 — StreamC clipboard text inline/meta split

> PLAN §M1b / STEP-1b.1
> 执行日期：2026-09-09　实际耗时：~55 min
> 结论：⚠️ 通过但有偏差（功能与本步测试通过；workspace Clippy 被既有 lint 阻断）

## 1. 做了什么

- `lan-mouse-proto/src/lib.rs`
  - 保留既有 wire-compatible `ClipboardText { fingerprint, sha256, size, content_inline }` 结构，不新增破坏旧 StreamC 解码的事件编号。
  - 新增 `CLIPBOARD_TEXT_INLINE_LIMIT = 1024` 与 `ClipboardText::from_content(...)`，统一按字节长度选择内联或 metadata-only；新增 `is_inline()`。
  - 补充 Inline、Meta、`ClipboardRequest` 顶层 `Vec<u8>` encode/decode round-trip，以及 1024 / 1025 / 100 KiB / 1 MiB 四档边界测试。Meta 测试还校验 wire payload 未保留大文本字节。
- `src/service.rs`
  - outbound 500 ms tick 与 recover push 统一使用协议构造器；`<= 1024` 字节发送 `content_inline`，`>= 1025` 只发送 metadata。
  - 新增 `pending_clipboard_requests: HashMap<[u8; 32], ()>`。inbound metadata 在 backend 检查前注册 SHA-256；新 metadata 会替换旧 pending hash，避免本步尚未消费时无限增长；本步不创建 `ClipboardRequest`、不发 HTTP/3 GET。
  - 增加 pending 注册单测。
- `src/quic_transport/protocol.rs`
  - 增加 7 个 StreamC 变长事件均路由到 `Channel::StreamC` 的回归测试。
- `src/listen.rs`
  - `cargo fmt --all` 修正一处既有函数签名排版；无运行时逻辑改动。
- `next/SUGGESTION-IGNORE.md`
  - 补充本机当前 Clippy 版本报告的既有 `doc_lazy_continuation` / `assertions_on_constants` 位置，留给独立 lint cleanup。

## 2. 验证结果

- `cargo build --workspace`：通过。
- `cargo test -p lan-mouse-proto`：29 passed / 0 failed。
- `cargo test -p lan-mouse --lib`：128 passed / 0 failed。
- `cargo test --workspace`：293 passed / 0 failed。
- `cargo fmt --all -- --check`：通过，无输出。
- `cargo clippy --workspace --all-targets -- -D warnings`：未通过；只报告既有位置：`src/connect.rs`（`doc_lazy_continuation`、`assertions_on_constants`、`too_many_arguments`）、`src/quic_transport/endpoint.rs`、`src/quic_transport/session.rs`、`src/service.rs:105-109`。本次新增行未命中，已按既有 `SUGGESTION-IGNORE.md` 记录不越界修复。

## 3. 与 PLAN 的偏差

- 功能性偏差：无。既有 `content_inline: Option<Vec<u8>>` 已是 M0a wire 契约，本步采用 `ClipboardText::from_content` 固化 Inline / Meta 分流，而不是引入新的嵌套事件变体，避免破坏已落地的 StreamC 编码。
- 按调用方确认，本步未触发 HTTP/3 GET，未实现源端 cache 失效，也未做跨机器或全栈 StreamC round-trip；这些留给 1b.2 / 1b.4。
- 全 workspace Clippy 仍受历史 lint 阻断，属于环境 / 基线偏差，不是本步引入。

## 4. 处理的 SUGGESTION 项

- 无活跃建议需要修复。
- 在 `SUGGESTION-IGNORE.md` 的既有 lint 忽略项中补充了当前工具链显示的历史错误位置。

## 5. 闸门检查

- 时间门：约 55 min，未超过 1.5 h 总预算。
- milestone 边界门：未触碰 M1b.2 的 HTTP/3 拉取、源端 cache 失效、LRU TTL/metrics 或 M1b.4 真机范围。
- 产物门：协议构造器、dispatcher 分流、pending 注册、StreamC 路由回归和四档测试均已落地。

## 6. 遗留

- `pending_clipboard_requests` 目前只注册最新 metadata SHA-256，不主动发送 `ClipboardRequest`；1b.2 需要消费并清理该 map，并接入 `Http3Client::get_text`。
- 大文本 source cache 与 push 前旧 fingerprint 删除仍未实现，按评审 #3 留给 1b.2。
- workspace Clippy 需要独立基线 cleanup；本步不改非关键路径历史 lint。
- 三平台真实剪贴板与双向大文本验收留给 1b.4 / 人类测试。

## 7. 下一步

- `STEP-1b.2`：接收端消费 pending metadata，发送 `ClipboardRequest` / HTTP/3 GET；实现 source cache 与 push 前旧 fingerprint 失效，并覆盖 404 静默处理。
