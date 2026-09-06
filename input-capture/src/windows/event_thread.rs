use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::ptr::addr_of_mut;

use std::default::Default;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::error::TrySendError;
use windows::Win32::Foundation::{FALSE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICEW, ENUM_CURRENT_SETTINGS,
    EnumDisplayDevicesW, EnumDisplaySettingsW,
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

use crate::geometry::{DisplayRect, clamp_to_display_bounds, cursor_within, entered_barrier};

use super::{BarrierKey, CaptureEvent, Position};

pub(crate) struct EventThread {
    request_buffer: Arc<Mutex<Vec<ClientUpdate>>>,
    thread: Option<thread::JoinHandle<()>>,
    thread_id: u32,
}

impl EventThread {
    pub(crate) fn new(event_tx: Sender<(BarrierKey, CaptureEvent)>) -> Self {
        let request_buffer = Default::default();
        let (thread, thread_id) = start(event_tx, Arc::clone(&request_buffer));
        Self {
            request_buffer,
            thread: Some(thread),
            thread_id,
        }
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
    static ACTIVE_CLIENT: Cell<Option<BarrierKey>> = const { Cell::new(None) };
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
    static PENDING_CLIENT: Cell<Option<BarrierKey>> = const { Cell::new(None) };
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
) -> (thread::JoinHandle<()>, u32) {
    /* condition variable to wait for thread id */
    let thread_id = Arc::new((Condvar::new(), Mutex::new(None)));
    let thread_id_ = Arc::clone(&thread_id);

    let msg_thread = thread::spawn(|| start_routine(thread_id_, event_tx, request_buffer));

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
) {
    EVENT_TX.replace(Some(event_tx));
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
        return ACTIVE_CLIENT.get().is_some();
    }
    let mouse_low_level: MSLLHOOKSTRUCT = unsafe { *(lparam.0 as *const MSLLHOOKSTRUCT) };
    let curr_pos = (mouse_low_level.pt.x as f64, mouse_low_level.pt.y as f64);
    let prev_pos = PREV_POS.get().unwrap_or(curr_pos);
    PREV_POS.replace(Some(curr_pos));

    /* While capturing: consume the event. mouse_proc returning true here leads to LRESULT(1) to swallow it. */
    if ACTIVE_CLIENT.get().is_some() {
        return true;
    }

    /* A pending client is already set → check whether the user pulled the cursor back inside.
     *
     * Pulled back → clear pending and notify the main thread (CancelPending),
     * entering the "no client" pass-through state. Pull-back followed by a
     * fresh crossing is picked up by `entered_barrier` on the next
     * WM_MOUSEMOVE, starting a new pending flow normally.
     */
    if let Some(pending_key) = PENDING_CLIENT.get() {
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
    let Some(key) = ACTIVE_CLIENT.get() else {
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
    let Some(client) = ACTIVE_CLIENT.get() else {
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
        enumerate_displays(displays);
        log::debug!("displays: {displays:?}");
        *generation = global_generation;
    }
}

fn enumerate_displays(display_rects: &mut Vec<DisplayRect>) {
    display_rects.clear();
    unsafe {
        let mut devices = vec![];
        for i in 0.. {
            let mut device: DISPLAY_DEVICEW = std::mem::zeroed();
            device.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
            let ret = EnumDisplayDevicesW(None, i, &mut device, EDD_GET_DEVICE_INTERFACE_NAME);
            if ret == FALSE {
                break;
            }
            if device
                .StateFlags
                .contains(DISPLAY_DEVICE_ATTACHED_TO_DESKTOP)
            {
                devices.push(device.DeviceName);
            }
        }
        for device in devices {
            let mut dev_mode: DEVMODEW = std::mem::zeroed();
            dev_mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
            let ret = EnumDisplaySettingsW(
                PCWSTR::from_raw(&device as *const _),
                ENUM_CURRENT_SETTINGS,
                &mut dev_mode,
            );
            if ret == FALSE {
                log::warn!("no display mode");
            }

            let pos = dev_mode.Anonymous1.Anonymous2.dmPosition;
            let (x, y) = (pos.x, pos.y);
            let (width, height) = (dev_mode.dmPelsWidth, dev_mode.dmPelsHeight);

            display_rects.push(DisplayRect::new(
                x as f64,
                y as f64,
                width as f64,
                height as f64,
            ));
        }
    }
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
            if PENDING_CLIENT.get() == Some(key.clone()) {
                PENDING_CLIENT.take();
            }
            if let Some(active_key) = ACTIVE_CLIENT.get() {
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
            if PENDING_CLIENT.get() != Some(key.clone()) {
                log::trace!(
                    "start_capture({key:?}) ignored: pending={:?}",
                    PENDING_CLIENT.get()
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
            if PENDING_CLIENT.get() == Some(key) {
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
