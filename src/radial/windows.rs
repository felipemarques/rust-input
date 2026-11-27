//! The wheel on Windows.
//!
//! A low level keyboard hook watches the keys and always passes them on. The
//! wheel is a layered window with per pixel alpha, topmost, never activated,
//! centered on the pointer within its monitor; `ClipCursor` keeps the pointer
//! in it while it shows.
//!
//! Everything runs on one thread with its own message loop. The hook only
//! queues the keys and posts a message, and the window procedure only touches
//! the state for messages that are posted, never for those Windows sends while
//! a window is shown or moved, so the state is never borrowed twice.

use std::{
    cell::RefCell,
    iter, ptr,
    sync::{Arc, OnceLock},
    time::Instant,
};

use tokio::sync::mpsc::UnboundedSender;
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM},
    Graphics::Gdi::{
        AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
        CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC,
        GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint, ReleaseDC,
        SelectObject,
    },
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI,
            SetThreadDpiAwarenessContext,
        },
        Input::KeyboardAndMouse::{VK_ESCAPE, VK_LCONTROL, VK_LMENU, VK_RCONTROL},
        WindowsAndMessaging::{
            CallNextHookEx, ClipCursor, CreateWindowExW, DefWindowProcW, DispatchMessageW,
            GetCursorPos, GetMessageW, HWND_TOPMOST, KBDLLHOOKSTRUCT, KillTimer, MA_NOACTIVATE,
            MSG, PostMessageW, RegisterClassW, SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE,
            SWP_SHOWWINDOW, SetCursorPos, SetTimer, SetWindowPos, SetWindowsHookExW, ShowWindow,
            TranslateMessage, ULW_ALPHA, UpdateLayeredWindow, WH_KEYBOARD_LL, WM_APP, WM_KEYDOWN,
            WM_MOUSEACTIVATE, WM_SYSKEYDOWN, WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
            WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
        },
    },
};

use crate::hotkeys::{self, Hotkeys};

use super::{
    Action, Fonts, Menu, Texts, Wheel,
    chord::{Chord, Key, Step},
    fonts_now,
};

/// posted by the hook: keys are waiting in [`KEYS`]
const WM_KEYS: u32 = WM_APP + 1;
const TIMER: usize = 1;
/// how often the pointer is looked at while the wheel shows, in ms
const FRAME: u32 = 16;
/// how often the keys are looked at while Ctrl+Alt are held, in ms
const WAITING: u32 = 30;

/// an open wheel
struct Open {
    wheel: Wheel,
    /// top left corner of the window, physical pixels
    at: (i32, i32),
    /// where the pointer was, it goes back there
    start: POINT,
}

struct State {
    hwnd: HWND,
    chord: Chord,
    hotkeys: Hotkeys,
    open: Option<Open>,
    menu: Menu,
    chosen: UnboundedSender<Action>,
    texts: Texts,
    font: Option<Arc<Fonts>>,
    /// the interval the timer runs at, if it runs
    timer: Option<u32>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    /// keys the hook saw, for the window procedure
    static KEYS: RefCell<Vec<(Key, hotkeys::Key, bool)>> = const { RefCell::new(Vec::new()) };
    static WINDOW: RefCell<HWND> = const { RefCell::new(ptr::null_mut()) };
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(iter::once(0)).collect()
}

pub(super) fn run(menu: Menu, chosen: UnboundedSender<Action>) -> Result<(), String> {
    static CLASS: OnceLock<Vec<u16>> = OnceLock::new();
    let class = CLASS.get_or_init(|| wide("RustInputRadialMenu"));
    let title = wide("RustInput menu");
    unsafe {
        // the window, the pointer and the monitors in physical pixels
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let instance = GetModuleHandleW(ptr::null());
        let window_class = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(window_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: ptr::null_mut(),
            hCursor: ptr::null_mut(),
            hbrBackground: ptr::null_mut(),
            lpszMenuName: ptr::null(),
            lpszClassName: class.as_ptr(),
        };
        if RegisterClassW(&window_class) == 0 {
            return Err("could not register the window class".into());
        }
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            0,
            0,
            1,
            1,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            ptr::null(),
        );
        if hwnd.is_null() {
            return Err("could not create the window".into());
        }
        WINDOW.with_borrow_mut(|w| *w = hwnd);
        STATE.with_borrow_mut(|state| {
            *state = Some(State {
                hwnd,
                chord: Chord::default(),
                hotkeys: Hotkeys::default(),
                open: None,
                menu,
                chosen,
                texts: Texts::new(),
                font: None,
                timer: None,
            })
        });
        if SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard), instance, 0).is_null() {
            return Err("could not watch the keyboard".into());
        }
        log::info!("radial menu: hold Ctrl and the left Alt");
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

/// the low level keyboard hook: queues the key and passes it on
unsafe extern "system" fn keyboard(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let info = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        let pressed = matches!(wparam as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
        // the right Alt is AltGr on many layouts, which types characters
        let key = match info.vkCode as u16 {
            VK_LCONTROL | VK_RCONTROL => Key::Ctrl,
            VK_LMENU => Key::Alt,
            VK_ESCAPE => Key::Escape,
            _ => Key::Other,
        };
        let shortcut = hotkeys::Key::from_windows(info.vkCode as u16);
        KEYS.with_borrow_mut(|keys| keys.push((key, shortcut, pressed)));
        let hwnd = WINDOW.with_borrow(|w| *w);
        unsafe { PostMessageW(hwnd, WM_KEYS, 0, 0) };
    }
    unsafe { CallNextHookEx(ptr::null_mut(), code, wparam, lparam) }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        // never take the focus, also not when clicked
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        // posted, so never while the state is in use
        WM_KEYS | WM_TIMER => {
            STATE.with_borrow_mut(|state| {
                if let Some(state) = state {
                    if message == WM_KEYS {
                        state.keys();
                    } else {
                        state.tick();
                    }
                    state.schedule();
                }
            });
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

impl State {
    fn keys(&mut self) {
        let keys = KEYS.with_borrow_mut(std::mem::take);
        for (key, shortcut, pressed) in keys {
            self.menu
                .shortcut(&mut self.hotkeys, shortcut, pressed, &self.chosen);
            let step = self.chord.key(key, pressed, Instant::now());
            self.step(step);
        }
    }

    fn tick(&mut self) {
        let step = self.chord.tick(Instant::now(), self.menu.away());
        self.step(step);
        if self.open.is_some() {
            self.follow_pointer();
        }
    }

    /// runs the timer as often as needed, or not at all
    fn schedule(&mut self) {
        let wanted = if self.open.is_some() {
            Some(FRAME)
        } else if self.chord.pending() {
            Some(WAITING)
        } else {
            None
        };
        if wanted == self.timer {
            return;
        }
        unsafe {
            match wanted {
                Some(ms) => {
                    SetTimer(self.hwnd, TIMER, ms, None);
                }
                None => {
                    KillTimer(self.hwnd, TIMER);
                }
            }
        }
        self.timer = wanted;
    }

    fn step(&mut self, step: Step) {
        match step {
            Step::Nothing => {}
            // only the shortcuts are watched
            Step::Open if !self.menu.wheel() => {}
            Step::Open => self.show(),
            Step::Pick => self.close(true),
            Step::Cancel => self.close(false),
        }
    }

    fn show(&mut self) {
        let items = self.menu.items();
        if items.is_empty() || self.open.is_some() {
            return;
        }
        let mut start = POINT { x: 0, y: 0 };
        if unsafe { GetCursorPos(&mut start) } == 0 {
            return;
        }
        let (area, dpi) = unsafe {
            let monitor = MonitorFromPoint(start, MONITOR_DEFAULTTONEAREST);
            let mut info: MONITORINFO = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            GetMonitorInfoW(monitor, &mut info);
            let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
            GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
            (info.rcMonitor, dpi_x)
        };
        let wheel = Wheel::new(
            items,
            dpi as f32 / 96.0,
            &self.texts,
            fonts_now(&mut self.font),
        );
        let size = wheel.size() as i32;
        // centered on the pointer, within its monitor
        let x = (start.x - size / 2).clamp(area.left, (area.right - size).max(area.left));
        let y = (start.y - size / 2).clamp(area.top, (area.bottom - size).max(area.top));
        let open = Open {
            wheel,
            at: (x, y),
            start,
        };
        self.draw(&open);
        unsafe {
            ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            SetWindowPos(
                self.hwnd,
                HWND_TOPMOST,
                x,
                y,
                size,
                size,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            // the pointer stays in the wheel: it must not cross to another
            // device
            let clip = RECT {
                left: x,
                top: y,
                right: x + size,
                bottom: y + size,
            };
            ClipCursor(&clip);
        }
        self.open = Some(open);
    }

    fn draw(&self, open: &Open) {
        let size = open.wheel.size() as i32;
        update_layered(self.hwnd, open.at, (size, size), &open.wheel.pixels());
    }

    fn follow_pointer(&mut self) {
        let mut at = POINT { x: 0, y: 0 };
        if unsafe { GetCursorPos(&mut at) } == 0 {
            return;
        }
        let Some(mut open) = self.open.take() else {
            return;
        };
        let size = open.wheel.size() as i32;
        let dx = (at.x - (open.at.0 + size / 2)) as f64;
        let dy = (at.y - (open.at.1 + size / 2)) as f64;
        if open.wheel.point(dx, dy) {
            self.draw(&open);
        }
        self.open = Some(open);
    }

    /// hides the wheel, with the action pointed at if `pick`
    fn close(&mut self, pick: bool) {
        let Some(open) = self.open.take() else {
            return;
        };
        let action = pick.then(|| open.wheel.chosen()).flatten();
        unsafe {
            ClipCursor(ptr::null());
            ShowWindow(self.hwnd, SW_HIDE);
            // the pointer goes back to where it was
            SetCursorPos(open.start.x, open.start.y);
        }
        if let Some(action) = action {
            log::info!("radial menu: {action:?}");
            let _ = self.chosen.send(action);
        }
    }
}

/// Shows the premultiplied BGRA `pixels` in the layered window `hwnd`, its
/// top left corner at `at` and of `size`, in physical pixels.
pub(crate) fn update_layered(hwnd: HWND, at: (i32, i32), size: (i32, i32), pixels: &[u8]) {
    let (width, height) = size;
    unsafe {
        let screen = GetDC(ptr::null_mut());
        let memory = CreateCompatibleDC(screen);
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            // negative: rows from the top down, like the pixels
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        };
        let mut bits: *mut std::ffi::c_void = ptr::null_mut();
        let bitmap = CreateDIBSection(memory, &info, DIB_RGB_COLORS, &mut bits, ptr::null_mut(), 0);
        let length = (width * height * 4) as usize;
        if !bitmap.is_null() && !bits.is_null() && pixels.len() >= length {
            ptr::copy_nonoverlapping(pixels.as_ptr(), bits as *mut u8, length);
            let previous = SelectObject(memory, bitmap);
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let to = POINT { x: at.0, y: at.1 };
            let extent = SIZE {
                cx: width,
                cy: height,
            };
            let from = POINT { x: 0, y: 0 };
            UpdateLayeredWindow(
                hwnd, screen, &to, &extent, memory, &from, 0, &blend, ULW_ALPHA,
            );
            SelectObject(memory, previous);
            DeleteObject(bitmap);
        }
        DeleteDC(memory);
        ReleaseDC(ptr::null_mut(), screen);
    }
}
