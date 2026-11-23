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
//!
//! For an administrator the daemon runs as SYSTEM instead, with the folders
//! of that user (their `LOCALAPPDATA`, where the configuration and the
//! certificate are): only SYSTEM may open the desktop of the lock screen,
//! of the screen to log on and of the prompts of User Account Control
//! (Winlogon), which the daemon follows (`input_capture`, `input_emulation`)
//! so another computer can type the password there. That administrator is
//! remembered (in `HKLM\SOFTWARE\RustInput`, which only administrators may
//! change): while nobody is logged on, after starting Windows or logging
//! off, the daemon runs as SYSTEM with their folders, for the screen to log
//! on. `login_screen = false` in their configuration turns all of this off,
//! and the daemon runs as them, as before 0.31.0. A user who is no
//! administrator always gets a daemon of their own, as SYSTEM would give
//! them rights they do not have.

use std::{
    ffi::{OsString, c_void},
    io, iter, mem,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    ptr, slice,
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
    Foundation::{ERROR_NO_TOKEN, ERROR_SUCCESS, HANDLE, WAIT_OBJECT_0},
    Security::{
        DuplicateTokenEx, GetTokenInformation, IsWellKnownSid, SecurityImpersonation,
        SetTokenInformation, TOKEN_ADJUST_DEFAULT, TOKEN_ADJUST_SESSIONID, TOKEN_ASSIGN_PRIMARY,
        TOKEN_DUPLICATE, TOKEN_ELEVATION, TOKEN_ELEVATION_TYPE, TOKEN_LINKED_TOKEN, TOKEN_QUERY,
        TOKEN_USER, TokenElevation, TokenElevationType, TokenElevationTypeLimited,
        TokenLinkedToken, TokenPrimary, TokenSessionId, TokenUser, WinLocalSystemSid,
    },
    System::{
        Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock},
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectExtendedLimitInformation, SetInformationJobObject,
        },
        Registry::{
            HKEY, HKEY_LOCAL_MACHINE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
            RRF_RT_REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteKeyValueW, RegGetValueW,
            RegSetValueExW,
        },
        RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken},
        SystemServices::MAXIMUM_ALLOWED,
        Threading::{
            CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW,
            GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, PROCESS_INFORMATION,
            ResumeThread, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
        },
    },
    UI::Shell::GetUserProfileDirectoryW,
};

/// the argument Windows starts the service with (windows_setup.rs, and
/// update-rustinput.ps1 for versions without the installer)
const ARGUMENT: &str = "windows-service";
/// how often the session at the screen is looked at
const TICK: Duration = Duration::from_secs(1);
/// a daemon that ran this long is started again at once when it ends
const STABLE: Duration = Duration::from_secs(60);
/// the longest wait before starting a daemon that keeps ending again
const LONGEST_DELAY: Duration = Duration::from_secs(60);
/// time the daemon has to end once it is told to
const END_TIMEOUT_MS: u32 = 5_000;
/// how often who is at the screen is looked at again, besides when Windows
/// tells of a logon, a logoff or a switch of users
const LOOK_AGAIN: Duration = Duration::from_secs(5);
/// where the administrator whose folders the daemon uses while nobody is
/// logged on is remembered (HKEY_LOCAL_MACHINE: only administrators may
/// change it)
pub(crate) const KEY: &str = r"SOFTWARE\RustInput";
const LOGIN_SCREEN_VALUE: &str = "LoginScreenLocalAppData";

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
            Ok(Control::SessionChanged) => supervisor.look = true,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }

    report(ServiceState::StopPending, ServiceControlAccept::empty())?;
    supervisor.end_daemon();
    log::info!("RustInput service stopped");
    report(ServiceState::Stopped, ServiceControlAccept::empty())
}

/// Who the daemon runs as, and with whose folders.
#[derive(Clone, Debug)]
enum RunAs {
    /// the user at the screen, who is no administrator
    User,
    /// SYSTEM, which also reaches the lock screen, the screen to log on and
    /// the prompts of User Account Control, with the folders of an
    /// administrator: the one at the screen, or while nobody is logged on
    /// the one remembered
    System {
        local_app_data: PathBuf,
        logged_on: bool,
    },
}

impl RunAs {
    /// whether a daemon running as `self` may stay when `wanted` is wanted:
    /// one for the screen to log on stays when the same administrator logs
    /// on, so nothing is interrupted
    fn serves(&self, wanted: &RunAs) -> bool {
        match (self, wanted) {
            (RunAs::User, RunAs::User) => true,
            (
                RunAs::System {
                    local_app_data: a, ..
                },
                RunAs::System {
                    local_app_data: b, ..
                },
            ) => a.as_os_str().eq_ignore_ascii_case(b.as_os_str()),
            _ => false,
        }
    }

    fn describe(&self) -> String {
        match self {
            RunAs::User => "as the user at the screen, who is no administrator: not on the lock \
                            screen and the screen to log on"
                .to_owned(),
            RunAs::System {
                local_app_data,
                logged_on: true,
            } => format!(
                "as SYSTEM with the folders in {}: also on the lock screen and the prompts of \
                 User Account Control",
                local_app_data.display()
            ),
            RunAs::System {
                local_app_data,
                logged_on: false,
            } => format!(
                "as SYSTEM for the screen to log on, nobody is logged on: with the folders in {} \
                 of the administrator who was last",
                local_app_data.display()
            ),
        }
    }
}

/// what the daemon should be started as now
struct Wanted {
    run_as: RunAs,
    /// of the user, for [`RunAs::User`]
    token: Option<OwnedHandle>,
    environment: Environment,
}

/// The daemon the service started.
struct Daemon {
    process: OwnedHandle,
    /// ends the daemon when the service ends, see [`new_job`]
    _job: OwnedHandle,
    pid: u32,
    session: u32,
    started: Instant,
    run_as: RunAs,
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
    /// who is at the screen is looked at again at the next tick
    look: bool,
    /// when who is at the screen was looked at last
    looked: Instant,
    /// the administrator remembered for the screen to log on, as last read
    /// or written (`None`: not known yet)
    remembered: Option<Option<PathBuf>>,
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
            look: true,
            looked: Instant::now(),
            remembered: None,
        }
    }

    /// the administrator remembered for the screen to log on
    fn remembered(&mut self) -> Option<PathBuf> {
        self.remembered
            .get_or_insert_with(|| read_registry(LOGIN_SCREEN_VALUE).map(PathBuf::from))
            .clone()
    }

    /// remembers `local_app_data` for the screen to log on, or forgets it
    fn remember(&mut self, local_app_data: Option<&Path>) {
        let now = local_app_data.map(Path::to_path_buf);
        if self.remembered() == now {
            return;
        }
        let written = match &now {
            Some(path) => write_registry(LOGIN_SCREEN_VALUE, &path.to_string_lossy()),
            None => delete_registry(LOGIN_SCREEN_VALUE),
        };
        match written {
            Ok(()) => match &now {
                Some(path) => log::info!(
                    "the screen to log on uses the folders in {} while nobody is logged on",
                    path.display()
                ),
                None => log::info!("no daemon for the screen to log on any more"),
            },
            Err(e) => {
                log::warn!("could not remember the administrator for the screen to log on: {e}")
            }
        }
        self.remembered = Some(now);
    }

    /// what the daemon should run as in `session` now; `None` while nobody
    /// is logged on and no administrator is remembered
    fn wanted(&mut self, session: u32) -> io::Result<Option<Wanted>> {
        let Some(token) = user_token(session)? else {
            return Ok(self.remembered().and_then(|local_app_data| {
                // the folders still there, the screen to log on still wanted
                let config = local_app_data.join("rustinput").join("config.toml");
                if !config.is_file() || !login_screen(&local_app_data) {
                    return None;
                }
                let mut environment = Environment::of(None).ok()?;
                environment.set("LOCALAPPDATA", &local_app_data.to_string_lossy());
                Some(Wanted {
                    run_as: RunAs::System {
                        local_app_data,
                        logged_on: false,
                    },
                    token: None,
                    environment,
                })
            }));
        };
        let user_environment = Environment::of(Some(&token))?;
        let administrator = is_elevated(token.as_raw_handle() as HANDLE);
        if administrator {
            let local_app_data = local_app_data_of(&token)?;
            if login_screen(&local_app_data) {
                self.remember(Some(&local_app_data));
                if user_environment
                    .get("LOCALAPPDATA")
                    .is_some_and(|own| Path::new(&own) != local_app_data)
                {
                    log::warn!(
                        "LOCALAPPDATA of the user points elsewhere; as SYSTEM RustInput uses \
                         {local_app_data:?}, the folder of the profile"
                    );
                }
                // Only the variables of the system and the folder of the
                // profile: the ones of the user (HKCU\Environment) any
                // program of the user may change, also to a folder other
                // users may write, or PATH to load a DLL as SYSTEM.
                let mut environment = Environment::of(None)?;
                environment.set("LOCALAPPDATA", &local_app_data.to_string_lossy());
                // as SYSTEM, Windows would tell RustInput the languages of
                // the system
                if let Some(languages) = user_languages(&token) {
                    environment.set(rustinput_i18n::USER_LANGUAGES, &languages);
                }
                return Ok(Some(Wanted {
                    run_as: RunAs::System {
                        local_app_data,
                        logged_on: true,
                    },
                    token: None,
                    environment,
                }));
            }
            // login_screen = false
            self.remember(None);
        }
        Ok(Some(Wanted {
            run_as: RunAs::User,
            token: Some(token),
            environment: user_environment,
        }))
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
            let wait = self
                .next_start
                .saturating_duration_since(Instant::now())
                .as_secs_f64()
                .ceil();
            if ran < Duration::from_secs(10) && rustinput_ipc::pipe_exists_in(daemon.session) {
                // "service already running!": the pipe of the session is taken
                self.note(format!(
                    "the daemon ended at once: another RustInput runs without the service, \
                     without administrator rights (close it from the tray icon); trying again \
                     every {wait} s"
                ));
            } else {
                log::warn!(
                    "the daemon (PID {}) ended after {} s with exit code {code:#x}, starting it \
                     again in {} s",
                    daemon.pid,
                    ran.as_secs(),
                    wait
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

        // no session is attached to the screen while one is being switched to
        if session == u32::MAX {
            return;
        }
        let look = self.daemon.is_none() || self.look || self.looked.elapsed() >= LOOK_AGAIN;
        if !look {
            return;
        }
        self.look = false;
        self.looked = Instant::now();
        let wanted = match self.wanted(session) {
            Ok(wanted) => wanted,
            Err(e) => {
                self.note(format!("could not get the user of session {session}: {e}"));
                if self.daemon.is_none() {
                    self.back_off();
                }
                return;
            }
        };
        if let Some(daemon) = self.daemon.as_mut() {
            match &wanted {
                Some(wanted) if daemon.run_as.serves(&wanted.run_as) => {
                    if let (RunAs::System { logged_on, .. }, RunAs::System { logged_on: now, .. }) =
                        (&mut daemon.run_as, &wanted.run_as)
                    {
                        if *logged_on != *now {
                            *logged_on = *now;
                            log::info!(
                                "the daemon (PID {}) stays: {}",
                                daemon.pid,
                                daemon.run_as.describe()
                            );
                        }
                    }
                    return;
                }
                // nobody logged on: the daemon of the user who left stays,
                // as before 0.31.0, one as SYSTEM no longer wanted does not
                None if matches!(daemon.run_as, RunAs::User) => return,
                _ => {}
            }
            let daemon = self.daemon.take().expect("a daemon");
            log::info!(
                "the daemon (PID {}) ran {}, now it should run {}",
                daemon.pid,
                daemon.run_as.describe(),
                wanted
                    .as_ref()
                    .map_or("not at all".to_owned(), |w| w.run_as.describe())
            );
            daemon.end();
            self.delay = Duration::ZERO;
            self.next_start = Instant::now();
        }
        let Some(wanted) = wanted else {
            self.note(format!(
                "nobody is logged on at the screen (session {session})"
            ));
            return;
        };
        if Instant::now() < self.next_start {
            // looked at again when it is time to start
            self.look = true;
            return;
        }
        match start(&self.exe, &wanted, session) {
            Ok(daemon) => {
                log::info!(
                    "started the daemon (PID {}) in session {session}, {}",
                    daemon.pid,
                    daemon.run_as.describe()
                );
                self.note.clear();
                self.daemon = Some(daemon);
            }
            Err(e) => {
                self.back_off();
                self.look = true;
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

/// Whether RustInput runs as SYSTEM, as the service starts it for an
/// administrator.
pub(crate) fn runs_as_system() -> bool {
    let mut token = ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut buffer = [0u64; 32];
    let mut len = 0;
    let read = unsafe {
        GetTokenInformation(
            token.as_raw_handle() as HANDLE,
            TokenUser,
            buffer.as_mut_ptr().cast::<c_void>(),
            mem::size_of_val(&buffer) as u32,
            &mut len,
        )
    };
    read != 0 && {
        let user = unsafe { &*(buffer.as_ptr() as *const TOKEN_USER) };
        unsafe { IsWellKnownSid(user.User.Sid, WinLocalSystemSid) != 0 }
    }
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

/// The environment to start the daemon with: that of the user of a token,
/// with their folders, not those of the service; or that of the system, as
/// `NAME=value` strings, each ending with a nul, and a nul at the end.
struct Environment(Vec<u16>);

impl Environment {
    fn of(token: Option<&OwnedHandle>) -> io::Result<Self> {
        let token = token.map_or(ptr::null_mut(), |t| t.as_raw_handle() as HANDLE);
        let mut block: *mut c_void = ptr::null_mut();
        if unsafe { CreateEnvironmentBlock(&mut block, token, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the block ends with two nuls, read up to them and freed
        let copy = unsafe {
            let start = block as *const u16;
            let mut len = 0;
            while *start.add(len) != 0 || *start.add(len + 1) != 0 {
                len += 1;
            }
            let copy = slice::from_raw_parts(start, len + 2).to_vec();
            DestroyEnvironmentBlock(block);
            copy
        };
        Ok(Self(copy))
    }

    fn variables(&self) -> impl Iterator<Item = String> + '_ {
        self.0
            .split(|&c| c == 0)
            .filter(|v| !v.is_empty())
            .map(String::from_utf16_lossy)
    }

    /// the value of `name`, whatever its case
    fn get(&self, name: &str) -> Option<String> {
        self.variables().find_map(|variable| {
            let (n, value) = variable.split_once('=')?;
            n.eq_ignore_ascii_case(name).then(|| value.to_owned())
        })
    }

    /// sets `name` to `value`, in place of what it was
    fn set(&mut self, name: &str, value: &str) {
        let mut variables: Vec<String> = self
            .variables()
            .filter(|v| {
                v.split_once('=')
                    .is_none_or(|(n, _)| n.is_empty() || !n.eq_ignore_ascii_case(name))
            })
            .collect();
        variables.push(format!("{name}={value}"));
        let mut block: Vec<u16> = Vec::new();
        for variable in variables {
            block.extend(variable.encode_utf16());
            block.push(0);
        }
        block.push(0);
        if block.len() == 1 {
            block.push(0);
        }
        self.0 = block;
    }
}

/// `<profile>\AppData\Local` of the user of `token`, from the profile
/// Windows keeps for that user (which only administrators can change), not
/// from the environment or the folders the user can redirect.
fn local_app_data_of(token: &OwnedHandle) -> io::Result<PathBuf> {
    let handle = token.as_raw_handle() as HANDLE;
    let mut size = 0u32;
    unsafe { GetUserProfileDirectoryW(handle, ptr::null_mut(), &mut size) };
    if size == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buffer = vec![0u16; size as usize];
    if unsafe { GetUserProfileDirectoryW(handle, buffer.as_mut_ptr(), &mut size) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    let profile = PathBuf::from(<OsString as std::os::windows::ffi::OsStringExt>::from_wide(
        &buffer[..end],
    ));
    Ok(profile.join("AppData").join("Local"))
}

/// A primary token of SYSTEM (the one of the service) in `session`: a
/// process started with it runs on the desktops of that session and may
/// open the one of the lock screen.
fn system_token(session: u32) -> io::Result<OwnedHandle> {
    let mut token = ptr::null_mut();
    let access = TOKEN_DUPLICATE
        | TOKEN_QUERY
        | TOKEN_ASSIGN_PRIMARY
        | TOKEN_ADJUST_SESSIONID
        | TOKEN_ADJUST_DEFAULT;
    if unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
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
    // without it, the daemon would start in the session of the service,
    // which sees no keyboard and no mouse
    if unsafe {
        SetTokenInformation(
            primary.as_raw_handle() as HANDLE,
            TokenSessionId,
            (&session as *const u32).cast::<c_void>(),
            mem::size_of::<u32>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(primary)
}

/// The display languages the user of `token` chose in Windows ("pt-BR;en-US"),
/// none when they keep those of the system.
fn user_languages(token: &OwnedHandle) -> Option<String> {
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Authorization::ConvertSidToStringSidW,
        System::Registry::{HKEY_USERS, RRF_RT_REG_MULTI_SZ},
    };
    let mut buffer = [0u64; 32];
    let mut len = 0;
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle() as HANDLE,
            TokenUser,
            buffer.as_mut_ptr().cast::<c_void>(),
            mem::size_of_val(&buffer) as u32,
            &mut len,
        )
    } == 0
    {
        return None;
    }
    let user = unsafe { &*(buffer.as_ptr() as *const TOKEN_USER) };
    let mut sid = ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid) } == 0 {
        return None;
    }
    let sid_text = unsafe {
        let mut end = 0;
        while *sid.add(end) != 0 {
            end += 1;
        }
        let text = String::from_utf16_lossy(slice::from_raw_parts(sid, end));
        LocalFree(sid.cast());
        text
    };
    // the hive of a user logged on is loaded under HKEY_USERS
    let key = wide(format!(r"{sid_text}\Control Panel\Desktop"));
    let name = wide("PreferredUILanguages");
    let mut data = [0u16; 512];
    let mut size = mem::size_of_val(&data) as u32;
    let read = unsafe {
        RegGetValueW(
            HKEY_USERS,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_MULTI_SZ,
            ptr::null_mut(),
            data.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if read != ERROR_SUCCESS {
        return None;
    }
    let used = (size as usize / 2).min(data.len());
    let languages: Vec<String> = data[..used]
        .split(|&c| c == 0)
        .filter(|l| !l.is_empty())
        .map(String::from_utf16_lossy)
        .collect();
    (!languages.is_empty()).then(|| languages.join(";"))
}

/// Whether the configuration in the `LOCALAPPDATA` of a user lets RustInput
/// work on the lock screen and the screen to log on (`login_screen`, on
/// when not set or not readable).
fn login_screen(local_app_data: &Path) -> bool {
    let path = local_app_data.join("rustinput").join("config.toml");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
        .and_then(|table| table.get("login_screen").and_then(toml::Value::as_bool))
        .unwrap_or(true)
}

/// the text of the value `name` of [`KEY`]
fn read_registry(name: &str) -> Option<String> {
    let (key, name) = (wide(KEY), wide(name));
    let mut data = [0u16; 1024];
    let mut size = mem::size_of_val(&data) as u32;
    let read = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            ptr::null_mut(),
            data.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if read != ERROR_SUCCESS {
        return None;
    }
    let end = data.iter().position(|&c| c == 0).unwrap_or(data.len());
    Some(String::from_utf16_lossy(&data[..end])).filter(|s| !s.is_empty())
}

/// sets the value `name` of [`KEY`] to `text`
fn write_registry(name: &str, text: &str) -> io::Result<()> {
    let (key_name, name, text) = (wide(KEY), wide(name), wide(text));
    let mut key: HKEY = ptr::null_mut();
    let created = unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            key_name.as_ptr(),
            0,
            ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            ptr::null(),
            &mut key,
            ptr::null_mut(),
        )
    };
    if created != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(created as i32));
    }
    let set = unsafe {
        RegSetValueExW(
            key,
            name.as_ptr(),
            0,
            REG_SZ,
            text.as_ptr().cast(),
            (text.len() * 2) as u32,
        )
    };
    unsafe { RegCloseKey(key) };
    match set {
        ERROR_SUCCESS => Ok(()),
        e => Err(io::Error::from_raw_os_error(e as i32)),
    }
}

/// removes the value `name` of [`KEY`]
fn delete_registry(name: &str) -> io::Result<()> {
    let (key, name) = (wide(KEY), wide(name));
    match unsafe { RegDeleteKeyValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), name.as_ptr()) } {
        ERROR_SUCCESS => Ok(()),
        // not there
        2 => Ok(()),
        e => Err(io::Error::from_raw_os_error(e as i32)),
    }
}

/// starts `exe daemon` in `session` as `wanted` says, on the desktop of the
/// user
fn start(exe: &Path, wanted: &Wanted, session: u32) -> io::Result<Daemon> {
    let system;
    let token = match (&wanted.run_as, &wanted.token) {
        (RunAs::User, Some(token)) => token,
        (RunAs::User, None) => return Err(io::Error::other("no token of the user")),
        (RunAs::System { .. }, _) => {
            system = system_token(session)?;
            &system
        }
    };
    let environment = &wanted.environment;
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
            environment.0.as_ptr().cast(),
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
        run_as: wanted.run_as.clone(),
    })
}
