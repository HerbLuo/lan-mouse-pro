# Changelog

All notable changes to Lan Mouse are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project aims to adhere to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added (M1: QUIC transport layer)

- **QUIC transport layer replaces DTLS + UDP.** Lan Mouse v4 talks to peers
  over [QUIC](https://datatracker.ietf.org/doc/html/rfc9000) (quinn 0.11 +
  rustls 0.23 with the `ring` crypto provider) instead of the previous
  `webrtc-dtls` + UDP stack. TLS 1.3 is negotiated as part of the QUIC
  handshake, so a separate TCP control channel is no longer required and
  the same UDP port (default `4242`) now carries datagrams, streams and the
  application-protocol Hello. See [`README.md` → Encryption & Transport](./README.md#encryption--transport).
- **Mouse datagram + keyboard / control stream channels.** Mouse motion
  is always sent as a QUIC datagram; keyboard events, mouse buttons and
  control messages (Enter / Leave / Ack) are sent over reliable, ordered
  QUIC streams using a length-prefixed `[u32 BE length][body]` frame
  codec. The per-client `input_channels` schema (see
  [Configuration](./README.md#configuration) in the README) lets each
  client independently choose whether mouse buttons and keyboard events
  go over a stream (reliable, possibly +200 ms of jitter on retransmit)
  or a datagram (lowest possible latency, may drop on packet loss).
- **Client / server mTLS with fingerprint pinning.** Both the server and
  the client now present a self-signed certificate. The server enforces
  mTLS via an explicit `authorized_keys` allowlist of certificate
  fingerprints (`authorized_fingerprints` in `config.toml`); the client
  pins the first server fingerprint it sees to
  `$XDG_DATA_HOME/lan-mouse/known_peers/<fp>.pin` (TOFU) and refuses any
  future swap. The `generate_fingerprint` algorithm (SHA-256 of the DER,
  lower-case hex joined by `:`) is unchanged from v3, so existing
  `authorized_fingerprints` entries continue to be accepted after
  upgrading.

### Removed (M1: DTLS gone)

- The `webrtc-dtls` and `webrtc-util` crates are no longer dependencies
  of the workspace. The previous 8-second application-layer idle timer
  (`RECV_IDLE_TIMEOUT`) is gone — QUIC's own `keep_alive_interval = 5s`
  and `max_idle_timeout = 30s` cover liveness detection instead.

[Unreleased]: https://github.com/feschber/lan-mouse/compare/v3...HEAD