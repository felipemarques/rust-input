//! The tray icon, embedded as PNG in a few sizes.
//!
//! The files are rendered from the logo by `scripts/render-icons.py`, so
//! no SVG renderer is needed at runtime. The icon of a paused RustInput
//! ([`paused`]) and the one telling that a new version is out
//! ([`new_version`]) are made from them.

use std::io::Cursor;

const PNGS: [&[u8]; 6] = [
    include_bytes!("../../resources/tray/rustinput-16.png"),
    include_bytes!("../../resources/tray/rustinput-22.png"),
    include_bytes!("../../resources/tray/rustinput-24.png"),
    include_bytes!("../../resources/tray/rustinput-32.png"),
    include_bytes!("../../resources/tray/rustinput-48.png"),
    include_bytes!("../../resources/tray/rustinput-64.png"),
];

/// an image as 8 bit RGBA, row by row
#[derive(Clone)]
pub(crate) struct Rgba {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pixels: Vec<u8>,
}

/// the icon in every embedded size, smallest first
pub(crate) fn icons() -> Vec<Rgba> {
    PNGS.iter()
        .filter_map(|png| match decode(png) {
            Ok(icon) => Some(icon),
            Err(e) => {
                log::warn!("could not decode a tray icon: {e}");
                None
            }
        })
        .collect()
}

/// the smallest icon at least `size` pixels wide, or the largest one
#[cfg(windows)]
pub(crate) fn icon(size: u32) -> Option<Rgba> {
    let mut icons = icons();
    match icons.iter().position(|icon| icon.width >= size) {
        Some(index) => Some(icons.swap_remove(index)),
        None => icons.pop(),
    }
}

/// the orange of the RustInput logo
const ACCENT: [f32; 3] = [232.0, 105.0, 46.0];
/// the green of the sign of a new version
const NEW_VERSION: [f32; 3] = [46.0, 160.0, 67.0];
/// the blue of the sign of the mouse and keyboard on another computer
const AWAY: [f32; 3] = [53.0, 132.0, 228.0];

/// a corner of the icon, for a sign
#[derive(Clone, Copy)]
enum Corner {
    TopRight,
    BottomRight,
}

/// The icon of a paused RustInput: grey and fainter, with a pause sign (two
/// white bars on an orange disc) at the bottom right.
pub(crate) fn paused(icon: &Rgba) -> Rgba {
    let mut pixels = icon.pixels.clone();
    for p in pixels.as_chunks_mut::<4>().0 {
        let grey = (0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32) as u8;
        *p = [grey, grey, grey, (p[3] as f32 * 0.55) as u8];
    }
    let grey = Rgba {
        width: icon.width,
        height: icon.height,
        pixels,
    };
    sign(&grey, Corner::BottomRight, 0.27, ACCENT, 0.0, |x, y| {
        y.abs() <= 0.5 && (0.17..=0.47).contains(&x.abs())
    })
}

/// The icon when a new version of RustInput is out: a white arrow up on a
/// green disc at the top right, with a thin gap around it so it stands out
/// of the monitors of the logo.
pub(crate) fn new_version(icon: &Rgba) -> Rgba {
    sign(icon, Corner::TopRight, 0.24, NEW_VERSION, 0.06, |x, y| {
        let head = (-0.64..=-0.02).contains(&y) && x.abs() <= 0.58 * (y + 0.64) / 0.62;
        let stem = (-0.1..=0.62).contains(&y) && x.abs() <= 0.19;
        head || stem
    })
}

/// The icon while the mouse and keyboard are on another computer: a white
/// arrow to the right on a blue disc at the bottom right (where the pause
/// sign goes, which can not show at the same time).
pub(crate) fn away(icon: &Rgba) -> Rgba {
    sign(icon, Corner::BottomRight, 0.27, AWAY, 0.0, |x, y| {
        let head = (0.02..=0.64).contains(&x) && y.abs() <= 0.58 * (0.64 - x) / 0.62;
        let stem = (-0.62..=0.1).contains(&x) && y.abs() <= 0.19;
        head || stem
    })
}

/// the icon in its looks: paused (or with the mouse and keyboard on another
/// computer, `away`) or not, with a new version or not
pub(crate) fn look(icon: &Rgba, paused: bool, new: bool, away: bool) -> Rgba {
    let icon = match (paused, away) {
        (true, _) => self::paused(icon),
        (false, true) => self::away(icon),
        (false, false) => icon.clone(),
    };
    match new {
        true => new_version(&icon),
        false => icon,
    }
}

/// `icon` with a disc of `color` in `corner`, `radius` times the size of
/// the icon, white where `white` says (given the point in radii from the
/// centre of the disc, y down), and a transparent gap of `gap` times the
/// size of the icon around the disc
fn sign(
    icon: &Rgba,
    corner: Corner,
    radius: f32,
    color: [f32; 3],
    gap: f32,
    white: impl Fn(f32, f32) -> bool,
) -> Rgba {
    const SAMPLES: usize = 4;
    let (w, h) = (icon.width as usize, icon.height as usize);
    let mut pixels = icon.pixels.clone();
    let size = w.min(h) as f32;
    let r = size * radius;
    let cut = r + size * gap;
    let cx = w as f32 - r - size * 0.01;
    let cy = match corner {
        Corner::TopRight => r + size * 0.01,
        Corner::BottomRight => h as f32 - r - size * 0.01,
    };
    for y in 0..h {
        for x in 0..w {
            // how much of the pixel the disc, its white sign and the gap
            // around it cover
            let (mut disc, mut marked, mut gone) = (0.0f32, 0.0f32, 0.0f32);
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let dx = x as f32 + (sx as f32 + 0.5) / SAMPLES as f32 - cx;
                    let dy = y as f32 + (sy as f32 + 0.5) / SAMPLES as f32 - cy;
                    let distance = dx * dx + dy * dy;
                    if distance <= cut * cut {
                        gone += 1.0;
                    }
                    if distance > r * r {
                        continue;
                    }
                    disc += 1.0;
                    if white(dx / r, dy / r) {
                        marked += 1.0;
                    }
                }
            }
            if gone == 0.0 {
                continue;
            }
            let all = (SAMPLES * SAMPLES) as f32;
            let at = (y * w + x) * 4;
            let p = &mut pixels[at..at + 4];
            let below = p[3] as f32 / 255.0 * (1.0 - (gone - disc) / all);
            if disc == 0.0 {
                p[3] = (below * 255.0).round() as u8;
                continue;
            }
            let white = marked / disc;
            let coverage = disc / all;
            let alpha = coverage + below * (1.0 - coverage);
            for (k, base) in color.iter().enumerate() {
                let color = base + (255.0 - base) * white;
                let mixed = color * coverage + p[k] as f32 * below * (1.0 - coverage);
                p[k] = (mixed / alpha.max(f32::EPSILON)).round().clamp(0.0, 255.0) as u8;
            }
            p[3] = (alpha * 255.0).round() as u8;
        }
    }
    Rgba {
        width: icon.width,
        height: icon.height,
        pixels,
    }
}

fn decode(png: &[u8]) -> Result<Rgba, String> {
    let decoder = png::Decoder::new(Cursor::new(png));
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let info = reader.info();
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return Err(format!(
            "expected 8 bit RGBA, found {:?} {:?}",
            info.color_type, info.bit_depth
        ));
    }
    let (width, height) = (info.width, info.height);
    let mut pixels = vec![0; width as usize * height as usize * 4];
    reader.next_frame(&mut pixels).map_err(|e| e.to_string())?;
    Ok(Rgba {
        width,
        height,
        pixels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_icons_decode() {
        let icons = icons();
        let sizes: Vec<_> = icons.iter().map(|i| (i.width, i.height)).collect();
        assert_eq!(
            sizes,
            [(16, 16), (22, 22), (24, 24), (32, 32), (48, 48), (64, 64)]
        );
        for icon in icons {
            assert_eq!(icon.pixels.len(), (icon.width * icon.height * 4) as usize);
            // the logo is not empty: some pixels are opaque
            assert!(icon.pixels.chunks(4).any(|p| p[3] == 0xff));
        }
    }

    #[test]
    fn the_paused_icon_is_grey_with_an_orange_sign() {
        for icon in icons() {
            let paused = paused(&icon);
            assert_eq!(paused.pixels.len(), icon.pixels.len());
            let (w, h) = (icon.width as usize, icon.height as usize);
            let pixel = |x: usize, y: usize| {
                let at = (y * w + x) * 4;
                &paused.pixels[at..at + 4]
            };
            // the sign at the bottom right: orange and opaque at its edge
            let r = (w as f32 * 0.27) as usize;
            let edge = pixel(w - 2, h - 1 - r);
            assert!(edge[0] > 180 && edge[1] < 160 && edge[3] > 200, "{edge:?}");
            // the rest is grey
            for p in paused.pixels.as_chunks::<4>().0.iter().take(w * h / 3) {
                assert_eq!((p[0], p[1]), (p[1], p[2]));
            }
        }
    }

    #[test]
    fn a_new_version_shows_as_a_green_sign_with_a_white_arrow() {
        for icon in icons() {
            let new = new_version(&icon);
            let (w, h) = (icon.width as usize, icon.height as usize);
            let pixel = |x: usize, y: usize| {
                let at = (y * w + x) * 4;
                &new.pixels[at..at + 4]
            };
            let size = w as f32;
            let r = size * 0.24;
            let (cx, cy) = (size - r - size * 0.01, r + size * 0.01);
            // green and opaque at the edge of the disc, white in the stem
            let edge = pixel((cx + r * 0.8) as usize, cy as usize);
            assert!(
                edge[1] > edge[0] + 60 && edge[1] > edge[2] + 60 && edge[3] > 200,
                "{edge:?}"
            );
            let stem = pixel(cx as usize, (cy + r * 0.35) as usize);
            assert!(stem.iter().all(|&c| c > 200), "{stem:?}");
            // the left of the logo stays as it was
            for y in 0..h {
                let (row, left) = (y * w * 4, w / 3 * 4);
                assert_eq!(new.pixels[row..row + left], icon.pixels[row..row + left]);
            }
        }
    }

    #[test]
    fn paused_with_a_new_version_shows_both_signs() {
        for icon in icons() {
            let both = look(&icon, true, true, false);
            assert_eq!(both.pixels, new_version(&paused(&icon)).pixels);
            assert_eq!(look(&icon, false, false, false).pixels, icon.pixels);
            // paused wins over away: the pointer can not be on another one
            assert_eq!(look(&icon, true, false, true).pixels, paused(&icon).pixels);
        }
    }

    #[test]
    fn away_has_a_blue_sign_at_the_bottom_right() {
        for icon in icons() {
            let with_new = look(&icon, false, true, true);
            assert_eq!(with_new.pixels, new_version(&self::away(&icon)).pixels);
            let away = look(&icon, false, false, true);
            let (w, h) = (icon.width as usize, icon.height as usize);
            let size = w.min(h) as f32;
            let r = size * 0.27;
            let (cx, cy) = (w as f32 - r - size * 0.01, h as f32 - r - size * 0.01);
            // the edge of the disc is blue, its middle white (the arrow)
            let at = |x: f32, y: f32| {
                let i = (y as usize * w + x as usize) * 4;
                [away.pixels[i], away.pixels[i + 1], away.pixels[i + 2]]
            };
            let edge = at(cx, cy + r * 0.85);
            assert!(edge[2] > 200 && edge[0] < 100, "{edge:?}");
            let middle = at(cx, cy);
            assert!(middle.iter().all(|&c| c > 200), "{middle:?}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn picks_the_closest_larger_size() {
        assert_eq!(icon(20).map(|i| i.width), Some(22));
        assert_eq!(icon(32).map(|i| i.width), Some(32));
        assert_eq!(icon(100).map(|i| i.width), Some(64));
    }
}
