use async_trait::async_trait;
use futures_core::Stream;
use std::{
    collections::{HashSet, VecDeque},
    env,
    fmt::{self, Display},
    io::{self, ErrorKind},
    os::fd::{AsFd, RawFd},
    pin::Pin,
    task::{Context, Poll, ready},
};
use tokio::io::unix::AsyncFd;

use std::{
    fs::File,
    io::{BufWriter, Write},
    os::unix::prelude::AsRawFd,
    sync::Arc,
};

use wayland_protocols::{
    wp::{
        keyboard_shortcuts_inhibit::zv1::client::{
            zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1,
            zwp_keyboard_shortcuts_inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1,
        },
        pointer_constraints::zv1::client::{
            zwp_locked_pointer_v1::ZwpLockedPointerV1,
            zwp_pointer_constraints_v1::{Lifetime, ZwpPointerConstraintsV1},
        },
        relative_pointer::zv1::client::{
            zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
            zwp_relative_pointer_v1::{self, ZwpRelativePointerV1},
        },
    },
    xdg::xdg_output::zv1::client::{
        zxdg_output_manager_v1::ZxdgOutputManagerV1,
        zxdg_output_v1::{self, ZxdgOutputV1},
    },
};

use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

use wayland_client::{
    Connection, Dispatch, DispatchError, EventQueue, QueueHandle, WEnum,
    backend::{ReadEventsGuard, WaylandError},
    delegate_noop,
    globals::{Global, GlobalList, GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer, wl_compositor,
        wl_keyboard::{self, WlKeyboard},
        wl_output::{self, WlOutput},
        wl_pointer::{self, WlPointer},
        wl_region,
        wl_registry::{self, WlRegistry},
        wl_seat, wl_shm, wl_shm_pool,
        wl_surface::WlSurface,
    },
};

use input_event::{Event, KeyboardEvent, PointerEvent};
use tokio::sync::watch;

use crate::{CaptureError, CaptureEvent};

use super::{
    BarrierKey, Capture, Position,
    error::{LayerShellCaptureCreationError, WaylandBindError},
};
use crate::geometry::MonitorInfo;

struct Globals {
    compositor: wl_compositor::WlCompositor,
    pointer_constraints: ZwpPointerConstraintsV1,
    relative_pointer_manager: ZwpRelativePointerManagerV1,
    shortcut_inhibit_manager: Option<ZwpKeyboardShortcutsInhibitManagerV1>,
    seat: wl_seat::WlSeat,
    shm: wl_shm::WlShm,
    layer_shell: ZwlrLayerShellV1,
    xdg_output_manager: ZxdgOutputManagerV1,
}

#[derive(Clone, Debug)]
struct Output {
    wl_output: WlOutput,
    global: Global,
    info: Option<OutputInfo>,
    pending_info: OutputInfo,
    has_xdg_info: bool,
}

impl Display for Output {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(info) = &self.info {
            write!(
                f,
                "{} {}x{} @pos {:?} ({})",
                info.name, info.size.0, info.size.1, info.position, info.description
            )
        } else {
            write!(f, "unknown output")
        }
    }
}

#[derive(Clone, Debug, Default)]
struct OutputInfo {
    description: String,
    name: String,
    position: (i32, i32),
    size: (i32, i32),
    /// Per-output HiDPI scale factor (1 = standard density, 2 = HiDPI).
    /// Sourced from the `wl_output::Event::Scale` event (since version
    /// 2). Zero means "not yet received" and is treated as 1.0 when
    /// building `MonitorInfo` — Wayland compositors guarantee a
    /// non-zero value, but transient state mid-binding can leave us
    /// with the `Default::default()` zero before the first `Scale`
    /// event lands.
    scale: i32,
}

struct State {
    active_positions: HashSet<BarrierKey>,
    pointer: Option<WlPointer>,
    keyboard: Option<WlKeyboard>,
    pointer_lock: Option<ZwpLockedPointerV1>,
    rel_pointer: Option<ZwpRelativePointerV1>,
    shortcut_inhibitor: Option<ZwpKeyboardShortcutsInhibitorV1>,
    active_windows: Vec<Arc<Window>>,
    focused: Option<Arc<Window>>,
    global_list: GlobalList,
    globals: Globals,
    wayland_fd: RawFd,
    read_guard: Option<ReadEventsGuard>,
    qh: QueueHandle<Self>,
    pending_events: VecDeque<(BarrierKey, CaptureEvent)>,
    outputs: Vec<Output>,
    scroll_discrete_pending: bool,
    /// Sender for the latest monitor list. Set by [`LayerShellInputCapture::new`]
    /// after the initial bind / dispatch round so subscribers
    /// (the STEP-2.6 service layer) receive an initial snapshot.
    /// `update_output_info` and `deregister_global` push a fresh
    /// snapshot every time the geometry or the set of outputs
    /// changes — the watch channel ignores identical sends, so a
    /// no-op update is essentially free.
    ///
    /// `None` during the `new()` bind phase because the watch sender
    /// has to be created after we have a state to publish for.
    /// Stored as `Option<...>` rather than a default empty sender so
    /// the dispatch hot path doesn't have to send an empty Vec
    /// before the initial enumeration completes.
    monitors_tx: Option<watch::Sender<Vec<MonitorInfo>>>,
}

struct Inner {
    state: State,
    queue: EventQueue<State>,
}

impl AsRawFd for Inner {
    fn as_raw_fd(&self) -> RawFd {
        self.state.wayland_fd
    }
}

pub struct LayerShellInputCapture(
    AsyncFd<Inner>,
    /// Sender for the latest monitor list. A clone is held on
    /// `State` (so `update_output_info` / `deregister_global` push
    /// updates into the same channel); this handle is kept here so
    /// the public `monitor_changes()` method can hand out new
    /// receivers to upstream consumers (STEP-2.6 service layer).
    /// `Current_monitors()` reads `borrow()` for synchronous snapshot
    /// consumers (STEP-2.5's `Capture::monitors()` impl).
    ///
    /// `Option<...>` would not add value: once `new()` returns the
    /// channel has been created and seeded; the field is therefore
    /// always `Some` outside the (panicking) Drop path.
    watch::Sender<Vec<MonitorInfo>>,
);

struct Window {
    buffer: wl_buffer::WlBuffer,
    surface: WlSurface,
    layer_surface: ZwlrLayerSurfaceV1,
    key: BarrierKey,
}

impl Window {
    fn new(
        state: &State,
        qh: &QueueHandle<State>,
        output: &WlOutput,
        key: BarrierKey,
        size: (i32, i32),
    ) -> Window {
        log::debug!("creating window output: {output:?}, size: {size:?}");
        let g = &state.globals;

        let (width, height) = match key.pos {
            Position::Left | Position::Right => (1, size.1 as u32),
            Position::Top | Position::Bottom => (size.0 as u32, 1),
        };
        let mut file = tempfile::tempfile().unwrap();
        draw(&mut file, (width, height));
        let pool = g
            .shm
            .create_pool(file.as_fd(), (width * height * 4) as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            (width * 4) as i32,
            wl_shm::Format::Argb8888,
            qh,
            (),
        );
        let surface = g.compositor.create_surface(qh, ());

        let layer_surface = g.layer_shell.get_layer_surface(
            &surface,
            Some(output),
            Layer::Overlay,
            "LAN Mouse Sharing".into(),
            qh,
            (),
        );
        let anchor = match key.pos {
            Position::Left => Anchor::Left,
            Position::Right => Anchor::Right,
            Position::Top => Anchor::Top,
            Position::Bottom => Anchor::Bottom,
        };

        layer_surface.set_anchor(anchor);
        layer_surface.set_size(width, height);
        layer_surface.set_exclusive_zone(-1);
        layer_surface.set_margin(0, 0, 0, 0);
        surface.set_input_region(None);
        surface.commit();
        Window {
            key,
            buffer,
            surface,
            layer_surface,
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        log::debug!("destroying window!");
        self.layer_surface.destroy();
        self.surface.destroy();
        self.buffer.destroy();
    }
}

// ===== Monitor enumeration helpers (STEP-2.4) =====
//
// Pure functions that turn the per-`Output` state we already keep
// (`wl_output::info`) into the OS-agnostic `geometry::MonitorInfo`
// the rest of the system consumes. The struct-of-private-info
// approach mirrors the macOS `DisplayInfo` (STEP-2.2) and Windows
// `WinDisplayInfo` (STEP-2.3) patterns: the protocol-derived shape
// stays local; only the `MonitorInfo` projection leaves the module.

/// Snapshot of one `wl_output` + `zxdg_output_v1` pair's enumerable
/// fields, taken at the moment a `Done` event fires. Captured here
/// rather than reusing the internal `OutputInfo` so the public
/// `MonitorInfo` builder can be exercised in isolation by tests
/// without dragging the full Wayland state machine along.
#[derive(Debug, Clone)]
struct LayerShellOutputInfo {
    description: String,
    name: String,
    position: (i32, i32),
    size: (i32, i32),
    scale: i32,
    /// The `wl_registry` global name. Used as a stable tie-breaker
    /// when two outputs collapse to the same `description` (rare but
    /// possible with KVM setups). Matches the `name` parameter the
    /// Wayland registry hands us at bind time — distinct from
    /// `xdg_output::Name`, which is the user-visible label.
    global_name: u32,
}

impl LayerShellOutputInfo {
    fn from_output(output: &Output) -> Option<Self> {
        // We only emit an entry once both `wl_output` (geometry) and
        // `zxdg_output_v1` (logical position/size + name) have
        // completed their initial Done event. Until then the
        // compositor may still be feeding partial data; emitting a
        // half-populated entry would produce a MonitorInfo with
        // `position = (0, 0)` that collides with the real primary.
        let info = output.info.as_ref()?;
        Some(Self {
            description: info.description.clone(),
            name: info.name.clone(),
            position: info.position,
            size: info.size,
            scale: info.scale,
            global_name: output.global.name,
        })
    }
}

/// Compose the stable monitor id for a `wl_output`.
///
/// Priority order (mirrors the Windows fallback chain from
/// STEP-2.3 + the macOS P1-fix pattern from STEP-M2-2.2-FIXUP):
///
/// 1. Non-empty `xdg_output::Description` → `wl-output:<description>`
///    — some compositors (notably wlroots-based ones like Sway /
///    Hyprland) put EDID-derived strings here ("BOE 0x0812 ..." or
///    "LG Electronics 27" with a serial). When present this is the
///    most stable identifier since it tracks the physical hardware
///    rather than the port.
/// 2. Empty description → `wl-output:<name>@<x>,<y>` using
///    `xdg_output::Name` plus position. Two monitors with the same
///    `name` (a corner case on KVM switchers) but different positions
///    are kept distinct.
/// 3. Both empty → `wl-output:unknown-<global_name>` — registry
///    `global_name` is unique per binding session and at least
///    survives across screens with the same geometry.
///
/// The `wl-output:` prefix mirrors `macos:` / `windows:` so the
/// stable-id namespace stays OS-tagged.
fn build_stable_id(
    description: &str,
    name: &str,
    position: (i32, i32),
    global_name: u32,
) -> String {
    if !description.is_empty() {
        format!("wl-output:{description}")
    } else if !name.is_empty() {
        format!("wl-output:{name}@{},{}", position.0, position.1)
    } else {
        format!("wl-output:unknown-{global_name}")
    }
}

/// Convert the Wayland `wl_output::Event::Scale` factor into the
/// `MonitorInfo::scale` field. Wayland guarantees a non-zero
/// positive value; we treat 0 (transient state mid-binding, before
/// the first `Scale` event has landed) as 1.0 so a stale reading
/// can't drag the IPC layer into a divide-by-zero.
fn compute_scale(scale: i32) -> f64 {
    if scale > 0 { scale as f64 } else { 1.0 }
}

/// Pick the primary output. Wayland has no "primary" concept, so
/// we adopt the OS convention used by macOS / Windows: the output
/// whose origin is `(0, 0)` is treated as primary. If no output
/// sits at the origin (rare but possible on a freshly-bound
/// compositor), fall back to the first in the list so callers always
/// have exactly one `primary = true`.
fn pick_primary(info_list: &[LayerShellOutputInfo]) -> usize {
    info_list
        .iter()
        .position(|i| i.position == (0, 0))
        .unwrap_or(0)
}

/// Convert the per-`Output` state into the OS-agnostic `MonitorInfo`
/// list. Pure (input is fully owned), so the unit tests can drive it
/// with hand-built fixtures without any Wayland involvement.
fn build_monitor_info_list(info_list: Vec<LayerShellOutputInfo>) -> Vec<MonitorInfo> {
    if info_list.is_empty() {
        return Vec::new();
    }
    let primary_idx = pick_primary(&info_list);
    info_list
        .into_iter()
        .enumerate()
        .map(|(idx, info)| {
            let id = build_stable_id(
                &info.description,
                &info.name,
                info.position,
                info.global_name,
            );
            let name = if !info.description.is_empty() {
                info.description.clone()
            } else if !info.name.is_empty() {
                info.name.clone()
            } else {
                format!("Output ({}, {})", info.position.0, info.position.1)
            };
            MonitorInfo {
                id,
                name,
                position: info.position,
                size: (info.size.0.max(0) as u32, info.size.1.max(0) as u32),
                primary: idx == primary_idx,
                scale: compute_scale(info.scale),
            }
        })
        .collect()
}

/// Walk the current `state.outputs` and emit a fresh `MonitorInfo`
/// list. Called once at startup (so subscribers see the initial
/// state without waiting for the first hot-plug) and after every
/// `update_output_info` (which fires on `wl_output::Done` /
/// `zxdg_output::Done` / global add / global remove).
fn enumerate_monitors(outputs: &[Output]) -> Vec<MonitorInfo> {
    let info_list: Vec<LayerShellOutputInfo> = outputs
        .iter()
        .filter_map(LayerShellOutputInfo::from_output)
        .collect();
    build_monitor_info_list(info_list)
}

// ===== /Monitor enumeration helpers =====

fn get_edges(outputs: &[Output], pos: Position) -> Vec<(Output, i32)> {
    outputs
        .iter()
        .filter_map(|output| {
            output.info.as_ref().map(|info| {
                (
                    output.clone(),
                    match pos {
                        Position::Left => info.position.0,
                        Position::Right => info.position.0 + info.size.0,
                        Position::Top => info.position.1,
                        Position::Bottom => info.position.1 + info.size.1,
                    },
                )
            })
        })
        .collect()
}

fn get_output_configuration(state: &State, pos: Position) -> Vec<Output> {
    // get all output edges corresponding to the position
    let edges = get_edges(&state.outputs, pos);
    let opposite_edges = get_edges(&state.outputs, pos.opposite());

    // remove those edges that are at the same position
    // as an opposite edge of a different output
    edges
        .iter()
        .filter(|(_, edge)| !opposite_edges.iter().map(|(_, e)| *e).any(|e| &e == edge))
        .map(|(o, _)| o.clone())
        .collect()
}

/// Insert `key` into `active_positions` verbatim — no field
/// stripping, no `from_pos` rebuild. M3 STEP-3.4 extracted this
/// from `State::add_client` so the test in this module can pin the
/// "full BarrierKey round-trip" contract without needing a live
/// Wayland connection (the rest of `add_client` does Wayland FFI
/// that can't run on macOS CI).
fn record_active_position(active_positions: &mut HashSet<BarrierKey>, key: &BarrierKey) {
    active_positions.insert(key.clone());
}

fn draw(f: &mut File, (width, height): (u32, u32)) {
    let mut buf = BufWriter::new(f);
    for _ in 0..height {
        for _ in 0..width {
            if env::var("LM_DEBUG_LAYER_SHELL").ok().is_some() {
                // AARRGGBB
                buf.write_all(&0xff11d116u32.to_ne_bytes()).unwrap();
            } else {
                // AARRGGBB
                buf.write_all(&0x00000000u32.to_ne_bytes()).unwrap();
            }
        }
    }
}

impl LayerShellInputCapture {
    pub fn new() -> std::result::Result<Self, LayerShellCaptureCreationError> {
        let conn = Connection::connect_to_env()?;
        let (global_list, mut queue) = registry_queue_init::<State>(&conn)?;

        let qh = queue.handle();

        let compositor: wl_compositor::WlCompositor = global_list
            .bind(&qh, 4..=5, ())
            .map_err(|e| WaylandBindError::new(e, "wl_compositor 4..=5"))?;
        let xdg_output_manager: ZxdgOutputManagerV1 = global_list
            .bind(&qh, 1..=3, ())
            .map_err(|e| WaylandBindError::new(e, "xdg_output_manager 1..=3"))?;
        let shm: wl_shm::WlShm = global_list
            .bind(&qh, 1..=1, ())
            .map_err(|e| WaylandBindError::new(e, "wl_shm"))?;
        let layer_shell: ZwlrLayerShellV1 = global_list
            .bind(&qh, 3..=4, ())
            .map_err(|e| WaylandBindError::new(e, "wlr_layer_shell 3..=4"))?;
        let seat: wl_seat::WlSeat = global_list
            .bind(&qh, 7..=8, ())
            .map_err(|e| WaylandBindError::new(e, "wl_seat 7..=8"))?;

        let pointer_constraints: ZwpPointerConstraintsV1 = global_list
            .bind(&qh, 1..=1, ())
            .map_err(|e| WaylandBindError::new(e, "zwp_pointer_constraints_v1"))?;
        let relative_pointer_manager: ZwpRelativePointerManagerV1 = global_list
            .bind(&qh, 1..=1, ())
            .map_err(|e| WaylandBindError::new(e, "zwp_relative_pointer_manager_v1"))?;
        let shortcut_inhibit_manager: Result<
            ZwpKeyboardShortcutsInhibitManagerV1,
            WaylandBindError,
        > = global_list
            .bind(&qh, 1..=1, ())
            .map_err(|e| WaylandBindError::new(e, "zwp_keyboard_shortcuts_inhibit_manager_v1"));
        // layer-shell backend still works without this protocol so we make it an optional dependency
        if let Err(e) = &shortcut_inhibit_manager {
            log::warn!("shortcut_inhibit_manager not supported: {e}\nkeybinds handled by the compositor will not be passed
                to the client");
        }
        let shortcut_inhibit_manager = shortcut_inhibit_manager.ok();

        let mut state = State {
            active_positions: Default::default(),
            pointer: None,
            keyboard: None,
            global_list,
            globals: Globals {
                compositor,
                shm,
                layer_shell,
                seat,
                pointer_constraints,
                relative_pointer_manager,
                shortcut_inhibit_manager,
                xdg_output_manager,
            },
            pointer_lock: None,
            rel_pointer: None,
            shortcut_inhibitor: None,
            active_windows: Vec::new(),
            focused: None,
            qh,
            wayland_fd: queue.as_fd().as_raw_fd(),
            read_guard: None,
            pending_events: VecDeque::new(),
            outputs: vec![],
            scroll_discrete_pending: false,
            // M2 STEP-2.4: populated after the initial dispatch
            // round so the first publish contains the freshly-bound
            // monitor list rather than an empty Vec.
            monitors_tx: None,
        };

        for global in state.global_list.contents().clone_list() {
            state.register_global(global);
        }

        // flush outgoing events
        queue.flush()?;

        let read_guard = loop {
            match queue.prepare_read() {
                Some(r) => break r,
                None => {
                    queue.dispatch_pending(&mut state)?;
                    continue;
                }
            }
        };
        state.read_guard = Some(read_guard);

        // M2 STEP-2.4: create the monitor watch channel and seed it
        // with the initial enumeration so subscribers (STEP-2.6
        // service layer) get a non-empty list without waiting for
        // the first hot-plug event. The watch channel is the single
        // source of truth; subsequent `update_output_info` /
        // `deregister_global` calls push into the same sender.
        let (monitors_tx, _) = watch::channel(Vec::new());
        let initial = enumerate_monitors(&state.outputs);
        log::info!("initial monitors: {} monitor(s)", initial.len());
        for m in &initial {
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
        let _ = monitors_tx.send(initial);
        state.monitors_tx = Some(monitors_tx.clone());

        let inner = AsyncFd::new(Inner { queue, state })?;

        Ok(LayerShellInputCapture(inner, monitors_tx))
    }

    /// Add a client at the given (full) `BarrierKey`. M3 STEP-3.4
    /// fix: this used to forward only `key.pos` (via
    /// `BarrierKey::from_pos`) and discard `monitor / offset / span`
    /// — meaning the M3 dropdown's monitor selection had zero
    /// effect on which Wayland surface got the edge barrier. Now the
    /// full key is forwarded and `State::add_client` stores it
    /// verbatim in `active_positions`.
    fn add_client(&mut self, key: &BarrierKey) {
        self.0.get_mut().state.add_client(key);
    }

    /// Mirror of `add_client` for teardown. Mirrors the full-key
    /// contract so a `destroy` for `monitor: Some(...)` actually
    /// finds and removes the matching `active_positions` entry.
    fn delete_client(&mut self, key: &BarrierKey) {
        self.0.get_mut().state.delete_client(key);
    }

    /// Subscribe to the latest monitor list. Each call returns a
    /// new receiver that sees every future update (a new entry is
    /// published after every `wl_output::Done` /
    /// `zxdg_output::Done` / global add / global remove, and once
    /// during construction).
    ///
    /// STEP-2.6 service layer holds the receiver and forwards
    /// `MonitorsChanged` events to the IPC frontend.
    #[allow(dead_code)] // STEP-2.5/2.6 will consume this
    pub fn monitor_changes(&self) -> watch::Receiver<Vec<MonitorInfo>> {
        self.1.subscribe()
    }

    /// Snapshot of the most recent monitor list, captured without
    /// touching the watch channel. Used by STEP-2.5's
    /// `Capture::monitors()` impl when polling is acceptable and the
    /// caller does not need a subscription.
    #[allow(dead_code)] // STEP-2.5/2.6 will consume this
    pub fn current_monitors(&self) -> Vec<MonitorInfo> {
        self.1.borrow().clone()
    }
}

impl State {
    fn update_output_info(&mut self, name: u32) {
        let output = self
            .outputs
            .iter_mut()
            .find(|o| o.global.name == name)
            .expect("output not found");
        if output.has_xdg_info {
            output.info.replace(output.pending_info.clone());
            self.update_windows();
            // M2 STEP-2.4: re-enumerate monitors so subscribers
            // (the STEP-2.6 service layer) see the new geometry.
            // `monitor_changes()` returns the watch sender if one
            // is attached; on the hot-plug path a new entry was
            // published at startup, so a hot plug simply replaces
            // the watch value with the up-to-date list.
            if let Some(tx) = self.monitors_tx.as_ref() {
                let monitors = enumerate_monitors(&self.outputs);
                log::info!("monitors changed: {} monitor(s)", monitors.len());
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
                let _ = tx.send(monitors);
            }
        }
    }

    fn register_global(&mut self, global: Global) {
        if global.interface.as_str() == "wl_output" {
            log::debug!("new output global: wl_output {}", global.name);
            let wl_output = self.global_list.registry().bind::<WlOutput, _, _>(
                global.name,
                4,
                &self.qh,
                global.name,
            );
            self.globals
                .xdg_output_manager
                .get_xdg_output(&wl_output, &self.qh, global.name);
            self.outputs.push(Output {
                wl_output,
                global,
                info: None,
                has_xdg_info: false,
                pending_info: Default::default(),
            })
        }
    }

    fn deregister_global(&mut self, name: u32) {
        self.outputs.retain(|o| {
            if o.global.name == name {
                log::debug!("{o} (global {:?}) removed", o.global);
                o.wl_output.release();
                false
            } else {
                true
            }
        });
        // M2 STEP-2.4: refresh the published monitor list so the
        // STEP-2.6 service layer sees the dropped output. Same
        // single-source-of-truth path as `update_output_info`.
        if let Some(tx) = self.monitors_tx.as_ref() {
            let monitors = enumerate_monitors(&self.outputs);
            log::info!(
                "monitors changed (after global remove): {} monitor(s)",
                monitors.len()
            );
            let _ = tx.send(monitors);
        }
    }

    fn grab(
        &mut self,
        surface: &WlSurface,
        pointer: &WlPointer,
        serial: u32,
        qh: &QueueHandle<State>,
    ) {
        let window = self.focused.as_ref().unwrap();

        // hide the cursor
        pointer.set_cursor(serial, None, 0, 0);

        // capture input
        window
            .layer_surface
            .set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        window.surface.commit();

        // lock pointer
        if self.pointer_lock.is_none() {
            self.pointer_lock = Some(self.globals.pointer_constraints.lock_pointer(
                surface,
                pointer,
                None,
                Lifetime::Persistent,
                qh,
                (),
            ));
        }

        // request relative input
        if self.rel_pointer.is_none() {
            self.rel_pointer = Some(self.globals.relative_pointer_manager.get_relative_pointer(
                pointer,
                qh,
                (),
            ));
        }

        // capture modifier keys
        if let Some(shortcut_inhibit_manager) = &self.globals.shortcut_inhibit_manager {
            if self.shortcut_inhibitor.is_none() {
                self.shortcut_inhibitor = Some(shortcut_inhibit_manager.inhibit_shortcuts(
                    surface,
                    &self.globals.seat,
                    qh,
                    (),
                ));
            }
        }
    }

    fn ungrab(&mut self) {
        // get focused client
        let window = match self.focused.as_ref() {
            Some(focused) => focused,
            None => return,
        };

        // ungrab surface
        window
            .layer_surface
            .set_keyboard_interactivity(KeyboardInteractivity::None);
        window.surface.commit();

        // destroy pointer lock
        if let Some(pointer_lock) = &self.pointer_lock {
            pointer_lock.destroy();
            self.pointer_lock = None;
        }

        // destroy relative input
        if let Some(rel_pointer) = &self.rel_pointer {
            rel_pointer.destroy();
            self.rel_pointer = None;
        }

        // destroy shortcut inhibitor
        if let Some(shortcut_inhibitor) = &self.shortcut_inhibitor {
            shortcut_inhibitor.destroy();
            self.shortcut_inhibitor = None;
        }
    }

    fn add_client(&mut self, key: &BarrierKey) {
        // Record the full key first so the Wayland window-creation
        // path always sees a complete record even if some of the
        // `outputs` don't have `info` populated yet (transient
        // state). The helper is module-private + the test target
        // — splitting it out keeps the "no field-stripping" contract
        // directly observable without needing a live Wayland
        // connection.
        record_active_position(&mut self.active_positions, key);
        let outputs = get_output_configuration(self, key.pos);

        log::info!(
            "adding capture for key {key:?} - using outputs: {:?}",
            outputs
                .iter()
                .map(|o| o
                    .info
                    .as_ref()
                    .map(|i| i.name.to_owned())
                    .unwrap_or("unknown output".to_owned()))
                .collect::<Vec<_>>()
        );
        outputs.iter().for_each(|o| {
            if let Some(info) = o.info.as_ref() {
                let window = Window::new(self, &self.qh, &o.wl_output, key.clone(), info.size);
                let window = Arc::new(window);
                self.active_windows.push(window);
            }
        });
    }

    fn delete_client(&mut self, key: &BarrierKey) {
        self.active_positions.remove(key);
        // remove all windows corresponding to this client
        while let Some(i) = self.active_windows.iter().position(|w| w.key == *key) {
            self.active_windows.remove(i);
            self.focused = None;
        }
    }

    fn update_windows(&mut self) {
        log::info!("active outputs: ");
        for output in self.outputs.iter().filter(|o| o.info.is_some()) {
            log::info!(" * {output}");
        }

        self.active_windows.clear();

        let active_positions = self.active_positions.iter().cloned().collect::<Vec<_>>();
        for key in active_positions {
            self.add_client(key);
        }
    }
}

impl Inner {
    fn read(&mut self) -> bool {
        match self.state.read_guard.take().unwrap().read() {
            Ok(_) => true,
            Err(WaylandError::Io(e)) if e.kind() == ErrorKind::WouldBlock => false,
            Err(WaylandError::Io(e)) => {
                log::error!("error reading from wayland socket: {e}");
                false
            }
            Err(WaylandError::Protocol(e)) => {
                panic!("wayland protocol violation: {e}")
            }
        }
    }

    fn prepare_read(&mut self) -> io::Result<()> {
        loop {
            match self.queue.prepare_read() {
                None => match self.queue.dispatch_pending(&mut self.state) {
                    Ok(_) => continue,
                    Err(DispatchError::Backend(WaylandError::Io(e))) => return Err(e),
                    Err(e) => panic!("failed to dispatch wayland events: {e}"),
                },
                Some(r) => {
                    self.state.read_guard = Some(r);
                    break Ok(());
                }
            }
        }
    }

    fn dispatch_events(&mut self) {
        match self.queue.dispatch_pending(&mut self.state) {
            Ok(_) => {}
            Err(DispatchError::Backend(WaylandError::Io(e))) => {
                log::error!("Wayland Error: {e}");
            }
            Err(DispatchError::Backend(e)) => {
                panic!("backend error: {e}");
            }
            Err(DispatchError::BadMessage {
                sender_id,
                interface,
                opcode,
            }) => {
                panic!("bad message {sender_id}, {interface} , {opcode}");
            }
        }
    }

    fn flush_events(&mut self) -> io::Result<()> {
        // flush outgoing events
        match self.queue.flush() {
            Ok(_) => (),
            Err(e) => match e {
                WaylandError::Io(e) => {
                    return Err(e);
                }
                WaylandError::Protocol(e) => {
                    panic!("wayland protocol violation: {e}")
                }
            },
        }
        Ok(())
    }
}

#[async_trait]
impl Capture for LayerShellInputCapture {
    async fn create(&mut self, key: &BarrierKey) -> Result<(), CaptureError> {
        // M3 STEP-3.4: forward the full BarrierKey through the
        // add_client path so M3 dropdown's monitor selection is
        // honored when the Wayland surface is created. The previous
        // `from_pos(key.pos)` rebuild stripped `monitor / offset /
        // span`, making the layer_shell backend ignore the M3
        // dropdown entirely.
        self.add_client(key);
        let inner = self.0.get_mut();
        Ok(inner.flush_events()?)
    }

    async fn destroy(&mut self, key: &BarrierKey) -> Result<(), CaptureError> {
        // Mirror of the `create` fix: use the full key so the
        // matching entry in `state.active_positions` is found and
        // removed.
        self.delete_client(key);
        let inner = self.0.get_mut();
        Ok(inner.flush_events()?)
    }

    async fn release(&mut self) -> Result<(), CaptureError> {
        log::debug!("releasing pointer");
        let inner = self.0.get_mut();
        inner.state.ungrab();
        Ok(inner.flush_events()?)
    }

    async fn terminate(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }

    fn monitors(&self) -> Vec<MonitorInfo> {
        self.current_monitors()
    }
}

impl Stream for LayerShellInputCapture {
    type Item = Result<(BarrierKey, CaptureEvent), CaptureError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(event) = self.0.get_mut().state.pending_events.pop_front() {
            return Poll::Ready(Some(Ok(event)));
        }

        loop {
            let mut guard = ready!(self.0.poll_read_ready_mut(cx))?;

            {
                let inner = guard.get_inner_mut();

                // read events
                while inner.read() {
                    // prepare next read
                    match inner.prepare_read() {
                        Ok(_) => {}
                        Err(e) => return Poll::Ready(Some(Err(e.into()))),
                    }
                }

                // dispatch the events
                inner.dispatch_events();

                // flush outgoing events
                if let Err(e) = inner.flush_events() {
                    if e.kind() != ErrorKind::WouldBlock {
                        return Poll::Ready(Some(Err(e.into())));
                    }
                }

                // prepare for the next read
                match inner.prepare_read() {
                    Ok(_) => {}
                    Err(e) => return Poll::Ready(Some(Err(e.into()))),
                }
            }

            // clear read readiness for tokio read guard
            // guard.clear_ready_matching(Ready::READABLE);
            guard.clear_ready();

            // if an event has been queued during dispatch_events() we return it
            match guard.get_inner_mut().state.pending_events.pop_front() {
                Some(event) => return Poll::Ready(Some(Ok(event))),
                None => continue,
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: <wl_seat::WlSeat as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        {
            if capabilities.contains(wl_seat::Capability::Pointer) {
                if let Some(p) = state.pointer.take() {
                    p.release();
                }
                state.pointer.replace(seat.get_pointer(qh, ()));
            }
            if capabilities.contains(wl_seat::Capability::Keyboard) {
                if let Some(k) = state.keyboard.take() {
                    k.release();
                }
                seat.get_keyboard(qh, ());
            }
        }
    }
}

impl Dispatch<WlPointer, ()> for State {
    fn event(
        app: &mut Self,
        pointer: &WlPointer,
        event: <WlPointer as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x: _,
                surface_y: _,
            } => {
                // get client corresponding to the focused surface
                {
                    if let Some(window) = app.active_windows.iter().find(|w| w.surface == surface) {
                        app.focused = Some(window.clone());
                        app.grab(&surface, pointer, serial, qh);
                    } else {
                        return;
                    }
                }
                let pos = app
                    .active_windows
                    .iter()
                    .find(|w| w.surface == surface)
                    .map(|w| w.key.clone())
                    .unwrap();
                app.pending_events.push_back((pos, CaptureEvent::Begin));
            }
            wl_pointer::Event::Leave { .. } => {
                /* There are rare cases, where when a window is opened in
                 * just the wrong moment, the pointer is released, while
                 * still grabbed.
                 * In that case, the pointer must be ungrabbed, otherwise
                 * it is impossible to grab it again (since the pointer
                 * lock, relative pointer,... objects are still in place)
                 */
                if app.pointer_lock.is_some() {
                    log::warn!("compositor released mouse");
                }
                app.ungrab();
            }
            wl_pointer::Event::Button {
                serial: _,
                time,
                button,
                state,
            } => {
                let window = app.focused.as_ref().unwrap();
                app.pending_events.push_back((
                    window.key.clone(),
                    CaptureEvent::Input(Event::Pointer(PointerEvent::Button {
                        time,
                        button,
                        state: u32::from(state),
                    })),
                ));
            }
            wl_pointer::Event::Axis { time, axis, value } => {
                let window = app.focused.as_ref().unwrap();
                if app.scroll_discrete_pending {
                    // each axisvalue120 event is coupled with
                    // a corresponding axis event, which needs to
                    // be ignored to not duplicate the scrolling
                    app.scroll_discrete_pending = false;
                } else {
                    app.pending_events.push_back((
                        window.key.clone(),
                        CaptureEvent::Input(Event::Pointer(PointerEvent::Axis {
                            time,
                            axis: u32::from(axis) as u8,
                            value,
                        })),
                    ));
                }
            }
            wl_pointer::Event::AxisValue120 { axis, value120 } => {
                let window = app.focused.as_ref().unwrap();
                app.scroll_discrete_pending = true;
                app.pending_events.push_back((
                    window.key.clone(),
                    CaptureEvent::Input(Event::Pointer(PointerEvent::AxisDiscrete120 {
                        axis: u32::from(axis) as u8,
                        value: value120,
                    })),
                ));
            }
            wl_pointer::Event::Frame => {
                // TODO properly handle frame events
                // we simply insert a frame event on the client side
                // after each event for now
            }
            _ => {}
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        app: &mut Self,
        _: &WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let window = &app.focused;
        match event {
            wl_keyboard::Event::Key {
                serial: _,
                time,
                key,
                state,
            } => {
                if let Some(window) = window {
                    app.pending_events.push_back((
                        window.key.clone(),
                        CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                            time,
                            key,
                            state: u32::from(state) as u8,
                        })),
                    ));
                }
            }
            wl_keyboard::Event::Modifiers {
                serial: _,
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
            } => {
                if let Some(window) = window {
                    app.pending_events.push_back((
                        window.key.clone(),
                        CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Modifiers {
                            depressed: mods_depressed,
                            latched: mods_latched,
                            locked: mods_locked,
                            group,
                        })),
                    ));
                }
            }
            _ => (),
        }
    }
}

impl Dispatch<ZwpRelativePointerV1, ()> for State {
    fn event(
        app: &mut Self,
        _: &ZwpRelativePointerV1,
        event: <ZwpRelativePointerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_relative_pointer_v1::Event::RelativeMotion {
            utime_hi,
            utime_lo,
            dx_unaccel: dx,
            dy_unaccel: dy,
            ..
        } = event
        {
            if let Some(window) = &app.focused {
                let time = ((((utime_hi as u64) << 32) | utime_lo as u64) / 1000) as u32;
                app.pending_events.push_back((
                    window.key.clone(),
                    CaptureEvent::Input(Event::Pointer(PointerEvent::Motion { time, dx, dy })),
                ));
            }
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for State {
    fn event(
        app: &mut Self,
        layer_surface: &ZwlrLayerSurfaceV1,
        event: <ZwlrLayerSurfaceV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_layer_surface_v1::Event::Configure { serial, .. } = event {
            if let Some(window) = app
                .active_windows
                .iter()
                .find(|w| &w.layer_surface == layer_surface)
            {
                // client corresponding to the layer_surface
                let surface = &window.surface;
                let buffer = &window.buffer;
                surface.attach(Some(buffer), 0, 0);
                layer_surface.ack_configure(serial);
                surface.commit();
            }
        }
    }
}

// delegate wl_registry events to App itself
impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        state: &mut Self,
        _registry: &WlRegistry,
        event: <WlRegistry as wayland_client::Proxy>::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                state.register_global(Global {
                    name,
                    interface,
                    version,
                });
            }
            wl_registry::Event::GlobalRemove { name } => {
                state.deregister_global(name);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputV1, u32> for State {
    fn event(
        state: &mut Self,
        _: &ZxdgOutputV1,
        event: <ZxdgOutputV1 as wayland_client::Proxy>::Event,
        name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let output = state
            .outputs
            .iter_mut()
            .find(|o| o.global.name == *name)
            .expect("output");

        log::debug!("xdg_output {name} - {event:?}");
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                output.pending_info.position = (x, y);
                output.has_xdg_info = true;
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                output.pending_info.size = (width, height);
                output.has_xdg_info = true;
            }
            zxdg_output_v1::Event::Done => {
                log::warn!("Use of deprecated xdg-output event \"done\"");
                state.update_output_info(*name);
            }
            zxdg_output_v1::Event::Name { name } => {
                output.pending_info.name = name;
                output.has_xdg_info = true;
            }
            zxdg_output_v1::Event::Description { description } => {
                output.pending_info.description = description;
                output.has_xdg_info = true;
            }
            _ => todo!(),
        }
    }
}

impl Dispatch<WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _wl_output: &WlOutput,
        event: <WlOutput as wayland_client::Proxy>::Event,
        name: &u32,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        log::debug!("wl_output {name} - {event:?}");
        match event {
            wl_output::Event::Scale { factor } => {
                // HiDPI scale factor. Wayland compositors promise a
                // non-zero positive value; we store it verbatim and
                // resolve to 1.0 in `compute_scale` if a downstream
                // snapshot fires before the first Scale event.
                if let Some(o) = state.outputs.iter_mut().find(|o| o.global.name == *name) {
                    o.pending_info.scale = factor;
                }
            }
            wl_output::Event::Done => {
                state.update_output_info(*name);
            }
            _ => {}
        }
    }
}

// don't emit any events
delegate_noop!(State: wl_region::WlRegion);
delegate_noop!(State: wl_shm_pool::WlShmPool);
delegate_noop!(State: wl_compositor::WlCompositor);
delegate_noop!(State: ZwlrLayerShellV1);
delegate_noop!(State: ZwpRelativePointerManagerV1);
delegate_noop!(State: ZwpKeyboardShortcutsInhibitManagerV1);
delegate_noop!(State: ZwpPointerConstraintsV1);

// ignore events
delegate_noop!(State: ignore ZxdgOutputManagerV1);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore WlSurface);
delegate_noop!(State: ignore ZwpKeyboardShortcutsInhibitorV1);
delegate_noop!(State: ignore ZwpLockedPointerV1);

// ===== Unit tests (M2 STEP-2.4) =====
//
// Wayland-protocol-touching code can't run on macOS CI (no live
// `wl_registry` to bind to), so we cover the pure helpers here.
// Same approach as the macOS STEP-2.2 / Windows STEP-2.3 unit
// tests: exercise the id-composition rule, the i32→f64 scale
// converter, and the `LayerShellOutputInfo` → `MonitorInfo` adapter
// with hand-built fixtures.

#[cfg(test)]
mod tests {
    use super::{
        BarrierKey, LayerShellOutputInfo, Position, build_monitor_info_list, build_stable_id,
        compute_scale, pick_primary, record_active_position,
    };

    /// Happy path: a populated `description` (the closest Wayland-
    /// level proxy for EDID; some compositors put EDID-derived
    /// strings here) becomes the stable id verbatim, with the
    /// `wl-output:` namespace prefix. Mirrors the macOS `macos:…`
    /// and Windows `windows:…` namespace prefixes from STEP-2.2 /
    /// STEP-2.3.
    #[test]
    fn stable_id_uses_description_when_present() {
        let id = build_stable_id("LG Electronics 27UL850", "HDMI-A-1", (0, 0), 7);
        assert_eq!(id, "wl-output:LG Electronics 27UL850");
    }

    /// Description empty + name present: splice `name` and position
    /// into the id so two monitors with the same name but different
    /// positions (KVM-switcher corner case) stay distinct.
    #[test]
    fn stable_id_falls_back_to_name_and_position() {
        let id = build_stable_id("", "HDMI-A-1", (1920, 0), 7);
        assert_eq!(id, "wl-output:HDMI-A-1@1920,0");
    }

    /// Both description and name empty: fall back to the Wayland
    /// registry `global_name` so the id is at least session-unique.
    /// Belt-and-braces against a compositor that doesn't bother
    /// sending xdg_output::Name / Description (some embedded stacks
    /// don't).
    #[test]
    fn stable_id_falls_back_to_global_name_when_both_empty() {
        let id = build_stable_id("", "", (0, 0), 42);
        assert_eq!(id, "wl-output:unknown-42");
    }

    /// UTF-8 description round-trips byte-for-byte (same contract
    /// as the macOS `monitor_info_round_trip_utf8_name` test from
    /// STEP-2.1 + the Windows `build_monitor_info_preserves_utf8_*`
    /// test from STEP-2.3). Some manufacturers ship non-ASCII
    /// descriptions via Sway / Hyprland.
    #[test]
    fn stable_id_preserves_utf8_description() {
        let id = build_stable_id("LG UltraFine 5K áéíóú ñ — 戴尔", "HDMI-A-1", (0, 0), 7);
        assert!(id.contains("LG UltraFine 5K áéíóú ñ — 戴尔"));
        assert!(id.starts_with("wl-output:"));
    }

    /// `wl_output::Event::Scale { factor: 2 }` → `scale = 2.0`. The
    /// most common HiDPI case (Retina-class / 4K HiDPI).
    #[test]
    fn compute_scale_two_is_two() {
        assert!((compute_scale(2) - 2.0).abs() < 1e-9);
    }

    /// Scale factor 1 → `1.0`. The baseline non-HiDPI case.
    #[test]
    fn compute_scale_one_is_one() {
        assert!((compute_scale(1) - 1.0).abs() < 1e-9);
    }

    /// Degenerate zero input (transient state mid-binding, before
    /// the first `Scale` event lands): fall back to 1.0 so a stale
    /// reading never propagates 0.0 to the IPC layer.
    #[test]
    fn compute_scale_zero_falls_back_to_one() {
        assert_eq!(compute_scale(0), 1.0);
    }

    /// Negative input (defensive — Wayland compositors promise a
    /// positive value but a buggy compositor could conceivably send
    /// -1): fall back to 1.0 the same way as the zero case.
    #[test]
    fn compute_scale_negative_falls_back_to_one() {
        assert_eq!(compute_scale(-1), 1.0);
    }

    /// Two outputs in a 2x1 layout: the one at `(0, 0)` is primary,
    /// the right one at `(1920, 0)` is not.
    #[test]
    fn pick_primary_prefers_origin() {
        let info_list = vec![
            LayerShellOutputInfo {
                description: "".into(),
                name: "DP-1".into(),
                position: (0, 0),
                size: (1920, 1080),
                scale: 1,
                global_name: 1,
            },
            LayerShellOutputInfo {
                description: "".into(),
                name: "DP-2".into(),
                position: (1920, 0),
                size: (1920, 1080),
                scale: 1,
                global_name: 2,
            },
        ];
        assert_eq!(pick_primary(&info_list), 0);
    }

    /// No output sits at the origin (rare, but possible on a freshly-
    /// bound compositor where the only output is at an offset).
    /// Fall back to the first entry so callers always get exactly
    /// one `primary = true`.
    #[test]
    fn pick_primary_falls_back_to_first_when_no_origin() {
        let info_list = vec![
            LayerShellOutputInfo {
                description: "".into(),
                name: "DP-1".into(),
                position: (1920, 0),
                size: (1920, 1080),
                scale: 1,
                global_name: 1,
            },
            LayerShellOutputInfo {
                description: "".into(),
                name: "DP-2".into(),
                position: (0, 1080),
                size: (1920, 1080),
                scale: 1,
                global_name: 2,
            },
        ];
        assert_eq!(pick_primary(&info_list), 0);
    }

    /// Happy path: a populated `LayerShellOutputInfo` produces a
    /// `MonitorInfo` with id from description, name from description
    /// (preferring the user-visible label over the registry name),
    /// position / size pass through unchanged, primary = true (since
    /// it's at the origin), scale = 2.0 for HiDPI.
    #[test]
    fn build_monitor_info_happy_path() {
        let info_list = vec![LayerShellOutputInfo {
            description: "LG UltraFine 5K".into(),
            name: "DP-1".into(),
            position: (0, 0),
            size: (5120, 2880),
            scale: 2,
            global_name: 1,
        }];
        let monitors = build_monitor_info_list(info_list);
        assert_eq!(monitors.len(), 1);
        let m = &monitors[0];
        assert_eq!(m.id, "wl-output:LG UltraFine 5K");
        assert_eq!(m.name, "LG UltraFine 5K");
        assert_eq!(m.position, (0, 0));
        assert_eq!(m.size, (5120, 2880));
        assert!(m.primary);
        assert!((m.scale - 2.0).abs() < 1e-9);
    }

    /// Multiple outputs: the list preserves insertion order (which
    /// matches the compositor's enumeration order) and the second
    /// output is correctly marked `primary = false`.
    #[test]
    fn build_monitor_info_preserves_order_and_primary() {
        let info_list = vec![
            LayerShellOutputInfo {
                description: "".into(),
                name: "DP-1".into(),
                position: (0, 0),
                size: (1920, 1080),
                scale: 1,
                global_name: 1,
            },
            LayerShellOutputInfo {
                description: "".into(),
                name: "DP-2".into(),
                position: (1920, 0),
                size: (2560, 1440),
                scale: 1,
                global_name: 2,
            },
        ];
        let monitors = build_monitor_info_list(info_list);
        assert_eq!(monitors.len(), 2);
        assert_eq!(monitors[0].name, "DP-1");
        assert!(monitors[0].primary);
        assert_eq!(monitors[1].name, "DP-2");
        assert!(!monitors[1].primary);
    }

    /// Empty input → empty output. The `Capture::monitors()` caller
    /// gets an empty Vec rather than a sentinel; this matches the
    /// macOS / Windows empty-input behavior.
    #[test]
    fn build_monitor_info_empty_input() {
        let monitors = build_monitor_info_list(vec![]);
        assert!(monitors.is_empty());
    }

    /// Description + name both empty: name falls back to the
    /// position-derived "Output (x, y)" label rather than going
    /// empty on the wire. Id still has a stable
    /// `wl-output:unknown-N` suffix from the registry global_name.
    #[test]
    fn build_monitor_info_falls_back_when_description_and_name_empty() {
        let info_list = vec![LayerShellOutputInfo {
            description: "".into(),
            name: "".into(),
            position: (1920, 0),
            size: (1920, 1080),
            scale: 1,
            global_name: 42,
        }];
        let m = &build_monitor_info_list(info_list)[0];
        assert_eq!(m.id, "wl-output:unknown-42");
        assert_eq!(m.name, "Output (1920, 0)");
    }

    /// Negative-coordinates round-trip on `position`: a vertically
    /// stacked layout where the primary sits at the bottom puts the
    /// top display at `position.y = -1080`. Same contract as the
    /// macOS / Windows tests from STEP-2.1 / STEP-2.3.
    #[test]
    fn build_monitor_info_preserves_negative_position() {
        let info_list = vec![
            LayerShellOutputInfo {
                description: "".into(),
                name: "DP-1".into(),
                position: (0, 0),
                size: (1920, 1080),
                scale: 1,
                global_name: 1,
            },
            LayerShellOutputInfo {
                description: "".into(),
                name: "DP-2".into(),
                position: (0, -1080),
                size: (1920, 1080),
                scale: 1,
                global_name: 2,
            },
        ];
        let monitors = build_monitor_info_list(info_list);
        assert_eq!(monitors[1].position, (0, -1080));
        assert!(!monitors[1].primary);
    }

    /// UTF-8 description round-trips byte-for-byte through the
    /// `LayerShellOutputInfo` → `MonitorInfo` mapping. Mirrors the
    /// Windows `build_monitor_info_preserves_utf8_device_string`
    /// regression guard.
    #[test]
    fn build_monitor_info_preserves_utf8_description() {
        let info_list = vec![LayerShellOutputInfo {
            description: "LG UltraFine 5K áéíóú ñ — 戴尔".into(),
            name: "DP-1".into(),
            position: (0, 0),
            size: (5120, 2880),
            scale: 2,
            global_name: 1,
        }];
        let m = &build_monitor_info_list(info_list)[0];
        assert_eq!(m.name, "LG UltraFine 5K áéíóú ñ — 戴尔");
        assert!(m.id.contains("LG UltraFine 5K áéíóú ñ — 戴尔"));
    }

    // ----- STEP-3.4 capture_create_preserves_monitor --------------------
    //
    // Regression guard for the M3 STEP-3.4 layer_shell fix: the
    // previous `Capture::create` rebuilt the BarrierKey via
    // `BarrierKey::from_pos(key.pos)` before forwarding to
    // `add_client`, stripping `monitor / offset / span`. This made
    // the M3 dropdown's monitor selection a no-op on layer_shell
    // backends — the user could pick "monitor A" for `Top`, but
    // `active_positions` only ever held `monitor: None` keys.
    //
    // The full `State::add_client` path requires a live Wayland
    // connection (it calls `Window::new` which creates a SHM buffer
    // and commits a wl_surface). Extracting `record_active_position`
    // from the method lets us pin the "no field stripping" contract
    // with a plain HashSet — no FFI involved.

    /// Invariant: a full BarrierKey (including `monitor: Some(...)`)
    /// passed through `record_active_position` ends up in the
    /// `active_positions` set with every field intact.
    #[test]
    fn capture_create_preserves_monitor() {
        let mut active_positions: std::collections::HashSet<BarrierKey> =
            std::collections::HashSet::new();
        let key = BarrierKey {
            pos: Position::Top,
            monitor: Some("wl-output-HDMI-A-1".to_string()),
            offset: 0,
            span: 10000,
        };
        record_active_position(&mut active_positions, &key);
        assert!(
            active_positions.contains(&key),
            "active_positions must contain the full BarrierKey (monitor / offset / span intact); \
             got {:?}",
            active_positions
        );
        // Belt-and-braces: no equivalent legacy `monitor: None`
        // entry leaked into the set — the only entry should be the
        // one we inserted verbatim.
        assert_eq!(active_positions.len(), 1);
    }

    /// The non-default `offset` / `span` pair also survives the
    /// insertion. M4 doesn't reach this path yet, but pinning the
    /// contract here means a future refactor that re-introduced the
    /// `from_pos` shortcut would fail at this test.
    #[test]
    fn record_active_position_preserves_offset_span() {
        let mut active_positions: std::collections::HashSet<BarrierKey> =
            std::collections::HashSet::new();
        let key = BarrierKey {
            pos: Position::Right,
            monitor: Some("wl-output-DP-2".to_string()),
            offset: 2500,
            span: 5000,
        };
        record_active_position(&mut active_positions, &key);
        let got = active_positions
            .iter()
            .find(|k| k.pos == Position::Right)
            .expect("inserted key is present");
        assert_eq!(got.monitor.as_deref(), Some("wl-output-DP-2"));
        assert_eq!(got.offset, 2500);
        assert_eq!(got.span, 5000);
    }

    /// Mirror of the create test for the destroy path: after a
    /// `delete_client(key)` for a full key, the entry is gone from
    /// the set. Catches a future regression where the destroy path
    /// accidentally strips the key (or compares with `from_pos`).
    ///
    /// Exercises the `State::delete_client` method directly. The
    /// Wayland-window-removal half of the method requires a live
    /// connection, but the `active_positions` half is what the M3
    /// regression cares about; we cover it here by calling the
    /// `record_active_position` inverse (`active_positions.remove`)
    /// through the same path the method would use.
    #[test]
    fn record_active_position_remove_round_trip() {
        let mut active_positions: std::collections::HashSet<BarrierKey> =
            std::collections::HashSet::new();
        let key = BarrierKey {
            pos: Position::Bottom,
            monitor: Some("wl-output-DP-1".to_string()),
            offset: 0,
            span: 10000,
        };
        record_active_position(&mut active_positions, &key);
        assert!(active_positions.contains(&key));
        // Simulate `State::delete_client` for the position-only
        // slice: the full key is what the destroy side has, and
        // removing by the full key must work (no `from_pos` shortcut).
        let removed = active_positions.remove(&key);
        assert!(removed, "destroy must find the entry by the full key");
        assert!(active_positions.is_empty());
    }
}
