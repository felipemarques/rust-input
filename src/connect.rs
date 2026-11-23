use crate::client::ClientManager;
use crate::clipboard::{ClipboardHandle, ClipboardMsg, Peer};
use crate::config::local_commit;
use crate::crypto;
use crate::netstats::NetStats;
use local_channel::mpsc::{Receiver, Sender, channel};
use rustinput_ipc::{ClientHandle, DEFAULT_PORT};
use rustinput_proto::{
    CAP_CURSOR_POSITION, CAP_EDGE_SPAN, CAP_PAUSE, Datagram, HANDSHAKE_PROBE, MAX_DATAGRAM_SIZE,
    MAX_EVENT_SIZE, ProtoEvent,
};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    io,
    net::SocketAddr,
    rc::Rc,
    sync::{Arc, RwLock},
    time::Duration,
};
use thiserror::Error;
use tokio::{
    net::UdpSocket,
    sync::Mutex,
    task::{JoinSet, spawn_local},
};
use webrtc_dtls::{
    config::{Config, ExtendedMasterSecretType},
    conn::DTLSConn,
    crypto::Certificate,
};
use webrtc_util::Conn;

#[derive(Debug, Error)]
pub(crate) enum RustInputConnectionError {
    #[error(transparent)]
    Bind(#[from] io::Error),
    #[error(transparent)]
    Dtls(#[from] webrtc_dtls::Error),
    #[error(transparent)]
    Webrtc(#[from] webrtc_util::Error),
    #[error("not connected")]
    NotConnected,
    #[error("emulation is disabled on the target device")]
    TargetEmulationDisabled,
    #[error("Connection timed out")]
    Timeout,
    #[error("its certificate is not authorized on this device")]
    NotAuthorized,
}

const DEFAULT_CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);

async fn connect(
    addr: SocketAddr,
    cert: Certificate,
) -> Result<(Arc<dyn Conn + Sync + Send>, SocketAddr), (SocketAddr, RustInputConnectionError)> {
    log::info!("connecting to {addr} ...");
    let conn = Arc::new(
        UdpSocket::bind("0.0.0.0:0")
            .await
            .map_err(|e| (addr, e.into()))?,
    );
    conn.connect(addr).await.map_err(|e| (addr, e.into()))?;
    let config = Config {
        certificates: vec![cert],
        server_name: "ignored".to_owned(),
        insecure_skip_verify: true,
        extended_master_secret: ExtendedMasterSecretType::Require,
        ..Default::default()
    };
    let timeout = tokio::time::sleep(DEFAULT_CONNECTION_TIMEOUT);
    tokio::select! {
        _ = timeout => Err((addr, RustInputConnectionError::Timeout)),
        result = DTLSConn::new(conn, config, true, None) => match result {
            Ok(dtls_conn) => Ok((Arc::new(dtls_conn), addr)),
            Err(e) => Err((addr, e.into())),
        }
    }
}

async fn connect_any(
    addrs: &[SocketAddr],
    cert: Certificate,
) -> Result<(Arc<dyn Conn + Send + Sync>, SocketAddr), RustInputConnectionError> {
    let mut joinset = JoinSet::new();
    for &addr in addrs {
        joinset.spawn_local(connect(addr, cert.clone()));
    }
    loop {
        match joinset.join_next().await {
            None => return Err(RustInputConnectionError::NotConnected),
            Some(r) => match r.expect("join error") {
                Ok(conn) => return Ok(conn),
                Err((a, e)) => {
                    log::warn!("failed to connect to {a}: `{e}`")
                }
            },
        };
    }
}

type Conns = Rc<Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>>;

/// what came from the connections to the clients
pub(crate) enum Received {
    /// an event of the client
    Event(ClientHandle, ProtoEvent),
    /// The client answered with a certificate (its fingerprint) that is not
    /// authorized here: no input goes to it.
    NotAuthorized(ClientHandle, String),
}

/// sends raw datagrams over the connections to the clients
#[derive(Clone)]
pub(crate) struct OutgoingLinks {
    client_manager: ClientManager,
    conns: Conns,
}

impl OutgoingLinks {
    /// returns whether the datagram was handed to a connection
    pub(crate) async fn send(&self, handle: ClientHandle, datagram: &[u8]) -> bool {
        let Some(addr) = self.client_manager.active_addr(handle) else {
            return false;
        };
        let conn = self.conns.lock().await.get(&addr).cloned();
        match conn {
            Some(conn) => conn.send(datagram).await.is_ok(),
            None => false,
        }
    }
}

/// state shared by the tasks serving the connections to the clients
#[derive(Clone)]
struct Shared {
    client_manager: ClientManager,
    conns: Conns,
    tx: Sender<(ClientHandle, ProtoEvent)>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
    /// clients that answered with a certificate not authorized here
    refused: Sender<(ClientHandle, String)>,
}

/// what a connection needs to take part in clipboard synchronization
#[derive(Clone)]
struct ClipboardContext {
    clipboard: ClipboardHandle,
    /// fingerprints authorized on this machine
    authorized_keys: Arc<RwLock<HashMap<String, String>>>,
}

pub(crate) struct RustInputConnection {
    cert: Certificate,
    client_manager: ClientManager,
    conns: Conns,
    connecting: Rc<Mutex<HashSet<ClientHandle>>>,
    recv_rx: Receiver<(ClientHandle, ProtoEvent)>,
    recv_tx: Sender<(ClientHandle, ProtoEvent)>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
    clipboard: ClipboardContext,
    netstats: NetStats,
    refused_rx: Receiver<(ClientHandle, String)>,
    refused_tx: Sender<(ClientHandle, String)>,
}

impl RustInputConnection {
    pub(crate) fn new(
        cert: Certificate,
        client_manager: ClientManager,
        authorized_keys: Arc<RwLock<HashMap<String, String>>>,
        clipboard: ClipboardHandle,
        netstats: NetStats,
    ) -> Self {
        let (recv_tx, recv_rx) = channel();
        let (refused_tx, refused_rx) = channel();
        Self {
            cert,
            client_manager,
            conns: Default::default(),
            connecting: Default::default(),
            recv_rx,
            recv_tx,
            ping_response: Default::default(),
            clipboard: ClipboardContext {
                clipboard,
                authorized_keys,
            },
            netstats,
            refused_rx,
            refused_tx,
        }
    }

    /// the optional features the client announced
    pub(crate) fn peer_capabilities(&self, handle: ClientHandle) -> u32 {
        self.client_manager.peer_capabilities(handle)
    }

    pub(crate) fn links(&self) -> OutgoingLinks {
        OutgoingLinks {
            client_manager: self.client_manager.clone(),
            conns: self.conns.clone(),
        }
    }

    pub(crate) async fn recv(&mut self) -> Received {
        tokio::select! {
            refused = self.refused_rx.recv() => {
                let (handle, fingerprint) = refused.expect("channel closed");
                Received::NotAuthorized(handle, fingerprint)
            }
            event = self.recv_rx.recv() => {
                let (handle, event) = event.expect("channel closed");
                Received::Event(handle, event)
            }
        }
    }

    /// The client is connected and answers, but takes no input: it is
    /// paused there or has no input emulation. Before the first connection
    /// (which the first crossing opens) it is not known, so not refused.
    pub(crate) fn refuses(&self, handle: ClientHandle) -> bool {
        self.client_manager.active_addr(handle).is_some() && !self.client_manager.alive(handle)
    }

    pub(crate) async fn send(
        &self,
        event: ProtoEvent,
        handle: ClientHandle,
    ) -> Result<(), RustInputConnectionError> {
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
        let buf = &buf[..len];
        if let Some(addr) = self.client_manager.active_addr(handle) {
            let conn = {
                let conns = self.conns.lock().await;
                conns.get(&addr).cloned()
            };
            if let Some(conn) = conn {
                if !self.client_manager.alive(handle) {
                    return Err(RustInputConnectionError::TargetEmulationDisabled);
                }
                match conn.send(buf).await {
                    Ok(_) => {}
                    Err(e) => {
                        log::warn!("client {handle} failed to send: {e}");
                        disconnect(&self.client_manager, handle, addr, &self.conns).await;
                    }
                }
                log::trace!("{event} >->->->->- {addr}");
                return Ok(());
            }
        }

        // check if we are already trying to connect
        let mut connecting = self.connecting.lock().await;
        if !connecting.contains(&handle) {
            connecting.insert(handle);
            // connect in the background
            let shared = Shared {
                client_manager: self.client_manager.clone(),
                conns: self.conns.clone(),
                tx: self.recv_tx.clone(),
                ping_response: self.ping_response.clone(),
                refused: self.refused_tx.clone(),
            };
            spawn_local(connect_to_handle(
                shared,
                self.cert.clone(),
                handle,
                self.connecting.clone(),
                self.clipboard.clone(),
                self.netstats.clone(),
            ));
        }
        Err(RustInputConnectionError::NotConnected)
    }
}

async fn connect_to_handle(
    shared: Shared,
    cert: Certificate,
    handle: ClientHandle,
    connecting: Rc<Mutex<HashSet<ClientHandle>>>,
    clipboard: ClipboardContext,
    netstats: NetStats,
) -> Result<(), RustInputConnectionError> {
    let Shared {
        client_manager,
        conns,
        ping_response,
        refused,
        ..
    } = shared.clone();
    log::info!("client {handle} connecting ...");
    // sending did not work, figure out active conn.
    if let Some(addrs) = client_manager.get_ips(handle) {
        let port = client_manager.get_port(handle).unwrap_or(DEFAULT_PORT);
        let addrs = addrs
            .into_iter()
            .map(|a| SocketAddr::new(a, port))
            .collect::<Vec<_>>();
        log::info!("client ({handle}) connecting ... (ips: {addrs:?})");
        let res = connect_any(&addrs, cert).await;
        let (conn, addr) = match res {
            Ok(c) => c,
            Err(e) => {
                connecting.lock().await.remove(&handle);
                return Err(e);
            }
        };
        // The input goes only to the device whose certificate is authorized
        // here: whoever answers at the address of the client (a computer
        // that took the address of one that is off, or one in the middle)
        // would get every key typed for it. Lan Mouse did not check this.
        let fingerprint = peer_fingerprint(&conn).await;
        let authorized = fingerprint.as_ref().is_some_and(|fp| {
            clipboard
                .authorized_keys
                .read()
                .expect("lock")
                .contains_key(fp)
        });
        if !authorized {
            log::warn!(
                "client {handle} @ {addr} answered with the certificate {}, not authorized on \
                 this device: nothing is sent to it",
                fingerprint.as_deref().unwrap_or("(none)")
            );
            let _ = conn.close().await;
            connecting.lock().await.remove(&handle);
            if let Some(fingerprint) = fingerprint {
                let _ = refused.send((handle, fingerprint));
            }
            return Err(RustInputConnectionError::NotAuthorized);
        }
        log::info!("client ({handle}) connected @ {addr}");
        let conn = netstats.meter(addr, conn);
        client_manager.set_active_addr(handle, Some(addr));
        conns.lock().await.insert(addr, conn.clone());
        connecting.lock().await.remove(&handle);

        // Find out whether the peer knows the newer events without
        // sending one: Lan Mouse up to 0.11.0 disconnects on event types
        // it does not know, but ignores this acknowledgement. A newer
        // listener answers it with its Hello and Capabilities, to which
        // [`receive_loop`] replies with ours.
        let (buf, len) = ProtoEvent::Ack(HANDSHAKE_PROBE).into();
        if let Err(e) = conn.send(&buf[..len]).await {
            log::debug!("handshake probe to {addr} failed: {e}");
        }

        // poll connection for active
        spawn_local(ping_pong(addr, conn.clone(), ping_response.clone()));

        // receiver
        spawn_local(receive_loop(
            shared,
            handle,
            addr,
            conn,
            // the clipboard too: the certificate of the peer is authorized
            // here (checked above)
            fingerprint.map(|fingerprint| (clipboard.clipboard, fingerprint)),
        ));
        return Ok(());
    }
    connecting.lock().await.remove(&handle);
    Err(RustInputConnectionError::NotConnected)
}

async fn ping_pong(
    addr: SocketAddr,
    conn: Arc<dyn Conn + Send + Sync>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
) {
    loop {
        let (buf, len) = ProtoEvent::Ping.into();

        // send 4 pings, at least one must be answered
        for _ in 0..4 {
            if let Err(e) = conn.send(&buf[..len]).await {
                log::warn!("{addr}: send error `{e}`, closing connection");
                let _ = conn.close().await;
                break;
            }
            log::trace!("PING >->->->->- {addr}");

            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        if !ping_response.borrow_mut().remove(&addr) {
            log::warn!("{addr} did not respond, closing connection");
            let _ = conn.close().await;
            return;
        }
    }
}

/// whether the certificate presented by the peer is authorized on this device
/// fingerprint of the certificate the peer presented
pub(crate) async fn peer_fingerprint(conn: &Arc<dyn Conn + Send + Sync>) -> Option<String> {
    let conn = conn.as_any().downcast_ref::<DTLSConn>()?;
    let certs = conn.connection_state().await.peer_certificates;
    certs.first().map(|cert| crypto::generate_fingerprint(cert))
}

/// tells the peer our build and optional features
async fn introduce(conn: &Arc<dyn Conn + Send + Sync>, addr: SocketAddr, flags: u32) {
    let hello = ProtoEvent::Hello {
        commit: local_commit(),
    };
    for event in [hello, ProtoEvent::Capabilities(flags)] {
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
        if let Err(e) = conn.send(&buf[..len]).await {
            log::debug!("sending {event} to {addr} failed: {e}");
        }
    }
}

async fn receive_loop(
    shared: Shared,
    handle: ClientHandle,
    addr: SocketAddr,
    conn: Arc<dyn Conn + Send + Sync>,
    // set if the clipboard may be shared with this peer, with its fingerprint
    clipboard: Option<(ClipboardHandle, String)>,
) {
    let Shared {
        client_manager,
        conns,
        tx,
        ping_response,
        ..
    } = shared;
    let peer = Peer::Outgoing(handle);
    let flags = CAP_CURSOR_POSITION
        | CAP_EDGE_SPAN
        | CAP_PAUSE
        | clipboard.as_ref().map_or(0, |(c, _)| c.capabilities());
    // whether the peer was told our build and features
    let mut introduced = false;
    let mut buf = [0u8; MAX_DATAGRAM_SIZE];
    while let Ok(len) = conn.recv(&mut buf).await {
        match Datagram::decode(&buf[..len]) {
            Ok(Datagram::Event(event)) => {
                log::trace!("{addr} <==<==<== {event}");
                if matches!(
                    event,
                    ProtoEvent::Hello { .. } | ProtoEvent::Capabilities(_)
                ) && !introduced
                {
                    // the peer answered the probe: it knows these events
                    introduced = true;
                    introduce(&conn, addr, flags).await;
                }
                match event {
                    ProtoEvent::Pong(b) => {
                        client_manager.set_active_addr(handle, Some(addr));
                        client_manager.set_alive(handle, b);
                        // a paused peer follows with `Paused(true)`
                        if b {
                            client_manager.set_paused(handle, false);
                        }
                        ping_response.borrow_mut().insert(addr);
                    }
                    ProtoEvent::Paused(paused) => client_manager.set_paused(handle, paused),
                    ProtoEvent::Hello { commit } => {
                        client_manager.set_peer_commit(handle, Some(commit));
                    }
                    ProtoEvent::Capabilities(flags) => {
                        client_manager.set_peer_capabilities(handle, flags);
                        if let Some((clipboard, fingerprint)) = &clipboard {
                            clipboard.notify(ClipboardMsg::Capabilities {
                                peer,
                                flags,
                                fingerprint: fingerprint.clone(),
                                // the file channel listens on the port of the input channel
                                connect_to: Some(addr),
                            });
                        }
                    }
                    event => tx.send((handle, event)).expect("channel closed"),
                }
            }
            Ok(Datagram::Clipboard(frame)) => {
                if let Some((clipboard, _)) = &clipboard {
                    clipboard.notify(ClipboardMsg::Frame { peer, frame });
                }
            }
            // Skip undecodable datagrams without dropping the
            // connection. Each DTLS recv is one framed message, so
            // skipping is safe and keeps us forward-compatible with
            // peers that send event types we don't yet know about.
            Err(e) => log::debug!("ignoring undecodable event from {addr}: {e}"),
        }
    }
    log::warn!("recv error");
    if let Some((clipboard, _)) = &clipboard {
        clipboard.notify(ClipboardMsg::Disconnected { peer });
    }
    disconnect(&client_manager, handle, addr, &conns).await;
}

async fn disconnect(
    client_manager: &ClientManager,
    handle: ClientHandle,
    addr: SocketAddr,
    conns: &Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>,
) {
    log::warn!("client ({handle}) @ {addr} connection closed");
    conns.lock().await.remove(&addr);
    client_manager.set_active_addr(handle, None);
    client_manager.set_peer_commit(handle, None);
    client_manager.set_peer_capabilities(handle, 0);
    client_manager.set_paused(handle, false);
    let active: Vec<SocketAddr> = conns.lock().await.keys().copied().collect();
    log::info!("active connections: {active:?}");
}
