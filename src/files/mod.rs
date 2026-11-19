//! File transfers between devices.
//!
//! When files are copied and the pointer moves to another device, they are
//! sent there over a channel of their own: TLS over TCP, on the same port
//! number as the input channel, so a large transfer does not hold up mouse
//! and keyboard. The connecting side of the input channel opens it as soon
//! as the peer announced the feature, and both directions share it. Both
//! sides authenticate with the certificate of the input channel (see
//! [`tls`]).
//!
//! Transfers to a device run one after the other and the files of a
//! transfer one by one. Received files land in a folder of their own (see
//! [`store`]) and the clipboard then points at them, so pasting them in a file
//! manager puts them wherever the user pastes.

mod manifest;
mod store;
mod tls;
mod wire;

pub(crate) use manifest::files_key;
pub(crate) use tls::AuthorizedKeys;

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    io,
    net::SocketAddr,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

use local_channel::mpsc::{Receiver, Sender, channel};
use rustinput_ipc::{TransferInfo, TransferState};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    task::{JoinHandle, spawn_local},
    time::timeout,
};
use tokio_rustls::{TlsAcceptor, TlsConnector, TlsStream};
use webrtc_dtls::crypto::Certificate;

use crate::clipboard::{ClipboardHandle, ClipboardMsg, Peer};
use manifest::Manifest;
use wire::{CHUNK_SIZE, Frame};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const RECONNECT_DELAY: Duration = Duration::from_secs(5);
const REPORT_INTERVAL: Duration = Duration::from_millis(250);
/// finished transfers stay visible this long
const SHOW_FINISHED: Duration = Duration::from_secs(30);
/// frames waiting to be written on a connection, data has to wait for room
const WRITE_QUEUE: usize = 8;

type Stream = TlsStream<TcpStream>;

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
    CancelAll,
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

    pub(crate) fn cancel_all(&self) {
        self.request(Request::CancelAll);
    }

    pub(crate) fn change_port(&self, port: u16) {
        self.request(Request::ChangePort(port));
    }
}

/// Starts the file transfers. Whenever the transfers change, their state is
/// sent to `reports`.
pub(crate) fn spawn(
    cert: Certificate,
    authorized: AuthorizedKeys,
    port: u16,
    clipboard: ClipboardHandle,
    reports: Sender<Vec<TransferInfo>>,
) -> FilesHandle {
    let (tx, requests) = channel();
    let (internal_tx, internal_rx) = channel();
    let received_dir = store::received_dir();
    match &received_dir {
        Some(dir) => store::cleanup(dir, store::KEEP_TRANSFERS),
        None => log::warn!("no folder for received files, they will be refused"),
    }
    let acceptor = match tls::server_config(&cert, authorized.clone()) {
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
        received_dir,
        peers: HashMap::new(),
        conns: HashMap::new(),
        connecting: HashSet::new(),
        outgoing: Vec::new(),
        finished: Vec::new(),
        next_transfer: 0,
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
    id: u32,
    peer: Peer,
    fingerprint: String,
    initiated: bool,
    manifest: Option<Manifest>,
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
    Received {
        conn: u64,
        paths: Vec<PathBuf>,
        progress: Progress,
    },
    ReceiveFailed {
        conn: u64,
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
    received_dir: Option<PathBuf>,
    peers: HashMap<Peer, PeerInfo>,
    conns: HashMap<u64, Conn>,
    /// fingerprints a connection is being opened to
    connecting: HashSet<String>,
    outgoing: Vec<Outgoing>,
    finished: Vec<(Instant, TransferInfo)>,
    next_transfer: u32,
    next_conn: u64,
    last_report: Option<Vec<TransferInfo>>,
}

impl Task {
    async fn run(mut self) {
        let mut report = tokio::time::interval(REPORT_INTERVAL);
        report.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                request = self.requests.recv() => match request {
                    Some(request) => self.request(request),
                    None => break,
                },
                Some(event) = self.internal_rx.recv() => self.internal(event).await,
                _ = report.tick() => self.report(),
            }
        }
    }

    fn listen(&mut self, port: u16) {
        if let Some(listener) = self.listener.take() {
            listener.abort();
        }
        let Some(acceptor) = self.acceptor.clone() else {
            return;
        };
        let internal = self.internal_tx.clone();
        self.listener = Some(spawn_local(listen(port, acceptor, internal)));
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
            Request::CancelAll => self.cancel_all(),
            Request::ChangePort(port) => self.listen(port),
        }
    }

    fn send(&mut self, peer: Peer, paths: Vec<PathBuf>) {
        let Some(info) = self.peers.get(&peer) else {
            log::info!("{peer} does not take file transfers, the copied files stay here");
            return;
        };
        let id = self.next_transfer;
        self.next_transfer = self.next_transfer.wrapping_add(1);
        log::info!("sending {} copied item(s) to {peer}", paths.len());
        self.outgoing.push(Outgoing {
            id,
            peer,
            fingerprint: info.fingerprint.clone(),
            initiated: info.initiated(),
            manifest: None,
            progress: Default::default(),
            state: OutState::Scanning,
        });
        // listing large folders takes a while, the input must not wait for it
        let internal = self.internal_tx.clone();
        spawn_local(async move {
            let manifest = tokio::task::spawn_blocking(move || manifest::scan(&paths))
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
                    if let Some(o) = self.outgoing.iter_mut().find(|o| o.id == id) {
                        *o.progress.borrow_mut() = Progress {
                            files: manifest.files() as u32,
                            bytes: manifest.bytes(),
                            ..Default::default()
                        };
                        o.manifest = Some(manifest);
                        o.state = OutState::Waiting;
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
            Internal::Received {
                conn,
                paths,
                progress,
            } => self.received(conn, paths, progress),
            Internal::ReceiveFailed {
                conn,
                reason,
                progress,
            } => {
                let peer = self.conn_name(conn);
                log::warn!("receiving files from {peer} failed: {reason}");
                self.finish(false, peer, progress, TransferState::Failed(reason));
            }
            Internal::Closed { conn } => self.closed(conn),
        }
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
            self.finish(
                false,
                peer,
                receive.progress,
                TransferState::Failed("the connection was lost".into()),
            );
        }
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
                self.finish(true, peer, progress, TransferState::Done);
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
        self.finish(true, peer, progress, TransferState::Failed(reason));
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

    fn cancel_all(&mut self) {
        let ids: Vec<u32> = self.outgoing.iter().map(|o| o.id).collect();
        for id in ids {
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
        // the sender answers with Cancel, then the reader discards the files
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

    fn received(&mut self, conn: u64, paths: Vec<PathBuf>, progress: Progress) {
        let peer = self.conns.get(&conn).and_then(|c| {
            self.peers
                .values()
                .find(|p| p.fingerprint == c.fingerprint && p.initiated() == c.initiated)
                .map(|p| p.peer)
        });
        let name = self.conn_name(conn);
        log::info!("received {} file(s) from {name}", progress.files);
        let key = files_key(&paths);
        self.clipboard
            .notify(ClipboardMsg::FilesReceived { peer, paths, key });
        if let Some(dir) = &self.received_dir {
            store::cleanup(dir, store::KEEP_TRANSFERS);
        }
        self.finish(false, name, progress, TransferState::Done);
    }

    fn finish(&mut self, sending: bool, peer: String, progress: Progress, state: TransferState) {
        let id = self.finished.last().map_or(0, |(_, t)| t.id) + 1;
        let info = info(id, sending, peer, &progress, state);
        self.finished.push((Instant::now(), info));
    }

    /// sends the state of the transfers if it changed
    fn report(&mut self) {
        self.finished.retain(|(at, _)| at.elapsed() < SHOW_FINISHED);
        let mut transfers: Vec<TransferInfo> = Vec::new();
        // running ones get ids above the finished ones, which stay stable
        let base = 1 << 32;
        for outgoing in &self.outgoing {
            let state = match outgoing.state {
                OutState::Scanning | OutState::Waiting => TransferState::Waiting,
                _ => TransferState::Running,
            };
            transfers.push(info(
                base + outgoing.id as u64,
                true,
                self.name(&outgoing.fingerprint),
                &outgoing.progress.borrow(),
                state,
            ));
        }
        for (&conn, c) in &self.conns {
            if let Some(receive) = c.receiving.borrow().as_ref() {
                transfers.push(info(
                    (2 << 32) + (conn << 16) + receive.id as u64,
                    false,
                    self.name(&c.fingerprint),
                    &receive.progress,
                    TransferState::Running,
                ));
            }
        }
        transfers.extend(self.finished.iter().map(|(_, t)| t.clone()));
        if self.last_report.as_ref() != Some(&transfers) {
            let _ = self.reports.send(transfers.clone());
            self.last_report = Some(transfers);
        }
    }
}

fn info(
    id: u64,
    sending: bool,
    peer: String,
    progress: &Progress,
    state: TransferState,
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
    }
}

/// accepts the file channels of the devices connecting to this one
async fn listen(port: u16, acceptor: TlsAcceptor, internal: Sender<Internal>) {
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
        let (acceptor, internal) = (acceptor.clone(), internal.clone());
        spawn_local(async move {
            let handshake = async {
                let mut stream = acceptor.accept(tcp).await?;
                wire::write_magic(&mut stream).await?;
                wire::read_magic(&mut stream).await?;
                Ok::<_, io::Error>(stream)
            };
            match timeout(HANDSHAKE_TIMEOUT, handshake).await {
                Ok(Ok(stream)) => {
                    let certs = stream.get_ref().1.peer_certificates();
                    if let Some(fingerprint) = tls::peer_fingerprint(certs) {
                        let _ = internal.send(Internal::Connected {
                            fingerprint,
                            initiated: false,
                            stream: Box::new(stream.into()),
                        });
                    }
                }
                Ok(Err(e)) => log::info!("file channel from {addr}: {e}"),
                Err(_) => log::info!("file channel from {addr}: handshake timed out"),
            }
        });
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
        let mut file = tokio::fs::File::open(source)
            .await
            .map_err(|e| format!("{name}: {e}"))?;
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
    let mut current: Option<(u32, store::Receiving)> = None;
    let progress_of = |id: u32, r: &store::Receiving| Receive {
        id,
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
                if let Some((_, previous)) = current.take() {
                    previous.discard().await;
                }
                let reply = match &received_dir {
                    None => Frame::Decline {
                        id,
                        reason: "no folder to receive files into".into(),
                    },
                    Some(dir) => match store::Receiving::start(dir, id, entries).await {
                        Ok(receiving) => {
                            current = Some((id, receiving));
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
                let Some((current_id, receiving)) = current.as_mut() else {
                    continue;
                };
                if *current_id != id {
                    continue;
                }
                if let Err(e) = receiving.write(&bytes).await {
                    let (id, receiving) = current.take().expect("current transfer");
                    let progress = progress_of(id, &receiving).progress;
                    receiving.discard().await;
                    let _ = writer.send(Frame::Abort { id }).await;
                    let _ = internal.send(Internal::ReceiveFailed {
                        conn,
                        reason: e.to_string(),
                        progress,
                    });
                }
            }
            Frame::Cancel { id } => {
                if current.as_ref().is_some_and(|(c, _)| *c == id) {
                    let (id, receiving) = current.take().expect("current transfer");
                    let progress = progress_of(id, &receiving).progress;
                    receiving.discard().await;
                    let _ = internal.send(Internal::ReceiveFailed {
                        conn,
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
        if current.as_ref().is_some_and(|(_, r)| r.is_complete()) {
            let (id, receiving) = current.take().expect("current transfer");
            let progress = progress_of(id, &receiving).progress;
            let paths = receiving.finish();
            if writer.send(Frame::Complete { id }).await.is_err() {
                break;
            }
            let _ = internal.send(Internal::Received {
                conn,
                paths,
                progress,
            });
        }
        *receiving.borrow_mut() = current.as_ref().map(|(id, r)| progress_of(*id, r));
    }
    if let Some((_, receiving)) = current.take() {
        receiving.discard().await;
    }
    let _ = internal.send(Internal::Closed { conn });
}
