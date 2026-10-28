//! Structured exporters from the unified [`model::Document`] — Track B of the
//! Phase 5 model epic.
//!
//! Unlike the [`super::office`] / [`super::web`] exporters (which place
//! absolutely-positioned text boxes recovered from a *PDF*), these serialize the
//! semantic [`model`](crate::model) tree into **real flowing structure**: one
//! `<w:p>` per paragraph/heading, a real `<w:tbl>` with column widths and
//! `w:gridSpan`/`w:vMerge`, list numbering, headers/footers — i.e. what an
//! office suite produces for a document authored in it, not an import of a
//! flattened page.
//!
//! They live beside the `*(&[ConvPage])` exporters and reuse the shared ZIP
//! container ([`super::zip`]) and the OOXML/ODF/DrawingML helpers in
//! [`super::office`]; only the *content* XML is built here.

use crate::content::num;
use crate::content::vector::PathSeg;
use crate::convert::office::{
    col_letter, dml_cust_geom, dml_fill, dml_line, docx_vml_shape, emu, esc, odf_path_d,
    odf_shape_style, office_image_format, shape_is_rect, twips,
};
use crate::convert::zip::ZipWriter;
use crate::convert::PlacedShape;
use crate::model::sheet::{MergeRange, SheetRow};
use crate::model::CellVAlign;
use crate::model::{
    Align, Block, BlockKind, Blockquote, BorderStyle, Cell, CharStyle, CodeBlock, Document,
    Heading, ImageRef, Inline, InlineRun, LineHeight, LinkTarget, List, ListMarker, ParaBorder,
    Paragraph, Row, Section, Shape, Sheet, SheetBlock, SheetCell, Slide, SlideBlock, Table,
    TextBox, VAlign,
};
use crate::model::{
    CellValue, NamedStyle, PageGeometry, ParagraphStyle, PlaceholderRole, StyleId, StyleTable,
};

// ───────────────────────────── shared model walkers ─────────────────────────────

/// An RGB triple (`0..=1`) as an upper-case `RRGGBB` hex string.
fn hex(rgb: [f64; 3]) -> String {
    let q = |c: f64| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("{:02X}{:02X}{:02X}", q(rgb[0]), q(rgb[1]), q(rgb[2]))
}

/// A char-style's colour as hex when **explicitly set**, else `None`.
///
/// The model distinguishes an explicitly-chosen colour (`CharStyle.color =
/// Some(_)`, even pure/near-black `Some([0,0,0])`) from an unset/default run
/// (`None`). A document that deliberately paints a run black must round-trip
/// that intent, so any `Some` colour emits its tag; only `None` is omitted (the
/// format's own default — black — then applies). Used by the Office exporters
/// (DOCX/ODT/PPTX/XLSX/ODS), where an explicit colour property is invisible
/// structure, not noise. The markup exporters use [`markup_color`] instead.
fn visible_color(style: &CharStyle) -> Option<String> {
    style.color.map(hex)
}

/// A char-style's colour as hex for **inline markup** (Markdown's HTML span,
/// EPUB's XHTML `style`), only when it is set and not (near-)black.
///
/// Text extracted from a PDF always carries a colour, almost always black, so
/// keying off `Some` here wrapped every run of an ordinary document in
/// `<span style="color:#000000">`. In markup that inline colour is pure noise,
/// and in EPUB it actively harms: a hard-coded black defeats the reader's dark
/// and sepia themes. Near-black is therefore left to the format default, with
/// the same threshold the RTF exporter uses
/// ([`crate::convert::style::is_non_black`]).
fn markup_color(style: &CharStyle) -> Option<String> {
    style
        .color
        .filter(|&rgb| crate::convert::style::is_non_black(rgb))
        .map(hex)
}

/// Default body font size in points when a run carries none (`size_pt == 0`).
const DEFAULT_SIZE_PT: f64 = 11.0;

fn run_size(style: &CharStyle) -> f64 {
    if style.size_pt > 0.0 {
        style.size_pt
    } else {
        DEFAULT_SIZE_PT
    }
}

// ─────────────────────────── document properties (meta) ───────────────────────────

/// Application name stamped into OOXML `docProps/app.xml` and ODF
/// `meta:generator`.
const META_GENERATOR: &str = "GigaPDF";

/// OOXML `docProps/core.xml` (`cp:coreProperties`, ECMA-376 Part 2 §11) from the
/// model metadata. Absent fields are omitted (never fabricated). `dc:title` /
/// `dc:creator` / `dc:subject` / `cp:keywords` map directly; `dc:language`
/// carries the BCP-47 tag when set.
fn ooxml_core_props(meta: &crate::model::DocMeta) -> String {
    let mut body = String::new();
    if let Some(title) = meta.title.as_deref().filter(|s| !s.is_empty()) {
        let mut v = String::new();
        esc(title, &mut v);
        body.push_str(&format!("<dc:title>{v}</dc:title>"));
    }
    if let Some(author) = meta.author.as_deref().filter(|s| !s.is_empty()) {
        let mut v = String::new();
        esc(author, &mut v);
        body.push_str(&format!("<dc:creator>{v}</dc:creator>"));
    }
    if let Some(subject) = meta.subject.as_deref().filter(|s| !s.is_empty()) {
        let mut v = String::new();
        esc(subject, &mut v);
        body.push_str(&format!("<dc:subject>{v}</dc:subject>"));
    }
    if !meta.keywords.is_empty() {
        let mut v = String::new();
        esc(&meta.keywords.join(", "), &mut v);
        body.push_str(&format!("<cp:keywords>{v}</cp:keywords>"));
    }
    if let Some(lang) = meta.lang.as_deref().filter(|s| !s.is_empty()) {
        let mut v = String::new();
        esc(lang, &mut v);
        body.push_str(&format!("<dc:language>{v}</dc:language>"));
    }
    if !meta.description.is_empty() {
        let mut v = String::new();
        esc(&meta.description, &mut v);
        body.push_str(&format!("<dc:description>{v}</dc:description>"));
    }
    if !meta.created.is_empty() {
        body.push_str(&format!(
            "<dcterms:created xsi:type=\"dcterms:W3CDTF\">{}</dcterms:created>",
            meta.created
        ));
    }
    if !meta.modified.is_empty() {
        body.push_str(&format!(
            "<dcterms:modified xsi:type=\"dcterms:W3CDTF\">{}</dcterms:modified>",
            meta.modified
        ));
    }
    // `cp:lastModifiedBy` / `cp:revision` — the exact inverse of the importer's
    // core-part reads, so a round-trip keeps who touched the document last and
    // its revision counter instead of silently dropping them.
    if !meta.last_modified_by.is_empty() {
        let mut v = String::new();
        esc(&meta.last_modified_by, &mut v);
        body.push_str(&format!("<cp:lastModifiedBy>{v}</cp:lastModifiedBy>"));
    }
    if !meta.revision.is_empty() {
        let mut v = String::new();
        esc(&meta.revision, &mut v);
        body.push_str(&format!("<cp:revision>{v}</cp:revision>"));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<cp:coreProperties xmlns:cp=\"http://schemas.openxmlformats.org/package/2006/metadata/core-properties\" \
xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
xmlns:dcterms=\"http://purl.org/dc/terms/\" \
xmlns:dcmitype=\"http://purl.org/dc/dcmitype/\" \
xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">{body}</cp:coreProperties>"
    )
}

/// OOXML `docProps/app.xml` (extended properties, ECMA-376 Part 1 §15.2.12.2):
/// the generating application (from the source document when available), plus
/// the company when known.
fn ooxml_app_props(meta: &crate::model::DocMeta) -> String {
    let app = if !meta.application.is_empty() {
        let mut v = String::new();
        esc(&meta.application, &mut v);
        v
    } else {
        META_GENERATOR.to_string()
    };
    let company = if !meta.company.is_empty() {
        let mut v = String::new();
        esc(&meta.company, &mut v);
        format!("<Company>{v}</Company>")
    } else {
        String::new()
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Properties xmlns=\"http://schemas.openxmlformats.org/officeDocument/2006/extended-properties\" \
xmlns:vt=\"http://schemas.openxmlformats.org/officeDocument/2006/docPropsVTypes\">\
<Application>{app}</Application>{company}</Properties>"
    )
}

/// `[Content_Types].xml` Overrides for the two `docProps` parts (shared by every
/// OOXML package).
const OOXML_DOCPROPS_OVERRIDES: &str = "<Override PartName=\"/docProps/core.xml\" \
ContentType=\"application/vnd.openxmlformats-package.core-properties+xml\"/>\
<Override PartName=\"/docProps/app.xml\" \
ContentType=\"application/vnd.openxmlformats-officedocument.extended-properties+xml\"/>";

/// Package-root `_rels/.rels` Relationships for the two `docProps` parts (the
/// `officeDocument` relationship is supplied per format by the caller).
const OOXML_DOCPROPS_RELS: &str = "<Relationship Id=\"rIdCore\" \
Type=\"http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties\" \
Target=\"docProps/core.xml\"/>\
<Relationship Id=\"rIdApp\" \
Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties\" \
Target=\"docProps/app.xml\"/>";

/// `[Content_Types].xml` `<Default>` entries for the image extensions present in
/// a package — one per *distinct* extension (a duplicate `Default Extension` is
/// invalid OOXML, OPC §10.1.2.2.1). `exts` is the package-native extension of
/// every embedded image (e.g. `["png", "jpeg", "png"]`); the result declares
/// `png` and `jpeg` once each.
fn ooxml_image_defaults(exts: &[&str]) -> String {
    let mut seen: Vec<&str> = Vec::new();
    let mut out = String::new();
    for &ext in exts {
        if seen.contains(&ext) {
            continue;
        }
        seen.push(ext);
        // `ext` is one of office_image_format's fixed spellings ⇒ its content
        // type is the matching `image/<ext>` (with `svg` → `image/svg+xml`).
        let mime = office_image_format(ext).0;
        out.push_str(&format!(
            "<Default Extension=\"{ext}\" ContentType=\"{mime}\"/>"
        ));
    }
    out
}

/// ODF `meta.xml` (`office:document-meta` → `office:meta`, ISO 26300 §3/§4) from
/// the model metadata. Absent fields are omitted. Mirrors the OOXML core part
/// (`dc:title` / `dc:creator` / `dc:subject` / `meta:keyword` / `dc:language`)
/// plus the ODF-native fields the importer reads back: `dc:description`,
/// `meta:creation-date`, `dc:date` (modified), `meta:editing-cycles`. The
/// `meta:generator` **preserves the source document's generator** when the model
/// carries one (a LibreOffice-authored file keeps saying so across a
/// round-trip); only a document with no recorded generator is stamped GigaPDF.
fn odf_meta_xml(meta: &crate::model::DocMeta) -> String {
    let generator = if meta.generator.is_empty() {
        META_GENERATOR.to_string()
    } else {
        let mut v = String::new();
        esc(&meta.generator, &mut v);
        v
    };
    let mut body = format!("<meta:generator>{generator}</meta:generator>");
    if let Some(title) = meta.title.as_deref().filter(|s| !s.is_empty()) {
        let mut v = String::new();
        esc(title, &mut v);
        body.push_str(&format!("<dc:title>{v}</dc:title>"));
    }
    if let Some(subject) = meta.subject.as_deref().filter(|s| !s.is_empty()) {
        let mut v = String::new();
        esc(subject, &mut v);
        body.push_str(&format!("<dc:subject>{v}</dc:subject>"));
    }
    if let Some(author) = meta.author.as_deref().filter(|s| !s.is_empty()) {
        let mut v = String::new();
        esc(author, &mut v);
        body.push_str(&format!("<dc:creator>{v}</dc:creator>"));
    }
    if let Some(lang) = meta.lang.as_deref().filter(|s| !s.is_empty()) {
        let mut v = String::new();
        esc(lang, &mut v);
        body.push_str(&format!("<dc:language>{v}</dc:language>"));
    }
    for kw in &meta.keywords {
        if kw.is_empty() {
            continue;
        }
        let mut v = String::new();
        esc(kw, &mut v);
        body.push_str(&format!("<meta:keyword>{v}</meta:keyword>"));
    }
    // The remaining ODF meta fields, the exact inverse of the importer's
    // `odf_doc_meta`: description → `dc:description`, created →
    // `meta:creation-date`, modified → `dc:date`, editing_cycles →
    // `meta:editing-cycles`. All kept verbatim (dates are raw ISO-8601 text).
    if !meta.description.is_empty() {
        let mut v = String::new();
        esc(&meta.description, &mut v);
        body.push_str(&format!("<dc:description>{v}</dc:description>"));
    }
    if !meta.created.is_empty() {
        let mut v = String::new();
        esc(&meta.created, &mut v);
        body.push_str(&format!("<meta:creation-date>{v}</meta:creation-date>"));
    }
    if !meta.modified.is_empty() {
        let mut v = String::new();
        esc(&meta.modified, &mut v);
        body.push_str(&format!("<dc:date>{v}</dc:date>"));
    }
    if !meta.editing_cycles.is_empty() {
        let mut v = String::new();
        esc(&meta.editing_cycles, &mut v);
        body.push_str(&format!("<meta:editing-cycles>{v}</meta:editing-cycles>"));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<office:document-meta xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" \
xmlns:meta=\"urn:oasis:names:tc:opendocument:xmlns:meta:1.0\" \
xmlns:dc=\"http://purl.org/dc/elements/1.1/\" office:version=\"1.3\">\
<office:meta>{body}</office:meta></office:document-meta>"
    )
}

/// The `<style:default-style>` inner markup carrying the document default
/// language (ISO 26300 §16.2), *without* the enclosing `office:styles` block.
/// Splits a BCP-47 tag into `fo:language` + `fo:country` (`fr-FR` → `fr` / `FR`).
/// Empty when no language is set.
fn odf_default_lang_inner(meta: &crate::model::DocMeta) -> String {
    let Some(tag) = meta.lang.as_deref().filter(|s| !s.is_empty()) else {
        return String::new();
    };
    let mut parts = tag.split(['-', '_']);
    let lang = parts.next().unwrap_or("");
    let mut attrs = String::new();
    if !lang.is_empty() {
        let mut v = String::new();
        esc(lang, &mut v);
        attrs.push_str(&format!(" fo:language=\"{v}\""));
    }
    if let Some(country) = parts.next().filter(|s| !s.is_empty()) {
        let mut v = String::new();
        esc(country, &mut v);
        attrs.push_str(&format!(" fo:country=\"{v}\""));
    }
    if attrs.is_empty() {
        return String::new();
    }
    format!(
        "<style:default-style style:family=\"paragraph\">\
<style:text-properties{attrs}/></style:default-style>"
    )
}

/// ODF `<office:styles>` carrying the document default language (ISO 26300
/// §16.2), or empty when no language is set. Used by the ODS/ODP exporters,
/// which have no named-style table.
fn odf_default_lang_styles(meta: &crate::model::DocMeta) -> String {
    let inner = odf_default_lang_inner(meta);
    if inner.is_empty() {
        return String::new();
    }
    format!("<office:styles>{inner}</office:styles>")
}

/// ODF `<office:styles>` for a text document: the document's named paragraph
/// styles (ISO 26300 §16) followed by the optional default-language style. Each
/// named style is a `<style:style style:family="paragraph" style:name="…"
/// style:display-name="…">` carrying `style:parent-style-name` (from `based_on`)
/// and paragraph/text properties. A paragraph's `style_ref` references the
/// matching `style:name`. Empty (no block) only when there are no named styles
/// and no language.
fn odf_text_named_styles(styles: &StyleTable, meta: &crate::model::DocMeta) -> String {
    let mut inner = String::new();
    for (id, style) in &styles.named {
        if id.0.is_empty() {
            continue;
        }
        inner.push_str(&odf_named_para_style(id, style));
    }
    inner.push_str(&odf_default_lang_inner(meta));
    if inner.is_empty() {
        return String::new();
    }
    format!("<office:styles>{inner}</office:styles>")
}

/// One named `<style:style style:family="paragraph">` for a modeled
/// [`NamedStyle`]: `style:name` + `style:display-name` (both the style id),
/// `style:parent-style-name` from `based_on` (when set and not self), and the
/// paragraph/text properties. Only modeled deltas are emitted (no synthetic
/// default size) so an inherited style stays inherited.
fn odf_named_para_style(id: &StyleId, style: &NamedStyle) -> String {
    let mut name = String::new();
    esc(&id.0, &mut name);
    let mut out = format!(
        "<style:style style:name=\"{name}\" style:display-name=\"{name}\" style:family=\"paragraph\""
    );
    if let Some(parent) = style
        .based_on
        .as_ref()
        .filter(|p| !p.0.is_empty() && **p != *id)
    {
        let mut p = String::new();
        esc(&parent.0, &mut p);
        out.push_str(&format!(" style:parent-style-name=\"{p}\""));
    }
    out.push('>');
    let pattrs = odf_para_prop_attrs(&style.para);
    let ptabs = odf_tab_stops_xml(&style.para);
    if !pattrs.is_empty() || !ptabs.is_empty() {
        out.push_str(&odf_paragraph_properties_xml(&pattrs, &ptabs));
    }
    let tattrs = odf_named_text_attrs(&style.char_);
    if !tattrs.is_empty() {
        out.push_str(&format!("<style:text-properties{tattrs}/>"));
    }
    out.push_str("</style:style>");
    out
}

/// The `fo:`/`style:` attributes of a named style's `<style:text-properties>`,
/// emitting **only** modeled deltas (no synthesised default `fo:font-size`), so
/// a style that merely sets, e.g., bold does not pin the font size. Empty when
/// fully default.
fn odf_named_text_attrs(style: &CharStyle) -> String {
    let generic = match style.generic {
        crate::convert::style::Generic::Sans => "swiss",
        crate::convert::style::Generic::Serif => "roman",
        crate::convert::style::Generic::Mono => "modern",
    };
    let mut p = String::new();
    if style.size_pt > 0.0 {
        p.push_str(&format!(" fo:font-size=\"{}pt\"", num(style.size_pt)));
    }
    if !style.family.is_empty() {
        let mut fam = String::new();
        esc(&style.family, &mut fam);
        p.push_str(&format!(
            " fo:font-family=\"{fam}\" style:font-family-generic=\"{generic}\""
        ));
    }
    if style.bold {
        p.push_str(" fo:font-weight=\"bold\"");
    }
    if style.italic {
        p.push_str(" fo:font-style=\"italic\"");
    }
    if style.underline {
        p.push_str(" style:text-underline-style=\"solid\" style:text-underline-width=\"auto\"");
    }
    if style.strike {
        p.push_str(" style:text-line-through-style=\"solid\"");
    }
    // Run superscript/subscript → `style:text-position` (ODF §20.371), the ODT
    // analogue of DOCX `w:vertAlign` and PPTX `a:rPr@baseline`. The value is a
    // vertical position plus an optional font-size scale; `super`/`sub` raise or
    // lower by the default amount and `58%` shrinks the glyph, mirroring the ODF
    // importer's `super`/`sub` → `VAlign::Super`/`Sub` so it round-trips.
    match style.vertical_align {
        VAlign::Super => p.push_str(" style:text-position=\"super 58%\""),
        VAlign::Sub => p.push_str(" style:text-position=\"sub 58%\""),
        VAlign::Baseline => {}
    }
    if let Some(c) = visible_color(style) {
        p.push_str(&format!(" fo:color=\"#{c}\""));
    }
    if let Some(bg) = style.background {
        p.push_str(&format!(" fo:background-color=\"#{}\"", hex(bg)));
    }
    // Hidden text → `text:display="none"` (ODF §20.427), the ODT analogue of
    // DOCX `w:vanish`: the run survives in the file instead of losing its flag.
    if style.hidden {
        p.push_str(" text:display=\"none\"");
    }
    p.push_str(&odf_text_extras(style));
    p
}

// ════════════════════════════════════ DOCX ════════════════════════════════════

/// Serialize a [`Document`] to a **flowing** Word document (`.docx`).
///
/// Each [`Paragraph`]/[`Heading`] becomes a `<w:p>` (heading paragraphs carry a
/// `w:pStyle` of `Heading1`..`Heading6` and a `w:outlineLvl`); runs map to
/// `<w:r>` with `<w:rPr>` from the [`CharStyle`]; [`List`]s emit real
/// `w:numPr` numbering wired to a generated `word/numbering.xml`; [`Table`]s
/// become `<w:tbl>` with a `<w:tblGrid>` of column widths and `w:gridSpan` /
/// `w:vMerge` merges. The first section's optional header/footer become
/// `header1.xml`/`footer1.xml`.
pub fn docx_from_model(doc: &Document) -> Vec<u8> {
    let mut zip = ZipWriter::new();
    let mut ctx = DocxCtx::new(&doc.resources, &doc.comments, &doc.footnotes);

    // Per-section header/footer parts. Each [`Section`] with a header/footer gets
    // its own `headerN.xml`/`footerN.xml` (1-based by section index) referenced
    // from that section's `w:sectPr`. Rendered up front so any lists *inside* a
    // header/footer register their markers before `numbering.xml` is generated
    // (their `w:numId` must resolve), matching the body render order. Rendered
    // with `in_hdrftr` set (run hyperlinks keep the relationship-free field
    // encoding there) and the owning section's page height for anchored frames.
    ctx.in_hdrftr = true;
    let sect_hf: Vec<SectionHf> = doc
        .sections
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let n = i + 1;
            ctx.page_h = s.geometry.height;
            SectionHf {
                header: s.header.as_ref().map(|h| docx_hdrftr_blocks(h, &mut ctx)),
                footer: s.footer.as_ref().map(|f| docx_hdrftr_blocks(f, &mut ctx)),
                // First-/even-page variants (`w:titlePg` / `w:evenAndOddHeaders`).
                // A blank variant (`Some(vec![])`) renders to an empty inner and
                // becomes an empty part — Word's "different first page" with a
                // blank header.
                header_first: s
                    .header_first
                    .as_ref()
                    .map(|h| docx_hdrftr_blocks(h, &mut ctx)),
                footer_first: s
                    .footer_first
                    .as_ref()
                    .map(|f| docx_hdrftr_blocks(f, &mut ctx)),
                header_even: s
                    .header_even
                    .as_ref()
                    .map(|h| docx_hdrftr_blocks(h, &mut ctx)),
                footer_even: s
                    .footer_even
                    .as_ref()
                    .map(|f| docx_hdrftr_blocks(f, &mut ctx)),
                number: n,
            }
        })
        .collect();
    ctx.in_hdrftr = false;

    let body = docx_body(doc, &sect_hf, &mut ctx);

    // Indices (matching `sect_hf`) that actually own a header / footer part.
    let header_nums: Vec<usize> = sect_hf
        .iter()
        .filter(|h| h.header.is_some())
        .map(|h| h.number)
        .collect();
    let footer_nums: Vec<usize> = sect_hf
        .iter()
        .filter(|h| h.footer.is_some())
        .map(|h| h.number)
        .collect();
    // First-/even-page variant parts: `(part file, relationship id, is_header)`
    // for the content-types + rels writers (part names `headerF{n}` /
    // `headerE{n}` / `footerF{n}` / `footerE{n}`, rel ids `rIdHdrF{n}` / …).
    let mut extra_hf: Vec<(String, String, bool)> = Vec::new();
    for hf in &sect_hf {
        let n = hf.number;
        if hf.header_first.is_some() {
            extra_hf.push((format!("headerF{n}.xml"), format!("rIdHdrF{n}"), true));
        }
        if hf.footer_first.is_some() {
            extra_hf.push((format!("footerF{n}.xml"), format!("rIdFtrF{n}"), false));
        }
        if hf.header_even.is_some() {
            extra_hf.push((format!("headerE{n}.xml"), format!("rIdHdrE{n}"), true));
        }
        if hf.footer_even.is_some() {
            extra_hf.push((format!("footerE{n}.xml"), format!("rIdFtrE{n}"), false));
        }
    }
    // `w:evenAndOddHeaders` is a document-wide setting: emit `settings.xml`
    // when any section carries an even-page variant.
    let has_settings = sect_hf
        .iter()
        .any(|h| h.header_even.is_some() || h.footer_even.is_some());
    // Footnote bodies (`word/footnotes.xml`), rendered before the numbering /
    // content-type decisions so any list inside a note registers its marker
    // and the part's presence is known. Only written when at least one anchor
    // was emitted (a `w:footnoteReference` must resolve both ways).
    let has_footnotes = ctx.used_footnotes;
    let footnotes_xml = has_footnotes.then(|| docx_footnotes_xml(&mut ctx));
    let has_num = !ctx.list_markers.is_empty();
    let has_comments = ctx.used_comments;

    let image_exts: Vec<&str> = ctx.images.iter().map(|(_, ext)| *ext).collect();
    zip.add_deflated(
        "[Content_Types].xml",
        docx_content_types(
            &image_exts,
            has_num,
            has_comments,
            &header_nums,
            &footer_nums,
            &extra_hf,
            has_settings,
            has_footnotes,
        )
        .as_bytes(),
    );
    zip.add_deflated(
        "_rels/.rels",
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" \
Target=\"word/document.xml\"/>{OOXML_DOCPROPS_RELS}</Relationships>"
        )
        .as_bytes(),
    );
    zip.add_deflated("docProps/core.xml", ooxml_core_props(&doc.meta).as_bytes());
    zip.add_deflated("docProps/app.xml", ooxml_app_props(&doc.meta).as_bytes());
    zip.add_deflated("word/document.xml", body.as_bytes());
    zip.add_deflated(
        "word/_rels/document.xml.rels",
        docx_rels(
            &image_exts,
            has_num,
            has_comments,
            &ctx.hyperlinks,
            &header_nums,
            &footer_nums,
            &extra_hf,
            has_settings,
            has_footnotes,
        )
        .as_bytes(),
    );
    if let Some(xml) = &footnotes_xml {
        zip.add_deflated("word/footnotes.xml", xml.as_bytes());
    }
    if has_settings {
        zip.add_deflated(
            "word/settings.xml",
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<w:settings xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
<w:evenAndOddHeaders/></w:settings>"
                .as_bytes(),
        );
    }
    zip.add_deflated(
        "word/styles.xml",
        docx_styles_xml(doc.meta.lang.as_deref(), &doc.styles).as_bytes(),
    );
    if has_num {
        zip.add_deflated(
            "word/numbering.xml",
            docx_numbering_xml(&ctx.list_markers).as_bytes(),
        );
    }
    // Review comments: their bodies (`word/comments.xml`), written only when at
    // least one anchor was emitted (a `w:commentReference` must resolve; an
    // anchor-less comment list would be invisible anyway).
    if has_comments {
        zip.add_deflated(
            "word/comments.xml",
            docx_comments_xml(ctx.comments, &ctx.comment_nums).as_bytes(),
        );
    }
    for hf in &sect_hf {
        if let Some(inner) = &hf.header {
            zip.add_deflated(
                &format!("word/header{}.xml", hf.number),
                docx_hdrftr_xml("hdr", inner).as_bytes(),
            );
        }
        if let Some(inner) = &hf.footer {
            zip.add_deflated(
                &format!("word/footer{}.xml", hf.number),
                docx_hdrftr_xml("ftr", inner).as_bytes(),
            );
        }
        // Variant parts. A blank variant still writes a valid part with one
        // empty paragraph (a header/footer part must hold ≥ 1 block).
        let variants = [
            (&hf.header_first, "hdr", format!("word/headerF{}.xml", hf.number)),
            (&hf.footer_first, "ftr", format!("word/footerF{}.xml", hf.number)),
            (&hf.header_even, "hdr", format!("word/headerE{}.xml", hf.number)),
            (&hf.footer_even, "ftr", format!("word/footerE{}.xml", hf.number)),
        ];
        for (inner, tag, name) in variants {
            if let Some(inner) = inner {
                let body = if inner.is_empty() { "<w:p/>" } else { inner };
                zip.add_deflated(&name, docx_hdrftr_xml(tag, body).as_bytes());
            }
        }
    }
    for (i, (bytes, ext)) in ctx.images.iter().enumerate() {
        zip.add_deflated(&format!("word/media/image{}.{ext}", i + 1), bytes);
    }
    zip.finish()
}

/// One list instance's numbering definition, captured by [`docx_list`] and
/// consumed by [`docx_numbering_xml`]: the table-wide marker/suffix/start (the
/// fallback for every depth), the ordinal `prefix` (`"("` for `(a)`-style
/// templates), and the list's per-depth [`ListLevelStyle`]s (empty ⇒ the
/// table-wide triple applies to all nine levels, the pre-M2 behaviour).
struct DocxNumDef {
    marker: ListMarker,
    suffix: char,
    start: u32,
    prefix: Option<String>,
    levels: Vec<crate::model::ListLevelStyle>,
}

/// A section's rendered running header/footer inner XML (`None` when the section
/// carries none), plus its 1-based `number` driving the `header{n}.xml` /
/// `footer{n}.xml` part name and the `rIdHdr{n}` / `rIdFtr{n}` relationship id.
struct SectionHf {
    header: Option<String>,
    footer: Option<String>,
    /// Rendered first-page header/footer (`w:titlePg`); `Some("")` = a blank
    /// variant part.
    header_first: Option<String>,
    footer_first: Option<String>,
    /// Rendered even-page header/footer (`w:evenAndOddHeaders`).
    header_even: Option<String>,
    footer_even: Option<String>,
    number: usize,
}

/// Mutable state threaded through a DOCX build: a flat image list (global order →
/// `media/imageN.<ext>` + `rId`), a running list-instance counter (each [`List`]
/// gets a distinct `w:numId`), and the document's resource table for image blobs.
struct DocxCtx<'a> {
    /// Each image: raw bytes plus its package-native extension (e.g. `"png"`,
    /// `"jpeg"`), so the media part, relationship target and `[Content_Types]`
    /// `Default` all carry the resource's real format.
    images: Vec<(Vec<u8>, &'static str)>,
    /// One definition per list *instance* emitted; its position + 1 is the
    /// list's `w:numId`, and the definition drives the generated
    /// `numbering.xml` (`w:numFmt`, the `w:lvlText` template's prefix/suffix,
    /// the level-0 `w:start`, and — when the list carries them — real
    /// per-depth level styles).
    list_markers: Vec<DocxNumDef>,
    /// Next unique drawing/object id.
    obj_id: usize,
    resources: &'a crate::model::ResourceTable,
    /// The document's review comments; `comment_nums[i]` is comment `i`'s
    /// numeric DOCX `w:id` (`w:commentReference` requires `ST_DecimalNumber`,
    /// but a model id is free text — ODT `office:name`s are). An all-digit
    /// unique model id keeps its own number so a DOCX-sourced comment
    /// round-trips its id; anything else gets a fresh number above the max.
    comments: &'a [crate::model::Comment],
    comment_nums: Vec<u64>,
    /// Whether any comment anchor was actually emitted (drives the
    /// `comments.xml` part + its relationship/content-type).
    used_comments: bool,
    /// The document's foot-/endnotes; `footnote_nums[i]` is note `i`'s numeric
    /// DOCX `w:id` (same stable-id scheme as the comments, floored at 1 since
    /// ids −1/0 belong to the stock separator notes).
    footnotes: &'a [crate::model::Footnote],
    footnote_nums: Vec<u64>,
    /// Whether any footnote anchor was actually emitted (drives the
    /// `footnotes.xml` part + its relationship/content-type).
    used_footnotes: bool,
    /// External hyperlink targets, in allocation order; entry `i` is relationship
    /// `rIdHl{i+1}` in `document.xml.rels` (`TargetMode="External"`).
    hyperlinks: Vec<String>,
    /// Rendering into a header/footer part: those parts have no `.rels` of their
    /// own here, so run hyperlinks keep the field encoding (`HYPERLINK "url"`)
    /// instead of a `w:hyperlink r:id` that would dangle, and comment anchors are
    /// skipped (`comments.xml` is a document-part relationship).
    in_hdrftr: bool,
    /// Page height (points) of the section currently being rendered — the flip
    /// axis turning the model's lower-left frames back into the top-down
    /// `wp:posOffset` the importer reads (`docx_anchor_frame`'s exact inverse).
    page_h: f64,
}

/// Stable numeric ids for a free-text id list: keep unique all-digit model
/// ids at or above `floor`, then number the rest above the maximum so no two
/// entries collide. Shared by the comment (`floor = 0`) and footnote
/// (`floor = 1` — `w:id` 0/-1 are the stock separator notes) numbering.
fn docx_stable_nums<'i>(ids: impl Iterator<Item = &'i str>, floor: u64) -> Vec<u64> {
    let mut nums: Vec<Option<u64>> = Vec::new();
    let mut used: Vec<u64> = Vec::new();
    for id in ids {
        let parsed = id
            .parse::<u64>()
            .ok()
            .filter(|n| *n >= floor && !used.contains(n));
        if let Some(n) = parsed {
            used.push(n);
        }
        nums.push(parsed);
    }
    let mut next = used.iter().max().map(|m| m + 1).unwrap_or(floor);
    nums.into_iter()
        .map(|n| {
            n.unwrap_or_else(|| {
                let v = next;
                next += 1;
                v
            })
        })
        .collect()
}

impl<'a> DocxCtx<'a> {
    fn new(
        resources: &'a crate::model::ResourceTable,
        comments: &'a [crate::model::Comment],
        footnotes: &'a [crate::model::Footnote],
    ) -> Self {
        // Stable numeric ids: keep unique all-digit model ids, then number the
        // rest above the maximum so no two comments/footnotes collide.
        let comment_nums = docx_stable_nums(comments.iter().map(|c| c.id.as_str()), 0);
        let footnote_nums = docx_stable_nums(footnotes.iter().map(|f| f.id.as_str()), 1);
        DocxCtx {
            images: Vec::new(),
            list_markers: Vec::new(),
            obj_id: 0,
            resources,
            comments,
            comment_nums,
            used_comments: false,
            footnotes,
            footnote_nums,
            used_footnotes: false,
            hyperlinks: Vec::new(),
            in_hdrftr: false,
            page_h: PageGeometry::default().height,
        }
    }
    fn next_obj(&mut self) -> usize {
        self.obj_id += 1;
        self.obj_id
    }
    /// The numeric `w:id` for the comment anchored by `id`, or `None` when no
    /// such comment exists (a dangling anchor emits nothing).
    fn comment_num(&self, id: &str) -> Option<u64> {
        self.comments
            .iter()
            .position(|c| c.id == id)
            .map(|i| self.comment_nums[i])
    }
    /// The numeric `w:id` for the footnote anchored by `id`, or `None` when no
    /// such note exists (a dangling anchor emits nothing).
    fn footnote_num(&self, id: &str) -> Option<u64> {
        self.footnotes
            .iter()
            .position(|f| f.id == id)
            .map(|i| self.footnote_nums[i])
    }
    /// Register an external hyperlink target, returning its `rIdHl{N}` id.
    fn add_hyperlink(&mut self, url: &str) -> usize {
        self.hyperlinks.push(url.to_string());
        self.hyperlinks.len()
    }
    /// Register image bytes (+ extension), returning the relationship id
    /// (`rId{N}`, 100-based).
    fn add_image(&mut self, bytes: Vec<u8>, ext: &'static str) -> usize {
        self.images.push((bytes, ext));
        100 + self.images.len() - 1
    }
    /// Resolve an image blob by resource key, returning its bytes and the
    /// package-native extension derived from the resource's format tag.
    fn resolve_image(&self, key: u64) -> Option<(Vec<u8>, &'static str)> {
        self.resources
            .images
            .get(&key)
            .map(|r| (r.bytes.clone(), office_image_format(&r.format).1))
    }
}

fn docx_body(doc: &Document, sect_hf: &[SectionHf], ctx: &mut DocxCtx) -> String {
    let mut blocks = String::new();
    // A `w:bookmarkStart`/`End` pair named `page{N}` at each page boundary, the
    // jump target for an internal `LinkTarget::Page` (`HYPERLINK \l "page{N}"`).
    // `w:bookmark*` are run-level elements valid as direct `w:body` children, so
    // no extra paragraph is introduced. The `w:id` is the 1-based page number, so
    // each id is unique across the document.
    //
    // Per-section page setup (#2): each section carries its own geometry +
    // running header/footer. WordprocessingML attaches a non-final section's
    // `w:sectPr` to the `w:pPr` of the *last paragraph of that section*, and the
    // *final* section's `w:sectPr` is a direct `w:body` child. So every section
    // but the last is terminated by an (otherwise empty) section-break paragraph
    // carrying its `w:sectPr`; the last section's `w:sectPr` is appended after
    // the body content.
    let mut page_no = 0usize;
    let last_idx = doc.sections.len().saturating_sub(1);
    // Outline destinations (#8): every outline entry becomes a `_Toc…` bookmark
    // wrapped around its matching heading, so Word has real TOC/cross-reference
    // destinations. `_Toc` is the canonical Word TOC-anchor namespace **and** a
    // name the importer deliberately treats as machinery (dropped on re-import),
    // so the bookmarks add zero round-trip noise. Ids continue after the page
    // bookmarks' (1-based page numbers) to stay document-unique.
    let total_pages: usize = doc.sections.iter().map(|s| s.pages.len()).sum();
    let mut outline_marks = outline_bookmark_marks(&doc.outline, total_pages);
    for (si, section) in doc.sections.iter().enumerate() {
        ctx.page_h = section.geometry.height;
        // A trailing *empty* paragraph on a non-final section hosts that
        // section's `w:sectPr` directly (the shape Word itself writes) instead
        // of an extra synthesized break paragraph — otherwise each round-trip
        // grows the document by one empty paragraph per section boundary
        // (import keeps the sectPr host as an empty paragraph, export then
        // added a fresh one).
        let merge_sect_para: Option<&Paragraph> = if si != last_idx {
            section
                .pages
                .last()
                .and_then(|p| p.blocks.last())
                .and_then(|b| match &b.kind {
                    BlockKind::Paragraph(p) if para_has_no_content(p) => Some(p),
                    _ => None,
                })
        } else {
            None
        };
        let page_count = section.pages.len();
        // Whether the previous page's trailing empty paragraph already carries
        // the hard page break (see `PageTail::MergePageBreak`).
        let mut break_merged = false;
        for (pi, page) in section.pages.iter().enumerate() {
            page_no += 1;
            // Insert a hard page break before each page boundary (except the very
            // first page of a section) so content from adjacent PDF pages does
            // not flow together as one continuous stream. This mirrors how the
            // user expects a multi-page PDF to map to a multi-page Word document.
            // When the previous page ended in an empty paragraph, the break rode
            // *that* paragraph — Word's own shape for an inserted page break —
            // so no extra paragraph is synthesized (the importer keeps a
            // break-only paragraph as an empty paragraph: a synthesized one grew
            // the document by one empty paragraph per page boundary per
            // round-trip).
            if pi > 0 && !break_merged {
                blocks.push_str("<w:p><w:r><w:br w:type=\"page\"/></w:r></w:p>");
            }
            blocks.push_str(&format!(
                "<w:bookmarkStart w:id=\"{page_no}\" w:name=\"page{page_no}\"/>\
<w:bookmarkEnd w:id=\"{page_no}\"/>"
            ));
            // Any trailing plain paragraph can host the break (Word's shape:
            // the break run ends the page's last paragraph); a page ending in
            // a table/list/image keeps the standalone break paragraph.
            let last_is_para = page
                .blocks
                .last()
                .is_some_and(|b| matches!(&b.kind, BlockKind::Paragraph(_)));
            let tail = if merge_sect_para.is_some() && pi + 1 == page_count {
                PageTail::SkipForSectPr
            } else if pi + 1 < page_count && last_is_para {
                PageTail::MergePageBreak
            } else {
                PageTail::None
            };
            break_merged = matches!(tail, PageTail::MergePageBreak);
            blocks.push_str(&docx_page_blocks(
                &page.blocks,
                page_no - 1,
                tail,
                &mut outline_marks,
                ctx,
            ));
        }
        if si != last_idx {
            // Section break: the section's `w:sectPr` rides the last (empty)
            // paragraph when one exists — keeping that paragraph's own
            // formatting — else an otherwise-empty break paragraph.
            let props = merge_sect_para
                .map(|p| {
                    let mut ppr = String::new();
                    if let Some(id) = style_ref_id(p.style_ref.as_ref()) {
                        let mut v = String::new();
                        esc(id, &mut v);
                        ppr.push_str(&format!("<w:pStyle w:val=\"{v}\"/>"));
                    }
                    ppr.push_str(&docx_style_ppr_props(&p.style));
                    ppr
                })
                .unwrap_or_default();
            blocks.push_str(&format!(
                "<w:p><w:pPr>{props}{}</w:pPr></w:p>",
                docx_sect_pr(section, sect_hf.get(si))
            ));
        }
    }
    // The final section's `w:sectPr` is a direct `w:body` child. When the document
    // has no sections at all, fall back to the default geometry (no header/footer).
    let final_sect = match doc.sections.last() {
        Some(section) => docx_sect_pr(section, sect_hf.last()),
        None => docx_sect_pr(&Section::default(), None),
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\" \
xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
xmlns:wp=\"http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing\" \
xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" \
xmlns:v=\"urn:schemas-microsoft-com:vml\" \
xmlns:o=\"urn:schemas-microsoft-com:office:office\" \
xmlns:w10=\"urn:schemas-microsoft-com:office:word\">\
<w:body>{blocks}{final_sect}</w:body></w:document>"
    )
}

/// One `<w:sectPr>` for a [`Section`]: its page size (`w:pgSz` with orientation),
/// margins (`w:pgMar`), and — when the matching [`SectionHf`] carries them — its
/// own header/footer references (`rIdHdr{n}` / `rIdFtr{n}`). `hf` is `None` only
/// for the synthetic empty-document fallback (no header/footer then).
fn docx_sect_pr(section: &Section, hf: Option<&SectionHf>) -> String {
    let geom = &section.geometry;
    let mut header_ref = String::new();
    let mut footer_ref = String::new();
    // `<w:titlePg/>` when the section declares a distinct first page (CT_SectPr
    // places it after `w:cols`; Word is lenient but keep it post-pgMar).
    let mut title_pg = "";
    if let Some(h) = hf {
        let n = h.number;
        if h.header.is_some() {
            header_ref.push_str(&format!(
                "<w:headerReference w:type=\"default\" r:id=\"rIdHdr{n}\"/>"
            ));
        }
        if h.header_first.is_some() {
            header_ref.push_str(&format!(
                "<w:headerReference w:type=\"first\" r:id=\"rIdHdrF{n}\"/>"
            ));
        }
        if h.header_even.is_some() {
            header_ref.push_str(&format!(
                "<w:headerReference w:type=\"even\" r:id=\"rIdHdrE{n}\"/>"
            ));
        }
        if h.footer.is_some() {
            footer_ref.push_str(&format!(
                "<w:footerReference w:type=\"default\" r:id=\"rIdFtr{n}\"/>"
            ));
        }
        if h.footer_first.is_some() {
            footer_ref.push_str(&format!(
                "<w:footerReference w:type=\"first\" r:id=\"rIdFtrF{n}\"/>"
            ));
        }
        if h.footer_even.is_some() {
            footer_ref.push_str(&format!(
                "<w:footerReference w:type=\"even\" r:id=\"rIdFtrE{n}\"/>"
            ));
        }
        if h.header_first.is_some() || h.footer_first.is_some() {
            title_pg = "<w:titlePg/>";
        }
    }
    // Multi-column layout: emit `<w:cols>` when the source page has more than
    // one text column. The model's `column_gap_pt` (when imported) restores the
    // authored gutter; absent, 720 twips (0.5″) approximates the standard
    // gutter between columns when the source PDF's gutter can't be measured.
    // Individual `column_widths_pt` re-emit as `<w:col w:w>` children with
    // `w:equalWidth="0"` (a non-uniform layout), and `column_separator` as
    // `w:sep="1"` — the symmetric inverse of the importer's `w:cols` read.
    let cols = if geom.column_count > 1 {
        let space = twips(geom.column_gap_pt.unwrap_or(36.0).max(0.0));
        let sep = if geom.column_separator {
            " w:sep=\"1\""
        } else {
            ""
        };
        match geom.column_widths_pt.as_ref().filter(|w| !w.is_empty()) {
            Some(widths) => {
                let mut inner = String::new();
                let last = widths.len().saturating_sub(1);
                for (i, w) in widths.iter().enumerate() {
                    // Word writes the gutter on every column but the last.
                    if i == last {
                        inner.push_str(&format!("<w:col w:w=\"{}\"/>", twips(w.max(0.0))));
                    } else {
                        inner.push_str(&format!(
                            "<w:col w:w=\"{}\" w:space=\"{space}\"/>",
                            twips(w.max(0.0))
                        ));
                    }
                }
                format!(
                    "<w:cols w:num=\"{}\" w:space=\"{space}\"{sep} w:equalWidth=\"0\">{inner}</w:cols>",
                    geom.column_count
                )
            }
            None => format!(
                "<w:cols w:num=\"{}\" w:space=\"{space}\"{sep}/>",
                geom.column_count
            ),
        }
    } else {
        String::new()
    };
    format!(
        "<w:sectPr>{header_ref}{footer_ref}{cols}<w:pgSz w:w=\"{w}\" w:h=\"{h}\" w:orient=\"{o}\"/>\
<w:pgMar w:top=\"{mt}\" w:right=\"{mr}\" w:bottom=\"{mb}\" w:left=\"{ml}\" w:header=\"{mt}\" w:footer=\"{mb}\" w:gutter=\"0\"/>{title_pg}</w:sectPr>",
        w = twips(geom.width),
        h = twips(geom.height),
        o = if geom.height >= geom.width { "portrait" } else { "landscape" },
        mt = twips(geom.margins.top),
        mr = twips(geom.margins.right),
        mb = twips(geom.margins.bottom),
        ml = twips(geom.margins.left),
    )
}

/// Serialize a block list to DOCX body XML.
fn docx_blocks(blocks: &[Block], ctx: &mut DocxCtx) -> String {
    let mut out = String::new();
    for b in blocks {
        docx_block(b, ctx, &mut out);
    }
    out
}

/// A pending outline destination (#8): the outline entry's 0-based `page` and
/// trimmed `title`, the `_Toc…` bookmark `name`/`id` to drop on the matching
/// heading, and whether it was already placed.
struct OutlineMark {
    page: usize,
    title: String,
    name: String,
    id: usize,
    used: bool,
}

/// Flatten the outline tree into pending bookmark marks. Names are Word's
/// canonical `_Toc` + 9-digit form; ids continue after the `total_pages` page
/// bookmarks so every `w:bookmarkStart w:id` stays document-unique.
fn outline_bookmark_marks(
    outline: &[crate::model::OutlineNode],
    total_pages: usize,
) -> Vec<OutlineMark> {
    fn flatten(nodes: &[crate::model::OutlineNode], out: &mut Vec<(usize, String)>) {
        for n in nodes {
            let title = n.title.trim().to_string();
            if !title.is_empty() {
                out.push((n.page, title));
            }
            flatten(&n.children, out);
        }
    }
    let mut flat = Vec::new();
    flatten(outline, &mut flat);
    flat.into_iter()
        .enumerate()
        .map(|(i, (page, title))| OutlineMark {
            page,
            title,
            name: format!("_Toc{:09}", 100_000_001 + i),
            id: total_pages + i + 1,
            used: false,
        })
        .collect()
}

/// Whether a paragraph carries no content at all (no text, no image/break/link/
/// comment inline) — the shape of a section-break host paragraph.
fn para_has_no_content(p: &Paragraph) -> bool {
    p.runs
        .iter()
        .all(|r| matches!(r, Inline::Run(ir) if ir.text.is_empty()))
}

/// The concatenated plain text of a heading (for outline-title matching).
fn heading_plain_text(h: &Heading) -> String {
    let mut out = String::new();
    inlines_plain_text(&h.para.runs, &mut out);
    out
}

/// How a page's trailing block is folded into the surrounding structure.
#[derive(PartialEq)]
enum PageTail {
    /// Nothing special: every block renders normally.
    None,
    /// The trailing empty paragraph is *not* emitted here — `docx_body`
    /// re-emits it carrying the section's `w:sectPr`.
    SkipForSectPr,
    /// The trailing empty paragraph carries the hard page break to the next
    /// page (`<w:br w:type="page"/>` as its run) instead of a synthesized
    /// break-only paragraph following it.
    MergePageBreak,
}

/// Serialize one page's top-level blocks, applying the page-scoped fixups the
/// plain [`docx_blocks`] must not (lists/cells reuse that one):
///
/// * **Outline bookmarks** (#8): a heading matching a pending [`OutlineMark`]
///   (same 0-based page, same trimmed title) is wrapped in the mark's
///   `w:bookmarkStart`/`End` pair.
/// * **Anchored-image absorption**: a positioned [`BlockKind::Image`] directly
///   following a paragraph rides *inside* that paragraph's `<w:p>` (as the
///   trailing `wp:anchor` run) — the exact inverse of the importer, which lifts
///   a floating drawing out of its host paragraph. A standalone wrapper `<w:p>`
///   would re-import as one extra empty paragraph per image per round-trip.
/// * **Tail folding** ([`PageTail`]): the trailing empty paragraph hosts the
///   section break or the hard page break instead of a synthesized paragraph.
fn docx_page_blocks(
    blocks: &[Block],
    page_idx: usize,
    tail: PageTail,
    marks: &mut [OutlineMark],
    ctx: &mut DocxCtx,
) -> String {
    let mut out = String::new();
    let end = blocks.len() - usize::from(tail == PageTail::SkipForSectPr);
    let mut i = 0usize;
    while i < end {
        let b = &blocks[i];
        // Outline destination on a matching heading.
        let mark: Option<(usize, String)> = match &b.kind {
            BlockKind::Heading(h) => {
                let text = heading_plain_text(h);
                let text = text.trim();
                marks
                    .iter_mut()
                    .find(|m| !m.used && m.page == page_idx && m.title == text)
                    .map(|m| {
                        m.used = true;
                        (m.id, m.name.clone())
                    })
            }
            _ => None,
        };
        if let Some((id, name)) = &mark {
            out.push_str(&format!(
                "<w:bookmarkStart w:id=\"{id}\" w:name=\"{name}\"/>"
            ));
        }
        match &b.kind {
            BlockKind::Paragraph(p) => {
                // Absorb the run of anchored images that immediately follows.
                let mut extra = String::new();
                let mut consumed = 0usize;
                while i + 1 + consumed < end {
                    match &blocks[i + 1 + consumed].kind {
                        BlockKind::Image(img) if blocks[i + 1 + consumed].frame.is_some() => {
                            extra.push_str(&docx_anchored_image(
                                img,
                                blocks[i + 1 + consumed].frame.unwrap_or_default(),
                                ctx,
                            ));
                            consumed += 1;
                        }
                        _ => break,
                    }
                }
                // The page's trailing paragraph carries the hard break to the
                // next page (see `PageTail::MergePageBreak`). The importer
                // reads `<w:br w:type="page"/>` as a **LineBreak run** plus
                // the page split, so the break *replaces* the paragraph's
                // trailing line break when it has one — export∘import is then
                // the identity on the runs instead of growing one line break
                // per round-trip cycle.
                if tail == PageTail::MergePageBreak && i + consumed + 1 == end {
                    extra.push_str("<w:r><w:br w:type=\"page\"/></w:r>");
                    let mut host = p.clone();
                    if matches!(host.runs.last(), Some(Inline::LineBreak)) {
                        host.runs.pop();
                    }
                    out.push_str(&docx_para_extra(
                        &host,
                        style_ref_id(host.style_ref.as_ref()),
                        0,
                        0,
                        &extra,
                        ctx,
                    ));
                } else {
                    out.push_str(&docx_para_extra(
                        p,
                        style_ref_id(p.style_ref.as_ref()),
                        0,
                        0,
                        &extra,
                        ctx,
                    ));
                }
                i += consumed;
            }
            _ => docx_block(b, ctx, &mut out),
        }
        if let Some((id, _)) = &mark {
            out.push_str(&format!("<w:bookmarkEnd w:id=\"{id}\"/>"));
        }
        i += 1;
    }
    out
}

fn docx_block(block: &Block, ctx: &mut DocxCtx, out: &mut String) {
    match &block.kind {
        BlockKind::Paragraph(p) => {
            out.push_str(&docx_para(p, style_ref_id(p.style_ref.as_ref()), 0, ctx))
        }
        BlockKind::Heading(h) => out.push_str(&docx_heading(h, ctx)),
        BlockKind::List(list) => docx_list(list, ctx, out),
        BlockKind::Table(table) => out.push_str(&docx_table(table, ctx)),
        BlockKind::Image(img) => out.push_str(&docx_image_para(img, block.frame, ctx)),
        BlockKind::Shape(shape) => out.push_str(&docx_shape_para(shape, ctx)),
        BlockKind::TextBox(tb) => docx_textbox(tb, ctx, out),
        BlockKind::CodeBlock(cb) => out.push_str(&docx_code(cb)),
        BlockKind::Blockquote(bq) => docx_blockquote(bq, ctx, out),
        BlockKind::HorizontalRule => out.push_str(docx_hr()),
        BlockKind::Sheet(sheet) => docx_sheet(sheet, ctx, out),
        BlockKind::Slide(slides) => docx_slides(slides, ctx, out),
    }
}

/// A code block → one shaded, monospaced `<w:p>` whose source lines are joined by
/// `<w:br/>` so the layout stays verbatim (Word renders the run preformatted).
fn docx_code(cb: &CodeBlock) -> String {
    let mut runs = String::new();
    for (i, line) in cb.code.split('\n').enumerate() {
        if i > 0 {
            runs.push_str("<w:r><w:br/></w:r>");
        }
        if !line.is_empty() {
            let mut t = String::new();
            esc(line, &mut t);
            runs.push_str(&format!(
                "<w:r><w:rPr><w:rFonts w:ascii=\"Courier New\" w:hAnsi=\"Courier New\" \
w:cs=\"Courier New\"/><w:sz w:val=\"20\"/></w:rPr><w:t xml:space=\"preserve\">{t}</w:t></w:r>"
            ));
        }
    }
    // A light grey shading + a thin box border sets the block off as code.
    format!(
        "<w:p><w:pPr><w:shd w:val=\"clear\" w:color=\"auto\" w:fill=\"F2F2F2\"/>\
<w:spacing w:after=\"0\"/></w:pPr>{runs}</w:p>"
    )
}

/// A block quote → its inner blocks rendered with extra left indent, so the
/// quotation reads as set off from the body. The indent is applied at the model
/// level (cloning the blocks and shifting `indent_left_pt`) rather than by XML
/// surgery, so nested structure (lists/tables/quotes) is preserved.
fn docx_blockquote(bq: &Blockquote, ctx: &mut DocxCtx, out: &mut String) {
    let indented: Vec<Block> = bq
        .blocks
        .iter()
        .map(|b| indent_block_left(b, 24.0))
        .collect();
    out.push_str(&docx_blocks(&indented, ctx));
}

/// A horizontal rule → an empty paragraph carrying only a bottom border.
fn docx_hr() -> &'static str {
    "<w:p><w:pPr><w:pBdr><w:bottom w:val=\"single\" w:sz=\"6\" w:space=\"1\" \
w:color=\"999999\"/></w:pBdr></w:pPr></w:p>"
}

/// A paragraph's named-style reference as a borrowed style id, or `None` when
/// the paragraph carries no (non-empty) `style_ref`. The id flows verbatim into
/// a DOCX `w:pStyle w:val` / ODF `text:style-name`, matching the `w:styleId` /
/// `style:name` emitted into the styles part.
fn style_ref_id(style_ref: Option<&StyleId>) -> Option<&str> {
    style_ref.map(|id| id.0.as_str()).filter(|s| !s.is_empty())
}

/// Re-flow a paragraph's **soft** line breaks for reflowable exports: a break
/// classified soft (`ParagraphStyle::soft_breaks[k] == true` — a word-wrap
/// consequence of the source layout, not an authored break) becomes a plain
/// space, so Word/Markdown re-flow the text natively instead of freezing the
/// PDF's line grid. Guards (statu quo otherwise): `soft_breaks` must be
/// non-empty, aligned with the paragraph's `LineBreak` count (links included,
/// in flow order), and classify at least one break as soft. Hard breaks keep
/// their `LineBreak`. The space appends onto the preceding run when possible
/// so it inherits that run's style. Returns `None` when nothing changes.
fn reflow_soft_breaks(para: &Paragraph) -> Option<Vec<Inline>> {
    fn count_breaks(runs: &[Inline]) -> usize {
        runs.iter()
            .map(|r| match r {
                Inline::LineBreak => 1,
                Inline::Link { children, .. } => count_breaks(children),
                _ => 0,
            })
            .sum()
    }
    fn rewrite(runs: &[Inline], soft: &[bool], k: &mut usize) -> Vec<Inline> {
        let mut out: Vec<Inline> = Vec::with_capacity(runs.len());
        for r in runs {
            match r {
                Inline::LineBreak => {
                    let is_soft = soft.get(*k).copied().unwrap_or(false);
                    *k += 1;
                    if is_soft {
                        match out.last_mut() {
                            Some(Inline::Run(prev)) => prev.text.push(' '),
                            _ => out.push(Inline::Run(InlineRun {
                                text: " ".to_string(),
                                style: CharStyle::default(),
                                source_index: None,
                            })),
                        }
                    } else {
                        out.push(Inline::LineBreak);
                    }
                }
                Inline::Link { href, children } => out.push(Inline::Link {
                    href: href.clone(),
                    children: rewrite(children, soft, k),
                }),
                other => out.push(other.clone()),
            }
        }
        out
    }

    let soft = &para.style.soft_breaks;
    if soft.is_empty() || !soft.iter().any(|b| *b) {
        return None;
    }
    if count_breaks(&para.runs) != soft.len() {
        return None; // stale classification — keep every break (statu quo)
    }
    let mut k = 0usize;
    Some(rewrite(&para.runs, soft, &mut k))
}

/// One paragraph. `style_id` (e.g. `Some("Heading1")`) emits a `w:pStyle`;
/// `num_id` > 0 wires the paragraph into list numbering at `level`.
fn docx_para(para: &Paragraph, style_id: Option<&str>, num_level: u8, ctx: &mut DocxCtx) -> String {
    docx_para_numbered(para, style_id, 0, num_level, ctx)
}

fn docx_para_numbered(
    para: &Paragraph,
    style_id: Option<&str>,
    num_id: usize,
    num_level: u8,
    ctx: &mut DocxCtx,
) -> String {
    docx_para_extra(para, style_id, num_id, num_level, "", ctx)
}

/// [`docx_para_numbered`] plus raw trailing run XML (`extra`) appended after the
/// paragraph's own runs — the carrier for anchored drawings absorbed into their
/// host paragraph by [`docx_page_blocks`].
fn docx_para_extra(
    para: &Paragraph,
    style_id: Option<&str>,
    num_id: usize,
    num_level: u8,
    extra: &str,
    ctx: &mut DocxCtx,
) -> String {
    let mut ppr = String::from("<w:pPr>");
    if let Some(id) = style_id {
        let mut v = String::new();
        esc(id, &mut v);
        ppr.push_str(&format!("<w:pStyle w:val=\"{v}\"/>"));
    }
    if num_id > 0 {
        ppr.push_str(&format!(
            "<w:numPr><w:ilvl w:val=\"{num_level}\"/><w:numId w:val=\"{num_id}\"/></w:numPr>"
        ));
    }
    // Direct paragraph formatting (overrides any referenced named style). Shares
    // the named-style property writer so direct and named formatting agree.
    ppr.push_str(&docx_style_ppr_props(&para.style));
    ppr.push_str("</w:pPr>");

    let mut runs = String::new();
    // Soft line breaks (reconstruction word-wraps) re-flow as spaces so Word
    // paginates natively; hard breaks stay `<w:br/>`. See `reflow_soft_breaks`.
    match reflow_soft_breaks(para) {
        Some(reflowed) => docx_runs(&reflowed, ctx, &mut runs),
        None => docx_runs(&para.runs, ctx, &mut runs),
    }
    format!("<w:p>{ppr}{runs}{extra}</w:p>")
}

fn docx_jc(align: Align) -> Option<&'static str> {
    match align {
        Align::Left => None,
        Align::Center => Some("center"),
        Align::Right => Some("right"),
        Align::Justify => Some("both"),
    }
}

fn docx_heading(h: &Heading, ctx: &mut DocxCtx) -> String {
    let level = h.level.clamp(1, 6);
    // A heading paragraph carries the style id; the outline level is implied by
    // the named style (defined in styles.xml). An explicit `style_ref` on the
    // paragraph wins over the level-derived `HeadingN` so a modeled named style
    // is honoured.
    let style = match h.para.style_ref.as_ref() {
        Some(id) => id.0.clone(),
        None => format!("Heading{level}"),
    };
    docx_para(&h.para, Some(&style), 0, ctx)
}

/// One run sequence → `<w:r>` children (text runs, breaks, inline images, links).
fn docx_runs(runs: &[Inline], ctx: &mut DocxCtx, out: &mut String) {
    for r in runs {
        match r {
            Inline::Run(run) => {
                if run.text.is_empty() {
                    continue;
                }
                let rpr = docx_rpr(&run.style);
                for part in run.text.split_inclusive('\t') {
                    let text = part.trim_end_matches('\t');
                    if !text.is_empty() {
                        let mut t = String::new();
                        esc(text, &mut t);
                        out.push_str(&format!(
                            "<w:r>{rpr}<w:t xml:space=\"preserve\">{t}</w:t></w:r>"
                        ));
                    }
                    if part.ends_with('\t') {
                        out.push_str(&format!("<w:r>{rpr}<w:tab/></w:r>"));
                    }
                }
            }
            Inline::LineBreak => out.push_str("<w:r><w:br/></w:r>"),
            Inline::Image(img) => out.push_str(&docx_inline_image(img, ctx)),
            Inline::Link { href, children } => {
                // An external URL → the canonical `<w:hyperlink r:id="…">`
                // (ECMA-376 §17.16.22) backed by a `TargetMode="External"`
                // relationship — what Word itself writes, so link styling /
                // editing behave natively. Inside a header/footer part (which
                // has no rels part here) the relationship-free `HYPERLINK "url"`
                // field encoding is kept so the r:id cannot dangle. An internal
                // page jump stays a `HYPERLINK \l "page{N}"` field, resolving to
                // the `w:bookmarkStart` named `page{N}` that `docx_body` drops
                // at each page boundary.
                match href {
                    LinkTarget::Url(url) if !url.is_empty() && !ctx.in_hdrftr => {
                        let n = ctx.add_hyperlink(url);
                        out.push_str(&format!("<w:hyperlink r:id=\"rIdHl{n}\">"));
                        docx_runs(children, ctx, out);
                        out.push_str("</w:hyperlink>");
                    }
                    LinkTarget::Url(url) if !url.is_empty() => {
                        let mut esc_url = String::new();
                        esc(url, &mut esc_url);
                        out.push_str(&format!(
                            "<w:r><w:fldChar w:fldCharType=\"begin\"/></w:r>\
<w:r><w:instrText xml:space=\"preserve\"> HYPERLINK \"{esc_url}\" </w:instrText></w:r>\
<w:r><w:fldChar w:fldCharType=\"separate\"/></w:r>"
                        ));
                        docx_runs(children, ctx, out);
                        out.push_str("<w:r><w:fldChar w:fldCharType=\"end\"/></w:r>");
                    }
                    LinkTarget::Page(p) => {
                        out.push_str(&format!(
                            "<w:r><w:fldChar w:fldCharType=\"begin\"/></w:r>\
<w:r><w:instrText xml:space=\"preserve\"> HYPERLINK \\l \"page{}\" </w:instrText></w:r>\
<w:r><w:fldChar w:fldCharType=\"separate\"/></w:r>",
                            p + 1
                        ));
                        docx_runs(children, ctx, out);
                        out.push_str("<w:r><w:fldChar w:fldCharType=\"end\"/></w:r>");
                    }
                    // A named in-document anchor: the canonical `w:hyperlink
                    // w:anchor` (ECMA-376 §17.16.22 — a bookmark jump, no
                    // relationship needed, so it works in header/footer parts
                    // too). The importer reads this exact shape back into
                    // `LinkTarget::Anchor` (resolved to its bookmark's page
                    // when one exists).
                    LinkTarget::Anchor(a) if !a.is_empty() => {
                        let mut esc_a = String::new();
                        esc(a, &mut esc_a);
                        out.push_str(&format!("<w:hyperlink w:anchor=\"{esc_a}\">"));
                        docx_runs(children, ctx, out);
                        out.push_str("</w:hyperlink>");
                    }
                    _ => docx_runs(children, ctx, out),
                }
            }
            // A comment anchor: `w:commentRangeStart`/`End` + the
            // `w:commentReference` run (ECMA-376 §17.13.4), whose numeric id
            // joins the matching body in `word/comments.xml`. A dangling anchor
            // (no comment with that id) emits nothing; header/footer parts skip
            // anchors too (the comments relationship belongs to document.xml).
            Inline::CommentRef { id } => {
                if !ctx.in_hdrftr {
                    if let Some(n) = ctx.comment_num(id) {
                        ctx.used_comments = true;
                        out.push_str(&format!(
                            "<w:commentRangeStart w:id=\"{n}\"/>\
<w:commentRangeEnd w:id=\"{n}\"/>\
<w:r><w:commentReference w:id=\"{n}\"/></w:r>"
                        ));
                    }
                }
            }
            // A footnote anchor: the superscript `w:footnoteReference` run
            // (ECMA-376 §17.11.14), whose numeric id joins the matching body in
            // `word/footnotes.xml`. A dangling anchor (no note with that id)
            // emits nothing; header/footer/footnote parts skip anchors too (the
            // footnotes relationship belongs to document.xml, and Word forbids
            // notes inside notes).
            Inline::FootnoteRef { id } => {
                if !ctx.in_hdrftr {
                    if let Some(n) = ctx.footnote_num(id) {
                        ctx.used_footnotes = true;
                        out.push_str(&format!(
                            "<w:r><w:rPr><w:rStyle w:val=\"FootnoteReference\"/>\
<w:vertAlign w:val=\"superscript\"/></w:rPr><w:footnoteReference w:id=\"{n}\"/></w:r>"
                        ));
                    }
                }
            }
        }
    }
}

/// The `<w:caps/>`/`<w:smallCaps/>`/`<w:position/>`/`<w:w/>`/`<w:kern/>` run
/// properties shared by the direct (`docx_rpr`) and named-style
/// (`docx_named_rpr`) writers — the inverse of the importer's captures.
/// `w:position` and `w:kern` are half-points (pt × 2), `w:w` a percentage.
fn docx_rpr_extras(style: &CharStyle) -> String {
    let mut p = String::new();
    if style.caps {
        p.push_str("<w:caps/>");
    }
    if style.small_caps {
        p.push_str("<w:smallCaps/>");
    }
    if let Some(v) = style.baseline_shift_pt.filter(|v| *v != 0.0) {
        p.push_str(&format!(
            "<w:position w:val=\"{}\"/>",
            (v * 2.0).round() as i64
        ));
    }
    if let Some(v) = style.char_scale_pct.filter(|v| *v > 0.0 && *v != 100.0) {
        p.push_str(&format!("<w:w w:val=\"{}\"/>", v.round() as i64));
    }
    if let Some(v) = style.kern_min_pt.filter(|v| *v > 0.0) {
        p.push_str(&format!(
            "<w:kern w:val=\"{}\"/>",
            (v * 2.0).round().max(1.0) as i64
        ));
    }
    p
}

/// `<w:rPr>` from a [`CharStyle`].
fn docx_rpr(style: &CharStyle) -> String {
    let mut p = String::from("<w:rPr>");
    if !style.family.is_empty() {
        let mut fam = String::new();
        esc(&style.family, &mut fam);
        p.push_str(&format!(
            "<w:rFonts w:ascii=\"{fam}\" w:hAnsi=\"{fam}\" w:cs=\"{fam}\"/>"
        ));
    }
    if style.bold {
        p.push_str("<w:b/>");
    }
    if style.italic {
        p.push_str("<w:i/>");
    }
    p.push_str(&docx_rpr_extras(style));
    if style.underline {
        p.push_str("<w:u w:val=\"single\"/>");
    }
    // A double strike-through supersedes the single one (OOXML forbids both
    // `w:strike` and `w:dstrike` on the same run; the importer sets `strike`
    // alongside `dstrike` only as the degraded-consumer fallback).
    if style.dstrike {
        p.push_str("<w:dstrike/>");
    } else if style.strike {
        p.push_str("<w:strike/>");
    }
    match style.vertical_align {
        VAlign::Super => p.push_str("<w:vertAlign w:val=\"superscript\"/>"),
        VAlign::Sub => p.push_str("<w:vertAlign w:val=\"subscript\"/>"),
        VAlign::Baseline => {}
    }
    if let Some(c) = visible_color(style) {
        p.push_str(&format!("<w:color w:val=\"{c}\"/>"));
    }
    // Run highlight / background → `w:shd@fill` (run shading), the inverse of the
    // importer's `w:rPr/w:shd@fill` → `CharStyle.background` so a highlight
    // round-trips through DOCX. Any colour is emitted (a dark highlight is valid);
    // `None` ⇒ nothing, keeping plain runs unchanged.
    if let Some(bg) = style.background {
        p.push_str(&format!(
            "<w:shd w:val=\"clear\" w:color=\"auto\" w:fill=\"{}\"/>",
            hex(bg)
        ));
    }
    // Letter spacing (`w:spacing w:val` in twips) — round-trips CSS/DOCX.
    if style.letter_spacing_pt != 0.0 {
        p.push_str(&format!(
            "<w:spacing w:val=\"{}\"/>",
            (style.letter_spacing_pt * 20.0).round() as i64
        ));
    }
    // Hidden text (`w:vanish`).
    if style.hidden {
        p.push_str("<w:vanish/>");
    }
    p.push_str(&format!(
        "<w:sz w:val=\"{}\"/>",
        (run_size(style) * 2.0).round().max(1.0) as i64
    ));
    p.push_str("</w:rPr>");
    p
}

/// Emit a [`List`] as a sequence of numbered paragraphs (one `w:numId` for the
/// whole list). Nested block content inside an item keeps its own numbering.
fn docx_list(list: &List, ctx: &mut DocxCtx, out: &mut String) {
    ctx.list_markers.push(DocxNumDef {
        marker: list.marker,
        suffix: list.suffix(),
        start: list.start.max(1),
        prefix: list.marker_prefix.clone(),
        levels: list.levels.clone(),
    });
    let num_id = ctx.list_markers.len();
    for item in &list.items {
        for (i, b) in item.blocks.iter().enumerate() {
            match &b.kind {
                // The item's *first* paragraph carries the bullet/number.
                BlockKind::Paragraph(p) if i == 0 => {
                    out.push_str(&docx_para_numbered(p, None, num_id, item.level, ctx))
                }
                BlockKind::Heading(h) if i == 0 => {
                    out.push_str(&docx_para_numbered(&h.para, None, num_id, item.level, ctx))
                }
                _ => docx_block(b, ctx, out),
            }
        }
        if item.blocks.is_empty() {
            // An empty item still needs a bulleted paragraph so the marker shows.
            out.push_str(&docx_para_numbered(
                &Paragraph::default(),
                None,
                num_id,
                item.level,
                ctx,
            ));
        }
    }
}

/// A [`Table`] → `<w:tbl>` with a `<w:tblGrid>` (column widths) and `w:gridSpan`/
/// `w:vMerge` honouring the cell spans.
fn docx_table(table: &Table, ctx: &mut DocxCtx) -> String {
    let cols = table_col_count(table);
    let widths = docx_col_widths(table, cols);

    let mut grid = String::from("<w:tblGrid>");
    for w in &widths {
        grid.push_str(&format!("<w:gridCol w:w=\"{}\"/>", twips(*w)));
    }
    grid.push_str("</w:tblGrid>");

    // Remaining vertical-merge span per physical column. While >1 a column is
    // covered by a `row_span` cell above, so each lower row needs a `w:vMerge`
    // continuation cell there to keep the grid rectangular (Word requires this
    // when the model expresses a row span by omitting the covered cells).
    let cols = cols.max(1);
    let mut vmerge_left = vec![0usize; cols];

    let mut rows = String::new();
    for row in &table.rows {
        rows.push_str("<w:tr>");
        // `w:trPr` carries the row height and/or the header flag. Child order
        // follows CT_TrPr: `w:trHeight` precedes `w:tblHeader`. Emit the wrapper
        // only when at least one is present (a plain body row keeps no `w:trPr`).
        if row.height.is_some() || row.is_header {
            rows.push_str("<w:trPr>");
            if let Some(h) = row.height {
                rows.push_str(&format!(
                    "<w:trHeight w:val=\"{}\" w:hRule=\"atLeast\"/>",
                    twips(h)
                ));
            }
            if row.is_header {
                rows.push_str("<w:tblHeader/>");
            }
            rows.push_str("</w:trPr>");
        }
        let mut phys = 0usize; // current physical column
        let mut cells = row.cells.iter();
        while phys < cols {
            if vmerge_left[phys] > 1 {
                // A merge from a row above covers this column: emit a continuation.
                rows.push_str("<w:tc><w:tcPr><w:vMerge/></w:tcPr><w:p/></w:tc>");
                vmerge_left[phys] -= 1;
                phys += 1;
                continue;
            }
            match cells.next() {
                Some(cell) => {
                    let span = cell.col_span.max(1) as usize;
                    let rspan = cell.row_span.max(1) as usize;
                    rows.push_str(&docx_cell(cell, span, rspan > 1, ctx));
                    if rspan > 1 {
                        let end = (phys + span).min(cols);
                        for slot in &mut vmerge_left[phys..end] {
                            *slot = rspan;
                        }
                    }
                    phys += span;
                }
                None => break, // row supplied fewer cells than columns
            }
        }
        // Any trailing authored cells beyond `cols` (ragged row): emit as-is.
        for cell in cells {
            let span = cell.col_span.max(1) as usize;
            rows.push_str(&docx_cell(cell, span, cell.row_span.max(1) > 1, ctx));
        }
        rows.push_str("</w:tr>");
    }

    // When the model carries real column widths, pin the layout: without
    // `w:tblLayout w:type="fixed"` Word re-runs its autofit algorithm and
    // discards the reconstructed `tblGrid` widths. (CT_TblPrBase child order:
    // `w:tblW` … `w:tblBorders` … `w:tblLayout`.) A width-less table keeps the
    // autofit default.
    let layout = if table.col_widths.iter().any(|w| *w > 0.0) {
        "<w:tblLayout w:type=\"fixed\"/>"
    } else {
        ""
    };
    format!(
        "<w:tbl><w:tblPr><w:tblW w:w=\"0\" w:type=\"auto\"/>{borders}{layout}</w:tblPr>{grid}{rows}</w:tbl>\
<w:p/>",
        borders = docx_tbl_borders(table),
    )
}

fn docx_tbl_borders(table: &Table) -> String {
    if table.border.width <= 0.0 {
        return String::new();
    }
    let sz = (table.border.width * 8.0).round().max(2.0) as i64; // eighths of a point
    let color = hex(table.border.color);
    let edge = |which: &str| {
        format!("<w:{which} w:val=\"single\" w:sz=\"{sz}\" w:space=\"0\" w:color=\"{color}\"/>")
    };
    format!(
        "<w:tblBorders>{}{}{}{}{}{}</w:tblBorders>",
        edge("top"),
        edge("left"),
        edge("bottom"),
        edge("right"),
        edge("insideH"),
        edge("insideV"),
    )
}

fn docx_cell(cell: &Cell, span: usize, vmerge_restart: bool, ctx: &mut DocxCtx) -> String {
    let mut tcpr = String::from("<w:tcPr>");
    if span > 1 {
        tcpr.push_str(&format!("<w:gridSpan w:val=\"{span}\"/>"));
    }
    if vmerge_restart {
        tcpr.push_str("<w:vMerge w:val=\"restart\"/>");
    }
    // Per-cell borders (`w:tcBorders`, CT_TcBorders child order top → left →
    // bottom → right; the model array is `[top, right, bottom, left]`). Under
    // a declared per-cell regime an unset side emits `w:val="nil"` — an
    // explicit "no line here" so Word doesn't fall back to the table default.
    // Emitted before `w:shd` (CT_TcPrBase order: tcBorders precedes shd).
    if cell.has_borders() {
        let mut sides = String::new();
        for (side, idx) in [("top", 0usize), ("left", 3), ("bottom", 2), ("right", 1)] {
            match &cell.borders[idx] {
                Some(b) => {
                    let sz = (b.width_pt * 8.0).round().max(2.0) as i64;
                    sides.push_str(&format!(
                        "<w:{side} w:val=\"{}\" w:sz=\"{sz}\" w:space=\"0\" w:color=\"{}\"/>",
                        docx_border_val(&b.style),
                        hex(b.color)
                    ));
                }
                None => sides.push_str(&format!("<w:{side} w:val=\"nil\"/>")),
            }
        }
        tcpr.push_str(&format!("<w:tcBorders>{sides}</w:tcBorders>"));
    }
    if let Some(shade) = cell.shading {
        tcpr.push_str(&format!(
            "<w:shd w:val=\"clear\" w:color=\"auto\" w:fill=\"{}\"/>",
            hex(shade)
        ));
    }
    if let Some(va) = cell.vertical_align {
        tcpr.push_str(&format!(
            "<w:vAlign w:val=\"{}\"/>",
            docx_cell_valign_attr(va)
        ));
    }
    tcpr.push_str("</w:tcPr>");

    let mut inner = docx_blocks(&cell.blocks, ctx);
    if inner.is_empty() {
        inner.push_str("<w:p/>"); // a cell must hold at least one block
    }
    format!("<w:tc>{tcpr}{inner}</w:tc>")
}

/// Column count = the widest physical-column extent across all rows.
fn table_col_count(table: &Table) -> usize {
    let from_rows = table
        .rows
        .iter()
        .map(|r| {
            r.cells
                .iter()
                .map(|c| c.col_span.max(1) as usize)
                .sum::<usize>()
        })
        .max()
        .unwrap_or(0);
    from_rows.max(table.col_widths.len())
}

/// Resolve per-column widths (points): explicit `col_widths` where present, else
/// an even split of a default 468pt content width.
fn docx_col_widths(table: &Table, cols: usize) -> Vec<f64> {
    let cols = cols.max(1);
    let mut widths = vec![0.0; cols];
    let default_total = 468.0; // 6.5" content width
    let even = default_total / cols as f64;
    for (i, w) in widths.iter_mut().enumerate() {
        *w = table
            .col_widths
            .get(i)
            .copied()
            .filter(|v| *v > 0.0)
            .unwrap_or(even);
    }
    widths
}

fn docx_textbox(tb: &TextBox, ctx: &mut DocxCtx, out: &mut String) {
    // A model text box flows its blocks inline in the body (a floating frame is
    // unnecessary for reflowable output).
    out.push_str(&docx_blocks(&tb.blocks, ctx));
}

fn docx_sheet(sheet: &SheetBlock, ctx: &mut DocxCtx, out: &mut String) {
    // Render each embedded worksheet as a flowing DOCX table.
    for s in &sheet.sheets {
        out.push_str(&docx_table(&sheet_to_table(s), ctx));
    }
}

fn docx_slides(slides: &SlideBlock, ctx: &mut DocxCtx, out: &mut String) {
    // Flatten slides to flowing paragraphs/images (a Word document has no slides).
    for slide in &slides.slides {
        for ph in &slide.placeholders {
            docx_block(&ph.block, ctx, out);
        }
        for sh in &slide.shapes {
            docx_block(sh, ctx, out);
        }
    }
}

/// A block-level image, wrapped in its own paragraph. A **positioned** block
/// (`frame = Some(rect)`, e.g. every image the PDF reconstruction places)
/// floats at its page coordinates via `wp:anchor`, sized to its real frame — a
/// logo or figure keeps its place instead of collapsing into the text flow at a
/// default extent. A frameless image (a flow import) stays a `wp:inline`.
fn docx_image_para(img: &ImageRef, frame: Option<crate::model::Rect>, ctx: &mut DocxCtx) -> String {
    match frame {
        Some(r) => format!("<w:p>{}</w:p>", docx_anchored_image(img, r, ctx)),
        None => format!("<w:p>{}</w:p>", docx_inline_image(img, ctx)),
    }
}

/// The picture crop as a DrawingML `<a:srcRect>` (thousandths of a percent per
/// side), or `""` when the image is uncropped — the symmetric inverse of the
/// importer's `parse_src_rect`.
fn docx_src_rect(crop: Option<[f64; 4]>) -> String {
    match crop {
        Some([l, t, r, b]) if [l, t, r, b].iter().any(|v| *v != 0.0) => {
            let f = |v: f64| (v.clamp(0.0, 1.0) * 100_000.0).round() as i64;
            format!(
                "<a:srcRect l=\"{}\" t=\"{}\" r=\"{}\" b=\"{}\"/>",
                f(l),
                f(t),
                f(r),
                f(b)
            )
        }
        _ => String::new(),
    }
}

/// The shared `<a:graphic>` picture payload of both the inline and the anchored
/// drawing (ECMA-376 `pic:pic` referencing an already-registered `rId`).
/// `src_rect` is the pre-rendered `<a:srcRect/>` crop (or `""`); CT_BlipFill
/// child order is `a:blip`, `a:srcRect?`, then the fill mode (`a:stretch`).
fn docx_pic_graphic(id: usize, rid: usize, cx: i64, cy: i64, src_rect: &str) -> String {
    format!(
        "<a:graphic xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\">\
<a:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/picture\">\
<pic:pic xmlns:pic=\"http://schemas.openxmlformats.org/drawingml/2006/picture\">\
<pic:nvPicPr><pic:cNvPr id=\"{id}\" name=\"img{id}\"/><pic:cNvPicPr/></pic:nvPicPr>\
<pic:blipFill><a:blip xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" r:embed=\"rId{rid}\"/>\
{src_rect}<a:stretch><a:fillRect/></a:stretch></pic:blipFill>\
<pic:spPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"{cx}\" cy=\"{cy}\"/></a:xfrm>\
<a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></pic:spPr></pic:pic></a:graphicData></a:graphic>"
    )
}

fn docx_inline_image(img: &ImageRef, ctx: &mut DocxCtx) -> String {
    // Resolve the blob via the (single) resource table threaded at the top.
    let (bytes, ext) = match ctx.resolve_image(img.resource) {
        Some(b) if !b.0.is_empty() => b,
        _ => return String::new(),
    };
    let id = ctx.next_obj();
    let rid = ctx.add_image(bytes, ext);
    // The authored display size (`ImageRef::size_pt`, from the source's own
    // `wp:extent`/`svg:width`) when known — a flow image round-trips at its
    // real size instead of the historical 1"-square default (kept as the
    // fallback; reflowable output sizes to the page anyway).
    let (w_pt, h_pt) = match img.size_pt {
        Some((w, h)) if w > 0.0 && h > 0.0 => (w, h),
        _ => (96.0, 96.0),
    };
    let (cx, cy) = (emu(w_pt), emu(h_pt));
    let mut alt = String::new();
    esc(img.alt.as_deref().unwrap_or(""), &mut alt);
    let graphic = docx_pic_graphic(id, rid, cx, cy, &docx_src_rect(img.crop));
    format!(
        "<w:r><w:drawing><wp:inline distT=\"0\" distB=\"0\" distL=\"0\" distR=\"0\">\
<wp:extent cx=\"{cx}\" cy=\"{cy}\"/><wp:effectExtent l=\"0\" t=\"0\" r=\"0\" b=\"0\"/>\
<wp:docPr id=\"{id}\" name=\"img{id}\" descr=\"{alt}\"/><wp:cNvGraphicFramePr/>\
{graphic}</wp:inline></w:drawing></w:r>"
    )
}

/// A floating picture anchored at its model frame, sized to the frame. Model
/// frames are **top-down points** (origin top-left — the convention
/// `pdf_from_model` renders and the geometric reconstruction produces), and
/// OOXML's `wp:posOffset` measures from the top-left page corner too, so the
/// offsets pass through unflipped. (Both this exporter and the importer's
/// `docx_anchor_frame` used to flip Y about the page height — two symmetric
/// bugs that cancelled on the docx→model→docx round-trip but mirrored every
/// anchored image of a PDF-reconstructed model to the wrong end of the page.)
/// `behindDoc` is `0` (the picture was painted with the page content, not as a
/// watermark) and the wrap is `wp:wrapNone`: the source PDF laid its text
/// without any notion of wrapping, so reserving wrap bands around each picture
/// (`wrapSquare`) would push the prose into pages the source never had — an
/// image-rich page must keep its own pagination.
fn docx_anchored_image(img: &ImageRef, r: crate::model::Rect, ctx: &mut DocxCtx) -> String {
    let (bytes, ext) = match ctx.resolve_image(img.resource) {
        Some(b) if !b.0.is_empty() => b,
        _ => return String::new(),
    };
    let id = ctx.next_obj();
    let rid = ctx.add_image(bytes, ext);
    let (cx, cy) = (emu(r.w.max(1.0)), emu(r.h.max(1.0)));
    let (x, y) = (emu(r.x.max(0.0)), emu(r.y.max(0.0)));
    let mut alt = String::new();
    esc(img.alt.as_deref().unwrap_or(""), &mut alt);
    let graphic = docx_pic_graphic(id, rid, cx, cy, &docx_src_rect(img.crop));
    format!(
        "<w:r><w:drawing><wp:anchor distT=\"0\" distB=\"0\" distL=\"0\" distR=\"0\" \
simplePos=\"0\" relativeHeight=\"{id}\" behindDoc=\"0\" locked=\"0\" layoutInCell=\"1\" \
allowOverlap=\"1\"><wp:simplePos x=\"0\" y=\"0\"/>\
<wp:positionH relativeFrom=\"page\"><wp:posOffset>{x}</wp:posOffset></wp:positionH>\
<wp:positionV relativeFrom=\"page\"><wp:posOffset>{y}</wp:posOffset></wp:positionV>\
<wp:extent cx=\"{cx}\" cy=\"{cy}\"/><wp:effectExtent l=\"0\" t=\"0\" r=\"0\" b=\"0\"/>\
<wp:wrapNone/>\
<wp:docPr id=\"{id}\" name=\"img{id}\" descr=\"{alt}\"/><wp:cNvGraphicFramePr/>\
{graphic}</wp:anchor></w:drawing></w:r>"
    )
}

fn docx_shape_para(shape: &Shape, ctx: &mut DocxCtx) -> String {
    let placed = shape_to_placed(shape);
    let id = ctx.next_obj();
    // An inline-flow VML shape (Transitional-native; the `wps:wsp` DrawingML
    // extension has no schema in the ECMA-376 Transitional set and is rejected
    // by `a:graphicData`'s strict wildcard — see docx_vml_shape in office.rs).
    format!("<w:p>{}</w:p>", docx_vml_shape(id, &placed, ""))
}

/// Serialize a running header/footer block list. Identical to [`docx_blocks`]
/// except that a paragraph whose whole text is a **page-number caption**
/// (`Page 1`, `Page 1 / 2`, `1/2`, `- 3 -`, …) is re-emitted with dynamic
/// `PAGE` / `NUMPAGES` fields instead of the frozen digits the reconstruction
/// captured from one physical page — so every page of the exported document
/// numbers itself.
///
/// **Limits**: the reconstruction keeps *one* representative footer copy, so
/// only its literal text is available here; a caption whose digits are mixed
/// into prose (`"3 of 10 — © ACME"`) is conservatively left frozen rather than
/// risking a wrong field split.
fn docx_hdrftr_blocks(blocks: &[Block], ctx: &mut DocxCtx) -> String {
    let mut out = String::new();
    for b in blocks {
        if let BlockKind::Paragraph(p) = &b.kind {
            if let Some(xml) = docx_page_number_para(p) {
                out.push_str(&xml);
                continue;
            }
        }
        docx_block(b, ctx, &mut out);
    }
    out
}

/// One piece of a recognized page-number caption: a literal chunk, the current
/// page number (with its frozen source digits as the cached field result), or
/// the page total.
enum PageNumTok {
    Lit(String),
    Current(String),
    Total(String),
}

/// Parse a header/footer paragraph text as a page-number caption. `None` when
/// the text is anything else (the paragraph then renders verbatim). Recognized
/// forms — the **entire** trimmed text must match, `page` case-insensitively:
///
/// * `Page N` — and `Page N / M`, `Page N of M`, `Page N sur M`, `Page N de M`
/// * `N / M` (also unspaced `N/M`, with or without the `Page ` prefix)
/// * `- N -`
fn parse_page_number_text(text: &str) -> Option<Vec<PageNumTok>> {
    use PageNumTok::{Current, Lit, Total};
    let is_int = |s: &str| !s.is_empty() && s.len() <= 6 && s.bytes().all(|b| b.is_ascii_digit());
    // Normalize an unspaced `N/M` token into `N / M` so one tokenizer serves.
    let mut toks: Vec<String> = Vec::new();
    for t in text.split_whitespace() {
        match t.split_once('/') {
            Some((n, m)) if is_int(n) && is_int(m) => {
                toks.push(n.to_string());
                toks.push("/".to_string());
                toks.push(m.to_string());
            }
            _ => toks.push(t.to_string()),
        }
    }
    let is_page_word = |s: &str| s.eq_ignore_ascii_case("page");
    let is_sep = |s: &str| {
        matches!(s, "/" | "-") || ["of", "sur", "de"].contains(&s.to_ascii_lowercase().as_str())
    };
    match toks.as_slice() {
        // `Page N`
        [w, n] if is_page_word(w) && is_int(n) => {
            Some(vec![Lit(format!("{w} ")), Current(n.clone())])
        }
        // `Page N / M` (any recognized separator word)
        [w, n, sep, m] if is_page_word(w) && is_int(n) && is_sep(sep) && is_int(m) => Some(vec![
            Lit(format!("{w} ")),
            Current(n.clone()),
            Lit(format!(" {sep} ")),
            Total(m.clone()),
        ]),
        // `N / M`
        [n, sep, m] if is_int(n) && sep == "/" && is_int(m) => Some(vec![
            Current(n.clone()),
            Lit(format!(" {sep} ")),
            Total(m.clone()),
        ]),
        // `- N -`
        [d1, n, d2] if d1 == "-" && d2 == "-" && is_int(n) => Some(vec![
            Lit("- ".to_string()),
            Current(n.clone()),
            Lit(" -".to_string()),
        ]),
        _ => None,
    }
}

/// Render a header/footer paragraph whose text is a page-number caption as a
/// `<w:p>` mixing literal runs with `PAGE`/`NUMPAGES` `w:fldSimple` fields (the
/// frozen digits stay as the cached field result, so a non-updating viewer
/// still shows something sensible). `None` when the paragraph is not a pure
/// page-number caption (any non-run inline, or an unrecognized text shape).
fn docx_page_number_para(p: &Paragraph) -> Option<String> {
    // Conservative: only plain runs qualify (a link/image/comment in the middle
    // means this is not a simple caption).
    let mut text = String::new();
    let mut style: Option<&CharStyle> = None;
    for inline in &p.runs {
        match inline {
            Inline::Run(r) => {
                if style.is_none() && !r.text.trim().is_empty() {
                    style = Some(&r.style);
                }
                text.push_str(&r.text);
            }
            _ => return None,
        }
    }
    let toks = parse_page_number_text(text.trim())?;

    let default_style = CharStyle::default();
    let rpr = docx_rpr(style.unwrap_or(&default_style));
    let mut runs = String::new();
    for tok in toks {
        match tok {
            PageNumTok::Lit(s) => {
                let mut t = String::new();
                esc(&s, &mut t);
                runs.push_str(&format!(
                    "<w:r>{rpr}<w:t xml:space=\"preserve\">{t}</w:t></w:r>"
                ));
            }
            PageNumTok::Current(cached) => runs.push_str(&format!(
                "<w:fldSimple w:instr=\" PAGE \"><w:r>{rpr}<w:t>{cached}</w:t></w:r></w:fldSimple>"
            )),
            PageNumTok::Total(cached) => runs.push_str(&format!(
                "<w:fldSimple w:instr=\" NUMPAGES \"><w:r>{rpr}<w:t>{cached}</w:t></w:r></w:fldSimple>"
            )),
        }
    }
    Some(format!(
        "<w:p><w:pPr>{}</w:pPr>{runs}</w:p>",
        docx_style_ppr_props(&p.style)
    ))
}

fn docx_hdrftr_xml(tag: &str, inner: &str) -> String {
    let inner = if inner.is_empty() { "<w:p/>" } else { inner };
    // The VML namespaces (`v`, `o`, `w10`) are declared on the part root exactly
    // as on `word/document.xml`: a header/footer whose blocks include a
    // [`Shape`] renders through `docx_vml_shape` (`<v:rect>`/`<v:shape>`), and a
    // part using the `v:` prefix without declaring it is not well-formed XML —
    // Word and LibreOffice then refuse to open the *whole package*. The classic
    // trigger is a letterhead rule (a thin line) lifted into the running header.
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<w:{tag} xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\" \
xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
xmlns:wp=\"http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing\" \
xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" \
xmlns:v=\"urn:schemas-microsoft-com:vml\" \
xmlns:o=\"urn:schemas-microsoft-com:office:office\" \
xmlns:w10=\"urn:schemas-microsoft-com:office:word\">{inner}</w:{tag}>"
    )
}

/// `word/comments.xml` (ECMA-376 §17.13.4): one `w:comment` per model
/// [`Comment`](crate::model::Comment), its `w:id` the numeric id from `nums`
/// (matching the `w:commentReference` anchors), `w:author`/`w:date` verbatim
/// when present, and the flattened text as a single `w:p` — the same one-line
/// shape the importer produces (it joins comment paragraphs with a space), so
/// a comment round-trips byte-identical.
/// `word/footnotes.xml`: the two stock separator notes Word expects
/// (`w:id="-1"`/`"0"`) plus one `<w:footnote w:id="N">` per model
/// [`Footnote`](crate::model::Footnote), ids matching the
/// `w:footnoteReference` anchors. Each body paragraph carries the
/// `FootnoteText` style; the first opens with the auto-number `w:footnoteRef`
/// run (the note's own call marker), Word's stock shape. Bodies render with
/// `in_hdrftr` set: footnotes.xml has no rels part here, so run hyperlinks
/// keep the relationship-free field encoding and nested anchors are skipped.
fn docx_footnotes_xml(ctx: &mut DocxCtx) -> String {
    let mut notes = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<w:footnotes xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\" \
xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
xmlns:wp=\"http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing\">\
<w:footnote w:type=\"separator\" w:id=\"-1\"><w:p><w:r><w:separator/></w:r></w:p></w:footnote>\
<w:footnote w:type=\"continuationSeparator\" w:id=\"0\"><w:p><w:r><w:continuationSeparator/></w:r></w:p></w:footnote>",
    );
    let was_hdrftr = ctx.in_hdrftr;
    ctx.in_hdrftr = true;
    for i in 0..ctx.footnotes.len() {
        let n = ctx.footnote_nums[i];
        let blocks = ctx.footnotes[i].blocks.clone();
        let mut inner = docx_blocks(&blocks, ctx);
        if inner.is_empty() {
            inner.push_str("<w:p/>");
        }
        // Open the first paragraph with the auto-number `w:footnoteRef` call
        // marker (Word's stock note shape): spliced right after the first
        // paragraph's `w:pPr` so the paragraph properties stay first-and-only.
        let marker_run = "<w:r><w:rPr><w:rStyle w:val=\"FootnoteReference\"/>\
<w:vertAlign w:val=\"superscript\"/></w:rPr><w:footnoteRef/></w:r>\
<w:r><w:t xml:space=\"preserve\"> </w:t></w:r>";
        if let Some(pos) = inner.find("</w:pPr>") {
            inner.insert_str(pos + "</w:pPr>".len(), marker_run);
        } else if inner.starts_with("<w:p/>") {
            inner = format!("<w:p>{marker_run}</w:p>{}", &inner["<w:p/>".len()..]);
        } else if let Some(rest) = inner.strip_prefix("<w:p>") {
            inner = format!("<w:p>{marker_run}{rest}");
        }
        notes.push_str(&format!("<w:footnote w:id=\"{n}\">{inner}</w:footnote>"));
    }
    ctx.in_hdrftr = was_hdrftr;
    notes.push_str("</w:footnotes>");
    notes
}

fn docx_comments_xml(comments: &[crate::model::Comment], nums: &[u64]) -> String {
    let mut body = String::new();
    for (c, n) in comments.iter().zip(nums) {
        let mut attrs = format!(" w:id=\"{n}\"");
        if !c.author.is_empty() {
            let mut v = String::new();
            esc(&c.author, &mut v);
            attrs.push_str(&format!(" w:author=\"{v}\""));
        }
        if !c.date.is_empty() {
            let mut v = String::new();
            esc(&c.date, &mut v);
            attrs.push_str(&format!(" w:date=\"{v}\""));
        }
        let mut t = String::new();
        esc(&c.text, &mut t);
        body.push_str(&format!(
            "<w:comment{attrs}><w:p><w:r><w:t xml:space=\"preserve\">{t}</w:t></w:r></w:p></w:comment>"
        ));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<w:comments xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">{body}</w:comments>"
    )
}

#[allow(clippy::too_many_arguments)] // one flat writer per package part list
fn docx_content_types(
    image_exts: &[&str],
    num: bool,
    comments: bool,
    header_nums: &[usize],
    footer_nums: &[usize],
    extra_hf: &[(String, String, bool)],
    settings: bool,
    footnotes: bool,
) -> String {
    let png = ooxml_image_defaults(image_exts);
    let mut overrides = String::from(
        "<Override PartName=\"/word/document.xml\" \
ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/>\
<Override PartName=\"/word/styles.xml\" \
ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml\"/>",
    );
    if num {
        overrides.push_str(
            "<Override PartName=\"/word/numbering.xml\" \
ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.numbering+xml\"/>",
        );
    }
    if comments {
        overrides.push_str(
            "<Override PartName=\"/word/comments.xml\" \
ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.comments+xml\"/>",
        );
    }
    for n in header_nums {
        overrides.push_str(&format!(
            "<Override PartName=\"/word/header{n}.xml\" \
ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.header+xml\"/>",
        ));
    }
    for n in footer_nums {
        overrides.push_str(&format!(
            "<Override PartName=\"/word/footer{n}.xml\" \
ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.footer+xml\"/>",
        ));
    }
    // First-/even-page header/footer variant parts.
    for (part, _rel, is_header) in extra_hf {
        let kind = if *is_header { "header" } else { "footer" };
        overrides.push_str(&format!(
            "<Override PartName=\"/word/{part}\" \
ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.{kind}+xml\"/>",
        ));
    }
    if settings {
        overrides.push_str(
            "<Override PartName=\"/word/settings.xml\" \
ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.settings+xml\"/>",
        );
    }
    if footnotes {
        overrides.push_str(
            "<Override PartName=\"/word/footnotes.xml\" \
ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.footnotes+xml\"/>",
        );
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
<Default Extension=\"xml\" ContentType=\"application/xml\"/>{png}{overrides}{OOXML_DOCPROPS_OVERRIDES}</Types>"
    )
}

#[allow(clippy::too_many_arguments)] // one flat writer per package part list
fn docx_rels(
    image_exts: &[&str],
    num: bool,
    comments: bool,
    hyperlinks: &[String],
    header_nums: &[usize],
    footer_nums: &[usize],
    extra_hf: &[(String, String, bool)],
    settings: bool,
    footnotes: bool,
) -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rIdStyles\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/>",
    );
    if num {
        s.push_str(
            "<Relationship Id=\"rIdNum\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering\" Target=\"numbering.xml\"/>",
        );
    }
    if comments {
        s.push_str(
            "<Relationship Id=\"rIdComments\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments\" Target=\"comments.xml\"/>",
        );
    }
    // One `TargetMode="External"` relationship per body hyperlink, ids
    // `rIdHl{n}` matching the `<w:hyperlink r:id>` emitted by `docx_runs`.
    for (i, url) in hyperlinks.iter().enumerate() {
        let mut u = String::new();
        esc(url, &mut u);
        s.push_str(&format!(
            "<Relationship Id=\"rIdHl{}\" \
Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink\" \
Target=\"{u}\" TargetMode=\"External\"/>",
            i + 1
        ));
    }
    // One relationship per section header/footer part, ids `rIdHdr{n}`/`rIdFtr{n}`
    // matching the `w:headerReference`/`w:footerReference` in each `w:sectPr`.
    for n in header_nums {
        s.push_str(&format!(
            "<Relationship Id=\"rIdHdr{n}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/header\" Target=\"header{n}.xml\"/>",
        ));
    }
    for n in footer_nums {
        s.push_str(&format!(
            "<Relationship Id=\"rIdFtr{n}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/footer\" Target=\"footer{n}.xml\"/>",
        ));
    }
    // First-/even-page variant parts (`rIdHdrF{n}`/`rIdHdrE{n}`/`rIdFtrF{n}`/
    // `rIdFtrE{n}` → `headerF{n}.xml`/…), matching the typed
    // `w:headerReference`/`w:footerReference` in each `w:sectPr`.
    for (part, rel, is_header) in extra_hf {
        let kind = if *is_header { "header" } else { "footer" };
        s.push_str(&format!(
            "<Relationship Id=\"{rel}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/{kind}\" Target=\"{part}\"/>",
        ));
    }
    if settings {
        s.push_str(
            "<Relationship Id=\"rIdSettings\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/settings\" Target=\"settings.xml\"/>",
        );
    }
    if footnotes {
        s.push_str(
            "<Relationship Id=\"rIdFootnotes\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/footnotes\" Target=\"footnotes.xml\"/>",
        );
    }
    for (i, ext) in image_exts.iter().enumerate() {
        s.push_str(&format!(
            "<Relationship Id=\"rId{id}\" \
Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/image\" \
Target=\"media/image{n}.{ext}\"/>",
            id = 100 + i,
            n = i + 1
        ));
    }
    s.push_str("</Relationships>");
    s
}

/// Style ids reserved by the built-in `Normal` + `Heading1`..`Heading6` styles
/// (ECMA-376 §17.7.4). A modeled [`NamedStyle`] reusing one of these is *not*
/// re-emitted (a duplicate `w:styleId` is invalid OOXML); the built-in keeps
/// its sensible defaults, and a paragraph referencing the id still resolves.
const DOCX_RESERVED_STYLE_IDS: [&str; 7] = [
    "Normal", "Heading1", "Heading2", "Heading3", "Heading4", "Heading5", "Heading6",
];

/// `styles.xml` defining the built-in `Normal` + `Heading1`..`Heading6` (with
/// outline levels) plus every modeled [`NamedStyle`] from the document's
/// [`StyleTable`] (ECMA-376 §17.7.4): each emits a `w:style w:type="paragraph"`
/// with a `w:name`, an optional `w:basedOn`, and `w:pPr`/`w:rPr` from the
/// style's paragraph/character formatting. A paragraph's `style_ref` resolves to
/// the matching `w:styleId`. When the document carries a language tag a
/// `w:docDefaults` default-run language is emitted (ECMA-376 §17.7.5.7) so
/// spell-check / hyphenation pick the right language.
fn docx_styles_xml(lang: Option<&str>, styles: &StyleTable) -> String {
    let defaults = match lang.filter(|s| !s.is_empty()) {
        Some(tag) => {
            let mut v = String::new();
            esc(tag, &mut v);
            format!(
                "<w:docDefaults><w:rPrDefault><w:rPr><w:lang w:val=\"{v}\"/></w:rPr></w:rPrDefault></w:docDefaults>"
            )
        }
        None => String::new(),
    };
    // A modeled style reusing a reserved id is **merged into** the built-in
    // slot (its own `w:pPr`/`w:rPr` deltas on the built-in scaffolding) rather
    // than dropped: skipping it silently lost e.g. a source document's coloured
    // `Heading1` (its `w:color` came back null after one round-trip). The
    // heading scaffolding (`w:keepNext` + `w:outlineLvl`, the level's default
    // size when the modeled style pins none) is kept so headings stay headings.
    let named = |id: &str| styles.named.get(&StyleId(id.to_string()));
    let normal_rpr = named("Normal")
        .map(|st| docx_named_rpr(&st.char_))
        .filter(|r| !r.is_empty())
        .map(|r| format!("<w:rPr>{r}</w:rPr>"))
        .unwrap_or_default();
    let normal_ppr = named("Normal")
        .map(|st| docx_style_ppr_props(&st.para))
        .filter(|p| !p.is_empty())
        .map(|p| format!("<w:pPr>{p}</w:pPr>"))
        .unwrap_or_default();
    let mut s = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<w:styles xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">{defaults}\
<w:style w:type=\"paragraph\" w:default=\"1\" w:styleId=\"Normal\"><w:name w:val=\"Normal\"/>{normal_ppr}{normal_rpr}</w:style>",
    );
    let sizes = [32, 26, 24, 22, 20, 18]; // half-points: H1=16pt … H6=9pt
    for (i, sz) in sizes.iter().enumerate() {
        let lvl = i + 1;
        let modeled = named(&format!("Heading{lvl}"));
        let ppr = modeled
            .map(|st| docx_style_ppr_props(&st.para))
            .unwrap_or_default();
        let rpr = match modeled {
            Some(st) => {
                let mut r = docx_named_rpr(&st.char_);
                if st.char_.size_pt <= 0.0 {
                    // Keep the level's built-in size when the style pins none.
                    r.push_str(&format!("<w:sz w:val=\"{sz}\"/>"));
                }
                r
            }
            None => format!("<w:b/><w:sz w:val=\"{sz}\"/>"),
        };
        // `w:outlineLvl` closes the pPr (it sits at the tail of CT_PPrBase);
        // the built-in `w:keepNext` yields to a modeled one to avoid a
        // duplicate element.
        let keep = if ppr.contains("<w:keepNext/>") {
            ""
        } else {
            "<w:keepNext/>"
        };
        s.push_str(&format!(
            "<w:style w:type=\"paragraph\" w:styleId=\"Heading{lvl}\">\
<w:name w:val=\"heading {lvl}\"/><w:basedOn w:val=\"Normal\"/>\
<w:pPr>{keep}{ppr}<w:outlineLvl w:val=\"{ol}\"/></w:pPr>\
<w:rPr>{rpr}</w:rPr></w:style>",
            ol = i,
        ));
    }
    for (id, style) in &styles.named {
        if id.0.is_empty() || DOCX_RESERVED_STYLE_IDS.contains(&id.0.as_str()) {
            continue;
        }
        s.push_str(&docx_named_style(id, style));
    }
    s.push_str("</w:styles>");
    s
}

/// One `<w:style w:type="paragraph">` for a modeled [`NamedStyle`] (ECMA-376
/// §17.7.4): `w:name` (display name = the style id), `w:basedOn` when the style
/// derives from another (and that parent is not the style itself), and
/// `w:pPr`/`w:rPr` carrying the paragraph/character formatting.
fn docx_named_style(id: &StyleId, style: &NamedStyle) -> String {
    let mut sid = String::new();
    esc(&id.0, &mut sid);
    let mut out =
        format!("<w:style w:type=\"paragraph\" w:styleId=\"{sid}\"><w:name w:val=\"{sid}\"/>");
    if let Some(parent) = style
        .based_on
        .as_ref()
        .filter(|p| !p.0.is_empty() && **p != *id)
    {
        let mut p = String::new();
        esc(&parent.0, &mut p);
        out.push_str(&format!("<w:basedOn w:val=\"{p}\"/>"));
    }
    let ppr = docx_style_ppr_props(&style.para);
    if !ppr.is_empty() {
        out.push_str(&format!("<w:pPr>{ppr}</w:pPr>"));
    }
    let rpr = docx_named_rpr(&style.char_);
    if !rpr.is_empty() {
        out.push_str(&format!("<w:rPr>{rpr}</w:rPr>"));
    }
    out.push_str("</w:style>");
    out
}

/// The inner `<w:pPr>` children (spacing / indent / justification) for a
/// [`ParagraphStyle`], without the enclosing `<w:pPr>` tag. Empty when the style
/// is fully default. Shared by the named-style writer and direct paragraph
/// formatting so the two stay in lock-step.
fn docx_style_ppr_props(ps: &ParagraphStyle) -> String {
    let mut out = String::new();
    let mut spacing = String::new();
    if ps.space_before_pt > 0.0 {
        spacing.push_str(&format!(" w:before=\"{}\"", twips(ps.space_before_pt)));
    }
    if ps.space_after_pt > 0.0 {
        spacing.push_str(&format!(" w:after=\"{}\"", twips(ps.space_after_pt)));
    }
    if let LineHeight::Multiple(m) = ps.line_height {
        spacing.push_str(&format!(
            " w:line=\"{}\" w:lineRule=\"auto\"",
            (m * 240.0).round() as i64
        ));
    } else if let LineHeight::Points(p) = ps.line_height {
        spacing.push_str(&format!(" w:line=\"{}\" w:lineRule=\"exact\"", twips(p)));
    }
    if !spacing.is_empty() {
        out.push_str(&format!("<w:spacing{spacing}/>"));
    }
    if ps.indent_left_pt != 0.0 || ps.indent_right_pt != 0.0 || ps.first_line_pt != 0.0 {
        let mut ind = String::new();
        if ps.indent_left_pt != 0.0 {
            ind.push_str(&format!(" w:left=\"{}\"", twips(ps.indent_left_pt)));
        }
        if ps.indent_right_pt != 0.0 {
            ind.push_str(&format!(" w:right=\"{}\"", twips(ps.indent_right_pt)));
        }
        if ps.first_line_pt > 0.0 {
            ind.push_str(&format!(" w:firstLine=\"{}\"", twips(ps.first_line_pt)));
        } else if ps.first_line_pt < 0.0 {
            ind.push_str(&format!(" w:hanging=\"{}\"", twips(-ps.first_line_pt)));
        }
        out.push_str(&format!("<w:ind{ind}/>"));
    }
    if let Some(jc) = docx_jc(ps.align) {
        out.push_str(&format!("<w:jc w:val=\"{jc}\"/>"));
    }
    if let Some(bg) = ps.background {
        out.push_str(&format!(
            "<w:shd w:val=\"clear\" w:color=\"auto\" w:fill=\"{}\"/>",
            hex(bg)
        ));
    }
    let pbdr = docx_p_bdr(&ps.borders);
    if !pbdr.is_empty() {
        out.push_str(&format!("<w:pBdr>{pbdr}</w:pBdr>"));
    }
    if ps.keep_with_next {
        out.push_str("<w:keepNext/>");
    }
    if ps.keep_together {
        out.push_str("<w:keepLines/>");
    }
    // Authored page break before the paragraph (`w:pageBreakBefore`) — the
    // inverse of the importer's capture, so the intent survives a round trip.
    if ps.page_break_before {
        out.push_str("<w:pageBreakBefore/>");
    }
    // Right-to-left paragraph flow (`w:bidi`) — the inverse of the importer's
    // `w:pPr/w:bidi` capture. Only emitted for an explicit RTL direction, so
    // LTR documents stay byte-identical.
    if ps.direction == Some(crate::model::Direction::Rtl) {
        out.push_str("<w:bidi/>");
    }
    // Tab stops → `<w:tabs>` with one `<w:tab>` per TabStop.
    if !ps.tab_stops.is_empty() {
        let mut tabs = String::new();
        for ts in &ps.tab_stops {
            let val = match ts.align {
                crate::model::TabAlign::Left => "left",
                crate::model::TabAlign::Center => "center",
                crate::model::TabAlign::Right => "right",
                crate::model::TabAlign::Decimal => "decimal",
            };
            tabs.push_str(&format!(
                "<w:tab w:val=\"{val}\" w:pos=\"{}\"/>",
                twips(ts.pos_pt)
            ));
        }
        out.push_str(&format!("<w:tabs>{tabs}</w:tabs>"));
    }
    out
}

/// The four `<w:top>`/`<w:left>`/`<w:bottom>`/`<w:right>` children of a
/// paragraph `<w:pBdr>`, or an empty string when no side carries a border.
/// `w:sz` is the border width in eighths of a point (ECMA-376 §17.3.2.18),
/// matching the table-border convention.
fn docx_p_bdr(borders: &[Option<ParaBorder>; 4]) -> String {
    let mut out = String::new();
    // The model's border array is `[top, right, bottom, left]`; CT_PBdr child
    // order is top, left, bottom, right — map explicitly. (An earlier version
    // enumerated the sides in emission order against the model indices, which
    // swapped the left and right borders on export.)
    for (side, idx) in [("top", 0), ("left", 3), ("bottom", 2), ("right", 1)] {
        if let Some(b) = &borders[idx] {
            let sz = (b.width_pt * 8.0).round().max(2.0) as i64;
            out.push_str(&format!(
                "<w:{side} w:val=\"{}\" w:sz=\"{sz}\" w:space=\"0\" w:color=\"{}\"/>",
                docx_border_val(&b.style),
                hex(b.color)
            ));
        }
    }
    out
}

/// Maps a model border-style string to the ECMA-376 `w:val` token: `solid` →
/// `single`, `none` → `nil`, and the rest pass through unchanged.
fn docx_border_val(style: &str) -> &str {
    match style {
        "solid" => "single",
        "none" => "nil",
        "dashed" | "dotted" | "double" => style,
        _ => "single",
    }
}

/// The inner `<w:rPr>` children for a named style's [`CharStyle`], without the
/// enclosing `<w:rPr>` tag and **without** synthesising a default size/colour —
/// only modeled deltas are emitted so a style that merely sets, e.g., `bold`
/// does not override the inherited font size. Empty when fully default.
fn docx_named_rpr(style: &CharStyle) -> String {
    let mut p = String::new();
    if !style.family.is_empty() {
        let mut fam = String::new();
        esc(&style.family, &mut fam);
        p.push_str(&format!(
            "<w:rFonts w:ascii=\"{fam}\" w:hAnsi=\"{fam}\" w:cs=\"{fam}\"/>"
        ));
    }
    if style.bold {
        p.push_str("<w:b/>");
    }
    if style.italic {
        p.push_str("<w:i/>");
    }
    p.push_str(&docx_rpr_extras(style));
    if style.underline {
        p.push_str("<w:u w:val=\"single\"/>");
    }
    if style.dstrike {
        p.push_str("<w:dstrike/>");
    } else if style.strike {
        p.push_str("<w:strike/>");
    }
    match style.vertical_align {
        VAlign::Super => p.push_str("<w:vertAlign w:val=\"superscript\"/>"),
        VAlign::Sub => p.push_str("<w:vertAlign w:val=\"subscript\"/>"),
        VAlign::Baseline => {}
    }
    if let Some(c) = visible_color(style) {
        p.push_str(&format!("<w:color w:val=\"{c}\"/>"));
    }
    if let Some(bg) = style.background {
        p.push_str(&format!(
            "<w:shd w:val=\"clear\" w:color=\"auto\" w:fill=\"{}\"/>",
            hex(bg)
        ));
    }
    if style.size_pt > 0.0 {
        p.push_str(&format!(
            "<w:sz w:val=\"{}\"/>",
            (style.size_pt * 2.0).round().max(1.0) as i64
        ));
    }
    p
}

/// numbering.xml with one abstract+concrete numbering per list instance. The
/// `markers` slice is one `(marker, ordinal suffix, start)` per list; entry `i`
/// drives the abstract num `i + 1` (matching the `w:numId` written on
/// paragraphs). Every level of a given list shares that list's format (bullet
/// char or ordinal style + suffix); the list's `start` seeds level 0's
/// `w:start` (deeper levels restart at 1).
fn docx_numbering_xml(markers: &[DocxNumDef]) -> String {
    let mut abstracts = String::new();
    let mut nums = String::new();
    for (i, def) in markers.iter().enumerate() {
        let n = i + 1;
        let prefix = def.prefix.as_deref().unwrap_or("");
        let mut lvls = String::new();
        for lvl in 0..9usize {
            // Per-depth style when the list carries real multi-level
            // definitions (`List::levels`, from a DOCX `numbering.xml` /
            // ODF list-style import); the table-wide triple otherwise —
            // level-less lists emit exactly the pre-M2 shape.
            let (marker, suffix, lvl_start) = match def.levels.get(lvl) {
                Some(ls) => (
                    ls.marker,
                    ls.suffix.unwrap_or('.'),
                    if ls.is_ordered() { ls.start.max(1) } else { 1 },
                ),
                None => (
                    def.marker,
                    def.suffix,
                    if lvl == 0 { def.start } else { 1 },
                ),
            };
            let (num_fmt, lvl_text_tmpl, bullet_font) = docx_list_format(marker, suffix);
            let indent = twips(18.0 + 18.0 * lvl as f64);
            // `%N` placeholders are 1-based on the level for ordered lists
            // (the ordinal `prefix` — a `"("` for `(a)`-style templates —
            // opens the text); the template is constant (the bullet char)
            // for bullet lists.
            let lvl_text = match marker {
                ListMarker::Bullet(_) => lvl_text_tmpl,
                _ => {
                    let mut esc_p = String::new();
                    esc(prefix, &mut esc_p);
                    format!(
                        "{esc_p}{}",
                        lvl_text_tmpl.replace("%N", &format!("%{}", lvl + 1))
                    )
                }
            };
            lvls.push_str(&format!(
                "<w:lvl w:ilvl=\"{lvl}\"><w:start w:val=\"{lvl_start}\"/><w:numFmt w:val=\"{num_fmt}\"/>\
<w:lvlText w:val=\"{lvl_text}\"/><w:lvlJc w:val=\"left\"/>\
<w:pPr><w:ind w:left=\"{indent}\" w:hanging=\"360\"/></w:pPr>{bullet_font}</w:lvl>",
            ));
        }
        abstracts.push_str(&format!(
            "<w:abstractNum w:abstractNumId=\"{n}\">{lvls}</w:abstractNum>"
        ));
        nums.push_str(&format!(
            "<w:num w:numId=\"{n}\"><w:abstractNumId w:val=\"{n}\"/></w:num>"
        ));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<w:numbering xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">{abstracts}{nums}</w:numbering>"
    )
}

/// Map a `(marker, ordinal suffix)` to its OOXML numbering shape:
/// `(w:numFmt value, w:lvlText template, optional <w:rPr> bullet font)`.
///
/// The template uses the sentinel `%N`, replaced per level with `%<level>` for
/// ordered lists (followed by the list's ordinal `suffix` — `%1.` or `%1)`);
/// bullet lists embed the literal bullet character.
fn docx_list_format(marker: ListMarker, suffix: char) -> (&'static str, String, &'static str) {
    let bullet_font =
        "<w:rPr><w:rFonts w:ascii=\"Symbol\" w:hAnsi=\"Symbol\" w:hint=\"default\"/></w:rPr>";
    match marker {
        ListMarker::Decimal => ("decimal", format!("%N{suffix}"), ""),
        ListMarker::LowerAlpha => ("lowerLetter", format!("%N{suffix}"), ""),
        ListMarker::UpperAlpha => ("upperLetter", format!("%N{suffix}"), ""),
        ListMarker::LowerRoman => ("lowerRoman", format!("%N{suffix}"), ""),
        ListMarker::UpperRoman => ("upperRoman", format!("%N{suffix}"), ""),
        ListMarker::Bullet(c) => {
            let mut esc_c = String::new();
            esc(&c.to_string(), &mut esc_c);
            ("bullet", esc_c, bullet_font)
        }
    }
}

// ════════════════════════════════════ XLSX ════════════════════════════════════

/// Serialize a [`Document`]'s [`Sheet`]s (from every [`BlockKind::Sheet`]) to a
/// **typed** workbook (`.xlsx`): numbers are stored as numeric cells, text as
/// inline strings, booleans as `b`; number formats, solid fills, and merged
/// ranges are preserved.
///
/// A document with no native sheet blocks (a reconstructed PDF, a Word/HTML
/// import…) falls back to [`sheets_from_pages`]: one worksheet per page, typed
/// tables as real rows/cells (spans merged), and every prose block as a
/// single-cell row in reading order — so the whole document text survives.
pub fn xlsx_from_model(doc: &Document) -> Vec<u8> {
    let mut sheets = collect_sheets(doc);
    if sheets.is_empty() {
        sheets = sheets_from_pages(doc);
    }
    let mut zip = ZipWriter::new();

    // Build the styles table: index 0 is the default; further indices carry a
    // numFmt and/or fill. Cells reference a style index via `s="N"`. Each
    // worksheet also reports its external hyperlink targets, which become the
    // sheet's own `.rels` part.
    let mut styler = XlsxStyler::new();
    let sheet_xmls: Vec<(String, Vec<String>)> = sheets
        .iter()
        .map(|s| xlsx_sheet_from_model(s, &mut styler))
        .collect();

    let names: Vec<String> = sheets.iter().map(|s| s.name.clone()).collect();
    let count = sheet_xmls.len().max(1);

    zip.add_deflated(
        "[Content_Types].xml",
        xlsx_model_content_types(count).as_bytes(),
    );
    zip.add_deflated(
        "_rels/.rels",
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" \
Target=\"xl/workbook.xml\"/>{OOXML_DOCPROPS_RELS}</Relationships>"
        )
        .as_bytes(),
    );
    zip.add_deflated("docProps/core.xml", ooxml_core_props(&doc.meta).as_bytes());
    zip.add_deflated("docProps/app.xml", ooxml_app_props(&doc.meta).as_bytes());
    zip.add_deflated(
        "xl/workbook.xml",
        xlsx_model_workbook(count, &names).as_bytes(),
    );
    zip.add_deflated(
        "xl/_rels/workbook.xml.rels",
        xlsx_model_workbook_rels(count).as_bytes(),
    );
    zip.add_deflated("xl/styles.xml", styler.to_xml().as_bytes());
    if sheet_xmls.is_empty() {
        zip.add_deflated("xl/worksheets/sheet1.xml", xlsx_empty_sheet().as_bytes());
    } else {
        for (i, (xml, ext_links)) in sheet_xmls.iter().enumerate() {
            zip.add_deflated(&format!("xl/worksheets/sheet{}.xml", i + 1), xml.as_bytes());
            // External hyperlink targets live in the worksheet's own rels part.
            if !ext_links.is_empty() {
                zip.add_deflated(
                    &format!("xl/worksheets/_rels/sheet{}.xml.rels", i + 1),
                    xlsx_sheet_rels(ext_links).as_bytes(),
                );
            }
        }
    }
    zip.finish()
}

/// A distinct font record (family/size/bold/italic/colour) for the `<fonts>`
/// table. `size_pt` is stored as integer hundredths of a point so the key is
/// hashable/comparable without `f64` issues.
#[derive(Clone, PartialEq, Eq)]
struct XlsxFont {
    family: String,
    size_centi: u32,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    /// `RRGGBB` hex, or empty for the default (automatic/black).
    color: String,
}

/// A horizontal + vertical alignment + wrap pairing for an `<alignment>` child.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct XlsxAlign {
    /// `None` ⇒ general (no horizontal attribute emitted).
    horizontal: Option<Align>,
    /// `None` ⇒ the OOXML default (bottom; no vertical attribute emitted).
    vertical: Option<CellVAlign>,
    wrap: bool,
    /// Text rotation in the model's signed degrees CCW; emitted as
    /// `@textRotation` (`0..=90` up, `91..=180` = `90 + |deg|` down — the
    /// exact inverse of the importer's `xlsx_text_rotation`). `None` ⇒
    /// horizontal (no attribute).
    rotation_deg: Option<i32>,
    /// Base reading direction; emitted as `@readingOrder` (`Rtl` ⇒ `2`, `Ltr` ⇒
    /// `1`; inverse of the importer's `xlsx_reading_order`). `None` ⇒ context
    /// (no attribute).
    direction: Option<crate::model::Direction>,
}

impl XlsxAlign {
    fn is_default(self) -> bool {
        self.horizontal.is_none()
            && self.vertical.is_none()
            && !self.wrap
            && self.rotation_deg.is_none()
            && self.direction.is_none()
    }
}

/// A full `cellXfs` style record: number format, fill, font, border, alignment.
#[derive(Clone, PartialEq, Eq)]
struct XlsxXf {
    num_fmt_id: u32,
    fill_idx: usize,
    font_idx: usize,
    border_idx: usize,
    align: XlsxAlign,
}

/// Accumulates `cellXfs` style records (numFmt + fill + font + border +
/// alignment) and returns stable indices. Index 0 is always the default xf.
struct XlsxStyler {
    /// Custom number-format codes → builtin/custom numFmtId (starts at 164).
    num_fmts: Vec<(u32, String)>,
    /// Fill colours (RRGGBB) → fill index (0 and 1 are reserved by spec).
    fills: Vec<String>,
    /// Distinct fonts; index 0 is the default Calibri 11.
    fonts: Vec<XlsxFont>,
    /// Distinct borders; index 0 is the empty/no-border border. Each record is
    /// the model's single all-edge border and/or a diagonal line.
    borders: Vec<XlsxBorderRec>,
    /// Style records keyed by their full tuple → cellXfs index.
    xfs: Vec<XlsxXf>,
}

/// One `<border>` record: the all-edge border (the model's single per-cell
/// [`BorderStyle`]) plus an optional diagonal line
/// ([`model::DiagonalBorder`](crate::model::DiagonalBorder)).
#[derive(Clone, PartialEq, Default)]
struct XlsxBorderRec {
    edges: Option<BorderStyle>,
    diagonal: Option<crate::model::DiagonalBorder>,
}

impl XlsxStyler {
    fn new() -> Self {
        // cellXfs[0] is the default (no numFmt, fill 0, font 0, border 0).
        XlsxStyler {
            num_fmts: Vec::new(),
            fills: Vec::new(),
            fonts: vec![XlsxFont {
                family: "Calibri".to_string(),
                size_centi: 1100,
                bold: false,
                italic: false,
                underline: false,
                strike: false,
                color: String::new(),
            }],
            borders: vec![XlsxBorderRec::default()],
            xfs: vec![XlsxXf {
                num_fmt_id: 0,
                fill_idx: 0,
                font_idx: 0,
                border_idx: 0,
                align: XlsxAlign::default(),
            }],
        }
    }

    /// Resolve (or create) the numFmtId for an optional number-format code.
    fn num_fmt_id(&mut self, number_format: Option<&str>) -> u32 {
        match number_format {
            None => 0,
            Some(code) => self
                .num_fmts
                .iter()
                .find(|(_, c)| c == code)
                .map(|(id, _)| *id)
                .unwrap_or_else(|| {
                    let id = 164 + self.num_fmts.len() as u32;
                    self.num_fmts.push((id, code.to_string()));
                    id
                }),
        }
    }

    /// Resolve (or create) the fillId for an optional fill colour. Built-in
    /// fills 0 (none) and 1 (gray125) precede the custom solids.
    fn fill_id(&mut self, fill: Option<[f64; 3]>) -> usize {
        match fill {
            None => 0,
            Some(rgb) => {
                let hexc = hex(rgb);
                let local = self
                    .fills
                    .iter()
                    .position(|f| *f == hexc)
                    .unwrap_or_else(|| {
                        self.fills.push(hexc);
                        self.fills.len() - 1
                    });
                local + 2
            }
        }
    }

    /// Resolve (or create) the fontId for a cell's character style. Returns 0
    /// (the default Calibri 11) when the style carries no distinguishing trait.
    fn font_id(&mut self, style: &CharStyle) -> usize {
        let want = XlsxFont {
            family: if style.family.is_empty() {
                "Calibri".to_string()
            } else {
                style.family.clone()
            },
            size_centi: (run_size(style) * 100.0).round() as u32,
            bold: style.bold,
            italic: style.italic,
            underline: style.underline,
            strike: style.strike,
            color: visible_color(style).unwrap_or_default(),
        };
        if want == self.fonts[0] {
            return 0;
        }
        if let Some(i) = self.fonts.iter().position(|f| *f == want) {
            return i;
        }
        self.fonts.push(want);
        self.fonts.len() - 1
    }

    /// Resolve (or create) the borderId for an optional cell border and/or
    /// diagonal line. Returns 0 (no border) when neither draws anything.
    fn border_id(
        &mut self,
        border: Option<BorderStyle>,
        diagonal: Option<crate::model::DiagonalBorder>,
    ) -> usize {
        let rec = XlsxBorderRec {
            edges: border.filter(|b| b.width > 0.0),
            diagonal: diagonal.filter(|d| (d.up || d.down) && d.border.width > 0.0),
        };
        if rec.edges.is_none() && rec.diagonal.is_none() {
            return 0;
        }
        if let Some(i) = self.borders.iter().position(|x| *x == rec) {
            return i;
        }
        self.borders.push(rec);
        self.borders.len() - 1
    }

    /// Resolve a `cellXfs` index for a sheet cell's full styling.
    fn style_for(&mut self, cell: &SheetCell) -> usize {
        let num_fmt_id = self.num_fmt_id(cell.number_format.as_deref());
        let fill_idx = self.fill_id(cell.fill);
        let font_idx = self.font_id(&cell.style);
        let border_idx = self.border_id(cell.border, cell.diagonal);
        let align = XlsxAlign {
            horizontal: cell.align,
            vertical: cell.vertical_align,
            wrap: cell.wrap,
            rotation_deg: cell.rotation_deg.filter(|d| (-90..=90).contains(d) && *d != 0),
            direction: cell.direction,
        };
        if num_fmt_id == 0
            && fill_idx == 0
            && font_idx == 0
            && border_idx == 0
            && align.is_default()
        {
            return 0;
        }
        let xf = XlsxXf {
            num_fmt_id,
            fill_idx,
            font_idx,
            border_idx,
            align,
        };
        if let Some(i) = self.xfs.iter().position(|x| *x == xf) {
            return i;
        }
        self.xfs.push(xf);
        self.xfs.len() - 1
    }

    fn to_xml(&self) -> String {
        let mut num_fmts = String::new();
        if !self.num_fmts.is_empty() {
            num_fmts.push_str(&format!("<numFmts count=\"{}\">", self.num_fmts.len()));
            for (id, code) in &self.num_fmts {
                let mut c = String::new();
                esc(code, &mut c);
                num_fmts.push_str(&format!("<numFmt numFmtId=\"{id}\" formatCode=\"{c}\"/>"));
            }
            num_fmts.push_str("</numFmts>");
        }

        // Fonts: at least the default Calibri 11.
        let mut fonts = String::new();
        for f in &self.fonts {
            let mut fam = String::new();
            esc(&f.family, &mut fam);
            let mut s = String::from("<font>");
            if f.bold {
                s.push_str("<b/>");
            }
            if f.italic {
                s.push_str("<i/>");
            }
            // CT_Font child order (ECMA-376 §18.8.22): `strike` then `u` precede
            // `sz`. A single underline (`<u/>` defaults to `val="single"`) and a
            // single strike-through, the spreadsheet analogue of the run flags
            // emitted for word-processing/presentation text.
            if f.strike {
                s.push_str("<strike/>");
            }
            if f.underline {
                s.push_str("<u/>");
            }
            s.push_str(&format!(
                "<sz val=\"{}\"/>",
                num(f.size_centi as f64 / 100.0)
            ));
            if !f.color.is_empty() {
                s.push_str(&format!("<color rgb=\"FF{}\"/>", f.color));
            }
            s.push_str(&format!("<name val=\"{fam}\"/></font>"));
            fonts.push_str(&s);
        }

        // Fills: 0 = none, 1 = gray125 (both spec-required), then custom solids.
        let mut fills = String::from(
            "<fill><patternFill patternType=\"none\"/></fill>\
<fill><patternFill patternType=\"gray125\"/></fill>",
        );
        for hexc in &self.fills {
            fills.push_str(&format!(
                "<fill><patternFill patternType=\"solid\"><fgColor rgb=\"FF{hexc}\"/><bgColor indexed=\"64\"/></patternFill></fill>"
            ));
        }
        let fill_count = 2 + self.fills.len();

        // Borders: index 0 is the empty border; the rest are the model's single
        // all-edge style applied to every side, plus any diagonal line
        // (`@diagonalUp`/`@diagonalDown` + a styled `<diagonal>` edge).
        let mut borders = String::new();
        for (i, rec) in self.borders.iter().enumerate() {
            if i == 0 {
                borders.push_str("<border><left/><right/><top/><bottom/><diagonal/></border>");
            } else {
                let mut attrs = String::new();
                if let Some(d) = &rec.diagonal {
                    if d.up {
                        attrs.push_str(" diagonalUp=\"1\"");
                    }
                    if d.down {
                        attrs.push_str(" diagonalDown=\"1\"");
                    }
                }
                let side = |e: &str, b: Option<&BorderStyle>| match b {
                    Some(b) => format!(
                        "<{e} style=\"{}\"><color rgb=\"FF{}\"/></{e}>",
                        xlsx_border_style(b.width),
                        hex(b.color)
                    ),
                    None => format!("<{e}/>"),
                };
                borders.push_str(&format!(
                    "<border{attrs}>{}{}{}{}{}</border>",
                    side("left", rec.edges.as_ref()),
                    side("right", rec.edges.as_ref()),
                    side("top", rec.edges.as_ref()),
                    side("bottom", rec.edges.as_ref()),
                    side("diagonal", rec.diagonal.as_ref().map(|d| &d.border)),
                ));
            }
        }
        let border_count = self.borders.len();

        let mut xfs = String::new();
        for xf in &self.xfs {
            let apply_num = if xf.num_fmt_id != 0 {
                " applyNumberFormat=\"1\""
            } else {
                ""
            };
            let apply_fill = if xf.fill_idx != 0 {
                " applyFill=\"1\""
            } else {
                ""
            };
            let apply_font = if xf.font_idx != 0 {
                " applyFont=\"1\""
            } else {
                ""
            };
            let apply_border = if xf.border_idx != 0 {
                " applyBorder=\"1\""
            } else {
                ""
            };
            let (apply_align, align_child) = if xf.align.is_default() {
                (String::new(), String::new())
            } else {
                let mut a = String::from("<alignment");
                if let Some(h) = xf.align.horizontal {
                    a.push_str(&format!(" horizontal=\"{}\"", xlsx_align_attr(h)));
                }
                if let Some(v) = xf.align.vertical {
                    a.push_str(&format!(" vertical=\"{}\"", xlsx_cell_valign_attr(v)));
                }
                if xf.align.wrap {
                    a.push_str(" wrapText=\"1\"");
                }
                if let Some(deg) = xf.align.rotation_deg {
                    // Model signed degrees CCW → XLSX convention: `0..=90`
                    // slants up, `90 + |deg|` slants down (inverse of the
                    // importer's `xlsx_text_rotation`).
                    let v = if deg >= 0 { deg } else { 90 - deg };
                    a.push_str(&format!(" textRotation=\"{v}\""));
                }
                if let Some(dir) = xf.align.direction {
                    // Base reading direction → `@readingOrder` (inverse of the
                    // importer's `xlsx_reading_order`): RTL ⇒ 2, LTR ⇒ 1.
                    let v = match dir {
                        crate::model::Direction::Rtl => 2,
                        crate::model::Direction::Ltr => 1,
                    };
                    a.push_str(&format!(" readingOrder=\"{v}\""));
                }
                a.push_str("/>");
                (" applyAlignment=\"1\"".to_string(), a)
            };
            xfs.push_str(&format!(
                "<xf numFmtId=\"{nf}\" fontId=\"{fo}\" fillId=\"{fi}\" borderId=\"{bo}\" xfId=\"0\"{apply_num}{apply_font}{apply_fill}{apply_border}{apply_align}>{align_child}</xf>",
                nf = xf.num_fmt_id,
                fo = xf.font_idx,
                fi = xf.fill_idx,
                bo = xf.border_idx,
            ));
        }

        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<styleSheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">\
{num_fmts}\
<fonts count=\"{font_count}\">{fonts}</fonts>\
<fills count=\"{fill_count}\">{fills}</fills>\
<borders count=\"{border_count}\">{borders}</borders>\
<cellStyleXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\"/></cellStyleXfs>\
<cellXfs count=\"{xf_count}\">{xfs}</cellXfs>\
<cellStyles count=\"1\"><cellStyle name=\"Normal\" xfId=\"0\" builtinId=\"0\"/></cellStyles>\
</styleSheet>",
            font_count = self.fonts.len(),
            xf_count = self.xfs.len(),
        )
    }
}

/// The OOXML `<alignment horizontal=...>` value for a model alignment. Justify
/// maps to `justify`; the rest are the obvious literals.
fn xlsx_align_attr(a: Align) -> &'static str {
    match a {
        Align::Left => "left",
        Align::Center => "center",
        Align::Right => "right",
        Align::Justify => "justify",
    }
}

/// The DOCX `w:vAlign@w:val` value for a cell vertical alignment (`CT_VerticalJc`):
/// `top`/`center`/`bottom`. `Middle` uses OOXML's American `center` spelling.
fn docx_cell_valign_attr(v: CellVAlign) -> &'static str {
    match v {
        CellVAlign::Top => "top",
        CellVAlign::Middle => "center",
        CellVAlign::Bottom => "bottom",
    }
}

/// The XLSX `<alignment vertical=...>` value for a cell vertical alignment:
/// `top`/`center`/`bottom` (same `center` spelling as DOCX).
fn xlsx_cell_valign_attr(v: CellVAlign) -> &'static str {
    match v {
        CellVAlign::Top => "top",
        CellVAlign::Middle => "center",
        CellVAlign::Bottom => "bottom",
    }
}

/// The ODF `style:vertical-align` value for a cell vertical alignment:
/// `top`/`middle`/`bottom` (ODF spells the centre value `middle`).
fn odf_cell_valign_attr(v: CellVAlign) -> &'static str {
    match v {
        CellVAlign::Top => "top",
        CellVAlign::Middle => "middle",
        CellVAlign::Bottom => "bottom",
    }
}

/// The DrawingML `a:tcPr@anchor` value for a slide-table cell vertical alignment:
/// `t`/`ctr`/`b`.
fn pptx_cell_anchor(v: CellVAlign) -> &'static str {
    match v {
        CellVAlign::Top => "t",
        CellVAlign::Middle => "ctr",
        CellVAlign::Bottom => "b",
    }
}

/// The OOXML border line style for a point width (`thin`/`medium`/`thick`).
fn xlsx_border_style(width: f64) -> &'static str {
    if width >= 2.5 {
        "thick"
    } else if width >= 1.5 {
        "medium"
    } else {
        "thin"
    }
}

/// The worksheet XML plus the sheet's **external** hyperlink targets in `rId`
/// order (`rId1`…), which the caller turns into the worksheet's `.rels` part.
fn xlsx_sheet_from_model(sheet: &Sheet, styler: &mut XlsxStyler) -> (String, Vec<String>) {
    let mut data = String::new();
    // Cell hyperlinks: `(cell ref, target)` collected while walking the grid,
    // emitted as a `<hyperlinks>` block after `<mergeCells>` (CT_Worksheet
    // order). External URLs resolve via the worksheet rels (`r:id`);
    // in-workbook `#Sheet!A1` targets use the `location` attribute.
    let mut links: Vec<(String, String)> = Vec::new();
    for (r, row) in sheet.rows.iter().enumerate() {
        let mut cells = String::new();
        for (c, cell) in row.cells.iter().enumerate() {
            let s_idx = styler.style_for(cell);
            let s_attr = if s_idx != 0 {
                format!(" s=\"{s_idx}\"")
            } else {
                String::new()
            };
            let r_ref = format!("{}{}", col_letter(c), r + 1);
            if let Some(target) = cell.hyperlink.as_deref().filter(|t| !t.is_empty()) {
                links.push((r_ref.clone(), target.to_string()));
            }
            // The formula (`<f>`) precedes the cached `<v>` in the `<c>` (ECMA-376
            // §18.3.1.4). The model strips the leading `=`, but tolerate a stray
            // one. A formula forces an explicit `<v>` value cell (never the
            // `inlineStr` shorthand, which cannot carry `<f>`); a text result uses
            // `t="str"` per §18.18.11.
            let formula = cell.formula.as_deref().map(|f| {
                let mut e = String::new();
                esc(f.strip_prefix('=').unwrap_or(f), &mut e);
                format!("<f>{e}</f>")
            });
            match (&cell.value, formula) {
                (CellValue::Empty, None) => {
                    if s_idx != 0 {
                        // Keep a styled-but-empty cell so its fill/format shows.
                        cells.push_str(&format!("<c r=\"{r_ref}\"{s_attr}/>"));
                    }
                }
                (CellValue::Empty, Some(f)) => {
                    cells.push_str(&format!("<c r=\"{r_ref}\"{s_attr}>{f}</c>"));
                }
                (CellValue::Number(n), f) => {
                    cells.push_str(&format!(
                        "<c r=\"{r_ref}\"{s_attr}>{}<v>{}</v></c>",
                        f.unwrap_or_default(),
                        num(*n)
                    ));
                }
                (CellValue::Bool(b), f) => {
                    cells.push_str(&format!(
                        "<c r=\"{r_ref}\"{s_attr} t=\"b\">{}<v>{}</v></c>",
                        f.unwrap_or_default(),
                        if *b { 1 } else { 0 }
                    ));
                }
                (CellValue::Text(t), Some(f)) => {
                    // A string-valued formula: `t="str"` with the cached result.
                    let mut esc_t = String::new();
                    esc(t, &mut esc_t);
                    cells.push_str(&format!(
                        "<c r=\"{r_ref}\"{s_attr} t=\"str\">{f}<v>{esc_t}</v></c>"
                    ));
                }
                (CellValue::Text(t), None) => {
                    let mut esc_t = String::new();
                    esc(t, &mut esc_t);
                    cells.push_str(&format!(
                        "<c r=\"{r_ref}\"{s_attr} t=\"inlineStr\"><is><t xml:space=\"preserve\">{esc_t}</t></is></c>"
                    ));
                }
            }
        }
        // Emit the row when it has cells or carries an explicit height.
        let row_attrs = match row.height {
            Some(h) if h > 0.0 => format!(" ht=\"{}\" customHeight=\"1\"", num(h)),
            _ => String::new(),
        };
        if !cells.is_empty() || !row_attrs.is_empty() {
            data.push_str(&format!("<row r=\"{}\"{row_attrs}>{cells}</row>", r + 1));
        }
    }

    let merges = if sheet.merges.is_empty() {
        String::new()
    } else {
        let mut m = format!("<mergeCells count=\"{}\">", sheet.merges.len());
        for mr in &sheet.merges {
            m.push_str(&format!(
                "<mergeCell ref=\"{}{}:{}{}\"/>",
                col_letter(mr.c0),
                mr.r0 + 1,
                col_letter(mr.c1),
                mr.r1 + 1
            ));
        }
        m.push_str("</mergeCells>");
        m
    };

    // `<hyperlinks>` sits after `<mergeCells>` (CT_Worksheet child order).
    let mut ext_targets: Vec<String> = Vec::new();
    let hyperlinks = if links.is_empty() {
        String::new()
    } else {
        let mut h = String::from("<hyperlinks>");
        for (r_ref, target) in links {
            if let Some(location) = target.strip_prefix('#') {
                let mut loc = String::new();
                esc(location, &mut loc);
                h.push_str(&format!("<hyperlink ref=\"{r_ref}\" location=\"{loc}\"/>"));
            } else {
                ext_targets.push(target);
                h.push_str(&format!(
                    "<hyperlink ref=\"{r_ref}\" r:id=\"rId{}\"/>",
                    ext_targets.len()
                ));
            }
        }
        h.push_str("</hyperlinks>");
        h
    };
    // Declare the relationships namespace only when an `r:id` is present, so a
    // link-free worksheet stays byte-identical to the previous output.
    let r_ns = if ext_targets.is_empty() {
        ""
    } else {
        " xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\""
    };

    // Frozen panes: `<sheetViews>` precedes `<cols>`/`<sheetData>`
    // (CT_Worksheet child order). The pane's `topLeftCell` is the first
    // unfrozen cell, `activePane` the quadrant that scrolls — the exact shape
    // Excel writes for View → Freeze Panes. A freeze-less sheet emits nothing
    // (byte-identical output to before).
    let sheet_views = match sheet.freeze {
        Some((rows, cols_f)) if rows > 0 || cols_f > 0 => {
            let top_left = format!("{}{}", col_letter(cols_f as usize), rows + 1);
            let active = match (rows > 0, cols_f > 0) {
                (true, true) => "bottomRight",
                (true, false) => "bottomLeft",
                _ => "topRight",
            };
            let mut pane = String::new();
            if cols_f > 0 {
                pane.push_str(&format!(" xSplit=\"{cols_f}\""));
            }
            if rows > 0 {
                pane.push_str(&format!(" ySplit=\"{rows}\""));
            }
            format!(
                "<sheetViews><sheetView workbookViewId=\"0\">\
<pane{pane} topLeftCell=\"{top_left}\" activePane=\"{active}\" state=\"frozen\"/>\
</sheetView></sheetViews>"
            )
        }
        _ => String::new(),
    };

    let cols = xlsx_cols_xml(&sheet.col_widths);
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"{r_ns}>\
{sheet_views}{cols}<sheetData>{data}</sheetData>{merges}{hyperlinks}</worksheet>"
    );
    (xml, ext_targets)
}

/// The worksheet-level `.rels` part declaring each external hyperlink target
/// (`TargetMode="External"`), `rId1`… matching [`xlsx_sheet_from_model`]'s
/// allocation order.
fn xlsx_sheet_rels(ext_targets: &[String]) -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">",
    );
    for (i, target) in ext_targets.iter().enumerate() {
        let mut t = String::new();
        esc(target, &mut t);
        s.push_str(&format!(
            "<Relationship Id=\"rId{}\" \
Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink\" \
Target=\"{t}\" TargetMode=\"External\"/>",
            i + 1
        ));
    }
    s.push_str("</Relationships>");
    s
}

/// `<cols>` block from per-column point widths (converted to Excel character
/// units ≈ points / 7). Empty when no widths are supplied.
fn xlsx_cols_xml(col_widths: &[f64]) -> String {
    if col_widths.is_empty() {
        return String::new();
    }
    let mut s = String::from("<cols>");
    for (i, w) in col_widths.iter().enumerate() {
        if *w <= 0.0 {
            continue;
        }
        s.push_str(&format!(
            "<col min=\"{n}\" max=\"{n}\" width=\"{width}\" customWidth=\"1\"/>",
            n = i + 1,
            width = num((w / 7.0).max(1.0)),
        ));
    }
    s.push_str("</cols>");
    s
}

fn xlsx_empty_sheet() -> String {
    String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><sheetData/></worksheet>",
    )
}

fn xlsx_model_content_types(sheet_count: usize) -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
<Default Extension=\"xml\" ContentType=\"application/xml\"/>\
<Override PartName=\"/xl/workbook.xml\" \
ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/>\
<Override PartName=\"/xl/styles.xml\" \
ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml\"/>",
    );
    for i in 0..sheet_count {
        s.push_str(&format!(
            "<Override PartName=\"/xl/worksheets/sheet{}.xml\" \
ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>",
            i + 1
        ));
    }
    s.push_str(OOXML_DOCPROPS_OVERRIDES);
    s.push_str("</Types>");
    s
}

fn xlsx_model_workbook(sheet_count: usize, names: &[String]) -> String {
    let mut sheets = String::new();
    for i in 0..sheet_count {
        let raw = names
            .get(i)
            .filter(|n| !n.is_empty())
            .cloned()
            .unwrap_or_else(|| format!("Sheet{}", i + 1));
        let mut nm = String::new();
        esc(&raw, &mut nm);
        sheets.push_str(&format!(
            "<sheet name=\"{nm}\" sheetId=\"{n}\" r:id=\"rId{n}\"/>",
            n = i + 1
        ));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" \
xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\">\
<sheets>{sheets}</sheets></workbook>"
    )
}

fn xlsx_model_workbook_rels(sheet_count: usize) -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rIdStyles\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/>",
    );
    for i in 0..sheet_count {
        s.push_str(&format!(
            "<Relationship Id=\"rId{n}\" \
Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" \
Target=\"worksheets/sheet{n}.xml\"/>",
            n = i + 1
        ));
    }
    s.push_str("</Relationships>");
    s
}

// ════════════════════════════════════ PPTX ════════════════════════════════════

/// Serialize a [`Document`]'s [`Slide`]s (from every [`BlockKind::Slide`]) to a
/// **slide-structured** presentation (`.pptx`): one slide per [`Slide`], with
/// title/body placeholders, free text boxes, images and shapes positioned by
/// their frames. A document with no slide blocks falls back to one slide per
/// page whose blocks flow into a single body text box.
pub fn pptx_from_model(doc: &Document) -> Vec<u8> {
    use crate::convert::office::{PPTX_LAYOUT, PPTX_MASTER, PPTX_THEME};

    // One deck size: the majority slide size, deviant slides scaled onto it
    // (PPTX has a single `p:sldSz` — see `normalize_slide_sizes`).
    let mut model_slides = collect_slides(doc);
    let (sw, sh) = normalize_slide_sizes(&mut model_slides);

    let mut zip = ZipWriter::new();
    // Each embedded image: bytes + its package-native extension (`"png"`,
    // `"jpeg"`, …), so the media part, `r:embed` relationship target and
    // `[Content_Types]` `Default` all carry the resource's real format.
    let mut media: Vec<(Vec<u8>, &'static str)> = Vec::new();
    let mut slide_xmls: Vec<String> = Vec::new();
    // Per-slide relationship accumulator (images + external hyperlinks, in
    // allocation order ⇒ `rId1..`). Drives both the slide XML's `r:embed`/`r:id`
    // and the slide's `.rels`.
    let mut slide_rels: Vec<PptxSlideRels> = Vec::new();
    // Speaker notes per slide: the rendered `notesSlideN.xml` body, or `None`
    // when the slide carries no notes (so no `notesSlide` part is emitted).
    let mut notes_xmls: Vec<Option<String>> = Vec::new();
    let slide_count = model_slides.len();
    for slide in &model_slides {
        let mut rels = PptxSlideRels::default();
        slide_xmls.push(pptx_slide_from_model(
            slide,
            doc,
            &mut media,
            &mut rels,
            slide_count,
        ));
        slide_rels.push(rels);
        notes_xmls.push(
            slide
                .notes
                .as_ref()
                .filter(|n| !n.is_empty())
                .map(|n| pptx_notes_from_model(n, doc)),
        );
    }
    let media_exts: Vec<&str> = media.iter().map(|(_, ext)| *ext).collect();
    // 1-based slide numbers that own a notesSlide part.
    let notes_slides: Vec<usize> = notes_xmls
        .iter()
        .enumerate()
        .filter_map(|(i, n)| n.as_ref().map(|_| i + 1))
        .collect();

    zip.add_deflated(
        "[Content_Types].xml",
        pptx_model_content_types(slide_xmls.len(), &media_exts, &notes_slides).as_bytes(),
    );
    zip.add_deflated(
        "_rels/.rels",
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" \
Target=\"ppt/presentation.xml\"/>{OOXML_DOCPROPS_RELS}</Relationships>"
        )
        .as_bytes(),
    );
    zip.add_deflated("docProps/core.xml", ooxml_core_props(&doc.meta).as_bytes());
    zip.add_deflated("docProps/app.xml", ooxml_app_props(&doc.meta).as_bytes());
    zip.add_deflated(
        "ppt/presentation.xml",
        pptx_model_presentation(slide_xmls.len(), sw, sh).as_bytes(),
    );
    zip.add_deflated(
        "ppt/_rels/presentation.xml.rels",
        pptx_model_presentation_rels(slide_xmls.len()).as_bytes(),
    );
    zip.add_deflated("ppt/theme/theme1.xml", PPTX_THEME.as_bytes());
    zip.add_deflated("ppt/slideMasters/slideMaster1.xml", PPTX_MASTER.as_bytes());
    zip.add_deflated(
        "ppt/slideMasters/_rels/slideMaster1.xml.rels",
        b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout\" \
Target=\"../slideLayouts/slideLayout1.xml\"/>\
<Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme\" \
Target=\"../theme/theme1.xml\"/></Relationships>",
    );
    zip.add_deflated("ppt/slideLayouts/slideLayout1.xml", PPTX_LAYOUT.as_bytes());
    zip.add_deflated(
        "ppt/slideLayouts/_rels/slideLayout1.xml.rels",
        b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster\" \
Target=\"../slideMasters/slideMaster1.xml\"/></Relationships>",
    );
    for (i, xml) in slide_xmls.iter().enumerate() {
        zip.add_deflated(&format!("ppt/slides/slide{}.xml", i + 1), xml.as_bytes());
        let has_notes = notes_xmls[i].is_some();
        zip.add_deflated(
            &format!("ppt/slides/_rels/slide{}.xml.rels", i + 1),
            pptx_model_slide_rels(&slide_rels[i], &media_exts, has_notes, i + 1).as_bytes(),
        );
    }
    // Notes slides: one `notesSlideN.xml` (+ its rels) per slide that has notes,
    // numbered by the owning slide's 1-based index.
    for (i, notes) in notes_xmls.iter().enumerate() {
        if let Some(xml) = notes {
            let n = i + 1;
            zip.add_deflated(
                &format!("ppt/notesSlides/notesSlide{n}.xml"),
                xml.as_bytes(),
            );
            zip.add_deflated(
                &format!("ppt/notesSlides/_rels/notesSlide{n}.xml.rels"),
                pptx_model_notes_rels(n).as_bytes(),
            );
        }
    }
    for (i, (bytes, ext)) in media.iter().enumerate() {
        zip.add_deflated(&format!("ppt/media/image{}.{ext}", i + 1), bytes);
    }
    zip.finish()
}

/// One relationship a slide owns, in allocation order. Both variants become a
/// `Relationship` in the slide's `.rels` and are referenced from the slide XML by
/// their 1-based position (`rId1`, `rId2`, …) — see [`pptx_model_slide_rels`].
enum SlideRel {
    /// An embedded picture: the global index into the presentation's `media`
    /// vector (the part is `ppt/media/image{global+1}.<ext>`).
    Image(usize),
    /// An external hyperlink target (`TargetMode="External"`).
    Hyperlink(String),
    /// An internal slide jump: the 0-based index of the target slide (the part is
    /// `ppt/slides/slide{target+1}.xml`). Drives an `a:hlinkClick` with
    /// `action="ppaction://hlinksldjump"` (a `LinkTarget::Page`).
    SlideJump(usize),
}

/// A slide's relationship accumulator. Images and hyperlinks share one ID space
/// allocated in build order, so the `r:embed`/`r:id` emitted inline always
/// matches the `.rels`. The free-floating image path, the run-image hoist and the
/// run-hyperlink path all allocate through here.
#[derive(Default)]
struct PptxSlideRels {
    rels: Vec<SlideRel>,
}

impl PptxSlideRels {
    /// Register an embedded image (by global media index); returns its `rId`.
    fn add_image(&mut self, global: usize) -> usize {
        self.rels.push(SlideRel::Image(global));
        self.rels.len()
    }
    /// Register an external hyperlink target; returns its `rId`.
    fn add_hyperlink(&mut self, url: &str) -> usize {
        self.rels.push(SlideRel::Hyperlink(url.to_string()));
        self.rels.len()
    }
    /// Register an internal slide jump to a 0-based target slide; returns its `rId`.
    fn add_slide_jump(&mut self, target: usize) -> usize {
        self.rels.push(SlideRel::SlideJump(target));
        self.rels.len()
    }
}

/// A run-level image to hoist onto the slide as a standalone `p:pic` (DrawingML
/// has no picture-in-text-run: `CT_RegularTextRun` carries only `a:rPr`/`a:t`).
/// The `rid` is its image relationship; `alt` becomes the picture's `descr`.
struct RunPic {
    rid: usize,
    alt: String,
}

/// Threaded through the slide run path so runs can allocate image/hyperlink
/// relationships. `doc`/`media`/`rels` are the presentation-wide blob store and
/// the slide's rel accumulator; `pending_pics` collects run images for hoisting
/// onto the slide's shape tree after the text shapes are emitted.
///
/// `inline_links` is `false` for contexts that have no `.rels` to record a
/// hyperlink in (speaker notes): there, links degrade to their plain children and
/// run images are dropped — exactly the historical behaviour for those parts.
struct PptxRunCtx<'a> {
    doc: &'a Document,
    media: &'a mut Vec<(Vec<u8>, &'static str)>,
    rels: &'a mut PptxSlideRels,
    pending_pics: Vec<RunPic>,
    inline_links: bool,
    /// Total slide count in the deck, so an internal page jump
    /// (`LinkTarget::Page`) only emits a relationship to a slide that exists; an
    /// out-of-range target degrades to plain runs (no dangling rId).
    slide_count: usize,
}

impl<'a> PptxRunCtx<'a> {
    fn new(
        doc: &'a Document,
        media: &'a mut Vec<(Vec<u8>, &'static str)>,
        rels: &'a mut PptxSlideRels,
        slide_count: usize,
    ) -> Self {
        PptxRunCtx {
            doc,
            media,
            rels,
            pending_pics: Vec::new(),
            inline_links: true,
            slide_count,
        }
    }
}

/// The `p:ph` `type`/`idx` attributes for a placeholder role. `Title` /
/// `Subtitle` / `Body` map to their canonical tokens; an [`PlaceholderRole::
/// Other`] keeps its **real** role instead of being squashed to `body`: a token
/// already in OOXML's `ST_PlaceholderType` set (ECMA-376 §19.7.10 — `ftr`,
/// `dt`, `sldNum`, `ctrTitle`, …) passes through verbatim, the ODF class
/// tokens the ODP importer preserves (`footer`, `page-number`, `date-time`, …)
/// map to their OOXML equivalent, and only a token with no PPTX counterpart
/// falls back to `body`. The importer reads the token straight back into
/// `Other(token)`, so a footer placeholder stays a footer across a round-trip.
fn pptx_ph_type(role: &PlaceholderRole) -> (&'static str, String) {
    const VALID: [&str; 16] = [
        "title", "body", "ctrTitle", "subTitle", "dt", "sldNum", "ftr", "hdr", "obj", "chart",
        "tbl", "clipArt", "dgm", "media", "sldImg", "pic",
    ];
    let ty: &'static str = match role {
        PlaceholderRole::Title => "title",
        PlaceholderRole::Subtitle => "subTitle",
        PlaceholderRole::Body => "body",
        PlaceholderRole::Other(token) => {
            let token = token.trim();
            match VALID.iter().find(|v| **v == token) {
                Some(v) => v,
                // ODF presentation classes → their OOXML placeholder type.
                None => match token {
                    "footer" => "ftr",
                    "header" => "hdr",
                    "page-number" => "sldNum",
                    "date-time" => "dt",
                    "picture" | "graphic" => "pic",
                    "object" => "obj",
                    "chart" => "chart",
                    "table" => "tbl",
                    "subtitle" => "subTitle",
                    "outline" | "text" => "body",
                    _ => "body",
                },
            }
        }
    };
    let idx = if matches!(ty, "title" | "ctrTitle") {
        String::new()
    } else {
        " idx=\"1\"".to_string()
    };
    (ty, idx)
}

/// Emit the tables and run pictures hoisted out of **one** just-emitted slide
/// shape, immediately after it — keeping the model's block order (= DrawingML
/// paint order) instead of stacking every hoist at the top of the z-stack
/// after all text shapes (#7). `tbl_n`/`pic_n` are slide-wide counters driving
/// the default stacking offsets of frameless hoists.
#[allow(clippy::too_many_arguments)]
fn pptx_flush_hoisted(
    tables: Vec<Table>,
    mut pics: Vec<RunPic>,
    slide: &Slide,
    doc: &Document,
    media: &mut Vec<(Vec<u8>, &'static str)>,
    rels: &mut PptxSlideRels,
    slide_count: usize,
    id: &mut usize,
    tbl_n: &mut usize,
    pic_n: &mut usize,
    tree: &mut String,
) {
    // Hoisted body tables: real `a:tbl` graphicFrames. They have no model frame
    // (they came from a flowing body), so stack them down the slide with a
    // sensible default box spanning the slide width. Emitted before this
    // shape's run-image hoists so pictures inside a table cell are collected.
    for table in &tables {
        let r = crate::model::Rect::new(
            0.0,
            8.0 + 220.0 * *tbl_n as f64,
            slide.geometry.width.max(1.0),
            200.0,
        );
        let mut rctx = PptxRunCtx::new(doc, media, rels, slide_count);
        tree.push_str(&pptx_table_frame(table, r, *id, &mut rctx));
        pics.append(&mut rctx.pending_pics);
        *id += 1;
        *tbl_n += 1;
    }
    // Hoisted run images as standalone pictures (DrawingML run images are not
    // expressible; they become real `p:pic` shapes referencing the same media +
    // relationship). Stacked at a default 96pt box near the top-left.
    for pic in &pics {
        let x = emu(8.0 + 8.0 * *pic_n as f64);
        let y = emu(8.0 + 8.0 * *pic_n as f64);
        let side = emu(96.0);
        let mut alt = String::new();
        esc(&pic.alt, &mut alt);
        tree.push_str(&format!(
            "<p:pic><p:nvPicPr><p:cNvPr id=\"{id}\" name=\"img{id}\" descr=\"{alt}\"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr>\
<p:blipFill><a:blip r:embed=\"rId{rid}\"/><a:stretch><a:fillRect/></a:stretch></p:blipFill>\
<p:spPr><a:xfrm><a:off x=\"{x}\" y=\"{y}\"/><a:ext cx=\"{side}\" cy=\"{side}\"/></a:xfrm>\
<a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></p:spPr></p:pic>",
            rid = pic.rid,
        ));
        *id += 1;
        *pic_n += 1;
    }
}

fn pptx_slide_from_model(
    slide: &Slide,
    doc: &Document,
    media: &mut Vec<(Vec<u8>, &'static str)>,
    rels: &mut PptxSlideRels,
    slide_count: usize,
) -> String {
    let mut tree = String::new();
    let mut id = 2usize;

    // Run images inside a shape's text are hoisted to standalone `p:pic`s and
    // body tables to sibling `p:graphicFrame`s (DrawingML text bodies can hold
    // neither) — but each hoist is flushed **right after its source shape**, so
    // the sp-tree keeps the model's block order = paint order (#7). Slide-wide
    // counters keep the frameless hoists' default stacking offsets distinct.
    let mut tbl_n = 0usize;
    let mut pic_n = 0usize;

    // Placeholders first (title/body/…), positioned by their frame when present.
    for ph in &slide.placeholders {
        let (ph_type, ph_idx) = pptx_ph_type(&ph.role);
        let frame = pptx_xfrm(ph.block.frame, ph.block.rotation);
        // Preserve real list/heading structure (and hoist tables) instead of
        // flattening the body to plain paragraphs (#2). `pptx_collect_body`
        // unwraps a TextBox (the page-fallback body) into its child blocks.
        let (body, tables, pics) = {
            let mut rctx = PptxRunCtx::new(doc, media, rels, slide_count);
            let mut built = pptx_struct_body(std::slice::from_ref(&ph.block), &mut rctx);
            let tables = std::mem::take(&mut built.tables);
            let pics = std::mem::take(&mut rctx.pending_pics);
            (built.into_txbody(), tables, pics)
        };
        tree.push_str(&format!(
            "<p:sp><p:nvSpPr><p:cNvPr id=\"{id}\" name=\"ph{id}\"/><p:cNvSpPr/>\
<p:nvPr><p:ph type=\"{ph_type}\"{ph_idx}/></p:nvPr></p:nvSpPr>\
<p:spPr>{frame}</p:spPr>{body}</p:sp>"
        ));
        id += 1;
        pptx_flush_hoisted(
            tables,
            pics,
            slide,
            doc,
            media,
            rels,
            slide_count,
            &mut id,
            &mut tbl_n,
            &mut pic_n,
            &mut tree,
        );
    }

    // Free-floating shapes (text boxes / images / shapes), in model order.
    for sh in &slide.shapes {
        match &sh.kind {
            BlockKind::Image(img) => {
                if let Some((bytes, ext)) = doc_image(doc, img.resource) {
                    media.push((bytes, ext));
                    let rid = rels.add_image(media.len() - 1);
                    let frame = pptx_xfrm(sh.frame, sh.rotation);
                    tree.push_str(&format!(
                        "<p:pic><p:nvPicPr><p:cNvPr id=\"{id}\" name=\"img{id}\"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr>\
<p:blipFill><a:blip r:embed=\"rId{rid}\"/><a:stretch><a:fillRect/></a:stretch></p:blipFill>\
<p:spPr>{frame}<a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></p:spPr></p:pic>"
                    ));
                    id += 1;
                }
            }
            BlockKind::Shape(shape) => {
                let placed = shape_to_placed(shape);
                let geom = if shape_is_rect(&placed) {
                    "<a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom>".to_string()
                } else {
                    dml_cust_geom(&placed, placed.width, placed.height)
                };
                let frame = pptx_xfrm(sh.frame, sh.rotation);
                // A full gradient primes over the flat solid fill; a drop
                // shadow appends as `a:effectLst` (CT_ShapeProperties order:
                // fill, ln, effectLst).
                let fill = match &shape.fill_gradient {
                    Some(g) if g.stops.len() >= 2 => pptx_grad_fill_xml(g),
                    _ => dml_fill(&placed),
                };
                let effects = shape
                    .shadow
                    .as_ref()
                    .map(pptx_effect_lst_xml)
                    .unwrap_or_default();
                tree.push_str(&format!(
                    "<p:sp><p:nvSpPr><p:cNvPr id=\"{id}\" name=\"s{id}\"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>\
<p:spPr>{frame}{geom}{fill}{ln}{effects}</p:spPr><p:txBody><a:bodyPr/><a:p/></p:txBody></p:sp>",
                    ln = dml_line(&placed),
                ));
                id += 1;
            }
            BlockKind::Table(table) => {
                // A real DrawingML table (`a:tbl`) inside a `p:graphicFrame`,
                // not a paragraph flatten (#26). Default the frame to the slide
                // box when the block is unplaced.
                let r = sh.frame.unwrap_or(crate::model::Rect::new(
                    0.0,
                    0.0,
                    slide.geometry.width,
                    slide.geometry.height,
                ));
                let pics = {
                    let mut rctx = PptxRunCtx::new(doc, media, rels, slide_count);
                    tree.push_str(&pptx_table_frame(table, r, id, &mut rctx));
                    std::mem::take(&mut rctx.pending_pics)
                };
                id += 1;
                pptx_flush_hoisted(
                    Vec::new(),
                    pics,
                    slide,
                    doc,
                    media,
                    rels,
                    slide_count,
                    &mut id,
                    &mut tbl_n,
                    &mut pic_n,
                    &mut tree,
                );
            }
            _ => {
                let frame = pptx_xfrm(sh.frame, sh.rotation);
                // Preserve list/heading structure (and hoist any nested table)
                // for a free text box too (#2).
                let (body, tables, pics) = {
                    let mut rctx = PptxRunCtx::new(doc, media, rels, slide_count);
                    let mut built = pptx_struct_body(std::slice::from_ref(sh), &mut rctx);
                    let tables = std::mem::take(&mut built.tables);
                    let pics = std::mem::take(&mut rctx.pending_pics);
                    (built.into_txbody(), tables, pics)
                };
                tree.push_str(&format!(
                    "<p:sp><p:nvSpPr><p:cNvPr id=\"{id}\" name=\"t{id}\"/><p:cNvSpPr txBox=\"1\"/><p:nvPr/></p:nvSpPr>\
<p:spPr>{frame}<a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom><a:noFill/></p:spPr>{body}</p:sp>"
                ));
                id += 1;
                pptx_flush_hoisted(
                    tables,
                    pics,
                    slide,
                    doc,
                    media,
                    rels,
                    slide_count,
                    &mut id,
                    &mut tbl_n,
                    &mut pic_n,
                    &mut tree,
                );
            }
        }
    }

    // The slide background (`p:bg` MUST precede `p:spTree` in `p:cSld`):
    // the full gradient when the model carries one, else a solid fill from
    // [`Slide::background`]. The importer reads both shapes back
    // (`p:bg/p:bgPr/a:gradFill|a:solidFill`), so backgrounds round-trip
    // instead of silently vanishing on every re-export.
    let bg = match (&slide.background_gradient, slide.background) {
        (Some(g), _) if g.stops.len() >= 2 => Some(format!(
            "<p:bg><p:bgPr>{}<a:effectLst/></p:bgPr></p:bg>",
            pptx_grad_fill_xml(g)
        )),
        (_, Some(rgb)) => Some(format!(
            "<p:bg><p:bgPr><a:solidFill><a:srgbClr val=\"{}\"/></a:solidFill>\
<a:effectLst/></p:bgPr></p:bg>",
            hex(rgb)
        )),
        _ => None,
    }
    .unwrap_or_default();

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<p:sld xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" \
xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\">\
<p:cSld>{bg}<p:spTree><p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>\
<p:grpSpPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"0\" cy=\"0\"/>\
<a:chOff x=\"0\" y=\"0\"/><a:chExt cx=\"0\" cy=\"0\"/></a:xfrm></p:grpSpPr>{tree}</p:spTree></p:cSld></p:sld>"
    )
}

/// A model [`Gradient`](crate::model::Gradient) as a DrawingML `<a:gradFill>`
/// (stop list + linear angle) — the exact inverse of the importer's capture in
/// `pptx_parse_sp_pr`/`pptx_bg_color`. A stop's `alpha < 1` emits the
/// `<a:alpha>` child (thousandths of a percent).
fn pptx_grad_fill_xml(g: &crate::model::Gradient) -> String {
    let mut stops = String::new();
    for s in &g.stops {
        let alpha = if s.alpha < 1.0 {
            format!(
                "<a:alpha val=\"{}\"/>",
                (s.alpha.clamp(0.0, 1.0) * 100_000.0).round() as i64
            )
        } else {
            String::new()
        };
        stops.push_str(&format!(
            "<a:gs pos=\"{}\"><a:srgbClr val=\"{}\">{alpha}</a:srgbClr></a:gs>",
            (s.pos.clamp(0.0, 1.0) * 100_000.0).round() as i64,
            hex(s.color)
        ));
    }
    let ang = (g.angle_deg.rem_euclid(360.0) * 60_000.0).round() as i64;
    format!("<a:gradFill><a:gsLst>{stops}</a:gsLst><a:lin ang=\"{ang}\" scaled=\"1\"/></a:gradFill>")
}

/// A model [`Shadow`](crate::model::Shadow) as a DrawingML
/// `<a:effectLst><a:outerShdw…>` — the inverse of the importer's capture
/// (`dist`/`dir` recomposed from the model's `dx/dy` screen offsets).
fn pptx_effect_lst_xml(s: &crate::model::Shadow) -> String {
    let dist = emu(s.dx_pt.hypot(s.dy_pt));
    let dir = (s.dy_pt.atan2(s.dx_pt).to_degrees().rem_euclid(360.0) * 60_000.0).round() as i64;
    let alpha = if s.alpha < 1.0 {
        format!(
            "<a:alpha val=\"{}\"/>",
            (s.alpha.clamp(0.0, 1.0) * 100_000.0).round() as i64
        )
    } else {
        String::new()
    };
    format!(
        "<a:effectLst><a:outerShdw blurRad=\"{}\" dist=\"{dist}\" dir=\"{dir}\">\
<a:srgbClr val=\"{}\">{alpha}</a:srgbClr></a:outerShdw></a:effectLst>",
        emu(s.blur_pt.max(0.0)),
        hex(s.color)
    )
}

/// `<a:xfrm>` from an optional model frame; absent ⇒ omit (the layout positions
/// the placeholder). Model rectangles are top-down points already. A non-zero
/// block [`Rotation`](crate::model::Rotation) (CCW degrees) becomes the OOXML
/// `@rot` attribute (clockwise 60000ths of a degree) — the exact inverse of the
/// importer's `xfrm_to_frame`, so a rotated element no longer comes out
/// straight on re-export.
fn pptx_xfrm(frame: Option<crate::model::Rect>, rotation: crate::model::Rotation) -> String {
    match frame {
        Some(r) => format!(
            "<a:xfrm{rot}><a:off x=\"{x}\" y=\"{y}\"/><a:ext cx=\"{cx}\" cy=\"{cy}\"/></a:xfrm>",
            rot = pptx_rot_attr(rotation),
            x = emu(r.x),
            y = emu(r.y),
            cx = emu(r.w.max(1.0)),
            cy = emu(r.h.max(1.0)),
        ),
        None => String::new(),
    }
}

/// The OOXML `rot="…"` attribute (with a leading space) for a model rotation:
/// CCW degrees → clockwise 60000ths, normalized to `[0, 360)`. Empty for zero.
fn pptx_rot_attr(rotation: crate::model::Rotation) -> String {
    let cw = (-rotation.degrees()).rem_euclid(360.0);
    let units = (cw * 60_000.0).round() as i64;
    if units == 0 || units == 21_600_000 {
        String::new()
    } else {
        format!(" rot=\"{units}\"")
    }
}

/// A `<p:txBody>` rendering a paragraph list as DrawingML paragraphs.
fn pptx_text_body(paras: &[Paragraph], ctx: &mut PptxRunCtx) -> String {
    let mut body = String::from("<p:txBody><a:bodyPr/><a:lstStyle/>");
    if paras.is_empty() {
        body.push_str("<a:p/>");
    }
    for p in paras {
        body.push_str(&pptx_paragraph(p, ctx));
    }
    body.push_str("</p:txBody>");
    body
}

/// Point size for a heading at `level` (1..=6) when its runs carry no explicit
/// size, mirroring the DOCX built-in heading scale (H1=16pt … H6=9pt). Used so a
/// [`Heading`] reaching a slide stays visibly a heading (#2).
const HEADING_SIZES_PT: [f64; 6] = [20.0, 17.0, 15.0, 13.0, 12.0, 11.0];

/// Style a heading's paragraph for slide rendering: bold every run and bump runs
/// that carry no explicit size to the level's heading size, so a [`Heading`]
/// surfaced on a slide keeps its emphasis instead of degrading to body text.
fn pptx_heading_para(h: &Heading) -> Paragraph {
    let size = HEADING_SIZES_PT[(h.level.clamp(1, 6) - 1) as usize];
    let mut para = h.para.clone();
    for inline in &mut para.runs {
        if let Inline::Run(r) = inline {
            r.style.bold = true;
            if r.style.size_pt <= 0.0 {
                r.style.size_pt = size;
            }
        }
    }
    para
}

/// A slide text-body built from a structured block list, preserving real list
/// bullets and heading emphasis. Tables — which a DrawingML text body cannot
/// contain — are collected for hoisting into sibling `p:graphicFrame`s (#2).
struct PptxBody {
    /// Accumulated `<a:p>` markup (no `<p:txBody>` wrapper).
    paras: String,
    /// Tables encountered in the block list, to emit as graphicFrames.
    tables: Vec<Table>,
    /// Whether any paragraph was emitted (an empty body needs a stub `<a:p/>`).
    any: bool,
}

impl PptxBody {
    fn new() -> Self {
        PptxBody {
            paras: String::new(),
            tables: Vec::new(),
            any: false,
        }
    }

    /// Wrap the accumulated paragraphs in a `<p:txBody>` (with the empty-body
    /// stub when nothing structural was produced).
    fn into_txbody(self) -> String {
        let inner = if self.any {
            self.paras
        } else {
            "<a:p/>".to_string()
        };
        format!("<p:txBody><a:bodyPr/><a:lstStyle/>{inner}</p:txBody>")
    }
}

/// Build a slide text body from a block list, keeping lists (`a:buChar`/
/// `a:buAutoNum` + `lvl`), headings (styled), and ordinary paragraphs as real
/// structure; collect any tables for hoisting. Genuinely unrepresentable blocks
/// (images, shapes, sheets, code, rules) degrade to plain paragraphs via the
/// existing flatten — never silently dropped.
fn pptx_struct_body(blocks: &[Block], ctx: &mut PptxRunCtx) -> PptxBody {
    let mut body = PptxBody::new();
    for b in blocks {
        pptx_collect_body(b, &mut body, ctx);
    }
    body
}

fn pptx_collect_body(block: &Block, body: &mut PptxBody, ctx: &mut PptxRunCtx) {
    match &block.kind {
        BlockKind::Paragraph(p) => {
            body.paras.push_str(&pptx_paragraph(p, ctx));
            body.any = true;
        }
        BlockKind::Heading(h) => {
            body.paras
                .push_str(&pptx_paragraph(&pptx_heading_para(h), ctx));
            body.any = true;
        }
        BlockKind::List(list) => {
            for item in &list.items {
                let mut first = true;
                for ib in &item.blocks {
                    match &ib.kind {
                        // The item's first paragraph/heading carries the bullet.
                        BlockKind::Paragraph(p) if first => {
                            body.paras.push_str(&pptx_paragraph_opt(
                                p,
                                Some((list.marker, item.level)),
                                ctx,
                            ));
                            body.any = true;
                        }
                        BlockKind::Heading(h) if first => {
                            body.paras.push_str(&pptx_paragraph_opt(
                                &pptx_heading_para(h),
                                Some((list.marker, item.level)),
                                ctx,
                            ));
                            body.any = true;
                        }
                        _ => pptx_collect_body(ib, body, ctx),
                    }
                    first = false;
                }
                if item.blocks.is_empty() {
                    // An empty item still shows its bullet.
                    body.paras.push_str(&pptx_paragraph_opt(
                        &Paragraph::default(),
                        Some((list.marker, item.level)),
                        ctx,
                    ));
                    body.any = true;
                }
            }
        }
        BlockKind::Table(table) => body.tables.push(table.clone()),
        BlockKind::TextBox(tb) => {
            for ib in &tb.blocks {
                pptx_collect_body(ib, body, ctx);
            }
        }
        BlockKind::Blockquote(bq) => {
            for ib in &bq.blocks {
                pptx_collect_body(ib, body, ctx);
            }
        }
        // Genuinely unrepresentable as slide-body structure: keep the historical
        // text-only flatten so the content still survives.
        _ => {
            for p in block_to_paras(block) {
                body.paras.push_str(&pptx_paragraph(&p, ctx));
                body.any = true;
            }
        }
    }
}

/// Build a `notesSlide` part (`p:notes`, ECMA-376 §13.3.5) carrying the slide's
/// speaker notes in the `body` placeholder (`<p:ph type="body"/>`). The notes
/// text is the slide's `notes` blocks flattened to paragraphs. The relationship
/// to the owning slide is written separately by [`pptx_model_notes_rels`].
fn pptx_notes_from_model(notes: &[Block], doc: &Document) -> String {
    // Notes own only a back-reference relationship (no image/hyperlink rels), so
    // links degrade to their plain children and run images are dropped here. A
    // throwaway media/rels backs the run ctx; nothing it allocates is serialized.
    let mut sink_media: Vec<(Vec<u8>, &'static str)> = Vec::new();
    let mut sink_rels = PptxSlideRels::default();
    let mut rctx = PptxRunCtx::new(doc, &mut sink_media, &mut sink_rels, 0);
    rctx.inline_links = false;
    let body = pptx_text_body(&blocks_to_paras(notes), &mut rctx);
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<p:notes xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" \
xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\">\
<p:cSld><p:spTree>\
<p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>\
<p:grpSpPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"0\" cy=\"0\"/>\
<a:chOff x=\"0\" y=\"0\"/><a:chExt cx=\"0\" cy=\"0\"/></a:xfrm></p:grpSpPr>\
<p:sp><p:nvSpPr><p:cNvPr id=\"2\" name=\"Notes Placeholder\"/><p:cNvSpPr/>\
<p:nvPr><p:ph type=\"body\" idx=\"1\"/></p:nvPr></p:nvSpPr>\
<p:spPr/>{body}</p:sp>\
</p:spTree></p:cSld></p:notes>"
    )
}

/// Relationships for a `notesSlide` part: the mandatory back-reference to its
/// owning slide (ECMA-376 §13.3.5 — a notesSlide is associated with exactly one
/// slide). `slide_number` is the 1-based slide index.
fn pptx_model_notes_rels(slide_number: usize) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" \
Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide\" \
Target=\"../slides/slide{slide_number}.xml\"/></Relationships>"
    )
}

/// A DrawingML `<a:txBody>` (the in-`a:tc` text body), as opposed to the
/// presentationml `<p:txBody>` used for shapes. Reuses [`pptx_paragraph`]
/// (which already emits `a:p`/`a:r`); only the wrapper element namespace differs.
fn dml_text_body(paras: &[Paragraph], ctx: &mut PptxRunCtx) -> String {
    let mut body = String::from("<a:txBody><a:bodyPr/><a:lstStyle/>");
    if paras.is_empty() {
        body.push_str("<a:p/>");
    }
    for p in paras {
        body.push_str(&pptx_paragraph(p, ctx));
    }
    body.push_str("</a:txBody>");
    body
}

/// URI identifying the DrawingML table payload of an `<a:graphicData>`
/// (ECMA-376 §21.1.3 — `a:tbl` lives under this `uri`).
const DML_TABLE_URI: &str = "http://schemas.openxmlformats.org/drawingml/2006/table";

/// Emit a slide table as a `<p:graphicFrame>` containing a DrawingML `<a:tbl>`
/// (ECMA-376 §21.1.3), placed by `frame` like the other slide shapes.
///
/// Reuses the shared table lowering ([`table_col_count`] / [`docx_col_widths`])
/// and the DrawingML run formatting ([`dml_text_body`] → [`pptx_paragraph`]),
/// so the cell text and styling match the DOCX/ODT path rather than duplicating
/// it. Spans are expressed the DrawingML way: a spanning `<a:tc>` carries
/// `gridSpan`/`rowSpan`, and every covered grid position still emits an
/// `<a:tc>` flagged `hMerge="1"` (horizontal) or `vMerge="1"` (vertical) so the
/// grid stays rectangular (PowerPoint requires a cell at every position).
fn pptx_table_frame(
    table: &Table,
    frame: crate::model::Rect,
    id: usize,
    ctx: &mut PptxRunCtx,
) -> String {
    let cols = table_col_count(table).max(1);
    let widths = docx_col_widths(table, cols);

    // Column grid (widths in EMU).
    let mut grid = String::from("<a:tblGrid>");
    for w in &widths {
        grid.push_str(&format!("<a:gridCol w=\"{}\"/>", emu(*w)));
    }
    grid.push_str("</a:tblGrid>");

    // Vertical-merge bookkeeping: while >1, that physical column is covered by a
    // `rowSpan` cell from a row above and needs a `vMerge="1"` continuation here.
    let mut vmerge_left = vec![0usize; cols];

    let mut rows = String::new();
    for row in &table.rows {
        let h_attr = row
            .height
            .map(|h| format!(" h=\"{}\"", emu(h)))
            .unwrap_or_default();
        rows.push_str(&format!("<a:tr{h_attr}>"));

        let mut phys = 0usize;
        let mut cells = row.cells.iter();
        while phys < cols {
            if vmerge_left[phys] > 1 {
                // Covered by a vertical merge from above.
                rows.push_str("<a:tc vMerge=\"1\"><a:txBody><a:bodyPr/><a:lstStyle/><a:p/></a:txBody><a:tcPr/></a:tc>");
                vmerge_left[phys] -= 1;
                phys += 1;
                continue;
            }
            match cells.next() {
                Some(cell) => {
                    let span = (cell.col_span.max(1) as usize).min(cols - phys).max(1);
                    let rspan = cell.row_span.max(1) as usize;
                    rows.push_str(&pptx_table_cell(cell, span, rspan, ctx));
                    // Horizontal-merge continuations for the spanned columns.
                    for _ in 1..span {
                        rows.push_str("<a:tc hMerge=\"1\"><a:txBody><a:bodyPr/><a:lstStyle/><a:p/></a:txBody><a:tcPr/></a:tc>");
                    }
                    if rspan > 1 {
                        let end = (phys + span).min(cols);
                        for slot in &mut vmerge_left[phys..end] {
                            *slot = rspan;
                        }
                    }
                    phys += span;
                }
                None => break, // fewer authored cells than columns
            }
        }
        // Trailing authored cells beyond the grid (ragged row): emit as-is.
        for cell in cells {
            let span = cell.col_span.max(1) as usize;
            rows.push_str(&pptx_table_cell(
                cell,
                span,
                cell.row_span.max(1) as usize,
                ctx,
            ));
            for _ in 1..span {
                rows.push_str("<a:tc hMerge=\"1\"><a:txBody><a:bodyPr/><a:lstStyle/><a:p/></a:txBody><a:tcPr/></a:tc>");
            }
        }
        rows.push_str("</a:tr>");
    }

    // `firstRow="1"` (a styled header row) is driven by the model's
    // `Row::is_header`, not hardcoded: a headerless table must not grow a fake
    // styled header (and `bandRow` banding has no model source, so it is not
    // claimed either). The importer maps `firstRow` back to `is_header`.
    let tbl_pr = if table.rows.first().is_some_and(|r| r.is_header) {
        "<a:tblPr firstRow=\"1\"/>"
    } else {
        "<a:tblPr/>"
    };
    format!(
        "<p:graphicFrame><p:nvGraphicFramePr>\
<p:cNvPr id=\"{id}\" name=\"tbl{id}\"/><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr>\
<p:xfrm><a:off x=\"{x}\" y=\"{y}\"/><a:ext cx=\"{cx}\" cy=\"{cy}\"/></p:xfrm>\
<a:graphic><a:graphicData uri=\"{uri}\">\
<a:tbl>{tbl_pr}{grid}{rows}</a:tbl>\
</a:graphicData></a:graphic></p:graphicFrame>",
        x = emu(frame.x),
        y = emu(frame.y),
        cx = emu(widths.iter().sum::<f64>().max(frame.w).max(1.0)),
        cy = emu(frame.h.max(1.0)),
        uri = DML_TABLE_URI,
    )
}

/// A single DrawingML `<a:tc>`: text body first, then `<a:tcPr>` (the schema
/// order). `span`/`rspan` map to `gridSpan`/`rowSpan`; cell shading becomes a
/// `<a:solidFill>` in `tcPr`.
fn pptx_table_cell(cell: &Cell, span: usize, rspan: usize, ctx: &mut PptxRunCtx) -> String {
    let mut attrs = String::new();
    if span > 1 {
        attrs.push_str(&format!(" gridSpan=\"{span}\""));
    }
    if rspan > 1 {
        attrs.push_str(&format!(" rowSpan=\"{rspan}\""));
    }
    let body = dml_text_body(&pptx_cell_paras(&cell.blocks), ctx);
    // `a:tcPr@anchor` carries the cell's vertical alignment (`t`/`ctr`/`b`).
    let anchor = match cell.vertical_align {
        Some(va) => format!(" anchor=\"{}\"", pptx_cell_anchor(va)),
        None => String::new(),
    };
    let tcpr = match cell.shading {
        Some(shade) => format!(
            "<a:tcPr{anchor}><a:solidFill><a:srgbClr val=\"{}\"/></a:solidFill></a:tcPr>",
            hex(shade)
        ),
        None => format!("<a:tcPr{anchor}/>"),
    };
    format!("<a:tc{attrs}>{body}{tcpr}</a:tc>")
}

/// Flatten a DrawingML table cell's blocks to paragraphs. Unlike the generic
/// [`blocks_to_paras`], a **nested table** — which DrawingML cannot express
/// inside an `<a:tc>` (an `a:txBody` holds only text paragraphs) — is rendered
/// as *structured text lines*: one paragraph per nested-table row, its cells'
/// plain text joined by tab separators (header rows bold). Degraded fidelity,
/// but the sub-table stays readable as rows instead of every nested cell
/// silently collapsing into a run-on sequence of unrelated paragraphs.
fn pptx_cell_paras(blocks: &[Block]) -> Vec<Paragraph> {
    fn walk(block: &Block, out: &mut Vec<Paragraph>) {
        match &block.kind {
            BlockKind::Table(nested) => {
                for row in &nested.rows {
                    let line = row
                        .cells
                        .iter()
                        .map(cell_plain_text)
                        .collect::<Vec<_>>()
                        .join("\t");
                    let mut p = plain_para(&line);
                    if row.is_header {
                        if let Some(Inline::Run(r)) = p.runs.first_mut() {
                            r.style.bold = true;
                        }
                    }
                    out.push(p);
                }
            }
            BlockKind::TextBox(tb) => {
                for b in &tb.blocks {
                    walk(b, out);
                }
            }
            BlockKind::Blockquote(bq) => {
                for b in &bq.blocks {
                    walk(b, out);
                }
            }
            BlockKind::List(list) => {
                for item in &list.items {
                    for b in &item.blocks {
                        walk(b, out);
                    }
                }
            }
            _ => collect_paras(block, out),
        }
    }
    let mut out = Vec::new();
    for b in blocks {
        walk(b, &mut out);
    }
    out
}

/// A model paragraph → a DrawingML `<a:p>` with a `<a:pPr>` carrying alignment
/// **and** spacing/indent/line-height (#2). DrawingML uses EMU for `marL`/`indent`
/// and `a:spcPts`/`a:spcPct` children for the spacing, mirroring the model→ODF
/// mapping in [`odf_para_prop_attrs`].
fn pptx_paragraph(para: &Paragraph, ctx: &mut PptxRunCtx) -> String {
    pptx_paragraph_opt(para, None, ctx)
}

/// A model paragraph → `<a:p>`, optionally carrying a list bullet. When `bullet`
/// is `Some((marker, level))` the paragraph becomes a list item: `<a:pPr>` gains
/// the 0-based `lvl`, an indent for that depth, and a `a:buChar` (unordered) or
/// `a:buAutoNum` (ordered) child — so a model [`List`] reaching a slide keeps its
/// real bullet structure instead of being flattened to plain text (#2).
fn pptx_paragraph_opt(
    para: &Paragraph,
    bullet: Option<(ListMarker, u8)>,
    ctx: &mut PptxRunCtx,
) -> String {
    let ps = &para.style;
    let mut attrs = String::new();
    if let Some((_, level)) = bullet {
        attrs.push_str(&format!(" lvl=\"{}\"", level.min(8)));
    }
    // Right-to-left paragraph flow (`a:pPr@rtl="1"`) — the inverse of the
    // importer's capture; LTR paragraphs emit nothing (byte-identical).
    if ps.direction == Some(crate::model::Direction::Rtl) {
        attrs.push_str(" rtl=\"1\"");
    }
    // `marL` = left indent, `marR` = right indent, `indent` = first-line/hanging
    // (signed) — all in EMU (ECMA-376 §21.1.2.2.7). `algn` is the horizontal
    // alignment. Left/zero default ⇒ omit so untouched paragraphs are unchanged.
    // A list item with no explicit indent gets a per-level hanging indent so its
    // bullet/number sits in the margin (PowerPoint's usual list geometry).
    let list_indent_pt = bullet.map(|(_, lvl)| 18.0 * (lvl as f64 + 1.0));
    let mar_l = if ps.indent_left_pt > 0.0 {
        Some(ps.indent_left_pt)
    } else {
        list_indent_pt
    };
    if let Some(m) = mar_l {
        attrs.push_str(&format!(" marL=\"{}\"", emu(m)));
    }
    if ps.indent_right_pt > 0.0 {
        attrs.push_str(&format!(" marR=\"{}\"", emu(ps.indent_right_pt)));
    }
    if ps.first_line_pt != 0.0 {
        attrs.push_str(&format!(" indent=\"{}\"", emu(ps.first_line_pt)));
    } else if list_indent_pt.is_some() {
        // Hang the marker back to the level's left edge.
        attrs.push_str(&format!(" indent=\"{}\"", emu(-18.0)));
    }
    attrs.push_str(match ps.align {
        Align::Left => "",
        Align::Center => " algn=\"ctr\"",
        Align::Right => " algn=\"r\"",
        Align::Justify => " algn=\"just\"",
    });

    // `CT_TextParagraphProperties` child order: lnSpc, spcBef, spcAft, then the
    // bullet group (buFont, buChar|buAutoNum). Line height: a multiple →
    // `a:spcPct` (1/1000th %, 100% = 100000); a fixed leading → `a:spcPts`
    // (centipoints). Before/after spacing → `a:spcPts`.
    let mut children = String::new();
    match ps.line_height {
        LineHeight::Multiple(m) => children.push_str(&format!(
            "<a:lnSpc><a:spcPct val=\"{}\"/></a:lnSpc>",
            (m * 100_000.0).round() as i64
        )),
        LineHeight::Points(p) => children.push_str(&format!(
            "<a:lnSpc><a:spcPts val=\"{}\"/></a:lnSpc>",
            (p * 100.0).round().max(0.0) as i64
        )),
        LineHeight::Normal => {}
    }
    if ps.space_before_pt > 0.0 {
        children.push_str(&format!(
            "<a:spcBef><a:spcPts val=\"{}\"/></a:spcBef>",
            (ps.space_before_pt * 100.0).round() as i64
        ));
    }
    if ps.space_after_pt > 0.0 {
        children.push_str(&format!(
            "<a:spcAft><a:spcPts val=\"{}\"/></a:spcAft>",
            (ps.space_after_pt * 100.0).round() as i64
        ));
    }
    if let Some((marker, _)) = bullet {
        children.push_str(&pptx_bullet(marker));
    }

    let ppr = if children.is_empty() {
        format!("<a:pPr{attrs}/>")
    } else {
        format!("<a:pPr{attrs}>{children}</a:pPr>")
    };

    let mut runs = String::new();
    pptx_runs(&para.runs, ctx, &mut runs);
    format!("<a:p>{ppr}{runs}</a:p>")
}

/// The DrawingML bullet child for a [`ListMarker`]: `a:buChar` for an unordered
/// bullet, `a:buAutoNum` (with the matching `type`) for an ordered list. The
/// bullet font is forced to a symbol-bearing face so the glyph renders.
fn pptx_bullet(marker: ListMarker) -> String {
    match marker {
        ListMarker::Bullet(c) => {
            let mut b = String::new();
            esc(&c.to_string(), &mut b);
            format!("<a:buFont typeface=\"Arial\"/><a:buChar char=\"{b}\"/>")
        }
        ListMarker::Decimal => "<a:buAutoNum type=\"arabicPeriod\"/>".to_string(),
        ListMarker::LowerAlpha => "<a:buAutoNum type=\"alphaLcPeriod\"/>".to_string(),
        ListMarker::UpperAlpha => "<a:buAutoNum type=\"alphaUcPeriod\"/>".to_string(),
        ListMarker::LowerRoman => "<a:buAutoNum type=\"romanLcPeriod\"/>".to_string(),
        ListMarker::UpperRoman => "<a:buAutoNum type=\"romanUcPeriod\"/>".to_string(),
    }
}

fn pptx_runs(runs: &[Inline], ctx: &mut PptxRunCtx, out: &mut String) {
    for r in runs {
        match r {
            Inline::Run(run) => {
                if run.text.is_empty() {
                    continue;
                }
                let mut t = String::new();
                esc(&run.text, &mut t);
                out.push_str(&format!(
                    "<a:r>{rpr}<a:t>{t}</a:t></a:r>",
                    rpr = pptx_rpr(&run.style, None)
                ));
            }
            Inline::LineBreak => out.push_str("<a:br/>"),
            // A run-level image is hoisted to a standalone `p:pic` on the slide
            // (DrawingML text runs cannot embed a picture — #2). Resolve the blob,
            // register the media + image relationship, and queue the picture.
            // Dropped in link-less contexts (notes), as before.
            Inline::Image(img) => {
                if ctx.inline_links {
                    if let Some((bytes, ext)) = doc_image(ctx.doc, img.resource) {
                        if !bytes.is_empty() {
                            ctx.media.push((bytes, ext));
                            let rid = ctx.rels.add_image(ctx.media.len() - 1);
                            ctx.pending_pics.push(RunPic {
                                rid,
                                alt: img.alt.clone().unwrap_or_default(),
                            });
                        }
                    }
                }
            }
            // A hyperlink → an `a:hlinkClick r:id` carried on each child run's
            // `a:rPr` (#2). An external URL adds a `TargetMode="External"`
            // relationship; an internal `LinkTarget::Page` adds a `slide`
            // relationship + `action="…hlinksldjump"` (only when the target slide
            // exists). A link-less context (notes) or an out-of-range page degrades
            // to plain runs.
            Inline::Link { href, children } => match href {
                LinkTarget::Url(url) if ctx.inline_links && !url.is_empty() => {
                    let rid = ctx.rels.add_hyperlink(url);
                    pptx_link_runs(
                        children,
                        ctx,
                        PptxHlink {
                            rid,
                            slide_jump: false,
                        },
                        out,
                    );
                }
                LinkTarget::Page(p) if ctx.inline_links && *p < ctx.slide_count => {
                    let rid = ctx.rels.add_slide_jump(*p);
                    pptx_link_runs(
                        children,
                        ctx,
                        PptxHlink {
                            rid,
                            slide_jump: true,
                        },
                        out,
                    );
                }
                _ => pptx_runs(children, ctx, out),
            },
            // DEFERRED: PPTX review comments need a whole part family of their
            // own (`ppt/commentAuthors.xml` + per-slide `ppt/comments/
            // commentN.xml`, each with authors registry, rels and content
            // types) and anchor by slide *position*, not by a run marker — a
            // disproportionate build for a run-level anchor. DOCX/ODT emit real
            // comments; here the anchor stays a clean no-op (no dangling ids).
            Inline::CommentRef { .. } => {}
            // PPTX has no footnote part; the anchor degrades to a visible
            // superscript ordinal run (the note's 1-based position) so the
            // call marker survives on the slide instead of vanishing. The
            // note *bodies* have no natural PPTX home and are not emitted
            // (documented lossy — DOCX/ODT carry them in full).
            Inline::FootnoteRef { id } => {
                if let Some(pos) = ctx.doc.footnotes.iter().position(|f| f.id == *id) {
                    out.push_str(&format!(
                        "<a:r><a:rPr lang=\"en-US\" baseline=\"30000\"/>\
<a:t>{}</a:t></a:r>",
                        pos + 1
                    ));
                }
            }
        }
    }
}

/// A run-level hyperlink reference: the slide-rels relationship id plus whether
/// it is an internal slide jump (which needs `action="ppaction://hlinksldjump"`
/// on the `a:hlinkClick`) or a plain external URL.
#[derive(Clone, Copy)]
struct PptxHlink {
    rid: usize,
    slide_jump: bool,
}

/// Render a hyperlink's child runs, threading the link relationship onto each
/// regular run's `a:rPr` as `<a:hlinkClick r:id="rId…"/>`. Nested non-run inlines
/// (line breaks, images, nested links) fall back to the ordinary run path.
fn pptx_link_runs(runs: &[Inline], ctx: &mut PptxRunCtx, link: PptxHlink, out: &mut String) {
    for r in runs {
        match r {
            Inline::Run(run) => {
                if run.text.is_empty() {
                    continue;
                }
                let mut t = String::new();
                esc(&run.text, &mut t);
                out.push_str(&format!(
                    "<a:r>{rpr}<a:t>{t}</a:t></a:r>",
                    rpr = pptx_rpr(&run.style, Some(link))
                ));
            }
            Inline::LineBreak => out.push_str("<a:br/>"),
            other => pptx_runs(std::slice::from_ref(other), ctx, out),
        }
    }
}

/// `<a:rPr>` from a [`CharStyle`]. `link`, when set, adds an
/// `<a:hlinkClick r:id="rId…"/>` — an external URL, or an internal slide jump
/// carrying `action="ppaction://hlinksldjump"` when `slide_jump` is set.
fn pptx_rpr(style: &CharStyle, link: Option<PptxHlink>) -> String {
    let mut attrs = format!(
        "lang=\"en-US\" sz=\"{}\"",
        (run_size(style) * 100.0).round().max(100.0) as i64
    );
    // Letter spacing → `spc` (ECMA-376 §21.1.2.3.9, hundredths of a point) —
    // the PPTX analogue of DOCX `w:spacing w:val` / CSS `letter-spacing`.
    if style.letter_spacing_pt != 0.0 {
        attrs.push_str(&format!(
            " spc=\"{}\"",
            (style.letter_spacing_pt * 100.0).round() as i64
        ));
    }
    // NOTE: `CharStyle.hidden` has no DrawingML counterpart (there is no
    // per-run vanish in `CT_TextCharacterProperties`); the run stays visible
    // rather than being silently omitted — visible text beats lost text.
    if style.bold {
        attrs.push_str(" b=\"1\"");
    }
    if style.italic {
        attrs.push_str(" i=\"1\"");
    }
    if style.underline {
        attrs.push_str(" u=\"sng\"");
    }
    // A double strike (`strike="dblStrike"`) supersedes the single one — the
    // importer sets `strike` alongside `dstrike` only as a degraded fallback.
    if style.dstrike {
        attrs.push_str(" strike=\"dblStrike\"");
    } else if style.strike {
        attrs.push_str(" strike=\"sngStrike\"");
    }
    // All-/small-capitals → `cap="all"`/`cap="small"` (the inverse of the
    // importer's `a:rPr@cap` capture).
    if style.caps {
        attrs.push_str(" cap=\"all\"");
    } else if style.small_caps {
        attrs.push_str(" cap=\"small\"");
    }
    // Minimum kerning size → `kern` (hundredths of a point).
    if let Some(v) = style.kern_min_pt.filter(|v| *v > 0.0) {
        attrs.push_str(&format!(" kern=\"{}\"", (v * 100.0).round() as i64));
    }
    // Run superscript/subscript → `a:rPr@baseline` (ECMA-376 §21.1.2.3.9), the
    // PPTX analogue of DOCX `w:vertAlign` and ODF `style:text-position`. The value
    // is a percentage of the line in 1000ths; PowerPoint's defaults are +30% for
    // superscript and -25% for subscript, so 30000 / -25000 mirror the importer's
    // `baseline > 0` / `< 0` → `VAlign::Super` / `Sub`. A point-precise
    // `baseline_shift_pt` (only meaningful on an otherwise-baseline run)
    // converts to the same percent-of-font-size scale.
    match style.vertical_align {
        VAlign::Super => attrs.push_str(" baseline=\"30000\""),
        VAlign::Sub => attrs.push_str(" baseline=\"-25000\""),
        VAlign::Baseline => {
            if let Some(shift) = style.baseline_shift_pt.filter(|v| *v != 0.0) {
                let size = run_size(style);
                if size > 0.0 {
                    attrs.push_str(&format!(
                        " baseline=\"{}\"",
                        (shift / size * 100_000.0).round() as i64
                    ));
                }
            }
        }
    }
    let mut inner = String::new();
    if let Some(c) = visible_color(style) {
        inner.push_str(&format!(
            "<a:solidFill><a:srgbClr val=\"{c}\"/></a:solidFill>"
        ));
    }
    // Run highlight / background → `a:highlight` (ECMA-376 §21.1.2.3.9), the
    // PPTX analogue of DOCX `w:shd@fill` and ODF `fo:background-color`. In the
    // `CT_TextCharacterProperties` child order, `a:highlight` follows the fill
    // group (`a:solidFill`) and precedes `a:latin`, so it is emitted here. Like
    // the text fill above, any explicitly-set colour is kept; `None` ⇒ nothing,
    // leaving plain runs unchanged.
    if let Some(bg) = style.background {
        inner.push_str(&format!(
            "<a:highlight><a:srgbClr val=\"{}\"/></a:highlight>",
            hex(bg)
        ));
    }
    if !style.family.is_empty() {
        let mut fam = String::new();
        esc(&style.family, &mut fam);
        inner.push_str(&format!("<a:latin typeface=\"{fam}\"/>"));
    }
    // `a:hlinkClick` (the run's hyperlink) follows the `a:latin` group in
    // `CT_TextCharacterProperties`'s child order; the `r:id` resolves to the
    // slide-rels relationship registered for the link. An internal page jump adds
    // `action="ppaction://hlinksldjump"` so PowerPoint navigates to the slide
    // rather than opening it as an external target.
    if let Some(link) = link {
        if link.slide_jump {
            inner.push_str(&format!(
                "<a:hlinkClick r:id=\"rId{}\" action=\"ppaction://hlinksldjump\"/>",
                link.rid
            ));
        } else {
            inner.push_str(&format!("<a:hlinkClick r:id=\"rId{}\"/>", link.rid));
        }
    }
    if inner.is_empty() {
        format!("<a:rPr {attrs}/>")
    } else {
        format!("<a:rPr {attrs}>{inner}</a:rPr>")
    }
}

fn pptx_model_content_types(
    slide_count: usize,
    image_exts: &[&str],
    notes_slides: &[usize],
) -> String {
    let png = ooxml_image_defaults(image_exts);
    let mut s = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
<Default Extension=\"xml\" ContentType=\"application/xml\"/>{png}\
<Override PartName=\"/ppt/presentation.xml\" \
ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml\"/>\
<Override PartName=\"/ppt/slideMasters/slideMaster1.xml\" \
ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml\"/>\
<Override PartName=\"/ppt/slideLayouts/slideLayout1.xml\" \
ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml\"/>\
<Override PartName=\"/ppt/theme/theme1.xml\" \
ContentType=\"application/vnd.openxmlformats-officedocument.theme+xml\"/>"
    );
    for i in 0..slide_count {
        s.push_str(&format!(
            "<Override PartName=\"/ppt/slides/slide{}.xml\" \
ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slide+xml\"/>",
            i + 1
        ));
    }
    for &n in notes_slides {
        s.push_str(&format!(
            "<Override PartName=\"/ppt/notesSlides/notesSlide{n}.xml\" \
ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.notesSlide+xml\"/>"
        ));
    }
    s.push_str(OOXML_DOCPROPS_OVERRIDES);
    s.push_str("</Types>");
    s
}

fn pptx_model_presentation(slide_count: usize, sw: f64, sh: f64) -> String {
    let mut ids = String::new();
    for i in 0..slide_count {
        ids.push_str(&format!(
            "<p:sldId id=\"{}\" r:id=\"rId{}\"/>",
            256 + i,
            2 + i
        ));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<p:presentation xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" \
xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\">\
<p:sldMasterIdLst><p:sldMasterId id=\"2147483648\" r:id=\"rId1\"/></p:sldMasterIdLst>\
<p:sldIdLst>{ids}</p:sldIdLst>\
<p:sldSz cx=\"{cx}\" cy=\"{cy}\"/><p:notesSz cx=\"6858000\" cy=\"9144000\"/></p:presentation>",
        cx = emu(sw),
        cy = emu(sh),
    )
}

fn pptx_model_presentation_rels(slide_count: usize) -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster\" \
Target=\"slideMasters/slideMaster1.xml\"/>",
    );
    for i in 0..slide_count {
        s.push_str(&format!(
            "<Relationship Id=\"rId{}\" \
Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide\" \
Target=\"slides/slide{}.xml\"/>",
            2 + i,
            i + 1
        ));
    }
    s.push_str("</Relationships>");
    s
}

/// Per-slide relationships, in allocation order: one relationship per entry the
/// slide accumulated — an `image` (embedded picture) or an `hyperlink`
/// (`TargetMode="External"`) — followed by the mandatory `slideLayout`
/// (ECMA-376 §13.3.8 — every slide MUST reference exactly one layout) and, when
/// the slide has speaker notes, a `notesSlide`. The accumulated rIds are
/// `rId1..rIdN` (matching the `r:embed`/`r:id` written into the slide body by
/// [`pptx_slide_from_model`]); the layout rId is `rId{N+1}` and the optional
/// notesSlide rId is `rId{N+2}`. The slide↔layout/notes links resolve by
/// relationship *type*, not by any `r:id` in the slide XML, so the numeric ids
/// are free to be last. `slide_number` is the owning slide's 1-based index.
fn pptx_model_slide_rels(
    rels: &PptxSlideRels,
    media_exts: &[&str],
    has_notes: bool,
    slide_number: usize,
) -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">",
    );
    for (i, rel) in rels.rels.iter().enumerate() {
        let rid = i + 1;
        match rel {
            SlideRel::Image(global) => {
                // The target's extension is the global image's real format.
                let ext = media_exts.get(*global).copied().unwrap_or("png");
                s.push_str(&format!(
                    "<Relationship Id=\"rId{rid}\" \
Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/image\" \
Target=\"../media/image{}.{ext}\"/>",
                    global + 1
                ));
            }
            SlideRel::Hyperlink(url) => {
                let mut u = String::new();
                esc(url, &mut u);
                s.push_str(&format!(
                    "<Relationship Id=\"rId{rid}\" \
Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink\" \
Target=\"{u}\" TargetMode=\"External\"/>"
                ));
            }
            SlideRel::SlideJump(target) => {
                // An internal slide reference (no `TargetMode` ⇒ Internal). Slides
                // are siblings in `ppt/slides/`, so the target is a bare
                // `slide{N}.xml`. Paired with `a:hlinkClick action="…hlinksldjump"`.
                s.push_str(&format!(
                    "<Relationship Id=\"rId{rid}\" \
Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide\" \
Target=\"slide{}.xml\"/>",
                    target + 1
                ));
            }
        }
    }
    let next = rels.rels.len() + 1;
    s.push_str(&format!(
        "<Relationship Id=\"rId{next}\" \
Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout\" \
Target=\"../slideLayouts/slideLayout1.xml\"/>"
    ));
    if has_notes {
        s.push_str(&format!(
            "<Relationship Id=\"rId{}\" \
Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesSlide\" \
Target=\"../notesSlides/notesSlide{slide_number}.xml\"/>",
            rels.rels.len() + 2
        ));
    }
    s.push_str("</Relationships>");
    s
}

// ════════════════════════════════════ ODF ════════════════════════════════════

/// The ODT page-master assignment for a document's sections (#2).
///
/// `section_master[i]` is the index, into a de-duplicated master-page list, of
/// section `i`'s page master — sections sharing identical geometry + running
/// header/footer collapse onto one master. `section_switch[i]` is `Some(style)`
/// when section `i` must begin on a *new* page master (its master differs from
/// the previous section's): the body prepends an empty paragraph using that
/// `style:master-page-name`-bearing style to effect the ODF page change. Section
/// 0 is always the canonical `Standard` master and never switches.
struct OdtSectionPlan {
    /// Per-section index into the unique master list (`0` = `Standard`).
    section_master: Vec<usize>,
    /// Per-section master-switch paragraph style name, or `None` (no switch).
    section_switch: Vec<Option<String>>,
    /// Number of distinct masters (`>= 1`; `1` ⇒ classic single-master document).
    master_count: usize,
}

/// The switch paragraph-style name for master index `m` (`m >= 1`; index 0 is
/// `Standard`, which never switches). Lives as an automatic paragraph style in
/// content.xml and carries `style:master-page-name="Master{m+1}"`.
fn odt_switch_style_name(m: usize) -> String {
    format!("SectMP{}", m + 1)
}

/// The `<style:master-page>` / `<style:page-layout>` names for master index `m`.
/// Index 0 keeps the historical `Standard`/`pm1` so a single-section document is
/// byte-identical to before; later masters are `Master{m+1}`/`pm{m+1}`.
fn odt_master_names(m: usize) -> (String, String) {
    if m == 0 {
        ("Standard".to_string(), "pm1".to_string())
    } else {
        (format!("Master{}", m + 1), format!("pm{}", m + 1))
    }
}

/// Decide each section's page master purely from its geometry + running
/// header/footer (no rendering). Adjacent sections that resolve to a *different*
/// master get a switch paragraph; identical-geometry/header/footer sections share
/// a master and flow on (no spurious page break).
fn odt_section_plan(doc: &Document) -> OdtSectionPlan {
    // De-duplicate masters by (geometry, header, footer). `Section` is `PartialEq`
    // so the triple comparison is exact; we keep one representative per unique key.
    type MasterKey<'a> = (&'a PageGeometry, &'a Option<Vec<Block>>, &'a Option<Vec<Block>>);
    let mut keys: Vec<MasterKey> = Vec::new();
    let mut section_master = Vec::with_capacity(doc.sections.len());
    for s in &doc.sections {
        let key = (&s.geometry, &s.header, &s.footer);
        let idx = keys.iter().position(|k| *k == key).unwrap_or_else(|| {
            keys.push(key);
            keys.len() - 1
        });
        section_master.push(idx);
    }
    let master_count = keys.len().max(1);

    let mut section_switch = Vec::with_capacity(section_master.len());
    for (i, &m) in section_master.iter().enumerate() {
        // Switch only when this section starts a different master than the one
        // before it. Section 0 (and any section continuing the same master) does
        // not — `Standard` (master 0) is the document's initial page master.
        let switches = i > 0 && m != section_master[i - 1];
        section_switch.push(switches.then(|| odt_switch_style_name(m)));
    }

    OdtSectionPlan {
        section_master,
        section_switch,
        master_count,
    }
}

/// Render the per-section page masters into styles.xml and the master-switch
/// paragraph styles into `ctx.auto` (content.xml). Returns
/// `(page_layouts, master_styles)`: the `<style:page-layout>` block for
/// styles.xml's `office:automatic-styles` and the `<style:master-page>` block
/// for its `office:master-styles`. Header/footer markup and any header/footer
/// images/styles are rendered into `hf_ctx`.
///
/// One master is emitted per unique geometry/header/footer; the first is the
/// canonical `Standard`/`pm1`. A switch style `SectMP{n}` (automatic paragraph
/// style) carries `style:master-page-name` so the body's empty switch paragraph
/// changes page master at the section boundary.
///
/// **Best-effort note:** ODF expresses a page change only via this
/// master-page-name mechanism, so section *content* keeps flowing — there is no
/// per-section column count, line-numbering restart, or "continuous" (no
/// page-break) section type as in OOXML; those section features cannot map and
/// are not represented.
fn odt_section_styles(
    doc: &Document,
    plan: &OdtSectionPlan,
    ctx: &mut OdfCtx,
    hf_ctx: &mut OdfCtx,
) -> (String, String) {
    let mut page_layouts = String::new();
    let mut master_styles = String::new();

    // The representative section for each master index (first section using it).
    for m in 0..plan.master_count {
        let section = plan
            .section_master
            .iter()
            .position(|&idx| idx == m)
            .and_then(|si| doc.sections.get(si));
        let geom = section.map(|s| s.geometry.clone()).unwrap_or_default();
        let (master_name, layout_name) = odt_master_names(m);

        page_layouts.push_str(&odf_page_layout(&layout_name, geom));

        // Running header/footer for this master, plus the first-page and even-page
        // variants (rendered into hf_ctx so their styles/images land in
        // styles.xml). ODF maps: default → `<style:header>`/`<style:footer>`
        // (also the odd/right page when an even variant is present), even →
        // `<style:header-left>`/`<style:footer-left>`, first → ODF 1.3
        // `<style:header-first>`/`<style:footer-first>`.
        let render = |b: Option<&Vec<Block>>, hf_ctx: &mut OdfCtx| b.map(|v| odt_blocks(v, hf_ctx));
        let header_xml = render(section.and_then(|s| s.header.as_ref()), hf_ctx);
        let header_even_xml = render(section.and_then(|s| s.header_even.as_ref()), hf_ctx);
        let header_first_xml = render(section.and_then(|s| s.header_first.as_ref()), hf_ctx);
        let footer_xml = render(section.and_then(|s| s.footer.as_ref()), hf_ctx);
        let footer_even_xml = render(section.and_then(|s| s.footer_even.as_ref()), hf_ctx);
        let footer_first_xml = render(section.and_then(|s| s.footer_first.as_ref()), hf_ctx);

        // ODF `<style:master-page>` child order: header, header-left,
        // header-first, then footer, footer-left, footer-first. When only a
        // variant (even/first) is present, an empty `<style:header/>` default is
        // emitted so the variant is honoured (ODF applies `-left`/`-first` as
        // overrides of the default).
        let mut hf = String::new();
        odt_push_hf(
            &mut hf,
            "header",
            &header_xml,
            &header_even_xml,
            &header_first_xml,
        );
        odt_push_hf(
            &mut hf,
            "footer",
            &footer_xml,
            &footer_even_xml,
            &footer_first_xml,
        );
        master_styles.push_str(&format!(
            "<style:master-page style:name=\"{master_name}\" style:page-layout-name=\"{layout_name}\">{hf}</style:master-page>"
        ));

        // A master-switch paragraph style for every master *after* the first.
        if m > 0 {
            let switch = odt_switch_style_name(m);
            ctx.auto.push_str(&format!(
                "<style:style style:name=\"{switch}\" style:family=\"paragraph\" \
style:master-page-name=\"{master_name}\"/>"
            ));
        }
    }

    (page_layouts, master_styles)
}

/// Append one header-or-footer group to a `<style:master-page>` body: the
/// `default` (`<style:{tag}>`, the odd/right page), the `even` variant
/// (`<style:{tag}-left>`, ODF's even/left page), and the `first` variant (ODF
/// 1.3 `<style:{tag}-first>`). When only a variant is present, an empty default
/// `<style:{tag}/>` is emitted first so ODF honours the `-left`/`-first`
/// override. Nothing is written when all three are absent (byte-identical to a
/// header-less master).
fn odt_push_hf(
    out: &mut String,
    tag: &str,
    default: &Option<String>,
    even: &Option<String>,
    first: &Option<String>,
) {
    if default.is_none() && even.is_none() && first.is_none() {
        return;
    }
    match default {
        Some(d) => out.push_str(&format!("<style:{tag}>{d}</style:{tag}>")),
        // A variant needs a default present for ODF to treat it as an override.
        None => out.push_str(&format!("<style:{tag}/>")),
    }
    if let Some(e) = even {
        out.push_str(&format!("<style:{tag}-left>{e}</style:{tag}-left>"));
    }
    if let Some(f) = first {
        out.push_str(&format!("<style:{tag}-first>{f}</style:{tag}-first>"));
    }
}

/// One `<style:page-layout>` (ODF page geometry + margins) for a master page.
/// Zero margin edges are omitted so a default geometry stays clean.
fn odf_page_layout(name: &str, geom: PageGeometry) -> String {
    let orient = if geom.height >= geom.width {
        "portrait"
    } else {
        "landscape"
    };
    let m = geom.margins;
    let mut margin_attrs = String::new();
    if m.top > 0.0 {
        margin_attrs.push_str(&format!(" fo:margin-top=\"{}pt\"", num(m.top)));
    }
    if m.bottom > 0.0 {
        margin_attrs.push_str(&format!(" fo:margin-bottom=\"{}pt\"", num(m.bottom)));
    }
    if m.left > 0.0 {
        margin_attrs.push_str(&format!(" fo:margin-left=\"{}pt\"", num(m.left)));
    }
    if m.right > 0.0 {
        margin_attrs.push_str(&format!(" fo:margin-right=\"{}pt\"", num(m.right)));
    }
    format!(
        "<style:page-layout style:name=\"{name}\">\
<style:page-layout-properties fo:page-width=\"{w}pt\" fo:page-height=\"{h}pt\" \
style:print-orientation=\"{orient}\"{margin_attrs}/></style:page-layout>",
        w = num(geom.width),
        h = num(geom.height),
    )
}

/// Serialize a [`Document`] to a **flowing** OpenDocument Text (`.odt`): real
/// `<text:h>`/`<text:p>`, `<text:list>` for lists, `<table:table>` for tables.
pub fn odt_from_model(doc: &Document) -> Vec<u8> {
    let mut zip = ZipWriter::new();
    zip.add_stored("mimetype", b"application/vnd.oasis.opendocument.text");

    let mut ctx = OdfCtx::new(&doc.resources, &doc.comments).with_footnotes(&doc.footnotes);

    // Per-section page setup (#2): ODF has no `w:sectPr`; a page (geometry +
    // running header/footer) change is driven by a paragraph style carrying
    // `style:master-page-name`. `odt_section_plan` decides — purely from the
    // section geometry/header/footer — which sections start a *new* page master
    // (and so prepend a master-switch paragraph in the body); section 0 is the
    // canonical `Standard`/`pm1`, kept for round-trip stability.
    let plan = odt_section_plan(doc);

    // Body first so body image indices are fixed before the header/footer images
    // continue from them (the original numbering: body images, then h/f images).
    let body = odt_body(doc, &plan.section_switch, &mut ctx);

    // Now render the master pages: their header/footer markup + styles/images go
    // into `hf_ctx` (styles.xml), and the master-switch paragraph styles into
    // `ctx.auto` (content.xml). Header/footer pictures continue the body's
    // count, and header/footer automatic styles carry the `HF` name prefix so
    // they can never collide with content.xml's (the importer merges both
    // parts' style maps, content.xml winning — a colliding name lost the
    // header run's real formatting, e.g. its colour).
    let mut hf_ctx = OdfCtx::new(&doc.resources, &doc.comments);
    hf_ctx.prefix = "HF";
    hf_ctx.image_base = ctx.images.len();
    let (page_layouts, master_styles) = odt_section_styles(doc, &plan, &mut ctx, &mut hf_ctx);

    zip.add_deflated(
        "content.xml",
        odt_model_content(&ctx.auto, &body).as_bytes(),
    );
    zip.add_deflated(
        "styles.xml",
        odf_text_styles_xml(
            &hf_ctx.auto,
            &page_layouts,
            &master_styles,
            &odf_text_named_styles(&doc.styles, &doc.meta),
        )
        .as_bytes(),
    );
    zip.add_deflated("meta.xml", odf_meta_xml(&doc.meta).as_bytes());
    // Image parts are numbered in document order: body images first, then the
    // header/footer context's. The manifest declares each in the same order.
    let image_exts: Vec<&str> = ctx
        .images
        .iter()
        .chain(hf_ctx.images.iter())
        .map(|(_, ext)| *ext)
        .collect();
    zip.add_deflated(
        "META-INF/manifest.xml",
        odf_manifest("text", &image_exts, false).as_bytes(),
    );
    for (i, (bytes, ext)) in ctx.images.iter().enumerate() {
        zip.add_deflated(&format!("Pictures/img{}.{ext}", i + 1), bytes);
    }
    for (i, (bytes, ext)) in hf_ctx.images.iter().enumerate() {
        zip.add_deflated(
            &format!("Pictures/img{}.{ext}", ctx.images.len() + i + 1),
            bytes,
        );
    }
    zip.finish()
}

/// Serialize a [`Document`]'s sheets to a **typed** OpenDocument Spreadsheet
/// (`.ods`): numeric cells carry `office:value-type="float"`/`office:value`,
/// text cells `string`, plus number formats, fills and merged ranges.
pub fn ods_from_model(doc: &Document) -> Vec<u8> {
    // Same native-sheets-then-per-page fallback as `xlsx_from_model`, so both
    // spreadsheet targets carry the full document text for any source format.
    let mut sheets = collect_sheets(doc);
    if sheets.is_empty() {
        sheets = sheets_from_pages(doc);
    }
    let mut zip = ZipWriter::new();
    zip.add_stored(
        "mimetype",
        b"application/vnd.oasis.opendocument.spreadsheet",
    );

    let mut styler = OdsStyler::default();
    let mut body = String::new();
    if sheets.is_empty() {
        body.push_str(
            "<table:table table:name=\"Sheet1\"><table:table-row><table:table-cell/></table:table-row></table:table>",
        );
    }
    for sheet in &sheets {
        body.push_str(&ods_sheet_from_model(sheet, &mut styler));
    }

    zip.add_deflated(
        "content.xml",
        ods_model_content(&styler.data_styles, &styler.cell_styles, &body).as_bytes(),
    );
    zip.add_deflated(
        "styles.xml",
        ods_styles_xml(&odf_default_lang_styles(&doc.meta)).as_bytes(),
    );
    zip.add_deflated("meta.xml", odf_meta_xml(&doc.meta).as_bytes());
    // Frozen panes live in settings.xml (ODF stores split/freeze view state
    // there, not in content). Emitted only when a sheet is frozen — otherwise
    // the package stays byte-identical to before.
    let settings = ods_settings_xml(&sheets);
    if let Some(xml) = &settings {
        zip.add_deflated("settings.xml", xml.as_bytes());
    }
    zip.add_deflated(
        "META-INF/manifest.xml",
        odf_manifest("spreadsheet", &[], settings.is_some()).as_bytes(),
    );
    zip.finish()
}

/// The ODF `settings.xml` restoring each sheet's frozen panes (LibreOffice reads
/// the freeze from view settings, not from content.xml). `None` when no sheet is
/// frozen, so a freeze-less workbook emits no settings part at all.
///
/// Each frozen `Sheet.freeze = (rows, cols)` becomes a
/// `config:config-item-map-entry` (keyed by the sheet name) carrying
/// `HorizontalSplitMode`/`VerticalSplitMode = 2` (frozen) with
/// `HorizontalSplitPosition = cols` / `VerticalSplitPosition = rows`, plus the
/// matching `PositionRight`/`PositionBottom` (the first scrolling cell). The
/// importer's `ods_frozen_panes` reads it back.
fn ods_settings_xml(sheets: &[Sheet]) -> Option<String> {
    let frozen: Vec<&Sheet> = sheets
        .iter()
        .filter(|s| matches!(s.freeze, Some((r, c)) if r > 0 || c > 0))
        .collect();
    if frozen.is_empty() {
        return None;
    }
    let mut tables = String::new();
    for sheet in frozen {
        let (rows, cols) = sheet.freeze.unwrap();
        let h_mode = if cols > 0 { 2 } else { 0 };
        let v_mode = if rows > 0 { 2 } else { 0 };
        let mut name = String::new();
        esc(&sheet.name, &mut name);
        tables.push_str(&format!(
            "<config:config-item-map-entry config:name=\"{name}\">\
<config:config-item config:name=\"HorizontalSplitMode\" config:type=\"short\">{h_mode}</config:config-item>\
<config:config-item config:name=\"VerticalSplitMode\" config:type=\"short\">{v_mode}</config:config-item>\
<config:config-item config:name=\"HorizontalSplitPosition\" config:type=\"int\">{cols}</config:config-item>\
<config:config-item config:name=\"VerticalSplitPosition\" config:type=\"int\">{rows}</config:config-item>\
<config:config-item config:name=\"PositionRight\" config:type=\"int\">{cols}</config:config-item>\
<config:config-item config:name=\"PositionBottom\" config:type=\"int\">{rows}</config:config-item>\
</config:config-item-map-entry>",
        ));
    }
    Some(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<office:document-settings \
xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" \
xmlns:config=\"urn:oasis:names:tc:opendocument:xmlns:config:1.0\" office:version=\"1.3\">\
<office:settings>\
<config:config-item-set config:name=\"ooo:view-settings\">\
<config:config-item-map-indexed config:name=\"Views\">\
<config:config-item-map-entry>\
<config:config-item-map-named config:name=\"Tables\">{tables}</config:config-item-map-named>\
</config:config-item-map-entry>\
</config:config-item-map-indexed>\
</config:config-item-set>\
</office:settings>\
</office:document-settings>"
    ))
}

/// ODS `styles.xml`: an otherwise-empty `office:document-styles`, optionally
/// carrying the document default-language `style:default-style` (ISO 26300
/// §16.2). The `style:` namespace is declared only when the block is present.
fn ods_styles_xml(default_styles: &str) -> String {
    let style_ns = if default_styles.is_empty() {
        ""
    } else {
        " xmlns:style=\"urn:oasis:names:tc:opendocument:xmlns:style:1.0\" \
xmlns:fo=\"urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0\""
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<office:document-styles xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\"{style_ns} \
office:version=\"1.3\">{default_styles}</office:document-styles>"
    )
}

/// Serialize a [`Document`]'s slides to an OpenDocument Presentation (`.odp`):
/// one `<draw:page>` per slide, each placeholder/shape a positioned frame.
pub fn odp_from_model(doc: &Document) -> Vec<u8> {
    // Same single-page-layout constraint as PPTX's `p:sldSz`: keep the
    // majority slide size and scale deviant slides onto it.
    let mut slides = collect_slides(doc);
    let (pw, ph) = normalize_slide_sizes(&mut slides);
    let mut zip = ZipWriter::new();
    zip.add_stored(
        "mimetype",
        b"application/vnd.oasis.opendocument.presentation",
    );

    let mut ctx = OdfCtx::new(&doc.resources, &doc.comments);
    let body = odp_body(&slides, doc, &mut ctx);

    zip.add_deflated(
        "content.xml",
        odp_model_content(&ctx.auto, &body).as_bytes(),
    );
    let styles_block = odf_styles_with_gradients(
        odf_default_lang_styles(&doc.meta),
        &odp_gradient_defs(&ctx.gradients),
    );
    zip.add_deflated(
        "styles.xml",
        odp_styles_xml_model(pw, ph, &styles_block).as_bytes(),
    );
    zip.add_deflated("meta.xml", odf_meta_xml(&doc.meta).as_bytes());
    let image_exts: Vec<&str> = ctx.images.iter().map(|(_, ext)| *ext).collect();
    zip.add_deflated(
        "META-INF/manifest.xml",
        odf_manifest("presentation", &image_exts, false).as_bytes(),
    );
    for (i, (bytes, ext)) in ctx.images.iter().enumerate() {
        zip.add_deflated(&format!("Pictures/img{}.{ext}", i + 1), bytes);
    }
    zip.finish()
}

/// Shared mutable state for ODT/ODP model builds: auto-styles, image list, and
/// the document's resource table for image-blob lookups.
struct OdfCtx<'a> {
    auto: String,
    /// Each image: raw bytes plus its package-native extension (`"png"`,
    /// `"jpeg"`, …), so the `Pictures/imgN.<ext>` part, the `draw:image`
    /// `xlink:href` and the `manifest.xml` media-type carry the real format.
    images: Vec<(Vec<u8>, &'static str)>,
    /// Offset added to this context's local image indices when forming
    /// `Pictures/imgN.<ext>` names, so a secondary context (e.g. header/footer)
    /// does not collide with the body's images. Default `0`.
    image_base: usize,
    style_id: usize,
    /// Prefix for every automatic-style name this context mints (`""` for the
    /// body). The header/footer context uses `"HF"`: its styles land in
    /// **styles.xml** while the body's land in content.xml, and the importer
    /// merges both parts' automatic styles into one name-keyed map with
    /// content.xml winning — a header span named `T0` was silently resolved
    /// against the body's unrelated `T0`, losing e.g. the header run's colour.
    prefix: &'static str,
    /// The document's review comments, so an [`Inline::CommentRef`] can re-emit
    /// its `office:annotation` at the anchor point.
    comments: &'a [crate::model::Comment],
    /// The document's foot-/endnotes, so an [`Inline::FootnoteRef`] can re-emit
    /// its `<text:note>` at the anchor point (see [`OdfCtx::with_footnotes`];
    /// empty for contexts that never see note anchors).
    footnotes: &'a [crate::model::Footnote],
    resources: &'a crate::model::ResourceTable,
    /// Linear gradients referenced by graphic / drawing-page styles minted by
    /// this context. Each becomes a named `<draw:gradient>` definition in the
    /// document's `<office:styles>` (`Gradient1`…, deduplicated by value).
    gradients: Vec<crate::model::Gradient>,
}

impl<'a> OdfCtx<'a> {
    fn new(
        resources: &'a crate::model::ResourceTable,
        comments: &'a [crate::model::Comment],
    ) -> Self {
        OdfCtx {
            auto: String::new(),
            images: Vec::new(),
            image_base: 0,
            style_id: 0,
            prefix: "",
            comments,
            footnotes: &[],
            resources,
            gradients: Vec::new(),
        }
    }

    /// Attach the document's foot-/endnotes so an [`Inline::FootnoteRef`] can
    /// re-emit its `<text:note>` at the anchor point.
    fn with_footnotes(mut self, footnotes: &'a [crate::model::Footnote]) -> Self {
        self.footnotes = footnotes;
        self
    }
    fn next_style(&mut self) -> usize {
        let id = self.style_id;
        self.style_id += 1;
        id
    }

    /// Register a gradient (deduplicated by value) and return the
    /// `<draw:gradient draw:name>` it will be defined under.
    fn gradient_name(&mut self, g: &crate::model::Gradient) -> String {
        let idx = self
            .gradients
            .iter()
            .position(|x| x == g)
            .unwrap_or_else(|| {
                self.gradients.push(g.clone());
                self.gradients.len() - 1
            });
        format!("Gradient{}", idx + 1)
    }
    /// Resolve an image blob by resource key, returning its bytes and the
    /// package-native extension derived from the resource's format tag.
    fn resolve_image(&self, key: u64) -> Option<(Vec<u8>, &'static str)> {
        self.resources
            .images
            .get(&key)
            .map(|r| (r.bytes.clone(), office_image_format(&r.format).1))
    }
}

fn odt_body(doc: &Document, section_switch: &[Option<String>], ctx: &mut OdfCtx) -> String {
    // Each section's running header/footer become real `<style:header>`/
    // `<style:footer>` in its master page (styles.xml), not inlined body text.
    let mut body = String::new();
    // A `text:bookmark` named `page{N}` at each page boundary, the jump target for
    // an internal `LinkTarget::Page` (`text:a xlink:href="#page{N}"`). ODF requires
    // a bookmark to live inside a paragraph, so it rides an empty `text:p` (as the
    // block-image/shape export wraps its drawing).
    let mut page_no = 0usize;
    for (si, section) in doc.sections.iter().enumerate() {
        // Per-section page setup (#2): a section that begins a new page master
        // carries a `style:master-page-name` switch style on its first paragraph,
        // which forces the ODF page change + new geometry/header/footer. The
        // switch rides the section's first page-bookmark paragraph (no extra
        // empty paragraph). Section 0 (and same-master sections) carries none.
        let switch_attr = section_switch
            .get(si)
            .and_then(|s| s.as_deref())
            .map(|name| format!(" text:style-name=\"{name}\""))
            .unwrap_or_default();
        for (pi, page) in section.pages.iter().enumerate() {
            page_no += 1;
            let attr = if pi == 0 { switch_attr.as_str() } else { "" };
            body.push_str(&format!(
                "<text:p{attr}><text:bookmark text:name=\"page{page_no}\"/></text:p>"
            ));
            body.push_str(&odt_blocks(&page.blocks, ctx));
        }
    }
    body
}

fn odt_blocks(blocks: &[Block], ctx: &mut OdfCtx) -> String {
    let mut out = String::new();
    for b in blocks {
        odt_block(b, ctx, &mut out);
    }
    out
}

fn odt_block(block: &Block, ctx: &mut OdfCtx, out: &mut String) {
    match &block.kind {
        BlockKind::Paragraph(p) => out.push_str(&odt_paragraph(p, "p", None, ctx)),
        BlockKind::Heading(h) => {
            let level = h.level.clamp(1, 6);
            let attr = format!(" text:outline-level=\"{level}\"");
            out.push_str(&odt_paragraph(&h.para, "h", Some(&attr), ctx));
        }
        BlockKind::List(list) => out.push_str(&odt_list(list, ctx)),
        BlockKind::Table(table) => out.push_str(&odt_table(table, ctx)),
        BlockKind::Image(img) => out.push_str(&odt_image_para(img, ctx)),
        BlockKind::Shape(shape) => out.push_str(&odt_shape_para(block, shape, ctx)),
        BlockKind::TextBox(tb) => out.push_str(&odt_blocks(&tb.blocks, ctx)),
        BlockKind::CodeBlock(cb) => out.push_str(&odt_code(cb, ctx)),
        BlockKind::Blockquote(bq) => {
            let indented: Vec<Block> = bq
                .blocks
                .iter()
                .map(|b| indent_block_left(b, 24.0))
                .collect();
            out.push_str(&odt_blocks(&indented, ctx));
        }
        BlockKind::HorizontalRule => out.push_str(&odt_hr(ctx)),
        BlockKind::Sheet(sheet) => {
            for s in &sheet.sheets {
                out.push_str(&odt_table(&sheet_to_table(s), ctx));
            }
        }
        BlockKind::Slide(slides) => {
            for slide in &slides.slides {
                for ph in &slide.placeholders {
                    odt_block(&ph.block, ctx, out);
                }
            }
        }
    }
}

/// A code block → a preformatted, monospaced ODF paragraph (grey background + a
/// thin box border), with source lines separated by `<text:line-break/>` so the
/// verbatim layout survives.
fn odt_code(cb: &CodeBlock, ctx: &mut OdfCtx) -> String {
    let pid = ctx.next_style();
    let pname = format!("{}Code{pid}", ctx.prefix);
    ctx.auto.push_str(&format!(
        "<style:style style:name=\"{pname}\" style:family=\"paragraph\">\
<style:paragraph-properties fo:background-color=\"#f2f2f2\" \
fo:border=\"0.5pt solid #cccccc\" fo:padding=\"3pt\"/>\
<style:text-properties style:font-name=\"Courier New\" fo:font-family=\"'Courier New'\" \
style:font-family-generic=\"modern\" fo:font-size=\"10pt\"/></style:style>"
    ));
    let sid = ctx.next_style();
    let sname = format!("{}CodeT{sid}", ctx.prefix);
    ctx.auto.push_str(&format!(
        "<style:style style:name=\"{sname}\" style:family=\"text\">\
<style:text-properties style:font-name=\"Courier New\" fo:font-family=\"'Courier New'\" \
style:font-family-generic=\"modern\" fo:font-size=\"10pt\"/></style:style>"
    ));
    let mut runs = String::new();
    for (i, line) in cb.code.split('\n').enumerate() {
        if i > 0 {
            runs.push_str("<text:line-break/>");
        }
        runs.push_str(&format!("<text:span text:style-name=\"{sname}\">"));
        esc(line, &mut runs);
        runs.push_str("</text:span>");
    }
    format!("<text:p text:style-name=\"{pname}\">{runs}</text:p>")
}

/// A horizontal rule → an empty paragraph carrying only a bottom border.
fn odt_hr(ctx: &mut OdfCtx) -> String {
    let pid = ctx.next_style();
    let pname = format!("{}Hr{pid}", ctx.prefix);
    ctx.auto.push_str(&format!(
        "<style:style style:name=\"{pname}\" style:family=\"paragraph\">\
<style:paragraph-properties fo:border-bottom=\"0.5pt solid #999999\" \
fo:margin-top=\"6pt\" fo:margin-bottom=\"6pt\"/></style:style>"
    ));
    format!("<text:p text:style-name=\"{pname}\"/>")
}

/// One paragraph or heading. `tag` is `"p"` or `"h"`; `extra` adds attributes
/// (e.g. the outline level for a heading).
///
/// When the paragraph carries a non-empty `style_ref`, it references the matching
/// named `style:style` (emitted in `styles.xml`'s `office:styles`): if there is
/// no direct paragraph formatting, the named style is referenced *directly*
/// (`text:style-name="…"`); otherwise an automatic style inheriting it
/// (`style:parent-style-name="…"`) carries the overrides. Without a `style_ref`,
/// an anonymous automatic style is used as before.
fn odt_paragraph(para: &Paragraph, tag: &str, extra: Option<&str>, ctx: &mut OdfCtx) -> String {
    let extra = extra.unwrap_or("");
    let mut runs = String::new();
    odt_runs(&para.runs, ctx, &mut runs);

    let style_name = match style_ref_id(para.style_ref.as_ref()) {
        Some(named) => {
            let direct = odf_para_prop_attrs(&para.style);
            let tabs = odf_tab_stops_xml(&para.style);
            if direct.is_empty() && tabs.is_empty() {
                // Reference the named style directly — no automatic style needed.
                let mut n = String::new();
                esc(named, &mut n);
                n
            } else {
                // Automatic style inheriting the named style, carrying overrides
                // (attribute deltas and/or explicit tab stops).
                let pid = ctx.next_style();
                let auto_name = format!("{}P{pid}", ctx.prefix);
                let mut parent = String::new();
                esc(named, &mut parent);
                ctx.auto.push_str(&format!(
                    "<style:style style:name=\"{auto_name}\" style:family=\"paragraph\" \
style:parent-style-name=\"{parent}\">{props}</style:style>",
                    props = odf_paragraph_properties_xml(&direct, &tabs)
                ));
                auto_name
            }
        }
        None => {
            let pid = ctx.next_style();
            let auto_name = format!("{}P{pid}", ctx.prefix);
            ctx.auto.push_str(&odf_para_style(&auto_name, para));
            auto_name
        }
    };
    format!("<text:{tag} text:style-name=\"{style_name}\"{extra}>{runs}</text:{tag}>")
}

/// Inline runs → ODF text spans (each styled run gets an automatic text style).
fn odt_runs(runs: &[Inline], ctx: &mut OdfCtx, out: &mut String) {
    for r in runs {
        match r {
            Inline::Run(run) => {
                if run.text.is_empty() {
                    continue;
                }
                let sid = ctx.next_style();
                let sname = format!("{}T{sid}", ctx.prefix);
                ctx.auto.push_str(&odf_span_style(&sname, &run.style));
                out.push_str(&format!("<text:span text:style-name=\"{sname}\">"));
                for part in run.text.split_inclusive('\t') {
                    let text = part.trim_end_matches('\t');
                    if !text.is_empty() {
                        esc(text, out);
                    }
                    if part.ends_with('\t') {
                        out.push_str("<text:tab/>");
                    }
                }
                out.push_str("</text:span>");
            }
            Inline::LineBreak => out.push_str("<text:line-break/>"),
            // A run-level inline image → a `draw:frame`/`draw:image` anchored
            // as a character inside the paragraph (the same `frInl` graphic style
            // and `Pictures/`+manifest plumbing as the block-level image path).
            Inline::Image(img) => {
                if let Some(frame) = odt_inline_image_frame(img, ctx) {
                    out.push_str(&frame);
                }
            }
            Inline::Link { href, children } => {
                // External URL → `text:a xlink:href="url"`; internal page jump →
                // `text:a xlink:href="#page{N}"`, resolving to the `text:bookmark`
                // (ODT) or `draw:page draw:name` (ODP, the shared `odp_body`) that
                // marks each page target.
                let target = match href {
                    LinkTarget::Url(url) if !url.is_empty() => {
                        let mut u = String::new();
                        esc(url, &mut u);
                        Some(u)
                    }
                    LinkTarget::Page(p) => Some(format!("#page{}", p + 1)),
                    LinkTarget::Url(_) => None,
                    // A named in-document anchor → `xlink:href="#name"`. Note
                    // an unresolved anchor has no known target position in the
                    // model, so no `text:bookmark` is minted for it here — the
                    // fragment link degrades gracefully in readers when the
                    // name resolves to nothing (an anchor that *did* resolve
                    // was already rewritten to `LinkTarget::Page` at import).
                    LinkTarget::Anchor(a) if !a.is_empty() => {
                        let mut n = String::new();
                        esc(a, &mut n);
                        Some(format!("#{n}"))
                    }
                    // A raw action summary has no ODF target: children render
                    // un-linked (same as an empty URL).
                    LinkTarget::Anchor(_) | LinkTarget::Other(_) => None,
                };
                if let Some(href) = target {
                    out.push_str(&format!(
                        "<text:a xlink:type=\"simple\" xlink:href=\"{href}\">"
                    ));
                    odt_runs(children, ctx, out);
                    out.push_str("</text:a>");
                } else {
                    odt_runs(children, ctx, out);
                }
            }
            // A comment anchor: re-emit the full `office:annotation` (ISO 26300
            // §14.1) at the anchor point — `office:name` carries the model id
            // verbatim (the importer's anchor key), `dc:creator`/`dc:date` the
            // author/date, and the flattened text one `text:p` (the importer
            // joins body paragraphs with a space, so one line round-trips
            // byte-identical). A dangling anchor (no matching comment) emits
            // nothing.
            Inline::CommentRef { id } => {
                if let Some(c) = ctx.comments.iter().find(|c| c.id == *id) {
                    let mut name = String::new();
                    esc(&c.id, &mut name);
                    out.push_str(&format!("<office:annotation office:name=\"{name}\">"));
                    if !c.author.is_empty() {
                        out.push_str("<dc:creator>");
                        esc(&c.author, out);
                        out.push_str("</dc:creator>");
                    }
                    if !c.date.is_empty() {
                        out.push_str("<dc:date>");
                        esc(&c.date, out);
                        out.push_str("</dc:date>");
                    }
                    out.push_str("<text:p>");
                    esc(&c.text, out);
                    out.push_str("</text:p></office:annotation>");
                }
            }
            // A footnote anchor: the full `<text:note text:note-class=
            // "footnote">` (ISO 26300 §6.3) — its citation is the note's
            // 1-based ordinal and its body the note's paragraphs, rendered
            // with the shared paragraph writer so styling survives. The ODT
            // importer reads this exact shape back (`text:note` →
            // `Document::footnotes` + an `Inline::FootnoteRef`), so notes
            // round-trip. A dangling anchor emits nothing.
            Inline::FootnoteRef { id } => {
                let Some(pos) = ctx.footnotes.iter().position(|f| f.id == *id) else {
                    continue;
                };
                let ordinal = pos + 1;
                let blocks = ctx.footnotes[pos].blocks.clone();
                let mut note_id = String::new();
                esc(id, &mut note_id);
                let mut body = String::new();
                for para in blocks_to_paras(&blocks) {
                    body.push_str(&odt_paragraph(&para, "p", None, ctx));
                }
                if body.is_empty() {
                    body.push_str("<text:p/>");
                }
                out.push_str(&format!(
                    "<text:note text:id=\"ftn{note_id}\" text:note-class=\"footnote\">\
<text:note-citation>{ordinal}</text:note-citation>\
<text:note-body>{body}</text:note-body></text:note>"
                ));
            }
        }
    }
}

fn odt_list(list: &List, ctx: &mut OdfCtx) -> String {
    let sid = ctx.next_style();
    let sname = format!("{}L{sid}", ctx.prefix);
    ctx.auto.push_str(&odf_list_style(&sname, list));

    // Honour `ListItem.level` (0-based) by nesting `<text:list>`s. ODF nests an
    // inner list *inside* the parent's still-open `<text:list-item>` (ISO 26300
    // §5.3.3 — `text:list` is valid list-item content), e.g. a level-2 item lives
    // in a `text:list` inside the level-1 item's `text:list-item`. The named list
    // style's per-level definitions drive the bullet/number + indent at each
    // depth. We keep each list-item open until we know the next item's depth:
    // `pending_item_close[d]` is true while the list-item that hosts depth `d+1`
    // awaits its `</text:list-item>`.
    let mut out = String::new();
    // For each currently-open `<text:list>` level (index 0 = outermost), whether
    // its current `<text:list-item>` is still open (awaiting close).
    let mut open_item: Vec<bool> = Vec::new();

    // Close the deepest open levels back to (but not below) `target` open lists.
    // Closing a nested list also closes the deepest item if open; the *parent*
    // list-item stays open (it hosts the closed child list and may host more).
    let close_to = |open_item: &mut Vec<bool>, out: &mut String, target: usize| {
        while open_item.len() > target {
            if open_item.pop() == Some(true) {
                out.push_str("</text:list-item>");
            }
            out.push_str("</text:list>");
        }
    };

    for item in &list.items {
        let want = item.level as usize + 1; // open-list depth this item lives at

        if want <= open_item.len() {
            close_to(&mut open_item, &mut out, want);
            // Close the sibling list-item still open at this level, if any, so the
            // new same-level item starts its own `<text:list-item>`.
            if let Some(last) = open_item.last_mut() {
                if *last {
                    out.push_str("</text:list-item>");
                    *last = false;
                }
            }
        } else {
            // Open nested lists down to `want`. The first new list is hosted by
            // the parent level's currently-open list-item; deeper ones get an
            // empty hosting list-item.
            while open_item.len() < want {
                if open_item.is_empty() {
                    out.push_str(&format!("<text:list text:style-name=\"{sname}\">"));
                } else {
                    // Need an open hosting list-item at the parent level.
                    if !*open_item.last().unwrap() {
                        out.push_str("<text:list-item>");
                        *open_item.last_mut().unwrap() = true;
                    }
                    out.push_str("<text:list>");
                }
                open_item.push(false);
            }
        }

        // Emit this item's `<text:list-item>` at the deepest level.
        out.push_str("<text:list-item>");
        *open_item.last_mut().unwrap() = true;
        if item.blocks.is_empty() {
            out.push_str("<text:p/>");
        } else {
            for b in &item.blocks {
                odt_block(b, ctx, &mut out);
            }
        }
    }

    // Close every still-open level.
    close_to(&mut open_item, &mut out, 0);
    out
}

fn odt_table(table: &Table, ctx: &mut OdfCtx) -> String {
    let cols = table_col_count(table).max(1);
    let widths = docx_col_widths(table, cols);
    let tid = ctx.next_style();
    let tname = format!("{}Tbl{tid}", ctx.prefix);
    // Table + per-column styles.
    ctx.auto.push_str(&format!(
        "<style:style style:name=\"{tname}\" style:family=\"table\">\
<style:table-properties style:width=\"{tw}pt\" table:align=\"left\"/></style:style>",
        tw = num(widths.iter().sum::<f64>()),
    ));
    let mut col_defs = String::new();
    let mut col_styles = String::new();
    for (i, w) in widths.iter().enumerate() {
        let cn = format!("{tname}c{i}");
        ctx.auto.push_str(&format!(
            "<style:style style:name=\"{cn}\" style:family=\"table-column\">\
<style:table-column-properties style:column-width=\"{cw}pt\"/></style:style>",
            cw = num(*w),
        ));
        col_defs.push_str(&format!("<table:table-column table:style-name=\"{cn}\"/>"));
        let _ = &mut col_styles;
    }

    // The table's border applies to each cell as an ODF `fo:border` (ODF puts
    // borders on table-cell-properties, not on the table; DOCX's table-level
    // `w:tblBorders` is the equivalent — match its presence/width/colour). Empty
    // when the model carries no border (`width <= 0`).
    let cell_border = if table.border.width > 0.0 {
        format!(
            " fo:border=\"{w}pt solid #{c}\"",
            w = num(table.border.width),
            c = hex(table.border.color),
        )
    } else {
        String::new()
    };

    let mut rows = String::new();
    // Leading contiguous header rows are wrapped in `<table:table-header-rows>`
    // (ODF requires the header rows to lead the table and live in that element).
    // Open it before the first header row, close it at the first body row; a
    // non-leading header row (rare) stays an ordinary row.
    let header_rows = table.rows.iter().take_while(|r| r.is_header).count();
    let mut header_open = false;
    for (ri, row) in table.rows.iter().enumerate() {
        if ri < header_rows && !header_open {
            rows.push_str("<table:table-header-rows>");
            header_open = true;
        } else if ri == header_rows && header_open {
            rows.push_str("</table:table-header-rows>");
            header_open = false;
        }
        // Row height → a row style carrying `style:row-height` (DOCX uses
        // `w:trHeight`; this is the ODF equivalent). `style:min-row-height` keeps
        // it a floor so taller content still fits, matching DOCX's `hRule="atLeast"`.
        let row_style = match row.height {
            Some(h) if h > 0.0 => {
                let rsid = ctx.next_style();
                let rsn = format!("{}Tr{rsid}", ctx.prefix);
                ctx.auto.push_str(&format!(
                    "<style:style style:name=\"{rsn}\" style:family=\"table-row\">\
<style:table-row-properties style:row-height=\"{hh}pt\" style:min-row-height=\"{hh}pt\"/></style:style>",
                    hh = num(h),
                ));
                format!(" table:style-name=\"{rsn}\"")
            }
            _ => String::new(),
        };
        rows.push_str(&format!("<table:table-row{row_style}>"));
        let mut phys = 0usize;
        for cell in &row.cells {
            let span = cell.col_span.max(1) as usize;
            let rspan = cell.row_span.max(1) as usize;
            let mut attrs = String::new();
            if span > 1 {
                attrs.push_str(&format!(" table:number-columns-spanned=\"{span}\""));
            }
            if rspan > 1 {
                attrs.push_str(&format!(" table:number-rows-spanned=\"{rspan}\""));
            }
            // Compose the cell's table-cell-properties from border, shading and/or
            // vertical alignment; emit an auto-style only when at least one is set.
            let mut tc_props = String::new();
            tc_props.push_str(&cell_border);
            if let Some(shade) = cell.shading {
                tc_props.push_str(&format!(" fo:background-color=\"#{}\"", hex(shade)));
            }
            if let Some(va) = cell.vertical_align {
                tc_props.push_str(&format!(
                    " style:vertical-align=\"{}\"",
                    odf_cell_valign_attr(va)
                ));
            }
            let cell_style = if tc_props.is_empty() {
                String::new()
            } else {
                let csid = ctx.next_style();
                let csn = format!("{}Tc{csid}", ctx.prefix);
                ctx.auto.push_str(&format!(
                    "<style:style style:name=\"{csn}\" style:family=\"table-cell\">\
<style:table-cell-properties{tc_props}/></style:style>"
                ));
                format!(" table:style-name=\"{csn}\"")
            };
            let mut inner = odt_blocks(&cell.blocks, ctx);
            if inner.is_empty() {
                inner.push_str("<text:p/>");
            }
            rows.push_str(&format!(
                "<table:table-cell{cell_style}{attrs}>{inner}</table:table-cell>"
            ));
            // Covered cells for a horizontal span.
            for _ in 1..span {
                rows.push_str("<table:covered-table-cell/>");
            }
            phys += span;
        }
        let _ = phys;
        rows.push_str("</table:table-row>");
    }
    // A table that is all header rows leaves the wrapper open: close it.
    if header_open {
        rows.push_str("</table:table-header-rows>");
    }

    format!("<table:table table:style-name=\"{tname}\">{col_defs}{rows}</table:table>")
}

/// Build a `draw:frame`/`draw:image` for an inline image anchored as a character
/// (the `frInl` graphic style), interning the blob into `Pictures/` + the
/// manifest. `None` when the resource is missing/empty (the caller omits it).
/// Shared by the block-level image paragraph and run-level inline images.
fn odt_inline_image_frame(img: &ImageRef, ctx: &mut OdfCtx) -> Option<String> {
    let (bytes, ext) = match ctx.resolve_image(img.resource) {
        Some(b) if !b.0.is_empty() => b,
        _ => return None,
    };
    ctx.images.push((bytes, ext));
    let n = ctx.image_base + ctx.images.len();
    Some(format!(
        "<draw:frame draw:style-name=\"frInl\" text:anchor-type=\"as-char\" \
svg:width=\"96pt\" svg:height=\"96pt\">\
<draw:image xlink:href=\"Pictures/img{n}.{ext}\" xlink:type=\"simple\" xlink:show=\"embed\" \
xlink:actuate=\"onLoad\"/></draw:frame>"
    ))
}

fn odt_image_para(img: &ImageRef, ctx: &mut OdfCtx) -> String {
    match odt_inline_image_frame(img, ctx) {
        Some(frame) => format!("<text:p>{frame}</text:p>"),
        None => "<text:p/>".to_string(),
    }
}

/// A block-level [`Shape`] → a `<text:p>` carrying an anchored `draw:rect` /
/// `draw:path` (ODF §10.3.2 / §10.3.8), the ODT analogue of the DOCX/PPTX/ODP
/// shape export. ODF requires a drawing in the text body to live inside a
/// paragraph (as the inline-image export does), so the shape is wrapped in a
/// `text:p` and `text:anchor-type="paragraph"`. A flowing body has no slot for a
/// shape that carries no placement box, so an unframed shape yields nothing —
/// the previous behaviour. Geometry + fill/stroke come from the same
/// `shape_to_placed` / `odf_shape_style` / `odf_path_d` helpers the ODP/PPTX
/// exporters use, so a shape renders identically across them.
fn odt_shape_para(block: &Block, shape: &Shape, ctx: &mut OdfCtx) -> String {
    let Some(r) = block.frame else {
        return String::new();
    };
    let placed = shape_to_placed(shape);
    let sid = ctx.next_style();
    let sname = format!("{}Sh{sid}", ctx.prefix);
    ctx.auto.push_str(&odf_shape_style(&sname, &placed));
    let draw = if shape_is_rect(&placed) {
        format!(
            "<draw:rect draw:style-name=\"{sname}\" text:anchor-type=\"paragraph\" \
draw:layer=\"layout\" svg:x=\"{x}pt\" svg:y=\"{y}pt\" svg:width=\"{w}pt\" svg:height=\"{h}pt\"/>",
            x = num(r.x),
            y = num(r.y),
            w = num(r.w.max(1.0)),
            h = num(r.h.max(1.0)),
        )
    } else {
        format!(
            "<draw:path draw:style-name=\"{sname}\" text:anchor-type=\"paragraph\" \
draw:layer=\"layout\" svg:x=\"{x}pt\" svg:y=\"{y}pt\" svg:width=\"{w}pt\" svg:height=\"{h}pt\" \
svg:viewBox=\"0 0 {vw} {vh}\" svg:d=\"{d}\"/>",
            x = num(r.x),
            y = num(r.y),
            w = num(r.w.max(1.0)),
            h = num(r.h.max(1.0)),
            vw = num(r.w.max(1.0)),
            vh = num(r.h.max(1.0)),
            d = odf_path_d(&placed.segments),
        )
    };
    format!("<text:p>{draw}</text:p>")
}

fn odt_model_content(auto: &str, body: &str) -> String {
    // The inline-image graphic style lives in automatic-styles.
    let frame_style = "<style:style style:name=\"frInl\" style:family=\"graphic\">\
<style:graphic-properties draw:fill=\"none\" draw:stroke=\"none\" style:vertical-pos=\"middle\" \
style:vertical-rel=\"text\"/></style:style>";
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<office:document-content xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" \
xmlns:style=\"urn:oasis:names:tc:opendocument:xmlns:style:1.0\" \
xmlns:text=\"urn:oasis:names:tc:opendocument:xmlns:text:1.0\" \
xmlns:table=\"urn:oasis:names:tc:opendocument:xmlns:table:1.0\" \
xmlns:draw=\"urn:oasis:names:tc:opendocument:xmlns:drawing:1.0\" \
xmlns:fo=\"urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0\" \
xmlns:svg=\"urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0\" \
xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
xmlns:xlink=\"http://www.w3.org/1999/xlink\" office:version=\"1.3\">\
<office:automatic-styles>{frame_style}{auto}</office:automatic-styles>\
<office:body><office:text>{body}</office:text></office:body></office:document-content>"
    )
}

/// The ODT `styles.xml`: the per-section page layouts + master pages (each
/// carrying its own `<style:header>`/`<style:footer>`), the header/footer
/// automatic styles, and the document's named styles. `page_layouts` /
/// `master_styles` are the `<style:page-layout>` / `<style:master-page>` blocks
/// built by [`odt_section_styles`]; `hf_auto` is the automatic-style markup the
/// header/footer paragraphs reference.
fn odf_text_styles_xml(
    hf_auto: &str,
    page_layouts: &str,
    master_styles: &str,
    default_styles: &str,
) -> String {
    // Header/footer images (rendered via the hf context) reference `frInl`, so
    // it must exist in this document's automatic styles too.
    let frame_style = if hf_auto.contains("frInl") {
        "<style:style style:name=\"frInl\" style:family=\"graphic\">\
<style:graphic-properties draw:fill=\"none\" draw:stroke=\"none\" style:vertical-pos=\"middle\" \
style:vertical-rel=\"text\"/></style:style>"
    } else {
        ""
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<office:document-styles xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" \
xmlns:style=\"urn:oasis:names:tc:opendocument:xmlns:style:1.0\" \
xmlns:text=\"urn:oasis:names:tc:opendocument:xmlns:text:1.0\" \
xmlns:table=\"urn:oasis:names:tc:opendocument:xmlns:table:1.0\" \
xmlns:draw=\"urn:oasis:names:tc:opendocument:xmlns:drawing:1.0\" \
xmlns:svg=\"urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0\" \
xmlns:xlink=\"http://www.w3.org/1999/xlink\" \
xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
xmlns:fo=\"urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0\" office:version=\"1.3\">\
<office:automatic-styles>{frame_style}{hf_auto}{page_layouts}</office:automatic-styles>{default_styles}\
<office:master-styles>{master_styles}</office:master-styles></office:document-styles>"
    )
}

/// The `fo:`/`style:` attributes of a paragraph style's
/// `<style:paragraph-properties>` (alignment, spacing, indents, leading), or an
/// empty string when the style is fully default. Shared by the automatic-style
/// writer, the named-style writer, and the override-detection in `odt_paragraph`.
fn odf_para_prop_attrs(ps: &ParagraphStyle) -> String {
    let mut props = String::new();
    match ps.align {
        Align::Left => {}
        Align::Center => props.push_str(" fo:text-align=\"center\""),
        Align::Right => props.push_str(" fo:text-align=\"end\""),
        Align::Justify => props.push_str(" fo:text-align=\"justify\""),
    }
    if ps.space_before_pt > 0.0 {
        props.push_str(&format!(" fo:margin-top=\"{}pt\"", num(ps.space_before_pt)));
    }
    if ps.space_after_pt > 0.0 {
        props.push_str(&format!(
            " fo:margin-bottom=\"{}pt\"",
            num(ps.space_after_pt)
        ));
    }
    if ps.indent_left_pt != 0.0 {
        props.push_str(&format!(" fo:margin-left=\"{}pt\"", num(ps.indent_left_pt)));
    }
    if ps.indent_right_pt != 0.0 {
        props.push_str(&format!(
            " fo:margin-right=\"{}pt\"",
            num(ps.indent_right_pt)
        ));
    }
    if ps.first_line_pt != 0.0 {
        props.push_str(&format!(" fo:text-indent=\"{}pt\"", num(ps.first_line_pt)));
    }
    if let LineHeight::Multiple(m) = ps.line_height {
        props.push_str(&format!(
            " fo:line-height=\"{}%\"",
            (m * 100.0).round() as i64
        ));
    } else if let LineHeight::Points(p) = ps.line_height {
        props.push_str(&format!(" fo:line-height=\"{}pt\"", num(p)));
    }
    if let Some(bg) = ps.background {
        props.push_str(&format!(" fo:background-color=\"#{}\"", hex(bg)));
    }
    // Per-side paragraph borders: `fo:border-<side>="{w}pt {style} #{color}"`.
    // The model array is ordered `[top, right, bottom, left]`.
    for (side, border) in ["top", "right", "bottom", "left"]
        .iter()
        .zip(ps.borders.iter())
    {
        if let Some(b) = border {
            props.push_str(&format!(
                " fo:border-{side}=\"{}pt {} #{}\"",
                num(b.width_pt),
                odf_border_style(&b.style),
                hex(b.color)
            ));
        }
    }
    if ps.keep_with_next {
        props.push_str(" fo:keep-with-next=\"always\"");
    }
    if ps.keep_together {
        props.push_str(" fo:keep-together=\"always\"");
    }
    // Authored page break before the paragraph (the ODF mirror of the DOCX
    // `w:pageBreakBefore` emission; inverse of the importer's capture).
    if ps.page_break_before {
        props.push_str(" fo:break-before=\"page\"");
    }
    // Right-to-left paragraph flow (`style:writing-mode="rl-tb"`) — the ODF
    // mirror of `w:bidi`; the importer's `odf_writing_mode` reads it back.
    if ps.direction == Some(crate::model::Direction::Rtl) {
        props.push_str(" style:writing-mode=\"rl-tb\"");
    }
    props
}

/// The `<style:tab-stops>` child of `<style:paragraph-properties>`: one
/// `<style:tab-stop>` per model [`crate::model::TabStop`] with its position in
/// points (this file's ODF length convention), its alignment (`style:type`,
/// omitted for the default `left`; `Decimal` maps to ODF's `char` type aligned
/// on `.` — ODF has no `decimal` token), and its leader as
/// `style:leader-style` + `style:leader-text`. Empty when the paragraph has no
/// explicit tab stops — the ODF mirror of the DOCX `<w:tabs>` emission.
fn odf_tab_stops_xml(ps: &ParagraphStyle) -> String {
    if ps.tab_stops.is_empty() {
        return String::new();
    }
    let mut stops = String::new();
    for ts in &ps.tab_stops {
        stops.push_str(&format!(
            "<style:tab-stop style:position=\"{}pt\"",
            num(ts.pos_pt)
        ));
        match ts.align {
            crate::model::TabAlign::Left => {}
            crate::model::TabAlign::Center => stops.push_str(" style:type=\"center\""),
            crate::model::TabAlign::Right => stops.push_str(" style:type=\"right\""),
            crate::model::TabAlign::Decimal => {
                stops.push_str(" style:type=\"char\" style:char=\".\"");
            }
        }
        if ts.leader != '\0' {
            // `.`/`·` read as a dotted rule, `-` as dashes, `_` (and anything
            // else) as a solid line; the literal character rides along in
            // `style:leader-text` so viewers that honour it reproduce it.
            let style = match ts.leader {
                '-' => "dash",
                '.' | '\u{b7}' => "dotted",
                _ => "solid",
            };
            let mut text = String::new();
            esc(&ts.leader.to_string(), &mut text);
            stops.push_str(&format!(
                " style:leader-style=\"{style}\" style:leader-text=\"{text}\""
            ));
        }
        stops.push_str("/>");
    }
    format!("<style:tab-stops>{stops}</style:tab-stops>")
}

/// Wrap already-rendered paragraph-property attributes and an optional
/// `<style:tab-stops>` child into the full `<style:paragraph-properties>`
/// element. Tab stops are child *elements*, not attributes (ODF §17.8), so the
/// shell only self-closes when there are none.
fn odf_paragraph_properties_xml(attrs: &str, tabs: &str) -> String {
    if tabs.is_empty() {
        format!("<style:paragraph-properties{attrs}/>")
    } else {
        format!("<style:paragraph-properties{attrs}>{tabs}</style:paragraph-properties>")
    }
}

/// Normalises a model border-style string to the CSS/XSL-FO token an ODF
/// `fo:border` accepts: `solid`/`dashed`/`dotted`/`double` pass through, `none`
/// stays `none`, and anything unknown falls back to `solid`.
fn odf_border_style(style: &str) -> &str {
    match style {
        "solid" | "dashed" | "dotted" | "double" | "none" => style,
        _ => "solid",
    }
}

/// ODF automatic `<style:style family="paragraph">` from a model paragraph
/// style. The `<style:paragraph-properties>` shell is always emitted (matching
/// the historical output) even when fully default; explicit tab stops become
/// its `<style:tab-stops>` child element.
fn odf_para_style(name: &str, para: &Paragraph) -> String {
    let attrs = odf_para_prop_attrs(&para.style);
    let tabs = odf_tab_stops_xml(&para.style);
    format!(
        "<style:style style:name=\"{name}\" style:family=\"paragraph\">{}</style:style>",
        odf_paragraph_properties_xml(&attrs, &tabs)
    )
}

/// ODF `<style:style family="text">` from a model char style.
/// The ODF text-property attributes for the M2 [`CharStyle`] extensions —
/// `fo:text-transform:uppercase` (caps), `fo:font-variant:small-caps`,
/// `style:text-line-through-type="double"` (double strike),
/// `style:text-position` from a point-precise baseline shift (skipped when a
/// categorical super/sub already owns the attribute or the run size is
/// unknown), `style:text-scale` (horizontal scaling) and
/// `style:letter-kerning` (ODF kerning is boolean — the threshold degrades).
/// Shared by [`odf_span_style`], [`odf_named_text_attrs`] and the ODS font
/// path so all three stay in lock-step.
fn odf_text_extras(style: &CharStyle) -> String {
    let mut p = String::new();
    if style.caps {
        p.push_str(" fo:text-transform=\"uppercase\"");
    }
    if style.small_caps {
        p.push_str(" fo:font-variant=\"small-caps\"");
    }
    if style.dstrike {
        if !style.strike {
            // The double type needs an active line-through style to render.
            p.push_str(" style:text-line-through-style=\"solid\"");
        }
        p.push_str(" style:text-line-through-type=\"double\"");
    }
    if style.vertical_align == VAlign::Baseline {
        if let Some(shift) = style.baseline_shift_pt.filter(|v| *v != 0.0) {
            let size = run_size(style);
            if size > 0.0 {
                p.push_str(&format!(
                    " style:text-position=\"{}% 100%\"",
                    num((shift / size * 100.0 * 10.0).round() / 10.0)
                ));
            }
        }
    }
    if let Some(v) = style.char_scale_pct.filter(|v| *v > 0.0 && *v != 100.0) {
        p.push_str(&format!(" style:text-scale=\"{}%\"", v.round() as i64));
    }
    if style.kern_min_pt.filter(|v| *v > 0.0).is_some() {
        p.push_str(" style:letter-kerning=\"true\"");
    }
    p
}

fn odf_span_style(name: &str, style: &CharStyle) -> String {
    let generic = match style.generic {
        crate::convert::style::Generic::Sans => "swiss",
        crate::convert::style::Generic::Serif => "roman",
        crate::convert::style::Generic::Mono => "modern",
    };
    let mut p = format!(
        "<style:text-properties fo:font-size=\"{}pt\"",
        num(run_size(style))
    );
    if !style.family.is_empty() {
        let mut fam = String::new();
        esc(&style.family, &mut fam);
        p.push_str(&format!(
            " fo:font-family=\"{fam}\" style:font-family-generic=\"{generic}\""
        ));
    }
    if style.bold {
        p.push_str(" fo:font-weight=\"bold\"");
    }
    if style.italic {
        p.push_str(" fo:font-style=\"italic\"");
    }
    if style.underline {
        p.push_str(" style:text-underline-style=\"solid\" style:text-underline-width=\"auto\"");
    }
    if style.strike {
        p.push_str(" style:text-line-through-style=\"solid\"");
    }
    // Run superscript/subscript → `style:text-position` (ODF §20.371), the ODT
    // analogue of DOCX `w:vertAlign` and PPTX `a:rPr@baseline`. The value is a
    // vertical position plus an optional font-size scale; `super`/`sub` raise or
    // lower by the default amount and `58%` shrinks the glyph, mirroring the ODF
    // importer's `super`/`sub` → `VAlign::Super`/`Sub` so it round-trips.
    match style.vertical_align {
        VAlign::Super => p.push_str(" style:text-position=\"super 58%\""),
        VAlign::Sub => p.push_str(" style:text-position=\"sub 58%\""),
        VAlign::Baseline => {}
    }
    if let Some(c) = visible_color(style) {
        p.push_str(&format!(" fo:color=\"#{c}\""));
    }
    // Run highlight / background → `fo:background-color` on the text style, the
    // inverse of the ODF importer's `fo:background-color` → `CharStyle.background`
    // so a highlight round-trips through ODT. Any colour is emitted; `None` ⇒
    // nothing, leaving plain runs unchanged.
    if let Some(bg) = style.background {
        p.push_str(&format!(" fo:background-color=\"#{}\"", hex(bg)));
    }
    // Hidden text → `text:display="none"` (ODF §20.427), the ODT analogue of
    // DOCX `w:vanish` — previously emitted for DOCX only, so a hidden run lost
    // its flag through ODT.
    if style.hidden {
        p.push_str(" text:display=\"none\"");
    }
    p.push_str(&odf_text_extras(style));
    p.push_str("/>");
    format!("<style:style style:name=\"{name}\" style:family=\"text\">{p}</style:style>")
}

/// ODF list style: a per-level bullet or number definition from the marker.
/// Ordered levels carry the list's real ordinal suffix (`style:num-suffix` —
/// `.` by default, `)` for `1)`-style sources) so it round-trips.
fn odf_list_style(name: &str, list: &List) -> String {
    let mut levels = String::new();
    let mut prefix = String::new();
    esc(list.marker_prefix.as_deref().unwrap_or(""), &mut prefix);
    let prefix_attr = if prefix.is_empty() {
        String::new()
    } else {
        format!(" style:num-prefix=\"{prefix}\"")
    };
    for lvl in 1..=9u32 {
        // Per-depth style when the list carries real multi-level definitions
        // (`List::levels`, entry `lvl − 1`); the table-wide marker/suffix
        // otherwise — level-less lists emit exactly the pre-M2 shape.
        let level_style = list.levels.get(lvl as usize - 1);
        let (marker, suffix_ch, ordered) = match level_style {
            Some(ls) => (ls.marker, ls.suffix.unwrap_or('.'), ls.is_ordered()),
            None => (list.marker, list.suffix(), list.ordered),
        };
        if ordered {
            let fmt = match marker {
                ListMarker::LowerAlpha => "a",
                ListMarker::UpperAlpha => "A",
                ListMarker::LowerRoman => "i",
                ListMarker::UpperRoman => "I",
                _ => "1",
            };
            let start_attr = match level_style.filter(|ls| ls.start > 1) {
                Some(ls) => format!(" text:start-value=\"{}\"", ls.start),
                None => String::new(),
            };
            let mut suffix = String::new();
            esc(&suffix_ch.to_string(), &mut suffix);
            levels.push_str(&format!(
                "<text:list-level-style-number text:level=\"{lvl}\"{prefix_attr} style:num-suffix=\"{suffix}\" style:num-format=\"{fmt}\"{start_attr}>\
<style:list-level-properties text:space-before=\"{sb}pt\" text:min-label-width=\"18pt\"/>\
</text:list-level-style-number>",
                sb = 18 * (lvl - 1),
            ));
        } else {
            let bullet = match marker {
                ListMarker::Bullet(c) => c,
                _ => '•',
            };
            let mut b = String::new();
            esc(&bullet.to_string(), &mut b);
            levels.push_str(&format!(
                "<text:list-level-style-bullet text:level=\"{lvl}\" text:bullet-char=\"{b}\">\
<style:list-level-properties text:space-before=\"{sb}pt\" text:min-label-width=\"18pt\"/>\
</text:list-level-style-bullet>",
                sb = 18 * (lvl - 1),
            ));
        }
    }
    format!("<text:list-style style:name=\"{name}\">{levels}</text:list-style>")
}

/// ODF `META-INF/manifest.xml`. `image_exts[i]` is the package-native extension
/// of `Pictures/img{i+1}.<ext>`; each picture is declared with that path and the
/// matching `image/<ext>` media-type (ISO 26300 §2.2 / OpenDocument package).
fn odf_manifest(kind: &str, image_exts: &[&str], has_settings: bool) -> String {
    let media = match kind {
        "text" => "application/vnd.oasis.opendocument.text",
        "spreadsheet" => "application/vnd.oasis.opendocument.spreadsheet",
        "presentation" => "application/vnd.oasis.opendocument.presentation",
        _ => "application/vnd.oasis.opendocument.text",
    };
    let mut s = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<manifest:manifest xmlns:manifest=\"urn:oasis:names:tc:opendocument:xmlns:manifest:1.0\" manifest:version=\"1.3\">\
<manifest:file-entry manifest:full-path=\"/\" manifest:version=\"1.3\" manifest:media-type=\"{media}\"/>\
<manifest:file-entry manifest:full-path=\"content.xml\" manifest:media-type=\"text/xml\"/>\
<manifest:file-entry manifest:full-path=\"styles.xml\" manifest:media-type=\"text/xml\"/>\
<manifest:file-entry manifest:full-path=\"meta.xml\" manifest:media-type=\"text/xml\"/>"
    );
    if has_settings {
        s.push_str(
            "<manifest:file-entry manifest:full-path=\"settings.xml\" manifest:media-type=\"text/xml\"/>",
        );
    }
    for (i, ext) in image_exts.iter().enumerate() {
        let mime = office_image_format(ext).0;
        s.push_str(&format!(
            "<manifest:file-entry manifest:full-path=\"Pictures/img{}.{ext}\" manifest:media-type=\"{mime}\"/>",
            i + 1
        ));
    }
    s.push_str("</manifest:manifest>");
    s
}

// ───────────────────────────── ODS (typed sheet) ─────────────────────────────

/// A cell-style cache key uniquely identifying a `(fill, number format, font,
/// border, alignment, wrap)` combination so identical cells share one style.
///
/// Fields: fill hex, data-style name, font traits `(family, bold, italic,
/// size_centi, color hex)`, border `(width_centi, color hex)`, horizontal
/// alignment, wrap flag — all optional except the booleans.
#[derive(Clone, PartialEq, Eq)]
struct CellStyleKey {
    fill: Option<String>,
    data_style: Option<String>,
    /// `(family, bold, italic, underline, strike, size_centi, color_hex)`.
    font: Option<(String, bool, bool, bool, bool, u32, String)>,
    border: Option<(u32, String)>,
    align: Option<Align>,
    valign: Option<CellVAlign>,
    wrap: bool,
    /// Base reading direction → `style:writing-mode` (`rl-tb`/`lr-tb`).
    direction: Option<crate::model::Direction>,
    /// Text rotation in the model's signed degrees CCW → `style:rotation-angle`
    /// (ODF's CCW degrees; a negative model angle wraps to `360 + deg`).
    rotation_deg: Option<i32>,
}

/// Accumulates ODS automatic styles: number-format data styles (`<number:*>`),
/// the table-cell styles that bind fill/data-style/font/border/alignment, plus
/// column and row styles. Cells reference a cell style via `table:style-name`;
/// identical styling shares one style.
#[derive(Default)]
struct OdsStyler {
    /// `<number:number-style>` definitions, keyed by their ODF format code.
    data_styles: String,
    /// `<style:style>` definitions for cell / column / row families.
    cell_styles: String,
    /// Format code → data-style name.
    fmts: Vec<(String, String)>,
    /// Cell-style key → cell-style name.
    cells: Vec<(CellStyleKey, String)>,
    /// Row height (points, hundredths) → row-style name.
    rows: Vec<(u32, String)>,
    next_col: usize,
    next_row: usize,
}

impl OdsStyler {
    /// The data-style name for a spreadsheet number-format code (created lazily).
    fn data_style(&mut self, code: &str) -> String {
        if let Some((_, name)) = self.fmts.iter().find(|(c, _)| c == code) {
            return name.clone();
        }
        let name = format!("N{}", self.fmts.len());
        self.data_styles.push_str(&ods_number_style(&name, code));
        self.fmts.push((code.to_string(), name.clone()));
        name
    }

    /// The `table:style-name` value for a cell's fill, number format, font,
    /// border and alignment (or empty when the cell carries none of these).
    fn cell_style(&mut self, cell: &SheetCell) -> String {
        let fill = cell.fill.map(hex);
        let data_style = cell.number_format.as_deref().map(|c| self.data_style(c));
        let font = ods_font_key(&cell.style);
        let border = cell
            .border
            .filter(|b| b.width > 0.0)
            .map(|b| ((b.width * 100.0).round() as u32, hex(b.color)));
        let key = CellStyleKey {
            fill,
            data_style,
            font,
            border,
            align: cell.align,
            valign: cell.vertical_align,
            wrap: cell.wrap,
            direction: cell.direction,
            rotation_deg: cell.rotation_deg.filter(|d| (-90..=90).contains(d) && *d != 0),
        };
        if key.fill.is_none()
            && key.data_style.is_none()
            && key.font.is_none()
            && key.border.is_none()
            && key.align.is_none()
            && key.valign.is_none()
            && !key.wrap
            && key.direction.is_none()
            && key.rotation_deg.is_none()
        {
            return String::new();
        }
        if let Some((_, name)) = self.cells.iter().find(|(k, _)| *k == key) {
            return format!(" table:style-name=\"{name}\"");
        }
        let name = format!("ce{}", self.cells.len());
        let data_attr = match &key.data_style {
            Some(d) => format!(" style:data-style-name=\"{d}\""),
            None => String::new(),
        };

        // table-cell-properties: fill, border, wrap, vertical alignment.
        let mut cell_props = String::new();
        if let Some(c) = &key.fill {
            cell_props.push_str(&format!(" fo:background-color=\"#{c}\""));
        }
        if let Some((w_centi, color)) = &key.border {
            cell_props.push_str(&format!(
                " fo:border=\"{w}pt solid #{color}\"",
                w = num(*w_centi as f64 / 100.0),
            ));
        }
        if key.wrap {
            cell_props.push_str(" fo:wrap-option=\"wrap\"");
        }
        if let Some(v) = key.valign {
            cell_props.push_str(&format!(
                " style:vertical-align=\"{}\"",
                odf_cell_valign_attr(v)
            ));
        }
        // Base reading direction → `style:writing-mode` (mirrors the paragraph
        // path; the importer's `odf_writing_mode` reads it back).
        if let Some(dir) = key.direction {
            let mode = match dir {
                crate::model::Direction::Rtl => "rl-tb",
                crate::model::Direction::Ltr => "lr-tb",
            };
            cell_props.push_str(&format!(" style:writing-mode=\"{mode}\""));
        }
        // Text rotation → `style:rotation-angle` (ODF CCW degrees; the importer's
        // `odf_rotation_angle` maps it back to the model's signed degrees).
        if let Some(deg) = key.rotation_deg {
            let angle = if deg >= 0 { deg } else { 360 + deg };
            cell_props.push_str(&format!(" style:rotation-angle=\"{angle}\""));
        }
        let cell_props_xml = if cell_props.is_empty() {
            String::new()
        } else {
            format!("<style:table-cell-properties{cell_props}/>")
        };

        // paragraph-properties: horizontal alignment.
        let para_props = match key.align {
            Some(a) => format!(
                "<style:paragraph-properties fo:text-align=\"{}\"/>",
                odf_text_align(a)
            ),
            None => String::new(),
        };

        // text-properties: font family / weight / style / underline / strike /
        // size / colour.
        let text_props = match &key.font {
            Some((family, bold, italic, underline, strike, size_centi, color)) => ods_text_props(
                family,
                *bold,
                *italic,
                *underline,
                *strike,
                *size_centi,
                color,
            ),
            None => String::new(),
        };

        self.cell_styles.push_str(&format!(
            "<style:style style:name=\"{name}\" style:family=\"table-cell\"{data_attr}>{cell_props_xml}{para_props}{text_props}</style:style>"
        ));
        self.cells.push((key, name.clone()));
        format!(" table:style-name=\"{name}\"")
    }

    /// A table-column style for an explicit width (points); returns its name.
    fn column(&mut self, width: f64) -> String {
        let name = format!("co{}", self.next_col);
        self.next_col += 1;
        self.cell_styles.push_str(&format!(
            "<style:style style:name=\"{name}\" style:family=\"table-column\">\
<style:table-column-properties style:column-width=\"{cw}pt\"/></style:style>",
            cw = num(width),
        ));
        name
    }

    /// A table-row style for an explicit height (points); returns its name.
    /// Identical heights share one style.
    fn row_style(&mut self, height: f64) -> String {
        let key = (height * 100.0).round() as u32;
        if let Some((_, name)) = self.rows.iter().find(|(k, _)| *k == key) {
            return name.clone();
        }
        let name = format!("ro{}", self.next_row);
        self.next_row += 1;
        self.cell_styles.push_str(&format!(
            "<style:style style:name=\"{name}\" style:family=\"table-row\">\
<style:table-row-properties style:row-height=\"{rh}pt\" style:use-optimal-row-height=\"false\"/></style:style>",
            rh = num(height),
        ));
        self.rows.push((key, name.clone()));
        name
    }
}

/// The font cache-key tuple for a char style, or `None` when it carries no
/// distinguishing trait (empty family, default size, no bold/italic/colour).
fn ods_font_key(style: &CharStyle) -> Option<(String, bool, bool, bool, bool, u32, String)> {
    let color = visible_color(style).unwrap_or_default();
    let has_size = style.size_pt > 0.0;
    if style.family.is_empty()
        && !style.bold
        && !style.italic
        && !style.underline
        && !style.strike
        && !has_size
        && color.is_empty()
    {
        return None;
    }
    Some((
        style.family.clone(),
        style.bold,
        style.italic,
        style.underline,
        style.strike,
        (run_size(style) * 100.0).round() as u32,
        color,
    ))
}

/// `<style:text-properties>` for a cell font
/// (family/weight/style/underline/strike/size/colour).
fn ods_text_props(
    family: &str,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    size_centi: u32,
    color: &str,
) -> String {
    let mut p = String::from("<style:text-properties");
    if !family.is_empty() {
        let mut fam = String::new();
        esc(family, &mut fam);
        p.push_str(&format!(" fo:font-family=\"{fam}\""));
    }
    if bold {
        p.push_str(" fo:font-weight=\"bold\"");
    }
    if italic {
        p.push_str(" fo:font-style=\"italic\"");
    }
    // Underline / strike-through → `style:text-underline-style` /
    // `style:text-line-through-style` (ODF §20.367 / §20.358), the ODS analogue
    // of the run flags carried on word-processing/presentation text styles.
    if underline {
        p.push_str(" style:text-underline-style=\"solid\" style:text-underline-width=\"auto\"");
    }
    if strike {
        p.push_str(" style:text-line-through-style=\"solid\"");
    }
    p.push_str(&format!(
        " fo:font-size=\"{}pt\"",
        num(size_centi as f64 / 100.0)
    ));
    if !color.is_empty() {
        p.push_str(&format!(" fo:color=\"#{color}\""));
    }
    p.push_str("/>");
    p
}

/// The ODF `fo:text-align` value for a model alignment (`end` for right,
/// `justify` for justified).
fn odf_text_align(a: Align) -> &'static str {
    match a {
        Align::Left => "start",
        Align::Center => "center",
        Align::Right => "end",
        Align::Justify => "justify",
    }
}

/// A minimal ODF `<number:number-style>` for a spreadsheet format code. Only the
/// common `0`/`0.00`/`#,##0` shapes are modelled precisely; anything else falls
/// back to a generic number with the decimal count inferred from the code.
fn ods_number_style(name: &str, code: &str) -> String {
    let decimals = code
        .split_once('.')
        .map(|(_, frac)| frac.chars().take_while(|c| *c == '0' || *c == '#').count())
        .unwrap_or(0);
    let grouping = if code.contains(',') {
        " number:grouping=\"true\""
    } else {
        ""
    };
    format!(
        "<number:number-style style:name=\"{name}\">\
<number:number number:decimal-places=\"{decimals}\" number:min-integer-digits=\"1\"{grouping}/>\
</number:number-style>"
    )
}

/// The merge a cell at `(r, c)` participates in: an *anchor* (its top-left, with
/// the row/column span) or a *covered* slot inside another cell's range.
#[derive(Clone, Copy)]
enum MergeSlot {
    /// `(rows_spanned, cols_spanned)` — emitted on the anchor cell. At least one
    /// is `> 1`.
    Anchor(usize, usize),
    /// This `(r, c)` is covered by an anchor above/left of it.
    Covered,
}

/// Map each `(r, c)` touched by a [`MergeRange`] to its [`MergeSlot`]. ODS, like
/// the table path, expresses merges as `number-{columns,rows}-spanned` on the
/// anchor plus `<table:covered-table-cell/>` for the rest of the rectangle.
fn ods_merge_slots(sheet: &Sheet) -> std::collections::BTreeMap<(usize, usize), MergeSlot> {
    let mut slots = std::collections::BTreeMap::new();
    for m in &sheet.merges {
        let (r0, c0) = (m.r0.min(m.r1), m.c0.min(m.c1));
        let (r1, c1) = (m.r0.max(m.r1), m.c0.max(m.c1));
        let (rows, cols) = (r1 - r0 + 1, c1 - c0 + 1);
        if rows <= 1 && cols <= 1 {
            continue; // degenerate 1×1 "merge" — nothing to span.
        }
        for r in r0..=r1 {
            for c in c0..=c1 {
                let slot = if r == r0 && c == c0 {
                    MergeSlot::Anchor(rows, cols)
                } else {
                    MergeSlot::Covered
                };
                slots.insert((r, c), slot);
            }
        }
    }
    slots
}

fn ods_sheet_from_model(sheet: &Sheet, styler: &mut OdsStyler) -> String {
    let mut nm = String::new();
    esc(&sheet.name, &mut nm);
    let slots = ods_merge_slots(sheet);
    let mut rows = String::new();
    for (r, row) in sheet.rows.iter().enumerate() {
        let height_attr = match row.height {
            Some(h) if h > 0.0 => {
                let rn = styler.row_style(h);
                format!(" table:style-name=\"{rn}\"")
            }
            _ => String::new(),
        };
        rows.push_str(&format!("<table:table-row{height_attr}>"));
        for (c, cell) in row.cells.iter().enumerate() {
            match slots.get(&(r, c)) {
                Some(MergeSlot::Covered) => rows.push_str("<table:covered-table-cell/>"),
                Some(MergeSlot::Anchor(rspan, cspan)) => {
                    rows.push_str(&ods_cell_from_model(cell, Some((*rspan, *cspan)), styler))
                }
                None => rows.push_str(&ods_cell_from_model(cell, None, styler)),
            }
        }
        rows.push_str("</table:table-row>");
    }
    if sheet.rows.is_empty() {
        rows.push_str("<table:table-row><table:table-cell/></table:table-row>");
    }

    // Column definitions for explicit widths.
    let mut col_defs = String::new();
    for w in &sheet.col_widths {
        if *w <= 0.0 {
            continue;
        }
        let cn = styler.column(*w);
        col_defs.push_str(&format!("<table:table-column table:style-name=\"{cn}\"/>"));
    }

    format!("<table:table table:name=\"{nm}\">{col_defs}{rows}</table:table>")
}

/// One ODS data cell. `span` is `Some((rows, cols))` when this cell anchors a
/// merge (its `number-{rows,columns}-spanned` attributes), else `None`.
fn ods_cell_from_model(
    cell: &SheetCell,
    span: Option<(usize, usize)>,
    styler: &mut OdsStyler,
) -> String {
    let style_attr = styler.cell_style(cell);
    let mut span_attr = String::new();
    if let Some((rows, cols)) = span {
        if cols > 1 {
            span_attr.push_str(&format!(" table:number-columns-spanned=\"{cols}\""));
        }
        if rows > 1 {
            span_attr.push_str(&format!(" table:number-rows-spanned=\"{rows}\""));
        }
    }
    // The formula, in the ODF OpenFormula namespace (`of:=…`, ISO 26300 §9.1.6 /
    // OpenFormula §3). The cached result stays in `office:value`/the `<text:p>`.
    // The model strips the leading `=`; tolerate a stray one.
    let formula_attr = match cell.formula.as_deref() {
        Some(f) => {
            let mut e = String::new();
            esc(f.strip_prefix('=').unwrap_or(f), &mut e);
            format!(" table:formula=\"of:={e}\"")
        }
        None => String::new(),
    };
    let attrs = format!("{style_attr}{span_attr}{formula_attr}");
    // A cell hyperlink wraps the display text in a `text:a` (ODF §6.1.8); the
    // in-workbook `#Sheet!A1` form keeps its `#` in `xlink:href` verbatim.
    let link = |disp: String| -> String {
        match cell.hyperlink.as_deref().filter(|t| !t.is_empty()) {
            Some(target) => {
                let mut href = String::new();
                esc(target, &mut href);
                format!("<text:a xlink:type=\"simple\" xlink:href=\"{href}\">{disp}</text:a>")
            }
            None => disp,
        }
    };
    match &cell.value {
        CellValue::Empty => format!("<table:table-cell{attrs}/>"),
        CellValue::Number(n) => format!(
            "<table:table-cell{attrs} office:value-type=\"float\" office:value=\"{v}\"><text:p>{disp}</text:p></table:table-cell>",
            v = num(*n),
            disp = link(num(*n)),
        ),
        CellValue::Bool(b) => format!(
            "<table:table-cell{attrs} office:value-type=\"boolean\" office:boolean-value=\"{bv}\"><text:p>{disp}</text:p></table:table-cell>",
            bv = if *b { "true" } else { "false" },
            disp = link(if *b { "TRUE" } else { "FALSE" }.to_string()),
        ),
        CellValue::Text(t) => {
            let mut esc_t = String::new();
            esc(t, &mut esc_t);
            format!(
                "<table:table-cell{attrs} office:value-type=\"string\"><text:p>{disp}</text:p></table:table-cell>",
                disp = link(esc_t),
            )
        }
    }
}

fn ods_model_content(data_styles: &str, auto: &str, body: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<office:document-content xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" \
xmlns:style=\"urn:oasis:names:tc:opendocument:xmlns:style:1.0\" \
xmlns:table=\"urn:oasis:names:tc:opendocument:xmlns:table:1.0\" \
xmlns:text=\"urn:oasis:names:tc:opendocument:xmlns:text:1.0\" \
xmlns:fo=\"urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0\" \
xmlns:number=\"urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0\" \
xmlns:xlink=\"http://www.w3.org/1999/xlink\" office:version=\"1.3\">\
<office:automatic-styles>{data_styles}{auto}</office:automatic-styles>\
<office:body><office:spreadsheet>{body}</office:spreadsheet></office:body></office:document-content>"
    )
}

// ───────────────────────────── ODP (slide pages) ─────────────────────────────

fn odp_body(slides: &[Slide], doc: &Document, ctx: &mut OdfCtx) -> String {
    let mut body = String::new();
    if slides.is_empty() {
        body.push_str("<draw:page draw:master-page-name=\"Default\"/>");
    }
    for (i, slide) in slides.iter().enumerate() {
        // A slide with a background fill gets its own drawing-page auto-style
        // (`draw:fill="solid"` + the colour); the shared `dp1` (fill none) is
        // the default. The importer resolves `draw:style-name` through the
        // drawing-page style map back to [`Slide::background`], so backgrounds
        // round-trip.
        let page_style = match (&slide.background_gradient, slide.background) {
            // A full gradient page fill primes over the flat colour: the
            // drawing-page style references a named `<draw:gradient>` (defined
            // in styles.xml's `office:styles` by `odp_gradient_defs`).
            (Some(g), _) if g.stops.len() >= 2 => {
                let grad = ctx.gradient_name(g);
                let sid = ctx.next_style();
                let name = format!("{}dpBg{sid}", ctx.prefix);
                ctx.auto.push_str(&format!(
                    "<style:style style:name=\"{name}\" style:family=\"drawing-page\">\
<style:drawing-page-properties draw:fill=\"gradient\" draw:fill-gradient-name=\"{grad}\"/>\
</style:style>"
                ));
                name
            }
            (_, Some(rgb)) => {
                let sid = ctx.next_style();
                let name = format!("{}dpBg{sid}", ctx.prefix);
                ctx.auto.push_str(&format!(
                    "<style:style style:name=\"{name}\" style:family=\"drawing-page\">\
<style:drawing-page-properties draw:fill=\"solid\" draw:fill-color=\"#{}\"/></style:style>",
                    hex(rgb)
                ));
                name
            }
            _ => "dp1".to_string(),
        };
        // `draw:name="page{N}"` is the jump target for an internal
        // `LinkTarget::Page` (`text:a xlink:href="#page{N}"`, emitted by the shared
        // `odt_runs`). The 1-based slide index matches the `#page{N}` anchor the
        // ODT export uses, so the same model link resolves in both ODF presentations
        // and text documents.
        body.push_str(&format!(
            "<draw:page draw:name=\"page{}\" draw:style-name=\"{page_style}\" draw:master-page-name=\"Default\">",
            i + 1
        ));
        for ph in &slide.placeholders {
            odp_frame(&ph.block, Some(&ph.role), slide, doc, ctx, &mut body);
        }
        for sh in &slide.shapes {
            odp_frame(sh, None, slide, doc, ctx, &mut body);
        }
        // Speaker notes: a `presentation:notes` aside on the page (ISO 26300
        // §9.1.5), carrying the notes text in a `presentation:class="notes"`
        // frame. Emitted only when the slide has notes.
        if let Some(notes) = &slide.notes {
            odp_notes(notes, ctx, &mut body);
        }
        body.push_str("</draw:page>");
    }
    body
}

/// Append a slide's `presentation:notes` block, rendering the notes blocks into
/// a `presentation:class="notes"` text frame (ISO 26300 §9.1.5 / §9.6.1). The
/// frame geometry uses the conventional ODF notes box on a portrait notes page.
fn odp_notes(notes: &[Block], ctx: &mut OdfCtx, out: &mut String) {
    let sid = ctx.next_style();
    let sname = format!("{}Nt{sid}", ctx.prefix);
    ctx.auto.push_str(&format!(
        "<style:style style:name=\"{sname}\" style:family=\"graphic\">\
<style:graphic-properties draw:fill=\"none\" draw:stroke=\"none\" \
draw:auto-grow-width=\"false\" draw:auto-grow-height=\"false\" fo:padding=\"0pt\" \
draw:textarea-vertical-align=\"top\"/></style:style>"
    ));
    let mut content = String::new();
    for p in blocks_to_paras(notes) {
        content.push_str(&odt_paragraph(&p, "p", None, ctx));
    }
    out.push_str(&format!(
        "<presentation:notes draw:style-name=\"dp1\">\
<draw:frame draw:style-name=\"{sname}\" draw:layer=\"layout\" \
presentation:class=\"notes\" \
svg:x=\"68pt\" svg:y=\"390pt\" svg:width=\"480pt\" svg:height=\"230pt\">\
<draw:text-box>{content}</draw:text-box></draw:frame></presentation:notes>"
    ));
}

/// ODF `presentation:class` value for a placeholder [`PlaceholderRole`]
/// (ISO 26300 §9.6.1 / §19.x). The inverse of [`office_import`'s
/// `odp_placeholder_role`]: `Title`/`Subtitle` map to `title`/`subtitle`,
/// `Body` to the canonical body class `outline`, and `Other(token)` emits the
/// preserved token verbatim (ODF supports e.g. `footer`, `page-number`,
/// `date-time`, `notes` natively), so the role round-trips losslessly.
fn odp_presentation_class(role: &PlaceholderRole) -> String {
    match role {
        PlaceholderRole::Title => "title".to_string(),
        PlaceholderRole::Subtitle => "subtitle".to_string(),
        PlaceholderRole::Body => "outline".to_string(),
        PlaceholderRole::Other(token) => {
            let mut s = String::new();
            esc(token.trim(), &mut s);
            s
        }
    }
}

/// The positioning attributes of a slide element (`draw:frame` / `draw:rect` /
/// `draw:path`): plain `svg:x`/`svg:y` for an unrotated block, or — when the
/// block carries a rotation — a LibreOffice-style
/// `draw:transform="rotate(<rad>) translate(<x> <y>)"` **replacing** them (a
/// transformed frame drops `svg:x`/`svg:y`, exactly as Impress writes it).
/// `svg:width`/`svg:height` are always kept: the importer requires them to
/// place a transform-only frame.
///
/// This is the exact inverse of the importer pair `odp_frame_box_xf` +
/// `odp_transform_rotation` (office_import): the angle is the model's CCW
/// degrees converted to **radians** (ODF §19.228 — radians, counter-clockwise),
/// emitted at full `f64` precision so arbitrary angles survive the round trip,
/// and the translate carries the frame's **lower-left** corner — the `svg:y`
/// origin plus the emitted height (the importer reads `top = ty - h`).
fn odp_pos_attrs(r: crate::model::Rect, rotation: crate::model::Rotation) -> String {
    let w = r.w.max(1.0);
    let h = r.h.max(1.0);
    let deg = rotation.degrees().rem_euclid(360.0);
    if deg == 0.0 {
        format!(
            "svg:x=\"{}pt\" svg:y=\"{}pt\" svg:width=\"{}pt\" svg:height=\"{}pt\"",
            num(r.x),
            num(r.y),
            num(w),
            num(h)
        )
    } else {
        format!(
            "draw:transform=\"rotate({rad}) translate({x}pt {y}pt)\" \
svg:width=\"{w}pt\" svg:height=\"{h}pt\"",
            rad = deg.to_radians(),
            x = num(r.x),
            y = num(r.y + h),
            w = num(w),
            h = num(h),
        )
    }
}

fn odp_frame(
    block: &Block,
    role: Option<&PlaceholderRole>,
    slide: &Slide,
    doc: &Document,
    ctx: &mut OdfCtx,
    out: &mut String,
) {
    // For a semantic placeholder, carry its ODF `presentation:class` (+
    // `presentation:placeholder="true"`) on the emitted draw element so viewers
    // treat it as title/subtitle/body. Free shapes (role = None) get nothing.
    let pres = role
        .map(|r| {
            format!(
                " presentation:class=\"{}\" presentation:placeholder=\"true\"",
                odp_presentation_class(r)
            )
        })
        .unwrap_or_default();
    let r = block.frame.unwrap_or(crate::model::Rect::new(
        0.0,
        0.0,
        slide.geometry.width,
        slide.geometry.height,
    ));
    match &block.kind {
        BlockKind::Image(img) => {
            let (bytes, ext) = match doc_image(doc, img.resource) {
                Some(b) if !b.0.is_empty() => b,
                _ => return,
            };
            ctx.images.push((bytes, ext));
            let n = ctx.images.len();
            out.push_str(&format!(
                "<draw:frame draw:style-name=\"frI\" draw:layer=\"layout\"{pres} {pos}>\
<draw:image xlink:href=\"Pictures/img{n}.{ext}\" xlink:type=\"simple\" xlink:show=\"embed\" \
xlink:actuate=\"onLoad\"/></draw:frame>",
                pos = odp_pos_attrs(r, block.rotation),
            ));
        }
        BlockKind::Shape(shape) => {
            let placed = shape_to_placed(shape);
            let sid = ctx.next_style();
            let sname = format!("{}Sh{sid}", ctx.prefix);
            match &shape.fill_gradient {
                // A full gradient fill: a graphic style referencing the named
                // `<draw:gradient>` (stroke settings mirror odf_shape_style).
                Some(g) if g.stops.len() >= 2 => {
                    let grad = ctx.gradient_name(g);
                    let stroke = match placed.stroke {
                        Some(rgb) => format!(
                            "draw:stroke=\"solid\" svg:stroke-color=\"#{}\" \
svg:stroke-width=\"{}pt\"",
                            hex(rgb),
                            num(placed.stroke_width.max(0.5))
                        ),
                        None => "draw:stroke=\"none\"".to_string(),
                    };
                    ctx.auto.push_str(&format!(
                        "<style:style style:name=\"{sname}\" style:family=\"graphic\">\
<style:graphic-properties draw:fill=\"gradient\" draw:fill-gradient-name=\"{grad}\" \
{stroke}/></style:style>"
                    ));
                }
                _ => ctx.auto.push_str(&odf_shape_style(&sname, &placed)),
            }
            if shape_is_rect(&placed) {
                out.push_str(&format!(
                    "<draw:rect draw:style-name=\"{sname}\" draw:layer=\"layout\"{pres} {pos}/>",
                    pos = odp_pos_attrs(r, block.rotation),
                ));
            } else {
                out.push_str(&format!(
                    "<draw:path draw:style-name=\"{sname}\" draw:layer=\"layout\"{pres} {pos} \
svg:viewBox=\"0 0 {vw} {vh}\" svg:d=\"{d}\"/>",
                    pos = odp_pos_attrs(r, block.rotation),
                    vw = num(r.w.max(1.0)),
                    vh = num(r.h.max(1.0)),
                    d = odf_path_d(&placed.segments),
                ));
            }
        }
        BlockKind::Table(table) => {
            // A real `table:table` (ISO 26300 §9.1.2) inside the slide's
            // `draw:frame`, not a paragraph flatten (#26). `odt_table` builds
            // the `table:table` (and registers its table/column/cell styles into
            // `ctx.auto`); we only wrap it in the positioned frame here.
            let table_xml = odt_table(table, ctx);
            out.push_str(&format!(
                "<draw:frame draw:layer=\"layout\"{pres} {pos}>{table_xml}</draw:frame>",
                pos = odp_pos_attrs(r, block.rotation),
            ));
        }
        _ => {
            // Text frame: render the block's content into a text box, keeping
            // real `text:list`/`text:h` structure (via `odt_block`) instead of a
            // paragraph flatten, and hoisting any table out to its own
            // `draw:frame` (a `table:table` does not belong in a `draw:text-box`)
            // (#2).
            let mut content = String::new();
            let mut tables: Vec<Table> = Vec::new();
            odp_collect_frame(block, &mut content, &mut tables, ctx);

            if !content.is_empty() {
                let sid = ctx.next_style();
                let sname = format!("{}Tx{sid}", ctx.prefix);
                ctx.auto.push_str(&format!(
                    "<style:style style:name=\"{sname}\" style:family=\"graphic\">\
<style:graphic-properties draw:fill=\"none\" draw:stroke=\"none\" \
draw:auto-grow-width=\"false\" draw:auto-grow-height=\"false\" fo:padding=\"0pt\" \
draw:textarea-vertical-align=\"top\"/></style:style>"
                ));
                out.push_str(&format!(
                    "<draw:frame draw:style-name=\"{sname}\" draw:layer=\"layout\"{pres} \
{pos}><draw:text-box>{content}</draw:text-box></draw:frame>",
                    pos = odp_pos_attrs(r, block.rotation),
                ));
            }

            // Hoisted tables: each a real `table:table` in its own positioned
            // `draw:frame`, stacked down the slide with a default box.
            for (i, table) in tables.iter().enumerate() {
                let ty = r.y + 8.0 + 220.0 * i as f64;
                let table_xml = odt_table(table, ctx);
                out.push_str(&format!(
                    "<draw:frame draw:layer=\"layout\"{pres} {pos}>{table_xml}</draw:frame>",
                    pos = odp_pos_attrs(
                        crate::model::Rect::new(r.x, ty, r.w.max(1.0), 200.0),
                        block.rotation
                    ),
                ));
            }
        }
    }
}

/// Split a slide text-frame block into its text-box content (rendered with the
/// structure-preserving [`odt_block`], so lists/headings survive) and the tables
/// to hoist into their own `draw:frame`s. Recurses through the page-fallback's
/// `TextBox`/`Blockquote` wrappers; everything else renders in place. A genuinely
/// unrepresentable leaf still renders via `odt_block` (never dropped).
fn odp_collect_frame(
    block: &Block,
    content: &mut String,
    tables: &mut Vec<Table>,
    ctx: &mut OdfCtx,
) {
    match &block.kind {
        BlockKind::Table(table) => tables.push(table.clone()),
        BlockKind::TextBox(tb) => {
            for b in &tb.blocks {
                odp_collect_frame(b, content, tables, ctx);
            }
        }
        BlockKind::Blockquote(bq) => {
            for b in &bq.blocks {
                odp_collect_frame(&indent_block_left(b, 24.0), content, tables, ctx);
            }
        }
        _ => odt_block(block, ctx, content),
    }
}

fn odp_model_content(auto: &str, body: &str) -> String {
    let base = "<style:style style:name=\"dp1\" style:family=\"drawing-page\">\
<style:drawing-page-properties draw:fill=\"none\"/></style:style>\
<style:style style:name=\"frI\" style:family=\"graphic\">\
<style:graphic-properties draw:fill=\"none\" draw:stroke=\"none\"/></style:style>";
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<office:document-content xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" \
xmlns:style=\"urn:oasis:names:tc:opendocument:xmlns:style:1.0\" \
xmlns:table=\"urn:oasis:names:tc:opendocument:xmlns:table:1.0\" \
xmlns:text=\"urn:oasis:names:tc:opendocument:xmlns:text:1.0\" \
xmlns:draw=\"urn:oasis:names:tc:opendocument:xmlns:drawing:1.0\" \
xmlns:fo=\"urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0\" \
xmlns:svg=\"urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0\" \
xmlns:presentation=\"urn:oasis:names:tc:opendocument:xmlns:presentation:1.0\" \
xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
xmlns:xlink=\"http://www.w3.org/1999/xlink\" office:version=\"1.3\">\
<office:automatic-styles>{base}{auto}</office:automatic-styles>\
<office:body><office:presentation>{body}</office:presentation></office:body></office:document-content>"
    )
}

/// The `<draw:gradient>` definitions for every gradient a context registered
/// (ODF models a linear gradient as a two-colour axis: the first and last
/// stops survive; intermediate stops collapse — documented lossy). The angle
/// converts from the model's DrawingML convention (degrees clockwise from +X,
/// `0` = left→right) to ODF's decidegrees counter-clockwise with `0` =
/// bottom→top: `odf = (270 − deg) mod 360`.
fn odp_gradient_defs(gradients: &[crate::model::Gradient]) -> String {
    let mut out = String::new();
    for (i, g) in gradients.iter().enumerate() {
        let (Some(first), Some(last)) = (g.stops.first(), g.stops.last()) else {
            continue;
        };
        let decideg = (((270.0 - g.angle_deg).rem_euclid(360.0)) * 10.0).round() as i64;
        out.push_str(&format!(
            "<draw:gradient draw:name=\"Gradient{}\" draw:style=\"linear\" \
draw:start-color=\"#{}\" draw:end-color=\"#{}\" draw:start-intensity=\"100%\" \
draw:end-intensity=\"100%\" draw:angle=\"{decideg}\" draw:border=\"0%\"/>",
            i + 1,
            hex(first.color),
            hex(last.color)
        ));
    }
    out
}

/// Merge gradient definitions into an existing `<office:styles>` block (or
/// mint one) so `odp_styles_xml_model`'s `default_styles` slot carries both.
fn odf_styles_with_gradients(default_styles: String, gradient_defs: &str) -> String {
    if gradient_defs.is_empty() {
        return default_styles;
    }
    match default_styles.strip_prefix("<office:styles>") {
        Some(rest) => format!("<office:styles>{gradient_defs}{rest}"),
        None => format!("<office:styles>{gradient_defs}</office:styles>"),
    }
}

fn odp_styles_xml_model(pw: f64, ph: f64, default_styles: &str) -> String {
    let orient = if ph >= pw { "portrait" } else { "landscape" };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<office:document-styles xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" \
xmlns:style=\"urn:oasis:names:tc:opendocument:xmlns:style:1.0\" \
xmlns:fo=\"urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0\" \
xmlns:draw=\"urn:oasis:names:tc:opendocument:xmlns:drawing:1.0\" office:version=\"1.3\">\
<office:automatic-styles>\
<style:page-layout style:name=\"pm1\">\
<style:page-layout-properties fo:page-width=\"{w}pt\" fo:page-height=\"{h}pt\" \
style:print-orientation=\"{o}\"/></style:page-layout></office:automatic-styles>{default_styles}\
<office:master-styles>\
<style:master-page style:name=\"Default\" style:page-layout-name=\"pm1\"/>\
</office:master-styles></office:document-styles>",
        w = num(pw),
        h = num(ph),
        o = orient
    )
}

// ════════════════════════════ model → leaf adapters ════════════════════════════

/// Collect every [`Sheet`] from a document's `Block::Sheet` blocks (in document
/// order across all pages).
fn collect_sheets(doc: &Document) -> Vec<Sheet> {
    let mut out = Vec::new();
    for section in &doc.sections {
        for page in &section.pages {
            for block in &page.blocks {
                collect_sheets_block(block, &mut out);
            }
        }
    }
    out
}

fn collect_sheets_block(block: &Block, out: &mut Vec<Sheet>) {
    match &block.kind {
        BlockKind::Sheet(sb) => out.extend(sb.sheets.iter().cloned()),
        BlockKind::TextBox(tb) => {
            for b in &tb.blocks {
                collect_sheets_block(b, out);
            }
        }
        _ => {}
    }
}

/// Synthesize one [`Sheet`] per page from the document's flowing blocks — the
/// spreadsheet counterpart of [`collect_slides`]'s per-page fallback, used when
/// the model carries no native `Block::Sheet` content (a reconstructed PDF, a
/// Word/HTML import…).
///
/// Typed [`Table`]s become real rows and cells (column spans preserved as
/// merges, header rows bold, cell shading kept as fills), and prose — headings,
/// paragraphs, list items, block quotes, code lines — becomes single-cell rows
/// in reading order, so **all** document text lands in the workbook. This
/// deliberately replaces the legacy text-grid reconstruction
/// (`Document::convert_grids_and_shapes` → `office::to_xlsx_with_shapes`),
/// which re-derived rows/columns from raw glyph positions and could drop runs
/// the model keeps. Images and vector shapes have no cell representation and
/// are skipped: a spreadsheet is a data target (the picture-bearing exports are
/// DOCX/PPTX/ODT/ODP/HTML).
fn sheets_from_pages(doc: &Document) -> Vec<Sheet> {
    let mut out = Vec::new();
    let mut page_no = 0usize;
    for section in &doc.sections {
        for page in &section.pages {
            page_no += 1;
            let mut sheet = Sheet {
                name: format!("Page {page_no}"),
                ..Sheet::default()
            };
            for block in &page.blocks {
                sheet_rows_from_block(block, &mut sheet);
            }
            out.push(sheet);
        }
    }
    out
}

/// Append `block`'s spreadsheet representation to `sheet` (see
/// [`sheets_from_pages`] for the mapping rules).
fn sheet_rows_from_block(block: &Block, sheet: &mut Sheet) {
    match &block.kind {
        BlockKind::Table(t) => {
            for row in &t.rows {
                let r = sheet.rows.len();
                let mut cells: Vec<SheetCell> = Vec::new();
                for cell in &row.cells {
                    let c0 = cells.len();
                    let text = cell_plain_text(cell);
                    let mut sc = SheetCell {
                        fill: cell.shading,
                        ..SheetCell::default()
                    };
                    if !text.is_empty() {
                        if row.is_header {
                            // A header label is metadata, not data: keep it as
                            // text even when it looks numeric (CSV-import rule).
                            sc.value = CellValue::Text(text);
                        } else {
                            // Type the cell — number / bool / ISO date /
                            // localized number ("1 234,50", "15,50 %") — instead
                            // of forcing everything to text, so the native
                            // writers emit real `Number`/`Bool` cells and
                            // number formats.
                            let (value, number_format) = typed_sheet_value(&text);
                            sc.value = value;
                            sc.number_format = number_format;
                        }
                    }
                    if row.is_header {
                        sc.style.bold = true;
                    }
                    cells.push(sc);
                    // A spanning cell keeps the grid aligned with empty filler
                    // cells and a merge range covering them.
                    let span = (cell.col_span.max(1)) as usize;
                    for _ in 1..span {
                        cells.push(SheetCell::default());
                    }
                    if span > 1 {
                        sheet.merges.push(MergeRange {
                            r0: r,
                            c0,
                            r1: r,
                            c1: c0 + span - 1,
                        });
                    }
                }
                sheet.rows.push(SheetRow {
                    cells,
                    height: row.height,
                });
            }
        }
        BlockKind::Paragraph(p) => push_sheet_text_row(sheet, para_plain_text(p), false),
        BlockKind::Heading(h) => push_sheet_text_row(sheet, para_plain_text(&h.para), true),
        BlockKind::List(list) => {
            let mut n = list.start.max(1);
            for item in &list.items {
                let marker = if list.ordered {
                    format!("{n}{} ", list.suffix())
                } else if let ListMarker::Bullet(c) = list.marker {
                    format!("{c} ")
                } else {
                    "• ".to_string()
                };
                let text: String = blocks_to_paras(&item.blocks)
                    .iter()
                    .map(para_plain_text)
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ");
                if !text.is_empty() {
                    push_sheet_text_row(sheet, format!("{marker}{text}"), false);
                }
                n += 1;
            }
        }
        BlockKind::CodeBlock(code) => {
            for line in code.code.lines() {
                if !line.trim().is_empty() {
                    push_sheet_text_row(sheet, line.to_string(), false);
                }
            }
        }
        BlockKind::TextBox(tb) => {
            for b in &tb.blocks {
                sheet_rows_from_block(b, sheet);
            }
        }
        BlockKind::Blockquote(bq) => {
            for b in &bq.blocks {
                sheet_rows_from_block(b, sheet);
            }
        }
        // Native sheets are handled by `collect_sheets` (this fallback only
        // runs when there are none); slides flatten their placeholder text.
        BlockKind::Sheet(sb) => {
            for s in &sb.sheets {
                for row in &s.rows {
                    sheet.rows.push(row.clone());
                }
            }
        }
        BlockKind::Slide(sb) => {
            for slide in &sb.slides {
                for ph in &slide.placeholders {
                    sheet_rows_from_block(&ph.block, sheet);
                }
                for shape in &slide.shapes {
                    sheet_rows_from_block(shape, sheet);
                }
            }
        }
        // No cell representation for pictures, vector art, or rules.
        BlockKind::Image(_) | BlockKind::Shape(_) | BlockKind::HorizontalRule => {}
    }
}

/// Infer a typed [`CellValue`] (plus optional `number_format`) for one
/// Table→Sheet bridge cell.
///
/// Two layers, both conservative (anything ambiguous stays text):
///
/// 1. the strict CSV-import core ([`csv_import::infer_cell`]): booleans, ISO
///    dates (→ serial + date format) and bare `f64` tokens (`1234.5`);
/// 2. locale-aware numeric forms a reconstructed PDF table actually contains:
///    grouped thousands with a decimal comma or dot (`1 234,50`, `1,234.50`,
///    `1.234,50`), a plain decimal comma (`15,5`), percentages (`15,50 %`) and
///    currency-tagged amounts (`1 234,50 €`, `$1,234.50`) — typed as numbers
///    with the matching spreadsheet `number_format` so the visual shape
///    survives.
///
/// Any letter in the token (an address like `12 rue X`) keeps it text.
fn typed_sheet_value(text: &str) -> (CellValue, Option<String>) {
    let trimmed = text.trim();
    let (value, fmt) = crate::convert::csv_import::infer_cell(trimmed);
    if !matches!(value, CellValue::Text(_)) {
        return (value, fmt);
    }
    if let Some((n, fmt)) = parse_localized_number(trimmed) {
        return (CellValue::Number(n), fmt);
    }
    (value, fmt)
}

/// Parse a locale-formatted numeric token — grouped thousands, decimal comma,
/// percent, currency symbol — to `(value, number_format)`. `None` when the
/// token is not confidently numeric (any letter, malformed grouping, ambiguous
/// `1,234`, leading-zero code, over-long digit run ⇒ text).
fn parse_localized_number(s: &str) -> Option<(f64, Option<String>)> {
    // A trailing `%` (with optional space, incl. NBSP) marks a percentage.
    let (body, percent) = match s.strip_suffix('%') {
        Some(rest) => (rest.trim_end_matches([' ', '\u{a0}', '\u{202f}']), true),
        None => (s, false),
    };
    // A currency symbol at either end (only when not a percentage).
    let (body, currency) = if percent {
        (body, None)
    } else {
        strip_currency_symbol(body)
    };
    let (normalized, grouped, decimals) = match normalize_grouped_number(body) {
        Some(t) => t,
        // A plain (unlocalized) body is still meaningful when a % or currency
        // tag was stripped ("50%", "$1200", "12.5 €").
        None if percent || currency.is_some() => {
            let decimals = body.rsplit_once('.').map_or(0, |(_, f)| f.len());
            (body.to_string(), false, decimals)
        }
        None => return None,
    };
    // Delegate the final guards (leading zero, digit cap, exponent shape) to
    // the shared strict parser.
    let n = crate::convert::csv_import::parse_number(&normalized)?;
    if percent {
        let fmt = if decimals > 0 { "0.00%" } else { "0%" };
        return Some((n / 100.0, Some(fmt.to_string())));
    }
    if let Some((symbol, suffix)) = currency {
        let digits = if decimals > 0 { "#,##0.00" } else { "#,##0" };
        let fmt = if suffix {
            format!("{digits}\" {symbol}\"")
        } else {
            format!("\"{symbol}\"{digits}")
        };
        return Some((n, Some(fmt)));
    }
    if grouped {
        let fmt = if decimals > 0 { "#,##0.00" } else { "#,##0" };
        return Some((n, Some(fmt.to_string())));
    }
    Some((n, None))
}

/// Strip a currency symbol (€, $, £) off either end of the token, returning the
/// remaining text plus `Some((symbol, is_suffix))` when one was found.
fn strip_currency_symbol(s: &str) -> (&str, Option<(char, bool)>) {
    const SYMBOLS: [char; 3] = ['€', '$', '£'];
    for sym in SYMBOLS {
        if let Some(rest) = s.strip_suffix(sym) {
            let rest = rest.trim_end_matches([' ', '\u{a0}', '\u{202f}']);
            return (rest, Some((sym, true)));
        }
        if let Some(rest) = s.strip_prefix(sym) {
            let rest = rest.trim_start_matches([' ', '\u{a0}', '\u{202f}']);
            return (rest, Some((sym, false)));
        }
    }
    (s, None)
}

/// Normalize a locale-formatted numeric token to a plain `digits[.digits]`
/// string [`csv_import::parse_number`] accepts. Returns `(normalized,
/// had_grouping, fraction_digit_count)`, or `None` when the token is not a
/// clean localized number:
///
/// - space-like separators (space, NBSP, narrow NBSP, thin space) are always
///   thousands grouping (`1 234`, `12 345 678,90`); groups must be exactly 3
///   digits (first 1–3);
/// - with both `.` and `,` present, the **last** one is the decimal separator
///   and the other groups thousands (`1,234.50` and `1.234,50` both work);
/// - a single `,` is a decimal comma (`15,50`) unless exactly 3 digits follow
///   (`1,234` is ambiguous between en-grouping and fr-decimal ⇒ text);
/// - a repeated single separator is grouping (`1,234,567`);
/// - a lone `.` is left to the strict core (plain `f64` syntax) ⇒ `None` here.
fn normalize_grouped_number(s: &str) -> Option<(String, bool, usize)> {
    let s = s.trim();
    let (sign, digits_part) = match s.strip_prefix(['+', '-']) {
        Some(rest) => (&s[..1], rest),
        None => ("", s),
    };
    // Tokenize into digit runs and single separators; anything else ⇒ not a number.
    let mut runs: Vec<&str> = Vec::new();
    let mut seps: Vec<char> = Vec::new();
    let mut run_start: Option<usize> = None;
    let mut last_was_sep = true; // also rejects a leading separator
    for (i, ch) in digits_part.char_indices() {
        if ch.is_ascii_digit() {
            if run_start.is_none() {
                run_start = Some(i);
            }
            last_was_sep = false;
        } else if matches!(ch, ' ' | '\u{a0}' | '\u{202f}' | '\u{2009}' | '.' | ',') {
            if last_was_sep {
                return None; // doubled/leading separator
            }
            let start = run_start.take()?;
            runs.push(&digits_part[start..i]);
            seps.push(if ch == '.' || ch == ',' { ch } else { ' ' });
            last_was_sep = true;
        } else {
            return None; // letters / other punctuation ⇒ text
        }
    }
    if last_was_sep {
        return None; // trailing separator
    }
    if let Some(start) = run_start {
        runs.push(&digits_part[start..]);
    }
    if seps.is_empty() {
        return None; // plain token — the strict core already judged it
    }

    // Decide which trailing separator (if any) is the decimal point.
    let last_sep = *seps.last().expect("at least one separator");
    let kinds: std::collections::BTreeSet<char> = seps.iter().copied().collect();
    let last_run_len = runs.last().map_or(0, |r| r.len());
    let decimal = match last_sep {
        ' ' => false, // space never marks decimals
        '.' | ',' => {
            if kinds.len() > 1 {
                // Mixed separators: the last, distinct one is the decimal.
                seps.iter().filter(|c| **c == last_sep).count() == 1
            } else if seps.len() > 1 {
                false // repeated same separator ⇒ all grouping (1,234,567)
            } else if last_sep == ',' {
                if last_run_len == 3 {
                    // "1,234" is ambiguous — en-grouped 1234 vs fr-decimal
                    // 1.234 — so it conservatively stays text.
                    return None;
                }
                true // "15,50" / "3,14159" — an unambiguous decimal comma
            } else {
                // A single dot is plain f64 syntax — the strict core's call.
                return None;
            }
        }
        _ => false,
    };

    // Split off the fraction, then validate the grouping shape.
    let (int_runs, frac): (&[&str], &str) = if decimal {
        (&runs[..runs.len() - 1], runs[runs.len() - 1])
    } else {
        (&runs[..], "")
    };
    if int_runs.is_empty() || int_runs[0].is_empty() || int_runs[0].len() > 3 {
        return None;
    }
    if int_runs[1..].iter().any(|r| r.len() != 3) {
        return None; // malformed grouping ("12 34", "1,23.4")
    }
    let grouped = int_runs.len() > 1;
    if !grouped && !decimal {
        return None; // nothing localized about it after all
    }
    let mut normalized = String::with_capacity(s.len());
    normalized.push_str(sign);
    for r in int_runs {
        normalized.push_str(r);
    }
    if decimal && !frac.is_empty() {
        normalized.push('.');
        normalized.push_str(frac);
    }
    Some((normalized, grouped, if decimal { frac.len() } else { 0 }))
}

/// Append a single-cell text row (skipping empty text); `bold` marks headings.
fn push_sheet_text_row(sheet: &mut Sheet, text: String, bold: bool) {
    if text.trim().is_empty() {
        return;
    }
    let mut cell = SheetCell {
        value: CellValue::Text(text),
        ..SheetCell::default()
    };
    cell.style.bold = bold;
    sheet.rows.push(SheetRow {
        cells: vec![cell],
        height: None,
    });
}

/// Collect every [`Slide`] from a document's `Block::Slide` blocks. When there
/// are none, synthesize one slide per page whose blocks flow into a body
/// placeholder, so `pptx_from_model`/`odp_from_model` always produce slides.
fn collect_slides(doc: &Document) -> Vec<Slide> {
    let mut out = Vec::new();
    for section in &doc.sections {
        for page in &section.pages {
            for block in &page.blocks {
                if let BlockKind::Slide(sb) = &block.kind {
                    out.extend(sb.slides.iter().cloned());
                }
            }
        }
    }
    if !out.is_empty() {
        return out;
    }
    // Fallback: one slide per page. Blocks that carry a `frame` (the PDF
    // reconstruction positions *every* block) become free-floating positioned
    // shapes — text boxes, `p:pic` pictures, `p:sp` shapes, `a:tbl` frames at
    // their real page coordinates — instead of collapsing into one flowing Body
    // placeholder, which discarded every position and produced a near-empty
    // slide (no media, one text stub) from an image-rich deck. Frameless blocks
    // (flow imports without geometry) keep the Body-placeholder fallback.
    for section in &doc.sections {
        for page in &section.pages {
            if page.blocks.is_empty() {
                continue;
            }
            let (framed, flow): (Vec<&Block>, Vec<&Block>) =
                page.blocks.iter().partition(|b| b.frame.is_some());
            let shapes: Vec<Block> = framed.into_iter().map(slide_shape_block).collect();
            let placeholders = if flow.is_empty() {
                Vec::new()
            } else {
                let body_block = Block {
                    kind: BlockKind::TextBox(TextBox {
                        blocks: flow.into_iter().cloned().collect(),
                    }),
                    ..Block::default()
                };
                vec![crate::model::Placeholder {
                    role: PlaceholderRole::Body,
                    block: body_block,
                }]
            };
            out.push(Slide {
                geometry: section.geometry.clone(),
                shapes,
                placeholders,
                ..Default::default()
            });
        }
    }
    out
}

/// Normalize a deck to **one** slide size (PPTX has a single `p:sldSz`; the
/// model ODP export mirrors it with a single page layout): the size kept is the
/// **majority** size across the deck — not the first slide's, which let one
/// odd-sized cover dictate the geometry of every other slide. Slides whose
/// geometry deviates are scaled proportionally onto the majority box
/// (positions *and* sizes × the per-axis ratio, shape paths included so ODP
/// `svg:viewBox`-relative geometry stays consistent), so their content keeps
/// its relative layout instead of overflowing or huddling in a corner. A deck
/// whose slides all share one size — the normal case — is returned unchanged.
/// Returns the deck size (the majority, or the 960×540 default when empty).
fn normalize_slide_sizes(slides: &mut [Slide]) -> (f64, f64) {
    let Some(first) = slides.first() else {
        return (960.0, 540.0);
    };
    let first_size = (first.geometry.width, first.geometry.height);
    if slides
        .iter()
        .all(|s| (s.geometry.width, s.geometry.height) == first_size)
    {
        return first_size;
    }
    // Majority size (ties resolved toward the earliest size encountered):
    // quantized to 1/100 pt so float noise does not split a size class.
    // One class = (quantized key, representative size, occurrence count).
    type SizeClass = ((i64, i64), (f64, f64), usize);
    let key = |w: f64, h: f64| ((w * 100.0).round() as i64, (h * 100.0).round() as i64);
    let mut classes: Vec<SizeClass> = Vec::new();
    for s in slides.iter() {
        let k = key(s.geometry.width, s.geometry.height);
        match classes.iter_mut().find(|(ck, _, _)| *ck == k) {
            Some((_, _, n)) => *n += 1,
            None => classes.push((k, (s.geometry.width, s.geometry.height), 1)),
        }
    }
    let (maj_key, (mw, mh), _) = classes
        .iter()
        .max_by_key(|(_, _, n)| *n)
        .copied()
        .expect("non-empty deck");
    for s in slides.iter_mut() {
        if key(s.geometry.width, s.geometry.height) == maj_key {
            continue;
        }
        let (sw, sh) = (s.geometry.width, s.geometry.height);
        if sw > 0.0 && sh > 0.0 {
            let (rx, ry) = (mw / sw, mh / sh);
            for ph in &mut s.placeholders {
                scale_slide_block(&mut ph.block, rx, ry);
            }
            for sh_block in &mut s.shapes {
                scale_slide_block(sh_block, rx, ry);
            }
        }
        s.geometry.width = mw;
        s.geometry.height = mh;
    }
    (mw, mh)
}

/// Scale one slide block onto a resized slide: its placement frame by
/// `(rx, ry)`, and — for a vector [`Shape`] — its path segments too (the ODP
/// writer derives `svg:viewBox`/`svg:d` from them relative to the frame).
fn scale_slide_block(block: &mut Block, rx: f64, ry: f64) {
    if let Some(f) = &mut block.frame {
        f.x *= rx;
        f.y *= ry;
        f.w *= rx;
        f.h *= ry;
    }
    if let BlockKind::Shape(shape) = &mut block.kind {
        for seg in &mut shape.segments {
            match seg {
                PathSeg::Move(x, y) | PathSeg::Line(x, y) => {
                    *x *= rx;
                    *y *= ry;
                }
                PathSeg::Cubic(x1, y1, x2, y2, x3, y3) => {
                    *x1 *= rx;
                    *y1 *= ry;
                    *x2 *= rx;
                    *y2 *= ry;
                    *x3 *= rx;
                    *y3 *= ry;
                }
                PathSeg::Close => {}
            }
        }
    }
}

/// Adapt one positioned page block into a slide shape. The reconstruction
/// anchors a paragraph's `indent_left_pt` at the **page** left edge, but a
/// positioned slide text box already supplies that offset through its own
/// frame — keeping the indent would shift the text right a second time inside
/// the box. Strip the frame's own X from every nested left indent (floored at
/// zero, so deeper genuine indents keep their *relative* depth).
fn slide_shape_block(block: &Block) -> Block {
    let dx = block.frame.map(|f| f.x).unwrap_or(0.0);
    if dx <= 0.0 {
        return block.clone();
    }
    strip_left_indent(block, dx)
}

/// Clone `block` with every nested paragraph-like left indent reduced by `dx`
/// points, floored at zero (the recursion mirrors [`indent_block_left`]).
fn strip_left_indent(block: &Block, dx: f64) -> Block {
    let mut b = block.clone();
    match &mut b.kind {
        BlockKind::Paragraph(p) => p.style.indent_left_pt = (p.style.indent_left_pt - dx).max(0.0),
        BlockKind::Heading(h) => {
            h.para.style.indent_left_pt = (h.para.style.indent_left_pt - dx).max(0.0)
        }
        BlockKind::List(list) => {
            for item in &mut list.items {
                item.blocks = item
                    .blocks
                    .iter()
                    .map(|ib| strip_left_indent(ib, dx))
                    .collect();
            }
        }
        BlockKind::TextBox(tb) => {
            tb.blocks = tb
                .blocks
                .iter()
                .map(|ib| strip_left_indent(ib, dx))
                .collect();
        }
        BlockKind::Blockquote(bq) => {
            bq.blocks = bq
                .blocks
                .iter()
                .map(|ib| strip_left_indent(ib, dx))
                .collect();
        }
        // Code/rule/table/image/shape/sheet/slide keep their own geometry.
        _ => {}
    }
    b
}

/// A worksheet → a plain [`Table`] of its cells' display text (for the DOCX/ODT
/// flowing-table rendering of an embedded sheet).
fn sheet_to_table(sheet: &Sheet) -> Table {
    let cols = sheet.rows.iter().map(|r| r.cells.len()).max().unwrap_or(0);
    let rows = sheet
        .rows
        .iter()
        .map(|r| Row {
            cells: r
                .cells
                .iter()
                .map(|c| Cell {
                    blocks: vec![text_block(&cell_display(&c.value))],
                    shading: c.fill,
                    ..Cell::default()
                })
                .collect(),
            height: None,
            // A flattened spreadsheet carries no table-header-row semantics.
            is_header: false,
        })
        .collect();
    Table {
        rows,
        col_widths: sheet.col_widths.clone(),
        border: crate::model::BorderStyle {
            width: 0.5,
            color: [0.0, 0.0, 0.0],
        },
    }
    .with_cols(cols)
}

trait TableCols {
    fn with_cols(self, cols: usize) -> Self;
}
impl TableCols for Table {
    fn with_cols(mut self, cols: usize) -> Self {
        if self.col_widths.len() < cols {
            // leave widths defaulting (docx_col_widths fills the gap)
            self.col_widths.resize(cols, 0.0);
        }
        self
    }
}

fn cell_display(v: &CellValue) -> String {
    match v {
        CellValue::Empty => String::new(),
        CellValue::Text(t) => t.clone(),
        CellValue::Number(n) => num(*n),
        CellValue::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
    }
}

/// Clone `block`, shifting the left indent of its paragraph-like content by `pt`
/// points (recursing through quotes/text boxes/list items/cells). Used to set a
/// block quote off from the body without XML surgery.
fn indent_block_left(block: &Block, pt: f64) -> Block {
    let mut b = block.clone();
    match &mut b.kind {
        BlockKind::Paragraph(p) => p.style.indent_left_pt += pt,
        BlockKind::Heading(h) => h.para.style.indent_left_pt += pt,
        BlockKind::List(list) => {
            for item in &mut list.items {
                item.blocks = item
                    .blocks
                    .iter()
                    .map(|ib| indent_block_left(ib, pt))
                    .collect();
            }
        }
        BlockKind::TextBox(tb) => {
            tb.blocks = tb
                .blocks
                .iter()
                .map(|ib| indent_block_left(ib, pt))
                .collect();
        }
        BlockKind::Blockquote(bq) => {
            bq.blocks = bq
                .blocks
                .iter()
                .map(|ib| indent_block_left(ib, pt))
                .collect();
        }
        // Code/rule/table/image/shape/sheet/slide keep their own geometry.
        _ => {}
    }
    b
}

/// A plain single-run paragraph block carrying `text`.
fn text_block(text: &str) -> Block {
    Block {
        kind: BlockKind::Paragraph(Paragraph {
            runs: if text.is_empty() {
                Vec::new()
            } else {
                vec![Inline::Run(crate::model::InlineRun {
                    text: text.to_string(),
                    ..Default::default()
                })]
            },
            ..Paragraph::default()
        }),
        ..Block::default()
    }
}

/// Flatten a block into a list of paragraphs (for placeholder/text-box bodies):
/// paragraphs and headings pass through; lists/tables/sheets are reduced to one
/// paragraph per textual line so the text survives even in a text-only frame.
fn block_to_paras(block: &Block) -> Vec<Paragraph> {
    let mut out = Vec::new();
    collect_paras(block, &mut out);
    out
}

/// Flatten a list of blocks (e.g. a table cell's content) into paragraphs.
fn blocks_to_paras(blocks: &[Block]) -> Vec<Paragraph> {
    let mut out = Vec::new();
    for b in blocks {
        collect_paras(b, &mut out);
    }
    out
}

fn collect_paras(block: &Block, out: &mut Vec<Paragraph>) {
    match &block.kind {
        BlockKind::Paragraph(p) => out.push(p.clone()),
        BlockKind::Heading(h) => out.push(h.para.clone()),
        BlockKind::List(list) => {
            for item in &list.items {
                for b in &item.blocks {
                    collect_paras(b, out);
                }
            }
        }
        BlockKind::Table(table) => {
            for row in &table.rows {
                for cell in &row.cells {
                    for b in &cell.blocks {
                        collect_paras(b, out);
                    }
                }
            }
        }
        BlockKind::TextBox(tb) => {
            for b in &tb.blocks {
                collect_paras(b, out);
            }
        }
        BlockKind::Sheet(sb) => {
            for s in &sb.sheets {
                for row in &s.rows {
                    let line = row
                        .cells
                        .iter()
                        .map(|c| cell_display(&c.value))
                        .collect::<Vec<_>>()
                        .join("\t");
                    out.push(plain_para(&line));
                }
            }
        }
        BlockKind::CodeBlock(cb) => {
            // One monospaced paragraph per source line so the code text survives
            // in a text-only frame.
            for line in cb.code.split('\n') {
                let mut p = plain_para(line);
                if let Some(Inline::Run(r)) = p.runs.first_mut() {
                    r.style.generic = crate::convert::style::Generic::Mono;
                    r.style.size_pt = 10.0;
                }
                out.push(p);
            }
        }
        BlockKind::Blockquote(bq) => {
            for b in &bq.blocks {
                collect_paras(b, out);
            }
        }
        // A rule carries no text.
        BlockKind::HorizontalRule => {}
        BlockKind::Image(_) | BlockKind::Shape(_) => {}
        BlockKind::Slide(sb) => {
            for slide in &sb.slides {
                for ph in &slide.placeholders {
                    collect_paras(&ph.block, out);
                }
            }
        }
    }
}

fn plain_para(text: &str) -> Paragraph {
    Paragraph {
        runs: vec![Inline::Run(crate::model::InlineRun {
            text: text.to_string(),
            ..Default::default()
        })],
        ..Paragraph::default()
    }
}

/// A model [`Shape`] → the [`PlacedShape`] the DrawingML/ODF helpers consume.
/// The frame is unknown here (the helpers only need geometry + paint); width and
/// height come from the segment bounds.
fn shape_to_placed(shape: &Shape) -> PlacedShape {
    let (mut minx, mut miny, mut maxx, mut maxy) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    let mut note = |x: f64, y: f64| {
        minx = minx.min(x);
        miny = miny.min(y);
        maxx = maxx.max(x);
        maxy = maxy.max(y);
    };
    for seg in &shape.segments {
        match *seg {
            PathSeg::Move(x, y) | PathSeg::Line(x, y) => note(x, y),
            PathSeg::Cubic(x1, y1, x2, y2, x3, y3) => {
                note(x1, y1);
                note(x2, y2);
                note(x3, y3);
            }
            PathSeg::Close => {}
        }
    }
    let (w, h) = if minx <= maxx {
        ((maxx - minx).max(1.0), (maxy - miny).max(1.0))
    } else {
        (1.0, 1.0)
    };
    PlacedShape {
        x: 0.0,
        y: 0.0,
        width: w,
        height: h,
        segments: shape.segments.clone(),
        fill: shape.fill,
        stroke: shape.stroke,
        stroke_width: shape.stroke_width,
        fill_alpha: 1.0,
        stroke_alpha: 1.0,
        dash: shape.dash.clone(),
    }
}

/// Resolve an image blob by resource key from the document's resource table,
/// returning its bytes and the package-native extension for its format tag.
fn doc_image(doc: &Document, key: u64) -> Option<(Vec<u8>, &'static str)> {
    doc.resources
        .images
        .get(&key)
        .map(|r| (r.bytes.clone(), office_image_format(&r.format).1))
}

// ════════════════════════════════ MARKDOWN ════════════════════════════════════

/// Serialize a [`Document`] to **GitHub-Flavored Markdown** — the inverse of the
/// CommonMark/GFM importer ([`super::md_import`]).
///
/// Unlike the office/HTML exporters this produces *flowing prose Markdown*, not a
/// positioned layout: each [`Heading`] becomes an ATX heading (`#`..`######`),
/// paragraphs become text blocks with inline emphasis (`**bold**`, `*italic*`,
/// `` `code` ``, `~~strike~~`, `<sup>`/`<sub>`), [`List`]s render as nested
/// ordered/unordered (and task) lists, and [`Table`]s become GFM pipe tables with
/// an alignment row. Document metadata is emitted as an optional YAML
/// front-matter block.
///
/// Constructs **without a Markdown representation** are handled best-effort:
/// images are content-addressed in the model (no URL), so they are linked to a
/// stable extracted-asset filename (`image-{key}.{ext}`); vector [`Shape`]s have
/// no textual form and are skipped; embedded sheets render as GFM tables and
/// slides flatten with `---` rules between them. Footnotes and explicit heading
/// anchors have no corresponding model node and are therefore not emitted.
pub fn markdown_from_model(doc: &Document) -> String {
    let mut out = String::new();
    md_front_matter(&doc.meta, &mut out);

    let w = MdWriter::new(&doc.resources, &doc.footnotes);

    // Header → body pages (rule-separated) → footer, mirroring the other
    // model walkers' top-to-bottom traversal.
    if let Some(header) = doc.sections.first().and_then(|s| s.header.as_ref()) {
        w.blocks(header, &mut out);
    }
    let mut first_page = true;
    for section in &doc.sections {
        for page in &section.pages {
            if !first_page && !page.blocks.is_empty() {
                md_rule(&mut out); // page boundary → thematic break
            }
            if !page.blocks.is_empty() {
                first_page = false;
            }
            w.blocks(&page.blocks, &mut out);
        }
    }
    if let Some(footer) = doc.sections.first().and_then(|s| s.footer.as_ref()) {
        w.blocks(footer, &mut out);
    }

    // Footnote definitions: one trailing GFM `[^label]: body` per note (their
    // in-flow `[^label]` call markers were emitted by `inline`). Continuation
    // paragraphs indent four spaces, per the GFM footnote convention.
    if !doc.footnotes.is_empty() {
        out.push('\n');
        for note in &doc.footnotes {
            let mut body: Vec<String> = Vec::new();
            for para in blocks_to_paras(&note.blocks) {
                let text = w.inlines(&para.runs);
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    body.push(trimmed.to_string());
                }
            }
            out.push_str(&format!(
                "[^{}]: {}\n",
                md_footnote_label(&note.id),
                body.join("\n    ")
            ));
        }
    }

    // Collapse any run of >2 blank lines and guarantee a single trailing newline.
    md_tidy(out)
}

/// A model footnote id as a GFM footnote label: GFM labels cannot contain
/// whitespace or brackets, so those characters map to `-` (ids are typically
/// already short tokens — DOCX numeric ids come through unchanged).
fn md_footnote_label(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_whitespace() || matches!(c, '[' | ']' | '^') {
                '-'
            } else {
                c
            }
        })
        .collect()
}

/// Threads the resource table through the walk so image references can resolve
/// their stored format tag (→ filename extension), and the document's
/// footnotes so `[^label]` call markers only emit for resolvable notes.
struct MdWriter<'a> {
    resources: &'a crate::model::ResourceTable,
    footnotes: &'a [crate::model::Footnote],
}

impl<'a> MdWriter<'a> {
    fn new(
        resources: &'a crate::model::ResourceTable,
        footnotes: &'a [crate::model::Footnote],
    ) -> Self {
        MdWriter {
            resources,
            footnotes,
        }
    }

    /// A flat block list, each block separated by one blank line.
    fn blocks(&self, blocks: &[Block], out: &mut String) {
        for b in blocks {
            self.block(b, out);
        }
    }

    fn block(&self, block: &Block, out: &mut String) {
        match &block.kind {
            BlockKind::Heading(h) => self.heading(h, out),
            BlockKind::Paragraph(p) => self.paragraph(p, out),
            BlockKind::List(list) => {
                self.list(list, 0, out);
                out.push('\n'); // blank line after the list
            }
            BlockKind::Table(table) => self.table(table, out),
            BlockKind::Image(img) => {
                out.push_str(&self.image_md(img));
                out.push_str("\n\n");
            }
            BlockKind::Shape(shape) => {
                // GFM permits inline HTML, so the vector geometry is preserved as
                // a self-contained inline `<svg>` (same mapping as the HTML/EPUB
                // exporters' `web::html_shape` / `xhtml_shape`) rather than dropped.
                xhtml_shape(shape, out);
                out.push_str("\n\n");
            }
            BlockKind::TextBox(tb) => self.blocks(&tb.blocks, out),
            BlockKind::CodeBlock(cb) => self.code_block(cb, out),
            BlockKind::Blockquote(bq) => self.blockquote(bq, out),
            BlockKind::HorizontalRule => md_rule(out),
            BlockKind::Sheet(sheet) => {
                for s in &sheet.sheets {
                    self.table(&sheet_to_table(s), out);
                }
            }
            BlockKind::Slide(slides) => {
                for (i, slide) in slides.slides.iter().enumerate() {
                    if i > 0 {
                        md_rule(out);
                    }
                    for ph in &slide.placeholders {
                        self.block(&ph.block, out);
                    }
                    for sh in &slide.shapes {
                        self.block(sh, out);
                    }
                }
            }
        }
    }

    fn heading(&self, h: &Heading, out: &mut String) {
        let level = h.level.clamp(1, 6) as usize;
        for _ in 0..level {
            out.push('#');
        }
        out.push(' ');
        let text = self.inlines(&h.para.runs);
        // A heading must be a single line: fold any hard/soft breaks to spaces.
        out.push_str(text.replace('\n', " ").trim_end());
        out.push_str("\n\n");
    }

    /// A fenced code block: a backtick fence (lengthened past the longest internal
    /// backtick run so the content can never close the fence), the optional
    /// language info-string, then the verbatim code, then the closing fence.
    fn code_block(&self, cb: &CodeBlock, out: &mut String) {
        let fence_len = longest_backtick_run(&cb.code).saturating_add(1).max(3);
        let fence: String = "`".repeat(fence_len);
        out.push_str(&fence);
        if let Some(lang) = &cb.lang {
            // The info-string is a single token; keep it as-is (no newlines).
            out.push_str(lang.replace(['\n', '\r', '`'], " ").trim());
        }
        out.push('\n');
        out.push_str(&cb.code);
        if !cb.code.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&fence);
        out.push_str("\n\n");
    }

    /// A block quote: render the inner blocks to Markdown, then prefix every line
    /// (including the blank separators) with `> ` so nested constructs survive.
    fn blockquote(&self, bq: &Blockquote, out: &mut String) {
        let mut inner = String::new();
        self.blocks(&bq.blocks, &mut inner);
        let inner = md_tidy(inner); // collapse the trailing newline noise
        for line in inner.trim_end_matches('\n').split('\n') {
            if line.is_empty() {
                out.push('>');
            } else {
                out.push_str("> ");
                out.push_str(line);
            }
            out.push('\n');
        }
        out.push('\n');
    }

    fn paragraph(&self, p: &Paragraph, out: &mut String) {
        // Soft line breaks re-flow as spaces (a Markdown consumer re-wraps
        // freely); hard breaks keep the two-space `  \n` form. Same guard as
        // the DOCX path — see `reflow_soft_breaks`.
        let text = match reflow_soft_breaks(p) {
            Some(reflowed) => self.inlines(&reflowed),
            None => self.inlines(&p.runs),
        };
        let trimmed = text.trim_matches(|c| c == ' ' || c == '\t');
        if trimmed.is_empty() {
            return; // an empty paragraph contributes no Markdown
        }
        out.push_str(trimmed);
        out.push_str("\n\n");
    }

    /// Render a list at nesting `level` (0 = top). Each item's first paragraph
    /// (or heading) carries the marker; remaining item blocks are indented
    /// continuation, and nested lists recurse deeper. An item's own
    /// [`ListItem::level`] adds to `level`, so a flat item list with recorded
    /// depths (the Office importers) emits real Markdown nesting; ordered
    /// ordinals run per depth (a shallower item resets deeper counters) and
    /// honour the list's [`start`](List::start) and `)`-style suffix (the only
    /// non-`.` ordinal marker CommonMark accepts).
    fn list(&self, list: &List, level: usize, out: &mut String) {
        let suffix = if list.suffix() == ')' { ')' } else { '.' };
        // depth → last emitted ordinal at that depth.
        let mut counters: std::collections::BTreeMap<usize, usize> =
            std::collections::BTreeMap::new();
        for item in &list.items {
            let depth = level + item.level as usize;
            let indent = "    ".repeat(depth); // 4 spaces per nesting level
            let marker = if list.ordered {
                counters.retain(|&l, _| l <= depth);
                let seed = if depth == level {
                    list.start.max(1) as usize - 1
                } else {
                    0
                };
                let n = counters.entry(depth).or_insert(seed);
                *n += 1;
                format!("{n}{suffix}")
            } else {
                "-".to_string()
            };

            // Build the leading text line from the item's first paragraph-like
            // block; a leading task checkbox (`- [ ]`/`- [x]`) is detected from a
            // `[ ] `/`[x] ` prefix the importer leaves on the text.
            let mut rest = &item.blocks[..];
            let lead = match item.blocks.first() {
                Some(b) => match &b.kind {
                    BlockKind::Paragraph(p) => {
                        rest = &item.blocks[1..];
                        self.inlines(&p.runs)
                    }
                    BlockKind::Heading(h) => {
                        rest = &item.blocks[1..];
                        self.inlines(&h.para.runs)
                    }
                    _ => String::new(),
                },
                None => String::new(),
            };
            let lead = lead.replace('\n', " ");
            let lead = lead.trim();

            out.push_str(&indent);
            out.push_str(&marker);
            out.push(' ');
            out.push_str(lead);
            out.push('\n');

            // Remaining blocks of the item.
            for b in rest {
                match &b.kind {
                    BlockKind::List(nested) => self.list(nested, depth + 1, out),
                    BlockKind::Paragraph(p) => {
                        let t = self.inlines(&p.runs);
                        let t = t.trim();
                        if !t.is_empty() {
                            out.push_str(&indent);
                            out.push_str("    ");
                            out.push_str(&t.replace('\n', " "));
                            out.push('\n');
                        }
                    }
                    BlockKind::Image(img) => {
                        out.push_str(&indent);
                        out.push_str("    ");
                        out.push_str(&self.image_md(img));
                        out.push('\n');
                    }
                    _ => {
                        // Tables / sheets / text boxes inside an item: flatten to
                        // indented text lines so their content survives.
                        for para in block_to_paras(b) {
                            let t = self.inlines(&para.runs);
                            let t = t.trim();
                            if !t.is_empty() {
                                out.push_str(&indent);
                                out.push_str("    ");
                                out.push_str(&t.replace('\n', " "));
                                out.push('\n');
                            }
                        }
                    }
                }
            }
        }
    }

    /// A GFM pipe table. The first row is treated as the header; the alignment
    /// row derives each column's alignment from the header cell's first
    /// paragraph. `col_span > 1` is best-effort: the spanned text lands in its
    /// first physical column and the covered columns are left blank.
    fn table(&self, table: &Table, out: &mut String) {
        if table.rows.is_empty() {
            return;
        }
        let cols = table_col_count(table).max(1);

        // Materialize each row into exactly `cols` rendered cells, honouring
        // col_span by blanking the covered columns.
        let mut grid: Vec<Vec<String>> = Vec::with_capacity(table.rows.len());
        let mut aligns: Vec<Align> = vec![Align::Left; cols];
        for (r, row) in table.rows.iter().enumerate() {
            let mut line = vec![String::new(); cols];
            let mut phys = 0usize;
            for cell in &row.cells {
                if phys >= cols {
                    break;
                }
                line[phys] = self.cell_md(cell);
                if r == 0 {
                    aligns[phys] = cell_align(cell);
                }
                phys += (cell.col_span.max(1) as usize).max(1);
            }
            grid.push(line);
        }

        // Header row.
        md_table_row(&grid[0], out);
        // Alignment delimiter row.
        out.push('|');
        for a in &aligns {
            out.push(' ');
            out.push_str(match a {
                Align::Left => ":---",
                Align::Center => ":--:",
                Align::Right => "---:",
                Align::Justify => "----", // GFM has no justify; plain delimiter
            });
            out.push(' ');
            out.push('|');
        }
        out.push('\n');
        // Body rows.
        for line in grid.iter().skip(1) {
            md_table_row(line, out);
        }
        out.push('\n');
    }

    /// One table cell's inline content, with pipes escaped and newlines folded to
    /// `<br>` (GFM cells cannot contain literal line breaks).
    fn cell_md(&self, cell: &Cell) -> String {
        let mut text = String::new();
        let mut first = true;
        for b in &cell.blocks {
            for para in block_to_paras(b) {
                let line = self.inlines(&para.runs);
                let line = line.replace('\n', " ");
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if !first {
                    text.push_str("<br>");
                }
                text.push_str(line);
                first = false;
            }
        }
        // `inlines()` already backslash-escapes `|` (via `md_escape`), so the
        // cell text is pipe-safe without a second pass.
        text
    }

    /// Render a sequence of inlines to Markdown text.
    fn inlines(&self, runs: &[Inline]) -> String {
        let mut s = String::new();
        for r in runs {
            self.inline(r, &mut s);
        }
        s
    }

    fn inline(&self, inline: &Inline, out: &mut String) {
        match inline {
            Inline::Run(run) => out.push_str(&styled_run(&run.text, &run.style)),
            Inline::LineBreak => out.push_str("  \n"), // hard line break
            Inline::Image(img) => out.push_str(&self.image_md(img)),
            Inline::Link { href, children } => {
                let text = self.inlines(children);
                let label = if text.trim().is_empty() {
                    // Degenerate link with no children: show the URL itself.
                    match href {
                        LinkTarget::Url(u) => md_escape(u),
                        LinkTarget::Page(p) => format!("page {}", p + 1),
                        // TODO(M2): richer degenerate labels for the typed targets.
                        LinkTarget::Anchor(a) => md_escape(a),
                        LinkTarget::Other(o) => md_escape(o),
                    }
                } else {
                    text
                };
                match href {
                    LinkTarget::Url(url) if !url.is_empty() => {
                        out.push('[');
                        out.push_str(&label);
                        out.push_str("](");
                        out.push_str(&md_link_dest(url));
                        out.push(')');
                    }
                    LinkTarget::Page(p) => {
                        // Internal page jump → anchor convention.
                        out.push('[');
                        out.push_str(&label);
                        out.push_str(&format!("](#page-{})", p + 1));
                    }
                    // Empty URL: emit the label as plain text.
                    LinkTarget::Url(_) => out.push_str(&label),
                    // A named in-document anchor → a `[label](#name)` fragment
                    // link (the Markdown convention the importer's
                    // `link_destination` reads back as an anchor target).
                    LinkTarget::Anchor(a) if !a.is_empty() => {
                        out.push('[');
                        out.push_str(&label);
                        out.push_str("](#");
                        out.push_str(&md_link_dest(a));
                        out.push(')');
                    }
                    // An empty anchor / raw action summary degrades to the
                    // plain label (no portable Markdown target).
                    LinkTarget::Anchor(_) | LinkTarget::Other(_) => out.push_str(&label),
                }
            }
            // Markdown has no comment syntax; a comment anchor emits nothing.
            Inline::CommentRef { .. } => {}
            // A footnote anchor → the GFM `[^id]` call marker; the matching
            // `[^id]: body` definitions are appended once at the end of the
            // document by `markdown_from_model` (see `md_footnote_defs`).
            Inline::FootnoteRef { id } => {
                if self.footnotes.iter().any(|f| f.id == *id) {
                    out.push_str(&format!("[^{}]", md_footnote_label(id)));
                }
            }
        }
    }

    /// `![alt](dest)` for a model image. The model stores content-addressed
    /// blobs (no source URL), so the destination is a stable extracted-asset
    /// filename derived from the resource key and its stored format tag.
    fn image_md(&self, img: &ImageRef) -> String {
        let ext = self
            .resources
            .images
            .get(&img.resource)
            .map(|r| r.format.as_str())
            .filter(|f| !f.is_empty())
            .unwrap_or("png");
        let alt = img.alt.as_deref().map(md_escape).unwrap_or_default();
        format!("![{}](image-{}.{})", alt, img.resource, ext)
    }
}

/// Emit a YAML front-matter block from document metadata, when any field is set.
fn md_front_matter(meta: &crate::model::DocMeta, out: &mut String) {
    let has_any = meta.title.is_some()
        || meta.author.is_some()
        || meta.subject.is_some()
        || !meta.keywords.is_empty()
        || meta.lang.is_some();
    if !has_any {
        return;
    }
    out.push_str("---\n");
    if let Some(t) = &meta.title {
        out.push_str(&format!("title: {}\n", yaml_scalar(t)));
    }
    if let Some(a) = &meta.author {
        out.push_str(&format!("author: {}\n", yaml_scalar(a)));
    }
    if let Some(s) = &meta.subject {
        out.push_str(&format!("subject: {}\n", yaml_scalar(s)));
    }
    if !meta.keywords.is_empty() {
        out.push_str("keywords:\n");
        for k in &meta.keywords {
            out.push_str(&format!("  - {}\n", yaml_scalar(k)));
        }
    }
    if let Some(l) = &meta.lang {
        out.push_str(&format!("lang: {}\n", yaml_scalar(l)));
    }
    out.push_str("---\n\n");
}

/// Quote a YAML scalar when it contains characters that would otherwise be
/// mis-parsed (`:` `#`, leading/trailing space, or YAML indicators), using
/// double quotes with `"`/`\` escaped.
fn yaml_scalar(s: &str) -> String {
    let needs_quote = s.is_empty()
        || s.starts_with(' ')
        || s.ends_with(' ')
        || s.starts_with([
            '!', '&', '*', '?', '|', '>', '%', '@', '`', '"', '\'', '#', '-',
        ])
        || s.contains(": ")
        || s.contains(" #")
        || s.contains('\n')
        || s.contains('\t');
    if !needs_quote {
        return s.to_string();
    }
    let mut q = String::with_capacity(s.len() + 2);
    q.push('"');
    for c in s.chars() {
        match c {
            '"' => q.push_str("\\\""),
            '\\' => q.push_str("\\\\"),
            '\n' => q.push_str("\\n"),
            '\t' => q.push_str("\\t"),
            _ => q.push(c),
        }
    }
    q.push('"');
    q
}

/// A `---` thematic break on its own line, with surrounding blank lines.
fn md_rule(out: &mut String) {
    if !out.is_empty() && !out.ends_with("\n\n") {
        if out.ends_with('\n') {
            out.push('\n');
        } else {
            out.push_str("\n\n");
        }
    }
    out.push_str("---\n\n");
}

/// The length of the longest consecutive run of backticks in `s` (0 if none) —
/// used to size a code fence so the content can never accidentally close it.
fn longest_backtick_run(s: &str) -> usize {
    let mut best = 0usize;
    let mut cur = 0usize;
    for c in s.chars() {
        if c == '`' {
            cur += 1;
            best = best.max(cur);
        } else {
            cur = 0;
        }
    }
    best
}

/// Emit one GFM table row (`| a | b |`) from already-rendered cells.
fn md_table_row(cells: &[String], out: &mut String) {
    out.push('|');
    for c in cells {
        out.push(' ');
        out.push_str(c);
        out.push(' ');
        out.push('|');
    }
    out.push('\n');
}

/// The alignment of a table cell, read from its first paragraph's style.
fn cell_align(cell: &Cell) -> Align {
    for b in &cell.blocks {
        if let BlockKind::Paragraph(p) = &b.kind {
            return p.style.align;
        }
        if let BlockKind::Heading(h) = &b.kind {
            return h.para.style.align;
        }
    }
    Align::Left
}

/// Wrap `text` (already content) in the emphasis markers implied by `style`.
/// Nesting order outer→inner: strike, bold, italic, code; super/sub use inline
/// HTML (valid in CommonMark). Empty text yields an empty string (no stray
/// markers). Leading/trailing spaces are kept *outside* the markers so the
/// emphasis renders (CommonMark forbids ` ** x **`).
fn styled_run(text: &str, style: &CharStyle) -> String {
    if text.is_empty() {
        return String::new();
    }
    // Split off surrounding whitespace so markers hug the visible text
    // (CommonMark forbids ` ** x ** `). Byte offsets are taken from the ASCII
    // whitespace run, so the `core` slice is always on a char boundary.
    let lead_len = text.len() - text.trim_start().len();
    let trail_len = text.len() - text.trim_end().len();
    if lead_len + trail_len >= text.len() {
        // All whitespace: emit verbatim (no markers, escaping unneeded for spaces).
        return text.to_string();
    }
    let lead_ws = &text[..lead_len];
    let trail_ws = &text[text.len() - trail_len..];
    let core = &text[lead_len..text.len() - trail_len];

    // Innermost: the text itself. A monospace run becomes a literal code span
    // (its content is *not* backslash-escaped — backticks are handled by the
    // fence length); otherwise super/sub use inline HTML and the text is
    // CommonMark-escaped.
    let mut body = if style.generic == crate::convert::style::Generic::Mono {
        code_span(core)
    } else if style.vertical_align == VAlign::Super {
        format!("<sup>{}</sup>", md_escape(core))
    } else if style.vertical_align == VAlign::Sub {
        format!("<sub>{}</sub>", md_escape(core))
    } else {
        md_escape(core)
    };

    if style.bold && style.italic {
        body = format!("***{}***", body);
    } else if style.bold {
        body = format!("**{}**", body);
    } else if style.italic {
        body = format!("*{}*", body);
    }
    if style.strike {
        body = format!("~~{}~~", body);
    }
    if style.underline {
        // CommonMark has no underline; HTML <u> is the faithful representation.
        body = format!("<u>{}</u>", body);
    }
    // CommonMark has no portable colour syntax; GFM permits inline HTML, so a
    // non-default (not near-black) run colour is carried by an outer
    // `<span style="color:#RRGGBB">…</span>` wrapping the emphasised body.
    if let Some(c) = markup_color(style) {
        body = format!("<span style=\"color:#{c}\">{body}</span>");
    }
    format!("{lead_ws}{body}{trail_ws}")
}

/// Wrap `text` in a code span, choosing a backtick fence longer than the longest
/// backtick run inside, and padding with a space when the content starts/ends
/// with a backtick (per CommonMark code-span rules).
fn code_span(text: &str) -> String {
    let mut longest = 0usize;
    let mut cur = 0usize;
    for c in text.chars() {
        if c == '`' {
            cur += 1;
            longest = longest.max(cur);
        } else {
            cur = 0;
        }
    }
    let fence = "`".repeat(longest + 1);
    let needs_pad = text.starts_with('`')
        || text.ends_with('`')
        || (longest > 0 && text == "`".repeat(text.len()));
    if needs_pad {
        format!("{fence} {text} {fence}")
    } else {
        format!("{fence}{text}{fence}")
    }
}

/// Escape the CommonMark/GFM punctuation that would otherwise be interpreted as
/// *inline* markup, plus pipes (for table safety). Backslash-escapes are honoured
/// by every CommonMark parser.
///
/// Block-level markers (`#`, `-`, `+`, `.`, `>`) are **not** escaped here: they
/// are only significant at the start of a line, and the paragraph/list/table
/// writers fully control line starts. Escaping them mid-text would litter prose
/// with ugly backslashes (e.g. `A paragraph\.`).
fn md_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' | '`' | '*' | '_' | '{' | '}' | '[' | ']' | '(' | ')' | '!' | '|' | '<' | '~' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// Encode a link destination: wrap in `<...>` when it contains spaces or control
/// characters, otherwise percent-escape the few characters that break inline
/// link syntax (`(` `)` ` `).
fn md_link_dest(url: &str) -> String {
    if url.is_empty() {
        return String::new();
    }
    if url.contains(' ') || url.chars().any(|c| c.is_control()) {
        // Angle-bracket form tolerates spaces; escape any literal `>`/`<`.
        let inner = url.replace('<', "%3C").replace('>', "%3E");
        return format!("<{}>", inner);
    }
    url.replace('(', "%28").replace(')', "%29")
}

/// Collapse runs of 3+ newlines into a blank-line separator and ensure the
/// output ends with exactly one newline (empty input ⇒ empty output).
fn md_tidy(s: String) -> String {
    let mut out = String::with_capacity(s.len());
    let mut newlines = 0usize;
    for c in s.chars() {
        if c == '\n' {
            newlines += 1;
            if newlines <= 2 {
                out.push('\n');
            }
        } else {
            newlines = 0;
            out.push(c);
        }
    }
    // Trim trailing blank lines, then guarantee a single terminating newline.
    while out.ends_with('\n') {
        out.pop();
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

// ════════════════════════════════════ CSV ════════════════════════════════════

/// Serialize a [`Document`] to **RFC 4180** CSV text.
///
/// Each [`Sheet`] (from every `Block::Sheet`) becomes a block of CSV rows —
/// one line per [`SheetRow`](crate::model::SheetRow), cells joined by `,`, lines
/// terminated by `\r\n`. When the document has **no** spreadsheet content, the
/// document's flowing [`Table`]s are used instead, with each cell reduced to its
/// plain text.
///
/// RFC 4180 has no multi-sheet concept, so the convention is kept
/// standard-friendly. A **single** sheet/table emits pure RFC 4180 — just its
/// records, with no preamble or separator. When there is **more than one**, each
/// block is introduced by a plain (RFC-4180-quotable) **name row** carrying the
/// sheet/table name — or `Sheet N` / `Table N` when unnamed — rather than a `#`
/// comment row (which a strict parser would mis-read as data), and consecutive
/// blocks are separated by a single blank line. A strict split on the blank line
/// thus yields one well-formed RFC-4180 block per sheet/table. A document with
/// neither sheets nor tables yields an empty string.
pub fn csv_from_model(doc: &Document) -> String {
    let sheets = collect_sheets(doc);
    if !sheets.is_empty() {
        return csv_from_sheets(&sheets);
    }
    let tables = collect_tables(doc);
    if !tables.is_empty() {
        return csv_from_tables(&tables);
    }
    String::new()
}

/// Quote a single field per RFC 4180: wrap in `"…"` (doubling any embedded `"`)
/// only when the field contains a comma, quote, CR or LF; otherwise emit it raw.
fn csv_field(field: &str) -> String {
    let needs_quote = field
        .chars()
        .any(|c| c == ',' || c == '"' || c == '\r' || c == '\n');
    if !needs_quote {
        return field.to_string();
    }
    let mut out = String::with_capacity(field.len() + 2);
    out.push('"');
    for ch in field.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

/// Join one record's already-stringified fields with `,` and the RFC-4180 CRLF
/// terminator.
fn csv_record(fields: &[String]) -> String {
    let mut line: String = fields
        .iter()
        .map(|f| csv_field(f))
        .collect::<Vec<_>>()
        .join(",");
    line.push_str("\r\n");
    line
}

fn csv_from_sheets(sheets: &[Sheet]) -> String {
    // A single sheet is emitted as pure RFC 4180 (no name row, no separator);
    // only with several sheets is a name row needed to tell them apart.
    let labelled = sheets.len() > 1;
    let mut out = String::new();
    for (i, sheet) in sheets.iter().enumerate() {
        if i > 0 {
            // Blank record separating consecutive sheets.
            out.push_str("\r\n");
        }
        if labelled {
            // A plain name row (RFC 4180), quoted by `csv_record` if it contains
            // a comma/quote/newline. Unnamed sheets fall back to `Sheet N`.
            let name = if sheet.name.is_empty() {
                format!("Sheet {}", i + 1)
            } else {
                sheet.name.clone()
            };
            out.push_str(&csv_record(&[name]));
        }
        for row in &sheet.rows {
            let fields: Vec<String> = row.cells.iter().map(|c| cell_display(&c.value)).collect();
            out.push_str(&csv_record(&fields));
        }
    }
    out
}

fn csv_from_tables(tables: &[Table]) -> String {
    let labelled = tables.len() > 1;
    let mut out = String::new();
    for (i, table) in tables.iter().enumerate() {
        if i > 0 {
            out.push_str("\r\n");
        }
        if labelled {
            // A plain name row (RFC 4180); flowing tables are unnamed, so `Table N`.
            out.push_str(&csv_record(&[format!("Table {}", i + 1)]));
        }
        for row in &table.rows {
            let fields: Vec<String> = row.cells.iter().map(cell_plain_text).collect();
            out.push_str(&csv_record(&fields));
        }
    }
    out
}

/// A table cell's plain text: every paragraph the cell flattens to, joined by a
/// single space (a CSV field is one logical line, so embedded paragraph breaks
/// are collapsed rather than emitting raw newlines).
fn cell_plain_text(cell: &Cell) -> String {
    let mut paras: Vec<Paragraph> = Vec::new();
    for b in &cell.blocks {
        collect_paras(b, &mut paras);
    }
    paras
        .iter()
        .map(para_plain_text)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// A paragraph's plain text: the concatenation of its run texts (links recurse
/// into their children); non-text inlines contribute nothing.
fn para_plain_text(para: &Paragraph) -> String {
    let mut out = String::new();
    inlines_plain_text(&para.runs, &mut out);
    out
}

fn inlines_plain_text(runs: &[Inline], out: &mut String) {
    for r in runs {
        match r {
            Inline::Run(run) => out.push_str(&run.text),
            Inline::Link { children, .. } => inlines_plain_text(children, out),
            Inline::LineBreak => out.push(' '),
            Inline::Image(_) => {}
            Inline::CommentRef { .. } => {}
            // A note anchor has no text of its own (body in `Document::footnotes`).
            Inline::FootnoteRef { .. } => {}
        }
    }
}

/// Collect every flowing [`Table`] from a document's `Block::Table` blocks (in
/// document order across all pages), descending into text boxes, lists, and the
/// cells of outer tables.
fn collect_tables(doc: &Document) -> Vec<Table> {
    let mut out = Vec::new();
    for section in &doc.sections {
        for page in &section.pages {
            for block in &page.blocks {
                collect_tables_block(block, &mut out);
            }
        }
    }
    out
}

fn collect_tables_block(block: &Block, out: &mut Vec<Table>) {
    match &block.kind {
        BlockKind::Table(table) => {
            out.push(table.clone());
            // Nested tables inside cells are emitted after their parent.
            for row in &table.rows {
                for cell in &row.cells {
                    for b in &cell.blocks {
                        collect_tables_block(b, out);
                    }
                }
            }
        }
        BlockKind::TextBox(tb) => {
            for b in &tb.blocks {
                collect_tables_block(b, out);
            }
        }
        BlockKind::List(list) => {
            for item in &list.items {
                for b in &item.blocks {
                    collect_tables_block(b, out);
                }
            }
        }
        _ => {}
    }
}

// ════════════════════════════════════ EPUB ═══════════════════════════════════

/// Serialize a [`Document`] to a valid **EPUB 3** publication (a ZIP container).
///
/// Layout (the `mimetype` part is **stored uncompressed and written first**, as
/// the OCF spec requires so readers can sniff the format from the archive head):
///
/// ```text
/// mimetype                      (application/epub+zip, stored)
/// META-INF/container.xml        (points at the OPF)
/// OEBPS/content.opf             (metadata from DocMeta + manifest + spine)
/// OEBPS/nav.xhtml               (EPUB 3 navigation document)
/// OEBPS/toc.ncx                 (EPUB 2 NCX, for back-compat)
/// OEBPS/text-N.xhtml            (one reflowable chapter per Section)
/// OEBPS/images/img-<key>.<ext>  (each ResourceTable image, embedded once)
/// ```
///
/// Each [`Section`] becomes one XHTML chapter built from its blocks
/// (headings/paragraphs/lists/tables/sheets/images/shapes) as strict,
/// well-formed XHTML; images are referenced by relative path and declared in the
/// manifest with their real media-type. The document always has at least one
/// spine item (an empty chapter is emitted for an empty document) so the result
/// is spec-valid.
pub fn epub_from_model(doc: &Document) -> Vec<u8> {
    let mut zip = ZipWriter::new();

    // 1. mimetype — MUST be first and stored (uncompressed).
    zip.add_stored("mimetype", b"application/epub+zip");

    // 2. OCF container pointing at the package document.
    zip.add_deflated("META-INF/container.xml", EPUB_CONTAINER_XML.as_bytes());

    // 3. Build chapters (one per section; guarantee at least one).
    let chapters = epub_chapters(doc);

    // 4. Embedded images, sorted by key for deterministic output.
    let images = epub_images(doc);

    // 5. A unique, deterministic publication identifier (content hash; no clock
    //    or RNG in the engine). The OPF `dc:identifier` and the NCX `dtb:uid`
    //    MUST agree, so it is computed once and threaded into both.
    let ident = epub_identifier(doc, &chapters);

    // 6. Package document, navigation, NCX.
    let opf = epub_opf(doc, &chapters, &images, &ident);
    let nav = epub_nav(doc, &chapters);
    let ncx = epub_ncx(doc, &chapters, &ident);
    zip.add_deflated("OEBPS/content.opf", opf.as_bytes());
    zip.add_deflated("OEBPS/nav.xhtml", nav.as_bytes());
    zip.add_deflated("OEBPS/toc.ncx", ncx.as_bytes());

    // 7. Chapter XHTML files.
    for ch in &chapters {
        zip.add_deflated(&format!("OEBPS/{}", ch.file), ch.xhtml.as_bytes());
    }

    // 8. Image blobs (already-compressed formats → stored).
    for img in &images {
        zip.add_stored(&format!("OEBPS/{}", img.path), &img.bytes);
    }

    zip.finish()
}

const EPUB_CONTAINER_XML: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<container version=\"1.0\" xmlns=\"urn:oasis:names:tc:opendocument:xmlns:container\">\
<rootfiles>\
<rootfile full-path=\"OEBPS/content.opf\" media-type=\"application/oebps-package+xml\"/>\
</rootfiles></container>";

/// A built chapter: its OPF item id, its file name (relative to `OEBPS/`), the
/// title used for the table of contents, the rendered XHTML, and the in-document
/// heading hierarchy (with stable anchor ids matching the `id` attributes
/// emitted on the chapter's headings) used to build a *nested* TOC.
struct EpubChapter {
    id: String,
    file: String,
    title: String,
    xhtml: String,
    headings: Vec<TocHeading>,
}

/// A heading captured while rendering a chapter, for the nested table of
/// contents. `level` is the heading level (1–6, clamped as emitted), `title` is
/// its plain text, and `id` is the anchor id set on the heading element in the
/// chapter XHTML (so `text-N.xhtml#id` resolves to the heading).
struct TocHeading {
    level: u8,
    title: String,
    id: String,
}

/// Per-chapter context for the nested TOC: which chapter is being rendered and
/// the running heading ordinal, accumulating each heading (with its assigned
/// anchor id) as the body is serialized. Built and consumed in a single pass so
/// the anchor ids in the XHTML and in the TOC can never diverge.
struct EpubToc {
    chapter: usize,
    seq: usize,
    headings: Vec<TocHeading>,
}

impl EpubToc {
    fn new(chapter: usize) -> Self {
        EpubToc {
            chapter,
            seq: 0,
            headings: Vec::new(),
        }
    }

    /// Allocate the next anchor id for a heading in this chapter and record it.
    /// Returns the id to set on the heading element.
    fn record(&mut self, level: u8, title: String) -> String {
        self.seq += 1;
        let id = format!("sec{}-h{}", self.chapter, self.seq);
        self.headings.push(TocHeading {
            level,
            title,
            id: id.clone(),
        });
        id
    }
}

/// A resolved image for embedding: OPF item id, path (relative to `OEBPS/`),
/// media-type, and bytes.
struct EpubImage {
    id: String,
    path: String,
    media_type: String,
    bytes: Vec<u8>,
}

/// Map an [`ImageResource`](crate::model::ImageResource) format tag to its
/// `(media-type, file extension)`.
fn epub_image_mime(format: &str) -> (&'static str, &'static str) {
    match format.to_ascii_lowercase().as_str() {
        "jpeg" | "jpg" => ("image/jpeg", "jpg"),
        "gif" => ("image/gif", "gif"),
        "webp" => ("image/webp", "webp"),
        "svg" => ("image/svg+xml", "svg"),
        _ => ("image/png", "png"),
    }
}

fn epub_images(doc: &Document) -> Vec<EpubImage> {
    // `ResourceTable::images` is a BTreeMap, so iteration is already key-ordered.
    doc.resources
        .images
        .iter()
        .map(|(key, res)| {
            let (media_type, ext) = epub_image_mime(&res.format);
            EpubImage {
                id: format!("img-{key}"),
                path: format!("images/img-{key}.{ext}"),
                media_type: media_type.to_string(),
                bytes: res.bytes.clone(),
            }
        })
        .collect()
}

/// A unique, deterministic publication identifier of the form
/// `urn:gigapdf:<16-hex>`, where `<16-hex>` is a 64-bit FNV-1a hash over the
/// document's title, language and the (title + serialized XHTML) of every
/// chapter — i.e. its text *and* structure. Two different documents therefore
/// get different identifiers, while the same document always hashes identically
/// (no clock or RNG, both of which are unavailable in this engine). `DocMeta`
/// has no dedicated identifier field, so the content hash is the source.
fn epub_identifier(doc: &Document, chapters: &[EpubChapter]) -> String {
    // FNV-1a (64-bit) over a length-prefixed digest so distinct field
    // boundaries can't collide (e.g. title "AB"+body "C" vs "A"+"BC").
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for &b in (bytes.len() as u64).to_le_bytes().iter().chain(bytes) {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01B3);
        }
    };
    feed(doc.meta.title.as_deref().unwrap_or("").as_bytes());
    feed(epub_lang(doc).as_bytes());
    for ch in chapters {
        feed(ch.title.as_bytes());
        feed(ch.xhtml.as_bytes());
    }
    format!("urn:gigapdf:{h:016x}")
}

/// One chapter per section. A chapter title is the first heading's text, else
/// `"Section N"`. An empty document still yields a single empty chapter so the
/// spine is never empty.
fn epub_chapters(doc: &Document) -> Vec<EpubChapter> {
    let mut chapters = Vec::new();
    for (i, section) in doc.sections.iter().enumerate() {
        let n = i + 1;
        let title = section_title(section).unwrap_or_else(|| format!("Section {n}"));
        let (xhtml, headings) = epub_chapter_xhtml(doc, section, &title, n);
        chapters.push(EpubChapter {
            id: format!("chap-{n}"),
            file: format!("text-{n}.xhtml"),
            title,
            xhtml,
            headings,
        });
    }
    if chapters.is_empty() {
        let title = doc
            .meta
            .title
            .clone()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "Document".to_string());
        let xhtml = epub_empty_chapter_xhtml(doc, &title);
        chapters.push(EpubChapter {
            id: "chap-1".to_string(),
            file: "text-1.xhtml".to_string(),
            title,
            xhtml,
            headings: Vec::new(),
        });
    }
    chapters
}

/// The text of a section's first heading block, if any (used as a chapter title).
fn section_title(section: &Section) -> Option<String> {
    for page in &section.pages {
        for block in &page.blocks {
            if let BlockKind::Heading(h) = &block.kind {
                let t = para_plain_text(&h.para);
                if !t.trim().is_empty() {
                    return Some(t);
                }
            }
        }
    }
    None
}

/// The BCP-47 language for the publication: the document's, else `"en"`.
fn epub_lang(doc: &Document) -> String {
    doc.meta
        .lang
        .clone()
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| "en".to_string())
}

/// XHTML document scaffold shared by all chapters: a strict XHTML5 skeleton with
/// the EPUB namespace, the publication language, and the chapter `title`. `body`
/// is the already-escaped serialized block content.
fn epub_xhtml_doc(lang: &str, title: &str, body: &str) -> String {
    let mut t = String::new();
    esc(title, &mut t);
    let lang_attr = {
        let mut s = String::new();
        esc(lang, &mut s);
        s
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE html>\n\
<html xmlns=\"http://www.w3.org/1999/xhtml\" \
xmlns:epub=\"http://www.idpf.org/2007/ops\" \
xml:lang=\"{lang_attr}\" lang=\"{lang_attr}\">\
<head><meta charset=\"utf-8\"/><title>{t}</title></head>\
<body>{body}</body></html>"
    )
}

fn epub_empty_chapter_xhtml(doc: &Document, title: &str) -> String {
    let lang = epub_lang(doc);
    let mut body = String::from("<h1>");
    esc(title, &mut body);
    body.push_str("</h1>");
    epub_xhtml_doc(&lang, title, &body)
}

fn epub_chapter_xhtml(
    doc: &Document,
    section: &Section,
    title: &str,
    chapter: usize,
) -> (String, Vec<TocHeading>) {
    let lang = epub_lang(doc);
    let mut body = String::new();
    let mut toc = EpubToc::new(chapter);
    if let Some(header) = &section.header {
        body.push_str("<header>");
        xhtml_blocks(header, doc, &mut body, &mut toc);
        body.push_str("</header>");
    }
    for page in &section.pages {
        xhtml_blocks(&page.blocks, doc, &mut body, &mut toc);
    }
    if let Some(footer) = &section.footer {
        body.push_str("<footer>");
        xhtml_blocks(footer, doc, &mut body, &mut toc);
        body.push_str("</footer>");
    }
    // Chapter-local notes section: the bodies of every footnote referenced in
    // this chapter (their in-flow `<sup>` call markers link down here).
    let mut referenced: Vec<String> = Vec::new();
    for page in &section.pages {
        collect_footnote_refs(&page.blocks, &mut referenced);
    }
    xhtml_footnotes(&referenced, doc, &mut body, &mut toc);
    if body.is_empty() {
        body.push_str("<h1>");
        esc(title, &mut body);
        body.push_str("</h1>");
    }
    (epub_xhtml_doc(&lang, title, &body), toc.headings)
}

/// Collect (in document order, first occurrence only) the ids of every
/// [`Inline::FootnoteRef`] under `blocks` — recursing through nested block
/// containers and link children — so a chapter can render exactly the notes
/// it references.
fn collect_footnote_refs(blocks: &[Block], out: &mut Vec<String>) {
    fn inlines(runs: &[Inline], out: &mut Vec<String>) {
        for r in runs {
            match r {
                Inline::FootnoteRef { id } => {
                    if !out.contains(id) {
                        out.push(id.clone());
                    }
                }
                Inline::Link { children, .. } => inlines(children, out),
                _ => {}
            }
        }
    }
    for b in blocks {
        match &b.kind {
            BlockKind::Paragraph(p) => inlines(&p.runs, out),
            BlockKind::Heading(h) => inlines(&h.para.runs, out),
            BlockKind::List(l) => {
                for item in &l.items {
                    collect_footnote_refs(&item.blocks, out);
                }
            }
            BlockKind::Table(t) => {
                for row in &t.rows {
                    for cell in &row.cells {
                        collect_footnote_refs(&cell.blocks, out);
                    }
                }
            }
            BlockKind::TextBox(tb) => collect_footnote_refs(&tb.blocks, out),
            BlockKind::Blockquote(bq) => collect_footnote_refs(&bq.blocks, out),
            _ => {}
        }
    }
}

// ───────────────────────── model → strict XHTML (EPUB) ──────────────────────

fn xhtml_blocks(blocks: &[Block], doc: &Document, out: &mut String, toc: &mut EpubToc) {
    for b in blocks {
        xhtml_block(b, doc, out, toc);
    }
}

fn xhtml_block(block: &Block, doc: &Document, out: &mut String, toc: &mut EpubToc) {
    match &block.kind {
        BlockKind::Paragraph(p) => {
            out.push_str(&format!("<p{}>", xhtml_align_attr(p)));
            xhtml_inlines(&p.runs, doc, out);
            out.push_str("</p>");
        }
        BlockKind::Heading(h) => {
            let lvl = h.level.clamp(1, 6);
            // Allocate a stable anchor id and record the heading for the nested
            // TOC; the same id is set here so `text-N.xhtml#id` resolves.
            let id = toc.record(lvl, para_plain_text(&h.para));
            out.push_str(&format!(
                "<h{lvl} id=\"{id}\"{}>",
                xhtml_align_attr(&h.para)
            ));
            xhtml_inlines(&h.para.runs, doc, out);
            out.push_str(&format!("</h{lvl}>"));
        }
        BlockKind::List(list) => xhtml_list(list, doc, out, toc),
        BlockKind::Table(table) => xhtml_table(table, doc, out, toc),
        BlockKind::Image(img) => {
            out.push_str("<p>");
            xhtml_image(img, doc, out);
            out.push_str("</p>");
        }
        BlockKind::Shape(shape) => xhtml_shape(shape, out),
        BlockKind::TextBox(tb) => {
            out.push_str("<div>");
            xhtml_blocks(&tb.blocks, doc, out, toc);
            out.push_str("</div>");
        }
        BlockKind::CodeBlock(cb) => {
            // Preformatted code: <pre><code> keeps whitespace + a language class.
            let class = match &cb.lang {
                Some(l) if !l.trim().is_empty() => {
                    let mut esc_lang = String::new();
                    esc(l.trim(), &mut esc_lang);
                    format!(" class=\"language-{esc_lang}\"")
                }
                _ => String::new(),
            };
            out.push_str(&format!("<pre><code{class}>"));
            esc(&cb.code, out);
            out.push_str("</code></pre>");
        }
        BlockKind::Blockquote(bq) => {
            out.push_str("<blockquote>");
            xhtml_blocks(&bq.blocks, doc, out, toc);
            out.push_str("</blockquote>");
        }
        BlockKind::HorizontalRule => out.push_str("<hr/>"),
        BlockKind::Sheet(sb) => {
            for s in &sb.sheets {
                xhtml_sheet(s, out);
            }
        }
        BlockKind::Slide(sb) => {
            for slide in &sb.slides {
                out.push_str("<section>");
                for ph in &slide.placeholders {
                    xhtml_block(&ph.block, doc, out, toc);
                }
                out.push_str("</section>");
            }
        }
    }
}

/// A `style="text-align:…"` attribute when the paragraph alignment is not the
/// default (left), else empty.
fn xhtml_align_attr(p: &Paragraph) -> String {
    match p.style.align {
        Align::Left => String::new(),
        Align::Center => " style=\"text-align:center\"".to_string(),
        Align::Right => " style=\"text-align:right\"".to_string(),
        Align::Justify => " style=\"text-align:justify\"".to_string(),
    }
}

fn xhtml_inlines(runs: &[Inline], doc: &Document, out: &mut String) {
    for r in runs {
        match r {
            Inline::Run(run) => {
                if run.text.is_empty() {
                    continue;
                }
                let style = xhtml_char_css(&run.style);
                if style.is_empty() {
                    esc(&run.text, out);
                } else {
                    out.push_str("<span style=\"");
                    esc(&style, out);
                    out.push_str("\">");
                    esc(&run.text, out);
                    out.push_str("</span>");
                }
            }
            Inline::LineBreak => out.push_str("<br/>"),
            Inline::Image(img) => xhtml_image(img, doc, out),
            Inline::Link { href, children } => {
                let target = match href {
                    LinkTarget::Url(u) => u.clone(),
                    LinkTarget::Page(p) => format!("#page{p}"),
                    // The fragment form is the natural XHTML lowering for a
                    // named anchor.
                    LinkTarget::Anchor(a) => format!("#{a}"),
                    // An action summary has no XHTML target — empty href
                    // degrades like an empty URL.
                    LinkTarget::Other(_) => String::new(),
                };
                out.push_str("<a href=\"");
                esc(&target, out);
                out.push_str("\">");
                xhtml_inlines(children, doc, out);
                out.push_str("</a>");
            }
            // A comment anchor with no body emits nothing in this XHTML path.
            Inline::CommentRef { .. } => {}
            // A footnote anchor: the superscript call marker (the note's
            // 1-based ordinal) linking to the chapter-local notes section
            // appended by `xhtml_footnotes` — parity with
            // `convert::web::html_from_model`. Dangling anchors emit nothing.
            Inline::FootnoteRef { id } => {
                if let Some(pos) = doc.footnotes.iter().position(|f| f.id == *id) {
                    let mut frag = String::new();
                    esc(id, &mut frag);
                    out.push_str(&format!(
                        "<sup class=\"fn-ref\" id=\"fnr-{frag}\">\
<a href=\"#fnt-{frag}\">{}</a></sup>",
                        pos + 1
                    ));
                }
            }
        }
    }
}

/// The chapter's notes `<aside>` — one entry per referenced footnote (its
/// ordinal, body blocks and a back-link to the in-flow call marker),
/// mirroring `convert::web::html_from_model`'s notes section. Emits nothing
/// when the chapter references no note.
fn xhtml_footnotes(
    referenced: &[String],
    doc: &Document,
    out: &mut String,
    toc: &mut EpubToc,
) {
    if referenced.is_empty() {
        return;
    }
    out.push_str("<aside class=\"footnotes\" epub:type=\"footnotes\"><ol>");
    for id in referenced {
        let Some(note) = doc.footnotes.iter().find(|f| f.id == *id) else {
            continue;
        };
        let mut frag = String::new();
        esc(id, &mut frag);
        out.push_str(&format!("<li id=\"fnt-{frag}\">"));
        xhtml_blocks(&note.blocks, doc, out, toc);
        out.push_str(&format!(" <a href=\"#fnr-{frag}\">\u{21a9}</a></li>"));
    }
    out.push_str("</ol></aside>");
}

/// Inline CSS for a [`CharStyle`] (the result is XML-escaped by the caller when
/// emitted into an attribute).
fn xhtml_char_css(style: &CharStyle) -> String {
    let mut css = String::new();
    if !style.family.is_empty() {
        css.push_str(&format!("font-family:'{}'", style.family));
    }
    if style.size_pt > 0.0 {
        if !css.is_empty() {
            css.push(';');
        }
        css.push_str(&format!("font-size:{}pt", num(style.size_pt)));
    }
    if style.bold {
        css.push_str(";font-weight:bold");
    }
    if style.italic {
        css.push_str(";font-style:italic");
    }
    let mut decos = Vec::new();
    if style.underline {
        decos.push("underline");
    }
    if style.strike || style.dstrike {
        decos.push("line-through");
    }
    if !decos.is_empty() {
        css.push_str(&format!(";text-decoration:{}", decos.join(" ")));
        // A double strike keeps `line-through` (broad reader support) and
        // adds the style refinement for renderers that honour it.
        if style.dstrike {
            css.push_str(";text-decoration-style:double");
        }
    }
    if let Some(c) = markup_color(style) {
        css.push_str(&format!(";color:#{c}"));
    }
    // M2 CharStyle extensions — display-time transforms and metrics.
    if style.caps {
        css.push_str(";text-transform:uppercase");
    }
    if style.small_caps {
        css.push_str(";font-variant:small-caps");
    }
    if let Some(v) = style.baseline_shift_pt.filter(|v| *v != 0.0) {
        css.push_str(&format!(";vertical-align:{}pt", num(v)));
    }
    if style.letter_spacing_pt != 0.0 {
        css.push_str(&format!(";letter-spacing:{}pt", num(style.letter_spacing_pt)));
    }
    css.trim_start_matches(';').to_string()
}

fn xhtml_list(list: &List, doc: &Document, out: &mut String, toc: &mut EpubToc) {
    let tag = if list.ordered { "ol" } else { "ul" };
    let type_attr = if list.ordered {
        match list.marker {
            ListMarker::LowerAlpha => " type=\"a\"",
            ListMarker::UpperAlpha => " type=\"A\"",
            ListMarker::LowerRoman => " type=\"i\"",
            ListMarker::UpperRoman => " type=\"I\"",
            _ => "",
        }
    } else {
        ""
    };
    out.push_str(&format!("<{tag}{type_attr}>"));
    for item in &list.items {
        out.push_str("<li>");
        xhtml_blocks(&item.blocks, doc, out, toc);
        out.push_str("</li>");
    }
    out.push_str(&format!("</{tag}>"));
}

fn xhtml_table(table: &Table, doc: &Document, out: &mut String, toc: &mut EpubToc) {
    out.push_str("<table>");
    // Leading contiguous header rows → `<thead>` with `<th>` cells; the rest →
    // `<tbody>` with `<td>` cells. No header row ⇒ a single `<tbody>`.
    let head = table.rows.iter().take_while(|r| r.is_header).count();
    if head > 0 {
        out.push_str("<thead>");
        for row in &table.rows[..head] {
            xhtml_table_row(row, true, doc, out, toc);
        }
        out.push_str("</thead>");
    }
    out.push_str("<tbody>");
    for row in &table.rows[head..] {
        xhtml_table_row(row, row.is_header, doc, out, toc);
    }
    out.push_str("</tbody>");
    out.push_str("</table>");
}

/// Emit one `<tr>` of an EPUB table; `header` ⇒ `<th>` cells, else `<td>`.
fn xhtml_table_row(row: &Row, header: bool, doc: &Document, out: &mut String, toc: &mut EpubToc) {
    let tag = if header { "th" } else { "td" };
    out.push_str("<tr>");
    for cell in &row.cells {
        let mut attrs = String::new();
        if cell.col_span > 1 {
            attrs.push_str(&format!(" colspan=\"{}\"", cell.col_span));
        }
        if cell.row_span > 1 {
            attrs.push_str(&format!(" rowspan=\"{}\"", cell.row_span));
        }
        if let Some(rgb) = cell.shading {
            attrs.push_str(&format!(" style=\"background-color:#{}\"", hex(rgb)));
        }
        out.push_str(&format!("<{tag}{attrs}>"));
        xhtml_blocks(&cell.blocks, doc, out, toc);
        out.push_str(&format!("</{tag}>"));
    }
    out.push_str("</tr>");
}

fn xhtml_sheet(sheet: &Sheet, out: &mut String) {
    out.push_str("<table>");
    for row in &sheet.rows {
        out.push_str("<tr>");
        for cell in &row.cells {
            let style = match cell.fill {
                Some(rgb) => format!(" style=\"background-color:#{}\"", hex(rgb)),
                None => String::new(),
            };
            out.push_str(&format!("<td{style}>"));
            esc(&cell_display(&cell.value), out);
            out.push_str("</td>");
        }
        out.push_str("</tr>");
    }
    out.push_str("</table>");
}

/// An image as a relative-path `<img/>` referencing its embedded OEBPS file.
/// Resources absent from the table (or that resolve to no path) contribute
/// nothing, so a dangling reference never produces a broken element.
fn xhtml_image(img: &ImageRef, doc: &Document, out: &mut String) {
    let Some(res) = doc.resources.images.get(&img.resource) else {
        return;
    };
    let (_, ext) = epub_image_mime(&res.format);
    let path = format!("images/img-{}.{ext}", img.resource);
    let mut alt = String::new();
    esc(img.alt.as_deref().unwrap_or(""), &mut alt);
    out.push_str(&format!("<img src=\"{path}\" alt=\"{alt}\"/>"));
}

fn xhtml_shape(shape: &Shape, out: &mut String) {
    // Preserve the vector geometry as a self-contained inline `<svg>` (mirrors
    // the HTML exporter's `web::html_shape`): the path's bounds give a `viewBox`
    // (and `width`/`height` in points so it scales in reflow), the segments
    // become the `d` attribute, and the shape's paint maps to
    // `fill`/`stroke`/`stroke-width`/`stroke-dasharray`. PDF geometry is in user
    // space (origin bottom-left, Y up); SVG is top-left/Y down, so points are
    // translated to the bounds origin and flipped vertically.
    let Some((min_x, min_y, max_x, max_y)) = xhtml_shape_bounds(&shape.segments) else {
        // No drawable geometry (empty path or a single point): fall back to a
        // tiny bordered box carrying the fill colour so the shape isn't lost.
        let mut style =
            String::from("display:inline-block;width:1em;height:1em;border:1px solid #888");
        if let Some(rgb) = shape.fill {
            style.push_str(&format!(";background:#{}", hex(rgb)));
        }
        out.push_str(&format!("<span style=\"{style}\"></span>"));
        return;
    };
    let width = max_x - min_x;
    let height = max_y - min_y;

    let mut d = String::new();
    // (x, y) in PDF user space → (x - min_x, max_y - y) in SVG space.
    let pt = |x: f64, y: f64| format!("{} {}", num(x - min_x), num(max_y - y));
    for seg in &shape.segments {
        match *seg {
            PathSeg::Move(x, y) => d.push_str(&format!("M{} ", pt(x, y))),
            PathSeg::Line(x, y) => d.push_str(&format!("L{} ", pt(x, y))),
            PathSeg::Cubic(x1, y1, x2, y2, x3, y3) => {
                d.push_str(&format!("C{} {} {} ", pt(x1, y1), pt(x2, y2), pt(x3, y3)));
            }
            PathSeg::Close => d.push_str("Z "),
        }
    }
    let d = d.trim_end();

    let mut paint = format!(" fill=\"{}\"", xhtml_svg_fill(shape.fill));
    if let Some(stroke) = shape.stroke {
        paint.push_str(&format!(" stroke=\"#{}\"", hex(stroke)));
        if shape.stroke_width > 0.0 {
            paint.push_str(&format!(" stroke-width=\"{}\"", num(shape.stroke_width)));
        }
        if !shape.dash.is_empty() {
            let dashes: Vec<String> = shape.dash.iter().map(|v| num(*v)).collect();
            paint.push_str(&format!(" stroke-dasharray=\"{}\"", dashes.join(",")));
        }
    }

    out.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" \
viewBox=\"0 0 {vw} {vh}\" width=\"{vw}pt\" height=\"{vh}pt\" \
style=\"display:inline-block\"><path d=\"{d}\"{paint}/></svg>",
        vw = num(width.max(0.0)),
        vh = num(height.max(0.0)),
    ));
}

/// Axis-aligned bounding box `(min_x, min_y, max_x, max_y)` over every point of a
/// path (Bézier control points included). `None` when the path has no points or
/// is a single degenerate point (zero width *and* height) — neither yields a
/// renderable `<svg>` viewBox, so the caller falls back to a placeholder. Mirrors
/// `web::shape_bounds`.
fn xhtml_shape_bounds(segments: &[PathSeg]) -> Option<(f64, f64, f64, f64)> {
    let mut bounds: Option<(f64, f64, f64, f64)> = None;
    let mut add = |x: f64, y: f64| match &mut bounds {
        Some((min_x, min_y, max_x, max_y)) => {
            *min_x = min_x.min(x);
            *min_y = min_y.min(y);
            *max_x = max_x.max(x);
            *max_y = max_y.max(y);
        }
        None => bounds = Some((x, y, x, y)),
    };
    for seg in segments {
        match *seg {
            PathSeg::Move(x, y) | PathSeg::Line(x, y) => add(x, y),
            PathSeg::Cubic(x1, y1, x2, y2, x3, y3) => {
                add(x1, y1);
                add(x2, y2);
                add(x3, y3);
            }
            PathSeg::Close => {}
        }
    }
    match bounds {
        Some((min_x, min_y, max_x, max_y)) if max_x > min_x || max_y > min_y => {
            Some((min_x, min_y, max_x, max_y))
        }
        _ => None,
    }
}

/// The SVG `fill` attribute value: the shape's fill colour as `#RRGGBB`, or
/// `none` for a stroke-only (unfilled) shape so the path isn't filled black by
/// default. Mirrors `web::svg_fill`.
fn xhtml_svg_fill(fill: Option<[f64; 3]>) -> String {
    match fill {
        Some(rgb) => format!("#{}", hex(rgb)),
        None => "none".to_string(),
    }
}

// ──────────────────────── OPF / nav / NCX (EPUB metadata) ────────────────────

fn epub_opf(doc: &Document, chapters: &[EpubChapter], images: &[EpubImage], ident: &str) -> String {
    let lang = epub_lang(doc);

    let mut meta = String::new();
    {
        let title = doc
            .meta
            .title
            .clone()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "Document".to_string());
        meta.push_str("<dc:identifier id=\"pub-id\">");
        esc(ident, &mut meta);
        meta.push_str("</dc:identifier>");
        meta.push_str("<dc:title>");
        esc(&title, &mut meta);
        meta.push_str("</dc:title>");
        meta.push_str("<dc:language>");
        esc(&lang, &mut meta);
        meta.push_str("</dc:language>");
        if let Some(author) = doc.meta.author.as_ref().filter(|a| !a.is_empty()) {
            meta.push_str("<dc:creator>");
            esc(author, &mut meta);
            meta.push_str("</dc:creator>");
        }
        if let Some(subject) = doc.meta.subject.as_ref().filter(|s| !s.is_empty()) {
            meta.push_str("<dc:subject>");
            esc(subject, &mut meta);
            meta.push_str("</dc:subject>");
        }
        for kw in &doc.meta.keywords {
            if kw.is_empty() {
                continue;
            }
            meta.push_str("<dc:subject>");
            esc(kw, &mut meta);
            meta.push_str("</dc:subject>");
        }
        // EPUB 3 requires a dcterms:modified; a fixed epoch keeps output stable.
        meta.push_str("<meta property=\"dcterms:modified\">1980-01-01T00:00:00Z</meta>");
    }

    let mut manifest = String::new();
    manifest.push_str(
        "<item id=\"nav\" href=\"nav.xhtml\" media-type=\"application/xhtml+xml\" properties=\"nav\"/>",
    );
    manifest
        .push_str("<item id=\"ncx\" href=\"toc.ncx\" media-type=\"application/x-dtbncx+xml\"/>");
    for ch in chapters {
        manifest.push_str(&format!(
            "<item id=\"{}\" href=\"{}\" media-type=\"application/xhtml+xml\"/>",
            ch.id, ch.file
        ));
    }
    for img in images {
        manifest.push_str(&format!(
            "<item id=\"{}\" href=\"{}\" media-type=\"{}\"/>",
            img.id, img.path, img.media_type
        ));
    }

    let mut spine = String::new();
    for ch in chapters {
        spine.push_str(&format!("<itemref idref=\"{}\"/>", ch.id));
    }

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<package xmlns=\"http://www.idpf.org/2007/opf\" version=\"3.0\" \
unique-identifier=\"pub-id\" xml:lang=\"{lang}\">\
<metadata xmlns:dc=\"http://purl.org/dc/elements/1.1/\">{meta}</metadata>\
<manifest>{manifest}</manifest>\
<spine toc=\"ncx\">{spine}</spine>\
</package>",
        lang = {
            let mut s = String::new();
            esc(&lang, &mut s);
            s
        }
    )
}

fn epub_nav(doc: &Document, chapters: &[EpubChapter]) -> String {
    let lang = epub_lang(doc);
    // Each chapter is a top-level entry; its in-document headings nest beneath it
    // (H1→H2→H3…) as nested `<ol>`/`<li>`, with anchors resolving to the heading
    // ids emitted in the chapter XHTML.
    let mut list = String::new();
    for ch in chapters {
        list.push_str("<li><a href=\"");
        esc(&ch.file, &mut list);
        list.push_str("\">");
        esc(&ch.title, &mut list);
        list.push_str("</a>");
        nav_heading_tree(&ch.headings, &ch.file, &mut list);
        list.push_str("</li>");
    }
    let title = toc_title(&lang);
    let body = format!("<nav epub:type=\"toc\" id=\"toc\"><h1>{title}</h1><ol>{list}</ol></nav>");
    epub_xhtml_doc(&lang, title, &body)
}

/// The "Table of Contents" heading in the publication's language, keyed on the
/// primary subtag of a BCP-47 tag (`fr-FR`, `fr_CA` and `FR` all map to French).
/// Unknown or empty tags fall back to English. Every value is plain text with
/// no XML-special characters, so it can be emitted unescaped.
fn toc_title(lang: &str) -> &'static str {
    let primary = lang
        .split(['-', '_'])
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match primary.as_str() {
        "fr" => "Table des matières",
        "de" => "Inhaltsverzeichnis",
        "es" => "Índice",
        "it" => "Indice",
        "pt" => "Índice",
        "nl" => "Inhoudsopgave",
        "he" | "iw" => "תוכן העניינים",
        "ar" => "جدول المحتويات",
        "zh" => "目录",
        "ja" => "目次",
        "ru" => "Содержание",
        _ => "Table of Contents",
    }
}

/// A node in a chapter's heading hierarchy: a heading plus the deeper headings
/// nested under it. Built from the flat `(level, title, id)` list by
/// [`build_toc_tree`] so both the nav `<ol>` and the NCX `navPoint`s share one
/// unambiguous nesting.
struct TocNode<'a> {
    heading: &'a TocHeading,
    children: Vec<TocNode<'a>>,
}

/// Turn a chapter's flat, document-ordered heading list into a forest, nesting
/// by level: each heading becomes a child of the most recent heading with a
/// strictly smaller level (else a root). Tolerates skipped levels (H1→H3) and a
/// first heading that isn't the shallowest.
fn build_toc_tree(headings: &[TocHeading]) -> Vec<TocNode<'_>> {
    let mut roots: Vec<TocNode> = Vec::new();
    // Stack of indices identifying the path to the last-inserted node, so the
    // parent for the next heading can be found by popping levels >= its own.
    let mut path: Vec<usize> = Vec::new();

    for h in headings {
        // Pop until the node at the top of the path is a strict ancestor.
        while let Some(&idx) = path.last() {
            let level = node_at(&roots, &path[..path.len() - 1], idx).heading.level;
            if level >= h.level {
                path.pop();
            } else {
                break;
            }
        }
        let node = TocNode {
            heading: h,
            children: Vec::new(),
        };
        if let Some((&last, parents)) = path.split_last() {
            let parent = node_at_mut(&mut roots, parents, last);
            parent.children.push(node);
            let child_idx = parent.children.len() - 1;
            path.push(child_idx);
        } else {
            roots.push(node);
            path.push(roots.len() - 1);
        }
    }
    roots
}

/// Follow a path of child indices from the roots to a node (shared immutable
/// lookup used while resolving the insertion point).
fn node_at<'t, 'a>(roots: &'t [TocNode<'a>], path: &[usize], idx: usize) -> &'t TocNode<'a> {
    let mut nodes = roots;
    for &p in path {
        nodes = &nodes[p].children;
    }
    &nodes[idx]
}

/// Mutable counterpart of [`node_at`]: resolve the parent node at `path` so a new
/// child can be pushed.
fn node_at_mut<'t, 'a>(
    roots: &'t mut [TocNode<'a>],
    path: &[usize],
    idx: usize,
) -> &'t mut TocNode<'a> {
    let mut nodes = roots;
    for &p in path {
        nodes = &mut nodes[p].children;
    }
    &mut nodes[idx]
}

/// Emit a chapter's headings as a nested `<ol>` of `<li><a>` entries (nothing
/// when the chapter has no headings). Each anchor links to `file#id`, where `id`
/// is the anchor set on the heading in the XHTML.
fn nav_heading_tree(headings: &[TocHeading], file: &str, out: &mut String) {
    let tree = build_toc_tree(headings);
    if tree.is_empty() {
        return;
    }
    let mut file_esc = String::new();
    esc(file, &mut file_esc);
    nav_node_list(&tree, &file_esc, out);
}

/// Render a `<ol>` of the given heading nodes (recursing into children).
fn nav_node_list(nodes: &[TocNode], file_esc: &str, out: &mut String) {
    out.push_str("<ol>");
    for node in nodes {
        out.push_str("<li><a href=\"");
        out.push_str(file_esc);
        out.push('#');
        esc(&node.heading.id, out);
        out.push_str("\">");
        esc(&node.heading.title, out);
        out.push_str("</a>");
        if !node.children.is_empty() {
            nav_node_list(&node.children, file_esc, out);
        }
        out.push_str("</li>");
    }
    out.push_str("</ol>");
}

fn epub_ncx(doc: &Document, chapters: &[EpubChapter], ident: &str) -> String {
    let lang = epub_lang(doc);
    let title = doc
        .meta
        .title
        .clone()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "Document".to_string());

    // Build nested navPoints: a navPoint per chapter (depth 1) with the chapter's
    // headings nested beneath it (depth 2…). `play_order` is a single document-
    // wide counter incremented in reading order; `depth` tracks the deepest
    // nesting for the NCX `dtb:depth` head meta.
    let mut nav_points = String::new();
    let mut play_order = 0usize;
    let mut depth = 0usize;
    for ch in chapters {
        let mut file_esc = String::new();
        esc(&ch.file, &mut file_esc);
        let tree = build_toc_tree(&ch.headings);
        play_order += 1;
        nav_points.push_str(&format!(
            "<navPoint id=\"navpt-{play_order}\" playOrder=\"{play_order}\"><navLabel><text>"
        ));
        esc(&ch.title, &mut nav_points);
        nav_points.push_str("</text></navLabel><content src=\"");
        nav_points.push_str(&file_esc);
        nav_points.push_str("\"/>");
        // Chapter occupies depth 1; its heading subtree starts at depth 2.
        let sub = ncx_node_points(&tree, &file_esc, &mut play_order, 2, &mut nav_points);
        depth = depth.max(sub.max(1));
        nav_points.push_str("</navPoint>");
    }

    let mut title_esc = String::new();
    esc(&title, &mut title_esc);
    let mut lang_esc = String::new();
    esc(&lang, &mut lang_esc);
    let mut ident_esc = String::new();
    esc(ident, &mut ident_esc);
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<ncx xmlns=\"http://www.daisy.org/z3986/2005/ncx/\" version=\"2005-1\" xml:lang=\"{lang_esc}\">\
<head>\
<meta name=\"dtb:uid\" content=\"{ident_esc}\"/>\
<meta name=\"dtb:depth\" content=\"{depth}\"/>\
<meta name=\"dtb:totalPageCount\" content=\"0\"/>\
<meta name=\"dtb:maxPageNumber\" content=\"0\"/>\
</head>\
<docTitle><text>{title_esc}</text></docTitle>\
<navMap>{nav_points}</navMap>\
</ncx>",
        depth = depth.max(1)
    )
}

/// Emit nested `<navPoint>`s for the given heading nodes into `out` (the NCX
/// counterpart of [`nav_node_list`]). `play_order` is the shared running counter;
/// `level` is the current NCX nesting depth. Returns the deepest depth reached
/// (so the caller can compute `dtb:depth`); `0` when there are no nodes.
fn ncx_node_points(
    nodes: &[TocNode],
    file_esc: &str,
    play_order: &mut usize,
    level: usize,
    out: &mut String,
) -> usize {
    let mut deepest = 0usize;
    for node in nodes {
        *play_order += 1;
        out.push_str(&format!(
            "<navPoint id=\"navpt-{play_order}\" playOrder=\"{play_order}\"><navLabel><text>"
        ));
        esc(&node.heading.title, out);
        out.push_str("</text></navLabel><content src=\"");
        out.push_str(file_esc);
        out.push('#');
        esc(&node.heading.id, out);
        out.push_str("\"/>");
        let child_depth = ncx_node_points(&node.children, file_esc, play_order, level + 1, out);
        out.push_str("</navPoint>");
        // This node sits at `level`; its subtree may go deeper.
        deepest = deepest.max(level.max(child_depth));
    }
    deepest
}

// ═══════════════════════════════════ tests ═══════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::zip::read_zip;
    use crate::model::{
        Block, BlockKind, Cell, Document, Heading, InlineRun, List, ListItem, MergeRange, Page,
        Paragraph, Row, Section, Sheet, SheetBlock, SheetCell, SheetRow, Table,
    };

    fn run(text: &str) -> Inline {
        Inline::Run(InlineRun {
            text: text.to_string(),
            ..Default::default()
        })
    }

    fn para(text: &str) -> Block {
        Block {
            kind: BlockKind::Paragraph(Paragraph {
                runs: vec![run(text)],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// A document with a heading, a paragraph, a 2-item list, and a 2×2 table
    /// whose top-left cell spans both columns.
    fn sample_doc() -> Document {
        let heading = Block {
            kind: BlockKind::Heading(Heading {
                level: 1,
                para: Paragraph {
                    runs: vec![run("Title")],
                    ..Default::default()
                },
            }),
            ..Default::default()
        };
        let list = Block {
            kind: BlockKind::List(List {
                ordered: false,
                marker: ListMarker::Bullet('•'),
                items: vec![
                    ListItem {
                        blocks: vec![para("first")],
                        level: 0,
                        label: None,
                    },
                    ListItem {
                        blocks: vec![para("second")],
                        level: 0,
                        label: None,
                    },
                ],

                ..Default::default()
            }),
            ..Default::default()
        };
        let span_cell = Cell {
            blocks: vec![para("spanning")],
            col_span: 2,
            row_span: 1,
            shading: None,
            ..Cell::default()
        };
        let table = Block {
            kind: BlockKind::Table(Table {
                rows: vec![
                    Row {
                        cells: vec![span_cell],
                        height: None,
                        is_header: false,
                    },
                    Row {
                        cells: vec![
                            Cell {
                                blocks: vec![para("a")],
                                ..Default::default()
                            },
                            Cell {
                                blocks: vec![para("b")],
                                ..Default::default()
                            },
                        ],
                        height: None,
                        is_header: false,
                    },
                ],
                col_widths: vec![100.0, 100.0],
                border: crate::model::BorderStyle {
                    width: 1.0,
                    color: [0.0, 0.0, 0.0],
                },
            }),
            ..Default::default()
        };
        Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![heading, para("A paragraph."), list, table],
                    absolute: false,
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn entry(zip: &[u8], name: &str) -> Option<Vec<u8>> {
        read_zip(zip).remove(name)
    }

    #[test]
    fn docx_from_model_is_a_zip_with_flowing_structure() {
        let bytes = docx_from_model(&sample_doc());
        assert_eq!(&bytes[..2], b"PK", "valid zip");
        let doc = String::from_utf8(entry(&bytes, "word/document.xml").unwrap()).unwrap();
        // Heading-styled paragraph.
        assert!(
            doc.contains("<w:pStyle w:val=\"Heading1\"/>"),
            "heading uses Heading1 style"
        );
        assert!(doc.contains("Title"));
        // List numbering wired to numbering.xml.
        assert!(doc.contains("<w:numPr>"), "list paragraph has numbering");
        assert!(
            entry(&bytes, "word/numbering.xml").is_some(),
            "numbering part present"
        );
        // Real table with a gridSpan.
        assert!(doc.contains("<w:tbl>"), "table present");
        assert!(
            doc.contains("<w:gridSpan w:val=\"2\"/>"),
            "merged cell carries gridSpan"
        );
        // styles.xml defines the heading style.
        let styles = String::from_utf8(entry(&bytes, "word/styles.xml").unwrap()).unwrap();
        assert!(styles.contains("w:styleId=\"Heading1\""));
    }

    /// A table cell whose paragraph carries an [`Inline::LineBreak`] (the line
    /// structure `recon::tables` emits between a cell's physical lines, fix a4)
    /// exports as an **in-cell** break: DOCX renders `<w:br/>` inside the
    /// cell's single `<w:p>`, ODT `<text:line-break/>` inside its single
    /// `<text:p>` — a line break WITHIN the cell, never a second paragraph.
    #[test]
    fn cell_linebreak_exports_as_an_in_cell_break_not_a_new_paragraph() {
        let cell_para = Block {
            kind: BlockKind::Paragraph(Paragraph {
                runs: vec![run("ligne un"), Inline::LineBreak, run("ligne deux")],
                ..Default::default()
            }),
            ..Default::default()
        };
        let table = Block {
            kind: BlockKind::Table(Table {
                rows: vec![Row {
                    cells: vec![Cell {
                        blocks: vec![cell_para],
                        ..Default::default()
                    }],
                    height: None,
                    is_header: false,
                }],
                col_widths: vec![200.0],
                ..Default::default()
            }),
            ..Default::default()
        };
        let doc = Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![table],
                    absolute: false,
                }],
                ..Default::default()
            }],
            ..Default::default()
        };

        // DOCX: one paragraph in the cell, the break between the two lines.
        let bytes = docx_from_model(&doc);
        let xml = String::from_utf8(entry(&bytes, "word/document.xml").unwrap()).unwrap();
        let tc_start = xml.find("<w:tc>").expect("cell present");
        let tc_end = xml.find("</w:tc>").expect("cell closed");
        let tc = &xml[tc_start..tc_end];
        assert_eq!(
            tc.matches("<w:br/>").count(),
            1,
            "the LineBreak is an in-cell <w:br/>: {tc}"
        );
        let paras_in_cell = tc.matches("<w:p>").count() + tc.matches("<w:p ").count();
        assert_eq!(paras_in_cell, 1, "ONE paragraph in the cell, not two: {tc}");
        let (a, br, b) = (
            tc.find("ligne un").expect("first line text"),
            tc.find("<w:br/>").expect("break"),
            tc.find("ligne deux").expect("second line text"),
        );
        assert!(a < br && br < b, "break sits between the two lines");

        // ODT: same semantics — one <text:p>, an in-cell <text:line-break/>.
        let bytes = odt_from_model(&doc);
        let content = String::from_utf8(entry(&bytes, "content.xml").unwrap()).unwrap();
        let tc_start = content.find("<table:table-cell").expect("odt cell present");
        let tc_end = content
            .find("</table:table-cell>")
            .expect("odt cell closed");
        let tc = &content[tc_start..tc_end];
        assert_eq!(
            tc.matches("<text:line-break/>").count(),
            1,
            "the LineBreak is an in-cell <text:line-break/>: {tc}"
        );
        assert_eq!(
            tc.matches("<text:p").count(),
            1,
            "ONE ODT paragraph in the cell: {tc}"
        );
    }

    /// A document carrying a named style table (`"Body"` derived from `"Normal"`,
    /// with centred alignment + bold + an explicit size) and a single paragraph
    /// referencing it via `style_ref`.
    fn styled_doc() -> Document {
        let mut styles = StyleTable::default();
        styles.named.insert(
            StyleId("Body".to_string()),
            NamedStyle {
                para: ParagraphStyle {
                    align: Align::Center,
                    ..Default::default()
                },
                char_: CharStyle {
                    bold: true,
                    size_pt: 13.0,
                    ..Default::default()
                },
                based_on: Some(StyleId("Normal".to_string())),
            },
        );
        let p = Block {
            kind: BlockKind::Paragraph(Paragraph {
                style_ref: Some(StyleId("Body".to_string())),
                runs: vec![run("styled body text")],
                ..Default::default()
            }),
            ..Default::default()
        };
        Document {
            styles,
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![p],
                    absolute: false,
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn docx_named_style_table_emitted_and_referenced() {
        let bytes = docx_from_model(&styled_doc());

        // styles.xml defines the named style, based on Normal, with pPr/rPr.
        let styles = String::from_utf8(entry(&bytes, "word/styles.xml").unwrap()).unwrap();
        assert!(
            styles.contains("<w:style w:type=\"paragraph\" w:styleId=\"Body\">"),
            "named style declared: {styles}"
        );
        assert!(
            styles.contains("<w:name w:val=\"Body\"/>"),
            "style has a name"
        );
        assert!(
            styles.contains("<w:basedOn w:val=\"Normal\"/>"),
            "based_on → w:basedOn"
        );
        assert!(
            styles.contains("<w:jc w:val=\"center\"/>"),
            "paragraph formatting in pPr"
        );
        assert!(styles.contains("<w:b/>"), "char formatting in rPr");
        assert!(
            styles.contains("<w:sz w:val=\"26\"/>"),
            "explicit 13pt → 26 half-points in rPr"
        );
        // The built-in Normal/Heading styles are still present (no duplicate id).
        assert!(styles.contains("w:styleId=\"Normal\""));
        assert_eq!(
            styles.matches("w:styleId=\"Body\"").count(),
            1,
            "no duplicate styleId"
        );

        // The paragraph references the named style.
        let doc = String::from_utf8(entry(&bytes, "word/document.xml").unwrap()).unwrap();
        assert!(
            doc.contains("<w:pStyle w:val=\"Body\"/>"),
            "paragraph references the named style: {doc}"
        );

        // OPC invariants: styles.xml declared in Content_Types + the doc rels.
        let ct = String::from_utf8(entry(&bytes, "[Content_Types].xml").unwrap()).unwrap();
        assert!(
            ct.contains("PartName=\"/word/styles.xml\""),
            "styles.xml in [Content_Types].xml"
        );
        let rels =
            String::from_utf8(entry(&bytes, "word/_rels/document.xml.rels").unwrap()).unwrap();
        assert!(
            rels.contains("Target=\"styles.xml\""),
            "styles.xml relationship present"
        );
    }

    #[test]
    fn docx_named_style_does_not_duplicate_builtin_id() {
        // A model style reusing a reserved id (`Heading1`) must not be re-emitted.
        let mut d = styled_doc();
        d.styles.named.insert(
            StyleId("Heading1".to_string()),
            NamedStyle {
                char_: CharStyle {
                    italic: true,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let bytes = docx_from_model(&d);
        let styles = String::from_utf8(entry(&bytes, "word/styles.xml").unwrap()).unwrap();
        assert_eq!(
            styles.matches("w:styleId=\"Heading1\"").count(),
            1,
            "the built-in Heading1 is kept; the model duplicate is skipped"
        );
    }

    #[test]
    fn docx_header_footer_parts_emitted() {
        let mut d = sample_doc();
        d.sections[0].header = Some(vec![para("HEADER")]);
        d.sections[0].footer = Some(vec![para("FOOTER")]);
        let bytes = docx_from_model(&d);
        let hdr = String::from_utf8(entry(&bytes, "word/header1.xml").unwrap()).unwrap();
        assert!(hdr.contains("HEADER"));
        let ftr = String::from_utf8(entry(&bytes, "word/footer1.xml").unwrap()).unwrap();
        assert!(ftr.contains("FOOTER"));
        let doc = String::from_utf8(entry(&bytes, "word/document.xml").unwrap()).unwrap();
        assert!(doc.contains("<w:headerReference"));
        assert!(doc.contains("<w:footerReference"));
    }

    #[test]
    fn docx_two_sections_emit_two_sectpr_with_own_geometry_and_hf() {
        use crate::model::{Margins, PageGeometry};
        // Section 1: A4 portrait (595.276 × 841.89 pt → 11906 × 16838 twips) with a
        // running header. Section 2: US-Letter landscape (792 × 612 pt → 15840 ×
        // 12240 twips) with a running footer. Each must produce its own `w:sectPr`:
        // section 1's lives in the section-break paragraph's `w:pPr`, section 2's
        // is the final direct `w:body` child.
        let sec1 = Section {
            geometry: PageGeometry {
                width: 595.276,
                height: 841.89,
                margins: Margins::uniform(72.0),

                ..Default::default()
            },
            header: Some(vec![para("SEC1 HEADER")]),
            footer: None,
            pages: vec![Page {
                blocks: vec![para("section one body")],
                absolute: false,
            }],
            ..Default::default()
        };
        let sec2 = Section {
            geometry: PageGeometry {
                width: 792.0,
                height: 612.0,
                margins: Margins::uniform(36.0),

                ..Default::default()
            },
            header: None,
            footer: Some(vec![para("SEC2 FOOTER")]),
            pages: vec![Page {
                blocks: vec![para("section two body")],
                absolute: false,
            }],
            ..Default::default()
        };
        let doc = Document {
            sections: vec![sec1, sec2],
            ..Default::default()
        };
        let bytes = docx_from_model(&doc);
        let xml = String::from_utf8(entry(&bytes, "word/document.xml").unwrap()).unwrap();

        // Exactly two section properties total.
        assert_eq!(
            xml.matches("<w:sectPr>").count(),
            2,
            "one w:sectPr per section: {xml}"
        );
        // Section 1's sectPr rides a section-break paragraph's pPr; section 2's is
        // the trailing body-level sectPr.
        assert!(
            xml.contains("<w:p><w:pPr><w:sectPr>"),
            "first section terminated by a sectPr paragraph: {xml}"
        );
        // Each section keeps its own page size + orientation.
        assert!(
            xml.contains("<w:pgSz w:w=\"11906\" w:h=\"16838\" w:orient=\"portrait\"/>"),
            "section 1 A4 portrait pgSz: {xml}"
        );
        assert!(
            xml.contains("<w:pgSz w:w=\"15840\" w:h=\"12240\" w:orient=\"landscape\"/>"),
            "section 2 US-Letter landscape pgSz: {xml}"
        );
        // Per-section running header/footer references.
        assert!(
            xml.contains("<w:headerReference w:type=\"default\" r:id=\"rIdHdr1\"/>"),
            "section 1 references its own header part: {xml}"
        );
        assert!(
            xml.contains("<w:footerReference w:type=\"default\" r:id=\"rIdFtr2\"/>"),
            "section 2 references its own footer part: {xml}"
        );
        // Distinct header/footer parts exist, one per owning section.
        let hdr = String::from_utf8(entry(&bytes, "word/header1.xml").unwrap()).unwrap();
        assert!(hdr.contains("SEC1 HEADER"), "section 1 header part: {hdr}");
        let ftr = String::from_utf8(entry(&bytes, "word/footer2.xml").unwrap()).unwrap();
        assert!(ftr.contains("SEC2 FOOTER"), "section 2 footer part: {ftr}");
        assert!(
            entry(&bytes, "word/footer1.xml").is_none(),
            "no spurious footer1 part"
        );
        assert!(
            entry(&bytes, "word/header2.xml").is_none(),
            "no spurious header2 part"
        );
        // Content-types + rels declare the right parts.
        let ct = String::from_utf8(entry(&bytes, "[Content_Types].xml").unwrap()).unwrap();
        assert!(
            ct.contains("/word/header1.xml"),
            "header1 content-type override"
        );
        assert!(
            ct.contains("/word/footer2.xml"),
            "footer2 content-type override"
        );
        let rels =
            String::from_utf8(entry(&bytes, "word/_rels/document.xml.rels").unwrap()).unwrap();
        assert!(
            rels.contains("Id=\"rIdHdr1\""),
            "rIdHdr1 relationship: {rels}"
        );
        assert!(
            rels.contains("Id=\"rIdFtr2\""),
            "rIdFtr2 relationship: {rels}"
        );
    }

    /// A paragraph whose single run carries a yellow highlight.
    fn highlighted_para(text: &str, bg: [f64; 3]) -> Block {
        Block {
            kind: BlockKind::Paragraph(Paragraph {
                runs: vec![Inline::Run(InlineRun {
                    text: text.to_string(),
                    style: CharStyle {
                        background: Some(bg),
                        ..Default::default()
                    },
                    ..Default::default()
                })],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn doc_with(block: Block) -> Document {
        Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![block],
                    absolute: false,
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn docx_run_highlight_emits_w_shd_fill() {
        // A run with `CharStyle.background` exports `<w:shd w:fill>` run shading;
        // a plain run (no background) emits no run shading at all.
        let bytes = docx_from_model(&doc_with(highlighted_para("lit", [1.0, 1.0, 0.0])));
        let xml = String::from_utf8(entry(&bytes, "word/document.xml").unwrap()).unwrap();
        assert!(
            xml.contains("<w:shd w:val=\"clear\" w:color=\"auto\" w:fill=\"FFFF00\"/>"),
            "yellow highlight → run shading: {xml}"
        );

        let plain = docx_from_model(&doc_with(para("plain")));
        let plain_xml = String::from_utf8(entry(&plain, "word/document.xml").unwrap()).unwrap();
        // The only `w:shd` baked into a plain doc is the code-block paragraph
        // shading; a plain text run must carry none.
        assert!(
            !plain_xml.contains("w:shd"),
            "plain run has no run shading: {plain_xml}"
        );
    }

    #[test]
    fn docx_highlight_round_trips_through_import() {
        // Export a highlighted run to DOCX, re-import it, and confirm the model
        // recovers the same background — the export/import are true inverses.
        let bytes = docx_from_model(&doc_with(highlighted_para("marked", [0.0, 1.0, 0.0])));
        let model = crate::convert::office_import::office_to_model(&bytes).expect("docx → model");
        let run = model
            .sections
            .iter()
            .flat_map(|s| s.pages.iter())
            .flat_map(|p| p.blocks.iter())
            .find_map(|b| match &b.kind {
                BlockKind::Paragraph(p) => p.runs.iter().find_map(|i| match i {
                    Inline::Run(r) if r.text == "marked" => Some(r.clone()),
                    _ => None,
                }),
                _ => None,
            })
            .expect("the 'marked' run survived the round-trip");
        assert_eq!(
            run.style.background,
            Some([0.0, 1.0, 0.0]),
            "green highlight round-trips through DOCX"
        );
    }

    #[test]
    fn odt_run_highlight_emits_fo_background_color() {
        // A run with `CharStyle.background` exports `fo:background-color` on its
        // text style; a plain run emits none.
        let bytes = odt_from_model(&doc_with(highlighted_para("lit", [1.0, 1.0, 0.0])));
        let content = String::from_utf8(entry(&bytes, "content.xml").unwrap()).unwrap();
        assert!(
            content.contains("fo:background-color=\"#FFFF00\""),
            "yellow highlight → fo:background-color: {content}"
        );

        let plain = odt_from_model(&doc_with(para("plain")));
        let plain_content = String::from_utf8(entry(&plain, "content.xml").unwrap()).unwrap();
        // ODF code blocks carry a paragraph background; a plain text-run style
        // must not. Check the run-level text style specifically stays clean by
        // confirming no `fo:background-color` appears for this plain paragraph.
        assert!(
            !plain_content.contains("fo:background-color"),
            "plain run has no run background: {plain_content}"
        );
    }

    /// A one-slide deck whose Title placeholder is `block` — the smallest
    /// `Document` that exercises the PPTX/ODP slide run writers.
    fn slide_doc_with(block: Block) -> Document {
        use crate::model::{Placeholder, PlaceholderRole, Slide, SlideBlock};
        let slide = Slide {
            geometry: crate::model::PageGeometry {
                width: 960.0,
                height: 540.0,
                margins: crate::model::Margins::uniform(0.0),

                ..Default::default()
            },
            shapes: Vec::new(),
            placeholders: vec![Placeholder {
                role: PlaceholderRole::Title,
                block,
            }],
            notes: None,
            background: None,
            ..Default::default()
        };
        Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![Block {
                        kind: BlockKind::Slide(SlideBlock {
                            slides: vec![slide],
                        }),
                        ..Default::default()
                    }],
                    absolute: true,
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn pptx_run_highlight_emits_a_highlight() {
        // #24: a run carrying `CharStyle.background` exports `a:highlight` in its
        // `a:rPr` (ECMA-376 §21.1.2.3.9) — the PPTX analogue of DOCX `w:shd` and
        // ODF `fo:background-color`. A run with colour + family + highlight must
        // emit them in the `CT_TextCharacterProperties` order
        // `a:solidFill` → `a:highlight` → `a:latin`. A plain run emits no
        // `a:highlight`.
        let block = Block {
            kind: BlockKind::Paragraph(Paragraph {
                runs: vec![Inline::Run(InlineRun {
                    text: "lit".to_string(),
                    style: CharStyle {
                        family: "Arial".to_string(),
                        color: Some([1.0, 0.0, 0.0]),
                        background: Some([1.0, 1.0, 0.0]),
                        ..Default::default()
                    },
                    ..Default::default()
                })],
                ..Default::default()
            }),
            ..Default::default()
        };
        let bytes = pptx_from_model(&slide_doc_with(block));
        let s = String::from_utf8(entry(&bytes, "ppt/slides/slide1.xml").unwrap()).unwrap();

        assert!(
            s.contains("<a:highlight><a:srgbClr val=\"FFFF00\"/></a:highlight>"),
            "yellow highlight → a:highlight: {s}"
        );
        // The run text and its other rPr props survive alongside the highlight.
        assert!(s.contains("<a:t>lit</a:t>"), "run text preserved: {s}");
        assert!(
            s.contains("<a:solidFill><a:srgbClr val=\"FF0000\"/></a:solidFill>"),
            "run colour preserved: {s}"
        );
        assert!(
            s.contains("<a:latin typeface=\"Arial\"/>"),
            "run family preserved: {s}"
        );
        // Schema child order: fill, then highlight, then latin.
        let fill = s.find("<a:solidFill>").expect("solidFill present");
        let hi = s.find("<a:highlight>").expect("highlight present");
        let latin = s.find("<a:latin").expect("latin present");
        assert!(
            fill < hi && hi < latin,
            "rPr child order solidFill < highlight < latin: {s}"
        );

        // A plain run (no background) carries no highlight at all.
        let plain = pptx_from_model(&slide_doc_with(para("plain")));
        let plain_s = String::from_utf8(entry(&plain, "ppt/slides/slide1.xml").unwrap()).unwrap();
        assert!(
            !plain_s.contains("a:highlight"),
            "plain run has no highlight: {plain_s}"
        );
    }

    #[test]
    fn odp_run_highlight_emits_fo_background_color() {
        // #24 (ODP side): a slide run's `CharStyle.background` already exports
        // `fo:background-color` on its text auto-style (shared `odf_span_style`
        // with ODT). Locked here so the slide path cannot silently regress.
        let bytes = odp_from_model(&slide_doc_with(highlighted_para("lit", [1.0, 1.0, 0.0])));
        let content = String::from_utf8(entry(&bytes, "content.xml").unwrap()).unwrap();
        assert!(
            content.contains("fo:background-color=\"#FFFF00\""),
            "yellow highlight → fo:background-color on slide run: {content}"
        );

        let plain = odp_from_model(&slide_doc_with(para("plain")));
        let plain_content = String::from_utf8(entry(&plain, "content.xml").unwrap()).unwrap();
        assert!(
            !plain_content.contains("fo:background-color"),
            "plain slide run has no run background: {plain_content}"
        );
    }

    #[test]
    fn docx_row_span_emits_vmerge_restart_and_continue() {
        // A 2-column, 2-row table whose top-left cell spans both rows, expressed
        // by the lower row supplying only the right-hand cell.
        let table = Block {
            kind: BlockKind::Table(Table {
                rows: vec![
                    Row {
                        cells: vec![
                            Cell {
                                blocks: vec![para("tall")],
                                row_span: 2,
                                ..Default::default()
                            },
                            Cell {
                                blocks: vec![para("r0c1")],
                                ..Default::default()
                            },
                        ],
                        height: None,
                        is_header: false,
                    },
                    Row {
                        // Only the right cell; the left is covered by the row span.
                        cells: vec![Cell {
                            blocks: vec![para("r1c1")],
                            ..Default::default()
                        }],
                        height: None,
                        is_header: false,
                    },
                ],
                col_widths: vec![100.0, 100.0],
                border: crate::model::BorderStyle::default(),
            }),
            ..Default::default()
        };
        let doc = Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![table],
                    absolute: false,
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let bytes = docx_from_model(&doc);
        let xml = String::from_utf8(entry(&bytes, "word/document.xml").unwrap()).unwrap();
        assert!(
            xml.contains("<w:vMerge w:val=\"restart\"/>"),
            "row-span originator restarts the merge"
        );
        assert!(
            xml.contains("<w:vMerge/>"),
            "second row emits a vMerge continuation cell"
        );
    }

    #[test]
    fn html_from_model_is_semantic() {
        let html = crate::convert::web::html_from_model(&sample_doc());
        assert!(html.contains("<h1>"), "heading → h1");
        assert!(html.contains("Title"));
        assert!(html.contains("<p>"), "paragraph → p");
        assert!(
            html.contains("<ul>") && html.contains("<li>"),
            "list → ul/li"
        );
        assert!(html.contains("<table"), "table → table");
        assert!(html.contains("colspan=\"2\""), "merged cell colspan");
    }

    #[test]
    fn xlsx_from_model_stores_numbers_and_merges() {
        let doc = Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![Block {
                        kind: BlockKind::Sheet(SheetBlock {
                            sheets: vec![Sheet {
                                name: "Data".to_string(),
                                rows: vec![
                                    SheetRow {
                                        cells: vec![
                                            SheetCell {
                                                value: CellValue::Text("Label".to_string()),
                                                ..Default::default()
                                            },
                                            SheetCell {
                                                value: CellValue::Number(42.5),
                                                ..Default::default()
                                            },
                                        ],
                                        ..Default::default()
                                    },
                                    SheetRow {
                                        cells: vec![SheetCell {
                                            value: CellValue::Number(7.0),
                                            fill: Some([1.0, 1.0, 0.0]),
                                            ..Default::default()
                                        }],
                                        ..Default::default()
                                    },
                                ],
                                merges: vec![MergeRange {
                                    r0: 0,
                                    c0: 0,
                                    r1: 0,
                                    c1: 1,
                                }],
                                col_widths: Vec::new(),
                                ..Default::default()
                            }],
                        }),
                        ..Default::default()
                    }],
                    absolute: false,
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let bytes = xlsx_from_model(&doc);
        assert_eq!(&bytes[..2], b"PK");
        let sheet = String::from_utf8(entry(&bytes, "xl/worksheets/sheet1.xml").unwrap()).unwrap();
        // The number is stored as a numeric <v>, NOT an inlineStr.
        assert!(
            sheet.contains("<v>42.5</v>"),
            "number stored numeric: {sheet}"
        );
        assert!(
            !sheet.contains("42.5</t>"),
            "number must not be an inline string"
        );
        // The text cell is an inline string.
        assert!(sheet.contains("Label"));
        // A merged range is present.
        assert!(
            sheet.contains("<mergeCell ref=\"A1:B1\"/>"),
            "merge present"
        );
        // The filled cell references a non-default style.
        assert!(sheet.contains(" s=\""), "styled fill cell");
        let styles = String::from_utf8(entry(&bytes, "xl/styles.xml").unwrap()).unwrap();
        assert!(styles.contains("patternType=\"solid\""), "fill defined");
    }

    /// A document with NO native `Block::Sheet` content (a reconstructed PDF, a
    /// Word/HTML import…) synthesizes one worksheet per page from its flowing
    /// blocks: typed tables become real rows/cells (spans merged), and every
    /// prose block — heading, paragraph, list item — lands as a single-cell row
    /// in reading order, so no document text is lost on the spreadsheet path.
    #[test]
    fn xlsx_from_model_synthesizes_sheets_from_flowing_pages() {
        let bytes = xlsx_from_model(&sample_doc());
        assert_eq!(&bytes[..2], b"PK");
        let workbook = String::from_utf8(entry(&bytes, "xl/workbook.xml").unwrap()).unwrap();
        assert!(
            workbook.contains("Page 1"),
            "per-page sheet name: {workbook}"
        );
        let sheet = String::from_utf8(entry(&bytes, "xl/worksheets/sheet1.xml").unwrap()).unwrap();
        for probe in [
            "Title",
            "A paragraph.",
            "first",
            "second",
            "spanning",
            "a",
            "b",
        ] {
            assert!(sheet.contains(probe), "sheet carries {probe:?}: {sheet}");
        }
        // The bullet marker prefixes each synthesized list row.
        assert!(sheet.contains("• first"), "list marker kept: {sheet}");
        // The 2-wide top-left table cell becomes a real merged range.
        assert!(
            sheet.contains("<mergeCell"),
            "table col_span synthesized as a merge: {sheet}"
        );
    }

    /// Same synthesis on the ODS side — the full flowing text must land in
    /// `content.xml`.
    #[test]
    fn ods_from_model_synthesizes_sheets_from_flowing_pages() {
        let bytes = ods_from_model(&sample_doc());
        let content = String::from_utf8(entry(&bytes, "content.xml").unwrap()).unwrap();
        for probe in ["Title", "A paragraph.", "first", "spanning"] {
            assert!(content.contains(probe), "ods carries {probe:?}");
        }
        assert!(content.contains("Page 1"), "per-page table name");
    }

    /// A one-row spreadsheet `Document` whose cells carry the given values and
    /// vertical alignments, for the export-side alignment tests.
    fn sheet_doc_with_valigns(cells: Vec<(CellValue, Option<CellVAlign>)>) -> Document {
        let row = SheetRow {
            cells: cells
                .into_iter()
                .map(|(value, va)| SheetCell {
                    value,
                    vertical_align: va,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![Block {
                        kind: BlockKind::Sheet(SheetBlock {
                            sheets: vec![Sheet {
                                name: "S".to_string(),
                                rows: vec![row],
                                merges: Vec::new(),
                                col_widths: Vec::new(),
                                ..Default::default()
                            }],
                        }),
                        ..Default::default()
                    }],
                    absolute: false,
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn xlsx_from_model_emits_cell_vertical_align() {
        // `SheetCell.vertical_align` becomes `<alignment vertical="...">` in the
        // cell xf (top/center/bottom); a cell with `None` emits no vertical
        // attribute (the OOXML default — bottom).
        let doc = sheet_doc_with_valigns(vec![
            (CellValue::Text("def".to_string()), None),
            (CellValue::Text("top".to_string()), Some(CellVAlign::Top)),
            (CellValue::Text("mid".to_string()), Some(CellVAlign::Middle)),
            (CellValue::Text("bot".to_string()), Some(CellVAlign::Bottom)),
        ]);
        let bytes = xlsx_from_model(&doc);
        let styles = String::from_utf8(entry(&bytes, "xl/styles.xml").unwrap()).unwrap();
        assert!(
            styles.contains("vertical=\"top\""),
            "Top → vertical=top: {styles}"
        );
        assert!(
            styles.contains("vertical=\"center\""),
            "Middle → vertical=center: {styles}"
        );
        assert!(
            styles.contains("vertical=\"bottom\""),
            "Bottom → vertical=bottom: {styles}"
        );
        // The first (default) cell carries no styled xf for vertical alignment:
        // exactly the three explicit anchors appear in the stylesheet.
        assert_eq!(
            styles.matches("vertical=\"").count(),
            3,
            "absent vertical_align emits no attribute: {styles}"
        );
    }

    #[test]
    fn ods_from_model_emits_cell_vertical_align() {
        // `SheetCell.vertical_align` → `style:table-cell-properties
        // @style:vertical-align="middle"` (ODF spells the centre value `middle`).
        let doc = sheet_doc_with_valigns(vec![(
            CellValue::Text("mid".to_string()),
            Some(CellVAlign::Middle),
        )]);
        let content =
            String::from_utf8(entry(&ods_from_model(&doc), "content.xml").unwrap()).unwrap();
        assert!(
            content.contains("style:vertical-align=\"middle\""),
            "Middle → style:vertical-align=middle: {content}"
        );
    }

    /// A one-cell sheet doc carrying a given cell `direction`, for the RTL
    /// reading-order export tests (item f).
    fn sheet_doc_with_direction(dir: crate::model::Direction) -> Document {
        Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![Block {
                        kind: BlockKind::Sheet(SheetBlock {
                            sheets: vec![Sheet {
                                name: "S".to_string(),
                                rows: vec![SheetRow {
                                    cells: vec![SheetCell {
                                        value: CellValue::Text("שלום".to_string()),
                                        direction: Some(dir),
                                        ..Default::default()
                                    }],
                                    ..Default::default()
                                }],
                                ..Default::default()
                            }],
                        }),
                        ..Default::default()
                    }],
                    absolute: false,
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn xlsx_from_model_emits_rtl_reading_order() {
        // `SheetCell.direction = Rtl` → `<alignment readingOrder="2">` (item f).
        let doc = sheet_doc_with_direction(crate::model::Direction::Rtl);
        let styles = String::from_utf8(entry(&xlsx_from_model(&doc), "xl/styles.xml").unwrap())
            .unwrap();
        assert!(
            styles.contains("readingOrder=\"2\""),
            "Rtl → readingOrder=2: {styles}"
        );
    }

    #[test]
    fn ods_from_model_emits_rtl_writing_mode() {
        // `SheetCell.direction = Rtl` → `style:writing-mode="rl-tb"` (item f).
        let doc = sheet_doc_with_direction(crate::model::Direction::Rtl);
        let content =
            String::from_utf8(entry(&ods_from_model(&doc), "content.xml").unwrap()).unwrap();
        assert!(
            content.contains("style:writing-mode=\"rl-tb\""),
            "Rtl → writing-mode=rl-tb: {content}"
        );
    }

    #[test]
    fn odt_from_model_emits_table_cell_vertical_align() {
        // A model table `Cell.vertical_align` becomes an ODT cell auto-style with
        // `style:table-cell-properties@style:vertical-align`.
        let cell = Cell {
            blocks: vec![para("Bot")],
            vertical_align: Some(CellVAlign::Bottom),
            ..Cell::default()
        };
        let doc = Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![Block {
                        kind: BlockKind::Table(Table {
                            rows: vec![crate::model::Row {
                                cells: vec![cell],
                                height: None,
                                is_header: false,
                            }],
                            col_widths: Vec::new(),
                            border: BorderStyle::default(),
                        }),
                        ..Default::default()
                    }],
                    absolute: false,
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let content =
            String::from_utf8(entry(&odt_from_model(&doc), "content.xml").unwrap()).unwrap();
        assert!(
            content.contains("style:vertical-align=\"bottom\""),
            "Bottom → style:vertical-align=bottom: {content}"
        );
    }

    /// A one-table document whose first row is a header (`is_header == true`) and
    /// whose second row is a body row, used by the header-row export tests.
    fn header_table_doc() -> Document {
        let cell = |t: &str| Cell {
            blocks: vec![para(t)],
            ..Cell::default()
        };
        Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![Block {
                        kind: BlockKind::Table(Table {
                            rows: vec![
                                Row {
                                    cells: vec![cell("H1"), cell("H2")],
                                    height: None,
                                    is_header: true,
                                },
                                Row {
                                    cells: vec![cell("D1"), cell("D2")],
                                    height: None,
                                    is_header: false,
                                },
                            ],
                            col_widths: vec![100.0, 100.0],
                            border: BorderStyle::default(),
                        }),
                        ..Default::default()
                    }],
                    absolute: false,
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// A header row exports as DOCX `w:trPr/w:tblHeader`; the body row carries no
    /// `w:tblHeader`.
    #[test]
    fn docx_from_model_emits_tbl_header_for_header_row() {
        let xml = String::from_utf8(
            entry(&docx_from_model(&header_table_doc()), "word/document.xml").unwrap(),
        )
        .unwrap();
        assert!(
            xml.contains("<w:trPr><w:tblHeader/></w:trPr>"),
            "header row → w:tblHeader: {xml}"
        );
        assert_eq!(
            xml.matches("<w:tblHeader/>").count(),
            1,
            "only the header row carries w:tblHeader: {xml}"
        );
    }

    /// Header rows export as an ODT `table:table-header-rows` wrapper around the
    /// leading header row(s); the body row sits after it.
    #[test]
    fn odt_from_model_wraps_header_rows() {
        let content =
            String::from_utf8(entry(&odt_from_model(&header_table_doc()), "content.xml").unwrap())
                .unwrap();
        let open = content
            .find("<table:table-header-rows>")
            .expect("header-rows wrapper present");
        let close = content
            .find("</table:table-header-rows>")
            .expect("header-rows wrapper closed");
        assert!(open < close, "wrapper well-formed");
        // The header cell falls inside the wrapper; the body cell after it.
        let h1 = content.find("H1").expect("H1 present");
        let d1 = content.find("D1").expect("D1 present");
        assert!(open < h1 && h1 < close, "H1 inside header-rows: {content}");
        assert!(d1 > close, "D1 after header-rows: {content}");
    }

    /// A header row exports as EPUB `<thead>` with `<th>` cells; the body row sits
    /// in `<tbody>` with `<td>` cells.
    #[test]
    fn epub_from_model_emits_thead_and_th_for_header_row() {
        let bytes = epub_from_model(&header_table_doc());
        let chap = String::from_utf8(entry(&bytes, "OEBPS/text-1.xhtml").unwrap()).unwrap();
        // Cell content is wrapped in a `<p>`; assert the cell tags, not bare text.
        assert!(chap.contains("<thead>"), "header rows in <thead>: {chap}");
        assert!(
            chap.contains("<th><p>H1</p></th>"),
            "header cell is <th>: {chap}"
        );
        assert!(chap.contains("<tbody>"), "body rows in <tbody>: {chap}");
        assert!(
            chap.contains("<td><p>D1</p></td>"),
            "body cell is <td>: {chap}"
        );
        assert!(!chap.contains("<th><p>D1"), "body cell is not <th>: {chap}");
    }

    #[test]
    fn pptx_from_model_emits_slide_table_cell_anchor() {
        // A slide-table `Cell.vertical_align` becomes `a:tcPr@anchor` (t/ctr/b); a
        // `None` cell emits a bare `<a:tcPr/>` (no anchor).
        use crate::model::{Slide, SlideBlock};
        let mk = |text: &str, va: Option<CellVAlign>| Cell {
            blocks: vec![para(text)],
            vertical_align: va,
            ..Cell::default()
        };
        let table = Block {
            kind: BlockKind::Table(Table {
                rows: vec![crate::model::Row {
                    cells: vec![mk("Mid", Some(CellVAlign::Middle)), mk("Def", None)],
                    height: None,
                    is_header: false,
                }],
                col_widths: Vec::new(),
                border: BorderStyle::default(),
            }),
            ..Default::default()
        };
        let slide = Slide {
            geometry: crate::model::PageGeometry {
                width: 960.0,
                height: 540.0,
                margins: crate::model::Margins::uniform(0.0),

                ..Default::default()
            },
            shapes: vec![table],
            placeholders: Vec::new(),
            notes: None,
            background: None,
            ..Default::default()
        };
        let doc = Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![Block {
                        kind: BlockKind::Slide(SlideBlock {
                            slides: vec![slide],
                        }),
                        ..Default::default()
                    }],
                    absolute: true,
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let s = String::from_utf8(entry(&pptx_from_model(&doc), "ppt/slides/slide1.xml").unwrap())
            .unwrap();
        // The "Mid" cell has no shading, so its tcPr is self-closing with anchor.
        assert!(
            s.contains("<a:tcPr anchor=\"ctr\"/>"),
            "Middle → a:tcPr anchor=ctr: {s}"
        );
        assert!(
            s.contains("<a:tcPr/>"),
            "absent vertical_align → bare a:tcPr (no anchor): {s}"
        );
    }

    #[test]
    fn pptx_from_model_has_one_slide_per_slide() {
        use crate::model::{Placeholder, PlaceholderRole, Slide, SlideBlock};
        let slide = Slide {
            geometry: crate::model::PageGeometry {
                width: 960.0,
                height: 540.0,
                margins: crate::model::Margins::uniform(0.0),

                ..Default::default()
            },
            shapes: Vec::new(),
            placeholders: vec![Placeholder {
                role: PlaceholderRole::Title,
                block: para("Slide title"),
            }],
            notes: None,
            background: None,
            ..Default::default()
        };
        let doc = Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![Block {
                        kind: BlockKind::Slide(SlideBlock {
                            slides: vec![slide],
                        }),
                        ..Default::default()
                    }],
                    absolute: true,
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let bytes = pptx_from_model(&doc);
        assert_eq!(&bytes[..2], b"PK");
        let s = String::from_utf8(entry(&bytes, "ppt/slides/slide1.xml").unwrap()).unwrap();
        assert!(s.contains("<p:ph type=\"title\""), "title placeholder");
        assert!(s.contains("Slide title"));
    }

    /// Build a `Document` carrying a single `SlideBlock` with `n` blank slides.
    fn slide_deck_doc(n: usize) -> Document {
        use crate::model::{Slide, SlideBlock};
        let slides = (0..n)
            .map(|i| Slide {
                geometry: crate::model::PageGeometry {
                    width: 960.0,
                    height: 540.0,
                    margins: crate::model::Margins::uniform(0.0),

                    ..Default::default()
                },
                shapes: Vec::new(),
                placeholders: vec![crate::model::Placeholder {
                    role: crate::model::PlaceholderRole::Title,
                    block: para(&format!("Slide {}", i + 1)),
                }],
                notes: None,
                background: None,
                ..Default::default()
            })
            .collect();
        Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![Block {
                        kind: BlockKind::Slide(SlideBlock { slides }),
                        ..Default::default()
                    }],
                    absolute: true,
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// Resolve an OPC relationship `Target` (relative to the source part's
    /// directory) into an absolute package part name, collapsing `..`/`.`.
    fn opc_resolve(rels_part: &str, target: &str) -> String {
        // The base directory is the source part's directory, i.e. the `.rels`
        // path with the trailing `_rels/<name>.rels` removed.
        let base = rels_part
            .rsplit_once("_rels/")
            .map(|(dir, _)| dir.trim_end_matches('/'))
            .unwrap_or("");
        let mut stack: Vec<&str> = if base.is_empty() {
            Vec::new()
        } else {
            base.split('/').collect()
        };
        for seg in target.split('/') {
            match seg {
                "" | "." => {}
                ".." => {
                    stack.pop();
                }
                s => stack.push(s),
            }
        }
        stack.join("/")
    }

    /// Collect every `Target="…"` attribute value from a `.rels` part body.
    fn opc_targets(body: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = body;
        while let Some(p) = rest.find("Target=\"") {
            rest = &rest[p + 8..];
            if let Some(end) = rest.find('"') {
                out.push(rest[..end].to_string());
                rest = &rest[end + 1..];
            } else {
                break;
            }
        }
        out
    }

    /// Assert the structural OPC invariants of a presentation package: every
    /// relationship `Target` resolves to a part that exists *and* is declared in
    /// `[Content_Types].xml` (by `<Override>` part name or `<Default>`
    /// extension). Internal (non-`External`) targets only.
    fn assert_pptx_opc_invariants(bytes: &[u8]) {
        let parts = read_zip(bytes);
        let ct = String::from_utf8(parts.get("[Content_Types].xml").unwrap().clone()).unwrap();
        let declared = |part: &str| -> bool {
            if ct.contains(&format!("PartName=\"/{part}\"")) {
                return true;
            }
            match part.rsplit_once('.') {
                Some((_, ext)) => ct.contains(&format!("Extension=\"{ext}\"")),
                None => false,
            }
        };
        for (name, data) in &parts {
            if !name.ends_with(".rels") {
                assert!(
                    declared(name),
                    "part {name} not declared in [Content_Types].xml"
                );
                continue;
            }
            let body = String::from_utf8(data.clone()).unwrap();
            for target in opc_targets(&body) {
                if target.starts_with("http://") || target.starts_with("https://") {
                    continue; // external relationship
                }
                let resolved = opc_resolve(name, &target);
                assert!(
                    parts.contains_key(&resolved),
                    "rels {name}: target {target} → {resolved} has no part"
                );
                assert!(
                    declared(&resolved),
                    "rels {name}: target {resolved} not in [Content_Types].xml"
                );
            }
        }
    }

    #[test]
    fn pptx_from_model_wires_full_layout_master_chain() {
        let bytes = pptx_from_model(&slide_deck_doc(2));
        assert_eq!(&bytes[..2], b"PK");

        // Generic OPC structural validation over the whole package.
        assert_pptx_opc_invariants(&bytes);

        // Every slide's .rels references the (existing) slide layout.
        for i in 1..=2 {
            let rels = String::from_utf8(
                entry(&bytes, &format!("ppt/slides/_rels/slide{i}.xml.rels")).unwrap(),
            )
            .unwrap();
            assert!(
                rels.contains(
                    "Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout\""
                ),
                "slide{i}.xml.rels missing slideLayout relationship"
            );
            assert!(
                rels.contains("Target=\"../slideLayouts/slideLayout1.xml\""),
                "slide{i}.xml.rels layout target wrong"
            );
            assert!(
                entry(&bytes, "ppt/slideLayouts/slideLayout1.xml").is_some(),
                "referenced slideLayout1.xml does not exist"
            );
        }

        // The layout points at the master, which exists and lists the layout.
        let layout_rels = String::from_utf8(
            entry(&bytes, "ppt/slideLayouts/_rels/slideLayout1.xml.rels").unwrap(),
        )
        .unwrap();
        assert!(
            layout_rels.contains("Target=\"../slideMasters/slideMaster1.xml\""),
            "layout does not reference the master"
        );
        let master =
            String::from_utf8(entry(&bytes, "ppt/slideMasters/slideMaster1.xml").unwrap()).unwrap();
        assert!(
            master.contains("<p:sldLayoutIdLst>"),
            "master missing p:sldLayoutIdLst"
        );

        // presentation.xml references the master both structurally and by rels.
        let pres = String::from_utf8(entry(&bytes, "ppt/presentation.xml").unwrap()).unwrap();
        assert!(
            pres.contains("<p:sldMasterIdLst>"),
            "presentation missing p:sldMasterIdLst"
        );
        let pres_rels =
            String::from_utf8(entry(&bytes, "ppt/_rels/presentation.xml.rels").unwrap()).unwrap();
        assert!(
            pres_rels.contains("Target=\"slideMasters/slideMaster1.xml\""),
            "presentation.rels missing master relationship"
        );

        // Content_Types declares the layout + master + theme parts.
        let ct = String::from_utf8(entry(&bytes, "[Content_Types].xml").unwrap()).unwrap();
        assert!(ct.contains("/ppt/slideLayouts/slideLayout1.xml"));
        assert!(ct.contains("/ppt/slideMasters/slideMaster1.xml"));
        assert!(ct.contains("/ppt/theme/theme1.xml"));
    }

    #[test]
    fn odt_from_model_is_flowing_odf() {
        let bytes = odt_from_model(&sample_doc());
        assert_eq!(&bytes[..2], b"PK");
        let content = String::from_utf8(entry(&bytes, "content.xml").unwrap()).unwrap();
        assert!(content.contains("<text:h"), "heading → text:h");
        assert!(content.contains("text:outline-level=\"1\""));
        assert!(content.contains("<text:list"), "list → text:list");
        assert!(content.contains("<table:table"), "table → table:table");
        assert!(
            content.contains("table:number-columns-spanned=\"2\""),
            "spanned cell"
        );
    }

    #[test]
    fn odt_named_style_emitted_and_referenced() {
        let bytes = odt_from_model(&styled_doc());

        // styles.xml's office:styles defines the named paragraph style.
        let styles = String::from_utf8(entry(&bytes, "styles.xml").unwrap()).unwrap();
        assert!(
            styles.contains("<office:styles>"),
            "office:styles block present"
        );
        assert!(
            styles.contains(
                "<style:style style:name=\"Body\" style:display-name=\"Body\" \
style:family=\"paragraph\""
            ),
            "named style declared: {styles}"
        );
        assert!(
            styles.contains("style:parent-style-name=\"Normal\""),
            "based_on → style:parent-style-name"
        );
        assert!(
            styles.contains("fo:text-align=\"center\""),
            "paragraph props on the named style"
        );
        assert!(
            styles.contains("fo:font-weight=\"bold\"") && styles.contains("fo:font-size=\"13pt\""),
            "text props on the named style"
        );

        // The paragraph (no direct override) references the named style directly.
        let content = String::from_utf8(entry(&bytes, "content.xml").unwrap()).unwrap();
        assert!(
            content.contains("<text:p text:style-name=\"Body\">"),
            "paragraph references the named style: {content}"
        );
    }

    #[test]
    fn odt_named_style_override_inherits_via_parent() {
        // A paragraph with BOTH a style_ref and direct formatting gets an
        // automatic style that inherits the named style and carries the override.
        let mut d = styled_doc();
        if let BlockKind::Paragraph(p) = &mut d.sections[0].pages[0].blocks[0].kind {
            p.style.indent_left_pt = 18.0;
        }
        let bytes = odt_from_model(&d);
        let content = String::from_utf8(entry(&bytes, "content.xml").unwrap()).unwrap();
        assert!(
            content.contains("style:parent-style-name=\"Body\""),
            "override inherits the named style via parent: {content}"
        );
        assert!(
            content.contains("fo:margin-left=\"18pt\""),
            "direct override is present on the automatic style"
        );
    }

    #[test]
    fn ods_from_model_types_numbers() {
        let doc = Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![Block {
                        kind: BlockKind::Sheet(SheetBlock {
                            sheets: vec![Sheet {
                                name: "S".to_string(),
                                rows: vec![SheetRow {
                                    cells: vec![SheetCell {
                                        value: CellValue::Number(7.25),
                                        number_format: Some("0.00".to_string()),
                                        ..Default::default()
                                    }],
                                    ..Default::default()
                                }],
                                merges: Vec::new(),
                                col_widths: Vec::new(),
                                ..Default::default()
                            }],
                        }),
                        ..Default::default()
                    }],
                    absolute: false,
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let bytes = ods_from_model(&doc);
        let content = String::from_utf8(entry(&bytes, "content.xml").unwrap()).unwrap();
        assert!(
            content.contains("office:value-type=\"float\" office:value=\"7.25\""),
            "typed numeric cell: {content}"
        );
        // The number format produced a data style referenced by a cell style.
        assert!(
            content.contains("<number:number-style"),
            "number format → data style"
        );
        assert!(
            content.contains("style:data-style-name="),
            "cell binds data style"
        );
    }

    #[test]
    fn odp_from_model_has_pages() {
        use crate::model::{Placeholder, PlaceholderRole, Slide, SlideBlock};
        let slide = Slide {
            geometry: crate::model::PageGeometry {
                width: 960.0,
                height: 540.0,
                margins: crate::model::Margins::uniform(0.0),

                ..Default::default()
            },
            shapes: Vec::new(),
            placeholders: vec![Placeholder {
                role: PlaceholderRole::Body,
                block: para("Body text"),
            }],
            notes: None,
            background: None,
            ..Default::default()
        };
        let doc = Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![Block {
                        kind: BlockKind::Slide(SlideBlock {
                            slides: vec![slide],
                        }),
                        ..Default::default()
                    }],
                    absolute: true,
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let bytes = odp_from_model(&doc);
        let content = String::from_utf8(entry(&bytes, "content.xml").unwrap()).unwrap();
        assert!(content.contains("<draw:page"), "slide → draw:page");
        assert!(content.contains("Body text"));
    }

    /// A one-slide deck carrying a title placeholder and speaker notes.
    fn slide_doc_with_notes(notes_text: &str) -> Document {
        use crate::model::{Placeholder, PlaceholderRole, Slide, SlideBlock};
        let slide = Slide {
            geometry: crate::model::PageGeometry {
                width: 960.0,
                height: 540.0,
                margins: crate::model::Margins::uniform(0.0),

                ..Default::default()
            },
            shapes: Vec::new(),
            placeholders: vec![Placeholder {
                role: PlaceholderRole::Title,
                block: para("Slide title"),
            }],
            notes: Some(vec![para(notes_text)]),
            background: None,
            ..Default::default()
        };
        Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![Block {
                        kind: BlockKind::Slide(SlideBlock {
                            slides: vec![slide],
                        }),
                        ..Default::default()
                    }],
                    absolute: true,
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn pptx_from_model_emits_speaker_notes() {
        let bytes = pptx_from_model(&slide_doc_with_notes("Remember to smile"));
        let files = read_zip(&bytes);
        // The notesSlide part exists and carries the notes text in a p:notes body.
        let notes = String::from_utf8(
            files
                .get("ppt/notesSlides/notesSlide1.xml")
                .expect("notesSlide part present")
                .clone(),
        )
        .unwrap();
        assert!(notes.contains("<p:notes"), "p:notes root: {notes}");
        assert!(
            notes.contains("<p:ph type=\"body\""),
            "notes body placeholder: {notes}"
        );
        assert!(
            notes.contains("Remember to smile"),
            "notes text preserved: {notes}"
        );
        // Declared in [Content_Types].
        let ct = String::from_utf8(files.get("[Content_Types].xml").unwrap().clone()).unwrap();
        assert!(
            ct.contains("/ppt/notesSlides/notesSlide1.xml")
                && ct.contains("presentationml.notesSlide+xml"),
            "notesSlide content-type override: {ct}"
        );
        // The slide → notesSlide relationship is wired, and the notesSlide back-
        // references its slide.
        let slide_rels = String::from_utf8(
            files
                .get("ppt/slides/_rels/slide1.xml.rels")
                .unwrap()
                .clone(),
        )
        .unwrap();
        assert!(
            slide_rels.contains("relationships/notesSlide")
                && slide_rels.contains("Target=\"../notesSlides/notesSlide1.xml\""),
            "slide → notesSlide relationship: {slide_rels}"
        );
        let notes_rels = String::from_utf8(
            files
                .get("ppt/notesSlides/_rels/notesSlide1.xml.rels")
                .expect("notesSlide rels present")
                .clone(),
        )
        .unwrap();
        assert!(
            notes_rels.contains("Target=\"../slides/slide1.xml\""),
            "notesSlide → slide back-reference: {notes_rels}"
        );
    }

    #[test]
    fn pptx_from_model_omits_notes_part_when_absent() {
        // A slide without notes must not produce a notesSlide part or relationship.
        let bytes = pptx_from_model(&slide_table_doc());
        let files = read_zip(&bytes);
        assert!(
            !files.keys().any(|k| k.contains("notesSlide")),
            "no notesSlide part for a note-less slide"
        );
        let ct = String::from_utf8(files.get("[Content_Types].xml").unwrap().clone()).unwrap();
        assert!(!ct.contains("notesSlide"), "no notesSlide override: {ct}");
    }

    #[test]
    fn odp_from_model_emits_speaker_notes() {
        let bytes = odp_from_model(&slide_doc_with_notes("Speak slowly"));
        let content = String::from_utf8(entry(&bytes, "content.xml").unwrap()).unwrap();
        assert!(
            content.contains("<presentation:notes"),
            "draw:page carries a presentation:notes aside: {content}"
        );
        assert!(
            content.contains("presentation:class=\"notes\""),
            "notes frame tagged with the notes class: {content}"
        );
        assert!(
            content.contains("Speak slowly"),
            "notes text preserved: {content}"
        );
    }

    /// A one-slide deck whose single shape is a 2×2 table (cells "R1C1".."R2C2").
    fn slide_table_doc() -> Document {
        use crate::model::{Slide, SlideBlock};
        let mk_cell = |t: &str| Cell {
            blocks: vec![para(t)],
            ..Default::default()
        };
        let table = Block {
            frame: Some(crate::model::Rect::new(100.0, 80.0, 400.0, 200.0)),
            kind: BlockKind::Table(Table {
                rows: vec![
                    Row {
                        cells: vec![mk_cell("R1C1"), mk_cell("R1C2")],
                        height: None,
                        is_header: false,
                    },
                    Row {
                        cells: vec![mk_cell("R2C1"), mk_cell("R2C2")],
                        height: None,
                        is_header: false,
                    },
                ],
                col_widths: vec![220.0, 180.0],
                border: BorderStyle::default(),
            }),
            ..Default::default()
        };
        let slide = Slide {
            geometry: crate::model::PageGeometry {
                width: 960.0,
                height: 540.0,
                margins: crate::model::Margins::uniform(0.0),

                ..Default::default()
            },
            shapes: vec![table],
            placeholders: Vec::new(),
            notes: None,
            background: None,
            ..Default::default()
        };
        Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![Block {
                        kind: BlockKind::Slide(SlideBlock {
                            slides: vec![slide],
                        }),
                        ..Default::default()
                    }],
                    absolute: true,
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn pptx_slide_table_emits_real_drawingml_table_not_paragraphs() {
        // #26: a Table block on a slide must export as a real `a:tbl` in a
        // `p:graphicFrame`, not a paragraph flatten.
        let bytes = pptx_from_model(&slide_table_doc());
        let s = String::from_utf8(entry(&bytes, "ppt/slides/slide1.xml").unwrap()).unwrap();

        assert!(s.contains("<p:graphicFrame>"), "table → graphicFrame");
        assert!(
            s.contains("uri=\"http://schemas.openxmlformats.org/drawingml/2006/table\""),
            "graphicData table uri",
        );
        assert!(s.contains("<a:tbl>"), "real DrawingML table");
        assert_eq!(s.matches("<a:gridCol").count(), 2, "two grid columns");
        assert_eq!(s.matches("<a:tr").count(), 2, "two rows");
        assert_eq!(s.matches("<a:tc>").count(), 4, "four (unspanned) cells");
        // Cell text survives, inside DrawingML cell text bodies.
        assert!(s.contains("<a:txBody>"), "cells carry a:txBody");
        for t in ["R1C1", "R1C2", "R2C1", "R2C2"] {
            assert!(s.contains(t), "cell text {t} preserved");
        }
        // Placed like the other slide shapes (graphicFrame xfrm), not flowed.
        assert!(s.contains("<p:xfrm>"), "table positioned with p:xfrm");
        assert!(
            s.contains("<a:gridCol w=\"2794000\"/>"),
            "220pt col width in EMU"
        );
    }

    #[test]
    fn odp_slide_table_emits_real_table_not_paragraphs() {
        // #26: the same deck to ODP yields a real `table:table` in a draw:frame.
        let bytes = odp_from_model(&slide_table_doc());
        let content = String::from_utf8(entry(&bytes, "content.xml").unwrap()).unwrap();

        assert!(
            content.contains("xmlns:table=\"urn:oasis:names:tc:opendocument:xmlns:table:1.0\""),
            "table namespace declared",
        );
        assert!(
            content.contains("<draw:frame"),
            "table wrapped in draw:frame"
        );
        assert!(content.contains("<table:table"), "real ODF table");
        assert_eq!(
            content.matches("<table:table-column").count(),
            2,
            "two columns",
        );
        assert_eq!(content.matches("<table:table-row>").count(), 2, "two rows");
        assert_eq!(
            content.matches("<table:table-cell").count(),
            4,
            "four cells",
        );
        for t in ["R1C1", "R1C2", "R2C1", "R2C2"] {
            assert!(content.contains(t), "cell text {t} preserved");
        }
    }

    /// A document with NO slide blocks whose single page carries a heading, an
    /// (unordered) list, and a 2×2 table — so the PPTX/ODP page-fallback builds a
    /// slide from flowing content. Used to exercise #2.
    fn fallback_page_doc() -> Document {
        let heading = Block {
            kind: BlockKind::Heading(Heading {
                level: 2,
                para: Paragraph {
                    runs: vec![run("Agenda")],
                    ..Default::default()
                },
            }),
            ..Default::default()
        };
        let list = Block {
            kind: BlockKind::List(List {
                ordered: false,
                marker: ListMarker::Bullet('•'),
                items: vec![
                    ListItem {
                        blocks: vec![para("alpha")],
                        level: 0,
                        label: None,
                    },
                    ListItem {
                        blocks: vec![para("beta")],
                        level: 1,
                        label: None,
                    },
                ],

                ..Default::default()
            }),
            ..Default::default()
        };
        let mk = |t: &str| Cell {
            blocks: vec![para(t)],
            ..Default::default()
        };
        let table = Block {
            kind: BlockKind::Table(Table {
                rows: vec![
                    Row {
                        cells: vec![mk("H1"), mk("H2")],
                        height: None,
                        is_header: false,
                    },
                    Row {
                        cells: vec![mk("D1"), mk("D2")],
                        height: None,
                        is_header: false,
                    },
                ],
                col_widths: vec![120.0, 120.0],
                border: BorderStyle::default(),
            }),
            ..Default::default()
        };
        Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![heading, list, table],
                    absolute: false,
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn pptx_page_fallback_keeps_list_and_table_structure() {
        // #2: a list/table/heading reaching a slide via the page-fallback must
        // export as real structure — bulleted `a:p` (a:buChar) + an `a:tbl`
        // graphicFrame — not a flat run of plain paragraphs.
        let bytes = pptx_from_model(&fallback_page_doc());
        let s = String::from_utf8(entry(&bytes, "ppt/slides/slide1.xml").unwrap()).unwrap();

        // List items keep their bullet + nesting level.
        assert!(
            s.contains("<a:buChar char=\"•\"/>"),
            "list bullet preserved: {s}"
        );
        assert!(
            s.contains("lvl=\"1\""),
            "nested list item carries its level: {s}"
        );
        // The table is a real DrawingML table, hoisted to a graphicFrame.
        assert!(s.contains("<p:graphicFrame>"), "table → graphicFrame: {s}");
        assert!(
            s.contains("<a:tbl>"),
            "real DrawingML table (not flattened): {s}"
        );
        assert_eq!(s.matches("<a:gridCol").count(), 2, "two grid columns");
        // The heading text survives as a (bold) paragraph, list/cell text too.
        for t in ["Agenda", "alpha", "beta", "H1", "D2"] {
            assert!(s.contains(t), "text {t} preserved: {s}");
        }
        assert!(
            s.contains("<a:rPr") && s.contains(" b=\"1\""),
            "heading run is bold: {s}"
        );
    }

    #[test]
    fn odp_page_fallback_keeps_list_and_table_structure() {
        // #2: the same fallback to ODP yields a real `text:list` (with a bullet
        // list style) plus a hoisted `table:table` in its own draw:frame, not a
        // paragraph flatten.
        let bytes = odp_from_model(&fallback_page_doc());
        let content = String::from_utf8(entry(&bytes, "content.xml").unwrap()).unwrap();

        assert!(content.contains("<text:list "), "real ODF list: {content}");
        assert!(
            content.contains("<text:h ") && content.contains("Agenda"),
            "heading kept as text:h: {content}"
        );
        assert!(
            content.contains("<table:table"),
            "real ODF table (not flattened): {content}"
        );
        assert_eq!(
            content.matches("<table:table-row>").count(),
            2,
            "two table rows: {content}"
        );
        for t in ["alpha", "beta", "H1", "D2"] {
            assert!(content.contains(t), "text {t} preserved: {content}");
        }
    }

    #[test]
    fn odp_placeholder_role_emits_presentation_class() {
        // #25: a slide placeholder carries its ODF `presentation:class`
        // (+ `presentation:placeholder="true"`) on the emitted `draw:frame`
        // (ISO 26300 §9.6.1), mirroring the PPTX `p:ph type=` mapping. A free
        // (non-placeholder) shape gets neither attribute.
        use crate::model::{Placeholder, PlaceholderRole, Slide, SlideBlock};

        let ph = |text: &str, role: PlaceholderRole, y: f64| Placeholder {
            role,
            block: Block {
                frame: Some(crate::model::Rect::new(40.0, y, 880.0, 80.0)),
                ..para(text)
            },
        };
        let slide = Slide {
            geometry: crate::model::PageGeometry {
                width: 960.0,
                height: 540.0,
                margins: crate::model::Margins::uniform(0.0),

                ..Default::default()
            },
            // A free text-box shape (not a placeholder) must stay role-less.
            shapes: vec![Block {
                frame: Some(crate::model::Rect::new(40.0, 460.0, 880.0, 40.0)),
                ..para("free shape")
            }],
            placeholders: vec![
                ph("The Title", PlaceholderRole::Title, 20.0),
                ph("A subtitle", PlaceholderRole::Subtitle, 120.0),
                ph("Body bullet", PlaceholderRole::Body, 220.0),
                // An unmapped role keeps its original ODF class token verbatim.
                ph(
                    "Confidential",
                    PlaceholderRole::Other("footer".to_string()),
                    420.0,
                ),
            ],
            notes: None,
            background: None,
            ..Default::default()
        };
        let doc = Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![Block {
                        kind: BlockKind::Slide(SlideBlock {
                            slides: vec![slide],
                        }),
                        ..Default::default()
                    }],
                    absolute: true,
                }],
                ..Default::default()
            }],
            ..Default::default()
        };

        let bytes = odp_from_model(&doc);
        let content = String::from_utf8(entry(&bytes, "content.xml").unwrap()).unwrap();

        // Each placeholder frame carries its presentation:class + the marker.
        for class in ["title", "subtitle", "outline", "footer"] {
            assert!(
                content.contains(&format!("presentation:class=\"{class}\"")),
                "placeholder class {class} emitted: {content}"
            );
        }
        assert_eq!(
            content.matches("presentation:placeholder=\"true\"").count(),
            4,
            "one placeholder marker per placeholder (and none on the free shape)",
        );
        // The four placeholder texts survive in their frames.
        for t in ["The Title", "A subtitle", "Body bullet", "Confidential"] {
            assert!(content.contains(t), "placeholder text {t} preserved");
        }
        // The free shape's text frame must NOT carry a presentation:class. It is
        // the only frame holding "free shape", so the class count stays at 4.
        assert!(content.contains("free shape"), "free shape text preserved");
        assert_eq!(
            content.matches("presentation:class=").count(),
            4,
            "no presentation:class on the free (non-placeholder) shape: {content}",
        );

        // Round-trip: re-import the exported ODP and confirm each emitted class
        // restores the original PlaceholderRole (`outline` → Body, `footer` kept
        // as Other("footer")), and the free shape stays a non-placeholder shape.
        let back =
            crate::convert::office_import::odp_to_model(&crate::convert::zip::read_zip(&bytes));
        let slides_back = collect_slides(&back);
        let s0 = slides_back.first().expect("one slide round-trips");
        let roles: Vec<PlaceholderRole> = s0.placeholders.iter().map(|p| p.role.clone()).collect();
        assert_eq!(
            roles,
            vec![
                PlaceholderRole::Title,
                PlaceholderRole::Subtitle,
                PlaceholderRole::Body,
                PlaceholderRole::Other("footer".to_string()),
            ],
            "placeholder roles round-trip losslessly",
        );
        assert_eq!(
            s0.shapes.len(),
            1,
            "the free shape stays a non-placeholder shape on re-import",
        );
    }

    #[test]
    fn rtf_from_model_has_paragraphs_and_styles() {
        let rtf = crate::convert::reverse::rtf_from_model(&sample_doc());
        let s = String::from_utf8(rtf).unwrap();
        assert!(s.starts_with("{\\rtf1"));
        assert!(s.contains("Title"));
        assert!(s.contains("\\par"), "paragraph breaks");
        assert!(s.contains("\\b"), "heading bold run");
    }

    // ── new exporter parity tests ──────────────────────────────────────────────

    /// Wrap a single sheet into a one-page document.
    fn sheet_doc(sheet: Sheet) -> Document {
        Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![Block {
                        kind: BlockKind::Sheet(SheetBlock {
                            sheets: vec![sheet],
                        }),
                        ..Default::default()
                    }],
                    absolute: false,
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn ods_export_emits_merges_as_spanned_and_covered_cells() {
        // A 2×2 grid whose top row spans both columns.
        let sheet = Sheet {
            name: "M".to_string(),
            rows: vec![
                SheetRow {
                    cells: vec![
                        SheetCell {
                            value: CellValue::Text("Title".to_string()),
                            ..Default::default()
                        },
                        SheetCell {
                            value: CellValue::Text("covered".to_string()),
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                },
                SheetRow {
                    cells: vec![
                        SheetCell {
                            value: CellValue::Number(1.0),
                            ..Default::default()
                        },
                        SheetCell {
                            value: CellValue::Number(2.0),
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                },
            ],
            merges: vec![MergeRange {
                r0: 0,
                c0: 0,
                r1: 0,
                c1: 1,
            }],
            col_widths: Vec::new(),
            ..Default::default()
        };
        let bytes = ods_from_model(&sheet_doc(sheet));
        let content = String::from_utf8(entry(&bytes, "content.xml").unwrap()).unwrap();
        assert!(
            content.contains("table:number-columns-spanned=\"2\""),
            "anchor carries the column span: {content}"
        );
        assert!(
            content.contains("<table:covered-table-cell/>"),
            "the covered slot is emitted: {content}"
        );
    }

    #[test]
    fn ods_export_emits_rows_spanned_for_vertical_merge() {
        // A 2×1 column merge (top-left spans two rows).
        let sheet = Sheet {
            name: "V".to_string(),
            rows: vec![
                SheetRow {
                    cells: vec![SheetCell {
                        value: CellValue::Text("tall".to_string()),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                SheetRow {
                    cells: vec![SheetCell {
                        value: CellValue::Text("covered".to_string()),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            ],
            merges: vec![MergeRange {
                r0: 0,
                c0: 0,
                r1: 1,
                c1: 0,
            }],
            col_widths: Vec::new(),
            ..Default::default()
        };
        let bytes = ods_from_model(&sheet_doc(sheet));
        let content = String::from_utf8(entry(&bytes, "content.xml").unwrap()).unwrap();
        assert!(
            content.contains("table:number-rows-spanned=\"2\""),
            "anchor carries the row span: {content}"
        );
        assert!(
            content.contains("<table:covered-table-cell/>"),
            "second row's covered slot emitted: {content}"
        );
    }

    /// A sheet with one fully-styled cell (bold red Arial 14, centered, bordered).
    fn styled_sheet() -> Sheet {
        Sheet {
            name: "Styled".to_string(),
            rows: vec![SheetRow {
                cells: vec![SheetCell {
                    value: CellValue::Text("Hi".to_string()),
                    style: CharStyle {
                        family: "Arial".to_string(),
                        size_pt: 14.0,
                        bold: true,
                        color: Some([0.8, 0.0, 0.0]),
                        ..Default::default()
                    },
                    border: Some(BorderStyle {
                        width: 1.0,
                        color: [0.0, 0.0, 0.0],
                    }),
                    align: Some(Align::Center),
                    wrap: true,
                    ..Default::default()
                }],
                height: Some(20.0),
            }],
            merges: Vec::new(),
            col_widths: Vec::new(),
            ..Default::default()
        }
    }

    #[test]
    fn xlsx_styler_emits_font_color_border_alignment() {
        let bytes = xlsx_from_model(&sheet_doc(styled_sheet()));
        let styles = String::from_utf8(entry(&bytes, "xl/styles.xml").unwrap()).unwrap();
        // A real font record (bold Arial 14) — not the hardcoded Calibri-only table.
        assert!(
            styles.contains("<name val=\"Arial\"/>"),
            "font family: {styles}"
        );
        assert!(styles.contains("<b/>"), "bold font");
        assert!(styles.contains("<sz val=\"14\"/>"), "14pt size");
        assert!(styles.contains("rgb=\"FFCC0000\""), "red font colour");
        // A border definition (the cell's all-edge style).
        assert!(styles.contains("<left style="), "border edges defined");
        // The alignment is carried on a cellXfs record.
        assert!(
            styles.contains("horizontal=\"center\"") && styles.contains("wrapText=\"1\""),
            "alignment + wrap: {styles}"
        );
        // The row height landed on the worksheet row.
        let sheet = String::from_utf8(entry(&bytes, "xl/worksheets/sheet1.xml").unwrap()).unwrap();
        assert!(sheet.contains("ht=\"20\""), "row height: {sheet}");
        assert!(sheet.contains("customHeight=\"1\""), "custom height flag");
    }

    #[test]
    fn ods_styler_emits_font_color_border_alignment() {
        let bytes = ods_from_model(&sheet_doc(styled_sheet()));
        let content = String::from_utf8(entry(&bytes, "content.xml").unwrap()).unwrap();
        assert!(
            content.contains("fo:font-family=\"Arial\""),
            "font family: {content}"
        );
        assert!(content.contains("fo:font-weight=\"bold\""), "bold");
        assert!(content.contains("fo:font-size=\"14pt\""), "14pt size");
        assert!(content.contains("fo:color=\"#CC0000\""), "red colour");
        assert!(
            content.contains("fo:border=\"1pt solid #000000\""),
            "border"
        );
        assert!(
            content.contains("fo:text-align=\"center\""),
            "center alignment"
        );
        assert!(content.contains("fo:wrap-option=\"wrap\""), "wrap option");
        assert!(content.contains("style:row-height=\"20pt\""), "row height");
    }

    #[test]
    fn round_trip_xlsx_to_model_to_ods_preserves_merges() {
        // Build a sheet with a merge, export to XLSX, re-import to the model, and
        // re-export to ODS — the merge must survive both directions.
        let sheet = Sheet {
            name: "RT".to_string(),
            rows: vec![SheetRow {
                cells: vec![
                    SheetCell {
                        value: CellValue::Text("A".to_string()),
                        ..Default::default()
                    },
                    SheetCell {
                        value: CellValue::Text("B".to_string()),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            merges: vec![MergeRange {
                r0: 0,
                c0: 0,
                r1: 0,
                c1: 1,
            }],
            col_widths: Vec::new(),
            ..Default::default()
        };
        let xlsx = xlsx_from_model(&sheet_doc(sheet));
        let zip = read_zip(&xlsx);
        let model = crate::convert::office_import::xlsx_to_model(&zip);
        // Sanity: the imported model carries the merge.
        let imported = collect_sheets(&model);
        assert_eq!(imported.len(), 1, "one sheet imported");
        assert!(
            !imported[0].merges.is_empty(),
            "merge survived the XLSX round trip"
        );
        let ods = ods_from_model(&model);
        let content = String::from_utf8(entry(&ods, "content.xml").unwrap()).unwrap();
        assert!(
            content.contains("table:number-columns-spanned=\"2\""),
            "merge re-exported to ODS as a spanned cell: {content}"
        );
        assert!(
            content.contains("<table:covered-table-cell/>"),
            "covered cell"
        );
    }

    /// A one-cell sheet carrying a formula and its cached numeric result.
    fn formula_sheet(formula: &str) -> Sheet {
        Sheet {
            name: "F".to_string(),
            rows: vec![SheetRow {
                cells: vec![SheetCell {
                    value: CellValue::Number(30.0),
                    formula: Some(formula.to_string()),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            merges: Vec::new(),
            col_widths: Vec::new(),
            ..Default::default()
        }
    }

    #[test]
    fn xlsx_from_model_emits_cell_formula() {
        // A formula cell must carry both `<f>` (the expression) and `<v>` (the
        // cached result), and a stray leading `=` is stripped.
        let xlsx = xlsx_from_model(&sheet_doc(formula_sheet("=SUM(A1:A2)")));
        let sheet = String::from_utf8(entry(&xlsx, "xl/worksheets/sheet1.xml").unwrap()).unwrap();
        assert!(
            sheet.contains("<f>SUM(A1:A2)</f>"),
            "formula emitted without the leading '=': {sheet}"
        );
        assert!(
            sheet.contains("<f>SUM(A1:A2)</f><v>30</v>"),
            "cached value follows the formula: {sheet}"
        );
        // The `<` in a comparison formula must be XML-escaped.
        let cmp = xlsx_from_model(&sheet_doc(formula_sheet("IF(A1<2,1,0)")));
        let cmp_sheet =
            String::from_utf8(entry(&cmp, "xl/worksheets/sheet1.xml").unwrap()).unwrap();
        assert!(
            cmp_sheet.contains("<f>IF(A1&lt;2,1,0)</f>"),
            "formula metacharacters escaped: {cmp_sheet}"
        );
    }

    #[test]
    fn ods_from_model_emits_cell_formula() {
        // The ODS cell must carry the formula in the OpenFormula namespace while
        // keeping the cached `office:value`.
        let ods = ods_from_model(&sheet_doc(formula_sheet("=SUM(A1:A2)")));
        let content = String::from_utf8(entry(&ods, "content.xml").unwrap()).unwrap();
        assert!(
            content.contains("table:formula=\"of:=SUM(A1:A2)\""),
            "ODS formula in the of: namespace, '=' stripped/re-added: {content}"
        );
        assert!(
            content.contains("office:value=\"30\""),
            "cached value kept alongside the formula: {content}"
        );
    }

    #[test]
    fn round_trip_formula_survives_xlsx_and_ods() {
        // Export a formula cell, re-import, and re-export both ways — the formula
        // must survive every hop (the importer strips/normalises the `=`).
        let xlsx = xlsx_from_model(&sheet_doc(formula_sheet("=SUM(A1:A2)")));
        let model = crate::convert::office_import::xlsx_to_model(&read_zip(&xlsx));
        let cell = &collect_sheets(&model)[0].rows[0].cells[0];
        assert_eq!(
            cell.formula.as_deref(),
            Some("SUM(A1:A2)"),
            "formula imported from XLSX"
        );
        // Re-export to ODS and back: still present.
        let ods = ods_from_model(&model);
        let back = crate::convert::office_import::ods_to_model(&read_zip(&ods));
        assert_eq!(
            collect_sheets(&back)[0].rows[0].cells[0].formula.as_deref(),
            Some("SUM(A1:A2)"),
            "formula survived XLSX → model → ODS → model"
        );
    }

    /// Build a one-page doc whose only block is the given list.
    fn list_doc(list: List) -> Document {
        Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![Block {
                        kind: BlockKind::List(list),
                        ..Default::default()
                    }],
                    absolute: false,
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn one_item_list(ordered: bool, marker: ListMarker) -> List {
        List {
            ordered,
            marker,
            items: vec![ListItem {
                blocks: vec![para("item")],
                level: 0,
                label: None,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn docx_numbering_honours_bullet_and_roman_markers() {
        // A bullet list → bullet numFmt with the literal bullet char.
        let bullet = docx_from_model(&list_doc(one_item_list(false, ListMarker::Bullet('▪'))));
        let num = String::from_utf8(entry(&bullet, "word/numbering.xml").unwrap()).unwrap();
        assert!(
            num.contains("<w:numFmt w:val=\"bullet\"/>"),
            "bullet list uses bullet format: {num}"
        );
        assert!(
            num.contains("w:val=\"▪\""),
            "bullet char carried through: {num}"
        );

        // A lower-roman ordered list → lowerRoman numFmt.
        let roman = docx_from_model(&list_doc(one_item_list(true, ListMarker::LowerRoman)));
        let num = String::from_utf8(entry(&roman, "word/numbering.xml").unwrap()).unwrap();
        assert!(
            num.contains("<w:numFmt w:val=\"lowerRoman\"/>"),
            "roman list uses lowerRoman format: {num}"
        );
        // Regression guard: the old code hardcoded `decimal` for every list.
        assert!(
            !num.contains("<w:numFmt w:val=\"decimal\"/>"),
            "a roman list must not fall back to decimal: {num}"
        );
    }

    #[test]
    fn docx_run_emits_vertical_align() {
        let mut sup = sample_doc();
        // Inject a superscript run into the first paragraph block (the heading
        // is block 0; the paragraph "A paragraph." is block 1).
        let para_block = &mut sup.sections[0].pages[0].blocks[1];
        if let BlockKind::Paragraph(p) = &mut para_block.kind {
            p.runs = vec![Inline::Run(InlineRun {
                text: "x2".to_string(),
                style: CharStyle {
                    vertical_align: VAlign::Super,
                    ..Default::default()
                },
                source_index: None,
            })];
        }
        let bytes = docx_from_model(&sup);
        let doc = String::from_utf8(entry(&bytes, "word/document.xml").unwrap()).unwrap();
        assert!(
            doc.contains("<w:vertAlign w:val=\"superscript\"/>"),
            "superscript run carries vertAlign: {doc}"
        );
    }

    #[test]
    fn docx_explicit_black_run_emits_colour_tag_but_unset_omits_it() {
        // #2: `visible_color` keys off explicit-vs-unset. A run whose colour is
        // genuinely unset (`None`) carries no `w:color` (the DOCX default black
        // applies); an *explicitly* black run (`Some([0,0,0])`) round-trips its
        // deliberate choice with `<w:color w:val="000000"/>`.
        let mut explicit = sample_doc();
        if let BlockKind::Paragraph(p) = &mut explicit.sections[0].pages[0].blocks[1].kind {
            p.runs = vec![Inline::Run(InlineRun {
                text: "ink".to_string(),
                style: CharStyle {
                    color: Some([0.0, 0.0, 0.0]),
                    ..Default::default()
                },
                source_index: None,
            })];
        }
        let bytes = docx_from_model(&explicit);
        let doc = String::from_utf8(entry(&bytes, "word/document.xml").unwrap()).unwrap();
        assert!(
            doc.contains("<w:color w:val=\"000000\"/>"),
            "explicit black run carries a colour tag: {doc}"
        );

        // A document built only from unset-colour runs must carry no `w:color` at
        // all (the default `sample_doc` runs all leave `color: None`).
        let plain = docx_from_model(&sample_doc());
        let plain_doc = String::from_utf8(entry(&plain, "word/document.xml").unwrap()).unwrap();
        assert!(
            !plain_doc.contains("<w:color"),
            "unset runs carry no colour tag: {plain_doc}"
        );
    }

    #[test]
    fn odt_export_emits_page_margins_and_running_header_footer() {
        let mut d = sample_doc();
        d.sections[0].geometry.margins = crate::model::Margins {
            top: 72.0,
            right: 54.0,
            bottom: 72.0,
            left: 54.0,
        };
        d.sections[0].header = Some(vec![para("PAGE HEADER")]);
        d.sections[0].footer = Some(vec![para("PAGE FOOTER")]);
        let bytes = odt_from_model(&d);
        let styles = String::from_utf8(entry(&bytes, "styles.xml").unwrap()).unwrap();
        assert!(
            styles.contains("fo:margin-top=\"72pt\"") && styles.contains("fo:margin-left=\"54pt\""),
            "page margins emitted: {styles}"
        );
        assert!(
            styles.contains("<style:header>") && styles.contains("PAGE HEADER"),
            "running header in master page: {styles}"
        );
        assert!(
            styles.contains("<style:footer>") && styles.contains("PAGE FOOTER"),
            "running footer in master page: {styles}"
        );
        // The header/footer must NOT be inlined into the body anymore.
        let content = String::from_utf8(entry(&bytes, "content.xml").unwrap()).unwrap();
        assert!(
            !content.contains("PAGE HEADER"),
            "header is not duplicated in the body: {content}"
        );
    }

    #[test]
    fn odt_two_sections_emit_distinct_master_page_per_geometry() {
        use crate::model::{Margins, PageGeometry};
        // Section 1: A4 portrait. Section 2: A4 landscape (geometry differs), so
        // ODF must mint a second master page + page-layout and switch to it via a
        // `style:master-page-name`-bearing paragraph at the boundary (#2). The
        // first section keeps the canonical `Standard`/`pm1`.
        let sec1 = Section {
            geometry: PageGeometry {
                width: 595.0,
                height: 842.0,
                margins: Margins::uniform(72.0),

                ..Default::default()
            },
            header: None,
            footer: None,
            pages: vec![Page {
                blocks: vec![para("portrait one")],
                absolute: false,
            }],
            ..Default::default()
        };
        let sec2 = Section {
            geometry: PageGeometry {
                width: 842.0,
                height: 595.0,
                margins: Margins::uniform(36.0),

                ..Default::default()
            },
            header: None,
            footer: None,
            pages: vec![Page {
                blocks: vec![para("landscape two")],
                absolute: false,
            }],
            ..Default::default()
        };
        let doc = Document {
            sections: vec![sec1, sec2],
            ..Default::default()
        };
        let bytes = odt_from_model(&doc);
        let styles = String::from_utf8(entry(&bytes, "styles.xml").unwrap()).unwrap();

        // Two page layouts (pm1 + pm2) and two master pages (Standard + Master2).
        assert!(
            styles.contains("style:name=\"pm1\""),
            "first page layout: {styles}"
        );
        assert!(
            styles.contains("style:name=\"pm2\""),
            "second page layout: {styles}"
        );
        assert!(
            styles.contains("<style:master-page style:name=\"Standard\""),
            "canonical Standard master: {styles}"
        );
        assert!(
            styles.contains("<style:master-page style:name=\"Master2\""),
            "second master page: {styles}"
        );
        // The two layouts carry their own geometry + orientation.
        assert!(
            styles.contains(
                "fo:page-width=\"595pt\" fo:page-height=\"842pt\" \
style:print-orientation=\"portrait\""
            ),
            "section 1 A4 portrait layout: {styles}"
        );
        assert!(
            styles.contains(
                "fo:page-width=\"842pt\" fo:page-height=\"595pt\" \
style:print-orientation=\"landscape\""
            ),
            "section 2 A4 landscape layout: {styles}"
        );

        // A switch paragraph style references Master2, and the body uses it.
        let content = String::from_utf8(entry(&bytes, "content.xml").unwrap()).unwrap();
        assert!(
            content.contains("style:master-page-name=\"Master2\""),
            "switch style binds to Master2: {content}"
        );
        assert!(
            content.contains("text:style-name=\"SectMP2\"><text:bookmark"),
            "section 2's first paragraph carries the master switch: {content}"
        );
        // Section 1 is the initial page master — no switch on its first paragraph.
        assert!(
            !content.contains("SectMP1"),
            "section 1 does not switch (it is the initial Standard master): {content}"
        );
    }

    #[test]
    fn odt_single_section_keeps_one_master_page() {
        // Guard: a one-section doc still mints exactly one page layout + the
        // canonical `Standard` master, and never emits a section-switch style.
        let bytes = odt_from_model(&sample_doc());
        let styles = String::from_utf8(entry(&bytes, "styles.xml").unwrap()).unwrap();
        assert_eq!(
            styles.matches("<style:page-layout ").count(),
            1,
            "exactly one page layout: {styles}"
        );
        assert_eq!(
            styles.matches("<style:master-page ").count(),
            1,
            "exactly one master page: {styles}"
        );
        assert!(
            styles.contains("style:name=\"Standard\""),
            "the single master is Standard: {styles}"
        );
        let content = String::from_utf8(entry(&bytes, "content.xml").unwrap()).unwrap();
        assert!(
            !content.contains("master-page-name"),
            "no master switch for a single section: {content}"
        );
    }

    #[test]
    fn sheet_schema_additions_default_to_unchanged_output() {
        // A plain cell with all new fields at their defaults must produce the same
        // XLSX/ODS as before the schema grew (no font/border/align/height markup).
        let plain = Sheet {
            name: "Plain".to_string(),
            rows: vec![SheetRow {
                cells: vec![SheetCell {
                    value: CellValue::Text("v".to_string()),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            merges: Vec::new(),
            col_widths: Vec::new(),
            ..Default::default()
        };
        let xlsx = xlsx_from_model(&sheet_doc(plain.clone()));
        let styles = String::from_utf8(entry(&xlsx, "xl/styles.xml").unwrap()).unwrap();
        // Default styler: a single Calibri-11 font, a single empty border, no alignment.
        assert!(
            styles.contains("<fonts count=\"1\">") && styles.contains("<name val=\"Calibri\"/>"),
            "default font table unchanged: {styles}"
        );
        assert!(
            styles.contains("<borders count=\"1\">"),
            "default border table unchanged: {styles}"
        );
        assert!(
            !styles.contains("<alignment"),
            "no alignment for a plain cell"
        );
        let sheet = String::from_utf8(entry(&xlsx, "xl/worksheets/sheet1.xml").unwrap()).unwrap();
        assert!(!sheet.contains("ht="), "no row height for a plain row");

        let ods = ods_from_model(&sheet_doc(plain));
        let content = String::from_utf8(entry(&ods, "content.xml").unwrap()).unwrap();
        assert!(
            !content.contains("fo:font-family"),
            "no font props by default"
        );
        assert!(!content.contains("fo:border"), "no border by default");
        assert!(
            !content.contains("style:row-height"),
            "no row height by default"
        );
    }

    #[test]
    fn xlsx_export_emits_cols_for_column_widths() {
        // A sheet carrying explicit per-column widths (points) re-emits the XLSX
        // `<cols><col>` block; points convert back to Excel character units
        // (140pt / 7 = 20), closing the import round-trip (`<col width="20"/>` →
        // 140pt → `<col width="20"/>`). A zero-width slot is skipped.
        let sheet = Sheet {
            name: "W".to_string(),
            rows: vec![SheetRow {
                cells: vec![SheetCell {
                    value: CellValue::Text("x".to_string()),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            merges: Vec::new(),
            col_widths: vec![140.0, 0.0, 70.0],
            ..Default::default()
        };
        let xlsx = xlsx_from_model(&sheet_doc(sheet));
        let ws = String::from_utf8(entry(&xlsx, "xl/worksheets/sheet1.xml").unwrap()).unwrap();
        assert!(ws.contains("<cols>"), "cols block emitted: {ws}");
        assert!(
            ws.contains("<col min=\"1\" max=\"1\" width=\"20\" customWidth=\"1\"/>"),
            "col1 140pt → 20ch: {ws}"
        );
        // Zero-width middle column is skipped (no `min="2"`); col 3 → 10ch.
        assert!(
            !ws.contains("min=\"2\""),
            "default column not emitted: {ws}"
        );
        assert!(
            ws.contains("<col min=\"3\" max=\"3\" width=\"10\" customWidth=\"1\"/>"),
            "col3 70pt → 10ch: {ws}"
        );
    }

    #[test]
    fn xlsx_export_no_cols_when_widths_empty() {
        // No explicit widths ⇒ no `<cols>` block (default-width sheets stay clean).
        let sheet = Sheet {
            name: "Z".to_string(),
            rows: vec![SheetRow {
                cells: vec![SheetCell {
                    value: CellValue::Text("x".to_string()),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            merges: Vec::new(),
            col_widths: Vec::new(),
            ..Default::default()
        };
        let xlsx = xlsx_from_model(&sheet_doc(sheet));
        let ws = String::from_utf8(entry(&xlsx, "xl/worksheets/sheet1.xml").unwrap()).unwrap();
        assert!(
            !ws.contains("<cols>"),
            "no cols block for default widths: {ws}"
        );
    }

    #[test]
    fn ods_export_emits_table_columns_for_widths() {
        // The ODS path emits a `table:table-column` per sized column plus a
        // column-width style (points). An empty `col_widths` emits neither.
        let sheet = Sheet {
            name: "W".to_string(),
            rows: vec![SheetRow {
                cells: vec![SheetCell {
                    value: CellValue::Text("x".to_string()),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            merges: Vec::new(),
            col_widths: vec![85.0394, 141.7323],
            ..Default::default()
        };
        let ods = ods_from_model(&sheet_doc(sheet));
        let content = String::from_utf8(entry(&ods, "content.xml").unwrap()).unwrap();
        assert!(
            content.contains("<table:table-column "),
            "table-column emitted: {content}"
        );
        assert!(
            content.contains("style:column-width=\"85.039pt\"")
                && content.contains("style:column-width=\"141.732pt\""),
            "both column widths in points: {content}"
        );

        let plain = Sheet {
            col_widths: Vec::new(),
            ..ods_from_model_plain_sheet()
        };
        let ods = ods_from_model(&sheet_doc(plain));
        let content = String::from_utf8(entry(&ods, "content.xml").unwrap()).unwrap();
        assert!(
            !content.contains("style:family=\"table-column\""),
            "no column style for default widths: {content}"
        );
    }

    /// A minimal one-cell sheet used to assert default (empty `col_widths`) ODS
    /// export emits no column styles.
    fn ods_from_model_plain_sheet() -> Sheet {
        Sheet {
            name: "Z".to_string(),
            rows: vec![SheetRow {
                cells: vec![SheetCell {
                    value: CellValue::Text("x".to_string()),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            merges: Vec::new(),
            col_widths: Vec::new(),
            ..Default::default()
        }
    }

    // ─────────────────────────────── CSV ───────────────────────────────

    /// Build a one-sheet document from rows of `&str` cells (text values).
    fn csv_sheet_doc(name: &str, rows: &[&[&str]]) -> Document {
        let sheet = Sheet {
            name: name.to_string(),
            rows: rows
                .iter()
                .map(|r| SheetRow {
                    cells: r
                        .iter()
                        .map(|c| SheetCell {
                            value: CellValue::Text((*c).to_string()),
                            ..Default::default()
                        })
                        .collect(),
                    height: None,
                })
                .collect(),
            ..Default::default()
        };
        Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![Block {
                        kind: BlockKind::Sheet(SheetBlock {
                            sheets: vec![sheet],
                        }),
                        ..Default::default()
                    }],
                    absolute: false,
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn csv_quotes_fields_with_special_characters() {
        // A comma, a quote, and a newline each force RFC-4180 quoting; a plain
        // field stays raw.
        let doc = csv_sheet_doc("", &[&["plain", "a,b", "she said \"hi\"", "line1\nline2"]]);
        let csv = csv_from_model(&doc);
        // A single sheet emits pure RFC 4180 — no name row / preamble — just the
        // one data record. The embedded LF stays *literal inside the quoted field*
        // (RFC 4180 §2.6) — it is NOT a record separator — so only the trailing
        // CRLF terminates the record.
        assert_eq!(
            csv,
            "plain,\"a,b\",\"she said \"\"hi\"\"\",\"line1\nline2\"\r\n"
        );
        // No `#` comment preamble leaked in, and the terminating CRLF is present.
        assert!(!csv.contains('#'), "no comment row: {csv:?}");
        assert!(csv.ends_with("\r\n"));
    }

    #[test]
    fn csv_uses_crlf_and_typed_values() {
        let sheet = Sheet {
            name: "Data".to_string(),
            rows: vec![
                SheetRow {
                    cells: vec![
                        SheetCell {
                            value: CellValue::Text("Label".to_string()),
                            ..Default::default()
                        },
                        SheetCell {
                            value: CellValue::Number(42.5),
                            ..Default::default()
                        },
                    ],
                    height: None,
                },
                SheetRow {
                    cells: vec![
                        SheetCell {
                            value: CellValue::Bool(true),
                            ..Default::default()
                        },
                        SheetCell {
                            value: CellValue::Empty,
                            ..Default::default()
                        },
                    ],
                    height: None,
                },
            ],
            ..Default::default()
        };
        let doc = Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![Block {
                        kind: BlockKind::Sheet(SheetBlock {
                            sheets: vec![sheet],
                        }),
                        ..Default::default()
                    }],
                    absolute: false,
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let csv = csv_from_model(&doc);
        // A single named sheet still emits pure RFC 4180: the name is NOT written
        // as a preamble (it only appears as a label when several sheets coexist).
        assert_eq!(
            csv, "Label,42.5\r\nTRUE,\r\n",
            "no preamble, typed values, trailing empty cell, CRLF"
        );
    }

    #[test]
    fn csv_separates_multiple_sheets_with_blank_line() {
        let mut doc = csv_sheet_doc("First", &[&["a", "b"]]);
        // Append a second sheet block.
        let second = Sheet {
            name: "Second".to_string(),
            rows: vec![SheetRow {
                cells: vec![SheetCell {
                    value: CellValue::Text("c".to_string()),
                    ..Default::default()
                }],
                height: None,
            }],
            ..Default::default()
        };
        doc.sections[0].pages[0].blocks.push(Block {
            kind: BlockKind::Sheet(SheetBlock {
                sheets: vec![second],
            }),
            ..Default::default()
        });
        let csv = csv_from_model(&doc);
        // Two sheets: each introduced by a *plain* (non-`#`) name row, separated
        // by one blank line. No comment rows anywhere.
        assert_eq!(
            csv, "First\r\na,b\r\n\r\nSecond\r\nc\r\n",
            "plain name rows, blank record between sheets, no `#` comments"
        );
        assert!(!csv.contains('#'), "no comment row: {csv:?}");
        // A strict split on the blank line yields two well-formed RFC-4180 blocks
        // (a name row + its data records each).
        let blocks: Vec<&str> = csv.split("\r\n\r\n").collect();
        assert_eq!(blocks.len(), 2, "two blank-line-separated blocks: {csv:?}");
        assert_eq!(blocks[0], "First\r\na,b");
        assert_eq!(blocks[1], "Second\r\nc\r\n");
    }

    #[test]
    fn csv_falls_back_to_tables_when_no_sheets() {
        // sample_doc() has no Sheet but a single 2×2 table (with a spanning cell).
        // A single table → pure RFC 4180, no name row / preamble.
        let csv = csv_from_model(&sample_doc());
        assert_eq!(csv, "spanning\r\na,b\r\n", "single table, no preamble");
        // No `#` comment row of any kind leaked in.
        assert!(!csv.contains('#'), "no comment row: {csv:?}");
    }

    #[test]
    fn csv_labels_multiple_tables_with_plain_name_rows() {
        // Two flowing tables in the fallback path → each introduced by a plain
        // `Table N` name row (no `#`), separated by a blank line.
        let mk_table = |text: &str| Block {
            kind: BlockKind::Table(Table {
                rows: vec![Row {
                    cells: vec![Cell {
                        blocks: vec![para(text)],
                        ..Default::default()
                    }],
                    height: None,
                    is_header: false,
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        let doc = Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![mk_table("x"), mk_table("y")],
                    absolute: false,
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let csv = csv_from_model(&doc);
        assert_eq!(csv, "Table 1\r\nx\r\n\r\nTable 2\r\ny\r\n");
        assert!(!csv.contains('#'), "no comment row: {csv:?}");
    }

    #[test]
    fn csv_quotes_sheet_name_row_with_comma() {
        // With several sheets the name row is emitted, and a name containing a
        // comma is RFC-4180-quoted (still parser-safe, never a `#` comment).
        let mut doc = csv_sheet_doc("a,b", &[&["1"]]);
        let second = Sheet {
            name: "plain".to_string(),
            rows: vec![SheetRow {
                cells: vec![SheetCell {
                    value: CellValue::Text("2".to_string()),
                    ..Default::default()
                }],
                height: None,
            }],
            ..Default::default()
        };
        doc.sections[0].pages[0].blocks.push(Block {
            kind: BlockKind::Sheet(SheetBlock {
                sheets: vec![second],
            }),
            ..Default::default()
        });
        let csv = csv_from_model(&doc);
        assert_eq!(csv, "\"a,b\"\r\n1\r\n\r\nplain\r\n2\r\n");
    }

    #[test]
    fn csv_is_empty_without_sheets_or_tables() {
        let doc = Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![para("just prose")],
                    absolute: false,
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(csv_from_model(&doc), "");
    }

    // ────────────────────────────── EPUB ───────────────────────────────

    /// Name and compression method (0 = stored, 8 = deflated) of the first local
    /// file header in a ZIP archive.
    fn first_entry(zip: &[u8]) -> (String, u16) {
        assert_eq!(&zip[0..4], &[0x50, 0x4b, 0x03, 0x04], "local file header");
        let method = u16::from_le_bytes([zip[8], zip[9]]);
        let nlen = u16::from_le_bytes([zip[26], zip[27]]) as usize;
        let name = String::from_utf8_lossy(&zip[30..30 + nlen]).to_string();
        (name, method)
    }

    #[test]
    fn epub_mimetype_is_first_and_stored() {
        let bytes = epub_from_model(&sample_doc());
        assert_eq!(&bytes[..2], b"PK", "valid zip");
        let (name, method) = first_entry(&bytes);
        assert_eq!(name, "mimetype", "mimetype is the first entry");
        assert_eq!(method, 0, "mimetype is stored, never deflated");
        assert_eq!(
            entry(&bytes, "mimetype").as_deref(),
            Some(&b"application/epub+zip"[..])
        );
    }

    #[test]
    fn epub_has_container_opf_and_chapter() {
        let mut doc = sample_doc();
        doc.meta.title = Some("My Book".to_string());
        doc.meta.author = Some("Ada".to_string());
        doc.meta.lang = Some("fr".to_string());
        let bytes = epub_from_model(&doc);

        // OCF container points at the package document.
        let container =
            String::from_utf8(entry(&bytes, "META-INF/container.xml").unwrap()).unwrap();
        assert!(container.contains("full-path=\"OEBPS/content.opf\""));

        // Package document carries the DocMeta and a spine.
        let opf = String::from_utf8(entry(&bytes, "OEBPS/content.opf").unwrap()).unwrap();
        assert!(opf.contains("<dc:title>My Book</dc:title>"));
        assert!(opf.contains("<dc:creator>Ada</dc:creator>"));
        assert!(opf.contains("<dc:language>fr</dc:language>"));
        assert!(opf.contains("<itemref idref=\"chap-1\"/>"), "spine item");
        assert!(opf.contains("properties=\"nav\""), "nav declared");

        // At least one chapter XHTML, well-formed and carrying the content.
        let chap = String::from_utf8(entry(&bytes, "OEBPS/text-1.xhtml").unwrap()).unwrap();
        assert!(chap.starts_with("<?xml version=\"1.0\""), "XML declaration");
        assert!(chap.contains("http://www.w3.org/1999/xhtml"), "XHTML ns");
        // The heading carries a stable anchor id (target of the nested TOC).
        assert!(
            chap.contains("<h1 id=\"sec1-h1\">Title</h1>"),
            "heading rendered with TOC anchor: {chap}"
        );
        assert!(chap.contains("A paragraph."), "paragraph text");
        assert!(chap.contains("<td colspan=\"2\">"), "spanning table cell");

        // Navigation document and NCX present.
        assert!(entry(&bytes, "OEBPS/nav.xhtml").is_some());
        assert!(entry(&bytes, "OEBPS/toc.ncx").is_some());
    }

    #[test]
    fn epub_embeds_images_and_declares_them() {
        // A document whose single paragraph carries an inline PNG image.
        let mut resources = crate::model::ResourceTable::default();
        let png = vec![0x89, b'P', b'N', b'G', 1, 2, 3];
        resources.images.insert(
            7,
            crate::model::ImageResource {
                bytes: png.clone(),
                format: "png".to_string(),
            },
        );
        let img_para = Block {
            kind: BlockKind::Paragraph(Paragraph {
                runs: vec![Inline::Image(ImageRef {
                    resource: 7,
                    alt: Some("a logo".to_string()),
                    ..Default::default()
                })],
                ..Default::default()
            }),
            ..Default::default()
        };
        let doc = Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![img_para],
                    absolute: false,
                }],
                ..Default::default()
            }],
            resources,
            ..Default::default()
        };
        let bytes = epub_from_model(&doc);

        // Blob embedded at the deterministic path with its original bytes.
        assert_eq!(
            entry(&bytes, "OEBPS/images/img-7.png").as_deref(),
            Some(png.as_slice())
        );
        // Declared in the manifest with the right media-type…
        let opf = String::from_utf8(entry(&bytes, "OEBPS/content.opf").unwrap()).unwrap();
        assert!(opf.contains("href=\"images/img-7.png\""));
        assert!(opf.contains("media-type=\"image/png\""));
        // …and referenced from the chapter by relative path.
        let chap = String::from_utf8(entry(&bytes, "OEBPS/text-1.xhtml").unwrap()).unwrap();
        assert!(chap.contains("<img src=\"images/img-7.png\" alt=\"a logo\"/>"));
    }

    #[test]
    fn epub_empty_document_still_has_one_spine_item() {
        let bytes = epub_from_model(&Document::default());
        let opf = String::from_utf8(entry(&bytes, "OEBPS/content.opf").unwrap()).unwrap();
        assert!(
            opf.contains("<itemref idref=\"chap-1\"/>"),
            "non-empty spine even for an empty model"
        );
        assert!(entry(&bytes, "OEBPS/text-1.xhtml").is_some());
    }

    /// A block-level filled + stroked vector `Shape` must reach the EPUB chapter
    /// XHTML as a self-contained inline `<svg><path d=…>` (geometry preserved,
    /// Y-flipped), NOT the legacy 1em bordered placeholder box.
    #[test]
    fn epub_shape_is_inline_svg_not_placeholder_box() {
        // A filled + stroked rectangle (10,20)-(110,70) in PDF user space.
        let shape = Shape {
            segments: vec![
                PathSeg::Move(10.0, 20.0),
                PathSeg::Line(110.0, 20.0),
                PathSeg::Line(110.0, 70.0),
                PathSeg::Line(10.0, 70.0),
                PathSeg::Close,
            ],
            fill: Some([1.0, 0.0, 0.0]),
            stroke: Some([0.0, 0.0, 1.0]),
            stroke_width: 2.0,
            dash: Vec::new(),
            ..Default::default()
        };
        let doc = md_doc(vec![Block {
            kind: BlockKind::Shape(shape),
            ..Default::default()
        }]);
        let bytes = epub_from_model(&doc);
        let chap = String::from_utf8(entry(&bytes, "OEBPS/text-1.xhtml").unwrap()).unwrap();

        assert!(chap.contains("<svg "), "inline svg emitted: {chap}");
        assert!(
            chap.contains("<path d=\"M0 50 L100 50 L100 0 L0 0 Z\""),
            "path geometry preserved (Y flipped): {chap}"
        );
        assert!(
            chap.contains("viewBox=\"0 0 100 50\"")
                && chap.contains("width=\"100pt\"")
                && chap.contains("height=\"50pt\""),
            "viewBox + size from bounds: {chap}"
        );
        assert!(chap.contains("fill=\"#FF0000\""), "fill colour: {chap}");
        assert!(
            chap.contains("stroke=\"#0000FF\"") && chap.contains("stroke-width=\"2\""),
            "stroke colour + width: {chap}"
        );
        assert!(
            !chap.contains("width:1em") && !chap.contains("border:1px solid #888"),
            "no longer a 1em bordered placeholder box: {chap}"
        );
    }

    /// A geometry-less shape (a single point) has no renderable `<svg>` viewBox,
    /// so it still falls back to the bordered-box placeholder.
    #[test]
    fn epub_shape_without_geometry_keeps_placeholder() {
        let shape = Shape {
            segments: vec![PathSeg::Move(5.0, 5.0)],
            fill: Some([0.0, 0.5, 0.0]),
            ..Default::default()
        };
        let doc = md_doc(vec![Block {
            kind: BlockKind::Shape(shape),
            ..Default::default()
        }]);
        let bytes = epub_from_model(&doc);
        let chap = String::from_utf8(entry(&bytes, "OEBPS/text-1.xhtml").unwrap()).unwrap();
        assert!(
            chap.contains("border:1px solid #888") && !chap.contains("<svg "),
            "point-less shape keeps the box fallback: {chap}"
        );
    }

    /// An H1/H2/H3 hierarchy must produce a *nested* TOC: nested `<ol>` in
    /// nav.xhtml and nested `<navPoint>`s in the NCX, with anchors that resolve to
    /// the heading ids emitted in the chapter XHTML.
    #[test]
    fn epub_nested_toc_from_heading_hierarchy() {
        let doc = md_doc(vec![
            md_heading(1, "Chapter One"),
            md_heading(2, "Section A"),
            md_heading(3, "Subsection A.1"),
            md_heading(2, "Section B"),
        ]);
        let bytes = epub_from_model(&doc);

        // Each heading carries a stable, document-ordered anchor id.
        let chap = String::from_utf8(entry(&bytes, "OEBPS/text-1.xhtml").unwrap()).unwrap();
        assert!(
            chap.contains("<h1 id=\"sec1-h1\">Chapter One</h1>"),
            "{chap}"
        );
        assert!(chap.contains("<h2 id=\"sec1-h2\">Section A</h2>"), "{chap}");
        assert!(
            chap.contains("<h3 id=\"sec1-h3\">Subsection A.1</h3>"),
            "{chap}"
        );
        assert!(chap.contains("<h2 id=\"sec1-h4\">Section B</h2>"), "{chap}");

        // nav.xhtml: the chapter node nests its headings as nested <ol>, H3 below
        // H2 below H1 — and the deep anchor resolves to the heading id.
        let nav = String::from_utf8(entry(&bytes, "OEBPS/nav.xhtml").unwrap()).unwrap();
        assert!(
            nav.contains("href=\"text-1.xhtml#sec1-h3\">Subsection A.1</a>"),
            "deep heading anchor resolvable: {nav}"
        );
        // H1 opens a child <ol> for the H2s; H2 (Section A) opens a child <ol>
        // for its H3. The exact nested shape proves the hierarchy.
        assert!(
            nav.contains(
                "<a href=\"text-1.xhtml#sec1-h1\">Chapter One</a>\
<ol><li><a href=\"text-1.xhtml#sec1-h2\">Section A</a>\
<ol><li><a href=\"text-1.xhtml#sec1-h3\">Subsection A.1</a></li></ol></li>\
<li><a href=\"text-1.xhtml#sec1-h4\">Section B</a></li></ol>"
            ),
            "nested <ol> hierarchy in nav: {nav}"
        );

        // NCX: nested navPoints + a depth meta greater than 1 (the chapter is
        // depth 1, H1 depth 2, H2 depth 3, H3 depth 4).
        let ncx = String::from_utf8(entry(&bytes, "OEBPS/toc.ncx").unwrap()).unwrap();
        assert!(
            ncx.contains("content src=\"text-1.xhtml#sec1-h3\""),
            "NCX deep heading anchor: {ncx}"
        );
        assert!(
            ncx.contains("name=\"dtb:depth\" content=\"4\""),
            "NCX nesting depth reflects the hierarchy: {ncx}"
        );
        // A navPoint nested inside another navPoint (not a flat <navMap>).
        assert!(
            ncx.contains("\"/><navPoint "),
            "navPoints nest (a navPoint follows a content inside its parent): {ncx}"
        );
    }

    #[test]
    fn epub_run_css_omits_black_but_keeps_real_colour() {
        // An inline `color:#000000` on every PDF-extracted run would defeat the
        // reader's dark/sepia themes; only a genuine colour is emitted.
        let black = CharStyle {
            color: Some([0.0, 0.0, 0.0]),
            ..Default::default()
        };
        let red = CharStyle {
            color: Some([1.0, 0.0, 0.0]),
            ..Default::default()
        };
        let doc = md_doc(vec![md_para(vec![
            styled("ink ", black),
            styled("alert", red),
        ])]);
        let bytes = epub_from_model(&doc);
        let chap = String::from_utf8(entry(&bytes, "OEBPS/text-1.xhtml").unwrap()).unwrap();
        assert!(
            !chap.contains("color:#000000"),
            "no black inline colour: {chap}"
        );
        assert!(chap.contains("ink"), "black run text kept: {chap}");
        assert!(
            chap.contains("color:#FF0000"),
            "red run keeps its colour: {chap}"
        );
    }

    #[test]
    fn toc_title_follows_the_primary_language_subtag() {
        assert_eq!(toc_title("fr-FR"), "Table des matières");
        assert_eq!(toc_title("fr_CA"), "Table des matières");
        assert_eq!(toc_title("FR"), "Table des matières");
        assert_eq!(toc_title("he"), "תוכן העניינים");
        assert_eq!(toc_title("de-AT"), "Inhaltsverzeichnis");
        assert_eq!(toc_title("ja"), "目次");
        assert_eq!(toc_title("x-klingon"), "Table of Contents");
        assert_eq!(toc_title("tlh"), "Table of Contents");
        assert_eq!(toc_title(""), "Table of Contents");
    }

    #[test]
    fn epub_nav_title_is_localised() {
        let mut doc = md_doc(vec![md_heading(1, "Chapitre")]);
        doc.meta.lang = Some("fr-FR".to_string());
        let nav =
            String::from_utf8(entry(&epub_from_model(&doc), "OEBPS/nav.xhtml").unwrap()).unwrap();
        assert!(nav.contains("<h1>Table des matières</h1>"), "{nav}");
        assert!(nav.contains("<title>Table des matières</title>"), "{nav}");
        assert!(!nav.contains("Table of Contents"), "{nav}");

        // No language → English default, unchanged.
        let nav_en = String::from_utf8(
            entry(
                &epub_from_model(&md_doc(vec![md_heading(1, "C")])),
                "OEBPS/nav.xhtml",
            )
            .unwrap(),
        )
        .unwrap();
        assert!(nav_en.contains("<h1>Table of Contents</h1>"), "{nav_en}");
    }

    /// The OPF `dc:identifier` is unique per document and deterministic: two
    /// different documents differ; the same document is identical across runs; the
    /// `unique-identifier` attribute, `dc:identifier`, and NCX `dtb:uid` all agree.
    #[test]
    fn epub_identifier_is_unique_and_deterministic() {
        let extract_ident = |opf: &str| -> String {
            let start = opf.find("<dc:identifier id=\"pub-id\">").unwrap()
                + "<dc:identifier id=\"pub-id\">".len();
            let end = opf[start..].find("</dc:identifier>").unwrap();
            opf[start..start + end].to_string()
        };
        let opf_of = |doc: &Document| -> String {
            let bytes = epub_from_model(doc);
            String::from_utf8(entry(&bytes, "OEBPS/content.opf").unwrap()).unwrap()
        };

        let doc_a = md_doc(vec![md_heading(1, "Alpha"), md_para(vec![run("body A")])]);
        let doc_b = md_doc(vec![md_heading(1, "Beta"), md_para(vec![run("body B")])]);

        let opf_a = opf_of(&doc_a);
        let opf_b = opf_of(&doc_b);
        let id_a = extract_ident(&opf_a);
        let id_b = extract_ident(&opf_b);

        // Deterministic: same document hashes identically across builds.
        assert_eq!(id_a, extract_ident(&opf_of(&doc_a)), "identifier is stable");
        // Unique: two different documents get different identifiers.
        assert_ne!(id_a, id_b, "different documents differ: {id_a} vs {id_b}");
        // Shaped as a urn:gigapdf hash (not the old hardcoded value).
        assert!(id_a.starts_with("urn:gigapdf:"), "urn form: {id_a}");
        assert_ne!(id_a, "urn:gigapdf:document", "no longer hardcoded");

        // The package `unique-identifier` points at pub-id, and the NCX dtb:uid
        // carries the same identifier so OPF and NCX agree.
        assert!(opf_a.contains("unique-identifier=\"pub-id\""), "{opf_a}");
        let bytes = epub_from_model(&doc_a);
        let ncx = String::from_utf8(entry(&bytes, "OEBPS/toc.ncx").unwrap()).unwrap();
        assert!(
            ncx.contains(&format!("name=\"dtb:uid\" content=\"{id_a}\"")),
            "NCX dtb:uid agrees with OPF identifier: {ncx}"
        );
    }

    /// A one-page document whose only block is a block-level image referencing a
    /// resource of the given format tag (the bytes are opaque placeholders).
    fn image_doc(format: &str) -> Document {
        let mut resources = crate::model::ResourceTable::default();
        resources.images.insert(
            9,
            crate::model::ImageResource {
                bytes: vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3],
                format: format.to_string(),
            },
        );
        let img = Block {
            kind: BlockKind::Image(ImageRef {
                resource: 9,
                alt: Some("photo".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![img],
                    absolute: false,
                }],
                ..Default::default()
            }],
            resources,
            ..Default::default()
        }
    }

    #[test]
    fn docx_export_honours_jpeg_image_format() {
        // A JPEG resource must land in a `.jpeg` part with the matching
        // content-type and relationship target — never a `.png` part.
        let bytes = docx_from_model(&image_doc("jpeg"));
        let files = read_zip(&bytes);
        assert!(
            files.contains_key("word/media/image1.jpeg"),
            "JPEG embedded as a .jpeg part"
        );
        assert!(
            !files.contains_key("word/media/image1.png"),
            "no spurious .png part for a JPEG resource"
        );
        let ct = String::from_utf8(files.get("[Content_Types].xml").unwrap().clone()).unwrap();
        assert!(
            ct.contains("<Default Extension=\"jpeg\" ContentType=\"image/jpeg\"/>"),
            "jpeg content-type declared: {ct}"
        );
        assert!(
            !ct.contains("Extension=\"png\""),
            "no png default when only a jpeg is present: {ct}"
        );
        let rels =
            String::from_utf8(files.get("word/_rels/document.xml.rels").unwrap().clone()).unwrap();
        assert!(
            rels.contains("Target=\"media/image1.jpeg\""),
            "relationship targets the .jpeg part: {rels}"
        );
    }

    #[test]
    fn odt_export_honours_jpeg_image_format() {
        // The ODF path must mirror the format in the Pictures/ part name, the
        // manifest media-type and the draw:image href.
        let bytes = odt_from_model(&image_doc("jpeg"));
        let files = read_zip(&bytes);
        assert!(
            files.contains_key("Pictures/img1.jpeg"),
            "JPEG embedded as a .jpeg picture"
        );
        assert!(
            !files.contains_key("Pictures/img1.png"),
            "no spurious .png picture for a JPEG resource"
        );
        let manifest =
            String::from_utf8(files.get("META-INF/manifest.xml").unwrap().clone()).unwrap();
        assert!(
            manifest.contains(
                "manifest:full-path=\"Pictures/img1.jpeg\" manifest:media-type=\"image/jpeg\""
            ),
            "manifest declares the jpeg with its media-type: {manifest}"
        );
        let content = String::from_utf8(files.get("content.xml").unwrap().clone()).unwrap();
        assert!(
            content.contains("xlink:href=\"Pictures/img1.jpeg\""),
            "draw:image references the .jpeg part: {content}"
        );
    }

    // ───────────────────────────── Markdown ─────────────────────────────

    /// One inline run carrying an explicit character style.
    fn styled(text: &str, style: CharStyle) -> Inline {
        Inline::Run(InlineRun {
            text: text.to_string(),
            style,
            ..Default::default()
        })
    }

    /// A single-page document holding exactly the given blocks (no metadata).
    fn md_doc(blocks: Vec<Block>) -> Document {
        Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks,
                    absolute: false,
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// A heading block at `level` with a single plain run.
    fn md_heading(level: u8, text: &str) -> Block {
        Block {
            kind: BlockKind::Heading(Heading {
                level,
                para: Paragraph {
                    runs: vec![run(text)],
                    ..Default::default()
                },
            }),
            ..Default::default()
        }
    }

    /// A paragraph block from raw inline runs.
    fn md_para(runs: Vec<Inline>) -> Block {
        Block {
            kind: BlockKind::Paragraph(Paragraph {
                runs,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn markdown_empty_document_is_empty_string() {
        assert_eq!(markdown_from_model(&Document::default()), "");
    }

    #[test]
    fn markdown_headings_render_levels_one_to_six() {
        let blocks = (1u8..=6).map(|l| md_heading(l, "T")).collect();
        let md = markdown_from_model(&md_doc(blocks));
        assert!(md.contains("# T\n"), "h1");
        assert!(md.contains("## T\n"), "h2");
        assert!(md.contains("### T\n"), "h3");
        assert!(md.contains("#### T\n"), "h4");
        assert!(md.contains("##### T\n"), "h5");
        assert!(md.contains("###### T\n"), "h6");
    }

    #[test]
    fn markdown_renders_run_emphasis() {
        let bold = CharStyle {
            bold: true,
            ..Default::default()
        };
        let ital = CharStyle {
            italic: true,
            ..Default::default()
        };
        let bold_ital = CharStyle {
            bold: true,
            italic: true,
            ..Default::default()
        };
        let strike = CharStyle {
            strike: true,
            ..Default::default()
        };
        let mono = CharStyle {
            generic: crate::convert::style::Generic::Mono,
            ..Default::default()
        };
        let md = markdown_from_model(&md_doc(vec![md_para(vec![
            styled("b", bold),
            run(" "),
            styled("i", ital),
            run(" "),
            styled("bi", bold_ital),
            run(" "),
            styled("s", strike),
            run(" "),
            styled("code", mono),
        ])]));
        assert!(md.contains("**b**"), "bold: {md}");
        assert!(md.contains("*i*"), "italic: {md}");
        assert!(md.contains("***bi***"), "bold+italic: {md}");
        assert!(md.contains("~~s~~"), "strikethrough: {md}");
        assert!(md.contains("`code`"), "monospace → code span: {md}");
    }

    #[test]
    fn markdown_wraps_coloured_run_in_inline_html_span() {
        // A non-default run colour has no portable Markdown form, so GFM inline
        // HTML carries it: an outer `<span style="color:#RRGGBB">…</span>`
        // wrapping the (still Markdown) emphasised body.
        let red_bold = CharStyle {
            bold: true,
            color: Some([1.0, 0.0, 0.0]),
            ..Default::default()
        };
        let md = markdown_from_model(&md_doc(vec![md_para(vec![styled("hi", red_bold)])]));
        assert!(
            md.contains("<span style=\"color:#FF0000\">**hi**</span>"),
            "coloured run → span wrapping the bold body: {md}"
        );
    }

    #[test]
    fn markdown_emits_colour_span_only_for_non_black_runs() {
        // PDF-extracted runs always carry a colour, almost always black: a span
        // around each of them is noise. Unset, explicit black and near-black all
        // fall back to the format default; a real colour keeps its span.
        let unset = CharStyle::default(); // color: None
        let md = markdown_from_model(&md_doc(vec![md_para(vec![styled("plain", unset)])]));
        assert!(!md.contains("<span"), "no colour span for unset run: {md}");
        assert!(md.contains("plain"), "text still present: {md}");

        for (label, rgb) in [
            ("black", [0.0, 0.0, 0.0]),
            ("near-black", [0.01, 0.015, 0.0]),
        ] {
            let style = CharStyle {
                color: Some(rgb),
                ..Default::default()
            };
            let md = markdown_from_model(&md_doc(vec![md_para(vec![styled("ink", style)])]));
            assert!(!md.contains("<span"), "{label} run → no colour span: {md}");
            assert!(md.contains("ink"), "{label} text still present: {md}");
        }

        let blue = CharStyle {
            color: Some([0.0, 0.2, 0.8]),
            ..Default::default()
        };
        let md_blue = markdown_from_model(&md_doc(vec![md_para(vec![styled("sky", blue)])]));
        assert!(
            md_blue.contains("<span style=\"color:#0033CC\">sky</span>"),
            "real colour → span kept: {md_blue}"
        );
    }

    #[test]
    fn markdown_emits_shape_as_inline_svg() {
        // A block-level vector Shape is preserved as a self-contained inline
        // `<svg>` (GFM inline HTML), not dropped. Geometry mirrors the HTML/EPUB
        // exporters: viewBox from the path bounds, `<path d>` Y-flipped.
        let shape = Shape {
            segments: vec![
                PathSeg::Move(10.0, 20.0),
                PathSeg::Line(110.0, 20.0),
                PathSeg::Line(110.0, 70.0),
                PathSeg::Line(10.0, 70.0),
                PathSeg::Close,
            ],
            fill: Some([1.0, 0.0, 0.0]),
            stroke: Some([0.0, 0.0, 1.0]),
            stroke_width: 2.0,
            dash: Vec::new(),
            ..Default::default()
        };
        let md = markdown_from_model(&md_doc(vec![Block {
            kind: BlockKind::Shape(shape),
            ..Default::default()
        }]));
        assert!(md.contains("<svg "), "inline svg emitted: {md}");
        assert!(
            md.contains("<path d=\"M0 50 L100 50 L100 0 L0 0 Z\""),
            "path geometry preserved (Y flipped): {md}"
        );
        assert!(
            md.contains("viewBox=\"0 0 100 50\""),
            "viewBox from bounds: {md}"
        );
        assert!(md.contains("fill=\"#FF0000\""), "fill colour: {md}");
        assert!(
            md.contains("stroke=\"#0000FF\"") && md.contains("stroke-width=\"2\""),
            "stroke colour + width: {md}"
        );
    }

    #[test]
    fn markdown_renders_links_and_images() {
        let url_link = Inline::Link {
            href: LinkTarget::Url("https://example.com".to_string()),
            children: vec![run("site")],
        };
        let page_link = Inline::Link {
            href: LinkTarget::Page(2),
            children: vec![run("see")],
        };
        let mut resources = crate::model::ResourceTable::default();
        resources.images.insert(
            5,
            crate::model::ImageResource {
                bytes: vec![1, 2, 3],
                format: "png".to_string(),
            },
        );
        let img = Inline::Image(ImageRef {
            resource: 5,
            alt: Some("alt".to_string()),
            ..Default::default()
        });
        let mut doc = md_doc(vec![md_para(vec![
            url_link,
            run(" "),
            page_link,
            run(" "),
            img,
        ])]);
        doc.resources = resources;
        let md = markdown_from_model(&doc);
        assert!(md.contains("[site](https://example.com)"), "url link: {md}");
        // LinkTarget::Page(2) is zero-based → human page 3.
        assert!(md.contains("[see](#page-3)"), "internal page link: {md}");
        assert!(md.contains("![alt](image-5.png)"), "image: {md}");
    }

    #[test]
    fn markdown_renders_nested_ordered_and_unordered_lists() {
        // A list block nested inside an item is already one level deeper by
        // *structure*; its items' own `level` is relative to that sub-list
        // (0 = its first depth), matching the HTML importer's shape. An item
        // `level` now adds real indentation, so double-encoding the nesting
        // (nested block *and* level 1) would indent twice.
        let nested = Block {
            kind: BlockKind::List(List {
                ordered: false,
                marker: ListMarker::Bullet('•'),
                items: vec![ListItem {
                    blocks: vec![para("sub")],
                    level: 0,
                    label: None,
                }],

                ..Default::default()
            }),
            ..Default::default()
        };
        let outer = Block {
            kind: BlockKind::List(List {
                ordered: true,
                marker: ListMarker::Decimal,
                items: vec![
                    ListItem {
                        blocks: vec![para("one"), nested],
                        level: 0,
                        label: None,
                    },
                    ListItem {
                        blocks: vec![para("two")],
                        level: 0,
                        label: None,
                    },
                ],

                ..Default::default()
            }),
            ..Default::default()
        };
        let md = markdown_from_model(&md_doc(vec![outer]));
        assert!(md.contains("1. one\n"), "ordered marker: {md}");
        assert!(md.contains("2. two\n"), "second ordered item: {md}");
        // Nested list is indented four spaces and uses the bullet marker.
        assert!(md.contains("\n    - sub\n"), "nested indented bullet: {md}");
    }

    #[test]
    fn markdown_renders_gfm_table_with_alignment_row() {
        // A 2-column table whose header cells are centre- and right-aligned.
        let hdr = |text: &str, align: Align| Cell {
            blocks: vec![Block {
                kind: BlockKind::Paragraph(Paragraph {
                    runs: vec![run(text)],
                    style: crate::model::ParagraphStyle {
                        align,
                        ..Default::default()
                    },
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        let table = Block {
            kind: BlockKind::Table(Table {
                rows: vec![
                    Row {
                        cells: vec![hdr("H1", Align::Center), hdr("H2", Align::Right)],
                        height: None,
                        // The GFM header row; Markdown export keys the alignment
                        // row off the first row regardless, so output is unchanged.
                        is_header: true,
                    },
                    Row {
                        cells: vec![
                            Cell {
                                blocks: vec![para("a")],
                                ..Default::default()
                            },
                            Cell {
                                blocks: vec![para("b")],
                                ..Default::default()
                            },
                        ],
                        height: None,
                        is_header: false,
                    },
                ],
                col_widths: vec![],
                ..Default::default()
            }),
            ..Default::default()
        };
        let md = markdown_from_model(&md_doc(vec![table]));
        assert!(md.contains("| H1 | H2 |\n"), "header row: {md}");
        assert!(md.contains("| :--: | ---: |\n"), "alignment row: {md}");
        assert!(md.contains("| a | b |\n"), "body row: {md}");
    }

    #[test]
    fn markdown_inserts_thematic_break_between_pages() {
        // Two pages, each with content → a `---` page boundary between them.
        let doc = Document {
            sections: vec![Section {
                pages: vec![
                    Page {
                        blocks: vec![para("page one")],
                        absolute: false,
                    },
                    Page {
                        blocks: vec![para("page two")],
                        absolute: false,
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        let md = markdown_from_model(&doc);
        assert!(md.contains("page one"), "first page: {md}");
        assert!(md.contains("page two"), "second page: {md}");
        assert!(md.contains("\n---\n"), "thematic break between pages: {md}");
    }

    #[test]
    fn markdown_escapes_inline_punctuation() {
        // `*` `_` `[` would be inline markup if unescaped.
        let md = markdown_from_model(&md_doc(vec![para("a*b_c[d")]));
        assert!(md.contains("a\\*b\\_c\\[d"), "escaped punctuation: {md}");
    }

    #[test]
    fn markdown_emits_front_matter_from_metadata() {
        let mut doc = md_doc(vec![para("body")]);
        doc.meta.title = Some("My Title".to_string());
        doc.meta.author = Some("Ada".to_string());
        doc.meta.lang = Some("en".to_string());
        let md = markdown_from_model(&doc);
        assert!(md.starts_with("---\n"), "front-matter opens: {md}");
        assert!(md.contains("title: My Title\n"), "title key: {md}");
        assert!(md.contains("author: Ada\n"), "author key: {md}");
        assert!(md.contains("lang: en\n"), "lang key: {md}");
        assert!(md.contains("\n---\n\n"), "front-matter closes: {md}");
    }

    // ── CodeBlock / Blockquote / HorizontalRule ───────────────────────────────

    fn code_blk(lang: Option<&str>, code: &str) -> Block {
        Block {
            kind: BlockKind::CodeBlock(crate::model::CodeBlock {
                lang: lang.map(str::to_string),
                code: code.to_string(),
            }),
            ..Default::default()
        }
    }

    fn quote_blk(blocks: Vec<Block>) -> Block {
        Block {
            kind: BlockKind::Blockquote(crate::model::Blockquote { blocks }),
            ..Default::default()
        }
    }

    fn rule_blk() -> Block {
        Block {
            kind: BlockKind::HorizontalRule,
            ..Default::default()
        }
    }

    #[test]
    fn markdown_code_block_emits_fenced_block_with_lang() {
        let md = markdown_from_model(&md_doc(vec![code_blk(Some("rust"), "fn main() {}")]));
        assert!(
            md.contains("```rust\nfn main() {}\n```"),
            "fenced + lang: {md}"
        );
    }

    #[test]
    fn markdown_code_fence_lengthens_past_internal_backticks() {
        // Content containing a ``` run must be wrapped in a longer fence.
        let md = markdown_from_model(&md_doc(vec![code_blk(None, "a ``` b")]));
        assert!(md.contains("````\na ``` b\n````"), "dynamic fence: {md}");
    }

    #[test]
    fn markdown_blockquote_prefixes_every_line() {
        let inner = vec![md_para(vec![run("quoted text")])];
        let md = markdown_from_model(&md_doc(vec![quote_blk(inner)]));
        assert!(md.contains("> quoted text"), "quoted line: {md}");
    }

    #[test]
    fn markdown_horizontal_rule_emits_dash_break() {
        let md = markdown_from_model(&md_doc(vec![para("before"), rule_blk(), para("after")]));
        assert!(md.contains("\n---\n"), "thematic break: {md}");
    }

    #[test]
    fn markdown_round_trips_code_quote_rule() {
        // model → markdown → model must preserve the three new block kinds.
        let original = md_doc(vec![
            code_blk(Some("python"), "print('hi')\nx = 1"),
            rule_blk(),
            quote_blk(vec![md_para(vec![run("a quote")])]),
        ]);
        let md = markdown_from_model(&original);
        let back = crate::convert::md_to_model(&md);
        let b = &back.sections[0].pages[0].blocks;

        // CodeBlock survives verbatim with its language.
        let cb = b
            .iter()
            .find_map(|x| match &x.kind {
                BlockKind::CodeBlock(cb) => Some(cb),
                _ => None,
            })
            .expect("a code block round-trips");
        assert_eq!(cb.lang.as_deref(), Some("python"));
        assert_eq!(cb.code, "print('hi')\nx = 1");

        // HorizontalRule survives.
        assert!(
            b.iter()
                .any(|x| matches!(&x.kind, BlockKind::HorizontalRule)),
            "a rule round-trips: {md}"
        );

        // Blockquote survives with its inner paragraph text.
        let bq = b
            .iter()
            .find_map(|x| match &x.kind {
                BlockKind::Blockquote(bq) => Some(bq),
                _ => None,
            })
            .expect("a blockquote round-trips");
        let text: String = bq
            .blocks
            .iter()
            .flat_map(|ib| match &ib.kind {
                BlockKind::Paragraph(p) => p
                    .runs
                    .iter()
                    .filter_map(|r| match r {
                        Inline::Run(run) => Some(run.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect();
        assert!(text.contains("a quote"), "quote text survives: {text}");
    }

    #[test]
    fn html_from_model_renders_code_quote_rule_tags() {
        let doc = md_doc(vec![
            code_blk(Some("rust"), "let x = 1;"),
            rule_blk(),
            quote_blk(vec![md_para(vec![run("cite")])]),
        ]);
        let html = crate::convert::web::html_from_model(&doc);
        assert!(
            html.contains("<pre><code class=\"language-rust\">"),
            "code: {html}"
        );
        assert!(html.contains("let x = 1;"), "code text: {html}");
        assert!(html.contains("<hr/>"), "rule: {html}");
        assert!(html.contains("<blockquote>"), "quote open: {html}");
        assert!(html.contains("cite"), "quote text: {html}");
    }

    #[test]
    fn html_to_pdf_renders_code_quote_rule_without_panic() {
        // The real HTML→PDF path: model → semantic HTML → the html render
        // pipeline. It must produce a valid, non-trivial PDF for all three.
        let doc = md_doc(vec![
            code_blk(Some("rust"), "fn main() {\n    println!(\"hi\");\n}"),
            rule_blk(),
            quote_blk(vec![md_para(vec![run("a quotation")])]),
        ]);
        let html = crate::convert::web::html_from_model(&doc);
        let pdf = crate::convert::reverse::html_to_pdf(&html);
        assert!(pdf.starts_with(b"%PDF-"), "valid PDF header");
        assert!(pdf.len() > 500, "non-trivial PDF: {} bytes", pdf.len());
    }

    #[test]
    fn pdf_from_model_lays_out_code_quote_rule() {
        // The compat model→ConvPage→PDF path must place the code text and the
        // rule shape (and not drop the quote's text).
        let doc = md_doc(vec![
            code_blk(None, "alpha\nbeta"),
            quote_blk(vec![md_para(vec![run("quoted body")])]),
            rule_blk(),
        ]);
        let pages: Vec<crate::convert::ConvPage> = (&doc).into();
        let all_text: String = pages
            .iter()
            .flat_map(|p| p.texts.iter().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("|");
        assert!(all_text.contains("alpha"), "code line 1: {all_text}");
        assert!(all_text.contains("beta"), "code line 2: {all_text}");
        assert!(all_text.contains("quoted body"), "quote text: {all_text}");
        let has_rule = pages.iter().any(|p| !p.shapes.is_empty());
        assert!(has_rule, "the horizontal rule produced a shape");
    }

    #[test]
    fn xhtml_export_renders_code_quote_rule() {
        // EPUB chapters are XHTML; the three constructs must be valid elements.
        let doc = md_doc(vec![
            code_blk(Some("js"), "console.log(1)"),
            rule_blk(),
            quote_blk(vec![md_para(vec![run("epub quote")])]),
        ]);
        let mut out = String::new();
        let mut toc = EpubToc::new(1);
        xhtml_blocks(&doc.sections[0].pages[0].blocks, &doc, &mut out, &mut toc);
        assert!(
            out.contains("<pre><code class=\"language-js\">"),
            "code: {out}"
        );
        assert!(out.contains("console.log(1)"), "code text: {out}");
        assert!(out.contains("<hr/>"), "rule: {out}");
        assert!(out.contains("<blockquote>"), "quote: {out}");
        assert!(out.contains("epub quote"), "quote text: {out}");
    }

    #[test]
    fn docx_export_renders_code_quote_rule() {
        let doc = md_doc(vec![
            code_blk(None, "x = 1"),
            rule_blk(),
            quote_blk(vec![md_para(vec![run("docx quote")])]),
        ]);
        let bytes = docx_from_model(&doc);
        let files = read_zip(&bytes);
        let document = files
            .get("word/document.xml")
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .expect("document.xml present");
        assert!(
            document.contains("Courier New"),
            "code uses a mono font: {document}"
        );
        assert!(document.contains("x = 1"), "code text present");
        assert!(
            document.contains("<w:pBdr>"),
            "rule emits a paragraph border"
        );
        assert!(document.contains("docx quote"), "quote text present");
    }

    // ── document properties (metadata + language) ───────────────────────────────

    /// A one-paragraph document with full metadata + a `fr-FR` language tag.
    fn meta_doc() -> Document {
        Document {
            meta: crate::model::DocMeta {
                title: Some("My <Title>".to_string()),
                author: Some("Jane Doe".to_string()),
                subject: Some("Quarterly".to_string()),
                keywords: vec!["alpha".to_string(), "beta".to_string()],
                lang: Some("fr-FR".to_string()),
                ..Default::default()
            },
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![para("Body.")],
                    absolute: false,
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// A sheet document carrying the same metadata (for XLSX/ODS).
    fn meta_sheet_doc() -> Document {
        let mut doc = sheet_doc(Sheet {
            name: "S".to_string(),
            rows: vec![SheetRow {
                cells: vec![SheetCell {
                    value: CellValue::Text("x".to_string()),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            merges: Vec::new(),
            col_widths: Vec::new(),
            ..Default::default()
        });
        doc.meta = meta_doc().meta;
        doc
    }

    /// A slide document carrying the same metadata (for PPTX/ODP).
    fn meta_slide_doc() -> Document {
        use crate::model::{Placeholder, PlaceholderRole, Slide, SlideBlock};
        let slide = Slide {
            geometry: crate::model::PageGeometry {
                width: 960.0,
                height: 540.0,
                margins: crate::model::Margins::uniform(0.0),

                ..Default::default()
            },
            shapes: Vec::new(),
            placeholders: vec![Placeholder {
                role: PlaceholderRole::Title,
                block: para("T"),
            }],
            notes: None,
            background: None,
            ..Default::default()
        };
        Document {
            meta: meta_doc().meta,
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![Block {
                        kind: BlockKind::Slide(SlideBlock {
                            slides: vec![slide],
                        }),
                        ..Default::default()
                    }],
                    absolute: true,
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// Assert an OOXML package wires both docProps parts through
    /// `[Content_Types].xml` + `_rels/.rels`, and that `core.xml` carries the
    /// model metadata (with XML escaping).
    fn assert_ooxml_docprops(bytes: &[u8]) {
        let core = String::from_utf8(entry(bytes, "docProps/core.xml").unwrap()).unwrap();
        assert!(
            core.contains("<dc:title>My &lt;Title&gt;</dc:title>"),
            "title escaped: {core}"
        );
        assert!(
            core.contains("<dc:creator>Jane Doe</dc:creator>"),
            "creator: {core}"
        );
        assert!(
            core.contains("<dc:subject>Quarterly</dc:subject>"),
            "subject: {core}"
        );
        assert!(
            core.contains("<cp:keywords>alpha, beta</cp:keywords>"),
            "keywords: {core}"
        );
        assert!(
            core.contains("<dc:language>fr-FR</dc:language>"),
            "language: {core}"
        );

        let app = String::from_utf8(entry(bytes, "docProps/app.xml").unwrap()).unwrap();
        assert!(
            app.contains("<Application>GigaPDF</Application>"),
            "app name: {app}"
        );

        let ct = String::from_utf8(entry(bytes, "[Content_Types].xml").unwrap()).unwrap();
        assert!(
            ct.contains("PartName=\"/docProps/core.xml\""),
            "core override declared: {ct}"
        );
        assert!(
            ct.contains("PartName=\"/docProps/app.xml\""),
            "app override declared: {ct}"
        );

        let rels = String::from_utf8(entry(bytes, "_rels/.rels").unwrap()).unwrap();
        assert!(
            rels.contains("Target=\"docProps/core.xml\""),
            "core relationship present: {rels}"
        );
        assert!(
            rels.contains("relationships/metadata/core-properties"),
            "core-properties rel type: {rels}"
        );
        assert!(
            rels.contains("Target=\"docProps/app.xml\""),
            "app relationship present: {rels}"
        );
    }

    #[test]
    fn docx_export_emits_core_props_and_default_language() {
        let bytes = docx_from_model(&meta_doc());
        assert_ooxml_docprops(&bytes);
        // Default-run language wired into styles.xml.
        let styles = String::from_utf8(entry(&bytes, "word/styles.xml").unwrap()).unwrap();
        assert!(
            styles.contains("<w:docDefaults><w:rPrDefault><w:rPr><w:lang w:val=\"fr-FR\"/>"),
            "default language: {styles}"
        );
    }

    #[test]
    fn xlsx_export_emits_core_props() {
        let bytes = xlsx_from_model(&meta_sheet_doc());
        assert_ooxml_docprops(&bytes);
    }

    #[test]
    fn pptx_export_emits_core_props() {
        let bytes = pptx_from_model(&meta_slide_doc());
        assert_ooxml_docprops(&bytes);
    }

    /// Assert an ODF package emits `meta.xml` with the model metadata, lists it
    /// in the manifest, and sets the default-style language in `styles.xml`.
    fn assert_odf_meta(bytes: &[u8]) {
        let meta = String::from_utf8(entry(bytes, "meta.xml").unwrap()).unwrap();
        assert!(
            meta.contains("<dc:title>My &lt;Title&gt;</dc:title>"),
            "title escaped: {meta}"
        );
        assert!(
            meta.contains("<dc:creator>Jane Doe</dc:creator>"),
            "creator: {meta}"
        );
        assert!(
            meta.contains("<dc:subject>Quarterly</dc:subject>"),
            "subject: {meta}"
        );
        assert!(
            meta.contains("<dc:language>fr-FR</dc:language>"),
            "language: {meta}"
        );
        assert!(
            meta.contains("<meta:keyword>alpha</meta:keyword>"),
            "keyword alpha: {meta}"
        );
        assert!(
            meta.contains("<meta:keyword>beta</meta:keyword>"),
            "keyword beta: {meta}"
        );
        assert!(
            meta.contains("<meta:generator>GigaPDF</meta:generator>"),
            "generator: {meta}"
        );

        let manifest = String::from_utf8(entry(bytes, "META-INF/manifest.xml").unwrap()).unwrap();
        assert!(
            manifest.contains("manifest:full-path=\"meta.xml\""),
            "meta.xml in manifest: {manifest}"
        );

        let styles = String::from_utf8(entry(bytes, "styles.xml").unwrap()).unwrap();
        assert!(
            styles.contains("<style:default-style style:family=\"paragraph\">"),
            "default-style present: {styles}"
        );
        assert!(
            styles.contains("fo:language=\"fr\"") && styles.contains("fo:country=\"FR\""),
            "fo:language/country split from BCP-47: {styles}"
        );
    }

    #[test]
    fn odt_export_emits_meta_and_default_language() {
        assert_odf_meta(&odt_from_model(&meta_doc()));
    }

    #[test]
    fn ods_export_emits_meta_and_default_language() {
        assert_odf_meta(&ods_from_model(&meta_sheet_doc()));
    }

    #[test]
    fn odp_export_emits_meta_and_default_language() {
        assert_odf_meta(&odp_from_model(&meta_slide_doc()));
    }

    #[test]
    fn export_omits_absent_metadata_fields() {
        // A document with no metadata must not fabricate any field, and the ODF
        // default-style language block must be absent entirely.
        let doc = sample_doc();
        let docx = docx_from_model(&doc);
        let core = String::from_utf8(entry(&docx, "docProps/core.xml").unwrap()).unwrap();
        assert!(!core.contains("<dc:title>"), "no title fabricated: {core}");
        assert!(
            !core.contains("<dc:creator>"),
            "no creator fabricated: {core}"
        );
        assert!(
            !core.contains("<dc:language>"),
            "no language fabricated: {core}"
        );
        let styles = String::from_utf8(entry(&docx, "word/styles.xml").unwrap()).unwrap();
        assert!(
            !styles.contains("<w:docDefaults>"),
            "no default lang: {styles}"
        );

        let odt = odt_from_model(&doc);
        let meta = String::from_utf8(entry(&odt, "meta.xml").unwrap()).unwrap();
        assert!(!meta.contains("<dc:title>"), "ODF no title: {meta}");
        assert!(!meta.contains("<dc:language>"), "ODF no language: {meta}");
        let odt_styles = String::from_utf8(entry(&odt, "styles.xml").unwrap()).unwrap();
        assert!(
            !odt_styles.contains("<style:default-style"),
            "ODF no default-style when language absent: {odt_styles}"
        );
    }

    // ───────────────────── #2 Medium: office-export fidelity ─────────────────────

    /// Build a one-slide deck whose single body placeholder holds `block`.
    fn one_slide_doc(block: Block) -> Document {
        use crate::model::{Placeholder, PlaceholderRole, Slide, SlideBlock};
        let slide = Slide {
            geometry: crate::model::PageGeometry {
                width: 960.0,
                height: 540.0,
                margins: crate::model::Margins::uniform(0.0),

                ..Default::default()
            },
            shapes: Vec::new(),
            placeholders: vec![Placeholder {
                role: PlaceholderRole::Body,
                block: Block {
                    frame: Some(crate::model::Rect::new(40.0, 40.0, 880.0, 400.0)),
                    ..block
                },
            }],
            notes: None,
            background: None,
            ..Default::default()
        };
        Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![Block {
                        kind: BlockKind::Slide(SlideBlock {
                            slides: vec![slide],
                        }),
                        ..Default::default()
                    }],
                    absolute: true,
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// #2 — a PPTX slide paragraph carries spacing/indent/line-height, not just
    /// alignment: `a:pPr@marL/@indent` (EMU) + `a:spcBef`/`a:spcAft` + `a:lnSpc`.
    #[test]
    fn pptx_paragraph_emits_spacing_indent_line_height() {
        let para = Block {
            kind: BlockKind::Paragraph(Paragraph {
                style: ParagraphStyle {
                    align: Align::Center,
                    space_before_pt: 6.0,
                    space_after_pt: 12.0,
                    indent_left_pt: 18.0,
                    indent_right_pt: 9.0,
                    first_line_pt: 24.0,
                    line_height: LineHeight::Multiple(1.5),

                    ..Default::default()
                },
                runs: vec![run("spaced")],
                ..Default::default()
            }),
            ..Default::default()
        };
        let bytes = pptx_from_model(&one_slide_doc(para));
        let slide = String::from_utf8(entry(&bytes, "ppt/slides/slide1.xml").unwrap()).unwrap();

        // Indent attributes in EMU (18pt = 228600 EMU; 24pt = 304800; 9pt = 114300).
        assert!(slide.contains("marL=\"228600\""), "left indent: {slide}");
        assert!(slide.contains("marR=\"114300\""), "right indent: {slide}");
        assert!(
            slide.contains("indent=\"304800\""),
            "first-line indent: {slide}"
        );
        assert!(slide.contains("algn=\"ctr\""), "alignment kept");
        // Spacing children (points×100) and a 150% line height (×1000th-percent).
        assert!(
            slide.contains("<a:spcBef><a:spcPts val=\"600\"/></a:spcBef>"),
            "space-before: {slide}"
        );
        assert!(
            slide.contains("<a:spcAft><a:spcPts val=\"1200\"/></a:spcAft>"),
            "space-after: {slide}"
        );
        assert!(
            slide.contains("<a:lnSpc><a:spcPct val=\"150000\"/></a:lnSpc>"),
            "line height 150%: {slide}"
        );
        assert!(slide.contains("spaced"), "run text preserved");
    }

    /// #2 — an ODT run-level inline image becomes a `draw:image` anchored as a
    /// character (not dropped) and is interned into a `Pictures/` part + manifest.
    #[test]
    fn odt_run_image_emits_draw_image_and_picture_part() {
        let mut resources = crate::model::ResourceTable::default();
        let png = vec![0x89, b'P', b'N', b'G', 9, 8, 7];
        resources.images.insert(
            42,
            crate::model::ImageResource {
                bytes: png.clone(),
                format: "png".to_string(),
            },
        );
        // A paragraph mixing text and an inline image in the *same* run list.
        let p = Block {
            kind: BlockKind::Paragraph(Paragraph {
                runs: vec![
                    run("before "),
                    Inline::Image(ImageRef {
                        resource: 42,
                        alt: Some("logo".to_string()),
                        ..Default::default()
                    }),
                    run(" after"),
                ],
                ..Default::default()
            }),
            ..Default::default()
        };
        let doc = Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![p],
                    absolute: false,
                }],
                ..Default::default()
            }],
            resources,
            ..Default::default()
        };
        let bytes = odt_from_model(&doc);
        let content = String::from_utf8(entry(&bytes, "content.xml").unwrap()).unwrap();

        // The image is an as-char draw:frame/draw:image *inside* the paragraph,
        // alongside its sibling text spans (so it is not a standalone paragraph).
        assert!(
            content.contains("text:anchor-type=\"as-char\""),
            "inline anchor: {content}"
        );
        assert!(
            content.contains("<draw:image xlink:href=\"Pictures/img1.png\""),
            "draw:image referencing the picture part: {content}"
        );
        assert!(
            content.contains("before") && content.contains("after"),
            "text kept"
        );
        // The blob is embedded and declared in the manifest.
        assert_eq!(
            entry(&bytes, "Pictures/img1.png").as_deref(),
            Some(png.as_slice()),
            "picture part embedded"
        );
        let manifest = String::from_utf8(entry(&bytes, "META-INF/manifest.xml").unwrap()).unwrap();
        assert!(
            manifest.contains("full-path=\"Pictures/img1.png\"")
                && manifest.contains("media-type=\"image/png\""),
            "picture declared in manifest: {manifest}"
        );
    }

    /// #2 — a PPTX run-level external hyperlink becomes an `a:hlinkClick r:id` on
    /// the run, with a matching `hyperlink` relationship (External) in the slide
    /// rels.
    #[test]
    fn pptx_run_link_emits_hlinkclick_and_rels_entry() {
        let p = Block {
            kind: BlockKind::Paragraph(Paragraph {
                runs: vec![
                    run("see "),
                    Inline::Link {
                        href: LinkTarget::Url("https://example.com/".to_string()),
                        children: vec![run("the site")],
                    },
                ],
                ..Default::default()
            }),
            ..Default::default()
        };
        let bytes = pptx_from_model(&one_slide_doc(p));
        let slide = String::from_utf8(entry(&bytes, "ppt/slides/slide1.xml").unwrap()).unwrap();
        let rels =
            String::from_utf8(entry(&bytes, "ppt/slides/_rels/slide1.xml.rels").unwrap()).unwrap();

        // The link run carries an a:hlinkClick referencing rId1 (first rel).
        assert!(
            slide.contains("<a:hlinkClick r:id=\"rId1\"/>"),
            "hlinkClick on the run: {slide}"
        );
        assert!(slide.contains("the site"), "link text preserved");
        // The rels file declares an External hyperlink relationship to the URL.
        assert!(
            rels.contains("Id=\"rId1\"")
                && rels.contains(
                    "Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink\""
                )
                && rels.contains("Target=\"https://example.com/\"")
                && rels.contains("TargetMode=\"External\""),
            "hyperlink relationship: {rels}"
        );
    }

    /// #2 — an ODT list whose items carry a level-2 (`level == 1`) item nests a
    /// `text:list` inside the level-1 item's `text:list-item`.
    #[test]
    fn odt_list_nesting_emits_nested_text_list() {
        let item = |text: &str, level: u8| ListItem {
            blocks: vec![para(text)],
            level,
            label: None,
        };
        let list = Block {
            kind: BlockKind::List(List {
                ordered: false,
                marker: ListMarker::Bullet('•'),
                items: vec![item("top", 0), item("nested", 1), item("back", 0)],

                ..Default::default()
            }),
            ..Default::default()
        };
        let doc = Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![list],
                    absolute: false,
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let bytes = odt_from_model(&doc);
        let content = String::from_utf8(entry(&bytes, "content.xml").unwrap()).unwrap();

        // Two `<text:list>` opens: the outer list + the nested one (level-2 item).
        assert_eq!(
            content.matches("<text:list ").count() + content.matches("<text:list>").count(),
            2,
            "outer + one nested list: {content}"
        );
        // The nested list lives inside the level-1 item: its paragraph is followed
        // by a `<text:list><text:list-item>` *before* that item closes — the
        // canonical ODF nesting shape (the level-1 `</text:list-item>` comes after
        // the nested list, not before it).
        assert!(
            content.contains("</text:p><text:list><text:list-item>"),
            "nested list inside the level-1 item: {content}"
        );
        // The nested item's `</text:list-item>` is immediately followed by the
        // nested list's close, then the parent item's close (proper unwinding).
        assert!(
            content.contains(
                "nested</text:span></text:p></text:list-item></text:list></text:list-item>"
            ),
            "nested item unwinds back to the parent item: {content}"
        );
        for t in ["top", "nested", "back"] {
            assert!(content.contains(t), "item text {t} preserved");
        }
    }

    /// #2 — an ODT table emits a cell `fo:border` (from the table border) and a
    /// `style:row-height` on the row carrying an explicit height.
    #[test]
    fn odt_table_emits_cell_border_and_row_height() {
        let table = Block {
            kind: BlockKind::Table(Table {
                rows: vec![Row {
                    cells: vec![
                        Cell {
                            blocks: vec![para("a")],
                            ..Default::default()
                        },
                        Cell {
                            blocks: vec![para("b")],
                            ..Default::default()
                        },
                    ],
                    height: Some(30.0),
                    is_header: false,
                }],
                col_widths: vec![100.0, 100.0],
                border: crate::model::BorderStyle {
                    width: 1.5,
                    color: [0.0, 0.0, 0.0],
                },
            }),
            ..Default::default()
        };
        let doc = Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![table],
                    absolute: false,
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let bytes = odt_from_model(&doc);
        let content = String::from_utf8(entry(&bytes, "content.xml").unwrap()).unwrap();

        // Cell border (1.5pt solid black) on a table-cell style.
        assert!(
            content.contains("fo:border=\"1.5pt solid #000000\""),
            "cell border: {content}"
        );
        // Row height on a table-row style, referenced by the row.
        assert!(
            content.contains("style:family=\"table-row\"")
                && content.contains("style:row-height=\"30pt\""),
            "row height style: {content}"
        );
        assert!(
            content.contains("<table:table-row table:style-name="),
            "row references its height style: {content}"
        );
    }

    /// #2 — super/subscript runs reach ODT (`style:text-position`) and PPTX
    /// (`a:rPr@baseline`), not only DOCX. A `VAlign::Super` run → `super 58%` /
    /// `baseline="30000"`; a `VAlign::Sub` run → `sub 58%` / `baseline="-25000"`.
    #[test]
    fn odt_and_pptx_emit_super_and_subscript() {
        let sup = |text: &str, va: VAlign| {
            Inline::Run(InlineRun {
                text: text.to_string(),
                style: CharStyle {
                    vertical_align: va,
                    ..Default::default()
                },
                source_index: None,
            })
        };
        let para = Block {
            kind: BlockKind::Paragraph(Paragraph {
                runs: vec![sup("up", VAlign::Super), sup("down", VAlign::Sub)],
                ..Default::default()
            }),
            ..Default::default()
        };

        // ODT: a `style:text-position` on each span's automatic text style.
        let odt = odt_from_model(&doc_with(para.clone()));
        let content = String::from_utf8(entry(&odt, "content.xml").unwrap()).unwrap();
        assert!(
            content.contains("style:text-position=\"super 58%\""),
            "ODT superscript text-position: {content}"
        );
        assert!(
            content.contains("style:text-position=\"sub 58%\""),
            "ODT subscript text-position: {content}"
        );

        // PPTX: a `baseline` attribute on each run's `a:rPr`.
        let pptx = pptx_from_model(&one_slide_doc(para));
        let slide = String::from_utf8(entry(&pptx, "ppt/slides/slide1.xml").unwrap()).unwrap();
        assert!(
            slide.contains("baseline=\"30000\""),
            "PPTX superscript baseline: {slide}"
        );
        assert!(
            slide.contains("baseline=\"-25000\""),
            "PPTX subscript baseline: {slide}"
        );
    }

    /// #2 — underline + strike on a spreadsheet cell reach the per-cell font:
    /// XLSX `<u/>` + `<strike/>`, ODS `style:text-underline-style` +
    /// `style:text-line-through-style`.
    #[test]
    fn spreadsheet_cell_font_emits_underline_and_strike() {
        let sheet = Sheet {
            name: "S".to_string(),
            rows: vec![SheetRow {
                cells: vec![SheetCell {
                    value: CellValue::Text("ul".to_string()),
                    style: CharStyle {
                        underline: true,
                        strike: true,
                        ..Default::default()
                    },
                    ..Default::default()
                }],
                height: None,
            }],
            merges: Vec::new(),
            col_widths: Vec::new(),
            ..Default::default()
        };

        // XLSX: `<u/>` and `<strike/>` in the cell's `<font>` record.
        let xlsx = xlsx_from_model(&sheet_doc(sheet.clone()));
        let styles = String::from_utf8(entry(&xlsx, "xl/styles.xml").unwrap()).unwrap();
        assert!(styles.contains("<u/>"), "XLSX underline font: {styles}");
        assert!(styles.contains("<strike/>"), "XLSX strike font: {styles}");

        // ODS: the cell's text style carries underline + line-through.
        let ods = ods_from_model(&sheet_doc(sheet));
        let content = String::from_utf8(entry(&ods, "content.xml").unwrap()).unwrap();
        assert!(
            content.contains("style:text-underline-style=\"solid\""),
            "ODS underline style: {content}"
        );
        assert!(
            content.contains("style:text-line-through-style=\"solid\""),
            "ODS strike style: {content}"
        );
    }

    /// #2 — a block-level `Shape` reaches ODT (it was silently dropped). A framed
    /// filled rectangle → a `draw:rect` carrying its geometry and a graphic style
    /// with the fill, wrapped in a `text:p` as ODF requires for a body drawing.
    #[test]
    fn odt_block_shape_emits_draw_shape_with_geometry_and_fill() {
        let shape = Shape {
            segments: vec![
                PathSeg::Move(0.0, 0.0),
                PathSeg::Line(100.0, 0.0),
                PathSeg::Line(100.0, 50.0),
                PathSeg::Line(0.0, 50.0),
                PathSeg::Close,
            ],
            fill: Some([1.0, 0.0, 0.0]),
            stroke: None,
            stroke_width: 0.0,
            dash: Vec::new(),
            ..Default::default()
        };
        let block = Block {
            frame: Some(crate::model::Rect::new(72.0, 144.0, 100.0, 50.0)),
            kind: BlockKind::Shape(shape),
            ..Default::default()
        };
        let bytes = odt_from_model(&doc_with(block));
        let content = String::from_utf8(entry(&bytes, "content.xml").unwrap()).unwrap();

        // A `draw:rect` placed at the frame, anchored inside a paragraph.
        assert!(
            content.contains("<text:p><draw:rect "),
            "shape wrapped in a paragraph: {content}"
        );
        assert!(
            content.contains("svg:x=\"72pt\"")
                && content.contains("svg:y=\"144pt\"")
                && content.contains("svg:width=\"100pt\"")
                && content.contains("svg:height=\"50pt\""),
            "shape geometry from the block frame: {content}"
        );
        // A graphic style carrying the red fill is referenced by the shape.
        assert!(
            content.contains("draw:fill=\"solid\"")
                && content.contains("draw:fill-color=\"#FF0000\""),
            "shape fill style: {content}"
        );
    }

    /// #2 — an internal `LinkTarget::Page` jump is emitted (it was dropped). DOCX:
    /// a `HYPERLINK \l "page{N}"` field + the matching `w:bookmarkStart`; ODT: a
    /// `text:a xlink:href="#page{N}"` + the page `text:bookmark`; PPTX: an
    /// `a:hlinkClick action="…hlinksldjump"` + a `slide` relationship.
    #[test]
    fn internal_page_link_emits_anchor_and_jump() {
        // Two pages so a jump to page 2 (`LinkTarget::Page(1)`) has a real target.
        let linked_para = Block {
            kind: BlockKind::Paragraph(Paragraph {
                runs: vec![Inline::Link {
                    href: LinkTarget::Page(1),
                    children: vec![run("go to page 2")],
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        let two_page_doc = || Document {
            sections: vec![Section {
                pages: vec![
                    Page {
                        blocks: vec![linked_para.clone()],
                        absolute: false,
                    },
                    Page {
                        blocks: vec![para("page two")],
                        absolute: false,
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        };

        // DOCX: the field jumps to the bookmark, and the bookmark exists at page 2.
        let docx = docx_from_model(&two_page_doc());
        let doc = String::from_utf8(entry(&docx, "word/document.xml").unwrap()).unwrap();
        assert!(
            doc.contains("HYPERLINK \\l \"page2\""),
            "DOCX internal hyperlink field: {doc}"
        );
        assert!(
            doc.contains("<w:bookmarkStart w:id=\"2\" w:name=\"page2\"/>"),
            "DOCX page-2 bookmark target: {doc}"
        );

        // ODT: the anchor href + the page bookmark.
        let odt = odt_from_model(&two_page_doc());
        let content = String::from_utf8(entry(&odt, "content.xml").unwrap()).unwrap();
        assert!(
            content.contains("<text:a xlink:type=\"simple\" xlink:href=\"#page2\">"),
            "ODT internal link href: {content}"
        );
        assert!(
            content.contains("<text:bookmark text:name=\"page2\"/>"),
            "ODT page-2 bookmark target: {content}"
        );

        // PPTX: each page becomes a slide; the jump targets slide 2 via a slide
        // relationship + `ppaction://hlinksldjump`.
        let pptx = pptx_from_model(&two_page_doc());
        let slide1 = String::from_utf8(entry(&pptx, "ppt/slides/slide1.xml").unwrap()).unwrap();
        let rels =
            String::from_utf8(entry(&pptx, "ppt/slides/_rels/slide1.xml.rels").unwrap()).unwrap();
        assert!(
            slide1.contains("action=\"ppaction://hlinksldjump\""),
            "PPTX slide-jump action: {slide1}"
        );
        assert!(
            rels.contains(
                "Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide\""
            ) && rels.contains("Target=\"slide2.xml\""),
            "PPTX slide-2 relationship: {rels}"
        );
    }

    // ─────────────── fidelity-audit regression tests (gaps 1/2/4/5/6/9) ───────────────

    /// A thin horizontal-rule [`Shape`] block (the classic letterhead filet).
    fn rule_shape_block() -> Block {
        Block {
            kind: BlockKind::Shape(Shape {
                segments: vec![PathSeg::Move(0.0, 0.0), PathSeg::Line(500.0, 0.0)],
                fill: None,
                stroke: Some([0.0, 0.0, 0.0]),
                stroke_width: 0.75,
                dash: Vec::new(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Every element opened with an `ns:` prefix in `xml` must have that prefix
    /// declared (`xmlns:ns=`) — the exact failure mode that made a whole DOCX
    /// package unreadable was `<v:rect>` in a header part that never declared
    /// `xmlns:v`.
    fn assert_prefixes_declared(xml: &str, part: &str) {
        let mut i = 0;
        let bytes = xml.as_bytes();
        while let Some(off) = xml[i..].find('<') {
            let start = i + off + 1;
            let rest = &xml[start..];
            // Element names only (skip closing tags, PIs, comments).
            if rest.starts_with(['/', '?', '!']) {
                i = start;
                continue;
            }
            let name_end = rest.find([' ', '>', '/']).unwrap_or(rest.len());
            let name = &rest[..name_end];
            if let Some((prefix, _)) = name.split_once(':') {
                assert!(
                    xml.contains(&format!("xmlns:{prefix}=")),
                    "{part}: prefix `{prefix}:` used but xmlns:{prefix} is not declared"
                );
            }
            i = start + name_end.min(bytes.len() - start);
        }
    }

    #[test]
    fn hdrftr_with_shape_declares_vml_namespaces() {
        // Gap 1 (P0): a header/footer containing a shape renders VML
        // (`<v:rect>`/`<v:shape>`); the part root must declare the VML
        // namespaces or Word/LibreOffice refuse the whole file.
        let doc = Document {
            sections: vec![Section {
                header: Some(vec![rule_shape_block()]),
                footer: Some(vec![rule_shape_block(), para("mentions légales")]),
                pages: vec![Page {
                    blocks: vec![para("body")],
                    absolute: false,
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let bytes = docx_from_model(&doc);
        let files = read_zip(&bytes);
        for part in ["word/header1.xml", "word/footer1.xml", "word/document.xml"] {
            let xml = String::from_utf8(files.get(part).unwrap().clone()).unwrap();
            assert!(
                xml.contains("xmlns:v=\"urn:schemas-microsoft-com:vml\""),
                "{part} declares xmlns:v: {xml}"
            );
            assert_prefixes_declared(&xml, part);
        }
        let header = String::from_utf8(files.get("word/header1.xml").unwrap().clone()).unwrap();
        assert!(
            header.contains("<v:shape") || header.contains("<v:rect"),
            "the header shape actually rendered as VML: {header}"
        );
    }

    #[test]
    fn hdrftr_page_number_caption_becomes_dynamic_fields() {
        // Gap 9: a reconstructed footer freezes one page's number ("Page 1 / 2");
        // the DOCX export must swap the digits for PAGE / NUMPAGES fields so
        // every page numbers itself.
        let doc = Document {
            sections: vec![Section {
                footer: Some(vec![para("Page 1 / 2")]),
                pages: vec![
                    Page {
                        blocks: vec![para("one")],
                        absolute: false,
                    },
                    Page {
                        blocks: vec![para("two")],
                        absolute: false,
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        let bytes = docx_from_model(&doc);
        let files = read_zip(&bytes);
        let footer = String::from_utf8(files.get("word/footer1.xml").unwrap().clone()).unwrap();
        assert!(
            footer.contains("<w:fldSimple w:instr=\" PAGE \">"),
            "current page is a dynamic PAGE field: {footer}"
        );
        assert!(
            footer.contains("<w:fldSimple w:instr=\" NUMPAGES \">"),
            "page total is a dynamic NUMPAGES field: {footer}"
        );
        assert!(
            footer.contains("Page </w:t>"),
            "the literal `Page ` prefix survives: {footer}"
        );
    }

    #[test]
    fn page_number_caption_parser_is_conservative() {
        // Recognized shapes…
        for ok in [
            "Page 3",
            "Page 1 / 2",
            "page 2 sur 10",
            "Page 4 of 9",
            "3 / 12",
            "3/12",
            "- 7 -",
        ] {
            assert!(
                parse_page_number_text(ok).is_some(),
                "`{ok}` is a page-number caption"
            );
        }
        // …and everything else renders verbatim (frozen is better than wrong).
        for no in [
            "Chapitre 1",
            "Page",
            "3",
            "Page trois",
            "3 of 10 — © ACME",
            "01/02/2026",
            "Article 313-1",
        ] {
            assert!(
                parse_page_number_text(no).is_none(),
                "`{no}` must NOT be treated as a page-number caption"
            );
        }
    }

    #[test]
    fn positioned_image_block_floats_via_wp_anchor() {
        // Gap 5: an image block carrying a frame (every reconstructed image
        // does) must anchor at its page position with its real size, not
        // collapse into the flow as a default-sized inline drawing.
        let mut doc = image_doc("png");
        doc.sections[0].pages[0].blocks[0].frame =
            Some(crate::model::Rect::new(56.7, 700.0, 112.6, 85.1));
        let page_h = doc.sections[0].geometry.height;
        let bytes = docx_from_model(&doc);
        let files = read_zip(&bytes);
        let xml = String::from_utf8(files.get("word/document.xml").unwrap().clone()).unwrap();
        assert!(xml.contains("<wp:anchor "), "framed image anchors: {xml}");
        assert!(
            xml.contains(&format!("<wp:posOffset>{}</wp:posOffset>", emu(56.7))),
            "X offset in EMU from the frame: {xml}"
        );
        // Model frames and `wp:posOffset` are BOTH top-down (origin top-left):
        // the Y offset passes through unflipped — the old page-height flip
        // mirrored every anchored image of a PDF-reconstructed model.
        let _ = page_h;
        assert!(
            xml.contains(&format!("<wp:posOffset>{}</wp:posOffset>", emu(700.0))),
            "Y offset in EMU is the frame's top-down Y: {xml}"
        );
        assert!(
            xml.contains(&format!(
                "<wp:extent cx=\"{}\" cy=\"{}\"/>",
                emu(112.6),
                emu(85.1)
            )),
            "extent = the frame size: {xml}"
        );
        assert!(
            xml.contains("behindDoc=\"0\""),
            "the picture sits with the content, not behind it: {xml}"
        );

        // A frameless image (flow import) keeps the inline drawing.
        let bytes = docx_from_model(&image_doc("png"));
        let files = read_zip(&bytes);
        let xml = String::from_utf8(files.get("word/document.xml").unwrap().clone()).unwrap();
        assert!(
            xml.contains("<wp:inline ") && !xml.contains("<wp:anchor "),
            "frameless image stays inline: {xml}"
        );
    }

    #[test]
    fn pdf_fallback_slides_position_framed_blocks_as_shapes() {
        // Gap 2 (PPTX near-empty): the page fallback must turn framed blocks
        // into positioned slide shapes — `p:pic` with embedded media for
        // images, positioned text boxes for prose — instead of flattening
        // everything into one Body placeholder that ignores `Block::frame`.
        let mut resources = crate::model::ResourceTable::default();
        resources.images.insert(
            9,
            crate::model::ImageResource {
                bytes: vec![0x89, b'P', b'N', b'G', 1, 2, 3],
                format: "png".to_string(),
            },
        );
        let image = Block {
            frame: Some(crate::model::Rect::new(100.0, 50.0, 200.0, 150.0)),
            kind: BlockKind::Image(ImageRef {
                resource: 9,
                alt: None,
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut text = para("hello slide");
        text.frame = Some(crate::model::Rect::new(72.0, 300.0, 300.0, 40.0));
        // The reconstruction anchors indents at the page edge; inside a
        // positioned box that offset must be stripped (or the text shifts
        // right a second time).
        if let BlockKind::Paragraph(p) = &mut text.kind {
            p.style.indent_left_pt = 72.0;
        }
        let doc = Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![image, text],
                    absolute: false,
                }],
                ..Default::default()
            }],
            resources,
            ..Default::default()
        };

        let bytes = pptx_from_model(&doc);
        let files = read_zip(&bytes);
        let slide = String::from_utf8(files.get("ppt/slides/slide1.xml").unwrap().clone()).unwrap();
        assert!(
            files.keys().any(|k| k.starts_with("ppt/media/")),
            "the slide's image media is embedded: {:?}",
            files.keys().collect::<Vec<_>>()
        );
        assert!(slide.contains("<p:pic>"), "framed image → p:pic: {slide}");
        assert!(
            slide.contains(&format!(
                "<a:off x=\"{}\" y=\"{}\"/>",
                emu(100.0),
                emu(50.0)
            )),
            "picture offset from its frame: {slide}"
        );
        assert!(
            slide.contains(&format!(
                "<a:off x=\"{}\" y=\"{}\"/>",
                emu(72.0),
                emu(300.0)
            )),
            "text box offset from its frame: {slide}"
        );
        assert!(slide.contains("hello slide"), "the prose survives: {slide}");
        assert!(
            !slide.contains(&format!("marL=\"{}\"", (72.0f64 * 12700.0) as i64)),
            "page-anchored indent stripped inside the positioned box: {slide}"
        );

        // A frameless page (flow import) keeps the Body-placeholder fallback.
        let flow_doc = Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![para("flowing text")],
                    absolute: false,
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let bytes = pptx_from_model(&flow_doc);
        let files = read_zip(&bytes);
        let slide = String::from_utf8(files.get("ppt/slides/slide1.xml").unwrap().clone()).unwrap();
        assert!(
            slide.contains("<p:ph type=\"body\""),
            "frameless blocks keep the Body placeholder: {slide}"
        );
    }

    // ── audit fidelity wave 1: typed cells, backgrounds, hyperlinks, rot, tables ──

    #[test]
    fn typed_sheet_value_covers_locale_forms() {
        // Strict core (shared with the CSV importer): bare numbers/booleans.
        assert_eq!(
            typed_sheet_value("1234.5"),
            (CellValue::Number(1234.5), None)
        );
        assert_eq!(typed_sheet_value("TRUE"), (CellValue::Bool(true), None));
        // French grouped thousands + decimal comma.
        let (v, f) = typed_sheet_value("1 234,50");
        assert_eq!(v, CellValue::Number(1234.5));
        assert_eq!(f.as_deref(), Some("#,##0.00"));
        // NBSP grouping (what a PDF reconstruction actually extracts).
        let (v, _) = typed_sheet_value("1\u{a0}234,50");
        assert_eq!(v, CellValue::Number(1234.5));
        // English and German grouped forms.
        assert_eq!(
            typed_sheet_value("1,234.50").0,
            CellValue::Number(1234.5),
            "en grouping + dot decimal"
        );
        assert_eq!(
            typed_sheet_value("1.234,50").0,
            CellValue::Number(1234.5),
            "de grouping + comma decimal"
        );
        assert_eq!(
            typed_sheet_value("1 234 567").0,
            CellValue::Number(1_234_567.0)
        );
        // Percentages: value scaled, percent number format attached.
        let (v, f) = typed_sheet_value("15,50 %");
        assert_eq!(v, CellValue::Number(15.5 / 100.0));
        assert_eq!(f.as_deref(), Some("0.00%"));
        let (v, f) = typed_sheet_value("50%");
        assert_eq!(v, CellValue::Number(0.5));
        assert_eq!(f.as_deref(), Some("0%"));
        // Currency-tagged amounts keep their symbol in the number format.
        let (v, f) = typed_sheet_value("1 234,50 €");
        assert_eq!(v, CellValue::Number(1234.5));
        assert_eq!(f.as_deref(), Some("#,##0.00\" €\""));
        let (v, f) = typed_sheet_value("$1,234.50");
        assert_eq!(v, CellValue::Number(1234.5));
        assert_eq!(f.as_deref(), Some("\"$\"#,##0.00"));
        // Plain decimal comma.
        assert_eq!(typed_sheet_value("15,5").0, CellValue::Number(15.5));
        // Conservative refusals — everything ambiguous stays text.
        assert_eq!(
            typed_sheet_value("abc"),
            (CellValue::Text("abc".to_string()), None)
        );
        assert_eq!(
            typed_sheet_value("12 rue X"),
            (CellValue::Text("12 rue X".to_string()), None),
            "an address starts with a number but contains letters"
        );
        assert_eq!(
            typed_sheet_value("1,234").0,
            CellValue::Text("1,234".to_string()),
            "ambiguous en-grouping vs fr-decimal stays text"
        );
        assert_eq!(
            typed_sheet_value("01234").0,
            CellValue::Text("01234".to_string()),
            "leading-zero code stays text"
        );
        assert_eq!(
            typed_sheet_value("12 34").0,
            CellValue::Text("12 34".to_string()),
            "malformed grouping stays text"
        );
        assert_eq!(
            typed_sheet_value("+1-555-0100").0,
            CellValue::Text("+1-555-0100".to_string()),
            "phone-like token stays text"
        );
    }

    #[test]
    fn table_to_sheet_bridge_types_numbers_not_text() {
        // A prose-page table (the PDF-reconstruction shape) exported to XLSX
        // must carry typed Number cells — previously EVERY cell came out as
        // text. Headers keep their labels verbatim (even numeric-looking ones).
        let mk = |t: &str| Cell {
            blocks: vec![para(t)],
            ..Default::default()
        };
        let table = Block {
            kind: BlockKind::Table(Table {
                rows: vec![
                    Row {
                        cells: vec![mk("Montant"), mk("2024")],
                        height: None,
                        is_header: true,
                    },
                    Row {
                        cells: vec![mk("1 234,50"), mk("15,50 %")],
                        height: None,
                        is_header: false,
                    },
                    Row {
                        cells: vec![mk("abc"), mk("12 rue X")],
                        height: None,
                        is_header: false,
                    },
                ],
                col_widths: vec![120.0, 120.0],
                border: BorderStyle::default(),
            }),
            ..Default::default()
        };
        let doc = Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![table],
                    absolute: false,
                }],
                ..Default::default()
            }],
            ..Default::default()
        };

        let xlsx = xlsx_from_model(&doc);
        let xml = String::from_utf8(entry(&xlsx, "xl/worksheets/sheet1.xml").unwrap()).unwrap();
        assert!(
            xml.contains("<v>1234.5</v>"),
            "the amount is a real number cell, not inlineStr: {xml}"
        );
        let styles = String::from_utf8(entry(&xlsx, "xl/styles.xml").unwrap()).unwrap();
        assert!(
            styles.contains("#,##0.00") && styles.contains("0.00%"),
            "locale number formats registered: {styles}"
        );

        // Round trip: the typed values survive the XLSX importer.
        let model = crate::convert::office_import::xlsx_to_model(&read_zip(&xlsx));
        let sheets = collect_sheets(&model);
        assert_eq!(sheets.len(), 1);
        let s = &sheets[0];
        assert_eq!(
            s.rows[0].cells[1].value,
            CellValue::Text("2024".to_string()),
            "numeric-looking header label stays text"
        );
        assert_eq!(s.rows[1].cells[0].value, CellValue::Number(1234.5));
        assert_eq!(s.rows[1].cells[1].value, CellValue::Number(15.5 / 100.0));
        assert_eq!(s.rows[2].cells[0].value, CellValue::Text("abc".to_string()));
        assert_eq!(
            s.rows[2].cells[1].value,
            CellValue::Text("12 rue X".to_string()),
            "an address stays text"
        );
    }

    /// A one-slide deck with the given background and free shapes.
    fn bg_slide_doc(background: Option<[f64; 3]>, shapes: Vec<Block>) -> Document {
        use crate::model::{Slide, SlideBlock};
        let slide = Slide {
            geometry: crate::model::PageGeometry {
                width: 960.0,
                height: 540.0,
                margins: crate::model::Margins::uniform(0.0),
                ..Default::default()
            },
            shapes,
            placeholders: Vec::new(),
            notes: None,
            background,
            ..Default::default()
        };
        Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![Block {
                        kind: BlockKind::Slide(SlideBlock {
                            slides: vec![slide],
                        }),
                        ..Default::default()
                    }],
                    absolute: true,
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// A framed one-paragraph free shape.
    fn framed_para(text: &str) -> Block {
        Block {
            frame: Some(crate::model::Rect::new(60.0, 60.0, 400.0, 60.0)),
            ..para(text)
        }
    }

    #[test]
    fn slide_background_round_trips_pptx() {
        // 22 background-bearing slides came back with 0 after a PPTX round trip:
        // the exporter never emitted `p:bg`. It now does, as the exact shape the
        // importer reads (`p:bg/p:bgPr/a:solidFill`).
        let doc = bg_slide_doc(Some([1.0, 0.0, 0.0]), vec![framed_para("on red")]);
        let bytes = pptx_from_model(&doc);
        let slide = String::from_utf8(entry(&bytes, "ppt/slides/slide1.xml").unwrap()).unwrap();
        assert!(
            slide.contains("<p:bg><p:bgPr><a:solidFill><a:srgbClr val=\"FF0000\"/>"),
            "solid slide background emitted before p:spTree: {slide}"
        );
        assert!(
            slide.find("<p:bg>").unwrap() < slide.find("<p:spTree>").unwrap(),
            "p:bg precedes p:spTree (CT_CommonSlideData order)"
        );

        let model = crate::convert::office_import::pptx_to_model(&read_zip(&bytes));
        let slides = collect_slides(&model);
        assert_eq!(slides.len(), 1);
        let bg = slides[0]
            .background
            .expect("background survives round trip");
        assert!(
            bg.iter()
                .zip([1.0, 0.0, 0.0])
                .all(|(a, b)| (a - b).abs() < 0.01),
            "red background round-trips, got {bg:?}"
        );
        // A background-less slide emits no p:bg at all.
        let plain = pptx_from_model(&bg_slide_doc(None, vec![framed_para("plain")]));
        let slide = String::from_utf8(entry(&plain, "ppt/slides/slide1.xml").unwrap()).unwrap();
        assert!(!slide.contains("<p:bg>"), "no fake background: {slide}");
    }

    #[test]
    fn slide_background_round_trips_odp() {
        let doc = bg_slide_doc(Some([0.0, 0.0, 1.0]), vec![framed_para("on blue")]);
        let bytes = odp_from_model(&doc);
        let content = String::from_utf8(entry(&bytes, "content.xml").unwrap()).unwrap();
        assert!(
            content.contains("draw:fill=\"solid\"")
                && content.contains("draw:fill-color=\"#0000FF\""),
            "drawing-page style carries the solid fill: {content}"
        );
        assert!(
            content.contains("draw:style-name=\"dpBg0\""),
            "the page references its background style: {content}"
        );

        let model = crate::convert::office_import::odp_to_model(&read_zip(&bytes));
        let slides = collect_slides(&model);
        assert_eq!(slides.len(), 1);
        let bg = slides[0]
            .background
            .expect("background survives round trip");
        assert!(
            bg.iter()
                .zip([0.0, 0.0, 1.0])
                .all(|(a, b)| (a - b).abs() < 0.01),
            "blue background round-trips, got {bg:?}"
        );
    }

    /// A one-row sheet with an external and an in-workbook cell hyperlink.
    fn hyperlink_sheet() -> Sheet {
        Sheet {
            name: "Links".to_string(),
            rows: vec![SheetRow {
                cells: vec![
                    SheetCell {
                        value: CellValue::Text("site".to_string()),
                        hyperlink: Some("https://example.com/".to_string()),
                        ..Default::default()
                    },
                    SheetCell {
                        value: CellValue::Text("jump".to_string()),
                        hyperlink: Some("#Sheet1!A1".to_string()),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn cell_hyperlink_round_trips_xlsx() {
        // `SheetCell::hyperlink` was set by the importer but never read by the
        // writer — links silently vanished on every re-export.
        let bytes = xlsx_from_model(&sheet_doc(hyperlink_sheet()));
        let sheet_xml =
            String::from_utf8(entry(&bytes, "xl/worksheets/sheet1.xml").unwrap()).unwrap();
        assert!(
            sheet_xml.contains("<hyperlinks><hyperlink ref=\"A1\" r:id=\"rId1\"/>"),
            "external link goes through the rels: {sheet_xml}"
        );
        assert!(
            sheet_xml.contains("<hyperlink ref=\"B1\" location=\"Sheet1!A1\"/>"),
            "in-workbook link uses @location: {sheet_xml}"
        );
        let rels = String::from_utf8(
            entry(&bytes, "xl/worksheets/_rels/sheet1.xml.rels").expect("worksheet rels part"),
        )
        .unwrap();
        assert!(
            rels.contains("Target=\"https://example.com/\" TargetMode=\"External\""),
            "external target declared in the sheet rels: {rels}"
        );

        let model = crate::convert::office_import::xlsx_to_model(&read_zip(&bytes));
        let sheets = collect_sheets(&model);
        assert_eq!(
            sheets[0].rows[0].cells[0].hyperlink.as_deref(),
            Some("https://example.com/"),
            "external hyperlink survives the round trip"
        );
        assert_eq!(
            sheets[0].rows[0].cells[1].hyperlink.as_deref(),
            Some("#Sheet1!A1"),
            "in-workbook hyperlink survives the round trip"
        );
        // A link-free export stays rels-less and r:id-less.
        let plain = xlsx_from_model(&sheet_doc(Sheet {
            name: "P".to_string(),
            rows: vec![SheetRow {
                cells: vec![SheetCell {
                    value: CellValue::Text("x".to_string()),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }));
        assert!(
            entry(&plain, "xl/worksheets/_rels/sheet1.xml.rels").is_none(),
            "no rels part without links"
        );
        let plain_xml =
            String::from_utf8(entry(&plain, "xl/worksheets/sheet1.xml").unwrap()).unwrap();
        assert!(!plain_xml.contains("<hyperlinks>"));
    }

    #[test]
    fn cell_hyperlink_round_trips_ods() {
        let bytes = ods_from_model(&sheet_doc(hyperlink_sheet()));
        let content = String::from_utf8(entry(&bytes, "content.xml").unwrap()).unwrap();
        assert!(
            content.contains(
                "<text:a xlink:type=\"simple\" xlink:href=\"https://example.com/\">site</text:a>"
            ),
            "cell text wrapped in a text:a: {content}"
        );

        let model = crate::convert::office_import::ods_to_model(&read_zip(&bytes));
        let sheets = collect_sheets(&model);
        assert_eq!(
            sheets[0].rows[0].cells[0].hyperlink.as_deref(),
            Some("https://example.com/"),
            "external hyperlink survives the ODS round trip"
        );
        assert_eq!(
            sheets[0].rows[0].cells[1].hyperlink.as_deref(),
            Some("#Sheet1!A1"),
            "in-workbook hyperlink survives the ODS round trip"
        );
        assert_eq!(
            sheets[0].rows[0].cells[0].value,
            CellValue::Text("site".to_string()),
            "the link display text stays the cell value"
        );
    }

    #[test]
    fn pptx_rot_attr_maps_model_ccw_to_ooxml_cw() {
        use crate::model::Rotation;
        assert_eq!(pptx_rot_attr(Rotation::D0), "");
        // Model CCW 45° = OOXML CW 315° = 18 900 000 (60000ths).
        assert_eq!(pptx_rot_attr(Rotation::Deg(45.0)), " rot=\"18900000\"");
        assert_eq!(pptx_rot_attr(Rotation::D90), " rot=\"16200000\"");
        assert_eq!(pptx_rot_attr(Rotation::D270), " rot=\"5400000\"");
        assert_eq!(pptx_rot_attr(Rotation::Deg(360.0)), "", "full turn = none");
    }

    #[test]
    fn rotated_shape_round_trips_pptx() {
        // The importer reads `a:xfrm@rot` but the exporter never wrote it: every
        // rotated element came out straight. A 45°-CCW free text box now emits
        // `rot="18900000"` and comes back at 45°.
        let shape = Block {
            frame: Some(crate::model::Rect::new(100.0, 100.0, 300.0, 60.0)),
            rotation: crate::model::Rotation::Deg(45.0),
            ..para("tilted")
        };
        let doc = bg_slide_doc(None, vec![shape]);
        let bytes = pptx_from_model(&doc);
        let slide = String::from_utf8(entry(&bytes, "ppt/slides/slide1.xml").unwrap()).unwrap();
        assert!(
            slide.contains("<a:xfrm rot=\"18900000\">"),
            "rotation emitted on the shape transform: {slide}"
        );

        let model = crate::convert::office_import::pptx_to_model(&read_zip(&bytes));
        let slides = collect_slides(&model);
        let rotated = slides[0]
            .shapes
            .iter()
            .find(|b| b.rotation != crate::model::Rotation::D0)
            .expect("the rotated shape survives");
        assert!(
            (rotated.rotation.degrees() - 45.0).abs() < 1e-6,
            "45° CCW round-trips, got {:?}",
            rotated.rotation
        );
    }

    #[test]
    fn odp_pos_attrs_rotation_variants() {
        use crate::model::{Rect, Rotation};
        let r = Rect::new(10.0, 20.0, 100.0, 50.0);
        // Unrotated: the plain svg cluster, byte-identical to the historical
        // output (no draw:transform anywhere near it).
        assert_eq!(
            odp_pos_attrs(r, Rotation::D0),
            "svg:x=\"10pt\" svg:y=\"20pt\" svg:width=\"100pt\" svg:height=\"50pt\""
        );
        assert_eq!(
            odp_pos_attrs(r, Rotation::Deg(360.0)),
            odp_pos_attrs(r, Rotation::D0),
            "a full turn is no rotation"
        );
        // Rotated: LibreOffice-style transform — CCW radians + the lower-left
        // corner (svg:y + emitted height) — replacing svg:x/svg:y, with the
        // size attributes kept.
        let d90 = odp_pos_attrs(r, Rotation::D90);
        let want = format!(
            "draw:transform=\"rotate({}) translate(10pt 70pt)\"",
            (90.0f64).to_radians()
        );
        assert!(d90.starts_with(&want), "quarter turn transform: {d90}");
        assert!(
            !d90.contains("svg:x="),
            "transform replaces svg:x/svg:y: {d90}"
        );
        assert!(
            d90.ends_with("svg:width=\"100pt\" svg:height=\"50pt\""),
            "size attributes stay: {d90}"
        );
    }

    #[test]
    fn odp_rotated_frame_emits_draw_transform_and_round_trips() {
        // `Block.rotation` reached PPTX (`rot=`) but never ODP: every rotated
        // element opened straight in Impress. A rotated frame now carries
        // `draw:transform="rotate(<rad>) translate(<x> <y>)"` — ODF §19.228
        // radians, counter-clockwise, translate at the frame's lower-left
        // corner — and loses `svg:x`/`svg:y` to it, while the unrotated twin
        // keeps the plain svg attributes and no transform at all.
        let tilted = Block {
            frame: Some(crate::model::Rect::new(100.0, 100.0, 300.0, 60.0)),
            rotation: crate::model::Rotation::Deg(45.0),
            ..para("tilted")
        };
        let straight = Block {
            frame: Some(crate::model::Rect::new(100.0, 100.0, 300.0, 60.0)),
            ..para("straight")
        };
        let bytes = odp_from_model(&bg_slide_doc(None, vec![tilted, straight]));
        let content = String::from_utf8(entry(&bytes, "content.xml").unwrap()).unwrap();

        // translate = (x, y + h): the lower-left corner, the exact inverse of
        // the importer's `top = ty - h` (odp_frame_box_xf).
        let xf = format!(
            "draw:transform=\"rotate({}) translate(100pt 160pt)\"",
            (45.0f64).to_radians()
        );
        assert!(content.contains(&xf), "rotate+translate emitted: {content}");
        assert_eq!(
            content.matches("draw:transform").count(),
            1,
            "no transform on the unrotated frame: {content}"
        );
        assert!(
            content.contains("svg:x=\"100pt\" svg:y=\"100pt\""),
            "the unrotated frame keeps svg:x/svg:y: {content}"
        );

        // Round trip through the ODP importer: the angle folds back into the
        // model, and the transform-positioned frame lands exactly where the
        // svg-positioned twin does.
        let back =
            crate::convert::office_import::odp_to_model(&crate::convert::zip::read_zip(&bytes));
        let slides = collect_slides(&back);
        let s0 = slides.first().expect("one slide round-trips");
        let rot = s0
            .shapes
            .iter()
            .find(|b| b.rotation != crate::model::Rotation::D0)
            .expect("the rotated shape survives");
        assert!(
            (rot.rotation.degrees() - 45.0).abs() < 1e-6,
            "45° CCW round-trips, got {:?}",
            rot.rotation
        );
        let plain = s0
            .shapes
            .iter()
            .find(|b| b.rotation == crate::model::Rotation::D0)
            .expect("the straight shape survives");
        let (rf, pf) = (rot.frame.expect("frame"), plain.frame.expect("frame"));
        for (got, want, what) in [
            (rf.x, pf.x, "x"),
            (rf.y, pf.y, "y"),
            (rf.w, pf.w, "w"),
            (rf.h, pf.h, "h"),
        ] {
            assert!(
                (got - want).abs() < 1e-3,
                "{what}: transform-positioned frame ({got}) must match the \
svg-positioned twin ({want})"
            );
        }
    }

    #[test]
    fn odt_tab_stops_emit_style_tab_stops_element() {
        // `ParagraphStyle.tab_stops` reached DOCX (`<w:tabs>`) but never ODF:
        // ODT dropped every explicit tab stop. They now land as a
        // `<style:tab-stops>` *child element* of the automatic style's
        // paragraph-properties (tab stops are elements, not attributes, so the
        // shell no longer self-closes). The ODF importer does not read
        // `style:tab-stops` yet (deferred to the ODF-import wave), so this pins
        // the emitted shape.
        const CM: f64 = 28.346_456_7; // 1 cm in pt — positions follow this file's pt convention
        let tabbed = Paragraph {
            runs: vec![run("name\t42")],
            style: ParagraphStyle {
                tab_stops: vec![
                    crate::model::TabStop {
                        pos_pt: 2.0 * CM,
                        align: crate::model::TabAlign::Left,
                        leader: '\0',
                    },
                    crate::model::TabStop {
                        pos_pt: 10.0 * CM,
                        align: crate::model::TabAlign::Right,
                        leader: '.',
                    },
                ],
                ..Default::default()
            },
            ..Default::default()
        };
        let bytes = odt_from_model(&doc_with(Block {
            kind: BlockKind::Paragraph(tabbed),
            ..Default::default()
        }));
        let content = String::from_utf8(entry(&bytes, "content.xml").unwrap()).unwrap();
        let want = format!(
            "<style:paragraph-properties><style:tab-stops>\
<style:tab-stop style:position=\"{}pt\"/>\
<style:tab-stop style:position=\"{}pt\" style:type=\"right\" \
style:leader-style=\"dotted\" style:leader-text=\".\"/>\
</style:tab-stops></style:paragraph-properties>",
            num(2.0 * CM),
            num(10.0 * CM)
        );
        assert!(
            content.contains(&want),
            "both stops inside the paragraph-properties body: {content}"
        );

        // A paragraph without explicit stops keeps the self-closing shell.
        let plain = odt_from_model(&doc_with(para("plain")));
        let plain_xml = String::from_utf8(entry(&plain, "content.xml").unwrap()).unwrap();
        assert!(
            !plain_xml.contains("<style:tab-stops>"),
            "no stops → no tab-stops element: {plain_xml}"
        );
    }

    #[test]
    fn odt_tab_stops_only_override_forces_automatic_style() {
        // A paragraph whose ONLY direct formatting is its tab stops used to
        // take the "no direct formatting" shortcut — referencing the named
        // style directly and silently dropping the stops. It now gets an
        // automatic style inheriting the named style, carrying the
        // `<style:tab-stops>`; and a *named* style's own tab stops reach
        // `styles.xml` too (Decimal maps to ODF's `char` type on `.`).
        let tabbed = Paragraph {
            runs: vec![run("x\ty")],
            style_ref: Some(StyleId("Body".to_string())),
            style: ParagraphStyle {
                tab_stops: vec![crate::model::TabStop {
                    pos_pt: 72.0,
                    align: crate::model::TabAlign::Center,
                    leader: '\0',
                }],
                ..Default::default()
            },
        };
        let mut doc = doc_with(Block {
            kind: BlockKind::Paragraph(tabbed),
            ..Default::default()
        });
        doc.styles
            .named
            .insert(StyleId("Body".to_string()), NamedStyle::default());
        doc.styles.named.insert(
            StyleId("Fig".to_string()),
            NamedStyle {
                para: ParagraphStyle {
                    tab_stops: vec![crate::model::TabStop {
                        pos_pt: 100.0,
                        align: crate::model::TabAlign::Decimal,
                        leader: '\0',
                    }],
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let bytes = odt_from_model(&doc);
        let content = String::from_utf8(entry(&bytes, "content.xml").unwrap()).unwrap();
        assert!(
            content.contains(
                "style:parent-style-name=\"Body\"><style:paragraph-properties>\
<style:tab-stops><style:tab-stop style:position=\"72pt\" style:type=\"center\"/>\
</style:tab-stops></style:paragraph-properties>"
            ),
            "tab-stop-only override forces an inheriting automatic style: {content}"
        );
        let styles_xml = String::from_utf8(entry(&bytes, "styles.xml").unwrap()).unwrap();
        assert!(
            styles_xml.contains(
                "<style:tab-stops><style:tab-stop style:position=\"100pt\" \
style:type=\"char\" style:char=\".\"/></style:tab-stops>"
            ),
            "named-style tab stops reach styles.xml: {styles_xml}"
        );
    }

    #[test]
    fn docx_table_with_known_widths_pins_fixed_layout() {
        // With real column widths, Word must not re-autofit the grid: the
        // exporter pins `w:tblLayout w:type="fixed"`.
        let bytes = docx_from_model(&sample_doc()); // sample table has col_widths
        let doc_xml = String::from_utf8(entry(&bytes, "word/document.xml").unwrap()).unwrap();
        assert!(
            doc_xml.contains("<w:tblLayout w:type=\"fixed\"/>"),
            "known widths pin the fixed layout: {doc_xml}"
        );
        // A width-less table keeps Word's autofit (no tblLayout).
        let table = Block {
            kind: BlockKind::Table(Table {
                rows: vec![Row {
                    cells: vec![Cell {
                        blocks: vec![para("a")],
                        ..Default::default()
                    }],
                    height: None,
                    is_header: false,
                }],
                col_widths: Vec::new(),
                border: BorderStyle::default(),
            }),
            ..Default::default()
        };
        let doc = Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![table],
                    absolute: false,
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let bytes = docx_from_model(&doc);
        let doc_xml = String::from_utf8(entry(&bytes, "word/document.xml").unwrap()).unwrap();
        assert!(
            !doc_xml.contains("<w:tblLayout"),
            "width-less table keeps autofit: {doc_xml}"
        );
    }

    /// A framed slide table whose first row is (or is not) a header.
    fn slide_table(first_row_header: bool) -> Block {
        let mk = |t: &str| Cell {
            blocks: vec![para(t)],
            ..Default::default()
        };
        Block {
            frame: Some(crate::model::Rect::new(50.0, 50.0, 600.0, 200.0)),
            kind: BlockKind::Table(Table {
                rows: vec![
                    Row {
                        cells: vec![mk("head")],
                        height: None,
                        is_header: first_row_header,
                    },
                    Row {
                        cells: vec![mk("body")],
                        height: None,
                        is_header: false,
                    },
                ],
                col_widths: vec![300.0],
                border: BorderStyle::default(),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn pptx_table_first_row_follows_row_is_header() {
        // `firstRow="1" bandRow="1"` was hardcoded on every table: a headerless
        // table grew a fake styled header and banding no model data asked for.
        let with_header = pptx_from_model(&bg_slide_doc(None, vec![slide_table(true)]));
        let slide =
            String::from_utf8(entry(&with_header, "ppt/slides/slide1.xml").unwrap()).unwrap();
        assert!(
            slide.contains("<a:tblPr firstRow=\"1\"/>"),
            "header row → firstRow=1: {slide}"
        );
        assert!(!slide.contains("bandRow"), "no unsourced banding: {slide}");
        // Round trip: `is_header` survives.
        let model = crate::convert::office_import::pptx_to_model(&read_zip(&with_header));
        let slides = collect_slides(&model);
        let table = slides[0]
            .shapes
            .iter()
            .find_map(|b| match &b.kind {
                BlockKind::Table(t) => Some(t),
                _ => None,
            })
            .expect("table survives");
        assert!(table.rows[0].is_header, "first row is a header again");
        assert!(!table.rows[1].is_header);

        let headerless = pptx_from_model(&bg_slide_doc(None, vec![slide_table(false)]));
        let slide =
            String::from_utf8(entry(&headerless, "ppt/slides/slide1.xml").unwrap()).unwrap();
        assert!(
            slide.contains("<a:tblPr/>"),
            "no header row → bare tblPr: {slide}"
        );
        let model = crate::convert::office_import::pptx_to_model(&read_zip(&headerless));
        let slides = collect_slides(&model);
        let table = slides[0]
            .shapes
            .iter()
            .find_map(|b| match &b.kind {
                BlockKind::Table(t) => Some(t),
                _ => None,
            })
            .expect("table survives");
        assert!(
            !table.rows[0].is_header,
            "no fake header on a headerless table"
        );
    }

    // ───────────────────── export-wave E1 fixes ─────────────────────

    /// #1 (DOCX): a review comment is really exported — `word/comments.xml`
    /// body, `w:commentRangeStart/End` + `w:commentReference` anchor, the
    /// relationship and content-type — and comes back anchored on re-import.
    #[test]
    fn docx_comment_round_trips_with_anchor() {
        let mut doc = sample_doc();
        doc.comments = vec![crate::model::Comment {
            id: "7".to_string(),
            author: "Reviewer".to_string(),
            date: "2026-07-01T10:00:00Z".to_string(),
            text: "Needs a source".to_string(),
        }];
        if let BlockKind::Paragraph(p) = &mut doc.sections[0].pages[0].blocks[1].kind {
            p.runs.push(Inline::CommentRef {
                id: "7".to_string(),
            });
        }

        let bytes = docx_from_model(&doc);
        let body = String::from_utf8(entry(&bytes, "word/document.xml").unwrap()).unwrap();
        assert!(
            body.contains("<w:commentRangeStart w:id=\"7\"/>")
                && body.contains("<w:commentRangeEnd w:id=\"7\"/>")
                && body.contains("<w:commentReference w:id=\"7\"/>"),
            "anchor emitted with the (numeric) model id: {body}"
        );
        let comments = String::from_utf8(entry(&bytes, "word/comments.xml").unwrap()).unwrap();
        assert!(
            comments.contains("w:id=\"7\"")
                && comments.contains("w:author=\"Reviewer\"")
                && comments.contains("w:date=\"2026-07-01T10:00:00Z\"")
                && comments.contains("Needs a source"),
            "comment body written: {comments}"
        );
        let ct = String::from_utf8(entry(&bytes, "[Content_Types].xml").unwrap()).unwrap();
        assert!(ct.contains("/word/comments.xml"), "content type: {ct}");
        let rels =
            String::from_utf8(entry(&bytes, "word/_rels/document.xml.rels").unwrap()).unwrap();
        assert!(rels.contains("Target=\"comments.xml\""), "rel: {rels}");

        let back = crate::convert::office_import::office_to_model(&bytes).expect("reimport");
        assert_eq!(back.comments.len(), 1, "comment body survives");
        assert_eq!(back.comments[0].author, "Reviewer");
        assert_eq!(back.comments[0].text, "Needs a source");
        let anchored = back.sections.iter().any(|s| {
            s.pages.iter().any(|pg| {
                pg.blocks.iter().any(|b| match &b.kind {
                    BlockKind::Paragraph(p) => p
                        .runs
                        .iter()
                        .any(|r| matches!(r, Inline::CommentRef { id } if id == "7")),
                    _ => false,
                })
            })
        });
        assert!(anchored, "the anchor survives in the run flow");
    }

    /// #1 (ODT): the same comment becomes an `office:annotation` at its anchor
    /// point (id via `office:name`, author/date/text) and round-trips.
    #[test]
    fn odt_comment_round_trips_as_annotation() {
        let mut doc = sample_doc();
        doc.comments = vec![crate::model::Comment {
            id: "noteA".to_string(),
            author: "Alice".to_string(),
            date: "2026-07-01T09:00:00".to_string(),
            text: "Check this".to_string(),
        }];
        if let BlockKind::Paragraph(p) = &mut doc.sections[0].pages[0].blocks[1].kind {
            p.runs.push(Inline::CommentRef {
                id: "noteA".to_string(),
            });
        }

        let bytes = odt_from_model(&doc);
        let content = String::from_utf8(entry(&bytes, "content.xml").unwrap()).unwrap();
        assert!(
            content.contains("<office:annotation office:name=\"noteA\">")
                && content.contains("<dc:creator>Alice</dc:creator>")
                && content.contains("<dc:date>2026-07-01T09:00:00</dc:date>")
                && content.contains("<text:p>Check this</text:p>"),
            "annotation emitted at the anchor: {content}"
        );

        let back = crate::convert::office_import::office_to_model(&bytes).expect("reimport");
        assert_eq!(back.comments.len(), 1);
        assert_eq!(back.comments[0].id, "noteA", "office:name keeps the id");
        assert_eq!(back.comments[0].author, "Alice");
        assert_eq!(back.comments[0].text, "Check this");
        let anchored = back.sections.iter().any(|s| {
            s.pages.iter().any(|pg| {
                pg.blocks.iter().any(|b| match &b.kind {
                    BlockKind::Paragraph(p) => p
                        .runs
                        .iter()
                        .any(|r| matches!(r, Inline::CommentRef { id } if id == "noteA")),
                    _ => false,
                })
            })
        });
        assert!(anchored, "the anchor survives in the run flow");
    }

    /// #2: a `PlaceholderRole::Other` with a real OOXML placeholder type (or a
    /// preserved ODF class with an OOXML equivalent) keeps that role in
    /// `p:ph type=` instead of being squashed to `body`, and round-trips.
    #[test]
    fn pptx_other_placeholder_role_round_trips() {
        let mk = |text: &str, role: PlaceholderRole, y: f64| crate::model::Placeholder {
            role,
            block: Block {
                frame: Some(crate::model::Rect::new(40.0, y, 400.0, 40.0)),
                ..para(text)
            },
        };
        let slide = Slide {
            geometry: PageGeometry {
                width: 960.0,
                height: 540.0,
                ..Default::default()
            },
            shapes: vec![],
            placeholders: vec![
                mk("The Title", PlaceholderRole::Title, 20.0),
                mk(
                    "Confidential",
                    PlaceholderRole::Other("ftr".to_string()),
                    480.0,
                ),
                mk("3", PlaceholderRole::Other("sldNum".to_string()), 500.0),
                // A preserved ODF class token maps to its OOXML equivalent.
                mk(
                    "2026",
                    PlaceholderRole::Other("date-time".to_string()),
                    510.0,
                ),
                // A token with no PPTX counterpart still degrades to body.
                mk("misc", PlaceholderRole::Other("weird".to_string()), 520.0),
            ],
            notes: None,
            background: None,
            ..Default::default()
        };
        let doc = Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![Block {
                        kind: BlockKind::Slide(SlideBlock {
                            slides: vec![slide],
                        }),
                        ..Default::default()
                    }],
                    absolute: true,
                }],
                ..Default::default()
            }],
            ..Default::default()
        };

        let bytes = pptx_from_model(&doc);
        let slide_xml = String::from_utf8(entry(&bytes, "ppt/slides/slide1.xml").unwrap()).unwrap();
        for ty in ["title", "ftr", "sldNum", "dt"] {
            assert!(
                slide_xml.contains(&format!("<p:ph type=\"{ty}\"")),
                "type {ty} preserved: {slide_xml}"
            );
        }
        assert_eq!(
            slide_xml.matches("<p:ph type=\"body\"").count(),
            1,
            "only the unmappable token degrades to body: {slide_xml}"
        );

        let back = crate::convert::office_import::pptx_to_model(&read_zip(&bytes));
        let slides = collect_slides(&back);
        let roles: Vec<PlaceholderRole> = slides[0]
            .placeholders
            .iter()
            .map(|p| p.role.clone())
            .collect();
        assert!(
            roles.contains(&PlaceholderRole::Other("ftr".to_string())),
            "footer placeholder stays a footer: {roles:?}"
        );
        assert!(
            roles.contains(&PlaceholderRole::Other("sldNum".to_string())),
            "slide-number placeholder preserved: {roles:?}"
        );
    }

    /// #3: run letter spacing is emitted as `a:rPr@spc` (centipoints).
    #[test]
    fn pptx_letter_spacing_emitted_as_spc() {
        let mut doc = sample_doc();
        if let BlockKind::Paragraph(p) = &mut doc.sections[0].pages[0].blocks[1].kind {
            if let Some(Inline::Run(r)) = p.runs.first_mut() {
                r.style.letter_spacing_pt = 1.2;
            }
        }
        let bytes = pptx_from_model(&doc);
        let slide_xml = String::from_utf8(entry(&bytes, "ppt/slides/slide1.xml").unwrap()).unwrap();
        assert!(
            slide_xml.contains(" spc=\"120\""),
            "1.2pt letter spacing → spc=120: {slide_xml}"
        );
    }

    /// #4: a hidden run carries `text:display="none"` in its ODT text style
    /// (the `w:vanish` analogue — previously DOCX-only).
    #[test]
    fn odt_hidden_run_emits_text_display_none() {
        let mut doc = sample_doc();
        if let BlockKind::Paragraph(p) = &mut doc.sections[0].pages[0].blocks[1].kind {
            if let Some(Inline::Run(r)) = p.runs.first_mut() {
                r.style.hidden = true;
            }
        }
        let bytes = odt_from_model(&doc);
        let content = String::from_utf8(entry(&bytes, "content.xml").unwrap()).unwrap();
        assert!(
            content.contains(" text:display=\"none\""),
            "hidden run flagged in its span style: {content}"
        );
    }

    /// #5: a table nested inside a DrawingML cell — inexpressible in PPTX —
    /// renders as one tab-separated text line per nested row instead of
    /// flattening every nested cell into disconnected paragraphs.
    #[test]
    fn pptx_nested_table_renders_rows_as_text_lines() {
        let nested = Table {
            rows: vec![
                Row {
                    cells: vec![
                        Cell {
                            blocks: vec![para("h1")],
                            ..Default::default()
                        },
                        Cell {
                            blocks: vec![para("h2")],
                            ..Default::default()
                        },
                    ],
                    height: None,
                    is_header: true,
                },
                Row {
                    cells: vec![
                        Cell {
                            blocks: vec![para("a")],
                            ..Default::default()
                        },
                        Cell {
                            blocks: vec![para("b")],
                            ..Default::default()
                        },
                    ],
                    height: None,
                    is_header: false,
                },
            ],
            col_widths: vec![50.0, 50.0],
            border: Default::default(),
        };
        let outer = Table {
            rows: vec![Row {
                cells: vec![Cell {
                    blocks: vec![
                        para("intro"),
                        Block {
                            kind: BlockKind::Table(nested),
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                }],
                height: None,
                is_header: false,
            }],
            col_widths: vec![200.0],
            border: Default::default(),
        };
        let slide = Slide {
            geometry: PageGeometry {
                width: 960.0,
                height: 540.0,
                ..Default::default()
            },
            shapes: vec![Block {
                frame: Some(crate::model::Rect::new(10.0, 10.0, 400.0, 200.0)),
                kind: BlockKind::Table(outer),
                ..Default::default()
            }],
            placeholders: vec![],
            notes: None,
            background: None,
            ..Default::default()
        };
        let doc = Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![Block {
                        kind: BlockKind::Slide(SlideBlock {
                            slides: vec![slide],
                        }),
                        ..Default::default()
                    }],
                    absolute: true,
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let bytes = pptx_from_model(&doc);
        let slide_xml = String::from_utf8(entry(&bytes, "ppt/slides/slide1.xml").unwrap()).unwrap();
        assert!(
            slide_xml.contains("<a:t>h1\th2</a:t>") && slide_xml.contains("<a:t>a\tb</a:t>"),
            "nested rows become tab-joined lines: {slide_xml}"
        );
    }

    /// #6: heterogeneous slide sizes — the single `p:sldSz` is the **majority**
    /// size (not the first slide's) and a deviant slide's content is scaled
    /// proportionally onto it. A homogeneous deck is untouched.
    #[test]
    fn pptx_deviant_slide_scales_to_majority_size() {
        let mk_slide = |w: f64, h: f64, frame: crate::model::Rect| Slide {
            geometry: PageGeometry {
                width: w,
                height: h,
                ..Default::default()
            },
            shapes: vec![Block {
                frame: Some(frame),
                ..para("content")
            }],
            placeholders: vec![],
            notes: None,
            background: None,
            ..Default::default()
        };
        // The deviant HALF-size slide comes first: the old first-slide rule
        // would have shrunk the whole deck to 480×270.
        let slides = vec![
            mk_slide(
                480.0,
                270.0,
                crate::model::Rect::new(10.0, 10.0, 100.0, 50.0),
            ),
            mk_slide(
                960.0,
                540.0,
                crate::model::Rect::new(40.0, 40.0, 200.0, 80.0),
            ),
            mk_slide(
                960.0,
                540.0,
                crate::model::Rect::new(40.0, 40.0, 200.0, 80.0),
            ),
        ];
        let doc = Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![Block {
                        kind: BlockKind::Slide(SlideBlock { slides }),
                        ..Default::default()
                    }],
                    absolute: true,
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let bytes = pptx_from_model(&doc);
        let pres = String::from_utf8(entry(&bytes, "ppt/presentation.xml").unwrap()).unwrap();
        assert!(
            pres.contains(&format!(
                "<p:sldSz cx=\"{}\" cy=\"{}\"/>",
                emu(960.0),
                emu(540.0)
            )),
            "sldSz is the majority size: {pres}"
        );
        let s1 = String::from_utf8(entry(&bytes, "ppt/slides/slide1.xml").unwrap()).unwrap();
        assert!(
            s1.contains(&format!(
                "<a:off x=\"{}\" y=\"{}\"/><a:ext cx=\"{}\" cy=\"{}\"/>",
                emu(20.0),
                emu(20.0),
                emu(200.0),
                emu(100.0)
            )),
            "deviant slide content scaled ×2 onto the majority box: {s1}"
        );
        // ODP mirrors the same normalization in its single page layout.
        let odp = odp_from_model(&doc);
        let styles = String::from_utf8(entry(&odp, "styles.xml").unwrap()).unwrap();
        assert!(
            styles.contains("fo:page-width=\"960pt\"")
                && styles.contains("fo:page-height=\"540pt\""),
            "ODP page layout uses the majority size: {styles}"
        );
    }

    /// #7: a table hoisted out of a text-shape body is emitted right after its
    /// source shape, before later model shapes — sp-tree order = paint order.
    #[test]
    fn pptx_hoisted_table_keeps_model_paint_order() {
        let boxed_table = Block {
            frame: Some(crate::model::Rect::new(10.0, 10.0, 400.0, 200.0)),
            kind: BlockKind::TextBox(TextBox {
                blocks: vec![
                    para("intro"),
                    Block {
                        kind: BlockKind::Table(Table {
                            rows: vec![Row {
                                cells: vec![Cell {
                                    blocks: vec![para("cell")],
                                    ..Default::default()
                                }],
                                height: None,
                                is_header: false,
                            }],
                            col_widths: vec![100.0],
                            border: Default::default(),
                        }),
                        ..Default::default()
                    },
                ],
            }),
            ..Default::default()
        };
        let later = Block {
            frame: Some(crate::model::Rect::new(10.0, 300.0, 400.0, 40.0)),
            ..para("AFTER")
        };
        let slide = Slide {
            geometry: PageGeometry {
                width: 960.0,
                height: 540.0,
                ..Default::default()
            },
            shapes: vec![boxed_table, later],
            placeholders: vec![],
            notes: None,
            background: None,
            ..Default::default()
        };
        let doc = Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![Block {
                        kind: BlockKind::Slide(SlideBlock {
                            slides: vec![slide],
                        }),
                        ..Default::default()
                    }],
                    absolute: true,
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let bytes = pptx_from_model(&doc);
        let slide_xml = String::from_utf8(entry(&bytes, "ppt/slides/slide1.xml").unwrap()).unwrap();
        let tbl_at = slide_xml.find("<a:tbl>").expect("hoisted table emitted");
        let after_at = slide_xml.find("AFTER").expect("later shape emitted");
        assert!(
            tbl_at < after_at,
            "hoisted table precedes the later model shape (paint order): {slide_xml}"
        );
    }

    /// #8: outline entries become Word-canonical `_Toc…` destination bookmarks
    /// wrapped around their matching headings (and only those).
    #[test]
    fn docx_outline_becomes_toc_bookmarks_on_headings() {
        let heading = |level: u8, text: &str| Block {
            kind: BlockKind::Heading(Heading {
                level,
                para: Paragraph {
                    runs: vec![run(text)],
                    ..Default::default()
                },
            }),
            ..Default::default()
        };
        let doc = Document {
            sections: vec![Section {
                pages: vec![
                    Page {
                        blocks: vec![heading(1, "One"), para("body"), heading(2, "Two")],
                        absolute: false,
                    },
                    Page {
                        blocks: vec![heading(1, "Three")],
                        absolute: false,
                    },
                ],
                ..Default::default()
            }],
            outline: vec![
                crate::model::OutlineNode {
                    title: "One".to_string(),
                    page: 0,
                    children: vec![crate::model::OutlineNode {
                        title: "Two".to_string(),
                        page: 0,
                        children: vec![],
                    }],
                },
                crate::model::OutlineNode {
                    title: "Three".to_string(),
                    page: 1,
                    children: vec![],
                },
            ],
            ..Default::default()
        };
        let bytes = docx_from_model(&doc);
        let xml = String::from_utf8(entry(&bytes, "word/document.xml").unwrap()).unwrap();
        assert_eq!(
            xml.matches("w:name=\"_Toc").count(),
            3,
            "one _Toc bookmark per outline entry: {xml}"
        );
        // Each bookmarkStart sits immediately before its heading's paragraph.
        for title in ["One", "Two", "Three"] {
            let at = xml.find(&format!(">{title}</w:t>")).expect("heading text");
            let before = &xml[..at];
            let bm = before
                .rfind("w:name=\"_Toc")
                .expect("bookmark before heading");
            assert!(
                !before[bm..].contains("</w:p>"),
                "no other paragraph between the _Toc bookmark and heading {title}"
            );
        }
    }

    /// #9: `cp:lastModifiedBy`/`cp:revision` reach `docProps/core.xml` and come
    /// back; ODF `meta.xml` preserves the source generator and re-emits the
    /// description / dates / editing-cycles the importer reads.
    #[test]
    fn meta_round_trips_core_and_odf_fields() {
        let mut doc = sample_doc();
        doc.meta.title = Some("Spec".to_string());
        doc.meta.last_modified_by = "Editor Name".to_string();
        doc.meta.revision = "12".to_string();
        doc.meta.generator = "Writer/1.0".to_string();
        doc.meta.editing_cycles = "3".to_string();
        doc.meta.description = "An abstract".to_string();
        doc.meta.created = "2019-12-31T23:59:59".to_string();
        doc.meta.modified = "2020-01-02T03:04:05".to_string();

        let bytes = docx_from_model(&doc);
        let core = String::from_utf8(entry(&bytes, "docProps/core.xml").unwrap()).unwrap();
        assert!(
            core.contains("<cp:lastModifiedBy>Editor Name</cp:lastModifiedBy>")
                && core.contains("<cp:revision>12</cp:revision>"),
            "core props carried: {core}"
        );
        let back = crate::convert::office_import::office_to_model(&bytes).expect("docx reimport");
        assert_eq!(back.meta.last_modified_by, "Editor Name");
        assert_eq!(back.meta.revision, "12");

        let odt = odt_from_model(&doc);
        let meta = String::from_utf8(entry(&odt, "meta.xml").unwrap()).unwrap();
        assert!(
            meta.contains("<meta:generator>Writer/1.0</meta:generator>"),
            "source generator preserved (not stamped GigaPDF): {meta}"
        );
        assert!(
            meta.contains("<meta:editing-cycles>3</meta:editing-cycles>")
                && meta.contains("<dc:description>An abstract</dc:description>")
                && meta.contains("<meta:creation-date>2019-12-31T23:59:59</meta:creation-date>")
                && meta.contains("<dc:date>2020-01-02T03:04:05</dc:date>"),
            "ODF meta fields emitted: {meta}"
        );
        let back = crate::convert::office_import::office_to_model(&odt).expect("odt reimport");
        assert_eq!(back.meta.generator, "Writer/1.0");
        assert_eq!(back.meta.editing_cycles, "3");
        assert_eq!(back.meta.description, "An abstract");
        // A document with no recorded generator is stamped GigaPDF.
        let mut anon = sample_doc();
        anon.meta.generator = String::new();
        let meta = String::from_utf8(entry(&odt_from_model(&anon), "meta.xml").unwrap()).unwrap();
        assert!(meta.contains("<meta:generator>GigaPDF</meta:generator>"));
    }

    /// #10: an external body hyperlink is the canonical `<w:hyperlink r:id>` +
    /// `TargetMode="External"` relationship, and survives a round-trip.
    #[test]
    fn docx_external_hyperlink_uses_canonical_w_hyperlink() {
        let mut doc = sample_doc();
        if let BlockKind::Paragraph(p) = &mut doc.sections[0].pages[0].blocks[1].kind {
            p.runs.push(Inline::Link {
                href: LinkTarget::Url("https://example.com/a?b=1&c=2".to_string()),
                children: vec![run("click")],
            });
        }
        let bytes = docx_from_model(&doc);
        let xml = String::from_utf8(entry(&bytes, "word/document.xml").unwrap()).unwrap();
        assert!(
            xml.contains("<w:hyperlink r:id=\"rIdHl1\">"),
            "canonical hyperlink element: {xml}"
        );
        assert!(
            !xml.contains("HYPERLINK \"https://example.com"),
            "no field encoding for the body link: {xml}"
        );
        let rels =
            String::from_utf8(entry(&bytes, "word/_rels/document.xml.rels").unwrap()).unwrap();
        assert!(
            rels.contains(
                "Id=\"rIdHl1\" \
Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink\" \
Target=\"https://example.com/a?b=1&amp;c=2\" TargetMode=\"External\""
            ),
            "external relationship written: {rels}"
        );
        let back = crate::convert::office_import::office_to_model(&bytes).expect("reimport");
        let mut found = false;
        for s in &back.sections {
            for pg in &s.pages {
                for b in &pg.blocks {
                    if let BlockKind::Paragraph(p) = &b.kind {
                        for r in &p.runs {
                            if let Inline::Link { href, .. } = r {
                                if matches!(href, LinkTarget::Url(u) if u == "https://example.com/a?b=1&c=2")
                                {
                                    found = true;
                                }
                            }
                        }
                    }
                }
            }
        }
        assert!(found, "the URL survives the round-trip");
    }

    /// #11a: no empty-paragraph growth — the section break rides the trailing
    /// empty paragraph, and an anchored image rides its preceding paragraph
    /// (the importer's floating-drawing lift then reproduces the model 1:1).
    #[test]
    fn docx_section_break_rides_trailing_empty_paragraph() {
        let doc = Document {
            sections: vec![
                Section {
                    pages: vec![Page {
                        blocks: vec![
                            para("first section"),
                            Block {
                                kind: BlockKind::Paragraph(Paragraph::default()),
                                ..Default::default()
                            },
                        ],
                        absolute: false,
                    }],
                    ..Default::default()
                },
                Section {
                    pages: vec![Page {
                        blocks: vec![para("second section")],
                        absolute: false,
                    }],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let xml =
            String::from_utf8(entry(&docx_from_model(&doc), "word/document.xml").unwrap()).unwrap();
        assert_eq!(
            xml.matches("<w:p>").count(),
            3,
            "content ×2 + the sectPr host — no extra break paragraph: {xml}"
        );
        assert!(
            xml.contains("<w:p><w:pPr><w:sectPr>"),
            "the trailing empty paragraph hosts the sectPr: {xml}"
        );
    }

    /// #11a (images) + #11b (symmetry): the anchored drawing is absorbed into
    /// its preceding paragraph's `<w:p>` and its frame survives a full
    /// round-trip unchanged (no ½-flip drift of the Y offset).
    #[test]
    fn docx_anchored_image_rides_host_paragraph_and_frame_round_trips() {
        let mut doc = image_doc("png");
        doc.sections[0].pages[0].blocks[0].frame =
            Some(crate::model::Rect::new(360.0, 625.9, 72.0, 72.0));
        doc.sections[0].pages[0]
            .blocks
            .insert(0, para("host paragraph"));

        let bytes = docx_from_model(&doc);
        let xml = String::from_utf8(entry(&bytes, "word/document.xml").unwrap()).unwrap();
        assert_eq!(
            xml.matches("<w:p>").count(),
            1,
            "one paragraph hosts both the text and the anchor: {xml}"
        );
        assert!(
            xml.contains("host paragraph") && xml.contains("<wp:anchor "),
            "text + anchored drawing in the same paragraph: {xml}"
        );

        let back = crate::convert::office_import::office_to_model(&bytes).expect("reimport");
        let frame = back
            .sections
            .iter()
            .flat_map(|s| &s.pages)
            .flat_map(|p| &p.blocks)
            .find_map(|b| match &b.kind {
                BlockKind::Image(_) => b.frame,
                _ => None,
            })
            .expect("image survives");
        assert!(
            (frame.y - 625.9).abs() < 0.5,
            "frame Y is stable across the round-trip (was drifting to ~144): {frame:?}"
        );
        assert!((frame.x - 360.0).abs() < 0.5, "frame X stable: {frame:?}");
    }

    /// #11c: a modeled style reusing a reserved id (`Heading1`) merges into the
    /// built-in slot — its colour survives instead of being dropped.
    #[test]
    fn docx_reserved_heading_style_keeps_its_color() {
        let mut doc = sample_doc();
        doc.styles.named.insert(
            StyleId("Heading1".to_string()),
            NamedStyle {
                char_: CharStyle {
                    color: Some([1.0, 0.0, 0.0]),
                    bold: true,
                    size_pt: 16.0,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let bytes = docx_from_model(&doc);
        let styles = String::from_utf8(entry(&bytes, "word/styles.xml").unwrap()).unwrap();
        assert_eq!(
            styles.matches("w:styleId=\"Heading1\"").count(),
            1,
            "exactly one Heading1 definition: {styles}"
        );
        let h1 = styles
            .split("w:styleId=\"Heading1\"")
            .nth(1)
            .unwrap()
            .split("</w:style>")
            .next()
            .unwrap();
        assert!(
            h1.contains("<w:color w:val=\"FF0000\"/>"),
            "the modeled colour reaches the reserved style: {h1}"
        );
        assert!(
            h1.contains("<w:outlineLvl w:val=\"0\"/>"),
            "the heading scaffolding is kept: {h1}"
        );
        let back = crate::convert::office_import::office_to_model(&bytes).expect("reimport");
        let h1 = back
            .styles
            .named
            .get(&StyleId("Heading1".to_string()))
            .expect("Heading1 present");
        assert_eq!(h1.char_.color, Some([1.0, 0.0, 0.0]), "colour round-trips");
    }

    /// #11d: header/footer automatic styles carry the `HF` prefix, so the
    /// importer's merged style map can no longer resolve a header span against
    /// an unrelated body style of the same name — the header colour survives.
    #[test]
    fn odt_header_run_color_survives_via_prefixed_styles() {
        let mut doc = sample_doc();
        doc.sections[0].header = Some(vec![Block {
            kind: BlockKind::Paragraph(Paragraph {
                runs: vec![Inline::Run(InlineRun {
                    text: "RUNNING HEADER".to_string(),
                    style: CharStyle {
                        color: Some([1.0, 0.0, 0.0]),
                        ..Default::default()
                    },
                    ..Default::default()
                })],
                ..Default::default()
            }),
            ..Default::default()
        }]);
        let bytes = odt_from_model(&doc);
        let styles = String::from_utf8(entry(&bytes, "styles.xml").unwrap()).unwrap();
        let content = String::from_utf8(entry(&bytes, "content.xml").unwrap()).unwrap();
        assert!(
            styles.contains("style:name=\"HFT0\"") && styles.contains("fo:color=\"#FF0000\""),
            "prefixed header span style with its colour in styles.xml: {styles}"
        );
        assert!(
            !content.contains("\"HFT0\""),
            "the prefixed name cannot collide with content.xml: shared names were \
resolved against the body's styles"
        );
        let back = crate::convert::office_import::office_to_model(&bytes).expect("reimport");
        let header = back.sections[0].header.as_ref().expect("header survives");
        let color = header.iter().find_map(|b| match &b.kind {
            BlockKind::Paragraph(p) => p.runs.iter().find_map(|r| match r {
                Inline::Run(ir) if ir.text.contains("RUNNING") => ir.style.color,
                _ => None,
            }),
            _ => None,
        });
        assert_eq!(
            color,
            Some([1.0, 0.0, 0.0]),
            "header run colour round-trips"
        );
    }

    // ── M2: deferred emissions + model extensions ────────────────────────────

    /// A one-paragraph document whose paragraph carries the given style.
    fn doc_with_para_style(style: ParagraphStyle, text: &str) -> Document {
        doc_with(Block {
            kind: BlockKind::Paragraph(Paragraph {
                style,
                runs: vec![run(text)],
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    #[test]
    fn m2_docx_direction_page_break_round_trip() {
        let doc = doc_with_para_style(
            ParagraphStyle {
                direction: Some(crate::model::Direction::Rtl),
                page_break_before: true,
                ..Default::default()
            },
            "שלום",
        );
        let bytes = docx_from_model(&doc);
        let xml = String::from_utf8(entry(&bytes, "word/document.xml").unwrap()).unwrap();
        assert!(xml.contains("<w:bidi/>"), "w:bidi emitted: {xml}");
        assert!(
            xml.contains("<w:pageBreakBefore/>"),
            "w:pageBreakBefore emitted"
        );
        let back =
            crate::convert::office_import::docx_to_model(&crate::convert::zip::read_zip(&bytes));
        let para = back
            .sections
            .iter()
            .flat_map(|s| &s.pages)
            .flat_map(|p| &p.blocks)
            .find_map(|b| match &b.kind {
                BlockKind::Paragraph(p) if p.runs.iter().any(|r| matches!(r, Inline::Run(ir) if ir.text.contains("שלום"))) => {
                    Some(p)
                }
                _ => None,
            })
            .expect("paragraph survives");
        assert_eq!(
            para.style.direction,
            Some(crate::model::Direction::Rtl),
            "direction round-trips"
        );
        assert!(para.style.page_break_before, "page break intent round-trips");
    }

    #[test]
    fn m2_docx_char_style_extras_round_trip() {
        let styled = Block {
            kind: BlockKind::Paragraph(Paragraph {
                runs: vec![Inline::Run(InlineRun {
                    text: "Extras".to_string(),
                    style: CharStyle {
                        caps: true,
                        small_caps: false,
                        dstrike: true,
                        strike: true, // degraded-fallback pairing
                        baseline_shift_pt: Some(3.0),
                        char_scale_pct: Some(150.0),
                        kern_min_pt: Some(14.0),
                        size_pt: 12.0,
                        ..Default::default()
                    },
                    source_index: None,
                })],
                ..Default::default()
            }),
            ..Default::default()
        };
        let bytes = docx_from_model(&doc_with(styled));
        let xml = String::from_utf8(entry(&bytes, "word/document.xml").unwrap()).unwrap();
        assert!(xml.contains("<w:caps/>"), "caps: {xml}");
        assert!(xml.contains("<w:dstrike/>"), "dstrike");
        assert!(
            !xml.contains("<w:strike/>"),
            "single strike suppressed under dstrike"
        );
        assert!(xml.contains("<w:position w:val=\"6\"/>"), "position 3pt→6");
        assert!(xml.contains("<w:w w:val=\"150\"/>"), "char scale 150%");
        assert!(xml.contains("<w:kern w:val=\"28\"/>"), "kern 14pt→28");
        // Round-trip through the DOCX importer.
        let back =
            crate::convert::office_import::docx_to_model(&crate::convert::zip::read_zip(&bytes));
        let style = back
            .sections
            .iter()
            .flat_map(|s| &s.pages)
            .flat_map(|p| &p.blocks)
            .find_map(|b| match &b.kind {
                BlockKind::Paragraph(p) => p.runs.iter().find_map(|r| match r {
                    Inline::Run(ir) if ir.text.contains("Extras") => Some(ir.style.clone()),
                    _ => None,
                }),
                _ => None,
            })
            .expect("styled run survives");
        assert!(style.caps, "caps round-trips");
        assert!(style.dstrike, "dstrike round-trips");
        assert_eq!(style.baseline_shift_pt, Some(3.0), "position round-trips");
        assert_eq!(style.char_scale_pct, Some(150.0), "scale round-trips");
        assert_eq!(style.kern_min_pt, Some(14.0), "kern round-trips");
    }

    #[test]
    fn m2_docx_anchor_link_round_trip() {
        let link = Block {
            kind: BlockKind::Paragraph(Paragraph {
                runs: vec![Inline::Link {
                    href: LinkTarget::Anchor("target1".to_string()),
                    children: vec![run("jump")],
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        let bytes = docx_from_model(&doc_with(link));
        let xml = String::from_utf8(entry(&bytes, "word/document.xml").unwrap()).unwrap();
        assert!(
            xml.contains("<w:hyperlink w:anchor=\"target1\">"),
            "typed anchor hyperlink emitted: {xml}"
        );
        let back =
            crate::convert::office_import::docx_to_model(&crate::convert::zip::read_zip(&bytes));
        let survives = back
            .sections
            .iter()
            .flat_map(|s| &s.pages)
            .flat_map(|p| &p.blocks)
            .any(|b| match &b.kind {
                BlockKind::Paragraph(p) => p.runs.iter().any(|r| {
                    matches!(r, Inline::Link { href: LinkTarget::Anchor(a), .. } if a == "target1")
                }),
                _ => None::<()>.is_some(),
            });
        assert!(survives, "anchor target survives the round trip");
    }

    #[test]
    fn m2_docx_footnotes_part_and_round_trip() {
        let mut doc = doc_with(Block {
            kind: BlockKind::Paragraph(Paragraph {
                runs: vec![
                    run("body"),
                    Inline::FootnoteRef {
                        id: "7".to_string(),
                    },
                ],
                ..Default::default()
            }),
            ..Default::default()
        });
        doc.footnotes.push(crate::model::Footnote {
            id: "7".to_string(),
            blocks: vec![para("the note body")],
        });
        let bytes = docx_from_model(&doc);
        let xml = String::from_utf8(entry(&bytes, "word/document.xml").unwrap()).unwrap();
        assert!(
            xml.contains("<w:footnoteReference w:id=\"7\"/>"),
            "anchor keeps its numeric id: {xml}"
        );
        let notes = String::from_utf8(entry(&bytes, "word/footnotes.xml").unwrap()).unwrap();
        assert!(notes.contains("w:type=\"separator\""), "stock separators");
        assert!(
            notes.contains("<w:footnote w:id=\"7\">"),
            "note body part: {notes}"
        );
        assert!(notes.contains("the note body"));
        let types = String::from_utf8(entry(&bytes, "[Content_Types].xml").unwrap()).unwrap();
        assert!(types.contains("/word/footnotes.xml"), "content type");
        let rels =
            String::from_utf8(entry(&bytes, "word/_rels/document.xml.rels").unwrap()).unwrap();
        assert!(rels.contains("Target=\"footnotes.xml\""), "relationship");
        // Round-trip: the importer folds footnotes back into the model.
        let back =
            crate::convert::office_import::docx_to_model(&crate::convert::zip::read_zip(&bytes));
        assert_eq!(back.footnotes.len(), 1, "one footnote survives");
        let mut note_text = String::new();
        for b in &back.footnotes[0].blocks {
            if let BlockKind::Paragraph(p) = &b.kind {
                inlines_plain_text(&p.runs, &mut note_text);
            }
        }
        assert!(
            note_text.contains("the note body"),
            "note body survives: {note_text:?}"
        );
    }

    #[test]
    fn m2_docx_soft_breaks_reflow_as_spaces() {
        let styled = Block {
            kind: BlockKind::Paragraph(Paragraph {
                style: ParagraphStyle {
                    // break 0 soft (word wrap), break 1 hard (authored).
                    soft_breaks: vec![true, false],
                    ..Default::default()
                },
                runs: vec![
                    run("one"),
                    Inline::LineBreak,
                    run("two"),
                    Inline::LineBreak,
                    run("three"),
                ],
                ..Default::default()
            }),
            ..Default::default()
        };
        let bytes = docx_from_model(&doc_with(styled));
        let xml = String::from_utf8(entry(&bytes, "word/document.xml").unwrap()).unwrap();
        assert_eq!(
            xml.matches("<w:br/>").count(),
            1,
            "only the hard break stays a w:br: {xml}"
        );
        assert!(
            xml.contains("one ") || xml.contains(">one</w:t>"),
            "soft break became a space"
        );
        // Misaligned classification ⇒ statu quo (both breaks stay hard).
        let stale = Block {
            kind: BlockKind::Paragraph(Paragraph {
                style: ParagraphStyle {
                    soft_breaks: vec![true], // 1 entry for 2 breaks
                    ..Default::default()
                },
                runs: vec![
                    run("a"),
                    Inline::LineBreak,
                    run("b"),
                    Inline::LineBreak,
                    run("c"),
                ],
                ..Default::default()
            }),
            ..Default::default()
        };
        let bytes = docx_from_model(&doc_with(stale));
        let xml = String::from_utf8(entry(&bytes, "word/document.xml").unwrap()).unwrap();
        assert_eq!(xml.matches("<w:br/>").count(), 2, "stale guard holds");
    }

    #[test]
    fn m2_docx_numbering_levels_and_prefix() {
        use crate::model::ListLevelStyle;
        let list = Block {
            kind: BlockKind::List(List {
                ordered: true,
                marker: ListMarker::Decimal,
                items: vec![
                    ListItem {
                        blocks: vec![para("first")],
                        level: 0,
                        label: None,
                    },
                    ListItem {
                        blocks: vec![para("second")],
                        level: 1,
                        label: None,
                    },
                ],
                start: 1,
                marker_prefix: Some("(".to_string()),
                levels: vec![
                    ListLevelStyle {
                        marker: ListMarker::Decimal,
                        suffix: Some('.'),
                        start: 1,
                    },
                    ListLevelStyle {
                        marker: ListMarker::LowerAlpha,
                        suffix: Some(')'),
                        start: 1,
                    },
                ],
                ..List::default()
            }),
            ..Default::default()
        };
        let bytes = docx_from_model(&doc_with(list));
        let numbering = String::from_utf8(entry(&bytes, "word/numbering.xml").unwrap()).unwrap();
        assert!(
            numbering.contains("<w:lvlText w:val=\"(%1.\"/>"),
            "level 0 keeps decimal + prefix: {numbering}"
        );
        assert!(
            numbering.contains("<w:numFmt w:val=\"lowerLetter\"/>")
                && numbering.contains("<w:lvlText w:val=\"(%2)\"/>"),
            "level 1 is lowerLetter with its own suffix"
        );
    }

    #[test]
    fn m2_docx_title_page_and_even_headers() {
        let mut doc = doc_with(para("body"));
        {
            let section = &mut doc.sections[0];
            section.header = Some(vec![para("default header")]);
            section.header_first = Some(vec![]); // blank first-page header
            section.footer_even = Some(vec![para("even footer")]);
        }
        let bytes = docx_from_model(&doc);
        let xml = String::from_utf8(entry(&bytes, "word/document.xml").unwrap()).unwrap();
        assert!(xml.contains("<w:titlePg/>"), "titlePg emitted: {xml}");
        assert!(
            xml.contains("w:type=\"first\" r:id=\"rIdHdrF1\""),
            "first header reference"
        );
        assert!(
            xml.contains("w:type=\"even\" r:id=\"rIdFtrE1\""),
            "even footer reference"
        );
        let first = String::from_utf8(entry(&bytes, "word/headerF1.xml").unwrap()).unwrap();
        assert!(first.contains("<w:p/>"), "blank first header part: {first}");
        let even = String::from_utf8(entry(&bytes, "word/footerE1.xml").unwrap()).unwrap();
        assert!(even.contains("even footer"));
        let settings = String::from_utf8(entry(&bytes, "word/settings.xml").unwrap()).unwrap();
        assert!(settings.contains("<w:evenAndOddHeaders/>"), "settings part");
        // Round-trip through the importer's variant capture.
        let back =
            crate::convert::office_import::docx_to_model(&crate::convert::zip::read_zip(&bytes));
        let section = &back.sections[0];
        assert_eq!(
            section.header_first,
            Some(vec![]),
            "blank first-page header survives as Some(vec![])"
        );
        let mut even_text = String::new();
        for b in section.footer_even.as_ref().expect("even footer survives") {
            if let BlockKind::Paragraph(p) = &b.kind {
                inlines_plain_text(&p.runs, &mut even_text);
            }
        }
        assert!(even_text.contains("even footer"));
    }

    /// ODT master pages now carry the first-page (`style:header-first`) and
    /// even-page (`style:header-left`/`-left`) header/footer variants (item c);
    /// the importer reads them back into the model's first/even slots. (A *blank*
    /// variant `Some(vec![])` lowers to an empty ODF element that re-imports as
    /// `None` — ODF cannot distinguish an empty running element from an absent
    /// one; non-empty variants round-trip fully.)
    #[test]
    fn m2_odt_first_and_even_headers_round_trip() {
        let mut doc = doc_with(para("body"));
        {
            let section = &mut doc.sections[0];
            section.header = Some(vec![para("default header")]);
            section.header_first = Some(vec![para("first header")]);
            section.footer_even = Some(vec![para("even footer")]);
        }
        let bytes = odt_from_model(&doc);
        let styles = String::from_utf8(entry(&bytes, "styles.xml").unwrap()).unwrap();
        assert!(
            styles.contains("<style:header-first>") && styles.contains("first header"),
            "first-page header emitted: {styles}"
        );
        // The even footer has no default footer, so an empty `<style:footer/>`
        // precedes the `<style:footer-left>` override.
        assert!(
            styles.contains("<style:footer-left>") && styles.contains("even footer"),
            "even footer emitted: {styles}"
        );

        // Round-trip through the ODT importer.
        let back =
            crate::convert::office_import::odt_to_model(&crate::convert::zip::read_zip(&bytes));
        let section = &back.sections[0];
        let text = |bs: &Option<Vec<Block>>| {
            let mut s = String::new();
            for b in bs.as_deref().unwrap_or(&[]) {
                if let BlockKind::Paragraph(p) = &b.kind {
                    inlines_plain_text(&p.runs, &mut s);
                }
            }
            s
        };
        assert!(
            text(&section.header).contains("default header"),
            "default header survives"
        );
        assert!(
            text(&section.header_first).contains("first header"),
            "first-page header survives"
        );
        assert!(
            text(&section.footer_even).contains("even footer"),
            "even footer survives"
        );
    }

    #[test]
    fn m2_docx_cell_borders_round_trip() {
        let table = Block {
            kind: BlockKind::Table(Table {
                rows: vec![Row {
                    cells: vec![Cell {
                        blocks: vec![para("x")],
                        borders: [
                            Some(ParaBorder {
                                width_pt: 1.5,
                                style: "dashed".to_string(),
                                color: [1.0, 0.0, 0.0],
                            }),
                            None,
                            None,
                            Some(ParaBorder {
                                width_pt: 0.75,
                                style: "solid".to_string(),
                                color: [0.0, 0.0, 1.0],
                            }),
                        ],
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        let bytes = docx_from_model(&doc_with(table));
        let xml = String::from_utf8(entry(&bytes, "word/document.xml").unwrap()).unwrap();
        assert!(
            xml.contains("<w:tcBorders><w:top w:val=\"dashed\" w:sz=\"12\""),
            "top side dashed 1.5pt: {xml}"
        );
        assert!(
            xml.contains("<w:right w:val=\"nil\"/>") && xml.contains("<w:bottom w:val=\"nil\"/>"),
            "unset sides are explicit nil"
        );
        let back =
            crate::convert::office_import::docx_to_model(&crate::convert::zip::read_zip(&bytes));
        let cell = back
            .sections
            .iter()
            .flat_map(|s| &s.pages)
            .flat_map(|p| &p.blocks)
            .find_map(|b| match &b.kind {
                BlockKind::Table(t) => t.rows.first().and_then(|r| r.cells.first()).cloned(),
                _ => None,
            })
            .expect("table cell survives");
        let top = cell.borders[0].as_ref().expect("top border survives");
        assert_eq!(top.style, "dashed");
        assert_eq!(top.color, [1.0, 0.0, 0.0]);
        assert!(cell.borders[1].is_none(), "nil right stays None");
        let left = cell.borders[3].as_ref().expect("left border survives");
        assert_eq!(left.color, [0.0, 0.0, 1.0]);
    }

    #[test]
    fn m2_docx_inline_image_size_and_crop() {
        let mut doc = doc_with(Block {
            kind: BlockKind::Image(ImageRef {
                resource: 1,
                size_pt: Some((144.0, 72.0)),
                crop: Some([0.1, 0.0, 0.25, 0.0]),
                ..Default::default()
            }),
            ..Default::default()
        });
        doc.resources.images.insert(
            1,
            crate::model::ImageResource {
                bytes: crate::raster::png::encode_png(1, 1, &[255, 0, 0, 255]),
                format: "png".to_string(),
            },
        );
        let bytes = docx_from_model(&doc);
        let xml = String::from_utf8(entry(&bytes, "word/document.xml").unwrap()).unwrap();
        assert!(
            xml.contains(&format!("<wp:extent cx=\"{}\" cy=\"{}\"/>", emu(144.0), emu(72.0))),
            "authored size drives wp:extent: {xml}"
        );
        assert!(
            xml.contains("<a:srcRect l=\"10000\" t=\"0\" r=\"25000\" b=\"0\"/>"),
            "crop emits a:srcRect"
        );
        // Round-trip via the DOCX importer (an inline image comes back as an
        // `Inline::Image` inside its host paragraph).
        let back =
            crate::convert::office_import::docx_to_model(&crate::convert::zip::read_zip(&bytes));
        let img = back
            .sections
            .iter()
            .flat_map(|s| &s.pages)
            .flat_map(|p| &p.blocks)
            .find_map(|b| match &b.kind {
                BlockKind::Image(i) => Some(i.clone()),
                BlockKind::Paragraph(p) => p.runs.iter().find_map(|r| match r {
                    Inline::Image(i) => Some(i.clone()),
                    _ => None,
                }),
                _ => None,
            })
            .expect("image survives");
        let (w, h) = img.size_pt.expect("size survives");
        assert!((w - 144.0).abs() < 0.1 && (h - 72.0).abs() < 0.1);
        assert_eq!(img.crop, Some([0.1, 0.0, 0.25, 0.0]), "crop survives");
    }

    #[test]
    fn m2_xlsx_freeze_rotation_diagonal_round_trip() {
        use crate::model::DiagonalBorder;
        let sheet = Sheet {
            name: "S1".to_string(),
            rows: vec![SheetRow {
                cells: vec![SheetCell {
                    value: CellValue::Text("t".to_string()),
                    rotation_deg: Some(-45),
                    diagonal: Some(DiagonalBorder {
                        up: true,
                        down: false,
                        border: BorderStyle {
                            width: 1.0,
                            color: [1.0, 0.0, 0.0],
                        },
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            freeze: Some((2, 1)),
            ..Default::default()
        };
        let doc = doc_with(Block {
            kind: BlockKind::Sheet(SheetBlock {
                sheets: vec![sheet],
            }),
            ..Default::default()
        });
        let bytes = xlsx_from_model(&doc);
        let ws = String::from_utf8(entry(&bytes, "xl/worksheets/sheet1.xml").unwrap()).unwrap();
        assert!(
            ws.contains("<pane xSplit=\"1\" ySplit=\"2\" topLeftCell=\"B3\" \
activePane=\"bottomRight\" state=\"frozen\"/>"),
            "frozen pane emitted: {ws}"
        );
        let styles = String::from_utf8(entry(&bytes, "xl/styles.xml").unwrap()).unwrap();
        assert!(
            styles.contains("textRotation=\"135\""),
            "-45° → 135 (90 + 45): {styles}"
        );
        assert!(
            styles.contains("diagonalUp=\"1\"") && styles.contains("<diagonal style="),
            "diagonal border emitted"
        );
        // Round-trip through the XLSX importer.
        let model =
            crate::convert::office_import::xlsx_to_model(&crate::convert::zip::read_zip(&bytes));
        let sheet = model
            .sections
            .iter()
            .flat_map(|s| &s.pages)
            .flat_map(|p| &p.blocks)
            .find_map(|b| match &b.kind {
                BlockKind::Sheet(sb) => sb.sheets.first().cloned(),
                _ => None,
            })
            .expect("sheet survives");
        assert_eq!(sheet.freeze, Some((2, 1)), "freeze round-trips");
        let cell = &sheet.rows[0].cells[0];
        assert_eq!(cell.rotation_deg, Some(-45), "rotation round-trips");
        let diag = cell.diagonal.expect("diagonal round-trips");
        assert!(diag.up && !diag.down);
    }

    /// ODS now emits frozen panes (settings.xml) and cell text rotation
    /// (`style:rotation-angle`), and the importer reads both back (item b).
    #[test]
    fn ods_freeze_rotation_round_trip() {
        let sheet = Sheet {
            name: "S1".to_string(),
            rows: vec![SheetRow {
                cells: vec![SheetCell {
                    value: CellValue::Text("t".to_string()),
                    rotation_deg: Some(-45),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            freeze: Some((2, 1)),
            ..Default::default()
        };
        let doc = doc_with(Block {
            kind: BlockKind::Sheet(SheetBlock {
                sheets: vec![sheet],
            }),
            ..Default::default()
        });
        let bytes = ods_from_model(&doc);

        // settings.xml carries the frozen split for the sheet.
        let settings = String::from_utf8(entry(&bytes, "settings.xml").unwrap()).unwrap();
        assert!(
            settings.contains("config:name=\"S1\""),
            "sheet freeze entry: {settings}"
        );
        assert!(
            settings.contains("HorizontalSplitPosition\" config:type=\"int\">1<"),
            "cols split = 1: {settings}"
        );
        assert!(
            settings.contains("VerticalSplitPosition\" config:type=\"int\">2<"),
            "rows split = 2: {settings}"
        );
        // -45° model rotation → ODF CCW 315.
        let content = String::from_utf8(entry(&bytes, "content.xml").unwrap()).unwrap();
        assert!(
            content.contains("style:rotation-angle=\"315\""),
            "-45° → 315: {content}"
        );
        // The manifest registers the new settings part.
        let manifest =
            String::from_utf8(entry(&bytes, "META-INF/manifest.xml").unwrap()).unwrap();
        assert!(
            manifest.contains("full-path=\"settings.xml\""),
            "manifest lists settings.xml: {manifest}"
        );

        // Round-trip through the ODS importer.
        let model =
            crate::convert::office_import::ods_to_model(&crate::convert::zip::read_zip(&bytes));
        let sheet = model
            .sections
            .iter()
            .flat_map(|s| &s.pages)
            .flat_map(|p| &p.blocks)
            .find_map(|b| match &b.kind {
                BlockKind::Sheet(sb) => sb.sheets.first().cloned(),
                _ => None,
            })
            .expect("sheet survives");
        assert_eq!(sheet.freeze, Some((2, 1)), "freeze round-trips");
        assert_eq!(
            sheet.rows[0].cells[0].rotation_deg,
            Some(-45),
            "rotation round-trips"
        );
    }

    #[test]
    fn m2_pptx_gradient_shadow_rtl_round_trip() {
        use crate::model::{Gradient, GradientStop, Shadow};
        let shape = Block {
            frame: Some(crate::model::Rect::new(10.0, 10.0, 100.0, 50.0)),
            kind: BlockKind::Shape(Shape {
                segments: vec![
                    PathSeg::Move(0.0, 0.0),
                    PathSeg::Line(100.0, 0.0),
                    PathSeg::Line(100.0, 50.0),
                    PathSeg::Close,
                ],
                fill: Some([1.0, 0.0, 0.0]),
                fill_gradient: Some(Gradient {
                    stops: vec![
                        GradientStop {
                            pos: 0.0,
                            color: [1.0, 0.0, 0.0],
                            alpha: 1.0,
                        },
                        GradientStop {
                            pos: 1.0,
                            color: [0.0, 0.0, 1.0],
                            alpha: 0.5,
                        },
                    ],
                    angle_deg: 90.0,
                }),
                shadow: Some(Shadow {
                    dx_pt: 3.0,
                    dy_pt: 4.0,
                    blur_pt: 2.0,
                    color: [0.0, 0.0, 0.0],
                    alpha: 0.4,
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut slide = Slide {
            shapes: vec![shape],
            ..Default::default()
        };
        slide.background_gradient = Some(crate::model::Gradient {
            stops: vec![
                crate::model::GradientStop {
                    pos: 0.0,
                    color: [0.9, 0.9, 0.9],
                    alpha: 1.0,
                },
                crate::model::GradientStop {
                    pos: 1.0,
                    color: [0.1, 0.1, 0.1],
                    alpha: 1.0,
                },
            ],
            angle_deg: 45.0,
        });
        let doc = doc_with(Block {
            kind: BlockKind::Slide(SlideBlock {
                slides: vec![slide],
            }),
            ..Default::default()
        });
        let bytes = pptx_from_model(&doc);
        let xml = String::from_utf8(entry(&bytes, "ppt/slides/slide1.xml").unwrap()).unwrap();
        assert!(
            xml.contains("<a:gradFill><a:gsLst><a:gs pos=\"0\">"),
            "shape gradient stops: {xml}"
        );
        assert!(
            xml.contains("<a:lin ang=\"5400000\" scaled=\"1\"/>"),
            "90° → 5400000"
        );
        assert!(
            xml.contains("<a:outerShdw blurRad=\"25400\" dist=\"63500\""),
            "shadow blur 2pt / dist 5pt in EMU"
        );
        assert!(
            xml.contains("<p:bg><p:bgPr><a:gradFill>"),
            "background gradient primes over the solid fill"
        );
        // Round-trip through the PPTX importer.
        let back =
            crate::convert::office_import::pptx_to_model(&crate::convert::zip::read_zip(&bytes));
        let slide = back
            .sections
            .iter()
            .flat_map(|s| &s.pages)
            .flat_map(|p| &p.blocks)
            .find_map(|b| match &b.kind {
                BlockKind::Slide(sb) => sb.slides.first().cloned(),
                _ => None,
            })
            .expect("slide survives");
        let bg = slide.background_gradient.expect("bg gradient survives");
        assert_eq!(bg.stops.len(), 2);
        assert!((bg.angle_deg - 45.0).abs() < 0.01);
        let shape_grad = slide
            .shapes
            .iter()
            .find_map(|b| match &b.kind {
                BlockKind::Shape(s) => s.fill_gradient.clone(),
                _ => None,
            })
            .expect("shape gradient survives");
        assert_eq!(shape_grad.stops.len(), 2);
        assert!((shape_grad.stops[1].alpha - 0.5).abs() < 0.01, "stop alpha");
        let shape_shadow = slide
            .shapes
            .iter()
            .find_map(|b| match &b.kind {
                BlockKind::Shape(s) => s.shadow,
                _ => None,
            })
            .expect("shadow survives");
        assert!((shape_shadow.dx_pt - 3.0).abs() < 0.05, "dx: {shape_shadow:?}");
        assert!((shape_shadow.dy_pt - 4.0).abs() < 0.05, "dy");
        assert!((shape_shadow.alpha - 0.4).abs() < 0.01, "alpha");
    }

    #[test]
    fn m2_odt_footnote_round_trip_and_anchor_link() {
        let mut doc = doc_with(Block {
            kind: BlockKind::Paragraph(Paragraph {
                runs: vec![
                    run("body"),
                    Inline::FootnoteRef {
                        id: "n1".to_string(),
                    },
                    Inline::Link {
                        href: LinkTarget::Anchor("dest".to_string()),
                        children: vec![run("jump")],
                    },
                ],
                ..Default::default()
            }),
            ..Default::default()
        });
        doc.footnotes.push(crate::model::Footnote {
            id: "n1".to_string(),
            blocks: vec![para("odt note body")],
        });
        let bytes = odt_from_model(&doc);
        let content = String::from_utf8(entry(&bytes, "content.xml").unwrap()).unwrap();
        assert!(
            content.contains("<text:note text:id=\"ftnn1\" text:note-class=\"footnote\">"),
            "text:note emitted: {content}"
        );
        assert!(content.contains("<text:note-citation>1</text:note-citation>"));
        assert!(content.contains("odt note body"));
        assert!(
            content.contains("xlink:href=\"#dest\""),
            "anchor fragment link"
        );
        // Round-trip: the ODT importer reads text:note back.
        let back =
            crate::convert::office_import::odt_to_model(&crate::convert::zip::read_zip(&bytes));
        assert_eq!(back.footnotes.len(), 1, "footnote survives the round trip");
    }

    #[test]
    fn m2_odt_char_extras_and_odp_gradient() {
        // ODT: caps/small-caps/scale attributes on the span style.
        let styled = Block {
            kind: BlockKind::Paragraph(Paragraph {
                runs: vec![Inline::Run(InlineRun {
                    text: "Caps".to_string(),
                    style: CharStyle {
                        caps: true,
                        char_scale_pct: Some(200.0),
                        size_pt: 10.0,
                        ..Default::default()
                    },
                    source_index: None,
                })],
                ..Default::default()
            }),
            ..Default::default()
        };
        let bytes = odt_from_model(&doc_with(styled));
        let content = String::from_utf8(entry(&bytes, "content.xml").unwrap()).unwrap();
        assert!(
            content.contains("fo:text-transform=\"uppercase\""),
            "caps: {content}"
        );
        assert!(content.contains("style:text-scale=\"200%\""), "text scale");

        // ODP: a gradient page background registers a draw:gradient definition.
        let slide = Slide {
            background_gradient: Some(crate::model::Gradient {
                stops: vec![
                    crate::model::GradientStop {
                        pos: 0.0,
                        color: [1.0, 0.0, 0.0],
                        alpha: 1.0,
                    },
                    crate::model::GradientStop {
                        pos: 1.0,
                        color: [0.0, 0.0, 1.0],
                        alpha: 1.0,
                    },
                ],
                angle_deg: 0.0, // left→right
            }),
            ..Default::default()
        };
        let doc = doc_with(Block {
            kind: BlockKind::Slide(SlideBlock {
                slides: vec![slide],
            }),
            ..Default::default()
        });
        let bytes = odp_from_model(&doc);
        let styles = String::from_utf8(entry(&bytes, "styles.xml").unwrap()).unwrap();
        assert!(
            styles.contains("<draw:gradient draw:name=\"Gradient1\" draw:style=\"linear\" \
draw:start-color=\"#FF0000\" draw:end-color=\"#0000FF\""),
            "gradient definition in office:styles: {styles}"
        );
        assert!(
            styles.contains("draw:angle=\"2700\""),
            "0° (L→R) → ODF 270.0 decideg"
        );
        let content = String::from_utf8(entry(&bytes, "content.xml").unwrap()).unwrap();
        assert!(
            content.contains("draw:fill=\"gradient\" draw:fill-gradient-name=\"Gradient1\""),
            "drawing-page style references the gradient: {content}"
        );
        // Round-trip: the ODP importer resolves the gradient back.
        let back =
            crate::convert::office_import::odp_to_model(&crate::convert::zip::read_zip(&bytes));
        let slide = back
            .sections
            .iter()
            .flat_map(|s| &s.pages)
            .flat_map(|p| &p.blocks)
            .find_map(|b| match &b.kind {
                BlockKind::Slide(sb) => sb.slides.first().cloned(),
                _ => None,
            })
            .expect("slide survives");
        let g = slide.background_gradient.expect("gradient survives");
        assert_eq!(g.stops.first().map(|s| s.color), Some([1.0, 0.0, 0.0]));
        assert_eq!(g.stops.last().map(|s| s.color), Some([0.0, 0.0, 1.0]));
        assert!((g.angle_deg - 0.0).abs() < 0.01, "angle round-trips");
    }

    #[test]
    fn m2_markdown_footnotes_anchor_and_soft_breaks() {
        let mut doc = doc_with(Block {
            kind: BlockKind::Paragraph(Paragraph {
                style: ParagraphStyle {
                    soft_breaks: vec![true],
                    ..Default::default()
                },
                runs: vec![
                    run("soft"),
                    Inline::LineBreak,
                    run("wrap"),
                    Inline::FootnoteRef {
                        id: "a".to_string(),
                    },
                    Inline::Link {
                        href: LinkTarget::Anchor("sec-2".to_string()),
                        children: vec![run("go")],
                    },
                ],
                ..Default::default()
            }),
            ..Default::default()
        });
        doc.footnotes.push(crate::model::Footnote {
            id: "a".to_string(),
            blocks: vec![para("md note")],
        });
        let md = markdown_from_model(&doc);
        assert!(md.contains("soft wrap"), "soft break re-flows: {md}");
        assert!(!md.contains("  \n"), "no hard break left");
        assert!(md.contains("[^a]"), "call marker");
        assert!(md.contains("[^a]: md note"), "definition list");
        assert!(md.contains("[go](#sec-2)"), "anchor fragment link");
    }

    #[test]
    fn m2_docx_cols_gap_widths_separator_round_trip() {
        let mut doc = doc_with(para("multi column"));
        {
            let geom = &mut doc.sections[0].geometry;
            geom.column_count = 2;
            geom.column_gap_pt = Some(24.0);
            geom.column_widths_pt = Some(vec![200.0, 100.0]);
            geom.column_separator = true;
        }
        let bytes = docx_from_model(&doc);
        let xml = String::from_utf8(entry(&bytes, "word/document.xml").unwrap()).unwrap();
        assert!(
            xml.contains("<w:cols w:num=\"2\" w:space=\"480\" w:sep=\"1\" w:equalWidth=\"0\">"),
            "cols attributes: {xml}"
        );
        assert!(
            xml.contains("<w:col w:w=\"4000\" w:space=\"480\"/><w:col w:w=\"2000\"/>"),
            "individual column widths (gutter on all but last)"
        );
        let back =
            crate::convert::office_import::docx_to_model(&crate::convert::zip::read_zip(&bytes));
        let geom = &back.sections[0].geometry;
        assert_eq!(geom.column_count, 2);
        assert_eq!(geom.column_gap_pt, Some(24.0), "gap round-trips");
        assert!(geom.column_separator, "separator round-trips");
        assert_eq!(
            geom.column_widths_pt,
            Some(vec![200.0, 100.0]),
            "widths round-trip"
        );
    }
}
