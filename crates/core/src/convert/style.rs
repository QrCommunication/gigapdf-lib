//! Text style recovered from a PDF font, for the Office exporters.
//!
//! PDF text carries no "bold"/"italic" flags — that information is encoded in
//! the font's `/BaseFont` name (e.g. `Helvetica-BoldOblique`, `ABCDEF+Arial,Italic`).
//! [`parse_base_font`] turns such a name into a [`TextStyle`] (display family,
//! generic class, bold, italic); the fill colour is attached separately by the
//! extractor.
//!
//! The name alone is unreliable: subset-prefixed or renamed fonts frequently
//! drop the "Bold"/"Italic" tokens. [`FontDescriptorStyle`] carries the matching
//! signals from the font's `/FontDescriptor` (ISO 32000-1 Table 121 `/Flags`,
//! `/FontWeight`, `/StemV`, `/ItalicAngle`); [`TextStyle::refine_with_descriptor`]
//! ORs them in so bold/italic survive even when the name is silent.

/// Generic font class, used as a portable fallback when the exact family is not
/// installed on the reader.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Generic {
    #[default]
    Sans,
    Serif,
    Mono,
}

impl Generic {
    /// CSS / ODF generic family keyword.
    pub fn css(self) -> &'static str {
        match self {
            Generic::Sans => "sans-serif",
            Generic::Serif => "serif",
            Generic::Mono => "monospace",
        }
    }
}

/// A text run's recovered style.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TextStyle {
    /// Display family name (e.g. "Helvetica", "Times New Roman").
    pub family: String,
    /// Generic fallback class.
    pub generic: Generic,
    pub bold: bool,
    pub italic: bool,
    /// RGB fill colour `0..=1`, `None` = default black.
    pub color: Option<[f64; 3]>,
    /// RGB text-highlight / run background `0..=1`, `None` = no highlight.
    /// Painted as a filled rectangle behind the run (a word-processor highlight).
    /// Default `None` keeps every existing run byte-for-byte unchanged.
    pub background: Option<[f64; 3]>,
}

/// Bold/italic signals read from a font's `/FontDescriptor`, used to refine a
/// [`TextStyle`] whose `/BaseFont` name omitted the style tokens (very common for
/// subset-prefixed or renamed fonts). All fields are optional — absent keys leave
/// the corresponding signal silent. Populated by the document layer, which is the
/// only place the descriptor dictionary is reachable.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FontDescriptorStyle {
    /// `/Flags` bit field (ISO 32000-1 Table 121). Bit positions are 1-indexed in
    /// the spec; here as a 0-indexed mask: ForceBold = bit 19 (`1 << 18`),
    /// Italic = bit 7 (`1 << 6`).
    pub flags: Option<u32>,
    /// `/FontWeight` (CSS-style numeric weight, 100–900). `>= 600` ⇒ bold.
    pub font_weight: Option<f64>,
    /// `/StemV` — vertical stem thickness in glyph space (1000/em, so size-
    /// independent). Used only as a last-resort bold heuristic.
    pub stem_v: Option<f64>,
    /// `/ItalicAngle` in degrees. A non-zero angle ⇒ italic/oblique.
    pub italic_angle: Option<f64>,
}

/// `/Flags` bit 19 (1-indexed) — ForceBold. The glyphs are intended to be bold.
const FLAG_FORCE_BOLD: u32 = 1 << 18;
/// `/Flags` bit 7 (1-indexed) — Italic. The glyphs have dominant vertical strokes
/// that are slanted.
const FLAG_ITALIC: u32 = 1 << 6;
/// `/FontWeight` at or above which a font counts as bold (CSS "semibold"+).
const BOLD_WEIGHT_MIN: f64 = 600.0;
/// `/StemV` at or above which a font is treated as bold *only* when every other
/// signal (name token, ForceBold flag, `/FontWeight`) is silent. Normal-weight
/// faces sit ~50–95; bold faces ~120+. Kept conservative to avoid false bold.
const BOLD_STEMV_MIN: f64 = 120.0;

impl TextStyle {
    /// Fold a font's `/FontDescriptor` signals into this style: bold/italic are
    /// only ever *added*, never cleared, so the name-based detection in
    /// [`parse_base_font`] stays authoritative when it already fired.
    ///
    /// * `/Flags` ForceBold ⇒ bold, Italic ⇒ italic;
    /// * `/FontWeight >= 600` ⇒ bold;
    /// * `/ItalicAngle != 0` ⇒ italic;
    /// * `/StemV >= 120` ⇒ bold, but **only** as a fallback when the name, the
    ///   ForceBold flag and `/FontWeight` were all silent (a deliberately
    ///   conservative last resort).
    pub fn refine_with_descriptor(&mut self, desc: &FontDescriptorStyle) {
        let flag_bold = desc.flags.is_some_and(|f| f & FLAG_FORCE_BOLD != 0);
        let flag_italic = desc.flags.is_some_and(|f| f & FLAG_ITALIC != 0);
        let weight_bold = desc.font_weight.is_some_and(|w| w >= BOLD_WEIGHT_MIN);
        let angle_italic = desc.italic_angle.is_some_and(|a| a != 0.0);

        // StemV is the weakest signal: only consult it when nothing stronger
        // (name token / ForceBold flag / FontWeight) has already established bold.
        let stem_bold = !(self.bold || flag_bold || weight_bold)
            && desc.stem_v.is_some_and(|s| s >= BOLD_STEMV_MIN);

        self.bold = self.bold || flag_bold || weight_bold || stem_bold;
        self.italic = self.italic || flag_italic || angle_italic;
    }

    /// The colour as a `RRGGBB` hex string, or `000000` when unset.
    pub fn hex_color(&self) -> String {
        let [r, g, b] = self.color.unwrap_or([0.0, 0.0, 0.0]);
        let q = |c: f64| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
        format!("{:02X}{:02X}{:02X}", q(r), q(g), q(b))
    }

    /// True when the colour is set and not (near-)black — worth emitting.
    pub fn has_visible_color(&self) -> bool {
        match self.color {
            Some([r, g, b]) => r > 0.02 || g > 0.02 || b > 0.02,
            None => false,
        }
    }
}

/// Strip a subset prefix like `ABCDEF+` (six uppercase letters + `+`).
fn strip_subset(name: &str) -> &str {
    let bytes = name.as_bytes();
    if bytes.len() > 7 && bytes[6] == b'+' && bytes[..6].iter().all(|b| b.is_ascii_uppercase()) {
        &name[7..]
    } else {
        name
    }
}

/// Parse a PDF `/BaseFont` name into a [`TextStyle`] (colour left default).
pub fn parse_base_font(base_font: &str) -> TextStyle {
    let name = strip_subset(base_font);
    let lower = name.to_lowercase();

    let bold = lower.contains("bold")
        || lower.contains("black")
        || lower.contains("heavy")
        || lower.contains("semibold");
    let italic = lower.contains("italic") || lower.contains("oblique");

    // Map well-known faces to a display family + generic class; otherwise derive
    // the family from the part before the first style separator.
    let (family, generic) = if lower.contains("times") {
        ("Times New Roman".to_string(), Generic::Serif)
    } else if lower.contains("georgia") {
        ("Georgia".to_string(), Generic::Serif)
    } else if lower.contains("garamond") {
        ("Garamond".to_string(), Generic::Serif)
    } else if lower.contains("courier") || lower.contains("mono") {
        ("Courier New".to_string(), Generic::Mono)
    } else if lower.contains("consolas") {
        ("Consolas".to_string(), Generic::Mono)
    } else if lower.contains("arial") {
        ("Arial".to_string(), Generic::Sans)
    } else if lower.contains("helvetica") {
        ("Helvetica".to_string(), Generic::Sans)
    } else if lower.contains("calibri") {
        ("Calibri".to_string(), Generic::Sans)
    } else if lower.contains("verdana") {
        ("Verdana".to_string(), Generic::Sans)
    } else {
        // Family = text up to the first '-' or ',', cleaned.
        let cut = name.find(['-', ',']).unwrap_or(name.len());
        let raw = name[..cut].trim();
        let family = if raw.is_empty() {
            "Helvetica".to_string()
        } else {
            raw.to_string()
        };
        let generic = if lower.contains("serif") && !lower.contains("sans") {
            Generic::Serif
        } else {
            Generic::Sans
        };
        (family, generic)
    };

    TextStyle {
        family,
        generic,
        bold,
        italic,
        color: None,
        background: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_style_and_family() {
        let s = parse_base_font("Helvetica-BoldOblique");
        assert_eq!(s.family, "Helvetica");
        assert_eq!(s.generic, Generic::Sans);
        assert!(s.bold && s.italic);

        let t = parse_base_font("ABCDEF+Times-Italic");
        assert_eq!(t.family, "Times New Roman");
        assert_eq!(t.generic, Generic::Serif);
        assert!(t.italic && !t.bold);

        let c = parse_base_font("CourierNewPSMT");
        assert_eq!(c.generic, Generic::Mono);
    }

    #[test]
    fn hex_color_quantizes() {
        let mut s = TextStyle::default();
        assert_eq!(s.hex_color(), "000000");
        s.color = Some([1.0, 0.0, 0.0]);
        assert_eq!(s.hex_color(), "FF0000");
        assert!(s.has_visible_color());
    }

    #[test]
    fn unknown_font_falls_back_to_family_stem() {
        let s = parse_base_font("FancyFont-Regular");
        assert_eq!(s.family, "FancyFont");
        assert!(!s.bold && !s.italic);
    }

    #[test]
    fn descriptor_force_bold_flag_makes_bold_without_name_token() {
        // Subset-renamed font: name says nothing, ForceBold flag (bit 19) set.
        let mut s = parse_base_font("ABCDEF+Subset123");
        assert!(!s.bold, "name has no bold token");
        s.refine_with_descriptor(&FontDescriptorStyle {
            flags: Some(FLAG_FORCE_BOLD),
            ..Default::default()
        });
        assert!(s.bold && !s.italic);
    }

    #[test]
    fn descriptor_italic_flag_and_angle_make_italic() {
        let mut s = parse_base_font("ABCDEF+Subset123");
        assert!(!s.italic);
        // Italic flag (bit 7).
        s.refine_with_descriptor(&FontDescriptorStyle {
            flags: Some(FLAG_ITALIC),
            ..Default::default()
        });
        assert!(s.italic && !s.bold);

        // A non-zero /ItalicAngle alone is also enough.
        let mut t = parse_base_font("ABCDEF+Subset123");
        t.refine_with_descriptor(&FontDescriptorStyle {
            italic_angle: Some(-12.0),
            ..Default::default()
        });
        assert!(t.italic && !t.bold);
    }

    #[test]
    fn descriptor_font_weight_threshold_makes_bold() {
        let mut s = parse_base_font("ABCDEF+Subset123");
        s.refine_with_descriptor(&FontDescriptorStyle {
            font_weight: Some(700.0),
            ..Default::default()
        });
        assert!(s.bold);

        // Just below the 600 threshold stays non-bold.
        let mut t = parse_base_font("ABCDEF+Subset123");
        t.refine_with_descriptor(&FontDescriptorStyle {
            font_weight: Some(500.0),
            ..Default::default()
        });
        assert!(!t.bold);
    }

    #[test]
    fn descriptor_no_signals_leaves_style_untouched() {
        let mut s = parse_base_font("ABCDEF+Subset123");
        s.refine_with_descriptor(&FontDescriptorStyle::default());
        assert!(!s.bold && !s.italic);

        // A normal-weight descriptor (regular flags, mid StemV, zero angle) must
        // not flip anything.
        let mut t = parse_base_font("ABCDEF+Subset123");
        t.refine_with_descriptor(&FontDescriptorStyle {
            flags: Some(1 << 5), // Nonsymbolic (bit 6), neither bold nor italic
            font_weight: Some(400.0),
            stem_v: Some(80.0),
            italic_angle: Some(0.0),
        });
        assert!(!t.bold && !t.italic);
    }

    #[test]
    fn descriptor_high_stemv_is_a_conservative_fallback() {
        // High StemV with no other signal ⇒ bold (last-resort heuristic).
        let mut s = parse_base_font("ABCDEF+Subset123");
        s.refine_with_descriptor(&FontDescriptorStyle {
            stem_v: Some(140.0),
            ..Default::default()
        });
        assert!(s.bold);

        // A normal StemV (below the conservative threshold) must NOT make bold —
        // this is the guard against false positives on regular-weight faces.
        let mut t = parse_base_font("ABCDEF+Subset123");
        t.refine_with_descriptor(&FontDescriptorStyle {
            stem_v: Some(95.0),
            ..Default::default()
        });
        assert!(!t.bold);
    }

    #[test]
    fn descriptor_never_clears_name_based_bold_italic() {
        // Name already established bold+italic; an empty descriptor must not undo it.
        let mut s = parse_base_font("Helvetica-BoldOblique");
        assert!(s.bold && s.italic);
        s.refine_with_descriptor(&FontDescriptorStyle::default());
        assert!(s.bold && s.italic);
    }

    #[test]
    fn generic_css_keywords() {
        assert_eq!(Generic::Sans.css(), "sans-serif");
        assert_eq!(Generic::Serif.css(), "serif");
        assert_eq!(Generic::Mono.css(), "monospace");
    }

    #[test]
    fn has_visible_color_thresholds() {
        let mut s = TextStyle::default();
        assert!(!s.has_visible_color(), "None ⇒ not visible");
        s.color = Some([0.0, 0.0, 0.0]);
        assert!(!s.has_visible_color(), "black ⇒ not visible");
        s.color = Some([0.01, 0.0, 0.01]);
        assert!(!s.has_visible_color(), "near-black ⇒ not visible");
        s.color = Some([0.0, 0.5, 0.0]);
        assert!(s.has_visible_color(), "green ⇒ visible");
    }

    #[test]
    fn maps_all_known_serif_families() {
        assert_eq!(parse_base_font("Georgia-Bold").family, "Georgia");
        assert_eq!(parse_base_font("Georgia-Bold").generic, Generic::Serif);
        assert_eq!(parse_base_font("AGaramondPro").family, "Garamond");
        assert_eq!(parse_base_font("AGaramondPro").generic, Generic::Serif);
    }

    #[test]
    fn maps_all_known_mono_and_sans_families() {
        assert_eq!(parse_base_font("Consolas").family, "Consolas");
        assert_eq!(parse_base_font("Consolas").generic, Generic::Mono);
        assert_eq!(parse_base_font("Calibri-Light").family, "Calibri");
        assert_eq!(parse_base_font("Calibri-Light").generic, Generic::Sans);
        assert_eq!(parse_base_font("Verdana").family, "Verdana");
        assert_eq!(parse_base_font("Verdana").generic, Generic::Sans);
    }

    #[test]
    fn unknown_serif_named_font_gets_serif_generic() {
        // No known-family match, but the name contains "serif" (not "sans").
        let s = parse_base_font("MyCustomSerif-Regular");
        assert_eq!(s.family, "MyCustomSerif");
        assert_eq!(s.generic, Generic::Serif);
        // "sans" in the name keeps it Sans even with "serif" present.
        let t = parse_base_font("MyFontSansSerif");
        assert_eq!(t.generic, Generic::Sans);
    }

    #[test]
    fn empty_family_stem_falls_back_to_helvetica() {
        // A name starting with the style separator yields an empty stem → Helvetica.
        let s = parse_base_font("-Italic");
        assert_eq!(s.family, "Helvetica");
        assert!(s.italic);
    }

    #[test]
    fn weight_keyword_variants_make_bold() {
        assert!(parse_base_font("Roboto-Black").bold);
        assert!(parse_base_font("Roboto-Heavy").bold);
        assert!(parse_base_font("Roboto-Semibold").bold);
    }
}
