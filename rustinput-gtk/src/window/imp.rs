use std::cell::{Cell, RefCell};

use adw::subclass::prelude::*;
use adw::{ActionRow, PreferencesGroup, ToastOverlay, prelude::*};
use glib::subclass::InitializingObject;
use gtk::glib::clone;
use gtk::{Button, CompositeTemplate, Entry, Image, Label, ListBox, gdk, gio, glib};

use rustinput_ipc::{DEFAULT_PORT, FrontendRequestWriter};

use crate::authorization_window::AuthorizationWindow;

#[derive(CompositeTemplate, Default)]
#[template(resource = "/io/github/felipemarques/RustInput/window.ui")]
pub struct Window {
    #[template_child]
    pub authorized_placeholder: TemplateChild<ActionRow>,
    #[template_child]
    pub fingerprint_row: TemplateChild<ActionRow>,
    #[template_child]
    pub version_row: TemplateChild<ActionRow>,
    #[template_child]
    pub theme_row: TemplateChild<adw::ComboRow>,
    /// the theme row is being set to what the service has
    pub updating_theme: Cell<bool>,
    #[template_child]
    pub port_edit_apply: TemplateChild<Button>,
    #[template_child]
    pub port_edit_cancel: TemplateChild<Button>,
    #[template_child]
    pub client_list: TemplateChild<ListBox>,
    #[template_child]
    pub client_placeholder: TemplateChild<ActionRow>,
    #[template_child]
    pub port_entry: TemplateChild<Entry>,
    #[template_child]
    pub hostname_copy_icon: TemplateChild<Image>,
    #[template_child]
    pub hostname_label: TemplateChild<Label>,
    #[template_child]
    pub toast_overlay: TemplateChild<ToastOverlay>,
    #[template_child]
    pub capture_emulation_group: TemplateChild<PreferencesGroup>,
    #[template_child]
    pub capture_status_row: TemplateChild<ActionRow>,
    #[template_child]
    pub emulation_status_row: TemplateChild<ActionRow>,
    #[template_child]
    pub input_emulation_button: TemplateChild<Button>,
    #[template_child]
    pub input_capture_button: TemplateChild<Button>,
    #[template_child]
    pub authorized_list: TemplateChild<ListBox>,
    #[template_child]
    pub files_group: TemplateChild<PreferencesGroup>,
    /// the page of the computers, marked when capture or emulation is off
    #[template_child]
    pub computers_page: TemplateChild<adw::ViewStackPage>,
    #[template_child]
    pub capture_status_icon: TemplateChild<Image>,
    #[template_child]
    pub emulation_status_icon: TemplateChild<Image>,
    #[template_child]
    pub logs_row: TemplateChild<ActionRow>,
    /// the version next to the name, always in sight
    #[template_child]
    pub version_label: TemplateChild<Label>,
    /// the pages: computers, arrangement, preferences, about
    #[template_child]
    pub stack: TemplateChild<adw::ViewStack>,
    /// the page of the arrangement, filled in by `setup_arrangement`
    #[template_child]
    pub arrangement_box: TemplateChild<gtk::Box>,
    #[template_child]
    pub language_row: TemplateChild<adw::ComboRow>,
    /// pauses RustInput, pressed while paused
    #[template_child]
    pub pause_button: TemplateChild<gtk::ToggleButton>,
    /// the line under the header bar while paused
    #[template_child]
    pub pause_revealer: TemplateChild<gtk::Revealer>,
    /// RustInput is paused on this computer, as the service says
    pub paused: Cell<bool>,
    /// the line under the header bar about new versions
    #[template_child]
    pub update_revealer: TemplateChild<gtk::Revealer>,
    #[template_child]
    pub update_label: TemplateChild<Label>,
    #[template_child]
    pub update_no: TemplateChild<Button>,
    #[template_child]
    pub update_yes: TemplateChild<Button>,
    #[template_child]
    pub update_check_switch: TemplateChild<gtk::Switch>,
    #[template_child]
    pub update_status_row: TemplateChild<ActionRow>,
    #[template_child]
    pub update_check_button: TemplateChild<Button>,
    #[template_child]
    pub update_install_button: TemplateChild<Button>,
    /// what the service knows about new versions
    pub updates: RefCell<rustinput_ipc::UpdateState>,
    /// the switch is being set to what the service has
    pub updating_update_switch: Cell<bool>,
    /// the new version whose line was closed with "Later"
    pub update_dismissed: RefCell<Option<String>>,
    /// the editor of the arrangement and what goes with it
    pub arrangement: RefCell<Option<crate::window::ArrangementParts>>,
    /// the certificate fingerprint of this computer, as copied (the row shows
    /// it in groups)
    pub fingerprint: RefCell<String>,
    /// what happens to copied files, as the service has it
    pub file_options: Cell<Option<rustinput_ipc::FileOptions>>,
    /// the switches of the file transfer settings: sending copied files, the
    /// file manager menu, the transfers window
    pub file_switches: RefCell<Option<[gtk::Switch; 3]>>,
    /// the switches are being set to what the service has
    pub updating_files: Cell<bool>,
    pub clients: RefCell<Option<gio::ListStore>>,
    pub authorized: RefCell<Option<gio::ListStore>>,
    pub frontend_request_writer: RefCell<Option<FrontendRequestWriter>>,
    pub port: Cell<u16>,
    pub capture_active: Cell<bool>,
    pub emulation_active: Cell<bool>,
    pub authorization_window: RefCell<Option<AuthorizationWindow>>,
    /// asks about other computers, shown after the open one: it is never
    /// swapped under the user for another
    pub authorization_queue: RefCell<std::collections::VecDeque<AuthorizationWindow>>,
}

#[glib::object_subclass]
impl ObjectSubclass for Window {
    // `NAME` needs to match `class` attribute of template
    const NAME: &'static str = "RustInputWindow";
    const ABSTRACT: bool = false;

    type Type = super::Window;
    type ParentType = adw::ApplicationWindow;

    fn class_init(klass: &mut Self::Class) {
        klass.bind_template();
        klass.bind_template_callbacks();
    }

    fn instance_init(obj: &InitializingObject<Self>) {
        obj.init_template();
    }
}

#[gtk::template_callbacks]
impl Window {
    #[template_callback]
    fn handle_add_client_pressed(&self, _button: &Button) {
        self.obj().request_client_create();
    }

    #[template_callback]
    fn handle_copy_hostname(&self, _: &Button) {
        if let Ok(hostname) = hostname::get() {
            let display = gdk::Display::default().unwrap();
            let clipboard = display.clipboard();
            clipboard.set_text(hostname.to_str().expect("hostname: invalid utf8"));
            let icon = self.hostname_copy_icon.clone();
            icon.set_icon_name(Some("emblem-ok-symbolic"));
            icon.set_css_classes(&["success"]);
            glib::spawn_future_local(clone!(
                #[weak]
                icon,
                async move {
                    glib::timeout_future_seconds(1).await;
                    icon.set_icon_name(Some("edit-copy-symbolic"));
                    icon.set_css_classes(&[]);
                }
            ));
        }
    }

    #[template_callback]
    fn handle_copy_fingerprint(&self, button: &Button) {
        let fingerprint = self.fingerprint.borrow().clone();
        let display = gdk::Display::default().unwrap();
        let clipboard = display.clipboard();
        clipboard.set_text(&fingerprint);
        button.set_icon_name("emblem-ok-symbolic");
        button.set_css_classes(&["success"]);
        glib::spawn_future_local(clone!(
            #[weak]
            button,
            async move {
                glib::timeout_future_seconds(1).await;
                button.set_icon_name("edit-copy-symbolic");
                button.set_css_classes(&[]);
            }
        ));
    }

    #[template_callback]
    fn handle_port_changed(&self, _entry: &Entry) {
        self.port_edit_apply.set_visible(true);
        self.port_edit_cancel.set_visible(true);
    }

    #[template_callback]
    fn handle_port_edit_apply(&self) {
        self.obj().request_port_change();
    }

    #[template_callback]
    fn handle_port_edit_cancel(&self) {
        log::debug!("cancel port edit");
        self.port_entry
            .set_text(self.port.get().to_string().as_str());
        self.port_edit_apply.set_visible(false);
        self.port_edit_cancel.set_visible(false);
    }

    #[template_callback]
    fn handle_emulation(&self) {
        // On macOS the emulation_status_row is hidden — capture_status_row
        // acts as the shared warning (see update_capture_emulation_status).
        // This handler still fires for the non-macOS platforms where the
        // emulation row is distinct.
        self.obj().request_emulation();
    }

    #[template_callback]
    fn handle_capture(&self) {
        #[cfg(target_os = "macos")]
        {
            use crate::macos_privacy;
            if macos_privacy::accessibility_granted() {
                // AX granted but the row is still visible => the daemon
                // subprocess bailed before AX was in place and needs a
                // fresh process.
                log::info!("capture row clicked in relaunch-required state");
                let Some(app) = self.obj().application() else {
                    log::error!("failed to relaunch: window has no application");
                    self.obj()
                        .show_toast(crate::i18n::tr("Failed to relaunch RustInput"));
                    return;
                };
                match macos_privacy::relaunch_bundle() {
                    Ok(()) => app.quit(),
                    Err(error) => {
                        log::error!("failed to schedule app relaunch: {error}");
                        self.obj()
                            .show_toast(crate::i18n::tr("Failed to relaunch RustInput"));
                    }
                }
                return;
            }
            log::info!("capture row clicked in AX-missing state, opening pane");
            macos_privacy::open_accessibility_settings();
        }
        self.obj().request_capture();
    }

    #[template_callback]
    fn handle_pause_toggled(&self) {
        let pressed = self.pause_button.is_active();
        // also when the service said so: then nothing changes
        if pressed != self.paused.get() {
            self.obj().request_pause(pressed);
        }
    }

    #[template_callback]
    fn handle_resume(&self) {
        self.obj().request_pause(false);
    }

    #[template_callback]
    fn handle_update_no(&self) {
        self.obj().update_answered(false);
    }

    #[template_callback]
    fn handle_update_yes(&self) {
        self.obj().update_answered(true);
    }

    #[template_callback]
    fn handle_check_updates(&self) {
        self.obj()
            .request(rustinput_ipc::FrontendRequest::CheckUpdates);
    }

    #[template_callback]
    fn handle_install_update(&self) {
        self.obj().install_update();
    }

    #[template_callback]
    fn handle_add_cert_fingerprint(&self, _button: &Button) {
        self.obj().open_fingerprint_dialog(None);
    }

    #[template_callback]
    fn handle_open_source(&self) {
        self.obj()
            .open_uri("https://github.com/felipemarques/rust-input");
    }

    #[template_callback]
    fn handle_open_lan_mouse(&self) {
        self.obj().open_uri("https://github.com/feschber/lan-mouse");
    }

    #[template_callback]
    fn handle_open_license(&self) {
        self.obj()
            .open_uri("https://github.com/felipemarques/rust-input/blob/main/LICENSE");
    }

    #[template_callback]
    fn handle_open_logs(&self) {
        if let Some(dir) = rustinput_ipc::log_dir() {
            self.obj().open_uri(&gio::File::for_path(dir).uri());
        }
    }

    pub fn set_port(&self, port: u16) {
        self.port.set(port);
        if port == DEFAULT_PORT {
            self.port_entry.set_text("");
        } else {
            self.port_entry.set_text(format!("{port}").as_str());
        }
        self.port_edit_apply.set_visible(false);
        self.port_edit_cancel.set_visible(false);
    }
}

impl ObjectImpl for Window {
    fn constructed(&self) {
        if let Ok(hostname) = hostname::get() {
            self.hostname_label
                .set_text(hostname.to_str().expect("hostname: invalid utf8"));
        }
        self.parent_constructed();
        self.set_port(DEFAULT_PORT);
        let obj = self.obj();
        obj.setup_icon();
        obj.setup_clients();
        obj.setup_authorized();
        obj.setup_arrangement();
        obj.setup_language();
        obj.setup_updates();
    }
}

impl WidgetImpl for Window {}
impl WindowImpl for Window {}
impl ApplicationWindowImpl for Window {}
impl AdwApplicationWindowImpl for Window {}
