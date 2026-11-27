//! The wheel on X11.
//!
//! The keys are watched through raw XInput 2 events, which every client gets
//! without taking them from the focused application. The wheel is an
//! override-redirect window with an alpha channel (and a round shape when no
//! compositor blends it), centered on the pointer within its monitor; the
//! pointer is confined to it while it shows.

use std::{
    os::fd::AsRawFd,
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::sync::mpsc::UnboundedSender;
use x11rb::{
    COPY_DEPTH_FROM_PARENT, CURRENT_TIME, NONE,
    connection::{Connection, RequestConnection},
    protocol::{
        Event,
        randr::ConnectionExt as _,
        shape::{self, ConnectionExt as _},
        xinput::{self, ConnectionExt as _},
        xproto::{self, ConnectionExt as _},
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

use crate::hotkeys::{self, Hotkeys};

use super::{
    Action, Fonts, Menu, Texts, Wheel,
    chord::{Chord, Key, Step},
    fonts_now,
};

const XK_CONTROL_L: u32 = 0xffe3;
const XK_CONTROL_R: u32 = 0xffe4;
const XK_ALT_L: u32 = 0xffe9;
const XK_ESCAPE: u32 = 0xff1b;
/// how often the pointer is looked at while the wheel shows
const FRAME: Duration = Duration::from_millis(16);
/// how often the keys are looked at while Ctrl+Alt are held
const WAITING: Duration = Duration::from_millis(30);

type Error = String;

fn err(e: impl std::fmt::Display) -> Error {
    e.to_string()
}

/// what each keycode means for the wheel
struct KeyMap {
    min: u8,
    keys: Vec<Key>,
}

impl KeyMap {
    fn new(conn: &RustConnection) -> Result<Self, Error> {
        let setup = conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let mapping = conn
            .get_keyboard_mapping(min, max - min + 1)
            .map_err(err)?
            .reply()
            .map_err(err)?;
        let per = mapping.keysyms_per_keycode.max(1) as usize;
        let keys = mapping
            .keysyms
            .chunks(per)
            .map(|syms| match syms.first().copied().unwrap_or(0) {
                XK_CONTROL_L | XK_CONTROL_R => Key::Ctrl,
                XK_ALT_L => Key::Alt,
                XK_ESCAPE => Key::Escape,
                _ => Key::Other,
            })
            .collect();
        Ok(Self { min, keys })
    }

    fn key(&self, keycode: u32) -> Key {
        (keycode as usize)
            .checked_sub(self.min as usize)
            .and_then(|i| self.keys.get(i))
            .copied()
            .unwrap_or(Key::Other)
    }
}

/// the window showing the wheel
struct Overlay {
    window: xproto::Window,
    colormap: Option<xproto::Colormap>,
    gc: xproto::Gcontext,
    depth: u8,
    /// the middle of the wheel, in root coordinates
    center: (i32, i32),
    /// where the pointer was when the wheel opened, it goes back there
    start: (i16, i16),
    wheel: Wheel,
}

struct State {
    conn: RustConnection,
    root: xproto::Window,
    screen: xproto::Screen,
    keys: KeyMap,
    chord: Chord,
    hotkeys: Hotkeys,
    overlay: Option<Overlay>,
    menu: Menu,
    chosen: UnboundedSender<Action>,
    texts: Texts,
    font: Option<Arc<Fonts>>,
}

pub(super) fn run(menu: Menu, chosen: UnboundedSender<Action>) -> Result<(), Error> {
    let (conn, screen_num) = x11rb::connect(None).map_err(err)?;
    let screen = conn.setup().roots[screen_num].clone();
    let root = screen.root;
    let version = conn
        .xinput_xi_query_version(2, 0)
        .map_err(err)?
        .reply()
        .map_err(err)?;
    if version.major_version < 2 {
        return Err("XInput 2 is not available".into());
    }
    conn.xinput_xi_select_events(
        root,
        &[xinput::EventMask {
            deviceid: xinput::Device::ALL_MASTER.into(),
            mask: vec![xinput::XIEventMask::RAW_KEY_PRESS | xinput::XIEventMask::RAW_KEY_RELEASE],
        }],
    )
    .map_err(err)?;
    conn.flush().map_err(err)?;
    let keys = KeyMap::new(&conn)?;
    log::info!("radial menu: hold Ctrl and the left Alt");
    let mut state = State {
        conn,
        root,
        screen,
        keys,
        chord: Chord::default(),
        hotkeys: Hotkeys::default(),
        overlay: None,
        menu,
        chosen,
        texts: Texts::new(),
        font: None,
    };
    loop {
        let timeout = if state.overlay.is_some() {
            Some(FRAME)
        } else if state.chord.pending() {
            Some(WAITING)
        } else {
            None
        };
        wait(&state.conn, timeout);
        while let Some(event) = state.conn.poll_for_event().map_err(err)? {
            let now = Instant::now();
            let step = match event {
                Event::XinputRawKeyPress(e) => {
                    state.shortcut(e.detail, true);
                    state.chord.key(state.keys.key(e.detail), true, now)
                }
                Event::XinputRawKeyRelease(e) => {
                    state.shortcut(e.detail, false);
                    state.chord.key(state.keys.key(e.detail), false, now)
                }
                Event::MappingNotify(_) => {
                    state.keys = KeyMap::new(&state.conn)?;
                    Step::Nothing
                }
                _ => Step::Nothing,
            };
            state.step(step)?;
        }
        let step = state.chord.tick(Instant::now(), state.menu.away());
        state.step(step)?;
        if state.overlay.is_some() {
            state.follow_pointer()?;
        }
    }
}

/// waits for something to read on the connection, at most `timeout`
pub(crate) fn wait(conn: &RustConnection, timeout: Option<Duration>) {
    let mut fd = libc::pollfd {
        fd: conn.stream().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = timeout.map_or(-1, |t| t.as_millis() as i32);
    unsafe { libc::poll(&mut fd, 1, ms) };
}

/// display scaling from Xft.dpi, which desktops set for HiDPI screens
pub(crate) fn display_scale(conn: &RustConnection, root: xproto::Window) -> f32 {
    let property = conn
        .get_property(
            false,
            root,
            xproto::AtomEnum::RESOURCE_MANAGER,
            xproto::AtomEnum::STRING,
            0,
            64 * 1024,
        )
        .ok()
        .and_then(|c| c.reply().ok());
    let dpi = property
        .and_then(|p| String::from_utf8(p.value).ok())
        .and_then(|resources| {
            resources.lines().find_map(|line| {
                let value = line.strip_prefix("Xft.dpi:")?;
                value.trim().parse::<f32>().ok()
            })
        })
        .unwrap_or(96.0);
    (dpi / 96.0).clamp(1.0, 4.0)
}

/// a 32 bit visual for a window with an alpha channel
pub(crate) fn argb_visual(screen: &xproto::Screen) -> Option<xproto::Visualid> {
    screen
        .allowed_depths
        .iter()
        .filter(|d| d.depth == 32)
        .flat_map(|d| d.visuals.iter())
        .find(|v| v.class == xproto::VisualClass::TRUE_COLOR)
        .map(|v| v.visual_id)
}

/// whether a compositor blends windows with their alpha channel
pub(crate) fn composited(conn: &RustConnection, root: xproto::Window) -> bool {
    let screen = conn
        .setup()
        .roots
        .iter()
        .position(|s| s.root == root)
        .unwrap_or(0);
    let name = format!("_NET_WM_CM_S{screen}");
    let Ok(atom) = conn.intern_atom(false, name.as_bytes()) else {
        return false;
    };
    let Some(atom) = atom.reply().ok().map(|a| a.atom) else {
        return false;
    };
    conn.get_selection_owner(atom)
        .ok()
        .and_then(|c| c.reply().ok())
        .is_some_and(|r| r.owner != NONE)
}

/// Puts the premultiplied BGRA `pixels`, `width` of them a row, into
/// `window`, in bands that fit into a request.
pub(crate) fn put_pixels(
    conn: &RustConnection,
    window: xproto::Window,
    gc: xproto::Gcontext,
    width: usize,
    depth: u8,
    pixels: &[u8],
) -> Result<(), String> {
    let row = width * 4;
    let max = conn.maximum_request_bytes().saturating_sub(64).max(row);
    let rows_per_band = (max / row).max(1);
    for (band, data) in pixels.chunks(rows_per_band * row).enumerate() {
        let rows = data.len() / row;
        conn.put_image(
            xproto::ImageFormat::Z_PIXMAP,
            window,
            gc,
            width as u16,
            rows as u16,
            0,
            (band * rows_per_band) as i16,
            0,
            depth,
            data,
        )
        .map_err(err)?;
    }
    conn.flush().map_err(err)
}

impl State {
    /// the X keycode `keycode` went down or up, for the shortcuts
    fn shortcut(&mut self, keycode: u32, pressed: bool) {
        // X keycodes are those of evdev plus 8
        let key = hotkeys::Key::from_linux(keycode.saturating_sub(8));
        self.menu
            .shortcut(&mut self.hotkeys, key, pressed, &self.chosen);
    }

    fn step(&mut self, step: Step) -> Result<(), Error> {
        match step {
            Step::Nothing => Ok(()),
            // only the shortcuts are watched
            Step::Open if !self.menu.wheel() => Ok(()),
            Step::Open => self.open(),
            Step::Pick => self.close(true),
            Step::Cancel => self.close(false),
        }
    }

    fn scale(&self) -> f32 {
        display_scale(&self.conn, self.root)
    }

    /// the monitor the point is on, as x, y, width, height
    fn monitor_at(&self, x: i32, y: i32) -> (i32, i32, i32, i32) {
        let screen = (
            0,
            0,
            self.screen.width_in_pixels as i32,
            self.screen.height_in_pixels as i32,
        );
        let Ok(cookie) = self.conn.randr_get_monitors(self.root, true) else {
            return screen;
        };
        let Ok(reply) = cookie.reply() else {
            return screen;
        };
        reply
            .monitors
            .iter()
            .map(|m| (m.x as i32, m.y as i32, m.width as i32, m.height as i32))
            .find(|(mx, my, mw, mh)| x >= *mx && y >= *my && x < mx + mw && y < my + mh)
            .unwrap_or(screen)
    }

    fn argb_visual(&self) -> Option<xproto::Visualid> {
        argb_visual(&self.screen)
    }

    fn composited(&self) -> bool {
        composited(&self.conn, self.root)
    }

    fn open(&mut self) -> Result<(), Error> {
        let items = self.menu.items();
        if items.is_empty() || self.overlay.is_some() {
            return Ok(());
        }
        let pointer = self
            .conn
            .query_pointer(self.root)
            .map_err(err)?
            .reply()
            .map_err(err)?;
        let wheel = Wheel::new(items, self.scale(), &self.texts, fonts_now(&mut self.font));
        let size = wheel.size() as i32;
        let (px, py) = (pointer.root_x as i32, pointer.root_y as i32);
        let (mx, my, mw, mh) = self.monitor_at(px, py);
        // centered on the pointer, within its monitor
        let x = (px - size / 2).clamp(mx, (mx + mw - size).max(mx));
        let y = (py - size / 2).clamp(my, (my + mh - size).max(my));

        let window = self.conn.generate_id().map_err(err)?;
        let (depth, visual, colormap) = match self.argb_visual() {
            Some(visual) => {
                let colormap = self.conn.generate_id().map_err(err)?;
                self.conn
                    .create_colormap(xproto::ColormapAlloc::NONE, colormap, self.root, visual)
                    .map_err(err)?;
                (32, visual, Some(colormap))
            }
            None => (self.screen.root_depth, self.screen.root_visual, None),
        };
        let mut aux = xproto::CreateWindowAux::new()
            .override_redirect(1)
            .background_pixel(0)
            .border_pixel(0)
            .event_mask(xproto::EventMask::BUTTON_PRESS);
        if let Some(colormap) = colormap {
            aux = aux.colormap(colormap);
        }
        self.conn
            .create_window(
                if colormap.is_some() {
                    depth
                } else {
                    COPY_DEPTH_FROM_PARENT
                },
                window,
                self.root,
                x as i16,
                y as i16,
                size as u16,
                size as u16,
                0,
                xproto::WindowClass::INPUT_OUTPUT,
                visual,
                &aux,
            )
            .map_err(err)?;
        let _ = self.conn.change_property8(
            xproto::PropMode::REPLACE,
            window,
            xproto::AtomEnum::WM_NAME,
            xproto::AtomEnum::STRING,
            b"RustInput menu",
        );
        if colormap.is_none() || !self.composited() {
            self.round_shape(window, size);
        }
        let gc = self.conn.generate_id().map_err(err)?;
        self.conn
            .create_gc(gc, window, &xproto::CreateGCAux::new())
            .map_err(err)?;
        self.conn.map_window(window).map_err(err)?;
        // the pointer stays in the wheel: it must not cross to another device
        let grabbed = self
            .conn
            .grab_pointer(
                false,
                window,
                xproto::EventMask::BUTTON_PRESS,
                xproto::GrabMode::ASYNC,
                xproto::GrabMode::ASYNC,
                window,
                NONE,
                CURRENT_TIME,
            )
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some_and(|r| r.status == xproto::GrabStatus::SUCCESS);
        if !grabbed {
            log::debug!("radial menu: the pointer could not be confined");
        }
        let overlay = Overlay {
            window,
            colormap,
            gc,
            depth: if colormap.is_some() {
                32
            } else {
                self.screen.root_depth
            },
            center: (x + size / 2, y + size / 2),
            start: (pointer.root_x, pointer.root_y),
            wheel,
        };
        self.draw(&overlay)?;
        self.overlay = Some(overlay);
        Ok(())
    }

    /// cuts the window round, for when nothing blends its transparency
    fn round_shape(&self, window: xproto::Window, size: i32) {
        if self
            .conn
            .extension_information(shape::X11_EXTENSION_NAME)
            .ok()
            .flatten()
            .is_none()
        {
            return;
        }
        let r = size as f64 / 2.0;
        let rows: Vec<xproto::Rectangle> = (0..size)
            .map(|y| {
                let dy = y as f64 + 0.5 - r;
                let half = (r * r - dy * dy).max(0.0).sqrt();
                xproto::Rectangle {
                    x: (r - half).round() as i16,
                    y: y as i16,
                    width: (2.0 * half).round() as u16,
                    height: 1,
                }
            })
            .collect();
        let _ = self.conn.shape_rectangles(
            shape::SO::SET,
            shape::SK::BOUNDING,
            xproto::ClipOrdering::UNSORTED,
            window,
            0,
            0,
            &rows,
        );
    }

    fn draw(&self, overlay: &Overlay) -> Result<(), Error> {
        put_pixels(
            &self.conn,
            overlay.window,
            overlay.gc,
            overlay.wheel.size() as usize,
            overlay.depth,
            &overlay.wheel.pixels(),
        )
    }

    fn follow_pointer(&mut self) -> Result<(), Error> {
        let pointer = self
            .conn
            .query_pointer(self.root)
            .map_err(err)?
            .reply()
            .map_err(err)?;
        let Some(overlay) = self.overlay.as_mut() else {
            return Ok(());
        };
        let dx = (pointer.root_x as i32 - overlay.center.0) as f64;
        let dy = (pointer.root_y as i32 - overlay.center.1) as f64;
        if overlay.wheel.point(dx, dy) {
            let overlay = self.overlay.take().expect("overlay");
            let drawn = self.draw(&overlay);
            self.overlay = Some(overlay);
            drawn?;
        }
        Ok(())
    }

    /// hides the wheel, with the action pointed at if `pick`
    fn close(&mut self, pick: bool) -> Result<(), Error> {
        let Some(overlay) = self.overlay.take() else {
            return Ok(());
        };
        let action = pick.then(|| overlay.wheel.chosen()).flatten();
        let _ = self.conn.ungrab_pointer(CURRENT_TIME);
        let _ = self.conn.destroy_window(overlay.window);
        let _ = self.conn.free_gc(overlay.gc);
        if let Some(colormap) = overlay.colormap {
            let _ = self.conn.free_colormap(colormap);
        }
        // the pointer goes back to where it was
        let _ = self.conn.warp_pointer(
            NONE,
            self.root,
            0,
            0,
            0,
            0,
            overlay.start.0,
            overlay.start.1,
        );
        self.conn.flush().map_err(err)?;
        if let Some(action) = action {
            log::info!("radial menu: {action:?}");
            let _ = self.chosen.send(action);
        }
        Ok(())
    }
}
