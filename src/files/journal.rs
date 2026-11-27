//! Log of the file transfers, kept across restarts so the user can check what
//! was sent and received (see [`rustinput_ipc::transfer_log`]).

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::PathBuf,
};

/// the log starts over (keeping the previous one) beyond this size
const MAX_SIZE: u64 = 5 * 1024 * 1024;
/// files listed per transfer at most
const MAX_FILES: usize = 1000;

pub(crate) struct Journal {
    file: Option<File>,
}

impl Journal {
    pub(crate) fn open() -> Self {
        let file = rustinput_ipc::transfer_log().and_then(|path| match open(&path) {
            Ok(file) => Some(file),
            Err(e) => {
                log::warn!("could not open the transfer log {path:?}: {e}");
                None
            }
        });
        Self { file }
    }

    /// a line about a transfer, with the time
    pub(crate) fn write(&mut self, text: &str) {
        self.write_with_files(text, &[]);
    }

    /// a line about a transfer followed by its files
    pub(crate) fn write_with_files(&mut self, text: &str, files: &[(String, u64)]) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let mut entry = format!("{}  {}\n", local_time(), rustinput_ipc::visible_name(text));
        for (path, size) in files.iter().take(MAX_FILES) {
            entry.push_str(&format!(
                "                     {} ({})\n",
                rustinput_ipc::visible_name(path),
                rustinput_ipc::human_size(*size)
            ));
        }
        if files.len() > MAX_FILES {
            entry.push_str(&format!(
                "                     ... and {} more\n",
                files.len() - MAX_FILES
            ));
        }
        if let Err(e) = file.write_all(entry.as_bytes()) {
            log::debug!("could not write the transfer log: {e}");
        }
    }
}

fn open(path: &PathBuf) -> std::io::Result<File> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    if fs::metadata(path).is_ok_and(|m| m.len() > MAX_SIZE) {
        let _ = fs::rename(path, path.with_extension("log.old"));
    }
    OpenOptions::new().create(true).append(true).open(path)
}

/// "2026-11-20 10:15:03" in the local time zone
fn local_time() -> String {
    #[cfg(unix)]
    {
        let now = unsafe { libc::time(std::ptr::null_mut()) };
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        if unsafe { libc::localtime_r(&now, &mut tm) }.is_null() {
            return String::new();
        }
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::GetLocalTime;
        let mut t = unsafe { std::mem::zeroed() };
        unsafe { GetLocalTime(&mut t) };
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
        )
    }
}
