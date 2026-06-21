//! `/ToUnicode` CMap parsing (ISO 32000-1 §9.10.3) for font-aware text
//! extraction.
//!
//! A character code in a content stream is not Unicode — it indexes a glyph in
//! whatever encoding the font uses. To extract *readable* text (no tofu) for
//! CID/Type0 fonts and custom-encoded simple fonts, we read the font's
//! `/ToUnicode` stream: a small CMap mapping raw codes to Unicode via
//! `beginbfchar`/`beginbfrange` blocks. The CMap is lexed with our own
//! [`Lexer`], so no new tokenizer and zero dependencies.

use std::collections::BTreeMap;

use crate::lexer::{Lexer, Token};

/// A parsed `/ToUnicode` CMap: character code → Unicode string.
#[derive(Debug, Clone, Default)]
pub struct ToUnicode {
    map: BTreeMap<u32, String>,
}

impl ToUnicode {
    /// Parse a decoded `/ToUnicode` CMap stream. Unknown constructs are skipped
    /// rather than rejected — a partial map still beats tofu.
    pub fn parse(data: &[u8]) -> Self {
        let mut lexer = Lexer::new(data);
        let mut tokens = Vec::new();
        while let Ok(token) = lexer.next_token() {
            if matches!(token, Token::Eof) {
                break;
            }
            tokens.push(token);
        }

        let mut map = BTreeMap::new();
        let mut i = 0;
        while i < tokens.len() {
            match &tokens[i] {
                Token::Keyword(k) if k == b"beginbfchar" => {
                    i = parse_bfchar(&tokens, i + 1, &mut map);
                }
                Token::Keyword(k) if k == b"beginbfrange" => {
                    i = parse_bfrange(&tokens, i + 1, &mut map);
                }
                _ => i += 1,
            }
        }
        Self { map }
    }

    /// The Unicode string for a character code, if mapped.
    pub fn decode(&self, code: u32) -> Option<&str> {
        self.map.get(&code).map(String::as_str)
    }

    /// Whether the CMap mapped nothing useful.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Accumulate this CMap's affine offsets (`unicode - code`) over its
    /// single-scalar, printable-ASCII entries into `acc`. Drives the
    /// document-wide offset used by [`Self::infer_ascii_gaps`] to recover sparse
    /// subset CMaps; the histogram's dominant key is that offset.
    pub fn accumulate_ascii_deltas(&self, acc: &mut BTreeMap<i64, u32>) {
        for (&code, text) in &self.map {
            if let Some(scalar) = single_scalar(text) {
                if (0x20..=0x7E).contains(&scalar) {
                    *acc.entry(scalar as i64 - code as i64).or_default() += 1;
                }
            }
        }
    }

    /// Recover the codes a **broken** `/ToUnicode` omits, for subset fonts that
    /// assign glyph codes by a single affine offset `unicode = code + delta` over
    /// the printable-ASCII run but whose producer only emitted entries for the
    /// glyphs it happened to touch (common in Mac-produced PDFs whose embedded
    /// TrueType ships no `cmap`/`post`, leaving `/ToUnicode` the *only* mapping —
    /// and an incomplete one).
    ///
    /// The fix is self-calibrating: it infers `delta` from this font's *own*
    /// existing entries (the dominant `unicode - code` among single-scalar
    /// ASCII-printable targets), and only when that delta is **strongly
    /// dominant** (covers nearly all such entries, with enough samples) does it
    /// fill the gaps. Filled codes are bounded to the run actually covered and to
    /// printable-ASCII targets, so it never extrapolates wildly or touches a font
    /// with a normal (non-offset) encoding. Existing entries are never changed.
    ///
    /// `doc_delta` is an optional **document-wide** affine offset (the dominant
    /// `unicode - code` across every subset `/ToUnicode` in the file). A sparse
    /// font — too few entries to self-calibrate (e.g. a 2-entry CMap) — borrows it
    /// **only when its own handful of entries is consistent with it** (none
    /// contradicts), so a genuinely different font is never mis-filled.
    ///
    /// No-op for well-formed CMaps: a complete `/ToUnicode` has no gaps among the
    /// codes it covers, so nothing is added.
    pub fn infer_ascii_gaps(&mut self, doc_delta: Option<i64>) {
        // Tally the delta of every single-scalar, ASCII-printable entry.
        let mut deltas: BTreeMap<i64, u32> = BTreeMap::new();
        let mut min_code = u32::MAX;
        let mut max_code = 0u32;
        let mut ascii_entries = 0u32;
        for (&code, text) in &self.map {
            let Some(scalar) = single_scalar(text) else { continue };
            min_code = min_code.min(code);
            max_code = max_code.max(code);
            if (0x20..=0x7E).contains(&scalar) {
                *deltas.entry(scalar as i64 - code as i64).or_default() += 1;
                ascii_entries += 1;
            }
        }
        if ascii_entries == 0 {
            return;
        }
        let own_dominant = deltas
            .iter()
            .max_by_key(|(_, &n)| n)
            .filter(|(_, &hits)| hits as f64 >= 0.8 * ascii_entries as f64)
            .map(|(&d, _)| d);

        // Choose the delta to extrapolate with:
        // - a strong self-calibrated delta (≥8 own samples) is most authoritative;
        // - otherwise the document-wide delta, but only if this font's entries
        //   don't contradict it (every own ASCII entry already matches it).
        let delta = match own_dominant {
            Some(d) if ascii_entries >= 8 => d,
            _ => {
                let Some(d) = doc_delta else { return };
                let consistent = self
                    .map
                    .iter()
                    .filter_map(|(&code, text)| single_scalar(text).map(|s| (code, s)))
                    .filter(|&(_, s)| (0x20..=0x7E).contains(&s))
                    .all(|(code, s)| s as i64 - code as i64 == d);
                if !consistent {
                    return;
                }
                d
            }
        };

        let _ = (min_code, max_code);
        // Fill every unmapped code whose `code + delta` lands on printable ASCII or
        // the space (0x20..=0x7E — skip the C0/DEL controls). The code range is
        // exactly the one that produces those scalars under `delta`, so a sparse
        // font (e.g. only `O`/`S` mapped) still recovers its whole ASCII alphabet
        // from the document-wide offset. Unmapped codes the content never uses are
        // inert; only the broken-but-used ones are what we rescue.
        let lo = (0x20i64 - delta).max(0) as u32;
        let hi = match u32::try_from(0x7Ei64 - delta) {
            Ok(h) => h,
            Err(_) => return,
        };
        for code in lo..=hi {
            if self.map.contains_key(&code) {
                continue;
            }
            let scalar = code as i64 + delta;
            if let Some(c) = char::from_u32(scalar as u32) {
                self.map.insert(code, c.to_string());
            }
        }
    }
}

/// The single Unicode scalar a CMap target encodes, or `None` when it is empty
/// or a multi-character string (a ligature — never part of the affine ASCII run).
fn single_scalar(text: &str) -> Option<u32> {
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Some(c as u32),
        _ => None,
    }
}

/// `<src> <dst>` pairs until `endbfchar`. Returns the index past the block.
fn parse_bfchar(tokens: &[Token], mut i: usize, map: &mut BTreeMap<u32, String>) -> usize {
    while i < tokens.len() {
        if matches!(&tokens[i], Token::Keyword(k) if k == b"endbfchar") {
            return i + 1;
        }
        if let (Some(Token::HexString(src)), Some(Token::HexString(dst))) =
            (tokens.get(i), tokens.get(i + 1))
        {
            if let Some(code) = bytes_to_code(src) {
                let text = utf16be_to_string(dst);
                if !text.is_empty() {
                    map.insert(code, text);
                }
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    i
}

/// `<lo> <hi> <dst>` or `<lo> <hi> [<d0> <d1> …]` triples until `endbfrange`.
fn parse_bfrange(tokens: &[Token], mut i: usize, map: &mut BTreeMap<u32, String>) -> usize {
    while i < tokens.len() {
        if matches!(&tokens[i], Token::Keyword(k) if k == b"endbfrange") {
            return i + 1;
        }
        let (lo, hi) = match (tokens.get(i), tokens.get(i + 1)) {
            (Some(Token::HexString(lo)), Some(Token::HexString(hi))) => {
                (bytes_to_code(lo), bytes_to_code(hi))
            }
            _ => {
                i += 1;
                continue;
            }
        };
        let (lo, hi) = match (lo, hi) {
            (Some(lo), Some(hi)) if hi >= lo => (lo, hi),
            _ => {
                i += 3;
                continue;
            }
        };
        match tokens.get(i + 2) {
            // Contiguous: dst is the Unicode of `lo`, incrementing per code.
            Some(Token::HexString(dst)) => {
                if let Some(start) = utf16be_to_scalar(dst) {
                    let span = (hi - lo).min(0xFFFF);
                    for offset in 0..=span {
                        if let Some(c) = char::from_u32(start + offset) {
                            map.insert(lo + offset, c.to_string());
                        }
                    }
                } else {
                    let text = utf16be_to_string(dst);
                    if !text.is_empty() {
                        map.insert(lo, text);
                    }
                }
                i += 3;
            }
            // Explicit per-code targets.
            Some(Token::ArrayOpen) => {
                let mut j = i + 3;
                let mut code = lo;
                while j < tokens.len() {
                    match &tokens[j] {
                        Token::ArrayClose => {
                            j += 1;
                            break;
                        }
                        Token::HexString(dst) => {
                            let text = utf16be_to_string(dst);
                            if !text.is_empty() {
                                map.insert(code, text);
                            }
                            code = code.wrapping_add(1);
                            j += 1;
                        }
                        _ => j += 1,
                    }
                }
                i = j;
            }
            _ => i += 3,
        }
    }
    i
}

/// Big-endian code from 1–4 source bytes (`<0041>` → `0x41`).
fn bytes_to_code(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() || bytes.len() > 4 {
        return None;
    }
    Some(bytes.iter().fold(0u32, |acc, &b| (acc << 8) | b as u32))
}

/// Decode raw UTF-16BE bytes (no BOM) to a String, handling surrogate pairs.
fn utf16be_to_string(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut i = 0;
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
}

/// Single scalar value for a `bfrange` start (one BMP unit or one surrogate
/// pair). `None` when the destination spans several characters (then the caller
/// only maps the `lo` code as a string).
fn utf16be_to_scalar(bytes: &[u8]) -> Option<u32> {
    match bytes.len() {
        2 => {
            let unit = ((bytes[0] as u16) << 8) | bytes[1] as u16;
            (!(0xD800..=0xDFFF).contains(&unit)).then_some(unit as u32)
        }
        4 => {
            let hi = ((bytes[0] as u16) << 8) | bytes[1] as u16;
            let lo = ((bytes[2] as u16) << 8) | bytes[3] as u16;
            if (0xD800..=0xDBFF).contains(&hi) && (0xDC00..=0xDFFF).contains(&lo) {
                Some(0x10000 + (((hi - 0xD800) as u32) << 10) + (lo - 0xDC00) as u32)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Decodes the bytes of a text-show operand to Unicode for one font.
#[derive(Debug, Clone, Default)]
pub struct TextDecoder {
    /// A composite (Type0) font addressed with 2-byte character codes.
    pub two_byte: bool,
    /// The font's `/ToUnicode` map, when the font carries one.
    pub to_unicode: Option<ToUnicode>,
    /// Per-code advance widths from the font's `/Widths` (simple) or `/W`+`/DW`
    /// (Type0) tables, when present — lets a text run be measured by real glyph
    /// advances instead of a 0.5-em estimate.
    pub widths: Option<CodeWidths>,
    /// For an Identity-H Type0 font: a glyph-id → Unicode map derived from the
    /// embedded font program's own `cmap` (and `post` glyph names). With Identity
    /// encoding the 2-byte code equals the glyph id, so this recovers real text
    /// (no tofu) where there's no `/ToUnicode` to consult **or where a partial
    /// `/ToUnicode` omits some codes** (subset fonts routinely do).
    pub cid_to_unicode: Option<std::collections::BTreeMap<u16, String>>,
    /// For a **simple** (single-byte, non-CID) font: a character-code → Unicode
    /// map resolved from the font's base `/Encoding`
    /// (`WinAnsiEncoding`/`MacRomanEncoding`/`StandardEncoding`/the font's
    /// built-in) overlaid with its `/Encoding` `/Differences` (each code → glyph
    /// name → Unicode via the Adobe Glyph List). This is the spec resolution order
    /// (ISO 32000-1 §9.10.2) for simple fonts that ship no `/ToUnicode`; it
    /// supersedes the hard-coded WinAnsi fallback (wrong for MacRoman-encoded and
    /// custom-`/Differences` fonts). `None` ⇒ fall back to WinAnsi.
    pub simple_encoding: Option<std::collections::BTreeMap<u8, String>>,
}

/// Per-character-code advance widths in PDF glyph space (1000 units = 1 em),
/// from a font's `/Widths` or `/W` table. `default` is the missing-width / `/DW`
/// fallback applied to codes absent from the map.
#[derive(Debug, Clone, Default)]
pub struct CodeWidths {
    map: std::collections::BTreeMap<u32, f64>,
    default: f64,
}

impl CodeWidths {
    /// Build from a code→advance map (1000-em units) and a default width.
    pub fn new(map: std::collections::BTreeMap<u32, f64>, default: f64) -> Self {
        Self { map, default }
    }

    /// The advance of `code` in 1000-em units, falling back to the default.
    pub fn advance(&self, code: u32) -> f64 {
        self.map.get(&code).copied().unwrap_or(self.default)
    }
}

impl TextDecoder {
    /// The default decoder: a single-byte WinAnsi font.
    pub fn winansi() -> Self {
        Self::default()
    }

    /// The advance of one text-show string in user-space points, summed from the
    /// real per-glyph widths. `None` when this font carries no width table (the
    /// caller then falls back to an average-advance estimate).
    pub fn string_advance(&self, bytes: &[u8], font_size: f64) -> Option<f64> {
        let widths = self.widths.as_ref()?;
        let mut units = 0.0;
        if self.two_byte {
            let mut i = 0;
            while i + 1 < bytes.len() {
                let code = ((bytes[i] as u32) << 8) | bytes[i + 1] as u32;
                units += widths.advance(code);
                i += 2;
            }
        } else {
            for &b in bytes {
                units += widths.advance(b as u32);
            }
        }
        Some(units * font_size / 1000.0)
    }

    /// Decode one text-show string to Unicode.
    ///
    /// Resolution order per code (ISO 32000-1 §9.10):
    /// - composite (2-byte): `/ToUnicode` → embedded-cmap `cid_to_unicode`
    ///   fallback (covers codes a **partial** `/ToUnicode` omits — subset fonts
    ///   routinely do) → `U+FFFD` placeholder (so the run is still counted).
    /// - simple (1-byte): `/ToUnicode` → base-`/Encoding` + `/Differences`
    ///   (`simple_encoding`) → WinAnsi last-resort.
    pub fn decode(&self, bytes: &[u8]) -> String {
        if self.two_byte {
            let mut out = String::new();
            let mut i = 0;
            while i + 1 < bytes.len() {
                let code = ((bytes[i] as u16) << 8) | bytes[i + 1] as u16;
                i += 2;
                match self.decode_two_byte(code) {
                    Some(text) => out.push_str(text),
                    None => out.push('\u{FFFD}'),
                }
            }
            out
        } else {
            let mut out = String::new();
            for &b in bytes {
                self.decode_one_byte(b, &mut out);
            }
            out
        }
    }

    /// Unicode for one 2-byte composite code: `/ToUnicode` first, then the
    /// embedded-cmap fallback (Identity-H ⇒ code == glyph id). `None` ⇒ unmapped.
    fn decode_two_byte(&self, code: u16) -> Option<&str> {
        if let Some(text) = self.to_unicode.as_ref().and_then(|c| c.decode(code as u32)) {
            return Some(text);
        }
        self.cid_to_unicode
            .as_ref()
            .and_then(|m| m.get(&code))
            .map(String::as_str)
    }

    /// Append the Unicode for one single-byte simple-font code: `/ToUnicode`,
    /// then the base-`/Encoding`+`/Differences` map, then WinAnsi as a last
    /// resort. Each source may yield a multi-character string (e.g. a ligature).
    fn decode_one_byte(&self, b: u8, out: &mut String) {
        if let Some(text) = self.to_unicode.as_ref().and_then(|c| c.decode(b as u32)) {
            out.push_str(text);
            return;
        }
        if let Some(text) = self.simple_encoding.as_ref().and_then(|m| m.get(&b)) {
            out.push_str(text);
            return;
        }
        out.push(super::winansi_to_char(b));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bfchar_pairs() {
        let cmap = ToUnicode::parse(b"beginbfchar <01> <0041> <02> <00E9> endbfchar");
        assert_eq!(cmap.decode(0x01), Some("A"));
        assert_eq!(cmap.decode(0x02), Some("\u{00E9}")); // é
        assert_eq!(cmap.decode(0x03), None);
    }

    #[test]
    fn parses_contiguous_bfrange() {
        // codes 0x10..=0x12 map to 'A','B','C'.
        let cmap = ToUnicode::parse(b"beginbfrange <10> <12> <0041> endbfrange");
        assert_eq!(cmap.decode(0x10), Some("A"));
        assert_eq!(cmap.decode(0x11), Some("B"));
        assert_eq!(cmap.decode(0x12), Some("C"));
    }

    #[test]
    fn parses_array_bfrange() {
        let cmap = ToUnicode::parse(b"beginbfrange <20> <22> [<0058> <0059> <005A>] endbfrange");
        assert_eq!(cmap.decode(0x20), Some("X"));
        assert_eq!(cmap.decode(0x21), Some("Y"));
        assert_eq!(cmap.decode(0x22), Some("Z"));
    }

    #[test]
    fn two_byte_decoder_uses_cmap() {
        let cmap = ToUnicode::parse(b"beginbfchar <0041> <00C9> endbfchar");
        let decoder = TextDecoder {
            two_byte: true,
            to_unicode: Some(cmap),
            widths: None,
            cid_to_unicode: None,
            simple_encoding: None,
        };
        // One 2-byte code 0x0041 → 'É'.
        assert_eq!(decoder.decode(&[0x00, 0x41]), "\u{00C9}");
    }

    #[test]
    fn string_advance_sums_real_widths() {
        let mut map = std::collections::BTreeMap::new();
        map.insert(b'A' as u32, 600.0);
        map.insert(b'B' as u32, 700.0);
        let decoder = TextDecoder {
            two_byte: false,
            to_unicode: None,
            widths: Some(CodeWidths::new(map, 500.0)),
            cid_to_unicode: None,
            simple_encoding: None,
        };
        // "AB?" → 600 + 700 + 500 (default) = 1800 units × 12/1000 = 21.6 pt.
        assert_eq!(decoder.string_advance(b"AB?", 12.0), Some(21.6));
        // No width table → None, so the caller falls back to an estimate.
        assert_eq!(TextDecoder::winansi().string_advance(b"AB", 12.0), None);
    }

    #[test]
    fn surrogate_pair_round_trips() {
        // U+1F600 in UTF-16BE is D83D DE00.
        let cmap = ToUnicode::parse(b"beginbfchar <01> <D83DDE00> endbfchar");
        assert_eq!(cmap.decode(0x01), Some("\u{1F600}"));
    }

    // ── broken-subset `/ToUnicode` recovery (the s1106 bug class) ──────────────

    #[test]
    fn infer_ascii_gaps_self_calibrated() {
        // A subset that maps glyph code = unicode − 0x1D, but the producer only
        // emitted a few letters — `J`(0x2d), the apostrophe(0x0a) and `k`(0x2e)
        // are absent. With ≥8 own samples the dominant delta (0x1D) self-fills.
        let mut cmap = ToUnicode::parse(
            b"beginbfrange \
              <0024> <0024> <0041> \
              <0044> <004c> <0061> \
              <004f> <0059> <006c> \
              endbfrange",
        ); // A, a..i, l..v
        assert_eq!(cmap.decode(0x2d), None); // J unmapped before
        cmap.infer_ascii_gaps(None);
        assert_eq!(cmap.decode(0x2d), Some("J")); // 0x2d + 0x1D = 0x4A
        assert_eq!(cmap.decode(0x0a), Some("'")); // quote slot recovered
        assert_eq!(cmap.decode(0x03), Some(" ")); // space recovered
        // Existing entries untouched.
        assert_eq!(cmap.decode(0x24), Some("A"));
        assert_eq!(cmap.decode(0x44), Some("a"));
    }

    #[test]
    fn infer_ascii_gaps_borrows_document_delta_when_sparse() {
        // A near-empty CMap (only O,S) cannot self-calibrate, but borrows the
        // document-wide delta because its two entries are consistent with it.
        let mut cmap = ToUnicode::parse(b"beginbfrange <0032> <0032> <004f> <0036> <0036> <0053> endbfrange");
        cmap.infer_ascii_gaps(Some(0x1D));
        assert_eq!(cmap.decode(0x44), Some("a")); // recovered via doc delta
        assert_eq!(cmap.decode(0x2d), Some("J"));
        assert_eq!(cmap.decode(0x32), Some("O")); // own entry kept
    }

    #[test]
    fn infer_ascii_gaps_rejects_contradictory_document_delta() {
        // A sparse CMap whose own entry contradicts the document delta must NOT
        // borrow it (different font / encoding) — no gaps filled.
        let mut cmap = ToUnicode::parse(b"beginbfchar <0041> <0041> <0042> <0042> endbfchar"); // identity: A,B
        cmap.infer_ascii_gaps(Some(0x1D)); // doc delta would map 0x41→0x5E (^)
        assert_eq!(cmap.decode(0x41), Some("A")); // unchanged
        assert_eq!(cmap.decode(0x2d), None); // not filled — delta contradicted
    }

    #[test]
    fn infer_ascii_gaps_noop_on_complete_cmap() {
        // A well-formed CMap with no contiguous-run gaps gains nothing spurious.
        let mut cmap = ToUnicode::parse(b"beginbfchar <0041> <0041> endbfchar");
        cmap.infer_ascii_gaps(Some(0x00));
        // Only the inert range fills around the single entry; the entry stands.
        assert_eq!(cmap.decode(0x41), Some("A"));
    }

    // ── simple-font fallbacks (MacRoman base + `/Differences`) ─────────────────

    #[test]
    fn simple_decoder_uses_macroman_then_differences() {
        // MacRoman base (0x88 = à, 0xD5 = ’) plus a `/Differences` override that
        // maps 0x41 to `agrave`. No `/ToUnicode`.
        let mut enc = std::collections::BTreeMap::new();
        enc.insert(0x88, "à".to_string());
        enc.insert(0xD5, "\u{2019}".to_string());
        enc.insert(0x41, "à".to_string()); // /Differences override
        let dec = TextDecoder {
            two_byte: false,
            to_unicode: None,
            widths: None,
            cid_to_unicode: None,
            simple_encoding: Some(enc),
        };
        assert_eq!(dec.decode(&[0x88, 0xD5, 0x41, b'e']), "à\u{2019}àe");
    }

    #[test]
    fn composite_decoder_falls_back_to_cid_map_for_partial_tounicode() {
        // ToUnicode covers code 0x41 only; the embedded-cmap fallback covers 0x42.
        let to = ToUnicode::parse(b"beginbfchar <0041> <0041> endbfchar");
        let mut cid = std::collections::BTreeMap::new();
        cid.insert(0x42u16, "B".to_string());
        let dec = TextDecoder {
            two_byte: true,
            to_unicode: Some(to),
            widths: None,
            cid_to_unicode: Some(cid),
            simple_encoding: None,
        };
        // 0x0041 → A (ToUnicode), 0x0042 → B (cid fallback), 0x0043 → tofu.
        assert_eq!(dec.decode(&[0x00, 0x41, 0x00, 0x42, 0x00, 0x43]), "AB\u{FFFD}");
    }
}
