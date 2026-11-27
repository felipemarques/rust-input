//! File transfers between devices.
//!
//! Copied files go to another device over a channel of their own: TLS over
//! TCP, on the same port number as the input channel, so a large transfer
//! does not hold up mouse and keyboard. The connecting side of the input
//! channel opens it as soon as the peer announced the feature, and both
//! directions share it. Both sides authenticate with the certificate of the
//! input channel (see [`tls`]).
//!
//! Files are sent when the user picks "Copy to RustInput" in a file manager
//! (see [`menu`]), and, if enabled, whenever the pointer moves to another
//! device while files are copied. Transfers to a device run one after the
//! other and the files of a transfer one by one. Received files land in a
//! folder of their own (see [`store`]) and the clipboard then points at them
//! as cut files: pasting moves them out of that folder to wherever the user
//! pastes, so nothing is left behind. Everything is written to a log (see
//! [`journal`]).
//!
//! With a device that announced `CAP_FILE_RESUME` (0.37.0) a transfer whose
//! connection is lost is not given up: the receiver keeps what arrived, the
//! sender keeps the transfer, and once they are connected again it goes on
//! from the first byte that is missing (see [`store`]).

mod journal;
mod manifest;
pub(crate) mod menu;
mod store;
pub(crate) mod tls;
mod wire;

pub(crate) use manifest::files_key;
pub(crate) use tls::AuthorizedKeys;

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    fs, io,
    net::SocketAddr,
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use local_channel::mpsc::{Receiver, Sender, channel};
use rustinput_ipc::{TransferInfo, TransferState, human_size};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, ReadHalf, WriteHalf},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    task::{JoinHandle, spawn_local},
    time::timeout,
};
use tokio_rustls::{TlsAcceptor, TlsConnector, TlsStream};
use webrtc_dtls::crypto::Certificate;

use crate::{
    arrangement::{self, Arrangement},
    clipboard::{ClipboardHandle, ClipboardMsg, Peer},
    crypto,
    pairing::{self, Pairing},
};
use journal::Journal;
use manifest::Manifest;
use wire::{CHUNK_SIZE, Entry, Frame, Position};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const RECONNECT_DELAY: Duration = Duration::from_secs(5);
const REPORT_INTERVAL: Duration = Duration::from_millis(250);
/// File managers may start one process per selected item for "Copy to
/// RustInput" (Explorer does): what arrives within this time goes together.
const MERGE_DELAY: Duration = Duration::from_millis(700);
/// how often received files are checked for having been pasted
const WATCH_INTERVAL: Duration = Duration::from_secs(1);
/// Received files that left the clipboard unpasted are removed after this
/// long: pasting them as a copy may still be reading them.
const REPLACED_GRACE: Duration = Duration::from_secs(10 * 60);
/// finished transfers kept for the transfers window
const MAX_HISTORY: usize = 100;
/// frames waiting to be written on a connection, data has to wait for room
const WRITE_QUEUE: usize = 8;
/// why a transfer stopped when its connection went away
const LOST: &str = "the connection was lost";
/// A resumable transfer offered again by itself that had arrived already
/// (its `Complete` was lost with the connection): the sender takes this
/// `Decline` for the `Complete`.
const ALREADY_RECEIVED: &str = "already received";
/// how long the device of a waiting transfer is tried every
/// [`RECONNECT_DELAY`], then every [`STANDBY_DELAY`]
const STANDBY_EAGER: Duration = Duration::from_secs(2 * 60);
const STANDBY_DELAY: Duration = Duration::from_secs(60);
/// how long an offer waits for another connection that held the same
/// transfer to be closed
const PREEMPT_WAIT: Duration = Duration::from_secs(3);

type Stream = TlsStream<TcpStream>;

/// the files of a transfer with their sizes, for the log
type Listing = Vec<(String, u64)>;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// a device that takes part in file transfers
#[derive(Clone, Debug)]
pub(crate) struct PeerInfo {
    pub(crate) peer: Peer,
    /// fingerprint of its certificate, which its file channel must present
    pub(crate) fingerprint: String,
    /// where its file channel listens, for devices this one connects to
    pub(crate) connect_to: Option<SocketAddr>,
    /// its transfers go on where they stopped (`CAP_FILE_RESUME`)
    pub(crate) resume: bool,
}

impl PeerInfo {
    /// whether this device opens the file channel to the peer
    fn initiated(&self) -> bool {
        matches!(self.peer, Peer::Outgoing(_))
    }
}

enum Request {
    AddPeer(PeerInfo),
    RemovePeer(Peer),
    Send(Peer, Vec<PathBuf>),
    SendNow(Vec<PathBuf>),
    Cancel(u64),
    CancelAll,
    RemoveReceived,
    ChangePort(u16),
}

/// what the rest of the service uses to talk to the file transfers
#[derive(Clone)]
pub(crate) struct FilesHandle {
    tx: Sender<Request>,
}

impl FilesHandle {
    fn request(&self, request: Request) {
        // the task only ends together with the service
        let _ = self.tx.send(request);
    }

    /// the peer announced file transfers
    pub(crate) fn add_peer(&self, info: PeerInfo) {
        self.request(Request::AddPeer(info));
    }

    pub(crate) fn remove_peer(&self, peer: Peer) {
        self.request(Request::RemovePeer(peer));
    }

    /// sends the copied `paths` (files and folders) to the peer
    pub(crate) fn send(&self, peer: Peer, paths: Vec<PathBuf>) {
        self.request(Request::Send(peer, paths));
    }

    /// sends `paths` to every connected device right away ("Copy to
    /// RustInput"), together with what follows within [`MERGE_DELAY`]
    pub(crate) fn send_now(&self, paths: Vec<PathBuf>) {
        self.request(Request::SendNow(paths));
    }

    /// cancels the transfer with the id it was reported with
    pub(crate) fn cancel(&self, id: u64) {
        self.request(Request::Cancel(id));
    }

    pub(crate) fn cancel_all(&self) {
        self.request(Request::CancelAll);
    }

    /// removes the received files that were not pasted
    pub(crate) fn remove_received(&self) {
        self.request(Request::RemoveReceived);
    }

    pub(crate) fn change_port(&self, port: u16) {
        self.request(Request::ChangePort(port));
    }
}

/// Starts the file transfers. Whenever the transfers change, their state is
/// sent to `reports`. The TCP port also takes pairing (see [`pairing`]); file
/// channels only if `enabled`.
// one argument per thing the TCP port answers for
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn(
    cert: Certificate,
    authorized: AuthorizedKeys,
    port: u16,
    clipboard: ClipboardHandle,
    reports: Sender<Vec<TransferInfo>>,
    pairing: Pairing,
    arrangement: Arrangement,
    enabled: bool,
) -> FilesHandle {
    let (tx, requests) = channel();
    let (internal_tx, internal_rx) = channel();
    let received_dir = rustinput_ipc::received_dir().filter(|_| enabled);
    match &received_dir {
        // nothing of an earlier run is on the clipboard anymore
        Some(dir) => store::cleanup(dir, 0),
        None if enabled => log::warn!("no folder for received files, they will be refused"),
        None => {}
    }
    let acceptor = match tls::server_config(&cert) {
        Ok(config) => Some(TlsAcceptor::from(config)),
        Err(e) => {
            log::warn!("file channel: can not accept connections: {e}");
            None
        }
    };
    let mut task = Task {
        requests,
        internal_rx,
        internal_tx,
        reports,
        clipboard,
        cert,
        authorized,
        acceptor,
        listener: None,
        pairing,
        arrangement,
        enabled,
        received_dir,
        journal: Journal::open(),
        peers: HashMap::new(),
        conns: HashMap::new(),
        connecting: HashSet::new(),
        outgoing: Vec::new(),
        incoming: HashMap::new(),
        standby: HashMap::new(),
        busy: Default::default(),
        refused: Default::default(),
        received_tags: Default::default(),
        history: Vec::new(),
        explicit: Vec::new(),
        explicit_at: None,
        next_transfer: 0,
        next_uid: 1,
        next_conn: 0,
        last_report: None,
    };
    task.listen(port);
    spawn_local(task.run());
    FilesHandle { tx }
}

/// progress of a transfer, shared with the task moving its data
#[derive(Clone, Debug, Default)]
struct Progress {
    files: u32,
    files_done: u32,
    bytes: u64,
    bytes_done: u64,
    current: Option<String>,
    /// went on where it stopped: the bytes there already, and when
    resumed: Option<(u64, u64)>,
}

/// the transfer the reader of a connection is receiving
#[derive(Clone, Debug)]
struct Receive {
    id: u32,
    started_ms: u64,
    progress: Progress,
    /// resumable: "<fingerprint>/<key>", and the folder kept to go on with
    kept: Option<(String, PathBuf)>,
}

/// Keys of resumable transfers being received right now, "<fingerprint of
/// the sender>/<key>": the same files offered over a second connection are
/// not written into the same folder. Removed when dropped.
struct Busy {
    keys: Rc<RefCell<HashSet<String>>>,
    tag: String,
}

impl Drop for Busy {
    fn drop(&mut self) {
        self.keys.borrow_mut().remove(&self.tag);
    }
}

struct Conn {
    fingerprint: String,
    /// this device opened the connection
    initiated: bool,
    /// where it connected to, if it opened it
    addr: Option<SocketAddr>,
    writer: mpsc::Sender<Frame>,
    receiving: Rc<RefCell<Option<Receive>>>,
    tasks: [JoinHandle<()>; 2],
}

impl Drop for Conn {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

#[derive(Debug)]
enum OutState {
    /// the copied files are being listed
    Scanning,
    Waiting,
    /// offered on the connection, waiting for the answer
    Offered(u64),
    Streaming(u64, JoinHandle<()>),
    /// everything was sent, waiting for the receiver to confirm
    Streamed(u64),
}

struct Outgoing {
    /// id reported to the user, the same from start to end
    uid: u64,
    /// id on the connection
    id: u32,
    peer: Peer,
    fingerprint: String,
    initiated: bool,
    manifest: Option<Rc<Manifest>>,
    listing: Listing,
    started_ms: u64,
    progress: Rc<RefCell<Progress>>,
    state: OutState,
    /// the device goes on with transfers where they stopped
    resume: bool,
    /// resumable: what identifies these files to the receiver
    key: Option<String>,
    /// resumable: its connection was lost, it goes on once there is one
    interrupted: bool,
}

impl Outgoing {
    fn conn(&self) -> Option<u64> {
        match self.state {
            OutState::Offered(c) | OutState::Streaming(c, _) | OutState::Streamed(c) => Some(c),
            OutState::Scanning | OutState::Waiting => None,
        }
    }
}

/// a transfer being received, by connection and id on it
struct IncomingInfo {
    uid: u64,
    started_ms: u64,
    listing: Listing,
}

/// a finished transfer
struct Record {
    info: TransferInfo,
    /// received: its top level items, which the clipboard points at
    paths: Vec<PathBuf>,
    /// received: the folder holding them, until it is removed
    dir: Option<PathBuf>,
    /// received: when a newer transfer replaced it on the clipboard
    replaced: Option<Instant>,
    /// received and interrupted: "<fingerprint>/<key>" and the folder kept
    /// to go on with
    kept: Option<(String, PathBuf)>,
}

impl Record {
    /// received files waiting in the temporary folder to be pasted
    fn ready(&self) -> bool {
        !self.info.sending && self.info.state == TransferState::Done && self.dir.is_some()
    }
}

enum Internal {
    Connected {
        fingerprint: String,
        initiated: bool,
        addr: Option<SocketAddr>,
        stream: Box<Stream>,
    },
    ConnectFailed {
        fingerprint: String,
    },
    Retry {
        fingerprint: String,
    },
    Scanned {
        id: u32,
        manifest: Result<Manifest, String>,
    },
    /// a frame about a transfer this device sends
    Frame {
        conn: u64,
        frame: Frame,
    },
    Streamed {
        id: u32,
        result: Result<(), String>,
    },
    /// a transfer is coming in
    Receiving {
        conn: u64,
        id: u32,
        started_ms: u64,
        listing: Listing,
        /// resumable: "<fingerprint>/<key>"
        tag: Option<String>,
        /// it goes on at this many bytes
        resumed: Option<u64>,
    },
    Received {
        conn: u64,
        id: u32,
        paths: Vec<PathBuf>,
        progress: Progress,
    },
    ReceiveFailed {
        conn: u64,
        id: u32,
        reason: String,
        progress: Progress,
    },
    Closed {
        conn: u64,
    },
    /// The resumable transfer "<fingerprint>/<key>" is offered again on
    /// `conn` while another connection still receives it: that one is dead
    /// without knowing it (the sender only offers it on one connection at a
    /// time), and is closed.
    Preempt {
        conn: u64,
        tag: String,
    },
}

struct Task {
    requests: Receiver<Request>,
    internal_rx: Receiver<Internal>,
    internal_tx: Sender<Internal>,
    reports: Sender<Vec<TransferInfo>>,
    clipboard: ClipboardHandle,
    cert: Certificate,
    authorized: AuthorizedKeys,
    acceptor: Option<TlsAcceptor>,
    listener: Option<JoinHandle<()>>,
    pairing: Pairing,
    arrangement: Arrangement,
    /// file channels are taken
    enabled: bool,
    received_dir: Option<PathBuf>,
    journal: Journal,
    peers: HashMap<Peer, PeerInfo>,
    conns: HashMap<u64, Conn>,
    /// fingerprints a connection is being opened to
    connecting: HashSet<String>,
    outgoing: Vec<Outgoing>,
    incoming: HashMap<(u64, u32), IncomingInfo>,
    /// Where to connect to again while a resumable transfer with a device
    /// waits for its connection, by fingerprint: when the input channel is
    /// gone as well, the device is no longer among `peers`, and the
    /// transfer would only go on once the pointer crosses to it.
    standby: HashMap<String, (SocketAddr, Instant)>,
    /// resumable transfers being received, see [`Busy`]
    busy: Rc<RefCell<HashSet<String>>>,
    /// resumable transfers cancelled here, "<fingerprint>/<key>": offered
    /// again, they are refused
    refused: Rc<RefCell<HashSet<String>>>,
    /// resumable transfers received in this run
    received_tags: Rc<RefCell<HashSet<String>>>,
    history: Vec<Record>,
    /// chosen in a file manager, waiting for the rest of the selection
    explicit: Vec<PathBuf>,
    explicit_at: Option<tokio::time::Instant>,
    next_transfer: u32,
    next_uid: u64,
    next_conn: u64,
    last_report: Option<Vec<TransferInfo>>,
}

impl Task {
    async fn run(mut self) {
        let mut report = tokio::time::interval(REPORT_INTERVAL);
        report.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut watch = tokio::time::interval(WATCH_INTERVAL);
        watch.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let merge = self.explicit_at;
            tokio::select! {
                request = self.requests.recv() => match request {
                    Some(request) => self.request(request),
                    None => break,
                },
                Some(event) = self.internal_rx.recv() => self.internal(event).await,
                _ = tokio::time::sleep_until(merge.unwrap_or_else(tokio::time::Instant::now)), if merge.is_some() => {
                    self.send_explicit();
                }
                _ = watch.tick() => self.watch_received(),
                _ = report.tick() => self.report(),
            }
        }
    }

    fn uid(&mut self) -> u64 {
        let uid = self.next_uid;
        self.next_uid += 1;
        uid
    }

    fn listen(&mut self, port: u16) {
        if let Some(listener) = self.listener.take() {
            listener.abort();
        }
        let Some(acceptor) = self.acceptor.clone() else {
            return;
        };
        let accept = Accept {
            acceptor,
            internal: self.internal_tx.clone(),
            pairing: self.pairing.clone(),
            arrangement: self.arrangement.clone(),
            authorized: self.authorized.clone(),
            own: crypto::certificate_fingerprint(&self.cert),
            enabled: self.enabled,
        };
        self.listener = Some(spawn_local(listen(port, accept)));
    }

    /// name of the device with `fingerprint`, as authorized on this one
    fn name(&self, fingerprint: &str) -> String {
        self.authorized
            .read()
            .expect("lock")
            .get(fingerprint)
            .cloned()
            .unwrap_or_else(|| fingerprint.chars().take(11).collect())
    }

    fn conn_for(&self, fingerprint: &str, initiated: bool) -> Option<u64> {
        self.conns
            .iter()
            .find(|(_, c)| c.fingerprint == fingerprint && c.initiated == initiated)
            .map(|(&id, _)| id)
    }

    fn request(&mut self, request: Request) {
        match request {
            Request::AddPeer(info) => {
                log::debug!("file transfers with {}", info.peer);
                let connect =
                    info.connect_to.is_some() && self.conn_for(&info.fingerprint, true).is_none();
                let fingerprint = info.fingerprint.clone();
                self.peers.insert(info.peer, info);
                if connect {
                    self.connect(fingerprint);
                }
                self.start_next();
            }
            Request::RemovePeer(peer) => {
                self.peers.remove(&peer);
                // transfers still waiting for this device are dropped, running
                // ones finish or fail with their connection; resumable ones
                // wait for the device to be back
                let waiting: Vec<u32> = self
                    .outgoing
                    .iter()
                    .filter(|o| o.peer == peer && o.conn().is_none() && !o.resume)
                    .map(|o| o.id)
                    .collect();
                for id in waiting {
                    self.fail_outgoing(id, "the device disconnected".into());
                }
            }
            Request::Send(peer, paths) => self.send(peer, paths),
            Request::SendNow(paths) => {
                self.explicit.extend(paths);
                self.explicit_at = Some(tokio::time::Instant::now() + MERGE_DELAY);
            }
            Request::Cancel(uid) => self.cancel(uid),
            Request::CancelAll => self.cancel_all(),
            Request::RemoveReceived => self.remove_received(),
            Request::ChangePort(port) => self.listen(port),
        }
    }

    /// sends what was chosen in a file manager to every connected device
    fn send_explicit(&mut self) {
        self.explicit_at = None;
        let mut seen = HashSet::new();
        let paths: Vec<PathBuf> = std::mem::take(&mut self.explicit)
            .into_iter()
            .filter(|p| seen.insert(p.clone()))
            .collect();
        if paths.is_empty() {
            return;
        }
        // Two devices listing each other are peers twice: once in each
        // direction. Send once per device, preferably over an open channel.
        let mut peers: Vec<&PeerInfo> = self.peers.values().collect();
        peers.sort_by_key(|p| self.conn_for(&p.fingerprint, p.initiated()).is_none());
        let mut devices = HashSet::new();
        let targets: Vec<Peer> = peers
            .into_iter()
            .filter(|p| devices.insert(p.fingerprint.clone()))
            .map(|p| p.peer)
            .collect();
        if targets.is_empty() {
            let reason = "no connected computer takes files".to_owned();
            log::warn!("{reason}, {} item(s) not sent", paths.len());
            let uid = self.uid();
            let progress = Progress {
                files: paths.len() as u32,
                ..Default::default()
            };
            self.finished(Finished {
                uid,
                sending: true,
                peer: "RustInput".into(),
                progress,
                state: TransferState::Failed(reason),
                started_ms: now_ms(),
                listing: Vec::new(),
                paths: Vec::new(),
            });
            return;
        }
        log::info!("sending {} item(s) chosen in a file manager", paths.len());
        for peer in targets {
            self.send(peer, paths.clone());
        }
    }

    fn send(&mut self, peer: Peer, paths: Vec<PathBuf>) {
        let Some(info) = self.peers.get(&peer) else {
            log::info!("{peer} does not take file transfers, the copied files stay here");
            return;
        };
        let (fingerprint, initiated, resume) =
            (info.fingerprint.clone(), info.initiated(), info.resume);
        let id = self.next_transfer;
        self.next_transfer = self.next_transfer.wrapping_add(1);
        let uid = self.uid();
        log::info!("sending {} copied item(s) to {peer}", paths.len());
        self.outgoing.push(Outgoing {
            uid,
            id,
            peer,
            fingerprint,
            initiated,
            manifest: None,
            listing: Vec::new(),
            started_ms: now_ms(),
            progress: Default::default(),
            state: OutState::Scanning,
            resume,
            key: None,
            interrupted: false,
        });
        // listing large folders takes a while, the input must not wait for it
        let internal = self.internal_tx.clone();
        spawn_local(async move {
            let manifest = tokio::task::spawn_blocking(move || as_user(|| manifest::scan(&paths)))
                .await
                .map_err(|e| e.to_string())
                .and_then(|r| r.map_err(|e| e.to_string()));
            let _ = internal.send(Internal::Scanned { id, manifest });
        });
    }

    fn connect(&mut self, fingerprint: String) {
        let Some(addr) = self
            .peers
            .values()
            .find(|p| p.fingerprint == fingerprint && p.connect_to.is_some())
            .and_then(|p| p.connect_to)
            .or_else(|| self.standby.get(&fingerprint).map(|(addr, _)| *addr))
        else {
            return;
        };
        if !self.connecting.insert(fingerprint.clone()) {
            return;
        }
        let (cert, internal) = (self.cert.clone(), self.internal_tx.clone());
        spawn_local(connect(cert, fingerprint, addr, internal));
    }

    async fn internal(&mut self, event: Internal) {
        match event {
            Internal::Connected {
                fingerprint,
                initiated,
                addr,
                stream,
            } => self.connected(fingerprint, initiated, addr, *stream),
            Internal::ConnectFailed { fingerprint } => {
                self.connecting.remove(&fingerprint);
                self.retry_later(fingerprint);
            }
            Internal::Retry { fingerprint } => {
                if self.conn_for(&fingerprint, true).is_none() {
                    self.connect(fingerprint);
                }
            }
            Internal::Scanned { id, manifest } => match manifest {
                Ok(manifest) => {
                    let names: HashMap<String, String> =
                        self.authorized.read().expect("lock").clone();
                    if let Some(o) = self.outgoing.iter_mut().find(|o| o.id == id) {
                        *o.progress.borrow_mut() = Progress {
                            files: manifest.files() as u32,
                            bytes: manifest.bytes(),
                            ..Default::default()
                        };
                        o.listing = listing(&manifest.entries);
                        let peer = names
                            .get(&o.fingerprint)
                            .cloned()
                            .unwrap_or_else(|| o.fingerprint.chars().take(11).collect());
                        let text = format!(
                            "↑ to {peer}: sending {}, {}",
                            files(manifest.files() as u32),
                            human_size(manifest.bytes())
                        );
                        if o.resume {
                            o.key = Some(manifest::resume_key(&manifest));
                        }
                        o.manifest = Some(Rc::new(manifest));
                        o.state = OutState::Waiting;
                        self.journal.write(&text);
                    }
                    self.start_next();
                }
                Err(reason) => self.fail_outgoing(id, reason),
            },
            Internal::Frame { conn, frame } => self.frame(conn, frame).await,
            Internal::Streamed { id, result } => match result {
                Ok(()) => {
                    if let Some(o) = self.outgoing.iter_mut().find(|o| o.id == id) {
                        if let OutState::Streaming(conn, _) = o.state {
                            o.state = OutState::Streamed(conn);
                        }
                    }
                }
                Err(reason) => {
                    let Some(o) = self.outgoing.iter().find(|o| o.id == id) else {
                        return;
                    };
                    // its connection was closed meanwhile, which interrupted it
                    if !matches!(o.state, OutState::Streaming(..)) {
                        return;
                    }
                    let resumable = o.key.is_some();
                    if resumable && reason == LOST {
                        self.interrupt(id);
                    } else {
                        self.send_frame(id, |id| Frame::Cancel { id }).await;
                        self.fail_outgoing(id, reason);
                    }
                }
            },
            Internal::Receiving {
                conn,
                id,
                started_ms,
                listing,
                tag,
                resumed,
            } => {
                let bytes = listing.iter().map(|(_, size)| size).sum();
                let text = match resumed {
                    Some(at) => format!(
                        "↓ from {}: going on with {}, {}, at {}",
                        self.conn_name(conn),
                        files(listing.len() as u32),
                        human_size(bytes),
                        human_size(at)
                    ),
                    None => format!(
                        "↓ from {}: receiving {}, {}",
                        self.conn_name(conn),
                        files(listing.len() as u32),
                        human_size(bytes)
                    ),
                };
                self.journal.write(&text);
                // the interrupted transfer goes on under the same id
                let earlier = tag.and_then(|tag| {
                    self.history.iter().position(|r| {
                        r.info.state == TransferState::Interrupted
                            && r.kept.as_ref().is_some_and(|(t, _)| *t == tag)
                    })
                });
                if let Some(index) = earlier {
                    let record = self.history.remove(index);
                    self.incoming.insert(
                        (conn, id),
                        IncomingInfo {
                            uid: record.info.id,
                            started_ms,
                            listing: Vec::new(),
                        },
                    );
                }
                let info = self.incoming_info(conn, id, started_ms);
                info.listing = listing;
            }
            Internal::Received {
                conn,
                id,
                paths,
                progress,
            } => self.received(conn, id, paths, progress),
            Internal::ReceiveFailed {
                conn,
                id,
                reason,
                progress,
            } => {
                let peer = self.conn_name(conn);
                log::warn!("receiving files from {peer} failed: {reason}");
                self.receive_failed(conn, id, peer, progress, reason);
            }
            Internal::Closed { conn } => self.closed(conn),
            Internal::Preempt { conn, tag } => {
                let stale: Vec<u64> = self
                    .conns
                    .iter()
                    .filter(|(&c, _)| c != conn)
                    .filter(|(_, c)| {
                        c.receiving
                            .borrow()
                            .as_ref()
                            .and_then(|r| r.kept.as_ref())
                            .is_some_and(|(t, _)| *t == tag)
                    })
                    .map(|(&c, _)| c)
                    .collect();
                for c in stale {
                    log::info!("file channel: the transfer goes on over a newer connection");
                    self.closed(c);
                }
            }
        }
    }

    /// the user visible id and the listing of a transfer coming in
    fn incoming_info(&mut self, conn: u64, id: u32, started_ms: u64) -> &mut IncomingInfo {
        if !self.incoming.contains_key(&(conn, id)) {
            let uid = self.uid();
            self.incoming.insert(
                (conn, id),
                IncomingInfo {
                    uid,
                    started_ms,
                    listing: Vec::new(),
                },
            );
        }
        self.incoming.get_mut(&(conn, id)).expect("inserted")
    }

    fn receive_failed(
        &mut self,
        conn: u64,
        id: u32,
        peer: String,
        progress: Progress,
        reason: String,
    ) {
        let info = self.incoming_info(conn, id, now_ms());
        let (uid, started_ms) = (info.uid, info.started_ms);
        self.incoming.remove(&(conn, id));
        self.finished(Finished {
            uid,
            sending: false,
            peer,
            progress,
            state: TransferState::Failed(reason),
            started_ms,
            listing: Vec::new(),
            paths: Vec::new(),
        });
    }

    fn connected(
        &mut self,
        fingerprint: String,
        initiated: bool,
        addr: Option<SocketAddr>,
        stream: Stream,
    ) {
        if initiated {
            self.connecting.remove(&fingerprint);
        }
        self.standby.remove(&fingerprint);
        // a newer connection replaces the previous one
        if let Some(old) = self.conn_for(&fingerprint, initiated) {
            self.closed(old);
        }
        let conn = self.next_conn;
        self.next_conn += 1;
        log::info!(
            "file channel {} {}",
            if initiated { "to" } else { "from" },
            self.name(&fingerprint)
        );
        let (reader, writer) = tokio::io::split(stream);
        let (frames_tx, frames_rx) = mpsc::channel(WRITE_QUEUE);
        let receiving: Rc<RefCell<Option<Receive>>> = Default::default();
        let read = spawn_local(read_loop(
            Reader {
                conn,
                fingerprint: fingerprint.clone(),
                writer: frames_tx.clone(),
                receiving: receiving.clone(),
                received_dir: self.received_dir.clone(),
                internal: self.internal_tx.clone(),
                busy: self.busy.clone(),
                refused: self.refused.clone(),
                received: self.received_tags.clone(),
            },
            reader,
        ));
        let write = spawn_local(write_loop(
            conn,
            writer,
            frames_rx,
            self.internal_tx.clone(),
        ));
        self.conns.insert(
            conn,
            Conn {
                fingerprint,
                initiated,
                addr,
                writer: frames_tx,
                receiving,
                tasks: [read, write],
            },
        );
        self.start_next();
    }

    fn closed(&mut self, conn: u64) {
        let Some(closed) = self.conns.remove(&conn) else {
            return;
        };
        let running: Vec<(u32, bool)> = self
            .outgoing
            .iter()
            .filter(|o| o.conn() == Some(conn))
            .map(|o| (o.id, o.key.is_some()))
            .collect();
        for (id, resumable) in running {
            if resumable {
                self.interrupt(id);
            } else {
                self.fail_outgoing(id, LOST.into());
            }
        }
        let receive = closed.receiving.borrow_mut().take();
        if let Some(receive) = receive {
            let peer = self.name(&closed.fingerprint);
            match receive.kept {
                Some(kept) => {
                    self.receive_interrupted(conn, receive.id, peer, receive.progress, kept)
                }
                None => self.receive_failed(conn, receive.id, peer, receive.progress, LOST.into()),
            }
        }
        self.incoming.retain(|(c, _), _| *c != conn);
        if closed.initiated {
            if let Some(addr) = closed.addr {
                if self.waiting_for(&closed.fingerprint) {
                    self.standby
                        .entry(closed.fingerprint.clone())
                        .or_insert((addr, Instant::now()))
                        .0 = addr;
                }
            }
            self.retry_later(closed.fingerprint.clone());
        }
    }

    /// whether a resumable transfer with the device waits to go on
    fn waiting_for(&self, fingerprint: &str) -> bool {
        let prefix = format!("{fingerprint}/");
        self.outgoing
            .iter()
            .any(|o| o.fingerprint == fingerprint && o.key.is_some())
            || self.history.iter().any(|r| {
                r.info.state == TransferState::Interrupted
                    && r.kept
                        .as_ref()
                        .is_some_and(|(tag, _)| tag.starts_with(&prefix))
            })
    }

    /// connects again in a while, if the device still takes file transfers
    /// or a resumable transfer with it waits to go on
    fn retry_later(&mut self, fingerprint: String) {
        let peer = self
            .peers
            .values()
            .any(|p| p.fingerprint == fingerprint && p.connect_to.is_some());
        if !self.waiting_for(&fingerprint) {
            self.standby.remove(&fingerprint);
        }
        // only for a waiting transfer: more seldom after a while
        let delay = match (peer, self.standby.get(&fingerprint)) {
            (true, _) => RECONNECT_DELAY,
            (false, Some((_, since))) if since.elapsed() < STANDBY_EAGER => RECONNECT_DELAY,
            (false, Some(_)) => STANDBY_DELAY,
            (false, None) => return,
        };
        let internal = self.internal_tx.clone();
        spawn_local(async move {
            tokio::time::sleep(delay).await;
            let _ = internal.send(Internal::Retry { fingerprint });
        });
    }

    /// a frame from the receiver of one of our transfers
    async fn frame(&mut self, conn: u64, frame: Frame) {
        let id = frame.id();
        let Some(index) = self
            .outgoing
            .iter()
            .position(|o| o.id == id && o.conn() == Some(conn))
        else {
            log::debug!("file channel: {frame:?} for no transfer");
            return;
        };
        match frame {
            Frame::Accept { .. } | Frame::AcceptAt { .. } => {
                let at = match frame {
                    Frame::AcceptAt { at, .. } => Some(at),
                    _ => None,
                };
                let Some(writer) = self.conns.get(&conn).map(|c| c.writer.clone()) else {
                    return;
                };
                let outgoing = &mut self.outgoing[index];
                let Some(manifest) = outgoing.manifest.clone() else {
                    return;
                };
                if at.is_some() != outgoing.key.is_some() {
                    log::debug!("file channel: {frame:?} does not answer the offer");
                    return;
                }
                let (progress, internal) = (outgoing.progress.clone(), self.internal_tx.clone());
                let task = spawn_local(async move {
                    let result = async {
                        let mut from = Position::default();
                        if let Some(at) = at {
                            from = resume_at(&manifest, at).await?;
                            if from != Position::default() {
                                log::info!(
                                    "sending files: going on at file {}, byte {}",
                                    from.file,
                                    from.offset
                                );
                            }
                            writer
                                .send(Frame::Start {
                                    id,
                                    file: from.file,
                                    offset: from.offset,
                                })
                                .await
                                .map_err(|_| LOST.to_owned())?;
                        }
                        stream(id, &manifest, from, writer, progress).await
                    };
                    let result = result.await;
                    let _ = internal.send(Internal::Streamed { id, result });
                });
                outgoing.state = OutState::Streaming(conn, task);
            }
            Frame::Decline { reason, .. }
                if reason == ALREADY_RECEIVED && self.outgoing[index].key.is_some() =>
            {
                self.delivered(index);
            }
            Frame::Decline { reason, .. } => {
                let peer = self.name(&self.outgoing[index].fingerprint);
                self.fail_outgoing(id, format!("{peer} refused the files: {reason}"));
            }
            Frame::Abort { .. } => {
                self.send_frame(id, |id| Frame::Cancel { id }).await;
                let peer = self.name(&self.outgoing[index].fingerprint);
                self.fail_outgoing(id, format!("cancelled on {peer}"));
            }
            Frame::Complete { .. } => self.delivered(index),
            frame => log::debug!("file channel: unexpected {frame:?}"),
        }
    }

    /// the receiver has everything of the outgoing transfer at `index`
    fn delivered(&mut self, index: usize) {
        let outgoing = self.outgoing.remove(index);
        if let OutState::Streaming(_, task) = &outgoing.state {
            task.abort();
        }
        let peer = self.name(&outgoing.fingerprint);
        let mut progress = outgoing.progress.borrow().clone();
        progress.files_done = progress.files;
        progress.bytes_done = progress.bytes;
        progress.current = None;
        log::info!("{} file(s) delivered to {peer}", progress.files);
        self.finished(Finished {
            uid: outgoing.uid,
            sending: true,
            peer,
            progress,
            state: TransferState::Done,
            started_ms: outgoing.started_ms,
            listing: outgoing.listing,
            paths: Vec::new(),
        });
        self.start_next();
    }

    /// writes a frame about the outgoing transfer `id` on its connection
    async fn send_frame(&self, id: u32, frame: impl FnOnce(u32) -> Frame) {
        let writer = self
            .outgoing
            .iter()
            .find(|o| o.id == id)
            .and_then(Outgoing::conn)
            .and_then(|c| self.conns.get(&c))
            .map(|c| c.writer.clone());
        if let Some(writer) = writer {
            let _ = writer.send(frame(id)).await;
        }
    }

    fn fail_outgoing(&mut self, id: u32, reason: String) {
        let Some(index) = self.outgoing.iter().position(|o| o.id == id) else {
            return;
        };
        let outgoing = self.outgoing.remove(index);
        if let OutState::Streaming(_, task) = &outgoing.state {
            task.abort();
        }
        let peer = self.name(&outgoing.fingerprint);
        log::warn!("sending files to {peer} failed: {reason}");
        let progress = outgoing.progress.borrow().clone();
        self.finished(Finished {
            uid: outgoing.uid,
            sending: true,
            peer,
            progress,
            state: TransferState::Failed(reason),
            started_ms: outgoing.started_ms,
            listing: Vec::new(),
            paths: Vec::new(),
        });
        self.start_next();
    }

    /// offers the next waiting transfer on every idle connection
    fn start_next(&mut self) {
        let busy: HashSet<u64> = self.outgoing.iter().filter_map(Outgoing::conn).collect();
        // the same files to the same device go one after the other: the
        // receiver would take a second offer of them for the first one
        // going on over another connection
        let mut moving: HashSet<(String, String)> = self
            .outgoing
            .iter()
            .filter(|o| o.conn().is_some())
            .filter_map(|o| Some((o.fingerprint.clone(), o.key.clone()?)))
            .collect();
        let mut started = HashSet::new();
        let peers = &self.peers;
        for outgoing in &mut self.outgoing {
            if !matches!(outgoing.state, OutState::Waiting) {
                continue;
            }
            // the device came back without going on where transfers stopped
            // (an older RustInput): a plain offer, from the start
            let mut known = peers
                .values()
                .filter(|p| p.fingerprint == outgoing.fingerprint)
                .peekable();
            if outgoing.key.is_some() && known.peek().is_some() && !known.all(|p| p.resume) {
                outgoing.key = None;
            }
            if let Some(key) = &outgoing.key {
                if moving.contains(&(outgoing.fingerprint.clone(), key.clone())) {
                    continue;
                }
            }
            // a resumable transfer may go on over the connection in the
            // other direction, the same as before is preferred
            let same = |c: &&Conn| c.fingerprint == outgoing.fingerprint;
            let Some(conn) = self
                .conns
                .iter()
                .filter(|(_, c)| same(c))
                .filter(|(_, c)| c.initiated == outgoing.initiated || outgoing.key.is_some())
                .min_by_key(|(_, c)| c.initiated != outgoing.initiated)
                .map(|(&conn, _)| conn)
            else {
                continue;
            };
            let c = &self.conns[&conn];
            if busy.contains(&conn) || !started.insert(conn) {
                continue;
            }
            let Some(manifest) = &outgoing.manifest else {
                continue;
            };
            let offer = match &outgoing.key {
                Some(key) => Frame::OfferResumable {
                    id: outgoing.id,
                    key: key.clone(),
                    entries: manifest.entries.clone(),
                    continued: outgoing.interrupted,
                },
                None => Frame::Offer {
                    id: outgoing.id,
                    entries: manifest.entries.clone(),
                },
            };
            // the queue has room: nothing else is offered on this connection
            if c.writer.try_send(offer).is_ok() {
                outgoing.state = OutState::Offered(conn);
                if let Some(key) = &outgoing.key {
                    moving.insert((outgoing.fingerprint.clone(), key.clone()));
                }
            }
        }
    }

    /// cancels the transfer reported with `uid`
    fn cancel(&mut self, uid: u64) {
        if let Some(id) = self.outgoing.iter().find(|o| o.uid == uid).map(|o| o.id) {
            self.cancel_outgoing(id);
            return;
        }
        let incoming = self
            .incoming
            .iter()
            .find(|(_, i)| i.uid == uid)
            .map(|(&key, _)| key);
        // the sender answers with Cancel, then the reader discards the files
        if let Some(c) = incoming.and_then(|(conn, id)| self.conns.get(&conn).map(|c| (c, id))) {
            let _ = c.0.writer.try_send(Frame::Abort { id: c.1 });
            return;
        }
        // interrupted here: what was kept goes, and the sender is refused
        // when it offers the files again
        let interrupted = self
            .history
            .iter()
            .position(|r| r.info.id == uid && r.info.state == TransferState::Interrupted);
        if let Some(index) = interrupted {
            let record = &mut self.history[index];
            if let Some((tag, root)) = record.kept.take() {
                store::remove_partial(&root);
                self.refused.borrow_mut().insert(tag);
            }
            record.info.state = TransferState::Failed("cancelled".into());
            record.info.finished_ms = Some(now_ms());
            let text = format!(
                "↓ from {}: {} cancelled, what had arrived is removed",
                record.info.peer,
                files(record.info.files)
            );
            self.journal.write(&text);
        }
    }

    /// A resumable transfer this device sends lost its connection: it waits
    /// for the device to be connected again, and goes on where it stopped.
    fn interrupt(&mut self, id: u32) {
        let Some(outgoing) = self.outgoing.iter_mut().find(|o| o.id == id) else {
            return;
        };
        if outgoing.conn().is_none() {
            // waiting already
            return;
        }
        if let OutState::Streaming(_, task) = &outgoing.state {
            task.abort();
        }
        outgoing.state = OutState::Waiting;
        outgoing.interrupted = true;
        let (p, fingerprint) = (
            outgoing.progress.borrow().clone(),
            outgoing.fingerprint.clone(),
        );
        outgoing.progress.borrow_mut().current = None;
        let peer = self.name(&fingerprint);
        log::info!("sending files to {peer}: {LOST}, it goes on when {peer} is back");
        let text = format!(
            "↑ to {peer}: interrupted after {} of {} files ({} of {}), goes on when {peer} is back",
            p.files_done,
            p.files,
            human_size(p.bytes_done),
            human_size(p.bytes)
        );
        self.journal.write(&text);
        self.start_next();
    }

    /// A resumable transfer this device receives lost its connection: what
    /// arrived is kept, for when the sender offers the files again.
    fn receive_interrupted(
        &mut self,
        conn: u64,
        id: u32,
        peer: String,
        progress: Progress,
        kept: (String, PathBuf),
    ) {
        let incoming = self.incoming_info(conn, id, now_ms());
        let (uid, started_ms) = (incoming.uid, incoming.started_ms);
        self.incoming.remove(&(conn, id));
        log::info!("receiving files from {peer}: {LOST}, what arrived is kept to go on with");
        let text = format!(
            "↓ from {peer}: interrupted after {} of {} files ({} of {}), kept to go on with",
            progress.files_done,
            progress.files,
            human_size(progress.bytes_done),
            human_size(progress.bytes)
        );
        self.journal.write(&text);
        let mut info = info(
            uid,
            false,
            peer,
            &progress,
            TransferState::Interrupted,
            started_ms,
        );
        info.current = None;
        self.history.push(Record {
            info,
            paths: Vec::new(),
            dir: None,
            replaced: None,
            kept: Some(kept),
        });
    }

    fn cancel_outgoing(&mut self, id: u32) {
        let conn = self
            .outgoing
            .iter()
            .find(|o| o.id == id)
            .and_then(Outgoing::conn);
        if let Some(c) = conn.and_then(|c| self.conns.get(&c)) {
            let _ = c.writer.try_send(Frame::Cancel { id });
        }
        self.fail_outgoing(id, "cancelled".into());
    }

    fn cancel_all(&mut self) {
        let ids: Vec<u32> = self.outgoing.iter().map(|o| o.id).collect();
        for id in ids {
            self.cancel_outgoing(id);
        }
        for conn in self.conns.values() {
            if let Some(receive) = conn.receiving.borrow().as_ref() {
                let _ = conn.writer.try_send(Frame::Abort { id: receive.id });
            }
        }
        let interrupted: Vec<u64> = self
            .history
            .iter()
            .filter(|r| r.info.state == TransferState::Interrupted && r.kept.is_some())
            .map(|r| r.info.id)
            .collect();
        for uid in interrupted {
            self.cancel(uid);
        }
    }

    fn conn_name(&self, conn: u64) -> String {
        self.conns
            .get(&conn)
            .map(|c| self.name(&c.fingerprint))
            .unwrap_or_default()
    }

    fn received(&mut self, conn: u64, id: u32, paths: Vec<PathBuf>, progress: Progress) {
        let peer = self.conns.get(&conn).and_then(|c| {
            self.peers
                .values()
                .find(|p| p.fingerprint == c.fingerprint && p.initiated() == c.initiated)
                .map(|p| p.peer)
        });
        let name = self.conn_name(conn);
        log::info!("received {} file(s) from {name}", progress.files);
        let info = self.incoming_info(conn, id, now_ms());
        let (uid, started_ms, listing) =
            (info.uid, info.started_ms, std::mem::take(&mut info.listing));
        self.incoming.remove(&(conn, id));
        // the earlier received files leave the clipboard
        for record in self.history.iter_mut().filter(|r| r.ready()) {
            record.replaced.get_or_insert_with(Instant::now);
        }
        let key = files_key(&paths);
        self.clipboard.notify(ClipboardMsg::FilesReceived {
            peer,
            paths: paths.clone(),
            key,
        });
        self.finished(Finished {
            uid,
            sending: false,
            peer: name,
            progress,
            state: TransferState::Done,
            started_ms,
            listing,
            paths,
        });
    }

    /// records a finished transfer, for the user and in the log
    fn finished(&mut self, f: Finished) {
        let now = now_ms();
        let p = &f.progress;
        // gone on where it stopped: the speed of what came since then
        let (since, moved) = match p.resumed {
            Some((there, at)) => (at, p.bytes_done.saturating_sub(there)),
            None => (f.started_ms, p.bytes_done),
        };
        let seconds = now.saturating_sub(since) as f64 / 1000.0;
        let rate = || {
            let speed = (moved as f64 / seconds.max(0.001)) as u64;
            match p.resumed {
                Some((there, _)) => format!(
                    "in {seconds:.1} s ({}/s) going on at {}",
                    human_size(speed),
                    human_size(there)
                ),
                None => format!("in {seconds:.1} s ({}/s)", human_size(speed)),
            }
        };
        let dir = f
            .paths
            .first()
            .and_then(|p| p.parent())
            .map(Path::to_path_buf);
        let text = match (&f.state, f.sending) {
            (TransferState::Done, true) => format!(
                "↑ to {}: {}, {} sent {}",
                f.peer,
                files(p.files),
                human_size(p.bytes),
                rate()
            ),
            (TransferState::Done, false) => format!(
                "↓ from {}: {}, {} received {}, ready to paste (in {})",
                f.peer,
                files(p.files),
                human_size(p.bytes),
                rate(),
                dir.as_deref()
                    .map(Path::display)
                    .map(|d| d.to_string())
                    .unwrap_or_default()
            ),
            (TransferState::Failed(reason), sending) => format!(
                "{} {}: failed after {} of {} files: {reason}",
                if sending { "↑ to" } else { "↓ from" },
                f.peer,
                p.files_done,
                p.files
            ),
            _ => String::new(),
        };
        self.journal.write_with_files(&text, &f.listing);
        let mut info = info(f.uid, f.sending, f.peer, p, f.state, f.started_ms);
        info.finished_ms = Some(now);
        info.location = dir.as_ref().map(|d| d.display().to_string());
        self.history.push(Record {
            info,
            paths: f.paths,
            dir,
            replaced: None,
            kept: None,
        });
        // the oldest go first, but not received files waiting to be pasted
        // nor those waiting to go on
        while self.history.len() > MAX_HISTORY {
            match self
                .history
                .iter()
                .position(|r| !r.ready() && r.kept.is_none())
            {
                Some(oldest) => self.history.remove(oldest),
                None => break,
            };
        }
    }

    /// received files that were pasted (moved away) are done with, and so are
    /// those that left the clipboard a while ago
    fn watch_received(&mut self) {
        let mut retire = Vec::new();
        for (index, record) in self.history.iter().enumerate() {
            if !record.ready() {
                continue;
            }
            if record
                .paths
                .iter()
                .all(|p| fs::symlink_metadata(p).is_err())
            {
                retire.push((index, TransferState::Pasted));
            } else if record
                .replaced
                .is_some_and(|at| at.elapsed() > REPLACED_GRACE)
            {
                retire.push((index, TransferState::Removed));
            }
        }
        for (index, state) in retire {
            self.retire(index, state);
        }
    }

    /// removes the received files that were not pasted, and what arrived
    /// of interrupted transfers
    fn remove_received(&mut self) {
        let ready: Vec<usize> = (0..self.history.len())
            .filter(|&i| self.history[i].ready())
            .collect();
        for index in ready {
            self.retire(index, TransferState::Removed);
        }
        for record in &mut self.history {
            if let Some((tag, root)) = record.kept.take() {
                store::remove_partial(&root);
                // not taken when it comes back by itself
                self.refused.borrow_mut().insert(tag);
                record.info.state = TransferState::Removed;
                record.info.finished_ms = Some(now_ms());
            }
        }
    }

    /// removes the temporary folder of a received transfer
    fn retire(&mut self, index: usize, state: TransferState) {
        let record = &mut self.history[index];
        if let Some(dir) = record.dir.take() {
            if let Err(e) = fs::remove_dir_all(&dir) {
                if e.kind() != io::ErrorKind::NotFound {
                    log::warn!("could not remove the received files in {dir:?}: {e}");
                }
            }
        }
        let what = match state {
            TransferState::Pasted => "pasted, the temporary folder is removed",
            _ => "not pasted, removed from the temporary folder",
        };
        log::info!("files from {}: {what}", record.info.peer);
        let text = format!(
            "↓ from {}: {} {what}",
            record.info.peer,
            files(record.info.files)
        );
        record.info.state = state;
        record.info.location = None;
        self.journal.write(&text);
    }

    /// sends the state of the transfers if it changed
    fn report(&mut self) {
        let mut transfers: Vec<TransferInfo> = Vec::new();
        for outgoing in &self.outgoing {
            let state = match outgoing.state {
                OutState::Waiting if outgoing.interrupted => TransferState::Interrupted,
                OutState::Scanning | OutState::Waiting => TransferState::Waiting,
                _ => TransferState::Running,
            };
            transfers.push(info(
                outgoing.uid,
                true,
                self.name(&outgoing.fingerprint),
                &outgoing.progress.borrow(),
                state,
                outgoing.started_ms,
            ));
        }
        let receiving: Vec<(u64, String, Receive)> = self
            .conns
            .iter()
            .filter_map(|(&conn, c)| {
                let receive = c.receiving.borrow().clone()?;
                Some((conn, self.name(&c.fingerprint), receive))
            })
            .collect();
        for (conn, peer, receive) in receiving {
            let uid = self.incoming_info(conn, receive.id, receive.started_ms).uid;
            transfers.push(info(
                uid,
                false,
                peer,
                &receive.progress,
                TransferState::Running,
                receive.started_ms,
            ));
        }
        transfers.extend(self.history.iter().map(|r| r.info.clone()));
        if self.last_report.as_ref() != Some(&transfers) {
            let _ = self.reports.send(transfers.clone());
            self.last_report = Some(transfers);
        }
    }
}

/// a transfer that ended
struct Finished {
    uid: u64,
    sending: bool,
    peer: String,
    progress: Progress,
    state: TransferState,
    started_ms: u64,
    listing: Listing,
    /// received: the top level items
    paths: Vec<PathBuf>,
}

/// "1 file", "3 files"
fn files(n: u32) -> String {
    if n == 1 {
        "1 file".to_owned()
    } else {
        format!("{n} files")
    }
}

/// the files of a transfer with their sizes
fn listing(entries: &[Entry]) -> Listing {
    entries
        .iter()
        .filter(|e| !e.dir)
        .map(|e| (e.path.clone(), e.size))
        .collect()
}

fn info(
    id: u64,
    sending: bool,
    peer: String,
    progress: &Progress,
    state: TransferState,
    started_ms: u64,
) -> TransferInfo {
    TransferInfo {
        id,
        sending,
        peer,
        files: progress.files,
        files_done: progress.files_done,
        bytes: progress.bytes,
        bytes_done: progress.bytes_done,
        current: progress.current.clone(),
        state,
        started_ms: progress.resumed.map_or(started_ms, |(_, at)| at),
        finished_ms: None,
        location: None,
        resumed_bytes: progress.resumed.map(|(bytes, _)| bytes),
    }
}

/// what the TCP port needs to answer connections
#[derive(Clone)]
struct Accept {
    acceptor: TlsAcceptor,
    internal: Sender<Internal>,
    pairing: Pairing,
    arrangement: Arrangement,
    authorized: AuthorizedKeys,
    /// fingerprint of this device
    own: String,
    /// file channels are taken
    enabled: bool,
}

impl Accept {
    /// answers one connection: a file channel from an authorized device, or
    /// pairing
    async fn connection(self, tcp: TcpStream, addr: SocketAddr) {
        let handshake = async {
            let mut stream = self.acceptor.accept(tcp).await?;
            let magic = wire::read_any_magic(&mut stream).await?;
            Ok::<_, io::Error>((stream, magic))
        };
        let (mut stream, magic) = match timeout(HANDSHAKE_TIMEOUT, handshake).await {
            Ok(Ok(accepted)) => accepted,
            Ok(Err(e)) => return log::info!("connection from {addr} on the TCP port: {e}"),
            Err(_) => return log::info!("connection from {addr}: handshake timed out"),
        };
        let certs = stream.get_ref().1.peer_certificates();
        let Some(fingerprint) = tls::peer_fingerprint(certs) else {
            return;
        };
        if magic == pairing::MAGIC {
            let answer = async {
                stream.write_all(&pairing::MAGIC).await?;
                stream.flush().await?;
                self.pairing
                    .serve(&mut stream, &fingerprint, &self.own)
                    .await
            };
            if let Err(e) = timeout(HANDSHAKE_TIMEOUT, answer)
                .await
                .unwrap_or_else(|_| Err(io::Error::new(io::ErrorKind::TimedOut, "timed out")))
            {
                log::info!("pairing with {addr}: {e}");
            }
            return;
        }
        let authorized = self
            .authorized
            .read()
            .expect("lock")
            .contains_key(&fingerprint);
        if magic == arrangement::MAGIC {
            let answer = async {
                stream.write_all(&arrangement::MAGIC).await?;
                stream.flush().await?;
                self.arrangement
                    .serve(&mut stream, &fingerprint, authorized, addr)
                    .await
            };
            if let Err(e) = timeout(arrangement::SERVE_TIMEOUT, answer)
                .await
                .unwrap_or_else(|_| Err(io::Error::new(io::ErrorKind::TimedOut, "timed out")))
            {
                log::info!("arrangement with {addr}: {e}");
            }
            return;
        }
        if magic != wire::MAGIC {
            return log::info!("connection from {addr}: not RustInput");
        }
        if !authorized || !self.enabled {
            let why = if authorized {
                "file transfers are off"
            } else {
                "not authorized"
            };
            return log::info!("file channel from {addr} refused: {why}");
        }
        if wire::write_magic(&mut stream).await.is_ok() && stream.flush().await.is_ok() {
            let _ = self.internal.send(Internal::Connected {
                fingerprint,
                initiated: false,
                addr: None,
                stream: Box::new(stream.into()),
            });
        }
    }
}

/// accepts the file channels of the devices connecting to this one, and
/// pairing
async fn listen(port: u16, accept: Accept) {
    let listener = match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(listener) => listener,
        Err(e) => {
            log::warn!("file channel: can not listen on TCP port {port}: {e}");
            return;
        }
    };
    log::info!("file channel listening on TCP port {port}");
    loop {
        let (tcp, addr) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                log::warn!("file channel: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let _ = tcp.set_nodelay(true);
        spawn_local(accept.clone().connection(tcp, addr));
    }
}

/// opens the file channel to the device with `fingerprint` at `addr`
async fn connect(
    cert: Certificate,
    fingerprint: String,
    addr: SocketAddr,
    internal: Sender<Internal>,
) {
    let attempt = async {
        let tcp = timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connection timed out"))??;
        tcp.set_nodelay(true)?;
        let config = tls::client_config(&cert, fingerprint.clone()).map_err(io::Error::other)?;
        let handshake = async {
            let mut stream = TlsConnector::from(config)
                .connect(tls::server_name(), tcp)
                .await?;
            wire::write_magic(&mut stream).await?;
            wire::read_magic(&mut stream).await?;
            Ok::<_, io::Error>(stream)
        };
        timeout(HANDSHAKE_TIMEOUT, handshake)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "handshake timed out"))?
    };
    match attempt.await {
        Ok(stream) => {
            let _ = internal.send(Internal::Connected {
                fingerprint,
                initiated: true,
                addr: Some(addr),
                stream: Box::new(stream.into()),
            });
        }
        Err(e) => {
            log::debug!("file channel to {addr}: {e}");
            let _ = internal.send(Internal::ConnectFailed { fingerprint });
        }
    }
}

/// the name of the file of `entry`, without its folders
fn file_name(entry: &Entry) -> String {
    entry
        .path
        .rsplit('/')
        .next()
        .unwrap_or(&entry.path)
        .to_owned()
}

/// Where a resumable transfer goes on, from where the receiver asks (`at`):
/// there, if the last bytes it has of that file are those of this file, else
/// at the start of that file. A file that changed since it was copied ends
/// the transfer.
async fn resume_at(manifest: &Manifest, at: Position) -> Result<Position, String> {
    let files: Vec<(&Entry, &PathBuf)> = manifest
        .entries
        .iter()
        .filter(|e| !e.dir)
        .zip(&manifest.sources)
        .collect();
    let index = at.file as usize;
    let start = Position {
        file: at.file,
        ..Default::default()
    };
    if index > files.len() {
        return Err("the receiver asked for a file the transfer does not have".into());
    }
    let Some(&(entry, source)) = files.get(index) else {
        // everything arrived
        return Ok(start);
    };
    if at.offset == 0 || at.offset > entry.size {
        return Ok(start);
    }
    let name = file_name(entry);
    let (path, size, modified) = (source.clone(), entry.size, entry.modified);
    let same = tokio::task::spawn_blocking(move || {
        as_user(|| {
            let mut file = fs::File::open(&path)?;
            let metadata = file.metadata()?;
            if metadata.len() != size
                || (modified.is_some() && manifest::modified(&metadata) != modified)
            {
                return Ok(None);
            }
            Ok(Some(store::tail_hash(&mut file, at.offset)? == at.tail))
        })
    })
    .await
    .map_err(|e| e.to_string())?;
    match same {
        Ok(Some(true)) => Ok(Position {
            offset: at.offset,
            ..start
        }),
        Ok(Some(false)) => {
            log::info!("{name}: what the receiver has is not this file, it is sent again");
            Ok(start)
        }
        Ok(None) => Err(format!("{name} changed since it was copied")),
        Err(e) => Err(format!("{name}: {e}")),
    }
}

/// Sends the contents of the files of a transfer, one file after the
/// other, from `from` on (the start, unless a resumable transfer goes on).
async fn stream(
    id: u32,
    manifest: &Manifest,
    from: Position,
    writer: mpsc::Sender<Frame>,
    progress: Rc<RefCell<Progress>>,
) -> Result<(), String> {
    let files = manifest.entries.iter().filter(|e| !e.dir);
    let before = files
        .clone()
        .take(from.file as usize)
        .map(|e| e.size)
        .sum::<u64>()
        + from.offset;
    {
        let mut progress = progress.borrow_mut();
        progress.files_done = from.file;
        progress.bytes_done = before;
        progress.resumed = (before > 0).then(|| (before, now_ms()));
    }
    for (index, (entry, source)) in files.zip(&manifest.sources).enumerate() {
        if index < from.file as usize {
            continue;
        }
        let skip = if index == from.file as usize {
            from.offset
        } else {
            0
        };
        let name = file_name(entry);
        progress.borrow_mut().current = Some(name.clone());
        let path = source.clone();
        let mut file = tokio::task::spawn_blocking(move || as_user(|| std::fs::File::open(&path)))
            .await
            .map_err(|e| e.to_string())
            .and_then(|opened| opened.map_err(|e| format!("{name}: {e}")))
            .map(tokio::fs::File::from_std)?;
        if skip > 0 {
            file.seek(io::SeekFrom::Start(skip))
                .await
                .map_err(|e| format!("{name}: {e}"))?;
        }
        let mut remaining = entry.size - skip;
        while remaining > 0 {
            let mut chunk = vec![0; remaining.min(CHUNK_SIZE as u64) as usize];
            file.read_exact(&mut chunk).await.map_err(|e| {
                if e.kind() == io::ErrorKind::UnexpectedEof {
                    format!("{name} got smaller while it was sent")
                } else {
                    format!("{name}: {e}")
                }
            })?;
            let length = chunk.len() as u64;
            writer
                .send(Frame::Data { id, bytes: chunk })
                .await
                .map_err(|_| LOST.to_owned())?;
            remaining -= length;
            progress.borrow_mut().bytes_done += length;
        }
        progress.borrow_mut().files_done += 1;
    }
    progress.borrow_mut().current = None;
    Ok(())
}

/// writes the frames queued for a connection
async fn write_loop(
    conn: u64,
    mut writer: WriteHalf<Stream>,
    mut frames: mpsc::Receiver<Frame>,
    internal: Sender<Internal>,
) {
    while let Some(frame) = frames.recv().await {
        if let Err(e) = wire::write_frame(&mut writer, &frame).await {
            log::debug!("file channel: {e}");
            break;
        }
        // flush once nothing else is waiting
        if frames.is_empty() && writer.flush().await.is_err() {
            break;
        }
    }
    let _ = writer.shutdown().await;
    let _ = internal.send(Internal::Closed { conn });
}

/// a transfer the reader of a connection is receiving
struct Incoming {
    id: u32,
    started_ms: u64,
    receiving: store::Receiving,
    /// resumable: its key, taken while it is received
    busy: Option<Busy>,
    /// went on where it stopped: the bytes there already, and when
    resumed: Option<(u64, u64)>,
}

impl Incoming {
    fn progress(&self) -> Receive {
        let r = &self.receiving;
        Receive {
            id: self.id,
            started_ms: self.started_ms,
            progress: Progress {
                files: r.files as u32,
                files_done: r.files_done as u32,
                bytes: r.bytes,
                bytes_done: r.bytes_done,
                current: r.current_name(),
                resumed: self.resumed,
            },
            kept: self
                .busy
                .as_ref()
                .map(|b| (b.tag.clone(), self.receiving.partial())),
        }
    }

    /// gives the transfer up: removes what arrived
    async fn discard(self) {
        self.receiving.discard().await;
    }
}

/// what the reader of a connection needs besides the connection
struct Reader {
    conn: u64,
    /// fingerprint of the peer
    fingerprint: String,
    writer: mpsc::Sender<Frame>,
    receiving: Rc<RefCell<Option<Receive>>>,
    received_dir: Option<PathBuf>,
    internal: Sender<Internal>,
    busy: Rc<RefCell<HashSet<String>>>,
    refused: Rc<RefCell<HashSet<String>>>,
    /// resumable transfers received in this run, see [`ALREADY_RECEIVED`]
    received: Rc<RefCell<HashSet<String>>>,
}

impl Reader {
    /// the answer to an offer: the folder to receive it into, or why not;
    /// `resumable`: its key, and whether it goes on by itself after its
    /// connection was lost
    async fn offer(
        &self,
        id: u32,
        resumable: Option<(String, bool)>,
        entries: Vec<Entry>,
    ) -> (Frame, Option<Incoming>) {
        let decline = |reason: String| Frame::Decline { id, reason };
        let Some(dir) = &self.received_dir else {
            return (decline("no folder to receive files into".into()), None);
        };
        let files = listing(&entries);
        let started_ms = now_ms();
        let (incoming, reply, resumed, tag) = match resumable {
            None => match store::Receiving::start(dir, id, entries).await {
                Ok(receiving) => {
                    let incoming = Incoming {
                        id,
                        started_ms,
                        receiving,
                        busy: None,
                        resumed: None,
                    };
                    (incoming, Frame::Accept { id }, None, None)
                }
                Err(e) => return (decline(e.to_string()), None),
            },
            Some((key, continued)) => {
                let tag = format!("{}/{key}", self.fingerprint);
                // cancelled here while interrupted: not when it goes on by
                // itself, but when the user sends the files again
                if continued && self.refused.borrow().contains(&tag) {
                    return (decline("the transfer was cancelled here".into()), None);
                }
                self.refused.borrow_mut().remove(&tag);
                if continued && self.received.borrow().contains(&tag) {
                    return (decline(ALREADY_RECEIVED.into()), None);
                }
                if self.busy.borrow().contains(&tag) {
                    let _ = self.internal.send(Internal::Preempt {
                        conn: self.conn,
                        tag: tag.clone(),
                    });
                    let until = tokio::time::Instant::now() + PREEMPT_WAIT;
                    while self.busy.borrow().contains(&tag) && tokio::time::Instant::now() < until {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
                if !self.busy.borrow_mut().insert(tag.clone()) {
                    return (
                        decline("these files are coming over another connection".into()),
                        None,
                    );
                }
                let busy = Busy {
                    keys: self.busy.clone(),
                    tag: tag.clone(),
                };
                match store::Receiving::resumable(dir, id, &self.fingerprint, &key, entries).await {
                    Ok((receiving, at)) => {
                        let resumed = (receiving.bytes_done > 0).then_some(receiving.bytes_done);
                        let incoming = Incoming {
                            id,
                            started_ms,
                            receiving,
                            busy: Some(busy),
                            resumed: None,
                        };
                        (incoming, Frame::AcceptAt { id, at }, resumed, Some(tag))
                    }
                    Err(e) => return (decline(e.to_string()), None),
                }
            }
        };
        let _ = self.internal.send(Internal::Receiving {
            conn: self.conn,
            id,
            started_ms,
            listing: files,
            tag,
            resumed,
        });
        (reply, Some(incoming))
    }

    /// the transfer failed here: it goes, and the sender is told
    async fn failed(&self, current: Incoming, reason: String) {
        let progress = current.progress().progress;
        let id = current.id;
        current.discard().await;
        let _ = self.writer.send(Frame::Abort { id }).await;
        let _ = self.internal.send(Internal::ReceiveFailed {
            conn: self.conn,
            id,
            reason,
            progress,
        });
    }
}

/// reads the frames of a connection: receives the transfers of the peer and
/// passes the answers about our own transfers on
async fn read_loop(r: Reader, mut reader: ReadHalf<Stream>) {
    let mut current: Option<Incoming> = None;
    loop {
        let frame = match wire::read_frame(&mut reader).await {
            Ok(frame) => frame,
            Err(e) => {
                log::debug!("file channel: {e}");
                break;
            }
        };
        match frame {
            Frame::Offer { id, entries } => {
                if let Some(previous) = current.take() {
                    if !previous.receiving.kept() {
                        previous.discard().await;
                    }
                }
                let (reply, incoming) = r.offer(id, None, entries).await;
                current = incoming;
                if r.writer.send(reply).await.is_err() {
                    break;
                }
            }
            Frame::OfferResumable {
                id,
                key,
                entries,
                continued,
            } => {
                if let Some(previous) = current.take() {
                    if !previous.receiving.kept() {
                        previous.discard().await;
                    }
                }
                let (reply, incoming) = r.offer(id, Some((key, continued)), entries).await;
                current = incoming;
                if r.writer.send(reply).await.is_err() {
                    break;
                }
            }
            Frame::Start { id, file, offset } => {
                let Some(c) = current.as_mut().filter(|c| c.id == id) else {
                    continue;
                };
                if offset > 0 {
                    log::info!("file channel: going on at file {file}, byte {offset}");
                }
                match c.receiving.begin(file, offset).await {
                    Ok(()) => {
                        // what was there already, to tell the speed of the rest
                        let there = c.receiving.bytes_done;
                        c.resumed = (there > 0).then_some((there, now_ms()));
                    }
                    Err(e) => {
                        let c = current.take().expect("current transfer");
                        r.failed(c, e.to_string()).await;
                    }
                }
            }
            Frame::Data { id, bytes } => {
                let Some(c) = current.as_mut().filter(|c| c.id == id) else {
                    continue;
                };
                if let Err(e) = c.receiving.write(&bytes).await {
                    let c = current.take().expect("current transfer");
                    r.failed(c, e.to_string()).await;
                }
            }
            Frame::Cancel { id } => {
                if current.as_ref().is_some_and(|c| c.id == id) {
                    let c = current.take().expect("current transfer");
                    let progress = c.progress().progress;
                    c.discard().await;
                    let _ = r.internal.send(Internal::ReceiveFailed {
                        conn: r.conn,
                        id,
                        reason: "cancelled".into(),
                        progress,
                    });
                }
            }
            frame => {
                let _ = r.internal.send(Internal::Frame {
                    conn: r.conn,
                    frame,
                });
            }
        }
        // a transfer is complete once its last byte (or its offer, if it
        // only has empty files) arrived
        if current.as_ref().is_some_and(|c| c.receiving.is_complete()) {
            let c = current.take().expect("current transfer");
            let progress = c.progress().progress;
            let id = c.id;
            if let Some(busy) = &c.busy {
                r.received.borrow_mut().insert(busy.tag.clone());
            }
            let paths = c.receiving.finish();
            // here before the answer: also when it does not get there, and
            // before this task can be stopped while it waits to send it
            *r.receiving.borrow_mut() = None;
            let _ = r.internal.send(Internal::Received {
                conn: r.conn,
                id,
                paths,
                progress,
            });
            if r.writer.send(Frame::Complete { id }).await.is_err() {
                break;
            }
        }
        *r.receiving.borrow_mut() = current.as_ref().map(Incoming::progress);
    }
    // a resumable transfer stays for when it goes on
    if let Some(c) = current.take() {
        if !c.receiving.kept() {
            c.discard().await;
        }
    }
    let _ = r.internal.send(Internal::Closed { conn: r.conn });
}

/// Runs `f` with the rights of the user logged on: when RustInput runs as
/// SYSTEM, the files it sends are listed and opened as that user, so no
/// program can have it send what the user may not read.
fn as_user<T>(f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    #[cfg(windows)]
    {
        crate::windows_system::as_session_user(f)
    }
    #[cfg(not(windows))]
    {
        f()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn at(file: u32, offset: u64, tail: &[u8]) -> Position {
        Position {
            file,
            offset,
            tail: if tail.is_empty() {
                [0; wire::TAIL_HASH]
            } else {
                Sha256::digest(tail).into()
            },
        }
    }

    #[tokio::test]
    async fn the_sender_goes_on_where_the_receiver_has_the_same_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("Pasta");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("a.bin"), b"0123456789").unwrap();
        fs::write(folder.join("b.bin"), b"abcdef").unwrap();
        let manifest = manifest::scan(std::slice::from_ref(&folder)).unwrap();
        // the receiver has a.bin and "abc" of b.bin
        let from = resume_at(&manifest, at(1, 3, b"abc")).await.unwrap();
        assert_eq!((from.file, from.offset), (1, 3));
        // what it has is not this file: b.bin again
        let from = resume_at(&manifest, at(1, 3, b"xyz")).await.unwrap();
        assert_eq!((from.file, from.offset), (1, 0));
        // longer than the file, or nothing of it
        let from = resume_at(&manifest, at(1, 99, b"abc")).await.unwrap();
        assert_eq!((from.file, from.offset), (1, 0));
        let from = resume_at(&manifest, at(0, 0, b"")).await.unwrap();
        assert_eq!(from, Position::default());
        // everything arrived
        let from = resume_at(&manifest, at(2, 0, b"")).await.unwrap();
        assert_eq!((from.file, from.offset), (2, 0));
        assert!(resume_at(&manifest, at(3, 0, b"")).await.is_err());
        // a file that changed since it was copied ends the transfer
        fs::write(folder.join("b.bin"), b"abcdefgh").unwrap();
        let changed = resume_at(&manifest, at(1, 3, b"abc")).await.unwrap_err();
        assert!(changed.contains("b.bin changed"), "{changed}");
    }

    #[tokio::test]
    async fn the_rest_is_streamed_from_the_position() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.bin"), b"0123456789").unwrap();
        fs::write(dir.path().join("b.bin"), b"abcdef").unwrap();
        let manifest =
            manifest::scan(&[dir.path().join("a.bin"), dir.path().join("b.bin")]).unwrap();
        let (writer, mut frames) = mpsc::channel(16);
        let progress = Rc::new(RefCell::new(Progress {
            files: 2,
            bytes: 16,
            ..Default::default()
        }));
        let from = Position {
            file: 1,
            offset: 2,
            ..Default::default()
        };
        tokio::task::LocalSet::new()
            .run_until(stream(7, &manifest, from, writer, progress.clone()))
            .await
            .unwrap();
        let mut data = Vec::new();
        while let Ok(frame) = frames.try_recv() {
            match frame {
                Frame::Data { id: 7, bytes } => data.extend(bytes),
                frame => panic!("{frame:?}"),
            }
        }
        assert_eq!(data, b"cdef");
        let progress = progress.borrow();
        assert_eq!((progress.files_done, progress.bytes_done), (2, 16));
        assert_eq!(progress.resumed.map(|(bytes, _)| bytes), Some(12));
    }

    #[test]
    fn the_key_follows_the_files() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.bin");
        fs::write(&file, b"0123").unwrap();
        let key = manifest::resume_key(&manifest::scan(std::slice::from_ref(&file)).unwrap());
        assert_eq!(key.len(), 64);
        assert_eq!(
            key,
            manifest::resume_key(&manifest::scan(std::slice::from_ref(&file)).unwrap())
        );
        fs::write(&file, b"01234").unwrap();
        assert_ne!(key, manifest::resume_key(&manifest::scan(&[file]).unwrap()));
    }
}
