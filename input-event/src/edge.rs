//! Positions along the screen edges, used to keep the pointer at the same
//! relative height (or width) when it moves from one device to another.
//!
//! Every device treats all of its monitors as one rectangle, like Synergy and
//! Deskflow: leaving the top of the left edge of one device enters the top of
//! the right edge of the next one, whatever the size of their screens.
//!
//! Arranged like monitors, two devices meet on a part of their edges only (a
//! [`Span`] of each): the pointer crosses there and nowhere else, and keeps
//! its place along the way ([`EdgeLink`]). By default a link joins the whole
//! edges, as above.

use serde::{Deserialize, Serialize};

/// a side of the screen
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

/// a rectangle of the desktop, in pixels
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Area {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Area {
    /// the smallest area containing all of `areas`, `None` if there are none
    pub fn bounding(areas: impl IntoIterator<Item = Area>) -> Option<Area> {
        let mut areas = areas.into_iter().filter(|a| a.width > 0 && a.height > 0);
        let first = areas.next()?;
        let (mut left, mut top) = (first.x, first.y);
        let (mut right, mut bottom) = (first.x + first.width, first.y + first.height);
        for a in areas {
            left = left.min(a.x);
            top = top.min(a.y);
            right = right.max(a.x + a.width);
            bottom = bottom.max(a.y + a.height);
        }
        Some(Area {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
        })
    }

    /// where `point` lies along `edge`: 0.0 at the top (left) end of the area,
    /// 1.0 at its bottom (right) end
    pub fn fraction(&self, edge: Edge, point: (i32, i32)) -> f64 {
        let (offset, length) = match edge {
            Edge::Left | Edge::Right => (point.1 - self.y, self.height),
            Edge::Top | Edge::Bottom => (point.0 - self.x, self.width),
        };
        if length <= 1 {
            return 0.5;
        }
        (offset as f64 / (length - 1) as f64).clamp(0.0, 1.0)
    }

    /// The point at `fraction` along `edge`, one pixel inside the area: the
    /// pointer must not rest on the edge itself, where it would cross right
    /// back at the slightest movement.
    pub fn point(&self, edge: Edge, fraction: f64) -> (i32, i32) {
        let fraction = if fraction.is_finite() {
            fraction.clamp(0.0, 1.0)
        } else {
            0.5
        };
        let along = |start: i32, length: i32| {
            start + (fraction * (length - 1).max(0) as f64).round() as i32
        };
        let inside = |start: i32, length: i32, far: bool| {
            let inset = 1.min(length - 1).max(0);
            if far {
                start + length - 1 - inset
            } else {
                start + inset
            }
        };
        match edge {
            Edge::Left => (
                inside(self.x, self.width, false),
                along(self.y, self.height),
            ),
            Edge::Right => (inside(self.x, self.width, true), along(self.y, self.height)),
            Edge::Top => (
                along(self.x, self.width),
                inside(self.y, self.height, false),
            ),
            Edge::Bottom => (along(self.x, self.width), inside(self.y, self.height, true)),
        }
    }
}

/// how close a fraction may be to a [`Span`] and still count as on it
const SPAN_TOLERANCE: f64 = 1e-6;

/// A part of an edge, from `lo` to `hi` along it, as fractions of the edge
/// (see [`Area::fraction`]).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Span {
    pub lo: f64,
    pub hi: f64,
}

// never NaN: `Span::new` and the parts of a link are finite numbers
impl Eq for Span {}

impl Default for Span {
    fn default() -> Self {
        Self::FULL
    }
}

impl Span {
    /// the whole edge
    pub const FULL: Span = Span { lo: 0.0, hi: 1.0 };

    /// `lo..hi` within `0.0..=1.0`, in order; the whole edge when they do not
    /// make a part of it
    pub fn new(lo: f64, hi: f64) -> Self {
        if !lo.is_finite() || !hi.is_finite() {
            return Self::FULL;
        }
        let (lo, hi) = (lo.min(hi).clamp(0.0, 1.0), lo.max(hi).clamp(0.0, 1.0));
        if hi - lo < SPAN_TOLERANCE {
            return Self::FULL;
        }
        Span { lo, hi }
    }

    pub fn is_full(&self) -> bool {
        self.lo <= SPAN_TOLERANCE && self.hi >= 1.0 - SPAN_TOLERANCE
    }

    /// whether `fraction` lies on this part of the edge
    pub fn contains(&self, fraction: f64) -> bool {
        fraction >= self.lo - SPAN_TOLERANCE && fraction <= self.hi + SPAN_TOLERANCE
    }

    /// whether this part and `other` share more than an end
    pub fn overlaps(&self, other: &Span) -> bool {
        self.lo < other.hi - SPAN_TOLERANCE && other.lo < self.hi - SPAN_TOLERANCE
    }

    /// how far `fraction` is from this part of the edge, 0.0 on it
    pub fn distance(&self, fraction: f64) -> f64 {
        if fraction < self.lo {
            self.lo - fraction
        } else if fraction > self.hi {
            fraction - self.hi
        } else {
            0.0
        }
    }

    fn length(&self) -> f64 {
        self.hi - self.lo
    }

    /// where `fraction` of the edge lies along this part, 0.0 to 1.0
    fn position_of(&self, fraction: f64) -> f64 {
        ((fraction - self.lo) / self.length()).clamp(0.0, 1.0)
    }

    /// the fraction of the edge `position` along this part
    fn at(&self, position: f64) -> f64 {
        self.lo + position.clamp(0.0, 1.0) * self.length()
    }
}

/// Where two devices meet: the part of the edge of this device (`own`) that
/// leads to the other device, onto the part of the edge of the other one
/// (`peer`). A place along one maps to the same relative place along the
/// other.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EdgeLink {
    pub own: Span,
    pub peer: Span,
}

impl Eq for EdgeLink {}

impl EdgeLink {
    /// whole edge to whole edge, as before arrangements
    pub const FULL: EdgeLink = EdgeLink {
        own: Span::FULL,
        peer: Span::FULL,
    };

    /// where the pointer arrives on the other device when it leaves this one
    /// at `fraction` along its edge
    pub fn to_peer(&self, fraction: f64) -> f64 {
        self.peer.at(self.own.position_of(fraction))
    }

    /// where the pointer arrives on this device when it leaves the other one
    /// at `fraction` along its edge
    pub fn to_own(&self, fraction: f64) -> f64 {
        self.own.at(self.peer.position_of(fraction))
    }

    /// the same link seen from the other device
    pub fn reversed(&self) -> EdgeLink {
        EdgeLink {
            own: self.peer,
            peer: self.own,
        }
    }

    pub fn is_full(&self) -> bool {
        self.own.is_full() && self.peer.is_full()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_HD: Area = Area {
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
    };

    #[test]
    fn bounding_area_of_stacked_monitors() {
        let bottom = Area {
            x: 0,
            y: 1080,
            width: 1920,
            height: 1080,
        };
        let area = Area::bounding([bottom, FULL_HD]).unwrap();
        assert_eq!(
            area,
            Area {
                x: 0,
                y: 0,
                width: 1920,
                height: 2160
            }
        );
        // monitors left of and above the origin (Windows allows that)
        let left = Area {
            x: -1280,
            y: -200,
            width: 1280,
            height: 1024,
        };
        let area = Area::bounding([FULL_HD, left]).unwrap();
        assert_eq!(
            (area.x, area.y, area.width, area.height),
            (-1280, -200, 3200, 1280)
        );
        assert_eq!(Area::bounding([]), None);
    }

    #[test]
    fn fraction_along_the_edges() {
        assert_eq!(FULL_HD.fraction(Edge::Left, (0, 0)), 0.0);
        assert_eq!(FULL_HD.fraction(Edge::Right, (1919, 1079)), 1.0);
        assert_eq!(FULL_HD.fraction(Edge::Top, (959, 0)), 959.0 / 1919.0);
        // outside of the area is clamped
        assert_eq!(FULL_HD.fraction(Edge::Left, (0, -50)), 0.0);
        assert_eq!(FULL_HD.fraction(Edge::Left, (0, 5000)), 1.0);
    }

    #[test]
    fn point_is_one_pixel_inside_the_edge() {
        assert_eq!(FULL_HD.point(Edge::Left, 0.0), (1, 0));
        assert_eq!(FULL_HD.point(Edge::Right, 1.0), (1918, 1079));
        assert_eq!(FULL_HD.point(Edge::Top, 0.5), (960, 1));
        assert_eq!(FULL_HD.point(Edge::Bottom, 0.25), (480, 1078));
        assert_eq!(FULL_HD.point(Edge::Left, f64::NAN), (1, 540));
    }

    #[test]
    fn same_relative_height_on_a_differently_sized_desktop() {
        // leaving a 1080 px high screen at a third of its height enters a
        // desktop of two stacked monitors at a third of its height
        let stacked = Area {
            x: 0,
            y: 0,
            width: 1920,
            height: 2160,
        };
        let fraction = FULL_HD.fraction(Edge::Left, (0, 359));
        // 359 / 1079 * 2159 = 718.3
        assert_eq!(stacked.point(Edge::Right, fraction), (1918, 718));
    }

    #[test]
    fn roundtrip_on_the_same_area() {
        for y in [0, 1, 500, 1078, 1079] {
            let fraction = FULL_HD.fraction(Edge::Right, (1919, y));
            assert_eq!(FULL_HD.point(Edge::Left, fraction).1, y);
        }
    }

    #[test]
    fn degenerate_areas_do_not_panic() {
        let tiny = Area {
            x: 5,
            y: 5,
            width: 1,
            height: 1,
        };
        assert_eq!(tiny.fraction(Edge::Left, (5, 5)), 0.5);
        assert_eq!(tiny.point(Edge::Right, 0.7), (5, 5));
    }

    #[test]
    fn spans_are_in_order_and_within_the_edge() {
        assert_eq!(Span::new(0.75, 0.25), Span { lo: 0.25, hi: 0.75 });
        assert_eq!(Span::new(-1.0, 0.5), Span { lo: 0.0, hi: 0.5 });
        assert_eq!(Span::new(0.4, 0.4), Span::FULL);
        assert_eq!(Span::new(f64::NAN, 0.5), Span::FULL);
        assert!(Span::new(0.0, 1.0).is_full());
        let half = Span::new(0.5, 1.0);
        assert!(half.contains(0.5) && half.contains(1.0) && !half.contains(0.49));
        assert_eq!(half.distance(0.25), 0.25);
        assert_eq!(half.distance(0.75), 0.0);
    }

    #[test]
    fn a_full_link_keeps_the_fraction() {
        for f in [0.0, 0.3, 1.0] {
            assert_eq!(EdgeLink::FULL.to_peer(f), f);
            assert_eq!(EdgeLink::FULL.to_own(f), f);
        }
    }

    #[test]
    fn a_partial_link_keeps_the_place_along_the_way() {
        // two monitors stacked (2160 px) beside one of 1080 px next to the
        // lower one: the lower half of this edge is the whole edge of the other
        let link = EdgeLink {
            own: Span::new(0.5, 1.0),
            peer: Span::FULL,
        };
        assert_eq!(link.to_peer(0.5), 0.0);
        assert_eq!(link.to_peer(0.75), 0.5);
        assert_eq!(link.to_peer(1.0), 1.0);
        assert_eq!(link.to_own(0.5), 0.75);
        // outside of the link stays at its end
        assert_eq!(link.to_peer(0.1), 0.0);
        let back = link.reversed();
        assert_eq!(back.to_peer(0.5), 0.75);
        assert_eq!(back.to_own(0.75), 0.5);
    }
}
