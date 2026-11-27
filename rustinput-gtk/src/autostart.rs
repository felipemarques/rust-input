//! Starting RustInput with the session on Linux and the BSDs (0.47.0): an
//! entry in the autostart folder of the desktop (XDG Autostart), which
//! starts it without its window. Windows has its installer for this, macOS
//! its login items.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// the name of the entry, the one of the desktop file of RustInput
const ENTRY: &str = "io.github.felipemarques.RustInput.desktop";

/// `$XDG_CONFIG_HOME/autostart/…`, `~/.config/autostart/…` without it
pub(crate) fn path() -> Option<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(config.join("autostart").join(ENTRY))
}

/// whether RustInput starts with the session
pub(crate) fn is_on() -> bool {
    path().is_some_and(|p| p.is_file())
}

/// what starts this RustInput: the AppImage it runs from, or its executable
fn program() -> io::Result<PathBuf> {
    match std::env::var_os("APPIMAGE").map(PathBuf::from) {
        Some(appimage) if appimage.is_file() => Ok(appimage),
        _ => std::env::current_exe(),
    }
}

/// a value of a desktop file in double quotes (its own escapes)
fn quoted(path: &Path) -> String {
    let text = path.to_string_lossy();
    let mut out = String::from("\"");
    for c in text.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// the entry that starts `program` without its window
fn entry(program: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=RustInput\n\
         Comment=Mouse and keyboard sharing\n\
         Exec=env RUSTINPUT_HIDDEN=1 {}\n\
         Icon=io.github.felipemarques.RustInput\n\
         Terminal=false\n\
         X-GNOME-Autostart-enabled=true\n",
        quoted(program)
    )
}

/// starts RustInput with the session, or no longer
pub(crate) fn set(on: bool) -> io::Result<()> {
    let path = path().ok_or_else(|| io::Error::other("no folder for the autostart entry"))?;
    if on {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(&path, entry(&program()?))
    } else {
        match fs::remove_file(&path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_entry_starts_it_without_its_window() {
        let text = entry(Path::new("/opt/Rust Input/rustinput"));
        assert!(text.contains("Exec=env RUSTINPUT_HIDDEN=1 \"/opt/Rust Input/rustinput\"\n"));
        assert!(text.starts_with("[Desktop Entry]\n"));
        assert_eq!(quoted(Path::new("/a/$b\"c")), "\"/a/\\$b\\\"c\"");
    }
}
