//! The numbers on X11: an override-redirect window with an alpha channel in
//! the middle of each monitor (cut round when no compositor blends it),
//! drawn again whenever it is uncovered.

use std::time::{Duration, Instant};

use x11rb::{
    COPY_DEPTH_FROM_PARENT,
    connection::{Connection, RequestConnection},
    protocol::{
        Event,
        shape::{self, ConnectionExt as _},
        xproto::{self, ConnectionExt as _},
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

use super::{Card, corner, place, render};
use crate::radial::{
    draw::bgra,
    fonts::Fonts,
    x11::{argb_visual, composited, display_scale, put_pixels, wait},
};

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// a card on the screen
struct Shown {
    window: xproto::Window,
    gc: xproto::Gcontext,
    width: usize,
    pixels: Vec<u8>,
}

pub(super) fn run(name: &str, cards: &[Card], shown: Duration) -> Result<(), String> {
    let (conn, screen_num) = x11rb::connect(None).map_err(err)?;
    let screen = conn.setup().roots[screen_num].clone();
    let root = screen.root;
    let scale = display_scale(&conn, root);
    let font = Fonts::load(rustinput_i18n::current());
    let visual = argb_visual(&screen);
    let colormap = match visual {
        Some(visual) => {
            let colormap = conn.generate_id().map_err(err)?;
            conn.create_colormap(xproto::ColormapAlloc::NONE, colormap, root, visual)
                .map_err(err)?;
            Some(colormap)
        }
        None => None,
    };
    let blended = colormap.is_some() && composited(&conn, root);
    let depth = if colormap.is_some() {
        32
    } else {
        screen.root_depth
    };

    let mut windows = Vec::new();
    for card in cards {
        let pixmap = render(card, name, scale, font.as_ref());
        let (w, h) = (pixmap.width() as i32, pixmap.height() as i32);
        let (x, y) = place(&card.monitor, (w, h));
        let window = conn.generate_id().map_err(err)?;
        let mut aux = xproto::CreateWindowAux::new()
            .override_redirect(1)
            .background_pixel(0)
            .border_pixel(0)
            .event_mask(xproto::EventMask::EXPOSURE | xproto::EventMask::BUTTON_PRESS);
        if let Some(colormap) = colormap {
            aux = aux.colormap(colormap);
        }
        conn.create_window(
            if colormap.is_some() {
                32
            } else {
                COPY_DEPTH_FROM_PARENT
            },
            window,
            root,
            x as i16,
            y as i16,
            w as u16,
            h as u16,
            0,
            xproto::WindowClass::INPUT_OUTPUT,
            visual.unwrap_or(screen.root_visual),
            &aux,
        )
        .map_err(err)?;
        let title = format!("RustInput monitor {}", card.number);
        let _ = conn.change_property8(
            xproto::PropMode::REPLACE,
            window,
            xproto::AtomEnum::WM_NAME,
            xproto::AtomEnum::STRING,
            title.as_bytes(),
        );
        if !blended {
            round_shape(&conn, window, w, h, corner(scale.clamp(1.0, 4.0)));
        }
        let gc = conn.generate_id().map_err(err)?;
        conn.create_gc(gc, window, &xproto::CreateGCAux::new())
            .map_err(err)?;
        conn.map_window(window).map_err(err)?;
        windows.push(Shown {
            window,
            gc,
            width: w as usize,
            pixels: bgra(&pixmap),
        });
    }
    for s in &windows {
        put_pixels(&conn, s.window, s.gc, s.width, depth, &s.pixels)?;
    }

    let until = Instant::now() + shown;
    'shown: loop {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        wait(&conn, Some(left));
        while let Some(event) = conn.poll_for_event().map_err(err)? {
            match event {
                Event::Expose(e) if e.count == 0 => {
                    if let Some(s) = windows.iter().find(|s| s.window == e.window) {
                        put_pixels(&conn, s.window, s.gc, s.width, depth, &s.pixels)?;
                    }
                }
                // a click hides them
                Event::ButtonPress(_) => break 'shown,
                _ => {}
            }
        }
    }
    for s in windows {
        let _ = conn.destroy_window(s.window);
        let _ = conn.free_gc(s.gc);
    }
    if let Some(colormap) = colormap {
        let _ = conn.free_colormap(colormap);
    }
    conn.flush().map_err(err)
}

/// cuts the corners of the window round, for when nothing blends its
/// transparency
fn round_shape(conn: &RustConnection, window: xproto::Window, w: i32, h: i32, r: f32) {
    if conn
        .extension_information(shape::X11_EXTENSION_NAME)
        .ok()
        .flatten()
        .is_none()
    {
        return;
    }
    let rows: Vec<xproto::Rectangle> = (0..h)
        .map(|y| {
            // how far the corner cuts into this row
            let from_edge = (y as f32 + 0.5).min(h as f32 - y as f32 - 0.5);
            let inset = if from_edge < r {
                let dy = r - from_edge;
                (r - (r * r - dy * dy).max(0.0).sqrt()).round() as i32
            } else {
                0
            };
            xproto::Rectangle {
                x: inset as i16,
                y: y as i16,
                width: (w - 2 * inset).max(0) as u16,
                height: 1,
            }
        })
        .collect();
    let _ = conn.shape_rectangles(
        shape::SO::SET,
        shape::SK::BOUNDING,
        xproto::ClipOrdering::UNSORTED,
        window,
        0,
        0,
        &rows,
    );
}
