//! Rich-fidelity **Office → PDF** import.
//!
//! Where [`super::reverse`] flattens an Office file to plain paragraphs, this
//! module maps it to **styled HTML** (headings, bold/italic, colour, tables,
//! lists, images) and renders it through the engine's own
//! [HTML→PDF pipeline](crate::html::render). The result keeps document
//! structure and typography — without any LibreOffice/headless dependency.
//!
//! Office containers are ZIP-of-XML, read with [`super::zip::read_zip`]. The XML
//! is walked with a tiny std-only streaming tokenizer ([`Xml`]); there is no
//! regex or XML crate. Each format's mapper emits an HTML body, wrapped in a
//! shared default stylesheet, then handed to [`crate::html::render`].
//!
//! Priority order of fidelity: DOCX, XLSX, PPTX, ODT, ODS, ODP, and a
//! best-effort text-only path for legacy OLE2 `.doc/.xls/.ppt`.

use super::zip::read_zip;
use std::collections::BTreeMap;

// US-Letter, points. Sheets/slides render landscape for more horizontal room.
const PAGE_W: f64 = 612.0;
const PAGE_H: f64 = 792.0;
const LAND_W: f64 = 792.0;
const LAND_H: f64 = 612.0;
const MARGIN: f64 = 36.0;

/// Auto-detect an Office container and render it to a styled PDF, or `None` if
/// the bytes are not a recognized OOXML/ODF archive (or, for legacy OLE2, hold
/// no readable text).
///
/// Dispatch mirrors [`super::reverse::office_to_pdf`] but produces rich output:
/// `word/document.xml`→DOCX, `ppt/presentation.xml`→PPTX, `xl/workbook.xml`→
/// XLSX, else the ODF `mimetype` marker → ODT/ODS/ODP, else the OLE2 magic →
/// legacy text extraction.
pub fn office_to_pdf(bytes: &[u8]) -> Option<Vec<u8>> {
    // Legacy OLE2 Compound File (.doc/.xls/.ppt) — not a ZIP.
    if bytes.len() >= 8 && bytes[..8] == [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1] {
        return ole2_to_pdf(bytes);
    }

    let zip = read_zip(bytes);
    if zip.contains_key("word/document.xml") {
        Some(docx_to_pdf(&zip))
    } else if zip.contains_key("ppt/presentation.xml") {
        Some(pptx_to_pdf(&zip))
    } else if zip.contains_key("xl/workbook.xml") {
        Some(xlsx_to_pdf(&zip))
    } else if let Some(mimetype) = zip.get("mimetype") {
        let mt = String::from_utf8_lossy(mimetype);
        if mt.contains("opendocument.text") {
            Some(odt_to_pdf(&zip))
        } else if mt.contains("opendocument.spreadsheet") {
            Some(ods_to_pdf(&zip))
        } else if mt.contains("opendocument.presentation") {
            Some(odp_to_pdf(&zip))
        } else {
            None
        }
    } else {
        None
    }
}

// ───────────────────────────── HTML shell + escaping ──────────────────────────

/// Default stylesheet wrapped around every generated body. Sensible document
/// defaults plus collapsed table borders; sizes in points so they map straight
/// to the renderer.
const BASE_CSS: &str = "\
body{font-family:sans-serif;font-size:11pt;color:#000}\
h1{font-size:20pt}h2{font-size:16pt}h3{font-size:13pt}\
h4{font-size:12pt}h5{font-size:11pt}h6{font-size:10pt}\
p{margin-top:4pt;margin-bottom:4pt}\
table{border-collapse:collapse;margin-top:6pt;margin-bottom:6pt}\
td,th{border:.5pt solid #888;padding:2pt;text-align:left}\
th{background:#eee}\
img{margin-top:4pt;margin-bottom:4pt}";

/// Wrap a body fragment in a minimal HTML document carrying [`BASE_CSS`].
fn html_doc(body: &str) -> String {
    format!("<html><head><style>{BASE_CSS}</style></head><body>{body}</body></html>")
}

/// Escape text for HTML body / double-quoted attribute context.
fn esc(s: &str, out: &mut String) {
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
}

/// Convenience: escape and return.
fn escaped(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    esc(s, &mut o);
    o
}

// ───────────────────────────── streaming XML walker ───────────────────────────

/// One token from the XML stream.
#[derive(Debug, Clone, PartialEq)]
enum Tok {
    /// `<name …>` — `(name, attrs, self_closing)`.
    Open(String, Vec<(String, String)>, bool),
    /// `</name>`.
    Close(String),
    /// Decoded text content between tags.
    Text(String),
}

/// A minimal pull tokenizer over an XML string: emits opens (with attributes),
/// closes and decoded text; skips comments, declarations, PIs and `<![CDATA[`
/// wrappers (their contents are surfaced as text). Local-name aware helpers let
/// callers ignore the namespace prefix.
struct Xml<'a> {
    s: &'a [u8],
    src: &'a str,
    i: usize,
}

impl<'a> Xml<'a> {
    fn new(src: &'a str) -> Xml<'a> {
        Xml {
            s: src.as_bytes(),
            src,
            i: 0,
        }
    }

    fn next(&mut self) -> Option<Tok> {
        if self.i >= self.s.len() {
            return None;
        }
        if self.s[self.i] == b'<' {
            // Comment / declaration / PI / CDATA.
            if self.src[self.i..].starts_with("<!--") {
                self.i = self.src[self.i..]
                    .find("-->")
                    .map(|j| self.i + j + 3)
                    .unwrap_or(self.s.len());
                return self.next();
            }
            if self.src[self.i..].starts_with("<![CDATA[") {
                let start = self.i + 9;
                let end = self.src[start..]
                    .find("]]>")
                    .map(|j| start + j)
                    .unwrap_or(self.s.len());
                let text = self.src[start..end].to_string();
                self.i = (end + 3).min(self.s.len());
                if text.is_empty() {
                    return self.next();
                }
                return Some(Tok::Text(text));
            }
            if matches!(self.s.get(self.i + 1), Some(b'!') | Some(b'?')) {
                self.i = self.src[self.i..]
                    .find('>')
                    .map(|j| self.i + j + 1)
                    .unwrap_or(self.s.len());
                return self.next();
            }
            // End tag.
            if self.s.get(self.i + 1) == Some(&b'/') {
                let end = self.src[self.i..]
                    .find('>')
                    .map(|j| self.i + j)
                    .unwrap_or(self.s.len());
                let name = self.src[self.i + 2..end].trim().to_string();
                self.i = (end + 1).min(self.s.len());
                return Some(Tok::Close(name));
            }
            // Start tag.
            let end = match self.src[self.i..].find('>') {
                Some(j) => self.i + j,
                None => {
                    self.i = self.s.len();
                    return None;
                }
            };
            let raw = &self.src[self.i + 1..end];
            self.i = end + 1;
            let self_closing = raw.trim_end().ends_with('/');
            let raw = raw.trim_end().trim_end_matches('/');
            let (name, attrs) = parse_start(raw);
            if name.is_empty() {
                return self.next();
            }
            Some(Tok::Open(name, attrs, self_closing))
        } else {
            let end = self.src[self.i..]
                .find('<')
                .map(|j| self.i + j)
                .unwrap_or(self.s.len());
            let text = decode(&self.src[self.i..end]);
            self.i = end;
            if text.is_empty() {
                return self.next();
            }
            Some(Tok::Text(text))
        }
    }
}

/// Split a start-tag body into `(name, attrs)`.
fn parse_start(raw: &str) -> (String, Vec<(String, String)>) {
    let raw = raw.trim();
    let mut name_end = raw.len();
    for (i, c) in raw.char_indices() {
        if c.is_whitespace() {
            name_end = i;
            break;
        }
    }
    let name = raw[..name_end].to_string();
    let mut attrs = Vec::new();
    let b = raw.as_bytes();
    let mut i = name_end;
    while i < b.len() {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let ns = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'=' {
            i += 1;
        }
        let an = raw[ns..i].to_string();
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut av = String::new();
        if i < b.len() && b[i] == b'=' {
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                let q = b[i];
                i += 1;
                let vs = i;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
                av = decode(&raw[vs..i.min(raw.len())]);
                i = (i + 1).min(b.len());
            } else {
                let vs = i;
                while i < b.len() && !b[i].is_ascii_whitespace() {
                    i += 1;
                }
                av = decode(&raw[vs..i]);
            }
        }
        if !an.is_empty() {
            attrs.push((an, av));
        }
    }
    (name, attrs)
}

/// The local name of a possibly-namespaced tag (`w:p` → `p`).
fn local(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

/// Look up an attribute by **local** name (namespace prefix ignored).
fn attr<'b>(attrs: &'b [(String, String)], local_name: &str) -> Option<&'b str> {
    attrs
        .iter()
        .find(|(k, _)| local(k).eq_ignore_ascii_case(local_name))
        .map(|(_, v)| v.as_str())
}

/// Decode XML entities — delegates to the shared decoder in [`super::reverse`].
fn decode(s: &str) -> String {
    super::reverse::unescape(s)
}

// ───────────────────────────────── rels parsing ───────────────────────────────

/// Parse an OOXML `_rels/*.rels` part into `rId → Target`.
fn parse_rels(xml: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let mut xml = Xml::new(xml);
    while let Some(tok) = xml.next() {
        if let Tok::Open(name, attrs, _) = tok {
            if local(&name) == "Relationship" {
                if let (Some(id), Some(target)) = (attr(&attrs, "Id"), attr(&attrs, "Target")) {
                    map.insert(id.to_string(), target.to_string());
                }
            }
        }
    }
    map
}

/// Resolve a relationship `Target` (often `media/img.png` or `../media/img.png`)
/// against the OOXML part folder `base` (e.g. `word` or `ppt`) to a zip key.
fn resolve_target(base: &str, target: &str) -> String {
    let t = target.trim_start_matches('/');
    if let Some(rest) = t.strip_prefix("../") {
        // Relative to the package root (drop one `base` segment).
        rest.to_string()
    } else {
        format!("{base}/{t}")
    }
}

// ─────────────────────────── image → data URI embedding ───────────────────────

/// Map a media filename to an image MIME the renderer can decode (PNG/JPEG/WebP).
/// Returns `None` for vector/legacy formats (EMF/WMF/TIFF/SVG) the engine can't
/// rasterize — those are skipped rather than emitted broken.
fn image_mime(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".png") {
        Some("image/png")
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        Some("image/jpeg")
    } else if lower.ends_with(".webp") {
        Some("image/webp")
    } else {
        None
    }
}

/// Build an `<img src="data:…">` tag for a media zip entry, or `None` if absent
/// or an unsupported format.
fn img_tag(zip: &BTreeMap<String, Vec<u8>>, key: &str) -> Option<String> {
    let mime = image_mime(key)?;
    let bytes = zip.get(key)?;
    Some(format!(
        "<img src=\"data:{mime};base64,{}\">",
        super::base64(bytes)
    ))
}

// ════════════════════════════════════ DOCX ════════════════════════════════════

/// DOCX → styled HTML → PDF. Maps paragraph styles to headings, run properties
/// (`b`/`i`/`sz`/`color`/`u`) to inline `<span>`s, `w:tbl`→`<table>`, and inline
/// images via `a:blip r:embed` resolved through the document relationships.
pub fn docx_to_pdf(zip: &BTreeMap<String, Vec<u8>>) -> Vec<u8> {
    let doc = part(zip, "word/document.xml");
    let rels = zip
        .get("word/_rels/document.xml.rels")
        .map(|b| parse_rels(&String::from_utf8_lossy(b)))
        .unwrap_or_default();

    let mut body = String::new();
    docx_body(&doc, zip, &rels, &mut body);
    crate::html::render(&html_doc(&body), &[], PAGE_W, PAGE_H, MARGIN)
}

/// Run/paragraph state while walking `w:document`.
#[derive(Default, Clone)]
struct RunStyle {
    bold: bool,
    italic: bool,
    underline: bool,
    size_half_pt: Option<f64>,
    color: Option<String>,
}

impl RunStyle {
    /// Open a `<span style>` reflecting this run's properties; empty if none.
    fn open_span(&self) -> String {
        let mut css = String::new();
        if self.bold {
            css.push_str("font-weight:bold;");
        }
        if self.italic {
            css.push_str("font-style:italic;");
        }
        if self.underline {
            css.push_str("text-decoration:underline;");
        }
        if let Some(half) = self.size_half_pt {
            css.push_str(&format!("font-size:{}pt;", half / 2.0));
        }
        if let Some(c) = &self.color {
            css.push_str(&format!("color:#{c};"));
        }
        if css.is_empty() {
            String::new()
        } else {
            format!("<span style=\"{css}\">")
        }
    }
}

/// Walk a DOCX body region (`w:body` or a `w:tc` cell), emitting HTML into `out`.
fn docx_body(
    xml: &str,
    zip: &BTreeMap<String, Vec<u8>>,
    rels: &BTreeMap<String, String>,
    out: &mut String,
) {
    let mut x = Xml::new(xml);
    // Walk only the top level of this region; tables and paragraphs recurse via
    // slices so a `w:tbl` is never double-emitted as loose paragraphs.
    docx_walk(&mut x, zip, rels, out, None);
}

/// Recursive DOCX walker. `stop` is the local tag name that ends the current
/// region (`None` at the top level). Handles `w:p`, `w:tbl`.
fn docx_walk(
    x: &mut Xml,
    zip: &BTreeMap<String, Vec<u8>>,
    rels: &BTreeMap<String, String>,
    out: &mut String,
    stop: Option<&str>,
) {
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, _, sc) => {
                let ln = local(&name);
                if ln == "p" && !sc {
                    docx_paragraph(x, zip, rels, out);
                } else if ln == "tbl" && !sc {
                    docx_table(x, zip, rels, out);
                }
                // Other containers (body, sdt, etc.) are transparent: keep
                // walking into them at this level.
            }
            Tok::Close(name) => {
                if Some(local(&name)) == stop {
                    return;
                }
            }
            Tok::Text(_) => {}
        }
    }
}

/// Emit one `w:p` (already consumed its open tag) until `</w:p>`.
fn docx_paragraph(
    x: &mut Xml,
    zip: &BTreeMap<String, Vec<u8>>,
    rels: &BTreeMap<String, String>,
    out: &mut String,
) {
    let mut heading: Option<u8> = None;
    let mut inner = String::new();
    let mut run = RunStyle::default();
    let mut in_rpr = false; // inside <w:rPr> (run properties)
    let mut in_ppr = false; // inside <w:pPr> (paragraph properties)
    let mut depth = 0i32; // nesting of <w:r> runs (to scope rPr)

    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => {
                let ln = local(&name);
                match ln {
                    "pPr" if !sc => in_ppr = true,
                    "rPr" if !sc => in_rpr = true,
                    "pStyle" => {
                        if in_ppr {
                            if let Some(v) = attr(&attrs, "val") {
                                heading = heading_level(v);
                            }
                        }
                    }
                    "r" if !sc => {
                        depth += 1;
                        run = RunStyle::default();
                    }
                    "b" if in_rpr => run.bold = !matches!(attr(&attrs, "val"), Some("0") | Some("false")),
                    "i" if in_rpr => run.italic = !matches!(attr(&attrs, "val"), Some("0") | Some("false")),
                    "u" if in_rpr => {
                        if !matches!(attr(&attrs, "val"), Some("none")) {
                            run.underline = true;
                        }
                    }
                    "sz" if in_rpr => {
                        run.size_half_pt = attr(&attrs, "val").and_then(|v| v.parse().ok());
                    }
                    "color" if in_rpr => {
                        if let Some(v) = attr(&attrs, "val") {
                            if v != "auto" && is_hex6(v) {
                                run.color = Some(v.to_ascii_uppercase());
                            }
                        }
                    }
                    "tab" => inner.push(' '),
                    "br" | "cr" => inner.push_str("<br>"),
                    "blip" => {
                        if let Some(rid) = attr(&attrs, "embed").or_else(|| attr(&attrs, "link")) {
                            if let Some(tag) = rels
                                .get(rid)
                                .map(|t| resolve_target("word", t))
                                .and_then(|k| img_tag(zip, &k))
                            {
                                inner.push_str(&tag);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Tok::Close(name) => {
                let ln = local(&name);
                match ln {
                    "p" => break,
                    "pPr" => in_ppr = false,
                    "rPr" => in_rpr = false,
                    "r" => depth = (depth - 1).max(0),
                    _ => {}
                }
            }
            Tok::Text(t) => {
                // Only surface text that lives inside a run (skip stray
                // property text). `w:t` content arrives here.
                if depth > 0 && !t.is_empty() {
                    let span = run.open_span();
                    if span.is_empty() {
                        esc(&t, &mut inner);
                    } else {
                        inner.push_str(&span);
                        esc(&t, &mut inner);
                        inner.push_str("</span>");
                    }
                }
            }
        }
    }

    let trimmed = inner.trim();
    match heading {
        Some(n) if !trimmed.is_empty() => {
            out.push_str(&format!("<h{n}>{inner}</h{n}>"));
        }
        _ => {
            // Always emit a <p> (even empty) to preserve blank-line spacing.
            out.push_str(&format!("<p>{inner}</p>"));
        }
    }
}

/// Emit one `w:tbl` (open already consumed) as an HTML `<table>`.
fn docx_table(
    x: &mut Xml,
    zip: &BTreeMap<String, Vec<u8>>,
    rels: &BTreeMap<String, String>,
    out: &mut String,
) {
    out.push_str("<table>");
    let mut in_row = false;
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, _, sc) => {
                let ln = local(&name);
                if ln == "tr" && !sc {
                    out.push_str("<tr>");
                    in_row = true;
                } else if ln == "tc" && !sc {
                    out.push_str("<td>");
                    // Cell body recurses with the same walker; stop at </w:tc>.
                    let mut cell = String::new();
                    docx_walk(x, zip, rels, &mut cell, Some("tc"));
                    out.push_str(cell.trim());
                    out.push_str("</td>");
                }
            }
            Tok::Close(name) => {
                let ln = local(&name);
                if ln == "tr" {
                    out.push_str("</tr>");
                    in_row = false;
                } else if ln == "tbl" {
                    break;
                }
            }
            Tok::Text(_) => {}
        }
    }
    let _ = in_row;
    out.push_str("</table>");
}

/// Map a DOCX style id (`Heading1`, `Title`, …) to a heading level 1..=6.
fn heading_level(style: &str) -> Option<u8> {
    let s = style.to_ascii_lowercase();
    if s == "title" {
        return Some(1);
    }
    if s == "subtitle" {
        return Some(2);
    }
    let digits = s.trim_start_matches("heading").trim_start_matches('-');
    if s.starts_with("heading") {
        if let Ok(n) = digits.parse::<u8>() {
            return Some(n.clamp(1, 6));
        }
        return Some(1);
    }
    None
}

// ════════════════════════════════════ XLSX ════════════════════════════════════

/// XLSX → one HTML `<table>` per sheet (page break between), sheet name as
/// `<h2>`. Resolves `t="s"` shared strings and the exporter's own
/// `t="inlineStr"` cells, positioning each cell by its column letter so columns
/// align. Rendered landscape for width.
pub fn xlsx_to_pdf(zip: &BTreeMap<String, Vec<u8>>) -> Vec<u8> {
    let shared = zip
        .get("xl/sharedStrings.xml")
        .map(|b| parse_shared_strings(&String::from_utf8_lossy(b)))
        .unwrap_or_default();

    // Sheet name order from the workbook; fall back to file order.
    let names = zip
        .get("xl/workbook.xml")
        .map(|b| parse_sheet_names(&String::from_utf8_lossy(b)))
        .unwrap_or_default();

    let mut sheets: Vec<(usize, String)> = zip
        .iter()
        .filter(|(k, _)| k.starts_with("xl/worksheets/sheet") && k.ends_with(".xml"))
        .filter_map(|(k, v)| {
            let n: usize = k["xl/worksheets/sheet".len()..k.len() - 4].parse().ok()?;
            Some((n, String::from_utf8_lossy(v).into_owned()))
        })
        .collect();
    sheets.sort_by_key(|(n, _)| *n);

    let mut body = String::new();
    for (idx, (n, xml)) in sheets.iter().enumerate() {
        if idx > 0 {
            body.push_str("<div style=\"page-break-before:always\"></div>");
        }
        let title = names
            .get(idx)
            .cloned()
            .unwrap_or_else(|| format!("Sheet {n}"));
        body.push_str(&format!("<h2>{}</h2>", escaped(&title)));
        body.push_str(&xlsx_sheet_table(xml, &shared));
    }
    if sheets.is_empty() {
        body.push_str("<p></p>");
    }
    crate::html::render(&html_doc(&body), &[], LAND_W, LAND_H, MARGIN)
}

/// Render one worksheet XML to an HTML `<table>`, gap-filling so cells land in
/// their declared column (`r="C3"`).
fn xlsx_sheet_table(xml: &str, shared: &[String]) -> String {
    let mut out = String::from("<table>");
    let mut x = Xml::new(xml);
    let mut in_sheet_data = false;
    let mut row_cells: Vec<(usize, String)> = Vec::new(); // (col_index, html)
    let mut row_open = false;

    // Current-cell scratch.
    let mut cell_col = 0usize;
    let mut cell_type = String::new();
    let mut cell_text = String::new();
    let mut in_cell = false;
    let mut in_value = false; // inside <v> or <t>

    let flush_row = |row_cells: &mut Vec<(usize, String)>, out: &mut String| {
        if row_cells.is_empty() {
            out.push_str("<tr></tr>");
            return;
        }
        out.push_str("<tr>");
        let max_col = row_cells.iter().map(|(c, _)| *c).max().unwrap_or(0);
        let mut by_col: BTreeMap<usize, String> = BTreeMap::new();
        for (c, h) in row_cells.drain(..) {
            by_col.insert(c, h);
        }
        for c in 0..=max_col {
            match by_col.get(&c) {
                Some(h) => out.push_str(&format!("<td>{h}</td>")),
                None => out.push_str("<td></td>"),
            }
        }
        out.push_str("</tr>");
    };

    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => match local(&name) {
                "sheetData" => in_sheet_data = true,
                "row" if in_sheet_data && !sc => {
                    row_open = true;
                    row_cells.clear();
                }
                "c" if in_sheet_data => {
                    in_cell = true;
                    cell_text.clear();
                    cell_type = attr(&attrs, "t").unwrap_or("n").to_string();
                    cell_col = attr(&attrs, "r").map(col_of_ref).unwrap_or(0);
                    if sc {
                        in_cell = false;
                    }
                }
                "v" | "t" if in_cell => in_value = true,
                _ => {}
            },
            Tok::Text(t) => {
                if in_cell && in_value {
                    cell_text.push_str(&t);
                }
            }
            Tok::Close(name) => match local(&name) {
                "v" | "t" => in_value = false,
                "c" => {
                    if in_cell {
                        let resolved = if cell_type == "s" {
                            cell_text
                                .trim()
                                .parse::<usize>()
                                .ok()
                                .and_then(|i| shared.get(i))
                                .cloned()
                                .unwrap_or_default()
                        } else {
                            cell_text.clone()
                        };
                        row_cells.push((cell_col, escaped(resolved.trim())));
                    }
                    in_cell = false;
                }
                "row" => {
                    if row_open {
                        flush_row(&mut row_cells, &mut out);
                        row_open = false;
                    }
                }
                "sheetData" => in_sheet_data = false,
                _ => {}
            },
        }
    }
    out.push_str("</table>");
    out
}

/// Parse `xl/sharedStrings.xml` into an index→string table. Concatenates the
/// `<t>` runs inside each `<si>`.
fn parse_shared_strings(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut x = Xml::new(xml);
    let mut in_si = false;
    let mut in_t = false;
    let mut cur = String::new();
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, _, sc) => match local(&name) {
                "si" if !sc => {
                    in_si = true;
                    cur.clear();
                }
                "t" if in_si && !sc => in_t = true,
                _ => {}
            },
            Tok::Text(t) => {
                if in_si && in_t {
                    cur.push_str(&t);
                }
            }
            Tok::Close(name) => match local(&name) {
                "t" => in_t = false,
                "si" => {
                    if in_si {
                        out.push(std::mem::take(&mut cur));
                        in_si = false;
                    }
                }
                _ => {}
            },
        }
    }
    out
}

/// Parse `xl/workbook.xml` `<sheet name=…>` in document order.
fn parse_sheet_names(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut x = Xml::new(xml);
    while let Some(tok) = x.next() {
        if let Tok::Open(name, attrs, _) = tok {
            if local(&name) == "sheet" {
                if let Some(nm) = attr(&attrs, "name") {
                    out.push(nm.to_string());
                }
            }
        }
    }
    out
}

/// Column index (0-based) from a cell reference like `"C3"` / `"AB12"`.
fn col_of_ref(r: &str) -> usize {
    let mut col = 0usize;
    for c in r.chars() {
        if c.is_ascii_alphabetic() {
            col = col * 26 + (c.to_ascii_uppercase() as usize - 'A' as usize + 1);
        } else {
            break;
        }
    }
    col.saturating_sub(1)
}

// ════════════════════════════════════ PPTX ════════════════════════════════════

/// PPTX → one page per slide (page break between). Text from `a:p`/`a:r`/`a:t`;
/// images via `a:blip r:embed` resolved through each slide's relationships.
pub fn pptx_to_pdf(zip: &BTreeMap<String, Vec<u8>>) -> Vec<u8> {
    let mut slides: Vec<(usize, String)> = zip
        .iter()
        .filter(|(k, _)| k.starts_with("ppt/slides/slide") && k.ends_with(".xml"))
        .filter_map(|(k, v)| {
            let n: usize = k["ppt/slides/slide".len()..k.len() - 4].parse().ok()?;
            Some((n, String::from_utf8_lossy(v).into_owned()))
        })
        .collect();
    slides.sort_by_key(|(n, _)| *n);

    let mut body = String::new();
    for (idx, (n, xml)) in slides.iter().enumerate() {
        if idx > 0 {
            body.push_str("<div style=\"page-break-before:always\"></div>");
        }
        let rels = zip
            .get(&format!("ppt/slides/_rels/slide{n}.xml.rels"))
            .map(|b| parse_rels(&String::from_utf8_lossy(b)))
            .unwrap_or_default();
        pptx_slide(xml, zip, &rels, &mut body);
    }
    if slides.is_empty() {
        body.push_str("<p></p>");
    }
    crate::html::render(&html_doc(&body), &[], LAND_W, LAND_H, MARGIN)
}

/// Emit one slide's text paragraphs and images into `out`.
fn pptx_slide(
    xml: &str,
    zip: &BTreeMap<String, Vec<u8>>,
    rels: &BTreeMap<String, String>,
    out: &mut String,
) {
    let mut x = Xml::new(xml);
    let mut para = String::new();
    let mut in_para = false;
    let mut run = RunStyle::default();
    let mut in_rpr = false;
    let mut in_text = false;

    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => match local(&name) {
                "p" if !sc => {
                    in_para = true;
                    para.clear();
                }
                "rPr" if !sc => {
                    in_rpr = true;
                    // a:rPr carries b/i/sz as attributes.
                    run = RunStyle {
                        bold: matches!(attr(&attrs, "b"), Some("1")),
                        italic: matches!(attr(&attrs, "i"), Some("1")),
                        size_half_pt: attr(&attrs, "sz")
                            .and_then(|v| v.parse::<f64>().ok())
                            .map(|sz| sz / 50.0), // hundredths-pt → half-pt
                        ..RunStyle::default()
                    };
                    if sc {
                        in_rpr = false;
                    }
                }
                "srgbClr" if in_rpr => {
                    if let Some(v) = attr(&attrs, "val") {
                        if is_hex6(v) {
                            run.color = Some(v.to_ascii_uppercase());
                        }
                    }
                }
                "t" if !sc => in_text = true,
                "br" => para.push_str("<br>"),
                "blip" => {
                    if let Some(rid) = attr(&attrs, "embed").or_else(|| attr(&attrs, "link")) {
                        if let Some(tag) = rels
                            .get(rid)
                            .map(|t| resolve_target("ppt", t))
                            .and_then(|k| img_tag(zip, &k))
                        {
                            // Flush any pending paragraph, then place the image.
                            if !para.trim().is_empty() {
                                out.push_str(&format!("<p>{}</p>", para.trim()));
                                para.clear();
                            }
                            out.push_str(&tag);
                        }
                    }
                }
                _ => {}
            },
            Tok::Text(t) => {
                if in_para && in_text && !t.is_empty() {
                    let span = run.open_span();
                    if span.is_empty() {
                        esc(&t, &mut para);
                    } else {
                        para.push_str(&span);
                        esc(&t, &mut para);
                        para.push_str("</span>");
                    }
                }
            }
            Tok::Close(name) => match local(&name) {
                "t" => in_text = false,
                "rPr" => in_rpr = false,
                "p" => {
                    if in_para && !para.trim().is_empty() {
                        out.push_str(&format!("<p>{}</p>", para.trim()));
                    }
                    in_para = false;
                }
                _ => {}
            },
        }
    }
}

// ════════════════════════════════════ ODF ═════════════════════════════════════

/// Build a `style-name → CSS` map from the automatic + named text styles in an
/// ODF part (`content.xml` or `styles.xml`). Captures `fo:font-weight`,
/// `fo:font-style`, `fo:color`, `fo:font-size` from each
/// `style:style`→`style:text-properties`.
fn odf_text_styles(xml: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let mut x = Xml::new(xml);
    let mut cur_name: Option<String> = None;
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, _) => match local(&name) {
                "style" => {
                    // style:style name="…"
                    cur_name = attr(&attrs, "name").map(|s| s.to_string());
                }
                "text-properties" => {
                    if let Some(nm) = &cur_name {
                        let mut css = String::new();
                        if let Some(w) = attr(&attrs, "font-weight") {
                            if w == "bold" {
                                css.push_str("font-weight:bold;");
                            }
                        }
                        if let Some(s) = attr(&attrs, "font-style") {
                            if s == "italic" || s == "oblique" {
                                css.push_str("font-style:italic;");
                            }
                        }
                        if let Some(c) = attr(&attrs, "color") {
                            let hex = c.trim_start_matches('#');
                            if is_hex6(hex) {
                                css.push_str(&format!("color:#{};", hex.to_ascii_uppercase()));
                            }
                        }
                        if let Some(u) = attr(&attrs, "text-underline-style") {
                            if u != "none" {
                                css.push_str("text-decoration:underline;");
                            }
                        }
                        if let Some(sz) = attr(&attrs, "font-size") {
                            if let Some(pt) = parse_odf_pt(sz) {
                                css.push_str(&format!("font-size:{pt}pt;"));
                            }
                        }
                        if !css.is_empty() {
                            map.insert(nm.clone(), css);
                        }
                    }
                }
                _ => {}
            },
            Tok::Close(name) => {
                if local(&name) == "style" {
                    cur_name = None;
                }
            }
            Tok::Text(_) => {}
        }
    }
    map
}

/// Parse an ODF length like `12pt`, `0.5cm`, `14px` to points (best effort).
fn parse_odf_pt(v: &str) -> Option<f64> {
    let v = v.trim();
    if let Some(n) = v.strip_suffix("pt") {
        n.trim().parse::<f64>().ok()
    } else if let Some(n) = v.strip_suffix("cm") {
        n.trim().parse::<f64>().ok().map(|c| c * 28.3464567)
    } else if let Some(n) = v.strip_suffix("mm") {
        n.trim().parse::<f64>().ok().map(|m| m * 2.83464567)
    } else if let Some(n) = v.strip_suffix("in") {
        n.trim().parse::<f64>().ok().map(|i| i * 72.0)
    } else if let Some(n) = v.strip_suffix("px") {
        n.trim().parse::<f64>().ok().map(|p| p * 0.75)
    } else {
        v.parse::<f64>().ok()
    }
}

/// ODT → styled HTML → PDF. `text:h`→`<hN>`, `text:p`→`<p>`, `text:span`
/// styled via the automatic/named style map, `table:table`→`<table>`,
/// `draw:image xlink:href`→`<img>`.
pub fn odt_to_pdf(zip: &BTreeMap<String, Vec<u8>>) -> Vec<u8> {
    let content = part(zip, "content.xml");
    let mut styles = zip
        .get("styles.xml")
        .map(|b| odf_text_styles(&String::from_utf8_lossy(b)))
        .unwrap_or_default();
    // Automatic styles in content.xml take precedence / add to the named ones.
    styles.extend(odf_text_styles(&content));

    let mut body = String::new();
    odf_walk(&mut Xml::new(&content), zip, &styles, &mut body, None);
    crate::html::render(&html_doc(&body), &[], PAGE_W, PAGE_H, MARGIN)
}

/// Recursive ODF text walker (shared by ODT body and table cells). `stop` ends
/// the region. Handles `text:h`, `text:p`, `table:table`.
fn odf_walk(
    x: &mut Xml,
    zip: &BTreeMap<String, Vec<u8>>,
    styles: &BTreeMap<String, String>,
    out: &mut String,
    stop: Option<&str>,
) {
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => {
                let ln = local(&name);
                match ln {
                    "h" if !sc => {
                        let lvl = attr(&attrs, "outline-level")
                            .and_then(|v| v.parse::<u8>().ok())
                            .unwrap_or(1)
                            .clamp(1, 6);
                        let inner = odf_inline(x, zip, styles, "h");
                        if !inner.trim().is_empty() {
                            out.push_str(&format!("<h{lvl}>{inner}</h{lvl}>"));
                        }
                    }
                    "p" if !sc => {
                        let inner = odf_inline(x, zip, styles, "p");
                        out.push_str(&format!("<p>{inner}</p>"));
                    }
                    "table" if !sc => odf_table(x, zip, styles, out),
                    _ => {}
                }
            }
            Tok::Close(name) => {
                if Some(local(&name)) == stop {
                    return;
                }
            }
            Tok::Text(_) => {}
        }
    }
}

/// Collect the inline content of an ODF block (`text:p` / `text:h`) until its
/// matching close, honoring `text:span` styles, `text:tab`/`text:line-break`
/// and `draw:image`.
fn odf_inline(
    x: &mut Xml,
    zip: &BTreeMap<String, Vec<u8>>,
    styles: &BTreeMap<String, String>,
    block: &str,
) -> String {
    let mut out = String::new();
    // Stack of currently-open span styles (to close them in order).
    let mut span_depth = 0i32;
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => {
                let ln = local(&name);
                match ln {
                    "span" if !sc => {
                        let css = attr(&attrs, "style-name")
                            .and_then(|n| styles.get(n))
                            .cloned()
                            .unwrap_or_default();
                        if css.is_empty() {
                            out.push_str("<span>");
                        } else {
                            out.push_str(&format!("<span style=\"{css}\">"));
                        }
                        span_depth += 1;
                    }
                    "tab" => out.push(' '),
                    "line-break" => out.push_str("<br>"),
                    "s" => {
                        // text:s = run of spaces (count via text:c).
                        let n = attr(&attrs, "c").and_then(|v| v.parse::<usize>().ok()).unwrap_or(1);
                        for _ in 0..n {
                            out.push(' ');
                        }
                    }
                    "image" => {
                        if let Some(href) = attr(&attrs, "href") {
                            let key = href.trim_start_matches('/').to_string();
                            if let Some(tag) = img_tag(zip, &key) {
                                out.push_str(&tag);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Tok::Text(t) => esc(&t, &mut out),
            Tok::Close(name) => {
                let ln = local(&name);
                if ln == "span" && span_depth > 0 {
                    out.push_str("</span>");
                    span_depth -= 1;
                } else if ln == block {
                    break;
                }
            }
        }
    }
    // Defensive: close any spans left open.
    for _ in 0..span_depth {
        out.push_str("</span>");
    }
    out
}

/// Emit one `table:table` (open already consumed) as an HTML `<table>`.
fn odf_table(
    x: &mut Xml,
    zip: &BTreeMap<String, Vec<u8>>,
    styles: &BTreeMap<String, String>,
    out: &mut String,
) {
    out.push_str("<table>");
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => {
                let ln = local(&name);
                if ln == "table-row" && !sc {
                    out.push_str("<tr>");
                } else if ln == "table-cell" && !sc {
                    let repeat = attr(&attrs, "number-columns-repeated")
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(1)
                        .min(64);
                    let mut cell = String::new();
                    odf_walk(x, zip, styles, &mut cell, Some("table-cell"));
                    let cell = cell.trim().to_string();
                    for _ in 0..repeat {
                        out.push_str("<td>");
                        out.push_str(&cell);
                        out.push_str("</td>");
                    }
                } else if ln == "covered-table-cell" && sc {
                    out.push_str("<td></td>");
                }
            }
            Tok::Close(name) => {
                let ln = local(&name);
                if ln == "table-row" {
                    out.push_str("</tr>");
                } else if ln == "table" {
                    break;
                }
            }
            Tok::Text(_) => {}
        }
    }
    out.push_str("</table>");
}

/// ODS → one HTML `<table>` per `table:table`, honoring
/// `table:number-columns-repeated` (capped ~64). Rendered landscape.
pub fn ods_to_pdf(zip: &BTreeMap<String, Vec<u8>>) -> Vec<u8> {
    let content = part(zip, "content.xml");
    let mut body = String::new();
    let mut x = Xml::new(&content);
    let mut first = true;
    while let Some(tok) = x.next() {
        if let Tok::Open(name, attrs, sc) = &tok {
            if local(name) == "table" && !sc {
                if !first {
                    body.push_str("<div style=\"page-break-before:always\"></div>");
                }
                first = false;
                if let Some(nm) = attr(attrs, "name") {
                    body.push_str(&format!("<h2>{}</h2>", escaped(nm)));
                }
                ods_table(&mut x, &mut body);
            }
        }
    }
    if first {
        body.push_str("<p></p>");
    }
    crate::html::render(&html_doc(&body), &[], LAND_W, LAND_H, MARGIN)
}

/// Emit one ODS `table:table` (open consumed) as an HTML `<table>`, expanding
/// repeated rows/columns (cap 64) and reading cell text from `text:p` runs.
fn ods_table(x: &mut Xml, out: &mut String) {
    out.push_str("<table>");
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => {
                let ln = local(&name);
                if ln == "table-row" && !sc {
                    let rep = attr(&attrs, "number-rows-repeated")
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(1)
                        .min(64);
                    let row = ods_row(x);
                    // Skip emitting many identical *empty* trailing rows.
                    let emit = if row.trim().is_empty() { rep.min(1) } else { rep };
                    for _ in 0..emit {
                        out.push_str(&format!("<tr>{row}</tr>"));
                    }
                } else if ln == "table" && sc {
                    // nested? ignore
                }
            }
            Tok::Close(name) => {
                if local(&name) == "table" {
                    break;
                }
            }
            Tok::Text(_) => {}
        }
    }
    out.push_str("</table>");
}

/// Collect the `<td>` cells of one `table:table-row` (open already consumed)
/// until `</table:table-row>`.
fn ods_row(x: &mut Xml) -> String {
    let mut out = String::new();
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => {
                let ln = local(&name);
                if (ln == "table-cell" || ln == "covered-table-cell") && !sc {
                    let rep = attr(&attrs, "number-columns-repeated")
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(1)
                        .min(64);
                    let text = ods_cell_text(x, ln);
                    let emit = if text.trim().is_empty() { rep.min(1) } else { rep };
                    for _ in 0..emit {
                        out.push_str(&format!("<td>{}</td>", text.trim()));
                    }
                } else if (ln == "table-cell" || ln == "covered-table-cell") && sc {
                    out.push_str("<td></td>");
                }
            }
            Tok::Close(name) => {
                if local(&name) == "table-row" {
                    break;
                }
            }
            Tok::Text(_) => {}
        }
    }
    out
}

/// Read the joined text of one ODS cell (open consumed) until `</…cell>`.
fn ods_cell_text(x: &mut Xml, cell_tag: &str) -> String {
    let mut out = String::new();
    let mut depth = 0i32; // <text:p> nesting
    let mut started = false;
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, _, sc) => {
                if local(&name) == "p" && !sc {
                    if started {
                        out.push(' ');
                    }
                    started = true;
                    depth += 1;
                }
            }
            Tok::Text(t) => {
                if depth > 0 {
                    esc(&t, &mut out);
                }
            }
            Tok::Close(name) => {
                let ln = local(&name);
                if ln == "p" {
                    depth = (depth - 1).max(0);
                } else if ln == cell_tag {
                    break;
                }
            }
        }
    }
    out
}

/// ODP → one page per `draw:page`; text from `text:p` (with `text:span`
/// styles), images via `draw:image`. Rendered landscape.
pub fn odp_to_pdf(zip: &BTreeMap<String, Vec<u8>>) -> Vec<u8> {
    let content = part(zip, "content.xml");
    let styles = odf_text_styles(&content);
    let mut body = String::new();
    let mut x = Xml::new(&content);
    let mut first = true;
    while let Some(tok) = x.next() {
        if let Tok::Open(name, _, sc) = &tok {
            if local(name) == "page" && !sc {
                if !first {
                    body.push_str("<div style=\"page-break-before:always\"></div>");
                }
                first = false;
                odp_page(&mut x, zip, &styles, &mut body);
            }
        }
    }
    if first {
        body.push_str("<p></p>");
    }
    crate::html::render(&html_doc(&body), &[], LAND_W, LAND_H, MARGIN)
}

/// Emit one `draw:page` (open consumed) — its paragraphs and images — until
/// `</draw:page>`.
fn odp_page(
    x: &mut Xml,
    zip: &BTreeMap<String, Vec<u8>>,
    styles: &BTreeMap<String, String>,
    out: &mut String,
) {
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => {
                let ln = local(&name);
                match ln {
                    "p" if !sc => {
                        let inner = odf_inline(x, zip, styles, "p");
                        if !inner.trim().is_empty() {
                            out.push_str(&format!("<p>{}</p>", inner.trim()));
                        }
                    }
                    "image" if sc => {
                        if let Some(href) = attr(&attrs, "href") {
                            let key = href.trim_start_matches('/').to_string();
                            if let Some(tag) = img_tag(zip, &key) {
                                out.push_str(&tag);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Tok::Close(name) => {
                if local(&name) == "page" {
                    break;
                }
            }
            Tok::Text(_) => {}
        }
    }
}

// ════════════════════════════════ legacy OLE2 ═════════════════════════════════

/// Legacy `.doc/.xls/.ppt` (OLE2 Compound File) → best-effort **text-only** PDF.
///
/// We parse the Compound File container (header → FAT → directory), locate the
/// document's main stream (`WordDocument` / `Workbook`/`Book` / `PowerPoint
/// Document`), and extract readable runs (UTF-16LE and ASCII), emitting `<p>`
/// paragraphs. There is no formatting recovery — the binary record formats are
/// out of scope for a zero-dependency engine. Returns `None` if nothing legible
/// is found.
fn ole2_to_pdf(bytes: &[u8]) -> Option<Vec<u8>> {
    let cfb = Cfb::parse(bytes)?;
    // Preferred main streams, in order.
    let candidates = [
        "WordDocument",
        "Workbook",
        "Book",
        "PowerPoint Document",
        "Contents",
    ];
    let mut stream: Option<Vec<u8>> = None;
    for name in candidates {
        if let Some(s) = cfb.stream(name) {
            stream = Some(s);
            break;
        }
    }
    // Fall back to the largest stream if no known name matched.
    let data = stream.or_else(|| cfb.largest_stream())?;
    let paras = extract_runs(&data);
    if paras.is_empty() {
        return None;
    }
    let mut body = String::new();
    for p in paras {
        body.push_str("<p>");
        esc(&p, &mut body);
        body.push_str("</p>");
    }
    Some(crate::html::render(&html_doc(&body), &[], PAGE_W, PAGE_H, MARGIN))
}

/// Extract readable text runs from a raw binary stream: prefer UTF-16LE runs of
/// printable BMP characters, and ASCII runs; split into paragraphs on long gaps
/// of non-text bytes. Heuristic, good-enough for legacy `.doc/.ppt`.
fn extract_runs(data: &[u8]) -> Vec<String> {
    let mut paras: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut gap = 0usize;

    // 1) UTF-16LE scan (Word/PowerPoint store Unicode text).
    let mut i = 0;
    while i + 1 < data.len() {
        let lo = data[i];
        let hi = data[i + 1];
        let code = u16::from_le_bytes([lo, hi]);
        if let Some(c) = printable_bmp(code) {
            cur.push(c);
            gap = 0;
        } else {
            gap += 1;
            if gap == 1 && !cur.is_empty() {
                // single break inside text → space
                if !cur.ends_with(' ') {
                    cur.push(' ');
                }
            }
            if gap > 6 {
                flush_run(&mut cur, &mut paras);
            }
        }
        i += 2;
    }
    flush_run(&mut cur, &mut paras);

    // 2) If UTF-16 yielded little, try an ASCII scan as a fallback.
    if paras.iter().map(|p| p.len()).sum::<usize>() < 16 {
        paras.clear();
        cur.clear();
        gap = 0;
        for &b in data {
            if (0x20..=0x7E).contains(&b) {
                cur.push(b as char);
                gap = 0;
            } else if b == b'\r' || b == b'\n' {
                flush_run(&mut cur, &mut paras);
                gap = 0;
            } else {
                gap += 1;
                if gap > 4 {
                    flush_run(&mut cur, &mut paras);
                }
            }
        }
        flush_run(&mut cur, &mut paras);
    }

    paras
}

/// True printable Basic-Multilingual-Plane char (letters/digits/punct/space),
/// excluding controls and the surrogate/PUA ranges that dominate binary noise.
fn printable_bmp(code: u16) -> Option<char> {
    match code {
        0x09 | 0x0A | 0x0D | 0x20 => Some(' '),
        0x21..=0x7E => char::from_u32(code as u32),
        0x00A0..=0x024F => char::from_u32(code as u32), // Latin-1/Extended
        0x2018..=0x201F => char::from_u32(code as u32), // smart quotes/dashes
        0x2022 => Some('•'),
        0x2026 => Some('…'),
        _ => None,
    }
}

/// Normalize and push a finished run as a paragraph if it has real words.
fn flush_run(cur: &mut String, paras: &mut Vec<String>) {
    let collapsed: String = cur.split_whitespace().collect::<Vec<_>>().join(" ");
    cur.clear();
    // Keep runs with at least one 2+ letter "word" — drops binary noise.
    let has_word = collapsed
        .split(' ')
        .any(|w| w.chars().filter(|c| c.is_alphabetic()).count() >= 2);
    if has_word && collapsed.len() >= 3 {
        paras.push(collapsed);
    }
}

/// A minimal read-only OLE2 / Compound File Binary container parser: header →
/// FAT → directory → stream reassembly (FAT + MiniFAT). Enough to pull the main
/// document stream's bytes; not a general CFB library.
struct Cfb {
    /// Directory entries: `(name, start_sector, size, is_stream)`.
    dirents: Vec<(String, u32, u64, bool)>,
    sector_size: usize,
    mini_sector_size: usize,
    fat: Vec<u32>,
    minifat: Vec<u32>,
    data: Vec<u8>,
    /// Root entry's stream (the MiniStream container).
    mini_stream: Vec<u8>,
    mini_cutoff: u64,
}

const FREESECT: u32 = 0xFFFF_FFFF;
const ENDOFCHAIN: u32 = 0xFFFF_FFFE;

impl Cfb {
    fn parse(bytes: &[u8]) -> Option<Cfb> {
        if bytes.len() < 512 || bytes[..8] != [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1] {
            return None;
        }
        let u16le = |o: usize| u16::from_le_bytes([bytes[o], bytes[o + 1]]);
        let u32le =
            |o: usize| u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);

        let sector_shift = u16le(30);
        let mini_shift = u16le(32);
        let sector_size = 1usize << sector_shift;
        let mini_sector_size = 1usize << mini_shift;
        if sector_size == 0 || !(7..=20).contains(&sector_shift) {
            return None;
        }
        let num_fat_sectors = u32le(44) as usize;
        let dir_start = u32le(48);
        let mini_cutoff = u32le(56) as u64;
        let minifat_start = u32le(60);
        let num_minifat = u32le(64) as usize;
        let difat_start = u32le(68);
        let num_difat = u32le(72) as usize;

        // DIFAT: first 109 entries in the header, then chained DIFAT sectors.
        let mut fat_sectors: Vec<u32> = Vec::new();
        for k in 0..109 {
            let s = u32le(76 + k * 4);
            if s == FREESECT || s == ENDOFCHAIN {
                continue;
            }
            fat_sectors.push(s);
        }
        // Follow extra DIFAT sectors if present.
        let mut difat_sec = difat_start;
        let mut guard = 0;
        while difat_sec != ENDOFCHAIN
            && difat_sec != FREESECT
            && guard < num_difat + 8
            && (difat_sec as usize) < usize::MAX
        {
            let base = (difat_sec as usize + 1) * sector_size;
            if base + sector_size > bytes.len() {
                break;
            }
            let per = sector_size / 4 - 1;
            for k in 0..per {
                let s = u32::from_le_bytes([
                    bytes[base + k * 4],
                    bytes[base + k * 4 + 1],
                    bytes[base + k * 4 + 2],
                    bytes[base + k * 4 + 3],
                ]);
                if s != FREESECT && s != ENDOFCHAIN {
                    fat_sectors.push(s);
                }
            }
            difat_sec = u32::from_le_bytes([
                bytes[base + per * 4],
                bytes[base + per * 4 + 1],
                bytes[base + per * 4 + 2],
                bytes[base + per * 4 + 3],
            ]);
            guard += 1;
        }
        let _ = num_fat_sectors;

        // Build the FAT (one u32 per sector).
        let mut fat: Vec<u32> = Vec::new();
        for &fs in &fat_sectors {
            let base = (fs as usize + 1) * sector_size;
            if base + sector_size > bytes.len() {
                continue;
            }
            for k in 0..(sector_size / 4) {
                fat.push(u32::from_le_bytes([
                    bytes[base + k * 4],
                    bytes[base + k * 4 + 1],
                    bytes[base + k * 4 + 2],
                    bytes[base + k * 4 + 3],
                ]));
            }
        }

        let data = bytes.to_vec();

        // Helper to read a FAT chain into bytes.
        let read_chain = |fat: &[u32], start: u32| -> Vec<u8> {
            let mut out = Vec::new();
            let mut sec = start;
            let mut steps = 0;
            while sec != ENDOFCHAIN && sec != FREESECT && (sec as usize) < fat.len().max(1) << 8 {
                let base = (sec as usize + 1) * sector_size;
                if base + sector_size > data.len() {
                    break;
                }
                out.extend_from_slice(&data[base..base + sector_size]);
                let next = *fat.get(sec as usize).unwrap_or(&ENDOFCHAIN);
                if next == sec {
                    break; // cycle guard
                }
                sec = next;
                steps += 1;
                if steps > fat.len() + 16 {
                    break;
                }
            }
            out
        };

        // Directory chain.
        let dir_bytes = read_chain(&fat, dir_start);
        let mut dirents = Vec::new();
        let mut root_start = 0u32;
        let mut root_size = 0u64;
        let ent_size = 128;
        let mut off = 0;
        while off + ent_size <= dir_bytes.len() {
            let name_len = u16::from_le_bytes([dir_bytes[off + 64], dir_bytes[off + 65]]) as usize;
            let obj_type = dir_bytes[off + 66];
            // name: up to 32 UTF-16LE code units, name_len includes NUL.
            let mut name = String::new();
            if name_len >= 2 {
                let chars = (name_len / 2).saturating_sub(1).min(32);
                for k in 0..chars {
                    let c = u16::from_le_bytes([
                        dir_bytes[off + k * 2],
                        dir_bytes[off + k * 2 + 1],
                    ]);
                    if let Some(ch) = char::from_u32(c as u32) {
                        name.push(ch);
                    }
                }
            }
            let start = u32::from_le_bytes([
                dir_bytes[off + 116],
                dir_bytes[off + 117],
                dir_bytes[off + 118],
                dir_bytes[off + 119],
            ]);
            let size = u64::from_le_bytes([
                dir_bytes[off + 120],
                dir_bytes[off + 121],
                dir_bytes[off + 122],
                dir_bytes[off + 123],
                dir_bytes[off + 124],
                dir_bytes[off + 125],
                dir_bytes[off + 126],
                dir_bytes[off + 127],
            ]);
            match obj_type {
                5 => {
                    // Root storage → MiniStream container.
                    root_start = start;
                    root_size = size;
                }
                2 => dirents.push((name, start, size, true)), // stream
                1 => dirents.push((name, start, size, false)), // storage
                _ => {}
            }
            off += ent_size;
        }

        // MiniFAT chain → mini allocation table.
        let mut minifat: Vec<u32> = Vec::new();
        if num_minifat > 0 && minifat_start != ENDOFCHAIN && minifat_start != FREESECT {
            let mf = read_chain(&fat, minifat_start);
            for k in 0..(mf.len() / 4) {
                minifat.push(u32::from_le_bytes([
                    mf[k * 4],
                    mf[k * 4 + 1],
                    mf[k * 4 + 2],
                    mf[k * 4 + 3],
                ]));
            }
        }

        // MiniStream = root storage's regular-FAT chain.
        let mut mini_stream = read_chain(&fat, root_start);
        mini_stream.truncate(root_size as usize);

        Some(Cfb {
            dirents,
            sector_size,
            mini_sector_size,
            fat,
            minifat,
            data,
            mini_stream,
            mini_cutoff,
        })
    }

    /// Read the named stream's bytes (FAT for large streams, MiniFAT for small).
    fn stream(&self, name: &str) -> Option<Vec<u8>> {
        let (_, start, size, _) = self
            .dirents
            .iter()
            .find(|(n, _, _, is_stream)| *is_stream && n == name)?;
        Some(self.read_stream(*start, *size))
    }

    /// The largest stream by declared size (fallback when no name matches).
    fn largest_stream(&self) -> Option<Vec<u8>> {
        let (_, start, size, _) = self
            .dirents
            .iter()
            .filter(|(_, _, _, is_stream)| *is_stream)
            .max_by_key(|(_, _, size, _)| *size)?;
        Some(self.read_stream(*start, *size))
    }

    fn read_stream(&self, start: u32, size: u64) -> Vec<u8> {
        if size < self.mini_cutoff {
            self.read_mini_chain(start, size)
        } else {
            let mut out = self.read_fat_chain(start);
            out.truncate(size as usize);
            out
        }
    }

    fn read_fat_chain(&self, start: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let mut sec = start;
        let mut steps = 0;
        while sec != ENDOFCHAIN && sec != FREESECT {
            let base = (sec as usize + 1) * self.sector_size;
            if base + self.sector_size > self.data.len() {
                break;
            }
            out.extend_from_slice(&self.data[base..base + self.sector_size]);
            let next = *self.fat.get(sec as usize).unwrap_or(&ENDOFCHAIN);
            if next == sec {
                break;
            }
            sec = next;
            steps += 1;
            if steps > self.fat.len() + 16 {
                break;
            }
        }
        out
    }

    fn read_mini_chain(&self, start: u32, size: u64) -> Vec<u8> {
        let mut out = Vec::new();
        let mut sec = start;
        let mut steps = 0;
        while sec != ENDOFCHAIN && sec != FREESECT {
            let base = sec as usize * self.mini_sector_size;
            if base + self.mini_sector_size > self.mini_stream.len() {
                break;
            }
            out.extend_from_slice(&self.mini_stream[base..base + self.mini_sector_size]);
            let next = *self.minifat.get(sec as usize).unwrap_or(&ENDOFCHAIN);
            if next == sec {
                break;
            }
            sec = next;
            steps += 1;
            if steps > self.minifat.len() + 16 {
                break;
            }
        }
        out.truncate(size as usize);
        out
    }
}

// ─────────────────────────────────── helpers ──────────────────────────────────

/// Fetch a zip part as a UTF-8 (lossy) string, or empty if absent.
fn part(zip: &BTreeMap<String, Vec<u8>>, key: &str) -> String {
    zip.get(key)
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default()
}

/// True for an exactly-6-char hex colour (`RRGGBB`).
fn is_hex6(s: &str) -> bool {
    s.len() == 6 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::zip::ZipWriter;

    fn opens(pdf: &[u8]) -> crate::Document {
        crate::Document::open(pdf).expect("valid PDF")
    }

    /// `Document::to_text` reconstructs the page line-by-line, so words land on
    /// separate lines; collapse all whitespace to single spaces so multi-word
    /// phrase assertions match the rendered content regardless of line breaks.
    fn norm(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// A tiny valid PNG (4×4 red) for image-embedding fixtures.
    fn red_png() -> Vec<u8> {
        let rgba = [255u8, 0, 0, 255].repeat(16);
        crate::raster::png::encode_png(4, 4, &rgba)
    }

    // ── streaming XML walker ──

    #[test]
    fn xml_walker_tokens_and_attrs() {
        let mut x = Xml::new(r#"<w:p><w:r a="1" b='two'>Hi &amp; bye</w:r></w:p>"#);
        assert_eq!(
            x.next(),
            Some(Tok::Open("w:p".into(), vec![], false))
        );
        let open = x.next().unwrap();
        match open {
            Tok::Open(n, attrs, sc) => {
                assert_eq!(n, "w:r");
                assert_eq!(attr(&attrs, "a"), Some("1"));
                assert_eq!(attr(&attrs, "b"), Some("two"));
                assert!(!sc);
            }
            _ => panic!("expected open"),
        }
        assert_eq!(x.next(), Some(Tok::Text("Hi & bye".into())));
        assert_eq!(x.next(), Some(Tok::Close("w:r".into())));
        assert_eq!(x.next(), Some(Tok::Close("w:p".into())));
        assert_eq!(x.next(), None);
    }

    #[test]
    fn self_closing_and_local_name() {
        let mut x = Xml::new(r#"<a:blip r:embed="rId7"/>"#);
        match x.next().unwrap() {
            Tok::Open(n, attrs, sc) => {
                assert_eq!(local(&n), "blip");
                assert!(sc);
                assert_eq!(attr(&attrs, "embed"), Some("rId7"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn col_ref_indices() {
        assert_eq!(col_of_ref("A1"), 0);
        assert_eq!(col_of_ref("B2"), 1);
        assert_eq!(col_of_ref("Z9"), 25);
        assert_eq!(col_of_ref("AA1"), 26);
        assert_eq!(col_of_ref("AB10"), 27);
    }

    // ── DOCX ──

    fn build_docx(document_xml: &str, rels_xml: Option<&str>, media: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut z = ZipWriter::new();
        z.add_stored("[Content_Types].xml", b"<Types/>");
        z.add_stored("word/document.xml", document_xml.as_bytes());
        if let Some(r) = rels_xml {
            z.add_stored("word/_rels/document.xml.rels", r.as_bytes());
        }
        for (name, bytes) in media {
            z.add_stored(name, bytes);
        }
        z.finish()
    }

    #[test]
    fn docx_headings_bold_and_table() {
        let doc = r#"<?xml version="1.0"?>
<w:document xmlns:w="x">
  <w:body>
    <w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Big Title</w:t></w:r></w:p>
    <w:p><w:r><w:rPr><w:b/></w:rPr><w:t>BoldWord</w:t></w:r><w:r><w:t> normal</w:t></w:r></w:p>
    <w:tbl>
      <w:tr><w:tc><w:p><w:r><w:t>R1C1</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>R1C2</w:t></w:r></w:p></w:tc></w:tr>
      <w:tr><w:tc><w:p><w:r><w:t>R2C1</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>R2C2</w:t></w:r></w:p></w:tc></w:tr>
    </w:tbl>
  </w:body>
</w:document>"#;
        let bytes = build_docx(doc, None, &[]);
        let pdf = office_to_pdf(&bytes).expect("docx converts");
        let document = opens(&pdf);
        assert!(document.page_count() >= 1);
        let text = norm(&document.to_text());
        for needle in ["Big Title", "BoldWord", "normal", "R1C1", "R2C2"] {
            assert!(text.contains(needle), "missing {needle:?} in: {text}");
        }
    }

    #[test]
    fn docx_inline_image_from_rels() {
        let doc = r#"<w:document xmlns:w="x" xmlns:a="y" xmlns:r="z">
  <w:body>
    <w:p><w:r><w:t>Before image</w:t></w:r></w:p>
    <w:p><w:r><w:drawing><a:blip r:embed="rId5"/></w:drawing></w:r></w:p>
  </w:body>
</w:document>"#;
        let rels = r#"<Relationships xmlns="x">
  <Relationship Id="rId5" Type="image" Target="media/pic.png"/>
</Relationships>"#;
        let bytes = build_docx(doc, Some(rels), &[("word/media/pic.png", red_png())]);
        let pdf = office_to_pdf(&bytes).expect("docx converts");
        let document = opens(&pdf);
        assert!(document.page_count() >= 1);
        assert!(norm(&document.to_text()).contains("Before image"));
    }

    // ── XLSX (round-trip via the exporter) ──

    #[test]
    fn xlsx_inline_strings_render_as_table() {
        // Build with the engine's own exporter (uses t="inlineStr").
        let grid = vec![
            vec!["Name".to_string(), "Score".to_string()],
            vec!["Alice".to_string(), "42".to_string()],
            vec!["Bob".to_string(), "7".to_string()],
        ];
        let xlsx = crate::convert::office::to_xlsx_named(
            std::slice::from_ref(&grid),
            &["People".to_string()],
        );
        let pdf = office_to_pdf(&xlsx).expect("xlsx converts");
        let document = opens(&pdf);
        assert!(document.page_count() >= 1);
        let text = norm(&document.to_text());
        for needle in ["People", "Name", "Score", "Alice", "42", "Bob"] {
            assert!(text.contains(needle), "missing {needle:?} in: {text}");
        }
    }

    #[test]
    fn xlsx_shared_strings_resolve() {
        // Hand-built XLSX exercising t="s" shared strings + a numeric cell.
        let mut z = ZipWriter::new();
        z.add_stored("[Content_Types].xml", b"<Types/>");
        z.add_stored(
            "xl/workbook.xml",
            br#"<workbook><sheets><sheet name="Data" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
        );
        z.add_stored(
            "xl/sharedStrings.xml",
            br#"<sst><si><t>Hello</t></si><si><t>World</t></si></sst>"#,
        );
        z.add_stored(
            "xl/worksheets/sheet1.xml",
            br#"<worksheet><sheetData>
              <row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c></row>
              <row r="2"><c r="A2"><v>99</v></c></row>
            </sheetData></worksheet>"#,
        );
        let xlsx = z.finish();
        let pdf = office_to_pdf(&xlsx).expect("xlsx converts");
        let text = norm(&opens(&pdf).to_text());
        for needle in ["Data", "Hello", "World", "99"] {
            assert!(text.contains(needle), "missing {needle:?} in: {text}");
        }
    }

    // ── PPTX ──

    #[test]
    fn pptx_one_page_per_slide() {
        let mut z = ZipWriter::new();
        z.add_stored("[Content_Types].xml", b"<Types/>");
        z.add_stored("ppt/presentation.xml", b"<p:presentation/>");
        z.add_stored(
            "ppt/slides/slide1.xml",
            br#"<p:sld xmlns:a="y"><a:p><a:r><a:t>Slide One Title</a:t></a:r></a:p></p:sld>"#,
        );
        z.add_stored(
            "ppt/slides/slide2.xml",
            br#"<p:sld xmlns:a="y"><a:p><a:r><a:t>Second Slide Body</a:t></a:r></a:p></p:sld>"#,
        );
        let pptx = z.finish();
        let pdf = office_to_pdf(&pptx).expect("pptx converts");
        let document = opens(&pdf);
        assert!(document.page_count() >= 2, "one page per slide");
        let text = norm(&document.to_text());
        assert!(text.contains("Slide One Title"));
        assert!(text.contains("Second Slide Body"));
    }

    // ── ODT ──

    #[test]
    fn odt_headings_spans_and_table() {
        let content = r#"<office:document-content xmlns:office="o" xmlns:text="t" xmlns:table="tb" xmlns:style="s" xmlns:fo="f">
  <office:automatic-styles>
    <style:style style:name="T1"><style:text-properties fo:font-weight="bold"/></style:style>
  </office:automatic-styles>
  <office:body><office:text>
    <text:h text:outline-level="1">Doc Heading</text:h>
    <text:p>plain <text:span text:style-name="T1">boldspan</text:span> end</text:p>
    <table:table table:name="T">
      <table:table-row><table:table-cell><text:p>CellA</text:p></table:table-cell><table:table-cell><text:p>CellB</text:p></table:table-cell></table:table-row>
    </table:table>
  </office:text></office:body>
</office:document-content>"#;
        let mut z = ZipWriter::new();
        z.add_stored("mimetype", b"application/vnd.oasis.opendocument.text");
        z.add_stored("content.xml", content.as_bytes());
        let odt = z.finish();
        let pdf = office_to_pdf(&odt).expect("odt converts");
        let text = norm(&opens(&pdf).to_text());
        for needle in ["Doc Heading", "plain", "boldspan", "end", "CellA", "CellB"] {
            assert!(text.contains(needle), "missing {needle:?} in: {text}");
        }
    }

    // ── ODS ──

    #[test]
    fn ods_table_with_repeated_columns() {
        let content = r#"<office:document-content xmlns:office="o" xmlns:table="tb" xmlns:text="t">
  <office:body><office:spreadsheet>
    <table:table table:name="Sheet1">
      <table:table-row>
        <table:table-cell><text:p>X</text:p></table:table-cell>
        <table:table-cell table:number-columns-repeated="2"><text:p>Y</text:p></table:table-cell>
      </table:table-row>
      <table:table-row>
        <table:table-cell><text:p>1</text:p></table:table-cell>
        <table:table-cell><text:p>2</text:p></table:table-cell>
      </table:table-row>
    </table:table>
  </office:spreadsheet></office:body>
</office:document-content>"#;
        let mut z = ZipWriter::new();
        z.add_stored("mimetype", b"application/vnd.oasis.opendocument.spreadsheet");
        z.add_stored("content.xml", content.as_bytes());
        let ods = z.finish();
        let pdf = office_to_pdf(&ods).expect("ods converts");
        let text = norm(&opens(&pdf).to_text());
        for needle in ["Sheet1", "X", "Y", "1", "2"] {
            assert!(text.contains(needle), "missing {needle:?} in: {text}");
        }
    }

    // ── ODP ──

    #[test]
    fn odp_one_page_per_draw_page() {
        let content = r#"<office:document-content xmlns:office="o" xmlns:draw="d" xmlns:text="t">
  <office:body><office:presentation>
    <draw:page draw:name="p1"><draw:frame><draw:text-box><text:p>First Deck Page</text:p></draw:text-box></draw:frame></draw:page>
    <draw:page draw:name="p2"><draw:frame><draw:text-box><text:p>Page Two Content</text:p></draw:text-box></draw:frame></draw:page>
  </office:presentation></office:body>
</office:document-content>"#;
        let mut z = ZipWriter::new();
        z.add_stored("mimetype", b"application/vnd.oasis.opendocument.presentation");
        z.add_stored("content.xml", content.as_bytes());
        let odp = z.finish();
        let pdf = office_to_pdf(&odp).expect("odp converts");
        let document = opens(&pdf);
        assert!(document.page_count() >= 2, "one page per draw:page");
        let text = norm(&document.to_text());
        assert!(text.contains("First Deck Page"));
        assert!(text.contains("Page Two Content"));
    }

    // ── dispatch / robustness ──

    #[test]
    fn non_office_bytes_return_none() {
        assert!(office_to_pdf(b"not an office file at all").is_none());
        assert!(office_to_pdf(&[0u8; 4]).is_none());
    }

    #[test]
    fn ole2_magic_without_payload_is_none() {
        // Magic header but not a valid CFB → graceful None, no panic.
        let mut bytes = vec![0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
        bytes.extend(std::iter::repeat_n(0u8, 600));
        assert!(office_to_pdf(&bytes).is_none());
    }

    /// Build a minimal valid Compound File Binary with one `WordDocument`
    /// stream (stored via the regular FAT — size ≥ mini cutoff). 512-byte
    /// sectors. Layout: sector 0 = directory, sector 1 = FAT, sectors 2.. =
    /// the WordDocument data.
    fn build_cfb_word(text_utf16: &[u8]) -> Vec<u8> {
        const SEC: usize = 512;
        const FREE: u32 = 0xFFFF_FFFF;
        const EOC: u32 = 0xFFFF_FFFE;
        const FATSECT: u32 = 0xFFFF_FFFD;

        let data_secs = text_utf16.len().div_ceil(SEC).max(1);
        let total_secs = 2 + data_secs; // dir + fat + data

        let mut out = vec![0u8; SEC * (1 + total_secs)]; // header sector + sectors

        // ── Header ──
        out[..8].copy_from_slice(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]);
        let put16 = |o: &mut [u8], at: usize, v: u16| o[at..at + 2].copy_from_slice(&v.to_le_bytes());
        let put32 = |o: &mut [u8], at: usize, v: u32| o[at..at + 4].copy_from_slice(&v.to_le_bytes());
        put16(&mut out, 24, 0x003E); // minor version
        put16(&mut out, 26, 0x0003); // major version 3 (512-byte sectors)
        put16(&mut out, 28, 0xFFFE); // byte order
        put16(&mut out, 30, 9); // sector shift → 512
        put16(&mut out, 32, 6); // mini sector shift → 64
        put32(&mut out, 44, 1); // number of FAT sectors
        put32(&mut out, 48, 0); // directory start sector = 0
        put32(&mut out, 56, 4096); // mini stream cutoff
        put32(&mut out, 60, EOC); // MiniFAT start
        put32(&mut out, 64, 0); // number of MiniFAT sectors
        put32(&mut out, 68, EOC); // DIFAT start
        put32(&mut out, 72, 0); // number of DIFAT sectors
        // DIFAT[0] = sector 1 (the FAT); rest free.
        put32(&mut out, 76, 1);
        for k in 1..109 {
            put32(&mut out, 76 + k * 4, FREE);
        }

        // Logical sector `s` lives at byte offset `SEC*(1+s)` — the file leads
        // with the 512-byte header "sector".
        let sector_off = |s: usize| SEC + SEC * s;

        // ── FAT (logical sector 1) ──
        let fat_base = sector_off(1);
        // Default everything free.
        for k in 0..(SEC / 4) {
            put32(&mut out, fat_base + k * 4, FREE);
        }
        put32(&mut out, fat_base, EOC); // sector 0 (dir) = end
        put32(&mut out, fat_base + 4, FATSECT); // sector 1 = FAT self
        // Data sectors 2..(2+data_secs) chained.
        for d in 0..data_secs {
            let sec = 2 + d;
            let next = if d + 1 < data_secs {
                (sec + 1) as u32
            } else {
                EOC
            };
            put32(&mut out, fat_base + sec * 4, next);
        }

        // ── Directory (logical sector 0) ──
        let dir_base = sector_off(0);
        // Entry 0: Root Entry (object type 5).
        let put_name = |o: &mut [u8], base: usize, name: &str| {
            let utf16: Vec<u16> = name.encode_utf16().collect();
            for (k, u) in utf16.iter().enumerate() {
                o[base + k * 2..base + k * 2 + 2].copy_from_slice(&u.to_le_bytes());
            }
            put16(o, base + 64, ((utf16.len() + 1) * 2) as u16); // name length incl NUL
        };
        put_name(&mut out, dir_base, "Root Entry");
        out[dir_base + 66] = 5; // object type: root storage
        put32(&mut out, dir_base + 116, EOC); // root has no mini stream here
        // size 0 → mini stream empty (our WordDocument uses the FAT chain).

        // Entry 1: WordDocument stream (object type 2), starts at sector 2.
        let e1 = dir_base + 128;
        put_name(&mut out, e1, "WordDocument");
        out[e1 + 66] = 2; // object type: stream
        put32(&mut out, e1 + 116, 2); // start sector
        // size (8 bytes at +120): the text length, ≥ cutoff guaranteed by caller.
        let size = text_utf16.len() as u64;
        out[e1 + 120..e1 + 128].copy_from_slice(&size.to_le_bytes());

        // ── Data (logical sector 2 onward) ──
        let data_base = sector_off(2);
        out[data_base..data_base + text_utf16.len()].copy_from_slice(text_utf16);

        out
    }

    #[test]
    fn ole2_word_stream_text_extracts() {
        // A WordDocument stream of UTF-16LE text, padded above the 4096 mini
        // cutoff so it is stored via the regular FAT chain.
        let phrase = "Legacy Word Document Body Text";
        let mut u16le: Vec<u8> = Vec::new();
        for ch in phrase.encode_utf16() {
            u16le.extend_from_slice(&ch.to_le_bytes());
        }
        // Pad with NULs to exceed the cutoff (forces FAT-chain storage).
        u16le.resize(5000, 0);

        let cfb = build_cfb_word(&u16le);
        let pdf = office_to_pdf(&cfb).expect("ole2 .doc extracts text");
        let document = opens(&pdf);
        assert!(document.page_count() >= 1);
        let text = norm(&document.to_text());
        for needle in ["Legacy", "Word", "Document", "Body", "Text"] {
            assert!(text.contains(needle), "missing {needle:?} in: {text}");
        }
    }
}
