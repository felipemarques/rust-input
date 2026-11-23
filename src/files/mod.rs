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
    io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf},
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
use wire::{CHUNK_SIZE, Entry, Frame};

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
}

/// the transfer the reader of a connection is receiving
#[derive(Clone, Debug)]
struct Receive {
    id: u32,
    started_ms: u64,
    progress: Progress,
}

struct Conn {
    fingerprint: String,
    /// this device opened the connection
    initiated: bool,
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
    manifest: Option<Manifest>,
    listing: Listing,
    started_ms: u64,
    progress: Rc<RefCell<Progress>>,
    state: OutState,
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
                // ones finish or fail with their connection
                let waiting: Vec<u32> = self
                    .outgoing
                    .iter()
                    .filter(|o| o.peer == peer && o.conn().is_none())
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
        let (fingerprint, initiated) = (info.fingerprint.clone(), info.initiated());
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
                stream,
            } => self.connected(fingerprint, initiated, *stream),
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
                        o.manifest = Some(manifest);
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
                    self.send_frame(id, |id| Frame::Cancel { id }).await;
                    self.fail_outgoing(id, reason);
                }
            },
            Internal::Receiving {
                conn,
                id,
                started_ms,
                listing,
            } => {
                let bytes = listing.iter().map(|(_, size)| size).sum();
                let text = format!(
                    "↓ from {}: receiving {}, {}",
                    self.conn_name(conn),
                    files(listing.len() as u32),
                    human_size(bytes)
                );
                self.journal.write(&text);
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

    fn connected(&mut self, fingerprint: String, initiated: bool, stream: Stream) {
        if initiated {
            self.connecting.remove(&fingerprint);
        }
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
            conn,
            reader,
            frames_tx.clone(),
            receiving.clone(),
            self.received_dir.clone(),
            self.internal_tx.clone(),
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
        let running: Vec<u32> = self
            .outgoing
            .iter()
            .filter(|o| o.conn() == Some(conn))
            .map(|o| o.id)
            .collect();
        for id in running {
            self.fail_outgoing(id, "the connection was lost".into());
        }
        if let Some(receive) = closed.receiving.borrow_mut().take() {
            let peer = self.name(&closed.fingerprint);
            self.receive_failed(
                conn,
                receive.id,
                peer,
                receive.progress,
                "the connection was lost".into(),
            );
        }
        self.incoming.retain(|(c, _), _| *c != conn);
        if closed.initiated {
            self.retry_later(closed.fingerprint.clone());
        }
    }

    /// connects again in a while, if the device still takes file transfers
    fn retry_later(&self, fingerprint: String) {
        let wanted = self
            .peers
            .values()
            .any(|p| p.fingerprint == fingerprint && p.connect_to.is_some());
        if !wanted {
            return;
        }
        let internal = self.internal_tx.clone();
        spawn_local(async move {
            tokio::time::sleep(RECONNECT_DELAY).await;
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
            Frame::Accept { .. } => {
                let Some(writer) = self.conns.get(&conn).map(|c| c.writer.clone()) else {
                    return;
                };
                let outgoing = &mut self.outgoing[index];
                let Some(manifest) = outgoing.manifest.take() else {
                    return;
                };
                let (progress, internal) = (outgoing.progress.clone(), self.internal_tx.clone());
                let task = spawn_local(async move {
                    let result = stream(id, manifest, writer, progress).await;
                    let _ = internal.send(Internal::Streamed { id, result });
                });
                outgoing.state = OutState::Streaming(conn, task);
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
            Frame::Complete { .. } => {
                let outgoing = self.outgoing.remove(index);
                let peer = self.name(&outgoing.fingerprint);
                let progress = outgoing.progress.borrow().clone();
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
            frame => log::debug!("file channel: unexpected {frame:?}"),
        }
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
        let mut started = HashSet::new();
        for outgoing in &mut self.outgoing {
            if !matches!(outgoing.state, OutState::Waiting) {
                continue;
            }
            let Some((&conn, c)) = self.conns.iter().find(|(_, c)| {
                c.fingerprint == outgoing.fingerprint && c.initiated == outgoing.initiated
            }) else {
                continue;
            };
            if busy.contains(&conn) || !started.insert(conn) {
                continue;
            }
            let Some(manifest) = &outgoing.manifest else {
                continue;
            };
            let offer = Frame::Offer {
                id: outgoing.id,
                entries: manifest.entries.clone(),
            };
            // the queue has room: nothing else is offered on this connection
            if c.writer.try_send(offer).is_ok() {
                outgoing.state = OutState::Offered(conn);
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
        }
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
        let seconds = now.saturating_sub(f.started_ms) as f64 / 1000.0;
        let p = &f.progress;
        let rate = || {
            let speed = (p.bytes_done as f64 / seconds.max(0.001)) as u64;
            format!("in {seconds:.1} s ({}/s)", human_size(speed))
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
        });
        // the oldest go first, but not received files waiting to be pasted
        while self.history.len() > MAX_HISTORY {
            match self.history.iter().position(|r| !r.ready()) {
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

    /// removes the received files that were not pasted
    fn remove_received(&mut self) {
        let ready: Vec<usize> = (0..self.history.len())
            .filter(|&i| self.history[i].ready())
            .collect();
        for index in ready {
            self.retire(index, TransferState::Removed);
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
        started_ms,
        finished_ms: None,
        location: None,
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
            if let Err(e) = timeout(HANDSHAKE_TIMEOUT, answer)
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
                stream: Box::new(stream.into()),
            });
        }
        Err(e) => {
            log::debug!("file channel to {addr}: {e}");
            let _ = internal.send(Internal::ConnectFailed { fingerprint });
        }
    }
}

/// sends the contents of the files of a transfer, one file after the other
async fn stream(
    id: u32,
    manifest: Manifest,
    writer: mpsc::Sender<Frame>,
    progress: Rc<RefCell<Progress>>,
) -> Result<(), String> {
    let files = manifest.entries.iter().filter(|e| !e.dir);
    for (entry, source) in files.zip(&manifest.sources) {
        let name = entry
            .path
            .rsplit('/')
            .next()
            .unwrap_or(&entry.path)
            .to_owned();
        progress.borrow_mut().current = Some(name.clone());
        let path = source.clone();
        let mut file = tokio::task::spawn_blocking(move || as_user(|| std::fs::File::open(&path)))
            .await
            .map_err(|e| e.to_string())
            .and_then(|opened| opened.map_err(|e| format!("{name}: {e}")))
            .map(tokio::fs::File::from_std)?;
        let mut remaining = entry.size;
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
                .map_err(|_| "the connection was lost".to_owned())?;
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

/// reads the frames of a connection: receives the transfers of the peer and
/// passes the answers about our own transfers on
async fn read_loop(
    conn: u64,
    mut reader: ReadHalf<Stream>,
    writer: mpsc::Sender<Frame>,
    receiving: Rc<RefCell<Option<Receive>>>,
    received_dir: Option<PathBuf>,
    internal: Sender<Internal>,
) {
    let mut current: Option<(u32, u64, store::Receiving)> = None;
    let progress_of = |id: u32, started_ms: u64, r: &store::Receiving| Receive {
        id,
        started_ms,
        progress: Progress {
            files: r.files as u32,
            files_done: r.files_done as u32,
            bytes: r.bytes,
            bytes_done: r.bytes_done,
            current: r.current_name(),
        },
    };
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
                if let Some((_, _, previous)) = current.take() {
                    previous.discard().await;
                }
                let files = listing(&entries);
                let started_ms = now_ms();
                let reply = match &received_dir {
                    None => Frame::Decline {
                        id,
                        reason: "no folder to receive files into".into(),
                    },
                    Some(dir) => match store::Receiving::start(dir, id, entries).await {
                        Ok(receiving) => {
                            current = Some((id, started_ms, receiving));
                            let _ = internal.send(Internal::Receiving {
                                conn,
                                id,
                                started_ms,
                                listing: files,
                            });
                            Frame::Accept { id }
                        }
                        Err(e) => Frame::Decline {
                            id,
                            reason: e.to_string(),
                        },
                    },
                };
                if writer.send(reply).await.is_err() {
                    break;
                }
            }
            Frame::Data { id, bytes } => {
                let Some((current_id, _, receiving)) = current.as_mut() else {
                    continue;
                };
                if *current_id != id {
                    continue;
                }
                if let Err(e) = receiving.write(&bytes).await {
                    let (id, started_ms, receiving) = current.take().expect("current transfer");
                    let progress = progress_of(id, started_ms, &receiving).progress;
                    receiving.discard().await;
                    let _ = writer.send(Frame::Abort { id }).await;
                    let _ = internal.send(Internal::ReceiveFailed {
                        conn,
                        id,
                        reason: e.to_string(),
                        progress,
                    });
                }
            }
            Frame::Cancel { id } => {
                if current.as_ref().is_some_and(|(c, _, _)| *c == id) {
                    let (id, started_ms, receiving) = current.take().expect("current transfer");
                    let progress = progress_of(id, started_ms, &receiving).progress;
                    receiving.discard().await;
                    let _ = internal.send(Internal::ReceiveFailed {
                        conn,
                        id,
                        reason: "cancelled".into(),
                        progress,
                    });
                }
            }
            frame => {
                let _ = internal.send(Internal::Frame { conn, frame });
            }
        }
        // a transfer is complete once its last byte (or its offer, if it
        // only has empty files) arrived
        if current.as_ref().is_some_and(|(_, _, r)| r.is_complete()) {
            let (id, started_ms, receiving) = current.take().expect("current transfer");
            let progress = progress_of(id, started_ms, &receiving).progress;
            let paths = receiving.finish();
            if writer.send(Frame::Complete { id }).await.is_err() {
                break;
            }
            let _ = internal.send(Internal::Received {
                conn,
                id,
                paths,
                progress,
            });
        }
        *receiving.borrow_mut() = current
            .as_ref()
            .map(|(id, started_ms, r)| progress_of(*id, *started_ms, r));
    }
    if let Some((_, _, receiving)) = current.take() {
        receiving.discard().await;
    }
    let _ = internal.send(Internal::Closed { conn });
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
