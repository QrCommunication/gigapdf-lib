//! Page margins and running header/footer baking for an existing PDF.
//!
//! This module carries the **value types** shared by the [`Document`] margin and
//! header/footer API ([`Document::page_margins`], [`Document::set_page_margins`],
//! [`Document::set_header`], [`Document::set_footer`], and the matching
//! `remove_*`); the behaviour lives in [`crate::document`]. Everything is in
//! **PDF points** (`1pt = 1/72 in`), `f64`.
//!
//! [`Document`]: crate::document::Document
//! [`Document::page_margins`]: crate::document::Document::page_margins
//! [`Document::set_page_margins`]: crate::document::Document::set_page_margins
//! [`Document::set_header`]: crate::document::Document::set_header
//! [`Document::set_footer`]: crate::document::Document::set_footer

/// Per-side page margins, in points. Field names mirror
/// [`html::Margins`](crate::html::Margins) and
/// [`model::geom::Margins`](crate::model::geom::Margins) so the three never drift;
/// this one is the type the PDF [`Document`](crate::document::Document) margin API
/// speaks (the `model` tree stays self-contained — importers convert at the edges).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Margins {
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
    pub left: f64,
}

impl Margins {
    /// The same margin on every side.
    pub fn uniform(m: f64) -> Self {
        Self {
            top: m,
            right: m,
            bottom: m,
            left: m,
        }
    }
}

impl Default for Margins {
    /// 0.5" on every side (`36pt`).
    fn default() -> Self {
        Self::uniform(36.0)
    }
}

/// Horizontal alignment of header/footer text within the printable width.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

impl Align {
    /// Parse the JSON/string spelling (`"left"`/`"center"`/`"right"`,
    /// case-insensitive). Unknown values fall back to [`Align::Left`].
    pub fn from_str_lossy(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "center" | "centre" | "middle" => Align::Center,
            "right" | "end" => Align::Right,
            _ => Align::Left,
        }
    }
}

/// A running header or footer to bake onto the pages of an existing PDF.
///
/// `text` may contain the tokens `{{page}}` (1-based page number) and `{{pages}}`
/// (total page count), substituted per page. The text is drawn in the standard
/// base-14 **Helvetica** (no embedding) inside the page's top (header) / bottom
/// (footer) margin band, horizontally aligned per [`align`](Self::align).
#[derive(Debug, Clone, PartialEq)]
pub struct HeaderFooterSpec {
    /// Template text, with `{{page}}` / `{{pages}}` tokens.
    pub text: String,
    /// Horizontal alignment within the printable width.
    pub align: Align,
    /// Font size in points.
    pub font_size: f64,
    /// RGB fill colour, `0.0..=1.0` per channel.
    pub color: [f64; 3],
    /// Inclusive 1-based page range `(first, last)` the H/F applies to; `None`
    /// means every page.
    pub page_range: Option<(usize, usize)>,
    /// Draw on the first page of the range too. When `false`, page 1 (or the
    /// first in-range page) is skipped — the common "no header on the cover" case.
    pub show_on_first_page: bool,
    /// Height of the band (from the page edge inward) the text sits in, in points.
    /// The baseline is vertically centred in this band.
    pub band_height: f64,
}

impl Default for HeaderFooterSpec {
    fn default() -> Self {
        Self {
            text: String::new(),
            align: Align::Left,
            font_size: 10.0,
            color: [0.0, 0.0, 0.0],
            page_range: None,
            show_on_first_page: true,
            band_height: 36.0,
        }
    }
}

impl HeaderFooterSpec {
    /// Substitute `{{page}}` (1-based) and `{{pages}}` (total) into `text`.
    pub fn render_text(&self, page_1based: usize, total_pages: usize) -> String {
        self.text
            .replace("{{page}}", &page_1based.to_string())
            .replace("{{pages}}", &total_pages.to_string())
    }

    /// Parse a flat JSON object into a spec, filling absent fields from
    /// [`Default`]. Returns `None` only on malformed JSON (so a host gets a clear
    /// error rather than a silently-wrong header). Recognised keys:
    /// `text` (string), `align` (string), `fontSize`/`font_size` (number),
    /// `color` (3-number array), `pageRange`/`page_range` (2-number array or
    /// null), `showOnFirstPage`/`show_on_first_page` (bool),
    /// `bandHeight`/`band_height` (number).
    pub fn from_json(s: &str) -> Option<Self> {
        let mut spec = HeaderFooterSpec::default();
        let mut p = ObjReader::new(s);
        p.object(|p, key| {
            match key {
                "text" => spec.text = p.string()?,
                "align" => spec.align = Align::from_str_lossy(&p.string()?),
                "fontSize" | "font_size" => spec.font_size = p.number()?,
                "color" => {
                    let nums = p.number_array()?;
                    if nums.len() >= 3 {
                        spec.color = [nums[0], nums[1], nums[2]];
                    }
                }
                "pageRange" | "page_range" => {
                    if p.peek()? == b'n' {
                        p.null()?;
                        spec.page_range = None;
                    } else {
                        let nums = p.number_array()?;
                        if nums.len() >= 2 {
                            spec.page_range = Some((nums[0] as usize, nums[1] as usize));
                        }
                    }
                }
                "showOnFirstPage" | "show_on_first_page" => {
                    spec.show_on_first_page = p.boolean()?
                }
                "bandHeight" | "band_height" => spec.band_height = p.number()?,
                _ => p.skip_value()?,
            }
            Some(())
        })?;
        p.ws();
        if p.done() {
            Some(spec)
        } else {
            None
        }
    }

    /// Serialise to a flat JSON object with the same keys [`from_json`] reads
    /// (`text`, `align`, `fontSize`, `color`, `pageRange`, `showOnFirstPage`,
    /// `bandHeight`), so `from_json(spec.to_json())` round-trips. Hand-rolled,
    /// zero-dependency; the string escaping matches the crate's other JSON
    /// emitters. `pageRange` is `null` when [`page_range`](Self::page_range) is
    /// `None`.
    ///
    /// [`from_json`]: Self::from_json
    /// [`page_range`]: Self::page_range
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        out.push_str("{\"text\":");
        json_escape(&self.text, &mut out);
        out.push_str(",\"align\":");
        json_escape(
            match self.align {
                Align::Left => "left",
                Align::Center => "center",
                Align::Right => "right",
            },
            &mut out,
        );
        out.push_str(&format!(",\"fontSize\":{}", self.font_size));
        out.push_str(&format!(
            ",\"color\":[{},{},{}]",
            self.color[0], self.color[1], self.color[2]
        ));
        match self.page_range {
            Some((first, last)) => out.push_str(&format!(",\"pageRange\":[{first},{last}]")),
            None => out.push_str(",\"pageRange\":null"),
        }
        out.push_str(&format!(",\"showOnFirstPage\":{}", self.show_on_first_page));
        out.push_str(&format!(",\"bandHeight\":{}}}", self.band_height));
        out
    }
}

/// Append a JSON-escaped string literal (`"…"`) to `out`. Same escaping as the
/// crate's other JSON emitters (`model::json`, the WASM layer).
fn json_escape(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// The running header and footer currently baked into a PDF, as recovered by
/// [`Document::header_footer`](crate::document::Document::header_footer). Each
/// side is `Some` only when a `/GPHF`-tagged span is present on the page(s);
/// `None` means no baked header (resp. footer) was found.
///
/// The recovered [`HeaderFooterSpec`] carries the **text** faithfully (the
/// important field for reflecting document state — e.g. a Word-like editor
/// toggle); `align`, `font_size`, `color`, etc. are best-effort defaults, since
/// the bake stores only the drawn text, not the original spec.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HeaderFooter {
    /// The recovered running header, or `None` when no baked header is present.
    pub header: Option<HeaderFooterSpec>,
    /// The recovered running footer, or `None` when no baked footer is present.
    pub footer: Option<HeaderFooterSpec>,
}

impl HeaderFooter {
    /// Serialise to JSON `{"header":<spec|null>,"footer":<spec|null>}`, each side
    /// using [`HeaderFooterSpec::to_json`] (or `null`). Hand-rolled,
    /// zero-dependency; the shape consumed by the SDK reader.
    pub fn to_json(&self) -> String {
        let mut out = String::from("{\"header\":");
        match &self.header {
            Some(h) => out.push_str(&h.to_json()),
            None => out.push_str("null"),
        }
        out.push_str(",\"footer\":");
        match &self.footer {
            Some(f) => out.push_str(&f.to_json()),
            None => out.push_str("null"),
        }
        out.push('}');
        out
    }
}

/// A minimal, tolerant JSON reader for a **flat** object: strings, numbers,
/// booleans, `null`, and number arrays. Object/array *values* it does not need
/// are skipped structurally. Zero-dependency, in the same spirit as
/// [`convert::grids`](crate::convert)'s `Reader`.
/// A minimal, zero-dependency JSON reader for small config objects. `pub(crate)`
/// so other features (e.g. `InfoFields::from_json`) can reuse it.
pub(crate) struct ObjReader<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> ObjReader<'a> {
    pub(crate) fn new(s: &'a str) -> Self {
        Self {
            b: s.as_bytes(),
            i: 0,
        }
    }

    fn done(&self) -> bool {
        self.i >= self.b.len()
    }

    fn ws(&mut self) {
        while self.i < self.b.len() && self.b[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.ws();
        self.b.get(self.i).copied()
    }

    fn eat(&mut self, c: u8) -> Option<()> {
        if self.peek()? == c {
            self.i += 1;
            Some(())
        } else {
            None
        }
    }

    /// `{ "key": value (, "key": value)* }` — invokes `member(self, key)` for
    /// each pair, which must consume exactly that pair's value.
    pub(crate) fn object(
        &mut self,
        mut member: impl FnMut(&mut Self, &str) -> Option<()>,
    ) -> Option<()> {
        self.eat(b'{')?;
        if self.peek()? == b'}' {
            self.i += 1;
            return Some(());
        }
        loop {
            let key = self.string()?;
            self.eat(b':')?;
            member(self, &key)?;
            match self.peek()? {
                b',' => self.i += 1,
                b'}' => {
                    self.i += 1;
                    return Some(());
                }
                _ => return None,
            }
        }
    }

    /// A JSON string. Standard escapes (`\" \\ \/ \n \r \t \b \f \uXXXX`,
    /// surrogate pairs) are decoded; UTF-8 bytes pass through.
    pub(crate) fn string(&mut self) -> Option<String> {
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

    fn unicode_escape(&mut self) -> Option<char> {
        let hi = self.hex4()?;
        if (0xD800..=0xDBFF).contains(&hi) {
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
        let mut v: u16 = 0;
        for _ in 0..4 {
            let d = *self.b.get(self.i)?;
            self.i += 1;
            let nibble = match d {
                b'0'..=b'9' => d - b'0',
                b'a'..=b'f' => d - b'a' + 10,
                b'A'..=b'F' => d - b'A' + 10,
                _ => return None,
            };
            v = (v << 4) | nibble as u16;
        }
        Some(v)
    }

    /// A JSON number (integer or float, incl. sign / exponent).
    fn number(&mut self) -> Option<f64> {
        self.ws();
        let start = self.i;
        while self.i < self.b.len() {
            match self.b[self.i] {
                b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E' => self.i += 1,
                _ => break,
            }
        }
        if self.i == start {
            return None;
        }
        std::str::from_utf8(&self.b[start..self.i])
            .ok()?
            .parse()
            .ok()
    }

    /// `[ number (, number)* ]`.
    fn number_array(&mut self) -> Option<Vec<f64>> {
        self.eat(b'[')?;
        let mut out = Vec::new();
        if self.peek()? == b']' {
            self.i += 1;
            return Some(out);
        }
        loop {
            out.push(self.number()?);
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

    fn boolean(&mut self) -> Option<bool> {
        match self.peek()? {
            b't' => {
                self.expect(b"true")?;
                Some(true)
            }
            b'f' => {
                self.expect(b"false")?;
                Some(false)
            }
            _ => None,
        }
    }

    fn null(&mut self) -> Option<()> {
        self.ws();
        self.expect(b"null")
    }

    fn expect(&mut self, word: &[u8]) -> Option<()> {
        self.ws();
        if self.b[self.i..].starts_with(word) {
            self.i += word.len();
            Some(())
        } else {
            None
        }
    }

    /// Consume and discard the value at the cursor (any JSON value), so unknown
    /// object keys don't abort the parse.
    pub(crate) fn skip_value(&mut self) -> Option<()> {
        match self.peek()? {
            b'"' => {
                self.string()?;
            }
            b'{' => {
                self.object(|p, _| p.skip_value())?;
            }
            b'[' => {
                self.eat(b'[')?;
                if self.peek()? == b']' {
                    self.i += 1;
                    return Some(());
                }
                loop {
                    self.skip_value()?;
                    match self.peek()? {
                        b',' => self.i += 1,
                        b']' => {
                            self.i += 1;
                            break;
                        }
                        _ => return None,
                    }
                }
            }
            b't' | b'f' => {
                self.boolean()?;
            }
            b'n' => {
                self.null()?;
            }
            _ => {
                self.number()?;
            }
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_text_substitutes_tokens() {
        let spec = HeaderFooterSpec {
            text: "Doc {{page}}/{{pages}}".into(),
            ..Default::default()
        };
        assert_eq!(spec.render_text(2, 5), "Doc 2/5");
    }

    #[test]
    fn from_json_full_object() {
        let spec = HeaderFooterSpec::from_json(
            r#"{"text":"P {{page}}","align":"right","fontSize":12,
               "color":[1,0,0],"pageRange":[2,4],"showOnFirstPage":false,
               "bandHeight":40}"#,
        )
        .unwrap();
        assert_eq!(spec.text, "P {{page}}");
        assert_eq!(spec.align, Align::Right);
        assert_eq!(spec.font_size, 12.0);
        assert_eq!(spec.color, [1.0, 0.0, 0.0]);
        assert_eq!(spec.page_range, Some((2, 4)));
        assert!(!spec.show_on_first_page);
        assert_eq!(spec.band_height, 40.0);
    }

    #[test]
    fn from_json_defaults_and_null_range() {
        let spec = HeaderFooterSpec::from_json(r#"{"text":"x","pageRange":null}"#).unwrap();
        assert_eq!(spec.text, "x");
        assert_eq!(spec.align, Align::Left);
        assert_eq!(spec.page_range, None);
        assert!(spec.show_on_first_page);
    }

    #[test]
    fn from_json_ignores_unknown_keys() {
        let spec = HeaderFooterSpec::from_json(r#"{"extra":{"a":[1,2]},"text":"y"}"#).unwrap();
        assert_eq!(spec.text, "y");
    }

    #[test]
    fn from_json_rejects_garbage() {
        assert!(HeaderFooterSpec::from_json("not json").is_none());
        assert!(HeaderFooterSpec::from_json(r#"{"text":"x"} trailing"#).is_none());
    }

    #[test]
    fn align_from_str_lossy() {
        assert_eq!(Align::from_str_lossy("CENTER"), Align::Center);
        assert_eq!(Align::from_str_lossy(" right "), Align::Right);
        assert_eq!(Align::from_str_lossy("weird"), Align::Left);
    }

    #[test]
    fn spec_to_json_round_trips_through_from_json() {
        let spec = HeaderFooterSpec {
            text: "Doc \"{{page}}\"\n\\end".into(),
            align: Align::Center,
            font_size: 11.0,
            color: [0.1, 0.2, 0.3],
            page_range: Some((2, 9)),
            show_on_first_page: false,
            band_height: 24.0,
        };
        let back = HeaderFooterSpec::from_json(&spec.to_json()).unwrap();
        assert_eq!(back, spec);
    }

    #[test]
    fn header_footer_to_json_shapes_both_sides() {
        let hf = HeaderFooter {
            header: Some(HeaderFooterSpec {
                text: "H".into(),
                ..Default::default()
            }),
            footer: None,
        };
        let json = hf.to_json();
        assert!(json.starts_with("{\"header\":{"));
        assert!(json.contains("\"footer\":null"));

        let empty = HeaderFooter::default().to_json();
        assert_eq!(empty, "{\"header\":null,\"footer\":null}");
    }
}
