mod arrangement;
mod authorization_window;
mod client_object;
mod client_row;
mod fingerprint_window;
mod i18n;
mod key_object;
mod key_row;
mod link_stats;
#[cfg(target_os = "macos")]
mod macos_privacy;
#[cfg(target_os = "macos")]
mod macos_status_item;
mod notification;
mod transfers;
#[cfg(not(target_os = "macos"))]
mod tray;
mod window;

use std::{env, process, str, sync::OnceLock};

use gtk::CssProvider;
pub use i18n::restart_requested;
use i18n::{tr, trf};
use window::Window;

/// Local build's commit hash, set once by [`run`] before the GTK
/// main loop starts. Read by per-row UI to compare against each
/// peer's [`rustinput_ipc::ClientState::peer_commit`] for the
/// soft-warn version-mismatch indicator.
pub(crate) static LOCAL_COMMIT: OnceLock<[u8; 8]> = OnceLock::new();

/// The theme in the configuration when the frontend started, set once by
/// [`run`]: applied before the window shows.
static THEME: OnceLock<Theme> = OnceLock::new();

/// Light or dark windows: `Theme::System` follows the preference of the
/// system, the others force it. It applies to every window of the
/// application.
pub(crate) fn apply_theme(theme: Theme) {
    let scheme = match theme {
        Theme::System => adw::ColorScheme::Default,
        Theme::Light => adw::ColorScheme::ForceLight,
        Theme::Dark => adw::ColorScheme::ForceDark,
    };
    adw::StyleManager::default().set_color_scheme(scheme);
}

/// Version of the application, set once by [`run`]. The frontend is a
/// crate of its own, so its `CARGO_PKG_VERSION` is not the one users see.
pub(crate) static APP_VERSION: OnceLock<&'static str> = OnceLock::new();

/// the version shown to the user, with the commit it was built from
pub(crate) fn version_str() -> String {
    format!(
        "{} ({})",
        APP_VERSION.get().copied().unwrap_or("unknown"),
        local_commit_str()
    )
}

/// Convenience: returns the local commit as an 8-char ASCII string,
/// or a placeholder if unset (which would indicate a programmer
/// error since [`run`] always sets it).
pub(crate) fn local_commit_str() -> String {
    LOCAL_COMMIT
        .get()
        .and_then(|c| std::str::from_utf8(c).ok())
        .unwrap_or("????????")
        .to_string()
}

use rustinput_ipc::{FrontendEvent, FrontendRequest, Theme, TransferState};

use adw::Application;
use gtk::{IconTheme, gdk::Display, glib::clone, prelude::*};
use gtk::{gio, glib, prelude::ApplicationExt};

use self::client_object::ClientObject;
use self::key_object::KeyObject;

use thiserror::Error;

#[derive(Error, Debug)]
pub enum GtkError {
    #[error("gtk frontend exited with non zero exit code: {0}")]
    NonZeroExitCode(i32),
}

/// Runs the window, the tray icon and what goes with them until RustInput
/// quits. `language` is the one configured ("pt-BR"), `None` for the one of
/// the system.
pub fn run(
    version: &'static str,
    local_commit: [u8; 8],
    theme: Theme,
    language: Option<String>,
) -> Result<(), GtkError> {
    log::debug!("running gtk frontend");
    APP_VERSION.set(version).expect("version set once");
    THEME.set(theme).expect("theme set once");
    i18n::start(language.as_deref());
    LOCAL_COMMIT
        .set(local_commit)
        .expect("local_commit set once");

    #[cfg(windows)]
    let ret = std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024) // https://gitlab.gnome.org/GNOME/gtk/-/commit/52dbb3f372b2c3ea339e879689c1de535ba2c2c3 -> caused crash on windows
        .name("gtk".into())
        .spawn(gtk_main)
        .unwrap()
        .join()
        .unwrap();
    #[cfg(not(windows))]
    let ret = gtk_main();

    match ret {
        glib::ExitCode::SUCCESS => Ok(()),
        e => Err(GtkError::NonZeroExitCode(e.value())),
    }
}

fn gtk_main() -> glib::ExitCode {
    #[cfg(target_os = "macos")]
    {
        configure_macos_bundle_environment();
        install_macos_gtk_log_filter();
    }

    gio::resources_register_include!("rustinput.gresource").expect("Failed to register resources.");
    // the windows in the language of the texts
    i18n::register_windows();

    // the name notifications show
    glib::set_application_name("RustInput");
    let app = Application::builder()
        .application_id("io.github.felipemarques.RustInput")
        .build();

    app.connect_startup(|app| {
        // before any window shows, so it does not flash in the other theme
        apply_theme(THEME.get().copied().unwrap_or_default());
        // right to left in Arabic, left to right in the others, whatever
        // the language of the system
        gtk::Widget::set_default_direction(i18n::direction());
        load_css();
        load_icons();
        setup_actions(app);
        setup_menu(app);
    });
    app.connect_activate(build_ui);

    let args: Vec<&'static str> = vec![];
    app.run_with_args(&args)
}

#[cfg(target_os = "macos")]
fn install_macos_gtk_log_filter() {
    glib::log_set_writer_func(|level, fields| {
        if level == glib::LogLevel::Warning && is_gtk_theme_parser_warning(fields) {
            return glib::LogWriterOutput::Handled;
        }

        glib::log_writer_default(level, fields)
    });
}

#[cfg(target_os = "macos")]
fn is_gtk_theme_parser_warning(fields: &[glib::LogField<'_>]) -> bool {
    let mut domain = None;
    let mut message = None;

    for field in fields {
        match field.key() {
            "GLIB_DOMAIN" => domain = field.value_str(),
            "MESSAGE" => message = field.value_str(),
            _ => {}
        }
    }

    domain == Some("Gtk")
        && message.is_some_and(|message| message.starts_with("Theme parser warning: gtk.css:"))
}

#[cfg(target_os = "macos")]
fn configure_macos_bundle_environment() {
    let Ok(exe) = env::current_exe() else {
        return;
    };
    let Some(contents) = exe
        .parent()
        .and_then(|dir| dir.parent())
        .map(std::path::Path::to_owned)
    else {
        return;
    };

    let share = contents.join("Resources").join("share");
    if !share.exists() {
        return;
    }

    let schemas = share.join("glib-2.0").join("schemas");
    if schemas.exists() {
        env::set_var("GSETTINGS_SCHEMA_DIR", schemas);
    }

    env::set_var("XDG_DATA_DIRS", &share);
    env::set_var(
        "GTK_DATA_PREFIX",
        contents.join("Resources").to_string_lossy().as_ref(),
    );
}

fn load_css() {
    let provider = CssProvider::default();
    provider.load_from_resource("io/github/felipemarques/RustInput/style.css");
    gtk::style_context_add_provider_for_display(
        &Display::default().expect("Could not connect to a display"),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

fn load_icons() {
    let display = &Display::default().expect("Could not connect to a display.");
    let icon_theme = IconTheme::for_display(display);
    icon_theme.add_resource_path("/io/github/felipemarques/RustInput/icons");
}

// Add application actions
fn setup_actions(app: &adw::Application) {
    // Quit action
    // This is important on macOS, where users expect a File->Quit action with a Cmd+Q shortcut.
    let quit_action = gio::SimpleAction::new("quit", None);
    quit_action.connect_activate({
        let app = app.clone();
        move |_, _| {
            app.quit();
        }
    });
    app.add_action(&quit_action);

    // start again, in the language just picked
    let restart_action = gio::SimpleAction::new("restart", None);
    restart_action.connect_activate({
        let app = app.clone();
        move |_, _| {
            i18n::request_restart();
            app.quit();
        }
    });
    app.add_action(&restart_action);
}

// Set up a global menu
//
// Currently this is used only on macOS
fn setup_menu(app: &adw::Application) {
    let menu = gio::Menu::new();

    let file_menu = gio::Menu::new();
    file_menu.append(Some(tr("Quit")), Some("app.quit"));
    menu.append_submenu(Some(tr("_File")), &file_menu);

    app.set_menubar(Some(&menu))
}

/// The daemon the Windows service starts in this session, with administrator
/// rights: when the service runs, waits for it (it comes up within seconds
/// of logging on), `None` if there is no service or it did not come up.
#[cfg(windows)]
fn daemon_of_the_windows_service() -> Option<(
    rustinput_ipc::FrontendEventReader,
    rustinput_ipc::FrontendRequestWriter,
)> {
    use std::{
        thread,
        time::{Duration, Instant},
    };
    if !rustinput_ipc::windows_service_runs() {
        return None;
    }
    log::info!("waiting for the daemon the RustInput service starts");
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Ok(conn) = rustinput_ipc::try_connect() {
            return Some(conn);
        }
        thread::sleep(Duration::from_millis(250));
    }
    log::warn!(
        "the RustInput service did not start the daemon (see {}\\Logs\\RustInput\\service.log), \
         starting one without administrator rights",
        env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_owned())
    );
    None
}

fn build_ui(app: &Application) {
    // If a window already exists (re-activation), just present it
    if let Some(window) = app.windows().first() {
        window.present();
        return;
    }

    log::debug!("connecting to rustinput-socket");
    let connection = rustinput_ipc::try_connect();
    #[cfg(windows)]
    let connection = connection.or_else(|e| daemon_of_the_windows_service().ok_or(e));
    let (mut frontend_rx, frontend_tx) = match connection {
        Ok(conn) => conn,
        Err(e) => {
            log::warn!("could not connect to daemon ({e}), spawning a new one");
            if let Err(spawn_err) = process::Command::new(
                env::current_exe().expect("could not determine executable path"),
            )
            .args(env::args().skip(1))
            .arg("daemon")
            .spawn()
            {
                log::error!("failed to spawn daemon: {spawn_err}");
                process::exit(1);
            }
            match rustinput_ipc::connect() {
                Ok(conn) => conn,
                Err(e) => {
                    log::error!("{e}");
                    process::exit(1);
                }
            }
        }
    };
    log::debug!("connected to rustinput-socket");

    let (sender, receiver) = async_channel::bounded(10);

    gio::spawn_blocking(move || {
        while let Some(e) = frontend_rx.next_event() {
            match e {
                Ok(e) => sender.send_blocking(e).unwrap(),
                Err(e) => {
                    log::error!("{e}");
                    break;
                }
            }
        }
    });

    let window = Window::new(app, frontend_tx);
    #[cfg(target_os = "macos")]
    {
        window.connect_close_request(|window| {
            window.set_visible(false);
            glib::Propagation::Stop
        });
        macos_status_item::setup(app, &window);
        // First-launch TCC prompts. No-op when already granted.
        macos_privacy::fire_initial_prompts();
        // Watch the Accessibility grant continuously for the lifetime
        // of the process. On a grant, swap the warning row into its
        // "relaunch required" state (the daemon subprocess already
        // bailed and can't recover without a restart). On a REVOKE,
        // quit immediately — an active CGEventTap at
        // HeadInsertEventTap can wedge system input if the process
        // lingers after losing AX, and forcing the process to exit is
        // the only bulletproof way to guarantee the kernel tears the
        // tap down.
        let window_weak = window.downgrade();
        let app_weak = app.downgrade();
        macos_privacy::watch_accessibility_state(move |change| match change {
            macos_privacy::AccessibilityChange::Granted => {
                if let Some(window) = window_weak.upgrade() {
                    window.present();
                    window.refresh_capture_emulation_status();
                }
            }
            macos_privacy::AccessibilityChange::Revoked => {
                log::warn!("Accessibility revoked — quitting to avoid wedging system input");
                if let Some(app) = app_weak.upgrade() {
                    app.quit();
                }
            }
        });
    }

    // what the notification about a new version does when clicked
    let show_window = gio::SimpleAction::new(notification::SHOW_WINDOW, None);
    show_window.connect_activate(clone!(
        #[weak]
        window,
        move |_, _| window.present()
    ));
    app.add_action(&show_window);
    let install_update = gio::SimpleAction::new(notification::INSTALL_UPDATE, None);
    install_update.connect_activate(clone!(
        #[weak]
        window,
        move |_, _| window.install_update()
    ));
    app.add_action(&install_update);

    window.setup_theme(THEME.get().copied().unwrap_or_default());
    let transfers = transfers::Transfers::new(&window);
    window.setup_file_settings(clone!(
        #[strong]
        transfers,
        move || transfers.present()
    ));

    // With a tray icon (the menu bar item on macOS) closing the window
    // only hides it, see `tray`.
    #[cfg(not(target_os = "macos"))]
    let tray = tray::setup(app, &window, &transfers);
    #[cfg(not(target_os = "macos"))]
    let in_tray = tray.is_some();
    #[cfg(target_os = "macos")]
    let in_tray = true;

    let transfers_window = transfers;
    glib::spawn_future_local(clone!(
        #[weak]
        window,
        async move {
            // received transfers already announced in the window
            let mut announced = std::collections::HashSet::new();
            // the new version told about with a notification of the system
            let mut told: Option<String> = None;
            // the arrangement, with the monitors of the other computers
            window.request(FrontendRequest::Arrangement);
            loop {
                let notify = receiver.recv().await.unwrap_or_else(|_| process::exit(1));
                match notify {
                    FrontendEvent::Created(handle, client, state) => {
                        window.new_client(handle, client, state);
                        window.request(FrontendRequest::Arrangement);
                    }
                    FrontendEvent::Deleted(client) => {
                        window.delete_client(client);
                        window.request(FrontendRequest::Arrangement);
                    }
                    FrontendEvent::Arrangement(view) => window.show_arrangement(&view),
                    FrontendEvent::ArrangementSent { name, error } => {
                        window.arrangement_sent(&name, error.as_deref())
                    }
                    FrontendEvent::ArrangementChanged { by } => window.arrangement_received(&by),
                    FrontendEvent::ScreenNotLocked { name, error } => {
                        window.screen_not_locked(&name, &error)
                    }
                    FrontendEvent::MonitorsNotIdentified { name, error } => {
                        window.monitors_not_identified(&name, &error)
                    }
                    FrontendEvent::PointerAt(handle) => window.set_pointer_at(handle),
                    FrontendEvent::State(handle, config, state) => {
                        window.update_client_config(handle, config);
                        window.update_client_state(handle, state);
                    }
                    FrontendEvent::NoSuchClient(_) => {}
                    FrontendEvent::Error(e) => window.show_toast(e.as_str()),
                    FrontendEvent::Enumerate(clients) => window.update_client_list(clients),
                    FrontendEvent::PortChanged(port, msg) => window.update_port(port, msg),
                    FrontendEvent::CaptureStatus(s) => window.set_capture(s.into()),
                    FrontendEvent::EmulationStatus(s) => window.set_emulation(s.into()),
                    FrontendEvent::AuthorizedUpdated(keys) => window.set_authorized_keys(keys),
                    FrontendEvent::PublicKeyFingerprint(fp) => window.set_pk_fp(&fp),
                    FrontendEvent::ServerNotAuthorized {
                        handle: _,
                        name,
                        fingerprint,
                    } => window.server_not_authorized(&name, &fingerprint),
                    FrontendEvent::ConnectionAttempt { fingerprint, code } => {
                        window.request_authorization(&fingerprint, code.as_deref());
                    }
                    FrontendEvent::Paired { fingerprint, name } => {
                        window.paired(&fingerprint, &name);
                    }
                    FrontendEvent::PairingResult { handle: _, result } => match result {
                        Ok(name) => window.show_toast(&trf(
                            "{name} and this computer authorized each other",
                            &[("name", &name)],
                        )),
                        Err(e) => {
                            window.show_toast(&trf("Not authorized: {error}", &[("error", &e)]))
                        }
                    },
                    FrontendEvent::DeviceConnected {
                        fingerprint: _,
                        addr,
                    } => {
                        window.show_toast(&trf("{address} connected", &[("address", &addr)]));
                    }
                    FrontendEvent::DeviceEntered {
                        fingerprint: _,
                        addr,
                        pos: _,
                    } => {
                        window.show_toast(&trf(
                            "The mouse came from {address}",
                            &[("address", &addr)],
                        ));
                    }
                    FrontendEvent::IncomingDisconnected(addr) => {
                        window.show_toast(&trf("{address} disconnected", &[("address", &addr)]));
                    }
                    FrontendEvent::Notice(notice) => window.show_toast(&notice),
                    FrontendEvent::LinkStats(stats) => window.update_link_stats(&stats),
                    FrontendEvent::Theme(theme) => window.set_theme(theme),
                    FrontendEvent::Updates(state) => {
                        #[cfg(not(target_os = "macos"))]
                        if let Some(tray) = &tray {
                            tray.show_update(
                                state.available.then(|| state.latest.clone()).flatten(),
                            );
                        }
                        if let Some(version) = notification::to_tell(&state, told.as_deref()) {
                            // the window in front shows it already
                            if !(window.is_visible() && window.is_active()) {
                                #[cfg(windows)]
                                if let Some(tray) = &tray {
                                    tray.notify(
                                        &notification::title(&version),
                                        &notification::text(),
                                    );
                                }
                                #[cfg(not(windows))]
                                if let Some(app) = window.application() {
                                    notification::send(&app, &version);
                                }
                            }
                            window.request(FrontendRequest::UpdateNotified(version.clone()));
                            told = Some(version);
                        }
                        window.set_updates(state);
                    }
                    FrontendEvent::Paused(paused) => {
                        #[cfg(not(target_os = "macos"))]
                        if let Some(tray) = &tray {
                            tray.show_paused(paused);
                        }
                        #[cfg(target_os = "macos")]
                        crate::macos_status_item::show_paused(paused);
                        window.set_paused(paused);
                    }
                    FrontendEvent::ShowWindow => window.present(),
                    FrontendEvent::ShowTransfers => transfers_window.present(),
                    FrontendEvent::FileOptions(options) => {
                        window.set_file_options(options);
                        transfers_window.set_options(options);
                        #[cfg(not(target_os = "macos"))]
                        if let Some(tray) = &tray {
                            tray.show_options(options);
                        }
                    }
                    FrontendEvent::Transfers(transfers) => {
                        transfers_window.update(&transfers);
                        #[cfg(not(target_os = "macos"))]
                        if let Some(tray) = &tray {
                            tray.show_transfers(&transfers);
                        }
                        for t in &transfers {
                            if !t.sending
                                && t.state == TransferState::Done
                                && announced.insert(t.id)
                            {
                                window.show_toast(&trf(
                                    "Files received from {name}: {count}, ready to paste",
                                    &[("name", &t.peer), ("count", &t.files)],
                                ));
                            }
                        }
                    }
                }
            }
        }
    ));

    // Present the main window on every launch so the user gets a visible
    // confirmation that the app is running — including the post-grant
    // relaunch on macOS and normal launches from a menu or a file manager.
    // Opt out by setting `RUSTINPUT_HIDDEN=1` in the environment (useful
    // for starting with the session, coming up quietly into the tray or
    // menu bar only). Without a tray the window is always shown, as there
    // would be no other way to reach it.
    if !in_tray || env::var_os("RUSTINPUT_HIDDEN").is_none() {
        window.present();
    }
}
