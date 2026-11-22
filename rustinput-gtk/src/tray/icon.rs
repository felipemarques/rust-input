//! The tray icon, embedded as PNG in a few sizes.
//!
//! The files are rendered from the logo by `scripts/render-icons.py`, so
//! no SVG renderer is needed at runtime. The icon of a paused RustInput is
//! made from them ([`paused`]).

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

/// The icon of a paused RustInput: grey and fainter, with a pause sign (two
/// white bars on an orange disc) at the bottom right.
pub(crate) fn paused(icon: &Rgba) -> Rgba {
    const SAMPLES: usize = 4;
    let (w, h) = (icon.width as usize, icon.height as usize);
    let mut pixels = icon.pixels.clone();
    for p in pixels.as_chunks_mut::<4>().0 {
        let grey = (0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32) as u8;
        *p = [grey, grey, grey, (p[3] as f32 * 0.55) as u8];
    }
    let size = w.min(h) as f32;
    let r = size * 0.27;
    let (cx, cy) = (w as f32 - r - size * 0.01, h as f32 - r - size * 0.01);
    let (bar, half, gap) = (r * 0.3, r * 0.5, r * 0.17);
    for y in 0..h {
        for x in 0..w {
            // how much of the pixel the disc and the bars cover
            let (mut disc, mut bars) = (0.0f32, 0.0f32);
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let dx = x as f32 + (sx as f32 + 0.5) / SAMPLES as f32 - cx;
                    let dy = y as f32 + (sy as f32 + 0.5) / SAMPLES as f32 - cy;
                    if dx * dx + dy * dy > r * r {
                        continue;
                    }
                    disc += 1.0;
                    let across = dx.abs();
                    if dy.abs() <= half && across >= gap && across <= gap + bar {
                        bars += 1.0;
                    }
                }
            }
            if disc == 0.0 {
                continue;
            }
            let white = bars / disc;
            let coverage = disc / (SAMPLES * SAMPLES) as f32;
            let at = (y * w + x) * 4;
            let p = &mut pixels[at..at + 4];
            let below = p[3] as f32 / 255.0;
            let alpha = coverage + below * (1.0 - coverage);
            for (k, accent) in ACCENT.iter().enumerate() {
                let color = accent + (255.0 - accent) * white;
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

    #[cfg(windows)]
    #[test]
    fn picks_the_closest_larger_size() {
        assert_eq!(icon(20).map(|i| i.width), Some(22));
        assert_eq!(icon(32).map(|i| i.width), Some(32));
        assert_eq!(icon(100).map(|i| i.width), Some(64));
    }
}
