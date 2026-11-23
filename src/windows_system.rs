//! The daemon as SYSTEM (0.31.0, for an administrator) works in the folders
//! of its user, `%LOCALAPPDATA%\rustinput`: configuration, certificate, logs,
//! received files. The programs of that user may change those folders under
//! it, also the ones without administrator rights: replacing a folder by a
//! junction they could make SYSTEM delete, create or overwrite files
//! elsewhere, and having files sent they could read what the user may not.
//! So, since 0.32.1:
//!
//! - Redirection Guard ([`guard_redirections`], Windows 11 and Server 2022):
//!   the process follows no junction made by a user who is not an
//!   administrator;
//! - files to send are listed and opened as the user logged on in the
//!   session ([`as_session_user`]), with the rights of that user;
//! - the configuration and the certificate are written to a new file renamed
//!   over the old one, and the log of the daemon is a new file: none follows a
//!   hard link put in their place (`config.rs`, `crypto.rs`, `windows_log.rs`);
//! - commands of the configuration (hooks) do not run with administrator
//!   rights ([`runs_elevated`]).

use std::{
    ffi::c_void,
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    ptr,
};

use windows_sys::Win32::{
    Foundation::HANDLE,
    Security::{
        GetTokenInformation, ImpersonateLoggedOnUser, RevertToSelf, TOKEN_ELEVATION, TOKEN_QUERY,
        TokenElevation,
    },
    System::{
        RemoteDesktop::{ProcessIdToSessionId, WTSQueryUserToken},
        SystemServices::PROCESS_MITIGATION_REDIRECTION_TRUST_POLICY,
        Threading::{
            GetCurrentProcess, GetCurrentProcessId, OpenProcessToken,
            ProcessRedirectionTrustPolicy, SetProcessMitigationPolicy,
        },
    },
};

/// Turns Redirection Guard on when RustInput runs as SYSTEM: no junction made
/// by a user who is not an administrator is followed. `None` when it does not
/// run as SYSTEM, an error where Windows does not have it (before Windows 11
/// and Server 2022).
pub fn guard_redirections() -> Option<io::Result<()>> {
    if !crate::lock::runs_as_system() {
        return None;
    }
    let mut policy = PROCESS_MITIGATION_REDIRECTION_TRUST_POLICY::default();
    // EnforceRedirectionTrust, the first bit
    policy.Anonymous.Flags = 1;
    let set = unsafe {
        SetProcessMitigationPolicy(
            ProcessRedirectionTrustPolicy,
            &policy as *const _ as *const c_void,
            size_of::<PROCESS_MITIGATION_REDIRECTION_TRUST_POLICY>(),
        )
    };
    Some(match set {
        0 => Err(io::Error::last_os_error()),
        _ => Ok(()),
    })
}

/// Runs `f` as the user logged on in the session of RustInput when it runs
/// as SYSTEM (with the rights of that user: what it reads, the user could
/// read), as itself otherwise. An error when nobody is logged on. `f` runs
/// on this thread and must not wait on anything that moves it to another.
pub(crate) fn as_session_user<T>(f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    if !crate::lock::runs_as_system() {
        return f();
    }
    let mut session = 0;
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) };
    let mut token: HANDLE = ptr::null_mut();
    if unsafe { WTSQueryUserToken(session, &mut token) } == 0 {
        let e = io::Error::last_os_error();
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("nobody is logged on in session {session}: {e}"),
        ));
    }
    let token = unsafe { OwnedHandle::from_raw_handle(token as _) };
    if unsafe { ImpersonateLoggedOnUser(token.as_raw_handle() as HANDLE) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let _back = Revert;
    f()
}

/// back to SYSTEM when dropped, also when `f` panics
struct Revert;

impl Drop for Revert {
    fn drop(&mut self) {
        if unsafe { RevertToSelf() } == 0 {
            // never go on as someone else
            std::process::abort();
        }
    }
}

/// Whether RustInput runs with administrator rights (as SYSTEM too): then
/// it runs no command of the configuration, which any program of the user
/// can change, with those rights.
pub(crate) fn runs_elevated() -> bool {
    let mut token: HANDLE = ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let token = unsafe { OwnedHandle::from_raw_handle(token as _) };
    let mut elevation = TOKEN_ELEVATION::default();
    let mut size = 0;
    let read = unsafe {
        GetTokenInformation(
            token.as_raw_handle() as HANDLE,
            TokenElevation,
            &mut elevation as *mut _ as *mut c_void,
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        )
    };
    read != 0 && elevation.TokenIsElevated != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_itself_unless_system() {
        // the tests do not run as SYSTEM
        assert!(guard_redirections().is_none());
        assert_eq!(as_session_user(|| Ok(7)).unwrap(), 7);
    }
}
