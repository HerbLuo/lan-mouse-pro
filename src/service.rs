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
};
use thiserror::Error;
use tokio::{process::Command, signal, sync::Notify};

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

        let conn = LanMouseConnection::new(
            client_endpoint,
            cert_der.0.clone(),
            cert_der.1.clone_key(),
            pins_dir,
            client_manager.clone(),
            quic_idle_timeout,
            peer_lost_tx,
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
        let emulation_backend = config.emulation_backend().map(|b| b.into());
        let emulation = Emulation::new(emulation_backend, listener);

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

    fn broadcast_client(&mut self, handle: ClientHandle) {
        let event = self
            .client_manager
            .get_state(handle)
            .map(|(c, s)| FrontendEvent::State(handle, c, s))
            .unwrap_or(FrontendEvent::NoSuchClient(handle));
        self.notify_frontend(event);
    }

    /// **STEP-M2-2.6**: reconcile active clients against a new
    /// monitor snapshot.
    ///
    /// For each currently active client, look at its
    /// `BarrierKey.monitor` field (M1: always `None`, so this is
    /// effectively a no-op today; M3+ will populate the field with
    /// a user-chosen `MonitorId`) and compare against the new
    /// monitor list:
    ///
    /// * **Monitor gone**: `client_manager.deactivate_client(handle)`
    ///   + `FrontendEvent::BindingInvalid(handle, reason)` so the
    ///     GUI highlights the row (red border + tooltip) and pauses
    ///     the toggle.
    /// * **Monitor still present, geometry changed** (position /
    ///   size / scale): `capture.destroy(old_key)` then
    ///   `capture.create(new_key, handle)` via the standard
    ///   `deactivate_client` → `activate_client` round-trip — same
    ///   pattern as `update_pos`.
    /// * **No change**: nothing.
    ///
    /// `last_monitors` is updated by `handle_capture_event` *before*
    /// this is called; this method compares the new list against the
    /// previous one.
    ///
    /// **Why the compare is two-list (old + new)**: the
    /// `BarrierKey` itself doesn't carry geometry — it's
    /// `(pos, monitor, offset, span)` with M2 offset/span at
    /// defaults. Without an old-vs-new diff we'd never know whether
    /// a barrier's "monitor" still maps to the same physical
    /// rectangle, and the only signal that something changed is
    /// "the geometry in the new list differs from the geometry in
    /// the old list". The PLAN §M2 STEP-2.6 "exists but geometry
    /// changed" branch hinges on this diff.
    fn reconcile_monitors_changed(
        &mut self,
        new_monitors: &[GeometryMonitorInfo],
        old_monitors: &[GeometryMonitorInfo],
    ) {
        // Snapshot active bindings first so the closures can mutably
        // borrow `self.client_manager` / `self.capture` without
        // conflicting with the iteration borrow.
        let mut active_bindings: Vec<(ClientHandle, BarrierKey)> = Vec::new();
        for handle in self.client_manager.active_clients() {
            if let Some(key) = self.client_manager.get_key(handle) {
                active_bindings.push((handle, key));
            }
        }

        let deactivations = reconcile_monitors(&active_bindings, new_monitors, old_monitors);
        for (handle, reason) in deactivations {
            log::info!("service: monitor-driven deactivate handle={handle} reason={reason:?}");
            self.deactivate_client(handle);
            self.notify_frontend(FrontendEvent::BindingInvalid(handle, reason));
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
}
