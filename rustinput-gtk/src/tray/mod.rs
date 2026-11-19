//! System tray icon (Linux and Windows; macOS has its menu bar item).
//!
//! With a tray icon, closing the window only hides it: RustInput keeps
//! sharing mouse, keyboard and clipboard, and the icon brings the window back
//! or quits. Without a system tray closing the window quits, as before.

mod action;
mod icon;
#[cfg(all(unix, not(target_os = "macos")))]
mod linux;
#[cfg(windows)]
mod windows;

#[cfg(all(unix, not(target_os = "macos")))]
use linux as platform;
#[cfg(windows)]
use windows as platform;

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use adw::Application;
use gtk::{glib, glib::clone, prelude::*};

pub(crate) use action::TrayAction;

use crate::window::Window;

/// Puts RustInput into the system tray and makes closing `window` hide it.
///
/// Returns `false` if there is no system tray, in which case nothing changes.
pub(crate) fn setup(app: &Application, window: &Window) -> bool {
    let (actions_tx, actions) = async_channel::unbounded();
    // shown in the tooltip and at the top of the menu
    let label = format!(
        "RustInput {}",
        crate::APP_VERSION.get().copied().unwrap_or_default()
    );
    let Some(handle) = platform::spawn(actions_tx, label) else {
        return false;
    };
    log::info!("tray icon shown, closing the window keeps RustInput running");

    // whether the icon is on screen right now
    let available = Rc::new(Cell::new(true));

    window.connect_close_request(clone!(
        #[strong]
        available,
        #[weak]
        app,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |window| {
            if available.get() {
                window.set_visible(false);
                glib::Propagation::Stop
            } else {
                // there would be no way to get the window back
                app.quit();
                glib::Propagation::Proceed
            }
        }
    ));

    // a hidden window must not end the application, and the icon goes away
    // together with it
    let resources = RefCell::new(Some((handle, app.hold())));
    app.connect_shutdown(move |_| {
        resources.borrow_mut().take();
    });

    glib::spawn_future_local(clone!(
        #[weak]
        app,
        #[weak]
        window,
        async move {
            while let Ok(action) = actions.recv().await {
                match action {
                    TrayAction::Show => window.present(),
                    TrayAction::Quit => app.quit(),
                    TrayAction::Available(on_screen) => {
                        available.set(on_screen);
                        if !on_screen && !window.is_visible() {
                            // nothing would be left on screen otherwise
                            window.present();
                        }
                    }
                }
            }
        }
    ));
    true
}
