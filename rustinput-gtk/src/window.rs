mod clipboard;
mod imp;
pub(crate) mod network;
pub(crate) mod shortcuts;

use std::collections::HashMap;

use adw::prelude::*;
use adw::subclass::prelude::*;
use glib::{Object, clone};
use gtk::{
    NoSelection, gio,
    glib::{self, closure_local},
};

use rustinput_ipc::{
    ClientConfig, ClientHandle, ClientState, DEFAULT_PORT, FileOptions, FrontendRequest,
    FrontendRequestWriter, LinkStats, Position, Theme, UpdateState,
};

use crate::{
    arrangement::ArrangementArea,
    authorization_window::AuthorizationWindow,
    fingerprint_window::FingerprintWindow,
    i18n::{self, tr, trf},
    key_object::KeyObject,
    key_row::KeyRow,
};

/// the editor of the arrangement, its state line, its buttons and, beside
/// it, the selected computer: where the mouse leaves it
pub struct ArrangementParts {
    area: ArrangementArea,
    status: gtk::Label,
    apply: gtk::Button,
    undo: gtk::Button,
    links: gtk::ListBox,
    links_title: gtk::Label,
    /// what to do, while no computer is selected
    hint: gtk::Label,
    /// the connection of the selected computer, on the page of the computers
    details: gtk::Button,
    /// the client of the selected computer, none for this one
    selected: std::cell::Cell<Option<ClientHandle>>,
}

use super::{client_object::ClientObject, client_row::ClientRow};

#[cfg(target_os = "macos")]
fn set_button_content_label(button: &gtk::Button, label: &str) {
    // The Reenable/Grant/Relaunch button wraps its icon+label in an
    // AdwButtonContent (see window.ui). Walk into it and swap the label
    // rather than GtkButton::set_label, which would replace the content
    // widget and drop the icon.
    if let Some(content) = button.child().and_downcast::<adw::ButtonContent>() {
        content.set_label(label);
    }
}

glib::wrapper! {
    pub struct Window(ObjectSubclass<imp::Window>)
        @extends adw::ApplicationWindow, gtk::Window, gtk::Widget,
        @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible, gtk::Buildable,
                    gtk::ConstraintTarget, gtk::Native, gtk::Root, gtk::ShortcutManager;
}

impl Window {
    pub(super) fn new(app: &adw::Application, conn: FrontendRequestWriter) -> Self {
        let window: Self = Object::builder().property("application", app).build();
        window
            .imp()
            .frontend_request_writer
            .borrow_mut()
            .replace(conn);
        let version = crate::APP_VERSION.get().copied().unwrap_or_default();
        window.set_title(Some(&format!("RustInput {version}")));
        window.imp().version_row.set_subtitle(&crate::version_str());
        let imp = window.imp();
        imp.version_label.set_text(version);
        imp.version_label
            .set_tooltip_text(Some(&format!("Version {}", crate::version_str())));
        if let Some(dir) = rustinput_ipc::log_dir().filter(|dir| dir.is_dir()) {
            let row = &window.imp().logs_row;
            row.set_subtitle(&dir.display().to_string());
            row.set_visible(true);
        }
        window
    }

    /// The page of the arrangement: the editor with a line on what is not
    /// applied or not reached, Undo and Apply below it, and beside it the
    /// selected computer: the computers next to it, each with whether they
    /// meet over the whole edges, and the way to its connection.
    pub(super) fn setup_arrangement(&self) {
        let area = ArrangementArea::default();
        let frame = gtk::Frame::builder()
            .child(&area)
            .vexpand(true)
            .hexpand(true)
            .build();
        frame.add_css_class("view");
        frame.add_css_class("arrangement");
        let status = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .hexpand(true)
            .build();
        status.add_css_class("dim-label");
        let undo = gtk::Button::with_label(tr("Undo changes"));
        let apply = gtk::Button::with_label(tr("Apply"));
        apply.add_css_class("suggested-action");
        let identify = gtk::Button::builder()
            .child(
                &adw::ButtonContent::builder()
                    .icon_name("video-display-symbolic")
                    .label(tr("Identify monitors"))
                    .build(),
            )
            .tooltip_text(tr(
                "Shows the number of each monitor on it for a few seconds, on every computer",
            ))
            .build();
        let bar = gtk::Box::builder().spacing(8).margin_top(10).build();
        bar.append(&identify);
        bar.append(&status);
        bar.append(&undo);
        bar.append(&apply);
        let editor = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .hexpand(true)
            .vexpand(true)
            .build();
        editor.append(&frame);
        editor.append(&bar);

        let links_title = gtk::Label::builder().xalign(0.0).wrap(true).build();
        links_title.add_css_class("heading");
        let hint = gtk::Label::builder()
            .label(tr(
                "Click a computer to see where the mouse leaves it and to choose whole edges.",
            ))
            .xalign(0.0)
            .wrap(true)
            .build();
        hint.add_css_class("dim-label");
        let links = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .build();
        links.add_css_class("boxed-list");
        let details = gtk::Button::builder()
            .child(
                &adw::ButtonContent::builder()
                    .icon_name("go-next-symbolic")
                    .label(tr("Connection settings"))
                    .build(),
            )
            .halign(gtk::Align::Start)
            .margin_top(6)
            .build();
        let panel = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .width_request(300)
            .hexpand(false)
            .build();
        panel.append(&links_title);
        panel.append(&hint);
        panel.append(&links);
        panel.append(&details);

        // a column of its own width: the editor takes the rest
        let side = adw::Clamp::builder()
            .maximum_size(340)
            .tightening_threshold(340)
            .hexpand(false)
            .valign(gtk::Align::Start)
            .child(&panel)
            .build();
        let content = gtk::Box::builder().spacing(24).vexpand(true).build();
        content.append(&editor);
        content.append(&side);
        self.imp().arrangement_box.append(&content);

        area.connect_changed(clone!(
            #[weak(rename_to = window)]
            self,
            move || window.arrangement_changed_here()
        ));
        area.connect_selected(clone!(
            #[weak(rename_to = window)]
            self,
            move |handle| {
                if let Some(parts) = window.imp().arrangement.borrow().as_ref() {
                    parts.selected.set(handle);
                }
                window.open_connection(handle);
                window.arrangement_changed_here();
            }
        ));
        undo.connect_clicked(clone!(
            #[weak]
            area,
            move |_| area.undo()
        ));
        identify.connect_clicked(clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.request(FrontendRequest::IdentifyMonitors)
        ));
        apply.connect_clicked(clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.apply_arrangement()
        ));
        details.connect_clicked(clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.show_connection_of_selected()
        ));
        self.imp().arrangement.replace(Some(ArrangementParts {
            area,
            status,
            apply,
            undo,
            links,
            links_title,
            hint,
            details,
            selected: Default::default(),
        }));
        self.arrangement_changed_here();
    }

    /// the connection of the computer selected in the arrangement, open, on
    /// the page of the computers
    fn show_connection_of_selected(&self) {
        let Some(handle) = self
            .imp()
            .arrangement
            .borrow()
            .as_ref()
            .and_then(|p| p.selected.get())
        else {
            return;
        };
        self.imp().stack.set_visible_child_name("computers");
        self.open_connection(Some(handle));
        if let Some(row) = self.row_for_handle(handle) {
            row.grab_focus();
        }
    }

    /// the service told how the computers are arranged
    pub(super) fn show_arrangement(&self, view: &rustinput_ipc::layout::ArrangementView) {
        if let Some(parts) = self.imp().arrangement.borrow().as_ref() {
            parts.area.set_view(view);
        }
        let arranged = !view.layout.machines.is_empty();
        for idx in 0..self.clients().n_items() {
            if let Some(row) = self.row_by_idx(idx as i32) {
                row.set_arranged(arranged);
            }
        }
    }

    /// the connection of the computer selected in the arrangement opens,
    /// the others close
    fn open_connection(&self, handle: Option<ClientHandle>) {
        for idx in 0..self.clients().n_items() {
            let (Some(client), Some(row)) = (self.client_by_idx(idx), self.row_by_idx(idx as i32))
            else {
                continue;
            };
            let open = Some(client.handle()) == handle;
            if row.is_expanded() != open {
                row.set_expanded(open);
            }
        }
    }

    fn select_in_arrangement(&self, handle: ClientHandle) {
        if let Some(parts) = self.imp().arrangement.borrow().as_ref() {
            parts.area.select(handle);
        }
    }

    /// the computer the mouse of this one is on, none for this one
    pub(super) fn set_pointer_at(&self, handle: Option<ClientHandle>) {
        if let Some(parts) = self.imp().arrangement.borrow().as_ref() {
            parts.area.set_pointer_at(handle);
        }
    }

    fn apply_arrangement(&self) {
        let Some(layout) = self
            .imp()
            .arrangement
            .borrow()
            .as_ref()
            .map(|p| p.area.layout())
        else {
            return;
        };
        self.request(FrontendRequest::Arrange(layout));
        if let Some(parts) = self.imp().arrangement.borrow().as_ref() {
            parts.area.mark_applied();
        }
        self.show_toast(tr("Arrangement applied"));
    }

    /// the computer `name` did not show the numbers of its monitors
    pub(super) fn monitors_not_identified(&self, name: &str, error: &str) {
        self.show_toast(&trf(
            "{name} did not show the numbers of its monitors: {error}",
            &[("name", &name), ("error", &error)],
        ));
    }

    /// the screen of the computer `name` did not lock
    pub(super) fn screen_not_locked(&self, name: &str, error: &str) {
        self.show_toast(&trf(
            "The screen of {name} did not lock: {error}",
            &[("name", &name), ("error", &error)],
        ));
    }

    /// the arrangement went to `name`, or not
    pub(super) fn arrangement_sent(&self, name: &str, error: Option<&str>) {
        match error {
            None => self.show_toast(&trf("Arrangement sent to {name}", &[("name", &name)])),
            Some(e) => self.show_toast(&trf(
                "Arrangement not sent to {name}: {error}",
                &[("name", &name), ("error", &e)],
            )),
        }
    }

    /// another computer changed the arrangement
    pub(super) fn arrangement_received(&self, by: &str) {
        self.show_toast(&trf("{name} changed the arrangement", &[("name", &by)]));
    }

    /// what the line under the editor says, the buttons and the rows of the
    /// selected computer, after any change in the editor
    fn arrangement_changed_here(&self) {
        let parts = self.imp().arrangement.borrow();
        let Some(parts) = parts.as_ref() else {
            return;
        };
        let dirty = parts.area.is_dirty();
        let unreachable = parts.area.unreachable();
        let guessed = parts.area.guessed();
        let text = if !unreachable.is_empty() {
            trf(
                "{names} does not touch a computer this one reaches: move it next to one",
                &[("names", &unreachable.join(", "))],
            )
        } else if parts.area.is_proposal() && dirty {
            tr("Made from the connections as they are now: arrange and apply").to_owned()
        } else if dirty {
            tr("Changes not applied yet").to_owned()
        } else if !guessed.is_empty() {
            trf(
                "{names} did not tell its size: update RustInput there to 0.26.0 or newer",
                &[("names", &guessed.join(", "))],
            )
        } else {
            tr("Applied on the computers of the arrangement").to_owned()
        };
        parts.status.set_text(&text);
        parts.apply.set_sensitive(dirty && unreachable.is_empty());
        parts.undo.set_sensitive(dirty && !parts.area.is_proposal());

        while let Some(row) = parts.links.first_child() {
            parts.links.remove(&row);
        }
        let selected = parts.area.selected_name();
        let links = parts.area.selected_links();
        parts.links_title.set_visible(selected.is_some());
        parts.links.set_visible(selected.is_some());
        parts.hint.set_visible(selected.is_none());
        parts
            .details
            .set_visible(selected.is_some() && parts.selected.get().is_some());
        let Some(selected) = selected else {
            return;
        };
        parts.links_title.set_text(&trf(
            "Where the mouse leaves {name}",
            &[("name", &selected)],
        ));
        if links.is_empty() {
            let row = adw::ActionRow::builder()
                .title(tr("Nowhere"))
                .subtitle(tr("It touches no other computer"))
                .build();
            parts.links.append(&row);
        }
        for link in links {
            let name = &link.other_name;
            let title = match link.pos {
                Position::Left => trf("Left edge: {name}", &[("name", name)]),
                Position::Right => trf("Right edge: {name}", &[("name", name)]),
                Position::Top => trf("Top edge: {name}", &[("name", name)]),
                Position::Bottom => trf("Bottom edge: {name}", &[("name", name)]),
            };
            let subtitle = if link.whole && link.can_be_whole {
                tr("The whole edge of both, the place along it in proportion").to_owned()
            } else if link.can_be_whole {
                trf(
                    "{part}, where they touch; switch on for the whole edge of both, in proportion",
                    &[("part", &link.part)],
                )
            } else {
                trf(
                    "{part}, where they touch: the edge is shared",
                    &[("part", &link.part)],
                )
            };
            let row = adw::ActionRow::builder()
                .title(title)
                .subtitle(subtitle)
                .build();
            let switch = gtk::Switch::builder()
                .valign(gtk::Align::Center)
                .active(link.whole && link.can_be_whole)
                .sensitive(link.can_be_whole)
                .tooltip_text(tr("Whole edge, in proportion"))
                .build();
            let other = link.other.clone();
            switch.connect_active_notify(clone!(
                #[weak(rename_to = area)]
                parts.area,
                move |switch| area.set_whole(&other, switch.is_active())
            ));
            row.add_suffix(&switch);
            row.set_activatable_widget(Some(&switch));
            parts.links.append(&row);
        }
    }

    /// Opens a web page or a folder the way the system does. A window not
    /// on screen is no parent: one never shown (RustInput started into the
    /// tray) has no surface yet, and GTK crashed on X11 asking for it (the
    /// tray icon and the notification open pages without the window).
    pub(super) fn open_uri(&self, uri: &str) {
        let parent = self.is_mapped().then_some(self);
        gtk::show_uri(parent, uri, gtk::gdk::CURRENT_TIME);
    }

    fn clients(&self) -> gio::ListStore {
        self.imp()
            .clients
            .borrow()
            .clone()
            .expect("Could not get clients")
    }

    fn authorized(&self) -> gio::ListStore {
        self.imp()
            .authorized
            .borrow()
            .clone()
            .expect("Could not get authorized")
    }

    /// the active clients by handle with their names, for the menu of the
    /// tray icon (macOS has its own menu bar item, without them)
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub(crate) fn active_computers(&self) -> Vec<(ClientHandle, String)> {
        (0..self.clients().n_items())
            .filter_map(|i| self.client_by_idx(i))
            .map(|c| c.get_data())
            .filter(|d| d.active)
            .map(|d| {
                let name = d
                    .hostname
                    .clone()
                    .or_else(|| d.ips.first().cloned())
                    .unwrap_or_else(|| format!("{}", d.handle));
                (d.handle, rustinput_ipc::plain_text(&name, 64))
            })
            .collect()
    }

    fn client_by_idx(&self, idx: u32) -> Option<ClientObject> {
        self.clients().item(idx).map(|o| o.downcast().unwrap())
    }

    fn authorized_by_idx(&self, idx: u32) -> Option<KeyObject> {
        self.authorized().item(idx).map(|o| o.downcast().unwrap())
    }

    fn row_by_idx(&self, idx: i32) -> Option<ClientRow> {
        self.imp()
            .client_list
            .get()
            .row_at_index(idx)
            .map(|o| o.downcast().expect("expected ClientRow"))
    }

    fn setup_authorized(&self) {
        let store = gio::ListStore::new::<KeyObject>();
        self.imp().authorized.replace(Some(store));
        let selection_model = NoSelection::new(Some(self.authorized()));
        self.imp().authorized_list.bind_model(
            Some(&selection_model),
            clone!(
                #[weak(rename_to = window)]
                self,
                #[upgrade_or_panic]
                move |obj| {
                    let key_obj = obj.downcast_ref().expect("object of type `KeyObject`");
                    let row = window.create_key_row(key_obj);
                    row.connect_closure(
                        "request-delete",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: KeyRow| {
                                if let Some(key_obj) = window.authorized_by_idx(row.index() as u32)
                                {
                                    window.request_fingerprint_remove(key_obj.get_fingerprint());
                                }
                            }
                        ),
                    );
                    row.upcast()
                }
            ),
        )
    }

    fn setup_clients(&self) {
        let model = gio::ListStore::new::<ClientObject>();
        self.imp().clients.replace(Some(model));

        let selection_model = NoSelection::new(Some(self.clients()));
        self.imp().client_list.bind_model(
            Some(&selection_model),
            clone!(
                #[weak(rename_to = window)]
                self,
                #[upgrade_or_panic]
                move |obj| {
                    let client_object = obj
                        .downcast_ref()
                        .expect("Expected object of type `ClientObject`.");
                    let row = window.create_client_row(client_object);
                    // opening a connection selects its computer in the
                    // arrangement
                    let handle = client_object.handle();
                    row.connect_expanded_notify(clone!(
                        #[weak]
                        window,
                        move |row| {
                            if row.is_expanded() {
                                window.select_in_arrangement(handle);
                            }
                        }
                    ));
                    row.connect_closure(
                        "request-hostname-change",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: ClientRow, hostname: String| {
                                log::debug!("request-hostname-change");
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    let hostname = Some(hostname).filter(|s| !s.is_empty());
                                    /* changed in response to FrontendEvent
                                     * -> do not request additional update */
                                    window.request(FrontendRequest::UpdateHostname(
                                        client.handle(),
                                        hostname,
                                    ));
                                }
                            }
                        ),
                    );
                    row.connect_closure(
                        "request-port-change",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: ClientRow, port: u32| {
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    window.request(FrontendRequest::UpdatePort(
                                        client.handle(),
                                        port as u16,
                                    ));
                                }
                            }
                        ),
                    );
                    row.connect_closure(
                        "request-activate",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: ClientRow, active: bool| {
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    log::debug!(
                                        "request: {} client",
                                        if active { "activating" } else { "deactivating" }
                                    );
                                    window.request(FrontendRequest::Activate(
                                        client.handle(),
                                        active,
                                    ));
                                }
                            }
                        ),
                    );
                    row.connect_closure(
                        "request-delete",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: ClientRow| {
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    window.request(FrontendRequest::Delete(client.handle()));
                                }
                            }
                        ),
                    );
                    row.connect_closure(
                        "request-pair",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: ClientRow| {
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    let data = client.get_data();
                                    window.open_pairing_dialog(data.handle, data.hostname);
                                }
                            }
                        ),
                    );
                    row.connect_closure(
                        "request-dns",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: ClientRow| {
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    window.request(FrontendRequest::ResolveDns(
                                        client.get_data().handle,
                                    ));
                                }
                            }
                        ),
                    );
                    row.connect_closure(
                        "request-position-change",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: ClientRow, pos_idx: u32| {
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    let position = match pos_idx {
                                        0 => Position::Left,
                                        1 => Position::Right,
                                        2 => Position::Top,
                                        _ => Position::Bottom,
                                    };
                                    window.request(FrontendRequest::UpdatePosition(
                                        client.handle(),
                                        position,
                                    ));
                                }
                            }
                        ),
                    );
                    row.upcast()
                }
            ),
        );
    }

    fn setup_icon(&self) {
        self.set_icon_name(Some("io.github.felipemarques.RustInput"));
    }

    /// workaround for a bug in libadwaita that shows an ugly line beneath
    /// the last element if a placeholder is set.
    /// https://gitlab.gnome.org/GNOME/gtk/-/merge_requests/6308
    fn update_placeholder_visibility(&self) {
        let visible = self.clients().n_items() == 0;
        let placeholder = self.imp().client_placeholder.get();
        self.imp().client_list.set_placeholder(match visible {
            true => Some(&placeholder),
            false => None,
        });
    }

    fn update_auth_placeholder_visibility(&self) {
        let visible = self.authorized().n_items() == 0;
        let placeholder = self.imp().authorized_placeholder.get();
        self.imp().authorized_list.set_placeholder(match visible {
            true => Some(&placeholder),
            false => None,
        });
    }

    fn create_client_row(&self, client_object: &ClientObject) -> ClientRow {
        let row = ClientRow::new(client_object);
        row.bind(client_object);
        row
    }

    fn create_key_row(&self, key_object: &KeyObject) -> KeyRow {
        let row = KeyRow::new();
        row.bind(key_object);
        row
    }

    pub(super) fn new_client(
        &self,
        handle: ClientHandle,
        client: ClientConfig,
        state: ClientState,
    ) {
        let client = ClientObject::new(handle, client, state.clone());
        self.clients().append(&client);
        self.update_placeholder_visibility();
        self.update_dns_state(handle, !state.ips.is_empty());
    }

    pub(super) fn update_client_list(
        &self,
        clients: Vec<(ClientHandle, ClientConfig, ClientState)>,
    ) {
        for (handle, client, state) in clients {
            if self.client_idx(handle).is_some() {
                self.update_client_config(handle, client);
                self.update_client_state(handle, state);
            } else {
                self.new_client(handle, client, state);
            }
        }
    }

    pub(super) fn update_port(&self, port: u16, msg: Option<String>) {
        if let Some(msg) = msg {
            self.show_toast(msg.as_str());
        }
        self.imp().set_port(port);
    }

    fn client_idx(&self, handle: ClientHandle) -> Option<usize> {
        self.clients()
            .iter::<ClientObject>()
            .position(|c| c.ok().map(|c| c.handle() == handle).unwrap_or_default())
    }

    pub(super) fn delete_client(&self, handle: ClientHandle) {
        let Some(idx) = self.client_idx(handle) else {
            log::warn!("could not find client with handle {handle}");
            return;
        };

        self.clients().remove(idx as u32);
        if self.clients().n_items() == 0 {
            self.update_placeholder_visibility();
        }
        self.forget_clipboard(handle);
    }

    pub(super) fn update_client_config(&self, handle: ClientHandle, client: ClientConfig) {
        let Some(row) = self.row_for_handle(handle) else {
            log::warn!("could not find row for handle {handle}");
            return;
        };
        row.set_hostname(client.hostname);
        row.set_port(client.port);
        row.set_position(client.pos);
    }

    pub(super) fn update_client_state(&self, handle: ClientHandle, state: ClientState) {
        let Some(row) = self.row_for_handle(handle) else {
            log::warn!("could not find row for handle {handle}");
            return;
        };
        let Some(client_object) = self.client_object_for_handle(handle) else {
            log::warn!("could not find row for handle {handle}");
            return;
        };

        /* activation state */
        row.set_active(state.active);
        row.imp().paused.set(state.paused);
        if let Some(parts) = self.imp().arrangement.borrow().as_ref() {
            parts
                .area
                .set_state(handle, state.active, state.alive, state.paused);
        }

        /* dns state */
        client_object.set_resolving(state.resolving);

        self.update_dns_state(handle, !state.ips.is_empty());
        let ips = state
            .ips
            .into_iter()
            .map(|ip| ip.to_string())
            .collect::<Vec<_>>();
        client_object.set_ips(ips);

        /* peer build version (drives the version-match indicator) */
        client_object.set_property(
            "peer-commit",
            crate::client_object::peer_commit_to_string(state.peer_commit),
        );
        row.refresh_version_status();
    }

    /// the input channel with every client over the last second
    pub(super) fn update_link_stats(&self, stats: &[LinkStats]) {
        for idx in 0..self.clients().n_items() {
            let (Some(client), Some(row)) = (self.client_by_idx(idx), self.row_by_idx(idx as i32))
            else {
                continue;
            };
            let handle = client.handle();
            row.show_link_stats(stats.iter().find(|s| s.handle == Some(handle)));
        }
    }

    fn client_object_for_handle(&self, handle: ClientHandle) -> Option<ClientObject> {
        self.client_idx(handle)
            .and_then(|i| self.client_by_idx(i as u32))
    }

    fn row_for_handle(&self, handle: ClientHandle) -> Option<ClientRow> {
        self.client_idx(handle)
            .and_then(|i| self.row_by_idx(i as i32))
    }

    fn update_dns_state(&self, handle: ClientHandle, resolved: bool) {
        if let Some(client_row) = self.row_for_handle(handle) {
            client_row.set_dns_state(resolved);
        }
    }

    fn request_port_change(&self) {
        let port = self
            .imp()
            .port_entry
            .get()
            .text()
            .as_str()
            .parse::<u16>()
            .unwrap_or(DEFAULT_PORT);
        self.request(FrontendRequest::ChangePort(port));
    }

    fn request_capture(&self) {
        self.request(FrontendRequest::EnableCapture);
    }

    fn request_emulation(&self) {
        self.request(FrontendRequest::EnableEmulation);
    }

    fn request_client_create(&self) {
        self.request(FrontendRequest::Create);
    }

    fn open_fingerprint_dialog(&self, fp: Option<String>) {
        let window = FingerprintWindow::new(fp);
        window.set_transient_for(Some(self));
        window.connect_closure(
            "confirm-clicked",
            false,
            closure_local!(
                #[strong(rename_to = parent)]
                self,
                move |w: FingerprintWindow, desc: String, fp: String| {
                    parent.request_fingerprint_add(desc, fp);
                    w.close();
                }
            ),
        );
        window.present();
    }

    fn request_fingerprint_add(&self, desc: String, fp: String) {
        self.request(FrontendRequest::AuthorizeKey(desc, fp));
    }

    fn request_fingerprint_remove(&self, fp: String) {
        self.request(FrontendRequest::RemoveAuthorizedKey(fp));
    }

    /// used by the tray icon, macOS has its own menu bar item
    #[cfg(not(target_os = "macos"))]
    pub(super) fn request_cancel_transfers(&self) {
        self.request(FrontendRequest::CancelTransfers);
    }

    /// Adds the settings of the file transfers. They show once the service
    /// said that file transfers are available.
    /// The appearance row, showing `theme`: picking another applies it right
    /// away and keeps it in the configuration.
    pub(super) fn setup_theme(&self, theme: Theme) {
        self.set_theme(theme);
        self.imp().theme_row.connect_selected_notify(clone!(
            #[weak(rename_to = window)]
            self,
            move |row| {
                if window.imp().updating_theme.get() {
                    return;
                }
                let theme = theme_at(row.selected());
                crate::apply_theme(theme);
                window.request(FrontendRequest::SetTheme(theme));
            }
        ));
    }

    /// `theme` from the configuration: applied and shown
    pub(super) fn set_theme(&self, theme: Theme) {
        let imp = self.imp();
        imp.updating_theme.set(true);
        imp.theme_row.set_selected(theme_index(theme));
        imp.updating_theme.set(false);
        crate::apply_theme(theme);
    }

    /// The language row: "As the system" and the languages, each in itself.
    /// Picking one keeps it in the configuration (the radial menu takes it
    /// right away) and offers to start RustInput again in it.
    pub(super) fn setup_language(&self) {
        let names: Vec<String> = std::iter::once(trf(
            "As the system ({language})",
            &[("language", &rustinput_i18n::system().name)],
        ))
        .chain(i18n::LANGUAGES.iter().map(|l| l.name.to_owned()))
        .collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let row = &self.imp().language_row;
        row.set_model(Some(&gtk::StringList::new(&names)));
        row.set_selected(i18n::index_of(i18n::configured()));
        row.connect_selected_notify(clone!(
            #[weak(rename_to = window)]
            self,
            move |row| {
                let picked = i18n::language_at(row.selected());
                window.request(FrontendRequest::SetLanguage(
                    picked.map(|l| l.code.to_owned()),
                ));
                let shown = rustinput_i18n::resolve(picked.map(|l| l.code));
                if shown == i18n::current() {
                    return;
                }
                let toast = adw::Toast::builder()
                    .title(tr(
                        "The windows change language when RustInput starts again",
                    ))
                    .button_label(tr("Restart now"))
                    .action_name("app.restart")
                    .timeout(0)
                    .build();
                window.add_toast(toast);
            }
        ));
    }

    /// The preferences of the updates. The service tells what it knows with
    /// [`Window::set_updates`].
    pub(super) fn setup_updates(&self) {
        let imp = self.imp();
        imp.update_check_switch.connect_active_notify(clone!(
            #[weak(rename_to = window)]
            self,
            move |switch| {
                if window.imp().updating_update_switch.get() {
                    return;
                }
                window.request(FrontendRequest::SetCheckUpdates(switch.is_active()));
            }
        ));
        imp.update_install_button.set_label(install_label());
        // the question shows once the service told that it was not asked yet
        imp.updates.borrow_mut().check = Some(false);
        self.show_updates();
    }

    /// what the service knows about new versions
    /// whether RustInput is paused here, as the service says: the button
    /// in the header bar, the line under it, the tooltips
    pub(super) fn set_paused(&self, paused: bool) {
        let imp = self.imp();
        imp.paused.set(paused);
        imp.pause_button.set_active(paused);
        imp.pause_button.set_tooltip_text(Some(if paused {
            tr("Resume RustInput")
        } else {
            tr("Pause RustInput")
        }));
        imp.pause_revealer.set_reveal_child(paused);
        self.update_capture_emulation_status();
    }

    pub(crate) fn is_paused(&self) -> bool {
        self.imp().paused.get()
    }

    /// asks the service to pause RustInput, or to go on
    pub(crate) fn request_pause(&self, paused: bool) {
        self.request(FrontendRequest::SetPaused(paused));
    }

    pub(super) fn set_updates(&self, state: UpdateState) {
        self.imp().updates.replace(state);
        self.show_updates();
    }

    /// the row of the preferences and the line under the header bar, from
    /// what the service knows
    fn show_updates(&self) {
        let imp = self.imp();
        let state = imp.updates.borrow().clone();
        imp.updating_update_switch.set(true);
        imp.update_check_switch
            .set_active(state.check == Some(true));
        imp.updating_update_switch.set(false);

        let current = crate::APP_VERSION.get().copied().unwrap_or_default();
        let latest = state.latest.clone().unwrap_or_default();
        let status = if state.checking {
            tr("Asking GitHub…").to_owned()
        } else if let Some(error) = &state.error {
            trf("Could not ask GitHub: {error}", &[("error", error)])
        } else if state.available {
            trf(
                "RustInput {version} is out; this computer has {current}",
                &[("version", &latest), ("current", &current)],
            )
        } else if state.latest.is_some() {
            trf("{version} is the latest version", &[("version", &current)])
        } else {
            trf("This computer has {current}", &[("current", &current)])
        };
        imp.update_status_row.set_subtitle(&status);
        imp.update_check_button.set_sensitive(!state.checking);
        imp.update_install_button.set_visible(state.available);

        let dismissed = imp.update_dismissed.borrow().clone();
        let banner = match state.check {
            None => Some((
                tr(
                    "Should RustInput look for new versions? It asks GitHub for the number of \
                    the latest version when it starts and every 12 hours; nothing about you \
                    or your computers is sent.",
                )
                .to_owned(),
                tr("No"),
                tr("Yes, look for them"),
            )),
            Some(true) if state.available && dismissed.as_deref() != state.latest.as_deref() => {
                Some((
                    trf(
                        "RustInput {version} is out; this computer has {current}.",
                        &[("version", &latest), ("current", &current)],
                    ),
                    tr("Later"),
                    install_label(),
                ))
            }
            _ => None,
        };
        match banner {
            Some((text, no, yes)) => {
                imp.update_label.set_text(&text);
                imp.update_no.set_label(no);
                imp.update_yes.set_label(yes);
                imp.update_revealer.set_reveal_child(true);
            }
            None => imp.update_revealer.set_reveal_child(false),
        }
    }

    /// the answer in the line under the header bar: to the question whether
    /// to look for new versions, or to the new version
    pub(super) fn update_answered(&self, yes: bool) {
        let state = self.imp().updates.borrow().clone();
        if state.check.is_none() {
            self.request(FrontendRequest::SetCheckUpdates(yes));
            self.imp().update_revealer.set_reveal_child(false);
        } else if yes {
            self.install_update();
        } else {
            // until the next version
            self.imp().update_dismissed.replace(state.latest);
            self.show_updates();
        }
    }

    /// Gets the new version: on Windows the updater the installer put beside
    /// RustInput, which asks for administrator rights, closes RustInput and
    /// installs; elsewhere the page of the release opens.
    pub(crate) fn install_update(&self) {
        #[cfg(windows)]
        if let Some(script) = updater_script() {
            let powershell = std::env::var_os("SystemRoot")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"))
                .join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
            match std::process::Command::new(powershell)
                .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
                .arg(&script)
                .spawn()
            {
                Ok(_) => {
                    self.show_toast(tr(
                        "The update starts: Windows asks for administrator rights",
                    ));
                    return;
                }
                Err(e) => log::warn!("could not start {}: {e}", script.display()),
            }
        }
        let url = self.imp().updates.borrow().url.clone();
        self.open_uri(&url);
    }

    pub(super) fn setup_file_settings(&self, open_transfers: impl Fn() + 'static) {
        let switch_row = |title: &str, subtitle: &str| {
            let row = adw::ActionRow::builder()
                .title(title)
                .subtitle(subtitle)
                .build();
            let switch = gtk::Switch::builder().valign(gtk::Align::Center).build();
            row.add_suffix(&switch);
            row.set_activatable_widget(Some(&switch));
            (row, switch)
        };
        let (send_row, send) = switch_row(
            tr("Send copied files automatically"),
            tr(
                "Files copied in the file manager go along with the pointer. When this \
                is off, only files chosen with “Copy to RustInput” are sent",
            ),
        );
        let (menu_row, menu) = switch_row(
            tr("“Copy to RustInput” in the file manager"),
            tr("In the menu of files and folders: Explorer, Nemo and Nautilus"),
        );
        let (show_row, show) = switch_row(
            tr("Show the transfers window"),
            tr("With the progress, when files are sent or received"),
        );
        let ask_row = adw::ActionRow::builder()
            .title(tr("Ask before receiving more than"))
            .subtitle(tr(
                "Files copied on another computer that come along with the pointer. 0: never ask",
            ))
            .build();
        let ask = gtk::SpinButton::builder()
            .adjustment(&gtk::Adjustment::new(
                0.0,
                0.0,
                1_048_576.0,
                256.0,
                1024.0,
                0.0,
            ))
            .digits(0)
            .numeric(true)
            .valign(gtk::Align::Center)
            .build();
        let mb = gtk::Label::new(Some("MB"));
        mb.add_css_class("dim-label");
        ask_row.add_suffix(&ask);
        ask_row.add_suffix(&mb);
        ask_row.set_activatable_widget(Some(&ask));
        let open_row = adw::ActionRow::builder()
            .title(tr("File transfers"))
            .subtitle(tr("Progress, history and log"))
            .build();
        let open = gtk::Button::builder()
            .label(tr("Open"))
            .valign(gtk::Align::Center)
            .build();
        open.connect_clicked(move |_| open_transfers());
        open_row.add_suffix(&open);
        open_row.set_activatable_widget(Some(&open));
        let group = &self.imp().files_group;
        for row in [&send_row, &ask_row, &menu_row, &show_row, &open_row] {
            group.add(row);
        }
        ask.connect_value_changed(clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.file_switch_changed()
        ));
        self.imp().ask_spin.replace(Some(ask));
        for switch in [&send, &menu, &show] {
            switch.connect_active_notify(clone!(
                #[weak(rename_to = window)]
                self,
                move |_| window.file_switch_changed()
            ));
        }
        self.imp().file_switches.replace(Some([send, menu, show]));
    }

    /// shows what happens to copied files, as the service has it
    pub(super) fn set_file_options(&self, options: FileOptions) {
        let imp = self.imp();
        imp.file_options.set(Some(options));
        imp.files_group.set_visible(options.available);
        if let Some([send, menu, show]) = imp.file_switches.borrow().as_ref() {
            imp.updating_files.set(true);
            send.set_active(options.send_copied);
            menu.set_active(options.file_manager_menu);
            show.set_active(options.show_window);
            if let Some(ask) = imp.ask_spin.borrow().as_ref() {
                ask.set_value(options.ask_above_mb as f64);
            }
            imp.updating_files.set(false);
        }
    }

    /// what happens to copied files, once the service said so (for the tray,
    /// macOS has its own menu bar item)
    #[cfg(not(target_os = "macos"))]
    pub(crate) fn file_options(&self) -> Option<FileOptions> {
        self.imp().file_options.get()
    }

    fn file_switch_changed(&self) {
        let imp = self.imp();
        if imp.updating_files.get() {
            return;
        }
        let Some(options) = imp.file_options.get() else {
            return;
        };
        let Some([send, menu, show]) = imp.file_switches.borrow().clone() else {
            return;
        };
        let ask_above_mb = imp
            .ask_spin
            .borrow()
            .as_ref()
            .map_or(options.ask_above_mb, |a| a.value() as u64);
        self.request(FrontendRequest::SetFileOptions(FileOptions {
            send_copied: send.is_active(),
            file_manager_menu: menu.is_active(),
            show_window: show.is_active(),
            ask_above_mb,
            ..options
        }));
    }

    pub(crate) fn request(&self, request: FrontendRequest) {
        let mut requester = self.imp().frontend_request_writer.borrow_mut();
        let requester = requester.as_mut().unwrap();
        if let Err(e) = requester.request(request) {
            log::error!("error sending message: {e}");
        };
    }

    pub(super) fn show_toast(&self, msg: &str) {
        let toast = adw::Toast::new(msg);
        self.add_toast(toast);
    }

    pub(super) fn add_toast(&self, toast: adw::Toast) {
        let toast_overlay = &self.imp().toast_overlay;
        toast_overlay.add_toast(toast);
    }

    pub(super) fn set_capture(&self, active: bool) {
        self.imp().capture_active.replace(active);
        self.update_capture_emulation_status();
    }

    pub(super) fn set_emulation(&self, active: bool) {
        self.imp().emulation_active.replace(active);
        self.update_capture_emulation_status();
    }

    #[cfg(target_os = "macos")]
    pub(super) fn refresh_capture_emulation_status(&self) {
        self.update_capture_emulation_status();
    }

    /// Whether this computer captures and emulates: a green row each when it
    /// does, a warning with a button to try again when it does not, and a
    /// mark on the page of the computers.
    fn update_capture_emulation_status(&self) {
        let imp = self.imp();
        let capture = imp.capture_active.get();
        let emulation = imp.emulation_active.get();

        #[cfg(target_os = "macos")]
        {
            // On macOS, capture and emulation share the same TCC gate
            // (Accessibility). Collapse to a single row — emulation_status_row
            // stays hidden and capture_status_row doubles as the shared
            // status indicator. When something is off, its text and button
            // mutate based on whether we're waiting for AX or waiting for the
            // user to relaunch the app.
            let ok = capture && emulation;
            imp.emulation_status_row.set_visible(false);
            show_status(
                &imp.capture_status_row,
                &imp.capture_status_icon,
                &imp.input_capture_button,
                ok,
                (
                    tr("Sharing the mouse and keyboard"),
                    tr("Capture and emulation are on"),
                ),
                (
                    tr("Input capture is off"),
                    tr("Needed for every connection"),
                ),
            );
            if !ok {
                self.update_macos_warning_row_text();
            }
        }

        #[cfg(not(target_os = "macos"))]
        {
            show_status(
                &imp.capture_status_row,
                &imp.capture_status_icon,
                &imp.input_capture_button,
                capture,
                (
                    tr("Sending"),
                    tr("Past a screen edge, the mouse and keyboard go to the next computer"),
                ),
                (
                    tr("Input capture is off"),
                    tr("Needed for every connection"),
                ),
            );
            show_status(
                &imp.emulation_status_row,
                &imp.emulation_status_icon,
                &imp.input_emulation_button,
                emulation,
                (
                    tr("Receiving"),
                    tr("The other computers can control this one"),
                ),
                (
                    tr("Input emulation is off"),
                    tr("Needed for the other computers to control this one"),
                ),
            );
        }
        // paused: working, but the mouse and keyboard stay here
        if imp.paused.get() {
            let rows = [
                (
                    &imp.capture_status_row,
                    &imp.capture_status_icon,
                    capture,
                    tr("Paused: the mouse and keyboard stay on this computer"),
                ),
                (
                    &imp.emulation_status_row,
                    &imp.emulation_status_icon,
                    emulation,
                    tr("Paused: no other computer controls this one"),
                ),
            ];
            for (row, icon, active, subtitle) in rows {
                if active {
                    row.set_subtitle(subtitle);
                    icon.set_icon_name(Some("media-playback-pause-symbolic"));
                    icon.set_css_classes(&["warning"]);
                }
            }
        }
        imp.computers_page
            .set_needs_attention(!capture || !emulation);
    }

    #[cfg(target_os = "macos")]
    fn update_macos_warning_row_text(&self) {
        let row = &self.imp().capture_status_row;
        let button = &self.imp().input_capture_button;

        if crate::macos_privacy::accessibility_granted() {
            // AX granted but capture/emulation still off → the daemon
            // subprocess bailed at startup and needs a fresh process to
            // re-initialize with the new grant in place.
            row.set_title(tr("Relaunch required"));
            row.set_subtitle(tr(
                "Accessibility granted: restart to turn on capture and emulation",
            ));
            set_button_content_label(button, tr("Relaunch"));
        } else {
            // AX missing → send the user to System Settings.
            row.set_title(tr("Input capture is off"));
            row.set_subtitle(tr("Grant the Accessibility permission to turn it on"));
            set_button_content_label(button, tr("Grant"));
        }
    }

    pub(super) fn set_authorized_keys(&self, fingerprints: HashMap<String, String>) {
        let authorized = self.authorized();
        // clear list
        authorized.remove_all();
        // insert fingerprints
        for (fingerprint, description) in fingerprints {
            let key_obj = KeyObject::new(description, fingerprint);
            authorized.append(&key_obj);
        }
        self.update_auth_placeholder_visibility();
    }

    /// The fingerprint of this computer, shown eight bytes to a line, so it
    /// fits the column and reads like the one the other computer shows.
    pub(super) fn set_pk_fp(&self, fingerprint: &str) {
        let imp = self.imp();
        imp.fingerprint.replace(fingerprint.to_owned());
        let bytes: Vec<&str> = fingerprint.split(':').collect();
        let shown: Vec<String> = bytes.chunks(8).map(|chunk| chunk.join(":")).collect();
        imp.fingerprint_row.set_subtitle(&shown.join("\n"));
    }

    /// asks for the code another computer shows, to authorize both on each
    /// other
    fn open_pairing_dialog(&self, handle: ClientHandle, hostname: Option<String>) {
        let entry = gtk::Entry::builder()
            .placeholder_text("123 456")
            .input_purpose(gtk::InputPurpose::Digits)
            .max_length(7)
            .halign(gtk::Align::Center)
            .width_chars(9)
            .activates_default(true)
            .css_classes(["title-2"])
            .build();
        let text = gtk::Label::builder()
            .label(match &hostname {
                Some(name) => trf(
                    "When this computer tries to connect, {name} asks whether to authorize it \
                     and shows a code. Enter it here to authorize both computers on each \
                     other: for control, clipboard and files.",
                    &[("name", name)],
                ),
                None => tr(
                    "When this computer tries to connect, the other computer asks \
                     whether to authorize it and shows a code. Enter it here to authorize both \
                     computers on each other: for control, clipboard and files.",
                )
                .to_owned(),
            })
            .wrap(true)
            .max_width_chars(48)
            .build();
        let cancel = gtk::Button::with_label(tr("Cancel"));
        let authorize = gtk::Button::builder()
            .label(tr("Authorize"))
            .css_classes(["suggested-action"])
            .sensitive(false)
            .build();
        let buttons = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .halign(gtk::Align::End)
            .build();
        buttons.append(&cancel);
        buttons.append(&authorize);
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(24)
            .margin_bottom(24)
            .margin_start(24)
            .margin_end(24)
            .build();
        content.append(&text);
        content.append(&entry);
        content.append(&buttons);
        let page = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        page.append(
            &adw::HeaderBar::builder()
                .title_widget(&adw::WindowTitle::new(
                    &match &hostname {
                        Some(name) => trf("Authorize with {name}", &[("name", name)]),
                        None => tr("Authorize with the other computer").to_owned(),
                    },
                    "",
                ))
                .build(),
        );
        page.append(&content);
        let dialog = adw::Window::builder()
            .transient_for(self)
            .modal(true)
            .resizable(false)
            .content(&page)
            .build();
        dialog.set_default_widget(Some(&authorize));
        entry.connect_changed(clone!(
            #[weak]
            authorize,
            move |entry| {
                let digits = entry.text().chars().filter(char::is_ascii_digit).count();
                authorize.set_sensitive(digits == 6);
            }
        ));
        cancel.connect_clicked(clone!(
            #[weak]
            dialog,
            move |_| dialog.close()
        ));
        authorize.connect_clicked(clone!(
            #[weak(rename_to = window)]
            self,
            #[weak]
            dialog,
            #[weak]
            entry,
            move |_| {
                let code: String = entry.text().chars().filter(char::is_ascii_digit).collect();
                window.request(FrontendRequest::Pair { handle, code });
                window.show_toast(tr("Authorizing…"));
                dialog.close();
            }
        ));
        dialog.present();
        entry.grab_focus();
    }

    /// the other device entered the code shown here
    pub(super) fn paired(&self, fingerprint: &str, name: &str) {
        self.imp()
            .authorization_queue
            .borrow_mut()
            .retain(|w| w.fingerprint() != fingerprint);
        let shown = self.imp().authorization_window.borrow().clone();
        if let Some(w) = shown.filter(|w| w.fingerprint() == fingerprint) {
            w.close();
        }
        self.show_toast(&trf(
            "{name} and this computer authorized each other",
            &[("name", &name)],
        ));
    }

    pub(super) fn request_authorization(&self, fingerprint: &str, code: Option<&str>) {
        self.ask_authorization(AuthorizationWindow::new(fingerprint, code));
    }

    /// the computer `name` answered with a certificate not authorized here
    pub(super) fn server_not_authorized(&self, name: &str, fingerprint: &str) {
        self.ask_authorization(AuthorizationWindow::for_server(name, fingerprint));
    }

    /// Shows `window`. One about another computer than the open one waits
    /// for it: someone trying to connect at the moment the user authorizes
    /// a computer must not swap the fingerprint under the user's eyes. One
    /// about the same computer (a new code) takes its place.
    fn ask_authorization(&self, window: AuthorizationWindow) {
        let imp = self.imp();
        let open = imp.authorization_window.borrow().clone();
        if let Some(open) = open {
            if open.fingerprint() != window.fingerprint() {
                let mut queue = imp.authorization_queue.borrow_mut();
                queue.retain(|w| w.fingerprint() != window.fingerprint());
                queue.push_back(window);
                return;
            }
            // closing it shows no other: it is no longer the open one
            imp.authorization_window.replace(None);
            open.close();
        }
        window.set_transient_for(Some(self));
        window.connect_close_request(clone!(
            #[weak(rename_to = parent)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |closing| {
                let imp = parent.imp();
                let current = imp.authorization_window.borrow().clone();
                if current.as_ref() == Some(closing) {
                    imp.authorization_window.replace(None);
                    let next = imp.authorization_queue.borrow_mut().pop_front();
                    if let Some(next) = next {
                        parent.ask_authorization(next);
                    }
                }
                glib::Propagation::Proceed
            }
        ));
        window.connect_closure(
            "confirm-clicked",
            false,
            closure_local!(
                #[strong(rename_to = parent)]
                self,
                move |w: AuthorizationWindow, fp: String| {
                    w.close();
                    parent.open_fingerprint_dialog(Some(fp));
                }
            ),
        );
        window.connect_closure(
            "cancel-clicked",
            false,
            closure_local!(move |w: AuthorizationWindow| {
                w.close();
            }),
        );
        window.present();
        self.imp().authorization_window.replace(Some(window));
    }
}

/// the themes in the order of the appearance row
const THEMES: [Theme; 3] = [Theme::System, Theme::Light, Theme::Dark];

/// A row of the status: `ok` and `off` are its title and subtitle each way.
fn show_status(
    row: &adw::ActionRow,
    icon: &gtk::Image,
    button: &gtk::Button,
    active: bool,
    ok: (&str, &str),
    off: (&str, &str),
) {
    let (title, subtitle) = if active { ok } else { off };
    row.set_title(title);
    row.set_subtitle(subtitle);
    icon.set_icon_name(Some(if active {
        "emblem-ok-symbolic"
    } else {
        "dialog-warning-symbolic"
    }));
    icon.set_css_classes(if active { &["success"] } else { &["warning"] });
    button.set_visible(!active);
    row.set_visible(true);
}

/// what the button that gets a new version says: on Windows it installs
/// it, elsewhere it opens the page to download it
pub(crate) fn install_label() -> &'static str {
    #[cfg(windows)]
    if updater_script().is_some() {
        return tr("Update now");
    }
    tr("Download")
}

/// the updater the Windows installer puts beside RustInput
#[cfg(windows)]
fn updater_script() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let script = exe.parent()?.join("update-rustinput.ps1");
    script.is_file().then_some(script)
}

fn theme_index(theme: Theme) -> u32 {
    THEMES.iter().position(|t| *t == theme).unwrap_or(0) as u32
}

fn theme_at(index: u32) -> Theme {
    THEMES.get(index as usize).copied().unwrap_or_default()
}
