//! Received files on the Windows clipboard.

use std::path::PathBuf;

/// Puts `paths` on the clipboard as cut files: the file list (`CF_HDROP`) and
/// "Preferred DropEffect" saying move, like Explorer does for Ctrl+X.
pub(crate) fn write_cut_files(paths: &[PathBuf]) -> Result<(), String> {
    use clipboard_win::{Clipboard, raw};
    /// `DROPEFFECT_MOVE`
    const MOVE: u32 = 2;
    let paths: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
    let _open = Clipboard::new_attempts(10).map_err(|e| e.to_string())?;
    raw::empty().map_err(|e| e.to_string())?;
    raw::set_file_list(&paths).map_err(|e| e.to_string())?;
    let effect = raw::register_format("Preferred DropEffect").ok_or("no clipboard format")?;
    raw::set_without_clear(effect.get(), &MOVE.to_le_bytes()).map_err(|e| e.to_string())?;
    Ok(())
}
