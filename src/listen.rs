use futures::{Stream, StreamExt};
use local_channel::mpsc::{Receiver, Sender, channel};
use rustinput_proto::{
    CAP_CURSOR_POSITION, CAP_EDGE_SPAN, CAP_PAUSE, Datagram, HANDSHAKE_PROBE, MAX_DATAGRAM_SIZE,
    MAX_EVENT_SIZE, ProtoEvent,
};
use rustls::pki_types::CertificateDer;
use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    net::SocketAddr,
    rc::Rc,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};
use thiserror::Error;
use tokio::{
    sync::Mutex as AsyncMutex,
    task::{JoinHandle, spawn_local},
};
use webrtc_dtls::{
    config::{ClientAuthType::RequireAnyClientCert, Config, ExtendedMasterSecretType},
    conn::DTLSConn,
    crypto::Certificate,
    listener::listen,
};
use webrtc_util::{Conn, Error, conn::Listener};

use crate::clipboard::{ClipboardHandle, ClipboardMsg, Peer};
use crate::config::local_commit;
use crate::crypto;
use crate::netstats::NetStats;

/// the connection attempts of strangers kept for the window, the latest
const MAX_ATTEMPTS_KEPT: usize = 16;

#[derive(Error, Debug)]
pub enum ListenerCreationError {
    #[error(transparent)]
    WebrtcUtil(#[from] webrtc_util::Error),
    #[error(transparent)]
    WebrtcDtls(#[from] webrtc_dtls::Error),
}

type ArcConn = Arc<dyn Conn + Send + Sync>;

/// optional features announced by the connected devices
type PeerCapabilities = Rc<RefCell<HashMap<SocketAddr, u32>>>;

/// sends raw datagrams over the connections accepted by the listener
#[derive(Clone)]
pub(crate) struct IncomingLinks {
    conns: Rc<AsyncMutex<Vec<(SocketAddr, ArcConn)>>>,
}

impl IncomingLinks {
    /// returns whether the datagram was handed to a connection
    pub(crate) async fn send(&self, addr: SocketAddr, datagram: &[u8]) -> bool {
        let conn = self
            .conns
            .lock()
            .await
            .iter()
            .find(|(a, _)| *a == addr)
            .map(|(_, c)| c.clone());
        match conn {
            Some(conn) => conn.send(datagram).await.is_ok(),
            None => false,
        }
    }
}

pub(crate) enum ListenEvent {
    Msg {
        event: ProtoEvent,
        addr: SocketAddr,
    },
    Accept {
        addr: SocketAddr,
        fingerprint: String,
    },
    Rejected {
        fingerprint: String,
    },
}

pub(crate) struct RustInputListener {
    listen_rx: Receiver<ListenEvent>,
    listen_tx: Sender<ListenEvent>,
    listen_task: JoinHandle<()>,
    conns: Rc<AsyncMutex<Vec<(SocketAddr, ArcConn)>>>,
    request_port_change: Sender<u16>,
    port_changed: Receiver<Result<u16, ListenerCreationError>>,
    peer_capabilities: PeerCapabilities,
}

type VerifyPeerCertificateFn = Arc<
    dyn (Fn(&[Vec<u8>], &[CertificateDer<'static>]) -> Result<(), webrtc_dtls::Error>)
        + Send
        + Sync,
>;

impl RustInputListener {
    pub(crate) async fn new(
        port: u16,
        cert: Certificate,
        authorized_keys: Arc<RwLock<HashMap<String, String>>>,
        clipboard: ClipboardHandle,
        netstats: NetStats,
    ) -> Result<Self, ListenerCreationError> {
        let (listen_tx, listen_rx) = channel();
        let (request_port_change, mut request_port_change_rx) = channel();
        let (port_changed_tx, port_changed) = channel();
        let connection_attempts: Arc<Mutex<VecDeque<String>>> = Default::default();

        let authorized = authorized_keys.clone();
        let verify_peer_certificate: Option<VerifyPeerCertificateFn> = {
            let connection_attempts = connection_attempts.clone();
            Some(Arc::new(
                move |certs: &[Vec<u8>], _chains: &[CertificateDer<'static>]| {
                    // exactly one certificate: anything else is refused (it
                    // was an assert, which anyone could trip to end the
                    // daemon)
                    let [cert] = certs else {
                        return Err(webrtc_dtls::Error::ErrVerifyDataMismatch);
                    };
                    let fingerprint = crypto::generate_fingerprint(cert);
                    if authorized.read().expect("lock").contains_key(&fingerprint) {
                        Ok(())
                    } else {
                        let mut attempts = connection_attempts.lock().expect("lock");
                        // the latest ones; strangers can try without end
                        if attempts.len() >= MAX_ATTEMPTS_KEPT {
                            attempts.pop_front();
                        }
                        attempts.push_back(fingerprint);
                        Err(webrtc_dtls::Error::ErrVerifyDataMismatch)
                    }
                },
            ))
        };
        let cfg = Config {
            certificates: vec![cert.clone()],
            extended_master_secret: ExtendedMasterSecretType::Require,
            client_auth: RequireAnyClientCert,
            verify_peer_certificate,
            ..Default::default()
        };

        let listen_addr = SocketAddr::new("0.0.0.0".parse().expect("invalid ip"), port);
        let mut listener = listen(listen_addr, cfg.clone()).await?;

        let conns: Rc<AsyncMutex<Vec<(SocketAddr, ArcConn)>>> =
            Rc::new(AsyncMutex::new(Vec::new()));
        let peer_capabilities = PeerCapabilities::default();
        let capabilities = peer_capabilities.clone();

        let conns_clone = conns.clone();
        let listen_task: JoinHandle<()> = {
            let listen_tx = listen_tx.clone();
            let connection_attempts = connection_attempts.clone();
            spawn_local(async move {
                loop {
                    let sleep = tokio::time::sleep(Duration::from_secs(2));
                    tokio::select! {
                        /* workaround for https://github.com/webrtc-rs/webrtc/issues/614 */
                        _ = sleep => continue,
                        c = listener.accept() => match c {
                            Ok((conn, addr)) => {
                                log::info!("dtls client connected, ip: {addr}");
                                let conn = netstats.meter(addr, conn);
                                let mut conns = conns_clone.lock().await;
                                conns.push((addr, conn.clone()));
                                let dtls_conn: &DTLSConn = conn.as_any().downcast_ref().expect("dtls conn");
                                let certs = dtls_conn.connection_state().await.peer_certificates;
                                let cert = certs.first().expect("cert");
                                let fingerprint = crypto::generate_fingerprint(cert);
                                listen_tx.send(ListenEvent::Accept { addr, fingerprint }).expect("channel closed");
                                spawn_local(read_loop(conns_clone.clone(), addr, conn, listen_tx.clone(), clipboard.clone(), capabilities.clone()));
                            },
                            Err(e) => {
                                if let Error::Std(ref e) = e {
                                    if let Some(e) = e.0.downcast_ref::<webrtc_dtls::Error>() {
                                        match e {
                                            webrtc_dtls::Error::ErrVerifyDataMismatch => {
                                                if let Some(fingerprint) = connection_attempts.lock().expect("lock").pop_front() {
                                                    listen_tx.send(ListenEvent::Rejected { fingerprint }).expect("channel closed");
                                                }
                                            }
                                            _ => log::warn!("accept: {e}"),
                                        }
                                    } else {
                                        log::warn!("accept: {e:?}");
                                    }
                                } else {
                                    log::warn!("accept: {e:?}");
                                }
                            }
                        },
                        port = request_port_change_rx.recv() => {
                            let port = port.expect("channel closed");
                            let listen_addr = SocketAddr::new("0.0.0.0".parse().expect("invalid ip"), port);
                            match listen(listen_addr, cfg.clone()).await {
                                Ok(new_listener) => {
                                    let _ = listener.close().await;
                                    listener = new_listener;
                                    port_changed_tx.send(Ok(port)).expect("channel closed");
                                }
                                Err(e) => {
                                    log::warn!("unable to change port: {e}");
                                    port_changed_tx.send(Err(e.into())).expect("channel closed");
                                }
                            };
                        },
                    };
                }
            })
        };

        Ok(Self {
            conns,
            listen_rx,
            listen_tx,
            listen_task,
            port_changed,
            request_port_change,
            peer_capabilities,
        })
    }

    /// the optional features the device connected from `addr` announced
    pub(crate) fn peer_capabilities(&self, addr: SocketAddr) -> u32 {
        self.peer_capabilities
            .borrow()
            .get(&addr)
            .copied()
            .unwrap_or(0)
    }

    pub(crate) fn links(&self) -> IncomingLinks {
        IncomingLinks {
            conns: self.conns.clone(),
        }
    }

    pub(crate) fn request_port_change(&mut self, port: u16) {
        self.request_port_change.send(port).expect("channel closed");
    }

    pub(crate) async fn port_changed(&mut self) -> Result<u16, ListenerCreationError> {
        self.port_changed.recv().await.expect("channel closed")
    }

    pub(crate) async fn terminate(&mut self) {
        self.listen_task.abort();
        let conns = self.conns.lock().await;
        for (_, conn) in conns.iter() {
            let _ = conn.close().await;
        }
        self.listen_tx.close();
    }

    pub(crate) async fn reply(&self, addr: SocketAddr, event: ProtoEvent) {
        log::trace!("reply {event} >=>=>=>=>=> {addr}");
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
        let conns = self.conns.lock().await;
        for (a, conn) in conns.iter() {
            if *a == addr {
                let _ = conn.send(&buf[..len]).await;
            }
        }
    }

    pub(crate) async fn get_certificate_fingerprint(&self, addr: SocketAddr) -> Option<String> {
        if let Some(conn) = self
            .conns
            .lock()
            .await
            .iter()
            .find(|(a, _)| *a == addr)
            .map(|(_, c)| c.clone())
        {
            let conn: &DTLSConn = conn.as_any().downcast_ref().expect("dtls conn");
            let certs = conn.connection_state().await.peer_certificates;
            let cert = certs.first()?;
            let fingerprint = crypto::generate_fingerprint(cert);
            Some(fingerprint)
        } else {
            None
        }
    }
}

impl Stream for RustInputListener {
    type Item = ListenEvent;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.listen_rx.poll_next_unpin(cx)
    }
}

async fn read_loop(
    conns: Rc<AsyncMutex<Vec<(SocketAddr, ArcConn)>>>,
    addr: SocketAddr,
    conn: ArcConn,
    dtls_tx: Sender<ListenEvent>,
    clipboard: ClipboardHandle,
    peer_capabilities: PeerCapabilities,
) -> Result<(), Error> {
    // every connection reaching this point presented an authorized certificate
    let peer = Peer::Incoming(addr);
    let fingerprint = crate::connect::peer_fingerprint(&conn)
        .await
        .unwrap_or_default();
    let mut b = [0u8; MAX_DATAGRAM_SIZE];
    // whether the peer controls this device now (entered and did not leave)
    let mut entered = false;

    while let Ok(len) = conn.recv(&mut b).await {
        match Datagram::decode(&b[..len]) {
            Ok(Datagram::Event(ProtoEvent::Ack(HANDSHAKE_PROBE))) => {
                // The connect side asks whether we know the newer
                // events: answer with our build and features. It will
                // then send its own (see `connect::receive_loop`).
                let hello = ProtoEvent::Hello {
                    commit: local_commit(),
                };
                let flags =
                    CAP_CURSOR_POSITION | CAP_EDGE_SPAN | CAP_PAUSE | clipboard.capabilities();
                for event in [hello, ProtoEvent::Capabilities(flags)] {
                    let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
                    let _ = conn.send(&buf[..len]).await;
                }
            }
            Ok(Datagram::Event(ProtoEvent::Capabilities(flags))) => {
                peer_capabilities.borrow_mut().insert(addr, flags);
                clipboard.notify(ClipboardMsg::Capabilities {
                    peer,
                    flags,
                    fingerprint: fingerprint.clone(),
                    // it opens the file channel to this device
                    connect_to: None,
                });
            }
            Ok(Datagram::Event(event)) => {
                match event {
                    ProtoEvent::Enter(_)
                    | ProtoEvent::EnterAt(..)
                    | ProtoEvent::EnterSpan { .. } => entered = true,
                    // The peer stopped controlling this device: whatever
                    // was copied here in the meantime goes back with it.
                    // Only then: a Leave at any time would let it pull
                    // every new copy (a password from a password manager).
                    ProtoEvent::Leave(_) => {
                        let controlled = std::mem::take(&mut entered);
                        if controlled {
                            clipboard.push(peer);
                        }
                    }
                    _ => {}
                }
                dtls_tx
                    .send(ListenEvent::Msg { event, addr })
                    .expect("channel closed")
            }
            Ok(Datagram::Clipboard(frame)) => {
                clipboard.notify(ClipboardMsg::Frame { peer, frame });
            }
            Err(e) => {
                // Skip the malformed/unknown datagram and keep
                // listening. Each DTLS recv returns one full
                // datagram, so a parse error here can't desync a
                // stream; the next call gets a fresh, framed
                // message. This makes the protocol forward-
                // compatible: a peer running a newer RustInput
                // version can introduce additional event types
                // and old peers will simply ignore them rather
                // than dropping the connection.
                log::debug!("ignoring undecodable event from {addr}: {e}");
            }
        }
    }
    log::info!("dtls client disconnected {addr:?}");
    peer_capabilities.borrow_mut().remove(&addr);
    clipboard.notify(ClipboardMsg::Disconnected { peer });
    let mut conns = conns.lock().await;
    let index = conns
        .iter()
        .position(|(a, _)| *a == addr)
        .expect("connection not found");
    conns.remove(index);
    Ok(())
}
