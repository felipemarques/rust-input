mod imp;

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
    FrontendRequestWriter, LinkStats, Position, Theme,
};

use crate::{
    arrangement::ArrangementArea, authorization_window::AuthorizationWindow,
    fingerprint_window::FingerprintWindow, key_object::KeyObject, key_row::KeyRow,
};

/// the editor of the arrangement, its state line, its buttons and the
/// computers next to the selected one
pub struct ArrangementParts {
    area: ArrangementArea,
    status: gtk::Label,
    apply: gtk::Button,
    undo: gtk::Button,
    links: gtk::ListBox,
    links_title: gtk::Label,
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

    /// The editor of the arrangement in its group: the area, a line on what
    /// is not applied or not reached, Undo and Apply, and the computers next
    /// to the selected one, each with whether they meet over the whole edges.
    pub(super) fn setup_arrangement(&self) {
        let area = ArrangementArea::default();
        let frame = gtk::Frame::builder().child(&area).build();
        frame.add_css_class("view");
        frame.add_css_class("arrangement");
        let status = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .hexpand(true)
            .build();
        status.add_css_class("dim-label");
        let undo = gtk::Button::with_label("Undo changes");
        let apply = gtk::Button::with_label("Apply");
        apply.add_css_class("suggested-action");
        let bar = gtk::Box::builder().spacing(8).margin_top(10).build();
        bar.append(&status);
        bar.append(&undo);
        bar.append(&apply);
        let links_title = gtk::Label::builder()
            .xalign(0.0)
            .margin_top(14)
            .margin_bottom(6)
            .build();
        links_title.add_css_class("heading");
        let links = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .build();
        links.add_css_class("boxed-list");
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&frame);
        content.append(&bar);
        content.append(&links_title);
        content.append(&links);
        self.imp().arrangement_group.add(&content);

        area.connect_changed(clone!(
            #[weak(rename_to = window)]
            self,
            move || window.arrangement_changed_here()
        ));
        area.connect_selected(clone!(
            #[weak(rename_to = window)]
            self,
            move |handle| window.open_connection(handle)
        ));
        undo.connect_clicked(clone!(
            #[weak]
            area,
            move |_| area.undo()
        ));
        apply.connect_clicked(clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.apply_arrangement()
        ));
        self.imp().arrangement.replace(Some(ArrangementParts {
            area,
            status,
            apply,
            undo,
            links,
            links_title,
        }));
        self.arrangement_changed_here();
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
        self.show_toast("Arrangement applied");
    }

    /// the arrangement went to `name`, or not
    pub(super) fn arrangement_sent(&self, name: &str, error: Option<&str>) {
        match error {
            None => self.show_toast(&format!("Arrangement sent to {name}")),
            Some(e) => self.show_toast(&format!("Arrangement not sent to {name}: {e}")),
        }
    }

    /// another computer changed the arrangement
    pub(super) fn arrangement_received(&self, by: &str) {
        self.show_toast(&format!("{by} changed the arrangement"));
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
            format!(
                "{} does not touch a computer this one reaches: move it next to one",
                unreachable.join(", ")
            )
        } else if parts.area.is_proposal() && dirty {
            "Made from the connections as they are now: arrange and apply".to_owned()
        } else if dirty {
            "Changes not applied yet".to_owned()
        } else if !guessed.is_empty() {
            format!(
                "{} did not tell its size: update RustInput there to 0.26.0 or newer",
                guessed.join(", ")
            )
        } else {
            "Applied on the computers of the arrangement".to_owned()
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
        let Some(selected) = selected else {
            return;
        };
        parts
            .links_title
            .set_text(&format!("Where the mouse leaves {selected}"));
        if links.is_empty() {
            let row = adw::ActionRow::builder()
                .title("Nowhere")
                .subtitle("It touches no other computer")
                .build();
            parts.links.append(&row);
        }
        for link in links {
            let side = match link.pos {
                Position::Left => "Left",
                Position::Right => "Right",
                Position::Top => "Top",
                Position::Bottom => "Bottom",
            };
            let subtitle = if link.whole && link.can_be_whole {
                "The whole edge of both, the place along it in proportion".to_owned()
            } else if link.can_be_whole {
                format!(
                    "{}, where they touch; switch on for the whole edge of both, in proportion",
                    link.part
                )
            } else {
                format!("{}, where they touch: the edge is shared", link.part)
            };
            let row = adw::ActionRow::builder()
                .title(format!("{side}: {}", link.other_name))
                .subtitle(subtitle)
                .build();
            let switch = gtk::Switch::builder()
                .valign(gtk::Align::Center)
                .active(link.whole && link.can_be_whole)
                .sensitive(link.can_be_whole)
                .tooltip_text("Whole edge, in proportion")
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

    /// opens a web page or a folder the way the system does
    pub(super) fn open_uri(&self, uri: &str) {
        gtk::show_uri(Some(self), uri, gtk::gdk::CURRENT_TIME);
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
        if let Some(parts) = self.imp().arrangement.borrow().as_ref() {
            parts.area.set_state(handle, state.active, state.alive);
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
            "Send copied files automatically",
            "Files copied in the file manager go along with the pointer. When this \
             is off, only files chosen with “Copy to RustInput” are sent",
        );
        let (menu_row, menu) = switch_row(
            "“Copy to RustInput” in the file manager",
            "In the menu of files and folders: Explorer, Nemo and Nautilus",
        );
        let (show_row, show) = switch_row(
            "Show the transfers window",
            "With the progress, when files are sent or received",
        );
        let open_row = adw::ActionRow::builder()
            .title("File transfers")
            .subtitle("Progress, history and log")
            .build();
        let open = gtk::Button::builder()
            .label("Open")
            .valign(gtk::Align::Center)
            .build();
        open.connect_clicked(move |_| open_transfers());
        open_row.add_suffix(&open);
        open_row.set_activatable_widget(Some(&open));
        let group = &self.imp().files_group;
        for row in [&send_row, &menu_row, &show_row, &open_row] {
            group.add(row);
        }
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
        self.request(FrontendRequest::SetFileOptions(FileOptions {
            send_copied: send.is_active(),
            file_manager_menu: menu.is_active(),
            show_window: show.is_active(),
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
                    "Sharing the mouse and keyboard",
                    "Capture and emulation are on",
                ),
                ("Input capture is off", "Needed for every connection"),
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
                    "Sending",
                    "Past a screen edge, the mouse and keyboard go to the next computer",
                ),
                ("Input capture is off", "Needed for every connection"),
            );
            show_status(
                &imp.emulation_status_row,
                &imp.emulation_status_icon,
                &imp.input_emulation_button,
                emulation,
                ("Receiving", "The other computers can control this one"),
                (
                    "Input emulation is off",
                    "Needed for the other computers to control this one",
                ),
            );
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
            row.set_title("relaunch required");
            row.set_subtitle("Accessibility granted — restart to activate capture and emulation");
            set_button_content_label(button, "Relaunch");
        } else {
            // AX missing → send the user to System Settings.
            row.set_title("input capture is disabled");
            row.set_subtitle("grant Accessibility permission to enable");
            set_button_content_label(button, "Grant");
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
        let name = hostname.unwrap_or_else(|| "the other computer".into());
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
            .label(format!(
                "When this computer tries to connect, {name} asks whether to authorize it and \
                 shows a code. Enter it here to authorize both computers on each other: for \
                 control, clipboard and files."
            ))
            .wrap(true)
            .max_width_chars(48)
            .build();
        let cancel = gtk::Button::with_label("Cancel");
        let authorize = gtk::Button::builder()
            .label("Authorize")
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
                    &format!("Authorize with {name}"),
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
                window.show_toast("Authorizing…");
                dialog.close();
            }
        ));
        dialog.present();
        entry.grab_focus();
    }

    /// the other device entered the code shown here
    pub(super) fn paired(&self, fingerprint: &str, name: &str) {
        let shown = self.imp().authorization_window.borrow().clone();
        if let Some(w) = shown.filter(|w| w.fingerprint() == fingerprint) {
            w.close();
        }
        self.show_toast(&format!("{name} and this computer authorized each other"));
    }

    pub(super) fn request_authorization(&self, fingerprint: &str, code: Option<&str>) {
        if let Some(w) = self.imp().authorization_window.borrow_mut().take() {
            w.close();
        }
        let window = AuthorizationWindow::new(fingerprint, code);
        window.set_transient_for(Some(self));
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

fn theme_index(theme: Theme) -> u32 {
    THEMES.iter().position(|t| *t == theme).unwrap_or(0) as u32
}

fn theme_at(index: u32) -> Theme {
    THEMES.get(index as usize).copied().unwrap_or_default()
}
