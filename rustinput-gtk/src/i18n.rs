//! The language of the windows and menus: the texts of the code through
//! [`tr`] and [`trf`], the windows (`.ui`) from the bundle of their
//! language, built with the translations put in (see `build.rs`).

use std::sync::{
    OnceLock,
    atomic::{AtomicBool, Ordering},
};

use gtk::{gio, glib};
pub(crate) use rustinput_i18n::{LANGUAGES, Language, current, tr, trf, trn};

include!(concat!(env!("OUT_DIR"), "/ui_resources.rs"));

/// the language configured when RustInput started, `None` for the one of
/// the system
static CONFIGURED: OnceLock<Option<String>> = OnceLock::new();

/// the language configured when RustInput started ("pt-BR"), `None` for
/// the one of the system
pub(crate) fn configured() -> Option<&'static str> {
    CONFIGURED.get().and_then(|c| c.as_deref())
}

/// Picks the language: the one configured, else the one of the system.
pub(crate) fn start(configured: Option<&str>) {
    let _ = CONFIGURED.set(configured.map(str::to_owned));
    let language = rustinput_i18n::resolve(configured);
    rustinput_i18n::set(language);
    log::info!("language: {} ({})", language.name, language.code);
    // GTK's own texts (the menu of a text field) in the same language, as
    // far as the system has them
    #[cfg(all(unix, not(target_os = "macos")))]
    if configured.and_then(Language::from_code).is_some() && !language.file.is_empty() {
        let primary = language.file.split('_').next().unwrap_or(language.file);
        std::env::set_var("LANGUAGE", format!("{}:{primary}", language.file));
    }
}

/// registers the windows in the current language, before any is made
pub(crate) fn register_windows() {
    let code = current().code;
    let data = UI_RESOURCES
        .iter()
        .find(|(c, _)| *c == code)
        .unwrap_or(&UI_RESOURCES[0])
        .1;
    let resource = gio::Resource::from_data(&glib::Bytes::from_static(data))
        .expect("the windows are a valid resource bundle");
    gio::resources_register(&resource);
}

/// the direction of the texts in the current language
pub(crate) fn direction() -> gtk::TextDirection {
    if current().rtl {
        gtk::TextDirection::Rtl
    } else {
        gtk::TextDirection::Ltr
    }
}

/// the language picked in the preferences takes over when RustInput starts
/// again, which it does on its own when asked to
static RESTART: AtomicBool = AtomicBool::new(false);

/// whether RustInput is to start again when the window quits, in the
/// language picked
pub fn restart_requested() -> bool {
    RESTART.load(Ordering::Relaxed)
}

pub(crate) fn request_restart() {
    RESTART.store(true, Ordering::Relaxed);
}

/// the language at `index` of the list of the preferences, which starts
/// with "As the system"; `None` for that one
pub(crate) fn language_at(index: u32) -> Option<Language> {
    match index {
        0 => None,
        i => LANGUAGES.get(i as usize - 1).copied(),
    }
}

/// where `configured` is in the list of the preferences
pub(crate) fn index_of(configured: Option<&str>) -> u32 {
    configured
        .and_then(Language::from_code)
        .map(|l| l.index() as u32 + 1)
        .unwrap_or(0)
}
