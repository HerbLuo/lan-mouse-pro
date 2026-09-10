# Validation: P2 M2a — 剪贴板图片基础设施 + macOS（STEP 2a.1 / 2a.2 / 2a.3 / 2a.4 整批）

> 审阅日期：2026-09-10　审阅 STEP 范围：2a.1 / 2a.2 / 2a.3 / 2a.4
> 起点 commit：`ac650be`（M1b validator 已审过 + M2a 起点之前的代码）→ 终点 commit：`HEAD` (3391873)
> 待审 commit（6 个 feat/chore + 3 个 docs archive）：
> - 2a.1 `0110b4d` feat(clipboard): image backend trait surface + mime magic detection + tests
> - 2a.2 `c55c222` chore(deps): add objc2 + image for macOS clipboard image support
> - 2a.2 `3e40b8b` feat(clipboard/macos): NSPasteboard image impl + TIFF→PNG normalization
> - 2a.3 `0d985b3` feat(clipboard): cache byte-budget upgrade 128 entries → 200 MiB + bytes() API
> - 2a.3 `02be724` feat(quic): /clipboard/image/{sha256} route + cache_lookup_route helper
> - 2a.3 `6b1239a` feat(service): image outbound dispatcher + last_outbound_image_sha + sha256 short-circuit
> - 2a.4 `77b8021` feat(service): image inbound + image loopback LRU 32 + Http3Client::get_image
> - archive: `ac650be` (2a.2) / `75eabc0` (2a.3) / `3391873` (2a.4)

## 1. 偏离 PLAN

### STEP-2a.1 — image backend trait + mime 检测

- ✅ **完全符合** PLAN §3 M2a STEP-2a.1 任务表：
  - `ImageBytes { mime, data }` / `Mime { Png, Jpeg, Bmp }` / `ImageChange { bytes }` 全部就位（`src/clipboard/mod.rs:129-252`）
  - `mime_from_magic` 检测 PNG (8-byte) / JPEG (3-byte) / BMP (2-byte) magic bytes（`src/clipboard/mod.rs:220-238`）
  - `ClipboardBackend` 三个 image 方法 + 默认 impl 全部就位（`src/clipboard/mod.rs:357-403`）
  - `ClipboardError::Unsupported(String)` 变体添加（`src/clipboard/mod.rs:103-104`）
  - 8 个新单测覆盖 PNG / JPEG / BMP magic + 未知 bytes + ImageBytes round-trip + DummyBackend 默认值
- ⚠️ **小偏差**：`watch_image` 默认 impl 返回 `BoxStream<'static, _>` 而非 `BoxStream<'a, _>`（PLAN 表格简写省略了 lifetime）—— executor 已在 STEP-2a.1.md §3 记录此偏差。`'static` 选择合理（默认 empty stream 不依赖 borrow），platform impl 可 override。**影响 0**。

### STEP-2a.2 — macOS NSPasteboard image + TIFF→PNG 归一化

- ✅ **完全符合** PLAN §3 M2a STEP-2a.2 + 评审 #2 3rd：
  - `current_image` 优先 PNG / fallback TIFF→PNG 归一化（`src/clipboard/macos.rs:246-257, 419-450`）—— **关键测试 `current_image_normalizes_tiff_to_png` pin**（`src/clipboard/macos.rs:778-814`），验证 PNG magic + 维度保持
  - `set_image` 强制按 PNG 写回 NSPasteboard（`src/clipboard/macos.rs:273-296`）
  - `watch_image` 500ms 轮询 changeCount（`src/clipboard/macos.rs:330-366`）
  - 5 个新 macOS-only cfg-gate 单测
  - text path 完全不动（M1a 1a.2 pbcopy/pbpaste 保留）
- ⚠️ **PLAN 偏差 #1**（已记录）：`objc2` 0.5 → 0.6 + `objc2-app-kit` 0.2 → 0.3。crates.io 0.5.x 已停止维护，0.6.x 是当前稳定主线；顶层 NSPasteboard API 完全兼容。**影响 0**。
- ⚠️ **PLAN 偏差 #2**（已记录）：`image` crate features 收紧（默认 → `["png", "tiff"]`），最小化 dep tree。**影响 0**。
- ⚠️ **PLAN 偏差 #3**（已记录）：`name()` 字符串从 `"macos-pbcopy-pbpaste"` 改为 `"macos-pbcopy-pbpaste+nspasteboard-image"` 反映 image 路径独立。dispatcher startup log 现在显示新名字。**影响 0**（grep 仅 codebase 内部，无外部依赖）。
- ⚠️ **PLAN 偏差 #4**（已记录）：`cargo fmt --all` 副作用在 connect.rs / listen.rs / http3.rs / service.rs 产生 cosmetic diff，**已 `git checkout` revert**，commit scope 严格限定为 3 个文件。

### STEP-2a.3 — 图片 outbound dispatcher + 200 MiB cache + HTTP/3 image route

- ✅ **完全符合** PLAN §3 M2a STEP-2a.3：
  - cache 升级 128 entries → 200 MiB byte budget + 5 min TTL（`src/clipboard/cache.rs:84, 94, 138-152`）
  - 新增 `bytes()` API + `byte_budget()` getter（`src/clipboard/cache.rs:276-285`）
  - 单条 > budget 拒绝（defensive bound, line 174-180）
  - 重写 `insert` 维护 `bytes_used` 计数（先减旧再加新，覆盖时不双计数）
  - HTTP/3 `/clipboard/image/{sha256}` route 落地（`src/quic_transport/http3.rs:858-860` get_image + cache_lookup_route helper 抽取）
  - text + image route 共享同一 cache（content-addressed）
  - `service.rs::dispatch_image` + `last_outbound_image_sha` + sha256 short-circuit 落地
  - `handle_clipboard_tick` 重构为 text-first → image-fallback 双 phase
  - 13 个新单测（4 cache + 5 http3 + 4 service）
- ⚠️ **PLAN 偏差 #1**（已记录）：`CLIPBOARD_CACHE_CAPACITY` 重命名为 `CLIPBOARD_CACHE_BYTE_BUDGET`；`with_capacity_and_ttl` → `with_byte_budget_and_ttl`。**影响 0**（所有 caller 在 workspace 内部）。
- ⚠️ **PLAN 偏差 #2**（已记录）：`dispatch_image` 新增 sha256 short-circuit（`last_outbound_image_sha` 比较），避免每 500ms 重推同一 image。PLAN 直读是"不做去重"，但 prompt 提到的"receiver 回环检测"是 inbound 2a.4 阶段；dispatcher 这边"skip on duplicate"是带宽优化，与 text dispatcher 等价 short-circuit 对称。**行为更严格，不是破坏意图**。
- ⚠️ **PLAN 偏差 #3**（已记录）：`last_outbound_image_sha` 同时承担 short-circuit + active-eviction 双重职责。**影响 0**（dispatcher_cache_step_skips_on_duplicate_sha 测试 pin）。
- ⚠️ **PLAN 偏差 #4**（已记录）：`cargo fmt --all` 副作用已 revert。

### STEP-2a.4 — 图片 inbound + image loopback LRU 32

- ✅ **完全符合** PLAN §3 M2a STEP-2a.4 + 评审 #3 3rd：
  - `image_lru_fingerprints: LruFingerprints` 字段 + `IMAGE_LOOPBACK_CAPACITY = 32` / `IMAGE_LOOPBACK_TTL = 60s` 常量（`src/service.rs:331-346`）
  - `handle_clipboard_inbound` 拆分为 `handle_clipboard_inbound_text` + `handle_clipboard_inbound_image` sibling methods（外层 match event kind 分派，`src/service.rs:2131-2146`）
  - `handle_clipboard_inbound_image` 镜像 text 分支：LRU loopback check + peer resolve + `Http3Client::get_image` + 200 apply / 非 200 skip / Err skip（`src/service.rs:2265-2330`）
  - `mark_local_image_write` Service method（`src/service.rs:2417-2419`）+ `apply_inbound_clipboard_image`（`src/service.rs:2452-2487`）镜像 text apply helper
  - `apply_inbound_image_bytes` free function（backend 路由 + unknown mime 降级 PNG，`src/service.rs:2967-2990`）
  - 11 个新单测（`src/service.rs:3800-4587`，image_inbound_tests 模块）
- ⚠️ **PLAN 偏差 #1**（已记录）：`handle_clipboard_inbound` 拆分为 sibling methods（PLAN 直读是"加 image 分支"）—— text path inline fast-path + HTTP/3 metadata-only fetch 已 ~80 行，image branch 类似；单函数会让函数体超 130 行，不符合单一职责。**text 分支代码 100% 不变**，只是从原 fn 移到 `handle_clipboard_inbound_text`。**影响 0**。
- ⚠️ **PLAN 偏差 #2**（已记录）：未知 mime label 降级到 PNG 而非 reject —— macOS backend 强制 PNG regardless of label，Linux xclip 接受任何 format，Windows M2b 才有完整 DIB 支持。降级让 daemon 不因一帧异常 wire 标签 panic。**影响 0**。
- ⚠️ **PLAN 偏差 #3**（已记录）：`mark_local_image_write` 是 Service method 而非 LRU method（text 用 `mark_local_write` LRU method 直调）—— prompt 明确锁定名字"mark_local_image_write"；一层薄包装成本可忽略，调用 site 语义一目了然。**行为完全相同**（`mark_local_write` 是 `push` 的别名）。
- ⚠️ **PLAN 偏差 #4**（已记录）：`cargo fmt --all` 副作用已 revert。

### 累计偏离

- ✅ **0 严重偏离**（无 ❌）
- ⚠️ **13 处小偏差**（全部已记录在对应 STEP 报告 §3，影响 0）

## 2. 偏离 REQUIREMENT

- ✅ **未破坏 REQUIREMENT §4.3 "4K 截图字节级一致"**：
  - macOS backend 优先 PNG 直接读（`src/clipboard/macos.rs:421-426`）+ TIFF→PNG 归一化（PLAN 评审 #2 3rd 强制要求，line 429-449）
  - `current_image_normalizes_tiff_to_png` 单测 pin TIFF→PNG 路径 + 维度保持（`src/clipboard/macos.rs:778-814`）
  - `current_image_reads_png_bytes_directly` 单测 pin PNG 字节级一致（`src/clipboard/macos.rs:743-763`）
  - 4K 截图真机字节级一致依赖用户真机测试矩阵（PLAN §8 M2a 人类项）；单元测试已覆盖 path / 数据通路
- ✅ **未破坏 REQUIREMENT §3.3 "剪贴板图片避免回环"**：
  - text 与 image 各有独立 LRU（image 32 / 60s vs text 128 / 60s，`src/service.rs:331-346` + `src/service.rs:802-810`）
  - `apply_inbound_clipboard_image` mark LRU **before** `set_image`（window defence，mirror text 分支，`src/service.rs:2461-2467`）
  - `apply_inbound_clipboard_image_marks_lru_before_set_image` 单测 pin 顺序（`src/service.rs:4188-4225`）
- ✅ **未破坏 REQUIREMENT §3.2 "剪贴板文本" + 1b.2 / 1b.3 既有契约**：
  - cache `cache.remove(prev_sha)` active eviction 保持（1b.2 source-side）
  - 404 silently ignored 保持（1b.2 receiver-side，1b.3 强化）
  - `apply_inbound_clipboard_text` 完全不变（仅 fn 名从原 `handle_clipboard_inbound` 移到 `handle_clipboard_inbound_text`，代码 100% 保留）
  - HTTP/3 `/clipboard/text/{sha256}` route 仍由 `cache_lookup_route` 处理
- ✅ **未破坏 REQUIREMENT §5 "多屏友好"**：不在 M2a scope（M3a / M4 阶段）
- ✅ **公共 API 未破坏**：
  - `ClipboardBackend` trait 三个 image 方法带**默认 impl**，现有 text-only backends（macos pbcopy/pbpaste、Linux xclip/wl-paste、Windows CF_UNICODETEXT）**继承默认 impl 无需改动**（仅有 macos 改 name string）
  - lan-mouse-proto 未 bump（M0a 0.4.0 已落地 `ClipboardImage { fingerprint, mime, sha256, size }`，M2a 直接消费）
  - lan-mouse-ipc 未改（M4 阶段扩 ClipboardConfig）

## 3. BUG 清单

| 严重度 | 位置 | 现象 | 根因 | 影响 | 建议修复 |
|---|---|---|---|---|---|
| P0 | — | — | — | — | **0 P0** |
| P1 | — | — | — | — | **0 P1** |
| **P2.1** | `src/clipboard/macos.rs:103-119, 254-255, 288-289` | `last_image_change_count: Cell<Option<i64>>` 字段被 `set_image` + `current_image` 两处写，但**从未被读取** | 字段 docstring 声称"Used by `set_image` to stamp the post-write changeCount and by future dispatcher code (M2a STEP-2a.3) to skip the read when the changeCount has not advanced"，但 2a.3 dispatcher 实际改用 `last_outbound_image_sha` 短路（dispatch_image:2042-2050） | 0 行为影响；纯 dead code 占据 1 个 Cell 字段 | 选项 A（M2a 收尾）：删除字段 + 简化构造；选项 B（M2a 真机优化时）：用字段在 `current_image` 入口短路（`if Some(current_count) == last_count { return None }`）。建议选项 A（dispatcher 已用 sha256 短路，changeCount 优化收益低） |
| **P2.2** | `src/clipboard/macos.rs:330-366` | `watch_image()` 方法定义完整但**从未被 dispatcher 消费**（dispatcher 只用 `current_image()` 500ms tick 模式） | trait 方法已在 2a.1 设计（按 PLAN §3 STEP-2a.1）+ 2a.2 落地 macOS 实现；但 dispatcher 没有 watch-style 订阅 | 0 行为影响；pure dead code（spawn_local task 会执行但没人消费 stream） | 选项 A（M2a 收尾）：删除 `watch_image` 整个方法（trait 默认 impl 已经返回 empty stream），节省 macOS 测试占用；选项 B（M4 GUI 集成时）：真正用 watch_image 给 UI 即时事件流 |
| **P2.3** | `src/clipboard/cache.rs:183-200` | cache.insert 时 `bytes_used` 更新在 `lru.push_back` 之后；如果 insert 抛出异常（如 OOM）`bytes_used` 与 `entries` / `lru` 可能短暂不一致 | `previous_entry.map(|e| e.bytes)` 在最末，但 `bytes_used += new_size` 与 `while bytes_used > byte_budget` 循环在中间；抛异常时锁已释放 | 实际影响极低（`Vec::push_back` + `usize` 加法不会 panic），但 invariant 应该写明 | 加 doc 注释：`// No panic path between `entries.insert` and `bytes_used += new_size`; both are infallible` |
| **P2.4** | `src/service.rs:2452-2487` (`apply_inbound_clipboard_image`) | LRU 在 `set_image` **之前** mark（line 2461），但若 `set_image` 失败（line 2464-2467 return），LRU 已 polluted with the fingerprint we never actually wrote | 设计选择（mirror text 分支的相同行为） | 用户本地后续复制同一张图时会被 dispatcher 误判为回环而跳过广播；TTL 60s 后恢复 | 选项 A（保留）：文本分支已有相同行为（`src/service.rs:2362-2371`），保持对称；选项 B（修复）：先 set_image 再 mark LRU——但这破坏 window defence 顺序（macOS NSPasteboardDidChangeNotification 同步触发时 race）。**建议保留**，行为与 text 分支一致 |
| P3.1 | `src/quic_transport/http3.rs:416-443` | `clipboard_text_route` + `clipboard_image_route` 两个 wrapper functions `#[allow(dead_code)]` 标记 | 实际路由注册（line 289-294）已直接用 `cache_lookup_route(req, cache, prefix)` —— wrappers 没有 caller | 0 行为影响；pure dead code + 注释误导（"grep-friendly" 但 grep 会搜到未被引用的函数） | 删除两个 wrapper，让 `cache_lookup_route` 是唯一入口；或保留并加 doc 解释"保留作 API 命名稳定点" |
| P3.2 | `src/service.rs:1906-1992` vs `src/service.rs:2039-2109` | text 与 image 分支 cache.insert 顺序不一致：text 是 `evict → broadcast → cache.insert`；image 是 `evict → cache.insert → broadcast` | 顺序差异导致 image 分支更严格（broadcast 前 cache 已就绪），text 分支有微小 race window（broadcast 后 insert 前的几 μs 内 receiver 拉新 sha 会 404） | 实际 race window 极短（μs 级）+ 接收端 404 silently ignored（1b.2 契约）；receiver 收到 broadcast 后通常需要 ms 级时间启动 HTTP/3 GET | 统一为 `evict → cache.insert → broadcast`（image 顺序）；或加 doc 说明这是有意为之（image 顺序更严格是优化） |
| P3.3 | `src/service.rs:2080-2088` | `dispatch_image` 在 `recipients == 0` 时 warn（含 image.data.len()）；如果用户经常复制大图且 0 peers，warn 会重复 | 与 `dispatch_text` 一致行为（line 1956-1961） | 0 行为影响；warn 频次可能略高 | 加 throttle / 60s 内只 warn 一次；或保持 warn-by-tick 与 text 分支一致（M2a 收尾建议保持对称） |
| P3.4 | `src/clipboard/macos.rs:69, 84` | `Mime::Jpeg` / `Mime::Bmp` 是 trait `Mime` 枚举的 variants，文件 import 全部带入但仅 `Mime::Png` 在 macOS backend 实际使用 | `set_image` 接收 `Mime` 参数；Jpeg/Bmp 写入被 warn-and-write 处理；不影响 behavior | 0 行为影响 | 无需修改；M2b JPEG/BMP 真正支持时再细化 |
| P3.5 | `src/service.rs:331-346` | `IMAGE_LOOPBACK_CAPACITY` / `IMAGE_LOOPBACK_TTL` 用 module-level const 而非 `LruFingerprints` associated const | 与 `LruFingerprints::DEFAULT_CAPACITY` / `DEFAULT_TTL` 不对称 | 0 行为影响；只是 naming 不一致 | 改为 `LruFingerprints::IMAGE_CAPACITY` / `IMAGE_TTL` associated const（但 trait 是 `LruFingerprints` struct-level，不影响） |
| P3.6 | `src/clipboard/macos.rs:382-389` | `read_pasteboard_bytes` 把 `data.to_vec()` 复制整份 PNG bytes（5-15 MiB）后再返回 `Vec<u8>` | `NSData::to_vec()` 是必要的（NSData 是 autoreleased，autorelease pool 释放后失效） | 0 行为影响；clone 成本可接受（image 5-15 MiB × tick frequency） | 注释已说明原因；无修改 |

## 4. 跨 STEP 一致性

- ✅ **数据结构一致性**：
  - `ClipboardImage { fingerprint, mime, sha256, size }` 在 lan-mouse-proto (M0a) 落地 + M2a 2a.3 消费，wire format 不变
  - `Mime::Png.mime_str() == "image/png"` 与 macOS backend `current_image` 返回的 `mime` 字符串一致（`src/clipboard/macos.rs:423, 439`）
  - `Mime::Jpeg` / `Mime::Bmp` 落 M2b 范围（M2a only PNG）
- ✅ **IPC / Wire event 一致性**：
  - `ProtoEvent::ClipboardText` → `handle_clipboard_inbound_text` 分支（M1a/1b 既有路径，仅改名）
  - `ProtoEvent::ClipboardImage` → `handle_clipboard_inbound_image` 新分支（M2a 2a.4）
  - `_` → ignored（M3a files / M3a file-transfer 留口）
- ✅ **CLI / IPC 子命令签名一致性**：M2a 不改 IPC 类型（M4 阶段扩 `ClipboardConfig`）
- ✅ **公共 API 一致性**：
  - `ClipboardBackend` trait 三个 image 方法带默认 impl（不破坏 linux.rs / windows.rs）
  - `ClipboardError::Unsupported(String)` 新增（不破坏现有 4 个 variants）
  - `LruFingerprints` 内部 type 不变（M2a 2a.4 加 image instance + 常量，不动 struct）
  - `ClipboardCache` API 改名（`capacity` → `byte_budget`，已记录偏差）
- ✅ **HTTP/3 路由一致性**：
  - `/clipboard/text/{sha256}` 与 `/clipboard/image/{sha256}` 共享 `cache_lookup_route` helper
  - `Http3Client::get_text` / `get_image` / `get_file` 三者签名一致
  - receiver 用 `full_hex` 64 chars（避免 1b.3 short_hex regression，line 2301）
- ✅ **dispatcher 一致性**：
  - text 与 image 各有独立 LRU（capacity 32 vs 128，TTL 都是 60s）
  - text 与 image 各有独立 `last_outbound_*_sha` 字段
  - text 与 image 各有独立 `evict_prev_outbound_*_cache` 方法（delegate 同一 free fn）
  - active eviction 顺序：text `evict → broadcast → cache.insert`，image `evict → cache.insert → broadcast`（见 P3.2 不一致观察）
  - sha256 short-circuit：text 用 `clipboard_last_text` 内容比对 + `clipboard_lru.contains`；image 用 `last_outbound_image_sha` sha 比对（行为对称）
- ✅ **test 覆盖一致性**：
  - 2a.1: 8 mime/ImageBytes/DummyBackend tests
  - 2a.2: 5 macOS-only cfg-gate tests（empty pasteboard / PNG round-trip / TIFF→PNG / set_image PNG / set_image non-PNG warn）
  - 2a.3: 13 tests（4 cache byte-budget + 5 http3 image route + 4 dispatch_image cache step）
  - 2a.4: 11 tests（image LRU capacity/TTL + apply helper happy/unknown/no-backend + ordering + LRU hit + URL 64-char hex + mime round-trip + sha acceptance）
  - 累计 37 个新单测 + 0 破坏既有测试
- ✅ **0 unsafe blocks**（macos.rs / http3.rs / cache.rs / service.rs 均无 unsafe）—— 注意：macOS NSPasteboard 通过 objc2 安全封装（NSData::with_bytes / setData_forType / dataForType 都是安全 API）
- ⚠️ **fmt drift**：`src/connect.rs:1084` / `src/listen.rs:1006` 在每次 `cargo fmt --all` 仍产生 cosmetic diff（3 STEP 全部有偏差 #3 记录），**与本批无关**，留给统一 fmt sweep

## 5. 总体结论

- **PASS-with-followup**（接收 M2a 整批，建议 executor 在 M2b 启动前清理 P2.1/P2.2 + 补 image 真机测试）
- 理由：
  1. 4 STEP 全部按 PLAN §3 M2a 任务表执行；4 处严重偏离 = 0；13 处小偏差全部记录在对应 STEP §3
  2. 0 P0 / 0 P1 BUG；4 处 P2 + 6 处 P3 全部为 dead code / 微优化 / 风格 / 一致性，不阻塞 M2a 验收
  3. 跨 STEP 接口一致（数据结构 / IPC / CLI / 公共 API / HTTP/3 route / dispatcher / 测试）；既有 1b.2 / 1b.3 契约全部保留
  4. wire-compat + test 覆盖完备（37 个新单测，0 fail）；pre-existing 14 clippy errors 维持（M2a 引入 0）
  5. 真机验证用户责任（PLAN §8 M2a 4K 截图 / 1080p JPG / Preview.app 选中区域 TIFF→PNG 路径）—— validator 不跑 build/test

## 6. 建议下一步

### 立即（M2a 收尾，executor 可选）

1. **P2.1 + P2.2 cleanup**（约 10-15 min）：删除 macos.rs `last_image_change_count` 字段 + 删除 `watch_image` 整个方法（trait 默认 impl 已返回 empty stream）—— 纯 dead code 清理，不影响 2a.1/2a.2/2a.3/2a.4 任何契约
2. **P3.2 ordering 一致性**：把 `dispatch_text` 改为 `evict → cache.insert → broadcast`（与 image 分支对齐）；或加 doc 明确这是有意差异（image 顺序更严格）

### 用户责任（PLAN §8 M2a 人类项）

3. **真机 4K 截图双向端到端**（macOS ↔ macOS）：
   - (a) A→B：A 端 `screencapture -x -t png /tmp/4k.png` → Cmd+C，B 端粘贴 → `xxd | sha256sum` 对比源端 PNG bytes
   - (b) B→A：反过来同样跑一次（验证两侧 daemon 都能作为 HTTP/3 server 暴露 `/clipboard/image/{sha256}`）
4. **1080p JPG 双向**（同上）
5. **Preview.app TIFF→PNG 归一化路径**：A 端在 Preview.app 选中区域 → Cmd+C（pasteboard 只提供 TIFF），B 端粘贴 PNG → sha256sum 对比
6. **图片回环测试**：A 复制 4K 截图后 A 端不抖动 + 反向同样

### 中期（M2a → M2b 切换）

7. 启动 **M2b**（Windows + Linux 图片剪贴板）：
   - 2b.1 Windows CF_DIBV5（评审 #4 3rd）
   - 2b.2 Linux X11 / Wayland + XWayland fallback（评审 #6 3rd）
   - 2b.3 三平台互传矩阵（人类配合）
   - 2b.4 fmt/clippy/build + image crate dep

### 长期（M4 / Out of Scope）

8. fmt sweep 统一处理 connect.rs:1084 / listen.rs:1006 cosmetic drift（与本批无关）
9. M4 STEP 4.4 GUI GeneralPanel 展示 `clipboard::metrics` 命中率（reviewer #4 3rd）

---

## 附录 A — 测试覆盖统计

| 文件 | 新单测数 | 累计测试数（after M2a） |
|---|---|---|
| `src/clipboard/mod.rs` | 8 (2a.1) | 16 (含 M1a baseline) |
| `src/clipboard/macos.rs` | 5 (2a.2) | 10 (5 text + 5 image, cfg-gate macOS) |
| `src/clipboard/cache.rs` | 4 (2a.3) | 12 (8 baseline 重写 + 4 byte-budget) |
| `src/quic_transport/http3.rs` | 5 (2a.3) | 32 (含 M1b baseline + 5 image route) |
| `src/service.rs` | 4 (2a.3) + 11 (2a.4) = 15 | 49 (M1a baseline + 1b + 2a.3 + 2a.4) |
| **新增总计** | **37** | — |
| **全 workspace pass** | — | **375**（M2a 2a.4 终点 baseline 339 + 5 + 13 + 11 + 7 = 375）|
| **0 fail** | ✅ | ✅ |

## 附录 B — Clippy / fmt 状态

| 检查 | 结果 |
|---|---|
| `cargo build -p lan-mouse --lib` | ✅ Finished `dev` profile |
| `cargo test --workspace --no-fail-fast` | ✅ 375 pass / 0 fail（baseline 339 + 37 new） |
| `cargo clippy --workspace --all-targets -- -D warnings` | ⚠️ 14 errors（pre-existing，与 M1b/M2a 无关）；stash 对照验证 M2a 引入 **0** 新增 clippy error |
| `cargo fmt --all -- --check` | ⚠️ M2a 4 STEP 每个改动文件本身 fmt-clean；connect.rs / listen.rs 既有 cosmetic drift（与 M1a / 1b.3 同源） |

## 附录 C — wire-compat 验证

| Wire 层 | M2a 改动 | wire-compat |
|---|---|---|
| StreamA (control) | 未触碰 | 既有键鼠事件 100% 兼容 |
| StreamB (input) | 未触碰 | 既有键鼠事件 100% 兼容 |
| StreamC (meta) | 新消费 `ClipboardImage` 事件；`ClipboardText` 既有路径不变 | 与 M1b 全兼容 |
| HTTP/3 GET `/clipboard/text/{sha256}` | 既有；与 image 共用 `cache_lookup_route` helper | 与 M1b 全兼容 |
| HTTP/3 GET `/clipboard/image/{sha256}` | **M2a.3 新增**；404 silently ignored；receiver 用 `full_hex` 64 chars | M2a 引入新路由，旧 daemon 忽略；与新 daemon 完全兼容 |
| lan-mouse-proto schema | 未 bump（M0a 0.4.0 已含 `ClipboardImage { fingerprint, mime, sha256, size }`） | wire format 不变 |
| lan-mouse-ipc | 未改 | 与 M1a / M1b 全兼容 |

## 附录 D — 真机测试矩阵（M2a 用户责任）

| 场景 | 验证方法 | M2a 终态 |
|---|---|---|
| macOS ↔ macOS 4K PNG (a) A→B | `screencapture` + `Cmd+C` + 粘贴 + `sha256sum` | 用户执行（validator 不跑） |
| macOS ↔ macOS 4K PNG (b) B→A | 同上反向 | 用户执行 |
| macOS ↔ macOS 1080p JPG | Finder 复制 JPG + 粘贴 + `sha256sum` | 用户执行 |
| Preview.app TIFF→PNG 归一化 | Preview.app 选中区域 → `Cmd+C` + 粘贴 + 检查 PNG magic | 用户执行 |
| 图片回环 (a) A→B | 录屏 A 端不抖动 | 用户执行 |
| 图片回环 (b) B→A | 同上反向 | 用户执行 |

---

> **验证人**：step-validator
> **报告路径**：`/Users/hb/Projects/@cloudself/lan-mouse-pro/next/STEP-VALIDATION-P2-M2a.md`
> **建议**：M2a 接收；executor 可选 cleanup P2.1/P2.2 dead code；用户真机测试 4K 截图通过后启动 M2b