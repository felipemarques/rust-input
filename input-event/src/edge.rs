//! Positions along the screen edges, used to keep the pointer at the same
//! relative height (or width) when it moves from one device to another.
//!
//! Every device treats all of its monitors as one rectangle, like Synergy and
//! Deskflow: leaving the top of the left edge of one device enters the top of
//! the right edge of the next one, whatever the size of their screens.

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
}
