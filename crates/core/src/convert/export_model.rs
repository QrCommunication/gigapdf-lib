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
    col_letter, dml_cust_geom, dml_fill, dml_line, emu, esc, odf_path_d, odf_shape_style,
    shape_is_rect, twips,
};
use crate::convert::zip::ZipWriter;
use crate::convert::PlacedShape;
use crate::model::{
    Align, Block, BlockKind, BorderStyle, Cell, CharStyle, Document, Heading, ImageRef, Inline,
    LineHeight, LinkTarget, List, ListMarker, Paragraph, Row, Shape, Sheet, SheetBlock, SheetCell,
    Slide, SlideBlock, Table, TextBox, VAlign,
};
use crate::model::{CellValue, PlaceholderRole};

// ───────────────────────────── shared model walkers ─────────────────────────────

/// An RGB triple (`0..=1`) as an upper-case `RRGGBB` hex string.
fn hex(rgb: [f64; 3]) -> String {
    let q = |c: f64| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("{:02X}{:02X}{:02X}", q(rgb[0]), q(rgb[1]), q(rgb[2]))
}

/// A char-style's colour as hex, only when set and not (near-)black.
fn visible_color(style: &CharStyle) -> Option<String> {
    match style.color {
        Some([r, g, b]) if r > 0.02 || g > 0.02 || b > 0.02 => Some(hex([r, g, b])),
        _ => None,
    }
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

    let body = docx_body(doc, &mut ctx);

    // Render header/footer up front so any lists *inside* them register their
    // markers before `numbering.xml` is generated (their `w:numId` must resolve).
    let header_inner = doc
        .sections
        .first()
        .and_then(|s| s.header.as_ref())
        .map(|h| docx_blocks(h, &mut ctx));
    let footer_inner = doc
        .sections
        .first()
        .and_then(|s| s.footer.as_ref())
        .map(|f| docx_blocks(f, &mut ctx));
    let has_header = header_inner.is_some();
    let has_footer = footer_inner.is_some();
    let has_num = !ctx.list_markers.is_empty();

    zip.add_deflated(
        "[Content_Types].xml",
        docx_content_types(ctx.images.len(), has_num, has_header, has_footer).as_bytes(),
    );
    zip.add_deflated(
        "_rels/.rels",
        b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" \
Target=\"word/document.xml\"/></Relationships>",
    );
    zip.add_deflated("word/document.xml", body.as_bytes());
    zip.add_deflated(
        "word/_rels/document.xml.rels",
        docx_rels(ctx.images.len(), has_num, has_header, has_footer).as_bytes(),
    );
    zip.add_deflated("word/styles.xml", docx_styles_xml().as_bytes());
    if has_num {
        zip.add_deflated(
            "word/numbering.xml",
            docx_numbering_xml(&ctx.list_markers).as_bytes(),
        );
    }
    if let Some(inner) = &header_inner {
        zip.add_deflated(
            "word/header1.xml",
            docx_hdrftr_xml("hdr", inner).as_bytes(),
        );
    }
    if let Some(inner) = &footer_inner {
        zip.add_deflated(
            "word/footer1.xml",
            docx_hdrftr_xml("ftr", inner).as_bytes(),
        );
    }
    for (i, img) in ctx.images.iter().enumerate() {
        zip.add_deflated(&format!("word/media/image{}.png", i + 1), img);
    }
    zip.finish()
}

/// Mutable state threaded through a DOCX build: a flat image list (global order →
/// `media/imageN.png` + `rId`), a running list-instance counter (each [`List`]
/// gets a distinct `w:numId`), and the document's resource table for image blobs.
struct DocxCtx<'a> {
    images: Vec<Vec<u8>>,
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
    /// Register image bytes, returning the relationship id (`rId{N}`, 100-based).
    fn add_image(&mut self, png: Vec<u8>) -> usize {
        self.images.push(png);
        100 + self.images.len() - 1
    }
    /// Resolve an image blob by resource key.
    fn resolve_image(&self, key: u64) -> Option<Vec<u8>> {
        self.resources.images.get(&key).map(|r| r.bytes.clone())
    }
}

fn docx_body(doc: &Document, ctx: &mut DocxCtx) -> String {
    let mut blocks = String::new();
    for section in &doc.sections {
        for page in &section.pages {
            blocks.push_str(&docx_blocks(&page.blocks, ctx));
        }
    }
    // A trailing empty paragraph keeps Word happy when the body would otherwise
    // end on a table (a `w:tbl` cannot be the last body child).
    let geom = doc.sections.first().map(|s| s.geometry).unwrap_or_default();
    let header_ref = if doc
        .sections
        .first()
        .and_then(|s| s.header.as_ref())
        .is_some()
    {
        "<w:headerReference w:type=\"default\" r:id=\"rIdHdr\"/>"
    } else {
        ""
    };
    let footer_ref = if doc
        .sections
        .first()
        .and_then(|s| s.footer.as_ref())
        .is_some()
    {
        "<w:footerReference w:type=\"default\" r:id=\"rIdFtr\"/>"
    } else {
        ""
    };
    let sect = format!(
        "<w:sectPr>{header_ref}{footer_ref}<w:pgSz w:w=\"{w}\" w:h=\"{h}\" w:orient=\"{o}\"/>\
<w:pgMar w:top=\"{mt}\" w:right=\"{mr}\" w:bottom=\"{mb}\" w:left=\"{ml}\" w:header=\"{mt}\" w:footer=\"{mb}\" w:gutter=\"0\"/></w:sectPr>",
        w = twips(geom.width),
        h = twips(geom.height),
        o = if geom.height >= geom.width { "portrait" } else { "landscape" },
        mt = twips(geom.margins.top),
        mr = twips(geom.margins.right),
        mb = twips(geom.margins.bottom),
        ml = twips(geom.margins.left),
    );
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\" \
xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
xmlns:wp=\"http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing\" \
xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\">\
<w:body>{blocks}{sect}</w:body></w:document>"
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
        BlockKind::Paragraph(p) => out.push_str(&docx_para(p, None, 0, ctx)),
        BlockKind::Heading(h) => out.push_str(&docx_heading(h, ctx)),
        BlockKind::List(list) => docx_list(list, ctx, out),
        BlockKind::Table(table) => out.push_str(&docx_table(table, ctx)),
        BlockKind::Image(img) => out.push_str(&docx_image_para(img, ctx)),
        BlockKind::Shape(shape) => out.push_str(&docx_shape_para(shape, ctx)),
        BlockKind::TextBox(tb) => docx_textbox(tb, ctx, out),
        BlockKind::Sheet(sheet) => docx_sheet(sheet, ctx, out),
        BlockKind::Slide(slides) => docx_slides(slides, ctx, out),
    }
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
        ppr.push_str(&format!("<w:pStyle w:val=\"{id}\"/>"));
    }
    if num_id > 0 {
        ppr.push_str(&format!(
            "<w:numPr><w:ilvl w:val=\"{num_level}\"/><w:numId w:val=\"{num_id}\"/></w:numPr>"
        ));
    }
    let ps = &para.style;
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
        ppr.push_str(&format!("<w:spacing{spacing}/>"));
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
        ppr.push_str(&format!("<w:ind{ind}/>"));
    }
    if let Some(jc) = docx_jc(ps.align) {
        ppr.push_str(&format!("<w:jc w:val=\"{jc}\"/>"));
    }
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
    let style = format!("Heading{level}");
    // A heading paragraph carries the style id; the outline level is implied by
    // the named style (defined in styles.xml).
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
                // Render link children as runs; an external URL gets a real
                // hyperlink field (`HYPERLINK "url"`), an internal page jump is
                // emitted as plain runs (the target anchor is not tracked here).
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
        if let Some(h) = row.height {
            rows.push_str(&format!(
                "<w:trPr><w:trHeight w:val=\"{}\" w:hRule=\"atLeast\"/></w:trPr>",
                twips(h)
            ));
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
    let png = ctx.resolve_image(img.resource).unwrap_or_default();
    if png.is_empty() {
        return String::new();
    }
    let id = ctx.next_obj();
    let rid = ctx.add_image(png);
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
    let geom = if shape_is_rect(&placed) {
        "<a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom>".to_string()
    } else {
        dml_cust_geom(&placed, placed.width, placed.height)
    };
    let sp_pr = format!(
        "<wps:spPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"{w}\" cy=\"{h}\"/></a:xfrm>{geom}{fill}{ln}</wps:spPr>",
        w = emu(placed.width.max(1.0)),
        h = emu(placed.height.max(1.0)),
        fill = dml_fill(&placed),
        ln = dml_line(&placed),
    );
    format!(
        "<w:p><w:r><w:drawing><wp:inline distT=\"0\" distB=\"0\" distL=\"0\" distR=\"0\">\
<wp:extent cx=\"{w}\" cy=\"{h}\"/><wp:effectExtent l=\"0\" t=\"0\" r=\"0\" b=\"0\"/>\
<wp:docPr id=\"{id}\" name=\"shape{id}\"/><wp:cNvGraphicFramePr/>\
<a:graphic xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\">\
<a:graphicData uri=\"http://schemas.microsoft.com/office/word/2010/wordprocessingShape\">\
<wps:wsp xmlns:wps=\"http://schemas.microsoft.com/office/word/2010/wordprocessingShape\">\
<wps:cNvSpPr/>{sp_pr}<wps:bodyPr/></wps:wsp></a:graphicData></a:graphic>\
</wp:inline></w:drawing></w:r></w:p>",
        w = emu(placed.width.max(1.0)),
        h = emu(placed.height.max(1.0)),
    )
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

fn docx_content_types(image_count: usize, num: bool, header: bool, footer: bool) -> String {
    let png = if image_count > 0 {
        "<Default Extension=\"png\" ContentType=\"image/png\"/>"
    } else {
        ""
    };
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
    if header {
        overrides.push_str(
            "<Override PartName=\"/word/header1.xml\" \
ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.header+xml\"/>",
        );
    }
    if footer {
        overrides.push_str(
            "<Override PartName=\"/word/footer1.xml\" \
ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.footer+xml\"/>",
        );
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
<Default Extension=\"xml\" ContentType=\"application/xml\"/>{png}{overrides}</Types>"
    )
}

fn docx_rels(image_count: usize, num: bool, header: bool, footer: bool) -> String {
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
    if header {
        s.push_str(
            "<Relationship Id=\"rIdHdr\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/header\" Target=\"header1.xml\"/>",
        );
    }
    if footer {
        s.push_str(
            "<Relationship Id=\"rIdFtr\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/footer\" Target=\"footer1.xml\"/>",
        );
    }
    for i in 0..image_count {
        s.push_str(&format!(
            "<Relationship Id=\"rId{id}\" \
Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/image\" \
Target=\"media/image{n}.png\"/>",
            id = 100 + i,
            n = i + 1
        ));
    }
    s.push_str("</Relationships>");
    s
}

/// Minimal styles.xml defining `Normal` + `Heading1`..`Heading6` with outline
/// levels, so heading paragraphs are recognised as such.
fn docx_styles_xml() -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<w:styles xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
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
    s.push_str("</w:styles>");
    s
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
        b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" \
Target=\"xl/workbook.xml\"/></Relationships>",
    );
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
    /// `RRGGBB` hex, or empty for the default (automatic/black).
    color: String,
}

/// A horizontal alignment + wrap pairing for an `<alignment>` child.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct XlsxAlign {
    /// `None` ⇒ general (no horizontal attribute emitted).
    horizontal: Option<Align>,
    wrap: bool,
}

impl XlsxAlign {
    fn is_default(self) -> bool {
        self.horizontal.is_none() && !self.wrap
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
            match &cell.value {
                CellValue::Empty => {
                    if s_idx != 0 {
                        // Keep a styled-but-empty cell so its fill/format shows.
                        cells.push_str(&format!("<c r=\"{r_ref}\"{s_attr}/>"));
                    }
                }
                CellValue::Number(n) => {
                    cells.push_str(&format!("<c r=\"{r_ref}\"{s_attr}><v>{}</v></c>", num(*n)));
                }
                CellValue::Bool(b) => {
                    cells.push_str(&format!(
                        "<c r=\"{r_ref}\"{s_attr} t=\"b\"><v>{}</v></c>",
                        if *b { 1 } else { 0 }
                    ));
                }
                CellValue::Text(t) => {
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
    let mut media: Vec<Vec<u8>> = Vec::new();
    let mut slide_xmls: Vec<String> = Vec::new();
    let mut slide_media: Vec<Vec<usize>> = Vec::new();
    for slide in &model_slides {
        let mut used = Vec::new();
        slide_xmls.push(pptx_slide_from_model(slide, doc, &mut media, &mut used));
        slide_media.push(used);
    }

    zip.add_deflated(
        "[Content_Types].xml",
        pptx_model_content_types(slide_xmls.len(), !media.is_empty()).as_bytes(),
    );
    zip.add_deflated(
        "_rels/.rels",
        b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" \
Target=\"ppt/presentation.xml\"/></Relationships>",
    );
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
        zip.add_deflated(
            &format!("ppt/slides/_rels/slide{}.xml.rels", i + 1),
            pptx_model_slide_rels(&slide_media[i]).as_bytes(),
        );
    }
    for (i, png) in media.iter().enumerate() {
        zip.add_deflated(&format!("ppt/media/image{}.png", i + 1), png);
    }
    zip.finish()
}

fn pptx_slide_from_model(
    slide: &Slide,
    doc: &Document,
    media: &mut Vec<Vec<u8>>,
    used: &mut Vec<usize>,
) -> String {
    let mut tree = String::new();
    let mut id = 2usize;

    // Placeholders first (title/body), positioned by their frame when present.
    for ph in &slide.placeholders {
        let (ph_type, ph_idx) = match &ph.role {
            PlaceholderRole::Title => ("title", String::new()),
            PlaceholderRole::Subtitle => ("subTitle", " idx=\"1\"".to_string()),
            PlaceholderRole::Body => ("body", " idx=\"1\"".to_string()),
            PlaceholderRole::Other(_) => ("body", " idx=\"1\"".to_string()),
        };
        let frame = pptx_xfrm(ph.block.frame, slide.geometry.width, slide.geometry.height);
        let body = pptx_text_body(&block_to_paras(&ph.block));
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
                if let Some(png) = doc_image(doc, img.resource) {
                    media.push(png);
                    used.push(media.len() - 1);
                    let rid = used.len();
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
            _ => {
                let frame = pptx_xfrm(sh.frame, slide.geometry.width, slide.geometry.height);
                let body = pptx_text_body(&block_to_paras(sh));
                tree.push_str(&format!(
                    "<p:sp><p:nvSpPr><p:cNvPr id=\"{id}\" name=\"t{id}\"/><p:cNvSpPr txBox=\"1\"/><p:nvPr/></p:nvSpPr>\
<p:spPr>{frame}<a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom><a:noFill/></p:spPr>{body}</p:sp>"
                ));
                id += 1;
            }
        }
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
fn pptx_text_body(paras: &[Paragraph]) -> String {
    let mut body = String::from("<p:txBody><a:bodyPr/><a:lstStyle/>");
    if paras.is_empty() {
        body.push_str("<a:p/>");
    }
    for p in paras {
        body.push_str(&pptx_paragraph(p));
    }
    body.push_str("</p:txBody>");
    body
}

fn pptx_paragraph(para: &Paragraph) -> String {
    let algn = match para.style.align {
        Align::Left => "",
        Align::Center => " algn=\"ctr\"",
        Align::Right => " algn=\"r\"",
        Align::Justify => " algn=\"just\"",
    };
    let mut runs = String::new();
    pptx_runs(&para.runs, &mut runs);
    if runs.is_empty() {
        format!("<a:p><a:pPr{algn}/></a:p>")
    } else {
        format!("<a:p><a:pPr{algn}/>{runs}</a:p>")
    }
}

fn pptx_runs(runs: &[Inline], out: &mut String) {
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
                    rpr = pptx_rpr(&run.style)
                ));
            }
            Inline::LineBreak => out.push_str("<a:br/>"),
            Inline::Image(_) => {}
            Inline::Link { children, .. } => pptx_runs(children, out),
        }
    }
}

fn pptx_rpr(style: &CharStyle) -> String {
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
    let mut inner = String::new();
    if let Some(c) = visible_color(style) {
        inner.push_str(&format!(
            "<a:solidFill><a:srgbClr val=\"{c}\"/></a:solidFill>"
        ));
    }
    if !style.family.is_empty() {
        let mut fam = String::new();
        esc(&style.family, &mut fam);
        inner.push_str(&format!("<a:latin typeface=\"{fam}\"/>"));
    }
    if inner.is_empty() {
        format!("<a:rPr {attrs}/>")
    } else {
        format!("<a:rPr {attrs}>{inner}</a:rPr>")
    }
}

fn pptx_model_content_types(slide_count: usize, has_media: bool) -> String {
    let png = if has_media {
        "<Default Extension=\"png\" ContentType=\"image/png\"/>"
    } else {
        ""
    };
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

fn pptx_model_slide_rels(media_indices: &[usize]) -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">",
    );
    for (local, &global) in media_indices.iter().enumerate() {
        s.push_str(&format!(
            "<Relationship Id=\"rId{}\" \
Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/image\" \
Target=\"../media/image{}.png\"/>",
            local + 1,
            global + 1
        ));
    }
    s.push_str("</Relationships>");
    s
}

// ════════════════════════════════════ ODF ════════════════════════════════════

/// Serialize a [`Document`] to a **flowing** OpenDocument Text (`.odt`): real
/// `<text:h>`/`<text:p>`, `<text:list>` for lists, `<table:table>` for tables.
pub fn odt_from_model(doc: &Document) -> Vec<u8> {
    let geom = doc.sections.first().map(|s| s.geometry).unwrap_or_default();
    let mut zip = ZipWriter::new();
    zip.add_stored("mimetype", b"application/vnd.oasis.opendocument.text");

    let mut ctx = OdfCtx::new(&doc.resources);
    let body = odt_body(doc, &mut ctx);

    // Render the first section's header/footer into their own auto-style buffer
    // so the markup AND its styles land in styles.xml's master page (ODF keeps
    // running header/footer styling separate from content.xml). Image counters
    // continue from the body's so any header/footer pictures get unique names.
    let mut hf_ctx = OdfCtx::new(&doc.resources);
    hf_ctx.image_base = ctx.images.len();
    let header_xml = doc
        .sections
        .first()
        .and_then(|s| s.header.as_ref())
        .map(|h| odt_blocks(h, &mut hf_ctx));
    let footer_xml = doc
        .sections
        .first()
        .and_then(|s| s.footer.as_ref())
        .map(|f| odt_blocks(f, &mut hf_ctx));

    zip.add_deflated(
        "content.xml",
        odt_model_content(&ctx.auto, &body).as_bytes(),
    );
    zip.add_deflated(
        "styles.xml",
        odf_text_styles_xml(
            geom.width,
            geom.height,
            geom.margins,
            &hf_ctx.auto,
            header_xml.as_deref(),
            footer_xml.as_deref(),
        )
        .as_bytes(),
    );
    let image_count = ctx.images.len() + hf_ctx.images.len();
    zip.add_deflated(
        "META-INF/manifest.xml",
        odf_manifest("text", image_count).as_bytes(),
    );
    for (i, png) in ctx.images.iter().enumerate() {
        zip.add_deflated(&format!("Pictures/img{}.png", i + 1), png);
    }
    for (i, png) in hf_ctx.images.iter().enumerate() {
        zip.add_deflated(
            &format!("Pictures/img{}.png", ctx.images.len() + i + 1),
            png,
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
        b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<office:document-styles xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" \
office:version=\"1.3\"></office:document-styles>",
    );
    zip.add_deflated(
        "META-INF/manifest.xml",
        odf_manifest("spreadsheet", 0).as_bytes(),
    );
    zip.finish()
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
    zip.add_deflated("styles.xml", odp_styles_xml_model(pw, ph).as_bytes());
    zip.add_deflated(
        "META-INF/manifest.xml",
        odf_manifest("presentation", ctx.images.len()).as_bytes(),
    );
    for (i, png) in ctx.images.iter().enumerate() {
        zip.add_deflated(&format!("Pictures/img{}.png", i + 1), png);
    }
    zip.finish()
}

/// Shared mutable state for ODT/ODP model builds: auto-styles, image list, and
/// the document's resource table for image-blob lookups.
struct OdfCtx<'a> {
    auto: String,
    images: Vec<Vec<u8>>,
    /// Offset added to this context's local image indices when forming
    /// `Pictures/imgN.png` names, so a secondary context (e.g. header/footer)
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
    fn resolve_image(&self, key: u64) -> Option<Vec<u8>> {
        self.resources.images.get(&key).map(|r| r.bytes.clone())
    }
}

fn odt_body(doc: &Document, ctx: &mut OdfCtx) -> String {
    // The first section's running header/footer become real `<style:header>`/
    // `<style:footer>` in the master page (styles.xml), not inlined body text.
    let mut body = String::new();
    for section in &doc.sections {
        for page in &section.pages {
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
        BlockKind::Shape(_) => {} // block shapes have no flow position in ODT text
        BlockKind::TextBox(tb) => out.push_str(&odt_blocks(&tb.blocks, ctx)),
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

/// One paragraph or heading. `tag` is `"p"` or `"h"`; `extra` adds attributes
/// (e.g. the outline level for a heading).
fn odt_paragraph(para: &Paragraph, tag: &str, extra: Option<&str>, ctx: &mut OdfCtx) -> String {
    let pid = ctx.next_style();
    let style_name = format!("P{pid}");
    ctx.auto.push_str(&odf_para_style(&style_name, para));
    let extra = extra.unwrap_or("");
    let mut runs = String::new();
    odt_runs(&para.runs, ctx, &mut runs);
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
            Inline::Image(_) => {}
            Inline::Link { href, children } => {
                if let LinkTarget::Url(url) = href {
                    if !url.is_empty() {
                        let mut u = String::new();
                        esc(url, &mut u);
                        out.push_str(&format!(
                            "<text:a xlink:type=\"simple\" xlink:href=\"{u}\">"
                        ));
                        odt_runs(children, ctx, out);
                        out.push_str("</text:a>");
                        continue;
                    }
                }
                odt_runs(children, ctx, out);
            }
        }
    }
}

fn odt_list(list: &List, ctx: &mut OdfCtx) -> String {
    let sid = ctx.next_style();
    let sname = format!("L{sid}");
    ctx.auto.push_str(&odf_list_style(&sname, list));
    let mut out = format!("<text:list text:style-name=\"{sname}\">");
    for item in &list.items {
        out.push_str("<text:list-item>");
        if item.blocks.is_empty() {
            out.push_str("<text:p/>");
        } else {
            for b in &item.blocks {
                odt_block(b, ctx, &mut out);
            }
        }
        out.push_str("</text:list-item>");
    }
    out.push_str("</text:list>");
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

    let mut rows = String::new();
    for row in &table.rows {
        rows.push_str("<table:table-row>");
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
            let cell_style = if let Some(shade) = cell.shading {
                let csid = ctx.next_style();
                let csn = format!("Tc{csid}");
                ctx.auto.push_str(&format!(
                    "<style:style style:name=\"{csn}\" style:family=\"table-cell\">\
<style:table-cell-properties fo:background-color=\"#{c}\"/></style:style>",
                    c = hex(shade),
                ));
                format!(" table:style-name=\"{csn}\"")
            } else {
                String::new()
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

    format!("<table:table table:style-name=\"{tname}\">{col_defs}{rows}</table:table>")
}

fn odt_image_para(img: &ImageRef, ctx: &mut OdfCtx) -> String {
    let png = ctx.resolve_image(img.resource).unwrap_or_default();
    if png.is_empty() {
        return "<text:p/>".to_string();
    }
    ctx.images.push(png);
    let n = ctx.image_base + ctx.images.len();
    // An inline image anchored as a character inside its own paragraph.
    format!(
        "<text:p><draw:frame draw:style-name=\"frInl\" text:anchor-type=\"as-char\" \
svg:width=\"96pt\" svg:height=\"96pt\">\
<draw:image xlink:href=\"Pictures/img{n}.png\" xlink:type=\"simple\" xlink:show=\"embed\" \
xlink:actuate=\"onLoad\"/></draw:frame></text:p>"
    )
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

/// The ODT `styles.xml`: a page layout (size + margins), the header/footer
/// automatic styles, and a master page that carries any `<style:header>`/
/// `<style:footer>`. `hf_auto` is the automatic-style markup the header/footer
/// paragraphs reference; `header`/`footer` are their rendered block XML.
fn odf_text_styles_xml(
    pw: f64,
    ph: f64,
    margins: crate::model::Margins,
    hf_auto: &str,
    header: Option<&str>,
    footer: Option<&str>,
) -> String {
    let orient = if ph >= pw { "portrait" } else { "landscape" };
    // Page margins (omit zero edges so a default geometry stays clean).
    let mut margin_attrs = String::new();
    if margins.top > 0.0 {
        margin_attrs.push_str(&format!(" fo:margin-top=\"{}pt\"", num(margins.top)));
    }
    if margins.bottom > 0.0 {
        margin_attrs.push_str(&format!(" fo:margin-bottom=\"{}pt\"", num(margins.bottom)));
    }
    if margins.left > 0.0 {
        margin_attrs.push_str(&format!(" fo:margin-left=\"{}pt\"", num(margins.left)));
    }
    if margins.right > 0.0 {
        margin_attrs.push_str(&format!(" fo:margin-right=\"{}pt\"", num(margins.right)));
    }
    // Header/footer images (rendered via the hf context) reference `frInl`, so
    // it must exist in this document's automatic styles too.
    let frame_style = if hf_auto.contains("frInl") {
        "<style:style style:name=\"frInl\" style:family=\"graphic\">\
<style:graphic-properties draw:fill=\"none\" draw:stroke=\"none\" style:vertical-pos=\"middle\" \
style:vertical-rel=\"text\"/></style:style>"
    } else {
        ""
    };
    let header_xml = match header {
        Some(h) => format!("<style:header>{h}</style:header>"),
        None => String::new(),
    };
    let footer_xml = match footer {
        Some(f) => format!("<style:footer>{f}</style:footer>"),
        None => String::new(),
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
<office:automatic-styles>{frame_style}{hf_auto}\
<style:page-layout style:name=\"pm1\">\
<style:page-layout-properties fo:page-width=\"{w}pt\" fo:page-height=\"{h}pt\" \
style:print-orientation=\"{o}\"{margin_attrs}/></style:page-layout></office:automatic-styles>\
<office:master-styles>\
<style:master-page style:name=\"Standard\" style:page-layout-name=\"pm1\">{header_xml}{footer_xml}</style:master-page>\
</office:master-styles></office:document-styles>",
        w = num(pw),
        h = num(ph),
        o = orient
    )
}

/// ODF `<style:style family="paragraph">` from a model paragraph style.
fn odf_para_style(name: &str, para: &Paragraph) -> String {
    let ps = &para.style;
    let mut props = String::from("<style:paragraph-properties");
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
    props.push_str("/>");
    format!("<style:style style:name=\"{name}\" style:family=\"paragraph\">{props}</style:style>")
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
    if let Some(c) = visible_color(style) {
        p.push_str(&format!(" fo:color=\"#{c}\""));
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

fn odf_manifest(kind: &str, image_count: usize) -> String {
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
<manifest:file-entry manifest:full-path=\"styles.xml\" manifest:media-type=\"text/xml\"/>"
    );
    for i in 0..image_count {
        s.push_str(&format!(
            "<manifest:file-entry manifest:full-path=\"Pictures/img{}.png\" manifest:media-type=\"image/png\"/>",
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
    font: Option<(String, bool, bool, u32, String)>,
    border: Option<(u32, String)>,
    align: Option<Align>,
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
            wrap: cell.wrap,
        };
        if key.fill.is_none()
            && key.data_style.is_none()
            && key.font.is_none()
            && key.border.is_none()
            && key.align.is_none()
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

        // table-cell-properties: fill, border, wrap.
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

        // text-properties: font family / weight / style / size / colour.
        let text_props = match &key.font {
            Some((family, bold, italic, size_centi, color)) => {
                ods_text_props(family, *bold, *italic, *size_centi, color)
            }
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
fn ods_font_key(style: &CharStyle) -> Option<(String, bool, bool, u32, String)> {
    let color = visible_color(style).unwrap_or_default();
    let has_size = style.size_pt > 0.0;
    if style.family.is_empty()
        && !style.bold
        && !style.italic
        && !has_size
        && color.is_empty()
    {
        return None;
    }
    Some((
        style.family.clone(),
        style.bold,
        style.italic,
        (run_size(style) * 100.0).round() as u32,
        color,
    ))
}

/// `<style:text-properties>` for a cell font (family/weight/style/size/colour).
fn ods_text_props(
    family: &str,
    bold: bool,
    italic: bool,
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
    let attrs = format!("{style_attr}{span_attr}");
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
    for slide in slides {
        body.push_str("<draw:page draw:style-name=\"dp1\" draw:master-page-name=\"Default\">");
        for ph in &slide.placeholders {
            odp_frame(&ph.block, slide, doc, ctx, &mut body);
        }
        for sh in &slide.shapes {
            odp_frame(sh, slide, doc, ctx, &mut body);
        }
        body.push_str("</draw:page>");
    }
    body
}

fn odp_frame(block: &Block, slide: &Slide, doc: &Document, ctx: &mut OdfCtx, out: &mut String) {
    let r = block.frame.unwrap_or(crate::model::Rect::new(
        0.0,
        0.0,
        slide.geometry.width,
        slide.geometry.height,
    ));
    match &block.kind {
        BlockKind::Image(img) => {
            let png = doc_image(doc, img.resource).unwrap_or_default();
            if png.is_empty() {
                return;
            }
            ctx.images.push(png);
            let n = ctx.images.len();
            out.push_str(&format!(
                "<draw:frame draw:style-name=\"frI\" draw:layer=\"layout\" \
svg:x=\"{x}pt\" svg:y=\"{y}pt\" svg:width=\"{w}pt\" svg:height=\"{h}pt\">\
<draw:image xlink:href=\"Pictures/img{n}.png\" xlink:type=\"simple\" xlink:show=\"embed\" \
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
                    "<draw:rect draw:style-name=\"{sname}\" draw:layer=\"layout\" \
svg:x=\"{x}pt\" svg:y=\"{y}pt\" svg:width=\"{w}pt\" svg:height=\"{h}pt\"/>",
                    x = num(r.x),
                    y = num(r.y),
                    w = num(r.w.max(1.0)),
                    h = num(r.h.max(1.0)),
                ));
            } else {
                out.push_str(&format!(
                    "<draw:path draw:style-name=\"{sname}\" draw:layer=\"layout\" \
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
        _ => {
            // Text frame: render the block's paragraphs into a text box.
            let sid = ctx.next_style();
            let sname = format!("Tx{sid}");
            ctx.auto.push_str(&format!(
                "<style:style style:name=\"{sname}\" style:family=\"graphic\">\
<style:graphic-properties draw:fill=\"none\" draw:stroke=\"none\" \
draw:auto-grow-width=\"false\" draw:auto-grow-height=\"false\" fo:padding=\"0pt\" \
draw:textarea-vertical-align=\"top\"/></style:style>"
            ));
            let mut content = String::new();
            for p in block_to_paras(block) {
                content.push_str(&odt_paragraph(&p, "p", None, ctx));
            }
            out.push_str(&format!(
                "<draw:frame draw:style-name=\"{sname}\" draw:layer=\"layout\" \
svg:x=\"{x}pt\" svg:y=\"{y}pt\" svg:width=\"{w}pt\" svg:height=\"{h}pt\"><draw:text-box>{content}</draw:text-box></draw:frame>",
                x = num(r.x),
                y = num(r.y),
                w = num(r.w.max(1.0)),
                h = num(r.h.max(1.0)),
            ));
        }
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

fn odp_styles_xml_model(pw: f64, ph: f64) -> String {
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
style:print-orientation=\"{o}\"/></style:page-layout></office:automatic-styles>\
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

/// Resolve an image blob by resource key from the document's resource table.
fn doc_image(doc: &Document, key: u64) -> Option<Vec<u8>> {
    doc.resources.images.get(&key).map(|r| r.bytes.clone())
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
            BlockKind::Shape(_) => {} // no Markdown representation for vector art
            BlockKind::TextBox(tb) => self.blocks(&tb.blocks, out),
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

    // Markdown has no portable colour; `style.color` is intentionally dropped.

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
        };
        let table = Block {
            kind: BlockKind::Table(Table {
                rows: vec![
                    Row {
                        cells: vec![span_cell],
                        height: None,
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
                    },
                    Row {
                        // Only the right cell; the left is covered by the row span.
                        cells: vec![Cell {
                            blocks: vec![para("r1c1")],
                            ..Default::default()
                        }],
                        height: None,
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
}
