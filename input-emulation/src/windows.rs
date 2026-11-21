use super::error::{EmulationError, WindowsEmulationCreationError};
use input_event::{
    BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event, KeyboardEvent, PointerEvent,
    edge::{Area, Edge},
    scancode,
};

use async_trait::async_trait;
use std::cell::Cell;
use std::ops::BitOrAssign;
use std::time::{Duration, Instant};
use tokio::task::AbortHandle;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_WHEEL, MOUSEINPUT,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT_0, KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP,
    SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    SetCursorPos, XBUTTON1, XBUTTON2,
};

use super::{Emulation, EmulationHandle};

const DEFAULT_REPEAT_DELAY: Duration = Duration::from_millis(500);
const DEFAULT_REPEAT_INTERVAL: Duration = Duration::from_millis(32);

pub(crate) struct WindowsEmulation {
    repeat_task: Option<AbortHandle>,
}

impl WindowsEmulation {
    pub(crate) fn new() -> Result<Self, WindowsEmulationCreationError> {
        Ok(Self { repeat_task: None })
    }
}

#[async_trait]
impl Emulation for WindowsEmulation {
    async fn consume(&mut self, event: Event, _: EmulationHandle) -> Result<(), EmulationError> {
        match event {
            Event::Pointer(pointer_event) => match pointer_event {
                PointerEvent::Motion { time: _, dx, dy } => {
                    rel_mouse(dx as i32, dy as i32);
                }
                PointerEvent::Button {
                    time: _,
                    button,
                    state,
                } => mouse_button(button, state),
                PointerEvent::Axis {
                    time: _,
                    axis,
                    value,
                } => scroll(axis, value as i32),
                PointerEvent::AxisDiscrete120 { axis, value } => scroll(axis, value),
            },
            Event::Keyboard(keyboard_event) => match keyboard_event {
                KeyboardEvent::Key {
                    time: _,
                    key,
                    state,
                } => {
                    match state {
                        0 => self.kill_repeat_task(),
                        // pressed
                        1 if repeats(key) => self.spawn_repeat_task(key).await,
                        1 => self.kill_repeat_task(),
                        _ => {}
                    }
                    key_event(key, state)
                }
                KeyboardEvent::Modifiers { .. } => {}
            },
        }
        // FIXME
        Ok(())
    }

    async fn create(&mut self, _handle: EmulationHandle) {}

    async fn destroy(&mut self, _handle: EmulationHandle) {}

    async fn terminate(&mut self) {}

    async fn warp(&mut self, edge: Edge, fraction: f64) {
        unsafe {
            // physical pixels, also when the displays are scaled
            let previous = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let desktop = Area {
                x: GetSystemMetrics(SM_XVIRTUALSCREEN),
                y: GetSystemMetrics(SM_YVIRTUALSCREEN),
                width: GetSystemMetrics(SM_CXVIRTUALSCREEN),
                height: GetSystemMetrics(SM_CYVIRTUALSCREEN),
            };
            let (x, y) = desktop.point(edge, fraction);
            if let Err(e) = SetCursorPos(x, y) {
                log::warn!("could not place the pointer at ({x}, {y}): {e}");
            }
            if !previous.is_invalid() {
                SetThreadDpiAwarenessContext(previous);
            }
        }
    }
}

impl WindowsEmulation {
    async fn spawn_repeat_task(&mut self, key: u32) {
        // there can only be one repeating key and it's
        // always the last to be pressed
        self.kill_repeat_task();
        let repeat_task = tokio::task::spawn_local(async move {
            tokio::time::sleep(DEFAULT_REPEAT_DELAY).await;
            loop {
                key_event(key, 1);
                tokio::time::sleep(DEFAULT_REPEAT_INTERVAL).await;
            }
        });
        self.repeat_task = Some(repeat_task.abort_handle());
    }
    fn kill_repeat_task(&mut self) {
        if let Some(task) = self.repeat_task.take() {
            task.abort();
        }
    }
}

thread_local! {
    /// when it was last logged that Windows refused input
    static REFUSED_LOGGED: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Windows refuses input while the lock screen or a prompt of User Account
/// Control is up, which RustInput does not reach: the input is dropped then,
/// after a few tries, rather than tried again forever.
fn send_input_safe(input: INPUT) {
    for _ in 0..3 {
        /* retval = number of successfully submitted events */
        if unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) } > 0 {
            return;
        }
    }
    let error = std::io::Error::last_os_error();
    let now = Instant::now();
    let due = REFUSED_LOGGED
        .get()
        .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(10));
    if due {
        REFUSED_LOGGED.set(Some(now));
        log::warn!(
            "Windows refuses the keyboard and the mouse of RustInput ({error}): the lock \
             screen or a prompt of User Account Control is up"
        );
    }
}

fn send_mouse_input(mi: MOUSEINPUT) {
    send_input_safe(INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi },
    });
}

fn send_keyboard_input(ki: KEYBDINPUT) {
    send_input_safe(INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki },
    });
}
fn rel_mouse(dx: i32, dy: i32) {
    let mi = MOUSEINPUT {
        dx,
        dy,
        mouseData: 0,
        dwFlags: MOUSEEVENTF_MOVE,
        time: 0,
        dwExtraInfo: 0,
    };
    send_mouse_input(mi);
}

fn mouse_button(button: u32, state: u32) {
    let dw_flags = match state {
        0 => match button {
            BTN_LEFT => MOUSEEVENTF_LEFTUP,
            BTN_RIGHT => MOUSEEVENTF_RIGHTUP,
            BTN_MIDDLE => MOUSEEVENTF_MIDDLEUP,
            BTN_BACK => MOUSEEVENTF_XUP,
            BTN_FORWARD => MOUSEEVENTF_XUP,
            _ => return,
        },
        1 => match button {
            BTN_LEFT => MOUSEEVENTF_LEFTDOWN,
            BTN_RIGHT => MOUSEEVENTF_RIGHTDOWN,
            BTN_MIDDLE => MOUSEEVENTF_MIDDLEDOWN,
            BTN_BACK => MOUSEEVENTF_XDOWN,
            BTN_FORWARD => MOUSEEVENTF_XDOWN,
            _ => return,
        },
        _ => return,
    };
    let mouse_data = match button {
        BTN_BACK => XBUTTON1 as u32,
        BTN_FORWARD => XBUTTON2 as u32,
        _ => 0,
    };
    let mi = MOUSEINPUT {
        dx: 0,
        dy: 0, // no movement
        mouseData: mouse_data,
        dwFlags: dw_flags,
        time: 0,
        dwExtraInfo: 0,
    };
    send_mouse_input(mi);
}

fn scroll(axis: u8, value: i32) {
    // WHEEL is positive up but HWHEEL positive right, like rustinput's horizontal axis
    let (event_type, value) = match axis {
        0 => (MOUSEEVENTF_WHEEL, value.saturating_neg()),
        1 => (MOUSEEVENTF_HWHEEL, value),
        _ => return,
    };
    let mi = MOUSEINPUT {
        dx: 0,
        dy: 0,
        mouseData: value as u32,
        dwFlags: event_type,
        time: 0,
        dwExtraInfo: 0,
    };
    send_mouse_input(mi);
}

fn key_event(key: u32, state: u8) {
    let Ok(linux) = scancode::Linux::try_from(key) else {
        log::warn!("unknown keycode: {key}");
        return;
    };
    // "Flip" of Logitech keyboards, "show all windows": the task view
    if linux == scancode::Linux::KeyScale {
        let keys = [scancode::Windows::KeyLeftGUI, scancode::Windows::KeyTab];
        if state == 1 {
            keys.iter().for_each(|k| scancode_event(*k as u16, 1));
        } else {
            keys.iter().rev().for_each(|k| scancode_event(*k as u16, 0));
        }
        return;
    }
    if let Some(vk) = scancode::windows_virtual_key(linux) {
        virtual_key_event(vk, linux, state);
        return;
    }
    if let Some(code) = linux_keycode_to_windows_scancode(linux) {
        scancode_event(code, state);
    }
}

/// whether holding `key` repeats it: the volume does, starting a program or
/// opening the task view again and again would not help
fn repeats(key: u32) -> bool {
    let Ok(linux) = scancode::Linux::try_from(key) else {
        return true;
    };
    match linux {
        scancode::Linux::KeyVolumeUp | scancode::Linux::KeyVolumeDown => true,
        scancode::Linux::KeyScale => false,
        _ => scancode::windows_virtual_key(linux).is_none(),
    }
}

/// a key of media or of starting a program as its virtual key, with its
/// scancode for the hooks of other programs
fn virtual_key_event(vk: u16, linux: scancode::Linux, state: u8) {
    let scancode = scancode::Windows::try_from(linux).map_or(0, |s| s as u32);
    let mut flags = KEYBD_EVENT_FLAGS::default();
    if scancode > 0xff {
        flags.bitor_assign(KEYEVENTF_EXTENDEDKEY);
    }
    if state == 0 {
        flags.bitor_assign(KEYEVENTF_KEYUP);
    }
    send_keyboard_input(KEYBDINPUT {
        wVk: VIRTUAL_KEY(vk),
        wScan: (scancode & 0xff) as u16,
        dwFlags: flags,
        time: 0,
        dwExtraInfo: 0,
    });
}

fn scancode_event(scancode: u16, state: u8) {
    let extended = scancode > 0xff;
    let scancode = scancode & 0xff;
    let mut flags = KEYEVENTF_SCANCODE;
    if extended {
        flags.bitor_assign(KEYEVENTF_EXTENDEDKEY);
    }
    if state == 0 {
        flags.bitor_assign(KEYEVENTF_KEYUP);
    }
    let ki = KEYBDINPUT {
        wVk: Default::default(),
        wScan: scancode,
        dwFlags: flags,
        time: 0,
        dwExtraInfo: 0,
    };
    send_keyboard_input(ki);
}

fn linux_keycode_to_windows_scancode(linux_scancode: scancode::Linux) -> Option<u16> {
    log::trace!("linux code: {linux_scancode:?}");
    let windows_scancode = match scancode::Windows::try_from(linux_scancode) {
        Ok(s) => s,
        Err(_) => {
            log::warn!("failed to translate linux code into windows scancode: {linux_scancode:?}");
            return None;
        }
    };
    log::trace!("windows code: {windows_scancode:?}");
    Some(windows_scancode as u16)
}
