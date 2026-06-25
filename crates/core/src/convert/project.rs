//! Compat bridge: the unified [`Document`](crate::model::Document) model lowered
//! onto the flat [`ConvPage`] IR every existing exporter already consumes.
//!
//! This is the **fallback** path for model epic #74 Phase 1: it guarantees that
//! the moment the model exists, *every* `ConvPage`-based exporter (DOCX, ODT,
//! PPTX, XLSX, HTML, and the [`PdfBuilder`](super::build::PdfBuilder) PDF path)
//! works on it with zero regression. Full-fidelity, structure-aware exporters
//! land later; here the goal is **correct but simple**.
//!
//! ## Mapping
//!
//! Coordinates are **top-down points, origin top-left** — the same convention
//! every `ConvPage` consumer expects (the PDF→top-down flip is done at the model
//! boundary, not here).
//!
//! - **Absolute blocks** ([`Block::frame`] = `Some`): placed at their frame.
//!   `TextBox`/`Paragraph`/`Heading` → [`PlacedText`], `Image` → [`PlacedImage`],
//!   `Shape` → [`PlacedShape`].
//! - **Flow blocks** (`frame = None`): laid out top-to-bottom inside the
//!   [`Section`](crate::model::Section) geometry minus its margins, advancing the
//!   pen by each line's height (`size_pt × line-height`), paginating onto fresh
//!   pages when the column is full.
//! - **[`Sheet`](crate::model::Sheet) block** → one [`ConvPage`] per sheet, cell
//!   text on a column grid keyed by the sheet's `col_widths`.
//! - **[`Slide`](crate::model::Slide) block** → one [`ConvPage`] per slide, its
//!   shapes/placeholders placed at their frames.

use crate::content::vector::PathSeg;
use crate::convert::style::{Generic, TextStyle};
use crate::convert::{ConvPage, PlacedImage, PlacedShape, PlacedText};
use crate::model::{
    Block, BlockKind, Blockquote, CharStyle, Document, Heading, ImageRef, Inline, LineHeight, List,
    ListMarker, PageGeometry, Paragraph, Rect, Shape, Sheet, Slide, Table, TextBox,
};

/// Default column width (points) for grid cells whose sheet omits a width.
const DEFAULT_COL_WIDTH: f64 = 72.0;
/// Minimum line advance (points) so empty/zero-size runs still progress.
const MIN_LINE_HEIGHT: f64 = 4.0;

/// Map a model [`CharStyle`] to the exporters' [`TextStyle`] (family, generic
/// class, bold/italic, colour). Size is carried separately on [`PlacedText`].
fn text_style(cs: &CharStyle) -> TextStyle {
    let family = if cs.family.trim().is_empty() {
        match cs.generic {
            Generic::Serif => "Times New Roman",
            Generic::Mono => "Courier New",
            Generic::Sans => "Helvetica",
        }
        .to_string()
    } else {
        cs.family.clone()
    };
    TextStyle {
        family,
        generic: cs.generic,
        bold: cs.bold,
        italic: cs.italic,
        color: cs.color,
        background: cs.background,
    }
}

/// Concatenate a paragraph's inline runs into a single plain-text string.
/// Line breaks become spaces, links contribute their children's text — enough
/// for the compat fallback (full inline fidelity is a later structured pass).
fn paragraph_text(para: &Paragraph) -> String {
    let mut out = String::new();
    collect_inline_text(&para.runs, &mut out);
    out
}

fn collect_inline_text(runs: &[Inline], out: &mut String) {
    for run in runs {
        match run {
            Inline::Run(r) => out.push_str(&r.text),
            Inline::LineBreak => out.push(' '),
            Inline::Image(_) => {}
            Inline::Link { children, .. } => collect_inline_text(children, out),
            Inline::CommentRef { .. } => {}
        }
    }
}

/// The dominant character style of a paragraph: the first text run's style, or a
/// default when the paragraph holds no styled run.
fn paragraph_char_style(para: &Paragraph) -> CharStyle {
    fn first(runs: &[Inline]) -> Option<&CharStyle> {
        for run in runs {
            match run {
                Inline::Run(r) => return Some(&r.style),
                Inline::Link { children, .. } => {
                    if let Some(s) = first(children) {
                        return Some(s);
                    }
                }
                _ => {}
            }
        }
        None
    }
    first(&para.runs).cloned().unwrap_or_default()
}

/// Resolve the effective font size for a char style, falling back to a sane
/// body size when the model leaves it at `0.0`.
fn effective_size(cs: &CharStyle) -> f64 {
    if cs.size_pt > 0.5 {
        cs.size_pt
    } else {
        11.0
    }
}

/// Line advance (points) for a char style: `size × line-height multiple`.
fn line_advance(cs: &CharStyle, line_height: LineHeight) -> f64 {
    let size = effective_size(cs);
    let h = match line_height {
        LineHeight::Normal => size * 1.2,
        LineHeight::Multiple(m) => size * m.max(0.1),
        LineHeight::Points(p) => p.max(MIN_LINE_HEIGHT),
    };
    h.max(MIN_LINE_HEIGHT)
}

/// The ordinal/bullet prefix for the `index`-th (zero-based) item of a list.
fn list_prefix(list: &List, index: usize) -> String {
    let n = index + 1;
    match list.marker {
        ListMarker::Bullet(c) => format!("{c} "),
        ListMarker::Decimal => format!("{n}. "),
        ListMarker::LowerAlpha => format!("{}. ", alpha(n, false)),
        ListMarker::UpperAlpha => format!("{}. ", alpha(n, true)),
        ListMarker::LowerRoman => format!("{}. ", roman(n).to_lowercase()),
        ListMarker::UpperRoman => format!("{}. ", roman(n)),
    }
}

/// Spreadsheet-style letters for `n` (1→A, 26→Z, 27→AA), upper or lower case.
fn alpha(mut n: usize, upper: bool) -> String {
    let base = if upper { b'A' } else { b'a' };
    let mut buf = Vec::new();
    while n > 0 {
        n -= 1;
        buf.push(base + (n % 26) as u8);
        n /= 26;
    }
    buf.reverse();
    String::from_utf8(buf).unwrap_or_default()
}

/// Uppercase Roman numerals for `1..=3999` (else decimal as a graceful cap).
fn roman(mut n: usize) -> String {
    const TABLE: [(usize, &str); 13] = [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    if !(1..=3999).contains(&n) {
        return n.to_string();
    }
    let mut out = String::new();
    for (v, s) in TABLE {
        while n >= v {
            out.push_str(s);
            n -= v;
        }
    }
    out
}

/// A top-to-bottom flow layouter over one section's content column. Emits
/// [`PlacedText`] lines and paginates onto fresh [`ConvPage`]s when the column
/// fills. Absolute blocks bypass the pen and are placed at their frame.
struct Flow {
    geometry: PageGeometry,
    /// Completed pages plus the page currently being filled (last element).
    pages: Vec<ConvPage>,
    /// Pen position (top-down points) on the current page.
    y: f64,
    /// Left edge of the content column.
    left: f64,
    /// Bottom limit of the content column (pen must stay above this).
    bottom: f64,
}

impl Flow {
    fn new(geometry: PageGeometry) -> Self {
        let m = geometry.margins;
        let mut flow = Flow {
            geometry,
            pages: Vec::new(),
            y: m.top,
            left: m.left,
            bottom: geometry.height - m.bottom,
        };
        flow.push_page();
        flow
    }

    fn push_page(&mut self) {
        self.pages.push(ConvPage {
            width: self.geometry.width,
            height: self.geometry.height,
            ..ConvPage::default()
        });
        self.y = self.geometry.margins.top;
    }

    /// Index of the page currently being filled.
    fn cur(&self) -> usize {
        self.pages.len() - 1
    }

    /// Ensure a line of `advance` points fits; start a new page if not.
    fn ensure_room(&mut self, advance: f64) {
        if self.y + advance > self.bottom && self.y > self.geometry.margins.top {
            self.push_page();
        }
    }

    /// Place one flowed text line at the pen and advance.
    fn emit_line(&mut self, indent: f64, text: &str, cs: &CharStyle, line_height: LineHeight) {
        let advance = line_advance(cs, line_height);
        self.ensure_room(advance);
        let page = self.cur();
        let size = effective_size(cs);
        let x = self.left + indent;
        let width = (self.bottom_right() - x).max(0.0);
        let y = self.y;
        if !text.is_empty() {
            self.pages[page].texts.push(PlacedText {
                text: text.to_string(),
                x,
                y,
                width,
                height: size,
                style: text_style(cs),
            });
        }
        self.y += advance;
    }

    /// Right edge of the content column.
    fn bottom_right(&self) -> f64 {
        self.geometry.width - self.geometry.margins.right
    }

    /// Lay out a list of flow/absolute blocks at the given extra left `indent`.
    fn layout_blocks(&mut self, blocks: &[Block], indent: f64) {
        for block in blocks {
            self.layout_block(block, indent);
        }
    }

    fn layout_block(&mut self, block: &Block, indent: f64) {
        if let Some(frame) = block.frame {
            self.place_absolute(block, frame);
            return;
        }
        match &block.kind {
            BlockKind::Paragraph(p) => self.flow_paragraph(p, indent),
            BlockKind::Heading(h) => self.flow_heading(h, indent),
            BlockKind::List(l) => self.flow_list(l, indent),
            BlockKind::Table(t) => self.flow_table(t, indent),
            BlockKind::TextBox(tb) => self.layout_blocks(&tb.blocks, indent),
            BlockKind::CodeBlock(cb) => self.flow_code(cb, indent),
            BlockKind::Blockquote(bq) => {
                // A quote flows its blocks at a deeper left indent.
                self.layout_blocks(&bq.blocks, indent + 18.0);
            }
            BlockKind::HorizontalRule => self.flow_rule(indent),
            BlockKind::Image(_) | BlockKind::Shape(_) => {
                // No frame ⇒ no geometry to flow a raster/vector into; skipped in
                // the compat fallback (kept by the structured exporters later).
            }
            BlockKind::Sheet(_) | BlockKind::Slide(_) => {
                // Sheets/slides are page-replacing and handled at the document
                // level (one ConvPage per sheet/slide), never inside a flow.
            }
        }
    }

    /// Flow a code block: each source line emitted verbatim in a monospace style.
    fn flow_code(&mut self, code: &crate::model::CodeBlock, indent: f64) {
        let cs = CharStyle {
            generic: Generic::Mono,
            size_pt: 10.0,
            ..CharStyle::default()
        };
        for line in code.code.split('\n') {
            self.emit_line(indent, line, &cs, LineHeight::Normal);
        }
    }

    /// Flow a horizontal rule: a thin full-width stroke at the current pen.
    fn flow_rule(&mut self, indent: f64) {
        self.ensure_room(MIN_LINE_HEIGHT);
        let page = self.cur();
        let x = self.left + indent;
        let w = (self.bottom_right() - x).max(1.0);
        let y = self.y + MIN_LINE_HEIGHT / 2.0;
        self.pages[page].shapes.push(PlacedShape {
            x,
            y,
            width: w,
            height: 0.0,
            segments: vec![PathSeg::Move(x, y), PathSeg::Line(x + w, y)],
            fill: None,
            stroke: Some([0.6, 0.6, 0.6]),
            stroke_width: 1.0,
            fill_alpha: 1.0,
            stroke_alpha: 1.0,
            dash: Vec::new(),
        });
        self.y += MIN_LINE_HEIGHT;
    }

    fn flow_paragraph(&mut self, para: &Paragraph, indent: f64) {
        let cs = paragraph_char_style(para);
        let text = paragraph_text(para);
        self.y += para.style.space_before_pt.max(0.0);
        let total_indent = indent + para.style.indent_left_pt.max(0.0);
        self.emit_line(total_indent, &text, &cs, para.style.line_height);
        self.y += para.style.space_after_pt.max(0.0);
    }

    fn flow_heading(&mut self, heading: &Heading, indent: f64) {
        // A heading is a styled paragraph; flow it the same way.
        self.flow_paragraph(&heading.para, indent);
    }

    fn flow_list(&mut self, list: &List, indent: f64) {
        for (i, item) in list.items.iter().enumerate() {
            let prefix = list_prefix(list, i);
            let item_indent = indent + f64::from(item.level) * 18.0;
            // Render the first paragraph prefixed with the bullet/ordinal; any
            // remaining blocks of the item flow underneath at the item indent.
            let mut blocks = item.blocks.iter();
            if let Some(first) = blocks.next() {
                match &first.kind {
                    BlockKind::Paragraph(p) if first.frame.is_none() => {
                        let cs = paragraph_char_style(p);
                        let text = format!("{prefix}{}", paragraph_text(p));
                        self.y += p.style.space_before_pt.max(0.0);
                        self.emit_line(item_indent, &text, &cs, p.style.line_height);
                        self.y += p.style.space_after_pt.max(0.0);
                    }
                    _ => self.layout_block(first, item_indent),
                }
            } else {
                // Empty item: still emit the marker so the structure is visible.
                self.emit_line(
                    item_indent,
                    prefix.trim_end(),
                    &CharStyle::default(),
                    LineHeight::Normal,
                );
            }
            for block in blocks {
                self.layout_block(block, item_indent);
            }
        }
    }

    fn flow_table(&mut self, table: &Table, indent: f64) {
        let offsets = column_offsets(&table.col_widths, indent);
        for row in &table.rows {
            let mut max_advance = MIN_LINE_HEIGHT;
            // Pre-compute the row advance from the tallest cell's first run.
            for cell in &row.cells {
                if let Some(cs) = first_cell_style(&cell.blocks) {
                    max_advance = max_advance.max(line_advance(&cs, LineHeight::Normal));
                }
            }
            self.ensure_room(max_advance);
            let page = self.cur();
            let y = self.y;
            for (ci, cell) in row.cells.iter().enumerate() {
                let text = cell_text(&cell.blocks);
                if text.is_empty() {
                    continue;
                }
                let cs = first_cell_style(&cell.blocks).unwrap_or_default();
                let x = self.left + offsets.get(ci).copied().unwrap_or(indent);
                let width = column_width(&table.col_widths, ci);
                self.pages[page].texts.push(PlacedText {
                    text,
                    x,
                    y,
                    width,
                    height: effective_size(&cs),
                    style: text_style(&cs),
                });
            }
            // Advance even for an all-empty row so it occupies grid space.
            self.y += row.height.unwrap_or(max_advance).max(MIN_LINE_HEIGHT);
        }
    }

    /// Place an absolutely-positioned block at `frame` (top-down points).
    fn place_absolute(&mut self, block: &Block, frame: Rect) {
        let page = self.cur();
        match &block.kind {
            BlockKind::Image(img) => place_image(&mut self.pages[page], img, frame),
            BlockKind::Shape(shape) => place_shape(&mut self.pages[page], shape, frame),
            BlockKind::Paragraph(p) => place_text_block(&mut self.pages[page], &p_text(p), frame),
            BlockKind::Heading(h) => {
                place_text_block(&mut self.pages[page], &p_text(&h.para), frame)
            }
            BlockKind::TextBox(tb) => place_text_box(&mut self.pages[page], tb, frame),
            BlockKind::List(l) => place_list_box(&mut self.pages[page], l, frame),
            BlockKind::Table(t) => place_table_box(&mut self.pages[page], t, frame),
            BlockKind::CodeBlock(cb) => place_code_box(&mut self.pages[page], cb, frame),
            BlockKind::Blockquote(bq) => place_quote_box(&mut self.pages[page], bq, frame),
            BlockKind::HorizontalRule => place_rule(&mut self.pages[page], frame),
            BlockKind::Sheet(_) | BlockKind::Slide(_) => {
                // Page-replacing kinds are never nested as absolute boxes.
            }
        }
    }
}

/// (text, char-style) of a paragraph, bundled for absolute placement.
struct ParaText {
    text: String,
    style: CharStyle,
}

fn p_text(para: &Paragraph) -> ParaText {
    ParaText {
        text: paragraph_text(para),
        style: paragraph_char_style(para),
    }
}

/// First text run's style found within a list of blocks (depth-first).
fn first_cell_style(blocks: &[Block]) -> Option<CharStyle> {
    for block in blocks {
        match &block.kind {
            BlockKind::Paragraph(p) => {
                let cs = paragraph_char_style(p);
                return Some(cs);
            }
            BlockKind::Heading(h) => return Some(paragraph_char_style(&h.para)),
            BlockKind::TextBox(tb) => {
                if let Some(cs) = first_cell_style(&tb.blocks) {
                    return Some(cs);
                }
            }
            _ => {}
        }
    }
    None
}

/// Flatten a cell's block content into a single line of text.
fn cell_text(blocks: &[Block]) -> String {
    let mut parts = Vec::new();
    for block in blocks {
        match &block.kind {
            BlockKind::Paragraph(p) => parts.push(paragraph_text(p)),
            BlockKind::Heading(h) => parts.push(paragraph_text(&h.para)),
            BlockKind::TextBox(tb) => parts.push(cell_text(&tb.blocks)),
            BlockKind::List(l) => {
                for (i, item) in l.items.iter().enumerate() {
                    parts.push(format!("{}{}", list_prefix(l, i), cell_text(&item.blocks)));
                }
            }
            _ => {}
        }
    }
    parts.retain(|s| !s.is_empty());
    parts.join(" ")
}

/// Left offset (relative to the column start) of each table column.
fn column_offsets(col_widths: &[f64], start: f64) -> Vec<f64> {
    let mut offsets = Vec::with_capacity(col_widths.len());
    let mut acc = start;
    for w in col_widths {
        offsets.push(acc);
        acc += if *w > 0.0 { *w } else { DEFAULT_COL_WIDTH };
    }
    offsets
}

/// Width of column `index`, defaulting when unspecified.
fn column_width(col_widths: &[f64], index: usize) -> f64 {
    match col_widths.get(index) {
        Some(w) if *w > 0.0 => *w,
        _ => DEFAULT_COL_WIDTH,
    }
}

/// Place an image XObject reference at `frame`. Resolution of the actual PNG
/// bytes happens through the [`ResourceTable`](crate::model::ResourceTable) by
/// the caller; here the bridge only carries the geometry plus an empty payload
/// when no bytes are attached (the structured exporter re-embeds the real blob).
fn place_image(page: &mut ConvPage, _img: &ImageRef, frame: Rect) {
    // The compat fallback has no access to the resource table at this layer, so
    // it records the placement box with an empty PNG; exporters that need the
    // bytes resolve them from the model. Skipped entirely when zero-sized.
    if frame.w <= 0.0 || frame.h <= 0.0 {
        return;
    }
    page.images.push(PlacedImage {
        png: Vec::new(),
        x: frame.x,
        y: frame.y,
        width: frame.w,
        height: frame.h,
    });
}

/// Place a vector shape at `frame`, translating its path into top-down points
/// offset by the frame origin.
fn place_shape(page: &mut ConvPage, shape: &Shape, frame: Rect) {
    let segments: Vec<PathSeg> = shape
        .segments
        .iter()
        .map(|seg| translate_seg(*seg, frame.x, frame.y))
        .collect();
    page.shapes.push(PlacedShape {
        x: frame.x,
        y: frame.y,
        width: frame.w,
        height: frame.h,
        segments,
        fill: shape.fill,
        stroke: shape.stroke,
        stroke_width: shape.stroke_width,
        fill_alpha: 1.0,
        stroke_alpha: 1.0,
        dash: shape.dash.clone(),
    });
}

/// Offset a path segment by `(dx, dy)` (segments are stored frame-relative).
fn translate_seg(seg: PathSeg, dx: f64, dy: f64) -> PathSeg {
    match seg {
        PathSeg::Move(x, y) => PathSeg::Move(x + dx, y + dy),
        PathSeg::Line(x, y) => PathSeg::Line(x + dx, y + dy),
        PathSeg::Cubic(x1, y1, x2, y2, x3, y3) => {
            PathSeg::Cubic(x1 + dx, y1 + dy, x2 + dx, y2 + dy, x3 + dx, y3 + dy)
        }
        PathSeg::Close => PathSeg::Close,
    }
}

/// Place a single paragraph/heading text run anchored at `frame`'s top-left.
fn place_text_block(page: &mut ConvPage, pt: &ParaText, frame: Rect) {
    if pt.text.is_empty() {
        return;
    }
    page.texts.push(PlacedText {
        text: pt.text.clone(),
        x: frame.x,
        y: frame.y,
        width: frame.w,
        height: effective_size(&pt.style),
        style: text_style(&pt.style),
    });
}

/// Place a text box's blocks stacked from `frame`'s top-left, advancing per line.
fn place_text_box(page: &mut ConvPage, tb: &TextBox, frame: Rect) {
    let mut y = frame.y;
    place_blocks_at(page, &tb.blocks, frame.x, &mut y, frame.w, 0.0);
}

/// Place a list at `frame`, one line per item with its bullet/ordinal prefix.
fn place_list_box(page: &mut ConvPage, list: &List, frame: Rect) {
    let mut y = frame.y;
    for (i, item) in list.items.iter().enumerate() {
        let prefix = list_prefix(list, i);
        let indent = f64::from(item.level) * 18.0;
        let text = format!("{prefix}{}", cell_text(&item.blocks));
        let cs = first_cell_style(&item.blocks).unwrap_or_default();
        if !text.is_empty() {
            page.texts.push(PlacedText {
                text,
                x: frame.x + indent,
                y,
                width: (frame.w - indent).max(0.0),
                height: effective_size(&cs),
                style: text_style(&cs),
            });
        }
        y += line_advance(&cs, LineHeight::Normal);
    }
}

/// Place a table at `frame` as a grid of cell text.
fn place_table_box(page: &mut ConvPage, table: &Table, frame: Rect) {
    let offsets = column_offsets(&table.col_widths, 0.0);
    let mut y = frame.y;
    for row in &table.rows {
        let mut advance = MIN_LINE_HEIGHT;
        for cell in &row.cells {
            if let Some(cs) = first_cell_style(&cell.blocks) {
                advance = advance.max(line_advance(&cs, LineHeight::Normal));
            }
        }
        for (ci, cell) in row.cells.iter().enumerate() {
            let text = cell_text(&cell.blocks);
            if text.is_empty() {
                continue;
            }
            let cs = first_cell_style(&cell.blocks).unwrap_or_default();
            page.texts.push(PlacedText {
                text,
                x: frame.x + offsets.get(ci).copied().unwrap_or(0.0),
                y,
                width: column_width(&table.col_widths, ci),
                height: effective_size(&cs),
                style: text_style(&cs),
            });
        }
        y += row.height.unwrap_or(advance).max(MIN_LINE_HEIGHT);
    }
}

/// Stack a list of text-bearing blocks downward from `(x, y)`, advancing `y`.
fn place_blocks_at(
    page: &mut ConvPage,
    blocks: &[Block],
    x: f64,
    y: &mut f64,
    width: f64,
    indent: f64,
) {
    for block in blocks {
        match &block.kind {
            BlockKind::Paragraph(p) => {
                let cs = paragraph_char_style(p);
                let text = paragraph_text(p);
                if !text.is_empty() {
                    page.texts.push(PlacedText {
                        text,
                        x: x + indent,
                        y: *y,
                        width: (width - indent).max(0.0),
                        height: effective_size(&cs),
                        style: text_style(&cs),
                    });
                }
                *y += line_advance(&cs, p.style.line_height);
            }
            BlockKind::Heading(h) => {
                let cs = paragraph_char_style(&h.para);
                let text = paragraph_text(&h.para);
                if !text.is_empty() {
                    page.texts.push(PlacedText {
                        text,
                        x: x + indent,
                        y: *y,
                        width: (width - indent).max(0.0),
                        height: effective_size(&cs),
                        style: text_style(&cs),
                    });
                }
                *y += line_advance(&cs, h.para.style.line_height);
            }
            BlockKind::List(l) => {
                for (i, item) in l.items.iter().enumerate() {
                    let prefix = list_prefix(l, i);
                    let text = format!("{prefix}{}", cell_text(&item.blocks));
                    let cs = first_cell_style(&item.blocks).unwrap_or_default();
                    if !text.is_empty() {
                        page.texts.push(PlacedText {
                            text,
                            x: x + indent + f64::from(item.level) * 18.0,
                            y: *y,
                            width: (width - indent).max(0.0),
                            height: effective_size(&cs),
                            style: text_style(&cs),
                        });
                    }
                    *y += line_advance(&cs, LineHeight::Normal);
                }
            }
            BlockKind::TextBox(tb) => place_blocks_at(page, &tb.blocks, x, y, width, indent),
            BlockKind::Blockquote(bq) => {
                place_blocks_at(page, &bq.blocks, x, y, width, indent + 18.0)
            }
            BlockKind::CodeBlock(cb) => {
                let cs = CharStyle {
                    generic: Generic::Mono,
                    size_pt: 10.0,
                    ..CharStyle::default()
                };
                let advance = line_advance(&cs, LineHeight::Normal);
                for line in cb.code.split('\n') {
                    if !line.is_empty() {
                        page.texts.push(PlacedText {
                            text: line.to_string(),
                            x: x + indent,
                            y: *y,
                            width: (width - indent).max(0.0),
                            height: effective_size(&cs),
                            style: text_style(&cs),
                        });
                    }
                    *y += advance;
                }
            }
            // A horizontal rule has no flowable text in an absolute box.
            BlockKind::HorizontalRule => {}
            _ => {}
        }
    }
}

/// Render one [`Sheet`] onto a fresh [`ConvPage`] as a grid of cell text.
fn sheet_page(sheet: &Sheet) -> ConvPage {
    // Column x-offsets from the sheet's widths (point grid).
    let offsets = column_offsets(&sheet.col_widths, 0.0);
    let row_h = 16.0;
    let n_cols = sheet
        .rows
        .iter()
        .map(|r| r.cells.len())
        .max()
        .unwrap_or(0)
        .max(sheet.col_widths.len());
    let width: f64 = (0..n_cols)
        .map(|c| column_width(&sheet.col_widths, c))
        .sum::<f64>()
        + 72.0;
    let height = (sheet.rows.len() as f64) * row_h + 72.0;
    let mut page = ConvPage {
        width: width.max(72.0),
        height: height.max(72.0),
        ..ConvPage::default()
    };
    let mut y = 36.0;
    for row in &sheet.rows {
        for (ci, cell) in row.cells.iter().enumerate() {
            let text = cell_value_text(&cell.value);
            if text.is_empty() {
                continue;
            }
            let x = 36.0 + offsets.get(ci).copied().unwrap_or(0.0);
            page.texts.push(PlacedText {
                text,
                x,
                y,
                width: column_width(&sheet.col_widths, ci),
                height: effective_size(&cell.style),
                style: text_style(&cell.style),
            });
        }
        y += row_h;
    }
    page
}

/// Render a cell's typed value as display text.
fn cell_value_text(value: &crate::model::CellValue) -> String {
    use crate::model::CellValue;
    match value {
        CellValue::Empty => String::new(),
        CellValue::Text(t) => t.clone(),
        CellValue::Number(n) => format_number(*n),
        CellValue::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
    }
}

/// Compact numeric rendering: integral values drop the fraction.
fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        let mut s = format!("{n}");
        if let Some(dot) = s.find('.') {
            // Trim to at most 6 fractional digits, then strip trailing zeros.
            let end = (dot + 7).min(s.len());
            s.truncate(end);
            while s.ends_with('0') {
                s.pop();
            }
            if s.ends_with('.') {
                s.pop();
            }
        }
        s
    }
}

/// Render one [`Slide`] onto a fresh [`ConvPage`], placing each shape and
/// placeholder at its frame.
fn slide_page(slide: &Slide) -> ConvPage {
    let mut page = ConvPage {
        width: slide.geometry.width,
        height: slide.geometry.height,
        ..ConvPage::default()
    };
    for shape in &slide.shapes {
        place_block_on(&mut page, shape);
    }
    for ph in &slide.placeholders {
        place_block_on(&mut page, &ph.block);
    }
    page
}

/// Place a single block on `page`, honouring its frame when present (slides give
/// every shape a frame; a frameless one falls back to the page margin origin).
fn place_block_on(page: &mut ConvPage, block: &Block) {
    let frame = block
        .frame
        .unwrap_or(Rect::new(36.0, 36.0, page.width - 72.0, page.height - 72.0));
    match &block.kind {
        BlockKind::Image(img) => place_image(page, img, frame),
        BlockKind::Shape(shape) => place_shape(page, shape, frame),
        BlockKind::Paragraph(p) => place_text_block(page, &p_text(p), frame),
        BlockKind::Heading(h) => place_text_block(page, &p_text(&h.para), frame),
        BlockKind::TextBox(tb) => place_text_box(page, tb, frame),
        BlockKind::List(l) => place_list_box(page, l, frame),
        BlockKind::Table(t) => place_table_box(page, t, frame),
        BlockKind::CodeBlock(cb) => place_code_box(page, cb, frame),
        BlockKind::Blockquote(bq) => place_quote_box(page, bq, frame),
        BlockKind::HorizontalRule => place_rule(page, frame),
        BlockKind::Sheet(_) | BlockKind::Slide(_) => {}
    }
}

/// Place a code block at `frame`: each line stacked verbatim in a monospace run.
fn place_code_box(page: &mut ConvPage, code: &crate::model::CodeBlock, frame: Rect) {
    let cs = CharStyle {
        generic: Generic::Mono,
        size_pt: 10.0,
        ..CharStyle::default()
    };
    let mut y = frame.y;
    let advance = line_advance(&cs, LineHeight::Normal);
    for line in code.code.split('\n') {
        if !line.is_empty() {
            page.texts.push(PlacedText {
                text: line.to_string(),
                x: frame.x,
                y,
                width: frame.w,
                height: effective_size(&cs),
                style: text_style(&cs),
            });
        }
        y += advance;
    }
}

/// Place a block quote at `frame`: its blocks stacked from the top-left, nudged
/// right so the quotation reads as set off from the surrounding flow.
fn place_quote_box(page: &mut ConvPage, quote: &Blockquote, frame: Rect) {
    let mut y = frame.y;
    place_blocks_at(page, &quote.blocks, frame.x, &mut y, frame.w, 18.0);
}

/// Place a horizontal rule across the top of `frame` as a thin stroke.
fn place_rule(page: &mut ConvPage, frame: Rect) {
    let w = frame.w.max(1.0);
    let y = frame.y;
    page.shapes.push(PlacedShape {
        x: frame.x,
        y,
        width: w,
        height: 0.0,
        segments: vec![PathSeg::Move(frame.x, y), PathSeg::Line(frame.x + w, y)],
        fill: None,
        stroke: Some([0.6, 0.6, 0.6]),
        stroke_width: 1.0,
        fill_alpha: 1.0,
        stroke_alpha: 1.0,
        dash: Vec::new(),
    });
}

impl From<&Document> for Vec<ConvPage> {
    /// Lower a model document to the flat [`ConvPage`] IR (compat fallback).
    fn from(doc: &Document) -> Vec<ConvPage> {
        let mut out: Vec<ConvPage> = Vec::new();
        for section in &doc.sections {
            for page in &section.pages {
                // A page whose sole content is a Sheet/Slide block expands into
                // one ConvPage per sheet/slide. Otherwise it is a flow/absolute
                // page laid out within the section geometry.
                let mut handled_special = false;
                for block in &page.blocks {
                    match &block.kind {
                        BlockKind::Sheet(sb) => {
                            for sheet in &sb.sheets {
                                out.push(sheet_page(sheet));
                            }
                            handled_special = true;
                        }
                        BlockKind::Slide(sl) => {
                            for slide in &sl.slides {
                                out.push(slide_page(slide));
                            }
                            handled_special = true;
                        }
                        _ => {}
                    }
                }
                if handled_special {
                    // Place any non-sheet/slide blocks of the same page in a flow
                    // page so nothing is silently dropped.
                    let rest: Vec<&Block> = page
                        .blocks
                        .iter()
                        .filter(|b| !matches!(b.kind, BlockKind::Sheet(_) | BlockKind::Slide(_)))
                        .collect();
                    if !rest.is_empty() {
                        let mut flow = Flow::new(section.geometry);
                        for b in rest {
                            flow.layout_block(b, 0.0);
                        }
                        out.extend(flow.pages);
                    }
                    continue;
                }

                let mut flow = Flow::new(section.geometry);
                flow.layout_blocks(&page.blocks, 0.0);
                out.extend(flow.pages);
            }
        }
        out
    }
}

/// Lower a model [`Document`] all the way to PDF bytes: `model → Vec<ConvPage> →`
/// the from-scratch [`PdfBuilder`](super::build::PdfBuilder) page path
/// (standard-14 fonts, WinAnsi, zero embedded font bytes). Text runs become
/// real `Tj` operators and shapes their bounding rectangles — the same
/// reconstruction every `*→PDF` reverse converter uses.
pub fn pdf_from_model(doc: &Document) -> Vec<u8> {
    use super::build::{PdfBuilder, StdFont};

    let pages: Vec<ConvPage> = doc.into();
    let mut builder = PdfBuilder::new();
    for page in &pages {
        let idx = builder.add_page(page.width, page.height);
        for shape in &page.shapes {
            builder.rect(
                idx,
                shape.x,
                shape.y,
                shape.width,
                shape.height,
                shape.stroke,
                shape.fill,
            );
        }
        for text in &page.texts {
            let font = StdFont::pick(
                matches!(text.style.generic, Generic::Serif),
                matches!(text.style.generic, Generic::Mono),
                text.style.bold,
                text.style.italic,
            );
            // Highlight: paint the run's background as a filled rectangle BEHIND
            // the glyphs (drawn first so the text sits on top). The box spans the
            // run's measured width and one font-size of height — a word-processor
            // text highlight (`w:highlight`/`fo:background-color`). Runs without a
            // background skip this entirely, so existing output is unchanged.
            if let Some(bg) = text.style.background {
                builder.rect(
                    idx,
                    text.x,
                    text.y,
                    text.width.max(0.1),
                    text.height.max(1.0),
                    None,
                    Some(bg),
                );
            }
            let color = text.style.color.unwrap_or([0.0, 0.0, 0.0]);
            // `height` carries the run's font size (see `convert_pages`).
            builder.text(
                idx,
                text.x,
                text.y,
                text.height.max(1.0),
                &text.text,
                font,
                color,
            );
        }
    }
    builder.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        Block, BlockId, BlockKind, CellValue, Document, Heading, Inline, InlineRun, Page,
        Paragraph, Section, Sheet, SheetBlock, SheetCell, SheetRow, TextBox,
    };

    fn run(text: &str) -> Inline {
        Inline::Run(InlineRun {
            text: text.to_string(),
            style: CharStyle {
                size_pt: 12.0,
                ..CharStyle::default()
            },
            source_index: None,
        })
    }

    fn paragraph(text: &str) -> Paragraph {
        Paragraph {
            runs: vec![run(text)],
            ..Paragraph::default()
        }
    }

    fn block(kind: BlockKind, frame: Option<Rect>) -> Block {
        Block {
            id: BlockId(0),
            frame,
            rotation: crate::model::Rotation::D0,
            kind,
        }
    }

    /// A page with a heading + a flowed paragraph + an absolute text box lowers
    /// to a ConvPage whose PlacedText carries all three strings, the text box
    /// near its frame.
    #[test]
    fn flow_and_absolute_text_lower_to_placed_text() {
        let heading = block(
            BlockKind::Heading(Heading {
                level: 1,
                para: paragraph("Big Title"),
            }),
            None,
        );
        let body = block(
            BlockKind::Paragraph(paragraph("Some flowing body text.")),
            None,
        );
        let textbox = block(
            BlockKind::TextBox(TextBox {
                blocks: vec![block(
                    BlockKind::Paragraph(paragraph("Floating note")),
                    None,
                )],
            }),
            Some(Rect::new(300.0, 250.0, 180.0, 40.0)),
        );

        let doc = Document {
            sections: vec![Section {
                geometry: PageGeometry::a4(),
                pages: vec![Page {
                    blocks: vec![heading, body, textbox],
                    absolute: false,
                }],
                ..Section::default()
            }],
            ..Document::default()
        };

        let pages: Vec<ConvPage> = (&doc).into();
        assert_eq!(pages.len(), 1, "single section page ⇒ one ConvPage");
        let texts = &pages[0].texts;

        let heading_run = texts
            .iter()
            .find(|t| t.text == "Big Title")
            .expect("heading text present");
        let body_run = texts
            .iter()
            .find(|t| t.text == "Some flowing body text.")
            .expect("flow paragraph text present");
        // The heading flows above the body.
        assert!(
            heading_run.y < body_run.y,
            "heading is laid out above the body"
        );

        let note = texts
            .iter()
            .find(|t| t.text == "Floating note")
            .expect("absolute text-box text present");
        // The absolute box sits at (≈300, ≈250) — near its frame origin.
        assert!((note.x - 300.0).abs() < 1.0, "text box near frame x");
        assert!((note.y - 250.0).abs() < 1.0, "text box near frame y");
    }

    /// A Sheet block lowers to a grid ConvPage holding the cell strings at
    /// distinct column positions.
    #[test]
    fn sheet_lowers_to_grid_conv_page() {
        let cell = |v: CellValue| SheetCell {
            value: v,
            ..SheetCell::default()
        };
        let sheet = Sheet {
            name: "Data".to_string(),
            rows: vec![
                SheetRow {
                    cells: vec![
                        cell(CellValue::Text("Name".to_string())),
                        cell(CellValue::Text("Qty".to_string())),
                    ],
                    ..Default::default()
                },
                SheetRow {
                    cells: vec![
                        cell(CellValue::Text("Widget".to_string())),
                        cell(CellValue::Number(42.0)),
                    ],
                    ..Default::default()
                },
            ],
            merges: Vec::new(),
            col_widths: vec![120.0, 80.0],
        };

        let doc = Document {
            sections: vec![Section {
                geometry: PageGeometry::a4(),
                pages: vec![Page {
                    blocks: vec![block(
                        BlockKind::Sheet(SheetBlock {
                            sheets: vec![sheet],
                        }),
                        None,
                    )],
                    absolute: true,
                }],
                ..Section::default()
            }],
            ..Document::default()
        };

        let pages: Vec<ConvPage> = (&doc).into();
        assert_eq!(pages.len(), 1, "one sheet ⇒ one ConvPage");
        let texts = &pages[0].texts;

        for want in ["Name", "Qty", "Widget", "42"] {
            assert!(
                texts.iter().any(|t| t.text == want),
                "cell text {want:?} present in grid"
            );
        }

        // Header cells share a row (same y) at different columns (different x).
        let name = texts.iter().find(|t| t.text == "Name").unwrap();
        let qty = texts.iter().find(|t| t.text == "Qty").unwrap();
        assert!((name.y - qty.y).abs() < 0.5, "header cells on the same row");
        assert!(qty.x > name.x, "Qty column to the right of Name column");
    }

    /// `pdf_from_model` produces a valid PDF whose text is extractable.
    #[test]
    fn pdf_from_model_round_trips_text() {
        let doc = Document {
            sections: vec![Section {
                geometry: PageGeometry::a4(),
                pages: vec![Page {
                    blocks: vec![block(BlockKind::Paragraph(paragraph("Bridge works")), None)],
                    absolute: false,
                }],
                ..Section::default()
            }],
            ..Document::default()
        };

        let pdf = pdf_from_model(&doc);
        assert_eq!(&pdf[0..5], b"%PDF-", "starts with PDF header");
        assert!(
            pdf.windows(5).any(|w| w == b"%%EOF"),
            "ends with EOF marker"
        );

        let reopened = crate::Document::open(&pdf).expect("re-open built PDF");
        assert_eq!(reopened.page_count(), 1);
        let runs = reopened.page_text_runs(1).unwrap();
        assert!(
            runs.iter().any(|r| r.text.contains("Bridge")),
            "model text survives the round-trip"
        );
    }

    /// A run paragraph carrying a highlight (`CharStyle.background`).
    fn highlighted_paragraph(text: &str, bg: [f64; 3]) -> Paragraph {
        Paragraph {
            runs: vec![Inline::Run(InlineRun {
                text: text.to_string(),
                style: CharStyle {
                    size_pt: 12.0,
                    background: Some(bg),
                    ..CharStyle::default()
                },
                source_index: None,
            })],
            ..Paragraph::default()
        }
    }

    /// A highlighted run lowers to a `PlacedText` whose style carries the
    /// background, and `pdf_from_model` paints that highlight as a filled
    /// rectangle (the fill colour op) *before* the glyphs.
    #[test]
    fn highlighted_run_paints_background_rect() {
        let doc = Document {
            sections: vec![Section {
                geometry: PageGeometry::a4(),
                pages: vec![Page {
                    blocks: vec![block(
                        BlockKind::Paragraph(highlighted_paragraph("HiLite", [1.0, 1.0, 0.0])),
                        None,
                    )],
                    absolute: false,
                }],
                ..Section::default()
            }],
            ..Document::default()
        };

        // The lowering carries the background onto the PlacedText.
        let pages: Vec<ConvPage> = (&doc).into();
        let placed = pages[0]
            .texts
            .iter()
            .find(|t| t.text == "HiLite")
            .expect("highlighted run present");
        assert_eq!(
            placed.style.background,
            Some([1.0, 1.0, 0.0]),
            "background lowered onto the placed run"
        );

        // The PDF content paints the yellow fill (a `1 1 0 rg` + `re ... f`)
        // before drawing the text — and a plain document carries no such fill.
        let pdf = pdf_from_model(&doc);
        let body = String::from_utf8_lossy(&pdf);
        assert!(
            body.contains("1 1 0 rg") && body.contains(" re\nf\n"),
            "highlight emits a filled yellow rectangle"
        );

        let plain = pdf_from_model(&Document {
            sections: vec![Section {
                geometry: PageGeometry::a4(),
                pages: vec![Page {
                    blocks: vec![block(BlockKind::Paragraph(paragraph("plain")), None)],
                    absolute: false,
                }],
                ..Section::default()
            }],
            ..Document::default()
        });
        assert!(
            !String::from_utf8_lossy(&plain).contains("1 1 0 rg"),
            "a plain run paints no highlight fill"
        );
    }
}
