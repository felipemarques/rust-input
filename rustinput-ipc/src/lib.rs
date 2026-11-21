use std::{
    collections::{HashMap, HashSet},
    env::VarError,
    fmt::Display,
    io,
    net::{IpAddr, SocketAddr},
    str::FromStr,
};
use thiserror::Error;

#[cfg(unix)]
use std::{
    env,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

mod dirs;
pub use dirs::{log_dir, received_dir, transfer_log};

/// "512 bytes", "1.5 KB", "120.3 MB", "2.1 GB" (powers of 1024, like file
/// managers)
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} bytes");
    }
    let mut size = bytes as f64 / 1024.0;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

mod connect;
mod connect_async;
mod listen;

pub use connect::{FrontendEventReader, FrontendRequestWriter, connect, try_connect};
pub use connect_async::{AsyncFrontendEventReader, AsyncFrontendRequestWriter, connect_async};
pub use listen::AsyncFrontendListener;

#[derive(Debug, Error)]
pub enum ConnectionError {
    #[error(transparent)]
    SocketPath(#[from] SocketPathError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("connection timed out")]
    Timeout,
}

#[derive(Debug, Error)]
pub enum IpcListenerCreationError {
    #[error("could not determine socket-path: `{0}`")]
    SocketPath(#[from] SocketPathError),
    #[error("service already running!")]
    AlreadyRunning,
    #[error("failed to bind rustinput socket: `{0}`")]
    Bind(io::Error),
}

#[derive(Debug, Error)]
pub enum IpcError {
    #[error("io error occured: `{0}`")]
    Io(#[from] io::Error),
    #[error("invalid json: `{0}`")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Connection(#[from] ConnectionError),
    #[error(transparent)]
    Listen(#[from] IpcListenerCreationError),
}

pub const DEFAULT_PORT: u16 = 4242;

#[derive(Debug, Default, Eq, Hash, PartialEq, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Position {
    #[default]
    Left,
    Right,
    Top,
    Bottom,
}

impl Position {
    pub fn opposite(&self) -> Self {
        match self {
            Position::Left => Position::Right,
            Position::Right => Position::Left,
            Position::Top => Position::Bottom,
            Position::Bottom => Position::Top,
        }
    }
}

#[derive(Debug, Error)]
#[error("not a valid position: {pos}")]
pub struct PositionParseError {
    pos: String,
}

impl FromStr for Position {
    type Err = PositionParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "left" => Ok(Self::Left),
            "right" => Ok(Self::Right),
            "top" => Ok(Self::Top),
            "bottom" => Ok(Self::Bottom),
            _ => Err(PositionParseError { pos: s.into() }),
        }
    }
}

impl Display for Position {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Position::Left => "left",
                Position::Right => "right",
                Position::Top => "top",
                Position::Bottom => "bottom",
            }
        )
    }
}

impl TryFrom<&str> for Position {
    type Error = ();

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "left" => Ok(Position::Left),
            "right" => Ok(Position::Right),
            "top" => Ok(Position::Top),
            "bottom" => Ok(Position::Bottom),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    /// hostname of this client
    pub hostname: Option<String>,
    /// fix ips, determined by the user
    pub fix_ips: Vec<IpAddr>,
    /// both active_addr and addrs can be None / empty so port needs to be stored seperately
    pub port: u16,
    /// position of a client on screen
    pub pos: Position,
    /// enter hook
    pub cmd: Option<String>,
    /// leave hook
    pub leave_cmd: Option<String>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            hostname: Default::default(),
            fix_ips: Default::default(),
            pos: Default::default(),
            cmd: None,
            leave_cmd: None,
        }
    }
}

pub type ClientHandle = u64;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ClientState {
    /// events should be sent to and received from the client
    pub active: bool,
    /// `active` address of the client, used to send data to.
    /// This should generally be the socket address where data
    /// was last received from.
    pub active_addr: Option<SocketAddr>,
    /// tracks whether or not the client is available for emulation
    pub alive: bool,
    /// ips from dns
    pub dns_ips: Vec<IpAddr>,
    /// all ip addresses associated with a particular client
    /// e.g. Laptops usually have at least an ethernet and a wifi port
    /// which have different ip addresses
    pub ips: HashSet<IpAddr>,
    /// client has pressed keys
    pub has_pressed_keys: bool,
    /// dns resolving in progress
    pub resolving: bool,
    /// Peer's build short commit hash from the [`Hello`] proto
    /// event. `None` means we haven't received a Hello yet — either
    /// the connection is fresh, or the peer is on an older build
    /// that predates the Hello event. The frontend uses this to
    /// soft-warn on version mismatch.
    pub peer_commit: Option<[u8; 8]>,
    /// optional features the peer announced (`CAP_*` flags of the
    /// protocol), 0 until it did
    #[serde(default)]
    pub peer_capabilities: u32,
}

/// a file transfer, as shown to the user
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferInfo {
    /// identifies the transfer among the reported ones
    pub id: u64,
    /// sent by this device (`true`) or received (`false`)
    pub sending: bool,
    /// name of the other device
    pub peer: String,
    pub files: u32,
    pub files_done: u32,
    pub bytes: u64,
    pub bytes_done: u64,
    /// the file being transferred
    pub current: Option<String>,
    pub state: TransferState,
    /// when the transfer started, in milliseconds since the Unix epoch
    #[serde(default)]
    pub started_ms: u64,
    /// when it ended
    #[serde(default)]
    pub finished_ms: Option<u64>,
    /// the folder holding the received files
    #[serde(default)]
    pub location: Option<String>,
}

impl TransferInfo {
    /// still waiting or running
    pub fn active(&self) -> bool {
        matches!(self.state, TransferState::Waiting | TransferState::Running)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransferState {
    /// queued behind another transfer to the same device
    Waiting,
    Running,
    /// sent, or received and on the clipboard, ready to be pasted
    Done,
    /// the received files were pasted (moved out of the temporary folder)
    Pasted,
    /// the received files were removed from the temporary folder unpasted
    Removed,
    Failed(String),
}

/// what happens to copied files, see the configuration file
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileOptions {
    /// file transfers are enabled at all (`clipboard_files`), read only
    pub available: bool,
    /// files copied in a file manager are sent when the pointer moves to
    /// another device (`send_copied_files`)
    pub send_copied: bool,
    /// the transfers window opens when a transfer starts
    /// (`show_transfers_window`)
    pub show_window: bool,
    /// "Copy to RustInput" in the menu of the file managers
    /// (`file_manager_menu`)
    pub file_manager_menu: bool,
}

/// Whether the windows are light or dark.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Theme {
    /// as the system: dark when it prefers dark colors
    #[default]
    System,
    Light,
    Dark,
}

impl Theme {
    /// the value of `theme` in the configuration file
    pub fn name(self) -> &'static str {
        match self {
            Theme::System => "system",
            Theme::Light => "light",
            Theme::Dark => "dark",
        }
    }

    /// the theme of a value of `theme` in the configuration file, in any case
    pub fn from_name(name: &str) -> Option<Self> {
        [Theme::System, Theme::Light, Theme::Dark]
            .into_iter()
            .find(|t| t.name().eq_ignore_ascii_case(name.trim()))
    }
}

/// One second of the input channel with a device: what went over the
/// network and how long the pings took, see [`FrontendEvent::LinkStats`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkStats {
    /// the client of the device, `None` for a device that is not a client
    /// here and only connects to this one
    pub handle: Option<ClientHandle>,
    /// address of the device
    pub ip: IpAddr,
    /// round trip time of the pings answered in this second, in
    /// microseconds; `None` if no ping was answered (only the side that
    /// connects pings)
    pub rtt_us: Option<u32>,
    /// pings that went unanswered in this second
    pub lost: u32,
    /// bytes per second sent and received, as on the network: the events
    /// plus an estimate of the DTLS, UDP and IP headers of each datagram
    pub sent_per_s: u64,
    pub received_per_s: u64,
    /// bytes sent and received since the connections were made
    pub sent_total: u64,
    pub received_total: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FrontendEvent {
    /// a client was created
    Created(ClientHandle, ClientConfig, ClientState),
    /// no such client
    NoSuchClient(ClientHandle),
    /// state changed
    State(ClientHandle, ClientConfig, ClientState),
    /// the client was deleted
    Deleted(ClientHandle),
    /// new port, reason of failure (if failed)
    PortChanged(u16, Option<String>),
    /// list of all clients, used for initial state synchronization
    Enumerate(Vec<(ClientHandle, ClientConfig, ClientState)>),
    /// an error occured
    Error(String),
    /// capture status
    CaptureStatus(Status),
    /// emulation status
    EmulationStatus(Status),
    /// authorized public key fingerprints have been updated
    AuthorizedUpdated(HashMap<String, String>),
    /// public key fingerprint of this device
    PublicKeyFingerprint(String),
    /// new device connected
    DeviceConnected {
        addr: SocketAddr,
        fingerprint: String,
    },
    /// incoming device entered the screen
    DeviceEntered {
        fingerprint: String,
        addr: SocketAddr,
        pos: Position,
    },
    /// incoming disconnected
    IncomingDisconnected(SocketAddr),
    /// the file transfers, running and recently finished ones
    Transfers(Vec<TransferInfo>),
    /// what happens to copied files
    FileOptions(FileOptions),
    /// something to tell the user
    Notice(String),
    /// show the main window (picked in the radial menu)
    ShowWindow,
    /// show the window of the file transfers (picked in the radial menu)
    ShowTransfers,
    /// failed connection attempt (approval for fingerprint required)
    ConnectionAttempt {
        fingerprint: String,
        /// entered on the device that tried to connect, it authorizes both
        /// devices on each other
        #[serde(default)]
        code: Option<String>,
    },
    /// the device with `fingerprint` entered the code and is authorized now,
    /// as `name`
    Paired { fingerprint: String, name: String },
    /// outcome of [`FrontendRequest::Pair`]: the name of the other device, or
    /// what went wrong
    PairingResult {
        handle: ClientHandle,
        result: Result<String, String>,
    },
    /// the input channel with every connected device over the last second,
    /// sent once a second
    LinkStats(Vec<LinkStats>),
    /// light or dark windows, from the configuration
    Theme(Theme),
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub enum FrontendRequest {
    /// activate/deactivate client
    Activate(ClientHandle, bool),
    /// add a new client
    Create,
    /// change the listen port (recreate udp listener)
    ChangePort(u16),
    /// remove a client
    Delete(ClientHandle),
    /// request an enumeration of all clients
    Enumerate(),
    /// resolve dns
    ResolveDns(ClientHandle),
    /// update hostname
    UpdateHostname(ClientHandle, Option<String>),
    /// update port
    UpdatePort(ClientHandle, u16),
    /// update position
    UpdatePosition(ClientHandle, Position),
    /// update fix-ips
    UpdateFixIps(ClientHandle, Vec<IpAddr>),
    /// request reenabling input capture
    EnableCapture,
    /// request reenabling input emulation
    EnableEmulation,
    /// synchronize all state
    Sync,
    /// authorize fingerprint (description, fingerprint)
    AuthorizeKey(String, String),
    /// remove fingerprint (fingerprint)
    RemoveAuthorizedKey(String),
    /// change the hook command
    UpdateEnterHook(u64, Option<String>),
    /// change the leave hook command
    UpdateLeaveHook(u64, Option<String>),
    /// save config file
    SaveConfiguration,
    /// cancel the running and waiting file transfers
    CancelTransfers,
    /// cancel one transfer, by its [`TransferInfo::id`]
    CancelTransfer(u64),
    /// send files and folders to the connected devices right away (the
    /// "Copy to RustInput" menu item of file managers)
    SendFiles(Vec<std::path::PathBuf>),
    /// change what happens to copied files (`available` is ignored)
    SetFileOptions(FileOptions),
    /// light or dark windows, kept in the configuration
    SetTheme(Theme),
    /// remove the received files that were not pasted
    RemoveReceivedFiles,
    /// authorize this device and the client on each other with the code the
    /// client shows
    Pair { handle: ClientHandle, code: String },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum Status {
    #[default]
    Disabled,
    Enabled,
}

impl From<Status> for bool {
    fn from(status: Status) -> Self {
        match status {
            Status::Enabled => true,
            Status::Disabled => false,
        }
    }
}

#[cfg(unix)]
const RUSTINPUT_SOCKET_NAME: &str = "rustinput-socket.sock";

#[derive(Debug, Error)]
pub enum SocketPathError {
    #[error("could not determine $XDG_RUNTIME_DIR: `{0}`")]
    XdgRuntimeDirNotFound(VarError),
    #[error("could not determine $HOME: `{0}`")]
    HomeDirNotFound(VarError),
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn default_socket_path() -> Result<PathBuf, SocketPathError> {
    let xdg_runtime_dir =
        env::var("XDG_RUNTIME_DIR").map_err(SocketPathError::XdgRuntimeDirNotFound)?;
    Ok(Path::new(xdg_runtime_dir.as_str()).join(RUSTINPUT_SOCKET_NAME))
}

#[cfg(all(unix, target_os = "macos"))]
pub fn default_socket_path() -> Result<PathBuf, SocketPathError> {
    let home = env::var("HOME").map_err(SocketPathError::HomeDirNotFound)?;
    Ok(Path::new(home.as_str())
        .join("Library")
        .join("Caches")
        .join(RUSTINPUT_SOCKET_NAME))
}

/// Check if a rustinput service is already running by probing the IPC socket.
#[cfg(unix)]
pub fn is_service_running() -> bool {
    let Ok(socket_path) = default_socket_path() else {
        return false;
    };
    std::os::unix::net::UnixStream::connect(socket_path).is_ok()
}

/// Check if a rustinput service is already running by probing the IPC socket.
#[cfg(windows)]
pub fn is_service_running() -> bool {
    std::net::TcpStream::connect("127.0.0.1:5252").is_ok()
}

/// the name of the Windows service (`rustinput windows-service`), installed
/// by update-rustinput.ps1
#[cfg(windows)]
pub const WINDOWS_SERVICE: &str = "RustInput";

/// Whether the Windows service runs, which starts the daemon in the session
/// of the user at the screen, with the administrator rights of that user:
/// the window then waits for that daemon instead of starting one of its own,
/// which would have no such rights and keep the port from the right one.
#[cfg(windows)]
pub fn windows_service_runs() -> bool {
    use windows_service::{
        service::{ServiceAccess, ServiceState},
        service_manager::{ServiceManager, ServiceManagerAccess},
    };
    let Ok(manager) = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
    else {
        return false;
    };
    let Ok(service) = manager.open_service(WINDOWS_SERVICE, ServiceAccess::QUERY_STATUS) else {
        return false;
    };
    service.query_status().is_ok_and(|status| {
        matches!(
            status.current_state,
            ServiceState::Running | ServiceState::StartPending
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_names() {
        for theme in [Theme::System, Theme::Light, Theme::Dark] {
            assert_eq!(Theme::from_name(theme.name()), Some(theme));
        }
        assert_eq!(Theme::from_name(" Dark "), Some(Theme::Dark));
        assert_eq!(Theme::from_name("escuro"), None);
        assert_eq!(Theme::default(), Theme::System);
    }
}
