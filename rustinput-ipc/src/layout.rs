//! The arrangement of the computers, like the monitors in the display
//! settings of a desktop: where each computer is, from which the parts of
//! their edges that meet follow ([`Layout::neighbors`]).
//!
//! Every computer of an arrangement keeps the same one: arranged on one of
//! them, it goes to the others (see the RustInput documentation). Each finds
//! itself in it and sets up its connections to its neighbors from it.

use std::net::IpAddr;

use input_event::edge::{EdgeLink, Span};
use serde::{Deserialize, Serialize};

use crate::{DEFAULT_PORT, Position};

/// two edges this close (in pixels) touch
const TOUCHING: i32 = 2;

/// a monitor of a computer, in pixels from the top left corner of its desktop
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Monitor {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// a computer of the arrangement
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Machine {
    /// how the computers know each other: the fingerprint of its certificate,
    /// or else its name or address
    pub id: String,
    /// shown in the arrangement
    pub name: String,
    /// the top left corner of its desktop in the arrangement, in pixels
    pub x: i32,
    pub y: i32,
    /// the size of its desktop, in pixels
    pub width: i32,
    pub height: i32,
    /// its monitors, from the top left corner of its desktop
    #[serde(default)]
    pub monitors: Vec<Monitor>,
    /// how the other computers reach it, for those that do not know it yet
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub ips: Vec<IpAddr>,
    #[serde(default = "default_port")]
    pub port: u16,
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

impl Machine {
    fn right(&self) -> i32 {
        self.x + self.width
    }

    fn bottom(&self) -> i32 {
        self.y + self.height
    }
}

/// the computers and how they are arranged
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    /// grows with every change: the arrangement with the higher one is the
    /// newer
    pub revision: u64,
    pub machines: Vec<Machine>,
    /// pairs of computers that meet over the whole of their edges, the
    /// pointer keeping its place in proportion (as without an arrangement),
    /// rather than only where their screens touch; each pair in order
    #[serde(default)]
    pub whole_edges: Vec<[String; 2]>,
}

/// a computer next to another one, as seen from that one
#[derive(Clone, Debug, PartialEq)]
pub struct Neighbor {
    pub id: String,
    /// the edge of the other computer it is at
    pub pos: Position,
    /// the parts of the edges that meet
    pub link: EdgeLink,
}

/// where two computers touch: the edge of the first one, and the stretch of
/// the arrangement along it they share
struct Touch {
    pos: Position,
    from: i32,
    to: i32,
}

fn touch(a: &Machine, b: &Machine) -> Option<Touch> {
    let near = |p: i32, q: i32| (p - q).abs() <= TOUCHING;
    let (from_y, to_y) = (a.y.max(b.y), a.bottom().min(b.bottom()));
    let (from_x, to_x) = (a.x.max(b.x), a.right().min(b.right()));
    let pos = if to_y > from_y && near(a.right(), b.x) {
        Position::Right
    } else if to_y > from_y && near(b.right(), a.x) {
        Position::Left
    } else if to_x > from_x && near(a.bottom(), b.y) {
        Position::Bottom
    } else if to_x > from_x && near(b.bottom(), a.y) {
        Position::Top
    } else {
        return None;
    };
    let (from, to) = match pos {
        Position::Left | Position::Right => (from_y, to_y),
        Position::Top | Position::Bottom => (from_x, to_x),
    };
    Some(Touch { pos, from, to })
}

/// the part of the edge of `m` at `pos` from `from` to `to` in the arrangement
fn span(m: &Machine, pos: Position, from: i32, to: i32) -> Span {
    let (start, length) = match pos {
        Position::Left | Position::Right => (m.y, m.height),
        Position::Top | Position::Bottom => (m.x, m.width),
    };
    if length <= 0 {
        return Span::FULL;
    }
    let at = |p: i32| (p - start) as f64 / length as f64;
    Span::new(at(from), at(to))
}

impl Layout {
    pub fn machine(&self, id: &str) -> Option<&Machine> {
        self.machines.iter().find(|m| m.id == id)
    }

    /// whether `a` and `b` meet over the whole of their edges
    pub fn whole(&self, a: &str, b: &str) -> bool {
        self.whole_edges
            .iter()
            .any(|[x, y]| (x == a && y == b) || (x == b && y == a))
    }

    /// how many computers touch the edge of `m` at `pos`
    fn touching(&self, m: &Machine, pos: Position) -> usize {
        self.machines
            .iter()
            .filter(|o| o.id != m.id)
            .filter(|o| touch(m, o).is_some_and(|t| t.pos == pos))
            .count()
    }

    /// The computers next to `id`, each with the parts of the edges that
    /// meet. A pair in [`Layout::whole_edges`] meets over the whole of both
    /// edges as long as neither edge is shared with a third computer.
    pub fn neighbors(&self, id: &str) -> Vec<Neighbor> {
        let Some(me) = self.machine(id) else {
            return Vec::new();
        };
        self.machines
            .iter()
            .filter(|o| o.id != me.id)
            .filter_map(|o| {
                let t = touch(me, o)?;
                let whole = self.whole(&me.id, &o.id)
                    && self.touching(me, t.pos) == 1
                    && self.touching(o, t.pos.opposite()) == 1;
                let link = if whole {
                    EdgeLink::FULL
                } else {
                    EdgeLink {
                        own: span(me, t.pos, t.from, t.to),
                        peer: span(o, t.pos.opposite(), t.from, t.to),
                    }
                };
                Some(Neighbor {
                    id: o.id.clone(),
                    pos: t.pos,
                    link,
                })
            })
            .collect()
    }

    /// The computers that do not touch any other one that leads to `id`:
    /// the pointer can not reach them from there.
    pub fn unreachable_from(&self, id: &str) -> Vec<String> {
        let mut reached = vec![id.to_owned()];
        let mut next = 0;
        while next < reached.len() {
            let current = reached[next].clone();
            next += 1;
            for n in self.neighbors(&current) {
                if !reached.contains(&n.id) {
                    reached.push(n.id);
                }
            }
        }
        self.machines
            .iter()
            .filter(|m| !reached.contains(&m.id))
            .map(|m| m.id.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine(id: &str, x: i32, y: i32, width: i32, height: i32) -> Machine {
        Machine {
            id: id.to_owned(),
            name: id.to_owned(),
            x,
            y,
            width,
            height,
            monitors: Vec::new(),
            hostname: None,
            ips: Vec::new(),
            port: DEFAULT_PORT,
        }
    }

    /// two stacked monitors (1920 x 2160) and a computer of 1920 x 1080
    /// right of the lower one
    fn linux_and_windows() -> Layout {
        Layout {
            revision: 1,
            machines: vec![
                machine("linux", 0, 0, 1920, 2160),
                machine("windows", 1920, 1080, 1920, 1080),
            ],
            whole_edges: Vec::new(),
        }
    }

    #[test]
    fn next_to_the_lower_monitor() {
        let layout = linux_and_windows();
        let from_linux = layout.neighbors("linux");
        assert_eq!(
            from_linux,
            vec![Neighbor {
                id: "windows".into(),
                pos: Position::Right,
                link: EdgeLink {
                    own: Span::new(0.5, 1.0),
                    peer: Span::FULL,
                },
            }]
        );
        let from_windows = layout.neighbors("windows");
        assert_eq!(from_windows[0].pos, Position::Left);
        assert_eq!(from_windows[0].link, from_linux[0].link.reversed());
    }

    #[test]
    fn the_whole_edges_in_proportion() {
        let mut layout = linux_and_windows();
        layout.whole_edges.push(["linux".into(), "windows".into()]);
        assert_eq!(layout.neighbors("linux")[0].link, EdgeLink::FULL);
        assert_eq!(layout.neighbors("windows")[0].link, EdgeLink::FULL);
    }

    #[test]
    fn a_shared_edge_is_never_whole() {
        let mut layout = linux_and_windows();
        // a laptop right of the upper monitor
        layout.machines.push(machine("laptop", 1920, 0, 1440, 900));
        layout.whole_edges.push(["linux".into(), "windows".into()]);
        let mut from_linux = layout.neighbors("linux");
        from_linux.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(from_linux[0].id, "laptop");
        assert_eq!(from_linux[0].link.own, Span::new(0.0, 900.0 / 2160.0));
        assert_eq!(from_linux[1].id, "windows");
        assert_eq!(from_linux[1].link.own, Span::new(0.5, 1.0));
    }

    #[test]
    fn above_and_below() {
        let layout = Layout {
            revision: 1,
            machines: vec![
                machine("a", 0, 0, 1000, 500),
                machine("b", 500, 500, 1000, 500),
            ],
            whole_edges: Vec::new(),
        };
        let n = &layout.neighbors("a")[0];
        assert_eq!(n.pos, Position::Bottom);
        assert_eq!(n.link.own, Span::new(0.5, 1.0));
        assert_eq!(n.link.peer, Span::new(0.0, 0.5));
        assert_eq!(layout.neighbors("b")[0].pos, Position::Top);
    }

    #[test]
    fn corners_do_not_meet_and_far_ones_are_unreachable() {
        let layout = Layout {
            revision: 1,
            machines: vec![
                machine("a", 0, 0, 100, 100),
                machine("corner", 100, 100, 100, 100),
                machine("b", 100, 0, 100, 100),
            ],
            whole_edges: Vec::new(),
        };
        let ids: Vec<String> = layout.neighbors("a").into_iter().map(|n| n.id).collect();
        assert_eq!(ids, vec!["b".to_owned()]);
        // the corner touches b from below
        assert!(layout.unreachable_from("a").is_empty());
        let alone = Layout {
            machines: vec![
                machine("a", 0, 0, 100, 100),
                machine("far", 500, 0, 100, 100),
            ],
            ..layout
        };
        assert_eq!(alone.unreachable_from("a"), vec!["far".to_owned()]);
    }
}
