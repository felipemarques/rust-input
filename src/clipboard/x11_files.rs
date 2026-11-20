//! Offering files on the X11 clipboard.
//!
//! arboard only offers files as `text/uri-list`, which Nemo, Nautilus, Caja
//! and Thunar do not paste: they want `x-special/gnome-copied-files`. This
//! owns the clipboard with all the usual file targets until another
//! application takes it over. The files are offered as cut: pasting moves
//! them out of the temporary folder they were received into.

use std::{
    path::{Path, PathBuf},
    thread,
};

use x11rb::{
    COPY_DEPTH_FROM_PARENT, CURRENT_TIME, NONE,
    connection::{Connection, RequestConnection},
    protocol::{
        Event,
        xproto::{
            Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
            SELECTION_NOTIFY_EVENT, SelectionNotifyEvent, SelectionRequestEvent, WindowClass,
        },
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

/// whether files have to be offered through [`serve`]
pub(crate) fn is_x11() -> bool {
    let set = |var| std::env::var_os(var).is_some_and(|v| !v.is_empty());
    set("DISPLAY") && !set("WAYLAND_DISPLAY")
}

/// `file://` URI of `path`, with everything but unreserved characters and `/`
/// percent encoded
fn uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            uri.push(byte as char);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

/// what the clipboard offers for `paths`, for every target
struct Contents {
    /// `cut`: pasting moves the files out of the temporary folder, so nothing
    /// is left behind there
    gnome: Vec<u8>,
    uri_list: Vec<u8>,
    text: Vec<u8>,
}

impl Contents {
    fn new(paths: &[PathBuf]) -> Self {
        let uris: Vec<String> = paths.iter().map(|p| uri(p)).collect();
        let mut gnome = String::from("cut");
        for uri in &uris {
            gnome.push('\n');
            gnome.push_str(uri);
        }
        let uri_list: String = uris.iter().map(|u| format!("{u}\r\n")).collect();
        let text: Vec<String> = paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        Self {
            gnome: gnome.into_bytes(),
            uri_list: uri_list.into_bytes(),
            text: text.join("\n").into_bytes(),
        }
    }
}

struct Atoms {
    clipboard: Atom,
    targets: Atom,
    timestamp: Atom,
    gnome: Atom,
    kde_cut: Atom,
    uri_list: Atom,
    utf8: Atom,
    plain_utf8: Atom,
}

fn atom(conn: &RustConnection, name: &str) -> Result<Atom, String> {
    Ok(conn
        .intern_atom(false, name.as_bytes())
        .map_err(|e| e.to_string())?
        .reply()
        .map_err(|e| e.to_string())?
        .atom)
}

/// Puts `paths` on the clipboard, until another application takes it over.
pub(crate) fn serve(paths: Vec<PathBuf>) -> Result<(), String> {
    let (conn, screen) = x11rb::connect(None).map_err(|e| e.to_string())?;
    let root = conn.setup().roots[screen].root;
    let atoms = Atoms {
        clipboard: atom(&conn, "CLIPBOARD")?,
        targets: atom(&conn, "TARGETS")?,
        timestamp: atom(&conn, "TIMESTAMP")?,
        gnome: atom(&conn, "x-special/gnome-copied-files")?,
        kde_cut: atom(&conn, "application/x-kde-cutselection")?,
        uri_list: atom(&conn, "text/uri-list")?,
        utf8: atom(&conn, "UTF8_STRING")?,
        plain_utf8: atom(&conn, "text/plain;charset=utf-8")?,
    };
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
        WindowClass::INPUT_ONLY,
        x11rb::COPY_FROM_PARENT,
        &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )
    .map_err(|e| e.to_string())?;
    conn.set_selection_owner(window, atoms.clipboard, CURRENT_TIME)
        .map_err(|e| e.to_string())?;
    let owner = conn
        .get_selection_owner(atoms.clipboard)
        .map_err(|e| e.to_string())?
        .reply()
        .map_err(|e| e.to_string())?
        .owner;
    if owner != window {
        return Err("could not take over the clipboard".into());
    }
    let contents = Contents::new(&paths);
    thread::Builder::new()
        .name("clipboard-files".into())
        .spawn(move || {
            if let Err(e) = answer_requests(&conn, window, &atoms, &contents) {
                log::warn!("offering the received files on the clipboard: {e}");
            }
            let _ = conn.destroy_window(window);
            let _ = conn.flush();
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn answer_requests(
    conn: &RustConnection,
    _window: u32,
    atoms: &Atoms,
    contents: &Contents,
) -> Result<(), String> {
    // the largest property that fits into a single request
    let max_length = conn.maximum_request_bytes().saturating_sub(64);
    loop {
        match conn.wait_for_event().map_err(|e| e.to_string())? {
            Event::SelectionClear(e) if e.selection == atoms.clipboard => {
                // another application owns the clipboard now
                return Ok(());
            }
            Event::SelectionRequest(request) => {
                let property = answer(conn, atoms, contents, &request, max_length)?;
                let notify = SelectionNotifyEvent {
                    response_type: SELECTION_NOTIFY_EVENT,
                    sequence: 0,
                    time: request.time,
                    requestor: request.requestor,
                    selection: request.selection,
                    target: request.target,
                    property,
                };
                conn.send_event(false, request.requestor, EventMask::NO_EVENT, notify)
                    .map_err(|e| e.to_string())?;
                conn.flush().map_err(|e| e.to_string())?;
            }
            _ => {}
        }
    }
}

/// stores the requested target on the requestor, returns the property used
/// or NONE if the target is not available
fn answer(
    conn: &RustConnection,
    atoms: &Atoms,
    contents: &Contents,
    request: &SelectionRequestEvent,
    max_length: usize,
) -> Result<Atom, String> {
    // obsolete clients leave the property empty
    let property = if request.property == NONE {
        request.target
    } else {
        request.property
    };
    let target = request.target;
    if target == atoms.targets {
        let targets = [
            atoms.targets,
            atoms.timestamp,
            atoms.gnome,
            atoms.kde_cut,
            atoms.uri_list,
            atoms.utf8,
            atoms.plain_utf8,
        ];
        conn.change_property32(
            PropMode::REPLACE,
            request.requestor,
            property,
            AtomEnum::ATOM,
            &targets,
        )
        .map_err(|e| e.to_string())?;
        return Ok(property);
    }
    if target == atoms.timestamp {
        conn.change_property32(
            PropMode::REPLACE,
            request.requestor,
            property,
            AtomEnum::INTEGER,
            &[CURRENT_TIME],
        )
        .map_err(|e| e.to_string())?;
        return Ok(property);
    }
    let data = if target == atoms.gnome {
        &contents.gnome
    } else if target == atoms.kde_cut {
        // Dolphin moves the files too
        &b"1"[..]
    } else if target == atoms.uri_list {
        &contents.uri_list
    } else if target == atoms.utf8 || target == atoms.plain_utf8 {
        &contents.text
    } else {
        return Ok(NONE);
    };
    if data.len() > max_length {
        log::warn!("the list of received files is too long for the clipboard");
        return Ok(NONE);
    }
    conn.change_property8(PropMode::REPLACE, request.requestor, property, target, data)
        .map_err(|e| e.to_string())?;
    Ok(property)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_are_percent_encoded() {
        assert_eq!(
            uri(Path::new("/home/ana/Área de trabalho/a b#1.txt")),
            "file:///home/ana/%C3%81rea%20de%20trabalho/a%20b%231.txt"
        );
    }

    #[test]
    fn contents_for_every_target() {
        let contents = Contents::new(&[PathBuf::from("/tmp/a.txt"), PathBuf::from("/tmp/b c")]);
        assert_eq!(
            String::from_utf8(contents.gnome).unwrap(),
            "cut\nfile:///tmp/a.txt\nfile:///tmp/b%20c"
        );
        assert_eq!(
            String::from_utf8(contents.uri_list).unwrap(),
            "file:///tmp/a.txt\r\nfile:///tmp/b%20c\r\n"
        );
        assert_eq!(
            String::from_utf8(contents.text).unwrap(),
            "/tmp/a.txt\n/tmp/b c"
        );
    }
}
