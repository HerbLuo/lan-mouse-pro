use crate::{
    capture::{Capture, CaptureType, ICaptureEvent},
    client::ClientManager,
    config::{Config, ConfigClient},
    connect::LanMouseConnection,
    crypto,
    dns::{DnsEvent, DnsResolver},
    emulation::{Emulation, EmulationEvent},
    listen::{LanMouseListener, ListenerCreationError},
};
use futures::StreamExt;
use input_capture::{BarrierKey, MonitorInfo as GeometryMonitorInfo};
use lan_mouse_ipc::{
    AsyncFrontendListener, ClientHandle, FrontendEvent, FrontendRequest, InputChannelConfig,
    IpcError, IpcListenerCreationError, MonitorInfo as IpcMonitorInfo, Position, Status,
};
use log;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    net::{IpAddr, SocketAddr},
    sync::{Arc, RwLock},
    time::Duration,
};
use thiserror::Error;
use tokio::{process::Command, signal, sync::Notify, sync::mpsc as tokio_mpsc};

use crate::clipboard::{ClipboardBackend, default_backend};
use lan_mouse_proto::{ClipboardText, ProtoEvent};
use sha2::{Digest, Sha256};

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error(transparent)]
    IpcListen(#[from] IpcListenerCreationError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    ListenError(#[from] ListenerCreationError),
    #[error("failed to load certificate: `{0}`")]
    Certificate(#[from] crypto::Error),
}

pub struct Service {
    /// configuration
    config: Config,
    /// input capture
    capture: Capture,
    /// input emulation
    emulation: Emulation,
    /// dns resolver
    resolver: DnsResolver,
    /// frontend listener
    frontend_listener: AsyncFrontendListener,
    /// authorized public key sha256 fingerprints
    authorized_keys: Arc<RwLock<HashMap<String, String>>>,
    /// (outgoing) client information
    client_manager: ClientManager,
    /// current port
    port: u16,
    /// the public key fingerprint for (D)TLS
    public_key_fingerprint: String,
    /// QUIC `max_idle_timeout` currently in effect (seconds). Frozen at
    /// startup from `config.quic_idle_timeout()`; updated only via
    /// [`Service::set_quic_idle_timeout`] (which still can't apply it
    /// to the running endpoint — see
    /// [`crate::quic_transport::tls::default_transport_config`]).
    quic_idle_timeout_secs: u64,
    /// notify for pending frontend events
    frontend_event_pending: Notify,
    /// frontend events queued for sending
    pending_frontend_events: VecDeque<FrontendEvent>,
    /// status of input capture (enabled / disabled)
    capture_status: Status,
    /// status of input emulation (enabled / disabled)
    emulation_status: Status,
    /// keep track of registered connections to avoid duplicate barriers
    incoming_conns: HashSet<SocketAddr>,
    /// map from capture handle to connection info
    incoming_conn_info: HashMap<ClientHandle, Incoming>,
    next_trigger_handle: u64,
    /// **STEP-M2-2.6**: most recently observed monitor snapshot.
    /// Used by `reconcile_monitors_changed` to detect which
    /// `MonitorId` disappeared / re-appeared / changed geometry
    /// between two backend emissions. Reset to `None` initially so
    /// the first emission (the session-start seed from
    /// `CaptureTask::do_capture`) is always treated as a "first
    /// observation" — no `BindingInvalid` is sent on startup.
    last_monitors: Option<Vec<GeometryMonitorInfo>>,
    /// **PLAN-2 / M1a STEP-1a.4** — clipboard state. `None` when
    /// the platform backend is unavailable
    /// ([`ClipboardError::NotImplemented`] / `ToolMissing`); the
    /// dispatch loop is skipped in that case. `Some(_)` means the
    /// 500 ms tick is active and the service pushes inbound
    /// `ClipboardText` events through this backend.
    clipboard_backend: Option<Box<dyn ClipboardBackend>>,
    /// **M1a STEP-1a.4** — LRU of recently-written fingerprints.
    /// Loopback defence: a peer-pushed `ClipboardText` whose
    /// `sha256` is in the LRU is treated as our own writeback and
    /// dropped instead of re-applying it to the local clipboard.
    /// Capacity 64 per PLAN §3 M1a "仅指纹比对防'收到本地写回内容'的最简回环".
    /// **M1a known limitation**: under "copy 64 different things
    /// in 60 s" pressure the LRU rolls and the same fingerprint
    /// can come back through — at that point we re-apply a write
    /// we did locally. M1b tightens the LRU to 128 + 60 s TTL +
    /// explicit `cache.remove` on push (PLAN §1 评审 #3 2nd + #4
    /// 3rd).
    clipboard_lru: LruFingerprints,
    /// **M1a STEP-1a.4** — last text observed by `current_text()`.
    /// Avoids re-hashing on every tick when the clipboard is
    /// quiescent. `None` until the first successful read; reset to
    /// `None` after an inbound `set_text` so the next tick re-reads
    /// and confirms the new value.
    clipboard_last_text: Option<String>,
    /// **M1a STEP-1a.4** — receiver for inbound clipboard events.
    /// Senders live in two places:
    /// - `Emulation::new` clones the sender into the
    ///   `ListenTask` (server side — `server_stream_c_reader_task`
    ///   pushes every var-codec frame as `ListenEvent::Msg`).
    /// - `connect_to_handle` clones the sender into
    ///   `peer.set_clipboard_inbox(...)` (client side — `peer.run`
    ///   forwards every `StreamEvent::ClipboardMeta` here).
    ///
    /// Both paths funnel to this single receiver, consumed by the
    /// `select!` arm in [`Service::run`].
    clipboard_inbound_rx: tokio_mpsc::UnboundedReceiver<(SocketAddr, ProtoEvent)>,
    /// **M1a STEP-1a.4** — sender cloned into
    /// [`crate::emulation::Emulation::new`] and
    /// [`crate::connect::connect_to_handle`]. The dispatcher's
    /// inbound path is the only consumer of the receiver half.
    /// Kept on the struct (rather than passed by value) so the
    /// `Emulation` constructed in `Service::new` retains its
    /// sender for the daemon's lifetime.
    #[allow(dead_code)]
    clipboard_inbound_tx: tokio_mpsc::UnboundedSender<(SocketAddr, ProtoEvent)>,
    /// **M1a follow-up #1** — receiver for "peer just became
    /// active" notifications from
    /// [`crate::connect::connect_to_handle`]. The sender clone
    /// lives on `LanMouseConnection::clipboard_push_notify_tx`
    /// and fires once per `set_active_addr(handle, Some(addr))`
    /// success path. The dispatcher arm
    /// (`handle_clipboard_recover_push`) reads the current local
    /// clipboard and pushes it through `broadcast_clipboard_event`
    /// to recover copies the user made during the 5–35 s dial
    /// window — see BUGS.md "启动时未跨鼠标边界,复制文本无法传到
    /// 被控端 (M1a follow-up #1)".
    clipboard_push_notify_rx: tokio_mpsc::UnboundedReceiver<ClientHandle>,
    /// **M1a STEP-1a.4** — 500 ms tick for the clipboard poll
    /// loop. `tokio::time::Interval` is `select!`-compatible so
    /// the service's main loop drives the dispatch directly (no
    /// separate task → no `Arc<Mutex<...>>` plumbing).
    /// `Interval::tick` skips the first tick immediately; the
    /// dispatch loop is `loop { _ = tick.tick() => ... }` so the
    /// first dispatch happens at t≈500 ms, not t=0. This avoids
    /// racing the daemon's startup handshake (which is also doing
    /// `select!` work in the same task).
    clipboard_tick: tokio::time::Interval,
    /// **M1a STEP-1a.4** — last clipboard-sync timestamps for the
    /// `FrontendEvent::ClipboardState` push. `None` until the
    /// first sync (text / image / file). Updated on every
    /// outbound push (local change) and every inbound apply
    /// (peer change). `last_source` carries the peer hostname /
    /// `SocketAddr` on inbound; `None` on local-origin.
    last_text_ts_ms: Option<u64>,
    last_image_ts_ms: Option<u64>,
    last_file_ts_ms: Option<u64>,
    /// **M1a STEP-1a.4** — peer `SocketAddr` (or local sentinel)
    /// that most recently updated the clipboard. Used for the
    /// `last_source` field of `FrontendEvent::ClipboardState`;
    /// `None` means the most recent change originated locally.
    last_clipboard_source: Option<SocketAddr>,
}

/// **PLAN-2 / M1a STEP-1a.4** — fixed-capacity LRU of SHA-256
/// fingerprints, used by the clipboard dispatcher's loopback defence.
///
/// **Implementation**: `VecDeque<[u8; 32]>` with linear
/// `contains`. Capacity 64 → `contains` is O(64) = ~64 byte
/// comparisons per inbound event, which is well below the dispatch
/// tick's 1-3 ms typical work. A `HashSet` would be asymptotically
/// faster but adds allocation pressure and code surface; the
/// `VecDeque` matches the M1a "minimum viable loopback defence"
/// scope (PLAN §3 M1a "仅指纹比对防'收到本地写回内容'的最简回环").
#[derive(Debug)]
struct LruFingerprints {
    capacity: usize,
    items: VecDeque<[u8; 32]>,
}

impl LruFingerprints {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            items: VecDeque::with_capacity(capacity),
        }
    }

    fn contains(&self, fp: &[u8; 32]) -> bool {
        self.items.contains(fp)
    }

    fn push(&mut self, fp: [u8; 32]) {
        if self.items.len() >= self.capacity {
            self.items.pop_front();
        }
        self.items.push_back(fp);
    }

    /// Test-only: drain the LRU. Used by the dispatcher unit tests
    /// to assert "marked" vs "not marked" without exposing the
    /// `VecDeque` to the test code.
    #[cfg(test)]
    #[allow(dead_code)]
    fn len(&self) -> usize {
        self.items.len()
    }
}

#[derive(Debug)]
struct Incoming {
    fingerprint: String,
    addr: SocketAddr,
    pos: Position,
}

impl Service {
    pub async fn new(config: Config) -> Result<Self, ServiceError> {
        let client_manager = ClientManager::default();
        for client in config.clients() {
            client_manager.add_with_config(client);
        }

        // Load (or self-sign) the server certificate and compute the
        // public-key fingerprint as SHA-256 over the cert DER. The
        // fingerprint algorithm matches the legacy WebRTC-DTLS path so
        // existing `authorized_keys` entries remain valid across the
        // QUIC migration. The same `(cert_chain, key)` is reused for
        // both the listener and outgoing connections.
        let cert_der = crypto::load_or_create_server_cert()?;
        let public_key_fingerprint = crypto::generate_fingerprint(cert_der.0[0].as_ref());

        // create frontend communication adapter, exit if already running
        let frontend_listener = AsyncFrontendListener::new().await?;

        let authorized_keys = Arc::new(RwLock::new(config.authorized_fingerprints()));
        // QUIC `max_idle_timeout` is read once at startup and frozen for the
        // lifetime of the daemon — both the listener endpoint and the client
        // dials use it. GUI edits to `quic.idle_timeout_secs` are persisted
        // to TOML but require a daemon restart to take effect (see the
        // `FrontendRequest::SetQuicIdleTimeout` docstring).
        let quic_idle_timeout = config.quic_idle_timeout();
        let listener = LanMouseListener::new(
            config.port(),
            cert_der.0.clone(),
            cert_der.1.clone_key(),
            authorized_keys.clone(),
            quic_idle_timeout,
        )
        .await?;
        let client_endpoint =
            crate::quic_transport::endpoint(std::net::SocketAddr::from(([0, 0, 0, 0], 0)))
                .map_err(|e| {
                    ServiceError::Io(io::Error::other(format!(
                        "client endpoint bind failed: {e}"
                    )))
                })?;
        let pins_dir = crypto::cert_pins_dir();

        // Pong-health → capture-release channel. Created once at service
        // startup; sender is cloned into every supervisor spawned by
        // `connect_to_handle`, receiver lives on `Capture` (consumed by
        // `do_capture_session`'s `select!` loop). See
        // `connect.rs::pong_health_watchdog` for the producer side.
        let (peer_lost_tx, peer_lost_rx) = local_channel::mpsc::channel();

        // **M1a STEP-1a.4** — clipboard inbound channel: sender is
        // cloned into the listen-side `Emulation::ListenTask` (for
        // server-pushed clipboard) and the connect-side
        // `peer.set_clipboard_inbox` (for client-pushed clipboard).
        // Both feed the same receiver consumed by `Service::run`'s
        // `select!` arm. Constructed BEFORE `LanMouseConnection::new`
        // because the conn's `dial` / `connect_to_handle` flow needs
        // to clone the sender for every new peer.
        let (clipboard_inbound_tx, clipboard_inbound_rx) =
            tokio_mpsc::unbounded_channel::<(SocketAddr, ProtoEvent)>();

        // **M1a follow-up #1** — "peer just became active"
        // notification channel. Sender clone is moved into
        // `LanMouseConnection` (and from there into every
        // `connect_to_handle` task), receiver half lives on
        // `Service` and is polled in `Service::run`'s `select!`
        // arm. See the field doc on `clipboard_push_notify_rx`
        // for the rationale.
        let (clipboard_push_notify_tx, clipboard_push_notify_rx) =
            tokio_mpsc::unbounded_channel::<ClientHandle>();

        let conn = LanMouseConnection::new(
            client_endpoint,
            cert_der.0.clone(),
            cert_der.1.clone_key(),
            pins_dir,
            client_manager.clone(),
            quic_idle_timeout,
            peer_lost_tx,
            clipboard_inbound_tx.clone(),
            clipboard_push_notify_tx,
        );

        // input capture + emulation
        let capture_backend = config.capture_backend().map(|b| b.into());
        // FIX 4：从 config.toml + env 构造 watchdog 配置，传给 Capture。
        let watchdog_config = config.watchdog_config();
        let capture = Capture::new(
            capture_backend,
            conn,
            config.release_bind(),
            watchdog_config,
            peer_lost_rx,
        );

        // Try to construct the platform clipboard backend. Failures
        // (NotImplemented on platforms without a built file,
        // ToolMissing when xclip / wl-paste / pbcopy are absent) are
        // logged + the dispatch loop is skipped — the rest of the
        // daemon stays alive.
        let clipboard_backend = match default_backend() {
            Ok(b) => {
                log::info!("clipboard backend selected: {}", b.name());
                Some(b)
            }
            Err(e) => {
                log::warn!("clipboard backend unavailable (clipboard sync disabled): {e}");
                None
            }
        };

        let emulation_backend = config.emulation_backend().map(|b| b.into());
        let emulation = Emulation::new(emulation_backend, listener, clipboard_inbound_tx.clone());

        // create dns resolver
        let resolver = DnsResolver::new()?;

        let port = config.port();
        let quic_idle_timeout_secs = quic_idle_timeout.as_secs();
        let service = Self {
            config,
            capture,
            emulation,
            frontend_listener,
            resolver,
            authorized_keys,
            public_key_fingerprint,
            quic_idle_timeout_secs,
            client_manager,
            frontend_event_pending: Default::default(),
            port,
            pending_frontend_events: Default::default(),
            capture_status: Default::default(),
            emulation_status: Default::default(),
            incoming_conn_info: Default::default(),
            incoming_conns: Default::default(),
            next_trigger_handle: 0,
            // STEP-M2-2.6: `None` until the first `ICaptureEvent::
            // MonitorsChanged` from `CaptureTask::do_capture`. The
            // first emission is treated as a seed — no reconcile
            // action fires until a second emission arrives with a
            // diff against this baseline.
            last_monitors: None,
            // **M1a STEP-1a.4** — clipboard dispatch state.
            // `clipboard_backend` is `Some` only when
            // `default_backend()` succeeded; otherwise the dispatch
            // tick + inbound arm in `Service::run` are no-ops.
            // `clipboard_tick` is constructed unconditionally — the
            // tick is `select!`-polled regardless, but the
            // `clipboard_backend` guard short-circuits the
            // no-backend case.
            clipboard_backend,
            clipboard_lru: LruFingerprints::new(64),
            clipboard_last_text: None,
            clipboard_inbound_rx,
            clipboard_inbound_tx,
            // **M1a follow-up #1** — push-notify receiver. The
            // matching sender was moved into `LanMouseConnection`
            // above. Polled in `Service::run`'s `select!` arm.
            clipboard_push_notify_rx,
            clipboard_tick: tokio::time::interval(Duration::from_millis(500)),
            last_text_ts_ms: None,
            last_image_ts_ms: None,
            last_file_ts_ms: None,
            last_clipboard_source: None,
        };
        Ok(service)
    }

    pub async fn run(&mut self) -> Result<(), ServiceError> {
        let active = self.client_manager.active_clients();
        for handle in active.iter() {
            // small hack: `activate_client()` checks, if the client
            // is already active in client_manager and does not create a
            // capture barrier in that case so we have to deactivate it first
            self.client_manager.deactivate_client(*handle);
        }

        for handle in active {
            self.activate_client(handle);
        }

        loop {
            tokio::select! {
                request = self.frontend_listener.next() => self.handle_frontend_request(request),
                _ = self.frontend_event_pending.notified() => self.handle_frontend_pending().await,
                event = self.emulation.event() => self.handle_emulation_event(event),
                event = self.capture.event() => self.handle_capture_event(event),
                event = self.resolver.event() => self.handle_resolver_event(event),
                _ = self.config.changed() => self.handle_config_change(),
                // **M1a STEP-1a.4** — clipboard dispatch tick. Polls
                // `clipboard_backend.current_text()` every 500 ms;
                // on change → LRU + push to peers. Short-circuits
                // when the backend is `None` (platform
                // unsupported / tool missing).
                _ = self.clipboard_tick.tick() => self.handle_clipboard_tick().await,
                // **M1a STEP-1a.4** — inbound clipboard event from a
                // peer. Server-side: `Emulation::ListenTask` pushes
                // here. Client-side: `peer.set_clipboard_inbox` (set
                // in `connect_to_handle`) pushes here.
                Some(inbound) = self.clipboard_inbound_rx.recv() => {
                    self.handle_clipboard_inbound(inbound);
                }
                // **M1a follow-up #1** — peer just transitioned
                // `active_addr: None → Some(addr)`. Read current
                // local clipboard and push it through
                // `broadcast_clipboard_event` to recover copies
                // made during the dial window. The
                // `broadcast_clipboard_event` gate
                // (`active_addr.is_none()` filter) is now satisfied
                // because the trigger fires *after*
                // `set_active_addr` succeeds.
                Some(handle) = self.clipboard_push_notify_rx.recv() => {
                    self.handle_clipboard_recover_push(handle).await;
                }
                r = signal::ctrl_c() => break r.expect("failed to wait for CTRL+C"),
            }
        }

        log::info!("terminating service ...");
        log::debug!("terminating capture ...");
        self.capture.terminate().await;
        log::debug!("terminating emulation ...");
        self.emulation.terminate().await;
        log::debug!("terminating dns resolver ...");
        self.resolver.terminate().await;

        Ok(())
    }

    fn handle_frontend_request(&mut self, request: Option<Result<FrontendRequest, IpcError>>) {
        let request = match request.expect("frontend listener closed") {
            Ok(r) => r,
            Err(e) => return log::error!("error receiving request: {e}"),
        };
        match request {
            FrontendRequest::Activate(handle, active) => {
                self.set_client_active(handle, active);
                self.save_config();
            }
            FrontendRequest::AuthorizeKey(desc, fp) => {
                self.add_authorized_key(desc, fp);
                self.save_config();
            }
            FrontendRequest::ChangePort(port) => self.change_port(port),
            FrontendRequest::Create => {
                self.add_client();
                self.save_config();
            }
            FrontendRequest::Delete(handle) => {
                self.remove_client(handle);
                self.save_config();
            }
            FrontendRequest::EnableCapture => self.capture.reenable(),
            FrontendRequest::EnableEmulation => self.emulation.reenable(),
            FrontendRequest::Enumerate() => self.enumerate(),
            FrontendRequest::UpdateFixIps(handle, fix_ips) => {
                self.update_fix_ips(handle, fix_ips);
                self.save_config();
            }
            FrontendRequest::UpdateHostname(handle, host) => {
                self.update_hostname(handle, host);
                self.save_config();
            }
            FrontendRequest::UpdatePort(handle, port) => {
                self.update_port(handle, port);
                self.save_config();
            }
            FrontendRequest::UpdatePosition(handle, pos) => {
                self.update_pos(handle, pos);
                self.save_config();
            }
            // **M3 — monitor binding**. The handler rebuilds the
            // active capture barrier so the new monitor scope reaches
            // `Capture::create` via the standard `deactivate +
            // activate` round-trip; this is the same shape as
            // `UpdatePosition`. `monitor = None` clears the binding
            // (back to legacy "any monitor" behavior); `Some(id)`
            // re-binds to a specific `MonitorInfo.id` (the GUI
            // dropdown sources its options from the most recent
            // `MonitorsChanged` event).
            FrontendRequest::UpdateMonitor(handle, monitor) => {
                self.update_monitor(handle, monitor);
                self.save_config();
            }
            FrontendRequest::ResolveDns(handle) => self.resolve(handle),
            FrontendRequest::Sync => self.sync_frontend(),
            FrontendRequest::RemoveAuthorizedKey(key) => {
                self.remove_authorized_key(key);
                self.save_config();
            }
            FrontendRequest::UpdateEnterHook(handle, enter_hook) => {
                self.update_enter_hook(handle, enter_hook)
            }
            FrontendRequest::SetClientInputChannels(handle, cfg) => {
                self.update_input_channels(handle, cfg);
                self.save_config();
            }
            FrontendRequest::SaveConfiguration => self.save_config(),
            FrontendRequest::SetQuicIdleTimeout(secs) => self.set_quic_idle_timeout(secs),
            // **M0c / PLAN-2**: daemon-global clipboard config.
            // Persists to TOML `[clipboard]` section. M1a wires the
            // `service::clipboard::apply_config` runtime effect; for
            // M0c we just persist + log so the value survives restart.
            FrontendRequest::SetClipboardConfig(cfg) => {
                self.set_clipboard_config(cfg);
            }
            // **M0c / PLAN-2**: per-peer clipboard opt-in. Persists
            // to TOML `[[clients]]` `enable_clipboard_to` field and
            // echoes back via the standard `FrontendEvent::State`
            // push. M1a gates `service::clipboard_dispatcher` on this
            // flag; for M0c we just persist.
            FrontendRequest::SetEnableClipboardTo(handle, enable) => {
                self.set_enable_clipboard_to(handle, enable);
                self.save_config();
            }
        }
    }

    fn save_config(&mut self) {
        let clients = self.client_manager.clients();
        let clients = clients
            .into_iter()
            .map(|(c, s)| ConfigClient {
                ips: HashSet::from_iter(c.fix_ips),
                hostname: c.hostname,
                port: c.port,
                pos: c.pos,
                active: s.active,
                enter_hook: c.cmd,
                input_channels: c.input_channels,
                // **M3**: forward the per-handle monitor binding so
                // `save_config` round-trips through the TOML layer
                // unchanged. The default-None omission is handled on
                // the `TomlClient` side (see `config_omits_monitor_field_when_none_on_writeback`).
                monitor: c.monitor,
                // **M0c / PLAN-2**: forward the per-peer clipboard
                // opt-in flag. Default-true omission is handled on
                // the `TomlClient` side (see
                // `config_omits_enable_clipboard_to_when_true_on_writeback`).
                enable_clipboard_to: c.enable_clipboard_to,
            })
            .collect();
        self.config.set_clients(clients);
        let authorized_keys = self.authorized_keys.read().expect("lock").clone();
        self.config.set_authorized_keys(authorized_keys);
        if let Err(e) = self.config.write_back() {
            log::warn!("failed to write config: {e}");
        }
    }

    fn handle_config_change(&mut self) {
        for h in self.client_manager.registered_clients() {
            self.remove_client(h);
        }
        for c in self.config.clients() {
            let handle = self.client_manager.add_with_config(c);
            log::info!("added client {handle}");
            let (c, s) = self.client_manager.get_state(handle).unwrap();
            if s.active {
                self.client_manager.deactivate_client(handle);
                self.activate_client(handle);
            }
            self.notify_frontend(FrontendEvent::Created(handle, c, s));
        }
        let release_bind = self.config.release_bind();
        self.capture.set_release_bind(release_bind);
        let authorized_keys = self.config.authorized_fingerprints();
        self.authorized_keys
            .write()
            .unwrap()
            .clone_from(&authorized_keys);
        self.sync_frontend();
    }

    async fn handle_frontend_pending(&mut self) {
        while let Some(event) = self.pending_frontend_events.pop_front() {
            self.frontend_listener.broadcast(event).await;
        }
    }

    fn handle_emulation_event(&mut self, event: EmulationEvent) {
        match event {
            EmulationEvent::ConnectionAttempt { fingerprint } => {
                self.notify_frontend(FrontendEvent::ConnectionAttempt { fingerprint });
            }
            EmulationEvent::Entered {
                addr,
                pos,
                fingerprint,
            } => {
                // check if already registered
                if !self.incoming_conns.contains(&addr) {
                    self.add_incoming(addr, pos, fingerprint.clone());
                    self.notify_frontend(FrontendEvent::DeviceEntered {
                        fingerprint,
                        addr,
                        pos,
                    });
                } else {
                    self.update_incoming(addr, pos, fingerprint);
                }
            }
            EmulationEvent::Disconnected { addr } => {
                // Preserve the capture barrier across transient
                // disconnects so the user can immediately trigger
                // Leave again after the network recovers. We only
                // notify the frontend here; the barrier, conn_info
                // entry, and addr stay in place so:
                //   - the barrier in the capture module keeps
                //     firing CaptureBegin on edge crossings;
                //   - the CaptureBegin handler can look up the
                //     addr via incoming_conn_info and call
                //     send_leave_event(addr);
                //   - the next Enter takes the update_incoming
                //     path (no-op if pos/fp unchanged; remove+add
                //     rebuild otherwise, leaving one orphan).
                //
                // Resource trade-off: each Disconnected leaks one
                // orphan barrier handle. Handles are numbered
                // `ENTER_HANDLE_BEGIN + next_trigger_handle` with
                // cap `u64::MAX/2`, so ordinary workloads will
                // never approach the limit; a GC pass
                // ("sweep orphans when incoming_conns is empty")
                // can be added later if heavy churn becomes a
                // problem.
                //
                // Cleanup still happens organically when pos or
                // fingerprint changes (update_incoming destroys
                // and rebuilds) and when the user disables a
                // client (deactivate_client runs on a different
                // event flow and never reaches this handler).
                log::info!(
                    "peer {addr} transiently disconnected — barrier preserved for fast recovery"
                );
                self.notify_frontend(FrontendEvent::IncomingDisconnected(addr));
            }
            EmulationEvent::PortChanged(port) => match port {
                Ok(port) => {
                    self.port = port;
                    self.notify_frontend(FrontendEvent::PortChanged(port, None));
                }
                Err(e) => self
                    .notify_frontend(FrontendEvent::PortChanged(self.port, Some(format!("{e}")))),
            },
            EmulationEvent::EmulationDisabled => {
                self.emulation_status = Status::Disabled;
                self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
            }
            EmulationEvent::EmulationEnabled => {
                self.emulation_status = Status::Enabled;
                self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
            }
            EmulationEvent::ReleaseNotify => self.capture.release(),
            EmulationEvent::Connected { addr, fingerprint } => {
                self.notify_frontend(FrontendEvent::DeviceConnected { addr, fingerprint });
            }
            EmulationEvent::PeerHello { addr, commit } => {
                // Map the peer's source addr back to its client handle
                // and stamp the commit. Skip if we don't have an
                // outgoing client configured for this peer (incoming-
                // only setup) — there's nowhere to display the version
                // in that case anyway.
                if let Some(handle) = self.client_manager.get_client(addr) {
                    self.client_manager.set_peer_commit(handle, Some(commit));
                    self.broadcast_client(handle);
                }
            }
        }
    }

    fn handle_capture_event(&mut self, event: ICaptureEvent) {
        match event {
            ICaptureEvent::CaptureBegin(handle) => {
                // we entered the capture zone for an incoming connection
                // => notify it that its capture should be released
                if let Some(incoming) = self.incoming_conn_info.get(&handle) {
                    self.emulation.send_leave_event(incoming.addr);
                }
            }
            ICaptureEvent::CaptureDisabled => {
                self.capture_status = Status::Disabled;
                self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
            }
            ICaptureEvent::CaptureEnabled => {
                self.capture_status = Status::Enabled;
                self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
            }
            ICaptureEvent::ClientEntered(handle) => {
                log::info!("entering client {handle} ...");
                self.spawn_hook_command(handle);
            }
            // STEP-M2-2.6: forward the backend monitor snapshot to
            // the IPC frontend and run the `BarrierKey.monitor`
            // reconcile against active clients.
            //
            // Two responsibilities, both bounded by `last_monitors`:
            //  1. Always: re-broadcast the new list as
            //     `FrontendEvent::MonitorsChanged` (GUI source of truth).
            //  2. Only when `last_monitors` is `Some` (i.e. this is
            //     *not* the startup seed): call
            //     `reconcile_monitors_changed` to deactivate
            //     clients whose monitor disappeared, and recreate
            //     barriers for clients whose monitor geometry changed.
            ICaptureEvent::MonitorsChanged(monitors) => {
                let old_monitors = self.last_monitors.replace(monitors.clone());
                let ipc_list = monitors.iter().map(geometry_to_ipc_monitor_info).collect();
                self.notify_frontend(FrontendEvent::MonitorsChanged(ipc_list));
                if let Some(old) = old_monitors {
                    self.reconcile_monitors_changed(&monitors, &old);
                }
            }
        }
    }

    fn handle_resolver_event(&mut self, event: DnsEvent) {
        let handle = match event {
            DnsEvent::Resolving(handle) => {
                self.client_manager.set_resolving(handle, true);
                handle
            }
            DnsEvent::Resolved(handle, hostname, ips) => {
                self.client_manager.set_resolving(handle, false);
                if let Err(e) = &ips {
                    log::warn!("could not resolve {hostname}: {e}");
                }
                let ips = ips.unwrap_or_default();
                self.client_manager.set_dns_ips(handle, ips);
                handle
            }
        };
        self.broadcast_client(handle);
    }

    fn resolve(&self, handle: ClientHandle) {
        if let Some(hostname) = self.client_manager.get_hostname(handle) {
            self.resolver.resolve(handle, hostname);
        }
    }

    fn sync_frontend(&mut self) {
        self.enumerate();
        self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
        self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
        self.notify_frontend(FrontendEvent::PortChanged(self.port, None));
        self.notify_frontend(FrontendEvent::PublicKeyFingerprint(
            self.public_key_fingerprint.clone(),
        ));
        self.notify_frontend(FrontendEvent::QuicConfig {
            idle_timeout_secs: self.quic_idle_timeout_secs,
        });
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
        // STEP-M3-3.x: re-broadcast the latest monitor snapshot on
        // every WS (re)connect. The seed `MonitorsChanged` from
        // `CaptureTask::do_capture` (src/capture.rs:611) fires
        // BEFORE any frontend is listening, so the broadcast in
        // `frontend_listener.broadcast` finds zero subscribers and
        // drops the event on the floor. Without this line the GUI
        // dropdown stays at the lone "Any (back-compat)" option
        // until the first hotplug tick from the 2s polling loop
        // (`MONITOR_POLL_INTERVAL`) lands. Mirror the same defensive
        // re-broadcast as `Enumerate` for clients — the source of
        // truth is `self.last_monitors`, populated by the first
        // `ICaptureEvent::MonitorsChanged` from the capture backend.
        // `None` means the capture backend has not yet produced its
        // seed snapshot (e.g. a still-loading platform monitor
        // service on a slow boot); skip rather than fabricate an
        // empty list, so the GUI keeps the existing "haven't
        // received the first event yet" placeholder behavior
        // (store/index.ts:62-64) instead of flashing an empty
        // dropdown.
        if let Some(monitors) = self.last_monitors.as_ref() {
            let ipc_list = monitors.iter().map(geometry_to_ipc_monitor_info).collect();
            self.notify_frontend(FrontendEvent::MonitorsChanged(ipc_list));
        }
    }

    const ENTER_HANDLE_BEGIN: u64 = u64::MAX / 2 + 1;

    fn add_incoming(&mut self, addr: SocketAddr, pos: Position, fingerprint: String) {
        let handle = Self::ENTER_HANDLE_BEGIN + self.next_trigger_handle;
        self.next_trigger_handle += 1;
        // STEP-1.3: wrap the IPC `pos` into a `BarrierKey` before handing
        // it to `Capture::create`. M1 keeps `monitor / offset / span` at
        // defaults; M3+ will lift the field to come from the frontend.
        let key = crate::capture::to_capture_pos(pos);
        let key = input_capture::BarrierKey::from_pos(key);
        self.capture.create(handle, &key, CaptureType::EnterOnly);
        self.incoming_conns.insert(addr);
        self.incoming_conn_info.insert(
            handle,
            Incoming {
                fingerprint,
                addr,
                pos,
            },
        );
    }

    fn update_incoming(&mut self, addr: SocketAddr, pos: Position, fingerprint: String) {
        let incoming = self
            .incoming_conn_info
            .iter_mut()
            .find(|(_, i)| i.addr == addr)
            .map(|(_, i)| i)
            .expect("no such client");
        let mut changed = false;
        if incoming.fingerprint != fingerprint {
            incoming.fingerprint = fingerprint.clone();
            changed = true;
        }
        if incoming.pos != pos {
            incoming.pos = pos;
            changed = true;
        }
        if changed {
            self.remove_incoming(addr);
            self.add_incoming(addr, pos, fingerprint.clone());
            self.notify_frontend(FrontendEvent::IncomingDisconnected(addr));
            self.notify_frontend(FrontendEvent::DeviceEntered {
                fingerprint,
                addr,
                pos,
            });
        }
    }

    fn remove_incoming(&mut self, addr: SocketAddr) -> Option<SocketAddr> {
        let handle = self
            .incoming_conn_info
            .iter()
            .find(|(_, incoming)| incoming.addr == addr)
            .map(|(k, _)| *k)?;
        self.capture.destroy(handle);
        self.incoming_conns.remove(&addr);
        self.incoming_conn_info
            .remove(&handle)
            .map(|incoming| incoming.addr)
    }

    fn notify_frontend(&mut self, event: FrontendEvent) {
        self.pending_frontend_events.push_back(event);
        self.frontend_event_pending.notify_one();
    }

    fn add_authorized_key(&mut self, desc: String, fp: String) {
        self.authorized_keys.write().expect("lock").insert(fp, desc);
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
    }

    fn remove_authorized_key(&mut self, fp: String) {
        self.authorized_keys.write().expect("lock").remove(&fp);
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
    }

    fn enumerate(&mut self) {
        let clients = self.client_manager.get_client_states();
        self.notify_frontend(FrontendEvent::Enumerate(clients));
    }

    fn add_client(&mut self) {
        let handle = self.client_manager.add_client();
        log::info!("added client {handle}");
        let (c, s) = self.client_manager.get_state(handle).unwrap();
        self.notify_frontend(FrontendEvent::Created(handle, c, s));
    }

    fn set_client_active(&mut self, handle: ClientHandle, active: bool) {
        if active {
            self.activate_client(handle);
        } else {
            self.deactivate_client(handle);
        }
    }

    fn deactivate_client(&mut self, handle: ClientHandle) {
        log::debug!("deactivating client {handle}");
        if self.client_manager.deactivate_client(handle) {
            self.capture.destroy(handle);
            self.broadcast_client(handle);
            log::info!("deactivated client {handle}");
        }
    }

    /// **sleep-week-bug**: 屏幕睡眠 / monitor 暂时消失时,只销毁 capture
    /// barrier,**不动** `s.active`、`peers[active_addr]`、`active_addr`。
    ///
    /// **为什么不直接 `deactivate_client`**:macOS 屏幕关闭(屏幕 dim /
    /// 关屏)触发的 `CGDisplayReconfiguration` 回调会让 capture backend
    /// 暂时上报 `monitor list = []`,然后屏幕唤醒后 `monitor list` 又
    /// 恢复。如果走 `deactivate_client`,会把 `s.active` 设成 `false`,
    /// 而 `s.active` 一旦为 `false` 既有的 `activate_client` 之外的任何
    /// 路径都不会把它翻回来 —— 屏幕唤醒后用户就再也无法 capture 了
    /// (`capture.create` 没被任何事件触发,`peers[addr]` 留着但前端仍
    /// 显示客户端未激活)。
    ///
    /// **保留 QUIC 连接的意义**:detach 期间 `peers[active_addr]`、
    /// `s.active_addr`、`ping_heartbeat_task`、`pong_health_watchdog`
    /// 全部继续运行,所以被控端每 500ms 仍然能收到 Ping,它的
    /// `last_response[addr]` 不会被 `> 1s` 触发超时而被 `close_with_wake_code`。
    /// 屏幕唤醒后 monitor 恢复,Phase B 的 `reattach_capture` 直接重建
    /// barrier,用户立即可以跨屏 —— 没有 reconnect 等待期。
    ///
    /// **为什么不需要 broadcast_state**:前端 `state.active` 没变(还是 `true`),
    /// 只会清掉 `invalidReason` badge —— 这个 badge 由 Phase B 的
    /// `reattach_capture` 调 `broadcast_client` 顺带清掉。
    ///
    /// **为什么这里仍要 `BindingInvalid`**:用户必须能在 GUI 上看到
    /// "⚠ invalid" 提示,知道当前 monitor 不可用,避免他把鼠标移到屏幕边
    /// 缘却发现没反应。
    fn detach_capture(&mut self, handle: ClientHandle, reason: &str) {
        log::info!("service: monitor-driven detach handle={handle} reason={reason:?}");
        self.capture.destroy(handle);
        self.notify_frontend(FrontendEvent::BindingInvalid(handle, reason.to_string()));
    }

    /// **sleep-week-bug**: 屏幕唤醒 / monitor 重新出现时,重建 capture
    /// barrier 让用户立刻可以跨屏。
    ///
    /// **调用前置条件**:本方法只在 `recover_monitors` 返回非空 handle 时
    /// 被调用,而 `recover_monitors` 只在 `(was_present=false, is_present=true)`
    /// 时返回该 handle —— 也就是说,这个 handle **之前**一定经过 Phase A
    /// 的 `detach_capture`,barrier **已经被 destroy 了**。所以这里直接
    /// `create` 即可,**绝不**再调 `destroy`,否则
    /// `input_capture::Capture::destroy` 会在 `id_map.remove(&id)` 拿到
    /// `None` 时 `.expect("no barrier key for this handle")` panic
    /// (现场日志:`thread 'main' panicked at input-capture/src/lib.rs:172:14`)。
    ///
    /// **为什么不调 `capture.dial(handle)`**:本方法的调用语境是"detached
    /// 客户端的 monitor 恢复",此时 `peers[active_addr]` 通常仍有有效
    /// entry(屏幕睡眠 < 30 min 时 QUIC idle_timeout 不会触发,slave
    /// `last_response` 也不会超时)。调 `capture.dial` 会 `spawn_local` 一个
    /// 新的 `connect_to_handle`,与仍存活的旧 peer 在 `peers[addr]` 上抢同
    /// 一槽位(主分支 `1a111aa` 就是死在这里),而旧 supervisor 的
    /// `peers.lock().await.remove(&addr)` 没有 `Arc::ptr_eq` 保护
    /// (`f87ad44` 回退了 `299fb54`),新 peer 的 entry 会被旧 supervisor
    /// 误删。
    ///
    /// **QUIC 连接已死的边界情况**:睡眠 > 30 min 时 QUIC idle_timeout 自
    /// 然超时,master `should_retry_after_close(TimedOut)` 返回 true,既有的
    /// supervisor redial 链路自动处理。屏幕唤醒后 `peers[addr]` 可能为空,
    /// 但用户跨屏触发 `send()` 时会走 redial 路径(`active_addr` 仍是 `Some`)。
    ///
    /// **不调 `active = true`**:`s.active` 在 detach 期间**没被**改过
    ///(detach 只动 barrier),所以这里也是 `true`。
    fn reattach_capture(&mut self, handle: ClientHandle) {
        let Some(key) = self.client_manager.get_key(handle) else {
            return;
        };
        log::info!("service: monitor-driven reattach handle={handle} (monitor re-appeared)");
        // 直接 create —— Phase A 的 `detach_capture` 已经把这个 handle 的
        // barrier destroy 掉了;再 destroy 会 panic。barrier 的 id_map
        // 此刻一定不含 `handle`,所以这里 `create` 一定不是重复创建。
        self.capture.create(handle, &key, CaptureType::Default);
        // broadcast 让前端的 invalidReason badge 消失(store/index.ts:248 写入,
        // mergeClient 在下一个 State 事件里清掉)。注意这不是 State(active=false)
        // 那种广播 —— `s.active` 一直是 `true`,只是清理 UX 标记。
        self.broadcast_client(handle);
    }

    fn activate_client(&mut self, handle: ClientHandle) {
        log::debug!("activating client {handle}");

        /* resolve dns on activate */
        self.resolve(handle);

        /* deactivate potential other client at this BarrierKey */
        let Some(key) = self.client_manager.get_key(handle) else {
            return;
        };

        if let Some(other) = self.client_manager.client_at(&key) {
            if other != handle {
                self.deactivate_client(other);
            }
        }

        /* activate the client */
        if self.client_manager.activate_client(handle) {
            /* notify capture and frontends */
            self.capture.create(handle, &key, CaptureType::Default);
            self.broadcast_client(handle);
            log::info!("activated client {handle} ({key:?})");

            // Fire-and-forget dial so an active client establishes
            // its connection immediately, even if no mouse movement
            // reaches the screen edge. Independent of the
            // capture-triggered dial path; `connect.rs` deduplicates
            // via `RetryState` + the connecting set.
            //
            // Order matters: must run after `client_manager.
            // activate_client` returns true (otherwise the handle
            // isn't active yet) and after `broadcast_client` so the
            // GUI sees the client as `active` while the dial is
            // already in flight.
            self.capture.dial(handle);
        }
    }

    fn change_port(&mut self, port: u16) {
        if self.port != port {
            self.emulation.request_port_change(port);
        } else {
            self.notify_frontend(FrontendEvent::PortChanged(self.port, None));
        }
    }

    /// Persist a new QUIC `idle_timeout_secs` value and echo it back
    /// to the frontend. The running endpoint is **not** rebuilt —
    /// the value will only take effect on the next daemon restart
    /// (see [`FrontendRequest::SetQuicIdleTimeout`]).
    ///
    /// Clamping + write-back mirrors [`Config::set_quic_idle_timeout`]:
    /// the floor of 5s prevents a stale GUI write from making the
    /// next restart panic in quinn.
    fn set_quic_idle_timeout(&mut self, secs: u64) {
        const MIN_SECS: u64 = 5;
        let secs = secs.max(MIN_SECS);
        if secs == self.quic_idle_timeout_secs {
            // No-op; still echo so the GUI re-syncs after a value-races-restart.
            self.notify_frontend(FrontendEvent::QuicConfig {
                idle_timeout_secs: self.quic_idle_timeout_secs,
            });
            return;
        }
        self.config.set_quic_idle_timeout(secs);
        self.quic_idle_timeout_secs = secs;
        if let Err(e) = self.config.write_back() {
            log::warn!("failed to persist quic.idle_timeout_secs: {e}");
        }
        self.notify_frontend(FrontendEvent::QuicConfig {
            idle_timeout_secs: secs,
        });
    }

    fn remove_client(&mut self, handle: ClientHandle) {
        if self
            .client_manager
            .remove_client(handle)
            .map(|(_, s)| s.active)
            .unwrap_or(false)
        {
            self.capture.destroy(handle);
        }
        self.notify_frontend(FrontendEvent::Deleted(handle));
    }

    fn update_fix_ips(&mut self, handle: ClientHandle, fix_ips: Vec<IpAddr>) {
        self.client_manager.set_fix_ips(handle, fix_ips);
        self.broadcast_client(handle);
    }

    fn update_hostname(&mut self, handle: ClientHandle, hostname: Option<String>) {
        log::info!("hostname changed: {hostname:?}");
        if self.client_manager.set_hostname(handle, hostname.clone()) {
            self.resolve(handle);
        }
        self.broadcast_client(handle);
    }

    fn update_port(&mut self, handle: ClientHandle, port: u16) {
        self.client_manager.set_port(handle, port);
        self.broadcast_client(handle);
    }

    fn update_pos(&mut self, handle: ClientHandle, pos: Position) {
        // update state in event input emulator & input capture
        if self.client_manager.set_pos(handle, pos) {
            self.deactivate_client(handle);
            self.activate_client(handle);
        }
        self.broadcast_client(handle);
    }

    /// **M3 — update the monitor binding** of a client. Mirrors
    /// `update_pos`: when the binding changes AND the client is
    /// active, rebuild the capture barrier so the new
    /// `BarrierKey.monitor` reaches `Capture::create`. The
    /// `activate_client` call re-reads the BarrierKey from the
    /// `ClientManager` (which now reflects the new monitor), so the
    /// barrier is scoped to the chosen monitor without any explicit
    /// "old key vs. new key" bookkeeping here.
    ///
    /// `monitor = None` clears the binding (legacy "any monitor");
    /// `Some(id)` re-binds. If `id` doesn't match a currently
    /// enumerated monitor, the binding stays put — the user can
    /// re-pick after the next `MonitorsChanged` event. (We don't
    /// validate against the latest monitor list here because the
    /// user may legitimately want to set a binding slightly ahead of
    /// a pending plug-in event.)
    fn update_monitor(&mut self, handle: ClientHandle, monitor: Option<String>) {
        if self.client_manager.set_monitor(handle, monitor) {
            self.deactivate_client(handle);
            self.activate_client(handle);
        }
        self.broadcast_client(handle);
    }

    fn update_enter_hook(&mut self, handle: ClientHandle, enter_hook: Option<String>) {
        self.client_manager.set_enter_hook(handle, enter_hook);
        self.broadcast_client(handle);
    }

    /// Per-input-event transport selection (datagram vs reliable
    /// stream) for the given outgoing client. Sender-side only — the
    /// receiver doesn't see this preference. Mirrors the
    /// `update_enter_hook` flow: setter → broadcast → save_config.
    /// Mid-session hot reload is not yet wired — the transport
    /// choice is locked in at dial time via `PeerSession::with_config`.
    fn update_input_channels(&mut self, handle: ClientHandle, cfg: InputChannelConfig) {
        if self.client_manager.set_input_channels(handle, cfg) {
            self.broadcast_client(handle);
        }
    }

    /// **M0c / PLAN-2** — handler for
    /// [`FrontendRequest::SetClipboardConfig`]. Persists the
    /// daemon-global `[clipboard]` section to TOML. The runtime
    /// effect (`service::clipboard::apply_config` gating the
    /// dispatcher on `ignore_*` / `auto_accept_files` / etc.) is
    /// M1a; for M0c we just persist + log so the value survives
    /// restart. The runtime will pick the new value up on the next
    /// daemon restart (matches the
    /// `FrontendRequest::SetQuicIdleTimeout` contract).
    fn set_clipboard_config(&mut self, cfg: lan_mouse_ipc::ClipboardConfig) {
        self.config.set_clipboard_config(cfg.clone());
        if let Err(e) = self.config.write_back() {
            log::warn!("failed to persist [clipboard] section: {e}");
        }
        log::info!(
            "clipboard config updated (M0c — runtime effect wired in M1a): \
             auto_accept_files={}, ignore_text={}, ignore_images={}, ignore_files={}, \
             accept_dir={:?}",
            cfg.auto_accept_files,
            cfg.ignore_text,
            cfg.ignore_images,
            cfg.ignore_files,
            cfg.accept_dir,
        );
    }

    /// **M0c / PLAN-2** — handler for
    /// [`FrontendRequest::SetEnableClipboardTo`]. Updates the
    /// per-peer `ClientConfig.enable_clipboard_to` field and saves
    /// to TOML. Echoes the new state via the standard `FrontendEvent
    /// ::State` push so the GUI re-syncs (matches the M3
    /// `set_monitor` / M0 `set_input_channels` contract).
    fn set_enable_clipboard_to(&mut self, handle: ClientHandle, enable: bool) {
        if self.client_manager.set_enable_clipboard_to(handle, enable) {
            self.broadcast_client(handle);
        }
    }

    fn broadcast_client(&mut self, handle: ClientHandle) {
        let event = self
            .client_manager
            .get_state(handle)
            .map(|(c, s)| FrontendEvent::State(handle, c, s))
            .unwrap_or(FrontendEvent::NoSuchClient(handle));
        self.notify_frontend(event);
    }

    /// **STEP-M2-2.6 / sleep-week-bug**: reconcile all clients against a
    /// new monitor snapshot.
    ///
    /// **Three-phase state machine** for each `(handle, key)` pair:
    ///
    /// | `(was_present, is_present)` | 分支                       | 动作                                                  |
    /// |------------------------------|---------------------------|------------------------------------------------------|
    /// | `(true, false)`              | **Phase A — detach**      | `detach_capture` (销毁 barrier,保持 `s.active=true`) |
    /// | `(false, true)`              | **Phase B — recover**     | `reattach_capture` (重建 barrier,**不**触发新 dial)   |
    /// | `(true, true)` + geometry Δ | **Phase C — recreate**    | `deactivate + activate` round-trip                    |
    /// | `(true, true)` + same geom   | no-op                                                      |
    /// | `(false, false)`             | no-op                                                      |
    ///
    /// **为什么是三相位而不是 `activate_client` 直接重激活**(主分支 `1a111aa`
    /// 走的就是 activate_client 路径):
    ///
    /// 1. `activate_client` → `capture.dial(handle)` → `spawn_local(connect_to_handle)`,
    ///    会用同一 `peers[addr]` 槽位覆盖仍存活的旧 peer;旧 supervisor 的
    ///    `peer.run()` 返回时 `peers.lock().await.remove(&addr)` 没有
    ///    `Arc::ptr_eq` 保护(`f87ad44` 回退了 `299fb54`),新 peer 的 entry
    ///    会被旧 supervisor 误删 → 链路永久死掉。
    /// 2. `activate_client` 会调 `client_manager.activate_client` 检查 `s.active`
    ///    并设成 `true`。我们 detach 期间没动 `s.active`,这里也无需再设。
    /// 3. `activate_client` 会触发新 dial,与睡眠期间**仍活着的** QUIC 连接
    ///    抢同一 peer entry。
    ///
    /// **Phase B 的 `reattach_capture` 只重建 barrier**:QUIC 连接若仍存活
    /// 直接复用,若已死则由既有 supervisor redial 在用户首次 `send()` 时
    /// 处理(详见 `reattach_capture` 的 docstring)。
    ///
    /// **为什么 snapshot 用 `registered_clients()` 而不是 `active_clients()`**:
    /// Phase B 需要看到 detached 后(但 `s.active` 仍是 `true`)的 handle,
    /// 而 `active_clients()` 在 detach 后仍包含它们 —— 但为了避免未来在
    /// `detach_capture` 里也改 `s.active` 的歧义,我们显式用 `registered_clients()`
    /// snapshot 全部 bindings(已 deactivated 的 `monitor=Some` 客户端也能被
    /// 找到),Phase A/B 内部再用 `active_clients().contains(&handle)` 过滤。
    ///
    /// **M1 default (monitor = None)**:这些 binding 在 `reconcile_monitors` /
    /// `recover_monitors` 里都被 `key.monitor.as_ref()` 检查跳过 —— 没有
    /// `monitor` 字段的客户端从不进入 reconcile,既不会被 deactivate 也不
    /// 需要 recover。
    ///
    /// `last_monitors` is updated by `handle_capture_event` *before* this is
    /// called; this method compares the new list against the previous one.
    fn reconcile_monitors_changed(
        &mut self,
        new_monitors: &[GeometryMonitorInfo],
        old_monitors: &[GeometryMonitorInfo],
    ) {
        // Snapshot **all** bindings (active ∪ inactive). We use
        // `registered_clients()` rather than `active_clients()` so the
        // snapshot includes any handle that has a monitor binding, even
        // if a future refactor makes `detach_capture` also clear
        // `s.active`. The pure helpers `reconcile_monitors` and
        // `recover_monitors` filter by `key.monitor.is_some()`; Phase A
        // and Phase B additionally filter by the active-client set
        // before mutating.
        let mut all_bindings: Vec<(ClientHandle, BarrierKey)> = Vec::new();
        for handle in self.client_manager.registered_clients() {
            if let Some(key) = self.client_manager.get_key(handle) {
                all_bindings.push((handle, key));
            }
        }

        // === Phase A: detach (monitor was present, now gone) ===
        //
        // Replaces the previous `deactivate_client` call. We intentionally
        // do NOT change `s.active` — see the function docstring table
        // and `detach_capture`'s docstring for the rationale.
        let deactivations = reconcile_monitors(&all_bindings, new_monitors, old_monitors);
        let active_set: std::collections::HashSet<ClientHandle> =
            self.client_manager.active_clients().into_iter().collect();
        for (handle, reason) in deactivations {
            if !active_set.contains(&handle) {
                // The user already deactivated it. Skip — no barrier to
                // detach (it was destroyed by the user's deactivate),
                // and re-detaching it would be a no-op that still
                // surfaces a misleading "⚠ invalid" badge in the GUI.
                continue;
            }
            self.detach_capture(handle, &reason);
        }

        // === Phase B: recover (monitor was gone, now back) ===
        //
        // New since sleep-week-bug. Reattaches the barrier for any active
        // handle whose bound monitor reappeared. **Does not** call
        // `capture.dial` — see function docstring.
        let recoveries = recover_monitors(&all_bindings, new_monitors, old_monitors);
        for handle in recoveries {
            if !active_set.contains(&handle) {
                // The user explicitly turned this client off while it
                // was detached; do not silently reattach a barrier the
                // user does not want.
                continue;
            }
            // Idempotent — also handles the (rare) case where Phase A
            // and Phase B both touch the same handle on the same
            // MonitorsChanged tick (e.g. transient flicker).
            self.reattach_capture(handle);
        }

        // === Phase C: geometry recreate (unchanged) ===
        //
        // Uses the active-only snapshot because Phase C is the legacy
        // "monitor still there but its rectangle moved" path; an inactive
        // client has no barrier to recreate.
        let mut active_bindings: Vec<(ClientHandle, BarrierKey)> = Vec::new();
        for handle in self.client_manager.active_clients() {
            if let Some(key) = self.client_manager.get_key(handle) {
                active_bindings.push((handle, key));
            }
        }
        let recreations = recreate_monitors(&active_bindings, new_monitors, old_monitors);
        for (handle, _old_key, _new_key) in recreations {
            log::info!(
                "service: monitor-driven barrier recreate handle={handle} \
                 (monitor geometry changed)"
            );
            // Same round-trip as `update_pos`: deactivate + activate.
            // `activate_client` re-reads the BarrierKey from the
            // client_manager (which we haven't mutated here, so the
            // "new" key equals the "old" key in M2 — but the
            // destroy/create cycle still runs through capture to
            // mirror what `update_pos` does).
            self.deactivate_client(handle);
            self.activate_client(handle);
        }
    }

    fn spawn_hook_command(&self, handle: ClientHandle) {
        let Some(cmd) = self.client_manager.get_enter_cmd(handle) else {
            return;
        };
        tokio::task::spawn_local(async move {
            log::info!("spawning command!");
            let mut child = match Command::new("sh").arg("-c").arg(cmd.as_str()).spawn() {
                Ok(c) => c,
                Err(e) => {
                    log::warn!("could not execute cmd: {e}");
                    return;
                }
            };
            match child.wait().await {
                Ok(s) => {
                    if s.success() {
                        log::info!("{cmd} exited successfully");
                    } else {
                        log::warn!("{cmd} exited with {s}");
                    }
                }
                Err(e) => log::warn!("{cmd}: {e}"),
            }
        });
    }

    /// **PLAN-2 / M1a STEP-1a.4** — 500 ms tick handler.
    ///
    /// 1. Read the current clipboard text. `None` → no-op (clipboard
    ///    holds non-text content, e.g. an image — skip this tick).
    /// 2. Compare to `clipboard_last_text`. Equal → no-op.
    /// 3. SHA-256 the text. If the hash is in the LRU → no-op
    ///    (loopback defence: this hash was *we* who wrote it
    ///    recently; pushing it again is wasted work).
    /// 4. Mark LRU, update `last_text`, broadcast `ClipboardText` to
    ///    every active peer with `enable_clipboard_to = true`,
    ///    push `FrontendEvent::ClipboardState { last_source: None }`.
    ///
    /// **Why a 500 ms tick**: matches PLAN §3 M1a "macOS 实现
    /// ... 500ms tick" / "Linux 500 ms tick" cadence. Fast enough
    /// to feel instant to a user copying text; slow enough that the
    /// backend read (1-3 ms for pbcopy / xclip / NSPasteboard) is
    /// negligible.
    ///
    /// **Why we don't drop the read on a hash match**: the LRU
    /// check happens *after* the read because the read is what
    /// surfaces the new text — there's no way to detect "the
    /// clipboard changed" without reading it. The hash check is
    /// the *dedup* layer: it prevents re-broadcasting a value we
    /// already broadcast this minute.
    async fn handle_clipboard_tick(&mut self) {
        let Some(backend) = self.clipboard_backend.as_mut() else {
            return;
        };
        let new_text = match backend.current_text() {
            Some(t) => t,
            None => return,
        };
        if Some(&new_text) == self.clipboard_last_text.as_ref() {
            return;
        }
        let sha = sha256_of(&new_text);
        if self.clipboard_lru.contains(&sha) {
            // Loopback — the LRU already holds this hash (e.g. we
            // just applied an inbound `set_text` that wrote this
            // value). Do not re-broadcast.
            log::info!(
                "clipboard tick: LRU loopback hit sha={} ({} bytes), skipping broadcast",
                short_hex(&sha),
                new_text.len()
            );
            self.clipboard_last_text = Some(new_text);
            return;
        }
        // New content — mark + broadcast.
        log::info!(
            "clipboard change detected: {} bytes (sha={})",
            new_text.len(),
            short_hex(&sha)
        );
        self.clipboard_lru.push(sha);
        self.clipboard_last_text = Some(new_text.clone());
        let event = ProtoEvent::ClipboardText(ClipboardText {
            fingerprint: sha,
            sha256: sha,
            size: new_text.len() as u64,
            // **M1a inline-only**: ≤ 1 KiB carried inline. Larger
            // payloads (M1b) would set `content_inline: None` and
            // rely on the HTTP/3 GET path. The `if let Some(content)`
            // is defensive: a clipboard value > 1 KiB would still be
            // pushed inline, but the receiver's `route_input` would
            // route the var-codec event to StreamC regardless of size.
            content_inline: Some(new_text.into_bytes()),
        });
        let mut recipients = 0usize;
        self.broadcast_clipboard_event(event, &mut recipients).await;
        if recipients == 0 {
            log::warn!(
                "clipboard dispatched to 0 peers (sha={}); peer gate filtered all clients — \
                 check `enable_clipboard_to` in TOML and that the connection is active",
                short_hex(&sha)
            );
        } else {
            log::info!(
                "clipboard dispatched to {} peer(s) (sha={})",
                recipients,
                short_hex(&sha)
            );
        }
        let now_ms = unix_now_ms();
        self.last_text_ts_ms = Some(now_ms);
        self.last_clipboard_source = None;
        self.notify_frontend(FrontendEvent::ClipboardState {
            last_text_ts: self.last_text_ts_ms,
            last_image_ts: self.last_image_ts_ms,
            last_file_ts: self.last_file_ts_ms,
            last_source: None,
        });
    }

    /// **PLAN-2 / M1a STEP-1a.4** — inbound `ClipboardText` from a
    /// peer (server or client side, see the field doc on
    /// `clipboard_inbound_rx`).
    ///
    /// Steps:
    /// 1. Drop the event if the LRU already holds the fingerprint
    ///    (loopback — we just wrote this content locally and a
    ///    peer's echo made it back).
    /// 2. Apply the inline bytes to the local OS clipboard via
    ///    `backend.set_text` (or skip if the payload is
    ///    `content_inline = None` — M1a only supports the inline
    ///    path; M1b adds the HTTP/3 GET fallback).
    /// 3. Mark LRU, update `last_text` (force re-read on the next
    ///    tick so the broadcast loop sees the new value and
    ///    de-dups).
    /// 4. Notify the frontend with `last_source: Some(<addr>)` so
    ///    the GUI can render the "clipboard was just changed by
    ///    <peer>" amber highlight (PLAN §3 M4 STEP-4.4 preview).
    fn handle_clipboard_inbound(&mut self, (addr, event): (SocketAddr, ProtoEvent)) {
        let Some(backend) = self.clipboard_backend.as_mut() else {
            return;
        };
        let ProtoEvent::ClipboardText(ct) = event else {
            // Only text is wired in M1a. Image / Files / FileTransfer
            // events flow through `clipboard_inbound_rx` once M2a /
            // M3a wire their own inbound arms in the dispatcher.
            return;
        };
        if self.clipboard_lru.contains(&ct.sha256) {
            log::debug!(
                "clipboard inbound: skipping loopback sha={}",
                short_hex(&ct.sha256)
            );
            return;
        }
        let content = match &ct.content_inline {
            Some(bytes) => bytes,
            None => {
                // M1a limitation: metadata-only (no inline) payloads
                // can't be applied without the HTTP/3 GET path which
                // is M1b. Log + skip; the receiver's clipboard
                // remains on the pre-push value.
                log::warn!(
                    "clipboard inbound: sha={} has no inline payload (M1a only supports ≤ 1 KiB inline); skipping",
                    short_hex(&ct.sha256)
                );
                return;
            }
        };
        // Apply to the local OS clipboard. The text is UTF-8 by
        // wire convention; if a peer sent non-UTF-8 bytes (corrupt
        // / older daemon) the lossy replace keeps the daemon from
        // panicking — the user will see replacement characters.
        let text = String::from_utf8_lossy(content);
        if let Err(e) = backend.set_text(&text) {
            log::warn!("clipboard inbound: set_text failed: {e}");
            return;
        }
        self.clipboard_lru.push(ct.sha256);
        // Force the next tick to re-read so `last_text` updates to
        // the freshly-written value; otherwise a stale `last_text`
        // would suppress the change-detection that triggers
        // `set_text` on the next inbound push with identical text
        // (an edge case, but the dispatcher must be correct).
        self.clipboard_last_text = None;
        let now_ms = unix_now_ms();
        self.last_text_ts_ms = Some(now_ms);
        self.last_clipboard_source = Some(addr);
        self.notify_frontend(FrontendEvent::ClipboardState {
            last_text_ts: self.last_text_ts_ms,
            last_image_ts: self.last_image_ts_ms,
            last_file_ts: self.last_file_ts_ms,
            last_source: Some(format!("{addr}")),
        });
        log::info!(
            "clipboard inbound: applied {} bytes from {addr} (sha={})",
            content.len(),
            short_hex(&ct.sha256)
        );
    }

    /// **PLAN-2 / M1a follow-up #1** — recover copies the user
    /// made during the dial window.
    ///
    /// Triggered by `clipboard_push_notify_rx` whenever a peer's
    /// `active_addr` transitions from `None → Some(addr)`. Reads
    /// the current local clipboard once, dispatches through the
    /// shared `broadcast_clipboard_event` helper. Mirrors the tick
    /// path closely so the GUI / state semantics match a "normal"
    /// local-origin push (timestamp + `last_source = None`).
    ///
    /// **Why a separate method instead of calling
    /// `handle_clipboard_tick`**: the tick path is gated by a 500 ms
    /// `tokio::time::Interval` and would have to wait for the next
    /// tick to fire — adding up to 500 ms of latency to a user copy
    /// that already waited 5–35 s for the dial window to close.
    /// Firing the push inline collapses the post-dial latency to
    /// "next loop iteration + ~5 ms backend read".
    ///
    /// **Why the LRU check is bypassed** (unlike the tick path):
    /// the tick at `handle_clipboard_tick` writes the hash to the
    /// LRU *before* `broadcast_clipboard_event`, even when the
    /// broadcast is suppressed by the `active_addr.is_none()` gate
    /// (`recipients == 0`). That "optimistic" LRU push is correct
    /// for tick-vs-tick dedup, but it incorrectly suppresses THIS
    /// push — the very push that's supposed to recover the copies
    /// the tick couldn't deliver. The fix is to bypass the LRU
    /// check here; the inbound arm's LRU check still applies
    /// against the push we just made (so a peer's echo round-trip
    /// is deduped correctly). Net effect: exactly one outbound
    /// `ClipboardText` per `set_active_addr` transition.
    ///
    /// **Idempotency vs the tick**: even though we bypass the LRU
    /// check, the `clipboard_last_text` write + the just-pushed LRU
    /// entry still cause the next tick to dedup via either path —
    /// net effect is exactly one outbound `ClipboardText`.
    ///
    /// **No-op when backend unavailable**: same `clipboard_backend
    /// .is_none()` guard as the tick path — the channel still
    /// drains, just nothing is dispatched.
    async fn handle_clipboard_recover_push(&mut self, handle: ClientHandle) {
        let Some(backend) = self.clipboard_backend.as_mut() else {
            return;
        };
        let new_text = match backend.current_text() {
            Some(t) => t,
            None => return,
        };
        let sha = sha256_of(&new_text);
        // Intentionally **not** checking `clipboard_lru.contains(&sha)`
        // here — see the docstring above for why the tick's
        // pre-broadcast LRU push would otherwise suppress this
        // recover push.
        log::info!(
            "clipboard recover push: peer handle={handle} just became active — \
             pushing {} bytes (sha={})",
            new_text.len(),
            short_hex(&sha)
        );
        self.clipboard_lru.push(sha);
        self.clipboard_last_text = Some(new_text.clone());
        let event = ProtoEvent::ClipboardText(ClipboardText {
            fingerprint: sha,
            sha256: sha,
            size: new_text.len() as u64,
            content_inline: Some(new_text.into_bytes()),
        });
        let mut recipients = 0usize;
        self.broadcast_clipboard_event(event, &mut recipients).await;
        if recipients == 0 {
            log::warn!(
                "clipboard recover push: dispatched to 0 peers (sha={})",
                short_hex(&sha)
            );
        } else {
            log::info!(
                "clipboard recover push: dispatched to {} peer(s) (sha={})",
                recipients,
                short_hex(&sha)
            );
        }
        let now_ms = unix_now_ms();
        self.last_text_ts_ms = Some(now_ms);
        self.last_clipboard_source = None;
        self.notify_frontend(FrontendEvent::ClipboardState {
            last_text_ts: self.last_text_ts_ms,
            last_image_ts: self.last_image_ts_ms,
            last_file_ts: self.last_file_ts_ms,
            last_source: None,
        });
    }

    /// **PLAN-2 / M1a STEP-1a.4** — broadcast a clipboard event to
    /// every active peer with `enable_clipboard_to = true`.
    ///
    /// Fire-and-forget: `Capture::send_event` queues the request on
    /// the capture task's `request_tx`; the actual `conn.send`
    /// happens off-thread. Per-peer send failures (peer
    /// disconnected mid-tick) are logged at `warn` inside
    /// `CaptureTask` but do not propagate here.
    async fn broadcast_clipboard_event(&self, event: ProtoEvent, recipients: &mut usize) {
        let mut skipped_disabled = 0usize;
        let mut skipped_inactive = 0usize;
        let mut skipped_no_addr = 0usize;
        for (handle, cfg, state) in self.client_manager.get_client_states() {
            if !cfg.enable_clipboard_to {
                log::info!(
                    "clipboard broadcast: skipping peer handle={} (enable_clipboard_to=false)",
                    handle
                );
                skipped_disabled += 1;
                continue;
            }
            if !state.active {
                log::info!(
                    "clipboard broadcast: skipping peer handle={} (client not active yet)",
                    handle
                );
                skipped_inactive += 1;
                continue;
            }
            if state.active_addr.is_none() {
                log::info!(
                    "clipboard broadcast: skipping peer handle={} (no active_addr — handshake incomplete?)",
                    handle
                );
                skipped_no_addr += 1;
                continue;
            }
            log::info!(
                "clipboard broadcast: -> peer handle={} active_addr={:?}",
                handle,
                state.active_addr
            );
            self.capture.send_event(event.clone(), handle);
            *recipients += 1;
        }
        if skipped_disabled + skipped_inactive + skipped_no_addr > 0 {
            log::info!(
                "clipboard broadcast gate summary: skipped disabled={} inactive={} no_addr={}",
                skipped_disabled,
                skipped_inactive,
                skipped_no_addr
            );
        }
    }
}

/// **PLAN-2 / M1a STEP-1a.4** — SHA-256 → `[u8; 32]` helper.
/// The clipboard dispatcher's fingerprint is the SHA-256 of the
/// text bytes (the same value the receiver uses as `ClipboardText
/// ::sha256`). A truncated 8-byte "fingerprint" is a M1b
/// optimisation; M1a uses the full hash.
fn sha256_of(text: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    let out = hasher.finalize();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&out);
    arr
}

/// Compact hex prefix for log lines (first 4 bytes = 8 hex chars).
fn short_hex(b: &[u8; 32]) -> String {
    let mut s = String::with_capacity(8);
    for byte in &b[..4] {
        s.push_str(&format!("{:02x}", byte));
    }
    s
}

/// **PLAN-2 / M1a STEP-1a.4** — milliseconds since the UNIX
/// epoch. Used for the `last_text_ts` / `last_image_ts` /
/// `last_file_ts` fields of `FrontendEvent::ClipboardState`.
/// Returns `0` on clock-read failure (the dispatch loop will
/// still publish a state event; the frontend treats 0 as "epoch"
/// which is well-defined if unusual).
fn unix_now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// **STEP-M2-2.6**: pure helper that turns an internal
/// `input_capture::geometry::MonitorInfo` into the on-wire
/// `lan_mouse_ipc::MonitorInfo` mirror.
///
/// The two types are field-for-field identical — STEP-2.1 chose
/// `pub use` over a generated From trait precisely because the
/// type would evolve independently from the wire schema, and we
/// didn't want a build.rs for one field. The conversion is a
/// straight-line field copy; if either type grows a new field the
/// other must grow the same field or this function (and the two
/// `monitor_info_*_tests` modules on both sides) will diverge.
fn geometry_to_ipc_monitor_info(m: &GeometryMonitorInfo) -> IpcMonitorInfo {
    IpcMonitorInfo {
        id: m.id.clone(),
        name: m.name.clone(),
        position: m.position,
        size: m.size,
        primary: m.primary,
        scale: m.scale,
    }
}

/// **STEP-M2-2.6**: pure helper — given the list of active
/// `(handle, BarrierKey)` pairs and an old + new monitor snapshot,
/// return the handles whose bound monitor disappeared in the new
/// list.
///
/// Pure function (no `&mut self`, no side effects) so the four
/// matrix cases the PLAN §M2 STEP-2.6 / §8 list — remove / geometry
/// change / no-op / startup seed — can be unit-tested in isolation
/// without standing up the full `Service`.
///
/// **M2 caveat**: `BarrierKey.monitor` is always `None` (the M1
/// default) because `ClientConfig.monitor` does not exist yet; M3
/// will populate it from `FrontendRequest::UpdateMonitor`. Until
/// then this function returns an empty `Vec` for every input —
/// the infrastructure is here, ready to fire as soon as clients
/// carry a `Some(id)` binding.
fn reconcile_monitors(
    active: &[(ClientHandle, BarrierKey)],
    new_monitors: &[GeometryMonitorInfo],
    old_monitors: &[GeometryMonitorInfo],
) -> Vec<(ClientHandle, String)> {
    let mut out = Vec::new();
    for (handle, key) in active {
        let Some(monitor_id) = key.monitor.as_ref() else {
            // M1 default: no binding. Nothing to reconcile.
            continue;
        };
        let was_present = old_monitors.iter().any(|m| m.id == *monitor_id);
        let is_present = new_monitors.iter().any(|m| m.id == *monitor_id);
        if was_present && !is_present {
            out.push((*handle, format!("monitor \"{monitor_id}\" disconnected")));
        }
    }
    out
}

/// **STEP-M2-2.6**: pure helper — given active `(handle,
/// BarrierKey)` pairs and an old + new monitor snapshot, return the
/// handles whose bound monitor is still present but whose geometry
/// (position / size) changed.
///
/// Like [`reconcile_monitors`], this is a pure function for
/// unit-testability. The caller is responsible for actually doing
/// the `destroy + create` via `deactivate_client` + `activate_client`.
///
/// **M2 caveat**: the `BarrierKey` has no geometry fields — it
/// only stores `(pos, monitor, offset, span)`. M2 always passes
/// `offset = 0, span = 10000`, so for a monitor whose id is
/// unchanged, the recomputed BarrierKey is byte-identical to the
/// old one. The returned tuple still records `(old_key, new_key)`
/// because the PLAN §M2 STEP-2.6 calls for the destroy + create
/// round-trip even when the keys are equal; this is the
/// conservative interpretation. M4 will revisit when
/// offset/span actually vary by geometry.
fn recreate_monitors(
    active: &[(ClientHandle, BarrierKey)],
    new_monitors: &[GeometryMonitorInfo],
    old_monitors: &[GeometryMonitorInfo],
) -> Vec<(ClientHandle, BarrierKey, BarrierKey)> {
    let mut out = Vec::new();
    for (handle, key) in active {
        let Some(monitor_id) = key.monitor.as_ref() else {
            continue;
        };
        let old = old_monitors.iter().find(|m| m.id == *monitor_id);
        let new = new_monitors.iter().find(|m| m.id == *monitor_id);
        match (old, new) {
            (Some(old), Some(new)) if monitor_geometry_changed(old, new) => {
                // M2: the recomputed key equals the old key
                // because BarrierKey has no geometry fields. The
                // OUTER caller still performs a destroy + create
                // round-trip on this handle, mirroring `update_pos`.
                out.push((*handle, key.clone(), key.clone()));
            }
            _ => {}
        }
    }
    out
}

/// Compare two `MonitorInfo` records for the geometry subset that
/// PLAN §M2 STEP-2.6 considers "the barrier's rectangle moved":
/// `position` and `size`. `scale` does NOT affect the physical
/// rectangle in M2 (sub-edge barriers ignore scale), so it's not
/// part of this comparison.
fn monitor_geometry_changed(a: &GeometryMonitorInfo, b: &GeometryMonitorInfo) -> bool {
    a.position != b.position || a.size != b.size
}

/// **sleep-week-bug — Phase B pure helper**: given *all*
/// `(handle, BarrierKey)` pairs (active ∪ inactive) and an old + new
/// monitor snapshot, return the handles whose bound monitor is **back**:
/// not present in `old_monitors` but present in `new_monitors`.
///
/// This completes the M2 set (`deactivate` + `recreate`):
///
/// | `(was_present, is_present)` | branch          |
/// |------------------------------|-----------------|
/// | `(true, false)`              | `reconcile_monitors` (Phase A — detach) |
/// | `(false, true)`              | `recover_monitors` (here — Phase B)     |
/// | `(true, true)`               | `recreate_monitors` (Phase C if geometry Δ, else no-op) |
/// | `(false, false)`             | no-op                                    |
///
/// **Why a separate helper (not part of `reconcile_monitors`)**:
/// `reconcile_monitors` only walks *active* bindings (legacy contract from
/// STEP-M2-2.6); the recover signal needs to consider **inactive** handles
/// too — a client that was detached in Phase A on a previous tick and is
/// still `s.active=true` (because `detach_capture` deliberately doesn't
/// clear `s.active`) must surface for reattach here. Splitting the
/// helper keeps the pure-function surface clean and unit-testable, and
/// matches the M2 split between "removal" and "geometry change" already
/// in this module.
///
/// **Why iterate `all` instead of asking `ClientManager` for inactive-only**:
/// `ClientManager` doesn't currently distinguish "user-deactivated" from
/// "auto-deactivated-by-detach" — both have `s.active=false` (well,
/// detach keeps `s.active=true`; user-deactivate sets it to `false`). The
/// caller (`reconcile_monitors_changed`) gates the actual mutation on
/// `active_clients().contains(&handle)`, so even if a *user-deactivated*
/// handle's `monitor` came back, it won't be reattached. This helper
/// just reports the *geometry* signal; activation gating is the caller's
/// responsibility.
///
/// **`monitor = None` legacy clients**: skipped by the same `let Some(...) else continue`
/// guard as `reconcile_monitors` — `None` bindings are never detached
/// (Phase A returns no entry for them), so there's nothing to recover.
fn recover_monitors(
    all: &[(ClientHandle, BarrierKey)],
    new_monitors: &[GeometryMonitorInfo],
    old_monitors: &[GeometryMonitorInfo],
) -> Vec<ClientHandle> {
    let mut out = Vec::new();
    for (handle, key) in all {
        let Some(monitor_id) = key.monitor.as_ref() else {
            // M1 default: no binding. Nothing to recover.
            continue;
        };
        let was_present = old_monitors.iter().any(|m| m.id == *monitor_id);
        let is_present = new_monitors.iter().any(|m| m.id == *monitor_id);
        if !was_present && is_present {
            out.push(*handle);
        }
    }
    out
}

#[cfg(test)]
mod reconcile_tests {
    //! Unit tests for the pure `reconcile_monitors` / `recreate_monitors`
    //! helpers that drive STEP-M2-2.6.
    //!
    //! The two helpers are deliberately pure functions (no `&mut
    //! Service`, no `ClientManager` dependency) so this test
    //! module can construct arbitrary `(ClientHandle, BarrierKey)`
    //! lists without touching the `Capture::new` / `LanMouseConnection`
    //! / `Emulation::new` / `AsyncFrontendListener` /
    //! `DnsResolver::new` plumbing that `Service::new` requires.
    //!
    //! See `tests::monitor_reconcile_*` for the end-to-end
    //! behaviour of `Service::handle_capture_event`.

    use super::*;
    use input_capture::Position;

    fn mk_monitor(id: &str, position: (i32, i32), size: (u32, u32)) -> GeometryMonitorInfo {
        GeometryMonitorInfo {
            id: id.to_string(),
            name: format!("monitor-{id}"),
            position,
            size,
            primary: id == "primary",
            scale: 1.0,
        }
    }

    fn binding(handle: ClientHandle, monitor: Option<&str>) -> (ClientHandle, BarrierKey) {
        (
            handle,
            BarrierKey {
                pos: Position::Right,
                monitor: monitor.map(str::to_string),
                offset: 0,
                span: 10000,
            },
        )
    }

    /// **M1 default**: clients whose `key.monitor` is `None` must
    /// never produce a `BindingInvalid`, regardless of what the
    /// monitor list does. This is the "M2 reconcile is a no-op for
    /// legacy clients" guarantee.
    #[test]
    fn reconcile_noop_when_all_clients_have_default_key_monitor_none() {
        let active = vec![binding(0, None), binding(1, None)];
        let old = vec![mk_monitor("DP-2", (0, 0), (1920, 1080))];
        let new: Vec<GeometryMonitorInfo> = vec![];
        assert!(reconcile_monitors(&active, &new, &old).is_empty());
        assert!(recreate_monitors(&active, &new, &old).is_empty());
    }

    /// **No-op case** (PLAN §M2 STEP-2.6 / §8): the new monitor
    /// list is identical to the old one — `reconcile_monitors` must
    /// return no deactivations and `recreate_monitors` must return
    /// no recreations, even for clients bound to a specific
    /// `MonitorId`.
    #[test]
    fn reconcile_noop_when_monitor_list_unchanged() {
        let monitors = vec![mk_monitor("DP-2", (0, 0), (1920, 1080))];
        let active = vec![binding(0, Some("DP-2"))];
        assert!(reconcile_monitors(&active, &monitors, &monitors).is_empty());
        assert!(recreate_monitors(&active, &monitors, &monitors).is_empty());
    }

    /// **Removal case** (PLAN §M2 STEP-2.6 / §8): a client bound
    /// to `DP-2` whose monitor disappears from the new list must
    /// produce exactly one `BindingInvalid` entry with a reason
    /// string that names the missing monitor id (the GUI surfaces
    /// this verbatim as a tooltip).
    #[test]
    fn reconcile_emits_binding_invalid_when_monitor_removed() {
        let old = vec![mk_monitor("DP-2", (0, 0), (1920, 1080))];
        let new: Vec<GeometryMonitorInfo> = vec![];
        let active = vec![binding(7, Some("DP-2"))];

        let deactivations = reconcile_monitors(&active, &new, &old);
        assert_eq!(deactivations.len(), 1, "expected 1 deactivation");
        let (handle, reason) = &deactivations[0];
        assert_eq!(*handle, 7);
        assert!(
            reason.contains("DP-2"),
            "reason should name the missing monitor; got {reason:?}"
        );
        assert!(
            reason.contains("disconnected"),
            "reason should be human-readable; got {reason:?}"
        );

        // A removal does NOT also trigger a recreate — the monitor
        // is gone, the only correct response is deactivate.
        assert!(recreate_monitors(&active, &new, &old).is_empty());
    }

    /// **Startup seed**: if a client is bound to `DP-2` but the
    /// *old* list never contained it (e.g. the daemon just started
    /// and `last_monitors` was `None`), the client must NOT be
    /// deactivated. This test pins the "first observation is not a
    /// removal" invariant — without it, restarting the daemon
    /// after reconnecting an external display would knock out
    /// every active client.
    #[test]
    fn reconcile_noop_when_old_list_did_not_contain_monitor() {
        let new = vec![mk_monitor("DP-2", (0, 0), (1920, 1080))];
        let old: Vec<GeometryMonitorInfo> = vec![];
        let active = vec![binding(3, Some("DP-2"))];
        assert!(reconcile_monitors(&active, &new, &old).is_empty());
        assert!(recreate_monitors(&active, &new, &old).is_empty());
    }

    /// **Geometry-change case** (PLAN §M2 STEP-2.6 / §8): a client
    /// bound to `DP-2` whose monitor's size changed between
    /// snapshots must produce exactly one recreate entry. The two
    /// keys passed back are byte-equal in M2 (the BarrierKey has
    /// no geometry field — see the M2 caveat in
    /// `recreate_monitors`'s docstring) but the *entry exists* —
    /// the destroy + create round-trip is what `update_pos`
    /// already does and the PLAN asks for it here.
    #[test]
    fn recreate_emits_entry_when_monitor_geometry_changes() {
        let old = vec![mk_monitor("DP-2", (0, 0), (1920, 1080))];
        let new = vec![mk_monitor("DP-2", (1920, 0), (2560, 1440))];
        let active = vec![binding(11, Some("DP-2"))];

        // No deactivate: monitor is still present.
        assert!(reconcile_monitors(&active, &new, &old).is_empty());

        // One recreate. The two BarrierKeys are equal in M2 (no
        // geometry fields). The caller still performs
        // `destroy + create` because that's what PLAN §M2 STEP-2.6
        // prescribes — this test pins the conservative
        // interpretation that the round-trip happens regardless
        // of key equality.
        let recreations = recreate_monitors(&active, &new, &old);
        assert_eq!(recreations.len(), 1, "expected 1 recreate");
        let (handle, _old_key, _new_key) = &recreations[0];
        assert_eq!(*handle, 11);
    }

    /// **Geometry unchanged but monitor swapped**: same id,
    /// identical geometry → no recreate (the rectangle the barrier
    /// attaches to is the same, even if the underlying EDID
    /// reported the same number by coincidence). The `id` is the
    /// only identity we care about.
    #[test]
    fn recreate_noop_when_monitor_geometry_unchanged() {
        let old = vec![mk_monitor("DP-2", (0, 0), (1920, 1080))];
        let new = vec![mk_monitor("DP-2", (0, 0), (1920, 1080))];
        let active = vec![binding(11, Some("DP-2"))];
        assert!(recreate_monitors(&active, &new, &old).is_empty());
    }

    /// **Mixed bindings**: some clients with `monitor = None` and
    /// some with `monitor = Some(...)`. The `None` ones never
    /// reconcile; the `Some` ones follow the rules above. This
    /// pins the "legacy + new coexist" path for the M2 → M3
    /// transition: clients that haven't been migrated to monitor
    /// binding yet must keep working alongside clients that have.
    #[test]
    fn reconcile_handles_mixed_default_and_bound_clients() {
        let old = vec![mk_monitor("DP-2", (0, 0), (1920, 1080))];
        let new: Vec<GeometryMonitorInfo> = vec![];
        let active = vec![
            binding(0, None),         // legacy — must not be deactivated
            binding(1, Some("DP-2")), // bound — must be deactivated
            binding(2, None),         // legacy — must not be deactivated
        ];

        let deactivations = reconcile_monitors(&active, &new, &old);
        assert_eq!(deactivations.len(), 1);
        let (handle, _) = &deactivations[0];
        assert_eq!(*handle, 1);
    }

    /// `geometry_to_ipc_monitor_info` is a field-for-field copy.
    /// Round-trip the conversion through JSON to confirm the wire
    /// shape matches `lan_mouse_ipc::MonitorInfo`'s serde contract
    /// (covered by the IPC crate's `monitor_info_tests`, but we
    /// pin the *direction* here: `from(geometry) -> ipc`).
    #[test]
    fn geometry_to_ipc_monitor_info_is_field_equivalent() {
        let g = mk_monitor("DP-2", (1920, -1080), (2560, 1440));
        let i = geometry_to_ipc_monitor_info(&g);
        assert_eq!(i.id, g.id);
        assert_eq!(i.name, g.name);
        assert_eq!(i.position, g.position);
        assert_eq!(i.size, g.size);
        assert_eq!(i.primary, g.primary);
        assert_eq!(i.scale, g.scale);
    }

    // ===== sleep-week-bug Phase B — `recover_monitors` tests =====
    //
    // Mirror the M2-style structure of the tests above. The helper is a
    // pure function over `(handle, key)` lists and old/new monitor
    // snapshots, with no `ClientManager` dependency, so we can reuse
    // `mk_monitor` / `binding` directly.

    /// **Recovery case** (the M3 bug fix on main + sleep-week-bug's
    /// single new code path): the `old` list does NOT contain the
    /// bound monitor (it disappeared in a previous reconcile), the
    /// `new` list DOES. The handle must be returned so the caller can
    /// reattach it. This is the path that fires after macOS
    /// sleep/wake: capture first reports `[]` (Phase A detaches),
    /// the bound client stays `s.active=true` but has no barrier,
    /// then capture reports the monitors again and Phase B
    /// reattaches.
    #[test]
    fn recover_emits_handle_when_monitor_reappears() {
        let new = vec![mk_monitor("DP-2", (0, 0), (1920, 1080))];
        let old: Vec<GeometryMonitorInfo> = vec![];
        // The handle's `s.active` is irrelevant to the pure helper.
        // In production this snapshot is taken from
        // `registered_clients()`, so the helper sees
        // `(handle, key)` regardless of activation state.
        let all = vec![binding(7, Some("DP-2"))];

        let recovered = recover_monitors(&all, &new, &old);
        assert_eq!(recovered, vec![7]);
    }

    /// **No recovery when monitor was always present**: a monitor
    /// that survived both snapshots is not a recovery candidate —
    /// it would only need a `recreate` if its geometry changed
    /// (Phase C).
    #[test]
    fn recover_noop_when_monitor_was_present_in_both_lists() {
        let monitors = vec![mk_monitor("DP-2", (0, 0), (1920, 1080))];
        let all = vec![binding(0, Some("DP-2"))];
        assert!(recover_monitors(&all, &monitors, &monitors).is_empty());
    }

    /// **No recovery when monitor is still missing**: if the bound
    /// monitor is in neither `old` nor `new`, the client was
    /// already detached by an earlier reconcile and remains so —
    /// the helper must not falsely "recover" it.
    #[test]
    fn recover_noop_when_monitor_still_missing() {
        let all = vec![binding(0, Some("DP-2"))];
        let new: Vec<GeometryMonitorInfo> = vec![];
        let old: Vec<GeometryMonitorInfo> = vec![];
        assert!(recover_monitors(&all, &new, &old).is_empty());
    }

    /// **M1 default clients** (`monitor = None`) must never trigger
    /// recovery, mirroring the `reconcile_monitors` invariant. Such
    /// clients are never detached in Phase A either, so they don't
    /// *need* recovery — but the guard is here to keep both helpers
    /// symmetrical.
    #[test]
    fn recover_skips_default_key_monitor_none() {
        let new = vec![mk_monitor("DP-2", (0, 0), (1920, 1080))];
        let old: Vec<GeometryMonitorInfo> = vec![];
        let all = vec![binding(0, None)];
        assert!(recover_monitors(&all, &new, &old).is_empty());
    }

    /// **Mixed bindings**: only bound clients whose monitor
    /// reappeared are recovered. Legacy (`monitor = None`) clients
    /// are skipped; bound clients whose monitor stayed missing are
    /// skipped; bound clients whose monitor is in `old` *and*
    /// `new` are skipped (they'd be a recreate candidate, not a
    /// recovery candidate — same as the existing
    /// `reconcile_noop_when_monitor_list_unchanged` test above).
    #[test]
    fn recover_handles_mixed_default_and_bound_clients() {
        let new = vec![
            mk_monitor("DP-2", (0, 0), (1920, 1080)),
            mk_monitor("DP-3", (1920, 0), (2560, 1440)),
        ];
        // DP-2 absent (will recover), DP-3 absent (must NOT recover),
        // DP-4 present in both (must NOT recover — it never left).
        let old = vec![mk_monitor("DP-3", (0, 0), (1920, 1080))];
        let all = vec![
            binding(0, None),         // legacy — skip
            binding(1, Some("DP-2")), // reappeared — recover
            binding(2, Some("DP-3")), // still missing — skip
            binding(3, Some("DP-4")), // present in both — skip
        ];

        let mut recovered = recover_monitors(&all, &new, &old);
        recovered.sort();
        assert_eq!(recovered, vec![1]);
    }
}
