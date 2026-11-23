//! Whether a password manager marked what the clipboard holds as secret.
//!
//! Password managers add a format of their own next to the password they
//! copy, so that clipboard histories leave it out: KeePassXC and KDE
//! `x-kde-passwordManagerHint` (X11), KeePass, 1Password and others
//! `ExcludeClipboardContentFromMonitorProcessing` or
//! `CanIncludeInClipboardHistory` set to 0 (Windows). The Clipboard tab of
//! the other computers then does not show it (it can still be brought over).
//! Not looked for on Wayland and macOS yet.

/// the formats that mark the content as secret by being there
const SECRET_FORMATS: &[&str] = &[
    "x-kde-passwordManagerHint",
    "ExcludeClipboardContentFromMonitorProcessing",
    "Clipboard Viewer Ignore",
    "org.nspasteboard.ConcealedType",
];

/// whether the clipboard offering the format `name` holds a secret
fn is_secret_format(name: &str) -> bool {
    SECRET_FORMATS.contains(&name)
}

/// Looks at the formats the clipboard offers; kept by the clipboard thread
/// between questions (on X11 it holds a connection to the X server).
#[derive(Default)]
pub(super) struct Marks {
    #[cfg(all(unix, not(target_os = "macos")))]
    x11: Option<x11::Targets>,
}

impl Marks {
    /// whether what the clipboard holds now is marked as secret
    pub(super) fn secret(&mut self) -> bool {
        #[cfg(all(unix, not(target_os = "macos")))]
        if super::x11_files::is_x11() {
            if self.x11.is_none() {
                match x11::Targets::new() {
                    Ok(targets) => self.x11 = Some(targets),
                    Err(e) => {
                        log::debug!("could not look at the clipboard formats: {e}");
                        return false;
                    }
                }
            }
            let targets = self.x11.as_mut().expect("just set");
            return match targets.names() {
                Ok(names) => names.iter().any(|n| is_secret_format(n)),
                Err(e) => {
                    log::debug!("could not look at the clipboard formats: {e}");
                    // a new connection next time
                    self.x11 = None;
                    false
                }
            };
        }
        #[cfg(windows)]
        return windows::secret();
        #[allow(unreachable_code)]
        false
    }
}

#[cfg(windows)]
mod windows {
    use clipboard_win::raw::{get_vec, is_format_avail, register_format};

    pub(super) fn secret() -> bool {
        let marked = super::SECRET_FORMATS
            .iter()
            .any(|name| register_format(name).is_some_and(|format| is_format_avail(format.get())));
        marked || not_for_history()
    }

    /// `CanIncludeInClipboardHistory` holding the number 0
    fn not_for_history() -> bool {
        let Some(format) = register_format("CanIncludeInClipboardHistory") else {
            return false;
        };
        if !is_format_avail(format.get()) {
            return false;
        }
        let Ok(_open) = clipboard_win::Clipboard::new_attempts(10) else {
            return false;
        };
        let mut value = Vec::new();
        match get_vec(format.get(), &mut value) {
            Ok(_) => value.len() >= 4 && value[..4] == [0, 0, 0, 0],
            Err(_) => false,
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod x11 {
    use std::time::{Duration, Instant};

    use x11rb::{
        COPY_DEPTH_FROM_PARENT, CURRENT_TIME,
        connection::Connection,
        protocol::{
            Event,
            xproto::{Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, Window, WindowClass},
        },
        rust_connection::RustConnection,
    };

    /// longest wait for the application owning the clipboard to answer
    const ANSWER: Duration = Duration::from_millis(500);
    /// most formats looked at
    const MAX_TARGETS: usize = 256;

    /// asks the owner of the clipboard which formats it offers
    pub(super) struct Targets {
        conn: RustConnection,
        window: Window,
        clipboard: Atom,
        targets: Atom,
        property: Atom,
    }

    impl Targets {
        pub(super) fn new() -> Result<Self, String> {
            let (conn, screen) = x11rb::connect(None).map_err(|e| e.to_string())?;
            let root = conn.setup().roots[screen].root;
            let window = conn.generate_id().map_err(|e| e.to_string())?;
            conn.create_window(
                COPY_DEPTH_FROM_PARENT,
                window,
                root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new(),
            )
            .map_err(|e| e.to_string())?;
            let atom = |name: &str| -> Result<Atom, String> {
                Ok(conn
                    .intern_atom(false, name.as_bytes())
                    .map_err(|e| e.to_string())?
                    .reply()
                    .map_err(|e| e.to_string())?
                    .atom)
            };
            let (clipboard, targets, property) = (
                atom("CLIPBOARD")?,
                atom("TARGETS")?,
                atom("RUSTINPUT_TARGETS")?,
            );
            Ok(Self {
                conn,
                window,
                clipboard,
                targets,
                property,
            })
        }

        /// the names of the formats the clipboard offers; none when nobody
        /// owns it
        pub(super) fn names(&mut self) -> Result<Vec<String>, String> {
            let err = |e: &dyn std::fmt::Display| e.to_string();
            let owner = self
                .conn
                .get_selection_owner(self.clipboard)
                .map_err(|e| err(&e))?
                .reply()
                .map_err(|e| err(&e))?
                .owner;
            if owner == x11rb::NONE {
                return Ok(Vec::new());
            }
            self.conn
                .convert_selection(
                    self.window,
                    self.clipboard,
                    self.targets,
                    self.property,
                    CURRENT_TIME,
                )
                .map_err(|e| err(&e))?;
            self.conn.flush().map_err(|e| err(&e))?;
            let started = Instant::now();
            let answered = loop {
                match self.conn.poll_for_event().map_err(|e| err(&e))? {
                    Some(Event::SelectionNotify(e)) if e.requestor == self.window => {
                        break e.property != x11rb::NONE;
                    }
                    Some(_) => {}
                    None if started.elapsed() > ANSWER => return Err("no answer".into()),
                    None => std::thread::sleep(Duration::from_millis(5)),
                }
            };
            if !answered {
                return Ok(Vec::new());
            }
            let reply = self
                .conn
                .get_property(
                    true,
                    self.window,
                    self.property,
                    AtomEnum::ATOM,
                    0,
                    MAX_TARGETS as u32,
                )
                .map_err(|e| err(&e))?
                .reply()
                .map_err(|e| err(&e))?;
            let atoms: Vec<Atom> = reply
                .value32()
                .map(|atoms| atoms.take(MAX_TARGETS).collect())
                .unwrap_or_default();
            // all questions first, then the answers: one round trip
            let cookies: Vec<_> = atoms
                .iter()
                .filter_map(|&atom| self.conn.get_atom_name(atom).ok())
                .collect();
            Ok(cookies
                .into_iter()
                .filter_map(|cookie| cookie.reply().ok())
                .map(|reply| String::from_utf8_lossy(&reply.name).into_owned())
                .collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_of_password_managers_are_secret() {
        assert!(is_secret_format("x-kde-passwordManagerHint"));
        assert!(is_secret_format(
            "ExcludeClipboardContentFromMonitorProcessing"
        ));
        assert!(!is_secret_format("UTF8_STRING"));
        assert!(!is_secret_format("text/plain"));
        assert!(!is_secret_format("CanIncludeInClipboardHistory"));
    }
}
