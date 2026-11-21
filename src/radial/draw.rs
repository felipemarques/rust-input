//! Drawing the wheel into a pixel buffer: slices around a disc, the one
//! pointed at highlighted, and what it does written in the middle.

use std::f32::consts::PI;

use super::fonts::{Fonts, visual};
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Transform};

/// diameter of the wheel at 100 % display scaling
const SIZE: f32 = 420.0;
/// the accent of the RustInput icon
const ACCENT: (u8, u8, u8) = (232, 105, 46);

/// size in pixels of the wheel at `scale`
pub(super) fn size(scale: f32) -> u32 {
    (SIZE * scale.clamp(1.0, 4.0)).round() as u32
}

/// The slice the pointer points at, `(dx, dy)` away from the middle, among
/// `n` slices starting at the top and going clockwise. `None` within the dead
/// zone in the middle.
pub(super) fn slice_at(n: usize, dx: f64, dy: f64, dead_zone: f64) -> Option<usize> {
    if n == 0 || dx.hypot(dy) < dead_zone {
        return None;
    }
    let width = 360.0 / n as f64;
    // screen coordinates grow downwards: -90 degrees is up
    let angle = dy.atan2(dx).to_degrees() + 90.0 + width / 2.0;
    Some((angle.rem_euclid(360.0) / width) as usize % n)
}

/// radius of the dead zone at `scale`: pointing into the middle picks nothing
pub(super) fn dead_zone(scale: f32) -> f64 {
    (SIZE * 0.5 * 0.38 * 0.6 * scale.clamp(1.0, 4.0)) as f64
}

/// the wheel with `labels` around it, `selected` highlighted and described in
/// the middle (`hint` when nothing is)
pub(super) fn render(
    labels: &[String],
    selected: Option<usize>,
    hint: &str,
    font: Option<&Fonts>,
    scale: f32,
) -> Pixmap {
    let scale = scale.clamp(1.0, 4.0);
    let size = size(scale);
    let mut pixmap = Pixmap::new(size, size).expect("wheel size");
    let c = size as f32 / 2.0;
    let outer = c - 2.0 * scale;
    let inner = outer * 0.38;
    let gap = 3.0 * scale;

    fill_circle(&mut pixmap, c, outer, Color::from_rgba8(18, 18, 22, 220));
    let n = labels.len();
    for i in 0..n {
        let (from, to) = slice_angles(i, n);
        let gap_angle = gap / outer;
        let color = if Some(i) == selected {
            Color::from_rgba8(ACCENT.0, ACCENT.1, ACCENT.2, 240)
        } else {
            Color::from_rgba8(255, 255, 255, 24)
        };
        fill_sector(
            &mut pixmap,
            c,
            inner + gap,
            outer - gap,
            from + gap_angle,
            to - gap_angle,
            color,
        );
    }
    fill_circle(&mut pixmap, c, inner - gap, Color::from_rgba8(0, 0, 0, 90));

    let Some(font) = font else {
        return pixmap;
    };
    let middle = (inner + outer) / 2.0;
    for (i, label) in labels.iter().enumerate() {
        let (from, to) = slice_angles(i, n);
        let angle = (from + to) / 2.0;
        let arc = (to - from) * middle;
        let width = (arc * 0.92).min(140.0 * scale);
        let (x, y) = (c + middle * angle.cos(), c + middle * angle.sin());
        let white = if Some(i) == selected { 255 } else { 225 };
        draw_text(
            &mut pixmap,
            font,
            label,
            13.0 * scale,
            (x, y),
            width,
            3,
            (white, white, white),
        );
    }
    let (text, size, color) = match selected.and_then(|i| labels.get(i)) {
        Some(label) => (label.as_str(), 18.0 * scale, (255, 255, 255)),
        None => (hint, 14.0 * scale, (170, 170, 170)),
    };
    draw_text(&mut pixmap, font, text, size, (c, c), inner * 1.6, 3, color);
    pixmap
}

/// the angles (radians, clockwise from the right) slice `i` of `n` spans,
/// the first one centered at the top
fn slice_angles(i: usize, n: usize) -> (f32, f32) {
    let width = 2.0 * PI / n.max(1) as f32;
    let middle = -PI / 2.0 + i as f32 * width;
    (middle - width / 2.0, middle + width / 2.0)
}

fn paint(color: Color) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    paint
}

fn fill_circle(pixmap: &mut Pixmap, c: f32, radius: f32, color: Color) {
    if let Some(path) = PathBuilder::from_circle(c, c, radius) {
        pixmap.fill_path(
            &path,
            &paint(color),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

/// a ring segment between the radii `inner` and `outer` and the angles
/// `from` and `to`
fn fill_sector(
    pixmap: &mut Pixmap,
    c: f32,
    inner: f32,
    outer: f32,
    from: f32,
    to: f32,
    color: Color,
) {
    const STEPS: usize = 48;
    let point = |radius: f32, angle: f32| (c + radius * angle.cos(), c + radius * angle.sin());
    let mut pb = PathBuilder::new();
    let (x, y) = point(outer, from);
    pb.move_to(x, y);
    for step in 1..=STEPS {
        let (x, y) = point(outer, from + (to - from) * step as f32 / STEPS as f32);
        pb.line_to(x, y);
    }
    for step in (0..=STEPS).rev() {
        let (x, y) = point(inner, from + (to - from) * step as f32 / STEPS as f32);
        pb.line_to(x, y);
    }
    pb.close();
    if let Some(path) = pb.finish() {
        pixmap.fill_path(
            &path,
            &paint(color),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

fn text_width(font: &Fonts, text: &str, px: f32) -> f32 {
    font.width(text, px)
}

/// a letter of Chinese or Japanese (or Korean): lines may break after any
fn is_cjk(c: char) -> bool {
    matches!(c, '\u{3000}'..='\u{9FFF}' | '\u{AC00}'..='\u{D7AF}' | '\u{F900}'..='\u{FAFF}' | '\u{FF00}'..='\u{FFEF}')
}

/// what a line may break between, each with whether a space goes before it:
/// the words, and every letter of Chinese and Japanese, which have no spaces
fn pieces(text: &str) -> Vec<(String, bool)> {
    let mut pieces = Vec::new();
    for word in text.split_whitespace() {
        let mut spaced = true;
        let mut run = String::new();
        for c in word.chars() {
            if is_cjk(c) {
                if !run.is_empty() {
                    pieces.push((std::mem::take(&mut run), spaced));
                    spaced = false;
                }
                pieces.push((c.to_string(), spaced));
                spaced = false;
            } else {
                run.push(c);
            }
        }
        if !run.is_empty() {
            pieces.push((run, spaced));
        }
    }
    pieces
}

/// `text` broken into lines of at most `width`, at most `max_lines` of them
fn wrap(font: &Fonts, text: &str, px: f32, width: f32, max_lines: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    for (piece, spaced) in pieces(text) {
        let candidate = if line.is_empty() {
            piece.clone()
        } else if spaced {
            format!("{line} {piece}")
        } else {
            format!("{line}{piece}")
        };
        if text_width(font, &candidate, px) <= width || line.is_empty() {
            line = candidate;
        } else {
            lines.push(std::mem::replace(&mut line, piece));
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    if lines.len() > max_lines {
        lines.truncate(max_lines);
        if let Some(last) = lines.last_mut() {
            last.push('…');
        }
    }
    // a single word wider than the room is shortened
    for line in &mut lines {
        while text_width(font, line, px) > width && line.chars().count() > 2 {
            let mut chars: Vec<char> = line.chars().collect();
            chars.truncate(chars.len() - 2);
            *line = chars.into_iter().collect::<String>() + "…";
        }
    }
    lines
}

/// draws `text` centered on `center`, wrapped to `width`
#[allow(clippy::too_many_arguments)]
fn draw_text(
    pixmap: &mut Pixmap,
    font: &Fonts,
    text: &str,
    px: f32,
    center: (f32, f32),
    width: f32,
    max_lines: usize,
    color: (u8, u8, u8),
) {
    let lines = wrap(font, text, px, width, max_lines);
    let (ascent, line_height) = font.line(px);
    let top = center.1 - line_height * lines.len() as f32 / 2.0;
    for (n, line) in lines.iter().enumerate() {
        let baseline = top + n as f32 * line_height + ascent;
        // as drawn: Arabic joined and from right to left
        let shown = visual(line);
        let mut x = center.0 - text_width(font, &shown, px) / 2.0;
        for ch in shown.chars() {
            let glyph = font.place(ch, px);
            font.draw(&glyph, px, x, baseline, |x, y, coverage| {
                blend(pixmap, x, y, coverage, color)
            });
            x += glyph.advance;
        }
    }
}

/// draws `color` at `(x, y)`, `coverage` (0 to 1) of it
fn blend(pixmap: &mut Pixmap, x: i32, y: i32, coverage: f32, color: (u8, u8, u8)) {
    let (pw, ph) = (pixmap.width() as i32, pixmap.height() as i32);
    if coverage <= 0.0 || x < 0 || y < 0 || x >= pw || y >= ph {
        return;
    }
    let data = pixmap.data_mut();
    let at = ((y * pw + x) * 4) as usize;
    let a = (coverage.min(1.0) * 255.0).round() as u32;
    // premultiplied: source over destination
    for (k, channel) in [color.0, color.1, color.2].into_iter().enumerate() {
        let src = channel as u32 * a / 255;
        data[at + k] = (src + data[at + k] as u32 * (255 - a) / 255) as u8;
    }
    data[at + 3] = (a + data[at + 3] as u32 * (255 - a) / 255) as u8;
}

/// the pixels as premultiplied BGRA, what X11 (32 bit visuals) and Windows
/// (layered windows) take
pub(super) fn bgra(pixmap: &Pixmap) -> Vec<u8> {
    let mut data = pixmap.data().to_vec();
    for at in (0..data.len()).step_by(4) {
        data.swap(at, at + 2);
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slices_start_at_the_top_and_go_clockwise() {
        // 4 slices: top, right, bottom, left
        assert_eq!(slice_at(4, 0.0, -100.0, 10.0), Some(0));
        assert_eq!(slice_at(4, 100.0, 0.0, 10.0), Some(1));
        assert_eq!(slice_at(4, 0.0, 100.0, 10.0), Some(2));
        assert_eq!(slice_at(4, -100.0, 0.0, 10.0), Some(3));
        // just left of the top still belongs to the first slice
        assert_eq!(slice_at(4, -30.0, -100.0, 10.0), Some(0));
        assert_eq!(slice_at(7, 0.0, -100.0, 10.0), Some(0));
        // the middle picks nothing
        assert_eq!(slice_at(4, 3.0, 3.0, 10.0), None);
        assert_eq!(slice_at(0, 0.0, -100.0, 10.0), None);
    }

    #[test]
    fn chinese_and_japanese_break_between_letters() {
        let joined = |text: &str| {
            pieces(text)
                .into_iter()
                .map(|(p, spaced)| if spaced { format!(" {p}") } else { p })
                .collect::<String>()
        };
        assert_eq!(pieces("instancia-b に移動").len(), 4);
        assert_eq!(joined("instancia-b に移動"), " instancia-b に移動");
        assert_eq!(joined("Go to xeon"), " Go to xeon");
        assert_eq!(pieces("Go to xeon").len(), 3);
    }

    #[test]
    fn the_wheel_is_drawn_without_a_font_too() {
        let labels = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let pixmap = render(&labels, Some(1), "hint", None, 1.0);
        assert_eq!(pixmap.width(), size(1.0));
        let c = (pixmap.width() / 2) as usize;
        let at = |x: usize, y: usize| pixmap.data()[(y * pixmap.width() as usize + x) * 4 + 3];
        // the corners are transparent, the wheel is not
        assert_eq!(at(0, 0), 0);
        assert!(at(c, c / 3) > 0);
        assert_eq!(size(2.0), 2 * size(1.0));
        assert_eq!(bgra(&pixmap).len(), pixmap.data().len());
    }
}
