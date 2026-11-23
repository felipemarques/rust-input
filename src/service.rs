use crate::{
    arrangement::{self, ArrangeError, Arrangement, ArrangementEvent, Description},
    capture::{Capture, CaptureType, ICaptureEvent},
    client::ClientManager,
    clipboard::{self, ClipboardHandle, ClipboardMsg, Peer, PeerLinks},
    config::{Config, ConfigClient},
    connect::RustInputConnection,
    crypto,
    dns::{DnsEvent, DnsResolver},
    emulation::{Emulation, EmulationEvent},
    files::{self, FilesHandle},
    listen::{ListenerCreationError, RustInputListener},
    netstats::{NetStats, Sample},
    pairing::{self, PairError, Paired, Pairing, PairingEvent},
    radial::{self, Action, Item, Menu, Texts},
    update,
};
use futures::StreamExt;
use local_channel::mpsc::{Receiver, Sender, channel};
use log;
use rustinput_ipc::{
    AsyncFrontendListener, ClientHandle, DEFAULT_PORT, EdgeLink, FileOptions, FrontendEvent,
    FrontendRequest, IpcError, IpcListenerCreationError, LinkStats, Position, Span, Status, Theme,
    TransferInfo, UpdateState,
    layout::{ArrangementView, Layout, Machine, Monitor, Peer as ArrangedPeer},
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::{Arc, RwLock},
    time::Duration,
};
use thiserror::Error;
use tokio::{process::Command, signal, sync::Notify};
use webrtc_dtls::crypto::Certificate;

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error(transparent)]
    IpcListen(#[from] IpcListenerCreationError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    ListenError(#[from] ListenerCreationError),
    #[error("failed to load certificate: `{0}`")]
    Certificate(#[from] crypto::Error),
}

pub struct Service {
    /// configuration
    config: Config,
    /// input capture
    capture: Capture,
    /// input emulation
    emulation: Emulation,
    /// dns resolver
    resolver: DnsResolver,
    /// frontend listener
    frontend_listener: AsyncFrontendListener,
    /// authorized public key sha256 fingerprints
    authorized_keys: Arc<RwLock<HashMap<String, String>>>,
    /// (outgoing) client information
    client_manager: ClientManager,
    /// current port
    port: u16,
    /// the public key fingerprint for (D)TLS
    public_key_fingerprint: String,
    /// notify for pending frontend events
    frontend_event_pending: Notify,
    /// frontend events queued for sending
    pending_frontend_events: VecDeque<FrontendEvent>,
    /// status of input capture (enabled / disabled)
    capture_status: Status,
    /// status of input emulation (enabled / disabled)
    emulation_status: Status,
    /// keep track of registered connections to avoid duplicate barriers
    incoming_conns: HashSet<SocketAddr>,
    /// map from capture handle to connection info
    incoming_conn_info: HashMap<ClientHandle, Incoming>,
    next_trigger_handle: u64,
    /// clipboard synchronization
    clipboard: ClipboardHandle,
    /// file transfers, if enabled
    files: Option<FilesHandle>,
    /// the TCP port: file channels and pairing
    file_channel: FilesHandle,
    /// what happens to copied files
    file_options: FileOptions,
    /// codes for devices that tried to connect, and pairing with them
    pairing: Pairing,
    pairing_events: Receiver<PairingEvent>,
    /// outcomes of pairing started here, with the client
    pair_results: Receiver<(ClientHandle, Result<Paired, PairError>)>,
    pair_results_tx: Sender<(ClientHandle, Result<Paired, PairError>)>,
    /// own certificate, for pairing
    cert: Certificate,
    /// how this device introduces itself when pairing
    name: String,
    /// the radial menu and what it picks
    radial: Menu,
    radial_actions: tokio::sync::mpsc::UnboundedReceiver<Action>,
    texts: Texts,
    /// what the user should know, from the clipboard
    notices: Receiver<String>,
    /// the pointer is kept on this device
    locked: bool,
    /// paused by the user: neither controlling nor controlled
    paused: bool,
    /// whether each client answered and whether it is paused, as the window
    /// last heard
    peer_states: HashMap<ClientHandle, (bool, bool)>,
    /// state of the file transfers, whenever it changes
    transfer_reports: Receiver<Vec<TransferInfo>>,
    /// bytes and pings of the input channel, reported once a second
    netstats: NetStats,
    /// light or dark windows, as configured
    theme: Theme,
    /// the arrangement of the computers, the same on all of them
    layout: Layout,
    /// answers the other devices about the arrangement, on the TCP port
    arrangement: Arrangement,
    arrangement_events: Receiver<ArrangementEvent>,
    /// outcomes of describing and arranging other devices from here
    arrange_results: Receiver<ArrangeResult>,
    arrange_results_tx: Sender<ArrangeResult>,
    /// the clients as they described themselves: fingerprint and description
    descriptions: HashMap<ClientHandle, Result<(String, Description), String>>,
    /// whether there is a newer version, and whether to ask GitHub
    updates: UpdateState,
    /// when to ask GitHub next, while asking is on
    update_due: Pin<Box<tokio::time::Sleep>>,
    /// the latest version, or why it is not known
    update_results: Receiver<Result<String, String>>,
    update_results_tx: Sender<Result<String, String>>,
}

/// the first question for the latest version, after the start: the network
/// may come up only after RustInput at login
const UPDATE_FIRST: Duration = Duration::from_secs(120);
/// the next questions
const UPDATE_PERIOD: Duration = Duration::from_secs(12 * 60 * 60);
/// again after a failed one
const UPDATE_RETRY: Duration = Duration::from_secs(60 * 60);

/// how describing or arranging another device went
enum ArrangeResult {
    Described(ClientHandle, Result<(String, Description), ArrangeError>),
    Sent(String, Result<(), ArrangeError>),
    /// the computer `name` was asked to identify its monitors
    Identified(String, Result<(), ArrangeError>),
    /// the screen of the computer `name` (this one too) was to lock
    Locked(String, Result<(), ArrangeError>),
}

/// before locking the screen: the screen locker can not take the mouse and
/// the keyboard while RustInput or the radial menu still hold them
const LOCK_DELAY: Duration = Duration::from_millis(300);

#[derive(Debug)]
struct Incoming {
    fingerprint: String,
    addr: SocketAddr,
    pos: Position,
    /// the part of the edge at `pos` that leads back to it
    span: Span,
}

impl Service {
    pub async fn new(config: Config) -> Result<Self, ServiceError> {
        // the texts of the radial menu
        rustinput_i18n::set(rustinput_i18n::resolve(config.language().as_deref()));
        let client_manager = ClientManager::default();
        for client in config.clients() {
            client_manager.add_with_config(client);
        }

        // load certificate
        let cert = crypto::load_or_generate_key_and_cert(config.cert_path())?;
        let public_key_fingerprint = crypto::certificate_fingerprint(&cert);

        // create frontend communication adapter, exit if already running
        let frontend_listener = AsyncFrontendListener::new().await?;

        let authorized_keys = Arc::new(RwLock::new(config.authorized_fingerprints()));
        // listener + connection
        let (clipboard, clipboard_rx) =
            ClipboardHandle::new(config.clipboard(), config.clipboard_files());
        let netstats = NetStats::default();
        let listener = RustInputListener::new(
            config.port(),
            cert.clone(),
            authorized_keys.clone(),
            clipboard.clone(),
            netstats.clone(),
        )
        .await?;
        let conn = RustInputConnection::new(
            cert.clone(),
            client_manager.clone(),
            authorized_keys.clone(),
            clipboard.clone(),
            netstats.clone(),
        );
        let links = PeerLinks {
            outgoing: conn.links(),
            incoming: listener.links(),
        };
        let (reports_tx, transfer_reports) = channel();
        let name = hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_else(|| "RustInput".into());
        let (pairing_tx, pairing_events) = channel();
        let pairing = Pairing::new(name.clone(), pairing_tx);
        let layout = config.layout();
        let (arrangement_tx, arrangement_events) = channel();
        let arrangement = Arrangement::new(name.clone(), layout.revision, arrangement_tx);
        let (arrange_results_tx, arrange_results) = channel();
        // the TCP port is open for pairing even without file transfers
        let file_channel = files::spawn(
            cert.clone(),
            authorized_keys.clone(),
            config.port(),
            clipboard.clone(),
            reports_tx,
            pairing.clone(),
            arrangement.clone(),
            clipboard.files_enabled(),
        );
        let files = clipboard.files_enabled().then(|| file_channel.clone());
        let (pair_results_tx, pair_results) = channel();
        let (notices_tx, notices) = channel();
        clipboard::spawn(&clipboard, clipboard_rx, links, files.clone(), notices_tx);
        let radial = Menu::default();
        let (radial_tx, radial_actions) = tokio::sync::mpsc::unbounded_channel();
        if config.radial_menu() {
            radial::spawn(radial.clone(), radial_tx);
        }
        let file_options = config.file_options();
        let theme = config.theme();
        clipboard.set_send_copied(file_options.available && file_options.send_copied);
        files::menu::install(file_options.available && file_options.file_manager_menu);

        // input capture + emulation
        let capture_backend = config.capture_backend().map(|b| b.into());
        let paused = config.paused();
        let capture = Capture::new(capture_backend, conn, config.release_bind());
        let emulation_backend = config.emulation_backend().map(|b| b.into());
        let emulation = Emulation::new(emulation_backend, listener);

        // create dns resolver
        let resolver = DnsResolver::new()?;

        let port = config.port();
        let updates = UpdateState {
            check: config.check_updates(),
            url: update::RELEASES_PAGE.to_owned(),
            notified: config.update_notified(),
            ..Default::default()
        };
        let (update_results_tx, update_results) = channel();
        let service = Self {
            config,
            capture,
            emulation,
            frontend_listener,
            resolver,
            authorized_keys,
            public_key_fingerprint,
            client_manager,
            frontend_event_pending: Default::default(),
            port,
            pending_frontend_events: Default::default(),
            capture_status: Default::default(),
            emulation_status: Default::default(),
            incoming_conn_info: Default::default(),
            incoming_conns: Default::default(),
            next_trigger_handle: 0,
            clipboard,
            files,
            file_channel,
            file_options,
            transfer_reports,
            netstats,
            theme,
            pairing,
            pairing_events,
            pair_results,
            pair_results_tx,
            cert,
            name,
            radial,
            radial_actions,
            texts: Texts::new(),
            notices,
            locked: false,
            paused,
            peer_states: HashMap::new(),
            layout,
            arrangement,
            arrangement_events,
            arrange_results,
            arrange_results_tx,
            descriptions: HashMap::new(),
            updates,
            update_due: Box::pin(tokio::time::sleep(UPDATE_FIRST)),
            update_results,
            update_results_tx,
        };
        Ok(service)
    }

    pub async fn run(&mut self) -> Result<(), ServiceError> {
        let active = self.client_manager.active_clients();
        for handle in active.iter() {
            // small hack: `activate_client()` checks, if the client
            // is already active in client_manager and does not create a
            // capture barrier in that case so we have to deactivate it first
            self.client_manager.deactivate_client(*handle);
        }

        for handle in active {
            self.activate_client(handle);
        }
        if self.paused {
            log::info!("paused (paused = true in the configuration)");
            self.capture.pause(true);
            self.emulation.pause(true);
        }
        // the radial menu has its slices also without a window connected
        self.update_radial();

        let mut link_stats = tokio::time::interval(LINK_STATS_PERIOD);
        link_stats.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut sampled = std::time::Instant::now();

        loop {
            tokio::select! {
                _ = link_stats.tick() => {
                    let elapsed = sampled.elapsed();
                    sampled = std::time::Instant::now();
                    self.report_link_stats(elapsed);
                    self.report_peer_states();
                }
                request = self.frontend_listener.next() => self.handle_frontend_request(request),
                _ = self.frontend_event_pending.notified() => self.handle_frontend_pending().await,
                event = self.emulation.event() => self.handle_emulation_event(event),
                event = self.capture.event() => self.handle_capture_event(event),
                event = self.resolver.event() => self.handle_resolver_event(event),
                Some(transfers) = self.transfer_reports.recv() => {
                    self.notify_frontend(FrontendEvent::Transfers(transfers));
                }
                _ = self.config.changed() => self.handle_config_change(),
                Some(event) = self.pairing_events.recv() => self.handle_pairing_event(event),
                Some(action) = self.radial_actions.recv() => self.radial_action(action),
                Some(notice) = self.notices.recv() => self.notify_frontend(FrontendEvent::Notice(notice)),
                Some((handle, result)) = self.pair_results.recv() => self.pairing_done(handle, result),
                Some(event) = self.arrangement_events.recv() => self.handle_arrangement_event(event),
                Some(result) = self.arrange_results.recv() => self.arrange_done(result),
                () = self.update_due.as_mut(), if self.updates.check == Some(true) => {
                    self.check_updates();
                }
                Some(result) = self.update_results.recv() => self.updates_checked(result),
                r = signal::ctrl_c() => break r.expect("failed to wait for CTRL+C"),
            }
        }

        log::info!("terminating service ...");
        log::debug!("terminating capture ...");
        self.capture.terminate().await;
        log::debug!("terminating emulation ...");
        self.emulation.terminate().await;
        log::debug!("terminating dns resolver ...");
        self.resolver.terminate().await;

        Ok(())
    }

    fn handle_frontend_request(&mut self, request: Option<Result<FrontendRequest, IpcError>>) {
        let request = match request.expect("frontend listener closed") {
            Ok(r) => r,
            Err(e) => return log::error!("error receiving request: {e}"),
        };
        match request {
            FrontendRequest::Activate(handle, active) => {
                self.set_client_active(handle, active);
                self.save_config();
            }
            FrontendRequest::AuthorizeKey(desc, fp) => {
                self.add_authorized_key(desc, fp);
                self.save_config();
            }
            FrontendRequest::ChangePort(port) => self.change_port(port),
            FrontendRequest::Create => {
                self.add_client();
                self.save_config();
            }
            FrontendRequest::Delete(handle) => {
                self.remove_client(handle);
                self.save_config();
            }
            FrontendRequest::EnableCapture => self.capture.reenable(),
            FrontendRequest::EnableEmulation => self.emulation.reenable(),
            FrontendRequest::Enumerate() => self.enumerate(),
            FrontendRequest::UpdateFixIps(handle, fix_ips) => {
                self.update_fix_ips(handle, fix_ips);
                self.save_config();
            }
            FrontendRequest::UpdateHostname(handle, host) => {
                self.update_hostname(handle, host);
                self.save_config();
            }
            FrontendRequest::UpdatePort(handle, port) => {
                self.update_port(handle, port);
                self.save_config();
            }
            FrontendRequest::UpdatePosition(handle, pos) => {
                self.update_pos(handle, pos);
                self.save_config();
            }
            FrontendRequest::UpdateLink(handle, link) => {
                self.update_link(handle, link);
                self.save_config();
            }
            FrontendRequest::Arrangement => self.show_arrangement(true),
            FrontendRequest::Arrange(layout) => self.arrange(layout),
            FrontendRequest::IdentifyMonitors => self.identify_monitors(),
            FrontendRequest::ResolveDns(handle) => self.resolve(handle),
            FrontendRequest::Sync => self.sync_frontend(),
            FrontendRequest::RemoveAuthorizedKey(key) => {
                self.remove_authorized_key(key);
                self.save_config();
            }
            FrontendRequest::UpdateEnterHook(handle, enter_hook) => {
                self.update_enter_hook(handle, enter_hook)
            }
            FrontendRequest::UpdateLeaveHook(handle, leave_hook) => {
                self.update_leave_hook(handle, leave_hook)
            }
            FrontendRequest::SaveConfiguration => self.save_config(),
            FrontendRequest::SetFileOptions(options) => self.set_file_options(options, true),
            FrontendRequest::SetTheme(theme) => {
                self.theme = theme;
                self.config.set_theme(theme);
                self.save_config();
                self.notify_frontend(FrontendEvent::Theme(theme));
            }
            FrontendRequest::SetLanguage(language) => {
                rustinput_i18n::set(rustinput_i18n::resolve(language.as_deref()));
                self.config.set_language(language);
                self.save_config();
                self.update_radial();
                // the item of Explorer in the new language
                let options = self.file_options;
                files::menu::install(options.available && options.file_manager_menu);
            }
            FrontendRequest::SetCheckUpdates(check) => {
                self.config.set_check_updates(check);
                self.save_config();
                self.updates.check = Some(check);
                if check {
                    self.check_updates();
                } else {
                    // what it found is no longer looked after
                    self.updates = UpdateState {
                        check: Some(false),
                        url: update::RELEASES_PAGE.to_owned(),
                        notified: self.updates.notified.take(),
                        ..Default::default()
                    };
                }
                self.notify_frontend(FrontendEvent::Updates(self.updates.clone()));
            }
            FrontendRequest::CheckUpdates => self.check_updates(),
            FrontendRequest::UpdateNotified(version) => {
                if self.updates.notified.as_ref() != Some(&version) {
                    self.config.set_update_notified(version.clone());
                    self.save_config();
                    self.updates.notified = Some(version);
                    self.notify_frontend(FrontendEvent::Updates(self.updates.clone()));
                }
            }
            FrontendRequest::SetPaused(paused) => self.set_paused(paused),
            FrontendRequest::SendFiles(paths) => match &self.files {
                Some(files) => files.send_now(paths),
                None => log::warn!("file transfers are turned off (clipboard_files = false)"),
            },
            FrontendRequest::CancelTransfer(id) => {
                if let Some(files) = &self.files {
                    files.cancel(id);
                }
            }
            FrontendRequest::Pair { handle, code } => self.pair(handle, code),
            FrontendRequest::RemoveReceivedFiles => {
                if let Some(files) = &self.files {
                    files.remove_received();
                }
            }
            FrontendRequest::CancelTransfers => {
                if let Some(files) = &self.files {
                    files.cancel_all();
                }
            }
        }
    }

    fn save_config(&mut self) {
        let clients = self.client_manager.clients();
        let clients = clients
            .into_iter()
            .map(|(c, s)| ConfigClient {
                ips: HashSet::from_iter(c.fix_ips),
                hostname: c.hostname,
                port: c.port,
                pos: c.pos,
                link: c.link,
                active: s.active,
                enter_hook: c.cmd,
                leave_hook: c.leave_cmd,
            })
            .collect();
        self.config.set_clients(clients);
        if !self.layout.machines.is_empty() {
            self.config.set_layout(self.layout.clone());
        }
        let authorized_keys = self.authorized_keys.read().expect("lock").clone();
        self.config.set_authorized_keys(authorized_keys);
        if let Err(e) = self.config.write_back() {
            log::warn!("failed to write config: {e}");
        }
    }

    /// applies (and with `save` stores) what happens to copied files
    fn set_file_options(&mut self, options: FileOptions, save: bool) {
        let options = FileOptions {
            available: self.file_options.available,
            ..options
        };
        if options.file_manager_menu != self.file_options.file_manager_menu {
            files::menu::install(options.available && options.file_manager_menu);
        }
        self.clipboard
            .set_send_copied(options.available && options.send_copied);
        self.file_options = options;
        if save {
            self.config.set_file_options(options);
            self.save_config();
        }
        self.notify_frontend(FrontendEvent::FileOptions(options));
    }

    fn handle_config_change(&mut self) {
        for h in self.client_manager.registered_clients() {
            self.remove_client(h);
        }
        for c in self.config.clients() {
            let handle = self.client_manager.add_with_config(c);
            log::info!("added client {handle}");
            let (c, s) = self.client_manager.get_state(handle).unwrap();
            if s.active {
                self.client_manager.deactivate_client(handle);
                self.activate_client(handle);
            }
            self.notify_frontend(FrontendEvent::Created(handle, c, s));
        }
        let release_bind = self.config.release_bind();
        self.capture.set_release_bind(release_bind);
        let authorized_keys = self.config.authorized_fingerprints();
        self.authorized_keys
            .write()
            .unwrap()
            .clone_from(&authorized_keys);
        let file_options = self.config.file_options();
        if file_options != self.file_options {
            self.set_file_options(file_options, false);
        }
        // changed in the file by hand
        let theme = self.config.theme();
        if theme != self.theme {
            self.theme = theme;
            self.notify_frontend(FrontendEvent::Theme(theme));
        }
        self.sync_frontend();
    }

    /// Tells the frontends what went over the input channel since the last
    /// report, `elapsed` ago, per device.
    fn report_link_stats(&mut self, elapsed: std::time::Duration) {
        // also when nobody looks: the next report covers one period only
        let samples = self.netstats.sample();
        // sent also without connections, which clears what the window shows
        if !self.frontend_listener.has_frontends() {
            return;
        }
        let clients = self.client_manager.get_client_states();
        let stats = samples
            .into_iter()
            .filter_map(|sample| {
                let ip = sample.ip?;
                // the client connected at that address, or else the one
                // whose addresses include it (a device that connects here)
                let handle = clients
                    .iter()
                    .find(|(_, _, s)| s.active_addr.is_some_and(|a| a.ip() == ip))
                    .or_else(|| clients.iter().find(|(_, _, s)| s.ips.contains(&ip)))
                    .map(|(h, _, _)| *h);
                Some(link_stats(handle, ip, &sample, elapsed))
            })
            .collect();
        self.notify_frontend(FrontendEvent::LinkStats(stats));
    }

    async fn handle_frontend_pending(&mut self) {
        while let Some(event) = self.pending_frontend_events.pop_front() {
            self.frontend_listener.broadcast(event).await;
        }
    }

    fn handle_emulation_event(&mut self, event: EmulationEvent) {
        match event {
            EmulationEvent::ConnectionAttempt { fingerprint } => {
                let code = Some(self.pairing.code_for(&fingerprint));
                self.notify_frontend(FrontendEvent::ConnectionAttempt { fingerprint, code });
            }
            EmulationEvent::Entered {
                addr,
                pos,
                fingerprint,
                span,
            } => {
                // check if already registered
                if !self.incoming_conns.contains(&addr) {
                    self.add_incoming(addr, pos, fingerprint.clone(), span);
                    self.notify_frontend(FrontendEvent::DeviceEntered {
                        fingerprint,
                        addr,
                        pos,
                    });
                } else {
                    self.update_incoming(addr, pos, fingerprint, span);
                }
            }
            EmulationEvent::Disconnected { addr } => {
                if let Some(addr) = self.remove_incoming(addr) {
                    self.notify_frontend(FrontendEvent::IncomingDisconnected(addr));
                }
            }
            EmulationEvent::PortChanged(port) => match port {
                Ok(port) => {
                    self.port = port;
                    self.file_channel.change_port(port);
                    self.notify_frontend(FrontendEvent::PortChanged(port, None));
                }
                Err(e) => self
                    .notify_frontend(FrontendEvent::PortChanged(self.port, Some(format!("{e}")))),
            },
            EmulationEvent::EmulationDisabled => {
                self.emulation_status = Status::Disabled;
                self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
            }
            EmulationEvent::EmulationEnabled => {
                self.emulation_status = Status::Enabled;
                self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
            }
            EmulationEvent::ReleaseNotify { at } => match at {
                Some((edge, fraction)) => self.capture.release_at(edge, fraction),
                None => self.capture.release(),
            },
            EmulationEvent::Connected { addr, fingerprint } => {
                self.notify_frontend(FrontendEvent::DeviceConnected { addr, fingerprint });
            }
            EmulationEvent::PeerHello { addr, commit } => {
                // Map the peer's source addr back to its client handle
                // and stamp the commit. Skip if we don't have an
                // outgoing client configured for this peer (incoming-
                // only setup) — there's nowhere to display the version
                // in that case anyway.
                if let Some(handle) = self.client_manager.get_client(addr) {
                    self.client_manager.set_peer_commit(handle, Some(commit));
                    self.broadcast_client(handle);
                }
            }
        }
    }

    fn handle_capture_event(&mut self, event: ICaptureEvent) {
        match event {
            ICaptureEvent::CaptureBegin(handle, fraction) => {
                // we entered the capture zone for an incoming connection
                // => notify it that its capture should be released, and
                // where along the edge the pointer left
                if let Some(incoming) = self.incoming_conn_info.get(&handle) {
                    self.emulation.send_leave_event(incoming.addr, fraction);
                    // control goes back to that device, and so does
                    // what was copied here in the meantime
                    self.clipboard.push(Peer::Incoming(incoming.addr));
                }
            }
            ICaptureEvent::CaptureDisabled => {
                self.capture_status = Status::Disabled;
                self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
            }
            ICaptureEvent::CaptureEnabled => {
                self.capture_status = Status::Enabled;
                self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
            }
            ICaptureEvent::ClientEntered(handle) => {
                log::info!("entering client {handle} ...");
                self.notify_frontend(FrontendEvent::PointerAt(Some(handle)));
                // the client shows its own radial menu
                self.radial.set_away(true);
                self.spawn_hook_command(handle, HookKind::Enter);
            }
            ICaptureEvent::ClientLeft(handle) => {
                log::info!("leaving client {handle} ...");
                self.notify_frontend(FrontendEvent::PointerAt(None));
                self.radial.set_away(false);
                self.spawn_hook_command(handle, HookKind::Leave);
            }
            ICaptureEvent::ClientAcknowledged(handle) => {
                // the client is being controlled now: bring the clipboard along
                self.clipboard.push(Peer::Outgoing(handle));
            }
            ICaptureEvent::PlacePointer(edge, fraction) => self.emulation.warp(edge, fraction),
        }
    }

    fn handle_resolver_event(&mut self, event: DnsEvent) {
        let handle = match event {
            DnsEvent::Resolving(handle) => {
                self.client_manager.set_resolving(handle, true);
                handle
            }
            DnsEvent::Resolved(handle, hostname, ips) => {
                self.client_manager.set_resolving(handle, false);
                if let Err(e) = &ips {
                    log::warn!("could not resolve {hostname}: {e}");
                }
                let ips = ips.unwrap_or_default();
                self.client_manager.set_dns_ips(handle, ips);
                handle
            }
        };
        self.broadcast_client(handle);
    }

    fn resolve(&self, handle: ClientHandle) {
        if let Some(hostname) = self.client_manager.get_hostname(handle) {
            self.resolver.resolve(handle, hostname);
        }
    }

    fn sync_frontend(&mut self) {
        self.enumerate();
        self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
        self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
        self.notify_frontend(FrontendEvent::PortChanged(self.port, None));
        self.notify_frontend(FrontendEvent::PublicKeyFingerprint(
            self.public_key_fingerprint.clone(),
        ));
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
        self.notify_frontend(FrontendEvent::FileOptions(self.file_options));
        self.notify_frontend(FrontendEvent::Theme(self.theme));
        self.notify_frontend(FrontendEvent::Updates(self.updates.clone()));
        self.notify_frontend(FrontendEvent::Paused(self.paused));
    }

    /// Pauses RustInput on this computer, or lets it go on: paused, the
    /// mouse and keyboard stay here (a client being controlled lets go) and
    /// no other computer controls this one (one that does lets go, the
    /// others hear it is paused). Kept in the configuration.
    fn set_paused(&mut self, paused: bool) {
        if paused == self.paused {
            return;
        }
        log::info!(
            "{}",
            if paused {
                "paused by the user"
            } else {
                "going on"
            }
        );
        self.paused = paused;
        self.capture.pause(paused);
        self.emulation.pause(paused);
        self.config.set_paused(paused);
        self.save_config();
        self.notify_frontend(FrontendEvent::Paused(paused));
        self.update_radial();
    }

    /// tells the window about clients that began or stopped answering, or
    /// that were paused or went on there (learnt from their pings)
    fn report_peer_states(&mut self) {
        let changed: Vec<ClientHandle> = self
            .client_manager
            .get_client_states()
            .into_iter()
            .filter(|(handle, _, s)| {
                self.peer_states.insert(*handle, (s.alive, s.paused)) != Some((s.alive, s.paused))
            })
            .map(|(handle, ..)| handle)
            .collect();
        for handle in changed {
            self.broadcast_client(handle);
        }
    }

    /// asks GitHub for the latest version, unless it is asking already
    fn check_updates(&mut self) {
        // the next question in any case, also when this one never ends
        self.update_due
            .as_mut()
            .reset(tokio::time::Instant::now() + UPDATE_PERIOD);
        if self.updates.checking {
            return;
        }
        self.updates.checking = true;
        self.notify_frontend(FrontendEvent::Updates(self.updates.clone()));
        let results = self.update_results_tx.clone();
        tokio::task::spawn_local(async move {
            let _ = results.send(update::latest().await);
        });
    }

    fn updates_checked(&mut self, result: Result<String, String>) {
        self.updates.checking = false;
        match result {
            Ok(latest) => {
                let current = env!("CARGO_PKG_VERSION");
                self.updates.available = update::newer(&latest, current);
                if self.updates.available {
                    log::info!("RustInput {latest} is out, this is {current}");
                } else {
                    log::debug!("RustInput {current} is the latest version");
                }
                self.updates.latest = Some(latest);
                self.updates.error = None;
            }
            Err(e) => {
                log::warn!("could not ask GitHub for the latest version: {e}");
                self.updates.error = Some(e);
                self.update_due
                    .as_mut()
                    .reset(tokio::time::Instant::now() + UPDATE_RETRY);
            }
        }
        self.notify_frontend(FrontendEvent::Updates(self.updates.clone()));
    }

    const ENTER_HANDLE_BEGIN: u64 = u64::MAX / 2 + 1;

    fn add_incoming(&mut self, addr: SocketAddr, pos: Position, fingerprint: String, span: Span) {
        let handle = Self::ENTER_HANDLE_BEGIN + self.next_trigger_handle;
        self.next_trigger_handle += 1;
        let link = EdgeLink {
            own: span,
            peer: Span::FULL,
        };
        self.capture
            .create(handle, pos, CaptureType::EnterOnly, link);
        self.incoming_conns.insert(addr);
        self.incoming_conn_info.insert(
            handle,
            Incoming {
                fingerprint,
                addr,
                pos,
                span,
            },
        );
    }

    fn update_incoming(
        &mut self,
        addr: SocketAddr,
        pos: Position,
        fingerprint: String,
        span: Span,
    ) {
        let incoming = self
            .incoming_conn_info
            .iter_mut()
            .find(|(_, i)| i.addr == addr)
            .map(|(_, i)| i)
            .expect("no such client");
        let mut changed = false;
        if incoming.fingerprint != fingerprint {
            incoming.fingerprint = fingerprint.clone();
            changed = true;
        }
        if incoming.pos != pos {
            incoming.pos = pos;
            changed = true;
        }
        if incoming.span != span {
            incoming.span = span;
            changed = true;
        }
        if changed {
            self.remove_incoming(addr);
            self.add_incoming(addr, pos, fingerprint.clone(), span);
            self.notify_frontend(FrontendEvent::IncomingDisconnected(addr));
            self.notify_frontend(FrontendEvent::DeviceEntered {
                fingerprint,
                addr,
                pos,
            });
        }
    }

    fn remove_incoming(&mut self, addr: SocketAddr) -> Option<SocketAddr> {
        let handle = self
            .incoming_conn_info
            .iter()
            .find(|(_, incoming)| incoming.addr == addr)
            .map(|(k, _)| *k)?;
        self.capture.destroy(handle);
        self.incoming_conns.remove(&addr);
        self.incoming_conn_info
            .remove(&handle)
            .map(|incoming| incoming.addr)
    }

    fn notify_frontend(&mut self, event: FrontendEvent) {
        let changes_menu = matches!(
            event,
            FrontendEvent::Created(..)
                | FrontendEvent::Deleted(_)
                | FrontendEvent::State(..)
                | FrontendEvent::Enumerate(_)
                | FrontendEvent::FileOptions(_)
        );
        self.pending_frontend_events.push_back(event);
        self.frontend_event_pending.notify_one();
        if changes_menu {
            self.update_radial();
        }
    }

    /// the slices of the radial menu: the clients to go to, then the actions;
    /// while paused only going on and the window
    fn update_radial(&mut self) {
        if self.paused {
            self.radial.set_items(vec![
                Item {
                    label: self.texts.pause(true),
                    action: Action::TogglePause,
                },
                Item {
                    label: self.texts.window(),
                    action: Action::ShowWindow,
                },
            ]);
            return;
        }
        let clients: Vec<(ClientHandle, String)> = self
            .client_manager
            .get_client_states()
            .into_iter()
            .filter(|(_, _, state)| state.active)
            .map(|(handle, config, state)| {
                let name = config
                    .hostname
                    .clone()
                    .or_else(|| state.ips.iter().next().map(|ip| ip.to_string()))
                    .unwrap_or_else(|| format!("{handle}"));
                (handle, name)
            })
            .collect();
        let files = self.file_options.available;
        let mut items = Vec::new();
        let mut add = |label: String, action: Action| items.push(Item { label, action });
        for (handle, name) in &clients {
            add(self.texts.go_to(name), Action::GoTo(*handle));
        }
        if self.clipboard.capabilities() != 0 {
            add(self.texts.send_clipboard(), Action::SendClipboard);
        }
        if files {
            add(self.texts.send_files(), Action::SendCopiedFiles);
        }
        add(self.texts.lock(self.locked), Action::ToggleLock);
        add(self.texts.pause(false), Action::TogglePause);
        for (handle, name) in &clients {
            add(self.texts.lock_screen(name), Action::LockScreen(*handle));
        }
        add(self.texts.lock_all_screens(), Action::LockAllScreens);
        if files {
            add(self.texts.transfers(), Action::ShowTransfers);
        }
        add(self.texts.window(), Action::ShowWindow);
        self.radial.set_items(items);
    }

    /// does what was picked in the radial menu
    fn radial_action(&mut self, action: Action) {
        match action {
            Action::GoTo(handle) => self.capture.enter(handle),
            Action::SendClipboard => self.clipboard.notify(ClipboardMsg::PushAll),
            Action::SendCopiedFiles => self.clipboard.notify(ClipboardMsg::SendCopiedFiles {
                none: self.texts.no_files(),
            }),
            Action::ToggleLock => {
                self.locked = !self.locked;
                self.capture.lock(self.locked);
                self.update_radial();
            }
            Action::TogglePause => self.set_paused(!self.paused),
            Action::LockScreen(handle) => self.lock_screens(vec![handle], false),
            Action::LockAllScreens => {
                let clients = self
                    .client_manager
                    .get_client_states()
                    .into_iter()
                    .filter(|(_, _, state)| state.active)
                    .map(|(handle, ..)| handle)
                    .collect();
                self.lock_screens(clients, true);
            }
            Action::ShowTransfers => self.notify_frontend(FrontendEvent::ShowTransfers),
            Action::ShowWindow => self.notify_frontend(FrontendEvent::ShowWindow),
        }
    }

    /// the other device entered the code shown here, or that code is used up
    fn handle_pairing_event(&mut self, event: PairingEvent) {
        match event {
            PairingEvent::Paired(Paired { fingerprint, name }) => {
                self.add_authorized_key(name.clone(), fingerprint.clone());
                self.save_config();
                self.notify_frontend(FrontendEvent::Paired { fingerprint, name });
            }
            PairingEvent::NewCode { fingerprint, code } => {
                self.notify_frontend(FrontendEvent::ConnectionAttempt {
                    fingerprint,
                    code: Some(code),
                });
            }
        }
    }

    /// authorizes this device and the client on each other with the code the
    /// client shows
    fn pair(&mut self, handle: ClientHandle, code: String) {
        let port = self.client_manager.get_port(handle).unwrap_or(DEFAULT_PORT);
        let mut addrs: Vec<SocketAddr> = self
            .client_manager
            .active_addr(handle)
            .into_iter()
            .collect();
        for ip in self.client_manager.get_ips(handle).unwrap_or_default() {
            let addr = SocketAddr::new(ip, port);
            if !addrs.contains(&addr) {
                addrs.push(addr);
            }
        }
        log::info!("pairing with client {handle} at {addrs:?}");
        let (cert, name, results) = (
            self.cert.clone(),
            self.name.clone(),
            self.pair_results_tx.clone(),
        );
        tokio::task::spawn_local(async move {
            let result = pairing::pair(cert, addrs, code, name).await;
            let _ = results.send((handle, result));
        });
    }

    fn pairing_done(&mut self, handle: ClientHandle, result: Result<Paired, PairError>) {
        let result = match result {
            Ok(Paired { fingerprint, name }) => {
                log::info!("paired with client {handle}: {name}");
                self.add_authorized_key(name.clone(), fingerprint);
                self.save_config();
                Ok(name)
            }
            Err(e) => {
                log::warn!("pairing with client {handle} failed: {e}");
                Err(e.to_string())
            }
        };
        self.notify_frontend(FrontendEvent::PairingResult { handle, result });
    }

    fn add_authorized_key(&mut self, desc: String, fp: String) {
        self.authorized_keys.write().expect("lock").insert(fp, desc);
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
    }

    fn remove_authorized_key(&mut self, fp: String) {
        self.authorized_keys.write().expect("lock").remove(&fp);
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
    }

    fn enumerate(&mut self) {
        let clients = self.client_manager.get_client_states();
        self.notify_frontend(FrontendEvent::Enumerate(clients));
    }

    fn add_client(&mut self) {
        let handle = self.client_manager.add_client();
        log::info!("added client {handle}");
        let (c, s) = self.client_manager.get_state(handle).unwrap();
        self.notify_frontend(FrontendEvent::Created(handle, c, s));
    }

    fn set_client_active(&mut self, handle: ClientHandle, active: bool) {
        if active {
            self.activate_client(handle);
        } else {
            self.deactivate_client(handle);
        }
    }

    fn deactivate_client(&mut self, handle: ClientHandle) {
        log::debug!("deactivating client {handle}");
        if self.client_manager.deactivate_client(handle) {
            self.capture.destroy(handle);
            self.broadcast_client(handle);
            log::info!("deactivated client {handle}");
        }
    }

    fn activate_client(&mut self, handle: ClientHandle) {
        log::debug!("activating client {handle}");

        /* resolve dns on activate */
        self.resolve(handle);

        /* deactivate other clients on the same part of this edge: clients
         * share an edge only on parts of it of their own */
        let Some(pos) = self.client_manager.get_pos(handle) else {
            return;
        };
        let link = self.client_manager.get_link(handle).unwrap_or_default();
        for (other, other_link) in self.client_manager.clients_at(pos) {
            if other != handle && other_link.own.overlaps(&link.own) {
                self.deactivate_client(other);
            }
        }

        /* activate the client */
        if self.client_manager.activate_client(handle) {
            /* notify capture and frontends */
            self.capture.create(handle, pos, CaptureType::Default, link);
            self.broadcast_client(handle);
            log::info!("activated client {handle} ({pos})");
        }
    }

    fn change_port(&mut self, port: u16) {
        if self.port != port {
            self.emulation.request_port_change(port);
        } else {
            self.notify_frontend(FrontendEvent::PortChanged(self.port, None));
        }
    }

    fn remove_client(&mut self, handle: ClientHandle) {
        if self
            .client_manager
            .remove_client(handle)
            .map(|(_, s)| s.active)
            .unwrap_or(false)
        {
            self.capture.destroy(handle);
        }
        self.notify_frontend(FrontendEvent::Deleted(handle));
    }

    fn update_fix_ips(&mut self, handle: ClientHandle, fix_ips: Vec<IpAddr>) {
        self.client_manager.set_fix_ips(handle, fix_ips);
        self.broadcast_client(handle);
    }

    fn update_hostname(&mut self, handle: ClientHandle, hostname: Option<String>) {
        log::info!("hostname changed: {hostname:?}");
        if self.client_manager.set_hostname(handle, hostname.clone()) {
            self.resolve(handle);
        }
        self.broadcast_client(handle);
    }

    fn update_port(&mut self, handle: ClientHandle, port: u16) {
        self.client_manager.set_port(handle, port);
        self.broadcast_client(handle);
    }

    fn update_pos(&mut self, handle: ClientHandle, pos: Position) {
        // update state in event input emulator & input capture
        if self.client_manager.set_pos(handle, pos) {
            self.deactivate_client(handle);
            self.activate_client(handle);
        }
        self.broadcast_client(handle);
    }

    /// where the client meets this device on the edge at its position
    fn update_link(&mut self, handle: ClientHandle, link: EdgeLink) {
        if self.client_manager.set_link(handle, link) {
            self.capture.update_link(handle, link);
        }
        self.broadcast_client(handle);
    }

    fn update_enter_hook(&mut self, handle: ClientHandle, enter_hook: Option<String>) {
        self.client_manager.set_enter_hook(handle, enter_hook);
        self.broadcast_client(handle);
    }

    fn update_leave_hook(&mut self, handle: ClientHandle, leave_hook: Option<String>) {
        self.client_manager.set_leave_hook(handle, leave_hook);
        self.broadcast_client(handle);
    }

    fn broadcast_client(&mut self, handle: ClientHandle) {
        let event = self
            .client_manager
            .get_state(handle)
            .map(|(c, s)| FrontendEvent::State(handle, c, s))
            .unwrap_or(FrontendEvent::NoSuchClient(handle));
        self.notify_frontend(event);
    }

    fn spawn_hook_command(&self, handle: ClientHandle, kind: HookKind) {
        let cmd = match kind {
            HookKind::Enter => self.client_manager.get_enter_cmd(handle),
            HookKind::Leave => self.client_manager.get_leave_cmd(handle),
        };
        let Some(cmd) = cmd else { return };
        // as SYSTEM, a command of the configuration of the user would get
        // the rights of Windows itself
        #[cfg(windows)]
        if crate::lock::runs_as_system() {
            log::warn!("the {kind} hook of client {handle} is not run: RustInput runs as SYSTEM");
            return;
        }
        tokio::task::spawn_local(async move {
            log::info!("spawning {kind} hook for client {handle}");
            let mut child = match Command::new("sh").arg("-c").arg(cmd.as_str()).spawn() {
                Ok(c) => c,
                Err(e) => {
                    log::warn!("could not execute {kind} hook for client {handle}: {e}");
                    return;
                }
            };
            match child.wait().await {
                Ok(s) => {
                    if s.success() {
                        log::info!("{kind} hook for client {handle} ({cmd}) exited successfully");
                    } else {
                        log::warn!("{kind} hook for client {handle} ({cmd}) exited with {s}");
                    }
                }
                Err(e) => log::warn!("{kind} hook for client {handle} ({cmd}): {e}"),
            }
        });
    }
}

#[derive(Clone, Copy, Debug)]
enum HookKind {
    Enter,
    Leave,
}

impl std::fmt::Display for HookKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HookKind::Enter => f.write_str("enter"),
            HookKind::Leave => f.write_str("leave"),
        }
    }
}

/// how often the frontends hear about the input channel
const LINK_STATS_PERIOD: std::time::Duration = std::time::Duration::from_secs(1);

/// `sample`, taken over `elapsed`, as the frontends see it
fn link_stats(
    handle: Option<ClientHandle>,
    ip: IpAddr,
    sample: &Sample,
    elapsed: std::time::Duration,
) -> LinkStats {
    let seconds = elapsed.as_secs_f64().max(0.001);
    LinkStats {
        handle,
        ip,
        rtt_us: sample.rtt(),
        lost: sample.lost,
        sent_per_s: (sample.sent as f64 / seconds).round() as u64,
        received_per_s: (sample.received as f64 / seconds).round() as u64,
        sent_total: sample.sent_total,
        received_total: sample.received_total,
    }
}

/// The arrangement of the computers (see [`crate::arrangement`] and
/// `rustinput_ipc::layout`): shown and changed in the window, sent to the
/// other computers of it, and taken from them.
impl Service {
    /// this computer as a machine of the arrangement, at its place in it
    fn own_machine(&self) -> Machine {
        let (monitors, width, height) =
            arrangement::own_monitors().unwrap_or_else(|| (Vec::new(), 1920, 1080));
        let placed = self.layout.machine(&self.public_key_fingerprint);
        Machine {
            id: self.public_key_fingerprint.clone(),
            name: self.name.clone(),
            x: placed.map_or(0, |m| m.x),
            y: placed.map_or(0, |m| m.y),
            width,
            height,
            monitors,
            os: std::env::consts::OS.to_owned(),
            hostname: Some(self.name.clone()),
            ips: Vec::new(),
            port: self.port,
        }
    }

    /// the id of a client in the arrangement: its fingerprint once it
    /// described itself, else a stand-in for it on this computer
    fn client_id(&self, handle: ClientHandle) -> String {
        match self.descriptions.get(&handle) {
            Some(Ok((fingerprint, _))) => fingerprint.clone(),
            _ => self
                .layout
                .machines
                .iter()
                .find(|m| self.client_for(m, None) == Some(handle))
                .map(|m| m.id.clone())
                .unwrap_or_else(|| format!("client-{handle}")),
        }
    }

    /// where a client listens: where it was reached, then its addresses
    fn client_addrs(&self, handle: ClientHandle) -> Vec<SocketAddr> {
        let port = self.client_manager.get_port(handle).unwrap_or(DEFAULT_PORT);
        let mut addrs: Vec<SocketAddr> = self
            .client_manager
            .active_addr(handle)
            .into_iter()
            .collect();
        for ip in self.client_manager.get_ips(handle).unwrap_or_default() {
            let addr = SocketAddr::new(ip, port);
            if !addrs.contains(&addr) {
                addrs.push(addr);
            }
        }
        addrs
    }

    /// The window shows the arrangement; with `describe`, the clients are
    /// asked about their monitors, and it is shown again as they answer.
    fn show_arrangement(&mut self, describe: bool) {
        let own = self.own_machine();
        let mut peers = Vec::new();
        for handle in self.client_manager.registered_clients() {
            let Some((config, state)) = self.client_manager.get_state(handle) else {
                continue;
            };
            let id = self.client_id(handle);
            let placed = self.layout.machine(&id);
            let (described, error, name, monitors, os) = match self.descriptions.get(&handle) {
                Some(Ok((_, d))) => (true, None, d.name.clone(), d.monitors.clone(), d.os.clone()),
                Some(Err(e)) => (
                    false,
                    Some(e.clone()),
                    String::new(),
                    Vec::new(),
                    String::new(),
                ),
                None => (false, None, String::new(), Vec::new(), String::new()),
            };
            let os = if os.is_empty() {
                placed.map(|m| m.os.clone()).unwrap_or_default()
            } else {
                os
            };
            let name = Some(name)
                .filter(|n| !n.is_empty())
                .or_else(|| config.hostname.clone())
                .or_else(|| config.fix_ips.first().map(|ip| ip.to_string()))
                .unwrap_or_else(|| format!("client {handle}"));
            let (width, height) = bounds(&monitors)
                .or_else(|| placed.map(|m| (m.width, m.height)))
                .unwrap_or((0, 0));
            let monitors = if monitors.is_empty() {
                placed.map(|m| m.monitors.clone()).unwrap_or_default()
            } else {
                monitors
            };
            peers.push(ArrangedPeer {
                handle,
                machine: Machine {
                    id,
                    name,
                    x: placed.map_or(0, |m| m.x),
                    y: placed.map_or(0, |m| m.y),
                    width,
                    height,
                    monitors,
                    os,
                    hostname: config.hostname.clone(),
                    ips: config.fix_ips.clone(),
                    port: config.port,
                },
                described,
                error,
                pos: config.pos,
                active: state.active,
                alive: state.alive,
                paused: state.paused,
            });
        }
        self.notify_frontend(FrontendEvent::Arrangement(ArrangementView {
            own,
            layout: self.layout.clone(),
            peers,
        }));
        if !describe {
            return;
        }
        for handle in self.client_manager.registered_clients() {
            let addrs = self.client_addrs(handle);
            let (cert, results) = (self.cert.clone(), self.arrange_results_tx.clone());
            tokio::task::spawn_local(async move {
                let result = arrangement::describe(cert, addrs, None).await;
                let _ = results.send(ArrangeResult::Described(handle, result));
            });
        }
    }

    /// arranges the computers as the window says, here and on the others
    fn arrange(&mut self, mut layout: Layout) {
        layout.revision = layout.revision.max(self.layout.revision) + 1;
        // this computer as it is now
        let own = self.own_machine();
        if let Some(me) = layout
            .machines
            .iter_mut()
            .find(|m| m.id == self.public_key_fingerprint)
        {
            me.name = own.name;
            me.hostname = own.hostname;
            me.port = own.port;
        }
        let me = self.public_key_fingerprint.clone();
        log::info!("arrangement {} from the window", layout.revision);
        self.apply_layout(layout.clone(), &me, None);
        for machine in layout.machines.iter().filter(|m| m.id != me) {
            let handle = self.client_for(machine, None);
            let mut addrs = handle.map(|h| self.client_addrs(h)).unwrap_or_default();
            for ip in &machine.ips {
                let addr = SocketAddr::new(*ip, machine.port);
                if !addrs.contains(&addr) {
                    addrs.push(addr);
                }
            }
            let expected = is_fingerprint(&machine.id).then(|| machine.id.clone());
            let (cert, results) = (self.cert.clone(), self.arrange_results_tx.clone());
            let (layout, you, name) = (layout.clone(), machine.id.clone(), machine.name.clone());
            tokio::task::spawn_local(async move {
                let result = arrangement::arrange(cert, addrs, expected, layout, you).await;
                let _ = results.send(ArrangeResult::Sent(name, result));
            });
        }
    }

    /// shows the numbers of the monitors here and on the other computers
    fn identify_monitors(&mut self) {
        crate::identify::show(self.name.clone());
        for handle in self.client_manager.registered_clients() {
            let addrs = self.client_addrs(handle);
            if addrs.is_empty() {
                continue;
            }
            let (expected, name) = self.peer(handle);
            let (cert, results) = (self.cert.clone(), self.arrange_results_tx.clone());
            tokio::task::spawn_local(async move {
                let result = arrangement::identify(cert, addrs, expected).await;
                let _ = results.send(ArrangeResult::Identified(name, result));
            });
        }
    }

    /// Locks the screens of the computers of `clients`, and of this one if
    /// `here`. Locking only ever protects a computer: it is asked even of
    /// one paused, and done here even when paused.
    fn lock_screens(&mut self, clients: Vec<ClientHandle>, here: bool) {
        for handle in clients {
            let addrs = self.client_addrs(handle);
            let (expected, name) = self.peer(handle);
            log::info!("asking {name} to lock its screen");
            let (cert, results) = (self.cert.clone(), self.arrange_results_tx.clone());
            tokio::task::spawn_local(async move {
                let result = arrangement::lock(cert, addrs, expected).await;
                let _ = results.send(ArrangeResult::Locked(name, result));
            });
        }
        if here {
            let locking = self.lock_here();
            let (name, results) = (self.name.clone(), self.arrange_results_tx.clone());
            tokio::task::spawn_local(async move {
                let result = locking.await.map_err(ArrangeError::Failed);
                let _ = results.send(ArrangeResult::Locked(name, result));
            });
        }
    }

    /// Locks the screen of this computer: the pointer comes back here first,
    /// as the screen locker can not take a mouse RustInput holds.
    fn lock_here(&self) -> impl std::future::Future<Output = Result<(), String>> + 'static {
        self.capture.release();
        async {
            tokio::time::sleep(LOCK_DELAY).await;
            tokio::task::spawn_blocking(crate::lock::lock_screen)
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
        }
    }

    /// the fingerprint of a client, when it described itself, and its name
    fn peer(&self, handle: ClientHandle) -> (Option<String>, String) {
        match self.descriptions.get(&handle) {
            Some(Ok((fingerprint, d))) => (Some(fingerprint.clone()), d.name.clone()),
            _ => (None, self.client_name(handle)),
        }
    }

    /// what a client is called before it described itself
    fn client_name(&self, handle: ClientHandle) -> String {
        let config = self.client_manager.get_state(handle).map(|(c, _)| c);
        config
            .as_ref()
            .and_then(|c| c.hostname.clone())
            .or_else(|| {
                config
                    .as_ref()
                    .and_then(|c| c.fix_ips.first().map(|ip| ip.to_string()))
            })
            .unwrap_or_else(|| format!("client {handle}"))
    }

    fn handle_arrangement_event(&mut self, event: ArrangementEvent) {
        let (layout, you, from, addr) = match event {
            ArrangementEvent::Received {
                layout,
                you,
                from,
                addr,
            } => (layout, you, from, addr),
            ArrangementEvent::Identify { addr } => {
                log::info!("{addr} asks to show the numbers of the monitors");
                crate::identify::show(self.name.clone());
                return;
            }
            ArrangementEvent::Lock { addr, done } => {
                log::info!("{addr} asks to lock the screen");
                let locking = self.lock_here();
                tokio::task::spawn_local(async move {
                    let result = locking.await;
                    match &result {
                        Ok(()) => log::info!("screen locked, as {addr} asked"),
                        Err(e) => log::warn!("screen not locked, as {addr} asked: {e}"),
                    }
                    let _ = done.send(result);
                });
                return;
            }
        };
        if layout.revision < self.layout.revision {
            log::info!(
                "arrangement {} from {addr} is older than this one ({}), kept this one",
                layout.revision,
                self.layout.revision
            );
            return;
        }
        let by = layout
            .machine(&from)
            .map(|m| m.name.clone())
            .or_else(|| {
                self.authorized_keys
                    .read()
                    .expect("lock")
                    .get(&from)
                    .cloned()
            })
            .unwrap_or_else(|| addr.ip().to_string());
        self.apply_layout(layout, &you, Some((from, addr)));
        self.notify_frontend(FrontendEvent::ArrangementChanged { by });
        self.show_arrangement(false);
    }

    fn arrange_done(&mut self, result: ArrangeResult) {
        match result {
            ArrangeResult::Described(handle, result) => {
                if let Err(e) = &result {
                    log::info!("client {handle} did not describe itself: {e}");
                }
                self.descriptions
                    .insert(handle, result.map_err(|e| e.to_string()));
                self.show_arrangement(false);
            }
            ArrangeResult::Sent(name, result) => {
                let error = result.err().map(|e| e.to_string());
                match &error {
                    None => log::info!("arrangement sent to {name}"),
                    Some(e) => log::warn!("arrangement not sent to {name}: {e}"),
                }
                self.notify_frontend(FrontendEvent::ArrangementSent { name, error });
            }
            ArrangeResult::Locked(name, result) => match result {
                Ok(()) => log::info!("the screen of {name} is locked"),
                Err(e) => {
                    log::warn!("the screen of {name} did not lock: {e}");
                    self.notify_frontend(FrontendEvent::ScreenNotLocked {
                        name,
                        error: e.to_string(),
                    });
                }
            },
            ArrangeResult::Identified(name, result) => match result {
                Ok(()) => log::info!("{name} shows the numbers of its monitors"),
                Err(e) => {
                    log::warn!("{name} did not show the numbers of its monitors: {e}");
                    self.notify_frontend(FrontendEvent::MonitorsNotIdentified {
                        name,
                        error: e.to_string(),
                    });
                }
            },
        }
    }

    /// The client of `machine`: by the fingerprint it described itself
    /// with, by the stand-in id of this computer, by name, by address, or
    /// (`sender`) by the address an arrangement came from.
    fn client_for(
        &self,
        machine: &Machine,
        sender: Option<&(String, SocketAddr)>,
    ) -> Option<ClientHandle> {
        let clients = self.client_manager.get_client_states();
        let by_fingerprint = clients.iter().find(
            |(h, ..)| matches!(self.descriptions.get(h), Some(Ok((fp, _))) if *fp == machine.id),
        );
        let by_stand_in = || {
            clients
                .iter()
                .find(|(h, ..)| machine.id == format!("client-{h}"))
        };
        let same_name = |name: &Option<String>| {
            name.as_ref().is_some_and(|n| {
                machine
                    .hostname
                    .iter()
                    .chain(std::iter::once(&machine.name))
                    .any(|m| m.eq_ignore_ascii_case(n))
            })
        };
        let by_name = || clients.iter().find(|(_, c, _)| same_name(&c.hostname));
        let by_address = || {
            clients.iter().find(|(_, c, s)| {
                let sent_from = sender
                    .filter(|(fp, _)| *fp == machine.id)
                    .map(|(_, addr)| addr.ip());
                c.fix_ips
                    .iter()
                    .chain(s.ips.iter())
                    .any(|ip| machine.ips.contains(ip) || Some(*ip) == sent_from)
            })
        };
        by_fingerprint
            .or_else(by_stand_in)
            .or_else(by_name)
            .or_else(by_address)
            .map(|(h, ..)| *h)
    }

    /// Takes `layout`, in which this computer is `me`: each computer next to
    /// it becomes an active client at its edge and part of it, a computer of
    /// the arrangement that does not touch it is not reached any more.
    /// `sender` is the computer the arrangement came from.
    fn apply_layout(&mut self, layout: Layout, me: &str, sender: Option<(String, SocketAddr)>) {
        let neighbors = layout.neighbors(me);
        for machine in layout.machines.iter().filter(|m| m.id != me) {
            let handle = self.client_for(machine, sender.as_ref());
            let neighbor = neighbors.iter().find(|n| n.id == machine.id);
            match (handle, neighbor) {
                (Some(handle), Some(n)) => self.place_client(handle, n.pos, n.link),
                (None, Some(n)) => {
                    // a computer this one did not know: reached at its name,
                    // its addresses, or where the arrangement came from
                    let handle = self.client_manager.add_client();
                    let mut ips: Vec<IpAddr> = machine.ips.clone();
                    if let Some((fp, addr)) = &sender {
                        if *fp == machine.id && !ips.contains(&addr.ip()) {
                            ips.push(addr.ip());
                        }
                    }
                    let hostname = machine.hostname.clone().filter(|_| ips.is_empty());
                    log::info!(
                        "arrangement: new client {handle} for {} ({hostname:?}, {ips:?})",
                        machine.name
                    );
                    self.client_manager.set_hostname(handle, hostname);
                    self.client_manager.set_fix_ips(handle, ips);
                    self.client_manager.set_port(handle, machine.port);
                    self.notify_frontend(FrontendEvent::Created(
                        handle,
                        self.client_manager.get_state(handle).expect("client").0,
                        self.client_manager.get_state(handle).expect("client").1,
                    ));
                    self.place_client(handle, n.pos, n.link);
                }
                (Some(handle), None) => {
                    // in the arrangement, not next to this computer
                    if self.client_manager.active_clients().contains(&handle) {
                        log::info!("arrangement: client {handle} is not next to this computer");
                        self.deactivate_client(handle);
                        self.broadcast_client(handle);
                    }
                }
                (None, None) => {}
            }
        }
        self.layout = layout;
        self.arrangement.set_revision(self.layout.revision);
        self.save_config();
    }

    /// puts the client at `pos`, on the parts of the edges in `link`, active
    fn place_client(&mut self, handle: ClientHandle, pos: Position, link: EdgeLink) {
        let active = self.client_manager.active_clients().contains(&handle);
        if active {
            self.deactivate_client(handle);
        }
        self.client_manager.set_pos(handle, pos);
        self.client_manager.set_link(handle, link);
        self.activate_client(handle);
        self.broadcast_client(handle);
        log::info!("arrangement: client {handle} at {pos}, {link:?}");
    }
}

/// the size of the desktop of `monitors`, none without monitors
fn bounds(monitors: &[Monitor]) -> Option<(i32, i32)> {
    let width = monitors.iter().map(|m| m.x + m.width).max()?;
    let height = monitors.iter().map(|m| m.y + m.height).max()?;
    Some((width, height))
}

/// whether `id` is a certificate fingerprint (32 bytes in hex, with colons)
fn is_fingerprint(id: &str) -> bool {
    id.len() == 95
        && id
            .split(':')
            .all(|b| b.len() == 2 && b.chars().all(|c| c.is_ascii_hexdigit()))
}
