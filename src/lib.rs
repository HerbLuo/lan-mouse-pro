mod capture;
pub mod capture_test;
pub mod client;
pub mod config;
mod connect;
mod crypto;
mod dns;
mod emulation;
pub mod emulation_test;
mod listen;
#[cfg(target_os = "macos")]
pub(crate) mod macos_power;
pub mod quic_transport;
pub mod service;

// 启动期必须先调：早于任何 `rustls::ClientConfig::builder` /
// `rustls::ServerConfig::builder`（见 STEP-2.1 / next/STEP-2.1.md）。
// `main.rs` 在 `fn main()` 顶部第一句调用；集成测试也在 `#[ctor]`
// 或测试首句调用（详见 quic_transport::install_crypto_provider 文档）。
pub use quic_transport::install_crypto_provider;
