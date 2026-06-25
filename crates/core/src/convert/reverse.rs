//! Reverse conversions: `TXT / HTML / RTF / DOCX / ODT / PPTX / XLSX / ODS → PDF`.
//!
//! Every source reduces to a list of text paragraphs (and, for slides/sheets,
//! page-break sections); [`flow_to_pdf`] lays them onto pages with the
//! [`PdfBuilder`](super::build::PdfBuilder). Office files are ZIP-of-XML, so we
//! read the relevant part (via [`super::zip::read_zip`]) and recover paragraphs
//! by replacing block-boundary tags with newlines and stripping the rest — which
//! works for both the engine's own exports and simple external files.
//!
//! This is a text-faithful conversion (all content, reading order, pagination),
//! not a pixel-perfect re-layout — the honest zero-dependency scope.

use super::build::{PdfBuilder, StdFont};

// ─────────────────────────────── text helpers ──────────────────────────────

/// Decode the XML/HTML entities our exporters (and common tools) emit.
pub(crate) fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        let (decoded, len) = if tail.starts_with("&amp;") {
            ('&', 5)
        } else if tail.starts_with("&lt;") {
            ('<', 4)
        } else if tail.starts_with("&gt;") {
            ('>', 4)
        } else if tail.starts_with("&quot;") {
            ('"', 6)
        } else if tail.starts_with("&apos;") {
            ('\'', 6)
        } else if tail.starts_with("&#") {
            // Numeric entity &#NN; or &#xHH;
            if let Some(semi) = tail.find(';') {
                let body = &tail[2..semi];
                let code =
                    if let Some(hex) = body.strip_prefix('x').or_else(|| body.strip_prefix('X')) {
                        u32::from_str_radix(hex, 16).ok()
                    } else {
                        body.parse::<u32>().ok()
                    };
                match code.and_then(char::from_u32) {
                    Some(c) => (c, semi + 1),
                    None => ('&', 1),
                }
            } else {
                ('&', 1)
            }
        } else {
            ('&', 1)
        };
        out.push(decoded);
        rest = &tail[len..];
    }
    out.push_str(rest);
    out
}

/// Recover paragraphs from XML: each `boundary` tag becomes a paragraph break,
/// each `cell_sep` tag a space; all other tags are stripped and entities
/// decoded. Robust for OOXML/ODF/HTML alike.
fn paragraphs_from_xml(xml: &str, boundaries: &[&str], cell_sep: &[&str]) -> Vec<String> {
    let mut s = xml.to_string();
    for tag in cell_sep {
        s = s.replace(tag, " \u{0}"); // keep a space but not a break
    }
    for tag in boundaries {
        s = s.replace(tag, "\n");
    }
    let mut text = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => text.push(c),
            _ => {}
        }
    }
    let text = unescape(&text).replace('\u{0}', "");
    text.split('\n')
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect()
}

/// Greedy word-wrap to at most `max_chars` per line (rough, char-count based).
fn wrap(text: &str, max_chars: usize) -> Vec<String> {
    if text.chars().count() <= max_chars {
        return vec![text.to_string()];
    }
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let add = if line.is_empty() { 0 } else { 1 };
        if line.chars().count() + add + word.chars().count() > max_chars && !line.is_empty() {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        // A single over-long word: hard-split it.
        if word.chars().count() > max_chars {
            for chunk in chunk_chars(word, max_chars) {
                if !line.is_empty() {
                    lines.push(std::mem::take(&mut line));
                }
                lines.push(chunk);
            }
        } else {
            line.push_str(word);
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

fn chunk_chars(s: &str, n: usize) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    chars.chunks(n).map(|c| c.iter().collect()).collect()
}

// ─────────────────────────────── layout → PDF ──────────────────────────────

/// Flow `sections` of paragraphs onto US-Letter pages. Each section after the
/// first starts on a new page (slides → one per slide; sheets → one per sheet).
pub fn flow_to_pdf(sections: &[Vec<String>]) -> Vec<u8> {
    const W: f64 = 612.0;
    const H: f64 = 792.0;
    const MARGIN: f64 = 56.0;
    const SIZE: f64 = 11.0;
    let line_h = SIZE * 1.4;
    let max_chars = ((W - 2.0 * MARGIN) / (SIZE * 0.5)).floor().max(8.0) as usize;

    let mut b = PdfBuilder::new();
    let mut page = b.add_page(W, H);
    let mut y = MARGIN;
    let mut page_has_content = false;

    for (si, section) in sections.iter().enumerate() {
        if si > 0 && page_has_content {
            page = b.add_page(W, H);
            y = MARGIN;
            page_has_content = false;
        }
        for para in section {
            for line in wrap(para, max_chars) {
                if y + line_h > H - MARGIN {
                    page = b.add_page(W, H);
                    y = MARGIN;
                }
                b.text(
                    page,
                    MARGIN,
                    y,
                    SIZE,
                    &line,
                    StdFont::Helvetica,
                    [0.0, 0.0, 0.0],
                );
                y += line_h;
                page_has_content = true;
            }
            y += line_h * 0.4; // paragraph spacing
        }
    }
    b.finish()
}

// ─────────────────────────────── sources → PDF ─────────────────────────────

/// Plain text → PDF (one paragraph per line; blank lines add spacing).
pub fn txt_to_pdf(text: &str) -> Vec<u8> {
    let paras: Vec<String> = text.lines().map(|l| l.trim_end().to_string()).collect();
    flow_to_pdf(&[paras])
}

/// HTML → PDF. Renders through the native HTML+CSS engine
/// ([`crate::html::render`]) so structure and typography are preserved
/// (headings, bold/italic, colour, tables, lists, `data:` images) — US-Letter
/// portrait with 0.5in margins. External `<img src>`/web-font URLs are omitted
/// here (no host fetch on this entry point); use the host-fetch path for those.
pub fn html_to_pdf(html: &str) -> Vec<u8> {
    crate::html::render(html, &[], 612.0, 792.0, 36.0)
}

/// DOCX → PDF.
pub fn docx_to_pdf(bytes: &[u8]) -> Vec<u8> {
    let zip = super::zip::read_zip(bytes);
    let xml = zip
        .get("word/document.xml")
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default();
    flow_to_pdf(&[paragraphs_from_xml(&xml, &["</w:p>"], &[])])
}

/// ODT → PDF.
pub fn odt_to_pdf(bytes: &[u8]) -> Vec<u8> {
    let zip = super::zip::read_zip(bytes);
    let xml = zip
        .get("content.xml")
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default();
    flow_to_pdf(&[paragraphs_from_xml(&xml, &["</text:p>", "</text:h>"], &[])])
}

/// PPTX → PDF (one page per slide).
pub fn pptx_to_pdf(bytes: &[u8]) -> Vec<u8> {
    let zip = super::zip::read_zip(bytes);
    // Slides in numeric order: slide1.xml, slide2.xml, …
    let mut slides: Vec<(usize, String)> = zip
        .iter()
        .filter(|(k, _)| k.starts_with("ppt/slides/slide") && k.ends_with(".xml"))
        .filter_map(|(k, v)| {
            let n: usize = k["ppt/slides/slide".len()..k.len() - 4].parse().ok()?;
            Some((n, String::from_utf8_lossy(v).into_owned()))
        })
        .collect();
    slides.sort_by_key(|(n, _)| *n);
    let mut sections: Vec<Vec<String>> = slides
        .iter()
        .map(|(_, xml)| paragraphs_from_xml(xml, &["</a:p>"], &[]))
        .collect();
    if sections.is_empty() {
        sections.push(Vec::new());
    }
    flow_to_pdf(&sections)
}

/// XLSX → PDF (one page per sheet; cells space-separated per row).
pub fn xlsx_to_pdf(bytes: &[u8]) -> Vec<u8> {
    let zip = super::zip::read_zip(bytes);
    let mut sheets: Vec<(usize, String)> = zip
        .iter()
        .filter(|(k, _)| k.starts_with("xl/worksheets/sheet") && k.ends_with(".xml"))
        .filter_map(|(k, v)| {
            let n: usize = k["xl/worksheets/sheet".len()..k.len() - 4].parse().ok()?;
            Some((n, String::from_utf8_lossy(v).into_owned()))
        })
        .collect();
    sheets.sort_by_key(|(n, _)| *n);
    let mut sections: Vec<Vec<String>> = sheets
        .iter()
        .map(|(_, xml)| paragraphs_from_xml(xml, &["</row>"], &["</c>"]))
        .collect();
    if sections.is_empty() {
        sections.push(Vec::new());
    }
    flow_to_pdf(&sections)
}

/// ODS → PDF (rows as paragraphs, cells space-separated).
pub fn ods_to_pdf(bytes: &[u8]) -> Vec<u8> {
    let zip = super::zip::read_zip(bytes);
    let xml = zip
        .get("content.xml")
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default();
    flow_to_pdf(&[paragraphs_from_xml(
        &xml,
        &["</table:table-row>"],
        &["</table:table-cell>"],
    )])
}

/// ODP → PDF (one page per slide; text runs from `draw:text-box`).
pub fn odp_to_pdf(bytes: &[u8]) -> Vec<u8> {
    let zip = super::zip::read_zip(bytes);
    let xml = zip
        .get("content.xml")
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default();
    // Slides are `<draw:page>`; their text lives in `<text:p>` runs.
    let sections: Vec<Vec<String>> = xml
        .split("<draw:page")
        .skip(1)
        .map(|slide| paragraphs_from_xml(slide, &["</text:p>"], &[]))
        .collect();
    let sections = if sections.is_empty() {
        vec![Vec::new()]
    } else {
        sections
    };
    flow_to_pdf(&sections)
}

/// Auto-detect an Office container and convert to PDF. Returns `None` if the
/// bytes are not a recognized OOXML/ODF archive.
///
/// Delegates to [`super::office_import::office_to_pdf`], which maps the document
/// to styled HTML (headings, bold/italic, colour, tables, lists, images) and
/// renders it through the native HTML→PDF engine for rich fidelity — and adds a
/// best-effort text path for legacy OLE2 `.doc/.xls/.ppt`. The per-format
/// text-only helpers below remain available for callers that want the flat flow.
pub fn office_to_pdf(bytes: &[u8]) -> Option<Vec<u8>> {
    super::office_import::office_to_pdf(bytes)
}

/// Phase 2 of the two-phase font flow: like [`office_to_pdf`] but also embeds the
/// `host`-supplied faces — the families [`office_needed_fonts`] reported as
/// referenced-but-unembedded, fetched by the host (Google Fonts / system) and
/// handed back so styled runs lay out with the right metrics (e.g. Carlito for a
/// Calibri reference). Faces the container embeds itself win on conflict, so this
/// never regresses a self-embedding document; `None` for an unrecognized archive.
///
/// Delegates to [`super::office_import::office_to_pdf_with_fonts`].
pub fn office_to_pdf_with_fonts(
    bytes: &[u8],
    host: &[crate::html::ProvidedFont],
) -> Option<Vec<u8>> {
    super::office_import::office_to_pdf_with_fonts(bytes, host)
}

/// The Google/system fonts an Office container **references but doesn't embed** —
/// the two-phase host fetch list for [`office_to_pdf`] (so referenced families
/// lay out with the right metrics). Fonts the container embeds itself, and the
/// base-14 standards, are excluded. `None` for an unrecognized archive. Mirrors
/// [`crate::html::needed_fonts`] for the Office path.
pub fn office_needed_fonts(bytes: &[u8]) -> Option<Vec<crate::html::FontRequest>> {
    super::office_import::office_needed_fonts(bytes)
}

// ─────────────────────────────── image → PDF ───────────────────────────────

/// A raster image (PNG / JPEG / GIF / WebP / AVIF) → a one-page PDF, the image
/// centred on an A4 portrait page and scaled to fit (preserving aspect, never
/// upscaled past 1:1). `None` if the bytes are not a recognized image.
///
/// PNG and JPEG embed directly (the PDF `/Image` XObject path takes them as-is —
/// JPEG verbatim via `/DCTDecode`, PNG decoded to a Flate colour stream + soft
/// mask). GIF / WebP / AVIF are **transcoded to PNG** first
/// ([`gif`](crate::raster::gif) / [`webp`](crate::raster::webp) /
/// [`avif`](crate::raster::avif) decode → [`encode_png`](crate::raster::png::encode_png)),
/// because the embedder only writes PNG/JPEG XObjects. Every step is pure Rust —
/// no third-party image library.
pub fn image_to_pdf(bytes: &[u8]) -> Option<Vec<u8>> {
    use crate::model::{Margins, PageGeometry};

    // Reduce any input to embeddable PNG/JPEG bytes + pixel dimensions.
    let (embed, w_px, h_px) = embeddable_image(bytes)?;
    let (iw, ih) = (w_px.max(1) as f64, h_px.max(1) as f64);

    // A4 portrait with the default margins, the image fit inside the content box.
    let geom = PageGeometry::default();
    let Margins {
        top,
        right,
        bottom,
        left,
    } = geom.margins;
    let avail_w = (geom.width - left - right).max(1.0);
    let avail_h = (geom.height - top - bottom).max(1.0);
    let scale = (avail_w / iw).min(avail_h / ih).min(1.0);
    let (draw_w, draw_h) = (iw * scale, ih * scale);
    // PDF user space is bottom-up: centre horizontally, and place the image's
    // lower-left so it is vertically centred in the content area.
    let x = left + (avail_w - draw_w) / 2.0;
    let y = bottom + (avail_h - draw_h) / 2.0;

    // Build a blank one-page PDF, then embed the image as a real XObject.
    let mut builder = PdfBuilder::new();
    builder.add_page(geom.width, geom.height);
    let mut doc = crate::Document::open(&builder.finish()).ok()?;
    doc.add_image(1, &embed, x, y, draw_w, draw_h, 1.0).ok()?;
    Some(doc.save())
}

/// Coerce a raster image to bytes the PDF image embedder accepts (PNG or JPEG)
/// plus its pixel size. `None` for unrecognized bytes.
///
/// * **JPEG** passes through untouched (embedded verbatim under `/DCTDecode`).
/// * **PNG** passes through untouched: the embedder ([`crate::content::image`])
///   fully decodes it to RGBA (any colour type, bit depth 1–16, interlaced)
///   and splits it into an RGB stream + alpha soft-mask, so transparency is
///   preserved for every PNG variant.
/// * **GIF / WebP / AVIF** are decoded to RGBA and re-encoded as an RGBA PNG
///   (colour type 6), keeping any alpha channel intact for the soft-mask path.
///
/// Exposed `pub(crate)` so the image-watermark path
/// ([`Document::add_image_watermark`](crate::Document::add_image_watermark)) can
/// accept the same five input formats this conversion does, without duplicating
/// the format-detect/transcode logic or the raster decoders.
pub(crate) fn embeddable_image(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    // PNG and JPEG are accepted by the embedder directly — read dimensions
    // from the header.
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        let (w, h, _) = super::import::image_dimensions(bytes)?;
        return Some((bytes.to_vec(), w, h));
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        let (w, h, _) = super::import::image_dimensions(bytes)?;
        return Some((bytes.to_vec(), w, h));
    }
    // GIF / WebP / AVIF: decode to RGBA, then re-encode as an RGBA (type 6) PNG
    // so the embedder keeps the alpha channel as a soft mask.
    let (w, h, rgba) = crate::raster::gif::decode_gif(bytes)
        .or_else(|| crate::raster::webp::decode_webp(bytes))
        .or_else(|| crate::raster::avif::decode_avif(bytes))?;
    Some((crate::raster::png::encode_png(w, h, &rgba), w, h))
}

// ─────────────────────────────── RTF (both ways) ───────────────────────────

/// Escape a string for an RTF body (`\`, `{`, `}` and non-ASCII via `\uN?`).
fn rtf_escape(text: &str, out: &mut String) {
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '{' => out.push_str("\\{"),
            '}' => out.push_str("\\}"),
            c if (c as u32) < 0x80 => out.push(c),
            c => {
                // RTF \uN uses a signed 16-bit code unit + an ASCII fallback char.
                let code = c as u32;
                if code <= 0xFFFF {
                    let signed = if code > 0x7FFF {
                        code as i32 - 0x10000
                    } else {
                        code as i32
                    };
                    out.push_str(&format!("\\u{signed}?"));
                } else {
                    out.push('?');
                }
            }
        }
    }
}

/// Export paragraphs to an RTF document.
pub fn to_rtf(paragraphs: &[String]) -> Vec<u8> {
    let mut s = String::from("{\\rtf1\\ansi\\deff0{\\fonttbl{\\f0 Helvetica;}}\\fs22\n");
    for (i, para) in paragraphs.iter().enumerate() {
        if i > 0 {
            s.push_str("\\par\n");
        }
        rtf_escape(para, &mut s);
    }
    s.push_str("}\n");
    s.into_bytes()
}

// ─────────────────────────────── model → RTF ───────────────────────────────

use crate::model::{
    Block, BlockKind, CharStyle, Document, Heading, ImageRef, Inline, LinkTarget, List, ListMarker,
    Paragraph, ResourceTable, Table,
};

/// Serialize a unified [`Document`] to *rich* RTF with real paragraph breaks and
/// character styling (bold/italic/underline/strike/size/colour), plus structural
/// fidelity:
///
/// * **Tables** become real `\trowd … \cellxN … \cell … \row` grids (column
///   right-edges derived from [`Table::col_widths`] in twips), not tab lines.
/// * **Lists** emit ordered vs unordered markers with per-level indentation and
///   running counters (`1.`, `a.`, `i.`, `•`), honouring [`List::ordered`] /
///   [`List::marker`] and each item's nesting [`ListItem::level`].
/// * **Images** ([`BlockKind::Image`]) interned in [`Document::resources`] emit a
///   `{\pict …}` group — `\pngblip` for PNG, `\jpegblip` for JPEG, with
///   `\picwgoal`/`\pichgoal` from the pixel size and a hex payload. Formats RTF
///   cannot carry (GIF/WebP/AVIF/…) are skipped.
/// * **Hyperlinks** ([`Inline::Link`]) emit
///   `{\field{\*\fldinst HYPERLINK "url"}{\fldrslt <styled children>}}`, the form
///   the RTF importer ([`super::rtf::rtf_to_model`]) reads back.
pub fn rtf_from_model(doc: &Document) -> Vec<u8> {
    // Collect the distinct run colours into the RTF colour table; runs reference
    // a colour by 1-based index (`\cfN`).
    let mut colors: Vec<[u8; 3]> = Vec::new();
    collect_colors(doc, &mut colors);

    let mut color_tbl = String::from("{\\colortbl;");
    for c in &colors {
        color_tbl.push_str(&format!("\\red{}\\green{}\\blue{};", c[0], c[1], c[2]));
    }
    color_tbl.push('}');

    let mut s = format!("{{\\rtf1\\ansi\\deff0{{\\fonttbl{{\\f0 Helvetica;}}}}{color_tbl}\\fs22\n");
    let mut first = true;
    rtf_blocks_from_model(
        &collect_blocks(doc),
        &colors,
        &doc.resources,
        &mut first,
        &mut s,
    );
    s.push_str("}\n");
    s.into_bytes()
}

/// All top-level blocks across the document's sections/pages (header first,
/// footer last) flattened into one sequence.
fn collect_blocks(doc: &Document) -> Vec<Block> {
    let mut out = Vec::new();
    if let Some(h) = doc.sections.first().and_then(|s| s.header.as_ref()) {
        out.extend(h.iter().cloned());
    }
    for section in &doc.sections {
        for page in &section.pages {
            out.extend(page.blocks.iter().cloned());
        }
    }
    if let Some(f) = doc.sections.first().and_then(|s| s.footer.as_ref()) {
        out.extend(f.iter().cloned());
    }
    out
}

fn collect_colors(doc: &Document, colors: &mut Vec<[u8; 3]>) {
    for b in collect_blocks(doc) {
        collect_block_colors(&b, colors);
    }
}

fn collect_block_colors(block: &Block, colors: &mut Vec<[u8; 3]>) {
    match &block.kind {
        BlockKind::Paragraph(p) => collect_para_colors(p, colors),
        BlockKind::Heading(h) => collect_para_colors(&h.para, colors),
        BlockKind::List(list) => {
            for item in &list.items {
                for b in &item.blocks {
                    collect_block_colors(b, colors);
                }
            }
        }
        BlockKind::Table(table) => {
            for row in &table.rows {
                for cell in &row.cells {
                    for b in &cell.blocks {
                        collect_block_colors(b, colors);
                    }
                }
            }
        }
        BlockKind::TextBox(tb) => {
            for b in &tb.blocks {
                collect_block_colors(b, colors);
            }
        }
        BlockKind::Blockquote(bq) => {
            for b in &bq.blocks {
                collect_block_colors(b, colors);
            }
        }
        // Code blocks render in the default colour; rules carry none.
        _ => {}
    }
}

fn collect_para_colors(para: &Paragraph, colors: &mut Vec<[u8; 3]>) {
    for r in &para.runs {
        collect_inline_colors(r, colors);
    }
}

fn collect_inline_colors(inline: &Inline, colors: &mut Vec<[u8; 3]>) {
    match inline {
        Inline::Run(run) => {
            // Both the text colour (`\cf`) and the highlight colour (`\highlight`)
            // reference the shared colour table, so register both.
            for c in [rtf_run_color(&run.style), rtf_run_highlight(&run.style)]
                .into_iter()
                .flatten()
            {
                if !colors.contains(&c) {
                    colors.push(c);
                }
            }
        }
        Inline::Link { children, .. } => {
            for c in children {
                collect_inline_colors(c, colors);
            }
        }
        _ => {}
    }
}

/// A run's colour as an RGB byte triple, when set and not (near-)black.
fn rtf_run_color(style: &CharStyle) -> Option<[u8; 3]> {
    match style.color {
        Some([r, g, b]) if r > 0.02 || g > 0.02 || b > 0.02 => {
            let q = |c: f64| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
            Some([q(r), q(g), q(b)])
        }
        _ => None,
    }
}

/// A run's highlight / background as an RGB byte triple, when set. Any colour is
/// honoured (a dark highlight is valid), unlike the near-black guard on the text
/// colour. `None` ⇒ the run carries no highlight.
fn rtf_run_highlight(style: &CharStyle) -> Option<[u8; 3]> {
    style.background.map(|[r, g, b]| {
        let q = |c: f64| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
        [q(r), q(g), q(b)]
    })
}

fn rtf_blocks_from_model(
    blocks: &[Block],
    colors: &[[u8; 3]],
    res: &ResourceTable,
    first: &mut bool,
    out: &mut String,
) {
    for b in blocks {
        rtf_block_from_model(b, colors, res, first, out);
    }
}

fn rtf_block_from_model(
    block: &Block,
    colors: &[[u8; 3]],
    res: &ResourceTable,
    first: &mut bool,
    out: &mut String,
) {
    match &block.kind {
        BlockKind::Paragraph(p) => rtf_para_from_model(p, colors, res, false, first, out),
        BlockKind::Heading(h) => rtf_heading_from_model(h, colors, first, out),
        BlockKind::List(list) => rtf_list_from_model(list, colors, res, first, out),
        BlockKind::Table(table) => rtf_table_from_model(table, colors, res, first, out),
        BlockKind::TextBox(tb) => rtf_blocks_from_model(&tb.blocks, colors, res, first, out),
        BlockKind::Sheet(sb) => {
            for sheet in &sb.sheets {
                for row in &sheet.rows {
                    let line = row
                        .cells
                        .iter()
                        .map(|c| match &c.value {
                            crate::model::CellValue::Empty => String::new(),
                            crate::model::CellValue::Text(t) => t.clone(),
                            crate::model::CellValue::Number(n) => crate::content::num(*n),
                            crate::model::CellValue::Bool(b) => {
                                if *b { "TRUE" } else { "FALSE" }.to_string()
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\t");
                    rtf_plain_line(&line, first, out);
                }
            }
        }
        BlockKind::Slide(sb) => {
            for slide in &sb.slides {
                for ph in &slide.placeholders {
                    rtf_block_from_model(&ph.block, colors, res, first, out);
                }
            }
        }
        BlockKind::CodeBlock(cb) => {
            // One verbatim line per source line (preformatted text).
            for line in cb.code.split('\n') {
                rtf_plain_line(line, first, out);
            }
        }
        BlockKind::Blockquote(bq) => {
            // Set the quote off with a left indent; render its blocks inside it.
            rtf_par_sep(first, out);
            out.push_str("{\\li360 ");
            let mut inner_first = true;
            rtf_blocks_from_model(&bq.blocks, colors, res, &mut inner_first, out);
            out.push('}');
        }
        BlockKind::HorizontalRule => {
            // A paragraph carrying a bottom border renders as a horizontal rule.
            rtf_par_sep(first, out);
            out.push_str("{\\pard\\brdrb\\brdrs\\brdrw10\\par}");
        }
        // A block-level image emits a `\pict` group (PNG/JPEG only); a Shape's
        // vector geometry has no portable RTF mapping and is dropped.
        BlockKind::Image(img) => rtf_image_from_model(img, res, first, out),
        BlockKind::Shape(_) => {}
    }
}

fn rtf_par_sep(first: &mut bool, out: &mut String) {
    if *first {
        *first = false;
    } else {
        out.push_str("\\par\n");
    }
}

fn rtf_plain_line(text: &str, first: &mut bool, out: &mut String) {
    rtf_par_sep(first, out);
    rtf_escape(text, out);
}

fn rtf_para_from_model(
    para: &Paragraph,
    colors: &[[u8; 3]],
    res: &ResourceTable,
    force_bold: bool,
    first: &mut bool,
    out: &mut String,
) {
    rtf_par_sep(first, out);
    // Paragraph alignment control word.
    match para.style.align {
        crate::model::Align::Left => {}
        crate::model::Align::Center => out.push_str("\\qc "),
        crate::model::Align::Right => out.push_str("\\qr "),
        crate::model::Align::Justify => out.push_str("\\qj "),
    }
    for r in &para.runs {
        rtf_inline_from_model(r, colors, res, force_bold, out);
    }
}

fn rtf_inline_from_model(
    inline: &Inline,
    colors: &[[u8; 3]],
    res: &ResourceTable,
    force_bold: bool,
    out: &mut String,
) {
    match inline {
        Inline::Run(run) => {
            if run.text.is_empty() {
                return;
            }
            out.push('{');
            rtf_char_controls(&run.style, colors, force_bold, out);
            rtf_escape(&run.text, out);
            out.push('}');
        }
        Inline::LineBreak => out.push_str("\\line "),
        // An inline image emits its own `\pict` group inline (PNG/JPEG only).
        Inline::Image(img) => {
            let mut sink = true; // a `\pict` carries no paragraph separator
            rtf_image_from_model(img, res, &mut sink, out);
        }
        // A hyperlink lowers to an RTF field whose `\fldinst` carries the
        // `HYPERLINK "url"` target and whose `\fldrslt` holds the styled visible
        // children — mirroring the form the RTF importer reads back.
        Inline::Link { href, children } => {
            let url = match href {
                LinkTarget::Url(u) => u.clone(),
                // No portable RTF for an internal page jump → emit a fragment
                // anchor so the link is at least preserved as a field target.
                LinkTarget::Page(p) => format!("#page{}", p + 1),
            };
            out.push_str("{\\field{\\*\\fldinst HYPERLINK \"");
            rtf_escape(&url, out);
            out.push_str("\"}{\\fldrslt ");
            for c in children {
                rtf_inline_from_model(c, colors, res, force_bold, out);
            }
            out.push_str("}}");
        }
    }
}

/// RTF character control words for a run, opening a styled group.
fn rtf_char_controls(style: &CharStyle, colors: &[[u8; 3]], force_bold: bool, out: &mut String) {
    if style.bold || force_bold {
        out.push_str("\\b");
    }
    if style.italic {
        out.push_str("\\i");
    }
    if style.underline {
        out.push_str("\\ul");
    }
    if style.strike {
        out.push_str("\\strike");
    }
    if style.size_pt > 0.0 {
        // RTF font size is in half-points.
        out.push_str(&format!(
            "\\fs{}",
            (style.size_pt * 2.0).round().max(1.0) as i64
        ));
    }
    if let Some(c) = rtf_run_color(style) {
        if let Some(idx) = colors.iter().position(|x| *x == c) {
            out.push_str(&format!("\\cf{}", idx + 1)); // 1-based (0 = default)
        }
    }
    // Run highlight → `\highlightN` (N = 1-based colour-table index), the RTF
    // text-highlight control word — the inverse of an importer's highlight read.
    // Omitted entirely when the run has no background, so plain runs are unchanged.
    if let Some(c) = rtf_run_highlight(style) {
        if let Some(idx) = colors.iter().position(|x| *x == c) {
            out.push_str(&format!("\\highlight{}", idx + 1));
        }
    }
    out.push(' ');
}

fn rtf_heading_from_model(h: &Heading, colors: &[[u8; 3]], first: &mut bool, out: &mut String) {
    // Headings render bold; the level is conveyed by the bold styling + text.
    // A heading never contains images, so an empty resource table suffices.
    rtf_para_from_model(&h.para, colors, &ResourceTable::default(), true, first, out);
}

/// Width (in twips) of one nesting level's left indent. 360 twips = 0.25 inch,
/// the customary RTF list indent step.
const RTF_LIST_INDENT: i64 = 360;

/// Emit a list as ordered/unordered items with per-level indentation and running
/// counters. Each item is its own `\pard` paragraph: the marker (a bullet for an
/// unordered list, the running ordinal for an ordered one) is followed by the
/// item's first-paragraph runs; any further blocks of the item render nested.
fn rtf_list_from_model(
    list: &List,
    colors: &[[u8; 3]],
    res: &ResourceTable,
    first: &mut bool,
    out: &mut String,
) {
    // Per-level 1-based counters for ordered lists; reset when a deeper level is
    // re-entered (so a sub-list restarts at 1 / a / i …).
    let mut counters: Vec<u64> = Vec::new();
    for item in &list.items {
        let level = item.level as usize;
        if counters.len() <= level {
            counters.resize(level + 1, 0);
        } else {
            // Re-entering this level: drop any deeper counters so nested lists
            // restart on their next descent.
            counters.truncate(level + 1);
        }
        counters[level] += 1;

        // The marker text: a bullet for unordered, else the formatted ordinal.
        let marker = if list.ordered {
            format!(
                "{}.\\tab ",
                ordered_marker(list.marker, level, counters[level])
            )
        } else {
            // `\bullet` is the RTF bullet glyph control word; a tab follows it so
            // the text aligns past the marker.
            "\\bullet\\tab ".to_string()
        };

        let indent = RTF_LIST_INDENT * (level as i64 + 1);
        for (i, b) in item.blocks.iter().enumerate() {
            if i == 0 {
                if let BlockKind::Paragraph(p) = &b.kind {
                    rtf_par_sep(first, out);
                    // `\fi-NNN` pulls the marker into the hanging indent so wrapped
                    // lines align under the text, not the marker.
                    out.push_str(&format!(
                        "\\pard\\li{indent}\\fi-{RTF_LIST_INDENT} {marker}"
                    ));
                    for r in &p.runs {
                        rtf_inline_from_model(r, colors, res, false, out);
                    }
                    continue;
                }
            }
            // Non-paragraph leading block, or trailing blocks of the item.
            rtf_block_from_model(b, colors, res, first, out);
        }
    }
    // Reset paragraph defaults after the list so following blocks are not
    // indented by the trailing `\li`/`\fi`.
    out.push_str("\\pard ");
}

/// The marker glyph for an ordered list at a given depth: cycle decimal →
/// lower-alpha → lower-roman by nesting level (matching common word-processor
/// defaults), unless [`ListMarker`] pins an explicit style.
fn ordered_marker(marker: ListMarker, level: usize, n: u64) -> String {
    let style = match marker {
        ListMarker::Decimal => OrderedStyle::Decimal,
        ListMarker::LowerAlpha => OrderedStyle::LowerAlpha,
        ListMarker::UpperAlpha => OrderedStyle::UpperAlpha,
        ListMarker::LowerRoman => OrderedStyle::LowerRoman,
        ListMarker::UpperRoman => OrderedStyle::UpperRoman,
        // A bullet marker on an `ordered` list is contradictory; fall back to the
        // by-depth cycle so the running number is still meaningful.
        ListMarker::Bullet(_) => match level % 3 {
            0 => OrderedStyle::Decimal,
            1 => OrderedStyle::LowerAlpha,
            _ => OrderedStyle::LowerRoman,
        },
    };
    style.format(n)
}

#[derive(Clone, Copy)]
enum OrderedStyle {
    Decimal,
    LowerAlpha,
    UpperAlpha,
    LowerRoman,
    UpperRoman,
}

impl OrderedStyle {
    fn format(self, n: u64) -> String {
        match self {
            OrderedStyle::Decimal => n.to_string(),
            OrderedStyle::LowerAlpha => alpha(n, false),
            OrderedStyle::UpperAlpha => alpha(n, true),
            OrderedStyle::LowerRoman => roman(n, false),
            OrderedStyle::UpperRoman => roman(n, true),
        }
    }
}

/// Spreadsheet-style base-26 letters: 1→a, 26→z, 27→aa, …
fn alpha(mut n: u64, upper: bool) -> String {
    if n == 0 {
        return "0".to_string();
    }
    let base = if upper { b'A' } else { b'a' };
    let mut buf = Vec::new();
    while n > 0 {
        let rem = ((n - 1) % 26) as u8;
        buf.push(base + rem);
        n = (n - 1) / 26;
    }
    buf.reverse();
    String::from_utf8(buf).unwrap_or_default()
}

/// Roman numerals for 1..=3999; outside that range falls back to decimal.
fn roman(n: u64, upper: bool) -> String {
    if n == 0 || n > 3999 {
        return n.to_string();
    }
    const VALS: [(u64, &str); 13] = [
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
    let mut s = String::new();
    for (v, sym) in VALS {
        while n >= v {
            s.push_str(sym);
            n -= v;
        }
    }
    if upper {
        s.to_ascii_uppercase()
    } else {
        s
    }
}

/// Emit a table as a real RTF row grid: each row is a `\trowd` definition listing
/// the cumulative right edges (`\cellxN`, twips), then each cell's paragraphs
/// terminated by `\cell`, closed by `\row`.
fn rtf_table_from_model(
    table: &Table,
    colors: &[[u8; 3]],
    res: &ResourceTable,
    first: &mut bool,
    out: &mut String,
) {
    rtf_par_sep(first, out);
    for row in &table.rows {
        let n = row.cells.len();
        if n == 0 {
            continue;
        }
        // Cumulative right-edge boundaries (twips) for `\cellxN`. Use the model
        // column widths (points → twips) when present, else a uniform fallback so
        // the grid is still well-formed for readers that need the boundaries.
        let edges = cell_edges(&table.col_widths, n);

        out.push_str("\\trowd\\trgaph108");
        for e in &edges {
            out.push_str(&format!("\\cellx{e}"));
        }
        out.push('\n');

        for cell in &row.cells {
            // Each cell's block content, then the `\cell` terminator. Cells flow
            // their paragraphs; a `\par` between them keeps multi-paragraph cells
            // readable (RTF readers treat intra-cell `\par` as a line break).
            let mut cell_first = true;
            for b in &cell.blocks {
                rtf_cell_block(b, colors, res, &mut cell_first, out);
            }
            out.push_str("\\cell ");
        }
        out.push_str("\\row\n");
    }
    // Close the table context for following blocks.
    out.push_str("\\pard ");
}

/// Render one block inside a table cell. Paragraphs/headings emit their runs
/// inline (no leading `\pard` so the cell's text flows); richer blocks recurse.
fn rtf_cell_block(
    block: &Block,
    colors: &[[u8; 3]],
    res: &ResourceTable,
    cell_first: &mut bool,
    out: &mut String,
) {
    match &block.kind {
        BlockKind::Paragraph(p) => {
            if *cell_first {
                *cell_first = false;
            } else {
                out.push_str("\\par ");
            }
            for r in &p.runs {
                rtf_inline_from_model(r, colors, res, false, out);
            }
        }
        BlockKind::Heading(h) => {
            if *cell_first {
                *cell_first = false;
            } else {
                out.push_str("\\par ");
            }
            for r in &h.para.runs {
                rtf_inline_from_model(r, colors, res, true, out);
            }
        }
        // Any other block kind (nested list/table/image…) renders via the general
        // emitter; its own separators apply within the cell.
        other => {
            let mut inner_first = !*cell_first;
            rtf_block_from_model(
                &Block {
                    kind: other.clone(),
                    ..Block::default()
                },
                colors,
                res,
                &mut inner_first,
                out,
            );
            *cell_first = false;
        }
    }
}

/// Cumulative right-edge boundaries (twips) for `n` cells. Maps each
/// [`Table::col_widths`] entry (points) to twips and accumulates; when widths are
/// missing/short, the remaining columns split a default 9360-twip (6.5") content
/// width evenly so the grid stays well-formed.
fn cell_edges(col_widths: &[f64], n: usize) -> Vec<i64> {
    // 6.5 inch (9360-twip) usable content width, split across unspecified columns.
    const DEFAULT_CONTENT_TWIPS: i64 = 9360;
    // Known widths in twips (clamped non-negative).
    let known: Vec<i64> = col_widths
        .iter()
        .take(n)
        .map(|w| (w.max(0.0) * 20.0).round() as i64)
        .collect();
    let known_sum: i64 = known.iter().sum();
    let missing = n.saturating_sub(known.len());
    // Each unspecified column gets an even share of whatever default width is
    // left after the known columns (never below a small minimum).
    let fill = if missing > 0 {
        ((DEFAULT_CONTENT_TWIPS - known_sum).max((missing as i64) * 200)) / missing as i64
    } else {
        0
    };

    let mut edges = Vec::with_capacity(n);
    let mut acc = 0i64;
    for i in 0..n {
        let w = known.get(i).copied().unwrap_or(fill).max(200);
        acc += w;
        edges.push(acc);
    }
    edges
}

/// Emit a `{\pict …}` group for an image resource, when it is a PNG or JPEG
/// (the formats RTF can carry). The display size comes from the pixel
/// dimensions (96 dpi → twips). Other formats (GIF/WebP/AVIF/…) are skipped.
fn rtf_image_from_model(img: &ImageRef, res: &ResourceTable, first: &mut bool, out: &mut String) {
    let Some(resource) = res.images.get(&img.resource) else {
        return;
    };
    let bytes = &resource.bytes;
    // Detect the real format from the header bytes (authoritative over the tag),
    // and the pixel dimensions for the `\picwgoal`/`\pichgoal` display size.
    let blip = match crate::convert::import::image_dimensions(bytes) {
        Some((w, h, "png")) => Some(("pngblip", w, h)),
        Some((w, h, "jpeg")) => Some(("jpegblip", w, h)),
        // GIF/WebP/AVIF have no RTF picture encoding → skip.
        _ => None,
    };
    let Some((blip, w, h)) = blip else {
        return;
    };

    rtf_par_sep(first, out);
    // 96 dpi: px → pt = px * 72/96 = px * 0.75; pt → twips = pt * 20. So
    // twips = px * 15. Guard against a zero goal (some readers ignore a 0 goal).
    let goalw = (w as i64 * 15).max(15);
    let goalh = (h as i64 * 15).max(15);
    out.push_str(&format!(
        "{{\\pict\\{blip}\\picwgoal{goalw}\\pichgoal{goalh} "
    ));
    push_hex(bytes, out);
    out.push('}');
}

/// Append `bytes` as lowercase hex digit pairs (RTF's default `\pict` payload
/// encoding), wrapping lines to keep the output readable.
fn push_hex(bytes: &[u8], out: &mut String) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && i % 40 == 0 {
            out.push('\n');
        }
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
}

/// Extract plain text paragraphs from an RTF document (minimal control-word
/// parser: handles groups, `\par`, `\'xx` hex bytes, `\uN` unicode, skips other
/// control words and the font/color tables). The RTF→model importer now routes
/// through the rich parser ([`super::rtf::rtf_to_model`]); this text-only
/// extractor is retained for the round-trip test below.
#[cfg(test)]
pub(crate) fn rtf_to_paragraphs(rtf: &str) -> Vec<String> {
    let bytes = rtf.as_bytes();
    let mut paras = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    let mut skip_group_depth: Option<i32> = None;
    let mut depth = 0i32;
    let mut uc_count = 1i64; // `\ucN`: fallback chars to skip after each `\uN`

    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'{' => {
                depth += 1;
                i += 1;
            }
            b'}' => {
                if let Some(d) = skip_group_depth {
                    if depth <= d {
                        skip_group_depth = None;
                    }
                }
                depth -= 1;
                i += 1;
            }
            b'\\' => {
                // Control word / symbol.
                if i + 1 < bytes.len() && !bytes[i + 1].is_ascii_alphanumeric() {
                    match bytes[i + 1] {
                        b'\'' if i + 3 < bytes.len() => {
                            let hex = &rtf[i + 2..i + 4];
                            if let Ok(b) = u8::from_str_radix(hex, 16) {
                                if skip_group_depth.is_none() {
                                    // WinAnsi byte → char.
                                    cur.push(b as char);
                                }
                            }
                            i += 4;
                        }
                        b'\\' | b'{' | b'}' => {
                            if skip_group_depth.is_none() {
                                cur.push(bytes[i + 1] as char);
                            }
                            i += 2;
                        }
                        _ => i += 2,
                    }
                    continue;
                }
                // Alphabetic control word + optional numeric parameter.
                let start = i + 1;
                let mut j = start;
                while j < bytes.len() && bytes[j].is_ascii_alphabetic() {
                    j += 1;
                }
                let word = &rtf[start..j];
                let mut k = j;
                let mut neg = false;
                if k < bytes.len() && bytes[k] == b'-' {
                    neg = true;
                    k += 1;
                }
                let num_start = k;
                while k < bytes.len() && bytes[k].is_ascii_digit() {
                    k += 1;
                }
                let param: Option<i64> =
                    rtf[num_start..k]
                        .parse()
                        .ok()
                        .map(|n: i64| if neg { -n } else { n });
                let mut fallback_skip = 0i64;
                match word {
                    "par" => {
                        if skip_group_depth.is_none() {
                            paras.push(std::mem::take(&mut cur));
                        }
                    }
                    "uc" => {
                        if let Some(n) = param {
                            uc_count = n.max(0);
                        }
                    }
                    "u" => {
                        if let Some(n) = param {
                            if skip_group_depth.is_none() {
                                let code = if n < 0 {
                                    (n + 0x10000) as u32
                                } else {
                                    n as u32
                                };
                                if let Some(ch) = char::from_u32(code) {
                                    cur.push(ch);
                                }
                            }
                            fallback_skip = uc_count;
                        }
                    }
                    "fonttbl" | "colortbl" | "stylesheet" | "info" | "pict" | "object" => {
                        skip_group_depth = Some(depth);
                    }
                    _ => {}
                }
                // A single space after a control word is its delimiter — consume it.
                if k < bytes.len() && bytes[k] == b' ' {
                    k += 1;
                }
                // Skip the `\uc`-count fallback characters that follow a `\uN`.
                for _ in 0..fallback_skip {
                    if k >= bytes.len() {
                        break;
                    }
                    let mut adv = 1;
                    while k + adv < bytes.len() && (bytes[k + adv] & 0xC0) == 0x80 {
                        adv += 1;
                    }
                    k += adv;
                }
                i = k;
            }
            b'\r' | b'\n' => i += 1,
            _ => {
                if skip_group_depth.is_none() {
                    cur.push(c as char);
                }
                i += 1;
            }
        }
    }
    if !cur.trim().is_empty() {
        paras.push(cur);
    }
    paras
        .into_iter()
        .map(|p| p.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|p| !p.is_empty())
        .collect()
}

/// RTF → PDF.
///
/// Routed through the stateful RTF parser ([`super::rtf::rtf_to_html`]), which
/// recovers character formatting (bold/italic/underline/strike, colour, font,
/// size), paragraph alignment/indents and tables, then renders the styled HTML
/// with the in-house [`crate::html`] engine. (The RTF→model importer is the rich
/// [`super::rtf::rtf_to_model`].)
pub fn rtf_to_pdf(rtf: &str) -> Vec<u8> {
    let html = super::rtf::rtf_to_html(rtf);
    crate::html::render(&html, &[], 612.0, 792.0, 36.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opens(pdf: &[u8]) -> crate::Document {
        crate::Document::open(pdf).expect("valid PDF")
    }

    #[test]
    fn unescape_decodes_entities() {
        assert_eq!(
            unescape("a &amp; b &lt;c&gt; &#65; &#x42;"),
            "a & b <c> A B"
        );
    }

    #[test]
    fn xml_paragraphs_strip_tags_and_split() {
        let xml = "<w:p><w:r><w:t>Hello</w:t></w:r></w:p><w:p><w:t>World &amp; co</w:t></w:p>";
        let paras = paragraphs_from_xml(xml, &["</w:p>"], &[]);
        assert_eq!(paras, vec!["Hello".to_string(), "World & co".to_string()]);
    }

    #[test]
    fn txt_to_pdf_is_valid_and_has_text() {
        let pdf = txt_to_pdf("First line\nSecond line\nThird");
        let doc = opens(&pdf);
        assert!(doc.page_count() >= 1);
        let text = doc.to_text();
        assert!(
            text.contains("Second line"),
            "text round-trips into the PDF"
        );
    }

    #[test]
    fn rtf_round_trips_text() {
        let rtf = to_rtf(&["Café déjà".to_string(), "Second \\ {brace}".to_string()]);
        let s = String::from_utf8(rtf).unwrap();
        assert!(s.starts_with("{\\rtf1"));
        let back = rtf_to_paragraphs(&s);
        assert_eq!(
            back,
            vec!["Café déjà".to_string(), "Second \\ {brace}".to_string()]
        );
    }

    #[test]
    fn rtf_from_model_emits_run_highlight() {
        use crate::model::{InlineRun, Page, Section};
        let doc = Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![Block {
                        kind: BlockKind::Paragraph(Paragraph {
                            runs: vec![Inline::Run(InlineRun {
                                text: "lit".to_string(),
                                style: CharStyle {
                                    background: Some([1.0, 1.0, 0.0]),
                                    ..CharStyle::default()
                                },
                                source_index: None,
                            })],
                            ..Paragraph::default()
                        }),
                        ..Default::default()
                    }],
                    absolute: false,
                }],
                ..Section::default()
            }],
            ..Document::default()
        };
        let rtf = String::from_utf8(rtf_from_model(&doc)).unwrap();
        // The highlight colour joins the colour table…
        assert!(
            rtf.contains("\\red255\\green255\\blue0;"),
            "yellow registered in the colour table: {rtf}"
        );
        // …and the run references it via `\highlight` (1-based index).
        assert!(
            rtf.contains("\\highlight1"),
            "run carries the highlight control word: {rtf}"
        );

        // A plain run emits no highlight at all.
        let plain = String::from_utf8(rtf_from_model(&Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![Block {
                        kind: BlockKind::Paragraph(Paragraph {
                            runs: vec![Inline::Run(InlineRun {
                                text: "plain".to_string(),
                                ..Default::default()
                            })],
                            ..Paragraph::default()
                        }),
                        ..Default::default()
                    }],
                    absolute: false,
                }],
                ..Section::default()
            }],
            ..Document::default()
        }))
        .unwrap();
        assert!(
            !plain.contains("\\highlight"),
            "a plain run carries no highlight: {plain}"
        );
    }

    // ── rich model → RTF: tables, lists, images, links (#4) ──

    /// Build a one-block document (no resources) for the structural tests.
    fn doc_with_block(kind: BlockKind) -> Document {
        use crate::model::{Page, Section};
        Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![Block {
                        kind,
                        ..Default::default()
                    }],
                    absolute: false,
                }],
                ..Section::default()
            }],
            ..Document::default()
        }
    }

    /// A plain styled run wrapped in a paragraph block.
    fn para_block(text: &str) -> Block {
        use crate::model::InlineRun;
        Block {
            kind: BlockKind::Paragraph(Paragraph {
                runs: vec![Inline::Run(InlineRun {
                    text: text.to_string(),
                    ..Default::default()
                })],
                ..Paragraph::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn model_table_emits_real_rtf_row_grid() {
        use crate::model::{Cell, Row};
        let mk_cell = |t: &str| Cell {
            blocks: vec![para_block(t)],
            ..Cell::default()
        };
        let table = Table {
            rows: vec![
                Row {
                    cells: vec![mk_cell("A1"), mk_cell("B1")],
                    height: None,
                },
                Row {
                    cells: vec![mk_cell("A2"), mk_cell("B2")],
                    height: None,
                },
            ],
            col_widths: vec![100.0, 200.0],
            ..Table::default()
        };
        let rtf =
            String::from_utf8(rtf_from_model(&doc_with_block(BlockKind::Table(table)))).unwrap();

        // A true grid, not tab-separated lines.
        assert!(rtf.contains("\\trowd"), "row definition present: {rtf}");
        assert!(rtf.contains("\\cellx"), "cell boundaries present: {rtf}");
        assert!(rtf.contains("\\cell "), "cell terminators present: {rtf}");
        assert!(rtf.contains("\\row"), "row terminators present: {rtf}");
        assert_eq!(rtf.matches("\\row").count(), 2, "two rows emitted: {rtf}");
        assert_eq!(
            rtf.matches("\\cell ").count(),
            4,
            "four cells emitted: {rtf}"
        );
        // 100pt → 2000 twips; cumulative 100+200pt → 6000 twips.
        assert!(rtf.contains("\\cellx2000"), "first column edge: {rtf}");
        assert!(rtf.contains("\\cellx6000"), "second column edge: {rtf}");
        assert!(!rtf.contains("\\tab "), "no flattened tab lines: {rtf}");

        // Round-trips through the rich importer as a 2×2 table.
        let doc = crate::convert::rtf::rtf_to_model(&rtf);
        let t = doc.sections[0].pages[0]
            .blocks
            .iter()
            .find_map(|b| match &b.kind {
                BlockKind::Table(t) => Some(t),
                _ => None,
            })
            .expect("a BlockKind::Table on re-import");
        assert_eq!(t.rows.len(), 2, "two rows round-trip");
        assert_eq!(t.rows[0].cells.len(), 2, "two cells per row round-trip");
    }

    #[test]
    fn model_ordered_nested_list_emits_numbers_and_indents() {
        use crate::model::ListItem;
        let item = |text: &str, level: u8| ListItem {
            blocks: vec![para_block(text)],
            level,
        };
        // 1. top-A / 2. top-B / (nested) 1. child / 3. top-C — an explicit
        // decimal marker is honoured at every depth; the nested counter restarts.
        let list = List {
            ordered: true,
            marker: ListMarker::Decimal,
            items: vec![
                item("top-A", 0),
                item("top-B", 0),
                item("child", 1),
                item("top-C", 0),
            ],
        };
        let rtf =
            String::from_utf8(rtf_from_model(&doc_with_block(BlockKind::List(list)))).unwrap();

        // Ordered: running decimal at the top level (not `\bullet`).
        assert!(
            rtf.contains("\\li360\\fi-360 1.\\tab "),
            "first ordinal: {rtf}"
        );
        assert!(
            rtf.contains("\\li360\\fi-360 2.\\tab "),
            "second ordinal: {rtf}"
        );
        assert!(
            !rtf.contains("\\bullet"),
            "ordered list has no bullet: {rtf}"
        );
        // Nested item: deeper indent (2 × 360) + a restarted counter ("1.").
        assert!(
            rtf.contains("\\li720\\fi-360 1.\\tab "),
            "nested item indents deeper and restarts at 1: {rtf}"
        );
        // After the nested item, the top level resumes at 3.
        assert!(
            rtf.contains("\\li360\\fi-360 3.\\tab "),
            "top counter resumes after nesting: {rtf}"
        );

        // When the marker is *not* pinned (a bullet glyph on an ordered list),
        // the depth-cycle kicks in: level-0 decimal, level-1 lower-alpha.
        let cycle = List {
            ordered: true,
            marker: ListMarker::default(), // Bullet('•')
            items: vec![item("a", 0), item("b", 1)],
        };
        let rtf2 =
            String::from_utf8(rtf_from_model(&doc_with_block(BlockKind::List(cycle)))).unwrap();
        assert!(
            rtf2.contains("\\li360\\fi-360 1.\\tab "),
            "depth-0 decimal: {rtf2}"
        );
        assert!(
            rtf2.contains("\\li720\\fi-360 a.\\tab "),
            "depth-1 lower-alpha by cycle: {rtf2}"
        );
    }

    #[test]
    fn model_unordered_list_emits_bullets() {
        use crate::model::ListItem;
        let list = List {
            ordered: false,
            marker: ListMarker::Bullet('•'),
            items: vec![
                ListItem {
                    blocks: vec![para_block("one")],
                    level: 0,
                },
                ListItem {
                    blocks: vec![para_block("two")],
                    level: 0,
                },
            ],
        };
        let rtf =
            String::from_utf8(rtf_from_model(&doc_with_block(BlockKind::List(list)))).unwrap();
        assert_eq!(
            rtf.matches("\\bullet").count(),
            2,
            "a bullet per item: {rtf}"
        );
        assert!(
            !rtf.contains("1.\\tab"),
            "unordered list has no numbers: {rtf}"
        );
    }

    #[test]
    fn model_png_image_emits_pict_pngblip() {
        use crate::model::{ImageResource, Page, Section};
        // A real 2×2 PNG interned in the resource table.
        let rgba = vec![
            0u8, 0, 255, 255, 255, 0, 0, 255, 0, 255, 0, 255, 255, 255, 0, 255,
        ];
        let png = crate::raster::encode_png(2, 2, &rgba);
        let mut images = std::collections::BTreeMap::new();
        images.insert(
            7u64,
            ImageResource {
                bytes: png.clone(),
                format: "png".to_string(),
            },
        );
        let doc = Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![Block {
                        kind: BlockKind::Image(ImageRef {
                            resource: 7,
                            alt: None,
                        }),
                        ..Default::default()
                    }],
                    absolute: false,
                }],
                ..Section::default()
            }],
            resources: ResourceTable { images },
            ..Document::default()
        };
        let rtf = String::from_utf8(rtf_from_model(&doc)).unwrap();

        assert!(rtf.contains("{\\pict\\pngblip"), "PNG picture group: {rtf}");
        assert!(
            rtf.contains("\\picwgoal"),
            "display width goal present: {rtf}"
        );
        assert!(
            rtf.contains("\\pichgoal"),
            "display height goal present: {rtf}"
        );
        // The payload is the hex of the PNG bytes (96 dpi: 2px → 30 twips).
        assert!(rtf.contains("\\picwgoal30"), "2px → 30 twips width: {rtf}");

        // Round-trips: the importer re-interns the same PNG bytes.
        let back = crate::convert::rtf::rtf_to_model(&rtf);
        let img = back.sections[0].pages[0]
            .blocks
            .iter()
            .find_map(|b| match &b.kind {
                BlockKind::Image(i) => Some(i),
                _ => None,
            })
            .expect("a BlockKind::Image on re-import");
        let res = back
            .resources
            .images
            .get(&img.resource)
            .expect("image bytes re-interned");
        assert_eq!(res.bytes, png, "PNG bytes survive the round-trip");
        assert_eq!(res.format, "png");
    }

    #[test]
    fn model_non_raster_image_is_skipped() {
        use crate::model::{ImageResource, Page, Section};
        // A GIF has no RTF picture encoding → no `\pict`, no leaked hex.
        let gif = b"GIF89a\x01\x00\x01\x00\x00\x00\x00;".to_vec();
        let mut images = std::collections::BTreeMap::new();
        images.insert(
            3u64,
            ImageResource {
                bytes: gif,
                format: "gif".to_string(),
            },
        );
        let doc = Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![
                        para_block("before"),
                        Block {
                            kind: BlockKind::Image(ImageRef {
                                resource: 3,
                                alt: None,
                            }),
                            ..Default::default()
                        },
                        para_block("after"),
                    ],
                    absolute: false,
                }],
                ..Section::default()
            }],
            resources: ResourceTable { images },
            ..Document::default()
        };
        let rtf = String::from_utf8(rtf_from_model(&doc)).unwrap();
        assert!(!rtf.contains("\\pict"), "GIF emits no picture: {rtf}");
        assert!(
            rtf.contains("before") && rtf.contains("after"),
            "surrounding text kept: {rtf}"
        );
    }

    #[test]
    fn model_link_emits_hyperlink_field() {
        use crate::model::InlineRun;
        // A paragraph whose run is a hyperlink wrapping styled text.
        let para = Paragraph {
            runs: vec![Inline::Link {
                href: LinkTarget::Url("https://example.com".to_string()),
                children: vec![Inline::Run(InlineRun {
                    text: "click".to_string(),
                    style: CharStyle {
                        underline: true,
                        ..CharStyle::default()
                    },
                    source_index: None,
                })],
            }],
            ..Paragraph::default()
        };
        let rtf =
            String::from_utf8(rtf_from_model(&doc_with_block(BlockKind::Paragraph(para)))).unwrap();

        assert!(
            rtf.contains("{\\field{\\*\\fldinst HYPERLINK \"https://example.com\"}"),
            "HYPERLINK field instruction: {rtf}"
        );
        assert!(
            rtf.contains("\\fldrslt"),
            "field result group present: {rtf}"
        );
        assert!(rtf.contains("click"), "visible link text present: {rtf}");

        // Round-trips: the importer recovers an Inline::Link to the same URL.
        let doc = crate::convert::rtf::rtf_to_model(&rtf);
        let link = doc.sections[0].pages[0].blocks.iter().find_map(|b| {
            if let BlockKind::Paragraph(p) = &b.kind {
                p.runs.iter().find_map(|i| match i {
                    Inline::Link { href, children } => Some((href.clone(), children.clone())),
                    _ => None,
                })
            } else {
                None
            }
        });
        let (href, _children) = link.expect("an Inline::Link on re-import");
        assert_eq!(
            href,
            LinkTarget::Url("https://example.com".to_string()),
            "link target round-trips"
        );
    }

    #[test]
    fn rtf_to_pdf_renders_styled_content_and_table() {
        // Bold + italic runs, a coloured run, and a two-cell table row — the
        // whole stateful-parser → HTML engine → PDF pipeline end to end.
        let rtf = r"{\rtf1\ansi{\colortbl ;\red255\green0\blue0;}\qc {\b Titre}\par
\ql {\i italique} et {\cf1 rouge}\par
\trowd \cell Cellule A\cell Cellule B\row}";
        let pdf = rtf_to_pdf(rtf);
        let doc = opens(&pdf);
        assert!(doc.page_count() >= 1, "valid PDF with a page");
        let text = doc.to_text();
        assert!(text.contains("Titre"), "heading text present: {text:?}");
        assert!(text.contains("italique"), "italic run present: {text:?}");
        assert!(text.contains("rouge"), "coloured run present: {text:?}");
        // Cell text may wrap across extracted lines ("Cellule\nA"); check words.
        for word in ["Cellule", "A", "B"] {
            assert!(
                text.contains(word),
                "table cell word {word:?} present: {text:?}"
            );
        }
    }

    #[test]
    fn rtf_to_pdf_decodes_cp1252_in_styled_html() {
        // The RTF layer's responsibility: \'80 → € in the styled HTML handed to
        // the renderer (the downstream PDF-text round-trip of U+20AC is a shared
        // html-engine/font concern, exercised by the html module's own tests).
        let html = super::super::rtf::rtf_to_html(r"{\rtf1\ansi Prix: 10\'80\par}");
        assert!(html.contains('€'), "euro decoded into HTML: {html}");
        // And the pipeline still yields a valid PDF.
        let pdf = rtf_to_pdf(r"{\rtf1\ansi Prix: 10\'80\par}");
        assert!(opens(&pdf).page_count() >= 1);
    }

    #[test]
    fn long_paragraph_wraps_across_lines() {
        let lines = wrap(&"word ".repeat(60), 40);
        assert!(lines.len() > 1, "wrapped into multiple lines");
        assert!(lines.iter().all(|l| l.chars().count() <= 40));
    }

    #[test]
    fn image_to_pdf_embeds_a_png() {
        // A 4×3 opaque red PNG → a valid one-page PDF carrying an image XObject.
        let rgba = [255u8, 0, 0, 255].repeat(4 * 3);
        let png = crate::raster::png::encode_png(4, 3, &rgba);
        let pdf = image_to_pdf(&png).expect("png → pdf");
        assert!(pdf.starts_with(b"%PDF-"), "valid PDF header");
        let doc = opens(&pdf);
        assert_eq!(doc.page_count(), 1, "one-page document");
        // The image is embedded as a real /Image XObject (DeviceRGB stream).
        let imgs = doc.page_image_elements(1);
        assert!(!imgs.is_empty(), "image embedded on the page");

        // Garbage is rejected.
        assert!(image_to_pdf(b"not an image").is_none());
    }

    #[test]
    fn image_to_pdf_embeds_a_transparent_rgba_png() {
        // 8×8 RGBA with half the pixels semi-transparent → exercises the SMask
        // (soft-mask) path: the image must still embed and the PDF be valid.
        let mut rgba = Vec::new();
        for i in 0..(8 * 8) {
            rgba.extend_from_slice(&[
                (i * 3) as u8,
                (i * 5) as u8,
                (i * 7) as u8,
                if i % 2 == 0 { 128 } else { 255 },
            ]);
        }
        let png = crate::raster::png::encode_png(8, 8, &rgba);
        let pdf = image_to_pdf(&png).expect("transparent RGBA png → pdf");
        assert!(pdf.starts_with(b"%PDF-"), "valid PDF header");
        let doc = opens(&pdf);
        assert_eq!(doc.page_count(), 1);
        assert!(
            !doc.page_image_elements(1).is_empty(),
            "RGBA image embedded on the page"
        );
    }

    #[test]
    fn image_to_pdf_embeds_a_16bit_rgba_png() {
        // Forge a 2×2, 16-bit RGBA PNG (real tools emit depth-16); it must no
        // longer yield an empty buffer.
        let png = make_test_png_16bit_rgba();
        let pdf = image_to_pdf(&png).expect("16-bit RGBA png → pdf (no empty buffer)");
        assert!(pdf.starts_with(b"%PDF-"), "valid PDF header");
        let doc = opens(&pdf);
        assert!(
            !doc.page_image_elements(1).is_empty(),
            "16-bit image embedded on the page"
        );
    }

    #[test]
    fn image_to_pdf_embeds_a_greyscale_png() {
        // 4×4, 8-bit greyscale (colour type 0) PNG → opaque grey image.
        let png = make_test_png_grey_8bit(4, 4);
        let pdf = image_to_pdf(&png).expect("greyscale png → pdf");
        assert!(pdf.starts_with(b"%PDF-"), "valid PDF header");
        let doc = opens(&pdf);
        assert!(
            !doc.page_image_elements(1).is_empty(),
            "greyscale image embedded on the page"
        );
    }

    // ── Tiny spec-conformant PNG forgers (filter-0 + zlib-stored IDAT) ─────

    fn png_crc32(bytes: &[u8]) -> u32 {
        let mut crc: u32 = 0xFFFF_FFFF;
        for &b in bytes {
            crc ^= b as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        crc ^ 0xFFFF_FFFF
    }

    fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let mut crc_in = Vec::new();
        crc_in.extend_from_slice(kind);
        crc_in.extend_from_slice(data);
        out.extend_from_slice(&png_crc32(&crc_in).to_be_bytes());
    }

    fn png_zlib_store(data: &[u8]) -> Vec<u8> {
        let mut out = vec![0x78, 0x01, 0x01];
        out.extend_from_slice(&(data.len() as u16).to_le_bytes());
        out.extend_from_slice(&(!(data.len() as u16)).to_le_bytes());
        out.extend_from_slice(data);
        let (mut a, mut b) = (1u32, 0u32);
        for &byte in data {
            a = (a + byte as u32) % 65521;
            b = (b + a) % 65521;
        }
        out.extend_from_slice(&((b << 16) | a).to_be_bytes());
        out
    }

    fn make_test_png(w: u32, h: u32, depth: u8, ct: u8, idat: &[u8]) -> Vec<u8> {
        let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&w.to_be_bytes());
        ihdr.extend_from_slice(&h.to_be_bytes());
        ihdr.extend_from_slice(&[depth, ct, 0, 0, 0]);
        png_chunk(&mut out, b"IHDR", &ihdr);
        png_chunk(&mut out, b"IDAT", &png_zlib_store(idat));
        png_chunk(&mut out, b"IEND", &[]);
        out
    }

    fn make_test_png_16bit_rgba() -> Vec<u8> {
        // 2×2, each pixel 4 channels × 2 bytes, filter byte 0 per row.
        let mut idat = Vec::new();
        for _row in 0..2 {
            idat.push(0u8);
            for px in 0..2u16 {
                for ch in 0..4u16 {
                    // (px+1)*(ch+1)*8000 ≤ 64000 < 65536, fits a 16-bit sample.
                    let v: u16 = (px + 1) * (ch + 1) * 8000;
                    idat.extend_from_slice(&v.to_be_bytes());
                }
            }
        }
        make_test_png(2, 2, 16, 6, &idat)
    }

    fn make_test_png_grey_8bit(w: u32, h: u32) -> Vec<u8> {
        let mut idat = Vec::new();
        for y in 0..h {
            idat.push(0u8);
            for x in 0..w {
                idat.push(((x * y) & 0xFF) as u8);
            }
        }
        make_test_png(w, h, 8, 0, &idat)
    }

    #[test]
    fn transcoded_transparent_webp_keeps_alpha_as_smask() {
        // A WebP (transcode path) with semi-transparent pixels must end up with
        // an /SMask soft mask in the PDF — i.e. transparency is NOT flattened.
        let mut rgba = Vec::new();
        for i in 0..(4 * 4) {
            rgba.extend_from_slice(&[
                (i * 9) as u8,
                (i * 5) as u8,
                (i * 3) as u8,
                if i % 2 == 0 { 64 } else { 255 },
            ]);
        }
        let webp = crate::raster::webp::encode_webp(4, 4, &rgba);
        let pdf = image_to_pdf(&webp).expect("transparent webp → pdf");
        assert!(pdf.starts_with(b"%PDF-"));
        let has_smask = pdf.windows(6).any(|w| w == b"/SMask");
        assert!(
            has_smask,
            "transparent WebP must embed an /SMask (alpha preserved, not flattened)"
        );
    }

    #[test]
    fn image_to_pdf_transcodes_avif() {
        // AVIF is decoded to RGBA and re-encoded as PNG before embedding (the
        // 32×32 still fixture shared with the raster::avif tests).
        let avif = include_bytes!("../raster/fixtures/av1test.avif");
        let pdf = image_to_pdf(avif).expect("avif → pdf");
        assert!(pdf.starts_with(b"%PDF-"), "valid PDF header");
        let doc = opens(&pdf);
        assert_eq!(doc.page_count(), 1);
        assert!(
            !doc.page_image_elements(1).is_empty(),
            "transcoded AVIF embedded as an image"
        );
    }
}
