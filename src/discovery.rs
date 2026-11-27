//! Finding the other computers that run RustInput on the local network
//! (0.40.0).
//!
//! Every RustInput with `discovery` on (off by default, the user switches it
//! on: it sends the name of the computer to the network) tells the others about
//! itself with UDP broadcasts on [`DISCOVERY_PORT`]: its name, the
//! fingerprint of its certificate, its port, version and system. It does
//! when it starts, every [`ANNOUNCE_EVERY`] and when asked: while a window
//! shows the computers, this one asks every [`QUERY_EVERY`] and the others
//! answer it right away. Turned off or closing, it says goodbye.
//!
//! What is found is only a suggestion: adding a computer found this way does
//! not authorize it, the code of the pairing does (anyone on the network can
//! send these broadcasts).
//!
//! Two sockets: one on [`DISCOVERY_PORT`], shared with other programs on the
//! computer (several RustInput of the tests, for instance), for broadcasts,
//! and one of its own for the answers to its questions, which are sent to
//! it alone.

use std::{
    collections::HashMap,
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::{Duration, Instant},
};

use rustinput_ipc::Discovered;
use serde::{Deserialize, Serialize};
use tokio::{
    net::UdpSocket,
    sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
};

/// the UDP port the computers tell each other about themselves on
pub(crate) const DISCOVERY_PORT: u16 = 4241;
/// "RustInput Discovery", version 1
const MAGIC: &str = "RID1";
const ANNOUNCE_EVERY: Duration = Duration::from_secs(60);
const QUERY_EVERY: Duration = Duration::from_secs(5);
/// a computer not heard of for this long is gone from the list
const FORGET_AFTER: Duration = Duration::from_secs(3 * ANNOUNCE_EVERY.as_secs());
/// largest message taken
const MAX_MESSAGE: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    /// here I am, to everyone
    Announce,
    /// who is there? to everyone, answered to the sender alone
    Query,
    /// here I am, to who asked
    Answer,
    /// I am gone (turned off or closing)
    Bye,
}

/// what goes in every datagram, as JSON
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Message {
    magic: String,
    kind: Kind,
    name: String,
    fingerprint: String,
    port: u16,
    version: String,
    os: String,
}

/// this computer, as it tells the others
#[derive(Clone, Debug)]
pub(crate) struct Me {
    pub(crate) name: String,
    pub(crate) fingerprint: String,
    pub(crate) port: u16,
}

impl Me {
    fn message(&self, kind: Kind) -> Message {
        Message {
            magic: MAGIC.into(),
            kind,
            name: self.name.clone(),
            fingerprint: self.fingerprint.clone(),
            port: self.port,
            version: env!("CARGO_PKG_VERSION").into(),
            os: std::env::consts::OS.into(),
        }
    }
}

/// at most `max` characters, without control characters nor those that
/// turn the text around
fn clean(text: &str, max: usize) -> String {
    rustinput_ipc::plain_text(text, max)
}

/// a datagram of another RustInput, if it is one
fn parse(bytes: &[u8]) -> Option<Message> {
    let message: Message = serde_json::from_slice(bytes).ok()?;
    if message.magic != MAGIC || message.fingerprint.is_empty() || message.port == 0 {
        return None;
    }
    Some(Message {
        name: clean(&message.name, 64),
        fingerprint: clean(&message.fingerprint, 128),
        version: clean(&message.version, 32),
        os: clean(&message.os, 32),
        ..message
    })
}

enum Request {
    /// a window shows the computers: ask for them now and then
    Look(bool),
    /// tell about this computer and look for the others, or neither
    Enable(bool),
    /// the port of this computer changed
    Port(u16),
}

/// what the service uses to talk to the discovery
#[derive(Clone)]
pub(crate) struct DiscoveryHandle {
    tx: UnboundedSender<Request>,
}

impl DiscoveryHandle {
    /// whether a window shows the computers found
    pub(crate) fn look(&self, look: bool) {
        let _ = self.tx.send(Request::Look(look));
    }

    pub(crate) fn enable(&self, enabled: bool) {
        let _ = self.tx.send(Request::Enable(enabled));
    }

    pub(crate) fn set_port(&self, port: u16) {
        let _ = self.tx.send(Request::Port(port));
    }
}

/// A computer heard of.
struct Seen {
    message: Message,
    address: IpAddr,
    at: Instant,
}

/// Starts the discovery on `port` (normally [`DISCOVERY_PORT`]); every
/// change of the computers found goes to `found`.
pub(crate) fn spawn(
    me: Me,
    port: u16,
    enabled: bool,
    found: UnboundedSender<Vec<Discovered>>,
) -> DiscoveryHandle {
    let (tx, rx) = unbounded_channel();
    tokio::task::spawn_local(run(me, port, enabled, found, rx));
    DiscoveryHandle { tx }
}

/// a UDP socket on `port` (0: any), for broadcasts, shared with others on
/// the same port
fn socket(port: u16) -> io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    if port != 0 {
        socket.set_reuse_address(true)?;
    }
    socket.set_broadcast(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into())?;
    UdpSocket::from_std(socket.into())
}

struct Discovery {
    me: Me,
    port: u16,
    enabled: bool,
    looking: bool,
    shared: Option<UdpSocket>,
    own: Option<UdpSocket>,
    seen: HashMap<String, Seen>,
    found: UnboundedSender<Vec<Discovered>>,
    last: Option<Vec<Discovered>>,
}

impl Discovery {
    fn open(&mut self) {
        match (socket(self.port), socket(0)) {
            (Ok(shared), Ok(own)) => {
                self.shared = Some(shared);
                self.own = Some(own);
                log::info!("discovery: on UDP port {}", self.port);
            }
            (Err(e), _) | (_, Err(e)) => {
                log::warn!("discovery: UDP port {} not available: {e}", self.port);
            }
        }
    }

    async fn close(&mut self) {
        self.send(Kind::Bye, self.broadcast()).await;
        self.shared = None;
        self.own = None;
        self.seen.clear();
        self.report();
    }

    fn broadcast(&self) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::BROADCAST, self.port))
    }

    async fn send(&self, kind: Kind, to: SocketAddr) {
        let Some(own) = &self.own else {
            return;
        };
        let Ok(bytes) = serde_json::to_vec(&self.me.message(kind)) else {
            return;
        };
        if let Err(e) = own.send_to(&bytes, to).await {
            log::debug!("discovery: {kind:?} to {to}: {e}");
        }
    }

    /// a datagram from `from`
    async fn heard(&mut self, bytes: &[u8], from: SocketAddr) {
        let Some(message) = parse(bytes) else {
            return;
        };
        if message.fingerprint == self.me.fingerprint {
            return;
        }
        match message.kind {
            Kind::Query => self.send(Kind::Answer, from).await,
            Kind::Bye => {
                if self.seen.remove(&message.fingerprint).is_some() {
                    self.report();
                }
                return;
            }
            Kind::Announce | Kind::Answer => {}
        }
        let fingerprint = message.fingerprint.clone();
        self.seen.insert(
            fingerprint,
            Seen {
                message,
                address: from.ip(),
                at: Instant::now(),
            },
        );
        self.report();
    }

    /// forgets the computers not heard of for long
    fn forget(&mut self) {
        let before = self.seen.len();
        self.seen.retain(|_, s| s.at.elapsed() < FORGET_AFTER);
        if self.seen.len() != before {
            self.report();
        }
    }

    /// sends the computers found if that changed
    fn report(&mut self) {
        let mut found: Vec<Discovered> = self
            .seen
            .values()
            .map(|s| Discovered {
                name: s.message.name.clone(),
                address: s.address,
                port: s.message.port,
                fingerprint: s.message.fingerprint.clone(),
                version: s.message.version.clone(),
                os: s.message.os.clone(),
                authorized: false,
                added: false,
            })
            .collect();
        found.sort_by(|a, b| a.name.cmp(&b.name).then(a.address.cmp(&b.address)));
        if self.last.as_ref() != Some(&found) {
            let _ = self.found.send(found.clone());
            self.last = Some(found);
        }
    }
}

/// receives on `socket`, or waits forever without one
async fn receive(socket: Option<&UdpSocket>, buffer: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
    match socket {
        Some(socket) => socket.recv_from(buffer).await,
        None => std::future::pending().await,
    }
}

async fn run(
    me: Me,
    port: u16,
    enabled: bool,
    found: UnboundedSender<Vec<Discovered>>,
    mut requests: UnboundedReceiver<Request>,
) {
    let mut d = Discovery {
        me,
        port,
        enabled,
        looking: false,
        shared: None,
        own: None,
        seen: HashMap::new(),
        found,
        last: None,
    };
    if d.enabled {
        d.open();
        d.send(Kind::Announce, d.broadcast()).await;
    }
    let mut announce = tokio::time::interval(ANNOUNCE_EVERY);
    announce.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut query = tokio::time::interval(QUERY_EVERY);
    query.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let (mut shared_buffer, mut own_buffer) = ([0; MAX_MESSAGE], [0; MAX_MESSAGE]);
    loop {
        tokio::select! {
            request = requests.recv() => match request {
                None => break,
                Some(Request::Look(look)) => {
                    d.looking = look;
                    if look && d.enabled {
                        d.send(Kind::Query, d.broadcast()).await;
                    }
                }
                Some(Request::Enable(enabled)) if enabled != d.enabled => {
                    d.enabled = enabled;
                    if enabled {
                        d.open();
                        d.send(Kind::Announce, d.broadcast()).await;
                    } else {
                        d.close().await;
                        log::info!("discovery: off");
                    }
                }
                Some(Request::Enable(_)) => {}
                Some(Request::Port(port)) => {
                    d.me.port = port;
                    if d.enabled {
                        d.send(Kind::Announce, d.broadcast()).await;
                    }
                }
            },
            received = receive(d.shared.as_ref(), &mut shared_buffer) => match received {
                Ok((length, from)) => {
                    let bytes = shared_buffer[..length].to_vec();
                    d.heard(&bytes, from).await;
                }
                Err(e) => log::debug!("discovery: {e}"),
            },
            received = receive(d.own.as_ref(), &mut own_buffer) => match received {
                Ok((length, from)) => {
                    let bytes = own_buffer[..length].to_vec();
                    d.heard(&bytes, from).await;
                }
                Err(e) => log::debug!("discovery: {e}"),
            },
            _ = announce.tick(), if d.enabled => {
                d.send(Kind::Announce, d.broadcast()).await;
                d.forget();
            }
            _ = query.tick(), if d.enabled && d.looking => {
                d.send(Kind::Query, d.broadcast()).await;
                d.forget();
            }
        }
    }
    if d.enabled {
        d.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me(name: &str, fingerprint: &str) -> Me {
        Me {
            name: name.into(),
            fingerprint: fingerprint.into(),
            port: 4242,
        }
    }

    #[test]
    fn messages_of_other_programs_are_ignored() {
        let good = serde_json::to_vec(&me("laptop", "aa:bb").message(Kind::Announce)).unwrap();
        let message = parse(&good).unwrap();
        assert_eq!(
            (message.name.as_str(), message.kind),
            ("laptop", Kind::Announce)
        );
        assert!(parse(b"hello").is_none());
        let mut other = me("x", "aa").message(Kind::Query);
        other.magic = "XYZ1".into();
        assert!(parse(&serde_json::to_vec(&other).unwrap()).is_none());
        // names without control characters, not too long
        let long = me(&format!("a\u{202e}b\nc{}", "x".repeat(100)), "aa").message(Kind::Answer);
        let message = parse(&serde_json::to_vec(&long).unwrap()).unwrap();
        assert!(message.name.starts_with("abc"));
        assert_eq!(message.name.chars().count(), 64);
    }

    // broadcasts to this computer itself, which CI runners of other systems
    // may not have
    #[cfg_attr(not(target_os = "linux"), ignore)]
    #[tokio::test]
    async fn computers_find_each_other() {
        // a port of its own, not the one of a RustInput running here
        let port = 4290;
        let (found_a, mut a_found) = unbounded_channel();
        let (found_b, _b_found) = unbounded_channel();
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let a = spawn(me("A", "fa"), port, true, found_a);
                let _b = spawn(me("B", "fb"), port, true, found_b);
                a.look(true);
                let list = tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        let list = a_found.recv().await.unwrap();
                        if !list.is_empty() {
                            return list;
                        }
                    }
                })
                .await
                .expect("B is found");
                assert_eq!(list.len(), 1);
                assert_eq!(
                    (list[0].name.as_str(), list[0].fingerprint.as_str()),
                    ("B", "fb")
                );
                assert_eq!(list[0].port, 4242);
                drop(_b);
                // B says goodbye
                let gone = tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        if a_found.recv().await.unwrap().is_empty() {
                            return;
                        }
                    }
                })
                .await;
                assert!(gone.is_ok(), "B is gone from the list");
            })
            .await;
    }
}
