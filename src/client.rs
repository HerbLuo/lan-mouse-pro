use std::{
    cell::RefCell,
    collections::HashSet,
    net::{IpAddr, SocketAddr},
    rc::Rc,
};

use slab::Slab;

use input_capture::BarrierKey;
use lan_mouse_ipc::{ClientConfig, ClientHandle, ClientState, InputChannelConfig, Position};

use crate::capture::{to_capture_pos, to_ipc_pos};
use crate::config::ConfigClient;

#[derive(Clone, Default)]
pub struct ClientManager {
    clients: Rc<RefCell<Slab<(ClientConfig, ClientState)>>>,
}

impl ClientManager {
    /// get all clients
    pub fn clients(&self) -> Vec<(ClientConfig, ClientState)> {
        self.clients
            .borrow()
            .iter()
            .map(|(_, c)| c.clone())
            .collect::<Vec<_>>()
    }

    pub fn add_with_config(&self, config_client: ConfigClient) -> ClientHandle {
        let config = ClientConfig {
            hostname: config_client.hostname,
            fix_ips: config_client.ips.into_iter().collect(),
            port: config_client.port,
            pos: config_client.pos,
            cmd: config_client.enter_hook,
            // Forward the per-handle input-channel selection from
            // `ConfigClient` to `ClientConfig` so the value reaches
            // the frontend editor and runtime.
            input_channels: config_client.input_channels,
            // **M3**: forward the per-handle monitor binding. Default
            // `None` for legacy clients; `Some(id)` for clients that
            // the user has bound to a specific `MonitorInfo.id`.
            monitor: config_client.monitor,
            // **M0c / PLAN-2**: forward the per-peer clipboard
            // opt-in flag. Default `true` for legacy clients
            // (matches the `#[serde(default)]` wire compat contract).
            enable_clipboard_to: config_client.enable_clipboard_to,
        };
        let state = ClientState {
            active: config_client.active,
            ips: HashSet::from_iter(config.fix_ips.iter().cloned()),
            ..Default::default()
        };
        let handle = self.add_client();
        self.set_config(handle, config);
        self.set_state(handle, state);
        handle
    }

    /// add a new client to this manager
    pub fn add_client(&self) -> ClientHandle {
        self.clients.borrow_mut().insert(Default::default()) as ClientHandle
    }

    /// set the config of the given client
    pub fn set_config(&self, handle: ClientHandle, config: ClientConfig) {
        if let Some((c, _)) = self.clients.borrow_mut().get_mut(handle as usize) {
            *c = config;
        }
    }

    /// set the state of the given client
    pub fn set_state(&self, handle: ClientHandle, state: ClientState) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            *s = state;
        }
    }

    /// activate the given client
    /// returns, whether the client was activated
    pub fn activate_client(&self, handle: ClientHandle) -> bool {
        let mut clients = self.clients.borrow_mut();
        match clients.get_mut(handle as usize) {
            Some((_, s)) if !s.active => {
                s.active = true;
                true
            }
            _ => false,
        }
    }

    /// deactivate the given client
    /// returns, whether the client was deactivated
    pub fn deactivate_client(&self, handle: ClientHandle) -> bool {
        let mut clients = self.clients.borrow_mut();
        match clients.get_mut(handle as usize) {
            Some((_, s)) if s.active => {
                s.active = false;
                true
            }
            _ => false,
        }
    }

    /// find a client by its address
    pub fn get_client(&self, addr: SocketAddr) -> Option<ClientHandle> {
        // since there shouldn't be more than a handful of clients at any given
        // time this is likely faster than using a HashMap
        self.clients
            .borrow()
            .iter()
            .find_map(|(k, (_, s))| {
                if s.active && s.ips.contains(&addr.ip()) {
                    Some(k)
                } else {
                    None
                }
            })
            .map(|p| p as ClientHandle)
    }

    /// get the client at the given [`BarrierKey`].
    ///
    /// M1: compared `c.pos` against `key.pos` after converting through
    /// the IPC position boundary (see [`to_ipc_pos`]). **M3**: now
    /// also compares `c.monitor` against `key.monitor` so two clients
    /// at the same position on *different* physical monitors don't
    /// collide and deactivate each other when `activate_client` looks
    /// up the existing occupant. `offset / span` are still defaulted
    /// on both sides of the comparison — they will widen in M4 when
    /// sub-edge barriers ship.
    pub fn client_at(&self, key: &BarrierKey) -> Option<ClientHandle> {
        let pos = to_ipc_pos(key.pos);
        let monitor = key.monitor.as_deref();
        self.clients
            .borrow()
            .iter()
            .find_map(|(k, (c, s))| {
                if s.active && c.pos == pos && c.monitor.as_deref() == monitor {
                    Some(k)
                } else {
                    None
                }
            })
            .map(|p| p as ClientHandle)
    }

    pub(crate) fn get_hostname(&self, handle: ClientHandle) -> Option<String> {
        self.clients
            .borrow_mut()
            .get_mut(handle as usize)
            .and_then(|(c, _)| c.hostname.clone())
    }

    /// **STEP-1.3 → STEP-M3-3.1**: get the [`BarrierKey`] for `handle`.
    /// M1 used to build `BarrierKey::from_pos(c.pos)` with default
    /// `monitor / offset / span`. **M3** widens this to include the
    /// per-handle `monitor` binding from `ClientConfig`, so the
    /// capture barrier is scoped to a specific physical monitor when
    /// the user has chosen one. `offset / span` stay at their legacy
    /// defaults (`0` / `10000`) — sub-edge barriers land in M4.
    pub(crate) fn get_key(&self, handle: ClientHandle) -> Option<BarrierKey> {
        self.clients
            .borrow()
            .get(handle as usize)
            .map(|(c, _)| BarrierKey {
                pos: to_capture_pos(c.pos),
                monitor: c.monitor.clone(),
                offset: 0,
                span: 10000,
            })
    }

    /// remove a client from the list
    pub fn remove_client(&self, client: ClientHandle) -> Option<(ClientConfig, ClientState)> {
        // remove id from occupied ids
        self.clients.borrow_mut().try_remove(client as usize)
    }

    /// get the config & state of the given client
    pub fn get_state(&self, handle: ClientHandle) -> Option<(ClientConfig, ClientState)> {
        self.clients.borrow().get(handle as usize).cloned()
    }

    /// get the current config & state of all clients
    pub fn get_client_states(&self) -> Vec<(ClientHandle, ClientConfig, ClientState)> {
        self.clients
            .borrow()
            .iter()
            .map(|(k, v)| (k as ClientHandle, v.0.clone(), v.1.clone()))
            .collect()
    }

    /// update the fix ips of the client
    pub fn set_fix_ips(&self, handle: ClientHandle, fix_ips: Vec<IpAddr>) {
        if let Some((c, _)) = self.clients.borrow_mut().get_mut(handle as usize) {
            c.fix_ips = fix_ips
        }
        self.update_ips(handle);
    }

    /// update the dns-ips of the client
    pub fn set_dns_ips(&self, handle: ClientHandle, dns_ips: Vec<IpAddr>) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            s.dns_ips = dns_ips
        }
        self.update_ips(handle);
    }

    fn update_ips(&self, handle: ClientHandle) {
        if let Some((c, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            s.ips = c
                .fix_ips
                .iter()
                .cloned()
                .chain(s.dns_ips.iter().cloned())
                .collect::<HashSet<_>>();
        }
    }

    /// update the hostname of the given client
    /// this automatically clears the active ip address and ips from dns
    pub fn set_hostname(&self, handle: ClientHandle, hostname: Option<String>) -> bool {
        let mut clients = self.clients.borrow_mut();
        let Some((c, s)) = clients.get_mut(handle as usize) else {
            return false;
        };

        // hostname changed
        if c.hostname != hostname {
            c.hostname = hostname;
            s.active_addr = None;
            s.dns_ips.clear();
            drop(clients);
            self.update_ips(handle);
            true
        } else {
            false
        }
    }

    /// update the port of the client
    pub(crate) fn set_port(&self, handle: ClientHandle, port: u16) {
        match self.clients.borrow_mut().get_mut(handle as usize) {
            Some((c, s)) if c.port != port => {
                c.port = port;
                s.active_addr = s.active_addr.map(|a| SocketAddr::new(a.ip(), port));
            }
            _ => {}
        };
    }

    /// update the position of the client
    /// returns true, if a change in capture position is required (pos changed & client is active)
    pub(crate) fn set_pos(&self, handle: ClientHandle, pos: Position) -> bool {
        match self.clients.borrow_mut().get_mut(handle as usize) {
            Some((c, s)) if c.pos != pos => {
                log::info!("update pos {handle} {} -> {}", c.pos, pos);
                c.pos = pos;
                s.active
            }
            _ => false,
        }
    }

    /// **M3 — update the monitor binding** of the client.
    /// Returns the client's `state.active` when the value changed,
    /// mirroring `set_pos` exactly. The service handler uses the
    /// bool to drive the `deactivate + activate` round-trip that
    /// rebuilds the `BarrierKey` so the new monitor scope reaches
    /// the capture backend — and ONLY when the client is currently
    /// active (otherwise we'd accidentally activate a client the
    /// user only meant to re-bind). If the client is inactive, the
    /// new binding is honored on the next `activate_client` (which
    /// reads `c.monitor` fresh via `get_key`).
    pub(crate) fn set_monitor(&self, handle: ClientHandle, monitor: Option<String>) -> bool {
        match self.clients.borrow_mut().get_mut(handle as usize) {
            Some((c, s)) if c.monitor != monitor => {
                log::info!("update monitor {handle} {:?} -> {:?}", c.monitor, monitor);
                c.monitor = monitor;
                s.active
            }
            _ => false,
        }
    }

    /// **M0c / PLAN-2** — set the per-peer `enable_clipboard_to`
    /// flag. Returns `true` only when the value changed (mirrors the
    /// `set_input_channels` / `set_monitor` flow:
    /// return-bool-on-change → broadcast → save_config).
    pub(crate) fn set_enable_clipboard_to(&self, handle: ClientHandle, enable: bool) -> bool {
        match self.clients.borrow_mut().get_mut(handle as usize) {
            Some((c, s)) if c.enable_clipboard_to != enable => {
                log::info!(
                    "update enable_clipboard_to {handle} {} -> {}",
                    c.enable_clipboard_to,
                    enable
                );
                c.enable_clipboard_to = enable;
                s.active
            }
            _ => false,
        }
    }

    /// update the enter hook command of the client
    pub(crate) fn set_enter_hook(&self, handle: ClientHandle, enter_hook: Option<String>) {
        if let Some((c, _s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            c.cmd = enter_hook;
        }
    }

    /// Update the per-input-event transport selection (datagram vs
    /// reliable stream) for the given client. Returns `true` only
    /// when the value changed. Sender-side preference; the receiver
    /// has no parallel concept. Mirrors the `set_enter_hook` flow:
    /// return-bool-on-change → broadcast → save_config.
    pub(crate) fn set_input_channels(&self, handle: ClientHandle, cfg: InputChannelConfig) -> bool {
        match self.clients.borrow_mut().get_mut(handle as usize) {
            Some((c, _)) if c.input_channels != cfg => {
                c.input_channels = cfg;
                true
            }
            _ => false,
        }
    }

    /// set resolving status of the client
    pub(crate) fn set_resolving(&self, handle: ClientHandle, status: bool) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            s.resolving = status;
        }
    }

    /// get the enter hook command
    pub(crate) fn get_enter_cmd(&self, handle: ClientHandle) -> Option<String> {
        self.clients
            .borrow()
            .get(handle as usize)
            .and_then(|(c, _)| c.cmd.clone())
    }

    /// returns all clients that are currently registered
    pub(crate) fn registered_clients(&self) -> Vec<ClientHandle> {
        self.clients
            .borrow()
            .iter()
            .map(|(h, _)| h as ClientHandle)
            .collect()
    }

    /// returns all clients that are currently active
    pub(crate) fn active_clients(&self) -> Vec<ClientHandle> {
        self.clients
            .borrow()
            .iter()
            .filter(|(_, (_, s))| s.active)
            .map(|(h, _)| h as ClientHandle)
            .collect()
    }

    pub(crate) fn set_active_addr(&self, handle: ClientHandle, addr: Option<SocketAddr>) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            s.active_addr = addr;
        }
    }

    pub(crate) fn set_peer_commit(&self, handle: ClientHandle, commit: Option<[u8; 8]>) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            s.peer_commit = commit;
        }
    }

    pub(crate) fn active_addr(&self, handle: ClientHandle) -> Option<SocketAddr> {
        self.clients
            .borrow()
            .get(handle as usize)
            .and_then(|(_, s)| s.active_addr)
    }

    pub(crate) fn get_port(&self, handle: ClientHandle) -> Option<u16> {
        self.clients
            .borrow()
            .get(handle as usize)
            .map(|(c, _)| c.port)
    }

    pub(crate) fn get_ips(&self, handle: ClientHandle) -> Option<HashSet<IpAddr>> {
        self.clients
            .borrow()
            .get(handle as usize)
            .map(|(_, s)| s.ips.clone())
    }

    /// Stable identity of a peer, used to name its TOFU pin file
    /// (`known_peers/<key>.pin`, see
    /// [`crate::quic_transport::TofuVerifier`]).
    ///
    /// Derived from the peer's **configured** identity — hostname first,
    /// then the lowest configured fix ip. Deliberately *not* derived from
    /// [`ClientState::ips`]: that is a `HashSet` whose iteration order
    /// varies per process, so a key taken from it would move between
    /// restarts, silently re-pair the peer, and litter `known_peers/` with a
    /// pin per ordering. `min()` (rather than `first()`) keeps the key
    /// stable even if the user reorders the configured ip list.
    ///
    /// The `client-<handle>` fallback only applies to a client configured
    /// with neither a hostname nor a fix ip — which cannot be dialed anyway,
    /// so it is a well-formedness guard rather than a real identity.
    pub(crate) fn peer_key(&self, handle: ClientHandle) -> Option<String> {
        self.clients.borrow().get(handle as usize).map(|(c, _)| {
            c.hostname
                .as_deref()
                .map(str::trim)
                .filter(|h| !h.is_empty())
                .map(str::to_owned)
                .or_else(|| c.fix_ips.iter().min().map(IpAddr::to_string))
                .unwrap_or_else(|| format!("client-{handle}"))
        })
    }

    /// Per-handle input-channel configuration (datagram vs. stream for
    /// mouse-button and keyboard). `None` is only returned for an
    /// out-of-range (invalid) handle; a valid handle always returns
    /// `Some(InputChannelConfig)`.
    ///
    /// Consumed by `LanMouseConnection::send`: the result is passed to
    /// [`crate::quic_transport::PeerSession::send_input`] as the key
    /// for `route_input` dispatch. Callers fall back to
    /// `unwrap_or_default()` on `None`, matching
    /// `InputChannelConfig::default()`.
    pub(crate) fn input_channels(&self, handle: ClientHandle) -> Option<InputChannelConfig> {
        self.clients
            .borrow()
            .get(handle as usize)
            .map(|(c, _)| c.input_channels)
    }
}

#[cfg(test)]
mod client_input_channels_tests {
    use super::*;
    use lan_mouse_ipc::{ChannelMode, DEFAULT_PORT};

    /// Asserts that `add_with_config` preserves `input_channels` across
    /// the `ConfigClient` -> `ClientConfig` conversion. Fails (compile
    /// or assert) if the field is dropped again.
    #[test]
    fn add_with_config_preserves_input_channels() {
        let cm = ClientManager::default();
        let cfg_client = ConfigClient {
            ips: HashSet::new(),
            hostname: Some("peer-east".into()),
            port: DEFAULT_PORT,
            pos: Position::Right,
            active: false,
            enter_hook: None,
            input_channels: InputChannelConfig {
                mouse_button: ChannelMode::Stream,
                keyboard: ChannelMode::Datagram,
            },
            // **M3**: the test fixture predates monitor binding; the
            // `add_with_config_preserves_input_channels` assertion is
            // about `input_channels`, so defaulting `monitor` to
            // `None` keeps the test focused. Add a parallel
            // `add_with_config_preserves_monitor` test below for the
            // monitor-binding half of the contract.
            monitor: None,
            // **M0c**: legacy default = true (matches
            // `client_config_input_channels_default_when_missing` and
            // the wire compat contract).
            enable_clipboard_to: true,
        };
        let handle = cm.add_with_config(cfg_client);
        let (c, _) = cm.get_state(handle).unwrap();
        assert_eq!(c.input_channels.mouse_button, ChannelMode::Stream);
        assert_eq!(c.input_channels.keyboard, ChannelMode::Datagram);
    }

    /// `set_input_channels` returns `true` only when the value actually
    /// changed. The service.rs handler uses this to skip the
    /// `broadcast_client` + `save_config` round-trip on no-op writes,
    /// matching the project-wide `set_*` family pattern.
    #[test]
    fn set_input_channels_returns_true_only_on_change() {
        let cm = ClientManager::default();
        let handle = cm.add_client();
        // `InputChannelConfig::default()` = { mouse: Datagram, keyboard: Stream }.
        // To exercise the "changed" branch on the first write, pick a
        // config that **differs** in at least one field — here both fields.
        let gaming = InputChannelConfig {
            mouse_button: ChannelMode::Stream,
            keyboard: ChannelMode::Datagram,
        };
        assert_ne!(
            gaming,
            InputChannelConfig::default(),
            "test fixture: gaming must differ from default for this assertion to be meaningful"
        );
        // first write: default -> gaming → changed
        assert!(cm.set_input_channels(handle, gaming));
        // second write: gaming -> gaming → no change
        assert!(!cm.set_input_channels(handle, gaming));
        // third write: gaming -> office (truly different config) → changed
        let office = InputChannelConfig {
            mouse_button: ChannelMode::Datagram,
            keyboard: ChannelMode::Stream,
        };
        // office differs from gaming in **both** fields (S/M vs D/S), so the
        // setter must report a change.
        assert!(cm.set_input_channels(handle, office));
        let (c, _) = cm.get_state(handle).unwrap();
        assert_eq!(c.input_channels, office);
    }

    /// **M3 — `add_with_config` carries the monitor binding through**
    /// the `ConfigClient` → `ClientConfig` boundary. Without this,
    /// `Config.monitor = Some(...)` set by `SaveConfiguration` would
    /// silently drop on the next daemon restart (because the
    /// `ClientManager` rebuilds clients from `ConfigClient`).
    #[test]
    fn add_with_config_preserves_monitor() {
        let cm = ClientManager::default();
        let cfg = ConfigClient {
            ips: HashSet::new(),
            hostname: Some("peer-east".into()),
            port: DEFAULT_PORT,
            pos: Position::Top,
            active: false,
            enter_hook: None,
            input_channels: InputChannelConfig::default(),
            monitor: Some("wl_output:eDP-1".into()),
            // M0c default
            enable_clipboard_to: true,
        };
        let handle = cm.add_with_config(cfg);
        let (c, _) = cm.get_state(handle).unwrap();
        assert_eq!(c.monitor.as_deref(), Some("wl_output:eDP-1"));
    }

    /// **M3 — `add_with_config` defaults monitor to `None`** for
    /// legacy clients whose `ConfigClient.monitor` was never set.
    /// Pins the "no monitor key in TOML → `None` in `ClientConfig`"
    /// contract; matches `client_config_monitor_default_when_missing`
    /// on the IPC side and `config_defaults_when_monitor_missing` on
    /// the config side.
    #[test]
    fn add_with_config_defaults_monitor_to_none() {
        let cm = ClientManager::default();
        let cfg = ConfigClient {
            ips: HashSet::new(),
            hostname: Some("legacy-peer".into()),
            port: DEFAULT_PORT,
            pos: Position::Right,
            active: false,
            enter_hook: None,
            input_channels: InputChannelConfig::default(),
            monitor: None,
            // M0c default
            enable_clipboard_to: true,
        };
        let handle = cm.add_with_config(cfg);
        let (c, _) = cm.get_state(handle).unwrap();
        assert_eq!(c.monitor, None);
    }

    /// **M3 — `set_monitor` returns `s.active` when the value
    /// changed**. Mirrors `set_pos` exactly: the bool tells the
    /// service handler whether to drive the `deactivate + activate`
    /// round-trip (only when the client is currently active). The
    /// underlying field is updated regardless of the return value
    /// — `false` means "no rebuild needed", not "value dropped".
    #[test]
    fn set_monitor_returns_active_when_value_changed() {
        let cm = ClientManager::default();
        let handle = cm.add_client();
        // Fresh client → inactive. First write changes the value
        // but the client isn't active → returns `false` (no
        // rebuild needed). The field IS still updated.
        assert!(!cm.set_monitor(handle, Some("DP-2".into())));
        let (c, _) = cm.get_state(handle).unwrap();
        assert_eq!(c.monitor.as_deref(), Some("DP-2"));

        // Same value again → no change at all → `false`.
        assert!(!cm.set_monitor(handle, Some("DP-2".into())));

        // Now activate, then change → returns `true` because the
        // client is active and the value differs.
        cm.activate_client(handle);
        assert!(cm.set_monitor(handle, Some("HDMI-A-1".into())));

        // Same value again → no change → `false`.
        assert!(!cm.set_monitor(handle, Some("HDMI-A-1".into())));

        // Different value while active → `true`.
        assert!(cm.set_monitor(handle, Some("DP-2".into())));

        // Deactivate, then change → returns `false` (not active).
        cm.deactivate_client(handle);
        assert!(!cm.set_monitor(handle, None));

        // Same None → no change → `false`.
        assert!(!cm.set_monitor(handle, None));

        let (c, _) = cm.get_state(handle).unwrap();
        assert_eq!(c.monitor, None);
    }

    /// **M3 — `get_key` reflects the monitor binding**. This is the
    /// single most important assertion for the M3 end-to-end story:
    /// after `set_monitor`, the `BarrierKey` returned by `get_key`
    /// must carry `Some(id)` so `activate_client` → `capture.create`
    /// reaches the backend with a monitor-scoped key. Without this
    /// the M1 default (`monitor: None`) would persist forever and
    /// every user-set binding would silently no-op.
    #[test]
    fn get_key_includes_monitor_binding() {
        let cm = ClientManager::default();
        let handle = cm.add_client();

        // Legacy default → key.monitor is None.
        let key = cm.get_key(handle).expect("handle valid");
        assert_eq!(key.monitor, None);

        // Bind to DP-2 → key.monitor is Some("DP-2").
        cm.set_monitor(handle, Some("DP-2".into()));
        let key = cm.get_key(handle).expect("handle valid");
        assert_eq!(key.monitor.as_deref(), Some("DP-2"));
        // pos + offset + span still default; the change is scoped
        // to the new field only.
        assert_eq!(key.pos, input_capture::Position::Left);
        assert_eq!(key.offset, 0);
        assert_eq!(key.span, 10000);

        // Bind to a different monitor → key changes accordingly.
        cm.set_monitor(handle, Some("HDMI-A-1".into()));
        let key = cm.get_key(handle).expect("handle valid");
        assert_eq!(key.monitor.as_deref(), Some("HDMI-A-1"));

        // Unbind (set_monitor(handle, None)) → key.monitor is None
        // again, matching the legacy default.
        cm.set_monitor(handle, None);
        let key = cm.get_key(handle).expect("handle valid");
        assert_eq!(key.monitor, None);
    }

    /// **M3 — `client_at` scopes by `(pos, monitor)`**. Two active
    /// clients at the same position on *different* monitors must not
    /// collide: `client_at` must look up only the one matching both
    /// fields. This is the symmetric M3 half of `get_key_includes_monitor_binding`
    /// — without it, activating one client would deactivate the
    /// other (the M1 `pos`-only lookup did exactly that).
    #[test]
    fn client_at_scopes_by_pos_and_monitor() {
        use input_capture::BarrierKey;

        let cm = ClientManager::default();
        let h_left = cm.add_client();
        let h_right = cm.add_client();
        let h_top = cm.add_client();
        // All three are at position Top, monitors are different.
        cm.set_pos(h_left, Position::Top);
        cm.set_pos(h_right, Position::Top);
        cm.set_pos(h_top, Position::Top);
        cm.activate_client(h_left);
        cm.activate_client(h_right);
        cm.activate_client(h_top);
        cm.set_monitor(h_left, Some("DP-2".into()));
        cm.set_monitor(h_right, Some("HDMI-A-1".into()));
        cm.set_monitor(h_top, None); // legacy "any monitor"

        let key_left = BarrierKey {
            pos: input_capture::Position::Top,
            monitor: Some("DP-2".into()),
            offset: 0,
            span: 10000,
        };
        let key_right = BarrierKey {
            pos: input_capture::Position::Top,
            monitor: Some("HDMI-A-1".into()),
            offset: 0,
            span: 10000,
        };
        let key_legacy = BarrierKey {
            pos: input_capture::Position::Top,
            monitor: None,
            offset: 0,
            span: 10000,
        };

        assert_eq!(cm.client_at(&key_left), Some(h_left));
        assert_eq!(cm.client_at(&key_right), Some(h_right));
        // Legacy key (monitor=None) must NOT match any of the bound
        // clients — it only matches the client that itself has
        // monitor=None. This is the half that protects against the
        // pre-M3 collision where activating a legacy client would
        // deactivate a bound one.
        assert_eq!(cm.client_at(&key_legacy), Some(h_top));
    }
}
