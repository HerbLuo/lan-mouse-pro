# Mousehop QUIC 迁移与扩展路线图

> 范围：把现有基于 `webrtc-dtls + UDP` 的传输替换为 QUIC，并在此之上引入文本、大文本、剪贴板图片、与"复制文件"的跨设备同步
> 状态：**定稿**

---

## 1. 背景与动机

当前实现（参考 `src/connect.rs` 与 `listen.rs`）存在以下局限：

| 问题                  | 触发场景           | 后果                       |
| --------------------- | ------------------ | -------------------------- |
| 应用层无 ACK / 重传   | UDP 丢包           | 按键、Enter/Leave 丢失     |
| 2 s idle 即断         | 对端暂停、GC、休眠 | 鼠标卡顿数秒               |
| 大文件 / 图片没有通道 | -                  | 传大文件会导致鼠标卡顿数秒 |

QUIC 对应能力：

- **unreliable datagram**：低延迟不可靠 → 鼠标移动 / 滚轮
- **reliable stream**：有序可靠 → 键盘 / 按键 / 控制信令 / 小文本
- **HTTP/3 streams**：标准请求/响应 → 大文本 / 图片 / 文件字节拉取
- **连接迁移**：4-tuple 变化不掉线 → Wi-Fi ↔ 有线切换
- **内置拥塞控制 / TLS 1.3 / 0-RTT**：弱网友好

关键洞察：**HTTP/3、原生 QUIC 流、QUIC datagram 共享同一条连接**。

---

## 2. 目标架构

```
┌─────────── 一条 quinn::Connection ────────────────┐
│                                                   │
│   ┌─ QUIC datagram（不可靠，最低延迟）────────┐  │
│   │  PointerMotion / Axis / AxisDiscrete120    │  │
│   └────────────────────────────────────────────┘  │
│                                                   │
│   ┌─ QUIC uni/bidi-stream（可靠，有序）───────┐  │
│   │  控制面：Enter / Leave / Ack / Hello      │  │
│   │         Ping / Pong / Bounds / CursorPos  │  │
│   │  输入：KeyboardKey / Modifiers / Button   │  │
│   │  文本剪贴板：Clipboard { content } ≤ 4 KiB│  │
│   │  元数据通知：ClipboardText / Image / Files │  │
│   └────────────────────────────────────────────┘  │
│                                                   │
│   ┌─ HTTP/3 streams（请求 / 响应）─────────────┐  │
│   │  GET  /clipboard/text/{sha256}    大文本  │  │
│   │  GET  /clipboard/image/{sha256}           │  │
│   │  HEAD /clipboard/image/{sha256}           │  │
│   │  GET  /clipboard/file/{sha256}            │  │
│   │  HEAD /clipboard/file/{sha256}            │  │
│   │  GET  /clipboard/file/{sha256}?range=…    │  │
│   │  GET  /healthz                     调试用  │  │
│   └────────────────────────────────────────────┘  │
└───────────────────────────────────────────────────┘
```

防火墙单端口 4252/UDP 不变。

---

## 3. 事件 → 通道映射

> **三 stream 多路设计**（防单流队头阻塞）。所有 stream 在同一条 `quinn::Connection` 上，并复用拥塞控制。
>
> | Stream                 | 方向 | 内容                                                                                                                      | 延迟等级                            |
> | ---------------------- | ---- | ------------------------------------------------------------------------------------------------------------------------- | ----------------------------------- |
> | **A — control**        | bidi | `Hello` / `Enter` / `Leave` / `Ack` / `Ping` / `Pong` / `Bounds` / `CursorPos` / `MotionAbsolute` / `ReceiverSensitivity` | 低频，与字节流解耦                  |
> | **B — input**          | bidi | `KeyboardKey` / `KeyboardModifiers` / `Button`（当对应配置选 Stream 模式时）                                              | 可靠、有序，可能因流控/重传产生延迟 |
> | **C — clipboard meta** | bidi | `Clipboard { content ≤ 1 KiB }` / `ClipboardText` / `ClipboardImage` / `ClipboardFiles`                                   | 元数据，不背字节                    |
>
> 字节（图片 / 大文本 / 文件）**永远**走 HTTP/3 GET。`LanMouseConnection::send()` 按本表分派；A/B/C 在连接建立时各开 1 条，长生命周期复用。

| ProtoEvent                                                  | 通道（默认）               | 通道（per-handle 可配）                     | 理由                                                   |
| ----------------------------------------------------------- | -------------------------- | ------------------------------------------- | ------------------------------------------------------ |
| `Input(PointerEvent::Motion)`                               | QUIC datagram              | （恒定 datagram）                           | 相对增量、丢一帧无感                                   |
| `Input(PointerEvent::Axis)`                                 | QUIC datagram              | （恒定 datagram）                           | 触屏板高频、带 momentum                                |
| `Input(PointerEvent::AxisDiscrete120)`                      | QUIC datagram              | （恒定 datagram）                           | 鼠标滚轮高频                                           |
| `Input(PointerEvent::Button)`                               | **QUIC datagram**          | Stream B（input）当 `mouse_button = Stream` | 鼠标点击默认 Datagram（实时反馈），办公场景可切 Stream |
| `Input(KeyboardEvent::Key)`                                 | **Stream B（input）**      | QUIC datagram 当 `keyboard = Datagram`      | 键盘默认 Stream（不漏字），游戏场景切 Datagram         |
| `Input(KeyboardEvent::Modifiers)`                           | Stream B（input）          | 同上                                        | 修饰键状态必须同步（默认 Stream）                      |
| `Enter` / `Leave` / `Ack`                                   | Stream A（control）        | （恒定）                                    | 控制信令                                               |
| `Hello`                                                     | Stream A（control）        | （恒定）                                    | 握手（QUIC TLS 之外的应用层 magic）                    |
| `Ping` / `Pong`                                             | Stream A（control）        | （恒定）                                    | 低频必到                                               |
| `Bounds` / `CursorPos` / `MotionAbsolute`                   | Stream A（control）        | （恒定）                                    | 建立期                                                 |
| `ReceiverSensitivity`                                       | Stream A（control）        | （恒定）                                    | 建立期                                                 |
| `Clipboard { fingerprint, content }`                        | Stream C（clipboard meta） | （恒定）                                    | **小文本 ≤ 1 KiB** 内联                                |
| **新** `ClipboardText { fingerprint, sha256, size }`        | Stream C（clipboard meta） | （恒定）                                    | **大文本 > 1 KiB** 只发元数据；字节走 HTTP/3           |
| **新** `ClipboardImage { fingerprint, mime, sha256, size }` | Stream C（clipboard meta） | （恒定）                                    | 只发元数据                                             |
| **新** `ClipboardFiles { fingerprint, entries }`            | Stream C（clipboard meta） | （恒定）                                    | 只发元数据                                             |
| HTTP/3 `GET /clipboard/{text,image,file}/{sha256}`          | QUIC stream（请求 / 响应） | （恒定）                                    | 字节永远按需拉取                                       |

> **阈值**：`MAX_CLIPBOARD_SIZE = 1 KiB`，从硬上限改为内联阈值（可配置）。
> **设计原则**：
>
> - 事件体尽量小（几十字节）；字节永远 HTTP/3 GET 拉取
> - 三个 stream 各自有序，**Stream B 与 Stream C 解耦**：剪贴板大文本 / 文件元数据阻塞 C，**不会**阻塞键鼠
> - datagram 优先级最高，token bucket
> - 字段类型：`size` / `Content-Length` 一律 `u64`，不上 `u32`（单文件 > 4 GiB 是现实场景）
> - 术语：本端抓键鼠的为 **Source**（QUIC client / dial），对端接收并模拟的为 **Target**（QUIC server / accept）

---

## 3.1 输入通道模式（per-handle 配置）

> 鼠标点击和键盘按键的实时性需求不同：办公场景下键盘不可漏字（Stream），但鼠标点击需要瞬时反馈（Datagram）；游戏场景下键盘（WASD / 组合键）也需要 Datagram 模式保证低延迟。
>
> 因此 mousehop 把"鼠标按键通道"和"键盘通道"作为**两个独立**的 per-handle 配置项，由用户在 config / GUI 中按对端分别配置。

### 3.1.1 枚举与默认

```rust
// mousehop-ipc/src/lib.rs
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelMode {
    /// 可靠、有序；可能因流控 / 重传产生延迟
    Stream,
    /// 不可靠、最低延迟；可能丢包
    Datagram,
}

/// per-handle 输入通道配置
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputChannelConfig {
    /// 鼠标按键（`PointerEvent::Button`）走哪个通道
    /// **默认 Datagram**：点击瞬时反馈，办公/游戏通用
    pub mouse_button: ChannelMode,
    /// 键盘事件（`KeyboardEvent::Key` / `Modifiers`）走哪个通道
    /// **默认 Stream**：不漏字，办公首选
    pub keyboard: ChannelMode,
}

impl Default for InputChannelConfig {
    fn default() -> Self {
        Self {
            mouse_button: ChannelMode::Datagram,
            keyboard: ChannelMode::Stream,
        }
    }
}
```

### 3.1.2 config.toml 暴露

```toml
[clients.desktop-east]
host = "desktop-east.local"
# 字段可省略（走默认）
input_channels = { mouse_button = "datagram", keyboard = "datagram" }
# ↑ 游戏本：键盘也切到 Datagram 模式

[clients.laptop-north]
host = "laptop-north.local"
# input_channels 省略，使用默认：mouse=datagram, keyboard=stream
# ↑ 办公本：默认配置
```

### 3.1.3 通道路由

```rust
// mousehop/src/quic_transport.rs
impl PeerSession {
    fn route_input(
        &self,
        event: &ProtoEvent,
        cfg: &InputChannelConfig,
    ) -> Channel {
        match event {
            ProtoEvent::Input(InputEvent::Pointer(p)) => match p {
                PointerEvent::Motion { .. }
                | PointerEvent::Axis { .. }
                | PointerEvent::AxisDiscrete120 { .. } => Channel::Datagram, // 恒定
                PointerEvent::Button { .. } => match cfg.mouse_button {
                    ChannelMode::Stream => Channel::StreamB,
                    ChannelMode::Datagram => Channel::Datagram,
                },
            },
            ProtoEvent::Input(InputEvent::Keyboard(_)) => match cfg.keyboard {
                ChannelMode::Stream => Channel::StreamB,
                ChannelMode::Datagram => Channel::Datagram,
            },
            _ => unreachable!("非输入事件不进入 route_input"),
        }
    }
}
```

### 3.1.4 接收端不感知

`InputChannelConfig` 只决定**发送端的通道选择**。对端永远同时监听 datagram + Stream B（如果存在），与自己的 `input_mode` 无关。Sender 选哪条通道，receiver 都接收后转发给 `input-emulation`。

### 3.1.5 模式切换与重连

切换通道配置**不需要**重连 QUIC——`route_input()` 读的是 peer config，hot reload 即生效。但 GUI 切换时给个提示："切换后已发送事件按新模式路由，对端无感"。

### 3.1.6 文档（README / DOC.md）

新增"输入通道模式"章节，明确：

> **Stream 模式** = 不丢操作，但可能因流控 / 重传产生延迟
>
> **Datagram 模式** = 可能丢操作，但最低延迟
>
> **默认配置**：
>
> - 鼠标按键 → **Datagram**（点击瞬时反馈）
> - 键盘按键 → **Stream**（不漏字）
>
> **游戏建议**：把键盘也切到 Datagram 模式。游戏帧率 60Hz 时单帧丢失无感，但 Stream 模式的重传延迟会破坏操作节奏。
