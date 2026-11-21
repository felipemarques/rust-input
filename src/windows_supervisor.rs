//! The Windows service "RustInput" (`rustinput windows-service`, which
//! Windows starts): it keeps `rustinput daemon` running in the session of the
//! user at the screen, with the administrator rights of that user.
//!
//! A service runs in a session of its own, which sees no keyboard and no
//! mouse, so it captures and emulates nothing itself. It starts the daemon in
//! the session at the screen with the token "Run as administrator" would give
//! that user, without asking: Windows then keeps neither the input going to
//! programs run as administrator from the hooks of RustInput, nor the input
//! RustInput emulates from those programs (User Interface Privilege
//! Isolation). The daemon runs in a job of the service, so it ends with the
//! service and no daemon is left behind holding the port. When the daemon
//! ends, the service starts it again; when another user comes to the screen,
//! the daemon moves to the session of that user.

use std::{
    ffi::{OsString, c_void},
    io, iter, mem,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    ptr,
    sync::mpsc,
    time::{Duration, Instant},
};

use rustinput_ipc::WINDOWS_SERVICE;
use windows_service::{
    define_windows_service,
    service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
};
use windows_sys::Win32::{
    Foundation::{ERROR_NO_TOKEN, HANDLE, WAIT_OBJECT_0},
    Security::{
        DuplicateTokenEx, GetTokenInformation, SecurityImpersonation, SetTokenInformation,
        TOKEN_ELEVATION, TOKEN_ELEVATION_TYPE, TOKEN_LINKED_TOKEN, TOKEN_QUERY, TokenElevation,
        TokenElevationType, TokenElevationTypeLimited, TokenLinkedToken, TokenPrimary,
        TokenSessionId,
    },
    System::{
        Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock},
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectExtendedLimitInformation, SetInformationJobObject,
        },
        RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken},
        SystemServices::MAXIMUM_ALLOWED,
        Threading::{
            CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW,
            GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, PROCESS_INFORMATION,
            ResumeThread, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
        },
    },
};

/// the argument Windows starts the service with (update-rustinput.ps1)
const ARGUMENT: &str = "windows-service";
/// how often the session at the screen is looked at
const TICK: Duration = Duration::from_secs(1);
/// a daemon that ran this long is started again at once when it ends
const STABLE: Duration = Duration::from_secs(60);
/// the longest wait before starting a daemon that keeps ending again
const LONGEST_DELAY: Duration = Duration::from_secs(60);
/// time the daemon has to end once it is told to
const END_TIMEOUT_MS: u32 = 5_000;

/// whether this process is the service, started by Windows
pub(crate) fn requested() -> bool {
    std::env::args().nth(1).as_deref() == Some(ARGUMENT)
}

/// Hands this process to Windows as the service; returns once it stopped.
pub(crate) fn run() -> windows_service::Result<()> {
    service_dispatcher::start(WINDOWS_SERVICE, ffi_service_main)
}

define_windows_service!(ffi_service_main, service_main);

fn service_main(_arguments: Vec<OsString>) {
    if let Err(e) = serve() {
        log::error!("the service failed: {e}");
    }
}

enum Control {
    Stop,
    /// someone logged on or off, locked, unlocked, switched users...
    SessionChanged,
}

fn serve() -> windows_service::Result<()> {
    let (tx, rx) = mpsc::channel();
    let handler = move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            let _ = tx.send(Control::Stop);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::SessionChange(_) => {
            let _ = tx.send(Control::SessionChanged);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let status = service_control_handler::register(WINDOWS_SERVICE, handler)?;
    let report = |state, controls_accepted| {
        status.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::from_secs(10),
            process_id: None,
        })
    };
    report(
        ServiceState::Running,
        ServiceControlAccept::STOP
            | ServiceControlAccept::SHUTDOWN
            | ServiceControlAccept::SESSION_CHANGE,
    )?;
    log::info!("RustInput service {} started", env!("CARGO_PKG_VERSION"));

    let mut supervisor = Supervisor::new();
    loop {
        supervisor.tick();
        match rx.recv_timeout(TICK) {
            Ok(Control::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(Control::SessionChanged) | Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }

    report(ServiceState::StopPending, ServiceControlAccept::empty())?;
    supervisor.end_daemon();
    log::info!("RustInput service stopped");
    report(ServiceState::Stopped, ServiceControlAccept::empty())
}

/// The daemon the service started.
struct Daemon {
    process: OwnedHandle,
    /// ends the daemon when the service ends, see [`new_job`]
    _job: OwnedHandle,
    pid: u32,
    session: u32,
    started: Instant,
}

impl Daemon {
    /// the exit code, once it ended
    fn exit_code(&self) -> Option<u32> {
        let process = self.process.as_raw_handle() as HANDLE;
        if unsafe { WaitForSingleObject(process, 0) } != WAIT_OBJECT_0 {
            return None;
        }
        let mut code = 0;
        unsafe { GetExitCodeProcess(process, &mut code) };
        Some(code)
    }

    fn end(self) {
        let process = self.process.as_raw_handle() as HANDLE;
        unsafe {
            TerminateProcess(process, 0);
            WaitForSingleObject(process, END_TIMEOUT_MS);
        }
        log::info!("ended the daemon (PID {})", self.pid);
    }
}

struct Supervisor {
    /// this program: the service runs from where only administrators may
    /// write, so starting it as administrator lets no one else in
    exe: PathBuf,
    daemon: Option<Daemon>,
    /// no daemon is started before
    next_start: Instant,
    /// the wait before starting again a daemon that ended
    delay: Duration,
    /// what was logged last about the session, so it is not repeated every
    /// second
    note: String,
}

impl Supervisor {
    fn new() -> Self {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rustinput.exe"));
        Self {
            exe,
            daemon: None,
            next_start: Instant::now(),
            delay: Duration::ZERO,
            note: String::new(),
        }
    }

    /// logs `text` unless it is what was logged last
    fn note(&mut self, text: String) {
        if self.note != text {
            log::info!("{text}");
            self.note = text;
        }
    }

    /// waits before starting a daemon again, longer every time it fails
    fn back_off(&mut self) {
        self.delay = (self.delay * 2).clamp(Duration::from_secs(1), LONGEST_DELAY);
        self.next_start = Instant::now() + self.delay;
    }

    fn tick(&mut self) {
        let session = unsafe { WTSGetActiveConsoleSessionId() };

        if let Some(code) = self.daemon.as_ref().and_then(Daemon::exit_code) {
            let daemon = self.daemon.take().expect("a daemon");
            let ran = daemon.started.elapsed();
            if ran >= STABLE {
                self.delay = Duration::ZERO;
                self.next_start = Instant::now() + Duration::from_secs(1);
            } else {
                self.back_off();
            }
            if ran < Duration::from_secs(10) && rustinput_ipc::is_service_running() {
                // "service already running!": the port of the window is taken
                self.note(format!(
                    "the daemon ended at once: another RustInput runs without the service, \
                     without administrator rights (close it from the tray icon); trying again \
                     every {} s",
                    self.delay.as_secs()
                ));
            } else {
                log::warn!(
                    "the daemon (PID {}) ended after {} s with exit code {code:#x}, starting it \
                     again in {} s",
                    daemon.pid,
                    ran.as_secs(),
                    self.next_start
                        .saturating_duration_since(Instant::now())
                        .as_secs()
                );
            }
        }

        if self.daemon.as_ref().is_some_and(|d| d.session != session) {
            let daemon = self.daemon.take().expect("a daemon");
            log::info!(
                "session {} is not at the screen any more, session {session} is",
                daemon.session
            );
            daemon.end();
            self.delay = Duration::ZERO;
            self.next_start = Instant::now();
        }

        if self.daemon.is_some() || Instant::now() < self.next_start {
            return;
        }
        // no session is attached to the screen while one is being switched to
        if session == u32::MAX {
            return;
        }
        let token = match user_token(session) {
            Ok(Some(token)) => token,
            Ok(None) => {
                self.note(format!(
                    "nobody is logged on at the screen (session {session})"
                ));
                return;
            }
            Err(e) => {
                self.note(format!("could not get the user of session {session}: {e}"));
                self.back_off();
                return;
            }
        };
        match start(&self.exe, &token, session) {
            Ok(daemon) => {
                log::info!(
                    "started the daemon (PID {}) in session {session}, {}",
                    daemon.pid,
                    if is_elevated(token.as_raw_handle() as HANDLE) {
                        "with administrator rights"
                    } else {
                        "without administrator rights: the user is not an administrator"
                    }
                );
                self.note.clear();
                self.daemon = Some(daemon);
            }
            Err(e) => {
                self.back_off();
                log::error!(
                    "could not start the daemon in session {session}: {e}; trying again in {} s",
                    self.delay.as_secs()
                );
            }
        }
    }

    fn end_daemon(&mut self) {
        if let Some(daemon) = self.daemon.take() {
            daemon.end();
        }
    }
}

/// Whether RustInput runs as administrator: then no program is out of reach
/// of its hooks and of the input it emulates.
pub(crate) fn runs_as_administrator() -> bool {
    let mut token = ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    is_elevated(token.as_raw_handle() as HANDLE)
}

fn is_elevated(token: HANDLE) -> bool {
    let mut elevation = TOKEN_ELEVATION::default();
    let mut len = 0;
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast::<c_void>(),
            mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
    };
    ok != 0 && elevation.TokenIsElevated != 0
}

/// The token to start the daemon with in `session`: the full one of an
/// administrator, which User Account Control keeps linked to the limited one
/// the programs of the user get. `None` if nobody is logged on there.
fn user_token(session: u32) -> io::Result<Option<OwnedHandle>> {
    let mut token = ptr::null_mut();
    if unsafe { WTSQueryUserToken(session, &mut token) } == 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(ERROR_NO_TOKEN as i32) {
            return Ok(None);
        }
        return Err(e);
    }
    let mut token = unsafe { OwnedHandle::from_raw_handle(token) };

    let mut kind: TOKEN_ELEVATION_TYPE = 0;
    let mut len = 0;
    let known = unsafe {
        GetTokenInformation(
            token.as_raw_handle() as HANDLE,
            TokenElevationType,
            (&mut kind as *mut TOKEN_ELEVATION_TYPE).cast::<c_void>(),
            mem::size_of::<TOKEN_ELEVATION_TYPE>() as u32,
            &mut len,
        )
    } != 0;
    if known && kind == TokenElevationTypeLimited {
        let mut linked = TOKEN_LINKED_TOKEN::default();
        let ok = unsafe {
            GetTokenInformation(
                token.as_raw_handle() as HANDLE,
                TokenLinkedToken,
                (&mut linked as *mut TOKEN_LINKED_TOKEN).cast::<c_void>(),
                mem::size_of::<TOKEN_LINKED_TOKEN>() as u32,
                &mut len,
            )
        };
        if ok != 0 {
            token = unsafe { OwnedHandle::from_raw_handle(linked.LinkedToken) };
        } else {
            log::warn!(
                "no administrator token of the user: {}",
                io::Error::last_os_error()
            );
        }
    }

    // a primary token of that session, to start a process with
    let mut primary = ptr::null_mut();
    if unsafe {
        DuplicateTokenEx(
            token.as_raw_handle() as HANDLE,
            MAXIMUM_ALLOWED,
            ptr::null(),
            SecurityImpersonation,
            TokenPrimary,
            &mut primary,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let primary = unsafe { OwnedHandle::from_raw_handle(primary) };
    let ok = unsafe {
        SetTokenInformation(
            primary.as_raw_handle() as HANDLE,
            TokenSessionId,
            (&session as *const u32).cast::<c_void>(),
            mem::size_of::<u32>() as u32,
        )
    };
    if ok == 0 {
        log::debug!(
            "could not set the session of the token: {}",
            io::Error::last_os_error()
        );
    }
    Ok(Some(primary))
}

/// `text` as a nul terminated wide string
fn wide(text: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    text.as_ref().encode_wide().chain(iter::once(0)).collect()
}

/// a job that ends its processes when the last handle to it closes, that
/// of the service at the latest; programs the daemon starts stay out of it
fn new_job() -> io::Result<OwnedHandle> {
    let job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
    if job.is_null() {
        return Err(io::Error::last_os_error());
    }
    let job = unsafe { OwnedHandle::from_raw_handle(job) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK;
    if unsafe {
        SetInformationJobObject(
            job.as_raw_handle() as HANDLE,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast::<c_void>(),
            mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(job)
}

/// the environment of the user of `token`: their folders, not those of the
/// service
struct Environment(*mut c_void);

impl Environment {
    fn of(token: &OwnedHandle) -> io::Result<Self> {
        let mut block = ptr::null_mut();
        if unsafe { CreateEnvironmentBlock(&mut block, token.as_raw_handle() as HANDLE, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(block))
    }
}

impl Drop for Environment {
    fn drop(&mut self) {
        unsafe { DestroyEnvironmentBlock(self.0) };
    }
}

/// starts `exe daemon` in `session` with `token`, on the desktop of the user
fn start(exe: &Path, token: &OwnedHandle, session: u32) -> io::Result<Daemon> {
    let environment = Environment::of(token)?;
    let application = wide(exe);
    let mut command_line = wide(format!("\"{}\" daemon", exe.display()));
    let directory = exe.parent().map(wide);
    let mut desktop = wide(r"winsta0\default");
    let startup = STARTUPINFOW {
        cb: mem::size_of::<STARTUPINFOW>() as u32,
        lpDesktop: desktop.as_mut_ptr(),
        ..Default::default()
    };
    let mut info = PROCESS_INFORMATION::default();
    if unsafe {
        CreateProcessAsUserW(
            token.as_raw_handle() as HANDLE,
            application.as_ptr(),
            command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            0,
            CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW | CREATE_SUSPENDED,
            environment.0,
            directory.as_ref().map_or(ptr::null(), |d| d.as_ptr()),
            &startup,
            &mut info,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let process = unsafe { OwnedHandle::from_raw_handle(info.hProcess) };
    let thread = unsafe { OwnedHandle::from_raw_handle(info.hThread) };
    let abandon = |e: io::Error| {
        unsafe { TerminateProcess(process.as_raw_handle() as HANDLE, 1) };
        e
    };

    // in the job before it runs: a daemon left behind by a service that
    // crashed would keep the port from the next one
    let job = new_job().map_err(abandon)?;
    if unsafe {
        AssignProcessToJobObject(
            job.as_raw_handle() as HANDLE,
            process.as_raw_handle() as HANDLE,
        )
    } == 0
    {
        return Err(abandon(io::Error::last_os_error()));
    }
    if unsafe { ResumeThread(thread.as_raw_handle() as HANDLE) } == u32::MAX {
        return Err(abandon(io::Error::last_os_error()));
    }
    Ok(Daemon {
        process,
        _job: job,
        pid: info.dwProcessId,
        session,
        started: Instant::now(),
    })
}
