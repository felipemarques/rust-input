//! The channel between the windows and the daemon on Windows: a named pipe
//! of the session, `\\.\pipe\rustinput-<session>`.
//!
//! Until 0.32.0 it was TCP on 127.0.0.1:5252, which every user of the
//! computer, every session and even web pages could reach, while the daemon
//! may run as SYSTEM or with the administrator rights of its user: anyone
//! could authorize a certificate, have files sent or set commands. Now:
//!
//! - the pipe refuses remote clients, and only SYSTEM, the administrators and
//!   interactive users may open it, none below medium integrity (its DACL and
//!   its label);
//! - the daemon checks every client before it reads or sends anything: its
//!   token (by impersonating it, at identification level) must be of the user
//!   the daemon runs as, of the user logged on in the session of the daemon
//!   (when it runs as SYSTEM) or of SYSTEM; in that session, at least medium
//!   integrity, not an app container;
//! - the windows open it at identification level only, so a fake daemon
//!   cannot act as them, and check its owner: SYSTEM, the administrators or
//!   themselves, so a pipe another user created first is refused.

use std::{
    ffi::c_void,
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    ptr,
};

use tokio::{
    net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions},
    sync::mpsc,
};
use windows_sys::Win32::{
    Foundation::{ERROR_SEM_TIMEOUT, HANDLE, LocalFree},
    Security::{
        Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
            SE_KERNEL_OBJECT,
        },
        EqualSid, GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, IsWellKnownSid,
        OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, RevertToSelf, SECURITY_ATTRIBUTES,
        TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TOKEN_USER,
        TokenIntegrityLevel, TokenIsAppContainer, TokenSessionId, TokenUser,
        WinBuiltinAdministratorsSid, WinLocalSystemSid,
    },
    System::{
        Pipes::{ImpersonateNamedPipeClient, WaitNamedPipeW},
        RemoteDesktop::{ProcessIdToSessionId, WTSQueryUserToken},
        SystemServices::SECURITY_MANDATORY_MEDIUM_RID,
        Threading::{
            GetCurrentProcess, GetCurrentProcessId, GetCurrentThread, OpenProcessToken,
            OpenThreadToken,
        },
    },
};

/// Full access for SYSTEM, the administrators and interactive users; the
/// label keeps processes below medium integrity from reading or writing.
const SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;IU)S:(ML;;NWNR;;;ME)";

/// the name of the pipe of the session of this process
pub fn pipe_name() -> String {
    pipe_name_in(own_session())
}

/// the name of the pipe of the daemon of `session`
pub fn pipe_name_in(session: u32) -> String {
    format!(r"\\.\pipe\rustinput-{session}")
}

fn own_session() -> u32 {
    let mut session = 0;
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) };
    session
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

/// Whether a daemon created the pipe of `session`, without connecting to
/// it: connecting takes the instance waiting for a window.
pub fn pipe_exists_in(session: u32) -> bool {
    let name = wide(&pipe_name_in(session));
    if unsafe { WaitNamedPipeW(name.as_ptr(), 1) } != 0 {
        return true;
    }
    // there, but no instance free right now
    io::Error::last_os_error().raw_os_error() == Some(ERROR_SEM_TIMEOUT as i32)
}

/// the security of the pipe, kept while instances are created
struct Security {
    descriptor: PSECURITY_DESCRIPTOR,
    attributes: SECURITY_ATTRIBUTES,
}

// the descriptor is plain memory of this process, freed once
unsafe impl Send for Security {}

impl Security {
    fn new() -> io::Result<Box<Self>> {
        let sddl = wide(SDDL);
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        };
        if converted == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Box::new(Self {
            descriptor,
            attributes: SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            },
        }))
    }

    fn create(&mut self, name: &str, first: bool) -> io::Result<NamedPipeServer> {
        unsafe {
            ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(
                    name,
                    &mut self.attributes as *mut SECURITY_ATTRIBUTES as *mut c_void,
                )
        }
    }
}

impl Drop for Security {
    fn drop(&mut self) {
        unsafe { LocalFree(self.descriptor) };
    }
}

/// Creates the pipe of this session and accepts windows on it, handing on
/// the ones that pass [`check_client`]. `PermissionDenied` when the pipe
/// exists already: another daemon runs in this session (or someone created
/// the name first).
pub(crate) fn listen() -> io::Result<mpsc::UnboundedReceiver<NamedPipeServer>> {
    let name = pipe_name();
    let mut security = Security::new()?;
    let first = security.create(&name, true)?;
    log::info!("waiting for the windows on {name}");
    let (windows, accepted) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut waiting = first;
        loop {
            let connected = waiting.connect().await;
            // the next window can come while this one is checked
            let next = match security.create(&name, false) {
                Ok(next) => next,
                Err(e) => {
                    log::error!("{name}: no new instance for the windows: {e}");
                    return;
                }
            };
            let pipe = std::mem::replace(&mut waiting, next);
            if let Err(e) = connected {
                log::warn!("{name}: a window did not connect: {e}");
                continue;
            }
            match check_client(&pipe) {
                Ok(()) => {
                    if windows.send(pipe).is_err() {
                        return;
                    }
                }
                Err(reason) => log::warn!("{name}: refused a connection: {reason}"),
            }
        }
    });
    Ok(accepted)
}

/// Whether the client of `pipe` may talk to this daemon, see the module.
fn check_client(pipe: &NamedPipeServer) -> Result<(), String> {
    let token = client_token(pipe)?;
    let user = TokenInfo::get(&token, TokenUser).map_err(|e| format!("its user: {e}"))?;
    let user = unsafe { (*(user.as_ptr() as *const TOKEN_USER)).User.Sid };
    let session = TokenInfo::get(&token, TokenSessionId)
        .map_err(|e| format!("its session: {e}"))?
        .read_u32();
    if session != own_session() {
        return Err(format!(
            "it runs in session {session}, not {}",
            own_session()
        ));
    }
    let container = TokenInfo::get(&token, TokenIsAppContainer)
        .map_err(|e| format!("whether it is an app container: {e}"))?
        .read_u32();
    if container != 0 {
        return Err("it runs in an app container".into());
    }
    let level =
        TokenInfo::get(&token, TokenIntegrityLevel).map_err(|e| format!("its integrity: {e}"))?;
    let level = integrity(unsafe {
        (*(level.as_ptr() as *const TOKEN_MANDATORY_LABEL))
            .Label
            .Sid
    });
    if level < SECURITY_MANDATORY_MEDIUM_RID as u32 {
        return Err(format!("its integrity is below medium ({level:#x})"));
    }
    if !allowed_user(user) {
        return Err("it runs as another user".into());
    }
    Ok(())
}

/// the token of the client of `pipe`, at the level it allows (identification)
fn client_token(pipe: &NamedPipeServer) -> Result<OwnedHandle, String> {
    let handle = pipe.as_raw_handle() as HANDLE;
    unsafe {
        if ImpersonateNamedPipeClient(handle) == 0 {
            return Err(format!("impersonating it: {}", io::Error::last_os_error()));
        }
        let mut token: HANDLE = ptr::null_mut();
        let opened = OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token);
        let error = io::Error::last_os_error();
        if RevertToSelf() == 0 {
            // never go on as someone else
            std::process::abort();
        }
        if opened == 0 {
            return Err(format!("its token: {error}"));
        }
        Ok(OwnedHandle::from_raw_handle(token as _))
    }
}

/// the user the daemon runs as, the one logged on in its session (when it
/// runs as SYSTEM), or SYSTEM
fn allowed_user(user: PSID) -> bool {
    if unsafe { IsWellKnownSid(user, WinLocalSystemSid) } != 0 {
        return true;
    }
    let same = |token: &OwnedHandle| {
        TokenInfo::get(token, TokenUser).is_ok_and(|own| unsafe {
            EqualSid((*(own.as_ptr() as *const TOKEN_USER)).User.Sid, user) != 0
        })
    };
    if own_token().is_some_and(|token| same(&token)) {
        return true;
    }
    // only SYSTEM may ask for the token of the user of a session
    let mut token: HANDLE = ptr::null_mut();
    if unsafe { WTSQueryUserToken(own_session(), &mut token) } == 0 {
        return false;
    }
    let token = unsafe { OwnedHandle::from_raw_handle(token as _) };
    same(&token)
}

fn own_token() -> Option<OwnedHandle> {
    let mut token: HANDLE = ptr::null_mut();
    (unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } != 0)
        .then(|| unsafe { OwnedHandle::from_raw_handle(token as _) })
}

/// the last sub authority of an integrity SID: its level
fn integrity(sid: PSID) -> u32 {
    unsafe {
        let count = *GetSidSubAuthorityCount(sid);
        match count {
            0 => 0,
            count => *GetSidSubAuthority(sid, count as u32 - 1),
        }
    }
}

/// what `GetTokenInformation` answers, aligned for the structures in it
struct TokenInfo(Vec<u64>);

impl TokenInfo {
    fn get(token: &OwnedHandle, class: TOKEN_INFORMATION_CLASS) -> io::Result<Self> {
        let handle = token.as_raw_handle() as HANDLE;
        let mut size = 0u32;
        unsafe { GetTokenInformation(handle, class, ptr::null_mut(), 0, &mut size) };
        if size == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buffer = vec![0u64; (size as usize).div_ceil(8)];
        let got = unsafe {
            GetTokenInformation(
                handle,
                class,
                buffer.as_mut_ptr() as *mut c_void,
                size,
                &mut size,
            )
        };
        if got == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(buffer))
    }

    fn as_ptr(&self) -> *const c_void {
        self.0.as_ptr() as *const c_void
    }

    fn read_u32(&self) -> u32 {
        unsafe { *(self.as_ptr() as *const u32) }
    }
}

/// Opens the pipe of this session (in a Tokio runtime) and checks that a
/// daemon of RustInput created it: its owner is SYSTEM, the administrators
/// or this user.
pub(crate) fn open() -> io::Result<NamedPipeClient> {
    // identification only (the default of Tokio): the daemon cannot act as us
    let pipe = ClientOptions::new().open(pipe_name())?;
    check_server(&pipe)?;
    Ok(pipe)
}

fn check_server(pipe: &NamedPipeClient) -> io::Result<()> {
    let mut owner: PSID = ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    let error = unsafe {
        GetSecurityInfo(
            pipe.as_raw_handle() as HANDLE,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    let trusted = unsafe {
        IsWellKnownSid(owner, WinLocalSystemSid) != 0
            || IsWellKnownSid(owner, WinBuiltinAdministratorsSid) != 0
            || own_token().is_some_and(|token| {
                TokenInfo::get(&token, TokenUser).is_ok_and(|own| {
                    EqualSid((*(own.as_ptr() as *const TOKEN_USER)).User.Sid, owner) != 0
                })
            })
    };
    unsafe { LocalFree(descriptor) };
    match trusted {
        true => Ok(()),
        false => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} belongs to another user, not to RustInput", pipe_name()),
        )),
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    use super::*;

    #[test]
    fn the_name_has_the_session() {
        assert_eq!(
            pipe_name(),
            format!(r"\\.\pipe\rustinput-{}", own_session())
        );
    }

    #[tokio::test]
    async fn a_window_of_the_same_user_gets_through() {
        // another name than the one of a daemon that may run here
        let name = format!(r"\\.\pipe\rustinput-test-{}", std::process::id());
        let mut security = Security::new().expect("the security of the pipe");
        let server = security.create(&name, true).expect("the pipe");
        // a second daemon can not take the name
        assert_eq!(
            security.create(&name, true).map(|_| ()).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        let client = ClientOptions::new().open(&name).expect("opened");
        server.connect().await.expect("connected");
        check_server(&client).expect("the owner is this user or the administrators");
        check_client(&server).expect("this process may talk to the daemon");
        let (mut client_rx, mut client_tx) = tokio::io::split(client);
        let (server_rx, mut server_tx) = tokio::io::split(server);
        client_tx.write_all(b"\"Sync\"\n").await.unwrap();
        let mut line = String::new();
        BufReader::new(server_rx)
            .read_line(&mut line)
            .await
            .unwrap();
        assert_eq!(line, "\"Sync\"\n");
        server_tx.write_all(b"ok\n").await.unwrap();
        let mut answer = String::new();
        BufReader::new(&mut client_rx)
            .read_line(&mut answer)
            .await
            .unwrap();
        assert_eq!(answer, "ok\n");
    }
}
