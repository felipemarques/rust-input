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

mod local;
mod transfer;
#[cfg(all(unix, not(target_os = "macos")))]
mod x11_files;

use std::{
    collections::HashMap,
    fmt::Display,
    net::SocketAddr,
    path::PathBuf,
    time::{Duration, Instant},
};

use local_channel::mpsc::{Receiver, Sender, channel};
use rustinput_ipc::ClientHandle;
use rustinput_proto::{CAP_CLIPBOARD, CAP_FILES, ClipboardFrame, ClipboardKind};
use tokio::task::spawn_local;

use crate::{
    connect::OutgoingLinks,
    files::{FilesHandle, PeerInfo},
    listen::IncomingLinks,
};
use local::{LocalClipboard, LocalContent};
use transfer::{Incoming, MAX_CONTENT_SIZE, Outgoing, Progress};

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
    /// files arrived (from `peer`, if it is still known) and go to the
    /// clipboard; `key` identifies them (see [`crate::files::files_key`])
    FilesReceived {
        peer: Option<Peer>,
        paths: Vec<PathBuf>,
        key: u64,
    },
}

/// what the rest of the service uses to talk to the clipboard task
#[derive(Clone)]
pub(crate) struct ClipboardHandle {
    tx: Sender<ClipboardMsg>,
    enabled: bool,
    /// copied files are sent too
    files: bool,
}

impl ClipboardHandle {
    pub(crate) fn new(enabled: bool, files: bool) -> (Self, Receiver<ClipboardMsg>) {
        let (tx, rx) = channel();
        let files = enabled && files;
        (Self { tx, enabled, files }, rx)
    }

    /// the capability flags to announce to peers
    pub(crate) fn capabilities(&self) -> u32 {
        match (self.enabled, self.files) {
            (true, true) => CAP_CLIPBOARD | CAP_FILES,
            (true, false) => CAP_CLIPBOARD,
            _ => 0,
        }
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

/// starts the clipboard task, which runs until the service ends
pub(crate) fn spawn(
    handle: &ClipboardHandle,
    rx: Receiver<ClipboardMsg>,
    links: PeerLinks,
    files: Option<FilesHandle>,
) {
    if !handle.enabled {
        log::info!("clipboard synchronization is disabled");
        return;
    }
    let manager = Manager {
        rx,
        tx: handle.tx.clone(),
        links,
        files,
        local: LocalClipboard::new(),
        peers: HashMap::new(),
        pending: HashMap::new(),
        next_transfer: 0,
    };
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
}

struct Manager {
    rx: Receiver<ClipboardMsg>,
    tx: Sender<ClipboardMsg>,
    links: PeerLinks,
    /// file transfers, if enabled
    files: Option<FilesHandle>,
    local: LocalClipboard,
    /// peers that take part in clipboard synchronization
    peers: HashMap<Peer, PeerState>,
    /// pushes requested before the peer announced its capabilities
    pending: HashMap<Peer, Instant>,
    next_transfer: u32,
}

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
                if let Some(files) = &self.files {
                    if state.files {
                        files.add_peer(PeerInfo {
                            peer,
                            fingerprint,
                            connect_to,
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
            ClipboardMsg::FilesReceived { peer, paths, key } => {
                // the files came from that peer: they do not go back to it
                if let Some(state) = peer.and_then(|p| self.peers.get_mut(&p)) {
                    state.synced = Some(key);
                }
                self.local.write_files(paths);
            }
            ClipboardMsg::Frame { peer, frame } => self.handle_frame(peer, frame).await,
            ClipboardMsg::Disconnected { peer } => self.forget(peer),
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
        let Some(files) = self.files.as_ref().filter(|_| state.files) else {
            log::info!("{peer} does not take files, the copied files stay here");
            return;
        };
        // recorded right away so a second push of the same files is a no-op
        state.synced = Some(key);
        files.send(peer, paths);
    }

    async fn start_transfer(&mut self, peer: Peer, content: Option<Content>) {
        let Some(state) = self.peers.get_mut(&peer) else {
            return;
        };
        state.reading = false;
        let Some(content) = content else {
            return;
        };
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
