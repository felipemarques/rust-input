//! The keyboard shortcuts to move between the computers, in Preferences
//! (0.42.0): a switch, and what the shortcuts are.

use std::cell::Cell;

use adw::prelude::*;
use gtk::{glib, glib::clone, subclass::prelude::ObjectSubclassIsExt};
use rustinput_i18n::tr;
use rustinput_ipc::FrontendRequest;

use super::Window;

/// what the window keeps of the group
pub(crate) struct Shortcuts {
    switch: gtk::Switch,
    /// the switch is being set to what the service has
    updating: Cell<bool>,
}

impl Window {
    /// the group goes before the one of the file transfers
    pub(super) fn setup_shortcuts(&self) {
        let group = adw::PreferencesGroup::builder()
            .title(tr("Keyboard shortcuts"))
            .description(tr(
                "They fire when the keys are let go, only if nothing else was pressed: \
                AltGr + Q still types",
            ))
            .build();
        let row = adw::ActionRow::builder()
            .title(tr("Move between the computers with the keyboard"))
            .build();
        let switch = gtk::Switch::builder().valign(gtk::Align::Center).build();
        row.add_suffix(&switch);
        row.set_activatable_widget(Some(&switch));
        group.add(&row);
        for (keys, what) in [
            (
                "AltGr + ⇧ + 1 … 9",
                tr("To that computer, in the order of the list"),
            ),
            ("AltGr + ⇧ + 0", tr("Back to this computer")),
            (
                "AltGr + ⇧",
                tr("To the other computer and back, or the next one"),
            ),
            ("AltGr + → / ←", tr("The next / the previous computer")),
            (
                "⇧ ⇧",
                tr("The right Shift twice quickly: the next computer"),
            ),
            (
                "⇧ 3 s",
                tr("The right Shift held for 3 seconds: back to this computer, always"),
            ),
        ] {
            let line = adw::ActionRow::builder().title(what).build();
            let label = gtk::Label::new(Some(keys));
            label.add_css_class("dim-label");
            label.add_css_class("monospace");
            line.add_suffix(&label);
            group.add(&line);
        }
        let files = self.imp().files_group.get();
        if let Some(parent) = files.parent().and_then(|p| p.downcast::<gtk::Box>().ok()) {
            parent.insert_child_after(&group, files.prev_sibling().as_ref());
        }
        switch.connect_active_notify(clone!(
            #[weak(rename_to = window)]
            self,
            move |switch| {
                let updating = window
                    .imp()
                    .shortcuts
                    .borrow()
                    .as_ref()
                    .is_some_and(|s| s.updating.get());
                if !updating {
                    window.request(FrontendRequest::SetHotkeys(switch.is_active()));
                }
            }
        ));
        self.imp().shortcuts.replace(Some(Shortcuts {
            switch,
            updating: Cell::new(false),
        }));
    }

    /// whether the shortcuts are on, as the service has it
    pub(crate) fn set_hotkeys(&self, on: bool) {
        if let Some(s) = self.imp().shortcuts.borrow().as_ref() {
            s.updating.set(true);
            s.switch.set_active(on);
            s.updating.set(false);
        }
    }
}
