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

/// Adobe **Symbol** font built-in encoding: `(code, Unicode scalar)` pairs
/// (Adobe `symbol.txt`, PDF spec Annex D.5). Symbol has its *own* code layout —
/// Greek letters, mathematical operators and arrows — so its bytes must **not**
/// be decoded as WinAnsi (where e.g. `0x61 'a'` is a Latin 'a', but in Symbol it
/// is α U+03B1). Used by [`symbol_to_char`] and the `Symbol` base encoding.
#[rustfmt::skip]
const SYMBOL_PAIRS: &[(u8, u16)] = &[
    (0x20,0x0020),(0x21,0x0021),(0x22,0x2200),(0x23,0x0023),(0x24,0x2203),(0x25,0x0025),
    (0x26,0x0026),(0x27,0x220B),(0x28,0x0028),(0x29,0x0029),(0x2A,0x2217),(0x2B,0x002B),
    (0x2C,0x002C),(0x2D,0x2212),(0x2E,0x002E),(0x2F,0x002F),(0x30,0x0030),(0x31,0x0031),
    (0x32,0x0032),(0x33,0x0033),(0x34,0x0034),(0x35,0x0035),(0x36,0x0036),(0x37,0x0037),
    (0x38,0x0038),(0x39,0x0039),(0x3A,0x003A),(0x3B,0x003B),(0x3C,0x003C),(0x3D,0x003D),
    (0x3E,0x003E),(0x3F,0x003F),(0x40,0x2245),(0x41,0x0391),(0x42,0x0392),(0x43,0x03A7),
    (0x44,0x0394),(0x45,0x0395),(0x46,0x03A6),(0x47,0x0393),(0x48,0x0397),(0x49,0x0399),
    (0x4A,0x03D1),(0x4B,0x039A),(0x4C,0x039B),(0x4D,0x039C),(0x4E,0x039D),(0x4F,0x039F),
    (0x50,0x03A0),(0x51,0x0398),(0x52,0x03A1),(0x53,0x03A3),(0x54,0x03A4),(0x55,0x03A5),
    (0x56,0x03C2),(0x57,0x2126),(0x58,0x039E),(0x59,0x03A8),(0x5A,0x0396),(0x5B,0x005B),
    (0x5C,0x2234),(0x5D,0x005D),(0x5E,0x22A5),(0x5F,0x005F),(0x60,0xF8E5),(0x61,0x03B1),
    (0x62,0x03B2),(0x63,0x03C7),(0x64,0x03B4),(0x65,0x03B5),(0x66,0x03C6),(0x67,0x03B3),
    (0x68,0x03B7),(0x69,0x03B9),(0x6A,0x03D5),(0x6B,0x03BA),(0x6C,0x03BB),(0x6D,0x03BC),
    (0x6E,0x03BD),(0x6F,0x03BF),(0x70,0x03C0),(0x71,0x03B8),(0x72,0x03C1),(0x73,0x03C3),
    (0x74,0x03C4),(0x75,0x03C5),(0x76,0x03D6),(0x77,0x03C9),(0x78,0x03BE),(0x79,0x03C8),
    (0x7A,0x03B6),(0x7B,0x007B),(0x7C,0x007C),(0x7D,0x007D),(0x7E,0x223C),
    (0xA0,0x20AC),(0xA1,0x03D2),(0xA2,0x2032),(0xA3,0x2264),(0xA4,0x2044),(0xA5,0x221E),
    (0xA6,0x0192),(0xA7,0x2663),(0xA8,0x2666),(0xA9,0x2665),(0xAA,0x2660),(0xAB,0x2194),
    (0xAC,0x2190),(0xAD,0x2191),(0xAE,0x2192),(0xAF,0x2193),(0xB0,0x00B0),(0xB1,0x00B1),
    (0xB2,0x2033),(0xB3,0x2265),(0xB4,0x00D7),(0xB5,0x221D),(0xB6,0x2202),(0xB7,0x2022),
    (0xB8,0x00F7),(0xB9,0x2260),(0xBA,0x2261),(0xBB,0x2248),(0xBC,0x2026),(0xBD,0xF8E6),
    (0xBE,0xF8E7),(0xBF,0x21B5),(0xC0,0x2135),(0xC1,0x2111),(0xC2,0x211C),(0xC3,0x2118),
    (0xC4,0x2297),(0xC5,0x2295),(0xC6,0x2205),(0xC7,0x2229),(0xC8,0x222A),(0xC9,0x2283),
    (0xCA,0x2287),(0xCB,0x2284),(0xCC,0x2282),(0xCD,0x2286),(0xCE,0x2208),(0xCF,0x2209),
    (0xD0,0x2220),(0xD1,0x2207),(0xD2,0x00AE),(0xD3,0x00A9),(0xD4,0x2122),(0xD5,0x220F),
    (0xD6,0x221A),(0xD7,0x22C5),(0xD8,0x00AC),(0xD9,0x2227),(0xDA,0x2228),(0xDB,0x21D4),
    (0xDC,0x21D0),(0xDD,0x21D1),(0xDE,0x21D2),(0xDF,0x21D3),(0xE0,0x25CA),(0xE1,0x2329),
    (0xE5,0x2211),(0xF1,0x232A),(0xF2,0x222B),(0xF3,0x2320),(0xF4,0xF8F5),(0xF5,0x2321),
];

/// ITC **ZapfDingbats** font built-in encoding: `(code, Unicode scalar)` pairs
/// (Adobe `zapfdingbats.txt`, PDF spec Annex D.6). The byte `0x34` is the
/// classic check mark ✔ (U+2714). Like Symbol, its bytes carry **none** of the
/// WinAnsi meanings — `0x34` is a checkmark, **not** the digit '4' — so decoding
/// a ZapfDingbats string as WinAnsi would draw Latin glyphs (e.g. "4"). This
/// table gives each code its true Unicode, so a substitute face draws the right
/// glyph where it has one (and nothing where it doesn't — e.g. the bundled
/// Latin face has no ✔, matching its prior blank rendering).
#[rustfmt::skip]
const ZAPF_DINGBATS_PAIRS: &[(u8, u16)] = &[
    (0x20,0x0020),(0x21,0x2701),(0x22,0x2702),(0x23,0x2703),(0x24,0x2704),(0x25,0x260E),
    (0x26,0x2706),(0x27,0x2707),(0x28,0x2708),(0x29,0x2709),(0x2A,0x261B),(0x2B,0x261E),
    (0x2C,0x270C),(0x2D,0x270D),(0x2E,0x270E),(0x2F,0x270F),(0x30,0x2710),(0x31,0x2711),
    (0x32,0x2712),(0x33,0x2713),(0x34,0x2714),(0x35,0x2715),(0x36,0x2716),(0x37,0x2717),
    (0x38,0x2718),(0x39,0x2719),(0x3A,0x271A),(0x3B,0x271B),(0x3C,0x271C),(0x3D,0x271D),
    (0x3E,0x271E),(0x3F,0x271F),(0x40,0x2720),(0x41,0x2721),(0x42,0x2722),(0x43,0x2723),
    (0x44,0x2724),(0x45,0x2725),(0x46,0x2726),(0x47,0x2727),(0x48,0x2605),(0x49,0x2729),
    (0x4A,0x272A),(0x4B,0x272B),(0x4C,0x272C),(0x4D,0x272D),(0x4E,0x272E),(0x4F,0x272F),
    (0x50,0x2730),(0x51,0x2731),(0x52,0x2732),(0x53,0x2733),(0x54,0x2734),(0x55,0x2735),
    (0x56,0x2736),(0x57,0x2737),(0x58,0x2738),(0x59,0x2739),(0x5A,0x273A),(0x5B,0x273B),
    (0x5C,0x273C),(0x5D,0x273D),(0x5E,0x273E),(0x5F,0x273F),(0x60,0x2740),(0x61,0x2741),
    (0x62,0x2742),(0x63,0x2743),(0x64,0x2744),(0x65,0x2745),(0x66,0x2746),(0x67,0x2747),
    (0x68,0x2748),(0x69,0x2749),(0x6A,0x274A),(0x6B,0x274B),(0x6C,0x25CF),(0x6D,0x274D),
    (0x6E,0x25A0),(0x6F,0x274F),(0x70,0x2750),(0x71,0x2751),(0x72,0x2752),(0x73,0x25B2),
    (0x74,0x25BC),(0x75,0x25C6),(0x76,0x2756),(0x77,0x25D7),(0x78,0x2758),(0x79,0x2759),
    (0x7A,0x275A),(0x7B,0x275B),(0x7C,0x275C),(0x7D,0x275D),(0x7E,0x275E),
    (0xA1,0x2761),(0xA2,0x2762),(0xA3,0x2763),(0xA4,0x2764),(0xA5,0x2765),(0xA6,0x2766),
    (0xA7,0x2767),(0xA8,0x2663),(0xA9,0x2666),(0xAA,0x2665),(0xAB,0x2660),(0xAC,0x2460),
    (0xAD,0x2461),(0xAE,0x2462),(0xAF,0x2463),(0xB0,0x2464),(0xB1,0x2465),(0xB2,0x2466),
    (0xB3,0x2467),(0xB4,0x2468),(0xB5,0x2469),(0xB6,0x2776),(0xB7,0x2777),(0xB8,0x2778),
    (0xB9,0x2779),(0xBA,0x277A),(0xBB,0x277B),(0xBC,0x277C),(0xBD,0x277D),(0xBE,0x277E),
    (0xBF,0x277F),(0xC0,0x2780),(0xC1,0x2781),(0xC2,0x2782),(0xC3,0x2783),(0xC4,0x2784),
    (0xC5,0x2785),(0xC6,0x2786),(0xC7,0x2787),(0xC8,0x2788),(0xC9,0x2789),(0xCA,0x278A),
    (0xCB,0x278B),(0xCC,0x278C),(0xCD,0x278D),(0xCE,0x278E),(0xCF,0x278F),(0xD0,0x2790),
    (0xD1,0x2791),(0xD2,0x2792),(0xD3,0x2793),(0xD4,0x2794),(0xD5,0x2192),(0xD6,0x2194),
    (0xD7,0x2195),(0xD8,0x2798),(0xD9,0x2799),(0xDA,0x279A),(0xDB,0x279B),(0xDC,0x279C),
    (0xDD,0x279D),(0xDE,0x279E),(0xDF,0x279F),(0xE0,0x27A0),(0xE1,0x27A1),(0xE2,0x27A2),
    (0xE3,0x27A3),(0xE4,0x27A4),(0xE5,0x27A5),(0xE6,0x27A6),(0xE7,0x27A7),(0xE8,0x27A8),
    (0xE9,0x27A9),(0xEA,0x27AA),(0xEB,0x27AB),(0xEC,0x27AC),(0xED,0x27AD),(0xEE,0x27AE),
    (0xEF,0x27AF),(0xF1,0x27B1),(0xF2,0x27B2),(0xF3,0x27B3),(0xF4,0x27B4),(0xF5,0x27B5),
    (0xF6,0x27B6),(0xF7,0x27B7),(0xF8,0x27B8),(0xF9,0x27B9),(0xFA,0x27BA),(0xFB,0x27BB),
    (0xFC,0x27BC),(0xFD,0x27BD),(0xFE,0x27BE),
];

/// Adobe **Symbol** code → Unicode character (built-in Symbol encoding), or
/// `None` when the code is undefined. See [`SYMBOL_PAIRS`].
pub fn symbol_to_char(code: u8) -> Option<char> {
    SYMBOL_PAIRS
        .iter()
        .find(|&&(c, _)| c == code)
        .and_then(|&(_, cp)| char::from_u32(cp as u32))
}

/// ITC **ZapfDingbats** code → Unicode character (built-in Dingbats encoding),
/// or `None` when the code is undefined. `0x34` → ✔ U+2714. See
/// [`ZAPF_DINGBATS_PAIRS`].
pub fn zapf_dingbats_to_char(code: u8) -> Option<char> {
    ZAPF_DINGBATS_PAIRS
        .iter()
        .find(|&&(c, _)| c == code)
        .and_then(|&(_, cp)| char::from_u32(cp as u32))
}

/// Which predefined base encoding a simple font's `/Encoding` names (or, for
/// `Symbol`/`ZapfDingbats`, which built-in encoding its `/BaseFont` implies).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseEncoding {
    /// `WinAnsiEncoding` (Windows-1252) — the office default.
    WinAnsi,
    /// `MacRomanEncoding` (Mac OS Roman).
    MacRoman,
    /// `StandardEncoding` (Adobe Standard).
    Standard,
    /// Adobe **Symbol** built-in encoding (Greek/maths). Not named via
    /// `/Encoding`; implied by `/BaseFont /Symbol`.
    Symbol,
    /// ITC **ZapfDingbats** built-in encoding (`0x34` → ✔). Not named via
    /// `/Encoding`; implied by `/BaseFont /ZapfDingbats`.
    ZapfDingbats,
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
            Self::Symbol => symbol_to_char(code),
            Self::ZapfDingbats => zapf_dingbats_to_char(code),
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
    // Subset-font glyph placeholder name `gNN` (e.g. `g49`): the producer named
    // the glyph by its index in the **Standard Macintosh Glyph Ordering** (the
    // 258-name list from the TrueType/OpenType `post` format-1 spec). This is the
    // deterministic convention Acrobat itself uses to recover such names — map
    // `gNN` → the standard glyph name at index `NN` → Unicode. (Mac OS, the
    // common Times-New-Roman subsetter, emits these `gNN` `/Differences` names.)
    if let Some(real) = glyph_index_name(name) {
        if real != name {
            return glyph_name_to_unicode(real);
        }
    }
    super::cff_to_otf::glyph_name_to_unicode_string(name)
}

/// Resolve a subset placeholder glyph name `gNN` to the Standard Macintosh Glyph
/// Ordering name at index `NN` (TrueType/OpenType `post` format-1, the 258 names
/// in `MAC_GLYPH_ORDER`). Returns `None` for any other name shape, or when the
/// index is out of range. The `g` must be followed only by ASCII digits, so real
/// AGL names that merely start with `g` (`grave`, `guillemotleft`, …) are not
/// misread.
pub fn glyph_index_name(name: &str) -> Option<&'static str> {
    let digits = name.strip_prefix('g')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let idx: usize = digits.parse().ok()?;
    MAC_GLYPH_ORDER.get(idx).copied()
}

/// Unicode string for glyph id `gid` under the Standard Macintosh Glyph Ordering
/// (`post` format-1). Used as a **last-resort** recovery for Identity-encoded
/// subset fonts whose glyph ids follow that standard order but which ship no
/// usable `/ToUnicode`, `cmap`, or `post` name table. `None` when the gid is out
/// of range or its standard name carries no code point.
pub fn mac_glyph_order_unicode(gid: u16) -> Option<String> {
    let name = MAC_GLYPH_ORDER.get(gid as usize).copied()?;
    glyph_name_to_unicode(name)
}

/// The Standard Macintosh Glyph Ordering (Apple TrueType `post` table format 1 /
/// OpenType spec): the 258 standard glyph names, in the canonical order a
/// `post` format-1 table and Mac-style subsetters assign. Index = glyph id.
const MAC_GLYPH_ORDER: [&str; 258] = [
    ".notdef", ".null", "nonmarkingreturn", "space", "exclam", "quotedbl", "numbersign", "dollar",
    "percent", "ampersand", "quotesingle", "parenleft", "parenright", "asterisk", "plus", "comma",
    "hyphen", "period", "slash", "zero", "one", "two", "three", "four", "five", "six", "seven",
    "eight", "nine", "colon", "semicolon", "less", "equal", "greater", "question", "at", "A", "B",
    "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R", "S", "T", "U",
    "V", "W", "X", "Y", "Z", "bracketleft", "backslash", "bracketright", "asciicircum", "underscore",
    "grave", "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q",
    "r", "s", "t", "u", "v", "w", "x", "y", "z", "braceleft", "bar", "braceright", "asciitilde",
    "Adieresis", "Aring", "Ccedilla", "Eacute", "Ntilde", "Odieresis", "Udieresis", "aacute",
    "agrave", "acircumflex", "adieresis", "atilde", "aring", "ccedilla", "eacute", "egrave",
    "ecircumflex", "edieresis", "iacute", "igrave", "icircumflex", "idieresis", "ntilde", "oacute",
    "ograve", "ocircumflex", "odieresis", "otilde", "uacute", "ugrave", "ucircumflex", "udieresis",
    "dagger", "degree", "cent", "sterling", "section", "bullet", "paragraph", "germandbls",
    "registered", "copyright", "trademark", "acute", "dieresis", "notequal", "AE", "Oslash",
    "infinity", "plusminus", "lessequal", "greaterequal", "yen", "mu", "partialdiff", "summation",
    "product", "pi", "integral", "ordfeminine", "ordmasculine", "Omega", "ae", "oslash",
    "questiondown", "exclamdown", "logicalnot", "radical", "florin", "approxequal", "Delta",
    "guillemotleft", "guillemotright", "ellipsis", "nonbreakingspace", "Agrave", "Atilde", "Otilde",
    "OE", "oe", "endash", "emdash", "quotedblleft", "quotedblright", "quoteleft", "quoteright",
    "divide", "lozenge", "ydieresis", "Ydieresis", "fraction", "currency", "guilsinglleft",
    "guilsinglright", "fi", "fl", "daggerdbl", "periodcentered", "quotesinglbase", "quotedblbase",
    "perthousand", "Acircumflex", "Ecircumflex", "Aacute", "Edieresis", "Egrave", "Iacute",
    "Icircumflex", "Idieresis", "Igrave", "Oacute", "Ocircumflex", "apple", "Ograve", "Uacute",
    "Ucircumflex", "Ugrave", "dotlessi", "circumflex", "tilde", "macron", "breve", "dotaccent",
    "ring", "cedilla", "hungarumlaut", "ogonek", "caron", "Lslash", "lslash", "Scaron", "scaron",
    "Zcaron", "zcaron", "brokenbar", "Eth", "eth", "Yacute", "yacute", "Thorn", "thorn", "minus",
    "multiply", "onesuperior", "twosuperior", "threesuperior", "onehalf", "onequarter",
    "threequarters", "franc", "Gbreve", "gbreve", "Idotaccent", "Scedilla", "scedilla", "Cacute",
    "cacute", "Ccaron", "ccaron", "dcroat",
];

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
    // Digit glyph names (AGL): needed so the Standard-Mac-Glyph-Ordering recovery
    // resolves `g19`..`g28` (zero..nine) — e.g. the "1" of "VOLET 1" (g20).
    ("zero", 0x0030),
    ("one", 0x0031),
    ("two", 0x0032),
    ("three", 0x0033),
    ("four", 0x0034),
    ("five", 0x0035),
    ("six", 0x0036),
    ("seven", 0x0037),
    ("eight", 0x0038),
    ("nine", 0x0039),
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
    fn subset_gnn_names_resolve_via_mac_glyph_order() {
        // `gNN` = index in the Standard Macintosh Glyph Ordering. These are the
        // exact placeholder names Mac-subset Times-New-Roman uses, and the codes
        // that drove the s3705 "Numéro/Adresse/VOLET 1" recovery.
        assert_eq!(glyph_name_to_unicode("g49").as_deref(), Some("N"));
        assert_eq!(glyph_name_to_unicode("g36").as_deref(), Some("A"));
        assert_eq!(glyph_name_to_unicode("g57").as_deref(), Some("V"));
        assert_eq!(glyph_name_to_unicode("g50").as_deref(), Some("O"));
        assert_eq!(glyph_name_to_unicode("g47").as_deref(), Some("L"));
        assert_eq!(glyph_name_to_unicode("g3").as_deref(), Some(" ")); // space
        assert_eq!(glyph_name_to_unicode("g20").as_deref(), Some("1")); // one
        assert_eq!(glyph_name_to_unicode("g106").as_deref(), Some("à")); // agrave
        // A real AGL name that merely starts with `g` must NOT be misread as `gNN`.
        assert_eq!(glyph_name_to_unicode("grave").as_deref(), Some("`"));
        assert_eq!(glyph_name_to_unicode("guillemotleft").as_deref(), Some("«"));
        // Out-of-range index → not in the 258-name table → None (no panic).
        assert_eq!(glyph_name_to_unicode("g9999"), None);
        // `g` with no digits / non-numeric tail is not an index name.
        assert_eq!(glyph_index_name("g"), None);
        assert_eq!(glyph_index_name("g1a"), None);
    }

    #[test]
    fn mac_glyph_order_unicode_recovers_letters_and_accents() {
        assert_eq!(mac_glyph_order_unicode(49).as_deref(), Some("N"));
        assert_eq!(mac_glyph_order_unicode(50).as_deref(), Some("O"));
        assert_eq!(mac_glyph_order_unicode(0x6a).as_deref(), Some("à")); // 106
        assert_eq!(mac_glyph_order_unicode(112).as_deref(), Some("é")); // eacute
        assert_eq!(mac_glyph_order_unicode(0).as_deref(), None); // .notdef
        assert_eq!(mac_glyph_order_unicode(9999), None); // out of range
    }

    #[test]
    fn base_encoding_from_name() {
        assert_eq!(BaseEncoding::from_name(b"WinAnsiEncoding"), Some(BaseEncoding::WinAnsi));
        assert_eq!(BaseEncoding::from_name(b"MacRomanEncoding"), Some(BaseEncoding::MacRoman));
        assert_eq!(BaseEncoding::from_name(b"StandardEncoding"), Some(BaseEncoding::Standard));
        assert_eq!(BaseEncoding::from_name(b"Identity-H"), None);
    }

    #[test]
    fn zapf_dingbats_encoding_maps_dingbats_not_latin() {
        // The classic checkbox glyph: `0x34` is ✔ (U+2714), NOT the digit '4'.
        assert_eq!(zapf_dingbats_to_char(0x34), Some('\u{2714}'));
        assert_eq!(zapf_dingbats_to_char(0x33), Some('\u{2713}')); // ✓
        assert_eq!(zapf_dingbats_to_char(0x6C), Some('\u{25CF}')); // ●
        assert_eq!(zapf_dingbats_to_char(0x20), Some(' '));
        assert_eq!(zapf_dingbats_to_char(0x80), None); // undefined slot
        // Routed through BaseEncoding too.
        assert_eq!(BaseEncoding::ZapfDingbats.to_char(0x34), Some('\u{2714}'));
    }

    #[test]
    fn symbol_encoding_maps_greek_and_maths_not_latin() {
        assert_eq!(symbol_to_char(0x61), Some('\u{03B1}')); // 'a' code → α
        assert_eq!(symbol_to_char(0x42), Some('\u{0392}')); // 'B' code → Β
        assert_eq!(symbol_to_char(0xB6), Some('\u{2202}')); // ∂
        assert_eq!(symbol_to_char(0xA3), Some('\u{2264}')); // ≤
        assert_eq!(BaseEncoding::Symbol.to_char(0x61), Some('\u{03B1}'));
    }
}
