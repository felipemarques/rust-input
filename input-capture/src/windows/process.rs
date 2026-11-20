//! What the capture needs to know about processes: whether a window belongs
//! to a program run as administrator, which Windows keeps out of reach of
//! RustInput's hooks (User Interface Privilege Isolation), and keeping
//! Windows from slowing RustInput down while it runs in the background.

use std::{ffi::c_void, sync::OnceLock};

use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, HWND, POINT},
        Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation},
        System::Threading::{
            GetCurrentProcess, GetCurrentProcessId, OpenProcess, OpenProcessToken,
            PROCESS_NAME_WIN32, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
            PROCESS_QUERY_LIMITED_INFORMATION, ProcessPowerThrottling, QueryFullProcessImageNameW,
            SetProcessInformation,
        },
        UI::WindowsAndMessaging::{
            GA_ROOT, GetAncestor, GetWindowThreadProcessId, WindowFromPoint,
        },
    },
    core::PWSTR,
};

/// The program a window belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Program {
    /// file name, "Taskmgr.exe"
    pub(super) name: String,
    /// run as administrator: `None` if RustInput may not even look, which
    /// is the case for programs of the system and of other users
    pub(super) elevated: Option<bool>,
}

impl Program {
    /// whether Windows keeps the input going to this program from the hooks
    /// of RustInput
    pub(super) fn out_of_reach(&self) -> bool {
        !runs_as_administrator() && self.elevated != Some(false)
    }

    /// "Taskmgr.exe, run as administrator"
    pub(super) fn describe(&self) -> String {
        match self.elevated {
            Some(true) => format!("{}, run as administrator", self.name),
            Some(false) => self.name.clone(),
            None => format!("{}, of the system or run as administrator", self.name),
        }
    }
}

/// the program of the window at `at`, physical pixels
pub(super) fn under(at: (i32, i32)) -> Option<Program> {
    let hwnd = unsafe { WindowFromPoint(POINT { x: at.0, y: at.1 }) };
    of_window(hwnd)
}

/// the program `hwnd` belongs to, `None` for no window or one of RustInput
pub(super) fn of_window(hwnd: HWND) -> Option<Program> {
    if hwnd.0.is_null() {
        return None;
    }
    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    let hwnd = if root.0.is_null() { hwnd } else { root };
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 || pid == unsafe { GetCurrentProcessId() } {
        return None;
    }
    Some(of_process(pid))
}

fn of_process(pid: u32) -> Program {
    let unknown = || Program {
        name: format!("process {pid}"),
        elevated: None,
    };
    let Ok(process) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) })
    else {
        return unknown();
    };
    let mut path = [0u16; 1024];
    let mut len = path.len() as u32;
    let name = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(path.as_mut_ptr()),
            &mut len,
        )
    }
    .ok()
    .map(|_| {
        let path = String::from_utf16_lossy(&path[..len as usize]);
        path.rsplit('\\').next().unwrap_or(&path).to_owned()
    });
    let elevated = elevated(process);
    unsafe {
        let _ = CloseHandle(process);
    }
    Program {
        name: name.unwrap_or_else(|| unknown().name),
        elevated,
    }
}

/// whether `process` runs as administrator, `None` if it can not be told
fn elevated(process: HANDLE) -> Option<bool> {
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }.ok()?;
    let mut elevation = TOKEN_ELEVATION::default();
    let mut len = 0;
    let result = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut TOKEN_ELEVATION as *mut c_void),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
    };
    unsafe {
        let _ = CloseHandle(token);
    }
    result.ok()?;
    Some(elevation.TokenIsElevated != 0)
}

/// whether RustInput itself runs as administrator: then nothing is out of
/// its reach
pub(super) fn runs_as_administrator() -> bool {
    static ELEVATED: OnceLock<bool> = OnceLock::new();
    *ELEVATED.get_or_init(|| elevated(unsafe { GetCurrentProcess() }).unwrap_or(false))
}

/// Asks Windows not to slow this process down to save energy: it may do so
/// for a process without visible windows, and a hook that answers late is
/// skipped, and after a while removed, by Windows.
pub(super) fn keep_full_speed() {
    let mut state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED
            | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        StateMask: 0,
    };
    let set = |state: &PROCESS_POWER_THROTTLING_STATE| unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            state as *const PROCESS_POWER_THROTTLING_STATE as *const c_void,
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
    };
    if set(&state).is_err() {
        // Windows 10 does not know about the timer resolution
        state.ControlMask = PROCESS_POWER_THROTTLING_EXECUTION_SPEED;
        if let Err(e) = set(&state) {
            log::warn!("could not keep Windows from slowing RustInput down: {e}");
        }
    }
}
