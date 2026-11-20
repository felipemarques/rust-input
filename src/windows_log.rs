//! Without a console of its own, RustInput logs to the terminal it was started
//! from, or else into a file in [`rustinput_ipc::log_dir`].

use std::fs::{self, OpenOptions};

use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};

/// logs of "Copy to RustInput" are kept up to this size
const MAX_APPENDED: u64 = 1024 * 1024;

pub(super) fn setup(logger: &mut env_logger::Builder) {
    if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) } != 0 {
        return;
    }
    let Some(dir) = rustinput_ipc::log_dir() else {
        return;
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (name, append) = if args.iter().any(|a| a == "daemon") {
        ("daemon.log", false)
    } else if args.iter().any(|a| a == "send-files") {
        // started once per selected item, the log of every start counts
        ("send-files.log", true)
    } else {
        ("rustinput.log", false)
    };
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join(name);
    let previous = path.with_extension("old.log");
    let too_large = fs::metadata(&path).is_ok_and(|m| m.len() > MAX_APPENDED);
    if !append || too_large {
        let _ = fs::rename(&path, &previous);
    }
    let file = OpenOptions::new().create(true).append(true).open(&path);
    if let Ok(file) = file {
        logger.target(env_logger::Target::Pipe(Box::new(file)));
    }
}
