mod imp;

use glib::Object;
use gtk::{gio, glib, prelude::GtkWindowExt, subclass::prelude::ObjectSubclassIsExt};

glib::wrapper! {
    pub struct AuthorizationWindow(ObjectSubclass<imp::AuthorizationWindow>)
    @extends adw::Window, gtk::Window, gtk::Widget,
    @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible, gtk::Buildable,
                gtk::ConstraintTarget, gtk::Native, gtk::Root, gtk::ShortcutManager;
}

impl AuthorizationWindow {
    pub(crate) fn new(fingerprint: &str, code: Option<&str>) -> Self {
        let window: Self = Object::builder().build();
        window.imp().set_fingerprint(fingerprint);
        window.imp().set_code(code);
        window
    }

    /// Asks about the certificate the computer `name` answered with, which
    /// is not authorized here: no input goes to it until it is.
    pub(crate) fn for_server(name: &str, fingerprint: &str) -> Self {
        let window = Self::new(fingerprint, None);
        window.set_title(Some(crate::i18n::tr("Certificate not authorized")));
        window.imp().intro.set_text(&crate::i18n::trf(
            "{name} answered with a certificate this computer has not authorized, so the mouse \
             and keyboard do not go to it. Authorize it if this fingerprint is the one RustInput \
             shows on {name}, or pair the two with “Enter code…” next to {name}.",
            &[("name", &name)],
        ));
        window
    }

    /// the fingerprint the window asks about
    pub(crate) fn fingerprint(&self) -> String {
        self.imp().fingerprint.text().trim().to_owned()
    }
}
