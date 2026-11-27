//! Clipboard synchronization.
//!
//! The clipboard follows the pointer: when this machine starts controlling a
//! client it sends that client its clipboard, and when control comes back the
//! client sends its own. Content is only transferred if it differs from what
//! was last exchanged with that peer, so moving back and forth does not copy
//! the same data again.
//!
//! Text and images are supported. Content is only exchanged with peers that
//! announced [`CAP_CLIPBOARD`] and that are authorized on this machine.
//! Copied files go to peers that announced [`CAP_FILES`] as well, over the
//! file channel (see [`crate::files`]).

mod history;
mod local;
mod secret;
mod transfer;
mod watch;
#[cfg(windows)]
mod windows_files;
#[cfg(all(unix, not(target_os = "macos")))]
mod x11_files;

use std::{
    cell::Cell,
    collections::HashMap,
    fmt::Display,
    net::SocketAddr,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

use local_channel::mpsc::{Receiver, Sender, channel};
use rustinput_ipc::ClientHandle;
use rustinput_proto::{
    CAP_CLIPBOARD, CAP_CLIPBOARD_HTML, CAP_FILE_RESUME, CAP_FILES, ClipboardFrame, ClipboardKind,
};
use tokio::{sync::oneshot, task::spawn_local};

use crate::{
    connect::OutgoingLinks,
    files::{FilesHandle, PeerInfo},
    listen::IncomingLinks,
};
pub(crate) use history::HistoryAnswer;
use history::{History, Kept};
pub(crate) use local::Peeked;
use local::{LocalClipboard, LocalContent};
pub(crate) use transfer::MAX_CONTENT_SIZE;
use transfer::{Incoming, Outgoing, Progress};

/// interval at which running transfers are checked for resends and timeouts
const TICK: Duration = Duration::from_millis(50);

/// how long a push waits for the peer to announce its capabilities
const PENDING_PUSH_TTL: Duration = Duration::from_secs(3);

/// something the clipboard held or is going to hold
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Content {
    pub(crate) kind: ClipboardKind,
    /// UTF-8 text or a PNG image
    pub(crate) data: Vec<u8>,
    pub(crate) hash: u64,
}

/// the other end of a connection
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum Peer {
    /// a client this machine connects to
    Outgoing(ClientHandle),
    /// a device connected to the listener of this machine
    Incoming(SocketAddr),
}

impl Display for Peer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Peer::Outgoing(handle) => write!(f, "client {handle}"),
            Peer::Incoming(addr) => write!(f, "{addr}"),
        }
    }
}

pub(crate) enum ClipboardMsg {
    /// the (trusted) peer announced its optional features
    Capabilities {
        peer: Peer,
        flags: u32,
        /// fingerprint of the certificate of the peer
        fingerprint: String,
        /// where to open the file channel, for peers this device connects to
        connect_to: Option<SocketAddr>,
    },
    /// a clipboard frame arrived from the peer
    Frame { peer: Peer, frame: ClipboardFrame },
    /// the connection to the peer is gone
    Disconnected { peer: Peer },
    /// send the local clipboard to the peer, if it has something new
    Push { peer: Peer },
    /// the local clipboard was read for a push to the peer
    Local {
        peer: Peer,
        content: Option<LocalContent>,
    },
    /// send the clipboard to every peer right away
    PushAll,
    /// send the files copied in the file manager to the connected devices,
    /// telling `none` if there are no files on the clipboard
    SendCopiedFiles { none: String },
    /// the clipboard was read for [`ClipboardMsg::SendCopiedFiles`]
    CopiedFiles {
        content: Option<LocalContent>,
        none: String,
    },
    /// files arrived (from `peer`, if it is still known) and go to the
    /// clipboard; `key` identifies them (see [`crate::files::files_key`])
    FilesReceived {
        peer: Option<Peer>,
        paths: Vec<PathBuf>,
        key: u64,
    },
    /// what the clipboard holds, for the Clipboard tab of a computer; no
    /// answer when it can not be read
    Peek {
        known: Option<u64>,
        reply: oneshot::Sender<Peeked>,
    },
    /// what the clipboard holds for the device with the certificate
    /// `fingerprint`, if it still holds `hash`: text and images in the
    /// answer, files as a file transfer to that device
    Fetch {
        hash: u64,
        fingerprint: String,
        reply: oneshot::Sender<Result<Fetched, String>>,
    },
    /// the clipboard was read for the questions of [`ClipboardMsg::Peek`]
    PeekRead(Option<Peeked>),
    /// the clipboard was read for [`ClipboardMsg::Fetch`]
    FetchRead {
        hash: u64,
        fingerprint: String,
        content: Option<LocalContent>,
        reply: oneshot::Sender<Result<Fetched, String>>,
    },
    /// what the clipboard of the device with the certificate `fingerprint`
    /// held, brought here from the Clipboard tab
    Bring {
        content: Content,
        fingerprint: String,
    },
    /// the history of the clipboard, unless the asker still knows its
    /// version
    History {
        known: Option<u64>,
        reply: oneshot::Sender<HistoryAnswer>,
    },
    /// something of the history back on the clipboard
    Restore {
        hash: u64,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// forget the history
    ClearHistory,
    /// keep at most so many copies in the history (0: none)
    HistorySize(usize),
    /// the clipboard was read after something was copied, `None` for nothing
    /// to keep
    Recorded(Option<local::Recorded>),
}

/// what the clipboard gave for [`ClipboardMsg::Fetch`]
#[derive(Debug)]
pub(crate) enum Fetched {
    Data(Content),
    /// that many files and folders go as a file transfer
    Files(usize),
}

/// what the rest of the service uses to talk to the clipboard task
#[derive(Clone)]
pub(crate) struct ClipboardHandle {
    tx: Sender<ClipboardMsg>,
    enabled: bool,
    /// files can be transferred
    files: bool,
    /// files copied in a file manager are sent when the pointer moves to
    /// another device (otherwise only with "Copy to RustInput")
    send_copied: Rc<Cell<bool>>,
}

impl ClipboardHandle {
    pub(crate) fn new(enabled: bool, files: bool) -> (Self, Receiver<ClipboardMsg>) {
        let (tx, rx) = channel();
        let files = enabled && files;
        let send_copied = Rc::new(Cell::new(false));
        (
            Self {
                tx,
                enabled,
                files,
                send_copied,
            },
            rx,
        )
    }

    /// whether files copied in a file manager go along with the pointer
    pub(crate) fn set_send_copied(&self, send: bool) {
        self.send_copied.set(send);
    }

    /// the capability flags to announce to peers
    pub(crate) fn capabilities(&self) -> u32 {
        match (self.enabled, self.files) {
            (true, true) => CAP_CLIPBOARD | CAP_CLIPBOARD_HTML | CAP_FILES | CAP_FILE_RESUME,
            (true, false) => CAP_CLIPBOARD | CAP_CLIPBOARD_HTML,
            _ => 0,
        }
    }

    /// whether the clipboard is shared at all (`clipboard`)
    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    /// whether copied files are sent to the peers
    pub(crate) fn files_enabled(&self) -> bool {
        self.files
    }

    pub(crate) fn notify(&self, msg: ClipboardMsg) {
        if self.enabled {
            // the task only ends together with the service
            let _ = self.tx.send(msg);
        }
    }

    pub(crate) fn push(&self, peer: Peer) {
        self.notify(ClipboardMsg::Push { peer });
    }
}

/// the connections clipboard frames are sent over
#[derive(Clone)]
pub(crate) struct PeerLinks {
    pub(crate) outgoing: OutgoingLinks,
    pub(crate) incoming: IncomingLinks,
}

impl PeerLinks {
    async fn send(&self, peer: Peer, frame: &ClipboardFrame) -> bool {
        let datagram = frame.encode();
        match peer {
            Peer::Outgoing(handle) => self.outgoing.send(handle, &datagram).await,
            Peer::Incoming(addr) => self.incoming.send(addr, &datagram).await,
        }
    }
}

/// Starts the clipboard task, which runs until the service ends; with a
/// history of `history` copies.
pub(crate) fn spawn(
    handle: &ClipboardHandle,
    rx: Receiver<ClipboardMsg>,
    links: PeerLinks,
    files: Option<FilesHandle>,
    notices: Sender<String>,
    history: usize,
) {
    if !handle.enabled {
        log::info!("clipboard synchronization is disabled");
        return;
    }
    let (changed, changes) = tokio::sync::mpsc::unbounded_channel();
    let watched = watch::spawn(changed);
    let mut manager = Manager {
        rx,
        tx: handle.tx.clone(),
        send_copied: handle.send_copied.clone(),
        notices,
        links,
        files,
        local: LocalClipboard::new(),
        peers: HashMap::new(),
        pending: HashMap::new(),
        next_transfer: 0,
        peek_waiting: Vec::new(),
        peek_running: None,
        history: History::new(history),
        watched,
        changes,
        recording: false,
        record_again: false,
    };
    // what the clipboard holds already is the first of the history
    manager.start_record();
    spawn_local(manager.run());
}

#[derive(Default)]
struct PeerState {
    /// hash of the content last sent to or received from the peer
    synced: Option<u64>,
    outgoing: Option<Outgoing>,
    incoming: Option<Incoming>,
    /// the local clipboard is being read for a push to this peer
    reading: bool,
    /// the peer takes copied files
    files: bool,
    /// the peer takes formatted text
    html: bool,
    /// fingerprint of the certificate of the peer
    fingerprint: String,
}

struct Manager {
    rx: Receiver<ClipboardMsg>,
    tx: Sender<ClipboardMsg>,
    links: PeerLinks,
    /// file transfers, if enabled
    files: Option<FilesHandle>,
    send_copied: Rc<Cell<bool>>,
    /// what the user should know
    notices: Sender<String>,
    local: LocalClipboard,
    /// peers that take part in clipboard synchronization
    peers: HashMap<Peer, PeerState>,
    /// pushes requested before the peer announced its capabilities
    pending: HashMap<Peer, Instant>,
    next_transfer: u32,
    /// questions of [`ClipboardMsg::Peek`] waiting for the next read
    peek_waiting: Vec<PeekQuestion>,
    /// the questions the running read is for, if one runs
    peek_running: Option<Vec<PeekQuestion>>,
    /// what was copied here
    history: History,
    /// copies are told here ([`watch`])
    watched: bool,
    /// something was copied
    changes: tokio::sync::mpsc::UnboundedReceiver<()>,
    /// the clipboard is being read for the history
    recording: bool,
    /// something else was copied while it was being read
    record_again: bool,
}

/// how long after a copy the clipboard is read for the history: the
/// application may still be putting its formats there
const RECORD_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

/// a question of [`ClipboardMsg::Peek`]: the hash the asker knows, where the
/// answer goes
type PeekQuestion = (Option<u64>, oneshot::Sender<Peeked>);

impl Manager {
    async fn run(mut self) {
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let busy = self.busy();
            tokio::select! {
                msg = self.rx.recv() => match msg {
                    Some(msg) => self.handle(msg).await,
                    None => break,
                },
                Some(()) = self.changes.recv() => self.start_record(),
                _ = tick.tick(), if busy => self.on_tick().await,
            }
        }
    }

    /// whether there is anything the tick has to look after
    fn busy(&self) -> bool {
        !self.pending.is_empty()
            || self
                .peers
                .values()
                .any(|p| p.outgoing.is_some() || p.incoming.is_some())
    }

    async fn handle(&mut self, msg: ClipboardMsg) {
        match msg {
            ClipboardMsg::Capabilities {
                peer,
                flags,
                fingerprint,
                connect_to,
            } => {
                if flags & CAP_CLIPBOARD == 0 {
                    self.forget(peer);
                    return;
                }
                let state = self.peers.entry(peer).or_insert_with(|| {
                    log::info!("clipboard is shared with {peer}");
                    PeerState::default()
                });
                state.files = flags & CAP_FILES != 0 && self.files.is_some();
                state.html = flags & CAP_CLIPBOARD_HTML != 0;
                state.fingerprint = fingerprint.clone();
                if let Some(files) = &self.files {
                    if state.files {
                        files.add_peer(PeerInfo {
                            peer,
                            fingerprint,
                            connect_to,
                            resume: flags & CAP_FILE_RESUME != 0,
                        });
                    } else {
                        files.remove_peer(peer);
                    }
                }
                if let Some(requested) = self.pending.remove(&peer) {
                    if requested.elapsed() < PENDING_PUSH_TTL {
                        self.read_for(peer);
                    }
                }
            }
            ClipboardMsg::Push { peer } => {
                if self.peers.contains_key(&peer) {
                    self.read_for(peer);
                } else {
                    // the capabilities may still be on their way
                    self.pending.insert(peer, Instant::now());
                }
            }
            ClipboardMsg::Local { peer, content } => match content {
                Some(LocalContent::Files { paths, key }) => self.send_files(peer, paths, key),
                Some(LocalContent::Data(content)) => self.start_transfer(peer, Some(content)).await,
                None => self.start_transfer(peer, None).await,
            },
            ClipboardMsg::PushAll => {
                let peers: Vec<Peer> = self.peers.keys().copied().collect();
                for peer in peers {
                    self.read_for(peer);
                }
            }
            ClipboardMsg::SendCopiedFiles { none } => {
                let (local, tx) = (self.local.clone(), self.tx.clone());
                spawn_local(async move {
                    let content = local.read().await;
                    let _ = tx.send(ClipboardMsg::CopiedFiles { content, none });
                });
            }
            ClipboardMsg::CopiedFiles { content, none } => match (content, &self.files) {
                (Some(LocalContent::Files { paths, .. }), Some(files)) => files.send_now(paths),
                _ => {
                    let _ = self.notices.send(none);
                }
            },
            ClipboardMsg::FilesReceived { peer, paths, key } => {
                // the files came from that peer: they do not go back to it
                if let Some(state) = peer.and_then(|p| self.peers.get_mut(&p)) {
                    state.synced = Some(key);
                }
                self.local.write_files(paths);
            }
            ClipboardMsg::Frame { peer, frame } => self.handle_frame(peer, frame).await,
            ClipboardMsg::Disconnected { peer } => self.forget(peer),
            ClipboardMsg::Peek { known, reply } => {
                self.peek_waiting.push((known, reply));
                self.start_peek();
            }
            ClipboardMsg::PeekRead(peeked) => {
                for (known, reply) in self.peek_running.take().unwrap_or_default() {
                    let answer = match &peeked {
                        Some(Peeked::Holds { hash, .. }) if known == Some(*hash) => {
                            Peeked::Unchanged
                        }
                        Some(peeked) => peeked.clone(),
                        // unreadable: no answer
                        None => continue,
                    };
                    let _ = reply.send(answer);
                }
                self.start_peek();
            }
            ClipboardMsg::Fetch {
                hash,
                fingerprint,
                reply,
            } => {
                let (local, tx) = (self.local.clone(), self.tx.clone());
                spawn_local(async move {
                    let content = local.read().await;
                    let _ = tx.send(ClipboardMsg::FetchRead {
                        hash,
                        fingerprint,
                        content,
                        reply,
                    });
                });
            }
            ClipboardMsg::FetchRead {
                hash,
                fingerprint,
                content,
                reply,
            } => {
                let _ = reply.send(self.fetched(hash, &fingerprint, content));
            }
            ClipboardMsg::Bring {
                content,
                fingerprint,
            } => {
                // it came from there: it does not go back
                for state in self.peers_with(&fingerprint) {
                    state.synced = Some(content.hash);
                }
                log::info!(
                    "clipboard brought here: {:?}, {} bytes",
                    content.kind,
                    content.data.len()
                );
                self.local.write(content);
            }
            ClipboardMsg::History { known, reply } => {
                let answer = if self.watched {
                    self.history.answer(known)
                } else {
                    HistoryAnswer::Unsupported
                };
                let _ = reply.send(answer);
            }
            ClipboardMsg::Restore { hash, reply } => {
                let result = match self.history.find(hash) {
                    Some(Kept::Data(content)) => {
                        log::info!("a copy of the history back on the clipboard");
                        self.local.write(content.clone());
                        Ok(())
                    }
                    Some(Kept::Files(_)) => {
                        Err("copied files are copied again in the file manager".into())
                    }
                    None => Err("it is no longer in the history".into()),
                };
                let _ = reply.send(result);
            }
            ClipboardMsg::ClearHistory => {
                log::info!("the history of the clipboard is forgotten");
                self.history.clear();
            }
            ClipboardMsg::HistorySize(size) => {
                let was = self.history.size();
                self.history.set_size(size);
                if was == 0 && size > 0 {
                    self.start_record();
                }
            }
            ClipboardMsg::Recorded(recorded) => {
                self.recording = false;
                if let Some(recorded) = recorded {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_secs());
                    self.history.add(recorded, now);
                }
                if std::mem::take(&mut self.record_again) {
                    self.start_record();
                }
            }
        }
    }

    /// Reads the clipboard for the history, a moment after something was
    /// copied; once at a time.
    fn start_record(&mut self) {
        if !self.watched || self.history.size() == 0 {
            return;
        }
        if self.recording {
            self.record_again = true;
            return;
        }
        self.recording = true;
        let (local, tx) = (self.local.clone(), self.tx.clone());
        spawn_local(async move {
            tokio::time::sleep(RECORD_DELAY).await;
            let recorded = local.record().await;
            let _ = tx.send(ClipboardMsg::Recorded(recorded));
        });
    }

    /// One read of the clipboard at a time answers every question that came
    /// meanwhile: reading an image can take longer than the Clipboard tabs
    /// take to ask again, and the questions must not pile up. Those whose
    /// asker stopped waiting are dropped.
    fn start_peek(&mut self) {
        if self.peek_running.is_some() {
            return;
        }
        let mut questions = std::mem::take(&mut self.peek_waiting);
        questions.retain(|(_, reply)| !reply.is_closed());
        let Some((first, _)) = questions.first() else {
            return;
        };
        // what they all know may be left out; otherwise everything
        let known = first.filter(|_| questions.iter().all(|(known, _)| known == first));
        self.peek_running = Some(questions);
        let (local, tx) = (self.local.clone(), self.tx.clone());
        spawn_local(async move {
            let peeked = local.peek(known).await;
            let _ = tx.send(ClipboardMsg::PeekRead(peeked));
        });
    }

    /// the peers with the certificate `fingerprint` (one, unless it both
    /// connects here and is connected to)
    fn peers_with<'a>(
        &'a mut self,
        fingerprint: &'a str,
    ) -> impl Iterator<Item = &'a mut PeerState> {
        self.peers
            .values_mut()
            .filter(move |state| state.fingerprint == fingerprint)
    }

    /// what goes to the device with the certificate `fingerprint`, which
    /// asked for the clipboard if it still holds `hash`
    fn fetched(
        &mut self,
        hash: u64,
        fingerprint: &str,
        content: Option<LocalContent>,
    ) -> Result<Fetched, String> {
        match content {
            Some(LocalContent::Data(content)) if content.hash == hash => {
                // it has it now: not sent again when the pointer moves there
                for state in self.peers_with(fingerprint) {
                    state.synced = Some(hash);
                }
                Ok(Fetched::Data(content))
            }
            Some(LocalContent::Files { paths, key }) if key == hash => {
                let files = self.files.clone();
                let peer = self
                    .peers
                    .iter_mut()
                    .find(|(_, state)| state.fingerprint == fingerprint && state.files);
                match (files, peer) {
                    (Some(files), Some((&peer, state))) => {
                        state.synced = Some(key);
                        let count = paths.len();
                        log::info!("sending the {count} copied item(s) to {peer}, as it asked");
                        files.send(peer, paths, false);
                        Ok(Fetched::Files(count))
                    }
                    _ => Err("files go only to a computer connected with file transfers on".into()),
                }
            }
            // not on the clipboard any more: maybe in the history
            _ => match self.history.find(hash).cloned() {
                Some(Kept::Data(content)) => {
                    self.fetched(hash, fingerprint, Some(LocalContent::Data(content)))
                }
                Some(Kept::Files(paths)) => {
                    let content = LocalContent::Files { paths, key: hash };
                    self.fetched(hash, fingerprint, Some(content))
                }
                None => Err("the clipboard changed meanwhile".into()),
            },
        }
    }

    fn forget(&mut self, peer: Peer) {
        self.pending.remove(&peer);
        if let Some(files) = &self.files {
            files.remove_peer(peer);
        }
        if self.peers.remove(&peer).is_some() {
            log::debug!("clipboard is no longer shared with {peer}");
        }
    }

    /// reads the local clipboard without holding up the transfers
    fn read_for(&mut self, peer: Peer) {
        let Some(state) = self.peers.get_mut(&peer) else {
            return;
        };
        if state.reading {
            return;
        }
        state.reading = true;
        let (local, tx) = (self.local.clone(), self.tx.clone());
        spawn_local(async move {
            let content = local.read().await;
            let _ = tx.send(ClipboardMsg::Local { peer, content });
        });
    }

    /// hands copied files over to the file transfers
    fn send_files(&mut self, peer: Peer, paths: Vec<PathBuf>, key: u64) {
        let Some(state) = self.peers.get_mut(&peer) else {
            return;
        };
        state.reading = false;
        if state.synced == Some(key) {
            return;
        }
        // Neither the files nor their paths as text: the clipboard of the
        // other device keeps what it has, files sent with "Copy to RustInput"
        // for instance.
        if !self.send_copied.get() {
            log::debug!("copied files are only sent with \"Copy to RustInput\"");
            return;
        }
        let Some(files) = self.files.as_ref().filter(|_| state.files) else {
            log::info!("{peer} does not take files, the copied files stay here");
            return;
        };
        // recorded right away so a second push of the same files is a no-op
        state.synced = Some(key);
        files.send(peer, paths, true);
    }

    async fn start_transfer(&mut self, peer: Peer, content: Option<Content>) {
        let Some(state) = self.peers.get_mut(&peer) else {
            return;
        };
        state.reading = false;
        let Some(content) = content else {
            return;
        };
        let content = if state.html { content } else { content.plain() };
        if content.data.is_empty() || state.synced == Some(content.hash) {
            return;
        }
        if content.data.len() > MAX_CONTENT_SIZE {
            log::info!(
                "clipboard content of {} bytes is too large to send",
                content.data.len()
            );
            return;
        }
        if let Some(previous) = state.outgoing.take() {
            let cancel = ClipboardFrame::Cancel {
                transfer: previous.id(),
            };
            self.links.send(peer, &cancel).await;
        }
        log::info!(
            "sending clipboard to {peer}: {:?}, {} bytes",
            content.kind,
            content.data.len()
        );
        let id = self.next_transfer;
        self.next_transfer = self.next_transfer.wrapping_add(1);
        // recorded right away so a second push of the same content is a no-op
        state.synced = Some(content.hash);
        state.outgoing = Some(Outgoing::new(id, content, Instant::now()));
        self.pump(peer).await;
    }

    async fn handle_frame(&mut self, peer: Peer, frame: ClipboardFrame) {
        let now = Instant::now();
        // frames of peers that are not part of the synchronization are dropped
        let Some(state) = self.peers.get_mut(&peer) else {
            return;
        };
        match frame {
            ClipboardFrame::Offer {
                transfer,
                kind,
                size,
                hash,
            } => {
                let size = size as usize;
                let reply = if let Some(incoming) =
                    state.incoming.as_ref().filter(|i| i.id() == transfer)
                {
                    // a repeated offer, our answer got lost
                    incoming.ack()
                } else if size == 0 || size > MAX_CONTENT_SIZE || state.synced == Some(hash) {
                    // nothing we want, or nothing new
                    ClipboardFrame::Cancel { transfer }
                } else {
                    log::debug!("{peer} offers its clipboard: {kind:?}, {size} bytes");
                    let incoming = Incoming::new(transfer, kind, size, hash, now);
                    let ack = incoming.ack();
                    state.incoming = Some(incoming);
                    ack
                };
                self.links.send(peer, &reply).await;
            }
            ClipboardFrame::Data {
                transfer,
                offset,
                data,
            } => {
                let Some(incoming) = state.incoming.as_mut().filter(|i| i.id() == transfer) else {
                    return;
                };
                let ack = incoming.on_data(offset, &data, now);
                if incoming.is_complete() {
                    let content = state.incoming.take().expect("incoming").into_content();
                    log::info!(
                        "received clipboard from {peer}: {:?}, {} bytes",
                        content.kind,
                        content.data.len()
                    );
                    state.synced = Some(content.hash);
                    self.local.write(content);
                }
                if let Some(ack) = ack {
                    self.links.send(peer, &ack).await;
                }
            }
            ClipboardFrame::Ack { transfer, received } => {
                if let Some(outgoing) = state.outgoing.as_mut().filter(|o| o.id() == transfer) {
                    outgoing.on_ack(received, now);
                    self.pump(peer).await;
                }
            }
            ClipboardFrame::Cancel { transfer } => {
                if state.outgoing.as_ref().is_some_and(|o| o.id() == transfer) {
                    log::debug!("{peer} declined the clipboard");
                    state.outgoing = None;
                }
                if state.incoming.as_ref().is_some_and(|i| i.id() == transfer) {
                    state.incoming = None;
                }
            }
        }
    }

    /// transmits what the outgoing transfer to `peer` has ready and retires
    /// the transfer once it is over
    async fn pump(&mut self, peer: Peer) {
        let now = Instant::now();
        let Some(state) = self.peers.get_mut(&peer) else {
            return;
        };
        let Some(outgoing) = state.outgoing.as_mut() else {
            return;
        };
        let mut connected = true;
        for frame in outgoing.poll(now) {
            if !self.links.send(peer, &frame).await {
                connected = false;
                break;
            }
        }
        let progress = if connected {
            outgoing.progress(now)
        } else {
            Progress::Failed
        };
        match progress {
            Progress::Active => {}
            Progress::Done => {
                log::info!("clipboard delivered to {peer} ({} bytes)", outgoing.size());
                state.outgoing = None;
            }
            Progress::Failed => {
                log::warn!("sending the clipboard to {peer} failed");
                state.outgoing = None;
                // let the next push try again
                state.synced = None;
            }
        }
    }

    async fn on_tick(&mut self) {
        let now = Instant::now();
        self.pending
            .retain(|_, requested| now.duration_since(*requested) < PENDING_PUSH_TTL);
        let mut sending = Vec::new();
        for (peer, state) in self.peers.iter_mut() {
            if state.incoming.as_ref().is_some_and(|i| i.is_stale(now)) {
                let incoming = state.incoming.take().expect("incoming");
                log::warn!(
                    "receiving the clipboard from {peer} timed out ({} bytes expected)",
                    incoming.size()
                );
            }
            if state.outgoing.is_some() {
                sending.push(*peer);
            }
        }
        for peer in sending {
            self.pump(peer).await;
        }
    }
}
