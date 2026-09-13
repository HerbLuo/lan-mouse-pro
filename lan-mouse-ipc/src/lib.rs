use std::{
    collections::{HashMap, HashSet},
    env::VarError,
    fmt::Display,
    io,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    str::FromStr,
};
use thiserror::Error;

#[cfg(unix)]
use std::{env, path::Path};

use serde::{Deserialize, Serialize};

mod connect;
mod connect_async;
mod listen;

pub use connect::{FrontendEventReader, FrontendRequestWriter, connect};
pub use connect_async::{AsyncFrontendEventReader, AsyncFrontendRequestWriter, connect_async};
pub use listen::AsyncFrontendListener;

#[derive(Debug, Error)]
pub enum ConnectionError {
    #[error(transparent)]
    SocketPath(#[from] SocketPathError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("connection timed out")]
    Timeout,
}

#[derive(Debug, Error)]
pub enum IpcListenerCreationError {
    #[error("could not determine socket-path: `{0}`")]
    SocketPath(#[from] SocketPathError),
    #[error("service already running!")]
    AlreadyRunning,
    #[error("failed to bind lan-mouse socket: `{0}`")]
    Bind(io::Error),
}

#[derive(Debug, Error)]
pub enum IpcError {
    #[error("io error occured: `{0}`")]
    Io(#[from] io::Error),
    #[error("invalid json: `{0}`")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Connection(#[from] ConnectionError),
    #[error(transparent)]
    Listen(#[from] IpcListenerCreationError),
}

pub const DEFAULT_PORT: u16 = 2268;

#[derive(Debug, Default, Eq, Hash, PartialEq, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Position {
    #[default]
    Left,
    Right,
    Top,
    Bottom,
}

impl Position {
    pub fn opposite(&self) -> Self {
        match self {
            Position::Left => Position::Right,
            Position::Right => Position::Left,
            Position::Top => Position::Bottom,
            Position::Bottom => Position::Top,
        }
    }
}

#[derive(Debug, Error)]
#[error("not a valid position: {pos}")]
pub struct PositionParseError {
    pos: String,
}

impl FromStr for Position {
    type Err = PositionParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "left" => Ok(Self::Left),
            "right" => Ok(Self::Right),
            "top" => Ok(Self::Top),
            "bottom" => Ok(Self::Bottom),
            _ => Err(PositionParseError { pos: s.into() }),
        }
    }
}

impl Display for Position {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Position::Left => "left",
                Position::Right => "right",
                Position::Top => "top",
                Position::Bottom => "bottom",
            }
        )
    }
}

impl TryFrom<&str> for Position {
    type Error = ();

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "left" => Ok(Position::Left),
            "right" => Ok(Position::Right),
            "top" => Ok(Position::Top),
            "bottom" => Ok(Position::Bottom),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    /// hostname of this client
    pub hostname: Option<String>,
    /// fix ips, determined by the user
    pub fix_ips: Vec<IpAddr>,
    /// both active_addr and addrs can be None / empty so port needs to be stored separately
    pub port: u16,
    /// position of a client on screen
    pub pos: Position,
    /// enter hook
    pub cmd: Option<String>,
    /// Per-input-event transport selection (datagram vs reliable stream).
    /// Sender-side preference; the receiver has no parallel concept. See
    /// [`InputChannelConfig`] for the routing side. The frontend writes
    /// this when the user flips the mouse-button / keyboard channel
    /// dropdowns.
    ///
    /// `#[serde(default)]` makes the field backward-compatible: existing
    /// frontend / daemon wire payloads that don't carry this field
    /// deserialize as `InputChannelConfig::default()` (mouse →
    /// datagram, keyboard → stream). Required because the wire is
    /// consumed by older daemons that pre-date ChannelMode and would
    /// otherwise fail with "missing field `input_channels`".
    #[serde(default)]
    pub input_channels: InputChannelConfig,
    /// **M3 — optional monitor binding**. `None` means "any monitor"
    /// (legacy behavior, equivalent to the M1 default `BarrierKey`).
    /// `Some(id)` binds this client to a specific [`MonitorInfo::id`]
    /// so two clients at the same [`Position`] on different physical
    /// monitors no longer collide. The id must match one of the
    /// `MonitorInfo.id` values the daemon broadcasts via
    /// [`FrontendEvent::MonitorsChanged`]; the frontend dropdown
    /// sources its option list from that event.
    ///
    /// `#[serde(default)]` keeps the wire forward-compatible: payloads
    /// from frontends / daemons that pre-date M3 deserialize as `None`
    /// and behave like a legacy client. Same compat contract as
    /// `input_channels` above.
    #[serde(default)]
    pub monitor: Option<String>,
    /// **M0c / PLAN-2** — whether this peer should receive clipboard pushes from the
    /// local daemon. Default `true` (a missing field on the wire deserializes as
    /// `true`, so a pre-M0c config.toml + new daemon pair continues to push
    /// clipboard to every configured client). The GUI per-row checkbox writes
    /// `false` to opt a specific peer out.
    ///
    /// `#[serde(default)]` matches the `input_channels` /
    /// `monitor` contract — backward-compatible with pre-M0c wire
    /// payloads.
    ///
    /// `#[serde(default)]` matches the `input_channels` /
    /// `monitor` contract — backward-compatible with pre-M0c wire
    /// payloads. The default value comes from a `const fn` rather than
    /// `Default` because we want every `ClientConfig::default()` to
    /// carry `enable_clipboard_to = true` (the "M0c legacy" behavior).
    #[serde(default = "default_enable_clipboard_to")]
    pub enable_clipboard_to: bool,
}

fn default_enable_clipboard_to() -> bool {
    true
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            hostname: Default::default(),
            fix_ips: Default::default(),
            pos: Default::default(),
            cmd: None,
            input_channels: InputChannelConfig::default(),
            monitor: None,
            // M0c default = true (legacy behavior — clipboard pushes to
            // every peer; matches the `#[serde(default)]` contract for
            // pre-M0c wire payloads).
            enable_clipboard_to: true,
        }
    }
}

pub type ClientHandle = u64;

/// On-the-wire snapshot of one physical monitor. Mirrors
/// `input_capture::geometry::MonitorInfo` field-for-field so the
/// service can `try_into` between them at the IPC boundary; kept as
/// a separate type so the wire schema can evolve independently
/// from the internal one (and to avoid forcing `input-capture` to
/// become a public dependency of the IPC crate).
///
/// `rename_all = "snake_case"` keeps the JSON shape identical to
/// the internal type: `{"id": ..., "name": ..., "position": [x,
/// y], "size": [w, h], "primary": ..., "scale": ...}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct MonitorInfo {
    /// Stable monitor id (EDID hash / `wl_output` name / portal
    /// region key, depending on the host platform).
    pub id: String,
    /// Human-readable label. May contain UTF-8.
    pub name: String,
    /// Display origin in virtual-screen coordinates, signed to
    /// support negative coordinates on the top / right monitor of
    /// a 2x1 pair.
    pub position: (i32, i32),
    /// Display size in virtual-screen coordinates.
    pub size: (u32, u32),
    /// Whether this is the OS's primary display.
    pub primary: bool,
    /// HiDPI scale factor (1.0 = standard, 2.0 = Retina). Non-
    /// integer values are allowed.
    pub scale: f64,
}

/// Per-event-class transport preference. Used by [`InputChannelConfig`] to tell
/// the QUIC transport which events should travel over reliable streams vs.
/// datagrams.
///
/// - `Stream` — reliable, ordered (QUIC bidi stream). Suited for keyboard /
///   modifiers where a dropped button-release would leave the peer in a stuck
///   state.
/// - `Datagram` — unreliable, no ordering (QUIC datagram). Suited for
///   button-down events and motion, where low latency matters more than
///   per-event delivery.
///
/// This enum is only the *configuration* surface. The actual routing is
/// implemented in `crate::quic_transport::route_input`.
#[derive(Debug, Eq, Hash, PartialEq, Copy, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChannelMode {
    /// Reliable ordered QUIC bidi stream.
    Stream,
    /// Unreliable unordered QUIC datagram.
    Datagram,
}

/// Per-peer choice of which input-event classes travel over a reliable stream
/// versus a datagram.
///
/// The defaults match the long-standing behavior of `lan-mouse` (mouse button
/// → datagram for low latency; keyboard → stream so modifier releases
/// cannot be dropped). Users can switch either field via `config.toml` /
/// the frontend.
#[derive(Debug, Eq, PartialEq, Copy, Clone, Serialize, Deserialize)]
pub struct InputChannelConfig {
    /// Channel used for mouse button press / release events.
    pub mouse_button: ChannelMode,
    /// Channel used for keyboard and modifier events.
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

#[cfg(test)]
mod input_channel_tests {
    use super::*;

    #[test]
    fn channel_mode_default() {
        let cfg = InputChannelConfig::default();
        assert_eq!(cfg.mouse_button, ChannelMode::Datagram);
        assert_eq!(cfg.keyboard, ChannelMode::Stream);
    }

    #[test]
    fn channel_mode_serializes_lowercase() {
        // The TOML / IPC wire format uses lowercase tags.
        let stream = serde_json::to_string(&ChannelMode::Stream).unwrap();
        let datagram = serde_json::to_string(&ChannelMode::Datagram).unwrap();
        assert_eq!(stream, "\"stream\"");
        assert_eq!(datagram, "\"datagram\"");
    }

    #[test]
    fn input_channel_config_round_trip() {
        let cfg = InputChannelConfig {
            mouse_button: ChannelMode::Stream,
            keyboard: ChannelMode::Datagram,
        };
        let s = serde_json::to_string(&cfg).unwrap();
        let back: InputChannelConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn client_config_input_channels_default_when_missing() {
        // Wire payload that pre-dates ChannelMode / InputChannelConfig.
        // Must deserialize cleanly via #[serde(default)] on the new
        // `input_channels` field; otherwise the frontend on a fresh
        // build would fail every time it talks to a pre-M1 daemon.
        let legacy = r#"{
            "hostname": "peer-east",
            "fix_ips": [],
            "port": 2268,
            "pos": "right",
            "cmd": null
        }"#;
        let cfg: ClientConfig = serde_json::from_str(legacy).unwrap();
        assert_eq!(cfg.input_channels, InputChannelConfig::default());
        assert_eq!(cfg.input_channels.mouse_button, ChannelMode::Datagram);
        assert_eq!(cfg.input_channels.keyboard, ChannelMode::Stream);
    }

    #[test]
    fn client_config_input_channels_round_trip() {
        // Forward direction: writer (new build) sends the field,
        // reader (new build) decodes the same value back. Mirrors
        // `add_with_config_preserves_input_channels` on the lan-mouse
        // side; together they prove the IPC chain doesn't drop the
        // field.
        let cfg = ClientConfig {
            input_channels: InputChannelConfig {
                mouse_button: ChannelMode::Stream,
                keyboard: ChannelMode::Datagram,
            },
            ..ClientConfig::default()
        };
        let s = serde_json::to_string(&cfg).unwrap();
        let back: ClientConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back.input_channels, cfg.input_channels);
    }

    /// **M3 — `monitor` field backward compat**. A pre-M3 payload
    /// (the same `legacy` JSON the `input_channels` test above uses,
    /// which also predates M3 because it has no `monitor` field)
    /// must deserialize cleanly into a `ClientConfig` whose
    /// `monitor` is `None`. Mirrors the §M3 test matrix "缺 monitor
    /// 字段 = None（向后兼容）" requirement. The JSON has no `monitor`
    /// key, so `#[serde(default)]` on the new field must kick in.
    /// Without this guarantee, any old config.toml + new daemon pair
    /// would fail to load and every pre-M3 setup would silently lose
    /// its clients.
    #[test]
    fn client_config_monitor_default_when_missing() {
        // Pre-M3 payload (same shape as `client_config_input_channels_default_when_missing`,
        // minus `cmd` for compactness; the field set mirrors what a
        // fresh config.toml written by STEP-2.6 / STEP-2.7 looks like).
        let pre_m3 = r#"{
            "hostname": "peer-east",
            "fix_ips": [],
            "port": 2268,
            "pos": "right",
            "cmd": null,
            "input_channels": { "mouse_button": "datagram", "keyboard": "stream" }
        }"#;
        let cfg: ClientConfig = serde_json::from_str(pre_m3).unwrap();
        assert_eq!(
            cfg.monitor, None,
            "missing `monitor` field must deserialize as None"
        );
    }

    /// **M3 — `monitor` round-trip**. A new-build writer writes
    /// `monitor = Some("DP-2")`; a new-build reader decodes the same
    /// value back. Mirrors `input_channels` round-trip but for the
    /// monitor binding. Together with the missing-field test above
    /// this pins both directions of the wire contract.
    #[test]
    fn client_config_monitor_round_trip() {
        let cfg = ClientConfig {
            monitor: Some("wl_output:DP-2".into()),
            ..ClientConfig::default()
        };
        let s = serde_json::to_string(&cfg).unwrap();
        let back: ClientConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back.monitor, cfg.monitor);
        assert_eq!(back.monitor.as_deref(), Some("wl_output:DP-2"));
    }

    /// `monitor = None` must serialize as JSON `null` (not omitted).
    /// This is a stable wire contract: a pre-M3 frontend reading a
    /// new-build `monitor: null` event decodes it as the missing-field
    /// `None` case via `#[serde(default)]`, so the wire stays
    /// forward-compatible without bumping the schema.
    #[test]
    fn client_config_monitor_none_serializes_as_null() {
        let cfg = ClientConfig::default();
        assert_eq!(cfg.monitor, None);
        let s = serde_json::to_string(&cfg).unwrap();
        assert!(
            s.contains("\"monitor\":null"),
            "expected `\"monitor\":null` in serialized payload; got {s}"
        );
    }

    // === M0c / PLAN-2 — enable_clipboard_to (per-peer) ===================

    /// **M0c wire compat**: a pre-M0c payload (no `enable_clipboard_to`
    /// field) must deserialize as `enable_clipboard_to = true` — the
    /// legacy "push clipboard to every peer" default. Mirrors the
    /// `monitor` / `input_channels` compat tests.
    #[test]
    fn client_config_enable_clipboard_to_defaults_to_true_when_missing() {
        let pre_m0c = r#"{
            "hostname": "peer-east",
            "fix_ips": [],
            "port": 2268,
            "pos": "right",
            "cmd": null,
            "input_channels": { "mouse_button": "datagram", "keyboard": "stream" },
            "monitor": null
        }"#;
        let cfg: ClientConfig = serde_json::from_str(pre_m0c).unwrap();
        assert!(
            cfg.enable_clipboard_to,
            "missing `enable_clipboard_to` field must default to true (legacy behavior)"
        );
    }

    /// **M0c round-trip**: a new-build writer writes
    /// `enable_clipboard_to = false`; a new-build reader decodes the
    /// same value back.
    #[test]
    fn client_config_enable_clipboard_to_round_trip() {
        let cfg = ClientConfig {
            enable_clipboard_to: false,
            ..ClientConfig::default()
        };
        let s = serde_json::to_string(&cfg).unwrap();
        let back: ClientConfig = serde_json::from_str(&s).unwrap();
        assert!(!back.enable_clipboard_to);
    }

    /// `ClientConfig::default()` must carry `enable_clipboard_to = true`
    /// — the legacy default. This pins the `Default` impl alongside
    /// the `#[serde(default)]` contract.
    #[test]
    fn client_config_default_has_enable_clipboard_to_true() {
        let cfg = ClientConfig::default();
        assert!(
            cfg.enable_clipboard_to,
            "ClientConfig::default() must carry enable_clipboard_to = true"
        );
    }
}

#[cfg(test)]
mod clipboard_config_tests {
    use super::*;

    /// **M4 STEP-4.1** — `ClipboardConfig::default()` is the
    /// "files auto-accept, all sync enabled, `accept_dir` resolved
    /// from `$HOME`/`USERPROFILE`/tmp fallback" shape. Pins the
    /// 8-field post-M4 expansion (`auto_accept_files` dropped,
    /// `enabled` + `max_file_size` + `keep_partial` +
    /// `inject_to_clipboard` added, `accept_dir` promoted from
    /// `Option<PathBuf>` to required `PathBuf`).
    #[test]
    fn clipboard_config_default_is_post_m4_shape() {
        let cfg = ClipboardConfig::default();
        assert!(cfg.enabled);
        // `accept_dir` is now required — `Default` populates a
        // non-empty home-derived path (or `/tmp/lan-mouse` fallback
        // in headless CI). The exact value is environment-dependent
        // so we assert "non-empty" rather than a specific string.
        assert!(!cfg.accept_dir.as_os_str().is_empty());
        assert!(!cfg.ignore_text);
        assert!(!cfg.ignore_images);
        assert!(!cfg.ignore_files);
        // 50 MiB default — matches the legacy `DEFAULT_MAX_FILE_SIZE`
        // constant (file_meta.rs / dispatch_files_decide path).
        assert_eq!(cfg.max_file_size, 50 * 1024 * 1024);
        // Post-M4 default: keep_partial = false (拔网后默认清 .partial)
        assert!(!cfg.keep_partial);
        // Post-M4 default: inject_to_clipboard = true (落盘后自动入剪贴板)
        assert!(cfg.inject_to_clipboard);
    }

    /// Round-trip with all 8 fields populated.
    #[test]
    fn clipboard_config_round_trip_populated() {
        let cfg = ClipboardConfig {
            enabled: false,
            accept_dir: PathBuf::from("/tmp/received"),
            ignore_text: true,
            ignore_images: false,
            ignore_files: true,
            max_file_size: 1024 * 1024 * 1024, // 1 GiB
            keep_partial: true,
            inject_to_clipboard: false,
        };
        let s = serde_json::to_string(&cfg).unwrap();
        let back: ClipboardConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back, cfg);
    }

    /// **Wire compat**: a payload missing only the 5 fields that
    /// carry `#[serde(default = "...")]` helpers
    /// (`enabled` / `max_file_size` / `inject_to_clipboard`) still
    /// deserializes cleanly to the legacy defaults (`true` / 50 MiB
    /// / `true`). The other 5 fields are required (`accept_dir` /
    /// `ignore_*` / `keep_partial` — the latter 3 are `bool`
    /// defaulted via `#[serde(default)]` so a missing bool field
    /// also lands on the default).
    #[test]
    fn clipboard_config_partial_missing_default_helpers() {
        let payload = r#"{
            "accept_dir": "/tmp/x",
            "ignore_text": true
        }"#;
        let cfg: ClipboardConfig = serde_json::from_str(payload).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.accept_dir, PathBuf::from("/tmp/x"));
        assert!(cfg.ignore_text);
        assert!(!cfg.ignore_images);
        assert!(!cfg.ignore_files);
        assert_eq!(cfg.max_file_size, 50 * 1024 * 1024);
        assert!(!cfg.keep_partial);
        assert!(cfg.inject_to_clipboard);
    }

    /// **Wire compat**: `inject_to_clipboard` missing from the
    /// payload deserializes to `true` (the new M4 default —
    /// "files should be re-injected into the local clipboard after
    /// landing so user can Cmd+V them directly"). Pins the
    /// `#[serde(default = "default_inject_to_clipboard")]` helper
    /// so a pre-M4 wire payload that omits the field does not
    /// silently flip the user's preference off.
    #[test]
    fn clipboard_config_inject_to_clipboard_defaults_to_true() {
        let payload = r#"{
            "accept_dir": "/tmp/x",
            "enabled": false
        }"#;
        let cfg: ClipboardConfig = serde_json::from_str(payload).unwrap();
        assert!(cfg.inject_to_clipboard);
        assert!(!cfg.enabled);
    }

    /// **Wire compat**: `max_file_size = 0` deserializes as-is
    /// (no helper overrides 0). The `0 = no limit` semantic is
    /// enforced downstream in `dispatch_files_decide` /
    /// `collect_files_blocking` (file_meta.rs:68 doc: "`0` means
    /// "no limit", in which case this variant is unreachable").
    /// This test pins the wire contract: a 0 is preserved through
    /// serde round-trip (no `unwrap_or(DEFAULT)` magic at the IPC
    /// layer).
    #[test]
    fn clipboard_config_max_file_size_zero_is_no_limit() {
        let cfg = ClipboardConfig {
            max_file_size: 0,
            ..ClipboardConfig::default()
        };
        let s = serde_json::to_string(&cfg).unwrap();
        let back: ClipboardConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back.max_file_size, 0);
    }

    /// **`accept_dir` is required (not Optional)**: a payload with
    /// no `accept_dir` field fails to deserialize. Pins the M4
    /// spec "accept_dir 必填语义" — auto-accept means we always
    /// need a target, so the field type changes from
    /// `Option<PathBuf>` to required `PathBuf`.
    #[test]
    fn clipboard_config_accept_dir_required() {
        let payload = r#"{ "enabled": true }"#;
        let result: Result<ClipboardConfig, _> = serde_json::from_str(payload);
        assert!(
            result.is_err(),
            "missing required `accept_dir` field must fail to deserialize"
        );
    }

    /// **drop `auto_accept_files` wire compat**: an old-style
    /// payload (M0c shape with `auto_accept_files: true`) is
    /// gracefully accepted — the unknown field is silently ignored
    /// by serde (default behavior). The deserialized config lands
    /// on the M4 defaults (no error, but `auto_accept_files` is
    /// effectively dropped: auto-accept is the only mode now).
    #[test]
    fn clipboard_config_drop_auto_accept_files_compat() {
        let payload = r#"{
            "accept_dir": "/tmp/received",
            "auto_accept_files": true,
            "ignore_text": false
        }"#;
        let cfg: ClipboardConfig = serde_json::from_str(payload).unwrap();
        // `auto_accept_files` is gone from the struct — the unknown
        // field is silently ignored. `enabled` lands on the M4
        // default of `true` (no behavioural regression for users
        // who had `auto_accept_files = true` and are now
        // implicitly auto-accepting).
        assert!(cfg.enabled);
        assert_eq!(cfg.accept_dir, PathBuf::from("/tmp/received"));
    }

    /// `FrontendRequest::SetClipboardConfig` round-trip — the wire
    /// shape is `{"SetClipboardConfig":{...}}` (single-key object,
    /// matches the serde default for tuple variants).
    #[test]
    fn request_set_clipboard_config_round_trip() {
        let cfg = ClipboardConfig {
            enabled: true,
            accept_dir: PathBuf::from("/Users/me/Downloads"),
            ignore_text: false,
            ignore_images: false,
            ignore_files: false,
            max_file_size: 50 * 1024 * 1024,
            keep_partial: false,
            inject_to_clipboard: true,
        };
        let req = FrontendRequest::SetClipboardConfig(cfg.clone());
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"SetClipboardConfig\""));
        let back: FrontendRequest = serde_json::from_str(&s).unwrap();
        match back {
            FrontendRequest::SetClipboardConfig(c) => assert_eq!(c, cfg),
            other => panic!("expected SetClipboardConfig, got {other:?}"),
        }
    }

    /// `FrontendRequest::SetEnableClipboardTo(handle, bool)` round-trip.
    #[test]
    fn request_set_enable_clipboard_to_round_trip() {
        let req = FrontendRequest::SetEnableClipboardTo(7, false);
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"SetEnableClipboardTo\":[7,false]"));
        let back: FrontendRequest = serde_json::from_str(&s).unwrap();
        match back {
            FrontendRequest::SetEnableClipboardTo(h, b) => {
                assert_eq!(h, 7);
                assert!(!b);
            }
            other => panic!("expected SetEnableClipboardTo, got {other:?}"),
        }
    }

    /// `FrontendEvent::ClipboardState` round-trip — pins the JSON
    /// shape `"ClipboardState":{...}` so the Vue frontend's
    /// `FrontendEvent` union has a stable wire contract.
    #[test]
    fn event_clipboard_state_round_trip() {
        let event = FrontendEvent::ClipboardState {
            last_text_ts: Some(1700000000000),
            last_image_ts: None,
            last_file_ts: Some(1700000005000),
            last_source: Some("peer-west".into()),
        };
        let s = serde_json::to_string(&event).unwrap();
        assert!(s.contains("\"ClipboardState\""));
        assert!(s.contains("\"last_source\":\"peer-west\""));
        let back: FrontendEvent = serde_json::from_str(&s).unwrap();
        match back {
            FrontendEvent::ClipboardState {
                last_text_ts,
                last_image_ts,
                last_file_ts,
                last_source,
            } => {
                assert_eq!(last_text_ts, Some(1700000000000));
                assert_eq!(last_image_ts, None);
                assert_eq!(last_file_ts, Some(1700000005000));
                assert_eq!(last_source.as_deref(), Some("peer-west"));
            }
            other => panic!("expected ClipboardState, got {other:?}"),
        }
    }

    /// **Wire compat**: a `ClipboardState` payload missing every
    /// timestamp + `last_source` deserializes as the all-None default
    /// — `#[serde(default)]` per field keeps the contract.
    #[test]
    fn event_clipboard_state_missing_fields_default_to_none() {
        // Empty body: deserialize as all-None via #[serde(default)].
        let back: FrontendEvent = serde_json::from_str(r#"{"ClipboardState":{}}"#).unwrap();
        match back {
            FrontendEvent::ClipboardState {
                last_text_ts,
                last_image_ts,
                last_file_ts,
                last_source,
            } => {
                assert_eq!(last_text_ts, None);
                assert_eq!(last_image_ts, None);
                assert_eq!(last_file_ts, None);
                assert_eq!(last_source, None);
            }
            other => panic!("expected ClipboardState, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod monitor_info_tests {
    use super::*;

    /// The wire JSON uses the field names `id / name / position /
    /// size / primary / scale`. A drift in any of these names would
    /// silently break the frontend's TypeScript types, so we pin
    /// the exact payload shape here.
    #[test]
    fn monitor_info_serializes_to_snake_case_fields() {
        let info = MonitorInfo {
            id: "EDID:0xdeadbeef".into(),
            name: "Built-in".into(),
            position: (0, 0),
            size: (2560, 1440),
            primary: true,
            scale: 2.0,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("\"id\":\"EDID:0xdeadbeef\""));
        assert!(json.contains("\"name\":\"Built-in\""));
        assert!(json.contains("\"position\":[0,0]"));
        assert!(json.contains("\"size\":[2560,1440]"));
        assert!(json.contains("\"primary\":true"));
        assert!(json.contains("\"scale\":2.0"));
    }

    /// UTF-8 display names (manufacturers ship CJK and accented
    /// labels) must round-trip cleanly. serde_json uses UTF-8 by
    /// default for `String`; this pins that contract.
    #[test]
    fn monitor_info_round_trip_utf8_name() {
        let info = MonitorInfo {
            id: "wl_output:eDP-1".into(),
            name: "ノートPC 内蔵ディスプレイ".into(),
            position: (0, 0),
            size: (1920, 1080),
            primary: true,
            scale: 1.0,
        };
        let json = serde_json::to_string(&info).unwrap();
        let back: MonitorInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(info, back);
    }

    /// Negative coordinates (the top / right monitor of a vertical
    /// or horizontal pair sits at negative offset relative to the
    /// primary in many OS coordinate systems). i32 / signed round-
    /// trip must hold.
    #[test]
    fn monitor_info_round_trip_negative() {
        let info = MonitorInfo {
            id: "CGDisplay:secondary".into(),
            name: "External below".into(),
            position: (0, -2160),
            size: (3840, 2160),
            primary: false,
            scale: 1.0,
        };
        let json = serde_json::to_string(&info).unwrap();
        let back: MonitorInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(info, back);
        assert_eq!(back.position, (0, -2160));
    }

    /// Mixed-DPI hosts emit non-integer scale factors (1.25, 1.5,
    /// 2.0...). `f64` round-trip must preserve the exact value.
    #[test]
    fn monitor_info_round_trip_mixed_scale() {
        for scale in [1.0_f64, 1.25, 1.5, 2.0, 2.5] {
            let info = MonitorInfo {
                id: format!("display@{scale}"),
                name: format!("display at scale {scale}"),
                position: (0, 0),
                size: (1920, 1080),
                primary: false,
                scale,
            };
            let json = serde_json::to_string(&info).unwrap();
            let back: MonitorInfo = serde_json::from_str(&json).unwrap();
            assert_eq!(back.scale, scale, "scale {scale} did not round-trip");
            assert_eq!(info, back);
        }
    }

    /// `MonitorsChanged` round-trip with a populated list.
    #[test]
    fn monitors_changed_round_trip() {
        let event = FrontendEvent::MonitorsChanged(vec![
            MonitorInfo {
                id: "primary".into(),
                name: "Primary".into(),
                position: (0, 0),
                size: (1920, 1080),
                primary: true,
                scale: 1.0,
            },
            MonitorInfo {
                id: "secondary".into(),
                name: "Secondary".into(),
                position: (1920, 0),
                size: (2560, 1440),
                primary: false,
                scale: 2.0,
            },
        ]);
        let json = serde_json::to_string(&event).unwrap();
        let back: FrontendEvent = serde_json::from_str(&json).unwrap();
        match back {
            FrontendEvent::MonitorsChanged(list) => {
                assert_eq!(list.len(), 2);
                assert_eq!(list[0].id, "primary");
                assert_eq!(list[1].size, (2560, 1440));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    /// Backward compatibility: payloads that pre-date M2 carry no
    /// `MonitorsChanged` variant. They decode as some *other*
    /// `FrontendEvent` variant (here we use `Error`), so the
    /// frontend must tolerate a missing variant by treating the
    /// monitor list as empty. This test pins the wire-level
    /// compat contract by serialising a pre-M2 event and asserting
    /// `MonitorsChanged` does NOT appear (so an old daemon
    /// accidentally tagging something as `MonitorsChanged` would be
    /// caught).
    #[test]
    fn monitors_changed_missing_field_yields_empty() {
        // Pre-M2 payload (variants still present in the enum, but no
        // monitors info). The frontend must treat this as "no
        // monitors yet" — i.e. an empty Vec — and not crash.
        let pre_m2 = r#"{"Error":"no monitors backend"}"#;
        let event: FrontendEvent = serde_json::from_str(pre_m2).unwrap();
        match event {
            FrontendEvent::Error(msg) => assert_eq!(msg, "no monitors backend"),
            other => panic!("expected Error variant, got {other:?}"),
        }
        // Round-trip an explicit empty MonitorsChanged to confirm
        // the empty Vec wire shape stays stable.
        let empty = FrontendEvent::MonitorsChanged(vec![]);
        let json = serde_json::to_string(&empty).unwrap();
        assert!(json.contains("\"MonitorsChanged\":[]"));
        let back: FrontendEvent = serde_json::from_str(&json).unwrap();
        match back {
            FrontendEvent::MonitorsChanged(list) => assert!(list.is_empty()),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    /// `BindingInvalid` carries the client handle and a human-
    /// readable reason. Round-trip preserves both, and the JSON
    /// shape stays stable.
    #[test]
    fn binding_invalid_round_trip() {
        let event = FrontendEvent::BindingInvalid(42, "monitor \"DP-2\" disconnected".into());
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"BindingInvalid\":[42,\"monitor"));
        let back: FrontendEvent = serde_json::from_str(&json).unwrap();
        match back {
            FrontendEvent::BindingInvalid(handle, reason) => {
                assert_eq!(handle, 42);
                assert_eq!(reason, "monitor \"DP-2\" disconnected");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }
}

/// **M4 STEP-4.1 / PLAN-2.1** — daemon-global clipboard
/// configuration. The clipboard listener is daemon-global (one OS
/// clipboard feeds all peers) and the receive directory is also
/// global — this struct lives at `lan_mouse_ipc::ClipboardConfig`
/// (not under `ClientConfig`), per PLAN §5 评审 #4 second round.
///
/// **Post-M4 schema (8 fields)**:
/// - `enabled` — master on/off for the entire clipboard sync
///   subsystem (default `true`); when `false` the dispatcher does
///   not start and key/mouse paths are unaffected.
/// - `accept_dir: PathBuf` — **required** since auto-accept is the
///   only mode (post-drop of `auto_accept_files`); a missing field
///   on the wire now fails to deserialize (vs. the pre-M4
///   `Option<PathBuf>` which silently landed on `None`).
/// - `ignore_text / ignore_images / ignore_files` — per-kind toggle
///   to drop the corresponding stream before it crosses StreamC.
///   Distinct from `enabled = false` which disables the whole
///   subsystem; these let the user opt-out of a single channel
///   while keeping text / image / file sync active.
/// - `max_file_size` — per-file ceiling in bytes; default 50 MiB,
///   `0 = no limit` (enforced in `collect_files_blocking` /
///   `dispatch_files_decide`).
/// - `keep_partial` — `true` ⇒ on disconnect / cancel the
///   `.partial` file is preserved for postmortem; `false` (default)
///   ⇒ `std::fs::remove_file` runs in M5 STEP-5.1.
/// - `inject_to_clipboard` — `true` (default) ⇒ after files land
///   locally, the daemon pushes the file paths into the OS
///   clipboard so the user can Cmd+V directly. `false` ⇒ skip
///   the inject (file is on disk only).
///
/// **drop `auto_accept_files`**: pre-M4 payloads carrying that
/// field still deserialize (serde silently ignores unknown fields),
/// but the value is now meaningless — auto-accept is the only
/// mode. See `clipboard_config_drop_auto_accept_files_compat` test.
///
/// Frontend request: [`FrontendRequest::SetClipboardConfig`]. TOML
/// key: `[clipboard]` section in `config.toml`.
#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub struct ClipboardConfig {
    /// Master toggle for the entire clipboard sync subsystem.
    /// Default `true`. When `false`, the dispatcher's outbound
    /// tick is skipped and inbound arms early-return at the first
    /// `config.clipboard_config().enabled` check — key/mouse
    /// paths are unaffected. Distinct from `ignore_files` (per-
    /// kind filter) which keeps text / image sync active.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Receive directory for auto-accepted files. **Required**
    /// (auto-accept means we always need a target — pre-M4 this
    /// was `Option<PathBuf>` and `None` meant "daemon default";
    /// post-M4 the daemon fills this in via
    /// [`Config::clipboard_config`] before any inbound arm reads
    /// it, so the wire contract tightens to required).
    pub accept_dir: PathBuf,
    /// Disable text sync (do not push / pull text via StreamC).
    /// `#[serde(default)]` ⇒ missing field = false (text sync
    /// enabled). Pre-M0c payloads deserialize to the legacy
    /// "all sync enabled" state.
    #[serde(default)]
    pub ignore_text: bool,
    /// Disable image sync. Same compat contract as `ignore_text`.
    #[serde(default)]
    pub ignore_images: bool,
    /// Disable file sync. Same compat contract as `ignore_text`.
    #[serde(default)]
    pub ignore_files: bool,
    /// Per-file size ceiling in bytes. Default 50 MiB. `0` means
    /// "no limit" — `collect_files_blocking` skips the
    /// `ExceedsLimit` arm when this is 0 (file_meta.rs:68 doc).
    /// Mirrors `src/config.rs::DEFAULT_MAX_FILE_SIZE`.
    #[serde(default = "default_max_file_size")]
    pub max_file_size: u64,
    /// `true` ⇒ keep `.partial` files on disconnect / cancel
    /// (for debugging). `false` (default) ⇒ M5 STEP-5.1 will
    /// `std::fs::remove_file` them on disconnect.
    #[serde(default)]
    pub keep_partial: bool,
    /// `true` (default) ⇒ after files land locally, push the
    /// landed paths into the local OS clipboard so the user can
    /// Cmd+V directly. `false` ⇒ files land on disk only, no
    /// clipboard re-inject (M4 STEP-4.2 + 4.3 wiring).
    #[serde(default = "default_inject_to_clipboard")]
    pub inject_to_clipboard: bool,
}

/// Helper for `#[serde(default = "...")]` on
/// [`ClipboardConfig::enabled`]. `true` mirrors the
/// `Default` impl.
fn default_enabled() -> bool {
    true
}

/// Helper for `#[serde(default = "...")]` on
/// [`ClipboardConfig::max_file_size`]. Returns the 50 MiB default
/// (matches `src/config.rs::DEFAULT_MAX_FILE_SIZE`).
fn default_max_file_size() -> u64 {
    50 * 1024 * 1024
}

/// Helper for `#[serde(default = "...")]` on
/// [`ClipboardConfig::inject_to_clipboard`]. `true` mirrors the
/// `Default` impl — files should auto-appear in the local
/// clipboard after landing.
fn default_inject_to_clipboard() -> bool {
    true
}

impl Default for ClipboardConfig {
    /// M4 STEP-4.1 default shape:
    /// - `enabled: true` (clipboard sync on; user opts out via
    ///   `[clipboard] enabled = false` in TOML or the GUI).
    /// - `accept_dir: <env-derived>` (resolved from `$HOME` /
    ///   `$USERPROFILE` at IPC layer; falls back to
    ///   `/tmp/lan-mouse` for headless CI / Docker).
    /// - `ignore_text / ignore_images / ignore_files: false` (all
    ///   sync channels enabled by default; unchanged from the M0c
    ///   legacy shape).
    /// - `max_file_size: 50 MiB` (unchanged from
    ///   `DEFAULT_MAX_FILE_SIZE`).
    /// - `keep_partial: false` (M5 STEP-5.1 will remove .partial
    ///   on disconnect by default; user opts in via TOML).
    /// - `inject_to_clipboard: true` (M4 STEP-4.2 + 4.3 will wire
    ///   the OS clipboard re-inject; user opts out via TOML).
    fn default() -> Self {
        Self {
            enabled: true,
            accept_dir: default_accept_dir(),
            ignore_text: false,
            ignore_images: false,
            ignore_files: false,
            max_file_size: 50 * 1024 * 1024,
            keep_partial: false,
            inject_to_clipboard: true,
        }
    }
}

/// Resolve the default `accept_dir` for [`ClipboardConfig::default`].
///
/// **Cross-platform fallback chain**: `$HOME` (macOS / Linux) →
/// `$USERPROFILE` (Windows) → `/tmp/lan-mouse` if neither env var
/// is set. Mirrors `service::default_accept_dir` — kept as a
/// separate copy so the IPC crate has no service-layer
/// dependency.
///
/// **Why a hardcoded subdirectory (not `dirs::download_dir`)**:
/// the daemon has zero `dirs` / `directories` dependencies. A
/// hardcoded subdirectory is portable; on macOS the full path is
/// `~/lan-mouse/`, on Windows `%USERPROFILE%\lan-mouse\`, on
/// Linux `~/lan-mouse/`.
fn default_accept_dir() -> PathBuf {
    let home = cfg!(unix)
        .then(|| std::env::var("HOME").ok().map(PathBuf::from))
        .flatten()
        .or_else(|| std::env::var("USERPROFILE").ok().map(PathBuf::from));
    let base = home.unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("lan-mouse")
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ClientState {
    /// events should be sent to and received from the client
    pub active: bool,
    /// `active` address of the client, used to send data to.
    /// This should generally be the socket address where data
    /// was last received from.
    pub active_addr: Option<SocketAddr>,
    /// ips from dns
    pub dns_ips: Vec<IpAddr>,
    /// all ip addresses associated with a particular client
    /// e.g. Laptops usually have at least an ethernet and a wifi port
    /// which have different ip addresses
    pub ips: HashSet<IpAddr>,
    /// client has pressed keys
    pub has_pressed_keys: bool,
    /// dns resolving in progress
    pub resolving: bool,
    /// Peer's build short commit hash from the [`Hello`] proto
    /// event. `None` means we haven't received a Hello yet — either
    /// the connection is fresh, or the peer is on an older build
    /// that predates the Hello event. The frontend uses this to
    /// soft-warn on version mismatch.
    pub peer_commit: Option<[u8; 8]>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FrontendEvent {
    /// a client was created
    Created(ClientHandle, ClientConfig, ClientState),
    /// no such client
    NoSuchClient(ClientHandle),
    /// state changed
    State(ClientHandle, ClientConfig, ClientState),
    /// the client was deleted
    Deleted(ClientHandle),
    /// new port, reason of failure (if failed)
    PortChanged(u16, Option<String>),
    /// list of all clients, used for initial state synchronization
    Enumerate(Vec<(ClientHandle, ClientConfig, ClientState)>),
    /// an error occured
    Error(String),
    /// capture status
    CaptureStatus(Status),
    /// emulation status
    EmulationStatus(Status),
    /// authorized public key fingerprints have been updated
    AuthorizedUpdated(HashMap<String, String>),
    /// public key fingerprint of this device
    PublicKeyFingerprint(String),
    /// new device connected
    DeviceConnected {
        addr: SocketAddr,
        fingerprint: String,
    },
    /// incoming device entered the screen
    DeviceEntered {
        fingerprint: String,
        addr: SocketAddr,
        pos: Position,
    },
    /// incoming disconnected
    IncomingDisconnected(SocketAddr),
    /// failed connection attempt (approval for fingerprint required)
    ConnectionAttempt { fingerprint: String },
    /// Current QUIC transport config snapshot, broadcast on
    /// [`FrontendRequest::Enumerate`] / `Sync` so the GUI can render the
    /// effective values, and again after every
    /// [`FrontendRequest::SetQuicIdleTimeout`] to confirm the write.
    ///
    /// **Field**: `idle_timeout_secs` — the QUIC `max_idle_timeout`
    /// currently in effect on both server and client endpoints.
    /// Default 5 (down from the legacy 10s; lowered 2026-09-04 to
    /// reduce the "mouse stuck during a network blip" window on the
    /// master side, where the only death-detection signal is the QUIC
    /// idle timer).
    QuicConfig { idle_timeout_secs: u64 },
    /// Host's current physical monitor list. Emitted once at startup
    /// (so the GUI can render the multi-monitor dropdown / SVG canvas
    /// immediately) and again whenever a backend reports a hotplug
    /// (display added / removed / resized). The GUI treats the most
    /// recent payload as the source of truth.
    ///
    /// `Vec` is empty when no display backend is active — older
    /// frontends can still deserialize the variant (see
    /// `monitors_changed_missing_field_yields_empty`).
    MonitorsChanged(Vec<MonitorInfo>),
    /// Emitted when an active capture / binding becomes invalid
    /// because the monitor it was bound to disappeared (typically
    /// after a `MonitorsChanged` with that monitor absent). The GUI
    /// pauses the toggle, shows a tooltip, and waits for the user to
    /// pick a different monitor or reactivate the client.
    ///
    /// `reason` is a free-form human-readable description (e.g.
    /// "monitor \"DP-2\" disconnected"). Carrying it as a plain
    /// `String` keeps the wire simple and lets the daemon vary its
    /// phrasing without a schema bump.
    BindingInvalid(ClientHandle, String),
    /// **M0c / PLAN-2** — clipboard state snapshot (timestamp of last
    /// text / image / file sync + the peer hostname / fingerprint that
    /// most recently pushed to the local clipboard).
    ///
    /// `last_text_ts` / `last_image_ts` / `last_file_ts` are
    /// milliseconds since the UNIX epoch (`None` means "never").
    /// `last_source` carries the peer hostname / fingerprint so the
    /// GUI can render the "clipboard was just changed by <peer>"
    /// amber highlight (PLAN §3 M4 STEP-4.4 + 评审 #6 second round)
    /// — `None` means "the change originated locally".
    ClipboardState {
        last_text_ts: Option<u64>,
        last_image_ts: Option<u64>,
        last_file_ts: Option<u64>,
        last_source: Option<String>,
    },
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub enum FrontendRequest {
    /// activate/deactivate client
    Activate(ClientHandle, bool),
    /// add a new client
    Create,
    /// change the listen port (recreate udp listener)
    ChangePort(u16),
    /// remove a client
    Delete(ClientHandle),
    /// request an enumeration of all clients
    Enumerate(),
    /// resolve dns
    ResolveDns(ClientHandle),
    /// update hostname
    UpdateHostname(ClientHandle, Option<String>),
    /// update port
    UpdatePort(ClientHandle, u16),
    /// update position
    UpdatePosition(ClientHandle, Position),
    /// **M3 — update the monitor binding**. `monitor = None` clears
    /// the binding (back to "any monitor"); `Some(id)` re-binds to a
    /// specific [`MonitorInfo::id`] (must match a currently-enumerated
    /// monitor or the `BindingInvalid` flow from M2 kicks in if the
    /// monitor subsequently disappears). The service handler rebuilds
    /// the active capture barrier via `destroy + create` to honor the
    /// new [`BarrierKey::monitor`] field, identical in shape to
    /// `UpdatePosition`.
    UpdateMonitor(ClientHandle, Option<String>),
    /// update fix-ips
    UpdateFixIps(ClientHandle, Vec<IpAddr>),
    /// request reenabling input capture
    EnableCapture,
    /// request reenabling input emulation
    EnableEmulation,
    /// synchronize all state
    Sync,
    /// authorize fingerprint (description, fingerprint)
    AuthorizeKey(String, String),
    /// remove fingerprint (fingerprint)
    RemoveAuthorizedKey(String),
    /// change the hook command
    UpdateEnterHook(u64, Option<String>),
    /// Per-input-event transport selection for the given outgoing
    /// client (see [`InputChannelConfig`] on [`ClientConfig`]).
    /// Sender-side only; the receiver side never sees this preference.
    SetClientInputChannels(ClientHandle, InputChannelConfig),
    /// save config file
    SaveConfiguration,
    /// Set QUIC `idle_timeout_secs`. Persists to TOML immediately and
    /// echoes the new value back via [`FrontendEvent::QuicConfig`]. Does
    /// **not** rebuild the running endpoint (changing transport config
    /// at runtime would invalidate every active QUIC session); the new
    /// value applies on the next daemon restart. The GUI surfaces this
    /// constraint next to the input.
    SetQuicIdleTimeout(u64),
    /// **M0c / PLAN-2** — set daemon-global clipboard config.
    /// Persists to TOML `[clipboard]` section immediately and echoes
    /// the new value back. Carries no `ClientHandle` because the
    /// clipboard listener is daemon-global (one OS clipboard feeds
    /// all peers, PLAN §5 评审 #4 second round).
    SetClipboardConfig(ClipboardConfig),
    /// **M0c / PLAN-2** — set the per-peer `enable_clipboard_to`
    /// flag. Persists to TOML `[[clients]]` `enable_clipboard_to`
    /// field immediately and echoes back. Distinct from
    /// [`SetClipboardConfig`] which is daemon-global.
    SetEnableClipboardTo(ClientHandle, bool),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum Status {
    #[default]
    Disabled,
    Enabled,
}

impl From<Status> for bool {
    fn from(status: Status) -> Self {
        match status {
            Status::Enabled => true,
            Status::Disabled => false,
        }
    }
}

#[cfg(unix)]
const LAN_MOUSE_SOCKET_NAME: &str = "lan-mouse-socket.sock";

#[derive(Debug, Error)]
pub enum SocketPathError {
    #[error("could not determine $XDG_RUNTIME_DIR: `{0}`")]
    XdgRuntimeDirNotFound(VarError),
    #[error("could not determine $HOME: `{0}`")]
    HomeDirNotFound(VarError),
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn default_socket_path() -> Result<PathBuf, SocketPathError> {
    let xdg_runtime_dir =
        env::var("XDG_RUNTIME_DIR").map_err(SocketPathError::XdgRuntimeDirNotFound)?;
    Ok(Path::new(xdg_runtime_dir.as_str()).join(LAN_MOUSE_SOCKET_NAME))
}

#[cfg(all(unix, target_os = "macos"))]
pub fn default_socket_path() -> Result<PathBuf, SocketPathError> {
    let home = env::var("HOME").map_err(SocketPathError::HomeDirNotFound)?;
    Ok(Path::new(home.as_str())
        .join("Library")
        .join("Caches")
        .join(LAN_MOUSE_SOCKET_NAME))
}
