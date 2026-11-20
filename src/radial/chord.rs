//! When the menu opens and closes: holding Ctrl and the left Alt, and nothing
//! else, for a moment. The keys are watched without taking them away from the
//! applications, so every shortcut with Ctrl+Alt keeps working: pressing
//! another key before the menu opens means it does not open, and while it is
//! open closes it without picking anything.

use std::time::{Duration, Instant};

/// how long Ctrl+Alt have to be held alone
pub(super) const HOLD: Duration = Duration::from_millis(250);

/// the keys that matter
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Key {
    /// either Ctrl
    Ctrl,
    /// the left Alt only: AltGr, the right one, types characters
    Alt,
    Escape,
    Other,
}

/// what the menu has to do
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Step {
    Nothing,
    Open,
    /// closes, picking what the pointer points at
    Pick,
    /// closes without picking anything
    Cancel,
}

#[derive(Debug, Default)]
pub(super) struct Chord {
    ctrl: bool,
    alt: bool,
    /// another key took part since Ctrl+Alt were first both held
    spoiled: bool,
    /// when Ctrl and Alt were both held
    since: Option<Instant>,
    open: bool,
}

impl Chord {
    #[cfg(test)]
    fn is_open(&self) -> bool {
        self.open
    }

    /// a key went down (`pressed`) or up
    pub(super) fn key(&mut self, key: Key, pressed: bool, now: Instant) -> Step {
        let step = match (key, pressed) {
            (Key::Ctrl, _) => {
                self.ctrl = pressed;
                Step::Nothing
            }
            (Key::Alt, _) => {
                self.alt = pressed;
                Step::Nothing
            }
            (Key::Escape | Key::Other, true) => {
                self.spoiled = self.ctrl || self.alt;
                if self.open {
                    self.open = false;
                    return Step::Cancel;
                }
                Step::Nothing
            }
            (Key::Escape | Key::Other, false) => Step::Nothing,
        };
        if self.ctrl && self.alt {
            self.since.get_or_insert(now);
        } else {
            self.since = None;
            if !self.ctrl && !self.alt {
                self.spoiled = false;
            }
            if self.open {
                self.open = false;
                return Step::Pick;
            }
        }
        step
    }

    /// to be called regularly while Ctrl+Alt are held; `away` tells that the
    /// pointer is on another device, which shows its own menu
    pub(super) fn tick(&mut self, now: Instant, away: bool) -> Step {
        let held = self.since.is_some_and(|s| now.duration_since(s) >= HOLD);
        if held && !self.spoiled && !self.open && !away {
            self.open = true;
            return Step::Open;
        }
        if self.open && away {
            self.open = false;
            return Step::Cancel;
        }
        Step::Nothing
    }

    /// whether [`Chord::tick`] may open the menu soon, so it is worth calling
    pub(super) fn pending(&self) -> bool {
        self.since.is_some() && !self.spoiled && !self.open
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn later(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    #[test]
    fn holding_ctrl_alt_opens_and_letting_go_picks() {
        let t = Instant::now();
        let mut c = Chord::default();
        assert_eq!(c.key(Key::Ctrl, true, t), Step::Nothing);
        assert_eq!(c.key(Key::Alt, true, later(t, 10)), Step::Nothing);
        assert_eq!(c.tick(later(t, 100), false), Step::Nothing);
        assert!(c.pending());
        assert_eq!(c.tick(later(t, 300), false), Step::Open);
        assert_eq!(c.tick(later(t, 400), false), Step::Nothing);
        assert_eq!(c.key(Key::Alt, false, later(t, 500)), Step::Pick);
        assert_eq!(c.key(Key::Ctrl, false, later(t, 510)), Step::Nothing);
        assert!(!c.is_open());
    }

    #[test]
    fn shortcuts_with_ctrl_alt_do_not_open_it() {
        let t = Instant::now();
        let mut c = Chord::default();
        c.key(Key::Ctrl, true, t);
        c.key(Key::Alt, true, t);
        // Ctrl+Alt+T
        assert_eq!(c.key(Key::Other, true, later(t, 50)), Step::Nothing);
        c.key(Key::Other, false, later(t, 80));
        assert_eq!(c.tick(later(t, 1000), false), Step::Nothing);
        // still not after letting go of T and holding on
        assert!(!c.pending());
        // letting go of both starts over
        c.key(Key::Alt, false, later(t, 1100));
        c.key(Key::Ctrl, false, later(t, 1100));
        c.key(Key::Alt, true, later(t, 1200));
        c.key(Key::Ctrl, true, later(t, 1200));
        assert_eq!(c.tick(later(t, 1500), false), Step::Open);
    }

    #[test]
    fn another_key_or_escape_closes_without_picking() {
        let t = Instant::now();
        let mut c = Chord::default();
        c.key(Key::Ctrl, true, t);
        c.key(Key::Alt, true, t);
        assert_eq!(c.tick(later(t, 300), false), Step::Open);
        assert_eq!(c.key(Key::Escape, true, later(t, 400)), Step::Cancel);
        // letting go afterwards picks nothing
        assert_eq!(c.key(Key::Alt, false, later(t, 500)), Step::Nothing);
    }

    #[test]
    fn not_while_the_pointer_is_away() {
        let t = Instant::now();
        let mut c = Chord::default();
        c.key(Key::Ctrl, true, t);
        c.key(Key::Alt, true, t);
        assert_eq!(c.tick(later(t, 300), true), Step::Nothing);
        assert_eq!(c.tick(later(t, 400), false), Step::Open);
        // the pointer goes away while it is open
        assert_eq!(c.tick(later(t, 500), true), Step::Cancel);
    }

    #[test]
    fn alt_alone_or_ctrl_alone_does_nothing() {
        let t = Instant::now();
        let mut c = Chord::default();
        c.key(Key::Alt, true, t);
        assert_eq!(c.tick(later(t, 1000), false), Step::Nothing);
        c.key(Key::Alt, false, later(t, 1000));
        c.key(Key::Ctrl, true, later(t, 1000));
        assert_eq!(c.tick(later(t, 2000), false), Step::Nothing);
    }
}
