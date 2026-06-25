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
use crate::html::{FontRequest, Margins, ProvidedFont, RenderOptions};
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
/// resolved page geometry, with **no document-embedded fonts**.
///
/// Families the document *references* (Calibri, Arial, …) are still resolved by
/// the host via the engine's two-phase contract ([`office_needed_fonts`]); the
/// real `font-family` names injected into the HTML let the host fetch and embed
/// the matching faces. Use [`render_geom_with_fonts`] to additionally pass faces
/// extracted from inside the container so a self-embedding document renders with
/// its own typefaces (exact glyphs + metrics) — no host round-trip needed.
fn render_geom(body: &str, geom: PageGeom) -> Vec<u8> {
    render_geom_with_fonts(body, geom, &[])
}

/// Like [`render_geom`] but feeds the [`ProvidedFont`](crate::html::ProvidedFont)
/// faces extracted from the Office container (DOCX/PPTX `word|ppt/fonts/*.odttf`
/// de-obfuscated, ODF `Fonts/*`) into the renderer. A run whose `font-family`
/// matches an extracted face is then laid out and painted with that exact face
/// (true advance widths + glyphs) instead of the bundled Liberation fallback.
fn render_geom_with_fonts(
    body: &str,
    geom: PageGeom,
    fonts: &[crate::html::ProvidedFont],
) -> Vec<u8> {
    crate::html::render_with(&html_doc(body), fonts, &geom.render_options())
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

/// Phase 2 of the two-phase font flow for [`office_to_pdf`]: render the container
/// with `host`-supplied faces (the families [`office_needed_fonts`] reported as
/// referenced-but-unembedded, fetched by the host and handed back here — e.g.
/// Carlito for a Calibri reference) so styled runs lay out with the right
/// advance widths instead of drifting onto the bundled Liberation metrics.
///
/// Each format [merges](merge_fonts) the host faces with whatever the document
/// embeds itself; **embedded faces win on conflict**, so calling this with the
/// right `host` set never regresses a self-embedding document. Dispatch mirrors
/// [`office_to_pdf`]; legacy OLE2 has no resolvable font references, so its output
/// is identical to [`office_to_pdf`] regardless of `host`. `None` for an
/// unrecognized archive.
pub fn office_to_pdf_with_fonts(bytes: &[u8], host: &[ProvidedFont]) -> Option<Vec<u8>> {
    // Legacy OLE2 Compound File (.doc/.xls/.ppt) — no resolvable font references.
    if bytes.len() >= 8 && bytes[..8] == [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1] {
        return ole2_to_pdf(bytes);
    }

    let zip = read_zip(bytes);
    if zip.contains_key("word/document.xml") {
        Some(docx_to_pdf_with(&zip, host))
    } else if zip.contains_key("ppt/presentation.xml") {
        Some(pptx_to_pdf_with(&zip, host))
    } else if zip.contains_key("xl/workbook.xml") {
        Some(xlsx_to_pdf_with(&zip, host))
    } else if let Some(mimetype) = zip.get("mimetype") {
        let mt = String::from_utf8_lossy(mimetype);
        if mt.contains("opendocument.text") {
            Some(odt_to_pdf_with(&zip, host))
        } else if mt.contains("opendocument.spreadsheet") {
            Some(ods_to_pdf_with(&zip, host))
        } else if mt.contains("opendocument.presentation") {
            Some(odp_to_pdf_with(&zip, host))
        } else {
            None
        }
    } else {
        None
    }
}

// ════════════════════════════ Office → unified model ══════════════════════════
//
// The `*_to_model` functions below are the structured counterpart of the
// `*_to_pdf` exporters above: instead of emitting styled HTML they populate the
// format-neutral [`crate::model::Document`] tree directly (paragraphs, headings,
// lists, tables, typed spreadsheet cells, slides). They REUSE the very same
// parsers (the [`Xml`] tokenizer, `parse_rels`/`parse_docx_styles`/
// `parse_shared_strings`/`parse_merges`/the PPTX/ODF helpers); only the
// *emit* step differs. The HTML path is retained as the rendering fallback.

use crate::model::style::{Align as MAlign, LineHeight as MLineHeight};
use crate::model::{
    self, Block, BlockKind, Cell, CharStyle, DocMeta, Document, Heading, Inline, InlineRun, List,
    ListItem, ListMarker, PageGeometry, Paragraph, ParagraphStyle, Row, Section, Sheet, SheetBlock,
    SheetCell, SheetRow, Slide, SlideBlock, Table,
};

/// Convert a `PageGeom` (Office fallback/declared geometry) to the model's
/// [`PageGeometry`], reusing the already-resolved size and margins.
fn page_geometry(g: PageGeom) -> PageGeometry {
    PageGeometry {
        width: g.w,
        height: g.h,
        margins: crate::model::Margins {
            top: g.margins.top,
            right: g.margins.right,
            bottom: g.margins.bottom,
            left: g.margins.left,
        },
    }
}

/// `#RRGGBB` / `RRGGBB` → RGB `0.0..=1.0`, reusing [`hex6_to_rgb`].
fn hex_to_rgb_f64(s: &str) -> Option<[f64; 3]> {
    hex6_to_rgb(s).map(|[r, g, b]| [r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0])
}

/// Format an RGB triple (`0.0..=1.0`, clamped) as an uppercase `RRGGBB` string.
fn rgb_to_hex6(rgb: [f64; 3]) -> String {
    let c = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("{:02X}{:02X}{:02X}", c(rgb[0]), c(rgb[1]), c(rgb[2]))
}

/// Convert an OOXML `a:hslClr` (`@hue` in 60000ths of a degree, `@sat`/`@lum` in
/// thousandths of a percent) to an uppercase `RRGGBB` string.
fn hsl_attrs_to_hex6(attrs: &[(String, String)]) -> Option<String> {
    let hue = attr(attrs, "hue").and_then(|v| v.trim().parse::<f64>().ok())? / 60000.0;
    let sat = attr(attrs, "sat").and_then(|v| v.trim().parse::<f64>().ok())? / 100_000.0;
    let lum = attr(attrs, "lum").and_then(|v| v.trim().parse::<f64>().ok())? / 100_000.0;
    Some(rgb_to_hex6(hsl_to_rgb(
        hue.rem_euclid(360.0),
        sat.clamp(0.0, 1.0),
        lum.clamp(0.0, 1.0),
    )))
}

/// HSL (`hue` degrees `0..360`, `sat`/`lum` `0.0..=1.0`) → RGB `0.0..=1.0`.
fn hsl_to_rgb(hue: f64, sat: f64, lum: f64) -> [f64; 3] {
    let c = (1.0 - (2.0 * lum - 1.0).abs()) * sat;
    let h = hue / 60.0;
    let x = c * (1.0 - (h.rem_euclid(2.0) - 1.0).abs());
    let (r1, g1, b1) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = lum - c / 2.0;
    [r1 + m, g1 + m, b1 + m]
}

/// Apply the OOXML colour-transform modifiers accumulated for one fill colour to
/// a base `RRGGBB`, returning the modulated `RRGGBB`. `lum_mod` scales each
/// channel and `lum_off` adds a flat offset (both fractions of full scale);
/// `shade` darkens toward black and `tint` lightens toward white (PowerPoint's
/// straight-RGB approximation — exact enough for run/cell colour fidelity without
/// a full linear-RGB round-trip). `None`/empty modifiers leave the base intact.
fn apply_color_mods(
    base: &str,
    lum_mod: Option<f64>,
    lum_off: Option<f64>,
    shade: Option<f64>,
    tint: Option<f64>,
) -> Option<String> {
    let mut rgb = hex_to_rgb_f64(base)?;
    if let Some(m) = lum_mod {
        rgb = rgb.map(|c| c * m);
    }
    if let Some(o) = lum_off {
        rgb = rgb.map(|c| c + o);
    }
    if let Some(s) = shade {
        rgb = rgb.map(|c| c * s);
    }
    if let Some(t) = tint {
        rgb = rgb.map(|c| c + (1.0 - c) * t);
    }
    Some(rgb_to_hex6(rgb))
}

/// The unresolved base of an OOXML fill colour: a literal `RRGGBB` (from
/// `a:srgbClr`/`a:hslClr`/`a:sysClr`) or a theme scheme slot name (`a:schemeClr@val`,
/// resolved against a [`PptxTheme`] only at [`finish`](PptxFillColor::finish_with)
/// time, where the theme is in scope).
enum PptxColorBase {
    Hex(String),
    Scheme(String),
}

/// Accumulates one OOXML fill colour while a `<a:solidFill>` / first `<a:gradFill>`
/// gradient stop is being walked: the (unresolved) base colour plus any
/// `a:lumMod`/`a:lumOff`/`a:shade`/`a:tint` child modifiers, folded together on
/// [`finish_with`](PptxFillColor::finish_with). The *first* base colour seen wins,
/// so a gradient's first stop is captured as a solid fallback.
#[derive(Default)]
struct PptxFillColor {
    base: Option<PptxColorBase>,
    lum_mod: Option<f64>,
    lum_off: Option<f64>,
    shade: Option<f64>,
    tint: Option<f64>,
}

impl PptxFillColor {
    /// Record a base colour from an `a:srgbClr`/`a:schemeClr`/`a:hslClr`/`a:sysClr`
    /// open tag (scheme slots kept raw, resolved on finish); the first one wins.
    fn set_base(&mut self, local_name: &str, attrs: &[(String, String)]) {
        if self.base.is_some() {
            return;
        }
        self.base = match local_name {
            "srgbClr" => attr(attrs, "val")
                .filter(|v| is_hex6(v))
                .map(|v| PptxColorBase::Hex(v.to_ascii_uppercase())),
            "schemeClr" => attr(attrs, "val").map(|v| PptxColorBase::Scheme(v.to_string())),
            "hslClr" => hsl_attrs_to_hex6(attrs).map(PptxColorBase::Hex),
            "sysClr" => attr(attrs, "lastClr")
                .filter(|v| is_hex6(v))
                .map(|v| PptxColorBase::Hex(v.to_ascii_uppercase())),
            _ => None,
        };
    }

    /// Record an `a:lumMod`/`a:lumOff`/`a:shade`/`a:tint` modifier (`@val` in
    /// thousandths of a percent) attached to the current base colour.
    fn set_mod(&mut self, local_name: &str, attrs: &[(String, String)]) {
        let Some(v) = attr(attrs, "val").and_then(|v| v.trim().parse::<f64>().ok()) else {
            return;
        };
        let frac = v / 100_000.0;
        match local_name {
            "lumMod" => self.lum_mod = Some(frac),
            "lumOff" => self.lum_off = Some(frac),
            "shade" => self.shade = Some(frac),
            "tint" => self.tint = Some(frac),
            _ => {}
        }
    }

    /// Resolve the base (a scheme slot through `theme`) and fold the modifiers into
    /// a final `RRGGBB`, or `None` when no usable base colour was captured.
    fn finish_with(self, theme: &PptxTheme) -> Option<String> {
        let base = match self.base? {
            PptxColorBase::Hex(h) => h,
            PptxColorBase::Scheme(slot) => theme.resolve_scheme(&slot)?,
        };
        apply_color_mods(&base, self.lum_mod, self.lum_off, self.shade, self.tint).or(Some(base))
    }
}

/// Map a DOCX `w:highlight@val` named colour (ECMA-376 §17.18.40) to its 6-hex
/// equivalent (no `#`). `none`/unknown ⇒ `None`. The 16 named highlight colours
/// are fixed by the spec, so the mapping is exact and dependency-free.
fn highlight_color(name: &str) -> Option<&'static str> {
    Some(match name {
        "yellow" => "FFFF00",
        "green" => "00FF00",
        "cyan" => "00FFFF",
        "magenta" => "FF00FF",
        "blue" => "0000FF",
        "red" => "FF0000",
        "darkBlue" => "000080",
        "darkCyan" => "008080",
        "darkGreen" => "008000",
        "darkMagenta" => "800080",
        "darkRed" => "800000",
        "darkYellow" => "808000",
        "darkGray" => "808080",
        "lightGray" => "C0C0C0",
        "black" => "000000",
        "white" => "FFFFFF",
        _ => return None, // "none" and any unrecognised token ⇒ no highlight
    })
}

/// Derive a [`CharStyle`] from a recovered [`RunStyle`]. The display family name
/// is kept verbatim; the portable generic class is inferred by reusing
/// [`super::style::parse_base_font`] (which classifies serif/sans/mono from a
/// family name). Size is half-points → points; colour is `RRGGBB` → RGB.
fn run_char_style(run: &RunStyle) -> CharStyle {
    let family = run.font_family.clone().unwrap_or_default();
    let generic = if family.is_empty() {
        crate::convert::style::Generic::default()
    } else {
        super::style::parse_base_font(&family).generic
    };
    CharStyle {
        family,
        generic,
        size_pt: run.size_half_pt.map(|h| h / 2.0).unwrap_or(0.0),
        bold: run.bold,
        italic: run.italic,
        underline: run.underline,
        strike: run.strike,
        color: run.color.as_deref().and_then(hex_to_rgb_f64),
        background: run.highlight.as_deref().and_then(hex_to_rgb_f64),
        vertical_align: model::VAlign::Baseline,
    }
}

/// Auto-detect an Office container and lower it to the unified
/// [`Document`] model, or `None` for an unrecognized archive (or a legacy OLE2
/// file with no readable text). Dispatch mirrors [`office_to_pdf`].
pub fn office_to_model(bytes: &[u8]) -> Option<Document> {
    // Legacy OLE2 Compound File (.doc/.xls/.ppt) — text-only paragraphs.
    if bytes.len() >= 8 && bytes[..8] == [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1] {
        return ole2_to_model(bytes);
    }

    let zip = read_zip(bytes);
    if zip.contains_key("word/document.xml") {
        Some(docx_to_model(&zip))
    } else if zip.contains_key("ppt/presentation.xml") {
        Some(pptx_to_model(&zip))
    } else if zip.contains_key("xl/workbook.xml") {
        Some(xlsx_to_model(&zip))
    } else if let Some(mimetype) = zip.get("mimetype") {
        let mt = String::from_utf8_lossy(mimetype);
        if mt.contains("opendocument.text") {
            Some(odt_to_model(&zip))
        } else if mt.contains("opendocument.spreadsheet") {
            Some(ods_to_model(&zip))
        } else if mt.contains("opendocument.presentation") {
            Some(odp_to_model(&zip))
        } else {
            None
        }
    } else {
        None
    }
}

// ───────────────────────────────── document metadata ─────────────────────────
//
// Both family of containers carry a document-metadata part that the importers
// must surface into [`DocMeta`] (otherwise the metadata is silently dropped on
// import — the inverse of the export side, which writes these very parts from
// `doc.meta`, see `export_model::ooxml_core_props`/`odf_meta_xml`).
//
// The full property set is read. The core five (`title`/`author`/`subject`/
// `keywords`/`lang`) are mapped, and the extended properties land in the
// matching [`DocMeta`] string fields: OOXML `dc:description`,
// `dcterms:created`/`dcterms:modified`, `cp:lastModifiedBy`, `cp:revision`
// (from `core.xml`) and `<Application>`/`<Company>` (from `app.xml`); ODF
// `dc:description`, `meta:creation-date`, `dc:date`, `meta:generator`,
// `meta:editing-cycles` (plus the Dublin-Core fields ODF shares with OOXML).
// Dates are kept as their raw ISO-8601 / W3CDTF source text.

/// Read OOXML document metadata into a [`DocMeta`] from a package: Dublin-Core
/// `docProps/core.xml` (ECMA-376 §15.2.12.1) plus `docProps/app.xml`
/// (§15.2.12.2). Core part: `dc:title`→title, `dc:creator`→author,
/// `dc:subject`→subject, `dc:language`→lang, `cp:keywords`→`keywords` (a single
/// delimited string, split on `,`/`;`), `dc:description`→description,
/// `dcterms:created`→created, `dcterms:modified`→modified,
/// `cp:lastModifiedBy`→last_modified_by, `cp:revision`→revision. App part:
/// `Application`→application, `Company`→company. Missing parts / fields ⇒ the
/// corresponding values stay unset (empty).
fn ooxml_doc_meta(zip: &BTreeMap<String, Vec<u8>>) -> DocMeta {
    let mut meta = DocMeta::default();
    if let Some(core) = zip.get("docProps/core.xml") {
        let core = String::from_utf8_lossy(core);
        let mut x = Xml::new(&core);
        while let Some(tok) = x.next() {
            if let Tok::Open(name, _, sc) = tok {
                if sc {
                    continue;
                }
                // The OOXML core part is flat: each property is a direct child of
                // `cp:coreProperties`. Dispatch on the local name (namespace
                // prefix ignored) and pull the element's text content; empty
                // strings are treated as "unset".
                let ln = local(&name).to_string();
                match ln.as_str() {
                    "title" => set_opt(&mut meta.title, xml_text_until(&mut x, &ln)),
                    "creator" => set_opt(&mut meta.author, xml_text_until(&mut x, &ln)),
                    "subject" => set_opt(&mut meta.subject, xml_text_until(&mut x, &ln)),
                    "language" => set_opt(&mut meta.lang, xml_text_until(&mut x, &ln)),
                    "description" => set_str(&mut meta.description, xml_text_until(&mut x, &ln)),
                    "created" => set_str(&mut meta.created, xml_text_until(&mut x, &ln)),
                    "modified" => set_str(&mut meta.modified, xml_text_until(&mut x, &ln)),
                    "lastModifiedBy" => {
                        set_str(&mut meta.last_modified_by, xml_text_until(&mut x, &ln))
                    }
                    "revision" => set_str(&mut meta.revision, xml_text_until(&mut x, &ln)),
                    "keywords" => {
                        let raw = xml_text_until(&mut x, &ln);
                        if meta.keywords.is_empty() {
                            meta.keywords = split_keywords(&raw);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    // Extended properties live in a separate part. `<Application>`/`<Company>`
    // are unprefixed in the extended-properties namespace.
    if let Some(app) = zip.get("docProps/app.xml") {
        let app = String::from_utf8_lossy(app);
        let mut x = Xml::new(&app);
        while let Some(tok) = x.next() {
            if let Tok::Open(name, _, sc) = tok {
                if sc {
                    continue;
                }
                let ln = local(&name).to_string();
                match ln.as_str() {
                    "Application" => set_str(&mut meta.application, xml_text_until(&mut x, &ln)),
                    "Company" => set_str(&mut meta.company, xml_text_until(&mut x, &ln)),
                    _ => {}
                }
            }
        }
    }
    meta
}

/// Read ODF document metadata into a [`DocMeta`] from `meta.xml`
/// (`office:document-meta`→`office:meta`, ISO 26300 §3/§4). Maps `dc:title`→
/// title, `dc:creator`→author, `dc:subject`→subject, `dc:language`→lang,
/// `dc:description`→description, `meta:creation-date`→created, `dc:date`→
/// modified, `meta:generator`→generator, `meta:editing-cycles`→editing_cycles,
/// and each repeated `meta:keyword` element → one keyword. Dates keep their raw
/// W3CDTF text. Missing part / fields ⇒ unset (empty).
fn odf_doc_meta(zip: &BTreeMap<String, Vec<u8>>) -> DocMeta {
    let mut meta = DocMeta::default();
    let Some(part) = zip.get("meta.xml") else {
        return meta;
    };
    let part = String::from_utf8_lossy(part);
    let mut x = Xml::new(&part);
    while let Some(tok) = x.next() {
        if let Tok::Open(name, _, sc) = tok {
            if sc {
                continue;
            }
            // `dc:date` (local `date`) is ODF's last-modified timestamp;
            // `meta:creation-date` (local `creation-date`) is the created one.
            let ln = local(&name).to_string();
            match ln.as_str() {
                "title" => set_opt(&mut meta.title, xml_text_until(&mut x, &ln)),
                "creator" => set_opt(&mut meta.author, xml_text_until(&mut x, &ln)),
                "subject" => set_opt(&mut meta.subject, xml_text_until(&mut x, &ln)),
                "language" => set_opt(&mut meta.lang, xml_text_until(&mut x, &ln)),
                "description" => set_str(&mut meta.description, xml_text_until(&mut x, &ln)),
                "creation-date" => set_str(&mut meta.created, xml_text_until(&mut x, &ln)),
                "date" => set_str(&mut meta.modified, xml_text_until(&mut x, &ln)),
                "generator" => set_str(&mut meta.generator, xml_text_until(&mut x, &ln)),
                "editing-cycles" => set_str(&mut meta.editing_cycles, xml_text_until(&mut x, &ln)),
                // ODF stores each keyword as its own `meta:keyword` element.
                "keyword" => {
                    let kw = xml_text_until(&mut x, &ln).trim().to_string();
                    if !kw.is_empty() && !meta.keywords.contains(&kw) {
                        meta.keywords.push(kw);
                    }
                }
                _ => {}
            }
        }
    }
    meta
}

/// Set an `Option<String>` `slot` to `value` only if `value` is non-empty (after
/// trimming) and the slot is still unset — so the first non-empty occurrence
/// wins and blank elements never clobber a real value.
fn set_opt(slot: &mut Option<String>, value: String) {
    if slot.is_some() {
        return;
    }
    let trimmed = value.trim();
    if !trimmed.is_empty() {
        *slot = Some(trimmed.to_string());
    }
}

/// Set a `String` `slot` (empty = absent) to `value` only if the slot is still
/// empty and `value` is non-empty after trimming — first non-empty wins.
fn set_str(slot: &mut String, value: String) {
    if !slot.is_empty() {
        return;
    }
    let trimmed = value.trim();
    if !trimmed.is_empty() {
        *slot = trimmed.to_string();
    }
}

/// Split an OOXML `cp:keywords` string into individual keywords. The standard
/// leaves the delimiter to the producer; the common ones are comma and
/// semicolon (the engine's own exporter joins with `", "`). Whitespace-only
/// fragments are dropped and surrounding whitespace trimmed.
fn split_keywords(raw: &str) -> Vec<String> {
    raw.split([',', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Wrap a flat list of flow blocks in a one-section, one-page [`Document`] with
/// the given page geometry (the common shape for prose: DOCX/ODT).
fn flow_document(blocks: Vec<Block>, geom: PageGeometry) -> Document {
    Document {
        sections: vec![Section {
            geometry: geom,
            header: None,
            footer: None,
            pages: vec![model::Page {
                blocks,
                absolute: false,
            }],
        }],
        ..Document::default()
    }
}

// ───────────────────────── document outline / bookmarks ───────────────────────
//
// Both Office model importers feed a single [`OutlineBuilder`]: each heading
// (DOCX `Heading*`/`Title` style or `w:outlineLvl`; ODF `text:h@outline-level`)
// records a flat, pre-order, `level`-tagged entry pointing at the page it lands
// on, and each *user-defined* bookmark (DOCX `w:bookmarkStart@w:name`, ODF
// `text:bookmark`/`text:bookmark-start@text:name`) records a navigable anchor
// nested under the enclosing heading. The flat list is then folded into the
// model's nested [`OutlineNode`](crate::model::OutlineNode) tree by the shared
// [`fold_outline`](crate::recon::fold_outline) assembler, which tolerates
// non-monotonic level jumps (a stray deep level attaches under the deepest open
// level rather than being dropped).

use crate::recon::FlatOutline;

/// Accumulates a document's outline entries (headings + bookmark anchors) in
/// reading order, then folds them into the nested model tree. `level` is the
/// 0-based outline depth (`0` = top-level heading); a bookmark nests one level
/// under the most recent heading so an internal `#name` anchor resolves to a
/// page within its section.
#[derive(Default)]
struct OutlineBuilder {
    flat: Vec<FlatOutline>,
    /// Depth of the most recently seen heading (`None` until the first heading),
    /// so a following bookmark can nest under it.
    last_heading_level: Option<usize>,
}

impl OutlineBuilder {
    /// Record a heading at 0-based outline `level` landing on `page`. Blank
    /// titles are skipped (an empty TOC entry is never useful).
    fn push_heading(&mut self, title: String, level: usize, page: usize) {
        let title = title.trim().to_string();
        self.last_heading_level = Some(level);
        if title.is_empty() {
            return;
        }
        self.flat.push(FlatOutline { title, level, page });
    }

    /// Record a bookmark anchor named `name` at `page`, nested under the current
    /// section (one level deeper than the last heading, or top-level if none).
    /// Word-internal bookmarks (`_GoBack`, `_Toc…`, `_Ref…`, `_Hlk…`, any
    /// `_`-prefixed name) are skipped — they are machinery, not navigation.
    fn push_bookmark(&mut self, name: &str, page: usize) {
        let name = name.trim();
        if name.is_empty() || !is_user_bookmark(name) {
            return;
        }
        let level = self.last_heading_level.map(|l| l + 1).unwrap_or(0);
        self.flat.push(FlatOutline {
            title: name.to_string(),
            level,
            page,
        });
    }

    /// Fold the collected flat entries into the nested outline tree.
    fn finish(self) -> Vec<crate::model::OutlineNode> {
        crate::recon::fold_outline(&self.flat)
    }
}

/// A bookmark is user-defined navigation (kept in the outline) unless it carries
/// a leading underscore, which Word/​Writer reserve for generated bookmarks
/// (`_GoBack`, `_Toc12345`, `_Ref…`, `_Hlk…`). Such machinery anchors are dropped
/// so they don't pollute the outline tree.
fn is_user_bookmark(name: &str) -> bool {
    !name.starts_with('_')
}

/// Flatten a slice of model [`Inline`]s into plain text (run text + link-child
/// text), rendering line breaks as spaces. Used to title an outline entry from
/// the heading runs already built during the walk.
fn inlines_plain_text(inlines: &[Inline]) -> String {
    let mut s = String::new();
    for inline in inlines {
        match inline {
            Inline::Run(run) => s.push_str(&run.text),
            Inline::LineBreak => s.push(' '),
            Inline::Link { children, .. } => {
                for c in children {
                    if let Inline::Run(run) = c {
                        s.push_str(&run.text);
                    }
                }
            }
            Inline::Image(_) => {}
        }
    }
    s.trim().to_string()
}

/// Map a DOCX heading signal to a 0-based outline depth. A `w:pStyle` heading
/// level (`Heading1`/`Title`/… → 1-based, via [`heading_level`]) and an explicit
/// `w:outlineLvl@w:val` (already 0-based, `0`=top) may both be present; the more
/// prominent (smaller) depth wins. Returns `None` when the paragraph is not an
/// outline entry.
fn docx_outline_level(style_level: Option<u8>, outline_lvl: Option<u32>) -> Option<usize> {
    let from_style = style_level.map(|l| (l.max(1) as usize) - 1);
    let from_lvl = outline_lvl.map(|v| v as usize);
    match (from_style, from_lvl) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

// ─────────────────────────────── DOCX → model ─────────────────────────────────

/// DOCX → [`Document`]: headings/paragraphs/lists/tables as model blocks.
/// Reuses the DOCX relationship/style/numbering parsers and the same `w:body`
/// grammar as [`docx_to_pdf`]; the per-paragraph run properties become
/// [`InlineRun`]s instead of `<span>`s.
pub fn docx_to_model(zip: &BTreeMap<String, Vec<u8>>) -> Document {
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
        page_h: geom.h,
    };

    let mut pages = DocxPages::new();
    let mut counters = ListCounters::default();
    let mut resources: BTreeMap<u64, model::ImageResource> = BTreeMap::new();
    let mut outline = OutlineBuilder::default();
    docx_walk_model(
        &mut Xml::new(&doc),
        &ctx,
        &mut pages,
        &mut counters,
        &mut resources,
        &mut outline,
        None,
    );
    let mut document = flow_document_pages(pages.finish(), page_geometry(geom));
    document.resources.images = resources;
    document.outline = outline.finish();
    // Lower `word/styles.xml`'s named paragraph styles into the model's style
    // table so each paragraph's `style_ref` (set from `w:pStyle`) resolves.
    document.styles = styles.to_style_table();
    document.meta = ooxml_doc_meta(zip);
    document
}

/// Wrap pre-split flow [`Page`]s in a one-section [`Document`] with the given
/// page geometry. Like [`flow_document`] but for a body that carried hard page
/// breaks (so it lowered to several pages). An empty list yields a single empty
/// page so the document always has at least one page.
fn flow_document_pages(pages: Vec<model::Page>, geom: PageGeometry) -> Document {
    let pages = if pages.is_empty() {
        vec![model::Page {
            blocks: Vec::new(),
            absolute: false,
        }]
    } else {
        pages
    };
    Document {
        sections: vec![Section {
            geometry: geom,
            header: None,
            footer: None,
            pages,
        }],
        ..Document::default()
    }
}

/// Accumulates the model blocks of a flowing DOCX body, split into [`Page`]s at
/// hard page breaks (`w:br w:type="page"`, `w:pPr/w:pageBreakBefore`, an
/// intermediate `w:pPr/w:sectPr`). The model represents a forced page break as a
/// section with several `Page`s, so each break opens a fresh page here; the
/// rasteriser then starts a new physical page per [`Page`]. There is always at
/// least one (possibly empty) open page.
#[derive(Default)]
struct DocxPages {
    pages: Vec<Vec<Block>>,
}

impl DocxPages {
    fn new() -> Self {
        DocxPages {
            pages: vec![Vec::new()],
        }
    }

    /// Blocks of the page currently being filled.
    fn cur(&mut self) -> &mut Vec<Block> {
        // Invariant: `pages` is never empty (seeded by `new`, restored by `break_page`).
        self.pages.last_mut().expect("at least one open page")
    }

    /// Zero-based index of the page currently being filled. Used to target
    /// outline entries (a heading/bookmark lands on the open page). A trailing
    /// empty page may be dropped by [`finish`](DocxPages::finish), but headings
    /// never sit on such a page (they add content), so the index stays valid.
    fn page_index(&self) -> usize {
        // Invariant: `pages` is never empty, so `len() >= 1`.
        self.pages.len().saturating_sub(1)
    }

    /// Start a new page boundary — but only if the current page already has
    /// content, so a leading break (or two consecutive breaks) can't inject a
    /// spurious blank page.
    fn break_page(&mut self) {
        if !self.cur().is_empty() {
            self.pages.push(Vec::new());
        }
    }

    /// Finalise into model [`Page`]s, dropping a trailing empty page left by a
    /// break at the very end of the body.
    fn finish(mut self) -> Vec<model::Page> {
        if self.pages.len() > 1 {
            if let Some(last) = self.pages.last() {
                if last.is_empty() {
                    self.pages.pop();
                }
            }
        }
        self.pages
            .into_iter()
            .map(|blocks| model::Page {
                blocks,
                absolute: false,
            })
            .collect()
    }
}

/// Recursive DOCX model walker (mirrors [`docx_walk`]). Emits `w:p`→paragraph/
/// heading/list-item blocks and `w:tbl`→[`Table`] blocks into `pages`, opening a
/// new page at each hard page break a paragraph signals.
fn docx_walk_model(
    x: &mut Xml,
    ctx: &DocxCtx,
    pages: &mut DocxPages,
    counters: &mut ListCounters,
    resources: &mut BTreeMap<u64, model::ImageResource>,
    outline: &mut OutlineBuilder,
    stop: Option<&str>,
) {
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => {
                let ln = local(&name);
                if ln == "p" && !sc {
                    docx_paragraph_model(x, ctx, pages, counters, resources, outline);
                } else if ln == "tbl" && !sc {
                    let table = docx_table_model(x, ctx, resources);
                    pages.cur().push(Block {
                        kind: BlockKind::Table(table),
                        ..Block::default()
                    });
                } else if ln == "bookmarkStart" {
                    // A bookmark anchored between paragraphs (body-level): record
                    // it against the current page so an internal `#name` link
                    // resolves. Bookmarks inside a `w:p` are handled by
                    // `docx_paragraph_model`.
                    if let Some(name) = attr(&attrs, "name") {
                        let page = pages.page_index();
                        outline.push_bookmark(name, page);
                    }
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

/// Map a collected [`ParaStyle`] (+ its resolved named style) to a model
/// [`ParagraphStyle`]. Alignment and line-height are translated; spacing/indents
/// carry over in points.
fn para_style_model(para: &ParaStyle) -> ParagraphStyle {
    let align = match para.align {
        Some("center") => MAlign::Center,
        Some("right") => MAlign::Right,
        Some("justify") => MAlign::Justify,
        _ => MAlign::Left,
    };
    let line_height = match para.line_height {
        Some(LineHeight::Multiple(m)) => MLineHeight::Multiple(m),
        Some(LineHeight::Points(p)) => MLineHeight::Points(p),
        None => MLineHeight::Normal,
    };
    // List indent stacks on top of any explicit left indent (mirrors style_attr).
    let list_indent = para
        .list_level
        .map(|lvl| (lvl as f64 + 1.0) * LIST_LEVEL_INDENT_PT)
        .unwrap_or(0.0);
    ParagraphStyle {
        align,
        space_before_pt: para.space_before_pt.unwrap_or(0.0),
        space_after_pt: para.space_after_pt.unwrap_or(0.0),
        indent_left_pt: para.indent_left_pt.unwrap_or(0.0) + list_indent,
        indent_right_pt: para.indent_right_pt.unwrap_or(0.0),
        first_line_pt: para.first_line_pt.unwrap_or(0.0),
        line_height,
    }
}

/// Emit one `w:p` (open already consumed) as a model block: a [`Heading`] when
/// the paragraph carries a heading style, a list-item-wrapped paragraph (kept as
/// a one-item [`List`] so the marker/ordinal is preserved) for `w:numPr`
/// paragraphs, else a plain [`Paragraph`]. Mirrors [`docx_paragraph`] but builds
/// [`Inline`] runs.
fn docx_paragraph_model(
    x: &mut Xml,
    ctx: &DocxCtx,
    pages: &mut DocxPages,
    counters: &mut ListCounters,
    resources: &mut BTreeMap<u64, model::ImageResource>,
    outline: &mut OutlineBuilder,
) {
    let mut heading: Option<u8> = None;
    // Floating drawings (`wp:anchor`) lifted out of the run flow: each becomes a
    // sibling `Block { kind: Image, frame: Some(Rect) }` flushed after this
    // paragraph's own block (a float can't sit mid-paragraph in the model).
    let mut floating: Vec<Block> = Vec::new();
    // `w:pPr/w:outlineLvl@w:val` (0-based, `0`=top): an outline level set
    // independently of any heading style. Folded into the document outline even
    // when the paragraph carries no heading `w:pStyle`.
    let mut outline_lvl: Option<u32> = None;
    // Bookmarks opened within this paragraph (`w:bookmarkStart@w:name`): recorded
    // as outline anchors once the page this paragraph lands on is known.
    let mut bookmarks: Vec<String> = Vec::new();
    let mut style_id: Option<String> = None;
    let mut runs: Vec<Inline> = Vec::new();
    let mut run = RunStyle::default();
    let mut para = ParaStyle::default();
    let mut num_ref = NumRef::default();
    let mut in_rpr = false;
    let mut in_ppr = false;
    let mut depth = 0i32;
    // `w:pPr/w:pageBreakBefore` → this paragraph opens a fresh page.
    let mut page_break_before = false;
    // A run-level `<w:br w:type="page"/>` or an intermediate `w:pPr/w:sectPr` →
    // a fresh page *after* this paragraph (mirrors the HTML render path).
    let mut page_break_after = false;
    // Open `<w:hyperlink>`: runs pushed while set are collected here and wrapped
    // in an `Inline::Link` on the matching close (DOCX hyperlinks don't nest).
    let mut link: Option<DocxLink> = None;

    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => {
                let ln = local(&name);
                match ln {
                    "pPr" if !sc => in_ppr = true,
                    "pageBreakBefore" if in_ppr => {
                        // `w:val="0"`/`"false"` cancels an inherited page break.
                        page_break_before =
                            !matches!(attr(&attrs, "val"), Some("0") | Some("false"));
                    }
                    "sectPr" if in_ppr => {
                        // A section break carried on a paragraph (`w:pPr/w:sectPr`)
                        // ends a section: the following content starts a new page.
                        // The document's final `w:sectPr` is a direct `w:body`
                        // child (not here), so this never adds a trailing page.
                        page_break_after = true;
                    }
                    "rPr" if !sc => in_rpr = true,
                    "pStyle" => {
                        if in_ppr {
                            if let Some(v) = attr(&attrs, "val") {
                                heading = heading_level(v);
                                style_id = Some(v.to_string());
                            }
                        }
                    }
                    "outlineLvl" if in_ppr => {
                        // `w:val` is the 0-based outline level (0..8). A
                        // `9` "body text" value (no outline) is dropped.
                        outline_lvl = attr(&attrs, "val")
                            .and_then(|v| v.trim().parse::<i32>().ok())
                            .filter(|&v| (0..=8).contains(&v))
                            .map(|v| v as u32);
                    }
                    // A bookmark start inside the paragraph: capture its name so an
                    // internal `#name` link can resolve to this paragraph's page.
                    "bookmarkStart" => {
                        if let Some(name) = attr(&attrs, "name") {
                            bookmarks.push(name.to_string());
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
                    "strike" | "dstrike" if in_rpr => {
                        run.strike = !matches!(attr(&attrs, "val"), Some("0") | Some("false"))
                    }
                    "highlight" if in_rpr => {
                        // `w:highlight@val` is a named colour (yellow/green/…);
                        // map it to hex, dropping `none`/unknown tokens.
                        run.highlight = attr(&attrs, "val")
                            .and_then(highlight_color)
                            .map(|h| h.to_string());
                    }
                    "shd" if in_rpr => {
                        // Run shading `w:shd@fill` (6-hex). `auto`/missing ⇒ none.
                        // Set only when `highlight` hasn't already claimed it.
                        if run.highlight.is_none() {
                            if let Some(v) = attr(&attrs, "fill") {
                                if v != "auto" && is_hex6(v) {
                                    run.highlight = Some(v.to_ascii_uppercase());
                                }
                            }
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
                    "tab" => push_run(active_inlines(&mut runs, &mut link), &run, " "),
                    "br" if matches!(attr(&attrs, "type"), Some("page")) => {
                        // An explicit run-level page break (`<w:br w:type="page"/>`):
                        // end the current line, then split onto a new page after
                        // this paragraph (the model splits at block boundaries, so
                        // the break lands between paragraphs — same as the HTML path).
                        active_inlines(&mut runs, &mut link).push(Inline::LineBreak);
                        page_break_after = true;
                    }
                    "br" | "cr" => active_inlines(&mut runs, &mut link).push(Inline::LineBreak),
                    // A hyperlink wraps its runs and points at a relationship
                    // (external URL via `r:id`) or an in-document `w:anchor`.
                    "hyperlink" if !sc => {
                        link = Some(DocxLink {
                            href: docx_link_target(ctx, &attrs),
                            children: Vec::new(),
                        });
                    }
                    // A drawing/picture. Reuse the same blip resolution + resource
                    // interning as the HTML path, now carrying the alt text and
                    // geometry. A `wp:inline` drawing joins the current run flow as
                    // an `Inline::Image` (alt set; an inline has no size slot — a
                    // model limitation). A floating `wp:anchor` is lifted to a
                    // sibling `Block` whose `frame` carries the size and (when a
                    // `wp:posOffset` is given) the absolute position.
                    "drawing" | "pict" | "object" if !sc => {
                        let tag = local(&name).to_string();
                        let DocxDrawingModel {
                            image,
                            anchored,
                            size,
                            off_x,
                            off_y,
                        } = docx_drawing_model(x, ctx, resources, &tag);
                        if let Some(img) = image {
                            if anchored {
                                let frame = docx_anchor_frame(size, off_x, off_y, ctx.page_h);
                                floating.push(Block {
                                    frame,
                                    kind: BlockKind::Image(img),
                                    ..Block::default()
                                });
                            } else {
                                active_inlines(&mut runs, &mut link).push(Inline::Image(img));
                            }
                        }
                    }
                    // A bare `a:blip` outside a `<w:drawing>` (legacy/VML).
                    "blip" => {
                        if let Some(img) = blip_image_ref(ctx, &attrs, resources) {
                            active_inlines(&mut runs, &mut link).push(Inline::Image(img));
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
                    "hyperlink" => {
                        // Close the link: fold its collected children into one
                        // `Inline::Link` appended to the top-level run flow.
                        if let Some(l) = link.take() {
                            if !l.children.is_empty() {
                                runs.push(Inline::Link {
                                    href: l.href,
                                    children: l.children,
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
            Tok::Text(t) => {
                if depth > 0 && !t.is_empty() {
                    push_run(active_inlines(&mut runs, &mut link), &run, &t);
                }
            }
        }
    }
    // A hyperlink left open at paragraph end (malformed input): flush its
    // children so no text is lost.
    if let Some(l) = link.take() {
        if !l.children.is_empty() {
            runs.push(Inline::Link {
                href: l.href,
                children: l.children,
            });
        }
    }

    let resolved = ctx.styles.effective(style_id.as_deref());
    para.apply_style_defaults(&resolved);
    let style = para_style_model(&para);

    let mut paragraph = Paragraph {
        style,
        style_ref: style_id.clone().map(model::StyleId),
        runs,
    };
    // Fold the resolved named style's run defaults under each run lacking them.
    apply_named_run_defaults(&mut paragraph.runs, &resolved);

    // This paragraph's outline depth, if any (a `Heading*`/`Title` style and/or
    // an explicit `w:outlineLvl`). Computed before the paragraph is moved into a
    // block so the title can be taken from its runs.
    let outline_entry = docx_outline_level(heading, outline_lvl)
        .map(|lvl| (inlines_plain_text(&paragraph.runs), lvl));

    // Build this paragraph's block (a one-item List for a numbered/bulleted
    // paragraph, else a Heading or Paragraph).
    let block = if let Some(level) = para.list_level {
        // A list paragraph: wrap as a one-item List so the marker/ordinal is
        // recorded (reusing the numbering resolution as in the HTML path).
        let (ordered, marker) = docx_list_marker(ctx, num_ref.num_id, level);
        if num_ref.num_id.is_some() {
            // Advance the running counter so ordinals are stable across the list.
            let _ = counters.next(num_ref.num_id.unwrap_or(0), level);
        }
        Block {
            kind: BlockKind::List(List {
                ordered,
                marker,
                items: vec![ListItem {
                    blocks: vec![Block {
                        kind: BlockKind::Paragraph(paragraph),
                        ..Block::default()
                    }],
                    level: level.min(u8::MAX as u32) as u8,
                }],
            }),
            ..Block::default()
        }
    } else {
        let kind = match heading {
            Some(level) => BlockKind::Heading(Heading {
                level,
                para: paragraph,
            }),
            None => BlockKind::Paragraph(paragraph),
        };
        Block {
            kind,
            ..Block::default()
        }
    };

    // A `w:pageBreakBefore` paragraph opens a new page *before* its content.
    if page_break_before {
        pages.break_page();
    }
    pages.cur().push(block);
    // Flush any floating drawings anchored in this paragraph as sibling blocks on
    // the same page (they were lifted out of the run flow above).
    for fb in floating {
        pages.cur().push(fb);
    }
    // Record this paragraph's outline contributions against the page it now sits
    // on: first the heading (so a following bookmark nests under it), then any
    // bookmarks opened in the paragraph.
    let page = pages.page_index();
    if let Some((title, level)) = outline_entry {
        outline.push_heading(title, level, page);
    }
    for name in &bookmarks {
        outline.push_bookmark(name, page);
    }
    // A run-level `<w:br w:type="page"/>` (or an intermediate `w:sectPr`) forces
    // the next page *after* this paragraph.
    if page_break_after {
        pages.break_page();
    }
}

/// An in-progress DOCX `<w:hyperlink>` while its runs are being collected: the
/// resolved target plus the inline children gathered until `</w:hyperlink>`.
struct DocxLink {
    href: model::LinkTarget,
    children: Vec<Inline>,
}

/// The inline buffer currently receiving runs: the open hyperlink's children
/// when one is being built, else the paragraph's top-level run list. Lets text /
/// images / breaks land inside a link without duplicating the push logic.
fn active_inlines<'a>(runs: &'a mut Vec<Inline>, link: &'a mut Option<DocxLink>) -> &'a mut Vec<Inline> {
    match link {
        Some(l) => &mut l.children,
        None => runs,
    }
}

/// Resolve a `<w:hyperlink>` to a model [`LinkTarget`]: an external URL via the
/// relationship `r:id` (the same `word/_rels` table the HTML/image path uses), or
/// an in-document jump for `w:anchor` (kept as page 0 — the model addresses pages,
/// not named bookmarks, so an internal anchor lands on the document start rather
/// than being dropped). Missing/blank ⇒ an empty URL.
fn docx_link_target(ctx: &DocxCtx, attrs: &[(String, String)]) -> model::LinkTarget {
    if let Some(rid) = attr(attrs, "id").filter(|v| !v.trim().is_empty()) {
        if let Some(target) = ctx.rels.get(rid) {
            return model::LinkTarget::Url(target.clone());
        }
    }
    if attr(attrs, "anchor").is_some_and(|a| !a.trim().is_empty()) {
        // In-document anchor: the model jumps by page index, so target the start.
        return model::LinkTarget::Page(0);
    }
    model::LinkTarget::Url(String::new())
}

/// Decode a supported image zip entry, intern its bytes in `resources` under a
/// content-hash key, and return an [`ImageRef`] to it (the inline-flow counterpart
/// of [`image_block`]). `None` for a missing or unsupported (vector/legacy) entry.
fn image_ref(
    zip: &BTreeMap<String, Vec<u8>>,
    key: &str,
    resources: &mut BTreeMap<u64, model::ImageResource>,
) -> Option<model::ImageRef> {
    let mime = image_mime(key)?;
    let bytes = zip.get(key)?.clone();
    let hash = fnv1a(&bytes);
    let format = mime.rsplit('/').next().unwrap_or("png").to_string();
    resources
        .entry(hash)
        .or_insert(model::ImageResource { bytes, format });
    Some(model::ImageRef {
        resource: hash,
        alt: None,
    })
}

/// Resolve an `a:blip@r:embed`/`@r:link` to an interned [`ImageRef`] via the
/// document relationships + media (the model counterpart of [`blip_img`]).
fn blip_image_ref(
    ctx: &DocxCtx,
    attrs: &[(String, String)],
    resources: &mut BTreeMap<u64, model::ImageResource>,
) -> Option<model::ImageRef> {
    let rid = attr(attrs, "embed").or_else(|| attr(attrs, "link"))?;
    let key = resolve_target("word", ctx.rels.get(rid)?);
    image_ref(ctx.zip, &key, resources)
}

/// A `<w:drawing>`/`<w:pict>`/`<w:object>` lowered for the model path. Carries the
/// interned picture plus the geometry/accessibility the model can represent.
///
/// What is lowered: the image (`a:blip`), the alt text (`wp:docPr@descr`, then
/// `@title` → [`model::ImageRef::alt`]), the on-page size (`wp:extent@cx/@cy`,
/// EMU→pt), and — for a floating `wp:anchor` — the absolute offset
/// (`wp:positionH/V/wp:posOffset`, EMU→pt, top-left origin).
///
/// What is **not** lowered (no model slot, out of scope to add): the wrap type
/// (`wp:wrapSquare`/`wrapTight`/`wrapTopAndBottom`/`wrapNone`/`wrapThrough`),
/// the z-order flag (`@behindDoc`), and the `@relativeFrom` anchor reference —
/// [`model::Block::frame`] is a single absolute [`model::Rect`] with no wrap,
/// z-order, or anchor-reference field. A `wp:align` keyword (no `wp:posOffset`)
/// also has no absolute coordinate at this layer, so only the size is taken then.
#[derive(Default)]
struct DocxDrawingModel {
    /// The interned picture, if a resolvable `a:blip` was present.
    image: Option<model::ImageRef>,
    /// `wp:anchor` seen ⇒ a floating object (vs an in-flow `wp:inline`).
    anchored: bool,
    /// `wp:extent@cx/@cy` in points (the drawing's on-page footprint).
    size: Option<(f64, f64)>,
    /// `wp:positionH/wp:posOffset` in points (top-left origin); `None` for `wp:align`.
    off_x: Option<f64>,
    /// `wp:positionV/wp:posOffset` in points (top-left origin); `None` for `wp:align`.
    off_y: Option<f64>,
}

/// Consume a `<w:drawing>`/`<w:pict>`/`<w:object>` subtree (its open tag already
/// seen) up to its matching close and resolve it for the model. Mirrors
/// [`docx_drawing`] (the HTML path) but lowers into [`DocxDrawingModel`]: the
/// first `a:blip` is interned, `wp:docPr@descr`/`@title` becomes the alt text,
/// `wp:extent` the size, and a `wp:anchor`'s `wp:posOffset` the absolute offset.
/// `stop` is the local name of the enclosing element so the right close ends the
/// scan. The caller decides inline (`Inline::Image`) vs floating (`Block.frame`).
fn docx_drawing_model(
    x: &mut Xml,
    ctx: &DocxCtx,
    resources: &mut BTreeMap<u64, model::ImageResource>,
    stop: &str,
) -> DocxDrawingModel {
    let mut out = DocxDrawingModel::default();
    let mut alt: Option<String> = None;
    let mut size_w: Option<f64> = None;
    let mut size_h: Option<f64> = None;
    // Which axis a following `wp:posOffset` text node belongs to (set by the
    // enclosing `wp:positionH`/`wp:positionV`). `wp:align` has no offset value.
    let mut cur_axis: Option<bool> = None; // Some(true)=H, Some(false)=V
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, _) => match local(&name) {
                "anchor" => out.anchored = true,
                "extent" => {
                    // `wp:extent@cx/@cy` is the drawing's overall footprint (EMU).
                    size_w = attr(&attrs, "cx").and_then(emu_to_pt).or(size_w);
                    size_h = attr(&attrs, "cy").and_then(emu_to_pt).or(size_h);
                }
                // `wp:docPr@descr` is the accessibility alt text; `@title` is the
                // fallback. Mirrors the ODP/PPTX `ImageRef.alt` precedence.
                "docPr" => {
                    if alt.is_none() {
                        alt = pick_nonblank(attr(&attrs, "descr"))
                            .or_else(|| pick_nonblank(attr(&attrs, "title")));
                    }
                }
                "positionH" => cur_axis = Some(true),
                "positionV" => cur_axis = Some(false),
                "blip" if out.image.is_none() => {
                    out.image = blip_image_ref(ctx, &attrs, resources);
                }
                _ => {}
            },
            Tok::Close(name) => {
                if local(&name) == stop {
                    break;
                }
            }
            Tok::Text(t) => {
                // `wp:posOffset` carries its EMU value as a text node; route it to
                // the axis set by the enclosing `wp:positionH`/`V`. A `wp:align`
                // keyword (left/center/right/top/bottom) has no absolute offset, so
                // it is ignored here (only the size is representable then).
                if let Some(axis) = cur_axis {
                    if let Some(pts) = emu_to_pt(&t) {
                        match axis {
                            true => out.off_x = Some(pts),
                            false => out.off_y = Some(pts),
                        }
                    }
                }
            }
        }
    }
    out.size = match (size_w, size_h) {
        (Some(w), Some(h)) => Some((w, h)),
        _ => None,
    };
    if let Some(img) = out.image.as_mut() {
        img.alt = alt;
    }
    out
}

/// Trim an attribute value, returning `None` when absent or blank. Used to pick
/// the first non-empty alt-text candidate (`wp:docPr@descr`/`@title`).
fn pick_nonblank(v: Option<&str>) -> Option<String> {
    v.map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

/// Build the placement [`model::Rect`] for a floating DOCX drawing from its
/// `wp:extent` `size` (points) and `wp:posOffset` `off_x`/`off_y` (points,
/// top-left origin). The offset is treated as page-absolute (matching the HTML
/// path's `relativeFrom="page"/"margin"` simplification) and the Y axis is
/// flipped about `page_h` into the model's lower-left convention — the same
/// mapping the PPTX/form-field frames use. A missing offset defaults to `0`
/// (a `wp:align`-only anchor has no absolute coordinate at this layer), but the
/// size is still carried so the box reserves its real footprint. Returns `None`
/// only when neither a size nor an offset is known (nothing to place).
fn docx_anchor_frame(
    size: Option<(f64, f64)>,
    off_x: Option<f64>,
    off_y: Option<f64>,
    page_h: f64,
) -> Option<model::Rect> {
    if size.is_none() && off_x.is_none() && off_y.is_none() {
        return None;
    }
    let (w, h) = size.unwrap_or((0.0, 0.0));
    let x = off_x.unwrap_or(0.0);
    let y_top = off_y.unwrap_or(0.0);
    // Top-left (OOXML) → lower-left (model): flip Y about the page height.
    Some(model::Rect::new(x, page_h - (y_top + h), w, h))
}

/// Fill each run's unset character attributes from the resolved named style
/// (bold/italic/underline/size/colour/family), so a `Heading1`/`Quote`/… style
/// propagates its typography to runs that didn't restate it.
fn apply_named_run_defaults(runs: &mut [Inline], style: &DocxStyle) {
    for inline in runs.iter_mut() {
        if let Inline::Run(r) = inline {
            if !r.style.bold {
                r.style.bold = style.bold == Some(true);
            }
            if !r.style.italic {
                r.style.italic = style.italic == Some(true);
            }
            if !r.style.underline {
                r.style.underline = style.underline == Some(true);
            }
            if r.style.size_pt == 0.0 {
                if let Some(half) = style.size_half_pt {
                    r.style.size_pt = half / 2.0;
                }
            }
            if r.style.color.is_none() {
                r.style.color = style.color.as_deref().and_then(hex_to_rgb_f64);
            }
            if r.style.family.is_empty() {
                if let Some(fam) = &style.font_family {
                    r.style.family = fam.clone();
                    r.style.generic = super::style::parse_base_font(fam).generic;
                }
            }
        }
    }
}

/// Append `text` to `runs` as a styled [`InlineRun`], coalescing with the
/// previous run when it carries an identical style (keeps the run list compact).
fn push_run(runs: &mut Vec<Inline>, run: &RunStyle, text: &str) {
    let style = run_char_style(run);
    if let Some(Inline::Run(last)) = runs.last_mut() {
        if last.style == style {
            last.text.push_str(text);
            return;
        }
    }
    runs.push(Inline::Run(InlineRun {
        text: text.to_string(),
        style,
        source_index: None,
    }));
}

/// Append `text` to `runs`, honouring the run's optional [`hyperlink`](RunStyle::hyperlink):
/// a hyperlinked run becomes an [`Inline::Link`] (coalesced with a preceding link
/// to the same URL so a multi-run anchor stays one node); a plain run defers to
/// [`push_run`]. Used by the PPTX slide/table paths where `a:hlinkClick` lives on
/// the run (`a:rPr`), not a wrapping element.
fn push_run_maybe_linked(runs: &mut Vec<Inline>, run: &RunStyle, text: &str) {
    let Some(url) = run.hyperlink.as_deref() else {
        push_run(runs, run, text);
        return;
    };
    let href = model::LinkTarget::Url(url.to_string());
    let style = run_char_style(run);
    if let Some(Inline::Link { href: h, children }) = runs.last_mut() {
        if *h == href {
            push_run(children, run, text);
            return;
        }
    }
    runs.push(Inline::Link {
        href,
        children: vec![Inline::Run(InlineRun {
            text: text.to_string(),
            style,
            source_index: None,
        })],
    });
}

/// Resolve a DOCX list paragraph's `(ordered, marker)` for the model: the
/// numbering format from `numbering.xml` maps to a [`ListMarker`]; bullet/unknown
/// → an unordered bullet. Reuses [`DocxNumbering::fmt`] and [`NumFmt`].
fn docx_list_marker(ctx: &DocxCtx, num_id: Option<u32>, level: u32) -> (bool, ListMarker) {
    match num_id.and_then(|nid| ctx.numbering.fmt(nid, level)) {
        Some(NumFmt::Decimal) => (true, ListMarker::Decimal),
        Some(NumFmt::LowerLetter) => (true, ListMarker::LowerAlpha),
        Some(NumFmt::UpperLetter) => (true, ListMarker::UpperAlpha),
        Some(NumFmt::LowerRoman) => (true, ListMarker::LowerRoman),
        Some(NumFmt::UpperRoman) => (true, ListMarker::UpperRoman),
        _ => (false, ListMarker::Bullet('\u{2022}')),
    }
}

/// Emit one `w:tbl` (open already consumed) as a model [`Table`], honouring
/// `w:tblGrid` column widths and `w:gridSpan`/`w:vMerge` cell merges via
/// [`Cell::col_span`]/[`Cell::row_span`]. Mirrors [`docx_table`].
fn docx_table_model(
    x: &mut Xml,
    ctx: &DocxCtx,
    resources: &mut BTreeMap<u64, model::ImageResource>,
) -> Table {
    let mut col_widths: Vec<f64> = Vec::new();
    let mut rows: Vec<Row> = Vec::new();
    let mut cur_row: Option<Vec<Cell>> = None;
    // Height for the row currently being read, from `w:trPr/w:trHeight@w:val`
    // (twips → points). Reset at every `w:tr`.
    let mut cur_height: Option<f64> = None;
    // The model carries one table-wide border. A `w:tblBorders` side seeds it
    // first; failing that, the first cell `w:tcBorders` edge does (mirrors the
    // PPTX path). The first declared width wins.
    let mut border: Option<model::BorderStyle> = None;
    // Track the relevant `w:tblPr` subtrees so a `w:tblBorders` side is not
    // confused with an identically-named child elsewhere (e.g. `w:tcBorders`).
    let mut in_tblpr = false;
    let mut in_tblborders = false;
    let mut in_trpr = false;

    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => {
                let ln = local(&name);
                match ln {
                    "tblPr" if !sc => in_tblpr = true,
                    "tblBorders" if in_tblpr && !sc => in_tblborders = true,
                    // Table border sides (`w:top/left/bottom/right/insideH/
                    // insideV`). The first side that declares a real width seeds
                    // the model's single table-wide border.
                    "top" | "left" | "bottom" | "right" | "insideH" | "insideV"
                        if in_tblborders =>
                    {
                        if border.is_none() {
                            if let Some(side) = parse_border_side(&attrs) {
                                border = Some(border_side_to_model(&side));
                            }
                        }
                    }
                    "gridCol" => {
                        if let Some(w) = attr(&attrs, "w").and_then(twips_to_pt) {
                            if w > 0.0 {
                                col_widths.push(w);
                            }
                        }
                    }
                    "tr" if !sc => {
                        cur_row = Some(Vec::new());
                        cur_height = None;
                    }
                    "trPr" if !sc => in_trpr = true,
                    // Row height `w:trHeight@w:val` (twips → points). Word's
                    // `hRule` ("atLeast"/"exact") only affects min-vs-fixed; the
                    // model keeps a single height, so the value carries over.
                    "trHeight" if in_trpr => {
                        cur_height = attr(&attrs, "val").and_then(twips_to_pt).or(cur_height);
                    }
                    "tc" if !sc => {
                        let out = docx_cell_model(x, ctx, resources);
                        if border.is_none() {
                            border = out.border;
                        }
                        if let (Some(row), Some(cell)) = (cur_row.as_mut(), out.cell) {
                            row.push(cell);
                        }
                    }
                    _ => {}
                }
            }
            Tok::Close(name) => {
                let ln = local(&name);
                match ln {
                    "tblBorders" => in_tblborders = false,
                    "tblPr" => in_tblpr = false,
                    "trPr" => in_trpr = false,
                    "tr" => {
                        if let Some(cells) = cur_row.take() {
                            rows.push(Row {
                                cells,
                                height: cur_height.take(),
                            });
                        }
                    }
                    "tbl" => break,
                    _ => {}
                }
            }
            Tok::Text(_) => {}
        }
    }

    Table {
        rows,
        col_widths,
        border: border.unwrap_or_default(),
    }
}

/// One lowered DOCX `w:tc`: the model [`Cell`] (`None` for a vertical-merge
/// continuation, covered by the restart cell above) plus the border its
/// `w:tcBorders` declared. The model holds a single table-wide [`BorderStyle`],
/// so a cell edge is surfaced for the table to seed (mirrors [`PptxCellOut`]); a
/// continuation cell still surfaces its border.
struct DocxCellOut {
    cell: Option<Cell>,
    border: Option<model::BorderStyle>,
}

/// Emit one `w:tc` cell (open already consumed) as a [`DocxCellOut`].
/// `w:gridSpan`→`col_span`, `w:vMerge="restart"`→`row_span = 2`,
/// `w:tcPr/w:shd@w:fill`→[`Cell::shading`], and the first `w:tcBorders` edge →
/// the surfaced [`model::BorderStyle`]. Mirrors [`docx_cell`].
fn docx_cell_model(
    x: &mut Xml,
    ctx: &DocxCtx,
    resources: &mut BTreeMap<u64, model::ImageResource>,
) -> DocxCellOut {
    let mut span = CellSpan::default();
    let mut in_tcpr = false;
    let mut in_tcborders = false;
    // A cell can't span pages, so any page break a paragraph signals collapses
    // when these pages are flattened back into the cell's block list at the end.
    let mut pages = DocxPages::new();
    let mut counters = ListCounters::default();
    // Cell paragraphs are not part of the document outline; their headings/
    // bookmarks go to a discarded builder so the top-level outline stays clean.
    let mut cell_outline = OutlineBuilder::default();
    // Cell background `w:tcPr/w:shd@w:fill` (6-hex), and the first `w:tcBorders`
    // edge that declares a real width (surfaced to seed the table-wide border).
    let mut shading: Option<[f64; 3]> = None;
    let mut border: Option<model::BorderStyle> = None;

    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => {
                let ln = local(&name);
                match ln {
                    "tcPr" if !sc => in_tcpr = true,
                    "tcBorders" if in_tcpr && !sc => in_tcborders = true,
                    // Cell border sides — the first real width seeds the surfaced
                    // border (the model has no per-cell border slot).
                    "top" | "left" | "bottom" | "right" | "insideH" | "insideV" if in_tcborders => {
                        if border.is_none() {
                            if let Some(side) = parse_border_side(&attrs) {
                                border = Some(border_side_to_model(&side));
                            }
                        }
                    }
                    "gridSpan" if in_tcpr => {
                        span.grid_span = attr(&attrs, "val")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                    }
                    "vMerge" if in_tcpr => match attr(&attrs, "val") {
                        Some("restart") => span.v_merge_restart = true,
                        _ => span.v_merge_continue = true,
                    },
                    // Cell shading `w:tcPr/w:shd@w:fill` → background. `auto`/
                    // missing leaves it unset. Guarded to the cell's own `w:tcPr`
                    // so a run/paragraph `w:shd` inside the cell never leaks here.
                    "shd" if in_tcpr => {
                        if let Some(v) = attr(&attrs, "fill") {
                            if v != "auto" && is_hex6(v) {
                                shading = hex_to_rgb_f64(v);
                            }
                        }
                    }
                    "p" if !sc => docx_paragraph_model(
                        x,
                        ctx,
                        &mut pages,
                        &mut counters,
                        resources,
                        &mut cell_outline,
                    ),
                    "tbl" if !sc => {
                        let table = docx_table_model(x, ctx, resources);
                        pages.cur().push(Block {
                            kind: BlockKind::Table(table),
                            ..Block::default()
                        });
                    }
                    _ => {}
                }
            }
            Tok::Close(name) => {
                let ln = local(&name);
                if ln == "tcBorders" {
                    in_tcborders = false;
                } else if ln == "tcPr" {
                    in_tcpr = false;
                } else if ln == "tc" {
                    break;
                }
            }
            Tok::Text(_) => {}
        }
    }

    if span.v_merge_continue {
        return DocxCellOut { cell: None, border };
    }
    // Collapse any intra-cell page breaks: concatenate the (degenerate) pages
    // back into one block list — a table cell is a single flow.
    let blocks: Vec<Block> = pages.finish().into_iter().flat_map(|p| p.blocks).collect();
    DocxCellOut {
        cell: Some(Cell {
            blocks,
            col_span: span.grid_span.max(1).min(u16::MAX as usize) as u16,
            row_span: if span.v_merge_restart { 2 } else { 1 },
            shading,
        }),
        border,
    }
}

// ─────────────────────────────── XLSX → model ─────────────────────────────────

/// XLSX → [`Document`] holding one [`BlockKind::Sheet`] with all worksheets.
/// Reuses the shared-strings, theme, style (fills + number formats), sheet-name
/// and merge parsers; cells become typed [`CellValue`]s rather than HTML.
pub fn xlsx_to_model(zip: &BTreeMap<String, Vec<u8>>) -> Document {
    let shared = zip
        .get("xl/sharedStrings.xml")
        .map(|b| parse_shared_strings(&String::from_utf8_lossy(b)))
        .unwrap_or_default();
    let theme = xlsx_theme(zip);
    let styles = zip
        .get("xl/styles.xml")
        .map(|b| parse_xlsx_styles(&String::from_utf8_lossy(b), &theme))
        .unwrap_or_default();
    let names = zip
        .get("xl/workbook.xml")
        .map(|b| parse_sheet_names(&String::from_utf8_lossy(b)))
        .unwrap_or_default();

    let mut sheet_parts: Vec<(usize, String)> = zip
        .iter()
        .filter(|(k, _)| k.starts_with("xl/worksheets/sheet") && k.ends_with(".xml"))
        .filter_map(|(k, v)| {
            let n: usize = k["xl/worksheets/sheet".len()..k.len() - 4].parse().ok()?;
            Some((n, String::from_utf8_lossy(v).into_owned()))
        })
        .collect();
    sheet_parts.sort_by_key(|(n, _)| *n);

    let mut sheets = Vec::new();
    for (idx, (n, xml)) in sheet_parts.iter().enumerate() {
        let name = names
            .get(idx)
            .cloned()
            .unwrap_or_else(|| format!("Sheet {n}"));
        // Worksheet relationships resolve `<hyperlink r:id>` targets to URLs.
        let rels = zip
            .get(&format!("xl/worksheets/_rels/sheet{n}.xml.rels"))
            .map(|b| parse_rels(&String::from_utf8_lossy(b)))
            .unwrap_or_default();
        sheets.push(xlsx_sheet_model(name, xml, &shared, &styles, &rels));
    }

    let block = Block {
        kind: BlockKind::Sheet(SheetBlock { sheets }),
        ..Block::default()
    };
    let mut doc = flow_document(vec![block], page_geometry(PageGeom::tabular_default()));
    doc.meta = ooxml_doc_meta(zip);
    doc
}

/// Build one model [`Sheet`] from a worksheet XML, reusing [`parse_merges`] and
/// the cell type/format/fill resolution from [`xlsx_sheet_table`] — but storing
/// typed [`CellValue`]s (`Number`/`Text`/`Bool`/`Empty`), per-cell
/// `number_format`, fill, font/border/alignment (from `styles`), expanded
/// shared formulas, cell hyperlinks (resolved via `rels`), plus the merge ranges.
fn xlsx_sheet_model(
    name: String,
    xml: &str,
    shared: &[String],
    styles: &XlsxStyles,
    rels: &BTreeMap<String, String>,
) -> Sheet {
    // Reuse the worksheet merge parser; map the engine's tuple form to the
    // model's `MergeRange` struct (0-based inclusive corners).
    let merges: Vec<model::MergeRange> = parse_merges(xml)
        .into_iter()
        .map(|(r0, c0, r1, c1)| model::MergeRange { r0, c0, r1, c1 })
        .collect();
    let mut rows: Vec<SheetRow> = Vec::new();
    let mut x = Xml::new(xml);
    let mut in_sheet_data = false;

    let mut row_idx = 0usize;
    let mut next_auto_row = 0usize;
    // Per-row cells keyed by 0-based column for gap-filling.
    let mut row_cells: BTreeMap<usize, SheetCell> = BTreeMap::new();
    let mut row_open = false;

    let mut cell_col = 0usize;
    let mut cell_type = String::new();
    let mut cell_text = String::new();
    let mut cell_formula = String::new();
    let mut cell_bg: Option<[f64; 3]> = None;
    let mut cell_fmt: Option<String> = None;
    let mut cell_style: CellFmt = CellFmt::default();
    let mut in_cell = false;
    let mut in_value = false;
    let mut in_formula = false;
    // The current `<f>`'s shared-group index and whether it is a `t="shared"`
    // follower (empty body to be expanded from its master).
    let mut f_shared_si: Option<u32> = None;
    let mut f_is_shared_follower = false;
    // Shared-formula masters: `si` → (master formula, anchor row, anchor col).
    let mut shared_masters: BTreeMap<u32, (String, usize, usize)> = BTreeMap::new();
    // Cell hyperlinks (`<hyperlinks>` after `<sheetData>`): (row, col) → target.
    let mut hyperlinks: BTreeMap<(usize, usize), String> = BTreeMap::new();

    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => match local(&name) {
                "sheetData" => in_sheet_data = true,
                "row" if in_sheet_data && !sc => {
                    row_open = true;
                    row_cells.clear();
                    row_idx = attr(&attrs, "r")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .map(|n| n.saturating_sub(1))
                        .unwrap_or(next_auto_row);
                    next_auto_row = row_idx + 1;
                }
                "c" if in_sheet_data => {
                    in_cell = true;
                    cell_text.clear();
                    cell_formula.clear();
                    f_shared_si = None;
                    f_is_shared_follower = false;
                    cell_type = attr(&attrs, "t").unwrap_or("n").to_string();
                    cell_col = attr(&attrs, "r").map(col_of_ref).unwrap_or(0);
                    let style_idx = attr(&attrs, "s").and_then(|v| v.trim().parse::<usize>().ok());
                    cell_bg = style_idx
                        .and_then(|i| styles.fill(i))
                        .as_deref()
                        .and_then(hex_to_rgb_f64);
                    cell_fmt = style_idx
                        .and_then(|i| styles.num_fmt(i))
                        .map(|(_, code)| code.clone());
                    cell_style = style_idx
                        .and_then(|i| styles.fmt(i))
                        .cloned()
                        .unwrap_or_default();
                    if sc {
                        in_cell = false;
                    }
                }
                "v" | "t" if in_cell => in_value = true,
                // `<f>` carries the formula expression as its text body. A shared
                // master (`t="shared"` with a body) seeds the group; a self-
                // closing follower (`t="shared"`, no body) is expanded from it.
                "f" if in_cell => {
                    f_shared_si = attr(&attrs, "si").and_then(|v| v.trim().parse::<u32>().ok());
                    f_is_shared_follower = attr(&attrs, "t") == Some("shared");
                    if !sc {
                        in_formula = true;
                    }
                }
                // `<hyperlink>` lives in a `<hyperlinks>` block after sheetData;
                // record each `ref`'s target (external via rels `r:id`, else a
                // `#` in-workbook `location`).
                "hyperlink" if !in_sheet_data => {
                    if let Some(cell_ref) = attr(&attrs, "ref") {
                        if let Some(target) = hyperlink_target(&attrs, rels) {
                            hyperlinks.insert(cell_ref_to_rc(cell_ref), target);
                        }
                    }
                }
                _ => {}
            },
            Tok::Text(t) => {
                if in_cell && in_formula {
                    cell_formula.push_str(&t);
                } else if in_cell && in_value {
                    cell_text.push_str(&t);
                }
            }
            Tok::Close(name) => match local(&name) {
                "v" | "t" => in_value = false,
                "f" => in_formula = false,
                "c" => {
                    if in_cell {
                        let value = xlsx_cell_value(&cell_type, cell_text.trim(), shared);
                        let formula = resolve_cell_formula(
                            cell_formula.trim(),
                            f_shared_si,
                            f_is_shared_follower,
                            cell_col,
                            row_idx,
                            &mut shared_masters,
                        );
                        row_cells.insert(
                            cell_col,
                            SheetCell {
                                value,
                                formula,
                                number_format: cell_fmt.take(),
                                fill: cell_bg.take(),
                                style: cell_style.style.clone(),
                                border: cell_style.border,
                                align: cell_style.align,
                                wrap: cell_style.wrap,
                                ..Default::default()
                            },
                        );
                        cell_style = CellFmt::default();
                    }
                    in_cell = false;
                }
                "row" => {
                    if row_open {
                        rows.resize(row_idx, SheetRow::default());
                        let max_col = row_cells.keys().last().copied();
                        let cells = match max_col {
                            Some(max) => {
                                let mut v = Vec::with_capacity(max + 1);
                                for c in 0..=max {
                                    v.push(row_cells.remove(&c).unwrap_or_default());
                                }
                                v
                            }
                            None => Vec::new(),
                        };
                        if rows.len() == row_idx {
                            rows.push(SheetRow {
                                cells,
                                ..Default::default()
                            });
                        } else {
                            rows[row_idx] = SheetRow {
                                cells,
                                ..Default::default()
                            };
                        }
                        row_open = false;
                    }
                }
                "sheetData" => in_sheet_data = false,
                _ => {}
            },
        }
    }

    // Apply collected hyperlinks onto their cells (growing the grid if a link
    // points at a cell with no value).
    for ((r, c), target) in hyperlinks {
        if rows.len() <= r {
            rows.resize(r + 1, SheetRow::default());
        }
        let cells = &mut rows[r].cells;
        if cells.len() <= c {
            cells.resize(c + 1, SheetCell::default());
        }
        cells[c].hyperlink = Some(target);
    }

    Sheet {
        name,
        rows,
        merges,
        col_widths: Vec::new(),
    }
}

/// Resolve a cell's `<f>` into the stored formula expression, expanding shared
/// formulas. A master (`shared` with a body, or any plain body) is recorded in
/// `masters` under its `si` with its anchor `(col, row)` and returned verbatim.
/// A shared follower (empty body) is rebuilt from its master by translating the
/// master's relative cell references by the row/column delta. Returns `None` for
/// a literal cell (no formula, no resolvable master).
fn resolve_cell_formula(
    body: &str,
    si: Option<u32>,
    is_shared_follower: bool,
    col: usize,
    row: usize,
    masters: &mut BTreeMap<u32, (String, usize, usize)>,
) -> Option<String> {
    if !body.is_empty() {
        // A formula with a body. If it carries a shared `si`, it is the group's
        // master: remember it (with its anchor) so followers can be expanded.
        if let Some(si) = si {
            masters
                .entry(si)
                .or_insert_with(|| (body.to_string(), col, row));
        }
        return Some(body.to_string());
    }
    // Empty body: a shared follower. Expand from the master if we have it.
    if is_shared_follower {
        if let Some(si) = si {
            if let Some((master, anchor_col, anchor_row)) = masters.get(&si) {
                let dc = col as isize - *anchor_col as isize;
                let dr = row as isize - *anchor_row as isize;
                return Some(translate_formula_refs(master, dc, dr));
            }
        }
    }
    None
}

/// Resolve a `<hyperlink>`'s target: an external URL from the worksheet rels via
/// `r:id`, else an in-workbook `#location` (e.g. `#Sheet2!A1`). `None` when the
/// link resolves to nothing.
fn hyperlink_target(attrs: &[(String, String)], rels: &BTreeMap<String, String>) -> Option<String> {
    if let Some(id) = attr(attrs, "id") {
        if let Some(t) = rels.get(id) {
            return Some(t.clone());
        }
    }
    attr(attrs, "location").map(|loc| format!("#{loc}"))
}

/// Split a cell reference like `"B3"` / `"AB12"` into 0-based `(row, col)`.
/// Anything past the alphabetic prefix is the 1-based row; a missing/0 row maps
/// to row 0.
fn cell_ref_to_rc(r: &str) -> (usize, usize) {
    let col = col_of_ref(r);
    let row = r
        .trim_start_matches(|c: char| c.is_ascii_alphabetic())
        .parse::<usize>()
        .ok()
        .map(|n| n.saturating_sub(1))
        .unwrap_or(0);
    (row, col)
}

/// Translate every **relative** A1 cell reference in a formula expression by
/// `(dc, dr)` columns/rows. `$`-anchored components (absolute column and/or row)
/// are left unchanged, matching Excel's shared-formula expansion. References
/// inside a sheet qualifier (`Sheet1!A1`) and ranges (`A1:B2`) are handled token
/// by token; out-of-range results clamp to the first row/column.
fn translate_formula_refs(formula: &str, dc: isize, dr: isize) -> String {
    let bytes = formula.as_bytes();
    let mut out = String::with_capacity(formula.len());
    let mut i = 0usize;
    while i < bytes.len() {
        // A reference token begins with an optional `$` then a letter. To avoid
        // misreading function names (`SUM`) or sheet names, only treat a run as
        // a cell ref when it has the shape [$]?LETTERS[$]?DIGITS and is not the
        // tail of an identifier (preceded by a letter/digit/`_`/`!`/`.`/`'`).
        let c = bytes[i];
        let prev_ident = i > 0 && {
            let p = bytes[i - 1];
            p.is_ascii_alphanumeric() || matches!(p, b'_' | b'!' | b'.' | b'\'')
        };
        if (c == b'$' || c.is_ascii_alphabetic()) && !prev_ident {
            if let Some((end, col_abs, col, row_abs, row)) = parse_a1_ref(bytes, i) {
                let new_col = if col_abs {
                    col
                } else {
                    (col as isize + dc).max(0) as usize
                };
                let new_row = if row_abs {
                    row
                } else {
                    (row as isize + dr).max(0) as usize
                };
                if col_abs {
                    out.push('$');
                }
                out.push_str(&col_to_letters(new_col));
                if row_abs {
                    out.push('$');
                }
                out.push_str(&(new_row + 1).to_string());
                i = end;
                continue;
            }
        }
        out.push(c as char);
        i += 1;
    }
    out
}

/// Parse an A1 cell reference starting at `start` in `bytes`, returning
/// `(end_index, col_absolute, col0, row_absolute, row0)` on success — where the
/// column/row are 0-based and the `*_absolute` flags mark a leading `$`. `None`
/// when the run is not a `[$]?LETTERS[$]?DIGITS` cell reference.
fn parse_a1_ref(bytes: &[u8], start: usize) -> Option<(usize, bool, usize, bool, usize)> {
    let mut i = start;
    let col_abs = bytes.get(i) == Some(&b'$');
    if col_abs {
        i += 1;
    }
    let letters_start = i;
    while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
        i += 1;
    }
    let letters_end = i;
    if letters_end == letters_start {
        return None;
    }
    let row_abs = bytes.get(i) == Some(&b'$');
    if row_abs {
        i += 1;
    }
    let digits_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == digits_start {
        return None;
    }
    // The token must not run into a further identifier char (`A1B` is a name).
    if let Some(&n) = bytes.get(i) {
        if n.is_ascii_alphabetic() || n == b'_' {
            return None;
        }
    }
    let letters = std::str::from_utf8(&bytes[letters_start..letters_end]).ok()?;
    let col0 = col_of_ref(letters);
    let row0 = std::str::from_utf8(&bytes[digits_start..i])
        .ok()?
        .parse::<usize>()
        .ok()?
        .checked_sub(1)?;
    Some((i, col_abs, col0, row_abs, row0))
}

/// Convert a 0-based column index to its A1 letters (`0`→`A`, `26`→`AA`).
fn col_to_letters(mut col: usize) -> String {
    let mut s = Vec::new();
    loop {
        s.push(b'A' + (col % 26) as u8);
        if col < 26 {
            break;
        }
        col = col / 26 - 1;
    }
    s.reverse();
    String::from_utf8(s).unwrap_or_default()
}

/// Resolve one XLSX cell's typed value: shared-string index (`t="s"`),
/// inline/string text (`t="str"`/`t="inlineStr"`), boolean (`t="b"`), else a
/// parsed [`CellValue::Number`] (or text when unparseable). Empty input ⇒
/// [`CellValue::Empty`].
fn xlsx_cell_value(cell_type: &str, raw: &str, shared: &[String]) -> model::CellValue {
    use model::CellValue;
    match cell_type {
        "s" => raw
            .parse::<usize>()
            .ok()
            .and_then(|i| shared.get(i))
            .cloned()
            .map(CellValue::Text)
            .unwrap_or(CellValue::Empty),
        "b" => CellValue::Bool(raw == "1" || raw.eq_ignore_ascii_case("true")),
        "str" | "inlineStr" => {
            if raw.is_empty() {
                CellValue::Empty
            } else {
                CellValue::Text(raw.to_string())
            }
        }
        _ => {
            if raw.is_empty() {
                CellValue::Empty
            } else if let Ok(n) = raw.parse::<f64>() {
                CellValue::Number(n)
            } else {
                CellValue::Text(raw.to_string())
            }
        }
    }
}

// ─────────────────────────────── PPTX → model ─────────────────────────────────

/// PPTX → [`Document`] with one [`BlockKind::Slide`] holding every slide. Each
/// `a:sp` shape becomes a [`TextBox`] placeholder (role inferred from
/// `p:ph@type`), `a:p`/`a:r` runs become paragraphs, and `a:blip` images become
/// [`Image`] shapes. Reuses the PPTX theme/geometry parsers and run props.
pub fn pptx_to_model(zip: &BTreeMap<String, Vec<u8>>) -> Document {
    let geom = pptx_page_geom(&part(zip, "ppt/presentation.xml"));
    let theme = pptx_theme(zip);
    let mut slide_parts: Vec<(usize, String)> = zip
        .iter()
        .filter(|(k, _)| k.starts_with("ppt/slides/slide") && k.ends_with(".xml"))
        .filter_map(|(k, v)| {
            let n: usize = k["ppt/slides/slide".len()..k.len() - 4].parse().ok()?;
            Some((n, String::from_utf8_lossy(v).into_owned()))
        })
        .collect();
    slide_parts.sort_by_key(|(n, _)| *n);

    let mut slides = Vec::new();
    let mut resources: BTreeMap<u64, model::ImageResource> = BTreeMap::new();
    for (n, xml) in &slide_parts {
        let rels = zip
            .get(&format!("ppt/slides/_rels/slide{n}.xml.rels"))
            .map(|b| parse_rels(&String::from_utf8_lossy(b)))
            .unwrap_or_default();
        slides.push(pptx_slide_model(
            xml,
            zip,
            &rels,
            &theme,
            geom,
            &mut resources,
            *n,
        ));
    }

    let block = Block {
        kind: BlockKind::Slide(SlideBlock { slides }),
        ..Block::default()
    };
    let mut doc = flow_document(vec![block], page_geometry(geom));
    doc.resources.images = resources;
    doc.meta = ooxml_doc_meta(zip);
    doc
}

/// Build one model [`Slide`] from a slide XML. Recursively walks the `p:spTree`,
/// composing group (`p:grpSp`) transforms onto descendants, and lowers each
/// shape (`p:sp`), picture (`p:pic`) and graphic frame (`p:graphicFrame` →
/// table / chart / SmartArt) to a model [`Block`] carrying its absolute
/// [`frame`](Block::frame)/[`rotation`](Block::rotation). Placeholder shapes
/// (title/body/…) keep their semantic role and a placeholder whose own shape
/// has no `a:xfrm` inherits geometry from the slide-layout → slide-master chain.
/// `n` is the 1-based slide number (to locate the layout part for inheritance).
fn pptx_slide_model(
    xml: &str,
    zip: &BTreeMap<String, Vec<u8>>,
    rels: &BTreeMap<String, String>,
    theme: &PptxTheme,
    geom: PageGeom,
    resources: &mut BTreeMap<u64, model::ImageResource>,
    n: usize,
) -> Slide {
    let inherit = PptxPlaceholderGeom::resolve(zip, rels, n);
    let mut acc = PptxSlideAcc::default();
    let mut background: Option<[f64; 3]> = None;
    let mut x = Xml::new(xml);
    // Read the slide background fill (`p:bg`, in the `p:cSld` preamble) then enter
    // the shape tree; the recursive walker consumes `p:spTree`'s children and any
    // nested groups. `p:bg` always precedes `p:spTree` in `p:cSld`.
    while let Some(tok) = x.next() {
        if let Tok::Open(name, _, sc) = &tok {
            match local(name) {
                "bg" if !sc => background = pptx_bg_color(&mut x, theme),
                "spTree" if !sc => {
                    pptx_walk_sptree(
                        &mut x,
                        zip,
                        rels,
                        theme,
                        geom,
                        resources,
                        &inherit,
                        IDENTITY_XFRM,
                        &mut acc,
                    );
                    break;
                }
                _ => {}
            }
        }
    }

    Slide {
        geometry: page_geometry(geom),
        shapes: acc.shapes,
        placeholders: acc.placeholders,
        notes: None,
        background,
    }
}

/// Resolve a slide background `p:bg` (open consumed) to a single RGB fill colour
/// for [`Slide::background`]. Handles the three forms that carry a concrete
/// colour: an explicit `p:bgPr/a:solidFill`, the **first stop** of a
/// `p:bgPr/a:gradFill` (the dominant visible colour), and a theme `p:bgRef`
/// whose own `a:srgbClr`/`a:schemeClr` colour overrides the indexed style. The
/// first `a:srgbClr`/`a:schemeClr` found inside the `p:bg` wins (which is the
/// first gradient stop, or the sole solid colour). Picture/tile/blip fills are
/// out of scope (→ `None`). Consumes up to `</p:bg>`.
fn pptx_bg_color(x: &mut Xml, theme: &PptxTheme) -> Option<[f64; 3]> {
    let mut color: Option<[f64; 3]> = None;
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, _) => match local(&name) {
                "srgbClr" if color.is_none() => {
                    color = attr(&attrs, "val")
                        .filter(|v| is_hex6(v))
                        .and_then(hex_to_rgb_f64);
                }
                "schemeClr" if color.is_none() => {
                    color = attr(&attrs, "val")
                        .and_then(|v| theme.resolve_scheme(v))
                        .as_deref()
                        .and_then(hex_to_rgb_f64);
                }
                _ => {}
            },
            Tok::Close(name) if local(&name) == "bg" => break,
            _ => {}
        }
    }
    color
}

/// Accumulated slide content: semantic placeholders vs free-floating shapes.
#[derive(Default)]
struct PptxSlideAcc {
    placeholders: Vec<model::Placeholder>,
    shapes: Vec<Block>,
}

/// A group's accumulated child→parent transform: the absolute box the group is
/// placed at (`off`/`ext`) plus the child coordinate space it declares
/// (`chOff`/`chExt`). Child offsets/extents are mapped through this so a grouped
/// shape lands at its real slide position. Composes for nested groups.
#[derive(Clone, Copy)]
struct GroupXfrm {
    /// Group placement on the parent surface (points, top-left origin).
    off_x: f64,
    off_y: f64,
    /// Group child coordinate-space origin (points).
    ch_off_x: f64,
    ch_off_y: f64,
    /// Multiplicative scale child→parent (`ext/chExt`), `1.0` when undeclared.
    scale_x: f64,
    scale_y: f64,
}

/// The identity transform (top-level shapes are already in slide coordinates).
const IDENTITY_XFRM: GroupXfrm = GroupXfrm {
    off_x: 0.0,
    off_y: 0.0,
    ch_off_x: 0.0,
    ch_off_y: 0.0,
    scale_x: 1.0,
    scale_y: 1.0,
};

impl GroupXfrm {
    /// Map a child point `(x, y)` (in the current child coordinate space) to the
    /// parent surface: subtract the child origin, scale, add the group offset.
    fn point(&self, x: f64, y: f64) -> (f64, f64) {
        (
            self.off_x + (x - self.ch_off_x) * self.scale_x,
            self.off_y + (y - self.ch_off_y) * self.scale_y,
        )
    }

    /// Map a child extent (width/height) to parent units (scale only).
    fn extent(&self, w: f64, h: f64) -> (f64, f64) {
        (w * self.scale_x, h * self.scale_y)
    }

    /// Compose this transform with a nested group's own `a:xfrm` so its children
    /// map straight to the outermost (slide) surface.
    fn compose(&self, inner: &XfrmBox, in_ch_off: (f64, f64), in_ch_ext: (f64, f64)) -> GroupXfrm {
        // Where the inner group sits on *our* surface, and how big.
        let (px, py) = self.point(inner.x, inner.y);
        let (pw, ph) = self.extent(inner.w, inner.h);
        let (cox, coy) = in_ch_off;
        let (cex, cey) = in_ch_ext;
        let sx = if cex > 0.0 { pw / cex } else { self.scale_x };
        let sy = if cey > 0.0 { ph / cey } else { self.scale_y };
        GroupXfrm {
            off_x: px,
            off_y: py,
            ch_off_x: cox,
            ch_off_y: coy,
            scale_x: sx,
            scale_y: sy,
        }
    }
}

/// Convert a shape's local `a:xfrm` box, mapped through the active group
/// transform `g`, into the model's lower-left-origin [`Rect`] (slide height
/// `slide_h`) plus a [`Rotation`](model::Rotation). `None` rect when the box is
/// degenerate (no usable `a:off`+`a:ext`) → caller falls back to flow / inherited
/// geometry. OOXML rotation is clockwise (60000ths of a degree); the model's
/// [`Rotation::Deg`] is counter-clockwise, so the sign is negated and the exact
/// cardinal angles map to the first-class variants.
///
/// Mirroring: the model carries a rotation but no reflection, so a combined
/// `@flipH` **and** `@flipV` (a point reflection = a 180° turn) is folded into the
/// rotation; a single-axis flip is a genuine reflection that the rotation cannot
/// express and is dropped (the box's absolute placement is still preserved).
fn xfrm_to_frame(
    b: &XfrmBox,
    g: &GroupXfrm,
    slide_h: f64,
) -> (Option<model::Rect>, crate::model::Rotation) {
    // flipH ^ flipV alone is an unrepresentable reflection (ignored); flipH & flipV
    // together equal a 180° turn, folded onto the OOXML rotation before mapping.
    let rot_cw = if b.flip_h && b.flip_v {
        b.rot_deg + 180.0
    } else {
        b.rot_deg
    };
    let rotation = ooxml_rot_to_rotation(rot_cw);
    if !b.is_placed() {
        return (None, rotation);
    }
    let (x_top, y_top) = g.point(b.x, b.y);
    let (w, h) = g.extent(b.w, b.h);
    // Top-left (OOXML) → lower-left (model): flip Y about the slide height.
    let rect = model::Rect::new(x_top, slide_h - (y_top + h), w, h);
    (Some(rect), rotation)
}

/// Map an OOXML clockwise rotation (degrees) to the model's CCW [`Rotation`],
/// snapping the exact quarter turns to the first-class cardinal variants.
fn ooxml_rot_to_rotation(rot_cw_deg: f64) -> crate::model::Rotation {
    use crate::model::Rotation;
    // CW (OOXML) → CCW (model): negate, normalise to [0, 360).
    let mut ccw = (-rot_cw_deg) % 360.0;
    if ccw < 0.0 {
        ccw += 360.0;
    }
    if ccw.abs() < 1e-6 || (ccw - 360.0).abs() < 1e-6 {
        Rotation::D0
    } else if (ccw - 90.0).abs() < 1e-6 {
        Rotation::D90
    } else if (ccw - 180.0).abs() < 1e-6 {
        Rotation::D180
    } else if (ccw - 270.0).abs() < 1e-6 {
        Rotation::D270
    } else {
        Rotation::Deg(ccw)
    }
}

/// Recursively walk a shape-tree body (`p:spTree` or a `p:grpSp` group; the open
/// tag already consumed). Dispatches each child shape, descending into nested
/// groups with their transform composed onto `g`, and stops at the matching
/// `</p:spTree>` / `</p:grpSp>` (or EOF). Placeholder shapes go to
/// `acc.placeholders`; everything else (incl. grouped content, charts, SmartArt)
/// to `acc.shapes`.
#[allow(clippy::too_many_arguments)]
fn pptx_walk_sptree(
    x: &mut Xml,
    zip: &BTreeMap<String, Vec<u8>>,
    rels: &BTreeMap<String, String>,
    theme: &PptxTheme,
    geom: PageGeom,
    resources: &mut BTreeMap<u64, model::ImageResource>,
    inherit: &PptxPlaceholderGeom,
    g: GroupXfrm,
    acc: &mut PptxSlideAcc,
) {
    while let Some(tok) = x.next() {
        match &tok {
            Tok::Open(name, attrs, sc) if !sc => match local(name) {
                "sp" => pptx_sp_model(x, rels, theme, geom, inherit, &g, acc),
                "pic" => pptx_pic_model(x, zip, rels, geom, resources, &g, acc),
                "graphicFrame" => pptx_graphic_frame_model(x, zip, rels, theme, geom, &g, acc),
                "grpSp" => {
                    pptx_grp_sp_model(x, zip, rels, theme, geom, resources, inherit, &g, acc)
                }
                _ => {}
            },
            Tok::Close(name) if matches!(local(name), "spTree" | "grpSp") => break,
            _ => {}
        }
    }
}

/// Walk a `p:grpSp` group (open consumed): read the group's own `p:grpSpPr/a:xfrm`
/// (its placement *and* child coordinate space via `a:chOff`/`a:chExt`), compose
/// it onto the inherited transform `g`, then recurse into the group body so each
/// descendant lands at its true slide position.
#[allow(clippy::too_many_arguments)]
fn pptx_grp_sp_model(
    x: &mut Xml,
    zip: &BTreeMap<String, Vec<u8>>,
    rels: &BTreeMap<String, String>,
    theme: &PptxTheme,
    geom: PageGeom,
    resources: &mut BTreeMap<u64, model::ImageResource>,
    inherit: &PptxPlaceholderGeom,
    g: &GroupXfrm,
    acc: &mut PptxSlideAcc,
) {
    // The group transform appears as the FIRST a:xfrm (in p:grpSpPr); read it,
    // then keep walking the same group body for its child shapes.
    let mut composed = *g;
    while let Some(tok) = x.next() {
        match &tok {
            Tok::Open(name, attrs, sc) => match local(name) {
                "xfrm" if !sc => {
                    let (b, ch_off, ch_ext) = parse_group_xfrm(x, attrs);
                    composed = g.compose(&b, ch_off, ch_ext);
                }
                // Child shapes — same dispatch as the top-level tree.
                "sp" if !sc => pptx_sp_model(x, rels, theme, geom, inherit, &composed, acc),
                "pic" if !sc => pptx_pic_model(x, zip, rels, geom, resources, &composed, acc),
                "graphicFrame" if !sc => {
                    pptx_graphic_frame_model(x, zip, rels, theme, geom, &composed, acc)
                }
                "grpSp" if !sc => pptx_grp_sp_model(
                    x, zip, rels, theme, geom, resources, inherit, &composed, acc,
                ),
                _ => {}
            },
            Tok::Close(name) if local(name) == "grpSp" => break,
            _ => {}
        }
    }
}

/// Read a group's `a:xfrm` (open consumed): its placement box (`a:off`/`a:ext`)
/// plus the child coordinate space (`a:chOff`/`a:chExt`). Consumes up to
/// `</a:xfrm>`. Returns `(box, (chOffX, chOffY), (chExtW, chExtH))` in points.
fn parse_group_xfrm(
    x: &mut Xml,
    xfrm_attrs: &[(String, String)],
) -> (XfrmBox, (f64, f64), (f64, f64)) {
    let mut b = XfrmBox::default();
    if let Some(r) = attr(xfrm_attrs, "rot").and_then(|v| v.trim().parse::<f64>().ok()) {
        b.rot_deg = r / 60000.0;
    }
    let mut ch_off = (0.0, 0.0);
    let mut ch_ext = (0.0, 0.0);
    let mut have_off = false;
    let mut have_ext = false;
    let mut have_ch_off = false;
    let mut have_ch_ext = false;
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, _) => match local(&name) {
                "off" if !have_off => {
                    b.x = attr(&attrs, "x").and_then(emu_to_pt).unwrap_or(0.0);
                    b.y = attr(&attrs, "y").and_then(emu_to_pt).unwrap_or(0.0);
                    have_off = true;
                }
                "ext" if !have_ext => {
                    b.w = attr(&attrs, "cx").and_then(emu_to_pt).unwrap_or(0.0);
                    b.h = attr(&attrs, "cy").and_then(emu_to_pt).unwrap_or(0.0);
                    have_ext = true;
                }
                "chOff" if !have_ch_off => {
                    ch_off = (
                        attr(&attrs, "x").and_then(emu_to_pt).unwrap_or(0.0),
                        attr(&attrs, "y").and_then(emu_to_pt).unwrap_or(0.0),
                    );
                    have_ch_off = true;
                }
                "chExt" if !have_ch_ext => {
                    ch_ext = (
                        attr(&attrs, "cx").and_then(emu_to_pt).unwrap_or(0.0),
                        attr(&attrs, "cy").and_then(emu_to_pt).unwrap_or(0.0),
                    );
                    have_ch_ext = true;
                }
                _ => {}
            },
            Tok::Close(name) if local(&name) == "xfrm" => break,
            _ => {}
        }
    }
    (b, ch_off, ch_ext)
}

/// Lower one `p:sp` shape (open consumed): its placeholder role (`p:ph@type`),
/// its own `a:xfrm` (→ absolute frame) and its `p:txBody` paragraphs (→ a
/// [`TextBox`]). A placeholder with no own `a:xfrm` inherits geometry from the
/// layout/master chain (`inherit`). Empty text boxes are dropped. Consumes the
/// subtree up to `</p:sp>`.
fn pptx_sp_model(
    x: &mut Xml,
    rels: &BTreeMap<String, String>,
    theme: &PptxTheme,
    geom: PageGeom,
    inherit: &PptxPlaceholderGeom,
    g: &GroupXfrm,
    acc: &mut PptxSlideAcc,
) {
    let mut ph: Option<PhKey> = None;
    let mut have_ph = false;
    let mut xfrm = XfrmBox::default();
    let mut have_xfrm = false;
    let mut paras: Vec<Block> = Vec::new();

    // Per-paragraph scratch.
    let mut para_runs: Vec<Inline> = Vec::new();
    let mut in_para = false;
    let mut run = RunStyle::default();
    let mut rpr = PptxRunPr::default();
    let mut in_text = false;

    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => match local(&name) {
                "ph" => {
                    have_ph = true;
                    ph = Some(PhKey::from_attrs(&attrs));
                }
                "xfrm" if !sc && !have_xfrm => {
                    xfrm = parse_xfrm(x, &attrs);
                    have_xfrm = true;
                }
                "p" if !sc => {
                    in_para = true;
                    para_runs = Vec::new();
                }
                "t" if !sc => in_text = true,
                "br" => para_runs.push(Inline::LineBreak),
                "rPr" => {
                    run = pptx_run_props(&attrs);
                    rpr = PptxRunPr::open();
                    // A self-closing `<a:rPr/>` carries no colour/link children.
                    if sc {
                        rpr.close(&mut run, theme, rels);
                    }
                }
                // Any other tag inside an open `a:rPr` (colour / latin / hlink).
                ln if rpr.active => rpr.on_open(ln, &attrs),
                _ => {}
            },
            Tok::Text(t) => {
                if in_para && in_text && !t.is_empty() {
                    push_run_maybe_linked(&mut para_runs, &run, &t);
                }
            }
            Tok::Close(name) => match local(&name) {
                "t" => in_text = false,
                // Inside an open `a:rPr`: update fill nesting; fold on `</a:rPr>`.
                ln if rpr.active && rpr.on_close(ln) => rpr.close(&mut run, theme, rels),
                "p" => {
                    if in_para && !para_runs.is_empty() {
                        paras.push(Block {
                            kind: BlockKind::Paragraph(Paragraph {
                                runs: std::mem::take(&mut para_runs),
                                ..Paragraph::default()
                            }),
                            ..Block::default()
                        });
                    }
                    in_para = false;
                }
                "sp" => break,
                _ => {}
            },
        }
    }

    if paras.is_empty() {
        return;
    }

    // Geometry: the shape's own xfrm wins; otherwise a placeholder inherits from
    // the layout/master chain by matching its `p:ph` key.
    let (frame, rotation) = if have_xfrm && xfrm.is_placed() {
        xfrm_to_frame(&xfrm, g, geom.h)
    } else if have_ph {
        match ph.as_ref().and_then(|k| inherit.lookup(k)) {
            Some(b) => xfrm_to_frame(&b, g, geom.h),
            None => (None, crate::model::Rotation::D0),
        }
    } else {
        (None, crate::model::Rotation::D0)
    };

    let block = Block {
        frame,
        rotation,
        kind: BlockKind::TextBox(model::TextBox { blocks: paras }),
        ..Block::default()
    };

    if have_ph {
        acc.placeholders.push(model::Placeholder {
            role: ph.map(|k| k.role()).unwrap_or(model::PlaceholderRole::Body),
            block,
        });
    } else {
        acc.shapes.push(block);
    }
}

/// Lower one `p:pic` picture (open consumed): its `a:xfrm` (→ frame) and embedded
/// `a:blip` image. Consumes the subtree up to `</p:pic>`. The image lands in
/// `acc.shapes` (a picture is not a semantic placeholder).
fn pptx_pic_model(
    x: &mut Xml,
    zip: &BTreeMap<String, Vec<u8>>,
    rels: &BTreeMap<String, String>,
    geom: PageGeom,
    resources: &mut BTreeMap<u64, model::ImageResource>,
    g: &GroupXfrm,
    acc: &mut PptxSlideAcc,
) {
    let mut xfrm = XfrmBox::default();
    let mut have_xfrm = false;
    let mut img: Option<Block> = None;

    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => match local(&name) {
                "xfrm" if !sc && !have_xfrm => {
                    xfrm = parse_xfrm(x, &attrs);
                    have_xfrm = true;
                }
                "blip" if img.is_none() => {
                    if let Some(rid) = attr(&attrs, "embed").or_else(|| attr(&attrs, "link")) {
                        img = rels
                            .get(rid)
                            .map(|t| resolve_rel_part("ppt/slides", t))
                            .and_then(|k| image_block(zip, &k, resources));
                    }
                }
                _ => {}
            },
            Tok::Close(name) if local(&name) == "pic" => break,
            _ => {}
        }
    }

    if let Some(mut block) = img {
        let (frame, rotation) = xfrm_to_frame(&xfrm, g, geom.h);
        block.frame = frame;
        block.rotation = rotation;
        acc.shapes.push(block);
    }
}

/// Lower one `p:graphicFrame` (open consumed): read its `p:xfrm` for placement,
/// then dispatch on the `a:graphicData` payload — a native table (`a:tbl`), a
/// chart (`c:chart`, resolved via rels → title + series as a [`Table`]) or
/// SmartArt (`dgm:relIds`, resolved → node text as a [`List`]). The resulting
/// block lands in `acc.shapes` with its absolute frame. Consumes up to
/// `</p:graphicFrame>`.
#[allow(clippy::too_many_arguments)]
fn pptx_graphic_frame_model(
    x: &mut Xml,
    zip: &BTreeMap<String, Vec<u8>>,
    rels: &BTreeMap<String, String>,
    theme: &PptxTheme,
    geom: PageGeom,
    g: &GroupXfrm,
    acc: &mut PptxSlideAcc,
) {
    let mut xfrm = XfrmBox::default();
    let mut have_xfrm = false;
    let mut block: Option<Block> = None;

    while let Some(tok) = x.next() {
        match &tok {
            Tok::Open(name, attrs, sc) => match local(name) {
                "xfrm" if !sc && !have_xfrm => {
                    xfrm = parse_xfrm(x, attrs);
                    have_xfrm = true;
                }
                "tbl" if !sc && block.is_none() => {
                    block = Some(Block {
                        kind: BlockKind::Table(pptx_table_model(x, rels, theme)),
                        ..Block::default()
                    });
                }
                // A chart reference: `<c:chart r:id="rIdN"/>`.
                "chart" if block.is_none() => {
                    if let Some(rid) = attr(attrs, "id") {
                        block = pptx_chart_model(zip, rels, rid).map(|t| Block {
                            kind: BlockKind::Table(t),
                            ..Block::default()
                        });
                    }
                }
                // A SmartArt diagram: `<dgm:relIds r:dm="rIdN" …/>` → data part.
                "relIds" if block.is_none() => {
                    if let Some(rid) = attr(attrs, "dm") {
                        block = pptx_smartart_model(zip, rels, rid).map(|list| Block {
                            kind: BlockKind::List(list),
                            ..Block::default()
                        });
                    }
                }
                _ => {}
            },
            Tok::Close(name) if local(name) == "graphicFrame" => break,
            _ => {}
        }
    }

    if let Some(mut block) = block {
        let (frame, rotation) = xfrm_to_frame(&xfrm, g, geom.h);
        block.frame = frame;
        block.rotation = rotation;
        acc.shapes.push(block);
    } else {
        // Unknown/unsupported graphic payload (e.g. an OLE object or a chart whose
        // part is missing): surface a labelled placeholder paragraph rather than
        // dropping the frame silently.
        let (frame, rotation) = xfrm_to_frame(&xfrm, g, geom.h);
        acc.shapes.push(Block {
            frame,
            rotation,
            kind: BlockKind::TextBox(model::TextBox {
                blocks: vec![text_paragraph_block("[graphic]".to_string())],
            }),
            ..Block::default()
        });
    }
}

/// A `p:ph` placeholder key: its semantic type and optional index (`@idx`), used
/// both to derive the model role and to match the layout/master geometry.
#[derive(Clone, PartialEq, Eq, Default)]
struct PhKey {
    ty: Option<String>,
    idx: Option<String>,
}

impl PhKey {
    fn from_attrs(attrs: &[(String, String)]) -> Self {
        PhKey {
            ty: attr(attrs, "type").map(|s| s.to_string()),
            idx: attr(attrs, "idx").map(|s| s.to_string()),
        }
    }

    /// The model placeholder role implied by `@type` (default `Body`).
    fn role(&self) -> model::PlaceholderRole {
        match self.ty.as_deref() {
            Some("title") | Some("ctrTitle") => model::PlaceholderRole::Title,
            Some("subTitle") => model::PlaceholderRole::Subtitle,
            Some("body") | None => model::PlaceholderRole::Body,
            Some(other) => model::PlaceholderRole::Other(other.to_string()),
        }
    }
}

/// Placeholder geometry inherited from the slide's layout → master chain, keyed
/// by `p:ph` `@idx` (preferred — unique per layout) then by `@type`. Built once
/// per slide; empty when the layout/master can't be resolved.
#[derive(Default)]
struct PptxPlaceholderGeom {
    by_idx: BTreeMap<String, XfrmBox>,
    by_type: BTreeMap<String, XfrmBox>,
}

impl PptxPlaceholderGeom {
    /// Resolve the layout (`ppt/slides/_rels/slideN.xml.rels` → `slideLayout`)
    /// then the master (`…/slideLayoutM.xml.rels` → `slideMaster`), collecting
    /// each `p:sp`'s placeholder `a:xfrm`. Layout entries win over master ones.
    fn resolve(
        zip: &BTreeMap<String, Vec<u8>>,
        rels: &BTreeMap<String, String>,
        _n: usize,
    ) -> Self {
        let mut geom = PptxPlaceholderGeom::default();
        // Slide → layout (the slide rels live in `ppt/slides/_rels`, so targets
        // resolve relative to `ppt/slides`).
        let Some(layout_key) = rels
            .values()
            .map(|t| resolve_rel_part("ppt/slides", t))
            .find(|k| k.contains("slideLayout") && k.ends_with(".xml"))
        else {
            return geom;
        };
        // Master is reached through the *layout's* rels.
        if let Some(master_xml) = layout_rels_master(zip, &layout_key) {
            geom.collect(&master_xml); // master first (lower priority)
        }
        if let Some(bytes) = zip.get(&layout_key) {
            geom.collect(&String::from_utf8_lossy(bytes)); // layout overrides
        }
        geom
    }

    /// Scan one layout/master XML, recording every placeholder shape's
    /// `a:xfrm` under its `@idx` and `@type`.
    fn collect(&mut self, xml: &str) {
        let mut x = Xml::new(xml);
        let mut cur: Option<PhKey> = None;
        let mut in_sp = false;
        while let Some(tok) = x.next() {
            match &tok {
                Tok::Open(name, attrs, sc) => match local(name) {
                    "sp" if !sc => {
                        in_sp = true;
                        cur = None;
                    }
                    "ph" if in_sp => cur = Some(PhKey::from_attrs(attrs)),
                    "xfrm" if in_sp && !sc => {
                        let b = parse_xfrm(&mut x, attrs);
                        if b.is_placed() {
                            if let Some(k) = &cur {
                                if let Some(idx) = &k.idx {
                                    self.by_idx.insert(idx.clone(), b);
                                }
                                if let Some(ty) = &k.ty {
                                    self.by_type.insert(ty.clone(), b);
                                }
                            }
                        }
                    }
                    _ => {}
                },
                Tok::Close(name) if local(name) == "sp" => in_sp = false,
                _ => {}
            }
        }
    }

    /// Look up a placeholder's inherited box: by `@idx` first (most specific),
    /// then by `@type`.
    fn lookup(&self, key: &PhKey) -> Option<XfrmBox> {
        key.idx
            .as_ref()
            .and_then(|i| self.by_idx.get(i))
            .or_else(|| key.ty.as_ref().and_then(|t| self.by_type.get(t)))
            .copied()
    }
}

/// Resolve a slide-layout part's master XML via its sibling `_rels`
/// (`…/slideLayouts/_rels/slideLayoutM.xml.rels` → `slideMaster`). `None` when
/// the rels or master part is missing.
fn layout_rels_master(zip: &BTreeMap<String, Vec<u8>>, layout_key: &str) -> Option<String> {
    let rels_key = part_rels_key(layout_key);
    let rels = parse_rels(&String::from_utf8_lossy(zip.get(&rels_key)?));
    // Targets in the layout's rels resolve relative to the layout's directory.
    let from_dir = layout_key.rsplit_once('/').map(|(d, _)| d).unwrap_or("ppt");
    let master_key = rels
        .values()
        .map(|t| resolve_rel_part(from_dir, t))
        .find(|k| k.contains("slideMaster") && k.ends_with(".xml"))?;
    zip.get(&master_key)
        .map(|b| String::from_utf8_lossy(b).into_owned())
}

/// The `_rels/<file>.rels` key for an OOXML part path (e.g.
/// `ppt/slideLayouts/slideLayout1.xml` → `ppt/slideLayouts/_rels/slideLayout1.xml.rels`).
fn part_rels_key(part: &str) -> String {
    match part.rsplit_once('/') {
        Some((dir, file)) => format!("{dir}/_rels/{file}.rels"),
        None => format!("_rels/{part}.rels"),
    }
}

/// Lower a PPTX `a:tbl` (open consumed) to a model [`Table`]: `a:tblGrid/a:gridCol@w`
/// seeds `col_widths` (points); each `a:tr`→[`Row`], each `a:tc`→[`Cell`] with
/// `@gridSpan`/`@rowSpan`→spans and `@hMerge`/`@vMerge` continuation cells folded
/// into empty placeholders. Cell `a:tcPr/a:solidFill` → [`Cell::shading`] and
/// `a:hlinkClick` runs → [`Inline::Link`]. The model carries one table-wide
/// [`BorderStyle`], so the first cell edge (`a:lnL/R/T/B`) that declares a width
/// seeds it. Cell text reuses the slide paragraph grammar.
fn pptx_table_model(x: &mut Xml, rels: &BTreeMap<String, String>, theme: &PptxTheme) -> Table {
    let mut col_widths: Vec<f64> = Vec::new();
    let mut rows: Vec<Row> = Vec::new();
    let mut cur: Option<Vec<Cell>> = None;
    // The model has a single table-wide border; the first declared cell edge wins.
    let mut border: Option<model::BorderStyle> = None;

    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => {
                let ln = local(&name);
                if ln == "gridCol" {
                    if let Some(w) = attr(&attrs, "w").and_then(emu_to_pt) {
                        if w > 0.0 {
                            col_widths.push(w);
                        }
                    }
                } else if ln == "tr" && !sc {
                    cur = Some(Vec::new());
                } else if ln == "tc" && !sc {
                    let out = pptx_table_cell_model(x, rels, theme, &attrs);
                    if border.is_none() {
                        border = out.border;
                    }
                    if let Some(row) = cur.as_mut() {
                        for c in out.cells {
                            row.push(c);
                        }
                    }
                }
            }
            Tok::Close(name) => {
                let ln = local(&name);
                if ln == "tr" {
                    if let Some(cells) = cur.take() {
                        rows.push(Row {
                            cells,
                            height: None,
                        });
                    }
                } else if ln == "tbl" {
                    break;
                }
            }
            Tok::Text(_) => {}
        }
    }

    Table {
        rows,
        col_widths,
        border: border.unwrap_or_default(),
    }
}

/// One lowered PPTX `a:tc`: the model cell(s) it expands to (one, plus padding for
/// `@gridSpan`) and the border style it declared (`a:lnL/R/T/B`), surfaced to the
/// table so the model's single table-wide [`BorderStyle`] can be seeded.
struct PptxCellOut {
    cells: Vec<Cell>,
    border: Option<model::BorderStyle>,
}

/// Lower one PPTX `a:tc` cell (open consumed, attrs in `cell_attrs`) to a
/// [`PptxCellOut`]. `@gridSpan` widens the cell and pads the row with empty cells
/// so the column count stays correct; `@rowSpan` sets `row_span`; an `@hMerge`
/// continuation is dropped (covered to its left) and a `@vMerge` continuation
/// becomes one empty cell. The cell's `a:tcPr/a:solidFill` becomes
/// [`Cell::shading`], its first `a:lnL/R/T/B` edge becomes the surfaced
/// [`BorderStyle`], and run `a:hlinkClick`s become [`Inline::Link`]s. Consumes up
/// to `</a:tc>`.
fn pptx_table_cell_model(
    x: &mut Xml,
    rels: &BTreeMap<String, String>,
    theme: &PptxTheme,
    cell_attrs: &[(String, String)],
) -> PptxCellOut {
    let grid_span = attr(cell_attrs, "gridSpan")
        .and_then(|v| v.trim().parse::<u16>().ok())
        .unwrap_or(1)
        .max(1);
    let row_span = attr(cell_attrs, "rowSpan")
        .and_then(|v| v.trim().parse::<u16>().ok())
        .unwrap_or(1)
        .max(1);
    let h_merge = matches!(attr(cell_attrs, "hMerge"), Some("1") | Some("true"));
    let v_merge = matches!(attr(cell_attrs, "vMerge"), Some("1") | Some("true"));

    let mut paras: Vec<Block> = Vec::new();
    let mut para_runs: Vec<Inline> = Vec::new();
    let mut in_para = false;
    let mut run = RunStyle::default();
    let mut rpr = PptxRunPr::default();
    let mut in_text = false;
    // Cell-properties scratch: the `a:tcPr/a:solidFill` (fill) and `a:lnL/R/T/B`
    // (border) live here, kept distinct from run fills (`a:rPr`).
    let mut tc_pr = PptxCellPr::default();
    let mut depth = 0i32; // `a:tc` nesting guard

    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => match local(&name) {
                "tc" if !sc => depth += 1,
                "p" if !sc => {
                    in_para = true;
                    para_runs = Vec::new();
                }
                "t" if !sc => in_text = true,
                "br" => para_runs.push(Inline::LineBreak),
                "rPr" => {
                    run = pptx_run_props(&attrs);
                    rpr = PptxRunPr::open();
                    if sc {
                        rpr.close(&mut run, theme, rels);
                    }
                }
                // Run-property children (colour / latin / hlink) take priority over
                // cell-property parsing so a run fill is never read as the cell fill.
                ln if rpr.active => rpr.on_open(ln, &attrs),
                ln => tc_pr.on_open(ln, &attrs),
            },
            Tok::Text(t) => {
                if in_para && in_text && !t.is_empty() {
                    push_run_maybe_linked(&mut para_runs, &run, &t);
                }
            }
            Tok::Close(name) => match local(&name) {
                "t" => in_text = false,
                ln if rpr.active && rpr.on_close(ln) => rpr.close(&mut run, theme, rels),
                "p" => {
                    if in_para && !para_runs.is_empty() {
                        paras.push(Block {
                            kind: BlockKind::Paragraph(Paragraph {
                                runs: std::mem::take(&mut para_runs),
                                ..Paragraph::default()
                            }),
                            ..Block::default()
                        });
                    }
                    in_para = false;
                }
                "tc" => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
                ln => tc_pr.on_close(ln),
            },
        }
    }

    let (fill, border) = tc_pr.finish(theme);

    // Horizontal-merge continuation: covered by the cell spanning it — drop.
    if h_merge {
        return PptxCellOut {
            cells: Vec::new(),
            border,
        };
    }
    // Vertical-merge continuation: one empty cell to keep the column count.
    if v_merge {
        return PptxCellOut {
            cells: vec![Cell::default()],
            border,
        };
    }

    let mut cells = vec![Cell {
        blocks: paras,
        col_span: grid_span,
        row_span,
        shading: fill,
    }];
    // Pad to `grid_span` physical columns (empty continuation cells).
    for _ in 1..grid_span {
        cells.push(Cell::default());
    }
    PptxCellOut { cells, border }
}

/// Streaming state for a PPTX `a:tcPr` cell-properties subtree: the cell fill
/// (`a:solidFill`, kept distinct from a border line's own fill) and the first
/// declared border edge (`a:lnL/R/T/B`, with its `@w` width and `a:solidFill`
/// colour). The model carries one fill per cell and one border per table, so only
/// the first of each is retained. Colours are resolved (scheme slots through the
/// theme) at [`finish`](PptxCellPr::finish) time.
#[derive(Default)]
struct PptxCellPr {
    /// True while inside a border line element (`a:lnL/R/T/B`) — its nested
    /// `a:solidFill` is a *border* colour, not the cell fill.
    in_line: bool,
    /// The current border line's stroke width (points), from `a:ln@w` (EMU).
    line_w: Option<f64>,
    /// The current border line's colour accumulator.
    line_color: PptxFillColor,
    /// The first border edge's `(width, colour)` once a line closes.
    border: Option<(f64, PptxFillColor)>,
    /// True while inside the cell's own `a:solidFill` (a direct `a:tcPr` child).
    in_cell_fill: bool,
    /// The cell fill colour accumulator.
    cell_fill: PptxFillColor,
}

impl PptxCellPr {
    /// Handle an open tag inside the `a:tc` (outside any `a:rPr`). Enters border
    /// lines and the cell fill, and routes colour bases/modifiers to whichever is
    /// active (a border line's fill shadows the cell fill while open).
    fn on_open(&mut self, ln: &str, attrs: &[(String, String)]) {
        match ln {
            "lnL" | "lnR" | "lnT" | "lnB" => {
                self.in_line = true;
                self.line_w = attr(attrs, "w").and_then(emu_to_pt);
                self.line_color = PptxFillColor::default();
            }
            "solidFill" if self.in_line => {} // colours below land in line_color
            "solidFill" => self.in_cell_fill = true,
            "srgbClr" | "schemeClr" | "hslClr" | "sysClr" => {
                if self.in_line {
                    self.line_color.set_base(ln, attrs);
                } else if self.in_cell_fill {
                    self.cell_fill.set_base(ln, attrs);
                }
            }
            "lumMod" | "lumOff" | "shade" | "tint" => {
                if self.in_line {
                    self.line_color.set_mod(ln, attrs);
                } else if self.in_cell_fill {
                    self.cell_fill.set_mod(ln, attrs);
                }
            }
            _ => {}
        }
    }

    /// Handle a close tag inside the `a:tc`: completing a border line records the
    /// first edge (width + colour accumulator); closing the cell fill leaves the
    /// accumulator for [`finish`](PptxCellPr::finish).
    fn on_close(&mut self, ln: &str) {
        match ln {
            "lnL" | "lnR" | "lnT" | "lnB" => {
                if self.border.is_none() {
                    if let Some(w) = self.line_w.filter(|w| *w > 0.0) {
                        self.border = Some((w, std::mem::take(&mut self.line_color)));
                    }
                }
                self.in_line = false;
            }
            "solidFill" => self.in_cell_fill = false,
            _ => {}
        }
    }

    /// Resolve the cell fill and border colours through `theme`, returning
    /// `(cell fill RGB, table border)`. A border with no resolvable colour falls
    /// back to black.
    fn finish(self, theme: &PptxTheme) -> (Option<[f64; 3]>, Option<model::BorderStyle>) {
        let fill = self
            .cell_fill
            .finish_with(theme)
            .as_deref()
            .and_then(hex_to_rgb_f64);
        let border = self.border.map(|(width, color)| {
            let color = color
                .finish_with(theme)
                .as_deref()
                .and_then(hex_to_rgb_f64)
                .unwrap_or([0.0, 0.0, 0.0]);
            model::BorderStyle { width, color }
        });
        (fill, border)
    }
}

/// Extract a chart referenced by `rid` (resolved via `rels` → `ppt/charts/chartN.xml`)
/// into a model [`Table`]: header row = the category axis (blank corner + each
/// category label); one row per series (series name + its values). `None` when
/// the chart part is missing or carries no legible series. This keeps the chart's
/// *data* editable instead of dropping it (a vector re-render is out of scope).
fn pptx_chart_model(
    zip: &BTreeMap<String, Vec<u8>>,
    rels: &BTreeMap<String, String>,
    rid: &str,
) -> Option<Table> {
    let key = resolve_rel_part("ppt/slides", rels.get(rid)?);
    let xml = String::from_utf8_lossy(zip.get(&key)?);
    let chart = parse_pptx_chart(&xml);
    if chart.series.is_empty() && chart.title.is_none() {
        return None;
    }

    let mut rows: Vec<Row> = Vec::new();

    // Optional title as a single full-width-ish first row (one cell).
    if let Some(title) = &chart.title {
        if !title.is_empty() {
            rows.push(Row {
                cells: vec![chart_cell(title)],
                height: None,
            });
        }
    }

    // Header: blank corner + categories (use the longest series' category list).
    let categories = chart
        .series
        .iter()
        .map(|s| s.categories.len())
        .max()
        .unwrap_or(0);
    if categories > 0 {
        let mut header = vec![Cell::default()];
        // Pick the first non-empty category list.
        let cats = chart
            .series
            .iter()
            .map(|s| &s.categories)
            .find(|c| !c.is_empty());
        if let Some(cats) = cats {
            for c in cats {
                header.push(chart_cell(c));
            }
        }
        rows.push(Row {
            cells: header,
            height: None,
        });
    }

    // One row per series: name + values.
    for s in &chart.series {
        let mut cells = vec![chart_cell(&s.name)];
        for v in &s.values {
            cells.push(chart_cell(v));
        }
        rows.push(Row {
            cells,
            height: None,
        });
    }

    if rows.is_empty() {
        return None;
    }
    Some(Table {
        rows,
        col_widths: Vec::new(),
        border: model::BorderStyle::default(),
    })
}

/// A plain text [`Cell`] holding one default-styled paragraph (chart/SmartArt
/// extraction).
fn chart_cell(text: &str) -> Cell {
    Cell {
        blocks: vec![text_paragraph_block(text.to_string())],
        ..Cell::default()
    }
}

/// One extracted chart series: its name plus its category labels and values.
#[derive(Default)]
struct PptxChartSeries {
    name: String,
    categories: Vec<String>,
    values: Vec<String>,
}

/// The legible content of a chart part: an optional title and its series.
#[derive(Default)]
struct PptxChart {
    title: Option<String>,
    series: Vec<PptxChartSeries>,
}

/// Parse a chart part (`c:chartSpace`) for its title and series. Series names,
/// categories and values are read from the cached string/number references
/// (`c:strRef/c:strCache` and `c:numRef/c:numCache` → `c:pt/c:v`) that every
/// saved chart embeds, so no spreadsheet evaluation is needed. The title is read
/// from `c:title` rich text (`a:t`) or its string cache.
fn parse_pptx_chart(xml: &str) -> PptxChart {
    let mut chart = PptxChart::default();
    let mut x = Xml::new(xml);

    // Context flags for the streaming walk.
    let mut in_title = false;
    let mut in_ser = false;
    let mut ser: PptxChartSeries = PptxChartSeries::default();
    // Which part of the series the current cache belongs to.
    #[derive(PartialEq)]
    enum Field {
        None,
        SerTx,
        Cat,
        Val,
    }
    let mut field = Field::None;
    let mut in_v = false;
    let mut v_buf = String::new();
    let mut title_buf = String::new();

    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, _attrs, sc) => match local(&name) {
                "title" if !sc => in_title = true,
                "ser" if !sc => {
                    in_ser = true;
                    ser = PptxChartSeries::default();
                }
                "tx" if in_ser => field = Field::SerTx,
                "cat" if in_ser => field = Field::Cat,
                "val" if in_ser => field = Field::Val,
                "v" if !sc => {
                    in_v = true;
                    v_buf.clear();
                }
                _ => {}
            },
            Tok::Text(t) => {
                if in_v {
                    v_buf.push_str(&t);
                } else if in_title {
                    // Title rich text (a:t) lands here too.
                    title_buf.push_str(&t);
                }
            }
            Tok::Close(name) => match local(&name) {
                "v" => {
                    in_v = false;
                    let val = v_buf.trim().to_string();
                    if !val.is_empty() {
                        match field {
                            Field::SerTx => {
                                if ser.name.is_empty() {
                                    ser.name = val;
                                }
                            }
                            Field::Cat => ser.categories.push(val),
                            Field::Val => ser.values.push(val),
                            Field::None => {}
                        }
                    }
                }
                "tx" | "cat" | "val" => field = Field::None,
                "ser" => {
                    in_ser = false;
                    chart.series.push(std::mem::take(&mut ser));
                }
                "title" => {
                    in_title = false;
                    let t = title_buf.trim();
                    if !t.is_empty() && chart.title.is_none() {
                        chart.title = Some(t.to_string());
                    }
                    title_buf.clear();
                }
                _ => {}
            },
        }
    }
    chart
}

/// Extract a SmartArt diagram's node text into a model [`List`]. `rid` is the
/// `dgm:relIds@r:dm` data-model relationship (resolved via `rels` →
/// `ppt/diagrams/dataN.xml`); each diagram point's text (`dgm:t` → `a:t`) becomes
/// a bullet item. `None` when the data part is missing or empty — keeping the
/// diagram's *text* rather than dropping it (rendering the diagram is out of
/// scope).
fn pptx_smartart_model(
    zip: &BTreeMap<String, Vec<u8>>,
    rels: &BTreeMap<String, String>,
    rid: &str,
) -> Option<List> {
    let key = resolve_rel_part("ppt/slides", rels.get(rid)?);
    let xml = String::from_utf8_lossy(zip.get(&key)?);
    let items = parse_pptx_diagram_text(&xml);
    if items.is_empty() {
        return None;
    }
    Some(List {
        ordered: false,
        marker: ListMarker::Bullet('\u{2022}'),
        items: items
            .into_iter()
            .map(|text| ListItem {
                blocks: vec![text_paragraph_block(text)],
                level: 0,
            })
            .collect(),
    })
}

/// Collect a SmartArt data model's node texts. Each `dgm:pt` text body
/// (`dgm:t`, a Drawing-ML text body) contributes one entry, with its paragraphs
/// joined by spaces. Empty texts are skipped.
fn parse_pptx_diagram_text(xml: &str) -> Vec<String> {
    let mut items: Vec<String> = Vec::new();
    let mut x = Xml::new(xml);
    let mut in_t = false; // inside a <dgm:t> text body
    let mut in_text = false; // inside an <a:t>
    let mut cur = String::new();

    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, _, sc) => match local(&name) {
                "t" if !sc => {
                    // `dgm:t` opens a text body; `a:t` (nested) carries the runs.
                    // Distinguish by depth: the outer `t` starts the body, the
                    // inner `t` is the run text.
                    if !in_t {
                        in_t = true;
                        cur.clear();
                    } else {
                        in_text = true;
                    }
                }
                _ => {}
            },
            Tok::Text(s) => {
                if in_t && in_text {
                    cur.push_str(&s);
                }
            }
            Tok::Close(name) => {
                if local(&name) == "t" {
                    if in_text {
                        in_text = false;
                    } else if in_t {
                        in_t = false;
                        let trimmed = cur.trim();
                        if !trimmed.is_empty() {
                            items.push(trimmed.to_string());
                        }
                    }
                }
            }
        }
    }
    items
}

// ─────────────────────────────── ODF → model ──────────────────────────────────

/// The resolved style tables an ODT/ODP model walk needs, bundled so the
/// recursive helpers take one immutable context instead of four loose maps
/// (mirrors the [`DocxCtx`] pattern). `resources` and the output vector stay
/// separate (they are `&mut`).
struct OdfModelCtx<'a> {
    zip: &'a BTreeMap<String, Vec<u8>>,
    /// `text:span` style name → CSS text-properties (run styling).
    styles: &'a BTreeMap<String, String>,
    /// Paragraph style name → resolved `fo:*` paragraph formatting.
    para_styles: &'a BTreeMap<String, OdfParaProps>,
    /// Column style name → width (points), for table `col_widths`.
    cols: &'a BTreeMap<String, f64>,
    /// Cell style name → `fo:background-color` RGB, for cell shading.
    cell_bg: &'a BTreeMap<String, [f64; 3]>,
}

impl OdfModelCtx<'_> {
    /// A context with only the run-style map populated (the others empty) — used
    /// by the ODP slide path, which needs inline run styling but not paragraph
    /// formatting, table widths, or cell shading.
    fn styles_only<'a>(
        zip: &'a BTreeMap<String, Vec<u8>>,
        styles: &'a BTreeMap<String, String>,
    ) -> OdfModelCtx<'a> {
        // Borrow process-wide empty maps so the context carries real references.
        static EMPTY_PARA: std::sync::OnceLock<BTreeMap<String, OdfParaProps>> =
            std::sync::OnceLock::new();
        static EMPTY_COLS: std::sync::OnceLock<BTreeMap<String, f64>> = std::sync::OnceLock::new();
        static EMPTY_BG: std::sync::OnceLock<BTreeMap<String, [f64; 3]>> =
            std::sync::OnceLock::new();
        OdfModelCtx {
            zip,
            styles,
            para_styles: EMPTY_PARA.get_or_init(BTreeMap::new),
            cols: EMPTY_COLS.get_or_init(BTreeMap::new),
            cell_bg: EMPTY_BG.get_or_init(BTreeMap::new),
        }
    }
}

/// ODT → [`Document`]: `text:h`→heading, `text:p`→paragraph, `text:list`→list,
/// `table:table`→table. Reuses the ODF text-style, paragraph-style and
/// column-width parsers; `fo:*` paragraph formatting, footnotes (`text:note`)
/// and body text boxes (`draw:text-box`) are lowered onto the model.
pub fn odt_to_model(zip: &BTreeMap<String, Vec<u8>>) -> Document {
    let content = part(zip, "content.xml");
    let styles_xml = part(zip, "styles.xml");
    let mut styles = odf_text_styles(&styles_xml);
    styles.extend(odf_text_styles(&content));
    // Paragraph-level formatting (`fo:*` align/indent/spacing) keyed by style
    // name, parent chains resolved. Automatic styles (content.xml) win over the
    // named styles (styles.xml) on a name clash.
    let mut para_styles = odf_para_styles(&styles_xml);
    para_styles.extend(odf_para_styles(&content));
    // Column widths and cell-background shading for tables.
    let mut cols = odf_column_widths(&styles_xml);
    cols.extend(odf_column_widths(&content));
    let mut cell_bg = odf_cell_backgrounds(&styles_xml);
    cell_bg.extend(odf_cell_backgrounds(&content));
    let geom = odf_geom(&styles_xml, &content, PageGeom::prose_default());
    let ctx = OdfModelCtx {
        zip,
        styles: &styles,
        para_styles: &para_styles,
        cols: &cols,
        cell_bg: &cell_bg,
    };
    let mut blocks = Vec::new();
    let mut resources: BTreeMap<u64, model::ImageResource> = BTreeMap::new();
    let mut outline = OutlineBuilder::default();
    odf_walk_model(
        &mut Xml::new(&content),
        &ctx,
        &mut blocks,
        None,
        None,
        &mut resources,
        &mut outline,
    );
    let mut doc = flow_document(blocks, page_geometry(geom));
    doc.resources.images = resources;
    doc.outline = outline.finish();
    // Lower `styles.xml`'s `office:styles` paragraph styles into the model's
    // style table so each paragraph's `style_ref` (set from `text:style-name`)
    // resolves to a present `NamedStyle`.
    doc.styles = odf_named_styles(&styles_xml);
    doc.meta = odf_doc_meta(zip);
    doc
}

/// Recursive ODF model walker (mirrors [`odf_walk`]). Handles `text:h`,
/// `text:p`, `text:list` (each item → list-item paragraph), `table:table` and a
/// body-anchored `draw:frame`/`draw:text-box` (→ [`BlockKind::TextBox`]). The
/// context's paragraph-style map (resolved from `text:style-name`) lowers `fo:*`
/// paragraph formatting onto each paragraph/heading.
fn odf_walk_model(
    x: &mut Xml,
    ctx: &OdfModelCtx,
    out: &mut Vec<Block>,
    stop: Option<&str>,
    list_level: Option<u32>,
    resources: &mut BTreeMap<u64, model::ImageResource>,
    outline: &mut OutlineBuilder,
) {
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => {
                let ln = local(&name);
                match ln {
                    "h" if !sc => {
                        let style = odf_paragraph_style(&attrs, ctx.para_styles, list_level);
                        // `text:style-name` → `style_ref` (resolves into the model
                        // style table built from `office:styles`).
                        let style_ref = odf_style_ref(&attrs);
                        // `text:outline-level` is 1-based (`1`=top); the model
                        // heading level clamps to 1..6, but the *outline* keeps
                        // the full 0-based depth (`level-1`).
                        let raw_lvl = attr(&attrs, "outline-level")
                            .and_then(|v| v.trim().parse::<u32>().ok())
                            .filter(|&v| v >= 1)
                            .unwrap_or(1);
                        let lvl = raw_lvl.clamp(1, 6) as u8;
                        // Text boxes anchored in the heading flush as sibling
                        // blocks after it (mirrors the paragraph case).
                        let mut anchored: Vec<Block> = Vec::new();
                        let runs = odf_inline_model(x, ctx, "h", resources, &mut anchored, outline);
                        // Record the outline entry. The whole document is one page,
                        // so the target is page 0; an empty title is dropped by
                        // `push_heading`.
                        outline.push_heading(inlines_plain_text(&runs), (raw_lvl - 1) as usize, 0);
                        if !runs.is_empty() {
                            out.push(Block {
                                kind: BlockKind::Heading(Heading {
                                    level: lvl,
                                    para: Paragraph {
                                        style,
                                        style_ref,
                                        runs,
                                    },
                                }),
                                ..Block::default()
                            });
                        }
                        out.append(&mut anchored);
                    }
                    "p" if !sc => {
                        let style = odf_paragraph_style(&attrs, ctx.para_styles, list_level);
                        // `text:style-name` → `style_ref` (resolves into the model
                        // style table built from `office:styles`).
                        let style_ref = odf_style_ref(&attrs);
                        // A frame anchored in this paragraph (`draw:frame` →
                        // `draw:text-box`) is captured here and emitted as a
                        // sibling block right after the paragraph.
                        let mut anchored: Vec<Block> = Vec::new();
                        let runs = odf_inline_model(x, ctx, "p", resources, &mut anchored, outline);
                        if runs.is_empty() && list_level.is_none() {
                            if anchored.is_empty() {
                                out.push(Block::default()); // preserve blank line spacing
                            } else {
                                out.append(&mut anchored);
                            }
                            continue;
                        }
                        let paragraph = Paragraph {
                            style,
                            style_ref,
                            runs,
                        };
                        match list_level {
                            Some(level) => out.push(Block {
                                kind: BlockKind::List(List {
                                    ordered: false,
                                    marker: ListMarker::Bullet('\u{2022}'),
                                    items: vec![ListItem {
                                        blocks: vec![Block {
                                            kind: BlockKind::Paragraph(paragraph),
                                            ..Block::default()
                                        }],
                                        level: level.min(u8::MAX as u32) as u8,
                                    }],
                                }),
                                ..Block::default()
                            }),
                            None => out.push(Block {
                                kind: BlockKind::Paragraph(paragraph),
                                ..Block::default()
                            }),
                        }
                        out.append(&mut anchored);
                    }
                    // A body-level `draw:frame` (sibling to paragraphs): a text
                    // box → a text-box block, a picture → an image block.
                    "frame" if !sc => match odf_frame_content(x, ctx, resources) {
                        OdfFrameContent::TextBox(blocks) => out.push(Block {
                            kind: BlockKind::TextBox(model::TextBox { blocks }),
                            ..Block::default()
                        }),
                        OdfFrameContent::Image(img) => out.push(Block {
                            kind: BlockKind::Image(img),
                            ..Block::default()
                        }),
                        OdfFrameContent::Empty => {}
                    },
                    "list" if !sc => {
                        let next = Some(list_level.map(|l| l + 1).unwrap_or(0));
                        odf_walk_model(x, ctx, out, Some("list"), next, resources, outline);
                    }
                    "table" if !sc => {
                        let table = odf_table_model(x, ctx, resources);
                        out.push(Block {
                            kind: BlockKind::Table(table),
                            ..Block::default()
                        });
                    }
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

/// Resolve a paragraph/heading's `text:style-name` to a model [`ParagraphStyle`]
/// via `para_styles` (parent chains already flattened), stacking the list indent
/// for the current `list_level`. An unknown/absent style still picks up the list
/// indent so list items keep their nesting offset.
fn odf_paragraph_style(
    attrs: &[(String, String)],
    para_styles: &BTreeMap<String, OdfParaProps>,
    list_level: Option<u32>,
) -> ParagraphStyle {
    attr(attrs, "style-name")
        .and_then(|n| para_styles.get(n))
        .cloned()
        .unwrap_or_default()
        .to_paragraph_style(list_level)
}

/// A paragraph/heading's `text:style-name` as a model [`StyleId`] for
/// `Paragraph.style_ref` — the source style reference (named style or, for a
/// paragraph carrying direct overrides, its automatic style). Resolution against
/// the model style table is best-effort: a named style is present (built by
/// [`odf_named_styles`]); an automatic style (declared in `content.xml`) records
/// the reference without a table entry, mirroring the DOCX `w:pStyle` behaviour.
/// Absent/empty ⇒ `None`.
fn odf_style_ref(attrs: &[(String, String)]) -> Option<model::StyleId> {
    attr(attrs, "style-name")
        .filter(|n| !n.is_empty())
        .map(|n| model::StyleId(n.to_string()))
}

/// The resolved content of a `draw:frame`: a text box (its captured blocks), a
/// single picture, or nothing usable.
enum OdfFrameContent {
    TextBox(Vec<Block>),
    Image(model::ImageRef),
    Empty,
}

/// Read one `draw:frame` (open already consumed) up to its matching
/// `</draw:frame>`, returning its content. A `draw:text-box` wins (its inner
/// `text:p`/`text:h`/`text:list`/`table:table` become the box's blocks); else the
/// first usable `draw:image` is interned as a picture. `draw:frame` is otherwise
/// transparent, so this preserves the prior inline-image behaviour while adding
/// text-box capture.
fn odf_frame_content(
    x: &mut Xml,
    ctx: &OdfModelCtx,
    resources: &mut BTreeMap<u64, model::ImageResource>,
) -> OdfFrameContent {
    let mut blocks: Vec<Block> = Vec::new();
    let mut found_box = false;
    let mut image: Option<model::ImageRef> = None;
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => {
                let ln = local(&name);
                if ln == "text-box" && !sc {
                    found_box = true;
                    // A floating text box is not part of the document outline; its
                    // headings/bookmarks go to a discarded builder.
                    let mut no_outline = OutlineBuilder::default();
                    odf_walk_model(
                        x,
                        ctx,
                        &mut blocks,
                        Some("text-box"),
                        None,
                        resources,
                        &mut no_outline,
                    );
                } else if ln == "image" && image.is_none() {
                    if let Some(href) = attr(&attrs, "href") {
                        let key = href.trim_start_matches('/').to_string();
                        image = image_ref(ctx.zip, &key, resources);
                    }
                }
            }
            Tok::Close(name) if local(&name) == "frame" => break,
            _ => {}
        }
    }
    if found_box && !blocks.is_empty() {
        OdfFrameContent::TextBox(blocks)
    } else if let Some(img) = image {
        OdfFrameContent::Image(img)
    } else {
        OdfFrameContent::Empty
    }
}

/// Collect an ODF block's inline content as model [`Inline`] runs, honouring
/// `text:span` styles (parsed from the style map into [`CharStyle`]),
/// `text:tab`/`text:s`/`text:line-break`, `text:a` hyperlinks (→ [`Inline::Link`]),
/// `text:note` footnotes/endnotes (citation marker + body text inlined) and
/// inline `draw:frame`/`draw:image` (→ [`Inline::Image`], bytes interned in
/// `resources`). A `draw:frame` wrapping a `draw:text-box` (a paragraph-anchored
/// text box) is captured and appended to `out_blocks` as a sibling block.
/// Mirrors [`odf_inline`].
fn odf_inline_model(
    x: &mut Xml,
    ctx: &OdfModelCtx,
    block: &str,
    resources: &mut BTreeMap<u64, model::ImageResource>,
    out_blocks: &mut Vec<Block>,
    outline: &mut OutlineBuilder,
) -> Vec<Inline> {
    let mut runs: Vec<Inline> = Vec::new();
    // Stack of span char-styles (closed in order).
    let mut span_stack: Vec<CharStyle> = Vec::new();
    // The currently open `text:a` hyperlink, if any; its runs are buffered here
    // and flushed as an `Inline::Link` on the matching close. ODF anchors do not
    // nest, so a single slot suffices (a nested `text:a` simply replaces it).
    let mut link: Option<DocxLink> = None;
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => {
                let ln = local(&name);
                match ln {
                    "span" if !sc => {
                        let css = attr(&attrs, "style-name")
                            .and_then(|n| ctx.styles.get(n))
                            .cloned()
                            .unwrap_or_default();
                        span_stack.push(odf_css_char_style(&css));
                    }
                    // `text:a` hyperlink: open a link buffer carrying the resolved
                    // target (`xlink:href`); its inner runs land in `children`.
                    "a" if !sc => {
                        link = Some(DocxLink {
                            href: odf_link_target(&attrs),
                            children: Vec::new(),
                        });
                    }
                    // A footnote/endnote: inline its citation marker followed by
                    // the note body text so neither is lost (the body lives inside
                    // the note element, right at the reference point).
                    "note" if !sc => {
                        let note = odf_note_inline(x, ctx);
                        for inline in note {
                            active_inlines(&mut runs, &mut link).push(inline);
                        }
                    }
                    "tab" => odf_push(active_inlines(&mut runs, &mut link), &span_stack, " "),
                    "line-break" => active_inlines(&mut runs, &mut link).push(Inline::LineBreak),
                    "s" => {
                        let n = attr(&attrs, "c")
                            .and_then(|v| v.parse::<usize>().ok())
                            .unwrap_or(1);
                        odf_push(
                            active_inlines(&mut runs, &mut link),
                            &span_stack,
                            &" ".repeat(n),
                        );
                    }
                    // A `draw:frame`: a paragraph-anchored text box flushes as a
                    // sibling block; a picture frame emits an inline image (so the
                    // image keeps its place in the run flow).
                    "frame" if !sc => match odf_frame_content(x, ctx, resources) {
                        OdfFrameContent::TextBox(blocks) => out_blocks.push(Block {
                            kind: BlockKind::TextBox(model::TextBox { blocks }),
                            ..Block::default()
                        }),
                        OdfFrameContent::Image(img) => {
                            active_inlines(&mut runs, &mut link).push(Inline::Image(img));
                        }
                        OdfFrameContent::Empty => {}
                    },
                    // An inline picture not wrapped in a frame: `draw:image
                    // @xlink:href`. Intern the bytes and emit an `Inline::Image`.
                    "image" if attr(&attrs, "href").is_some() => {
                        if let Some(href) = attr(&attrs, "href") {
                            let key = href.trim_start_matches('/').to_string();
                            if let Some(img) = image_ref(ctx.zip, &key, resources) {
                                active_inlines(&mut runs, &mut link).push(Inline::Image(img));
                            }
                        }
                    }
                    // ODF bookmarks within the block flow: `text:bookmark` (a
                    // point) and `text:bookmark-start` (a range start). Both carry
                    // `text:name`; record it as a navigable outline anchor (the
                    // whole document is one page → page 0).
                    "bookmark" | "bookmark-start" => {
                        if let Some(name) = attr(&attrs, "name") {
                            outline.push_bookmark(name, 0);
                        }
                    }
                    _ => {}
                }
            }
            Tok::Text(t) => odf_push(active_inlines(&mut runs, &mut link), &span_stack, &t),
            Tok::Close(name) => {
                let ln = local(&name);
                if ln == "span" {
                    span_stack.pop();
                } else if ln == "a" {
                    // Flush the hyperlink (drop empties) into the run flow.
                    if let Some(l) = link.take() {
                        if !l.children.is_empty() {
                            runs.push(Inline::Link {
                                href: l.href,
                                children: l.children,
                            });
                        }
                    }
                } else if ln == block {
                    break;
                }
            }
        }
    }
    // A hyperlink left open at block end (malformed input): flush it.
    if let Some(l) = link.take() {
        if !l.children.is_empty() {
            runs.push(Inline::Link {
                href: l.href,
                children: l.children,
            });
        }
    }
    runs
}

/// Collect a `text:note` (open already consumed) as inline runs: the
/// `text:note-citation` text rendered as a superscript marker, then the
/// `text:note-body` paragraphs' text (paragraphs joined by a space). Consumes up
/// to the matching `</text:note>`. Span styling inside the body is flattened to
/// plain text (the note is surfaced as a parenthetical, not a styled subtree).
fn odf_note_inline(x: &mut Xml, ctx: &OdfModelCtx) -> Vec<Inline> {
    let mut citation = String::new();
    let mut body = String::new();
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, _, sc) => {
                let ln = local(&name);
                if ln == "note-citation" && !sc {
                    citation = odf_text_only(x, "note-citation");
                } else if ln == "note-body" && !sc {
                    // Walk the body into throwaway blocks, then flatten to text.
                    // The note body is not part of the document outline, so its
                    // headings/bookmarks go to a discarded builder.
                    let mut blocks: Vec<Block> = Vec::new();
                    let mut resources: BTreeMap<u64, model::ImageResource> = BTreeMap::new();
                    let mut no_outline = OutlineBuilder::default();
                    odf_walk_model(
                        x,
                        ctx,
                        &mut blocks,
                        Some("note-body"),
                        None,
                        &mut resources,
                        &mut no_outline,
                    );
                    body = inline_blocks_text(&blocks);
                }
            }
            Tok::Close(name) if local(&name) == "note" => break,
            _ => {}
        }
    }
    let mut out: Vec<Inline> = Vec::new();
    let citation = citation.trim();
    if !citation.is_empty() {
        out.push(Inline::Run(InlineRun {
            text: citation.to_string(),
            style: CharStyle {
                vertical_align: model::style::VAlign::Super,
                ..CharStyle::default()
            },
            source_index: None,
        }));
    }
    let body = body.trim();
    if !body.is_empty() {
        // A leading space separates the note body from the citation marker / the
        // surrounding text so the inlined note reads cleanly.
        out.push(Inline::Run(InlineRun {
            text: format!(" {body}"),
            ..InlineRun::default()
        }));
    }
    out
}

/// The concatenated plain text of a block list (paragraph/heading runs joined,
/// paragraphs separated by a single space), trimmed. Used to flatten a footnote
/// body to inline text.
fn inline_blocks_text(blocks: &[Block]) -> String {
    let mut out = String::new();
    for b in blocks {
        match &b.kind {
            BlockKind::Paragraph(p) | BlockKind::Heading(Heading { para: p, .. }) => {
                for r in &p.runs {
                    if let Inline::Run(run) = r {
                        out.push_str(&run.text);
                    }
                }
                out.push(' ');
            }
            BlockKind::List(l) => {
                for it in &l.items {
                    let inner = inline_blocks_text(&it.blocks);
                    if !inner.is_empty() {
                        out.push_str(&inner);
                        out.push(' ');
                    }
                }
            }
            _ => {}
        }
    }
    out.trim().to_string()
}

/// Collect only the text content of an element (open already consumed) until its
/// matching close `</…stop>`, ignoring any child markup. Used for simple
/// text-only ODF elements such as `text:note-citation`.
fn odf_text_only(x: &mut Xml, stop: &str) -> String {
    let mut out = String::new();
    while let Some(tok) = x.next() {
        match tok {
            Tok::Text(t) => out.push_str(&t),
            Tok::Close(name) if local(&name) == stop => break,
            _ => {}
        }
    }
    out
}

/// Resolve an ODF `text:a` to a model [`LinkTarget`]: an external URL from
/// `xlink:href`. A purely in-document reference (`#bookmark`/`#frame`) has no page
/// the model can address by index, so it lands on the document start (page 0),
/// matching the DOCX anchor behaviour; a blank href ⇒ an empty URL.
fn odf_link_target(attrs: &[(String, String)]) -> model::LinkTarget {
    match attr(attrs, "href").map(str::trim) {
        Some(h) if h.starts_with('#') => model::LinkTarget::Page(0),
        Some(h) if !h.is_empty() => model::LinkTarget::Url(decode(h)),
        _ => model::LinkTarget::Url(String::new()),
    }
}

/// Append `text` as an [`InlineRun`] carrying the innermost open span style
/// (default when no span is open), coalescing with an identical previous run.
fn odf_push(runs: &mut Vec<Inline>, span_stack: &[CharStyle], text: &str) {
    if text.is_empty() {
        return;
    }
    let style = span_stack.last().cloned().unwrap_or_default();
    if let Some(Inline::Run(last)) = runs.last_mut() {
        if last.style == style {
            last.text.push_str(text);
            return;
        }
    }
    runs.push(Inline::Run(InlineRun {
        text: text.to_string(),
        style,
        source_index: None,
    }));
}

/// Parse an ODF `text-properties` CSS fragment (as produced by
/// [`odf_text_styles`]) back into a [`CharStyle`] (bold/italic/underline/colour/
/// size/family) — the inverse of the HTML emission, for the model path.
fn odf_css_char_style(css: &str) -> CharStyle {
    let mut style = CharStyle::default();
    for decl in css.split(';') {
        let Some((k, v)) = decl.split_once(':') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        match k {
            "font-weight" if v == "bold" => style.bold = true,
            "font-style" if v == "italic" => style.italic = true,
            "text-decoration" if v.contains("underline") => style.underline = true,
            "text-decoration" if v.contains("line-through") => style.strike = true,
            "color" => style.color = hex_to_rgb_f64(v.trim_start_matches('#')),
            "background-color" => style.background = hex_to_rgb_f64(v.trim_start_matches('#')),
            "font-size" => {
                if let Some(pt) = v
                    .strip_suffix("pt")
                    .and_then(|n| n.trim().parse::<f64>().ok())
                {
                    style.size_pt = pt;
                }
            }
            "font-family" => {
                let fam = v.trim_matches(['\'', '"']).to_string();
                if !fam.is_empty() {
                    style.generic = super::style::parse_base_font(&fam).generic;
                    style.family = fam;
                }
            }
            _ => {}
        }
    }
    style
}

/// Emit one ODF `table:table` (open already consumed) as a model [`Table`],
/// expanding `table:number-columns-repeated` (cap 64). Column widths come from
/// the `table:table-column` declarations (resolved via the context's `cols`,
/// style-name → width(pt)) and each cell's `fo:background-color` shading from
/// `cell_bg` (style-name → RGB). Mirrors [`odf_table`].
fn odf_table_model(
    x: &mut Xml,
    ctx: &OdfModelCtx,
    resources: &mut BTreeMap<u64, model::ImageResource>,
) -> Table {
    let mut rows: Vec<Row> = Vec::new();
    let mut cur_row: Option<Vec<Cell>> = None;
    // Grid-column widths, expanded over `table:number-columns-repeated`.
    let mut col_widths: Vec<f64> = Vec::new();

    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => {
                let ln = local(&name);
                if ln == "table-column" {
                    let repeat = attr(&attrs, "number-columns-repeated")
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(1)
                        .min(64);
                    let width = attr(&attrs, "style-name")
                        .and_then(|n| ctx.cols.get(n))
                        .copied()
                        .unwrap_or(0.0);
                    for _ in 0..repeat {
                        col_widths.push(width);
                    }
                } else if ln == "table-row" && !sc {
                    cur_row = Some(Vec::new());
                } else if ln == "table-cell" && !sc {
                    let repeat = attr(&attrs, "number-columns-repeated")
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(1)
                        .min(64);
                    let shading = attr(&attrs, "style-name")
                        .and_then(|n| ctx.cell_bg.get(n))
                        .copied();
                    let mut blocks = Vec::new();
                    // Cell content is not part of the document outline; its
                    // headings/bookmarks go to a discarded builder.
                    let mut no_outline = OutlineBuilder::default();
                    odf_walk_model(
                        x,
                        ctx,
                        &mut blocks,
                        Some("table-cell"),
                        None,
                        resources,
                        &mut no_outline,
                    );
                    if let Some(row) = cur_row.as_mut() {
                        for _ in 0..repeat {
                            row.push(Cell {
                                blocks: blocks.clone(),
                                shading,
                                ..Cell::default()
                            });
                        }
                    }
                } else if ln == "covered-table-cell" && sc {
                    if let Some(row) = cur_row.as_mut() {
                        row.push(Cell::default());
                    }
                }
            }
            Tok::Close(name) => {
                let ln = local(&name);
                if ln == "table-row" {
                    if let Some(cells) = cur_row.take() {
                        rows.push(Row {
                            cells,
                            height: None,
                        });
                    }
                } else if ln == "table" {
                    break;
                }
            }
            Tok::Text(_) => {}
        }
    }

    // Only carry widths through when at least one column declared a real width
    // (a table with no `style:column-width` keeps the auto-layout default).
    if !col_widths.iter().any(|w| *w > 0.0) {
        col_widths.clear();
    }
    Table {
        rows,
        col_widths,
        border: model::BorderStyle::default(),
    }
}

/// ODS → [`Document`] with one [`BlockKind::Sheet`]; each `table:table` becomes a
/// model [`Sheet`] of typed cells carrying per-cell number format / fill / font /
/// border / alignment / wrap, plus per-column widths, per-row heights and merge
/// ranges (`table:number-columns/rows-spanned`). ODF spreadsheets carry the
/// displayed value as `text:p`. Reuses the ODF style maps shared with the render
/// path; an automatic style in `content.xml` overrides a same-named one in
/// `styles.xml`.
pub fn ods_to_model(zip: &BTreeMap<String, Vec<u8>>) -> Document {
    let content = part(zip, "content.xml");
    let styles_xml = part(zip, "styles.xml");
    let geom = odf_geom(&styles_xml, &content, PageGeom::tabular_default());

    // Resolved style tables, named (styles.xml) then automatic (content.xml wins).
    let mut props = OdsStyleTables::default();
    props.absorb(&styles_xml);
    props.absorb(&content);

    let mut sheets: Vec<Sheet> = Vec::new();
    let mut x = Xml::new(&content);
    while let Some(tok) = x.next() {
        if let Tok::Open(name, attrs, sc) = &tok {
            if local(name) == "table" && !sc {
                let sheet_name = attr(attrs, "name").unwrap_or("Sheet").to_string();
                sheets.push(ods_sheet_model(&mut x, sheet_name, &props));
            }
        }
    }

    let block = Block {
        kind: BlockKind::Sheet(SheetBlock { sheets }),
        ..Block::default()
    };
    let mut doc = flow_document(vec![block], page_geometry(geom));
    doc.meta = odf_doc_meta(zip);
    doc
}

/// Build one model [`Sheet`] from a `table:table` (its open already consumed):
/// expand repeated rows/columns (cap 64), resolve each cell's style (cell own →
/// row default → column default) into typed number format / fill / font / border
/// / alignment / wrap, collect per-column widths and per-row heights, and turn
/// `table:number-columns/rows-spanned` into [`MergeRange`]s. Mirrors the spans /
/// repeat / collapse rules of [`ods_table`]/[`ods_row`].
fn ods_sheet_model(x: &mut Xml, name: String, props: &OdsStyleTables) -> Sheet {
    let mut rows: Vec<SheetRow> = Vec::new();
    let mut merges: Vec<model::MergeRange> = Vec::new();
    let mut col_widths: Vec<f64> = Vec::new();
    // Per-column default cell-style name, expanded over `number-columns-repeated`.
    let mut col_defaults: Vec<Option<String>> = Vec::new();

    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => {
                let ln = local(&name);
                if ln == "table-column" {
                    // Both `<table:table-column .../>` and an open form land here.
                    let rep = attr(&attrs, "number-columns-repeated")
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(1)
                        .min(1024);
                    let width = attr(&attrs, "style-name").and_then(|s| props.col_widths.get(s));
                    let def = attr(&attrs, "default-cell-style-name").map(str::to_string);
                    for _ in 0..rep {
                        col_widths.push(width.copied().unwrap_or(0.0));
                        col_defaults.push(def.clone());
                    }
                } else if ln == "table-row" && !sc {
                    let rep = attr(&attrs, "number-rows-repeated")
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(1)
                        .min(64);
                    let height = attr(&attrs, "style-name").and_then(|s| props.row_heights.get(s));
                    let row_default = attr(&attrs, "default-cell-style-name");
                    let base_row = rows.len();
                    let (cells, spans) = ods_row_model(x, props, &col_defaults, row_default);
                    // Collapse a run of fully-blank rows to one — but a row holding
                    // any formula carries content even with blank cached results.
                    let blank = cells
                        .iter()
                        .all(|c| c.value == model::CellValue::Empty && c.formula.is_none());
                    let emit = if blank { rep.min(1) } else { rep };
                    let height = height.copied().filter(|h| *h > 0.0);
                    for _ in 0..emit {
                        rows.push(SheetRow {
                            cells: cells.clone(),
                            height,
                        });
                    }
                    // A row span anchors at this row; when the row is repeated a
                    // distinct multi-row span is ambiguous, so clamp it to one row.
                    for (c0, cspan, rspan) in spans {
                        let r1 = if emit == 1 {
                            base_row + rspan.saturating_sub(1)
                        } else {
                            base_row
                        };
                        merges.push(model::MergeRange {
                            r0: base_row,
                            c0,
                            r1,
                            c1: c0 + cspan.saturating_sub(1),
                        });
                    }
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

    // Trim trailing zero (default) column widths so the field stays empty when no
    // explicit widths were authored, matching the XLSX path's `Vec::new()`.
    while matches!(col_widths.last(), Some(w) if *w == 0.0) {
        col_widths.pop();
    }

    Sheet {
        name,
        rows,
        merges,
        col_widths,
    }
}

/// Collect one `table:table-row`'s typed cells (open already consumed) plus its
/// merge spans. Reuses [`ods_cell_text`] for the displayed value, classifying it
/// as a number when it parses (preferring `office:value` for typed numeric
/// cells), captures a `table:formula` (→ [`SheetCell::formula`]), resolves the
/// cell's style (own → row default → column default) into number format / fill /
/// font / border / alignment / wrap, and records `(col, colspan, rowspan)` for
/// any `table:number-columns/rows-spanned` anchor. `covered-table-cell`s (merge
/// fillers) keep their slot but carry no span.
fn ods_row_model(
    x: &mut Xml,
    props: &OdsStyleTables,
    col_defaults: &[Option<String>],
    row_default: Option<&str>,
) -> (Vec<SheetCell>, Vec<(usize, usize, usize)>) {
    let mut cells: Vec<SheetCell> = Vec::new();
    let mut spans: Vec<(usize, usize, usize)> = Vec::new();
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => {
                let ln = local(&name);
                let is_cell = ln == "table-cell" || ln == "covered-table-cell";
                if is_cell && !sc {
                    let rep = attr(&attrs, "number-columns-repeated")
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(1)
                        .min(64);
                    // `table:formula` (e.g. `of:=SUM([.A1:.A9])`) → the authored
                    // expression without its namespace prefix and leading `=`.
                    let formula = attr(&attrs, "formula").and_then(odf_formula_expr);
                    // Typed numeric cells carry the canonical value in
                    // `office:value` — prefer it over the (possibly locale-
                    // formatted) display text for correct number typing.
                    let typed_num = matches!(
                        attr(&attrs, "value-type"),
                        Some("float") | Some("percentage") | Some("currency")
                    )
                    .then(|| attr(&attrs, "value").and_then(|v| v.trim().parse::<f64>().ok()))
                    .flatten();
                    // Resolve style by precedence: cell own → row default → column
                    // default (by the current physical column index).
                    let style_name = attr(&attrs, "style-name")
                        .map(str::to_string)
                        .or_else(|| row_default.map(str::to_string))
                        .or_else(|| col_defaults.get(cells.len()).and_then(|d| d.clone()));
                    let look = style_name.as_deref().and_then(|s| props.cells.get(s));
                    let mut cell = SheetCell::default();
                    if let Some(p) = look {
                        cell.style = p.char_.clone();
                        cell.fill = p.fill;
                        cell.border = p.border;
                        cell.align = p.align;
                        cell.wrap = p.wrap;
                        cell.number_format = p.number_format.clone();
                    }
                    // `table:number-{columns,rows}-spanned` define a merge anchor.
                    let cspan = attr(&attrs, "number-columns-spanned")
                        .and_then(|v| v.parse::<usize>().ok())
                        .filter(|n| *n > 1);
                    let rspan = attr(&attrs, "number-rows-spanned")
                        .and_then(|v| v.parse::<usize>().ok())
                        .filter(|n| *n > 1);
                    if cspan.is_some() || rspan.is_some() {
                        spans.push((cells.len(), cspan.unwrap_or(1), rspan.unwrap_or(1)));
                    }
                    let text = ods_cell_text(x, ln);
                    let trimmed = text.trim();
                    cell.value = if let Some(n) = typed_num {
                        model::CellValue::Number(n)
                    } else if trimmed.is_empty() {
                        model::CellValue::Empty
                    } else if let Ok(n) = trimmed.parse::<f64>() {
                        model::CellValue::Number(n)
                    } else {
                        model::CellValue::Text(trimmed.to_string())
                    };
                    cell.formula = formula;
                    // A formula or styled cell is meaningful even when blank, so
                    // emit each repeat (don't collapse a styled run to one).
                    let meaningful = cell.value != model::CellValue::Empty
                        || cell.formula.is_some()
                        || look.is_some();
                    let emit = if meaningful { rep } else { rep.min(1) };
                    for _ in 0..emit {
                        cells.push(cell.clone());
                    }
                } else if is_cell && sc {
                    cells.push(SheetCell::default());
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
    (cells, spans)
}

/// ODP → [`Document`] with one [`BlockKind::Slide`]; each `draw:page` → a
/// [`Slide`] whose `text:p` paragraphs become body placeholders and `draw:image`
/// become image shapes. Reuses the ODF inline walker and geometry.
pub fn odp_to_model(zip: &BTreeMap<String, Vec<u8>>) -> Document {
    let content = part(zip, "content.xml");
    let styles_xml = part(zip, "styles.xml");
    let mut styles = odf_text_styles(&styles_xml);
    styles.extend(odf_text_styles(&content));
    let geom = odf_geom(&styles_xml, &content, PageGeom::slide_default());

    // Drawing-page fill colours (`styles.xml` + `content.xml`) and master-page →
    // drawing-page-style links, so each `draw:page` can resolve its background
    // from its own `draw:style-name`, falling back to its master page.
    let mut page_fills = odf_drawing_page_fills(&styles_xml);
    page_fills.extend(odf_drawing_page_fills(&content));
    let master_styles = odf_master_page_styles(&styles_xml);

    let mut slides: Vec<Slide> = Vec::new();
    let mut resources: BTreeMap<u64, model::ImageResource> = BTreeMap::new();
    let mut x = Xml::new(&content);
    while let Some(tok) = x.next() {
        if let Tok::Open(name, attrs, sc) = &tok {
            if local(name) == "page" && !sc {
                let background = odp_page_background(attrs, &page_fills, &master_styles);
                slides.push(odp_page_model(
                    &mut x,
                    zip,
                    &styles,
                    geom,
                    background,
                    &mut resources,
                ));
            }
        }
    }

    let block = Block {
        kind: BlockKind::Slide(SlideBlock { slides }),
        ..Block::default()
    };
    let mut doc = flow_document(vec![block], page_geometry(geom));
    doc.resources.images = resources;
    doc.meta = odf_doc_meta(zip);
    doc
}

/// Emit one `draw:page` (open consumed) as a model [`Slide`]. Bare `text:p`
/// children become body placeholders and bare `draw:image` become picture
/// placeholders (the legacy flow layout). Positioned `draw:frame`s become
/// absolutely-placed shapes in [`Slide::shapes`] (geometry from `svg:x/y/
/// width/height`), and `draw:g` groups are descended recursively, the group's
/// own `svg:x/y` composed onto the children's positions. `background` is the
/// resolved page/master fill (computed by the caller). Mirrors [`odp_page`].
fn odp_page_model(
    x: &mut Xml,
    zip: &BTreeMap<String, Vec<u8>>,
    styles: &BTreeMap<String, String>,
    geom: PageGeom,
    background: Option<[f64; 3]>,
    resources: &mut BTreeMap<u64, model::ImageResource>,
) -> Slide {
    let mut placeholders: Vec<model::Placeholder> = Vec::new();
    let mut shapes: Vec<Block> = Vec::new();
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => {
                let ln = local(&name);
                match ln {
                    // A positioned frame → an absolutely-placed shape (consumes its
                    // subtree). A frame without a usable box falls through to the
                    // flow branches below so its inner content is still captured.
                    "frame" if !sc && odp_frame_box_xf(&attrs).is_some() => {
                        let bx = odp_frame_box_xf(&attrs).unwrap();
                        odp_frame_model(
                            x,
                            zip,
                            styles,
                            geom,
                            (0.0, 0.0),
                            bx,
                            &attrs,
                            resources,
                            &mut shapes,
                            &mut placeholders,
                        );
                    }
                    // A group: descend, composing the group's own offset.
                    "g" if !sc => {
                        let (gx, gy) = odp_group_offset(&attrs);
                        odp_group_model(
                            x,
                            zip,
                            styles,
                            geom,
                            (gx, gy),
                            resources,
                            &mut shapes,
                            &mut placeholders,
                        );
                    }
                    "p" if !sc => {
                        let role =
                            odp_placeholder_role(&attrs).unwrap_or(model::PlaceholderRole::Body);
                        let ctx = OdfModelCtx::styles_only(zip, styles);
                        let mut anchored: Vec<Block> = Vec::new();
                        // ODP slides build no document outline; bookmark anchors
                        // captured here are discarded with this throwaway builder.
                        let mut no_outline = OutlineBuilder::default();
                        let runs = odf_inline_model(
                            x,
                            &ctx,
                            "p",
                            resources,
                            &mut anchored,
                            &mut no_outline,
                        );
                        if !runs.is_empty() {
                            placeholders.push(model::Placeholder {
                                role,
                                block: Block {
                                    kind: BlockKind::Paragraph(Paragraph {
                                        runs,
                                        ..Paragraph::default()
                                    }),
                                    ..Block::default()
                                },
                            });
                        }
                    }
                    "image" if sc => {
                        if let Some(href) = attr(&attrs, "href") {
                            let key = href.trim_start_matches('/').to_string();
                            if let Some(mut img) = image_block(zip, &key, resources) {
                                if let BlockKind::Image(ir) = &mut img.kind {
                                    ir.alt = odp_frame_alt(None, None, &attrs);
                                }
                                placeholders.push(model::Placeholder {
                                    role: odp_placeholder_role(&attrs).unwrap_or_else(|| {
                                        model::PlaceholderRole::Other("picture".to_string())
                                    }),
                                    block: img,
                                });
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
    Slide {
        geometry: page_geometry(geom),
        shapes,
        placeholders,
        notes: None,
        background,
    }
}

/// A `draw:g` group's own placement offset in points: `(svg:x, svg:y)` (each
/// defaulting to `0`). ODF nests absolute child positions inside an optionally
/// translated group; this offset is added to descendant frame origins so a
/// grouped shape lands at its true slide position. (A `draw:transform` matrix on
/// the group, when present, is not decomposed — only the transl/offset is honoured,
/// the zero-dependency pragmatic choice.)
fn odp_group_offset(attrs: &[(String, String)]) -> (f64, f64) {
    let x = attr(attrs, "x").and_then(parse_odf_pt).unwrap_or(0.0);
    let y = attr(attrs, "y").and_then(parse_odf_pt).unwrap_or(0.0);
    (x, y)
}

/// Walk a `draw:g` group body (open consumed; `off` = the accumulated parent
/// offset including this group's own). Positioned frames become shapes (their
/// origin shifted by `off`); nested groups recurse with their offset composed
/// additively; bare paragraphs/images still surface as placeholders. Stops at the
/// matching `</draw:g>` (or EOF).
#[allow(clippy::too_many_arguments)]
fn odp_group_model(
    x: &mut Xml,
    zip: &BTreeMap<String, Vec<u8>>,
    styles: &BTreeMap<String, String>,
    geom: PageGeom,
    off: (f64, f64),
    resources: &mut BTreeMap<u64, model::ImageResource>,
    shapes: &mut Vec<Block>,
    placeholders: &mut Vec<model::Placeholder>,
) {
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => {
                let ln = local(&name);
                match ln {
                    "frame" if !sc && odp_frame_box_xf(&attrs).is_some() => {
                        let bx = odp_frame_box_xf(&attrs).unwrap();
                        odp_frame_model(
                            x,
                            zip,
                            styles,
                            geom,
                            off,
                            bx,
                            &attrs,
                            resources,
                            shapes,
                            placeholders,
                        );
                    }
                    "g" if !sc => {
                        let (gx, gy) = odp_group_offset(&attrs);
                        odp_group_model(
                            x,
                            zip,
                            styles,
                            geom,
                            (off.0 + gx, off.1 + gy),
                            resources,
                            shapes,
                            placeholders,
                        );
                    }
                    "p" if !sc => {
                        let role =
                            odp_placeholder_role(&attrs).unwrap_or(model::PlaceholderRole::Body);
                        let ctx = OdfModelCtx::styles_only(zip, styles);
                        let mut anchored: Vec<Block> = Vec::new();
                        // ODP slides build no document outline; bookmark anchors
                        // captured here are discarded with this throwaway builder.
                        let mut no_outline = OutlineBuilder::default();
                        let runs = odf_inline_model(
                            x,
                            &ctx,
                            "p",
                            resources,
                            &mut anchored,
                            &mut no_outline,
                        );
                        if !runs.is_empty() {
                            placeholders.push(model::Placeholder {
                                role,
                                block: Block {
                                    kind: BlockKind::Paragraph(Paragraph {
                                        runs,
                                        ..Paragraph::default()
                                    }),
                                    ..Block::default()
                                },
                            });
                        }
                    }
                    "image" if sc => {
                        if let Some(href) = attr(&attrs, "href") {
                            let key = href.trim_start_matches('/').to_string();
                            if let Some(mut img) = image_block(zip, &key, resources) {
                                if let BlockKind::Image(ir) = &mut img.kind {
                                    ir.alt = odp_frame_alt(None, None, &attrs);
                                }
                                placeholders.push(model::Placeholder {
                                    role: odp_placeholder_role(&attrs).unwrap_or_else(|| {
                                        model::PlaceholderRole::Other("picture".to_string())
                                    }),
                                    block: img,
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
            Tok::Close(name) if local(&name) == "g" => break,
            _ => {}
        }
    }
}

/// Lower one positioned `draw:frame` (open consumed; `bx` = its `(x, y, w, h)`
/// page box in points, `off` = the enclosing group offset). The block carries a
/// lower-left-origin [`Rect`] frame and the CCW rotation parsed from the frame's
/// [`draw:transform`](odp_transform_rotation). The frame body — a `draw:text-box`
/// of paragraphs, or a `draw:image` — chooses the block kind: a single image ⇒
/// [`BlockKind::Image`] (its `alt` taken from the frame's
/// `svg:title`/`svg:desc`/`draw:name`); otherwise a [`BlockKind::TextBox`]. A
/// **text** frame tagged with `presentation:class` is emitted as a semantic
/// [`model::Placeholder`] (mirroring the PPTX `p:ph` path); everything else lands
/// in `shapes` (a picture is not a placeholder). An empty frame is dropped.
/// Consumes up to `</draw:frame>`.
#[allow(clippy::too_many_arguments)]
fn odp_frame_model(
    x: &mut Xml,
    zip: &BTreeMap<String, Vec<u8>>,
    styles: &BTreeMap<String, String>,
    geom: PageGeom,
    off: (f64, f64),
    bx: (f64, f64, f64, f64),
    frame_attrs: &[(String, String)],
    resources: &mut BTreeMap<u64, model::ImageResource>,
    shapes: &mut Vec<Block>,
    placeholders: &mut Vec<model::Placeholder>,
) {
    let mut blocks: Vec<Block> = Vec::new();
    let mut image: Option<model::ImageRef> = None;
    let mut title: Option<String> = None;
    let mut desc: Option<String> = None;
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => {
                let ln = local(&name);
                match ln {
                    "p" if !sc => {
                        let ctx = OdfModelCtx::styles_only(zip, styles);
                        let mut anchored: Vec<Block> = Vec::new();
                        // ODP slides build no document outline; bookmark anchors
                        // captured here are discarded with this throwaway builder.
                        let mut no_outline = OutlineBuilder::default();
                        let runs = odf_inline_model(
                            x,
                            &ctx,
                            "p",
                            resources,
                            &mut anchored,
                            &mut no_outline,
                        );
                        if !runs.is_empty() {
                            blocks.push(Block {
                                kind: BlockKind::Paragraph(Paragraph {
                                    runs,
                                    ..Paragraph::default()
                                }),
                                ..Block::default()
                            });
                        }
                    }
                    // Accessible alt text: `svg:title`/`svg:desc` carry their text
                    // as a single child node; capture it for `ImageRef::alt`.
                    "title" if !sc && title.is_none() => title = Some(xml_text_until(x, "title")),
                    "desc" if !sc && desc.is_none() => desc = Some(xml_text_until(x, "desc")),
                    "image" if image.is_none() => {
                        if let Some(href) = attr(&attrs, "href") {
                            let key = href.trim_start_matches('/').to_string();
                            image = image_ref(zip, &key, resources);
                        }
                    }
                    _ => {}
                }
            }
            Tok::Close(name) if local(&name) == "frame" => break,
            _ => {}
        }
    }

    let (fx, fy, fw, fh) = bx;
    // Top-left (ODF, offset by the enclosing group) → lower-left (model).
    let top_x = fx + off.0;
    let top_y = fy + off.1;
    let rect = model::Rect::new(top_x, (geom.h - (top_y + fh)).max(0.0), fw, fh);
    let rotation = odp_transform_rotation(frame_attrs);

    if !blocks.is_empty() {
        // A text frame: a text box of the captured editable blocks. (An image
        // alongside text is rare for a placeholder frame; the text runs win.)
        let block = Block {
            frame: Some(rect),
            rotation,
            kind: BlockKind::TextBox(model::TextBox { blocks }),
            ..Block::default()
        };
        // A `presentation:class` makes this a semantic placeholder (title/body/…);
        // otherwise it is a free text shape.
        match odp_placeholder_role(frame_attrs) {
            Some(role) => placeholders.push(model::Placeholder { role, block }),
            None => shapes.push(block),
        }
    } else if let Some(mut img) = image {
        // No text: a pure picture frame becomes an image block (carrying any alt
        // text); an empty frame is dropped.
        img.alt = odp_frame_alt(title, desc, frame_attrs);
        shapes.push(Block {
            frame: Some(rect),
            rotation,
            kind: BlockKind::Image(img),
            ..Block::default()
        });
    }
}

/// Legacy `.doc/.xls/.ppt` (OLE2) → text-only model: best-effort runs as
/// paragraphs. Reuses the CFB parser and [`extract_runs`]. `None` if nothing
/// legible is found.
fn ole2_to_model(bytes: &[u8]) -> Option<Document> {
    let cfb = Cfb::parse(bytes)?;
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
    let data = stream.or_else(|| cfb.largest_stream())?;
    let paras = extract_runs(&data);
    if paras.is_empty() {
        return None;
    }
    let blocks = paras.into_iter().map(text_paragraph_block).collect();
    Some(flow_document(
        blocks,
        page_geometry(PageGeom::prose_default()),
    ))
}

/// A plain-text paragraph [`Block`] carrying a single default-styled run.
fn text_paragraph_block(text: String) -> Block {
    Block {
        kind: BlockKind::Paragraph(Paragraph {
            runs: vec![Inline::Run(InlineRun {
                text,
                style: CharStyle::default(),
                source_index: None,
            })],
            ..Paragraph::default()
        }),
        ..Block::default()
    }
}

/// Decode a supported image zip entry, register its bytes in `resources` under a
/// content-hash key, and return an [`BlockKind::Image`] block referencing that
/// key. `None` for a missing or unsupported (vector/legacy) entry. Identical
/// bytes hash identically, so a reused picture is stored once.
fn image_block(
    zip: &BTreeMap<String, Vec<u8>>,
    key: &str,
    resources: &mut BTreeMap<u64, model::ImageResource>,
) -> Option<Block> {
    let mime = image_mime(key)?;
    let bytes = zip.get(key)?.clone();
    let hash = fnv1a(&bytes);
    let format = mime.rsplit('/').next().unwrap_or("png").to_string();
    resources
        .entry(hash)
        .or_insert(model::ImageResource { bytes, format });
    Some(Block {
        kind: BlockKind::Image(model::ImageRef {
            resource: hash,
            alt: None,
        }),
        ..Block::default()
    })
}

/// 64-bit FNV-1a content hash — a stable, dependency-free key for the
/// [`crate::model::ResourceTable`] (identical bytes hash identically).
fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
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

/// Concatenate the text content of the currently-open element (its open tag
/// already consumed) up to its matching close `</…>` (local name `tag`),
/// skipping any markup in between. Nested same-name opens are tracked so the
/// close that ends the right element is found.
fn xml_text_until(x: &mut Xml, tag: &str) -> String {
    let mut out = String::new();
    let mut depth = 1usize;
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, _, sc) if !sc && local(&name) == tag => depth += 1,
            Tok::Close(name) if local(&name) == tag => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Tok::Text(t) => out.push_str(&t),
            _ => {}
        }
    }
    out
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

/// Resolve a relationship `Target` against the **directory of the source part**
/// (`from_dir`), following the OPC convention exactly: an absolute `/x` is taken
/// from the package root, otherwise the target is appended to `from_dir` with
/// each leading `../` popping one `from_dir` segment. This is correct for parts
/// nested more than one level deep (e.g. a slide at `ppt/slides/slideN.xml`
/// whose rels point at `../charts/chartN.xml` → `ppt/charts/chartN.xml`), which
/// the package-root [`resolve_target`] mishandles.
fn resolve_rel_part(from_dir: &str, target: &str) -> String {
    if let Some(abs) = target.strip_prefix('/') {
        return abs.to_string();
    }
    let mut segs: Vec<&str> = from_dir.split('/').filter(|s| !s.is_empty()).collect();
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                segs.pop();
            }
            other => segs.push(other),
        }
    }
    segs.join("/")
}

// ════════════════════════ embedded-font extraction ════════════════════════════
//
// A self-embedding Office file ships the actual typefaces it uses inside the
// container, so it renders identically anywhere — even offline, even with fonts
// the host doesn't have. We surface those faces as the renderer's
// [`ProvidedFont`]s so the layout uses their *real* advance widths (no reflow
// drift) and the painter draws their *real* glyphs (not the Liberation
// fallback).
//
// Two embedding schemes:
//   • OOXML (DOCX/PPTX): `word|ppt/fontTable.xml` lists `<w:font w:name="…">`
//     with `<w:embedRegular|Bold|Italic|BoldItalic r:id w:fontKey>`. Each `r:id`
//     resolves (via the sibling `_rels`) to `…/fonts/fontN.odttf` — an
//     **obfuscated** TTF/OTF whose first 32 bytes are XORed with the GUID
//     (ECMA-376 §17.8.1). We de-obfuscate, then validate as a font program.
//   • ODF (ODT/ODS/ODP): `Fonts/*` holds plain TTF/OTF; `content.xml`/`styles.xml`
//     `<style:font-face><svg:font-face-src><svg:font-face-uri xlink:href="…">`
//     names the family and points at the file.

/// Every embedded face we could extract from the container, ready to hand to the
/// renderer. The `family` is the **raw** typeface name (matched case-insensitively
/// against the run `font-family` the HTML carries); `weight`/`italic` pick the
/// nearest face for a styled run. Empty when the document embeds no fonts (the
/// common case) — then referenced families are resolved by the host instead.
fn extract_embedded_fonts(zip: &BTreeMap<String, Vec<u8>>) -> Vec<ProvidedFont> {
    if zip.contains_key("word/fontTable.xml") {
        ooxml_embedded_fonts(zip, "word")
    } else if zip.contains_key("ppt/fontTable.xml") {
        ooxml_embedded_fonts(zip, "ppt")
    } else if zip.contains_key("xl/fontTable.xml") {
        // Rare, but XLSX can embed fonts the same way.
        ooxml_embedded_fonts(zip, "xl")
    } else if zip.contains_key("mimetype") {
        odf_embedded_fonts(zip)
    } else {
        Vec::new()
    }
}

/// The renderer's face-key for de-duplication: `(family lowercased, bold,
/// italic)`, where `bold` mirrors the painter's `weight >= 600` threshold so two
/// faces collide here exactly when they would in [`crate::html`]'s font book.
fn font_key(f: &ProvidedFont) -> (String, bool, bool) {
    (f.family.to_ascii_lowercase(), f.weight >= 600, f.italic)
}

/// Merge the faces the container embeds itself with the `host`-supplied faces
/// (phase 2 of the two-phase font flow: families [`office_needed_fonts`] reported
/// as referenced-but-unembedded, fetched and handed back by the host — e.g.
/// Carlito for a Calibri reference).
///
/// **Embedded wins on conflict.** Embedded faces are listed first and the
/// renderer resolves a run by the *first* matching face (exact key → same family
/// → any), so a document that ships its own typeface keeps it; a `host` face is
/// only appended when its exact key isn't already embedded, so it fills the gaps
/// (referenced-but-unembedded families) without ever shadowing an embedded face
/// and without poisoning the font book with dead duplicates.
fn merge_fonts(embedded: Vec<ProvidedFont>, host: &[ProvidedFont]) -> Vec<ProvidedFont> {
    if host.is_empty() {
        return embedded;
    }
    let mut keys: std::collections::BTreeSet<(String, bool, bool)> =
        embedded.iter().map(font_key).collect();
    let mut out = embedded;
    out.reserve(host.len());
    for f in host {
        if keys.insert(font_key(f)) {
            out.push(f.clone());
        }
    }
    out
}

/// A single `<w:embed…>` reference recovered from an OOXML `fontTable.xml`:
/// which face of `family` it is, the relationship id pointing at the obfuscated
/// font part, and the GUID used to de-obfuscate it.
struct OoxmlFontRef {
    family: String,
    bold: bool,
    italic: bool,
    rel_id: String,
    font_key: Option<String>,
}

/// Parse `<base>/fontTable.xml`, resolve each embedded face through the sibling
/// `_rels`, de-obfuscate the `.odttf` part, and return the validated faces.
fn ooxml_embedded_fonts(zip: &BTreeMap<String, Vec<u8>>, base: &str) -> Vec<ProvidedFont> {
    let table = part(zip, &format!("{base}/fontTable.xml"));
    if table.is_empty() {
        return Vec::new();
    }
    let rels = zip
        .get(&format!("{base}/_rels/fontTable.xml.rels"))
        .map(|b| parse_rels(&String::from_utf8_lossy(b)))
        .unwrap_or_default();

    let mut out = Vec::new();
    for r in parse_ooxml_font_table(&table) {
        let Some(target) = rels.get(&r.rel_id) else {
            continue;
        };
        let key = resolve_target(base, target);
        let Some(raw) = zip.get(&key) else { continue };
        // De-obfuscate when a GUID is present (OOXML obfuscated `.odttf`);
        // a missing key means the part is a plain font program already.
        let program = match r.font_key.as_deref().and_then(parse_guid) {
            Some(guid) => deobfuscate_odttf(raw, &guid),
            None => raw.clone(),
        };
        if let Some(font) = make_provided_font(&r.family, r.bold, r.italic, program) {
            out.push(font);
        }
    }
    out
}

/// Walk an OOXML `fontTable.xml`, emitting one [`OoxmlFontRef`] per embedded
/// face. Inside each `<w:font w:name="…">` the embed elements
/// (`w:embedRegular|Bold|Italic|BoldItalic`) carry the `r:id` and `w:fontKey`.
fn parse_ooxml_font_table(xml: &str) -> Vec<OoxmlFontRef> {
    let mut out = Vec::new();
    let mut current: Option<String> = None; // family of the open <w:font>
    let mut x = Xml::new(xml);
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, _) => match local(&name) {
                "font" => current = attr(&attrs, "name").map(|s| s.to_string()),
                tag @ ("embedRegular" | "embedBold" | "embedItalic" | "embedBoldItalic") => {
                    if let (Some(fam), Some(id)) = (
                        current.as_ref(),
                        attr(&attrs, "id").or_else(|| attr(&attrs, "rid")),
                    ) {
                        let (bold, italic) = match tag {
                            "embedBold" => (true, false),
                            "embedItalic" => (false, true),
                            "embedBoldItalic" => (true, true),
                            _ => (false, false),
                        };
                        out.push(OoxmlFontRef {
                            family: fam.clone(),
                            bold,
                            italic,
                            rel_id: id.to_string(),
                            font_key: attr(&attrs, "fontKey").map(|s| s.to_string()),
                        });
                    }
                }
                _ => {}
            },
            Tok::Close(name) if local(&name) == "font" => current = None,
            _ => {}
        }
    }
    out
}

/// Parse an OOXML obfuscation GUID (`{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}`)
/// into its 16 raw bytes in **string order** — i.e. the first hex pair is byte 0.
/// The de-obfuscation XOR key reverses this (see [`deobfuscate_odttf`]). Returns
/// `None` unless exactly 32 hex digits are present.
fn parse_guid(guid: &str) -> Option<[u8; 16]> {
    let hex: Vec<u8> = guid.bytes().filter(|b| b.is_ascii_hexdigit()).collect();
    if hex.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        let hi = (hex[i * 2] as char).to_digit(16)? as u8;
        let lo = (hex[i * 2 + 1] as char).to_digit(16)? as u8;
        *byte = (hi << 4) | lo;
    }
    Some(out)
}

/// De-obfuscate an OOXML embedded font part (`.odttf`). ECMA-376 §17.8.1: the
/// first **32 bytes** of the program are XORed with a 16-byte key derived from
/// the `fontKey` GUID, applied twice (bytes 0..16 and 16..32). The GUID bytes
/// are used in **reverse** of their textual order, which is exactly the
/// little-endian layout of the GUID's hex string read back-to-front. The rest of
/// the file is the untouched TTF/OTF program.
fn deobfuscate_odttf(data: &[u8], guid_str_order: &[u8; 16]) -> Vec<u8> {
    let mut out = data.to_vec();
    // Key = GUID bytes in reverse string order.
    let mut key = [0u8; 16];
    for (i, k) in key.iter_mut().enumerate() {
        *k = guid_str_order[15 - i];
    }
    let n = out.len().min(32);
    for (i, b) in out.iter_mut().take(n).enumerate() {
        *b ^= key[i % 16];
    }
    out
}

// ─────────────────────────────── ODF embedded fonts ───────────────────────────

/// Extract embedded faces from an ODF package. The `<style:font-face>` entries in
/// `content.xml`/`styles.xml` declare the family and (via
/// `<svg:font-face-uri xlink:href>`) the `Fonts/*` part holding a plain TTF/OTF.
/// Weight/italic are read from the font-face's `fo:font-weight`/`fo:font-style`.
fn odf_embedded_fonts(zip: &BTreeMap<String, Vec<u8>>) -> Vec<ProvidedFont> {
    let mut out: Vec<ProvidedFont> = Vec::new();
    let mut seen: Vec<(String, bool, bool)> = Vec::new();
    for part_name in ["content.xml", "styles.xml"] {
        let xml = part(zip, part_name);
        if xml.is_empty() {
            continue;
        }
        for r in parse_odf_font_faces(&xml) {
            let Some(raw) = zip.get(&r.href) else {
                continue;
            };
            let dedup = (r.family.to_ascii_lowercase(), r.bold, r.italic);
            if seen.contains(&dedup) {
                continue;
            }
            if let Some(font) = make_provided_font(&r.family, r.bold, r.italic, raw.clone()) {
                seen.push(dedup);
                out.push(font);
            }
        }
    }
    out
}

/// One embedded ODF face: family + weight/italic + the `Fonts/*` zip key.
struct OdfFontRef {
    family: String,
    bold: bool,
    italic: bool,
    href: String,
}

/// Parse ODF `<style:font-face>` blocks into [`OdfFontRef`]s. A face is emitted
/// only when it contains an `<svg:font-face-uri>` pointing at an embedded part
/// (declarations without an embedded file are skipped — the family is still
/// referenced and resolved by the host).
fn parse_odf_font_faces(xml: &str) -> Vec<OdfFontRef> {
    let mut out = Vec::new();
    // State for the currently-open <style:font-face>.
    let mut family: Option<String> = None;
    let mut bold = false;
    let mut italic = false;
    let mut href: Option<String> = None;
    let mut x = Xml::new(xml);
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, self_closing) => match local(&name) {
                "font-face" => {
                    family = attr(&attrs, "name")
                        .or_else(|| attr(&attrs, "font-family"))
                        .map(|s| s.trim_matches('\'').trim_matches('"').to_string());
                    bold = attr(&attrs, "font-weight")
                        .map(odf_weight_is_bold)
                        .unwrap_or(false);
                    italic = attr(&attrs, "font-style")
                        .map(|s| matches!(s, "italic" | "oblique"))
                        .unwrap_or(false);
                    href = None;
                }
                "font-face-uri" => {
                    if let Some(h) = attr(&attrs, "href") {
                        href = Some(h.trim_start_matches('/').to_string());
                    }
                    // `<svg:font-face-uri>` is usually a container; the close is
                    // handled below. If it self-closes, flush immediately.
                    if self_closing {
                        flush_odf_face(&family, bold, italic, &href, &mut out);
                        href = None;
                    }
                }
                _ => {}
            },
            Tok::Close(name) => match local(&name) {
                "font-face-uri" => {
                    flush_odf_face(&family, bold, italic, &href, &mut out);
                    href = None;
                }
                "font-face" => {
                    family = None;
                    href = None;
                    bold = false;
                    italic = false;
                }
                _ => {}
            },
            Tok::Text(_) => {}
        }
    }
    out
}

/// Emit an [`OdfFontRef`] when the open font-face has both a family and an href.
fn flush_odf_face(
    family: &Option<String>,
    bold: bool,
    italic: bool,
    href: &Option<String>,
    out: &mut Vec<OdfFontRef>,
) {
    if let (Some(fam), Some(h)) = (family, href) {
        if !fam.is_empty() && !h.is_empty() {
            out.push(OdfFontRef {
                family: fam.clone(),
                bold,
                italic,
                href: h.clone(),
            });
        }
    }
}

/// ODF `fo:font-weight` → bold? Accepts `bold` and numeric weights (≥600).
fn odf_weight_is_bold(w: &str) -> bool {
    if w.eq_ignore_ascii_case("bold") {
        return true;
    }
    w.parse::<u16>().map(|n| n >= 600).unwrap_or(false)
}

// ─────────────────────────── shared face construction ─────────────────────────

/// Validate a (de-obfuscated / raw) font program and wrap it as a
/// [`ProvidedFont`]. Accepts both glyf-TrueType and OpenType-CFF (`OTTO`) — the
/// renderer embeds either — so a CFF-flavoured embedded face still renders with
/// its real glyphs. Returns `None` for bytes that aren't a usable sfnt program.
fn make_provided_font(
    family: &str,
    bold: bool,
    italic: bool,
    program: Vec<u8>,
) -> Option<ProvidedFont> {
    let family = family.trim();
    if family.is_empty() {
        return None;
    }
    if !is_sfnt_font(&program) {
        return None;
    }
    Some(ProvidedFont {
        family: family.to_string(),
        weight: if bold { 700 } else { 400 },
        italic,
        ttf: program,
    })
}

/// Cheap structural check that `bytes` is a usable sfnt font program — glyf
/// TrueType (`0x00010000` or `true`), an `OTTO` OpenType-CFF, or a `ttcf`
/// collection. `parse_metrics` accepts all three (it tolerates the missing
/// `glyf` of a CFF font), matching what the renderer's `embed_font` can embed.
fn is_sfnt_font(bytes: &[u8]) -> bool {
    matches!(
        bytes.get(0..4),
        Some(b"\x00\x01\x00\x00") | Some(b"OTTO") | Some(b"true") | Some(b"ttcf")
    ) && crate::font::truetype::TrueTypeFont::parse_metrics(bytes).is_some()
}

// ─────────────────────────── referenced fonts (phase 1) ───────────────────────

/// The fonts an Office container **references but does not embed** — the set the
/// host should fetch (Google Fonts / system) before [`office_to_pdf`] so styled
/// runs lay out and paint with the right face. Families the container embeds
/// itself ([`extract_embedded_fonts`]) and the base-14 standards are excluded:
/// the former render from the embedded bytes, the latter from the bundled
/// substitute — neither needs a host fetch.
///
/// Returns `None` for an unrecognized archive. Use this for the
/// fetch-then-supply (two-phase) host flow; if you skip it, referenced-but-
/// unembedded families fall back to the nearest bundled metric-compatible face.
pub fn office_needed_fonts(bytes: &[u8]) -> Option<Vec<FontRequest>> {
    // Legacy OLE2 carries no font program references we can resolve.
    if bytes.len() >= 8 && bytes[..8] == [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1] {
        return Some(Vec::new());
    }
    let zip = read_zip(bytes);
    // Render to HTML once (per format) and ask the HTML engine which families it
    // references; then drop the ones the container already embeds.
    let body = office_body_html(&zip)?;
    let embedded: Vec<String> = extract_embedded_fonts(&zip)
        .iter()
        .map(|f| f.family.to_ascii_lowercase())
        .collect();
    let reqs = crate::html::needed_fonts(&html_doc(&body))
        .into_iter()
        .filter(|r| !embedded.contains(&r.family.to_ascii_lowercase()))
        .collect();
    Some(reqs)
}

/// Build just the HTML `<body>` content for a recognized container (no render),
/// reusing each format's mapper. Shared by [`office_needed_fonts`] so the font
/// scan sees exactly the families the real render would.
fn office_body_html(zip: &BTreeMap<String, Vec<u8>>) -> Option<String> {
    if zip.contains_key("word/document.xml") {
        Some(docx_body_html(zip))
    } else if zip.contains_key("ppt/presentation.xml") {
        Some(pptx_body_html(zip))
    } else if zip.contains_key("xl/workbook.xml") {
        Some(xlsx_body_html(zip))
    } else if let Some(mimetype) = zip.get("mimetype") {
        let mt = String::from_utf8_lossy(mimetype);
        if mt.contains("opendocument.text") {
            Some(odt_body_html(zip))
        } else if mt.contains("opendocument.spreadsheet") {
            Some(ods_body_html(zip))
        } else if mt.contains("opendocument.presentation") {
            Some(odp_body_html(zip))
        } else {
            None
        }
    } else {
        None
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
    img_tag_sized(zip, key, None)
}

/// Like [`img_tag`] but stamps a `width`/`height` (points) on the `<img>` when a
/// size is given. The HTML engine reads an inline image's box from these
/// attributes (numeric = points), so without them an inline image falls back to
/// a fixed default box instead of its real footprint. Used for DOCX drawings
/// whose `wp:extent` declares the on-page size.
fn img_tag_sized(
    zip: &BTreeMap<String, Vec<u8>>,
    key: &str,
    size: Option<(f64, f64)>,
) -> Option<String> {
    let mime = image_mime(key)?;
    let bytes = zip.get(key)?;
    let dims = match size {
        Some((w, h)) if w > 0.0 && h > 0.0 => {
            format!(" width=\"{}\" height=\"{}\"", fmt_pt(w), fmt_pt(h))
        }
        _ => String::new(),
    };
    Some(format!(
        "<img src=\"data:{mime};base64,{}\"{dims}>",
        super::base64(bytes)
    ))
}

// ════════════════════════════════════ DOCX ════════════════════════════════════

/// DOCX → styled HTML → PDF. Maps paragraph styles to headings, run properties
/// (`b`/`i`/`sz`/`color`/`u`) to inline `<span>`s, `w:tbl`→`<table>`, and inline
/// images via `a:blip r:embed` resolved through the document relationships.
pub fn docx_to_pdf(zip: &BTreeMap<String, Vec<u8>>) -> Vec<u8> {
    docx_to_pdf_with(zip, &[])
}

/// Like [`docx_to_pdf`] but also feeds `host` faces (phase 2 of
/// [`office_needed_fonts`]): the DOCX's own embedded faces (`word/fonts/*.odttf`)
/// are [merged](merge_fonts) with the host-supplied ones (embedded wins) so a
/// referenced-but-unembedded family (Calibri→Carlito) lays out with the right
/// metrics.
fn docx_to_pdf_with(zip: &BTreeMap<String, Vec<u8>>, host: &[ProvidedFont]) -> Vec<u8> {
    let (body, geom) = docx_body_geom(zip);
    render_geom_with_fonts(&body, geom, &merge_fonts(extract_embedded_fonts(zip), host))
}

/// Build the DOCX HTML `<body>` and resolve its page geometry, without
/// rendering. Shared by [`docx_to_pdf`] (which then renders it) and the
/// font-need scan ([`office_needed_fonts`]) so both see identical markup.
fn docx_body_geom(zip: &BTreeMap<String, Vec<u8>>) -> (String, PageGeom) {
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
        page_h: geom.h,
    };

    let mut body = String::new();
    // Headers precede the main flow; footers follow it (single-flow render).
    docx_header_footer(zip, &ctx, "header", &mut body);
    docx_body(&doc, &ctx, &mut body);
    docx_footnotes_section(&ctx, &mut body);
    docx_header_footer(zip, &ctx, "footer", &mut body);
    (body, geom)
}

/// The DOCX HTML `<body>` only (geometry dropped) — used by the font-need scan.
fn docx_body_html(zip: &BTreeMap<String, Vec<u8>>) -> String {
    docx_body_geom(zip).0
}

/// Per-document DOCX context threaded through the body walker: media/relationship
/// access plus the resolved styles, numbering and footnotes tables.
struct DocxCtx<'a> {
    zip: &'a BTreeMap<String, Vec<u8>>,
    rels: &'a BTreeMap<String, String>,
    styles: &'a DocxStyles,
    numbering: &'a DocxNumbering,
    footnotes: &'a DocxFootnotes,
    /// Page height (points). Used to flip a floating drawing's top-left
    /// `wp:posOffset` into the model's lower-left [`model::Block::frame`].
    page_h: f64,
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
    /// Strike-through from `w:strike`/`w:dstrike` (DOCX) — single and double
    /// strike collapse to one strike in the model.
    strike: bool,
    /// Text-highlight / run shading as 6-hex (no `#`): `w:highlight@val` (a named
    /// colour, mapped to hex) or `w:shd@fill` (already hex). `None` ⇒ no
    /// background. Surfaced as the model run's [`CharStyle::background`].
    highlight: Option<String>,
    size_half_pt: Option<f64>,
    color: Option<String>,
    /// Typeface name from `w:rFonts@ascii` (DOCX) / `a:latin@typeface` (PPTX) /
    /// `fo:font-name` (ODF). Surfaced as `font-family` so the host two-phase
    /// font fetch embeds the real face and the layout uses its true metrics.
    font_family: Option<String>,
    /// Hyperlink target URL for the run (PPTX `a:hlinkClick@r:id` resolved through
    /// the slide rels). `None` ⇒ a plain run; `Some` ⇒ wrapped in an
    /// [`Inline::Link`] when pushed. Only consulted by the PPTX model path.
    hyperlink: Option<String>,
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
    /// Per-side paragraph borders from `w:pPr/w:pBdr` (`[top, right, bottom,
    /// left]`). Each present side becomes a `border-{side}` declaration so the
    /// "encadré" (framed paragraph, e.g. a boxed invoice note) is drawn.
    borders: [Option<BorderSide>; 4],
    /// Paragraph shading colour from `w:pPr/w:shd@w:fill` (6-hex, no `#`) →
    /// `background-color`. `auto`/missing leaves it `None`.
    shading: Option<String>,
}

/// One side of a paragraph border (`w:pBdr/w:{top,left,bottom,right}`), reduced
/// to what the HTML engine renders: a width in points (`w:sz`, eighths of a
/// point) and an optional colour (`w:color`, 6-hex). `w:val` (the line style:
/// `single`/`dashed`/…) is mapped to a CSS keyword purely for fidelity — the
/// engine treats every visible border as solid — and `w:val="nil"`/`"none"`
/// suppresses the side entirely.
#[derive(Clone)]
struct BorderSide {
    /// Border width in points (from `w:sz`, eighths of a point).
    width_pt: f64,
    /// CSS line-style keyword mapped from `w:val` (default `solid`).
    style: &'static str,
    /// Border colour as 6-hex (no `#`), when `w:color` is a real value.
    color: Option<String>,
}

/// Map an OOXML `w:pBdr` side (its attributes) to a [`BorderSide`], or `None`
/// when the side is absent (`w:val="nil"`/`"none"`) or carries no width.
/// `w:sz` is eighths of a point; a side with `w:sz="0"` but a real style still
/// renders as a hairline (1px), matching Word.
fn parse_border_side(attrs: &[(String, String)]) -> Option<BorderSide> {
    let val = attr(attrs, "val").unwrap_or("single");
    if matches!(val, "nil" | "none") {
        return None;
    }
    // `w:sz` is in eighths of a point; clamp a zero/absent size up to a hairline
    // so a declared-but-thin border is still visible.
    let width_pt = attr(attrs, "sz")
        .and_then(|v| v.trim().parse::<f64>().ok())
        .map(|eighths| eighths / 8.0)
        .filter(|w| *w > 0.0)
        .unwrap_or(0.75);
    let color = attr(attrs, "color")
        .filter(|v| *v != "auto" && is_hex6(v))
        .map(|v| v.to_ascii_uppercase());
    Some(BorderSide {
        width_pt,
        style: border_val_to_css(val),
        color,
    })
}

/// Reduce a parsed [`BorderSide`] to the model's single [`model::BorderStyle`]
/// (`width` in points + RGB `color`). The model carries one table-wide border —
/// no per-side, no line-style — so only the width and colour survive; an unset
/// or `auto` colour defaults to black, mirroring the export side
/// (`export_model::docx_tbl_borders`, which writes `w:sz = width*8` eighths and
/// `hex(color)`).
fn border_side_to_model(side: &BorderSide) -> model::BorderStyle {
    let color = side
        .color
        .as_deref()
        .and_then(hex_to_rgb_f64)
        .unwrap_or([0.0, 0.0, 0.0]);
    model::BorderStyle {
        width: side.width_pt,
        color,
    }
}

/// Map a `w:pBdr` side's `w:val` line style to the nearest CSS border-style
/// keyword. The HTML engine renders any visible border as solid, so this only
/// affects the emitted markup's fidelity; unknown styles fall back to `solid`.
fn border_val_to_css(val: &str) -> &'static str {
    match val {
        "dashed" | "dashSmallGap" | "dotDash" | "dotDotDash" => "dashed",
        "dotted" => "dotted",
        "double" | "thinThickThinSmallGap" | "thickThinSmallGap" | "triple" => "double",
        _ => "solid",
    }
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
        // Paragraph shading (`w:shd@w:fill`) → a background fill behind the text.
        if let Some(fill) = &self.shading {
            css.push_str(&format!("background-color:#{fill};"));
        }
        // Per-side paragraph borders (`w:pBdr`) → `border-{side}` declarations,
        // drawing the "encadré" around the text.
        const SIDE_PROP: [&str; 4] = ["border-top", "border-right", "border-bottom", "border-left"];
        let mut has_border = false;
        for (i, side) in self.borders.iter().enumerate() {
            if let Some(b) = side {
                has_border = true;
                css.push_str(&format!("{}:{}pt {}", SIDE_PROP[i], fmt_pt(b.width_pt), b.style));
                if let Some(c) = &b.color {
                    css.push_str(&format!(" #{c}"));
                }
                css.push(';');
            }
        }
        // A small inset so the frame/shading doesn't crowd the glyphs (Word draws
        // `w:pBdr`/`w:shd` with a default offset). Only when there's a box to inset.
        if has_border || self.shading.is_some() {
            css.push_str("padding:2pt;");
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

/// What a `<w:drawing>` resolves to, once its envelope is inspected.
///
/// A `wp:inline` (or an enveloped-but-not-anchored drawing) stays in the text
/// flow as an `<img>`; a `wp:anchor` (a floating/anchored object) is lifted out
/// of flow into an absolutely-positioned `<div>` so a corner logo, a floating
/// image, or a text-box keeps its page position instead of collapsing into the
/// paragraph flow.
enum DrawingResult {
    /// Markup that belongs inline in the run (typically an `<img>`).
    Inline(String),
    /// An absolutely-positioned wrapper `<div>` to emit as a paragraph sibling.
    Float(String),
    /// Nothing renderable (e.g. an embed that resolved to no media).
    Empty,
}

/// An anchored drawing's placement, gathered from `wp:anchor`:
/// `wp:positionH/positionV → wp:posOffset` (absolute EMU offset) or `wp:align`
/// (relative keyword), and the `wp:extent@cx/@cy` size. Offsets are taken
/// relative to the page content box (the layout engine's containing block for
/// `position:absolute`), which is exactly right for `relativeFrom="margin"` and
/// a sound, non-regressing approximation for `page`-relative anchors.
#[derive(Default)]
struct AnchorBox {
    /// Horizontal offset in points (`wp:positionH/wp:posOffset`).
    off_x: Option<f64>,
    /// Vertical offset in points (`wp:positionV/wp:posOffset`).
    off_y: Option<f64>,
    /// `wp:positionH/wp:align` — `left` / `center` / `right`.
    align_h: Option<&'static str>,
    /// `wp:positionV/wp:align` — `top` / `center` / `bottom`.
    align_v: Option<&'static str>,
    /// Box width in points (`wp:extent@cx`).
    w: Option<f64>,
    /// Box height in points (`wp:extent@cy`).
    h: Option<f64>,
}

impl AnchorBox {
    /// The inline CSS for the absolute wrapper. `wp:posOffset` maps to
    /// `left`/`top`; a `wp:align` keyword maps to the matching edge inset
    /// (`right:0`/`bottom:0`) or centring via an auto margin. Width/height come
    /// from `wp:extent` when present so the box reserves its real footprint.
    fn abs_style(&self) -> String {
        let mut css = String::from("position:absolute");
        match (self.off_x, self.align_h) {
            (Some(x), _) => css.push_str(&format!(";left:{}pt", fmt_pt(x))),
            (None, Some("right")) => css.push_str(";right:0pt"),
            (None, Some("center")) => {
                css.push_str(";left:0pt;right:0pt;margin-left:auto;margin-right:auto")
            }
            (None, Some(_)) => css.push_str(";left:0pt"), // "left"/inside/outside → left edge
            (None, None) => css.push_str(";left:0pt"),
        }
        match (self.off_y, self.align_v) {
            (Some(y), _) => css.push_str(&format!(";top:{}pt", fmt_pt(y))),
            (None, Some("bottom")) => css.push_str(";bottom:0pt"),
            (None, Some(_)) => css.push_str(";top:0pt"), // "top"/"center" → top edge
            (None, None) => css.push_str(";top:0pt"),
        }
        if let Some(w) = self.w {
            css.push_str(&format!(";width:{}pt", fmt_pt(w)));
        }
        if let Some(h) = self.h {
            css.push_str(&format!(";height:{}pt", fmt_pt(h)));
        }
        css
    }
}

/// Consume a whole `<w:drawing>` subtree (its open tag already seen) up to
/// `</w:drawing>` and resolve it to inline or floating markup.
///
/// Detection: an enclosed `wp:anchor` means a floating object → absolute
/// `<div>`; otherwise (`wp:inline`, or a bare drawing) the content stays inline.
/// The drawing's body is either an image (`a:blip`) or a Word/VML text box
/// (`w:txbxContent`, reached through `mc:AlternateContent`/`wps:txbx`/
/// `v:textbox`) — the text box is rendered as its own styled box (the
/// "encadré"). For a float we wrap whatever body we built in the absolutely-
/// positioned `<div>`; inline drawings emit the body as-is.
fn docx_drawing(x: &mut Xml, ctx: &DocxCtx) -> DrawingResult {
    let mut anchored = false;
    let mut anchor = AnchorBox::default();
    // Which axis a `wp:posOffset`/`wp:align` we are reading belongs to.
    let mut cur_axis: Option<bool> = None; // Some(true)=H, Some(false)=V
    let mut body = String::new();
    // True while inside `wp:extent` so a stray `a:ext` (image scale) cannot
    // override the drawing's declared footprint.
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => match local(&name) {
                "anchor" => anchored = true,
                "extent" => {
                    // `wp:extent@cx/@cy` is the drawing's overall size (EMU).
                    anchor.w = attr(&attrs, "cx").and_then(emu_to_pt).or(anchor.w);
                    anchor.h = attr(&attrs, "cy").and_then(emu_to_pt).or(anchor.h);
                }
                "positionH" => cur_axis = Some(true),
                "positionV" => cur_axis = Some(false),
                "align" => {}     // value arrives as the element's text node
                "posOffset" => {} // value arrives as the element's text node
                // A blip inside the drawing → embed the referenced image, sized
                // to the drawing's `wp:extent` (read just above) so an inline
                // image gets its real on-page footprint instead of a default box.
                "blip" => {
                    let size = match (anchor.w, anchor.h) {
                        (Some(w), Some(h)) => Some((w, h)),
                        _ => None,
                    };
                    if let Some(tag) = blip_img(ctx, &attrs, size) {
                        body.push_str(&tag);
                    }
                }
                // A Word/VML text box: render its paragraphs as a styled box.
                "txbxContent" if !sc => {
                    let mut inner = String::new();
                    docx_walk(x, ctx, &mut inner, Some("txbxContent"));
                    if !inner.trim().is_empty() {
                        body.push_str(&format!(
                            "<div style=\"border:1px solid #000;padding:2pt\">{inner}</div>"
                        ));
                    }
                }
                _ => {}
            },
            Tok::Close(name) => {
                if local(&name) == "drawing" {
                    break;
                }
            }
            Tok::Text(t) => {
                // `wp:posOffset` / `wp:align` carry their value as text; route it
                // to the current axis (H/V) set by the enclosing position element.
                let v = t.trim();
                if v.is_empty() {
                    continue;
                }
                if let Ok(emu) = v.parse::<f64>() {
                    let pts = emu / EMU_PER_PT;
                    match cur_axis {
                        Some(true) => anchor.off_x = Some(pts),
                        Some(false) => anchor.off_y = Some(pts),
                        None => {}
                    }
                } else {
                    let kw = match v {
                        "left" | "center" | "right" => Some(v),
                        "top" => Some("top"),
                        "bottom" => Some("bottom"),
                        _ => None,
                    };
                    if let Some(k) = kw {
                        // Map borrowed slice to a 'static keyword for storage.
                        let s: &'static str = match k {
                            "left" => "left",
                            "center" => "center",
                            "right" => "right",
                            "top" => "top",
                            "bottom" => "bottom",
                            _ => "left",
                        };
                        match cur_axis {
                            Some(true) => anchor.align_h = Some(s),
                            Some(false) => anchor.align_v = Some(s),
                            None => {}
                        }
                    }
                }
            }
        }
    }

    if body.trim().is_empty() {
        return DrawingResult::Empty;
    }
    if anchored {
        DrawingResult::Float(format!(
            "<div style=\"{}\">{body}</div>",
            anchor.abs_style()
        ))
    } else {
        DrawingResult::Inline(body)
    }
}

/// Resolve an `a:blip@r:embed`/`@r:link` to an `<img>` data-URI via the document
/// relationships + media, or `None` when the target is missing/unsupported.
/// `size` is the drawing's `wp:extent` (points) when known, so the inline image
/// is laid out at its declared footprint rather than a default box.
fn blip_img(ctx: &DocxCtx, attrs: &[(String, String)], size: Option<(f64, f64)>) -> Option<String> {
    let rid = attr(attrs, "embed").or_else(|| attr(attrs, "link"))?;
    ctx.rels
        .get(rid)
        .map(|t| resolve_target("word", t))
        .and_then(|k| img_tag_sized(ctx.zip, &k, size))
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
    let mut in_pbdr = false; // inside <w:pPr>/<w:pBdr> (paragraph borders)
    let mut depth = 0i32; // nesting of <w:r> runs (to scope rPr)
    let mut field_instr = String::new(); // accumulating <w:instrText>
    let mut in_instr = false;
    // Absolutely-positioned drawings (`wp:anchor`) collected here and flushed as
    // paragraph siblings after the `<p>`, so they keep their page position
    // instead of being trapped in the text flow.
    let mut floats = String::new();
    // `w:pPr/w:pageBreakBefore` → this paragraph starts a new page.
    let mut page_break_before = false;
    // A run-level `<w:br w:type="page"/>` → force a new page after this paragraph.
    let mut page_break_after = false;

    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => {
                let ln = local(&name);
                match ln {
                    "pPr" if !sc => in_ppr = true,
                    "pageBreakBefore" if in_ppr => {
                        // `w:val="0"`/`"false"` cancels an inherited page break.
                        page_break_before =
                            !matches!(attr(&attrs, "val"), Some("0") | Some("false"));
                    }
                    "sectPr" if in_ppr => {
                        // A section break carried on a paragraph (`w:pPr/w:sectPr`)
                        // ends a section: the following content starts a new page
                        // (the default `nextPage` section start). The document's
                        // final `w:sectPr` is a direct `w:body` child, not here, so
                        // this never adds a spurious trailing page.
                        page_break_after = true;
                    }
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
                    // `w:pPr/w:pBdr` opens the per-side paragraph border group;
                    // its `w:top`/`w:left`/`w:bottom`/`w:right` children carry the
                    // width/style/colour of each side (the "encadré" frame).
                    "pBdr" if in_ppr && !sc => in_pbdr = true,
                    "top" if in_pbdr => para.borders[0] = parse_border_side(&attrs),
                    "right" if in_pbdr => para.borders[1] = parse_border_side(&attrs),
                    "bottom" if in_pbdr => para.borders[2] = parse_border_side(&attrs),
                    "left" if in_pbdr => para.borders[3] = parse_border_side(&attrs),
                    // `w:pPr/w:shd@w:fill` shades the paragraph background. Guard
                    // against the run-mark `w:pPr/w:rPr/w:shd` (run shading) so only
                    // the paragraph-level fill is taken.
                    "shd" if in_ppr && !in_rpr => {
                        para.shading = attr(&attrs, "fill")
                            .filter(|v| *v != "auto" && is_hex6(v))
                            .map(|v| v.to_ascii_uppercase());
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
                    "br" if matches!(attr(&attrs, "type"), Some("page")) => {
                        // An explicit run-level page break (`<w:br w:type="page"/>`)
                        // ends the current line and forces a new page after this
                        // paragraph (flushed as a sibling so the break is a real
                        // block boundary, not nested in `<p>`).
                        inner.push_str("<br>");
                        page_break_after = true;
                    }
                    "br" | "cr" => inner.push_str("<br>"),
                    // A drawing: inline images stay in the run; anchored
                    // (floating) objects are lifted into absolute siblings.
                    "drawing" if !sc => match docx_drawing(x, ctx) {
                        DrawingResult::Inline(tag) => inner.push_str(&tag),
                        DrawingResult::Float(div) => floats.push_str(&div),
                        DrawingResult::Empty => {}
                    },
                    // Bare blip outside a `<w:drawing>` (legacy/VML) → inline image.
                    // No `wp:extent` here, so the image keeps its default box.
                    "blip" => {
                        if let Some(tag) = blip_img(ctx, &attrs, None) {
                            inner.push_str(&tag);
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
                    "pBdr" => in_pbdr = false,
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

    // A `w:pageBreakBefore` paragraph opens a new page: emit a block break
    // *before* the paragraph so the engine advances to the next page boundary.
    if page_break_before {
        out.push_str(PAGE_BREAK_DIV);
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

    // A run-level `<w:br w:type="page"/>` forces the next page *after* this
    // paragraph's content.
    if page_break_after {
        out.push_str(PAGE_BREAK_DIV);
    }

    // Anchored (floating) drawings ride along as paragraph siblings: being
    // out-of-flow (`position:absolute`), they anchor to the page content box at
    // their own coordinates regardless of where they sit in the body stream.
    out.push_str(&floats);
}

/// A block element the HTML engine treats as a hard page break
/// (`page-break-before: always`); used for DOCX explicit page breaks.
const PAGE_BREAK_DIV: &str = "<div style=\"page-break-before:always\"></div>";

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

/// The unresolved identity + own formatting of one `w:style` from
/// `word/styles.xml`, kept so the named-style **table** can be lowered into
/// [`Document.styles`](crate::model::Document::styles) without the `basedOn`
/// flattening that [`DocxStyles::by_id`] applies (the model keeps the
/// inheritance edge explicitly via [`NamedStyle::based_on`]).
#[derive(Default, Clone)]
struct DocxRawStyle {
    /// `w:style@w:type` (`paragraph`/`character`/`table`/`numbering`).
    kind: Option<String>,
    /// `w:basedOn@w:val` — the parent style id (→ [`NamedStyle::based_on`]).
    based_on: Option<String>,
    /// The style's **own** `w:pPr`/`w:rPr` formatting (not basedOn-flattened).
    own: DocxStyle,
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
    /// styleId → unresolved identity + own formatting, in document order, for
    /// lowering the named-style **table** (`Document.styles`) with `based_on`
    /// edges intact. Distinct from [`by_id`](DocxStyles::by_id), which is
    /// flattened for inline paragraph resolution.
    raw: BTreeMap<String, DocxRawStyle>,
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

    /// Lower the parsed `w:style` entries into the model's [`StyleTable`] so each
    /// paragraph's `style_ref` (set from `w:pStyle`) resolves to a present
    /// [`NamedStyle`]. Only **paragraph** styles are lowered (the model's named
    /// styles are paragraph+character defaults; `character`/`table`/`numbering`
    /// styles have no paragraph identity to host and are skipped). Each style's
    /// **own** `w:pPr`/`w:rPr` is lowered with the same field mappings as direct
    /// paragraph/run formatting (alignment/spacing/indent/line-height + font/
    /// size/bold/italic/underline/colour); `w:basedOn` becomes
    /// [`NamedStyle::based_on`] (the edge is kept, not flattened, so the model
    /// can resolve inheritance itself). `w:name` (the human display name) has no
    /// model slot — the [`StyleId`] key carries the machine id — so it is not
    /// retained.
    fn to_style_table(&self) -> model::StyleTable {
        let mut named = BTreeMap::new();
        for (id, raw) in &self.raw {
            // Default `w:type` is `paragraph` (ECMA-376 §17.7.4.17), so a missing
            // `w:type` is treated as a paragraph style.
            let is_para = raw
                .kind
                .as_deref()
                .map(|k| k == "paragraph")
                .unwrap_or(true);
            if !is_para {
                continue;
            }
            named.insert(
                model::StyleId(id.clone()),
                model::NamedStyle {
                    para: docx_style_to_paragraph(&raw.own),
                    char_: docx_style_to_char(&raw.own),
                    based_on: raw.based_on.clone().map(model::StyleId),
                },
            );
        }
        model::StyleTable { named }
    }
}

/// Lower a [`DocxStyle`]'s paragraph (`w:pPr`) properties to a model
/// [`ParagraphStyle`], mirroring [`para_style_model`] (no list-indent: a named
/// style is not a list context). Unset fields fall back to the model defaults.
fn docx_style_to_paragraph(s: &DocxStyle) -> ParagraphStyle {
    ParagraphStyle {
        align: match s.align {
            Some("center") => MAlign::Center,
            Some("right") => MAlign::Right,
            Some("justify") => MAlign::Justify,
            _ => MAlign::Left,
        },
        space_before_pt: s.space_before_pt.unwrap_or(0.0),
        space_after_pt: s.space_after_pt.unwrap_or(0.0),
        indent_left_pt: s.indent_left_pt.unwrap_or(0.0),
        indent_right_pt: s.indent_right_pt.unwrap_or(0.0),
        first_line_pt: s.first_line_pt.unwrap_or(0.0),
        // `DocxStyle::line_height` is the local importer enum; map it to the
        // model's, exactly as [`para_style_model`].
        line_height: match s.line_height {
            Some(LineHeight::Multiple(m)) => MLineHeight::Multiple(m),
            Some(LineHeight::Points(p)) => MLineHeight::Points(p),
            None => MLineHeight::Normal,
        },
    }
}

/// Lower a [`DocxStyle`]'s run (`w:rPr`) properties to a model [`CharStyle`],
/// using the same field mappings as [`apply_named_run_defaults`]
/// (`w:sz` half-points → points, `w:color` hex → RGB, `w:rFonts` → family +
/// portable [`Generic`](super::style::Generic) class). Unset fields fall back to
/// the [`CharStyle`] defaults.
fn docx_style_to_char(s: &DocxStyle) -> CharStyle {
    let mut c = CharStyle {
        bold: s.bold == Some(true),
        italic: s.italic == Some(true),
        underline: s.underline == Some(true),
        size_pt: s.size_half_pt.map(|h| h / 2.0).unwrap_or(0.0),
        color: s.color.as_deref().and_then(hex_to_rgb_f64),
        ..CharStyle::default()
    };
    if let Some(fam) = &s.font_family {
        c.generic = super::style::parse_base_font(fam).generic;
        c.family = fam.clone();
    }
    c
}

/// Parse `word/styles.xml` into a [`DocxStyles`]: read each `w:style`'s direct
/// `w:rPr`/`w:pPr` and `w:basedOn`, then flatten the inheritance chains so each
/// id maps to its fully-resolved formatting. `w:docDefaults` seeds the baseline.
fn parse_docx_styles(xml: &str) -> DocxStyles {
    // Raw, pre-resolution data per style id: (basedOn, own props). Used to
    // flatten the inline-resolution table `by_id`.
    let mut raw: BTreeMap<String, (Option<String>, DocxStyle)> = BTreeMap::new();
    // Identity + own props per style id, for the model style table (kept with
    // `basedOn` edges, not flattened).
    let mut raw_styles: BTreeMap<String, DocxRawStyle> = BTreeMap::new();
    let mut defaults = DocxStyle::default();

    let mut x = Xml::new(xml);
    // Walk state.
    let mut cur_id: Option<String> = None;
    let mut cur_based: Option<String> = None;
    let mut cur_kind: Option<String> = None; // <w:style w:type>
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
                        cur_kind = attr(&attrs, "type").map(|s| s.to_string());
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
                        let based = cur_based.take();
                        let own = std::mem::take(&mut cur);
                        // Keep the identity + own props for the model style table
                        // (basedOn kept, not flattened).
                        raw_styles.insert(
                            id.clone(),
                            DocxRawStyle {
                                kind: cur_kind.take(),
                                based_on: based.clone(),
                                own: own.clone(),
                            },
                        );
                        raw.insert(id, (based, own));
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

    DocxStyles {
        defaults,
        by_id,
        raw: raw_styles,
    }
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
/// Built from `word/numbering.xml`'s `w:num → w:abstractNumId → w:lvl@w:numFmt`,
/// with each `w:num`'s `w:lvlOverride/w:lvl/w:numFmt` applied on top of its
/// abstract definition so a list-instance that re-formats a level resolves to
/// the overriding format.
///
/// Only the per-level *format* is retained, because that is all the model's
/// [`List`]/[`ListMarker`] can express. The level's `w:start` /
/// `w:lvlOverride/w:startOverride` (restart-at-N) and `w:lvlText` (custom
/// prefix template such as `%1)` or legal `%1.%2`) are **not** carried: the
/// model derives ordinals positionally (no start field) and renders ordered
/// markers with a fixed `.` suffix (no template field). See `docs/CONVERSIONS.md`.
#[derive(Default)]
struct DocxNumbering {
    /// numId → (level → format). Levels are 0-based; `w:lvlOverride` formats are
    /// already folded in.
    by_num: BTreeMap<u32, BTreeMap<u32, NumFmt>>,
}

impl DocxNumbering {
    /// Format for a given list (`numId`) at `level`, if known.
    fn fmt(&self, num_id: u32, level: u32) -> Option<NumFmt> {
        self.by_num.get(&num_id)?.get(&level).copied()
    }
}

/// Parse `word/numbering.xml`: collect `w:abstractNum` level formats, then map
/// each `w:num@w:numId` to its `w:abstractNumId`, folding in any per-instance
/// `w:lvlOverride/w:lvl/w:numFmt`. Returns numId → level → format.
fn parse_docx_numbering(xml: &str) -> DocxNumbering {
    // abstractNumId → (level → format).
    let mut abstracts: BTreeMap<u32, BTreeMap<u32, NumFmt>> = BTreeMap::new();
    // numId → abstractNumId.
    let mut num_to_abstract: BTreeMap<u32, u32> = BTreeMap::new();
    // numId → (level → overriding format) from `w:lvlOverride/w:lvl/w:numFmt`.
    let mut num_overrides: BTreeMap<u32, BTreeMap<u32, NumFmt>> = BTreeMap::new();

    let mut x = Xml::new(xml);
    let mut cur_abstract: Option<u32> = None;
    // Current `w:lvl@w:ilvl` while inside a `w:abstractNum` body.
    let mut cur_level: Option<u32> = None;
    // num mapping context.
    let mut cur_num: Option<u32> = None;
    // Inside a `<w:num>` body (vs a `<w:abstractNum>`).
    let mut in_num = false;
    // Current `w:lvlOverride@w:ilvl` while inside a `w:num` body (the level a
    // nested `w:lvl/w:numFmt` overrides).
    let mut cur_override_level: Option<u32> = None;

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
                    cur_override_level = None;
                }
                // `w:ilvl` selects the level inside an abstract's `w:lvl`, or the
                // overridden level inside a `w:num`'s `w:lvlOverride`.
                "lvl" if !in_num => {
                    cur_level = attr(&attrs, "ilvl").and_then(|v| v.trim().parse::<u32>().ok());
                }
                // A `w:lvlOverride@w:ilvl` re-defines that level for this list
                // instance only; a nested `w:lvl/w:numFmt` carries the new format
                // (its own `w:ilvl` mirrors the override and is not re-read).
                "lvlOverride" if in_num => {
                    cur_override_level =
                        attr(&attrs, "ilvl").and_then(|v| v.trim().parse::<u32>().ok());
                }
                "numFmt" => {
                    if let Some(v) = attr(&attrs, "val") {
                        let fmt = NumFmt::parse(v);
                        if in_num {
                            // Inside `w:lvlOverride/w:lvl`: a per-instance override.
                            if let (Some(n), Some(l)) = (cur_num, cur_override_level) {
                                num_overrides.entry(n).or_default().insert(l, fmt);
                            }
                        } else if let (Some(a), Some(l)) = (cur_abstract, cur_level) {
                            abstracts.entry(a).or_default().insert(l, fmt);
                        }
                    }
                }
                "num" => {
                    in_num = true;
                    cur_num = attr(&attrs, "numId").and_then(|v| v.trim().parse::<u32>().ok());
                    cur_override_level = None;
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

    let mut by_num: BTreeMap<u32, BTreeMap<u32, NumFmt>> = BTreeMap::new();
    // Each `w:num` mapped to an abstract: its abstract's level formats.
    for (num_id, abstract_id) in &num_to_abstract {
        if let Some(levels) = abstracts.get(abstract_id) {
            by_num.insert(*num_id, levels.clone());
        }
    }
    // Apply each `w:num`'s `w:lvlOverride` formats on top (also seeds entries for
    // an override-only `w:num` whose abstract is absent — partial numbering.xml).
    for (num_id, over) in num_overrides {
        by_num.entry(num_id).or_default().extend(over);
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
    xlsx_to_pdf_with(zip, &[])
}

/// Like [`xlsx_to_pdf`] but also feeds `host` faces (phase 2 of
/// [`office_needed_fonts`]); embedded faces still win on conflict.
/// Spreadsheets have no single declared page size — rendered landscape for width.
fn xlsx_to_pdf_with(zip: &BTreeMap<String, Vec<u8>>, host: &[ProvidedFont]) -> Vec<u8> {
    render_geom_with_fonts(
        &xlsx_body_html(zip),
        PageGeom::tabular_default(),
        &merge_fonts(extract_embedded_fonts(zip), host),
    )
}

/// Build the XLSX HTML `<body>` (one `<table>` per sheet) without rendering.
/// Shared by [`xlsx_to_pdf`] and the font-need scan.
fn xlsx_body_html(zip: &BTreeMap<String, Vec<u8>>) -> String {
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
    body
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
    // (col_index, escaped html, optional `#RRGGBB` background, non-fill CSS).
    let mut row_cells: Vec<(usize, String, Option<String>, String)> = Vec::new();
    let mut row_open = false;
    // 0-based index of the current row: from the row's `r` attribute when
    // present, else a running counter incremented per `<row>`.
    let mut row_idx = 0usize;
    let mut next_auto_row = 0usize;
    // The current row's `<tr>` style fragment (custom height), reset per row.
    let mut row_style = String::new();

    // Current-cell scratch.
    let mut cell_col = 0usize;
    let mut cell_type = String::new();
    let mut cell_text = String::new();
    let mut cell_bg: Option<String> = None;
    // Non-fill CSS (font/border/alignment) resolved from `c@s`.
    let mut cell_css = String::new();
    // numFmt code resolved from `c@s`, applied to numeric cells at close.
    let mut cell_fmt: Option<String> = None;
    let mut in_cell = false;
    let mut in_value = false; // inside <v> or <t>

    let flush_row = |row: usize,
                     row_style: &str,
                     row_cells: &mut Vec<(usize, String, Option<String>, String)>,
                     out: &mut String| {
        if row_cells.is_empty() {
            out.push_str(&format!("<tr{}></tr>", style_attr(row_style)));
            return;
        }
        out.push_str(&format!("<tr{}>", style_attr(row_style)));
        let max_col = row_cells.iter().map(|(c, _, _, _)| *c).max().unwrap_or(0);
        let mut by_col: BTreeMap<usize, (String, Option<String>, String)> = BTreeMap::new();
        for (c, h, bg, css) in row_cells.drain(..) {
            by_col.insert(c, (h, bg, css));
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
                Some((h, bg, css)) => {
                    let style = td_style_attr(bg.as_deref(), css);
                    out.push_str(&format!("<td{span}{style}>{h}</td>"));
                }
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
                    // `@ht` is a custom row height in points (only when `customHeight`).
                    row_style = match attr(&attrs, "ht")
                        .filter(|_| attr(&attrs, "customHeight").is_some())
                        .and_then(|v| v.trim().parse::<f64>().ok())
                        .filter(|h| *h > 0.0)
                    {
                        Some(h) => format!("height:{}pt", fmt_pt(h)),
                        None => String::new(),
                    };
                }
                "c" if in_sheet_data => {
                    in_cell = true;
                    cell_text.clear();
                    cell_type = attr(&attrs, "t").unwrap_or("n").to_string();
                    cell_col = attr(&attrs, "r").map(col_of_ref).unwrap_or(0);
                    // `c@s` is the cellXfs index → solid-fill colour + numFmt + CSS.
                    let style_idx = attr(&attrs, "s").and_then(|v| v.trim().parse::<usize>().ok());
                    cell_bg = style_idx.and_then(|i| styles.fill(i));
                    cell_fmt = style_idx
                        .and_then(|i| styles.num_fmt(i))
                        .map(|(_, code)| code.clone());
                    cell_css = style_idx
                        .map(|i| styles.css(i).to_string())
                        .unwrap_or_default();
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
                        row_cells.push((
                            cell_col,
                            escaped(resolved.trim()),
                            cell_bg.take(),
                            std::mem::take(&mut cell_css),
                        ));
                        cell_fmt = None;
                    }
                    in_cell = false;
                }
                "row" => {
                    if row_open {
                        flush_row(row_idx, &row_style, &mut row_cells, &mut out);
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

/// ` style="…"` attribute for a table cell, combining the solid-fill
/// `background-color` (if any) with the non-fill CSS (font/border/alignment).
/// Empty string when neither contributes anything.
fn td_style_attr(bg: Option<&str>, css: &str) -> String {
    let mut style = String::new();
    if let Some(bg) = bg {
        style.push_str(&format!("background-color:{bg};"));
    }
    style.push_str(css);
    style_attr(&style)
}

/// ` style="…"` attribute wrapping a CSS fragment (a trailing `;` is trimmed),
/// or `""` when the fragment is empty. Shared by `<tr>`/`<td>` emitters.
fn style_attr(css: &str) -> String {
    let css = css.trim_end_matches(';');
    if css.is_empty() {
        String::new()
    } else {
        format!(" style=\"{css}\"")
    }
}

/// Resolved per-cell-style XLSX formatting: for each `cellXfs` index (a cell's
/// `@s`), the solid-fill background colour (if any), the number-format id + its
/// format code (so numeric cells can be formatted: dates, currency, …), and the
/// combined non-fill CSS (font weight/style/underline/size/colour/family from
/// the referenced `<font>`, a collapsed `border` from the referenced
/// `<border>`, and `text-align`/`vertical-align` from the `xf`'s `<alignment>`).
#[derive(Default)]
struct XlsxStyles {
    /// cellXfs index → `Some("#RRGGBB")` solid fill, else `None`.
    fills: Vec<Option<String>>,
    /// cellXfs index → `(numFmtId, format-code)`. The code is resolved from the
    /// built-in table or the custom `<numFmts>` map; `None` when general/absent.
    num_fmts: Vec<Option<(u32, String)>>,
    /// cellXfs index → combined CSS declarations (font + border + alignment),
    /// already terminated with `;`. Empty string when the style adds nothing.
    /// This is the HTML/render path; the editable-model path uses [`fmts`].
    css: Vec<String>,
    /// cellXfs index → structured per-cell formatting (font/border/alignment),
    /// for the typed editable-model path ([`xlsx_sheet_model`]).
    fmts: Vec<CellFmt>,
}

/// Structured (non-fill, non-numFmt) formatting resolved for one `cellXfs`
/// index, mirroring the CSS produced for the HTML path but as typed model
/// values: the referenced font as a [`CharStyle`] delta, the collapsed cell
/// [`model::BorderStyle`], and the `xf`'s `<alignment>` (`horizontal` → align,
/// `wrapText` → [`wrap`](CellFmt::wrap)).
#[derive(Default, Clone)]
struct CellFmt {
    style: CharStyle,
    border: Option<model::BorderStyle>,
    align: Option<MAlign>,
    wrap: bool,
}

impl XlsxStyles {
    fn fill(&self, idx: usize) -> Option<String> {
        self.fills.get(idx).and_then(|c| c.clone())
    }
    fn num_fmt(&self, idx: usize) -> Option<&(u32, String)> {
        self.num_fmts.get(idx).and_then(|f| f.as_ref())
    }
    /// Non-fill CSS for a cellXfs index (`""` when none / out of range).
    fn css(&self, idx: usize) -> &str {
        self.css.get(idx).map(String::as_str).unwrap_or("")
    }
    /// Structured font/border/alignment for a cellXfs index (`None` out of range).
    fn fmt(&self, idx: usize) -> Option<&CellFmt> {
        self.fmts.get(idx)
    }
}

/// Parse `xl/styles.xml` (with `theme` for theme-colour resolution) into an
/// [`XlsxStyles`]: the `cellXfs` order maps each style index to its solid-fill
/// colour (`@fillId → fills[…] → patternFill@fgColor`, resolving `rgb`,
/// `theme`+`tint` and `indexed`), its number format (`@numFmtId`, resolved
/// against the built-in ids and the custom `<numFmts>` map), and its combined
/// non-fill CSS — the referenced `<font>` (`@fontId → fonts[…]`: bold/italic/
/// underline/size/colour/family), the referenced `<border>` (`@borderId →
/// borders[…]`: a collapsed `border` shorthand), and the `xf`'s own
/// `<alignment>` (`text-align`/`vertical-align`).
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

    // Pass 1b: fontId → CSS, borderId → CSS (ordered lists of `<font>`/`<border>`),
    // plus their typed counterparts for the editable-model path.
    let font_css = parse_xlsx_fonts(xml, theme);
    let border_css = parse_xlsx_borders(xml, theme);
    let font_struct = parse_xlsx_fonts_struct(xml, theme);
    let border_struct = parse_xlsx_borders_struct(xml, theme);

    // Pass 2: cellXfs order → (fillId → colour, numFmtId → format code, combined
    // non-fill CSS from fontId + borderId + the xf's own `<alignment>`, and the
    // same resolved structurally as a `CellFmt`).
    let mut fills: Vec<Option<String>> = Vec::new();
    let mut num_fmts: Vec<Option<(u32, String)>> = Vec::new();
    let mut css: Vec<String> = Vec::new();
    let mut fmts: Vec<CellFmt> = Vec::new();
    {
        let mut x = Xml::new(xml);
        let mut in_cellxfs = false;
        // The xf currently being assembled (open, not yet closed): its combined
        // CSS and `CellFmt` so a child `<alignment>` can append before we push.
        let mut cur_css: Option<String> = None;
        let mut cur_fmt: Option<CellFmt> = None;
        while let Some(tok) = x.next() {
            match tok {
                Tok::Open(name, attrs, sc) => match local(&name) {
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
                        // Combine the referenced font + border CSS now; a nested
                        // `<alignment>` (if present) appends before the xf closes.
                        let mut c = String::new();
                        if let Some(f) = attr(&attrs, "fontId")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .and_then(|i| font_css.get(i))
                            .and_then(|f| f.as_deref())
                        {
                            c.push_str(f);
                        }
                        if let Some(b) = attr(&attrs, "borderId")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .and_then(|i| border_css.get(i))
                            .and_then(|b| b.as_deref())
                        {
                            c.push_str(b);
                        }
                        // The typed counterpart: clone the referenced font /
                        // border (the `<alignment>` child fills in align/wrap).
                        let mut cf = CellFmt {
                            style: attr(&attrs, "fontId")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                                .and_then(|i| font_struct.get(i))
                                .cloned()
                                .unwrap_or_default(),
                            ..CellFmt::default()
                        };
                        cf.border = attr(&attrs, "borderId")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .and_then(|i| border_struct.get(i))
                            .and_then(|b| *b);
                        if sc {
                            // Self-closing xf: no `<alignment>` child possible.
                            css.push(c);
                            fmts.push(cf);
                        } else {
                            cur_css = Some(c);
                            cur_fmt = Some(cf);
                        }
                    }
                    "alignment" if in_cellxfs => {
                        if let Some(c) = cur_css.as_mut() {
                            c.push_str(&xlsx_alignment_css(&attrs));
                        }
                        if let Some(cf) = cur_fmt.as_mut() {
                            let (align, wrap) = xlsx_alignment_struct(&attrs);
                            cf.align = align;
                            cf.wrap = wrap;
                        }
                    }
                    _ => {}
                },
                Tok::Close(name) => match local(&name) {
                    "xf" if in_cellxfs => {
                        if let Some(c) = cur_css.take() {
                            css.push(c);
                        }
                        if let Some(cf) = cur_fmt.take() {
                            fmts.push(cf);
                        }
                    }
                    "cellXfs" => in_cellxfs = false,
                    _ => {}
                },
                Tok::Text(_) => {}
            }
        }
    }
    XlsxStyles {
        fills,
        num_fmts,
        css,
        fmts,
    }
}

/// Map `xl/styles.xml`'s `<fonts>` list to per-`fontId` CSS: `<b>`→`font-weight:
/// bold`, `<i>`→`font-style:italic`, `<u>`→`text-decoration:underline`, `<sz
/// val>`→`font-size`, `<color>`→`color` (rgb/theme+tint/indexed via
/// [`xlsx_color`]), `<name val>`→`font-family`. `None` when a font adds nothing.
fn parse_xlsx_fonts(xml: &str, theme: &XlsxTheme) -> Vec<Option<String>> {
    let mut out: Vec<Option<String>> = Vec::new();
    let mut x = Xml::new(xml);
    let mut in_fonts = false;
    let mut cur = String::new();
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => match local(&name) {
                "fonts" => in_fonts = true,
                // A self-closing `<font/>` carries no children → empty entry; this
                // keeps the index aligned with `fontId` (self-closing emits no Close).
                "font" if in_fonts && sc => out.push(None),
                _ if in_fonts => match local(&name) {
                    // `<b/>` with no/`val="true"` ⇒ bold; `val="0"/"false"` cancels.
                    "b" if xlsx_bool_attr(&attrs) => cur.push_str("font-weight:bold;"),
                    "i" if xlsx_bool_attr(&attrs) => cur.push_str("font-style:italic;"),
                    "u" if attr(&attrs, "val") != Some("none") => {
                        cur.push_str("text-decoration:underline;")
                    }
                    "sz" => {
                        if let Some(pt) =
                            attr(&attrs, "val").and_then(|v| v.trim().parse::<f64>().ok())
                        {
                            cur.push_str(&format!("font-size:{}pt;", fmt_pt(pt)));
                        }
                    }
                    "color" => {
                        if let Some(c) = xlsx_color(&attrs, theme) {
                            cur.push_str(&format!("color:{c};"));
                        }
                    }
                    "name" | "rFont" => {
                        if let Some(fam) = attr(&attrs, "val") {
                            let family = css_font_family(fam);
                            if !family.is_empty() {
                                cur.push_str(&format!("font-family:{family};"));
                            }
                        }
                    }
                    _ => {}
                },
                _ => {}
            },
            Tok::Close(name) => match local(&name) {
                "font" if in_fonts => {
                    out.push(if cur.is_empty() {
                        None
                    } else {
                        Some(std::mem::take(&mut cur))
                    });
                }
                "fonts" => in_fonts = false,
                _ => {}
            },
            Tok::Text(_) => {}
        }
    }
    out
}

/// Map `xl/styles.xml`'s `<borders>` list to per-`borderId` CSS. XLSX borders
/// are per-edge, but the HTML engine only honours a single uniform `border`
/// shorthand, so we collapse: emit `border:<w>px solid <colour>` when ANY edge
/// has a real (non-`none`) style, using the heaviest edge's width and the first
/// edge colour found. `None` when no edge is styled.
fn parse_xlsx_borders(xml: &str, theme: &XlsxTheme) -> Vec<Option<String>> {
    let mut out: Vec<Option<String>> = Vec::new();
    let mut x = Xml::new(xml);
    let mut in_borders = false;
    // Per-`<border>` accumulator: heaviest edge width (px) and first edge colour.
    let mut width = 0.0f64;
    let mut color: Option<String> = None;
    // The edge element we're inside, so a nested `<color>` attaches to it.
    let mut in_edge = false;
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => match local(&name) {
                "borders" => in_borders = true,
                // A self-closing `<border/>` has no edges → no border; keep the
                // index aligned with `borderId` (self-closing emits no Close).
                "border" if in_borders && sc => out.push(None),
                "left" | "right" | "top" | "bottom" | "diagonal" if in_borders => {
                    in_edge = true;
                    if let Some(w) = border_style_width(attr(&attrs, "style")) {
                        width = width.max(w);
                    }
                }
                "color" if in_borders && in_edge => {
                    if color.is_none() {
                        color = xlsx_color(&attrs, theme);
                    }
                }
                _ => {}
            },
            Tok::Close(name) => match local(&name) {
                "left" | "right" | "top" | "bottom" | "diagonal" if in_borders => in_edge = false,
                "border" if in_borders => {
                    out.push(if width > 0.0 {
                        let c = color.take().unwrap_or_else(|| "#000000".to_string());
                        Some(format!("border:{}px solid {c};", fmt_pt(width)))
                    } else {
                        color = None;
                        None
                    });
                    width = 0.0;
                    in_edge = false;
                }
                "borders" => in_borders = false,
                _ => {}
            },
            Tok::Text(_) => {}
        }
    }
    out
}

/// Map `xl/styles.xml`'s `<fonts>` list to per-`fontId` [`CharStyle`] deltas, the
/// typed counterpart of [`parse_xlsx_fonts`] for the editable-model path: `<b>`→
/// `bold`, `<i>`→`italic`, `<u>`→`underline`, `<sz val>`→`size_pt`, `<color>`→
/// `color` (rgb/theme+tint/indexed via [`xlsx_color`]), `<name val>`→`family`.
/// One entry per font, index-aligned with `fontId` (self-closing `<font/>` ⇒ a
/// default style).
fn parse_xlsx_fonts_struct(xml: &str, theme: &XlsxTheme) -> Vec<CharStyle> {
    let mut out: Vec<CharStyle> = Vec::new();
    let mut x = Xml::new(xml);
    let mut in_fonts = false;
    let mut cur = CharStyle::default();
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => match local(&name) {
                "fonts" => in_fonts = true,
                "font" if in_fonts && sc => out.push(CharStyle::default()),
                _ if in_fonts => match local(&name) {
                    "b" if xlsx_bool_attr(&attrs) => cur.bold = true,
                    "i" if xlsx_bool_attr(&attrs) => cur.italic = true,
                    "u" if attr(&attrs, "val") != Some("none") => cur.underline = true,
                    "sz" => {
                        if let Some(pt) =
                            attr(&attrs, "val").and_then(|v| v.trim().parse::<f64>().ok())
                        {
                            cur.size_pt = pt;
                        }
                    }
                    "color" => {
                        if let Some(rgb) = xlsx_color(&attrs, theme)
                            .as_deref()
                            .and_then(hex_to_rgb_f64)
                        {
                            cur.color = Some(rgb);
                        }
                    }
                    "name" | "rFont" => {
                        if let Some(fam) = attr(&attrs, "val") {
                            cur.family = fam.to_string();
                        }
                    }
                    _ => {}
                },
                _ => {}
            },
            Tok::Close(name) => match local(&name) {
                "font" if in_fonts => out.push(std::mem::take(&mut cur)),
                "fonts" => in_fonts = false,
                _ => {}
            },
            Tok::Text(_) => {}
        }
    }
    out
}

/// Map `xl/styles.xml`'s `<borders>` list to per-`borderId` [`model::BorderStyle`],
/// the typed counterpart of [`parse_xlsx_borders`]: the heaviest non-`none` edge
/// sets the width (px), the first edge colour the colour (defaulting to black).
/// `None` when no edge is styled. Index-aligned with `borderId`.
fn parse_xlsx_borders_struct(xml: &str, theme: &XlsxTheme) -> Vec<Option<model::BorderStyle>> {
    let mut out: Vec<Option<model::BorderStyle>> = Vec::new();
    let mut x = Xml::new(xml);
    let mut in_borders = false;
    let mut width = 0.0f64;
    let mut color: Option<[f64; 3]> = None;
    let mut in_edge = false;
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => match local(&name) {
                "borders" => in_borders = true,
                "border" if in_borders && sc => out.push(None),
                "left" | "right" | "top" | "bottom" | "diagonal" if in_borders => {
                    in_edge = true;
                    if let Some(w) = border_style_width(attr(&attrs, "style")) {
                        width = width.max(w);
                    }
                }
                "color" if in_borders && in_edge => {
                    if color.is_none() {
                        color = xlsx_color(&attrs, theme)
                            .as_deref()
                            .and_then(hex_to_rgb_f64);
                    }
                }
                _ => {}
            },
            Tok::Close(name) => match local(&name) {
                "left" | "right" | "top" | "bottom" | "diagonal" if in_borders => in_edge = false,
                "border" if in_borders => {
                    out.push((width > 0.0).then(|| model::BorderStyle {
                        width,
                        color: color.unwrap_or([0.0, 0.0, 0.0]),
                    }));
                    width = 0.0;
                    color = None;
                    in_edge = false;
                }
                "borders" => in_borders = false,
                _ => {}
            },
            Tok::Text(_) => {}
        }
    }
    out
}

/// Resolve an `xf`'s `<alignment>` to `(Option<Align>, wrap)` for the typed
/// model path: `@horizontal` → [`MAlign`] (`center`/`centerContinuous`→Center,
/// `right`→Right, `justify`/`distributed`/`fill`→Justify, `left`→Left; unknown
/// ⇒ `None`), and `@wrapText` → bool.
fn xlsx_alignment_struct(attrs: &[(String, String)]) -> (Option<MAlign>, bool) {
    let align = match attr(attrs, "horizontal") {
        Some("left") => Some(MAlign::Left),
        Some("center") | Some("centerContinuous") => Some(MAlign::Center),
        Some("right") => Some(MAlign::Right),
        Some("justify") | Some("distributed") | Some("fill") => Some(MAlign::Justify),
        _ => None,
    };
    let wrap = matches!(attr(attrs, "wrapText"), Some("1") | Some("true"));
    (align, wrap)
}

/// Width in CSS px for an XLSX border line `style` (`thin`/`medium`/`thick`/
/// `hair`/`dashed`/…). `None`/`"none"` ⇒ no edge. Thin styles map to 1px,
/// medium to 2px, thick to 3px — enough to make the grid visible.
fn border_style_width(style: Option<&str>) -> Option<f64> {
    match style? {
        "none" | "" => None,
        "thick" | "double" => Some(3.0),
        "medium" | "mediumDashed" | "mediumDashDot" | "mediumDashDotDot" => Some(2.0),
        // thin, hair, dotted, dashed, dashDot, dashDotDot, slantDashDot, …
        _ => Some(1.0),
    }
}

/// CSS for an `xf`'s `<alignment>`: `@horizontal` → `text-align`, `@vertical` →
/// `vertical-align`. Excel's `center`/`centerContinuous` map to `center`;
/// `justify`/`distributed` to `justify`. Unknown values are skipped.
fn xlsx_alignment_css(attrs: &[(String, String)]) -> String {
    let mut css = String::new();
    if let Some(h) = attr(attrs, "horizontal") {
        let v = match h {
            "left" => "left",
            "center" | "centerContinuous" => "center",
            "right" => "right",
            "justify" | "distributed" | "fill" => "justify",
            _ => "",
        };
        if !v.is_empty() {
            css.push_str(&format!("text-align:{v};"));
        }
    }
    if let Some(va) = attr(attrs, "vertical") {
        let v = match va {
            "top" => "top",
            "center" => "middle",
            "bottom" => "bottom",
            _ => "",
        };
        if !v.is_empty() {
            css.push_str(&format!("vertical-align:{v};"));
        }
    }
    css
}

/// An XLSX boolean toggle (`<b/>`, `<i/>`): present means on unless an explicit
/// `val` says otherwise (`0`/`false`/`off` ⇒ off).
fn xlsx_bool_attr(attrs: &[(String, String)]) -> bool {
    !matches!(attr(attrs, "val"), Some("0") | Some("false") | Some("off"))
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
    pptx_to_pdf_with(zip, &[])
}

/// Like [`pptx_to_pdf`] but also feeds `host` faces (phase 2 of
/// [`office_needed_fonts`]); embedded deck fonts still win on conflict.
fn pptx_to_pdf_with(zip: &BTreeMap<String, Vec<u8>>, host: &[ProvidedFont]) -> Vec<u8> {
    let (body, geom) = pptx_body_geom(zip);
    render_geom_with_fonts(&body, geom, &merge_fonts(extract_embedded_fonts(zip), host))
}

/// Build the PPTX HTML `<body>` (one slide per page) and resolve slide geometry,
/// without rendering. Shared by [`pptx_to_pdf`] and the font-need scan.
fn pptx_body_geom(zip: &BTreeMap<String, Vec<u8>>) -> (String, PageGeom) {
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
    (body, geom)
}

/// The PPTX HTML `<body>` only — used by the font-need scan.
fn pptx_body_html(zip: &BTreeMap<String, Vec<u8>>) -> String {
    pptx_body_geom(zip).0
}

/// The deck's resolved typefaces for the OOXML theme-font placeholders that text
/// runs reference with `a:latin typeface="+mn-lt"` (minor / body) and `"+mj-lt"`
/// (major / heading), plus the theme colour scheme (`a:clrScheme`) that runs and
/// fills reference with `a:schemeClr@val`. Read from the deck's first theme part.
#[derive(Default, Clone)]
struct PptxTheme {
    minor_latin: Option<String>,
    major_latin: Option<String>,
    /// Theme colour scheme slot name (`dk1`/`lt1`/`accent1`…/`hlink`/`folHlink`)
    /// → `RRGGBB` (uppercase, no `#`). Empty when no theme part was present.
    scheme: BTreeMap<String, String>,
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

    /// Resolve an `a:schemeClr@val` slot to a concrete `RRGGBB` colour.
    ///
    /// The presentation-level `p:clrMap` aliases (`bg1`/`tx1`/`bg2`/`tx2`) are
    /// folded onto the canonical scheme slots it maps them to (default mapping:
    /// `bg1→lt1`, `tx1→dk1`, `bg2→lt2`, `tx2→dk2`); `phClr` is a placeholder
    /// resolved by the caller's context, so it has no fixed colour here.
    fn resolve_scheme(&self, val: &str) -> Option<String> {
        let slot = match val {
            "bg1" => "lt1",
            "tx1" => "dk1",
            "bg2" => "lt2",
            "tx2" => "dk2",
            other => other,
        };
        self.scheme.get(slot).cloned()
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

/// Parse a theme XML's `a:fontScheme` (the first `a:latin@typeface` inside
/// `a:majorFont` → major family, inside `a:minorFont` → minor family) and its
/// `a:clrScheme` (each named slot's `a:srgbClr@val` / `a:sysClr@lastClr` → its
/// `RRGGBB` colour, keyed by slot name).
fn parse_pptx_theme(xml: &str) -> PptxTheme {
    const COLOR_SLOTS: [&str; 12] = [
        "dk1", "lt1", "dk2", "lt2", "accent1", "accent2", "accent3", "accent4", "accent5",
        "accent6", "hlink", "folHlink",
    ];

    let mut theme = PptxTheme::default();
    let mut x = Xml::new(xml);
    let mut in_major = false;
    let mut in_minor = false;
    let mut in_clr_scheme = false;
    // The colour-scheme slot we're currently inside (its `a:srgbClr`/`a:sysClr`
    // child carries the value).
    let mut cur_slot: Option<&'static str> = None;
    let slot_name =
        |ln: &str| -> Option<&'static str> { COLOR_SLOTS.iter().copied().find(|s| *s == ln) };

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
                "clrScheme" => in_clr_scheme = true,
                ln if in_clr_scheme && slot_name(ln).is_some() => {
                    cur_slot = slot_name(ln);
                }
                "srgbClr" if in_clr_scheme => {
                    if let (Some(slot), Some(v)) = (cur_slot, attr(&attrs, "val")) {
                        if is_hex6(v) {
                            theme
                                .scheme
                                .entry(slot.to_string())
                                .or_insert_with(|| v.to_ascii_uppercase());
                        }
                    }
                }
                "sysClr" if in_clr_scheme => {
                    // System colours carry a resolved `lastClr` (e.g. window text).
                    if let (Some(slot), Some(v)) = (cur_slot, attr(&attrs, "lastClr")) {
                        if is_hex6(v) {
                            theme
                                .scheme
                                .entry(slot.to_string())
                                .or_insert_with(|| v.to_ascii_uppercase());
                        }
                    }
                }
                _ => {}
            },
            Tok::Close(name) => match local(&name) {
                "majorFont" => in_major = false,
                "minorFont" => in_minor = false,
                "clrScheme" => {
                    in_clr_scheme = false;
                    cur_slot = None;
                }
                ln if cur_slot == slot_name(ln) => cur_slot = None,
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

/// A shape's absolute placement from its `a:xfrm`: offset (`a:off@x,@y`), extent
/// (`a:ext@cx,@cy`), all in points, plus the `a:xfrm@rot` (60000ths of a degree)
/// and the `@flipH`/`@flipV` booleans. A shape WITH an explicit `a:xfrm` is laid
/// out at absolute coordinates; without one (layout/master inheritance) it falls
/// back to document-order flow.
#[derive(Default, Clone, Copy)]
struct XfrmBox {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    /// Clockwise rotation in degrees (OOXML stores 60000ths of a degree).
    rot_deg: f64,
    flip_h: bool,
    flip_v: bool,
}

impl XfrmBox {
    /// True once an `a:off` + `a:ext` defined a usable (non-degenerate) box.
    fn is_placed(&self) -> bool {
        self.w > 0.0 && self.h > 0.0
    }

    /// The inline CSS for an absolutely-positioned wrapper `<div>` at this box.
    /// A non-zero rotation and the flips combine into one `transform` (rotation
    /// about the box centre, matching PowerPoint). The HTML engine ignores
    /// `transform` today, so this is a forward-compatible hint that never
    /// regresses the absolute left/top/width/height placement it does honour.
    fn abs_style(&self) -> String {
        let mut s = format!(
            "position:absolute;left:{}pt;top:{}pt;width:{}pt;height:{}pt",
            fmt_pt(self.x),
            fmt_pt(self.y),
            fmt_pt(self.w),
            fmt_pt(self.h),
        );
        let mut tf = String::new();
        if self.rot_deg != 0.0 {
            tf.push_str(&format!("rotate({}deg)", fmt_pt(self.rot_deg)));
        }
        if self.flip_h {
            tf.push_str("scaleX(-1)");
        }
        if self.flip_v {
            tf.push_str("scaleY(-1)");
        }
        if !tf.is_empty() {
            s.push_str(";transform:");
            s.push_str(&tf);
            s.push_str(";transform-origin:center");
        }
        s
    }
}

/// Read a shape's transform from an `a:xfrm` open tag (its attrs carry `@rot`/
/// `@flipH`/`@flipV`) and the immediately-following `a:off`/`a:ext` children,
/// consuming the subtree up to `</a:xfrm>`. Other children (e.g. `a:chOff`) are
/// skipped; only the FIRST `a:off`/`a:ext` are taken (the shape's own).
fn parse_xfrm(x: &mut Xml, xfrm_attrs: &[(String, String)]) -> XfrmBox {
    let mut b = XfrmBox::default();
    if let Some(r) = attr(xfrm_attrs, "rot").and_then(|v| v.trim().parse::<f64>().ok()) {
        b.rot_deg = r / 60000.0;
    }
    b.flip_h = matches!(attr(xfrm_attrs, "flipH"), Some("1") | Some("true"));
    b.flip_v = matches!(attr(xfrm_attrs, "flipV"), Some("1") | Some("true"));
    let mut have_off = false;
    let mut have_ext = false;
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, _) => match local(&name) {
                "off" if !have_off => {
                    if let Some(v) = attr(&attrs, "x").and_then(emu_to_pt) {
                        b.x = v;
                    }
                    if let Some(v) = attr(&attrs, "y").and_then(emu_to_pt) {
                        b.y = v;
                    }
                    have_off = true;
                }
                "ext" if !have_ext => {
                    if let Some(v) = attr(&attrs, "cx").and_then(emu_to_pt) {
                        b.w = v;
                    }
                    if let Some(v) = attr(&attrs, "cy").and_then(emu_to_pt) {
                        b.h = v;
                    }
                    have_ext = true;
                }
                _ => {}
            },
            Tok::Close(name) if local(&name) == "xfrm" => break,
            _ => {}
        }
    }
    b
}

/// Emit one slide into `out`. Each shape tree (`p:sp` / `p:pic` /
/// `p:graphicFrame`) carrying an explicit `a:xfrm` is wrapped in an
/// absolutely-positioned `<div>` at its slide coordinates, so a deck's layout is
/// preserved instead of stacking every box in document order; shapes WITHOUT an
/// `a:xfrm` (layout/master inheritance) fall back to flow. The slide background
/// (`p:cSld/p:bg` solid fill) becomes a full-slide backdrop. Theme fonts
/// (`+mn-lt`/`+mj-lt`) and colours (`a:schemeClr`) resolve through `theme`;
/// `a:tbl` renders as a real HTML `<table>`.
fn pptx_slide(
    xml: &str,
    zip: &BTreeMap<String, Vec<u8>>,
    rels: &BTreeMap<String, String>,
    theme: &PptxTheme,
    out: &mut String,
) {
    // Build this slide into a local buffer so the "no shapes → flow fallback"
    // decision sees only THIS slide's output, not the deck accumulated in `out`.
    let mut slide = String::new();
    let mut x = Xml::new(xml);
    while let Some(tok) = x.next() {
        if let Tok::Open(name, _attrs, sc) = tok {
            match local(&name) {
                // Slide background fill (solid colour) → full-slide backdrop.
                "bg" if !sc => {
                    if let Some(div) = pptx_bg(&mut x, theme) {
                        slide.push_str(&div);
                    }
                }
                // A shape / picture / graphic-frame subtree: position it from its
                // own `a:xfrm`, falling back to flow when it has none.
                "sp" | "pic" | "graphicFrame" if !sc => {
                    pptx_shape(&mut x, zip, rels, theme, local(&name), &mut slide);
                }
                _ => {}
            }
        }
    }
    // No recognised shapes (e.g. a minimal/synthetic slide): parse the whole body
    // as flowing content so plain `a:p`/`a:tbl`/`a:blip` still render.
    if slide.is_empty() {
        let mut x2 = Xml::new(xml);
        pptx_content(&mut x2, zip, rels, theme, None, &mut slide);
    }
    out.push_str(&slide);
}

/// Render the slide background `p:bg` (open consumed): a `p:bgPr/a:solidFill`
/// with an `a:srgbClr`/`a:schemeClr` becomes a full-slide absolutely-positioned
/// backdrop `<div>`. Width is `100%` of the page box (so any slide aspect ratio
/// is covered); height is the standard slide height (`SLIDE_H`, 7.5in = 540pt —
/// PowerPoint's height for both 4:3 and 16:9). The engine clips any overflow to
/// the page. Returns `None` for picture/gradient/inherited fills (out of scope
/// here). Consumes up to `</p:bg>`.
fn pptx_bg(x: &mut Xml, theme: &PptxTheme) -> Option<String> {
    let mut color: Option<String> = None;
    let mut in_solid = false;
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, _) => match local(&name) {
                "solidFill" => in_solid = true,
                "srgbClr" if in_solid && color.is_none() => {
                    if let Some(v) = attr(&attrs, "val").filter(|v| is_hex6(v)) {
                        color = Some(v.to_ascii_uppercase());
                    }
                }
                "schemeClr" if in_solid && color.is_none() => {
                    color = attr(&attrs, "val").and_then(|v| theme.resolve_scheme(v));
                }
                _ => {}
            },
            Tok::Close(name) => match local(&name) {
                "solidFill" => in_solid = false,
                "bg" => break,
                _ => {}
            },
            Tok::Text(_) => {}
        }
    }
    color.map(|c| {
        format!(
            "<div style=\"position:absolute;left:0pt;top:0pt;width:100%;min-height:{}pt;background:#{c}\"></div>",
            fmt_pt(SLIDE_H),
        )
    })
}

/// Render one shape subtree (`p:sp` / `p:pic` / `p:graphicFrame`, open consumed;
/// `tag` is its local name). The first `a:xfrm` (the shape's own, in `p:spPr` /
/// `p:grpSpPr` or the graphic-frame `p:xfrm`) gives the absolute box; the body
/// (text/table/image) is rendered via [`pptx_content`]. With a usable box the
/// body is wrapped in an absolutely-positioned `<div>`; otherwise it flows.
/// Consumes the subtree up to the matching `</tag>`.
fn pptx_shape(
    x: &mut Xml,
    zip: &BTreeMap<String, Vec<u8>>,
    rels: &BTreeMap<String, String>,
    theme: &PptxTheme,
    tag: &str,
    out: &mut String,
) {
    let mut xfrm = XfrmBox::default();
    let mut have_xfrm = false;
    let mut body = String::new();

    while let Some(tok) = x.next() {
        match &tok {
            Tok::Open(name, attrs, sc) => match local(name) {
                // The shape transform: take the first one (its own), then keep
                // scanning the rest of the shape for its body content.
                "xfrm" if !sc && !have_xfrm => {
                    xfrm = parse_xfrm(x, attrs);
                    have_xfrm = true;
                }
                // Body grammar — delegate each content subtree to pptx_content,
                // which stops at the element's own close tag.
                "p" if !sc => {
                    pptx_content_paragraph(x, theme, &mut body);
                }
                "tbl" if !sc => {
                    pptx_table(x, theme, &mut body);
                }
                "blip" => {
                    if let Some(rid) = attr(attrs, "embed").or_else(|| attr(attrs, "link")) {
                        if let Some(t) = rels
                            .get(rid)
                            .map(|t| resolve_target("ppt", t))
                            .and_then(|k| img_tag(zip, &k))
                        {
                            body.push_str(&t);
                        }
                    }
                }
                _ => {}
            },
            Tok::Close(name) if local(name) == tag => break,
            _ => {}
        }
    }

    if body.trim().is_empty() {
        return;
    }
    if have_xfrm && xfrm.is_placed() {
        out.push_str(&format!(
            "<div style=\"{}\">{}</div>",
            xfrm.abs_style(),
            body
        ));
    } else {
        out.push_str(&body);
    }
}

/// Parse a flowing run of slide content (paragraphs, tables, images) until
/// `stop_tag`'s close (or EOF when `None`), emitting into `out`. Shared by the
/// orphan-content fallback. Paragraph runs honour `a:srgbClr`/`a:schemeClr`
/// colour and `a:latin` typeface (resolved through `theme`).
fn pptx_content(
    x: &mut Xml,
    zip: &BTreeMap<String, Vec<u8>>,
    rels: &BTreeMap<String, String>,
    theme: &PptxTheme,
    stop_tag: Option<&str>,
    out: &mut String,
) {
    while let Some(tok) = x.next() {
        match &tok {
            Tok::Open(name, attrs, sc) => match local(name) {
                "tbl" if !sc => pptx_table(x, theme, out),
                "p" if !sc => pptx_content_paragraph(x, theme, out),
                "blip" => {
                    if let Some(rid) = attr(attrs, "embed").or_else(|| attr(attrs, "link")) {
                        if let Some(t) = rels
                            .get(rid)
                            .map(|t| resolve_target("ppt", t))
                            .and_then(|k| img_tag(zip, &k))
                        {
                            out.push_str(&t);
                        }
                    }
                }
                _ => {}
            },
            Tok::Close(name) if stop_tag == Some(local(name)) => break,
            _ => {}
        }
    }
}

/// Emit one PPTX `a:p` paragraph (open consumed) as `<p>…</p>` into `out`,
/// applying each run's `a:rPr` (`b`/`i`/`sz`), `a:srgbClr`/`a:schemeClr` colour
/// and `a:latin` typeface. Consumes up to `</a:p>`.
fn pptx_content_paragraph(x: &mut Xml, theme: &PptxTheme, out: &mut String) {
    let mut para = String::new();
    let mut run = RunStyle::default();
    let mut in_rpr = false;
    let mut in_text = false;

    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => match local(&name) {
                "rPr" if !sc => {
                    in_rpr = true;
                    run = pptx_run_props(&attrs);
                }
                "rPr" if sc => run = pptx_run_props(&attrs),
                "srgbClr" if in_rpr => {
                    if let Some(v) = attr(&attrs, "val") {
                        if is_hex6(v) {
                            run.color = Some(v.to_ascii_uppercase());
                        }
                    }
                }
                "schemeClr" if in_rpr => {
                    if let Some(c) = attr(&attrs, "val").and_then(|v| theme.resolve_scheme(v)) {
                        run.color = Some(c);
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
                if in_text && !t.is_empty() {
                    push_run_text(&run, &t, &mut para);
                }
            }
            Tok::Close(name) => match local(&name) {
                "t" => in_text = false,
                "rPr" => in_rpr = false,
                "p" => break,
                _ => {}
            },
        }
    }
    if !para.trim().is_empty() {
        out.push_str(&format!("<p>{}</p>", para.trim()));
    }
}

/// Streaming state for one PPTX `a:rPr` run-properties subtree: the run's fill
/// colour (the `a:solidFill`, or — as a graceful fallback — the FIRST stop of an
/// `a:gradFill`), its `a:latin` typeface, and its `a:hlinkClick` target. A child
/// `a:srgbClr`/`a:schemeClr` is only taken when it belongs to a text *fill*
/// (`active_fill`), so a text outline's colour (`a:ln`) is never mistaken for the
/// run colour. The folded result lands in the caller's [`RunStyle`] on close.
#[derive(Default)]
struct PptxRunPr {
    /// True while inside an `a:rPr` (the caller routes child tokens to us).
    active: bool,
    /// True while inside the run's `a:solidFill` or the first `a:gradFill` stop.
    in_fill: bool,
    /// True once a `gradFill`'s first `a:gs` stop has been consumed (later stops
    /// are ignored — only the first becomes the solid fallback).
    grad_stop_done: bool,
    fill: PptxFillColor,
    /// Typeface from `a:latin@typeface` (resolved through the theme on close).
    latin: Option<String>,
    /// Hyperlink relationship id from `a:hlinkClick@r:id` (resolved on close).
    hlink_rid: Option<String>,
}

impl PptxRunPr {
    /// Begin collecting an `a:rPr` subtree.
    fn open() -> Self {
        PptxRunPr {
            active: true,
            ..PptxRunPr::default()
        }
    }

    /// Handle an open tag inside the `a:rPr`. Enters a text `a:solidFill` or the
    /// first `a:gradFill` stop, records colour bases/modifiers while in a fill,
    /// and captures `a:latin` / `a:hlinkClick` at the run level.
    fn on_open(&mut self, ln: &str, attrs: &[(String, String)]) {
        match ln {
            "solidFill" => self.in_fill = true,
            // A gradient: capture the first stop's colour as a solid fallback.
            "gs" if !self.grad_stop_done => self.in_fill = true,
            "srgbClr" | "schemeClr" | "hslClr" | "sysClr" if self.in_fill => {
                self.fill.set_base(ln, attrs);
            }
            "lumMod" | "lumOff" | "shade" | "tint" if self.in_fill => self.fill.set_mod(ln, attrs),
            "latin" => {
                self.latin = attr(attrs, "typeface").map(|t| t.to_string());
            }
            "hlinkClick" => {
                self.hlink_rid = attr(attrs, "id")
                    .filter(|v| !v.trim().is_empty())
                    .map(|v| v.to_string());
            }
            _ => {}
        }
    }

    /// Handle a close tag inside the `a:rPr`; returns `true` when it closes the
    /// `a:rPr` itself (the caller then folds the result via [`close`](Self::close)).
    fn on_close(&mut self, ln: &str) -> bool {
        match ln {
            "solidFill" => {
                self.in_fill = false;
                false
            }
            "gs" => {
                if self.in_fill {
                    self.grad_stop_done = true;
                }
                self.in_fill = false;
                false
            }
            "rPr" => true,
            _ => false,
        }
    }

    /// Fold the collected colour / typeface / hyperlink into `run` and reset.
    fn close(&mut self, run: &mut RunStyle, theme: &PptxTheme, rels: &BTreeMap<String, String>) {
        let collected = std::mem::take(self);
        if let Some(color) = collected.fill.finish_with(theme) {
            run.color = Some(color);
        }
        if let Some(t) = collected.latin {
            run.font_family = theme.resolve(&t);
        }
        if let Some(rid) = collected.hlink_rid {
            if let Some(url) = rels.get(&rid).filter(|u| !u.trim().is_empty()) {
                run.hyperlink = Some(url.clone());
            }
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

/// Lower a `style:text-properties` element's attributes to a CSS declaration
/// string the **model** char-style parser ([`odf_css_char_style`]) understands:
/// `fo:font-weight`/`-style`/`-color`/`-size`, `style:text-underline-style`,
/// `style:text-line-through-*` (→ strikethrough), `fo:background-color` (run
/// highlight) and `fo:font-name`/`style:font-name`. Shared by [`odf_text_styles`]
/// (run styling) and [`odf_named_styles`] (named-style table char defaults) so
/// the mapping lives in one place. Distinct from [`odf_text_props_css`], which
/// targets the WYSIWYG HTML render path and omits strike/highlight. An empty
/// string means no recognised properties.
fn odf_text_props_char_css(attrs: &[(String, String)]) -> String {
    let mut css = String::new();
    if let Some(w) = attr(attrs, "font-weight") {
        if w == "bold" {
            css.push_str("font-weight:bold;");
        }
    }
    if let Some(s) = attr(attrs, "font-style") {
        if s == "italic" || s == "oblique" {
            css.push_str("font-style:italic;");
        }
    }
    if let Some(c) = attr(attrs, "color") {
        let hex = c.trim_start_matches('#');
        if is_hex6(hex) {
            css.push_str(&format!("color:#{};", hex.to_ascii_uppercase()));
        }
    }
    if let Some(u) = attr(attrs, "text-underline-style") {
        if u != "none" {
            css.push_str("text-decoration:underline;");
        }
    }
    // `style:text-line-through-style`/`-type` (≠ none) ⇒ strikethrough. ODF
    // carries it as its own property, not a CSS `text-decoration`; emit a
    // `line-through` token the model char-style parser recognises.
    if odf_line_through_set(attrs) {
        css.push_str("text-decoration:line-through;");
    }
    // `fo:background-color` on a text style ⇒ run highlight
    // (`CharStyle.background`). `transparent`/`none` ⇒ none.
    if let Some(bg) = attr(attrs, "background-color") {
        let b = bg.trim();
        if !matches!(b, "transparent" | "none" | "") {
            let hex = b.trim_start_matches('#');
            if is_hex6(hex) {
                css.push_str(&format!("background-color:#{};", hex.to_ascii_uppercase()));
            }
        }
    }
    if let Some(sz) = attr(attrs, "font-size") {
        if let Some(pt) = parse_odf_pt(sz) {
            css.push_str(&format!("font-size:{pt}pt;"));
        }
    }
    // `fo:font-name` (or `style:font-name`) → real family so the host embeds the
    // matching face and uses its metrics.
    if let Some(fam) = attr(attrs, "font-name") {
        let family = css_font_family(fam);
        if !family.is_empty() {
            css.push_str(&format!("font-family:{family};"));
        }
    }
    css
}

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
                        let css = odf_text_props_char_css(&attrs);
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

/// Build a `drawing-page-style-name → background RGB` map from an ODF part
/// (`styles.xml` or `content.xml`). Reads each `style:style` of family
/// `drawing-page` whose `style:drawing-page-properties` declares a solid fill
/// (`draw:fill="solid"` — or omitted but with a colour present — and a 6-hex
/// `draw:fill-color`). `draw:fill="none"`/`bitmap`/`gradient`/`hatch` are skipped
/// (gradient/bitmap page fills are deferred — they reference a named fill style).
fn odf_drawing_page_fills(xml: &str) -> BTreeMap<String, [f64; 3]> {
    let mut map = BTreeMap::new();
    let mut x = Xml::new(xml);
    // The current `style:style`: its name plus whether it is family `drawing-page`
    // (only those carry a page background fill).
    let mut cur_name: Option<String> = None;
    let mut is_page_family = false;
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, _) => match local(&name) {
                "style" => {
                    cur_name = attr(&attrs, "name").map(str::to_string);
                    is_page_family = attr(&attrs, "family") == Some("drawing-page");
                }
                "drawing-page-properties" => {
                    if let Some(nm) = cur_name.clone() {
                        if is_page_family {
                            if let Some(rgb) = odf_solid_fill_rgb(&attrs) {
                                map.insert(nm, rgb);
                            }
                        }
                    }
                }
                _ => {}
            },
            Tok::Close(name) if local(&name) == "style" => {
                cur_name = None;
                is_page_family = false;
            }
            _ => {}
        }
    }
    map
}

/// Resolve a `style:drawing-page-properties`' solid fill to an RGB colour: the
/// `draw:fill-color` (6-hex) when `draw:fill` is `solid` (or absent — LibreOffice
/// often omits it while still setting a colour). Any other `draw:fill`
/// (`none`/`gradient`/`bitmap`/`hatch`) yields `None`.
fn odf_solid_fill_rgb(attrs: &[(String, String)]) -> Option<[f64; 3]> {
    match attr(attrs, "fill") {
        Some("solid") | None => {}
        _ => return None,
    }
    let raw = attr(attrs, "fill-color")?.trim().trim_start_matches('#');
    if is_hex6(raw) {
        hex_to_rgb_f64(raw)
    } else {
        None
    }
}

/// Build a `master-page-name → drawing-page-style-name` map from an ODF part
/// (`styles.xml`). Each `style:master-page` names a `draw:style-name` carrying
/// its page background; a `draw:page` without its own fill inherits this.
fn odf_master_page_styles(xml: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let mut x = Xml::new(xml);
    while let Some(tok) = x.next() {
        if let Tok::Open(name, attrs, _) = tok {
            if local(&name) == "master-page" {
                if let (Some(nm), Some(sty)) = (attr(&attrs, "name"), attr(&attrs, "style-name")) {
                    map.insert(nm.to_string(), sty.to_string());
                }
            }
        }
    }
    map
}

/// Resolve a `draw:page`'s background from its open-tag attributes: its own
/// `draw:style-name` fill first, else its `draw:master-page-name`'s master
/// drawing-page-style fill. `None` when neither resolves to a solid colour.
fn odp_page_background(
    page_attrs: &[(String, String)],
    page_fills: &BTreeMap<String, [f64; 3]>,
    master_styles: &BTreeMap<String, String>,
) -> Option<[f64; 3]> {
    if let Some(rgb) = attr(page_attrs, "style-name").and_then(|s| page_fills.get(s)) {
        return Some(*rgb);
    }
    attr(page_attrs, "master-page-name")
        .and_then(|m| master_styles.get(m))
        .and_then(|s| page_fills.get(s))
        .copied()
}

/// True when an ODF text style declares an active strikethrough:
/// `style:text-line-through-style` (the primary property) ≠ `none`, or, when that
/// is absent, `style:text-line-through-type` ≠ `none`. Either being present and
/// non-`none` marks the run as struck through.
fn odf_line_through_set(attrs: &[(String, String)]) -> bool {
    let style = attr(attrs, "text-line-through-style");
    let kind = attr(attrs, "text-line-through-type");
    matches!(style, Some(s) if !s.eq_ignore_ascii_case("none"))
        || (style.is_none() && matches!(kind, Some(k) if !k.eq_ignore_ascii_case("none")))
}

/// Paragraph-level formatting collected from a `style:paragraph-properties`
/// element. Every field is optional so a child style only overrides what it
/// names; `parent` chains to the `style:parent-style-name` (resolved by
/// [`odf_para_styles`]). Distances are points (ODF lengths via [`parse_odf_pt`]).
#[derive(Default, Clone)]
struct OdfParaProps {
    parent: Option<String>,
    /// `fo:text-align` (`start`/`end`/`center`/`justify`) → model [`MAlign`].
    align: Option<MAlign>,
    /// `fo:margin-top` (space above) in points.
    space_before_pt: Option<f64>,
    /// `fo:margin-bottom` (space below) in points.
    space_after_pt: Option<f64>,
    /// `fo:margin-left` in points.
    indent_left_pt: Option<f64>,
    /// `fo:margin-right` in points.
    indent_right_pt: Option<f64>,
    /// `fo:text-indent` (first-line indent; may be negative for a hanging
    /// indent) in points.
    first_line_pt: Option<f64>,
    /// `fo:line-height`: a `%` value → a unitless multiple, a length → fixed
    /// points (a bare number is treated as a length, per ODF).
    line_height: Option<MLineHeight>,
}

impl OdfParaProps {
    /// Fold a parent style's properties under this one: a field set on `self`
    /// (the more-derived style) wins; the parent fills the gaps. `parent` is
    /// taken from `self` (a style references at most one parent).
    fn inherit(&mut self, base: &OdfParaProps) {
        self.align = self.align.or(base.align);
        self.space_before_pt = self.space_before_pt.or(base.space_before_pt);
        self.space_after_pt = self.space_after_pt.or(base.space_after_pt);
        self.indent_left_pt = self.indent_left_pt.or(base.indent_left_pt);
        self.indent_right_pt = self.indent_right_pt.or(base.indent_right_pt);
        self.first_line_pt = self.first_line_pt.or(base.first_line_pt);
        self.line_height = self.line_height.or(base.line_height);
    }

    /// Lower to a model [`ParagraphStyle`], stacking a list indent (each level
    /// adds [`LIST_LEVEL_INDENT_PT`], mirroring [`para_style_model`]) on top of
    /// any explicit left margin. Unset fields fall back to the model defaults.
    fn to_paragraph_style(&self, list_level: Option<u32>) -> ParagraphStyle {
        let list_indent = list_level
            .map(|lvl| (lvl as f64 + 1.0) * LIST_LEVEL_INDENT_PT)
            .unwrap_or(0.0);
        ParagraphStyle {
            align: self.align.unwrap_or(MAlign::Left),
            space_before_pt: self.space_before_pt.unwrap_or(0.0),
            space_after_pt: self.space_after_pt.unwrap_or(0.0),
            indent_left_pt: self.indent_left_pt.unwrap_or(0.0) + list_indent,
            indent_right_pt: self.indent_right_pt.unwrap_or(0.0),
            first_line_pt: self.first_line_pt.unwrap_or(0.0),
            line_height: self.line_height.unwrap_or(MLineHeight::Normal),
        }
    }
}

/// Map an ODF `fo:text-align` value (`start`/`end`/`center`/`justify`, plus the
/// `left`/`right` synonyms seen in the wild) to a model [`MAlign`]. ODF is
/// writing-direction aware (`start`/`end`); LTR is assumed (the engine has no
/// RTL flow), so `start`→left and `end`→right.
fn odf_text_align(v: &str) -> Option<MAlign> {
    match v.trim() {
        "center" => Some(MAlign::Center),
        "end" | "right" => Some(MAlign::Right),
        "justify" => Some(MAlign::Justify),
        "start" | "left" => Some(MAlign::Left),
        _ => None,
    }
}

/// Parse an ODF `fo:line-height`: a `%` value is a unitless multiple of the font
/// size (`150%` → `1.5`); any length (`14pt`, `0.5cm`, or a bare number treated
/// as a length per ODF) is a fixed leading in points. `normal` (or anything
/// unrecognised) ⇒ `None` (the model default leading).
fn odf_line_height(v: &str) -> Option<MLineHeight> {
    let v = v.trim();
    if let Some(pct) = v.strip_suffix('%') {
        return pct
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|p| *p > 0.0)
            .map(|p| MLineHeight::Multiple(p / 100.0));
    }
    if v.eq_ignore_ascii_case("normal") {
        return None;
    }
    parse_odf_pt(v)
        .filter(|p| *p > 0.0)
        .map(MLineHeight::Points)
}

/// Build a `paragraph-style-name → resolved [`ParagraphStyle`] source` map from
/// an ODF part: each `style:style` (any family — paragraph styles carry the
/// `fo:*` formatting, but ODF also lets a cell/graphic style hold one) with a
/// `style:paragraph-properties` child. `style:parent-style-name` chains are
/// resolved so a derived style inherits its base's formatting. Empty entries
/// (no formatting and no parent) are omitted. Mirrors [`odf_text_styles`].
fn odf_para_styles(xml: &str) -> BTreeMap<String, OdfParaProps> {
    let mut raw: BTreeMap<String, OdfParaProps> = BTreeMap::new();
    let mut x = Xml::new(xml);
    let mut cur_name: Option<String> = None;
    let mut cur = OdfParaProps::default();
    let mut seen = false;
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, _) => match local(&name) {
                "style" => {
                    cur_name = attr(&attrs, "name").map(|s| s.to_string());
                    cur = OdfParaProps {
                        parent: attr(&attrs, "parent-style-name").map(|s| s.to_string()),
                        ..OdfParaProps::default()
                    };
                    seen = cur.parent.is_some();
                }
                "paragraph-properties" => {
                    if cur_name.is_some() {
                        seen = true;
                        if let Some(a) = attr(&attrs, "text-align").and_then(odf_text_align) {
                            cur.align = Some(a);
                        }
                        if let Some(v) = attr(&attrs, "margin-top").and_then(parse_odf_pt) {
                            cur.space_before_pt = Some(v.max(0.0));
                        }
                        if let Some(v) = attr(&attrs, "margin-bottom").and_then(parse_odf_pt) {
                            cur.space_after_pt = Some(v.max(0.0));
                        }
                        if let Some(v) = attr(&attrs, "margin-left").and_then(parse_odf_pt) {
                            cur.indent_left_pt = Some(v);
                        }
                        if let Some(v) = attr(&attrs, "margin-right").and_then(parse_odf_pt) {
                            cur.indent_right_pt = Some(v);
                        }
                        if let Some(v) = attr(&attrs, "text-indent").and_then(parse_odf_pt) {
                            cur.first_line_pt = Some(v);
                        }
                        if let Some(lh) = attr(&attrs, "line-height").and_then(odf_line_height) {
                            cur.line_height = Some(lh);
                        }
                    }
                }
                _ => {}
            },
            Tok::Close(name) => {
                if local(&name) == "style" {
                    if let Some(nm) = cur_name.take() {
                        if seen {
                            raw.insert(nm, std::mem::take(&mut cur));
                        }
                    }
                    cur = OdfParaProps::default();
                    seen = false;
                }
            }
            Tok::Text(_) => {}
        }
    }
    // Flatten parent chains so a lookup needs no further resolution. The chain is
    // bounded (cap 16) to ignore any pathological cycle in malformed input.
    let mut resolved: BTreeMap<String, OdfParaProps> = BTreeMap::new();
    for (name, props) in &raw {
        let mut acc = props.clone();
        let mut parent = props.parent.clone();
        for _ in 0..16 {
            let Some(p) = parent else { break };
            let Some(base) = raw.get(&p) else { break };
            acc.inherit(base);
            parent = base.parent.clone();
        }
        resolved.insert(name.clone(), acc);
    }
    resolved
}

/// Build the model's named-style **table** ([`StyleTable`]) from an ODF
/// `styles.xml` (the `office:styles` part). Each `style:style` of family
/// `paragraph` becomes a [`NamedStyle`] keyed by its `style:name`
/// ([`StyleId`]): its `style:paragraph-properties` lower to the para defaults
/// (the same `fo:*` mappings [`odf_para_styles`] uses, via [`OdfParaProps`]),
/// its `style:text-properties` lower to the char defaults (the same mappings
/// run styling uses, via [`odf_text_props_char_css`] → [`odf_css_char_style`]),
/// and `style:parent-style-name` becomes [`NamedStyle::based_on`] — the
/// inheritance **edge is kept, not flattened**, so the model resolves it itself
/// (mirrors the DOCX [`DocxStyles::to_style_table`]).
///
/// Only `paragraph`-family styles are lowered: the model's named styles are
/// paragraph+character defaults, so `text` (run), `table`, `graphic`, … styles
/// have no paragraph identity to host and are skipped (a `text:span`'s run
/// styling is still applied inline). `style:display-name` (the human label) has
/// no model slot — the [`StyleId`] key carries the machine name — so it is not
/// retained.
fn odf_named_styles(xml: &str) -> model::StyleTable {
    let mut named = BTreeMap::new();
    let mut x = Xml::new(xml);
    // Per-style accumulation state.
    let mut cur_name: Option<String> = None;
    let mut is_para_family = false;
    let mut parent: Option<String> = None;
    let mut para = OdfParaProps::default();
    let mut char_css = String::new();
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, _) => match local(&name) {
                "style" => {
                    cur_name = attr(&attrs, "name").map(str::to_string);
                    is_para_family = attr(&attrs, "family") == Some("paragraph");
                    parent = attr(&attrs, "parent-style-name").map(str::to_string);
                    para = OdfParaProps::default();
                    char_css = String::new();
                }
                // Paragraph formatting (`fo:*`) — same fields as `odf_para_styles`.
                "paragraph-properties" if is_para_family && cur_name.is_some() => {
                    if let Some(a) = attr(&attrs, "text-align").and_then(odf_text_align) {
                        para.align = Some(a);
                    }
                    if let Some(v) = attr(&attrs, "margin-top").and_then(parse_odf_pt) {
                        para.space_before_pt = Some(v.max(0.0));
                    }
                    if let Some(v) = attr(&attrs, "margin-bottom").and_then(parse_odf_pt) {
                        para.space_after_pt = Some(v.max(0.0));
                    }
                    if let Some(v) = attr(&attrs, "margin-left").and_then(parse_odf_pt) {
                        para.indent_left_pt = Some(v);
                    }
                    if let Some(v) = attr(&attrs, "margin-right").and_then(parse_odf_pt) {
                        para.indent_right_pt = Some(v);
                    }
                    if let Some(v) = attr(&attrs, "text-indent").and_then(parse_odf_pt) {
                        para.first_line_pt = Some(v);
                    }
                    if let Some(lh) = attr(&attrs, "line-height").and_then(odf_line_height) {
                        para.line_height = Some(lh);
                    }
                }
                // Run defaults (`fo:*`/`style:*`) — same fields as run styling.
                "text-properties" if is_para_family && cur_name.is_some() => {
                    char_css = odf_text_props_char_css(&attrs);
                }
                _ => {}
            },
            Tok::Close(name) => {
                if local(&name) == "style" {
                    if let (true, Some(nm)) = (is_para_family, cur_name.take()) {
                        named.insert(
                            model::StyleId(nm),
                            model::NamedStyle {
                                // No list context for a named style ⇒ no list indent.
                                para: para.to_paragraph_style(None),
                                char_: odf_css_char_style(&char_css),
                                based_on: parent.take().map(model::StyleId),
                            },
                        );
                    }
                    is_para_family = false;
                    parent = None;
                    para = OdfParaProps::default();
                    char_css = String::new();
                }
            }
            Tok::Text(_) => {}
        }
    }
    model::StyleTable { named }
}

/// Build a `cell-style-name → background RGB` map from an ODF part: each
/// `style:style` (a table-cell style) whose `style:table-cell-properties` carries
/// a real `fo:background-color` (6-hex; `transparent`/`none` ignored). Used to
/// lower ODT table cell shading onto the model. Mirrors [`odf_column_widths`].
fn odf_cell_backgrounds(xml: &str) -> BTreeMap<String, [f64; 3]> {
    let mut map = BTreeMap::new();
    let mut x = Xml::new(xml);
    let mut cur_name: Option<String> = None;
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, _) => match local(&name) {
                "style" => cur_name = attr(&attrs, "name").map(|s| s.to_string()),
                "table-cell-properties" => {
                    if let Some(nm) = &cur_name {
                        if let Some(rgb) = attr(&attrs, "background-color")
                            .map(str::trim)
                            .filter(|b| !matches!(*b, "transparent" | "none" | ""))
                            .and_then(|b| hex_to_rgb_f64(b.trim_start_matches('#')))
                        {
                            map.insert(nm.clone(), rgb);
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

/// Normalise an ODF `table:formula` to a bare expression: strip the OpenFormula
/// namespace prefix (`of:` / any `prefix:` before the `=`) and the leading `=`,
/// trim, and drop empties. `of:=SUM([.A1:.A9])` ⇒ `SUM([.A1:.A9])`. The OpenFormula
/// cell-reference syntax (`[.A1]`) is preserved verbatim — the model stores the
/// authored expression, not a translated one.
fn odf_formula_expr(raw: &str) -> Option<String> {
    let s = raw.trim();
    // A leading `namespace:` qualifier precedes the `=` in ODF (`of:=…`,
    // `oooc:=…`). Strip it only when it sits before the first `=`.
    let body = match (s.find(':'), s.find('=')) {
        (Some(c), Some(e)) if c < e => &s[c + 1..],
        _ => s,
    };
    let expr = body.strip_prefix('=').unwrap_or(body).trim();
    (!expr.is_empty()).then(|| expr.to_string())
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

/// Build a `cell-style-name → CSS` map from an ODF part, for the WYSIWYG render
/// of spreadsheet cells. Each `style:style` (any family) contributes its
/// `style:text-properties` (font weight/style/underline/colour/size/family) and
/// `style:table-cell-properties` (collapsed `fo:border*` → uniform `border`,
/// `fo:background-color`, `style:vertical-align`). `None`-valued styles are
/// simply omitted. Mirrors [`odf_text_styles`] but also reads cell properties.
fn odf_cell_styles(xml: &str) -> BTreeMap<String, String> {
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    let mut x = Xml::new(xml);
    let mut cur_name: Option<String> = None;
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, _) => match local(&name) {
                "style" => cur_name = attr(&attrs, "name").map(|s| s.to_string()),
                "text-properties" => {
                    if let Some(nm) = &cur_name {
                        let css = odf_text_props_css(&attrs);
                        if !css.is_empty() {
                            map.entry(nm.clone()).or_default().push_str(&css);
                        }
                    }
                }
                "table-cell-properties" => {
                    if let Some(nm) = &cur_name {
                        let css = odf_cell_props_css(&attrs);
                        if !css.is_empty() {
                            map.entry(nm.clone()).or_default().push_str(&css);
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

/// CSS for a `style:text-properties` element: `fo:font-weight`/`fo:font-style`/
/// `fo:color`/`text-underline-style`/`fo:font-size`/`fo:font-name`.
fn odf_text_props_css(attrs: &[(String, String)]) -> String {
    let mut css = String::new();
    if attr(attrs, "font-weight") == Some("bold") {
        css.push_str("font-weight:bold;");
    }
    if matches!(attr(attrs, "font-style"), Some("italic") | Some("oblique")) {
        css.push_str("font-style:italic;");
    }
    if let Some(c) = attr(attrs, "color") {
        let hex = c.trim_start_matches('#');
        if is_hex6(hex) {
            css.push_str(&format!("color:#{};", hex.to_ascii_uppercase()));
        }
    }
    if matches!(attr(attrs, "text-underline-style"), Some(u) if u != "none") {
        css.push_str("text-decoration:underline;");
    }
    if let Some(pt) = attr(attrs, "font-size").and_then(parse_odf_pt) {
        css.push_str(&format!("font-size:{}pt;", fmt_pt(pt)));
    }
    if let Some(fam) = attr(attrs, "font-name") {
        let family = css_font_family(fam);
        if !family.is_empty() {
            css.push_str(&format!("font-family:{family};"));
        }
    }
    css
}

/// CSS for a `style:table-cell-properties` element. ODF borders are per-edge,
/// but the HTML engine honours one uniform `border`, so we collapse: `fo:border`
/// (uniform) when present, else the first styled edge among top/right/bottom/
/// left. Also `fo:background-color` and `style:vertical-align`.
fn odf_cell_props_css(attrs: &[(String, String)]) -> String {
    let mut css = String::new();
    if let Some(bg) = attr(attrs, "background-color") {
        let b = bg.trim();
        // `transparent`/`none` ⇒ no fill; a `#RRGGBB` becomes the background.
        if !matches!(b, "transparent" | "none" | "") {
            let hex = b.trim_start_matches('#');
            if is_hex6(hex) {
                css.push_str(&format!("background-color:#{};", hex.to_ascii_uppercase()));
            }
        }
    }
    // Uniform border first; otherwise the first per-edge border that is set.
    let border = attr(attrs, "border").or_else(|| {
        ["border-top", "border-right", "border-bottom", "border-left"]
            .iter()
            .find_map(|k| attr(attrs, k))
    });
    if let Some(spec) = border
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "none")
    {
        css.push_str(&format!("border:{spec};"));
    }
    if let Some(va) = attr(attrs, "vertical-align") {
        let v = match va {
            "top" => "top",
            "middle" => "middle",
            "bottom" => "bottom",
            _ => "",
        };
        if !v.is_empty() {
            css.push_str(&format!("vertical-align:{v};"));
        }
    }
    css
}

/// Build a `row-style-name → row-height(pt)` map from an ODF part. Reads each
/// `style:style`'s `style:table-row-properties/@style:row-height` (ODF lengths).
fn odf_row_heights(xml: &str) -> BTreeMap<String, f64> {
    let mut map = BTreeMap::new();
    let mut x = Xml::new(xml);
    let mut cur_name: Option<String> = None;
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, _) => match local(&name) {
                "style" => cur_name = attr(&attrs, "name").map(|s| s.to_string()),
                "table-row-properties" => {
                    if let Some(nm) = &cur_name {
                        if let Some(h) = attr(&attrs, "row-height")
                            .and_then(parse_odf_pt)
                            .filter(|h| *h > 0.0)
                        {
                            map.insert(nm.clone(), h);
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

/// Typed cell formatting recovered from an ODF `style:style` (cell family),
/// mirroring the [`SheetCell`] style fields the XLSX model path fills.
#[derive(Debug, Clone, Default)]
struct OdsCellProps {
    char_: CharStyle,
    fill: Option<[f64; 3]>,
    border: Option<model::BorderStyle>,
    align: Option<model::Align>,
    wrap: bool,
    number_format: Option<String>,
}

/// Resolved ODS style tables for the typed model path: column widths, row
/// heights, and per-style cell formatting (number format / fill / font / border /
/// alignment / wrap). [`absorb`](OdsStyleTables::absorb) folds one ODF part in;
/// call it on `styles.xml` then `content.xml` so automatic styles override the
/// named ones (later insert wins).
#[derive(Debug, Default)]
struct OdsStyleTables {
    col_widths: BTreeMap<String, f64>,
    row_heights: BTreeMap<String, f64>,
    cells: BTreeMap<String, OdsCellProps>,
}

impl OdsStyleTables {
    /// Fold one ODF part's column/row/cell styles into the tables (later parts
    /// override earlier ones for same-named styles).
    fn absorb(&mut self, xml: &str) {
        for (k, v) in odf_column_widths(xml) {
            self.col_widths.insert(k, v);
        }
        for (k, v) in odf_row_heights(xml) {
            self.row_heights.insert(k, v);
        }
        let data_styles = odf_data_styles(xml);
        for (k, v) in odf_cell_props(xml, &data_styles) {
            self.cells.insert(k, v);
        }
    }
}

/// Build a `cell-style-name → `[`OdsCellProps`]` map from one ODF part: each
/// `style:style` (cell family) contributes its `style:text-properties` (font,
/// colour, size, weight/italic/underline/strike → [`CharStyle`]),
/// `style:table-cell-properties` (`fo:background-color` → fill, collapsed
/// `fo:border*` → [`BorderStyle`], `fo:wrap-option=wrap` → wrap),
/// `style:paragraph-properties` (`fo:text-align` → [`Align`]), and the
/// number-format code resolved from `@style:data-style-name` against
/// `data_styles`. A style adding nothing at all is omitted.
fn odf_cell_props(
    xml: &str,
    data_styles: &BTreeMap<String, String>,
) -> BTreeMap<String, OdsCellProps> {
    let mut map: BTreeMap<String, OdsCellProps> = BTreeMap::new();
    let mut x = Xml::new(xml);
    let mut cur: Option<(String, OdsCellProps, bool)> = None; // (name, props, any)
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => match local(&name) {
                "style" => {
                    // Only cell-family (or untyped) styles describe cell boxes.
                    let fam = attr(&attrs, "family");
                    if matches!(fam, None | Some("table-cell")) {
                        let nm = attr(&attrs, "name").map(str::to_string);
                        let mut props = OdsCellProps::default();
                        let mut any = false;
                        if let Some(code) = attr(&attrs, "data-style-name")
                            .and_then(|d| data_styles.get(d))
                            .cloned()
                        {
                            props.number_format = Some(code);
                            any = true;
                        }
                        // A self-closing `<style:style …/>` (e.g. data-style only,
                        // no property children) has no matching close: record now.
                        if sc {
                            if let (Some(n), true) = (nm, any) {
                                map.insert(n, props);
                            }
                            cur = None;
                        } else {
                            cur = nm.map(|n| (n, props, any));
                        }
                    } else {
                        cur = None;
                    }
                }
                "text-properties" => {
                    if let Some((_, props, any)) = cur.as_mut() {
                        let css = odf_text_props_css(&attrs);
                        if !css.is_empty() {
                            props.char_ = odf_css_char_style(&css);
                            *any = true;
                        }
                    }
                }
                "table-cell-properties" => {
                    if let Some((_, props, any)) = cur.as_mut() {
                        if let Some(c) = odf_cell_fill(&attrs) {
                            props.fill = Some(c);
                            *any = true;
                        }
                        if let Some(b) = odf_cell_border(&attrs) {
                            props.border = Some(b);
                            *any = true;
                        }
                        if odf_cell_wrap(&attrs) {
                            props.wrap = true;
                            *any = true;
                        }
                    }
                }
                "paragraph-properties" => {
                    if let Some((_, props, any)) = cur.as_mut() {
                        if let Some(a) = attr(&attrs, "text-align").and_then(odf_align) {
                            props.align = Some(a);
                            *any = true;
                        }
                    }
                }
                _ => {}
            },
            Tok::Close(name) => {
                if local(&name) == "style" {
                    if let Some((nm, props, any)) = cur.take() {
                        if any {
                            map.insert(nm, props);
                        }
                    }
                }
            }
            Tok::Text(_) => {}
        }
    }
    map
}

/// `fo:background-color` of a `style:table-cell-properties` → typed RGB fill.
/// `transparent`/`none`/malformed ⇒ `None`.
fn odf_cell_fill(attrs: &[(String, String)]) -> Option<[f64; 3]> {
    let b = attr(attrs, "background-color")?.trim();
    if matches!(b, "transparent" | "none" | "") {
        return None;
    }
    hex_to_rgb_f64(b.trim_start_matches('#'))
}

/// Collapse an ODF cell's per-edge borders to one [`BorderStyle`]: prefer the
/// uniform `fo:border`, else the first styled edge. Parses the CSS-like
/// `<width> <style> <color>` shorthand (width via [`parse_odf_pt`], colour via a
/// trailing `#RRGGBB`); a `none`/zero-width border yields `None`.
fn odf_cell_border(attrs: &[(String, String)]) -> Option<model::BorderStyle> {
    let spec = attr(attrs, "border")
        .or_else(|| {
            ["border-top", "border-right", "border-bottom", "border-left"]
                .iter()
                .find_map(|k| attr(attrs, k))
        })?
        .trim();
    if spec.is_empty() || spec.eq_ignore_ascii_case("none") {
        return None;
    }
    let mut width = 0.0_f64;
    let mut color = [0.0_f64; 3];
    for tok in spec.split_whitespace() {
        if let Some(c) = tok.strip_prefix('#').filter(|h| is_hex6(h)) {
            if let Some(rgb) = hex_to_rgb_f64(c) {
                color = rgb;
            }
        } else if let Some(w) = parse_odf_pt(tok) {
            // The `solid`/`double`/… style keyword has no numeric value, so only
            // a real length sets the width.
            if w > 0.0 {
                width = w;
            }
        }
    }
    (width > 0.0).then_some(model::BorderStyle { width, color })
}

/// True when a `style:table-cell-properties` requests text wrapping
/// (`fo:wrap-option="wrap"`).
fn odf_cell_wrap(attrs: &[(String, String)]) -> bool {
    attr(attrs, "wrap-option") == Some("wrap")
}

/// Map an ODF `fo:text-align` value to a model [`Align`]. ODF uses the CSS
/// keywords plus the bidi `start`/`end` (resolved LTR). Unknown ⇒ `None`.
fn odf_align(v: &str) -> Option<model::Align> {
    match v.trim() {
        "start" | "left" => Some(model::Align::Left),
        "center" => Some(model::Align::Center),
        "end" | "right" => Some(model::Align::Right),
        "justify" => Some(model::Align::Justify),
        _ => None,
    }
}

/// The kind of an ODF `number:*-style`, used to finish its format code.
#[derive(Debug, Clone, Copy)]
enum DataStyleKind {
    Number,
    Percentage,
    Currency,
}

/// Build a `data-style-name → spreadsheet-format-code` map from one ODF part.
/// Reconstructs a `0.00` / `#,##0` / `0%` / `$#,##0.00` style code from the
/// `number:number` / `number:currency-symbol` / `number:percentage` children of
/// each numeric `number:*-style`. Best-effort: only the common numeric, percent
/// and currency forms are reconstructed; date/time styles (whose field pattern we
/// don't rebuild) are omitted, leaving such cells unformatted.
fn odf_data_styles(xml: &str) -> BTreeMap<String, String> {
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    let mut x = Xml::new(xml);
    let mut cur: Option<(String, DataStyleKind, String)> = None; // (name, kind, code)
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, _) => {
                let ln = local(&name);
                match ln {
                    "number-style" | "percentage-style" | "currency-style" => {
                        let nm = attr(&attrs, "name").map(str::to_string);
                        let kind = match ln {
                            "percentage-style" => DataStyleKind::Percentage,
                            "currency-style" => DataStyleKind::Currency,
                            _ => DataStyleKind::Number,
                        };
                        cur = nm.map(|n| (n, kind, String::new()));
                    }
                    "number" => {
                        if let Some((_, _, code)) = cur.as_mut() {
                            let dec = attr(&attrs, "decimal-places")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                                .unwrap_or(0)
                                .min(20);
                            let group = attr(&attrs, "grouping") == Some("true");
                            code.push_str(if group { "#,##0" } else { "0" });
                            if dec > 0 {
                                code.push('.');
                                for _ in 0..dec {
                                    code.push('0');
                                }
                            }
                        }
                    }
                    "currency-symbol" => {
                        // A leading symbol prefixes the number part (best effort:
                        // emit a `$` when no numeric child has run yet).
                        if let Some((_, _, code)) = cur.as_mut() {
                            if !code.contains(['#', '0']) {
                                code.push('$');
                            }
                        }
                    }
                    _ => {}
                }
            }
            Tok::Close(name) => {
                if matches!(
                    local(&name),
                    "number-style" | "percentage-style" | "currency-style"
                ) {
                    if let Some((nm, kind, mut code)) = cur.take() {
                        match kind {
                            DataStyleKind::Percentage => {
                                if code.is_empty() {
                                    code.push('0');
                                }
                                code.push('%');
                            }
                            DataStyleKind::Currency => {
                                if !code.contains('$') {
                                    code.insert(0, '$');
                                }
                                if !code.contains(['#', '0']) {
                                    code.push_str("#,##0.00");
                                }
                            }
                            DataStyleKind::Number => {}
                        }
                        if !code.is_empty() {
                            map.insert(nm, code);
                        }
                    }
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
    odt_to_pdf_with(zip, &[])
}

/// Like [`odt_to_pdf`] but also feeds `host` faces (phase 2 of
/// [`office_needed_fonts`]); ODF embedded faces (`Fonts/*`) still win on conflict.
fn odt_to_pdf_with(zip: &BTreeMap<String, Vec<u8>>, host: &[ProvidedFont]) -> Vec<u8> {
    let (body, geom) = odt_body_geom(zip);
    render_geom_with_fonts(&body, geom, &merge_fonts(extract_embedded_fonts(zip), host))
}

/// Build the ODT HTML `<body>` and resolve geometry, without rendering. Shared
/// by [`odt_to_pdf`] and the font-need scan.
fn odt_body_geom(zip: &BTreeMap<String, Vec<u8>>) -> (String, PageGeom) {
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
    (body, geom)
}

/// The ODT HTML `<body>` only — used by the font-need scan.
fn odt_body_html(zip: &BTreeMap<String, Vec<u8>>) -> String {
    odt_body_geom(zip).0
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
    ods_to_pdf_with(zip, &[])
}

/// Like [`ods_to_pdf`] but also feeds `host` faces (phase 2 of
/// [`office_needed_fonts`]); ODF embedded faces (`Fonts/*`) still win on conflict.
fn ods_to_pdf_with(zip: &BTreeMap<String, Vec<u8>>, host: &[ProvidedFont]) -> Vec<u8> {
    let (body, geom) = ods_body_geom(zip);
    render_geom_with_fonts(&body, geom, &merge_fonts(extract_embedded_fonts(zip), host))
}

/// Build the ODS HTML `<body>` (one `<table>` per sheet) and resolve geometry,
/// without rendering. Shared by [`ods_to_pdf`] and the font-need scan.
fn ods_body_geom(zip: &BTreeMap<String, Vec<u8>>) -> (String, PageGeom) {
    let content = part(zip, "content.xml");
    let styles_xml = part(zip, "styles.xml");
    let geom = odf_geom(&styles_xml, &content, PageGeom::tabular_default());
    let mut cols = odf_column_widths(&styles_xml);
    cols.extend(odf_column_widths(&content));
    // Cell-style → CSS and row-style → height, from both the named styles
    // (styles.xml) and the automatic styles (content.xml; latter wins).
    let mut cell_styles = odf_cell_styles(&styles_xml);
    cell_styles.extend(odf_cell_styles(&content));
    let mut row_heights = odf_row_heights(&styles_xml);
    row_heights.extend(odf_row_heights(&content));
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
                ods_table(&mut x, &cols, &cell_styles, &row_heights, &mut body);
            }
        }
    }
    if first {
        body.push_str("<p></p>");
    }
    (body, geom)
}

/// The ODS HTML `<body>` only — used by the font-need scan.
fn ods_body_html(zip: &BTreeMap<String, Vec<u8>>) -> String {
    ods_body_geom(zip).0
}

/// Emit one ODS `table:table` (open consumed) as an HTML `<table>`, expanding
/// repeated rows/columns (cap 64) and reading cell text from `text:p` runs.
/// `table:table-column` declarations seed a leading `<colgroup>` and record each
/// column's `@table:default-cell-style-name`. Each row applies its style's
/// height and resolves cell formatting (cell own style → row default → column
/// default) from `cell_styles` for a WYSIWYG render.
fn ods_table(
    x: &mut Xml,
    cols: &BTreeMap<String, f64>,
    cell_styles: &BTreeMap<String, String>,
    row_heights: &BTreeMap<String, f64>,
    out: &mut String,
) {
    out.push_str("<table>");
    let mut pending_cols = String::new();
    let mut colgroup_done = false;
    // Per-column default cell-style name, expanded over repeats; index = column.
    let mut col_defaults: Vec<Option<String>> = Vec::new();
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => {
                let ln = local(&name);
                if ln == "table-column" {
                    let rep = attr(&attrs, "number-columns-repeated")
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(1)
                        .min(1024);
                    let def = attr(&attrs, "default-cell-style-name").map(str::to_string);
                    for _ in 0..rep {
                        col_defaults.push(def.clone());
                    }
                    odf_push_column(&attrs, cols, &mut pending_cols);
                } else if ln == "table-row" && !sc {
                    flush_odf_colgroup(&mut pending_cols, &mut colgroup_done, out);
                    let rep = attr(&attrs, "number-rows-repeated")
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(1)
                        .min(64);
                    // Row height from the row style; row default cell style.
                    let row_css = attr(&attrs, "style-name")
                        .and_then(|s| row_heights.get(s))
                        .map(|h| format!("height:{}pt", fmt_pt(*h)))
                        .unwrap_or_default();
                    let row_default = attr(&attrs, "default-cell-style-name");
                    let row = ods_row(x, cell_styles, &col_defaults, row_default);
                    // Skip emitting many identical *empty* trailing rows.
                    let emit = if row.trim().is_empty() {
                        rep.min(1)
                    } else {
                        rep
                    };
                    for _ in 0..emit {
                        out.push_str(&format!("<tr{}>{row}</tr>", style_attr(&row_css)));
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
/// until `</table:table-row>`. Each cell's CSS is resolved from its own
/// `@table:style-name`, else the row default, else the column default (by
/// column index, honouring `@number-columns-repeated`), via `cell_styles`.
fn ods_row(
    x: &mut Xml,
    cell_styles: &BTreeMap<String, String>,
    col_defaults: &[Option<String>],
    row_default: Option<&str>,
) -> String {
    let mut out = String::new();
    let mut col = 0usize;
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => {
                let ln = local(&name);
                let is_cell = ln == "table-cell" || ln == "covered-table-cell";
                if is_cell {
                    let rep = attr(&attrs, "number-columns-repeated")
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(1)
                        .min(64);
                    // Resolve style: cell own → row default → column default.
                    let css = attr(&attrs, "style-name")
                        .or(row_default)
                        .and_then(|s| cell_styles.get(s))
                        .map(String::as_str)
                        .or_else(|| {
                            col_defaults
                                .get(col)
                                .and_then(|d| d.as_deref())
                                .and_then(|s| cell_styles.get(s))
                                .map(String::as_str)
                        })
                        .unwrap_or("");
                    let style = style_attr(css);
                    let text = if sc {
                        String::new()
                    } else {
                        ods_cell_text(x, ln)
                    };
                    let trimmed = text.trim();
                    // Many identical *empty* trailing cells collapse to one.
                    let emit = if trimmed.is_empty() { rep.min(1) } else { rep };
                    for _ in 0..emit {
                        out.push_str(&format!("<td{style}>{trimmed}</td>"));
                    }
                    col += rep;
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
    odp_to_pdf_with(zip, &[])
}

/// Like [`odp_to_pdf`] but also feeds `host` faces (phase 2 of
/// [`office_needed_fonts`]); ODF embedded faces (`Fonts/*`) still win on conflict.
fn odp_to_pdf_with(zip: &BTreeMap<String, Vec<u8>>, host: &[ProvidedFont]) -> Vec<u8> {
    let (body, geom) = odp_body_geom(zip);
    render_geom_with_fonts(&body, geom, &merge_fonts(extract_embedded_fonts(zip), host))
}

/// Build the ODP HTML `<body>` (one slide per page) and resolve geometry,
/// without rendering. Shared by [`odp_to_pdf`] and the font-need scan.
fn odp_body_geom(zip: &BTreeMap<String, Vec<u8>>) -> (String, PageGeom) {
    let content = part(zip, "content.xml");
    let styles_xml = part(zip, "styles.xml");
    let mut styles = odf_text_styles(&styles_xml);
    styles.extend(odf_text_styles(&content));
    let mut geom = odf_geom(&styles_xml, &content, PageGeom::slide_default());
    // Slides bleed to the edges: drop the prose content margins so absolutely
    // positioned `draw:frame`s (whose `svg:x/y` are page-relative) land at the
    // right place — the layout engine's initial containing block is the page box.
    geom.margins = Margins::uniform(0.0);
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
    (body, geom)
}

/// The ODP HTML `<body>` only — used by the font-need scan.
fn odp_body_html(zip: &BTreeMap<String, Vec<u8>>) -> String {
    odp_body_geom(zip).0
}

/// Emit one `draw:page` (open consumed) until `</draw:page>`.
///
/// A `draw:frame` carrying a position+size (`svg:x`/`svg:y` + `svg:width`/
/// `svg:height`) is rendered as an absolutely-positioned `<div>` at those page
/// coordinates, preserving the slide layout; its whole subtree is consumed so it
/// is not also flowed. A frame WITHOUT a position is left to the flat-flow path
/// (its inner `text:p`/`draw:image` render in document order), matching the
/// previous behaviour.
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
                    // A positioned frame → absolute box (consumes its subtree).
                    // A frame without a box falls through to the flow grammar.
                    "frame" if !sc && odp_frame_box(&attrs).is_some() => {
                        let bx = odp_frame_box(&attrs).unwrap();
                        odp_frame(x, zip, styles, bx, out);
                    }
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

/// A `draw:frame`'s page box in points: `(x, y, w, h)` from `svg:x`/`svg:y`/
/// `svg:width`/`svg:height` (ODF units, cm/mm/in/pt/px). `None` unless BOTH an
/// origin component and a non-zero size are present (an unpositioned frame).
fn odp_frame_box(attrs: &[(String, String)]) -> Option<(f64, f64, f64, f64)> {
    let w = attr(attrs, "width").and_then(parse_odf_pt)?;
    let h = attr(attrs, "height").and_then(parse_odf_pt)?;
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let x = attr(attrs, "x").and_then(parse_odf_pt)?;
    let y = attr(attrs, "y").and_then(parse_odf_pt)?;
    Some((x, y, w, h))
}

/// The frame box of a `draw:frame` that may be positioned solely through a
/// `draw:transform` (LibreOffice emits rotated frames as `rotate(θ) translate(x y)`
/// and omits `svg:x`/`svg:y`). Returns `(x, y, w, h)` in points: the origin is
/// `svg:x`/`svg:y` when present, else the `translate(…)` component of
/// `draw:transform`; a non-zero `svg:width`/`svg:height` is always required.
fn odp_frame_box_xf(attrs: &[(String, String)]) -> Option<(f64, f64, f64, f64)> {
    if let Some(bx) = odp_frame_box(attrs) {
        return Some(bx);
    }
    let w = attr(attrs, "width").and_then(parse_odf_pt)?;
    let h = attr(attrs, "height").and_then(parse_odf_pt)?;
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let (tx, ty) = attr(attrs, "transform").and_then(odp_transform_translate)?;
    // The transform places the frame's lower-left corner (ODF text origin, Y up);
    // the model box is top-left/Y-down, so the top is `ty - h`.
    Some((tx, ty - h, w, h))
}

/// Parse the `rotate(<angle>)` of a `draw:transform` into the model's CCW
/// [`Rotation`]. ODF angles are **radians, counter-clockwise** (§19.228) — the
/// same orientation as [`crate::model::Rotation`], so we only convert rad→deg and
/// snap the exact quarter turns. No `rotate` (or only translate/scale) ⇒ `D0`.
fn odp_transform_rotation(attrs: &[(String, String)]) -> crate::model::Rotation {
    use crate::model::Rotation;
    let Some(rad) = attr(attrs, "transform").and_then(odp_transform_rotate_rad) else {
        return Rotation::D0;
    };
    let mut deg = rad.to_degrees() % 360.0;
    if deg < 0.0 {
        deg += 360.0;
    }
    if deg.abs() < 1e-6 || (deg - 360.0).abs() < 1e-6 {
        Rotation::D0
    } else if (deg - 90.0).abs() < 1e-6 {
        Rotation::D90
    } else if (deg - 180.0).abs() < 1e-6 {
        Rotation::D180
    } else if (deg - 270.0).abs() < 1e-6 {
        Rotation::D270
    } else {
        Rotation::Deg(deg)
    }
}

/// Extract the `rotate(<rad>)` angle (radians) from a `draw:transform` value list,
/// e.g. `"rotate(0.5235987756) translate(2cm 3cm)"`. `None` if absent/malformed.
fn odp_transform_rotate_rad(transform: &str) -> Option<f64> {
    odp_transform_fn_args(transform, "rotate")?
        .first()
        .and_then(|a| a.trim().parse::<f64>().ok())
}

/// Extract the `translate(<x> <y>)` offset (points) from a `draw:transform`. The
/// two args are ODF lengths separated by whitespace and/or a comma. `None` if no
/// `translate(…)` is present or its components are not parseable lengths.
fn odp_transform_translate(transform: &str) -> Option<(f64, f64)> {
    let args = odp_transform_fn_args(transform, "translate")?;
    let x = args.first().and_then(|a| parse_odf_pt(a))?;
    let y = args.get(1).and_then(|a| parse_odf_pt(a))?;
    Some((x, y))
}

/// Pull the whitespace/comma-separated argument list of the first `name(…)`
/// function in a `draw:transform` value (e.g. `translate` → `["2cm", "3cm"]`).
/// `None` if the named function is not present.
fn odp_transform_fn_args(transform: &str, name: &str) -> Option<Vec<String>> {
    let mut rest = transform;
    while let Some(open) = rest.find('(') {
        let head = rest[..open].trim_start();
        // The function name is the token ending right before the '(' — match it
        // exactly so `rotate` does not also catch a hypothetical `xrotate`.
        let fname = head
            .rsplit(|c: char| c.is_whitespace() || c == ')')
            .next()?;
        let close = rest[open + 1..].find(')')? + open + 1;
        let body = &rest[open + 1..close];
        if fname == name {
            return Some(
                body.split([' ', '\t', '\n', '\r', ','])
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect(),
            );
        }
        rest = &rest[close + 1..];
    }
    None
}

/// Map a `presentation:class` value to the portable [`PlaceholderRole`]. ODF
/// presentation classes (`title`, `subtitle`, `outline`, `text`, `notes`,
/// `page-number`, …) collapse onto the model's first-class roles where they have
/// an equivalent, otherwise [`PlaceholderRole::Other`] keeps the original token.
/// `None` ⇒ the element carried no `presentation:class`.
fn odp_placeholder_role(attrs: &[(String, String)]) -> Option<crate::model::PlaceholderRole> {
    use crate::model::PlaceholderRole;
    let class = attr(attrs, "class")?.trim();
    Some(match class {
        "title" | "ctrTitle" => PlaceholderRole::Title,
        "subtitle" => PlaceholderRole::Subtitle,
        "outline" | "text" | "body" | "subtitle-text" => PlaceholderRole::Body,
        other => PlaceholderRole::Other(other.to_string()),
    })
}

/// Read a `draw:frame`'s accessible alt text: the text of its `svg:title` or
/// `svg:desc` child (title preferred), falling back to the `xlink:title` then
/// `draw:name` attribute. `None` if nothing descriptive is present. Used to fill
/// [`crate::model::ImageRef::alt`] for picture frames.
fn odp_frame_alt(
    title: Option<String>,
    desc: Option<String>,
    attrs: &[(String, String)],
) -> Option<String> {
    let pick = |s: Option<String>| {
        s.filter(|t| !t.trim().is_empty())
            .map(|t| t.trim().to_string())
    };
    pick(title)
        .or_else(|| pick(desc))
        .or_else(|| {
            attr(attrs, "title")
                .map(str::to_string)
                .filter(|t| !t.trim().is_empty())
        })
        .or_else(|| {
            attr(attrs, "name")
                .map(str::to_string)
                .filter(|t| !t.trim().is_empty())
        })
}

/// Render a positioned `draw:frame` (open consumed; `bx` = its page box) as an
/// absolutely-positioned `<div>`. The body is the frame's `draw:text-box`
/// paragraphs and `draw:image`s. Consumes the subtree up to `</draw:frame>`.
fn odp_frame(
    x: &mut Xml,
    zip: &BTreeMap<String, Vec<u8>>,
    styles: &BTreeMap<String, String>,
    bx: (f64, f64, f64, f64),
    out: &mut String,
) {
    let mut body = String::new();
    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => {
                let ln = local(&name);
                match ln {
                    "p" if !sc => {
                        let inner = odf_inline(x, zip, styles, "p");
                        if !inner.trim().is_empty() {
                            body.push_str(&format!("<p>{}</p>", inner.trim()));
                        }
                    }
                    "image" if sc => {
                        if let Some(href) = attr(&attrs, "href") {
                            let key = href.trim_start_matches('/').to_string();
                            if let Some(tag) = img_tag(zip, &key) {
                                body.push_str(&tag);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Tok::Close(name) if local(&name) == "frame" => break,
            _ => {}
        }
    }
    let (fx, fy, fw, fh) = bx;
    out.push_str(&format!(
        "<div style=\"position:absolute;left:{}pt;top:{}pt;width:{}pt;height:{}pt\">{}</div>",
        fmt_pt(fx),
        fmt_pt(fy),
        fmt_pt(fw),
        fmt_pt(fh),
        body,
    ));
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

    // ── document metadata (issue #29) ──

    fn zip_one(name: &str, body: &[u8]) -> BTreeMap<String, Vec<u8>> {
        let mut z = ZipWriter::new();
        z.add_stored(name, body);
        read_zip(&z.finish())
    }

    /// `ooxml_doc_meta` maps the core fields, splits `cp:keywords` on commas,
    /// and reads the extended core properties (description, created/modified,
    /// last-modified-by, revision) plus `Application`/`Company` from `app.xml`;
    /// namespace prefixes are ignored.
    #[test]
    fn ooxml_doc_meta_maps_core_fields() {
        let mut z = ZipWriter::new();
        z.add_stored(
            "docProps/core.xml",
            br#"<cp:coreProperties xmlns:cp="c" xmlns:dc="d" xmlns:dcterms="t">
              <dc:title>Doc Title</dc:title>
              <dc:creator>The Author</dc:creator>
              <dc:subject>The Subject</dc:subject>
              <dc:description>An abstract</dc:description>
              <cp:keywords>one, two , three</cp:keywords>
              <dc:language>de-DE</dc:language>
              <cp:lastModifiedBy>Editor Name</cp:lastModifiedBy>
              <cp:revision>12</cp:revision>
              <dcterms:created>2021-02-03T04:05:06Z</dcterms:created>
              <dcterms:modified>2021-07-08T09:10:11Z</dcterms:modified>
            </cp:coreProperties>"#,
        );
        z.add_stored(
            "docProps/app.xml",
            br#"<Properties xmlns="e"><Application>GigaWord</Application><Company>ACME</Company></Properties>"#,
        );
        let zip = read_zip(&z.finish());
        let m = ooxml_doc_meta(&zip);
        assert_eq!(m.title.as_deref(), Some("Doc Title"));
        assert_eq!(m.author.as_deref(), Some("The Author"));
        assert_eq!(m.subject.as_deref(), Some("The Subject"));
        assert_eq!(m.lang.as_deref(), Some("de-DE"));
        assert_eq!(m.keywords, vec!["one", "two", "three"]);
        assert_eq!(m.description, "An abstract");
        assert_eq!(m.last_modified_by, "Editor Name");
        assert_eq!(m.revision, "12");
        assert_eq!(m.created, "2021-02-03T04:05:06Z");
        assert_eq!(m.modified, "2021-07-08T09:10:11Z");
        assert_eq!(m.application, "GigaWord");
        assert_eq!(m.company, "ACME");
    }

    /// Semicolon is also accepted as a `cp:keywords` delimiter, and blank /
    /// whitespace-only fields never produce a value.
    #[test]
    fn ooxml_doc_meta_semicolons_and_blanks() {
        let zip = zip_one(
            "docProps/core.xml",
            br#"<cp:coreProperties xmlns:cp="c" xmlns:dc="d">
              <dc:title>  </dc:title>
              <cp:keywords>a; b ;; c</cp:keywords>
            </cp:coreProperties>"#,
        );
        let m = ooxml_doc_meta(&zip);
        assert_eq!(m.title, None, "whitespace-only title stays unset");
        assert_eq!(m.keywords, vec!["a", "b", "c"]);
        assert_eq!(m.author, None);
    }

    /// A missing `docProps/core.xml` ⇒ a default `DocMeta` (no panic).
    #[test]
    fn ooxml_doc_meta_absent_part_is_default() {
        let zip = zip_one("word/document.xml", b"<w:document/>");
        assert_eq!(ooxml_doc_meta(&zip), DocMeta::default());
    }

    /// `odf_doc_meta` maps the Dublin-Core fields, collects each repeated
    /// `meta:keyword` element (de-duplicating and skipping blanks), and reads
    /// the extended properties: `meta:generator`→generator,
    /// `meta:creation-date`→created, `dc:date`→modified, `dc:description`→
    /// description, `meta:editing-cycles`→editing_cycles.
    #[test]
    fn odf_doc_meta_maps_fields_and_keywords() {
        let zip = zip_one(
            "meta.xml",
            br#"<office:document-meta xmlns:office="o" xmlns:meta="m" xmlns:dc="d">
              <office:meta>
                <meta:generator>Writer/1.0</meta:generator>
                <dc:title>ODF Title</dc:title>
                <dc:creator>ODF Author</dc:creator>
                <dc:subject>ODF Subject</dc:subject>
                <dc:description>ODF abstract</dc:description>
                <dc:language>es</dc:language>
                <meta:creation-date>2019-12-31T23:59:59</meta:creation-date>
                <dc:date>2020-05-05T05:05:05</dc:date>
                <meta:editing-cycles>3</meta:editing-cycles>
                <meta:keyword>kw1</meta:keyword>
                <meta:keyword> </meta:keyword>
                <meta:keyword>kw2</meta:keyword>
                <meta:keyword>kw1</meta:keyword>
              </office:meta>
            </office:document-meta>"#,
        );
        let m = odf_doc_meta(&zip);
        assert_eq!(m.title.as_deref(), Some("ODF Title"));
        assert_eq!(m.author.as_deref(), Some("ODF Author"));
        assert_eq!(m.subject.as_deref(), Some("ODF Subject"));
        assert_eq!(m.lang.as_deref(), Some("es"));
        assert_eq!(m.keywords, vec!["kw1", "kw2"], "blanks skipped, deduped");
        assert_eq!(m.description, "ODF abstract");
        assert_eq!(m.generator, "Writer/1.0");
        assert_eq!(m.created, "2019-12-31T23:59:59");
        assert_eq!(m.modified, "2020-05-05T05:05:05");
        assert_eq!(m.editing_cycles, "3");
    }

    /// A missing `meta.xml` ⇒ a default `DocMeta` (no panic).
    #[test]
    fn odf_doc_meta_absent_part_is_default() {
        let zip = zip_one("content.xml", b"<office:document-content/>");
        assert_eq!(odf_doc_meta(&zip), DocMeta::default());
    }

    #[test]
    fn split_keywords_trims_and_drops_empties() {
        assert_eq!(split_keywords("a, b;c"), vec!["a", "b", "c"]);
        assert_eq!(split_keywords("  solo  "), vec!["solo"]);
        assert!(split_keywords("  ,; ").is_empty());
        assert!(split_keywords("").is_empty());
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

    #[test]
    fn docx_paragraph_border_and_shading_emit_box() {
        // A framed, shaded paragraph (the "encadré"): `w:pBdr` per-side borders +
        // `w:shd` fill. `w:sz` is eighths of a point (24 → 3pt; 8 → 1pt).
        let doc = r#"<w:document xmlns:w="x">
  <w:body>
    <w:p>
      <w:pPr>
        <w:pBdr>
          <w:top w:val="single" w:sz="24" w:color="FF0000"/>
          <w:left w:val="single" w:sz="8" w:color="00FF00"/>
          <w:bottom w:val="dashed" w:sz="16" w:color="0000FF"/>
          <w:right w:val="single" w:sz="8"/>
        </w:pBdr>
        <w:shd w:val="clear" w:fill="FFFF00"/>
      </w:pPr>
      <w:r><w:t>Boxed note</w:t></w:r>
    </w:p>
  </w:body>
</w:document>"#;
        let bytes = build_docx(doc, None, &[]);

        // The intermediate HTML carries the per-side borders + the fill on the <p>.
        let zip = crate::convert::zip::read_zip(&bytes);
        let html = docx_body_html(&zip);
        assert!(
            html.contains("border-top:3pt solid #FF0000"),
            "top border missing: {html}"
        );
        assert!(
            html.contains("border-left:1pt solid #00FF00"),
            "left border missing: {html}"
        );
        assert!(
            html.contains("border-bottom:2pt dashed #0000FF"),
            "bottom border missing: {html}"
        );
        // Colourless side: width + style, no `#…`.
        assert!(
            html.contains("border-right:1pt solid;"),
            "right border missing: {html}"
        );
        assert!(
            html.contains("background-color:#FFFF00"),
            "shading missing: {html}"
        );
        assert!(html.contains("padding:2pt"), "inset missing: {html}");

        // And the document still renders (the engine draws the frame + fill).
        let pdf = office_to_pdf(&bytes).expect("docx converts");
        let document = opens(&pdf);
        assert!(document.page_count() >= 1);
        assert!(norm(&document.to_text()).contains("Boxed note"));
    }

    #[test]
    fn docx_inline_image_sized_from_extent() {
        // An inline drawing with `wp:extent` (EMU): 914400 = 72pt, 457200 = 36pt.
        // The emitted <img> must carry those as width/height (the engine reads an
        // inline image's box from the attributes, not the bitmap's native size).
        let doc = r#"<w:document xmlns:w="x" xmlns:wp="w" xmlns:a="y" xmlns:r="z">
  <w:body>
    <w:p><w:r><w:drawing>
      <wp:inline>
        <wp:extent cx="914400" cy="457200"/>
        <a:graphic><a:graphicData><pic:pic xmlns:pic="p">
          <pic:blipFill><a:blip r:embed="rId7"/></pic:blipFill>
        </pic:pic></a:graphicData></a:graphic>
      </wp:inline>
    </w:drawing></w:r></w:p>
  </w:body>
</w:document>"#;
        let rels = r#"<Relationships xmlns="x">
  <Relationship Id="rId7" Type="image" Target="media/logo.png"/>
</Relationships>"#;
        let bytes = build_docx(doc, Some(rels), &[("word/media/logo.png", red_png())]);

        let zip = crate::convert::zip::read_zip(&bytes);
        let html = docx_body_html(&zip);
        assert!(
            html.contains("<img src=\"data:image/png;base64,"),
            "image missing: {html}"
        );
        assert!(
            html.contains("width=\"72\""),
            "extent width not applied: {html}"
        );
        assert!(
            html.contains("height=\"36\""),
            "extent height not applied: {html}"
        );

        let pdf = office_to_pdf(&bytes).expect("docx converts");
        assert!(opens(&pdf).page_count() >= 1);
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

    /// Drive `ods_table` over a single `<table:table>` fragment and return the
    /// emitted HTML, so cell/row styling can be asserted on the markup directly.
    fn ods_table_html(content: &str) -> String {
        let cell_styles = odf_cell_styles(content);
        let row_heights = odf_row_heights(content);
        let cols = odf_column_widths(content);
        let mut x = Xml::new(content);
        let mut out = String::new();
        while let Some(tok) = x.next() {
            if let Tok::Open(name, _, sc) = &tok {
                if local(name) == "table" && !sc {
                    ods_table(&mut x, &cols, &cell_styles, &row_heights, &mut out);
                    break;
                }
            }
        }
        out
    }

    #[test]
    fn ods_cell_style_font_border_background_apply_to_td() {
        // `ce1` = bold + red + Arial 12 + thin black border + yellow background +
        // middle vertical-align. The cell that references it must carry them all.
        let content = r##"<office:document-content xmlns:office="o" xmlns:table="tb" xmlns:text="t" xmlns:style="st" xmlns:fo="fo">
  <office:automatic-styles>
    <style:style style:name="ce1" style:family="table-cell">
      <style:table-cell-properties fo:border="0.5pt solid #000000" fo:background-color="#FFFF00" style:vertical-align="middle"/>
      <style:text-properties fo:font-weight="bold" fo:color="#FF0000" fo:font-size="12pt" style:font-name="Arial"/>
    </style:style>
  </office:automatic-styles>
  <office:body><office:spreadsheet>
    <table:table table:name="Sheet1">
      <table:table-row>
        <table:table-cell table:style-name="ce1"><text:p>Styled</text:p></table:table-cell>
        <table:table-cell><text:p>Plain</text:p></table:table-cell>
      </table:table-row>
    </table:table>
  </office:spreadsheet></office:body>
</office:document-content>"##;

        // Parse-level: ce1 collapses text + cell properties into one CSS blob.
        let styles = odf_cell_styles(content);
        let css = styles.get("ce1").map(String::as_str).unwrap_or("");
        assert!(css.contains("font-weight:bold;"), "bold: {css}");
        assert!(css.contains("color:#FF0000;"), "red: {css}");
        assert!(css.contains("font-size:12pt;"), "size: {css}");
        assert!(css.contains("font-family:Arial;"), "family: {css}");
        assert!(css.contains("border:0.5pt solid #000000;"), "border: {css}");
        assert!(css.contains("background-color:#FFFF00;"), "bg: {css}");
        assert!(css.contains("vertical-align:middle;"), "v-align: {css}");

        // Render-level: the styled <td> carries them; the plain one is styleless.
        let table = ods_table_html(content);
        assert!(table.contains("font-weight:bold"), "td bold: {table}");
        assert!(table.contains("color:#FF0000"), "td red: {table}");
        assert!(
            table.contains("border:0.5pt solid #000000"),
            "td border: {table}"
        );
        assert!(table.contains("background-color:#FFFF00"), "td bg: {table}");
        assert!(table.contains("<td>Plain</td>"), "plain unstyled: {table}");

        // End-to-end PDF still carries the text.
        let mut z = ZipWriter::new();
        z.add_stored(
            "mimetype",
            b"application/vnd.oasis.opendocument.spreadsheet",
        );
        z.add_stored("content.xml", content.as_bytes());
        let pdf = office_to_pdf(&z.finish()).expect("ods converts");
        let text = norm(&opens(&pdf).to_text());
        assert!(text.contains("Styled") && text.contains("Plain"), "{text}");
    }

    #[test]
    fn ods_column_default_cell_style_and_row_height_apply() {
        // A column-level default cell style (bold) applies to cells without their
        // own style; the row style sets a height on the <tr>.
        let content = r#"<office:document-content xmlns:office="o" xmlns:table="tb" xmlns:text="t" xmlns:style="st" xmlns:fo="fo">
  <office:automatic-styles>
    <style:style style:name="ceBold" style:family="table-cell">
      <style:text-properties fo:font-weight="bold"/>
    </style:style>
    <style:style style:name="roTall" style:family="table-row">
      <style:table-row-properties style:row-height="24pt"/>
    </style:style>
  </office:automatic-styles>
  <office:body><office:spreadsheet>
    <table:table table:name="Sheet1">
      <table:table-column table:default-cell-style-name="ceBold"/>
      <table:table-column/>
      <table:table-row table:style-name="roTall">
        <table:table-cell><text:p>InheritBold</text:p></table:table-cell>
        <table:table-cell><text:p>Normal</text:p></table:table-cell>
      </table:table-row>
    </table:table>
  </office:spreadsheet></office:body>
</office:document-content>"#;

        let table = ods_table_html(content);
        // First column's default style (bold) reaches the first cell.
        assert!(
            table.contains("<td style=\"font-weight:bold\">InheritBold</td>"),
            "column default applied: {table}"
        );
        // Second column has no default → its cell is styleless.
        assert!(table.contains("<td>Normal</td>"), "no default: {table}");
        // The row style's height lands on the <tr>.
        assert!(
            table.contains("<tr style=\"height:24pt\">"),
            "row height: {table}"
        );
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

    // ── PPTX absolute positioning (wave 1) ──

    /// Render one slide's HTML body directly (no theme, empty zip/rels) to assert
    /// on the generated markup — the absolute coordinates we care about.
    fn slide_html(xml: &str) -> String {
        let zip = BTreeMap::new();
        let rels = BTreeMap::new();
        let theme = PptxTheme::default();
        let mut out = String::new();
        pptx_slide(xml, &zip, &rels, &theme, &mut out);
        out
    }

    #[test]
    fn pptx_shape_with_xfrm_is_absolutely_positioned() {
        // a:off x=914400 EMU (72pt), y=457200 (36pt); a:ext cx=1828800 (144pt),
        // cy=914400 (72pt). 1pt = 12700 EMU.
        let xml = r#"<p:sld xmlns:a="a" xmlns:p="p">
          <p:cSld><p:spTree>
            <p:sp>
              <p:spPr><a:xfrm><a:off x="914400" y="457200"/><a:ext cx="1828800" cy="914400"/></a:xfrm></p:spPr>
              <p:txBody><a:p><a:r><a:t>Positioned Box</a:t></a:r></a:p></p:txBody>
            </p:sp>
          </p:spTree></p:cSld>
        </p:sld>"#;
        let html = slide_html(xml);
        assert!(html.contains("position:absolute"), "absolute: {html}");
        assert!(html.contains("left:72pt"), "left 72pt: {html}");
        assert!(html.contains("top:36pt"), "top 36pt: {html}");
        assert!(html.contains("width:144pt"), "width 144pt: {html}");
        assert!(html.contains("height:72pt"), "height 72pt: {html}");
        assert!(html.contains("Positioned Box"), "text kept: {html}");
    }

    #[test]
    fn pptx_two_shapes_keep_distinct_positions_not_stacked() {
        let xml = r#"<p:sld xmlns:a="a" xmlns:p="p"><p:cSld><p:spTree>
            <p:sp><p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="1270000" cy="635000"/></a:xfrm></p:spPr>
              <p:txBody><a:p><a:r><a:t>Top Left</a:t></a:r></a:p></p:txBody></p:sp>
            <p:sp><p:spPr><a:xfrm><a:off x="3810000" y="2540000"/><a:ext cx="1270000" cy="635000"/></a:xfrm></p:spPr>
              <p:txBody><a:p><a:r><a:t>Lower Right</a:t></a:r></a:p></p:txBody></p:sp>
          </p:spTree></p:cSld></p:sld>"#;
        let html = slide_html(xml);
        // Box 1 at (0,0); box 2 at (300pt,200pt) — both present, not stacked.
        assert!(html.contains("left:0pt;top:0pt"), "box1 origin: {html}");
        assert!(html.contains("left:300pt;top:200pt"), "box2 offset: {html}");
        // Two distinct absolute wrappers.
        assert_eq!(
            html.matches("position:absolute").count(),
            2,
            "two positioned shapes: {html}"
        );
        assert!(html.contains("Top Left") && html.contains("Lower Right"));
    }

    #[test]
    fn pptx_shape_rotation_and_flip_emit_transform() {
        // rot=5400000 (60000ths) = 90deg; flipH=1.
        let xml = r#"<p:sld xmlns:a="a" xmlns:p="p"><p:cSld><p:spTree>
            <p:sp><p:spPr><a:xfrm rot="5400000" flipH="1"><a:off x="127000" y="127000"/><a:ext cx="635000" cy="635000"/></a:xfrm></p:spPr>
              <p:txBody><a:p><a:r><a:t>Rotated</a:t></a:r></a:p></p:txBody></p:sp>
          </p:spTree></p:cSld></p:sld>"#;
        let html = slide_html(xml);
        assert!(html.contains("transform:rotate(90deg)"), "rotate: {html}");
        assert!(html.contains("scaleX(-1)"), "flipH: {html}");
        assert!(html.contains("transform-origin:center"), "origin: {html}");
    }

    #[test]
    fn pptx_shape_without_xfrm_stays_in_flow() {
        let xml = r#"<p:sld xmlns:a="a" xmlns:p="p"><p:cSld><p:spTree>
            <p:sp><p:spPr></p:spPr>
              <p:txBody><a:p><a:r><a:t>Flowing Text</a:t></a:r></a:p></p:txBody></p:sp>
          </p:spTree></p:cSld></p:sld>"#;
        let html = slide_html(xml);
        assert!(
            !html.contains("position:absolute"),
            "no-xfrm shape must flow, not absolute: {html}"
        );
        assert!(html.contains("Flowing Text"), "text kept: {html}");
        assert!(html.contains("<p>"), "flowed as paragraph: {html}");
    }

    #[test]
    fn pptx_scheme_colour_resolves_through_theme() {
        let theme_xml = r#"<a:theme xmlns:a="a"><a:themeElements><a:clrScheme name="Office">
            <a:dk1><a:srgbClr val="000000"/></a:dk1>
            <a:lt1><a:srgbClr val="FFFFFF"/></a:lt1>
            <a:accent1><a:srgbClr val="4472C4"/></a:accent1>
          </a:clrScheme></a:themeElements></a:theme>"#;
        let theme = parse_pptx_theme(theme_xml);
        assert_eq!(theme.resolve_scheme("accent1").as_deref(), Some("4472C4"));
        // clrMap alias bg1 → lt1 (white).
        assert_eq!(theme.resolve_scheme("bg1").as_deref(), Some("FFFFFF"));

        let xml = r#"<p:sld xmlns:a="a" xmlns:p="p"><p:cSld><p:spTree>
            <p:sp><p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="635000" cy="635000"/></a:xfrm></p:spPr>
              <p:txBody><a:p><a:r><a:rPr><a:solidFill><a:schemeClr val="accent1"/></a:solidFill></a:rPr><a:t>Themed</a:t></a:r></a:p></p:txBody></p:sp>
          </p:spTree></p:cSld></p:sld>"#;
        let zip = BTreeMap::new();
        let rels = BTreeMap::new();
        let mut out = String::new();
        pptx_slide(xml, &zip, &rels, &theme, &mut out);
        assert!(out.contains("color:#4472C4"), "schemeClr → colour: {out}");
    }

    #[test]
    fn pptx_solid_background_fill_becomes_backdrop() {
        let xml = r#"<p:sld xmlns:a="a" xmlns:p="p"><p:cSld>
            <p:bg><p:bgPr><a:solidFill><a:srgbClr val="203864"/></a:solidFill></p:bgPr></p:bg>
            <p:spTree>
              <p:sp><p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="635000" cy="635000"/></a:xfrm></p:spPr>
                <p:txBody><a:p><a:r><a:t>On Dark</a:t></a:r></a:p></p:txBody></p:sp>
            </p:spTree></p:cSld></p:sld>"#;
        let html = slide_html(xml);
        assert!(html.contains("background:#203864"), "bg fill: {html}");
        // Backdrop is a full-slide absolute div at the origin.
        assert!(
            html.contains("left:0pt;top:0pt;width:") && html.contains("background:#203864"),
            "full-slide backdrop: {html}"
        );
        assert!(html.contains("On Dark"), "content over backdrop: {html}");
    }

    #[test]
    fn pptx_graphic_frame_table_is_positioned() {
        // A table inside a graphicFrame carries its p:xfrm; the whole table must
        // be wrapped absolutely.
        let xml = r#"<p:sld xmlns:a="a" xmlns:p="p"><p:cSld><p:spTree>
            <p:graphicFrame>
              <p:xfrm><a:off x="635000" y="1270000"/><a:ext cx="2540000" cy="1270000"/></p:xfrm>
              <a:graphic><a:graphicData>
                <a:tbl><a:tblGrid><a:gridCol w="1270000"/><a:gridCol w="1270000"/></a:tblGrid>
                  <a:tr><a:tc><a:txBody><a:p><a:r><a:t>R1C1</a:t></a:r></a:p></a:txBody></a:tc>
                  <a:tc><a:txBody><a:p><a:r><a:t>R1C2</a:t></a:r></a:p></a:txBody></a:tc></a:tr>
                </a:tbl>
              </a:graphicData></a:graphic>
            </p:graphicFrame>
          </p:spTree></p:cSld></p:sld>"#;
        let html = slide_html(xml);
        assert!(html.contains("position:absolute"), "frame absolute: {html}");
        assert!(html.contains("left:50pt;top:100pt"), "frame coords: {html}");
        assert!(html.contains("<table>"), "renders a table: {html}");
        assert!(
            html.contains("R1C1") && html.contains("R1C2"),
            "cells: {html}"
        );
        // The table markup is inside the absolute wrapper.
        let abs = html.find("position:absolute").unwrap();
        let tbl = html.find("<table>").unwrap();
        assert!(abs < tbl, "table nested in absolute div: {html}");
    }

    #[test]
    fn parse_xfrm_reads_offset_extent_rotation_flip() {
        let mut x = Xml::new(
            r#"<a:xfrm rot="2700000" flipV="1"><a:off x="254000" y="127000"/><a:ext cx="508000" cy="254000"/></a:xfrm>"#,
        );
        // Advance to the a:xfrm open token, then hand off to parse_xfrm.
        let attrs = loop {
            match x.next() {
                Some(Tok::Open(name, attrs, _)) if local(&name) == "xfrm" => break attrs,
                Some(_) => continue,
                None => panic!("no xfrm open"),
            }
        };
        let b = parse_xfrm(&mut x, &attrs);
        assert!((b.x - 20.0).abs() < 1e-9, "x=20pt: {}", b.x);
        assert!((b.y - 10.0).abs() < 1e-9, "y=10pt: {}", b.y);
        assert!((b.w - 40.0).abs() < 1e-9, "w=40pt: {}", b.w);
        assert!((b.h - 20.0).abs() < 1e-9, "h=20pt: {}", b.h);
        assert!((b.rot_deg - 45.0).abs() < 1e-9, "rot=45deg: {}", b.rot_deg);
        assert!(b.flip_v && !b.flip_h, "flipV only");
        assert!(b.is_placed());
    }

    // ── ODP absolute positioning (wave 1) ──

    /// Render one `draw:page`'s HTML body directly to assert on positioning.
    fn odp_page_html(page_xml: &str) -> String {
        let zip = BTreeMap::new();
        let styles = BTreeMap::new();
        let mut x = Xml::new(page_xml);
        // Advance into the <draw:page> open tag.
        loop {
            match x.next() {
                Some(Tok::Open(name, _, sc)) if local(&name) == "page" && !sc => break,
                Some(_) => continue,
                None => panic!("no draw:page open"),
            }
        }
        let mut out = String::new();
        odp_page(&mut x, &zip, &styles, &mut out);
        out
    }

    #[test]
    fn odp_positioned_frame_is_absolute() {
        // svg:x=2cm (≈56.69pt), y=1cm (≈28.35pt), width=8cm, height=3cm.
        let page = r#"<draw:page xmlns:draw="d" xmlns:svg="s" xmlns:text="t">
            <draw:frame svg:x="2cm" svg:y="1cm" svg:width="8cm" svg:height="3cm">
              <draw:text-box><text:p>Placed Frame</text:p></draw:text-box>
            </draw:frame>
          </draw:page>"#;
        let html = odp_page_html(page);
        assert!(html.contains("position:absolute"), "absolute: {html}");
        assert!(html.contains("left:56.69pt"), "x≈56.69pt: {html}");
        assert!(html.contains("top:28.35pt"), "y≈28.35pt: {html}");
        assert!(html.contains("width:226.77pt"), "w 8cm: {html}");
        assert!(html.contains("Placed Frame"), "text kept: {html}");
    }

    #[test]
    fn odp_two_positioned_frames_not_stacked() {
        let page = r#"<draw:page xmlns:draw="d" xmlns:svg="s" xmlns:text="t">
            <draw:frame svg:x="1cm" svg:y="1cm" svg:width="4cm" svg:height="2cm">
              <draw:text-box><text:p>Frame A</text:p></draw:text-box></draw:frame>
            <draw:frame svg:x="10cm" svg:y="6cm" svg:width="4cm" svg:height="2cm">
              <draw:text-box><text:p>Frame B</text:p></draw:text-box></draw:frame>
          </draw:page>"#;
        let html = odp_page_html(page);
        assert_eq!(
            html.matches("position:absolute").count(),
            2,
            "two positioned frames: {html}"
        );
        assert!(html.contains("left:28.35pt"), "frame A x=1cm: {html}");
        assert!(html.contains("left:283.46pt"), "frame B x=10cm: {html}");
        assert!(html.contains("Frame A") && html.contains("Frame B"));
    }

    #[test]
    fn odp_unpositioned_frame_stays_in_flow() {
        let page = r#"<draw:page xmlns:draw="d" xmlns:text="t">
            <draw:frame><draw:text-box><text:p>Flowing Frame</text:p></draw:text-box></draw:frame>
          </draw:page>"#;
        let html = odp_page_html(page);
        assert!(
            !html.contains("position:absolute"),
            "no-position frame must flow: {html}"
        );
        assert!(html.contains("Flowing Frame"), "text kept: {html}");
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
            page_h: A4_H,
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

    // ── embedded-font extraction ──

    /// A real, parseable font program for fixtures: the bundled Liberation Sans
    /// (already in the crate). We embed it under a *different* family name so we
    /// can prove the renderer used the extracted face, not its own fallback.
    fn fixture_ttf() -> Vec<u8> {
        crate::font::bundled::FALLBACK_TTF.to_vec()
    }

    #[test]
    fn parse_guid_reads_32_hex_digits() {
        let g = parse_guid("{01234567-89AB-CDEF-0011-223344556677}").expect("valid GUID");
        assert_eq!(g[0], 0x01, "first hex pair is byte 0 (string order)");
        assert_eq!(g[15], 0x77, "last hex pair is byte 15");
        // Anything other than exactly 32 hex digits is rejected.
        assert!(parse_guid("not-a-guid").is_none());
        assert!(parse_guid("{0123}").is_none());
    }

    #[test]
    fn deobfuscate_odttf_is_its_own_inverse() {
        // ECMA-376 obfuscation is a 32-byte XOR with the reversed-GUID key, so
        // applying it twice with the same key restores the original — the same
        // routine de-obfuscates and (in reverse) would obfuscate.
        let guid = parse_guid("{DEADBEEF-1234-5678-9ABC-DEF012345678}").unwrap();
        let original = fixture_ttf();
        // Obfuscate (== deobfuscate, the XOR is symmetric), then de-obfuscate.
        let obfuscated = deobfuscate_odttf(&original, &guid);
        let restored = deobfuscate_odttf(&obfuscated, &guid);
        assert_eq!(restored, original, "XOR round-trips to the original bytes");
        // The first 32 bytes actually changed (the sfnt header is scrambled).
        assert_ne!(
            &obfuscated[..32],
            &original[..32],
            "the header is obfuscated"
        );
        assert_eq!(
            &obfuscated[32..],
            &original[32..],
            "only the first 32 bytes are touched"
        );
    }

    /// Build a DOCX that embeds an (obfuscated) font, referenced from a
    /// `fontTable.xml`, under `family` with the given GUID. The font part is the
    /// real `fixture_ttf()` obfuscated with `guid` (so de-obfuscation yields a
    /// parseable program).
    fn build_docx_with_embedded_font(family: &str, guid: &str) -> Vec<u8> {
        let g = parse_guid(guid).expect("test GUID parses");
        let obfuscated = deobfuscate_odttf(&fixture_ttf(), &g); // XOR == obfuscate
        let document = format!(
            r#"<w:document xmlns:w="x"><w:body>
                 <w:p><w:r><w:rPr><w:rFonts w:ascii="{family}"/></w:rPr><w:t>Embedded Sample</w:t></w:r></w:p>
               </w:body></w:document>"#
        );
        let font_table = format!(
            r#"<w:fonts xmlns:w="x" xmlns:r="y">
                 <w:font w:name="{family}">
                   <w:embedRegular r:id="rId1" w:fontKey="{guid}"/>
                 </w:font>
               </w:fonts>"#
        );
        let font_rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
            <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/font" Target="fonts/font1.odttf"/>
          </Relationships>"#;
        build_docx(
            &document,
            None,
            &[
                ("word/fontTable.xml", font_table.into_bytes()),
                (
                    "word/_rels/fontTable.xml.rels",
                    font_rels.as_bytes().to_vec(),
                ),
                ("word/fonts/font1.odttf", obfuscated),
            ],
        )
    }

    #[test]
    fn docx_embedded_font_is_deobfuscated_and_provided_to_renderer() {
        // A DOCX that ships its own "Calibri" face: extraction must return the
        // de-obfuscated, parseable program under that exact family — so the
        // renderer lays the run out and paints it with the document's own font
        // rather than the bundled Liberation fallback.
        let guid = "{12345678-9ABC-DEF0-1122-334455667788}";
        let docx = build_docx_with_embedded_font("Calibri", guid);
        let zip = read_zip(&docx);

        let fonts = extract_embedded_fonts(&zip);
        assert_eq!(
            fonts.len(),
            1,
            "one embedded face extracted: {:?}",
            fonts.len()
        );
        let f = &fonts[0];
        assert_eq!(
            f.family, "Calibri",
            "raw family name preserved (matches HTML)"
        );
        assert_eq!(f.weight, 400);
        assert!(!f.italic);
        // The bytes are a real, parseable font program (de-obfuscation worked).
        assert!(
            crate::font::truetype::TrueTypeFont::parse(&f.ttf).is_some(),
            "the extracted, de-obfuscated bytes parse as a TrueType program"
        );
        // And they equal the original fixture (round-tripped through obfuscation).
        assert_eq!(
            f.ttf,
            fixture_ttf(),
            "extracted face == the embedded original"
        );

        // End-to-end: the rendered PDF embeds the document's OWN face. The run's
        // `font-family` is "Calibri", so the painter must use the extracted
        // Calibri face — proven by a `/BaseFont …Calibri…` font program in the
        // output (a subset prefix like `ABCDEF+Calibri` is normal). If the
        // extraction had failed, the run would render with the bundled Liberation
        // fallback and no "Calibri" font would appear.
        let pdf = docx_to_pdf(&zip);
        assert!(pdf.starts_with(b"%PDF-"), "valid PDF produced");
        let out = crate::Document::open(&pdf).expect("re-open rendered PDF");
        let fonts = out.embedded_fonts();
        assert!(
            fonts
                .iter()
                .any(|f| f.base_font.to_ascii_lowercase().contains("calibri")),
            "the document's own Calibri face is embedded in the output: {fonts:?}"
        );
    }

    #[test]
    fn office_needed_fonts_lists_referenced_but_unembedded_family() {
        // A DOCX that *references* Calibri without embedding it: the host must be
        // told to fetch Calibri (so the run lays out with the right metrics). A
        // base-14 family (Arial) would be excluded — the bundled substitute draws
        // it natively — so use a non-base-14 family the catalogue knows.
        let doc = r#"<w:document xmlns:w="x"><w:body>
            <w:p><w:r><w:rPr><w:rFonts w:ascii="Roboto"/></w:rPr><w:t>Hello Roboto</w:t></w:r></w:p>
          </w:body></w:document>"#;
        let docx = build_docx(doc, None, &[]);
        let reqs = office_needed_fonts(&docx).expect("recognized DOCX");
        assert!(
            reqs.iter().any(|r| r.family.eq_ignore_ascii_case("Roboto")),
            "referenced Roboto is requested for the host to fetch: {reqs:?}"
        );
    }

    #[test]
    fn office_needed_fonts_excludes_self_embedded_family() {
        // When the container embeds the very family it references, the host must
        // NOT be asked to fetch it — the embedded bytes already render it. Use a
        // catalogue family (not base-14) so the only reason it's excluded is the
        // embedding.
        let guid = "{AABBCCDD-1122-3344-5566-77889900AABB}";
        let docx = build_docx_with_embedded_font("Roboto", guid);
        let reqs = office_needed_fonts(&docx).expect("recognized DOCX");
        assert!(
            !reqs.iter().any(|r| r.family.eq_ignore_ascii_case("Roboto")),
            "self-embedded Roboto is NOT in the host fetch list: {reqs:?}"
        );
    }

    #[test]
    fn office_to_pdf_with_fonts_uses_host_face_for_referenced_unembedded_family() {
        // A DOCX that *references* "Calibri" but does not embed it. Phase 1
        // (`office_needed_fonts`) would tell the host to fetch it; phase 2
        // (`office_to_pdf_with_fonts`) hands the fetched face back so the run is
        // laid out and painted with it (a Carlito-like metric-compatible
        // substitute) instead of drifting onto the bundled Liberation fallback.
        let doc = r#"<w:document xmlns:w="x"><w:body>
            <w:p><w:r><w:rPr><w:rFonts w:ascii="Calibri"/></w:rPr><w:t>Hello Calibri</w:t></w:r></w:p>
          </w:body></w:document>"#;
        let docx = build_docx(doc, None, &[]);

        // Sanity: the DOCX embeds nothing, so without a host face the renderer
        // falls back to the bundled "Fallback Sans" — no "Calibri" in the output.
        let baseline = office_to_pdf(&docx).expect("recognized DOCX");
        let base_doc = crate::Document::open(&baseline).expect("re-open baseline PDF");
        assert!(
            !base_doc
                .embedded_fonts()
                .iter()
                .any(|f| f.base_font.to_ascii_lowercase().contains("calibri")),
            "without a host face, the Calibri run uses the bundled fallback (no Calibri embedded): {:?}",
            base_doc.embedded_fonts()
        );

        // Phase 2: supply a "Calibri" face (here the bundled program reused under
        // that family name, standing in for a host-fetched Carlito). The renderer
        // embeds it under "Calibri" — proving the FOURNIE face was consulted, not
        // the Liberation fallback (which embeds as "Fallback Sans").
        let host = vec![ProvidedFont {
            family: "Calibri".to_string(),
            weight: 400,
            italic: false,
            ttf: fixture_ttf(),
        }];
        let pdf = office_to_pdf_with_fonts(&docx, &host).expect("recognized DOCX");
        assert!(pdf.starts_with(b"%PDF-"), "valid PDF produced");
        let out = crate::Document::open(&pdf).expect("re-open rendered PDF");
        let fonts = out.embedded_fonts();
        assert!(
            fonts
                .iter()
                .any(|f| f.base_font.to_ascii_lowercase().contains("calibri")),
            "the host-supplied Calibri face is embedded in the output: {fonts:?}"
        );
        assert!(
            !fonts
                .iter()
                .any(|f| f.base_font.contains("Fallback Sans")),
            "the bundled fallback is NOT used for the Calibri run when the host face is supplied: {fonts:?}"
        );

        // `office_to_pdf` (no host fonts) must remain unchanged — same output as
        // the baseline (the public no-fonts API is not regressed by phase 2).
        assert_eq!(
            office_to_pdf(&docx).expect("recognized DOCX"),
            baseline,
            "office_to_pdf without host fonts is byte-identical (no regression)"
        );
    }

    #[test]
    fn merge_fonts_keeps_embedded_over_host_and_appends_gaps() {
        let prog = fixture_ttf();
        let embedded = vec![ProvidedFont {
            family: "Calibri".to_string(),
            weight: 400,
            italic: false,
            ttf: prog.clone(),
        }];
        let host = vec![
            // Same exact key as the embedded face (case-insensitive) → dropped:
            // the embedded face wins on conflict.
            ProvidedFont {
                family: "calibri".to_string(),
                weight: 400,
                italic: false,
                ttf: vec![9, 9, 9],
            },
            // A family the container does NOT embed → appended (fills the gap).
            ProvidedFont {
                family: "Cambria".to_string(),
                weight: 400,
                italic: false,
                ttf: prog.clone(),
            },
            // Same family, different weight (bold ≥ 600) → distinct key, kept.
            ProvidedFont {
                family: "Calibri".to_string(),
                weight: 700,
                italic: false,
                ttf: prog.clone(),
            },
        ];

        let merged = merge_fonts(embedded, &host);
        // Embedded Calibri/regular first (priority), then the two non-colliding
        // host faces; the colliding host Calibri/regular is dropped.
        assert_eq!(merged.len(), 3, "one collision dropped: {:?}", merged.len());
        assert_eq!(
            merged[0].family, "Calibri",
            "embedded face stays first (wins)"
        );
        assert_eq!(
            merged[0].ttf, prog,
            "embedded bytes preserved, not the host's"
        );
        assert!(
            merged.iter().any(|f| f.family == "Cambria"),
            "a referenced-but-unembedded host family is appended"
        );
        assert!(
            merged
                .iter()
                .any(|f| f.family == "Calibri" && f.weight == 700),
            "a host face with a different weight key is appended"
        );
        assert!(
            !merged.iter().any(|f| f.ttf == vec![9, 9, 9]),
            "the colliding host Calibri/regular is dropped (embedded won)"
        );
    }

    #[test]
    fn merge_fonts_empty_host_is_identity() {
        let embedded = vec![ProvidedFont {
            family: "Roboto".to_string(),
            weight: 400,
            italic: false,
            ttf: fixture_ttf(),
        }];
        let merged = merge_fonts(embedded.clone(), &[]);
        assert_eq!(merged.len(), embedded.len());
        assert_eq!(merged[0].family, "Roboto");
        assert_eq!(merged[0].ttf, embedded[0].ttf);
    }

    #[test]
    fn odf_embedded_font_extracted_from_fonts_dir() {
        // An ODT that embeds a plain TTF in `Fonts/` and declares it via a
        // `<style:font-face>` with an `<svg:font-face-uri>`: extraction returns
        // the face under its declared family, ready for the renderer.
        let content = r#"<?xml version="1.0"?>
          <office:document-content
              xmlns:office="o" xmlns:style="s" xmlns:svg="g" xmlns:fo="f" xmlns:xlink="x" xmlns:text="t">
            <office:font-face-decls>
              <style:font-face style:name="MyEmbedded" svg:font-family="MyEmbedded" fo:font-weight="bold">
                <svg:font-face-src>
                  <svg:font-face-uri xlink:href="Fonts/embed.ttf"/>
                </svg:font-face-src>
              </style:font-face>
            </office:font-face-decls>
            <office:body><office:text>
              <text:p>Hello</text:p>
            </office:text></office:body>
          </office:document-content>"#;
        let mut z = ZipWriter::new();
        z.add_stored("mimetype", b"application/vnd.oasis.opendocument.text");
        z.add_stored("content.xml", content.as_bytes());
        z.add_stored("Fonts/embed.ttf", &fixture_ttf());
        let odt = z.finish();
        let zip = read_zip(&odt);

        let fonts = extract_embedded_fonts(&zip);
        assert_eq!(fonts.len(), 1, "one ODF face extracted");
        assert_eq!(fonts[0].family, "MyEmbedded");
        assert_eq!(fonts[0].weight, 700, "fo:font-weight=bold → 700");
        assert_eq!(
            fonts[0].ttf,
            fixture_ttf(),
            "plain ODF font bytes pass through"
        );
    }

    #[test]
    fn non_font_bytes_are_rejected_as_provided_face() {
        // A corrupt / non-sfnt embedded part must not become a ProvidedFont
        // (it would otherwise poison the renderer's font book).
        assert!(make_provided_font("Bad", false, false, vec![0, 1, 2, 3, 4, 5]).is_none());
        assert!(!is_sfnt_font(b"not a font"));
        assert!(
            is_sfnt_font(&fixture_ttf()),
            "the bundled fixture is a valid sfnt"
        );
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
    fn xlsx_cell_font_border_alignment_apply_to_td() {
        // fontId 1 = bold + red + Arial 14; borderId 1 = thin black box; the xf's
        // own <alignment> centres the cell. The <td> must carry all of them.
        let styles_xml = r#"<styleSheet>
          <fonts count="2">
            <font><sz val="11"/><name val="Calibri"/></font>
            <font><b/><sz val="14"/><color rgb="FFFF0000"/><name val="Arial"/></font>
          </fonts>
          <fills count="1"><fill><patternFill patternType="none"/></fill></fills>
          <borders count="2">
            <border><left/><right/><top/><bottom/></border>
            <border>
              <left style="thin"><color rgb="FF000000"/></left>
              <right style="thin"><color rgb="FF000000"/></right>
              <top style="thin"><color rgb="FF000000"/></top>
              <bottom style="thin"><color rgb="FF000000"/></bottom>
            </border>
          </borders>
          <cellXfs count="2">
            <xf fontId="0" borderId="0"/>
            <xf fontId="1" borderId="1" applyFont="1" applyBorder="1" applyAlignment="1">
              <alignment horizontal="center" vertical="center"/>
            </xf>
          </cellXfs>
        </styleSheet>"#;
        let s = parse_xlsx_styles(styles_xml, &XlsxTheme::default());

        // Style 0 = the default font (Calibri 11) — its size/family may be set,
        // but it must NOT carry the discriminating bold/border/alignment styles.
        let css0 = s.css(0);
        assert!(!css0.contains("font-weight"), "default not bold: {css0}");
        assert!(!css0.contains("border"), "default no border: {css0}");
        assert!(!css0.contains("text-align"), "default no align: {css0}");
        // Style 1 carries the full font + border + alignment CSS.
        let css = s.css(1);
        assert!(css.contains("font-weight:bold;"), "bold: {css}");
        assert!(css.contains("color:#FF0000;"), "red font: {css}");
        assert!(css.contains("font-size:14pt;"), "size: {css}");
        assert!(css.contains("font-family:Arial;"), "family: {css}");
        assert!(css.contains("border:1px solid #000000;"), "border: {css}");
        assert!(css.contains("text-align:center;"), "h-align: {css}");
        assert!(css.contains("vertical-align:middle;"), "v-align: {css}");

        // And those land on the cell when rendered.
        let sheet = r#"<worksheet><sheetData>
          <row r="1">
            <c r="A1" s="0" t="inlineStr"><is><t>Plain</t></is></c>
            <c r="B1" s="1" t="inlineStr"><is><t>Fancy</t></is></c>
          </row>
        </sheetData></worksheet>"#;
        let table = xlsx_sheet_table(sheet, &[], &s);
        assert!(table.contains("font-weight:bold"), "td bold: {table}");
        assert!(table.contains("color:#FF0000"), "td red: {table}");
        assert!(
            table.contains("border:1px solid #000000"),
            "td border: {table}"
        );
        assert!(table.contains("text-align:center"), "td align: {table}");
        // The plain cell renders (it may carry only the default font, but no
        // bold/border/alignment from the fancy style).
        assert!(table.contains(">Plain</td>"), "plain present: {table}");

        // Full pipeline still renders a valid PDF carrying the text.
        let mut z = ZipWriter::new();
        z.add_stored("[Content_Types].xml", b"<Types/>");
        z.add_stored(
            "xl/workbook.xml",
            br#"<workbook><sheets><sheet name="S" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
        );
        z.add_stored("xl/styles.xml", styles_xml.as_bytes());
        z.add_stored("xl/worksheets/sheet1.xml", sheet.as_bytes());
        let pdf = office_to_pdf(&z.finish()).expect("xlsx converts");
        let text = norm(&opens(&pdf).to_text());
        assert!(text.contains("Fancy") && text.contains("Plain"), "{text}");
    }

    #[test]
    fn xlsx_theme_font_colour_resolves_and_custom_row_height() {
        // Font colour by theme index 4 (accent1=blue) → resolved #4472C4; and a
        // custom row height becomes a `height:..pt` on the <tr>.
        let theme = parse_xlsx_theme(
            r#"<theme><themeElements><clrScheme>
              <dk1><srgbClr val="000000"/></dk1>
              <lt1><srgbClr val="FFFFFF"/></lt1>
              <dk2><srgbClr val="44546A"/></dk2>
              <lt2><srgbClr val="E7E6E6"/></lt2>
              <accent1><srgbClr val="4472C4"/></accent1>
            </clrScheme></themeElements></theme>"#,
        );
        let styles_xml = r#"<styleSheet>
          <fonts count="2">
            <font/>
            <font><color theme="4"/></font>
          </fonts>
          <fills count="1"><fill><patternFill patternType="none"/></fill></fills>
          <cellXfs count="2">
            <xf fontId="0"/>
            <xf fontId="1" applyFont="1"/>
          </cellXfs>
        </styleSheet>"#;
        let s = parse_xlsx_styles(styles_xml, &theme);
        assert!(
            s.css(1).contains("color:#4472C4;"),
            "theme font colour resolved: {}",
            s.css(1)
        );

        // customHeight row → height on the <tr>; a plain row has none.
        let sheet = r#"<worksheet><sheetData>
          <row r="1" ht="30" customHeight="1">
            <c r="A1" s="1" t="inlineStr"><is><t>Tall</t></is></c>
          </row>
          <row r="2">
            <c r="A2" t="inlineStr"><is><t>Normal</t></is></c>
          </row>
        </sheetData></worksheet>"#;
        let table = xlsx_sheet_table(sheet, &[], &s);
        assert!(
            table.contains("<tr style=\"height:30pt\">"),
            "custom row height: {table}"
        );
        assert!(
            table.contains("color:#4472C4"),
            "themed font on td: {table}"
        );
        // The second row carries no height style.
        assert!(
            table.contains("<tr><td>Normal</td></tr>"),
            "plain row: {table}"
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
            page_h: A4_H,
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

    // ── DOCX wave 2: floating/anchored objects + page breaks ──

    /// Render a DOCX body that references media, so anchored-image markup
    /// (the absolute wrapper around an `<img>`) can be asserted on.
    fn docx_html_with_media(
        document_xml: &str,
        rels_xml: &str,
        media: &[(&str, Vec<u8>)],
    ) -> String {
        let mut zip: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for (k, v) in media {
            zip.insert((*k).to_string(), v.clone());
        }
        let rels = parse_rels(rels_xml);
        let styles = DocxStyles::default();
        let numbering = DocxNumbering::default();
        let footnotes = DocxFootnotes::default();
        let ctx = DocxCtx {
            zip: &zip,
            rels: &rels,
            styles: &styles,
            numbering: &numbering,
            footnotes: &footnotes,
            page_h: A4_H,
        };
        let mut body = String::new();
        docx_body(document_xml, &ctx, &mut body);
        body
    }

    #[test]
    fn docx_anchored_drawing_emits_absolute_at_offset() {
        // wp:anchor with posOffset X=914400 EMU (72pt), Y=457200 (36pt) and
        // wp:extent cx=1828800 (144pt), cy=914400 (72pt). The image must be
        // wrapped in a position:absolute div at left:72pt;top:36pt.
        let doc = r#"<w:document xmlns:w="x" xmlns:a="y" xmlns:r="z" xmlns:wp="wp"><w:body>
            <w:p><w:r><w:drawing>
              <wp:anchor>
                <wp:extent cx="1828800" cy="914400"/>
                <wp:positionH relativeFrom="page"><wp:posOffset>914400</wp:posOffset></wp:positionH>
                <wp:positionV relativeFrom="page"><wp:posOffset>457200</wp:posOffset></wp:positionV>
                <a:blip r:embed="rId7"/>
              </wp:anchor>
            </w:drawing></w:r></w:p>
          </w:body></w:document>"#;
        let rels = r#"<Relationships xmlns="x">
          <Relationship Id="rId7" Type="image" Target="media/logo.png"/>
        </Relationships>"#;
        let html = docx_html_with_media(doc, rels, &[("word/media/logo.png", red_png())]);
        assert!(
            html.contains("position:absolute"),
            "absolute wrapper: {html}"
        );
        assert!(html.contains("left:72pt"), "left 72pt: {html}");
        assert!(html.contains("top:36pt"), "top 36pt: {html}");
        assert!(html.contains("width:144pt"), "extent w 144pt: {html}");
        assert!(html.contains("height:72pt"), "extent h 72pt: {html}");
        assert!(
            html.contains("<img src=\"data:image/png;base64,"),
            "image embedded inside the float: {html}"
        );
        // The absolute div is a paragraph sibling, not nested inside <p>.
        let p_open = html.find("<p").unwrap();
        let abs = html.find("position:absolute").unwrap();
        let p_close = html.find("</p>").unwrap();
        assert!(
            abs > p_close || abs < p_open,
            "float is a sibling of <p>: {html}"
        );
    }

    #[test]
    fn docx_inline_drawing_stays_in_flow_not_absolute() {
        // A wp:inline drawing (no anchor) must remain an inline <img>, never
        // an absolute wrapper — guards the existing inline-image behaviour.
        let doc = r#"<w:document xmlns:w="x" xmlns:a="y" xmlns:r="z" xmlns:wp="wp"><w:body>
            <w:p><w:r><w:drawing>
              <wp:inline><wp:extent cx="914400" cy="914400"/><a:blip r:embed="rId3"/></wp:inline>
            </w:drawing></w:r></w:p>
          </w:body></w:document>"#;
        let rels = r#"<Relationships xmlns="x">
          <Relationship Id="rId3" Type="image" Target="media/p.png"/>
        </Relationships>"#;
        let html = docx_html_with_media(doc, rels, &[("word/media/p.png", red_png())]);
        assert!(
            !html.contains("position:absolute"),
            "inline drawing must stay in flow: {html}"
        );
        assert!(
            html.contains("<img src=\"data:image/png;base64,"),
            "inline img: {html}"
        );
    }

    #[test]
    fn docx_anchored_textbox_becomes_absolute_frame() {
        // A wp:anchor wrapping a w:txbxContent (a Word text box / "encadré")
        // must surface as an absolutely-positioned box carrying its text.
        let doc = r#"<w:document xmlns:w="x" xmlns:wp="wp" xmlns:mc="mc" xmlns:wps="wps"><w:body>
            <w:p><w:r><w:drawing>
              <wp:anchor>
                <wp:extent cx="2540000" cy="635000"/>
                <wp:positionH relativeFrom="margin"><wp:posOffset>635000</wp:posOffset></wp:positionH>
                <wp:positionV relativeFrom="margin"><wp:posOffset>1270000</wp:posOffset></wp:positionV>
                <mc:AlternateContent><mc:Choice><wps:wsp><wps:txbx><w:txbxContent>
                  <w:p><w:r><w:t>Boxed note</w:t></w:r></w:p>
                </w:txbxContent></wps:txbx></wps:wsp></mc:Choice></mc:AlternateContent>
              </wp:anchor>
            </w:drawing></w:r></w:p>
          </w:body></w:document>"#;
        let html = docx_html(doc);
        assert!(html.contains("position:absolute"), "absolute frame: {html}");
        // posOffset 635000 EMU = 50pt, 1270000 EMU = 100pt.
        assert!(html.contains("left:50pt"), "frame x=50pt: {html}");
        assert!(html.contains("top:100pt"), "frame y=100pt: {html}");
        assert!(html.contains("width:200pt"), "extent w=200pt: {html}");
        assert!(html.contains("Boxed note"), "text box content kept: {html}");
        assert!(
            html.contains("border:1px solid"),
            "rendered as a framed box: {html}"
        );
    }

    #[test]
    fn docx_anchor_align_keywords_map_to_edges() {
        // wp:align right/bottom → pin to the right/bottom edge of the box.
        let doc = r#"<w:document xmlns:w="x" xmlns:a="y" xmlns:r="z" xmlns:wp="wp"><w:body>
            <w:p><w:r><w:drawing>
              <wp:anchor>
                <wp:extent cx="914400" cy="914400"/>
                <wp:positionH relativeFrom="margin"><wp:align>right</wp:align></wp:positionH>
                <wp:positionV relativeFrom="margin"><wp:align>bottom</wp:align></wp:positionV>
                <a:blip r:embed="rId1"/>
              </wp:anchor>
            </w:drawing></w:r></w:p>
          </w:body></w:document>"#;
        let rels = r#"<Relationships xmlns="x">
          <Relationship Id="rId1" Type="image" Target="media/c.png"/>
        </Relationships>"#;
        let html = docx_html_with_media(doc, rels, &[("word/media/c.png", red_png())]);
        assert!(html.contains("position:absolute"), "absolute: {html}");
        assert!(
            html.contains("right:0pt"),
            "align right → right edge: {html}"
        );
        assert!(
            html.contains("bottom:0pt"),
            "align bottom → bottom edge: {html}"
        );
    }

    #[test]
    fn docx_run_page_break_forces_new_page() {
        // <w:br w:type="page"/> must emit a hard page break after the paragraph.
        let doc = r#"<w:document xmlns:w="x"><w:body>
            <w:p><w:r><w:t>Page one</w:t></w:r><w:r><w:br w:type="page"/></w:r></w:p>
            <w:p><w:r><w:t>Page two</w:t></w:r></w:p>
          </w:body></w:document>"#;
        let html = docx_html(doc);
        assert!(
            html.contains("page-break-before:always"),
            "explicit page break emitted: {html}"
        );
        // The break sits between the two paragraphs.
        let one = html.find("Page one").unwrap();
        let brk = html.find("page-break-before:always").unwrap();
        let two = html.find("Page two").unwrap();
        assert!(one < brk && brk < two, "break between the pages: {html}");
        // A plain in-paragraph soft break stays a <br>, not a page break.
        let soft = docx_html(
            r#"<w:document xmlns:w="x"><w:body><w:p><w:r><w:t>a</w:t></w:r><w:r><w:br/></w:r><w:r><w:t>b</w:t></w:r></w:p></w:body></w:document>"#,
        );
        assert!(
            soft.contains("<br>") && !soft.contains("page-break"),
            "soft break: {soft}"
        );
    }

    #[test]
    fn docx_page_break_before_paragraph_starts_new_page() {
        // w:pPr/w:pageBreakBefore → a hard break right before the paragraph.
        let doc = r#"<w:document xmlns:w="x"><w:body>
            <w:p><w:r><w:t>Section A</w:t></w:r></w:p>
            <w:p><w:pPr><w:pageBreakBefore/></w:pPr><w:r><w:t>Section B</w:t></w:r></w:p>
          </w:body></w:document>"#;
        let html = docx_html(doc);
        let a = html.find("Section A").unwrap();
        let brk = html.find("page-break-before:always").unwrap();
        let b = html.find("Section B").unwrap();
        assert!(a < brk && brk < b, "break precedes Section B: {html}");
    }

    #[test]
    fn docx_section_break_paragraph_starts_new_page() {
        // A paragraph carrying w:pPr/w:sectPr ends a section → next page.
        let doc = r#"<w:document xmlns:w="x"><w:body>
            <w:p><w:r><w:t>End of section one</w:t></w:r></w:p>
            <w:p><w:pPr><w:sectPr><w:pgSz w:w="11906" w:h="16838"/></w:sectPr></w:pPr></w:p>
            <w:p><w:r><w:t>Start of section two</w:t></w:r></w:p>
          </w:body></w:document>"#;
        let html = docx_html(doc);
        assert!(
            html.contains("page-break-before:always"),
            "intermediate sectPr forces a page break: {html}"
        );
        let one = html.find("End of section one").unwrap();
        let brk = html.find("page-break-before:always").unwrap();
        let two = html.find("Start of section two").unwrap();
        assert!(one < brk && brk < two, "break between sections: {html}");
    }

    #[test]
    fn docx_anchor_and_page_break_render_to_multipage_pdf_end_to_end() {
        // Full pipeline (parse → HTML → layout → PDF): an anchored image plus a
        // hard page break must yield a valid, multi-page PDF without panicking.
        let doc = r#"<?xml version="1.0"?>
<w:document xmlns:w="x" xmlns:a="y" xmlns:r="z" xmlns:wp="wp"><w:body>
  <w:p><w:r><w:t>First page body</w:t></w:r></w:p>
  <w:p><w:r><w:drawing>
    <wp:anchor>
      <wp:extent cx="914400" cy="914400"/>
      <wp:positionH relativeFrom="page"><wp:posOffset>457200</wp:posOffset></wp:positionH>
      <wp:positionV relativeFrom="page"><wp:posOffset>457200</wp:posOffset></wp:positionV>
      <a:blip r:embed="rId4"/>
    </wp:anchor>
  </w:drawing></w:r></w:p>
  <w:p><w:r><w:br w:type="page"/></w:r></w:p>
  <w:p><w:r><w:t>Second page body</w:t></w:r></w:p>
</w:body></w:document>"#;
        let rels = r#"<Relationships xmlns="x">
          <Relationship Id="rId4" Type="image" Target="media/anchor.png"/>
        </Relationships>"#;
        let bytes = build_docx(doc, Some(rels), &[("word/media/anchor.png", red_png())]);
        let pdf = office_to_pdf(&bytes).expect("docx converts");
        let document = opens(&pdf);
        assert!(
            document.page_count() >= 2,
            "hard page break splits into >=2 pages (got {})",
            document.page_count()
        );
        let text = norm(&document.to_text());
        assert!(text.contains("First page body"), "first page text: {text}");
        assert!(
            text.contains("Second page body"),
            "second page text: {text}"
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

    // ── DOCX → editable model (images / hyperlinks / strike / highlight) ──

    /// All inline runs/images/links across the first section's top-level
    /// paragraphs (descends one level into list-item paragraphs), for assertions.
    fn model_first_section_inlines(doc: &Document) -> Vec<Inline> {
        let mut out = Vec::new();
        let mut visit = |para: &Paragraph| out.extend(para.runs.iter().cloned());
        for block in &doc.sections[0].pages[0].blocks {
            match &block.kind {
                BlockKind::Paragraph(p) => visit(p),
                BlockKind::Heading(h) => visit(&h.para),
                BlockKind::List(l) => {
                    for item in &l.items {
                        for b in &item.blocks {
                            if let BlockKind::Paragraph(p) = &b.kind {
                                visit(p);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }

    #[test]
    fn docx_model_inline_image_lands_in_resources() {
        // A `<w:drawing><a:blip>` in the model path → an `Inline::Image` whose
        // resource is interned in `Document.resources` (the editor sees the image).
        let doc = r#"<w:document xmlns:w="x" xmlns:a="y" xmlns:r="z">
  <w:body>
    <w:p><w:r><w:t>Before</w:t></w:r></w:p>
    <w:p><w:r><w:drawing><a:blip r:embed="rId5"/></w:drawing></w:r></w:p>
  </w:body>
</w:document>"#;
        let rels = r#"<Relationships xmlns="x">
  <Relationship Id="rId5" Type="image" Target="media/pic.png"/>
</Relationships>"#;
        let bytes = build_docx(doc, Some(rels), &[("word/media/pic.png", red_png())]);
        let model = office_to_model(&bytes).expect("docx → model");

        // Exactly one image inline, and its resource blob is present.
        let inlines = model_first_section_inlines(&model);
        let img = inlines
            .iter()
            .find_map(|i| match i {
                Inline::Image(r) => Some(r.clone()),
                _ => None,
            })
            .expect("an Inline::Image in the model");
        assert!(
            model.resources.images.contains_key(&img.resource),
            "image bytes interned in the resource table"
        );
        assert_eq!(model.resources.images[&img.resource].format, "png");
        assert!(!model.resources.images[&img.resource].bytes.is_empty());
    }

    #[test]
    fn docx_model_hyperlink_becomes_link() {
        // `<w:hyperlink r:id>` → `Inline::Link` with the URL resolved from rels,
        // wrapping the run text.
        let doc = r#"<w:document xmlns:w="x" xmlns:r="z">
  <w:body>
    <w:p>
      <w:r><w:t>see </w:t></w:r>
      <w:hyperlink r:id="rId9"><w:r><w:t>our site</w:t></w:r></w:hyperlink>
    </w:p>
  </w:body>
</w:document>"#;
        let rels = r#"<Relationships xmlns="x">
  <Relationship Id="rId9" Type="hyperlink" Target="https://example.com/" TargetMode="External"/>
</Relationships>"#;
        let bytes = build_docx(doc, Some(rels), &[]);
        let model = office_to_model(&bytes).expect("docx → model");

        let inlines = model_first_section_inlines(&model);
        let (href, children) = inlines
            .iter()
            .find_map(|i| match i {
                Inline::Link { href, children } => Some((href.clone(), children.clone())),
                _ => None,
            })
            .expect("an Inline::Link in the model");
        assert_eq!(href, model::LinkTarget::Url("https://example.com/".to_string()));
        let link_text: String = children
            .iter()
            .filter_map(|c| match c {
                Inline::Run(r) => Some(r.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(link_text, "our site");
    }

    #[test]
    fn docx_model_floating_anchor_image_block_with_frame_and_alt() {
        // A floating `wp:anchor` drawing (posOffset + wrapSquare + descr="logo")
        // lifts out of the run flow into a sibling `Block { kind: Image, frame }`:
        // the alt text is carried, the `wp:extent` size and the `wp:posOffset`
        // position land on the frame (Y flipped about the A4 page height).
        let doc = r#"<w:document xmlns:w="x" xmlns:a="y" xmlns:r="z" xmlns:wp="wp">
  <w:body>
    <w:p><w:r><w:t>Before the logo</w:t></w:r>
      <w:r><w:drawing>
        <wp:anchor behindDoc="0">
          <wp:extent cx="1828800" cy="914400"/>
          <wp:positionH relativeFrom="page"><wp:posOffset>914400</wp:posOffset></wp:positionH>
          <wp:positionV relativeFrom="page"><wp:posOffset>457200</wp:posOffset></wp:positionV>
          <wp:wrapSquare wrapText="bothSides"/>
          <wp:docPr id="1" name="Picture 1" descr="logo"/>
          <a:blip r:embed="rId7"/>
        </wp:anchor>
      </w:drawing></w:r>
    </w:p>
  </w:body>
</w:document>"#;
        let rels = r#"<Relationships xmlns="x">
  <Relationship Id="rId7" Type="image" Target="media/pic.png"/>
</Relationships>"#;
        let bytes = build_docx(doc, Some(rels), &[("word/media/pic.png", red_png())]);
        let model = office_to_model(&bytes).expect("docx → model");

        // The float is a sibling Image block (not an inline of the paragraph).
        let blocks = &model.sections[0].pages[0].blocks;
        let (img, frame) = blocks
            .iter()
            .find_map(|b| match &b.kind {
                BlockKind::Image(ir) => Some((ir.clone(), b.frame)),
                _ => None,
            })
            .expect("a floating Image block");

        // Alt text from `wp:docPr@descr`.
        assert_eq!(img.alt.as_deref(), Some("logo"));
        // The picture bytes are interned in the resource table.
        assert!(model.resources.images.contains_key(&img.resource));

        // Size from `wp:extent` (1828800 EMU = 144pt, 914400 = 72pt).
        let rect = frame.expect("floating block carries a frame");
        assert!((rect.w - 144.0).abs() < 1e-6, "width 144pt, got {}", rect.w);
        assert!((rect.h - 72.0).abs() < 1e-6, "height 72pt, got {}", rect.h);
        // Position from `wp:posOffset` (X 914400 EMU = 72pt). Y is flipped about
        // the A4 page height (841.89pt): y_top=36pt, h=72pt → 841.89-(36+72).
        assert!((rect.x - 72.0).abs() < 1e-6, "x 72pt, got {}", rect.x);
        let expect_y = A4_H - (36.0 + 72.0);
        assert!(
            (rect.y - expect_y).abs() < 1e-3,
            "y flipped to {expect_y}, got {}",
            rect.y
        );

        // It did NOT also leak into the paragraph's inline run flow.
        let inline_imgs = model_first_section_inlines(&model)
            .iter()
            .filter(|i| matches!(i, Inline::Image(_)))
            .count();
        assert_eq!(inline_imgs, 0, "floating image stays out of the run flow");
    }

    #[test]
    fn docx_model_inline_drawing_stays_inline_with_alt() {
        // A `wp:inline` drawing (no `wp:anchor`) stays an `Inline::Image` in the
        // run flow; its `wp:docPr@descr` still rides along as the alt text. An
        // inline image has no size slot in the model, so the extent isn't lowered.
        let doc = r#"<w:document xmlns:w="x" xmlns:a="y" xmlns:r="z" xmlns:wp="wp">
  <w:body>
    <w:p><w:r><w:drawing>
      <wp:inline>
        <wp:extent cx="914400" cy="914400"/>
        <wp:docPr id="2" name="Inline 1" descr="inline pic"/>
        <a:blip r:embed="rId3"/>
      </wp:inline>
    </w:drawing></w:r></w:p>
  </w:body>
</w:document>"#;
        let rels = r#"<Relationships xmlns="x">
  <Relationship Id="rId3" Type="image" Target="media/pic.png"/>
</Relationships>"#;
        let bytes = build_docx(doc, Some(rels), &[("word/media/pic.png", red_png())]);
        let model = office_to_model(&bytes).expect("docx → model");

        // Exactly one inline image, with the alt text set; no floating block.
        let inlines = model_first_section_inlines(&model);
        let img = inlines
            .iter()
            .find_map(|i| match i {
                Inline::Image(r) => Some(r.clone()),
                _ => None,
            })
            .expect("an Inline::Image in the run flow");
        assert_eq!(img.alt.as_deref(), Some("inline pic"));
        assert!(model.resources.images.contains_key(&img.resource));

        let float_blocks = model.sections[0].pages[0]
            .blocks
            .iter()
            .filter(|b| matches!(b.kind, BlockKind::Image(_)))
            .count();
        assert_eq!(
            float_blocks, 0,
            "an inline drawing is not lifted to a block"
        );
    }

    #[test]
    fn docx_model_drawing_without_descr_has_no_alt() {
        // A drawing with no `wp:docPr@descr`/`@title` → `ImageRef.alt` stays None
        // (no panic, no empty-string alt).
        let doc = r#"<w:document xmlns:w="x" xmlns:a="y" xmlns:r="z" xmlns:wp="wp">
  <w:body>
    <w:p><w:r><w:drawing>
      <wp:inline><wp:extent cx="914400" cy="914400"/><a:blip r:embed="rId5"/></wp:inline>
    </w:drawing></w:r></w:p>
  </w:body>
</w:document>"#;
        let rels = r#"<Relationships xmlns="x">
  <Relationship Id="rId5" Type="image" Target="media/pic.png"/>
</Relationships>"#;
        let bytes = build_docx(doc, Some(rels), &[("word/media/pic.png", red_png())]);
        let model = office_to_model(&bytes).expect("docx → model");

        let img = model_first_section_inlines(&model)
            .iter()
            .find_map(|i| match i {
                Inline::Image(r) => Some(r.clone()),
                _ => None,
            })
            .expect("an Inline::Image");
        assert_eq!(img.alt, None, "no descr/title ⇒ no alt text");
    }

    #[test]
    fn docx_model_strike_and_highlight() {
        // `<w:strike>` → CharStyle.strike; `<w:highlight w:val="yellow">` →
        // CharStyle.background (the named colour mapped to RGB).
        let doc = r#"<w:document xmlns:w="x">
  <w:body>
    <w:p>
      <w:r><w:rPr><w:strike/><w:highlight w:val="yellow"/></w:rPr><w:t>marked</w:t></w:r>
    </w:p>
  </w:body>
</w:document>"#;
        let bytes = build_docx(doc, None, &[]);
        let model = office_to_model(&bytes).expect("docx → model");

        let run = model_first_section_inlines(&model)
            .into_iter()
            .find_map(|i| match i {
                Inline::Run(r) if r.text == "marked" => Some(r),
                _ => None,
            })
            .expect("the 'marked' run");
        assert!(run.style.strike, "strike-through carried into the model");
        assert_eq!(
            run.style.background,
            Some([1.0, 1.0, 0.0]),
            "yellow highlight → RGB background"
        );
    }

    #[test]
    fn docx_model_run_shading_sets_background() {
        // `<w:shd w:fill>` run shading also populates the model background.
        let doc = r#"<w:document xmlns:w="x">
  <w:body>
    <w:p><w:r><w:rPr><w:shd w:val="clear" w:fill="00FF00"/></w:rPr><w:t>shaded</w:t></w:r></w:p>
  </w:body>
</w:document>"#;
        let bytes = build_docx(doc, None, &[]);
        let model = office_to_model(&bytes).expect("docx → model");
        let run = model_first_section_inlines(&model)
            .into_iter()
            .find_map(|i| match i {
                Inline::Run(r) if r.text == "shaded" => Some(r),
                _ => None,
            })
            .expect("the 'shaded' run");
        assert_eq!(run.style.background, Some([0.0, 1.0, 0.0]));
    }

    /// The first `BlockKind::Table` block of a lowered model document.
    fn docx_model_first_table(doc: &Document) -> &Table {
        doc.sections[0].pages[0]
            .blocks
            .iter()
            .find_map(|b| match &b.kind {
                BlockKind::Table(t) => Some(t),
                _ => None,
            })
            .expect("a Table block in the model")
    }

    #[test]
    fn docx_model_paragraph_alignment_indent_spacing_lowered() {
        // `w:pPr` alignment (`w:jc`), spacing (`w:spacing`) and indentation
        // (`w:ind`) lower to the model's `ParagraphStyle` (twips → points).
        let doc = r#"<w:document xmlns:w="x">
  <w:body>
    <w:p>
      <w:pPr>
        <w:jc w:val="center"/>
        <w:spacing w:before="240" w:after="120"/>
        <w:ind w:left="720" w:right="360" w:firstLine="240"/>
      </w:pPr>
      <w:r><w:t>Centered</w:t></w:r>
    </w:p>
  </w:body>
</w:document>"#;
        let bytes = build_docx(doc, None, &[]);
        let model = office_to_model(&bytes).expect("docx → model");
        let para = model.sections[0].pages[0]
            .blocks
            .iter()
            .find_map(|b| match &b.kind {
                BlockKind::Paragraph(p) => Some(p),
                _ => None,
            })
            .expect("a Paragraph block");
        let s = &para.style;
        assert_eq!(s.align, MAlign::Center, "w:jc center → Align::Center");
        // 240 twips = 12pt, 120 = 6pt, 720 = 36pt, 360 = 18pt.
        assert!((s.space_before_pt - 12.0).abs() < 1e-6, "before: {s:?}");
        assert!((s.space_after_pt - 6.0).abs() < 1e-6, "after: {s:?}");
        assert!((s.indent_left_pt - 36.0).abs() < 1e-6, "left: {s:?}");
        assert!((s.indent_right_pt - 18.0).abs() < 1e-6, "right: {s:?}");
        assert!((s.first_line_pt - 12.0).abs() < 1e-6, "firstLine: {s:?}");
    }

    /// Build a DOCX whose `word/styles.xml` is `styles_xml` (added verbatim via
    /// `build_docx`'s media slot) alongside `document_xml`.
    fn build_docx_with_styles(document_xml: &str, styles_xml: &str) -> Vec<u8> {
        build_docx(
            document_xml,
            None,
            &[("word/styles.xml", styles_xml.as_bytes().to_vec())],
        )
    }

    #[test]
    fn docx_model_named_style_table_lowered_and_style_ref_resolves() {
        // `word/styles.xml` defines `Normal` and a `Heading1` (basedOn Normal,
        // bold, centred, 16pt). Each `w:style w:type="paragraph"` must become a
        // `NamedStyle` in `Document.styles`, with `w:basedOn` kept as `based_on`
        // (not flattened). A paragraph `w:pStyle w:val="Heading1"` must carry a
        // `style_ref` that resolves into the table.
        let styles = r#"<?xml version="1.0"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:docDefaults><w:rPrDefault><w:rPr><w:sz w:val="22"/></w:rPr></w:rPrDefault></w:docDefaults>
  <w:style w:type="paragraph" w:styleId="Normal">
    <w:name w:val="Normal"/>
  </w:style>
  <w:style w:type="paragraph" w:styleId="Heading1">
    <w:name w:val="heading 1"/>
    <w:basedOn w:val="Normal"/>
    <w:pPr><w:jc w:val="center"/></w:pPr>
    <w:rPr><w:b/><w:sz w:val="32"/></w:rPr>
  </w:style>
  <w:style w:type="character" w:styleId="Emphasis">
    <w:name w:val="Emphasis"/>
    <w:rPr><w:i/></w:rPr>
  </w:style>
</w:styles>"#;
        let doc = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Big Title</w:t></w:r></w:p>
  </w:body>
</w:document>"#;
        let bytes = build_docx_with_styles(doc, styles);
        let model = office_to_model(&bytes).expect("docx → model");

        // The named-style table holds the two paragraph styles; the character
        // style (`Emphasis`) is skipped (no paragraph identity to host).
        let h1 = model
            .styles
            .named
            .get(&model::StyleId("Heading1".to_string()))
            .expect("Heading1 lowered into Document.styles");
        assert_eq!(
            h1.based_on,
            Some(model::StyleId("Normal".to_string())),
            "w:basedOn → based_on (kept, not flattened)"
        );
        assert!(h1.char_.bold, "w:b → bold");
        assert!((h1.char_.size_pt - 16.0).abs() < 1e-6, "w:sz 32 → 16pt");
        assert_eq!(h1.para.align, MAlign::Center, "w:jc center → Center");
        assert!(
            model
                .styles
                .named
                .contains_key(&model::StyleId("Normal".to_string())),
            "Normal lowered too"
        );
        assert!(
            !model
                .styles
                .named
                .contains_key(&model::StyleId("Emphasis".to_string())),
            "character style not lowered into the paragraph style table"
        );

        // The paragraph's style_ref points at Heading1 and resolves in the table.
        let para = model.sections[0].pages[0]
            .blocks
            .iter()
            .find_map(|b| match &b.kind {
                BlockKind::Heading(h) => Some(&h.para),
                BlockKind::Paragraph(p) => Some(p),
                _ => None,
            })
            .expect("a heading/paragraph block");
        let sref = para.style_ref.clone().expect("style_ref set from w:pStyle");
        assert_eq!(sref, model::StyleId("Heading1".to_string()));
        assert!(
            model.styles.named.contains_key(&sref),
            "style_ref must resolve into Document.styles (no dangling id)"
        );
    }

    #[test]
    fn docx_model_no_styles_xml_yields_empty_style_table() {
        // A DOCX without `word/styles.xml` must lower to an empty style table
        // (no panic, no spurious entries).
        let doc = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body><w:p><w:r><w:t>Plain</w:t></w:r></w:p></w:body>
</w:document>"#;
        let bytes = build_docx(doc, None, &[]);
        let model = office_to_model(&bytes).expect("docx → model");
        assert!(
            model.styles.named.is_empty(),
            "no styles.xml ⇒ empty style table, got {:?}",
            model.styles.named
        );
    }

    #[test]
    fn docx_model_table_borders_shading_height_and_span_lowered() {
        // `w:tblBorders` → the table-wide BorderStyle; `w:tcPr/w:shd@w:fill` →
        // Cell.shading; `w:trPr/w:trHeight` → Row.height; `w:gridSpan` →
        // Cell.col_span. (`w:vAlign` has no model slot — see the issue note.)
        let doc = r#"<w:document xmlns:w="x">
  <w:body>
    <w:tbl>
      <w:tblPr>
        <w:tblBorders>
          <w:top w:val="single" w:sz="8" w:color="FF0000"/>
          <w:left w:val="single" w:sz="8" w:color="FF0000"/>
          <w:bottom w:val="single" w:sz="8" w:color="FF0000"/>
          <w:right w:val="single" w:sz="8" w:color="FF0000"/>
        </w:tblBorders>
      </w:tblPr>
      <w:tblGrid><w:gridCol w:w="2880"/><w:gridCol w:w="2880"/></w:tblGrid>
      <w:tr>
        <w:trPr><w:trHeight w:val="480"/></w:trPr>
        <w:tc>
          <w:tcPr>
            <w:gridSpan w:val="2"/>
            <w:shd w:val="clear" w:color="auto" w:fill="00FF00"/>
            <w:vAlign w:val="center"/>
          </w:tcPr>
          <w:p><w:r><w:t>Wide</w:t></w:r></w:p>
        </w:tc>
      </w:tr>
    </w:tbl>
  </w:body>
</w:document>"#;
        let bytes = build_docx(doc, None, &[]);
        let model = office_to_model(&bytes).expect("docx → model");
        let table = docx_model_first_table(&model);

        // Border: w:sz="8" eighths = 1pt, colour red.
        assert!((table.border.width - 1.0).abs() < 1e-6, "border 1pt");
        assert_eq!(table.border.color, [1.0, 0.0, 0.0], "border red");

        // Row height: 480 twips = 24pt.
        let row = &table.rows[0];
        let h = row.height.expect("row height set");
        assert!((h - 24.0).abs() < 1e-6, "row height 24pt, got {h}");

        // Cell shading 00FF00 (green) and a 2-column grid span.
        let cell = &row.cells[0];
        assert_eq!(cell.shading, Some([0.0, 1.0, 0.0]), "cell shading green");
        assert_eq!(cell.col_span, 2, "gridSpan=2 → col_span");
    }

    #[test]
    fn docx_model_cell_borders_seed_table_border() {
        // With no `w:tblBorders`, the first `w:tcBorders` edge seeds the model's
        // single table-wide BorderStyle (mirrors the PPTX cell-edge path).
        let doc = r#"<w:document xmlns:w="x">
  <w:body>
    <w:tbl>
      <w:tblGrid><w:gridCol w:w="2880"/></w:tblGrid>
      <w:tr><w:tc>
        <w:tcPr>
          <w:tcBorders>
            <w:top w:val="single" w:sz="16" w:color="0000FF"/>
          </w:tcBorders>
        </w:tcPr>
        <w:p><w:r><w:t>Boxed</w:t></w:r></w:p>
      </w:tc></w:tr>
    </w:tbl>
  </w:body>
</w:document>"#;
        let bytes = build_docx(doc, None, &[]);
        let model = office_to_model(&bytes).expect("docx → model");
        let table = docx_model_first_table(&model);
        // w:sz="16" eighths = 2pt, colour blue, taken from the cell edge.
        assert!(
            (table.border.width - 2.0).abs() < 1e-6,
            "border 2pt from tcBorders"
        );
        assert_eq!(
            table.border.color,
            [0.0, 0.0, 1.0],
            "border blue from tcBorders"
        );
    }

    // ── #39: DOCX hard page breaks + numbering (model path) ──

    /// Concatenated plain text of all `Inline::Run`s on one model [`Page`]
    /// (paragraphs, headings, and list-item paragraphs), for asserting which
    /// page a paragraph lands on.
    fn page_text(page: &model::Page) -> String {
        fn para_text(p: &Paragraph, out: &mut String) {
            for r in &p.runs {
                if let Inline::Run(run) = r {
                    out.push_str(&run.text);
                }
            }
        }
        let mut out = String::new();
        for block in &page.blocks {
            match &block.kind {
                BlockKind::Paragraph(p) => para_text(p, &mut out),
                BlockKind::Heading(h) => para_text(&h.para, &mut out),
                BlockKind::List(l) => {
                    for item in &l.items {
                        for b in &item.blocks {
                            if let BlockKind::Paragraph(p) = &b.kind {
                                para_text(p, &mut out);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// Every `(ordered, marker, level, text)` of the single-item `List` blocks
    /// across all pages of the first section, in document order.
    fn docx_model_list_paragraphs(doc: &Document) -> Vec<(bool, ListMarker, u8, String)> {
        let mut out = Vec::new();
        for page in &doc.sections[0].pages {
            for block in &page.blocks {
                if let BlockKind::List(l) = &block.kind {
                    for item in &l.items {
                        let mut text = String::new();
                        for b in &item.blocks {
                            if let BlockKind::Paragraph(p) = &b.kind {
                                for r in &p.runs {
                                    if let Inline::Run(run) = r {
                                        text.push_str(&run.text);
                                    }
                                }
                            }
                        }
                        out.push((l.ordered, l.marker, item.level, text));
                    }
                }
            }
        }
        out
    }

    #[test]
    fn docx_model_run_page_break_splits_into_two_pages() {
        // `<w:br w:type="page"/>` ends page one; the next paragraph lands on a
        // fresh model `Page` (the model represents a hard break as several pages
        // in one section).
        let doc = r#"<w:document xmlns:w="x"><w:body>
            <w:p><w:r><w:t>Page one</w:t></w:r><w:r><w:br w:type="page"/></w:r></w:p>
            <w:p><w:r><w:t>Page two</w:t></w:r></w:p>
          </w:body></w:document>"#;
        let bytes = build_docx(doc, None, &[]);
        let model = office_to_model(&bytes).expect("docx → model");
        let pages = &model.sections[0].pages;
        assert_eq!(pages.len(), 2, "hard page break → two model pages");
        assert!(
            page_text(&pages[0]).contains("Page one"),
            "p0: {:?}",
            page_text(&pages[0])
        );
        assert!(
            page_text(&pages[1]).contains("Page two"),
            "p1: {:?}",
            page_text(&pages[1])
        );
        assert!(
            !page_text(&pages[0]).contains("Page two"),
            "second paragraph not on page one"
        );
    }

    #[test]
    fn docx_model_soft_break_stays_on_one_page() {
        // A plain `<w:br/>` (no `w:type="page"`) is an in-paragraph line break:
        // it stays an `Inline::LineBreak` and does NOT split pages.
        let doc = r#"<w:document xmlns:w="x"><w:body>
            <w:p><w:r><w:t>a</w:t></w:r><w:r><w:br/></w:r><w:r><w:t>b</w:t></w:r></w:p>
          </w:body></w:document>"#;
        let bytes = build_docx(doc, None, &[]);
        let model = office_to_model(&bytes).expect("docx → model");
        assert_eq!(
            model.sections[0].pages.len(),
            1,
            "soft break keeps one page"
        );
        let inlines = model_first_section_inlines(&model);
        assert!(
            inlines.iter().any(|i| matches!(i, Inline::LineBreak)),
            "soft break stays an Inline::LineBreak: {inlines:?}"
        );
    }

    #[test]
    fn docx_model_page_break_before_starts_new_page() {
        // `w:pPr/w:pageBreakBefore` opens a new model page *before* the paragraph.
        let doc = r#"<w:document xmlns:w="x"><w:body>
            <w:p><w:r><w:t>Section A</w:t></w:r></w:p>
            <w:p><w:pPr><w:pageBreakBefore/></w:pPr><w:r><w:t>Section B</w:t></w:r></w:p>
          </w:body></w:document>"#;
        let bytes = build_docx(doc, None, &[]);
        let model = office_to_model(&bytes).expect("docx → model");
        let pages = &model.sections[0].pages;
        assert_eq!(pages.len(), 2, "pageBreakBefore → two model pages");
        assert!(
            page_text(&pages[0]).contains("Section A"),
            "p0: {:?}",
            page_text(&pages[0])
        );
        assert!(
            page_text(&pages[1]).contains("Section B"),
            "p1: {:?}",
            page_text(&pages[1])
        );
    }

    #[test]
    fn docx_model_page_break_before_false_does_not_split() {
        // `w:pageBreakBefore w:val="0"` cancels the break: one page.
        let doc = r#"<w:document xmlns:w="x"><w:body>
            <w:p><w:r><w:t>One</w:t></w:r></w:p>
            <w:p><w:pPr><w:pageBreakBefore w:val="0"/></w:pPr><w:r><w:t>Two</w:t></w:r></w:p>
          </w:body></w:document>"#;
        let bytes = build_docx(doc, None, &[]);
        let model = office_to_model(&bytes).expect("docx → model");
        assert_eq!(
            model.sections[0].pages.len(),
            1,
            "w:val=\"0\" cancels the page break"
        );
    }

    #[test]
    fn docx_model_two_level_numbering_lowers_per_level_format() {
        // numbering.xml: numId 5 → abstract 0; level 0 decimal ("%1."), level 1
        // lowerLetter ("%2)"). The *format* lowers per level (each list paragraph
        // is its own one-item List): level 0 → Decimal, level 1 → LowerAlpha.
        // (The `%1.` / `%2)` lvlText templates and the per-level start are not
        // carried — the model has no such slot; see docs/CONVERSIONS.md.)
        let numbering = r#"<w:numbering xmlns:w="x">
          <w:abstractNum w:abstractNumId="0">
            <w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl>
            <w:lvl w:ilvl="1"><w:start w:val="1"/><w:numFmt w:val="lowerLetter"/><w:lvlText w:val="%2)"/></w:lvl>
          </w:abstractNum>
          <w:num w:numId="5"><w:abstractNumId w:val="0"/></w:num>
        </w:numbering>"#;
        let doc = r#"<w:document xmlns:w="x"><w:body>
          <w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="5"/></w:numPr></w:pPr><w:r><w:t>One</w:t></w:r></w:p>
          <w:p><w:pPr><w:numPr><w:ilvl w:val="1"/><w:numId w:val="5"/></w:numPr></w:pPr><w:r><w:t>Sub a</w:t></w:r></w:p>
          <w:p><w:pPr><w:numPr><w:ilvl w:val="1"/><w:numId w:val="5"/></w:numPr></w:pPr><w:r><w:t>Sub b</w:t></w:r></w:p>
          <w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="5"/></w:numPr></w:pPr><w:r><w:t>Two</w:t></w:r></w:p>
        </w:body></w:document>"#;
        let bytes = build_docx(
            doc,
            None,
            &[("word/numbering.xml", numbering.as_bytes().to_vec())],
        );
        let model = office_to_model(&bytes).expect("docx → model");
        let lists = docx_model_list_paragraphs(&model);
        assert_eq!(lists.len(), 4, "four list paragraphs: {lists:?}");
        // Level-0 paragraphs → ordered Decimal.
        assert_eq!(lists[0], (true, ListMarker::Decimal, 0, "One".to_string()));
        assert_eq!(lists[3], (true, ListMarker::Decimal, 0, "Two".to_string()));
        // Level-1 paragraphs → ordered LowerAlpha at nesting level 1.
        assert_eq!(
            lists[1],
            (true, ListMarker::LowerAlpha, 1, "Sub a".to_string())
        );
        assert_eq!(
            lists[2],
            (true, ListMarker::LowerAlpha, 1, "Sub b".to_string())
        );
    }

    #[test]
    fn docx_model_lvl_override_changes_level_format() {
        // `w:num` 7 maps to abstract 0 (decimal at level 0) but carries a
        // `w:lvlOverride w:ilvl="0"` whose nested `w:lvl/w:numFmt` re-defines the
        // level as upperRoman, plus a `w:startOverride` (restart, not lowered).
        // The instance's overriding *format* must win → UpperRoman marker.
        let numbering = r#"<w:numbering xmlns:w="x">
          <w:abstractNum w:abstractNumId="0">
            <w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl>
          </w:abstractNum>
          <w:num w:numId="7">
            <w:abstractNumId w:val="0"/>
            <w:lvlOverride w:ilvl="0">
              <w:startOverride w:val="5"/>
              <w:lvl w:ilvl="0"><w:start w:val="5"/><w:numFmt w:val="upperRoman"/><w:lvlText w:val="%1)"/></w:lvl>
            </w:lvlOverride>
          </w:num>
        </w:numbering>"#;
        let doc = r#"<w:document xmlns:w="x"><w:body>
          <w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="7"/></w:numPr></w:pPr><w:r><w:t>Item</w:t></w:r></w:p>
        </w:body></w:document>"#;
        let bytes = build_docx(
            doc,
            None,
            &[("word/numbering.xml", numbering.as_bytes().to_vec())],
        );
        let model = office_to_model(&bytes).expect("docx → model");
        let lists = docx_model_list_paragraphs(&model);
        assert_eq!(lists.len(), 1, "one list paragraph: {lists:?}");
        assert_eq!(
            lists[0],
            (true, ListMarker::UpperRoman, 0, "Item".to_string()),
            "w:lvlOverride numFmt (upperRoman) wins over the abstract's decimal"
        );
    }

    #[test]
    fn parse_numbering_folds_lvl_override_format() {
        // Direct unit check of the parser: the resolved per-level format reflects
        // the `w:lvlOverride` (lowerRoman), not the abstract's decimal.
        let numbering = r#"<w:numbering xmlns:w="x">
          <w:abstractNum w:abstractNumId="0">
            <w:lvl w:ilvl="0"><w:numFmt w:val="decimal"/></w:lvl>
            <w:lvl w:ilvl="1"><w:numFmt w:val="lowerLetter"/></w:lvl>
          </w:abstractNum>
          <w:num w:numId="3">
            <w:abstractNumId w:val="0"/>
            <w:lvlOverride w:ilvl="1"><w:lvl w:ilvl="1"><w:numFmt w:val="lowerRoman"/></w:lvl></w:lvlOverride>
          </w:num>
        </w:numbering>"#;
        let n = parse_docx_numbering(numbering);
        // Level 0 unchanged (decimal); level 1 overridden (lowerRoman).
        assert_eq!(n.fmt(3, 0), Some(NumFmt::Decimal), "level 0 stays decimal");
        assert_eq!(
            n.fmt(3, 1),
            Some(NumFmt::LowerRoman),
            "level 1 overridden to lowerRoman"
        );
    }

    // ── DOCX / ODF → document outline (headings + bookmarks) (#31) ──

    /// Pre-order flatten of an [`OutlineNode`] tree into `(depth, title, page)`
    /// rows, where `depth` is the node's nesting depth (`0` = a root). Lets a
    /// nested outline be asserted as a flat, readable expectation list.
    fn flat_outline(nodes: &[crate::model::OutlineNode]) -> Vec<(usize, String, usize)> {
        fn go(
            nodes: &[crate::model::OutlineNode],
            depth: usize,
            out: &mut Vec<(usize, String, usize)>,
        ) {
            for n in nodes {
                out.push((depth, n.title.clone(), n.page));
                go(&n.children, depth + 1, out);
            }
        }
        let mut out = Vec::new();
        go(nodes, 0, &mut out);
        out
    }

    #[test]
    fn docx_outline_nests_headings_by_level() {
        // H1 / H2 / H2 / H1 → two roots, the first carrying two H2 children.
        let doc = r#"<w:document xmlns:w="x"><w:body>
          <w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Chapter 1</w:t></w:r></w:p>
          <w:p><w:pPr><w:pStyle w:val="Heading2"/></w:pPr><w:r><w:t>Section 1.1</w:t></w:r></w:p>
          <w:p><w:pPr><w:pStyle w:val="Heading2"/></w:pPr><w:r><w:t>Section 1.2</w:t></w:r></w:p>
          <w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Chapter 2</w:t></w:r></w:p>
        </w:body></w:document>"#;
        let model = office_to_model(&build_docx(doc, None, &[])).expect("docx → model");
        let tree = &model.outline;
        assert_eq!(tree.len(), 2, "two top-level chapters: {tree:?}");
        assert_eq!(tree[0].title, "Chapter 1");
        assert_eq!(tree[0].children.len(), 2, "Chapter 1 has two sections");
        assert_eq!(tree[0].children[0].title, "Section 1.1");
        assert_eq!(tree[0].children[1].title, "Section 1.2");
        assert_eq!(tree[1].title, "Chapter 2");
        assert!(tree[1].children.is_empty());
        // Single-page document: every entry targets page 0.
        assert!(
            flat_outline(tree).iter().all(|(_, _, page)| *page == 0),
            "all entries on page 0 (no page breaks): {tree:?}"
        );
    }

    #[test]
    fn docx_outline_skipped_level_nests_sensibly() {
        // H1 then H3 (no intervening H2): the H3 must still attach (under H1),
        // never be dropped — the shared folder clamps the jump.
        let doc = r#"<w:document xmlns:w="x"><w:body>
          <w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Top</w:t></w:r></w:p>
          <w:p><w:pPr><w:pStyle w:val="Heading3"/></w:pPr><w:r><w:t>Deep</w:t></w:r></w:p>
        </w:body></w:document>"#;
        let model = office_to_model(&build_docx(doc, None, &[])).expect("docx → model");
        let tree = &model.outline;
        assert_eq!(tree.len(), 1, "one root: {tree:?}");
        assert_eq!(tree[0].title, "Top");
        assert_eq!(tree[0].children.len(), 1, "the deep heading is not dropped");
        assert_eq!(tree[0].children[0].title, "Deep");
    }

    #[test]
    fn docx_outline_from_outline_lvl_without_heading_style() {
        // A paragraph with only `w:outlineLvl` (no heading `w:pStyle`) is still an
        // outline entry. `val=0`→top, `val=1`→nested. The block itself stays a
        // plain paragraph (no Heading promotion), but the outline records it.
        let doc = r#"<w:document xmlns:w="x"><w:body>
          <w:p><w:pPr><w:outlineLvl w:val="0"/></w:pPr><w:r><w:t>Part A</w:t></w:r></w:p>
          <w:p><w:pPr><w:outlineLvl w:val="1"/></w:pPr><w:r><w:t>Part A.1</w:t></w:r></w:p>
        </w:body></w:document>"#;
        let model = office_to_model(&build_docx(doc, None, &[])).expect("docx → model");
        assert_eq!(
            flat_outline(&model.outline),
            vec![(0, "Part A".to_string(), 0), (1, "Part A.1".to_string(), 0)],
        );
        // No heading style → the blocks remain paragraphs, not headings.
        let has_heading = model.sections[0].pages[0]
            .blocks
            .iter()
            .any(|b| matches!(b.kind, BlockKind::Heading(_)));
        assert!(
            !has_heading,
            "outlineLvl alone must not promote to a Heading"
        );
    }

    #[test]
    fn docx_outline_style_and_outline_lvl_take_the_more_prominent() {
        // A `Heading2` style (depth 1) that ALSO sets `w:outlineLvl=0` → the more
        // prominent depth (0) wins.
        let doc = r#"<w:document xmlns:w="x"><w:body>
          <w:p><w:pPr><w:pStyle w:val="Heading2"/><w:outlineLvl w:val="0"/></w:pPr><w:r><w:t>Promoted</w:t></w:r></w:p>
        </w:body></w:document>"#;
        let model = office_to_model(&build_docx(doc, None, &[])).expect("docx → model");
        assert_eq!(
            flat_outline(&model.outline),
            vec![(0, "Promoted".to_string(), 0)],
            "min(style depth 1, outlineLvl 0) = 0"
        );
    }

    #[test]
    fn docx_outline_tracks_page_across_hard_break() {
        // A hard page break between two H1s: the second heading targets page 1.
        let doc = r#"<w:document xmlns:w="x"><w:body>
          <w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>On Page One</w:t></w:r></w:p>
          <w:p><w:pPr><w:pStyle w:val="Heading1"/><w:pageBreakBefore/></w:pPr><w:r><w:t>On Page Two</w:t></w:r></w:p>
        </w:body></w:document>"#;
        let model = office_to_model(&build_docx(doc, None, &[])).expect("docx → model");
        assert_eq!(
            flat_outline(&model.outline),
            vec![
                (0, "On Page One".to_string(), 0),
                (0, "On Page Two".to_string(), 1),
            ],
        );
    }

    #[test]
    fn docx_outline_user_bookmark_nests_under_heading_internal_machinery_dropped() {
        // A user bookmark (`Anchor1`) inside a section nests under the current
        // H1; a Word-internal bookmark (`_Toc1`, `_GoBack`) is dropped.
        let doc = r#"<w:document xmlns:w="x"><w:body>
          <w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:bookmarkStart w:id="0" w:name="_Toc1"/><w:r><w:t>Intro</w:t></w:r></w:p>
          <w:p><w:bookmarkStart w:id="1" w:name="Anchor1"/><w:bookmarkStart w:id="2" w:name="_GoBack"/><w:r><w:t>body text</w:t></w:r></w:p>
        </w:body></w:document>"#;
        let model = office_to_model(&build_docx(doc, None, &[])).expect("docx → model");
        assert_eq!(
            flat_outline(&model.outline),
            vec![(0, "Intro".to_string(), 0), (1, "Anchor1".to_string(), 0)],
            "user bookmark nests under the heading; _Toc/_GoBack dropped"
        );
    }

    #[test]
    fn docx_outline_empty_when_no_headings() {
        // A body with paragraphs but no headings/bookmarks → an empty outline
        // (and no panic).
        let doc = r#"<w:document xmlns:w="x"><w:body>
          <w:p><w:r><w:t>just a paragraph</w:t></w:r></w:p>
          <w:p><w:r><w:t>another one</w:t></w:r></w:p>
        </w:body></w:document>"#;
        let model = office_to_model(&build_docx(doc, None, &[])).expect("docx → model");
        assert!(model.outline.is_empty(), "no headings → empty outline");
    }

    #[test]
    fn docx_outline_heading_title_flattens_runs_and_links() {
        // A heading split across runs (and a hyperlink run) yields the joined
        // text as its outline title.
        let doc = r#"<w:document xmlns:w="x" xmlns:r="z"><w:body>
          <w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr>
            <w:r><w:t>Part </w:t></w:r>
            <w:hyperlink r:id="rId1"><w:r><w:t>One</w:t></w:r></w:hyperlink>
          </w:p>
        </w:body></w:document>"#;
        let rels = r#"<Relationships xmlns="x">
          <Relationship Id="rId1" Type="hyperlink" Target="https://e/" TargetMode="External"/>
        </Relationships>"#;
        let model = office_to_model(&build_docx(doc, Some(rels), &[])).expect("docx → model");
        assert_eq!(
            flat_outline(&model.outline),
            vec![(0, "Part One".to_string(), 0)],
        );
    }

    // ── ODF → model (links / strike / highlight / inline images / formulas / groups) ──

    /// Build an ODT zip from a `content.xml` body and optional `(path, bytes)`
    /// media parts, returning the lowered [`Document`].
    fn odt_model(content: &str, media: &[(&str, Vec<u8>)]) -> Document {
        let mut z = ZipWriter::new();
        z.add_stored("mimetype", b"application/vnd.oasis.opendocument.text");
        z.add_stored("content.xml", content.as_bytes());
        for (path, bytes) in media {
            z.add_stored(path, bytes);
        }
        office_to_model(&z.finish()).expect("odt → model")
    }

    #[test]
    fn odt_model_hyperlink_becomes_link() {
        // `<text:a xlink:href>` → `Inline::Link` (external URL) wrapping its runs.
        let content = r#"<office:document-content xmlns:office="o" xmlns:text="t" xmlns:xlink="xl">
  <office:body><office:text>
    <text:p>see <text:a xlink:href="https://example.com/">our site</text:a> now</text:p>
  </office:text></office:body>
</office:document-content>"#;
        let model = odt_model(content, &[]);
        let (href, children) = model_first_section_inlines(&model)
            .into_iter()
            .find_map(|i| match i {
                Inline::Link { href, children } => Some((href, children)),
                _ => None,
            })
            .expect("an Inline::Link in the model");
        assert_eq!(
            href,
            model::LinkTarget::Url("https://example.com/".to_string())
        );
        let link_text: String = children
            .iter()
            .filter_map(|c| match c {
                Inline::Run(r) => Some(r.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(link_text, "our site");
    }

    #[test]
    fn odt_model_internal_anchor_link_targets_document_start() {
        // A `#bookmark` reference has no model-addressable page → page 0 (matches
        // the DOCX `w:anchor` behaviour), and never drops the link.
        let content = r##"<office:document-content xmlns:office="o" xmlns:text="t" xmlns:xlink="xl">
  <office:body><office:text>
    <text:p><text:a xlink:href="#Section2">jump</text:a></text:p>
  </office:text></office:body>
</office:document-content>"##;
        let model = odt_model(content, &[]);
        let href = model_first_section_inlines(&model)
            .into_iter()
            .find_map(|i| match i {
                Inline::Link { href, .. } => Some(href),
                _ => None,
            })
            .expect("an Inline::Link");
        assert_eq!(href, model::LinkTarget::Page(0));
    }

    #[test]
    fn odt_model_strike_and_highlight_from_span_style() {
        // `style:text-line-through-style` ≠ none → CharStyle.strike;
        // `fo:background-color` on the text style → CharStyle.background.
        let content = r##"<office:document-content xmlns:office="o" xmlns:text="t" xmlns:style="s" xmlns:fo="f">
  <office:automatic-styles>
    <style:style style:name="T1" style:family="text">
      <style:text-properties style:text-line-through-style="solid" fo:background-color="#00FF00"/>
    </style:style>
  </office:automatic-styles>
  <office:body><office:text>
    <text:p>x <text:span text:style-name="T1">marked</text:span> y</text:p>
  </office:text></office:body>
</office:document-content>"##;
        let model = odt_model(content, &[]);
        let run = model_first_section_inlines(&model)
            .into_iter()
            .find_map(|i| match i {
                Inline::Run(r) if r.text == "marked" => Some(r),
                _ => None,
            })
            .expect("the 'marked' run");
        assert!(run.style.strike, "line-through carried into the model");
        assert_eq!(
            run.style.background,
            Some([0.0, 1.0, 0.0]),
            "fo:background-color → RGB highlight"
        );
        // A run outside the span keeps the defaults (regression guard).
        let plain = model_first_section_inlines(&model)
            .into_iter()
            .find_map(|i| match i {
                Inline::Run(r) if r.text.trim() == "x" => Some(r),
                _ => None,
            })
            .expect("the leading run");
        assert!(!plain.style.strike && plain.style.background.is_none());
    }

    #[test]
    fn odt_model_inline_image_lands_in_resources() {
        // `<draw:frame><draw:image xlink:href>` inside a paragraph → an
        // `Inline::Image` whose bytes are interned in `Document.resources`.
        let content = r#"<office:document-content xmlns:office="o" xmlns:text="t" xmlns:draw="d" xmlns:xlink="xl">
  <office:body><office:text>
    <text:p>pic <draw:frame><draw:image xlink:href="Pictures/p.png"/></draw:frame></text:p>
  </office:text></office:body>
</office:document-content>"#;
        let model = odt_model(content, &[("Pictures/p.png", red_png())]);
        let img = model_first_section_inlines(&model)
            .into_iter()
            .find_map(|i| match i {
                Inline::Image(r) => Some(r),
                _ => None,
            })
            .expect("an Inline::Image in the model");
        assert!(
            model.resources.images.contains_key(&img.resource),
            "image bytes interned in the resource table"
        );
        assert_eq!(model.resources.images[&img.resource].format, "png");
    }

    /// The first paragraph/heading block's [`ParagraphStyle`] (text boxes expose
    /// their first paragraph's), for the ODT paragraph-formatting tests.
    fn first_para_style(doc: &Document) -> ParagraphStyle {
        fn find(blocks: &[Block]) -> Option<ParagraphStyle> {
            for b in blocks {
                match &b.kind {
                    BlockKind::Paragraph(p) => return Some(p.style.clone()),
                    BlockKind::Heading(h) => return Some(h.para.style.clone()),
                    BlockKind::TextBox(tb) => {
                        if let Some(s) = find(&tb.blocks) {
                            return Some(s);
                        }
                    }
                    _ => {}
                }
            }
            None
        }
        find(&doc.sections[0].pages[0].blocks).expect("a paragraph/heading block")
    }

    #[test]
    fn odt_model_paragraph_style_alignment_spacing_indent() {
        // `text:p@text:style-name` → a `style:paragraph-properties` with
        // `fo:text-align`/`fo:margin-*`/`fo:text-indent` lowers onto the model
        // paragraph's `ParagraphStyle` (ODF lengths converted to points).
        let content = r#"<office:document-content xmlns:office="o" xmlns:text="t" xmlns:style="s" xmlns:fo="f">
  <office:automatic-styles>
    <style:style style:name="P1" style:family="paragraph">
      <style:paragraph-properties fo:text-align="center" fo:margin-top="0.5cm" fo:margin-bottom="6pt" fo:margin-left="1cm" fo:text-indent="0.25in"/>
    </style:style>
  </office:automatic-styles>
  <office:body><office:text>
    <text:p text:style-name="P1">Centered</text:p>
  </office:text></office:body>
</office:document-content>"#;
        let style = first_para_style(&odt_model(content, &[]));
        assert_eq!(style.align, model::style::Align::Center);
        assert!(
            (style.space_before_pt - 14.1732).abs() < 0.01,
            "0.5cm → ~14.17pt (got {})",
            style.space_before_pt
        );
        assert!((style.space_after_pt - 6.0).abs() < 0.001);
        assert!(
            (style.indent_left_pt - 28.3465).abs() < 0.01,
            "1cm → ~28.35pt (got {})",
            style.indent_left_pt
        );
        assert!(
            (style.first_line_pt - 18.0).abs() < 0.001,
            "0.25in → 18pt (got {})",
            style.first_line_pt
        );
    }

    #[test]
    fn odt_model_paragraph_line_height_percent_and_justify() {
        // `fo:line-height="150%"` → a unitless multiple (1.5); `fo:text-align`
        // `justify` → justified.
        let content = r#"<office:document-content xmlns:office="o" xmlns:text="t" xmlns:style="s" xmlns:fo="f">
  <office:automatic-styles>
    <style:style style:name="P1" style:family="paragraph">
      <style:paragraph-properties fo:text-align="justify" fo:line-height="150%"/>
    </style:style>
  </office:automatic-styles>
  <office:body><office:text>
    <text:p text:style-name="P1">Body</text:p>
  </office:text></office:body>
</office:document-content>"#;
        let style = first_para_style(&odt_model(content, &[]));
        assert_eq!(style.align, model::style::Align::Justify);
        assert_eq!(style.line_height, model::style::LineHeight::Multiple(1.5));
    }

    #[test]
    fn odt_model_paragraph_style_inherits_from_parent() {
        // A derived style fills its gaps from `style:parent-style-name`: the
        // child sets only the alignment, the parent supplies the left margin.
        let content = r#"<office:document-content xmlns:office="o" xmlns:text="t" xmlns:style="s" xmlns:fo="f">
  <office:styles>
    <style:style style:name="Base" style:family="paragraph">
      <style:paragraph-properties fo:margin-left="2cm"/>
    </style:style>
  </office:styles>
  <office:automatic-styles>
    <style:style style:name="P1" style:family="paragraph" style:parent-style-name="Base">
      <style:paragraph-properties fo:text-align="end"/>
    </style:style>
  </office:automatic-styles>
  <office:body><office:text>
    <text:p text:style-name="P1">Derived</text:p>
  </office:text></office:body>
</office:document-content>"#;
        let style = first_para_style(&odt_model(content, &[]));
        assert_eq!(style.align, model::style::Align::Right, "child override");
        assert!(
            (style.indent_left_pt - 56.6929).abs() < 0.01,
            "2cm inherited from the parent (got {})",
            style.indent_left_pt
        );
    }

    #[test]
    fn odt_model_named_style_table_lowered_and_style_ref_resolves() {
        // `styles.xml`'s `office:styles` declares a `Heading_20_1` paragraph
        // style (parent `Standard`, bold, centred, 16pt). It must lower to a
        // `NamedStyle` in `Document.styles` with `style:parent-style-name` kept
        // as `based_on`. A `text:p text:style-name="Heading_20_1"` must carry a
        // `style_ref` that resolves into the table. A `text` (run) family style
        // is not lowered into the paragraph style table.
        let styles_xml = r#"<?xml version="1.0"?>
<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
  xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
  xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0">
  <office:styles>
    <style:style style:name="Standard" style:family="paragraph">
      <style:paragraph-properties fo:margin-left="1cm"/>
    </style:style>
    <style:style style:name="Heading_20_1" style:display-name="Heading 1"
        style:family="paragraph" style:parent-style-name="Standard">
      <style:paragraph-properties fo:text-align="center"/>
      <style:text-properties fo:font-weight="bold" fo:font-size="16pt"/>
    </style:style>
    <style:style style:name="Strong" style:family="text">
      <style:text-properties fo:font-weight="bold"/>
    </style:style>
  </office:styles>
</office:document-styles>"#;
        let content = r#"<office:document-content xmlns:office="o" xmlns:text="t">
  <office:body><office:text>
    <text:h text:style-name="Heading_20_1" text:outline-level="1">Big Title</text:h>
  </office:text></office:body>
</office:document-content>"#;
        let model = odt_model(content, &[("styles.xml", styles_xml.as_bytes().to_vec())]);

        let h1 = model
            .styles
            .named
            .get(&model::StyleId("Heading_20_1".to_string()))
            .expect("Heading_20_1 lowered into Document.styles");
        assert_eq!(
            h1.based_on,
            Some(model::StyleId("Standard".to_string())),
            "style:parent-style-name → based_on (kept, not flattened)"
        );
        assert!(h1.char_.bold, "fo:font-weight bold → bold");
        assert!((h1.char_.size_pt - 16.0).abs() < 1e-6, "fo:font-size 16pt");
        assert_eq!(h1.para.align, model::style::Align::Center, "fo:text-align");
        assert!(
            model
                .styles
                .named
                .contains_key(&model::StyleId("Standard".to_string())),
            "Standard lowered too"
        );
        assert!(
            !model
                .styles
                .named
                .contains_key(&model::StyleId("Strong".to_string())),
            "text (run) family style not lowered into the paragraph style table"
        );

        // The heading paragraph's style_ref points at the named style and resolves.
        let para = model.sections[0].pages[0]
            .blocks
            .iter()
            .find_map(|b| match &b.kind {
                BlockKind::Heading(h) => Some(&h.para),
                BlockKind::Paragraph(p) => Some(p),
                _ => None,
            })
            .expect("a heading/paragraph block");
        let sref = para
            .style_ref
            .clone()
            .expect("style_ref set from text:style-name");
        assert_eq!(sref, model::StyleId("Heading_20_1".to_string()));
        assert!(
            model.styles.named.contains_key(&sref),
            "style_ref must resolve into Document.styles (no dangling id)"
        );
    }

    #[test]
    fn odt_model_no_styles_xml_yields_empty_style_table() {
        // An ODT without a `styles.xml` part must lower to an empty style table
        // (no panic, no spurious entries).
        let content = r#"<office:document-content xmlns:office="o" xmlns:text="t">
  <office:body><office:text><text:p>Plain</text:p></office:text></office:body>
</office:document-content>"#;
        let model = odt_model(content, &[]);
        assert!(
            model.styles.named.is_empty(),
            "no styles.xml ⇒ empty style table, got {:?}",
            model.styles.named
        );
    }

    #[test]
    fn odt_model_footnote_text_and_citation_inlined() {
        // A `text:note` (footnote) inlines its citation marker and body text at
        // the reference point so neither is dropped.
        let content = r#"<office:document-content xmlns:office="o" xmlns:text="t">
  <office:body><office:text>
    <text:p>Claim<text:note text:note-class="footnote"><text:note-citation>1</text:note-citation><text:note-body><text:p>The source.</text:p></text:note-body></text:note> stands.</text:p>
  </office:text></office:body>
</office:document-content>"#;
        let model = odt_model(content, &[]);
        let text: String = model_first_section_inlines(&model)
            .iter()
            .filter_map(|i| match i {
                Inline::Run(r) => Some(r.text.as_str()),
                _ => None,
            })
            .collect();
        assert!(text.contains('1'), "citation marker kept: {text:?}");
        assert!(text.contains("The source."), "note body kept: {text:?}");
        assert!(text.contains("Claim"), "host text kept: {text:?}");
        // The citation marker is surfaced as a superscript run.
        let super_run = model_first_section_inlines(&model).into_iter().any(|i| {
            matches!(i, Inline::Run(r)
                if r.text == "1" && r.style.vertical_align == model::style::VAlign::Super)
        });
        assert!(super_run, "citation rendered as a superscript run");
    }

    #[test]
    fn odt_model_body_text_box_becomes_textbox_block() {
        // A body `draw:frame`/`draw:text-box` → a `BlockKind::TextBox` carrying
        // its inner paragraphs, whether anchored in a paragraph or a sibling.
        let content = r#"<office:document-content xmlns:office="o" xmlns:text="t" xmlns:draw="d">
  <office:body><office:text>
    <text:p>Lead<draw:frame><draw:text-box><text:p>Boxed note</text:p></draw:text-box></draw:frame></text:p>
  </office:text></office:body>
</office:document-content>"#;
        let model = odt_model(content, &[]);
        let tb = model.sections[0].pages[0]
            .blocks
            .iter()
            .find(|b| matches!(b.kind, BlockKind::TextBox(_)))
            .expect("a TextBox block from the draw:text-box");
        assert_eq!(block_text(tb), "Boxed note");
        // The host paragraph's own text is still present as a sibling block.
        let para_text: String = model.sections[0].pages[0]
            .blocks
            .iter()
            .filter(|b| matches!(b.kind, BlockKind::Paragraph(_)))
            .map(block_text)
            .collect();
        assert!(
            para_text.contains("Lead"),
            "host paragraph kept: {para_text:?}"
        );
    }

    #[test]
    fn odt_model_table_column_widths_and_cell_shading() {
        // `table:table-column@table:style-name` → `style:column-width` fills the
        // table's `col_widths`; a cell style's `fo:background-color` → the cell's
        // shading.
        let content = r##"<office:document-content xmlns:office="o" xmlns:text="t" xmlns:table="tb" xmlns:style="s" xmlns:fo="f">
  <office:automatic-styles>
    <style:style style:name="co1" style:family="table-column">
      <style:table-column-properties style:column-width="3cm"/>
    </style:style>
    <style:style style:name="co2" style:family="table-column">
      <style:table-column-properties style:column-width="5cm"/>
    </style:style>
    <style:style style:name="ceShade" style:family="table-cell">
      <style:table-cell-properties fo:background-color="#FF0000"/>
    </style:style>
  </office:automatic-styles>
  <office:body><office:text>
    <table:table>
      <table:table-column table:style-name="co1"/>
      <table:table-column table:style-name="co2"/>
      <table:table-row>
        <table:table-cell table:style-name="ceShade"><text:p>A</text:p></table:table-cell>
        <table:table-cell><text:p>B</text:p></table:table-cell>
      </table:table-row>
    </table:table>
  </office:text></office:body>
</office:document-content>"##;
        let model = odt_model(content, &[]);
        let table = model.sections[0].pages[0]
            .blocks
            .iter()
            .find_map(|b| match &b.kind {
                BlockKind::Table(t) => Some(t),
                _ => None,
            })
            .expect("a Table block");
        assert_eq!(table.col_widths.len(), 2, "both columns sized");
        assert!(
            (table.col_widths[0] - 85.0394).abs() < 0.01,
            "3cm → ~85.04pt"
        );
        assert!(
            (table.col_widths[1] - 141.7323).abs() < 0.01,
            "5cm → ~141.73pt"
        );
        let shaded = &table.rows[0].cells[0].shading;
        assert_eq!(*shaded, Some([1.0, 0.0, 0.0]), "first cell shaded red");
        assert_eq!(table.rows[0].cells[1].shading, None, "second cell unshaded");
    }

    // ── ODT → document outline (#31) ──

    #[test]
    fn odt_outline_nests_headings_by_level() {
        // `text:outline-level` 1/2/2/1 → two roots, the first with two children.
        // The whole ODT is one page, so every entry targets page 0.
        let content = r#"<office:document-content xmlns:office="o" xmlns:text="t">
  <office:body><office:text>
    <text:h text:outline-level="1">Chapter 1</text:h>
    <text:h text:outline-level="2">Section 1.1</text:h>
    <text:h text:outline-level="2">Section 1.2</text:h>
    <text:h text:outline-level="1">Chapter 2</text:h>
  </office:text></office:body>
</office:document-content>"#;
        let model = odt_model(content, &[]);
        assert_eq!(
            flat_outline(&model.outline),
            vec![
                (0, "Chapter 1".to_string(), 0),
                (1, "Section 1.1".to_string(), 0),
                (1, "Section 1.2".to_string(), 0),
                (0, "Chapter 2".to_string(), 0),
            ],
        );
        // Cross-check the nested shape (not just the pre-order flattening).
        assert_eq!(model.outline.len(), 2);
        assert_eq!(model.outline[0].children.len(), 2);
    }

    #[test]
    fn odt_outline_skipped_level_nests_sensibly() {
        // outline-level 1 then 3 (no 2): the deep heading attaches under the H1,
        // never dropped.
        let content = r#"<office:document-content xmlns:office="o" xmlns:text="t">
  <office:body><office:text>
    <text:h text:outline-level="1">Top</text:h>
    <text:h text:outline-level="3">Deep</text:h>
  </office:text></office:body>
</office:document-content>"#;
        let model = odt_model(content, &[]);
        assert_eq!(model.outline.len(), 1);
        assert_eq!(model.outline[0].title, "Top");
        assert_eq!(model.outline[0].children.len(), 1);
        assert_eq!(model.outline[0].children[0].title, "Deep");
    }

    #[test]
    fn odt_outline_bookmark_nests_under_heading() {
        // A `text:bookmark-start`/`text:bookmark` named anchor inside a paragraph
        // nests under the preceding heading; a `_`-prefixed name is dropped.
        let content = r#"<office:document-content xmlns:office="o" xmlns:text="t">
  <office:body><office:text>
    <text:h text:outline-level="1">Intro</text:h>
    <text:p><text:bookmark-start text:name="Anchor1"/>see here<text:bookmark-end text:name="Anchor1"/></text:p>
    <text:p><text:bookmark text:name="_Internal"/>machinery</text:p>
  </office:text></office:body>
</office:document-content>"#;
        let model = odt_model(content, &[]);
        assert_eq!(
            flat_outline(&model.outline),
            vec![(0, "Intro".to_string(), 0), (1, "Anchor1".to_string(), 0)],
            "user bookmark nests under the heading; _Internal dropped"
        );
    }

    #[test]
    fn odt_outline_empty_when_no_headings() {
        // Paragraphs only, no headings/bookmarks → empty outline, no panic.
        let content = r#"<office:document-content xmlns:office="o" xmlns:text="t">
  <office:body><office:text>
    <text:p>just text</text:p>
    <text:p>more text</text:p>
  </office:text></office:body>
</office:document-content>"#;
        let model = odt_model(content, &[]);
        assert!(model.outline.is_empty(), "no headings → empty outline");
    }

    #[test]
    fn ods_model_preserves_formula_with_cached_value() {
        // `table:formula="of:=SUM([.A1:.A2])"` + `office:value="30"`: the model
        // keeps the bare expression AND the cached numeric result.
        let content = r#"<office:document-content xmlns:office="o" xmlns:table="tb" xmlns:text="t">
  <office:body><office:spreadsheet>
    <table:table table:name="Calc">
      <table:table-row><table:table-cell office:value-type="float" office:value="10"><text:p>10</text:p></table:table-cell></table:table-row>
      <table:table-row><table:table-cell office:value-type="float" office:value="20"><text:p>20</text:p></table:table-cell></table:table-row>
      <table:table-row><table:table-cell table:formula="of:=SUM([.A1:.A2])" office:value-type="float" office:value="30"><text:p>30</text:p></table:table-cell></table:table-row>
    </table:table>
  </office:spreadsheet></office:body>
</office:document-content>"#;
        let mut z = ZipWriter::new();
        z.add_stored(
            "mimetype",
            b"application/vnd.oasis.opendocument.spreadsheet",
        );
        z.add_stored("content.xml", content.as_bytes());
        let model = office_to_model(&z.finish()).expect("ods → model");

        let sheet = match &model.sections[0].pages[0].blocks[0].kind {
            BlockKind::Sheet(sb) => &sb.sheets[0],
            other => panic!("expected a Sheet block, got {other:?}"),
        };
        let cell = &sheet.rows[2].cells[0];
        assert_eq!(
            cell.formula.as_deref(),
            Some("SUM([.A1:.A2])"),
            "of: prefix and leading = stripped"
        );
        assert_eq!(cell.value, model::CellValue::Number(30.0));
        // A literal cell carries no formula (regression guard).
        assert!(sheet.rows[0].cells[0].formula.is_none());
    }

    /// Zip a `content.xml` (+ optional `styles.xml`) ODS and lower it to its first
    /// model [`Sheet`].
    fn ods_sheet(content: &str, styles_xml: Option<&str>) -> Sheet {
        let mut z = ZipWriter::new();
        z.add_stored(
            "mimetype",
            b"application/vnd.oasis.opendocument.spreadsheet",
        );
        z.add_stored("content.xml", content.as_bytes());
        if let Some(s) = styles_xml {
            z.add_stored("styles.xml", s.as_bytes());
        }
        let model = office_to_model(&z.finish()).expect("ods → model");
        match &model.sections[0].pages[0].blocks[0].kind {
            BlockKind::Sheet(sb) => sb.sheets[0].clone(),
            other => panic!("expected a Sheet block, got {other:?}"),
        }
    }

    #[test]
    fn ods_model_resolves_per_cell_style_and_number_format() {
        // `ce1` = bold + red + Arial 12 + thin black border + yellow fill +
        // centred, with a 2-decimal grouped number format `N2`. The styled cell
        // must carry every typed field; the plain cell stays default.
        let content = r##"<office:document-content xmlns:office="o" xmlns:table="tb" xmlns:text="t" xmlns:style="st" xmlns:fo="fo" xmlns:number="nb">
  <office:automatic-styles>
    <number:number-style style:name="N2">
      <number:number number:decimal-places="2" number:grouping="true"/>
    </number:number-style>
    <style:style style:name="ce1" style:family="table-cell" style:data-style-name="N2">
      <style:table-cell-properties fo:border="0.75pt solid #000000" fo:background-color="#FFFF00"/>
      <style:text-properties fo:font-weight="bold" fo:color="#FF0000" fo:font-size="12pt" style:font-name="Arial"/>
      <style:paragraph-properties fo:text-align="center"/>
    </style:style>
  </office:automatic-styles>
  <office:body><office:spreadsheet>
    <table:table table:name="Sheet1">
      <table:table-row>
        <table:table-cell table:style-name="ce1" office:value-type="float" office:value="1234.5"><text:p>1,234.50</text:p></table:table-cell>
        <table:table-cell><text:p>Plain</text:p></table:table-cell>
      </table:table-row>
    </table:table>
  </office:spreadsheet></office:body>
</office:document-content>"##;
        let sheet = ods_sheet(content, None);
        let styled = &sheet.rows[0].cells[0];
        // Value typed from office:value (not the locale-formatted display text).
        assert_eq!(styled.value, model::CellValue::Number(1234.5));
        // Number format reconstructed from the number:number-style.
        assert_eq!(styled.number_format.as_deref(), Some("#,##0.00"));
        // Font (text-properties) → CharStyle.
        assert!(styled.style.bold, "bold");
        assert_eq!(styled.style.color, Some([1.0, 0.0, 0.0]), "red");
        assert!((styled.style.size_pt - 12.0).abs() < 1e-6, "12pt");
        assert_eq!(styled.style.family, "Arial");
        // Fill, border, alignment.
        assert_eq!(styled.fill, Some([1.0, 1.0, 0.0]), "yellow fill");
        let b = styled.border.expect("border set");
        assert!((b.width - 0.75).abs() < 1e-6, "border width: {}", b.width);
        assert_eq!(b.color, [0.0, 0.0, 0.0], "black border");
        assert_eq!(styled.align, Some(model::Align::Center));
        // The plain cell carries no styling at all (regression guard).
        let plain = &sheet.rows[0].cells[1];
        assert_eq!(plain.value, model::CellValue::Text("Plain".into()));
        assert!(plain.number_format.is_none() && plain.fill.is_none());
        assert!(plain.border.is_none() && plain.align.is_none());
        assert_eq!(plain.style, CharStyle::default());
    }

    #[test]
    fn ods_model_number_format_percentage_and_currency() {
        // A percentage style → `0.00%`; a currency style → `$#,##0.00`.
        let content = r##"<office:document-content xmlns:office="o" xmlns:table="tb" xmlns:text="t" xmlns:style="st" xmlns:fo="fo" xmlns:number="nb">
  <office:automatic-styles>
    <number:percentage-style style:name="P2">
      <number:number number:decimal-places="2"/>
      <number:text>%</number:text>
    </number:percentage-style>
    <number:currency-style style:name="C0">
      <number:currency-symbol>$</number:currency-symbol>
      <number:number number:decimal-places="2" number:grouping="true"/>
    </number:currency-style>
    <style:style style:name="cP" style:family="table-cell" style:data-style-name="P2"/>
    <style:style style:name="cC" style:family="table-cell" style:data-style-name="C0"/>
  </office:automatic-styles>
  <office:body><office:spreadsheet>
    <table:table table:name="S">
      <table:table-row>
        <table:table-cell table:style-name="cP" office:value-type="percentage" office:value="0.5"><text:p>50%</text:p></table:table-cell>
        <table:table-cell table:style-name="cC" office:value-type="currency" office:value="9"><text:p>$9.00</text:p></table:table-cell>
      </table:table-row>
    </table:table>
  </office:spreadsheet></office:body>
</office:document-content>"##;
        let sheet = ods_sheet(content, None);
        assert_eq!(
            sheet.rows[0].cells[0].number_format.as_deref(),
            Some("0.00%")
        );
        assert_eq!(
            sheet.rows[0].cells[1].number_format.as_deref(),
            Some("$#,##0.00")
        );
    }

    #[test]
    fn ods_model_spanned_cells_become_merge_ranges() {
        // A 2×2 anchor (`number-columns/rows-spanned="2"`) + the covered fillers.
        // The model records one MergeRange (0,0)..=(1,1).
        let content = r#"<office:document-content xmlns:office="o" xmlns:table="tb" xmlns:text="t">
  <office:body><office:spreadsheet>
    <table:table table:name="M">
      <table:table-row>
        <table:table-cell table:number-columns-spanned="2" table:number-rows-spanned="2"><text:p>Merged</text:p></table:table-cell>
        <table:covered-table-cell/>
      </table:table-row>
      <table:table-row>
        <table:covered-table-cell/>
        <table:covered-table-cell/>
      </table:table-row>
      <table:table-row>
        <table:table-cell><text:p>After</text:p></table:table-cell>
      </table:table-row>
    </table:table>
  </office:spreadsheet></office:body>
</office:document-content>"#;
        let sheet = ods_sheet(content, None);
        assert_eq!(sheet.merges.len(), 1, "one merge range: {:?}", sheet.merges);
        assert_eq!(
            sheet.merges[0],
            model::MergeRange {
                r0: 0,
                c0: 0,
                r1: 1,
                c1: 1
            }
        );
        // The anchor keeps its text; the covered slot is present but empty.
        assert_eq!(
            sheet.rows[0].cells[0].value,
            model::CellValue::Text("Merged".into())
        );
        assert_eq!(sheet.rows[0].cells[1].value, model::CellValue::Empty);
        assert_eq!(
            sheet.rows[2].cells[0].value,
            model::CellValue::Text("After".into())
        );
    }

    #[test]
    fn ods_model_column_widths_and_row_heights() {
        // Two column styles (3cm, 1.5cm) and a 24pt row → col_widths + row height.
        let content = r#"<office:document-content xmlns:office="o" xmlns:table="tb" xmlns:text="t" xmlns:style="st">
  <office:automatic-styles>
    <style:style style:name="co1" style:family="table-column">
      <style:table-column-properties style:column-width="3cm"/>
    </style:style>
    <style:style style:name="co2" style:family="table-column">
      <style:table-column-properties style:column-width="1.5cm"/>
    </style:style>
    <style:style style:name="ro1" style:family="table-row">
      <style:table-row-properties style:row-height="24pt"/>
    </style:style>
  </office:automatic-styles>
  <office:body><office:spreadsheet>
    <table:table table:name="Sz">
      <table:table-column table:style-name="co1"/>
      <table:table-column table:style-name="co2"/>
      <table:table-row table:style-name="ro1">
        <table:table-cell><text:p>A</text:p></table:table-cell>
        <table:table-cell><text:p>B</text:p></table:table-cell>
      </table:table-row>
    </table:table>
  </office:spreadsheet></office:body>
</office:document-content>"#;
        let sheet = ods_sheet(content, None);
        let cm = 28.3464567;
        assert_eq!(sheet.col_widths.len(), 2, "two column widths");
        assert!((sheet.col_widths[0] - 3.0 * cm).abs() < 1e-3, "col0 3cm");
        assert!((sheet.col_widths[1] - 1.5 * cm).abs() < 1e-3, "col1 1.5cm");
        assert_eq!(sheet.rows[0].height, Some(24.0), "row height 24pt");
    }

    #[test]
    fn ods_model_inherits_column_default_style_and_wrap() {
        // A column default cell style (bold + wrap) reaches a cell without its own
        // style; a cell with its own style overrides; the named style lives in
        // styles.xml while the row sits in content.xml.
        let styles_xml = r#"<office:document-styles xmlns:office="o" xmlns:style="st" xmlns:fo="fo">
  <office:styles>
    <style:style style:name="ceWrap" style:family="table-cell">
      <style:table-cell-properties style:wrap-option="wrap"/>
      <style:text-properties fo:font-weight="bold"/>
    </style:style>
  </office:styles>
</office:document-styles>"#;
        let content = r#"<office:document-content xmlns:office="o" xmlns:table="tb" xmlns:text="t" xmlns:style="st" xmlns:fo="fo">
  <office:automatic-styles>
    <style:style style:name="ceItalic" style:family="table-cell">
      <style:text-properties fo:font-style="italic"/>
    </style:style>
  </office:automatic-styles>
  <office:body><office:spreadsheet>
    <table:table table:name="Inh">
      <table:table-column table:default-cell-style-name="ceWrap"/>
      <table:table-row>
        <table:table-cell><text:p>Inherited</text:p></table:table-cell>
        <table:table-cell table:style-name="ceItalic"><text:p>Own</text:p></table:table-cell>
      </table:table-row>
    </table:table>
  </office:spreadsheet></office:body>
</office:document-content>"#;
        let sheet = ods_sheet(content, Some(styles_xml));
        // Column default reaches cell 0 (bold + wrap, from styles.xml).
        assert!(sheet.rows[0].cells[0].style.bold, "inherited bold");
        assert!(sheet.rows[0].cells[0].wrap, "inherited wrap");
        // Cell 1's own style wins (italic, not the column default's bold).
        assert!(sheet.rows[0].cells[1].style.italic, "own italic");
        assert!(
            !sheet.rows[0].cells[1].style.bold,
            "own style overrides default"
        );
    }

    /// Build a one-page ODP (`width`×`height` cm slide) from a `draw:page` body and
    /// optional media, returning its lowered [`Slide`].
    fn odp_model_slide(page_body: &str, media: &[(&str, Vec<u8>)]) -> Slide {
        let content = format!(
            r#"<office:document-content xmlns:office="o" xmlns:draw="d" xmlns:text="t" xmlns:svg="sv" xmlns:xlink="xl" xmlns:style="s" xmlns:fo="f">
  <office:automatic-styles>
    <style:page-layout style:name="PL"><style:page-layout-properties fo:page-width="25.4cm" fo:page-height="19.05cm" fo:margin="0cm"/></style:page-layout>
  </office:automatic-styles>
  <office:body><office:presentation>
    <draw:page draw:name="p1">{page_body}</draw:page>
  </office:presentation></office:body>
</office:document-content>"#
        );
        let mut z = ZipWriter::new();
        z.add_stored(
            "mimetype",
            b"application/vnd.oasis.opendocument.presentation",
        );
        z.add_stored("content.xml", content.as_bytes());
        for (path, bytes) in media {
            z.add_stored(path, bytes);
        }
        let model = office_to_model(&z.finish()).expect("odp → model");
        match model.sections[0].pages[0].blocks[0].kind.clone() {
            BlockKind::Slide(sb) => sb.slides.into_iter().next().expect("one slide"),
            other => panic!("expected a Slide block, got {other:?}"),
        }
    }

    #[test]
    fn odp_model_positioned_frame_becomes_shape_with_geometry() {
        // A positioned `draw:frame` (svg:x=2cm, y=1cm, w=8cm, h=3cm) holding a
        // text box → a free shape carrying its lower-left-origin frame. Slide is
        // 25.4×19.05cm = 720×540pt; 1cm = 28.3464567pt.
        let slide = odp_model_slide(
            r#"<draw:frame svg:x="2cm" svg:y="1cm" svg:width="8cm" svg:height="3cm">
                 <draw:text-box><text:p>Boxed</text:p></draw:text-box>
               </draw:frame>"#,
            &[],
        );
        assert_eq!(slide.shapes.len(), 1, "positioned frame → free shape");
        let s = &slide.shapes[0];
        let f = s.frame.expect("frame set from svg geometry");
        let cm = 28.3464567;
        assert!((f.x - 2.0 * cm).abs() < 1e-3, "x: {}", f.x);
        assert!((f.w - 8.0 * cm).abs() < 1e-3, "w: {}", f.w);
        assert!((f.h - 3.0 * cm).abs() < 1e-3, "h: {}", f.h);
        // top y=1cm, h=3cm → lower-left y = 540 - (1+3)cm.
        let exp_y = 540.0 - (1.0 + 3.0) * cm;
        assert!((f.y - exp_y).abs() < 1e-3, "y: {} exp {}", f.y, exp_y);
        assert_eq!(block_text(s), "Boxed");
    }

    #[test]
    fn odp_model_frame_image_becomes_image_shape() {
        // A positioned frame wrapping a `draw:image` → an image shape, the bytes
        // interned in the document resources.
        let slide = odp_model_slide(
            r#"<draw:frame svg:x="1cm" svg:y="1cm" svg:width="4cm" svg:height="4cm">
                 <draw:image xlink:href="Pictures/x.png"/>
               </draw:frame>"#,
            &[("Pictures/x.png", red_png())],
        );
        assert_eq!(slide.shapes.len(), 1);
        let s = &slide.shapes[0];
        assert!(s.frame.is_some(), "image shape positioned");
        assert!(
            matches!(s.kind, BlockKind::Image(_)),
            "pure picture frame → Image block, got {:?}",
            s.kind
        );
    }

    #[test]
    fn odp_model_group_composes_child_frame_positions() {
        // A `draw:g` group translated to (10cm, 5cm) holds a frame at child
        // (1cm,1cm); the child's slide position is the group offset + its own.
        // BOTH a directly-placed frame and the grouped one appear (no drop).
        let slide = odp_model_slide(
            r#"<draw:frame svg:x="0cm" svg:y="0cm" svg:width="2cm" svg:height="2cm">
                 <draw:text-box><text:p>Top</text:p></draw:text-box>
               </draw:frame>
               <draw:g svg:x="10cm" svg:y="5cm">
                 <draw:frame svg:x="1cm" svg:y="1cm" svg:width="3cm" svg:height="3cm">
                   <draw:text-box><text:p>Grouped</text:p></draw:text-box>
                 </draw:frame>
               </draw:g>"#,
            &[],
        );
        assert_eq!(slide.shapes.len(), 2, "ungrouped + grouped both present");
        let cm = 28.3464567;
        let grouped = slide
            .shapes
            .iter()
            .find(|s| block_text(s) == "Grouped")
            .expect("the grouped shape");
        let f = grouped.frame.expect("grouped frame");
        // x = (10 + 1)cm; top y = (5 + 1)cm, h = 3cm → lower-left = 540 - (6+3)cm.
        assert!((f.x - 11.0 * cm).abs() < 1e-3, "x: {}", f.x);
        let exp_y = 540.0 - (6.0 + 3.0) * cm;
        assert!((f.y - exp_y).abs() < 1e-3, "y: {} exp {}", f.y, exp_y);
    }

    #[test]
    fn odp_model_frame_transform_rotation_into_block() {
        // `draw:transform="rotate(<rad>)"` on a positioned frame folds into the
        // model's CCW `Block.rotation`. ODF angles are radians CCW; 90° = π/2 and
        // snaps to the first-class `D90`.
        let slide = odp_model_slide(
            r#"<draw:frame svg:x="2cm" svg:y="1cm" svg:width="8cm" svg:height="3cm"
                          draw:transform="rotate(1.5707963267948966)">
                 <draw:text-box><text:p>Tilted</text:p></draw:text-box>
               </draw:frame>"#,
            &[],
        );
        assert_eq!(slide.shapes.len(), 1, "no presentation:class → free shape");
        assert_eq!(slide.shapes[0].rotation, crate::model::Rotation::D90);
        assert_eq!(block_text(&slide.shapes[0]), "Tilted");
    }

    #[test]
    fn odp_model_frame_transform_arbitrary_angle_and_translate_origin() {
        // A frame positioned solely through `draw:transform` (no svg:x/y), as
        // LibreOffice emits rotated frames: `rotate(0.785…) translate(4cm 8cm)`.
        // 0.7853981633974483 rad = 45° → free-form `Deg(45)`; the translate's
        // lower-left origin (Y up) becomes the model box top-left (Y down).
        let slide = odp_model_slide(
            r#"<draw:frame svg:width="6cm" svg:height="2cm"
                          draw:transform="rotate(0.7853981633974483) translate(4cm 8cm)">
                 <draw:text-box><text:p>Skewed</text:p></draw:text-box>
               </draw:frame>"#,
            &[],
        );
        assert_eq!(slide.shapes.len(), 1, "transform-only frame still placed");
        let s = &slide.shapes[0];
        match s.rotation {
            crate::model::Rotation::Deg(d) => assert!((d - 45.0).abs() < 1e-6, "deg: {d}"),
            other => panic!("expected Deg(45), got {other:?}"),
        }
        let f = s.frame.expect("frame from transform translate");
        let cm = 28.3464567;
        // translate x → box x; translate y is the LL origin (Y up): top = 8cm - 2cm,
        // lower-left model y = 540 - (top + h) = 540 - 8cm.
        assert!((f.x - 4.0 * cm).abs() < 1e-3, "x: {}", f.x);
        let exp_y = 540.0 - 8.0 * cm;
        assert!((f.y - exp_y).abs() < 1e-3, "y: {} exp {}", f.y, exp_y);
        assert_eq!(block_text(s), "Skewed");
    }

    #[test]
    fn odp_model_presentation_class_maps_placeholder_roles() {
        // `presentation:class` on positioned text frames maps to the model's
        // semantic `PlaceholderRole`: title→Title, subtitle→Subtitle, outline→Body,
        // and an unknown class is preserved verbatim as `Other`.
        let slide = odp_model_slide(
            r#"<draw:frame presentation:class="title" svg:x="1cm" svg:y="1cm" svg:width="20cm" svg:height="3cm">
                 <draw:text-box><text:p>The Title</text:p></draw:text-box>
               </draw:frame>
               <draw:frame presentation:class="subtitle" svg:x="1cm" svg:y="5cm" svg:width="20cm" svg:height="2cm">
                 <draw:text-box><text:p>The Subtitle</text:p></draw:text-box>
               </draw:frame>
               <draw:frame presentation:class="outline" svg:x="1cm" svg:y="8cm" svg:width="20cm" svg:height="6cm">
                 <draw:text-box><text:p>Bullet</text:p></draw:text-box>
               </draw:frame>
               <draw:frame presentation:class="footer" svg:x="1cm" svg:y="17cm" svg:width="20cm" svg:height="1cm">
                 <draw:text-box><text:p>Footer text</text:p></draw:text-box>
               </draw:frame>"#,
            &[],
        );
        // Text frames with a presentation:class become placeholders, not shapes.
        assert!(
            slide.shapes.is_empty(),
            "classed text frames → placeholders"
        );
        let role_of = |txt: &str| {
            slide
                .placeholders
                .iter()
                .find(|p| block_text(&p.block) == txt)
                .map(|p| p.role.clone())
                .unwrap_or_else(|| panic!("no placeholder {txt:?}"))
        };
        assert_eq!(role_of("The Title"), model::PlaceholderRole::Title);
        assert_eq!(role_of("The Subtitle"), model::PlaceholderRole::Subtitle);
        assert_eq!(role_of("Bullet"), model::PlaceholderRole::Body);
        assert_eq!(
            role_of("Footer text"),
            model::PlaceholderRole::Other("footer".to_string())
        );
    }

    #[test]
    fn odp_model_bare_paragraph_presentation_class_maps_role() {
        // A bare `text:p` under the page carrying `presentation:class` is tagged
        // with that role (not the default Body).
        let slide = odp_model_slide(
            r#"<text:p presentation:class="title">Bare Title</text:p>"#,
            &[],
        );
        assert_eq!(slide.placeholders.len(), 1);
        assert_eq!(slide.placeholders[0].role, model::PlaceholderRole::Title);
        assert_eq!(block_text(&slide.placeholders[0].block), "Bare Title");
    }

    #[test]
    fn odp_model_image_alt_from_svg_title_desc() {
        // `svg:title` (preferred) supplies the picture frame's `ImageRef.alt`.
        let slide = odp_model_slide(
            r#"<draw:frame svg:x="1cm" svg:y="1cm" svg:width="4cm" svg:height="4cm">
                 <svg:title>A red square</svg:title>
                 <svg:desc>Longer description</svg:desc>
                 <draw:image xlink:href="Pictures/x.png"/>
               </draw:frame>"#,
            &[("Pictures/x.png", red_png())],
        );
        assert_eq!(slide.shapes.len(), 1);
        match &slide.shapes[0].kind {
            BlockKind::Image(ir) => {
                assert_eq!(ir.alt.as_deref(), Some("A red square"), "svg:title wins");
            }
            other => panic!("expected an Image block, got {other:?}"),
        }
    }

    #[test]
    fn odp_model_image_alt_falls_back_to_draw_name() {
        // With no svg:title/desc, the frame's `draw:name` is used as alt text.
        let slide = odp_model_slide(
            r#"<draw:frame draw:name="Logo" svg:x="1cm" svg:y="1cm" svg:width="4cm" svg:height="4cm">
                 <draw:image xlink:href="Pictures/x.png"/>
               </draw:frame>"#,
            &[("Pictures/x.png", red_png())],
        );
        match &slide.shapes[0].kind {
            BlockKind::Image(ir) => assert_eq!(ir.alt.as_deref(), Some("Logo")),
            other => panic!("expected an Image block, got {other:?}"),
        }
    }

    #[test]
    fn odp_transform_helpers_parse_rotate_and_translate() {
        // Unit-level checks of the `draw:transform` value parser.
        assert_eq!(
            odp_transform_rotate_rad("rotate(1.5707963267948966) translate(2cm 3cm)"),
            Some(std::f64::consts::FRAC_PI_2)
        );
        assert_eq!(
            odp_transform_translate("rotate(0.5) translate(2cm 3cm)"),
            Some((2.0 * 28.3464567, 3.0 * 28.3464567))
        );
        // No rotate present → D0; only a translate present → no angle.
        assert_eq!(
            odp_transform_rotation(&[("draw:transform".into(), "translate(1cm 2cm)".into())]),
            crate::model::Rotation::D0
        );
        assert!(odp_transform_rotate_rad("translate(1cm 2cm)").is_none());
    }

    #[test]
    fn xlsx_model_preserves_formula_with_cached_value() {
        // A `<c><f>SUM(A1:A2)</f><v>30</v></c>` cell: the model keeps the formula
        // expression AND the cached numeric result (the display value).
        let mut z = ZipWriter::new();
        z.add_stored("[Content_Types].xml", b"<Types/>");
        z.add_stored(
            "xl/workbook.xml",
            br#"<workbook><sheets><sheet name="Calc" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
        );
        z.add_stored(
            "xl/worksheets/sheet1.xml",
            br#"<worksheet><sheetData>
              <row r="1"><c r="A1"><v>10</v></c></row>
              <row r="2"><c r="A2"><v>20</v></c></row>
              <row r="3"><c r="A3"><f>SUM(A1:A2)</f><v>30</v></c></row>
            </sheetData></worksheet>"#,
        );
        let xlsx = z.finish();
        let model = office_to_model(&xlsx).expect("xlsx → model");

        let sheet = match &model.sections[0].pages[0].blocks[0].kind {
            BlockKind::Sheet(sb) => &sb.sheets[0],
            other => panic!("expected a Sheet block, got {other:?}"),
        };
        // Row index 2 (A3), column 0.
        let cell = &sheet.rows[2].cells[0];
        assert_eq!(
            cell.formula.as_deref(),
            Some("SUM(A1:A2)"),
            "formula expression preserved"
        );
        assert_eq!(
            cell.value,
            model::CellValue::Number(30.0),
            "cached result kept as the display value"
        );
        // A literal cell carries no formula (regression guard).
        assert!(sheet.rows[0].cells[0].formula.is_none());
    }

    /// Build an XLSX from a `styles.xml` + `sheet1.xml` body (+ optional worksheet
    /// rels) and return the lowered first [`Sheet`] of the editable model.
    fn xlsx_model_sheet(styles: &str, sheet: &str, sheet_rels: Option<&str>) -> model::Sheet {
        let mut z = ZipWriter::new();
        z.add_stored("[Content_Types].xml", b"<Types/>");
        z.add_stored(
            "xl/workbook.xml",
            br#"<workbook><sheets><sheet name="S" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
        );
        if !styles.is_empty() {
            z.add_stored("xl/styles.xml", styles.as_bytes());
        }
        z.add_stored("xl/worksheets/sheet1.xml", sheet.as_bytes());
        if let Some(rels) = sheet_rels {
            z.add_stored("xl/worksheets/_rels/sheet1.xml.rels", rels.as_bytes());
        }
        let model = office_to_model(&z.finish()).expect("xlsx → model");
        match model.sections[0].pages[0].blocks[0].kind.clone() {
            BlockKind::Sheet(sb) => sb.sheets.into_iter().next().expect("one sheet"),
            other => panic!("expected a Sheet block, got {other:?}"),
        }
    }

    #[test]
    fn xlsx_model_reads_per_cell_font_border_alignment_wrap() {
        // fontId 1 = bold-italic-underline red Arial 14; borderId 1 = thin box;
        // the xf centres + wraps. The imported cell must carry all of them as
        // typed model values (style/border/align/wrap), not just CSS.
        let styles = r#"<styleSheet>
          <fonts count="2">
            <font><sz val="11"/><name val="Calibri"/></font>
            <font><b/><i/><u/><sz val="14"/><color rgb="FFFF0000"/><name val="Arial"/></font>
          </fonts>
          <fills count="1"><fill><patternFill patternType="none"/></fill></fills>
          <borders count="2">
            <border><left/><right/><top/><bottom/></border>
            <border>
              <left style="thin"><color rgb="FF000000"/></left>
              <bottom style="thin"><color rgb="FF000000"/></bottom>
            </border>
          </borders>
          <cellXfs count="2">
            <xf fontId="0" borderId="0"/>
            <xf fontId="1" borderId="1" applyFont="1" applyBorder="1" applyAlignment="1">
              <alignment horizontal="center" wrapText="1"/>
            </xf>
          </cellXfs>
        </styleSheet>"#;
        let sheet = r#"<worksheet><sheetData>
          <row r="1">
            <c r="A1" s="0" t="inlineStr"><is><t>Plain</t></is></c>
            <c r="B1" s="1" t="inlineStr"><is><t>Fancy</t></is></c>
          </row>
        </sheetData></worksheet>"#;
        let s = xlsx_model_sheet(styles, sheet, None);

        // Plain cell: default style, no border/align/wrap.
        let plain = &s.rows[0].cells[0];
        assert!(plain.border.is_none(), "plain no border");
        assert_eq!(plain.align, None, "plain general align");
        assert!(!plain.wrap, "plain no wrap");
        assert!(!plain.style.bold, "plain not bold");

        // Fancy cell carries the full typed styling.
        let fancy = &s.rows[0].cells[1];
        assert!(fancy.style.bold, "bold");
        assert!(fancy.style.italic, "italic");
        assert!(fancy.style.underline, "underline");
        assert_eq!(fancy.style.family, "Arial", "family");
        assert!((fancy.style.size_pt - 14.0).abs() < 1e-6, "size");
        assert_eq!(fancy.style.color, Some([1.0, 0.0, 0.0]), "red font");
        let border = fancy.border.expect("border set");
        assert!(border.width > 0.0, "border width: {}", border.width);
        assert_eq!(fancy.align, Some(MAlign::Center), "centered");
        assert!(fancy.wrap, "wrapped");
    }

    #[test]
    fn xlsx_model_expands_shared_formulas() {
        // C1 is the shared master `A1+B1` (si=0); C2/C3 are followers with empty
        // bodies. Each follower's formula is rebuilt with row-translated refs.
        let sheet = r#"<worksheet><sheetData>
          <row r="1">
            <c r="A1"><v>1</v></c><c r="B1"><v>2</v></c>
            <c r="C1"><f t="shared" ref="C1:C3" si="0">A1+B1</f><v>3</v></c>
          </row>
          <row r="2">
            <c r="A2"><v>4</v></c><c r="B2"><v>5</v></c>
            <c r="C2"><f t="shared" si="0"/><v>9</v></c>
          </row>
          <row r="3">
            <c r="A3"><v>7</v></c><c r="B3"><v>8</v></c>
            <c r="C3"><f t="shared" si="0"/><v>15</v></c>
          </row>
        </sheetData></worksheet>"#;
        let s = xlsx_model_sheet("", sheet, None);
        assert_eq!(
            s.rows[0].cells[2].formula.as_deref(),
            Some("A1+B1"),
            "master"
        );
        assert_eq!(
            s.rows[1].cells[2].formula.as_deref(),
            Some("A2+B2"),
            "follower row 2 translated"
        );
        assert_eq!(
            s.rows[2].cells[2].formula.as_deref(),
            Some("A3+B3"),
            "follower row 3 translated"
        );
        // Cached results are still kept as the display values.
        assert_eq!(s.rows[1].cells[2].value, model::CellValue::Number(9.0));
    }

    #[test]
    fn xlsx_model_reads_cell_hyperlinks() {
        // A1 → external URL (via rels r:id); B1 → in-workbook `#Sheet1!A1`.
        let sheet = r#"<worksheet>
          <sheetData>
            <row r="1">
              <c r="A1" t="inlineStr"><is><t>site</t></is></c>
              <c r="B1" t="inlineStr"><is><t>jump</t></is></c>
            </row>
          </sheetData>
          <hyperlinks>
            <hyperlink ref="A1" r:id="rId1"/>
            <hyperlink ref="B1" location="Sheet1!A1"/>
          </hyperlinks>
        </worksheet>"#;
        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
          <Relationship Id="rId1" Type="http://.../hyperlink" Target="https://example.com/" TargetMode="External"/>
        </Relationships>"#;
        let s = xlsx_model_sheet("", sheet, Some(rels));
        assert_eq!(
            s.rows[0].cells[0].hyperlink.as_deref(),
            Some("https://example.com/"),
            "external link"
        );
        assert_eq!(
            s.rows[0].cells[1].hyperlink.as_deref(),
            Some("#Sheet1!A1"),
            "in-workbook link"
        );
    }

    #[test]
    fn xlsx_model_keeps_date_number_format_code() {
        // A date cell (numFmtId 14 → `mm-dd-yy`) keeps its format code on the
        // model so re-export re-applies a date format; the serial is the value.
        let styles = r#"<styleSheet>
          <fonts count="1"><font/></fonts>
          <fills count="1"><fill><patternFill patternType="none"/></fill></fills>
          <borders count="1"><border/></borders>
          <cellXfs count="2">
            <xf/>
            <xf numFmtId="14" applyNumberFormat="1"/>
          </cellXfs>
        </styleSheet>"#;
        let sheet = r#"<worksheet><sheetData>
          <row r="1"><c r="A1" s="1"><v>45000</v></c></row>
        </sheetData></worksheet>"#;
        let s = xlsx_model_sheet(styles, sheet, None);
        let cell = &s.rows[0].cells[0];
        assert_eq!(
            cell.number_format.as_deref(),
            Some("mm-dd-yy"),
            "date fmt code"
        );
        assert_eq!(
            cell.value,
            model::CellValue::Number(45000.0),
            "serial value"
        );
    }

    #[test]
    fn translate_formula_refs_relative_and_absolute() {
        // Relative refs shift by (dc, dr); `$`-anchored components stay put.
        assert_eq!(translate_formula_refs("A1+B1", 0, 1), "A2+B2");
        assert_eq!(translate_formula_refs("A1+B1", 2, 0), "C1+D1");
        assert_eq!(translate_formula_refs("$A$1+B2", 1, 1), "$A$1+C3");
        assert_eq!(translate_formula_refs("$A1+A$1", 1, 1), "$A2+B$1");
        // Function names and ranges survive; only the cell refs move.
        assert_eq!(translate_formula_refs("SUM(A1:A3)", 1, 0), "SUM(B1:B3)");
        assert_eq!(col_to_letters(0), "A");
        assert_eq!(col_to_letters(26), "AA");
        assert_eq!(col_to_letters(27), "AB");
    }

    // ── PPTX → model (geometry / groups / charts / inheritance / SmartArt) ──

    /// Build a single-slide PPTX zip with a 960×540pt slide size and the given
    /// slide-XML body, returning the lowered [`Slide`].
    fn pptx_model_slide(slide_xml: &str) -> Slide {
        pptx_model_slide_parts(&[("ppt/slides/slide1.xml", slide_xml)], &[])
    }

    /// Build a PPTX zip from `(path, xml)` parts (the slide(s), layout/master,
    /// charts, diagrams, …) plus `(path, bytes)` raw parts, and return the FIRST
    /// slide of the lowered model.
    fn pptx_model_slide_parts(parts: &[(&str, &str)], raw: &[(&str, &[u8])]) -> Slide {
        let mut z = ZipWriter::new();
        z.add_stored("[Content_Types].xml", b"<Types/>");
        z.add_stored(
            "ppt/presentation.xml",
            br#"<p:presentation xmlns:p="p"><p:sldSz cx="12192000" cy="6858000"/></p:presentation>"#,
        );
        for (path, xml) in parts {
            z.add_stored(path, xml.as_bytes());
        }
        for (path, bytes) in raw {
            z.add_stored(path, bytes);
        }
        let pptx = z.finish();
        let model = office_to_model(&pptx).expect("pptx → model");
        match model.sections[0].pages[0].blocks[0].kind.clone() {
            BlockKind::Slide(sb) => sb.slides.into_iter().next().expect("one slide"),
            other => panic!("expected a Slide block, got {other:?}"),
        }
    }

    /// The concatenated plain text of a [`TextBox`]/paragraph block tree.
    fn block_text(b: &Block) -> String {
        fn walk(blocks: &[Block], out: &mut String) {
            for b in blocks {
                match &b.kind {
                    BlockKind::Paragraph(p) | BlockKind::Heading(Heading { para: p, .. }) => {
                        for r in &p.runs {
                            if let Inline::Run(run) = r {
                                out.push_str(&run.text);
                            }
                        }
                        out.push(' ');
                    }
                    BlockKind::TextBox(tb) => walk(&tb.blocks, out),
                    BlockKind::List(l) => {
                        for it in &l.items {
                            walk(&it.blocks, out);
                        }
                    }
                    _ => {}
                }
            }
        }
        let mut s = String::new();
        walk(std::slice::from_ref(b), &mut s);
        s.trim().to_string()
    }

    #[test]
    fn pptx_model_shape_xfrm_becomes_frame_and_rotation() {
        // off x=914400 (72pt) y=457200 (36pt); ext cx=1828800 (144pt) cy=914400
        // (72pt); rot=5400000 (90° clockwise). A non-placeholder text box → a
        // free shape carrying its lower-left-origin frame and CCW rotation.
        let slide = pptx_model_slide(
            r#"<p:sld xmlns:a="a" xmlns:p="p"><p:cSld><p:spTree>
              <p:sp>
                <p:spPr><a:xfrm rot="5400000"><a:off x="914400" y="457200"/><a:ext cx="1828800" cy="914400"/></a:xfrm></p:spPr>
                <p:txBody><a:p><a:r><a:t>Free Box</a:t></a:r></a:p></p:txBody>
              </p:sp>
            </p:spTree></p:cSld></p:sld>"#,
        );
        assert_eq!(slide.shapes.len(), 1, "non-placeholder → free shape");
        assert!(slide.placeholders.is_empty());
        let s = &slide.shapes[0];
        let f = s.frame.expect("frame set from a:xfrm");
        assert!((f.x - 72.0).abs() < 1e-6, "x: {}", f.x);
        // top-left y=36, h=72 → lower-left y = 540 - (36+72) = 432.
        assert!((f.y - 432.0).abs() < 1e-6, "y: {}", f.y);
        assert!((f.w - 144.0).abs() < 1e-6, "w: {}", f.w);
        assert!((f.h - 72.0).abs() < 1e-6, "h: {}", f.h);
        // 90° clockwise (OOXML) → 270° CCW (model).
        assert_eq!(s.rotation, crate::model::Rotation::D270);
        assert_eq!(block_text(s), "Free Box");
    }

    #[test]
    fn pptx_model_grouped_shapes_descend_with_composed_transform() {
        // A group placed at (100pt,100pt) with a 1:1 child coordinate space
        // (chOff=0, chExt=ext) holds two shapes; their child offsets map straight
        // to the slide surface, and BOTH appear (no silent drop of grouped
        // content). One shape is itself a nested group.
        let slide = pptx_model_slide(
            r#"<p:sld xmlns:a="a" xmlns:p="p"><p:cSld><p:spTree>
              <p:grpSp>
                <p:grpSpPr><a:xfrm>
                  <a:off x="1270000" y="1270000"/><a:ext cx="2540000" cy="2540000"/>
                  <a:chOff x="0" y="0"/><a:chExt cx="2540000" cy="2540000"/>
                </a:xfrm></p:grpSpPr>
                <p:sp>
                  <p:spPr><a:xfrm><a:off x="127000" y="127000"/><a:ext cx="635000" cy="635000"/></a:xfrm></p:spPr>
                  <p:txBody><a:p><a:r><a:t>Grouped A</a:t></a:r></a:p></p:txBody>
                </p:sp>
                <p:grpSp>
                  <p:grpSpPr><a:xfrm>
                    <a:off x="0" y="0"/><a:ext cx="1270000" cy="1270000"/>
                    <a:chOff x="0" y="0"/><a:chExt cx="1270000" cy="1270000"/>
                  </a:xfrm></p:grpSpPr>
                  <p:sp>
                    <p:spPr><a:xfrm><a:off x="635000" y="635000"/><a:ext cx="635000" cy="635000"/></a:xfrm></p:spPr>
                    <p:txBody><a:p><a:r><a:t>Nested B</a:t></a:r></a:p></p:txBody>
                  </p:sp>
                </p:grpSp>
              </p:grpSp>
            </p:spTree></p:cSld></p:sld>"#,
        );
        assert_eq!(slide.shapes.len(), 2, "both grouped shapes surface");
        let texts: Vec<String> = slide.shapes.iter().map(block_text).collect();
        assert!(texts.iter().any(|t| t == "Grouped A"), "A: {texts:?}");
        assert!(texts.iter().any(|t| t == "Nested B"), "B: {texts:?}");

        // "Grouped A" at child (10pt,10pt) → group off (100,100) → (110,110)
        // top-left; ext 50pt → lower-left y = 540 - (110 + 50) = 380.
        let a = slide
            .shapes
            .iter()
            .find(|s| block_text(s) == "Grouped A")
            .unwrap();
        let fa = a.frame.expect("grouped A has a frame");
        assert!((fa.x - 110.0).abs() < 1e-6, "A.x: {}", fa.x);
        assert!((fa.y - 380.0).abs() < 1e-6, "A.y: {}", fa.y);

        // "Nested B": inner group at outer-child (0,0) → outer (100,100); inner is
        // 1:1, B at (50pt,50pt) → (150,150) top-left; ext 50pt → y = 540-200 = 340.
        let b = slide
            .shapes
            .iter()
            .find(|s| block_text(s) == "Nested B")
            .unwrap();
        let fb = b.frame.expect("nested B has a frame");
        assert!((fb.x - 150.0).abs() < 1e-6, "B.x: {}", fb.x);
        assert!((fb.y - 340.0).abs() < 1e-6, "B.y: {}", fb.y);
    }

    #[test]
    fn pptx_model_chart_becomes_table_of_series() {
        // A graphicFrame referencing a chart part (via the slide rels) is lowered
        // to a Table: title row, category header, then one row per series — the
        // data stays editable instead of being dropped.
        let chart = r#"<c:chartSpace xmlns:c="c" xmlns:a="a">
          <c:chart>
            <c:title><c:tx><c:rich><a:p><a:r><a:t>Quarterly Sales</a:t></a:r></a:p></c:rich></c:tx></c:title>
            <c:plotArea>
              <c:barChart>
                <c:ser>
                  <c:tx><c:strRef><c:strCache><c:pt idx="0"><c:v>Region A</c:v></c:pt></c:strCache></c:strRef></c:tx>
                  <c:cat><c:strRef><c:strCache>
                    <c:pt idx="0"><c:v>Q1</c:v></c:pt><c:pt idx="1"><c:v>Q2</c:v></c:pt>
                  </c:strCache></c:strRef></c:cat>
                  <c:val><c:numRef><c:numCache>
                    <c:pt idx="0"><c:v>10</c:v></c:pt><c:pt idx="1"><c:v>20</c:v></c:pt>
                  </c:numCache></c:numRef></c:val>
                </c:ser>
                <c:ser>
                  <c:tx><c:strRef><c:strCache><c:pt idx="0"><c:v>Region B</c:v></c:pt></c:strCache></c:strRef></c:tx>
                  <c:val><c:numRef><c:numCache>
                    <c:pt idx="0"><c:v>30</c:v></c:pt><c:pt idx="1"><c:v>40</c:v></c:pt>
                  </c:numCache></c:numRef></c:val>
                </c:ser>
              </c:barChart>
            </c:plotArea>
          </c:chart>
        </c:chartSpace>"#;
        let slide = pptx_model_slide_parts(
            &[
                (
                    "ppt/slides/slide1.xml",
                    r#"<p:sld xmlns:a="a" xmlns:p="p" xmlns:r="r"><p:cSld><p:spTree>
                      <p:graphicFrame>
                        <p:xfrm><a:off x="635000" y="635000"/><a:ext cx="3810000" cy="2540000"/></p:xfrm>
                        <a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/chart">
                          <c:chart xmlns:c="c" r:id="rId1"/>
                        </a:graphicData></a:graphic>
                      </p:graphicFrame>
                    </p:spTree></p:cSld></p:sld>"#,
                ),
                (
                    "ppt/slides/_rels/slide1.xml.rels",
                    r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
                      <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart1.xml"/>
                    </Relationships>"#,
                ),
                ("ppt/charts/chart1.xml", chart),
            ],
            &[],
        );
        assert_eq!(slide.shapes.len(), 1, "chart → one shape");
        let table = match &slide.shapes[0].kind {
            BlockKind::Table(t) => t,
            other => panic!("expected a Table, got {other:?}"),
        };
        // Frame placed from p:xfrm: off (50pt,50pt), ext (300pt,200pt) → y = 540-250 = 290.
        let f = slide.shapes[0].frame.expect("chart frame");
        assert!(
            (f.x - 50.0).abs() < 1e-6 && (f.y - 290.0).abs() < 1e-6,
            "frame: {f:?}"
        );
        // Flatten all cell text and assert the data is present.
        let all: String = table
            .rows
            .iter()
            .flat_map(|r| r.cells.iter())
            .map(|c| {
                block_text(&Block {
                    kind: BlockKind::TextBox(model::TextBox {
                        blocks: c.blocks.clone(),
                    }),
                    ..Block::default()
                })
            })
            .collect::<Vec<_>>()
            .join("|");
        for needle in [
            "Quarterly Sales",
            "Q1",
            "Q2",
            "Region A",
            "Region B",
            "10",
            "20",
            "30",
            "40",
        ] {
            assert!(
                all.contains(needle),
                "chart data missing {needle:?} in: {all}"
            );
        }
    }

    #[test]
    fn pptx_model_placeholder_inherits_layout_master_geometry() {
        // A body placeholder with NO own a:xfrm inherits its box from the layout
        // (matched by @idx); the layout in turn reaches the master via its rels.
        let slide = pptx_model_slide_parts(
            &[
                (
                    "ppt/slides/slide1.xml",
                    r#"<p:sld xmlns:a="a" xmlns:p="p"><p:cSld><p:spTree>
                      <p:sp>
                        <p:nvSpPr><p:nvPr><p:ph type="body" idx="1"/></p:nvPr></p:nvSpPr>
                        <p:spPr/>
                        <p:txBody><a:p><a:r><a:t>Inherited Body</a:t></a:r></a:p></p:txBody>
                      </p:sp>
                    </p:spTree></p:cSld></p:sld>"#,
                ),
                (
                    "ppt/slides/_rels/slide1.xml.rels",
                    r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
                      <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
                    </Relationships>"#,
                ),
                (
                    "ppt/slideLayouts/slideLayout1.xml",
                    r#"<p:sldLayout xmlns:a="a" xmlns:p="p"><p:cSld><p:spTree>
                      <p:sp>
                        <p:nvSpPr><p:nvPr><p:ph type="body" idx="1"/></p:nvPr></p:nvSpPr>
                        <p:spPr><a:xfrm><a:off x="635000" y="2540000"/><a:ext cx="7620000" cy="2540000"/></a:xfrm></p:spPr>
                      </p:sp>
                    </p:spTree></p:cSld></p:sldLayout>"#,
                ),
                (
                    "ppt/slideLayouts/_rels/slideLayout1.xml.rels",
                    r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
                      <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="../slideMasters/slideMaster1.xml"/>
                    </Relationships>"#,
                ),
                (
                    "ppt/slideMasters/slideMaster1.xml",
                    r#"<p:sldMaster xmlns:a="a" xmlns:p="p"><p:cSld><p:spTree/></p:cSld></p:sldMaster>"#,
                ),
            ],
            &[],
        );
        assert_eq!(slide.placeholders.len(), 1, "one body placeholder");
        let ph = &slide.placeholders[0];
        assert_eq!(ph.role, model::PlaceholderRole::Body);
        let f = ph
            .block
            .frame
            .expect("placeholder inherits a frame from the layout");
        // layout off (50pt,200pt), ext (600pt,200pt) → lower-left y = 540-(200+200)=140.
        assert!((f.x - 50.0).abs() < 1e-6, "x: {}", f.x);
        assert!((f.y - 140.0).abs() < 1e-6, "y: {}", f.y);
        assert!((f.w - 600.0).abs() < 1e-6, "w: {}", f.w);
        assert!((f.h - 200.0).abs() < 1e-6, "h: {}", f.h);
        assert_eq!(block_text(&ph.block), "Inherited Body");
    }

    #[test]
    fn pptx_model_smartart_text_becomes_list() {
        // A SmartArt graphicFrame (dgm:relIds → data model) surfaces each node's
        // text as a bullet list, rather than being dropped silently.
        let data = r#"<dgm:dataModel xmlns:dgm="dgm" xmlns:a="a">
          <dgm:ptLst>
            <dgm:pt modelId="1" type="node"><dgm:t><a:p><a:r><a:t>First Node</a:t></a:r></a:p></dgm:t></dgm:pt>
            <dgm:pt modelId="2" type="node"><dgm:t><a:p><a:r><a:t>Second Node</a:t></a:r></a:p></dgm:t></dgm:pt>
            <dgm:pt modelId="0" type="doc"><dgm:t><a:p><a:endParaRPr/></a:p></dgm:t></dgm:pt>
          </dgm:ptLst>
        </dgm:dataModel>"#;
        let slide = pptx_model_slide_parts(
            &[
                (
                    "ppt/slides/slide1.xml",
                    r#"<p:sld xmlns:a="a" xmlns:p="p" xmlns:r="r"><p:cSld><p:spTree>
                      <p:graphicFrame>
                        <p:xfrm><a:off x="0" y="0"/><a:ext cx="3810000" cy="2540000"/></p:xfrm>
                        <a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/diagram">
                          <dgm:relIds xmlns:dgm="dgm" r:dm="rId1" r:lo="rId2" r:qs="rId3" r:cs="rId4"/>
                        </a:graphicData></a:graphic>
                      </p:graphicFrame>
                    </p:spTree></p:cSld></p:sld>"#,
                ),
                (
                    "ppt/slides/_rels/slide1.xml.rels",
                    r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
                      <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/diagramData" Target="../diagrams/data1.xml"/>
                    </Relationships>"#,
                ),
                ("ppt/diagrams/data1.xml", data),
            ],
            &[],
        );
        assert_eq!(slide.shapes.len(), 1, "SmartArt → one shape");
        let list = match &slide.shapes[0].kind {
            BlockKind::List(l) => l,
            other => panic!("expected a List, got {other:?}"),
        };
        assert_eq!(
            list.items.len(),
            2,
            "two node texts (the empty doc node is skipped)"
        );
        let texts: Vec<String> = list
            .items
            .iter()
            .map(|it| {
                block_text(&Block {
                    kind: BlockKind::TextBox(model::TextBox {
                        blocks: it.blocks.clone(),
                    }),
                    ..Block::default()
                })
            })
            .collect();
        assert_eq!(texts, vec!["First Node", "Second Node"]);
    }

    #[test]
    fn pptx_model_graphic_frame_table_keeps_cells_and_frame() {
        // A native a:tbl inside a graphicFrame becomes a model Table with the
        // frame from p:xfrm (regression guard alongside the chart/SmartArt paths).
        let slide = pptx_model_slide(
            r#"<p:sld xmlns:a="a" xmlns:p="p"><p:cSld><p:spTree>
              <p:graphicFrame>
                <p:xfrm><a:off x="0" y="0"/><a:ext cx="2540000" cy="1270000"/></p:xfrm>
                <a:graphic><a:graphicData>
                  <a:tbl><a:tblGrid><a:gridCol w="1270000"/><a:gridCol w="1270000"/></a:tblGrid>
                    <a:tr><a:tc><a:txBody><a:p><a:r><a:t>R1C1</a:t></a:r></a:p></a:txBody></a:tc>
                    <a:tc><a:txBody><a:p><a:r><a:t>R1C2</a:t></a:r></a:p></a:txBody></a:tc></a:tr>
                  </a:tbl>
                </a:graphicData></a:graphic>
              </p:graphicFrame>
            </p:spTree></p:cSld></p:sld>"#,
        );
        assert_eq!(slide.shapes.len(), 1);
        let table = match &slide.shapes[0].kind {
            BlockKind::Table(t) => t,
            other => panic!("expected Table, got {other:?}"),
        };
        assert_eq!(table.col_widths.len(), 2, "two grid columns");
        assert_eq!(table.rows.len(), 1);
        assert_eq!(table.rows[0].cells.len(), 2);
        assert!(slide.shapes[0].frame.is_some(), "table frame from p:xfrm");
    }

    // ── #47: run hyperlinks, table cell fill/borders, theme colours, mirror ──

    /// The first run of the first paragraph block in a TextBox shape.
    fn first_textbox_runs(b: &Block) -> &[Inline] {
        let tb = match &b.kind {
            BlockKind::TextBox(tb) => tb,
            other => panic!("expected a TextBox, got {other:?}"),
        };
        match &tb.blocks[0].kind {
            BlockKind::Paragraph(p) => &p.runs,
            other => panic!("expected a Paragraph, got {other:?}"),
        }
    }

    #[test]
    fn pptx_model_run_hyperlink_becomes_inline_link() {
        // `a:hlinkClick@r:id` on a run resolves through the slide rels to an
        // external URL and wraps the run in an `Inline::Link` (instead of being
        // dropped). A second, plain run stays a bare `Inline::Run`.
        let slide = pptx_model_slide_parts(
            &[
                (
                    "ppt/slides/slide1.xml",
                    r#"<p:sld xmlns:a="a" xmlns:p="p" xmlns:r="r"><p:cSld><p:spTree>
                      <p:sp><p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="635000" cy="635000"/></a:xfrm></p:spPr>
                        <p:txBody><a:p>
                          <a:r><a:rPr><a:hlinkClick r:id="rId1"/></a:rPr><a:t>Visit</a:t></a:r>
                          <a:r><a:rPr/><a:t> plain</a:t></a:r>
                        </a:p></p:txBody></p:sp>
                    </p:spTree></p:cSld></p:sld>"#,
                ),
                (
                    "ppt/slides/_rels/slide1.xml.rels",
                    r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
                      <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.com/docs" TargetMode="External"/>
                    </Relationships>"#,
                ),
            ],
            &[],
        );
        let runs = first_textbox_runs(&slide.shapes[0]);
        let link = runs
            .iter()
            .find_map(|i| match i {
                Inline::Link { href, children } => Some((href, children)),
                _ => None,
            })
            .expect("a hyperlink run → Inline::Link");
        assert_eq!(
            *link.0,
            model::LinkTarget::Url("https://example.com/docs".to_string())
        );
        let linked_text: String = link
            .1
            .iter()
            .filter_map(|i| match i {
                Inline::Run(r) => Some(r.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(linked_text, "Visit", "anchor text inside the link");
        // The plain run is NOT swallowed into the link.
        assert!(
            runs.iter()
                .any(|i| matches!(i, Inline::Run(r) if r.text == " plain")),
            "plain run stays bare: {runs:?}"
        );
    }

    #[test]
    fn pptx_model_table_cell_fill_and_border_are_read() {
        // `a:tc/a:tcPr/a:solidFill` → Cell.shading; the first `a:lnL/R/T/B` edge
        // (width + colour) → the model's single table-wide BorderStyle.
        let slide = pptx_model_slide(
            r#"<p:sld xmlns:a="a" xmlns:p="p"><p:cSld><p:spTree>
              <p:graphicFrame>
                <p:xfrm><a:off x="0" y="0"/><a:ext cx="2540000" cy="1270000"/></p:xfrm>
                <a:graphic><a:graphicData>
                  <a:tbl><a:tblGrid><a:gridCol w="1270000"/></a:tblGrid>
                    <a:tr><a:tc>
                      <a:txBody><a:p><a:r><a:t>Filled</a:t></a:r></a:p></a:txBody>
                      <a:tcPr>
                        <a:lnL w="12700"><a:solidFill><a:srgbClr val="FF0000"/></a:solidFill></a:lnL>
                        <a:solidFill><a:srgbClr val="00FF00"/></a:solidFill>
                      </a:tcPr>
                    </a:tc></a:tr>
                  </a:tbl>
                </a:graphicData></a:graphic>
              </p:graphicFrame>
            </p:spTree></p:cSld></p:sld>"#,
        );
        let table = match &slide.shapes[0].kind {
            BlockKind::Table(t) => t,
            other => panic!("expected Table, got {other:?}"),
        };
        // Cell fill 00FF00 → green shading.
        assert_eq!(
            table.rows[0].cells[0].shading,
            Some([0.0, 1.0, 0.0]),
            "cell solidFill → shading"
        );
        // Border 1pt (12700 EMU) red, taken from a:lnL (not the cell fill).
        assert!((table.border.width - 1.0).abs() < 1e-6, "border 1pt");
        assert_eq!(table.border.color, [1.0, 0.0, 0.0], "border red from lnL");
    }

    #[test]
    fn pptx_model_run_scheme_colour_with_tint_resolves() {
        // `a:schemeClr val="accent1"` resolves through the theme, and a
        // `lumMod`/`lumOff` tint modulates it (40% luminance + 60% offset of the
        // pure-blue accent ⇒ a lighter blue, no longer the raw accent or black).
        let slide = pptx_model_slide_parts(
            &[
                (
                    "ppt/theme/theme1.xml",
                    r#"<a:theme xmlns:a="a"><a:themeElements><a:clrScheme name="Office">
                      <a:dk1><a:srgbClr val="000000"/></a:dk1>
                      <a:lt1><a:srgbClr val="FFFFFF"/></a:lt1>
                      <a:accent1><a:srgbClr val="0000FF"/></a:accent1>
                    </a:clrScheme></a:themeElements></a:theme>"#,
                ),
                (
                    "ppt/slides/slide1.xml",
                    r#"<p:sld xmlns:a="a" xmlns:p="p"><p:cSld><p:spTree>
                      <p:sp><p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="635000" cy="635000"/></a:xfrm></p:spPr>
                        <p:txBody><a:p><a:r>
                          <a:rPr><a:solidFill><a:schemeClr val="accent1"><a:lumMod val="40000"/><a:lumOff val="60000"/></a:schemeClr></a:solidFill></a:rPr>
                          <a:t>Tinted</a:t>
                        </a:r></a:p></p:txBody></p:sp>
                    </p:spTree></p:cSld></p:sld>"#,
                ),
            ],
            &[],
        );
        let runs = first_textbox_runs(&slide.shapes[0]);
        let color = runs
            .iter()
            .find_map(|i| match i {
                Inline::Run(r) => r.style.color,
                _ => None,
            })
            .expect("run colour set");
        // Base 0000FF → lumMod 0.4 → [0,0,0.4]; lumOff +0.6 → [0.6,0.6,1.0].
        assert!((color[0] - 0.6).abs() < 0.01, "R tint: {color:?}");
        assert!((color[1] - 0.6).abs() < 0.01, "G tint: {color:?}");
        assert!((color[2] - 1.0).abs() < 0.01, "B tint: {color:?}");
    }

    #[test]
    fn pptx_model_double_flip_folds_into_rotation() {
        // flipH AND flipV together = a 180° point reflection; with no `@rot` the
        // model rotation becomes D180 (a single-axis flip, an unrepresentable
        // reflection, would leave it D0 — covered separately below).
        let slide = pptx_model_slide(
            r#"<p:sld xmlns:a="a" xmlns:p="p"><p:cSld><p:spTree>
              <p:sp>
                <p:spPr><a:xfrm flipH="1" flipV="1"><a:off x="127000" y="127000"/><a:ext cx="635000" cy="635000"/></a:xfrm></p:spPr>
                <p:txBody><a:p><a:r><a:t>Mirrored</a:t></a:r></a:p></p:txBody>
              </p:sp>
            </p:spTree></p:cSld></p:sld>"#,
        );
        assert_eq!(slide.shapes.len(), 1);
        assert_eq!(
            slide.shapes[0].rotation,
            crate::model::Rotation::D180,
            "flipH+flipV → 180° rotation"
        );
        assert!(slide.shapes[0].frame.is_some(), "frame still placed");
    }

    #[test]
    fn pptx_model_single_flip_keeps_frame_without_reflection() {
        // A single-axis flip is a reflection the model's rotation cannot express;
        // it is dropped, but the box's absolute placement is still preserved (not
        // dropped to flow).
        let slide = pptx_model_slide(
            r#"<p:sld xmlns:a="a" xmlns:p="p"><p:cSld><p:spTree>
              <p:sp>
                <p:spPr><a:xfrm flipH="1"><a:off x="127000" y="127000"/><a:ext cx="635000" cy="635000"/></a:xfrm></p:spPr>
                <p:txBody><a:p><a:r><a:t>Flipped</a:t></a:r></a:p></p:txBody>
              </p:sp>
            </p:spTree></p:cSld></p:sld>"#,
        );
        assert_eq!(slide.shapes.len(), 1);
        assert_eq!(
            slide.shapes[0].rotation,
            crate::model::Rotation::D0,
            "single flip leaves rotation unchanged"
        );
        assert!(slide.shapes[0].frame.is_some(), "frame still placed");
    }

    // ── #51: slide/page background fill (PPTX p:bg, ODP draw:page) ──

    /// Approximate equality for an RGB triple against an expected `#RRGGBB`.
    fn assert_bg(actual: Option<[f64; 3]>, hex: &str) {
        let exp = hex_to_rgb_f64(hex).expect("valid expected hex");
        let got = actual.expect("slide background present");
        for i in 0..3 {
            assert!(
                (got[i] - exp[i]).abs() < 1e-6,
                "channel {i}: got {got:?} exp {exp:?} (#{hex})"
            );
        }
    }

    #[test]
    fn pptx_model_slide_background_solid_srgb_reaches_model() {
        // A `p:cSld/p:bg/p:bgPr/a:solidFill/a:srgbClr` full-slide fill lands in
        // `Slide::background` as RGB (not dropped as it was before #51).
        let slide = pptx_model_slide(
            r#"<p:sld xmlns:a="a" xmlns:p="p"><p:cSld>
              <p:bg><p:bgPr><a:solidFill><a:srgbClr val="203864"/></a:solidFill></p:bgPr></p:bg>
              <p:spTree>
                <p:sp><p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="457200"/></a:xfrm></p:spPr>
                  <p:txBody><a:p><a:r><a:t>On colour</a:t></a:r></a:p></p:txBody></p:sp>
              </p:spTree></p:cSld></p:sld>"#,
        );
        assert_bg(slide.background, "203864");
        assert_eq!(slide.shapes.len(), 1, "shapes still parsed alongside bg");
    }

    #[test]
    fn pptx_model_slide_background_scheme_colour_resolves_via_theme() {
        // A `a:schemeClr val="accent1"` background resolves through the theme's
        // colour scheme (accent1 = 4472C4 here).
        let slide = pptx_model_slide_parts(
            &[
                (
                    "ppt/slides/slide1.xml",
                    r#"<p:sld xmlns:a="a" xmlns:p="p"><p:cSld>
                      <p:bg><p:bgPr><a:solidFill><a:schemeClr val="accent1"/></a:solidFill></p:bgPr></p:bg>
                      <p:spTree/></p:cSld></p:sld>"#,
                ),
                (
                    "ppt/theme/theme1.xml",
                    r#"<a:theme xmlns:a="a"><a:themeElements><a:clrScheme name="X">
                      <a:dk1><a:srgbClr val="000000"/></a:dk1><a:lt1><a:srgbClr val="FFFFFF"/></a:lt1>
                      <a:dk2><a:srgbClr val="44546A"/></a:dk2><a:lt2><a:srgbClr val="E7E6E6"/></a:lt2>
                      <a:accent1><a:srgbClr val="4472C4"/></a:accent1><a:accent2><a:srgbClr val="ED7D31"/></a:accent2>
                      <a:accent3><a:srgbClr val="A5A5A5"/></a:accent3><a:accent4><a:srgbClr val="FFC000"/></a:accent4>
                      <a:accent5><a:srgbClr val="5B9BD5"/></a:accent5><a:accent6><a:srgbClr val="70AD47"/></a:accent6>
                      <a:hlink><a:srgbClr val="0563C1"/></a:hlink><a:folHlink><a:srgbClr val="954F72"/></a:folHlink>
                      </a:clrScheme></a:themeElements></a:theme>"#,
                ),
            ],
            &[],
        );
        assert_bg(slide.background, "4472C4");
    }

    #[test]
    fn pptx_model_slide_background_gradient_takes_first_stop() {
        // A gradient page fill keeps the first stop's colour as the dominant
        // visible background (image/tile fills remain out of scope → None).
        let slide = pptx_model_slide(
            r#"<p:sld xmlns:a="a" xmlns:p="p"><p:cSld>
              <p:bg><p:bgPr><a:gradFill><a:gsLst>
                <a:gs pos="0"><a:srgbClr val="112233"/></a:gs>
                <a:gs pos="100000"><a:srgbClr val="FFFFFF"/></a:gs>
              </a:gsLst></a:gradFill></p:bgPr></p:bg>
              <p:spTree/></p:cSld></p:sld>"#,
        );
        assert_bg(slide.background, "112233");
    }

    #[test]
    fn pptx_model_picture_background_yields_none() {
        // A blip (picture) page fill carries no resolvable colour → background None
        // (so the slide is not tinted by a stray colour).
        let slide = pptx_model_slide(
            r#"<p:sld xmlns:a="a" xmlns:p="p" xmlns:r="r"><p:cSld>
              <p:bg><p:bgPr><a:blipFill><a:blip r:embed="rId9"/></a:blipFill></p:bgPr></p:bg>
              <p:spTree/></p:cSld></p:sld>"#,
        );
        assert!(slide.background.is_none(), "picture fill → no colour");
    }

    /// Build an ODP whose single `draw:page` has `draw:style-name`/
    /// `draw:master-page-name` attrs, with the given `styles.xml` `<office:styles>`
    /// body and `<office:master-styles>` body. Returns the first lowered slide.
    fn odp_model_slide_styled(page_attrs: &str, styles_body: &str, master_body: &str) -> Slide {
        let content = format!(
            r#"<office:document-content xmlns:office="o" xmlns:draw="d" xmlns:text="t" xmlns:svg="sv" xmlns:style="s" xmlns:fo="f">
  <office:automatic-styles>
    <style:page-layout style:name="PL"><style:page-layout-properties fo:page-width="25.4cm" fo:page-height="19.05cm" fo:margin="0cm"/></style:page-layout>
  </office:automatic-styles>
  <office:body><office:presentation>
    <draw:page draw:name="p1" {page_attrs}><text:p>Slide body</text:p></draw:page>
  </office:presentation></office:body>
</office:document-content>"#
        );
        let styles = format!(
            r#"<office:document-styles xmlns:office="o" xmlns:draw="d" xmlns:style="s" xmlns:fo="f">
  <office:styles>{styles_body}</office:styles>
  <office:master-styles>{master_body}</office:master-styles>
</office:document-styles>"#
        );
        let mut z = ZipWriter::new();
        z.add_stored(
            "mimetype",
            b"application/vnd.oasis.opendocument.presentation",
        );
        z.add_stored("content.xml", content.as_bytes());
        z.add_stored("styles.xml", styles.as_bytes());
        let model = office_to_model(&z.finish()).expect("odp → model");
        match model.sections[0].pages[0].blocks[0].kind.clone() {
            BlockKind::Slide(sb) => sb.slides.into_iter().next().expect("one slide"),
            other => panic!("expected a Slide block, got {other:?}"),
        }
    }

    #[test]
    fn odp_model_page_solid_fill_reaches_model() {
        // A `draw:page` whose own `draw:style-name` (family drawing-page) declares
        // `draw:fill="solid"` + `draw:fill-color` → that colour in Slide::background.
        let slide = odp_model_slide_styled(
            r#"draw:style-name="dp1""#,
            r##"<style:style style:name="dp1" style:family="drawing-page">
                 <style:drawing-page-properties draw:fill="solid" draw:fill-color="#1F4E79"/>
               </style:style>"##,
            "",
        );
        assert_bg(slide.background, "1F4E79");
    }

    #[test]
    fn odp_model_page_inherits_master_background() {
        // A `draw:page` with NO own fill, only a `draw:master-page-name`, inherits
        // the master page's drawing-page-style fill colour.
        let slide = odp_model_slide_styled(
            r#"draw:master-page-name="Default""#,
            r##"<style:style style:name="dpMaster" style:family="drawing-page">
                 <style:drawing-page-properties draw:fill="solid" draw:fill-color="#C00000"/>
               </style:style>"##,
            r#"<style:master-page style:name="Default" draw:style-name="dpMaster"/>"#,
        );
        assert_bg(slide.background, "C00000");
    }

    #[test]
    fn odp_model_page_own_fill_overrides_master() {
        // The page's own `draw:style-name` fill wins over its master's.
        let slide = odp_model_slide_styled(
            r#"draw:style-name="dpPage" draw:master-page-name="Default""#,
            r##"<style:style style:name="dpPage" style:family="drawing-page">
                 <style:drawing-page-properties draw:fill="solid" draw:fill-color="#00B050"/>
               </style:style>
               <style:style style:name="dpMaster" style:family="drawing-page">
                 <style:drawing-page-properties draw:fill="solid" draw:fill-color="#C00000"/>
               </style:style>"##,
            r#"<style:master-page style:name="Default" draw:style-name="dpMaster"/>"#,
        );
        assert_bg(slide.background, "00B050");
    }

    #[test]
    fn odp_model_page_fill_none_yields_no_background() {
        // `draw:fill="none"` (an explicitly empty page) leaves the slide white.
        let slide = odp_model_slide_styled(
            r#"draw:style-name="dpNone""#,
            r##"<style:style style:name="dpNone" style:family="drawing-page">
                 <style:drawing-page-properties draw:fill="none" draw:fill-color="#1F4E79"/>
               </style:style>"##,
            "",
        );
        assert!(slide.background.is_none(), "fill=none → no background");
    }
}
