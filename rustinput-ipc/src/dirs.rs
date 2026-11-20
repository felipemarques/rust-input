//! Where RustInput keeps its files, shared by the service and the interface.

use std::path::PathBuf;

/// per user folder for data that can be recreated
fn cache_base() -> Option<PathBuf> {
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Caches"));
    #[cfg(all(unix, not(target_os = "macos")))]
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")));
    base.map(|b| b.join("rustinput"))
}

/// per user folder for logs
fn state_base() -> Option<PathBuf> {
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("rustinput"));
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Logs/rustinput"));
    #[cfg(all(unix, not(target_os = "macos")))]
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .map(|d| d.join("rustinput"));
    base
}

/// folder holding the received files until they are pasted
pub fn received_dir() -> Option<PathBuf> {
    cache_base().map(|b| b.join("received"))
}

/// log of the file transfers, kept across restarts
pub fn transfer_log() -> Option<PathBuf> {
    state_base().map(|b| b.join("transfers.log"))
}

/// folder of the log files written when there is no console to log to
pub fn log_dir() -> Option<PathBuf> {
    state_base().map(|b| b.join("logs"))
}
