//! The computers running RustInput found on the network, below the
//! connections in the Computers tab (0.40.0): adding one makes it a
//! connection like one added by hand; authorizing it is up to the code.

use std::cell::Cell;

use adw::prelude::*;
use gtk::{glib, glib::clone, subclass::prelude::ObjectSubclassIsExt};
use rustinput_i18n::{tr, trf};
use rustinput_ipc::{Discovered, FrontendRequest};

use super::Window;

/// what the window keeps of the group
pub(crate) struct Network {
    switch: gtk::Switch,
    list: gtk::ListBox,
    placeholder: adw::ActionRow,
    /// the switch is being set to what the service has
    updating: Cell<bool>,
    /// the service was asked to look, while the tab shows
    looking: Cell<bool>,
}

/// what a system is called
fn system(os: &str) -> String {
    match os {
        "linux" => "Linux".into(),
        "windows" => "Windows".into(),
        "macos" => "macOS".into(),
        other => other.into(),
    }
}

impl Window {
    pub(super) fn setup_network(&self) {
        let group = adw::PreferencesGroup::builder()
            .title(tr("On this network"))
            .description(tr(
                "Other computers running RustInput nearby. Adding one does not authorize it: \
                the code it shows does.",
            ))
            .build();
        let switch_row = adw::ActionRow::builder()
            .title(tr("Show this computer to the others"))
            .subtitle(tr(
                "Other computers with RustInput see its name and address, and this one sees theirs",
            ))
            .build();
        let switch = gtk::Switch::builder().valign(gtk::Align::Center).build();
        switch_row.add_suffix(&switch);
        switch_row.set_activatable_widget(Some(&switch));
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .margin_top(12)
            .build();
        let placeholder = adw::ActionRow::builder()
            .title(tr("No other computer found yet"))
            .subtitle(tr(
                "RustInput has to run on it, on the same network, with this switched on too",
            ))
            .build();
        list.set_placeholder(Some(&placeholder));
        let switches = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        switches.append(&switch_row);
        group.add(&switches);
        group.add(&list);
        self.imp().computers_box.append(&group);
        switch.connect_active_notify(clone!(
            #[weak(rename_to = window)]
            self,
            move |switch| {
                let updating = window
                    .imp()
                    .network
                    .borrow()
                    .as_ref()
                    .is_some_and(|n| n.updating.get());
                if !updating {
                    window.request(FrontendRequest::SetDiscovery(switch.is_active()));
                }
            }
        ));
        self.imp().network.replace(Some(Network {
            switch,
            list,
            placeholder,
            updating: Cell::new(false),
            looking: Cell::new(false),
        }));
        let imp = self.imp();
        imp.stack.connect_visible_child_name_notify(clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.update_looking()
        ));
        self.connect_visible_notify(|window| window.update_looking());
    }

    /// asks the service to look while the Computers tab shows
    fn update_looking(&self) {
        let imp = self.imp();
        let shown =
            self.is_visible() && imp.stack.visible_child_name().as_deref() == Some("computers");
        let changed = imp
            .network
            .borrow()
            .as_ref()
            .is_some_and(|n| n.looking.replace(shown) != shown);
        if changed {
            self.request(FrontendRequest::Discover(shown));
        }
    }

    /// whether this computer is shown to the others, as the service has it
    pub(crate) fn set_discovery(&self, on: bool) {
        if let Some(n) = self.imp().network.borrow().as_ref() {
            n.updating.set(true);
            n.switch.set_active(on);
            n.updating.set(false);
            n.list.set_visible(on);
        }
        // the service looks again once it is on
        if on {
            if let Some(n) = self.imp().network.borrow().as_ref() {
                n.looking.set(false);
            }
            self.update_looking();
        }
    }

    /// the computers found, as the service has them
    pub(crate) fn set_discovered(&self, found: Vec<Discovered>) {
        let network = self.imp().network.borrow();
        let Some(n) = network.as_ref() else {
            return;
        };
        while let Some(row) = n.list.row_at_index(0) {
            n.list.remove(&row);
        }
        n.list.set_placeholder(Some(&n.placeholder));
        for d in found {
            let mut about = vec![d.address.to_string()];
            if !d.version.is_empty() {
                about.push(format!("RustInput {}", d.version));
            }
            if !d.os.is_empty() {
                about.push(system(&d.os));
            }
            if d.authorized {
                about.push(tr("authorized").to_owned());
            }
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&d.name).as_str())
                .subtitle(about.join(" · "))
                .build();
            let icon = gtk::Image::from_icon_name("computer-symbolic");
            row.add_prefix(&icon);
            if d.added {
                let added = gtk::Label::new(Some(tr("Added")));
                added.add_css_class("dim-label");
                row.add_suffix(&added);
            } else {
                let add = gtk::Button::builder()
                    .label(tr("Add"))
                    .valign(gtk::Align::Center)
                    .build();
                add.connect_clicked(clone!(
                    #[weak(rename_to = window)]
                    self,
                    move |button| {
                        button.set_sensitive(false);
                        window.request(FrontendRequest::AddDiscovered {
                            name: d.name.clone(),
                            address: d.address,
                            port: d.port,
                        });
                        window.show_toast(&trf(
                            "{name} added: switch it on and move the pointer to it, it shows a \
                            code to enter here",
                            &[("name", &d.name)],
                        ));
                    }
                ));
                row.add_suffix(&add);
            }
            n.list.append(&row);
        }
    }
}
