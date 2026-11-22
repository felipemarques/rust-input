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
//!
//! All are answered only to authorized devices: an arrangement changes
//! where the pointer goes, and the monitors are no business of strangers.

use std::{cell::RefCell, fmt, io, net::SocketAddr, rc::Rc, sync::Arc, time::Duration};

use local_channel::mpsc::Sender;
use rustinput_ipc::layout::{Layout, Monitor};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};
use tokio_rustls::TlsConnector;
use webrtc_dtls::crypto::Certificate;

use crate::files::tls;

/// first bytes of a connection about the arrangement, both ways
pub(crate) const MAGIC: [u8; 4] = *b"RIA1";
const MAX_MESSAGE: u32 = 256 * 1024;
/// connecting and one question and answer
const TIMEOUT: Duration = Duration::from_secs(10);
/// longest name accepted from the other device
const MAX_NAME: usize = 64;

#[derive(Debug, Serialize, Deserialize)]
enum Message {
    Describe,
    Description(Description),
    Arrange { layout: Layout, you: String },
    Arranged,
    Identify,
    Identified,
    Refused { reason: String },
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
        let message = read(stream).await?;
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
}

impl fmt::Display for ArrangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArrangeError::Unreachable(why) => write!(f, "{why}"),
            ArrangeError::Refused(why) => write!(f, "refused: {why}"),
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
        // an older RustInput closes the connection on a message it does not
        // know
        Err(ArrangeError::Unreachable(e)) if e.contains("eof") || e.contains("closed") => Err(
            ArrangeError::Unreachable("it runs an older RustInput, without this".into()),
        ),
        Err(e) => Err(e),
    }
}

/// `message` to the first of `addrs` that answers, and its answer
async fn ask(
    cert: Certificate,
    addrs: Vec<SocketAddr>,
    expected: Option<String>,
    message: Message,
) -> Result<(String, Message), ArrangeError> {
    let config = match &expected {
        Some(fingerprint) => tls::client_config(&cert, fingerprint.clone()),
        None => tls::any_server_config(&cert),
    }
    .map_err(|e| ArrangeError::Unreachable(format!("TLS: {e}")))?;
    let mut last = ArrangeError::Unreachable("its address is not known yet".into());
    for addr in addrs {
        match timeout(TIMEOUT, attempt(config.clone(), addr, &message)).await {
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
) -> io::Result<(String, Message)> {
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
    let answer = read(&mut stream).await?;
    Ok((peer, answer))
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
}
