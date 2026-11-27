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
pub use input_event::edge::{EdgeLink, Span};

/// the arrangement of the computers
pub mod layout;

/// "512 bytes", "1.5 KB", "120.3 MB", "2.1 GB" (powers of 1024, like file
/// managers)
/// whether `c` changes the direction of the text around it
fn bidi(c: char) -> bool {
    matches!(c, '\u{200e}' | '\u{200f}' | '\u{061c}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// A name another computer gave (its host name, a name it was authorized
/// with), safe to show and store: at most `max` characters, without control
/// characters nor those that change the direction of the text (which could
/// make one name look like another), trimmed.
pub fn plain_text(text: &str, max: usize) -> String {
    let text: String = text
        .chars()
        .filter(|&c| !c.is_control() && !bidi(c))
        .take(max)
        .collect();
    text.trim().to_owned()
}

/// A file name as it can be shown safely: the characters that change the
/// direction of the text (which make "photo\u{202e}gpj.exe" look like
/// "photoexe.jpg") and other control characters as ⟨U+202E⟩.
pub fn visible_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if bidi(c) || c.is_control() {
                format!("⟨U+{:04X}⟩", c as u32)
            } else {
                c.to_string()
            }
        })
        .collect()
}

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
#[cfg(windows)]
mod windows_pipe;
#[cfg(windows)]
pub use windows_pipe::{pipe_exists_in, pipe_name, pipe_name_in};

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
    /// the part of the edge at `pos` that leads to the client, and the part
    /// of its edge it leads to: the whole edges unless arranged otherwise
    #[serde(default)]
    pub link: EdgeLink,
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
            link: EdgeLink::FULL,
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
    /// the client is paused by its user: it neither controls nor is
    /// controlled (it answers, but `alive` is false)
    #[serde(default)]
    pub paused: bool,
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
    /// Went on where it stopped: the bytes that were there already then.
    /// `started_ms` is when it went on, so the speed counts the rest only.
    #[serde(default)]
    pub resumed_bytes: Option<u64>,
    /// what of the copied files did not go or come as it was, and why
    /// (0.39.0)
    #[serde(default)]
    pub left_out: Vec<LeftOut>,
}

/// A computer running RustInput found on the local network (0.40.0).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Discovered {
    /// its host name
    pub name: String,
    pub address: IpAddr,
    /// the port of its RustInput
    pub port: u16,
    /// the fingerprint it says its certificate has (not proven: adding it
    /// does not authorize it)
    pub fingerprint: String,
    pub version: String,
    /// as `std::env::consts::OS`: "linux", "windows", "macos"
    pub os: String,
    /// its fingerprint is authorized here
    pub authorized: bool,
    /// already among the computers of this one (by its address or name)
    pub added: bool,
}

/// Something of a transfer that did not go or come as it was copied, and
/// why. Also part of the file channel between devices, so `why` is a word
/// a newer RustInput may add to, shown as `detail` when it is not known.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeftOut {
    /// path inside the transfer, components separated by `/`
    pub path: String,
    /// one of the `LEFT_OUT_*` words
    pub why: String,
    /// the error, or the new name for [`LEFT_OUT_RENAMED`]
    #[serde(default)]
    pub detail: String,
}

/// left out: a link to a folder, which could lead into itself
pub const LEFT_OUT_LINK: &str = "link";
/// left out: it could not be read (`detail`: the error)
pub const LEFT_OUT_UNREADABLE: &str = "unreadable";
/// left out: neither a file nor a folder (a socket, a device)
pub const LEFT_OUT_SPECIAL: &str = "special";
/// left out: it changed while it was sent
pub const LEFT_OUT_CHANGED: &str = "changed";
/// left out: it was removed before it was sent
pub const LEFT_OUT_GONE: &str = "gone";
/// came, but with another name (`detail`), its own is not allowed on the
/// receiving system
pub const LEFT_OUT_RENAMED: &str = "renamed";

impl LeftOut {
    pub fn new(path: impl Into<String>, why: &str, detail: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            why: why.to_owned(),
            detail: detail.into(),
        }
    }

    /// came anyway, with another name
    pub fn came(&self) -> bool {
        self.why == LEFT_OUT_RENAMED
    }
}

impl TransferInfo {
    /// still waiting, running or waiting to go on: can be cancelled
    pub fn active(&self) -> bool {
        matches!(
            self.state,
            TransferState::Waiting | TransferState::Running | TransferState::Interrupted
        )
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
    /// The connection was lost: what arrived is kept, and the transfer goes
    /// on from there when the other device is connected again (0.37.0).
    Interrupted,
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
    /// Files sent by themselves because they were copied when the pointer
    /// came here (`send_copied_files` on the other computer) larger than
    /// this many MB (of 1024 × 1024 bytes) are only received if the user
    /// says so, see [`FrontendEvent::AskTransfer`]; 0: never asked
    /// (`ask_before_receiving_mb`, 0.38.0)
    #[serde(default)]
    pub ask_above_mb: u64,
}

/// how long [`FrontendEvent::AskTransfer`] waits for an answer: then the
/// files are not received
pub const ASK_TRANSFER_SECONDS: u64 = 120;

/// What the clipboard of a computer holds, as the Clipboard tab shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardPeek {
    /// nothing, or nothing RustInput takes along
    Empty,
    /// text: its start (at most [`CLIPBOARD_PEEK_TEXT`] bytes), how many
    /// characters and lines it has, and whether it is formatted (HTML)
    Text {
        start: String,
        chars: u64,
        lines: u64,
        formatted: bool,
    },
    /// an image: its size and a small PNG of it (at most
    /// [`CLIPBOARD_THUMBNAIL`] pixels each way)
    Image {
        width: u32,
        height: u32,
        #[serde(default)]
        thumbnail: Vec<u8>,
    },
    /// copied files and folders: the names of the first ones (at most
    /// [`CLIPBOARD_PEEK_FILES`]) and how many there are
    Files { names: Vec<String>, count: u64 },
    /// something a password manager marked as secret: not shown, but it can
    /// be brought here
    Secret,
}

/// longest start of a text shown of a clipboard, in bytes
pub const CLIPBOARD_PEEK_TEXT: usize = 4096;
/// longest start of a text in the history of a clipboard, in bytes
pub const CLIPBOARD_HISTORY_TEXT: usize = 1024;
/// largest thumbnail of an image in the history of a clipboard, in pixels
/// each way
pub const CLIPBOARD_HISTORY_THUMBNAIL: u32 = 128;
/// copies kept in the history of a clipboard when the configuration does not
/// say (`clipboard_history`)
pub const CLIPBOARD_HISTORY_DEFAULT: usize = 30;

/// Something the clipboard of a computer held, in its history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardHistoryItem {
    /// [`FrontendRequest::BringClipboard`] with it brings it here
    pub hash: u64,
    /// when it was copied, in seconds since 1970 (on the clock of that
    /// computer)
    pub when: u64,
    /// shortened: [`CLIPBOARD_HISTORY_TEXT`], [`CLIPBOARD_HISTORY_THUMBNAIL`]
    pub peek: ClipboardPeek,
}

/// The history of the clipboard of a computer
/// ([`FrontendEvent::ClipboardHistory`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardHistory {
    /// what it copied while RustInput ran, the latest first, without what it
    /// holds now
    Items(Vec<ClipboardHistoryItem>),
    /// it keeps none (`clipboard_history = 0`)
    Off,
    /// it can not tell when something is copied on its system (Wayland)
    Unsupported,
    /// it runs a RustInput without history (before 0.35.0)
    Older,
}
/// largest thumbnail of an image on a clipboard, in pixels each way
pub const CLIPBOARD_THUMBNAIL: u32 = 320;
/// most names of copied files shown of a clipboard
pub const CLIPBOARD_PEEK_FILES: usize = 12;

/// The clipboard of a computer, as far as this one can see it
/// ([`FrontendEvent::Clipboard`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardSeen {
    /// it holds `peek`; [`FrontendRequest::BringClipboard`] with `hash`
    /// brings it here
    Holds { hash: u64, peek: ClipboardPeek },
    /// it does not show its clipboard: switched off there, or no clipboard
    /// sharing at all
    Hidden,
    /// it could not be asked, and why
    Unavailable(String),
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

/// What RustInput knows about its newer versions, see
/// [`FrontendEvent::Updates`].
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateState {
    /// whether it asks GitHub for the latest version, `None` when the user
    /// was not asked yet
    pub check: Option<bool>,
    /// asking right now
    pub checking: bool,
    /// the latest version on GitHub, once known ("0.27.0")
    pub latest: Option<String>,
    /// the latest version is newer than this one
    pub available: bool,
    /// the page of the latest release
    pub url: String,
    /// why the last question failed
    pub error: Option<String>,
    /// the new version the user was told about with a notification of the
    /// system, kept in the configuration: once per version
    /// ([`FrontendRequest::UpdateNotified`])
    #[serde(default)]
    pub notified: Option<String>,
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
    /// The computer `peer` sends `files` files of `bytes` bytes that were
    /// copied there when the pointer came here, more than
    /// [`FileOptions::ask_above_mb`]: received only if the user says so
    /// with [`FrontendRequest::AnswerTransfer`] within
    /// [`ASK_TRANSFER_SECONDS`] (0.38.0).
    AskTransfer {
        id: u64,
        peer: String,
        files: u32,
        bytes: u64,
    },
    /// the computers running RustInput found on the network (0.40.0)
    Discovered(Vec<Discovered>),
    /// whether the keyboard shortcuts to move between the computers are on
    /// (`hotkeys`, 0.42.0)
    Hotkeys(bool),
    /// The authorized computer `by` was paired with the computer
    /// `fingerprint` called `name` (or tells about it, one of those it was
    /// paired with): authorize it here too? Answered with
    /// [`FrontendRequest::AnswerIntroduction`] (0.41.0).
    Introduction {
        by: String,
        name: String,
        fingerprint: String,
    },
    /// whether this computer tells the others about itself and looks for
    /// them (`discovery`)
    Discovery(bool),
    /// show the main window on its Clipboards tab (picked in the radial menu;
    /// [`FrontendEvent::ShowClipboard`] is whether the others see this one)
    ShowClipboardsTab,
    /// The computer `name` (the client `handle`) answered with a certificate
    /// that is not authorized here: no input goes to it until `fingerprint`
    /// is authorized (or the two are paired with a code).
    ServerNotAuthorized {
        handle: ClientHandle,
        name: String,
        fingerprint: String,
    },
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
    /// this computer, the arrangement and the computers it connects to,
    /// answering [`FrontendRequest::Arrangement`], again as they describe
    /// themselves
    Arrangement(layout::ArrangementView),
    /// the arrangement went to the computer `name`, or did not (`error`)
    ArrangementSent { name: String, error: Option<String> },
    /// another computer, `by`, changed the arrangement
    ArrangementChanged { by: String },
    /// the keyboard and the mouse of this computer control the client, or
    /// this computer itself (`None`)
    PointerAt(Option<ClientHandle>),
    /// whether there is a newer version of RustInput, and whether to ask
    Updates(UpdateState),
    /// whether RustInput is paused on this computer
    /// ([`FrontendRequest::SetPaused`])
    Paused(bool),
    /// the computer `name` was asked to show the numbers of its monitors
    /// ([`FrontendRequest::IdentifyMonitors`]) and did not (`error`)
    MonitorsNotIdentified { name: String, error: String },
    /// the screen of the computer `name` (this one too) was to lock, from
    /// the radial menu, and did not (`error`)
    ScreenNotLocked { name: String, error: String },
    /// what the clipboard of the client (of this computer: `None`) holds,
    /// answering [`FrontendRequest::PeekClipboards`]
    Clipboard {
        handle: Option<ClientHandle>,
        name: String,
        seen: ClipboardSeen,
    },
    /// the clipboard of the computer `name` was brought here
    /// ([`FrontendRequest::BringClipboard`]), or not (`error`); `files`
    /// when it held files, which come as a file transfer
    ClipboardBrought {
        /// `None`: a copy of the history of this computer is back on its
        /// clipboard
        handle: Option<ClientHandle>,
        name: String,
        files: bool,
        error: Option<String>,
    },
    /// whether the other computers see the clipboard of this one
    /// (`show_clipboard` in the configuration)
    ShowClipboard(bool),
    /// the history of the clipboard of the client (of this computer:
    /// `None`), answering [`FrontendRequest::PeekClipboards`] when it changed
    ClipboardHistory {
        handle: Option<ClientHandle>,
        history: ClipboardHistory,
    },
    /// whether this computer keeps a history of its clipboard
    /// (`clipboard_history` in the configuration is not 0)
    KeepClipboardHistory(bool),
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
    /// change where the client meets this device on the edge at its position
    UpdateLink(ClientHandle, EdgeLink),
    /// the arrangement of the computers, with their monitors
    /// ([`FrontendEvent::Arrangement`])
    Arrangement,
    /// arrange the computers so, here and on the others
    Arrange(layout::Layout),
    /// show the number of each monitor on it for a few seconds, here and
    /// on the other computers, as the arrangement numbers them
    /// ([`layout::monitor_numbers`])
    IdentifyMonitors,
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
    /// the answer to [`FrontendEvent::AskTransfer`] `id`: receive the
    /// files or not
    AnswerTransfer { id: u64, receive: bool },
    /// a window shows the computers found on the network (or no longer):
    /// look for them now and then
    Discover(bool),
    /// switches the keyboard shortcuts to move between the computers on or
    /// off
    SetHotkeys(bool),
    /// moves the mouse and keyboard to this computer (`None`) or to a client
    /// without crossing an edge (from the tray icon, 0.43.0)
    GoTo(Option<ClientHandle>),
    /// the answer to [`FrontendEvent::Introduction`] of `fingerprint`
    AnswerIntroduction {
        fingerprint: String,
        authorize: bool,
    },
    /// tell the others about this computer and look for them, or not
    SetDiscovery(bool),
    /// adds a computer found on the network: its name, address and port
    AddDiscovered {
        name: String,
        address: IpAddr,
        port: u16,
    },
    /// send files and folders to the connected devices right away (the
    /// "Copy to RustInput" menu item of file managers)
    SendFiles(Vec<std::path::PathBuf>),
    /// change what happens to copied files (`available` is ignored)
    SetFileOptions(FileOptions),
    /// light or dark windows, kept in the configuration
    SetTheme(Theme),
    /// the language of the windows, menus and the radial menu ("pt-BR"),
    /// `None` for that of the system, kept in the configuration
    SetLanguage(Option<String>),
    /// whether to ask GitHub for the latest version, kept in the
    /// configuration ([`FrontendEvent::Updates`])
    SetCheckUpdates(bool),
    /// ask GitHub for the latest version now
    CheckUpdates,
    /// the user was told about the new version with a notification of the
    /// system (or saw it in the window), so it is not told again; kept in
    /// the configuration ([`UpdateState::notified`])
    UpdateNotified(String),
    /// Pause RustInput on this computer, or let it go on: paused, the mouse
    /// and keyboard stay here and no other computer controls this one. Kept
    /// in the configuration ([`FrontendEvent::Paused`]).
    SetPaused(bool),
    /// remove the received files that were not pasted
    RemoveReceivedFiles,
    /// authorize this device and the client on each other with the code the
    /// client shows
    Pair { handle: ClientHandle, code: String },
    /// What the clipboards of this computer and of the clients hold
    /// ([`FrontendEvent::Clipboard`]): only what changed since the last
    /// time, or everything (`all`). The Clipboard tab asks every few seconds
    /// while it is shown.
    PeekClipboards { all: bool },
    /// Bring what the clipboard of the client holds, or held (its
    /// history), here, if it still has `hash`
    /// ([`FrontendEvent::ClipboardBrought`]); `None`: something of the
    /// history of this computer back on its clipboard.
    BringClipboard {
        handle: Option<ClientHandle>,
        hash: u64,
    },
    /// whether this computer keeps a history of its clipboard, kept in the
    /// configuration ([`FrontendEvent::KeepClipboardHistory`])
    SetClipboardHistory(bool),
    /// forget the history of the clipboard of this computer
    ClearClipboardHistory,
    /// whether the other computers see the clipboard of this one, kept in
    /// the configuration ([`FrontendEvent::ShowClipboard`])
    SetShowClipboard(bool),
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

/// Check if a rustinput service is already running in this session: whether
/// the named pipe of the session exists.
#[cfg(windows)]
pub fn is_service_running() -> bool {
    let mut session = 0;
    unsafe {
        windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId(
            std::process::id(),
            &mut session,
        )
    };
    windows_pipe::pipe_exists_in(session)
}

/// the name of the Windows service (`rustinput windows-service`), installed
/// by update-rustinput.ps1
#[cfg(windows)]
pub const WINDOWS_SERVICE: &str = "RustInput";

/// Whether the Windows service runs, which starts the daemon in the session
/// of the user at the screen, with the administrator rights of that user:
/// the window then waits for that daemon instead of starting one of its own,
/// which would have no such rights and keep the pipe from the right one.
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
