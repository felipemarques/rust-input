//! Identifying the monitors: the number of each one shown on it for a few
//! seconds, as the window numbers them in the arrangement
//! ([`rustinput_ipc::layout::monitor_numbers`]), with the name of the
//! computer and the size of the monitor. The window asks for it on this
//! computer and on the others ([`crate::arrangement`]).
//!
//! A card in the middle of each monitor, drawn like the radial menu and with
//! its fonts; it never takes the focus, and a click hides it. Available on
//! X11 and Windows, like the radial menu.

// the numbers only show on X11 and Windows: on macOS this is built, not used
#![cfg_attr(target_os = "macos", allow(dead_code))]

#[cfg(windows)]
mod windows;
#[cfg(all(unix, not(target_os = "macos")))]
mod x11;

use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use rustinput_ipc::layout::{Monitor, monitor_numbers};
use tiny_skia::{Color, FillRule, Paint, Path, PathBuilder, Pixmap, Stroke, Transform};

use crate::radial::{
    draw::{ACCENT, draw_text},
    fonts::Fonts,
};

/// how long the numbers show
const SHOWN: Duration = Duration::from_secs(4);
/// the size of a card at 100 % display scaling
const CARD: (f32, f32) = (300.0, 230.0);

/// the numbers show now: asked again meanwhile, they do not show twice
static SHOWING: AtomicBool = AtomicBool::new(false);

/// a monitor of this computer, in pixels of its desktop, and its number
#[derive(Clone, Debug, PartialEq)]
struct Card {
    number: usize,
    monitor: Monitor,
}

/// the monitors of this computer with their numbers, none when they can not
/// be read here
fn cards() -> Option<Vec<Card>> {
    let monitors: Vec<Monitor> = input_capture::desktop::monitors()?
        .into_iter()
        .map(|a| Monitor {
            x: a.x,
            y: a.y,
            width: a.width,
            height: a.height,
        })
        .collect();
    let numbers = monitor_numbers(&monitors);
    Some(
        monitors
            .into_iter()
            .zip(numbers)
            .map(|(monitor, number)| Card { number, monitor })
            .collect(),
    )
}

/// Shows the numbers of the monitors of this computer, called `name`, for a
/// few seconds, from a thread of its own; nothing when they show already.
pub(crate) fn show(name: String) {
    if SHOWING.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("identify-monitors".into())
        .spawn(move || {
            let result = run(&name);
            SHOWING.store(false, Ordering::Release);
            if let Err(e) = result {
                log::warn!("identify monitors: {e}");
            }
        });
    if let Err(e) = spawned {
        SHOWING.store(false, Ordering::Release);
        log::warn!("identify monitors: {e}");
    }
}

fn run(name: &str) -> Result<(), String> {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let x11 = std::env::var_os("DISPLAY").is_some_and(|d| !d.is_empty())
            && std::env::var_os("WAYLAND_DISPLAY").is_none_or(|d| d.is_empty());
        if !x11 {
            return Err("only available on X11 and Windows".into());
        }
        let cards = cards().ok_or("the monitors can not be read here")?;
        log::info!("identify monitors: {} of them", cards.len());
        x11::run(name, &cards, SHOWN)
    }
    #[cfg(windows)]
    {
        let cards = cards().ok_or("the monitors can not be read here")?;
        log::info!("identify monitors: {} of them", cards.len());
        windows::run(name, &cards, SHOWN)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = name;
        Err("only available on X11 and Windows".into())
    }
}

/// a rectangle with round corners
fn rounded(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<Path> {
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish()
}

fn paint(color: Color) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    paint
}

/// the radius of the corners of a card at `scale`
fn corner(scale: f32) -> f32 {
    18.0 * scale
}

/// The card of `card` on the computer `name`, at `scale` (1 at 100 %
/// display scaling): its number, big, with the name and the size of the
/// monitor under it.
fn render(card: &Card, name: &str, scale: f32, font: Option<&Fonts>) -> Pixmap {
    let scale = scale.clamp(1.0, 4.0);
    let (w, h) = (CARD.0 * scale, CARD.1 * scale);
    let mut pixmap = Pixmap::new(w.round() as u32, h.round() as u32).expect("card size");
    let r = corner(scale);
    if let Some(path) = rounded(0.0, 0.0, w, h, r) {
        pixmap.fill_path(
            &path,
            &paint(Color::from_rgba8(18, 18, 22, 228)),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
    let line = 3.0 * scale;
    if let Some(path) = rounded(line / 2.0, line / 2.0, w - line, h - line, r - line / 2.0) {
        let stroke = Stroke {
            width: line,
            ..Stroke::default()
        };
        pixmap.stroke_path(
            &path,
            &paint(Color::from_rgba8(ACCENT.0, ACCENT.1, ACCENT.2, 255)),
            &stroke,
            Transform::identity(),
            None,
        );
    }
    let Some(font) = font else {
        return pixmap;
    };
    let width = w - 32.0 * scale;
    let m = &card.monitor;
    let number = card.number.to_string();
    draw_text(
        &mut pixmap,
        font,
        &number,
        128.0 * scale,
        (w / 2.0, h * 0.40),
        width,
        1,
        (255, 255, 255),
    );
    draw_text(
        &mut pixmap,
        font,
        name,
        20.0 * scale,
        (w / 2.0, h * 0.75),
        width,
        1,
        (235, 235, 235),
    );
    let size = format!("{} × {}", m.width, m.height);
    draw_text(
        &mut pixmap,
        font,
        &size,
        15.0 * scale,
        (w / 2.0, h * 0.88),
        width,
        1,
        (165, 165, 165),
    );
    pixmap
}

/// where the card of `size` goes: in the middle of its monitor
fn place(monitor: &Monitor, size: (i32, i32)) -> (i32, i32) {
    (
        monitor.x + (monitor.width - size.0) / 2,
        monitor.y + (monitor.height - size.1) / 2,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(number: usize) -> Card {
        Card {
            number,
            monitor: Monitor {
                x: 1920,
                y: 0,
                width: 1920,
                height: 1080,
            },
        }
    }

    #[test]
    fn a_card_in_the_middle_of_its_monitor() {
        let pixmap = render(&card(2), "xeon", 1.0, None);
        assert_eq!((pixmap.width(), pixmap.height()), (300, 230));
        let alpha = |x: u32, y: u32| pixmap.data()[((y * pixmap.width() + x) * 4 + 3) as usize];
        // round corners, filled inside
        assert_eq!(alpha(0, 0), 0);
        assert!(alpha(150, 115) > 200);
        assert_eq!(render(&card(1), "xeon", 2.0, None).width(), 600);
        assert_eq!(place(&card(2).monitor, (300, 230)), (1920 + 810, 425));
    }
}
