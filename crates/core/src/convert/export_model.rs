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
    Align, Block, BlockKind, Cell, CharStyle, Document, Heading, ImageRef, Inline, LineHeight,
    LinkTarget, List, ListMarker, Paragraph, Row, Shape, Sheet, SheetBlock, SheetCell, Slide,
    SlideBlock, Table, TextBox,
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

    let has_header = doc
        .sections
        .first()
        .and_then(|s| s.header.as_ref())
        .is_some();
    let has_footer = doc
        .sections
        .first()
        .and_then(|s| s.footer.as_ref())
        .is_some();
    let has_num = ctx.list_count > 0;

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
            docx_numbering_xml(ctx.list_count).as_bytes(),
        );
    }
    if let Some(header) = doc.sections.first().and_then(|s| s.header.as_ref()) {
        let inner = docx_blocks(header, &mut ctx);
        zip.add_deflated(
            "word/header1.xml",
            docx_hdrftr_xml("hdr", &inner).as_bytes(),
        );
    }
    if let Some(footer) = doc.sections.first().and_then(|s| s.footer.as_ref()) {
        let inner = docx_blocks(footer, &mut ctx);
        zip.add_deflated(
            "word/footer1.xml",
            docx_hdrftr_xml("ftr", &inner).as_bytes(),
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
    /// Number of list *instances* emitted; also the next `w:numId`.
    list_count: usize,
    /// Next unique drawing/object id.
    obj_id: usize,
    resources: &'a crate::model::ResourceTable,
}

impl<'a> DocxCtx<'a> {
    fn new(resources: &'a crate::model::ResourceTable) -> Self {
        DocxCtx {
            images: Vec::new(),
            list_count: 0,
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
    ctx.list_count += 1;
    let num_id = ctx.list_count;
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

/// numbering.xml with one abstract+concrete numbering per list instance. Each
/// abstract num defines 9 levels alternating decimal so nested items number
/// independently; the concrete `w:num` ids are `1..=count` (matching the
/// `w:numId` written on paragraphs).
fn docx_numbering_xml(count: usize) -> String {
    let mut abstracts = String::new();
    let mut nums = String::new();
    for n in 1..=count {
        let mut lvls = String::new();
        for lvl in 0..9 {
            let indent = twips(18.0 + 18.0 * lvl as f64);
            lvls.push_str(&format!(
                "<w:lvl w:ilvl=\"{lvl}\"><w:start w:val=\"1\"/><w:numFmt w:val=\"decimal\"/>\
<w:lvlText w:val=\"%{one}.\"/><w:lvlJc w:val=\"left\"/>\
<w:pPr><w:ind w:left=\"{indent}\" w:hanging=\"360\"/></w:pPr></w:lvl>",
                one = lvl + 1,
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

/// Accumulates `cellXfs` style records (numFmt + fill) and returns stable indices.
struct XlsxStyler {
    /// Custom number-format codes → builtin/custom numFmtId (starts at 164).
    num_fmts: Vec<(u32, String)>,
    /// Fill colours (RRGGBB) → fill index (0 and 1 are reserved by spec).
    fills: Vec<String>,
    /// Style records: `(numFmtId, fillIndex)` → cellXfs index.
    xfs: Vec<(u32, usize)>,
}

impl XlsxStyler {
    fn new() -> Self {
        // cellXfs[0] is the default (no numFmt, fill 0).
        XlsxStyler {
            num_fmts: Vec::new(),
            fills: Vec::new(),
            xfs: vec![(0, 0)],
        }
    }

    /// Resolve a style index for an optional number format and fill colour.
    fn style_for(&mut self, number_format: Option<&str>, fill: Option<[f64; 3]>) -> usize {
        if number_format.is_none() && fill.is_none() {
            return 0;
        }
        let num_fmt_id = match number_format {
            None => 0,
            Some(code) => {
                let existing = self
                    .num_fmts
                    .iter()
                    .find(|(_, c)| c == code)
                    .map(|(id, _)| *id);
                existing.unwrap_or_else(|| {
                    let id = 164 + self.num_fmts.len() as u32;
                    self.num_fmts.push((id, code.to_string()));
                    id
                })
            }
        };
        let fill_idx = match fill {
            None => 0,
            Some(rgb) => {
                let hexc = hex(rgb);
                let pos = self.fills.iter().position(|f| *f == hexc);
                let local = pos.unwrap_or_else(|| {
                    self.fills.push(hexc);
                    self.fills.len() - 1
                });
                // Built-in fills 0 (none) and 1 (gray125) precede custom fills.
                local + 2
            }
        };
        if let Some(i) = self
            .xfs
            .iter()
            .position(|&(n, f)| n == num_fmt_id && f == fill_idx)
        {
            return i;
        }
        self.xfs.push((num_fmt_id, fill_idx));
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

        let mut xfs = String::new();
        for &(num_fmt_id, fill_idx) in &self.xfs {
            let apply_num = if num_fmt_id != 0 {
                " applyNumberFormat=\"1\""
            } else {
                ""
            };
            let apply_fill = if fill_idx != 0 {
                " applyFill=\"1\""
            } else {
                ""
            };
            xfs.push_str(&format!(
                "<xf numFmtId=\"{num_fmt_id}\" fontId=\"0\" fillId=\"{fill_idx}\" borderId=\"0\" xfId=\"0\"{apply_num}{apply_fill}/>"
            ));
        }

        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<styleSheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">\
{num_fmts}\
<fonts count=\"1\"><font><sz val=\"11\"/><name val=\"Calibri\"/></font></fonts>\
<fills count=\"{fill_count}\">{fills}</fills>\
<borders count=\"1\"><border><left/><right/><top/><bottom/><diagonal/></border></borders>\
<cellStyleXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\"/></cellStyleXfs>\
<cellXfs count=\"{xf_count}\">{xfs}</cellXfs>\
<cellStyles count=\"1\"><cellStyle name=\"Normal\" xfId=\"0\" builtinId=\"0\"/></cellStyles>\
</styleSheet>",
            xf_count = self.xfs.len(),
        )
    }
}

fn xlsx_sheet_from_model(sheet: &Sheet, styler: &mut XlsxStyler) -> String {
    let mut data = String::new();
    for (r, row) in sheet.rows.iter().enumerate() {
        let mut cells = String::new();
        for (c, cell) in row.cells.iter().enumerate() {
            let s_idx = styler.style_for(cell.number_format.as_deref(), cell.fill);
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
        if !cells.is_empty() {
            data.push_str(&format!("<row r=\"{}\">{cells}</row>", r + 1));
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

    zip.add_deflated(
        "content.xml",
        odt_model_content(&ctx.auto, &body).as_bytes(),
    );
    zip.add_deflated(
        "styles.xml",
        odf_text_styles_xml(geom.width, geom.height).as_bytes(),
    );
    zip.add_deflated(
        "META-INF/manifest.xml",
        odf_manifest("text", ctx.images.len()).as_bytes(),
    );
    for (i, png) in ctx.images.iter().enumerate() {
        zip.add_deflated(&format!("Pictures/img{}.png", i + 1), png);
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
    style_id: usize,
    resources: &'a crate::model::ResourceTable,
}

impl<'a> OdfCtx<'a> {
    fn new(resources: &'a crate::model::ResourceTable) -> Self {
        OdfCtx {
            auto: String::new(),
            images: Vec::new(),
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
    let mut body = String::new();
    // Header/footer flow at the top of the body (ODF running headers live in
    // styles.xml; for reflowable text we prepend them as ordinary paragraphs).
    if let Some(header) = doc.sections.first().and_then(|s| s.header.as_ref()) {
        body.push_str(&odt_blocks(header, ctx));
    }
    for section in &doc.sections {
        for page in &section.pages {
            body.push_str(&odt_blocks(&page.blocks, ctx));
        }
    }
    if let Some(footer) = doc.sections.first().and_then(|s| s.footer.as_ref()) {
        body.push_str(&odt_blocks(footer, ctx));
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
    let n = ctx.images.len();
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

fn odf_text_styles_xml(pw: f64, ph: f64) -> String {
    let orient = if ph >= pw { "portrait" } else { "landscape" };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<office:document-styles xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" \
xmlns:style=\"urn:oasis:names:tc:opendocument:xmlns:style:1.0\" \
xmlns:fo=\"urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0\" office:version=\"1.3\">\
<office:automatic-styles>\
<style:page-layout style:name=\"pm1\">\
<style:page-layout-properties fo:page-width=\"{w}pt\" fo:page-height=\"{h}pt\" \
style:print-orientation=\"{o}\"/></style:page-layout></office:automatic-styles>\
<office:master-styles>\
<style:master-page style:name=\"Standard\" style:page-layout-name=\"pm1\"/>\
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

/// A cell-style cache key: its fill colour (hex) and data-style name, each
/// optional, identifying a unique `(fill, number format)` pairing.
type CellStyleKey = (Option<String>, Option<String>);

/// Accumulates ODS automatic styles: number-format data styles (`<number:*>`),
/// and the table-cell styles that bind a fill colour and/or a data style. Cells
/// reference a cell style via `table:style-name`; identical (fill, format) pairs
/// share one style.
#[derive(Default)]
struct OdsStyler {
    /// `<number:number-style>` definitions, keyed by their ODF format code.
    data_styles: String,
    /// `<style:style family="table-cell">` definitions + column styles.
    cell_styles: String,
    /// Format code → data-style name.
    fmts: Vec<(String, String)>,
    /// (fill hex option, data-style name option) → cell-style name.
    cells: Vec<(CellStyleKey, String)>,
    next_col: usize,
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

    /// The `table:style-name` value for a cell's fill + number format (or empty
    /// when neither applies).
    fn cell_style(&mut self, fill: Option<[f64; 3]>, number_format: Option<&str>) -> String {
        if fill.is_none() && number_format.is_none() {
            return String::new();
        }
        let fill_hex = fill.map(hex);
        let data_name = number_format.map(|c| self.data_style(c));
        let key = (fill_hex.clone(), data_name.clone());
        if let Some((_, name)) = self.cells.iter().find(|(k, _)| *k == key) {
            return format!(" table:style-name=\"{name}\"");
        }
        let name = format!("ce{}", self.cells.len());
        let data_attr = match &data_name {
            Some(d) => format!(" style:data-style-name=\"{d}\""),
            None => String::new(),
        };
        let props = match &fill_hex {
            Some(c) => format!("<style:table-cell-properties fo:background-color=\"#{c}\"/>"),
            None => String::new(),
        };
        self.cell_styles.push_str(&format!(
            "<style:style style:name=\"{name}\" style:family=\"table-cell\"{data_attr}>{props}</style:style>"
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

fn ods_sheet_from_model(sheet: &Sheet, styler: &mut OdsStyler) -> String {
    let mut nm = String::new();
    esc(&sheet.name, &mut nm);
    let mut rows = String::new();
    for row in &sheet.rows {
        rows.push_str("<table:table-row>");
        for cell in &row.cells {
            rows.push_str(&ods_cell_from_model(cell, styler));
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

fn ods_cell_from_model(cell: &SheetCell, styler: &mut OdsStyler) -> String {
    let style_attr = styler.cell_style(cell.fill, cell.number_format.as_deref());
    match &cell.value {
        CellValue::Empty => format!("<table:table-cell{style_attr}/>"),
        CellValue::Number(n) => format!(
            "<table:table-cell{style_attr} office:value-type=\"float\" office:value=\"{v}\"><text:p>{disp}</text:p></table:table-cell>",
            v = num(*n),
            disp = num(*n),
        ),
        CellValue::Bool(b) => format!(
            "<table:table-cell{style_attr} office:value-type=\"boolean\" office:boolean-value=\"{bv}\"><text:p>{disp}</text:p></table:table-cell>",
            bv = if *b { "true" } else { "false" },
            disp = if *b { "TRUE" } else { "FALSE" },
        ),
        CellValue::Text(t) => {
            let mut esc_t = String::new();
            esc(t, &mut esc_t);
            format!(
                "<table:table-cell{style_attr} office:value-type=\"string\"><text:p>{esc_t}</text:p></table:table-cell>"
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
                                    },
                                    SheetRow {
                                        cells: vec![SheetCell {
                                            value: CellValue::Number(7.0),
                                            fill: Some([1.0, 1.0, 0.0]),
                                            ..Default::default()
                                        }],
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
}
