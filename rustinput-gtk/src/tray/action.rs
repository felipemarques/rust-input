/// what the tray icon asks the application to do
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TrayAction {
    /// show the main window
    Show,
    /// quit RustInput, which also stops the service it started
    Quit,
    /// The tray icon disappeared or came back, for example because the
    /// panel showing it restarted. While it is gone, closing the window
    /// quits RustInput as there would be no way to get the window back.
    #[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
    Available(bool),
}
