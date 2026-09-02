//! Embedded HTTP / WebSocket frontend for lan-mouse.
//!
//! When the daemon is launched without any subcommand it now:
//!
//! 1. Starts the [`crate::service::Service`] on the local IPC socket as
//!    usual (the CLI still talks to it through that socket).
//! 2. Spawns the HTTP/WS server defined here.
//! 3. Calls [`WebServer::run`] which binds `127.0.0.1:<port>` (default
//!    `3939`, configurable via `LAN_MOUSE_WEB_PORT` or `[frontend]` in
//!    `config.toml`), serves the embedded Vue SPA, and proxies a
//!    bidirectional WebSocket onto the same IPC socket the local CLI /
//!    web frontend uses.
//!
//! The WebSocket wire format is intentionally byte-for-byte identical
//! to the existing Unix-socket IPC: one JSON object per text frame, no
//! length framing. That keeps [`lan_mouse_ipc::FrontendEvent`] /
//! [`lan_mouse_ipc::FrontendRequest`] the only serialization schema in
//! the project and means the Vue side can reuse the same TypeScript
//! shapes we already serialize on the Rust side.

use std::{net::SocketAddr, sync::OnceLock, time::Duration};

use axum::{
    Router,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::{IntoResponse, Json},
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use lan_mouse_ipc::{FrontendEvent, FrontendRequest, connect_async};
use log;
use rust_embed::RustEmbed;
use serde::Serialize;
use thiserror::Error;
use tokio::{
    net::TcpListener as TokioTcpListener,
    sync::{broadcast, mpsc},
    task::JoinHandle,
};
use tower_http::trace::TraceLayer;

/// Default port for the embedded web UI. Chosen to avoid the common
/// 3000/5000/8000/8080 range dev tools occupy. Override via
/// `LAN_MOUSE_WEB_PORT=…` or `[frontend] port = …` in `config.toml`.
pub const DEFAULT_WEB_PORT: u16 = 3939;

/// Capacity of the daemon→browser event bus. Plenty for any realistic
/// number of connected tabs / devices; oldest events are dropped if a
/// slow consumer falls behind, which is the correct failure mode for a
/// UI (we'd rather show stale state than block the daemon).
const EVENT_BUS_CAPACITY: usize = 256;

/// Files baked into the binary by `vite build` and copied to
/// `lan-mouse-vue/dist` (see `build.rs`). `rust-embed` looks up entries
/// in this bundle by relative path.
#[derive(RustEmbed)]
#[folder = "lan-mouse-vue/dist/"]
struct Asset;

#[derive(Debug, Error)]
pub enum WebError {
    #[error("could not bind web frontend on {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    #[error("could not connect to lan-mouse IPC socket: {0}")]
    IpcConnect(#[from] lan_mouse_ipc::ConnectionError),
    #[error("IPC handshake failed: {0}")]
    Ipc(#[from] lan_mouse_ipc::IpcError),
    #[error("could not determine local hostname: {0}")]
    Hostname(#[from] std::io::Error),
}

/// Lightweight initial-state response sent to the Vue app on first
/// load. Kept small on purpose — everything else is streamed through
/// the WebSocket so a fresh browser tab comes up with the current
/// daemon state in one round trip.
#[derive(Debug, Serialize)]
pub struct InitialInfo {
    pub hostname: String,
    pub web_port: u16,
    /// Outbound-facing IPv4 address, discovered via the classic
    /// "open a UDP socket to a public IP, peek at the local addr"
    /// trick. Empty if the host is offline or only has IPv6. Surfaces
    /// in the General panel so users have something to give the peer.
    pub primary_ip: String,
    /// All non-loopback IPv4 / IPv6 addresses reported by the OS,
    /// sorted with IPv4 first. Handy for hosts that have Ethernet +
    /// Wi-Fi + VPN all live at once — the user picks the right one.
    pub all_ips: Vec<String>,
}

/// Shared state handed to every axum handler. Cloned per request — only
/// contains cheap handles, no heavy state.
#[derive(Clone)]
struct WebState {
    web_port: u16,
    hostname: String,
    primary_ip: String,
    all_ips: Vec<String>,
    /// FrontendRequest sender side of the IPC bridge. The WebSocket
    /// handler clones this for each connection and feeds decoded
    /// `FrontendRequest`s through it.
    request_tx: mpsc::Sender<FrontendRequest>,
    /// Clone of the daemon→browser event bus. Each WebSocket
    /// connection subscribes to its own receiver.
    event_tx: broadcast::Sender<FrontendEvent>,
}

// ---- event bus ------------------------------------------------------

static EVENT_BUS: OnceLock<broadcast::Sender<FrontendEvent>> = OnceLock::new();

/// Initialise the global event bus. Called exactly once from
/// [`crate::main`] before any client can connect.
pub fn init_event_bus() {
    let (tx, _rx) = broadcast::channel(EVENT_BUS_CAPACITY);
    let _ = EVENT_BUS.set(tx);
}

fn event_bus() -> &'static broadcast::Sender<FrontendEvent> {
    EVENT_BUS
        .get()
        .expect("event bus not initialised — call init_event_bus() first")
}

// ---- IPC bridge -----------------------------------------------------

/// Establish a TCP connection to the daemon's IPC socket and fork off
/// two long-lived tasks:
///
/// * one task that pumps every incoming `FrontendEvent` into the
///   global broadcast bus so the WebSocket layer can fan them out to
///   many clients;
/// * one task that pumps every `FrontendRequest` submitted by the
///   WebSocket layer back into the IPC socket.
///
/// Returns the request sender (clone it per WebSocket client) and a
/// join handle for the event-pump task — pass both to
/// [`WebServer::bind`].
pub async fn spawn_ipc_bridge() -> Result<(mpsc::Sender<FrontendRequest>, JoinHandle<()>), WebError> {
    // The daemon creates this socket in `AsyncFrontendListener::new`.
    // Connecting to it from the same process is supported — the kernel
    // happily round-trips a same-process connect(2) — and lets us
    // reuse 100% of the existing IPC framing code instead of
    // refactoring the service to expose events over a channel.
    let (mut event_rx, mut request_tx) =
        connect_async(Some(Duration::from_millis(500))).await?;

    let event_tx = event_bus().clone();
    let event_pump: JoinHandle<()> = tokio::spawn(async move {
        while let Some(ev) = event_rx.next().await {
            match ev {
                Ok(event) => {
                    // Broadcast returns Result<_, SendError>; we
                    // ignore "no active receivers" because the WebSocket
                    // may briefly be empty (page reload) without us
                    // wanting to bail.
                    let _ = event_tx.send(event);
                }
                Err(e) => {
                    log::warn!("IPC event stream error: {e}");
                    break;
                }
            }
        }
        log::info!("IPC event stream closed");
    });

    // Pump browser → daemon over a single mpsc so we don't have to
    // &mut-share the `FrontendRequestWriter` across tasks.
    let (browser_tx, mut browser_rx) = mpsc::channel::<FrontendRequest>(64);
    tokio::spawn(async move {
        while let Some(req) = browser_rx.recv().await {
            if let Err(e) = request_tx.request(req).await {
                log::warn!("could not forward frontend request: {e}");
            }
        }
    });

    Ok((browser_tx, event_pump))
}

// ---- WebServer ------------------------------------------------------

pub struct WebServer {
    addr: SocketAddr,
    state: WebState,
}

impl WebServer {
    pub async fn bind(
        web_port: u16,
        request_tx: mpsc::Sender<FrontendRequest>,
    ) -> Result<Self, WebError> {
        let addr = SocketAddr::from(([127, 0, 0, 1], web_port));
        let hostname = hostname::get()?.to_string_lossy().into_owned();
        let (primary_ip, all_ips) = collect_local_ips().await;
        Ok(Self {
            addr,
            state: WebState {
                web_port,
                hostname,
                primary_ip,
                all_ips,
                request_tx,
                event_tx: event_bus().clone(),
            },
        })
    }

    /// Serve forever. Returns only on fatal bind / accept failure.
    pub async fn run(self) -> Result<(), WebError> {
        let listener = TokioTcpListener::bind(self.addr).await.map_err(|e| WebError::Bind {
            addr: self.addr,
            source: e,
        })?;
        log::info!("lan-mouse web UI listening on http://{}", self.addr);

        let router = Router::new()
            .route("/api/info", get(info_handler))
            .route("/ws", get(ws_handler))
            .fallback(static_handler)
            .layer(TraceLayer::new_for_http())
            .with_state(self.state);

        axum::serve(listener, router).await?;
        Ok(())
    }
}

// ---- handlers -------------------------------------------------------

async fn info_handler(State(state): State<WebState>) -> Json<InitialInfo> {
    Json(InitialInfo {
        hostname: state.hostname.clone(),
        web_port: state.web_port,
        primary_ip: state.primary_ip.clone(),
        all_ips: state.all_ips.clone(),
    })
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<WebState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: WebState) {
    let (mut sink, mut stream) = socket.split();
    let mut event_rx = state.event_tx.subscribe();
    let request_tx = state.request_tx.clone();

    // Forward daemon events → browser until either side hangs up.
    let write_task = tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(event) => {
                    let json = match serde_json::to_string(&event) {
                        Ok(j) => j,
                        Err(e) => {
                            log::warn!("could not serialize event: {e}");
                            continue;
                        }
                    };
                    if sink
                        .send(Message::Text(json.into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    log::warn!("ws subscriber lagged by {n} events");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    while let Some(msg) = stream.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                log::debug!("ws recv error: {e}");
                break;
            }
        };
        match msg {
            Message::Text(text) => match serde_json::from_str::<FrontendRequest>(&text) {
                Ok(req) => {
                    if request_tx.send(req).await.is_err() {
                        log::warn!("daemon request channel closed");
                        break;
                    }
                }
                Err(e) => log::warn!("invalid request from browser: {e}"),
            },
            Message::Close(_) => break,
            // We don't accept binary frames; axum handles ping/pong for
            // us so no need to do anything with Ping/Pong here.
            _ => {}
        }
    }

    write_task.abort();
}

// ---- static asset fallback -----------------------------------------

async fn static_handler(uri: axum::http::Uri) -> impl IntoResponse {
    let mut path = uri.path().trim_start_matches('/').to_string();
    if path.is_empty() {
        path = "index.html".into();
    }

    match Asset::get(&path) {
        Some(file) => {
            let mime = mime_guess::from_path(&path).first_or_octet_stream();
            (
                [(axum::http::header::CONTENT_TYPE, mime.as_ref())],
                file.data.into_owned(),
            )
                .into_response()
        }
        // SPA fallback: any unknown path returns index.html so the
        // Vue Router can take over on the client side.
        None => match Asset::get("index.html") {
            Some(index) => {
                let mime = mime_guess::from_path("index.html").first_or_octet_stream();
                (
                    [(axum::http::header::CONTENT_TYPE, mime.as_ref())],
                    index.data.into_owned(),
                )
                    .into_response()
            }
            None => (axum::http::StatusCode::NOT_FOUND, "not found").into_response(),
        },
    }
}

// ---- helpers -------------------------------------------------------

/// Best-effort: discover the host's LAN-facing IPs.
///
/// Returns `(primary_ip, all_ips)` where `primary_ip` is the address
/// the OS would use for outbound traffic (most useful for the peer to
/// dial) and `all_ips` is every non-loopback address, IPv4 first.
///
/// Implementation: open a UDP socket to a public IP and read its
/// local addr. No packets are actually sent (UDP `connect` is purely
/// a kernel-side route table lookup). This works on every platform
/// without touching `/proc/net/fib_trie` or ioctl.
///
/// If the host has no network at all we still return the all-IPs list
/// (empty) plus an empty `primary_ip` so the frontend can render a
/// "no IP detected" hint instead of crashing.
async fn collect_local_ips() -> (String, Vec<String>) {
    use std::net::{IpAddr, ToSocketAddrs};

    // Probe targets — order matters: the first one whose route is
    // reachable wins. We deliberately hit public IPs that are
    // well-known to respond to UDP; nothing is actually exchanged
    // because UDP `connect` doesn't transmit.
    const PROBES: &[&str] = &["8.8.8.8:80", "1.1.1.1:80", "114.114.114.114:53"];

    let mut primary_ip = String::new();
    for probe in PROBES {
        let Some(addr) = probe.to_socket_addrs().ok().and_then(|mut it| it.next()) else {
            continue;
        };
        // UDP "connect" doesn't send anything; it just pins the
        // outbound route so peeking local_addr() gives us the right
        // NIC.
        if let Ok(socket) = tokio::net::UdpSocket::bind("0.0.0.0:0").await {
            if socket.connect(addr).await.is_ok() {
                if let Ok(local) = socket.local_addr() {
                    primary_ip = local.ip().to_string();
                    break;
                }
            }
        }
    }

    // Gather every non-loopback address the OS will tell us about.
    // Uses the `local_ip_address` crate which abstracts away
    // getifaddrs (Unix) / GetAdaptersAddresses (Windows).
    let mut all_ips: Vec<IpAddr> = Vec::new();
    if let Ok(list) = local_ip_address::list_afinet_netifas() {
        for ip in list.into_iter().map(|(_, addr)| addr) {
            if !ip.is_loopback() {
                all_ips.push(ip);
            }
        }
    }
    // IPv4 first for readability.
    all_ips.sort_by_key(|ip| if ip.is_ipv4() { 0 } else { 1 });
    let all_ips: Vec<String> = all_ips.into_iter().map(|ip| ip.to_string()).collect();

    // If the probe failed but we did collect addresses from the NIC
    // walk, prefer the first IPv4 from that list as primary.
    if primary_ip.is_empty() {
        if let Some(first_v4) = all_ips.iter().find(|s| s.contains('.')) {
            primary_ip = first_v4.clone();
        }
    }

    (primary_ip, all_ips)
}

#[allow(dead_code)]
/// Print the URL the user should point their browser at — used by
/// `main` when running headless (no GUI to auto-open).
pub fn local_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// Resolve the port to bind on. Priority: explicit argument > env var >
/// config file > [`DEFAULT_WEB_PORT`].
pub fn resolve_port(arg: Option<u16>, config_port: Option<u16>) -> u16 {
    if let Some(p) = arg {
        return p;
    }
    if let Ok(p) = std::env::var("LAN_MOUSE_WEB_PORT") {
        if let Ok(p) = p.parse() {
            return p;
        }
    }
    config_port.unwrap_or(DEFAULT_WEB_PORT)
}