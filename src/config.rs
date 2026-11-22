use crate::capture_test::TestCaptureArgs;
use crate::emulation_test::TestEmulationArgs;
use clap::{Parser, Subcommand, ValueEnum};
use notify::event::ModifyKind;
use notify::{EventKind, RecommendedWatcher, Watcher};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env::{self, VarError};
use std::fmt::Display;
use std::fs::{self, File};
use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::{collections::HashSet, io};
use thiserror::Error;
use toml;
use toml_edit::{self, DocumentMut};

use rustinput_cli::CliArgs;
use rustinput_ipc::{DEFAULT_PORT, EdgeLink, FileOptions, Position, Span, Theme, layout::Layout};

use input_event::scancode::{
    self,
    Linux::{KeyLeftAlt, KeyLeftCtrl, KeyLeftMeta, KeyLeftShift},
};

use shadow_rs::shadow;

shadow!(build);

/// Local build's 8-byte ASCII short commit hash, suitable for use
/// in [`rustinput_proto::ProtoEvent::Hello`]. Pads with `'?'` if
/// shadow_rs returns an unexpected length so the field is always
/// well-formed on the wire.
pub fn local_commit() -> [u8; 8] {
    let bytes = build::SHORT_COMMIT.as_bytes();
    let mut out = [b'?'; 8];
    let n = bytes.len().min(8);
    out[..n].copy_from_slice(&bytes[..n]);
    out
}

const CONFIG_FILE_NAME: &str = "config.toml";
const CERT_FILE_NAME: &str = "rustinput.pem";

fn default_path() -> Result<PathBuf, VarError> {
    #[cfg(unix)]
    let default_path = {
        let xdg_config_home =
            env::var("XDG_CONFIG_HOME").unwrap_or(format!("{}/.config", env::var("HOME")?));
        format!("{xdg_config_home}/rustinput/")
    };

    #[cfg(not(unix))]
    let default_path = {
        let app_data =
            env::var("LOCALAPPDATA").unwrap_or(format!("{}/.config", env::var("USERPROFILE")?));
        format!("{app_data}\\rustinput\\")
    };
    Ok(PathBuf::from(default_path))
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
struct ConfigToml {
    capture_backend: Option<CaptureBackend>,
    emulation_backend: Option<EmulationBackend>,
    port: Option<u16>,
    release_bind: Option<Vec<scancode::Linux>>,
    cert_path: Option<PathBuf>,
    clipboard: Option<bool>,
    clipboard_files: Option<bool>,
    send_copied_files: Option<bool>,
    show_transfers_window: Option<bool>,
    file_manager_menu: Option<bool>,
    radial_menu: Option<bool>,
    /// "system", "light" or "dark": a string, so a value this version does
    /// not know does not make the whole file unreadable
    theme: Option<String>,
    /// the language of the windows, menus and the radial menu ("pt-BR"),
    /// that of the system when missing
    language: Option<String>,
    /// whether RustInput asks GitHub for its latest version; not asked yet
    /// when missing (the window asks)
    check_updates: Option<bool>,
    /// RustInput is paused on this computer: the mouse and keyboard stay
    /// here and no other computer controls it (from the tray icon, the
    /// window or the radial menu)
    paused: Option<bool>,
    clients: Option<Vec<TomlClient>>,
    authorized_fingerprints: Option<HashMap<String, String>>,
    /// the arrangement of the computers, the same on all of them (see
    /// `rustinput_ipc::layout`)
    layout: Option<Layout>,
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
struct TomlClient {
    hostname: Option<String>,
    host_name: Option<String>,
    ips: Option<Vec<IpAddr>>,
    port: Option<u16>,
    position: Option<Position>,
    /// the part of the edge at `position` that leads to the client, from 0.0
    /// (top or left end) to 1.0, the whole edge when missing
    edge: Option<[f64; 2]>,
    /// the part of the edge of the client it leads to, the whole edge when
    /// missing
    peer_edge: Option<[f64; 2]>,
    activate_on_startup: Option<bool>,
    enter_hook: Option<String>,
    leave_hook: Option<String>,
}

impl ConfigToml {
    fn new(path: &Path) -> Result<ConfigToml, ConfigError> {
        let config = fs::read_to_string(path)?;
        Ok(toml::from_str::<_>(&config)?)
    }
}

#[derive(Parser, Debug)]
#[command(author, version=build::CLAP_LONG_VERSION, about, long_about = None)]
struct Args {
    /// the listen port for rustinput
    #[arg(short, long)]
    port: Option<u16>,

    /// non-default config file location
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// capture backend override
    #[arg(long)]
    capture_backend: Option<CaptureBackend>,

    /// emulation backend override
    #[arg(long)]
    emulation_backend: Option<EmulationBackend>,

    /// path to non-default certificate location
    #[arg(long)]
    cert_path: Option<PathBuf>,

    /// subcommands
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// test input emulation
    TestEmulation(TestEmulationArgs),
    /// test input capture
    TestCapture(TestCaptureArgs),
    /// RustInput commandline interface
    Cli(CliArgs),
    /// send files and folders to the connected devices, where they go to the
    /// clipboard ready to be pasted (the "Copy to RustInput" menu item)
    SendFiles {
        /// files and folders to send
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },
    /// run in daemon mode
    Daemon,
    /// run as the Windows service "RustInput", which Windows starts: it keeps
    /// the daemon running in the session of the user at the screen, with the
    /// administrator rights of that user (installed by the installer or by
    /// update-rustinput.ps1)
    #[cfg(windows)]
    WindowsService,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
pub enum CaptureBackend {
    #[cfg(libei_capture)]
    #[serde(rename = "input-capture-portal")]
    InputCapturePortal,
    #[cfg(layer_shell_capture)]
    #[serde(rename = "layer-shell")]
    LayerShell,
    #[cfg(x11_capture)]
    #[serde(rename = "x11")]
    X11,
    #[cfg(windows)]
    #[serde(rename = "windows")]
    Windows,
    #[cfg(target_os = "macos")]
    #[serde(rename = "macos")]
    MacOs,
    #[serde(rename = "dummy")]
    Dummy,
}

impl Display for CaptureBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(libei_capture)]
            CaptureBackend::InputCapturePortal => write!(f, "input-capture-portal"),
            #[cfg(layer_shell_capture)]
            CaptureBackend::LayerShell => write!(f, "layer-shell"),
            #[cfg(x11_capture)]
            CaptureBackend::X11 => write!(f, "X11"),
            #[cfg(windows)]
            CaptureBackend::Windows => write!(f, "windows"),
            #[cfg(target_os = "macos")]
            CaptureBackend::MacOs => write!(f, "MacOS"),
            CaptureBackend::Dummy => write!(f, "dummy"),
        }
    }
}

impl From<CaptureBackend> for input_capture::Backend {
    fn from(backend: CaptureBackend) -> Self {
        match backend {
            #[cfg(libei_capture)]
            CaptureBackend::InputCapturePortal => Self::InputCapturePortal,
            #[cfg(layer_shell_capture)]
            CaptureBackend::LayerShell => Self::LayerShell,
            #[cfg(x11_capture)]
            CaptureBackend::X11 => Self::X11,
            #[cfg(windows)]
            CaptureBackend::Windows => Self::Windows,
            #[cfg(target_os = "macos")]
            CaptureBackend::MacOs => Self::MacOs,
            CaptureBackend::Dummy => Self::Dummy,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
pub enum EmulationBackend {
    #[cfg(wlroots_emulation)]
    #[serde(rename = "wlroots")]
    Wlroots,
    #[cfg(libei_emulation)]
    #[serde(rename = "libei")]
    Libei,
    #[cfg(rdp_emulation)]
    #[serde(rename = "xdp")]
    Xdp,
    #[cfg(x11_emulation)]
    #[serde(rename = "x11")]
    X11,
    #[cfg(windows)]
    #[serde(rename = "windows")]
    Windows,
    #[cfg(target_os = "macos")]
    #[serde(rename = "macos")]
    MacOs,
    #[serde(rename = "dummy")]
    Dummy,
}

impl From<EmulationBackend> for input_emulation::Backend {
    fn from(backend: EmulationBackend) -> Self {
        match backend {
            #[cfg(wlroots_emulation)]
            EmulationBackend::Wlroots => Self::Wlroots,
            #[cfg(libei_emulation)]
            EmulationBackend::Libei => Self::Libei,
            #[cfg(rdp_emulation)]
            EmulationBackend::Xdp => Self::Xdp,
            #[cfg(x11_emulation)]
            EmulationBackend::X11 => Self::X11,
            #[cfg(windows)]
            EmulationBackend::Windows => Self::Windows,
            #[cfg(target_os = "macos")]
            EmulationBackend::MacOs => Self::MacOs,
            EmulationBackend::Dummy => Self::Dummy,
        }
    }
}

impl Display for EmulationBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(wlroots_emulation)]
            EmulationBackend::Wlroots => write!(f, "wlroots"),
            #[cfg(libei_emulation)]
            EmulationBackend::Libei => write!(f, "libei"),
            #[cfg(rdp_emulation)]
            EmulationBackend::Xdp => write!(f, "xdg-desktop-portal"),
            #[cfg(x11_emulation)]
            EmulationBackend::X11 => write!(f, "X11"),
            #[cfg(windows)]
            EmulationBackend::Windows => write!(f, "windows"),
            #[cfg(target_os = "macos")]
            EmulationBackend::MacOs => write!(f, "macos"),
            EmulationBackend::Dummy => write!(f, "dummy"),
        }
    }
}

#[derive(Debug)]
pub struct Config {
    /// command line arguments
    args: Args,
    /// path to the certificate file used
    cert_path: PathBuf,
    /// path to the config file used
    config_path: PathBuf,
    /// path to config directory (parent of above)
    config_dir: PathBuf,
    /// the (optional) toml config and it's path
    config_toml: Option<ConfigToml>,
    /// notices changes made to the file by hand; none when the system had
    /// no watcher to give (the limit of inotify instances reached, say):
    /// RustInput runs on and reads such changes when it starts again
    watcher: Option<notify::RecommendedWatcher>,
    // channel for filesystem events
    watch_rx: tokio::sync::mpsc::Receiver<Result<notify::Event, notify::Error>>,
}

pub struct ConfigClient {
    pub ips: HashSet<IpAddr>,
    pub hostname: Option<String>,
    pub port: u16,
    pub pos: Position,
    pub link: EdgeLink,
    pub active: bool,
    pub enter_hook: Option<String>,
    pub leave_hook: Option<String>,
}

impl From<TomlClient> for ConfigClient {
    fn from(toml: TomlClient) -> Self {
        let active = toml.activate_on_startup.unwrap_or(false);
        let enter_hook = toml.enter_hook;
        let leave_hook = toml.leave_hook;
        let hostname = toml.hostname;
        let ips = HashSet::from_iter(toml.ips.into_iter().flatten());
        let port = toml.port.unwrap_or(DEFAULT_PORT);
        let pos = toml.position.unwrap_or_default();
        let span = |part: Option<[f64; 2]>| part.map_or(Span::FULL, |[lo, hi]| Span::new(lo, hi));
        let link = EdgeLink {
            own: span(toml.edge),
            peer: span(toml.peer_edge),
        };
        Self {
            ips,
            hostname,
            port,
            pos,
            link,
            active,
            enter_hook,
            leave_hook,
        }
    }
}

impl From<ConfigClient> for TomlClient {
    fn from(client: ConfigClient) -> Self {
        let hostname = client.hostname;
        let host_name = None;
        let mut ips = client.ips.into_iter().collect::<Vec<_>>();
        ips.sort();
        let ips = Some(ips);
        let port = if client.port == DEFAULT_PORT {
            None
        } else {
            Some(client.port)
        };
        let position = Some(client.pos);
        // only when not the whole edge
        let part = |span: Span| (!span.is_full()).then_some([span.lo, span.hi]);
        let edge = part(client.link.own);
        let peer_edge = part(client.link.peer);
        let activate_on_startup = if client.active { Some(true) } else { None };
        let enter_hook = client.enter_hook;
        let leave_hook = client.leave_hook;
        Self {
            hostname,
            host_name,
            ips,
            port,
            position,
            edge,
            peer_edge,
            activate_on_startup,
            enter_hook,
            leave_hook,
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error(transparent)]
    Toml(#[from] toml::de::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Var(#[from] VarError),
    #[error(transparent)]
    Watcher(#[from] notify::Error),
}

const DEFAULT_RELEASE_KEYS: [scancode::Linux; 4] =
    [KeyLeftCtrl, KeyLeftShift, KeyLeftMeta, KeyLeftAlt];

impl Config {
    pub fn new() -> Result<Self, ConfigError> {
        let args = Args::parse();

        // --config <file> overrules default location
        let config_path = args
            .config
            .clone()
            .unwrap_or(default_path()?.join(CONFIG_FILE_NAME));
        let config_dir = config_path
            .parent()
            .expect("config directory")
            .to_path_buf();

        // Ensure the config directory exists and write a default config file
        // if none is present. Runs on every Config::new(), regardless of which
        // entry path (GUI main, spawned daemon, CLI, test commands) we're on,
        // so a fresh Mac never hits "No such file or directory" on config.toml
        // and notify::Watcher (which requires the dir to exist on macOS
        // FSEvents and some Linux backends) has a concrete path to watch.
        fs::create_dir_all(&config_dir)?;
        if !config_path.exists() {
            let default_toml = toml::to_string_pretty(&ConfigToml::default())
                .expect("default ConfigToml serialization cannot fail");
            fs::write(&config_path, default_toml)?;
        }

        let config_toml = match ConfigToml::new(&config_path) {
            Err(e) => {
                log::warn!("{config_path:?}: {e}");
                log::warn!("Continuing without config file ...");
                None
            }
            Ok(c) => Some(c),
        };

        // --cert-path <file> overrules default location
        let cert_path = args
            .cert_path
            .clone()
            .or(config_toml.as_ref().and_then(|c| c.cert_path.clone()))
            .unwrap_or(default_path()?.join(CERT_FILE_NAME));

        let (tx, watch_rx) = tokio::sync::mpsc::channel(16);
        let watcher = RecommendedWatcher::new(
            move |res| {
                let _ = tx.blocking_send(res);
            },
            notify::Config::default(),
        )
        .inspect_err(|e| {
            log::warn!(
                "changes made to {config_path:?} by hand are noticed only when RustInput \
                 starts again: no watcher for it ({e})"
            )
        })
        .ok();
        let mut config = Config {
            args,
            cert_path,
            config_path,
            config_dir,
            config_toml,
            watcher,
            watch_rx,
        };
        if let Err(e) = config.watch() {
            log::warn!("changes made to the configuration by hand are not noticed: {e}");
            config.watcher = None;
        }
        Ok(config)
    }

    fn watch(&mut self) -> Result<(), notify::Error> {
        if let Some(watcher) = &mut self.watcher {
            watcher.watch(&self.config_dir, notify::RecursiveMode::NonRecursive)?;
        }
        Ok(())
    }

    fn unwatch(&mut self) -> Result<(), notify::Error> {
        if let Some(watcher) = &mut self.watcher {
            watcher.unwatch(&self.config_dir)?;
        }
        Ok(())
    }

    pub async fn changed(&mut self) -> Result<(), notify::Error> {
        if self.watcher.is_none() {
            return std::future::pending().await;
        }
        loop {
            let Some(event) = self.watch_rx.recv().await else {
                return std::future::pending().await;
            };
            let event = match event {
                Ok(event) => event,
                Err(e) => {
                    log::warn!("watching the configuration: {e}");
                    continue;
                }
            };
            if event.paths.contains(&self.config_path)
                && matches!(
                    event.kind,
                    EventKind::Create(_)
                        | EventKind::Modify(ModifyKind::Data(_))
                        | EventKind::Remove(_)
                )
                && self.read_from_disk()?
            {
                return Ok(());
            }
        }
    }

    /// the command to run
    pub fn command(&self) -> Option<Command> {
        self.args.command.clone()
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// public key fingerprints authorized for connection
    pub fn authorized_fingerprints(&self) -> HashMap<String, String> {
        self.config_toml
            .as_ref()
            .and_then(|c| c.authorized_fingerprints.clone())
            .unwrap_or_default()
    }

    /// path to certificate
    pub fn cert_path(&self) -> &Path {
        &self.cert_path
    }

    /// optional input-capture backend override
    pub fn capture_backend(&self) -> Option<CaptureBackend> {
        self.args
            .capture_backend
            .or(self.config_toml.as_ref().and_then(|c| c.capture_backend))
    }

    /// optional input-emulation backend override
    pub fn emulation_backend(&self) -> Option<EmulationBackend> {
        self.args
            .emulation_backend
            .or(self.config_toml.as_ref().and_then(|c| c.emulation_backend))
    }

    /// the port to use (initially)
    pub fn port(&self) -> u16 {
        self.args
            .port
            .or(self.config_toml.as_ref().and_then(|c| c.port))
            .unwrap_or(DEFAULT_PORT)
    }

    /// list of configured clients
    pub fn clients(&self) -> Vec<ConfigClient> {
        self.config_toml
            .as_ref()
            .map(|c| c.clients.clone())
            .unwrap_or_default()
            .into_iter()
            .flatten()
            .map(From::<TomlClient>::from)
            .collect()
    }

    /// whether the clipboard is synchronized with the connected devices
    pub fn clipboard(&self) -> bool {
        self.config_toml
            .as_ref()
            .and_then(|c| c.clipboard)
            .unwrap_or(true)
    }

    /// whether copied files are sent along with the clipboard
    pub fn clipboard_files(&self) -> bool {
        self.config_toml
            .as_ref()
            .and_then(|c| c.clipboard_files)
            .unwrap_or(true)
    }

    /// What happens to copied files. Without `send_copied_files` they only
    /// go to the other devices with "Copy to RustInput" in the file manager:
    /// sending everything copied while working with files would be too much.
    pub fn file_options(&self) -> FileOptions {
        let c = self.config_toml.as_ref();
        FileOptions {
            available: self.clipboard() && self.clipboard_files(),
            send_copied: c.and_then(|c| c.send_copied_files).unwrap_or(false),
            show_window: c.and_then(|c| c.show_transfers_window).unwrap_or(true),
            file_manager_menu: c.and_then(|c| c.file_manager_menu).unwrap_or(true),
        }
    }

    /// whether holding Ctrl+Alt shows the radial menu (on by default)
    pub fn radial_menu(&self) -> bool {
        self.config_toml
            .as_ref()
            .and_then(|c| c.radial_menu)
            .unwrap_or(true)
    }

    /// whether the windows are light, dark or as the system (`theme`)
    pub fn theme(&self) -> Theme {
        let Some(name) = self.config_toml.as_ref().and_then(|c| c.theme.as_deref()) else {
            return Theme::System;
        };
        Theme::from_name(name).unwrap_or_else(|| {
            log::warn!("unknown theme {name:?}, use \"system\", \"light\" or \"dark\"");
            Theme::System
        })
    }

    pub fn set_theme(&mut self, theme: Theme) {
        let c = self.config_toml.get_or_insert_with(Default::default);
        c.theme = Some(theme.name().to_owned());
    }

    /// the language configured (`language`, "pt-BR"), `None` for the one of
    /// the system
    pub fn language(&self) -> Option<String> {
        self.config_toml
            .as_ref()
            .and_then(|c| c.language.clone())
            .filter(|l| !l.trim().is_empty())
    }

    pub fn set_language(&mut self, language: Option<String>) {
        let c = self.config_toml.get_or_insert_with(Default::default);
        c.language = language;
    }

    /// whether RustInput asks GitHub for its latest version
    /// (`check_updates`), `None` when the user was not asked yet
    pub fn check_updates(&self) -> Option<bool> {
        self.config_toml.as_ref().and_then(|c| c.check_updates)
    }

    pub fn set_check_updates(&mut self, check: bool) {
        let c = self.config_toml.get_or_insert_with(Default::default);
        c.check_updates = Some(check);
    }

    /// whether RustInput is paused on this computer (`paused`)
    pub fn paused(&self) -> bool {
        self.config_toml
            .as_ref()
            .and_then(|c| c.paused)
            .unwrap_or(false)
    }

    /// pauses RustInput, or lets it go on (`paused` only while paused)
    pub fn set_paused(&mut self, paused: bool) {
        let c = self.config_toml.get_or_insert_with(Default::default);
        c.paused = paused.then_some(true);
    }

    /// changes what happens to copied files
    pub fn set_file_options(&mut self, options: FileOptions) {
        let c = self.config_toml.get_or_insert_with(Default::default);
        c.send_copied_files = Some(options.send_copied);
        c.show_transfers_window = Some(options.show_window);
        c.file_manager_menu = Some(options.file_manager_menu);
    }

    /// release bind for returning control to the host
    pub fn release_bind(&self) -> Vec<scancode::Linux> {
        self.config_toml
            .as_ref()
            .and_then(|c| c.release_bind.clone())
            .unwrap_or(Vec::from_iter(DEFAULT_RELEASE_KEYS.iter().cloned()))
    }

    /// set configured clients
    pub fn set_clients(&mut self, clients: Vec<ConfigClient>) {
        if clients.is_empty() {
            return;
        }
        if self.config_toml.is_none() {
            self.config_toml = Some(Default::default());
        }
        self.config_toml.as_mut().expect("config").clients =
            Some(clients.into_iter().map(|c| c.into()).collect::<Vec<_>>());
    }

    /// the arrangement of the computers, empty when there is none
    pub fn layout(&self) -> Layout {
        self.config_toml
            .as_ref()
            .and_then(|c| c.layout.clone())
            .unwrap_or_default()
    }

    pub fn set_layout(&mut self, layout: Layout) {
        if self.config_toml.is_none() {
            self.config_toml = Some(Default::default());
        }
        self.config_toml.as_mut().expect("config").layout = Some(layout);
    }

    /// set authorized keys
    pub fn set_authorized_keys(&mut self, fingerprints: HashMap<String, String>) {
        if self.config_toml.is_none() {
            self.config_toml = Some(Default::default());
        }
        self.config_toml
            .as_mut()
            .expect("config")
            .authorized_fingerprints = Some(fingerprints);
    }

    pub fn read_from_disk(&mut self) -> Result<bool, io::Error> {
        log::info!("reading config from {:?}", self.config_path);

        let current_config = fs::read_to_string(&self.config_path)?;
        let current_config = match current_config.parse::<DocumentMut>() {
            Ok(c) => c,
            Err(e) => {
                log::warn!("{:?} {e}", self.config_path());
                return Ok(false);
            }
        };
        let mut changed = false;
        match toml_edit::de::from_document::<ConfigToml>(current_config) {
            Ok(current_config) => {
                changed = self
                    .config_toml
                    .as_ref()
                    .is_none_or(|c| c != &current_config);
                self.config_toml.replace(current_config);
            }
            Err(e) => log::warn!("{:?} {e}", self.config_path()),
        };
        if changed {
            log::info!("config changed");
        } else {
            log::info!("config unchanged");
        }
        Ok(changed)
    }

    pub fn write_back(&mut self) -> Result<(), io::Error> {
        log::info!("writing config to {:?}", self.config_path);
        /* the new config */
        let new_config = self.config_toml.clone().unwrap_or_default();
        let new_config = toml_edit::ser::to_string_pretty(&new_config).expect("config");

        /*
         * TODO merge with current config file to preserve comments
         * => eventually we might want to split this up into clients configured
         * via the config file and clients managed through the GUI / frontend.
         * The latter should be saved to $XDG_DATA_HOME instead of $XDG_CONFIG_HOME,
         * and clients configured through .config could be made permanent.
         * For now we just override the config file.
         */

        let _ = self.unwatch();
        /* write new config to file */
        if let Some(p) = self.config_path().parent() {
            fs::create_dir_all(p)?;
        }
        {
            let mut f = File::create(self.config_path())?;
            f.write_all(new_config.as_bytes())?;
            f.sync_all()?;
        }

        let _ = self.watch();

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_theme_this_version_does_not_know_keeps_the_file_readable() {
        let c: ConfigToml = toml::from_str("theme = \"purple\"\nport = 4243\n").unwrap();
        assert_eq!(c.port, Some(4243));
        assert_eq!(c.theme.as_deref(), Some("purple"));
        let c: ConfigToml = toml::from_str("theme = \"Dark\"").unwrap();
        assert_eq!(
            c.theme.as_deref().and_then(Theme::from_name),
            Some(Theme::Dark)
        );
    }
}
