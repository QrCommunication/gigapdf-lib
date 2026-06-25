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
use crate::model::CellVAlign;
use crate::model::{
    Align, Block, BlockKind, Blockquote, BorderStyle, Cell, CharStyle, CodeBlock, Document, Heading,
    ImageRef, Inline, LineHeight, LinkTarget, List, ListMarker, Paragraph, Row, Section, Shape,
    Sheet, SheetBlock, SheetCell, Slide, SlideBlock, Table, TextBox, VAlign,
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
/// format's own default — black — then applies). Applied uniformly across every
/// exporter that colours runs (DOCX/ODT/PPTX/XLSX/ODS/Markdown/EPUB).
fn visible_color(style: &CharStyle) -> Option<String> {
    style.color.map(hex)
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
/// just the generating application — there is no per-document data to fabricate.
fn ooxml_app_props() -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Properties xmlns=\"http://schemas.openxmlformats.org/officeDocument/2006/extended-properties\" \
xmlns:vt=\"http://schemas.openxmlformats.org/officeDocument/2006/docPropsVTypes\">\
<Application>{META_GENERATOR}</Application></Properties>"
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
/// the model metadata. Absent fields are omitted. Mirrors the OOXML core part:
/// `dc:title` / `dc:creator` / `dc:subject` / `meta:keyword` / `dc:language`.
fn odf_meta_xml(meta: &crate::model::DocMeta) -> String {
    let mut body = format!("<meta:generator>{META_GENERATOR}</meta:generator>");
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
    if !pattrs.is_empty() {
        out.push_str(&format!("<style:paragraph-properties{pattrs}/>"));
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
    let mut ctx = DocxCtx::new(&doc.resources);

    // Per-section header/footer parts. Each [`Section`] with a header/footer gets
    // its own `headerN.xml`/`footerN.xml` (1-based by section index) referenced
    // from that section's `w:sectPr`. Rendered up front so any lists *inside* a
    // header/footer register their markers before `numbering.xml` is generated
    // (their `w:numId` must resolve), matching the body render order.
    let sect_hf: Vec<SectionHf> = doc
        .sections
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let n = i + 1;
            SectionHf {
                header: s.header.as_ref().map(|h| docx_blocks(h, &mut ctx)),
                footer: s.footer.as_ref().map(|f| docx_blocks(f, &mut ctx)),
                number: n,
            }
        })
        .collect();

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
    let has_num = !ctx.list_markers.is_empty();

    let image_exts: Vec<&str> = ctx.images.iter().map(|(_, ext)| *ext).collect();
    zip.add_deflated(
        "[Content_Types].xml",
        docx_content_types(&image_exts, has_num, &header_nums, &footer_nums).as_bytes(),
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
    zip.add_deflated("docProps/app.xml", ooxml_app_props().as_bytes());
    zip.add_deflated("word/document.xml", body.as_bytes());
    zip.add_deflated(
        "word/_rels/document.xml.rels",
        docx_rels(&image_exts, has_num, &header_nums, &footer_nums).as_bytes(),
    );
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
    }
    for (i, (bytes, ext)) in ctx.images.iter().enumerate() {
        zip.add_deflated(&format!("word/media/image{}.{ext}", i + 1), bytes);
    }
    zip.finish()
}

/// A section's rendered running header/footer inner XML (`None` when the section
/// carries none), plus its 1-based `number` driving the `header{n}.xml` /
/// `footer{n}.xml` part name and the `rIdHdr{n}` / `rIdFtr{n}` relationship id.
struct SectionHf {
    header: Option<String>,
    footer: Option<String>,
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
    /// One marker per list *instance* emitted; its position + 1 is the list's
    /// `w:numId`, and the marker drives the generated `numbering.xml` format.
    list_markers: Vec<ListMarker>,
    /// Next unique drawing/object id.
    obj_id: usize,
    resources: &'a crate::model::ResourceTable,
}

impl<'a> DocxCtx<'a> {
    fn new(resources: &'a crate::model::ResourceTable) -> Self {
        DocxCtx {
            images: Vec::new(),
            list_markers: Vec::new(),
            obj_id: 0,
            resources,
        }
    }
    fn next_obj(&mut self) -> usize {
        self.obj_id += 1;
        self.obj_id
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
    for (si, section) in doc.sections.iter().enumerate() {
        for page in &section.pages {
            page_no += 1;
            blocks.push_str(&format!(
                "<w:bookmarkStart w:id=\"{page_no}\" w:name=\"page{page_no}\"/>\
<w:bookmarkEnd w:id=\"{page_no}\"/>"
            ));
            blocks.push_str(&docx_blocks(&page.blocks, ctx));
        }
        if si != last_idx {
            // Section break: an empty paragraph whose `w:pPr` holds this section's
            // `w:sectPr` (geometry + its own header/footer refs).
            blocks.push_str(&format!(
                "<w:p><w:pPr>{}</w:pPr></w:p>",
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
    let geom = section.geometry;
    let header_ref = match hf {
        Some(h) if h.header.is_some() => {
            format!("<w:headerReference w:type=\"default\" r:id=\"rIdHdr{}\"/>", h.number)
        }
        _ => String::new(),
    };
    let footer_ref = match hf {
        Some(h) if h.footer.is_some() => {
            format!("<w:footerReference w:type=\"default\" r:id=\"rIdFtr{}\"/>", h.number)
        }
        _ => String::new(),
    };
    format!(
        "<w:sectPr>{header_ref}{footer_ref}<w:pgSz w:w=\"{w}\" w:h=\"{h}\" w:orient=\"{o}\"/>\
<w:pgMar w:top=\"{mt}\" w:right=\"{mr}\" w:bottom=\"{mb}\" w:left=\"{ml}\" w:header=\"{mt}\" w:footer=\"{mb}\" w:gutter=\"0\"/></w:sectPr>",
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

fn docx_block(block: &Block, ctx: &mut DocxCtx, out: &mut String) {
    match &block.kind {
        BlockKind::Paragraph(p) => {
            out.push_str(&docx_para(p, style_ref_id(p.style_ref.as_ref()), 0, ctx))
        }
        BlockKind::Heading(h) => out.push_str(&docx_heading(h, ctx)),
        BlockKind::List(list) => docx_list(list, ctx, out),
        BlockKind::Table(table) => out.push_str(&docx_table(table, ctx)),
        BlockKind::Image(img) => out.push_str(&docx_image_para(img, ctx)),
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
    let indented: Vec<Block> = bq.blocks.iter().map(|b| indent_block_left(b, 24.0)).collect();
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
    docx_runs(&para.runs, ctx, &mut runs);
    format!("<w:p>{ppr}{runs}</w:p>")
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
                let mut t = String::new();
                esc(&run.text, &mut t);
                out.push_str(&format!(
                    "<w:r>{rpr}<w:t xml:space=\"preserve\">{t}</w:t></w:r>",
                    rpr = docx_rpr(&run.style)
                ));
            }
            Inline::LineBreak => out.push_str("<w:r><w:br/></w:r>"),
            Inline::Image(img) => out.push_str(&docx_inline_image(img, ctx)),
            Inline::Link { href, children } => {
                // Render link children as runs wrapped in a hyperlink field. An
                // external URL → `HYPERLINK "url"`; an internal page jump →
                // `HYPERLINK \l "page{N}"`, resolving to the `w:bookmarkStart`
                // named `page{N}` that `docx_body` drops at each page boundary.
                match href {
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
                    _ => docx_runs(children, ctx, out),
                }
            }
        }
    }
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
    if style.underline {
        p.push_str("<w:u w:val=\"single\"/>");
    }
    if style.strike {
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
    ctx.list_markers.push(list.marker);
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

    format!(
        "<w:tbl><w:tblPr><w:tblW w:w=\"0\" w:type=\"auto\"/>{borders}</w:tblPr>{grid}{rows}</w:tbl>\
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

/// A block-level image, wrapped in its own (inline-drawing) paragraph.
fn docx_image_para(img: &ImageRef, ctx: &mut DocxCtx) -> String {
    format!("<w:p>{}</w:p>", docx_inline_image(img, ctx))
}

fn docx_inline_image(img: &ImageRef, ctx: &mut DocxCtx) -> String {
    // Resolve the blob via the (single) resource table threaded at the top.
    let (bytes, ext) = match ctx.resolve_image(img.resource) {
        Some(b) if !b.0.is_empty() => b,
        _ => return String::new(),
    };
    let id = ctx.next_obj();
    let rid = ctx.add_image(bytes, ext);
    // A 1"-square default extent; reflowable output sizes to the page anyway.
    let (cx, cy) = (emu(96.0), emu(96.0));
    let mut alt = String::new();
    esc(img.alt.as_deref().unwrap_or(""), &mut alt);
    format!(
        "<w:r><w:drawing><wp:inline distT=\"0\" distB=\"0\" distL=\"0\" distR=\"0\">\
<wp:extent cx=\"{cx}\" cy=\"{cy}\"/><wp:effectExtent l=\"0\" t=\"0\" r=\"0\" b=\"0\"/>\
<wp:docPr id=\"{id}\" name=\"img{id}\" descr=\"{alt}\"/><wp:cNvGraphicFramePr/>\
<a:graphic xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\">\
<a:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/picture\">\
<pic:pic xmlns:pic=\"http://schemas.openxmlformats.org/drawingml/2006/picture\">\
<pic:nvPicPr><pic:cNvPr id=\"{id}\" name=\"img{id}\"/><pic:cNvPicPr/></pic:nvPicPr>\
<pic:blipFill><a:blip xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" r:embed=\"rId{rid}\"/>\
<a:stretch><a:fillRect/></a:stretch></pic:blipFill>\
<pic:spPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"{cx}\" cy=\"{cy}\"/></a:xfrm>\
<a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></pic:spPr></pic:pic></a:graphicData></a:graphic>\
</wp:inline></w:drawing></w:r>"
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

fn docx_hdrftr_xml(tag: &str, inner: &str) -> String {
    let inner = if inner.is_empty() { "<w:p/>" } else { inner };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<w:{tag} xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\" \
xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
xmlns:wp=\"http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing\" \
xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\">{inner}</w:{tag}>"
    )
}

fn docx_content_types(
    image_exts: &[&str],
    num: bool,
    header_nums: &[usize],
    footer_nums: &[usize],
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
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
<Default Extension=\"xml\" ContentType=\"application/xml\"/>{png}{overrides}{OOXML_DOCPROPS_OVERRIDES}</Types>"
    )
}

fn docx_rels(
    image_exts: &[&str],
    num: bool,
    header_nums: &[usize],
    footer_nums: &[usize],
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
    let mut s = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<w:styles xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">{defaults}\
<w:style w:type=\"paragraph\" w:default=\"1\" w:styleId=\"Normal\"><w:name w:val=\"Normal\"/></w:style>",
    );
    let sizes = [32, 26, 24, 22, 20, 18]; // half-points: H1=16pt … H6=9pt
    for (i, sz) in sizes.iter().enumerate() {
        let lvl = i + 1;
        s.push_str(&format!(
            "<w:style w:type=\"paragraph\" w:styleId=\"Heading{lvl}\">\
<w:name w:val=\"heading {lvl}\"/><w:basedOn w:val=\"Normal\"/>\
<w:pPr><w:keepNext/><w:outlineLvl w:val=\"{ol}\"/></w:pPr>\
<w:rPr><w:b/><w:sz w:val=\"{sz}\"/></w:rPr></w:style>",
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
    out
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
    if style.underline {
        p.push_str("<w:u w:val=\"single\"/>");
    }
    if style.strike {
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
/// `markers` slice is one [`ListMarker`] per list; entry `i` drives the abstract
/// num `i + 1` (matching the `w:numId` written on paragraphs). Every level of a
/// given list shares that list's format (bullet char or ordinal style).
fn docx_numbering_xml(markers: &[ListMarker]) -> String {
    let mut abstracts = String::new();
    let mut nums = String::new();
    for (i, marker) in markers.iter().enumerate() {
        let n = i + 1;
        let (num_fmt, lvl_text_tmpl, bullet_font) = docx_list_format(*marker);
        let mut lvls = String::new();
        for lvl in 0..9 {
            let indent = twips(18.0 + 18.0 * lvl as f64);
            // `%N` placeholders are 1-based on the level for ordered lists; the
            // template is constant (the bullet char) for bullet lists.
            let lvl_text = lvl_text_tmpl.replace("%N", &format!("%{}", lvl + 1));
            lvls.push_str(&format!(
                "<w:lvl w:ilvl=\"{lvl}\"><w:start w:val=\"1\"/><w:numFmt w:val=\"{num_fmt}\"/>\
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

/// Map a [`ListMarker`] to its OOXML numbering shape:
/// `(w:numFmt value, w:lvlText template, optional <w:rPr> bullet font)`.
///
/// The template uses the sentinel `%N`, replaced per level with `%<level>` for
/// ordered lists; bullet lists embed the literal bullet character.
fn docx_list_format(marker: ListMarker) -> (&'static str, String, &'static str) {
    let bullet_font = "<w:rPr><w:rFonts w:ascii=\"Symbol\" w:hAnsi=\"Symbol\" w:hint=\"default\"/></w:rPr>";
    match marker {
        ListMarker::Decimal => ("decimal", "%N.".to_string(), ""),
        ListMarker::LowerAlpha => ("lowerLetter", "%N.".to_string(), ""),
        ListMarker::UpperAlpha => ("upperLetter", "%N.".to_string(), ""),
        ListMarker::LowerRoman => ("lowerRoman", "%N.".to_string(), ""),
        ListMarker::UpperRoman => ("upperRoman", "%N.".to_string(), ""),
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
/// ranges are preserved. A document with no sheet blocks yields a single empty
/// worksheet.
pub fn xlsx_from_model(doc: &Document) -> Vec<u8> {
    let sheets = collect_sheets(doc);
    let mut zip = ZipWriter::new();

    // Build the styles table: index 0 is the default; further indices carry a
    // numFmt and/or fill. Cells reference a style index via `s="N"`.
    let mut styler = XlsxStyler::new();
    let sheet_xmls: Vec<String> = sheets
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
    zip.add_deflated("docProps/app.xml", ooxml_app_props().as_bytes());
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
        for (i, xml) in sheet_xmls.iter().enumerate() {
            zip.add_deflated(&format!("xl/worksheets/sheet{}.xml", i + 1), xml.as_bytes());
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
}

impl XlsxAlign {
    fn is_default(self) -> bool {
        self.horizontal.is_none() && self.vertical.is_none() && !self.wrap
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
    /// Distinct borders (the model's single all-edge border per cell); index 0
    /// is the empty/no-border border.
    borders: Vec<BorderStyle>,
    /// Style records keyed by their full tuple → cellXfs index.
    xfs: Vec<XlsxXf>,
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
            borders: vec![BorderStyle::default()],
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

    /// Resolve (or create) the borderId for an optional cell border. Returns 0
    /// (no border) for `None` or a zero-width border.
    fn border_id(&mut self, border: Option<BorderStyle>) -> usize {
        match border {
            Some(b) if b.width > 0.0 => {
                if let Some(i) = self.borders.iter().position(|x| *x == b) {
                    return i;
                }
                self.borders.push(b);
                self.borders.len() - 1
            }
            _ => 0,
        }
    }

    /// Resolve a `cellXfs` index for a sheet cell's full styling.
    fn style_for(&mut self, cell: &SheetCell) -> usize {
        let num_fmt_id = self.num_fmt_id(cell.number_format.as_deref());
        let fill_idx = self.fill_id(cell.fill);
        let font_idx = self.font_id(&cell.style);
        let border_idx = self.border_id(cell.border);
        let align = XlsxAlign {
            horizontal: cell.align,
            vertical: cell.vertical_align,
            wrap: cell.wrap,
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
            s.push_str(&format!("<sz val=\"{}\"/>", num(f.size_centi as f64 / 100.0)));
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
        // all-edge style applied to every side.
        let mut borders = String::new();
        for (i, b) in self.borders.iter().enumerate() {
            if i == 0 {
                borders.push_str("<border><left/><right/><top/><bottom/><diagonal/></border>");
            } else {
                let style = xlsx_border_style(b.width);
                let color = format!("<color rgb=\"FF{}\"/>", hex(b.color));
                let edge = format!("<{{e}} style=\"{style}\">{color}</{{e}}>");
                let side = |e: &str| edge.replace("{e}", e);
                borders.push_str(&format!(
                    "<border>{}{}{}{}<diagonal/></border>",
                    side("left"),
                    side("right"),
                    side("top"),
                    side("bottom"),
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

fn xlsx_sheet_from_model(sheet: &Sheet, styler: &mut XlsxStyler) -> String {
    let mut data = String::new();
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

    let cols = xlsx_cols_xml(&sheet.col_widths);
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">\
{cols}<sheetData>{data}</sheetData>{merges}</worksheet>"
    )
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

    let model_slides = collect_slides(doc);
    let (sw, sh) = model_slides
        .first()
        .map(|s| (s.geometry.width, s.geometry.height))
        .unwrap_or((960.0, 540.0));

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
    zip.add_deflated("docProps/app.xml", ooxml_app_props().as_bytes());
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

fn pptx_slide_from_model(
    slide: &Slide,
    doc: &Document,
    media: &mut Vec<(Vec<u8>, &'static str)>,
    rels: &mut PptxSlideRels,
    slide_count: usize,
) -> String {
    let mut tree = String::new();
    let mut id = 2usize;

    // Run images encountered inside placeholder/shape/table text are hoisted to
    // standalone `p:pic` shapes appended after the text shapes (DrawingML runs
    // cannot embed a picture). Collected across all run contexts on this slide.
    let mut hoisted: Vec<RunPic> = Vec::new();
    // Tables reached via a placeholder/text-box body (e.g. the page-fallback)
    // cannot live inside a DrawingML text body, so they are hoisted to sibling
    // `p:graphicFrame`s emitted after the text shapes (#2).
    let mut hoisted_tables: Vec<Table> = Vec::new();

    // Placeholders first (title/body), positioned by their frame when present.
    for ph in &slide.placeholders {
        let (ph_type, ph_idx) = match &ph.role {
            PlaceholderRole::Title => ("title", String::new()),
            PlaceholderRole::Subtitle => ("subTitle", " idx=\"1\"".to_string()),
            PlaceholderRole::Body => ("body", " idx=\"1\"".to_string()),
            PlaceholderRole::Other(_) => ("body", " idx=\"1\"".to_string()),
        };
        let frame = pptx_xfrm(ph.block.frame, slide.geometry.width, slide.geometry.height);
        let mut rctx = PptxRunCtx::new(doc, media, rels, slide_count);
        // Preserve real list/heading structure (and hoist tables) instead of
        // flattening the body to plain paragraphs (#2). `pptx_collect_body`
        // unwraps a TextBox (the page-fallback body) into its child blocks.
        let mut built = pptx_struct_body(std::slice::from_ref(&ph.block), &mut rctx);
        hoisted.append(&mut rctx.pending_pics);
        hoisted_tables.append(&mut built.tables);
        let body = built.into_txbody();
        tree.push_str(&format!(
            "<p:sp><p:nvSpPr><p:cNvPr id=\"{id}\" name=\"ph{id}\"/><p:cNvSpPr/>\
<p:nvPr><p:ph type=\"{ph_type}\"{ph_idx}/></p:nvPr></p:nvSpPr>\
<p:spPr>{frame}</p:spPr>{body}</p:sp>"
        ));
        id += 1;
    }

    // Free-floating shapes (text boxes / images / shapes).
    for sh in &slide.shapes {
        match &sh.kind {
            BlockKind::Image(img) => {
                if let Some((bytes, ext)) = doc_image(doc, img.resource) {
                    media.push((bytes, ext));
                    let rid = rels.add_image(media.len() - 1);
                    let frame = pptx_xfrm(sh.frame, slide.geometry.width, slide.geometry.height);
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
                let frame = pptx_xfrm(sh.frame, slide.geometry.width, slide.geometry.height);
                tree.push_str(&format!(
                    "<p:sp><p:nvSpPr><p:cNvPr id=\"{id}\" name=\"s{id}\"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>\
<p:spPr>{frame}{geom}{fill}{ln}</p:spPr><p:txBody><a:bodyPr/><a:p/></p:txBody></p:sp>",
                    fill = dml_fill(&placed),
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
                let mut rctx = PptxRunCtx::new(doc, media, rels, slide_count);
                tree.push_str(&pptx_table_frame(table, r, id, &mut rctx));
                hoisted.append(&mut rctx.pending_pics);
                id += 1;
            }
            _ => {
                let frame = pptx_xfrm(sh.frame, slide.geometry.width, slide.geometry.height);
                let mut rctx = PptxRunCtx::new(doc, media, rels, slide_count);
                // Preserve list/heading structure (and hoist any nested table)
                // for a free text box too (#2).
                let mut built = pptx_struct_body(std::slice::from_ref(sh), &mut rctx);
                hoisted.append(&mut rctx.pending_pics);
                hoisted_tables.append(&mut built.tables);
                let body = built.into_txbody();
                tree.push_str(&format!(
                    "<p:sp><p:nvSpPr><p:cNvPr id=\"{id}\" name=\"t{id}\"/><p:cNvSpPr txBox=\"1\"/><p:nvPr/></p:nvSpPr>\
<p:spPr>{frame}<a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom><a:noFill/></p:spPr>{body}</p:sp>"
                ));
                id += 1;
            }
        }
    }

    // Emit the hoisted body tables as real `a:tbl` graphicFrames (#2). They have
    // no model frame (they came from a flowing body), so stack them down the
    // slide with a sensible default box spanning the slide width. Done before the
    // run-image hoists so any pictures inside a table cell are still collected.
    for (i, table) in hoisted_tables.iter().enumerate() {
        let r = crate::model::Rect::new(
            0.0,
            8.0 + 220.0 * i as f64,
            slide.geometry.width.max(1.0),
            200.0,
        );
        let mut rctx = PptxRunCtx::new(doc, media, rels, slide_count);
        tree.push_str(&pptx_table_frame(table, r, id, &mut rctx));
        hoisted.append(&mut rctx.pending_pics);
        id += 1;
    }

    // Emit the hoisted run images as standalone pictures (DrawingML run images
    // are not expressible; they become real `p:pic` shapes referencing the same
    // media + relationship). Stacked at a default 96pt box near the top-left.
    for (i, pic) in hoisted.iter().enumerate() {
        let x = emu(8.0 + 8.0 * i as f64);
        let y = emu(8.0 + 8.0 * i as f64);
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
        id += 1;
    }

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<p:sld xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" \
xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\">\
<p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>\
<p:grpSpPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"0\" cy=\"0\"/>\
<a:chOff x=\"0\" y=\"0\"/><a:chExt cx=\"0\" cy=\"0\"/></a:xfrm></p:grpSpPr>{tree}</p:spTree></p:cSld></p:sld>"
    )
}

/// `<a:xfrm>` from an optional model frame; absent ⇒ omit (the layout positions
/// the placeholder). Model rectangles are top-down points already.
fn pptx_xfrm(frame: Option<crate::model::Rect>, _w: f64, _h: f64) -> String {
    match frame {
        Some(r) => format!(
            "<a:xfrm><a:off x=\"{x}\" y=\"{y}\"/><a:ext cx=\"{cx}\" cy=\"{cy}\"/></a:xfrm>",
            x = emu(r.x),
            y = emu(r.y),
            cx = emu(r.w.max(1.0)),
            cy = emu(r.h.max(1.0)),
        ),
        None => String::new(),
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

    format!(
        "<p:graphicFrame><p:nvGraphicFramePr>\
<p:cNvPr id=\"{id}\" name=\"tbl{id}\"/><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr>\
<p:xfrm><a:off x=\"{x}\" y=\"{y}\"/><a:ext cx=\"{cx}\" cy=\"{cy}\"/></p:xfrm>\
<a:graphic><a:graphicData uri=\"{uri}\">\
<a:tbl><a:tblPr firstRow=\"1\" bandRow=\"1\"/>{grid}{rows}</a:tbl>\
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
    let body = dml_text_body(&blocks_to_paras(&cell.blocks), ctx);
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
            format!(
                "<a:buFont typeface=\"Arial\"/><a:buChar char=\"{b}\"/>"
            )
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
    if style.bold {
        attrs.push_str(" b=\"1\"");
    }
    if style.italic {
        attrs.push_str(" i=\"1\"");
    }
    if style.underline {
        attrs.push_str(" u=\"sng\"");
    }
    if style.strike {
        attrs.push_str(" strike=\"sngStrike\"");
    }
    // Run superscript/subscript → `a:rPr@baseline` (ECMA-376 §21.1.2.3.9), the
    // PPTX analogue of DOCX `w:vertAlign` and ODF `style:text-position`. The value
    // is a percentage of the line in 1000ths; PowerPoint's defaults are +30% for
    // superscript and -25% for subscript, so 30000 / -25000 mirror the importer's
    // `baseline > 0` / `< 0` → `VAlign::Super` / `Sub`.
    match style.vertical_align {
        VAlign::Super => attrs.push_str(" baseline=\"30000\""),
        VAlign::Sub => attrs.push_str(" baseline=\"-25000\""),
        VAlign::Baseline => {}
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
    type MasterKey<'a> = (PageGeometry, &'a Option<Vec<Block>>, &'a Option<Vec<Block>>);
    let mut keys: Vec<MasterKey> = Vec::new();
    let mut section_master = Vec::with_capacity(doc.sections.len());
    for s in &doc.sections {
        let key = (s.geometry, &s.header, &s.footer);
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
        let geom = section.map(|s| s.geometry).unwrap_or_default();
        let (master_name, layout_name) = odt_master_names(m);

        page_layouts.push_str(&odf_page_layout(&layout_name, geom));

        // Running header/footer for this master (rendered into hf_ctx so their
        // styles/images land in styles.xml).
        let header_xml = section
            .and_then(|s| s.header.as_ref())
            .map(|h| odt_blocks(h, hf_ctx));
        let footer_xml = section
            .and_then(|s| s.footer.as_ref())
            .map(|f| odt_blocks(f, hf_ctx));
        let header = match &header_xml {
            Some(h) => format!("<style:header>{h}</style:header>"),
            None => String::new(),
        };
        let footer = match &footer_xml {
            Some(f) => format!("<style:footer>{f}</style:footer>"),
            None => String::new(),
        };
        master_styles.push_str(&format!(
            "<style:master-page style:name=\"{master_name}\" style:page-layout-name=\"{layout_name}\">{header}{footer}</style:master-page>"
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

    let mut ctx = OdfCtx::new(&doc.resources);

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
    // `ctx.auto` (content.xml). Header/footer pictures continue the body's count.
    let mut hf_ctx = OdfCtx::new(&doc.resources);
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
        odf_manifest("text", &image_exts).as_bytes(),
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
    let sheets = collect_sheets(doc);
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
    zip.add_deflated(
        "META-INF/manifest.xml",
        odf_manifest("spreadsheet", &[]).as_bytes(),
    );
    zip.finish()
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
    let slides = collect_slides(doc);
    let (pw, ph) = slides
        .first()
        .map(|s| (s.geometry.width, s.geometry.height))
        .unwrap_or((960.0, 540.0));
    let mut zip = ZipWriter::new();
    zip.add_stored(
        "mimetype",
        b"application/vnd.oasis.opendocument.presentation",
    );

    let mut ctx = OdfCtx::new(&doc.resources);
    let body = odp_body(&slides, doc, &mut ctx);

    zip.add_deflated(
        "content.xml",
        odp_model_content(&ctx.auto, &body).as_bytes(),
    );
    zip.add_deflated(
        "styles.xml",
        odp_styles_xml_model(pw, ph, &odf_default_lang_styles(&doc.meta)).as_bytes(),
    );
    zip.add_deflated("meta.xml", odf_meta_xml(&doc.meta).as_bytes());
    let image_exts: Vec<&str> = ctx.images.iter().map(|(_, ext)| *ext).collect();
    zip.add_deflated(
        "META-INF/manifest.xml",
        odf_manifest("presentation", &image_exts).as_bytes(),
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
    resources: &'a crate::model::ResourceTable,
}

impl<'a> OdfCtx<'a> {
    fn new(resources: &'a crate::model::ResourceTable) -> Self {
        OdfCtx {
            auto: String::new(),
            images: Vec::new(),
            image_base: 0,
            style_id: 0,
            resources,
        }
    }
    fn next_style(&mut self) -> usize {
        let id = self.style_id;
        self.style_id += 1;
        id
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
            let indented: Vec<Block> =
                bq.blocks.iter().map(|b| indent_block_left(b, 24.0)).collect();
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
    let pname = format!("Code{pid}");
    ctx.auto.push_str(&format!(
        "<style:style style:name=\"{pname}\" style:family=\"paragraph\">\
<style:paragraph-properties fo:background-color=\"#f2f2f2\" \
fo:border=\"0.5pt solid #cccccc\" fo:padding=\"3pt\"/>\
<style:text-properties style:font-name=\"Courier New\" fo:font-family=\"'Courier New'\" \
style:font-family-generic=\"modern\" fo:font-size=\"10pt\"/></style:style>"
    ));
    let sid = ctx.next_style();
    let sname = format!("CodeT{sid}");
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
    let pname = format!("Hr{pid}");
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
            if direct.is_empty() {
                // Reference the named style directly — no automatic style needed.
                let mut n = String::new();
                esc(named, &mut n);
                n
            } else {
                // Automatic style inheriting the named style, carrying overrides.
                let pid = ctx.next_style();
                let auto_name = format!("P{pid}");
                let mut parent = String::new();
                esc(named, &mut parent);
                ctx.auto.push_str(&format!(
                    "<style:style style:name=\"{auto_name}\" style:family=\"paragraph\" \
style:parent-style-name=\"{parent}\"><style:paragraph-properties{direct}/></style:style>"
                ));
                auto_name
            }
        }
        None => {
            let pid = ctx.next_style();
            let auto_name = format!("P{pid}");
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
                let sname = format!("T{sid}");
                ctx.auto.push_str(&odf_span_style(&sname, &run.style));
                out.push_str(&format!("<text:span text:style-name=\"{sname}\">"));
                esc(&run.text, out);
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
        }
    }
}

fn odt_list(list: &List, ctx: &mut OdfCtx) -> String {
    let sid = ctx.next_style();
    let sname = format!("L{sid}");
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
    let tname = format!("Tbl{tid}");
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
                let rsn = format!("Tr{rsid}");
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
                let csn = format!("Tc{csid}");
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
    let sname = format!("Sh{sid}");
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
    props
}

/// ODF automatic `<style:style family="paragraph">` from a model paragraph
/// style. The `<style:paragraph-properties>` shell is always emitted (matching
/// the historical output) even when fully default.
fn odf_para_style(name: &str, para: &Paragraph) -> String {
    let attrs = odf_para_prop_attrs(&para.style);
    format!(
        "<style:style style:name=\"{name}\" style:family=\"paragraph\">\
<style:paragraph-properties{attrs}/></style:style>"
    )
}

/// ODF `<style:style family="text">` from a model char style.
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
    p.push_str("/>");
    format!("<style:style style:name=\"{name}\" style:family=\"text\">{p}</style:style>")
}

/// ODF list style: a per-level bullet or number definition from the marker.
fn odf_list_style(name: &str, list: &List) -> String {
    let mut levels = String::new();
    for lvl in 1..=9u32 {
        if list.ordered {
            let fmt = match list.marker {
                ListMarker::LowerAlpha => "a",
                ListMarker::UpperAlpha => "A",
                ListMarker::LowerRoman => "i",
                ListMarker::UpperRoman => "I",
                _ => "1",
            };
            levels.push_str(&format!(
                "<text:list-level-style-number text:level=\"{lvl}\" style:num-suffix=\".\" style:num-format=\"{fmt}\">\
<style:list-level-properties text:space-before=\"{sb}pt\" text:min-label-width=\"18pt\"/>\
</text:list-level-style-number>",
                sb = 18 * (lvl - 1),
            ));
        } else {
            let bullet = match list.marker {
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
fn odf_manifest(kind: &str, image_exts: &[&str]) -> String {
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
        };
        if key.fill.is_none()
            && key.data_style.is_none()
            && key.font.is_none()
            && key.border.is_none()
            && key.align.is_none()
            && key.valign.is_none()
            && !key.wrap
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
    match &cell.value {
        CellValue::Empty => format!("<table:table-cell{attrs}/>"),
        CellValue::Number(n) => format!(
            "<table:table-cell{attrs} office:value-type=\"float\" office:value=\"{v}\"><text:p>{disp}</text:p></table:table-cell>",
            v = num(*n),
            disp = num(*n),
        ),
        CellValue::Bool(b) => format!(
            "<table:table-cell{attrs} office:value-type=\"boolean\" office:boolean-value=\"{bv}\"><text:p>{disp}</text:p></table:table-cell>",
            bv = if *b { "true" } else { "false" },
            disp = if *b { "TRUE" } else { "FALSE" },
        ),
        CellValue::Text(t) => {
            let mut esc_t = String::new();
            esc(t, &mut esc_t);
            format!(
                "<table:table-cell{attrs} office:value-type=\"string\"><text:p>{esc_t}</text:p></table:table-cell>"
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
xmlns:number=\"urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0\" office:version=\"1.3\">\
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
        // `draw:name="page{N}"` is the jump target for an internal
        // `LinkTarget::Page` (`text:a xlink:href="#page{N}"`, emitted by the shared
        // `odt_runs`). The 1-based slide index matches the `#page{N}` anchor the
        // ODT export uses, so the same model link resolves in both ODF presentations
        // and text documents.
        body.push_str(&format!(
            "<draw:page draw:name=\"page{}\" draw:style-name=\"dp1\" draw:master-page-name=\"Default\">",
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
    let sname = format!("Nt{sid}");
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
                "<draw:frame draw:style-name=\"frI\" draw:layer=\"layout\"{pres} \
svg:x=\"{x}pt\" svg:y=\"{y}pt\" svg:width=\"{w}pt\" svg:height=\"{h}pt\">\
<draw:image xlink:href=\"Pictures/img{n}.{ext}\" xlink:type=\"simple\" xlink:show=\"embed\" \
xlink:actuate=\"onLoad\"/></draw:frame>",
                x = num(r.x),
                y = num(r.y),
                w = num(r.w.max(1.0)),
                h = num(r.h.max(1.0)),
            ));
        }
        BlockKind::Shape(shape) => {
            let placed = shape_to_placed(shape);
            let sid = ctx.next_style();
            let sname = format!("Sh{sid}");
            ctx.auto.push_str(&odf_shape_style(&sname, &placed));
            if shape_is_rect(&placed) {
                out.push_str(&format!(
                    "<draw:rect draw:style-name=\"{sname}\" draw:layer=\"layout\"{pres} \
svg:x=\"{x}pt\" svg:y=\"{y}pt\" svg:width=\"{w}pt\" svg:height=\"{h}pt\"/>",
                    x = num(r.x),
                    y = num(r.y),
                    w = num(r.w.max(1.0)),
                    h = num(r.h.max(1.0)),
                ));
            } else {
                out.push_str(&format!(
                    "<draw:path draw:style-name=\"{sname}\" draw:layer=\"layout\"{pres} \
svg:x=\"{x}pt\" svg:y=\"{y}pt\" svg:width=\"{w}pt\" svg:height=\"{h}pt\" \
svg:viewBox=\"0 0 {vw} {vh}\" svg:d=\"{d}\"/>",
                    x = num(r.x),
                    y = num(r.y),
                    w = num(r.w.max(1.0)),
                    h = num(r.h.max(1.0)),
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
                "<draw:frame draw:layer=\"layout\"{pres} \
svg:x=\"{x}pt\" svg:y=\"{y}pt\" svg:width=\"{w}pt\" svg:height=\"{h}pt\">{table_xml}</draw:frame>",
                x = num(r.x),
                y = num(r.y),
                w = num(r.w.max(1.0)),
                h = num(r.h.max(1.0)),
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
                let sname = format!("Tx{sid}");
                ctx.auto.push_str(&format!(
                    "<style:style style:name=\"{sname}\" style:family=\"graphic\">\
<style:graphic-properties draw:fill=\"none\" draw:stroke=\"none\" \
draw:auto-grow-width=\"false\" draw:auto-grow-height=\"false\" fo:padding=\"0pt\" \
draw:textarea-vertical-align=\"top\"/></style:style>"
                ));
                out.push_str(&format!(
                    "<draw:frame draw:style-name=\"{sname}\" draw:layer=\"layout\"{pres} \
svg:x=\"{x}pt\" svg:y=\"{y}pt\" svg:width=\"{w}pt\" svg:height=\"{h}pt\"><draw:text-box>{content}</draw:text-box></draw:frame>",
                    x = num(r.x),
                    y = num(r.y),
                    w = num(r.w.max(1.0)),
                    h = num(r.h.max(1.0)),
                ));
            }

            // Hoisted tables: each a real `table:table` in its own positioned
            // `draw:frame`, stacked down the slide with a default box.
            for (i, table) in tables.iter().enumerate() {
                let ty = r.y + 8.0 + 220.0 * i as f64;
                let table_xml = odt_table(table, ctx);
                out.push_str(&format!(
                    "<draw:frame draw:layer=\"layout\"{pres} \
svg:x=\"{x}pt\" svg:y=\"{y}pt\" svg:width=\"{w}pt\" svg:height=\"200pt\">{table_xml}</draw:frame>",
                    x = num(r.x),
                    y = num(ty),
                    w = num(r.w.max(1.0)),
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
xmlns:xlink=\"http://www.w3.org/1999/xlink\" office:version=\"1.3\">\
<office:automatic-styles>{base}{auto}</office:automatic-styles>\
<office:body><office:presentation>{body}</office:presentation></office:body></office:document-content>"
    )
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
    // Fallback: one slide per page, blocks → a single Body placeholder.
    for section in &doc.sections {
        for page in &section.pages {
            if page.blocks.is_empty() {
                continue;
            }
            let body_block = Block {
                kind: BlockKind::TextBox(TextBox {
                    blocks: page.blocks.clone(),
                }),
                ..Block::default()
            };
            out.push(Slide {
                geometry: section.geometry,
                shapes: Vec::new(),
                placeholders: vec![crate::model::Placeholder {
                    role: PlaceholderRole::Body,
                    block: body_block,
                }],
                notes: None,
                background: None,
            });
        }
    }
    out
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
                item.blocks = item.blocks.iter().map(|ib| indent_block_left(ib, pt)).collect();
            }
        }
        BlockKind::TextBox(tb) => {
            tb.blocks = tb.blocks.iter().map(|ib| indent_block_left(ib, pt)).collect();
        }
        BlockKind::Blockquote(bq) => {
            bq.blocks = bq.blocks.iter().map(|ib| indent_block_left(ib, pt)).collect();
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

    let w = MdWriter::new(&doc.resources);

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

    // Collapse any run of >2 blank lines and guarantee a single trailing newline.
    md_tidy(out)
}

/// Threads the resource table through the walk so image references can resolve
/// their stored format tag (→ filename extension).
struct MdWriter<'a> {
    resources: &'a crate::model::ResourceTable,
}

impl<'a> MdWriter<'a> {
    fn new(resources: &'a crate::model::ResourceTable) -> Self {
        MdWriter { resources }
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
        let text = self.inlines(&p.runs);
        let trimmed = text.trim_matches(|c| c == ' ' || c == '\t');
        if trimmed.is_empty() {
            return; // an empty paragraph contributes no Markdown
        }
        out.push_str(trimmed);
        out.push_str("\n\n");
    }

    /// Render a list at nesting `level` (0 = top). Each item's first paragraph
    /// (or heading) carries the marker; remaining item blocks are indented
    /// continuation, and nested lists recurse with `level + 1`.
    fn list(&self, list: &List, level: usize, out: &mut String) {
        let indent = "    ".repeat(level); // 4 spaces per nesting level
        for (i, item) in list.items.iter().enumerate() {
            let marker = if list.ordered {
                format!("{}.", i + 1)
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
                    BlockKind::List(nested) => self.list(nested, level + 1, out),
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
        let alt = img
            .alt
            .as_deref()
            .map(md_escape)
            .unwrap_or_default();
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
        || s.starts_with(['!', '&', '*', '?', '|', '>', '%', '@', '`', '"', '\'', '#', '-'])
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
    // CommonMark has no portable colour syntax; GFM permits inline HTML, so an
    // explicitly-set run colour is carried by an outer
    // `<span style="color:#RRGGBB">…</span>` wrapping the emphasised body.
    if let Some(c) = visible_color(style) {
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
    let needs_pad =
        text.starts_with('`') || text.ends_with('`') || (longest > 0 && text == "`".repeat(text.len()));
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
    if body.is_empty() {
        body.push_str("<h1>");
        esc(title, &mut body);
        body.push_str("</h1>");
    }
    (epub_xhtml_doc(&lang, title, &body), toc.headings)
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
                };
                out.push_str("<a href=\"");
                esc(&target, out);
                out.push_str("\">");
                xhtml_inlines(children, doc, out);
                out.push_str("</a>");
            }
        }
    }
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
    if style.strike {
        decos.push("line-through");
    }
    if !decos.is_empty() {
        css.push_str(&format!(";text-decoration:{}", decos.join(" ")));
    }
    if let Some(c) = visible_color(style) {
        css.push_str(&format!(";color:#{c}"));
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
    let body =
        format!("<nav epub:type=\"toc\" id=\"toc\"><h1>Table of Contents</h1><ol>{list}</ol></nav>");
    epub_xhtml_doc(&lang, "Table of Contents", &body)
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
                    },
                    ListItem {
                        blocks: vec![para("second")],
                        level: 0,
                    },
                ],
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
            },
            header: Some(vec![para("SEC1 HEADER")]),
            footer: None,
            pages: vec![Page {
                blocks: vec![para("section one body")],
                absolute: false,
            }],
        };
        let sec2 = Section {
            geometry: PageGeometry {
                width: 792.0,
                height: 612.0,
                margins: Margins::uniform(36.0),
            },
            header: None,
            footer: Some(vec![para("SEC2 FOOTER")]),
            pages: vec![Page {
                blocks: vec![para("section two body")],
                absolute: false,
            }],
        };
        let doc = Document {
            sections: vec![sec1, sec2],
            ..Default::default()
        };
        let bytes = docx_from_model(&doc);
        let xml = String::from_utf8(entry(&bytes, "word/document.xml").unwrap()).unwrap();

        // Exactly two section properties total.
        assert_eq!(xml.matches("<w:sectPr>").count(), 2, "one w:sectPr per section: {xml}");
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
        assert!(entry(&bytes, "word/footer1.xml").is_none(), "no spurious footer1 part");
        assert!(entry(&bytes, "word/header2.xml").is_none(), "no spurious header2 part");
        // Content-types + rels declare the right parts.
        let ct = String::from_utf8(entry(&bytes, "[Content_Types].xml").unwrap()).unwrap();
        assert!(ct.contains("/word/header1.xml"), "header1 content-type override");
        assert!(ct.contains("/word/footer2.xml"), "footer2 content-type override");
        let rels =
            String::from_utf8(entry(&bytes, "word/_rels/document.xml.rels").unwrap()).unwrap();
        assert!(rels.contains("Id=\"rIdHdr1\""), "rIdHdr1 relationship: {rels}");
        assert!(rels.contains("Id=\"rIdFtr2\""), "rIdFtr2 relationship: {rels}");
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
            },
            shapes: Vec::new(),
            placeholders: vec![Placeholder {
                role: PlaceholderRole::Title,
                block,
            }],
            notes: None,
            background: None,
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
        let content = String::from_utf8(entry(&ods_from_model(&doc), "content.xml").unwrap()).unwrap();
        assert!(
            content.contains("style:vertical-align=\"middle\""),
            "Middle → style:vertical-align=middle: {content}"
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
        let content = String::from_utf8(entry(&odt_from_model(&doc), "content.xml").unwrap()).unwrap();
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
            },
            shapes: vec![table],
            placeholders: Vec::new(),
            notes: None,
            background: None,
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
            },
            shapes: Vec::new(),
            placeholders: vec![Placeholder {
                role: PlaceholderRole::Title,
                block: para("Slide title"),
            }],
            notes: None,
            background: None,
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
                },
                shapes: Vec::new(),
                placeholders: vec![crate::model::Placeholder {
                    role: crate::model::PlaceholderRole::Title,
                    block: para(&format!("Slide {}", i + 1)),
                }],
                notes: None,
                background: None,
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
            },
            shapes: Vec::new(),
            placeholders: vec![Placeholder {
                role: PlaceholderRole::Body,
                block: para("Body text"),
            }],
            notes: None,
            background: None,
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
            },
            shapes: Vec::new(),
            placeholders: vec![Placeholder {
                role: PlaceholderRole::Title,
                block: para("Slide title"),
            }],
            notes: Some(vec![para(notes_text)]),
            background: None,
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
            },
            shapes: vec![table],
            placeholders: Vec::new(),
            notes: None,
            background: None,
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
                    },
                    ListItem {
                        blocks: vec![para("beta")],
                        level: 1,
                    },
                ],
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
        assert!(s.contains("<a:buChar char=\"•\"/>"), "list bullet preserved: {s}");
        assert!(s.contains("lvl=\"1\""), "nested list item carries its level: {s}");
        // The table is a real DrawingML table, hoisted to a graphicFrame.
        assert!(s.contains("<p:graphicFrame>"), "table → graphicFrame: {s}");
        assert!(s.contains("<a:tbl>"), "real DrawingML table (not flattened): {s}");
        assert_eq!(s.matches("<a:gridCol").count(), 2, "two grid columns");
        // The heading text survives as a (bold) paragraph, list/cell text too.
        for t in ["Agenda", "alpha", "beta", "H1", "D2"] {
            assert!(s.contains(t), "text {t} preserved: {s}");
        }
        assert!(s.contains("<a:rPr") && s.contains(" b=\"1\""), "heading run is bold: {s}");
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
        assert!(content.contains("<table:table"), "real ODF table (not flattened): {content}");
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
        }
    }

    #[test]
    fn xlsx_styler_emits_font_color_border_alignment() {
        let bytes = xlsx_from_model(&sheet_doc(styled_sheet()));
        let styles = String::from_utf8(entry(&bytes, "xl/styles.xml").unwrap()).unwrap();
        // A real font record (bold Arial 14) — not the hardcoded Calibri-only table.
        assert!(styles.contains("<name val=\"Arial\"/>"), "font family: {styles}");
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
        assert!(content.contains("fo:border=\"1pt solid #000000\""), "border");
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
        assert!(content.contains("<table:covered-table-cell/>"), "covered cell");
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
            }],
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
            },
            header: None,
            footer: None,
            pages: vec![Page {
                blocks: vec![para("portrait one")],
                absolute: false,
            }],
        };
        let sec2 = Section {
            geometry: PageGeometry {
                width: 842.0,
                height: 595.0,
                margins: Margins::uniform(36.0),
            },
            header: None,
            footer: None,
            pages: vec![Page {
                blocks: vec![para("landscape two")],
                absolute: false,
            }],
        };
        let doc = Document {
            sections: vec![sec1, sec2],
            ..Default::default()
        };
        let bytes = odt_from_model(&doc);
        let styles = String::from_utf8(entry(&bytes, "styles.xml").unwrap()).unwrap();

        // Two page layouts (pm1 + pm2) and two master pages (Standard + Master2).
        assert!(styles.contains("style:name=\"pm1\""), "first page layout: {styles}");
        assert!(styles.contains("style:name=\"pm2\""), "second page layout: {styles}");
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
            styles.contains("fo:page-width=\"595pt\" fo:page-height=\"842pt\" \
style:print-orientation=\"portrait\""),
            "section 1 A4 portrait layout: {styles}"
        );
        assert!(
            styles.contains("fo:page-width=\"842pt\" fo:page-height=\"595pt\" \
style:print-orientation=\"landscape\""),
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
        assert!(!styles.contains("<alignment"), "no alignment for a plain cell");
        let sheet = String::from_utf8(entry(&xlsx, "xl/worksheets/sheet1.xml").unwrap()).unwrap();
        assert!(!sheet.contains("ht="), "no row height for a plain row");

        let ods = ods_from_model(&sheet_doc(plain));
        let content = String::from_utf8(entry(&ods, "content.xml").unwrap()).unwrap();
        assert!(!content.contains("fo:font-family"), "no font props by default");
        assert!(!content.contains("fo:border"), "no border by default");
        assert!(!content.contains("style:row-height"), "no row height by default");
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
        assert!(chap.contains("<h1 id=\"sec1-h1\">Chapter One</h1>"), "{chap}");
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
    fn markdown_unset_run_emits_no_colour_span_but_explicit_black_does() {
        // `visible_color` keys off explicit-vs-unset, not the colour value: a run
        // whose colour is genuinely unset (`None`) emits no inline-HTML span (the
        // format default — black — applies); an *explicitly* black run
        // (`Some([0,0,0])`) round-trips its deliberate choice with a span (#2).
        let unset = CharStyle::default(); // color: None
        let md = markdown_from_model(&md_doc(vec![md_para(vec![styled("plain", unset)])]));
        assert!(!md.contains("<span"), "no colour span for unset run: {md}");
        assert!(md.contains("plain"), "text still present: {md}");

        let explicit_black = CharStyle {
            color: Some([0.0, 0.0, 0.0]),
            ..Default::default()
        };
        let md_black =
            markdown_from_model(&md_doc(vec![md_para(vec![styled("ink", explicit_black)])]));
        assert!(
            md_black.contains("<span style=\"color:#000000\">ink</span>"),
            "explicit black → colour span emitted: {md_black}"
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
        });
        let mut doc =
            md_doc(vec![md_para(vec![url_link, run(" "), page_link, run(" "), img])]);
        doc.resources = resources;
        let md = markdown_from_model(&doc);
        assert!(md.contains("[site](https://example.com)"), "url link: {md}");
        // LinkTarget::Page(2) is zero-based → human page 3.
        assert!(md.contains("[see](#page-3)"), "internal page link: {md}");
        assert!(md.contains("![alt](image-5.png)"), "image: {md}");
    }

    #[test]
    fn markdown_renders_nested_ordered_and_unordered_lists() {
        let nested = Block {
            kind: BlockKind::List(List {
                ordered: false,
                marker: ListMarker::Bullet('•'),
                items: vec![ListItem {
                    blocks: vec![para("sub")],
                    level: 1,
                }],
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
                    },
                    ListItem {
                        blocks: vec![para("two")],
                        level: 0,
                    },
                ],
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
        assert!(md.contains("```rust\nfn main() {}\n```"), "fenced + lang: {md}");
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
        let md = markdown_from_model(&md_doc(vec![
            para("before"),
            rule_blk(),
            para("after"),
        ]));
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
            b.iter().any(|x| matches!(&x.kind, BlockKind::HorizontalRule)),
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
        assert!(html.contains("<pre><code class=\"language-rust\">"), "code: {html}");
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
        assert!(out.contains("<pre><code class=\"language-js\">"), "code: {out}");
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
        assert!(document.contains("Courier New"), "code uses a mono font: {document}");
        assert!(document.contains("x = 1"), "code text present");
        assert!(document.contains("<w:pBdr>"), "rule emits a paragraph border");
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
            },
            shapes: Vec::new(),
            placeholders: vec![Placeholder {
                role: PlaceholderRole::Title,
                block: para("T"),
            }],
            notes: None,
            background: None,
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

        let manifest =
            String::from_utf8(entry(bytes, "META-INF/manifest.xml").unwrap()).unwrap();
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
        let manifest =
            String::from_utf8(entry(&bytes, "META-INF/manifest.xml").unwrap()).unwrap();
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
        };
        let list = Block {
            kind: BlockKind::List(List {
                ordered: false,
                marker: ListMarker::Bullet('•'),
                items: vec![item("top", 0), item("nested", 1), item("back", 0)],
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
}

