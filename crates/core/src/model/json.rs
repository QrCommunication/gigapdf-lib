//! Zero-dependency JSON serialization for the [`Document`](super::Document) model.
//!
//! A single stable envelope: [`Document::to_json`] emits it, [`Document::from_json`]
//! parses it back, and the two are exact inverses (structural round-trip). The
//! writer mirrors the crate's existing JSON conventions — the
//! [`json_str`](self) string escaper is the same shape as
//! [`js::boa`](crate::js)'s and the WASM layer's — and the reader extends the
//! [`convert::grids`](crate::convert) `Reader` (`ws`/`peek`/`eat`/`array`/`string`)
//! with numbers, booleans, `null`, and objects.
//!
//! ## Envelope
//!
//! ```json
//! {
//!   "v": 1,
//!   "meta":      { "title": "…"|null, "author": …, "subject": …,
//!                  "keywords": ["…"], "lang": "en"|null },
//!   "styles":    { "named": { "<StyleId>": <NamedStyle>, … } },
//!   "sections":  [ { "geometry": <PageGeometry>,
//!                    "header": [<Block>…]|null, "footer": …,
//!                    "pages": [ { "blocks": [<Block>…], "absolute": false } ] } ],
//!   "outline":   [ { "title": "…", "page": 0, "children": [<OutlineNode>…] } ],
//!   "resources": { "images": { "<u64>": { "bytes": "<base64>", "format": "png" } } }
//! }
//! ```
//!
//! `<Block>`, `<BlockKind>`, the inline tree, sheets and slides are tagged
//! objects; see the per-type writers/readers below. Floats are emitted in
//! Rust's shortest round-tripping form (`{}`), so `parse::<f64>()` recovers the
//! exact bits. The model carries no NaN/∞; any non-finite float is written as
//! `0`.

use crate::content::vector::PathSeg;
use crate::convert::base64;
use crate::convert::style::Generic;
use crate::model::geom::{Margins, PageGeometry, Rect, Rotation};
use crate::model::sheet::{CellValue, MergeRange, Sheet, SheetBlock, SheetCell, SheetRow};
use crate::model::slide::{Placeholder, PlaceholderRole, Slide, SlideBlock};
use crate::model::style::{
    Align, CellVAlign, CharStyle, LineHeight, NamedStyle, ParaBorder, ParagraphStyle, StyleId,
    StyleTable, VAlign,
};
use crate::model::{
    Block, BlockId, BlockKind, Blockquote, BorderStyle, Cell, CodeBlock, Comment, DocMeta, Document,
    Heading, ImageRef, ImageResource, Inline, InlineRun, LinkTarget, List, ListItem, ListMarker,
    OutlineNode, Page, Paragraph, ResourceTable, Row, Section, Shape, Table, TextBox,
};

/// Current envelope version. Bump on any incompatible layout change.
const VERSION: u64 = 1;

impl Document {
    /// Serialize this document to the stable JSON envelope.
    pub fn to_json(&self) -> String {
        let mut w = Writer::new();
        w.document(self);
        w.out
    }

    /// Parse a document from the JSON envelope. Returns `None` on any malformed
    /// or structurally-unexpected input (wrong type, missing required field,
    /// trailing junk).
    pub fn from_json(s: &str) -> Option<Document> {
        let mut r = Reader::new(s.as_bytes());
        let doc = r.document()?;
        r.ws();
        // Reject anything after the top-level object.
        if r.i == r.b.len() {
            Some(doc)
        } else {
            None
        }
    }
}

/// Serialize a single [`Block`] to its stable JSON shape — the exact inverse of
/// [`block_from_json`]. Lets a host emit a block to splice via
/// [`model::edit`](crate::model::edit)'s `insertBlock` op.
pub fn block_to_json(block: &Block) -> String {
    let mut w = Writer::new();
    w.block(block);
    w.out
}

/// Parse a single [`Block`] from the same JSON shape [`block_to_json`] (and
/// [`Document::to_json`]) emit for a block — used by
/// [`model::edit`](crate::model::edit)'s `insertBlock` op. Returns `None` on
/// malformed input or trailing junk.
pub fn block_from_json(s: &str) -> Option<Block> {
    let mut r = Reader::new(s.as_bytes());
    let block = r.block()?;
    r.ws();
    if r.i == r.b.len() {
        Some(block)
    } else {
        None
    }
}

// ───────────────────────── writer ────────────────────────────────────────────

/// Tiny JSON writer: object/array scaffolding + primitive emitters. Keeps a
/// `first` stack so commas are inserted between members without trailing commas.
struct Writer {
    out: String,
    first: Vec<bool>,
}

impl Writer {
    fn new() -> Self {
        Self {
            out: String::new(),
            first: Vec::new(),
        }
    }

    fn obj_open(&mut self) {
        self.out.push('{');
        self.first.push(true);
    }
    fn obj_close(&mut self) {
        self.out.push('}');
        self.first.pop();
    }
    fn arr_open(&mut self) {
        self.out.push('[');
        self.first.push(true);
    }
    fn arr_close(&mut self) {
        self.out.push(']');
        self.first.pop();
    }

    /// Comma before any element but the first within the current container.
    fn sep(&mut self) {
        let f = self.first.last_mut().expect("sep outside container");
        if *f {
            *f = false;
        } else {
            self.out.push(',');
        }
    }

    /// Write `"key":` (with the leading comma if needed).
    fn key(&mut self, k: &str) {
        self.sep();
        json_str(k, &mut self.out);
        self.out.push(':');
    }

    /// A bare array element separator (comma if needed).
    fn elem(&mut self) {
        self.sep();
    }

    fn str_val(&mut self, s: &str) {
        json_str(s, &mut self.out);
    }
    fn bool_val(&mut self, b: bool) {
        self.out.push_str(if b { "true" } else { "false" });
    }
    fn null(&mut self) {
        self.out.push_str("null");
    }
    fn u64(&mut self, n: u64) {
        self.out.push_str(&n.to_string());
    }
    fn usize(&mut self, n: usize) {
        self.out.push_str(&n.to_string());
    }
    fn f64(&mut self, v: f64) {
        // Shortest round-tripping form; the model carries no non-finite floats.
        if v.is_finite() {
            self.out.push_str(&format!("{v}"));
        } else {
            self.out.push('0');
        }
    }

    // ── named-field convenience helpers ───────────────────────────────────────

    fn k_str(&mut self, k: &str, s: &str) {
        self.key(k);
        self.str_val(s);
    }
    fn k_bool(&mut self, k: &str, b: bool) {
        self.key(k);
        self.bool_val(b);
    }
    fn k_u64(&mut self, k: &str, n: u64) {
        self.key(k);
        self.u64(n);
    }
    fn k_usize(&mut self, k: &str, n: usize) {
        self.key(k);
        self.usize(n);
    }
    fn k_f64(&mut self, k: &str, v: f64) {
        self.key(k);
        self.f64(v);
    }
    /// `"k": "s"` when `Some`, `"k": null` when `None`.
    fn k_opt_str(&mut self, k: &str, s: &Option<String>) {
        self.key(k);
        match s {
            Some(v) => self.str_val(v),
            None => self.null(),
        }
    }
    /// `"k": <rgb array>` when `Some`, `"k": null` when `None`.
    fn k_opt_rgb(&mut self, k: &str, c: &Option<[f64; 3]>) {
        self.key(k);
        match c {
            Some(rgb) => self.rgb(rgb),
            None => self.null(),
        }
    }
    fn rgb(&mut self, c: &[f64; 3]) {
        self.arr_open();
        for v in c {
            self.elem();
            self.f64(*v);
        }
        self.arr_close();
    }

    // ── top level ─────────────────────────────────────────────────────────────

    fn document(&mut self, d: &Document) {
        self.obj_open();
        self.k_u64("v", VERSION);
        self.key("meta");
        self.meta(&d.meta);
        self.key("styles");
        self.style_table(&d.styles);
        self.key("sections");
        self.arr_open();
        for s in &d.sections {
            self.elem();
            self.section(s);
        }
        self.arr_close();
        self.key("outline");
        self.arr_open();
        for n in &d.outline {
            self.elem();
            self.outline_node(n);
        }
        self.arr_close();
        self.key("resources");
        self.resources(&d.resources);
        self.key("comments");
        self.arr_open();
        for c in &d.comments {
            self.elem();
            self.comment(c);
        }
        self.arr_close();
        self.obj_close();
    }

    fn comment(&mut self, c: &Comment) {
        self.obj_open();
        self.k_str("id", &c.id);
        self.k_str("author", &c.author);
        self.k_str("date", &c.date);
        self.k_str("text", &c.text);
        self.obj_close();
    }

    fn meta(&mut self, m: &DocMeta) {
        self.obj_open();
        self.k_opt_str("title", &m.title);
        self.k_opt_str("author", &m.author);
        self.k_opt_str("subject", &m.subject);
        self.key("keywords");
        self.str_array(&m.keywords);
        self.k_opt_str("lang", &m.lang);
        // Extended properties: emit each only when present (empty ⇒ absent), so
        // a document with no extra metadata serializes exactly as before.
        for (k, v) in [
            ("description", &m.description),
            ("created", &m.created),
            ("modified", &m.modified),
            ("lastModifiedBy", &m.last_modified_by),
            ("revision", &m.revision),
            ("application", &m.application),
            ("company", &m.company),
            ("generator", &m.generator),
            ("editingCycles", &m.editing_cycles),
        ] {
            if !v.is_empty() {
                self.k_str(k, v);
            }
        }
        self.obj_close();
    }

    fn str_array(&mut self, items: &[String]) {
        self.arr_open();
        for s in items {
            self.elem();
            self.str_val(s);
        }
        self.arr_close();
    }

    // ── styles ────────────────────────────────────────────────────────────────

    fn style_table(&mut self, t: &StyleTable) {
        self.obj_open();
        self.key("named");
        self.obj_open();
        for (id, ns) in &t.named {
            self.key(&id.0);
            self.named_style(ns);
        }
        self.obj_close();
        self.obj_close();
    }

    fn named_style(&mut self, ns: &NamedStyle) {
        self.obj_open();
        self.key("para");
        self.para_style(&ns.para);
        self.key("char");
        self.char_style(&ns.char_);
        self.key("based_on");
        match &ns.based_on {
            Some(id) => self.str_val(&id.0),
            None => self.null(),
        }
        self.obj_close();
    }

    fn para_style(&mut self, p: &ParagraphStyle) {
        self.obj_open();
        self.k_str("align", align_tag(p.align));
        self.k_f64("space_before_pt", p.space_before_pt);
        self.k_f64("space_after_pt", p.space_after_pt);
        self.k_f64("indent_left_pt", p.indent_left_pt);
        self.k_f64("indent_right_pt", p.indent_right_pt);
        self.k_f64("first_line_pt", p.first_line_pt);
        self.key("line_height");
        self.line_height(p.line_height);
        self.k_opt_rgb("background", &p.background);
        self.key("borders");
        self.para_borders(&p.borders);
        self.k_bool("keep_with_next", p.keep_with_next);
        self.k_bool("keep_together", p.keep_together);
        self.obj_close();
    }

    /// The four-element `[top, right, bottom, left]` border array: each side is
    /// either `null` or `{width_pt, style, color}`. Matches [`Reader::para_borders`].
    fn para_borders(&mut self, borders: &[Option<ParaBorder>; 4]) {
        self.arr_open();
        for b in borders {
            self.elem();
            match b {
                Some(b) => self.para_border(b),
                None => self.null(),
            }
        }
        self.arr_close();
    }

    fn para_border(&mut self, b: &ParaBorder) {
        self.obj_open();
        self.k_f64("width_pt", b.width_pt);
        self.k_str("style", &b.style);
        self.key("color");
        self.rgb(&b.color);
        self.obj_close();
    }

    fn line_height(&mut self, lh: LineHeight) {
        self.obj_open();
        match lh {
            LineHeight::Normal => self.k_str("t", "normal"),
            LineHeight::Multiple(m) => {
                self.k_str("t", "multiple");
                self.k_f64("v", m);
            }
            LineHeight::Points(p) => {
                self.k_str("t", "points");
                self.k_f64("v", p);
            }
        }
        self.obj_close();
    }

    fn char_style(&mut self, c: &CharStyle) {
        self.obj_open();
        self.k_str("family", &c.family);
        self.k_str("generic", generic_tag(c.generic));
        self.k_f64("size_pt", c.size_pt);
        self.k_bool("bold", c.bold);
        self.k_bool("italic", c.italic);
        self.k_bool("underline", c.underline);
        self.k_bool("strike", c.strike);
        self.k_opt_rgb("color", &c.color);
        self.k_opt_rgb("background", &c.background);
        self.k_str("valign", valign_tag(c.vertical_align));
        self.obj_close();
    }

    // ── sections / pages / blocks ─────────────────────────────────────────────

    fn section(&mut self, s: &Section) {
        self.obj_open();
        self.key("geometry");
        self.geometry(&s.geometry);
        self.key("header");
        self.opt_blocks(&s.header);
        self.key("footer");
        self.opt_blocks(&s.footer);
        self.key("pages");
        self.arr_open();
        for p in &s.pages {
            self.elem();
            self.page(p);
        }
        self.arr_close();
        self.obj_close();
    }

    fn opt_blocks(&mut self, b: &Option<Vec<Block>>) {
        match b {
            Some(v) => self.block_array(v),
            None => self.null(),
        }
    }

    fn block_array(&mut self, blocks: &[Block]) {
        self.arr_open();
        for b in blocks {
            self.elem();
            self.block(b);
        }
        self.arr_close();
    }

    fn geometry(&mut self, g: &PageGeometry) {
        self.obj_open();
        self.k_f64("width", g.width);
        self.k_f64("height", g.height);
        self.key("margins");
        self.margins(&g.margins);
        self.k_f64("column_count", g.column_count as f64);
        self.obj_close();
    }

    fn margins(&mut self, m: &Margins) {
        self.obj_open();
        self.k_f64("top", m.top);
        self.k_f64("right", m.right);
        self.k_f64("bottom", m.bottom);
        self.k_f64("left", m.left);
        self.obj_close();
    }

    fn page(&mut self, p: &Page) {
        self.obj_open();
        self.key("blocks");
        self.block_array(&p.blocks);
        self.k_bool("absolute", p.absolute);
        self.obj_close();
    }

    fn block(&mut self, b: &Block) {
        self.obj_open();
        self.k_u64("id", b.id.0);
        self.key("frame");
        match &b.frame {
            Some(r) => self.rect(r),
            None => self.null(),
        }
        self.key("rotation");
        self.rotation(b.rotation);
        self.key("kind");
        self.block_kind(&b.kind);
        self.obj_close();
    }

    fn rect(&mut self, r: &Rect) {
        self.obj_open();
        self.k_f64("x", r.x);
        self.k_f64("y", r.y);
        self.k_f64("w", r.w);
        self.k_f64("h", r.h);
        self.obj_close();
    }

    fn rotation(&mut self, r: Rotation) {
        self.obj_open();
        match r {
            Rotation::D0 => self.k_str("t", "d0"),
            Rotation::D90 => self.k_str("t", "d90"),
            Rotation::D180 => self.k_str("t", "d180"),
            Rotation::D270 => self.k_str("t", "d270"),
            Rotation::Deg(d) => {
                self.k_str("t", "deg");
                self.k_f64("v", d);
            }
        }
        self.obj_close();
    }

    fn block_kind(&mut self, k: &BlockKind) {
        self.obj_open();
        match k {
            BlockKind::Paragraph(p) => {
                self.k_str("t", "paragraph");
                self.key("v");
                self.paragraph(p);
            }
            BlockKind::Heading(h) => {
                self.k_str("t", "heading");
                self.key("v");
                self.heading(h);
            }
            BlockKind::List(l) => {
                self.k_str("t", "list");
                self.key("v");
                self.list(l);
            }
            BlockKind::Table(t) => {
                self.k_str("t", "table");
                self.key("v");
                self.table(t);
            }
            BlockKind::Image(i) => {
                self.k_str("t", "image");
                self.key("v");
                self.image_ref(i);
            }
            BlockKind::Shape(s) => {
                self.k_str("t", "shape");
                self.key("v");
                self.shape(s);
            }
            BlockKind::TextBox(tb) => {
                self.k_str("t", "textbox");
                self.key("v");
                self.text_box(tb);
            }
            BlockKind::Sheet(sb) => {
                self.k_str("t", "sheet");
                self.key("v");
                self.sheet_block(sb);
            }
            BlockKind::Slide(sb) => {
                self.k_str("t", "slide");
                self.key("v");
                self.slide_block(sb);
            }
            BlockKind::CodeBlock(cb) => {
                self.k_str("t", "code");
                self.key("v");
                self.code_block(cb);
            }
            BlockKind::Blockquote(bq) => {
                self.k_str("t", "blockquote");
                self.key("v");
                self.blockquote(bq);
            }
            BlockKind::HorizontalRule => self.k_str("t", "hr"),
        }
        self.obj_close();
    }

    fn code_block(&mut self, cb: &CodeBlock) {
        self.obj_open();
        self.key("lang");
        match &cb.lang {
            Some(l) => self.str_val(l),
            None => self.null(),
        }
        self.k_str("code", &cb.code);
        self.obj_close();
    }

    fn blockquote(&mut self, bq: &Blockquote) {
        self.obj_open();
        self.key("blocks");
        self.block_array(&bq.blocks);
        self.obj_close();
    }

    fn paragraph(&mut self, p: &Paragraph) {
        self.obj_open();
        self.key("style");
        self.para_style(&p.style);
        self.key("style_ref");
        match &p.style_ref {
            Some(id) => self.str_val(&id.0),
            None => self.null(),
        }
        self.key("runs");
        self.inline_array(&p.runs);
        self.obj_close();
    }

    fn heading(&mut self, h: &Heading) {
        self.obj_open();
        self.k_u64("level", h.level as u64);
        self.key("para");
        self.paragraph(&h.para);
        self.obj_close();
    }

    fn inline_array(&mut self, runs: &[Inline]) {
        self.arr_open();
        for i in runs {
            self.elem();
            self.inline(i);
        }
        self.arr_close();
    }

    fn inline(&mut self, i: &Inline) {
        self.obj_open();
        match i {
            Inline::Run(r) => {
                self.k_str("t", "run");
                self.key("v");
                self.inline_run(r);
            }
            Inline::LineBreak => self.k_str("t", "br"),
            Inline::Image(img) => {
                self.k_str("t", "image");
                self.key("v");
                self.image_ref(img);
            }
            Inline::Link { href, children } => {
                self.k_str("t", "link");
                self.key("href");
                self.link_target(href);
                self.key("children");
                self.inline_array(children);
            }
            Inline::CommentRef { id } => {
                self.k_str("t", "commentRef");
                self.k_str("id", id);
            }
        }
        self.obj_close();
    }

    fn inline_run(&mut self, r: &InlineRun) {
        self.obj_open();
        self.k_str("text", &r.text);
        self.key("style");
        self.char_style(&r.style);
        self.key("source_index");
        match r.source_index {
            Some(n) => self.usize(n),
            None => self.null(),
        }
        self.obj_close();
    }

    fn link_target(&mut self, t: &LinkTarget) {
        self.obj_open();
        match t {
            LinkTarget::Url(u) => {
                self.k_str("t", "url");
                self.k_str("v", u);
            }
            LinkTarget::Page(p) => {
                self.k_str("t", "page");
                self.k_usize("v", *p);
            }
        }
        self.obj_close();
    }

    fn list(&mut self, l: &List) {
        self.obj_open();
        self.k_bool("ordered", l.ordered);
        self.key("marker");
        self.list_marker(l.marker);
        self.key("items");
        self.arr_open();
        for it in &l.items {
            self.elem();
            self.list_item(it);
        }
        self.arr_close();
        self.obj_close();
    }

    fn list_marker(&mut self, m: ListMarker) {
        self.obj_open();
        match m {
            ListMarker::Bullet(c) => {
                self.k_str("t", "bullet");
                let mut tmp = [0u8; 4];
                self.k_str("v", c.encode_utf8(&mut tmp));
            }
            ListMarker::Decimal => self.k_str("t", "decimal"),
            ListMarker::LowerAlpha => self.k_str("t", "lower_alpha"),
            ListMarker::UpperAlpha => self.k_str("t", "upper_alpha"),
            ListMarker::LowerRoman => self.k_str("t", "lower_roman"),
            ListMarker::UpperRoman => self.k_str("t", "upper_roman"),
        }
        self.obj_close();
    }

    fn list_item(&mut self, it: &ListItem) {
        self.obj_open();
        self.key("blocks");
        self.block_array(&it.blocks);
        self.k_u64("level", it.level as u64);
        self.obj_close();
    }

    fn table(&mut self, t: &Table) {
        self.obj_open();
        self.key("rows");
        self.arr_open();
        for r in &t.rows {
            self.elem();
            self.row(r);
        }
        self.arr_close();
        self.key("col_widths");
        self.f64_array(&t.col_widths);
        self.key("border");
        self.border(&t.border);
        self.obj_close();
    }

    fn f64_array(&mut self, vals: &[f64]) {
        self.arr_open();
        for v in vals {
            self.elem();
            self.f64(*v);
        }
        self.arr_close();
    }

    fn row(&mut self, r: &Row) {
        self.obj_open();
        self.key("cells");
        self.arr_open();
        for c in &r.cells {
            self.elem();
            self.cell(c);
        }
        self.arr_close();
        self.key("height");
        match r.height {
            Some(h) => self.f64(h),
            None => self.null(),
        }
        self.key("is_header");
        self.bool_val(r.is_header);
        self.obj_close();
    }

    fn cell(&mut self, c: &Cell) {
        self.obj_open();
        self.key("blocks");
        self.block_array(&c.blocks);
        self.k_u64("col_span", c.col_span as u64);
        self.k_u64("row_span", c.row_span as u64);
        self.k_opt_rgb("shading", &c.shading);
        self.key("vertical_align");
        match c.vertical_align {
            Some(v) => self.str_val(cell_valign_tag(v)),
            None => self.null(),
        }
        self.obj_close();
    }

    fn border(&mut self, b: &BorderStyle) {
        self.obj_open();
        self.k_f64("width", b.width);
        self.key("color");
        self.rgb(&b.color);
        self.obj_close();
    }

    fn image_ref(&mut self, i: &ImageRef) {
        self.obj_open();
        self.k_u64("resource", i.resource);
        self.key("alt");
        match &i.alt {
            Some(a) => self.str_val(a),
            None => self.null(),
        }
        self.obj_close();
    }

    fn shape(&mut self, s: &Shape) {
        self.obj_open();
        self.key("segments");
        self.arr_open();
        for seg in &s.segments {
            self.elem();
            self.path_seg(seg);
        }
        self.arr_close();
        self.k_opt_rgb("fill", &s.fill);
        self.k_opt_rgb("stroke", &s.stroke);
        self.k_f64("stroke_width", s.stroke_width);
        self.key("dash");
        self.f64_array(&s.dash);
        self.obj_close();
    }

    fn path_seg(&mut self, seg: &PathSeg) {
        self.obj_open();
        match *seg {
            PathSeg::Move(x, y) => {
                self.k_str("t", "m");
                self.k_f64("x", x);
                self.k_f64("y", y);
            }
            PathSeg::Line(x, y) => {
                self.k_str("t", "l");
                self.k_f64("x", x);
                self.k_f64("y", y);
            }
            PathSeg::Cubic(x1, y1, x2, y2, x, y) => {
                self.k_str("t", "c");
                self.k_f64("x1", x1);
                self.k_f64("y1", y1);
                self.k_f64("x2", x2);
                self.k_f64("y2", y2);
                self.k_f64("x", x);
                self.k_f64("y", y);
            }
            PathSeg::Close => self.k_str("t", "z"),
        }
        self.obj_close();
    }

    fn text_box(&mut self, tb: &TextBox) {
        self.obj_open();
        self.key("blocks");
        self.block_array(&tb.blocks);
        self.obj_close();
    }

    // ── sheets ────────────────────────────────────────────────────────────────

    fn sheet_block(&mut self, sb: &SheetBlock) {
        self.obj_open();
        self.key("sheets");
        self.arr_open();
        for s in &sb.sheets {
            self.elem();
            self.sheet(s);
        }
        self.arr_close();
        self.obj_close();
    }

    fn sheet(&mut self, s: &Sheet) {
        self.obj_open();
        self.k_str("name", &s.name);
        self.key("rows");
        self.arr_open();
        for r in &s.rows {
            self.elem();
            self.sheet_row(r);
        }
        self.arr_close();
        self.key("merges");
        self.arr_open();
        for m in &s.merges {
            self.elem();
            self.merge_range(m);
        }
        self.arr_close();
        self.key("col_widths");
        self.f64_array(&s.col_widths);
        self.obj_close();
    }

    fn sheet_row(&mut self, r: &SheetRow) {
        self.obj_open();
        self.key("cells");
        self.arr_open();
        for c in &r.cells {
            self.elem();
            self.sheet_cell(c);
        }
        self.arr_close();
        self.key("height");
        match r.height {
            Some(h) => self.f64(h),
            None => self.null(),
        }
        self.obj_close();
    }

    fn sheet_cell(&mut self, c: &SheetCell) {
        self.obj_open();
        self.key("value");
        self.cell_value(&c.value);
        self.key("formula");
        match &c.formula {
            Some(f) => self.str_val(f),
            None => self.null(),
        }
        self.key("number_format");
        match &c.number_format {
            Some(f) => self.str_val(f),
            None => self.null(),
        }
        self.k_opt_rgb("fill", &c.fill);
        self.key("style");
        self.char_style(&c.style);
        self.key("border");
        match &c.border {
            Some(b) => self.border(b),
            None => self.null(),
        }
        self.key("align");
        match c.align {
            Some(a) => self.str_val(align_tag(a)),
            None => self.null(),
        }
        self.key("vertical_align");
        match c.vertical_align {
            Some(v) => self.str_val(cell_valign_tag(v)),
            None => self.null(),
        }
        self.k_bool("wrap", c.wrap);
        self.key("hyperlink");
        match &c.hyperlink {
            Some(h) => self.str_val(h),
            None => self.null(),
        }
        self.obj_close();
    }

    fn cell_value(&mut self, v: &CellValue) {
        self.obj_open();
        match v {
            CellValue::Empty => self.k_str("t", "empty"),
            CellValue::Text(s) => {
                self.k_str("t", "text");
                self.k_str("v", s);
            }
            CellValue::Number(n) => {
                self.k_str("t", "number");
                self.k_f64("v", *n);
            }
            CellValue::Bool(b) => {
                self.k_str("t", "bool");
                self.k_bool("v", *b);
            }
        }
        self.obj_close();
    }

    fn merge_range(&mut self, m: &MergeRange) {
        self.obj_open();
        self.k_usize("r0", m.r0);
        self.k_usize("c0", m.c0);
        self.k_usize("r1", m.r1);
        self.k_usize("c1", m.c1);
        self.obj_close();
    }

    // ── slides ────────────────────────────────────────────────────────────────

    fn slide_block(&mut self, sb: &SlideBlock) {
        self.obj_open();
        self.key("slides");
        self.arr_open();
        for s in &sb.slides {
            self.elem();
            self.slide(s);
        }
        self.arr_close();
        self.obj_close();
    }

    fn slide(&mut self, s: &Slide) {
        self.obj_open();
        self.key("geometry");
        self.geometry(&s.geometry);
        self.key("shapes");
        self.block_array(&s.shapes);
        self.key("placeholders");
        self.arr_open();
        for p in &s.placeholders {
            self.elem();
            self.placeholder(p);
        }
        self.arr_close();
        self.key("notes");
        self.opt_blocks(&s.notes);
        self.k_opt_rgb("background", &s.background);
        self.obj_close();
    }

    fn placeholder(&mut self, p: &Placeholder) {
        self.obj_open();
        self.key("role");
        self.placeholder_role(&p.role);
        self.key("block");
        self.block(&p.block);
        self.obj_close();
    }

    fn placeholder_role(&mut self, r: &PlaceholderRole) {
        self.obj_open();
        match r {
            PlaceholderRole::Title => self.k_str("t", "title"),
            PlaceholderRole::Subtitle => self.k_str("t", "subtitle"),
            PlaceholderRole::Body => self.k_str("t", "body"),
            PlaceholderRole::Other(s) => {
                self.k_str("t", "other");
                self.k_str("v", s);
            }
        }
        self.obj_close();
    }

    // ── outline / resources ───────────────────────────────────────────────────

    fn outline_node(&mut self, n: &OutlineNode) {
        self.obj_open();
        self.k_str("title", &n.title);
        self.k_usize("page", n.page);
        self.key("children");
        self.arr_open();
        for c in &n.children {
            self.elem();
            self.outline_node(c);
        }
        self.arr_close();
        self.obj_close();
    }

    fn resources(&mut self, r: &ResourceTable) {
        self.obj_open();
        self.key("images");
        self.obj_open();
        for (id, img) in &r.images {
            self.key(&id.to_string());
            self.image_resource(img);
        }
        self.obj_close();
        self.obj_close();
    }

    fn image_resource(&mut self, img: &ImageResource) {
        self.obj_open();
        self.k_str("bytes", &base64(&img.bytes));
        self.k_str("format", &img.format);
        self.obj_close();
    }
}

// ── enum tag helpers (write) ──────────────────────────────────────────────────

fn align_tag(a: Align) -> &'static str {
    match a {
        Align::Left => "left",
        Align::Center => "center",
        Align::Right => "right",
        Align::Justify => "justify",
    }
}
fn valign_tag(v: VAlign) -> &'static str {
    match v {
        VAlign::Baseline => "baseline",
        VAlign::Super => "super",
        VAlign::Sub => "sub",
    }
}

fn cell_valign_tag(v: CellVAlign) -> &'static str {
    match v {
        CellVAlign::Top => "top",
        CellVAlign::Middle => "middle",
        CellVAlign::Bottom => "bottom",
    }
}
fn generic_tag(g: Generic) -> &'static str {
    match g {
        Generic::Sans => "sans",
        Generic::Serif => "serif",
        Generic::Mono => "mono",
    }
}

/// Append a JSON-escaped string literal (`"…"`) to `out` — same escaping as the
/// crate's other JSON emitters (`js::boa::json_str`, the WASM layer).
fn json_str(s: &str, out: &mut String) {
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

// ───────────────────────── reader ────────────────────────────────────────────

/// Recursive-descent JSON reader. Extends the `convert::grids` `Reader`
/// (`ws`/`peek`/`eat`/`array`/`string`) with `number`, `bool`, `null`, and
/// keyed-object iteration. Every method returns `None` on any unexpected token,
/// which bubbles up to a single `from_json` failure.
struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Reader<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, i: 0 }
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

    /// Match a bare literal (`true`/`false`/`null`) at the cursor.
    fn lit(&mut self, word: &[u8]) -> Option<()> {
        self.ws();
        if self.b.get(self.i..self.i + word.len()) == Some(word) {
            self.i += word.len();
            Some(())
        } else {
            None
        }
    }

    fn bool(&mut self) -> Option<bool> {
        match self.peek()? {
            b't' => self.lit(b"true").map(|_| true),
            b'f' => self.lit(b"false").map(|_| false),
            _ => None,
        }
    }

    fn null(&mut self) -> Option<()> {
        self.lit(b"null")
    }

    /// A JSON number → `f64` (sign, digits, fraction, exponent).
    fn number(&mut self) -> Option<f64> {
        self.ws();
        let start = self.i;
        if matches!(self.b.get(self.i), Some(b'-')) {
            self.i += 1;
        }
        let mut digits = false;
        while matches!(self.b.get(self.i), Some(b'0'..=b'9')) {
            self.i += 1;
            digits = true;
        }
        if self.b.get(self.i) == Some(&b'.') {
            self.i += 1;
            while matches!(self.b.get(self.i), Some(b'0'..=b'9')) {
                self.i += 1;
                digits = true;
            }
        }
        if !digits {
            self.i = start;
            return None;
        }
        if matches!(self.b.get(self.i), Some(b'e') | Some(b'E')) {
            self.i += 1;
            if matches!(self.b.get(self.i), Some(b'+') | Some(b'-')) {
                self.i += 1;
            }
            let mut exp = false;
            while matches!(self.b.get(self.i), Some(b'0'..=b'9')) {
                self.i += 1;
                exp = true;
            }
            if !exp {
                self.i = start;
                return None;
            }
        }
        std::str::from_utf8(&self.b[start..self.i])
            .ok()?
            .parse()
            .ok()
    }

    fn usize(&mut self) -> Option<usize> {
        // Non-negative integer via the number scanner, range-checked.
        let n = self.number()?;
        if n.fract() == 0.0 && n >= 0.0 && n <= usize::MAX as f64 {
            Some(n as usize)
        } else {
            None
        }
    }

    fn u64(&mut self) -> Option<u64> {
        let n = self.number()?;
        if n.fract() == 0.0 && n >= 0.0 && n <= u64::MAX as f64 {
            Some(n as u64)
        } else {
            None
        }
    }

    fn u8(&mut self) -> Option<u8> {
        let n = self.u64()?;
        if n <= u8::MAX as u64 {
            Some(n as u8)
        } else {
            None
        }
    }

    fn u16(&mut self) -> Option<u16> {
        let n = self.u64()?;
        if n <= u16::MAX as u64 {
            Some(n as u16)
        } else {
            None
        }
    }

    /// `[ item (, item)* ]` — empty `[]` yields an empty `Vec`. (Same shape as
    /// `convert::grids::Reader::array`.)
    fn array<T>(&mut self, mut item: impl FnMut(&mut Self) -> Option<T>) -> Option<Vec<T>> {
        self.eat(b'[')?;
        let mut out = Vec::new();
        if self.peek()? == b']' {
            self.i += 1;
            return Some(out);
        }
        loop {
            out.push(item(self)?);
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

    /// Iterate the members of `{ "k": <v>, … }`, calling `member(self, key)` for
    /// each key. The callback consumes the value. Empty `{}` is allowed.
    fn object(&mut self, mut member: impl FnMut(&mut Self, &str) -> Option<()>) -> Option<()> {
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

    /// A JSON string (full escape handling, surrogate pairs) — verbatim from
    /// `convert::grids::Reader::string`.
    fn string(&mut self) -> Option<String> {
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
        let hex = self.b.get(self.i..self.i + 4)?;
        self.i += 4;
        u16::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()
    }

    /// An optional string: `null` → `None`, `"…"` → `Some`.
    fn opt_string(&mut self) -> Option<Option<String>> {
        if self.peek()? == b'n' {
            self.null()?;
            Some(None)
        } else {
            Some(Some(self.string()?))
        }
    }

    /// An optional RGB triple: `null` → `None`, `[r,g,b]` → `Some`.
    fn opt_rgb(&mut self) -> Option<Option<[f64; 3]>> {
        if self.peek()? == b'n' {
            self.null()?;
            Some(None)
        } else {
            Some(Some(self.rgb()?))
        }
    }

    fn rgb(&mut self) -> Option<[f64; 3]> {
        let v = self.array(Reader::number)?;
        if v.len() == 3 {
            Some([v[0], v[1], v[2]])
        } else {
            None
        }
    }

    // ── top level ─────────────────────────────────────────────────────────────

    fn document(&mut self) -> Option<Document> {
        let mut version: Option<u64> = None;
        let mut d = Document::default();
        self.object(|r, k| {
            match k {
                "v" => version = Some(r.u64()?),
                "meta" => d.meta = r.meta()?,
                "styles" => d.styles = r.style_table()?,
                "sections" => d.sections = r.array(Reader::section)?,
                "outline" => d.outline = r.array(Reader::outline_node)?,
                "resources" => d.resources = r.resources()?,
                // Optional (added after v1 shipped without comments): a document
                // serialized before this field simply omits it ⇒ empty.
                "comments" => d.comments = r.array(Reader::comment)?,
                _ => return None,
            }
            Some(())
        })?;
        // Require the version marker we recognise.
        if version != Some(VERSION) {
            return None;
        }
        Some(d)
    }

    fn meta(&mut self) -> Option<DocMeta> {
        let mut m = DocMeta::default();
        self.object(|r, k| {
            match k {
                "title" => m.title = r.opt_string()?,
                "author" => m.author = r.opt_string()?,
                "subject" => m.subject = r.opt_string()?,
                "keywords" => m.keywords = r.array(Reader::string)?,
                "lang" => m.lang = r.opt_string()?,
                "description" => m.description = r.string()?,
                "created" => m.created = r.string()?,
                "modified" => m.modified = r.string()?,
                "lastModifiedBy" => m.last_modified_by = r.string()?,
                "revision" => m.revision = r.string()?,
                "application" => m.application = r.string()?,
                "company" => m.company = r.string()?,
                "generator" => m.generator = r.string()?,
                "editingCycles" => m.editing_cycles = r.string()?,
                _ => return None,
            }
            Some(())
        })?;
        Some(m)
    }

    fn style_table(&mut self) -> Option<StyleTable> {
        let mut t = StyleTable::default();
        self.object(|r, k| {
            match k {
                "named" => {
                    r.object(|r2, id| {
                        let ns = r2.named_style()?;
                        t.named.insert(StyleId(id.to_string()), ns);
                        Some(())
                    })?;
                }
                _ => return None,
            }
            Some(())
        })?;
        Some(t)
    }

    fn named_style(&mut self) -> Option<NamedStyle> {
        let mut ns = NamedStyle::default();
        self.object(|r, k| {
            match k {
                "para" => ns.para = r.para_style()?,
                "char" => ns.char_ = r.char_style()?,
                "based_on" => ns.based_on = r.opt_string()?.map(StyleId),
                _ => return None,
            }
            Some(())
        })?;
        Some(ns)
    }

    fn para_style(&mut self) -> Option<ParagraphStyle> {
        let mut p = ParagraphStyle::default();
        self.object(|r, k| {
            match k {
                "align" => p.align = parse_align(&r.string()?)?,
                "space_before_pt" => p.space_before_pt = r.number()?,
                "space_after_pt" => p.space_after_pt = r.number()?,
                "indent_left_pt" => p.indent_left_pt = r.number()?,
                "indent_right_pt" => p.indent_right_pt = r.number()?,
                "first_line_pt" => p.first_line_pt = r.number()?,
                "line_height" => p.line_height = r.line_height()?,
                "background" => p.background = r.opt_rgb()?,
                "borders" => p.borders = r.para_borders()?,
                "keep_with_next" => p.keep_with_next = r.bool()?,
                "keep_together" => p.keep_together = r.bool()?,
                _ => return None,
            }
            Some(())
        })?;
        Some(p)
    }

    /// The four-element `[top, right, bottom, left]` border array. Each element
    /// is `null` (no border) or a `{width_pt, style, color}` object.
    fn para_borders(&mut self) -> Option<[Option<ParaBorder>; 4]> {
        let v: Vec<Option<ParaBorder>> = self.array(Reader::opt_para_border)?;
        if v.len() != 4 {
            return None;
        }
        let mut arr = <[Option<ParaBorder>; 4]>::default();
        for (i, b) in v.into_iter().enumerate() {
            arr[i] = b;
        }
        Some(arr)
    }

    /// `null` → `None`, else a full [`ParaBorder`] object.
    fn opt_para_border(&mut self) -> Option<Option<ParaBorder>> {
        if self.peek()? == b'n' {
            self.null()?;
            Some(None)
        } else {
            Some(Some(self.para_border()?))
        }
    }

    fn para_border(&mut self) -> Option<ParaBorder> {
        let mut b = ParaBorder::default();
        self.object(|r, k| {
            match k {
                "width_pt" => b.width_pt = r.number()?,
                "style" => b.style = r.string()?,
                "color" => b.color = r.rgb()?,
                _ => return None,
            }
            Some(())
        })?;
        Some(b)
    }

    fn line_height(&mut self) -> Option<LineHeight> {
        let mut tag: Option<String> = None;
        let mut v: Option<f64> = None;
        self.object(|r, k| {
            match k {
                "t" => tag = Some(r.string()?),
                "v" => v = Some(r.number()?),
                _ => return None,
            }
            Some(())
        })?;
        match tag.as_deref()? {
            "normal" => Some(LineHeight::Normal),
            "multiple" => Some(LineHeight::Multiple(v?)),
            "points" => Some(LineHeight::Points(v?)),
            _ => None,
        }
    }

    fn char_style(&mut self) -> Option<CharStyle> {
        let mut c = CharStyle::default();
        self.object(|r, k| {
            match k {
                "family" => c.family = r.string()?,
                "generic" => c.generic = parse_generic(&r.string()?)?,
                "size_pt" => c.size_pt = r.number()?,
                "bold" => c.bold = r.bool()?,
                "italic" => c.italic = r.bool()?,
                "underline" => c.underline = r.bool()?,
                "strike" => c.strike = r.bool()?,
                "color" => c.color = r.opt_rgb()?,
                "background" => c.background = r.opt_rgb()?,
                "valign" => c.vertical_align = parse_valign(&r.string()?)?,
                _ => return None,
            }
            Some(())
        })?;
        Some(c)
    }

    fn section(&mut self) -> Option<Section> {
        let mut s = Section::default();
        self.object(|r, k| {
            match k {
                "geometry" => s.geometry = r.geometry()?,
                "header" => s.header = r.opt_blocks()?,
                "footer" => s.footer = r.opt_blocks()?,
                "pages" => s.pages = r.array(Reader::page)?,
                _ => return None,
            }
            Some(())
        })?;
        Some(s)
    }

    fn opt_blocks(&mut self) -> Option<Option<Vec<Block>>> {
        if self.peek()? == b'n' {
            self.null()?;
            Some(None)
        } else {
            Some(Some(self.array(Reader::block)?))
        }
    }

    fn geometry(&mut self) -> Option<PageGeometry> {
        let mut g = PageGeometry::default();
        self.object(|r, k| {
            match k {
                "width" => g.width = r.number()?,
                "height" => g.height = r.number()?,
                "margins" => g.margins = r.margins()?,
                "column_count" => g.column_count = r.number().unwrap_or(1.0).clamp(1.0, 8.0) as u8,
                _ => return None,
            }
            Some(())
        })?;
        Some(g)
    }

    fn margins(&mut self) -> Option<Margins> {
        let mut m = Margins::default();
        self.object(|r, k| {
            match k {
                "top" => m.top = r.number()?,
                "right" => m.right = r.number()?,
                "bottom" => m.bottom = r.number()?,
                "left" => m.left = r.number()?,
                _ => return None,
            }
            Some(())
        })?;
        Some(m)
    }

    fn page(&mut self) -> Option<Page> {
        let mut p = Page::default();
        self.object(|r, k| {
            match k {
                "blocks" => p.blocks = r.array(Reader::block)?,
                "absolute" => p.absolute = r.bool()?,
                _ => return None,
            }
            Some(())
        })?;
        Some(p)
    }

    fn block(&mut self) -> Option<Block> {
        let mut id = BlockId::default();
        let mut frame: Option<Rect> = None;
        let mut rotation = Rotation::default();
        let mut kind: Option<BlockKind> = None;
        self.object(|r, k| {
            match k {
                "id" => id = BlockId(r.u64()?),
                "frame" => frame = r.opt_rect()?,
                "rotation" => rotation = r.rotation()?,
                "kind" => kind = Some(r.block_kind()?),
                _ => return None,
            }
            Some(())
        })?;
        Some(Block {
            id,
            frame,
            rotation,
            kind: kind?,
        })
    }

    fn opt_rect(&mut self) -> Option<Option<Rect>> {
        if self.peek()? == b'n' {
            self.null()?;
            Some(None)
        } else {
            Some(Some(self.rect()?))
        }
    }

    fn rect(&mut self) -> Option<Rect> {
        let mut r = Rect::default();
        self.object(|rd, k| {
            match k {
                "x" => r.x = rd.number()?,
                "y" => r.y = rd.number()?,
                "w" => r.w = rd.number()?,
                "h" => r.h = rd.number()?,
                _ => return None,
            }
            Some(())
        })?;
        Some(r)
    }

    fn rotation(&mut self) -> Option<Rotation> {
        let mut tag: Option<String> = None;
        let mut v: Option<f64> = None;
        self.object(|r, k| {
            match k {
                "t" => tag = Some(r.string()?),
                "v" => v = Some(r.number()?),
                _ => return None,
            }
            Some(())
        })?;
        match tag.as_deref()? {
            "d0" => Some(Rotation::D0),
            "d90" => Some(Rotation::D90),
            "d180" => Some(Rotation::D180),
            "d270" => Some(Rotation::D270),
            "deg" => Some(Rotation::Deg(v?)),
            _ => None,
        }
    }

    fn block_kind(&mut self) -> Option<BlockKind> {
        // Capture the tag, then the raw value object/marker once seen. We buffer
        // the value parse by recording the tag first then dispatching on the
        // "v" key. Most variants carry a "v"; the tagless `hr` (like the inline
        // `br`) carries only "t" and is resolved from the tag after the object.
        let mut tag: Option<String> = None;
        let mut kind: Option<BlockKind> = None;
        self.object(|r, k| {
            match k {
                "t" => tag = Some(r.string()?),
                "v" => {
                    let t = tag.as_deref()?;
                    kind = Some(match t {
                        "paragraph" => BlockKind::Paragraph(r.paragraph()?),
                        "heading" => BlockKind::Heading(r.heading()?),
                        "list" => BlockKind::List(r.list()?),
                        "table" => BlockKind::Table(r.table()?),
                        "image" => BlockKind::Image(r.image_ref()?),
                        "shape" => BlockKind::Shape(r.shape()?),
                        "textbox" => BlockKind::TextBox(r.text_box()?),
                        "sheet" => BlockKind::Sheet(r.sheet_block()?),
                        "slide" => BlockKind::Slide(r.slide_block()?),
                        "code" => BlockKind::CodeBlock(r.code_block()?),
                        "blockquote" => BlockKind::Blockquote(r.blockquote()?),
                        _ => return None,
                    });
                }
                _ => return None,
            }
            Some(())
        })?;
        // Tagless variants resolve from the tag alone.
        match (kind, tag.as_deref()) {
            (Some(k), _) => Some(k),
            (None, Some("hr")) => Some(BlockKind::HorizontalRule),
            _ => None,
        }
    }

    fn code_block(&mut self) -> Option<CodeBlock> {
        let mut cb = CodeBlock::default();
        self.object(|r, k| {
            match k {
                "lang" => cb.lang = r.opt_string()?,
                "code" => cb.code = r.string()?,
                _ => return None,
            }
            Some(())
        })?;
        Some(cb)
    }

    fn blockquote(&mut self) -> Option<Blockquote> {
        let mut bq = Blockquote::default();
        self.object(|r, k| {
            match k {
                "blocks" => bq.blocks = r.array(Reader::block)?,
                _ => return None,
            }
            Some(())
        })?;
        Some(bq)
    }

    fn paragraph(&mut self) -> Option<Paragraph> {
        let mut p = Paragraph::default();
        self.object(|r, k| {
            match k {
                "style" => p.style = r.para_style()?,
                "style_ref" => p.style_ref = r.opt_string()?.map(StyleId),
                "runs" => p.runs = r.array(Reader::inline)?,
                _ => return None,
            }
            Some(())
        })?;
        Some(p)
    }

    fn heading(&mut self) -> Option<Heading> {
        let mut h = Heading::default();
        self.object(|r, k| {
            match k {
                "level" => h.level = r.u8()?,
                "para" => h.para = r.paragraph()?,
                _ => return None,
            }
            Some(())
        })?;
        Some(h)
    }

    fn inline(&mut self) -> Option<Inline> {
        let mut tag: Option<String> = None;
        let mut run: Option<InlineRun> = None;
        let mut img: Option<ImageRef> = None;
        let mut href: Option<LinkTarget> = None;
        let mut children: Option<Vec<Inline>> = None;
        let mut comment_id: Option<String> = None;
        self.object(|r, k| {
            match k {
                "t" => tag = Some(r.string()?),
                "v" => match tag.as_deref()? {
                    "run" => run = Some(r.inline_run()?),
                    "image" => img = Some(r.image_ref()?),
                    _ => return None,
                },
                "href" => href = Some(r.link_target()?),
                "children" => children = Some(r.array(Reader::inline)?),
                "id" => comment_id = Some(r.string()?),
                _ => return None,
            }
            Some(())
        })?;
        match tag.as_deref()? {
            "run" => Some(Inline::Run(run?)),
            "br" => Some(Inline::LineBreak),
            "image" => Some(Inline::Image(img?)),
            "link" => Some(Inline::Link {
                href: href?,
                children: children?,
            }),
            "commentRef" => Some(Inline::CommentRef { id: comment_id? }),
            _ => None,
        }
    }

    fn comment(&mut self) -> Option<Comment> {
        let mut c = Comment::default();
        self.object(|r, k| {
            match k {
                "id" => c.id = r.string()?,
                "author" => c.author = r.string()?,
                "date" => c.date = r.string()?,
                "text" => c.text = r.string()?,
                _ => return None,
            }
            Some(())
        })?;
        Some(c)
    }

    fn inline_run(&mut self) -> Option<InlineRun> {
        let mut r = InlineRun::default();
        let mut src_seen = false;
        self.object(|rd, k| {
            match k {
                "text" => r.text = rd.string()?,
                "style" => r.style = rd.char_style()?,
                "source_index" => {
                    src_seen = true;
                    r.source_index = if rd.peek()? == b'n' {
                        rd.null()?;
                        None
                    } else {
                        Some(rd.usize()?)
                    };
                }
                _ => return None,
            }
            Some(())
        })?;
        let _ = src_seen;
        Some(r)
    }

    fn link_target(&mut self) -> Option<LinkTarget> {
        let mut tag: Option<String> = None;
        let mut sval: Option<String> = None;
        let mut pval: Option<usize> = None;
        self.object(|r, k| {
            match k {
                "t" => tag = Some(r.string()?),
                "v" => match tag.as_deref()? {
                    "url" => sval = Some(r.string()?),
                    "page" => pval = Some(r.usize()?),
                    _ => return None,
                },
                _ => return None,
            }
            Some(())
        })?;
        match tag.as_deref()? {
            "url" => Some(LinkTarget::Url(sval?)),
            "page" => Some(LinkTarget::Page(pval?)),
            _ => None,
        }
    }

    fn list(&mut self) -> Option<List> {
        let mut l = List::default();
        self.object(|r, k| {
            match k {
                "ordered" => l.ordered = r.bool()?,
                "marker" => l.marker = r.list_marker()?,
                "items" => l.items = r.array(Reader::list_item)?,
                _ => return None,
            }
            Some(())
        })?;
        Some(l)
    }

    fn list_marker(&mut self) -> Option<ListMarker> {
        let mut tag: Option<String> = None;
        let mut v: Option<String> = None;
        self.object(|r, k| {
            match k {
                "t" => tag = Some(r.string()?),
                "v" => v = Some(r.string()?),
                _ => return None,
            }
            Some(())
        })?;
        match tag.as_deref()? {
            "bullet" => {
                let s = v?;
                let mut chars = s.chars();
                let c = chars.next()?;
                if chars.next().is_some() {
                    return None; // exactly one char
                }
                Some(ListMarker::Bullet(c))
            }
            "decimal" => Some(ListMarker::Decimal),
            "lower_alpha" => Some(ListMarker::LowerAlpha),
            "upper_alpha" => Some(ListMarker::UpperAlpha),
            "lower_roman" => Some(ListMarker::LowerRoman),
            "upper_roman" => Some(ListMarker::UpperRoman),
            _ => None,
        }
    }

    fn list_item(&mut self) -> Option<ListItem> {
        let mut it = ListItem::default();
        self.object(|r, k| {
            match k {
                "blocks" => it.blocks = r.array(Reader::block)?,
                "level" => it.level = r.u8()?,
                _ => return None,
            }
            Some(())
        })?;
        Some(it)
    }

    fn table(&mut self) -> Option<Table> {
        let mut t = Table::default();
        self.object(|r, k| {
            match k {
                "rows" => t.rows = r.array(Reader::row)?,
                "col_widths" => t.col_widths = r.array(Reader::number)?,
                "border" => t.border = r.border()?,
                _ => return None,
            }
            Some(())
        })?;
        Some(t)
    }

    fn row(&mut self) -> Option<Row> {
        let mut row = Row::default();
        self.object(|r, k| {
            match k {
                "cells" => row.cells = r.array(Reader::cell)?,
                "height" => {
                    row.height = if r.peek()? == b'n' {
                        r.null()?;
                        None
                    } else {
                        Some(r.number()?)
                    };
                }
                "is_header" => row.is_header = r.bool()?,
                _ => return None,
            }
            Some(())
        })?;
        Some(row)
    }

    fn cell(&mut self) -> Option<Cell> {
        let mut c = Cell::default();
        self.object(|r, k| {
            match k {
                "blocks" => c.blocks = r.array(Reader::block)?,
                "col_span" => c.col_span = r.u16()?,
                "row_span" => c.row_span = r.u16()?,
                "shading" => c.shading = r.opt_rgb()?,
                "vertical_align" => {
                    c.vertical_align = match r.opt_string()? {
                        Some(s) => Some(parse_cell_valign(&s)?),
                        None => None,
                    };
                }
                _ => return None,
            }
            Some(())
        })?;
        Some(c)
    }

    fn border(&mut self) -> Option<BorderStyle> {
        let mut b = BorderStyle::default();
        self.object(|r, k| {
            match k {
                "width" => b.width = r.number()?,
                "color" => b.color = r.rgb()?,
                _ => return None,
            }
            Some(())
        })?;
        Some(b)
    }

    /// `null` ⇒ `None`; an object ⇒ `Some(border)`.
    fn opt_border(&mut self) -> Option<Option<BorderStyle>> {
        if self.peek()? == b'n' {
            self.null()?;
            Some(None)
        } else {
            Some(Some(self.border()?))
        }
    }

    fn image_ref(&mut self) -> Option<ImageRef> {
        let mut i = ImageRef::default();
        self.object(|r, k| {
            match k {
                "resource" => i.resource = r.u64()?,
                "alt" => i.alt = r.opt_string()?,
                _ => return None,
            }
            Some(())
        })?;
        Some(i)
    }

    fn shape(&mut self) -> Option<Shape> {
        let mut s = Shape::default();
        self.object(|r, k| {
            match k {
                "segments" => s.segments = r.array(Reader::path_seg)?,
                "fill" => s.fill = r.opt_rgb()?,
                "stroke" => s.stroke = r.opt_rgb()?,
                "stroke_width" => s.stroke_width = r.number()?,
                "dash" => s.dash = r.array(Reader::number)?,
                _ => return None,
            }
            Some(())
        })?;
        Some(s)
    }

    fn path_seg(&mut self) -> Option<PathSeg> {
        let mut tag: Option<String> = None;
        let (mut x, mut y) = (None, None);
        let (mut x1, mut y1, mut x2, mut y2) = (None, None, None, None);
        self.object(|r, k| {
            match k {
                "t" => tag = Some(r.string()?),
                "x" => x = Some(r.number()?),
                "y" => y = Some(r.number()?),
                "x1" => x1 = Some(r.number()?),
                "y1" => y1 = Some(r.number()?),
                "x2" => x2 = Some(r.number()?),
                "y2" => y2 = Some(r.number()?),
                _ => return None,
            }
            Some(())
        })?;
        match tag.as_deref()? {
            "m" => Some(PathSeg::Move(x?, y?)),
            "l" => Some(PathSeg::Line(x?, y?)),
            "c" => Some(PathSeg::Cubic(x1?, y1?, x2?, y2?, x?, y?)),
            "z" => Some(PathSeg::Close),
            _ => None,
        }
    }

    fn text_box(&mut self) -> Option<TextBox> {
        let mut tb = TextBox::default();
        self.object(|r, k| {
            match k {
                "blocks" => tb.blocks = r.array(Reader::block)?,
                _ => return None,
            }
            Some(())
        })?;
        Some(tb)
    }

    fn sheet_block(&mut self) -> Option<SheetBlock> {
        let mut sb = SheetBlock::default();
        self.object(|r, k| {
            match k {
                "sheets" => sb.sheets = r.array(Reader::sheet)?,
                _ => return None,
            }
            Some(())
        })?;
        Some(sb)
    }

    fn sheet(&mut self) -> Option<Sheet> {
        let mut s = Sheet::default();
        self.object(|r, k| {
            match k {
                "name" => s.name = r.string()?,
                "rows" => s.rows = r.array(Reader::sheet_row)?,
                "merges" => s.merges = r.array(Reader::merge_range)?,
                "col_widths" => s.col_widths = r.array(Reader::number)?,
                _ => return None,
            }
            Some(())
        })?;
        Some(s)
    }

    fn sheet_row(&mut self) -> Option<SheetRow> {
        let mut row = SheetRow::default();
        self.object(|r, k| {
            match k {
                "cells" => row.cells = r.array(Reader::sheet_cell)?,
                "height" => {
                    row.height = if r.peek()? == b'n' {
                        r.null()?;
                        None
                    } else {
                        Some(r.number()?)
                    };
                }
                _ => return None,
            }
            Some(())
        })?;
        Some(row)
    }

    fn sheet_cell(&mut self) -> Option<SheetCell> {
        let mut c = SheetCell::default();
        self.object(|r, k| {
            match k {
                "value" => c.value = r.cell_value()?,
                "formula" => c.formula = r.opt_string()?,
                "number_format" => c.number_format = r.opt_string()?,
                "fill" => c.fill = r.opt_rgb()?,
                "style" => c.style = r.char_style()?,
                "border" => c.border = r.opt_border()?,
                "align" => {
                    c.align = match r.opt_string()? {
                        Some(s) => Some(parse_align(&s)?),
                        None => None,
                    };
                }
                "vertical_align" => {
                    c.vertical_align = match r.opt_string()? {
                        Some(s) => Some(parse_cell_valign(&s)?),
                        None => None,
                    };
                }
                "wrap" => c.wrap = r.bool()?,
                "hyperlink" => c.hyperlink = r.opt_string()?,
                _ => return None,
            }
            Some(())
        })?;
        Some(c)
    }

    fn cell_value(&mut self) -> Option<CellValue> {
        let mut tag: Option<String> = None;
        let mut sval: Option<String> = None;
        let mut nval: Option<f64> = None;
        let mut bval: Option<bool> = None;
        self.object(|r, k| {
            match k {
                "t" => tag = Some(r.string()?),
                "v" => match tag.as_deref()? {
                    "text" => sval = Some(r.string()?),
                    "number" => nval = Some(r.number()?),
                    "bool" => bval = Some(r.bool()?),
                    _ => return None,
                },
                _ => return None,
            }
            Some(())
        })?;
        match tag.as_deref()? {
            "empty" => Some(CellValue::Empty),
            "text" => Some(CellValue::Text(sval?)),
            "number" => Some(CellValue::Number(nval?)),
            "bool" => Some(CellValue::Bool(bval?)),
            _ => None,
        }
    }

    fn merge_range(&mut self) -> Option<MergeRange> {
        let mut m = MergeRange::default();
        self.object(|r, k| {
            match k {
                "r0" => m.r0 = r.usize()?,
                "c0" => m.c0 = r.usize()?,
                "r1" => m.r1 = r.usize()?,
                "c1" => m.c1 = r.usize()?,
                _ => return None,
            }
            Some(())
        })?;
        Some(m)
    }

    fn slide_block(&mut self) -> Option<SlideBlock> {
        let mut sb = SlideBlock::default();
        self.object(|r, k| {
            match k {
                "slides" => sb.slides = r.array(Reader::slide)?,
                _ => return None,
            }
            Some(())
        })?;
        Some(sb)
    }

    fn slide(&mut self) -> Option<Slide> {
        let mut s = Slide::default();
        self.object(|r, k| {
            match k {
                "geometry" => s.geometry = r.geometry()?,
                "shapes" => s.shapes = r.array(Reader::block)?,
                "placeholders" => s.placeholders = r.array(Reader::placeholder)?,
                "notes" => s.notes = r.opt_blocks()?,
                "background" => s.background = r.opt_rgb()?,
                _ => return None,
            }
            Some(())
        })?;
        Some(s)
    }

    fn placeholder(&mut self) -> Option<Placeholder> {
        let mut role: Option<PlaceholderRole> = None;
        let mut block: Option<Block> = None;
        self.object(|r, k| {
            match k {
                "role" => role = Some(r.placeholder_role()?),
                "block" => block = Some(r.block()?),
                _ => return None,
            }
            Some(())
        })?;
        Some(Placeholder {
            role: role?,
            block: block?,
        })
    }

    fn placeholder_role(&mut self) -> Option<PlaceholderRole> {
        let mut tag: Option<String> = None;
        let mut v: Option<String> = None;
        self.object(|r, k| {
            match k {
                "t" => tag = Some(r.string()?),
                "v" => v = Some(r.string()?),
                _ => return None,
            }
            Some(())
        })?;
        match tag.as_deref()? {
            "title" => Some(PlaceholderRole::Title),
            "subtitle" => Some(PlaceholderRole::Subtitle),
            "body" => Some(PlaceholderRole::Body),
            "other" => Some(PlaceholderRole::Other(v?)),
            _ => None,
        }
    }

    fn outline_node(&mut self) -> Option<OutlineNode> {
        let mut n = OutlineNode::default();
        self.object(|r, k| {
            match k {
                "title" => n.title = r.string()?,
                "page" => n.page = r.usize()?,
                "children" => n.children = r.array(Reader::outline_node)?,
                _ => return None,
            }
            Some(())
        })?;
        Some(n)
    }

    fn resources(&mut self) -> Option<ResourceTable> {
        let mut t = ResourceTable::default();
        self.object(|r, k| {
            match k {
                "images" => {
                    r.object(|r2, id| {
                        let key: u64 = id.parse().ok()?;
                        let img = r2.image_resource()?;
                        t.images.insert(key, img);
                        Some(())
                    })?;
                }
                _ => return None,
            }
            Some(())
        })?;
        Some(t)
    }

    fn image_resource(&mut self) -> Option<ImageResource> {
        let mut img = ImageResource::default();
        self.object(|r, k| {
            match k {
                "bytes" => img.bytes = base64_decode(&r.string()?)?,
                "format" => img.format = r.string()?,
                _ => return None,
            }
            Some(())
        })?;
        Some(img)
    }
}

// ── enum tag helpers (parse) ──────────────────────────────────────────────────

fn parse_align(s: &str) -> Option<Align> {
    match s {
        "left" => Some(Align::Left),
        "center" => Some(Align::Center),
        "right" => Some(Align::Right),
        "justify" => Some(Align::Justify),
        _ => None,
    }
}
fn parse_valign(s: &str) -> Option<VAlign> {
    match s {
        "baseline" => Some(VAlign::Baseline),
        "super" => Some(VAlign::Super),
        "sub" => Some(VAlign::Sub),
        _ => None,
    }
}

fn parse_cell_valign(s: &str) -> Option<CellVAlign> {
    match s {
        "top" => Some(CellVAlign::Top),
        "middle" => Some(CellVAlign::Middle),
        "bottom" => Some(CellVAlign::Bottom),
        _ => None,
    }
}
fn parse_generic(s: &str) -> Option<Generic> {
    match s {
        "sans" => Some(Generic::Sans),
        "serif" => Some(Generic::Serif),
        "mono" => Some(Generic::Mono),
        _ => None,
    }
}

/// Decode standard Base64 (RFC 4648), inverse of [`base64`]. Ignores nothing —
/// rejects stray characters and bad padding by returning `None`.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let b = s.as_bytes();
    if !b.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(b.len() / 4 * 3);
    for chunk in b.chunks(4) {
        let pad = chunk.iter().filter(|&&c| c == b'=').count();
        if pad > 2 {
            return None;
        }
        // Padding only at the very end.
        if pad > 0 && !std::ptr::eq(chunk.as_ptr(), b[b.len() - 4..].as_ptr()) {
            return None;
        }
        let c0 = val(chunk[0])?;
        let c1 = val(chunk[1])?;
        let n = ((c0 as u32) << 18) | ((c1 as u32) << 12);
        if pad == 2 {
            // `xx==` → 1 byte; trailing bits of c1 must be zero.
            if c1 & 0x0F != 0 {
                return None;
            }
            out.push((n >> 16) as u8);
        } else if pad == 1 {
            // `xxx=` → 2 bytes; trailing bits of c2 must be zero.
            let c2 = val(chunk[2])?;
            if c2 & 0x03 != 0 {
                return None;
            }
            let n = n | ((c2 as u32) << 6);
            out.push((n >> 16) as u8);
            out.push((n >> 8) as u8);
        } else {
            let c2 = val(chunk[2])?;
            let c3 = val(chunk[3])?;
            let n = n | ((c2 as u32) << 6) | c3 as u32;
            out.push((n >> 16) as u8);
            out.push((n >> 8) as u8);
            out.push(n as u8);
        }
    }
    Some(out)
}

// ───────────────────────── tests ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a non-trivial document touching: a Section + Page with a Heading
    /// and a styled two-run Paragraph, a 2×2 Table, a Sheet with typed cells,
    /// and a Slide carrying a titled placeholder. Plus meta, named styles, a
    /// nested outline, an image resource, a Shape, a List, a Link, and the
    /// Option/None/empty cases.
    fn sample_doc() -> Document {
        let blue = Some([0.1, 0.2, 0.3]);

        let run_a = Inline::Run(InlineRun {
            text: "Hello, ".to_string(),
            style: CharStyle {
                family: "Helvetica".to_string(),
                generic: Generic::Sans,
                size_pt: 12.0,
                bold: true,
                italic: false,
                underline: false,
                strike: false,
                color: None,
                background: None,
                vertical_align: VAlign::Baseline,
            },
            source_index: Some(0),
        });
        let run_b = Inline::Run(InlineRun {
            text: "café — 😀 \"quoted\"\n".to_string(),
            style: CharStyle {
                family: "Times New Roman".to_string(),
                generic: Generic::Serif,
                size_pt: 12.5,
                bold: false,
                italic: true,
                underline: true,
                strike: true,
                color: blue,
                background: Some([1.0, 1.0, 0.0]),
                vertical_align: VAlign::Super,
            },
            source_index: None,
        });
        let link = Inline::Link {
            href: LinkTarget::Page(2),
            children: vec![
                Inline::Run(InlineRun {
                    text: "see page".to_string(),
                    style: CharStyle::default(),
                    source_index: None,
                }),
                Inline::LineBreak,
            ],
        };

        let paragraph = Paragraph {
            style: ParagraphStyle {
                align: Align::Justify,
                space_before_pt: 6.0,
                space_after_pt: 0.0,
                indent_left_pt: 18.0,
                indent_right_pt: 0.0,
                first_line_pt: -9.0,
                line_height: LineHeight::Multiple(1.5),
                ..Default::default()
            },
            style_ref: Some(StyleId("Body".to_string())),
            runs: vec![
                run_a,
                run_b,
                link,
                Inline::Image(ImageRef {
                    resource: 7,
                    alt: Some("logo".to_string()),
                }),
            ],
        };

        let heading = Heading {
            level: 2,
            para: Paragraph {
                style: ParagraphStyle {
                    align: Align::Center,
                    line_height: LineHeight::Points(20.0),
                    ..Default::default()
                },
                style_ref: None,
                runs: vec![Inline::Run(InlineRun {
                    text: "Title".to_string(),
                    style: CharStyle {
                        family: "Arial".to_string(),
                        size_pt: 18.0,
                        ..Default::default()
                    },
                    source_index: None,
                })],
            },
        };

        // 2×2 table; cells carry nested paragraphs, spans, and an optional shade.
        let mk_cell =
            |text: &str, shade: Option<[f64; 3]>, span: u16, va: Option<CellVAlign>| Cell {
                blocks: vec![Block {
                    id: BlockId(100),
                    frame: None,
                    rotation: Rotation::D0,
                    kind: BlockKind::Paragraph(Paragraph {
                        runs: vec![Inline::Run(InlineRun {
                            text: text.to_string(),
                            style: CharStyle::default(),
                            source_index: None,
                        })],
                        ..Default::default()
                    }),
                }],
                col_span: span,
                row_span: 1,
                shading: shade,
                vertical_align: va,
            };
        let table = Table {
            rows: vec![
                Row {
                    cells: vec![
                        mk_cell("r0c0", Some([0.9, 0.9, 0.9]), 1, Some(CellVAlign::Middle)),
                        mk_cell("r0c1", None, 1, None),
                    ],
                    height: Some(24.0),
                    // Header row so the round-trip test exercises `is_header`.
                    is_header: true,
                },
                Row {
                    cells: vec![mk_cell("r1c0", None, 2, Some(CellVAlign::Bottom))],
                    height: None,
                    is_header: false,
                },
            ],
            col_widths: vec![120.0, 80.5],
            border: BorderStyle {
                width: 0.75,
                color: [0.0, 0.0, 0.0],
            },
        };

        let list = List {
            ordered: true,
            marker: ListMarker::Bullet('★'),
            items: vec![
                ListItem {
                    blocks: vec![Block {
                        id: BlockId(200),
                        frame: Some(Rect::new(1.0, 2.0, 3.0, 4.0)),
                        rotation: Rotation::Deg(33.25),
                        kind: BlockKind::Paragraph(Paragraph::default()),
                    }],
                    level: 0,
                },
                ListItem {
                    blocks: vec![],
                    level: 1,
                },
            ],
        };

        let shape = Shape {
            segments: vec![
                PathSeg::Move(0.0, 0.0),
                PathSeg::Line(10.0, 0.0),
                PathSeg::Cubic(10.0, 5.0, 5.0, 10.0, 0.0, 10.0),
                PathSeg::Close,
            ],
            fill: Some([1.0, 0.5, 0.25]),
            stroke: None,
            stroke_width: 1.5,
            dash: vec![3.0, 2.0],
        };

        let sheet = SheetBlock {
            sheets: vec![Sheet {
                name: "Données".to_string(),
                rows: vec![
                    SheetRow {
                        cells: vec![
                            SheetCell {
                                value: CellValue::Text("Name".to_string()),
                                formula: None,
                                number_format: None,
                                fill: Some([0.8, 0.8, 1.0]),
                                style: CharStyle {
                                    bold: true,
                                    ..Default::default()
                                },
                                border: Some(BorderStyle {
                                    width: 1.0,
                                    color: [0.2, 0.2, 0.2],
                                }),
                                align: Some(Align::Center),
                                vertical_align: Some(CellVAlign::Middle),
                                wrap: true,
                                hyperlink: Some("https://example.com/".to_string()),
                            },
                            SheetCell {
                                value: CellValue::Number(1234.56),
                                formula: Some("SUM(A1:A9)".to_string()),
                                number_format: Some("0.00".to_string()),
                                fill: None,
                                style: CharStyle::default(),
                                align: Some(Align::Right),
                                ..Default::default()
                            },
                        ],
                        height: Some(18.0),
                    },
                    SheetRow {
                        cells: vec![
                            SheetCell {
                                value: CellValue::Bool(true),
                                number_format: None,
                                fill: None,
                                style: CharStyle::default(),
                                ..Default::default()
                            },
                            SheetCell {
                                value: CellValue::Empty,
                                number_format: None,
                                fill: None,
                                style: CharStyle::default(),
                                ..Default::default()
                            },
                        ],
                        height: None,
                    },
                ],
                merges: vec![MergeRange {
                    r0: 0,
                    c0: 0,
                    r1: 0,
                    c1: 1,
                }],
                col_widths: vec![64.0, 64.0],
            }],
        };

        let slide = SlideBlock {
            slides: vec![Slide {
                geometry: PageGeometry {
                    width: 960.0,
                    height: 540.0,
                    margins: Margins::uniform(36.0),
                
                ..Default::default()
},
                shapes: vec![Block {
                    id: BlockId(300),
                    frame: Some(Rect::new(50.0, 50.0, 400.0, 100.0)),
                    rotation: Rotation::D0,
                    kind: BlockKind::TextBox(TextBox {
                        blocks: vec![Block {
                            id: BlockId(301),
                            frame: None,
                            rotation: Rotation::D0,
                            kind: BlockKind::Paragraph(Paragraph {
                                runs: vec![Inline::Run(InlineRun {
                                    text: "Box".to_string(),
                                    style: CharStyle::default(),
                                    source_index: None,
                                })],
                                ..Default::default()
                            }),
                        }],
                    }),
                }],
                placeholders: vec![Placeholder {
                    role: PlaceholderRole::Title,
                    block: Block {
                        id: BlockId(310),
                        frame: None,
                        rotation: Rotation::D0,
                        kind: BlockKind::Heading(Heading {
                            level: 1,
                            para: Paragraph {
                                runs: vec![Inline::Run(InlineRun {
                                    text: "Deck Title".to_string(),
                                    style: CharStyle::default(),
                                    source_index: None,
                                })],
                                ..Default::default()
                            },
                        }),
                    },
                }],
                notes: Some(vec![Block {
                    id: BlockId(320),
                    frame: None,
                    rotation: Rotation::D0,
                    kind: BlockKind::Paragraph(Paragraph::default()),
                }]),
                background: Some([0.125, 0.219, 0.392]),
            }],
        };

        let page = Page {
            blocks: vec![
                Block {
                    id: BlockId(1),
                    frame: None,
                    rotation: Rotation::D90,
                    kind: BlockKind::Heading(heading),
                },
                Block {
                    id: BlockId(2),
                    frame: Some(Rect::new(40.0, 700.0, 515.0, 60.0)),
                    rotation: Rotation::D0,
                    kind: BlockKind::Paragraph(paragraph),
                },
                Block {
                    id: BlockId(3),
                    frame: None,
                    rotation: Rotation::D0,
                    kind: BlockKind::Table(table),
                },
                Block {
                    id: BlockId(4),
                    frame: None,
                    rotation: Rotation::D180,
                    kind: BlockKind::List(list),
                },
                Block {
                    id: BlockId(5),
                    frame: None,
                    rotation: Rotation::D0,
                    kind: BlockKind::Shape(shape),
                },
                Block {
                    id: BlockId(6),
                    frame: None,
                    rotation: Rotation::D0,
                    kind: BlockKind::Sheet(sheet),
                },
                Block {
                    id: BlockId(7),
                    frame: None,
                    rotation: Rotation::D0,
                    kind: BlockKind::Slide(slide),
                },
                Block {
                    id: BlockId(8),
                    frame: None,
                    rotation: Rotation::D0,
                    kind: BlockKind::Image(ImageRef {
                        resource: 7,
                        alt: None,
                    }),
                },
                Block {
                    id: BlockId(9),
                    frame: None,
                    rotation: Rotation::D0,
                    kind: BlockKind::CodeBlock(crate::model::CodeBlock {
                        lang: Some("rust".to_string()),
                        code: "fn main() {\n    println!(\"héllo `~` ```\");\n}".to_string(),
                    }),
                },
                Block {
                    id: BlockId(10),
                    frame: None,
                    rotation: Rotation::D0,
                    kind: BlockKind::Blockquote(crate::model::Blockquote {
                        blocks: vec![
                            Block {
                                id: BlockId(1001),
                                frame: None,
                                rotation: Rotation::D0,
                                kind: BlockKind::Paragraph(Paragraph {
                                    runs: vec![Inline::Run(InlineRun {
                                        text: "quoted line".to_string(),
                                        style: CharStyle::default(),
                                        source_index: None,
                                    })],
                                    ..Default::default()
                                }),
                            },
                            Block {
                                id: BlockId(1002),
                                frame: None,
                                rotation: Rotation::D0,
                                kind: BlockKind::HorizontalRule,
                            },
                        ],
                    }),
                },
                Block {
                    id: BlockId(11),
                    frame: None,
                    rotation: Rotation::D0,
                    kind: BlockKind::HorizontalRule,
                },
            ],
            absolute: false,
        };

        let mut named = std::collections::BTreeMap::new();
        named.insert(
            StyleId("Body".to_string()),
            NamedStyle {
                para: ParagraphStyle {
                    align: Align::Left,
                    ..Default::default()
                },
                char_: CharStyle {
                    family: "Helvetica".to_string(),
                    size_pt: 11.0,
                    ..Default::default()
                },
                based_on: None,
            },
        );
        named.insert(
            StyleId("Heading1".to_string()),
            NamedStyle {
                para: ParagraphStyle {
                    align: Align::Center,
                    ..Default::default()
                },
                char_: CharStyle {
                    family: "Arial".to_string(),
                    size_pt: 24.0,
                    bold: true,
                    ..Default::default()
                },
                based_on: Some(StyleId("Body".to_string())),
            },
        );

        let mut images = std::collections::BTreeMap::new();
        images.insert(
            7u64,
            ImageResource {
                bytes: vec![0u8, 1, 2, 254, 255, 137, 80, 78, 71],
                format: "png".to_string(),
            },
        );
        images.insert(
            42u64,
            ImageResource {
                bytes: vec![],
                format: "jpeg".to_string(),
            },
        );

        Document {
            meta: DocMeta {
                title: Some("Round-Trip Test".to_string()),
                author: None,
                subject: Some("model/json".to_string()),
                keywords: vec!["pdf".to_string(), "café".to_string(), "🎯".to_string()],
                lang: Some("en".to_string()),
                // Extended metadata must also survive the JSON round-trip.
                description: "A round-trip fixture".to_string(),
                created: "2020-01-01T00:00:00Z".to_string(),
                modified: "2020-12-31T23:59:59Z".to_string(),
                last_modified_by: "Tester".to_string(),
                revision: "2".to_string(),
                application: "GigaPDF".to_string(),
                company: "ACME".to_string(),
                generator: "model/json".to_string(),
                editing_cycles: "5".to_string(),
            },
            styles: StyleTable { named },
            sections: vec![Section {
                geometry: PageGeometry {
                    width: 595.27,
                    height: 841.89,
                    margins: Margins::symmetric(72.0, 54.0),
                
                ..Default::default()
},
                header: Some(vec![Block {
                    id: BlockId(900),
                    frame: None,
                    rotation: Rotation::D0,
                    kind: BlockKind::Paragraph(Paragraph {
                        runs: vec![
                            Inline::Run(InlineRun {
                                text: "Header".to_string(),
                                style: CharStyle::default(),
                                source_index: None,
                            }),
                            // A comment anchor must survive the JSON round trip.
                            Inline::CommentRef {
                                id: "c1".to_string(),
                            },
                        ],
                        ..Default::default()
                    }),
                }]),
                footer: None,
                pages: vec![page],
            }],
            outline: vec![OutlineNode {
                title: "Chapter 1".to_string(),
                page: 0,
                children: vec![
                    OutlineNode {
                        title: "1.1".to_string(),
                        page: 0,
                        children: vec![],
                    },
                    OutlineNode {
                        title: "1.2 — détails".to_string(),
                        page: 1,
                        children: vec![],
                    },
                ],
            }],
            resources: ResourceTable { images },
            // A review comment matching the header's `Inline::CommentRef` anchor;
            // exercises the comments round trip alongside the structural fields.
            comments: vec![Comment {
                id: "c1".to_string(),
                author: "Reviewer".to_string(),
                date: "2026-06-24T10:00:00Z".to_string(),
                text: "Round-trip me — café 🎯".to_string(),
            }],
        }
    }

    #[test]
    fn round_trip_structural_equality() {
        let doc = sample_doc();
        let json = doc.to_json();
        let back = Document::from_json(&json).expect("parse must succeed");
        assert_eq!(doc, back, "model must survive a JSON round trip unchanged");
    }

    #[test]
    fn round_trip_is_idempotent() {
        // to_json ∘ from_json ∘ to_json yields the identical text.
        let doc = sample_doc();
        let j1 = doc.to_json();
        let j2 = Document::from_json(&j1).unwrap().to_json();
        assert_eq!(j1, j2, "serialization is stable across a round trip");
    }

    #[test]
    fn empty_document_round_trips() {
        let doc = Document::default();
        let back = Document::from_json(&doc.to_json()).unwrap();
        assert_eq!(doc, back);
    }

    #[test]
    fn f64_precision_preserved() {
        // Odd, full-precision floats must survive exactly (shortest round-trip).
        let mut doc = Document::default();
        doc.sections.push(Section {
            geometry: PageGeometry {
                width: 595.2755905511812,
                height: 0.1 + 0.2, // 0.30000000000000004
                margins: Margins {
                    top: -12.345_678_9,
                    right: 1e-9,
                    bottom: 1234567.89,
                    left: 0.0,
                },
            
            ..Default::default()
},
            header: None,
            footer: None,
            pages: vec![],
        });
        let back = Document::from_json(&doc.to_json()).unwrap();
        assert_eq!(doc, back);
        let g = back.sections[0].geometry;
        assert_eq!(g.height, 0.1 + 0.2);
        assert_eq!(g.margins.top, -12.345_678_9);
        assert_eq!(g.margins.right, 1e-9);
    }

    #[test]
    fn rejects_malformed() {
        assert!(Document::from_json("").is_none());
        assert!(Document::from_json("{}").is_none(), "missing version");
        assert!(Document::from_json(r#"{"v":2}"#).is_none(), "wrong version");
        // Valid doc + trailing junk must be rejected.
        let good = Document::default().to_json();
        assert!(Document::from_json(&format!("{good} junk")).is_none());
        // Unknown top-level key.
        assert!(Document::from_json(r#"{"v":1,"bogus":1}"#).is_none());
    }

    #[test]
    fn base64_decode_inverts_encode() {
        for sample in [
            &b""[..],
            b"f",
            b"fo",
            b"foo",
            b"foob",
            b"fooba",
            b"foobar",
            &[0, 255, 13, 10, 0x80],
        ] {
            assert_eq!(base64_decode(&base64(sample)).unwrap(), sample);
        }
        assert!(base64_decode("abc").is_none(), "length not a multiple of 4");
        assert!(base64_decode("====").is_none(), "too much padding");
        assert!(base64_decode("a@==").is_none(), "illegal char");
    }
}
