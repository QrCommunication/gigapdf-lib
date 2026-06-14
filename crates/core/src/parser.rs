//! Object parser (ISO 32000-1 §7.3): assembles [`Token`]s into [`Object`]s.
//!
//! Handles values, indirect references (`n g R`), arrays, dictionaries and
//! streams. Stream data is delimited by searching for `endstream` rather than
//! trusting `/Length` (which may be an indirect reference unresolved at parse
//! time) — robust against the malformed `/Length` values seen in real files.

use crate::error::{EngineError, Result};
use crate::lexer::{Lexer, Token};
use crate::object::{Dictionary, Object, Stream, StringKind};

/// Find `needle` in `hay` at or after `from`. Returns the absolute index.
pub(crate) fn find_subslice(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from >= hay.len() || needle.len() > hay.len() - from {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|i| from + i)
}

/// A recursive-descent parser over PDF bytes.
#[derive(Debug)]
pub struct Parser<'a> {
    lexer: Lexer<'a>,
}

impl<'a> Parser<'a> {
    /// New parser at the start of `data`.
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            lexer: Lexer::new(data),
        }
    }

    /// New parser positioned at `pos` (e.g. just after `n g obj`).
    pub fn at(data: &'a [u8], pos: usize) -> Self {
        Self {
            lexer: Lexer::at(data, pos),
        }
    }

    /// Current byte offset.
    pub fn position(&self) -> usize {
        self.lexer.position()
    }

    /// Parse one PDF value (object) at the cursor.
    pub fn parse_value(&mut self) -> Result<Object> {
        let token = self.lexer.next_token()?;
        self.value_from(token)
    }

    fn value_from(&mut self, token: Token) -> Result<Object> {
        match token {
            Token::Integer(n) => self.integer_or_reference(n),
            Token::Real(r) => Ok(Object::Real(r)),
            Token::Name(n) => Ok(Object::Name(n)),
            Token::LiteralString(s) => Ok(Object::String(s, StringKind::Literal)),
            Token::HexString(s) => Ok(Object::String(s, StringKind::Hex)),
            Token::ArrayOpen => self.parse_array(),
            Token::DictOpen => self.parse_dict_or_stream(),
            Token::Keyword(k) => match k.as_slice() {
                b"true" => Ok(Object::Boolean(true)),
                b"false" => Ok(Object::Boolean(false)),
                b"null" => Ok(Object::Null),
                _ => Err(EngineError::parse(
                    self.lexer.position(),
                    format!("unexpected keyword '{}'", String::from_utf8_lossy(&k)),
                )),
            },
            other => Err(EngineError::parse(
                self.lexer.position(),
                format!("unexpected token {other:?}"),
            )),
        }
    }

    /// After an integer, look ahead for `g R` (an indirect reference). If it's
    /// not a reference, rewind and return the plain integer.
    fn integer_or_reference(&mut self, n: i64) -> Result<Object> {
        let save = self.lexer.position();
        if let Ok(Token::Integer(g)) = self.lexer.next_token() {
            if let Ok(Token::Keyword(k)) = self.lexer.next_token() {
                if k == b"R" && n >= 0 && (0..=u16::MAX as i64).contains(&g) {
                    return Ok(Object::Reference((n as u32, g as u16)));
                }
            }
        }
        self.lexer.set_position(save);
        Ok(Object::Integer(n))
    }

    fn parse_array(&mut self) -> Result<Object> {
        let mut items = Vec::new();
        loop {
            let token = self.lexer.next_token()?;
            match token {
                Token::ArrayClose => break,
                Token::Eof => {
                    return Err(EngineError::parse(self.lexer.position(), "unterminated array"))
                }
                other => items.push(self.value_from(other)?),
            }
        }
        Ok(Object::Array(items))
    }

    fn parse_dict_or_stream(&mut self) -> Result<Object> {
        let mut dict = Dictionary::new();
        loop {
            let token = self.lexer.next_token()?;
            match token {
                Token::DictClose => break,
                Token::Name(key) => {
                    let value = self.parse_value()?;
                    dict.set(key, value);
                }
                Token::Eof => {
                    return Err(EngineError::parse(
                        self.lexer.position(),
                        "unterminated dictionary",
                    ))
                }
                other => {
                    return Err(EngineError::parse(
                        self.lexer.position(),
                        format!("expected name key in dictionary, got {other:?}"),
                    ))
                }
            }
        }

        // A dictionary is a stream iff it's immediately followed by `stream`.
        let save = self.lexer.position();
        match self.lexer.next_token()? {
            Token::Keyword(k) if k == b"stream" => self.read_stream(dict),
            _ => {
                self.lexer.set_position(save);
                Ok(Object::Dictionary(dict))
            }
        }
    }

    fn read_stream(&mut self, dict: Dictionary) -> Result<Object> {
        let data = self.lexer.data();
        let mut start = self.lexer.position();

        // Exactly one EOL follows the `stream` keyword (CRLF or LF per spec;
        // tolerate a bare CR too).
        if data.get(start) == Some(&b'\r') {
            start += 1;
        }
        if data.get(start) == Some(&b'\n') {
            start += 1;
        }

        let end_kw = find_subslice(data, b"endstream", start)
            .ok_or_else(|| EngineError::parse(start, "missing endstream"))?;

        // Strip the single EOL that precedes `endstream` (it is not stream data).
        let mut end = end_kw;
        if end > start && data[end - 1] == b'\n' {
            end -= 1;
        }
        if end > start && data[end - 1] == b'\r' {
            end -= 1;
        }

        let raw = data[start..end].to_vec();
        self.lexer.set_position(end_kw + b"endstream".len());
        Ok(Object::Stream(Stream::new(dict, raw)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::ObjectId;

    fn parse(data: &[u8]) -> Object {
        Parser::new(data).parse_value().unwrap()
    }

    #[test]
    fn scalars() {
        assert_eq!(parse(b"42"), Object::Integer(42));
        assert_eq!(parse(b"3.5"), Object::Real(3.5));
        assert_eq!(parse(b"true"), Object::Boolean(true));
        assert_eq!(parse(b"null"), Object::Null);
        assert_eq!(parse(b"/Page"), Object::Name(b"Page".to_vec()));
    }

    #[test]
    fn reference() {
        assert_eq!(parse(b"12 0 R"), Object::Reference((12, 0)));
    }

    #[test]
    fn array_of_mixed() {
        let obj = parse(b"[1 2.0 /N (s) 9 0 R]");
        let items = obj.as_array().unwrap();
        assert_eq!(items.len(), 5);
        assert_eq!(items[0], Object::Integer(1));
        assert_eq!(items[4], Object::Reference((9, 0)));
    }

    #[test]
    fn dictionary_nested() {
        let obj = parse(b"<< /Type /Pages /Count 2 /Kids [4 0 R 5 0 R] >>");
        let dict = obj.as_dict().unwrap();
        assert_eq!(dict.get(b"Type"), Some(&Object::Name(b"Pages".to_vec())));
        assert_eq!(dict.get(b"Count"), Some(&Object::Integer(2)));
        let kids = dict.get(b"Kids").unwrap().as_array().unwrap();
        let expected: ObjectId = (4, 0);
        assert_eq!(kids[0].as_reference(), Some(expected));
    }

    #[test]
    fn stream_object() {
        let data = b"<< /Length 11 >> stream\nHello world\nendstream";
        let obj = parse(data);
        let stream = obj.as_stream().unwrap();
        assert_eq!(stream.raw, b"Hello world");
        assert_eq!(stream.dict.get(b"Length"), Some(&Object::Integer(11)));
    }

    #[test]
    fn stream_with_wrong_length_falls_back_to_endstream() {
        // /Length deliberately wrong — we must still cut at `endstream`.
        let data = b"<< /Length 999 >> stream\nABC\nendstream";
        let obj = parse(data);
        assert_eq!(obj.as_stream().unwrap().raw, b"ABC");
    }
}
