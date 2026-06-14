//! Byte-level PDF tokenizer (ISO 32000-1 §7.2). Pure `std`, zero dependencies.
//!
//! Turns raw file bytes into [`Token`]s. The same lexer drives both the file
//! parser (objects, xref) and the content-stream parser (operators), since both
//! are built from the same lexical grammar — only the assembly differs.

use crate::error::{EngineError, Result};

/// A single PDF lexical token.
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    /// Integer number.
    Integer(i64),
    /// Real number.
    Real(f64),
    /// Name (bytes after `/`, `#xx` escapes resolved).
    Name(Vec<u8>),
    /// `( ... )` literal string, escapes resolved.
    LiteralString(Vec<u8>),
    /// `< ... >` hex string, decoded to bytes.
    HexString(Vec<u8>),
    /// `[`
    ArrayOpen,
    /// `]`
    ArrayClose,
    /// `<<`
    DictOpen,
    /// `>>`
    DictClose,
    /// A bare keyword/operator: `obj`, `R`, `stream`, `true`, `Tj`, `re`, …
    Keyword(Vec<u8>),
    /// End of input.
    Eof,
}

/// A cursor over PDF bytes producing [`Token`]s.
#[derive(Debug)]
pub struct Lexer<'a> {
    data: &'a [u8],
    pos: usize,
}

#[inline]
fn is_whitespace(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n' | 0x0C | 0x00)
}

#[inline]
fn is_delimiter(b: u8) -> bool {
    matches!(
        b,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

#[inline]
fn is_regular(b: u8) -> bool {
    !is_whitespace(b) && !is_delimiter(b)
}

#[inline]
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Normalize PDF real syntax (`.5`, `4.`, `+.002`) to a Rust-parseable float.
fn parse_pdf_real(s: &str) -> Option<f64> {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + 2);
    let mut i = 0;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        if bytes[i] == b'-' {
            out.push('-');
        }
        i += 1;
    }
    let frac = &s[i..];
    if frac.starts_with('.') {
        out.push('0');
    }
    out.push_str(frac);
    if out.ends_with('.') {
        out.push('0');
    }
    if out.is_empty() || out == "-" {
        return None;
    }
    out.parse::<f64>().ok()
}

impl<'a> Lexer<'a> {
    /// New lexer at the start of `data`.
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// New lexer positioned at `pos`.
    pub fn at(data: &'a [u8], pos: usize) -> Self {
        Self { data, pos }
    }

    /// Current byte offset.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Jump the cursor (used by the parser for backtracking / stream data).
    pub fn set_position(&mut self, pos: usize) {
        self.pos = pos;
    }

    /// The underlying bytes.
    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    fn peek_byte(&self) -> Option<u8> {
        self.data.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.data.get(self.pos).copied();
        if b.is_some() {
            self.pos += 1;
        }
        b
    }

    /// Skip whitespace and `%` comments.
    pub fn skip_whitespace(&mut self) {
        while let Some(b) = self.peek_byte() {
            if b == b'%' {
                while let Some(c) = self.peek_byte() {
                    self.pos += 1;
                    if c == b'\n' || c == b'\r' {
                        break;
                    }
                }
            } else if is_whitespace(b) {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    /// Read the next token, or [`Token::Eof`] at end of input.
    pub fn next_token(&mut self) -> Result<Token> {
        self.skip_whitespace();
        let b = match self.peek_byte() {
            None => return Ok(Token::Eof),
            Some(b) => b,
        };
        match b {
            b'[' => {
                self.pos += 1;
                Ok(Token::ArrayOpen)
            }
            b']' => {
                self.pos += 1;
                Ok(Token::ArrayClose)
            }
            b'<' => {
                if self.data.get(self.pos + 1) == Some(&b'<') {
                    self.pos += 2;
                    Ok(Token::DictOpen)
                } else {
                    self.read_hex_string()
                }
            }
            b'>' => {
                if self.data.get(self.pos + 1) == Some(&b'>') {
                    self.pos += 2;
                    Ok(Token::DictClose)
                } else {
                    self.pos += 1;
                    Err(EngineError::parse(self.pos - 1, "unexpected '>'"))
                }
            }
            b'(' => self.read_literal_string(),
            b'/' => self.read_name(),
            b'+' | b'-' | b'.' | b'0'..=b'9' => self.read_number(),
            // Braces appear in PostScript-style content (Type4 funcs) — keep them
            // as single keywords so the content lexer doesn't choke.
            b'{' | b'}' => {
                self.pos += 1;
                Ok(Token::Keyword(vec![b]))
            }
            _ => self.read_keyword(),
        }
    }

    fn read_number(&mut self) -> Result<Token> {
        let start = self.pos;
        let mut seen_dot = false;
        if matches!(self.peek_byte(), Some(b'+') | Some(b'-')) {
            self.pos += 1;
        }
        while let Some(b) = self.peek_byte() {
            match b {
                b'0'..=b'9' => self.pos += 1,
                b'.' => {
                    seen_dot = true;
                    self.pos += 1;
                }
                _ => break,
            }
        }
        let slice = &self.data[start..self.pos];
        let text = std::str::from_utf8(slice)
            .map_err(|_| EngineError::parse(start, "non-utf8 number"))?;
        if seen_dot {
            let value =
                parse_pdf_real(text).ok_or_else(|| EngineError::parse(start, "invalid real"))?;
            Ok(Token::Real(value))
        } else {
            let value: i64 = text
                .parse()
                .map_err(|_| EngineError::parse(start, "invalid integer"))?;
            Ok(Token::Integer(value))
        }
    }

    fn read_name(&mut self) -> Result<Token> {
        self.pos += 1; // skip '/'
        let mut name = Vec::new();
        while let Some(b) = self.peek_byte() {
            if is_whitespace(b) || is_delimiter(b) {
                break;
            }
            if b == b'#' {
                let h1 = self.data.get(self.pos + 1).copied().and_then(hex_val);
                let h2 = self.data.get(self.pos + 2).copied().and_then(hex_val);
                if let (Some(d1), Some(d2)) = (h1, h2) {
                    name.push((d1 << 4) | d2);
                    self.pos += 3;
                    continue;
                }
                // Malformed escape: treat '#' literally.
                name.push(b'#');
                self.pos += 1;
            } else {
                name.push(b);
                self.pos += 1;
            }
        }
        Ok(Token::Name(name))
    }

    fn read_hex_string(&mut self) -> Result<Token> {
        self.pos += 1; // skip '<'
        let mut nibbles: Vec<u8> = Vec::new();
        loop {
            match self.peek_byte() {
                None => return Err(EngineError::parse(self.pos, "unterminated hex string")),
                Some(b'>') => {
                    self.pos += 1;
                    break;
                }
                Some(b) if is_whitespace(b) => self.pos += 1,
                Some(b) => match hex_val(b) {
                    Some(v) => {
                        nibbles.push(v);
                        self.pos += 1;
                    }
                    None => return Err(EngineError::parse(self.pos, "invalid hex digit")),
                },
            }
        }
        if nibbles.len() % 2 == 1 {
            nibbles.push(0); // odd count: implicit trailing 0
        }
        let bytes = nibbles.chunks(2).map(|c| (c[0] << 4) | c[1]).collect();
        Ok(Token::HexString(bytes))
    }

    fn read_literal_string(&mut self) -> Result<Token> {
        self.pos += 1; // skip '('
        let mut out = Vec::new();
        let mut depth = 1usize;
        while let Some(b) = self.bump() {
            match b {
                b'\\' => {
                    let escaped = match self.bump() {
                        Some(e) => e,
                        None => {
                            return Err(EngineError::parse(self.pos, "unterminated string escape"))
                        }
                    };
                    match escaped {
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0C),
                        b'(' => out.push(b'('),
                        b')' => out.push(b')'),
                        b'\\' => out.push(b'\\'),
                        // Backslash-newline = line continuation (no output).
                        b'\r' => {
                            if self.peek_byte() == Some(b'\n') {
                                self.pos += 1;
                            }
                        }
                        b'\n' => {}
                        b'0'..=b'7' => {
                            // Up to three octal digits (first already consumed).
                            let mut value = (escaped - b'0') as u16;
                            for _ in 0..2 {
                                match self.peek_byte() {
                                    Some(d @ b'0'..=b'7') => {
                                        value = value * 8 + (d - b'0') as u16;
                                        self.pos += 1;
                                    }
                                    _ => break,
                                }
                            }
                            out.push(value as u8);
                        }
                        // Unknown escape: the backslash is ignored, char kept.
                        other => out.push(other),
                    }
                }
                b'(' => {
                    depth += 1;
                    out.push(b'(');
                }
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    out.push(b')');
                }
                _ => out.push(b),
            }
        }
        Ok(Token::LiteralString(out))
    }

    fn read_keyword(&mut self) -> Result<Token> {
        let start = self.pos;
        while let Some(b) = self.peek_byte() {
            if is_regular(b) {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == start {
            let b = self.data[self.pos];
            self.pos += 1; // consume one byte to guarantee progress
            return Err(EngineError::parse(start, format!("unexpected byte 0x{b:02X}")));
        }
        Ok(Token::Keyword(self.data[start..self.pos].to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(data: &[u8]) -> Vec<Token> {
        let mut lexer = Lexer::new(data);
        let mut out = Vec::new();
        loop {
            let token = lexer.next_token().unwrap();
            if token == Token::Eof {
                break;
            }
            out.push(token);
        }
        out
    }

    #[test]
    fn integers_and_reals() {
        assert_eq!(
            toks(b"1 -2 3.5 -.002 4."),
            vec![
                Token::Integer(1),
                Token::Integer(-2),
                Token::Real(3.5),
                Token::Real(-0.002),
                Token::Real(4.0),
            ]
        );
    }

    #[test]
    fn names_with_hex_escapes() {
        assert_eq!(
            toks(b"/Type /A#20B"),
            vec![Token::Name(b"Type".to_vec()), Token::Name(b"A B".to_vec())]
        );
    }

    #[test]
    fn dict_and_array_delimiters() {
        assert_eq!(
            toks(b"<< >> [ ]"),
            vec![
                Token::DictOpen,
                Token::DictClose,
                Token::ArrayOpen,
                Token::ArrayClose
            ]
        );
    }

    #[test]
    fn literal_string_with_escapes_and_nesting() {
        assert_eq!(
            toks(b"(a\\(b\\)c\\n(d))"),
            vec![Token::LiteralString(b"a(b)c\n(d)".to_vec())]
        );
    }

    #[test]
    fn octal_escape() {
        assert_eq!(toks(b"(\\101)"), vec![Token::LiteralString(b"A".to_vec())]);
    }

    #[test]
    fn hex_strings() {
        assert_eq!(
            toks(b"<48656C6C6F>"),
            vec![Token::HexString(b"Hello".to_vec())]
        );
        assert_eq!(toks(b"<4>"), vec![Token::HexString(vec![0x40])]);
    }

    #[test]
    fn reference_and_keywords() {
        assert_eq!(
            toks(b"12 0 R obj endobj true"),
            vec![
                Token::Integer(12),
                Token::Integer(0),
                Token::Keyword(b"R".to_vec()),
                Token::Keyword(b"obj".to_vec()),
                Token::Keyword(b"endobj".to_vec()),
                Token::Keyword(b"true".to_vec()),
            ]
        );
    }

    #[test]
    fn comments_are_skipped() {
        assert_eq!(
            toks(b"1 % a comment here\n 2"),
            vec![Token::Integer(1), Token::Integer(2)]
        );
    }
}
