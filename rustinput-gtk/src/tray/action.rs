/// what the tray icon asks the application to do
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TrayAction {
    /// show the main window
    Show,
    /// quit RustInput, which also stops the service it started
    Quit,
    /// cancel the running and waiting file transfers
    CancelTransfers,
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
}
