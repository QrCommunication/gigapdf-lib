//! Adobe Core-14 (standard-14) glyph **advance widths**, in 1000-unit em, for
//! the Latin text faces — Helvetica, Times and Courier and their styles.
//!
//! These are the published AFM metrics. They let a synthesised text appearance
//! (FreeText `/Q` quadding) position each line at the *true* width of the `/DA`
//! font, exactly the way a conforming viewer lays the text out, instead of a
//! crude `chars × size × factor` estimate. Centre/right alignment then matches
//! what the viewer actually draws.
//!
//! Accented Latin glyphs share their base letter's advance in every Core-14 face
//! (an exact identity in the AFMs — `Aacute` is 667 just like `A` in Helvetica),
//! so the high WinAnsi range folds onto the ASCII widths; only the
//! punctuation/symbol codes that have their own advance carry explicit entries.

use super::bundled::{base14_kind, Base14};
use super::winansi_to_char;

/// Which Core-14 metric family + style to measure a run with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Afm {
    Helv,
    HelvBold,
    Times,
    TimesBold,
    TimesItalic,
    TimesBoldItalic,
    /// Courier (monospaced) — every glyph advances 600.
    Courier,
}

/// Classify a `/DA` font name (full base-14 name, AcroForm short name, or any
/// other) to a Core-14 metric family. Helvetica is the fallback (the `/DA`
/// default and the engine's own FreeText font). Oblique/Italic Helvetica shares
/// the regular advances, so it needs no separate table.
fn classify(font: &str) -> Afm {
    let raw = font.split('+').next_back().unwrap_or(font).trim();
    let lower = raw.to_ascii_lowercase();

    // AcroForm `/DR` short names (`/Helv 12 Tf`, `/TiBo`, `/CoOb`, …).
    match lower.as_str() {
        "helv" | "heob" => return Afm::Helv,
        "hebo" => return Afm::HelvBold, // HeBo / HeBO (bold, oblique == bold advances)
        "tiro" => return Afm::Times,
        "tibo" => return Afm::TimesBold,
        "tiit" => return Afm::TimesItalic,
        "tibi" => return Afm::TimesBoldItalic,
        "cour" | "cobo" | "coob" => return Afm::Courier, // Courier is monospaced
        _ => {}
    }

    let bold = lower.contains("bold");
    let italic = lower.contains("italic") || lower.contains("oblique");
    match base14_kind(font) {
        Some(Base14::Serif) => match (bold, italic) {
            (true, true) => Afm::TimesBoldItalic,
            (true, false) => Afm::TimesBold,
            (false, true) => Afm::TimesItalic,
            (false, false) => Afm::Times,
        },
        Some(Base14::Mono) => Afm::Courier,
        // Sans / Symbol / ZapfDingbats / unknown → Helvetica metrics (a safe
        // default for any non-text or unrecognised body font).
        _ => {
            if bold {
                Afm::HelvBold
            } else {
                Afm::Helv
            }
        }
    }
}

/// The canonical standard-14 `/BaseFont` name for `font`, used so a synthesised
/// appearance references the same face it is measured with (a conforming viewer
/// then draws the run at the spacing we computed).
pub(crate) fn base_font_name(font: &str) -> &'static str {
    match classify(font) {
        Afm::Helv => "Helvetica",
        Afm::HelvBold => "Helvetica-Bold",
        Afm::Times => "Times-Roman",
        Afm::TimesBold => "Times-Bold",
        Afm::TimesItalic => "Times-Italic",
        Afm::TimesBoldItalic => "Times-BoldItalic",
        Afm::Courier => "Courier",
    }
}

/// Width (in text-space units at `font_size`) of `text` drawn in `font`, summing
/// the Core-14 advance of each glyph. `text` is measured exactly as it is
/// painted — encoded to WinAnsi first (the encoding the synthesised appearances
/// use), so an unrepresentable character counts as `?`.
pub(crate) fn measure_winansi(font: &str, text: &str, font_size: f64) -> f64 {
    let afm = classify(font);
    let total: u32 = super::encode_winansi(text)
        .iter()
        .map(|&code| width_of(afm, winansi_to_char(code)) as u32)
        .sum();
    total as f64 / 1000.0 * font_size
}

fn is_serif(afm: Afm) -> bool {
    matches!(
        afm,
        Afm::Times | Afm::TimesBold | Afm::TimesItalic | Afm::TimesBoldItalic
    )
}

/// The advance of one character in `afm`, in 1000-unit em.
fn width_of(afm: Afm, c: char) -> u16 {
    if afm == Afm::Courier {
        return 600;
    }
    if let Some(base) = fold_accent(c) {
        return ascii_width(afm, base);
    }
    if ('\u{20}'..='\u{7E}').contains(&c) {
        return ascii_width(afm, c);
    }
    special_width(afm, c)
}

/// The advance of an ASCII printable (`0x20..=0x7E`) in `afm`.
fn ascii_width(afm: Afm, c: char) -> u16 {
    let table: &[u16; 95] = match afm {
        Afm::Helv => &HELV,
        Afm::HelvBold => &HELV_BOLD,
        Afm::Times => &TIMES,
        Afm::TimesBold => &TIMES_BOLD,
        Afm::TimesItalic => &TIMES_ITALIC,
        Afm::TimesBoldItalic => &TIMES_BOLD_ITALIC,
        Afm::Courier => return 600,
    };
    let idx = (c as usize).saturating_sub(0x20);
    // Fall back to lowercase-'n' width for anything out of the ASCII range.
    table
        .get(idx)
        .copied()
        .unwrap_or(table['n' as usize - 0x20])
}

/// Map an accented Latin letter to its base ASCII letter. In every Core-14 face
/// the accented form has the **same advance** as its base, so this fold is exact.
/// Returns `None` for non-letters and the special letters (Æ, Ø, Œ, ß, Þ, ð…)
/// that have their own advance (handled by [`special_width`]).
fn fold_accent(c: char) -> Option<char> {
    Some(match c {
        'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' => 'A',
        'Ç' => 'C',
        'È' | 'É' | 'Ê' | 'Ë' => 'E',
        'Ì' | 'Í' | 'Î' | 'Ï' => 'I',
        'Ñ' => 'N',
        'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' => 'O',
        'Ù' | 'Ú' | 'Û' | 'Ü' => 'U',
        'Ý' | 'Ÿ' => 'Y',
        'Š' => 'S',
        'Ž' => 'Z',
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => 'a',
        'ç' => 'c',
        'è' | 'é' | 'ê' | 'ë' => 'e',
        'ì' | 'í' | 'î' | 'ï' => 'i',
        'ñ' => 'n',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' => 'o',
        'ù' | 'ú' | 'û' | 'ü' => 'u',
        'ý' | 'ÿ' => 'y',
        'š' => 's',
        'ž' => 'z',
        _ => return None,
    })
}

/// The advance of a WinAnsi punctuation/symbol code (and the special letters that
/// do not fold to a base ASCII letter), in 1000-unit em. Distinguishes the serif
/// (Times) from the sans (Helvetica) family; weight differences for these rare
/// symbols are ignored (regular-weight AFM values), which only affects the
/// horizontal placement of a line by a fraction of a point.
fn special_width(afm: Afm, c: char) -> u16 {
    let serif = is_serif(afm);
    let s = |t: u16, h: u16| if serif { t } else { h };
    match c {
        '€' => s(500, 556),
        '\u{201A}' => s(333, 222), // ‚ single low quote
        'ƒ' => s(500, 556),
        '\u{201E}' => s(444, 333), // „ double low quote
        '…' => 1000,
        '†' => s(500, 556),
        '‡' => s(500, 556),
        'ˆ' => 333,
        '‰' => 1000,
        '‹' => 333,
        'Œ' => s(889, 1000),
        '\u{2018}' | '\u{2019}' => s(333, 222), // ‘ ’
        '\u{201C}' | '\u{201D}' => s(444, 333), // “ ”
        '•' => 350,
        '–' => s(500, 556),
        '—' => 1000,
        '˜' => 333,
        '™' => s(980, 1000),
        '›' => 333,
        'œ' => s(722, 944),
        '\u{00A0}' => s(250, 278), // no-break space
        '¡' => 333,
        '¢' => s(500, 556),
        '£' => s(500, 556),
        '¤' => s(500, 556),
        '¥' => s(500, 556),
        '¦' => s(200, 260),
        '§' => s(500, 556),
        '¨' => 333,
        '©' => s(760, 737),
        'ª' => s(276, 370),
        '«' => s(500, 556),
        '¬' => s(564, 584),
        '\u{00AD}' => 333, // soft hyphen
        '®' => s(760, 737),
        '¯' => 333,
        '°' => 400,
        '±' => s(564, 584),
        '²' | '³' | '¹' => s(300, 333),
        '´' => 333,
        'µ' => s(500, 556),
        '¶' => s(453, 537),
        '·' => s(250, 278),
        '¸' => 333,
        'º' => s(310, 365),
        '»' => s(500, 556),
        '¼' | '½' | '¾' => s(750, 834),
        '¿' => s(444, 611),
        'Æ' => s(889, 1000),
        'Ð' => 722,
        '×' => s(564, 584),
        'Ø' => s(722, 778),
        'Þ' => s(556, 667),
        'ß' => s(500, 611),
        'æ' => s(667, 889),
        'ð' => s(500, 556),
        '÷' => s(564, 584),
        'ø' => s(500, 611),
        'þ' => s(500, 556),
        _ => ascii_width(afm, 'n'),
    }
}

// ── Core-14 ASCII (`0x20..=0x7E`) advance tables, 1000-unit em ────────────────

#[rustfmt::skip]
const HELV: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278, // 0x20-0x2F
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 278, 278, 584, 584, 584, 556, // 0x30-0x3F
    1015, 667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667, 556, 833, 722, 778, // 0x40-0x4F
    667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 278, 278, 278, 469, 556, // 0x50-0x5F
    333, 556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500, 222, 833, 556, 556, // 0x60-0x6F
    556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500, 334, 260, 334, 584,       // 0x70-0x7E
];

#[rustfmt::skip]
const HELV_BOLD: [u16; 95] = [
    278, 333, 474, 556, 556, 889, 722, 238, 333, 333, 389, 584, 278, 333, 278, 278,
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 333, 333, 584, 584, 584, 611,
    975, 722, 722, 722, 722, 667, 611, 778, 722, 278, 556, 722, 611, 833, 722, 778,
    667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 333, 278, 333, 584, 556,
    333, 556, 611, 556, 611, 556, 333, 611, 611, 278, 278, 556, 278, 889, 611, 611,
    611, 611, 389, 556, 333, 611, 556, 778, 556, 556, 500, 389, 280, 389, 584,
];

#[rustfmt::skip]
const TIMES: [u16; 95] = [
    250, 333, 408, 500, 500, 833, 778, 180, 333, 333, 500, 564, 250, 333, 250, 278,
    500, 500, 500, 500, 500, 500, 500, 500, 500, 500, 278, 278, 564, 564, 564, 444,
    921, 722, 667, 667, 722, 611, 556, 722, 722, 333, 389, 722, 611, 889, 722, 722,
    556, 722, 667, 556, 611, 722, 722, 944, 722, 722, 611, 333, 278, 333, 469, 500,
    333, 444, 500, 444, 500, 444, 333, 500, 500, 278, 278, 500, 278, 778, 500, 500,
    500, 500, 333, 389, 278, 500, 500, 722, 500, 500, 444, 480, 200, 480, 541,
];

#[rustfmt::skip]
const TIMES_BOLD: [u16; 95] = [
    250, 333, 555, 500, 500, 1000, 833, 278, 333, 333, 500, 570, 250, 333, 250, 278,
    500, 500, 500, 500, 500, 500, 500, 500, 500, 500, 333, 333, 570, 570, 570, 500,
    930, 722, 667, 722, 722, 667, 611, 778, 778, 389, 500, 778, 667, 944, 722, 778,
    611, 778, 722, 556, 667, 722, 722, 1000, 722, 722, 667, 333, 278, 333, 581, 500,
    333, 500, 556, 444, 556, 444, 333, 500, 556, 278, 333, 556, 278, 833, 556, 500,
    556, 556, 444, 389, 333, 556, 500, 722, 500, 500, 444, 394, 220, 394, 520,
];

#[rustfmt::skip]
const TIMES_ITALIC: [u16; 95] = [
    250, 333, 420, 500, 500, 833, 778, 214, 333, 333, 500, 675, 250, 333, 250, 278,
    500, 500, 500, 500, 500, 500, 500, 500, 500, 500, 333, 333, 675, 675, 675, 500,
    920, 611, 611, 667, 722, 611, 611, 722, 722, 333, 444, 667, 556, 833, 667, 722,
    611, 722, 611, 500, 556, 722, 611, 833, 611, 556, 556, 389, 278, 389, 422, 500,
    333, 500, 500, 444, 500, 444, 278, 500, 500, 278, 278, 444, 278, 722, 500, 500,
    500, 500, 389, 389, 278, 500, 444, 667, 444, 444, 389, 400, 275, 400, 541,
];

#[rustfmt::skip]
const TIMES_BOLD_ITALIC: [u16; 95] = [
    250, 389, 555, 500, 500, 833, 778, 278, 333, 333, 500, 570, 250, 333, 250, 278,
    500, 500, 500, 500, 500, 500, 500, 500, 500, 500, 333, 333, 570, 570, 570, 500,
    832, 667, 667, 667, 722, 667, 667, 722, 778, 389, 500, 667, 611, 889, 722, 722,
    611, 722, 667, 556, 611, 722, 667, 889, 667, 611, 611, 333, 278, 333, 570, 500,
    333, 500, 500, 444, 500, 444, 333, 500, 556, 278, 278, 500, 278, 778, 556, 500,
    500, 500, 389, 389, 278, 556, 444, 667, 500, 444, 389, 348, 220, 348, 570,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_da_font_names() {
        assert_eq!(classify("Helv"), Afm::Helv);
        assert_eq!(classify("Helvetica"), Afm::Helv);
        assert_eq!(classify("Helvetica-Bold"), Afm::HelvBold);
        assert_eq!(classify("HeBo"), Afm::HelvBold);
        assert_eq!(classify("HeOb"), Afm::Helv); // oblique == regular advances
        assert_eq!(classify("Arial,Bold"), Afm::HelvBold);
        assert_eq!(classify("Times-Roman"), Afm::Times);
        assert_eq!(classify("TiRo"), Afm::Times);
        assert_eq!(classify("Times-BoldItalic"), Afm::TimesBoldItalic);
        assert_eq!(classify("TiBI"), Afm::TimesBoldItalic);
        assert_eq!(classify("Courier"), Afm::Courier);
        assert_eq!(classify("CoBo"), Afm::Courier);
        assert_eq!(classify("F1"), Afm::Helv); // unknown → Helvetica
    }

    #[test]
    fn ascii_widths_match_published_afm() {
        // A few well-known Helvetica metrics.
        assert_eq!(width_of(Afm::Helv, ' '), 278);
        assert_eq!(width_of(Afm::Helv, 'A'), 667);
        assert_eq!(width_of(Afm::Helv, 'W'), 944);
        assert_eq!(width_of(Afm::Helv, 'i'), 222);
        assert_eq!(width_of(Afm::Helv, '@'), 1015);
        // Bold differs.
        assert_eq!(width_of(Afm::HelvBold, 'A'), 722);
        // Times.
        assert_eq!(width_of(Afm::Times, 'A'), 722);
        assert_eq!(width_of(Afm::Times, ' '), 250);
        // Courier is monospaced.
        assert_eq!(width_of(Afm::Courier, 'A'), 600);
        assert_eq!(width_of(Afm::Courier, '.'), 600);
    }

    #[test]
    fn accented_latin_folds_to_base_letter() {
        // Exact identity in the Core-14 AFMs.
        assert_eq!(width_of(Afm::Helv, 'É'), width_of(Afm::Helv, 'E'));
        assert_eq!(width_of(Afm::Helv, 'é'), width_of(Afm::Helv, 'e'));
        assert_eq!(width_of(Afm::Times, 'À'), width_of(Afm::Times, 'A'));
        assert_eq!(width_of(Afm::Helv, 'Ÿ'), width_of(Afm::Helv, 'Y'));
    }

    #[test]
    fn winansi_specials_have_widths() {
        assert_eq!(width_of(Afm::Helv, '€'), 556);
        assert_eq!(width_of(Afm::Helv, '•'), 350);
        assert_eq!(width_of(Afm::Helv, '—'), 1000);
        assert_eq!(width_of(Afm::Helv, '\u{00A0}'), 278); // nbsp == space
    }

    #[test]
    fn measure_sums_real_advances() {
        // "Hello" in Helvetica 12pt: (722+556+222+222+556)/1000*12.
        let expected = (722.0 + 556.0 + 222.0 + 222.0 + 556.0) / 1000.0 * 12.0;
        let got = measure_winansi("Helv", "Hello", 12.0);
        assert!((got - expected).abs() < 1e-9, "got {got}, want {expected}");
        // Differs from the old crude `chars*size*0.5` estimate (5*12*0.5 = 30).
        assert!(got < 30.0);
        // Empty text measures zero.
        assert_eq!(measure_winansi("Helv", "", 12.0), 0.0);
    }

    #[test]
    fn base_font_name_round_trips() {
        assert_eq!(base_font_name("Helv"), "Helvetica");
        assert_eq!(base_font_name("HeBo"), "Helvetica-Bold");
        assert_eq!(base_font_name("Times-Italic"), "Times-Italic");
        assert_eq!(base_font_name("Courier"), "Courier");
        assert_eq!(base_font_name("Unknown"), "Helvetica");
    }
}
