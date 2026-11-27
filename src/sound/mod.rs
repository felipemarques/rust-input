//! The shared sound (0.52.0): every computer's sound in the headphones of
//! one, the one that listens, each at its own volume.
//!
//! The listening computer hands each computer of its list a stream id and
//! a key over the authenticated channel of the arrangement, every few
//! seconds while it listens ([`crate::arrangement::sound`]); a computer that
//! sends its sound (`sound_send`, on by default) records what it plays,
//! 5 ms at a time, and sends it sealed ([`wire`]) by UDP to the port of the
//! listening one (`sound_port`, 4243), which holds each stream a little
//! ([`jitter`]), mixes them and plays them. Unless `sound_keep_speakers`,
//! the speakers of a sending computer are silent meanwhile. A sending
//! computer that hears nothing from the listening one for
//! [`OFFER_LIFETIME`] stops.
//!
//! The sound is recorded and played with the threads of [`Listener`] and
//! [`Sender`], away from the loop of the service.

// on systems without a backend (macOS) the threads only wait, and what
// records, mixes and plays is not used
#![cfg_attr(
    not(any(target_os = "linux", windows)),
    allow(dead_code, unused_imports)
)]

pub(crate) mod control;
pub(crate) mod jitter;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod win;
pub(crate) mod wire;

use std::{
    collections::HashMap,
    net::{Ipv6Addr, SocketAddr, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

#[cfg(target_os = "linux")]
use linux as backend;
#[cfg(windows)]
use win as backend;

use jitter::Jitter;
use wire::{PCM_BYTES, Seal, StreamId};

/// the port the listening computer receives the sound on
pub(crate) const DEFAULT_PORT: u16 = 4243;
/// how often the listening computer hands out its streams again
pub(crate) const OFFER_PERIOD: Duration = Duration::from_secs(5);
/// a sending computer stops when it heard nothing for this long
pub(crate) const OFFER_LIFETIME: Duration = Duration::from_secs(16);

/// whether this system can share its sound
pub(crate) const SUPPORTED: bool = cfg!(any(target_os = "linux", windows));

/// puts back what an earlier RustInput left (the speakers it silenced)
pub(crate) fn clean_up() {
    #[cfg(any(target_os = "linux", windows))]
    if let Err(e) = backend::clean_up() {
        log::debug!("sound: nothing to put back ({e})");
    }
}

/// a stream id and a key, new
pub(crate) fn new_stream() -> (StreamId, [u8; 32]) {
    use ring::rand::SecureRandom;
    let rng = ring::rand::SystemRandom::new();
    let mut stream = [0u8; 8];
    let mut key = [0u8; 32];
    rng.fill(&mut stream).expect("random bytes");
    rng.fill(&mut key).expect("random bytes");
    (stream, key)
}

pub(crate) fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn from_hex<const N: usize>(text: &str) -> Option<[u8; N]> {
    let text = text.trim();
    if text.len() != 2 * N {
        return None;
    }
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

/// one sending computer, on the listening one
struct Source {
    seal: Seal,
    jitter: Jitter,
    volume: f32,
    muted: bool,
    heard: Option<Instant>,
}

/// The listening side: receives the streams handed out, mixes and plays
/// them. Stops when dropped.
pub(crate) struct Listener {
    port: u16,
    sources: Arc<Mutex<HashMap<StreamId, Source>>>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

/// a UDP socket for both IPv4 and IPv6 on `port`
fn bind(port: u16) -> Result<UdpSocket, String> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))
        .and_then(|s| {
            s.set_only_v6(false)?;
            s.bind(&SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)).into())?;
            Ok(s)
        })
        .map(UdpSocket::from)
        .or_else(|_| UdpSocket::bind(("0.0.0.0", port)))
        .map_err(|e| format!("UDP port {port}: {e}"))?;
    Ok(socket)
}

impl Listener {
    /// listens on UDP `port`; `failed` hears why playing does not work
    pub(crate) fn start(
        port: u16,
        failed: impl Fn(String) + Send + 'static,
    ) -> Result<Self, String> {
        let socket = bind(port)?;
        let port = socket.local_addr().map(|a| a.port()).unwrap_or(port);
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .map_err(|e| e.to_string())?;
        let sources: Arc<Mutex<HashMap<StreamId, Source>>> = Default::default();
        let stop = Arc::new(AtomicBool::new(false));
        let receive = {
            let (sources, stop) = (sources.clone(), stop.clone());
            std::thread::Builder::new()
                .name("sound-receive".into())
                .spawn(move || receive(socket, sources, stop))
                .map_err(|e| e.to_string())?
        };
        let play = {
            let (sources, stop) = (sources.clone(), stop.clone());
            std::thread::Builder::new()
                .name("sound-play".into())
                .spawn(move || play(sources, stop, failed))
                .map_err(|e| e.to_string())?
        };
        log::info!("sound: listening on UDP port {port}");
        Ok(Self {
            port,
            sources,
            stop,
            threads: vec![receive, play],
        })
    }

    /// the UDP port it listens on
    pub(crate) fn port(&self) -> u16 {
        self.port
    }

    /// receives the stream `stream`, sealed with `key`
    pub(crate) fn add(&self, stream: StreamId, key: &[u8; 32], volume: u8, muted: bool) {
        let mut sources = self.sources.lock().expect("lock");
        sources.entry(stream).or_insert_with(|| Source {
            seal: Seal::new(stream, key),
            jitter: Jitter::default(),
            volume: f32::from(volume.min(100)) / 100.0,
            muted,
            heard: None,
        });
    }

    pub(crate) fn remove(&self, stream: &StreamId) {
        self.sources.lock().expect("lock").remove(stream);
    }

    pub(crate) fn set_volume(&self, stream: &StreamId, volume: u8, muted: bool) {
        if let Some(source) = self.sources.lock().expect("lock").get_mut(stream) {
            source.volume = f32::from(volume.min(100)) / 100.0;
            source.muted = muted;
        }
    }

    /// whether sound of `stream` arrived in the last second
    pub(crate) fn heard(&self, stream: &StreamId) -> bool {
        self.sources
            .lock()
            .expect("lock")
            .get(stream)
            .and_then(|s| s.heard)
            .is_some_and(|at| at.elapsed() < Duration::from_secs(1))
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
        log::info!("sound: no longer listening");
    }
}

fn receive(
    socket: UdpSocket,
    sources: Arc<Mutex<HashMap<StreamId, Source>>>,
    stop: Arc<AtomicBool>,
) {
    let mut buf = [0u8; 2048];
    while !stop.load(Ordering::Relaxed) {
        let Ok((n, _)) = socket.recv_from(&mut buf) else {
            continue;
        };
        let datagram = &buf[..n];
        let Some(stream) = wire::stream_of(datagram) else {
            continue;
        };
        let mut sources = sources.lock().expect("lock");
        if let Some(source) = sources.get_mut(&stream) {
            if let Some((number, pcm)) = source.seal.open(datagram) {
                source.jitter.push(number, wire::samples(&pcm));
                source.heard = Some(Instant::now());
            }
        }
    }
}

fn play(
    sources: Arc<Mutex<HashMap<StreamId, Source>>>,
    stop: Arc<AtomicBool>,
    failed: impl Fn(String),
) {
    #[cfg(any(target_os = "linux", windows))]
    {
        let mut player: Option<backend::Player> = None;
        let mut idle = 0u32;
        let mut retry_at = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            let turn: Vec<_> = {
                let mut sources = sources.lock().expect("lock");
                sources
                    .values_mut()
                    .filter_map(|s| {
                        let volume = if s.muted { 0.0 } else { s.volume };
                        s.jitter.pop().map(|samples| (samples, volume))
                    })
                    .collect()
            };
            if turn.is_empty() {
                // nothing to play: the output is let go after a while
                idle += 1;
                if idle > 400 {
                    player = None;
                }
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            idle = 0;
            if player.is_none() && Instant::now() >= retry_at {
                match backend::Player::new() {
                    Ok(p) => player = Some(p),
                    Err(e) => {
                        failed(e);
                        retry_at = Instant::now() + Duration::from_secs(10);
                    }
                }
            }
            let pcm = wire::bytes(&jitter::mix(&turn));
            match player.as_mut() {
                // waits while the output holds enough: the pace of the turns
                Some(p) => {
                    if let Err(e) = p.write(&pcm) {
                        failed(e);
                        player = None;
                    }
                }
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = (sources, failed);
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// where a sending computer sends: the address and the seal of the stream
struct Target {
    to: SocketAddr,
    stream: StreamId,
    seal: Seal,
}

/// The sending side: records the sound of this computer and sends it to
/// the listening one. Stops when dropped, and the speakers play again.
pub(crate) struct Sender {
    target: Arc<Mutex<Target>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Sender {
    /// sends to `to` the stream `stream` sealed with `key`; with
    /// `keep_speakers` this computer plays its sound too. `failed` hears why
    /// it stopped by itself.
    pub(crate) fn start(
        to: SocketAddr,
        stream: StreamId,
        key: &[u8; 32],
        keep_speakers: bool,
        failed: impl Fn(String) + Send + 'static,
    ) -> Self {
        let target = Arc::new(Mutex::new(Target {
            to,
            stream,
            seal: Seal::new(stream, key),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (target, stop) = (target.clone(), stop.clone());
            std::thread::Builder::new()
                .name("sound-send".into())
                .spawn(move || {
                    if let Err(e) = send(target, stop, keep_speakers) {
                        log::warn!("sound: sending stopped: {e}");
                        failed(e);
                    }
                })
                .ok()
        };
        log::info!("sound: sending to {to}");
        Self {
            target,
            stop,
            thread,
        }
    }

    /// the listening computer handed out its stream again (perhaps a new
    /// one, or from another address)
    pub(crate) fn retarget(&self, to: SocketAddr, stream: StreamId, key: &[u8; 32]) {
        let mut target = self.target.lock().expect("lock");
        target.to = to;
        if target.stream != stream {
            target.stream = stream;
            target.seal = Seal::new(stream, key);
        }
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        log::info!("sound: no longer sending");
    }
}

fn send(
    target: Arc<Mutex<Target>>,
    stop: Arc<AtomicBool>,
    keep_speakers: bool,
) -> Result<(), String> {
    #[cfg(any(target_os = "linux", windows))]
    {
        let mut capture = backend::Capture::new(keep_speakers)?;
        let to = target.lock().expect("lock").to;
        let socket = UdpSocket::bind(if to.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" })
            .map_err(|e| e.to_string())?;
        let mut pcm = [0u8; PCM_BYTES];
        let mut number = 0u32;
        let mut warned = false;
        while !stop.load(Ordering::Relaxed) {
            capture.read(&mut pcm)?;
            let (to, datagram) = {
                let target = target.lock().expect("lock");
                (target.to, target.seal.seal(number, &pcm))
            };
            if let Err(e) = socket.send_to(&datagram, to) {
                if !warned {
                    log::warn!("sound: sending to {to}: {e}");
                    warned = true;
                }
            }
            // a key is for one stream, whose numbers never come back
            number = number
                .checked_add(1)
                .ok_or("the stream ran out of numbers")?;
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = (target, stop, keep_speakers);
        Err("this system does not share its sound".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_of_streams_and_keys() {
        let (stream, key) = new_stream();
        assert_eq!(from_hex::<8>(&to_hex(&stream)), Some(stream));
        assert_eq!(from_hex::<32>(&to_hex(&key)), Some(key));
        assert_eq!(from_hex::<8>("abc"), None);
        assert_eq!(from_hex::<8>("zz00000000000000"), None);
    }

    #[test]
    fn a_listener_receives_what_is_sealed_for_it() {
        let listener = Listener::start(0, |_| {}).expect("listening");
        let (stream, key) = new_stream();
        let (other, other_key) = new_stream();
        listener.add(stream, &key, 100, false);
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let to = ("127.0.0.1", listener.port());
        // one datagram only: the stream holds two before it plays, so this
        // test never plays anything on the computer running it
        socket
            .send_to(&Seal::new(other, &other_key).seal(0, &[0; PCM_BYTES]), to)
            .unwrap();
        socket
            .send_to(&Seal::new(stream, &other_key).seal(0, &[0; PCM_BYTES]), to)
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(!listener.heard(&stream), "not sealed with its key");
        socket
            .send_to(&Seal::new(stream, &key).seal(0, &[0; PCM_BYTES]), to)
            .unwrap();
        let mut heard = false;
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(50));
            heard |= listener.heard(&stream);
        }
        assert!(heard);
        assert!(!listener.heard(&other));
    }
}
