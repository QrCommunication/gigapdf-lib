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
use crate::html::{Margins, RenderOptions};
use std::collections::BTreeMap;

// ─────────────────────────────── page geometry ────────────────────────────────
//
// The real page size and margins are read from each format's document part
// (DOCX `w:sectPr`, PPTX `p:sldSz`, ODF `style:page-layout-properties`). These
// constants are only the *fallback* used when the source declares no geometry.
//
// Documents (DOCX/ODT) fall back to A4 portrait; spreadsheets/slides
// (XLSX/PPTX/ODS/ODP) get more horizontal room (A4 landscape / 16:9). Margins
// default to a comfortable 72pt for prose and 36pt for tabular/slide layouts.

/// A4 portrait, points (`210mm × 297mm`). Prose fallback.
const A4_W: f64 = 595.276;
const A4_H: f64 = 841.890;
/// 16:9 slide, points (`10in × 5.625in` = PowerPoint default 960×540pt).
const SLIDE_W: f64 = 960.0;
const SLIDE_H: f64 = 540.0;
/// Default prose margin (1 inch) and tabular/slide margin (0.5 inch), points.
const PROSE_MARGIN: f64 = 72.0;
const TABULAR_MARGIN: f64 = 36.0;

/// DOCX/ODF length unit conversions to PDF points.
/// DOCX measurements are twentieths-of-a-point (twips): `pt = twips / 20`.
const TWIP_PER_PT: f64 = 20.0;
/// OOXML drawing measurements (PPTX slide size) are EMUs: `pt = emu / 12700`.
const EMU_PER_PT: f64 = 12700.0;

/// Resolved page size + margins for an Office document, in PDF points.
#[derive(Debug, Clone, Copy)]
struct PageGeom {
    w: f64,
    h: f64,
    margins: Margins,
}

impl PageGeom {
    /// Prose fallback: A4 portrait, 1in margins (DOCX/ODT with no `sectPr`).
    fn prose_default() -> Self {
        PageGeom {
            w: A4_W,
            h: A4_H,
            margins: Margins::uniform(PROSE_MARGIN),
        }
    }

    /// Tabular fallback: A4 landscape, 0.5in margins (XLSX/ODS).
    fn tabular_default() -> Self {
        PageGeom {
            w: A4_H,
            h: A4_W,
            margins: Margins::uniform(TABULAR_MARGIN),
        }
    }

    /// Slide fallback: 16:9, 0.5in margins (PPTX/ODP with no slide size).
    fn slide_default() -> Self {
        PageGeom {
            w: SLIDE_W,
            h: SLIDE_H,
            margins: Margins::uniform(TABULAR_MARGIN),
        }
    }

    /// Build the [`RenderOptions`] the HTML engine expects.
    fn render_options(&self) -> RenderOptions {
        let mut opts = RenderOptions::new(self.w, self.h);
        opts.margins = self.margins;
        opts
    }
}

/// Sanity-clamp a page dimension so a malformed source can't produce a
/// zero/negative or absurdly large page. Keeps the layout engine well-behaved.
fn clamp_page_dim(pt: f64) -> f64 {
    pt.clamp(72.0, 14400.0) // 1in … 200in
}

/// Render the generated `html` body through the HTML→PDF engine using the
/// resolved page geometry. Fonts are host-supplied via the engine's two-phase
/// contract ([`crate::html::needed_fonts`]); this in-engine path has no font
/// bytes of its own, so it passes an empty set — the real `font-family` names
/// injected into the HTML let the host resolve and embed the matching faces.
fn render_geom(body: &str, geom: PageGeom) -> Vec<u8> {
    crate::html::render_with(&html_doc(body), &[], &geom.render_options())
}

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
    let styles = parse_docx_styles(&part(zip, "word/styles.xml"));
    let numbering = parse_docx_numbering(&part(zip, "word/numbering.xml"));
    let footnotes = parse_docx_footnotes(&part(zip, "word/footnotes.xml"));

    let geom = docx_page_geom(&doc);
    let ctx = DocxCtx {
        zip,
        rels: &rels,
        styles: &styles,
        numbering: &numbering,
        footnotes: &footnotes,
    };

    let mut body = String::new();
    // Headers precede the main flow; footers follow it (single-flow render).
    docx_header_footer(zip, &ctx, "header", &mut body);
    docx_body(&doc, &ctx, &mut body);
    docx_footnotes_section(&ctx, &mut body);
    docx_header_footer(zip, &ctx, "footer", &mut body);
    render_geom(&body, geom)
}

/// Per-document DOCX context threaded through the body walker: media/relationship
/// access plus the resolved styles, numbering and footnotes tables.
struct DocxCtx<'a> {
    zip: &'a BTreeMap<String, Vec<u8>>,
    rels: &'a BTreeMap<String, String>,
    styles: &'a DocxStyles,
    numbering: &'a DocxNumbering,
    footnotes: &'a DocxFootnotes,
}

/// Render every `word/header*.xml` (or `footer*.xml`, per `kind`) as plain
/// paragraphs, wrapped so they read as header/footer matter around the main
/// flow. Parts are emitted in filename order for determinism.
fn docx_header_footer(
    zip: &BTreeMap<String, Vec<u8>>,
    ctx: &DocxCtx,
    kind: &str,
    out: &mut String,
) {
    let prefix = format!("word/{kind}");
    let mut parts: Vec<&String> = zip
        .keys()
        .filter(|k| k.starts_with(&prefix) && k.ends_with(".xml") && !k.contains("_rels"))
        .collect();
    parts.sort();
    for key in parts {
        let xml = String::from_utf8_lossy(&zip[key]);
        let mut frag = String::new();
        // Header/footer parts use the same w:p/w:r grammar as the body.
        docx_walk(&mut Xml::new(&xml), ctx, &mut frag, None);
        if !frag.trim().is_empty() {
            out.push_str(&frag);
        }
    }
}

/// Emit collected footnote bodies as a trailing block (a thin separator then one
/// `<p>` per footnote, numbered). No-op when the document has none.
fn docx_footnotes_section(ctx: &DocxCtx, out: &mut String) {
    let notes = &ctx.footnotes.ordered;
    if notes.is_empty() {
        return;
    }
    out.push_str("<hr>");
    for (i, html) in notes.iter().enumerate() {
        let n = i + 1;
        out.push_str(&format!("<p>{n}. {html}</p>"));
    }
}

/// Read the section's page size/margins from the body's `w:sectPr` —
/// `w:pgSz@w:w/@w:h` (+ `@w:orient`) and `w:pgMar@w:top/@w:right/@w:bottom/@w:left`,
/// all in twips (`pt = twips / 20`). Falls back to A4 portrait with 1in margins.
fn docx_page_geom(document_xml: &str) -> PageGeom {
    let mut geom = PageGeom::prose_default();
    let mut x = Xml::new(document_xml);
    let mut in_sect = false;
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => match local(&name) {
                "sectPr" => in_sect = true,
                "pgSz" if in_sect => {
                    let w = attr(&attrs, "w").and_then(twips_to_pt);
                    let h = attr(&attrs, "h").and_then(twips_to_pt);
                    if let (Some(w), Some(h)) = (w, h) {
                        // `w:orient="landscape"` reports w<h but means swapped.
                        let landscape = matches!(attr(&attrs, "orient"), Some("landscape"));
                        let (pw, ph) = if landscape && h > w { (h, w) } else { (w, h) };
                        geom.w = clamp_page_dim(pw);
                        geom.h = clamp_page_dim(ph);
                    }
                    let _ = sc; // pgSz is self-closing; keep scanning for pgMar.
                }
                "pgMar" if in_sect => {
                    let m = &mut geom.margins;
                    if let Some(v) = attr(&attrs, "top").and_then(twips_to_pt) {
                        m.top = v.max(0.0);
                    }
                    if let Some(v) = attr(&attrs, "right").and_then(twips_to_pt) {
                        m.right = v.max(0.0);
                    }
                    if let Some(v) = attr(&attrs, "bottom").and_then(twips_to_pt) {
                        m.bottom = v.max(0.0);
                    }
                    if let Some(v) = attr(&attrs, "left").and_then(twips_to_pt) {
                        m.left = v.max(0.0);
                    }
                }
                _ => {}
            },
            Tok::Close(name) => {
                if local(&name) == "sectPr" {
                    // First section's geometry is enough for a single-flow render.
                    break;
                }
            }
            Tok::Text(_) => {}
        }
    }
    geom
}

/// Twips (`1/20` pt) attribute string → points.
fn twips_to_pt(v: &str) -> Option<f64> {
    v.trim().parse::<f64>().ok().map(|t| t / TWIP_PER_PT)
}

/// Run/paragraph state while walking `w:document`.
#[derive(Default, Clone)]
struct RunStyle {
    bold: bool,
    italic: bool,
    underline: bool,
    size_half_pt: Option<f64>,
    color: Option<String>,
    /// Typeface name from `w:rFonts@ascii` (DOCX) / `a:latin@typeface` (PPTX) /
    /// `fo:font-name` (ODF). Surfaced as `font-family` so the host two-phase
    /// font fetch embeds the real face and the layout uses its true metrics.
    font_family: Option<String>,
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
        if let Some(fam) = &self.font_family {
            let family = css_font_family(fam);
            if !family.is_empty() {
                css.push_str(&format!("font-family:{family};"));
            }
        }
        if css.is_empty() {
            String::new()
        } else {
            format!("<span style=\"{css}\">")
        }
    }
}

/// Paragraph-level formatting from `w:pPr` mapped to inline block CSS:
/// `w:jc` → `text-align`, `w:spacing@before/@after` → `margin-top/-bottom`,
/// `w:spacing@line/@lineRule` → `line-height`, `w:ind@left/@right/@firstLine` →
/// `margin-left/-right`/`text-indent`, and `w:numPr@ilvl` → list `margin-left`
/// (the bullet is prepended to the text). All distances are twips
/// (`pt = twips / 20`).
#[derive(Default, Clone)]
struct ParaStyle {
    align: Option<&'static str>,
    space_before_pt: Option<f64>,
    space_after_pt: Option<f64>,
    indent_left_pt: Option<f64>,
    indent_right_pt: Option<f64>,
    first_line_pt: Option<f64>,
    /// Resolved `line-height`: either a unitless multiple (`w:lineRule="auto"`,
    /// `line/240`) or an absolute points value (`exact`/`atLeast`, `line/20`).
    line_height: Option<LineHeight>,
    /// List indent level from `w:numPr/w:ilvl` (each level adds 36pt of
    /// `margin-left`, on top of any explicit `w:ind`).
    list_level: Option<u32>,
}

/// A DOCX line-spacing value, mapped to the engine's `line-height`.
#[derive(Debug, Clone, Copy, PartialEq)]
enum LineHeight {
    /// Unitless multiple of the font size (`w:lineRule="auto"`).
    Multiple(f64),
    /// Absolute points (`w:lineRule="exact"` / `"atLeast"`).
    Points(f64),
}

/// Each DOCX list indent level (`w:ilvl`) maps to this much left margin.
const LIST_LEVEL_INDENT_PT: f64 = 36.0;

impl ParaStyle {
    /// Fill any paragraph property left unset inline from the resolved named
    /// style (`w:pStyle` + `w:docDefaults`): the direct `w:pPr` already collected
    /// wins, the style supplies the gaps. (Run-level style props are applied
    /// separately as the paragraph's outer span.)
    fn apply_style_defaults(&mut self, style: &DocxStyle) {
        self.align = self.align.or(style.align);
        self.space_before_pt = self.space_before_pt.or(style.space_before_pt);
        self.space_after_pt = self.space_after_pt.or(style.space_after_pt);
        self.indent_left_pt = self.indent_left_pt.or(style.indent_left_pt);
        self.indent_right_pt = self.indent_right_pt.or(style.indent_right_pt);
        self.first_line_pt = self.first_line_pt.or(style.first_line_pt);
        self.line_height = self.line_height.or(style.line_height);
    }

    /// A ` style="…"` attribute (with leading space) for the block element, or
    /// an empty string when no paragraph property was set. List levels add
    /// `LIST_LEVEL_INDENT_PT` per level to any explicit left indent.
    fn style_attr(&self) -> String {
        let mut css = String::new();
        if let Some(a) = self.align {
            css.push_str(&format!("text-align:{a};"));
        }
        if let Some(v) = self.space_before_pt {
            css.push_str(&format!("margin-top:{v}pt;"));
        }
        if let Some(v) = self.space_after_pt {
            css.push_str(&format!("margin-bottom:{v}pt;"));
        }
        // List level indent stacks on top of any explicit w:ind left margin.
        let list_indent = self
            .list_level
            .map(|lvl| (lvl as f64 + 1.0) * LIST_LEVEL_INDENT_PT);
        let left = match (self.indent_left_pt, list_indent) {
            (Some(a), Some(b)) => Some(a + b),
            (a, b) => a.or(b),
        };
        if let Some(v) = left {
            css.push_str(&format!("margin-left:{v}pt;"));
        }
        if let Some(v) = self.indent_right_pt {
            css.push_str(&format!("margin-right:{v}pt;"));
        }
        if let Some(v) = self.first_line_pt {
            css.push_str(&format!("text-indent:{v}pt;"));
        }
        match self.line_height {
            Some(LineHeight::Multiple(m)) => css.push_str(&format!("line-height:{m};")),
            Some(LineHeight::Points(p)) => css.push_str(&format!("line-height:{p}pt;")),
            None => {}
        }
        if css.is_empty() {
            String::new()
        } else {
            format!(" style=\"{css}\"")
        }
    }

    /// Like [`style_attr`](Self::style_attr) but also folds in the resolved named
    /// style's **run** defaults (bold/italic/colour/size/font-family) as block
    /// CSS, so the whole paragraph inherits the style's typography while each run
    /// can still override via its own inner `<span>`. The direct paragraph CSS
    /// from [`style_attr`](Self::style_attr) is kept verbatim.
    fn style_attr_with(&self, style: &DocxStyle) -> String {
        let base = self.style_attr();
        let mut run_css = String::new();
        if style.bold == Some(true) {
            run_css.push_str("font-weight:bold;");
        }
        if style.italic == Some(true) {
            run_css.push_str("font-style:italic;");
        }
        if style.underline == Some(true) {
            run_css.push_str("text-decoration:underline;");
        }
        if let Some(half) = style.size_half_pt {
            run_css.push_str(&format!("font-size:{}pt;", half / 2.0));
        }
        if let Some(c) = &style.color {
            run_css.push_str(&format!("color:#{c};"));
        }
        if let Some(fam) = &style.font_family {
            let family = css_font_family(fam);
            if !family.is_empty() {
                run_css.push_str(&format!("font-family:{family};"));
            }
        }
        if run_css.is_empty() {
            return base;
        }
        // Merge with whatever style_attr produced.
        if base.is_empty() {
            format!(" style=\"{run_css}\"")
        } else {
            // base is ` style="…"`; splice run_css before the closing quote.
            let inner = &base[" style=\"".len()..base.len() - 1];
            format!(" style=\"{inner}{run_css}\"")
        }
    }
}

/// Map a `w:spacing@line` (+ `@lineRule`) to a [`LineHeight`]. With `auto` (or
/// no rule) the value is 240ths of a line (`240` = single); with `exact`/
/// `atLeast` it is twentieths of a point. Returns `None` for an unparseable or
/// non-positive value.
fn line_spacing(line: &str, rule: Option<&str>) -> Option<LineHeight> {
    let n: f64 = line.trim().parse().ok()?;
    if n <= 0.0 {
        return None;
    }
    match rule {
        Some("exact") | Some("atLeast") => Some(LineHeight::Points(n / TWIP_PER_PT)),
        _ => Some(LineHeight::Multiple(n / 240.0)),
    }
}

/// Map a `w:jc@w:val` justification to a CSS `text-align` keyword.
fn jc_to_align(val: &str) -> Option<&'static str> {
    match val {
        "center" => Some("center"),
        "right" | "end" => Some("right"),
        "both" | "distribute" => Some("justify"),
        "left" | "start" => Some("left"),
        _ => None,
    }
}

/// Walk a DOCX body region (`w:body` or a `w:tc` cell), emitting HTML into `out`.
fn docx_body(xml: &str, ctx: &DocxCtx, out: &mut String) {
    let mut x = Xml::new(xml);
    // Walk only the top level of this region; tables and paragraphs recurse via
    // slices so a `w:tbl` is never double-emitted as loose paragraphs.
    docx_walk(&mut x, ctx, out, None);
}

/// Recursive DOCX walker. `stop` is the local tag name that ends the current
/// region (`None` at the top level). Handles `w:p`, `w:tbl`. Each top-level
/// numbered list carries a fresh counter set ([`ListCounters`]) so ordinals
/// restart per list.
fn docx_walk(x: &mut Xml, ctx: &DocxCtx, out: &mut String, stop: Option<&str>) {
    let mut counters = ListCounters::default();
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, _, sc) => {
                let ln = local(&name);
                if ln == "p" && !sc {
                    docx_paragraph(x, ctx, out, &mut counters);
                } else if ln == "tbl" && !sc {
                    docx_table(x, ctx, out);
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

/// Per-list running ordinal counters, keyed by `(numId, level)`. Incrementing a
/// level resets every deeper level so sub-lists renumber from 1.
#[derive(Default)]
struct ListCounters {
    counts: BTreeMap<(u32, u32), u32>,
}

impl ListCounters {
    /// Advance the counter for `(num_id, level)` and return the new 1-based value,
    /// resetting all deeper levels of the same list.
    fn next(&mut self, num_id: u32, level: u32) -> u32 {
        self.counts.retain(|&(n, l), _| !(n == num_id && l > level));
        let c = self.counts.entry((num_id, level)).or_insert(0);
        *c += 1;
        *c
    }
}

/// Paragraph-level list reference: `w:numPr/w:numId` + `w:ilvl`.
#[derive(Default, Clone, Copy)]
struct NumRef {
    num_id: Option<u32>,
    level: u32,
}

/// Emit one `w:p` (already consumed its open tag) until `</w:p>`.
fn docx_paragraph(x: &mut Xml, ctx: &DocxCtx, out: &mut String, counters: &mut ListCounters) {
    let mut heading: Option<u8> = None;
    let mut style_id: Option<String> = None;
    let mut inner = String::new();
    let mut run = RunStyle::default();
    let mut para = ParaStyle::default();
    let mut num_ref = NumRef::default();
    let mut in_rpr = false; // inside <w:rPr> (run properties)
    let mut in_ppr = false; // inside <w:pPr> (paragraph properties)
    let mut depth = 0i32; // nesting of <w:r> runs (to scope rPr)
    let mut field_instr = String::new(); // accumulating <w:instrText>
    let mut in_instr = false;

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
                                style_id = Some(v.to_string());
                            }
                        }
                    }
                    "jc" if in_ppr => {
                        if let Some(v) = attr(&attrs, "val") {
                            if let Some(a) = jc_to_align(v) {
                                para.align = Some(a);
                            }
                        }
                    }
                    "spacing" if in_ppr => {
                        para.space_before_pt = attr(&attrs, "before")
                            .and_then(twips_to_pt)
                            .or(para.space_before_pt);
                        para.space_after_pt = attr(&attrs, "after")
                            .and_then(twips_to_pt)
                            .or(para.space_after_pt);
                        if let Some(line) = attr(&attrs, "line") {
                            if let Some(lh) = line_spacing(line, attr(&attrs, "lineRule")) {
                                para.line_height = Some(lh);
                            }
                        }
                    }
                    "numPr" if in_ppr => {
                        // A paragraph in a list; default level 0 unless w:ilvl says.
                        para.list_level = Some(para.list_level.unwrap_or(0));
                    }
                    "ilvl" if in_ppr => {
                        if let Some(lvl) = attr(&attrs, "val").and_then(|v| v.trim().parse().ok()) {
                            para.list_level = Some(lvl);
                            num_ref.level = lvl;
                        }
                    }
                    "numId" if in_ppr => {
                        num_ref.num_id = attr(&attrs, "val").and_then(|v| v.trim().parse().ok());
                    }
                    "ind" if in_ppr => {
                        para.indent_left_pt = attr(&attrs, "left")
                            .and_then(twips_to_pt)
                            .or(para.indent_left_pt);
                        para.indent_right_pt = attr(&attrs, "right")
                            .and_then(twips_to_pt)
                            .or(para.indent_right_pt);
                        para.first_line_pt = attr(&attrs, "firstLine")
                            .and_then(twips_to_pt)
                            .or(para.first_line_pt);
                    }
                    "r" if !sc => {
                        depth += 1;
                        run = RunStyle::default();
                    }
                    "rFonts" if in_rpr => {
                        run.font_family = attr(&attrs, "ascii")
                            .or_else(|| attr(&attrs, "hAnsi"))
                            .filter(|v| !v.trim().is_empty())
                            .map(|v| v.to_string());
                    }
                    "b" if in_rpr => {
                        run.bold = !matches!(attr(&attrs, "val"), Some("0") | Some("false"))
                    }
                    "i" if in_rpr => {
                        run.italic = !matches!(attr(&attrs, "val"), Some("0") | Some("false"))
                    }
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
                    "instrText" => {
                        in_instr = true;
                    }
                    "tab" => inner.push(' '),
                    "br" | "cr" => inner.push_str("<br>"),
                    "blip" => {
                        if let Some(rid) = attr(&attrs, "embed").or_else(|| attr(&attrs, "link")) {
                            if let Some(tag) = ctx
                                .rels
                                .get(rid)
                                .map(|t| resolve_target("word", t))
                                .and_then(|k| img_tag(ctx.zip, &k))
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
                    "instrText" => in_instr = false,
                    "r" => depth = (depth - 1).max(0),
                    _ => {}
                }
            }
            Tok::Text(t) => {
                if in_instr {
                    // A field instruction (e.g. " PAGE \\* MERGEFORMAT ").
                    field_instr.push_str(&t);
                } else if depth > 0 && !t.is_empty() {
                    // Only surface text that lives inside a run (skip stray
                    // property text). `w:t` content arrives here.
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

    // Apply named-style + document-default formatting *under* the direct
    // properties already collected: anything the run/paragraph set inline wins;
    // the style fills the gaps. Heading level can also come from the style id.
    let resolved = ctx.styles.effective(style_id.as_deref());
    para.apply_style_defaults(&resolved);

    // PAGE / NUMPAGES field codes have no live value at convert time → a small
    // placeholder so the surrounding text still reads naturally.
    if let Some(rep) = field_code_placeholder(&field_instr) {
        if inner.trim().is_empty() {
            inner.push_str(rep);
        }
    }

    // List paragraphs get a numbering prefix: the real ordinal from
    // numbering.xml when known (1./a./i.…), else a bullet.
    if let Some(level) = para.list_level {
        if !inner.trim().is_empty() {
            let marker = list_marker(ctx, num_ref.num_id, level, counters);
            inner.insert_str(0, &format!("{marker} "));
        }
    }

    let trimmed = inner.trim();
    let para_attr = para.style_attr_with(&resolved);
    match heading {
        Some(n) if !trimmed.is_empty() => {
            out.push_str(&format!("<h{n}{para_attr}>{inner}</h{n}>"));
        }
        _ => {
            // Always emit a <p> (even empty) to preserve blank-line spacing.
            out.push_str(&format!("<p{para_attr}>{inner}</p>"));
        }
    }
}

/// Resolve a list paragraph's marker: the formatted ordinal from `numbering.xml`
/// (advancing the running counter), or a bullet when the format is bullet/
/// unknown or the numbering is missing.
fn list_marker(
    ctx: &DocxCtx,
    num_id: Option<u32>,
    level: u32,
    counters: &mut ListCounters,
) -> String {
    match num_id.and_then(|nid| ctx.numbering.fmt(nid, level).map(|f| (nid, f))) {
        Some((nid, fmt)) if !matches!(fmt, NumFmt::Bullet | NumFmt::Other) => {
            let n = counters.next(nid, level);
            fmt.render(n)
        }
        _ => "\u{2022}".to_string(),
    }
}

/// Map a Word field instruction to a static placeholder when it's one we can't
/// evaluate at convert time. `PAGE`/`NUMPAGES` get conventional placeholders;
/// everything else yields `None` (left to whatever literal text the field holds).
fn field_code_placeholder(instr: &str) -> Option<&'static str> {
    let upper = instr.trim().to_ascii_uppercase();
    let first = upper.split_whitespace().next().unwrap_or("");
    match first {
        "PAGE" => Some("1"),
        "NUMPAGES" => Some("1"),
        _ => None,
    }
}

/// Cell-merge metadata read from `w:tc/w:tcPr`.
#[derive(Default, Clone, Copy)]
struct CellSpan {
    /// `w:gridSpan@w:val` — horizontal merge (columns covered).
    grid_span: usize,
    /// `w:vMerge` with no `@w:val` (or `="continue"`) — this cell is the
    /// continuation of a vertical merge started above.
    v_merge_continue: bool,
    /// `w:vMerge@w:val="restart"` — this cell starts a vertical merge.
    v_merge_restart: bool,
}

/// Emit one `w:tbl` (open already consumed) as an HTML `<table>`. Reads the
/// `w:tblGrid` (`w:gridCol w:w`, twips) into a leading `<colgroup>` so the
/// layout honours real column widths. Honours cell merges: `w:gridSpan` widens
/// the cell (expanded to that many physical `<td>`s so the layout reflects the
/// span, the first carrying a `colspan` for forward-compat); `w:vMerge` carries
/// a `rowspan` hint and the covered continuation cells are dropped.
fn docx_table(x: &mut Xml, ctx: &DocxCtx, out: &mut String) {
    out.push_str("<table>");
    // Collect `w:gridCol w:w` widths (twips→pt) and flush them as a <colgroup>
    // just before the first row. `w:tblGrid` always precedes the rows.
    let mut col_pts: Vec<f64> = Vec::new();
    let mut colgroup_done = false;
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => {
                let ln = local(&name);
                if ln == "gridCol" {
                    if let Some(w) = attr(&attrs, "w").and_then(twips_to_pt) {
                        if w > 0.0 {
                            col_pts.push(w);
                        }
                    }
                } else if ln == "tr" && !sc {
                    flush_colgroup(&mut col_pts, &mut colgroup_done, out);
                    out.push_str("<tr>");
                } else if ln == "tc" && !sc {
                    docx_cell(x, ctx, out);
                }
            }
            Tok::Close(name) => {
                let ln = local(&name);
                if ln == "tr" {
                    out.push_str("</tr>");
                } else if ln == "tbl" {
                    break;
                }
            }
            Tok::Text(_) => {}
        }
    }
    out.push_str("</table>");
}

/// Emit a `<colgroup>` of `<col style="width:Xpt">` from collected point widths,
/// once, before the first row. No-op when no widths were declared.
fn flush_colgroup(col_pts: &mut Vec<f64>, done: &mut bool, out: &mut String) {
    if *done {
        return;
    }
    *done = true;
    if col_pts.is_empty() {
        return;
    }
    out.push_str("<colgroup>");
    for w in col_pts.drain(..) {
        out.push_str(&format!("<col style=\"width:{}pt\">", fmt_pt(w)));
    }
    out.push_str("</colgroup>");
}

/// Format a point value compactly (trim trailing zeros) for inline CSS.
fn fmt_pt(v: f64) -> String {
    let mut s = format!("{v:.2}");
    while s.contains('.') && s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    s
}

/// Emit one `w:tc` cell (open already consumed) until `</w:tc>`, applying its
/// `w:tcPr` merge properties. A `w:gridSpan="N"` cell is emitted as N physical
/// `<td>`s (content + `colspan="N"` in the first, the rest empty) so the
/// equal-width table layout still spreads the cell across N columns. A
/// `w:vMerge` continuation cell is suppressed (its content belongs to the
/// restart cell above); a restart cell gets a `rowspan="2"` hint.
fn docx_cell(x: &mut Xml, ctx: &DocxCtx, out: &mut String) {
    let mut span = CellSpan::default();
    let mut in_tcpr = false;
    let mut body = String::new();
    // Cells are self-contained for numbering (a list rarely spans cells).
    let mut counters = ListCounters::default();

    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => {
                let ln = local(&name);
                match ln {
                    "tcPr" if !sc => in_tcpr = true,
                    "gridSpan" if in_tcpr => {
                        span.grid_span = attr(&attrs, "val")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                    }
                    "vMerge" if in_tcpr => match attr(&attrs, "val") {
                        Some("restart") => span.v_merge_restart = true,
                        // No val (or "continue") → this is a covered cell.
                        _ => span.v_merge_continue = true,
                    },
                    "p" if !sc => docx_paragraph(x, ctx, &mut body, &mut counters),
                    "tbl" if !sc => docx_table(x, ctx, &mut body),
                    _ => {}
                }
            }
            Tok::Close(name) => {
                let ln = local(&name);
                if ln == "tcPr" {
                    in_tcpr = false;
                } else if ln == "tc" {
                    break;
                }
            }
            Tok::Text(_) => {}
        }
    }

    // A vertical-merge continuation cell is covered by the restart cell above:
    // drop it so the column count of the row above is preserved.
    if span.v_merge_continue {
        return;
    }

    let trimmed = body.trim();
    let cols = span.grid_span.max(1);
    let colspan_attr = if cols > 1 {
        format!(" colspan=\"{cols}\"")
    } else {
        String::new()
    };
    let rowspan_attr = if span.v_merge_restart {
        " rowspan=\"2\""
    } else {
        ""
    };
    out.push_str(&format!("<td{colspan_attr}{rowspan_attr}>{trimmed}</td>"));
    // Pad with empty cells so the equal-column layout actually advances `cols`
    // columns for this logically-merged cell.
    for _ in 1..cols {
        out.push_str("<td></td>");
    }
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

// ───────────────────────── DOCX named styles (styles.xml) ──────────────────────

/// The subset of run/paragraph formatting a `w:style` (or `w:docDefaults`) can
/// carry. Every field is optional so a child style / direct property can be
/// *merged on top* of an inherited one (`merge_under`): the more specific value
/// wins, the inherited value fills the gaps.
#[derive(Default, Clone)]
struct DocxStyle {
    // Run properties (`w:rPr`).
    bold: Option<bool>,
    italic: Option<bool>,
    underline: Option<bool>,
    size_half_pt: Option<f64>,
    color: Option<String>,
    font_family: Option<String>,
    // Paragraph properties (`w:pPr`).
    align: Option<&'static str>,
    space_before_pt: Option<f64>,
    space_after_pt: Option<f64>,
    indent_left_pt: Option<f64>,
    indent_right_pt: Option<f64>,
    first_line_pt: Option<f64>,
    line_height: Option<LineHeight>,
}

impl DocxStyle {
    /// Fill any property left unset here from `base` (the inherited/lower-priority
    /// style). Self's already-set values win.
    fn fill_from(&mut self, base: &DocxStyle) {
        macro_rules! inherit {
            ($($f:ident),* $(,)?) => {$(
                if self.$f.is_none() { self.$f = base.$f.clone(); }
            )*};
        }
        inherit!(
            bold,
            italic,
            underline,
            size_half_pt,
            color,
            font_family,
            align,
            space_before_pt,
            space_after_pt,
            indent_left_pt,
            indent_right_pt,
            first_line_pt,
            line_height,
        );
    }
}

/// Resolved DOCX styles: per-style-id formatting with `w:basedOn` chains already
/// flattened, plus the document defaults (`w:docDefaults`). Built once per
/// document from `word/styles.xml`.
#[derive(Default)]
struct DocxStyles {
    /// Document defaults (`w:rPrDefault` + `w:pPrDefault`) — the baseline under
    /// every paragraph, below even the paragraph's own named style.
    defaults: DocxStyle,
    /// styleId → fully-resolved (basedOn-flattened) formatting.
    by_id: BTreeMap<String, DocxStyle>,
}

impl DocxStyles {
    /// The effective formatting for a paragraph whose `w:pStyle` is `style_id`:
    /// the named style merged over the document defaults. With no style id (or an
    /// unknown one) just the defaults.
    fn effective(&self, style_id: Option<&str>) -> DocxStyle {
        let mut s = style_id
            .and_then(|id| self.by_id.get(id))
            .cloned()
            .unwrap_or_default();
        s.fill_from(&self.defaults);
        s
    }
}

/// Parse `word/styles.xml` into a [`DocxStyles`]: read each `w:style`'s direct
/// `w:rPr`/`w:pPr` and `w:basedOn`, then flatten the inheritance chains so each
/// id maps to its fully-resolved formatting. `w:docDefaults` seeds the baseline.
fn parse_docx_styles(xml: &str) -> DocxStyles {
    // Raw, pre-resolution data per style id: (basedOn, own props).
    let mut raw: BTreeMap<String, (Option<String>, DocxStyle)> = BTreeMap::new();
    let mut defaults = DocxStyle::default();

    let mut x = Xml::new(xml);
    // Walk state.
    let mut cur_id: Option<String> = None;
    let mut cur_based: Option<String> = None;
    let mut cur = DocxStyle::default();
    let mut in_style = false;
    let mut in_defaults = false; // inside <w:docDefaults>
    let mut in_rpr = false;
    let mut in_ppr = false;

    // Apply one run/paragraph property element to a `DocxStyle` target.
    fn apply_prop(t: &mut DocxStyle, ln: &str, attrs: &[(String, String)], in_rpr: bool) {
        match ln {
            "rFonts" if in_rpr => {
                if let Some(v) = attr(attrs, "ascii")
                    .or_else(|| attr(attrs, "hAnsi"))
                    .filter(|v| !v.trim().is_empty())
                {
                    t.font_family = Some(v.to_string());
                }
            }
            "b" if in_rpr => {
                t.bold = Some(!matches!(attr(attrs, "val"), Some("0") | Some("false")))
            }
            "i" if in_rpr => {
                t.italic = Some(!matches!(attr(attrs, "val"), Some("0") | Some("false")))
            }
            "u" if in_rpr => t.underline = Some(!matches!(attr(attrs, "val"), Some("none"))),
            "sz" if in_rpr => {
                if let Some(v) = attr(attrs, "val").and_then(|v| v.parse().ok()) {
                    t.size_half_pt = Some(v);
                }
            }
            "color" if in_rpr => {
                if let Some(v) = attr(attrs, "val") {
                    if v != "auto" && is_hex6(v) {
                        t.color = Some(v.to_ascii_uppercase());
                    }
                }
            }
            "jc" => {
                if let Some(a) = attr(attrs, "val").and_then(jc_to_align) {
                    t.align = Some(a);
                }
            }
            "spacing" => {
                if let Some(v) = attr(attrs, "before").and_then(twips_to_pt) {
                    t.space_before_pt = Some(v);
                }
                if let Some(v) = attr(attrs, "after").and_then(twips_to_pt) {
                    t.space_after_pt = Some(v);
                }
                if let Some(line) = attr(attrs, "line") {
                    if let Some(lh) = line_spacing(line, attr(attrs, "lineRule")) {
                        t.line_height = Some(lh);
                    }
                }
            }
            "ind" => {
                if let Some(v) = attr(attrs, "left").and_then(twips_to_pt) {
                    t.indent_left_pt = Some(v);
                }
                if let Some(v) = attr(attrs, "right").and_then(twips_to_pt) {
                    t.indent_right_pt = Some(v);
                }
                if let Some(v) = attr(attrs, "firstLine").and_then(twips_to_pt) {
                    t.first_line_pt = Some(v);
                }
            }
            _ => {}
        }
    }

    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => {
                let ln = local(&name);
                match ln {
                    "docDefaults" if !sc => in_defaults = true,
                    "style" if !sc && !in_defaults => {
                        in_style = true;
                        cur_id = attr(&attrs, "styleId").map(|s| s.to_string());
                        cur_based = None;
                        cur = DocxStyle::default();
                    }
                    "basedOn" if in_style => {
                        cur_based = attr(&attrs, "val").map(|s| s.to_string());
                    }
                    "rPr" if !sc => in_rpr = true,
                    "pPr" if !sc => in_ppr = true,
                    _ => {
                        if in_rpr || in_ppr {
                            let target = if in_defaults && !in_style {
                                &mut defaults
                            } else {
                                &mut cur
                            };
                            apply_prop(target, ln, &attrs, in_rpr);
                        }
                    }
                }
            }
            Tok::Close(name) => match local(&name) {
                "rPr" => in_rpr = false,
                "pPr" => in_ppr = false,
                "docDefaults" => in_defaults = false,
                "style" if in_style => {
                    if let Some(id) = cur_id.take() {
                        raw.insert(id, (cur_based.take(), std::mem::take(&mut cur)));
                    }
                    in_style = false;
                }
                _ => {}
            },
            Tok::Text(_) => {}
        }
    }

    // Flatten basedOn chains (bounded depth guards against cycles).
    let mut by_id: BTreeMap<String, DocxStyle> = BTreeMap::new();
    for id in raw.keys() {
        let mut resolved = DocxStyle::default();
        let mut cur_id = Some(id.clone());
        let mut seen = 0;
        // Collect the chain id → basedOn → … then merge from most-specific down.
        let mut chain: Vec<&DocxStyle> = Vec::new();
        while let Some(cid) = cur_id {
            let Some((based, props)) = raw.get(&cid) else {
                break;
            };
            chain.push(props);
            cur_id = based.clone();
            seen += 1;
            if seen > 32 {
                break;
            }
        }
        for props in chain {
            resolved.fill_from(props);
        }
        by_id.insert(id.clone(), resolved);
    }

    DocxStyles { defaults, by_id }
}

// ─────────────────────── DOCX list numbering (numbering.xml) ───────────────────

/// A DOCX number format kept per list level — enough to render the ordinal.
#[derive(Debug, Clone, Copy, PartialEq)]
enum NumFmt {
    Decimal,
    LowerLetter,
    UpperLetter,
    LowerRoman,
    UpperRoman,
    Bullet,
    /// Anything we don't reconstruct (`decimalZero`, `ordinal`, …) → bullet.
    Other,
}

impl NumFmt {
    fn parse(s: &str) -> NumFmt {
        match s {
            "decimal" | "decimalZero" => NumFmt::Decimal,
            "lowerLetter" => NumFmt::LowerLetter,
            "upperLetter" => NumFmt::UpperLetter,
            "lowerRoman" => NumFmt::LowerRoman,
            "upperRoman" => NumFmt::UpperRoman,
            "bullet" | "none" => NumFmt::Bullet,
            _ => NumFmt::Other,
        }
    }

    /// Render `n` (1-based) for this format. Bullet/other formats yield `•`.
    fn render(self, n: u32) -> String {
        match self {
            NumFmt::Decimal => format!("{n}."),
            NumFmt::LowerLetter => format!("{}.", alpha_ordinal(n, false)),
            NumFmt::UpperLetter => format!("{}.", alpha_ordinal(n, true)),
            NumFmt::LowerRoman => format!("{}.", roman(n, false)),
            NumFmt::UpperRoman => format!("{}.", roman(n, true)),
            NumFmt::Bullet | NumFmt::Other => "\u{2022}".to_string(),
        }
    }
}

/// Spreadsheet-style ordinal: 1→a, 26→z, 27→aa (lower/upper).
fn alpha_ordinal(n: u32, upper: bool) -> String {
    if n == 0 {
        return String::new();
    }
    let mut n = n;
    let mut s = Vec::new();
    while n > 0 {
        let rem = ((n - 1) % 26) as u8;
        s.push(if upper { b'A' + rem } else { b'a' + rem });
        n = (n - 1) / 26;
    }
    s.reverse();
    String::from_utf8(s).unwrap_or_default()
}

/// Roman numeral for `n` (1..=3999 effectively; larger just repeats M).
fn roman(n: u32, upper: bool) -> String {
    if n == 0 {
        return String::new();
    }
    const VALS: [(u32, &str); 13] = [
        (1000, "m"),
        (900, "cm"),
        (500, "d"),
        (400, "cd"),
        (100, "c"),
        (90, "xc"),
        (50, "l"),
        (40, "xl"),
        (10, "x"),
        (9, "ix"),
        (5, "v"),
        (4, "iv"),
        (1, "i"),
    ];
    let mut n = n;
    let mut out = String::new();
    for (v, sym) in VALS {
        while n >= v {
            out.push_str(sym);
            n -= v;
        }
    }
    if upper {
        out.to_ascii_uppercase()
    } else {
        out
    }
}

/// Resolved DOCX numbering: each `w:numId` maps to a per-level number format.
/// Built from `word/numbering.xml`'s `w:num → w:abstractNumId → w:lvl@w:numFmt`.
#[derive(Default)]
struct DocxNumbering {
    /// numId → (level → format). Levels are 0-based.
    by_num: BTreeMap<u32, BTreeMap<u32, NumFmt>>,
}

impl DocxNumbering {
    /// Format for a given list (`numId`) at `level`, if known.
    fn fmt(&self, num_id: u32, level: u32) -> Option<NumFmt> {
        self.by_num.get(&num_id)?.get(&level).copied()
    }
}

/// Parse `word/numbering.xml`: collect `w:abstractNum` level formats, then map
/// each `w:num@w:numId` to its `w:abstractNumId`. Returns numId → level → format.
fn parse_docx_numbering(xml: &str) -> DocxNumbering {
    // abstractNumId → (level → format).
    let mut abstracts: BTreeMap<u32, BTreeMap<u32, NumFmt>> = BTreeMap::new();
    // numId → abstractNumId.
    let mut num_to_abstract: BTreeMap<u32, u32> = BTreeMap::new();

    let mut x = Xml::new(xml);
    let mut cur_abstract: Option<u32> = None;
    let mut cur_level: Option<u32> = None;
    // num mapping context.
    let mut cur_num: Option<u32> = None;
    let mut in_num = false; // inside <w:num> (vs <w:abstractNum>)

    while let Some(tok) = x.next() {
        if let Tok::Open(name, attrs, _) = tok {
            match local(&name) {
                "abstractNum" => {
                    cur_abstract =
                        attr(&attrs, "abstractNumId").and_then(|v| v.trim().parse::<u32>().ok());
                    if let Some(a) = cur_abstract {
                        abstracts.entry(a).or_default();
                    }
                    in_num = false;
                }
                "lvl" => {
                    cur_level = attr(&attrs, "ilvl").and_then(|v| v.trim().parse::<u32>().ok());
                }
                "numFmt" => {
                    if let (Some(a), Some(l)) = (cur_abstract, cur_level) {
                        if let Some(v) = attr(&attrs, "val") {
                            abstracts.entry(a).or_default().insert(l, NumFmt::parse(v));
                        }
                    }
                }
                "num" => {
                    in_num = true;
                    cur_num = attr(&attrs, "numId").and_then(|v| v.trim().parse::<u32>().ok());
                }
                "abstractNumId" if in_num => {
                    if let (Some(n), Some(a)) = (
                        cur_num,
                        attr(&attrs, "val").and_then(|v| v.trim().parse::<u32>().ok()),
                    ) {
                        num_to_abstract.insert(n, a);
                    }
                }
                _ => {}
            }
        }
    }

    let mut by_num = BTreeMap::new();
    for (num_id, abstract_id) in num_to_abstract {
        if let Some(levels) = abstracts.get(&abstract_id) {
            by_num.insert(num_id, levels.clone());
        }
    }
    DocxNumbering { by_num }
}

// ───────────────────────── DOCX footnotes (footnotes.xml) ──────────────────────

/// Footnote bodies from `word/footnotes.xml`, in reference order. The synthetic
/// `separator`/`continuationSeparator` notes (ids `0`/`-1`) are dropped; the rest
/// keep their document order so the trailing section numbers them 1, 2, 3, ….
#[derive(Default)]
struct DocxFootnotes {
    /// Pre-escaped HTML text of each real footnote, in id order.
    ordered: Vec<String>,
}

/// Parse `word/footnotes.xml`, extracting the plain text of each real
/// `w:footnote` (skipping the `separator`/`continuationSeparator` placeholders).
fn parse_docx_footnotes(xml: &str) -> DocxFootnotes {
    let mut ordered: Vec<String> = Vec::new();
    if xml.trim().is_empty() {
        return DocxFootnotes { ordered };
    }
    let mut x = Xml::new(xml);
    let mut in_note = false;
    let mut skip = false; // separator / continuationSeparator
    let mut in_text = false; // inside <w:t>
    let mut cur = String::new();
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, _) => match local(&name) {
                "footnote" => {
                    in_note = true;
                    cur.clear();
                    skip = matches!(
                        attr(&attrs, "type"),
                        Some("separator") | Some("continuationSeparator")
                    );
                }
                "t" if in_note => in_text = true,
                "tab" if in_note => cur.push(' '),
                _ => {}
            },
            Tok::Text(t) => {
                if in_note && in_text && !skip {
                    esc(&t, &mut cur);
                }
            }
            Tok::Close(name) => match local(&name) {
                "t" => in_text = false,
                "footnote" => {
                    if in_note && !skip && !cur.trim().is_empty() {
                        ordered.push(cur.trim().to_string());
                    }
                    in_note = false;
                }
                _ => {}
            },
        }
    }
    DocxFootnotes { ordered }
}

// ════════════════════════════════════ XLSX ════════════════════════════════════

/// XLSX → one HTML `<table>` per sheet (page break between), sheet name as
/// `<h2>`. Resolves `t="s"` shared strings and the exporter's own
/// `t="inlineStr"` cells, positioning each cell by its column letter so columns
/// align, and colours cells from their style's solid fill. Rendered landscape
/// for width.
pub fn xlsx_to_pdf(zip: &BTreeMap<String, Vec<u8>>) -> Vec<u8> {
    let shared = zip
        .get("xl/sharedStrings.xml")
        .map(|b| parse_shared_strings(&String::from_utf8_lossy(b)))
        .unwrap_or_default();

    // Workbook theme colour scheme, for `@theme`+`tint` fill resolution.
    let theme = xlsx_theme(zip);

    // Cell-style index → resolved formatting (solid fill colour + number format),
    // from xl/styles.xml. Resolves theme/indexed colours and the numFmt table.
    let styles = zip
        .get("xl/styles.xml")
        .map(|b| parse_xlsx_styles(&String::from_utf8_lossy(b), &theme))
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
        body.push_str(&xlsx_sheet_table(xml, &shared, &styles));
    }
    if sheets.is_empty() {
        body.push_str("<p></p>");
    }
    // Spreadsheets have no single declared page size; render landscape for width.
    render_geom(&body, PageGeom::tabular_default())
}

/// Render one worksheet XML to an HTML `<table>`, gap-filling so cells land in
/// their declared column (`r="C3"`), colouring each cell from its style index
/// (`c@s` → [`XlsxStyles::fill`] → `background-color`, with theme/indexed
/// resolution), formatting numeric/date cells via their `numFmt`
/// ([`XlsxStyles::num_fmt`] → [`format_cell_number`]), and honouring
/// `<mergeCells>` by emitting `colspan`/`rowspan` on anchor cells and skipping
/// the covered ones.
fn xlsx_sheet_table(xml: &str, shared: &[String], styles: &XlsxStyles) -> String {
    // Merge regions: anchors carry spans, covered cells are suppressed.
    let merges = MergeMap::build(&parse_merges(xml));

    let mut out = String::from("<table>");
    let mut x = Xml::new(xml);
    let mut in_sheet_data = false;
    // (col_index, escaped html, optional `#RRGGBB` background).
    let mut row_cells: Vec<(usize, String, Option<String>)> = Vec::new();
    let mut row_open = false;
    // 0-based index of the current row: from the row's `r` attribute when
    // present, else a running counter incremented per `<row>`.
    let mut row_idx = 0usize;
    let mut next_auto_row = 0usize;

    // Current-cell scratch.
    let mut cell_col = 0usize;
    let mut cell_type = String::new();
    let mut cell_text = String::new();
    let mut cell_bg: Option<String> = None;
    // numFmt code resolved from `c@s`, applied to numeric cells at close.
    let mut cell_fmt: Option<String> = None;
    let mut in_cell = false;
    let mut in_value = false; // inside <v> or <t>

    let flush_row =
        |row: usize, row_cells: &mut Vec<(usize, String, Option<String>)>, out: &mut String| {
            if row_cells.is_empty() {
                out.push_str("<tr></tr>");
                return;
            }
            out.push_str("<tr>");
            let max_col = row_cells.iter().map(|(c, _, _)| *c).max().unwrap_or(0);
            let mut by_col: BTreeMap<usize, (String, Option<String>)> = BTreeMap::new();
            for (c, h, bg) in row_cells.drain(..) {
                by_col.insert(c, (h, bg));
            }
            for c in 0..=max_col {
                // A cell covered by a merge (not its anchor) is dropped entirely.
                if merges.is_covered(row, c) {
                    continue;
                }
                let span = merges
                    .anchor(row, c)
                    .map(|(cs, rs)| span_attrs(cs, rs))
                    .unwrap_or_default();
                match by_col.get(&c) {
                    Some((h, Some(bg))) => out.push_str(&format!(
                        "<td{span} style=\"background-color:{bg}\">{h}</td>"
                    )),
                    Some((h, None)) => out.push_str(&format!("<td{span}>{h}</td>")),
                    None => out.push_str(&format!("<td{span}></td>")),
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
                    // `<row r="N">` is 1-based; fall back to the running counter.
                    row_idx = attr(&attrs, "r")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .map(|n| n.saturating_sub(1))
                        .unwrap_or(next_auto_row);
                    next_auto_row = row_idx + 1;
                }
                "c" if in_sheet_data => {
                    in_cell = true;
                    cell_text.clear();
                    cell_type = attr(&attrs, "t").unwrap_or("n").to_string();
                    cell_col = attr(&attrs, "r").map(col_of_ref).unwrap_or(0);
                    // `c@s` is the cellXfs index → solid-fill colour + numFmt.
                    let style_idx = attr(&attrs, "s").and_then(|v| v.trim().parse::<usize>().ok());
                    cell_bg = style_idx.and_then(|i| styles.fill(i));
                    cell_fmt = style_idx
                        .and_then(|i| styles.num_fmt(i))
                        .map(|(_, code)| code.clone());
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
                            // Numeric/date cell: apply its number format when one
                            // is set and the value parses; else show as-is.
                            match cell_fmt
                                .as_deref()
                                .and_then(|code| format_cell_number(cell_text.trim(), code))
                            {
                                Some(formatted) => formatted,
                                None => cell_text.clone(),
                            }
                        };
                        row_cells.push((cell_col, escaped(resolved.trim()), cell_bg.take()));
                        cell_fmt = None;
                    }
                    in_cell = false;
                }
                "row" => {
                    if row_open {
                        flush_row(row_idx, &mut row_cells, &mut out);
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

/// Build the ` colspan="…" rowspan="…"` attribute fragment for a merge anchor,
/// emitting each part only when it spans more than one cell.
fn span_attrs(colspan: usize, rowspan: usize) -> String {
    let mut s = String::new();
    if colspan > 1 {
        s.push_str(&format!(" colspan=\"{colspan}\""));
    }
    if rowspan > 1 {
        s.push_str(&format!(" rowspan=\"{rowspan}\""));
    }
    s
}

/// Resolved per-cell-style XLSX formatting: for each `cellXfs` index (a cell's
/// `@s`), the solid-fill background colour (if any) and the number-format id +
/// its format code, so numeric cells can be formatted (dates, currency, …).
#[derive(Default)]
struct XlsxStyles {
    /// cellXfs index → `Some("#RRGGBB")` solid fill, else `None`.
    fills: Vec<Option<String>>,
    /// cellXfs index → `(numFmtId, format-code)`. The code is resolved from the
    /// built-in table or the custom `<numFmts>` map; `None` when general/absent.
    num_fmts: Vec<Option<(u32, String)>>,
}

impl XlsxStyles {
    fn fill(&self, idx: usize) -> Option<String> {
        self.fills.get(idx).and_then(|c| c.clone())
    }
    fn num_fmt(&self, idx: usize) -> Option<&(u32, String)> {
        self.num_fmts.get(idx).and_then(|f| f.as_ref())
    }
}

/// Parse `xl/styles.xml` (with `theme` for theme-colour resolution) into an
/// [`XlsxStyles`]: the `cellXfs` order maps each style index to its solid-fill
/// colour (`@fillId → fills[…] → patternFill@fgColor`, resolving `rgb`,
/// `theme`+`tint` and `indexed`) and its number format (`@numFmtId`, resolved
/// against the built-in ids and the custom `<numFmts>` map).
fn parse_xlsx_styles(xml: &str, theme: &XlsxTheme) -> XlsxStyles {
    // Pass 0: custom number formats (numFmtId → formatCode).
    let mut custom_fmts: BTreeMap<u32, String> = BTreeMap::new();
    {
        let mut x = Xml::new(xml);
        while let Some(tok) = x.next() {
            if let Tok::Open(name, attrs, _) = tok {
                if local(&name) == "numFmt" {
                    if let (Some(id), Some(code)) = (
                        attr(&attrs, "numFmtId").and_then(|v| v.trim().parse::<u32>().ok()),
                        attr(&attrs, "formatCode"),
                    ) {
                        custom_fmts.insert(id, code.to_string());
                    }
                }
            }
        }
    }

    // Pass 1: fillId → colour. `fills` is an ordered list of `<fill>`.
    let mut fill_colors: Vec<Option<String>> = Vec::new();
    {
        let mut x = Xml::new(xml);
        let mut in_fills = false;
        let mut cur: Option<String> = None;
        let mut solid = false;
        while let Some(tok) = x.next() {
            match tok {
                Tok::Open(name, attrs, sc) => match local(&name) {
                    "fills" if !sc => in_fills = true,
                    "patternFill" if in_fills => {
                        solid = matches!(attr(&attrs, "patternType"), Some("solid"));
                        // Some writers put fgColor as an attribute; usually a child.
                        if solid {
                            cur = argb_to_hex6(attr(&attrs, "fgColor"));
                        }
                    }
                    "fgColor" if in_fills && solid => {
                        cur = xlsx_color(&attrs, theme).or(cur.take());
                    }
                    _ => {}
                },
                Tok::Close(name) => match local(&name) {
                    "fill" if in_fills => {
                        fill_colors.push(cur.take());
                        solid = false;
                    }
                    "fills" => in_fills = false,
                    _ => {}
                },
                Tok::Text(_) => {}
            }
        }
    }

    // Pass 2: cellXfs order → (fillId → colour, numFmtId → format code).
    let mut fills: Vec<Option<String>> = Vec::new();
    let mut num_fmts: Vec<Option<(u32, String)>> = Vec::new();
    {
        let mut x = Xml::new(xml);
        let mut in_cellxfs = false;
        while let Some(tok) = x.next() {
            match tok {
                Tok::Open(name, attrs, _) => match local(&name) {
                    "cellXfs" => in_cellxfs = true,
                    "xf" if in_cellxfs => {
                        let color = attr(&attrs, "fillId")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .and_then(|fid| fill_colors.get(fid))
                            .and_then(|c| c.clone());
                        fills.push(color);
                        let fmt = attr(&attrs, "numFmtId")
                            .and_then(|v| v.trim().parse::<u32>().ok())
                            .and_then(|id| num_fmt_code(id, &custom_fmts).map(|code| (id, code)));
                        num_fmts.push(fmt);
                    }
                    _ => {}
                },
                Tok::Close(name) => {
                    if local(&name) == "cellXfs" {
                        in_cellxfs = false;
                    }
                }
                Tok::Text(_) => {}
            }
        }
    }
    XlsxStyles { fills, num_fmts }
}

/// Resolve a colour element's `rgb` / `theme`+`tint` / `indexed` attributes to
/// `#RRGGBB`, or `None`. Used for `fgColor`/`bgColor`/font `color`.
fn xlsx_color(attrs: &[(String, String)], theme: &XlsxTheme) -> Option<String> {
    if let Some(c) = argb_to_hex6(attr(attrs, "rgb")) {
        return Some(c);
    }
    if let Some(idx) = attr(attrs, "theme").and_then(|v| v.trim().parse::<usize>().ok()) {
        let tint = attr(attrs, "tint")
            .and_then(|v| v.trim().parse::<f64>().ok())
            .unwrap_or(0.0);
        return theme.color(idx).map(|rgb| apply_tint(rgb, tint));
    }
    if let Some(idx) = attr(attrs, "indexed").and_then(|v| v.trim().parse::<usize>().ok()) {
        return indexed_color(idx);
    }
    None
}

/// Apply an OOXML `tint` (-1.0…1.0) to an `#RRGGBB`: negative darkens toward
/// black, positive lightens toward white (HSL-luminance approximation that the
/// simple linear blend captures well enough for cell shading).
fn apply_tint(hex: [u8; 3], tint: f64) -> String {
    let t = tint.clamp(-1.0, 1.0);
    let adj = |c: u8| -> u8 {
        let c = c as f64;
        let v = if t < 0.0 {
            c * (1.0 + t)
        } else {
            c * (1.0 - t) + 255.0 * t
        };
        v.round().clamp(0.0, 255.0) as u8
    };
    format!("#{:02X}{:02X}{:02X}", adj(hex[0]), adj(hex[1]), adj(hex[2]))
}

/// The standard XLSX indexed colour palette (legacy `indexed` attribute). Only
/// the well-defined slots are mapped; out-of-range indices yield `None`.
fn indexed_color(idx: usize) -> Option<String> {
    // Classic 56-entry palette; indices 0..=7 duplicate 8..=15 historically.
    const PALETTE: [&str; 56] = [
        "000000", "FFFFFF", "FF0000", "00FF00", "0000FF", "FFFF00", "FF00FF", "00FFFF", "000000",
        "FFFFFF", "FF0000", "00FF00", "0000FF", "FFFF00", "FF00FF", "00FFFF", "800000", "008000",
        "000080", "808000", "800080", "008080", "C0C0C0", "808080", "9999FF", "993366", "FFFFCC",
        "CCFFFF", "660066", "FF8080", "0066CC", "CCCCFF", "000080", "FF00FF", "FFFF00", "00FFFF",
        "800080", "800000", "008080", "0000FF", "00CCFF", "CCFFFF", "CCFFCC", "FFFF99", "99CCFF",
        "FF99CC", "CC99FF", "FFCC99", "3366FF", "33CCCC", "99CC00", "FFCC00", "FF9900", "FF6600",
        "666699", "969696",
    ];
    // 64/65 are system foreground/background (black/white).
    match idx {
        i if i < PALETTE.len() => Some(format!("#{}", PALETTE[i])),
        64 => Some("#000000".to_string()),
        65 => Some("#FFFFFF".to_string()),
        _ => None,
    }
}

/// Convert an XLSX colour string to `#RRGGBB`, or `None`. XLSX `rgb` is ARGB
/// (`FFFFFF00`); the leading alpha byte is dropped. A bare `RRGGBB` is also
/// accepted. Fully-transparent (`00……`) and unparseable values yield `None`.
fn argb_to_hex6(v: Option<&str>) -> Option<String> {
    let s = v?.trim();
    let s = s.strip_prefix('#').unwrap_or(s);
    let (alpha, rgb) = match s.len() {
        8 => (Some(&s[0..2]), &s[2..8]),
        6 => (None, s),
        _ => return None,
    };
    if !rgb.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    // Drop a fully transparent colour (alpha 00).
    if alpha == Some("00") {
        return None;
    }
    Some(format!("#{}", rgb.to_ascii_uppercase()))
}

// ───────────────────────── XLSX theme colours (theme1.xml) ─────────────────────

/// The workbook theme's colour scheme (`a:clrScheme`), indexed the way cell
/// styles reference it via `@theme`: 0=lt1(bg1), 1=dk1(tx1), 2=lt2(bg2),
/// 3=dk2(tx2), 4=accent1, …, 9=accent6, 10=hlink, 11=folHlink. (Spreadsheet
/// theme indices swap the first two pairs vs the scheme's document order.)
#[derive(Default, Clone)]
struct XlsxTheme {
    /// Theme colour slot → `[r,g,b]`. Empty when no theme part was present.
    colors: Vec<[u8; 3]>,
}

impl XlsxTheme {
    fn color(&self, idx: usize) -> Option<[u8; 3]> {
        self.colors.get(idx).copied()
    }
}

/// Read the workbook theme part's `a:clrScheme`, mapping each named entry to the
/// **spreadsheet theme index** order (dk1/lt1 swapped to lt1/dk1, dk2/lt2 to
/// lt2/dk2) used by cell `@theme` references.
fn xlsx_theme(zip: &BTreeMap<String, Vec<u8>>) -> XlsxTheme {
    let key = zip
        .keys()
        .filter(|k| k.starts_with("xl/theme/theme") && k.ends_with(".xml"))
        .min();
    let Some(key) = key else {
        return XlsxTheme::default();
    };
    parse_xlsx_theme(&String::from_utf8_lossy(&zip[key]))
}

/// Parse `a:clrScheme`: collect (name → rgb) for dk1/lt1/dk2/lt2/accent1..6/
/// hlink/folHlink, then emit them in the cell `@theme` index order.
fn parse_xlsx_theme(xml: &str) -> XlsxTheme {
    let mut named: BTreeMap<&'static str, [u8; 3]> = BTreeMap::new();
    let mut x = Xml::new(xml);
    let mut in_scheme = false;
    // Current scheme slot name we're inside (dk1, lt1, accent1, …).
    let mut cur_slot: Option<&'static str> = None;

    const SLOTS: [&str; 12] = [
        "dk1", "lt1", "dk2", "lt2", "accent1", "accent2", "accent3", "accent4", "accent5",
        "accent6", "hlink", "folHlink",
    ];
    let slot_name = |ln: &str| -> Option<&'static str> { SLOTS.iter().copied().find(|s| *s == ln) };

    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, _) => {
                let ln = local(&name);
                if ln == "clrScheme" {
                    in_scheme = true;
                } else if in_scheme {
                    if let Some(slot) = slot_name(ln) {
                        cur_slot = Some(slot);
                    } else if ln == "srgbClr" {
                        if let (Some(slot), Some(rgb)) = (cur_slot, attr(&attrs, "val")) {
                            if let Some(c) = hex6_to_rgb(rgb) {
                                named.insert(slot, c);
                            }
                        }
                    } else if ln == "sysClr" {
                        // System colours carry a resolved `lastClr` (e.g. window text).
                        if let (Some(slot), Some(rgb)) = (cur_slot, attr(&attrs, "lastClr")) {
                            if let Some(c) = hex6_to_rgb(rgb) {
                                named.insert(slot, c);
                            }
                        }
                    }
                }
            }
            Tok::Close(name) => {
                let ln = local(&name);
                if ln == "clrScheme" {
                    in_scheme = false;
                } else if cur_slot == slot_name(ln) {
                    cur_slot = None;
                }
            }
            Tok::Text(_) => {}
        }
    }

    // Spreadsheet @theme index order: lt1, dk1, lt2, dk2, accent1..6, hlink, fol.
    let order = [
        "lt1", "dk1", "lt2", "dk2", "accent1", "accent2", "accent3", "accent4", "accent5",
        "accent6", "hlink", "folHlink",
    ];
    let colors = order
        .iter()
        .map(|n| named.get(n).copied().unwrap_or([0, 0, 0]))
        .collect();
    XlsxTheme { colors }
}

/// Parse a 6-hex-digit colour to `[r,g,b]`, ignoring a leading `#`.
fn hex6_to_rgb(s: &str) -> Option<[u8; 3]> {
    let s = s.trim().strip_prefix('#').unwrap_or(s.trim());
    if s.len() != 6 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let p = |i: usize| u8::from_str_radix(&s[i..i + 2], 16).ok();
    Some([p(0)?, p(2)?, p(4)?])
}

// ─────────────────────── XLSX number formats (numFmt) ──────────────────────────

/// Resolve a `numFmtId` to its format code: the custom `<numFmts>` map first,
/// then the built-in ids we care about. Returns `None` for `0` (General) and
/// unknown ids (the raw value is then shown as-is).
fn num_fmt_code(id: u32, custom: &BTreeMap<u32, String>) -> Option<String> {
    if let Some(code) = custom.get(&id) {
        return Some(code.clone());
    }
    // Built-in formats (ECMA-376 §18.8.30). Only the common numeric/date/currency
    // ids are mapped; the rest fall through to General.
    let code = match id {
        1 => "0",
        2 => "0.00",
        3 => "#,##0",
        4 => "#,##0.00",
        9 => "0%",
        10 => "0.00%",
        11 => "0.00E+00",
        14 => "mm-dd-yy",
        15 => "d-mmm-yy",
        16 => "d-mmm",
        17 => "mmm-yy",
        18 => "h:mm AM/PM",
        19 => "h:mm:ss AM/PM",
        20 => "h:mm",
        21 => "h:mm:ss",
        22 => "m/d/yy h:mm",
        37 | 38 => "#,##0",
        39 | 40 => "#,##0.00",
        // Currency / accounting.
        5..=8 => "$#,##0.00",
        44 | 42 | 41 => "$#,##0.00",
        45 => "mm:ss",
        46 => "[h]:mm:ss",
        47 => "mmss.0",
        48 => "##0.0E+0",
        49 => "@",
        _ => return None,
    };
    Some(code.to_string())
}

/// Apply a resolved number format to a raw numeric cell value, returning the
/// display text — or `None` to fall back to the raw value (text formats, parse
/// failures, formats we don't render). Recognises the kind of format from its
/// code: a date/time code formats the Excel serial as a date; a `%` code scales
/// and appends `%`; a `$`/currency code prefixes `$` with grouping; otherwise a
/// plain grouped/decimal number.
fn format_cell_number(raw: &str, code: &str) -> Option<String> {
    let v: f64 = raw.trim().parse().ok()?;
    // Strip format sections / colour & locale tokens for classification.
    let lower = code.to_ascii_lowercase();
    let has_date = lower.contains('y')
        || lower.contains('d')
        || (lower.contains('m') && (lower.contains('y') || lower.contains('d')))
        || lower.contains("mmm");
    let has_time = lower.contains('h') || lower.contains("ss") || lower.contains("am/pm");

    if has_date || has_time {
        return Some(format_excel_datetime(v, has_date, has_time));
    }
    if code.contains('%') {
        let pct = v * 100.0;
        let decimals = decimals_in(&lower);
        return Some(format!("{}%", trim_float(pct, decimals)));
    }
    let currency = code.contains('$') || lower.contains("usd");
    let grouped = code.contains("#,##0") || code.contains(',');
    let decimals = decimals_in(&lower);
    let mut s = if grouped {
        group_thousands(v, decimals)
    } else {
        trim_float(v, decimals)
    };
    if currency {
        let neg = s.starts_with('-');
        if neg {
            s.remove(0);
        }
        s = if neg {
            format!("-${s}")
        } else {
            format!("${s}")
        };
    }
    Some(s)
}

/// Count the digits after the decimal point implied by a format code's
/// fractional part (`0.00` → 2, `#,##0` → 0). Caps at 9.
fn decimals_in(code: &str) -> usize {
    // Look at the first format section only.
    let section = code.split(';').next().unwrap_or(code);
    match section.find('.') {
        Some(dot) => section[dot + 1..]
            .chars()
            .take_while(|c| *c == '0' || *c == '#')
            .filter(|c| *c == '0' || *c == '#')
            .count()
            .min(9),
        None => 0,
    }
}

/// Format `v` to `decimals` places, trimming a trailing `.0…` when the value is
/// integral and the format allowed optional decimals.
fn trim_float(v: f64, decimals: usize) -> String {
    if decimals == 0 {
        return format!("{}", v.round() as i64);
    }
    let s = format!("{v:.decimals$}");
    s
}

/// Format `v` with thousands separators and `decimals` fractional digits.
fn group_thousands(v: f64, decimals: usize) -> String {
    let neg = v < 0.0;
    let v = v.abs();
    let int_part = v.trunc() as u64;
    let int_str = {
        let digits = int_part.to_string();
        let bytes = digits.as_bytes();
        let mut grouped = String::new();
        let len = bytes.len();
        for (i, b) in bytes.iter().enumerate() {
            if i > 0 && (len - i).is_multiple_of(3) {
                grouped.push(',');
            }
            grouped.push(*b as char);
        }
        grouped
    };
    let mut out = if neg { format!("-{int_str}") } else { int_str };
    if decimals > 0 {
        let frac = (v.fract() * 10f64.powi(decimals as i32)).round() as u64;
        out.push('.');
        out.push_str(&format!("{frac:0width$}", width = decimals));
    }
    out
}

/// Convert an Excel serial date/time (`v`, days since 1899-12-31 with the well-
/// known 1900 leap-year bug, fractional part = time of day) to a display string.
/// `date`/`time` select which parts to emit.
fn format_excel_datetime(v: f64, date: bool, time: bool) -> String {
    let mut out = String::new();
    if date {
        let serial = v.trunc() as i64;
        if let Some((y, m, d)) = excel_serial_to_ymd(serial) {
            out.push_str(&format!("{y:04}-{m:02}-{d:02}"));
        } else {
            return trim_float(v, 0);
        }
    }
    if time {
        let frac = v.fract();
        let total_secs = (frac * 86400.0).round() as i64;
        let h = (total_secs / 3600) % 24;
        let mi = (total_secs % 3600) / 60;
        let s = total_secs % 60;
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&format!("{h:02}:{mi:02}:{s:02}"));
    }
    out
}

/// Convert an Excel 1900-system serial day number to `(year, month, day)`.
/// Serial 1 = 1900-01-01; serial 60 is Excel's spurious 1900-02-29 (mapped to
/// 1900-02-28). Returns `None` for non-positive serials.
fn excel_serial_to_ymd(serial: i64) -> Option<(i64, u32, u32)> {
    if serial <= 0 {
        return None;
    }
    // Excel day 1 == 1900-01-01. Account for the fictitious 1900-02-29 (day 60):
    // days at/after 60 are shifted back by one to align with the real calendar.
    let days_since_1900_01_01 = if serial >= 60 { serial - 1 } else { serial };
    // Convert to a proleptic-Gregorian date counting from 1900-01-01.
    let mut days = days_since_1900_01_01 - 1; // 0-based offset from 1900-01-01
    let mut year = 1900i64;
    loop {
        let leap = is_leap(year);
        let in_year = if leap { 366 } else { 365 };
        if days < in_year {
            break;
        }
        days -= in_year;
        year += 1;
    }
    let months = [
        31,
        if is_leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 0usize;
    while month < 12 && days >= months[month] {
        days -= months[month];
        month += 1;
    }
    Some((year, month as u32 + 1, days as u32 + 1))
}

/// Gregorian leap-year test.
fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

// ─────────────────────────── XLSX merged cells ─────────────────────────────────

/// A merged region, as 0-based inclusive `(row0, col0, row1, col1)`.
type MergeRange = (usize, usize, usize, usize);

/// Parse a worksheet's `<mergeCells><mergeCell ref="A1:C2"/>…` into 0-based
/// inclusive ranges. Single-cell or malformed refs are dropped.
fn parse_merges(xml: &str) -> Vec<MergeRange> {
    let mut out = Vec::new();
    let mut x = Xml::new(xml);
    while let Some(tok) = x.next() {
        if let Tok::Open(name, attrs, _) = tok {
            if local(&name) == "mergeCell" {
                if let Some(r) = attr(&attrs, "ref").and_then(parse_merge_ref) {
                    out.push(r);
                }
            }
        }
    }
    out
}

/// Parse a merge `ref` like `A1:C2` to 0-based inclusive `(r0,c0,r1,c1)`,
/// normalising the corner order. `None` for a single cell or a malformed ref.
fn parse_merge_ref(s: &str) -> Option<MergeRange> {
    let (a, b) = s.split_once(':')?;
    let (r0, c0) = cell_ref_rc(a.trim())?;
    let (r1, c1) = cell_ref_rc(b.trim())?;
    let (r0, r1) = (r0.min(r1), r0.max(r1));
    let (c0, c1) = (c0.min(c1), c0.max(c1));
    if r0 == r1 && c0 == c1 {
        return None;
    }
    Some((r0, c0, r1, c1))
}

/// Split a cell reference (`AB12`) into 0-based `(row, col)`. `None` if it has no
/// alphabetic column or no numeric row.
fn cell_ref_rc(r: &str) -> Option<(usize, usize)> {
    let col_end = r.find(|c: char| c.is_ascii_digit())?;
    if col_end == 0 {
        return None;
    }
    let col = col_of_ref(&r[..col_end]);
    let row: usize = r[col_end..].trim().parse().ok()?;
    if row == 0 {
        return None;
    }
    Some((row - 1, col))
}

/// Per-cell merge resolution built from the range list: which `(row,col)` cells
/// are merge anchors (carrying span dimensions) and which are covered.
#[derive(Default)]
struct MergeMap {
    /// (row, col) → (colspan, rowspan) for anchor (top-left) cells.
    anchors: BTreeMap<(usize, usize), (usize, usize)>,
    /// (row, col) covered by some merge but not the anchor — suppressed.
    covered: std::collections::BTreeSet<(usize, usize)>,
}

impl MergeMap {
    fn build(ranges: &[MergeRange]) -> MergeMap {
        let mut m = MergeMap::default();
        for &(r0, c0, r1, c1) in ranges {
            let colspan = c1 - c0 + 1;
            let rowspan = r1 - r0 + 1;
            m.anchors.insert((r0, c0), (colspan, rowspan));
            for r in r0..=r1 {
                for c in c0..=c1 {
                    if (r, c) != (r0, c0) {
                        m.covered.insert((r, c));
                    }
                }
            }
        }
        m
    }

    /// `(colspan, rowspan)` if `(row, col)` is a merge anchor (top-left cell).
    fn anchor(&self, row: usize, col: usize) -> Option<(usize, usize)> {
        self.anchors.get(&(row, col)).copied()
    }

    /// Whether `(row, col)` is covered by a merge but is not its anchor, and so
    /// must be omitted from the rendered row.
    fn is_covered(&self, row: usize, col: usize) -> bool {
        self.covered.contains(&(row, col))
    }
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
    let geom = pptx_page_geom(&part(zip, "ppt/presentation.xml"));
    // The deck's font scheme (`+mn-lt`/`+mj-lt`) from the first theme part.
    let theme = pptx_theme(zip);
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
        pptx_slide(xml, zip, &rels, &theme, &mut body);
    }
    if slides.is_empty() {
        body.push_str("<p></p>");
    }
    render_geom(&body, geom)
}

/// The deck's resolved typefaces for the OOXML theme-font placeholders that text
/// runs reference with `a:latin typeface="+mn-lt"` (minor / body) and `"+mj-lt"`
/// (major / heading). Read from the theme's `a:fontScheme`.
#[derive(Default, Clone)]
struct PptxTheme {
    minor_latin: Option<String>,
    major_latin: Option<String>,
}

impl PptxTheme {
    /// Resolve an `a:latin@typeface` value to a concrete family: the theme font
    /// for `+mn-lt`/`+mj-lt` (and the `+mn-cs`/`+mj-cs` complex-script aliases,
    /// mapped to the same latin face), else the literal name as given.
    fn resolve(&self, typeface: &str) -> Option<String> {
        match typeface {
            "+mn-lt" | "+mn-cs" | "+mn-ea" => self.minor_latin.clone(),
            "+mj-lt" | "+mj-cs" | "+mj-ea" => self.major_latin.clone(),
            t if t.starts_with('+') => None, // unknown placeholder
            t if t.trim().is_empty() => None,
            t => Some(t.to_string()),
        }
    }
}

/// Build a [`PptxTheme`] from the first `ppt/theme/theme*.xml` part: read the
/// `a:fontScheme`'s `a:minorFont/a:latin@typeface` and `a:majorFont/a:latin`.
fn pptx_theme(zip: &BTreeMap<String, Vec<u8>>) -> PptxTheme {
    let key = zip
        .keys()
        .filter(|k| k.starts_with("ppt/theme/theme") && k.ends_with(".xml"))
        .min();
    let Some(key) = key else {
        return PptxTheme::default();
    };
    parse_pptx_theme(&String::from_utf8_lossy(&zip[key]))
}

/// Parse a theme XML's `a:fontScheme`: the first `a:latin@typeface` inside
/// `a:majorFont` → major family, inside `a:minorFont` → minor family.
fn parse_pptx_theme(xml: &str) -> PptxTheme {
    let mut theme = PptxTheme::default();
    let mut x = Xml::new(xml);
    let mut in_major = false;
    let mut in_minor = false;
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, _) => match local(&name) {
                "majorFont" => {
                    in_major = true;
                    in_minor = false;
                }
                "minorFont" => {
                    in_minor = true;
                    in_major = false;
                }
                "latin" => {
                    let face = attr(&attrs, "typeface").filter(|v| !v.trim().is_empty());
                    if let Some(face) = face {
                        if in_major && theme.major_latin.is_none() {
                            theme.major_latin = Some(face.to_string());
                        } else if in_minor && theme.minor_latin.is_none() {
                            theme.minor_latin = Some(face.to_string());
                        }
                    }
                }
                _ => {}
            },
            Tok::Close(name) => match local(&name) {
                "majorFont" => in_major = false,
                "minorFont" => in_minor = false,
                _ => {}
            },
            Tok::Text(_) => {}
        }
    }
    theme
}

/// Read the slide size from `p:presentation/p:sldSz@cx/@cy` (EMUs,
/// `pt = emu / 12700`); margins are zero (slides bleed to the edges). Falls back
/// to a 16:9 slide when absent.
fn pptx_page_geom(presentation_xml: &str) -> PageGeom {
    let mut geom = PageGeom::slide_default();
    let mut x = Xml::new(presentation_xml);
    while let Some(tok) = x.next() {
        if let Tok::Open(name, attrs, _) = tok {
            if local(&name) == "sldSz" {
                let cx = attr(&attrs, "cx").and_then(emu_to_pt);
                let cy = attr(&attrs, "cy").and_then(emu_to_pt);
                if let (Some(w), Some(h)) = (cx, cy) {
                    geom.w = clamp_page_dim(w);
                    geom.h = clamp_page_dim(h);
                    geom.margins = Margins::uniform(0.0);
                }
                break;
            }
        }
    }
    geom
}

/// EMU (`1/914400` inch) attribute string → points (`emu / 12700`).
fn emu_to_pt(v: &str) -> Option<f64> {
    v.trim().parse::<f64>().ok().map(|e| e / EMU_PER_PT)
}

/// Emit one slide's text paragraphs, tables and images into `out`. Theme fonts
/// (`+mn-lt`/`+mj-lt`) are resolved through `theme`; `a:tbl` renders as a real
/// HTML `<table>` with a `<colgroup>` from `a:tblGrid`.
fn pptx_slide(
    xml: &str,
    zip: &BTreeMap<String, Vec<u8>>,
    rels: &BTreeMap<String, String>,
    theme: &PptxTheme,
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
                // A graphic-frame table: hand the whole subtree to pptx_table.
                "tbl" if !sc => {
                    if !para.trim().is_empty() {
                        out.push_str(&format!("<p>{}</p>", para.trim()));
                        para.clear();
                    }
                    pptx_table(&mut x, theme, out);
                }
                "p" if !sc => {
                    in_para = true;
                    para.clear();
                }
                "rPr" if !sc => {
                    in_rpr = true;
                    run = pptx_run_props(&attrs);
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
                "latin" if in_rpr => {
                    run.font_family = attr(&attrs, "typeface").and_then(|t| theme.resolve(t));
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
                    push_run_text(&run, &t, &mut para);
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

/// Read a PPTX `a:rPr` open-tag's run attributes (`b`/`i`/`sz`) into a
/// [`RunStyle`]. Colour and typeface arrive as child elements, set by the caller.
fn pptx_run_props(attrs: &[(String, String)]) -> RunStyle {
    RunStyle {
        bold: matches!(attr(attrs, "b"), Some("1")),
        italic: matches!(attr(attrs, "i"), Some("1")),
        size_half_pt: attr(attrs, "sz")
            .and_then(|v| v.parse::<f64>().ok())
            .map(|sz| sz / 50.0), // hundredths-pt → half-pt
        ..RunStyle::default()
    }
}

/// Append a text run to `out`, wrapped in the run's `<span style>` when it
/// carries any formatting (shared by the slide body and table cells).
fn push_run_text(run: &RunStyle, text: &str, out: &mut String) {
    let span = run.open_span();
    if span.is_empty() {
        esc(text, out);
    } else {
        out.push_str(&span);
        esc(text, out);
        out.push_str("</span>");
    }
}

/// Emit a PPTX `a:tbl` (open already consumed) as an HTML `<table>`. The
/// `a:tblGrid/a:gridCol@w` widths (EMU→pt) seed a leading `<colgroup>` so the
/// layout honours real column widths; `a:tc@gridSpan`/`@rowSpan` map to
/// `colspan`/`rowspan` and the `a:tc@hMerge`/`@vMerge` continuation cells are
/// dropped. Cell borders come from the default table CSS.
fn pptx_table(x: &mut Xml, theme: &PptxTheme, out: &mut String) {
    out.push_str("<table>");
    let mut col_pts: Vec<f64> = Vec::new();
    let mut colgroup_done = false;
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => {
                let ln = local(&name);
                if ln == "gridCol" {
                    if let Some(w) = attr(&attrs, "w").and_then(emu_to_pt) {
                        if w > 0.0 {
                            col_pts.push(w);
                        }
                    }
                } else if ln == "tr" && !sc {
                    flush_colgroup(&mut col_pts, &mut colgroup_done, out);
                    out.push_str("<tr>");
                } else if ln == "tc" && !sc {
                    pptx_table_cell(x, theme, &attrs, out);
                }
            }
            Tok::Close(name) => {
                let ln = local(&name);
                if ln == "tr" {
                    out.push_str("</tr>");
                } else if ln == "tbl" {
                    break;
                }
            }
            Tok::Text(_) => {}
        }
    }
    out.push_str("</table>");
}

/// Emit one PPTX `a:tc` cell (open already consumed, its attrs in `cell_attrs`)
/// as a `<td>`. `gridSpan`/`rowSpan` become `colspan`/`rowspan`; a horizontal-
/// merge continuation (`hMerge`) is suppressed (covered by the span to its left);
/// a vertical-merge continuation (`vMerge`) emits an empty placeholder `<td>` so
/// the row keeps its column count. Cell text reuses the slide paragraph grammar.
fn pptx_table_cell(
    x: &mut Xml,
    theme: &PptxTheme,
    cell_attrs: &[(String, String)],
    out: &mut String,
) {
    let grid_span = attr(cell_attrs, "gridSpan")
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(1)
        .max(1);
    let row_span = attr(cell_attrs, "rowSpan")
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(1)
        .max(1);
    let h_merge = matches!(attr(cell_attrs, "hMerge"), Some("1") | Some("true"));
    let v_merge = matches!(attr(cell_attrs, "vMerge"), Some("1") | Some("true"));

    // Collect the cell's text (paragraphs joined by <br>) regardless, so the
    // walker stays in sync; merged-continuation cells just discard it.
    let mut body = String::new();
    let mut para = String::new();
    let mut in_para = false;
    let mut run = RunStyle::default();
    let mut in_rpr = false;
    let mut in_text = false;
    let mut depth = 0i32; // <a:tc> nesting guard (tables don't nest in practice)

    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => match local(&name) {
                "tc" if !sc => depth += 1,
                "p" if !sc => {
                    in_para = true;
                    para.clear();
                }
                "rPr" if !sc => {
                    in_rpr = true;
                    run = pptx_run_props(&attrs);
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
                "latin" if in_rpr => {
                    run.font_family = attr(&attrs, "typeface").and_then(|t| theme.resolve(t));
                }
                "t" if !sc => in_text = true,
                "br" => para.push_str("<br>"),
                _ => {}
            },
            Tok::Text(t) => {
                if in_para && in_text && !t.is_empty() {
                    push_run_text(&run, &t, &mut para);
                }
            }
            Tok::Close(name) => match local(&name) {
                "t" => in_text = false,
                "rPr" => in_rpr = false,
                "p" => {
                    if in_para && !para.trim().is_empty() {
                        if !body.is_empty() {
                            body.push_str("<br>");
                        }
                        body.push_str(para.trim());
                    }
                    in_para = false;
                }
                "tc" => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
                _ => {}
            },
        }
    }

    // A horizontal-merge continuation is covered by the spanning cell to its
    // left — drop it entirely.
    if h_merge {
        return;
    }
    // A vertical-merge continuation keeps the column count but carries no content.
    if v_merge {
        out.push_str("<td></td>");
        return;
    }

    let colspan_attr = if grid_span > 1 {
        format!(" colspan=\"{grid_span}\"")
    } else {
        String::new()
    };
    let rowspan_attr = if row_span > 1 {
        format!(" rowspan=\"{row_span}\"")
    } else {
        String::new()
    };
    out.push_str(&format!(
        "<td{colspan_attr}{rowspan_attr}>{}</td>",
        body.trim()
    ));
    // Pad the row to `grid_span` physical columns (like the DOCX gridSpan path)
    // so the equal/colgroup layout advances the right number of columns.
    for _ in 1..grid_span {
        out.push_str("<td></td>");
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
                        // `fo:font-name` (or `style:font-name`) → real family so
                        // the host embeds the matching face and uses its metrics.
                        if let Some(fam) = attr(&attrs, "font-name") {
                            let family = css_font_family(fam);
                            if !family.is_empty() {
                                css.push_str(&format!("font-family:{family};"));
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

/// Read the first `style:page-layout-properties` from an ODF part —
/// `fo:page-width`/`fo:page-height` and `fo:margin-*` (with `fo:margin`
/// shorthand) — into a [`PageGeom`], using `fallback` for anything absent. ODF
/// lengths (`21cm`, `2.54cm`, …) are parsed via [`parse_odf_pt`].
fn odf_page_geom(xml: &str, fallback: PageGeom) -> PageGeom {
    let mut geom = fallback;
    let mut x = Xml::new(xml);
    while let Some(tok) = x.next() {
        if let Tok::Open(name, attrs, _) = tok {
            if local(&name) == "page-layout-properties" {
                if let Some(w) = attr(&attrs, "page-width").and_then(parse_odf_pt) {
                    geom.w = clamp_page_dim(w);
                }
                if let Some(h) = attr(&attrs, "page-height").and_then(parse_odf_pt) {
                    geom.h = clamp_page_dim(h);
                }
                if let Some(all) = attr(&attrs, "margin").and_then(parse_odf_pt) {
                    geom.margins = Margins::uniform(all.max(0.0));
                }
                if let Some(v) = attr(&attrs, "margin-top").and_then(parse_odf_pt) {
                    geom.margins.top = v.max(0.0);
                }
                if let Some(v) = attr(&attrs, "margin-right").and_then(parse_odf_pt) {
                    geom.margins.right = v.max(0.0);
                }
                if let Some(v) = attr(&attrs, "margin-bottom").and_then(parse_odf_pt) {
                    geom.margins.bottom = v.max(0.0);
                }
                if let Some(v) = attr(&attrs, "margin-left").and_then(parse_odf_pt) {
                    geom.margins.left = v.max(0.0);
                }
                // First page layout is the document default — stop here.
                break;
            }
        }
    }
    geom
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

/// Build a `column-style-name → width(pt)` map from an ODF part. Reads each
/// `style:style`'s `style:table-column-properties/@style:column-width` (ODF
/// lengths via [`parse_odf_pt`]). Malformed/absent widths are simply omitted.
fn odf_column_widths(xml: &str) -> BTreeMap<String, f64> {
    let mut map = BTreeMap::new();
    let mut x = Xml::new(xml);
    let mut cur_name: Option<String> = None;
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, _) => match local(&name) {
                "style" => cur_name = attr(&attrs, "name").map(|s| s.to_string()),
                "table-column-properties" => {
                    if let Some(nm) = &cur_name {
                        if let Some(w) = attr(&attrs, "column-width")
                            .and_then(parse_odf_pt)
                            .filter(|w| *w > 0.0)
                        {
                            map.insert(nm.clone(), w);
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

/// Handle a `table:table-column` token inside an ODF table: append `<col>`
/// entries (honouring `table:number-columns-repeated`, cap 64) carrying the
/// resolved width (when the column style declares one) into `pending`.
fn odf_push_column(attrs: &[(String, String)], cols: &BTreeMap<String, f64>, pending: &mut String) {
    let repeat = attr(attrs, "number-columns-repeated")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(1)
        .min(64);
    let width = attr(attrs, "style-name").and_then(|s| cols.get(s).copied());
    for _ in 0..repeat {
        match width {
            Some(w) => pending.push_str(&format!("<col style=\"width:{}pt\">", fmt_pt(w))),
            None => pending.push_str("<col>"),
        }
    }
}

/// Wrap accumulated `<col>` entries in a `<colgroup>` (once, before the first
/// row). Emits nothing when no column carried a width (`<col>`-only padding is
/// pointless for equal columns).
fn flush_odf_colgroup(pending: &mut String, done: &mut bool, out: &mut String) {
    if *done {
        return;
    }
    *done = true;
    if pending.is_empty() || !pending.contains("width:") {
        pending.clear();
        return;
    }
    out.push_str("<colgroup>");
    out.push_str(pending);
    out.push_str("</colgroup>");
    pending.clear();
}

/// ODT → styled HTML → PDF. `text:h`→`<hN>`, `text:p`→`<p>`, `text:span`
/// styled via the automatic/named style map, `table:table`→`<table>`,
/// `draw:image xlink:href`→`<img>`.
pub fn odt_to_pdf(zip: &BTreeMap<String, Vec<u8>>) -> Vec<u8> {
    let content = part(zip, "content.xml");
    let styles_xml = part(zip, "styles.xml");
    let mut styles = odf_text_styles(&styles_xml);
    // Automatic styles in content.xml take precedence / add to the named ones.
    styles.extend(odf_text_styles(&content));
    let mut cols = odf_column_widths(&styles_xml);
    cols.extend(odf_column_widths(&content));

    let geom = odf_geom(&styles_xml, &content, PageGeom::prose_default());
    let mut body = String::new();
    odf_walk(
        &mut Xml::new(&content),
        zip,
        &styles,
        &cols,
        &mut body,
        None,
        None,
    );
    render_geom(&body, geom)
}

/// Resolve ODF page geometry: the page-layout in `styles.xml` is authoritative
/// for prose; an automatic page-layout in `content.xml` (presentations/sheets
/// often put it there) overrides. `fallback` covers a part with neither.
fn odf_geom(styles_xml: &str, content_xml: &str, fallback: PageGeom) -> PageGeom {
    let from_styles = odf_page_geom(styles_xml, fallback);
    odf_page_geom(content_xml, from_styles)
}

/// Recursive ODF text walker (shared by ODT body and table cells). `stop` ends
/// the region. Handles `text:h`, `text:p`, `table:table`, and `text:list`
/// (nested lists indent and bullet their paragraphs). `list_level` is `Some(n)`
/// when walking inside a list (`n` = nesting depth, 0-based).
fn odf_walk(
    x: &mut Xml,
    zip: &BTreeMap<String, Vec<u8>>,
    styles: &BTreeMap<String, String>,
    cols: &BTreeMap<String, f64>,
    out: &mut String,
    stop: Option<&str>,
    list_level: Option<u32>,
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
                        match list_level {
                            // A paragraph inside a list item → bullet + indent.
                            Some(lvl) if !inner.trim().is_empty() => {
                                let indent = (lvl as f64 + 1.0) * LIST_LEVEL_INDENT_PT;
                                out.push_str(&format!(
                                    "<p style=\"margin-left:{indent}pt\">\u{2022} {inner}</p>"
                                ));
                            }
                            _ => out.push_str(&format!("<p>{inner}</p>")),
                        }
                    }
                    // text:list nests; descend with a deeper level. text:list-item
                    // is transparent — its paragraphs are handled at this level.
                    "list" if !sc => {
                        let next = Some(list_level.map(|l| l + 1).unwrap_or(0));
                        odf_walk(x, zip, styles, cols, out, Some("list"), next);
                    }
                    "table" if !sc => odf_table(x, zip, styles, cols, out),
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
                        let n = attr(&attrs, "c")
                            .and_then(|v| v.parse::<usize>().ok())
                            .unwrap_or(1);
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

/// Emit one `table:table` (open already consumed) as an HTML `<table>`. Reads
/// the `table:table-column` declarations into a leading `<colgroup>` so the
/// layout honours each column style's `style:column-width`.
fn odf_table(
    x: &mut Xml,
    zip: &BTreeMap<String, Vec<u8>>,
    styles: &BTreeMap<String, String>,
    cols: &BTreeMap<String, f64>,
    out: &mut String,
) {
    out.push_str("<table>");
    let mut pending_cols = String::new();
    let mut colgroup_done = false;
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => {
                let ln = local(&name);
                if ln == "table-column" {
                    odf_push_column(&attrs, cols, &mut pending_cols);
                } else if ln == "table-row" && !sc {
                    flush_odf_colgroup(&mut pending_cols, &mut colgroup_done, out);
                    out.push_str("<tr>");
                } else if ln == "table-cell" && !sc {
                    let repeat = attr(&attrs, "number-columns-repeated")
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(1)
                        .min(64);
                    let mut cell = String::new();
                    odf_walk(x, zip, styles, cols, &mut cell, Some("table-cell"), None);
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
    let styles_xml = part(zip, "styles.xml");
    let geom = odf_geom(&styles_xml, &content, PageGeom::tabular_default());
    let mut cols = odf_column_widths(&styles_xml);
    cols.extend(odf_column_widths(&content));
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
                ods_table(&mut x, &cols, &mut body);
            }
        }
    }
    if first {
        body.push_str("<p></p>");
    }
    render_geom(&body, geom)
}

/// Emit one ODS `table:table` (open consumed) as an HTML `<table>`, expanding
/// repeated rows/columns (cap 64) and reading cell text from `text:p` runs.
/// `table:table-column` declarations seed a leading `<colgroup>`.
fn ods_table(x: &mut Xml, cols: &BTreeMap<String, f64>, out: &mut String) {
    out.push_str("<table>");
    let mut pending_cols = String::new();
    let mut colgroup_done = false;
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => {
                let ln = local(&name);
                if ln == "table-column" {
                    odf_push_column(&attrs, cols, &mut pending_cols);
                } else if ln == "table-row" && !sc {
                    flush_odf_colgroup(&mut pending_cols, &mut colgroup_done, out);
                    let rep = attr(&attrs, "number-rows-repeated")
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(1)
                        .min(64);
                    let row = ods_row(x);
                    // Skip emitting many identical *empty* trailing rows.
                    let emit = if row.trim().is_empty() {
                        rep.min(1)
                    } else {
                        rep
                    };
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
                    let emit = if text.trim().is_empty() {
                        rep.min(1)
                    } else {
                        rep
                    };
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
    let styles_xml = part(zip, "styles.xml");
    let mut styles = odf_text_styles(&styles_xml);
    styles.extend(odf_text_styles(&content));
    let geom = odf_geom(&styles_xml, &content, PageGeom::slide_default());
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
    render_geom(&body, geom)
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
    // Legacy binaries carry no recoverable page geometry — prose A4 default.
    Some(render_geom(&body, PageGeom::prose_default()))
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
                    let c =
                        u16::from_le_bytes([dir_bytes[off + k * 2], dir_bytes[off + k * 2 + 1]]);
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

/// Normalize a typeface name into a safe CSS `font-family` token: drop the
/// characters that would break an inline `style="…"` declaration (`;:"<>`),
/// collapse internal whitespace, and single-quote names that contain a space
/// (e.g. `Times New Roman` → `'Times New Roman'`). Returns an empty string for
/// names that reduce to nothing (caller then omits the declaration).
fn css_font_family(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| !matches!(c, ';' | ':' | '"' | '\'' | '<' | '>' | '{' | '}') && !c.is_control())
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        String::new()
    } else if collapsed.contains(' ') {
        format!("'{collapsed}'")
    } else {
        collapsed
    }
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
        assert_eq!(x.next(), Some(Tok::Open("w:p".into(), vec![], false)));
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

    fn build_docx(
        document_xml: &str,
        rels_xml: Option<&str>,
        media: &[(&str, Vec<u8>)],
    ) -> Vec<u8> {
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
        z.add_stored(
            "mimetype",
            b"application/vnd.oasis.opendocument.spreadsheet",
        );
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
        z.add_stored(
            "mimetype",
            b"application/vnd.oasis.opendocument.presentation",
        );
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
        let put16 =
            |o: &mut [u8], at: usize, v: u16| o[at..at + 2].copy_from_slice(&v.to_le_bytes());
        let put32 =
            |o: &mut [u8], at: usize, v: u32| o[at..at + 4].copy_from_slice(&v.to_le_bytes());
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

    // ── page geometry & font-family ──

    /// Build the DOCX HTML body the renderer receives (bypasses PDF rendering so
    /// we can assert on the generated markup directly).
    fn docx_html(document_xml: &str) -> String {
        docx_html_with(document_xml, "", "")
    }

    /// Build the DOCX body HTML with optional `styles.xml` / `numbering.xml`
    /// payloads so style-inheritance and list-numbering tests can exercise them.
    fn docx_html_with(document_xml: &str, styles_xml: &str, numbering_xml: &str) -> String {
        let zip = BTreeMap::new();
        let rels = BTreeMap::new();
        let styles = parse_docx_styles(styles_xml);
        let numbering = parse_docx_numbering(numbering_xml);
        let footnotes = DocxFootnotes::default();
        let ctx = DocxCtx {
            zip: &zip,
            rels: &rels,
            styles: &styles,
            numbering: &numbering,
            footnotes: &footnotes,
        };
        let mut body = String::new();
        docx_body(document_xml, &ctx, &mut body);
        body
    }

    #[test]
    fn unit_conversions_twips_and_emu() {
        // 1 inch = 1440 twips = 72pt; A4 width 11906 twips ≈ 595.3pt.
        assert!((twips_to_pt("1440").unwrap() - 72.0).abs() < 1e-9);
        assert!((twips_to_pt("11906").unwrap() - 595.3).abs() < 0.1);
        assert!(twips_to_pt("nope").is_none());
        // 1 inch = 914400 EMU = 72pt; PowerPoint 16:9 width 9144000 EMU = 720pt.
        assert!((emu_to_pt("914400").unwrap() - 72.0).abs() < 1e-9);
        assert!((emu_to_pt("9144000").unwrap() - 720.0).abs() < 1e-9);
        assert!(emu_to_pt("x").is_none());
    }

    #[test]
    fn css_font_family_quotes_and_sanitizes() {
        assert_eq!(css_font_family("Arial"), "Arial");
        assert_eq!(css_font_family("Times New Roman"), "'Times New Roman'");
        // Stray delimiters that would break an inline style are dropped.
        assert_eq!(css_font_family("Ev;il\"<x>"), "Evilx");
        assert_eq!(css_font_family("  "), "");
    }

    #[test]
    fn docx_a4_page_size_from_sectpr() {
        // A4 portrait: 11906 × 16838 twips, 1440-twip (1in) margins.
        let doc = r#"<w:document xmlns:w="x"><w:body>
            <w:p><w:r><w:t>Body</w:t></w:r></w:p>
            <w:sectPr>
              <w:pgSz w:w="11906" w:h="16838"/>
              <w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440"/>
            </w:sectPr>
          </w:body></w:document>"#;
        // Geometry parser resolves ~A4 (595.3 × 841.9).
        let geom = docx_page_geom(doc);
        assert!((geom.w - 595.3).abs() < 0.5, "w = {}", geom.w);
        assert!((geom.h - 841.9).abs() < 0.5, "h = {}", geom.h);
        assert!((geom.margins.top - 72.0).abs() < 0.5);
        // …and the rendered PDF's first page carries that media box.
        let pdf = office_to_pdf(&build_docx(doc, None, &[])).expect("docx converts");
        let (w, h, _rot) = opens(&pdf).page_info(1).expect("page size");
        assert!(
            (w - 595.0).abs() < 2.0 && (h - 842.0).abs() < 2.0,
            "{w}x{h}"
        );
    }

    #[test]
    fn docx_landscape_orientation_swaps_axes() {
        // orient="landscape" with w<h means the axes are swapped at render time.
        let doc = r#"<w:document xmlns:w="x"><w:body>
            <w:sectPr><w:pgSz w:w="11906" w:h="16838" w:orient="landscape"/></w:sectPr>
          </w:body></w:document>"#;
        let geom = docx_page_geom(doc);
        assert!(geom.w > geom.h, "landscape is wider than tall: {geom:?}");
    }

    #[test]
    fn docx_missing_sectpr_falls_back_to_a4() {
        let geom = docx_page_geom(r#"<w:document xmlns:w="x"><w:body/></w:document>"#);
        assert!((geom.w - A4_W).abs() < 0.01 && (geom.h - A4_H).abs() < 0.01);
    }

    #[test]
    fn docx_run_font_injected_as_font_family() {
        let doc = r#"<w:document xmlns:w="x"><w:body>
            <w:p><w:r><w:rPr><w:rFonts w:ascii="Arial"/></w:rPr><w:t>Hello</w:t></w:r></w:p>
          </w:body></w:document>"#;
        let html = docx_html(doc);
        assert!(html.contains("font-family:Arial"), "html was: {html}");
    }

    #[test]
    fn docx_paragraph_alignment_spacing_indent() {
        let doc = r#"<w:document xmlns:w="x"><w:body>
            <w:p>
              <w:pPr>
                <w:jc w:val="center"/>
                <w:spacing w:before="240" w:after="120"/>
                <w:ind w:left="720" w:firstLine="360"/>
              </w:pPr>
              <w:r><w:t>Centered</w:t></w:r>
            </w:p>
          </w:body></w:document>"#;
        let html = docx_html(doc);
        assert!(html.contains("text-align:center"), "html: {html}");
        assert!(
            html.contains("margin-top:12pt"),
            "before 240twip=12pt: {html}"
        );
        assert!(
            html.contains("margin-bottom:6pt"),
            "after 120twip=6pt: {html}"
        );
        assert!(
            html.contains("margin-left:36pt"),
            "ind left 720twip=36pt: {html}"
        );
        assert!(
            html.contains("text-indent:18pt"),
            "firstLine 360twip=18pt: {html}"
        );
    }

    #[test]
    fn docx_jc_right_and_both_map_to_css() {
        let right = docx_html(
            r#"<w:document xmlns:w="x"><w:body><w:p><w:pPr><w:jc w:val="right"/></w:pPr><w:r><w:t>R</w:t></w:r></w:p></w:body></w:document>"#,
        );
        assert!(right.contains("text-align:right"), "{right}");
        let just = docx_html(
            r#"<w:document xmlns:w="x"><w:body><w:p><w:pPr><w:jc w:val="both"/></w:pPr><w:r><w:t>J</w:t></w:r></w:p></w:body></w:document>"#,
        );
        assert!(just.contains("text-align:justify"), "{just}");
    }

    #[test]
    fn pptx_slide_size_from_presentation() {
        // 16:9 = 9144000 × 5143500 EMU → 720 × 405 pt, zero margins.
        let geom = pptx_page_geom(
            r#"<p:presentation xmlns:p="x"><p:sldSz cx="9144000" cy="5143500"/></p:presentation>"#,
        );
        assert!((geom.w - 720.0).abs() < 0.01, "w = {}", geom.w);
        assert!((geom.h - 405.0).abs() < 0.01, "h = {}", geom.h);
        assert_eq!(geom.margins.left, 0.0);
        // Absent → 16:9 fallback.
        let fb = pptx_page_geom("<p:presentation/>");
        assert!((fb.w - SLIDE_W).abs() < 0.01 && (fb.h - SLIDE_H).abs() < 0.01);
    }

    #[test]
    fn pptx_latin_typeface_injected_and_theme_skipped() {
        // A real typeface flows into the generated slide HTML as font-family.
        let mut body = String::new();
        pptx_slide(
            r#"<p:sld xmlns:a="y"><a:p><a:r><a:rPr><a:latin typeface="Calibri"/></a:rPr><a:t>Hi</a:t></a:r></a:p></p:sld>"#,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &PptxTheme::default(),
            &mut body,
        );
        assert!(body.contains("font-family:Calibri"), "body: {body}");

        // Theme placeholders (`+mn-lt`) with no theme resolve to nothing — skipped.
        let mut themed = String::new();
        pptx_slide(
            r#"<p:sld xmlns:a="y"><a:p><a:r><a:rPr><a:latin typeface="+mn-lt"/></a:rPr><a:t>Hi</a:t></a:r></a:p></p:sld>"#,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &PptxTheme::default(),
            &mut themed,
        );
        assert!(!themed.contains("font-family"), "themed: {themed}");
    }

    #[test]
    fn pptx_theme_font_resolves_placeholder() {
        // With a theme, `+mn-lt`/`+mj-lt` resolve to the scheme's real faces.
        let theme = parse_pptx_theme(
            r#"<a:theme xmlns:a="x"><a:themeElements><a:fontScheme>
                 <a:majorFont><a:latin typeface="Georgia"/></a:majorFont>
                 <a:minorFont><a:latin typeface="Verdana"/></a:minorFont>
               </a:fontScheme></a:themeElements></a:theme>"#,
        );
        assert_eq!(theme.minor_latin.as_deref(), Some("Verdana"));
        assert_eq!(theme.major_latin.as_deref(), Some("Georgia"));

        let mut body = String::new();
        pptx_slide(
            r#"<p:sld xmlns:a="y"><a:p><a:r><a:rPr><a:latin typeface="+mn-lt"/></a:rPr><a:t>Body</a:t></a:r></a:p></p:sld>"#,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &theme,
            &mut body,
        );
        assert!(body.contains("font-family:Verdana"), "body: {body}");
    }

    #[test]
    fn pptx_table_renders_with_colgroup_and_spans() {
        // a:tbl with a 2-col grid (914400 EMU = 72pt each); first row has a
        // 2-column gridSpan header; second row two normal cells.
        let xml = r#"<p:sld xmlns:a="y"><a:graphicFrame><a:graphic><a:graphicData>
          <a:tbl>
            <a:tblGrid>
              <a:gridCol w="914400"/>
              <a:gridCol w="1828800"/>
            </a:tblGrid>
            <a:tr>
              <a:tc gridSpan="2"><a:txBody><a:p><a:r><a:t>Header</a:t></a:r></a:p></a:txBody></a:tc>
              <a:tc hMerge="1"><a:txBody><a:p><a:r><a:t>covered</a:t></a:r></a:p></a:txBody></a:tc>
            </a:tr>
            <a:tr>
              <a:tc><a:txBody><a:p><a:r><a:t>Left</a:t></a:r></a:p></a:txBody></a:tc>
              <a:tc><a:txBody><a:p><a:r><a:t>Right</a:t></a:r></a:p></a:txBody></a:tc>
            </a:tr>
          </a:tbl>
        </a:graphicData></a:graphic></a:graphicFrame></p:sld>"#;
        let mut body = String::new();
        pptx_slide(
            xml,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &PptxTheme::default(),
            &mut body,
        );
        assert!(body.contains("<table>"), "table emitted: {body}");
        assert!(body.contains("<colgroup>"), "colgroup emitted: {body}");
        assert!(body.contains("width:72pt"), "first col 72pt: {body}");
        assert!(body.contains("width:144pt"), "second col 144pt: {body}");
        assert!(body.contains("colspan=\"2\""), "gridSpan→colspan: {body}");
        // hMerge continuation cell content is dropped.
        assert!(!body.contains("covered"), "hMerge cell dropped: {body}");
        for needle in ["Header", "Left", "Right"] {
            assert!(body.contains(needle), "missing {needle:?}: {body}");
        }
        // The whole slide still renders to a valid multi-cell PDF.
        let mut z = ZipWriter::new();
        z.add_stored("[Content_Types].xml", b"<Types/>");
        z.add_stored("ppt/presentation.xml", b"<p:presentation/>");
        z.add_stored("ppt/slides/slide1.xml", xml.as_bytes());
        let pdf = office_to_pdf(&z.finish()).expect("pptx converts");
        let text = norm(&opens(&pdf).to_text());
        for needle in ["Header", "Left", "Right"] {
            assert!(text.contains(needle), "missing {needle:?} in PDF: {text}");
        }
    }

    #[test]
    fn odf_page_geometry_and_font_name() {
        // ODF page layout in styles.xml: 21cm × 29.7cm (A4), 2cm margins.
        let styles = r#"<office:document-styles xmlns:office="o" xmlns:style="s" xmlns:fo="f">
            <office:automatic-styles>
              <style:page-layout style:name="PL1">
                <style:page-layout-properties fo:page-width="21cm" fo:page-height="29.7cm" fo:margin="2cm"/>
              </style:page-layout>
            </office:automatic-styles>
          </office:document-styles>"#;
        let geom = odf_geom(styles, "", PageGeom::prose_default());
        assert!((geom.w - 595.28).abs() < 0.5, "w = {}", geom.w); // 21cm
        assert!((geom.h - 841.89).abs() < 0.5, "h = {}", geom.h); // 29.7cm
        assert!((geom.margins.left - 56.69).abs() < 0.5, "2cm margin"); // 2cm

        // fo:font-name flows into the text-style CSS map.
        let css = odf_text_styles(
            r#"<doc xmlns:style="s" xmlns:fo="f">
              <style:style style:name="T1"><style:text-properties fo:font-name="Liberation Serif"/></style:style>
            </doc>"#,
        );
        assert_eq!(
            css.get("T1").map(String::as_str),
            Some("font-family:'Liberation Serif';")
        );
    }

    // ── P2/P3: line spacing, lists, cell merge, image data URIs, xlsx fills ──

    #[test]
    fn line_spacing_auto_and_exact() {
        // auto: 240ths of a line — 360 → 1.5×.
        assert_eq!(
            line_spacing("360", Some("auto")),
            Some(LineHeight::Multiple(1.5))
        );
        // no rule defaults to auto semantics.
        assert_eq!(line_spacing("240", None), Some(LineHeight::Multiple(1.0)));
        // exact/atLeast: twentieths of a point — 360 → 18pt.
        assert_eq!(
            line_spacing("360", Some("exact")),
            Some(LineHeight::Points(18.0))
        );
        assert_eq!(
            line_spacing("480", Some("atLeast")),
            Some(LineHeight::Points(24.0))
        );
        assert!(line_spacing("0", Some("auto")).is_none());
        assert!(line_spacing("x", None).is_none());
    }

    #[test]
    fn docx_line_height_auto_injected() {
        let doc = r#"<w:document xmlns:w="x"><w:body>
            <w:p><w:pPr><w:spacing w:line="360" w:lineRule="auto"/></w:pPr>
              <w:r><w:t>Spaced</w:t></w:r></w:p>
          </w:body></w:document>"#;
        let html = docx_html(doc);
        assert!(html.contains("line-height:1.5"), "html: {html}");
    }

    #[test]
    fn docx_line_height_exact_in_points() {
        let doc = r#"<w:document xmlns:w="x"><w:body>
            <w:p><w:pPr><w:spacing w:line="480" w:lineRule="exact"/></w:pPr>
              <w:r><w:t>Fixed</w:t></w:r></w:p>
          </w:body></w:document>"#;
        let html = docx_html(doc);
        assert!(html.contains("line-height:24pt"), "html: {html}");
    }

    #[test]
    fn docx_list_bullet_and_indent() {
        // ilvl 0 → 36pt; ilvl 1 → 72pt. Bullet prepended.
        let doc = r#"<w:document xmlns:w="x"><w:body>
            <w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr>
              <w:r><w:t>First</w:t></w:r></w:p>
            <w:p><w:pPr><w:numPr><w:ilvl w:val="1"/><w:numId w:val="1"/></w:numPr></w:pPr>
              <w:r><w:t>Nested</w:t></w:r></w:p>
          </w:body></w:document>"#;
        let html = docx_html(doc);
        assert!(html.contains("\u{2022} First"), "bullet on item: {html}");
        assert!(html.contains("margin-left:36pt"), "ilvl0 indent: {html}");
        assert!(html.contains("margin-left:72pt"), "ilvl1 indent: {html}");
        // The whole document still renders to a valid PDF carrying the text.
        let pdf = office_to_pdf(&build_docx(doc, None, &[])).expect("docx converts");
        let text = norm(&opens(&pdf).to_text());
        assert!(text.contains("First") && text.contains("Nested"), "{text}");
    }

    #[test]
    fn docx_list_indent_stacks_on_explicit_ind() {
        // Explicit w:ind left 18pt + ilvl0 (36pt) → 54pt total.
        let doc = r#"<w:document xmlns:w="x"><w:body>
            <w:p><w:pPr>
              <w:ind w:left="360"/>
              <w:numPr><w:ilvl w:val="0"/></w:numPr>
            </w:pPr><w:r><w:t>Item</w:t></w:r></w:p>
          </w:body></w:document>"#;
        let html = docx_html(doc);
        assert!(html.contains("margin-left:54pt"), "stacked indent: {html}");
    }

    // ── P1: DOCX named-style inheritance, headers/footers, footnotes, fields ──

    #[test]
    fn docx_named_style_run_and_para_props_inherited() {
        // A paragraph that only references a style id (no inline rPr/pPr) inherits
        // the style's bold + colour + centre alignment from styles.xml.
        let styles = r#"<w:styles xmlns:w="x">
          <w:style w:type="paragraph" w:styleId="Fancy">
            <w:pPr><w:jc w:val="center"/></w:pPr>
            <w:rPr><w:b/><w:color w:val="FF0000"/></w:rPr>
          </w:style>
        </w:styles>"#;
        let doc = r#"<w:document xmlns:w="x"><w:body>
          <w:p><w:pPr><w:pStyle w:val="Fancy"/></w:pPr><w:r><w:t>Styled</w:t></w:r></w:p>
        </w:body></w:document>"#;
        let html = docx_html_with(doc, styles, "");
        assert!(html.contains("font-weight:bold"), "style bold: {html}");
        assert!(html.contains("color:#FF0000"), "style colour: {html}");
        assert!(html.contains("text-align:center"), "style align: {html}");
    }

    #[test]
    fn docx_direct_props_override_style() {
        // The style sets size 24 half-pt (12pt); the run overrides to 40 (20pt).
        let styles = r#"<w:styles xmlns:w="x">
          <w:style w:type="paragraph" w:styleId="Body">
            <w:rPr><w:sz w:val="24"/></w:rPr>
          </w:style>
        </w:styles>"#;
        let doc = r#"<w:document xmlns:w="x"><w:body>
          <w:p><w:pPr><w:pStyle w:val="Body"/></w:pPr>
            <w:r><w:rPr><w:sz w:val="40"/></w:rPr><w:t>Big</w:t></w:r></w:p>
        </w:body></w:document>"#;
        let html = docx_html_with(doc, styles, "");
        // The run's own 20pt span wins for the text; the paragraph still carries
        // the inherited 12pt as its block default.
        assert!(html.contains("font-size:20pt"), "run override: {html}");
        assert!(html.contains("font-size:12pt"), "style default: {html}");
    }

    #[test]
    fn docx_basedon_chain_resolves() {
        // Child style based on a bold parent; child adds italics.
        let styles = r#"<w:styles xmlns:w="x">
          <w:style w:type="paragraph" w:styleId="Base">
            <w:rPr><w:b/></w:rPr>
          </w:style>
          <w:style w:type="paragraph" w:styleId="Derived">
            <w:basedOn w:val="Base"/>
            <w:rPr><w:i/></w:rPr>
          </w:style>
        </w:styles>"#;
        let doc = r#"<w:document xmlns:w="x"><w:body>
          <w:p><w:pPr><w:pStyle w:val="Derived"/></w:pPr><w:r><w:t>X</w:t></w:r></w:p>
        </w:body></w:document>"#;
        let html = docx_html_with(doc, styles, "");
        assert!(html.contains("font-weight:bold"), "inherited bold: {html}");
        assert!(html.contains("font-style:italic"), "own italic: {html}");
    }

    #[test]
    fn docx_doc_defaults_apply() {
        // docDefaults set a default font; a plain paragraph inherits it.
        let styles = r#"<w:styles xmlns:w="x">
          <w:docDefaults>
            <w:rPrDefault><w:rPr><w:rFonts w:ascii="Garamond"/></w:rPr></w:rPrDefault>
          </w:docDefaults>
        </w:styles>"#;
        let doc = r#"<w:document xmlns:w="x"><w:body>
          <w:p><w:r><w:t>Plain</w:t></w:r></w:p>
        </w:body></w:document>"#;
        let html = docx_html_with(doc, styles, "");
        assert!(
            html.contains("font-family:Garamond"),
            "doc default font: {html}"
        );
    }

    #[test]
    fn docx_field_code_page_placeholder() {
        // A PAGE field with no run text → a "1" placeholder in the paragraph.
        let doc = r#"<w:document xmlns:w="x"><w:body>
          <w:p><w:r><w:fldChar w:fldCharType="begin"/></w:r>
            <w:r><w:instrText> PAGE \* MERGEFORMAT </w:instrText></w:r>
            <w:r><w:fldChar w:fldCharType="end"/></w:r></w:p>
        </w:body></w:document>"#;
        let html = docx_html(doc);
        assert!(html.contains("<p>1</p>"), "page placeholder: {html}");
    }

    #[test]
    fn docx_headers_footers_and_footnotes_render() {
        let doc = r#"<w:document xmlns:w="x"><w:body>
          <w:p><w:r><w:t>Main body line</w:t></w:r></w:p>
        </w:body></w:document>"#;
        let header = r#"<w:hdr xmlns:w="x"><w:p><w:r><w:t>Top Header</w:t></w:r></w:p></w:hdr>"#;
        let footer = r#"<w:ftr xmlns:w="x"><w:p><w:r><w:t>Bottom Footer</w:t></w:r></w:p></w:ftr>"#;
        let footnotes = r#"<w:footnotes xmlns:w="x">
          <w:footnote w:type="separator" w:id="-1"><w:p><w:r><w:t>SEP</w:t></w:r></w:p></w:footnote>
          <w:footnote w:id="1"><w:p><w:r><w:t>A note text</w:t></w:r></w:p></w:footnote>
        </w:footnotes>"#;
        let mut z = ZipWriter::new();
        z.add_stored("[Content_Types].xml", b"<Types/>");
        z.add_stored("word/document.xml", doc.as_bytes());
        z.add_stored("word/header1.xml", header.as_bytes());
        z.add_stored("word/footer1.xml", footer.as_bytes());
        z.add_stored("word/footnotes.xml", footnotes.as_bytes());
        let pdf = office_to_pdf(&z.finish()).expect("docx converts");
        let text = norm(&opens(&pdf).to_text());
        for needle in [
            "Top Header",
            "Main body line",
            "Bottom Footer",
            "A note text",
        ] {
            assert!(text.contains(needle), "missing {needle:?}: {text}");
        }
        // The separator placeholder footnote is not surfaced.
        assert!(!text.contains("SEP"), "separator dropped: {text}");
    }

    // ── P1/P3: DOCX list ordinals from numbering.xml ──

    #[test]
    fn docx_ordered_list_shows_decimal_ordinals() {
        // numId 1 → abstractNum 0, level 0 = decimal: items render 1. 2. 3.
        let numbering = r#"<w:numbering xmlns:w="x">
          <w:abstractNum w:abstractNumId="0">
            <w:lvl w:ilvl="0"><w:numFmt w:val="decimal"/></w:lvl>
          </w:abstractNum>
          <w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>
        </w:numbering>"#;
        let doc = r#"<w:document xmlns:w="x"><w:body>
          <w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>First</w:t></w:r></w:p>
          <w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>Second</w:t></w:r></w:p>
          <w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>Third</w:t></w:r></w:p>
        </w:body></w:document>"#;
        let html = docx_html_with(doc, "", numbering);
        assert!(html.contains("1. First"), "ordinal 1: {html}");
        assert!(html.contains("2. Second"), "ordinal 2: {html}");
        assert!(html.contains("3. Third"), "ordinal 3: {html}");
        // No bullet for an ordered list.
        assert!(!html.contains("\u{2022} First"), "no bullet: {html}");
    }

    #[test]
    fn docx_list_letter_and_nested_reset() {
        // Level 0 decimal, level 1 lowerLetter; nested level restarts at a.
        let numbering = r#"<w:numbering xmlns:w="x">
          <w:abstractNum w:abstractNumId="0">
            <w:lvl w:ilvl="0"><w:numFmt w:val="decimal"/></w:lvl>
            <w:lvl w:ilvl="1"><w:numFmt w:val="lowerLetter"/></w:lvl>
          </w:abstractNum>
          <w:num w:numId="2"><w:abstractNumId w:val="0"/></w:num>
        </w:numbering>"#;
        let doc = r#"<w:document xmlns:w="x"><w:body>
          <w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="2"/></w:numPr></w:pPr><w:r><w:t>One</w:t></w:r></w:p>
          <w:p><w:pPr><w:numPr><w:ilvl w:val="1"/><w:numId w:val="2"/></w:numPr></w:pPr><w:r><w:t>Sub A</w:t></w:r></w:p>
          <w:p><w:pPr><w:numPr><w:ilvl w:val="1"/><w:numId w:val="2"/></w:numPr></w:pPr><w:r><w:t>Sub B</w:t></w:r></w:p>
          <w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="2"/></w:numPr></w:pPr><w:r><w:t>Two</w:t></w:r></w:p>
        </w:body></w:document>"#;
        let html = docx_html_with(doc, "", numbering);
        assert!(html.contains("1. One"), "level0 #1: {html}");
        assert!(html.contains("a. Sub A"), "level1 a: {html}");
        assert!(html.contains("b. Sub B"), "level1 b: {html}");
        assert!(html.contains("2. Two"), "level0 #2: {html}");
    }

    #[test]
    fn docx_list_bullet_format_keeps_bullet() {
        // A bullet-format level still renders the bullet glyph.
        let numbering = r#"<w:numbering xmlns:w="x">
          <w:abstractNum w:abstractNumId="0">
            <w:lvl w:ilvl="0"><w:numFmt w:val="bullet"/></w:lvl>
          </w:abstractNum>
          <w:num w:numId="3"><w:abstractNumId w:val="0"/></w:num>
        </w:numbering>"#;
        let doc = r#"<w:document xmlns:w="x"><w:body>
          <w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="3"/></w:numPr></w:pPr><w:r><w:t>Dot</w:t></w:r></w:p>
        </w:body></w:document>"#;
        let html = docx_html_with(doc, "", numbering);
        assert!(html.contains("\u{2022} Dot"), "bullet kept: {html}");
    }

    #[test]
    fn docx_list_without_numbering_falls_back_to_bullet() {
        // No numbering.xml → the legacy bullet behaviour is preserved.
        let doc = r#"<w:document xmlns:w="x"><w:body>
          <w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="9"/></w:numPr></w:pPr><w:r><w:t>X</w:t></w:r></w:p>
        </w:body></w:document>"#;
        let html = docx_html(doc);
        assert!(html.contains("\u{2022} X"), "fallback bullet: {html}");
    }

    #[test]
    fn ordinal_helpers_letter_and_roman() {
        assert_eq!(alpha_ordinal(1, false), "a");
        assert_eq!(alpha_ordinal(26, false), "z");
        assert_eq!(alpha_ordinal(27, false), "aa");
        assert_eq!(alpha_ordinal(2, true), "B");
        assert_eq!(roman(4, false), "iv");
        assert_eq!(roman(9, true), "IX");
        assert_eq!(roman(2024, true), "MMXXIV");
    }

    #[test]
    fn docx_gridspan_expands_to_physical_cells() {
        // A 2-column gridSpan cell becomes "<td colspan=2>…</td><td></td>".
        let doc = r#"<w:document xmlns:w="x"><w:body>
            <w:tbl>
              <w:tr>
                <w:tc><w:tcPr><w:gridSpan w:val="2"/></w:tcPr><w:p><w:r><w:t>Wide</w:t></w:r></w:p></w:tc>
              </w:tr>
              <w:tr>
                <w:tc><w:p><w:r><w:t>L</w:t></w:r></w:p></w:tc>
                <w:tc><w:p><w:r><w:t>R</w:t></w:r></w:p></w:tc>
              </w:tr>
            </w:tbl>
          </w:body></w:document>"#;
        let html = docx_html(doc);
        assert!(html.contains("colspan=\"2\""), "colspan emitted: {html}");
        // The spanning cell carries the content; a single empty <td> pads the
        // row to 2 physical columns so the equal-width layout spreads it.
        assert!(
            html.contains("Wide</p></td><td></td>"),
            "padded cell: {html}"
        );
        let pdf = office_to_pdf(&build_docx(doc, None, &[])).expect("docx converts");
        let text = norm(&opens(&pdf).to_text());
        for needle in ["Wide", "L", "R"] {
            assert!(text.contains(needle), "missing {needle:?}: {text}");
        }
    }

    #[test]
    fn docx_tblgrid_emits_proportional_colgroup() {
        // w:tblGrid 3000/1000 twips → 150pt / 50pt columns in a leading
        // <colgroup>; the rows still emit their cells unchanged.
        let doc = r#"<w:document xmlns:w="x"><w:body>
            <w:tbl>
              <w:tblGrid>
                <w:gridCol w:w="3000"/>
                <w:gridCol w:w="1000"/>
              </w:tblGrid>
              <w:tr>
                <w:tc><w:p><w:r><w:t>Wide</w:t></w:r></w:p></w:tc>
                <w:tc><w:p><w:r><w:t>Narrow</w:t></w:r></w:p></w:tc>
              </w:tr>
            </w:tbl>
          </w:body></w:document>"#;
        let html = docx_html(doc);
        assert!(
            html.contains(
                "<colgroup><col style=\"width:150pt\"><col style=\"width:50pt\"></colgroup>"
            ),
            "proportional colgroup before rows: {html}"
        );
        // The colgroup precedes the first row.
        let cg = html.find("<colgroup>").expect("colgroup present");
        let tr = html.find("<tr>").expect("row present");
        assert!(cg < tr, "colgroup precedes first row: {html}");
        let pdf = office_to_pdf(&build_docx(doc, None, &[])).expect("docx converts");
        let text = norm(&opens(&pdf).to_text());
        assert!(text.contains("Wide") && text.contains("Narrow"), "{text}");
    }

    #[test]
    fn docx_gridspan_still_works_with_tblgrid() {
        // gridSpan expansion (colspan + padding) is unaffected by the colgroup.
        let doc = r#"<w:document xmlns:w="x"><w:body>
            <w:tbl>
              <w:tblGrid><w:gridCol w:w="2000"/><w:gridCol w:w="2000"/></w:tblGrid>
              <w:tr>
                <w:tc><w:tcPr><w:gridSpan w:val="2"/></w:tcPr><w:p><w:r><w:t>Wide</w:t></w:r></w:p></w:tc>
              </w:tr>
            </w:tbl>
          </w:body></w:document>"#;
        let html = docx_html(doc);
        assert!(
            html.contains("colspan=\"2\""),
            "colspan still emitted: {html}"
        );
        assert!(
            html.contains("Wide</p></td><td></td>"),
            "padding intact: {html}"
        );
        assert!(
            html.contains("<colgroup>"),
            "colgroup still emitted: {html}"
        );
    }

    #[test]
    fn docx_table_without_grid_has_no_colgroup() {
        // No w:tblGrid ⇒ no <colgroup> (layout keeps equal columns).
        let doc = r#"<w:document xmlns:w="x"><w:body>
            <w:tbl><w:tr>
              <w:tc><w:p><w:r><w:t>A</w:t></w:r></w:p></w:tc>
              <w:tc><w:p><w:r><w:t>B</w:t></w:r></w:p></w:tc>
            </w:tr></w:tbl>
          </w:body></w:document>"#;
        let html = docx_html(doc);
        assert!(
            !html.contains("<colgroup>"),
            "no grid ⇒ no colgroup: {html}"
        );
    }

    #[test]
    fn odf_table_column_widths_become_colgroup() {
        // table:table-column referencing a style whose column-width is 3cm/1cm
        // ⇒ a <colgroup> with the converted point widths (3cm ≈ 85.04pt).
        let xml = r#"<x xmlns:table="tb" xmlns:text="t">
            <table:table table:name="T">
              <table:table-column table:style-name="co1"/>
              <table:table-column table:style-name="co2"/>
              <table:table-row>
                <table:table-cell><text:p>A</text:p></table:table-cell>
                <table:table-cell><text:p>B</text:p></table:table-cell>
              </table:table-row>
            </table:table>
          </x>"#;
        let mut cols = BTreeMap::new();
        cols.insert("co1".to_string(), 85.04); // 3cm
        cols.insert("co2".to_string(), 28.35); // 1cm
        let zip: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        let styles: BTreeMap<String, String> = BTreeMap::new();
        let mut x = Xml::new(xml);
        // Advance to the <table:table> open so odf_table consumes from there.
        while let Some(tok) = x.next() {
            if let Tok::Open(name, _, false) = &tok {
                if local(name) == "table" {
                    break;
                }
            }
        }
        let mut out = String::new();
        odf_table(&mut x, &zip, &styles, &cols, &mut out);
        assert!(
            out.contains(
                "<colgroup><col style=\"width:85.04pt\"><col style=\"width:28.35pt\"></colgroup>"
            ),
            "ODF column widths in colgroup: {out}"
        );
        let cg = out.find("<colgroup>").expect("colgroup present");
        let tr = out.find("<tr>").expect("row present");
        assert!(cg < tr, "colgroup precedes first row: {out}");
    }

    #[test]
    fn odf_column_widths_parses_column_properties() {
        // style:column-width on a table-column style is read (cm→pt).
        let xml = r#"<x xmlns:style="s" xmlns:table="tb">
            <style:style style:name="co1" style:family="table-column">
              <style:table-column-properties style:column-width="2cm"/>
            </style:style>
          </x>"#;
        let map = odf_column_widths(xml);
        let w = map.get("co1").copied().expect("co1 width parsed");
        assert!((w - 56.6929134).abs() < 0.01, "2cm ≈ 56.69pt ({w})");
    }

    #[test]
    fn docx_vmerge_restart_and_continue() {
        // restart → rowspan hint; continue cell is dropped (column preserved).
        let doc = r#"<w:document xmlns:w="x"><w:body>
            <w:tbl>
              <w:tr>
                <w:tc><w:tcPr><w:vMerge w:val="restart"/></w:tcPr><w:p><w:r><w:t>Merged</w:t></w:r></w:p></w:tc>
                <w:tc><w:p><w:r><w:t>Top</w:t></w:r></w:p></w:tc>
              </w:tr>
              <w:tr>
                <w:tc><w:tcPr><w:vMerge/></w:tcPr><w:p><w:r><w:t>Hidden</w:t></w:r></w:p></w:tc>
                <w:tc><w:p><w:r><w:t>Bottom</w:t></w:r></w:p></w:tc>
              </w:tr>
            </w:tbl>
          </w:body></w:document>"#;
        let html = docx_html(doc);
        assert!(html.contains("rowspan=\"2\""), "rowspan on restart: {html}");
        // The continuation cell's content ("Hidden") is suppressed.
        assert!(!html.contains("Hidden"), "covered cell dropped: {html}");
        // Second row therefore has exactly one <td> (Bottom).
        let pdf = office_to_pdf(&build_docx(doc, None, &[])).expect("docx converts");
        let text = norm(&opens(&pdf).to_text());
        assert!(text.contains("Merged") && text.contains("Bottom"), "{text}");
    }

    #[test]
    fn argb_strips_alpha_and_rejects_transparent() {
        assert_eq!(argb_to_hex6(Some("FFFFFF00")), Some("#FFFF00".to_string()));
        assert_eq!(argb_to_hex6(Some("ff00ff00")), Some("#00FF00".to_string()));
        assert_eq!(argb_to_hex6(Some("00AABB")), Some("#00AABB".to_string()));
        assert_eq!(argb_to_hex6(Some("#FF112233")), Some("#112233".to_string()));
        assert!(argb_to_hex6(Some("00FFFFFF")).is_none(), "transparent");
        assert!(argb_to_hex6(Some("xyz")).is_none());
        assert!(argb_to_hex6(None).is_none());
    }

    #[test]
    fn parse_xlsx_styles_maps_style_index_to_solid_colour() {
        // cellXfs[0] → fillId 0 (none); [1] → fillId 2 (solid yellow).
        let styles = r#"<styleSheet xmlns="s">
          <fills count="3">
            <fill><patternFill patternType="none"/></fill>
            <fill><patternFill patternType="gray125"/></fill>
            <fill><patternFill patternType="solid"><fgColor rgb="FFFFFF00"/><bgColor indexed="64"/></patternFill></fill>
          </fills>
          <cellXfs count="2">
            <xf numFmtId="0" fontId="0" fillId="0"/>
            <xf numFmtId="0" fontId="0" fillId="2" applyFill="1"/>
          </cellXfs>
        </styleSheet>"#;
        let s = parse_xlsx_styles(styles, &XlsxTheme::default());
        assert_eq!(s.fills.len(), 2);
        assert_eq!(s.fill(0), None, "default style: no fill");
        assert_eq!(s.fill(1), Some("#FFFF00".to_string()), "solid yellow");
    }

    #[test]
    fn xlsx_cell_fill_becomes_background_color() {
        // Hand-built XLSX where B1 uses a yellow solid fill (style index 1).
        let mut z = ZipWriter::new();
        z.add_stored("[Content_Types].xml", b"<Types/>");
        z.add_stored(
            "xl/workbook.xml",
            br#"<workbook><sheets><sheet name="Painted" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
        );
        z.add_stored(
            "xl/styles.xml",
            br#"<styleSheet>
              <fills count="3">
                <fill><patternFill patternType="none"/></fill>
                <fill><patternFill patternType="gray125"/></fill>
                <fill><patternFill patternType="solid"><fgColor rgb="FFFFFF00"/></patternFill></fill>
              </fills>
              <cellXfs count="2">
                <xf fillId="0"/>
                <xf fillId="2" applyFill="1"/>
              </cellXfs>
            </styleSheet>"#,
        );
        z.add_stored(
            "xl/worksheets/sheet1.xml",
            br#"<worksheet><sheetData>
              <row r="1">
                <c r="A1" t="inlineStr"><is><t>Plain</t></is></c>
                <c r="B1" s="1" t="inlineStr"><is><t>Yellow</t></is></c>
              </row>
            </sheetData></worksheet>"#,
        );
        let xlsx = z.finish();
        // Exercise the table HTML directly so we can assert on the colour.
        let shared: Vec<String> = Vec::new();
        let styles = parse_xlsx_styles(
            &String::from_utf8_lossy(&read_zip(&xlsx)["xl/styles.xml"]),
            &XlsxTheme::default(),
        );
        let sheet_xml =
            String::from_utf8_lossy(&read_zip(&xlsx)["xl/worksheets/sheet1.xml"]).into_owned();
        let table = xlsx_sheet_table(&sheet_xml, &shared, &styles);
        assert!(
            table.contains("background-color:#FFFF00"),
            "B1 painted: {table}"
        );
        // And the inlineStr text is present.
        assert!(
            table.contains("Yellow") && table.contains("Plain"),
            "{table}"
        );
        // Full pipeline still produces a valid PDF with the text.
        let pdf = office_to_pdf(&xlsx).expect("xlsx converts");
        let text = norm(&opens(&pdf).to_text());
        assert!(
            text.contains("Painted") && text.contains("Yellow"),
            "{text}"
        );
    }

    #[test]
    fn xlsx_theme_and_indexed_fills_resolve_to_concrete_rgb() {
        // accent1 = blue (#4472C4). cellXfs[0] → fillId 1 = theme accent1 with a
        // positive tint (lightens); cellXfs[1] → fillId 2 = indexed red (idx 2).
        let theme = parse_xlsx_theme(
            r#"<theme><themeElements><clrScheme>
              <dk1><srgbClr val="000000"/></dk1>
              <lt1><srgbClr val="FFFFFF"/></lt1>
              <dk2><srgbClr val="44546A"/></dk2>
              <lt2><srgbClr val="E7E6E6"/></lt2>
              <accent1><srgbClr val="4472C4"/></accent1>
            </clrScheme></themeElements></theme>"#,
        );
        // Spreadsheet @theme index 4 == accent1.
        assert_eq!(theme.color(4), Some([0x44, 0x72, 0xC4]), "accent1 parsed");

        let styles_xml = r#"<styleSheet>
          <fills count="3">
            <fill><patternFill patternType="none"/></fill>
            <fill><patternFill patternType="solid"><fgColor theme="4" tint="0.5"/></patternFill></fill>
            <fill><patternFill patternType="solid"><fgColor indexed="2"/></patternFill></fill>
          </fills>
          <cellXfs count="2">
            <xf fillId="1" applyFill="1"/>
            <xf fillId="2" applyFill="1"/>
          </cellXfs>
        </styleSheet>"#;
        let s = parse_xlsx_styles(styles_xml, &theme);
        // Theme accent1 lightened by tint=0.5 → blended toward white, not None.
        let themed = s.fill(0).expect("themed fill resolved");
        assert_eq!(themed, apply_tint([0x44, 0x72, 0xC4], 0.5));
        assert_ne!(themed, "#4472C4", "tint actually applied");
        // Indexed colour 2 is pure red in the classic palette.
        assert_eq!(s.fill(1), Some("#FF0000".to_string()), "indexed red");

        // And it lands on the <td> background when rendered.
        let sheet = r#"<worksheet><sheetData>
          <row r="1">
            <c r="A1" s="0" t="inlineStr"><is><t>Themed</t></is></c>
            <c r="B1" s="1" t="inlineStr"><is><t>Indexed</t></is></c>
          </row>
        </sheetData></worksheet>"#;
        let table = xlsx_sheet_table(sheet, &[], &s);
        assert!(
            table.contains(&format!("background-color:{themed}")),
            "themed bg present: {table}"
        );
        assert!(
            table.contains("background-color:#FF0000"),
            "indexed bg present: {table}"
        );
    }

    #[test]
    fn xlsx_numfmt_renders_date_serial_and_currency() {
        // numFmtId 14 = built-in date (mm-dd-yy → date); custom 164 = currency.
        let styles_xml = r#"<styleSheet>
          <numFmts count="1">
            <numFmt numFmtId="164" formatCode="&quot;$&quot;#,##0.00"/>
          </numFmts>
          <fills count="1"><fill><patternFill patternType="none"/></fill></fills>
          <cellXfs count="3">
            <xf numFmtId="0"/>
            <xf numFmtId="14" applyNumberFormat="1"/>
            <xf numFmtId="164" applyNumberFormat="1"/>
          </cellXfs>
        </styleSheet>"#;
        let s = parse_xlsx_styles(styles_xml, &XlsxTheme::default());

        // Direct format-code checks: serial 45000 = 2023-03-15; 1234.5 → grouped $.
        // (Excel 1900 system: serial 45000 → 2023-03-15.)
        assert_eq!(
            format_cell_number("45000", "mm-dd-yy"),
            Some("2023-03-15".to_string()),
            "date serial → formatted date"
        );
        assert_eq!(
            format_cell_number("1234.5", "\"$\"#,##0.00"),
            Some("$1,234.50".to_string()),
            "currency → grouped with $"
        );

        // Rendered: the raw serial/value must NOT appear; the formatted form does.
        let sheet = r#"<worksheet><sheetData>
          <row r="1">
            <c r="A1" s="1"><v>45000</v></c>
            <c r="B1" s="2"><v>1234.5</v></c>
          </row>
        </sheetData></worksheet>"#;
        let table = xlsx_sheet_table(sheet, &[], &s);
        assert!(
            table.contains("2023-03-15"),
            "date rendered formatted: {table}"
        );
        assert!(!table.contains(">45000<"), "raw serial suppressed: {table}");
        assert!(
            table.contains("$1,234.50"),
            "currency rendered formatted: {table}"
        );
    }

    #[test]
    fn xlsx_merge_cells_emit_spans_and_skip_covered() {
        // A1:B1 horizontal merge (colspan 2) and A2:A3 vertical merge (rowspan 2).
        let sheet = r#"<worksheet>
          <mergeCells count="2">
            <mergeCell ref="A1:B1"/>
            <mergeCell ref="A2:A3"/>
          </mergeCells>
          <sheetData>
            <row r="1">
              <c r="A1" t="inlineStr"><is><t>Wide</t></is></c>
              <c r="B1" t="inlineStr"><is><t>Hidden</t></is></c>
            </row>
            <row r="2">
              <c r="A2" t="inlineStr"><is><t>Tall</t></is></c>
              <c r="B2" t="inlineStr"><is><t>Right</t></is></c>
            </row>
            <row r="3">
              <c r="A3" t="inlineStr"><is><t>Covered</t></is></c>
              <c r="B3" t="inlineStr"><is><t>Below</t></is></c>
            </row>
          </sheetData>
        </worksheet>"#;
        let table = xlsx_sheet_table(sheet, &[], &XlsxStyles::default());

        // Anchor A1 carries colspan=2; the covered B1 ("Hidden") is dropped.
        assert!(
            table.contains("<td colspan=\"2\">Wide</td>"),
            "A1 spans 2 cols: {table}"
        );
        assert!(!table.contains("Hidden"), "B1 covered & skipped: {table}");

        // Anchor A2 carries rowspan=2; the covered A3 ("Covered") is dropped.
        assert!(
            table.contains("<td rowspan=\"2\">Tall</td>"),
            "A2 spans 2 rows: {table}"
        );
        assert!(!table.contains("Covered"), "A3 covered & skipped: {table}");

        // Non-merged neighbours stay put.
        assert!(
            table.contains("Right") && table.contains("Below"),
            "B2/B3 preserved: {table}"
        );

        // MergeMap accessors agree with the rendering.
        let m = MergeMap::build(&parse_merges(sheet));
        assert_eq!(m.anchor(0, 0), Some((2, 1)), "A1 colspan");
        assert_eq!(m.anchor(1, 0), Some((1, 2)), "A2 rowspan");
        assert!(m.is_covered(0, 1), "B1 covered");
        assert!(m.is_covered(2, 0), "A3 covered");
        assert!(!m.is_covered(1, 1), "B2 not covered");
    }

    #[test]
    fn docx_image_embedded_as_data_uri() {
        // The DOCX blip→rels→media path emits an <img src="data:image/png">.
        let doc = r#"<w:document xmlns:w="x" xmlns:a="y" xmlns:r="z"><w:body>
            <w:p><w:r><w:drawing><a:blip r:embed="rId9"/></w:drawing></w:r></w:p>
          </w:body></w:document>"#;
        let rels = r#"<Relationships xmlns="x">
          <Relationship Id="rId9" Type="image" Target="media/logo.png"/>
        </Relationships>"#;
        let png = red_png();
        let zip = {
            let mut z = ZipWriter::new();
            z.add_stored("word/document.xml", doc.as_bytes());
            z.add_stored("word/_rels/document.xml.rels", rels.as_bytes());
            z.add_stored("word/media/logo.png", &png);
            read_zip(&z.finish())
        };
        let rmap = parse_rels(&String::from_utf8_lossy(
            &zip["word/_rels/document.xml.rels"],
        ));
        let styles = DocxStyles::default();
        let numbering = DocxNumbering::default();
        let footnotes = DocxFootnotes::default();
        let ctx = DocxCtx {
            zip: &zip,
            rels: &rmap,
            styles: &styles,
            numbering: &numbering,
            footnotes: &footnotes,
        };
        let mut body = String::new();
        docx_body(
            &String::from_utf8_lossy(&zip["word/document.xml"]),
            &ctx,
            &mut body,
        );
        assert!(
            body.contains("<img src=\"data:image/png;base64,"),
            "image embedded as data URI: {body}"
        );
    }

    #[test]
    fn odf_list_bullets_and_indents() {
        // text:list → bulleted, indented paragraphs; nested list indents more.
        let content = r#"<office:document-content xmlns:office="o" xmlns:text="t">
          <office:body><office:text>
            <text:list>
              <text:list-item><text:p>Alpha</text:p></text:list-item>
              <text:list-item>
                <text:list>
                  <text:list-item><text:p>Beta</text:p></text:list-item>
                </text:list>
              </text:list-item>
            </text:list>
          </office:text></office:body>
        </office:document-content>"#;
        let mut z = ZipWriter::new();
        z.add_stored("mimetype", b"application/vnd.oasis.opendocument.text");
        z.add_stored("content.xml", content.as_bytes());
        let odt = z.finish();
        // Inspect the generated body markup directly.
        let zip = read_zip(&odt);
        let styles = BTreeMap::new();
        let cols = BTreeMap::new();
        let mut body = String::new();
        odf_walk(
            &mut Xml::new(&String::from_utf8_lossy(&zip["content.xml"])),
            &zip,
            &styles,
            &cols,
            &mut body,
            None,
            None,
        );
        assert!(
            body.contains("\u{2022} Alpha"),
            "bullet on top item: {body}"
        );
        assert!(
            body.contains("\u{2022} Beta"),
            "bullet on nested item: {body}"
        );
        assert!(body.contains("margin-left:36pt"), "level-0 indent: {body}");
        assert!(body.contains("margin-left:72pt"), "level-1 indent: {body}");
        // Full pipeline still renders both items.
        let pdf = office_to_pdf(&odt).expect("odt converts");
        let text = norm(&opens(&pdf).to_text());
        assert!(text.contains("Alpha") && text.contains("Beta"), "{text}");
    }
}
