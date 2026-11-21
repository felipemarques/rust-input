//! "Copy to RustInput" in the menus of file managers: an action for Nemo, a
//! script for Nautilus, and a verb of all files and folders in the registry
//! of the user for Windows Explorer. They start `rustinput send-files` with
//! the selected items, which the service sends to the connected devices.

use std::path::PathBuf;

/// adds the menu items, or removes them when `enabled` is false
pub(crate) fn install(enabled: bool) {
    let Some(exe) = executable() else {
        log::warn!("the path of the program is unknown, no file manager menu");
        return;
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    linux::install(enabled, &exe);
    #[cfg(windows)]
    windows::install(enabled, &exe);
    #[cfg(target_os = "macos")]
    let _ = (enabled, exe);
}

/// the program the menu items start: an AppImage itself, not its temporary
/// mount point
fn executable() -> Option<PathBuf> {
    std::env::var_os("APPIMAGE")
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .or_else(|| std::env::current_exe().ok())
}

/// `text` quoted for a shell (and for the command lines of Nemo actions)
#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

#[cfg(all(unix, not(target_os = "macos")))]
mod linux {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
    };

    use super::shell_quote;

    const NEMO_ACTION: &str = "nemo/actions/rustinput-copy.nemo_action";
    const NAUTILUS_SCRIPT: &str = "nautilus/scripts/Copy to RustInput";

    fn data_home() -> Option<PathBuf> {
        std::env::var_os("XDG_DATA_HOME")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
    }

    fn installed(program: &str) -> bool {
        std::env::var_os("PATH")
            .is_some_and(|path| std::env::split_paths(&path).any(|d| d.join(program).is_file()))
    }

    pub(super) fn install(enabled: bool, exe: &Path) {
        let Some(data) = data_home() else {
            return;
        };
        let exe = shell_quote(&exe.display().to_string());
        // the name and the comment in every language of RustInput: Nemo
        // shows those of the language of the desktop
        let (name, comment) = (
            rustinput_i18n::mark("Copy to RustInput"),
            rustinput_i18n::mark("Send to the other computer, ready to be pasted there"),
        );
        let mut names = String::new();
        for language in &rustinput_i18n::LANGUAGES[1..] {
            let file = language.file;
            if let Some(text) = rustinput_i18n::translate(*language, None, name) {
                names.push_str(&format!("Name[{file}]={text}\n"));
            }
            if let Some(text) = rustinput_i18n::translate(*language, None, comment) {
                names.push_str(&format!("Comment[{file}]={text}\n"));
            }
        }
        let action = format!(
            "[Nemo Action]\n\
             # added by RustInput, which removes it when its file manager menu is turned off\n\
             Name={name}\n\
             Comment={comment}\n\
             {names}\
             Exec={exe} send-files %F\n\
             Icon-Name=document-send\n\
             Selection=notnone\n\
             Extensions=any;\n"
        );
        update(
            &data.join(NEMO_ACTION),
            enabled && installed("nemo"),
            &action,
            false,
        );
        let script = format!(
            "#!/bin/sh\n\
             # added by RustInput, which removes it when its file manager menu is turned off\n\
             exec {exe} send-files \"$@\"\n"
        );
        update(
            &data.join(NAUTILUS_SCRIPT),
            enabled && installed("nautilus"),
            &script,
            true,
        );
    }

    /// writes `contents` to `path` if `wanted`, removes the file (if RustInput
    /// wrote it) otherwise
    fn update(path: &Path, wanted: bool, contents: &str, executable: bool) {
        let current = fs::read_to_string(path).ok();
        if !wanted {
            if current.is_some_and(|c| c.contains("added by RustInput")) {
                match fs::remove_file(path) {
                    Ok(()) => log::info!("removed the file manager menu item {path:?}"),
                    Err(e) => log::warn!("could not remove {path:?}: {e}"),
                }
            }
            return;
        }
        if current.as_deref() == Some(contents) {
            return;
        }
        let write = || {
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir)?;
            }
            fs::write(path, contents)?;
            if executable {
                fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
            }
            Ok::<_, std::io::Error>(())
        };
        match write() {
            Ok(()) => log::info!("added \"Copy to RustInput\" to the file manager: {path:?}"),
            Err(e) => log::warn!("could not write {path:?}: {e}"),
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::{iter, path::Path, ptr};

    use windows_sys::Win32::{
        Foundation::ERROR_SUCCESS,
        System::Registry::{
            HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
            RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW,
        },
    };

    /// files and folders alike
    const KEY: &str = r"Software\Classes\AllFilesystemObjects\shell\RustInput.CopyTo";

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(iter::once(0)).collect()
    }

    pub(super) fn install(enabled: bool, exe: &Path) {
        if !enabled {
            let key = wide(KEY);
            if unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, key.as_ptr()) } == ERROR_SUCCESS {
                log::info!("removed \"Copy to RustInput\" from Explorer");
            }
            return;
        }
        let exe = exe.display().to_string();
        // in the language of RustInput, written again when it changes
        let label = rustinput_i18n::tr("Copy to RustInput");
        // Explorer starts the command once per selected item, up to 100 items
        // with the "Player" model (15 otherwise): the service puts together
        // what arrives in quick succession.
        let command = format!("\"{exe}\" send-files \"%1\"");
        let icon = format!("\"{exe}\",0");
        let values = [
            (KEY.to_owned(), None, label),
            (KEY.to_owned(), Some("MUIVerb"), label),
            (KEY.to_owned(), Some("Icon"), icon.as_str()),
            (KEY.to_owned(), Some("MultiSelectModel"), "Player"),
            (format!(r"{KEY}\command"), None, command.as_str()),
        ];
        for (key, name, value) in values {
            if let Err(code) = set(&key, name, value) {
                log::warn!("could not add \"Copy to RustInput\" to Explorer: error {code}");
                return;
            }
        }
        log::debug!("\"Copy to RustInput\" is in the menu of Explorer");
    }

    /// sets a string value below HKEY_CURRENT_USER, the default one if `name`
    /// is `None`
    fn set(key: &str, name: Option<&str>, value: &str) -> Result<(), u32> {
        let key = wide(key);
        let mut hkey: HKEY = ptr::null_mut();
        let created = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                0,
                ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                ptr::null(),
                &mut hkey,
                ptr::null_mut(),
            )
        };
        if created != ERROR_SUCCESS {
            return Err(created);
        }
        let name = name.map(wide);
        let data = wide(value);
        let written = unsafe {
            RegSetValueExW(
                hkey,
                name.as_ref().map_or(ptr::null(), |n| n.as_ptr()),
                0,
                REG_SZ,
                data.as_ptr().cast(),
                (data.len() * 2) as u32,
            )
        };
        unsafe { RegCloseKey(hkey) };
        if written == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(written)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::shell_quote;

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("/usr/bin/rustinput"), "'/usr/bin/rustinput'");
        assert_eq!(shell_quote("/home/o'neil/x"), r"'/home/o'\''neil/x'");
    }
}
