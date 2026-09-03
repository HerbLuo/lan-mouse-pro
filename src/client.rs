use std::{
    cell::RefCell,
    collections::HashSet,
    net::{IpAddr, SocketAddr},
    rc::Rc,
};

use slab::Slab;

use lan_mouse_ipc::{ClientConfig, ClientHandle, ClientState, InputChannelConfig, Position};

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
            // STEP-4.5a: forward the per-handle input-channel
            // selection. Without this line the `ConfigClient` →
            // `ClientConfig` conversion silently drops the field, so
            // whatever the user wrote in `config.toml`
            // (`input_channels = { ... }`) never reaches the frontend
            // editor / runtime. Half-link bug introduced by STEP-4.2
            // (which only stored the field on disk). Mirrored from
            // bak `mousehop/src/client.rs:1-50 add_with_config`.
            input_channels: config_client.input_channels,
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

    /// get the client at the given position
    pub fn client_at(&self, pos: Position) -> Option<ClientHandle> {
        self.clients
            .borrow()
            .iter()
            .find_map(|(k, (c, s))| {
                if s.active && c.pos == pos {
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

    /// get the position of the corresponding client
    pub(crate) fn get_pos(&self, handle: ClientHandle) -> Option<Position> {
        self.clients
            .borrow()
            .get(handle as usize)
            .map(|(c, _)| c.pos)
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

    /// STEP-6.1: per-handle 输入通道配置（mouse_button / keyboard 各选
    /// datagram 或 stream）。`None` 仅出现在 handle 越界（无效）；正常
    /// handle 总是 `Some(InputChannelConfig)`（STEP-4.5a 已落实
    /// `ConfigClient.input_channels` 透传到 `ClientConfig`）。
    ///
    /// **`LanMouseConnection::send` 消费** —— 拿到本返回值后传给
    /// [`crate::quic_transport::PeerSession::send_input`] 作为
    /// `route_input` 分派的 key。`None` 在 caller 处走
    /// `unwrap_or_default()` 兜底（与 STEP-4.1 `InputChannelConfig::default()`
    /// 一致）。
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

    /// STEP-4.5a: regression test for the half-link bug STEP-4.2 left
    /// behind. `ConfigClient` was carrying `input_channels` on disk
    /// (STEP-4.2), but `add_with_config` was discarding the field on
    /// the way into `ClientConfig`. This test asserts the field now
    /// survives the conversion. Will fail (compile or assert) if
    /// `add_with_config` reverts to dropping `input_channels`.
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
        };
        let handle = cm.add_with_config(cfg_client);
        let (c, _) = cm.get_state(handle).unwrap();
        assert_eq!(c.input_channels.mouse_button, ChannelMode::Stream);
        assert_eq!(c.input_channels.keyboard, ChannelMode::Datagram);
    }

    /// STEP-4.5a: setter on `ClientManager`. Returns `true` only when
    /// the value actually changed — used by the service.rs handler to
    /// skip the `broadcast_client` + `save_config` round-trip on no-op
    /// writes (matches the bak mousehop `set_input_channels` contract
    /// and the project-wide `set_*` family pattern).
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
}
