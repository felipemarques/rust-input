//! Keyboard shortcuts to move between the computers (0.42.0):
//!
//! - AltGr + right Shift + 1 … 9: to that computer, in the order of the
//!   list; + 0: back to this one;
//! - AltGr + right Shift alone: with one other computer, there or back;
//!   with more, the next one;
//! - AltGr + Right / AltGr + Left, or the right Shift twice quickly: the
//!   next / the previous computer (this one counts as one of them);
//! - the right Shift held for 3 seconds alone: back to this computer, always.
//!
//! A shortcut fires when its keys are let go, and only if nothing else was
//! pressed meanwhile, so AltGr + Q or Shift + 1 still type. The keys are only
//! watched, never taken from the applications. The left Ctrl does not count:
//! Windows sends it with AltGr.
//!
//! [`Hotkeys`] is fed the keys both where the pointer is here (the watcher of
//! the radial menu, X11 and Windows) and while it is on another computer (the
//! capture, every system).

use std::time::{Duration, Instant};

/// the right Shift twice within this long is a double tap
pub(crate) const DOUBLE_TAP: Duration = Duration::from_millis(400);
/// the right Shift held this long alone brings the pointer back
pub(crate) const HOLD_HOME: Duration = Duration::from_secs(3);

/// the keys that matter
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) enum Key {
    /// the right Alt
    AltGr,
    RightShift,
    /// the left Ctrl, which Windows sends along with AltGr: never counts
    LeftCtrl,
    Digit(u8),
    Left,
    Right,
    Other,
}

impl Key {
    /// from an evdev key code (`scancode::Linux`, X keycode − 8)
    pub(crate) fn from_linux(code: u32) -> Self {
        match code {
            100 => Key::AltGr,
            54 => Key::RightShift,
            29 => Key::LeftCtrl,
            2..=10 => Key::Digit((code - 1) as u8),
            11 => Key::Digit(0),
            105 => Key::Left,
            106 => Key::Right,
            _ => Key::Other,
        }
    }

    /// from a virtual key code of Windows, left and right told apart
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn from_windows(vk: u16) -> Self {
        match vk {
            0xA5 => Key::AltGr,      // VK_RMENU
            0xA1 => Key::RightShift, // VK_RSHIFT
            0xA2 => Key::LeftCtrl,   // VK_LCONTROL
            0x30..=0x39 => Key::Digit((vk - 0x30) as u8),
            0x25 => Key::Left,  // VK_LEFT
            0x27 => Key::Right, // VK_RIGHT
            _ => Key::Other,
        }
    }
}

/// what a shortcut does
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Hotkey {
    /// to the n-th computer of the list (from 1)
    GoTo(u8),
    /// back to this computer
    Home,
    /// with one other computer there or back, with more the next
    Toggle,
    Next,
    Previous,
}

#[derive(Debug, Default)]
pub(crate) struct Hotkeys {
    alt_gr: bool,
    shift: bool,
    /// keys down other than AltGr, the right Shift and the left Ctrl
    others: Vec<Key>,
    /// what fires once everything is let go
    pending: Option<Hotkey>,
    /// another key took part: nothing fires
    spoiled: bool,
    /// the right Shift went down with nothing else
    shift_alone: Option<Instant>,
    /// when the right Shift was last tapped alone
    last_tap: Option<Instant>,
}

impl Hotkeys {
    /// forgets keys half pressed (the pointer moved to another computer,
    /// whose own shortcuts take over)
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    fn idle(&self) -> bool {
        !self.alt_gr && !self.shift && self.others.is_empty()
    }

    /// A key went down (`pressed`) or up; the shortcut that fires, if one
    /// does.
    pub(crate) fn key(&mut self, key: Key, pressed: bool, now: Instant) -> Option<Hotkey> {
        if key == Key::LeftCtrl {
            return None;
        }
        if pressed {
            self.down(key, now);
            None
        } else {
            self.up(key, now)
        }
    }

    fn down(&mut self, key: Key, now: Instant) {
        let fresh = self.idle();
        if fresh {
            self.spoiled = false;
            self.pending = None;
        }
        match key {
            Key::AltGr => {
                self.alt_gr = true;
                self.shift_alone = None;
            }
            Key::RightShift => {
                self.shift = true;
                self.shift_alone = (fresh && !self.alt_gr).then_some(now);
            }
            Key::Digit(n) if self.alt_gr && self.shift && self.others.is_empty() => {
                self.others.push(key);
                self.pending = Some(if n == 0 {
                    Hotkey::Home
                } else {
                    Hotkey::GoTo(n)
                });
            }
            Key::Left | Key::Right if self.alt_gr && !self.shift && self.others.is_empty() => {
                self.others.push(key);
                self.pending = Some(if key == Key::Right {
                    Hotkey::Next
                } else {
                    Hotkey::Previous
                });
            }
            other => {
                if !self.others.contains(&other) {
                    self.others.push(other);
                }
                self.spoiled = true;
                self.shift_alone = None;
                self.last_tap = None;
            }
        }
        if self.alt_gr && self.shift && self.others.is_empty() && self.pending.is_none() {
            self.pending = Some(Hotkey::Toggle);
        }
    }

    fn up(&mut self, key: Key, now: Instant) -> Option<Hotkey> {
        match key {
            Key::AltGr => self.alt_gr = false,
            Key::RightShift => {
                self.shift = false;
                // the right Shift alone: held long, or tapped twice
                if let Some(since) = self.shift_alone.take() {
                    if !self.spoiled && self.idle() {
                        if now.duration_since(since) >= HOLD_HOME {
                            self.last_tap = None;
                            return Some(Hotkey::Home);
                        }
                        let double = self
                            .last_tap
                            .is_some_and(|last| now.duration_since(last) <= DOUBLE_TAP);
                        self.last_tap = (!double).then_some(now);
                        return double.then_some(Hotkey::Next);
                    }
                }
            }
            other => self.others.retain(|k| *k != other),
        }
        if self.idle() {
            let fired = self.pending.take().filter(|_| !self.spoiled);
            self.spoiled = false;
            return fired;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// presses `keys` in order, then lets them go in reverse, `step` apart;
    /// what fired
    fn chord(h: &mut Hotkeys, keys: &[Key], at: &mut Instant) -> Vec<Hotkey> {
        let mut fired = Vec::new();
        for &k in keys {
            fired.extend(h.key(k, true, *at));
            *at += Duration::from_millis(30);
        }
        for &k in keys.iter().rev() {
            fired.extend(h.key(k, false, *at));
            *at += Duration::from_millis(30);
        }
        fired
    }

    #[test]
    fn shortcuts_fire_when_let_go() {
        let (mut h, mut t) = (Hotkeys::default(), Instant::now());
        use Key::*;
        assert_eq!(
            chord(&mut h, &[AltGr, RightShift, Digit(2)], &mut t),
            [Hotkey::GoTo(2)]
        );
        assert_eq!(
            chord(&mut h, &[AltGr, RightShift, Digit(0)], &mut t),
            [Hotkey::Home]
        );
        assert_eq!(
            chord(&mut h, &[AltGr, RightShift], &mut t),
            [Hotkey::Toggle]
        );
        assert_eq!(chord(&mut h, &[AltGr, Right], &mut t), [Hotkey::Next]);
        assert_eq!(chord(&mut h, &[AltGr, Left], &mut t), [Hotkey::Previous]);
        // Windows sends the left Ctrl with AltGr
        assert_eq!(
            chord(&mut h, &[LeftCtrl, AltGr, RightShift, Digit(1)], &mut t),
            [Hotkey::GoTo(1)]
        );
    }

    #[test]
    fn typing_with_altgr_fires_nothing() {
        let (mut h, mut t) = (Hotkeys::default(), Instant::now());
        use Key::*;
        // AltGr + Q types "/" on ABNT2
        assert!(chord(&mut h, &[AltGr, Other], &mut t).is_empty());
        assert!(chord(&mut h, &[AltGr, RightShift, Digit(1), Other], &mut t).is_empty());
        assert!(chord(&mut h, &[AltGr, RightShift, Digit(1), Digit(2)], &mut t).is_empty());
        assert!(
            chord(&mut h, &[RightShift, Digit(1)], &mut t).is_empty(),
            "Shift + 1 types !"
        );
        assert!(chord(&mut h, &[AltGr, Digit(1)], &mut t).is_empty());
        // and the next one still works
        assert_eq!(
            chord(&mut h, &[AltGr, RightShift, Digit(3)], &mut t),
            [Hotkey::GoTo(3)]
        );
    }

    #[test]
    fn the_right_shift_alone() {
        let (mut h, mut t) = (Hotkeys::default(), Instant::now());
        use Key::*;
        // one tap, then another quickly: the next computer
        assert!(chord(&mut h, &[RightShift], &mut t).is_empty());
        assert_eq!(chord(&mut h, &[RightShift], &mut t), [Hotkey::Next]);
        // a third tap starts again
        assert!(chord(&mut h, &[RightShift], &mut t).is_empty());
        // too slow
        t += Duration::from_secs(1);
        assert!(chord(&mut h, &[RightShift], &mut t).is_empty());
        // a letter between the taps
        assert!(chord(&mut h, &[Other], &mut t).is_empty());
        assert!(chord(&mut h, &[RightShift], &mut t).is_empty());
        // held for 3 seconds: back here
        t += Duration::from_secs(1);
        assert!(h.key(RightShift, true, t).is_none());
        t += HOLD_HOME;
        assert_eq!(h.key(RightShift, false, t), Some(Hotkey::Home));
        // held while typing a capital letter: nothing
        assert!(h.key(RightShift, true, t).is_none());
        assert!(h.key(Other, true, t).is_none());
        assert!(h.key(Other, false, t).is_none());
        t += HOLD_HOME;
        assert!(h.key(RightShift, false, t).is_none());
    }

    #[test]
    fn keys_of_linux_and_windows() {
        assert_eq!(Key::from_linux(100), Key::AltGr);
        assert_eq!(Key::from_linux(2), Key::Digit(1));
        assert_eq!(Key::from_linux(10), Key::Digit(9));
        assert_eq!(Key::from_linux(11), Key::Digit(0));
        assert_eq!(Key::from_linux(16), Key::Other);
        assert_eq!(Key::from_windows(0x31), Key::Digit(1));
        assert_eq!(Key::from_windows(0xA5), Key::AltGr);
        assert_eq!(Key::from_windows(0xA0), Key::Other, "the left Shift");
    }
}
