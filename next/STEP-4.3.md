# STEP-4.3 — `config.toml` 示例更新

> PLAN-M1 §STEP-4 / STEP-4.3
> 执行日期：2026-08-31　实际耗时：~20 min
> 结论：⚠️ 通过但有 2 处偏差（均因主仓与 PLAN/bak 的文档结构差异，已按最保守读法处置，待 Leader 确认）

## 1. 做了什么

在示例配置里补上 `input_channels` 的注释示例，让用户知道 STEP-4.1/4.2
落地的这个字段存在、取什么值、默认是什么。

改动 2 个文件，**均为纯注释，无任何源码改动**：

- `/Users/hb/Projects/@cloudself/lan-mouse-pro/config.toml`：+8 / -0 行
- `/Users/hb/Projects/@cloudself/lan-mouse-pro/README.md`：+8 / -0 行（§Configuration 内嵌的那份 config.toml 逐字拷贝，line 411 后）

两处内容**逐字一致**（见 §2 diff 校验）。

### 1.1 加在哪里：第二个 `[[clients]]`（thorium）末尾

追加的 8 行（`config.toml` line 35-42）：

```toml
# optional per-client input channel modes.
# mouse_button and keyboard each accept "datagram" (low latency, may drop
# on packet loss) or "stream" (reliable and ordered). Mouse motion always
# uses datagram regardless of this setting.
# Omitting the key keeps the defaults, which are:
# input_channels = { mouse_button = "datagram", keyboard = "stream" }
# For the lowest possible input latency, at the risk of dropped keys:
# input_channels = { mouse_button = "datagram", keyboard = "datagram" }
```

设计取舍：

1. **全部注释掉**（不是 active 配置）—— 与 bak `config.toml:35-36` 完全同构。
   示例文件被用户直接 `cp` 走，若写成生效配置会**静默改变**其传输行为。
2. **放第二个 client 块末尾** —— 对齐 bak 结构（bak 也在 thorium 块末尾）。
3. **给两组取值** —— 第一行是 PLAN §4.3 硬性要求的默认组合
   （`datagram`/`stream`）；第二行给非默认组合说明"这个字段能改什么"
   （纯默认值的示例对读者信息量为 0）。第二行取值抄 bak（`datagram`/`datagram`）。
4. **点明 Motion 不受此设置影响** —— 呼应 PLAN §4.4 "Motion 永远走 Datagram"，
   避免用户误以为设 `mouse_button = "stream"` 能让鼠标移动变可靠。

## 2. 验证结果

### 2.1 PLAN §4.3 指定的 grep

```bash
$ grep -A 1 input_channels config.toml
# input_channels = { mouse_button = "datagram", keyboard = "stream" }
# For the lowest possible input latency, at the risk of dropped keys:
# input_channels = { mouse_button = "datagram", keyboard = "datagram" }
```

`README.md` 同 grep 输出一致。

### 2.2 两份拷贝一致性

```bash
$ diff <(sed -n '/^# example configuration/,$p' config.toml) \
       <(sed -n '/^# example configuration/,/^```$/p' README.md | sed '/^```$/d')
IDENTICAL ✅
```

### 2.3 `cargo test -p lan-mouse config::tests` —— 跑不通（预期内）

```
error: could not compile `lan-mouse` (lib) due to 14 previous errors
error: could not compile `lan-mouse` (lib test) due to 14 previous errors
```

14 errors **全部**来自 `src/connect.rs` / `src/listen.rs` 的 `webrtc_dtls` /
`webrtc_util` 引用（STEP-1.2 故意留下，待 STEP-6.x 切 PeerSession），
**0 来自 config**。与 SUGGESTION #S-5 同根因，非本步引入的回归。

### 2.4 补偿验证：示例行真的能反序列化（本步新增手段）

因 §2.3 阻塞，"示例与新 schema 一致"这条验收若只靠肉眼看会**验不到**。
故临时建了个 `/tmp/ic_verify` 一次性 crate（依赖真实的
`lan-mouse-ipc` + `toml`，验完即 `rm -rf` 删除，未落任何文件进仓库），
把注释里的两行**原样**喂给真实的 `InputChannelConfig` 反序列化：

```
ok  documented default line:      mouse_button=Datagram keyboard=Stream
ok  documented low-latency line:  mouse_button=Datagram keyboard=Datagram
ok  key omitted keeps defaults:   mouse_button=Datagram keyboard=Stream
ok  documented default == InputChannelConfig::default()
ALL EXAMPLE LINES VERIFIED
```

第 4 条是关键：注释里断言"这就是默认值"，该断言被**证明**等于
`InputChannelConfig::default()`，而不是靠人工核对。

再对整份 `config.toml` 做一次全文件解析：

```
config.toml parses OK: port=4242 fingerprints=1 clients=2
  client hostname="iridium" position="right" input_channels=None
  client hostname="thorium" position="left"  input_channels=None
OK: all input_channels are None -> examples are inert comments
```

同时证明两件事：**(a)** 加注释没破坏 TOML 语法；**(b)** 两个 client 的
`input_channels` 都是 `None` —— 新增行确实是惰性注释，对照抄此配置的
用户行为零变化（向后兼容）。

## 3. 与 PLAN-M1 的偏差 / M1 边界

### 3.1 偏差 #N-3：`DOC.md` 没有 config 段落，改的是 `README.md`

**现象**：PLAN §4.3 与 Leader brief 都写"`DOC.md` 中 config 段落"，但主仓
`DOC.md` 是**纯软件架构文档**（77 行：Events / Requests / Problems /
Device State），**完全没有** config 段落。主仓真正的配置文档在
`README.md` §Configuration（line 367-416），其中内嵌一份 `config.toml`
的逐字拷贝。

**根因**：PLAN §4.3 的表述源自 bak —— bak 的 `DOC.md` 确实有 config 章节
（`bak/DOC.md:127-213` 讲 `input_channels` 的持久化语义）。主仓 `DOC.md`
尚未演化出该章节。

**处置（本步选择）**：改 `README.md` 内嵌拷贝，**不动** `DOC.md`。理由：
1. README §Configuration 才是本仓用户实际查配置的地方；
2. 在架构文档里新开 Configuration 章节属**结构性改动**，超出"示例更新"
   这一步的范围（PLAN §4.3 估时 15 min）；
3. PLAN §4.6 本来就要改 README/DOC 文档，届时若 Leader 要在 DOC.md 补
   config 章节，一并做更合适。

**待 Leader 确认**：是否接受"README 代替 DOC.md"。若坚持要 DOC.md 有
config 章节，建议并入 STEP-4.6（那一步本就是文档步）。

### 3.2 偏差 #N-4：`[clients.desktop-east]` 段在主仓不存在

**现象**：PLAN §4.3 / brief 要求"在 `[clients.desktop-east]` 段下加注释
示例"。主仓 `config.toml` 用的是 TOML **数组表** `[[clients]]`，两个 client
是 `iridium`(right) / `thorium`(left)，**没有** `desktop-east` 这个名字，
也没有 `[clients.<name>]` 这种 inline-table 写法。

**根因**：`desktop-east` 应是 PLAN 撰写时的泛指占位名，非主仓实际 schema。

**处置**：按语义等价映射到"第二个 `[[clients]]` 块（thorium）末尾"，
与 bak `config.toml:35-36` 的位置完全一致。

### 3.3 字段顺序 / 向后兼容

PLAN §4.3 要求"字段顺序保留向后兼容（缺省可省略）"：

- 新增行**追加在块末尾**，既有字段（`position` / `hostname` / `ips` /
  `port`）顺序与内容 **1 字节未动**（见 §2 diff：纯 `+`，无 `-`）
- 全部注释掉 → 缺省即省略，§2.4 已证明解析结果 `input_channels=None`
- 旧 config 文件不受影响（`Option` + `#[serde(default)]`，STEP-4.2 已落）

### 3.4 M1 边界检查（§9）

| §9 类别 | 本步是否触碰 | 说明 |
|---|---|---|
| `proto` 变体 / 常量 / 错误 / codec | 否 | 没动 `lan-mouse-proto` |
| `input-event` | 否 | 没动 |
| `ipc::TransportEvent` | 否 | 没动 ipc（只在 /tmp 临时 crate 里**读**了 `InputChannelConfig`，已删） |
| `lan-mouse-gtk::status_bar` | 否 | 没动 gtk |
| `lan-mouse-cli` stderr 订阅 | 否 | 没动 cli |
| `lan-mouse/src/clipboard*.rs` | 否 | 没动 |
| `Cargo.toml` 引入 h3/h3-quinn/http | 否 | **0 依赖变更**（/tmp 临时 crate 不在 workspace 内，已 `rm -rf`） |
| `quic_transport.rs::Stream C` reader | 否 | 没动 |
| `connect.rs` mDNS / discovery | 否 | 没动 |
| 剪贴板 / 文件同步文案 | 否 | 注释只讲 mouse_button / keyboard 两个字段 |

**结论**：本步 0 越界。本步是**纯注释改动**，无源码、无依赖、无 API 变化。

## 4. 处理的 SUGGESTION 项

- **#S-5（🟡 中）**：本步再次撞上"lib 14 DTLS errors 导致 `cargo test -p
  lan-mouse` 跑不起来"。**未删除**该条目（根因仍在，待 STEP-6.x），但本步
  用 §2.4 的独立反序列化验证补上了验收缺口 —— 建议后续被同一问题阻塞的
  STEP 复用这一手段，而非把"测试跑不通"直接记为遗留。已在 SUGGESTION.md
  的 #S-5 下补一段说明。
- 无新增 SUGGESTION 条目。

## 5. 闸门检查（§1 时间门 / §9 边界门）

- **§1 时间门**：~20 min（PLAN 估 15 min），低于 30 min 目标，无需拆步。
- **§9 边界门**：见 §3.4，全部 ✅。
- **STEP-4.2 依赖**：✅ 已归档（commit cf1f0b4），`ConfigClient.input_channels`
  + `TomlClient.input_channels: Option<...>` 就位；本步示例的 wire 格式
  （`rename_all = "lowercase"` → `"datagram"` / `"stream"`）与之一致，
  §2.4 已实证。
- **闸 3 全量回归**：⏸ 未跑。本步为纯注释改动（0 行源码），且
  `cargo build --workspace` 目前必然因 14 个既有 DTLS errors 失败
  （STEP-1.2 遗留，STEP-6.x 才修）——跑了也只会重现既有红，无新信息。

## 6. 遗留

- **偏差 #N-3 待 Leader 拍板**：`DOC.md` 要不要补 config 章节（建议并入 STEP-4.6）。
- **#S-5 根因仍在**：`cargo test -p lan-mouse config::tests`（含 STEP-4.2 那 2 个
  单测）要等 STEP-6.x 消灭 14 DTLS errors 后才能真跑；届时请 Leader 补跑一次。
- **STEP-4.6 会再碰 README**：那一步要加"Stream 模式不丢操作 / Datagram 模式
  丢操作"的取舍说明。本步注释已埋下 datagram/stream 的一句话对比，
  STEP-4.6 展开时注意**不要与本步注释重复或矛盾**。

## 7. 下一步

**建议下一步**：STEP-4.4 `route_input()` 纯函数 + 四个组合测试。

**前置条件就绪**：
- `InputChannelConfig` / `ChannelMode` 已稳定 `pub`（STEP-4.1）
- `ConfigClient.input_channels` 可直接读（STEP-4.2）
- 本步已把"Motion 永远走 Datagram"写进用户可见文档 —— STEP-4.4 实现
  `route_input()` 时必须与该文案保持一致，否则文档与实现打架

**未做 git commit**：等 Leader 处理（2 文件，各 +8 行，纯注释）。
