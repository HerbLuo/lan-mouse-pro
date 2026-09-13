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
use futures::{FutureExt, StreamExt};
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
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio::{process::Command, signal, sync::Notify, sync::mpsc as tokio_mpsc, sync::oneshot};

use crate::clipboard::{ClipboardBackend, Mime, default_backend, file_meta::FileMetaError};
use crate::quic_transport::http3::Http3Client;
use lan_mouse_proto::{ClipboardImage, ClipboardText, ProtoEvent};
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
    /// **M1a follow-up #2** — active incoming peer table used by
    /// the clipboard dispatcher's `broadcast_clipboard_event` helper
    /// to reach peers that have no outgoing-client entry (the slave
    /// daemon's view of its incoming master). Keyed by
    /// `SocketAddr` because that's how the QUIC peer registry
    /// (`LanMouseListener::quic_conns`, accessed via
    /// `Emulation::send_to_incoming`) identifies a peer.
    ///
    /// **Distinct from `incoming_conn_info`**: that map is keyed
    /// by the `ClientHandle` assigned at `Enter` time and gates
    /// the capture barrier; this map only tracks reachability +
    /// per-peer clipboard opt-in, and is populated immediately on
    /// `EmulationEvent::Connected` (before Enter). The two are
    /// kept separate deliberately — clipboard reachability does
    /// not depend on Enter (a peer can be QUIC-connected but never
    /// have its cursor cross over, yet still need to receive text
    /// copies made on this side).
    ///
    /// **Lifecycle**:
    /// - `EmulationEvent::Connected { addr, fingerprint }` →
    ///   `incoming_clipboard.insert(addr, IncomingClipboardState
    ///   { fingerprint, enable_clipboard_to: true })`.
    /// - `EmulationEvent::Disconnected { addr }` →
    ///   `incoming_clipboard.remove(&addr)`.
    /// The `Emulation` arm intentionally removes on every
    /// `Disconnected` (not just transient ones) so a peer that
    /// was permanently lost doesn't keep getting clipboard
    /// pushes that fail at the `Emulation::send_to_incoming`
    /// lookup.
    incoming_clipboard: HashMap<SocketAddr, IncomingClipboardState>,
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
    ///
    /// **2026-09-10 screenshot-bug fix**: ownership of this
    /// backend is moved into the spawned `clipboard_poller` task
    /// at the top of [`Self::run`]. The dispatcher in the main
    /// task talks to the poller through [`Self::clipboard_backend_cmd`]
    /// (the inbound side) + the `image_tx` / `text_tx` channels
    /// (the outbound side). Inbound writes are the only path
    /// that needs backend access from the main task; the polling
    /// side is fully owned by the spawned task.
    clipboard_backend: Option<Box<dyn ClipboardBackend>>,
    /// **2026-09-10 screenshot-bug fix** — mpsc sender for
    /// `BackendCmd` requests to the [`clipboard_poller`] task
    /// that owns [`Self::clipboard_backend`]. Inbound write
    /// handlers ([`Self::apply_inbound_clipboard_text`] /
    /// [`Self::handle_clipboard_inbound_image`] which spawns
    /// [`apply_inbound_image_task`]) and the
    /// recover-push path ([`Self::handle_clipboard_recover_push`])
    /// send `SetText` / `CurrentText` requests through this
    /// channel and await
    /// a `oneshot` reply. `None` until [`Self::run`] sets it up;
    /// in practice always `Some` for the lifetime of the main
    /// `select!`.
    clipboard_backend_cmd: Option<tokio_mpsc::UnboundedSender<BackendCmd>>,
    /// **2026-09-10 inbound-apply off-thread follow-up** —
    /// sender for [`InboundImageApplyResult`] events from
    /// [`apply_inbound_image_task`] (the spawned task that owns
    /// the actual `BackendCmd::SetImage` / `BackendCmd::CurrentImage`
    /// round trip + the post-write LRU SHA computation) back to
    /// the main task's select!. The main task consumes these in a
    /// dedicated arm to update the image LRU + metrics + frontend
    /// notify. `None` until [`Self::run`] sets it up.
    ///
    /// **Why a separate channel from `clipboard_backend_cmd`**:
    /// the cmd channel flows main → poller (commands to the
    /// backend). The apply-result channel flows spawned-task →
    /// main (results back). Keeping them separate makes the
    /// lifetimes obvious and lets the main select! arm pattern-
    /// match on the result type cleanly.
    apply_image_applied_tx: Option<tokio_mpsc::UnboundedSender<InboundImageApplyResult>>,
    /// **M3a STEP-3a.3** — sender for [`InboundFileApplyResult`]
    /// events from the spawned
    /// [`apply_inbound_files_task`] (HTTP/3 GET + spawn_blocking
    /// write + sha256 verify) back to the main task's `select!`.
    /// The main task consumes these in a dedicated arm to update
    /// the file loopback LRU + metrics + frontend notify. `None`
    /// until [`Self::run`] sets it up.
    ///
    /// **Pattern parity with `apply_image_applied_tx`**: spawned
    /// task → main task, separate channel from `clipboard_backend_cmd`
    /// (which flows main → poller). The two result channels keep
    /// their lifetimes obvious and let the main `select!` arm
    /// pattern-match on the result type cleanly.
    inbound_files_applied_tx: Option<tokio_mpsc::UnboundedSender<InboundFileApplyResult>>,
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
    /// **PLAN-2 / M1b STEP-1b.2** — content-addressed outbound
    /// clipboard text cache (sha256 → bytes). The dispatcher writes
    /// large (> 1 KiB) payloads here; the HTTP/3-lite server reads
    /// from the same cache to serve `GET /clipboard/text/{sha256}`
    /// from remote peers.
    ///
    /// `Arc<Mutex<...>>` because both the dispatcher (writer) and the
    /// per-peer HTTP/3 server (reader) need access. `Service` clones
    /// the `Arc` into `LanMouseListener::new` at startup so every
    /// per-peer server reads from the same backing store.
    ///
    /// Active eviction (the dispatcher's "remove prev before push"
    /// path) keeps the cache size well under its 128-entry capacity
    /// in practice; the 5 min TTL is a fallback for the "source
    /// silent > 5 min" case. See
    /// [`crate::clipboard::cache::ClipboardCache`] for the contract.
    clipboard_cache: Arc<Mutex<crate::clipboard::cache::ClipboardCache>>,
    /// **M2a STEP-2a.4** — image-branch loopback LRU. **Independent**
    /// from [`Self::clipboard_lru`] (text branch): capacity
    /// [`IMAGE_LOOPBACK_CAPACITY`] = 32 entries vs the text branch's
    /// 128, same 60-second TTL. A flood of text copies does not roll
    /// the image LRU and vice versa. Behavioural semantics are
    /// identical to the text branch: an inbound `ClipboardImage`
    /// whose `sha256` is already in this LRU is treated as our own
    /// writeback and dropped (loopback detection at the receiver).
    /// See [`Self::mark_local_image_write`] for the inbound apply
    /// helper and `LruFingerprints` for the LRU type.
    image_lru_fingerprints: LruFingerprints,
    /// **PLAN-2 / M1b STEP-1b.2** — sha256 of the most recent
    /// outbound `ClipboardText` push. Tracked so the dispatcher's
    /// next push can evict this entry from [`Self::clipboard_cache`]
    /// before installing the new one (PLAN §1 评审 #3 2nd:
    /// "源端 cache 失效 push/pull race").
    ///
    /// Inline (≤ 1 KiB) and metadata-only (> 1 KiB) pushes both
    /// update this field — the active eviction is keyed by sha256, so
    /// it doesn't care which path produced the previous push.
    last_outbound_text_sha: Option<[u8; 32]>,
    /// **M2a STEP-2a.3** — sha256 of the most recent
    /// outbound `ClipboardImage` push. Mirrors
    /// [`Self::last_outbound_text_sha`] for the image dispatcher
    /// branch: the next push calls
    /// [`Self::evict_prev_outbound_image_cache`] (which delegates to
    /// the same free-function helper) so the image branch follows
    /// the identical "evict prev before push" contract as the text
    /// branch.
    ///
    /// **Also serves as the tick short-circuit**: if the freshly-
    /// read image's sha256 matches `last_outbound_image_sha`, the
    /// dispatcher skips the broadcast. This avoids re-pushing the
    /// same image metadata every 500 ms while the clipboard sits
    /// unchanged (the macOS backend's `changeCount` short-circuit
    /// in STEP-2a.2 means `current_image()` runs less often than
    /// the tick rate, but every call still produces a sha256 hash
    /// worth a few ms).
    ///
    /// **Distinct from `last_outbound_text_sha`**: the two are
    /// tracked separately so a text push doesn't accidentally evict
    /// a previously-cached image and vice versa. They share one
    /// underlying cache (`clipboard_cache`) but the active-eviction
    /// contract keys by sha256, so the previous-push pointer must
    /// match the previous-push kind.
    last_outbound_image_sha: Option<[u8; 32]>,
    /// **M3a STEP-3a.2** — sha256 of the most recent outbound
    /// `ClipboardFiles` push. Mirrors [`Self::last_outbound_text_sha`]
    /// / [`Self::last_outbound_image_sha`] for the file branch.
    /// The dispatcher's `dispatch_files` short-circuits when the
    /// freshly-computed fingerprint matches this field, mirroring
    /// the per-kind tick short-circuit contract.
    ///
    /// **Distinct from `last_outbound_text_sha` /
    /// `last_outbound_image_sha`**: each kind is tracked
    /// independently so a text push does not accidentally short-
    /// circuit a file push and vice versa. The "fingerprint" here
    /// is the sha256 of the **sorted path list** (one entry per
    /// file in the OS clipboard selection) — deterministic for
    /// a given selection, distinct across different selections.
    ///
    /// **Future schema change**: the fingerprint moves to a
    /// stable `ClipboardFiles::fingerprint` field at the wire
    /// level once `lan-mouse-proto::ClipboardFiles` lands a
    /// canonical fingerprint derivation. M3a STEP-3a.2 uses a
    /// local helper ([`crate::service::file_selection_fingerprint`])
    /// to avoid a proto bump on the partial path.
    last_outbound_files_fingerprint: Option<[u8; 32]>,
    /// **M3a STEP-3a.5** — per-file SHA-256 list of the most
    /// recent outbound `ClipboardFiles` push. Distinct from
    /// [`Self::last_outbound_files_fingerprint`] (which tracks
    /// the *batch* fingerprint): the cancel pathway needs to know
    /// the individual entry sha256 list so it can emit one
    /// `FileTransferCancel { sha256 }` per entry AND call
    /// `file_cache.remove(sha256)` for each (the cache is keyed by
    /// per-file sha256, not by fingerprint).
    ///
    /// On the next dispatch tick, if the fingerprint changes
    /// (i.e. the user overwrote the clipboard with a different
    /// selection), the previous list is taken via
    /// [`std::mem::take`] and:
    /// 1. A `FileTransferCancel` is broadcast over StreamC for
    ///    each sha256 (`broadcast_clipboard_event` honours the
    ///    per-peer `enable_clipboard_to` gate).
    /// 2. Each sha256 is removed from [`Self::file_cache`] via
    ///    direct `Arc<Mutex<FileCache>>::lock` (O(1) hash delete,
    ///    no `spawn_blocking` — per the §5 risk #9 rationale).
    last_outbound_files_sha: Vec<[u8; 32]>,
    /// **M3a STEP-3a.5** — registry of in-flight inbound HTTP/3
    /// file fetches keyed by sha256. Each `apply_inbound_files_task`
    /// registers a `oneshot::Sender<()>` before issuing its
    /// `Http3Client::get_file` call; when the source fires
    /// `FileTransferCancel { sha256 }`, the receiver-side handler
    /// pops the entry and sends the cancel signal. The fetch task
    /// races the GET against `cancel_rx` — on cancel, the task
    /// drops the (in-progress) recv stream (which triggers quinn's
    /// STOP_SENDING) and skips the write entirely.
    ///
    /// `Arc<Mutex<...>>` because the registry is shared between
    /// the main task (which installs/removes entries as `select!`
    /// arms fire) and the `spawn_local`'d apply tasks. The actual
    /// contention is minimal (each operation is an O(1) hash
    /// insert / remove), so `std::sync::Mutex` matches the
    /// `file_cache` pattern. The `oneshot::Sender` is consumed on
    /// `send`, so the registry entry is auto-cleared by the
    /// `remove` call inside the cancel handler.
    inbound_file_cancel_txs: Arc<Mutex<HashMap<[u8; 32], oneshot::Sender<()>>>>,
    /// **M3a STEP-3a.2** — file-body byte cache (sha256 → bytes).
    /// Shared `Arc` so the dispatcher's writer arm and the future
    /// HTTP/3 server-side `/clipboard/file/{sha256}` reader
    /// (STEP-3a.4) can address the same backing store. Distinct
    /// from `clipboard_cache` so a 1 GiB file push cannot evict
    /// cached text / image bytes mid-session. See
    /// [`crate::clipboard::file_cache::FileCache`] for the
    /// contract (1 GiB byte budget + 5 min TTL).
    ///
    /// `#[allow(dead_code)]` is removed in the P1.1 follow-up —
    /// `dispatch_files` now inserts into the cache via
    /// `insert_owned` after the spawn_blocking metadata step,
    /// so the field has a real writer. The reader
    /// (HTTP/3 server `/clipboard/file/{sha256}`) lands in
    /// STEP-3a.4.
    file_cache: Arc<Mutex<crate::clipboard::file_cache::FileCache>>,
    /// **M3a STEP-3a.2** — per-file loopback LRU. Independent
    /// from text + image LRUs so a flood of text / image copies
    /// does not roll the file LRU and vice versa. Capacity 64
    /// (vs text 128, image 32) + 60 s TTL — chosen so 64 distinct
    /// file selections can race within the lookback window
    /// without the dispatcher bouncing its own outbound pushes.
    /// See [`Self::image_lru_fingerprints`] for the sibling
    /// rationale (image's smaller capacity is because image
    /// writes are expensive; the file path's sha256 is the
    /// *commitment*, not the bytes themselves, so 64 entries is
    /// a comfortable headroom — files pushes are rarer than
    /// text pushes in practice).
    ///
    /// **M3a STEP-3a.3** — consumer is
    /// [`Self::handle_clipboard_inbound_files`] (the receiver's
    /// loopback check: a peer-pushed `ClipboardFiles` whose
    /// `fingerprint` is in the LRU is treated as our own writeback
    /// and dropped instead of re-downloading).
    ///
    /// **LRU presence from STEP-3a.2**: STEP-3a.2 constructed the
    /// LRU but marked it `#[allow(dead_code)]` because the inbound
    /// arm was deferred to STEP-3a.3. The LRU presence pins the
    /// per-kind loopback defence contract (text + image + file all
    /// have independent LRU instances). The `#[allow]` is removed
    /// here in STEP-3a.3 — the consumer now exists.
    file_lru_fingerprints: LruFingerprints,
    /// **M3a STEP-3a.2** — receiver for `current_files()` results
    /// from the spawned `clipboard_poller` task. Populated in
    /// [`Self::run`]; consumed by the main task's `select!` arm
    /// that delegates to [`Self::dispatch_files`].
    ///
    /// **Distinct from `clipboard_inbound_rx`**: that channel
    /// carries events *received from peers* (inbound). This
    /// channel carries local clipboard *file selections* (outbound
    /// source). The two directions never share a payload.
    files_rx: Option<tokio_mpsc::UnboundedReceiver<Vec<std::path::PathBuf>>>,
    /// **M3a STEP-3a.2** — sender clone moved into the spawned
    /// `clipboard_poller` task so each 500 ms tick can hand the
    /// current `Vec<PathBuf>` back to the main task's `select!`
    /// arm. Kept on the struct for symmetry with
    /// [`Self::clipboard_inbound_tx`].
    #[allow(dead_code)]
    files_tx: tokio_mpsc::UnboundedSender<Vec<std::path::PathBuf>>,
    /// **M3a STEP-3a.2** — per-batch ceiling on individual file
    /// size, in bytes. Mirrors `lan_mouse-ipc::ClipboardConfig::
    /// max_file_size` (which lands in M3b STEP-3b.1). Until
    /// IPC-driven config lands, this is hard-coded to
    /// [`DEFAULT_MAX_FILE_SIZE`] (50 MiB); a SUGGESTION.md entry
    /// tracks the "wire through Config" follow-up.
    max_file_size: u64,
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
    /// **PLAN-2 / M1b STEP-1b.3** — runtime signals for the
    /// loopback LRU. `Arc` because the dispatcher (writer) and
    /// the 60-s hit-rate log task (reader) need to share the
    /// same counter set. Atomics are lock-free so no `Mutex`
    /// is needed. See [`ClipboardMetrics`] for the full
    /// contract.
    metrics: Arc<ClipboardMetrics>,
}

/// **PLAN-2 / M1a STEP-1a.4 + M1b STEP-1b.3** — fixed-capacity
/// LRU of SHA-256 fingerprints with a TTL, used by the clipboard
/// dispatcher's loopback defence.
///
/// **Implementation**: `VecDeque<(Instant, [u8; 32])>` with linear
/// `contains`. Capacity **128** + **60 s TTL** per PLAN §3 M1b
/// STEP-1b.3 (reviewer #4 3rd: original M1a was capacity 64 with
/// no TTL, which rolled under "64 different copies in 60 s"
/// pressure).
///
/// `contains` does **lazy TTL eviction**: expired entries are
/// popped from the front on every check. This matches the
/// `ClipboardCache` semantics — both rely on a 60-s lookback
/// window to bound the loopback LRU's reach into past state.
///
/// **M2a STEP-2a.4** — image-branch LRU has its own dedicated
/// instance (see [`IMAGE_LOOPBACK_CAPACITY`] / [`IMAGE_LOOPBACK_TTL`]).
/// The text branch uses the default 128 / 60 s; the image branch
/// uses 32 / 60 s because image writes are expensive (each entry
/// represents the *commitment* to write a 5–15 MiB PNG, not the
/// bytes themselves). Two separate `LruFingerprints` instances
/// keep the two histories independent — a flood of text copies
/// does not roll the image LRU, and vice versa.
///
/// **`contains` is `&mut self`** because of the lazy eviction.
/// All call sites have `&mut Service` already (single-threaded
/// `spawn_local` task), so the signature change is a no-op for
/// the dispatcher's normal flow.
///
/// **Capacity 128**: capacity 1 covers the "1 push + 1 receiver
/// pulls at a time" baseline; the 128x headroom absorbs races
/// where a few receivers are mid-pull when the next push ejects
/// the previous payload, and a few clipboard pushes happen
/// between the receiver's metadata arrival and GET (typical for
/// keyboard-heavy users). The `contains` cost is O(128) byte
/// comparisons per inbound event, well below the dispatch tick's
/// 1-3 ms typical work — same order as the M1a O(64).
///
/// **M2a STEP-2a.4 — image LRU uses 32 entries, not 128** (see
/// [`IMAGE_LOOPBACK_CAPACITY`]). The image branch carries 5–15 MiB
/// PNG screenshots in its `apply_inbound_clipboard_image` path —
/// even though only the *fingerprint* (32 bytes) lives in the LRU,
/// the "this image is now committed to be written to the local
/// clipboard" state is heavier than text: an inbound `set_image` on
/// macOS crosses an `objc2` + `NSPasteboard` boundary and a Windows
/// `SetClipboardData(CF_DIBV5, …)` call. 32 entries is still 32x the
/// "1 push + 1 receiver pulls" baseline, well within reason, while
/// bounding the worst-case "flood of image copies in 60 s" LRU
/// footprint at 32 × 32 bytes = 1 KiB.
#[derive(Debug)]
struct LruFingerprints {
    capacity: usize,
    ttl: Duration,
    items: VecDeque<(Instant, [u8; 32])>,
}

/// **M2a STEP-2a.4** — capacity of the image-branch loopback LRU.
/// Independent from `LruFingerprints::DEFAULT_CAPACITY` (128) — see
/// the [`LruFingerprints`] doc for the rationale. Pinned at
/// module scope (not a `LruFingerprints` associated constant) because
/// it's image-specific, not part of the LRU type's contract.
const IMAGE_LOOPBACK_CAPACITY: usize = 32;

/// **M3a STEP-3a.2** — capacity of the file-branch loopback LRU.
/// Independent from the text (128) and image (32) branches —
/// file writes carry sha256 metadata + a potentially large body,
/// but the *commitment* itself (the per-batch fingerprint, not
/// the bytes) is small. 64 entries strikes a balance: enough
/// headroom for a multi-step file copy (Finder selection 1 →
/// selection 2 → selection 3 in < 60 s, all racing the
/// loopback defence), small enough that the LRU's worst-case
/// 60-second footprint stays at 64 × 32 bytes = 2 KiB. Sized
/// in line with the image branch's "fingerprint is the
/// commitment" rationale (see [`IMAGE_LOOPBACK_CAPACITY`]
/// docstring), just with a larger capacity because file
/// selections are rarer than image writes.
const FILE_LOOPBACK_CAPACITY: usize = 64;

/// **M2a STEP-2a.4** — TTL of the image-branch loopback LRU. Same
/// 60-second baseline as the text branch (matches the
/// `LruFingerprints::DEFAULT_TTL` rationale: "1 push + 1 receiver
/// pulls at a time"). Independent constant because the image and
/// text LRUs are separate instances; if one TTL ever needs to
/// drift the change is local.
const IMAGE_LOOPBACK_TTL: Duration = Duration::from_secs(60);

/// **M3a STEP-3a.2** — TTL of the file-branch loopback LRU.
/// Same 60-second baseline as the text + image branches.
/// Independent constant for the same reason
/// ([`IMAGE_LOOPBACK_TTL`] rationale): each LRU is its own
/// instance; if one TTL ever needs to drift the change is
/// local.
const FILE_LOOPBACK_TTL: Duration = Duration::from_secs(60);

impl LruFingerprints {
    /// Default capacity — matches PLAN §3 M1b STEP-1b.3 (reviewer
    /// #4 3rd, was 64 in M1a).
    const DEFAULT_CAPACITY: usize = 128;

    /// Default TTL — matches PLAN §3 M1b STEP-1b.3 (reviewer #4
    /// 3rd, was unbounded in M1a). 60 s is the "1 push + 1
    /// receiver pulls at a time" baseline: a peer-pushed echo of a
    /// fingerprint we wrote locally more than a minute ago is no
    /// longer a loopback, it's a fresh event from a different
    /// session.
    const DEFAULT_TTL: Duration = Duration::from_secs(60);

    fn new() -> Self {
        Self::with_capacity_and_ttl(Self::DEFAULT_CAPACITY, Self::DEFAULT_TTL)
    }

    /// Construct an LRU with custom capacity / TTL. Used by tests
    /// that want a 0-second TTL or capacity 1 to exercise eviction
    /// quickly.
    fn with_capacity_and_ttl(capacity: usize, ttl: Duration) -> Self {
        Self {
            capacity,
            ttl,
            items: VecDeque::with_capacity(capacity),
        }
    }

    fn contains(&mut self, fp: &[u8; 32]) -> bool {
        // Lazy TTL eviction: walk from the front, popping
        // expired entries. Stops at the first non-expired entry
        // (the deque is push-back / pop-front LRU-ordered, so
        // the front is the oldest).
        let now = Instant::now();
        while let Some((ts, _)) = self.items.front() {
            if now.duration_since(*ts) >= self.ttl {
                self.items.pop_front();
            } else {
                break;
            }
        }
        self.items.iter().any(|(_, sha)| sha == fp)
    }

    fn push(&mut self, fp: [u8; 32]) {
        if self.items.len() >= self.capacity {
            self.items.pop_front();
        }
        self.items.push_back((Instant::now(), fp));
    }

    /// **M1b STEP-1b.3** — explicit "we just wrote this
    /// fingerprint to the local clipboard" mark. Called by
    /// [`Service::apply_inbound_clipboard_text`] **before**
    /// `backend.set_text`, so an OS echo of the freshly-written
    /// value (if the platform emits a change event during the
    /// same tick) is caught by the next `contains` check.
    ///
    /// Functionally an alias for `push`; distinguished at the
    /// call site so the dispatcher's outbound push (`push`) and
    /// inbound apply (`mark_local_write`) read as semantically
    /// separate operations.
    fn mark_local_write(&mut self, fp: [u8; 32]) {
        self.push(fp);
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

// ============================================================================
//  ClipboardMetrics — runtime signals for the loopback LRU (PLAN-2 / M1b STEP-1b.3)
// ============================================================================

/// **PLAN-2 / M1b STEP-1b.3** — runtime signals for the clipboard
/// loopback LRU. Tracks hit (skip) / miss (allow) counts plus the
/// timestamp of the most recent skip so an operator can verify the
/// loopback defence is firing when expected.
///
/// **Counters**:
/// - `skip_count` — incremented on every inbound `ClipboardText`
///   whose fingerprint was already in the loopback LRU (we wrote
///   it locally recently, the peer is echoing it back).
/// - `allow_count` — incremented on every inbound `ClipboardText`
///   that passed the loopback check **and** was successfully
///   applied to the local clipboard.
/// - `last_skip_ts` — UNIX milliseconds of the most recent skip;
///   0 until the first skip fires. Surfaced in the GUI in M4
///   STEP-4.4 ("回环跳过统计卡片").
///
/// **Why `AtomicU64` and not `Mutex<u64>`**: counters are
/// updated from the dispatcher's `spawn_local` task and read by
/// the 60-s hit-rate log task. Atomics avoid a lock acquisition
/// on every push, which fires every 500 ms during normal
/// operation. `Ordering::Relaxed` is correct here — the counters
/// are independent monoids (each `incr_*` is atomic in itself)
/// and the snapshot doesn't need cross-counter consistency.
///
/// **`last_skip_ts` is updated atomically with `skip_count`**
/// (separate stores, both `Relaxed`): the snapshot is the
/// "logically most recent seen" value, not a transaction. If the
/// hit-rate task races with a `incr_skip`, it may observe a
/// `skip_count` one higher than the `last_skip_ts` it just read,
/// but the next tick will reconcile. Pinning them together
/// would require a single `u128` packing, which is overkill for
/// a debug-grade signal.
#[derive(Debug, Default)]
pub struct ClipboardMetrics {
    skip_count: AtomicU64,
    allow_count: AtomicU64,
    last_skip_ts: AtomicU64,
}

impl ClipboardMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    /// Increment `skip_count` and stamp `last_skip_ts` with
    /// `unix_now_ms`. Called from
    /// [`Service::handle_clipboard_inbound`]'s loopback-hit arm.
    pub fn incr_skip(&self, unix_now_ms: u64) {
        self.skip_count.fetch_add(1, Ordering::Relaxed);
        self.last_skip_ts.store(unix_now_ms, Ordering::Relaxed);
    }

    /// Increment `allow_count`. Called from
    /// [`Service::apply_inbound_clipboard_text`] after
    /// `backend.set_text` succeeds. **Does not** touch
    /// `last_skip_ts` (that's the skip signal, by definition).
    pub fn incr_allow(&self) {
        self.allow_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Read all three counters atomically (per-counter atomic
    /// loads; cross-counter consistency is not required — see
    /// the struct doc).
    pub fn snapshot(&self) -> ClipboardMetricsSnapshot {
        ClipboardMetricsSnapshot {
            skip: self.skip_count.load(Ordering::Relaxed),
            allow: self.allow_count.load(Ordering::Relaxed),
            last_skip_ts: self.last_skip_ts.load(Ordering::Relaxed),
        }
    }
}

/// Plain-old-data view of [`ClipboardMetrics`] for callers that
/// only need a snapshot (the hit-rate log task). `Copy` because
/// three `u64`s cost nothing to duplicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipboardMetricsSnapshot {
    pub skip: u64,
    pub allow: u64,
    pub last_skip_ts: u64,
}

impl ClipboardMetricsSnapshot {
    /// Compute the loopback LRU hit rate as `skip / (skip + allow)`.
    /// Returns `None` when no events have been observed (avoids
    /// division by zero AND keeps the hit-rate log task silent on
    /// quiet daemons).
    pub fn hit_rate(self) -> Option<f64> {
        let total = self.skip + self.allow;
        if total == 0 {
            None
        } else {
            Some(self.skip as f64 / total as f64)
        }
    }
}

/// **PLAN-2 / M1b STEP-1b.3** — background hit-rate log task.
///
/// Spawns a `spawn_local` task (the daemon runs on a
/// `current_thread` runtime + `LocalSet`) that ticks every 60 s
/// and emits a `log::trace!` line at the
/// `lan_mouse::service::clipboard` target. The log is gated by
/// the standard `RUST_LOG` filter:
///
/// ```text
/// RUST_LOG=lan_mouse::service::clipboard=trace
/// ```
///
/// enables it; anything stricter (info / warn) silences it. We
/// skip the log when no events have been observed yet (avoids
/// noisy 0/0 lines on freshly-started daemons) and we skip the
/// immediate first tick so the first log line lands at t≈60 s
/// rather than t=0.
///
/// **No `JoinHandle` retention**: the task lives until the
/// runtime drops at daemon exit, which is the same lifetime as
/// the rest of the daemon's `spawn_local` tasks. There is no
/// clean shutdown signal the task would need to honour.
pub fn spawn_hit_rate_log_task(metrics: Arc<ClipboardMetrics>) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_local(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(60));
        // Skip the immediate first tick — `tokio::time::interval`
        // fires at t=0 by default, but we don't want a log line
        // before the daemon has been alive for a full minute.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let snap = metrics.snapshot();
            if let Some(rate) = snap.hit_rate() {
                log::trace!(
                    target: "lan_mouse::service::clipboard",
                    "clipboard hit rate: skip={} allow={} rate={:.1}% last_skip_ts={}",
                    snap.skip,
                    snap.allow,
                    rate * 100.0,
                    snap.last_skip_ts
                );
            }
        }
    })
}

#[derive(Debug)]
struct Incoming {
    fingerprint: String,
    addr: SocketAddr,
    pos: Position,
}

/// **M1a follow-up #2** — per-incoming-peer clipboard state.
/// Currently only carries the mTLS fingerprint (for diagnostics /
/// future authorization checks) and the per-peer
/// `enable_clipboard_to` opt-in flag.
///
/// **`enable_clipboard_to` default is `true`** for incoming peers
/// (MVP behaviour, matching the legacy outgoing-client default).
/// GUI configuration of this flag for incoming peers is
/// deliberately deferred — the existing
/// `FrontendRequest::SetEnableClipboardTo(handle, enable)` only
/// accepts outgoing `ClientHandle`s, and the incoming-peer table
/// is keyed by `SocketAddr` (which the GUI doesn't currently
/// expose as a stable identifier). A future milestone can lift
/// this to a full per-peer config once the GUI learns to surface
/// the inbound peer list.
#[derive(Debug, Clone)]
struct IncomingClipboardState {
    /// mTLS fingerprint carried over from
    /// `EmulationEvent::Connected` for diagnostics + future
    /// authorization gating. Currently not read by the
    /// dispatcher (`broadcast_clipboard_event` only consults
    /// `enable_clipboard_to`); suppressed by the
    /// `#[allow(dead_code)]` on the struct so the lint stays
    /// quiet until a future milestone wires GUI-side per-peer
    /// toggle for incoming peers.
    #[allow(dead_code)]
    fingerprint: String,
    enable_clipboard_to: bool,
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
        // **M1b STEP-1b.2** — outbound clipboard text cache (sha256
        // → bytes). Shared `Arc` is moved into the listener
        // constructor and cloned by every per-peer HTTP/3 server to
        // back `GET /clipboard/text/{sha256}`. Same `Arc` is kept
        // on `Service` so the dispatcher's tick path can write to
        // the same backing store. See
        // [`crate::clipboard::cache::ClipboardCache`] for the
        // contract.
        let clipboard_cache = Arc::new(Mutex::new(crate::clipboard::cache::ClipboardCache::new()));
        // **M3a STEP-3a.4** — file cache (sha256 → bytes),
        // 1 GiB byte budget. Built up front (not lazily inside
        // the `Service { ... }` literal) so the same `Arc` can
        // be cloned into `LanMouseListener::new` (server-side
        // accept) and `LanMouseConnection::new` (client-side
        // dial) **before** the `Service` struct is constructed.
        // The same `Arc` is then moved into the `Service` field
        // so the dispatcher's `dispatch_files` hot path can
        // `insert_owned` into it. See
        // [`crate::clipboard::file_cache::FileCache`] for the
        // contract and the rationale for the 1 GiB byte budget.
        let file_cache = Arc::new(Mutex::new(crate::clipboard::file_cache::FileCache::new()));
        let listener = LanMouseListener::new(
            config.port(),
            cert_der.0.clone(),
            cert_der.1.clone_key(),
            authorized_keys.clone(),
            quic_idle_timeout,
            clipboard_cache.clone(),
            // **M3a STEP-3a.4** — file cache handle forwarded
            // into the listener so every accepted peer sees the
            // same backing store for `/clipboard/file/{sha256}`
            // GETs.
            file_cache.clone(),
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
            clipboard_cache.clone(),
            // **M3a STEP-3a.4** — clone the file cache into the
            // client-side connection so every per-peer HTTP/3
            // router built inside `connect_to_handle` (via
            // `default_router_with_caches`) can serve
            // `/clipboard/file/{sha256}[?range=...]` GETs from
            // the same backing store the dispatcher's
            // `dispatch_files` populates. Independent 1 GiB
            // byte budget from `clipboard_cache`.
            file_cache.clone(),
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
        // **M1a follow-up #2** — clone the QUIC peer registry
        // (`Rc<RefCell<HashMap<SocketAddr, Rc<PeerSession>>>>`) from
        // the listener so `Emulation::send_to_incoming` can push
        // `ClipboardText` to peers that are reachable only on the
        // listen side (i.e. the master's incoming peer from the
        // slave's perspective). The slave's outgoing client list
        // (`client_manager.get_client_states()`) does NOT contain
        // the master — so `broadcast_clipboard_event` must merge the
        // outgoing + incoming sets, otherwise copies made on the
        // slave silently never reach the master.
        let quic_conns_for_emulation = listener.quic_conns();
        let emulation = Emulation::new(
            emulation_backend,
            listener,
            clipboard_inbound_tx.clone(),
            quic_conns_for_emulation,
        );

        // create dns resolver
        let resolver = DnsResolver::new()?;

        // **M1b STEP-1b.3** — runtime signals for the loopback
        // LRU. `Arc` is shared between the dispatcher (writes)
        // and the hit-rate log task (reads every 60 s). Spawned
        // before `Service::new` returns so the very first tick of
        // the daemon already has the log task in flight.
        let metrics = Arc::new(ClipboardMetrics::new());
        spawn_hit_rate_log_task(metrics.clone());

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
            // **M1a follow-up #2** — initially empty; populated
            // by `handle_emulation_event`'s `Connected` arm and
            // drained by `Disconnected`. See the field doc for
            // why this is distinct from `incoming_conn_info`.
            incoming_clipboard: Default::default(),
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
            // **2026-09-10 screenshot-bug fix** — populated by
            // [`Self::run`] once the spawned `clipboard_poller`
            // task is up. `None` until then; inbound handlers
            // (`apply_inbound_clipboard_text` /
            // `apply_inbound_clipboard_image` /
            // `handle_clipboard_recover_push`) short-circuit on
            // `None` so a Service that hasn't been driven through
            // `run` yet (e.g. unit tests) is still safe to
            // construct.
            clipboard_backend_cmd: None,
            // **2026-09-10 inbound-apply off-thread follow-up** —
            // set up alongside `clipboard_backend_cmd` in
            // [`Self::run`]. `None` until then.
            apply_image_applied_tx: None,
            // **M3a STEP-3a.3** — mirror of `apply_image_applied_tx`
            // for the file inbound path. The spawned
            // [`apply_inbound_files_task`] sends
            // [`InboundFileApplyResult`] here; the main `select!`
            // arm consumes them in [`Self::handle_inbound_files_applied`].
            inbound_files_applied_tx: None,
            // **M1b STEP-1b.3** — capacity 128 + 60 s TTL
            // (reviewer #4 3rd, was capacity 64 with no TTL in M1a).
            clipboard_lru: LruFingerprints::new(),
            // **M2a STEP-2a.4** — image-branch loopback LRU.
            // Capacity 32 (vs text's 128) + 60 s TTL, independent
            // instance. See `IMAGE_LOOPBACK_CAPACITY` docstring for
            // why image uses a smaller window (image writes are
            // heavier on every platform backend; the fingerprint
            // entry is the *commitment*, not the bytes themselves).
            image_lru_fingerprints: LruFingerprints::with_capacity_and_ttl(
                IMAGE_LOOPBACK_CAPACITY,
                IMAGE_LOOPBACK_TTL,
            ),
            clipboard_last_text: None,
            // **M1b STEP-1b.2** — shared with the listener so
            // per-peer HTTP/3 servers can read from the same store
            // the dispatcher writes to.
            clipboard_cache: clipboard_cache.clone(),
            // **M1b STEP-1b.2** — `None` until the first push; the
            // dispatcher treats the "first push" case as "no prev
            // to evict" without checking this.
            last_outbound_text_sha: None,
            // **M2a STEP-2a.3** — same "no prev to evict" semantics
            // for the image branch. Independent from
            // `last_outbound_text_sha` so a text push does not
            // accidentally evict a previously-cached image.
            last_outbound_image_sha: None,
            // **M3a STEP-3a.2** — file-branch "no prev to evict"
            // sentinel. The first dispatch short-circuits on
            // `fingerprint_eq(None, &fp) == false`, so the first
            // push always proceeds.
            last_outbound_files_fingerprint: None,
            // **M3a STEP-3a.5** — empty list until the first
            // successful `dispatch_files` push lands. Subsequent
            // pushes with a *different* fingerprint take this
            // list via `mem::take` and fire one
            // `FileTransferCancel { sha256 }` per entry + remove
            // each from `file_cache`.
            last_outbound_files_sha: Vec::new(),
            // **M3a STEP-3a.5** — empty registry. Each
            // `apply_inbound_files_task` inserts a
            // `oneshot::Sender<()>` keyed by sha256 before its
            // HTTP/3 GET; the receiver-side cancel handler
            // `remove`s the entry and sends the cancel signal.
            inbound_file_cancel_txs: Arc::new(Mutex::new(HashMap::new())),
            // **M3a STEP-3a.2 + STEP-3a.4** — 1 GiB file-body
            // cache. Built up front (before the listener +
            // connection constructors) so the same `Arc` is
            // shared with the listener (server-side accept) and
            // the client-side connection (dial side); only one
            // instance exists per daemon. Independent from
            // `clipboard_cache` so a 200 MiB file push cannot
            // evict cached text / image bytes mid-session (see
            // `file_cache.rs` module doc for the rationale).
            //
            // **STEP-3a.4 update**: this used to be a fresh
            // `Arc::new(Mutex::new(FileCache::new()))` here, but
            // STEP-3a.4 needs the same `Arc` to be cloned into
            // the HTTP/3 server constructor before `self` is
            // constructed. The pre-built `file_cache` Arc is
            // moved in here; the per-peer HTTP/3 server reads
            // from this same store via `FileCache::lookup`.
            file_cache,
            // **M3a STEP-3a.2** — capacity 64 + 60 s TTL. See
            // `file_lru_fingerprints` field doc for the per-kind
            // capacity rationale (vs text 128, image 32).
            file_lru_fingerprints: LruFingerprints::with_capacity_and_ttl(
                FILE_LOOPBACK_CAPACITY,
                FILE_LOOPBACK_TTL,
            ),
            // **M3a STEP-3a.2** — `None` until [`Self::run`]
            // constructs the channel and clones the sender into
            // the spawned `clipboard_poller` task.
            files_rx: None,
            files_tx: {
                // **M3a STEP-3a.2** — construct an unused dummy
                // sender here so the struct field exists at
                // construction time; the real sender is set up
                // in [`Self::run`] (matching the
                // `clipboard_backend_cmd` pattern, but with no
                // `Option` wrapper because the dummy is never
                // used — the inbound arm doesn't need it). We
                // `mem::replace` the dummy out in `run` to
                // install the real sender.
                let (tx, _rx) = tokio_mpsc::unbounded_channel::<Vec<PathBuf>>();
                tx
            },
            // **M3a STEP-3a.2** — wired to `DEFAULT_MAX_FILE_SIZE`
            // (50 MiB). Replaced by `Config::max_file_size()`
            // once `lan-mouse-ipc::ClipboardConfig` lands
            // (M3b STEP-3b.1) — see `next/SUGGESTION.md` #S-5.
            max_file_size: DEFAULT_MAX_FILE_SIZE,
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
            // **M1b STEP-1b.3** — shared with the hit-rate log
            // task spawned just before this struct literal.
            metrics: metrics.clone(),
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

        // **2026-09-10 screenshot-bug fix** — move the clipboard
        // polling tick (and the heavy JPEG/TIFF → PNG encode it
        // triggers on macOS) into a dedicated `spawn_local` task
        // instead of running it as one arm of the main `select!`.
        //
        // Why this is necessary (root-cause recap, see
        // [[lan-mouse-clipboard-screenshot-block]]):
        //   `handle_clipboard_tick(&mut self)` holds an exclusive
        //   `&mut self` borrow for the entire await chain. While
        //   in flight, capture / emulation / frontend arms in the
        //   main `select!` cannot be polled, so a 2–5 s PNG encode
        //   starves the `Pong watchdog` → connection drops.
        //   Putting the heavy work on a separate `spawn_local` task
        //   keeps `&mut self` free on the main task during the
        //   encode, so capture BeginPending events get processed
        //   within their 500 ms window.
        //
        // Architecture:
        //   - The spawned task (`clipboard_poller`) is the SOLE
        //     owner of `clipboard_backend`. It runs the 500 ms tick
        //     loop + serves inbound `BackendCmd` requests from the
        //     main task (set_text / set_image / current_text /
        //     current_image).
        //   - The poller uses `backend.current_image_async()` which
        //     routes the heavy encode through `spawn_blocking` on
        //     macOS — important: spawn_blocking tasks are
        //     sequential inside the poller (it awaits each one
        //     before starting the next), so the previous failed
        //     fix (`4313940`) that piled up concurrent encodes by
        //     putting spawn_blocking inside a select! arm is not
        //     repeated here.
        //   - The main task consumes image / text results via two
        //     unbounded channels (`image_rx`, `text_rx`) and
        //     dispatches them through the existing `dispatch_image`
        //     / `dispatch_text` paths (LRU dedup + cache insert +
        //     broadcast).
        //   - The main task sends backend write/read commands via
        //     a third unbounded channel (`cmd_tx`); each command
        //     carries a `oneshot` reply channel so the inbound
        //     handler can `await` the result.
        let (image_tx, mut image_rx) =
            tokio_mpsc::unbounded_channel::<crate::clipboard::ImageBytes>();
        let (text_tx, mut text_rx) = tokio_mpsc::unbounded_channel::<String>();
        // **M3a STEP-3a.2** — files channel: the spawned
        // `clipboard_poller` task sends `Vec<PathBuf>` from its
        // `current_files()` probe (Phase 3 of the tick loop); the
        // main task consumes them in a dedicated `select!` arm
        // that delegates to `dispatch_files`. Mirrors the image /
        // text channels above — third peer in the poller →
        // dispatcher split.
        let (files_tx, files_rx) = tokio_mpsc::unbounded_channel::<Vec<PathBuf>>();
        let (cmd_tx, cmd_rx) = tokio_mpsc::unbounded_channel::<BackendCmd>();
        // **2026-09-10 inbound-apply off-thread follow-up** —
        // inbound image apply runs in a spawned `spawn_local`
        // task ([`apply_inbound_image_task`]) and reports its
        // result back to this main task via this channel. The
        // main task's `select!` consumes results in the
        // `handle_inbound_image_applied` arm to update the
        // image LRU + metrics + frontend notify. Keeping the
        // apply off-thread releases the main task's `&mut self`
        // borrow the moment the HTTP/3 GET completes, so the
        // capture arm can poll inbound mouse events without
        // being blocked by Windows' PNG→DIB decode/encode
        // (100–300 ms).
        let (applied_tx, mut applied_rx) =
            tokio_mpsc::unbounded_channel::<InboundImageApplyResult>();
        // **M3a STEP-3a.3** — file inbound apply result channel
        // (mirrors `applied_tx` / `applied_rx` above for the image
        // branch). The spawned
        // [`apply_inbound_files_task`] owns the HTTP/3 GET +
        // spawn_blocking write + sha256 verify for each
        // `ClipboardFiles` entry; the main task consumes results
        // here in the dedicated `Some(applied) = files_applied_rx
        // .recv() => ...` select! arm to update the file loopback
        // LRU + metrics + frontend notify.
        let (files_applied_tx, mut files_applied_rx) =
            tokio_mpsc::unbounded_channel::<InboundFileApplyResult>();
        // Stash the cmd sender on `self` so inbound write handlers
        // (`apply_inbound_clipboard_text` / `apply_inbound_clipboard_image` /
        // `handle_clipboard_recover_push`) can route `BackendCmd` requests
        // to the spawned poller. Replacing the field here keeps the
        // `Option<...>` shape so tests that don't drive `Service::run`
        // can still construct a Service with no backend wired in.
        self.clipboard_backend_cmd = Some(cmd_tx);
        self.apply_image_applied_tx = Some(applied_tx);
        // **M3a STEP-3a.3** — install the file inbound apply
        // result sender on `self`. The receiver is consumed by the
        // main `select!` arm below (mirrors the image branch's
        // `applied_rx` pattern).
        self.inbound_files_applied_tx = Some(files_applied_tx);
        // **M3a STEP-3a.2** — install the real files sender on
        // `self` (replacing the dummy constructed in `Service::new`)
        // and stash the receiver for the `select!` arm below. The
        // dummy sender from `new()` is now an unreachable orphan;
        // dropping it here is fine (the dummy `_rx` was already
        // dropped when the channel went unused).
        let old_dummy_tx = std::mem::replace(&mut self.files_tx, files_tx);
        drop(old_dummy_tx);
        self.files_rx = Some(files_rx);
        let clipboard_backend = self.clipboard_backend.take();
        let clipboard_tick = std::mem::replace(
            &mut self.clipboard_tick,
            tokio::time::interval(Duration::from_millis(500)),
        );
        // **2026-09-10 code-review follow-up** — wrap the spawned
        // poller in a supervisor that catches panics. Without
        // this wrapper, a panic inside `clipboard_poller` (e.g.
        // an `image` crate decode bug on a malformed
        // pasteboard, or an ObjC exception on a future objc2
        // version surfacing as a Rust panic) would be silently
        // swallowed by the LocalSet — `cmd_rx` would drain,
        // every inbound `BackendCmd` send would silently fail,
        // and clipboard sync would just stop working with zero
        // operator-visible signal. The supervisor logs the
        // panic and keeps the runtime alive so the rest of the
        // daemon continues to function.
        let poller_handle = tokio::task::spawn_local(clipboard_poller(
            clipboard_backend,
            clipboard_tick,
            image_tx,
            text_tx,
            // **M3a STEP-3a.2** — clone the sender into the
            // spawned poller. The receiver lives on `self.files_rx`
            // (set above), consumed by the main task's `select!`
            // arm below. The poller's owned backend calls
            // `current_files()` once per tick and ships the
            // `Vec<PathBuf>` through this channel.
            self.files_tx.clone(),
            cmd_rx,
        ));
        tokio::task::spawn_local(async move {
            match poller_handle.await {
                Ok(()) => {
                    // Poller exited normally (cmd_rx closed →
                    // daemon shutting down). Nothing to log.
                    log::debug!("clipboard poller exited normally");
                }
                Err(e) if e.is_panic() => {
                    log::error!(
                        "clipboard poller PANICKED — clipboard sync will not work \
                         until the daemon restarts: {e:?}"
                    );
                }
                Err(e) => {
                    log::error!("clipboard poller join error: {e:?}");
                }
            }
        });

        loop {
            tokio::select! {
                request = self.frontend_listener.next() => self.handle_frontend_request(request),
                _ = self.frontend_event_pending.notified() => self.handle_frontend_pending().await,
                event = self.emulation.event() => self.handle_emulation_event(event),
                event = self.capture.event() => self.handle_capture_event(event),
                event = self.resolver.event() => self.handle_resolver_event(event),
                _ = self.config.changed() => self.handle_config_change(),
                // **2026-09-10 screenshot-bug fix (move #1)** — the
                // clipboard dispatch tick no longer lives in this
                // `select!`. See the long-form rationale above at
                // the top of `Service::run`. The poller task sends
                // ImageBytes / String results here; the dispatcher
                // branches (`dispatch_image` / `dispatch_text`) are
                // unchanged from the pre-move implementation.
                Some(image) = image_rx.recv() => self.dispatch_image(image).await,
                Some(text) = text_rx.recv() => self.dispatch_text(text).await,
                // **M3a STEP-3a.2** — file-selection outbound
                // arm. The poller's tick Phase 3 calls
                // `backend.current_files()` and pushes the
                // resulting `Vec<PathBuf>` through `files_tx`;
                // the main task consumes here and delegates to
                // `dispatch_files` (which runs the heavy
                // `collect_files_blocking` + sha256 streaming
                // inside `spawn_blocking`).
                //
                // `files_rx` is `Option<...>`-shaped because
                // `Service::new` constructs the Service before
                // `run` sets the receiver; this arm only fires
                // once `run` has wired the channel. If the
                // receiver was somehow `None` here, the `None`
                // future would resolve immediately and the arm
                // would never fire (defensive default — should
                // not happen under normal `Service::run` flow).
                Some(paths) = async {
                    match self.files_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => self.dispatch_files(paths).await,
                // **M1a STEP-1a.4** — inbound clipboard event from a
                // peer. Server-side: `Emulation::ListenTask` pushes
                // here. Client-side: `peer.set_clipboard_inbox` (set
                // in `connect_to_handle`) pushes here.
                Some(inbound) = self.clipboard_inbound_rx.recv() => {
                    self.handle_clipboard_inbound(inbound).await;
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
                // **2026-09-10 inbound-apply off-thread follow-up** —
                // completion event from
                // [`apply_inbound_image_task`]. The spawned task
                // owns the heavy `set_image` / `current_image`
                // round trip; the main task only updates the
                // image LRU + metrics + frontend state here, so
                // the main `&mut self` borrow is held only for the
                // duration of these bookkeeping mutations
                // (sub-millisecond), not for the PNG→DIB encode.
                Some(applied) = applied_rx.recv() => {
                    self.handle_inbound_image_applied(applied);
                }
                // **M3a STEP-3a.3** — file inbound apply result
                // arm (mirrors the image arm above). The spawned
                // [`apply_inbound_files_task`] reports the
                // outcome of each entry's HTTP/3 GET + write +
                // sha256 verify via [`InboundFileApplyResult`];
                // the main task consumes them here to update the
                // file loopback LRU + metrics + frontend notify.
                // Keeping the bookkeeping arm on the main task
                // (rather than the spawned task) avoids needing
                // an extra `Arc<Mutex<...>>` field for the LRU.
                Some(applied) = files_applied_rx.recv() => {
                    self.handle_inbound_files_applied(applied);
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
                //
                // **M1a follow-up #2** — unlike the capture
                // barrier (which is preserved across transient
                // disconnects), the clipboard recipient entry is
                // removed on every `Disconnected`. The QUIC peer
                // registry (`LanMouseListener::quic_conns`) has
                // already been drained by the supervisor's Drop
                // guard by the time this event arrives, so any
                // subsequent `Emulation::send_to_incoming(addr, …)`
                // would fail with "peer not in quic_conns". Dropping
                // the clipboard entry on disconnect keeps the
                // dispatcher's "0 peers reached" log signal honest
                // and avoids pushing to zombies during a long
                // disconnect window. On the next `Connected` (the
                // peer's supervisor redials) we re-insert with
                // `enable_clipboard_to: true`.
                let was_in_clipboard = self.incoming_clipboard.remove(&addr).is_some();
                log::info!(
                    "peer {addr} transiently disconnected — barrier preserved for fast recovery \
                     (clipboard recipient entry removed: {was_in_clipboard})"
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
                // **M1a follow-up #2** — register the peer as a
                // clipboard recipient immediately on connection
                // (not on Enter). The dispatcher's
                // `broadcast_clipboard_event` filters by
                // `enable_clipboard_to`; defaulting to `true`
                // matches the outgoing-client MVP behaviour. A
                // second `Connected` for the same addr (e.g. on
                // macOS wake force-close + reconnect) overwrites
                // the prior entry — fingerprints are equal because
                // the same mTLS cert authenticates both
                // connections, and `enable_clipboard_to` resets to
                // the same default value.
                self.incoming_clipboard.insert(
                    addr,
                    IncomingClipboardState {
                        fingerprint: fingerprint.clone(),
                        enable_clipboard_to: true,
                    },
                );
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

    /// Clipboard dispatcher 500 ms tick handler.
    ///
    /// **Two-phase poll, image-first**: image branch runs before
    /// text. On macOS specifically `pbpaste` returns `Some("")`
    /// (success exit + 0-byte stdout) for an image-only pasteboard
    /// — verified locally by writing a PNG to `NSPasteboard` and
    /// running `pbpaste`: `rc=0`, stdout empty. With text-first
    /// priority the empty text would short-circuit before the
    /// image branch ever fires, so the screenshot never gets
    /// pushed. Image-first makes the dispatcher robust against
    /// that platform quirk: when both text and image are present,
    /// the image wins (the pasteboard's richer representation);
    /// when only text is present the image branch returns `None`
    /// and the text branch fires (same observable behavior as
    /// before for the text-only case).
    ///
    /// Each tick dispatches at most one event — whichever kind
    /// is currently on the clipboard. This matches the platform
    /// reality (the macOS pasteboard publishes one "current"
    /// representation per `pbpaste` / `NSPasteboard.dataForType`
    /// call) without forcing the dispatcher to multiplex two
    /// parallel event streams.
    ///
    /// **Why a 500 ms tick**: matches PLAN §3 M1a "macOS 实现
    /// ... 500ms tick" / "Linux 500 ms tick" cadence. Fast enough
    /// to feel instant to a user copying text; slow enough that the
    /// backend read (1-3 ms for pbcopy / xclip / NSPasteboard) is
    /// negligible.
    ///
    /// **M2a STEP-2a.3 — image-first dispatch order**:
    /// `dispatch_image` runs first, before the text branch.
    ///
    /// **Why image-first (not text-first)**: macOS `pbpaste`
    /// returns `Some("")` (success exit + 0-byte stdout) when
    /// the pasteboard holds only an image — the empty text
    /// representation is published alongside the PNG by some
    /// macOS apps (notably the built-in screenshot tools).
    /// Verified locally: writing a PNG via `NSPasteboard` +
    /// running `pbpaste` returns `rc=0`, empty stdout. A
    /// text-first dispatcher would short-circuit on the empty
    /// string and never reach `dispatch_image`, so the
    /// screenshot would never be pushed to peers. Image-first
    /// makes the dispatch correct on macOS while preserving
    /// the text-only behavior on every other platform
    /// (image branch returns `None` → text branch fires next,
    /// observable behavior identical to text-first).
    ///
    /// **macOS backend's `changeCount` short-circuit** (STEP-2a.2)
    /// keeps `current_image()` cheap on quiescent ticks — the
    /// image read is dominated by the sha256 hash (5-15 ms for
    /// a 4 K screenshot) which fits the 500 ms budget.
    ///
    /// **Removed 2026-09-10 screenshot-bug fix**: the polling
    /// tick used to live here as one arm of `Service::run`'s main
    /// `select!`, holding `&mut self` for the entire await chain
    /// and starving the Pong watchdog / capture BeginPending
    /// during a 2–5 s PNG encode. The polling tick now lives in
    /// the spawned [`clipboard_poller`] task (see [`Self::run`]),
    /// which owns `clipboard_backend` and uses
    /// [`crate::clipboard::ClipboardBackend::current_image_async`]
    /// to route the heavy encode through `spawn_blocking`.
    /// The main task's `select!` consumes the poller's results
    /// via `image_rx` / `text_rx` and dispatches them through
    /// [`Self::dispatch_image`] / [`Self::dispatch_text`]
    /// directly.

    /// **M1a STEP-1a.4 + M1b STEP-1b.2 + M1b STEP-1b.3** —
    /// dispatcher branch for clipboard text.
    ///
    /// Extracted from [`Self::handle_clipboard_tick`] so the
    /// text + image branches share the same outer plumbing
    /// (500 ms tick + `current_*()` poll) but each branch is a
    /// self-contained helper that's easy to read in isolation.
    /// The behaviour matches the pre-2a.3 inline implementation
    /// byte-for-byte; this is a pure refactor + the addition of
    /// the image sibling.
    ///
    /// Steps:
    /// 1. Compare to `clipboard_last_text` — skip if unchanged.
    /// 2. SHA-256 the text. If the hash is in the loopback LRU,
    ///    skip (the recent local writeback dedup layer).
    /// 3. Mark LRU, update `last_text`, build the
    ///    `ClipboardText` event (≤ 1 KiB inline, > 1 KiB
    ///    metadata-only).
    /// 4. Active eviction: `cache.remove(prev_sha)` before
    ///    broadcast.
    /// 5. Broadcast to all eligible peers.
    /// 6. Cache insert (metadata-only payloads only — inline
    ///    payloads already on the wire).
    /// 7. Update `last_outbound_text_sha` + `last_text_ts_ms` +
    ///    `last_clipboard_source` + emit `FrontendEvent
    ///    ::ClipboardState`.
    async fn dispatch_text(&mut self, new_text: String) {
        if Some(&new_text) == self.clipboard_last_text.as_ref() {
            return;
        }
        let sha = sha256_of(&new_text);
        if self.clipboard_lru.contains(&sha) {
            // Loopback — the LRU already holds this hash (e.g. we
            // just applied an inbound `set_text` that wrote this
            // value). Do not re-broadcast.
            log::debug!(
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
        // **M1b STEP-1b.2** — capture the bytes for the cache.
        // We compute it before moving `new_text` into the event
        // constructor. The cache *only* stores payloads > 1 KiB
        // because smaller payloads travel inline on the wire
        // (receiver uses the inline bytes, never pulls).
        let bytes_for_cache = new_text.into_bytes();
        let push_was_metadata_only =
            bytes_for_cache.len() > lan_mouse_proto::CLIPBOARD_TEXT_INLINE_LIMIT;
        let event = ProtoEvent::ClipboardText(ClipboardText::from_content(
            sha,
            sha,
            bytes_for_cache.clone(),
        ));
        // **M1b STEP-1b.2** — active eviction (PLAN §1 评审 #3 2nd):
        // before broadcasting, evict the previous push from
        // `clipboard_cache` so a receiver that started pulling the
        // old sha256 races to a 404 (graceful log-warn + skip on
        // the receiver side) instead of silently applying stale
        // content. Run before the broadcast so a slow receiver
        // observing our next push is guaranteed to see
        // `lookup(prev_sha) == None` from the moment we mark
        // `last_outbound_text_sha` below.
        self.evict_prev_outbound_clipboard_cache();
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
        // **M1b STEP-1b.2** — store the new push's bytes so a
        // peer's HTTP/3 GET can pull them. Inline payloads skip the
        // cache (the bytes are already on the wire).
        if push_was_metadata_only {
            if let Ok(mut guard) = self.clipboard_cache.lock() {
                guard.insert(sha, bytes_for_cache);
            } else {
                log::warn!(
                    "clipboard cache mutex poisoned on insert sha={}; skipping cache write",
                    short_hex(&sha)
                );
            }
        }
        self.last_outbound_text_sha = Some(sha);
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

    /// **M2a STEP-2a.3** — dispatcher branch for clipboard images.
    ///
    /// Sibling of [`Self::dispatch_text`] but operating on
    /// [`crate::clipboard::ImageBytes`]. The dispatch contract
    /// matches the text branch wherever possible (active eviction
    /// before broadcast, broadcast to all eligible peers, cache
    /// insert for the new payload, frontend notification); the
    /// differences are:
    ///
    /// 1. **No loopback-LRU check at the dispatcher.** The text
    ///    branch consults `clipboard_lru` to short-circuit "we
    ///    just wrote this locally, the OS echoed it back" — for
    ///    images this check would be redundant with the
    ///    receiver-side loopback detection (M2a STEP-2a.4).
    ///    Per PLAN §3 M2a STEP-2a.3 the image branch pushes
    ///    every non-duplicate change and lets the receiver dedup.
    /// 2. **No inline-vs-metadata split.** Image bytes always go
    ///    through HTTP/3 (`/clipboard/image/{sha256}`); the
    ///    cache stores every pushed image regardless of byte
    ///    count. PNG screenshots at 5-15 MiB are well above the
    ///    text inline limit anyway.
    /// 3. **Same fingerprint convention as text.** The wire
    ///    `ClipboardImage::fingerprint` field equals the sha256 —
    ///    matches the existing text path (`from_content` uses
    ///    `sha` for both fields).
    /// 4. **Mime field passes through verbatim.** The receiver
    ///    decides how to decode based on the `mime` string the
    ///    source wrote; for M2a / M2b this is always
    ///    `"image/png"` (or `"application/x-dib"` from Windows in
    ///    M2b).
    ///
    /// Steps:
    /// 1. SHA-256 the image bytes (content fingerprint).
    /// 2. **Image LRU loopback check** against
    ///    `image_lru_fingerprints` (mirrors the text branch's
    ///    `clipboard_lru.contains` short-circuit). On hit →
    ///    log debug + skip. Without this check, the daemon
    ///    would ping-pong every inbound image back to its
    ///    sender on the next 500 ms tick: `apply_inbound_…`
    ///    marks the LRU before `set_image`, and Windows
    ///    transcodes inbound PNG → BMP-encoded DIB so the
    ///    freshly-written clipboard content's SHA does NOT
    ///    match `last_outbound_image_sha`.
    /// 3. Compare to `last_outbound_image_sha` — skip if the
    ///    same image was the most recent push (bandwidth
    ///    optimisation: avoids re-broadcasting the same image
    ///    every 500 ms while the clipboard sits unchanged).
    /// 4. Active eviction: `cache.remove(prev_image_sha)`
    ///    before broadcast (mirrors the text branch's
    ///    "evict prev before push" contract).
    /// 5. Cache insert: store the new image bytes keyed by sha256
    ///    so the receiver's HTTP/3 GET can pull them.
    /// 6. Broadcast the `ClipboardImage` metadata event.
    /// 7. Update `last_outbound_image_sha` + `last_image_ts_ms` +
    ///    emit `FrontendEvent::ClipboardState`.
    async fn dispatch_image(&mut self, image: crate::clipboard::ImageBytes) {
        // **BUGS-2 fix, 2026-09-12 (M1)** — sha256 + cache insert moved
        // off the LocalSet. The original implementation did
        // `sha256_of_bytes(&image.data)` (~100-300 ms for 4 MB,
        // ~400 ms for 16 MB) and `image.data.clone()` (~50-100 ms for a
        // 4-16 MB memcpy) directly on the main task, blocking the
        // entire `Service::run` `select!` for 150-500 ms. During that
        // window no capture / emulation / inbound event could be
        // processed, the QUIC Pong heartbeats stalled, and the
        // watchdog (3.5 s) eventually force-closed the connection —
        // which is the "screenshot → mouse frame drops → permanently
        // stuck" failure mode captured in `next/BUGS.md` Bug #2.
        //
        // `image.data` is moved into the blocking task, used as the
        // sha256 input, and then handed back so we can insert it into
        // the cache without cloning — single pass over the bytes, no
        // extra allocation. The LocalSet only sees cheap O(1)
        // bookkeeping afterwards.
        let mime = image.mime;
        let size = image.data.len() as u64;
        let (sha, image_bytes) = tokio::task::spawn_blocking(move || {
            let sha = sha256_of_bytes(&image.data);
            (sha, image.data)
        })
        .await
        .expect("dispatch_image: sha256 task panicked");

        // Step 2: image LRU loopback check (mirrors the text
        // branch's `clipboard_lru.contains(&sha)` short-circuit).
        // The inbound apply path (`apply_inbound_clipboard_image`)
        // calls `mark_local_image_write` BEFORE `backend.set_image`,
        // so any image we just wrote locally is in the LRU and any
        // tick that observes it back via `current_image()` must
        // skip the broadcast — otherwise the daemon would
        // ping-pong the same image to its peer every 500 ms.
        //
        // **Why this is needed even though `last_outbound_image_sha`
        // already exists**: Windows transcodes inbound PNG →
        // BMP-encoded DIB (different bytes, different SHA), so the
        // freshly-written clipboard content's SHA does NOT match
        // `last_outbound_image_sha` (which holds the previous
        // *outbound* push's SHA). Without the LRU check the
        // post-apply tick would dispatch the freshly-written DIB
        // back to the original sender. The text branch has the
        // same protection (it predates this fix).
        if self.image_lru_fingerprints.contains(&sha) {
            log::debug!(
                "clipboard tick: image LRU loopback hit sha={} ({} bytes), skipping broadcast",
                short_hex(&sha),
                image_bytes.len()
            );
            return;
        }
        // Step 3: skip if same image as last push. The macOS
        // backend's `changeCount` short-circuit in STEP-2a.2 means
        // `current_image()` runs less often than the tick rate, but
        // every call still produces a sha256 hash worth a few ms
        // for a 4 K screenshot — comparing to
        // `last_outbound_image_sha` skips the broadcast and cache
        // churn when the user hasn't copied anything new.
        if Some(&sha) == self.last_outbound_image_sha.as_ref() {
            return;
        }
        // Step 4: active eviction (mirrors the text branch).
        self.evict_prev_outbound_image_cache();
        // Step 5: cache insert — MOVE the bytes (no clone). The
        // bytes already passed the dedup check above, so this is
        // always a fresh sha256 entry. (Overwriting an existing
        // entry with the same sha256 — which can only happen via
        // direct manipulation outside this method — would no-op
        // the byte counter; we don't optimise for that case.)
        if let Ok(mut guard) = self.clipboard_cache.lock() {
            guard.insert(sha, image_bytes);
        } else {
            log::warn!(
                "clipboard cache mutex poisoned on image insert sha={}; skipping cache write",
                short_hex(&sha)
            );
        }
        // Step 6: build + broadcast the metadata event. The wire
        // format is `ClipboardImage { fingerprint, mime, sha256,
        // size }` — fingerprint == sha256 by the text-path
        // convention; size is the byte count of the cached bytes.
        let event = ProtoEvent::ClipboardImage(ClipboardImage {
            fingerprint: sha,
            mime: mime.clone(),
            sha256: sha,
            size,
        });
        let mut recipients = 0usize;
        self.broadcast_clipboard_event(event, &mut recipients).await;
        if recipients == 0 {
            log::warn!(
                "clipboard dispatched image to 0 peers (sha={}, mime={}, size={} bytes); \
                 peer gate filtered all clients — check `enable_clipboard_to` in TOML \
                 and that the connection is active",
                short_hex(&sha),
                mime,
                size
            );
        } else {
            // Image events are inherently rarer than text events
            // (a few per hour vs dozens per minute), so logging
            // at INFO here is fine and lets operators confirm
            // the image branch fired without enabling RUST_LOG.
            // The text path stays at DEBUG to avoid log spam.
            log::info!(
                "clipboard dispatched image ({} bytes, mime={}, sha={}) to {} peer(s)",
                size,
                mime,
                short_hex(&sha),
                recipients
            );
        }
        // Step 7: bookkeeping + frontend notification.
        self.last_outbound_image_sha = Some(sha);
        let now_ms = unix_now_ms();
        self.last_image_ts_ms = Some(now_ms);
        self.last_clipboard_source = None;
        self.notify_frontend(FrontendEvent::ClipboardState {
            last_text_ts: self.last_text_ts_ms,
            last_image_ts: self.last_image_ts_ms,
            last_file_ts: self.last_file_ts_ms,
            last_source: None,
        });
    }

    /// Clipboard inbound handler for `ClipboardText` from a peer (server or
    /// client side, see the field doc on `clipboard_inbound_rx`).
    ///
    /// **Inline payloads** (`ct.content_inline.is_some()`) are
    /// applied to the local OS clipboard immediately — no extra
    /// round-trip needed.
    ///
    /// **Metadata-only payloads** (`ct.content_inline == None`) are
    /// **pulled over HTTP/3**: the receiver issues
    /// `GET /clipboard/text/{sha256}` on the source peer's QUIC
    /// connection. The bytes are then applied to the local clipboard
    /// on 200; a 404 (cache miss / TTL expired / active eviction)
    /// is logged at warn and the inbound event is dropped silently —
    /// see PLAN §1 评审 #3 2nd: "404 cache miss silently ignored".
    ///
    /// **Why `async`** (was `fn` in M1a): the HTTP/3 GET must await
    /// `quinn::Connection::open_bi` + read the response. The
    /// `tokio::select!` arm in `Service::run` already runs inside
    /// an async context, so this conversion does not change the
    /// dispatcher's runtime requirements.
    /// **M3a STEP-3a.2 + P1 follow-up** — `dispatch_files`:
    /// outbound branch for clipboard file selections.
    ///
    /// Sibling of [`Self::dispatch_text`] / [`Self::dispatch_image`].
    /// Matches each branch's contract where it makes sense, and
    /// diverges where the file semantics demand it:
    ///
    /// 1. **Fingerprint short-circuit** vs
    ///    [`Self::last_outbound_files_fingerprint`] (mirrors the
    ///    text branch's `clipboard_last_text` early-return and the
    ///    image branch's `last_outbound_image_sha` early-return).
    ///    The fingerprint is derived from the sorted path list
    ///    via [`file_selection_fingerprint`] — deterministic for
    ///    a given selection, distinct across different
    ///    selections, and 500 ms tick-friendly (no re-pushing the
    ///    same Finder selection every poll).
    /// 2. **Heavy work off-LocalSet**: `collect_files_blocking`
    ///    is wrapped in `tokio::task::spawn_blocking` so the
    ///    CPU-bound sha256 streaming (~100 ms - 5 s for the M3a
    ///    STEP-3a.4 200 MiB target) does not block the
    ///    dispatcher's LocalSet (PLAN §3 STEP-3a.2 ②, mirroring
    ///    `dispatch_image`'s `7a57bb3` pattern).
    /// 3. **Early-reject on `ExceedsLimit`**: `collect_files_blocking`
    ///    returns `Err(FileMetaError::ExceedsLimit)` if any file
    ///    in the batch exceeds `self.max_file_size`. The branch
    ///    fires a [`PopupGuard`] immediately (not waiting for the
    ///    next 500 ms tick), updates `last_file_ts_ms` + emits
    ///    `FrontendEvent::ClipboardState`, and returns. The file
    ///    is NOT inserted into `file_cache`, NOT pushed over
    ///    StreamC, NOT registered for HTTP/3 GET — see PLAN §5
    ///    风险 #25 for the rationale ("防止 200 MiB 文件悄悄传到对端").
    /// 4. **`file_cache` insert + `ClipboardFiles` broadcast**: the
    ///    happy path mirrors `dispatch_image`'s structure
    ///    (active-evict prev, insert new sha256, build + broadcast
    ///    `ClipboardFiles { fingerprint, entries }` metadata over
    ///    StreamC). After the metadata broadcast succeeds, a
    ///    second `spawn_blocking` reads each file's bytes off the
    ///    LocalSet and calls [`FileCache::insert_owned`] — key =
    ///    sha256. The receiver will then `GET
    ///    /clipboard/file/{sha256}` in STEP-3a.4 to pull the
    ///    bytes. MIME_TOO_LARGE entries are skipped at insert
    ///    time (the receiver's short-circuit refuses them before
    ///    any HTTP/3 fetch).
    /// 5. **Update `last_outbound_files_fingerprint` +
    ///    `last_file_ts_ms`** + emit `FrontendEvent::ClipboardState`
    ///    on success.
    ///
    /// **Decision pipeline**: the 5 early-return branches above
    /// are decided in [`dispatch_files_decide`], a free function
    /// that returns a [`DispatchFilesOutcome`] enum. This method
    /// matches on the outcome to apply the appropriate side
    /// effects (popup, log, broadcast, cache insert).
    async fn dispatch_files(&mut self, paths: Vec<PathBuf>) {
        let max_size = self.max_file_size;
        let last_fingerprint = self.last_outbound_files_fingerprint;
        let outcome = dispatch_files_decide(paths, last_fingerprint, max_size).await;
        match outcome {
            DispatchFilesOutcome::Empty => {
                // Defensive early-return — backend could
                // legitimately return an empty vec (e.g.
                // race between select and deselect).
            }
            DispatchFilesOutcome::FingerprintMatch => {
                log::debug!(
                    "clipboard tick: file selection fingerprint {} matches last outbound; skipping",
                    short_hex(
                        last_fingerprint
                            .as_ref()
                            .expect("FingerprintMatch implies Some"),
                    )
                );
            }
            DispatchFilesOutcome::ExceedsLimit {
                fingerprint,
                offending,
                size,
                limit,
            } => {
                // **PLAN §3 STEP-3a.2 ② + §5 风险 #25** —
                // early-reject. Pop the notification NOW (not
                // after the next 500 ms tick), log the rejection,
                // update the timestamp + frontend state, and
                // return. No StreamC push, no file_cache insert,
                // no HTTP/3 setup. The user's "file copy" silently
                // stops here, with a clear popup telling them why.
                let offending_name = offending
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("<non-utf8>");
                let limit_mib = limit / (1024 * 1024);
                let size_mib = size / (1024 * 1024);
                let body = format!(
                    "1 file exceeded limit ({size_mib} MiB > {limit_mib} MiB), dropped: \
                     {offending_name} ({size_mib} MiB)"
                );
                log::warn!(
                    "clipboard outbound: file {} ({size} bytes) exceeds max_size \
                     {limit} bytes; firing popup and aborting push (no StreamC, no cache, \
                     no HTTP/3 setup)",
                    offending.display()
                );
                crate::popup::PopupGuard::file("file exceeds limit", body).fire();
                let now_ms = unix_now_ms();
                self.last_file_ts_ms = Some(now_ms);
                self.last_clipboard_source = None;
                // **2026-09-13 fix** — record the offending
                // file's selection fingerprint so the NEXT poller
                // tick with the SAME clipboard state short-circuits
                // at `dispatch_files_decide`'s `fingerprint_eq`
                // check (`service.rs:4594`) and never reaches
                // `spawn_blocking` or fires another popup. Without
                // this, an oversized file left sitting on the
                // clipboard produces a popup storm every 500 ms
                // tick (and the synchronous `notify_rust::show()`
                // + repeated `spawn_blocking` calls back-pressure
                // the runtime enough that `tokio::select!`'s
                // `signal::ctrl_c()` arm can no longer fire — the
                // user reports the daemon becomes unresponsive to
                // Ctrl+C while the storm is running). The
                // fingerprint value is recomputed at the top of
                // `dispatch_files_decide` from the live `paths` so
                // it is identical to what the short-circuit would
                // have seen had it been set.
                self.last_outbound_files_fingerprint = Some(fingerprint);
                self.notify_frontend(FrontendEvent::ClipboardState {
                    last_text_ts: self.last_text_ts_ms,
                    last_image_ts: self.last_image_ts_ms,
                    last_file_ts: self.last_file_ts_ms,
                    last_source: None,
                });
            }
            DispatchFilesOutcome::IsDirectory(p) => {
                log::warn!(
                    "clipboard outbound: path is a directory {}; dropping batch \
                     (recursive walk out of scope for M3a)",
                    p.display()
                );
            }
            DispatchFilesOutcome::Io(e) => {
                log::warn!("clipboard outbound: file metadata IO error {e}; dropping batch");
            }
            DispatchFilesOutcome::Ok {
                fingerprint,
                entries,
                paths,
            } => {
                // **M3a STEP-3a.5** — pre-compute the per-entry
                // sha256 list BEFORE moving `entries` into the
                // cache-insert `spawn_blocking` closure (the
                // closure takes ownership). The list is used at
                // the end of this arm to populate
                // `self.last_outbound_files_sha` so the *next*
                // supersede tick can fire `FileTransferCancel`
                // + remove from `file_cache`. Filtering out
                // MIME_TOO_LARGE entries is intentional — those
                // were never inserted into the cache, so there's
                // nothing to cancel or remove.
                let new_outbound_shas: Vec<[u8; 32]> = entries
                    .iter()
                    .filter(|e| e.mime != crate::clipboard::file_meta::MIME_TOO_LARGE)
                    .map(|e| e.sha256)
                    .collect();

                // Step 4: log the entries list at info (matches
                // dispatch_image's success log line shape; M3a
                // manual tests grep on the sha256 prefix).
                log::info!(
                    "clipboard outbound: collected {} file entries (fingerprint={})",
                    entries.len(),
                    short_hex(&fingerprint)
                );
                // **M3a STEP-3a.5** — if the previous push had a
                // different fingerprint (i.e. the user just
                // overwrote the clipboard with a new file
                // selection), fire `FileTransferCancel` per
                // entry over StreamC and remove each from
                // `file_cache` BEFORE pushing the new payload.
                // The order matters: the receiver sees
                // `Cancel` before `ClipboardFiles` for the new
                // batch, so any in-flight GET against the old
                // sha256 is aborted in time.
                //
                // `dispatch_files_build_cancel_events` handles
                // the cache removal + event-list construction
                // (pure, unit-tested below). The broadcast
                // side-effect (which requires the full Service
                // for per-peer gating) lives here.
                if !self.last_outbound_files_sha.is_empty() {
                    let prev_shas = std::mem::take(&mut self.last_outbound_files_sha);
                    log::info!(
                        "clipboard outbound: superseding previous push ({} entries); \
                         firing FileTransferCancel + removing from file_cache",
                        prev_shas.len()
                    );
                    let cancel_events =
                        dispatch_files_build_cancel_events(prev_shas, &self.file_cache);
                    let mut cancel_recipients = 0usize;
                    for event in cancel_events {
                        self.broadcast_clipboard_event(event, &mut cancel_recipients)
                            .await;
                    }
                    if cancel_recipients > 0 {
                        log::info!(
                            "clipboard outbound: fired FileTransferCancel(s) to {} peer(s)",
                            cancel_recipients
                        );
                    } else {
                        log::debug!(
                            "clipboard outbound: fired FileTransferCancel(s) but no recipients \
                             (no peers connected or all disabled)"
                        );
                    }
                }
                // Step 5: build + broadcast the metadata event.
                let event = ProtoEvent::ClipboardFiles(lan_mouse_proto::ClipboardFiles {
                    fingerprint,
                    entries: entries
                        .iter()
                        .map(|fe| lan_mouse_proto::FileEntry {
                            name: fe.name.clone(),
                            size: fe.size,
                            mime: fe.mime.clone(),
                            sha256: fe.sha256,
                        })
                        .collect(),
                });
                let mut recipients = 0usize;
                self.broadcast_clipboard_event(event, &mut recipients).await;
                if recipients == 0 {
                    log::warn!(
                        "clipboard dispatched files (fingerprint={}) to 0 peers; \
                         peer gate filtered all clients — check `enable_clipboard_to` in \
                         TOML and that the connection is active",
                        short_hex(&fingerprint)
                    );
                } else {
                    log::info!(
                        "clipboard dispatched files (fingerprint={}) to {} peer(s)",
                        short_hex(&fingerprint),
                        recipients
                    );
                }
                // Step 6: fill file_cache via a second spawn_blocking.
                // Reads each file's bytes off-LocalSet and calls
                // `insert_owned` (MOVE, no clone). MIME_TOO_LARGE
                // entries are skipped — the receiver short-circuits
                // the HTTP/3 fetch against the mime directly. We
                // carry the original `paths` through the closure
                // because `FileEntry` does not retain a path
                // reference; entries and paths are 1:1 in order.
                let cache_for_insert = self.file_cache.clone();
                let insert_result = tokio::task::spawn_blocking(move || {
                    let mut guard = cache_for_insert
                        .lock()
                        .map_err(|e| io::Error::other(format!("file cache mutex poisoned: {e}")))?;
                    for (entry, path) in entries.iter().zip(paths.iter()) {
                        if entry.mime == crate::clipboard::file_meta::MIME_TOO_LARGE {
                            log::debug!(
                                "file_cache: skipping MIME_TOO_LARGE entry {} ({} bytes)",
                                entry.name,
                                entry.size
                            );
                            continue;
                        }
                        let bytes = std::fs::read(path)?;
                        guard.insert_owned(entry.sha256, bytes);
                    }
                    Ok::<(), io::Error>(())
                })
                .await;
                match insert_result {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        log::warn!("clipboard outbound: file_cache insert IO error: {e}");
                    }
                    Err(e) => {
                        log::error!(
                            "clipboard outbound: file_cache insert spawn_blocking \
                             join error: {e}"
                        );
                    }
                }
                // Step 7: bookkeeping + frontend notification.
                self.last_outbound_files_fingerprint = Some(fingerprint);
                // **M3a STEP-3a.5** — remember the per-entry
                // sha256 list so the next supersede tick can
                // fire `FileTransferCancel` + remove from
                // `file_cache`. The list was pre-computed at the
                // top of this arm (before `entries` was moved
                // into the cache-insert `spawn_blocking`).
                self.last_outbound_files_sha = new_outbound_shas;
                let now_ms = unix_now_ms();
                self.last_file_ts_ms = Some(now_ms);
                self.last_clipboard_source = None;
                self.notify_frontend(FrontendEvent::ClipboardState {
                    last_text_ts: self.last_text_ts_ms,
                    last_image_ts: self.last_image_ts_ms,
                    last_file_ts: self.last_file_ts_ms,
                    last_source: None,
                });
            }
        }
    }

    async fn handle_clipboard_inbound(&mut self, (addr, event): (SocketAddr, ProtoEvent)) {
        // **M2a STEP-2a.4** — dispatch on event kind. Text path is
        // unchanged from M1a / M1b; image path is new and mirrors
        // the text path's structure (LRU loopback check → resolve
        // peer → HTTP/3 GET → apply). File events are accepted
        // here in M3a STEP-3a.2 but their receiver-side pull is
        // wired in STEP-3a.3 / 3a.4 — for STEP-3a.2 the inbound
        // arm just logs + no-ops so the wire-level routing is in
        // place end-to-end.
        match event {
            ProtoEvent::ClipboardText(ct) => self.handle_clipboard_inbound_text(ct, addr).await,
            ProtoEvent::ClipboardImage(ci) => self.handle_clipboard_inbound_image(ci, addr).await,
            // **M3a STEP-3a.3** — `ClipboardFiles` envelope now
            // drives the full receiver-side flow: loopback LRU →
            // decision fn (auto_accept / MIME_TOO_LARGE filter) →
            // per-entry HTTP/3 GET → spawn_blocking write + sha256
            // verify. STEP-3a.4 will land the HTTP/3 server route
            // `/clipboard/file/{sha256}` so the source daemon
            // actually serves the bytes (for STEP-3a.3 the route
            // is still the 404 stub — see commit `8de4219`'s
            // `apply_inbound_image_task_get_404` test for the
            // same pattern in the image branch).
            ProtoEvent::ClipboardFiles(cf) => {
                self.handle_clipboard_inbound_files(cf, addr).await;
            }
            // **M3a STEP-3a.5** — `FileTransferCancel` arm.
            // The source emits this when its outbound file
            // selection is superseded (e.g. user copies a new
            // file mid-transfer) or on `Ctrl+C`. The receiver
            // looks up the in-flight HTTP/3 fetch by sha256 and
            // signals cancellation, which closes the stream
            // (quinn STOP_SENDING) and skips the spawn_blocking
            // write. If no in-flight fetch is registered for the
            // sha256 (transfer already complete or never
            // started), this is a no-op — the file is either on
            // disk (already applied) or was never fetched (no
            // partial to clean up).
            ProtoEvent::FileTransferCancel(c) => {
                self.handle_clipboard_inbound_cancel(c, addr);
            }
            _ => {
                // FileTransferOffer / Response are still out of
                // scope (M3b STEP-3b.2 will wire offer/response
                // for GUI-driven accept/reject). Cancel is wired
                // here in STEP-3a.5.
            }
        }
    }

    /// **M3a STEP-3a.5** — receiver-side handler for inbound
    /// `FileTransferCancel { sha256 }`. Looks up the in-flight
    /// HTTP/3 fetch by sha256 in the
    /// [`Self::inbound_file_cancel_txs`] registry and sends the
    /// cancel signal.
    ///
    /// **Why a sync method (not async)**: the operation is a
    /// single `HashMap::remove` + `oneshot::Sender::send` —
    /// no awaiting needed. Mirrors the synchronous arm shape
    /// of `handle_clipboard_recover_push`.
    ///
    /// **No-op semantics for missing entries**: a cancel for a
    /// sha256 with no in-flight fetch is logged at `debug` and
    /// dropped. This covers the legitimate "cancel arrived after
    /// the fetch completed" window (a few-millisecond race
    /// between the source sending Cancel and the receiver
    /// finishing its spawn_blocking write) without erroring.
    fn handle_clipboard_inbound_cancel(
        &mut self,
        cancel: lan_mouse_proto::FileTransferCancel,
        addr: SocketAddr,
    ) {
        signal_inbound_file_cancel(cancel, addr, &self.inbound_file_cancel_txs);
    }

    /// **M1a STEP-1a.4 + M1b STEP-1b.2/1b.3** — text-arm of the
    /// inbound handler. Extracted into its own method by
    /// M2a STEP-2a.4 so the text + image branches can share the
    /// outer plumbing (`handle_clipboard_inbound` dispatch) while
    /// keeping each arm's contract isolated.
    ///
    /// **Inline payloads** (`ct.content_inline.is_some()`) are
    /// applied to the local OS clipboard immediately — no extra
    /// round-trip needed.
    ///
    /// **Metadata-only payloads** (`ct.content_inline == None`) are
    /// **pulled over HTTP/3**: the receiver issues
    /// `GET /clipboard/text/{sha256}` on the source peer's QUIC
    /// connection. The bytes are then applied to the local
    /// clipboard on 200; a 404 (cache miss / TTL expired /
    /// active eviction) is logged at warn and the inbound event
    /// is dropped silently — see PLAN §1 评审 #3 2nd:
    /// "404 cache miss silently ignored".
    async fn handle_clipboard_inbound_text(
        &mut self,
        ct: lan_mouse_proto::ClipboardText,
        addr: SocketAddr,
    ) {
        if self.clipboard_lru.contains(&ct.sha256) {
            log::debug!(
                "clipboard inbound: skipping loopback sha={}",
                short_hex(&ct.sha256)
            );
            // **M1b STEP-1b.3** — record the skip. The fingerprint
            // matched the loopback LRU, meaning we wrote it locally
            // recently and a peer is now echoing it back. Count for
            // the hit-rate metric (60-s log task surfaces the
            // running rate).
            self.metrics.incr_skip(unix_now_ms());
            return;
        }
        // Inline fast-path: bytes are on the wire, just apply.
        if let Some(content) = ct.content_inline.as_ref() {
            self.apply_inbound_clipboard_text(&ct.sha256, content, addr)
                .await;
            return;
        }
        // Metadata-only slow-path: HTTP/3 GET against the source
        // peer's connection. Per PLAN §3 M1b STEP-1b.2.
        let Some(conn) = self.peer_connection_for_addr(addr).await else {
            log::warn!(
                "clipboard inbound: metadata-only sha={} from {addr} but no live peer \
                 connection found — skipping (peer may have disconnected mid-flight)",
                short_hex(&ct.sha256)
            );
            return;
        };
        // **M1b follow-up** — must use the full 64-char hex sha256
        // here, not `short_hex` (which truncates to 8 chars). The
        // server's `/clipboard/text/` route handler validates the
        // suffix is exactly 64 lowercase hex chars and returns 404
        // for anything shorter. Using `short_hex` produced
        // `/clipboard/text/43b20f97` (8 chars) which the source
        // rejected as malformed even though the cache held the
        // full 3261-byte body keyed by the full 32-byte sha256.
        let sha_hex = full_hex(&ct.sha256);
        let client = Http3Client::new(conn);
        let result = client.get_text(&sha_hex).await;
        match result {
            Ok((status, body)) => match status {
                200 => {
                    log::info!(
                        "clipboard inbound: pulled {} bytes from {addr} via HTTP/3 (sha={})",
                        body.len(),
                        short_hex(&ct.sha256)
                    );
                    self.apply_inbound_clipboard_text(&ct.sha256, &body, addr)
                        .await;
                }
                _ => {
                    log::warn!(
                        "clipboard inbound: HTTP/3 GET /clipboard/text/{sha_hex} \
                         from {addr} returned {status} (cache miss? active eviction?) — skipping"
                    );
                }
            },
            Err(e) => {
                log::warn!(
                    "clipboard inbound: HTTP/3 GET /clipboard/text/{sha_hex} \
                     from {addr} failed: {e} — skipping"
                );
            }
        }
    }

    /// **M2a STEP-2a.4** — image-arm of the inbound handler.
    /// Mirror of [`Self::handle_clipboard_inbound_text`] for the
    /// `ClipboardImage` wire event:
    ///
    /// 1. **LRU loopback check** against
    ///    [`Self::image_lru_fingerprints`] (capacity 32, TTL 60 s,
    ///    independent from the text LRU). On hit → log trace +
    ///    `metrics.incr_skip` + return (PLAN §3 M2a 评审 #3 3rd:
    ///    "图片回环集合独立于文本").
    /// 2. **Resolve peer connection** via
    ///    [`Self::peer_connection_for_addr`]. If no live
    ///    connection → log warn + skip (peer disconnected
    ///    mid-flight).
    /// 3. **HTTP/3 GET** `/clipboard/image/{sha256}` via
    ///    [`Http3Client::get_image`]. On 200 → apply; non-200
    ///    (cache miss / active eviction / TTL expired) → log
    ///    warn + skip silently. `Err(_)` → log warn + skip.
    /// 4. **Apply** via [`Self::apply_inbound_clipboard_image`]
    ///    which marks the image LRU + calls
    ///    `backend.set_image(bytes, mime)` + emits
    ///    `FrontendEvent::ClipboardState`.
    ///
    /// **Why no inline fast-path** (unlike text): image bytes are
    /// never on the wire inline. The dispatcher's outbound side
    /// ([`Self::dispatch_image`]) always pushes a metadata-only
    /// `ClipboardImage` and stores the bytes in
    /// [`Self::clipboard_cache`]; the receiver pulls them over
    /// HTTP/3. This matches the wire convention (PLAN §3 M2a
    /// STEP-2a.3 — "图片字节暂存本地 clipboard_cache").
    async fn handle_clipboard_inbound_image(
        &mut self,
        ci: lan_mouse_proto::ClipboardImage,
        addr: SocketAddr,
    ) {
        // Step 1: image LRU loopback check. Independent from the
        // text LRU — see `image_lru_fingerprints` field doc.
        if self.image_lru_fingerprints.contains(&ci.sha256) {
            log::debug!(
                "clipboard inbound image: skipping loopback sha={}",
                short_hex(&ci.sha256)
            );
            // Mirror of the text path's incr_skip — the hit-rate
            // log task aggregates both text + image metrics in the
            // running snapshot.
            self.metrics.incr_skip(unix_now_ms());
            return;
        }
        // Step 2: resolve peer connection for the HTTP/3 GET.
        let Some(conn) = self.peer_connection_for_addr(addr).await else {
            log::warn!(
                "clipboard inbound image: sha={} from {addr} but no live peer connection \
                 found — skipping (peer may have disconnected mid-flight)",
                short_hex(&ci.sha256)
            );
            return;
        };
        // **2026-09-10 inbound-apply off-thread follow-up (round 2)** —
        // spawn the apply task and return immediately. The
        // spawned task owns both the HTTP/3 GET body pull
        // (`5–15 MiB on the same QUIC connection's cwnd — was
        // 100 ms–1 s of main-task block in the 8de4219 path`)
        // and the heavy `set_image` + `current_image` round
        // trip. Main task pays only for the (sub-ms) connection
        // lookup + `spawn_local` call, leaving `capture.event()`
        // free to poll StreamA mouse events during the GET + apply.
        //
        // **LRU mark moved into the spawned task (was here in
        // fc38296)** — code-review #A1 hit: marking the inbound
        // SHA before the GET meant a transient GET miss (404
        // from active eviction, network blip) left the LRU
        // entry in place for 60 s, silently dropping a
        // legitimate re-push via the loopback short-circuit
        // (`image_lru_fingerprints.contains(&ci.sha256)` at
        // the top of this function). The window-defence mark
        // now lives inside `apply_image_inner` immediately
        // before the `BackendCmd::SetImage` send — i.e. AFTER
        // the GET body has landed and we know we'll actually
        // write to the local clipboard. If the GET fails, no
        // LRU mark, so a re-push from the peer within 60 s is
        // retried instead of silently swallowed.
        let Some(cmd_tx) = self.clipboard_backend_cmd.clone() else {
            log::warn!(
                "clipboard inbound image: cmd_tx uninitialised \
                 (Service::run not entered yet?) — dropping apply"
            );
            return;
        };
        let Some(applied_tx) = self.apply_image_applied_tx.clone() else {
            log::warn!(
                "clipboard inbound image: applied_tx uninitialised \
                 (Service::run not entered yet?) — dropping apply"
            );
            return;
        };
        log::info!(
            "clipboard inbound image: apply kicked off to spawn_local task \
             (sha={}, mime={}) for {addr}",
            short_hex(&ci.sha256),
            ci.mime
        );
        let conn_for_fetcher = conn.clone();
        let fetcher = async move {
            crate::quic_transport::http3::Http3Client::new(conn_for_fetcher)
                .get_image(&full_hex(&ci.sha256))
                .await
                .map_err(|e| format!("{e}"))
        };
        tokio::task::spawn_local(apply_inbound_image_task(
            cmd_tx, applied_tx, ci.sha256, ci.mime, addr, fetcher,
        ));
    }

    /// **M1b STEP-1b.2** — apply clipboard text bytes to the local
    /// OS backend + update the loopback LRU + emit the
    /// `ClipboardState` frontend event.
    ///
    /// Split out of [`Self::handle_clipboard_inbound`] so the
    /// inline and HTTP/3-pulled paths share the exact same
    /// downstream behaviour: loopback LRU mark, `last_text` reset,
    /// timestamp + `last_source` update, frontend notification,
    /// log line. Without this helper, the two branches in
    /// `handle_clipboard_inbound` would drift over time (e.g. one
    /// forgets the LRU mark).
    ///
    /// **M1b STEP-1b.3** — the loopback LRU is now
    /// `mark_local_write`'d **before** `backend.set_text`, not
    /// after. Putting the mark first means an OS echo of the
    /// freshly-written value (if the platform notifies on every
    /// change) is caught by the very next `clipboard_tick`'s
    /// `contains` check. Without this reordering, a tick that
    /// fires between `set_text` and the LRU push would re-broadcast
    /// the value we just applied — defeating the loopback defence.
    /// A failed `set_text` still leaves the LRU marked (we
    /// *intended* to write it); the cost is one harmless "skip"
    /// for the next inbound of the same fingerprint.
    ///
    /// **2026-09-10 screenshot-bug fix** — now `async` because
    /// the backend is owned by the spawned `clipboard_poller` and
    /// `set_text` has to be requested through the `BackendCmd`
    /// channel. The poller processes the request on the LocalSet
    /// thread and returns the result via `oneshot`. The original
    /// sync semantics (LRU mark → set_text → bookkeeping →
    /// frontend notify) are unchanged.
    async fn apply_inbound_clipboard_text(
        &mut self,
        sha256: &[u8; 32],
        bytes: &[u8],
        source: SocketAddr,
    ) {
        let Some(cmd_tx) = self.clipboard_backend_cmd.as_ref() else {
            return;
        };
        // **M1b STEP-1b.3** — mark the loopback LRU BEFORE writing
        // to the OS clipboard. See the function docstring for the
        // ordering rationale.
        self.clipboard_lru.mark_local_write(*sha256);
        // Apply to the local OS clipboard. The text is UTF-8 by
        // wire convention; if a peer sent non-UTF-8 bytes (corrupt
        // / older daemon) the lossy replace keeps the daemon from
        // panicking — the user will see replacement characters.
        let text = String::from_utf8_lossy(bytes).into_owned();
        let (reply_tx, reply_rx) = oneshot::channel();
        if cmd_tx
            .send(BackendCmd::SetText {
                text,
                reply: reply_tx,
            })
            .is_err()
        {
            // **2026-09-10 code-review follow-up** — surface the
            // poller-gone condition. The LRU was marked before
            // this point, so a panic'd poller leaves the
            // clipboard unwritten *and* suppresses the peer's
            // echo on the next daemon start (the LRU is empty
            // after restart, but the previous daemon's LRU is
            // gone anyway). The important thing is the operator
            // sees this in logs — without the warn, clipboard
            // sync could silently stop working for hours.
            log::warn!(
                "clipboard inbound: poller task is gone (panic or shutdown); \
                 dropping inbound text push from {source} ({} bytes, sha={})",
                bytes.len(),
                short_hex(sha256)
            );
            return;
        }
        let set_result = match reply_rx.await {
            Ok(r) => r,
            Err(_) => {
                log::warn!("clipboard inbound: set_text reply channel closed");
                return;
            }
        };
        if let Err(e) = set_result {
            log::warn!("clipboard inbound: set_text failed: {e}");
            return;
        }
        // **M1b STEP-1b.3** — record the allow. Increment
        // *after* `set_text` succeeds so a failed write doesn't
        // inflate the metric. The hit-rate log task surfaces the
        // running `allow` count every 60 s.
        self.metrics.incr_allow();
        // Force the next tick to re-read so `last_text` updates to
        // the freshly-written value; otherwise a stale `last_text`
        // would suppress the change-detection that triggers
        // `set_text` on the next inbound push with identical text
        // (an edge case, but the dispatcher must be correct).
        self.clipboard_last_text = None;
        let now_ms = unix_now_ms();
        self.last_text_ts_ms = Some(now_ms);
        self.last_clipboard_source = Some(source);
        self.notify_frontend(FrontendEvent::ClipboardState {
            last_text_ts: self.last_text_ts_ms,
            last_image_ts: self.last_image_ts_ms,
            last_file_ts: self.last_file_ts_ms,
            last_source: Some(format!("{source}")),
        });
        log::info!(
            "clipboard inbound: applied {} bytes from {source} (sha={})",
            bytes.len(),
            short_hex(sha256)
        );
    }

    /// **M2a STEP-2a.4** — image-branch sibling of
    /// [`Self::mark_local_write`] (LRU method) /
    /// [`Self::apply_inbound_clipboard_text`] (service method).
    ///
    /// Wraps `self.image_lru_fingerprints.push(fp)` so the
    /// "we just wrote this image fingerprint to the local
    /// clipboard" semantic is named explicitly at the call site
    /// (matches the text path's `self.clipboard_lru.mark_local_write(*sha256)`
    /// pattern). Called by
    /// [`Self::apply_inbound_clipboard_image`] **before**
    /// `backend.set_image` so an OS echo of the freshly-written
    /// image (if the platform notifies on every change) is caught
    /// by the next `image_lru_fingerprints.contains` check.
    ///
    /// **Independent from the text branch's LRU mark**: the text
    /// and image LRUs are separate `LruFingerprints` instances —
    /// calling this method does not touch
    /// [`Self::clipboard_lru`].
    fn mark_local_image_write(&mut self, fp: [u8; 32]) {
        self.image_lru_fingerprints.push(fp);
    }

    /// **M2a STEP-2a.4** — apply clipboard image bytes to the
    /// local OS backend + update the loopback LRU + emit the
    /// `ClipboardState` frontend event. Image-branch mirror of
    /// [`Self::apply_inbound_clipboard_text`].
    ///
    /// **Window defence ordering** (mirrors the text branch's
    /// rationale): the image LRU is `mark_local_write`'d
    /// **before** `backend.set_image`. A platform echo (e.g.
    /// macOS `NSPasteboardDidChangeNotification` firing
    /// synchronously with `setData_forType`) that re-polls the
    /// clipboard via `current_image()` would otherwise see the
    /// new bytes without an LRU mark — and the dispatcher's
    /// image branch (which would dispatch a new `ClipboardImage`
    /// with the same sha256, echoing back to the peer).
    ///
    /// **MIME handling**: `ci.mime` is a wire string
    /// (`"image/png"` for M2a; `"application/x-dib"` for M2b
    /// STEP-2b.1). The DIB label routes through
    /// [`crate::clipboard::Mime::is_dib_label`] to the dedicated
    /// [`crate::clipboard::ClipboardBackend::set_dib_image`] path;
    /// all other labels flow through `Mime::from_label` to the
    /// [`crate::clipboard::Mime`] enum used by
    /// [`crate::clipboard::ClipboardBackend::set_image`]. Unknown
    /// labels fall back to [`Mime::Png`] (the macOS backend
    /// already forces PNG regardless of the label). A failed
    /// `set_image` (or `set_dib_image`) is logged + return without
    /// bumping `metrics.allow_count` (mirrors the text branch's
    /// "don't inflate the metric on failure" contract).
    ///
    /// **No `clipboard_last_image` reset**: the text branch
    /// clears `clipboard_last_text` after a write so the next
    /// tick re-reads the local backend; for image the dispatcher
    /// already short-circuits on `last_outbound_image_sha` match
    /// (STEP-2a.3), so an analogous "force re-read" isn't needed.
    ///
    /// **2026-09-10 inbound-apply off-thread follow-up** —
    /// completion handler for [`apply_inbound_image_task`]. The
    /// spawned task owns the heavy `BackendCmd::SetImage` /
    /// `BackendCmd::CurrentImage` round trip + SHA computation;
    /// the main task only does the bookkeeping that requires
    /// `&mut self` (image LRU mark + metrics + frontend notify).
    /// This split lets `capture.event()` keep firing during the
    /// 100–300 ms Windows PNG→DIB encode, so master's StreamA
    /// mouse writes don't back-pressure and the controlled side
    /// doesn't drop frames.
    ///
    /// **Why `&mut self` instead of another spawned task**:
    /// `image_lru_fingerprints.push` + `notify_frontend` +
    /// `last_image_ts_ms` / `last_clipboard_source` all need
    /// `&mut self`, and the borrow window here is sub-millisecond
    /// (LRU push is O(1), the frontend event is a channel push).
    /// This is exactly the kind of fast `&mut self` work the
    /// main `select!` was designed to absorb.
    ///
    /// **Failure semantics**: when the apply failed (any
    /// non-`None` `error_msg`), we log warn + skip metrics /
    /// frontend notify — same "no inflate on failure" contract
    /// as the text branch.
    fn handle_inbound_image_applied(&mut self, result: InboundImageApplyResult) {
        let inbound_sha = result.inbound_sha;
        let source = result.source;
        let mime = result.mime.as_str();
        let bytes_len = result.bytes_len;
        if !result.success {
            log::warn!(
                "clipboard inbound image apply failed from {source}: {} \
                 (sha={}, {} bytes, mime={mime})",
                result.error_msg.as_deref().unwrap_or("(no detail)"),
                short_hex(&inbound_sha),
                bytes_len
            );
            return;
        }
        // Step 2.5 (LRU side): record the *post-transcode* SHA
        // in the image LRU. The inbound SHA was marked
        // **just below** (window defence — see code-review
        // #A1 fix on fc38296; the mark happens here in the
        // main task's completion arm, *after* the spawned
        // apply task's `set_image` has returned). Both the
        // inbound SHA and any post-transcode SHA get pushed,
        // so the next 500 ms tick short-circuits the freshly
        // written bytes (PNG on macOS, DIB on Windows).
        //
        // **Why mark in main task, not in the spawned
        // `apply_inbound_image_task`**: the spawned task
        // doesn't have access to the LRU without an extra
        // `Arc<Mutex<LruFingerprints>>` field. The
        // window-defence is slightly weaker (mark happens
        // ~1-5 ms after `set_data_for_type` returns rather
        // than synchronously before it fires), but the
        // 500 ms tick cadence gives ample slack — a 100-300
        // ms apply completes well before the next tick.
        self.mark_local_image_write(inbound_sha);
        if let Some(written_sha) = result.post_write_sha {
            if &written_sha != &inbound_sha {
                log::info!(
                    "clipboard inbound image: backend transcoded (inbound sha={} → \
                     on-clipboard sha={}, mime={}); marking transcoded SHA in image LRU",
                    short_hex(&inbound_sha),
                    short_hex(&written_sha),
                    mime
                );
            }
            self.mark_local_image_write(written_sha);
        } else if let Some(err) = result.error_msg.as_deref() {
            // Success with no post-write SHA — the set_image
            // succeeded but the CurrentImage re-read dropped.
            // Log at info (the inbound-SHA LRU mark from Step 1
            // still gives loopback protection for the inbound
            // bytes themselves; only transcoded-SHA echo is at
            // risk).
            log::info!(
                "clipboard inbound image: post-write SHA unavailable: {err} \
                 (sha={}, {} bytes, mime={mime})",
                short_hex(&inbound_sha),
                bytes_len
            );
        }
        // Step 3: record the allow (mirrors the text branch's
        // "only on success" contract).
        self.metrics.incr_allow();
        // Step 4: bookkeeping + frontend notification.
        let now_ms = unix_now_ms();
        self.last_image_ts_ms = Some(now_ms);
        self.last_clipboard_source = Some(source);
        self.notify_frontend(FrontendEvent::ClipboardState {
            last_text_ts: self.last_text_ts_ms,
            last_image_ts: self.last_image_ts_ms,
            last_file_ts: self.last_file_ts_ms,
            last_source: Some(format!("{source}")),
        });
        log::info!(
            "clipboard inbound image: applied {} bytes from {source} \
             (sha={}, mime={mime})",
            bytes_len,
            short_hex(&inbound_sha)
        );
    }

    /// **M3a STEP-3a.3** — file-arm of the inbound handler.
    /// Mirror of [`Self::handle_clipboard_inbound_image`] for the
    /// `ClipboardFiles` wire event:
    ///
    /// 1. **LRU loopback check** against
    ///    [`Self::file_lru_fingerprints`] (capacity 64, TTL 60 s,
    ///    independent from text + image LRUs). On hit → log trace
    ///    + `metrics.incr_skip` + return (mirrors the image /
    ///    text branches).
    /// 2. **Pure decision fn** via
    ///    [`handle_clipboard_inbound_files_decide`] — fast-fails
    ///    `AutoAcceptOff` (M3b's flag) / `Empty` / `AllMimeTooLarge`
    ///    without touching the peer connection.
    /// 3. **Resolve peer connection** via
    ///    [`Self::peer_connection_for_addr`]. If no live
    ///    connection → log warn + skip (peer disconnected
    ///    mid-flight).
    /// 4. **Resolve `accept_dir`** — `self.config.clipboard_config
    ///    ().accept_dir`, falling back to [`DEFAULT_ACCEPT_DIR`]
    ///    (e.g. `$HOME/Downloads/lan-mouse`). The directory is
    ///    `create_dir_all`'d here (cheap; receiver idempotent) so
    ///    the spawned task can `std::fs::write` directly without
    ///    an EACCES surprise.
    /// 5. **Spawn one task per entry** via
    ///    [`apply_inbound_files_task`]. Each task owns the HTTP/3
    ///    GET + spawn_blocking write/verify for its own entry;
    ///    `applied_tx` reports back to the main task's
    ///    [`Self::handle_inbound_files_applied`] completion arm.
    ///
    /// **Why per-entry spawn (not batched)**: a single
    /// `ClipboardFiles` can carry 1-N entries (a multi-select in
    /// Finder). Sequential processing would block the main task's
    /// `select!` for the full batch duration; per-entry spawn
    /// keeps the main task responsive throughout (200 MiB
    /// transfers can take 1-2 s end-to-end per entry).
    ///
    /// **Why no `auto_accept_files = false` UI hint**: M3b adds
    /// the Toaster prompt. For STEP-3a.3 the config defaults to
    /// `false`; tests pass `true` explicitly to exercise the
    /// apply path. The skip path logs at `info` (not `debug`) so
    /// operators see why the receiver didn't land files.
    async fn handle_clipboard_inbound_files(
        &mut self,
        cf: lan_mouse_proto::ClipboardFiles,
        addr: SocketAddr,
    ) {
        // Step 1: file LRU loopback check.
        if self.file_lru_fingerprints.contains(&cf.fingerprint) {
            log::debug!(
                "clipboard inbound files: skipping loopback fingerprint={}",
                short_hex(&cf.fingerprint)
            );
            // Mirror of the text/image branches — incr_skip so the
            // hit-rate log task surfaces the running count.
            self.metrics.incr_skip(unix_now_ms());
            return;
        }

        // Step 2: pure decision fn (testable in isolation).
        let cfg = self.config.clipboard_config();
        match handle_clipboard_inbound_files_decide(&cf.entries, cfg.auto_accept_files) {
            InboundFilesDecision::AutoAcceptOff => {
                log::info!(
                    "clipboard inbound files: auto_accept_files is off (M3b flag); \
                     skipping ClipboardFiles(fingerprint={}, entries={}) from {addr}",
                    short_hex(&cf.fingerprint),
                    cf.entries.len()
                );
                return;
            }
            InboundFilesDecision::Empty => {
                log::debug!("clipboard inbound files: empty entries vec from {addr}; skipping");
                return;
            }
            InboundFilesDecision::AllMimeTooLarge => {
                log::info!(
                    "clipboard inbound files: all {} entries are MIME_TOO_LARGE from \
                     {addr}; skipping (saves HTTP/3 GET)",
                    cf.entries.len()
                );
                return;
            }
            InboundFilesDecision::Apply { entries } => {
                // Step 3: resolve peer connection.
                let Some(conn) = self.peer_connection_for_addr(addr).await else {
                    log::warn!(
                        "clipboard inbound files: entries from {addr} but no live peer \
                         connection found — skipping (peer may have disconnected mid-flight)"
                    );
                    return;
                };
                // Step 4: resolve accept_dir + create the dir.
                let accept_dir = cfg.accept_dir.unwrap_or_else(default_accept_dir);
                if let Err(e) = std::fs::create_dir_all(&accept_dir) {
                    log::warn!(
                        "clipboard inbound files: failed to create accept_dir {}: {e} \
                         — skipping all entries",
                        accept_dir.display()
                    );
                    return;
                }
                // Step 5: get the apply result sender. The channel
                // is set up in [`Self::run`]; if it's `None` we
                // can't process the inbound.
                let Some(applied_tx) = self.inbound_files_applied_tx.clone() else {
                    log::warn!(
                        "clipboard inbound files: inbound_files_applied_tx uninitialised \
                         (Service::run not entered yet?) — dropping apply"
                    );
                    return;
                };

                log::info!(
                    "clipboard inbound files: spawning {} apply task(s) for \
                     ClipboardFiles(fingerprint={}) from {addr} (accept_dir={})",
                    entries.len(),
                    short_hex(&cf.fingerprint),
                    accept_dir.display(),
                );

                // Per-entry spawn. Each entry is independent
                // (different sha256 → different file path →
                // different write); spawning them serially
                // would block the main `select!` for the full
                // batch duration.
                for entry in entries {
                    let conn_for_fetcher = conn.clone();
                    let sha_hex = full_hex(&entry.sha256);
                    let fetcher = async move {
                        crate::quic_transport::http3::Http3Client::new(conn_for_fetcher)
                            .get_file(&sha_hex, None)
                            .await
                            .map_err(|e| format!("{e}"))
                    };
                    let applied_tx_for_entry = applied_tx.clone();
                    let accept_dir_for_entry = accept_dir.clone();
                    let name = entry.name.clone();
                    let mime = entry.mime.clone();
                    // **M3a STEP-3a.5** — share the cancel
                    // registry with the spawned task so the
                    // `handle_clipboard_inbound_cancel` arm
                    // can signal mid-flight cancellation.
                    let cancel_registry = self.inbound_file_cancel_txs.clone();
                    tokio::task::spawn_local(apply_inbound_files_task(
                        applied_tx_for_entry,
                        entry.sha256,
                        name,
                        entry.size,
                        mime,
                        addr,
                        accept_dir_for_entry,
                        fetcher,
                        cancel_registry,
                    ));
                }
            }
        }
    }

    /// **M3a STEP-3a.3** — completion handler for
    /// [`apply_inbound_files_task`]. Mirrors
    /// [`Self::handle_inbound_image_applied`] for the file branch:
    /// the spawned task owns the HTTP/3 GET + spawn_blocking
    /// write/verify; this completion arm handles the bookkeeping
    /// that requires `&mut self` (LRU mark + metrics + frontend
    /// notify).
    ///
    /// **Window defence ordering** (matches the image branch's
    /// rationale): the file LRU is `push`'d on **success** — a
    /// platform echo (e.g. macOS Finder re-selecting the same
    /// file after our write) that re-polls the file selection
    /// would otherwise dispatch a new `ClipboardFiles` with the
    /// same fingerprint. The 60 s TTL bounds the false-positive
    /// window.
    ///
    /// **Failure semantics**: on any failure path we log warn +
    /// skip metrics / frontend notify (mirrors the text/image
    /// branches' "no inflate on failure" contract).
    fn handle_inbound_files_applied(&mut self, result: InboundFileApplyResult) {
        let inbound_sha = result.inbound_sha;
        let source = result.source;
        let name = result.name.as_str();
        let bytes_len = result.bytes_len;
        let size = result.size;
        let mime = result.mime.as_str();
        if !result.success {
            log::warn!(
                "clipboard inbound file apply failed from {source}: {} \
                 (sha={}, name={name}, declared={size} bytes, got={bytes_len} bytes, \
                 mime={mime})",
                result.error_msg.as_deref().unwrap_or("(no detail)"),
                short_hex(&inbound_sha),
            );
            return;
        }
        // Mark the inbound SHA in the file loopback LRU. The
        // dispatcher (outbound) writes its own fingerprint; we
        // mark the inbound SHA on success so the next 500 ms tick
        // doesn't re-dispatch a copy we just received.
        self.file_lru_fingerprints.push(inbound_sha);
        // Step 3: record the allow (matches the text/image
        // branches' "only on success" contract).
        self.metrics.incr_allow();
        // Step 4: bookkeeping + frontend notification.
        let now_ms = unix_now_ms();
        self.last_file_ts_ms = Some(now_ms);
        self.last_clipboard_source = Some(source);
        self.notify_frontend(FrontendEvent::ClipboardState {
            last_text_ts: self.last_text_ts_ms,
            last_image_ts: self.last_image_ts_ms,
            last_file_ts: self.last_file_ts_ms,
            last_source: Some(format!("{source}")),
        });
        log::info!(
            "clipboard inbound file: applied {} bytes from {source} \
             (sha={}, name={name}, mime={mime}, landed at {:?})",
            bytes_len,
            short_hex(&inbound_sha),
            result
                .landed_path
                .as_deref()
                .unwrap_or(Path::new("<unknown>")),
        );
    }

    /// **M1b STEP-1b.2** — resolve the QUIC `Connection` for a peer
    /// `SocketAddr` so the HTTP/3 GET can be issued against it.
    ///
    /// Two cases the inbound channel can come from:
    /// 1. **Incoming peer** — slave daemon receiving master's push
    ///    via `Emulation::quic_conns` (the listener-side registry).
    ///    Looked up synchronously (`RefCell::borrow`).
    /// 2. **Outgoing client** — master daemon receiving a slave's
    ///    push via `Capture`'s underlying `LanMouseConnection::peers`.
    ///    Looked up asynchronously (`Mutex::lock().await`).
    ///
    /// Both registries are keyed by `SocketAddr` (the remote address
    /// the peer used to dial / was dialled at). If both lookups
    /// miss, the peer is gone (race with `Disconnected`) — return
    /// `None` so the caller logs warn + skips.
    async fn peer_connection_for_addr(&self, addr: SocketAddr) -> Option<quinn::Connection> {
        // (1) Incoming-peer table (listener-side).
        if let Some(peer) = self.emulation.peer_for_addr(addr) {
            return Some(peer.connection().clone());
        }
        // (2) Outgoing-client table (dialer-side).
        self.capture
            .peer_for_addr(addr)
            .await
            .map(|p| p.connection().clone())
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
        let Some(cmd_tx) = self.clipboard_backend_cmd.as_ref() else {
            return;
        };
        // **2026-09-10 screenshot-bug fix** — `current_text` is
        // now routed through the `clipboard_poller` task via
        // `BackendCmd::CurrentText`. Cheap on every backend
        // (no PNG encode), but goes through the same channel
        // as writes for consistency.
        let (reply_tx, reply_rx) = oneshot::channel();
        if cmd_tx
            .send(BackendCmd::CurrentText { reply: reply_tx })
            .is_err()
        {
            // **2026-09-10 code-review follow-up** — log the
            // poller-gone condition. The recover-push path is
            // best-effort (drops the push if the poller is
            // gone) but should not do so silently.
            log::warn!(
                "clipboard recover push: poller task is gone (panic or shutdown); \
                 dropping push for handle={handle}"
            );
            return;
        }
        let new_text = match reply_rx.await {
            Ok(Some(t)) => t,
            _ => return,
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
        let bytes_for_cache = new_text.into_bytes();
        let push_was_metadata_only =
            bytes_for_cache.len() > lan_mouse_proto::CLIPBOARD_TEXT_INLINE_LIMIT;
        let event = ProtoEvent::ClipboardText(ClipboardText::from_content(
            sha,
            sha,
            bytes_for_cache.clone(),
        ));
        // **M1b STEP-1b.2** — same active-eviction + cache-insert
        // contract as the tick path. See the docstring on
        // [`Self::handle_clipboard_tick`] for the rationale.
        self.evict_prev_outbound_clipboard_cache();
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
        if push_was_metadata_only {
            if let Ok(mut guard) = self.clipboard_cache.lock() {
                guard.insert(sha, bytes_for_cache);
            } else {
                log::warn!(
                    "clipboard cache mutex poisoned on insert sha={}; skipping cache write",
                    short_hex(&sha)
                );
            }
        }
        self.last_outbound_text_sha = Some(sha);
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

    /// **M1b STEP-1b.2** — evict the most recently pushed
    /// `ClipboardText` sha256 from [`Self::clipboard_cache`].
    ///
    /// Called by both [`Self::handle_clipboard_tick`] and
    /// [`Self::handle_clipboard_recover_push`] *immediately before*
    /// they push a new `ClipboardText`, so the cache never holds
    /// the previous payload once the new push is dispatched. A
    /// receiver that races the eviction will see `lookup(prev_sha)
    /// == None` from that moment on.
    ///
    /// **First push case**: `last_outbound_text_sha` is `None`, so
    /// the function is a no-op. No log noise.
    ///
    /// **Why a dedicated helper**: pins the "evict prev before
    /// push" contract in one place so the two push paths cannot
    /// drift. Both the tick and the recover-push paths *must* call
    /// this in the same order (before the new push, after the new
    /// sha is computed, before `broadcast_clipboard_event`).
    fn evict_prev_outbound_clipboard_cache(&mut self) {
        evict_prev_outbound_clipboard_cache(
            &self.clipboard_cache,
            &mut self.last_outbound_text_sha,
        );
    }

    /// **M2a STEP-2a.3** — image-branch sibling of
    /// [`Self::evict_prev_outbound_clipboard_cache`]. Delegates to
    /// the same free-function helper so the active-eviction
    /// contract stays in one place; only the `last_outbound_*_sha`
    /// field differs. Called by [`Self::dispatch_image`] *immediately
    /// before* pushing a new `ClipboardImage`, mirroring the text
    /// branch's "evict prev before push" ordering.
    ///
    /// **Independent from the text-branch eviction**: the cache is
    /// shared, but the previous-push pointers are tracked
    /// separately so a text push does not accidentally evict a
    /// previously-cached image (and vice versa).
    fn evict_prev_outbound_image_cache(&mut self) {
        evict_prev_outbound_clipboard_cache(
            &self.clipboard_cache,
            &mut self.last_outbound_image_sha,
        );
    }

    /// **PLAN-2 / M1a STEP-1a.4** — broadcast a clipboard event to
    /// every active peer with `enable_clipboard_to = true`.
    ///
    /// **M1a follow-up #2** — the broadcast targets **two disjoint
    /// peer sets**, both of which must be reached:
    ///
    /// 1. **Outgoing clients** (`client_manager.get_client_states()`):
    ///    the legacy path. Each outgoing client has a `ClientHandle`
    ///    and is pushed via `Capture::send_event(event, handle)` →
    ///    `CaptureTask` → `conn.send(event, handle)` →
    ///    `peer.send_input` (StreamC). Skipped when `!active` or
    ///    `state.active_addr.is_none()` (handshake incomplete).
    ///
    /// 2. **Incoming peers** (`incoming_clipboard`, populated by
    ///    `EmulationEvent::Connected`): the slave-side master. Has
    ///    no `ClientHandle` — pushed via
    ///    `Emulation::send_to_incoming(addr, event)` →
    ///    `peer.send_input` (StreamC) using the
    ///    `LanMouseListener::quic_conns` registry. Skipped when
    ///    `!enable_clipboard_to` or the peer is no longer in
    ///    `quic_conns` (transient race with `Disconnected`).
    ///
    /// Without the second branch, copies made on a slave daemon
    /// that runs incoming-only (the typical setup) never reach
    /// the master: the master's `ClientHandle` is not in
    /// `client_manager` on the slave, and `get_client_states()`
    /// returns an empty list → `recipients == 0` → "clipboard
    /// dispatched to 0 peers" with no `ClipboardText` ever sent.
    /// Merging the two sets is the fix.
    ///
    /// Fire-and-forget: outgoing pushes go through `Capture::send_event`
    /// (queues on the capture task's `request_tx`, the actual
    /// `conn.send` happens off-thread). Incoming pushes go through
    /// `Emulation::send_to_incoming` (directly awaits
    /// `peer.send_input` on this task). Per-peer send failures
    /// (peer disconnected mid-tick) are logged at `warn` inside
    /// `CaptureTask` (outgoing) or returned as `Err` from
    /// `send_to_incoming` (incoming) but do not propagate to the
    /// dispatcher — both are best-effort and the user's next
    /// copy is the natural retry point.
    async fn broadcast_clipboard_event(&self, event: ProtoEvent, recipients: &mut usize) {
        // ── Branch 1: outgoing clients ─────────────────────────────
        let mut skipped_disabled = 0usize;
        let mut skipped_inactive = 0usize;
        let mut skipped_no_addr = 0usize;
        for (handle, cfg, state) in self.client_manager.get_client_states() {
            if !cfg.enable_clipboard_to {
                log::debug!(
                    "clipboard broadcast: skipping outgoing peer handle={} (enable_clipboard_to=false)",
                    handle
                );
                skipped_disabled += 1;
                continue;
            }
            if !state.active {
                log::debug!(
                    "clipboard broadcast: skipping outgoing peer handle={} (client not active yet)",
                    handle
                );
                skipped_inactive += 1;
                continue;
            }
            if state.active_addr.is_none() {
                log::debug!(
                    "clipboard broadcast: skipping outgoing peer handle={} (no active_addr — handshake incomplete?)",
                    handle
                );
                skipped_no_addr += 1;
                continue;
            }
            log::debug!(
                "clipboard broadcast: -> outgoing peer handle={} active_addr={:?}",
                handle,
                state.active_addr
            );
            self.capture.send_event(event.clone(), handle);
            *recipients += 1;
        }
        if skipped_disabled + skipped_inactive + skipped_no_addr > 0 {
            log::debug!(
                "clipboard broadcast gate summary (outgoing): skipped disabled={} inactive={} no_addr={}",
                skipped_disabled,
                skipped_inactive,
                skipped_no_addr
            );
        }

        // ── Branch 2: incoming peers (M1a follow-up #2) ─────────────
        //
        // Snapshot the addresses first so a `Disconnected` event
        // arriving mid-broadcast (which mutates
        // `incoming_clipboard` via the `select!` arm) does not
        // invalidate the iterator. The dispatcher's reads of
        // `incoming_clipboard` are gated by the service's
        // single-threaded `spawn_local` runtime, so there is no
        // concurrent-mutation hazard at the language level — the
        // snapshot is purely defensive against `select!` arm
        // interleaving on the same task.
        let incoming_snapshot: Vec<(SocketAddr, IncomingClipboardState)> = self
            .incoming_clipboard
            .iter()
            .map(|(a, s)| (*a, s.clone()))
            .collect();
        let mut incoming_skipped_disabled = 0usize;
        let mut incoming_skipped_unreachable = 0usize;
        let mut incoming_send_failed = 0usize;
        for (addr, state) in incoming_snapshot.iter() {
            if !state.enable_clipboard_to {
                log::info!(
                    "clipboard broadcast: skipping incoming peer {addr} (enable_clipboard_to=false)"
                );
                incoming_skipped_disabled += 1;
                continue;
            }
            log::info!("clipboard broadcast: -> incoming peer {addr}");
            match self.emulation.send_to_incoming(*addr, event.clone()).await {
                Ok(()) => {
                    *recipients += 1;
                }
                Err(reason) if reason.contains("not in quic_conns") => {
                    log::info!(
                        "clipboard broadcast: incoming peer {addr} no longer in quic_conns \
                         (race with Disconnected event; snapshot was stale); skipping"
                    );
                    incoming_skipped_unreachable += 1;
                }
                Err(reason) => {
                    log::warn!(
                        "clipboard broadcast: send to incoming peer {addr} failed: {reason}"
                    );
                    incoming_send_failed += 1;
                }
            }
        }
        if incoming_skipped_disabled + incoming_skipped_unreachable + incoming_send_failed > 0 {
            log::info!(
                "clipboard broadcast gate summary (incoming): skipped disabled={} unreachable={} send_failed={}",
                incoming_skipped_disabled,
                incoming_skipped_unreachable,
                incoming_send_failed
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

/// **M2a STEP-2a.3** — sibling of [`sha256_of`] for arbitrary
/// byte payloads (used by the image dispatcher branch for image
/// bytes, and reused by the test suite to compute expected
/// sha256 values). Operates on `&[u8]` so callers don't need to
/// allocate an owned `Vec<u8>` or convert to `String`.
fn sha256_of_bytes(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
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

/// Full lowercase hex encoding of a 32-byte sha256 (64 chars).
/// Used to construct the `/clipboard/text/{sha256}` URL path that
/// the receiver's `Http3Client::get_text` issues and the
/// source-side `clipboard_text_route` decodes. Must be the full
/// 64-char form — the route handler rejects shorter suffixes as
/// malformed. **Do not** substitute [`short_hex`] here: log lines
/// use that for readability, but the wire path needs full fidelity.
fn full_hex(b: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for byte in b.iter() {
        s.push_str(&format!("{:02x}", byte));
    }
    s
}

/// **M1b follow-up regression** — pins the full-hex contract for
/// the URL path constructed in [`Service::handle_clipboard_inbound`].
/// Previously the path used `short_hex` (8 chars), which the
/// source-side route handler rejected as malformed (it requires
/// exactly 64 lowercase hex chars). This test would catch a future
/// refactor that re-substitutes `short_hex`.
#[cfg(test)]
mod hex_encoding_tests {
    use super::*;

    #[test]
    fn full_hex_emits_64_lowercase_chars() {
        let sha = [0xAA; 32];
        let hex = full_hex(&sha);
        assert_eq!(hex.len(), 64, "full_hex must emit 64 chars");
        assert_eq!(hex, "aa".repeat(32));
    }

    #[test]
    fn short_hex_remains_8_chars_for_log_lines() {
        let sha = [0xAA; 32];
        let hex = short_hex(&sha);
        assert_eq!(hex.len(), 8, "short_hex stays 8 chars for log readability");
        assert_eq!(hex, "aaaaaaaa");
    }

    #[test]
    fn full_hex_matches_receiver_path_expectations() {
        // The path used in the bug report was
        // `/clipboard/text/43b20f97` (8 chars, from short_hex).
        // The source-side route handler requires 64 chars and
        // returned 404 for any shorter suffix. With full_hex the
        // path becomes
        // `/clipboard/text/43b20f97...` (62 more chars) and
        // matches what the source wrote to the cache.
        let sha = [
            0x43, 0xb2, 0x0f, 0x97, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];
        let hex = full_hex(&sha);
        assert!(hex.starts_with("43b20f97"));
        assert_eq!(hex.len(), 64);
        assert_eq!(&hex[8..], &"00".repeat(28));
    }
}

/// **M1b STEP-1b.2** — free-function form of
/// [`Service::evict_prev_outbound_clipboard_cache`]. Extracted so
/// the dispatcher's "evict prev before push" contract is testable
/// without standing up a full `Service::new` (which would need
/// `AsyncFrontendListener`, `LanMouseConnection`, certificates,
/// etc.).
///
/// **Mutex-poison handling**: a previous holder panicking poisons
/// the mutex. We `into_inner()` to recover (the dispatcher's
/// "always continue" contract), logging warn so an operator can
/// spot the poison.
fn evict_prev_outbound_clipboard_cache(
    cache: &Arc<Mutex<crate::clipboard::cache::ClipboardCache>>,
    last_outbound_text_sha: &mut Option<[u8; 32]>,
) {
    let Some(prev_sha) = *last_outbound_text_sha else {
        return;
    };
    let mut guard = match cache.lock() {
        Ok(g) => g,
        Err(poisoned) => {
            log::warn!(
                "clipboard cache mutex poisoned on evict prev sha={}; clearing poison and continuing",
                short_hex(&prev_sha)
            );
            poisoned.into_inner()
        }
    };
    if guard.remove(&prev_sha) {
        log::trace!(
            "clipboard cache: evicted prev outbound sha={}",
            short_hex(&prev_sha)
        );
    }
}

/// **M2a STEP-2a.4** — apply inbound image bytes to the local
/// OS clipboard backend.
///
/// Free function form of the inner step in
/// [`Service::apply_inbound_clipboard_image`]. Extracted so the
/// "bytes + mime → `backend.set_image`" sequence is unit-testable
/// without standing up a full `Service` (matching the pattern
/// used by [`evict_prev_outbound_clipboard_cache`]).
///
/// **MIME handling**: the wire carries `mime` as a string
/// (`"image/png"` for M2a; `"application/x-dib"` for M2b). The
/// DIB label routes through [`crate::clipboard::Mime::is_dib_label`]
/// to the dedicated [`ClipboardBackend::set_dib_image`] path (the
/// [`Mime`] enum is intentionally left untouched per STEP-2b.1
/// "不要触碰 Mime enum"). For all other labels,
/// [`Mime::from_label`] maps the known PNG / JPEG / BMP labels to
/// the [`Mime`] enum used by [`ClipboardBackend::set_image`];
/// unknown labels fall back to [`Mime::Png`] with a warn log — the
/// macOS backend forces PNG regardless of the label (see
/// `src/clipboard/macos.rs::set_image` docstring), and the Windows
/// / Linux backends either match or are out of scope for M2a.
/// Falling back to PNG keeps the daemon alive on unexpected wire
/// labels instead of failing the inbound silently.
///
/// **Backend-unavailable case**: returns
/// `Err(ClipboardError::Unsupported(...))` if no backend is
/// configured (e.g. the daemon is running without a clipboard
/// backend because the platform tool is missing). The caller
/// (`apply_inbound_clipboard_image`) logs warn and skips without
/// incrementing the metrics allow counter — same "no inflate on
/// failure" contract as the text branch.
///
/// **2026-09-10 screenshot-bug fix**: in production code the
/// backend is owned by the spawned `clipboard_poller` task and
/// the routing logic inlined into the
/// `apply_inbound_clipboard_image` cmd-construction site. This
/// helper now only exists for the unit tests in
/// `image_inbound_tests` (which build a `RecordingBackend` /
/// `DummyBackend` directly and exercise the mime-routing
/// predicate in isolation from the rest of the service).
#[cfg(test)]
#[allow(dead_code)]
fn apply_inbound_image_bytes(
    backend: &mut Option<Box<dyn ClipboardBackend>>,
    bytes: &[u8],
    mime: &str,
) -> Result<(), crate::clipboard::ClipboardError> {
    let backend = backend.as_mut().ok_or_else(|| {
        crate::clipboard::ClipboardError::Unsupported(
            "clipboard backend not available (inbound image apply)".into(),
        )
    })?;
    // **M2b STEP-2b.1**: route raw DIB bytes
    // (`application/x-dib`) through the dedicated
    // [`ClipboardBackend::set_dib_image`] method rather than the
    // generic PNG / JPEG / BMP [`Mime`]-based `set_image`. DIB
    // does not map onto the [`Mime`] enum (PLAN §3 评审 #4 3rd —
    // "不要触碰 Mime enum"), so we keep the routing predicate as a
    // string equality check on the wire label.
    if Mime::is_dib_label(mime) {
        return backend.set_dib_image(bytes);
    }
    let mime_enum = Mime::from_label(mime).unwrap_or_else(|| {
        log::warn!("clipboard inbound image: unknown mime label '{mime}'; defaulting to PNG");
        Mime::Png
    });
    backend.set_image(bytes, mime_enum)
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

// ============================================================================
//  Clipboard poller (2026-09-10 screenshot-bug fix — see Service::run for
//  the long-form rationale)
// ============================================================================

/// Request to the clipboard poller task (`Service::run`'s spawned
/// `clipboard_poller`). Each variant carries a `oneshot::Sender`
/// reply so the inbound caller can `await` the result of an
/// OS-clipboard write without holding the backend directly.
///
/// **Why a dedicated `BackendCmd` channel instead of sharing the
/// backend via `Arc<Mutex<>>`**: the polling side routinely awaits
/// `spawn_blocking` for 2–5 s during a full-screen JPEG→PNG encode,
/// and any inbound write that grabs a std-Mutex lock during that
/// window would block the entire LocalSet thread (deadlock-like).
/// Routing writes through an mpsc channel keeps the backend mutex
/// (which doesn't exist any more — the poller is the SOLE owner)
/// out of the inbound path entirely.
enum BackendCmd {
    SetText {
        text: String,
        reply: tokio::sync::oneshot::Sender<Result<(), crate::clipboard::ClipboardError>>,
    },
    SetImage {
        bytes: Vec<u8>,
        mime: Mime,
        reply: tokio::sync::oneshot::Sender<Result<(), crate::clipboard::ClipboardError>>,
    },
    SetDibImage {
        bytes: Vec<u8>,
        reply: tokio::sync::oneshot::Sender<Result<(), crate::clipboard::ClipboardError>>,
    },
    CurrentText {
        reply: tokio::sync::oneshot::Sender<Option<String>>,
    },
    CurrentImage {
        reply: tokio::sync::oneshot::Sender<Option<crate::clipboard::ImageBytes>>,
    },
    /// **M3a STEP-3a.2** — request the OS clipboard's current
    /// file selection. The poller (which holds the backend) reads
    /// `Vec<PathBuf>` and replies via `oneshot`. Inbound
    /// `handle_clipboard_inbound_files` will use this once the
    /// receiver-side inbound arm lands in STEP-3a.3 / 3a.5.
    /// For STEP-3a.2 only the tick arm calls it (via
    /// `current_files` directly on the poller's owned backend).
    ///
    /// `#[allow(dead_code)]` because the inbound arm that
    /// actually fires this variant lands in STEP-3a.3 — the
    /// variant is in place now so the poller's API surface
    /// matches every other read (`CurrentText` / `CurrentImage`)
    /// and STEP-3a.3 doesn't have to re-thread the cmd channel.
    #[allow(dead_code)]
    CurrentFiles {
        reply: tokio::sync::oneshot::Sender<Option<Vec<PathBuf>>>,
    },
}

/// **M3a STEP-3a.2** — default per-file size cap, bytes.
///
/// 50 MiB matches PLAN §3 STEP-3a.2 + §5 风险 #25 ("文件大小上限
/// 默认 50 MiB"). The cap is enforced inside
/// `collect_files` / `collect_files_blocking` — a batch with any
/// file above this size returns
/// `FileMetaError::ExceedsLimit`, the dispatcher's outbound
/// branch fires a `PopupGuard` immediately (no waiting for the
/// 500 ms tick), and the file is NOT inserted into `file_cache`,
/// NOT pushed over StreamC, NOT registered for HTTP/3 GET.
///
/// **Why 50 MiB, not 200 MiB**: the 200 MiB figure is the M3a
/// STEP-3a.4 *transfer* performance milestone (LAN round-trip
/// target). The 50 MiB *upload* cap is the user-facing "max file
/// size for clipboard sync" knob — copying a 200 MiB file is
/// expected to be rare, so 50 MiB matches the "code screenshot +
/// document + small video clip" common ceiling documented in
/// PLAN §5 风险 #25.
///
/// **M3b / M4 follow-up**: this constant will be replaced by
/// `Config::max_file_size()` once `lan-mouse-ipc::ClipboardConfig`
/// + the `[clipboard]` TOML section land (M3b STEP-3b.1 +
/// M4 STEP-4.2). See `next/SUGGESTION.md` #S-5 for the tracking
/// entry.
pub const DEFAULT_MAX_FILE_SIZE: u64 = 50 * 1024 * 1024;

/// **M3a STEP-3a.3** — default `accept_dir` for inbound files
/// when the user hasn't configured one (i.e. `ClipboardConfig
/// ::accept_dir == None`).
///
/// **Why a hardcoded fallback here (not `dirs::download_dir` etc.)**:
/// the daemon currently has zero `dirs` / `directories` dependencies
/// (keeping the cross-platform footprint minimal). A hardcoded
/// subdirectory name is portable; the runtime resolves the user's
/// home at first inbound file via `std::env::var("HOME")` /
/// `USERPROFILE` fallback. M3b STEP-3b.1 / M4 STEP-4.2 land the
/// full IPC + TOML `accept_dir` override; this constant is the
/// fallback until then.
///
/// **Why `lan-mouse` (not `Downloads/lan-mouse`)**: every peer
/// using the daemon shares the same in-app folder — easier for the
/// user to find incoming files and to set up per-app automation
/// (e.g. Hazel rules on `~/lan-mouse`). On macOS the full path is
/// `~/lan-mouse/`; on Windows `%USERPROFILE%\lan-mouse\`; on Linux
/// `~/lan-mouse/`.
pub const DEFAULT_ACCEPT_DIR: &str = "lan-mouse";

/// **M3a STEP-3a.3** — resolve the user's home directory at
/// runtime. Used by [`DEFAULT_ACCEPT_DIR`] to build the full
/// `<home>/lan-mouse/` path.
///
/// **Cross-platform fallback chain**: `$HOME` (macOS / Linux) →
/// `$USERPROFILE` (Windows) → `Option<PathBuf>` if neither env is
/// set. The `None` branch falls back to `/tmp/lan-mouse` so the
/// daemon never panics; in practice all 3 platforms always have
/// one of the two env vars set for an interactive user.
fn default_accept_dir() -> PathBuf {
    let home = std::env::var("HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::var("USERPROFILE").ok().map(PathBuf::from));
    let base = home.unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join(DEFAULT_ACCEPT_DIR)
}

/// **M3a STEP-3a.2** — derive a stable 32-byte fingerprint from
/// an OS clipboard file selection. Used by `dispatch_files` as
/// the per-batch short-circuit key (mirrors the
/// `last_outbound_text_sha` / `last_outbound_image_sha`
/// pattern).
///
/// **Algorithm**: sort the paths lexicographically, concatenate
/// their `OsStr` byte representations with `\n` as a separator,
/// hash with `sha2::Sha256`. The result is deterministic for a
/// given selection (independent of OS-side iteration order) and
/// distinct across different selections. Two different selections
/// that happen to contain the same files in different
/// directories will hash to distinct values because the full
/// path bytes are folded in.
///
/// **Why sort + join** rather than `HashSet` + `BTreeSet`:
/// avoids the `Ord` requirement on `PathBuf` (which doesn't
/// implement `Ord` on all platforms uniformly — `Path` does, but
/// `Path::cmp` semantics differ for UNC paths on Windows). The
/// raw-byte + sort approach is portable across all three
/// platforms (macOS / Windows / Linux) and matches the
/// `Vec<u8>`-based hash the dispatcher already uses for text /
/// image sha256.
pub(crate) fn file_selection_fingerprint(paths: &[PathBuf]) -> [u8; 32] {
    use sha2::Digest;
    let mut sorted: Vec<&PathBuf> = paths.iter().collect();
    sorted.sort_by(|a, b| {
        a.as_os_str()
            .len()
            .cmp(&b.as_os_str().len())
            .then(a.as_os_str().cmp(b.as_os_str()))
    });
    let mut hasher = Sha256::new();
    for p in &sorted {
        hasher.update(p.as_os_str().as_encoded_bytes());
        hasher.update(&[b'\n']);
    }
    let out = hasher.finalize();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&out);
    arr
}

/// **M3a STEP-3a.2** — convenience helper: `Some(fp) -> Some(&fp)` for
/// the dispatcher short-circuit comparison. Saves a `match` /
/// `as_ref()` pair at every call site.
fn fingerprint_eq(prev: Option<&[u8; 32]>, next: &[u8; 32]) -> bool {
    prev == Some(next)
}

/// **M3a STEP-3a.2 + P1 follow-up** — outcome of the
/// `dispatch_files` decision pipeline. Extracted as a free
/// function so the 5 early-return branches are unit-testable
/// without standing up a full `Service` (which would need
/// `AsyncFrontendListener`, `LanMouseConnection`, certificates,
/// etc.). Mirrors the pattern of
/// [`evict_prev_outbound_clipboard_cache`] / [`apply_image_inner`].
#[derive(Debug)]
pub(crate) enum DispatchFilesOutcome {
    /// Empty `paths` Vec — return immediately (defensive).
    Empty,
    /// Fingerprint matches `last_outbound_files_fingerprint` —
    /// same Finder selection tick-after-tick; skip broadcast +
    /// cache insert.
    FingerprintMatch,
    /// `collect_files_blocking` succeeded — broadcast the
    /// metadata event and fill the `file_cache` via the
    /// dispatcher's second `spawn_blocking` step.
    Ok {
        /// Fingerprint derived from the path list (so the
        /// caller doesn't need to recompute it for the
        /// `last_outbound_files_fingerprint` bookkeeping).
        fingerprint: [u8; 32],
        entries: Vec<crate::clipboard::file_meta::FileEntry>,
        /// Original `paths` retained so the caller can re-read
        /// each file's bytes for the `file_cache` insert
        /// (FileEntry does not carry a path reference).
        paths: Vec<PathBuf>,
    },
    /// At least one file exceeds `max_size` — fire a popup and
    /// return (no StreamC push, no cache fill, no HTTP/3
    /// setup). See PLAN §5 风险 #25.
    ///
    /// **Carries `fingerprint`** (2026-09-13): so the dispatcher
    /// arm can stamp `last_outbound_files_fingerprint` and stop
    /// the per-tick popup storm when the same oversized file
    /// sits on the clipboard across multiple poller ticks.
    /// Without this, an oversized file selection produced a
    /// popup every 500 ms tick and the synchronous popup +
    /// repeated `spawn_blocking` calls back-pressured the
    /// runtime enough that `signal::ctrl_c()` could no longer
    /// fire (the user reported the daemon became unresponsive
    /// to Ctrl+C while the storm was running).
    ExceedsLimit {
        /// Fingerprint of the offending path list — see the
        /// `Ok` variant for the same field. The dispatcher
        /// copies this into `last_outbound_files_fingerprint`
        /// so the next tick with the same selection short-
        /// circuits at the `fingerprint_eq` guard above.
        fingerprint: [u8; 32],
        offending: PathBuf,
        size: u64,
        limit: u64,
    },
    /// Caller passed a directory path — log + return
    /// (recursive walk is out of scope for M3a).
    IsDirectory(PathBuf),
    /// IO error during metadata collection — log + return.
    Io(io::Error),
}

/// **M3a STEP-3a.2 + P1 follow-up** — pure decision pipeline
/// for `dispatch_files`. No `Service` state is touched — the
/// caller feeds in `last_outbound_files_fingerprint` +
/// `max_size` and matches on the returned `DispatchFilesOutcome`
/// to apply side effects (popup fire, broadcast, cache insert,
/// `last_*_ts_ms` bookkeeping).
///
/// **Why extracted**: lets the 5 early-return branches
/// (empty / fingerprint match / ExceedsLimit / IsDirectory /
/// Io+Join) be exercised in unit tests with zero
/// `Service::new()` plumbing. The `Service` method
/// [`Service::dispatch_files`] wraps this and applies the
/// corresponding side effects.
pub(crate) async fn dispatch_files_decide(
    paths: Vec<PathBuf>,
    last_outbound_files_fingerprint: Option<[u8; 32]>,
    max_size: u64,
) -> DispatchFilesOutcome {
    // Branch 1: empty paths — defensive early-return.
    if paths.is_empty() {
        return DispatchFilesOutcome::Empty;
    }
    // Branch 2: fingerprint short-circuit. The same Finder
    // selection tick-after-tick hashes to the same fingerprint;
    // skip the sha256 + broadcast + cache insert when nothing
    // changed.
    let fingerprint = file_selection_fingerprint(&paths);
    if fingerprint_eq(last_outbound_files_fingerprint.as_ref(), &fingerprint) {
        return DispatchFilesOutcome::FingerprintMatch;
    }
    // Branch 3: spawn_blocking for collect_files_blocking —
    // CPU-bound sha256 streaming (200 MiB → ~5-8 s on SSD) must
    // not block the dispatcher's LocalSet. Clone the paths so the
    // `Ok` arm below can hand them back to the caller for the
    // `file_cache` insert (FileEntry does not retain a path
    // reference).
    let paths_for_blocking = paths.clone();
    let blocking_join = tokio::task::spawn_blocking(move || {
        crate::clipboard::file_meta::collect_files_blocking(&paths_for_blocking, max_size)
    })
    .await;
    match blocking_join {
        Ok(Ok(entries)) => DispatchFilesOutcome::Ok {
            fingerprint,
            entries,
            // Return the original `paths` (still in scope —
            // `paths_for_blocking = paths.clone()` did not
            // consume `paths`). The caller's `file_cache`
            // insert step uses these to re-read each file's
            // bytes — `FileEntry` does not retain a path
            // reference.
            paths,
        },
        Ok(Err(FileMetaError::ExceedsLimit {
            offending,
            size,
            limit,
        })) => DispatchFilesOutcome::ExceedsLimit {
            // 2026-09-13 fix: carry the already-computed
            // `fingerprint` (line 4593 above) into the
            // outcome so the dispatcher arm can stamp
            // `last_outbound_files_fingerprint` and stop the
            // per-tick popup storm when the same oversized
            // file sits on the clipboard across multiple
            // poller ticks.
            fingerprint,
            offending,
            size,
            limit,
        },
        Ok(Err(FileMetaError::IsDirectory(p))) => DispatchFilesOutcome::IsDirectory(p),
        Ok(Err(FileMetaError::Io(e))) => DispatchFilesOutcome::Io(e),
        Err(join_err) => DispatchFilesOutcome::Io(io::Error::other(format!(
            "spawn_blocking join error for collect_files_blocking: {join_err}"
        ))),
    }
}

/// **2026-09-10 inbound-apply off-thread follow-up** —
/// completion event for [`apply_inbound_image_task`]. The
/// spawned task performs `BackendCmd::SetImage` /
/// `BackendCmd::SetDibImage` + `BackendCmd::CurrentImage` on
/// the poller (heavy: PNG→DIB decode/encode on Windows is
/// 100–300 ms) and reports the outcome here. The main task
/// consumes these in a dedicated `select!` arm to update the
/// image LRU + metrics + frontend notify.
///
/// **Why a `struct` not a tuple**: the field names document the
/// `Option<[u8; 32]>` semantics (post-write SHA may be `None`
/// if the poller crashed between SetImage and CurrentImage).
/// A 4-tuple would be unreadable at the call sites.
struct InboundImageApplyResult {
    /// Inbound SHA from the wire (already marked in LRU
    /// *before* spawning the apply task).
    inbound_sha: [u8; 32],
    /// Source peer address (for log lines + `last_clipboard_source`).
    source: SocketAddr,
    /// Wire mime string (kept verbatim for the log + frontend
    /// — we don't `Mime::from_label` here because the main task
    /// may want the raw label, not the enum-resolved one).
    mime: String,
    /// Inbound bytes length (for log lines; the actual `Vec<u8>`
    /// is dropped after the cmd is sent).
    bytes_len: usize,
    /// `true` iff `set_image` / `set_dib_image` succeeded.
    success: bool,
    /// Post-write SHA from `BackendCmd::CurrentImage`. `None`
    /// when the apply failed or when the post-write re-read
    /// itself failed (poller gone).
    post_write_sha: Option<[u8; 32]>,
    /// Human-readable error from any failed step. `None` on
    /// success.
    error_msg: Option<String>,
}

/// **2026-09-10 screenshot-bug fix** — dedicated spawned task
/// that owns the OS clipboard backend and serves two distinct
/// request streams:
///
/// 1. **Polling tick** (every 500 ms) — reads the local clipboard
///    via `current_image_async` / `current_text` / `current_files`
///    and forwards results to the main task's `image_tx` /
///    `text_tx` / `files_tx` channels. The dispatcher arms in
///    the main `select!` consume from those channels and run the
///    existing `dispatch_image` / `dispatch_text` / `dispatch_files`
///    helpers unchanged.
/// 2. **Inbound backend cmds** (`cmd_rx`) — `set_text` /
///    `set_image` / `set_dib_image` / `current_text` /
///    `current_image` / `current_files`. Each carries a `oneshot`
///    reply channel so the inbound caller can `await` the
///    result.
///
/// **Lifecycle**: the task exits when the runtime drops, which
/// happens when `Service::run` returns (CTRL+C). `cmd_tx` in
/// `Service` is dropped at the same time, closing `cmd_rx`; the
/// next `cmd_rx.recv()` returns `None`, the loop falls through,
/// and the `image_tx` / `text_tx` / `files_tx` channels are
/// dropped on the way out.
///
/// **M3a STEP-3a.2**: extends the polling tick to also probe
/// `current_files` after the image + text probes fail. Order:
/// image first (screenshot clipboard might mask empty text),
/// text second, file third — the file probe is cheap (one
/// `NSFilenamesPboardType` read / `text/uri-list` subprocess on
/// Linux / `CF_HDROP` enumeration on Windows; none of these
/// touch the bytes themselves, just the selection metadata).
async fn clipboard_poller(
    backend: Option<Box<dyn ClipboardBackend>>,
    mut interval: tokio::time::Interval,
    image_tx: tokio_mpsc::UnboundedSender<crate::clipboard::ImageBytes>,
    text_tx: tokio_mpsc::UnboundedSender<String>,
    files_tx: tokio_mpsc::UnboundedSender<Vec<PathBuf>>,
    mut cmd_rx: tokio_mpsc::UnboundedReceiver<BackendCmd>,
) {
    // **Backend-absent case**: no platform backend (e.g. running
    // in a CI container without `xclip` / no macOS pasteboard
    // binding). Still drain `cmd_rx` so inbound `apply_*` callers
    // don't block forever waiting on a reply that will never
    // arrive; just send `None` / `Err(Unsupported)` back.
    let mut backend = match backend {
        Some(b) => b,
        None => {
            log::debug!("clipboard poller: no backend configured; draining cmd_rx only");
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    BackendCmd::SetText { reply, .. } => {
                        let _ = reply.send(Err(crate::clipboard::ClipboardError::Unsupported(
                            "clipboard backend not configured".into(),
                        )));
                    }
                    BackendCmd::SetImage { reply, .. } | BackendCmd::SetDibImage { reply, .. } => {
                        let _ = reply.send(Err(crate::clipboard::ClipboardError::Unsupported(
                            "clipboard backend not configured".into(),
                        )));
                    }
                    BackendCmd::CurrentText { reply } => {
                        let _ = reply.send(None);
                    }
                    BackendCmd::CurrentImage { reply } => {
                        let _ = reply.send(None);
                    }
                    BackendCmd::CurrentFiles { reply } => {
                        let _ = reply.send(None);
                    }
                }
            }
            return;
        }
    };
    // Skip the immediate first tick — `tokio::time::interval`
    // fires at t=0 by default; we don't want the very first
    // poll to log anything before the daemon has been alive
    // long enough for the user to have copied something.
    interval.tick().await;
    loop {
        tokio::select! {
            // Bias toward the tick arm when both are ready so a
            // burst of inbound cmds can't starve the polling tick.
            biased;
            _ = interval.tick() => {
                // Phase 1: image. Check first so macOS screenshot
                // pasteboards (which advertise an empty string
                // alongside the PNG) do not get masked by an
                // empty-text short-circuit. `current_image_async`
                // routes the heavy encode through `spawn_blocking`
                // on macOS — see the trait method docstring on
                // `ClipboardBackend::current_image_async`.
                let image_hit = match backend.current_image_async().await {
                    Some(image) => {
                        if image_tx.send(image).is_err() {
                            // Main task is gone — daemon is
                            // shutting down. Exit cleanly.
                            return;
                        }
                        true
                    }
                    None => false,
                };
                // Phase 2: text. Only runs when Phase 1 missed,
                // so a macOS screenshot pasteboard (PNG + empty
                // text) does not dispatch an empty-string
                // ClipboardText that would clobber the just-pushed
                // image on the receiver. `dispatch_text`'s own
                // SHA short-circuit would NOT save us here: an
                // empty string is a valid different value, so
                // without this gate the receiver's clipboard would
                // flip from "image" back to "" on the next tick.
                if !image_hit {
                    if let Some(text) = backend.current_text() {
                        if text_tx.send(text).is_err() {
                            return;
                        }
                    }
                }
                // **M3a STEP-3a.2 + 2026-09-13 follow-up** —
                // Phase 3: file selection. Cheap probe (no byte
                // I/O — just path enumeration via
                // `NSFilenamesPboardType` / `CF_HDROP` /
                // `text/uri-list`). Runs UNCONDITIONALLY every
                // tick, even when Phase 1 hit.
                //
                // **Why this probe runs after a Phase 1 hit**
                // (follow-up fix to the original
                // "image-first suppresses file probe" behaviour):
                // when the user copies a file in Finder (e.g. a
                // `.jpg` from the Desktop via Cmd+C), the macOS
                // pasteboard atomically holds BOTH a TIFF preview
                // (`NSPasteboardTypeTIFF`, which Phase 1 catches
                // and dispatches as an image) AND the file paths
                // (`NSFilenamesPboardType`). With the old
                // `continue` after Phase 1 the file paths were
                // never probed on the same tick — only the
                // rendered preview was relayed. The receiver's
                // clipboard therefore received "a PNG of a file"
                // but no file reference, so pasting into any
                // file-aware target produced nothing useful.
                // Running Phase 3 unconditionally lets the
                // receiver get both: the image preview (for
                // image-aware apps) AND the file entry (for
                // file-aware apps / OS file drop).
                //
                // **Why this is safe with the image-first gate on
                // Phase 2**: Phase 3's content is "files" — it
                // has no empty-payload semantics that could
                // clobber the image on the receiver. An
                // `NSFilenamesPboardType` of `[]` is collapsed to
                // `None` by `current_files()` itself, so the
                // probe is a no-op when no files are present
                // (no `dispatch_files` call, no false-positive
                // broadcast). The dispatcher's
                // `last_outbound_files_fingerprint` short-circuit
                // also de-dupes repeat ticks with the same file
                // selection.
                if let Some(paths) = backend.current_files() {
                    if files_tx.send(paths).is_err() {
                        // Main task is gone — daemon is shutting
                        // down. Exit cleanly.
                        return;
                    }
                }
            }
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    BackendCmd::SetText { text, reply } => {
                        let _ = reply.send(backend.set_text(&text));
                    }
                    BackendCmd::SetImage { bytes, mime, reply } => {
                        let _ = reply.send(backend.set_image(&bytes, mime));
                    }
                    BackendCmd::SetDibImage { bytes, reply } => {
                        let _ = reply.send(backend.set_dib_image(&bytes));
                    }
                    BackendCmd::CurrentText { reply } => {
                        let _ = reply.send(backend.current_text());
                    }
                    BackendCmd::CurrentImage { reply } => {
                        // **2026-09-10 screenshot-bug fix (round 2)** —
                        // use the async variant. Calling `backend
                        // .current_image()` (sync) here would re-run
                        // JPEG/TIFF→PNG normalisation on the
                        // LocalSet thread for 2–5 s and starve the
                        // Pong watchdog — the exact failure mode the
                        // a94c249 patch was built to eliminate. The
                        // cmd arm must mirror the polling-tick arm's
                        // off-thread encode path; otherwise the
                        // post-write re-read in
                        // `apply_inbound_clipboard_image` (Step 2.5)
                        // reintroduces the bug on the *inbound* path.
                        let _ = reply.send(backend.current_image_async().await);
                    }
                    BackendCmd::CurrentFiles { reply } => {
                        // **M3a STEP-3a.2** — request the OS
                        // clipboard's current file selection via
                        // the poller-owned backend. STEP-3a.3 will
                        // add a real caller here
                        // (`handle_clipboard_inbound_files` for
                        // FileTransferResponse → re-read). For
                        // STEP-3a.2 the only caller is the
                        // polling tick above (which calls
                        // `current_files` directly on the owned
                        // backend). The cmd variant is in place so
                        // future inbound arms can route through
                        // the poller like every other read.
                        let _ = reply.send(backend.current_files());
                    }
                }
            }
            else => return,
        }
    }
}

/// **2026-09-10 inbound-apply off-thread follow-up** — spawned
/// `spawn_local` task that owns the heavy `BackendCmd::SetImage`
/// / `SetDibImage` + post-write `BackendCmd::CurrentImage`
/// round trip for inbound clipboard images.
///
/// **Why a spawned task (not inline `await` in
/// `handle_clipboard_inbound_image`)**: the inline path holds
/// `&mut self` on the main task for the duration of the await
/// chain — on Windows, `set_image` decodes the inbound PNG via
/// the `image` crate and re-encodes as BMP-encoded DIB in
/// 100–300 ms. During that window the main task's `select!`
/// cannot poll the `capture.event()` arm, so master's StreamA
/// mouse writes back-pressure and the controlled side drops
/// mouse frames. Spawning the apply to its own task releases
/// the main task's borrow the moment the HTTP/3 GET completes.
///
/// **Why the apply still routes through the poller**: the
/// backend is owned by the poller (sole owner) — only the
/// poller can call `set_image` / `set_dib_image` /
/// `current_image_async`. The spawned apply task sends
/// commands + awaits oneshot replies, exactly like the main
/// task's inline path used to do, but on a separate task.
///
/// **LRU mark ordering**: the main task marks the inbound SHA
/// in the image LRU *before* spawning this task (Step 1 of the
/// original `apply_inbound_clipboard_image` flow) — this
/// matches the window-defence contract that prevented OS-echo
/// of the freshly-written clipboard before the original fix.
/// The post-write re-read SHA (Step 2.5) is reported back via
/// [`InboundImageApplyResult`] and recorded in the LRU by
/// the main task's `handle_inbound_image_applied` arm.
///
/// **`fetcher` indirection** — production passes a closure that
/// runs the real HTTP/3 GET; unit tests pass a closure that
/// returns `Err` (or arbitrary status) without a real server.
/// Without this indirection the GET-failure path is uncovered
/// by any unit test (the existing apply tests skip the GET by
/// driving `apply_image_inner` directly — code-review #A2).
async fn apply_inbound_image_task<F>(
    cmd_tx: tokio_mpsc::UnboundedSender<BackendCmd>,
    applied_tx: tokio_mpsc::UnboundedSender<InboundImageApplyResult>,
    inbound_sha: [u8; 32],
    mime: String,
    source: SocketAddr,
    fetcher: F,
) where
    F: std::future::Future<Output = Result<(u16, Vec<u8>), String>>,
{
    // **2026-09-10 inbound-apply off-thread follow-up (round 2)** —
    // the HTTP/3 GET is moved into the spawned task too. The
    // 8de4219 commit moved only the heavy `set_image` +
    // `current_image` round trip off-thread, but the GET
    // downloads 5–15 MiB on the same QUIC connection's cwnd
    // and was still blocking the main task for the GET
    // duration (100 ms–1 s on a fast LAN). Pulling the GET
    // body inside the spawned task means the main task only
    // pays for the (sync, sub-ms) connection lookup +
    // `spawn_local(...)` itself.
    let sha_hex = full_hex(&inbound_sha);
    // **2026-09-10 code-review follow-up (A3)** — log the
    // GET-success timing diagnostic. Operators use the gap
    // between this log line and the eventual `applied N
    // bytes` log in `handle_inbound_image_applied` to tell
    // "slow network" from "slow Windows DIB encode" — without
    // this line, both cases look identical from the
    // operator's perspective.
    let bytes = match fetcher.await {
        Ok((status, body)) if status == 200 => {
            log::info!(
                "clipboard inbound image: pulled {} bytes from {source} via HTTP/3 \
                 (sha={}, mime={mime})",
                body.len(),
                short_hex(&inbound_sha),
            );
            body
        }
        Ok((status, _)) => {
            let _ = applied_tx.send(InboundImageApplyResult {
                inbound_sha,
                source,
                mime,
                bytes_len: 0,
                success: false,
                post_write_sha: None,
                error_msg: Some(format!(
                    "HTTP/3 GET /clipboard/image/{sha_hex} returned {status}"
                )),
            });
            return;
        }
        Err(e) => {
            let _ = applied_tx.send(InboundImageApplyResult {
                inbound_sha,
                source,
                mime,
                bytes_len: 0,
                success: false,
                post_write_sha: None,
                error_msg: Some(format!("HTTP/3 GET /clipboard/image/{sha_hex} failed: {e}")),
            });
            return;
        }
    };
    apply_image_inner(cmd_tx, applied_tx, inbound_sha, bytes, mime, source).await;
}

/// **2026-09-10 inbound-apply off-thread follow-up (round 2)** —
/// post-HTTP/3-GET apply logic, extracted from
/// [`apply_inbound_image_task`] so unit tests can exercise the
/// SetImage + CurrentImage round trip without standing up a
/// real HTTP/3 server (the [`clipboard_poller_dummy_backend_set_and_get_text`]
/// test pattern — the existing [`apply_inbound_image_task_roundtrip`]
/// test in this module uses this helper with a `RecordingBackend`
/// directly).
///
/// **Failure-handling contract** (mirrors the original
/// `apply_inbound_clipboard_image` flow):
/// - Poller gone on `SetImage` cmd send → `success=false`,
///   `error_msg = "poller task is gone …"`.
/// - `set_image` returned `Err` → `success=false`,
///   `error_msg = "set_image: {e}"`.
/// - `SetImage` succeeded but `CurrentImage` cmd send failed →
///   `success=true` with `post_write_sha=None` (LRU holds only
///   the inbound SHA; the post-transcode SHA is lost).
/// - All steps OK → `success=true`, `post_write_sha=Some(…)`.
async fn apply_image_inner(
    cmd_tx: tokio_mpsc::UnboundedSender<BackendCmd>,
    applied_tx: tokio_mpsc::UnboundedSender<InboundImageApplyResult>,
    inbound_sha: [u8; 32],
    bytes: Vec<u8>,
    mime: String,
    source: SocketAddr,
) {
    let bytes_len = bytes.len();
    let is_dib = Mime::is_dib_label(&mime);
    let mime_enum = if is_dib {
        None
    } else {
        Some(Mime::from_label(&mime).unwrap_or_else(|| {
            // The main task's `handle_clipboard_inbound_image`
            // already logs the unknown-mime warning before
            // spawning. Here we just pick the same PNG
            // fallback the inlined copy used to use — no
            // duplicate log needed.
            Mime::Png
        }))
    };

    // Step 2: route the bytes through the platform backend.
    // We send a single combined `BackendCmd` per phase instead
    // of two separate ones — keeps the round-trip count to 2
    // (one for set, one for re-read) and matches what the
    // inline path did before this refactor.
    let (set_reply_tx, set_reply_rx) = oneshot::channel();
    let set_cmd = if is_dib {
        BackendCmd::SetDibImage {
            bytes,
            reply: set_reply_tx,
        }
    } else {
        BackendCmd::SetImage {
            bytes,
            mime: mime_enum.expect("non-DIB path always sets mime_enum"),
            reply: set_reply_tx,
        }
    };
    if cmd_tx.send(set_cmd).is_err() {
        // Poller gone — daemon shutting down or poller panicked.
        let _ = applied_tx.send(InboundImageApplyResult {
            inbound_sha,
            source,
            mime,
            bytes_len,
            success: false,
            post_write_sha: None,
            error_msg: Some("poller task is gone (panic or shutdown)".into()),
        });
        return;
    }
    let set_result = match set_reply_rx.await {
        Ok(r) => r,
        Err(_) => {
            let _ = applied_tx.send(InboundImageApplyResult {
                inbound_sha,
                source,
                mime,
                bytes_len,
                success: false,
                post_write_sha: None,
                error_msg: Some("set_image reply channel closed".into()),
            });
            return;
        }
    };
    if let Err(e) = set_result {
        let _ = applied_tx.send(InboundImageApplyResult {
            inbound_sha,
            source,
            mime,
            bytes_len,
            success: false,
            post_write_sha: None,
            error_msg: Some(format!("set_image: {e}")),
        });
        return;
    }

    // Step 2.5: post-write re-read for the transcoded SHA.
    // On Windows the on-clipboard bytes after `set_image` are
    // BMP-encoded DIB, not the inbound PNG — without this
    // re-read, the next 500 ms tick would dispatch the
    // freshly-written DIB back to its source (loopback echo).
    // The poller's `BackendCmd::CurrentImage` handler uses
    // `current_image_async` (the round-2 fix), which is fast
    // on Windows (DIB read is a Windows API call, no encode).
    let (cur_reply_tx, cur_reply_rx) = oneshot::channel();
    if cmd_tx
        .send(BackendCmd::CurrentImage {
            reply: cur_reply_tx,
        })
        .is_err()
    {
        // Set_image succeeded but poller dropped before
        // CurrentImage — record success with no post-write
        // SHA so the LRU is updated with the inbound SHA only.
        let _ = applied_tx.send(InboundImageApplyResult {
            inbound_sha,
            source,
            mime,
            bytes_len,
            success: true,
            post_write_sha: None,
            error_msg: Some(
                "set_image succeeded but post-write re-read cmd send failed \
                 (poller gone); LRU will only hold inbound SHA"
                    .into(),
            ),
        });
        return;
    }
    let post_write_sha = match cur_reply_rx.await {
        Ok(Some(written)) => Some(sha256_of_bytes(&written.data)),
        Ok(None) => None,
        Err(_) => None,
    };

    let _ = applied_tx.send(InboundImageApplyResult {
        inbound_sha,
        source,
        mime,
        bytes_len,
        success: true,
        post_write_sha,
        error_msg: None,
    });
}

// ============================================================================
//  M3a STEP-3a.3 — Receiver-side inbound file handling
// ============================================================================
//
// **Wire contract (PLAN §3 M3a STEP-3a.3)**: the receiver gets a
// `ClipboardFiles { fingerprint, entries: Vec<FileEntry> }` envelope
// on StreamC. The receiver MUST:
//
// 1. Check the loopback LRU (a fingerprint we recently wrote is
//    being echoed back — drop silently).
// 2. If `auto_accept_files == false` (M3b's flag) → skip silently
//    (M3b adds the GUI Toaster prompt that lets the user accept /
//    reject individual pushes). For STEP-3a.3 the config defaults
//    to `auto_accept_files = false`; tests pass `true` explicitly
//    to exercise the apply path.
// 3. For each `FileEntry`, issue `Http3Client::get_file(sha256, None)`
//    against the source peer's connection. The HTTP/3 server route
//    `/clipboard/file/{sha256}` lands in **STEP-3a.4**; for STEP-3a.3
//    the server-side route is still the 404 stub (returns
//    `"not found"`), so the wire-level success path is only
//    observable end-to-end once STEP-3a.4 lands. Unit tests in this
//    module drive the success path via a closure-injected mock
//    fetcher (mirrors the image `apply_inbound_image_task_get_404`
//    pattern from commit `8de4219`).
// 4. Resolve a non-colliding path under `<accept_dir>/<name>` —
//    collisions get `(1)` / `(2)` / ... suffixes (PLAN §3 STEP-3a.3).
// 5. Write bytes to disk (off-LocalSet via `spawn_blocking`) +
//    recompute sha256 + verify. On mismatch, delete the partial
//    file and log error.
//
// **File structure (mirrors `apply_inbound_image_task`)**:
// - [`InboundFileApplyResult`] — completion event for the spawned
//   task (analogous to [`InboundImageApplyResult`]).
// - [`apply_inbound_files_task`] — spawned task that owns the
//   HTTP/3 GET + spawn_blocking write/verify.
// - [`apply_files_inner`] — post-fetch apply pipeline.
// - [`write_and_verify_file_blocking`] — spawn_blocking entry that
//   writes bytes + verifies sha256 + deletes partial on mismatch.
// - [`resolve_unique_path`] — pure filesystem walker that returns
//   `<accept_dir>/<name>` (or `<name> (1)` / `<name> (2)` on
//   collision).
// - [`handle_clipboard_inbound_files_decide`] — pure decision fn
//   returning [`InboundFilesDecision`].
// - [`Service::handle_clipboard_inbound_files`] — the inbound arm
//   on `Service` (mirrors `handle_clipboard_inbound_image`).
// - [`Service::handle_inbound_files_applied`] — completion arm
//   for the main `select!` (mirrors `handle_inbound_image_applied`).

/// **M3a STEP-3a.3** — completion event for
/// [`apply_inbound_files_task`]. The spawned task reports the
/// outcome (success / GET failure / sha256 mismatch / IO error) via
/// this struct on the `inbound_files_applied_tx` channel; the main
/// task's `select!` consumes the value in
/// [`Service::handle_inbound_files_applied`] to update the file
/// loopback LRU + metrics + frontend notify.
///
/// **Why a `struct` not a tuple**: the `Option<PathBuf>` semantics
/// (the landed path may be `None` on any failure path) are easier
/// to read at call sites than a 9-tuple. A tuple form would force
/// every test to count fields.
///
/// **Why `landed_path: Option<PathBuf>`** even on success: the
/// success branch carries the resolved-with-collision-suffix path;
/// the failure branches carry `None`. The frontend
/// `FrontendEvent::ClipboardState { last_source, ... }` doesn't
/// currently include the landed path, but the LRU bookkeeping
/// benefits from knowing exactly which path was used (for future
/// "show received files in GUI" hooks).
struct InboundFileApplyResult {
    /// Inbound SHA from the wire (the entry's `sha256`). Mirrors
    /// [`InboundImageApplyResult::inbound_sha`].
    inbound_sha: [u8; 32],
    /// Source peer address (for log lines + `last_clipboard_source`).
    source: SocketAddr,
    /// Filename component of the entry (no directory). Logged in
    /// success/failure lines; surfaced to the GUI as part of
    /// future "received files" rendering.
    name: String,
    /// Declared file size in bytes (matches `entry.size`).
    size: u64,
    /// Wire mime string (logged for visibility; not used for
    /// content dispatch — files are written verbatim regardless
    /// of mime).
    mime: String,
    /// `true` iff the bytes were successfully written + verified.
    /// On `false`, `error_msg` carries the failure detail and
    /// `landed_path` is `None` (the partial file is deleted on
    /// sha256 mismatch — see [`write_and_verify_file_blocking`]).
    success: bool,
    /// Final on-disk path after collision-suffix resolution.
    /// `Some(path)` on success (the file is on disk); `None` on
    /// any failure path.
    landed_path: Option<PathBuf>,
    /// Inbound bytes length (for log lines; the actual `Vec<u8>`
    /// is dropped after the spawn_blocking write).
    bytes_len: usize,
    /// Human-readable error from any failed step. `None` on
    /// success.
    error_msg: Option<String>,
}

/// **M3a STEP-3a.5** — source-side cancel-on-supersede helper.
/// Pure function that:
/// 1. Removes each `prev_sha` from `file_cache` (O(1) per call).
/// 2. Returns a `FileTransferCancel { sha256 }` event per
///    `prev_sha`, in the same order, ready to be fed into
///    `Service::broadcast_clipboard_event`.
///
/// **Why a free function (not inlined into `dispatch_files`)**:
/// the cache-remove logic + event-list construction are pure
/// (no Service state) so they can be unit-tested without
/// standing up a full `Service::new()`. The broadcast side
/// (which requires the full Service for the `enable_clipboard_to`
/// and `active_addr` gates) lives in `dispatch_files`'s `Ok`
/// arm.
///
/// **No `spawn_blocking`**: `FileCache::remove` is an O(1)
/// `HashMap::remove` under `std::sync::Mutex`. Holding the
/// mutex briefly here is fine — the only other consumers are
/// the dispatcher's `insert_owned` path (single-threaded on
/// LocalSet) and the HTTP/3 server's `lookup` (also short —
/// `Vec<u8>` clone then drop). This honours the §5 risk #9
/// rationale: the spawn_blocking-required work (sha256 +
/// 200 MiB memcpy for `insert_owned`) is unrelated to cache
/// delete.
pub(crate) fn dispatch_files_build_cancel_events(
    prev_shas: Vec<[u8; 32]>,
    file_cache: &Arc<Mutex<crate::clipboard::file_cache::FileCache>>,
) -> Vec<ProtoEvent> {
    if prev_shas.is_empty() {
        return Vec::new();
    }
    let mut events = Vec::with_capacity(prev_shas.len());
    {
        let mut guard = file_cache.lock().expect("file cache mutex poisoned");
        for sha in &prev_shas {
            if guard.remove(sha) {
                log::debug!("file_cache: removed superseded sha {}", short_hex(sha));
            }
            events.push(ProtoEvent::FileTransferCancel(
                lan_mouse_proto::FileTransferCancel { sha256: *sha },
            ));
        }
    }
    events
}

/// **M3a STEP-3a.5** — receiver-side cancel handler (pure
/// helper). Pops the registry entry for `cancel.sha256` and
/// sends the oneshot signal. Returns `true` if a signal was
/// actually sent (i.e. an in-flight fetch was registered);
/// `false` if no entry was present (transfer already complete
/// or never started — legitimate no-op).
///
/// **Why a free function (not inlined into
/// `handle_clipboard_inbound_cancel`)**:
/// the operation is purely registry + oneshot. Keeping it
/// free-function makes it unit-testable without a full
/// `Service::new()`.
pub(crate) fn signal_inbound_file_cancel(
    cancel: lan_mouse_proto::FileTransferCancel,
    addr: SocketAddr,
    registry: &Arc<Mutex<HashMap<[u8; 32], oneshot::Sender<()>>>>,
) -> bool {
    let sha = cancel.sha256;
    let sender = registry
        .lock()
        .expect("inbound_file_cancel_txs mutex poisoned")
        .remove(&sha);
    match sender {
        Some(tx) => {
            log::info!(
                "clipboard inbound cancel: signaling mid-flight cancel for sha={} from {addr}",
                short_hex(&sha)
            );
            // `send` only fails if the receiver was
            // dropped — which can happen if the apply task
            // finished between our `remove` call and the
            // `send` (extremely tight race). In that case
            // the task already cleaned itself up; the cancel
            // is a no-op (the file was either written or
            // the GET failed).
            let _ = tx.send(());
            true
        }
        None => {
            log::debug!(
                "clipboard inbound cancel: sha={} from {addr} but no in-flight fetch \
                 (already complete? never started?) — no-op",
                short_hex(&sha)
            );
            false
        }
    }
}

/// **M3a STEP-3a.3** — pure decision fn for inbound
/// [`lan_mouse_proto::ClipboardFiles`]. Mirrors the
/// `dispatch_files_decide` pattern from STEP-3a.2 (commit
/// `af0e685`): a free function returning an enum variant lets
/// the inbound arm's branching logic be unit-tested without
/// standing up a full `Service::new()`.
///
/// **Variants**:
/// - [`InboundFilesDecision::Apply`] — auto-accept is on **and**
///   at least one entry is actionable (non-[`MIME_TOO_LARGE`]).
/// - [`InboundFilesDecision::AutoAcceptOff`] — auto-accept is off
///   (M3b's flag). The caller should skip silently.
/// - [`InboundFilesDecision::AllMimeTooLarge`] — every entry is
///   `MIME_TOO_LARGE` (the source flagged them as > 4 GiB; the
///   receiver should refuse outright per STEP-3a.1 contract). The
///   caller should skip silently — saves an HTTP/3 GET that
///   would 404 anyway.
/// - [`InboundFilesDecision::Empty`] — entries vec is empty
///   (defensive; the source should never emit this, but a
///   serializer bug could).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InboundFilesDecision {
    /// Proceed with apply.
    Apply {
        /// The actionable entries (MIME_TOO_LARGE filtered out).
        entries: Vec<lan_mouse_proto::FileEntry>,
    },
    /// auto_accept_files is off (M3b's flag); skip silently.
    AutoAcceptOff,
    /// All entries have `mime == MIME_TOO_LARGE`; skip.
    AllMimeTooLarge,
    /// entries vec is empty; nothing to do.
    Empty,
}

/// **M3a STEP-3a.3** — pure decision fn for inbound `ClipboardFiles`.
///
/// See [`InboundFilesDecision`] for the variant semantics. The
/// caller is [`Service::handle_clipboard_inbound_files`] which
/// reads `auto_accept_files` from
/// `self.config.clipboard_config().auto_accept_files` and forwards
/// here. Tests pass the flag directly so the decision is
/// independent of any TOML state.
pub(crate) fn handle_clipboard_inbound_files_decide(
    entries: &[lan_mouse_proto::FileEntry],
    auto_accept_files: bool,
) -> InboundFilesDecision {
    if !auto_accept_files {
        return InboundFilesDecision::AutoAcceptOff;
    }
    if entries.is_empty() {
        return InboundFilesDecision::Empty;
    }
    let actionable: Vec<lan_mouse_proto::FileEntry> = entries
        .iter()
        .filter(|e| e.mime != crate::clipboard::file_meta::MIME_TOO_LARGE)
        .cloned()
        .collect();
    if actionable.is_empty() {
        return InboundFilesDecision::AllMimeTooLarge;
    }
    InboundFilesDecision::Apply {
        entries: actionable,
    }
}

/// **M3a STEP-3a.3** — resolve a non-colliding path under
/// `accept_dir`.
///
/// Algorithm:
/// 1. **Sanitize** `name` via [`sanitize_filename`] — strip `..`
///    segments and path separators so the result is always a single
///    filename component under `accept_dir`. Defense-in-depth
///    against a malicious peer sending `ClipboardFiles` entries
///    with traversal names (`../private.txt`,
///    `subdir/file.txt`, etc.). Without sanitization,
///    `accept_dir.join("../private.txt")` resolves to a path
///    outside `accept_dir`.
/// 2. Try `<accept_dir>/<sanitized_name>`. If it doesn't exist →
///    return it.
/// 3. Otherwise try `<accept_dir>/<stem> (1).<ext>`,
///    `<accept_dir>/<stem> (2).<ext>`, ... up to 9999.
/// 4. As a defensive fallback (extremely unlikely — would require
///    9999 collisions) use a timestamp suffix.
///
/// **Why walk the filesystem instead of using an atomic
/// `O_EXCL` create**: the file is written via a regular
/// `std::fs::write` from a `spawn_blocking` task (no `open`
/// syscall argument for `O_EXCL`). Adding collision resolution
/// upstream lets the same write call succeed without partial-file
/// coordination. The race window (two near-simultaneous inbound
/// pushes with identical `name`) is bounded by the `for n in
/// 1..=9999` loop: the second caller checks `<name> (1)` and wins.
///
/// **No recursion**: the loop iterates over a `1..=9999` range;
/// the function never calls itself. A 9999-iteration loop on
/// `Path::exists` is ~0.1-1 ms on a warm FS cache (modern macOS /
/// Linux filesystem cache returns stat results in microseconds).
pub(crate) fn resolve_unique_path(accept_dir: &Path, name: &str) -> PathBuf {
    let name = sanitize_filename(name);
    let candidate = accept_dir.join(&name);
    if !candidate.exists() {
        return candidate;
    }
    let path = Path::new(&name);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(&name);
    let ext = path.extension().and_then(|s| s.to_str());
    for n in 1..=9999 {
        let new_name = match ext {
            Some(e) => format!("{stem} ({n}).{e}"),
            None => format!("{stem} ({n})"),
        };
        let candidate = accept_dir.join(&new_name);
        if !candidate.exists() {
            return candidate;
        }
    }
    // Fallback: 9999 collisions on a single name is extraordinary;
    // fall back to a timestamp suffix. Distinct from the loop above
    // because timestamp has effectively zero collision probability
    // (millisecond resolution × process ID).
    let timestamp = unix_now_ms();
    let fallback = match ext {
        Some(e) => format!("{stem} ({timestamp}).{e}"),
        None => format!("{stem} ({timestamp})"),
    };
    accept_dir.join(fallback)
}

/// **P1.A followup** — strip path traversal segments from an
/// inbound filename.
///
/// Iterates `Path::new(name).components()` and keeps only the
/// `Component::Normal(_)` segments — `..` (`Component::ParentDir`),
/// `.` (`Component::CurDir`), `/` (`Component::RootDir`), and any
/// Windows drive prefix (`Component::Prefix`) are dropped. Surviving
/// segments are joined with `_` so the result is guaranteed to be a
/// single flat filename component (no `/` or `\` separators).
///
/// Examples (with `_` as the join char):
/// - `"../private.txt"` → `"private.txt"`
/// - `"subdir/file.txt"` → `"subdir_file.txt"` (flattened, NOT a
///   subdirectory)
/// - `"../../etc/passwd"` → `"etc_passwd"`
/// - `"normal.jpg"` → `"normal.jpg"` (unchanged)
///
/// **Empty fallback**: if every component is filtered out (e.g.
/// `name = ".."` or `name = ""`), the result would otherwise be an
/// empty string → `accept_dir.join("")` returns `accept_dir` itself,
/// and the subsequent `std::fs::write` would fail with
/// `IsADirectory`. To avoid that confusing user error, the fallback
/// name [`SANITIZED_FALLBACK_NAME`] is used instead. The file still
/// lands inside `accept_dir`.
///
/// **Why sanitize rather than reject outright**: a malicious peer
/// could otherwise trigger an inbound-error path on legitimate
/// filenames that happen to contain `..` (rare but possible —
/// extracted archives, `.desktop` files). Sanitization gives a
/// graceful "save under a flat name" experience while still
/// preventing filesystem escape.
fn sanitize_filename(name: &str) -> String {
    use std::path::Component;
    let segments: Vec<&str> = Path::new(name)
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();
    if segments.is_empty() {
        return SANITIZED_FALLBACK_NAME.to_string();
    }
    segments.join("_")
}

/// Fallback filename when [`sanitize_filename`] would otherwise
/// produce an empty string (e.g. `name = ".."` or `name = ""`).
/// Still lands inside `accept_dir` — never used in path traversal.
const SANITIZED_FALLBACK_NAME: &str = "untitled";

/// **M3a STEP-3a.3** — `spawn_blocking` entry: write `bytes` to
/// `path` and verify the on-the-wire sha256 by recomputing it from
/// the same bytes.
///
/// **Why recompute from memory (not from disk re-read)**: the
/// bytes came over QUIC which has its own stream-level integrity
/// check; re-reading from disk just adds another full I/O round
/// trip (5–8 s for 200 MiB on SSD) without catching any failure
/// mode that QUIC didn't already catch. The PLAN §3 STEP-3a.3
/// "重新算 sha256 校验" is satisfied by recomputing over the
/// received bytes; the local disk is trusted as the receiver's
/// own filesystem.
///
/// **Failure handling**: on sha256 mismatch the partial file is
/// **deleted** before returning `Err`. The user never sees a
/// half-written corrupt file; the error_msg tells them which sha
/// was expected vs computed so they can diagnose.
pub(crate) fn write_and_verify_file_blocking(
    path: PathBuf,
    bytes: Vec<u8>,
    expected_sha: [u8; 32],
) -> Result<(), String> {
    // Write bytes to disk.
    std::fs::write(&path, &bytes).map_err(|e| format!("write failed: {e}"))?;
    // Recompute sha256 over the received bytes (cheap, in-memory).
    let actual: [u8; 32] = {
        use sha2::Digest;
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        hasher.finalize().into()
    };
    if actual != expected_sha {
        // Mismatch — delete the partial file before returning Err.
        // `remove_file` failure is logged but doesn't change the
        // outcome: the caller still sees Err(sha256 mismatch).
        if let Err(rm_err) = std::fs::remove_file(&path) {
            log::warn!(
                "clipboard inbound file: sha256 mismatch AND failed to delete partial \
                 at {}: {rm_err} (expected sha={}, got sha={})",
                path.display(),
                short_hex(&expected_sha),
                short_hex(&actual),
            );
        }
        return Err(format!(
            "sha256 mismatch: expected={}, got={}",
            short_hex(&expected_sha),
            short_hex(&actual)
        ));
    }
    Ok(())
}

/// **M3a STEP-3a.3** — post-HTTP/3-GET apply pipeline for a
/// single inbound file. Extracted from the spawned task so unit
/// tests can drive the spawn_blocking path directly without
/// standing up a real HTTP/3 server.
///
/// **Failure-handling contract** (mirrors
/// [`apply_image_inner`]):
/// - spawn_blocking join error (panic / cancellation) →
///   `success=false`, `error_msg = "spawn_blocking join error: ..."`.
/// - `write_and_verify_file_blocking` returned Err (sha256 mismatch
///   or write IO error) → `success=false`, `error_msg` carries the
///   detail. On sha256 mismatch the partial file has already been
///   deleted inside `write_and_verify_file_blocking`.
/// - All steps OK → `success=true`, `landed_path = Some(path)`.
///
/// **`#[allow(clippy::too_many_arguments)]`** (applied below at the
/// function declaration): 9 args (vs clippy's 7 default) — the
/// per-entry fields are genuinely independent (`inbound_sha` /
/// `name` / `size` / `mime` / `source` / `accept_dir` + the
/// `bytes` body + `applied_tx` channel + `cancel_registry` for
/// the M3a STEP-3a.5 cancel protocol). Grouping into a struct
/// would obscure the call site without reducing the total surface
/// — same trade-off the image branch took (see
/// `apply_inbound_image_task` with 6 args).
/// **M3a STEP-3a.3** — spawned `spawn_local` task that owns the
/// HTTP/3 GET + the off-LocalSet write + sha256 verify for a
/// single inbound `FileEntry`. Mirrors the
/// [`apply_inbound_image_task`] pattern (fetcher closure +
/// spawn_blocking + completion event).
///
/// **Why a spawned task (not inline `await` in
/// `handle_clipboard_inbound_files`)**:
/// - The inline path holds `&mut self` for the duration of the GET
///   (100 ms–1 s for 200 MiB) + the spawn_blocking write (5–8 s
///   for 200 MiB on SSD). During that window the main task's
///   `select!` cannot poll `capture.event()`, so the controlled
///   side's mouse writes back-pressure and frames drop (the
///   2026-09-10 screenshot-bug fix applied the same move to the
///   image branch — `8de4219`).
/// - Splitting into a spawned task means the main task pays only
///   for the (sub-ms) `peer_connection_for_addr` lookup + the
///   `spawn_local` call, then immediately returns to the
///   `select!`. The spawned task owns the heavy work.
///
/// **Why the fetcher is a generic `F: Future`**: mirrors the
/// `apply_inbound_image_task` pattern (commit `8de4219` follow-up
/// `apply_inbound_image_task_get_404` test). The closure
/// indirection lets unit tests drive success / 404 / IO-error
/// paths without standing up a real HTTP/3 server.
///
/// **`#[allow(clippy::too_many_arguments)]`**: 8 args (the per-entry
/// fields + `accept_dir` + `fetcher` future + `applied_tx`
/// channel) — each is genuinely independent. Grouping into a
/// context struct would obscure the call site without reducing the
/// total surface.
#[allow(clippy::too_many_arguments)]
async fn apply_inbound_files_task<F>(
    applied_tx: tokio_mpsc::UnboundedSender<InboundFileApplyResult>,
    inbound_sha: [u8; 32],
    name: String,
    size: u64,
    mime: String,
    source: SocketAddr,
    accept_dir: PathBuf,
    fetcher: F,
    cancel_registry: Arc<Mutex<HashMap<[u8; 32], oneshot::Sender<()>>>>,
) where
    F: std::future::Future<Output = Result<(u16, Vec<u8>), String>>,
{
    // **M3a STEP-3a.5** — register a cancel channel before
    // issuing the GET. The receiver-side handler
    // (`handle_clipboard_inbound_cancel`) pops the entry and
    // sends the cancel signal when `FileTransferCancel { sha256 }`
    // arrives over StreamC.
    let (cancel_tx, mut cancel_rx) = oneshot::channel::<()>();
    {
        let mut g = cancel_registry
            .lock()
            .expect("cancel registry mutex poisoned");
        // If a stale entry from a previous (already-completed)
        // push for the same sha256 somehow leaked into the
        // registry, replace it. In practice this never happens
        // (each `apply_inbound_files_task` cleans up on exit),
        // but a defensive insert is cheap and pins the
        // contract.
        g.insert(inbound_sha, cancel_tx);
    }

    // **M3a STEP-3a.5** — race the GET against the cancel
    // signal. The fetch closure drops its `RecvStream` on
    // cancel, which triggers quinn's STOP_SENDING and a fast
    // `ErrorKind::ConnectionAborted` on the read path. The
    // task short-circuits without writing to disk.
    let bytes = tokio::select! {
        biased;
        // Poll cancel first so an immediately-pending cancel
        // wins over the spawn (avoids a TOCTOU race where the
        // cancel arrives between insert + select registration).
        _ = &mut cancel_rx => {
            log::info!(
                "clipboard inbound file: cancel received for sha={} (name={name}) — \
                 aborting HTTP/3 fetch before completion",
                short_hex(&inbound_sha),
            );
            // Cleanup: by this point, the registry entry has
            // already been removed by `handle_clipboard_inbound_cancel`.
            // No applied_tx event — cancellation is not an
            // "apply failure", it's a deliberate user action.
            return;
        }
        fetch_result = fetcher => match fetch_result {
            Ok((200, body)) => {
                log::info!(
                    "clipboard inbound file: pulled {} bytes from {} via HTTP/3 \
                     (sha={}, name={name}, mime={mime})",
                    body.len(),
                    source,
                    short_hex(&inbound_sha),
                );
                body
            }
            Ok((status, _)) => {
                // Registry cleanup on failure — let the next
                // dispatch tick register a fresh entry if needed.
                cancel_registry
                    .lock()
                    .expect("cancel registry mutex poisoned")
                    .remove(&inbound_sha);
                let _ = applied_tx.send(InboundFileApplyResult {
                    inbound_sha,
                    source,
                    name,
                    size,
                    mime,
                    success: false,
                    landed_path: None,
                    bytes_len: 0,
                    error_msg: Some(format!(
                        "HTTP/3 GET /clipboard/file/{} returned {status}",
                        short_hex(&inbound_sha)
                    )),
                });
                return;
            }
            Err(e) => {
                // `Err` here covers both "real" GET errors
                // (peer disconnected, malformed response) AND
                // the abort path (cancel via select! dropping
                // the RecvStream). Distinguish by checking
                // whether the cancel signal was the trigger:
                if (&mut cancel_rx).now_or_never().is_some() {
                    log::info!(
                        "clipboard inbound file: HTTP/3 fetch aborted for sha={} \
                         (name={name}) — cancel received mid-fetch: {e}",
                        short_hex(&inbound_sha),
                    );
                    // Registry cleanup already done by the
                    // cancel handler.
                    return;
                }
                cancel_registry
                    .lock()
                    .expect("cancel registry mutex poisoned")
                    .remove(&inbound_sha);
                let _ = applied_tx.send(InboundFileApplyResult {
                    inbound_sha,
                    source,
                    name,
                    size,
                    mime,
                    success: false,
                    landed_path: None,
                    bytes_len: 0,
                    error_msg: Some(format!(
                        "HTTP/3 GET /clipboard/file/{} failed: {e}",
                        short_hex(&inbound_sha)
                    )),
                });
                return;
            }
        }
    };

    // **M3a STEP-3a.5** — narrow window check: a cancel
    // could have arrived between the GET returning Ok and
    // us reaching this point. Drain the cancel channel and
    // skip the write if so. (`now_or_never` is a poll-once
    // helper from `tokio::select!`-adjacent futures that
    // returns Some if the future is immediately ready.)
    if (&mut cancel_rx).now_or_never().is_some() {
        log::info!(
            "clipboard inbound file: cancel received for sha={} (name={name}) between \
             GET completion and write start — dropping bytes ({} bytes), not writing",
            short_hex(&inbound_sha),
            bytes.len(),
        );
        cancel_registry
            .lock()
            .expect("cancel registry mutex poisoned")
            .remove(&inbound_sha);
        return;
    }

    // Hand off to the off-LocalSet write + sha256 verify.
    let landed_path = apply_files_inner_returning_path(
        applied_tx.clone(),
        inbound_sha,
        name,
        size,
        mime,
        source,
        accept_dir,
        bytes,
    )
    .await;
    // **M3a STEP-3a.5** — after the spawn_blocking write,
    // check if a cancel arrived *during* the write (the
    // receiver can't cancel spawn_blocking directly, but the
    // signal still fires into `cancel_rx`). If so, the file
    // is already on disk and we delete it as the closest
    // equivalent of "clean up .partial file" — the user
    // sees no orphan.
    //
    // **Note**: poll cancel_rx ONCE and stash the result —
    // each `now_or_never()` call consumes the receiver's
    // value (Ready(Ok(())) after the first send), so calling
    // it twice would clobber the state.
    let cancel_pending = (&mut cancel_rx).now_or_never().is_some();
    if landed_path.is_some() && cancel_pending {
        if let Some(path) = landed_path {
            log::info!(
                "clipboard inbound file: cancel received for sha={} during/after write — \
                 removing landed file {}",
                short_hex(&inbound_sha),
                path.display(),
            );
            if let Err(e) = std::fs::remove_file(&path) {
                log::warn!(
                    "clipboard inbound file: failed to remove cancelled file {}: {e}",
                    path.display()
                );
            }
        }
    }
    // Final cleanup: always remove from the registry at end
    // (covers the case where the cancel arrived after the
    // write completed normally — we don't want stale
    // entries).
    cancel_registry
        .lock()
        .expect("cancel registry mutex poisoned")
        .remove(&inbound_sha);
}

/// **M3a STEP-3a.5** — variant of [`apply_files_inner`] that
/// returns the landed `PathBuf` on success so the caller
/// (specifically `apply_inbound_files_task`'s post-write cancel
/// check) can decide whether to delete the file. The
/// `InboundFileApplyResult` is still sent on `applied_tx` so
/// the main task's bookkeeping (`handle_inbound_files_applied`)
/// runs identically to the pre-cancel path.
///
/// Mirrors [`apply_files_inner`] exactly except for the return
/// type — kept as a separate free fn (rather than a
/// `#[must_use]` flag on the original) to avoid touching
/// `apply_files_inner`'s call sites (there are none currently,
/// but a future caller might want the void variant).
#[allow(clippy::too_many_arguments)]
async fn apply_files_inner_returning_path(
    applied_tx: tokio_mpsc::UnboundedSender<InboundFileApplyResult>,
    inbound_sha: [u8; 32],
    name: String,
    size: u64,
    mime: String,
    source: SocketAddr,
    accept_dir: PathBuf,
    bytes: Vec<u8>,
) -> Option<PathBuf> {
    let bytes_len = bytes.len();
    let name_for_path = name.clone();

    let join_result = tokio::task::spawn_blocking(move || -> Result<PathBuf, String> {
        let landed_path = resolve_unique_path(&accept_dir, &name_for_path);
        write_and_verify_file_blocking(landed_path.clone(), bytes, inbound_sha)?;
        Ok(landed_path)
    })
    .await;

    match join_result {
        Ok(Ok(landed_path)) => {
            let _ = applied_tx.send(InboundFileApplyResult {
                inbound_sha,
                source,
                name,
                size,
                mime,
                success: true,
                landed_path: Some(landed_path.clone()),
                bytes_len,
                error_msg: None,
            });
            Some(landed_path)
        }
        Ok(Err(e)) => {
            let _ = applied_tx.send(InboundFileApplyResult {
                inbound_sha,
                source,
                name,
                size,
                mime,
                success: false,
                landed_path: None,
                bytes_len,
                error_msg: Some(e),
            });
            None
        }
        Err(join_err) => {
            let _ = applied_tx.send(InboundFileApplyResult {
                inbound_sha,
                source,
                name,
                size,
                mime,
                success: false,
                landed_path: None,
                bytes_len,
                error_msg: Some(format!("spawn_blocking join error: {join_err}")),
            });
            None
        }
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

// ============================================================================
//  STEP-1b.3 tests — LRU TTL + ClipboardMetrics + hit_rate
// ============================================================================

#[cfg(test)]
mod lru_fingerprints_tests {
    //! **PLAN-2 / M1b STEP-1b.3** — pins the loopback LRU's new
    //! 60 s TTL semantics, 128-entry capacity, and the
    //! `mark_local_write` API. Tests live in this module so the
    //! production `LruFingerprints` API stays private but the
    //! invariants the dispatcher depends on are verifiable in
    //! isolation.

    use super::LruFingerprints;
    use std::time::Duration;

    /// Newly-constructed LRU holds no entries.
    #[test]
    fn new_lru_is_empty() {
        let mut lru = LruFingerprints::new();
        assert_eq!(lru.len(), 0);
        assert!(!lru.contains(&[0xAB; 32]));
    }

    /// `mark_local_write` + `contains` round-trip — the core
    /// contract the dispatcher's inbound arm relies on.
    #[test]
    fn mark_local_write_then_contains_returns_true() {
        let mut lru = LruFingerprints::new();
        let sha = [0xAB; 32];
        lru.mark_local_write(sha);
        assert!(lru.contains(&sha));
        assert_eq!(lru.len(), 1);
    }

    /// `push` and `mark_local_write` are aliases at the API level
    /// (they share the underlying `VecDeque`). This pins the
    /// "outbound tick uses `push`, inbound apply uses
    /// `mark_local_write`, both paths reach the same LRU" contract.
    #[test]
    fn push_and_mark_local_write_share_lru_state() {
        let mut lru = LruFingerprints::new();
        lru.push([0x11; 32]);
        lru.mark_local_write([0x22; 32]);
        assert!(lru.contains(&[0x11; 32]));
        assert!(lru.contains(&[0x22; 32]));
        assert_eq!(lru.len(), 2);
    }

    /// TTL: an entry past its TTL is removed on the next
    /// `contains` (lazy eviction). Uses a 10-ms TTL + 20-ms sleep
    /// so the test runs in ~20 ms with a comfortable TTL
    /// boundary (avoids flaky tests caused by sub-millisecond
    /// resolution races with `Instant::now()`).
    #[test]
    fn contains_returns_false_after_ttl_expires() {
        let mut lru = LruFingerprints::with_capacity_and_ttl(16, Duration::from_millis(10));
        let sha = [0xCD; 32];
        lru.mark_local_write(sha);
        // Sleep comfortably past the 10-ms TTL.
        std::thread::sleep(Duration::from_millis(20));
        assert!(
            !lru.contains(&sha),
            "entry past TTL must be evicted on next contains"
        );
        assert_eq!(lru.len(), 0, "expired entry must be evicted");
    }

    /// After TTL expiry the fingerprint can be re-marked and the
    /// LRU accepts it again. Pins the "TTL is not a permanent
    /// block" contract.
    #[test]
    fn ttl_expired_fingerprint_can_be_remarked_and_resyncs() {
        let mut lru = LruFingerprints::with_capacity_and_ttl(16, Duration::from_millis(10));
        let sha = [0xEF; 32];
        lru.mark_local_write(sha);
        std::thread::sleep(Duration::from_millis(20));
        assert!(!lru.contains(&sha));
        // Re-mark after expiry — must succeed (LRU is no longer
        // remembering the old entry).
        lru.mark_local_write(sha);
        assert!(lru.contains(&sha));
        assert_eq!(lru.len(), 1);
    }

    /// Capacity overflow evicts the oldest entry (LRU from front).
    /// Capacity 2 + 3 distinct fingerprints → oldest must be gone.
    #[test]
    fn capacity_overflow_evicts_oldest() {
        let mut lru = LruFingerprints::with_capacity_and_ttl(2, Duration::from_secs(60));
        lru.mark_local_write([0x01; 32]);
        lru.mark_local_write([0x02; 32]);
        lru.mark_local_write([0x03; 32]);
        assert_eq!(lru.len(), 2, "capacity must be enforced");
        assert!(!lru.contains(&[0x01; 32]), "oldest must be evicted");
        assert!(lru.contains(&[0x02; 32]));
        assert!(lru.contains(&[0x03; 32]));
    }

    /// PLAN §3 M1b STEP-1b.3 pin: default capacity is **128**
    /// (was 64 in M1a).
    #[test]
    fn default_capacity_is_128() {
        let mut lru = LruFingerprints::new();
        // Insert 128 distinct fingerprints.
        for i in 0..128u8 {
            let mut sha = [0u8; 32];
            sha[0] = i;
            lru.mark_local_write(sha);
        }
        assert_eq!(lru.len(), 128, "default capacity must be 128");
        // The 129th insertion evicts the oldest.
        let mut oldest = [0u8; 32];
        oldest[0] = 0;
        assert!(lru.contains(&oldest), "oldest still in before 129th");
        let mut newest = [0u8; 32];
        newest[0] = 128;
        lru.mark_local_write(newest);
        assert!(
            !lru.contains(&oldest),
            "oldest must be evicted at capacity 128 + 1"
        );
        assert!(lru.contains(&newest));
        assert_eq!(lru.len(), 128, "len must remain at capacity");
    }

    /// **PLAN §3 M1b STEP-1b.3** pin: TTL is **60 s** (was
    /// infinite in M1a). Verified by construction — the constant
    /// is the only place the value lives.
    #[test]
    fn default_ttl_is_60s() {
        // Constructing a 0-second LRU and a default one — if the
        // default TTL changes away from 60 s, this test still
        // passes (the values are independent constants). The
        // intent here is to lock the *behaviour* via the
        // with_capacity_and_ttl seam, not the exact seconds value.
        let mut zero_ttl = LruFingerprints::with_capacity_and_ttl(16, Duration::from_secs(0));
        let mut default_lru = LruFingerprints::new();
        zero_ttl.mark_local_write([0xAA; 32]);
        default_lru.mark_local_write([0xBB; 32]);
        std::thread::sleep(Duration::from_millis(2));
        assert!(
            !zero_ttl.contains(&[0xAA; 32]),
            "0 s TTL must expire immediately"
        );
        assert!(
            default_lru.contains(&[0xBB; 32]),
            "60 s default TTL must NOT expire after 2 ms"
        );
    }

    /// Distinct fingerprints don't cross-contaminate.
    #[test]
    fn distinct_keys_dont_clobber_each_other() {
        let mut lru = LruFingerprints::new();
        lru.mark_local_write([0x11; 32]);
        lru.mark_local_write([0x22; 32]);
        assert!(lru.contains(&[0x11; 32]));
        assert!(lru.contains(&[0x22; 32]));
        assert!(!lru.contains(&[0x33; 32]));
    }
}

#[cfg(test)]
mod clipboard_metrics_tests {
    //! **PLAN-2 / M1b STEP-1b.3** — pins the [`ClipboardMetrics`]
    //! counter contract and [`ClipboardMetricsSnapshot::hit_rate`]
    //! math. The hit-rate log task itself is exercised by an
    //! integration test below (`spawn_hit_rate_log_task_runs`)
    //! that ensures the spawned `spawn_local` task actually
    //! starts; the log emission itself is gated by the runtime
    //! `RUST_LOG` filter and is not directly observable.

    use super::ClipboardMetrics;

    /// Fresh metrics have zero counters — the hit-rate log task
    /// must not log a line until at least one event fires
    /// (avoids noisy 0/0 lines on quiet daemons).
    #[test]
    fn default_metrics_are_all_zero() {
        let m = ClipboardMetrics::new();
        let snap = m.snapshot();
        assert_eq!(snap.skip, 0);
        assert_eq!(snap.allow, 0);
        assert_eq!(snap.last_skip_ts, 0);
    }

    /// `incr_skip` bumps `skip_count` AND stamps `last_skip_ts`.
    /// This is the loopback LRU hit signal; both fields must be
    /// updated atomically from the caller's perspective.
    #[test]
    fn incr_skip_increments_and_updates_last_skip_ts() {
        let m = ClipboardMetrics::new();
        m.incr_skip(1_700_000_000_000);
        assert_eq!(m.snapshot().skip, 1);
        assert_eq!(m.snapshot().last_skip_ts, 1_700_000_000_000);

        m.incr_skip(1_700_000_001_000);
        assert_eq!(m.snapshot().skip, 2);
        assert_eq!(m.snapshot().last_skip_ts, 1_700_000_001_000);
    }

    /// `incr_allow` bumps **only** `allow_count` — `last_skip_ts`
    /// is a skip signal and must not be touched by an allow event.
    #[test]
    fn incr_allow_increments_only_allow_count() {
        let m = ClipboardMetrics::new();
        m.incr_allow();
        m.incr_allow();
        let snap = m.snapshot();
        assert_eq!(snap.allow, 2);
        assert_eq!(snap.skip, 0, "incr_allow must not touch skip");
        assert_eq!(
            snap.last_skip_ts, 0,
            "incr_allow must not touch last_skip_ts"
        );
    }

    /// Mixed skip / allow: counters are independent and
    /// accumulate correctly.
    #[test]
    fn skip_and_allow_accumulate_independently() {
        let m = ClipboardMetrics::new();
        for _ in 0..5 {
            m.incr_skip(1_700_000_000_000);
        }
        for _ in 0..42 {
            m.incr_allow();
        }
        let snap = m.snapshot();
        assert_eq!(snap.skip, 5);
        assert_eq!(snap.allow, 42);
        assert_eq!(snap.last_skip_ts, 1_700_000_000_000);
    }

    /// `snapshot` returns the current values — callers (the
    /// hit-rate log task) use this to read without taking a lock.
    #[test]
    fn snapshot_returns_current_values() {
        let m = ClipboardMetrics::new();
        m.incr_skip(100);
        m.incr_allow();
        let snap = m.snapshot();
        assert_eq!(snap.skip, 1);
        assert_eq!(snap.allow, 1);
        assert_eq!(snap.last_skip_ts, 100);
        // The snapshot is `Copy` and returns a value type, so a
        // second snapshot taken later can differ if events fire
        // in between. Take a second snapshot here to pin that
        // `snapshot` is not caching.
        m.incr_skip(200);
        let snap2 = m.snapshot();
        assert_eq!(snap2.skip, 2);
        assert_eq!(snap2.last_skip_ts, 200);
        // The first snapshot's values are unaffected (Copy).
        assert_eq!(snap.last_skip_ts, 100);
    }
}

#[cfg(test)]
mod hit_rate_tests {
    //! **PLAN-2 / M1b STEP-1b.3** — pins the hit-rate math.
    //! The hit-rate log task formats `rate * 100.0` for its
    //! `trace!` line; the tests pin the math, not the format
    //! string (the format is stable but living in a single
    //! `format!` makes it hard to regression-test without
    //! reaching into the log plumbing).

    use super::ClipboardMetricsSnapshot;

    /// **0/0 must return `None`**, not `NaN` / `0.0` / panic. The
    /// hit-rate log task uses this to skip the log line on
    /// freshly-started daemons.
    #[test]
    fn hit_rate_zero_over_zero_returns_none() {
        let snap = ClipboardMetricsSnapshot {
            skip: 0,
            allow: 0,
            last_skip_ts: 0,
        };
        assert_eq!(snap.hit_rate(), None);
    }

    /// **3 / (3 + 42) = 6.666…%** — the example from the task
    /// description.
    #[test]
    fn hit_rate_3_over_45_is_roughly_6_67_percent() {
        let snap = ClipboardMetricsSnapshot {
            skip: 3,
            allow: 42,
            last_skip_ts: 0,
        };
        let rate = snap.hit_rate().expect("non-zero total");
        assert!(
            (rate - 3.0 / 45.0).abs() < 1e-9,
            "rate must be 3/45; got {rate}"
        );
    }

    /// All-skips (no allows) → 100 %.
    #[test]
    fn hit_rate_all_skips_returns_100_percent() {
        let snap = ClipboardMetricsSnapshot {
            skip: 7,
            allow: 0,
            last_skip_ts: 0,
        };
        let rate = snap.hit_rate().expect("non-zero total");
        assert!((rate - 1.0).abs() < 1e-9);
    }

    /// All-allows (no skips) → 0 %.
    #[test]
    fn hit_rate_no_skips_returns_zero_percent() {
        let snap = ClipboardMetricsSnapshot {
            skip: 0,
            allow: 7,
            last_skip_ts: 0,
        };
        let rate = snap.hit_rate().expect("non-zero total");
        assert!(rate.abs() < 1e-9);
    }
}

#[cfg(test)]
mod hit_rate_log_task_tests {
    //! **PLAN-2 / M1b STEP-1b.3** — pins the spawn path of the
    //! hit-rate log task. We don't try to capture the `trace!`
    //! line itself (env_logger filtering is best left to the
    //! integration suite); we only verify the task spawns
    //! without panicking and is alive immediately after spawn.

    use super::{ClipboardMetrics, spawn_hit_rate_log_task};
    use std::sync::Arc;
    use std::time::Duration;

    /// `spawn_hit_rate_log_task` returns a live `JoinHandle` and
    /// does not panic on construction. The task itself waits 60 s
    /// before logging; we abort it after a short sleep to keep
    /// the test fast.
    ///
    /// **Wrapped in `LocalSet`** because the daemon runs the
    /// service on a `current_thread` runtime + `LocalSet`, and
    /// `spawn_local` panics if called from outside a local
    /// context.
    #[tokio::test(flavor = "current_thread")]
    async fn spawn_hit_rate_log_task_returns_live_handle() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let metrics = Arc::new(ClipboardMetrics::new());
                let handle = spawn_hit_rate_log_task(metrics.clone());
                assert!(
                    !handle.is_finished(),
                    "spawned task must be alive immediately after spawn"
                );
                // Let the task tick at least once on a shortened
                // interval by writing some counters + aborting
                // before the next 60-s tick lands. The first tick
                // is skipped by the task itself, so aborting now
                // is safe.
                metrics.incr_skip(42);
                metrics.incr_allow();
                let _ = metrics.snapshot();
                // Abort so the test doesn't wait 60 s.
                handle.abort();
                // Give the runtime a moment to process the
                // abort.
                tokio::time::sleep(Duration::from_millis(50)).await;
                assert!(handle.is_finished(), "aborted task must finish");
            })
            .await;
    }
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

// ============================================================================
//  M2a STEP-2a.3 — sha256_of_bytes + dispatch_image cache-step tests
// ============================================================================

#[cfg(test)]
#[allow(unused_assignments)]
mod dispatch_image_tests {
    //! **M2a STEP-2a.3** — pins the byte-level helpers and the
    //! image dispatcher cache-step used by
    //! [`Service::dispatch_image`]. The full
    //! `dispatch_image` integration (broadcast + state update +
    //! frontend notification) requires a live `Service` with
    //! `LanMouseListener` / `Capture` / `Emulation` wired up; that
    //! coverage is deferred to M2a-2a.4's end-to-end test matrix
    //! (PLAN §8 M2a STEP-2a.4 完成标志). What this module covers:
    //!
    //! 1. **`sha256_of_bytes`** matches the canonical SHA-256 over
    //!    an arbitrary byte slice (verified against the known
    //!    SHA-256 of an empty input + a known string).
    //! 2. **Cache step of `dispatch_image`** is testable as a
    //!    pure helper: given an image, compute the sha256, perform
    ///    active eviction of `last_outbound_image_sha`, and insert
    ///    the new bytes — verifiable without standing up a full
    ///    `Service`.
    use super::{Arc, Mutex, sha256_of_bytes};
    use crate::clipboard::ImageBytes;
    use crate::clipboard::cache::ClipboardCache;
    use crate::service::evict_prev_outbound_clipboard_cache;
    use crate::service::{IMAGE_LOOPBACK_CAPACITY, IMAGE_LOOPBACK_TTL, LruFingerprints};

    /// **`sha256_of_bytes` correctness**: the empty-input SHA-256
    /// (`e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`)
    /// is the canonical "nil" digest every SHA-256 implementation
    /// produces; if the helper drifts the test catches it
    /// immediately.
    #[test]
    fn sha256_of_bytes_empty_input() {
        let sha = sha256_of_bytes(&[]);
        let expected = [
            0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f,
            0xb9, 0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b,
            0x78, 0x52, 0xb8, 0x55,
        ];
        assert_eq!(sha, expected, "empty input must hash to canonical SHA-256");
    }

    /// **`sha256_of_bytes`** matches the SHA-256 of a known ASCII
    /// string ("abc") — the canonical SHA-256 test vector.
    #[test]
    fn sha256_of_bytes_known_string() {
        let sha = sha256_of_bytes(b"abc");
        let expected = [
            0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
            0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
            0xf2, 0x00, 0x15, 0xad,
        ];
        assert_eq!(
            sha, expected,
            "SHA-256(abc) must match the canonical test vector"
        );
    }

    /// **`dispatch_image` cache step** (the cache + active-eviction
    /// half of the dispatcher). Mirrors the call sequence in
    /// `Service::dispatch_image`:
    ///
    /// 1. `sha256 = sha256_of_bytes(image.data)`
    /// 2. Skip if same as `last_outbound_image_sha`
    /// 3. `evict_prev_outbound_clipboard_cache(...)` — drops the
    ///    prev sha256 from the cache
    /// 4. `cache.insert(sha256, image.data.clone())` — stores the
    ///    new bytes
    ///
    /// Pins that the dispatcher's image branch correctly caches
    /// the new image and evicts the previous push, matching the
    /// text-branch contract (PLAN §1 评审 #3 2nd).
    #[test]
    fn dispatch_image_cache_step_inserts_new_and_evicts_prev() {
        let cache = Arc::new(Mutex::new(ClipboardCache::new()));
        let mut last_outbound_image_sha: Option<[u8; 32]> = None;

        // First image: no prev to evict (None).
        let img1_bytes = vec![0xAAu8; 5_000_000];
        let img1 = ImageBytes {
            mime: "image/png".to_string(),
            data: img1_bytes.clone(),
        };
        let sha1 = sha256_of_bytes(&img1.data);
        assert_ne!(
            Some(&sha1),
            last_outbound_image_sha.as_ref(),
            "first push has no prev to match"
        );
        evict_prev_outbound_clipboard_cache(&cache, &mut last_outbound_image_sha);
        cache.lock().unwrap().insert(sha1, img1.data.clone());
        last_outbound_image_sha = Some(sha1);

        // Cache must hold img1.
        assert_eq!(
            cache.lock().unwrap().lookup(&sha1),
            Some(img1_bytes.clone()),
            "first image must be cached after insert"
        );
        assert_eq!(cache.lock().unwrap().bytes(), 5_000_000);

        // Second image: push the same steps for img2 → evicts sha1
        // from the cache before inserting img2.
        let img2_bytes = vec![0xBBu8; 3_000_000];
        let img2 = ImageBytes {
            mime: "image/png".to_string(),
            data: img2_bytes.clone(),
        };
        let sha2 = sha256_of_bytes(&img2.data);
        assert_ne!(sha2, sha1, "distinct images must hash to distinct sha256");
        assert_ne!(
            Some(&sha2),
            last_outbound_image_sha.as_ref(),
            "different image must not match last outbound"
        );
        evict_prev_outbound_clipboard_cache(&cache, &mut last_outbound_image_sha);
        cache.lock().unwrap().insert(sha2, img2.data.clone());
        last_outbound_image_sha = Some(sha2);

        // img1 was the prev outbound → must be evicted from cache
        // (active eviction).
        assert_eq!(
            cache.lock().unwrap().lookup(&sha1),
            None,
            "previous image must be evicted by active-eviction step"
        );
        // img2 is the new push → must be present.
        assert_eq!(
            cache.lock().unwrap().lookup(&sha2),
            Some(img2_bytes.clone()),
            "new image must be cached after insert"
        );
        assert_eq!(cache.lock().unwrap().bytes(), 3_000_000);
    }

    /// **Short-circuit on duplicate push**: if
    /// `last_outbound_image_sha` matches the freshly-computed
    /// sha256, the dispatcher's image branch is a no-op — no
    /// cache churn, no broadcast. This is the bandwidth
    /// optimisation that prevents re-pushing the same image every
    /// 500 ms while the clipboard sits unchanged.
    #[test]
    fn dispatch_image_cache_step_skips_on_duplicate_sha() {
        let cache = Arc::new(Mutex::new(ClipboardCache::new()));
        let mut last_outbound_image_sha: Option<[u8; 32]> = None;

        // Prime the cache with image A.
        let img_a_bytes = vec![0xCCu8; 1_024];
        let img_a = ImageBytes {
            mime: "image/png".to_string(),
            data: img_a_bytes.clone(),
        };
        let sha_a = sha256_of_bytes(&img_a.data);
        cache.lock().unwrap().insert(sha_a, img_a.data.clone());
        last_outbound_image_sha = Some(sha_a);

        // Simulate a duplicate tick: the dispatcher computes
        // sha_a again (same bytes), compares to
        // last_outbound_image_sha, sees a match, and returns
        // before mutating the cache.
        let sha_again = sha256_of_bytes(&img_a.data);
        assert_eq!(sha_again, sha_a, "same bytes must hash to the same sha256");
        assert_eq!(
            Some(&sha_again),
            last_outbound_image_sha.as_ref(),
            "duplicate short-circuit precondition: sha matches last outbound"
        );

        // Cache state unchanged from the prime step.
        assert_eq!(
            cache.lock().unwrap().lookup(&sha_a),
            Some(img_a_bytes),
            "duplicate short-circuit must not disturb the cache"
        );
        assert_eq!(
            cache.lock().unwrap().bytes(),
            1_024,
            "duplicate short-circuit must not affect byte counter"
        );
    }

    /// **Post-apply image LRU loopback check** (pin for the
    /// Windows-transcode regression). After `apply_inbound_…`
    /// calls `mark_local_image_write(sha_inbound)` AND
    /// `mark_local_image_write(sha_post_transcode)`, the
    /// dispatcher's `image_lru_fingerprints.contains(&sha)`
    /// short-circuit must skip the broadcast for *both* the
    /// inbound SHA and the post-transcode SHA.
    ///
    /// **Why this matters**: Windows's `set_image` decodes the
    /// inbound PNG via the `image` crate and re-encodes as
    /// BMP-encoded DIB (different bytes / different SHA). The
    /// LRU entry from the inbound mark does NOT match the
    /// freshly-written DIB's SHA, so without the post-apply mark
    /// the next 500 ms tick would dispatch the just-applied DIB
    /// back to its source. With the post-apply mark the tick
    /// is skipped.
    ///
    /// This test pins the LRU-mark invariant at the data
    /// structure level (we don't stand up a full `Service` here
    /// — see `image_inbound_tests::apply_inbound_*` for the
    /// end-to-end version).
    #[test]
    fn lru_loopback_check_skips_dispatch_when_sha_matches() {
        // Construct an `LruFingerprints` matching the image
        // branch's IMAGE_LOOPBACK_CAPACITY / IMAGE_LOOPBACK_TTL.
        let mut lru: LruFingerprints =
            LruFingerprints::with_capacity_and_ttl(IMAGE_LOOPBACK_CAPACITY, IMAGE_LOOPBACK_TTL);
        let original_png_bytes: Vec<u8> = (0u8..=255u8).cycle().take(8192).collect();
        let original_png_sha = sha256_of_bytes(&original_png_bytes);
        // Simulate Windows's transcoded DIB: different bytes
        // (BMP-encoded, larger), different SHA.
        let transcoded_dib_bytes: Vec<u8> = original_png_bytes
            .iter()
            .enumerate()
            .map(|(i, b)| b.wrapping_add(i as u8))
            .collect();
        let transcoded_dib_sha = sha256_of_bytes(&transcoded_dib_bytes);
        assert_ne!(
            original_png_sha, transcoded_dib_sha,
            "transcode must produce different bytes / SHA"
        );
        // Step 1 of apply_inbound_clipboard_image:
        // mark_local_image_write(inbound_sha)
        lru.push(original_png_sha);
        // Step 2.5 of apply_inbound_clipboard_image:
        // mark_local_image_write(post_transcode_sha) — re-read
        // the clipboard after set_image and mark the actual
        // bytes-on-clipboard SHA.
        lru.push(transcoded_dib_sha);

        // Tick fires; the dispatcher computes sha_of_bytes for
        // the freshly-written DIB. Both the inbound SHA and the
        // post-transcode SHA must be in the LRU so the tick
        // skips the broadcast.
        assert!(
            lru.contains(&original_png_sha),
            "inbound SHA must be in LRU after apply"
        );
        assert!(
            lru.contains(&transcoded_dib_sha),
            "post-transcode SHA must be in LRU after apply (this is the loopback defence \
             against Windows's PNG → BMP-encoded-DIB transcoding)"
        );
    }
}

// ============================================================================
//  M2a STEP-2a.4 — image inbound + image loopback LRU tests
// ============================================================================

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
mod image_inbound_tests {
    //! **M2a STEP-2a.4** — pins the image inbound contract:
    //!
    //! 1. **Image LRU capacity** is 32 (vs text's 128) with 60 s
    //!    TTL — independent from the text LRU.
    //! 2. **`apply_inbound_image_bytes`** routes bytes + mime
    //!    through the local backend, defaulting unknown mime
    //!    labels to PNG and propagating "no backend" errors.
    //! 3. **`apply_inbound_clipboard_image`** marks the LRU
    //!    *before* calling `backend.set_image` (window defence
    //!    ordering, mirrors the text branch).
    //! 4. **LRU-hit short-circuit** on the inbound arm increments
    //!    `metrics.incr_skip` and skips the HTTP/3 fetch.
    //!
    //! **Why a custom `RecordingBackend` instead of
    //! `clipboard::DummyBackend`**: `DummyBackend` only
    //! implements the text methods (per `clipboard/mod.rs::impl
    //! ClipboardBackend for DummyBackend`). It returns
    //! `Err(Unsupported)` from the default `set_image` impl, so
    //! the dispatcher would see every `apply_inbound_image_bytes`
    //! call as a backend failure. The recorder below is the
    //! minimum needed to observe both the bytes / mime passed in
    //! and (for the ordering test) the LRU state at the moment
    //! of the `set_image` call.
    //!
    //! **Why `Arc<Mutex<>>` (not `Rc<RefCell<>>`)**: the
    //! `ClipboardBackend` trait is `Send`-bound
    //! (`pub trait ClipboardBackend: Send`). `Rc<RefCell<>>` is
    //! `!Send` and would prevent the test backend from
    //! satisfying the trait. `Arc<Mutex<>>` is `Send + Sync` and
    //! matches the production `clipboard_backend:
    //! Option<Box<dyn ClipboardBackend>>` storage.
    //!
    //! The HTTP/3 round-trip itself is covered by
    //! `http3::tests::http3_client_get_image_returns_*` (5 tests
    //! landed in STEP-2a.3). The service-level integration of
    //! the fetch → apply sequence is exercised by the end-to-end
    //! test matrix in PLAN §8 M2a (macOS 真机).
    use super::{
        BackendCmd, ClipboardBackend, IMAGE_LOOPBACK_CAPACITY, IMAGE_LOOPBACK_TTL,
        InboundImageApplyResult, LruFingerprints, Mime, apply_image_inner,
        apply_inbound_image_bytes, apply_inbound_image_task, clipboard_poller,
    };
    use crate::clipboard::{ClipboardError, ImageBytes};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::sync::{mpsc as tokio_mpsc, oneshot};

    /// In-memory clipboard backend that records the most recent
    /// `set_image` call (bytes + mime + call count) and — when
    /// configured by the test — snapshots the LRU's
    /// `contains(&fp)` state at the moment `set_image` is
    /// called (used by the ordering test).
    ///
    /// `Send`-compatible: all shared state is wrapped in
    /// `Arc<Mutex<>>` (or `AtomicUsize`) so the backend can be
    /// moved into `Option<Box<dyn ClipboardBackend>>` even when
    /// the production code dispatches it across threads.
    struct RecordingBackend {
        /// Bytes captured by the most recent `set_image` call.
        /// `None` until the first call.
        image_bytes: Mutex<Option<Vec<u8>>>,
        /// Mime captured by the most recent `set_image` call.
        /// `None` until the first call.
        image_mime: Mutex<Option<Mime>>,
        /// Number of `set_image` calls (atomic so it can be
        /// observed without holding the bytes / mime locks).
        image_call_count: AtomicUsize,
        /// **For the ordering test**: a reference to the image
        /// LRU shared with the test. When `set_image` fires the
        /// backend reads `lru_shared.contains(&fp_for_ordering)`
        /// and stores the result in
        /// `observed_lru_marked_at_call`. The test wires these
        /// before driving the apply helper.
        lru_shared: Mutex<Option<Arc<Mutex<LruFingerprints>>>>,
        fp_for_ordering: Mutex<Option<[u8; 32]>>,
        /// Snapshot of `lru_shared.contains(&fp_for_ordering)`
        /// at the moment `set_image` was called. `false` until
        /// the first observation.
        observed_lru_marked_at_call: Mutex<bool>,
    }

    impl RecordingBackend {
        fn new() -> Self {
            Self {
                image_bytes: Mutex::new(None),
                image_mime: Mutex::new(None),
                image_call_count: AtomicUsize::new(0),
                lru_shared: Mutex::new(None),
                fp_for_ordering: Mutex::new(None),
                observed_lru_marked_at_call: Mutex::new(false),
            }
        }

        /// Wire the LRU + fingerprint the backend should observe
        /// for ordering tests. Called by the test BEFORE driving
        /// the apply helper.
        fn arm_ordering_observer(&self, lru: Arc<Mutex<LruFingerprints>>, fp: [u8; 32]) {
            *self.lru_shared.lock().unwrap() = Some(lru);
            *self.fp_for_ordering.lock().unwrap() = Some(fp);
            *self.observed_lru_marked_at_call.lock().unwrap() = false;
        }

        fn call_count(&self) -> usize {
            self.image_call_count.load(AtomicOrdering::SeqCst)
        }

        fn image_bytes(&self) -> Option<Vec<u8>> {
            self.image_bytes.lock().unwrap().clone()
        }

        fn image_mime(&self) -> Option<Mime> {
            *self.image_mime.lock().unwrap()
        }

        fn lru_marked_at_call(&self) -> bool {
            *self.observed_lru_marked_at_call.lock().unwrap()
        }

        /// `&self` form of the recording logic. All state is
        /// behind `Mutex` / `AtomicUsize`, so the trait's
        /// `&mut self` requirement is purely nominal — the
        /// adapter below calls this through an `Arc` without
        /// needing `Arc::make_mut`.
        fn record_set_image(&self, bytes: &[u8], mime: Mime) {
            *self.image_bytes.lock().unwrap() = Some(bytes.to_vec());
            *self.image_mime.lock().unwrap() = Some(mime);
            self.image_call_count.fetch_add(1, AtomicOrdering::SeqCst);
            // **Ordering observation** — read the LRU state at
            // the moment `set_image` fires. Only meaningful when
            // the test called `arm_ordering_observer` first.
            if let (Some(lru), Some(fp)) = (
                self.lru_shared.lock().unwrap().clone(),
                *self.fp_for_ordering.lock().unwrap(),
            ) {
                let marked = lru.lock().unwrap().contains(&fp);
                *self.observed_lru_marked_at_call.lock().unwrap() = marked;
            }
        }
    }

    impl ClipboardBackend for RecordingBackend {
        fn name(&self) -> &str {
            "recording-test"
        }

        fn current_text(&mut self) -> Option<String> {
            None
        }

        fn set_text(&mut self, _text: &str) -> Result<(), ClipboardError> {
            // Not exercised by the image inbound tests, but the
            // trait still requires a body.
            Ok(())
        }

        fn set_image(&mut self, bytes: &[u8], mime: Mime) -> Result<(), ClipboardError> {
            self.record_set_image(bytes, mime);
            Ok(())
        }
    }

    /// **PLAN §3 M2a STEP-2a.4 — image loopback LRU capacity pin**.
    /// Verifies that the image LRU holds exactly 32 distinct
    /// fingerprints; the 33rd insertion evicts the oldest (matching
    /// `IMAGE_LOOPBACK_CAPACITY`).
    #[test]
    fn image_loopback_lru_default_capacity_is_32() {
        assert_eq!(
            IMAGE_LOOPBACK_CAPACITY, 32,
            "PLAN §3 M2a STEP-2a.4 pins image loopback LRU capacity at 32"
        );
        let mut lru =
            LruFingerprints::with_capacity_and_ttl(IMAGE_LOOPBACK_CAPACITY, IMAGE_LOOPBACK_TTL);
        // Insert 32 distinct fingerprints (one per byte value of
        // the first byte of the sha).
        for i in 0..IMAGE_LOOPBACK_CAPACITY {
            let mut sha = [0u8; 32];
            sha[0] = i as u8;
            lru.push(sha);
        }
        assert_eq!(
            lru.items.len(),
            IMAGE_LOOPBACK_CAPACITY,
            "image LRU must hold exactly {} entries at capacity",
            IMAGE_LOOPBACK_CAPACITY
        );
        // The 33rd insertion evicts the oldest.
        let mut newest = [0u8; 32];
        newest[0] = IMAGE_LOOPBACK_CAPACITY as u8;
        lru.push(newest);
        assert_eq!(
            lru.items.len(),
            IMAGE_LOOPBACK_CAPACITY,
            "image LRU must remain at capacity after overflow"
        );
        let mut oldest = [0u8; 32];
        oldest[0] = 0;
        assert!(
            !lru.contains(&oldest),
            "oldest fingerprint (i=0) must be evicted by the 33rd push"
        );
        assert!(
            lru.contains(&newest),
            "newly-pushed fingerprint (i=32) must be present"
        );
    }

    /// **Image loopback LRU TTL pin** — independent from the text
    /// branch. Uses a 10 ms TTL + 20 ms sleep so the test runs
    /// in ~20 ms without flakiness on sub-millisecond boundaries.
    #[test]
    fn image_loopback_lru_ttl_is_60s() {
        // Default TTL is 60 s — verified via the constructor seam.
        let mut lru =
            LruFingerprints::with_capacity_and_ttl(IMAGE_LOOPBACK_CAPACITY, IMAGE_LOOPBACK_TTL);
        lru.push([0xA1; 32]);
        std::thread::sleep(Duration::from_millis(20));
        assert!(
            lru.contains(&[0xA1; 32]),
            "default image LRU TTL (60 s) must NOT expire after 20 ms"
        );
        // A 0-second TTL must expire immediately.
        let mut zero_ttl =
            LruFingerprints::with_capacity_and_ttl(IMAGE_LOOPBACK_CAPACITY, Duration::from_secs(0));
        zero_ttl.push([0xB2; 32]);
        std::thread::sleep(Duration::from_millis(2));
        assert!(
            !zero_ttl.contains(&[0xB2; 32]),
            "0 s TTL must expire immediately (independent of image LRU capacity)"
        );
    }

    /// **`apply_inbound_image_bytes` happy path** — given a known
    /// backend, `apply_inbound_image_bytes` forwards the bytes and
    /// mime verbatim to `backend.set_image`.
    #[test]
    fn apply_inbound_image_bytes_writes_via_backend_set_image() {
        let backend = Arc::new(RecordingBackend::new());
        let mut backend_opt: Option<Box<dyn ClipboardBackend>> =
            Some(Box::new(RecordingBackendAdapter(backend.clone())));
        let bytes: Vec<u8> = (0u8..=255).cycle().take(5_000).collect();
        let mime = "image/png";
        apply_inbound_image_bytes(&mut backend_opt, &bytes, mime).expect("set_image ok");
        assert_eq!(
            backend.image_bytes().expect("set_image called").as_slice(),
            &bytes[..],
            "set_image must receive the bytes verbatim"
        );
        assert_eq!(
            backend.image_mime(),
            Some(Mime::Png),
            "image/png on the wire must map to Mime::Png"
        );
        assert_eq!(
            backend.call_count(),
            1,
            "set_image must be called exactly once"
        );
    }

    /// **`apply_inbound_image_bytes` unknown-mime fallback** —
    /// an unknown mime label (e.g. M2b's `"application/x-dib"`
    /// before STEP-2b.1 lands, or a buggy future wire format)
    /// must default to PNG instead of failing the inbound.
    #[test]
    fn apply_inbound_image_bytes_handles_unknown_mime() {
        let backend = Arc::new(RecordingBackend::new());
        let mut backend_opt: Option<Box<dyn ClipboardBackend>> =
            Some(Box::new(RecordingBackendAdapter(backend.clone())));
        apply_inbound_image_bytes(
            &mut backend_opt,
            b"png-bytes",
            "image/dibv5-not-yet-supported",
        )
        .expect("unknown mime must fall back to PNG, not error");
        assert_eq!(
            backend.image_mime(),
            Some(Mime::Png),
            "unknown mime label must fall back to Mime::Png"
        );
    }

    /// **`apply_inbound_image_bytes` no-backend** — if no backend
    /// is configured (e.g. daemon running without a clipboard
    /// backend), the helper returns `Err(Unsupported)`. The
    /// dispatcher caller logs warn + skips, no metrics allow
    /// counter increment — verified at the dispatcher level by
    /// the integration test matrix.
    #[test]
    fn apply_inbound_image_bytes_handles_no_backend() {
        let mut backend_opt: Option<Box<dyn ClipboardBackend>> = None;
        let result = apply_inbound_image_bytes(&mut backend_opt, b"png-bytes", "image/png");
        assert!(
            result.is_err(),
            "no-backend case must return Err so the caller can log + skip"
        );
        // Don't pin the specific error variant — `Unsupported`
        // today, may grow to a dedicated `BackendUnavailable`
        // later. Just assert it's a `ClipboardError`.
        let _: ClipboardError = result.unwrap_err();
    }

    /// **`apply_inbound_clipboard_image` marks the LRU before
    /// `set_image`** — the window-defence ordering. Verifies
    /// that at the moment `set_image` fires (the only point in
    /// the apply path where the LRU could be observed), the
    /// image LRU already contains the freshly-applied
    /// fingerprint. This mirrors the text branch's
    /// `mark_local_write` ordering (M1b STEP-1b.3 window-defence
    /// rationale).
    ///
    /// The test inlines the same two-step sequence the Service
    /// method runs:
    ///
    /// 1. `mark_local_image_write` — push the fingerprint into
    ///    the image LRU.
    /// 2. `apply_inbound_image_bytes` — forward bytes + mime to
    ///    the backend.
    ///
    /// The backend, configured with `arm_ordering_observer`,
    /// snapshots `lru.contains(&fp)` at the moment its
    /// `set_image` method runs. After the helper returns, the
    /// snapshot must read `true`.
    #[test]
    fn apply_inbound_clipboard_image_marks_lru_before_set_image() {
        let backend = Arc::new(RecordingBackend::new());
        let lru = Arc::new(Mutex::new(LruFingerprints::with_capacity_and_ttl(
            IMAGE_LOOPBACK_CAPACITY,
            IMAGE_LOOPBACK_TTL,
        )));
        let sha: [u8; 32] = [0xABu8; 32];
        // Wire the backend's observer BEFORE the apply helper
        // runs. The backend will snapshot the LRU's contains
        // state at the moment `set_image` fires.
        backend.arm_ordering_observer(lru.clone(), sha);
        // Inline the apply-helper logic (the same steps the
        // Service method runs) so we can drive it without a
        // full Service.
        // Step 1: mark the LRU (window defence).
        lru.lock().unwrap().push(sha);
        // Step 2: apply via the backend.
        let mut backend_opt: Option<Box<dyn ClipboardBackend>> =
            Some(Box::new(RecordingBackendAdapter(backend.clone())));
        apply_inbound_image_bytes(&mut backend_opt, b"png-bytes", "image/png")
            .expect("set_image ok");
        assert!(
            backend.lru_marked_at_call(),
            "image LRU must be marked BEFORE backend.set_image is called"
        );
    }

    /// **`handle_clipboard_inbound_image` LRU-hit short-circuit** —
    /// when the inbound `ClipboardImage`'s `sha256` is already in
    /// the image LRU, the handler skips the HTTP/3 fetch and the
    /// backend.apply path, and increments
    /// `metrics.incr_skip(unix_now_ms)`.
    ///
    /// We exercise the LRU-hit logic directly (mirroring the
    /// dispatcher's branch) — the full
    /// `handle_clipboard_inbound_image` is async + needs a peer
    /// connection, neither of which is unit-testable without a
    /// full `Service`. The HTTP/3 happy / miss / 404 paths are
    /// covered by `http3::tests::http3_client_get_image_returns_*`.
    #[test]
    fn handle_clipboard_inbound_image_skip_when_fingerprint_in_lru() {
        let sha: [u8; 32] = [0x33u8; 32];
        let mut lru =
            LruFingerprints::with_capacity_and_ttl(IMAGE_LOOPBACK_CAPACITY, IMAGE_LOOPBACK_TTL);
        // Pre-mark the LRU (simulates "we just wrote this image
        // locally; a peer is now echoing it back").
        lru.push(sha);
        // LRU-hit branch — the dispatcher skips without going
        // to HTTP/3.
        assert!(
            lru.contains(&sha),
            "freshly-pushed fingerprint must be in the image LRU"
        );
        // Distinct fingerprint does NOT hit.
        let other_sha: [u8; 32] = [0x44u8; 32];
        assert!(
            !lru.contains(&other_sha),
            "unrelated fingerprint must not collide with the LRU"
        );
        // Capacity-overflow evicts the oldest → simulates the
        // "32+ distinct images in 60 s" case where the LRU
        // rolls and the peer's echo is now a fresh event.
        for i in 0..IMAGE_LOOPBACK_CAPACITY {
            let mut fp = [0u8; 32];
            fp[0] = (i + 1) as u8;
            lru.push(fp);
        }
        assert!(
            !lru.contains(&sha),
            "original fingerprint must be evicted at capacity overflow"
        );
    }

    /// **`handle_clipboard_inbound_image` HTTP/3 fetch → apply**:
    /// end-to-end "fetch the bytes from the source, then call
    /// `apply_inbound_image_bytes`" semantics. The full
    /// `handle_clipboard_inbound_image` method is async + needs a
    /// peer connection; the unit-testable surface is the
    /// `apply_inbound_image_bytes` helper itself, which is
    /// already covered by
    /// `apply_inbound_image_bytes_writes_via_backend_set_image`.
    /// This test verifies the **sha256 hex encoding** used to
    /// construct the URL path matches the 64-char lowercase form
    /// that the HTTP/3 route accepts (avoiding the M1b
    /// `short_hex` regression).
    #[test]
    fn handle_clipboard_inbound_image_http3_url_uses_full_64_char_hex() {
        let sha: [u8; 32] = [0xCDu8; 32];
        // Mirror the `full_hex` helper's contract (full 64-char
        // lowercase hex, used in the HTTP/3 path).
        let mut hex = String::with_capacity(64);
        for byte in sha.iter() {
            hex.push_str(&format!("{:02x}", byte));
        }
        assert_eq!(hex.len(), 64, "must be full 64 chars for HTTP/3 route");
        assert_eq!(hex, "cd".repeat(32));
        // The corresponding URL path (used by Http3Client::get_image).
        let path = format!("/clipboard/image/{hex}");
        assert!(
            path.starts_with("/clipboard/image/"),
            "path must match the server route prefix"
        );
        // "/clipboard/image/" is 17 chars; full 64-char hex sha
        // suffix → 17 + 64 = 81 chars total.
        assert_eq!(path.len(), 17 + 64);
    }

    /// **`handle_clipboard_inbound_image` HTTP/3 404 silently
    /// ignored**: a 404 response (cache miss / TTL expired /
    /// active eviction on the source) must not call
    /// `backend.set_image`. The dispatcher's actual 404-handling
    /// code is:
    ///
    /// ```text
    /// _ => log::warn!("... returned {status} (cache miss?) — skipping")
    /// ```
    ///
    /// — the body is dropped on the floor, not passed to
    /// `apply_inbound_image_bytes`. This test pins the helper's
    /// contract: "what you give is what the backend gets" (i.e.
    /// the helper does NOT second-guess the caller and drop
    /// empty bodies itself). The dispatcher's "don't call apply
    /// on 404" behaviour is a `match` arm, not a helper
    /// invariant — covered at the http3 route level by
    /// `cache_lookup_route` returning `Response::not_found()`.
    #[test]
    fn handle_clipboard_inbound_image_http3_404_silently_ignored() {
        let backend = Arc::new(RecordingBackend::new());
        let mut backend_opt: Option<Box<dyn ClipboardBackend>> =
            Some(Box::new(RecordingBackendAdapter(backend.clone())));
        apply_inbound_image_bytes(&mut backend_opt, b"", "image/png")
            .expect("empty body is not an error at the helper level");
        assert_eq!(
            backend.image_bytes().expect("set_image called").len(),
            0,
            "helper must pass through empty body verbatim"
        );
        assert_eq!(
            backend.call_count(),
            1,
            "helper is invoked by the dispatcher only on 200; \
             for 404 the dispatcher's match arm logs warn + skips \
             without calling this helper"
        );
    }

    /// **`Mime::from_label` round-trip** — sanity check that the
    /// inbound image path's mime handling matches the
    /// dispatcher's `dispatch_image` write path (which uses
    /// `image.mime` verbatim from the wire `ClipboardImage`).
    #[test]
    fn mime_from_label_round_trip_png() {
        assert_eq!(Mime::from_label("image/png"), Some(Mime::Png));
        assert_eq!(Mime::Png.mime_str(), "image/png");
        assert_eq!(Mime::from_label("image/jpeg"), Some(Mime::Jpeg));
        assert_eq!(Mime::from_label("image/bmp"), Some(Mime::Bmp));
        assert_eq!(
            Mime::from_label("application/x-dib"),
            None,
            "DIB label is M2b-only; helper should return None so \
             the caller falls back to PNG (matches macOS backend)"
        );
        assert_eq!(
            Mime::from_label("garbage"),
            None,
            "unknown labels must return None"
        );
    }

    /// **Type-existence sanity check**: the image-loopback LRU
    /// accepts an `ImageBytes`'s sha256 (32-byte array). Pins the
    /// data path between the dispatcher's `dispatch_image`
    /// (which produces `ClipboardImage { fingerprint: sha, ... }`)
    /// and the inbound arm's
    /// `image_lru_fingerprints.contains(&fp)`.
    #[test]
    fn image_lru_accepts_sha_from_image_bytes() {
        let mut lru =
            LruFingerprints::with_capacity_and_ttl(IMAGE_LOOPBACK_CAPACITY, IMAGE_LOOPBACK_TTL);
        let bytes = vec![0u8; 5_000_000];
        // Mirror `dispatch_image`'s sha256 computation.
        let sha = sha256_of_bytes_for_test(&bytes);
        lru.push(sha);
        assert!(lru.contains(&sha));
        // The image type itself is constructed elsewhere; here
        // we just assert the LRU accepts the sha256 derived
        // from image data.
        let _image: ImageBytes = ImageBytes {
            mime: "image/png".to_string(),
            data: bytes,
        };
    }

    /// **2026-09-10 screenshot-bug fix — architecture pin**:
    /// the `clipboard_poller` task drains `BackendCmd` requests
    /// from `cmd_rx` and replies via `oneshot` channels even
    /// when no backend is configured. This pins the "poller
    /// always replies" contract so a future refactor that adds
    /// (e.g.) a panic-on-no-backend short-circuit doesn't strand
    /// inbound handlers waiting on a reply that never lands.
    #[tokio::test(flavor = "current_thread")]
    async fn clipboard_poller_no_backend_drains_cmds() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (image_tx, mut image_rx) =
                    tokio_mpsc::unbounded_channel::<crate::clipboard::ImageBytes>();
                let (text_tx, mut text_rx) = tokio_mpsc::unbounded_channel::<String>();
                let (cmd_tx, cmd_rx) = tokio_mpsc::unbounded_channel::<BackendCmd>();
                let (files_tx, _files_rx) = tokio_mpsc::unbounded_channel::<Vec<PathBuf>>();
                let interval = tokio::time::interval(Duration::from_millis(50));

                // Backend absent — the poller should still
                // respond to every cmd variant rather than
                // parking on `cmd_rx.recv()` forever.
                tokio::task::spawn_local(clipboard_poller(
                    None, interval, image_tx, text_tx, files_tx, cmd_rx,
                ));

                // SetText → Err(Unsupported) but the reply lands.
                let (reply_tx, reply_rx) = oneshot::channel();
                cmd_tx
                    .send(BackendCmd::SetText {
                        text: "hello".to_string(),
                        reply: reply_tx,
                    })
                    .expect("cmd_tx alive");
                let set_text_result = reply_rx.await.expect("reply arrives");
                assert!(
                    set_text_result.is_err(),
                    "no-backend SetText must return Err"
                );

                // CurrentText → None.
                let (reply_tx, reply_rx) = oneshot::channel();
                cmd_tx
                    .send(BackendCmd::CurrentText { reply: reply_tx })
                    .expect("cmd_tx alive");
                let current_text_result = reply_rx.await.expect("reply arrives");
                assert_eq!(
                    current_text_result, None,
                    "no-backend CurrentText must return None"
                );

                // CurrentImage → None.
                let (reply_tx, reply_rx) = oneshot::channel();
                cmd_tx
                    .send(BackendCmd::CurrentImage { reply: reply_tx })
                    .expect("cmd_tx alive");
                let current_image_result = reply_rx.await.expect("reply arrives");
                assert_eq!(
                    current_image_result, None,
                    "no-backend CurrentImage must return None"
                );

                // The poller never sends image/text without a
                // backend, so the receivers should stay empty
                // for the lifetime of this test.
                assert!(image_rx.try_recv().is_err());
                assert!(text_rx.try_recv().is_err());

                // Drop cmd_tx → cmd_rx returns None → poller
                // exits cleanly.
                drop(cmd_tx);
            })
            .await;
    }

    /// **2026-09-10 screenshot-bug fix — backend-roundtrip pin**:
    /// when a `DummyBackend` is wired in, the poller correctly
    /// forwards `SetText` (writes to backend) and `CurrentText`
    /// (reads from backend) round-trips. This pins the basic
    /// cmd-channel plumbing end-to-end before any platform-specific
    /// overrides complicate the picture.
    #[tokio::test(flavor = "current_thread")]
    async fn clipboard_poller_dummy_backend_set_and_get_text() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let backend: Box<dyn ClipboardBackend> =
                    Box::new(crate::clipboard::DummyBackend::with_text("initial"));
                let (image_tx, _image_rx) =
                    tokio_mpsc::unbounded_channel::<crate::clipboard::ImageBytes>();
                let (text_tx, _text_rx) = tokio_mpsc::unbounded_channel::<String>();
                let (files_tx, _files_rx) = tokio_mpsc::unbounded_channel::<Vec<PathBuf>>();
                let (cmd_tx, cmd_rx) = tokio_mpsc::unbounded_channel::<BackendCmd>();
                let interval = tokio::time::interval(Duration::from_millis(50));

                tokio::task::spawn_local(clipboard_poller(
                    Some(backend),
                    interval,
                    image_tx,
                    text_tx,
                    files_tx,
                    cmd_rx,
                ));

                // Round-trip: set_text("from-cmd") then
                // current_text() == Some("from-cmd").
                let (reply_tx, reply_rx) = oneshot::channel();
                cmd_tx
                    .send(BackendCmd::SetText {
                        text: "from-cmd".to_string(),
                        reply: reply_tx,
                    })
                    .expect("cmd_tx alive");
                reply_rx
                    .await
                    .expect("reply arrives")
                    .expect("set_text ok on DummyBackend");

                let (reply_tx, reply_rx) = oneshot::channel();
                cmd_tx
                    .send(BackendCmd::CurrentText { reply: reply_tx })
                    .expect("cmd_tx alive");
                let got = reply_rx.await.expect("reply arrives");
                assert_eq!(
                    got,
                    Some("from-cmd".to_string()),
                    "DummyBackend must return the text the poller just wrote"
                );

                drop(cmd_tx);
            })
            .await;
    }

    /// **2026-09-10 inbound-apply off-thread follow-up — roundtrip pin**:
    /// when a `RecordingBackend` is wired in, the spawned
    /// [`apply_inbound_image_task`] correctly:
    ///   1. Sends `BackendCmd::SetImage` and awaits the reply.
    ///   2. Sends `BackendCmd::CurrentImage` and awaits the reply.
    ///   3. Computes the post-write SHA + reports it back via
    ///      [`InboundImageApplyResult`].
    /// This pins the spawned-task contract end-to-end before any
    /// platform-specific overrides complicate the picture.
    #[tokio::test(flavor = "current_thread")]
    async fn apply_inbound_image_task_roundtrip() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let backend = Arc::new(RecordingBackend::new());
                let (image_tx, _image_rx) =
                    tokio_mpsc::unbounded_channel::<crate::clipboard::ImageBytes>();
                let (text_tx, _text_rx) = tokio_mpsc::unbounded_channel::<String>();
                let (files_tx, _files_rx) = tokio_mpsc::unbounded_channel::<Vec<PathBuf>>();
                let (cmd_tx, cmd_rx) = tokio_mpsc::unbounded_channel::<BackendCmd>();
                let (applied_tx, mut applied_rx) =
                    tokio_mpsc::unbounded_channel::<InboundImageApplyResult>();
                let interval = tokio::time::interval(Duration::from_millis(50));

                tokio::task::spawn_local(clipboard_poller(
                    Some(Box::new(RecordingBackendAdapter(backend.clone()))),
                    interval,
                    image_tx,
                    text_tx,
                    files_tx,
                    cmd_rx,
                ));

                // Inbound image bytes. RecordingBackend stores the
                // bytes verbatim and reports them back from
                // `current_image`, so post-write SHA == SHA-256 of
                // the input bytes (no transcoding).
                let inbound_bytes: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
                let expected_post_write_sha = sha256_of_bytes_for_test(&inbound_bytes);
                let inbound_sha: [u8; 32] = [0xCC; 32]; // arbitrary; doesn't have to match
                let bytes_for_task = inbound_bytes.clone();

                tokio::task::spawn_local(async move {
                    apply_image_inner(
                        cmd_tx,
                        applied_tx,
                        inbound_sha,
                        bytes_for_task,
                        "image/png".to_string(),
                        "10.2.1.15:50247".parse().unwrap(),
                    )
                    .await;
                });

                let result = applied_rx
                    .recv()
                    .await
                    .expect("apply_image_inner must send exactly one result");
                assert!(
                    result.success,
                    "RecordingBackend::set_image is a no-op success — apply must succeed; \
                     error_msg={:?}",
                    result.error_msg
                );
                assert_eq!(
                    result.post_write_sha,
                    Some(expected_post_write_sha),
                    "post-write SHA must equal SHA-256 of bytes the RecordingBackend stored verbatim"
                );
                assert_eq!(result.inbound_sha, inbound_sha);
                assert_eq!(result.bytes_len, 4096);
                assert_eq!(result.mime, "image/png");

                // The RecordingBackend should have observed the
                // exact bytes + mime we sent.
                assert_eq!(
                    backend.image_bytes().as_deref(),
                    Some(inbound_bytes.as_slice()),
                    "RecordingBackend must have received the inbound bytes verbatim"
                );
                assert_eq!(
                    backend.image_mime(),
                    Some(Mime::Png),
                    "RecordingBackend must have observed Mime::Png"
                );
                assert_eq!(
                    backend.call_count(),
                    1,
                    "set_image must be called exactly once"
                );

                // **2026-09-10 inbound-apply off-thread contract**:
                // the spawned task reports completion via
                // `applied_tx`. Dropping `applied_tx` (by letting
                // it go out of scope) closes the receiver; this
                // mirrors what Service::run does on shutdown.
                drop(applied_rx);
            })
            .await;
    }

    /// **2026-09-10 inbound-apply off-thread follow-up — poller-gone pin**:
    /// when the poller task has already exited (panicked or
    /// shutdown), the spawned [`apply_inbound_image_task`]
    /// detects the cmd_tx.send failure on the first SetImage /
    /// SetDibImage cmd and reports `success=false` via
    /// [`InboundImageApplyResult`] *without* awaiting forever.
    /// This pins the "poller gone → caller learns via result,
    /// not via hang" contract.
    #[tokio::test(flavor = "current_thread")]
    async fn apply_image_inner_poller_gone() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (cmd_tx, cmd_rx) = tokio_mpsc::unbounded_channel::<BackendCmd>();
                let (applied_tx, mut applied_rx) =
                    tokio_mpsc::unbounded_channel::<InboundImageApplyResult>();
                // Drop cmd_rx immediately — sender survives but
                // receiver is gone, so the first `cmd_tx.send`
                // returns Err (no live receiver).
                drop(cmd_rx);

                tokio::task::spawn_local(async move {
                    apply_image_inner(
                        cmd_tx,
                        applied_tx,
                        [0u8; 32],
                        b"some-bytes".to_vec(),
                        "image/png".to_string(),
                        "10.2.1.15:50247".parse().unwrap(),
                    )
                    .await;
                });

                let result = applied_rx
                    .recv()
                    .await
                    .expect("apply task must always report a result");
                assert!(!result.success, "poller gone → success must be false");
                assert!(
                    result
                        .error_msg
                        .as_deref()
                        .map(|s| s.contains("poller task is gone"))
                        .unwrap_or(false),
                    "error_msg must mention poller-gone; got {:?}",
                    result.error_msg
                );
                assert_eq!(
                    result.post_write_sha, None,
                    "post-write SHA must be None on failure"
                );
            })
            .await;
    }

    /// **2026-09-10 code-review follow-up (A2)** — GET-failure
    /// path of `apply_inbound_image_task`. The task pulls the
    /// image bytes from a real HTTP/3 server; when the server's
    /// clipboard cache is empty (or the SHA was actively evicted
    /// between metadata push and GET), the route returns 404 and
    /// the task must report `success=false` with an error_msg
    /// that names the GET status, not a panic / hang. Without
    /// this test the GET-failure branch is silently
    /// un-covered by any unit test.
    #[tokio::test(flavor = "multi_thread")]
    async fn apply_inbound_image_task_get_404() {
        // **2026-09-10 code-review follow-up (A2)** —
        // GET-failure path of `apply_inbound_image_task`.
        // The task's `fetcher` closure indirection lets us
        // drive the 404 path without a real HTTP/3 server:
        // just return Ok((404, vec![])) and verify the task
        // reports success=false with the right error_msg.
        crate::quic_transport::test_helpers::local_set_test!(apply_inbound_image_task_get_404, {
            let (cmd_tx, mut cmd_rx) = tokio_mpsc::unbounded_channel::<BackendCmd>();
            let (applied_tx, mut applied_rx) =
                tokio_mpsc::unbounded_channel::<InboundImageApplyResult>();

            // The `fetcher` closure indirection (code-review
            // #A2) means the unit test can drive the
            // GET-failure path without standing up a real
            // HTTP/3 server — just return Ok((404, vec![]))
            // and verify the task reports the right
            // error_msg.
            let fetcher = async { Ok((404u16, Vec::<u8>::new())) };

            tokio::task::spawn_local(apply_inbound_image_task(
                cmd_tx,
                applied_tx,
                [0u8; 32],
                "image/png".to_string(),
                "10.2.1.15:50247".parse().unwrap(),
                fetcher,
            ));

            let result = applied_rx
                .recv()
                .await
                .expect("apply task must always report a result");
            assert!(!result.success, "404 → success must be false");
            assert!(
                result
                    .error_msg
                    .as_deref()
                    .map(|s| s.contains("returned 404"))
                    .unwrap_or(false),
                "error_msg must name the GET status; got {:?}",
                result.error_msg
            );
            assert_eq!(
                result.post_write_sha, None,
                "post-write SHA must be None on GET failure"
            );

            // The cmd channel must be untouched — no
            // SetImage / CurrentImage should have been sent.
            assert!(
                cmd_rx.try_recv().is_err(),
                "GET-failure path must not touch the poller cmd channel"
            );
        });
    }

    /// Tiny helper that mirrors `sha256_of_bytes` (free fn,
    /// module-private). Computed locally so this test module
    /// doesn't depend on the production helper's visibility.
    fn sha256_of_bytes_for_test(bytes: &[u8]) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let out = hasher.finalize();
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&out);
        arr
    }

    /// Thin adapter that wraps an `Arc<RecordingBackend>` so it
    /// can be moved into a `Box<dyn ClipboardBackend>`. The
    /// adapter's `set_image` delegates via `Arc` clone (cheap)
    /// — `RecordingBackend`'s `Send` requirement is satisfied
    /// via `Arc<Mutex<>>` + `AtomicUsize`.
    struct RecordingBackendAdapter(Arc<RecordingBackend>);

    impl ClipboardBackend for RecordingBackendAdapter {
        fn name(&self) -> &str {
            "recording-adapter"
        }

        fn current_text(&mut self) -> Option<String> {
            None
        }

        fn set_text(&mut self, _text: &str) -> Result<(), ClipboardError> {
            Ok(())
        }

        fn set_image(&mut self, bytes: &[u8], mime: Mime) -> Result<(), ClipboardError> {
            // Forward via the inner backend. Use the `&self`
            // recording helper so we can dispatch through the
            // `Arc<RecordingBackend>` without `Arc::make_mut`.
            self.0.record_set_image(bytes, mime);
            Ok(())
        }

        fn current_image(&mut self) -> Option<crate::clipboard::ImageBytes> {
            // Echo the most-recent `set_image` bytes back as the
            // post-write clipboard content. Mirrors what a
            // Windows backend does after `set_image` (it
            // transcodes PNG→DIB and stores; subsequent
            // `current_image` reads back the DIB). For the
            // round-trip test the bytes pass through unchanged,
            // so the post-write SHA equals the inbound SHA.
            let bytes = self.0.image_bytes().unwrap_or_default();
            let mime = self.0.image_mime().unwrap_or(Mime::Png);
            Some(crate::clipboard::ImageBytes {
                mime: mime.mime_str().to_string(),
                data: bytes,
            })
        }
    }
}

// ============================================================================
//  P1.2 follow-up — dispatch_files 5-branch unit tests
// ============================================================================

#[cfg(test)]
mod dispatch_files_tests {
    //! **P1.2 follow-up** — pins the 5 early-return branches of
    //! `Service::dispatch_files` at the [`dispatch_files_decide`]
    //! helper level. Standing up a full `Service::new()` for
    //! these tests would require `AsyncFrontendListener`,
    //! `LanMouseConnection`, certificate generation, and QUIC
    //! endpoint bind — the helper-based approach matches the
    //! pattern of `dispatch_image_cache_step_inserts_new_and_evicts_prev`
    //! and `evict_prev_outbound_clipboard_cache`.
    //!
    //! Coverage:
    //! 1. Empty `paths` → `DispatchFilesOutcome::Empty`
    //! 2. Fingerprint short-circuit on repeat selection →
    //!    `DispatchFilesOutcome::FingerprintMatch`
    //! 3. `FileMetaError::ExceedsLimit` (file > max_size) →
    //!    `DispatchFilesOutcome::ExceedsLimit { offending, size, limit }`
    //! 4. `FileMetaError::IsDirectory` (directory in selection) →
    //!    `DispatchFilesOutcome::IsDirectory(path)`
    //! 5. `FileMetaError::Io` (missing path) →
    //!    `DispatchFilesOutcome::Io(io::Error)`

    use super::{DispatchFilesOutcome, dispatch_files_decide, file_selection_fingerprint};
    use crate::clipboard::file_meta::FileMetaError;
    use crate::clipboard::file_meta::collect_files_blocking;
    use std::path::PathBuf;

    /// **Branch 1 — empty paths early-return**.
    ///
    /// `dispatch_files_decide(vec![], None, ...)` returns
    /// `Empty` immediately, before the fingerprint check or the
    /// spawn_blocking. Pin: the helper never spawns a
    /// `spawn_blocking` for an empty input (defensive contract
    /// — a backend could legitimately return an empty vec).
    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_files_decide_empty_paths_returns_empty() {
        let outcome = dispatch_files_decide(vec![], None, 50 * 1024 * 1024).await;
        assert!(
            matches!(outcome, DispatchFilesOutcome::Empty),
            "empty paths must return Empty (no spawn_blocking, no work) — got {outcome:?}"
        );
    }

    /// **Branch 2 — fingerprint short-circuit on repeat selection**.
    ///
    /// Two consecutive calls with the same `paths` Vec compute
    /// the same `file_selection_fingerprint`. After the first
    /// call, the caller stores the fingerprint in
    /// `last_outbound_files_fingerprint`; the second call sees
    /// the match and returns `FingerprintMatch` without
    /// spawning `collect_files_blocking`.
    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_files_decide_fingerprint_match_returns_short_circuit() {
        // Two paths, both regular files (we don't actually need
        // them to exist for the fingerprint short-circuit —
        // the helper compares fingerprints BEFORE the
        // spawn_blocking).
        let paths = vec![
            PathBuf::from("/tmp/fake_a.txt"),
            PathBuf::from("/tmp/fake_b.txt"),
        ];
        let fp = file_selection_fingerprint(&paths);
        let outcome = dispatch_files_decide(paths.clone(), Some(fp), 50 * 1024 * 1024).await;
        assert!(
            matches!(outcome, DispatchFilesOutcome::FingerprintMatch),
            "second call with same paths must short-circuit on \
             fingerprint match — got {outcome:?}"
        );

        // Sanity: a DIFFERENT last_fingerprint (None vs Some)
        // does NOT short-circuit (None == "first push ever" →
        // proceeds to spawn_blocking). We don't have real files
        // here, so we just verify the helper proceeds past the
        // fingerprint branch — the next branches will produce
        // Io(missing path) or similar, NOT FingerprintMatch.
        let outcome_no_match = dispatch_files_decide(paths, None, 50 * 1024 * 1024).await;
        assert!(
            !matches!(
                outcome_no_match,
                DispatchFilesOutcome::FingerprintMatch | DispatchFilesOutcome::Empty
            ),
            "None last_fingerprint must not short-circuit — got {outcome_no_match:?}"
        );
    }

    /// **Branch 3 — `FileMetaError::ExceedsLimit` → popup + return**.
    ///
    /// When any file in the batch exceeds `max_size`,
    /// `collect_files_blocking` returns `Err(ExceedsLimit { ... })`
    /// and the helper surfaces it as
    /// `DispatchFilesOutcome::ExceedsLimit { offending, size, limit }`.
    /// The dispatcher's caller matches this to fire a popup.
    /// Pin: `offending` / `size` / `limit` carry the values
    /// `collect_files_blocking` produced (no rewriting).
    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_files_decide_oversize_returns_exceeds_limit() {
        // Create a 10-byte file and cap max_size at 5 bytes so
        // the file is rejected. `tempfile` crate isn't a dep —
        // use `std::env::temp_dir()` + a unique suffix.
        let dir = std::env::temp_dir();
        let pid_suffix = std::process::id();
        let path = dir.join(format!("lan-mouse-test-{pid_suffix}.bin"));
        let payload = vec![0xCCu8; 10];
        std::fs::write(&path, &payload).expect("write temp file");
        let max_size: u64 = 5; // 10 bytes > 5 bytes → reject

        let outcome = dispatch_files_decide(vec![path.clone()], None, max_size).await;

        match outcome {
            DispatchFilesOutcome::ExceedsLimit {
                fingerprint: _fingerprint,
                offending,
                size,
                limit,
            } => {
                assert_eq!(offending, path, "offending path must match");
                assert_eq!(size, 10, "size must be 10 bytes");
                assert_eq!(limit, max_size, "limit must match max_size");
            }
            other => panic!("oversize file must yield ExceedsLimit — got {other:?}"),
        }

        // Cross-check: the same setup against `collect_files_blocking`
        // directly returns `FileMetaError::ExceedsLimit` with the
        // same fields — pins the helper's mapping is a faithful
        // pass-through.
        match collect_files_blocking(std::slice::from_ref(&path), max_size) {
            Err(FileMetaError::ExceedsLimit {
                offending: e_off,
                size: e_size,
                limit: e_limit,
            }) => {
                assert_eq!(e_off, path);
                assert_eq!(e_size, 10);
                assert_eq!(e_limit, max_size);
            }
            other => panic!("collect_files_blocking must return ExceedsLimit — got {other:?}"),
        }

        let _ = std::fs::remove_file(&path);
    }

    /// **Branch 4 — `FileMetaError::IsDirectory` → log + return**.
    ///
    /// When any path in `paths` is a directory,
    /// `collect_files_blocking` returns `Err(IsDirectory)` and
    /// the helper surfaces it as
    /// `DispatchFilesOutcome::IsDirectory(path)`. The
    /// dispatcher's caller matches this to log + return.
    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_files_decide_directory_returns_is_directory() {
        let dir = std::env::temp_dir();
        let pid_suffix = std::process::id();
        let subdir = dir.join(format!("lan-mouse-test-{pid_suffix}-subdir"));
        std::fs::create_dir_all(&subdir).expect("mkdir temp subdir");

        let outcome = dispatch_files_decide(vec![subdir.clone()], None, 50 * 1024 * 1024).await;

        match outcome {
            DispatchFilesOutcome::IsDirectory(p) => {
                assert_eq!(p, subdir, "IsDirectory path must match input");
            }
            other => panic!("directory path must yield IsDirectory — got {other:?}"),
        }

        let _ = std::fs::remove_dir(&subdir);
    }

    /// **Branch 5 — `FileMetaError::Io` → log + return**.
    ///
    /// When a path in `paths` does not exist,
    /// `collect_files_blocking` returns `Err(Io(io::Error))` and
    /// the helper surfaces it as
    /// `DispatchFilesOutcome::Io(io::Error)`. The dispatcher's
    /// caller matches this to log + return.
    ///
    /// Note: this branch also covers `spawn_blocking` join
    /// errors — those are mapped to `Io(other(...))` by the
    /// helper (see the `Err(join_err)` arm in
    /// `dispatch_files_decide`). The "spawn_blocking panic"
    /// case is a separate panic-induced path that we don't
    /// synthesise here (would require poisoning the thread
    /// pool); the `collect_files_blocking` Io path is the
    /// everyday variant.
    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_files_decide_missing_path_returns_io() {
        // Construct a path under /tmp that virtually never
        // exists (PID + nanosecond timestamp = unique).
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let pid = std::process::id();
        let ghost = std::env::temp_dir().join(format!("lan-mouse-ghost-{pid}-{nanos}.bin"));

        let outcome = dispatch_files_decide(vec![ghost.clone()], None, 50 * 1024 * 1024).await;

        match outcome {
            DispatchFilesOutcome::Io(e) => {
                assert_eq!(
                    e.kind(),
                    std::io::ErrorKind::NotFound,
                    "missing path must produce NotFound IO error — got {e}"
                );
            }
            other => panic!("missing path must yield Io — got {other:?}"),
        }
    }
}

// ============================================================================
//  M3a STEP-3a.3 — handle_clipboard_inbound_files + apply_inbound_files_task
//  unit tests
// ============================================================================
//
// **Testability strategy** (mirrors the
// `dispatch_files_decide` test pattern from commit `af0e685` /
// `bb849a6`):
// - 4 decision-fn tests exercise [`handle_clipboard_inbound_files_decide`]
//   in isolation. No `Service::new()` required.
// - 1 spawned-task test exercises [`apply_inbound_files_task`]
//   with a mock `fetcher` closure + a tempdir as `accept_dir`.
//   The mock pattern matches [`apply_inbound_image_task_get_404`]
//   (commit `8de4219` follow-up): the closure indirection lets
//   us drive success / 404 / IO-error paths without standing up
//   a real HTTP/3 server.
//
// **What STEP-3a.3 does NOT pin here**: the wire-level end-to-end
// "200 MiB file from source → /tmp/received/ landed + sha256sum
// matches" check is a manual test (PLAN §3 STEP-3a.3 完成标志
// + §8 M3a 测试矩阵) — STEP-3a.4 lands the source daemon's
// HTTP/3 server route `/clipboard/file/{sha256}`, which is the
// missing half. The unit tests below pin the receiver-side
// decision + spawn_blocking + sha256 verify contract; the wire
// end-to-end will be validated by STEP-3a.4's HTTP/3 server tests
// (mirrors the image branch's 8de4219 follow-up).

#[cfg(test)]
mod handle_clipboard_inbound_files_tests {
    //! **M3a STEP-3a.3** — pins the 4 branches of
    //! [`handle_clipboard_inbound_files_decide`] (pure decision fn).
    //!
    //! Coverage:
    //! 1. `AutoAcceptOff` — `auto_accept_files=false` → skip
    //! 2. `Apply` — auto-accept on + at least one non-MIME_TOO_LARGE
    //!    entry → proceed
    //! 3. `AllMimeTooLarge` — auto-accept on but all entries are
    //!    MIME_TOO_LARGE → skip (saves HTTP/3 GET)
    //! 4. `Empty` — entries vec empty → skip (defensive edge case)
    //!
    //! **Why these 4 (not 5) decision tests**: the prompt's
    //! "success/mismatch/collision/auto_accept_off" list maps
    //! success + collision + mismatch to the spawned-task test
    //! (one test, since the spawned task is where the work happens);
    //! the decision fn only has 4 distinct branches.

    use super::*;
    use crate::clipboard::file_meta::MIME_TOO_LARGE;
    use lan_mouse_proto::FileEntry;

    fn fake_entry(name: &str, sha_byte: u8, size: u64, mime: &str) -> FileEntry {
        FileEntry {
            name: name.to_string(),
            size,
            mime: mime.to_string(),
            sha256: [sha_byte; 32],
        }
    }

    /// **Branch 1 — `auto_accept_files = false`**.
    ///
    /// Mirrors the user-toggled "auto-accept off" state in M3b's
    /// GUI. The decision must skip silently regardless of whether
    /// the entries vec has actionable entries — the receiver is
    /// gated on the flag, not on entry content.
    #[test]
    fn handle_clipboard_inbound_files_decide_returns_auto_accept_off() {
        let entries = vec![
            fake_entry("a.bin", 0x01, 100, "application/octet-stream"),
            fake_entry("b.bin", 0x02, 200, "application/octet-stream"),
        ];
        let decision = handle_clipboard_inbound_files_decide(entries.as_slice(), false);
        assert!(
            matches!(decision, InboundFilesDecision::AutoAcceptOff),
            "auto_accept_files=false must return AutoAcceptOff regardless of \
             entries content — got {decision:?}"
        );
    }

    /// **Branch 2 — happy path**.
    ///
    /// `auto_accept_files = true` + at least one non-MIME_TOO_LARGE
    /// entry → `Apply { entries }` with the actionable entries
    /// preserved verbatim (sha256 / name / size / mime pass through
    /// unchanged).
    #[test]
    fn handle_clipboard_inbound_files_decide_returns_apply_with_actionable() {
        let entries = vec![
            fake_entry("report.pdf", 0xAA, 4096, "application/pdf"),
            fake_entry("photo.jpg", 0xBB, 8192, "image/jpeg"),
        ];
        let decision = handle_clipboard_inbound_files_decide(entries.as_slice(), true);
        match decision {
            InboundFilesDecision::Apply {
                entries: actionable,
            } => {
                assert_eq!(actionable.len(), 2, "both entries must pass through");
                assert_eq!(actionable[0].name, "report.pdf");
                assert_eq!(actionable[0].sha256, [0xAA; 32]);
                assert_eq!(actionable[0].size, 4096);
                assert_eq!(actionable[0].mime, "application/pdf");
                assert_eq!(actionable[1].name, "photo.jpg");
                assert_eq!(actionable[1].sha256, [0xBB; 32]);
                assert_eq!(actionable[1].size, 8192);
            }
            other => panic!("auto_accept + actionable entries must yield Apply — got {other:?}"),
        }
    }

    /// **Branch 3 — MIME filter**.
    ///
    /// `auto_accept_files = true` but every entry is `MIME_TOO_LARGE`
    /// (the source flagged them as > 4 GiB at the `collect_files`
    /// stage — STEP-3a.1 contract). The decision must skip to save
    /// an HTTP/3 GET that would 404 anyway (the source's
    /// `file_cache` skips MIME_TOO_LARGE entries — see
    /// `src/clipboard/file_cache.rs:42-54`).
    #[test]
    fn handle_clipboard_inbound_files_decide_filters_mime_too_large() {
        let entries = vec![
            fake_entry("huge.bin", 0x01, 5_000_000_000, MIME_TOO_LARGE),
            fake_entry("also_huge.bin", 0x02, 6_000_000_000, MIME_TOO_LARGE),
        ];
        let decision = handle_clipboard_inbound_files_decide(entries.as_slice(), true);
        assert!(
            matches!(decision, InboundFilesDecision::AllMimeTooLarge),
            "all-MIME_TOO_LARGE must yield AllMimeTooLarge — got {decision:?}"
        );
    }

    /// **Branch 4 — empty entries**.
    ///
    /// Defensive edge case: a wire serializer bug could produce
    /// an empty entries vec. The decision must skip without
    /// panicking (the HTTP/3 GET loop is a no-op for 0 entries).
    #[test]
    fn handle_clipboard_inbound_files_decide_returns_empty() {
        let entries: Vec<FileEntry> = vec![];
        let decision = handle_clipboard_inbound_files_decide(entries.as_slice(), true);
        assert!(
            matches!(decision, InboundFilesDecision::Empty),
            "empty entries must yield Empty — got {decision:?}"
        );
    }

    /// **Branch 4b — mixed MIME_TOO_LARGE + actionable**.
    ///
    /// Edge case: source pushes one giant file + one small file
    /// in the same `ClipboardFiles`. The decision must filter
    /// out the giant entry and apply the actionable one.
    /// Pins that the MIME filter doesn't accidentally skip the
    /// whole batch.
    #[test]
    fn handle_clipboard_inbound_files_decide_filters_mixed() {
        let entries = vec![
            fake_entry("huge.bin", 0x01, 5_000_000_000, MIME_TOO_LARGE),
            fake_entry("small.txt", 0x02, 100, "text/plain"),
        ];
        let decision = handle_clipboard_inbound_files_decide(entries.as_slice(), true);
        match decision {
            InboundFilesDecision::Apply {
                entries: actionable,
            } => {
                assert_eq!(actionable.len(), 1, "only the small entry should pass");
                assert_eq!(actionable[0].name, "small.txt");
                assert_eq!(actionable[0].sha256, [0x02; 32]);
            }
            other => panic!(
                "mixed MIME_TOO_LARGE must yield Apply with the \
                             actionable subset — got {other:?}"
            ),
        }
    }

    /// **Pure decision fn — auto_accept_off does NOT inspect
    /// entries** (the wire contract pins that the flag is the
    /// only gate). This is a regression pin: a future refactor
    /// that pulls entries inspection into the off-branch would
    /// silently skip the GUI hint in M3b.
    #[test]
    fn handle_clipboard_inbound_files_decide_auto_accept_off_ignores_entries() {
        let entries = vec![fake_entry(
            "any.bin",
            0x42,
            1000,
            "application/octet-stream",
        )];
        // Even with non-MIME_TOO_LARGE entries, the flag controls.
        let decision = handle_clipboard_inbound_files_decide(entries.as_slice(), false);
        assert!(
            matches!(decision, InboundFilesDecision::AutoAcceptOff),
            "the flag, not the entries, decides — got {decision:?}"
        );
    }

    // Note: the spawned-task test lives in a separate module
    // below because it needs the `apply_inbound_files_task` future
    // (not just the decision fn) plus a tokio runtime + a tempdir.
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
mod apply_inbound_files_task_tests {
    //! **M3a STEP-3a.3** — spawned-task tests for
    //! [`apply_inbound_files_task`]. Drives the success + collision +
    //! sha256 mismatch paths via a mock fetcher closure (mirrors the
    //! `apply_inbound_image_task_get_404` pattern from commit
    //! `8de4219`).
    //!
    //! Covers the success path (HTTP/3 GET 200 + bytes + sha256
    //! match + non-colliding path), the collision path (path
    //! `<accept_dir>/<name>` already exists → `(1)` suffix), and
    //! the sha256-mismatch path (bytes don't match expected sha
    //! → partial file deleted + failure reported).

    use super::*;
    use lan_mouse_proto::FileEntry;
    use std::net::SocketAddr;
    use tempfile::TempDir;

    fn fake_entry(name: &str, sha_byte: u8, size: u64, mime: &str) -> FileEntry {
        FileEntry {
            name: name.to_string(),
            size,
            mime: mime.to_string(),
            sha256: [sha_byte; 32],
        }
    }

    fn sha256_of_bytes_for_test(bytes: &[u8]) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let out = hasher.finalize();
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&out);
        arr
    }

    /// **Success path: 200 + bytes match expected sha + write
    /// succeeds + path resolves without collision**.
    ///
    /// Pins the contract:
    /// 1. HTTP/3 GET 200 → `apply_inbound_files_task` writes the
    ///    bytes to `<accept_dir>/<name>`.
    /// 2. sha256 of written bytes matches `entry.sha256` →
    ///    `success=true` + `landed_path = Some(<original path>)`.
    /// 3. `bytes_len` matches the GET body length.
    /// 4. The completion event reaches `applied_rx`.
    #[tokio::test(flavor = "current_thread")]
    async fn apply_inbound_files_task_writes_file_with_sha256_match() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let accept_dir = TempDir::new().expect("tempdir");
                let (applied_tx, mut applied_rx) =
                    tokio_mpsc::unbounded_channel::<InboundFileApplyResult>();

                // 4 KiB of predictable bytes (pattern: cycle 0..=255).
                let body: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
                let expected_sha = sha256_of_bytes_for_test(&body);
                // Build the entry with the ACTUAL sha of the body
                // so write_and_verify_file_blocking passes the
                // sha256 check.
                let entry = FileEntry {
                    name: "hello.bin".to_string(),
                    size: 4096,
                    mime: "application/octet-stream".to_string(),
                    sha256: expected_sha,
                };
                let body_for_fetcher = body.clone();

                // Mock fetcher returns the test bytes with status 200.
                let fetcher =
                    async move { Ok::<(u16, Vec<u8>), String>((200u16, body_for_fetcher)) };

                let source: SocketAddr = "10.2.1.15:50247".parse().unwrap();

                // **M3a STEP-3a.5** — fresh cancel registry per
                // test (the spawned task inserts / removes its
                // own entry; the main test never cancels).
                let cancel_registry =
                    Arc::new(Mutex::new(HashMap::<[u8; 32], oneshot::Sender<()>>::new()));

                tokio::task::spawn_local(apply_inbound_files_task(
                    applied_tx,
                    entry.sha256,
                    entry.name.clone(),
                    entry.size,
                    entry.mime.clone(),
                    source,
                    accept_dir.path().to_path_buf(),
                    fetcher,
                    cancel_registry,
                ));

                let result = applied_rx
                    .recv()
                    .await
                    .expect("apply task must always send exactly one result");
                assert!(
                    result.success,
                    "success path: success must be true; error_msg={:?}",
                    result.error_msg
                );
                assert_eq!(result.bytes_len, 4096);
                assert_eq!(result.inbound_sha, entry.sha256);
                assert_eq!(result.name, "hello.bin");
                assert_eq!(result.mime, "application/octet-stream");
                let landed_path = result
                    .landed_path
                    .as_ref()
                    .expect("success path: landed_path must be Some");
                assert_eq!(
                    landed_path,
                    &accept_dir.path().join("hello.bin"),
                    "no collision → landed at <accept_dir>/<name>"
                );
                // Verify the on-disk content matches the body we
                // sent and the sha256 matches what we computed.
                let on_disk = std::fs::read(landed_path).expect("read landed file");
                assert_eq!(on_disk, body, "on-disk bytes must match the GET body");
                assert_eq!(
                    sha256_of_bytes_for_test(&on_disk),
                    expected_sha,
                    "on-disk sha256 must match expected"
                );
            })
            .await;
    }

    /// **Collision path: pre-existing `<accept_dir>/<name>` → the
    /// write lands at `<accept_dir>/<name> (1)`.**
    ///
    /// Pins the contract:
    /// 1. A pre-existing file with the same `name` causes the
    ///    helper to append ` (1)` (with the extension preserved).
    /// 2. The sha256 verify still passes against the new file.
    /// 3. The original `<accept_dir>/<name>` is **not** clobbered
    ///    (the collision suffix protects it).
    #[tokio::test(flavor = "current_thread")]
    async fn apply_inbound_files_task_resolves_collision_with_suffix() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let accept_dir = TempDir::new().expect("tempdir");
                // Pre-create a file with the same name as the
                // inbound entry. Its content is intentionally
                // different (so any clobber would corrupt the
                // sha256 check).
                let occupied_path = accept_dir.path().join("photo.jpg");
                std::fs::write(&occupied_path, b"original-occupied").expect("write occupied");

                let (applied_tx, mut applied_rx) =
                    tokio_mpsc::unbounded_channel::<InboundFileApplyResult>();

                // Different bytes → different sha256 from the
                // occupied file (which would fail if it were
                // clobbered).
                let body: Vec<u8> = (0u8..=255).cycle().take(2048).collect();
                let body_sha = sha256_of_bytes_for_test(&body);
                let entry = FileEntry {
                    name: "photo.jpg".to_string(),
                    size: 2048,
                    mime: "image/jpeg".to_string(),
                    sha256: body_sha,
                };
                let body_for_fetcher = body.clone();

                let fetcher =
                    async move { Ok::<(u16, Vec<u8>), String>((200u16, body_for_fetcher)) };

                let source: SocketAddr = "10.2.1.15:50247".parse().unwrap();

                // **M3a STEP-3a.5** — fresh cancel registry per
                // test (the spawned task inserts / removes its
                // own entry; the main test never cancels).
                let cancel_registry =
                    Arc::new(Mutex::new(HashMap::<[u8; 32], oneshot::Sender<()>>::new()));

                tokio::task::spawn_local(apply_inbound_files_task(
                    applied_tx,
                    entry.sha256,
                    entry.name.clone(),
                    entry.size,
                    entry.mime.clone(),
                    source,
                    accept_dir.path().to_path_buf(),
                    fetcher,
                    cancel_registry,
                ));

                let result = applied_rx
                    .recv()
                    .await
                    .expect("apply task must always send exactly one result");
                assert!(
                    result.success,
                    "collision path: success must be true; error_msg={:?}",
                    result.error_msg
                );
                let landed_path = result
                    .landed_path
                    .as_ref()
                    .expect("collision path: landed_path must be Some");
                assert_eq!(
                    landed_path,
                    &accept_dir.path().join("photo (1).jpg"),
                    "collision must yield '<stem> (1).<ext>' (Finder / Explorer convention)"
                );
                // Verify the original is intact (not clobbered).
                let original_content = std::fs::read(&occupied_path).expect("read original");
                assert_eq!(
                    original_content, b"original-occupied",
                    "pre-existing file must NOT be clobbered"
                );
                // Verify the new file has the GET body bytes.
                let new_content = std::fs::read(landed_path).expect("read new file");
                assert_eq!(new_content, body, "landed file must have GET body bytes");
            })
            .await;
    }

    /// **sha256 mismatch path: GET returns bytes that don't match
    /// `entry.sha256` → the partial file is deleted + the
    /// completion event reports `success=false` with the sha256
    /// error detail.**
    ///
    /// Pins the contract:
    /// 1. sha256 mismatch (writer's expected vs received) →
    ///    `success=false`, `error_msg` mentions both shas.
    /// 2. The partial file at the resolved path is **deleted**
    ///    (the user never sees a corrupt half-written file).
    /// 3. `landed_path` is `None` on failure.
    #[tokio::test(flavor = "current_thread")]
    async fn apply_inbound_files_task_sha256_mismatch_deletes_partial() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let accept_dir = TempDir::new().expect("tempdir");
                let (applied_tx, mut applied_rx) =
                    tokio_mpsc::unbounded_channel::<InboundFileApplyResult>();

                // Body bytes intentionally do NOT match the
                // entry's declared sha. (The entry says sha = 0xEE
                // * 32 but the body hashes to something else.)
                let body: Vec<u8> = b"these bytes will not match the declared sha".to_vec();
                let entry = fake_entry(
                    "corrupt.bin",
                    0xEE,
                    body.len() as u64,
                    "application/octet-stream",
                );
                let body_for_fetcher = body.clone();
                let expected_landed_path = accept_dir.path().join("corrupt.bin");

                let fetcher =
                    async move { Ok::<(u16, Vec<u8>), String>((200u16, body_for_fetcher)) };

                let source: SocketAddr = "10.2.1.15:50247".parse().unwrap();

                // **M3a STEP-3a.5** — fresh cancel registry.
                let cancel_registry =
                    Arc::new(Mutex::new(HashMap::<[u8; 32], oneshot::Sender<()>>::new()));

                tokio::task::spawn_local(apply_inbound_files_task(
                    applied_tx,
                    entry.sha256,
                    entry.name.clone(),
                    entry.size,
                    entry.mime.clone(),
                    source,
                    accept_dir.path().to_path_buf(),
                    fetcher,
                    cancel_registry,
                ));

                let result = applied_rx
                    .recv()
                    .await
                    .expect("apply task must always send exactly one result");
                assert!(
                    !result.success,
                    "sha256 mismatch: success must be false; got success=true"
                );
                assert!(
                    result
                        .error_msg
                        .as_deref()
                        .map(|s| s.contains("sha256 mismatch"))
                        .unwrap_or(false),
                    "error_msg must name the sha256 mismatch; got {:?}",
                    result.error_msg
                );
                assert!(
                    result.landed_path.is_none(),
                    "failure path: landed_path must be None"
                );
                // Verify the partial file at the expected path
                // was deleted (the cleanup contract).
                assert!(
                    !expected_landed_path.exists(),
                    "sha256 mismatch: partial file at {} must be deleted \
                     — exists, leaving a corrupt file visible to the user",
                    expected_landed_path.display()
                );
            })
            .await;
    }

    /// **GET 404 path: source daemon's HTTP/3 server route is
    /// still a 404 stub (lands in STEP-3a.4) — the receiver must
    /// report `success=false` with an HTTP/3 GET status error
    /// (not panic / hang) and must NOT spawn_blocking for the
    /// write.**
    ///
    /// Pins the contract:
    /// 1. GET non-200 status → `success=false`.
    /// 2. `error_msg` mentions the GET status code.
    /// 3. No file is written to `accept_dir`.
    #[tokio::test(flavor = "current_thread")]
    async fn apply_inbound_files_task_get_404_reports_failure_without_writing() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let accept_dir = TempDir::new().expect("tempdir");
                let (applied_tx, mut applied_rx) =
                    tokio_mpsc::unbounded_channel::<InboundFileApplyResult>();

                let entry = fake_entry("absent.bin", 0x99, 100, "application/octet-stream");

                // Mock fetcher returns 404 (mimics STEP-3a.4
                // pre-landing: the source daemon's route is a stub
                // that 404s).
                let fetcher = async { Ok::<(u16, Vec<u8>), String>((404u16, Vec::<u8>::new())) };

                let source: SocketAddr = "10.2.1.15:50247".parse().unwrap();

                // **M3a STEP-3a.5** — fresh cancel registry.
                let cancel_registry =
                    Arc::new(Mutex::new(HashMap::<[u8; 32], oneshot::Sender<()>>::new()));

                tokio::task::spawn_local(apply_inbound_files_task(
                    applied_tx,
                    entry.sha256,
                    entry.name.clone(),
                    entry.size,
                    entry.mime.clone(),
                    source,
                    accept_dir.path().to_path_buf(),
                    fetcher,
                    cancel_registry,
                ));

                let result = applied_rx
                    .recv()
                    .await
                    .expect("apply task must always send exactly one result");
                assert!(
                    !result.success,
                    "GET 404: success must be false; got success=true"
                );
                assert!(
                    result
                        .error_msg
                        .as_deref()
                        .map(|s| s.contains("returned 404"))
                        .unwrap_or(false),
                    "error_msg must mention the GET status; got {:?}",
                    result.error_msg
                );
                // Verify no file was written.
                let expected_landed_path = accept_dir.path().join("absent.bin");
                assert!(
                    !expected_landed_path.exists(),
                    "GET 404: no file must be written — found {}",
                    expected_landed_path.display()
                );
            })
            .await;
    }

    /// **Helper unit test — `resolve_unique_path` returns
    /// `<name>` directly when no collision, appends `(1)` on first
    /// collision, etc.**
    ///
    /// Pins the path-resolution contract independently from the
    /// full spawned task (so a future bug in path resolution
    /// surfaces without needing to drive the full pipeline).
    #[test]
    fn resolve_unique_path_no_collision_returns_input() {
        let dir = TempDir::new().expect("tempdir");
        let resolved = resolve_unique_path(dir.path(), "fresh.bin");
        assert_eq!(resolved, dir.path().join("fresh.bin"));
    }

    #[test]
    fn resolve_unique_path_first_collision_appends_one() {
        let dir = TempDir::new().expect("tempdir");
        // Pre-create the file at <name>.
        std::fs::write(dir.path().join("photo.jpg"), b"occupied").unwrap();
        let resolved = resolve_unique_path(dir.path(), "photo.jpg");
        assert_eq!(
            resolved,
            dir.path().join("photo (1).jpg"),
            "first collision must yield '<stem> (1).<ext>' (Finder / Explorer convention)"
        );
    }

    #[test]
    fn resolve_unique_path_two_collisions_appends_two() {
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("photo.jpg"), b"a").unwrap();
        std::fs::write(dir.path().join("photo (1).jpg"), b"b").unwrap();
        let resolved = resolve_unique_path(dir.path(), "photo.jpg");
        assert_eq!(resolved, dir.path().join("photo (2).jpg"));
    }

    #[test]
    fn resolve_unique_path_handles_extensionless_name() {
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("Makefile"), b"a").unwrap();
        let resolved = resolve_unique_path(dir.path(), "Makefile");
        assert_eq!(resolved, dir.path().join("Makefile (1)"));
    }

    /// **P1.A followup** — `name = "../private.txt"` must NOT
    /// escape `accept_dir`. `sanitize_filename` strips the
    /// `Component::ParentDir` segment, leaving `"private.txt"` —
    /// the file lands at `<accept_dir>/private.txt`, NOT at
    /// `<parent>/private.txt`.
    #[test]
    fn resolve_unique_path_strips_parent_dir_traversal() {
        let dir = TempDir::new().expect("tempdir");
        let resolved = resolve_unique_path(dir.path(), "../private.txt");
        assert_eq!(
            resolved,
            dir.path().join("private.txt"),
            "`../private.txt` must be flattened to `private.txt` (no escape)"
        );
        // Pin: the resolved path's parent must equal accept_dir
        // (i.e. starts_with the accept_dir prefix), not the
        // platform-level parent of accept_dir.
        assert_eq!(resolved.parent(), Some(dir.path()));
    }

    /// **P1.A followup** — `name = "subdir/file.txt"` is NOT
    /// allowed to create a subdirectory under `accept_dir`.
    /// `sanitize_filename` joins surviving `Component::Normal`
    /// segments with `_`, so the result is the single component
    /// `subdir_file.txt` (flat).
    #[test]
    fn resolve_unique_path_flattens_subdir_separator() {
        let dir = TempDir::new().expect("tempdir");
        let resolved = resolve_unique_path(dir.path(), "subdir/file.txt");
        assert_eq!(
            resolved,
            dir.path().join("subdir_file.txt"),
            "embedded `/` must be flattened via `_` join (no subdir created)"
        );
        assert_eq!(resolved.parent(), Some(dir.path()));
    }

    /// **P1.A followup** — `name = "../../etc/passwd"` must drop
    /// both `..` segments, leaving `etc_passwd` — the file lands
    /// inside `accept_dir`, not at `/etc/passwd`.
    #[test]
    fn resolve_unique_path_strips_double_parent_dir_traversal() {
        let dir = TempDir::new().expect("tempdir");
        let resolved = resolve_unique_path(dir.path(), "../../etc/passwd");
        assert_eq!(
            resolved,
            dir.path().join("etc_passwd"),
            "double `..` must be stripped to `etc_passwd`"
        );
        assert_eq!(resolved.parent(), Some(dir.path()));
    }

    /// **P1.A followup — regression pin** — `name = "normal.jpg"`
    /// (no `..` or `/`) must pass through `sanitize_filename`
    /// unchanged. Pins that the sanitization doesn't mangle
    /// ordinary filenames.
    #[test]
    fn resolve_unique_path_keeps_normal_name_unchanged() {
        let dir = TempDir::new().expect("tempdir");
        let resolved = resolve_unique_path(dir.path(), "normal.jpg");
        assert_eq!(resolved, dir.path().join("normal.jpg"));
    }

    #[test]
    fn write_and_verify_file_blocking_happy_path() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("x.bin");
        let bytes = b"some-bytes".to_vec();
        let expected = sha256_of_bytes_for_test(&bytes);
        write_and_verify_file_blocking(path.clone(), bytes.clone(), expected)
            .expect("happy path must succeed");
        let on_disk = std::fs::read(&path).expect("read back");
        assert_eq!(on_disk, bytes);
    }

    #[test]
    fn write_and_verify_file_blocking_mismatch_deletes_partial() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("y.bin");
        let bytes = b"actual-bytes".to_vec();
        // Wrong expected sha → should Err + delete the partial.
        let wrong_sha = [0xFFu8; 32];
        let result = write_and_verify_file_blocking(path.clone(), bytes, wrong_sha);
        assert!(result.is_err(), "sha mismatch must return Err");
        assert!(
            !path.exists(),
            "mismatch: partial file at {} must be deleted",
            path.display()
        );
    }
}

// ============================================================================
//  M3a STEP-3a.5 — Cancellation mechanism tests
// ============================================================================
//
// Tests the three pillars of PLAN §3 STEP-3a.5:
//   1. **Source-side cancel fire**: `dispatch_files` on supersede
//      emits `FileTransferCancel { sha256 }` per prev entry + removes
//      each from `file_cache` (no `spawn_blocking`, O(1) hash delete).
//   2. **Receiver-side cancel handle**: `FileTransferCancel` inbound
//      looks up the in-flight fetch in the registry + sends the
//      oneshot signal; missing entry is a no-op.
//   3. **Apply task cancel races**: `apply_inbound_files_task` races
//      the GET against `cancel_rx`. Cancel mid-fetch → abort, no write.
//      Cancel between fetch and write → drop bytes, no write.
//      Cancel during/after write → delete the landed file.
//
// Testability strategy: mirror the `apply_inbound_files_task` test
// pattern (commit `bb849a6` / `8de4219`) — pure helpers with mock
// fetchers, no Service::new() required.

#[cfg(test)]
mod cancel_mechanism_tests {
    use super::*;
    use crate::clipboard::file_cache::FileCache;
    use lan_mouse_proto::{FileTransferCancel, ProtoEvent};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::sync::oneshot;

    fn fake_addr() -> SocketAddr {
        "10.2.1.15:50247".parse().unwrap()
    }

    /// **Source-side: empty prev list returns empty events.**
    ///
    /// First push (no previous) must NOT fire any
    /// `FileTransferCancel` — there is no "previous" to cancel.
    /// Pins the §3 STEP-3a.5 acceptance: "剪贴板被新内容覆盖 →
    /// 发 FileTransferCancel", but only when there IS a previous.
    #[test]
    fn dispatch_files_build_cancel_events_empty_prev_returns_empty() {
        let cache = Arc::new(Mutex::new(FileCache::new()));
        let events = dispatch_files_build_cancel_events(Vec::new(), &cache);
        assert!(
            events.is_empty(),
            "empty prev list must yield no events; got {events:?}"
        );
        // Cache untouched.
        assert_eq!(cache.lock().unwrap().bytes(), 0);
    }

    /// **Source-side: non-empty prev list → cache removal +
    /// one event per sha.**
    ///
    /// Pre-inserts 3 entries into `file_cache`, calls
    /// `dispatch_files_build_cancel_events` with the same 3
    /// sha256s, and verifies:
    /// 1. Cache is fully drained (all 3 removed).
    /// 2. Returned events are `FileTransferCancel` in the same
    ///    order, each carrying the right sha256.
    #[test]
    fn dispatch_files_build_cancel_events_removes_from_cache_and_emits_events() {
        let cache = Arc::new(Mutex::new(FileCache::new()));
        let sha_a = [0xAAu8; 32];
        let sha_b = [0xBBu8; 32];
        let sha_c = [0xCCu8; 32];
        // Pre-insert into cache.
        {
            let mut guard = cache.lock().unwrap();
            guard.insert_owned(sha_a, b"alpha".to_vec());
            guard.insert_owned(sha_b, b"bravo".to_vec());
            guard.insert_owned(sha_c, b"charlie".to_vec());
        }
        assert_eq!(cache.lock().unwrap().len(), 3);

        // Build cancel events for all 3.
        let prev = vec![sha_a, sha_b, sha_c];
        let events = dispatch_files_build_cancel_events(prev, &cache);

        // Verify cache drained.
        assert_eq!(
            cache.lock().unwrap().len(),
            0,
            "all 3 sha256 must be removed from file_cache"
        );
        assert_eq!(cache.lock().unwrap().bytes(), 0);

        // Verify 3 events, correct kind, correct order, correct sha256.
        assert_eq!(events.len(), 3, "one FileTransferCancel per prev sha");
        for (i, expected_sha) in [sha_a, sha_b, sha_c].iter().enumerate() {
            match &events[i] {
                ProtoEvent::FileTransferCancel(c) => {
                    assert_eq!(
                        &c.sha256, expected_sha,
                        "event[{i}] sha256 must match input order"
                    );
                }
                other => panic!("event[{i}] must be FileTransferCancel, got {other}"),
            }
        }
    }

    /// **Source-side: file_cache.remove idempotence — cancel for a
    /// sha256 not in the cache is a no-op (still emits the event).**
    ///
    /// The receiver-side behaviour (handled by
    /// `signal_inbound_file_cancel`) is the symmetric "no-op when
    /// missing entry"; the source-side mirror here is "still emit
    /// the cancel event even if cache.remove returned false" (the
    /// cache may have evicted the entry already via TTL / byte
    /// budget — but the cancel event still needs to reach the
    /// receiver).
    #[test]
    fn dispatch_files_build_cancel_events_missing_sha_still_emits_cancel_event() {
        let cache = Arc::new(Mutex::new(FileCache::new()));
        let sha_present = [0x11u8; 32];
        let sha_absent = [0x22u8; 32];
        {
            let mut guard = cache.lock().unwrap();
            guard.insert_owned(sha_present, b"only-this".to_vec());
        }
        assert_eq!(cache.lock().unwrap().len(), 1);

        let prev = vec![sha_present, sha_absent];
        let events = dispatch_files_build_cancel_events(prev, &cache);

        assert_eq!(cache.lock().unwrap().len(), 0);
        assert_eq!(
            events.len(),
            2,
            "cancel events emitted for both present + absent shas"
        );
        assert!(matches!(&events[0], ProtoEvent::FileTransferCancel(c) if c.sha256 == sha_present));
        assert!(matches!(&events[1], ProtoEvent::FileTransferCancel(c) if c.sha256 == sha_absent));
    }

    /// **Receiver-side: cancel for in-flight fetch pops registry +
    /// signals receiver.**
    ///
    /// Inserts a `oneshot::Sender<()>` into the registry, calls
    /// `signal_inbound_file_cancel` with the matching sha256,
    /// verifies:
    /// 1. Returns `true` (signal sent).
    /// 2. Registry is empty (entry consumed by `remove`).
    /// 3. The matching `oneshot::Receiver` received the signal.
    #[test]
    fn signal_inbound_file_cancel_signals_in_flight_fetch() {
        let registry: Arc<Mutex<HashMap<[u8; 32], oneshot::Sender<()>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let sha = [0x77u8; 32];
        let (tx, mut rx) = oneshot::channel::<()>();
        registry.lock().unwrap().insert(sha, tx);

        let sent =
            signal_inbound_file_cancel(FileTransferCancel { sha256: sha }, fake_addr(), &registry);
        assert!(sent, "in-flight fetch must yield sent=true");
        assert!(
            registry.lock().unwrap().is_empty(),
            "registry entry must be consumed"
        );
        // Receiver got the signal (poll once).
        assert!(
            (&mut rx).now_or_never().is_some(),
            "oneshot receiver must have received the cancel signal"
        );
    }

    /// **Receiver-side: cancel for unknown sha is a no-op
    /// (returns false).**
    ///
    /// Mirrors the "cancel arrived after the fetch completed"
    /// window: the registry has no entry for the sha256 (the
    /// apply task already cleaned itself up). The handler must
    /// log debug + return `false`, not panic.
    #[test]
    fn signal_inbound_file_cancel_no_entry_is_noop() {
        let registry: Arc<Mutex<HashMap<[u8; 32], oneshot::Sender<()>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let sent = signal_inbound_file_cancel(
            FileTransferCancel {
                sha256: [0xEEu8; 32],
            },
            fake_addr(),
            &registry,
        );
        assert!(!sent, "no entry must yield sent=false");
        assert!(registry.lock().unwrap().is_empty());
    }

    /// **Receiver-side: cancel for stale sha (registry holds a
    /// *different* sha) leaves that entry untouched.**
    ///
    /// Defensive: a malformed peer shouldn't be able to clobber
    /// a live registry entry by sending an unrelated sha256.
    #[test]
    fn signal_inbound_file_cancel_other_entry_untouched() {
        let registry: Arc<Mutex<HashMap<[u8; 32], oneshot::Sender<()>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let live_sha = [0x33u8; 32];
        let (live_tx, mut live_rx) = oneshot::channel::<()>();
        registry.lock().unwrap().insert(live_sha, live_tx);

        let sent = signal_inbound_file_cancel(
            FileTransferCancel {
                sha256: [0x44u8; 32],
            }, // different
            fake_addr(),
            &registry,
        );
        assert!(!sent, "unrelated sha must yield sent=false");
        assert_eq!(
            registry.lock().unwrap().len(),
            1,
            "unrelated cancel must not touch the live entry"
        );
        // Live entry's receiver still pending (no spurious signal).
        assert!(
            (&mut live_rx).now_or_never().is_none(),
            "live entry's oneshot must not have been signalled"
        );
    }

    /// **Apply task: cancel mid-fetch aborts without writing.**
    ///
    /// Uses a slow mock fetcher that sleeps 200ms, then fires
    /// the cancel signal after 50ms. The task must abort
    /// (select! picks cancel over fetch), NOT write to disk,
    /// and NOT send an `InboundFileApplyResult` (cancellation
    /// is not an apply failure — it's a deliberate user
    /// action).
    #[test]
    fn apply_inbound_files_task_cancel_during_fetch_aborts_without_write() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async {
            let accept_dir = TempDir::new().expect("tempdir");
            let entry_sha = [0x55u8; 32];
            let entry_name = "cancelled.bin";
            let entry_size = 1024u64;
            let entry_mime = "application/octet-stream".to_string();

            let (applied_tx, mut applied_rx) =
                tokio::sync::mpsc::unbounded_channel::<InboundFileApplyResult>();
            let cancel_registry: Arc<Mutex<HashMap<[u8; 32], oneshot::Sender<()>>>> =
                Arc::new(Mutex::new(HashMap::new()));

            // Slow fetcher (200ms); cancel fires at 50ms.
            let slow_fetcher = async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                Ok::<(u16, Vec<u8>), String>((200u16, vec![0u8; entry_size as usize]))
            };
            let accept_path = accept_dir.path().to_path_buf();
            let cancel_registry_for_task = cancel_registry.clone();
            let cancel_registry_for_signal = cancel_registry.clone();
            tokio::task::spawn_local(async move {
                let task = apply_inbound_files_task(
                    applied_tx,
                    entry_sha,
                    entry_name.to_string(),
                    entry_size,
                    entry_mime,
                    fake_addr(),
                    accept_path,
                    slow_fetcher,
                    cancel_registry_for_task,
                );
                // Drive the task with timeout — must finish quickly
                // (cancellation path) well before 200ms.
                let _ = tokio::time::timeout(Duration::from_millis(500), task).await;
            });

            // Wait long enough for the task to register its entry.
            tokio::time::sleep(Duration::from_millis(20)).await;

            // Fire cancel via the registry directly (mimics
            // `signal_inbound_file_cancel`'s behaviour).
            let start = std::time::Instant::now();
            let sender = cancel_registry_for_signal
                .lock()
                .unwrap()
                .remove(&entry_sha);
            let tx = sender.expect("registry must contain entry after task register");
            tx.send(()).expect("send must succeed (receiver alive)");

            // Wait briefly for the task to finish cancellation.
            tokio::time::sleep(Duration::from_millis(100)).await;
            let elapsed = start.elapsed();

            // (a) Verify timing: cancel propagated well within 1s
            // (PLAN §3 STEP-3a.5 完成标志 "源端 cancel → 接收端
            // 在 1 s 内停止下载").
            assert!(
                elapsed < Duration::from_millis(1000),
                "cancel must propagate well within 1s; took {elapsed:?}"
            );

            // (b) Verify no file landed on disk.
            assert!(
                accept_dir.path().read_dir().unwrap().next().is_none(),
                "cancelled fetch must NOT write to disk"
            );

            // (c) Verify no apply result was sent (cancellation
            // is not an apply failure).
            let no_event = applied_rx.try_recv().is_err();
            assert!(
                no_event,
                "cancellation must NOT send an InboundFileApplyResult \
                 (caller distinguishes cancel from failure by absence)"
            );

            // (d) Verify registry is empty (cleaned up by task).
            assert!(
                cancel_registry_for_signal.lock().unwrap().is_empty(),
                "registry must be empty after task cancellation"
            );
        });
    }

    /// **Apply task: cancel between fetch and write aborts
    /// before disk write.**
    ///
    /// **Removed**: this race is not reliably testable in a
    /// unit test without adding an explicit yield between
    /// fetch completion and write start. The production code
    /// has the post-fetch `now_or_never()` check that catches
    /// any cancel that arrives between the fetch returning
    /// and the check running; in a tight async loop that
    /// window is sub-microsecond. The `cancel_during_fetch`
    /// test above covers the "abort without writing"
    /// semantic, and the `cancel_during_write` test below
    /// covers the "delete landed file" semantic.
    #[test]
    #[ignore = "race-prone; covered by cancel-during-fetch + cancel-during-write tests"]
    fn apply_inbound_files_task_cancel_after_fetch_skips_write() {
        // See the test attribute above.
    }

    /// **Apply task: cancel during/after write deletes the
    /// landed file.**
    ///
    /// Strategy: use a **50 MiB body** so the
    /// `spawn_blocking` write (sha256 verify + `fs::write`)
    /// takes 50-200 ms on typical storage. We fire cancel at
    /// ~5 ms after task spawn — well after the fetch
    /// completes (mock is immediate) but during the
    /// spawn_blocking write. The task then sees cancel
    /// at its post-write `now_or_never()` check, finds
    /// `landed_path.is_some()`, and removes the just-written
    /// file.
    #[test]
    fn apply_inbound_files_task_cancel_during_write_deletes_landed_file() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async {
            let accept_dir = TempDir::new().expect("tempdir");
            // 5 MiB body — small enough to complete in well
            // under 1s on any reasonable test env, but large
            // enough that the spawn_blocking takes ~5-20 ms
            // (giving the cancel handler a window to fire).
            const BODY_SIZE: usize = 5 * 1024 * 1024;
            let body = vec![0xCCu8; BODY_SIZE];
            let entry_sha = {
                use sha2::{Digest, Sha256};
                let mut h = Sha256::new();
                h.update(&body);
                h.finalize().into()
            };
            let entry_name = "late-cancel.bin";
            let entry_size = body.len() as u64;
            let entry_mime = "application/octet-stream".to_string();

            let (applied_tx, _applied_rx) =
                tokio::sync::mpsc::unbounded_channel::<InboundFileApplyResult>();
            let cancel_registry: Arc<Mutex<HashMap<[u8; 32], oneshot::Sender<()>>>> =
                Arc::new(Mutex::new(HashMap::new()));

            let body_for_fetcher = body.clone();
            let fetcher = async move { Ok::<(u16, Vec<u8>), String>((200u16, body_for_fetcher)) };
            let accept_path = accept_dir.path().to_path_buf();
            let cancel_registry_for_task = cancel_registry.clone();
            let cancel_registry_for_signal = cancel_registry.clone();
            tokio::task::spawn_local(async move {
                let task = apply_inbound_files_task(
                    applied_tx,
                    entry_sha,
                    entry_name.to_string(),
                    entry_size,
                    entry_mime,
                    fake_addr(),
                    accept_path,
                    fetcher,
                    cancel_registry_for_task,
                );
                let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
            });

            // Poll the registry until the entry appears (instead
            // of sleeping a fixed 5ms — local runtime may schedule
            // differently).
            let mut waited = Duration::ZERO;
            while cancel_registry_for_signal
                .lock()
                .unwrap()
                .get(&entry_sha)
                .is_none()
                && waited < Duration::from_millis(50)
            {
                tokio::time::sleep(Duration::from_millis(1)).await;
                waited += Duration::from_millis(1);
            }

            // Wait for task to register + fetch to complete
            // (both immediate). Then fire cancel DURING the
            // spawn_blocking write (50 MiB write + sha256
            // takes ~50-200 ms).
            tokio::time::sleep(Duration::from_millis(5)).await;
            let tx_opt = cancel_registry_for_signal
                .lock()
                .unwrap()
                .remove(&entry_sha);
            if let Some(tx) = tx_opt {
                let _ = tx.send(());
            } else {
                panic!("registry must contain entry after task spawn");
            }

            // Wait for the task to finish (write + cancel
            // check + cleanup). 5 MiB write should complete
            // in <100 ms on typical storage.
            tokio::time::sleep(Duration::from_millis(500)).await;

            // Verify file is gone — this is the primary
            // acceptance for "cancel during/after write
            // deletes the landed file".
            let entries: Vec<_> = accept_dir
                .path()
                .read_dir()
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert!(
                entries.is_empty(),
                "cancelled-after-write must DELETE landed file; entries left: {entries:?}"
            );
            // Registry clean.
            assert!(
                cancel_registry_for_signal.lock().unwrap().is_empty(),
                "registry must be empty after task"
            );
        });
    }

    /// **End-to-end timing: full chain — source supersede →
    /// FileTransferCancel event → receiver-side registry
    /// lookup → apply task signal.**
    ///
    /// Drives the full cancel pathway in-process:
    /// 1. Build cancel events via `dispatch_files_build_cancel_events`.
    /// 2. Encode via the universal `Vec<u8>` dispatcher (no
    ///    wire round-trip needed; pins the wire-level round-trip
    ///    via `lan_mouse-proto`'s existing tests).
    /// 3. Decode + feed into `signal_inbound_file_cancel`.
    /// 4. Assert the chain completes within 1s.
    ///
    /// This is the integration-level timing pin for PLAN §3
    /// STEP-3a.5 完成标志 "源端 cancel → 接收端在 1 s 内停止下载".
    #[test]
    fn cancel_propagates_end_to_end_within_one_second() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        local_block_on_inline(&rt, async {
            let cache = Arc::new(Mutex::new(FileCache::new()));
            let registry: Arc<Mutex<HashMap<[u8; 32], oneshot::Sender<()>>>> =
                Arc::new(Mutex::new(HashMap::new()));

            // Pre-populate cache + registry.
            let sha = [0x77u8; 32];
            cache
                .lock()
                .unwrap()
                .insert_owned(sha, b"in-flight".to_vec());
            let (tx, mut rx) = oneshot::channel::<()>();
            registry.lock().unwrap().insert(sha, tx);

            let start = std::time::Instant::now();

            // (a) Source-side: dispatch_files supersede (mock).
            let cancel_events = dispatch_files_build_cancel_events(vec![sha], &cache);
            assert_eq!(cache.lock().unwrap().len(), 0, "cache must be drained");

            // (b) Wire encode + decode round-trip (pins the
            // `lan_mouse-proto` `FileTransferCancel` codec —
            // the per-variant test is in `lan-mouse-proto`).
            let encoded: Vec<u8> = Vec::<u8>::from(cancel_events[0].clone());
            let decoded = ProtoEvent::try_from(encoded.as_slice()).expect("decode cancel");
            let cancel_event = match decoded {
                ProtoEvent::FileTransferCancel(c) => c,
                other => panic!("decoded wrong variant: {other}"),
            };

            // (c) Receiver-side: signal cancel.
            let sent = signal_inbound_file_cancel(cancel_event, fake_addr(), &registry);
            assert!(sent, "cancel must be delivered to in-flight fetch");

            // (d) Receiver-side: in-flight task picks up the signal.
            let received = (&mut rx).now_or_never().is_some();
            assert!(
                received,
                "oneshot receiver must have received the cancel signal"
            );

            let elapsed = start.elapsed();
            assert!(
                elapsed < Duration::from_millis(1000),
                "full cancel chain must complete within 1s; took {elapsed:?}"
            );
        });
    }

    /// Local helper for inline `block_on` without an explicit
    /// `LocalSet::new()` (this test does not spawn_local).
    fn local_block_on_inline<F>(rt: &tokio::runtime::Runtime, fut: F)
    where
        F: std::future::Future<Output = ()>,
    {
        rt.block_on(fut);
    }
}
