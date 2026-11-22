//! The radial menu, like the weapon wheel of GTA V: holding Ctrl and the left
//! Alt alone shows a wheel of actions around the pointer, on the screen of the
//! device the pointer is on; pointing at one and letting go of the keys picks
//! it, letting go in the middle or pressing Escape picks nothing.
//!
//! The keys are watched without taking them from the applications (see
//! [`chord`]). While the wheel shows, the pointer stays inside it, so it does
//! not cross to another device, and it goes back to where it was afterwards.
//! Available on X11 and Windows.

// the wheel only shows on X11 and Windows: on macOS it is built, never used
#![cfg_attr(target_os = "macos", allow(dead_code))]

mod chord;
pub(crate) mod draw;
pub(crate) mod fonts;
#[cfg(windows)]
pub(crate) mod windows;
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) mod x11;

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use fonts::Fonts;
use rustinput_i18n::{tr, trf};
use rustinput_ipc::ClientHandle;
use tokio::sync::mpsc::UnboundedSender;

/// what a slice of the wheel does
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Action {
    /// move the pointer to the client, without crossing the edge
    GoTo(ClientHandle),
    /// send the clipboard to the connected devices right away
    SendClipboard,
    /// send the files copied in the file manager ("Copy to RustInput")
    SendCopiedFiles,
    /// keep the pointer on this device, or let it go again
    ToggleLock,
    /// pause RustInput on this device, or let it go on
    TogglePause,
    /// lock the screen of the client, as Win+L there
    LockScreen(ClientHandle),
    /// lock the screens of this device and of the clients
    LockAllScreens,
    ShowTransfers,
    ShowWindow,
}

/// a slice of the wheel
#[derive(Clone, Debug)]
pub(crate) struct Item {
    pub(crate) label: String,
    pub(crate) action: Action,
}

/// what the service tells the wheel, shared with its thread
#[derive(Clone, Default)]
pub(crate) struct Menu {
    items: Arc<Mutex<Vec<Item>>>,
    away: Arc<AtomicBool>,
}

impl Menu {
    /// the slices, starting at the top and going clockwise
    pub(crate) fn set_items(&self, items: Vec<Item>) {
        *self.items.lock().expect("lock") = items;
    }

    /// the pointer is on another device, which shows its own wheel
    pub(crate) fn set_away(&self, away: bool) {
        self.away.store(away, Ordering::Relaxed);
    }

    fn items(&self) -> Vec<Item> {
        self.items.lock().expect("lock").clone()
    }

    fn away(&self) -> bool {
        self.away.load(Ordering::Relaxed)
    }
}

/// The texts of the wheel, in the language of RustInput (`language`, else
/// that of the system), see `rustinput-i18n`.
#[derive(Default)]
pub(crate) struct Texts;

impl Texts {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn go_to(&self, name: &str) -> String {
        trf("Go to {name}", &[("name", &name)])
    }

    pub(crate) fn send_clipboard(&self) -> String {
        tr("Send the clipboard").to_owned()
    }

    pub(crate) fn send_files(&self) -> String {
        tr("Send copied files").to_owned()
    }

    pub(crate) fn lock(&self, locked: bool) -> String {
        match locked {
            true => tr("Free the pointer"),
            false => tr("Keep the pointer here"),
        }
        .to_owned()
    }

    pub(crate) fn pause(&self, paused: bool) -> String {
        match paused {
            true => tr("Resume RustInput"),
            false => tr("Pause RustInput"),
        }
        .to_owned()
    }

    /// locking the screen of the computer `name`
    pub(crate) fn lock_screen(&self, name: &str) -> String {
        trf("Lock {name}", &[("name", &name)])
    }

    pub(crate) fn lock_all_screens(&self) -> String {
        tr("Lock all computers").to_owned()
    }

    pub(crate) fn transfers(&self) -> String {
        tr("File transfers").to_owned()
    }

    pub(crate) fn window(&self) -> String {
        tr("Open RustInput").to_owned()
    }

    pub(crate) fn no_files(&self) -> String {
        tr("No files copied: copy files in the file manager first").to_owned()
    }

    fn hint(&self) -> String {
        tr("Point at an action and let go").to_owned()
    }
}

/// The fonts for the labels in the language of RustInput, those of `cache`
/// while it does not change: loaded again after it changed.
pub(crate) fn fonts_now(cache: &mut Option<Arc<Fonts>>) -> Option<Arc<Fonts>> {
    let language = rustinput_i18n::current();
    if cache.as_ref().is_none_or(|f| f.language != language) {
        *cache = Fonts::load(language).map(Arc::new);
    }
    cache.clone()
}

/// An open wheel: what it shows and where, shared by the platforms.
struct Wheel {
    items: Vec<Item>,
    labels: Vec<String>,
    selected: Option<usize>,
    scale: f32,
    hint: String,
    font: Option<Arc<Fonts>>,
}

impl Wheel {
    fn new(items: Vec<Item>, scale: f32, texts: &Texts, font: Option<Arc<Fonts>>) -> Self {
        let labels = items.iter().map(|i| i.label.clone()).collect();
        Self {
            items,
            labels,
            selected: None,
            scale,
            hint: texts.hint(),
            font,
        }
    }

    fn size(&self) -> u32 {
        draw::size(self.scale, self.items.len())
    }

    /// the pointer is `(dx, dy)` from the middle; whether the slice changed
    fn point(&mut self, dx: f64, dy: f64) -> bool {
        let n = self.items.len();
        let selected = draw::slice_at(n, dx, dy, draw::dead_zone(self.scale, n));
        let changed = selected != self.selected;
        self.selected = selected;
        changed
    }

    /// premultiplied BGRA pixels of the wheel
    fn pixels(&self) -> Vec<u8> {
        let pixmap = draw::render(
            &self.labels,
            self.selected,
            &self.hint,
            self.font.as_deref(),
            self.scale,
        );
        draw::bgra(&pixmap)
    }

    fn chosen(&self) -> Option<Action> {
        self.selected
            .and_then(|i| self.items.get(i))
            .map(|i| i.action.clone())
    }
}

/// Starts watching for the keys of the wheel, on X11 and Windows. Picked
/// actions go to `chosen`.
pub(crate) fn spawn(menu: Menu, chosen: UnboundedSender<Action>) {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let x11 = std::env::var_os("DISPLAY").is_some_and(|d| !d.is_empty())
            && std::env::var_os("WAYLAND_DISPLAY").is_none_or(|d| d.is_empty());
        if !x11 {
            log::info!("radial menu: only available on X11 and Windows");
            return;
        }
        let spawned = std::thread::Builder::new()
            .name("radial-menu".into())
            .spawn(move || {
                if let Err(e) = x11::run(menu, chosen) {
                    log::warn!("radial menu: {e}");
                }
            });
        if let Err(e) = spawned {
            log::warn!("radial menu: {e}");
        }
    }
    #[cfg(windows)]
    {
        let spawned = std::thread::Builder::new()
            .name("radial-menu".into())
            .spawn(move || {
                if let Err(e) = windows::run(menu, chosen) {
                    log::warn!("radial menu: {e}");
                }
            });
        if let Err(e) = spawned {
            log::warn!("radial menu: {e}");
        }
    }
    #[cfg(target_os = "macos")]
    {
        let _ = (menu, chosen);
        log::info!("radial menu: only available on X11 and Windows");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wheel_picks_what_the_pointer_points_at() {
        let items = vec![
            Item {
                label: "Go to B".into(),
                action: Action::GoTo(0),
            },
            Item {
                label: "Send".into(),
                action: Action::SendClipboard,
            },
        ];
        let texts = Texts;
        let mut wheel = Wheel::new(items, 1.0, &texts, None);
        assert!(!wheel.point(1.0, 1.0), "the middle picks nothing");
        assert_eq!(wheel.chosen(), None);
        assert!(wheel.point(0.0, -150.0));
        assert_eq!(wheel.chosen(), Some(Action::GoTo(0)));
        assert!(!wheel.point(5.0, -160.0), "the same slice");
        assert!(wheel.point(0.0, 150.0));
        assert_eq!(wheel.chosen(), Some(Action::SendClipboard));
        assert_eq!(
            wheel.pixels().len(),
            (wheel.size() * wheel.size() * 4) as usize
        );
    }

    #[test]
    fn texts_in_english() {
        let texts = Texts;
        assert_eq!(texts.go_to("xeon"), "Go to xeon");
        assert_eq!(texts.lock(true), "Free the pointer");
        assert_eq!(texts.lock_screen("laptop"), "Lock laptop");
    }
}
