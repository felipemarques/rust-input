//! The arrangement of the computers, drawn like the monitors in the display
//! settings of a desktop: each computer at the size of its desktop, with its
//! monitors. Dragging a computer, or moving the selected one with the arrow
//! keys, glues it to the nearest free edge of another one, aligned with the
//! ends, the middle and the monitors where it comes close; the parts of the
//! edges that meet are drawn in the accent color, lighter where a pair meets
//! over the whole of its edges in proportion.

use std::cell::{Cell, RefCell};

use adw::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk, pango, prelude::*};
use rustinput_ipc::{
    ClientHandle, Position,
    layout::{ArrangementView, Layout, Machine},
};

use crate::i18n::{tr, trf};

/// the size given to a computer that did not tell its own
const GUESSED: (i32, i32) = (1920, 1080);
/// room around the computers, on screen
const PADDING: f64 = 28.0;
/// a computer comes this close (on screen) to a line it then sits on
const ALIGN: f64 = 12.0;
/// a step of the arrow keys, as a part of the width of the computer
const KEY_STEP: f64 = 0.05;

/// a computer of the arrangement in the editor
#[derive(Clone, Debug)]
pub(crate) struct Item {
    pub(crate) machine: Machine,
    /// this computer
    pub(crate) own: bool,
    /// the connection to it, for the others
    pub(crate) handle: Option<ClientHandle>,
    /// it described itself: its size is its own, not a guess
    pub(crate) described: bool,
    /// the connection is switched on, and it answers
    pub(crate) active: bool,
    pub(crate) alive: bool,
}

/// what the editor shows about the selected computer: one row per computer
/// next to it
#[derive(Clone, Debug)]
pub(crate) struct LinkInfo {
    pub(crate) other: String,
    pub(crate) other_name: String,
    /// the edge of the selected computer it is at
    pub(crate) pos: Position,
    /// where along that edge, as a text
    pub(crate) part: String,
    pub(crate) whole: bool,
    /// whether the pair can meet over the whole edges: neither edge is shared
    pub(crate) can_be_whole: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct Transform {
    scale: f64,
    dx: f64,
    dy: f64,
}

impl Transform {
    fn to_screen(self, x: f64, y: f64) -> (f64, f64) {
        (x * self.scale + self.dx, y * self.scale + self.dy)
    }

    fn to_layout(self, x: f64, y: f64) -> (f64, f64) {
        ((x - self.dx) / self.scale, (y - self.dy) / self.scale)
    }
}

#[derive(Clone, Copy, Debug)]
struct Drag {
    index: usize,
    /// where the pointer grabbed the computer, from its corner, in pixels of
    /// the arrangement
    offset: (f64, f64),
    start: (f64, f64),
    moved: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Snapshot {
    places: Vec<(String, i32, i32)>,
    whole: Vec<[String; 2]>,
}

#[derive(Default)]
struct Model {
    items: Vec<Item>,
    whole: Vec<[String; 2]>,
    revision: u64,
    selected: Option<usize>,
    drag: Option<Drag>,
    /// the transform kept while a computer is dragged
    frozen: Option<Transform>,
    /// as last applied or received
    applied: Snapshot,
    /// made from the connections, not applied yet
    proposed: bool,
    /// the computer the keyboard and the mouse of this one control, this
    /// one itself when none
    pointer_at: Option<ClientHandle>,
}

impl Model {
    fn snapshot(&self) -> Snapshot {
        let mut places: Vec<(String, i32, i32)> = self
            .items
            .iter()
            .map(|i| (i.machine.id.clone(), i.machine.x, i.machine.y))
            .collect();
        places.sort();
        let mut whole = self.whole.clone();
        whole.sort();
        Snapshot { places, whole }
    }

    fn layout(&self) -> Layout {
        Layout {
            revision: self.revision,
            machines: self.items.iter().map(|i| i.machine.clone()).collect(),
            whole_edges: self.whole.clone(),
        }
    }

    fn transform(&self, width: f64, height: f64) -> Transform {
        if let Some(frozen) = self.frozen {
            return frozen;
        }
        let Some((left, top, right, bottom)) = bounds(self.items.iter().map(|i| &i.machine)) else {
            return Transform {
                scale: 1.0,
                ..Default::default()
            };
        };
        let (w, h) = ((right - left) as f64, (bottom - top) as f64);
        let scale = ((width - 2.0 * PADDING) / w)
            .min((height - 2.0 * PADDING) / h)
            .max(0.001);
        Transform {
            scale,
            dx: (width - w * scale) / 2.0 - left as f64 * scale,
            dy: (height - h * scale) / 2.0 - top as f64 * scale,
        }
    }

    fn hit(&self, x: f64, y: f64) -> Option<usize> {
        self.items.iter().rposition(|i| {
            let m = &i.machine;
            x >= m.x as f64
                && x <= (m.x + m.width) as f64
                && y >= m.y as f64
                && y <= (m.y + m.height) as f64
        })
    }
}

fn bounds<'a>(machines: impl Iterator<Item = &'a Machine>) -> Option<(i32, i32, i32, i32)> {
    machines.fold(None, |acc, m| {
        let (l, t, r, b) = (m.x, m.y, m.x + m.width, m.y + m.height);
        Some(match acc {
            None => (l, t, r, b),
            Some((al, at, ar, ab)) => (al.min(l), at.min(t), ar.max(r), ab.max(b)),
        })
    })
}

fn overlaps(a: &Machine, x: i32, y: i32, w: i32, h: i32) -> bool {
    x < a.x + a.width && x + w > a.x && y < a.y + a.height && y + h > a.y
}

fn align_to(value: i32, targets: &[i32], tolerance: f64) -> i32 {
    targets
        .iter()
        .copied()
        .filter(|t| ((t - value) as f64).abs() <= tolerance)
        .min_by_key(|t| (t - value).abs())
        .unwrap_or(value)
}

/// The free place nearest to (`x`, `y`) where item `index` touches another
/// computer along at least a tenth of the shorter of the two edges, aligned
/// with what it comes within `tolerance` of.
fn snap(items: &[Item], index: usize, x: i32, y: i32, tolerance: f64) -> (i32, i32) {
    let me = &items[index].machine;
    let (w, h) = (me.width, me.height);
    let others: Vec<&Machine> = items
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != index)
        .map(|(_, i)| &i.machine)
        .collect();
    if others.is_empty() {
        return (x, y);
    }
    let mut best: Option<((i32, i32), f64)> = None;
    for o in &others {
        let min_v = (h.min(o.height) / 10).max(1);
        let min_h = (w.min(o.width) / 10).max(1);
        let (lo_y, hi_y) = (o.y - h + min_v, o.y + o.height - min_v);
        let (lo_x, hi_x) = (o.x - w + min_h, o.x + o.width - min_h);
        let mut ty = vec![o.y, o.y + o.height - h, o.y + (o.height - h) / 2];
        let mut tx = vec![o.x, o.x + o.width - w, o.x + (o.width - w) / 2];
        for m in &o.monitors {
            ty.extend([o.y + m.y, o.y + m.y + m.height - h]);
            tx.extend([o.x + m.x, o.x + m.x + m.width - w]);
        }
        for m in &me.monitors {
            ty.extend([o.y - m.y, o.y + o.height - m.y - m.height]);
            tx.extend([o.x - m.x, o.x + o.width - m.x - m.width]);
        }
        let cy = align_to(y.clamp(lo_y, hi_y), &ty, tolerance).clamp(lo_y, hi_y);
        let cx = align_to(x.clamp(lo_x, hi_x), &tx, tolerance).clamp(lo_x, hi_x);
        for (px, py) in [
            (o.x + o.width, cy),
            (o.x - w, cy),
            (cx, o.y + o.height),
            (cx, o.y - h),
        ] {
            if others.iter().any(|a| overlaps(a, px, py, w, h)) {
                continue;
            }
            let d = (((px - x) as f64).powi(2) + ((py - y) as f64).powi(2)).sqrt();
            if best.is_none_or(|(_, b)| d < b) {
                best = Some(((px, py), d));
            }
        }
    }
    best.map_or((x, y), |(p, _)| p)
}

/// a place for item `index` at `pos` of this computer (item 0), free
fn place_at(items: &[Item], index: usize, pos: Position) -> (i32, i32) {
    let own = &items[0].machine;
    let me = &items[index].machine;
    let (x, y) = match pos {
        Position::Right => (own.x + own.width, own.y + (own.height - me.height) / 2),
        Position::Left => (own.x - me.width, own.y + (own.height - me.height) / 2),
        Position::Bottom => (own.x + (own.width - me.width) / 2, own.y + own.height),
        Position::Top => (own.x + (own.width - me.width) / 2, own.y - me.height),
    };
    snap(items, index, x, y, 0.0)
}

/// the name of a system as people know it, none when unknown
fn system(os: &str) -> Option<&str> {
    match os {
        "" => None,
        "linux" => Some("Linux"),
        "windows" => Some("Windows"),
        "macos" => Some("macOS"),
        "freebsd" => Some("FreeBSD"),
        other => Some(other),
    }
}

fn pair(a: &str, b: &str) -> [String; 2] {
    if a <= b {
        [a.to_owned(), b.to_owned()]
    } else {
        [b.to_owned(), a.to_owned()]
    }
}

/// what is called when the user selects a computer
type OnSelected = dyn Fn(Option<ClientHandle>);

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct ArrangementArea {
        pub(super) model: RefCell<Model>,
        /// called whenever the arrangement or the selection changes
        pub(super) changed: RefCell<Option<Box<dyn Fn()>>>,
        /// called when the user selects a computer
        pub(super) selected: RefCell<Option<Box<OnSelected>>>,
        pub(super) last_size: Cell<(f64, f64)>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ArrangementArea {
        const NAME: &'static str = "RustInputArrangementArea";
        type Type = super::ArrangementArea;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for ArrangementArea {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_focusable(true);
            obj.set_hexpand(true);
            obj.set_height_request(300);
            obj.set_cursor_from_name(Some("grab"));
            obj.update_property(&[gtk::accessible::Property::Label(
                "Arrangement of the computers: drag a computer next to another one, or select \
                 one and move it with the arrow keys",
            )]);
            obj.setup_controllers();
        }
    }

    impl WidgetImpl for ArrangementArea {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            self.obj().draw(snapshot);
        }
    }
}

glib::wrapper! {
    pub struct ArrangementArea(ObjectSubclass<imp::ArrangementArea>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for ArrangementArea {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl ArrangementArea {
    pub(crate) fn connect_changed(&self, f: impl Fn() + 'static) {
        self.imp().changed.replace(Some(Box::new(f)));
    }

    /// called with the connection of a computer the user selects in the
    /// arrangement (none for this computer)
    pub(crate) fn connect_selected(&self, f: impl Fn(Option<ClientHandle>) + 'static) {
        self.imp().selected.replace(Some(Box::new(f)));
    }

    fn selected_now(&self) {
        let handle = {
            let model = self.model();
            model
                .selected
                .and_then(|s| model.items.get(s))
                .and_then(|i| i.handle)
        };
        if let Some(f) = self.imp().selected.borrow().as_ref() {
            f(handle);
        }
    }

    /// selects the computer of the connection `handle`, from its row
    pub(crate) fn select(&self, handle: ClientHandle) {
        let mut model = self.model_mut();
        let index = model.items.iter().position(|i| i.handle == Some(handle));
        if index.is_none() || model.selected == index {
            return;
        }
        model.selected = index;
        drop(model);
        self.changed();
    }

    /// whether the connection is switched on and answers
    pub(crate) fn set_state(&self, handle: ClientHandle, active: bool, alive: bool) {
        let mut model = self.model_mut();
        if let Some(item) = model.items.iter_mut().find(|i| i.handle == Some(handle)) {
            item.active = active;
            item.alive = alive;
        }
        drop(model);
        self.queue_draw();
    }

    /// the computer the mouse of this one is on, this one when none
    pub(crate) fn set_pointer_at(&self, handle: Option<ClientHandle>) {
        self.model_mut().pointer_at = handle;
        self.queue_draw();
    }

    fn changed(&self) {
        self.queue_draw();
        if let Some(f) = self.imp().changed.borrow().as_ref() {
            f();
        }
    }

    fn model(&self) -> std::cell::Ref<'_, Model> {
        self.imp().model.borrow()
    }

    fn model_mut(&self) -> std::cell::RefMut<'_, Model> {
        self.imp().model.borrow_mut()
    }

    /// Shows what the service says. Places a user moved but did not apply
    /// yet stay; computers it does not know go next to this one, at the
    /// edge they are at now.
    pub(crate) fn set_view(&self, view: &ArrangementView) {
        let dirty = self.is_dirty();
        let mut model = self.model_mut();
        let kept: Vec<(String, i32, i32)> = model
            .items
            .iter()
            .map(|i| (i.machine.id.clone(), i.machine.x, i.machine.y))
            .collect();
        let selected = model
            .selected
            .and_then(|s| model.items.get(s))
            .map(|i| i.machine.id.clone());
        let mut own = view.own.clone();
        if own.width <= 0 || own.height <= 0 {
            (own.width, own.height) = GUESSED;
        }
        let mut items = vec![Item {
            machine: own,
            own: true,
            handle: None,
            described: true,
            active: true,
            alive: true,
        }];
        let in_layout = |id: &str| view.layout.machine(id).is_some();
        let mut unplaced = Vec::new();
        for peer in &view.peers {
            let mut machine = peer.machine.clone();
            if machine.width <= 0 || machine.height <= 0 {
                (machine.width, machine.height) = GUESSED;
            }
            if !in_layout(&machine.id) {
                unplaced.push((items.len(), peer.pos, peer.active));
            }
            let alive = peer.alive;
            items.push(Item {
                machine,
                own: false,
                handle: Some(peer.handle),
                described: peer.described,
                active: peer.active,
                alive,
            });
        }
        if dirty {
            for item in &mut items {
                if let Some((_, x, y)) = kept.iter().find(|(id, ..)| *id == item.machine.id) {
                    (item.machine.x, item.machine.y) = (*x, *y);
                }
            }
        }
        let mut whole = if dirty {
            model.whole.clone()
        } else {
            view.layout.whole_edges.clone()
        };
        // without an arrangement, the connections as they are: whole edges,
        // as RustInput behaved without one
        let proposed = view.layout.machines.is_empty() && !view.peers.is_empty();
        for (index, pos, active) in unplaced {
            if dirty && kept.iter().any(|(id, ..)| *id == items[index].machine.id) {
                continue;
            }
            let (x, y) = place_at(&items, index, pos);
            (items[index].machine.x, items[index].machine.y) = (x, y);
            if active {
                let p = pair(&items[0].machine.id, &items[index].machine.id);
                if !whole.contains(&p) {
                    whole.push(p);
                }
            }
        }
        model.items = items;
        model.whole = whole;
        model.revision = view.layout.revision;
        model.selected =
            selected.and_then(|id| model.items.iter().position(|i| i.machine.id == id));
        model.proposed = proposed;
        if !dirty {
            // a proposal is not applied yet
            model.applied = if proposed {
                Snapshot::default()
            } else {
                model.snapshot()
            };
        }
        drop(model);
        self.changed();
    }

    pub(crate) fn layout(&self) -> Layout {
        self.model().layout()
    }

    pub(crate) fn is_dirty(&self) -> bool {
        let model = self.model();
        !model.items.is_empty() && model.snapshot() != model.applied
    }

    /// made from the connections as they are, not applied yet
    pub(crate) fn is_proposal(&self) -> bool {
        self.model().proposed
    }

    /// what was applied is what is shown now
    pub(crate) fn mark_applied(&self) {
        let mut model = self.model_mut();
        model.applied = model.snapshot();
        model.proposed = false;
        drop(model);
        self.changed();
    }

    /// back to what was applied
    pub(crate) fn undo(&self) {
        let mut model = self.model_mut();
        let applied = model.applied.clone();
        if applied.places.is_empty() {
            return;
        }
        for item in &mut model.items {
            if let Some((_, x, y)) = applied
                .places
                .iter()
                .find(|(id, ..)| *id == item.machine.id)
            {
                (item.machine.x, item.machine.y) = (*x, *y);
            }
        }
        model.whole = applied.whole;
        drop(model);
        self.changed();
    }

    /// the names of the computers the pointer can not reach from this one
    pub(crate) fn unreachable(&self) -> Vec<String> {
        let model = self.model();
        let layout = model.layout();
        let Some(own) = model.items.first() else {
            return Vec::new();
        };
        layout
            .unreachable_from(&own.machine.id)
            .into_iter()
            .filter_map(|id| layout.machine(&id).map(|m| m.name.clone()))
            .collect()
    }

    /// the names of the computers that did not describe themselves
    pub(crate) fn guessed(&self) -> Vec<String> {
        self.model()
            .items
            .iter()
            .filter(|i| !i.described)
            .map(|i| i.machine.name.clone())
            .collect()
    }

    pub(crate) fn selected_name(&self) -> Option<String> {
        let model = self.model();
        model
            .selected
            .and_then(|s| model.items.get(s))
            .map(|i| i.machine.name.clone())
    }

    /// the computers next to the selected one
    pub(crate) fn selected_links(&self) -> Vec<LinkInfo> {
        let model = self.model();
        let Some(item) = model.selected.and_then(|s| model.items.get(s)) else {
            return Vec::new();
        };
        let layout = model.layout();
        let me = &item.machine.id;
        let neighbors = layout.neighbors(me);
        // a pair is whole only when neither edge is shared: one neighbor at
        // each of the two edges
        let count =
            |id: &str, pos: Position| layout.neighbors(id).iter().filter(|n| n.pos == pos).count();
        neighbors
            .iter()
            .map(|n| {
                let other = layout.machine(&n.id);
                let whole = layout.whole(me, &n.id);
                let can_be_whole = count(me, n.pos) == 1 && count(&n.id, n.pos.opposite()) == 1;
                let part = if n.link.own.is_full() && n.link.peer.is_full() {
                    tr("The whole edge").to_owned()
                } else {
                    let from = format!("{:.0}", n.link.own.lo * 100.0);
                    let to = format!("{:.0}", n.link.own.hi * 100.0);
                    let values: [(&str, &dyn std::fmt::Display); 2] =
                        [("from", &from), ("to", &to)];
                    match n.pos {
                        Position::Left | Position::Right => {
                            trf("From {from}% to {to}% of the height", &values)
                        }
                        Position::Top | Position::Bottom => {
                            trf("From {from}% to {to}% of the width", &values)
                        }
                    }
                };
                LinkInfo {
                    other: n.id.clone(),
                    other_name: other.map(|m| m.name.clone()).unwrap_or_default(),
                    pos: n.pos,
                    part,
                    whole,
                    can_be_whole,
                }
            })
            .collect()
    }

    /// whether the selected computer and `other` meet over the whole of
    /// their edges, in proportion
    pub(crate) fn set_whole(&self, other: &str, whole: bool) {
        let mut model = self.model_mut();
        let Some(me) = model
            .selected
            .and_then(|s| model.items.get(s))
            .map(|i| i.machine.id.clone())
        else {
            return;
        };
        let p = pair(&me, other);
        model.whole.retain(|w| *w != p);
        if whole {
            model.whole.push(p);
        }
        drop(model);
        self.changed();
    }

    fn setup_controllers(&self) {
        let drag = gtk::GestureDrag::new();
        drag.connect_drag_begin(glib::clone!(
            #[weak(rename_to = area)]
            self,
            move |_, x, y| area.drag_begin(x, y)
        ));
        drag.connect_drag_update(glib::clone!(
            #[weak(rename_to = area)]
            self,
            move |_, dx, dy| area.drag_update(dx, dy)
        ));
        drag.connect_drag_end(glib::clone!(
            #[weak(rename_to = area)]
            self,
            move |_, _, _| area.drag_end()
        ));
        self.add_controller(drag);

        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(glib::clone!(
            #[weak(rename_to = area)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, key, _, _| area.key(key)
        ));
        self.add_controller(keys);
    }

    fn size(&self) -> (f64, f64) {
        (self.width() as f64, self.height() as f64)
    }

    fn drag_begin(&self, x: f64, y: f64) {
        self.grab_focus();
        let (w, h) = self.size();
        let mut model = self.model_mut();
        let t = model.transform(w, h);
        let (lx, ly) = t.to_layout(x, y);
        model.selected = model.hit(lx, ly);
        if let Some(index) = model.selected {
            let m = &model.items[index].machine;
            model.drag = Some(Drag {
                index,
                offset: (lx - m.x as f64, ly - m.y as f64),
                start: (x, y),
                moved: false,
            });
            model.frozen = Some(t);
            drop(model);
            self.set_cursor_from_name(Some("grabbing"));
        } else {
            drop(model);
        }
        self.changed();
        self.selected_now();
    }

    fn drag_update(&self, dx: f64, dy: f64) {
        let mut model = self.model_mut();
        let (Some(mut drag), Some(t)) = (model.drag, model.frozen) else {
            return;
        };
        if !drag.moved && dx.hypot(dy) < 3.0 {
            return;
        }
        drag.moved = true;
        model.drag = Some(drag);
        let (lx, ly) = t.to_layout(drag.start.0 + dx, drag.start.1 + dy);
        let m = &mut model.items[drag.index].machine;
        m.x = (lx - drag.offset.0).round() as i32;
        m.y = (ly - drag.offset.1).round() as i32;
        drop(model);
        self.queue_draw();
    }

    fn drag_end(&self) {
        let mut model = self.model_mut();
        let drag = model.drag.take();
        let t = model.frozen.take();
        if let (Some(drag), Some(t)) = (drag, t) {
            if drag.moved {
                let m = &model.items[drag.index].machine;
                let (x, y) = snap(&model.items, drag.index, m.x, m.y, ALIGN / t.scale);
                let m = &mut model.items[drag.index].machine;
                (m.x, m.y) = (x, y);
            }
        }
        drop(model);
        self.set_cursor_from_name(Some("grab"));
        self.changed();
    }

    fn key(&self, key: gdk::Key) -> glib::Propagation {
        let step = match key {
            gdk::Key::Left => (-1.0, 0.0),
            gdk::Key::Right => (1.0, 0.0),
            gdk::Key::Up => (0.0, -1.0),
            gdk::Key::Down => (0.0, 1.0),
            gdk::Key::Tab | gdk::Key::ISO_Left_Tab => return glib::Propagation::Proceed,
            gdk::Key::space => {
                // select the next computer
                let mut model = self.model_mut();
                let count = model.items.len();
                if count > 0 {
                    model.selected = Some(model.selected.map_or(0, |s| (s + 1) % count));
                }
                drop(model);
                self.changed();
                self.selected_now();
                return glib::Propagation::Stop;
            }
            _ => return glib::Propagation::Proceed,
        };
        let mut model = self.model_mut();
        let Some(index) = model.selected else {
            return glib::Propagation::Proceed;
        };
        let m = &model.items[index].machine;
        let along = (m.width.max(m.height) as f64 * KEY_STEP).max(1.0);
        let (x, y) = (m.x + (step.0 * along) as i32, m.y + (step.1 * along) as i32);
        let (x, y) = snap(&model.items, index, x, y, 0.0);
        let m = &mut model.items[index].machine;
        (m.x, m.y) = (x, y);
        drop(model);
        self.changed();
        glib::Propagation::Stop
    }

    fn color(&self, name: &str, fallback: gdk::RGBA) -> gdk::RGBA {
        #[allow(deprecated)]
        self.style_context().lookup_color(name).unwrap_or(fallback)
    }

    fn draw(&self, snapshot: &gtk::Snapshot) {
        let (w, h) = self.size();
        self.imp().last_size.set((w, h));
        let model = self.model();
        if model.items.is_empty() {
            return;
        }
        let t = model.transform(w, h);
        #[allow(deprecated)]
        let fg = self.style_context().color();
        let accent = self.color("accent_bg_color", gdk::RGBA::new(0.21, 0.52, 0.89, 1.0));
        let card = self.color("card_bg_color", gdk::RGBA::new(1.0, 1.0, 1.0, 0.08));
        let with_alpha = |c: gdk::RGBA, a: f32| gdk::RGBA::new(c.red(), c.green(), c.blue(), a);
        let border = with_alpha(fg, 0.25);
        let monitor = with_alpha(fg, 0.10);
        let dim = with_alpha(fg, 0.6);
        let warning = self.color("warning_color", gdk::RGBA::new(0.8, 0.5, 0.0, 1.0));
        let success = self.color("success_color", gdk::RGBA::new(0.15, 0.64, 0.41, 1.0));

        let layout = model.layout();
        let own_id = model.items[0].machine.id.clone();
        let unreachable = layout.unreachable_from(&own_id);

        for (index, item) in model.items.iter().enumerate() {
            let m = &item.machine;
            let (x, y) = t.to_screen(m.x as f64, m.y as f64);
            let (mw, mh) = (m.width as f64 * t.scale, m.height as f64 * t.scale);
            let rect = graphene::Rect::new(x as f32, y as f32, mw as f32, mh as f32);
            let rounded = gsk::RoundedRect::from_rect(rect, 6.0);
            snapshot.push_rounded_clip(&rounded);
            snapshot.append_color(&card, &rect);
            // the monitors
            for mon in &m.monitors {
                let r = graphene::Rect::new(
                    (x + mon.x as f64 * t.scale + 3.0) as f32,
                    (y + mon.y as f64 * t.scale + 3.0) as f32,
                    (mon.width as f64 * t.scale - 6.0).max(0.0) as f32,
                    (mon.height as f64 * t.scale - 6.0).max(0.0) as f32,
                );
                snapshot.push_rounded_clip(&gsk::RoundedRect::from_rect(r, 3.0));
                snapshot.append_color(&monitor, &r);
                snapshot.pop();
            }
            snapshot.pop();
            let selected = model.selected == Some(index);
            let (width, color) = if selected {
                (2.5, accent)
            } else if unreachable.contains(&m.id) {
                (2.0, warning)
            } else {
                (1.0, border)
            };
            snapshot.append_border(&rounded, &[width; 4], &[color; 4]);

            // the name, the system and the size, and how it is
            let mut lines = vec![format!("<b>{}</b>", glib::markup_escape_text(&m.name))];
            let size = if item.described {
                format!("{} × {}", m.width, m.height)
            } else {
                tr("size unknown").to_owned()
            };
            lines.push(match system(&m.os) {
                Some(os) => format!("{os} · {size}"),
                None => size,
            });
            lines.push(
                if item.own {
                    tr("this computer")
                } else if unreachable.contains(&m.id) {
                    tr("not reached")
                } else if !item.active {
                    tr("switched off")
                } else if item.alive {
                    tr("connected")
                } else {
                    tr("not connected")
                }
                .into(),
            );
            // Right to left (Arabic) the inside of a box is mirrored: its
            // lines on the right, the dot on the left, the pill on the
            // right. The boxes themselves stay where the computers are.
            let rtl = self.direction() == gtk::TextDirection::Rtl;
            // the dot of the connection takes room at the top
            let dotted = !item.own && item.active;
            let dot_room = if dotted { 14.0 } else { 0.0 };
            let text = self.create_pango_layout(None);
            // all lines in the direction of the language, names in Latin
            // letters too, so they line up
            text.set_auto_dir(false);
            if rtl {
                text.set_alignment(pango::Alignment::Right);
            }
            text.set_markup(&lines.join("\n"));
            text.set_width(((mw - 16.0 - dot_room).max(10.0) * pango::SCALE as f64) as i32);
            text.set_ellipsize(pango::EllipsizeMode::End);
            let text_x = if rtl { x + 10.0 + dot_room } else { x + 10.0 };
            snapshot.save();
            snapshot.push_clip(&rect);
            snapshot.translate(&graphene::Point::new(text_x as f32, (y + 8.0) as f32));
            snapshot.append_layout(&text, &if item.own { fg } else { dim });
            snapshot.pop();
            snapshot.restore();

            // the computer the mouse is on
            let here = if item.own {
                model.pointer_at.is_none()
            } else {
                item.handle.is_some() && model.pointer_at == item.handle
            };
            if here && mh > 60.0 {
                let pill = self.create_pango_layout(Some(tr("mouse here")));
                let (pw, ph) = pill.pixel_size();
                let pill_x = if rtl {
                    x + mw - (pw + 16) as f64 - 8.0
                } else {
                    x + 8.0
                };
                let r = graphene::Rect::new(
                    pill_x as f32,
                    (y + mh - ph as f64 - 14.0) as f32,
                    (pw + 16) as f32,
                    (ph + 6) as f32,
                );
                snapshot.push_rounded_clip(&gsk::RoundedRect::from_rect(r, (ph + 6) as f32 / 2.0));
                snapshot.append_color(&accent, &r);
                snapshot.pop();
                snapshot.save();
                snapshot.translate(&graphene::Point::new(r.x() + 8.0, r.y() + 3.0));
                snapshot.append_layout(&pill, &gdk::RGBA::WHITE);
                snapshot.restore();
            }

            // whether its connection answers: a dot at the top right
            if dotted {
                let dot_x = if rtl { x + 8.0 } else { x + mw - 16.0 };
                let dot = graphene::Rect::new(dot_x as f32, (y + 10.0) as f32, 8.0, 8.0);
                let color = if item.alive { success } else { dim };
                snapshot.push_rounded_clip(&gsk::RoundedRect::from_rect(dot, 4.0));
                snapshot.append_color(&color, &dot);
                snapshot.pop();
            }
        }

        // where the computers meet
        let own_neighbors_seen = &mut Vec::<(String, String)>::new();
        for item in &model.items {
            for n in layout.neighbors(&item.machine.id) {
                let key = if item.machine.id < n.id {
                    (item.machine.id.clone(), n.id.clone())
                } else {
                    (n.id.clone(), item.machine.id.clone())
                };
                if own_neighbors_seen.contains(&key) {
                    continue;
                }
                own_neighbors_seen.push(key);
                let (Some(a), Some(b)) = (Some(&item.machine), layout.machine(&n.id)) else {
                    continue;
                };
                self.draw_link(snapshot, t, a, b, n.pos, accent, layout.whole(&a.id, &b.id));
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_link(
        &self,
        snapshot: &gtk::Snapshot,
        t: Transform,
        a: &Machine,
        b: &Machine,
        pos: Position,
        accent: gdk::RGBA,
        whole: bool,
    ) {
        let bar = |from: f64, to: f64, at: f64, vertical: bool, alpha: f32| {
            if to - from <= 0.0 {
                return;
            }
            let rect = if vertical {
                let (x, y0) = t.to_screen(at, from);
                let (_, y1) = t.to_screen(at, to);
                graphene::Rect::new((x - 3.0) as f32, y0 as f32, 6.0, (y1 - y0) as f32)
            } else {
                let (x0, y) = t.to_screen(from, at);
                let (x1, _) = t.to_screen(to, at);
                graphene::Rect::new(x0 as f32, (y - 3.0) as f32, (x1 - x0) as f32, 6.0)
            };
            let color = gdk::RGBA::new(accent.red(), accent.green(), accent.blue(), alpha);
            snapshot.append_color(&color, &rect);
        };
        let vertical = matches!(pos, Position::Left | Position::Right);
        let at = match pos {
            Position::Right => (a.x + a.width) as f64,
            Position::Left => a.x as f64,
            Position::Bottom => (a.y + a.height) as f64,
            Position::Top => a.y as f64,
        };
        let (a0, a1, b0, b1) = if vertical {
            (a.y, a.y + a.height, b.y, b.y + b.height)
        } else {
            (a.x, a.x + a.width, b.x, b.x + b.width)
        };
        let (from, to) = (a0.max(b0) as f64, a1.min(b1) as f64);
        bar(from, to, at, vertical, 1.0);
        if whole {
            let (lo, hi) = (a0.min(b0) as f64, a1.max(b1) as f64);
            bar(lo, from, at, vertical, 0.4);
            bar(to, hi, at, vertical, 0.4);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, x: i32, y: i32, w: i32, h: i32) -> Item {
        Item {
            machine: Machine {
                id: id.into(),
                name: id.into(),
                x,
                y,
                width: w,
                height: h,
                monitors: Vec::new(),
                os: String::new(),
                hostname: None,
                ips: Vec::new(),
                port: 4242,
            },
            own: false,
            handle: None,
            described: true,
            active: true,
            alive: true,
        }
    }

    #[test]
    fn dropped_near_an_edge_it_sticks_to_it() {
        let items = vec![
            item("linux", 0, 0, 1920, 2160),
            item("win", 2100, 1200, 1920, 1080),
        ];
        // dropped a little to the right, near the lower monitor
        assert_eq!(snap(&items, 1, 2100, 1200, 150.0), (1920, 1080));
        // far from any alignment it keeps its height
        assert_eq!(snap(&items, 1, 2000, 600, 10.0), (1920, 600));
    }

    #[test]
    fn it_does_not_go_on_top_of_another_one() {
        let items = vec![
            item("linux", 0, 0, 1920, 2160),
            item("win", 1920, 0, 1920, 1080),
            item("mac", 1920, 100, 1440, 900),
        ];
        let (x, y) = snap(&items, 2, 1920, 100, 0.0);
        assert!(!overlaps(&items[1].machine, x, y, 1440, 900));
        assert!(!overlaps(&items[0].machine, x, y, 1440, 900));
    }

    #[test]
    fn next_to_this_computer_at_its_edge() {
        let items = vec![
            item("linux", 0, 0, 1920, 2160),
            item("win", 0, 0, 1920, 1080),
        ];
        assert_eq!(place_at(&items, 1, Position::Right), (1920, 540));
        assert_eq!(place_at(&items, 1, Position::Top), (0, -1080));
    }
}
