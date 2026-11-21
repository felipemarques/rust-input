//! The monitors of this computer, for the arrangement of the computers
//! (see the RustInput documentation): where each one is on the desktop, in
//! pixels, as the capture sees them.

use input_event::edge::Area;

/// The monitors of the desktop, in pixels, `None` when they can not be read
/// here (no X server, a system without a capture backend).
pub fn monitors() -> Option<Vec<Area>> {
    let monitors = read()?;
    let monitors: Vec<Area> = monitors
        .into_iter()
        .filter(|m| m.width > 0 && m.height > 0)
        .collect();
    (!monitors.is_empty()).then_some(monitors)
}

#[cfg(x11)]
fn read() -> Option<Vec<Area>> {
    use x11rb::{
        connection::Connection,
        protocol::{randr::ConnectionExt as _, xproto::ConnectionExt as _},
    };

    let (conn, screen) = x11rb::connect(None).ok()?;
    let root = conn.setup().roots.get(screen)?.root;
    // RandR 1.5 for monitors; without it, the screen as one
    let monitors = conn
        .randr_get_monitors(root, true)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|reply| {
            reply
                .monitors
                .iter()
                .map(|m| Area {
                    x: m.x as i32,
                    y: m.y as i32,
                    width: m.width as i32,
                    height: m.height as i32,
                })
                .collect::<Vec<_>>()
        })
        .filter(|monitors| !monitors.is_empty());
    if monitors.is_some() {
        return monitors;
    }
    let root = conn.get_geometry(root).ok()?.reply().ok()?;
    Some(vec![Area {
        x: 0,
        y: 0,
        width: root.width as i32,
        height: root.height as i32,
    }])
}

#[cfg(windows)]
fn read() -> Option<Vec<Area>> {
    Some(
        crate::windows::displays()
            .into_iter()
            .map(|r| Area {
                x: r.left,
                y: r.top,
                width: r.right - r.left,
                height: r.bottom - r.top,
            })
            .collect(),
    )
}

#[cfg(target_os = "macos")]
fn read() -> Option<Vec<Area>> {
    use core_graphics::display::CGDisplay;

    let displays = CGDisplay::active_displays().ok()?;
    Some(
        displays
            .into_iter()
            .map(|id| {
                let display = CGDisplay::new(id);
                let bounds = display.bounds();
                // pixels rather than points, as the other systems
                let scale = display.pixels_wide() as f64 / bounds.size.width.max(1.0);
                Area {
                    x: (bounds.origin.x * scale).round() as i32,
                    y: (bounds.origin.y * scale).round() as i32,
                    width: display.pixels_wide() as i32,
                    height: display.pixels_high() as i32,
                }
            })
            .collect(),
    )
}

#[cfg(not(any(x11, windows, target_os = "macos")))]
fn read() -> Option<Vec<Area>> {
    None
}
