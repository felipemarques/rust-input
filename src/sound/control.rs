//! What the service keeps about the shared sound (0.52.0): whether this
//! computer listens, the streams it handed out to the computers of its
//! list, and the computer it sends its own sound to. The service does the
//! asking over the network; this decides.

use std::{
    collections::HashMap,
    net::SocketAddr,
    time::{Duration, Instant},
};

use rustinput_ipc::{ClientHandle, SoundSource, SoundState};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use super::{
    Listener, OFFER_LIFETIME, OFFER_PERIOD, Sender, from_hex, new_stream, to_hex, wire::StreamId,
};

/// after sending failed by itself, offers are refused this long
const REFUSE_AFTER_FAILURE: Duration = Duration::from_secs(60);

/// the stream handed out to one computer of the list
struct Offer {
    stream: StreamId,
    key: [u8; 32],
    name: String,
    fingerprint: Option<String>,
    answer: Option<Result<(), String>>,
}

/// what this computer sends its sound to
struct Sending {
    sender: Sender,
    /// the fingerprint and the name of the listening computer
    listener: String,
    name: String,
    until: Instant,
}

/// a stream to hand out: to whom, and what
pub(crate) struct OfferRequest {
    pub(crate) handle: ClientHandle,
    pub(crate) fingerprint: Option<String>,
    pub(crate) port: u16,
    pub(crate) stream: String,
    pub(crate) key: String,
}

pub(crate) struct SoundControl {
    listen: bool,
    send: bool,
    keep_speakers: bool,
    port: u16,
    listener: Option<Listener>,
    offers: HashMap<ClientHandle, Offer>,
    next_offers: Instant,
    sending: Option<Sending>,
    error: Option<String>,
    refuse: Option<(Instant, String)>,
    failures_tx: UnboundedSender<String>,
    /// why recording or playing stopped by itself, from their threads
    pub(crate) failures: UnboundedReceiver<String>,
}

impl SoundControl {
    pub(crate) fn new(listen: bool, send: bool, keep_speakers: bool, port: u16) -> Self {
        let (failures_tx, failures) = unbounded_channel();
        let mut control = Self {
            listen: false,
            send,
            keep_speakers,
            port,
            listener: None,
            offers: HashMap::new(),
            next_offers: Instant::now(),
            sending: None,
            error: None,
            refuse: None,
            failures_tx,
            failures,
        };
        control.set_listen(listen);
        control
    }

    /// whether the service has something to do every second
    pub(crate) fn active(&self) -> bool {
        self.listener.is_some() || self.sending.is_some()
    }

    /// starts or stops listening; when it stops, the computers to tell
    /// (their fingerprints)
    pub(crate) fn set_listen(&mut self, on: bool) -> Vec<(ClientHandle, String)> {
        self.listen = on;
        if !super::SUPPORTED {
            return Vec::new();
        }
        if on && self.listener.is_none() {
            // a computer that listens does not send its own sound
            self.sending = None;
            let failures = self.failures_tx.clone();
            match Listener::start(self.port, move |e| {
                let _ = failures.send(e);
            }) {
                Ok(listener) => {
                    self.listener = Some(listener);
                    self.error = None;
                    self.next_offers = Instant::now();
                }
                Err(e) => {
                    log::warn!("sound: {e}");
                    self.error = Some(e);
                }
            }
            return Vec::new();
        }
        if !on {
            self.listener = None;
            let told = self
                .offers
                .drain()
                .filter_map(|(handle, offer)| offer.fingerprint.map(|f| (handle, f)))
                .collect();
            if self.error.is_some() && self.sending.is_none() {
                self.error = None;
            }
            return told;
        }
        Vec::new()
    }

    pub(crate) fn set_send(&mut self, on: bool) {
        self.send = on;
        if !on {
            self.sending = None;
        }
    }

    pub(crate) fn set_keep_speakers(&mut self, on: bool) {
        if self.keep_speakers != on {
            self.keep_speakers = on;
            // started again with or without the speakers at the next offer
            self.sending = None;
        }
    }

    /// The streams to hand out now, to the computers of the list switched
    /// on (`clients`: handle, name, fingerprint if known); every
    /// [`OFFER_PERIOD`] while listening.
    pub(crate) fn offers_due(
        &mut self,
        clients: &[(ClientHandle, String, Option<String>)],
    ) -> Vec<OfferRequest> {
        let Some(listener) = &self.listener else {
            return Vec::new();
        };
        let now = Instant::now();
        if now < self.next_offers {
            return Vec::new();
        }
        self.next_offers = now + OFFER_PERIOD;
        // computers no longer in the list or switched off
        let gone: Vec<_> = self
            .offers
            .keys()
            .filter(|h| !clients.iter().any(|c| c.0 == **h))
            .copied()
            .collect();
        for handle in gone {
            if let Some(offer) = self.offers.remove(&handle) {
                listener.remove(&offer.stream);
            }
        }
        let port = listener.port();
        clients
            .iter()
            .map(|(handle, name, fingerprint)| {
                let offer = self.offers.entry(*handle).or_insert_with(|| {
                    let (stream, key) = new_stream();
                    Offer {
                        stream,
                        key,
                        name: name.clone(),
                        fingerprint: None,
                        answer: None,
                    }
                });
                offer.name = name.clone();
                if offer.fingerprint.is_none() {
                    offer.fingerprint = fingerprint.clone();
                }
                OfferRequest {
                    handle: *handle,
                    fingerprint: offer.fingerprint.clone(),
                    port,
                    stream: to_hex(&offer.stream),
                    key: to_hex(&offer.key),
                }
            })
            .collect()
    }

    /// The answer of a computer to its stream. Once it is known whose
    /// certificate it has, its sound is received at `volume`.
    pub(crate) fn answered(
        &mut self,
        handle: ClientHandle,
        fingerprint: Option<String>,
        answer: Result<(), String>,
        volume: (u8, bool),
    ) {
        let Some(offer) = self.offers.get_mut(&handle) else {
            return;
        };
        if fingerprint.is_some() {
            offer.fingerprint = fingerprint;
        }
        if let Some(listener) = &self.listener {
            if answer.is_ok() {
                listener.add(offer.stream, &offer.key, volume.0, volume.1);
            }
        }
        offer.answer = Some(answer);
    }

    /// the fingerprint of a client whose stream was handed out
    pub(crate) fn fingerprint(&self, handle: ClientHandle) -> Option<String> {
        self.offers.get(&handle).and_then(|o| o.fingerprint.clone())
    }

    pub(crate) fn set_volume(&self, handle: ClientHandle, volume: u8, muted: bool) {
        if let (Some(listener), Some(offer)) = (&self.listener, self.offers.get(&handle)) {
            listener.set_volume(&offer.stream, volume, muted);
        }
    }

    /// The computer with the certificate `from`, called `name`, at `addr`,
    /// listens and asks for the sound of this one.
    pub(crate) fn incoming(
        &mut self,
        from: &str,
        name: &str,
        addr: SocketAddr,
        port: u16,
        stream: &str,
        key: &str,
    ) -> Result<(), String> {
        if !super::SUPPORTED {
            return Err("this system does not share its sound".into());
        }
        if !self.send {
            return Err("sending its sound is off there".into());
        }
        if self.listen {
            return Err("it listens itself".into());
        }
        if let Some((until, why)) = &self.refuse {
            if Instant::now() < *until {
                return Err(why.clone());
            }
            self.refuse = None;
        }
        let (Some(stream), Some(key)) = (from_hex::<8>(stream), from_hex::<32>(key)) else {
            return Err("a stream that is not one".into());
        };
        let to = SocketAddr::new(addr.ip(), port);
        let until = Instant::now() + OFFER_LIFETIME;
        if let Some(sending) = &mut self.sending {
            if sending.listener != from && Instant::now() < sending.until {
                return Err(format!("it sends its sound to {}", sending.name));
            }
            if sending.listener == from {
                sending.sender.retarget(to, stream, &key);
                sending.until = until;
                return Ok(());
            }
        }
        let failures = self.failures_tx.clone();
        self.sending = Some(Sending {
            sender: Sender::start(to, stream, &key, self.keep_speakers, move |e| {
                let _ = failures.send(e);
            }),
            listener: from.to_owned(),
            name: name.to_owned(),
            until,
        });
        self.error = None;
        Ok(())
    }

    /// the computer with the certificate `from` no longer listens
    pub(crate) fn stopped(&mut self, from: &str) {
        if self.sending.as_ref().is_some_and(|s| s.listener == from) {
            self.sending = None;
        }
    }

    /// recording or playing stopped by itself
    pub(crate) fn failed(&mut self, why: String) {
        if self.sending.take().is_some() {
            self.refuse = Some((Instant::now() + REFUSE_AFTER_FAILURE, why.clone()));
        }
        self.error = Some(why);
    }

    /// every second: stops sending when the listening computer went quiet;
    /// whether something changed
    pub(crate) fn tick(&mut self) -> bool {
        if self
            .sending
            .as_ref()
            .is_some_and(|s| Instant::now() >= s.until)
        {
            log::info!("sound: the listening computer went quiet");
            self.sending = None;
            return true;
        }
        false
    }

    /// what the window shows; `volume` gives the volume of a fingerprint
    pub(crate) fn state(&self, volume: impl Fn(&str) -> (u8, bool)) -> SoundState {
        let mut sources: Vec<SoundSource> = self
            .offers
            .iter()
            .map(|(handle, offer)| {
                let (volume, muted) = offer.fingerprint.as_deref().map_or((100, false), &volume);
                SoundSource {
                    handle: *handle,
                    name: offer.name.clone(),
                    volume,
                    muted,
                    answer: offer.answer.clone(),
                    heard: self
                        .listener
                        .as_ref()
                        .is_some_and(|l| l.heard(&offer.stream)),
                }
            })
            .collect();
        sources.sort_by_key(|s| s.handle);
        SoundState {
            supported: super::SUPPORTED,
            listen: self.listen,
            send: self.send,
            keep_speakers: self.keep_speakers,
            sources,
            sending_to: self.sending.as_ref().map(|s| s.name.clone()),
            error: self.error.clone(),
        }
    }
}
