use clap::{Args, Parser, Subcommand};
use futures::StreamExt;

use std::{net::IpAddr, path::PathBuf, time::Duration};
use thiserror::Error;

use lan_mouse_ipc::{
    ClientHandle, ClipboardConfig, ConnectionError, FrontendEvent, FrontendRequest, IpcError,
    Position, connect_async,
};

/// One MiB in bytes. Used by `SetClipboardConfig --max-file-size`
/// to convert the CLI's MiB integer input into the bytes-encoded
/// wire payload that `lan_mouse_ipc::ClipboardConfig::max_file_size`
/// expects (see M4 STEP-4.1 spec; `0` is reserved as the "no
/// limit" sentinel and is preserved verbatim).
const MIB: u64 = 1024 * 1024;

#[derive(Debug, Error)]
pub enum CliError {
    /// is the service running?
    #[error("could not connect: `{0}` - is the service running?")]
    ServiceNotRunning(#[from] ConnectionError),
    #[error("error communicating with service: {0}")]
    Ipc(#[from] IpcError),
}

#[derive(Parser, Clone, Debug, PartialEq, Eq)]
#[command(name = "lan-mouse-cli", about = "LanMouse CLI interface")]
pub struct CliArgs {
    #[command(subcommand)]
    command: CliSubcommand,
}

#[derive(Args, Clone, Debug, PartialEq, Eq)]
struct Client {
    #[arg(long)]
    hostname: Option<String>,
    #[arg(long)]
    port: Option<u16>,
    #[arg(long)]
    ips: Option<Vec<IpAddr>>,
    #[arg(long)]
    enter_hook: Option<String>,
}

/// **M5 STEP-5.5** — arguments for the `SetClipboardConfig`
/// subcommand. Mirrors `lan_mouse_ipc::ClipboardConfig` 1:1;
/// the dispatch helper (`build_clipboard_config`) does the
/// MiB → bytes conversion before the IPC write.
///
/// **Flag semantics**: every boolean field is a `SetTrue`-style
/// flag — presence = `true`, absence = `false`. The full 8-field
/// payload is sent verbatim, so flags the caller omits land on
/// `false` on the daemon side. To preserve an existing
/// `true` value, pass the flag explicitly. This is consistent
/// with how the Vue GUI commits the entire `clipboardConfig`
/// store value on every change (M5 STEP-5.4 commitClipboard
/// pattern).
#[derive(Args, Clone, Debug, PartialEq, Eq)]
#[allow(clippy::too_many_arguments)]
struct SetClipboardConfigArgs {
    /// Master toggle for the entire clipboard sync subsystem.
    /// Pass `--enabled` to enable; omit to set `false`.
    #[arg(long)]
    enabled: bool,
    /// Receive directory for auto-accepted files. Required on
    /// the wire (M4 STEP-4.1: `accept_dir: PathBuf` is
    /// non-optional — auto-accept is the only mode and must
    /// always have a target).
    #[arg(long)]
    accept_dir: PathBuf,
    /// Disable text sync. Presence = `true`.
    #[arg(long)]
    ignore_text: bool,
    /// Disable image sync. Presence = `true`.
    #[arg(long)]
    ignore_images: bool,
    /// Disable file sync. Presence = `true`.
    #[arg(long)]
    ignore_files: bool,
    /// Per-file size ceiling, **in MiB**. CLI converts to bytes
    /// (MiB × 1024 × 1024) before sending. `0` means "no limit"
    /// and is preserved as-is on the wire (sentinel for
    /// `lan_mouse_ipc::ClipboardConfig::max_file_size`).
    #[arg(long)]
    max_file_size: u64,
    /// Keep `.partial` files on disconnect / cancel for
    /// debugging. Presence = `true`. M5 STEP-5.1 deletes the
    /// `.partial` by default when this is `false`.
    #[arg(long)]
    keep_partial: bool,
    /// After files land locally, push the landed paths back
    /// into the local OS clipboard so the user can Cmd+V
    /// directly. Presence = `true`. M4 STEP-4.3 wires the
    /// re-inject; pass `--inject-to-clipboard` to opt in.
    #[arg(long)]
    inject_to_clipboard: bool,
}

#[derive(Clone, Subcommand, Debug, PartialEq, Eq)]
enum CliSubcommand {
    /// add a new client
    AddClient(Client),
    /// remove an existing client
    RemoveClient { id: ClientHandle },
    /// activate a client
    Activate { id: ClientHandle },
    /// deactivate a client
    Deactivate { id: ClientHandle },
    /// list configured clients
    List,
    /// change hostname
    SetHost {
        id: ClientHandle,
        host: Option<String>,
    },
    /// change port
    SetPort { id: ClientHandle, port: u16 },
    /// set position
    SetPosition { id: ClientHandle, pos: Position },
    /// **M3 — set the monitor binding** of a client. Pass an empty
    /// string (`""`) to clear the binding back to "any monitor"
    /// (legacy default). Any other value is treated as a literal
    /// `MonitorInfo.id` and forwarded as `Some(id)` on the wire.
    /// The CLI does not validate against the current monitor list
    /// (that's the GUI's job, via its dropdown sourced from
    /// `MonitorsChanged`).
    SetMonitor { id: ClientHandle, monitor: String },
    /// set ips
    SetIps { id: ClientHandle, ips: Vec<IpAddr> },
    /// **M5 STEP-5.5** — set the daemon-global clipboard config.
    /// Emits `FrontendRequest::SetClipboardConfig` over IPC; the
    /// daemon handler writes the TOML `[clipboard]` section
    /// immediately and echoes `FrontendEvent::ClipboardConfigChanged`
    /// back (M5 STEP-5.3). See `SetClipboardConfigArgs` for the
    /// 8-flag contract and flag semantics.
    SetClipboardConfig(SetClipboardConfigArgs),
    /// **M5 STEP-5.5** — toggle the per-peer
    /// `enable_clipboard_to` flag (M0c IPC field). Emits
    /// `FrontendRequest::SetEnableClipboardTo(handle, bool)`;
    /// the daemon persists to TOML `[[clients]]` and echoes
    /// `FrontendEvent::State`. Pass `true` to push clipboard
    /// to this peer, `false` to disable.
    SetEnableClipboardTo {
        id: ClientHandle,
        /// `true` to push clipboard to this peer, `false` to
        /// disable. clap derive defaults `bool` positional args
        /// to the `SetTrue` action (flag-style); we override
        /// with `Set` so the user passes an explicit `true` /
        /// `false` value (e.g. `set-enable-clipboard-to 0 false`).
        #[arg(action = clap::ArgAction::Set, value_parser = clap::value_parser!(bool))]
        enable: bool,
    },
    /// re-enable capture
    EnableCapture,
    /// re-enable emulation
    EnableEmulation,
    /// authorize a public key
    AuthorizeKey {
        description: String,
        sha256_fingerprint: String,
    },
    /// deauthorize a public key
    RemoveAuthorizedKey { sha256_fingerprint: String },
    /// save configuration to file
    SaveConfig,
}

pub async fn run(args: CliArgs) -> Result<(), CliError> {
    execute(args.command).await?;
    Ok(())
}

/// Pure helper: convert a parsed `SetClipboardConfigArgs` into the
/// `ClipboardConfig` value that gets sent on the wire. Split out of
/// `execute` so unit tests can verify the IPC encoding without
/// opening a socket connection. The MiB → bytes conversion lives
/// here too (clap parses `--max-file-size` as MiB; the IPC struct
/// carries bytes per M4 STEP-4.1).
fn build_clipboard_config(args: &SetClipboardConfigArgs) -> ClipboardConfig {
    ClipboardConfig {
        enabled: args.enabled,
        accept_dir: args.accept_dir.clone(),
        ignore_text: args.ignore_text,
        ignore_images: args.ignore_images,
        ignore_files: args.ignore_files,
        max_file_size: args.max_file_size.saturating_mul(MIB),
        keep_partial: args.keep_partial,
        inject_to_clipboard: args.inject_to_clipboard,
    }
}

async fn execute(cmd: CliSubcommand) -> Result<(), CliError> {
    let (mut rx, mut tx) = connect_async(Some(Duration::from_millis(500))).await?;
    match cmd {
        CliSubcommand::AddClient(Client {
            hostname,
            port,
            ips,
            enter_hook,
        }) => {
            tx.request(FrontendRequest::Create).await?;
            while let Some(e) = rx.next().await {
                if let FrontendEvent::Created(handle, _, _) = e? {
                    if let Some(hostname) = hostname {
                        tx.request(FrontendRequest::UpdateHostname(handle, Some(hostname)))
                            .await?;
                    }
                    if let Some(port) = port {
                        tx.request(FrontendRequest::UpdatePort(handle, port))
                            .await?;
                    }
                    if let Some(ips) = ips {
                        tx.request(FrontendRequest::UpdateFixIps(handle, ips))
                            .await?;
                    }
                    if let Some(enter_hook) = enter_hook {
                        tx.request(FrontendRequest::UpdateEnterHook(handle, Some(enter_hook)))
                            .await?;
                    }
                    break;
                }
            }
        }
        CliSubcommand::RemoveClient { id } => tx.request(FrontendRequest::Delete(id)).await?,
        CliSubcommand::Activate { id } => tx.request(FrontendRequest::Activate(id, true)).await?,
        CliSubcommand::Deactivate { id } => {
            tx.request(FrontendRequest::Activate(id, false)).await?
        }
        CliSubcommand::List => {
            tx.request(FrontendRequest::Enumerate()).await?;
            while let Some(e) = rx.next().await {
                if let FrontendEvent::Enumerate(clients) = e? {
                    for (handle, config, state) in clients {
                        let host = config.hostname.unwrap_or("unknown".to_owned());
                        let port = config.port;
                        let pos = config.pos;
                        let active = state.active;
                        let ips = state.ips;
                        println!(
                            "id {handle}: {host}:{port} ({pos}) active: {active}, ips: {ips:?}"
                        );
                    }
                    break;
                }
            }
        }
        CliSubcommand::SetHost { id, host } => {
            tx.request(FrontendRequest::UpdateHostname(id, host))
                .await?
        }
        CliSubcommand::SetPort { id, port } => {
            tx.request(FrontendRequest::UpdatePort(id, port)).await?
        }
        CliSubcommand::SetPosition { id, pos } => {
            tx.request(FrontendRequest::UpdatePosition(id, pos)).await?
        }
        // **M3**: empty string means "clear the binding" (legacy
        // behavior). Anything else is forwarded verbatim as
        // `Some(id)`; the daemon does no validation against the
        // current monitor list.
        CliSubcommand::SetMonitor { id, monitor } => {
            let monitor = if monitor.is_empty() {
                None
            } else {
                Some(monitor)
            };
            tx.request(FrontendRequest::UpdateMonitor(id, monitor))
                .await?
        }
        CliSubcommand::SetIps { id, ips } => {
            tx.request(FrontendRequest::UpdateFixIps(id, ips)).await?
        }
        // **M5 STEP-5.5**: dispatch the daemon-global clipboard
        // config via the same single-line pattern as `SetMonitor`.
        // The full 8-field payload is sent verbatim; the daemon
        // handler (M4 STEP-4.1) writes TOML and pushes the
        // `ClipboardConfigChanged` echo event (M5 STEP-5.3).
        CliSubcommand::SetClipboardConfig(args) => {
            let cfg = build_clipboard_config(&args);
            tx.request(FrontendRequest::SetClipboardConfig(cfg)).await?
        }
        // **M5 STEP-5.5**: toggle the per-peer
        // `enable_clipboard_to` flag. Same pattern as
        // `Activate` / `Deactivate` — single IPC write, no
        // follow-up reads (the daemon echoes the new client
        // state via the regular `State` broadcast).
        CliSubcommand::SetEnableClipboardTo { id, enable } => {
            tx.request(FrontendRequest::SetEnableClipboardTo(id, enable))
                .await?
        }
        CliSubcommand::EnableCapture => tx.request(FrontendRequest::EnableCapture).await?,
        CliSubcommand::EnableEmulation => tx.request(FrontendRequest::EnableEmulation).await?,
        CliSubcommand::AuthorizeKey {
            description,
            sha256_fingerprint,
        } => {
            tx.request(FrontendRequest::AuthorizeKey(
                description,
                sha256_fingerprint,
            ))
            .await?
        }
        CliSubcommand::RemoveAuthorizedKey { sha256_fingerprint } => {
            tx.request(FrontendRequest::RemoveAuthorizedKey(sha256_fingerprint))
                .await?
        }
        CliSubcommand::SaveConfig => tx.request(FrontendRequest::SaveConfiguration).await?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Unit tests for `M5 STEP-5.5` CLI integration.
    //!
    //! Covers:
    //! - `set-clipboard-config` parses with all 8 flags (the CLI
    //!   form is the kebab-case auto-derivation of the
    //!   `CliSubcommand::SetClipboardConfig` Rust variant — clap
    //!   default `rename_all` rule)
    //! - `build_clipboard_config` encodes the full 8-field
    //!   `ClipboardConfig` payload with correct MiB → bytes
    //!   conversion (`0` preserved as "no limit" sentinel)
    //! - `FrontendRequest::SetClipboardConfig` round-trips via
    //!   `serde_json` (the daemon's wire format)
    //! - drop `auto_accept_files` wire compat: a pre-M4 payload
    //!   carrying the dropped `auto_accept_files` field is still
    //!   accepted by the current IPC layer (serde silently drops
    //!   unknown fields — same contract as the `lan-mouse-ipc`
    //!   crate's own `clipboard_config_drop_auto_accept_files_compat`
    //!   test, re-verified from the CLI's perspective)
    //! - `set-enable-clipboard-to <handle> <bool>` parses + encodes
    //!   as `[handle, bool]` JSON tuple
    use super::*;

    /// Parse an argv-style slice into the `CliSubcommand` variant.
    /// Panics if parsing fails (test-only helper).
    fn parse(argv: &[&str]) -> CliSubcommand {
        CliArgs::parse_from(argv).command
    }

    /// All 8 `--` flags parse correctly and round-trip through the
    /// clap derive. Pin flag names + boolean semantics
    /// (`SetTrue` action — presence = true, absence = false).
    /// The CLI subcommand name is `set-clipboard-config` (kebab-case
    /// auto-derivation of the PascalCase Rust variant).
    #[test]
    fn set_clipboard_config_parses_with_all_eight_flags() {
        let cmd = parse(&[
            "lan-mouse-cli",
            "set-clipboard-config",
            "--accept-dir",
            "/tmp/recv",
            "--max-file-size",
            "100",
            "--enabled",
            "--ignore-text",
            "--ignore-images",
            "--ignore-files",
            "--keep-partial",
            "--inject-to-clipboard",
        ]);
        let CliSubcommand::SetClipboardConfig(args) = cmd else {
            panic!("expected SetClipboardConfig variant");
        };
        assert!(args.enabled);
        assert_eq!(args.accept_dir, PathBuf::from("/tmp/recv"));
        assert!(args.ignore_text);
        assert!(args.ignore_images);
        assert!(args.ignore_files);
        assert_eq!(args.max_file_size, 100, "MiB integer preserved verbatim");
        assert!(args.keep_partial);
        assert!(args.inject_to_clipboard);
    }

    /// Omitted flags default to `false` (clap `SetTrue` behavior).
    /// Documents the "full payload replace" semantics — the user
    /// is responsible for passing every flag they want set to
    /// `true`; absent flags land as `false` on the daemon.
    #[test]
    fn set_clipboard_config_omitted_flags_default_to_false() {
        let cmd = parse(&[
            "lan-mouse-cli",
            "set-clipboard-config",
            "--accept-dir",
            "/tmp/recv",
            "--max-file-size",
            "50",
        ]);
        let CliSubcommand::SetClipboardConfig(args) = cmd else {
            panic!("expected SetClipboardConfig variant");
        };
        assert!(!args.enabled, "no --enabled → enabled = false");
        assert!(!args.ignore_text);
        assert!(!args.ignore_images);
        assert!(!args.ignore_files);
        assert!(!args.keep_partial);
        assert!(!args.inject_to_clipboard);
        assert_eq!(args.max_file_size, 50);
    }

    /// The pure helper encodes all 8 fields correctly and the
    /// resulting `FrontendRequest::SetClipboardConfig` round-trips
    /// through `serde_json`. Also pins the MiB → bytes conversion
    /// (100 MiB → 104857600 bytes) and the `0 = no limit`
    /// sentinel preservation.
    #[test]
    fn build_clipboard_config_encodes_all_eight_fields_with_mib_to_bytes() {
        let args = SetClipboardConfigArgs {
            enabled: true,
            accept_dir: PathBuf::from("/Users/me/Downloads/lan-mouse"),
            ignore_text: false,
            ignore_images: true,
            ignore_files: false,
            max_file_size: 100,
            keep_partial: true,
            inject_to_clipboard: false,
        };
        let cfg = build_clipboard_config(&args);
        // Sanity check the MiB → bytes conversion at the helper
        // boundary.
        assert_eq!(cfg.max_file_size, 100 * MIB);
        assert_eq!(cfg.max_file_size, 104_857_600);

        let req = FrontendRequest::SetClipboardConfig(cfg.clone());
        let s = serde_json::to_string(&req).expect("serialize SetClipboardConfig");

        // Wire shape pins (subset of the JSON keys, sufficient to
        // catch accidental field rename / drop).
        assert!(
            s.contains("\"SetClipboardConfig\""),
            "expected SetClipboardConfig tag, got {s}"
        );
        assert!(s.contains("\"accept_dir\":\"/Users/me/Downloads/lan-mouse\""));
        assert!(s.contains("\"max_file_size\":104857600"));
        assert!(s.contains("\"keep_partial\":true"));
        assert!(s.contains("\"inject_to_clipboard\":false"));
        assert!(s.contains("\"ignore_images\":true"));
        assert!(s.contains("\"enabled\":true"));

        // Round-trip back via the IPC layer's own deserializer.
        let back: FrontendRequest = serde_json::from_str(&s).expect("deserialize back");
        match back {
            FrontendRequest::SetClipboardConfig(c) => assert_eq!(c, cfg),
            other => panic!("expected SetClipboardConfig, got {other:?}"),
        }
    }

    /// `max_file_size = 0` is the wire-level "no limit" sentinel
    /// (see `lan_mouse_ipc::ClipboardConfig::max_file_size` doc).
    /// The CLI must not multiply it into `0` (which is the same
    /// value, so this also pins that `saturating_mul` does not
    /// change semantics) and must not fall back to any default —
    /// the value must reach the daemon verbatim so the daemon's
    /// `collect_files_blocking` / `dispatch_files_decide` can
    /// honor the `0` sentinel (file_meta.rs:68 doc).
    #[test]
    fn build_clipboard_config_preserves_zero_as_no_limit_sentinel() {
        let args = SetClipboardConfigArgs {
            enabled: false,
            accept_dir: PathBuf::from("/tmp/x"),
            ignore_text: false,
            ignore_images: false,
            ignore_files: false,
            max_file_size: 0,
            keep_partial: false,
            inject_to_clipboard: true,
        };
        let cfg = build_clipboard_config(&args);
        assert_eq!(
            cfg.max_file_size, 0,
            "`0` must reach the wire verbatim as the no-limit sentinel"
        );
    }

    /// **drop `auto_accept_files` wire compat** (M4 STEP-4.1):
    /// a payload carrying the pre-M4 `auto_accept_files` field
    /// is silently accepted by the current IPC layer (serde
    /// default behavior: unknown fields are dropped). This pins
    /// the cross-version contract from the CLI's perspective —
    /// a CLI speaking to an older daemon (or vice-versa via a
    /// recorded payload) that still carries the legacy field
    /// must not fail. Mirrors
    /// `lan_mouse_ipc::clipboard_config_tests::clipboard_config_drop_auto_accept_files_compat`.
    #[test]
    fn clipboard_config_drop_auto_accept_files_wire_compat() {
        let payload = r#"{
            "accept_dir": "/tmp/received",
            "auto_accept_files": true,
            "ignore_text": false
        }"#;
        let cfg: ClipboardConfig =
            serde_json::from_str(payload).expect("legacy payload must still deserialize");
        // `auto_accept_files` is silently dropped; missing fields
        // land on M4 defaults (`enabled = true`, `max_file_size =
        // 50 MiB`, `inject_to_clipboard = true`).
        assert!(cfg.enabled, "missing `enabled` defaults to `true`");
        assert_eq!(cfg.accept_dir, PathBuf::from("/tmp/received"));
        assert!(!cfg.ignore_text);
        assert_eq!(
            cfg.max_file_size,
            50 * MIB,
            "missing `max_file_size` defaults to 50 MiB"
        );
        assert!(
            cfg.inject_to_clipboard,
            "missing `inject_to_clipboard` defaults to `true`"
        );
    }

    /// `set-enable-clipboard-to <handle> <bool>` parses the two
    /// positional args and emits the
    /// `FrontendRequest::SetEnableClipboardTo` tuple variant with
    /// the expected JSON wire shape `{"SetEnableClipboardTo":
    /// [handle, bool]}`.
    #[test]
    fn set_enable_clipboard_to_parses_and_round_trips() {
        let cmd = parse(&["lan-mouse-cli", "set-enable-clipboard-to", "7", "false"]);
        match cmd {
            CliSubcommand::SetEnableClipboardTo { id, enable } => {
                assert_eq!(id, 7);
                assert!(!enable);
            }
            other => panic!("expected SetEnableClipboardTo, got {other:?}"),
        }

        let req = FrontendRequest::SetEnableClipboardTo(7, false);
        let s = serde_json::to_string(&req).expect("serialize SetEnableClipboardTo");
        assert_eq!(
            s, "{\"SetEnableClipboardTo\":[7,false]}",
            "tuple variant emits single-key object with positional array"
        );

        let back: FrontendRequest =
            serde_json::from_str(&s).expect("deserialize SetEnableClipboardTo");
        match back {
            FrontendRequest::SetEnableClipboardTo(h, b) => {
                assert_eq!(h, 7);
                assert!(!b);
            }
            other => panic!("expected SetEnableClipboardTo, got {other:?}"),
        }

        // Also verify the `true` branch — keeps the literal
        // `true` / `false` straight in the wire shape.
        let req_true = FrontendRequest::SetEnableClipboardTo(0, true);
        let s_true = serde_json::to_string(&req_true).unwrap();
        assert_eq!(s_true, "{\"SetEnableClipboardTo\":[0,true]}");
    }
}
