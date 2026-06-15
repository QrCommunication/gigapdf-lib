//! Text style recovered from a PDF font, for the Office exporters.
//!
//! PDF text carries no "bold"/"italic" flags — that information is encoded in
//! the font's `/BaseFont` name (e.g. `Helvetica-BoldOblique`, `ABCDEF+Arial,Italic`).
//! [`parse_base_font`] turns such a name into a [`TextStyle`] (display family,
//! generic class, bold, italic); the fill colour is attached separately by the
//! extractor.

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
}

impl TextStyle {
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
}
