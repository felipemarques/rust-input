//! X11 input capture.
//!
//! While idle the backend listens to XInput2 raw motion events on the root
//! window. Those are delivered no matter which window is focused, so they tell
//! us when the pointer is moving; the pointer position is then queried and
//! compared with the outer edges of the monitors. Once the pointer reaches an
//! edge that has a client, pointer and keyboard are grabbed, the cursor is
//! hidden and every input event is forwarded until the capture is released.
//!
//! Everything runs on the task polling the stream: the file descriptor of the
//! X connection is registered with the tokio reactor, so there is no helper
//! thread and no periodic polling. Should the process die, the X server drops
//! the grabs on its own.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    os::fd::{AsRawFd, RawFd},
    pin::Pin,
    task::{Context, Poll, Waker},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use futures_core::Stream;
use tokio::io::{Interest, unix::AsyncFd};
use x11rb::{
    CURRENT_TIME, NONE,
    connection::{Connection, RequestConnection},
    errors::ReplyOrIdError,
    protocol::{
        Event as XEvent,
        randr::{self, ConnectionExt as _},
        xinput::{self, ConnectionExt as _},
        xkb::{self, ConnectionExt as _},
        xproto::{self, ConnectionExt as _},
    },
    rust_connection::RustConnection,
};

use input_event::{
    BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event, KeyboardEvent, PointerEvent,
    edge::{Area, Edge, Span},
};

use super::{Capture, CaptureError, CaptureEvent, Position, error::X11InputCaptureCreationError};

/// order in which the edges are checked when the pointer sits in a corner
const POSITIONS: [Position; 4] = [
    Position::Left,
    Position::Right,
    Position::Top,
    Position::Bottom,
];

/// distance from the parking spot at which the hidden pointer is warped back
const WARP_DISTANCE: i32 = 100;

/// motion events tolerated without seeing the echo of a warp before the warp
/// is sent again
const WARP_RETRY_EVENTS: u32 = 64;

/// time during which no new capture starts after one was released, so the
/// pointer does not bounce straight back to the client it just returned from
const REARM_DELAY: Duration = Duration::from_millis(200);

/// buttons 1 to 5 of the core pointer state
const ANY_BUTTON_MASK: u16 = 0x1f00;

/// id addressing all master devices in XInput2 requests
const XI_ALL_MASTER_DEVICES: u16 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Rect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl Rect {
    fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }

    fn center(&self) -> (i32, i32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }
}

/// the rectangle spanned by all monitors
fn desktop(monitors: &[Rect]) -> Option<Area> {
    Area::bounding(monitors.iter().map(|m| Area {
        x: m.x,
        y: m.y,
        width: m.w,
        height: m.h,
    }))
}

fn index(pos: Position) -> usize {
    match pos {
        Position::Left => 0,
        Position::Right => 1,
        Position::Top => 2,
        Position::Bottom => 3,
    }
}

/// whether the pixel `(x, y)` lies on the `pos` edge of its monitor with no
/// other monitor continuing on the far side of that edge
fn at_outer_edge(monitors: &[Rect], x: i32, y: i32, pos: Position) -> bool {
    let Some(m) = monitors.iter().find(|m| m.contains(x, y)) else {
        return false;
    };
    let (on_edge, beyond) = match pos {
        Position::Left => (x == m.x, (x - 1, y)),
        Position::Right => (x == m.x + m.w - 1, (x + 1, y)),
        Position::Top => (y == m.y, (x, y - 1)),
        Position::Bottom => (y == m.y + m.h - 1, (x, y + 1)),
    };
    on_edge && !monitors.iter().any(|m| m.contains(beyond.0, beyond.1))
}

/// whether a relative motion of `(dx, dy)` moves towards `pos`
fn pushes_outward(pos: Position, dx: f64, dy: f64) -> bool {
    match pos {
        Position::Left => dx < 0.0,
        Position::Right => dx > 0.0,
        Position::Top => dy < 0.0,
        Position::Bottom => dy > 0.0,
    }
}

fn fp3232(value: &xinput::Fp3232) -> f64 {
    value.integral as f64 + value.frac as f64 / (1u64 << 32) as f64
}

/// horizontal and vertical component (valuators 0 and 1) of a raw motion.
///
/// Only the valuators flagged in the mask are transmitted, in ascending order.
fn raw_delta(valuator_mask: &[u32], values: &[xinput::Fp3232]) -> (f64, f64) {
    let mut values = values.iter();
    let (mut dx, mut dy) = (0.0, 0.0);
    for (word, bits) in valuator_mask.iter().enumerate() {
        for bit in 0..32 {
            if bits & (1 << bit) == 0 {
                continue;
            }
            let Some(value) = values.next() else {
                return (dx, dy);
            };
            match word * 32 + bit {
                0 => dx = fp3232(value),
                1 => dy = fp3232(value),
                _ => {}
            }
        }
    }
    (dx, dy)
}

/// translates a core button press / release.
///
/// Buttons 4 to 7 are the scroll wheel: X reports each notch as a press
/// followed by a release, only the press is forwarded.
fn button_event(button: u8, pressed: bool, time: u32) -> Option<PointerEvent> {
    let state = pressed as u32;
    let button_code = match button {
        1 => BTN_LEFT,
        2 => BTN_MIDDLE,
        3 => BTN_RIGHT,
        8 => BTN_BACK,
        9 => BTN_FORWARD,
        4..=7 if pressed => {
            return Some(PointerEvent::AxisDiscrete120 {
                axis: if button <= 5 { 0 } else { 1 },
                // 4 and 6 scroll up / left, 5 and 7 down / right
                value: if matches!(button, 4 | 6) { -120 } else { 120 },
            });
        }
        _ => return None,
    };
    Some(PointerEvent::Button {
        time,
        button: button_code,
        state,
    })
}

/// Turns the absolute positions reported while the pointer is grabbed into
/// relative motion.
///
/// The hidden pointer is parked at `center` and warped back there whenever it
/// strays too far, so it never runs into a screen edge. Deltas are measured
/// against the previous position rather than against the parking spot, which
/// keeps them correct for the events still in flight when a warp is requested.
#[derive(Clone, Copy, Debug)]
struct MotionTracker {
    center: (i32, i32),
    last: (i32, i32),
    /// a warp was requested and its motion event has not arrived yet
    warping: bool,
    /// motion events seen since the warp was requested
    since_warp: u32,
}

impl MotionTracker {
    /// tracker for a pointer currently at `at`, with a warp to `center` pending
    fn new(at: (i32, i32), center: (i32, i32)) -> Self {
        Self {
            center,
            last: at,
            warping: true,
            since_warp: 0,
        }
    }

    /// feeds a pointer position, returns the motion to forward (if any) and
    /// whether the pointer has to be warped back to the parking spot
    fn motion(&mut self, at: (i32, i32)) -> (Option<(i32, i32)>, bool) {
        if self.warping && at == self.center {
            // the echo of our own warp
            self.warping = false;
            self.last = at;
            return (None, false);
        }
        let delta = (at.0 - self.last.0, at.1 - self.last.1);
        self.last = at;
        let delta = (delta != (0, 0)).then_some(delta);

        let far = (at.0 - self.center.0).abs() > WARP_DISTANCE
            || (at.1 - self.center.1).abs() > WARP_DISTANCE;
        if self.warping {
            self.since_warp += 1;
        }
        let warp = far && (!self.warping || self.since_warp > WARP_RETRY_EVENTS);
        if warp {
            self.warping = true;
            self.since_warp = 0;
        }
        (delta, warp)
    }
}

struct Grab {
    pos: Position,
    /// where the pointer was when the capture started, it returns there
    entry: (i16, i16),
    tracker: MotionTracker,
}

pub struct X11InputCapture {
    /// readiness of the connection socket, declared before `conn` so it is
    /// deregistered before the socket is closed
    fd: AsyncFd<RawFd>,
    conn: RustConnection,
    root: xproto::Window,
    /// invisible cursor shown while the pointer is grabbed
    blank_cursor: xproto::Cursor,
    has_xkb: bool,
    has_randr: bool,
    monitors: Vec<Rect>,
    /// edges with at least one client
    zones: HashSet<Position>,
    /// the parts of each edge that lead to a client, all of it if none
    spans: HashMap<Position, Vec<Span>>,
    grab: Option<Grab>,
    /// relative motion gathered since the edges were last checked
    motion: Option<(f64, f64)>,
    /// edges the pointer was touching at the last check, indexed by [`index`]
    touching: [bool; 4],
    /// where along its edge the pointer was when the current capture began
    entry_fraction: Option<f64>,
    released_at: Option<Instant>,
    /// keycodes currently held down, used to drop auto-repeated presses
    pressed: HashSet<u8>,
    /// modifier state last forwarded
    modifiers: Option<(u32, u32, u32, u32)>,
    pending: VecDeque<(Position, CaptureEvent)>,
    needs_flush: bool,
    waker: Option<Waker>,
}

impl X11InputCapture {
    pub fn new() -> Result<Self, X11InputCaptureCreationError> {
        if std::env::var_os("WAYLAND_DISPLAY").is_some_and(|d| !d.is_empty()) {
            // XWayland only sees the input of X11 windows
            return Err(X11InputCaptureCreationError::Wayland);
        }

        let (conn, screen) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen].root;

        if conn
            .extension_information(xinput::X11_EXTENSION_NAME)?
            .is_none()
        {
            return Err(X11InputCaptureCreationError::MissingExtension("XInput"));
        }
        let version = conn.xinput_xi_query_version(2, 2)?.reply()?;
        if version.major_version < 2 {
            return Err(X11InputCaptureCreationError::MissingExtension("XInput 2"));
        }
        conn.xinput_xi_select_events(
            root,
            &[xinput::EventMask {
                deviceid: XI_ALL_MASTER_DEVICES,
                mask: vec![xinput::XIEventMask::RAW_MOTION],
            }],
        )?
        .check()?;

        // with detectable auto-repeat a held key repeats as press events only,
        // instead of release / press pairs that look like real key strokes
        let mut has_xkb = false;
        if conn
            .extension_information(xkb::X11_EXTENSION_NAME)?
            .is_some()
            && conn.xkb_use_extension(1, 0)?.reply()?.supported
        {
            has_xkb = true;
            let flag = xkb::PerClientFlag::DETECTABLE_AUTO_REPEAT;
            let none = xkb::BoolCtrl::from(0u32);
            if let Err(e) = conn
                .xkb_per_client_flags(xkb::ID::USE_CORE_KBD.into(), flag, flag, none, none, none)?
                .reply()
            {
                log::warn!("x11: could not enable detectable auto-repeat: {e}");
            }
        } else {
            log::warn!("x11: XKB is unavailable, modifier state will not be forwarded");
        }

        let mut has_randr = false;
        if conn
            .extension_information(randr::X11_EXTENSION_NAME)?
            .is_some()
        {
            let version = conn.randr_query_version(1, 5)?.reply()?;
            has_randr = (version.major_version, version.minor_version) >= (1, 5);
            let mask = randr::NotifyMask::SCREEN_CHANGE | randr::NotifyMask::CRTC_CHANGE;
            conn.randr_select_input(root, mask)?;
        }

        let blank_cursor = create_blank_cursor(&conn, root)?;
        conn.flush()?;

        // SAFETY: the socket belongs to `conn`, which is kept next to `fd` in
        // the struct and declared after it, so it is closed only once `fd` is
        // dropped (deregistered); a `RawFd` always gives the same number.
        let fd = unsafe {
            AsyncFd::register_with_interest(conn.stream().as_raw_fd(), Interest::READABLE)
        }
        .map_err(std::io::Error::from)?;

        let mut capture = Self {
            fd,
            conn,
            root,
            blank_cursor,
            has_xkb,
            has_randr,
            monitors: vec![],
            zones: HashSet::new(),
            spans: HashMap::new(),
            grab: None,
            motion: None,
            touching: [false; 4],
            entry_fraction: None,
            released_at: None,
            pressed: HashSet::new(),
            modifiers: None,
            pending: VecDeque::new(),
            needs_flush: false,
            waker: None,
        };
        capture.monitors = capture.query_monitors()?;
        log::debug!("x11: monitors: {:?}", capture.monitors);
        Ok(capture)
    }

    fn query_monitors(&self) -> Result<Vec<Rect>, x11rb::errors::ReplyError> {
        if self.has_randr {
            let monitors: Vec<Rect> = self
                .conn
                .randr_get_monitors(self.root, true)?
                .reply()?
                .monitors
                .iter()
                .map(|m| Rect {
                    x: m.x as i32,
                    y: m.y as i32,
                    w: m.width as i32,
                    h: m.height as i32,
                })
                .filter(|m| m.w > 0 && m.h > 0)
                .collect();
            if !monitors.is_empty() {
                return Ok(monitors);
            }
        }
        let root = self.conn.get_geometry(self.root)?.reply()?;
        Ok(vec![Rect {
            x: 0,
            y: 0,
            w: root.width as i32,
            h: root.height as i32,
        }])
    }

    /// wakes the task polling the stream, used after requests made outside of
    /// `poll_next`: waiting for their replies may have buffered events for
    /// which the socket will not signal readiness again
    fn wake(&mut self) {
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }

    fn handle_event(&mut self, event: XEvent) -> Result<(), CaptureError> {
        match event {
            XEvent::XinputRawMotion(ev) => {
                if self.grab.is_none() && !self.zones.is_empty() {
                    let (dx, dy) = raw_delta(&ev.valuator_mask, &ev.axisvalues);
                    let (x, y) = self.motion.unwrap_or_default();
                    self.motion = Some((x + dx, y + dy));
                }
            }
            XEvent::MotionNotify(ev) => self.handle_motion(ev)?,
            XEvent::ButtonPress(ev) => self.handle_button(ev.detail, true, ev.time),
            XEvent::ButtonRelease(ev) => self.handle_button(ev.detail, false, ev.time),
            XEvent::KeyPress(ev) => self.handle_key(ev.detail, true, ev.time)?,
            XEvent::KeyRelease(ev) => self.handle_key(ev.detail, false, ev.time)?,
            XEvent::RandrScreenChangeNotify(_) | XEvent::RandrNotify(_) => {
                self.monitors = self.query_monitors()?;
                log::debug!("x11: monitors changed: {:?}", self.monitors);
            }
            XEvent::Error(e) => log::debug!("x11: request failed: {e:?}"),
            _ => {}
        }
        Ok(())
    }

    fn handle_motion(&mut self, ev: xproto::MotionNotifyEvent) -> Result<(), CaptureError> {
        let Some(grab) = self.grab.as_mut() else {
            return Ok(());
        };
        let at = (ev.root_x as i32, ev.root_y as i32);
        let (delta, warp) = grab.tracker.motion(at);
        if let Some((dx, dy)) = delta {
            self.pending.push_back((
                grab.pos,
                CaptureEvent::Input(Event::Pointer(PointerEvent::Motion {
                    time: ev.time,
                    dx: dx as f64,
                    dy: dy as f64,
                })),
            ));
        }
        if warp {
            let (x, y) = grab.tracker.center;
            self.conn
                .warp_pointer(NONE, self.root, 0, 0, 0, 0, x as i16, y as i16)?;
            self.needs_flush = true;
        }
        Ok(())
    }

    fn handle_button(&mut self, button: u8, pressed: bool, time: u32) {
        let Some(grab) = self.grab.as_ref() else {
            return;
        };
        if let Some(event) = button_event(button, pressed, time) {
            self.pending
                .push_back((grab.pos, CaptureEvent::Input(Event::Pointer(event))));
        }
    }

    fn handle_key(&mut self, keycode: u8, pressed: bool, time: u32) -> Result<(), CaptureError> {
        let Some(pos) = self.grab.as_ref().map(|g| g.pos) else {
            return Ok(());
        };
        if pressed {
            if !self.pressed.insert(keycode) {
                // auto-repeat, the receiving side repeats on its own
                return Ok(());
            }
        } else {
            self.pressed.remove(&keycode);
        }
        self.pending.push_back((
            pos,
            CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                time,
                // X keycodes are the evdev codes shifted by 8
                key: (keycode as u32).saturating_sub(8),
                state: pressed as u8,
            })),
        ));
        self.forward_modifiers(pos)
    }

    /// queues the XKB modifier state if it differs from the one last sent
    fn forward_modifiers(&mut self, pos: Position) -> Result<(), CaptureError> {
        if !self.has_xkb {
            return Ok(());
        }
        let state = self
            .conn
            .xkb_get_state(xkb::ID::USE_CORE_KBD.into())?
            .reply()?;
        let modifiers = (
            u32::from(state.base_mods),
            u32::from(state.latched_mods),
            u32::from(state.locked_mods),
            u32::from(state.group),
        );
        if self.modifiers != Some(modifiers) {
            self.modifiers = Some(modifiers);
            let (depressed, latched, locked, group) = modifiers;
            self.pending.push_back((
                pos,
                CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Modifiers {
                    depressed,
                    latched,
                    locked,
                    group,
                })),
            ));
        }
        Ok(())
    }

    /// looks at where the pointer ended up after the motion gathered while
    /// idle and starts a capture if it reached an edge with a client
    fn check_edges(&mut self, motion: (f64, f64)) -> Result<(), CaptureError> {
        if self.grab.is_some() || self.zones.is_empty() {
            return Ok(());
        }
        let pointer = self.conn.query_pointer(self.root)?.reply()?;
        if !pointer.same_screen {
            return Ok(());
        }
        let (x, y) = (pointer.root_x as i32, pointer.root_y as i32);

        let mut hit = None;
        let mut touching = [false; 4];
        for pos in POSITIONS {
            let touches = self.zones.contains(&pos)
                && at_outer_edge(&self.monitors, x, y, pos)
                && self.on_span(pos, (x, y));
            touching[index(pos)] = touches;
            // either the pointer just arrived at the edge or it is being
            // pushed against it; sliding along the edge does not count
            let arrived = !self.touching[index(pos)];
            if touches && hit.is_none() && (arrived || pushes_outward(pos, motion.0, motion.1)) {
                hit = Some(pos);
            }
        }
        self.touching = touching;

        let Some(pos) = hit else {
            return Ok(());
        };
        if self.released_at.is_some_and(|t| t.elapsed() < REARM_DELAY) {
            return Ok(());
        }
        if u16::from(pointer.mask) & ANY_BUTTON_MASK != 0 {
            // a drag is in progress, its owner holds the pointer
            return Ok(());
        }
        self.begin(pos, (pointer.root_x, pointer.root_y))
    }

    /// whether `point` of the edge at `pos` leads to a client: elsewhere the
    /// edge is a wall
    fn on_span(&self, pos: Position, point: (i32, i32)) -> bool {
        let Some(spans) = self.spans.get(&pos).filter(|s| !s.is_empty()) else {
            return true;
        };
        desktop(&self.monitors)
            .map(|d| d.fraction(pos.into(), point))
            .is_none_or(|f| spans.iter().any(|s| s.contains(f)))
    }

    fn begin(&mut self, pos: Position, entry: (i16, i16)) -> Result<(), CaptureError> {
        let mask = xproto::EventMask::POINTER_MOTION
            | xproto::EventMask::BUTTON_PRESS
            | xproto::EventMask::BUTTON_RELEASE;
        let pointer = self
            .conn
            .grab_pointer(
                false,
                self.root,
                mask,
                xproto::GrabMode::ASYNC,
                xproto::GrabMode::ASYNC,
                NONE,
                self.blank_cursor,
                CURRENT_TIME,
            )?
            .reply()?;
        if pointer.status != xproto::GrabStatus::SUCCESS {
            log::debug!("x11: pointer is grabbed elsewhere ({:?})", pointer.status);
            return Ok(());
        }
        let keyboard = self
            .conn
            .grab_keyboard(
                false,
                self.root,
                CURRENT_TIME,
                xproto::GrabMode::ASYNC,
                xproto::GrabMode::ASYNC,
            )?
            .reply()?;
        if keyboard.status != xproto::GrabStatus::SUCCESS {
            log::debug!("x11: keyboard is grabbed elsewhere ({:?})", keyboard.status);
            self.conn.ungrab_pointer(CURRENT_TIME)?;
            self.conn.flush()?;
            return Ok(());
        }

        let at = (entry.0 as i32, entry.1 as i32);
        let center = self
            .monitors
            .iter()
            .find(|m| m.contains(at.0, at.1))
            .map(Rect::center)
            .unwrap_or(at);
        self.conn.warp_pointer(
            NONE,
            self.root,
            0,
            0,
            0,
            0,
            center.0 as i16,
            center.1 as i16,
        )?;
        self.conn.flush()?;

        self.entry_fraction = desktop(&self.monitors).map(|d| d.fraction(pos.into(), at));
        log::debug!(
            "x11: capture started at {pos}, pointer was at {entry:?} ({:?} along the edge)",
            self.entry_fraction
        );
        self.grab = Some(Grab {
            pos,
            entry,
            tracker: MotionTracker::new(at, center),
        });
        self.pressed.clear();
        self.modifiers = None;
        self.pending.push_back((pos, CaptureEvent::Begin));
        self.forward_modifiers(pos)
    }

    /// Ends the capture. The pointer goes to `fraction` along the edge of the
    /// capture if given, where it left the other device, and back to where it
    /// left this one otherwise.
    fn end(&mut self, fraction: Option<f64>) -> Result<(), CaptureError> {
        let Some(grab) = self.grab.take() else {
            return Ok(());
        };
        let target = fraction
            .zip(desktop(&self.monitors))
            .map(|(fraction, desktop)| desktop.point(Edge::from(grab.pos), fraction))
            .map(|(x, y)| (x as i16, y as i16))
            .unwrap_or(grab.entry);
        // while the cursor is still hidden
        self.conn
            .warp_pointer(NONE, self.root, 0, 0, 0, 0, target.0, target.1)?;
        self.conn.ungrab_keyboard(CURRENT_TIME)?;
        self.conn.ungrab_pointer(CURRENT_TIME)?;
        self.conn.flush()?;
        log::debug!("x11: capture at {} released", grab.pos);

        // nothing of the finished capture may be delivered anymore
        self.pending.clear();
        self.pressed.clear();
        self.modifiers = None;
        self.motion = None;
        self.released_at = Some(Instant::now());
        // the pointer is back at (or next to) the edge: that is not a new arrival
        self.entry_fraction = None;
        let (x, y) = (target.0 as i32, target.1 as i32);
        for pos in POSITIONS {
            self.touching[index(pos)] = at_outer_edge(&self.monitors, x, y, pos);
        }
        Ok(())
    }
}

/// a fully transparent cursor
fn create_blank_cursor(
    conn: &RustConnection,
    root: xproto::Window,
) -> Result<xproto::Cursor, ReplyOrIdError> {
    let pixmap = conn.generate_id()?;
    let gc = conn.generate_id()?;
    let cursor = conn.generate_id()?;
    conn.create_pixmap(1, pixmap, root, 1, 1)?;
    conn.create_gc(gc, pixmap, &xproto::CreateGCAux::new().foreground(0))?;
    let pixel = xproto::Rectangle {
        x: 0,
        y: 0,
        width: 1,
        height: 1,
    };
    conn.poly_fill_rectangle(pixmap, gc, &[pixel])?;
    // an all zero mask leaves no pixel of the cursor visible
    conn.create_cursor(cursor, pixmap, pixmap, 0, 0, 0, 0, 0, 0, 0, 0)?;
    conn.free_gc(gc)?;
    conn.free_pixmap(pixmap)?;
    Ok(cursor)
}

#[async_trait]
impl Capture for X11InputCapture {
    async fn create(&mut self, pos: Position) -> Result<(), CaptureError> {
        self.zones.insert(pos);
        // a pointer already resting on this edge has not just arrived there
        let pointer = self.conn.query_pointer(self.root)?.reply()?;
        let (x, y) = (pointer.root_x as i32, pointer.root_y as i32);
        self.touching[index(pos)] = at_outer_edge(&self.monitors, x, y, pos);
        self.wake();
        Ok(())
    }

    fn set_spans(&mut self, pos: Position, spans: Vec<Span>) {
        self.spans.insert(pos, spans);
    }

    async fn destroy(&mut self, pos: Position) -> Result<(), CaptureError> {
        self.zones.remove(&pos);
        self.spans.remove(&pos);
        self.touching[index(pos)] = false;
        if self.grab.as_ref().is_some_and(|g| g.pos == pos) {
            self.end(None)?;
            self.wake();
        }
        Ok(())
    }

    async fn release(&mut self) -> Result<(), CaptureError> {
        self.end(None)?;
        self.wake();
        Ok(())
    }

    async fn release_to(&mut self, fraction: f64) -> Result<(), CaptureError> {
        self.end(Some(fraction))?;
        self.wake();
        Ok(())
    }

    async fn enter(&mut self, pos: Position) -> Result<(), CaptureError> {
        if self.grab.is_some() || !self.zones.contains(&pos) {
            return Ok(());
        }
        let pointer = self.conn.query_pointer(self.root)?.reply()?;
        let at = (pointer.root_x as i32, pointer.root_y as i32);
        // as if the pointer crossed the edge right there
        let entry = desktop(&self.monitors)
            .map(|d| d.point(pos.into(), d.fraction(pos.into(), at)))
            .unwrap_or(at);
        self.begin(pos, (entry.0 as i16, entry.1 as i16))?;
        self.wake();
        Ok(())
    }

    fn entry_fraction(&self) -> Option<f64> {
        self.entry_fraction
    }

    async fn terminate(&mut self) -> Result<(), CaptureError> {
        self.end(None)?;
        self.zones.clear();
        self.conn.free_cursor(self.blank_cursor)?;
        self.conn.flush()?;
        self.wake();
        Ok(())
    }
}

impl Stream for X11InputCapture {
    type Item = Result<(Position, CaptureEvent), CaptureError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        this.waker = Some(cx.waker().clone());
        loop {
            if let Some(event) = this.pending.pop_front() {
                return Poll::Ready(Some(Ok(event)));
            }

            // events already buffered, including the ones read while waiting
            // for a reply, plus whatever the socket has available right now
            match this.conn.poll_for_event() {
                Ok(Some(event)) => {
                    if let Err(e) = this.handle_event(event) {
                        return Poll::Ready(Some(Err(e)));
                    }
                    continue;
                }
                Ok(None) => {}
                Err(e) => return Poll::Ready(Some(Err(e.into()))),
            }

            // the queue is drained: one position check covers all the motion
            if let Some(motion) = this.motion.take() {
                if let Err(e) = this.check_edges(motion) {
                    return Poll::Ready(Some(Err(e)));
                }
                continue;
            }

            if this.needs_flush {
                this.needs_flush = false;
                if let Err(e) = this.conn.flush() {
                    return Poll::Ready(Some(Err(e.into())));
                }
            }

            match this.fd.poll_read_ready(cx) {
                Poll::Ready(Ok(mut guard)) => guard.clear_ready(),
                Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e.into()))),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SINGLE: [Rect; 1] = [Rect {
        x: 0,
        y: 0,
        w: 1920,
        h: 1080,
    }];

    /// a 1920x1080 monitor with a smaller 1280x720 one to its right
    const DUAL: [Rect; 2] = [
        Rect {
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
        },
        Rect {
            x: 1920,
            y: 0,
            w: 1280,
            h: 720,
        },
    ];

    #[test]
    fn single_monitor_edges() {
        assert!(at_outer_edge(&SINGLE, 0, 500, Position::Left));
        assert!(at_outer_edge(&SINGLE, 1919, 500, Position::Right));
        assert!(at_outer_edge(&SINGLE, 500, 0, Position::Top));
        assert!(at_outer_edge(&SINGLE, 500, 1079, Position::Bottom));
        assert!(!at_outer_edge(&SINGLE, 1, 500, Position::Left));
        assert!(!at_outer_edge(&SINGLE, 0, 500, Position::Right));
        assert!(!at_outer_edge(&SINGLE, 960, 540, Position::Top));
    }

    #[test]
    fn corner_touches_both_edges() {
        assert!(at_outer_edge(&SINGLE, 0, 0, Position::Left));
        assert!(at_outer_edge(&SINGLE, 0, 0, Position::Top));
        assert!(!at_outer_edge(&SINGLE, 0, 0, Position::Bottom));
    }

    #[test]
    fn edge_between_monitors_is_not_outer() {
        // the seam between both monitors
        assert!(!at_outer_edge(&DUAL, 1919, 300, Position::Right));
        assert!(!at_outer_edge(&DUAL, 1920, 300, Position::Left));
        // below the smaller monitor the first one has nothing to its right
        assert!(at_outer_edge(&DUAL, 1919, 900, Position::Right));
        // far edges
        assert!(at_outer_edge(&DUAL, 0, 300, Position::Left));
        assert!(at_outer_edge(&DUAL, 3199, 300, Position::Right));
        // each monitor has its own bottom edge
        assert!(at_outer_edge(&DUAL, 2500, 719, Position::Bottom));
        assert!(at_outer_edge(&DUAL, 500, 1079, Position::Bottom));
        assert!(!at_outer_edge(&DUAL, 500, 719, Position::Bottom));
    }

    #[test]
    fn position_outside_all_monitors() {
        assert!(!at_outer_edge(&DUAL, 2500, 900, Position::Bottom));
    }

    #[test]
    fn outward_direction() {
        assert!(pushes_outward(Position::Left, -1.0, 0.0));
        assert!(!pushes_outward(Position::Left, 0.0, 5.0));
        assert!(pushes_outward(Position::Right, 0.5, 0.0));
        assert!(pushes_outward(Position::Top, 0.0, -2.0));
        assert!(pushes_outward(Position::Bottom, 0.0, 2.0));
        assert!(!pushes_outward(Position::Bottom, 3.0, -2.0));
    }

    fn fp(value: f64) -> xinput::Fp3232 {
        let integral = value.floor();
        xinput::Fp3232 {
            integral: integral as i32,
            frac: ((value - integral) * (1u64 << 32) as f64) as u32,
        }
    }

    #[test]
    fn raw_delta_reads_the_first_two_valuators() {
        assert_eq!(raw_delta(&[0b11], &[fp(-3.0), fp(1.5)]), (-3.0, 1.5));
        // only the vertical axis moved
        assert_eq!(raw_delta(&[0b10], &[fp(4.0)]), (0.0, 4.0));
        // a scroll valuator next to the horizontal axis
        assert_eq!(raw_delta(&[0b101], &[fp(2.0), fp(15.0)]), (2.0, 0.0));
        // truncated event
        assert_eq!(raw_delta(&[0b11], &[fp(2.0)]), (2.0, 0.0));
        assert_eq!(raw_delta(&[], &[]), (0.0, 0.0));
    }

    #[test]
    fn buttons() {
        let pressed = |button| button_event(button, true, 7);
        assert_eq!(
            pressed(1),
            Some(PointerEvent::Button {
                time: 7,
                button: BTN_LEFT,
                state: 1
            })
        );
        assert_eq!(
            button_event(3, false, 7),
            Some(PointerEvent::Button {
                time: 7,
                button: BTN_RIGHT,
                state: 0
            })
        );
        assert!(matches!(
            pressed(8),
            Some(PointerEvent::Button {
                button: BTN_BACK,
                ..
            })
        ));
        assert_eq!(pressed(10), None);
    }

    #[test]
    fn scroll_wheel() {
        let scroll = |button| button_event(button, true, 0);
        assert_eq!(
            scroll(4),
            Some(PointerEvent::AxisDiscrete120 {
                axis: 0,
                value: -120
            })
        );
        assert_eq!(
            scroll(5),
            Some(PointerEvent::AxisDiscrete120 {
                axis: 0,
                value: 120
            })
        );
        assert_eq!(
            scroll(6),
            Some(PointerEvent::AxisDiscrete120 {
                axis: 1,
                value: -120
            })
        );
        assert_eq!(
            scroll(7),
            Some(PointerEvent::AxisDiscrete120 {
                axis: 1,
                value: 120
            })
        );
        // the release of a wheel "button" carries no information
        assert_eq!(button_event(4, false, 0), None);
    }

    #[test]
    fn tracker_reports_deltas_between_positions() {
        let center = (500, 500);
        let mut tracker = MotionTracker::new((0, 300), center);
        // still on the way to the parking spot: measured from the entry point
        assert_eq!(tracker.motion((0, 305)), (Some((0, 5)), false));
        // the echo of the initial warp moves nothing
        assert_eq!(tracker.motion(center), (None, false));
        assert_eq!(tracker.motion((497, 502)), (Some((-3, 2)), false));
        assert_eq!(tracker.motion((497, 502)), (None, false));
        assert_eq!(tracker.motion((490, 500)), (Some((-7, -2)), false));
    }

    #[test]
    fn tracker_warps_back_without_counting_the_jump() {
        let center = (500, 500);
        let mut tracker = MotionTracker::new((0, 300), center);
        assert_eq!(tracker.motion(center), (None, false));
        // strays too far: a warp is requested once
        assert_eq!(tracker.motion((350, 500)), (Some((-150, 0)), true));
        // events sent before the server handled the warp keep their deltas
        assert_eq!(tracker.motion((340, 500)), (Some((-10, 0)), false));
        // the echo of the warp is not motion
        assert_eq!(tracker.motion(center), (None, false));
        assert_eq!(tracker.motion((495, 500)), (Some((-5, 0)), false));
    }

    #[test]
    fn tracker_repeats_a_lost_warp() {
        let center = (500, 500);
        let mut tracker = MotionTracker::new((0, 300), center);
        assert_eq!(tracker.motion(center), (None, false));
        assert!(tracker.motion((300, 500)).1);
        let mut at = (300, 500);
        let mut warps = 0;
        for _ in 0..=WARP_RETRY_EVENTS {
            at.1 += 1;
            if tracker.motion(at).1 {
                warps += 1;
            }
        }
        assert_eq!(warps, 1);
    }
}
