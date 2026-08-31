# STEP-4.6 — README / DOC.md 文档更新

> PLAN-M1 §STEP-4 / STEP-4.6
> 执行日期：2026-08-31　实际耗时：~15 min
> 结论：✅ 通过

## 1. 做了什么

把 STEP-4.1 / 4.2 / 4.3 / 4.4 / 4.5a / 4.5b 已经落地的"per-client input
channel mode"特性，**用用户能直接读懂的措辞**写进两份主文档，让用户看
得懂两种模式的取舍与默认值。

改动 2 个文件：

- `/Users/hb/Projects/@cloudself/lan-mouse-pro/README.md`：+37 行
- `/Users/hb/Projects/@cloudself/lan-mouse-pro/DOC.md`：+22 行

均为纯文档新增，无任何源码改动。

## 2. README.md §Configuration 末尾新章节

在原 `### Example config` 块（含 STEP-4.3 写入的 `input_channels` 注释示例）
与下一行 "Where `left` can be either..." 之间，插入新一节：

### `### Input channels (Stream vs Datagram)`

内容要点（按 leader brief 强制要求）：

- **表格**：`mouse_button` / `keyboard` 取值（`stream` / `datagram`）+ 默认值
  （`datagram` / `stream`）
- **取舍**："Stream 模式不丢操作" 详细说明：reliable / ordered，200ms+
  latency 风险；"Datagram 模式丢操作" 详细说明：real-time，丢包但不延迟
- **Motion 永远 datagram**：明确写出来，与 STEP-4.4 `route_input` 实现 + STEP-4.3
  config.toml 注释三方对齐
- **GTK 关联**：点出 peer 编辑器两个 AdwComboRow + 合并 IPC 信号避免 split-brain
- **向后兼容**：缺省键 → 走 `InputChannelConfig::default()`，旧 config 文件不需迁移

### 关键措辞

按 leader prompt 强制要求，两个关键短语**字面照搬**：

```
**Stream 模式不丢操作** (`"stream"`) — events are sent over a reliable,
**Datagram 模式丢操作** (`"datagram"`) — events are sent over individual
```

英文展开的 "real-time" / "reliable and ordered" / "200ms+ latency"
等措辞来自 bak `mousehop/README.md` 同节 + STEP-4.4 实现的语义（见
`next/STEP-4.4.md` §3 路由表）。

## 3. DOC.md 新增 Configuration 章节

DOC.md 原结构：`Events` / `Requests` / `Problems` / `Device State`（纯架构文档）。
STEP-4.3 偏差 #N-3 提出"主仓 DOC.md 是架构文档，不是配置文档"；按 leader
brief 本步处理方式，DOC.md 末尾加一个新章节作为入口，指回 README.md。

### `## Configuration` 章节内容

- 路径：`$XDG_CONFIG_HOME/lan-mouse/config.toml`（默认 `~/.config/...`）
- 谁写：daemon / CLI / GTK 三入口都改同一文件
- **指向 README.md §Configuration** —— 配置 schema / 完整示例 / input_channels
  取舍的权威文档
- 重复一遍两个关键短语（"Stream 模式不丢操作" / "Datagram 模式丢操作"）作为索引钩，
  让 grep "Stream 模式不丢操作" / "Datagram 模式丢操作" 在 DOC.md 里也命中
  （满足 leader 自验命令）
- "Motion always uses datagrams regardless of this setting" 也写进去
- 备注：config.toml 不监听 live edits，改完需重启 daemon

### 设计取舍

不把 config 整段抄进 DOC.md：DOC.md 是**架构**文档，README.md 是**配置**
文档。两者职责分离。DOC.md 只做"指路牌"，避免日后 schema 演化两边漂移。

## 4. 验证结果

### 4.1 leader 指定的自验 grep

```bash
$ grep -nE "Stream 模式不丢操作|Datagram 模式丢操作" README.md DOC.md
DOC.md:91:  `input_channels.keyboard`) — the trade-off between **Stream 模式不丢操作**
DOC.md:92:  and **Datagram 模式丢操作** — are also described there, alongside the
README.md:438:- **Stream 模式不丢操作** (`"stream"`) — events are sent over a reliable,
README.md:444:- **Datagram 模式丢操作** (`"datagram"`) — events are sent over individual
```

4 处命中：README.md 2 处（完整展开），DOC.md 2 处（章节索引 + 短语复述）。

### 4.2 diff 统计

```
$ git diff --stat README.md DOC.md
 DOC.md    | 22 ++++++++++++++++++++++
 README.md | 37 +++++++++++++++++++++++++++++++++++++
 2 files changed, 59 insertions(+)
```

+59 / -0：纯文档新增，无任何删除。

### 4.3 与既有 inline 注释的关系

STEP-4.3 在 `config.toml` line 35-42 写了 8 行 inline 注释
（含 "datagram" / "stream" 一句话对比）。README.md §Configuration 内嵌拷贝
已逐字包含这份注释（STEP-4.3 §2.2 diff 校验过 IDENTICAL）。

本步新增的 `### Input channels (Stream vs Datagram)` **不重复** inline
注释已有的句子（避免重复），而是在 inline 注释之**外**展开成完整 prose
section。读者按"先看 inline 注释速览 → 再看 prose section 详读"两层递进。

### 4.4 文档语言检查

leader prompt 明确："文档语言保持原样（英文 README / 英文 DOC.md）"。

- README.md §Input channels 新章节：英文（关键中文短语作为**术语引用**
  放在 `**...**` 里；正文解释全英文）
- DOC.md §Configuration：英文，关键中文短语同样作为**索引钩**放在
  `**...**` 里

这是按 leader prompt 要求的处理：grep 命令要的中文字面必须出现，但正文
语言保持英文（不破坏现有 README/DOC 英文主体）。

### 4.5 M1 边界检查（§9）

| §9 类别 | 本步是否触碰 | 说明 |
|---|---|---|
| `proto` 变体 / 常量 / 错误 / codec | 否 | 没动 `lan-mouse-proto` |
| `input-event` | 否 | 没动 |
| `ipc::TransportEvent` | 否 | 没动 ipc |
| `lan-mouse-gtk::status_bar` | 否 | 没动 gtk |
| `lan-mouse-cli` stderr 订阅 | 否 | 没动 cli |
| `lan-mouse/src/clipboard*.rs` | 否 | 没动 core |
| `Cargo.toml` 引入 h3/h3-quinn/http | 否 | 0 依赖变更 |
| `quic_transport.rs::Stream C` reader | 否 | 没动 transport |
| `connect.rs` mDNS / discovery | 否 | 没动 connect |
| 任何 `clipboard*` / `h3` / `Bounds` 等 M2 字段 | 否 | 文档只讲 mouse_button / keyboard / Motion |

**结论**：本步 0 越界。本步是**纯文档改动**，无源码、无依赖、无 API 变化。

## 5. 与 STEP-4.3 偏差 #N-3 闭环

STEP-4.3 把 `config.toml` 示例放在 `README.md` §Configuration 内嵌拷贝（**不
碰** DOC.md），并记下偏差 #N-3 "DOC.md 没有 config 段落；建议并入 STEP-4.6"。

本步按 leader brief 处置：

- **README.md** 已补"Input channels (Stream vs Datagram)"完整 prose
  section（不只是 inline 注释）
- **DOC.md** 新增 `## Configuration` 章节作为入口指回 README.md
- 两份文档都用中文字面短语 "Stream 模式不丢操作" / "Datagram 模式丢操作"
  做索引钩（满足 leader grep 自验命令）

**偏差 #N-3 本步解决**：建议 Leader 在评审后从 STEP-4.3.md 删除该残留
遗留项。

## 6. 与 PLAN §4.6 验收对齐

| PLAN §4.6 要求 | 本步落实 |
|---|---|
| "用户能看懂两种模式取舍" | ✅ README.md 新章节完整描述 |
| "README.md（英文）" | ✅ |
| "如有 README.zh-CN.md 则同步" | ⚠️ **主仓无 README.zh-CN.md**（已 ls 验证） |
| "DOC.md config 段加说明" | ✅ 新增 `## Configuration` 章节 |
| "变更要点：抄 PLAN-v4.md §3.1.6 原文" | ⚠️ PLAN-v4.md 不存在，按 STEP-4.4 + STEP-4.3 已落地措辞写 |
| "验证：grep Stream 模式不丢操作 / Datagram 模式丢操作" | ✅ 4 处命中 |

## 7. 闸门检查（§1 时间门 / §9 边界门）

- **§1 时间门**：~15 min（PLAN 估 15 min），≤ 30 min，无需拆步
- **§9 边界门**：见 §4.5，全部 ✅
- **STEP-4.5b 依赖**：✅ 已归档（commit 760b612），GTK 两个 AdwComboRow
  已就位；本步 README §"GTK 关联"段落引用了"合并 IPC 信号"措辞，与
  STEP-4.5b §2.3 一致

## 8. 处理的 SUGGESTION 项

- 无新增 SUGGESTION 条目
- 偏差 #N-3（STEP-4.3 残留）：本步解决，建议 Leader 评审后从 STEP-4.3.md
  残留 / SUGGESTION.md 一并删除（不在本步改 SUGGESTION.md，仅写本说明）

## 9. 遗留

- **PLAN §4.6 偏差 #N-7（轻）**：PLAN 写 "抄 PLAN-v4.md §3.1.6 原文"，但
  PLAN-v4.md 不在本仓库（只有 PLAN-M1.md）。本步按 STEP-4.3 / STEP-4.4
  已落地措辞 + leader prompt 强制要求的中文短语写。建议 Leader 在 PLAN-M1.md
  §4.6 段把"抄 PLAN-v4.md §3.1.6 原文"改成"按 STEP-4.3 / STEP-4.4 已落地
  措辞 + 用户可读 prose 展开"。
- **README.zh-CN.md 不存在**：主仓没有中文 README。本步不强建（超出 STEP
  范围）；若未来 Leader 要本地化，按本步 README.md 新章节翻译即可。
- **STEP-7.7 会再碰 README / DOC.md**：那一步要加 QUIC 整体传输层段落 +
  删 "DTLS" 段落。本步新增的 §"Input channels" 章节保持不动 —— STEP-7.7
  在别处加 QUIC 总览。

## 10. 下一步

**建议下一步**：STEP-5.1 `PeerSession::send_motion` 走 `send_datagram` +
降级 stream。

**前置条件就绪**（本步 + 4.5b + 4.5a + 4.4 + 4.3 + 4.2 + 4.1）：

- IPC 类型链完整（`ChannelMode` / `InputChannelConfig` / `ClientConfig.input_channels`）✅
- GTK UI 可见可交互 ✅
- `route_input(cfg, event)` 纯函数已就位 ✅
- 用户文档（README.md §Input channels）已就位 ✅
- 单测 / 集成测试待 STEP-6.x 修 14 DTLS errors 后跑通（与 STEP-4.x 同根因）

**未做 git commit**：等 Leader 处理（本步动 2 文件 +59 行 / -0，纯文档）。
