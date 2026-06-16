//! Parse a JSON `string[][][]` (pages → rows → cells) into the grid shape the
//! spreadsheet writers ([`to_xlsx`](super::office::to_xlsx) /
//! [`to_ods`](super::office::to_ods)) consume — without a JSON dependency.
//!
//! This lets a host hand the engine an *already-reconstructed* table grid (e.g.
//! built by its own heuristic) and get a native `.xlsx`/`.ods` back, reusing the
//! exact zip/sheet writers used by `Document::to_xlsx`. Only the `string[][][]`
//! subset of JSON is accepted; anything else returns `None`.

/// Parse a JSON array-of-arrays-of-arrays-of-strings into `pages[rows][cells]`.
/// Returns `None` on any malformed input (wrong nesting, bad escape, trailing
/// junk). UTF-8 in cell text and the standard JSON string escapes
/// (`\" \\ \/ \n \r \t \b \f \uXXXX`, including surrogate pairs) are honoured.
pub fn from_json(s: &str) -> Option<Vec<Vec<Vec<String>>>> {
    let mut p = Reader {
        b: s.as_bytes(),
        i: 0,
    };
    let grids = p.array(|p| p.array(|p| p.array(Reader::string)))?;
    p.ws();
    // Reject anything after the closing bracket (other than whitespace).
    if p.i == p.b.len() {
        Some(grids)
    } else {
        None
    }
}

/// Parse a flat JSON `string[]` (e.g. per-sheet names) into a `Vec<String>`.
/// `None` on malformed input. Same escape handling as [`from_json`].
pub fn strings_from_json(s: &str) -> Option<Vec<String>> {
    let mut p = Reader {
        b: s.as_bytes(),
        i: 0,
    };
    let v = p.array(Reader::string)?;
    p.ws();
    if p.i == p.b.len() {
        Some(v)
    } else {
        None
    }
}

struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}

impl Reader<'_> {
    fn ws(&mut self) {
        while self.i < self.b.len() && self.b[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    /// The next non-whitespace byte, without consuming it.
    fn peek(&mut self) -> Option<u8> {
        self.ws();
        self.b.get(self.i).copied()
    }

    /// Consume `c` if it is the next non-whitespace byte.
    fn eat(&mut self, c: u8) -> Option<()> {
        if self.peek()? == c {
            self.i += 1;
            Some(())
        } else {
            None
        }
    }

    /// `[ item (, item)* ]` — an empty `[]` yields an empty `Vec`.
    fn array<T>(&mut self, mut item: impl FnMut(&mut Self) -> Option<T>) -> Option<Vec<T>> {
        self.eat(b'[')?;
        let mut out = Vec::new();
        if self.peek()? == b']' {
            self.i += 1;
            return Some(out);
        }
        loop {
            out.push(item(self)?);
            match self.peek()? {
                b',' => self.i += 1,
                b']' => {
                    self.i += 1;
                    return Some(out);
                }
                _ => return None,
            }
        }
    }

    /// A JSON string. Bytes accumulate into a buffer (preserving multi-byte
    /// UTF-8) and escapes push their decoded char's UTF-8; the buffer is decoded
    /// once at the closing quote.
    fn string(&mut self) -> Option<String> {
        self.eat(b'"')?;
        let mut buf: Vec<u8> = Vec::new();
        loop {
            let c = *self.b.get(self.i)?;
            self.i += 1;
            match c {
                b'"' => return String::from_utf8(buf).ok(),
                b'\\' => {
                    let e = *self.b.get(self.i)?;
                    self.i += 1;
                    match e {
                        b'"' => buf.push(b'"'),
                        b'\\' => buf.push(b'\\'),
                        b'/' => buf.push(b'/'),
                        b'n' => buf.push(b'\n'),
                        b'r' => buf.push(b'\r'),
                        b't' => buf.push(b'\t'),
                        b'b' => buf.push(0x08),
                        b'f' => buf.push(0x0C),
                        b'u' => {
                            let ch = self.unicode_escape()?;
                            let mut tmp = [0u8; 4];
                            buf.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
                        }
                        _ => return None,
                    }
                }
                _ => buf.push(c),
            }
        }
    }

    /// Decode the four hex digits after a `\u`, resolving a high surrogate by
    /// consuming the following `\uXXXX` low surrogate.
    fn unicode_escape(&mut self) -> Option<char> {
        let hi = self.hex4()?;
        if (0xD800..=0xDBFF).contains(&hi) {
            // High surrogate — a low surrogate must follow.
            if self.b.get(self.i) != Some(&b'\\') || self.b.get(self.i + 1) != Some(&b'u') {
                return None;
            }
            self.i += 2;
            let lo = self.hex4()?;
            if !(0xDC00..=0xDFFF).contains(&lo) {
                return None;
            }
            let cp = 0x10000 + (((hi - 0xD800) as u32) << 10) + (lo - 0xDC00) as u32;
            char::from_u32(cp)
        } else {
            char::from_u32(hi as u32)
        }
    }

    fn hex4(&mut self) -> Option<u16> {
        let hex = self.b.get(self.i..self.i + 4)?;
        self.i += 4;
        u16::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pages_rows_cells() {
        let g = from_json(r#"[[["A","B"],["C","D"]],[["solo"]]]"#).unwrap();
        assert_eq!(g.len(), 2, "two pages");
        assert_eq!(g[0], vec![vec!["A", "B"], vec!["C", "D"]]);
        assert_eq!(g[1], vec![vec!["solo"]]);
    }

    #[test]
    fn handles_escapes_and_utf8() {
        // Quote, backslash, newline, tab, a BMP \u escape, raw UTF-8, surrogate pair.
        let g = from_json(r#"[[["a\"b\\c\n\t","café","café","😀"]]]"#).unwrap();
        assert_eq!(g[0][0][0], "a\"b\\c\n\t");
        assert_eq!(g[0][0][1], "café");
        assert_eq!(g[0][0][2], "café");
        assert_eq!(g[0][0][3], "😀");
    }

    #[test]
    fn empty_arrays_and_whitespace() {
        assert_eq!(from_json("[]").unwrap(), Vec::<Vec<Vec<String>>>::new());
        assert_eq!(from_json("  [ [ [] ] ] ").unwrap(), vec![vec![Vec::<String>::new()]]);
    }

    #[test]
    fn parses_flat_string_array() {
        assert_eq!(
            strings_from_json(r#"["Page 1","Sheet1","café"]"#).unwrap(),
            vec!["Page 1", "Sheet1", "café"]
        );
        assert_eq!(strings_from_json("[]").unwrap(), Vec::<String>::new());
        assert!(strings_from_json(r#"["a",2]"#).is_none(), "numbers rejected");
    }

    #[test]
    fn rejects_malformed() {
        assert!(from_json("").is_none());
        assert!(from_json("[[[1]]]").is_none(), "numbers are not strings");
        assert!(from_json(r#"[[["x"]]"#).is_none(), "unbalanced");
        assert!(from_json(r#"[[["x"]]] junk"#).is_none(), "trailing junk");
        assert!(from_json(r#"[["x"]]"#).is_none(), "only two levels");
    }
}
