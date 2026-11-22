//! The numbers on Windows: a layered window with per pixel alpha in the
//! middle of each monitor, topmost and never activated, on a thread of its
//! own with its own message loop.

use std::{iter, ptr, sync::OnceLock, time::Duration};

use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
    Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromPoint},
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI,
            SetThreadDpiAwarenessContext,
        },
        WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
            HWND_TOPMOST, KillTimer, MA_NOACTIVATE, MSG, RegisterClassW, SW_SHOWNOACTIVATE,
            SWP_NOACTIVATE, SWP_SHOWWINDOW, SetTimer, SetWindowPos, ShowWindow, TranslateMessage,
            WM_LBUTTONDOWN, WM_MOUSEACTIVATE, WM_RBUTTONDOWN, WM_TIMER, WNDCLASSW, WS_EX_LAYERED,
            WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
        },
    },
};

use super::{Card, place, render};
use crate::radial::{draw::bgra, fonts::Fonts, windows::update_layered};

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(iter::once(0)).collect()
}

pub(super) fn run(name: &str, cards: &[Card], shown: Duration) -> Result<(), String> {
    static CLASS: OnceLock<Vec<u16>> = OnceLock::new();
    static REGISTERED: OnceLock<bool> = OnceLock::new();
    let class = CLASS.get_or_init(|| wide("RustInputIdentifyMonitors"));
    let font = Fonts::load(rustinput_i18n::current());
    unsafe {
        // the windows and the monitors in physical pixels
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let instance = GetModuleHandleW(ptr::null());
        let registered = *REGISTERED.get_or_init(|| {
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
            RegisterClassW(&window_class) != 0
        });
        if !registered {
            return Err("could not register the window class".into());
        }
        let mut windows: Vec<HWND> = Vec::new();
        for card in cards {
            let m = &card.monitor;
            let middle = POINT {
                x: m.x + m.width / 2,
                y: m.y + m.height / 2,
            };
            let monitor = MonitorFromPoint(middle, MONITOR_DEFAULTTONEAREST);
            let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
            GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
            let pixmap = render(card, name, dpi_x as f32 / 96.0, font.as_ref());
            let size = (pixmap.width() as i32, pixmap.height() as i32);
            let (x, y) = place(m, size);
            let title = wide(&format!("RustInput monitor {}", card.number));
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                class.as_ptr(),
                title.as_ptr(),
                WS_POPUP,
                x,
                y,
                size.0,
                size.1,
                ptr::null_mut(),
                ptr::null_mut(),
                instance,
                ptr::null(),
            );
            if hwnd.is_null() {
                continue;
            }
            update_layered(hwnd, (x, y), size, &bgra(&pixmap));
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                x,
                y,
                size.0,
                size.1,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            windows.push(hwnd);
        }
        if windows.is_empty() {
            return Err("could not create the windows".into());
        }
        // a timer of the thread: its message comes without a window
        let timer = SetTimer(ptr::null_mut(), 0, shown.as_millis() as u32, None);
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
            if msg.message == WM_TIMER && msg.hwnd.is_null() && msg.wParam == timer {
                break;
            }
            // a click hides them
            if matches!(msg.message, WM_LBUTTONDOWN | WM_RBUTTONDOWN) && windows.contains(&msg.hwnd)
            {
                break;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        KillTimer(ptr::null_mut(), timer);
        for hwnd in windows {
            DestroyWindow(hwnd);
        }
    }
    Ok(())
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
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}
