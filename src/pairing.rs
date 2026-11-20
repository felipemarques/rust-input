//! Authorizing two devices on each other from one of them, with a code.
//!
//! A device refusing a connection from an unknown device shows a six digit
//! code along with the request. Entered on the device that tried to connect,
//! the code authorizes both devices on each other, for control, clipboard and
//! files, without going to the other one.
//!
//! The code itself never travels. Both sides run SPAKE2, a password
//! authenticated key exchange, with the code as password and the fingerprints
//! of the certificates they see in TLS as identities, then prove to each
//! other that they derived the same key. Someone in between sees different
//! certificates on each side, so the keys differ; someone guessing gets
//! [`MAX_ATTEMPTS`] tries per code, and watching an exchange does not help to
//! find the code afterwards.
//!
//! This runs on the TCP port of the file channel (see [`crate::files`]),
//! starting with [`MAGIC`] instead of the magic of the file channel.

use std::{
    cell::RefCell,
    collections::HashMap,
    fmt, io,
    net::SocketAddr,
    rc::Rc,
    time::{Duration, Instant},
};

use local_channel::mpsc::Sender;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};
use tokio_rustls::TlsConnector;
use webrtc_dtls::crypto::Certificate;

use crate::{crypto, files::tls};

/// first bytes of a pairing connection, both ways
pub(crate) const MAGIC: [u8; 4] = *b"RIP1";
/// a code stays valid this long
const CODE_LIFETIME: Duration = Duration::from_secs(10 * 60);
/// tries per code, right or wrong: a new code is made after that
const MAX_ATTEMPTS: u32 = 3;
const MAX_MESSAGE: u32 = 64 * 1024;
const TIMEOUT: Duration = Duration::from_secs(15);
const PROOF_ACCEPTING: &[u8] = b"rustinput pairing: accepting side";
const PROOF_CONNECTING: &[u8] = b"rustinput pairing: connecting side";
/// longest name accepted from the other device
const MAX_NAME: usize = 64;

#[derive(Debug, Serialize, Deserialize)]
enum Message {
    /// connecting side: its name and its SPAKE2 message
    Request {
        name: String,
        spake: Vec<u8>,
    },
    /// accepting side: its name, its SPAKE2 message and its proof of the key
    Challenge {
        name: String,
        spake: Vec<u8>,
        proof: Vec<u8>,
    },
    /// connecting side: its proof of the key
    Confirm {
        proof: Vec<u8>,
    },
    /// accepting side: both are authorized now
    Accepted,
    Refused {
        reason: String,
    },
}

/// what the accepting side reports to the service
#[derive(Debug)]
pub(crate) enum PairingEvent {
    /// the device entered the right code and is authorized now
    Paired(Paired),
    /// the code for the device was used up, this one replaces it
    NewCode { fingerprint: String, code: String },
}

/// a device authorized by pairing
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Paired {
    pub(crate) fingerprint: String,
    pub(crate) name: String,
}

struct Pending {
    code: String,
    created: Instant,
    attempts: u32,
}

impl Pending {
    fn new() -> Self {
        Self {
            code: new_code(),
            created: Instant::now(),
            attempts: 0,
        }
    }

    fn valid(&self) -> bool {
        self.created.elapsed() < CODE_LIFETIME && self.attempts < MAX_ATTEMPTS
    }
}

/// six random digits
fn new_code() -> String {
    let mut bytes = [0u8; 4];
    getrandom::fill(&mut bytes).expect("random numbers");
    format!("{:06}", u32::from_le_bytes(bytes) % 1_000_000)
}

/// the codes shown for devices that tried to connect, and the accepting side
/// of pairing
#[derive(Clone)]
pub(crate) struct Pairing {
    pending: Rc<RefCell<HashMap<String, Pending>>>,
    events: Sender<PairingEvent>,
    name: String,
}

impl Pairing {
    /// `name` is how this device introduces itself
    pub(crate) fn new(name: String, events: Sender<PairingEvent>) -> Self {
        Self {
            pending: Default::default(),
            events,
            name,
        }
    }

    /// the code to show for the device with `fingerprint`, which tried to
    /// connect
    pub(crate) fn code_for(&self, fingerprint: &str) -> String {
        let mut pending = self.pending.borrow_mut();
        pending.retain(|_, p| p.valid());
        pending
            .entry(fingerprint.to_owned())
            .or_insert_with(Pending::new)
            .code
            .clone()
    }

    /// The accepting side, on a TLS connection on which `peer` presented its
    /// certificate and this device `own`.
    pub(crate) async fn serve<S>(&self, stream: &mut S, peer: &str, own: &str) -> io::Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let Message::Request { name, spake } = read(stream).await? else {
            return Err(invalid("expected a request"));
        };
        // counted before the outcome: giving up halfway is a try as well
        let code = self
            .pending
            .borrow_mut()
            .get_mut(peer)
            .filter(|p| p.valid())
            .map(|p| {
                p.attempts += 1;
                p.code.clone()
            });
        let Some(code) = code else {
            let reason = "there is no code for this computer: try to connect from it first, \
                          the other computer then shows one";
            return write(stream, &refused(reason)).await;
        };
        let (state, outbound) = Spake2::<Ed25519Group>::start_b(
            &Password::new(code.as_bytes()),
            &Identity::new(peer.as_bytes()),
            &Identity::new(own.as_bytes()),
        );
        let key = state
            .finish(&spake)
            .map_err(|e| invalid(&format!("pairing: {e}")))?;
        let challenge = Message::Challenge {
            name: self.name.clone(),
            spake: outbound,
            proof: proof(&key, PROOF_ACCEPTING),
        };
        write(stream, &challenge).await?;
        let right = match read(stream).await {
            Ok(Message::Confirm { proof: theirs }) => theirs == proof(&key, PROOF_CONNECTING),
            _ => false,
        };
        if !right {
            log::warn!("pairing: a wrong code was entered on {}", short(peer));
            self.used_up(peer);
            return write(stream, &refused("wrong code")).await;
        }
        self.pending.borrow_mut().remove(peer);
        write(stream, &Message::Accepted).await?;
        let paired = Paired {
            fingerprint: peer.to_owned(),
            name: clean_name(&name, peer),
        };
        log::info!("pairing: {} authorized with the code", paired.name);
        let _ = self.events.send(PairingEvent::Paired(paired));
        Ok(())
    }

    /// replaces a code that was tried too often
    fn used_up(&self, peer: &str) {
        let mut pending = self.pending.borrow_mut();
        if pending.get(peer).is_some_and(|p| !p.valid()) {
            let fresh = Pending::new();
            let code = fresh.code.clone();
            pending.insert(peer.to_owned(), fresh);
            drop(pending);
            let _ = self.events.send(PairingEvent::NewCode {
                fingerprint: peer.to_owned(),
                code,
            });
        }
    }
}

/// why pairing did not work
#[derive(Debug)]
pub(crate) enum PairError {
    /// the other device answered no
    Refused(String),
    /// it could not be reached
    Unreachable(String),
}

impl fmt::Display for PairError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PairError::Refused(reason) => write!(f, "{reason}"),
            PairError::Unreachable(e) => write!(f, "the other computer can not be reached: {e}"),
        }
    }
}

impl From<io::Error> for PairError {
    fn from(e: io::Error) -> Self {
        PairError::Unreachable(e.to_string())
    }
}

/// The connecting side: pairs with the device at one of `addrs` using the
/// `code` it shows, introducing this device as `name`.
pub(crate) async fn pair(
    cert: Certificate,
    addrs: Vec<SocketAddr>,
    code: String,
    name: String,
) -> Result<Paired, PairError> {
    let own = crypto::certificate_fingerprint(&cert);
    let config =
        tls::any_server_config(&cert).map_err(|e| PairError::Unreachable(format!("TLS: {e}")))?;
    let code: String = code.chars().filter(char::is_ascii_digit).collect();
    let mut last = PairError::Unreachable("its address is not known yet".into());
    for addr in addrs {
        let attempt = attempt(config.clone(), addr, &code, &name, &own);
        match timeout(TIMEOUT, attempt).await {
            Ok(Ok(paired)) => return Ok(paired),
            // an answer from the device itself: no point in trying elsewhere
            Ok(Err(PairError::Refused(reason))) => return Err(PairError::Refused(reason)),
            Ok(Err(e)) => last = e,
            Err(_) => last = PairError::Unreachable(format!("{addr} does not answer")),
        }
    }
    Err(last)
}

async fn attempt(
    config: std::sync::Arc<rustls::ClientConfig>,
    addr: SocketAddr,
    code: &str,
    name: &str,
    own: &str,
) -> Result<Paired, PairError> {
    // up to 0.17 the other side refuses unknown certificates in TLS already
    let outdated = || {
        PairError::Refused(
            "the other computer refused the connection: it may run RustInput older than 0.18.0, \
             update it there or authorize this computer there"
                .into(),
        )
    };
    let tcp = TcpStream::connect(addr).await?;
    let mut stream = TlsConnector::from(config)
        .connect(tls::server_name(), tcp)
        .await
        .map_err(|e| {
            if e.to_string().contains("alert") {
                outdated()
            } else {
                PairError::Unreachable(e.to_string())
            }
        })?;
    stream.write_all(&MAGIC).await?;
    stream.flush().await?;
    let mut magic = [0u8; 4];
    if stream.read_exact(&mut magic).await.is_err() || magic != MAGIC {
        return Err(outdated());
    }
    let peer = tls::peer_fingerprint(stream.get_ref().1.peer_certificates())
        .ok_or_else(|| PairError::Unreachable("no certificate".into()))?;
    let (state, outbound) = Spake2::<Ed25519Group>::start_a(
        &Password::new(code.as_bytes()),
        &Identity::new(own.as_bytes()),
        &Identity::new(peer.as_bytes()),
    );
    let request = Message::Request {
        name: name.to_owned(),
        spake: outbound,
    };
    write(&mut stream, &request).await?;
    let (peer_name, key) = match read(&mut stream).await? {
        Message::Refused { reason } => return Err(PairError::Refused(reason)),
        Message::Challenge {
            name: peer_name,
            spake,
            proof: theirs,
        } => {
            let key = state
                .finish(&spake)
                .map_err(|e| PairError::Refused(format!("pairing: {e}")))?;
            if theirs != proof(&key, PROOF_ACCEPTING) {
                // the other side counts the try and shows a new code if needed
                let _ = write(&mut stream, &Message::Confirm { proof: Vec::new() }).await;
                let _ = read(&mut stream).await;
                return Err(PairError::Refused("wrong code".into()));
            }
            (peer_name, key)
        }
        _ => return Err(PairError::Refused("unexpected answer".into())),
    };
    let confirm = Message::Confirm {
        proof: proof(&key, PROOF_CONNECTING),
    };
    write(&mut stream, &confirm).await?;
    match read(&mut stream).await? {
        Message::Accepted => Ok(Paired {
            name: clean_name(&peer_name, &peer),
            fingerprint: peer,
        }),
        Message::Refused { reason } => Err(PairError::Refused(reason)),
        _ => Err(PairError::Refused("unexpected answer".into())),
    }
}

fn refused(reason: &str) -> Message {
    Message::Refused {
        reason: reason.to_owned(),
    }
}

/// proof of knowing `key`, different for each side
fn proof(key: &[u8], side: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(side);
    hasher.update(key);
    hasher.finalize().to_vec()
}

/// the name the other device gave, made safe to show and store
fn clean_name(name: &str, fingerprint: &str) -> String {
    let name: String = name
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME)
        .collect();
    let name = name.trim();
    if name.is_empty() {
        short(fingerprint)
    } else {
        name.to_owned()
    }
}

fn short(fingerprint: &str) -> String {
    fingerprint.chars().take(11).collect()
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
        return Err(invalid("pairing message too large"));
    }
    let mut data = vec![0; length as usize];
    stream.read_exact(&mut data).await?;
    serde_json::from_slice(&data).map_err(|e| invalid(&e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_channel::mpsc::channel;

    /// runs both sides over an in-memory stream, without TLS
    async fn exchange(
        pairing: &Pairing,
        entered: &str,
        connecting: (&str, &str),
        accepting: (&str, &str),
    ) -> (Result<Paired, PairError>, io::Result<()>) {
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        let connect = async {
            let (own, peer) = connecting;
            let (state, outbound) = Spake2::<Ed25519Group>::start_a(
                &Password::new(entered.as_bytes()),
                &Identity::new(own.as_bytes()),
                &Identity::new(peer.as_bytes()),
            );
            write(
                &mut a,
                &Message::Request {
                    name: "pc\u{7}-a".into(),
                    spake: outbound,
                },
            )
            .await?;
            let Message::Challenge {
                name,
                spake,
                proof: theirs,
            } = read(&mut a).await?
            else {
                return Err(PairError::Refused("refused".into()));
            };
            let key = state.finish(&spake).expect("key");
            if theirs != proof(&key, PROOF_ACCEPTING) {
                let _ = write(&mut a, &Message::Confirm { proof: Vec::new() }).await;
                let _ = read(&mut a).await;
                return Err(PairError::Refused("wrong code".into()));
            }
            write(
                &mut a,
                &Message::Confirm {
                    proof: proof(&key, PROOF_CONNECTING),
                },
            )
            .await?;
            match read(&mut a).await? {
                Message::Accepted => Ok(Paired {
                    fingerprint: peer.into(),
                    name,
                }),
                _ => Err(PairError::Refused("refused".into())),
            }
        };
        let (peer, own) = accepting;
        let accept = pairing.serve(&mut b, peer, own);
        tokio::join!(connect, accept)
    }

    fn pairing() -> (Pairing, local_channel::mpsc::Receiver<PairingEvent>) {
        let (tx, rx) = channel();
        (Pairing::new("pc-b".into(), tx), rx)
    }

    #[tokio::test]
    async fn the_right_code_authorizes_both() {
        let (pairing, mut events) = pairing();
        let code = pairing.code_for("fp-a");
        assert_eq!(code.len(), 6);
        assert_eq!(
            pairing.code_for("fp-a"),
            code,
            "the same code while it is valid"
        );
        let (connected, accepted) =
            exchange(&pairing, &code, ("fp-a", "fp-b"), ("fp-a", "fp-b")).await;
        accepted.expect("accepting side");
        assert_eq!(
            connected.expect("connecting side"),
            Paired {
                fingerprint: "fp-b".into(),
                name: "pc-b".into()
            }
        );
        match events.recv().await {
            Some(PairingEvent::Paired(p)) => {
                assert_eq!(
                    p,
                    Paired {
                        fingerprint: "fp-a".into(),
                        name: "pc-a".into()
                    }
                )
            }
            other => panic!("{other:?}"),
        }
        assert_ne!(pairing.code_for("fp-a"), "", "a used code is gone");
    }

    #[tokio::test]
    async fn wrong_codes_are_limited() {
        let (pairing, mut events) = pairing();
        let code = pairing.code_for("fp-a");
        let wrong = if code == "000000" { "000001" } else { "000000" };
        for _ in 0..MAX_ATTEMPTS {
            let (connected, accepted) =
                exchange(&pairing, wrong, ("fp-a", "fp-b"), ("fp-a", "fp-b")).await;
            assert!(connected.is_err());
            accepted.expect("accepting side");
        }
        // the code is replaced and shown again
        let fresh = match events.recv().await {
            Some(PairingEvent::NewCode { fingerprint, code }) => {
                assert_eq!(fingerprint, "fp-a");
                code
            }
            other => panic!("{other:?}"),
        };
        assert_eq!(pairing.code_for("fp-a"), fresh);
        // the old code no longer works, even if it was right
        let (connected, _) = exchange(&pairing, &code, ("fp-a", "fp-b"), ("fp-a", "fp-b")).await;
        assert!(connected.is_err() || code == fresh);
    }

    #[tokio::test]
    async fn someone_in_between_fails() {
        // each side sees another certificate than the other one has
        let (pairing, _events) = pairing();
        let code = pairing.code_for("fp-mitm");
        let (connected, _) =
            exchange(&pairing, &code, ("fp-a", "fp-mitm2"), ("fp-mitm", "fp-b")).await;
        assert!(connected.is_err());
    }

    #[tokio::test]
    async fn no_code_without_a_connection_attempt() {
        let (pairing, _events) = pairing();
        let (connected, accepted) =
            exchange(&pairing, "123456", ("fp-a", "fp-b"), ("fp-a", "fp-b")).await;
        assert!(connected.is_err());
        accepted.expect("accepting side");
    }

    #[test]
    fn names_are_cleaned() {
        assert_eq!(clean_name(" pc\u{1b}[31m ", "AB:CD:EF:01:23"), "pc[31m");
        assert_eq!(clean_name("\n", "AB:CD:EF:01:23:45"), "AB:CD:EF:01");
        assert_eq!(clean_name(&"x".repeat(100), "fp").len(), MAX_NAME);
    }

    #[test]
    fn codes_have_six_digits() {
        for _ in 0..100 {
            let code = new_code();
            assert_eq!(code.len(), 6);
            assert!(code.chars().all(|c| c.is_ascii_digit()));
        }
    }
}
