mod authorization_window;
mod client_object;
mod client_row;
mod fingerprint_window;
mod key_object;
mod key_row;
#[cfg(target_os = "macos")]
mod macos_privacy;
#[cfg(target_os = "macos")]
mod macos_status_item;
mod transfers;
#[cfg(not(target_os = "macos"))]
mod tray;
mod window;

use std::{env, process, str, sync::OnceLock};

use gtk::CssProvider;
use window::Window;

/// Local build's commit hash, set once by [`run`] before the GTK
/// main loop starts. Read by per-row UI to compare against each
/// peer's [`rustinput_ipc::ClientState::peer_commit`] for the
/// soft-warn version-mismatch indicator.
pub(crate) static LOCAL_COMMIT: OnceLock<[u8; 8]> = OnceLock::new();

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

use rustinput_ipc::{FrontendEvent, TransferState};

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

pub fn run(version: &'static str, local_commit: [u8; 8]) -> Result<(), GtkError> {
    log::debug!("running gtk frontend");
    APP_VERSION.set(version).expect("version set once");
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

    let app = Application::builder()
        .application_id("io.github.felipemarques.RustInput")
        .build();

    app.connect_startup(|app| {
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
}

// Set up a global menu
//
// Currently this is used only on macOS
fn setup_menu(app: &adw::Application) {
    let menu = gio::Menu::new();

    let file_menu = gio::Menu::new();
    file_menu.append(Some("Quit"), Some("app.quit"));
    menu.append_submenu(Some("_File"), &file_menu);

    app.set_menubar(Some(&menu))
}

fn build_ui(app: &Application) {
    // If a window already exists (re-activation), just present it
    if let Some(window) = app.windows().first() {
        window.present();
        return;
    }

    log::debug!("connecting to rustinput-socket");
    let (mut frontend_rx, frontend_tx) = match rustinput_ipc::try_connect() {
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
            loop {
                let notify = receiver.recv().await.unwrap_or_else(|_| process::exit(1));
                match notify {
                    FrontendEvent::Created(handle, client, state) => {
                        window.new_client(handle, client, state)
                    }
                    FrontendEvent::Deleted(client) => window.delete_client(client),
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
                    FrontendEvent::ConnectionAttempt { fingerprint, code } => {
                        window.request_authorization(&fingerprint, code.as_deref());
                    }
                    FrontendEvent::Paired { fingerprint, name } => {
                        window.paired(&fingerprint, &name);
                    }
                    FrontendEvent::PairingResult { handle: _, result } => match result {
                        Ok(name) => window
                            .show_toast(&format!("{name} and this computer authorized each other")),
                        Err(e) => window.show_toast(&format!("Not authorized: {e}")),
                    },
                    FrontendEvent::DeviceConnected {
                        fingerprint: _,
                        addr,
                    } => {
                        window.show_toast(format!("device connected: {addr}").as_str());
                    }
                    FrontendEvent::DeviceEntered {
                        fingerprint: _,
                        addr,
                        pos,
                    } => {
                        window.show_toast(format!("device entered: {addr} ({pos})").as_str());
                    }
                    FrontendEvent::IncomingDisconnected(addr) => {
                        window.show_toast(format!("{addr} disconnected").as_str());
                    }
                    FrontendEvent::Notice(notice) => window.show_toast(&notice),
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
                                let files = if t.files == 1 {
                                    "1 file".to_owned()
                                } else {
                                    format!("{} files", t.files)
                                };
                                window.show_toast(&format!(
                                    "{files} received from {}, ready to paste",
                                    t.peer
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
