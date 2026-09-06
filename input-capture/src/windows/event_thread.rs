use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::ptr::addr_of_mut;

use std::default::Default;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::watch;
use windows::Win32::Foundation::{FALSE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICE_PRIMARY_DEVICE, DISPLAY_DEVICEW,
    ENUM_CURRENT_SETTINGS, EnumDisplayDevicesW, EnumDisplaySettingsW,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::core::{PCWSTR, w};

use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DispatchMessageW, EDD_GET_DEVICE_INTERFACE_NAME, GetMessageW,
    HOOKPROC, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, MSG, MSLLHOOKSTRUCT, PostThreadMessageW,
    RegisterClassW, SetWindowsHookExW, TranslateMessage, WH_KEYBOARD_LL, WH_MOUSE_LL, WINDOW_STYLE,
    WM_DISPLAYCHANGE, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN,
    WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP,
    WM_SYSKEYDOWN, WM_SYSKEYUP, WM_USER, WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSW, WNDPROC,
};

use input_event::{
    BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event, KeyboardEvent, PointerEvent,
    scancode::{self, Linux},
};

use crate::geometry::{
    DisplayRect, MonitorInfo, clamp_to_display_bounds, cursor_within, entered_barrier,
};

use super::{BarrierKey, CaptureEvent};

pub(crate) struct EventThread {
    request_buffer: Arc<Mutex<Vec<ClientUpdate>>>,
    thread: Option<thread::JoinHandle<()>>,
    thread_id: u32,
    /// Sender for the latest monitor list. The watch channel is the
    /// single source of truth — `monitor_changes()` hands out new
    /// receivers to upstream consumers (STEP-2.6 service layer) and
    /// `current_monitors()` reads `borrow().clone()` for synchronous
    /// snapshot consumers (STEP-2.5's `Capture::monitors()` impl).
    ///
    /// The channel is created at `EventThread::new` and seeded with an
    /// initial enumeration so subscribers can read the current state
    /// without waiting for the first WM_DISPLAYCHANGE.
    monitors_tx: watch::Sender<Vec<MonitorInfo>>,
}

impl EventThread {
    pub(crate) fn new(event_tx: Sender<(BarrierKey, CaptureEvent)>) -> Self {
        let request_buffer = Default::default();
        let (monitors_tx, _) = watch::channel(Vec::new());
        // Seed the watch channel with the initial enumeration so
        // subscribers (STEP-2.6 service layer) get a non-empty list
        // immediately, even if no display change happens during the
        // process lifetime. Runs on the calling thread; the FFI
        // touchpoints (`EnumDisplayDevicesW` /
        // `EnumDisplaySettingsW`) are safe to invoke from any thread.
        let initial = enumerate_monitors();
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
        let (thread, thread_id) = start(event_tx, Arc::clone(&request_buffer), monitors_tx.clone());
        Self {
            request_buffer,
            thread: Some(thread),
            thread_id,
            monitors_tx,
        }
    }

    /// Subscribe to the latest monitor list. Each call returns a new
    /// receiver that sees every future update (a new entry is published
    /// on every `WM_DISPLAYCHANGE` and once during construction).
    /// STEP-2.6 service layer holds the receiver and forwards
    /// `MonitorsChanged` events to the IPC frontend.
    #[allow(dead_code)] // STEP-2.5/2.6 will consume this
    pub(crate) fn monitor_changes(&self) -> watch::Receiver<Vec<MonitorInfo>> {
        self.monitors_tx.subscribe()
    }

    /// Snapshot of the most recent monitor list, captured without
    /// touching the watch channel. Used by STEP-2.5's
    /// `Capture::monitors()` impl when polling is acceptable and the
    /// caller does not need a subscription.
    #[allow(dead_code)] // STEP-2.5/2.6 will consume this
    pub(crate) fn current_monitors(&self) -> Vec<MonitorInfo> {
        self.monitors_tx.borrow().clone()
    }

    pub(crate) fn release_capture(&self) {
        self.signal(RequestType::Release);
    }

    pub(crate) fn create(&self, key: BarrierKey) {
        self.client_update(ClientUpdate::Create(key));
    }

    pub(crate) fn destroy(&self, key: BarrierKey) {
        self.client_update(ClientUpdate::Destroy(key));
    }

    /// **Pending-capture handshake (main thread entry)**: promotes the
    /// pending Begin on `key` to active. Called by the main thread after
    /// the remote Ack arrives.
    ///
    /// Reuses the existing `ClientUpdate` + `RequestType::ClientUpdate`
    /// channel — no new `RequestType` variant is introduced. Processing
    /// is serialized inside the Windows thread, so the pending → active
    /// transition is atomic (the hook thread and message loop share the
    /// same thread, so there are no concurrent writers).
    pub(crate) fn start_capture(&self, key: BarrierKey) {
        self.client_update(ClientUpdate::StartCapture(key));
    }

    /// **Pending-capture handshake (main thread entry)**: cancels the
    /// pending Begin on `key` (if any). Called by the main thread in
    /// these scenarios:
    /// - Enter send failed (network down)
    /// - 500ms tick detected Ack timeout
    /// - `release_bind` was pressed while pending
    pub(crate) fn cancel_pending(&self, key: BarrierKey) {
        self.client_update(ClientUpdate::CancelPending(key));
    }

    fn exit(&self) {
        self.signal(RequestType::Exit);
    }

    fn client_update(&self, request: ClientUpdate) {
        {
            let mut requests = self.request_buffer.lock().unwrap();
            requests.push(request);
        }
        self.signal(RequestType::ClientUpdate);
    }

    fn signal(&self, event_type: RequestType) {
        let id = self.thread_id;
        unsafe { PostThreadMessageW(id, WM_USER, WPARAM(event_type as usize), LPARAM(0)).unwrap() };
    }
}

impl Drop for EventThread {
    fn drop(&mut self) {
        self.exit();
        let _ = self.thread.take().expect("thread").join();
    }
}

enum RequestType {
    ClientUpdate = 0,
    Release = 1,
    Exit = 2,
}

enum ClientUpdate {
    Create(BarrierKey),
    Destroy(BarrierKey),
    /// Main thread says: Ack received, promote the pending Begin on
    /// `BarrierKey` to an active capture (cursor hidden, events consumed).
    /// No-op if no pending Begin matches.
    StartCapture(BarrierKey),
    /// Main thread says: cancel any pending Begin on `BarrierKey`
    /// (Ack timeout, send failure, or release-bind pressed). No-op if
    /// no pending Begin matches.
    CancelPending(BarrierKey),
}

fn blocking_send_event(key: BarrierKey, event: CaptureEvent) {
    EVENT_TX.with_borrow_mut(|tx| tx.as_mut().unwrap().blocking_send((key, event)).unwrap())
}

fn try_send_event(
    key: BarrierKey,
    event: CaptureEvent,
) -> Result<(), TrySendError<(BarrierKey, CaptureEvent)>> {
    EVENT_TX.with_borrow_mut(|tx| tx.as_mut().unwrap().try_send((key, event)))
}

thread_local! {
    /// all configured clients
    static CLIENTS: RefCell<HashSet<BarrierKey>> = RefCell::new(HashSet::new());
    /// currently active client (cursor hidden, events consumed).
    ///
    /// Held in a [`RefCell`] rather than a [`Cell`] because
    /// [`BarrierKey`] carries an `Option<String>` `monitor` field and
    /// is therefore not `Copy`. The Windows hook + message loop run on
    /// the same thread, so the runtime borrow is never contested in
    /// practice — the `RefCell` exists purely so the slot can hold a
    /// non-Copy type.
    static ACTIVE_CLIENT: RefCell<Option<BarrierKey>> = const { RefCell::new(None) };
    /// Pending client (cursor still visible on the host, Enter already
    /// sent to the remote, waiting for the Ack). Mutually exclusive with
    /// [`ACTIVE_CLIENT`] — promotion clears pending, cancel clears
    /// pending without setting active.
    ///
    /// **Why this intermediate state exists**: previously
    /// `check_client_activation` set `ACTIVE_CLIENT` the moment the
    /// cursor crossed a barrier, which caused `mouse_proc` to start
    /// consuming events (`LRESULT(1)`) synchronously. If the remote
    /// was unreachable, the hook had already swallowed the cursor
    /// before the main thread could wait for the Ack. With pending,
    /// `mouse_proc` falls through to `CallNextHookEx` while only
    /// `PENDING_CLIENT` is set — the cursor stays on the host; the
    /// main thread promotes it to active via `start_capture` once the
    /// Ack arrives.
    static PENDING_CLIENT: RefCell<Option<BarrierKey>> = const { RefCell::new(None) };
    /// Entry point captured at the moment of barrier crossing. Preserved
    /// across promotion so the eventual active Begin yields the same
    /// Motion deltas as if we'd gone active immediately.
    static PENDING_ENTRY_POINT: Cell<(f64, f64)> = const { Cell::new((0.0, 0.0)) };
    /// input event channel
    static EVENT_TX: RefCell<Option<Sender<(BarrierKey, CaptureEvent)>>> = const { RefCell::new(None) };
    /// position of barrier entry (active)
    static ENTRY_POINT: Cell<(f64, f64)> = const { Cell::new((0.0, 0.0)) };
    /// previous mouse position (kept as `f64` to match the rest of the
    /// geometry module and avoid i32/f64 round-trips on every event).
    static PREV_POS: Cell<Option<(f64, f64)>> = const { Cell::new(None) };
    /// displays and generation counter
    static DISPLAYS: RefCell<(Vec<DisplayRect>, i32)> = const { RefCell::new((Vec::new(), 0)) };
    /// Sender for the latest monitor list. Set by `start_routine` from
    /// the `monitors_tx` field of [`EventThread`]. The watch channel is
    /// the single source of truth for monitor enumeration; subscribers
    /// are served by [`EventThread::monitor_changes`], which returns
    /// receivers cloned from this sender.
    static MONITORS_TX: RefCell<Option<watch::Sender<Vec<MonitorInfo>>>> =
        const { RefCell::new(None) };
}

fn get_msg() -> Option<MSG> {
    unsafe {
        let mut msg = std::mem::zeroed();
        let ret = GetMessageW(addr_of_mut!(msg), None, 0, 0);
        match ret.0 {
            0 => None,
            x if x > 0 => Some(msg),
            _ => panic!("error in GetMessageW"),
        }
    }
}

fn start(
    event_tx: Sender<(BarrierKey, CaptureEvent)>,
    request_buffer: Arc<Mutex<Vec<ClientUpdate>>>,
    monitors_tx: watch::Sender<Vec<MonitorInfo>>,
) -> (thread::JoinHandle<()>, u32) {
    /* condition variable to wait for thread id */
    let thread_id = Arc::new((Condvar::new(), Mutex::new(None)));
    let thread_id_ = Arc::clone(&thread_id);

    let msg_thread =
        thread::spawn(|| start_routine(thread_id_, event_tx, request_buffer, monitors_tx));

    /* wait for thread to set its id */
    let (cond, thread_id) = &*thread_id;
    let mut thread_id = thread_id.lock().unwrap();
    while (*thread_id).is_none() {
        thread_id = cond.wait(thread_id).expect("channel closed");
    }
    (msg_thread, thread_id.expect("thread id"))
}

fn start_routine(
    ready: Arc<(Condvar, Mutex<Option<u32>>)>,
    event_tx: Sender<(BarrierKey, CaptureEvent)>,
    request_buffer: Arc<Mutex<Vec<ClientUpdate>>>,
    monitors_tx: watch::Sender<Vec<MonitorInfo>>,
) {
    EVENT_TX.replace(Some(event_tx));
    MONITORS_TX.replace(Some(monitors_tx));
    /* communicate thread id */
    {
        let (cnd, mtx) = &*ready;
        let mut ready = mtx.lock().unwrap();
        *ready = Some(unsafe { GetCurrentThreadId() });
        cnd.notify_one();
    }

    let mouse_proc: HOOKPROC = Some(mouse_proc);
    let kybrd_proc: HOOKPROC = Some(kybrd_proc);
    let window_proc: WNDPROC = Some(window_proc);

    /* register hooks */
    unsafe {
        let _ = SetWindowsHookExW(WH_MOUSE_LL, mouse_proc, None, 0).unwrap();
        let _ = SetWindowsHookExW(WH_KEYBOARD_LL, kybrd_proc, None, 0).unwrap();
    }

    let instance = unsafe { GetModuleHandleW(None).unwrap() };
    let instance = instance.into();
    let window_class: WNDCLASSW = WNDCLASSW {
        lpfnWndProc: window_proc,
        hInstance: instance,
        lpszClassName: w!("lan-mouse-message-window-class"),
        ..Default::default()
    };

    static WINDOW_CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);
    if WINDOW_CLASS_REGISTERED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        /* register window class if not yet done so */
        unsafe {
            let ret = RegisterClassW(&window_class);
            if ret == 0 {
                panic!("RegisterClassW");
            }
        }
    }

    /* window is used to receive WM_DISPLAYCHANGE messages */
    unsafe {
        CreateWindowExW(
            Default::default(),
            w!("lan-mouse-message-window-class"),
            w!("lan-mouse-msg-window"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )
        .expect("CreateWindowExW");
    }

    /* run message loop */
    while let Some(msg) = get_msg() {
        // mouse / keybrd proc do not actually return a message
        if msg.hwnd.0.is_null() {
            /* messages sent via PostThreadMessage */
            match msg.wParam.0 {
                x if x == RequestType::Exit as usize => break,
                x if x == RequestType::Release as usize => {
                    // Release clears both active and pending — belt-and-suspenders.
                    // The main thread, when calling `release_capture` while pending,
                    // first sends `CancelPending` via capture.cancel_pending(...),
                    // but we re-clear here as well in case the cancel_pending
                    // message has not yet been drained by this loop when release
                    // arrives.
                    ACTIVE_CLIENT.take();
                    PENDING_CLIENT.take();
                }
                x if x == RequestType::ClientUpdate as usize => {
                    let requests = {
                        let mut res = vec![];
                        let mut requests = request_buffer.lock().unwrap();
                        for request in requests.drain(..) {
                            res.push(request);
                        }
                        res
                    };

                    for request in requests {
                        update_clients(request)
                    }
                }
                _ => {}
            }
        } else {
            /* other messages for window_procs */
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

fn check_client_activation(wparam: WPARAM, lparam: LPARAM) -> bool {
    if wparam.0 != WM_MOUSEMOVE as usize {
        return ACTIVE_CLIENT.with_borrow(|c| c.is_some());
    }
    let mouse_low_level: MSLLHOOKSTRUCT = unsafe { *(lparam.0 as *const MSLLHOOKSTRUCT) };
    let curr_pos = (mouse_low_level.pt.x as f64, mouse_low_level.pt.y as f64);
    let prev_pos = PREV_POS.get().unwrap_or(curr_pos);
    PREV_POS.replace(Some(curr_pos));

    /* While capturing: consume the event. mouse_proc returning true here leads to LRESULT(1) to swallow it. */
    if ACTIVE_CLIENT.with_borrow(|c| c.is_some()) {
        return true;
    }

    /* A pending client is already set → check whether the user pulled the cursor back inside.
     *
     * Pulled back → clear pending and notify the main thread (CancelPending),
     * entering the "no client" pass-through state. Pull-back followed by a
     * fresh crossing is picked up by `entered_barrier` on the next
     * WM_MOUSEMOVE, starting a new pending flow normally.
     */
    if let Some(pending_key) = PENDING_CLIENT.with_borrow(|c| c.clone()) {
        let within = DISPLAYS.with_borrow_mut(|(displays, generation)| {
            update_display_regions(displays, generation);
            cursor_within(curr_pos, displays, pending_key.pos)
        });
        if within {
            PENDING_CLIENT.take();
            log::debug!("CANCEL pending {pending_key:?} (cursor pulled back inside)");
            blocking_send_event(pending_key, CaptureEvent::CancelPending);
        }
        return false;
    }

    /* No active / no pending → check for a barrier crossing. */
    let entered = DISPLAYS.with_borrow_mut(|(displays, generation)| {
        update_display_regions(displays, generation);
        entered_barrier(prev_pos, curr_pos, displays)
    });

    let Some(pos) = entered else {
        return false;
    };

    // M1: lift the detected Position into a BarrierKey. monitor /
    // offset / span stay at their legacy defaults until M2 wires
    // monitor info end-to-end.
    let key = BarrierKey::from_pos(pos);

    /* check if a client is registered for the barrier */
    if !CLIENTS.with_borrow(|clients| clients.contains(&key)) {
        return false;
    }

    /* Enter pending — do NOT set ACTIVE_CLIENT.
     * When mouse_proc sees PENDING_CLIENT (and not ACTIVE_CLIENT) it does
     * not consume events, so the cursor stays on the host and moves normally.
     */
    PENDING_CLIENT.replace(Some(key.clone()));
    let entry_point =
        DISPLAYS.with_borrow(|(displays, _)| clamp_to_display_bounds(displays, prev_pos, curr_pos));
    PENDING_ENTRY_POINT.replace(entry_point);

    log::debug!("PENDING @ {prev_pos:?} -> {curr_pos:?}");
    blocking_send_event(key, CaptureEvent::BeginPending);

    false
}

unsafe extern "system" fn mouse_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let active = check_client_activation(wparam, lparam);

    /* no client was active */
    if !active {
        return CallNextHookEx(None, ncode, wparam, lparam);
    }

    /* get active client if any */
    let Some(key) = ACTIVE_CLIENT.with_borrow(|c| c.clone()) else {
        return LRESULT(1);
    };

    /* convert to lan-mouse event */
    let Some(pointer_event) = to_mouse_event(wparam, lparam) else {
        return LRESULT(1);
    };

    /* notify mainthread (drop events if sending too fast) */
    if let Err(e) = try_send_event(key, CaptureEvent::Input(Event::Pointer(pointer_event))) {
        log::warn!("e: {e}");
    }

    /* don't pass event to applications */
    LRESULT(1)
}

unsafe extern "system" fn kybrd_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    /* get active client if any */
    let Some(client) = ACTIVE_CLIENT.with_borrow(|c| c.clone()) else {
        return CallNextHookEx(None, ncode, wparam, lparam);
    };

    /* convert to key event */
    let Some(key_event) = to_key_event(wparam, lparam) else {
        return LRESULT(1);
    };

    if let Err(e) = try_send_event(client, CaptureEvent::Input(Event::Keyboard(key_event))) {
        log::warn!("e: {e}");
    }

    /* don't pass event to applications */
    LRESULT(1)
}

unsafe extern "system" fn window_proc(
    _hwnd: HWND,
    uint: u32,
    _wparam: WPARAM,
    _lparam: LPARAM,
) -> LRESULT {
    if uint == WM_DISPLAYCHANGE {
        log::debug!("display resolution changed");
        DISPLAY_RESOLUTION_GENERATION.fetch_add(1, Ordering::Release);
    }
    LRESULT(1)
}

static DISPLAY_RESOLUTION_GENERATION: AtomicI32 = AtomicI32::new(1);

fn update_display_regions(displays: &mut Vec<DisplayRect>, generation: &mut i32) {
    let global_generation = DISPLAY_RESOLUTION_GENERATION.load(Ordering::Acquire);
    if *generation != global_generation {
        let info = enumerate_displays_inner();
        // Refresh the legacy `Vec<DisplayRect>` first so barrier
        // detection on the next mouse move sees the new layout.
        displays.clear();
        for d in &info {
            displays.push(DisplayRect::new(
                d.position.0 as f64,
                d.position.1 as f64,
                d.size.0 as f64,
                d.size.1 as f64,
            ));
        }
        log::debug!("displays: {displays:?}");
        // Then publish the new monitor list to subscribers (STEP-2.6
        // service layer). Done unconditionally — even if the new list
        // is identical to the previous one — so a downstream layer
        // that snapshots "on every generation bump" still gets a
        // notification. The watch channel ignores identical sends
        // anyway (it only marks "changed" when the inner value
        // changes by `Eq`), so this is essentially free for the
        // no-change case.
        let monitors = build_monitor_info_list(&info);
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
        MONITORS_TX.with_borrow(|tx| {
            if let Some(tx) = tx {
                let _ = tx.send(monitors);
            }
        });
        *generation = global_generation;
    }
}

/// Per-display info extracted from a single `EnumDisplayDevicesW` +
/// `EnumDisplaySettingsW` pair. The `device_name` is the GDI display
/// name (`\\.\DISPLAY1`); `device_id` is the PnP device instance ID
/// (typically `MONITOR\{EDID-hash}\{instance-guid}`); `device_string`
/// is the friendly display string (often empty, in which case we fall
/// back to `device_name`). `scale` is derived from `dmLogPixels`
/// (96 DPI = 1.0).
#[derive(Debug, Default, Clone)]
struct WinDisplayInfo {
    device_name: String,
    device_id: String,
    device_string: String,
    position: (i32, i32),
    size: (u32, u32),
    primary: bool,
    scale: f64,
}

/// Walk every active desktop-attached display and extract the fields
/// the backend needs. Pulled out so both the legacy `Vec<DisplayRect>`
/// producer (`enumerate_displays`) and the M2 monitor-info producer
/// (`build_monitor_info_list` → `enumerate_monitors`) share the same
/// FFI loop and EDID / state-flag handling.
fn enumerate_displays_inner() -> Vec<WinDisplayInfo> {
    let mut out = Vec::new();
    unsafe {
        for i in 0.. {
            let mut device: DISPLAY_DEVICEW = std::mem::zeroed();
            device.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
            let ret = EnumDisplayDevicesW(None, i, &mut device, EDD_GET_DEVICE_INTERFACE_NAME);
            if ret == FALSE {
                break;
            }
            if !device
                .StateFlags
                .contains(DISPLAY_DEVICE_ATTACHED_TO_DESKTOP)
            {
                continue;
            }
            let device_name = wide_string_to_string(&device.DeviceName);
            let device_id = wide_string_to_string(&device.DeviceID);
            let device_string = wide_string_to_string(&device.DeviceString);
            let primary = device.StateFlags.contains(DISPLAY_DEVICE_PRIMARY_DEVICE);

            let mut dev_mode: DEVMODEW = std::mem::zeroed();
            dev_mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
            let ret = EnumDisplaySettingsW(
                PCWSTR::from_raw(device.DeviceName.as_ptr()),
                ENUM_CURRENT_SETTINGS,
                &mut dev_mode,
            );
            if ret == FALSE {
                log::warn!("no display mode for {device_name}");
                continue;
            }

            let pos = dev_mode.Anonymous1.Anonymous2.dmPosition;
            let position = (pos.x, pos.y);
            let size = (dev_mode.dmPelsWidth, dev_mode.dmPelsHeight);
            let scale = compute_scale(dev_mode.dmLogPixels);

            out.push(WinDisplayInfo {
                device_name,
                device_id,
                device_string,
                position,
                size,
                primary,
                scale,
            });
        }
    }
    out
}

fn enumerate_displays(display_rects: &mut Vec<DisplayRect>) {
    display_rects.clear();
    for d in enumerate_displays_inner() {
        display_rects.push(DisplayRect::new(
            d.position.0 as f64,
            d.position.1 as f64,
            d.size.0 as f64,
            d.size.1 as f64,
        ));
    }
}

/// Compose a stable Windows monitor id from the PnP `DeviceID` and
/// the GDI `DeviceName`. `DeviceID` is the OS-reported stable id
/// (typically `MONITOR\{EDID-hash}\{instance-guid}`); it survives
/// replug on the same port but changes if the EDID changes (rare).
/// We prepend a `windows:` prefix so the id namespace is OS-tagged,
/// matching the macOS pattern from STEP-2.2.
///
/// Fallback chain:
///   1. Non-empty `DeviceID` → `windows:{device_id}`
///   2. Empty `DeviceID` → `windows:unknown-{device_name}` so two
///      simultaneously-DeviceID-less displays still produce distinct
///      ids. Without this they would collide on `windows:` alone —
///      same P1-shaped regression the macOS side fixed in
///      STEP-M2-2.2-FIXUP.
fn build_stable_id(device_id: &str, device_name: &str) -> String {
    if !device_id.is_empty() {
        format!("windows:{device_id}")
    } else {
        format!("windows:unknown-{device_name}")
    }
}

/// Convert the Windows per-monitor `dmLogPixels` (DPI) into the
/// `MonitorInfo::scale` (point→pixel ratio) the rest of the system
/// uses. 96 DPI is the Win32 default ("100%") and maps to scale=1.0;
/// a 4K monitor reported as "150% scaling" writes 144 DPI → scale=1.5.
///
/// `dmLogPixels = 0` means the field wasn't filled in (rare, but seen
/// on transient state mid-reconfigure); we fall back to 1.0 so a
/// missing value never propagates a NaN / Inf scale into the IPC
/// payload.
fn compute_scale(dm_log_pixels: u16) -> f64 {
    if dm_log_pixels == 0 {
        1.0
    } else {
        dm_log_pixels as f64 / 96.0
    }
}

/// Translate the raw `DISPLAY_DEVICEW::DeviceName` /
/// `DeviceID` / `DeviceString` wide-char arrays into UTF-8 strings.
/// Returns an empty string when the buffer is unterminated or empty
/// (which can happen during transient reconfiguration states).
fn wide_string_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// Convert the per-OS enumeration into the OS-agnostic
/// `geometry::MonitorInfo` list. Pure (input is fully owned), so it
/// can be exercised in tests with hand-built fixtures.
fn build_monitor_info_list(displays: &[WinDisplayInfo]) -> Vec<MonitorInfo> {
    displays
        .iter()
        .map(|d| {
            let id = build_stable_id(&d.device_id, &d.device_name);
            let name = if !d.device_string.is_empty() {
                d.device_string.clone()
            } else if !d.device_name.is_empty() {
                format!("Display {}", d.device_name.trim_start_matches("\\\\.\\"))
            } else {
                // Both empty — extremely unlikely (would mean
                // EnumDisplayDevicesW returned a half-populated
                // struct), but fall back to a position-derived label
                // so we never emit an empty `name` over the wire.
                format!("Display ({}, {})", d.position.0, d.position.1)
            };
            MonitorInfo {
                id,
                name,
                position: d.position,
                size: d.size,
                primary: d.primary,
                scale: d.scale,
            }
        })
        .collect()
}

/// Public snapshot enumeration — invoked at `EventThread::new` time
/// to seed the watch channel. Also useful for STEP-2.5's
/// `Capture::monitors()` impl on cold start.
fn enumerate_monitors() -> Vec<MonitorInfo> {
    build_monitor_info_list(&enumerate_displays_inner())
}

fn update_clients(request: ClientUpdate) {
    match request {
        ClientUpdate::Create(key) => {
            CLIENTS.with_borrow_mut(|clients| clients.insert(key));
        }
        ClientUpdate::Destroy(key) => {
            // When removing the client's key, both pending and active must be
            // cleared; otherwise StartCapture / mouse_proc would still operate
            // on the stale key and misbehave.
            if PENDING_CLIENT.with_borrow(|c| c == &Some(key.clone())) {
                PENDING_CLIENT.take();
            }
            if let Some(active_key) = ACTIVE_CLIENT.with_borrow(|c| c.clone()) {
                if key == active_key {
                    let _ = ACTIVE_CLIENT.take();
                }
            }
            CLIENTS.with_borrow_mut(|clients| clients.remove(&key));
        }
        ClientUpdate::StartCapture(key) => {
            // Only promote when the pending key matches. On mismatch
            // (e.g. the user switched sides during pending), it's a no-op
            // and the remote Ack path naturally falls through to Idle.
            if !PENDING_CLIENT.with_borrow(|c| c == &Some(key.clone())) {
                log::trace!(
                    "start_capture({key:?}) ignored: pending={:?}",
                    PENDING_CLIENT.with_borrow(|c| c.clone())
                );
                return;
            }
            PENDING_CLIENT.take();
            ACTIVE_CLIENT.replace(Some(key.clone()));
            ENTRY_POINT.replace(PENDING_ENTRY_POINT.get());
            log::debug!("promoted pending client {key:?} to active");
            // Actively emit Begin to the main thread — the main thread calls
            // start_capture upon receiving the Ack, completing the
            // ack-to-begin loop.
            blocking_send_event(key, CaptureEvent::Begin);
        }
        ClientUpdate::CancelPending(key) => {
            if PENDING_CLIENT.with_borrow(|c| c == &Some(key.clone())) {
                PENDING_CLIENT.take();
                log::debug!("cleared pending client {key:?} (cancelled by main)");
            }
        }
    }
}

fn to_key_event(wparam: WPARAM, lparam: LPARAM) -> Option<KeyboardEvent> {
    let kybrdllhookstruct: KBDLLHOOKSTRUCT = unsafe { *(lparam.0 as *const KBDLLHOOKSTRUCT) };
    let mut scan_code = kybrdllhookstruct.scanCode;
    log::trace!("scan_code: {scan_code}");
    if kybrdllhookstruct.flags.contains(LLKHF_EXTENDED) {
        scan_code |= 0xE000;
    }
    let Ok(win_scan_code) = scancode::Windows::try_from(scan_code) else {
        log::warn!("failed to translate to windows scancode: {scan_code}");
        return None;
    };
    log::trace!("windows_scan: {win_scan_code:?}");
    let Ok(linux_scan_code): Result<Linux, ()> = win_scan_code.try_into() else {
        log::warn!("failed to translate into linux scancode: {win_scan_code:?}");
        return None;
    };
    log::trace!("windows_scan: {linux_scan_code:?}");
    let scan_code = linux_scan_code as u32;
    match wparam {
        WPARAM(p) if p == WM_KEYDOWN as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 1,
        }),
        WPARAM(p) if p == WM_KEYUP as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 0,
        }),
        WPARAM(p) if p == WM_SYSKEYDOWN as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 1,
        }),
        WPARAM(p) if p == WM_SYSKEYUP as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 0,
        }),
        _ => None,
    }
}

fn to_mouse_event(wparam: WPARAM, lparam: LPARAM) -> Option<PointerEvent> {
    let mouse_low_level: MSLLHOOKSTRUCT = unsafe { *(lparam.0 as *const MSLLHOOKSTRUCT) };
    match wparam {
        WPARAM(p) if p == WM_LBUTTONDOWN as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_LEFT,
            state: 1,
        }),
        WPARAM(p) if p == WM_MBUTTONDOWN as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_MIDDLE,
            state: 1,
        }),
        WPARAM(p) if p == WM_RBUTTONDOWN as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_RIGHT,
            state: 1,
        }),
        WPARAM(p) if p == WM_LBUTTONUP as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_LEFT,
            state: 0,
        }),
        WPARAM(p) if p == WM_MBUTTONUP as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_MIDDLE,
            state: 0,
        }),
        WPARAM(p) if p == WM_RBUTTONUP as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_RIGHT,
            state: 0,
        }),
        WPARAM(p) if p == WM_MOUSEMOVE as usize => {
            let (x, y) = (mouse_low_level.pt.x as f64, mouse_low_level.pt.y as f64);
            let (ex, ey) = ENTRY_POINT.get();
            let (dx, dy) = (x - ex, y - ey);
            Some(PointerEvent::Motion { time: 0, dx, dy })
        }
        WPARAM(p) if p == WM_MOUSEWHEEL as usize => Some(PointerEvent::AxisDiscrete120 {
            axis: 0,
            value: -(mouse_low_level.mouseData as i32 >> 16),
        }),
        WPARAM(p) if p == WM_XBUTTONDOWN as usize || p == WM_XBUTTONUP as usize => {
            let hb = mouse_low_level.mouseData >> 16;
            let button = match hb {
                1 => BTN_BACK,
                2 => BTN_FORWARD,
                _ => {
                    log::warn!("unknown mouse button");
                    return None;
                }
            };
            Some(PointerEvent::Button {
                time: 0,
                button,
                state: if p == WM_XBUTTONDOWN as usize { 1 } else { 0 },
            })
        }
        WPARAM(p) if p == WM_MOUSEHWHEEL as usize => Some(PointerEvent::AxisDiscrete120 {
            axis: 1, // Horizontal
            value: mouse_low_level.mouseData as i32 >> 16,
        }),
        w => {
            log::warn!("unknown mouse event: {w:?}");
            None
        }
    }
}

// ===== Unit tests (M2 STEP-2.3) ====================================
//
// The FFI-touching parts of the Windows monitor enumeration can't
// run on macOS / Linux CI (no `EnumDisplayDevicesW` available), so
// we cover the pure helpers here. Same approach as the macOS
// STEP-2.2 unit tests: exercise the id composition, the
// DPI→scale conversion, the wide-string decoder, and the
// `WinDisplayInfo` → `MonitorInfo` adapter with hand-built fixtures.

#[cfg(test)]
mod tests {
    use super::{
        WinDisplayInfo, build_monitor_info_list, build_stable_id, compute_scale,
        wide_string_to_string,
    };

    /// Happy path: a populated PnP `DeviceID` becomes the stable id
    /// verbatim, with the `windows:` namespace prefix.
    #[test]
    fn stable_id_uses_device_id_when_present() {
        let id = build_stable_id("MONITOR\\GSM5B23\\{abc-123-def-456}", r"\\.\DISPLAY1");
        assert_eq!(id, r"windows:MONITOR\GSM5B23\{abc-123-def-456}");
    }

    /// Empty `DeviceID` fallback: splice `device_name` into the
    /// `unknown-` segment so two DeviceID-less displays still get
    /// distinct ids. Mirrors the macOS `DisplayInfo::unknown` P1 fix.
    #[test]
    fn stable_id_falls_back_to_device_name_when_device_id_empty() {
        let id = build_stable_id("", r"\\.\DISPLAY1");
        assert_eq!(id, r"windows:unknown-\\.\DISPLAY1");
    }

    /// Both empty (transient state): the resulting id is still
    /// `windows:` rather than empty — never emit a blank id to the
    /// wire.
    #[test]
    fn stable_id_handles_both_empty_gracefully() {
        let id = build_stable_id("", "");
        assert_eq!(id, "windows:unknown-");
    }

    /// `dmLogPixels = 96` is the Win32 "100% scaling" baseline and
    /// maps to `scale = 1.0`. The most common case on a single-1080p
    /// display.
    #[test]
    fn compute_scale_96_dpi_is_one() {
        assert!((compute_scale(96) - 1.0).abs() < 1e-9);
    }

    /// 4K monitor with 150% Windows scaling reports `dmLogPixels = 144`
    /// → `scale = 1.5`. Mixed-DPI awareness exercises this path.
    #[test]
    fn compute_scale_144_dpi_is_one_point_five() {
        assert!((compute_scale(144) - 1.5).abs() < 1e-9);
    }

    /// `dmLogPixels = 192` is the "200% scaling" used by most Surface
    /// and HiDPI laptop screens.
    #[test]
    fn compute_scale_192_dpi_is_two() {
        assert!((compute_scale(192) - 2.0).abs() < 1e-9);
    }

    /// Degenerate zero input (transient state mid-reconfigure): fall
    /// back to 1.0 so we never propagate 0.0 scale to the IPC layer.
    #[test]
    fn compute_scale_zero_falls_back_to_one() {
        assert_eq!(compute_scale(0), 1.0);
    }

    /// Wide-string decoder strips the trailing NUL and decodes the
    /// buffer as UTF-16LE (the encoding `DISPLAY_DEVICEW` uses).
    #[test]
    fn wide_string_to_string_decodes_utf16_with_nul() {
        // "DISPLAY1" in UTF-16LE: D=0x44, I=0x49, S=0x53, P=0x50, L=0x4C, A=0x41, Y=0x59, 1=0x31
        let buf: [u16; 9] = [0x44, 0x49, 0x53, 0x50, 0x4C, 0x41, 0x59, 0x31, 0];
        assert_eq!(wide_string_to_string(&buf), "DISPLAY1");
    }

    /// Wide-string decoder handles CJK correctly. Some manufacturers
    /// ship CJK device strings (e.g. LG's Korean firmware).
    #[test]
    fn wide_string_to_string_decodes_utf16_cjk() {
        // "戴尔" (Dell, simplified Chinese)
        let mut buf = [0u16; 3];
        buf[0] = 0x6C;
        buf[1] = 0x5C;
        // 尔
        buf[2] = 0x8033;
        // No trailing NUL — exercise the "use full length" branch.
        assert_eq!(wide_string_to_string(&buf), "戴尔");
    }

    /// Empty buffer → empty string (used to detect unterminated /
    /// unused `DISPLAY_DEVICEW` slots).
    #[test]
    fn wide_string_to_string_empty_buffer() {
        let buf: [u16; 0] = [];
        assert_eq!(wide_string_to_string(&buf), "");
    }

    /// `WinDisplayInfo` → `MonitorInfo` happy path: id uses DeviceID,
    /// name uses DeviceString, primary/scale/position/size pass through
    /// unchanged.
    #[test]
    fn build_monitor_info_happy_path() {
        let displays = vec![WinDisplayInfo {
            device_name: r"\\.\DISPLAY1".to_string(),
            device_id: r"MONITOR\GSM5B23\{abc-123}".to_string(),
            device_string: "Generic PnP Monitor".to_string(),
            position: (0, 0),
            size: (1920, 1080),
            primary: true,
            scale: 1.0,
        }];
        let monitors = build_monitor_info_list(&displays);
        assert_eq!(monitors.len(), 1);
        let m = &monitors[0];
        assert_eq!(m.id, r"windows:MONITOR\GSM5B23\{abc-123}");
        assert_eq!(m.name, "Generic PnP Monitor");
        assert_eq!(m.position, (0, 0));
        assert_eq!(m.size, (1920, 1080));
        assert!(m.primary);
        assert!((m.scale - 1.0).abs() < 1e-9);
    }

    /// Empty DeviceString falls back to a `Display {name}` label
    /// (rather than an empty `name`). The frontend tooltip relies on
    /// a non-empty `name` so it can show "(unknown)" otherwise.
    #[test]
    fn build_monitor_info_falls_back_to_display_name() {
        let displays = vec![WinDisplayInfo {
            device_name: r"\\.\DISPLAY2".to_string(),
            device_id: "".to_string(),
            device_string: "".to_string(),
            position: (1920, 0),
            size: (2560, 1440),
            primary: false,
            scale: 1.5,
        }];
        let m = &build_monitor_info_list(&displays)[0];
        // DeviceID empty → id falls back to device_name.
        assert_eq!(m.id, r"windows:unknown-\\.\DISPLAY2");
        // DeviceString empty → name derived from device_name, with
        // the `\\.\` prefix stripped for human readability.
        assert_eq!(m.name, "Display DISPLAY2");
        assert!(!m.primary);
        assert!((m.scale - 1.5).abs() < 1e-9);
    }

    /// Belt-and-braces: when both DeviceString and DeviceName are
    /// empty (would only happen with a malformed EnumDisplayDevicesW
    /// result), the name falls back to a position-derived label
    /// rather than going empty on the wire.
    #[test]
    fn build_monitor_info_handles_fully_empty_identity() {
        let displays = vec![WinDisplayInfo {
            device_name: "".to_string(),
            device_id: "".to_string(),
            device_string: "".to_string(),
            position: (-1920, 0),
            size: (1920, 1080),
            primary: false,
            scale: 1.0,
        }];
        let m = &build_monitor_info_list(&displays)[0];
        assert_eq!(m.id, "windows:unknown-");
        assert_eq!(m.name, "Display (-1920, 0)");
    }

    /// Multiple displays: the list preserves insertion order (which
    /// matches the OS's enumeration order) so the frontend can show
    /// them in a stable, predictable sequence.
    #[test]
    fn build_monitor_info_preserves_order() {
        let displays = vec![
            WinDisplayInfo {
                device_name: r"\\.\DISPLAY1".to_string(),
                device_id: "id-1".to_string(),
                device_string: "Left".to_string(),
                position: (0, 0),
                size: (1920, 1080),
                primary: true,
                scale: 1.0,
            },
            WinDisplayInfo {
                device_name: r"\\.\DISPLAY2".to_string(),
                device_id: "id-2".to_string(),
                device_string: "Right".to_string(),
                position: (1920, 0),
                size: (2560, 1440),
                primary: false,
                scale: 1.5,
            },
        ];
        let monitors = build_monitor_info_list(&displays);
        assert_eq!(monitors.len(), 2);
        assert_eq!(monitors[0].name, "Left");
        assert_eq!(monitors[1].name, "Right");
        assert!(monitors[0].primary);
        assert!(!monitors[1].primary);
    }

    /// UTF-8 device string round-trip: verify a CJK / accented
    /// `device_string` (which `wide_string_to_string` would produce
    /// from the `DISPLAY_DEVICEW` buffer) survives the
    /// `WinDisplayInfo` → `MonitorInfo` mapping byte-for-byte. This
    /// is the Windows-equivalent regression guard for the macOS
    /// `monitor_info_round_trip_utf8_name` test added in STEP-2.1.
    #[test]
    fn build_monitor_info_preserves_utf8_device_string() {
        let displays = vec![WinDisplayInfo {
            device_name: r"\\.\DISPLAY1".to_string(),
            device_id: "id-1".to_string(),
            device_string: "LG UltraFine 5K áéíóú ñ — 戴尔".to_string(),
            position: (0, 0),
            size: (5120, 2880),
            primary: true,
            scale: 2.0,
        }];
        let m = &build_monitor_info_list(&displays)[0];
        assert_eq!(m.name, "LG UltraFine 5K áéíóú ñ — 戴尔");
    }
}
