//! Locking the screen of this computer, as Win+L does on Windows: from the
//! radial menu ("Lock all computers"), or because another computer asked
//! for it ([`crate::arrangement`]).
//!
//! - Windows: `LockWorkStation`, from the daemon in the session at the
//!   screen;
//! - macOS: `SACLockScreenImmediate` (what the menu of the user does), else
//!   the display sleeps (`pmset displaysleepnow`), which locks it when a
//!   password is asked right after;
//! - Linux and the BSDs: the screen locker of the desktop
//!   (`XDG_CURRENT_DESKTOP`), else `loginctl lock-session`, to which GNOME,
//!   KDE and most others listen, else `xdg-screensaver lock`.
//!
//! The screen locker can not take a mouse RustInput holds: the service
//! brings the pointer back to this computer first.
//!
//! Windows refuses the keyboard and the mouse of RustInput on its lock
//! screen ([`screen_locked`]): another computer does not enter it then, and
//! one controlling it gets its pointer back, rather than lose it there.

#[cfg(unix)]
use std::time::{Duration, Instant};

/// longest a command to lock the screen may take
#[cfg(unix)]
const WAIT: Duration = Duration::from_secs(5);

/// locks the screen of this computer now; why not when it could not
#[cfg(windows)]
pub(crate) fn lock_screen() -> Result<(), String> {
    use windows_sys::Win32::System::Shutdown::LockWorkStation;
    // SAFETY: no arguments; it only asks Windows to show the lock screen
    if unsafe { LockWorkStation() } != 0 {
        Ok(())
    } else {
        Err(format!(
            "LockWorkStation: {}",
            std::io::Error::last_os_error()
        ))
    }
}

/// Whether the lock screen of Windows shows (in the session of the daemon,
/// the one at the screen). Not known counts as not locked, as before this
/// was asked. The flags are the other way around before Windows 10, which
/// Rust no longer runs on.
#[cfg(windows)]
pub(crate) fn screen_locked() -> bool {
    use windows_sys::Win32::System::RemoteDesktop::{
        WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION, WTS_SESSIONSTATE_LOCK, WTSFreeMemory,
        WTSINFOEXW, WTSQuerySessionInformationW, WTSSessionInfoEx,
    };
    let mut buffer: *mut u16 = std::ptr::null_mut();
    let mut bytes = 0u32;
    // SAFETY: Windows allocates the buffer, read only when it is as large
    // as asked, and freed by WTSFreeMemory
    unsafe {
        let asked = WTSQuerySessionInformationW(
            WTS_CURRENT_SERVER_HANDLE,
            WTS_CURRENT_SESSION,
            WTSSessionInfoEx,
            &mut buffer,
            &mut bytes,
        );
        if asked == 0 || buffer.is_null() {
            return false;
        }
        let locked = bytes as usize >= std::mem::size_of::<WTSINFOEXW>() && {
            let info = &*(buffer as *const WTSINFOEXW);
            info.Level == 1
                && info.Data.WTSInfoExLevel1.SessionFlags == WTS_SESSIONSTATE_LOCK as i32
        };
        WTSFreeMemory(buffer.cast());
        locked
    }
}

/// Whether the lock screen shows: only Windows refuses the input of
/// RustInput there; elsewhere the screen locker takes it like any other.
#[cfg(not(windows))]
pub(crate) fn screen_locked() -> bool {
    false
}

/// locks the screen of this computer now; why not when it could not
#[cfg(target_os = "macos")]
pub(crate) fn lock_screen() -> Result<(), String> {
    use std::ffi::c_void;
    const LOGIN: &std::ffi::CStr =
        c"/System/Library/PrivateFrameworks/login.framework/Versions/Current/login";
    // SAFETY: the framework stays loaded (never closed), and the function
    // takes nothing and returns an int, as in the headers of Apple
    let locked = unsafe {
        let login = libc::dlopen(LOGIN.as_ptr(), libc::RTLD_LAZY);
        let function = match login.is_null() {
            true => std::ptr::null_mut(),
            false => libc::dlsym(login, c"SACLockScreenImmediate".as_ptr()),
        };
        !function.is_null() && {
            let lock = std::mem::transmute::<*mut c_void, extern "C" fn() -> i32>(function);
            lock() == 0
        }
    };
    match locked {
        true => Ok(()),
        false => run(&["pmset", "displaysleepnow"]),
    }
}

/// locks the screen of this computer now; why not when it could not
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn lock_screen() -> Result<(), String> {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let mut errors = Vec::new();
    for command in commands(&desktop) {
        match run(command) {
            Ok(()) => {
                log::info!("screen locked with {}", command.join(" "));
                return Ok(());
            }
            Err(e) => errors.push(e),
        }
    }
    Err(format!("no screen locker answered ({})", errors.join("; ")))
}

/// The commands that lock the screen, in the order to try them: those of
/// `desktop` (`XDG_CURRENT_DESKTOP`, a list like `ubuntu:GNOME`) first,
/// then those of logind and of xdg-utils. `loginctl` succeeds even when
/// nothing listens, so it comes after the lockers of the desktops that do
/// not listen to it.
#[cfg(all(unix, not(target_os = "macos")))]
fn commands(desktop: &str) -> Vec<&'static [&'static str]> {
    let mut commands: Vec<&'static [&'static str]> = Vec::new();
    for name in desktop.split(':') {
        let name = name.trim().to_ascii_lowercase();
        let command: &'static [&'static str] = match name.trim_start_matches("x-") {
            "cinnamon" => &["cinnamon-screensaver-command", "--lock"],
            "mate" => &["mate-screensaver-command", "--lock"],
            "xfce" => &["xflock4"],
            "lxqt" => &["lxqt-leave", "--lockscreen"],
            _ => continue,
        };
        if !commands.contains(&command) {
            commands.push(command);
        }
    }
    commands.push(&["loginctl", "lock-session"]);
    commands.push(&["xdg-screensaver", "lock"]);
    commands
}

/// runs `command`, waiting at most [`WAIT`] for it
#[cfg(unix)]
fn run(command: &[&str]) -> Result<(), String> {
    use std::{
        io::Read,
        process::{Command, Stdio},
    };
    let name = command[0];
    let mut child = Command::new(name)
        .args(&command[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{name}: {e}"))?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stderr.take() {
                    let _ = pipe.read_to_string(&mut stderr);
                }
                let why = stderr
                    .lines()
                    .map(str::trim)
                    .rfind(|l| !l.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| status.to_string());
                return Err(format!("{name}: {why}"));
            }
            Ok(None) if start.elapsed() < WAIT => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{name} did not answer"));
            }
            Err(e) => return Err(format!("{name}: {e}")),
        }
    }
}

#[cfg(all(test, unix, not(target_os = "macos")))]
mod tests {
    use super::*;

    #[test]
    fn the_locker_of_the_desktop_comes_first() {
        let first = |desktop: &str| commands(desktop)[0][0];
        assert_eq!(first("X-Cinnamon"), "cinnamon-screensaver-command");
        assert_eq!(first("MATE"), "mate-screensaver-command");
        assert_eq!(first("XFCE"), "xflock4");
        // GNOME and KDE listen to logind
        assert_eq!(first("ubuntu:GNOME"), "loginctl");
        assert_eq!(first("KDE"), "loginctl");
        assert_eq!(first(""), "loginctl");
        assert_eq!(commands("X-Cinnamon").len(), 3);
        assert_eq!(commands("X-Cinnamon:X-Cinnamon").len(), 3);
        assert_eq!(commands("").last().unwrap()[0], "xdg-screensaver");
    }

    #[test]
    fn a_failed_command_says_why() {
        assert!(run(&["true"]).is_ok());
        let e = run(&["sh", "-c", "echo 'no screensaver running' >&2; exit 3"]).unwrap_err();
        assert_eq!(e, "sh: no screensaver running");
        assert!(run(&["rustinput-no-such-command"]).is_err());
    }
}
