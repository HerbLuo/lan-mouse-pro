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
pub mod web;

// Must be called once at startup, before any
// `rustls::ClientConfig::builder` / `rustls::ServerConfig::builder`. `main.rs`
// calls this as the first statement of `fn main()`; integration tests do the
// same in `#[ctor]` or at the top of the test (see
// `quic_transport::install_crypto_provider`).
pub use quic_transport::install_crypto_provider;
