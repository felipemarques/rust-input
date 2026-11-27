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
//!
//! The history of each clipboard (0.35.0, [`crate::clipboard`]) comes with
//! the same questions, unless the version the window has is still the one
//! there; "Copy here" on it brings an older copy, from this computer back to
//! its clipboard too.

use std::time::Duration;

use rustinput_ipc::{ClientHandle, ClipboardHistory, ClipboardSeen, FrontendEvent};
use tokio::{sync::oneshot, time::timeout};

use super::Service;
use crate::{
    arrangement::{self, ArrangeError, ArrangementEvent, FetchedHere},
    clipboard::{ClipboardMsg, HistoryAnswer, Peeked},
};

/// longest the clipboard of this computer may take to be read
const PEEK_HERE_TIMEOUT: Duration = Duration::from_secs(5);

/// how asking about a clipboard went
pub(super) enum ClipboardResult {
    /// what the clipboard of the client (of this computer: `None`) holds,
    /// `None` when it did not change, and its history, `None` from a
    /// RustInput without it; with the fingerprint that answered
    Peeked {
        handle: Option<ClientHandle>,
        name: String,
        result: Result<Peeked3, ArrangeError>,
    },
    /// what the clipboard of the client held came
    Brought {
        handle: ClientHandle,
        name: String,
        result: Result<(String, FetchedHere), ArrangeError>,
    },
    /// a copy of the history of this computer is back on its clipboard
    Restored {
        name: String,
        result: Result<(), String>,
    },
}

/// the fingerprint that answered, what the clipboard holds and its history
type Peeked3 = (Option<String>, Option<ClipboardSeen>, Option<HistoryAnswer>);

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
            let histories: Vec<_> = self
                .clipboard_histories
                .iter()
                .map(|(handle, (_, history))| (*handle, history.clone()))
                .collect();
            for (handle, history) in histories {
                self.notify_frontend(FrontendEvent::ClipboardHistory { handle, history });
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

    /// the version of the history of the clipboard the window has, 0 for none
    fn known_history(&self, handle: Option<ClientHandle>) -> u64 {
        self.clipboard_histories
            .get(&handle)
            .map_or(0, |(version, _)| *version)
    }

    fn peek_here(&mut self) {
        if !self.clipboard_peeking.insert(None) {
            return;
        }
        let known = self.known_hash(None);
        let (reply, peeked) = oneshot::channel();
        self.clipboard.notify(ClipboardMsg::Peek { known, reply });
        let version = self.known_history(None);
        let (reply, history) = oneshot::channel();
        self.clipboard.notify(ClipboardMsg::History {
            known: (version > 0).then_some(version),
            reply,
        });
        let (results, name) = (self.clipboard_results_tx.clone(), self.name.clone());
        tokio::task::spawn_local(async move {
            let result = match timeout(PEEK_HERE_TIMEOUT, peeked).await {
                Ok(Ok(Peeked::Unchanged)) => Ok((None, None, history.await.ok())),
                Ok(Ok(Peeked::Holds { hash, peek })) => {
                    let seen = Some(ClipboardSeen::Holds { hash, peek });
                    Ok((None, seen, history.await.ok()))
                }
                // nobody reads it: the clipboard is not shared here
                Ok(Err(_)) => Ok((None, Some(ClipboardSeen::Hidden), None)),
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
        let version = self.known_history(Some(handle));
        let (cert, results) = (self.cert.clone(), self.clipboard_results_tx.clone());
        tokio::task::spawn_local(async move {
            let result = arrangement::peek(cert, addrs, expected, known, version)
                .await
                .map(|(peer, seen, history)| (Some(peer), seen, history));
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

    /// Brings what the clipboard of the client holds, or held, here, if it
    /// still has `hash`; of this computer (`None`): from its history back
    /// on its clipboard.
    /// Brings here what the clipboard of the client holds now, whatever it
    /// is: it is asked first (AltGr + right Shift + C, 0.44.0).
    pub(super) fn bring_latest(&mut self, handle: ClientHandle) {
        let (expected, name) = self.clipboard_peer(handle);
        if !self.clipboard.enabled() {
            return;
        }
        let addrs = self.client_addrs(handle);
        log::info!("bringing the clipboard of {name} here, as asked");
        let (cert, results) = (self.cert.clone(), self.clipboard_results_tx.clone());
        tokio::task::spawn_local(async move {
            let asked = arrangement::peek(cert.clone(), addrs.clone(), expected.clone(), None, 0);
            let result = match asked.await {
                Ok((
                    _,
                    Some(ClipboardSeen::Holds {
                        peek: rustinput_ipc::ClipboardPeek::Empty,
                        ..
                    }),
                    _,
                )) => Err(ArrangeError::Failed("its clipboard is empty".into())),
                Ok((_, Some(ClipboardSeen::Holds { hash, .. }), _)) => {
                    arrangement::fetch(cert, addrs, expected, hash).await
                }
                Ok((_, Some(ClipboardSeen::Hidden), _)) => Err(ArrangeError::Refused(
                    "it does not show its clipboard".into(),
                )),
                Ok((_, Some(ClipboardSeen::Unavailable(e)), _)) => Err(ArrangeError::Failed(e)),
                Ok((_, None, _)) => Err(ArrangeError::Failed("no answer".into())),
                Err(e) => Err(e),
            };
            let _ = results.send(ClipboardResult::Brought {
                handle,
                name,
                result,
            });
        });
    }

    pub(super) fn bring_clipboard(&mut self, handle: Option<ClientHandle>, hash: u64) {
        let Some(handle) = handle else {
            let (reply, restored) = oneshot::channel();
            self.clipboard.notify(ClipboardMsg::Restore { hash, reply });
            let (results, name) = (self.clipboard_results_tx.clone(), self.name.clone());
            tokio::task::spawn_local(async move {
                let result = restored
                    .await
                    .unwrap_or_else(|_| Err("the clipboard is not shared here".into()));
                let _ = results.send(ClipboardResult::Restored { name, result });
            });
            return;
        };
        let (expected, name) = self.clipboard_peer(handle);
        if !self.clipboard.enabled() {
            self.notify_frontend(FrontendEvent::ClipboardBrought {
                handle: Some(handle),
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
            let _ = results.send(ClipboardResult::Brought {
                handle,
                name,
                result,
            });
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
                let (seen, history) = match result {
                    Ok((Some(peer), ..)) if !self.is_authorized(&peer) => (
                        Some(ClipboardSeen::Unavailable(
                            "its certificate is not authorized here".into(),
                        )),
                        None,
                    ),
                    Ok((peer, seen, history)) => {
                        if let (Some(handle), Some(peer)) = (handle, peer) {
                            self.clipboard_fingerprints.insert(handle, peer);
                        }
                        // a client that answered without it is older
                        let older = handle.is_some() && seen != Some(ClipboardSeen::Hidden);
                        let history = match history {
                            None if older => Some(HistoryAnswer::Older),
                            history => history,
                        };
                        (seen, history)
                    }
                    Err(e) => (Some(ClipboardSeen::Unavailable(e.to_string())), None),
                };
                if let Some(seen) = seen {
                    self.clipboard_seen_now(handle, name, seen);
                }
                if let Some(history) = history {
                    self.clipboard_history_now(handle, history);
                }
            }
            ClipboardResult::Restored { name, result } => {
                self.notify_frontend(FrontendEvent::ClipboardBrought {
                    handle: None,
                    name,
                    files: false,
                    error: result.err(),
                });
            }
            ClipboardResult::Brought {
                handle,
                name,
                result,
            } => {
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
                self.notify_frontend(FrontendEvent::ClipboardBrought {
                    handle: Some(handle),
                    name,
                    files,
                    error,
                });
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

    /// keeps the history of a clipboard and tells the window if it changed
    fn clipboard_history_now(&mut self, handle: Option<ClientHandle>, answer: HistoryAnswer) {
        let (version, history) = match answer {
            HistoryAnswer::Unchanged => return,
            HistoryAnswer::Items { version, items } => (version, ClipboardHistory::Items(items)),
            HistoryAnswer::Off => (0, ClipboardHistory::Off),
            HistoryAnswer::Unsupported => (0, ClipboardHistory::Unsupported),
            HistoryAnswer::Older => (0, ClipboardHistory::Older),
        };
        let changed = self
            .clipboard_histories
            .get(&handle)
            .is_none_or(|(_, known)| *known != history);
        self.clipboard_histories
            .insert(handle, (version, history.clone()));
        if changed {
            self.notify_frontend(FrontendEvent::ClipboardHistory { handle, history });
        }
    }

    /// forgets what the clipboard of a deleted client held
    pub(super) fn forget_clipboard(&mut self, handle: ClientHandle) {
        self.clipboard_seen.remove(&Some(handle));
        self.clipboard_histories.remove(&Some(handle));
        self.clipboard_fingerprints.remove(&handle);
    }

    /// whether this computer keeps a history of its clipboard, kept in the
    /// configuration
    pub(super) fn set_clipboard_history(&mut self, keep: bool) {
        self.config.set_clipboard_history(keep);
        self.save_config();
        let size = self.config.clipboard_history();
        log::info!("the history of the clipboard keeps {size} copies");
        self.clipboard.notify(ClipboardMsg::HistorySize(size));
        self.keep_clipboard_history = size > 0;
        self.notify_frontend(FrontendEvent::KeepClipboardHistory(size > 0));
    }

    /// forgets the history of the clipboard of this computer
    pub(super) fn clear_clipboard_history(&mut self) {
        self.clipboard.notify(ClipboardMsg::ClearHistory);
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
                history,
                mut done,
            } => {
                log::debug!("{addr} looks at the clipboard");
                if !self.show_clipboard {
                    let _ = done.send((Some(ClipboardSeen::Hidden), None));
                    return;
                }
                let (reply, peeked) = oneshot::channel();
                self.clipboard.notify(ClipboardMsg::Peek { known, reply });
                let history = history.map(|version| {
                    let (reply, history) = oneshot::channel();
                    self.clipboard.notify(ClipboardMsg::History {
                        known: (version > 0).then_some(version),
                        reply,
                    });
                    history
                });
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
                    let history = match history {
                        Some(history) => history.await.ok(),
                        None => None,
                    };
                    let _ = done.send((seen, history));
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
