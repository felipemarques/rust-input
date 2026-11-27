/// what the tray icon asks the application to do
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TrayAction {
    /// show the main window
    Show,
    /// quit RustInput, which also stops the service it started
    Quit,
    /// cancel the running and waiting file transfers
    CancelTransfers,
    /// open the window with the file transfers
    ShowTransfers,
    /// open the Clipboards tab of the window
    ShowClipboards,
    /// turn sending copied files along with the pointer on or off
    ToggleSendCopied,
    /// get the new version of RustInput
    Update,
    /// pause RustInput, or let it go on
    TogglePause,
    /// the mouse and keyboard to this computer (`None`) or to a client
    GoTo(Option<u64>),
    /// The tray icon disappeared or came back, for example because the
    /// panel showing it restarted. While it is gone, closing the window
    /// quits RustInput as there would be no way to get the window back.
    #[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
    Available(bool),
}

/// what the tray shows about the file transfers
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Summary {
    /// one line per transfer, running ones first
    pub(crate) lines: Vec<String>,
    /// what runs right now, for the tooltip
    pub(crate) status: Option<String>,
    /// some transfer can still be cancelled
    pub(crate) cancellable: bool,
    /// whether copied files go along with the pointer, `None` without file
    /// transfers
    pub(crate) send_copied: Option<bool>,
    /// the item that gets a new version of RustInput, when there is one
    pub(crate) update: Option<String>,
    /// the line of the tooltip about the new version, when there is one;
    /// the icon then has a green sign with an arrow up
    pub(crate) update_status: Option<String>,
    /// whether RustInput is paused, `None` until the service said
    pub(crate) paused: Option<bool>,
    /// this computer and the active clients, to move the mouse and keyboard
    /// to (0.43.0); empty without clients
    pub(crate) computers: Vec<Computer>,
    /// the name of the computer the mouse and keyboard are on, when not this
    /// one: the icon has a blue sign with an arrow
    pub(crate) away: Option<String>,
}

/// a computer of the menu of the icon
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Computer {
    /// the client, `None` for this computer
    pub(crate) handle: Option<u64>,
    pub(crate) name: String,
    /// the mouse and keyboard are on it
    pub(crate) here: bool,
}
