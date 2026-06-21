//! Simple-font `/Encoding` resolution to Unicode (ISO 32000-1 §9.6.6, §D.2).
//!
//! A simple (single-byte, non-CID) PDF font maps each character code to a glyph
//! via a *base encoding* (`WinAnsiEncoding`, `MacRomanEncoding`,
//! `StandardEncoding`, or the font's built-in) optionally overlaid with an
//! `/Encoding` `/Differences` array (`code → glyph name`). To extract readable
//! text without a `/ToUnicode` CMap, the engine resolves each code to Unicode:
//!
//! 1. base encoding: code → Unicode scalar (the three predefined tables here;
//!    `WinAnsiEncoding` reuses [`super::winansi_to_char`]).
//! 2. `/Differences`: code → glyph name → Unicode via the Adobe Glyph List
//!    ([`glyph_name_to_unicode`]) — overrides the base for listed codes.
//!
//! Tables are derived directly from Adobe's published encoding vectors (PDF spec
//! Annex D); only the printable entries with a defined Unicode value are listed,
//! so a code absent here simply has no glyph (omitted from the run).

/// `MacRomanEncoding` codes 0x80–0xFF → Unicode (Adobe, PDF spec Annex D.2).
/// 0x20–0x7E is ASCII (handled by the caller). Mac OS Roman is the historical
/// Macintosh Latin encoding — punctuation and accented letters live in entirely
/// different code slots than WinAnsi, so decoding Mac-encoded bytes as WinAnsi
/// corrupts every accent and curly quote.
const MAC_ROMAN_HIGH: [u16; 128] = [
    // 0x80–0x8F
    0x00C4, 0x00C5, 0x00C7, 0x00C9, 0x00D1, 0x00D6, 0x00DC, 0x00E1, 0x00E0, 0x00E2, 0x00E4, 0x00E3,
    0x00E5, 0x00E7, 0x00E9, 0x00E8,
    // 0x90–0x9F
    0x00EA, 0x00EB, 0x00ED, 0x00EC, 0x00EE, 0x00EF, 0x00F1, 0x00F3, 0x00F2, 0x00F4, 0x00F6, 0x00F5,
    0x00FA, 0x00F9, 0x00FB, 0x00FC,
    // 0xA0–0xAF
    0x2020, 0x00B0, 0x00A2, 0x00A3, 0x00A7, 0x2022, 0x00B6, 0x00DF, 0x00AE, 0x00A9, 0x2122, 0x00B4,
    0x00A8, 0x2260, 0x00C6, 0x00D8,
    // 0xB0–0xBF
    0x221E, 0x00B1, 0x2264, 0x2265, 0x00A5, 0x00B5, 0x2202, 0x2211, 0x220F, 0x03C0, 0x222B, 0x00AA,
    0x00BA, 0x03A9, 0x00E6, 0x00F8,
    // 0xC0–0xCF
    0x00BF, 0x00A1, 0x00AC, 0x221A, 0x0192, 0x2248, 0x2206, 0x00AB, 0x00BB, 0x2026, 0x00A0, 0x00C0,
    0x00C3, 0x00D5, 0x0152, 0x0153,
    // 0xD0–0xDF
    0x2013, 0x2014, 0x201C, 0x201D, 0x2018, 0x2019, 0x00F7, 0x25CA, 0x00FF, 0x0178, 0x2044, 0x20AC,
    0x2039, 0x203A, 0xFB01, 0xFB02,
    // 0xE0–0xEF
    0x2021, 0x00B7, 0x201A, 0x201E, 0x2030, 0x00C2, 0x00CA, 0x00C1, 0x00CB, 0x00C8, 0x00CD, 0x00CE,
    0x00CF, 0x00CC, 0x00D3, 0x00D4,
    // 0xF0–0xFF
    0xF8FF, 0x00D2, 0x00DA, 0x00DB, 0x00D9, 0x0131, 0x02C6, 0x02DC, 0x00AF, 0x02D8, 0x02D9, 0x02DA,
    0x00B8, 0x02DD, 0x02DB, 0x02C7,
];

/// `MacRomanEncoding` code → Unicode character (PDF spec Annex D.2).
pub fn mac_roman_to_char(code: u8) -> char {
    if code < 0x80 {
        return char::from_u32(code as u32).unwrap_or('\u{FFFD}');
    }
    char::from_u32(MAC_ROMAN_HIGH[(code - 0x80) as usize] as u32).unwrap_or('\u{FFFD}')
}

/// `StandardEncoding` codes 0x20–0xFF → Unicode (Adobe StandardEncoding, PDF
/// spec Annex D.2), as `(code, scalar)` pairs. Codes absent from the table are
/// undefined in StandardEncoding (no glyph). The lower run mostly matches ASCII
/// except for a few punctuation slots (`'`=0x27 is `quoteright` U+2019,
/// `` ` ``=0x60 is `quoteleft` U+2018), which is why a Standard-encoded font
/// decoded as ASCII/WinAnsi gets its quotes wrong.
const STANDARD_PAIRS: &[(u8, u16)] = &[
    (0x27, 0x2019),
    (0x60, 0x2018),
    (0xA1, 0x00A1),
    (0xA2, 0x00A2),
    (0xA3, 0x00A3),
    (0xA4, 0x2044),
    (0xA5, 0x00A5),
    (0xA6, 0x0192),
    (0xA7, 0x00A7),
    (0xA8, 0x00A4),
    (0xA9, 0x0027),
    (0xAA, 0x201C),
    (0xAB, 0x00AB),
    (0xAC, 0x2039),
    (0xAD, 0x203A),
    (0xAE, 0xFB01),
    (0xAF, 0xFB02),
    (0xB1, 0x2013),
    (0xB2, 0x2020),
    (0xB3, 0x2021),
    (0xB4, 0x00B7),
    (0xB6, 0x00B6),
    (0xB7, 0x2022),
    (0xB8, 0x201A),
    (0xB9, 0x201E),
    (0xBA, 0x201D),
    (0xBB, 0x00BB),
    (0xBC, 0x2026),
    (0xBD, 0x2030),
    (0xBF, 0x00BF),
    (0xC1, 0x0060),
    (0xC2, 0x00B4),
    (0xC3, 0x02C6),
    (0xC4, 0x02DC),
    (0xC5, 0x00AF),
    (0xC6, 0x02D8),
    (0xC7, 0x02D9),
    (0xC8, 0x00A8),
    (0xCA, 0x02DA),
    (0xCB, 0x00B8),
    (0xCD, 0x02DD),
    (0xCE, 0x02DB),
    (0xCF, 0x02C7),
    (0xD0, 0x2014),
    (0xE1, 0x00C6),
    (0xE3, 0x00AA),
    (0xE8, 0x0141),
    (0xE9, 0x00D8),
    (0xEA, 0x0152),
    (0xEB, 0x00BA),
    (0xF1, 0x00E6),
    (0xF5, 0x0131),
    (0xF8, 0x0142),
    (0xF9, 0x00F8),
    (0xFA, 0x0153),
    (0xFB, 0x00DF),
];

/// `StandardEncoding` code → Unicode character, or `None` when undefined. The
/// printable ASCII run 0x20–0x7E maps to itself except the two overridden quote
/// slots; high codes come from [`STANDARD_PAIRS`].
pub fn standard_encoding_to_char(code: u8) -> Option<char> {
    if let Some(&(_, cp)) = STANDARD_PAIRS.iter().find(|&&(c, _)| c == code) {
        return char::from_u32(cp as u32);
    }
    if (0x20..=0x7E).contains(&code) {
        return char::from_u32(code as u32);
    }
    None
}

/// Which predefined base encoding a simple font's `/Encoding` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseEncoding {
    /// `WinAnsiEncoding` (Windows-1252) — the office default.
    WinAnsi,
    /// `MacRomanEncoding` (Mac OS Roman).
    MacRoman,
    /// `StandardEncoding` (Adobe Standard).
    Standard,
}

impl BaseEncoding {
    /// Resolve a base-encoding name (the `/Encoding` name or `/BaseEncoding`).
    pub fn from_name(name: &[u8]) -> Option<Self> {
        match name {
            b"WinAnsiEncoding" => Some(Self::WinAnsi),
            b"MacRomanEncoding" => Some(Self::MacRoman),
            b"StandardEncoding" | b"PDFDocEncoding" => Some(Self::Standard),
            _ => None,
        }
    }

    /// Decode one base-encoding code to Unicode, or `None` when undefined.
    pub fn to_char(self, code: u8) -> Option<char> {
        match self {
            Self::WinAnsi => Some(super::winansi_to_char(code)),
            Self::MacRoman => Some(mac_roman_to_char(code)),
            Self::Standard => standard_encoding_to_char(code),
        }
    }
}

/// Resolve an Adobe glyph **name** (as found in an `/Encoding` `/Differences`
/// array) to a Unicode string. Handles, in order:
/// - the named glyphs of the standard Latin encodings (the Adobe Glyph List
///   subset used by WinAnsi/MacRoman/Standard — `quoteright`, `agrave`,
///   `bullet`, `fi`, …);
/// - AGL algorithmic names (`uniXXXX(XXXX…)`, `uXXXXXX`) and single-character
///   names, plus the standard f-ligatures — via
///   [`super::cff_to_otf::glyph_name_to_unicode_string`].
///
/// Returns `None` for stylistic/unknown names (the caller then leaves the base
/// encoding in place). A subset prefix (`ABCDEF+name`) and a `.suffix` are
/// stripped before lookup.
pub fn glyph_name_to_unicode(raw: &str) -> Option<String> {
    let name = raw.rsplit('+').next().unwrap_or(raw);
    let name = name.split('.').next().unwrap_or(name);
    if name.is_empty() {
        return None;
    }
    if let Some(&cp) = AGL_NAMES.iter().find(|&&(n, _)| n == name).map(|(_, cp)| cp) {
        return char::from_u32(cp as u32).map(|c| c.to_string());
    }
    super::cff_to_otf::glyph_name_to_unicode_string(name)
}

/// Adobe glyph-name → Unicode for the named glyphs of the standard Latin
/// encodings (WinAnsi/MacRoman/Standard) — the slice of the Adobe Glyph List a
/// French/Latin office document's `/Differences` actually references. Names that
/// are single ASCII letters/digits resolve through the single-character rule in
/// `glyph_name_to_unicode_string`, so they are intentionally omitted here.
const AGL_NAMES: &[(&str, u16)] = &[
    ("space", 0x0020),
    ("exclam", 0x0021),
    ("quotedbl", 0x0022),
    ("numbersign", 0x0023),
    ("dollar", 0x0024),
    ("percent", 0x0025),
    ("ampersand", 0x0026),
    ("quotesingle", 0x0027),
    ("quoteright", 0x2019),
    ("parenleft", 0x0028),
    ("parenright", 0x0029),
    ("asterisk", 0x002A),
    ("plus", 0x002B),
    ("comma", 0x002C),
    ("hyphen", 0x002D),
    ("period", 0x002E),
    ("slash", 0x002F),
    ("colon", 0x003A),
    ("semicolon", 0x003B),
    ("less", 0x003C),
    ("equal", 0x003D),
    ("greater", 0x003E),
    ("question", 0x003F),
    ("at", 0x0040),
    ("bracketleft", 0x005B),
    ("backslash", 0x005C),
    ("bracketright", 0x005D),
    ("asciicircum", 0x005E),
    ("underscore", 0x005F),
    ("grave", 0x0060),
    ("quoteleft", 0x2018),
    ("braceleft", 0x007B),
    ("bar", 0x007C),
    ("braceright", 0x007D),
    ("asciitilde", 0x007E),
    // Punctuation / symbols.
    ("exclamdown", 0x00A1),
    ("cent", 0x00A2),
    ("sterling", 0x00A3),
    ("currency", 0x00A4),
    ("yen", 0x00A5),
    ("brokenbar", 0x00A6),
    ("section", 0x00A7),
    ("dieresis", 0x00A8),
    ("copyright", 0x00A9),
    ("ordfeminine", 0x00AA),
    ("guillemotleft", 0x00AB),
    ("logicalnot", 0x00AC),
    ("registered", 0x00AE),
    ("macron", 0x00AF),
    ("degree", 0x00B0),
    ("plusminus", 0x00B1),
    ("twosuperior", 0x00B2),
    ("threesuperior", 0x00B3),
    ("acute", 0x00B4),
    ("mu", 0x00B5),
    ("paragraph", 0x00B6),
    ("periodcentered", 0x00B7),
    ("cedilla", 0x00B8),
    ("onesuperior", 0x00B9),
    ("ordmasculine", 0x00BA),
    ("guillemotright", 0x00BB),
    ("onequarter", 0x00BC),
    ("onehalf", 0x00BD),
    ("threequarters", 0x00BE),
    ("questiondown", 0x00BF),
    ("multiply", 0x00D7),
    ("divide", 0x00F7),
    ("florin", 0x0192),
    ("circumflex", 0x02C6),
    ("tilde", 0x02DC),
    ("breve", 0x02D8),
    ("dotaccent", 0x02D9),
    ("ring", 0x02DA),
    ("ogonek", 0x02DB),
    ("hungarumlaut", 0x02DD),
    ("caron", 0x02C7),
    ("endash", 0x2013),
    ("emdash", 0x2014),
    ("quotedblleft", 0x201C),
    ("quotedblright", 0x201D),
    ("quotedblbase", 0x201E),
    ("quotesinglbase", 0x201A),
    ("guilsinglleft", 0x2039),
    ("guilsinglright", 0x203A),
    ("dagger", 0x2020),
    ("daggerdbl", 0x2021),
    ("bullet", 0x2022),
    ("ellipsis", 0x2026),
    ("perthousand", 0x2030),
    ("fraction", 0x2044),
    ("Euro", 0x20AC),
    ("trademark", 0x2122),
    ("partialdiff", 0x2202),
    ("minus", 0x2212),
    ("fi", 0xFB01),
    ("fl", 0xFB02),
    // Accented Latin capitals.
    ("Agrave", 0x00C0),
    ("Aacute", 0x00C1),
    ("Acircumflex", 0x00C2),
    ("Atilde", 0x00C3),
    ("Adieresis", 0x00C4),
    ("Aring", 0x00C5),
    ("AE", 0x00C6),
    ("Ccedilla", 0x00C7),
    ("Egrave", 0x00C8),
    ("Eacute", 0x00C9),
    ("Ecircumflex", 0x00CA),
    ("Edieresis", 0x00CB),
    ("Igrave", 0x00CC),
    ("Iacute", 0x00CD),
    ("Icircumflex", 0x00CE),
    ("Idieresis", 0x00CF),
    ("Eth", 0x00D0),
    ("Ntilde", 0x00D1),
    ("Ograve", 0x00D2),
    ("Oacute", 0x00D3),
    ("Ocircumflex", 0x00D4),
    ("Otilde", 0x00D5),
    ("Odieresis", 0x00D6),
    ("Oslash", 0x00D8),
    ("Ugrave", 0x00D9),
    ("Uacute", 0x00DA),
    ("Ucircumflex", 0x00DB),
    ("Udieresis", 0x00DC),
    ("Yacute", 0x00DD),
    ("Thorn", 0x00DE),
    ("OE", 0x0152),
    ("Scaron", 0x0160),
    ("Ydieresis", 0x0178),
    ("Zcaron", 0x017D),
    ("Lslash", 0x0141),
    ("Idotaccent", 0x0130),
    // Accented Latin smalls.
    ("germandbls", 0x00DF),
    ("agrave", 0x00E0),
    ("aacute", 0x00E1),
    ("acircumflex", 0x00E2),
    ("atilde", 0x00E3),
    ("adieresis", 0x00E4),
    ("aring", 0x00E5),
    ("ae", 0x00E6),
    ("ccedilla", 0x00E7),
    ("egrave", 0x00E8),
    ("eacute", 0x00E9),
    ("ecircumflex", 0x00EA),
    ("edieresis", 0x00EB),
    ("igrave", 0x00EC),
    ("iacute", 0x00ED),
    ("icircumflex", 0x00EE),
    ("idieresis", 0x00EF),
    ("eth", 0x00F0),
    ("ntilde", 0x00F1),
    ("ograve", 0x00F2),
    ("oacute", 0x00F3),
    ("ocircumflex", 0x00F4),
    ("otilde", 0x00F5),
    ("odieresis", 0x00F6),
    ("oslash", 0x00F8),
    ("ugrave", 0x00F9),
    ("uacute", 0x00FA),
    ("ucircumflex", 0x00FB),
    ("udieresis", 0x00FC),
    ("yacute", 0x00FD),
    ("thorn", 0x00FE),
    ("ydieresis", 0x00FF),
    ("oe", 0x0153),
    ("scaron", 0x0161),
    ("zcaron", 0x017E),
    ("dotlessi", 0x0131),
    ("lslash", 0x0142),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_roman_accents_and_quotes() {
        assert_eq!(mac_roman_to_char(0x88), 'à'); // 0x88 = a-grave in MacRoman
        assert_eq!(mac_roman_to_char(0x8E), 'é'); // 0x8E = e-acute
        assert_eq!(mac_roman_to_char(0xD5), '\u{2019}'); // 0xD5 = right single quote
        assert_eq!(mac_roman_to_char(b'J'), 'J'); // ASCII passes through
    }

    #[test]
    fn standard_encoding_quotes() {
        assert_eq!(standard_encoding_to_char(0x27), Some('\u{2019}')); // quoteright
        assert_eq!(standard_encoding_to_char(0x60), Some('\u{2018}')); // quoteleft
        assert_eq!(standard_encoding_to_char(b'A'), Some('A'));
    }

    #[test]
    fn glyph_names_resolve() {
        assert_eq!(glyph_name_to_unicode("quoteright").as_deref(), Some("\u{2019}"));
        assert_eq!(glyph_name_to_unicode("agrave").as_deref(), Some("à"));
        assert_eq!(glyph_name_to_unicode("eacute").as_deref(), Some("é"));
        assert_eq!(glyph_name_to_unicode("bullet").as_deref(), Some("\u{2022}"));
        assert_eq!(glyph_name_to_unicode("fi").as_deref(), Some("\u{FB01}"));
        // Algorithmic + single-char fall through to the AGL resolver.
        assert_eq!(glyph_name_to_unicode("uni00E9").as_deref(), Some("é"));
        assert_eq!(glyph_name_to_unicode("A").as_deref(), Some("A"));
        // Subset prefix + suffix stripped.
        assert_eq!(glyph_name_to_unicode("ABCDEF+agrave.sc").as_deref(), Some("à"));
        // Unknown stylistic name → None.
        assert_eq!(glyph_name_to_unicode("foobar"), None);
    }

    #[test]
    fn base_encoding_from_name() {
        assert_eq!(BaseEncoding::from_name(b"WinAnsiEncoding"), Some(BaseEncoding::WinAnsi));
        assert_eq!(BaseEncoding::from_name(b"MacRomanEncoding"), Some(BaseEncoding::MacRoman));
        assert_eq!(BaseEncoding::from_name(b"StandardEncoding"), Some(BaseEncoding::Standard));
        assert_eq!(BaseEncoding::from_name(b"Identity-H"), None);
    }
}
