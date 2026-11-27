//! A notification of the system when a new version of RustInput is out,
//! once per version: the service keeps the version told about
//! (`update_notified` in `config.toml`), so starting again does not tell it
//! again. The window in front already shows the new version, so then there
//! is no notification.
//!
//! On Linux and macOS it goes through GApplication: the notification service
//! of the desktop (`org.freedesktop.Notifications`, the portal in a sandbox)
//! or the notification center of macOS. Clicking it shows the window, and
//! its button gets the update. On Windows it is a balloon of the tray icon,
//! which Windows 10 and 11 show as a notification; clicking it shows the
//! window, which offers the update.

use rustinput_ipc::UpdateState;

use crate::i18n::trf;

/// the application action that shows the window
pub(crate) const SHOW_WINDOW: &str = "show-window";
/// the application action that gets the new version
pub(crate) const INSTALL_UPDATE: &str = "install-update";

/// The version to tell the user about now, if any: newer than this one,
/// looked for because the user wants it, and not told about yet (`told` is
/// the one told about since RustInput started).
pub(crate) fn to_tell(state: &UpdateState, told: Option<&str>) -> Option<String> {
    let latest = state.latest.as_deref()?;
    let new = state.check == Some(true) && state.available && !state.checking;
    let known = state.notified.as_deref() == Some(latest) || told == Some(latest);
    (new && !known).then(|| latest.to_owned())
}

/// "RustInput 0.32.0 is out": the title of the notification, and a line of
/// the tooltip of the tray icon
pub(crate) fn title(version: &str) -> String {
    trf("RustInput {version} is out", &[("version", &version)])
}

/// the text of the notification about the new version
pub(crate) fn text() -> String {
    let current = crate::APP_VERSION.get().copied().unwrap_or_default();
    trf("This computer has {current}", &[("current", &current)])
}

/// shows the notification about `version`
#[cfg(not(windows))]
pub(crate) fn send(
    app: &impl gtk::prelude::IsA<gtk::gio::Application>,
    version: &str,
    installable: bool,
) {
    use gtk::{gio, prelude::*};
    let title = title(version);
    let notification = gio::Notification::new(&title);
    notification.set_body(Some(&text()));
    notification.set_icon(&gio::ThemedIcon::new("software-update-available"));
    notification.set_default_action(&format!("app.{SHOW_WINDOW}"));
    notification.add_button(
        crate::window::install_label(installable),
        &format!("app.{INSTALL_UPDATE}"),
    );
    // a newer notification about a newer version takes its place
    app.send_notification(Some("new-version"), &notification);
    log::info!("notification shown: {title}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(latest: &str, notified: Option<&str>) -> UpdateState {
        UpdateState {
            check: Some(true),
            latest: Some(latest.into()),
            available: true,
            notified: notified.map(Into::into),
            ..Default::default()
        }
    }

    #[test]
    fn a_new_version_is_told_once() {
        assert_eq!(
            to_tell(&state("0.32.0", None), None).as_deref(),
            Some("0.32.0")
        );
        // told before RustInput started again, or since it started
        assert_eq!(to_tell(&state("0.32.0", Some("0.32.0")), None), None);
        assert_eq!(to_tell(&state("0.32.0", None), Some("0.32.0")), None);
        // but a newer one is told about again
        assert_eq!(
            to_tell(&state("0.33.0", Some("0.32.0")), Some("0.32.0")).as_deref(),
            Some("0.33.0")
        );
    }

    #[test]
    fn nothing_to_tell_without_a_new_version_looked_for() {
        let none = |state: UpdateState| assert_eq!(to_tell(&state, None), None);
        none(UpdateState::default());
        none(UpdateState {
            available: false,
            ..state("0.31.0", None)
        });
        none(UpdateState {
            check: Some(false),
            ..state("0.32.0", None)
        });
        none(UpdateState {
            checking: true,
            ..state("0.32.0", None)
        });
    }
}
