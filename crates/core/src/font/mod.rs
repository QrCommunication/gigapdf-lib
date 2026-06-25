//! Font encodings — decode/encode the bytes of a text run to/from Unicode.
//!
//! `WinAnsiEncoding` (Windows-1252) is the default for simple Latin PDF fonts
//! and covers the vast majority of office documents. CID/Type0 (2-byte) and
//! `/ToUnicode` CMap handling live in [`cmap`], built on top of this.

pub mod bundled;
pub mod catalog;
pub mod cff;
pub mod cff_to_otf;
pub mod cmap;
pub mod color;
pub mod embed;
pub mod encoding;
pub mod glyphless;
pub mod google;
pub mod shape;
pub mod truetype;
pub mod type1;

mod brotli;
mod brotli_dict;
mod brotli_tables;
#[cfg(test)]
mod brotli_test_vectors;

/// A glyph outline source: an embedded TrueType (`/FontFile2`) or CFF
/// (`/FontFile3`) program. Both expose the same outline interface so the
/// rasterizer treats them uniformly.
#[derive(Debug, Clone)]
pub enum GlyphSource {
    /// A TrueType (`glyf`) program.
    TrueType(truetype::TrueTypeFont),
    /// A CFF / Type2 program.
    Cff(cff::CffFont),
    /// A raw Type 1 (`/FontFile`) program: its `eexec`-decrypted charstrings are
    /// interpreted directly into outlines (see [`type1::Type1Font`]).
    Type1(type1::Type1Font),
}

impl GlyphSource {
    /// Font design units per em.
    pub fn units_per_em(&self) -> f64 {
        match self {
            GlyphSource::TrueType(f) => f.units_per_em(),
            GlyphSource::Cff(f) => f.units_per_em(),
            GlyphSource::Type1(f) => f.units_per_em(),
        }
    }

    /// Glyph advance width in font units.
    pub fn advance_width(&self, gid: u16) -> f64 {
        match self {
            GlyphSource::TrueType(f) => f.advance_width(gid),
            GlyphSource::Cff(f) => f.advance_width(gid),
            GlyphSource::Type1(f) => f.advance_width(gid),
        }
    }

    /// Flattened glyph contours in font units.
    pub fn glyph_polygons(&self, gid: u16) -> Vec<Vec<(f64, f64)>> {
        match self {
            GlyphSource::TrueType(f) => f.glyph_polygons(gid),
            GlyphSource::Cff(f) => f.glyph_polygons(gid),
            GlyphSource::Type1(f) => f.glyph_polygons(gid),
        }
    }

    /// Map a Unicode scalar to a glyph id (TrueType cmap only; CFF and Type 1
    /// return `None`, as their simple fonts map via the PDF encoding/charset and
    /// composite CFF uses the code as the glyph id directly).
    pub fn gid_for_unicode(&self, cp: u32) -> Option<u16> {
        match self {
            GlyphSource::TrueType(f) => f.gid_for_unicode(cp),
            GlyphSource::Cff(_) | GlyphSource::Type1(_) => None,
        }
    }

    /// The CFF program, if this source is one. Lets the PDF layer resolve a
    /// simple font's `/Encoding` against the CFF charset (`code → name → gid`).
    pub fn as_cff(&self) -> Option<&cff::CffFont> {
        match self {
            GlyphSource::Cff(f) => Some(f),
            GlyphSource::TrueType(_) | GlyphSource::Type1(_) => None,
        }
    }

    /// The Type 1 program, if this source is one. Lets the PDF layer resolve a
    /// simple font's `/Encoding` against the Type 1 charstring names
    /// (`code → name → gid`), the same way `as_cff` drives the CFF charset path.
    pub fn as_type1(&self) -> Option<&type1::Type1Font> {
        match self {
            GlyphSource::Type1(f) => Some(f),
            GlyphSource::TrueType(_) | GlyphSource::Cff(_) => None,
        }
    }
}

/// WinAnsi mapping for the 0x80–0x9F range (the only bytes that differ from
/// Latin-1). 0x00–0x7F and 0xA0–0xFF map to the same Unicode scalar value.
const WINANSI_HIGH: [u16; 32] = [
    0x20AC, 0x0081, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, // 0x80–0x87
    0x02C6, 0x2030, 0x0160, 0x2039, 0x0152, 0x008D, 0x017D, 0x008F, // 0x88–0x8F
    0x0090, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, // 0x90–0x97
    0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0x009D, 0x017E, 0x0178, // 0x98–0x9F
];

/// Decode a single WinAnsi byte to its Unicode character.
pub fn winansi_to_char(code: u8) -> char {
    let cp = if (0x80..=0x9F).contains(&code) {
        WINANSI_HIGH[(code - 0x80) as usize] as u32
    } else {
        code as u32
    };
    char::from_u32(cp).unwrap_or('\u{FFFD}')
}

/// Encode a Unicode character to a WinAnsi byte, if representable.
pub fn char_to_winansi(c: char) -> Option<u8> {
    let cp = c as u32;
    if cp <= 0x7F || (0xA0..=0xFF).contains(&cp) {
        return Some(cp as u8);
    }
    WINANSI_HIGH
        .iter()
        .position(|&mapped| mapped as u32 == cp)
        .map(|i| 0x80 + i as u8)
}

/// Decode WinAnsi-encoded bytes to a String.
pub fn decode_winansi(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| winansi_to_char(b)).collect()
}

/// Encode a string to WinAnsi bytes; characters outside WinAnsi become `?`.
pub fn encode_winansi(text: &str) -> Vec<u8> {
    text.chars()
        .map(|c| char_to_winansi(c).unwrap_or(b'?'))
        .collect()
}

/// Decode a PDF *text string* (ISO 32000-1 §7.9.2.2): UTF-16BE when it starts
/// with the `FE FF` byte-order mark, otherwise WinAnsi/PDFDocEncoding.
pub fn decode_pdf_text(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
        let mut out = String::new();
        let mut i = 2;
        while i + 1 < bytes.len() {
            let unit = ((bytes[i] as u16) << 8) | bytes[i + 1] as u16;
            i += 2;
            if (0xD800..=0xDBFF).contains(&unit) && i + 1 < bytes.len() {
                let low = ((bytes[i] as u16) << 8) | bytes[i + 1] as u16;
                if (0xDC00..=0xDFFF).contains(&low) {
                    i += 2;
                    let scalar = 0x10000 + (((unit - 0xD800) as u32) << 10) + (low - 0xDC00) as u32;
                    if let Some(c) = char::from_u32(scalar) {
                        out.push(c);
                    }
                    continue;
                }
            }
            if let Some(c) = char::from_u32(unit as u32) {
                out.push(c);
            }
        }
        out
    } else {
        decode_winansi(bytes)
    }
}

/// Encode a PDF *text string*: WinAnsi when fully representable, otherwise
/// UTF-16BE with a `FE FF` byte-order mark.
pub fn encode_pdf_text(text: &str) -> Vec<u8> {
    if text.chars().all(|c| char_to_winansi(c).is_some()) {
        return encode_winansi(text);
    }
    let mut out = vec![0xFE, 0xFF];
    let mut buf = [0u16; 2];
    for c in text.chars() {
        for unit in c.encode_utf16(&mut buf) {
            out.push((*unit >> 8) as u8);
            out.push((*unit & 0xFF) as u8);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_is_identity() {
        assert_eq!(decode_winansi(b"Hello"), "Hello");
        assert_eq!(encode_winansi("Hello"), b"Hello");
    }

    #[test]
    fn latin1_accents() {
        assert_eq!(decode_winansi(&[0xE9, 0xE8, 0xE0]), "éèà");
        assert_eq!(encode_winansi("éèà"), vec![0xE9, 0xE8, 0xE0]);
    }

    #[test]
    fn windows_specials() {
        assert_eq!(decode_winansi(&[0x80]), "€");
        assert_eq!(encode_winansi("€"), vec![0x80]);
        assert_eq!(decode_winansi(&[0x92]), "\u{2019}"); // right single quote
        assert_eq!(encode_winansi("\u{2019}"), vec![0x92]);
        assert_eq!(decode_winansi(&[0x97]), "—"); // em dash
        assert_eq!(encode_winansi("—"), vec![0x97]);
    }

    #[test]
    fn unrepresentable_becomes_question_mark() {
        assert_eq!(encode_winansi("中"), b"?");
    }

    #[test]
    fn decodes_utf16be_text_string() {
        // FE FF BOM + "Hé" in UTF-16BE.
        let bytes = [0xFE, 0xFF, 0x00, 0x48, 0x00, 0xE9];
        assert_eq!(decode_pdf_text(&bytes), "Hé");
    }

    #[test]
    fn pdf_text_roundtrip_ascii_and_unicode() {
        assert_eq!(decode_pdf_text(&encode_pdf_text("ascii é")), "ascii é");
        assert_eq!(decode_pdf_text(&encode_pdf_text("日本語")), "日本語");
    }
}
