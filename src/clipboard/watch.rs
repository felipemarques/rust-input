//! Telling when something new is copied, for the history of the clipboard
//! (0.35.0).
//!
//! - X11: XFixes reports every new owner of the CLIPBOARD selection, which
//!   every copy sets, also when the same application copies again;
//! - Windows: the sequence number of the clipboard, raised by every change,
//!   looked at a few times a second;
//! - macOS: the change count of the general pasteboard, the same way;
//! - Wayland: not yet.

use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;

/// how often the number of the clipboard is looked at (Windows, macOS)
#[cfg_attr(all(unix, not(target_os = "macos")), allow(dead_code))]
const POLL: Duration = Duration::from_millis(400);

/// Starts watching the clipboard, on a thread of its own: a `()` goes to
/// `changed` whenever something is copied. False where it can not be done.
pub(super) fn spawn(changed: UnboundedSender<()>) -> bool {
    let started = start(changed);
    if let Err(e) = &started {
        log::info!("no history of the clipboard: {e}");
    }
    started.is_ok()
}

#[cfg(all(unix, not(target_os = "macos")))]
fn start(changed: UnboundedSender<()>) -> Result<(), String> {
    if !super::x11_files::is_x11() {
        return Err("copies can not be told on Wayland yet".into());
    }
    x11::start(changed)
}

#[cfg(windows)]
fn start(changed: UnboundedSender<()>) -> Result<(), String> {
    poll(changed, || {
        clipboard_win::raw::seq_num().map_or(0, |n| n.get() as i64)
    })
}

#[cfg(target_os = "macos")]
fn start(changed: UnboundedSender<()>) -> Result<(), String> {
    if macos::change_count().is_none() {
        return Err("the pasteboard can not be read".into());
    }
    poll(changed, || macos::change_count().unwrap_or(0) as i64)
}

/// looks at `number` every [`POLL`] and tells when it changes
#[cfg(any(windows, target_os = "macos"))]
fn poll(
    changed: UnboundedSender<()>,
    number: impl Fn() -> i64 + Send + 'static,
) -> Result<(), String> {
    std::thread::Builder::new()
        .name("clipboard-watch".into())
        .spawn(move || {
            let mut last = number();
            loop {
                std::thread::sleep(POLL);
                let now = number();
                if now != last {
                    last = now;
                    if changed.send(()).is_err() {
                        break;
                    }
                }
            }
        })
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(all(unix, not(target_os = "macos")))]
mod x11 {
    use tokio::sync::mpsc::UnboundedSender;
    use x11rb::{
        COPY_DEPTH_FROM_PARENT,
        connection::{Connection, RequestConnection},
        protocol::{
            Event,
            xfixes::{self, ConnectionExt as _, SelectionEventMask},
            xproto::{ConnectionExt as _, CreateWindowAux, WindowClass},
        },
    };

    pub(super) fn start(changed: UnboundedSender<()>) -> Result<(), String> {
        let err = |e: &dyn std::fmt::Display| e.to_string();
        let (conn, screen) = x11rb::connect(None).map_err(|e| err(&e))?;
        if conn
            .extension_information(xfixes::X11_EXTENSION_NAME)
            .map_err(|e| err(&e))?
            .is_none()
        {
            return Err("the X server has no XFixes".into());
        }
        conn.xfixes_query_version(5, 0)
            .map_err(|e| err(&e))?
            .reply()
            .map_err(|e| err(&e))?;
        let root = conn.setup().roots[screen].root;
        let window = conn.generate_id().map_err(|e| err(&e))?;
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
        .map_err(|e| err(&e))?;
        let clipboard = conn
            .intern_atom(false, b"CLIPBOARD")
            .map_err(|e| err(&e))?
            .reply()
            .map_err(|e| err(&e))?
            .atom;
        conn.xfixes_select_selection_input(
            window,
            clipboard,
            SelectionEventMask::SET_SELECTION_OWNER,
        )
        .map_err(|e| err(&e))?;
        conn.flush().map_err(|e| err(&e))?;
        std::thread::Builder::new()
            .name("clipboard-watch".into())
            .spawn(move || {
                loop {
                    match conn.wait_for_event() {
                        Ok(Event::XfixesSelectionNotify(_)) => {
                            if changed.send(()).is_err() {
                                break;
                            }
                        }
                        Ok(_) => {}
                        Err(e) => {
                            log::warn!("no longer watching the clipboard: {e}");
                            break;
                        }
                    }
                }
            })
            .map(|_| ())
            .map_err(|e| err(&e))
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::{c_char, c_void};

    type Id = *mut c_void;
    type Sel = *const c_void;

    // NSPasteboard is in AppKit
    #[link(name = "AppKit", kind = "framework")]
    extern "C" {}

    #[link(name = "objc")]
    extern "C" {
        fn objc_getClass(name: *const c_char) -> Id;
        fn sel_registerName(name: *const c_char) -> Sel;
        fn objc_msgSend();
        fn objc_autoreleasePoolPush() -> *mut c_void;
        fn objc_autoreleasePoolPop(pool: *mut c_void);
    }

    /// `[[NSPasteboard generalPasteboard] changeCount]`
    pub(super) fn change_count() -> Option<isize> {
        // SAFETY: objc_msgSend is called with the types of the two methods,
        // on a class and an object the runtime returned, within a pool of
        // its own.
        unsafe {
            let pool = objc_autoreleasePoolPush();
            let send_id = std::mem::transmute::<
                unsafe extern "C" fn(),
                unsafe extern "C" fn(Id, Sel) -> Id,
            >(objc_msgSend);
            let send_count = std::mem::transmute::<
                unsafe extern "C" fn(),
                unsafe extern "C" fn(Id, Sel) -> isize,
            >(objc_msgSend);
            let class = objc_getClass(c"NSPasteboard".as_ptr());
            let count = if class.is_null() {
                None
            } else {
                let board = send_id(class, sel_registerName(c"generalPasteboard".as_ptr()));
                (!board.is_null())
                    .then(|| send_count(board, sel_registerName(c"changeCount".as_ptr())))
            };
            objc_autoreleasePoolPop(pool);
            count
        }
    }
}
