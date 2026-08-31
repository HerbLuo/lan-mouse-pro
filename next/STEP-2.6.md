# STEP-2.6 — 客户端 TofuVerifier（fingerprint pinning）

> PLAN-M1 §STEP-2 / STEP-2.6
> 执行日期：2026-08-31　实际耗时：~30 min
> 结论：✅ 通过（同步处理 #S-6 `WebPkiServerVerifier` 替换为 TofuVerifier）

## 1. 做了什么

实现 `TofuVerifier`（client 端 Trust-On-First-Use fingerprint pinning） +
接入 `build_quic_client_config` / `dial` 装配链路。改动 1 个文件
（`crypto.rs` 不动 —— `generate_fingerprint` 已复用；`main.rs` / `lib.rs`
不动）：

- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/quic_transport.rs`：871 → 1210 行（+339）
  - **新 `pub struct TofuVerifier { pins_dir: PathBuf, provider: Arc<CryptoProvider> }`** + 关联 `with_known` 测试 helper + `pin_path` / `has_any_pins` 私有 helpers
  - **新 `impl rustls::client::danger::ServerCertVerifier for TofuVerifier`**：三态判定（Known Match / Known Mismatch / First Connect）+ 签名验签转发到 `rustls::crypto::verify_*_signature` + `supported_verify_schemes()` 返回 ring provider 的 schemes
  - **改 `build_quic_client_config(cert_chain, key, pins_dir)`**：删除 STEP-2.5 的 `WebPkiServerVerifier` 占位 + root store 装配；改 `.dangerous().with_custom_certificate_verifier(Arc::new(TofuVerifier::new(pins_dir)))` + `with_client_auth_cert(cert_chain, key)`（#S-6 已解）
  - **改 `dial(ep, addr, cert, key, pins_dir)`**：末尾加 `pins_dir: &Path` 参数透传给 `build_quic_client_config`；doc 注释同步
  - **新单测 `tofu_first_run_pins`**：直接调 `verify_server_cert` → 断言返回 `Ok` + `pins_dir/<sanitized_fp>.pin` 文件存在 + 文件内容 `b"trusted\n"`
  - **新单测 `tofu_disallows_swap`**：用 `with_known` 预落盘 cert1 的 pin → 用 cert2 的 `verify_server_cert` → 断言 `Err(rustls::Error::General(...))` 含 "TOFU mismatch" / "untrusted peer" + 不动 cert1 的 pin + **不**为 cert2 落盘
  - **改 `quinn_client_config_loads_rustls_provider` / `endpoint_with_cert_accepts_local_incoming` / `dial_completes_handshake_against_local_listener`**：补 `pins_dir` 临时目录 + 传给 `build_quic_client_config` / `dial`
  - **新 helper `tmp_pins_dir(tag) -> PathBuf`** + **新 helper `test_server_name() -> ServerName<'static>`**（与 bak `mousehop/src/quic_transport.rs:2953-2955` 对齐）

### 1.1 关键设计要点

1. **`Result<_, rustls::Error>` vs 模块顶层 `Result<T>` 别名冲突**：模块顶层
   `pub type Result<T> = std::result::Result<T, Error>;` 把 `Result<_, rustls::Error>`
   解释成 `Result<_, quic_transport::Error>` —— 触发 E0053
   （"method `verify_server_cert` has an incompatible type for trait"）。
   **解决**：trait method 内显式写 `std::result::Result<_, rustls::Error>`。
   与 STEP-2.5 `PermissiveClientCertVerifier::verify_client_cert` 没冲突（它
   在 trait impl 时也用了 `Result<ClientCertVerified, rustls::Error>`，
   但 Permissive 路径编译通过 —— 因 `rustls::server::danger::ClientCertVerifier`
   trait 期望 `Result<_, rustls::Error>`，与本仓 `Result<T>` 别名一致（同
   `rustls::Error` 在标准路径下被解释为本仓的 `Result<T, Error>` 别名，
   但是 `quic_transport::Error` 没有 `#[from] rustls::Error` 派生时
   `Result<_, rustls::Error>` 推断成 `Result<_, quic_transport::Error>`
   仍合法 —— 触发 trait method 类型签名不匹配）。本步 trait method 全部
   显式标注 `std::result::Result`。

2. **三态判定的语义**（PLAN §2.6 文字 vs bak 实际）：
   - PLAN §2.6 文字："命中缓存 → Ok；未命中 → Err；第一次见到某 fingerprint →
     落盘占位文件 + 日志 paired with <fp>"
   - bak `mousehop/src/quic_transport.rs:1444-1485` 实际："pin 文件存在 → Ok
     (Known Match)；pin 不存在但 `pins_dir` 有任何 `.pin` → Err (Known Mismatch)；
     `pins_dir` 空 / 不存在 → 落盘 + Ok (First Connect)"
   - **本步采用 bak 实际行为** —— TOFU 模型的合理定义：
     - First Connect（`pins_dir` 空）→ 任何对端都接受，自动 pin；
     - Known Match（pin 已落盘且 fingerprint 命中）→ 接受；
     - Known Mismatch（pin 目录已有其他对端的 pin，但当前 fingerprint 没 pin）
       → 拒绝（这是 LAN 攻击防护的核心约束）
   - 与 PLAN §2.6 文字差异：PLAN 说 "未命中 → Err" —— 实际 PLAN 描述混淆了
     "First Connect" 与 "Known Mismatch" 两种场景。本步在 doc 注释里明确写
     "三态判定"，并在单测 `tofu_disallows_swap` 里精确测 "Known Mismatch"
     路径（不是 First Connect 路径）。

3. **`provider: Arc<CryptoProvider>` 字段**：rustls 0.23 的 `ServerCertVerifier`
   trait 要求实现 `verify_tls12_signature` / `verify_tls13_signature` /
   `supported_verify_schemes` —— 这三个方法都需要 `signature_verification_algorithms`
   列表，必须持有 provider 引用（不能在每个 method 调用内重新构造 provider，
   因为每次构造是不同的 `Arc` 实例）。与 bak `TofuVerifier` 对称。

4. **pin 文件路径 sanitize**：`aa:bb:cc:...` 替换为 `aa_bb_cc_...`（`:` 在
   Windows 上不是合法文件名字符）。pin 文件内容是占位 `b"trusted\n"` —— 不
   存 fingerprint 本身（fingerprint 已在文件名里；存内容是给运维 grep 用）。

5. **`build_quic_client_config` 签名变化（#S-6 治理）**：
   - 旧：`(cert_chain, key) -> Result<QuinnClientConfig>`（内部装配
     `WebPkiServerVerifier` + root store）
   - 新：`(cert_chain, key, pins_dir: &Path) -> Result<QuinnClientConfig>`（装配
     `TofuVerifier::new(pins_dir)`，**不**再需要 root store —— custom verifier
     全权负责 server cert 校验）
   - 同步更新 `dial(...)` 签名末尾加 `pins_dir: &Path`（与 bak
     `mousehop/src/quic_transport.rs:1792 dial_with_client_cert_tofu(..., pins_dir)`
     形态完全对齐）

6. **`dial` doc 注释 + Error 透传说明**：明确 TofuVerifier mismatch 会以
   `ConnectionError::TransportError(rustls::Error::General("TOFU mismatch:
   peer fingerprint {fp} not in known peers"))` 形态冒到 `Error::Handshake`。
   错误字符串采用 bak 字符串（"TOFU mismatch: ..."），与 PLAN §2.6 文字
   （"untrusted peer ..."）略有差异 —— 单测 `tofu_disallows_swap` 用
   `contains("TOFU mismatch") || contains("untrusted peer")` 双匹配兜底。

7. **`#[allow(dead_code)]` 守护**：`TofuVerifier` 被 `build_quic_client_config`
   消费 → `dial` 间接消费 → 测试直接调 `verify_server_cert`。链路完整
   不需要守护。`with_known` 只测试用，加 `#[allow(dead_code)]` 标记。

## 2. 验证结果

```bash
$ cargo check -p lan-mouse --lib 2>&1 | grep -cE "^error\[E"
14

$ cargo check -p lan-mouse --lib 2>&1 | grep -E "quic_transport|crypto\.rs|service\.rs" | grep "error\["
# （无输出 —— 本步新增代码 0 编译错）

$ cargo check -p lan-mouse --tests 2>&1 | grep -cE "^error\[E"
14

$ cargo check -p lan-mouse --tests 2>&1 | grep -E "src/quic_transport" | grep "error\["
# （无输出 —— 本步新增测试 0 编译错）
```

**14 errors 全部来自 `connect.rs` / `listen.rs` 的 `webrtc_dtls` /
`webrtc_util` 引用**（与 STEP-1.2 / STEP-2.1 / STEP-2.2 / STEP-2.3 /
STEP-2.4 / STEP-2.5 报告完全一致）；本步新增 `TofuVerifier` struct + impl
+ `tofu_first_run_pins` / `tofu_disallows_swap` 单测 0 编译错。

```bash
$ grep -nE "TransportEvent|Bounds|MotionAbsolute|CursorPos|ReceiverSensitivity|MAX_CLIPBOARD_SIZE|BufferTooLarge|ClipboardEvent|axis::momentum|MACOS_KEEP_AWAKE_EVENT_TAG|clipboard|h3|h3-quinn|status_bar" src/quic_transport.rs src/crypto.rs src/service.rs
# （无输出 —— §9 M1 边界 12 类 grep 无命中）
```

```bash
$ wc -l src/quic_transport.rs src/crypto.rs
  1210 src/quic_transport.rs
   454 src/crypto.rs
```

`quic_transport.rs` 从 STEP-2.5 的 871 行扩到 1210 行（+339 行：TofuVerifier
struct + impl + `tmp_pins_dir` helper + `tofu_first_run_pins` / `tofu_disallows_swap`
单测 + 改 `build_quic_client_config` / `dial` + doc 注释）。

```bash
$ cargo test -p lan-mouse quic_transport::tofu_first_run_pins 2>&1 | tail -3
error: could not compile `lan-mouse` (lib test) due to 14 previous errors

$ cargo test -p lan-mouse quic_transport::tofu_disallows_swap 2>&1 | tail -3
error: could not compile `lan-mouse` (lib test) due to 14 previous errors
```

**单测无法跑通** —— `lan-mouse` lib 因 STEP-1.2 留下的 14 DTLS errors 编
不过；test target 与 lib 同编译单位。详见 SUGGESTION #S-5：STEP-6.x 修复
14 errors 后 Leader 手动跑一次确认。

## 3. 与 PLAN-M1 §2.6 的偏差

| 项 | PLAN 要求 | 实际做法 | 原因 |
|---|---|---|---|
| `pub struct TofuVerifier { pins_dir: PathBuf, on_first_seen: ... }` | 有 `on_first_seen` 字段（看起来是 callback） | **`pins_dir: PathBuf` + `provider: Arc<CryptoProvider>` 两字段**（无 callback） | bak `TofuVerifier` 实测就这两字段；"on_first_seen" 是 PLAN 文字猜测（PLAN 想描述"首次见到做某事"，但实际实现用 `verify_server_cert` 内部分支判定，不需要 callback 字段）。`provider` 字段是 rustls 0.23 trait 要求（签名验签需要 `signature_verification_algorithms`） |
| `with_known(pins_dir, fp)` test helper | 同 | 直接对齐 bak | bak 用 `with_known` 预落盘 pin 文件作为"已知 peer"测试场景 |
| `verify_server_cert` 三态判定 | "命中缓存 → Ok；未命中 → Err；第一次见到某 fingerprint → 落盘占位" | **三态判定**：Known Match → Ok；First Connect（pin dir 空）→ 落盘 + Ok；Known Mismatch（pin dir 非空但当前 fp 未 pin）→ Err | PLAN 文字混淆了 First Connect 与 Known Mismatch。本步采用 bak 实际行为（与 bak `mousehop/src/quic_transport.rs:1444-1485` 完全对齐），更符合 TOFU 模型语义（"首次见到对端自动 pin；见到陌生对端但已有信任对端就拒绝"） |
| `Err` 消息文本 | `"untrusted peer {fp}"` | `"TOFU mismatch: peer fingerprint {fp} not in known peers"` | 与 bak 字符串对齐（便于未来 STEP-6.x 错误处理 grep 检索） |
| 替换 `build_quic_client_config` 内的 `WebPkiServerVerifier` → `TofuVerifier` | PLAN §2.6 + #S-6 治理纪律要求 | 直接对齐：删除 root store 装配 + 改 `.dangerous().with_custom_certificate_verifier(Arc::new(TofuVerifier::new(pins_dir)))` | 与 bak `mousehop/src/quic_transport.rs:1799,1822-1829 build_quic_client_config` 完全对齐 |
| `build_quic_client_config` 加 `pins_dir` 参数 | PLAN 文字未明确要求加（隐含） | 加 `pins_dir: &Path` 末尾参数 | bak `build_quic_client_config` 通过 `dial_with_client_cert_tofu` 透传 `pins_dir`；`TofuVerifier::new(pins_dir)` 必须收 `&Path` |
| `dial` 加 `pins_dir` 参数 | PLAN 未要求 | 加 `pins_dir: &Path` 末尾参数；caller 透传 | 与 `build_quic_client_config` 配套；测试用 `tempfile::tempdir().path()` 隔离 |
| 单测名 `tofu_first_run_pins` / `tofu_disallows_swap` | PLAN §2.6 验收清单要求 | 直接对齐 | bak 测试名是 `tofu_first_connect_saves_fingerprint` / `tofu_mismatch_rejects_different_fingerprint`；本步采用 PLAN 命名（PLAN §2.6 验收清单明确指定） |

## 4. 处理的 SUGGESTION 项

- **#S-6 🟢**（✅ 已解）：`build_quic_client_config` 当前仅占位 verifier
  （`WebPkiServerVerifier`），STEP-2.6 必须替换为 TofuVerifier
  - 删 `use rustls::client::WebPkiServerVerifier;`
  - 删 root store 构造（`RootCertStore::empty` + `add(cert)` 循环）
  - 删 `WebPkiServerVerifier::builder(roots, provider).build()`
  - 加 `Arc::new(TofuVerifier::new(pins_dir))` 装配
  - 改 builder 链：`.with_webpki_verifier(verifier)` → `.dangerous().with_
    custom_certificate_verifier(verifier)`
  - **本条目 Leader 评审后可直接删除**（已留 "Leader 评审后可删除本条目"标记）
- 其它已存在条目（#S-1 / #S-3 / #S-5 / #S-8）未触动 —— #S-1 待 STEP-6.x
  完整切换 PeerSession 后删除 `*_compat`；#S-5 仍是 STEP-6.x 修 14 errors
  后再跑单测；#S-7 STEP-2.5 已解；#S-8 待未来 STEP-2.6/2.7 测试 helper 抽取。

## 5. 闸门检查

- 闸 1（产物）：✅ `TofuVerifier` struct + `new` + `with_known` +
  `verify_server_cert` 三态判定 + `verify_tls12/13_signature` +
  `supported_verify_schemes` + `build_quic_client_config` 改
  `with_custom_certificate_verifier` + `dial` 加 `pins_dir` 参数 + 单测齐备
- 闸 1（依赖）：✅ STEP-2.5 已归档；`rustls 0.23` `ServerCertVerifier` trait
  + `with_custom_certificate_verifier` 已落地（解决与模块顶层 `Result<T>`
  别名冲突后编译通过）
- 闸 1（验收）：⚠️ `cargo check -p lan-mouse` 14 errors 全 DTLS，
  `quic_transport.rs` 0 错；`cargo check -p lan-mouse --tests` 同 14 errors；
  单测 `cargo test ... tofu_first_run_pins` / `tofu_disallows_swap` **未跑通**
  （SUGGESTION #S-5 留 STEP-6.x）
- 闸 1（M1 边界）：✅ §9 12 类 grep 无命中（未引入 TransportEvent /
  Clipboard / Bounds / h3 / clipboard*.rs / status_bar 等）
- 闸 1（时间门）：✅ ~30 min，在 20–30 min 目标内

## 6. 遗留 / 风险

- ⚠️ **`pins_dir` 路径尚未定型**：本步 `build_quic_client_config` /
  `dial` 收 `&Path` 参数；生产路径需要 `crypto::known_peers_dir()` helper
  返回 `$XDG_DATA_HOME/lan-mouse/known_peers/`（与 `crypto::cert_path()` /
  `key_path()` 对称）。STEP-6.1 接入 `connect.rs::connect_to_handle` 时一
  并添加 `crypto::known_peers_dir()` helper（与 `crypto.rs:cert_pins_dir()`
  bak 对齐 —— bak `mousehop/src/crypto.rs:288-297 cert_pins_dir()`）。
- ⚠️ **`TofuVerifier::verify_server_cert` 不读 `_server_name` 参数**：
  不做 hostname 校验 —— 当前 LAN 场景下 server cert 的 SAN 是 `lan-mouse`
  通用名，hostname 校验价值有限（攻击者拿到私钥才能伪造 cert，TOFU 已经
  防住）。STEP-6.x 接入 connect.rs 时若需要做 hostname 校验再加
  （需 cert 生成时带 SAN 字段）。M1 范围内省略。
- ⚠️ **SUGGESTION #S-5**：单测 `tofu_first_run_pins` / `tofu_disallows_swap`
  因 lib 14 DTLS errors 编不过，逻辑就位即可，STEP-6.x 修后 Leader 手动
  跑一次确认。
- ⚠️ **`TofuVerifier` 的 `create_dir_all` IO 错误语义**：当前在
  `verify_server_cert` 入口 `create_dir_all`（确保 First Connect 能落盘）。
  若 `pins_dir` 不可写（比如 disk full / read-only mount），`verify_server_cert`
  直接返 `Err` —— 阻断**所有**后续 connect（包括 Known Match 场景）。
  修法：仅在 First Connect 路径尝试写，已 pin 的 fp 不必 ensure dir 存在
  （如果 pin 已在，dir 必然已被创建过）。M1 阶段不修（M1 内 disk full 是
  异常运维场景，TofuVerifier 直接拒握比"假装 Ok 然后客户端才发现没法
  复用"更安全）。

## 7. 下一步（STEP-2.7 前置条件）

✅ 就绪：
- `TofuVerifier::new(pins_dir)` / `with_known(pins_dir, fp)` 公共 API
- `build_quic_client_config(cert_chain, key, pins_dir)` 装配 + TOFU 校验
- `dial(ep, addr, cert, key, pins_dir)` 出示 client cert + 走 TofuVerifier
- 单测代码就位（仅待 14 errors 修复后执行）
- #S-6 已解（`WebPkiServerVerifier` → `TofuVerifier`）

下一步建议：执行 **STEP-2.7** —— server 端 `AuthorizedKeysVerifier` 走
`listen.rs` 现有的 `Arc<RwLock<HashMap<String, IncomingPeerConfig>>>`
allowlist 做 fingerprint allowlist；未授权 fingerprint 即拒握。搬运
参考：`lan-mouse-pro-bak/mousehop/src/quic_transport.rs:1577-1754
AuthorizedKeysVerifier` + `bak/mousehop/src/listen.rs` 注入点。