//! The Clipboard tab: what the clipboards of this computer and of the
//! clients hold, and bringing one of them here.
//!
//! The window asks every few seconds while the tab is shown
//! ([`FrontendRequest::PeekClipboards`](rustinput_ipc::FrontendRequest)).
//! The clients are asked over the TCP port ([`arrangement::peek`]) with the
//! hash of what they held the last time, so an unchanged clipboard costs a
//! short answer; the window hears only what changed. A client is believed
//! only with a certificate authorized here, the one it described itself with
//! or the first one it answered with.

use std::time::Duration;

use rustinput_ipc::{ClientHandle, ClipboardSeen, FrontendEvent};
use tokio::{sync::oneshot, time::timeout};

use super::Service;
use crate::{
    arrangement::{self, ArrangeError, ArrangementEvent, FetchedHere},
    clipboard::{ClipboardMsg, Peeked},
};

/// longest the clipboard of this computer may take to be read
const PEEK_HERE_TIMEOUT: Duration = Duration::from_secs(5);

/// how asking about a clipboard went
pub(super) enum ClipboardResult {
    /// what the clipboard of the client (of this computer: `None`) holds;
    /// `None` when it did not change, with the fingerprint that answered
    Peeked {
        handle: Option<ClientHandle>,
        name: String,
        result: Result<(Option<String>, Option<ClipboardSeen>), ArrangeError>,
    },
    /// what the clipboard of the client held came
    Brought {
        name: String,
        result: Result<(String, FetchedHere), ArrangeError>,
    },
}

impl Service {
    /// Asks what the clipboards hold: the window hears what changed, or
    /// first everything known (`all`).
    pub(super) fn peek_clipboards(&mut self, all: bool) {
        if all {
            let known: Vec<_> = self
                .clipboard_seen
                .iter()
                .map(|(handle, (name, seen))| (*handle, name.clone(), seen.clone()))
                .collect();
            for (handle, name, seen) in known {
                self.notify_frontend(FrontendEvent::Clipboard { handle, name, seen });
            }
        }
        self.peek_here();
        for handle in self.client_manager.registered_clients() {
            self.peek_client(handle);
        }
    }

    /// the hash of what the clipboard held the last time
    fn known_hash(&self, handle: Option<ClientHandle>) -> Option<u64> {
        match self.clipboard_seen.get(&handle) {
            Some((_, ClipboardSeen::Holds { hash, .. })) => Some(*hash),
            _ => None,
        }
    }

    fn peek_here(&mut self) {
        if !self.clipboard_peeking.insert(None) {
            return;
        }
        let known = self.known_hash(None);
        let (reply, peeked) = oneshot::channel();
        self.clipboard.notify(ClipboardMsg::Peek { known, reply });
        let (results, name) = (self.clipboard_results_tx.clone(), self.name.clone());
        tokio::task::spawn_local(async move {
            let result = match timeout(PEEK_HERE_TIMEOUT, peeked).await {
                Ok(Ok(Peeked::Unchanged)) => Ok((None, None)),
                Ok(Ok(Peeked::Holds { hash, peek })) => {
                    Ok((None, Some(ClipboardSeen::Holds { hash, peek })))
                }
                // nobody reads it: the clipboard is not shared here
                Ok(Err(_)) => Ok((None, Some(ClipboardSeen::Hidden))),
                Err(_) => Err(ArrangeError::Failed(
                    "the clipboard did not answer in time".into(),
                )),
            };
            let _ = results.send(ClipboardResult::Peeked {
                handle: None,
                name,
                result,
            });
        });
    }

    fn peek_client(&mut self, handle: ClientHandle) {
        if self.clipboard_peeking.contains(&Some(handle)) {
            return;
        }
        let addrs = self.client_addrs(handle);
        let (expected, name) = self.clipboard_peer(handle);
        if addrs.is_empty() {
            let seen = ClipboardSeen::Unavailable("its address is not known yet".into());
            self.clipboard_seen_now(Some(handle), name, seen);
            return;
        }
        self.clipboard_peeking.insert(Some(handle));
        let known = self.known_hash(Some(handle));
        let (cert, results) = (self.cert.clone(), self.clipboard_results_tx.clone());
        tokio::task::spawn_local(async move {
            let result = arrangement::peek(cert, addrs, expected, known)
                .await
                .map(|(peer, seen)| (Some(peer), seen));
            let _ = results.send(ClipboardResult::Peeked {
                handle: Some(handle),
                name,
                result,
            });
        });
    }

    /// the certificate a client has to answer with, when known, and its name
    fn clipboard_peer(&self, handle: ClientHandle) -> (Option<String>, String) {
        let (described, name) = self.peer(handle);
        let expected = described.or_else(|| self.clipboard_fingerprints.get(&handle).cloned());
        (expected, name)
    }

    fn is_authorized(&self, fingerprint: &str) -> bool {
        self.authorized_keys
            .read()
            .expect("lock")
            .contains_key(fingerprint)
    }

    /// Brings what the clipboard of the client holds here, if it still
    /// holds `hash`.
    pub(super) fn bring_clipboard(&mut self, handle: ClientHandle, hash: u64) {
        let (expected, name) = self.clipboard_peer(handle);
        if !self.clipboard.enabled() {
            self.notify_frontend(FrontendEvent::ClipboardBrought {
                name,
                files: false,
                error: Some(
                    "the clipboard is not shared on this computer (clipboard = false)".into(),
                ),
            });
            return;
        }
        let addrs = self.client_addrs(handle);
        log::info!("bringing the clipboard of {name} here");
        let (cert, results) = (self.cert.clone(), self.clipboard_results_tx.clone());
        tokio::task::spawn_local(async move {
            let result = arrangement::fetch(cert, addrs, expected, hash).await;
            let _ = results.send(ClipboardResult::Brought { name, result });
        });
    }

    pub(super) fn clipboard_result(&mut self, result: ClipboardResult) {
        match result {
            ClipboardResult::Peeked {
                handle,
                name,
                result,
            } => {
                self.clipboard_peeking.remove(&handle);
                // a client deleted meanwhile
                if let Some(h) = handle {
                    if !self.client_manager.registered_clients().contains(&h) {
                        return;
                    }
                }
                let seen = match result {
                    Ok((Some(peer), _)) if !self.is_authorized(&peer) => Some(
                        ClipboardSeen::Unavailable("its certificate is not authorized here".into()),
                    ),
                    Ok((peer, seen)) => {
                        if let (Some(handle), Some(peer)) = (handle, peer) {
                            self.clipboard_fingerprints.insert(handle, peer);
                        }
                        seen
                    }
                    Err(e) => Some(ClipboardSeen::Unavailable(e.to_string())),
                };
                if let Some(seen) = seen {
                    self.clipboard_seen_now(handle, name, seen);
                }
            }
            ClipboardResult::Brought { name, result } => {
                let (files, error) = match result {
                    Ok((peer, _)) if !self.is_authorized(&peer) => (
                        false,
                        Some("its certificate is not authorized here".to_owned()),
                    ),
                    Ok((peer, FetchedHere::Data(content))) => {
                        self.clipboard.notify(ClipboardMsg::Bring {
                            content,
                            fingerprint: peer,
                        });
                        (false, None)
                    }
                    Ok((_, FetchedHere::Files(count))) => {
                        log::info!("{count} item(s) of the clipboard of {name} on their way");
                        (true, None)
                    }
                    Err(e) => {
                        log::info!("could not bring the clipboard of {name} here: {e}");
                        (false, Some(e.to_string()))
                    }
                };
                self.notify_frontend(FrontendEvent::ClipboardBrought { name, files, error });
            }
        }
    }

    /// keeps what a clipboard holds and tells the window if it changed
    fn clipboard_seen_now(
        &mut self,
        handle: Option<ClientHandle>,
        name: String,
        seen: ClipboardSeen,
    ) {
        let changed = self
            .clipboard_seen
            .get(&handle)
            .is_none_or(|(known_name, known)| *known_name != name || *known != seen);
        if changed {
            self.clipboard_seen
                .insert(handle, (name.clone(), seen.clone()));
            self.notify_frontend(FrontendEvent::Clipboard { handle, name, seen });
        }
    }

    /// forgets what the clipboard of a deleted client held
    pub(super) fn forget_clipboard(&mut self, handle: ClientHandle) {
        self.clipboard_seen.remove(&Some(handle));
        self.clipboard_fingerprints.remove(&handle);
    }

    /// whether the other computers see the clipboard of this one, kept in
    /// the configuration
    pub(super) fn set_show_clipboard(&mut self, show: bool) {
        self.show_clipboard = show;
        self.config.set_show_clipboard(show);
        self.save_config();
        log::info!(
            "the other computers {} the clipboard of this one",
            if show { "see" } else { "do not see" }
        );
        self.notify_frontend(FrontendEvent::ShowClipboard(show));
    }

    /// answers an authorized device asking about the clipboard
    pub(super) fn clipboard_asked(&mut self, event: ArrangementEvent) {
        match event {
            ArrangementEvent::Peek {
                addr,
                known,
                mut done,
            } => {
                log::debug!("{addr} looks at the clipboard");
                if !self.show_clipboard {
                    let _ = done.send(Some(ClipboardSeen::Hidden));
                    return;
                }
                let (reply, peeked) = oneshot::channel();
                self.clipboard.notify(ClipboardMsg::Peek { known, reply });
                tokio::task::spawn_local(async move {
                    // the asker gave up: the clipboard is not read for it
                    let peeked = tokio::select! {
                        peeked = peeked => peeked,
                        () = done.closed() => return,
                    };
                    let seen = match peeked {
                        Ok(Peeked::Unchanged) => None,
                        Ok(Peeked::Holds { hash, peek }) => {
                            Some(ClipboardSeen::Holds { hash, peek })
                        }
                        // nobody reads it: the clipboard is not shared here
                        Err(_) => Some(ClipboardSeen::Hidden),
                    };
                    let _ = done.send(seen);
                });
            }
            ArrangementEvent::Fetch {
                addr,
                hash,
                from,
                done,
            } => {
                if !self.show_clipboard {
                    let _ = done.send(Err("this computer does not show its clipboard".into()));
                    return;
                }
                log::info!("{addr} asks for what the clipboard holds");
                self.clipboard.notify(ClipboardMsg::Fetch {
                    hash,
                    fingerprint: from,
                    reply: done,
                });
            }
            _ => {}
        }
    }
}
