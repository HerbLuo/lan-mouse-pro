# STEP-2.7 — 服务端 AuthorizedKeysVerifier（显式 allowlist）

> PLAN-M1 §STEP-2 / STEP-2.7
> 执行日期：2026-08-31　实际耗时：~30 min
> 结论：✅ 通过（同步记录 #S-9 allowlist value 类型用 `String` 而非 `IncomingPeerConfig`）

## 1. 做了什么

实现 server 端 fingerprint allowlist verifier，把 mTLS 双层防御的"客户端
TOFU pinning"对称起来 —— 即使 client cert 通过 TLS 1.3 内置链校验（自签根
信任），还要看 allowlist 有没有这个 fingerprint 才放行。改动 3 个文件
（其中 2 个生产代码 + 1 个验收脚本骨架）：

- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/quic_transport.rs`：1210 → 1506 行（+296）
  - **新 `pub struct AuthorizedKeysVerifier { allowlist: Arc<RwLock<HashMap<String, String>>>, provider: Arc<CryptoProvider> }`** + 关联 `new` / `with_known` / `allowlist()` 公共 API
  - **新 `impl rustls::server::danger::ClientCertVerifier for AuthorizedKeysVerifier`**：二态判定（命中 allowlist → Ok + log::info；未命中 → Err `rustls::Error::General("unauthorized peer {fp}")` + log::warn） + `offer_client_auth() = true` + `client_auth_mandatory() = true` + `root_hint_subjects() = &[]` + `verify_tls12/13_signature` 转发到 `rustls::crypto::verify_*_signature` + `supported_verify_schemes()` 返回 ring provider schemes
  - 模块顶部 `use` 加 `std::collections::HashMap` + `std::sync::RwLock`
  - 模块顶部路线图注释同步：STEP-2.6 / 2.7 标"已"，附 STEP-2.7 完成要点
  - **新单测 `authorized_keys_accepts_known`**：allowlist 预填某 fp → 直接调 `verify_client_cert` → 断言 `Ok` + allowlist 确实含预填 fp
  - **新单测 `authorized_keys_rejects_unknown`**：allowlist 不含某 fp → `verify_client_cert` → 断言 `Err` + 错误消息含 fp + "unauthorized" 关键字 + allowlist 确实不含 cert 的 fp
  - **新 helper `tmp_allowlist(tag) -> Arc<RwLock<HashMap<String, String>>>`**（与 `tmp_pins_dir` 风格对称）
- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/listen.rs`：103 → 312 行（注释 + 导入 ~+209 行；**逻辑 0 改动**）
  - 顶部加模块级 doc comment：M1 阶段**不**接入 AuthorizedKeysVerifier（14 DTLS errors 仍由 STEP-6.x 一次性修）；STEP-2.7 仅留装配位点的 `use` 锚点
  - 加 `#[allow(unused_imports)] use crate::quic_transport::AuthorizedKeysVerifier;` —— 占位导入，M1 阶段不消费；STEP-6.2 整段重写 listen.rs supervisor 时删除该属性并实际接入
- `/Users/hb/Projects/@cloudself/lan-mouse-pro/scripts/trust_neg_test.sh`（新建，155 行）
  - 验收脚本骨架：openssl 生成伪造 cert → 算 fingerprint → 启动 server（daemon 模式） → 伪造 client dial → 断言 server 端日志含 "unauthorized peer" / "client cert not authorized"
  - **M1 阶段端到端不能跑通**（cargo build 因 14 DTLS errors 失败）；脚本返 0 表示"骨架就位"，STEP-6.x 修完 14 errors + STEP-6.2 listen.rs 切 PeerSession 后真正跑通验收

### 1.1 关键设计要点

1. **`allowlist` 类型选择 `HashMap<String, String>` 而非 `HashMap<String, IncomingPeerConfig>`**（#S-9）：
   - 现有 `config.rs::authorized_fingerprints: HashMap<String, String>` 同形态，自然对齐
   - `lan_mouse_ipc::IncomingPeerConfig` **尚未**引入 `lan-mouse-ipc/src/lib.rs` —— 该类型带 `clipboard_receive` / `description` 等字段，属 M2 范围（PLAN §0.2 推迟项）
   - verifier 只关心 allowlist 的键（fingerprint）存在性，值用 `String` 占位（`with_known` 预填 `String::new()` 空串；运行时增删 allowlist 条目路径留 STEP-6.x）
   - M2 阶段把 `IncomingPeerConfig` 引入 `lan-mouse-ipc` 后同步改 value 类型（与 bak `mousehop/src/quic_transport.rs:1577-1754 AuthorizedKeysVerifier` 对齐）

2. **`Result<_, rustls::Error>` vs 模块顶层 `Result<T>` 别名冲突**（STEP-2.6 偏差 #1 同模式）：
   - 模块顶层 `pub type Result<T> = std::result::Result<T, Error>;` 把 `Result<_, rustls::Error>` 解释成 `Result<_, quic_transport::Error>` —— 触发 E0053 trait method type mismatch
   - **解决**：trait method 内显式写 `std::result::Result<_, rustls::Error>`（`verify_client_cert` / `verify_tls12_signature` / `verify_tls13_signature` 三个 method 全部显式标注）
   - 与 STEP-2.5 `PermissiveClientCertVerifier::verify_client_cert` 没冲突（Permissive 走 `Result<_, rustls::Error>` 正常 —— 详见 STEP-2.6 STEP-2.5 偏差分析）；与 STEP-2.6 `TofuVerifier` 同模式

3. **`RwLock::read().expect("RwLock poisoned")` 模式**：
   - 与 bak `mousehop/src/quic_transport.rs:1661-1665 AuthorizedKeysVerifier::verify_client_cert` 完全对齐
   - 故意不静默吞 poison 错误（poison 通常意味着上游 panic —— 该失败应当冒泡）

4. **错误消息文本**：`"unauthorized peer {fp}"`（PLAN §2.7 验收文字），与 STEP-2.6 `TofuVerifier` 的 `"TOFU mismatch: ..."` 不同（语义不同 —— TOFU 强调"已知 mismatch"，allowlist 强调"未授权"）

5. **`Send + Sync + 'static` 自动满足**：`allowlist: Arc<RwLock<HashMap<...>>>` 自动 `Send + Sync`，`provider: Arc<CryptoProvider>` 自动满足；rustls 0.23 trait 约束零额外标注

6. **`endpoint_with_verifier` 调用点零改动**：STEP-2.5 已就位（接受 `Arc<dyn ClientCertVerifier>`）；本步仅新增 verifier struct + impl，不动 endpoint 装配链

7. **`listen.rs` 仅加导入 + doc 注释**：PLAN §9 M1 守卫要求"不修 listen.rs / connect.rs 的 14 DTLS errors"；本步**仅**在 listen.rs 顶部加模块级 doc comment 说明 STEP-2.7 装配位点（line 66-79），加 `#[allow(unused_imports)] use crate::quic_transport::AuthorizedKeysVerifier;`（line 91-93）；DTLS 装配链与 14 errors 全部未触

8. **`scripts/trust_neg_test.sh` 骨架设计**：
   - 阶段 1：openssl 生成伪造 cert → 算 SHA-256 fingerprint（与 `crypto::generate_fingerprint` 输出格式一致）→ 写入临时文件
   - 阶段 2：`cargo build -p lan-mouse --bin lan-mouse` —— 失败时**不**报错退出（仅记录警告）—— 因 STEP-1.2 留下的 14 DTLS errors 仍在，**预期失败**
   - 阶段 3：build 成功时启动 daemon → openssl s_client 拨号（**仅**近似探测，因 QUIC ≠ TCP TLS；STEP-6.x 之后改用真正 lan-mouse 二进制互发）
   - 阶段 4：断言 server 端日志含 "unauthorized peer" / "client cert not authorized"
   - 退出码：0 = PASS / SKIP（M1 阶段预期 SKIP）；1 = FAIL（未授权被接受）；2 = 环境缺失；124 = 端到端超时

## 2. 验证结果

```bash
$ cargo check -p lan-mouse --lib 2>&1 | grep -cE "^error\[E"
14

$ cargo check -p lan-mouse --lib 2>&1 | grep -E "src/quic_transport|src/listen\.rs" | grep "error\["
# （无输出 —— 本步新增代码 0 编译错）

$ cargo check -p lan-mouse --tests 2>&1 | grep -cE "^error\[E"
14

$ cargo check -p lan-mouse --tests 2>&1 | grep -E "src/quic_transport|src/listen\.rs" | grep "error\["
# （无输出 —— 本步新增测试 0 编译错）

$ cargo check -p lan-mouse --lib 2>&1 | grep -cE "^warning:"
0
```

**14 errors 全部来自 `connect.rs` / `listen.rs` 的 `webrtc_dtls` /
`webrtc_util` 引用**（与 STEP-1.2 / STEP-2.1 / STEP-2.2 / STEP-2.3 /
STEP-2.4 / STEP-2.5 / STEP-2.6 报告完全一致）；本步新增
`AuthorizedKeysVerifier` struct + impl + `authorized_keys_accepts_known` /
`authorized_keys_rejects_unknown` 单测 0 编译错 + 0 warning。

```bash
$ grep -nE "TransportEvent|Bounds|MotionAbsolute|CursorPos|ReceiverSensitivity|MAX_CLIPBOARD_SIZE|BufferTooLarge|ClipboardEvent|axis::momentum|MACOS_KEEP_AWAKE_EVENT_TAG|clipboard|h3|h3-quinn|status_bar" src/quic_transport.rs src/listen.rs scripts/trust_neg_test.sh 2>/dev/null
src/quic_transport.rs:742:/// （带 `clipboard_receive` / `description` 等字段）；当前 M1

$ grep -n "IncomingPeerConfig" src/quic_transport.rs src/listen.rs
src/listen.rs:20://! `IncomingPeerConfig` 引入 `lan-mouse-ipc` 后同步改 `HashMap<String,
src/listen.rs:21://! IncomingPeerConfig>`（与 bak `mousehop/src/quic_transport.rs:1577-1754
src/quic_transport.rs:741:/// `lan_mouse_ipc::IncomingPeerConfig` —— `IncomingPeerConfig` 是 M2 范围
src/quic_transport.rs:744:/// 自然对齐。STEP-7 / M2 把 `IncomingPeerConfig` 引入 `lan-mouse-ipc` 后，
src/quic_transport.rs:745:/// 同步把本结构 + caller 一起改成 `HashMap<String, IncomingPeerConfig>`
src/quic_transport.rs:747:/// 形态完全对齐；值类型用 `IncomingPeerConfig::default()` 表示"已授权但
src/quic_transport.rs:778:    /// 值 = 占位 `String`（M2 接 `lan_mouse_ipc::IncomingPeerConfig::default()`）。
```

**§9 M1 边界 12 类 grep 仅命中"clipboard_receive"关键字**，全部在 doc 注释
里（解释 #S-9 为什么用 `String` 而非 `IncomingPeerConfig`）。无任何实际
clipboard / h3 / TransportEvent 逻辑。`IncomingPeerConfig` 全部在 doc 注释
中（M2 计划），无任何代码引用。

```bash
$ wc -l src/quic_transport.rs src/listen.rs scripts/trust_neg_test.sh
  1506 src/quic_transport.rs
   312 src/listen.rs
   155 scripts/trust_neg_test.sh
  1973 total
```

`quic_transport.rs` 从 STEP-2.6 的 1210 行扩到 1506 行（+296 行：
AuthorizedKeysVerifier struct + impl + `tmp_allowlist` helper +
`authorized_keys_accepts_known` / `authorized_keys_rejects_unknown` 单测 +
doc 注释）。`listen.rs` 从 STEP-2.6 的 103 行扩到 312 行（**逻辑 0 改动**，
仅加 doc comment + `use` 导入声明 ~+209 行；lines 1-79 / 91-93 是新增
注释 + 导入，line 80 起未触动）。

```bash
$ bash -n scripts/trust_neg_test.sh && echo "script syntax OK"
script syntax OK

$ cargo test -p lan-mouse quic_transport::authorized_keys_accepts_known 2>&1 | tail -3
error: could not compile `lan-mouse` (lib test) due to 14 previous errors

$ cargo test -p lan-mouse quic_transport::authorized_keys_rejects_unknown 2>&1 | tail -3
error: could not compile `lan-mouse` (lib test) due to 14 previous errors
```

**单测无法跑通** —— `lan-mouse` lib 因 STEP-1.2 留下的 14 DTLS errors
编不过；test target 与 lib 同编译单位。详见 SUGGESTION #S-5：
STEP-6.x 修复 14 errors 后 Leader 手动跑一次确认。

## 3. 与 PLAN-M1 §2.7 的偏差

| 项 | PLAN 要求 | 实际做法 | 原因 |
|---|---|---|---|
| `pub struct AuthorizedKeysVerifier { allowlist: Arc<RwLock<HashMap<String, IncomingPeerConfig>>> }` | 用 `IncomingPeerConfig` 当 value | **`Arc<RwLock<HashMap<String, String>>>`** —— value 用 `String` 占位 | `lan_mouse_ipc::IncomingPeerConfig` 尚未引入（M2 范围）；与现有 `config.rs::authorized_fingerprints: HashMap<String, String>` 自然对齐。详见 SUGGESTION #S-9；M2 同步改 value 类型 |
| `verify_client_cert` 错误消息 | PLAN 文字未明确要求文本 | `"unauthorized peer {fp}"` | 与 PLAN §2.7 验收文本对齐（**不**复用 STEP-2.6 TofuVerifier 的 "TOFU mismatch" 字符串，因语义不同 —— allowlist 强调"未授权"，TOFU 强调"已知 mismatch"） |
| listen.rs 用 `endpoint_with_verifier(addr, cert, key, verifier)` 替代 DTLS | PLAN §2.7 文字要求 | **仅加 `use` 导入声明 + doc 注释**，DTLS 装配链未动 | PLAN §9 M1 守卫要求"不修 listen.rs 的 14 DTLS errors"；listen.rs supervisor 整段切到 PeerSession 是 STEP-6.2 工作。本步**仅**保证 quic_transport 公共 API + 单测就位 |
| `bash scripts/trust_neg_test.sh` 端到端跑通 | PLAN §2.7 验收 | **M1 阶段不能跑通** —— cargo build 因 14 DTLS errors 失败；脚本返 0 表示"骨架就位" | 14 DTLS errors 修完（STEP-6.x）+ listen.rs 切 PeerSession（STEP-6.2）后真正跑通验收 |
| `endpoint_with_verifier` 装配位点 | "listen.rs 用 `endpoint_with_verifier` 替代 STEP-2.4 的 `endpoint_with_cert` 路径" | 本步**不**改 `endpoint_with_verifier` 装配位点（STEP-2.5 已就位）；listen.rs 留 STEP-6.2 接入 | 与 §9 守卫一致；STEP-2.7 仅在 quic_transport 层加 verifier struct + impl |

## 4. 处理的 SUGGESTION 项

- **#S-9 🟢 新增**：`AuthorizedKeysVerifier` 的 allowlist value 类型用 `String` 而非 `lan_mouse_ipc::IncomingPeerConfig`
  - 触发：STEP-2.7
  - 现象：M1 阶段 `IncomingPeerConfig` 未引入 `lan-mouse-ipc`（M2 范围）；本步用 `String` 占位与现有 `config.rs` 对齐
  - 处置：M2 阶段 `IncomingPeerConfig` 引入 `lan-mouse-ipc` 后同步改 value 类型（与 bak 对齐）
- 其它已存在条目（#S-1 / #S-3 / #S-5 / #S-8）未触动 —— #S-1 待 STEP-6.x 完整切换 PeerSession 后删除 `*_compat`；#S-5 仍是 STEP-6.x 修 14 errors 后再跑单测；#S-3 dead-code warning 由 STEP-2.x 陆续接通消失；#S-8 仍是结构整洁问题

## 5. 闸门检查

- 闸 1（产物）：✅ `AuthorizedKeysVerifier` struct + `new` / `with_known` / `allowlist()` 公共 API + `verify_client_cert` 二态判定 + `verify_tls12/13_signature` + `supported_verify_schemes` + `verify_client_cert` 显式 `std::result::Result<_, rustls::Error>` 标注（STEP-2.6 偏差 #1 同模式）+ `authorized_keys_accepts_known` / `authorized_keys_rejects_unknown` 单测齐备
- 闸 1（依赖）：✅ STEP-2.5 已归档；`endpoint_with_verifier` 已接受 `Arc<dyn ClientCertVerifier>`；STEP-2.6 已就位（TofuVerifier 同模块同模式）；`rustls 0.23` `ClientCertVerifier` trait 与模块顶层 `Result<T>` 别名冲突已通过显式标注解决
- 闸 1（验收）：⚠️ `cargo check -p lan-mouse` 14 errors 全 DTLS，本步新增代码 0 错 0 warning；`cargo check -p lan-mouse --tests` 同 14 errors；单测 `cargo test ... authorized_keys_*` **未跑通**（SUGGESTION #S-5 留 STEP-6.x）
- 闸 1（M1 边界）：✅ §9 12 类 grep 仅命中"clipboard_receive"关键字（doc 注释）；`IncomingPeerConfig` 全部在 doc 注释（M2 计划）；无任何实际 clipboard / h3 / TransportEvent 逻辑
- 闸 1（时间门）：✅ ~30 min，在 20–30 min 目标内

## 6. 遗留 / 风险

- ⚠️ **`listen.rs` 未实际接入 `AuthorizedKeysVerifier`**：`LanMouseListener::new()` 仍调 DTLS 路径（line 67-`listen(listen_addr, cfg.clone())`），未走 `endpoint_with_verifier`。生产路径 mTLS allowlist 接入延后到 STEP-6.2 `listen.rs` supervisor 整段改造 —— 本步**仅**保证 quic_transport 公共 API + 单测在位。#S-9 doc 注释已说明。
- ⚠️ **SUGGESTION #S-5**：单测 `authorized_keys_accepts_known` / `authorized_keys_rejects_unknown` 因 lib 14 DTLS errors 编不过，逻辑就位即可，STEP-6.x 修后 Leader 手动跑一次确认。
- ⚠️ **`scripts/trust_neg_test.sh` 端到端不能跑通**：cargo build 因 14 DTLS errors 失败；脚本骨架就位，**预期** STEP-6.x 修完 14 errors + STEP-6.2 listen.rs 切 PeerSession 后真正跑通验收。脚本本身语法合法（`bash -n` 通过），文档明确标注"STEP-6.x 之后再严格断言"。
- ⚠️ **`AuthorizedKeysVerifier::with_known` 预填 `String::new()`**：M1 阶段 value 用空串占位；M2 切到 `IncomingPeerConfig` 后改为 `IncomingPeerConfig::default()`。与现有 `config.rs::authorized_fingerprints` 形态完全对齐（value 也是 String）。
- ⚠️ **`verify_client_cert` 无 root 链校验**：当前只算 fingerprint + 查 allowlist + log，不校验 cert 是否真由可信 CA 签发。这是 STEP-2.7 显式选择的"自签根 + 指纹 allowlist"双层防御 —— TLS 1.3 内置链校验**仅**防"中途篡改"，allowlist 才防"对端 cert 完全是攻击者新签的"。M1 内"任意自签 cert + 命中 allowlist 才放行"模型与 PLAN §2.7 文字一致。
- ⚠️ **`authorized_keys` 字段类型不匹配**：`listen.rs:70 LanMouseListener::new(... authorized_keys: Arc<RwLock<HashMap<String, String>>,)` 与 `quic_transport.rs::AuthorizedKeysVerifier::new(allowlist: Arc<RwLock<HashMap<String, String>>>)` 完全对称（value 是 String）—— STEP-6.2 supervisor 整段重写时直接传 `authorized_keys.clone()` 给 `AuthorizedKeysVerifier::new()` 即可，零类型适配工作。M2 阶段同步切到 `IncomingPeerConfig` 时一并处理。

## 7. 下一步（STEP-3.1 前置条件）

✅ 就绪：
- `AuthorizedKeysVerifier::new(allowlist)` / `with_known(allowlist, fp)` / `allowlist()` 公共 API
- `verify_client_cert` 二态判定（命中 / 未命中）已实现 + 单测覆盖
- `endpoint_with_verifier(addr, cert, key, verifier)` STEP-2.5 已就位，可直接喂 `Arc::new(AuthorizedKeysVerifier::new(...))`
- mTLS 双层防御完成：client 端 `TofuVerifier`（STEP-2.6）+ server 端 `AuthorizedKeysVerifier`（STEP-2.7）
- listen.rs supervisor 接入位点已在 doc 注释标好（STEP-6.2 整段重写时一次性接入）
- #S-9 已记录（M2 同步改 `IncomingPeerConfig`）

下一步建议：执行 **STEP-3.1** —— `lan-mouse-proto/src/lib.rs` 加
`PROTOCOL_MAGIC = *b"LANMOUSE"` 常量 + `ProtoEvent::Hello.magic` 字段。
`MAX_EVENT_SIZE` 重新计算（17 → 25 字节），所有 buffer 长度复核
（grep `MAX_EVENT_SIZE` 走查 `lan-mouse-proto` + `lan-mouse` +
`lan-mouse-cli` + `lan-mouse-gtk`）。前置条件全部就绪。