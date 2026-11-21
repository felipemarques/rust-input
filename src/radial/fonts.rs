//! The fonts of the labels of the wheel, and the text as it is drawn.
//!
//! The first font is one for the language of RustInput (Chinese and
//! Japanese need fonts of their own), the others fill in what it lacks:
//! the names of the computers are often in Latin letters. Glyphs are read
//! from a font only when drawn (ab_glyph), so a font with all of Chinese
//! costs no time at start. Arabic is joined (its letters take the form for
//! their place in the word) and put from right to left, as the wheel draws
//! one glyph after the other.

use std::path::PathBuf;

use ab_glyph::{Font, FontVec, Glyph, GlyphId, PxScale, ScaleFont, point};
use rustinput_i18n::Language;
use unicode_bidi::BidiInfo;

/// the fonts of the labels, for one language
pub(crate) struct Fonts {
    list: Vec<FontVec>,
    /// the language they are for
    pub(crate) language: Language,
}

/// a glyph laid out: from which font, which one and how far the next goes
pub(super) struct Placed {
    font: usize,
    id: GlyphId,
    pub(super) advance: f32,
}

impl Fonts {
    /// the fonts for `language` and for Latin letters, `None` if there are
    /// none on this computer
    pub(crate) fn load(language: Language) -> Option<Fonts> {
        let mut list = Vec::new();
        for (path, index) in candidates(language) {
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            match FontVec::try_from_vec_and_index(bytes, index) {
                Ok(font) => {
                    log::debug!("radial menu: font {path:?} ({index})");
                    list.push(font);
                }
                Err(e) => log::debug!("radial menu: font {path:?}: {e}"),
            }
            // one for the language and one for the rest are enough
            if list.len() == 2 {
                break;
            }
        }
        if list.is_empty() {
            log::warn!("radial menu: no font found, the actions show without names");
            return None;
        }
        Some(Fonts { list, language })
    }

    /// the scale of `font` for a size of `px` pixels to the em
    fn scale(&self, font: usize, px: f32) -> PxScale {
        let f = &self.list[font];
        let em = f.units_per_em().unwrap_or(1000.0);
        PxScale::from(px * f.height_unscaled() / em)
    }

    /// `c` in the first font that has it
    pub(super) fn place(&self, c: char, px: f32) -> Placed {
        let font = self
            .list
            .iter()
            .position(|f| f.glyph_id(c).0 != 0)
            .unwrap_or(0);
        let f = &self.list[font];
        let id = f.glyph_id(c);
        let advance = f.as_scaled(self.scale(font, px)).h_advance(id);
        Placed { font, id, advance }
    }

    /// how wide `text` is at `px`
    pub(super) fn width(&self, text: &str, px: f32) -> f32 {
        text.chars().map(|c| self.place(c, px).advance).sum()
    }

    /// the ascent and the height of a line at `px`, of the first font
    pub(super) fn line(&self, px: f32) -> (f32, f32) {
        let scaled = self.list[0].as_scaled(self.scale(0, px));
        (
            scaled.ascent(),
            scaled.ascent() - scaled.descent() + scaled.line_gap(),
        )
    }

    /// draws `glyph` with its origin at `(x, baseline)`: `pixel` gets each
    /// pixel it covers and how much
    pub(super) fn draw(
        &self,
        glyph: &Placed,
        px: f32,
        x: f32,
        baseline: f32,
        mut pixel: impl FnMut(i32, i32, f32),
    ) {
        let f = &self.list[glyph.font];
        let positioned: Glyph = glyph
            .id
            .with_scale_and_position(self.scale(glyph.font, px), point(x, baseline));
        if let Some(outlined) = f.outline_glyph(positioned) {
            let bounds = outlined.px_bounds();
            outlined.draw(|gx, gy, coverage| {
                pixel(
                    bounds.min.x as i32 + gx as i32,
                    bounds.min.y as i32 + gy as i32,
                    coverage,
                )
            });
        }
    }
}

/// `line` in the order its letters are drawn from left to right: Arabic
/// joined and from right to left, the rest as it is
pub(super) fn visual(line: &str) -> String {
    if !line.chars().any(is_arabic) {
        return line.to_owned();
    }
    let joined = ar_reshaper::reshape_line(line);
    let info = BidiInfo::new(&joined, None);
    let Some(paragraph) = info.paragraphs.first() else {
        return joined;
    };
    let ordered = info.reorder_line(paragraph, paragraph.range.clone());
    // brackets face the other way in a right to left run
    ordered
        .chars()
        .map(|c| match c {
            '(' if paragraph.level.is_rtl() => ')',
            ')' if paragraph.level.is_rtl() => '(',
            c => c,
        })
        .collect()
}

fn is_arabic(c: char) -> bool {
    matches!(c, '\u{0600}'..='\u{06FF}' | '\u{0750}'..='\u{077F}' | '\u{FB50}'..='\u{FDFF}' | '\u{FE70}'..='\u{FEFF}')
}

/// the fonts to try for `language`, with their index in a collection: the
/// one for the language first, then those for Latin letters
fn candidates(language: Language) -> Vec<(PathBuf, u32)> {
    let mut candidates: Vec<(PathBuf, u32)> = Vec::new();
    #[cfg(windows)]
    {
        let fonts = std::env::var_os("WINDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
            .join("Fonts");
        let own: &[&str] = match language.code {
            "ja" => &["YuGothB.ttc", "meiryob.ttc", "meiryo.ttc", "msgothic.ttc"],
            "zh-CN" => &["msyhbd.ttc", "msyh.ttc", "simhei.ttf", "simsun.ttc"],
            _ => &[],
        };
        for name in own
            .iter()
            .chain(&["segoeuib.ttf", "segoeui.ttf", "arialbd.ttf", "arial.ttf"])
        {
            candidates.push((fonts.join(name), 0));
        }
    }
    #[cfg(not(windows))]
    {
        // the fonts the desktop uses: for the language, then for the rest
        let lang = match language.code {
            "zh-CN" => "zh-cn",
            code => code,
        };
        for pattern in [format!("sans:bold:lang={lang}"), "sans:bold".to_owned()] {
            if let Ok(out) = std::process::Command::new("fc-match")
                .args(["-f", "%{file}\n%{index}", &pattern])
                .output()
            {
                let out = String::from_utf8_lossy(&out.stdout);
                let mut lines = out.lines();
                if let Some(file) = lines.next().filter(|f| !f.is_empty()) {
                    let index = lines
                        .next()
                        .and_then(|i| i.trim().parse().ok())
                        .unwrap_or(0);
                    candidates.push((PathBuf::from(file), index));
                }
            }
        }
        let own: &[&str] = match language.code {
            "ja" | "zh-CN" => &[
                "/usr/share/fonts/opentype/noto/NotoSansCJK-Bold.ttc",
                "/usr/share/fonts/noto-cjk/NotoSansCJK-Bold.ttc",
                "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
            ],
            _ => &[],
        };
        for path in own.iter().chain(&[
            "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
            "/usr/share/fonts/truetype/ubuntu/Ubuntu-B.ttf",
            "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
            "/usr/share/fonts/dejavu/DejaVuSans-Bold.ttf",
            "/usr/share/fonts/truetype/noto/NotoSans-Bold.ttf",
            "/System/Library/Fonts/Helvetica.ttc",
        ]) {
            candidates.push((PathBuf::from(path), 0));
        }
    }
    // each once
    let mut seen = Vec::new();
    candidates.retain(|c| {
        let new = !seen.contains(c);
        seen.push(c.clone());
        new
    });
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latin_text_stays_as_it_is() {
        assert_eq!(visual("Go to xeon-X99-D8"), "Go to xeon-X99-D8");
    }

    #[test]
    fn arabic_is_joined_and_from_right_to_left() {
        // "salam": four letters, joined into their forms for their places
        let shown = visual("سلام");
        assert!(
            shown
                .chars()
                .all(|c| ('\u{FB50}'..='\u{FEFF}').contains(&c))
        );
        // the first letter, in its form at the start of a word, is drawn last
        let first = ar_reshaper::reshape_line("سلام").chars().next().unwrap();
        assert_eq!(shown.chars().last(), Some(first));
        // a name in Latin letters keeps its own order
        assert!(visual("اذهب إلى xeon").contains("xeon"));
    }
}
