//! Keeping the arrangement of the computers the same on all of them.
//!
//! Like pairing ([`crate::pairing`]), this runs on the TCP port of the file
//! channel, in TLS with the certificates of both devices, starting with
//! [`MAGIC`]. One message is answered per connection:
//!
//! - [`Message::Describe`]: the other device answers with its name and its
//!   monitors, which the window shows to arrange it;
//! - [`Message::Arrange`]: the other device takes the arrangement, finds
//!   itself in it (`you`) and sets up its connections from it;
//! - [`Message::Identify`]: the other device shows the number of each of
//!   its monitors on it for a few seconds ([`crate::identify`]). An older
//!   RustInput does not know it and closes the connection.
//! - [`Message::Lock`]: the other device locks its screen ([`crate::lock`]),
//!   and answers once it did, or why not.
//! - [`Message::Peek`]: the other device tells what its clipboard holds, for
//!   the Clipboard tab: the start of a text, a thumbnail of an image (after
//!   the message), the names of files; nothing when it still holds what
//!   `known` stands for, or when it does not show its clipboard
//!   (`show_clipboard`).
//! - [`Message::Fetch`]: the other device sends what its clipboard holds,
//!   if it still holds `hash`: text and images after the answer, files as a
//!   file transfer.
//!
//! All are answered only to authorized devices: an arrangement changes
//! where the pointer goes, the monitors are no business of strangers, and
//! only a device allowed to control this one may lock it or read its
//! clipboard.

use std::{cell::RefCell, fmt, io, net::SocketAddr, rc::Rc, sync::Arc, time::Duration};

use local_channel::mpsc::Sender;
use rustinput_ipc::{
    ClipboardPeek, ClipboardSeen,
    layout::{Layout, Monitor},
};
use rustinput_proto::ClipboardKind;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    sync::oneshot,
    time::timeout,
};
use tokio_rustls::TlsConnector;
use webrtc_dtls::crypto::Certificate;

use crate::{
    clipboard::{Content, Fetched, HistoryAnswer, MAX_CONTENT_SIZE},
    files::tls,
};

/// first bytes of a connection about the arrangement, both ways
pub(crate) const MAGIC: [u8; 4] = *b"RIA1";
/// a history of 200 copies with the start of their texts fits
const MAX_MESSAGE: u32 = 1024 * 1024;
/// connecting and one question and answer
const TIMEOUT: Duration = Duration::from_secs(10);
/// longest name accepted from the other device
const MAX_NAME: usize = 64;
/// longest the screen may take to lock, within [`TIMEOUT`]
const LOCK_TIMEOUT: Duration = Duration::from_secs(7);
/// longest the clipboard may take to be read for [`Message::Peek`], within
/// [`TIMEOUT`] of the asker: arboard gives up on an image after 4 s
const PEEK_TIMEOUT: Duration = Duration::from_secs(8);
/// longest the clipboard may take to be read for [`Message::Fetch`]
const FETCH_READ_TIMEOUT: Duration = Duration::from_secs(20);
/// connecting, asking for the clipboard and receiving it, up to
/// [`MAX_CONTENT_SIZE`]
const FETCH_TIMEOUT: Duration = Duration::from_secs(120);
/// longest the question may take to arrive
const QUESTION_TIMEOUT: Duration = Duration::from_secs(10);
/// longest a connection is served, the answer to [`Message::Fetch`] being the
/// longest
pub(crate) const SERVE_TIMEOUT: Duration = Duration::from_secs(130);
/// largest thumbnail of an image after [`Message::Peeked`]
const MAX_THUMBNAIL: u32 = 4 * 1024 * 1024;
/// most bytes of the thumbnails of the history after [`Message::Peeked`]
const MAX_HISTORY_THUMBNAILS: usize = 8 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
enum Message {
    Describe,
    Description(Description),
    Arrange {
        layout: Layout,
        you: String,
    },
    Arranged,
    Identify,
    Identified,
    Lock,
    /// the screen locked, or `error`
    Locked {
        error: Option<String>,
    },
    Refused {
        reason: String,
    },
    /// what the clipboard holds, unless it still holds what `known` (a
    /// hash) stands for; with its history when `history` is there (the
    /// version the asker knows, 0 for none; 0.35.0)
    Peek {
        known: Option<u64>,
        #[serde(default)]
        history: Option<u64>,
    },
    /// `None`: it still holds what `known` stands for; the thumbnail of an
    /// image follows the message (`attachment` bytes), then those of the
    /// images of the history, in their order (`attachments`). Without
    /// `history` from a RustInput before 0.35.0.
    Peeked {
        seen: Option<ClipboardSeen>,
        attachment: u32,
        #[serde(default)]
        history: Option<HistoryAnswer>,
        #[serde(default)]
        attachments: Vec<u32>,
    },
    /// what the clipboard holds, if it still holds `hash`
    Fetch {
        hash: u64,
    },
    /// `size` bytes of [`ClipboardKind`] `kind` follow the message
    Fetched {
        kind: u8,
        hash: u64,
        size: u32,
    },
    /// the clipboard held `count` files and folders, which go as a file
    /// transfer
    FilesSent {
        count: u64,
    },
}

/// a device as it describes itself
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Description {
    pub(crate) name: String,
    /// its monitors, from the top left corner of its desktop
    pub(crate) monitors: Vec<Monitor>,
    /// its system (`std::env::consts::OS`)
    #[serde(default)]
    pub(crate) os: String,
    /// the revision of the arrangement it has
    pub(crate) revision: u64,
}

/// what the other devices bring to the service
#[derive(Debug)]
pub(crate) enum ArrangementEvent {
    /// an authorized device sent an arrangement, in which this one is `you`
    Received {
        layout: Layout,
        you: String,
        /// fingerprint of the sender
        from: String,
        addr: SocketAddr,
    },
    /// an authorized device asks to show the numbers of the monitors
    Identify { addr: SocketAddr },
    /// an authorized device asks to lock the screen; `done` tells it how
    /// that went
    Lock {
        addr: SocketAddr,
        done: oneshot::Sender<Result<(), String>>,
    },
    /// an authorized device asks what the clipboard holds, unless it still
    /// holds what `known` stands for (then `None`)
    Peek {
        addr: SocketAddr,
        known: Option<u64>,
        /// the history too, unless the asker knows this version of it
        history: Option<u64>,
        done: oneshot::Sender<(Option<ClipboardSeen>, Option<HistoryAnswer>)>,
    },
    /// the authorized device with the certificate `from` asks for what the
    /// clipboard holds, if it still holds `hash`
    Fetch {
        addr: SocketAddr,
        hash: u64,
        from: String,
        done: oneshot::Sender<Result<Fetched, String>>,
    },
}

/// the monitors of this device, from the top left corner of its desktop,
/// and the size of the desktop; none when they can not be read here
pub(crate) fn own_monitors() -> Option<(Vec<Monitor>, i32, i32)> {
    let areas = input_capture::desktop::monitors()?;
    let left = areas.iter().map(|a| a.x).min()?;
    let top = areas.iter().map(|a| a.y).min()?;
    let right = areas.iter().map(|a| a.x + a.width).max()?;
    let bottom = areas.iter().map(|a| a.y + a.height).max()?;
    let monitors = areas
        .iter()
        .map(|a| Monitor {
            x: a.x - left,
            y: a.y - top,
            width: a.width,
            height: a.height,
        })
        .collect();
    Some((monitors, right - left, bottom - top))
}

/// the answering side, on the TCP port
#[derive(Clone)]
pub(crate) struct Arrangement {
    events: Sender<ArrangementEvent>,
    name: String,
    /// the revision of the arrangement this device has
    revision: Rc<RefCell<u64>>,
}

impl Arrangement {
    pub(crate) fn new(name: String, revision: u64, events: Sender<ArrangementEvent>) -> Self {
        Self {
            events,
            name,
            revision: Rc::new(RefCell::new(revision)),
        }
    }

    pub(crate) fn set_revision(&self, revision: u64) {
        self.revision.replace(revision);
    }

    fn describe(&self) -> Description {
        Description {
            name: self.name.clone(),
            monitors: own_monitors().map(|(m, ..)| m).unwrap_or_default(),
            os: std::env::consts::OS.to_owned(),
            revision: *self.revision.borrow(),
        }
    }

    /// Answers one question of the device with `peer` (fingerprint), which
    /// is `authorized` on this one or not.
    pub(crate) async fn serve<S>(
        &self,
        stream: &mut S,
        peer: &str,
        authorized: bool,
        addr: SocketAddr,
    ) -> io::Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let message = timeout(QUESTION_TIMEOUT, read(stream))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no question"))??;
        if !authorized {
            let reason = "this computer is not authorized there".to_owned();
            return write(stream, &Message::Refused { reason }).await;
        }
        let answer = match message {
            Message::Describe => Message::Description(self.describe()),
            Message::Arrange { layout, you } => {
                log::info!(
                    "arrangement {} from {addr}, with this computer as {you}",
                    layout.revision
                );
                let _ = self.events.send(ArrangementEvent::Received {
                    layout,
                    you,
                    from: peer.to_owned(),
                    addr,
                });
                Message::Arranged
            }
            Message::Identify => {
                let _ = self.events.send(ArrangementEvent::Identify { addr });
                Message::Identified
            }
            Message::Lock => {
                let (done, result) = oneshot::channel();
                let _ = self.events.send(ArrangementEvent::Lock { addr, done });
                let error = match timeout(LOCK_TIMEOUT, result).await {
                    Ok(Ok(Ok(()))) => None,
                    Ok(Ok(Err(e))) => Some(e),
                    Ok(Err(_)) => Some("RustInput is closing".into()),
                    Err(_) => Some("the screen did not lock in time".into()),
                };
                Message::Locked { error }
            }
            Message::Peek { known, history } => {
                let (done, result) = oneshot::channel();
                let _ = self.events.send(ArrangementEvent::Peek {
                    addr,
                    known,
                    history,
                    done,
                });
                let (seen, history) = match timeout(PEEK_TIMEOUT, result).await {
                    Ok(Ok(answer)) => answer,
                    // nobody to read it: the clipboard is not shared here
                    Ok(Err(_)) => (Some(ClipboardSeen::Hidden), None),
                    Err(_) => (
                        Some(ClipboardSeen::Unavailable(
                            "its clipboard did not answer in time".into(),
                        )),
                        None,
                    ),
                };
                return write_peeked(stream, seen, history).await;
            }
            Message::Fetch { hash } => {
                let (done, result) = oneshot::channel();
                let from = peer.to_owned();
                let _ = self.events.send(ArrangementEvent::Fetch {
                    addr,
                    hash,
                    from,
                    done,
                });
                match timeout(FETCH_READ_TIMEOUT, result).await {
                    Ok(Ok(Ok(Fetched::Data(content)))) => {
                        log::info!(
                            "clipboard sent to {addr} as it asked: {:?}, {} bytes",
                            content.kind,
                            content.data.len()
                        );
                        return write_fetched(stream, content).await;
                    }
                    Ok(Ok(Ok(Fetched::Files(count)))) => Message::FilesSent {
                        count: count as u64,
                    },
                    Ok(Ok(Err(reason))) => Message::Refused { reason },
                    Ok(Err(_)) => Message::Refused {
                        reason: "this computer does not show its clipboard".into(),
                    },
                    Err(_) => Message::Refused {
                        reason: "its clipboard did not answer in time".into(),
                    },
                }
            }
            _ => Message::Refused {
                reason: "unexpected message".into(),
            },
        };
        write(stream, &answer).await
    }
}

/// why a device could not be described or arranged
#[derive(Debug)]
pub(crate) enum ArrangeError {
    /// no answer at any of its addresses
    Unreachable(String),
    /// it answered no
    Refused(String),
    /// it tried and could not
    Failed(String),
}

impl fmt::Display for ArrangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArrangeError::Unreachable(why) => write!(f, "{why}"),
            ArrangeError::Refused(why) => write!(f, "refused: {why}"),
            ArrangeError::Failed(why) => write!(f, "{why}"),
        }
    }
}

impl From<io::Error> for ArrangeError {
    fn from(e: io::Error) -> Self {
        ArrangeError::Unreachable(e.to_string())
    }
}

/// asks the device at one of `addrs` (with the certificate `expected`, when
/// known) to describe itself; returns its fingerprint with the description
pub(crate) async fn describe(
    cert: Certificate,
    addrs: Vec<SocketAddr>,
    expected: Option<String>,
) -> Result<(String, Description), ArrangeError> {
    match ask(cert, addrs, expected, Message::Describe).await? {
        (peer, Message::Description(mut description)) => {
            description.name.truncate(MAX_NAME);
            Ok((peer, description))
        }
        (_, Message::Refused { reason }) => Err(ArrangeError::Refused(reason)),
        _ => Err(ArrangeError::Refused("unexpected answer".into())),
    }
}

/// sends `layout` to the device at one of `addrs`, which is `you` in it
pub(crate) async fn arrange(
    cert: Certificate,
    addrs: Vec<SocketAddr>,
    expected: Option<String>,
    layout: Layout,
    you: String,
) -> Result<(), ArrangeError> {
    match ask(cert, addrs, expected, Message::Arrange { layout, you }).await? {
        (_, Message::Arranged) => Ok(()),
        (_, Message::Refused { reason }) => Err(ArrangeError::Refused(reason)),
        _ => Err(ArrangeError::Refused("unexpected answer".into())),
    }
}

/// asks the device at one of `addrs` to show the numbers of its monitors
pub(crate) async fn identify(
    cert: Certificate,
    addrs: Vec<SocketAddr>,
    expected: Option<String>,
) -> Result<(), ArrangeError> {
    match ask(cert, addrs, expected, Message::Identify).await {
        Ok((_, Message::Identified)) => Ok(()),
        Ok((_, Message::Refused { reason })) => Err(ArrangeError::Refused(reason)),
        Ok(_) => Err(ArrangeError::Refused("unexpected answer".into())),
        Err(e) => Err(older(e)),
    }
}

/// asks the device at one of `addrs` to lock its screen, and waits until it
/// did
pub(crate) async fn lock(
    cert: Certificate,
    addrs: Vec<SocketAddr>,
    expected: Option<String>,
) -> Result<(), ArrangeError> {
    match ask(cert, addrs, expected, Message::Lock).await {
        Ok((_, Message::Locked { error: None })) => Ok(()),
        Ok((_, Message::Locked { error: Some(e) })) => Err(ArrangeError::Failed(e)),
        Ok((_, Message::Refused { reason })) => Err(ArrangeError::Refused(reason)),
        Ok(_) => Err(ArrangeError::Refused("unexpected answer".into())),
        Err(e) => Err(older(e)),
    }
}

/// what [`peek`] found: the fingerprint of the device that answered, what
/// its clipboard holds (`None`: still what `known` stands for) and its
/// history (`None`: a RustInput before 0.35.0)
pub(crate) type PeekedHere = (String, Option<ClipboardSeen>, Option<HistoryAnswer>);

/// What the clipboard of the device at one of `addrs` holds, and its history
/// unless the version `history` is still the one it has.
pub(crate) async fn peek(
    cert: Certificate,
    addrs: Vec<SocketAddr>,
    expected: Option<String>,
    known: Option<u64>,
    history: u64,
) -> Result<PeekedHere, ArrangeError> {
    let question = Message::Peek {
        known,
        history: Some(history),
    };
    match ask_within(cert, addrs, expected, question, TIMEOUT).await {
        Ok((peer, Message::Peeked { seen, history, .. }, _)) => Ok((peer, seen, history)),
        Ok((_, Message::Refused { reason }, _)) => Err(ArrangeError::Refused(reason)),
        Ok(_) => Err(ArrangeError::Refused("unexpected answer".into())),
        Err(e) => Err(older(e)),
    }
}

/// what came for [`fetch`]
#[derive(Debug)]
pub(crate) enum FetchedHere {
    Data(Content),
    /// that many files and folders come as a file transfer
    Files(u64),
}

/// What the clipboard of the device at one of `addrs` holds, if it still
/// holds `hash`, with the fingerprint of the device that answered.
pub(crate) async fn fetch(
    cert: Certificate,
    addrs: Vec<SocketAddr>,
    expected: Option<String>,
    hash: u64,
) -> Result<(String, FetchedHere), ArrangeError> {
    match ask_within(
        cert,
        addrs,
        expected,
        Message::Fetch { hash },
        FETCH_TIMEOUT,
    )
    .await
    {
        Ok((peer, Message::Fetched { kind, hash, .. }, data)) => {
            let kind = ClipboardKind::try_from(kind)
                .map_err(|_| ArrangeError::Failed("unknown kind of content".into()))?;
            Ok((peer, FetchedHere::Data(Content { kind, data, hash })))
        }
        Ok((peer, Message::FilesSent { count }, _)) => Ok((peer, FetchedHere::Files(count))),
        Ok((_, Message::Refused { reason }, _)) => Err(ArrangeError::Refused(reason)),
        Ok(_) => Err(ArrangeError::Refused("unexpected answer".into())),
        Err(e) => Err(older(e)),
    }
}

/// An older RustInput closes the connection on a message it does not know:
/// said so instead of the end of the connection.
fn older(e: ArrangeError) -> ArrangeError {
    match e {
        ArrangeError::Unreachable(e) if e.contains("eof") || e.contains("closed") => {
            ArrangeError::Unreachable("it runs an older RustInput, without this".into())
        }
        e => e,
    }
}

/// `message` to the first of `addrs` that answers, and its answer
async fn ask(
    cert: Certificate,
    addrs: Vec<SocketAddr>,
    expected: Option<String>,
    message: Message,
) -> Result<(String, Message), ArrangeError> {
    ask_within(cert, addrs, expected, message, TIMEOUT)
        .await
        .map(|(peer, answer, _)| (peer, answer))
}

/// `message` to the first of `addrs` that answers within `limit` each, and
/// its answer with the bytes after it
async fn ask_within(
    cert: Certificate,
    addrs: Vec<SocketAddr>,
    expected: Option<String>,
    message: Message,
    limit: Duration,
) -> Result<(String, Message, Vec<u8>), ArrangeError> {
    let config = match &expected {
        Some(fingerprint) => tls::client_config(&cert, fingerprint.clone()),
        None => tls::any_server_config(&cert),
    }
    .map_err(|e| ArrangeError::Unreachable(format!("TLS: {e}")))?;
    let mut last = ArrangeError::Unreachable("its address is not known yet".into());
    for addr in addrs {
        match timeout(limit, attempt(config.clone(), addr, &message)).await {
            Ok(Ok(answer)) => return Ok(answer),
            Ok(Err(e)) => last = e.into(),
            Err(_) => last = ArrangeError::Unreachable(format!("{addr} does not answer")),
        }
    }
    Err(last)
}

async fn attempt(
    config: Arc<rustls::ClientConfig>,
    addr: SocketAddr,
    message: &Message,
) -> io::Result<(String, Message, Vec<u8>)> {
    let tcp = TcpStream::connect(addr).await?;
    tcp.set_nodelay(true)?;
    let mut stream = TlsConnector::from(config)
        .connect(tls::server_name(), tcp)
        .await?;
    stream.write_all(&MAGIC).await?;
    stream.flush().await?;
    let mut magic = [0u8; 4];
    stream.read_exact(&mut magic).await?;
    if magic != MAGIC {
        return Err(invalid("not an arrangement, an older RustInput?"));
    }
    let peer = tls::peer_fingerprint(stream.get_ref().1.peer_certificates())
        .ok_or_else(|| invalid("no certificate"))?;
    write(&mut stream, message).await?;
    let (answer, attachment) = read_answer(&mut stream).await?;
    Ok((peer, answer, attachment))
}

/// an answer with the bytes after it, put back in place for thumbnails
async fn read_answer<S: AsyncRead + Unpin>(stream: &mut S) -> io::Result<(Message, Vec<u8>)> {
    let mut answer = read(stream).await?;
    if let Message::Peeked { attachments, .. } = &answer {
        if attachments.iter().any(|&a| a > MAX_THUMBNAIL)
            || attachments.iter().map(|&a| a as usize).sum::<usize>() > MAX_HISTORY_THUMBNAILS
        {
            return Err(invalid("thumbnails too large"));
        }
    }
    let size = match &answer {
        Message::Peeked { attachment, .. } if *attachment > MAX_THUMBNAIL => {
            return Err(invalid("thumbnail too large"));
        }
        Message::Peeked { attachment, .. } => *attachment as usize,
        Message::Fetched { size, .. } if *size as usize > MAX_CONTENT_SIZE => {
            return Err(invalid("clipboard content too large"));
        }
        Message::Fetched { size, .. } => *size as usize,
        _ => 0,
    };
    let mut bytes = vec![0; size];
    stream.read_exact(&mut bytes).await?;
    if let Message::Peeked {
        seen:
            Some(ClipboardSeen::Holds {
                peek: ClipboardPeek::Image { thumbnail, .. },
                ..
            }),
        ..
    } = &mut answer
    {
        *thumbnail = std::mem::take(&mut bytes);
    }
    if let Message::Peeked {
        history: Some(HistoryAnswer::Items { items, .. }),
        attachments,
        ..
    } = &mut answer
    {
        let mut images = items.iter_mut().filter_map(|item| match &mut item.peek {
            ClipboardPeek::Image { thumbnail, .. } => Some(thumbnail),
            _ => None,
        });
        for &size in attachments.iter() {
            let mut thumbnail = vec![0; size as usize];
            stream.read_exact(&mut thumbnail).await?;
            if let Some(image) = images.next() {
                *image = thumbnail;
            }
        }
    }
    Ok((answer, bytes))
}

/// [`Message::Peeked`], the thumbnail of an image after it, then those of the
/// history
async fn write_peeked<S: AsyncWrite + Unpin>(
    stream: &mut S,
    mut seen: Option<ClipboardSeen>,
    mut history: Option<HistoryAnswer>,
) -> io::Result<()> {
    let mut thumbnail = match &mut seen {
        Some(ClipboardSeen::Holds {
            peek: ClipboardPeek::Image { thumbnail, .. },
            ..
        }) => std::mem::take(thumbnail),
        _ => Vec::new(),
    };
    if thumbnail.len() > MAX_THUMBNAIL as usize {
        thumbnail.clear();
    }
    let attachment = thumbnail.len() as u32;
    // the thumbnails of the history in their order, as long as they fit
    let mut thumbnails = Vec::new();
    if let Some(HistoryAnswer::Items { items, .. }) = &mut history {
        let mut total = 0;
        for item in items.iter_mut() {
            if let ClipboardPeek::Image { thumbnail, .. } = &mut item.peek {
                let small = std::mem::take(thumbnail);
                let fits = small.len() <= MAX_THUMBNAIL as usize
                    && total + small.len() <= MAX_HISTORY_THUMBNAILS;
                total += if fits { small.len() } else { 0 };
                thumbnails.push(if fits { small } else { Vec::new() });
            }
        }
    }
    let attachments = thumbnails.iter().map(|t| t.len() as u32).collect();
    let message = Message::Peeked {
        seen,
        attachment,
        history,
        attachments,
    };
    write(stream, &message).await?;
    stream.write_all(&thumbnail).await?;
    for small in thumbnails {
        stream.write_all(&small).await?;
    }
    stream.flush().await
}

/// [`Message::Fetched`], the content after it
async fn write_fetched<S: AsyncWrite + Unpin>(stream: &mut S, content: Content) -> io::Result<()> {
    let message = Message::Fetched {
        kind: content.kind.into(),
        hash: content.hash,
        size: content.data.len() as u32,
    };
    write(stream, &message).await?;
    stream.write_all(&content.data).await?;
    stream.flush().await
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

async fn write<S: AsyncWrite + Unpin>(stream: &mut S, message: &Message) -> io::Result<()> {
    let data = serde_json::to_vec(message).map_err(io::Error::other)?;
    stream.write_all(&(data.len() as u32).to_be_bytes()).await?;
    stream.write_all(&data).await?;
    stream.flush().await
}

async fn read<S: AsyncRead + Unpin>(stream: &mut S) -> io::Result<Message> {
    let mut length = [0u8; 4];
    stream.read_exact(&mut length).await?;
    let length = u32::from_be_bytes(length);
    if length > MAX_MESSAGE {
        return Err(invalid("arrangement message too large"));
    }
    let mut data = vec![0; length as usize];
    stream.read_exact(&mut data).await?;
    serde_json::from_slice(&data).map_err(|e| invalid(&e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_channel::mpsc::channel;
    use rustinput_ipc::layout::Machine;

    fn layout() -> Layout {
        Layout {
            revision: 7,
            machines: vec![Machine {
                id: "a".into(),
                name: "a".into(),
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
                monitors: Vec::new(),
                os: "linux".into(),
                hostname: Some("a".into()),
                ips: Vec::new(),
                port: 4242,
            }],
            whole_edges: Vec::new(),
        }
    }

    /// one question over an in-memory stream, without TLS
    async fn exchange(arrangement: &Arrangement, message: Message, authorized: bool) -> Message {
        let (mut ours, mut theirs) = tokio::io::duplex(64 * 1024);
        let addr: SocketAddr = "192.168.1.51:4242".parse().unwrap();
        let serving = arrangement.serve(&mut theirs, "peer", authorized, addr);
        let asking = async {
            write(&mut ours, &message).await.unwrap();
            read(&mut ours).await.unwrap()
        };
        let (served, answer) = tokio::join!(serving, asking);
        served.unwrap();
        answer
    }

    #[tokio::test]
    async fn describes_itself_to_authorized_devices_only() {
        let (tx, _rx) = channel();
        let arrangement = Arrangement::new("xeon".into(), 3, tx);
        match exchange(&arrangement, Message::Describe, true).await {
            Message::Description(d) => {
                assert_eq!(d.name, "xeon");
                assert_eq!(d.revision, 3);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            exchange(&arrangement, Message::Describe, false).await,
            Message::Refused { .. }
        ));
    }

    #[tokio::test]
    async fn takes_an_arrangement_from_authorized_devices_only() {
        let (tx, mut rx) = channel();
        let arrangement = Arrangement::new("xeon".into(), 0, tx);
        let arrange = || Message::Arrange {
            layout: layout(),
            you: "b".into(),
        };
        assert!(matches!(
            exchange(&arrangement, arrange(), false).await,
            Message::Refused { .. }
        ));
        assert!(matches!(
            exchange(&arrangement, arrange(), true).await,
            Message::Arranged
        ));
        match rx.recv().await.unwrap() {
            ArrangementEvent::Received {
                layout, you, from, ..
            } => {
                assert_eq!(layout.revision, 7);
                assert_eq!(you, "b");
                assert_eq!(from, "peer");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn identifies_its_monitors_for_authorized_devices_only() {
        let (tx, mut rx) = channel();
        let arrangement = Arrangement::new("xeon".into(), 0, tx);
        assert!(matches!(
            exchange(&arrangement, Message::Identify, false).await,
            Message::Refused { .. }
        ));
        assert!(
            timeout(Duration::from_millis(50), rx.recv()).await.is_err(),
            "nothing shown for a stranger"
        );
        assert!(matches!(
            exchange(&arrangement, Message::Identify, true).await,
            Message::Identified
        ));
        assert!(matches!(
            rx.recv().await.unwrap(),
            ArrangementEvent::Identify { .. }
        ));
    }

    /// one question over an in-memory stream, the answer with what follows
    /// it, the service answering with `service`
    async fn ask_service(
        arrangement: &Arrangement,
        rx: &mut local_channel::mpsc::Receiver<ArrangementEvent>,
        message: Message,
        authorized: bool,
        service: impl FnOnce(ArrangementEvent),
    ) -> (Message, Vec<u8>) {
        let (mut ours, mut theirs) = tokio::io::duplex(64 * 1024);
        let addr: SocketAddr = "192.168.1.51:4242".parse().unwrap();
        let serving = arrangement.serve(&mut theirs, "peer", authorized, addr);
        let asking = async {
            write(&mut ours, &message).await.unwrap();
            read_answer(&mut ours).await.unwrap()
        };
        let answering = async {
            if authorized {
                service(rx.recv().await.unwrap());
            }
        };
        let (served, answer, ()) = tokio::join!(serving, asking, answering);
        served.unwrap();
        answer
    }

    #[tokio::test]
    async fn tells_authorized_devices_what_the_clipboard_holds() {
        let (tx, mut rx) = channel();
        let arrangement = Arrangement::new("xeon".into(), 0, tx);
        // a stranger hears nothing, the service is not even asked
        let (answer, _) = ask_service(
            &arrangement,
            &mut rx,
            Message::Peek {
                known: None,
                history: None,
            },
            false,
            |_| {},
        )
        .await;
        assert!(matches!(answer, Message::Refused { .. }));
        assert!(timeout(Duration::from_millis(50), rx.recv()).await.is_err());

        // an image: its thumbnail comes after the message
        let thumbnail: Vec<u8> = (0..=255).cycle().take(70_000).collect();
        let image = ClipboardSeen::Holds {
            hash: 42,
            peek: ClipboardPeek::Image {
                width: 1920,
                height: 1080,
                thumbnail: thumbnail.clone(),
            },
        };
        let sent = image.clone();
        let (answer, _) = ask_service(
            &arrangement,
            &mut rx,
            Message::Peek {
                known: Some(7),
                history: None,
            },
            true,
            move |event| match event {
                ArrangementEvent::Peek { known, done, .. } => {
                    assert_eq!(known, Some(7));
                    let _ = done.send((Some(sent), None));
                }
                other => panic!("unexpected {other:?}"),
            },
        )
        .await;
        match answer {
            Message::Peeked {
                seen, attachment, ..
            } => {
                assert_eq!(attachment as usize, thumbnail.len());
                assert_eq!(seen, Some(image));
            }
            other => panic!("unexpected {other:?}"),
        }

        // unchanged, and nobody to read it (no clipboard sharing here)
        let (answer, _) = ask_service(
            &arrangement,
            &mut rx,
            Message::Peek {
                known: Some(42),
                history: None,
            },
            true,
            |event| match event {
                ArrangementEvent::Peek { done, .. } => {
                    let _ = done.send((None, None));
                }
                other => panic!("unexpected {other:?}"),
            },
        )
        .await;
        assert!(matches!(answer, Message::Peeked { seen: None, .. }));
        let (answer, _) = ask_service(
            &arrangement,
            &mut rx,
            Message::Peek {
                known: None,
                history: None,
            },
            true,
            drop,
        )
        .await;
        assert!(matches!(
            answer,
            Message::Peeked {
                seen: Some(ClipboardSeen::Hidden),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn sends_the_clipboard_to_authorized_devices_that_ask() {
        let (tx, mut rx) = channel();
        let arrangement = Arrangement::new("xeon".into(), 0, tx);
        let (answer, _) = ask_service(
            &arrangement,
            &mut rx,
            Message::Fetch { hash: 1 },
            false,
            |_| {},
        )
        .await;
        assert!(matches!(answer, Message::Refused { .. }));

        // a text comes after the answer, as it is
        let text = "olá ".repeat(100_000).into_bytes();
        let content = Content {
            kind: ClipboardKind::Text,
            data: text.clone(),
            hash: 9,
        };
        let (answer, data) = ask_service(
            &arrangement,
            &mut rx,
            Message::Fetch { hash: 9 },
            true,
            move |event| match event {
                ArrangementEvent::Fetch {
                    hash, from, done, ..
                } => {
                    assert_eq!((hash, from.as_str()), (9, "peer"));
                    let _ = done.send(Ok(Fetched::Data(content)));
                }
                other => panic!("unexpected {other:?}"),
            },
        )
        .await;
        match answer {
            Message::Fetched { kind, hash, size } => {
                assert_eq!(ClipboardKind::try_from(kind).unwrap(), ClipboardKind::Text);
                assert_eq!((hash, size as usize), (9, text.len()));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(data, text);

        // files go as a transfer; a changed clipboard is said so
        let (answer, _) = ask_service(
            &arrangement,
            &mut rx,
            Message::Fetch { hash: 5 },
            true,
            |event| match event {
                ArrangementEvent::Fetch { done, .. } => {
                    let _ = done.send(Ok(Fetched::Files(3)));
                }
                other => panic!("unexpected {other:?}"),
            },
        )
        .await;
        assert!(matches!(answer, Message::FilesSent { count: 3 }));
        let (answer, _) = ask_service(
            &arrangement,
            &mut rx,
            Message::Fetch { hash: 5 },
            true,
            |event| match event {
                ArrangementEvent::Fetch { done, .. } => {
                    let _ = done.send(Err("the clipboard changed meanwhile".into()));
                }
                other => panic!("unexpected {other:?}"),
            },
        )
        .await;
        match answer {
            Message::Refused { reason } => assert_eq!(reason, "the clipboard changed meanwhile"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_history_comes_with_its_thumbnails() {
        let (tx, mut rx) = channel();
        let arrangement = Arrangement::new("xeon".into(), 0, tx);
        let item = |hash: u64, peek: ClipboardPeek| rustinput_ipc::ClipboardHistoryItem {
            hash,
            when: 1_700_000_000 + hash,
            peek,
        };
        let image = |n: u8| ClipboardPeek::Image {
            width: 640,
            height: 480,
            thumbnail: vec![n; 1000 + n as usize],
        };
        let text = ClipboardPeek::Text {
            start: "olá".into(),
            chars: 3,
            lines: 1,
            formatted: false,
        };
        let history = HistoryAnswer::Items {
            version: 4,
            items: vec![item(1, image(1)), item(2, text), item(3, image(3))],
        };
        let current = ClipboardSeen::Holds {
            hash: 1,
            peek: image(9),
        };
        let sent = (Some(current.clone()), Some(history.clone()));
        let (answer, _) = ask_service(
            &arrangement,
            &mut rx,
            Message::Peek {
                known: None,
                history: Some(2),
            },
            true,
            move |event| match event {
                ArrangementEvent::Peek { history, done, .. } => {
                    assert_eq!(history, Some(2));
                    let _ = done.send(sent);
                }
                other => panic!("unexpected {other:?}"),
            },
        )
        .await;
        match answer {
            Message::Peeked {
                seen,
                history: received,
                attachments,
                ..
            } => {
                assert_eq!(seen, Some(current));
                assert_eq!(attachments, vec![1001, 1003]);
                assert_eq!(received, Some(history), "every thumbnail back in place");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_peek_without_history_is_what_0_34_sends_and_reads() {
        // 0.34.0 asks without `history` and answers without it
        let asked: Message = serde_json::from_str(r#"{"Peek":{"known":7}}"#).unwrap();
        assert!(matches!(
            asked,
            Message::Peek {
                known: Some(7),
                history: None
            }
        ));
        let answered: Message =
            serde_json::from_str(r#"{"Peeked":{"seen":null,"attachment":0}}"#).unwrap();
        assert!(matches!(
            answered,
            Message::Peeked {
                history: None,
                ref attachments,
                ..
            } if attachments.is_empty()
        ));
    }

    #[tokio::test]
    async fn answers_that_announce_too_much_are_refused() {
        for answer in [
            Message::Peeked {
                seen: None,
                attachment: MAX_THUMBNAIL + 1,
                history: None,
                attachments: Vec::new(),
            },
            Message::Peeked {
                seen: None,
                attachment: 0,
                history: None,
                attachments: vec![MAX_THUMBNAIL; 3],
            },
            Message::Fetched {
                kind: 0,
                hash: 0,
                size: MAX_CONTENT_SIZE as u32 + 1,
            },
        ] {
            let (mut ours, mut theirs) = tokio::io::duplex(1024);
            write(&mut theirs, &answer).await.unwrap();
            assert!(read_answer(&mut ours).await.is_err());
        }
    }

    #[tokio::test]
    async fn locks_its_screen_for_authorized_devices_only_and_says_how_it_went() {
        let (tx, mut rx) = channel();
        let arrangement = Arrangement::new("xeon".into(), 0, tx);
        assert!(matches!(
            exchange(&arrangement, Message::Lock, false).await,
            Message::Refused { .. }
        ));
        assert!(
            timeout(Duration::from_millis(50), rx.recv()).await.is_err(),
            "not locked for a stranger"
        );
        let failed = "no screen locker answered".to_owned();
        for (result, expected) in [(Ok(()), None), (Err(failed.clone()), Some(failed))] {
            // the service locks the screen and says how it went
            let service = async {
                match rx.recv().await.unwrap() {
                    ArrangementEvent::Lock { done, .. } => {
                        let _ = done.send(result);
                    }
                    other => panic!("unexpected {other:?}"),
                }
            };
            let (answer, ()) = tokio::join!(exchange(&arrangement, Message::Lock, true), service);
            match answer {
                Message::Locked { error } => assert_eq!(error, expected),
                other => panic!("unexpected {other:?}"),
            }
        }
    }
}
