use ashpd::{
    desktop::{
        Session,
        input_capture::{
            Activated, ActivatedBarrier, Barrier, BarrierID, Capabilities, CreateSessionOptions,
            InputCapture, Region, ReleaseOptions, Zones,
        },
    },
    enumflags2::BitFlags,
};
use async_trait::async_trait;
use futures::{FutureExt, StreamExt};
use reis::{
    ei::{self, handshake::ContextType},
    event::{Connection, DeviceCapability, EiEvent},
    tokio::EiConvertEventStream,
};
use std::{
    cell::Cell,
    collections::HashMap,
    io,
    num::NonZeroU32,
    os::unix::net::UnixStream,
    pin::Pin,
    rc::Rc,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::{
    sync::{
        Notify,
        mpsc::{self, Receiver, Sender},
        watch,
    },
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use futures_core::Stream;

use input_event::Event;

use crate::{CaptureEvent, geometry::MonitorInfo};

use super::{
    BarrierKey, Capture as LanMouseInputCapture, Position,
    error::{CaptureError, LibeiCaptureCreationError},
};

/* there is a bug in xdg-remote-desktop-portal-gnome / mutter that
 * prevents receiving further events after a session has been disabled once.
 * Therefore the session needs to be recreated when the barriers are updated */

/// events that necessitate restarting the capture session
#[derive(Clone, Debug)]
enum LibeiNotifyEvent {
    Create(BarrierKey),
    Destroy(BarrierKey),
}

#[allow(dead_code)]
pub struct LibeiInputCapture {
    input_capture: Pin<Box<InputCapture>>,
    capture_task: JoinHandle<Result<(), CaptureError>>,
    event_rx: Receiver<(BarrierKey, CaptureEvent)>,
    notify_capture: Sender<LibeiNotifyEvent>,
    notify_release: Arc<Notify>,
    cancellation_token: CancellationToken,
    terminated: bool,
    /// Sender for the latest monitor list. Seeded with the initial
    /// `Zones` snapshot inside `new()` and refreshed on every
    /// `zones_changed` event inside the capture task. The watch
    /// channel is the single source of truth — `current_monitors()`
    /// reads `borrow()` for synchronous consumers (STEP-2.5's
    /// `Capture::monitors()` impl) and `monitor_changes()` hands
    /// out a subscription receiver to the STEP-2.6 service layer.
    #[allow(dead_code)] // STEP-2.5/2.6 will consume this
    monitors_tx: watch::Sender<Vec<MonitorInfo>>,
}

/// returns (start pos, end pos), inclusive
fn pos_to_barrier(r: &Region, pos: Position) -> (i32, i32, i32, i32) {
    let (x, y) = (r.x_offset(), r.y_offset());
    let (w, h) = (r.width() as i32, r.height() as i32);
    match pos {
        Position::Left => (x, y, x, y + h - 1),
        Position::Right => (x + w, y, x + w, y + h - 1),
        Position::Top => (x, y, x + w - 1, y),
        Position::Bottom => (x, y + h, x + w - 1, y + h),
    }
}

// ===== Monitor enumeration helpers (STEP-2.4) =====
//
// Pure functions that turn the libei portal's `Zones.regions()`
// (which carries position + size per region, but no name / no EDID)
// into the OS-agnostic `geometry::MonitorInfo` list. Mirrors the
// pattern used in `macos.rs::enumerate_monitors` (STEP-2.2),
// `windows/event_thread.rs::enumerate_monitors` (STEP-2.3), and the
// layer_shell helpers above.

/// Snapshot of one portal region's enumerable fields. Captured
/// here rather than reusing ashpd's `Region` directly so the
/// `MonitorInfo` projection can be exercised by unit tests with
/// hand-built fixtures — ashpd's `Region` is a borrowed handle
/// over a zvariant tuple and isn't trivially constructible outside
/// a live portal.
#[derive(Debug, Clone)]
struct LibeiZoneInfo {
    x_offset: i32,
    y_offset: i32,
    width: u32,
    height: u32,
    /// Index within `Zones.regions()`. Used as a stable tie-breaker
    /// when two regions collapse to the same `(x, y)` (shouldn't
    /// happen in practice but belt-and-braces).
    index: usize,
}

impl LibeiZoneInfo {
    fn from_region(region: &Region, index: usize) -> Self {
        Self {
            x_offset: region.x_offset(),
            y_offset: region.y_offset(),
            width: region.width(),
            height: region.height(),
            index,
        }
    }
}

/// Compose the stable monitor id for a portal region.
///
/// libei's `Region` carries no identifier (it represents a screen
/// rectangle but doesn't expose an EDID or compositor-assigned
/// name). Per the PLAN §M2 STEP-2.4 spec we fall back to the
/// region's `(x_offset, y_offset)` so two regions with different
/// origins stay distinct. The `libei-zone:` prefix mirrors the
/// `macos:` / `windows:` / `wl-output:` namespace prefixes from
/// STEP-2.2 / STEP-2.3 / layer_shell above.
fn build_stable_id(info: &LibeiZoneInfo) -> String {
    if info.width > 0 && info.height > 0 {
        format!("libei-zone:{},{}", info.x_offset, info.y_offset)
    } else {
        // Degenerate region (transient state during zone reorg): use
        // the index as a tie-breaker so two empty regions don't
        // collide on the same id. Wire parser never sees this
        // branch in practice — `regions()` returns full geometry.
        format!(
            "libei-zone:unknown-{}-{},{}",
            info.index, info.x_offset, info.y_offset
        )
    }
}

/// libei's portal does not expose a per-region scale factor.
/// Reported scale is always 1.0 — this matches the assumption
/// documented in PLAN §M2 STEP-2.4 ("libei portal 通常不报 scale,
/// 默认 1.0"). The wrapper function exists so the call site
/// reads symmetrically with the macOS / Windows / layer_shell
/// equivalents, and a future protocol extension that does report
/// scale can slot in here without changing the public builder.
fn compute_scale() -> f64 {
    1.0
}

/// Pick the primary region. Mirrors the macOS / Windows / layer_shell
/// convention: the region whose origin is `(0, 0)` is treated as
/// primary; if no region sits at the origin, fall back to the first
/// entry so callers always have exactly one `primary = true`.
fn pick_primary(zones: &[LibeiZoneInfo]) -> usize {
    zones
        .iter()
        .position(|z| z.x_offset == 0 && z.y_offset == 0)
        .unwrap_or(0)
}

/// Convert the per-region list into the OS-agnostic `MonitorInfo`
/// list. Pure (input is fully owned) so unit tests can drive it
/// with hand-built fixtures without any portal involvement.
fn build_monitor_info_list(zones: &[LibeiZoneInfo]) -> Vec<MonitorInfo> {
    if zones.is_empty() {
        return Vec::new();
    }
    let primary_idx = pick_primary(zones);
    zones
        .iter()
        .enumerate()
        .map(|(idx, zone)| {
            let id = build_stable_id(zone);
            // libei regions have no name; derive a position-based
            // label so the wire shape is never `(empty. (frontend
            // tooltip relies on a non-empty name to show
            // "(unknown)" otherwise).
            let name = format!("Region ({}, {})", zone.x_offset, zone.y_offset);
            MonitorInfo {
                id,
                name,
                position: (zone.x_offset, zone.y_offset),
                size: (zone.width, zone.height),
                primary: idx == primary_idx,
                scale: compute_scale(),
            }
        })
        .collect()
}

/// Build a `MonitorInfo` list from a live `Zones` handle. Used both
/// for the initial publication (in `new()`) and for every
/// `zones_changed` event (in `do_capture`'s outer loop).
fn enumerate_zones(zones: &Zones) -> Vec<MonitorInfo> {
    let info_list: Vec<LibeiZoneInfo> = zones
        .regions()
        .iter()
        .enumerate()
        .map(|(i, r)| LibeiZoneInfo::from_region(r, i))
        .collect();
    build_monitor_info_list(&info_list)
}

// ===== /Monitor enumeration helpers =====

/// Ashpd does not expose fields
#[derive(Clone, Copy, Debug)]
struct ICBarrier {
    barrier_id: BarrierID,
    position: (i32, i32, i32, i32),
}

impl ICBarrier {
    fn new(barrier_id: BarrierID, position: (i32, i32, i32, i32)) -> Self {
        Self {
            barrier_id,
            position,
        }
    }
}

impl From<ICBarrier> for Barrier {
    fn from(barrier: ICBarrier) -> Self {
        Barrier::new(barrier.barrier_id, barrier.position)
    }
}

fn select_barriers(
    zones: &Zones,
    clients: &[BarrierKey],
    next_barrier_id: &mut NonZeroU32,
) -> (Vec<ICBarrier>, HashMap<BarrierID, BarrierKey>) {
    let mut key_for_barrier = HashMap::new();
    let mut barriers: Vec<ICBarrier> = vec![];

    for key in clients {
        let mut client_barriers = zones
            .regions()
            .iter()
            .map(|r| {
                let id = *next_barrier_id;
                *next_barrier_id = next_barrier_id
                    .checked_add(1)
                    .expect("barrier id out of range");
                let position = pos_to_barrier(r, key.pos);
                key_for_barrier.insert(id, key.clone());
                ICBarrier::new(id, position)
            })
            .collect();
        barriers.append(&mut client_barriers);
    }
    (barriers, key_for_barrier)
}

async fn update_barriers(
    input_capture: &InputCapture,
    session: &Session<InputCapture>,
    active_clients: &[BarrierKey],
    next_barrier_id: &mut NonZeroU32,
) -> Result<(Vec<ICBarrier>, HashMap<BarrierID, BarrierKey>), ashpd::Error> {
    let zones = input_capture
        .zones(session, Default::default())
        .await?
        .response()?;
    log::debug!("zones: {zones:?}");

    let (barriers, id_map) = select_barriers(&zones, active_clients, next_barrier_id);
    log::debug!("barriers: {barriers:?}");
    log::debug!("client for barrier id: {id_map:?}");

    let ashpd_barriers: Vec<Barrier> = barriers.iter().copied().map(|b| b.into()).collect();
    let response = input_capture
        .set_pointer_barriers(
            session,
            &ashpd_barriers,
            zones.zone_set(),
            Default::default(),
        )
        .await?;
    let response = response.response()?;
    log::debug!("{response:?}");
    Ok((barriers, id_map))
}

async fn create_session(
    input_capture: &InputCapture,
) -> std::result::Result<(Session<InputCapture>, BitFlags<Capabilities>), ashpd::Error> {
    log::debug!("creating input capture session");
    let create_session_options = CreateSessionOptions::default().set_capabilities(
        Capabilities::Keyboard | Capabilities::Pointer | Capabilities::Touchscreen,
    );
    input_capture
        .create_session(None, create_session_options)
        .await
}

async fn connect_to_eis(
    input_capture: &InputCapture,
    session: &Session<InputCapture>,
) -> Result<(ei::Context, Connection, EiConvertEventStream), CaptureError> {
    log::debug!("connect_to_eis");
    let fd = input_capture
        .connect_to_eis(session, Default::default())
        .await?;

    // create unix stream from fd
    let stream = UnixStream::from(fd);
    stream.set_nonblocking(true)?;

    // create ei context
    let context = ei::Context::new(stream)?;
    let (conn, event_stream) = context
        .handshake_tokio("de.feschber.LanMouse", ContextType::Receiver)
        .await?;

    Ok((context, conn, event_stream))
}

async fn libei_event_handler(
    mut ei_event_stream: EiConvertEventStream,
    context: ei::Context,
    event_tx: Sender<(BarrierKey, CaptureEvent)>,
    release_session: Arc<Notify>,
    current_key: Rc<Cell<Option<BarrierKey>>>,
) -> Result<(), CaptureError> {
    loop {
        let ei_event = ei_event_stream
            .next()
            .await
            .ok_or(CaptureError::EndOfStream)??;
        log::trace!("from ei: {ei_event:?}");
        let client = current_key.get();
        handle_ei_event(ei_event, client, &context, &event_tx, &release_session).await?;
    }
}

impl LibeiInputCapture {
    pub async fn new() -> std::result::Result<Self, LibeiCaptureCreationError> {
        let input_capture = Box::pin(InputCapture::new().await?);
        let input_capture_ptr = input_capture.as_ref().get_ref() as *const InputCapture;
        let first_session = Some(create_session(unsafe { &*input_capture_ptr }).await?);

        let (event_tx, event_rx) = mpsc::channel(1);
        let (notify_capture, notify_rx) = mpsc::channel(1);
        let notify_release = Arc::new(Notify::new());

        let cancellation_token = CancellationToken::new();

        // M2 STEP-2.4: create the monitor watch channel and seed it
        // with the initial `Zones` snapshot so subscribers
        // (STEP-2.6 service layer) get a non-empty list without
        // waiting for the first zones_changed event. The watch
        // channel is the single source of truth; subsequent
        // `zones_changed` events inside `do_capture` push into the
        // same sender.
        let (monitors_tx, _) = watch::channel(Vec::new());
        if let Some((ref session, _)) = first_session {
            match fetch_zones_for_monitor(unsafe { &*input_capture_ptr }, session).await {
                Ok(zones) => {
                    let monitors = enumerate_zones(&zones);
                    log::info!("initial monitors: {} monitor(s)", monitors.len());
                    for m in &monitors {
                        log::info!(
                            "  monitor: id={} name={:?} pos={:?} size={:?} primary={} scale={}",
                            m.id,
                            m.name,
                            m.position,
                            m.size,
                            m.primary,
                            m.scale
                        );
                    }
                    let _ = monitors_tx.send(monitors);
                }
                Err(e) => {
                    log::warn!("fetch_zones_for_monitor (initial) failed: {e}");
                }
            }
        }

        let capture = do_capture(
            input_capture_ptr,
            notify_rx,
            notify_release.clone(),
            first_session,
            event_tx,
            cancellation_token.clone(),
            monitors_tx.clone(),
        );
        let capture_task = tokio::task::spawn_local(capture);

        let producer = Self {
            input_capture,
            event_rx,
            capture_task,
            notify_capture,
            notify_release,
            cancellation_token,
            terminated: false,
            monitors_tx,
        };

        Ok(producer)
    }

    /// Subscribe to the latest monitor list. Each call returns a
    /// new receiver that sees every future update (a new entry is
    /// published after every `zones_changed` event from the portal
    /// and once during construction).
    ///
    /// STEP-2.6 service layer holds the receiver and forwards
    /// `MonitorsChanged` events to the IPC frontend.
    #[allow(dead_code)] // STEP-2.5/2.6 will consume this
    pub fn monitor_changes(&self) -> watch::Receiver<Vec<MonitorInfo>> {
        self.monitors_tx.subscribe()
    }

    /// Snapshot of the most recent monitor list, captured without
    /// touching the watch channel. Used by STEP-2.5's
    /// `Capture::monitors()` impl when polling is acceptable and the
    /// caller does not need a subscription.
    #[allow(dead_code)] // STEP-2.5/2.6 will consume this
    pub fn current_monitors(&self) -> Vec<MonitorInfo> {
        self.monitors_tx.borrow().clone()
    }
}

/// Fetch zones from the portal in a side-effect-free helper. The
/// caller owns the session; this helper exists so both the initial
/// publication (in `new()`) and the per-zone_changed refresh (in
/// `do_capture`) can share the same "fetch + push" code path.
async fn fetch_zones_for_monitor(
    input_capture: &InputCapture,
    session: &Session<InputCapture>,
) -> Result<Zones, ashpd::Error> {
    Ok(input_capture
        .zones(session, Default::default())
        .await?
        .response()?)
}

/// STEP-2.4-FIXUP: gated publisher for the monitor watch channel.
///
/// Calls `fetch_monitors` only when `zones_have_changed == true`;
/// on `Ok`, publishes the resulting `Vec<MonitorInfo>` to
/// `monitors_tx`; on `Err`, logs and skips the publish so the
/// watch channel stays at its previous value (callers can still
/// see a stale list rather than a closed / empty one).
///
/// The flag MUST be read AFTER
/// `tokio::join!(capture_session, handle_session_update_request)`
/// — reading it before the join would see the value the flag held
/// at the top of the loop iteration (always `false` after the
/// reset on line ~483), so the fetch would never run and
/// `monitor_changes()` updates would stall on the initial `new()`
/// seed. See STEP-VALIDATION-M2-2.3-2.4-2.5.md §3 BUG #1.
///
/// Generic over `E` so unit tests don't need to construct an
/// `ashpd::Error` (which requires a live portal). All call sites
/// use `ashpd::Error` and benefit from `Display`.
async fn publish_monitors_if_changed<E, F, Fut>(
    zones_have_changed: bool,
    monitors_tx: &watch::Sender<Vec<MonitorInfo>>,
    fetch_monitors: F,
) where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Vec<MonitorInfo>, E>>,
    E: std::fmt::Display,
{
    if !zones_have_changed {
        return;
    }
    match fetch_monitors().await {
        Ok(monitors) => {
            log::info!(
                "monitors changed (zones_changed event): {} monitor(s)",
                monitors.len()
            );
            for m in &monitors {
                log::info!(
                    "  monitor: id={} name={:?} pos={:?} size={:?} primary={} scale={}",
                    m.id,
                    m.name,
                    m.position,
                    m.size,
                    m.primary,
                    m.scale
                );
            }
            let _ = monitors_tx.send(monitors);
        }
        Err(e) => {
            log::warn!("fetch_zones_for_monitor (zones_changed) failed: {e}");
        }
    }
}

async fn do_capture(
    input_capture: *const InputCapture,
    mut capture_event: Receiver<LibeiNotifyEvent>,
    notify_release: Arc<Notify>,
    session: Option<(Session<InputCapture>, BitFlags<Capabilities>)>,
    event_tx: Sender<(BarrierKey, CaptureEvent)>,
    cancellation_token: CancellationToken,
    monitors_tx: watch::Sender<Vec<MonitorInfo>>,
) -> Result<(), CaptureError> {
    let mut session = session.map(|s| s.0);

    /* safety: libei_task does not outlive Self */
    let input_capture = unsafe { &*input_capture };
    let mut active_clients: Vec<BarrierKey> = vec![];
    let mut next_barrier_id = NonZeroU32::new(1).expect("id must be non-zero");

    let mut zones_changed = input_capture.receive_zones_changed().await?;

    loop {
        // do capture session
        let cancel_session = CancellationToken::new();
        let cancel_update = CancellationToken::new();

        let mut capture_event_occured: Option<LibeiNotifyEvent> = None;
        let mut zones_have_changed = false;

        // kill session if clients need to be updated
        let handle_session_update_request = async {
            tokio::select! {
                _ = cancellation_token.cancelled() => {
                    log::debug!("cancelled")
                }, /* exit requested */
                _ = cancel_update.cancelled() => {
                    log::debug!("update task cancelled");
                }, /* session exited */
                _ = zones_changed.next() => {
                    log::debug!("zones changed!");
                    zones_have_changed = true
                }, /* zones have changed */
                e = capture_event.recv() => if let Some(e) = e { /* clients changed */
                    log::debug!("capture event: {e:?}");
                    capture_event_occured.replace(e);
                },
            }
            // kill session (might already be dead!)
            log::debug!("=> cancelling session");
            cancel_session.cancel();
        };

        if !active_clients.is_empty() {
            // create session
            let mut session = match session.take() {
                Some(s) => s,
                None => create_session(input_capture).await?.0,
            };

            // STEP-2.4 (FIXUP): refresh the published monitor list
            // when the portal reports a zone change. The fetch is
            // issued AFTER `tokio::join!` below — checking the flag
            // before the join reads the value it held at the top
            // of the loop iteration (always `false` after the
            // reset), so `zones_have_changed` would never reflect
            // what the future actually observed and the fetch
            // would never run. See
            // STEP-VALIDATION-M2-2.3-2.4-2.5.md §3 BUG #1.
            //
            // The fetch happens in the active branch because we
            // need a live `session` to query the portal. The idle
            // branch (when `active_clients.is_empty()`) can't
            // refresh here without keeping a session alive
            // permanently — that's an architectural change beyond
            // STEP-2.4 scope. Idle users who hot-plug monitors
            // before activating a client will see the watch
            // channel update on the next active iteration (when a
            // fresh session is created and the fetch runs again).
            // Documented in STEP-M2-2.4 §6 遗留.

            let capture_session = do_capture_session(
                input_capture,
                &mut session,
                &event_tx,
                &active_clients,
                &mut next_barrier_id,
                &notify_release,
                (cancel_session.clone(), cancel_update.clone()),
            );

            let (capture_result, ()) = tokio::join!(capture_session, handle_session_update_request);
            log::debug!("capture session + session_update task done!");

            // STEP-2.4 (FIXUP): gate-check + publish happens AFTER
            // the join so the flag reflects session-time state.
            // Gating on `zones_have_changed` (set by the future's
            // `zones_changed.next()` arm) keeps the portal DBus
            // round-trip cost at "one fetch per portal event"
            // rather than "one fetch per loop iteration" — the
            // watch channel dedups identical Vec sends, but the
            // `input_capture.zones(...)` IPC ahead of `send` is
            // not free.
            publish_monitors_if_changed(zones_have_changed, &monitors_tx, || async {
                let zones = fetch_zones_for_monitor(input_capture, &session).await?;
                Ok::<_, ashpd::Error>(enumerate_zones(&zones))
            })
            .await;

            // disable capture
            log::debug!("disabling input capture");
            if let Err(e) = input_capture.disable(&session, Default::default()).await {
                log::warn!("input_capture.disable(&session) {e}");
            }
            if let Err(e) = session.close().await {
                log::warn!("session.close(): {e}");
            }

            // propagate error from capture session
            capture_result?;
        } else {
            // Idle branch: no live session, so even if
            // `zones_have_changed` flips during the await below,
            // there's no portal to query. Refreshing the idle
            // path would require keeping a session alive
            // permanently — an architectural change beyond
            // STEP-2.4 scope. Idle users who hot-plug before
            // activating a client see the watch channel update
            // on the next active iteration (when a fresh session
            // is created and the fetch runs again). Documented
            // in STEP-M2-2.4 §6 遗留.
            handle_session_update_request.await;
        }

        // update clients if requested
        if let Some(event) = capture_event_occured.take() {
            match event {
                LibeiNotifyEvent::Create(k) => active_clients.push(k),
                LibeiNotifyEvent::Destroy(k) => active_clients.retain(|existing| existing != &k),
            }
        }

        // break
        if cancellation_token.is_cancelled() {
            break Ok(());
        }
    }
}

async fn do_capture_session(
    input_capture: &InputCapture,
    session: &mut Session<InputCapture>,
    event_tx: &Sender<(BarrierKey, CaptureEvent)>,
    active_clients: &[BarrierKey],
    next_barrier_id: &mut NonZeroU32,
    notify_release: &Notify,
    cancel: (CancellationToken, CancellationToken),
) -> Result<(), CaptureError> {
    let (cancel_session, cancel_update) = cancel;
    // current client
    let current_key = Rc::new(Cell::new(None));

    // connect to eis server
    let (context, _conn, ei_event_stream) = connect_to_eis(input_capture, session).await?;

    // set barriers
    let (barriers, key_for_barrier_id) =
        update_barriers(input_capture, session, active_clients, next_barrier_id).await?;

    log::debug!("enabling session");
    input_capture.enable(session, Default::default()).await?;

    // cancellation token to release session
    let release_session = Arc::new(Notify::new());

    // async event task
    let cancel_ei_handler = CancellationToken::new();
    let event_chan = event_tx.clone();
    let key = current_key.clone();
    let cancel_session_clone = cancel_session.clone();
    let release_session_clone = release_session.clone();
    let cancel_ei_handler_clone = cancel_ei_handler.clone();
    let ei_task = async move {
        tokio::select! {
            r = libei_event_handler(
                ei_event_stream,
                context,
                event_chan,
                release_session_clone,
                key,
            ) => {
                log::debug!("libei exited: {r:?} cancelling session task");
                cancel_session_clone.cancel();
            }
            _ = cancel_ei_handler_clone.cancelled() => {},
        }
        Ok::<(), CaptureError>(())
    };

    let capture_session_task = async {
        // receiver for activation tokens
        let mut activated = input_capture.receive_activated().await?;
        let mut ei_devices_changed = false;
        loop {
            tokio::select! {
                activated = activated.next() => {
                    let activated = activated.ok_or(CaptureError::ActivationClosed)?;
                    log::debug!("activated: {activated:?}");

                    // get barrier id from activation
                    let barrier_id = match activated.barrier_id() {
                        Some(ActivatedBarrier::Barrier(id)) => id,
                        // workaround for KDE plasma not reporting barrier ids
                        Some(ActivatedBarrier::UnknownBarrier) | None => find_corresponding_client(&barriers, activated.cursor_position().expect("no cursor position reported by compositor")),
                    };

                    // find client corresponding to barrier
                    let key = match key_for_barrier_id.get(&barrier_id) {
                        Some(k) => k.clone(),
                        None => {
                            log::warn!("INVALID BARRIER ID: Id {barrier_id} does not exist!");
                            let id = find_corresponding_client(&barriers, activated.cursor_position().expect("no cursor position reported by compositor"));
                            key_for_barrier_id.get(&id).expect("invalid barrier id").clone()
                        },
                    };
                    current_key.replace(Some(key.clone()));

                    // client entered => send event
                    event_tx.send((key.clone(), CaptureEvent::Begin)).await.expect("no channel");

                    tokio::select! {
                        _ = notify_release.notified() => { /* capture release */
                            log::debug!("release session requested");
                        },
                        _ = release_session.notified() => { /* release session */
                            log::debug!("ei devices changed");
                            ei_devices_changed = true;
                        },
                        _ = cancel_session.cancelled() => { /* kill session notify */
                            log::debug!("session cancel requested");
                            break
                        },
                    }

                    release_capture(input_capture, session, activated, &key).await?;

                }
                _ = notify_release.notified() => { /* capture release -> we are not capturing anyway, so ignore */
                    log::debug!("release session requested");
                },
                _ = release_session.notified() => { /* release session */
                    log::debug!("ei devices changed");
                    ei_devices_changed = true;
                },
                _ = cancel_session.cancelled() => { /* kill session notify */
                    log::debug!("session cancel requested");
                    break
                },
            }
            if ei_devices_changed {
                /* for whatever reason, GNOME seems to kill the session
                 * as soon as devices are added or removed, so we need
                 * to cancel */
                break;
            }
        }
        // cancel libei task
        log::debug!("session exited: killing libei task");
        cancel_ei_handler.cancel();
        Ok::<(), CaptureError>(())
    };

    let (a, b) = tokio::join!(ei_task, capture_session_task);

    cancel_update.cancel();

    log::debug!("both session and ei task finished!");
    a?;
    b?;

    Ok(())
}

async fn release_capture(
    input_capture: &InputCapture,
    session: &Session<InputCapture>,
    activated: Activated,
    current_key: &BarrierKey,
) -> Result<(), CaptureError> {
    if let Some(activation_id) = activated.activation_id() {
        log::debug!("releasing input capture {activation_id}");
    }
    let (x, y) = activated
        .cursor_position()
        .expect("compositor did not report cursor position!");
    log::debug!("client entered @ ({x}, {y})");
    let (dx, dy) = match current_key.pos {
        // offset cursor position to not enter again immediately
        Position::Left => (1., 0.),
        Position::Right => (-1., 0.),
        Position::Top => (0., 1.),
        Position::Bottom => (0., -1.),
    };
    // release 1px to the right of the entered zone
    let cursor_position = (x as f64 + dx, y as f64 + dy);
    let release_options = ReleaseOptions::default()
        .set_activation_id(activated.activation_id())
        .set_cursor_position(Some(cursor_position));
    input_capture.release(session, release_options).await?;
    Ok(())
}

fn find_corresponding_client(barriers: &[ICBarrier], pos: (f32, f32)) -> BarrierID {
    barriers
        .iter()
        .copied()
        .min_by_key(|b| {
            let (x1, y1, x2, y2) = b.position;
            let (x1, y1, x2, y2) = (x1 as f32, y1 as f32, x2 as f32, y2 as f32);
            distance_to_line(((x1, y1), (x2, y2)), pos) as i32
        })
        .expect("could not find barrier corresponding to client")
        .barrier_id
}

fn distance_to_line(line: ((f32, f32), (f32, f32)), p: (f32, f32)) -> f32 {
    let ((x1, y1), (x2, y2)) = line;
    let (x0, y0) = p;
    /*
     * we use the fact that for the triangle spanned by the line and p,
     * the height of the triangle is the desired distance and can be calculated by
     * h = 2A / b with b being the line_length and
     */
    let double_triangle_area = ((y2 - y1) * x0 - (x2 - x1) * y0 + x2 * y1 - y2 * x1).abs();
    let line_length = ((y2 - y1).powf(2.0) + (x2 - x1).powf(2.0)).sqrt();
    let distance = double_triangle_area / line_length;
    log::debug!("distance to line({line:?}, {p:?}) = {distance}");
    distance
}

async fn handle_ei_event(
    ei_event: EiEvent,
    current_client: Option<BarrierKey>,
    context: &ei::Context,
    event_tx: &Sender<(BarrierKey, CaptureEvent)>,
    release_session: &Notify,
) -> Result<(), CaptureError> {
    let all_capabilities = DeviceCapability::Pointer
        | DeviceCapability::PointerAbsolute
        | DeviceCapability::Keyboard
        | DeviceCapability::Touch
        | DeviceCapability::Scroll
        | DeviceCapability::Button;
    match ei_event {
        EiEvent::SeatAdded(s) => {
            s.seat.bind_capabilities(all_capabilities);
            context.flush().map_err(|e| io::Error::new(e.kind(), e))?;
        }
        EiEvent::SeatRemoved(_) | /* EiEvent::DeviceAdded(_) | */ EiEvent::DeviceRemoved(_) => {
            log::debug!("releasing session: {ei_event:?}");
            release_session.notify_waiters();
        }
        EiEvent::DevicePaused(_) | EiEvent::DeviceResumed(_) => {}
        EiEvent::DeviceStartEmulating(_) => log::debug!("START EMULATING"),
        EiEvent::DeviceStopEmulating(_) => log::debug!("STOP EMULATING"),
        EiEvent::Disconnected(d) => {
            return Err(CaptureError::Disconnected(format!("{:?}", d.reason)))
        }
        _ => {
            if let Some(key) = current_client {
                for event in Event::from_ei_event(ei_event) {
                    event_tx.send((key.clone(), CaptureEvent::Input(event))).await.expect("no channel");
                }
            }
        }
    }
    Ok(())
}

#[async_trait]
impl LanMouseInputCapture for LibeiInputCapture {
    async fn create(&mut self, key: &BarrierKey) -> Result<(), CaptureError> {
        // M1: backends only consume `pos`. Forward the full BarrierKey
        // through the notify channel; the capture task keeps its
        // `monitor = None / offset = 0 / span = 10000` default until
        // M2 wires monitor info end-to-end.
        let _ = self
            .notify_capture
            .send(LibeiNotifyEvent::Create(key.clone()))
            .await;
        Ok(())
    }

    async fn destroy(&mut self, key: &BarrierKey) -> Result<(), CaptureError> {
        let _ = self
            .notify_capture
            .send(LibeiNotifyEvent::Destroy(key.clone()))
            .await;
        Ok(())
    }

    async fn release(&mut self) -> Result<(), CaptureError> {
        self.notify_release.notify_waiters();
        Ok(())
    }

    async fn terminate(&mut self) -> Result<(), CaptureError> {
        self.cancellation_token.cancel();
        let task = &mut self.capture_task;
        log::debug!("waiting for capture to terminate...");
        let res = if !task.is_finished() {
            task.await.expect("libei task panic")
        } else {
            Ok(())
        };
        self.terminated = true;
        log::debug!("done!");
        res
    }

    fn monitors(&self) -> Vec<MonitorInfo> {
        self.current_monitors()
    }
}

impl Drop for LibeiInputCapture {
    fn drop(&mut self) {
        if !self.terminated {
            /* this workaround is needed until async drop is stabilized */
            panic!("LibeiInputCapture dropped without being terminated!");
        }
    }
}

impl Stream for LibeiInputCapture {
    type Item = Result<(BarrierKey, CaptureEvent), CaptureError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        match self.capture_task.poll_unpin(cx) {
            Poll::Ready(r) => match r.expect("failed to join") {
                Ok(()) => Poll::Ready(None),
                Err(e) => Poll::Ready(Some(Err(e))),
            },
            // M1: the capture task produces BarrierKey-tagged events
            // directly. monitor / offset / span stay at their legacy
            // defaults (`None` / `0` / `10000`) until M2 wires monitor
            // info end-to-end.
            Poll::Pending => self.event_rx.poll_recv(cx).map(|e| e.map(Result::Ok)),
        }
    }
}

// ===== Unit tests (M2 STEP-2.4) =====
//
// Portal-touching code (`fetch_zones_for_monitor`, the live
// `do_capture` loop) can't run on macOS CI, so we cover the pure
// helpers here. Same approach as the macOS / Windows unit tests:
// exercise the id-composition rule, the scale-default constant,
// and the `LibeiZoneInfo` → `MonitorInfo` adapter with hand-built
// fixtures.

#[cfg(test)]
mod tests {
    use super::{
        LibeiZoneInfo, build_monitor_info_list, build_stable_id, compute_scale, pick_primary,
        publish_monitors_if_changed,
    };
    use crate::geometry::MonitorInfo;

    /// Happy path: a 2x1 layout's left region at `(0, 0)` gets
    /// `libei-zone:0,0` as its stable id with the namespace prefix.
    #[test]
    fn stable_id_zero_origin_uses_position() {
        let z = LibeiZoneInfo {
            x_offset: 0,
            y_offset: 0,
            width: 1920,
            height: 1080,
            index: 0,
        };
        assert_eq!(build_stable_id(&z), "libei-zone:0,0");
    }

    /// 2x1 layout's right region: positive x offset makes the id
    /// distinct from the left.
    #[test]
    fn stable_id_positive_offset_distinct() {
        let z = LibeiZoneInfo {
            x_offset: 1920,
            y_offset: 0,
            width: 1920,
            height: 1080,
            index: 1,
        };
        assert_eq!(build_stable_id(&z), "libei-zone:1920,0");
    }

    /// Vertical pair: negative y offset (top display) and positive
    /// (bottom) yield distinct ids, mirroring the macOS / Windows
    /// negative-coordinate tests.
    #[test]
    fn stable_id_negative_y_offset_distinct() {
        let top = LibeiZoneInfo {
            x_offset: 0,
            y_offset: -1080,
            width: 1920,
            height: 1080,
            index: 0,
        };
        let bottom = LibeiZoneInfo {
            x_offset: 0,
            y_offset: 0,
            width: 1920,
            height: 1080,
            index: 1,
        };
        assert_ne!(build_stable_id(&top), build_stable_id(&bottom));
        assert_eq!(build_stable_id(&top), "libei-zone:0,-1080");
    }

    /// Degenerate region (width or height is zero): fall back to an
    /// index-tagged id so two empty regions don't collide on the
    /// same id. Belt-and-braces against a portal that emits an
    /// empty region during a zone reorg.
    #[test]
    fn stable_id_falls_back_to_index_when_zero_size() {
        let z = LibeiZoneInfo {
            x_offset: 0,
            y_offset: 0,
            width: 0,
            height: 0,
            index: 7,
        };
        assert_eq!(build_stable_id(&z), "libei-zone:unknown-7-0,0");
    }

    /// libei's portal doesn't report a per-region scale factor, so
    /// `compute_scale` always returns 1.0. Pinned here so a future
    /// protocol extension that does report scale can slot in
    /// without breaking the public contract.
    #[test]
    fn compute_scale_is_always_one() {
        assert!((compute_scale() - 1.0).abs() < 1e-9);
    }

    /// 2x1 layout: region at `(0, 0)` is primary.
    #[test]
    fn pick_primary_prefers_origin() {
        let zones = vec![
            LibeiZoneInfo {
                x_offset: 0,
                y_offset: 0,
                width: 1920,
                height: 1080,
                index: 0,
            },
            LibeiZoneInfo {
                x_offset: 1920,
                y_offset: 0,
                width: 1920,
                height: 1080,
                index: 1,
            },
        ];
        assert_eq!(pick_primary(&zones), 0);
    }

    /// No region at origin: fall back to the first entry so callers
    /// always have exactly one `primary = true`.
    #[test]
    fn pick_primary_falls_back_to_first_when_no_origin() {
        let zones = vec![
            LibeiZoneInfo {
                x_offset: 1920,
                y_offset: 0,
                width: 1920,
                height: 1080,
                index: 0,
            },
            LibeiZoneInfo {
                x_offset: 0,
                y_offset: 1080,
                width: 1920,
                height: 1080,
                index: 1,
            },
        ];
        assert_eq!(pick_primary(&zones), 0);
    }

    /// Happy path: a populated `LibeiZoneInfo` produces a
    /// `MonitorInfo` with id from position, position-derived name,
    /// position / size pass through unchanged, primary = true, scale
    /// = 1.0.
    #[test]
    fn build_monitor_info_happy_path() {
        let zones = vec![LibeiZoneInfo {
            x_offset: 0,
            y_offset: 0,
            width: 1920,
            height: 1080,
            index: 0,
        }];
        let monitors = build_monitor_info_list(&zones);
        assert_eq!(monitors.len(), 1);
        let m = &monitors[0];
        assert_eq!(m.id, "libei-zone:0,0");
        assert_eq!(m.name, "Region (0, 0)");
        assert_eq!(m.position, (0, 0));
        assert_eq!(m.size, (1920, 1080));
        assert!(m.primary);
        assert!((m.scale - 1.0).abs() < 1e-9);
    }

    /// Multiple zones: list preserves order and only the origin is
    /// primary.
    #[test]
    fn build_monitor_info_preserves_order_and_primary() {
        let zones = vec![
            LibeiZoneInfo {
                x_offset: 0,
                y_offset: 0,
                width: 1920,
                height: 1080,
                index: 0,
            },
            LibeiZoneInfo {
                x_offset: 1920,
                y_offset: 0,
                width: 2560,
                height: 1440,
                index: 1,
            },
        ];
        let monitors = build_monitor_info_list(&zones);
        assert_eq!(monitors.len(), 2);
        assert_eq!(monitors[0].position, (0, 0));
        assert!(monitors[0].primary);
        assert_eq!(monitors[1].position, (1920, 0));
        assert!(!monitors[1].primary);
    }

    /// Empty input → empty output (matches the macOS / Windows /
    /// layer_shell empty-input behavior).
    #[test]
    fn build_monitor_info_empty_input() {
        let monitors = build_monitor_info_list(&[]);
        assert!(monitors.is_empty());
    }

    /// Negative-coordinate round-trip: a vertical pair where the
    /// top display sits at `y = -1080`. Same contract as the
    /// macOS / Windows tests from STEP-2.1 / STEP-2.3.
    #[test]
    fn build_monitor_info_preserves_negative_position() {
        let zones = vec![
            LibeiZoneInfo {
                x_offset: 0,
                y_offset: 0,
                width: 1920,
                height: 1080,
                index: 0,
            },
            LibeiZoneInfo {
                x_offset: 0,
                y_offset: -1080,
                width: 1920,
                height: 1080,
                index: 1,
            },
        ];
        let monitors = build_monitor_info_list(&zones);
        assert_eq!(monitors[1].position, (0, -1080));
        assert!(!monitors[1].primary);
    }

    // ===== STEP-2.4-FIXUP regression tests =====
    //
    // The P1 fix moved the `if zones_have_changed` gate-check
    // from before `tokio::join!(...)` to after it. The gate-check
    // + publish logic was extracted into the
    // `publish_monitors_if_changed` helper so the three behavioral
    // invariants can be exercised without a live portal:
    //
    //   1. flag == false → fetch closure MUST NOT run (perf)
    //   2. flag == true && fetch Ok → `monitors_tx` MUST be
    //      notified with the new Vec
    //   3. flag == true && fetch Err → `monitors_tx` MUST NOT be
    //      notified (the watch channel stays at its previous value)
    //
    // macOS dev can't run libei code at all (`build.rs` only sets
    // `cfg(libei)` on unix & !macos); these tests live behind that
    // cfg and run on Linux CI.

    /// Invariant 1: when `zones_have_changed == false` the fetch
    /// closure is never invoked, so the no-op fast path costs zero
    /// DBus round-trips. Catches regressions where someone
    /// re-orders the gate-check back above `tokio::join!` and the
    /// flag is read in its reset state (the P1 BUG).
    #[tokio::test]
    async fn publish_monitors_if_changed_skips_fetch_when_flag_false() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (tx, rx) = tokio::sync::watch::channel(Vec::<MonitorInfo>::new());
        let fetch_called = AtomicBool::new(false);
        publish_monitors_if_changed(false, &tx, || async {
            fetch_called.store(true, Ordering::SeqCst);
            Err::<Vec<MonitorInfo>, String>("should not be called".to_string())
        })
        .await;
        assert!(
            !fetch_called.load(Ordering::SeqCst),
            "fetch closure must not run when zones_have_changed == false"
        );
        // Watch channel was never sent to, so the receiver has no
        // pending change.
        assert!(
            !rx.has_changed().unwrap(),
            "monitors_tx must not be notified when gate is closed"
        );
    }

    /// Invariant 2: when `zones_have_changed == true` and the fetch
    /// returns Ok, `monitors_tx` is notified with the new Vec.
    /// Mirrors the production path `do_capture` runs after the
    /// `tokio::join!` returns, when the future has actually polled
    /// `zones_changed.next()`.
    #[tokio::test]
    async fn publish_monitors_if_changed_publishes_on_ok() {
        let (tx, mut rx) = tokio::sync::watch::channel(Vec::<MonitorInfo>::new());
        let monitor = MonitorInfo {
            id: "libei-zone:0,0".into(),
            name: "Region (0, 0)".into(),
            position: (0, 0),
            size: (1920, 1080),
            primary: true,
            scale: 1.0,
        };
        publish_monitors_if_changed(true, &tx, || async move {
            Ok::<Vec<MonitorInfo>, String>(vec![monitor])
        })
        .await;
        assert!(
            rx.has_changed().unwrap(),
            "monitors_tx must be notified when fetch succeeds"
        );
        let monitors = rx.borrow_and_update();
        assert_eq!(monitors.len(), 1);
        assert_eq!(monitors[0].id, "libei-zone:0,0");
        assert!(monitors[0].primary);
        assert!((monitors[0].scale - 1.0).abs() < 1e-9);
    }

    /// Invariant 3: when `zones_have_changed == true` but the
    /// fetch returns Err, `monitors_tx` is NOT notified (the watch
    /// channel stays at its previous value so callers keep seeing
    /// a usable list rather than a closed / empty one).
    #[tokio::test]
    async fn publish_monitors_if_changed_does_not_publish_on_err() {
        let (tx, rx) = tokio::sync::watch::channel(Vec::<MonitorInfo>::new());
        publish_monitors_if_changed(true, &tx, || async {
            Err::<Vec<MonitorInfo>, String>("test portal error".to_string())
        })
        .await;
        assert!(
            !rx.has_changed().unwrap(),
            "monitors_tx must not be notified on fetch error"
        );
    }
}
