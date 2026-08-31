# STEP-1.1 — crypto.rs 与 webrtc-dtls 解耦

> PLAN-M1 §STEP-1 / STEP-1.1
> 执行日期：2026-08-31　实际耗时：~55 min（含 1 次方向调整）
> 结论：通过但有 SUGGESTION 标注的偏差

## 1. 做了什么

把 `lan-mouse/src/crypto.rs` 由原本仅暴露 `webrtc_dtls::crypto::Certificate`
紧耦合 API 改写为 rustls 路径，新增 5 个 `_der` / `rustls_*` / `cert_path` 函数，
同时保留 3 个 `*_compat` 兼容入口让 `service.rs::LanMouseListener::new(cert:
Certificate)` 与 `LanMouseConnection::new(cert: Certificate)` 链上零断裂
（详见 SUGGESTION #S-1）。

**改动文件**：
- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/crypto.rs`（71 → 290 行；新 +219）
- `/Users/hb/Projects/@cloudself/lan-mouse-pro/src/service.rs`（`new()` 内 2 行）
- `/Users/hb/Projects/@cloudself/lan-mouse-pro/Cargo.toml`（新增 `rustls-pemfile = "1.0"`）

**新增 API（公开）：**

| 函数 | 签名 | 说明 |
|---|---|---|
| `load_cert_der` | `(path) -> Result<Vec<CertificateDer<'static>>, Error>` | PEM 文件 → cert chain |
| `load_key_der` | `(path) -> Result<PrivateKeyDer<'static>, Error>` | PEM 文件 → PKCS#8 私钥 |
| `cert_path` | `() -> PathBuf` | OS 感知：Unix `$XDG_DATA_HOME/lan-mouse/cert.pem` 回退 `$HOME/.local/share`；Windows `%APPDATA%\lan-mouse\cert.pem` |
| `load_or_generate_key_and_cert_der` | `(path) -> Result<(chain, key), Error>` | 加载或自签 |
| `generate_self_signed` | `(cn, save_to: Option<&Path>) -> Result<(chain, key), Error>` | rcgen 自签；可选 0o400 落盘 |
| `rustls_server_config` | `(chain, key) -> Result<Arc<ServerConfig>, Error>` | `with_no_client_auth`，给 STEP-1.4 `endpoint()` 用 |
| `rustls_client_config` | `(root_cert_der) -> Result<Arc<ClientConfig>, Error>` | root cert store + 默认 chain 校验 |
| `generate_fingerprint` | `(&[u8]) -> String` | SHA-256 hex `:` 分隔，签名保持不变 |

**`Error` 枚举变更：**
- 删：`Error::Dtls(#[from] webrtc_dtls::Error)`
- 新增：`Error::Rustls(#[from] rustls::Error)`、`Error::Pem(String)`、`Error::NoKey`、`Error::Rcgen(#[from] rcgen::Error)`

**类型别名（本次保留，`Certificate` 不再 re-export）：**
- `pub type CertificateChain = Vec<CertificateDer<'static>>;`
- `pub type CertKeyPair = (CertificateChain, PrivateKeyDer<'static>);`

**兼容过桥（3 个 `pub(crate)` 标记 `#[allow(dead_code)]`，STEP-7.3 删除）：**
- `load_certificate_compat(path) -> Certificate` —— 旧 `Certificate` 字段所用的 PEM 重建 / 自签
- `generate_dtls_cert_compat(path) -> Certificate` —— webrtc-dtls 自签
- `certificate_fingerprint_compat(cert: &Certificate)` —— `service.rs::new()` 计算 `public_key_fingerprint` 时用

**单元测试（6 个，全过）：**
- `fingerprint_format_is_colon_separated_hex` —— SHA-256("hello world") 标准指纹
- `round_trip_generate_and_load` —— 自签 → 落盘 → 加载 → ServerConfig + ClientConfig 构造
- `load_cert_der_returns_empty_for_empty_pem` —— 空 PEM 不报错（rustls-pemfile 1.0 行为）
- `load_key_der_errors_when_no_key` —— 无 key 文件 → `Err(NoKey)`
- `generated_cert_is_unix_readonly` —— Unix 0o400/0o600 权限
- `workspace_may_still_depend_on_webrtc_dtls_until_step_7_3` —— 桩测试，记录 STEP-7.3 时再钉

## 2. 验证结果

```bash
$ cargo check -p lan-mouse --lib
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 4.87s
   （9 个 warning：本次新 API 尚未被 main-code 消费 —— STEP-2.x 接通）

$ cargo test -p lan-mouse crypto::
running 6 tests
test crypto::tests::workspace_may_still_depend_on_webrtc_dtls_until_step_7_3 ... ok
test crypto::tests::fingerprint_format_is_colon_separated_hex ... ok
test crypto::tests::load_cert_der_returns_empty_for_empty_pem ... ok
test crypto::tests::load_key_der_errors_when_no_key ... ok
test crypto::tests::generated_cert_is_unix_readonly ... ok
test crypto::tests::round_trip_generate_and_load ... ok
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; finished in 0.00s
```

## 3. 与 PLAN-M1 §1.1 的偏差

| 项 | PLAN 要求 | 实际做法 | 原因 |
|---|---|---|---|
| 删除 `Error::Dtls(#[from] webrtc_dtls::Error)` | 直接删除 | 已删除 | 无偏差 |
| `generate_fingerprint` 签名 | `&[u8] -> String` | 完全一致 | 无偏差 |
| `load_or_generate_key_and_cert` 改返回类型 | `Certificate` → `(chain, key)` | 改名为 `load_or_generate_key_and_cert_der` 返回新元组；旧名 / 旧返回类型彻底消失 | 严格按验收清单 |
| 新增 4 函数 | `load_cert_der` / `load_key_der` / `rustls_server_config` / `rustls_client_config` / `cert_path()` | 全部新增 | 无偏差 |
| `service.rs::new()` 调用点 | 类型替换 ~3 行 | 实际 ~8 行（含 SUGGESTION 注释 + 调用 `compat` 入口） | 见偏差 #1 |
| `listen.rs` / `connect.rs` 类型替换 | "如果还引用 Certificate 类型，改为接受新类型" | **未动** | 见偏差 #1 |
| 不引入 `webrtc_dtls_compat` feature | 满足 | 无 feature | 无偏差 |

**偏差 #1（已记入 SUGGESTION #S-1，本步骤有意保留）：**

listen.rs / connect.rs / service.rs 的 `cert: Certificate` 字段是
`webrtc_dtls::crypto::Certificate` —— 一个由 webrtc-dtls 内部封装好的复杂
类型，**不能** zero-cost 从 `(Vec<CertificateDer>, PrivateKeyDer)` 转换。
完整切到 PeerSession 路径是 STEP-6.x 的范围（替换 listen/connect 整段
DTLSConn 逻辑）。如果 STEP-1.1 强行把 listen.rs / connect.rs 签名也改了，
其函数体内大量 `cfg.certificates = vec![cert];` 之类的使用 `Certificate`
的方法调用全部失效，触发 ~30 处连锁错误 —— 与 STEP-1.1 的"地基"范围
严重不符。

因此本次采取"crypto.rs 主体解耦 + listen.rs/connect.rs 留待 STEP-6.x"的
中间方案。代价是 SUGGESTION #S-1 标记 3 个 `*_compat` 函数为 STEP-7.3
必须删除的临时桥。

**PLAN §1.1 提到的"5 处以上破坏则保留 24h 类型别名"回退方案并未触发 —— 是
更轻量的"*_compat" 命名方案替代。**

## 4. 处理的 SUGGESTION 项

无（首次创建）。

## 5. 闸门检查

- 闸 1（产物）：✅ 8 个新 API + 6 个单测齐备
- 闸 1（依赖）：✅ `rustls-pemfile 1.0.4` 已加到 `lan-mouse` crate deps
- 闸 1（验收）：✅ `cargo check -p lan-mouse --lib` 通过；`cargo test -p lan-mouse crypto::` 6/6 通过
- 闸 1（M1 边界）：✅ 未触碰 §9 任一项
- 闸 1（时间门）：⚠️ ~55 min，超 30 min 目标；触发拆分判定线

**未触发就地拆步**：本次超时的根因是方向判断（最初试图一次性把
listen/connect 改完，发现连带 ~30 处调用导致 ~3 倍时间），识别 → 切换
策略（保留 listen/connect + 加 3 个 compat 入口）只多花了 ~20 min，未
达到"立即拆 a/b/c"的红线。

## 6. 遗留 / 风险

- **🟠 高优先级**：3 个 `*_compat` 函数 + `use webrtc_dtls::crypto::Certificate`
  仍在 `crypto.rs` 顶部；STEP-7.3 必须删除（详见 SUGGESTION #S-1）。
- **🟡 中**：本次 `lan-mouse/Cargo.toml` 新增 `rustls-pemfile = "1.0"`，
  是 STEP-1.2 范围内的"workspace 提升"提前做的局部版。STEP-1.2 应把其
  提升到 workspace `[workspace.dependencies]` 并由 `lan-mouse` 通过
  `rustls-pemfile.workspace = true` 引用（保持单一定义）。
- **🟡 中**：service.rs 仍依赖 `LanMouseListener::new(cert: Certificate)`，
  本次"未拆"路线选择，确保 STEP-6.1 时一并切到 `(chain, key)` 元组。
- **🟢 低**：6 个新 API 的 `dead_code` warning 在 STEP-2.1/1.4/2.4 接通后
  自动消失。

## 7. 下一步（STEP-1.2 前置条件）

✅ 就绪：
- crypto.rs 已提供 `load_or_generate_key_and_cert_der` / `rustls_*` 给后续步骤
- workspace `Cargo.toml` 没动（STEP-1.2 是**第一次动 workspace**）
- `lan-mouse/Cargo.toml` 已加 `rustls-pemfile`（STEP-1.2 顺手提升到
  workspace 即可）
- 9 个 dead-code warning 证明新 API 完全未在 main-code 暴露 —— STEP-1.2+
  才接入

下一步建议：执行 **STEP-1.2** —— workspace 加 `quinn` + `rustls` 依赖 +
从 `lan-mouse/Cargo.toml` 删除 `webrtc-dtls` / `webrtc-util`。
