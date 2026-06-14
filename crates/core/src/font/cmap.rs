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
}

impl TextDecoder {
    /// The default decoder: a single-byte WinAnsi font.
    pub fn winansi() -> Self {
        Self::default()
    }

    /// Decode one text-show string to Unicode.
    pub fn decode(&self, bytes: &[u8]) -> String {
        match (&self.to_unicode, self.two_byte) {
            (Some(cmap), true) => {
                let mut out = String::new();
                let mut i = 0;
                while i + 1 < bytes.len() {
                    let code = ((bytes[i] as u32) << 8) | bytes[i + 1] as u32;
                    i += 2;
                    match cmap.decode(code) {
                        Some(text) => out.push_str(text),
                        None => out.push('\u{FFFD}'),
                    }
                }
                out
            }
            (Some(cmap), false) => {
                let mut out = String::new();
                for &b in bytes {
                    match cmap.decode(b as u32) {
                        Some(text) => out.push_str(text),
                        None => out.push(super::winansi_to_char(b)),
                    }
                }
                out
            }
            // Composite font without a ToUnicode map: the codes are CIDs we
            // cannot turn into Unicode here. Emit one placeholder per glyph so
            // the run is still detected and counted.
            (None, true) => "\u{FFFD}".repeat(bytes.len() / 2),
            (None, false) => super::decode_winansi(bytes),
        }
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
        };
        // One 2-byte code 0x0041 → 'É'.
        assert_eq!(decoder.decode(&[0x00, 0x41]), "\u{00C9}");
    }

    #[test]
    fn surrogate_pair_round_trips() {
        // U+1F600 in UTF-16BE is D83D DE00.
        let cmap = ToUnicode::parse(b"beginbfchar <01> <D83DDE00> endbfchar");
        assert_eq!(cmap.decode(0x01), Some("\u{1F600}"));
    }
}
