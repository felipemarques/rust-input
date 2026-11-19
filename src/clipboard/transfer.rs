//! Sending and receiving side of a single clipboard transfer.
//!
//! Both are plain state machines: they are fed frames and the current time and
//! return the frames to transmit, which keeps them independent of the network
//! and easy to test against a lossy link.

use std::time::{Duration, Instant};

use rustinput_proto::{ClipboardFrame, ClipboardKind, clipboard::CLIPBOARD_CHUNK_SIZE};

use super::Content;

/// largest content that is sent or accepted
pub(crate) const MAX_CONTENT_SIZE: usize = 64 * 1024 * 1024;

/// a transfer making no progress for this long is given up
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(5);

/// data sent ahead of the last acknowledgement
const WINDOW: usize = 64 * CLIPBOARD_CHUNK_SIZE;

/// the offer is repeated at this interval until the receiver answers
const OFFER_INTERVAL: Duration = Duration::from_millis(250);

/// unacknowledged data is sent again after this long without an answer
const RESEND_AFTER: Duration = Duration::from_millis(150);

/// the receiver acknowledges every this many chunks
const ACK_EVERY: u32 = 16;

/// shortest interval between acknowledgements of chunks that do not fit
const REPEATED_ACK_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Progress {
    Active,
    Done,
    Failed,
}

/// the sending side of a transfer
pub(crate) struct Outgoing {
    id: u32,
    content: Content,
    /// the receiver answered the offer
    accepted: bool,
    /// bytes the receiver confirmed
    acked: usize,
    /// next byte to transmit
    next: usize,
    last_offer: Option<Instant>,
    last_send: Instant,
    last_progress: Instant,
}

impl Outgoing {
    pub(crate) fn new(id: u32, content: Content, now: Instant) -> Self {
        Self {
            id,
            content,
            accepted: false,
            acked: 0,
            next: 0,
            last_offer: None,
            last_send: now,
            last_progress: now,
        }
    }

    pub(crate) fn id(&self) -> u32 {
        self.id
    }

    pub(crate) fn size(&self) -> usize {
        self.content.data.len()
    }

    /// the receiver holds the first `received` bytes
    pub(crate) fn on_ack(&mut self, received: u32, now: Instant) {
        let received = (received as usize).min(self.size());
        if !self.accepted || received > self.acked {
            self.last_progress = now;
        } else if received == self.acked && self.next > self.acked {
            // The receiver repeats itself: a chunk arrived that did not
            // continue what it has, so something in between got lost.
            // Resend from there right away instead of waiting for the
            // timeout; the receiver spaces these repetitions out.
            self.next = self.acked;
        }
        self.accepted = true;
        self.acked = self.acked.max(received);
        // nothing below the acknowledgement needs to be sent again
        self.next = self.next.max(self.acked);
    }

    /// the frames to transmit at `now`
    pub(crate) fn poll(&mut self, now: Instant) -> Vec<ClipboardFrame> {
        if self.progress(now) != Progress::Active {
            return vec![];
        }
        if !self.accepted {
            if self
                .last_offer
                .is_some_and(|t| now.duration_since(t) < OFFER_INTERVAL)
            {
                return vec![];
            }
            self.last_offer = Some(now);
            return vec![ClipboardFrame::Offer {
                transfer: self.id,
                kind: self.content.kind,
                size: self.size() as u32,
                hash: self.content.hash,
            }];
        }

        // the receiver went quiet with data outstanding: go back to the last
        // acknowledged byte
        if self.next > self.acked && now.duration_since(self.last_send) >= RESEND_AFTER {
            self.next = self.acked;
        }

        let limit = self.size().min(self.acked + WINDOW);
        let mut frames = Vec::new();
        while self.next < limit {
            let end = limit.min(self.next + CLIPBOARD_CHUNK_SIZE);
            frames.push(ClipboardFrame::Data {
                transfer: self.id,
                offset: self.next as u32,
                data: self.content.data[self.next..end].to_vec(),
            });
            self.next = end;
        }
        if !frames.is_empty() {
            self.last_send = now;
        }
        frames
    }

    pub(crate) fn progress(&self, now: Instant) -> Progress {
        if self.accepted && self.acked == self.size() {
            Progress::Done
        } else if now.duration_since(self.last_progress) > TRANSFER_TIMEOUT {
            Progress::Failed
        } else {
            Progress::Active
        }
    }
}

/// the receiving side of a transfer
pub(crate) struct Incoming {
    id: u32,
    kind: ClipboardKind,
    hash: u64,
    size: usize,
    data: Vec<u8>,
    chunks_since_ack: u32,
    last_repeated_ack: Option<Instant>,
    last_activity: Instant,
}

impl Incoming {
    pub(crate) fn new(id: u32, kind: ClipboardKind, size: usize, hash: u64, now: Instant) -> Self {
        Self {
            id,
            kind,
            hash,
            size,
            // the announced size is not trusted for the allocation
            data: Vec::with_capacity(size.min(1024 * 1024)),
            chunks_since_ack: 0,
            last_repeated_ack: None,
            last_activity: now,
        }
    }

    pub(crate) fn id(&self) -> u32 {
        self.id
    }

    pub(crate) fn size(&self) -> usize {
        self.size
    }

    /// the acknowledgement of everything received so far, which also answers
    /// the offer
    pub(crate) fn ack(&self) -> ClipboardFrame {
        ClipboardFrame::Ack {
            transfer: self.id,
            received: self.data.len() as u32,
        }
    }

    /// a chunk arrived, returns the acknowledgement to send (if any)
    pub(crate) fn on_data(
        &mut self,
        offset: u32,
        chunk: &[u8],
        now: Instant,
    ) -> Option<ClipboardFrame> {
        let in_order = offset as usize == self.data.len();
        if in_order && self.data.len() + chunk.len() <= self.size {
            self.data.extend_from_slice(chunk);
            self.last_activity = now;
            self.chunks_since_ack += 1;
            if self.is_complete() || self.chunks_since_ack >= ACK_EVERY {
                self.chunks_since_ack = 0;
                return Some(self.ack());
            }
            return None;
        }
        // a gap, a repetition or more than announced: tell the sender where
        // we are, but do not answer a whole burst of such chunks
        if self
            .last_repeated_ack
            .is_some_and(|t| now.duration_since(t) < REPEATED_ACK_INTERVAL)
        {
            return None;
        }
        self.last_repeated_ack = Some(now);
        Some(self.ack())
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.data.len() == self.size
    }

    pub(crate) fn is_stale(&self, now: Instant) -> bool {
        now.duration_since(self.last_activity) > TRANSFER_TIMEOUT
    }

    pub(crate) fn into_content(self) -> Content {
        Content {
            kind: self.kind,
            data: self.data,
            hash: self.hash,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content(size: usize) -> Content {
        Content {
            kind: ClipboardKind::Text,
            data: (0..size).map(|i| (i % 251) as u8).collect(),
            hash: 42,
        }
    }

    /// Runs a transfer over a link that drops the frames `drop` selects (by a
    /// running frame counter), returns what was received and the rounds taken.
    fn transfer(size: usize, mut drop: impl FnMut(u64) -> bool) -> (Option<Content>, u32) {
        let mut now = Instant::now();
        let mut sender = Outgoing::new(1, content(size), now);
        let mut receiver: Option<Incoming> = None;
        let mut counter = 0u64;
        let mut lost = |counter: &mut u64| {
            *counter += 1;
            drop(*counter)
        };
        for round in 0..5_000 {
            match sender.progress(now) {
                Progress::Done => {
                    return (receiver.map(Incoming::into_content), round);
                }
                Progress::Failed => return (None, round),
                Progress::Active => {}
            }
            let mut acks = Vec::new();
            for frame in sender.poll(now) {
                if lost(&mut counter) {
                    continue;
                }
                match frame {
                    ClipboardFrame::Offer {
                        transfer,
                        kind,
                        size,
                        hash,
                    } => {
                        let incoming = receiver.get_or_insert_with(|| {
                            Incoming::new(transfer, kind, size as usize, hash, now)
                        });
                        acks.push(incoming.ack());
                    }
                    ClipboardFrame::Data { offset, data, .. } => {
                        let incoming = receiver.as_mut().expect("data before the offer");
                        acks.extend(incoming.on_data(offset, &data, now));
                    }
                    frame => panic!("unexpected frame from the sender: {frame:?}"),
                }
            }
            for ack in acks {
                if lost(&mut counter) {
                    continue;
                }
                match ack {
                    ClipboardFrame::Ack { received, .. } => sender.on_ack(received, now),
                    frame => panic!("unexpected frame from the receiver: {frame:?}"),
                }
            }
            now += Duration::from_millis(10);
        }
        panic!("transfer did not finish");
    }

    #[test]
    fn transfers_over_a_perfect_link() {
        for size in [
            1,
            CLIPBOARD_CHUNK_SIZE - 1,
            CLIPBOARD_CHUNK_SIZE,
            CLIPBOARD_CHUNK_SIZE + 1,
            WINDOW,
            3 * WINDOW + 17,
        ] {
            let (received, _) = transfer(size, |_| false);
            assert_eq!(received, Some(content(size)), "size {size}");
        }
    }

    #[test]
    fn a_perfect_link_needs_no_resend_timeouts() {
        // one round for the offer, then one window per round
        let (received, rounds) = transfer(10 * WINDOW, |_| false);
        assert!(received.is_some());
        assert!(rounds <= 12, "took {rounds} rounds");
    }

    #[test]
    fn survives_lost_frames() {
        for loss_percent in [1, 10, 30, 50] {
            // a fixed pseudo random sequence keeps the test reproducible
            let mut state = 0x2545_f491_4f6c_dd1du64;
            let lossy = |_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state % 100 < loss_percent
            };
            let size = 2 * WINDOW + 333;
            let (received, _) = transfer(size, lossy);
            assert_eq!(received, Some(content(size)), "{loss_percent}% loss");
        }
    }

    #[test]
    fn a_repeated_ack_triggers_an_immediate_resend() {
        let now = Instant::now();
        let mut sender = Outgoing::new(1, content(4 * CLIPBOARD_CHUNK_SIZE), now);
        sender.poll(now);
        sender.on_ack(0, now);
        assert_eq!(sender.poll(now).len(), 4);
        // nothing new to send while waiting for acknowledgements
        assert!(sender.poll(now).is_empty());
        // the receiver got the first chunk and then one that did not fit
        sender.on_ack(CLIPBOARD_CHUNK_SIZE as u32, now);
        sender.on_ack(CLIPBOARD_CHUNK_SIZE as u32, now);
        let resent = sender.poll(now);
        assert_eq!(resent.len(), 3);
        assert!(matches!(
            resent[0],
            ClipboardFrame::Data { offset, .. } if offset as usize == CLIPBOARD_CHUNK_SIZE
        ));
    }

    #[test]
    fn survives_a_lost_offer_and_a_lost_last_chunk() {
        let size = CLIPBOARD_CHUNK_SIZE * 3;
        // frame 1 is the first offer; frames 2 and 3 are the repeated offer
        // and its answer, 4 to 6 the three chunks
        let (received, _) = transfer(size, |n| n == 1 || n == 6);
        assert_eq!(received, Some(content(size)));
    }

    #[test]
    fn gives_up_on_a_dead_link() {
        let (received, _) = transfer(1000, |_| true);
        assert_eq!(received, None);
    }

    #[test]
    fn receiver_ignores_chunks_that_do_not_fit() {
        let now = Instant::now();
        let mut incoming = Incoming::new(1, ClipboardKind::Text, 10, 0, now);
        // a gap is answered with the current position
        assert_eq!(
            incoming.on_data(4, &[1, 2], now),
            Some(ClipboardFrame::Ack {
                transfer: 1,
                received: 0
            })
        );
        // more of them right away are not answered again
        assert_eq!(incoming.on_data(6, &[1, 2], now), None);
        assert_eq!(incoming.on_data(0, &[1, 2, 3, 4], now), None);
        // more than announced
        let later = now + Duration::from_millis(100);
        assert_eq!(
            incoming.on_data(4, &[0; 7], later),
            Some(ClipboardFrame::Ack {
                transfer: 1,
                received: 4
            })
        );
        assert!(!incoming.is_complete());
        // the rest completes the transfer and is acknowledged at once
        assert_eq!(
            incoming.on_data(4, &[5, 6, 7, 8, 9, 10], later),
            Some(ClipboardFrame::Ack {
                transfer: 1,
                received: 10
            })
        );
        assert!(incoming.is_complete());
        assert_eq!(
            incoming.into_content().data,
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10]
        );
    }

    #[test]
    fn stale_incoming_transfers_expire() {
        let now = Instant::now();
        let incoming = Incoming::new(1, ClipboardKind::Text, 10, 0, now);
        assert!(!incoming.is_stale(now + Duration::from_secs(1)));
        assert!(incoming.is_stale(now + TRANSFER_TIMEOUT + Duration::from_millis(1)));
    }
}
