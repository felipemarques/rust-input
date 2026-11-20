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
mod draw;
#[cfg(windows)]
mod windows;
#[cfg(all(unix, not(target_os = "macos")))]
mod x11;

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

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
    /// turn sending copied files along with the pointer on or off
    ToggleSendCopied,
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

/// The texts of the wheel, in Portuguese when the system speaks it and in
/// English otherwise.
pub(crate) struct Texts {
    portuguese: bool,
}

impl Texts {
    pub(crate) fn new() -> Self {
        Self {
            portuguese: system_language().starts_with("pt"),
        }
    }

    fn pick(&self, en: &str, pt: &str) -> String {
        if self.portuguese { pt } else { en }.to_owned()
    }

    pub(crate) fn go_to(&self, name: &str) -> String {
        if self.portuguese {
            format!("Ir para {name}")
        } else {
            format!("Go to {name}")
        }
    }

    pub(crate) fn send_clipboard(&self) -> String {
        self.pick("Send the clipboard", "Enviar o clipboard")
    }

    pub(crate) fn send_files(&self) -> String {
        self.pick("Send copied files", "Enviar arquivos copiados")
    }

    pub(crate) fn lock(&self, locked: bool) -> String {
        match locked {
            true => self.pick("Free the pointer", "Soltar o cursor"),
            false => self.pick("Keep the pointer here", "Prender o cursor aqui"),
        }
    }

    pub(crate) fn send_copied(&self, on: bool) -> String {
        match on {
            true => self.pick("Turn auto-send off", "Desligar envio automático"),
            false => self.pick("Turn auto-send on", "Ligar envio automático"),
        }
    }

    pub(crate) fn transfers(&self) -> String {
        self.pick("File transfers", "Transferências")
    }

    pub(crate) fn window(&self) -> String {
        self.pick("Open RustInput", "Abrir o RustInput")
    }

    pub(crate) fn no_files(&self) -> String {
        self.pick(
            "No files copied: copy files in the file manager first",
            "Nenhum arquivo copiado: copie arquivos no gerenciador primeiro",
        )
    }

    fn hint(&self) -> String {
        self.pick("Point at an action and let go", "Aponte e solte as teclas")
    }
}

/// the language of the user interface, "pt_BR", "en", ...
fn system_language() -> String {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Globalization::GetUserDefaultUILanguage;
        // the primary language id of Portuguese
        if unsafe { GetUserDefaultUILanguage() } & 0x3ff == 0x16 {
            return "pt".into();
        }
        "en".into()
    }
    #[cfg(not(windows))]
    {
        ["LC_ALL", "LC_MESSAGES", "LANG"]
            .iter()
            .filter_map(|v| std::env::var(v).ok())
            .find(|v| !v.is_empty())
            .unwrap_or_default()
    }
}

/// a font of the system for the labels, `None` if none was found
fn load_font() -> Option<fontdue::Font> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    #[cfg(windows)]
    {
        let fonts = std::env::var_os("WINDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
            .join("Fonts");
        for name in ["segoeuib.ttf", "segoeui.ttf", "arialbd.ttf", "arial.ttf"] {
            candidates.push(fonts.join(name));
        }
    }
    #[cfg(not(windows))]
    {
        // the font the desktop uses
        if let Ok(out) = std::process::Command::new("fc-match")
            .args(["-f", "%{file}", "sans:bold"])
            .output()
        {
            if let Ok(path) = String::from_utf8(out.stdout) {
                candidates.push(PathBuf::from(path));
            }
        }
        for path in [
            "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
            "/usr/share/fonts/truetype/ubuntu/Ubuntu-B.ttf",
            "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
            "/usr/share/fonts/dejavu/DejaVuSans-Bold.ttf",
            "/usr/share/fonts/truetype/noto/NotoSans-Bold.ttf",
            "/System/Library/Fonts/Helvetica.ttc",
        ] {
            candidates.push(PathBuf::from(path));
        }
    }
    for path in candidates {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        match fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()) {
            Ok(font) => {
                log::debug!("radial menu: font {path:?}");
                return Some(font);
            }
            Err(e) => log::debug!("radial menu: font {path:?}: {e}"),
        }
    }
    log::warn!("radial menu: no font found, the actions show without names");
    None
}

/// An open wheel: what it shows and where, shared by the platforms.
struct Wheel {
    items: Vec<Item>,
    labels: Vec<String>,
    selected: Option<usize>,
    scale: f32,
    hint: String,
    font: Option<Arc<fontdue::Font>>,
}

impl Wheel {
    fn new(items: Vec<Item>, scale: f32, texts: &Texts, font: Option<Arc<fontdue::Font>>) -> Self {
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
        draw::size(self.scale)
    }

    /// the pointer is `(dx, dy)` from the middle; whether the slice changed
    fn point(&mut self, dx: f64, dy: f64) -> bool {
        let selected = draw::slice_at(self.items.len(), dx, dy, draw::dead_zone(self.scale));
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
        let texts = Texts { portuguese: true };
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
    fn texts_in_both_languages() {
        let pt = Texts { portuguese: true };
        let en = Texts { portuguese: false };
        assert_eq!(pt.go_to("xeon"), "Ir para xeon");
        assert_eq!(en.go_to("xeon"), "Go to xeon");
        assert_eq!(pt.lock(false), "Prender o cursor aqui");
        assert_eq!(en.lock(true), "Free the pointer");
    }
}
