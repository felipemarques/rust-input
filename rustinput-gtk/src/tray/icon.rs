//! The tray icon, embedded as PNG in a few sizes.
//!
//! The files are rendered from the logo by `scripts/render-icons.py`, so
//! no SVG renderer is needed at runtime.

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

    #[cfg(windows)]
    #[test]
    fn picks_the_closest_larger_size() {
        assert_eq!(icon(20).map(|i| i.width), Some(22));
        assert_eq!(icon(32).map(|i| i.width), Some(32));
        assert_eq!(icon(100).map(|i| i.width), Some(64));
    }
}
