//! QUIC 传输抽象层（占位骨架）。
//!
//! STEP-1.3 仅建骨架：定义 `PeerSession` 与 `Error` 两个顶层符号，便于
//! `lib.rs` 注册 `pub mod quic_transport;` 不引入编译错。后续步骤填实：
//!
//! - STEP-1.4：`endpoint()` —— UDP socket → `quinn::Endpoint`
//! - STEP-2.1：`build_quic_client_config` + ring provider
//! - STEP-2.2 / 2.3：`dial()` / `accept()`
//! - STEP-2.6 / 2.7：`TofuVerifier` / `AuthorizedKeysVerifier`
//! - STEP-3.2：`client_hello` / `server_hello` 握手
//! - STEP-4.4：`route_input()` ChannelMode 分派
//! - STEP-5.x：数据通道（datagram + 3 stream）
//! - STEP-6.x：出入站集成（替换 `LanMouseConnection` / `LanMouseListener`）

use thiserror::Error;

/// QUIC 传输层主入口（占位）。STEP-5.4 起承担端到端 IO。
pub struct PeerSession {
    _private: (),
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("not implemented (STEP-1.3 占位)")]
    NotImplemented,
}

pub type Result<T> = std::result::Result<T, Error>;