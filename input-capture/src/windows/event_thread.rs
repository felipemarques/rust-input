use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ptr::addr_of_mut;

use std::default::Default;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::error::TrySendError;
use windows::Win32::Foundation::{FALSE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICEW, ENUM_CURRENT_SETTINGS,
    EnumDisplayDevicesW, EnumDisplaySettingsW,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::{
    GetCurrentThread, GetCurrentThreadId, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows::core::{PCWSTR, w};

use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DispatchMessageW, EDD_GET_DEVICE_INTERFACE_NAME,
    EVENT_SYSTEM_FOREGROUND, GetCursorPos, GetForegroundWindow, GetMessageW, HHOOK, HOOKPROC,
    KBDLLHOOKSTRUCT, LLKHF_EXTENDED, MSG, MSLLHOOKSTRUCT, PostThreadMessageW, RegisterClassW,
    SetCursorPos, SetTimer, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
    WH_KEYBOARD_LL, WH_MOUSE_LL, WINDOW_STYLE, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
    WM_DISPLAYCHANGE, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN,
    WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP,
    WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER, WM_USER, WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSW,
    WNDPROC,
};

use input_event::{
    BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event, KeyboardEvent, PointerEvent,
    edge::{Area, Edge, Span},
    scancode::{self, Linux},
};

use super::{
    CaptureEvent, Position, display_util,
    process::{self, Program},
};

pub(crate) struct EventThread {
    request_buffer: Arc<Mutex<Vec<ClientUpdate>>>,
    thread: Option<thread::JoinHandle<()>>,
    thread_id: u32,
}

impl EventThread {
    pub(crate) fn new(event_tx: Sender<(Position, CaptureEvent)>) -> Self {
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

    /// releases the capture, putting the pointer at `fraction` along the edge
    pub(crate) fn release_capture_to(&self, fraction: f64) {
        let id = self.thread_id;
        let request = WPARAM(RequestType::ReleaseTo as usize);
        let fraction = LPARAM(fraction.to_bits() as isize);
        unsafe { PostThreadMessageW(id, WM_USER, request, fraction).unwrap() };
    }

    /// starts capturing for the client at `pos` as if the pointer crossed
    /// that edge where it is now
    pub(crate) fn enter(&self, pos: Position) {
        let id = self.thread_id;
        let request = WPARAM(RequestType::Enter as usize);
        let pos = LPARAM(position_code(pos));
        unsafe { PostThreadMessageW(id, WM_USER, request, pos).unwrap() };
    }

    pub(crate) fn create(&self, pos: Position) {
        self.client_update(ClientUpdate::Create(pos));
    }

    pub(crate) fn destroy(&self, pos: Position) {
        self.client_update(ClientUpdate::Destroy(pos));
    }

    /// the parts of the edge at `pos` that lead to a client, all of it if none
    pub(crate) fn set_spans(&self, pos: Position, spans: Vec<Span>) {
        self.client_update(ClientUpdate::Spans(pos, spans));
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
    /// release with the pointer placed along the edge, the position travels
    /// as the bits of an f64 in the LPARAM
    ReleaseTo = 3,
    /// start capturing without crossing, the position of the client travels
    /// in the LPARAM (see [`position_code`])
    Enter = 4,
}

fn position_code(pos: Position) -> isize {
    match pos {
        Position::Left => 0,
        Position::Right => 1,
        Position::Top => 2,
        Position::Bottom => 3,
    }
}

fn position_from_code(code: isize) -> Option<Position> {
    Some(match code {
        0 => Position::Left,
        1 => Position::Right,
        2 => Position::Top,
        3 => Position::Bottom,
        _ => return None,
    })
}

/// starts capturing for the client at `pos` as if the pointer crossed that
/// edge where it is now
fn enter(pos: Position) {
    if ACTIVE_CLIENT.get().is_some() || !CLIENTS.with_borrow(|c| c.contains(&pos)) {
        return;
    }
    let desktop = DISPLAYS.with_borrow_mut(|(displays, generation)| {
        update_display_regions(displays, generation);
        desktop(displays)
    });
    let Some(desktop) = desktop else {
        return;
    };
    let mut point = POINT::default();
    let entry = unsafe {
        // the display regions and the hook use physical pixels
        let previous = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let at = GetCursorPos(&mut point).ok().map(|_| (point.x, point.y));
        let entry = at.map(|at| {
            let fraction = desktop.fraction(Edge::from(pos), at);
            (fraction, desktop.point(Edge::from(pos), fraction))
        });
        if let Some((_, (x, y))) = entry {
            let _ = SetCursorPos(x, y);
        }
        if !previous.is_invalid() {
            SetThreadDpiAwarenessContext(previous);
        }
        entry
    };
    let Some((fraction, point)) = entry else {
        return;
    };
    PREV_POS.replace(Some(point));
    WATCH.with_borrow_mut(|w| w.cursor = Some(point));
    if begin(pos, point, Some(fraction)) {
        log::debug!("ENTERED on request @ {point:?}");
    }
}

/// Starts capturing for the client at `pos`, the pointer entering it at
/// `point` of this computer, `fraction` along the edge. Returns whether it
/// did: the hooks never wait for the service, if it is too busy to be told
/// the pointer does not cross this time and the next movement tries again.
fn begin(pos: Position, point: (i32, i32), fraction: Option<f64>) -> bool {
    // read by the service as soon as it is told
    ENTRY_POINT.replace(point);
    ENTRY_FRACTION.store(fraction.map_or(u64::MAX, f64::to_bits), Ordering::Relaxed);
    if let Err(e) = try_send_event(pos, CaptureEvent::Begin) {
        if every("begin", Duration::from_secs(5)) {
            log::warn!("the pointer did not cross: RustInput is busy ({e})");
        }
        return false;
    }
    ACTIVE_CLIENT.replace(Some(pos));
    HELD_KEYS.with_borrow_mut(HashSet::clear);
    // forget keys whose release went by unseen
    LOCAL_KEYS
        .with_borrow_mut(|keys| keys.retain(|&vk| unsafe { GetAsyncKeyState(vk as i32) } < 0));
    WATCH.with_borrow_mut(|w| w.held_at = None);
    let foreground = FOREGROUND.with_borrow(Clone::clone);
    if let Some(program) = foreground.filter(|_| every("begin in front", Duration::from_secs(30))) {
        log::warn!(
            "{} is in front: Windows does not hand its keyboard to RustInput, \
             what is typed goes to it and not to the other computer",
            program.describe()
        );
    }
    true
}

/// [`f64`] bits of the entry fraction of the current capture, NaN if unknown
static ENTRY_FRACTION: AtomicU64 = AtomicU64::new(u64::MAX);

/// where along its edge the pointer was when the current capture began
pub(crate) fn entry_fraction() -> Option<f64> {
    let fraction = f64::from_bits(ENTRY_FRACTION.load(Ordering::Relaxed));
    (!fraction.is_nan()).then_some(fraction)
}

/// the rectangle spanned by all displays
fn desktop(displays: &[RECT]) -> Option<Area> {
    Area::bounding(displays.iter().map(|d| Area {
        x: d.left,
        y: d.top,
        width: d.right - d.left,
        height: d.bottom - d.top,
    }))
}

/// releases the capture, putting the pointer at `fraction` along its edge
fn release_to(fraction: f64) {
    let Some(pos) = ACTIVE_CLIENT.take() else {
        return;
    };
    let Some(desktop) = DISPLAYS.with_borrow(|(displays, _)| desktop(displays)) else {
        return;
    };
    let (x, y) = desktop.point(Edge::from(pos), fraction);
    unsafe {
        // the display regions and the hook use physical pixels: so must the
        // pointer position, also when the displays are scaled
        let previous = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        if let Err(e) = SetCursorPos(x, y) {
            log::warn!("could not place the pointer at ({x}, {y}): {e}");
        }
        if !previous.is_invalid() {
            SetThreadDpiAwarenessContext(previous);
        }
    }
    PREV_POS.replace(Some((x, y)));
    WATCH.with_borrow_mut(|w| w.cursor = Some((x, y)));
}

enum ClientUpdate {
    Create(Position),
    Destroy(Position),
    Spans(Position, Vec<Span>),
}

fn try_send_event(
    pos: Position,
    event: CaptureEvent,
) -> Result<(), TrySendError<(Position, CaptureEvent)>> {
    EVENT_TX.with_borrow_mut(|tx| tx.as_mut().unwrap().try_send((pos, event)))
}

thread_local! {
    /// all configured clients
    static CLIENTS: RefCell<HashSet<Position>> = RefCell::new(HashSet::new());
    /// the parts of each edge that lead to a client, all of it if none
    static SPANS: RefCell<HashMap<Position, Vec<Span>>> = RefCell::new(HashMap::new());
    /// currently active client
    static ACTIVE_CLIENT: Cell<Option<Position>> = const { Cell::new(None) };
    /// input event channel
    static EVENT_TX: RefCell<Option<Sender<(Position, CaptureEvent)>>> = const { RefCell::new(None) };
    /// position of barrier entry
    static ENTRY_POINT: Cell<(i32, i32)> = const { Cell::new((0, 0)) };
    /// previous mouse position
    static PREV_POS: Cell<Option<(i32, i32)>> = const { Cell::new(None) };
    /// displays and generation counter
    static DISPLAYS: RefCell<(Vec<RECT>, i32)> = const { RefCell::new((Vec::new(), 0)) };
    /// keys held down while capturing, to recognize auto-repeat
    static HELD_KEYS: RefCell<HashSet<u32>> = RefCell::new(HashSet::new());
    /// virtual keys held down on this computer: when the pointer leaves
    /// with one of them held, Windows still gets its release
    static LOCAL_KEYS: RefCell<HashSet<u32>> = RefCell::new(HashSet::new());
    /// mouse buttons held down on this computer, the same for them
    static LOCAL_BUTTONS: RefCell<HashSet<u32>> = RefCell::new(HashSet::new());
    /// the hooks, to install them again
    static HOOKS: Cell<Option<(HHOOK, HHOOK)>> = const { Cell::new(None) };
    /// calls of the mouse hook so far
    static MOUSE_CALLS: Cell<u64> = const { Cell::new(0) };
    /// what the watchdog saw at its previous look
    static WATCH: RefCell<Watch> = RefCell::new(Watch::default());
    /// the program in front if it is out of reach of the hooks
    static FOREGROUND: RefCell<Option<Program>> = const { RefCell::new(None) };
    /// when something was last logged, see [`every`]
    static LOGGED: RefCell<HashMap<String, Instant>> = RefCell::new(HashMap::new());
    /// events that did not fit into the channel since the last report
    static DROPPED: Cell<u64> = const { Cell::new(0) };
}

/// What the watchdog saw at its previous look, see [`watchdog`].
#[derive(Default)]
struct Watch {
    /// where the pointer was
    cursor: Option<(i32, i32)>,
    /// [`MOUSE_CALLS`] then
    calls: u64,
    /// looks in a row the pointer moved without the mouse hook being called
    unseen: u32,
    /// where the pointer stays while capturing: the mouse hook keeps it there
    held_at: Option<(i32, i32)>,
    /// the capture ended here but the service could not be told yet
    release_pending: Option<Position>,
}

/// how often the watchdog looks, in ms
const WATCHDOG_PERIOD: u32 = 250;
const WATCHDOG_TIMER: usize = 1;
/// an input reaching the hooks this late is reported, in ms
const LATE: u32 = 200;

/// whether something identified by `key` was not logged in the last
/// `period`, which then starts again
fn every(key: &str, period: Duration) -> bool {
    LOGGED.with_borrow_mut(|logged| {
        let now = Instant::now();
        match logged.get(key) {
            Some(last) if now.duration_since(*last) < period => false,
            _ => {
                logged.insert(key.to_owned(), now);
                true
            }
        }
    })
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
    event_tx: Sender<(Position, CaptureEvent)>,
    request_buffer: Arc<Mutex<Vec<ClientUpdate>>>,
) -> (thread::JoinHandle<()>, u32) {
    /* condition variable to wait for thead id */
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
    event_tx: Sender<(Position, CaptureEvent)>,
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

    let window_proc: WNDPROC = Some(window_proc);

    unsafe {
        // the pointer, the displays and the hooks in physical pixels
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        // Windows waits for the hooks with every input: they must answer
        // right away also while the computer is busy
        if let Err(e) = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) {
            log::warn!("could not raise the priority of the hooks: {e}");
        }
    }
    process::keep_full_speed();

    /* register hooks */
    install_hooks().expect("could not watch the keyboard and the mouse");
    log::info!(
        "watching the keyboard and the mouse, RustInput {} as administrator",
        if process::runs_as_administrator() {
            "runs"
        } else {
            "does not run"
        }
    );

    let instance = unsafe { GetModuleHandleW(None).unwrap() };
    let instance = instance.into();
    let window_class: WNDCLASSW = WNDCLASSW {
        lpfnWndProc: window_proc,
        hInstance: instance,
        lpszClassName: w!("rustinput-message-window-class"),
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

    /* window is used ro receive WM_DISPLAYCHANGE messages, and the timer of
     * the watchdog */
    let window = unsafe {
        CreateWindowExW(
            Default::default(),
            w!("rustinput-message-window-class"),
            w!("rustinput-msg-window"),
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
        .expect("CreateWindowExW")
    };
    unsafe {
        if SetTimer(Some(window), WATCHDOG_TIMER, WATCHDOG_PERIOD, None) == 0 {
            log::warn!("could not start the watchdog of the hooks");
        }
        // which program is in front: Windows keeps the keyboard going to
        // one run as administrator from the hooks
        let hook = SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(foreground_changed),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        );
        if hook.is_invalid() {
            log::warn!("could not follow the window in front");
        }
        in_front(GetForegroundWindow());
    }

    /* run message loop */
    while let Some(msg) = get_msg() {
        // mouse / keybrd proc do not actually return a message
        if msg.hwnd.0.is_null() && msg.message == WM_USER {
            /* messages sent via PostThreadMessage */
            match msg.wParam.0 {
                x if x == RequestType::Exit as usize => break,
                x if x == RequestType::Release as usize => {
                    ACTIVE_CLIENT.take();
                }
                x if x == RequestType::ReleaseTo as usize => {
                    release_to(f64::from_bits(msg.lParam.0 as u64));
                }
                x if x == RequestType::Enter as usize => {
                    if let Some(pos) = position_from_code(msg.lParam.0) {
                        enter(pos);
                    }
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
    let curr_pos = (mouse_low_level.pt.x, mouse_low_level.pt.y);
    let prev_pos = PREV_POS.get().unwrap_or(curr_pos);
    PREV_POS.replace(Some(curr_pos));

    /* next event is the first actual event */
    let ret = ACTIVE_CLIENT.get().is_some();

    /* client already active, no need to check */
    if ACTIVE_CLIENT.get().is_some() {
        return ret;
    }

    /* check if a client was activated */
    let entered = DISPLAYS.with_borrow_mut(|(displays, generation)| {
        update_display_regions(displays, generation);
        display_util::entered_barrier(prev_pos, curr_pos, displays)
    });

    let Some(pos) = entered else {
        return ret;
    };

    /* check if a client is registered for the barrier */
    if !CLIENTS.with_borrow(|clients| clients.contains(&pos)) {
        return ret;
    }

    /* update active client and entry point, notify main thread */
    let entry_point = DISPLAYS.with_borrow(|(displays, _)| {
        display_util::clamp_to_display_bounds(displays, prev_pos, curr_pos)
    });
    let fraction = DISPLAYS.with_borrow(|(displays, _)| {
        desktop(displays).map(|d| d.fraction(Edge::from(pos), entry_point))
    });
    /* a part of the edge that leads nowhere: a wall */
    let on_span = SPANS.with_borrow(|spans| {
        spans
            .get(&pos)
            .filter(|spans| !spans.is_empty())
            .is_none_or(|spans| fraction.is_none_or(|f| spans.iter().any(|s| s.contains(f))))
    });
    if !on_span {
        return ret;
    }
    if begin(pos, entry_point, fraction) {
        log::debug!("ENTERED @ {prev_pos:?} -> {curr_pos:?}");
    }

    ret
}

unsafe extern "system" fn mouse_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if ncode < 0 {
        return CallNextHookEx(None, ncode, wparam, lparam);
    }
    MOUSE_CALLS.set(MOUSE_CALLS.get().wrapping_add(1));
    note_delay(unsafe { (*(lparam.0 as *const MSLLHOOKSTRUCT)).time });

    let active = check_client_activation(wparam, lparam);

    /* no client was active */
    if !active {
        if let Some((button, down)) = button_of(wparam, lparam) {
            LOCAL_BUTTONS.with_borrow_mut(|held| {
                if down {
                    held.insert(button)
                } else {
                    held.remove(&button)
                }
            });
        }
        return CallNextHookEx(None, ncode, wparam, lparam);
    }

    /* get active client if any */
    let Some(pos) = ACTIVE_CLIENT.get() else {
        return LRESULT(1);
    };

    /* a button held on this computer when the pointer left: Windows gets
     * its release, the other computer never saw it pressed. Pressed again
     * means its release went by unseen, it is the other computer's now. */
    if let Some((button, down)) = button_of(wparam, lparam) {
        if LOCAL_BUTTONS.with_borrow_mut(|held| held.remove(&button)) && !down {
            return CallNextHookEx(None, ncode, wparam, lparam);
        }
    }

    /* convert to rustinput event */
    let Some(pointer_event) = to_mouse_event(wparam, lparam) else {
        return LRESULT(1);
    };

    /* notify mainthread (drop events if sending too fast) */
    if try_send_event(pos, CaptureEvent::Input(Event::Pointer(pointer_event))).is_err() {
        dropped();
    }

    /* don't pass event to applications */
    LRESULT(1)
}

/// the mouse button of a press or release, and whether it is a press
fn button_of(wparam: WPARAM, lparam: LPARAM) -> Option<(u32, bool)> {
    if matches!(
        wparam.0 as u32,
        WM_MOUSEMOVE | WM_MOUSEWHEEL | WM_MOUSEHWHEEL
    ) {
        return None;
    }
    match to_mouse_event(wparam, lparam)? {
        PointerEvent::Button { button, state, .. } => Some((button, state == 1)),
        _ => None,
    }
}

/// an event that did not fit into the channel: reported at most every 5 s
fn dropped() {
    DROPPED.set(DROPPED.get() + 1);
    if every("dropped", Duration::from_secs(5)) {
        log::warn!(
            "RustInput is busy: {} events of the keyboard and the mouse were dropped",
            DROPPED.take()
        );
    }
}

/// Windows hands every input to the hooks one after the other, waiting for
/// each: an input reaching them late tells that Windows holds RustInput
/// back. `time` is when the input happened, as [`GetTickCount`].
fn note_delay(time: u32) {
    let late = unsafe { GetTickCount() }.wrapping_sub(time);
    if (LATE..60_000).contains(&late) && every("late", Duration::from_secs(10)) {
        log::warn!(
            "the keyboard and the mouse reach RustInput {late} ms late: \
             Windows holds it back (the computer is busy or saving energy)"
        );
    }
}

unsafe extern "system" fn kybrd_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if ncode < 0 {
        return CallNextHookEx(None, ncode, wparam, lparam);
    }
    let hook: KBDLLHOOKSTRUCT = unsafe { *(lparam.0 as *const KBDLLHOOKSTRUCT) };
    note_delay(hook.time);
    let down = matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);

    /* get active client if any */
    let Some(client) = ACTIVE_CLIENT.get() else {
        LOCAL_KEYS.with_borrow_mut(|held| {
            if down {
                held.insert(hook.vkCode)
            } else {
                held.remove(&hook.vkCode)
            }
        });
        return CallNextHookEx(None, ncode, wparam, lparam);
    };

    /* a key held on this computer when the pointer left: its repeats are
     * dropped and Windows gets its release, the other computer never saw
     * it pressed */
    if LOCAL_KEYS.with_borrow(|held| held.contains(&hook.vkCode)) {
        if down {
            return LRESULT(1);
        }
        LOCAL_KEYS.with_borrow_mut(|held| held.remove(&hook.vkCode));
        return CallNextHookEx(None, ncode, wparam, lparam);
    }

    /* convert to key event */
    let Some(key_event) = to_key_event(wparam, lparam) else {
        return LRESULT(1);
    };

    /* Windows repeats a held key as further key-down events. Every receiving
     * side repeats on its own (the X server, the compositor, the Windows and
     * macOS emulation), forwarding these would repeat twice as fast, so they
     * are dropped like the other capture backends do. */
    if let KeyboardEvent::Key { key, state, .. } = key_event {
        if HELD_KEYS.with_borrow_mut(|held| is_repeat(held, key, state)) {
            return LRESULT(1);
        }
    }

    if try_send_event(client, CaptureEvent::Input(Event::Keyboard(key_event))).is_err() {
        dropped();
    }

    /* don't pass event to applications */
    LRESULT(1)
}

unsafe extern "system" fn window_proc(
    _hwnd: HWND,
    uint: u32,
    wparam: WPARAM,
    _lparam: LPARAM,
) -> LRESULT {
    if uint == WM_DISPLAYCHANGE {
        log::debug!("display resolution changed");
        DISPLAY_RESOLUTION_GENERATION.fetch_add(1, Ordering::Release);
    }
    if uint == WM_TIMER && wparam.0 == WATCHDOG_TIMER {
        watchdog();
        return LRESULT(0);
    }
    LRESULT(1)
}

fn install_hooks() -> windows::core::Result<()> {
    let mouse_proc: HOOKPROC = Some(mouse_proc);
    let kybrd_proc: HOOKPROC = Some(kybrd_proc);
    unsafe {
        let mouse = SetWindowsHookExW(WH_MOUSE_LL, mouse_proc, None, 0)?;
        let keyboard = match SetWindowsHookExW(WH_KEYBOARD_LL, kybrd_proc, None, 0) {
            Ok(keyboard) => keyboard,
            Err(e) => {
                let _ = UnhookWindowsHookEx(mouse);
                return Err(e);
            }
        };
        HOOKS.set(Some((mouse, keyboard)));
    }
    Ok(())
}

/// Installs the hooks again, at most every 5 s; whether it did. Windows
/// removes a hook without telling when it answers too late.
fn reinstall_hooks() -> bool {
    if !every("reinstall", Duration::from_secs(5)) {
        return false;
    }
    if let Some((mouse, keyboard)) = HOOKS.take() {
        unsafe {
            let _ = UnhookWindowsHookEx(mouse);
            let _ = UnhookWindowsHookEx(keyboard);
        }
    }
    if let Err(e) = install_hooks() {
        log::error!("could not watch the keyboard and the mouse again: {e}");
    }
    true
}

/// where the pointer is, physical pixels; `None` while another desktop
/// shows, the lock screen or a confirmation of the user account control
fn cursor_pos() -> Option<(i32, i32)> {
    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point) }.ok()?;
    Some((point.x, point.y))
}

/// Looks every [`WATCHDOG_PERIOD`] whether Windows still hands the mouse to
/// the hooks: the pointer moving although they were not called, or while
/// the mouse hook keeps it in place, tells that it does not. Over a program
/// run as administrator that is Windows keeping it from RustInput (User
/// Interface Privilege Isolation); otherwise the hooks were removed and are
/// installed again.
fn watchdog() {
    if let Some(pos) = WATCH.with_borrow(|w| w.release_pending) {
        if try_send_event(pos, CaptureEvent::Released).is_ok() {
            WATCH.with_borrow_mut(|w| w.release_pending = None);
        }
    }
    let Some(at) = cursor_pos() else {
        return;
    };
    let calls = MOUSE_CALLS.get();
    let (unseen, held_at) = WATCH.with_borrow_mut(|w| {
        let moved = w.cursor.is_some_and(|c| c != at);
        let seen = w.calls != calls;
        w.cursor = Some(at);
        w.calls = calls;
        w.unseen = if moved && !seen { w.unseen + 1 } else { 0 };
        (w.unseen, w.held_at)
    });
    match ACTIVE_CLIENT.get() {
        Some(pos) => match held_at {
            None => WATCH.with_borrow_mut(|w| w.held_at = Some(at)),
            Some(held) if (held.0 - at.0).abs() > 2 || (held.1 - at.1).abs() > 2 => {
                match process::under(at).filter(Program::out_of_reach) {
                    Some(program) => give_back(
                        pos,
                        &format!(
                            "the pointer moved here, over {}: Windows does not hand \
                             the mouse to RustInput over a program run as administrator",
                            program.describe()
                        ),
                    ),
                    None => {
                        give_back(
                            pos,
                            "the pointer moved here: Windows stopped calling the mouse \
                             hook of RustInput, it is installed again",
                        );
                        reinstall_hooks();
                    }
                }
            }
            Some(_) => {}
        },
        // half a second of movement the hook did not see
        None if unseen >= 2 => {
            WATCH.with_borrow_mut(|w| w.unseen = 0);
            match process::under(at).filter(Program::out_of_reach) {
                Some(program) => {
                    if every(&program.name, Duration::from_secs(60)) {
                        log::warn!(
                            "the pointer is over {}: Windows does not hand the mouse to \
                             RustInput over a program run as administrator, from there it \
                             can not go to another computer",
                            program.describe()
                        );
                    }
                }
                None => {
                    if reinstall_hooks() && every("unseen", Duration::from_secs(60)) {
                        log::warn!(
                            "the mouse moves but Windows does not call the mouse hook of \
                             RustInput any more: it is installed again"
                        );
                    }
                }
            }
        }
        None => {}
    }
}

/// Windows stopped handing the input to the hooks while capturing: the
/// keyboard and the mouse are given back to this computer, and the service
/// told so, which lets go of the keys on the other computer.
fn give_back(pos: Position, why: &str) {
    ACTIVE_CLIENT.take();
    HELD_KEYS.with_borrow_mut(HashSet::clear);
    WATCH.with_borrow_mut(|w| {
        w.held_at = None;
        w.unseen = 0;
    });
    log::warn!("the keyboard and the mouse are back on this computer: {why}");
    if try_send_event(pos, CaptureEvent::Released).is_err() {
        WATCH.with_borrow_mut(|w| w.release_pending = Some(pos));
    }
}

unsafe extern "system" fn foreground_changed(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    _object: i32,
    _child: i32,
    _thread: u32,
    _time: u32,
) {
    in_front(hwnd);
}

/// `hwnd` came to the front
fn in_front(hwnd: HWND) {
    let program = process::of_window(hwnd);
    let out_of_reach = program.clone().filter(Program::out_of_reach);
    let before = FOREGROUND.replace(out_of_reach.clone());
    match (&out_of_reach, &before) {
        (Some(now), _) if before.as_ref() != Some(now) => {
            log::warn!(
                "in front now: {}. While it is in front, Windows does not hand the \
                 keyboard to RustInput",
                now.describe()
            );
            if let Some(pos) = ACTIVE_CLIENT.get() {
                give_back(
                    pos,
                    &format!(
                        "{} came to the front, and what is typed would go to it",
                        now.describe()
                    ),
                );
            }
        }
        (Some(_), _) => {}
        (None, Some(_)) => log::info!(
            "in front now: {}, the keyboard reaches RustInput again",
            program.map_or("a window of RustInput".to_owned(), |p| p.describe())
        ),
        (None, None) => log::debug!(
            "in front now: {}",
            program.map_or("a window of RustInput".to_owned(), |p| p.describe())
        ),
    }
}

static DISPLAY_RESOLUTION_GENERATION: AtomicI32 = AtomicI32::new(1);

fn update_display_regions(displays: &mut Vec<RECT>, generation: &mut i32) {
    let global_generation = DISPLAY_RESOLUTION_GENERATION.load(Ordering::Acquire);
    if *generation != global_generation {
        enumerate_displays(displays);
        log::debug!("displays: {displays:?}");
        *generation = global_generation;
    }
}

fn enumerate_displays(display_rects: &mut Vec<RECT>) {
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

            display_rects.push(RECT {
                left: x,
                right: x + width as i32,
                top: y,
                bottom: y + height as i32,
            });
        }
    }
}

fn update_clients(request: ClientUpdate) {
    match request {
        ClientUpdate::Create(pos) => {
            CLIENTS.with_borrow_mut(|clients| clients.insert(pos));
        }
        ClientUpdate::Destroy(pos) => {
            if let Some(active_pos) = ACTIVE_CLIENT.get() {
                if pos == active_pos {
                    let _ = ACTIVE_CLIENT.take();
                }
            }
            CLIENTS.with_borrow_mut(|clients| clients.remove(&pos));
            SPANS.with_borrow_mut(|spans| spans.remove(&pos));
        }
        ClientUpdate::Spans(pos, new) => {
            SPANS.with_borrow_mut(|spans| spans.insert(pos, new));
        }
    }
}

/// records a key press or release, returns whether a press is an auto-repeat
fn is_repeat(held: &mut HashSet<u32>, key: u32, state: u8) -> bool {
    if state == 1 {
        !held.insert(key)
    } else {
        held.remove(&key);
        false
    }
}

fn to_key_event(wparam: WPARAM, lparam: LPARAM) -> Option<KeyboardEvent> {
    let kybrdllhookstruct: KBDLLHOOKSTRUCT = unsafe { *(lparam.0 as *const KBDLLHOOKSTRUCT) };
    // keys of media and of starting programs by their virtual key: some
    // keyboards report no scancode for them
    let linux_scan_code = match u16::try_from(kybrdllhookstruct.vkCode)
        .ok()
        .and_then(scancode::linux_key_of_windows_virtual_key)
    {
        Some(key) => key,
        None => linux_key_of_scancode(&kybrdllhookstruct)?,
    };
    log::trace!("linux key: {linux_scan_code:?}");
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

fn linux_key_of_scancode(kybrdllhookstruct: &KBDLLHOOKSTRUCT) -> Option<Linux> {
    let mut scan_code = kybrdllhookstruct.scanCode;
    log::trace!("scan_code: {scan_code}");
    if kybrdllhookstruct.flags.contains(LLKHF_EXTENDED) {
        scan_code |= 0xE000;
    }
    let Ok(win_scan_code) = scancode::Windows::try_from(scan_code) else {
        log::warn!(
            "failed to translate to windows scancode: {scan_code:#x} (virtual key {:#x})",
            kybrdllhookstruct.vkCode
        );
        return None;
    };
    log::trace!("windows_scan: {win_scan_code:?}");
    let Ok(linux_scan_code): Result<Linux, ()> = win_scan_code.try_into() else {
        log::warn!("failed to translate into linux scancode: {win_scan_code:?}");
        return None;
    };
    Some(linux_scan_code)
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
            let (x, y) = (mouse_low_level.pt.x, mouse_low_level.pt.y);
            let (ex, ey) = ENTRY_POINT.get();
            let (dx, dy) = (x - ex, y - ey);
            let (dx, dy) = (dx as f64, dy as f64);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_repeat_is_recognized() {
        let mut held = HashSet::new();
        assert!(!is_repeat(&mut held, 30, 1));
        // the key stays down: Windows sends more presses
        assert!(is_repeat(&mut held, 30, 1));
        assert!(is_repeat(&mut held, 30, 1));
        // another key in between is a real press
        assert!(!is_repeat(&mut held, 29, 1));
        assert!(!is_repeat(&mut held, 30, 0));
        // pressed again after the release
        assert!(!is_repeat(&mut held, 30, 1));
        // releases are never repeats
        assert!(!is_repeat(&mut held, 29, 0));
        assert!(!is_repeat(&mut held, 29, 0));
    }
}
