//! Holding the datagrams of each sending computer a little before playing
//! them (0.52.0): the network delivers them unevenly, and late, lost or
//! doubled ones must not crackle.
//!
//! Each stream starts holding [`START`] datagrams (10 ms). A datagram that
//! was missing when its turn came plays as silence and makes the stream
//! hold one more, up to [`MOST`] (80 ms); after [`CALM`] turns without one
//! missing (10 s) it holds one less again. When the sending computer's
//! clock runs faster than this one's the stream fills up: past its target
//! by [`SLACK`] the oldest datagram is dropped. A stream that went quiet
//! for [`QUIET`] turns starts again from nothing.

use std::collections::VecDeque;

use super::wire::SAMPLES;

pub(crate) const START: usize = 2;
pub(crate) const MOST: usize = 16;
pub(crate) const CALM: u32 = 2000;
pub(crate) const SLACK: usize = 6;
pub(crate) const QUIET: u32 = 40;
/// datagrams ahead of the one to play that are kept at most
const WINDOW: u32 = 64;

/// the datagrams of one stream, by their numbers
#[derive(Debug)]
pub(crate) struct Jitter {
    /// the number of the datagram that plays next, once playing
    next: Option<u32>,
    /// the datagrams from `next` on (`None` for one not here)
    slots: VecDeque<Option<Box<[i16; SAMPLES]>>>,
    /// how many datagrams to hold before playing
    target: usize,
    playing: bool,
    calm: u32,
    missing: u32,
}

impl Default for Jitter {
    fn default() -> Self {
        Self {
            next: None,
            slots: VecDeque::new(),
            target: START,
            playing: false,
            calm: 0,
            missing: 0,
        }
    }
}

impl Jitter {
    /// how many datagrams it holds
    pub(crate) fn held(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    #[cfg(test)]
    pub(crate) fn target(&self) -> usize {
        self.target
    }

    /// datagram `number` arrived
    pub(crate) fn push(&mut self, number: u32, samples: [i16; SAMPLES]) {
        let next = *self.next.get_or_insert(number);
        let ahead = number.wrapping_sub(next);
        if ahead >= u32::MAX / 2 {
            // late: its turn is over
            return;
        }
        if ahead >= WINDOW {
            // far ahead (the sender started again): start from it
            self.reset();
            self.next = Some(number);
            self.slots.push_back(Some(Box::new(samples)));
            return;
        }
        let ahead = ahead as usize;
        while self.slots.len() <= ahead {
            self.slots.push_back(None);
        }
        self.slots[ahead].get_or_insert_with(|| Box::new(samples));
    }

    fn reset(&mut self) {
        *self = Self {
            target: self.target,
            ..Self::default()
        };
    }

    /// The next 5 ms of this stream, `None` while it holds too little to
    /// start or went quiet.
    pub(crate) fn pop(&mut self) -> Option<[i16; SAMPLES]> {
        if !self.playing {
            if self.held() < self.target {
                return None;
            }
            self.playing = true;
        }
        // the sender's clock is faster: catch up
        while self.slots.len() > self.target + SLACK {
            self.slots.pop_front();
            self.next = self.next.map(|n| n.wrapping_add(1));
        }
        let slot = self.slots.pop_front().flatten();
        self.next = self.next.map(|n| n.wrapping_add(1));
        match slot {
            Some(samples) => {
                self.missing = 0;
                self.calm += 1;
                if self.calm >= CALM && self.target > START {
                    self.target -= 1;
                    self.calm = 0;
                }
                Some(*samples)
            }
            None => {
                self.missing += 1;
                self.calm = 0;
                if self.missing >= QUIET {
                    // the sender stopped: wait for it to fill up again
                    self.reset();
                    return None;
                }
                if self.missing == 1 {
                    self.target = (self.target + 1).min(MOST);
                }
                Some([0; SAMPLES])
            }
        }
    }
}

/// `sources` mixed, each at its volume (0.0 to 1.0)
pub(crate) fn mix(sources: &[([i16; SAMPLES], f32)]) -> [i16; SAMPLES] {
    let mut out = [0i16; SAMPLES];
    for (i, sample) in out.iter_mut().enumerate() {
        let sum: f32 = sources.iter().map(|(s, v)| f32::from(s[i]) * v).sum();
        *sample = sum.clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(n: i16) -> [i16; SAMPLES] {
        [n; SAMPLES]
    }

    #[test]
    fn it_holds_then_plays_in_order() {
        let mut j = Jitter::default();
        j.push(10, d(10));
        assert_eq!(j.pop(), None, "holds two first");
        j.push(12, d(12));
        j.push(11, d(11));
        assert_eq!(j.pop(), Some(d(10)));
        assert_eq!(j.pop(), Some(d(11)));
        assert_eq!(j.pop(), Some(d(12)));
        // a late one and a doubled one are dropped
        j.push(11, d(99));
        j.push(13, d(13));
        j.push(13, d(98));
        assert_eq!(j.pop(), Some(d(13)));
    }

    #[test]
    fn a_missing_one_is_silence_and_holds_more() {
        let mut j = Jitter::default();
        for n in 0..3 {
            j.push(n, d(1));
        }
        j.pop();
        j.pop();
        j.pop();
        assert_eq!(j.pop(), Some(d(0)), "silence");
        assert_eq!(j.target(), START + 1);
        // calm again for long: back to holding less
        for n in 4..4 + CALM {
            j.push(n, d(1));
            j.pop();
        }
        assert_eq!(j.target(), START);
    }

    #[test]
    fn a_faster_sender_does_not_pile_up() {
        let mut j = Jitter::default();
        let mut n = 0;
        for _ in 0..200 {
            // two arrive for every one played
            j.push(n, d(1));
            j.push(n + 1, d(1));
            n += 2;
            j.pop();
        }
        assert!(j.held() <= j.target() + SLACK + 2, "{}", j.held());
    }

    #[test]
    fn a_quiet_sender_starts_again() {
        let mut j = Jitter::default();
        j.push(0, d(1));
        j.push(1, d(1));
        j.pop();
        j.pop();
        for _ in 0..QUIET {
            j.pop();
        }
        assert_eq!(j.pop(), None);
        // it sends again, numbers far ahead
        j.push(5000, d(2));
        j.push(5001, d(2));
        j.push(5002, d(2));
        j.push(5003, d(2));
        assert_eq!(j.pop(), Some(d(2)));
    }

    #[test]
    fn mixing_adds_and_clips() {
        let mixed = mix(&[(d(1000), 1.0), (d(500), 0.5)]);
        assert_eq!(mixed[0], 1250);
        assert_eq!(mix(&[(d(30000), 1.0), (d(30000), 1.0)])[0], i16::MAX);
        assert_eq!(mix(&[])[0], 0);
    }
}
