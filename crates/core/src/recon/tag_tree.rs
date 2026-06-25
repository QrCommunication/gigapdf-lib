//! Stage 0 — **tagged-PDF structure**. When the catalog carries a
//! `/StructTreeRoot` (ISO 32000-1 §14.7), the author already declared the
//! logical structure; we trust it over the geometric heuristics. We walk the
//! structure tree, map each standard structure type to a model construct
//! (`/P`→Paragraph, `/H1..6`→Heading, `/L /LI`→List, `/Table /TR /TH /TD`→Table,
//! `/Figure`→Image placeholder) and bind the marked-content each leaf owns
//! (`/K` → MCID) back to the page's decoded glyph runs.
//!
//! Binding is done by a self-contained marked-content scan of each page's
//! content stream: `BDC … EMC` ranges tagged with an `/MCID` accumulate their
//! `Tj`/`TJ` text, keyed by `(page, mcid)`. A structure element's `/K` MCID
//! references then pull that text. When the document has **no** struct tree —
//! or the tree yields no bound text — this returns `None` and the caller falls
//! back to the heuristic pipeline.

use std::collections::BTreeMap;

use super::{char_style, IdGen};
use crate::content::{parse_content, FontDecoders};
use crate::convert::style::TextStyle;
use crate::font::cmap::TextDecoder;
use crate::model::{
    geom::{Rect, Rotation},
    Block, BlockKind, Cell, Heading, Inline, InlineRun, List, ListItem, ListMarker, Paragraph,
    ParagraphStyle, Row, Table,
};
use crate::object::{Dictionary, Object};

/// The model construct a standard structure type maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StructRole {
    /// Paragraph (`/P`, `/Note`, `/Quote`, …).
    Paragraph,
    /// Heading at the given level 1..=6 (`/H1`..`/H6`, or a generic `/H` → 1).
    Heading(u8),
    /// List container (`/L`).
    List,
    /// List item (`/LI`, `/Lbl`, `/LBody`).
    ListItem,
    /// Table container (`/Table`).
    Table,
    /// Table row (`/TR`).
    TableRow,
    /// Table cell (`/TD`, `/TH`).
    TableCell,
    /// Figure / image (`/Figure`).
    Figure,
}

/// Map a standard structure-type tag to a [`StructRole`]. Unknown / grouping
/// tags (`/Document`, `/Part`, `/Sect`, `/Div`, `/Art`, `/Span`, …) return
/// `None` so the walker recurses through them without emitting a block.
pub fn structure_role(tag: &[u8]) -> Option<StructRole> {
    match tag {
        b"P" | b"Note" | b"Quote" | b"BlockQuote" | b"Caption" => Some(StructRole::Paragraph),
        b"H" => Some(StructRole::Heading(1)),
        b"H1" => Some(StructRole::Heading(1)),
        b"H2" => Some(StructRole::Heading(2)),
        b"H3" => Some(StructRole::Heading(3)),
        b"H4" => Some(StructRole::Heading(4)),
        b"H5" => Some(StructRole::Heading(5)),
        b"H6" => Some(StructRole::Heading(6)),
        b"L" => Some(StructRole::List),
        b"LI" | b"Lbl" | b"LBody" => Some(StructRole::ListItem),
        b"Table" => Some(StructRole::Table),
        b"TR" => Some(StructRole::TableRow),
        b"TD" | b"TH" => Some(StructRole::TableCell),
        b"Figure" | b"Formula" => Some(StructRole::Figure),
        _ => None,
    }
}

// ── structure-element attributes (`/A`, ISO 32000-1 §14.8.5.2) ───────────────
//
// A structure element's layout/table/list properties live in its `/A` entry: a
// single attribute dictionary, or an array of them (each optionally followed by
// a revision-number integer we ignore). Every attribute dict carries an `/O`
// (owner) name — `/Table` owns `/ColSpan`/`/RowSpan`, `/List` owns
// `/ListNumbering`, `/Layout` owns `/BBox`. These helpers are pure over a
// resolver closure so they can be unit-tested with hand-built dictionaries.

/// Yield each attribute dictionary in an element's `/A`, in order. Resolves an
/// indirect `/A`, flattens a single dict or an array (skipping the interleaved
/// revision-number integers of the `[dict rev dict rev …]` form).
fn attribute_dicts<'a, R>(elem: &'a Dictionary, resolve: &R) -> Vec<&'a Dictionary>
where
    R: Fn(&'a Object) -> &'a Object,
{
    let Some(a) = elem.get(b"A").map(resolve) else {
        return Vec::new();
    };
    match a {
        Object::Dictionary(d) => vec![d],
        Object::Array(items) => items
            .iter()
            .filter_map(|i| match resolve(i) {
                Object::Dictionary(d) => Some(d),
                _ => None, // revision-number integer (or anything else): skip
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The value of `key` in the first attribute dict owned by `owner` (its `/O`
/// name equals `owner`). A dict with no `/O` is treated as a wildcard match so
/// minimally-tagged producers (attributes without an explicit owner) still
/// resolve.
fn attribute_value<'a, R>(
    elem: &'a Dictionary,
    owner: &[u8],
    key: &[u8],
    resolve: &R,
) -> Option<&'a Object>
where
    R: Fn(&'a Object) -> &'a Object,
{
    for dict in attribute_dicts(elem, resolve) {
        let dict_owner = dict.get(b"O").map(resolve).and_then(Object::as_name);
        if matches!(dict_owner, Some(o) if o != owner) {
            continue; // explicit, non-matching owner
        }
        if let Some(v) = dict.get(key) {
            return Some(resolve(v));
        }
    }
    None
}

/// The `(col_span, row_span)` of a `/TD`/`/TH` cell element from its `/Table`
/// `/ColSpan`/`/RowSpan` attributes (default 1 each; clamped to ≥1).
fn cell_spans<'a, R>(elem: &'a Dictionary, resolve: &R) -> (u16, u16)
where
    R: Fn(&'a Object) -> &'a Object,
{
    let span = |key: &[u8]| -> u16 {
        attribute_value(elem, b"Table", key, resolve)
            .and_then(Object::as_i64)
            .filter(|n| *n >= 1)
            .map(|n| n.min(u16::MAX as i64) as u16)
            .unwrap_or(1)
    };
    (span(b"ColSpan"), span(b"RowSpan"))
}

/// The `(ordered, marker)` of an `/L` list element from its `/List`
/// `/ListNumbering` attribute. `None` when the attribute is absent, leaving the
/// caller's unordered-bullet default. `/None` maps to an unordered list; the
/// glyph kinds (`/Disc`/`/Circle`/`/Square`) map to bullet markers; the numeric
/// kinds map to the corresponding ordered [`ListMarker`].
fn list_numbering<'a, R>(elem: &'a Dictionary, resolve: &R) -> Option<(bool, ListMarker)>
where
    R: Fn(&'a Object) -> &'a Object,
{
    let kind = attribute_value(elem, b"List", b"ListNumbering", resolve)?.as_name()?;
    Some(match kind {
        b"Decimal" => (true, ListMarker::Decimal),
        b"UpperRoman" => (true, ListMarker::UpperRoman),
        b"LowerRoman" => (true, ListMarker::LowerRoman),
        b"UpperAlpha" => (true, ListMarker::UpperAlpha),
        b"LowerAlpha" => (true, ListMarker::LowerAlpha),
        b"Disc" => (false, ListMarker::Bullet('•')),
        b"Circle" => (false, ListMarker::Bullet('◦')),
        b"Square" => (false, ListMarker::Bullet('▪')),
        // `/None` (and any non-standard kind) → unordered, default bullet.
        _ => (false, ListMarker::default()),
    })
}

/// The element's bounding box from its `/Layout` `/BBox` attribute
/// (`[llx lly urx ury]` in default user space) as a [`Rect`] (lower-left +
/// width/height). `None` when absent or malformed.
fn layout_bbox<'a, R>(elem: &'a Dictionary, resolve: &R) -> Option<Rect>
where
    R: Fn(&'a Object) -> &'a Object,
{
    let arr = attribute_value(elem, b"Layout", b"BBox", resolve)?.as_array()?;
    if arr.len() != 4 {
        return None;
    }
    let n = |i: usize| resolve(&arr[i]).as_f64();
    let (llx, lly, urx, ury) = (n(0)?, n(1)?, n(2)?, n(3)?);
    Some(Rect::new(
        llx.min(urx),
        lly.min(ury),
        (urx - llx).abs(),
        (ury - lly).abs(),
    ))
}

/// The 0-based page index a structure element's `/Pg` points at: the position
/// of its referenced page object in the document's page-id list. `None` when
/// `/Pg` is absent, not an indirect reference, or doesn't match a page.
fn pg_page_index(elem: &Dictionary, page_ids: &[crate::object::ObjectId]) -> Option<usize> {
    let Some(Object::Reference(id)) = elem.get(b"Pg") else {
        return None;
    };
    page_ids.iter().position(|p| p == id)
}

/// Per-`(page index, MCID)` accumulated text from a page's marked content.
type MarkedText = BTreeMap<(usize, i64), String>;

/// Walk the document's `/StructTreeRoot` into model blocks. `None` when the
/// document is not tagged, or the tree produced no text-bearing blocks (so the
/// caller uses the heuristic reconstruction instead).
///
/// The blocks are returned in document reading order, **flattened** across all
/// pages. The per-block page each one belongs to (resolved from the element's
/// `/Pg`) is available via [`reconstruct_from_struct_tree_paged`]; this entry
/// point drops it for callers that place the whole list on one page.
pub fn reconstruct_from_struct_tree(doc: &crate::Document, ids: &mut IdGen) -> Option<Vec<Block>> {
    let paged = reconstruct_from_struct_tree_paged(doc, ids)?;
    Some(paged.into_iter().map(|(_page, block)| block).collect())
}

/// Like [`reconstruct_from_struct_tree`] but pairs each top-level block with the
/// **0-based page index** it belongs to, resolved from the structure element's
/// `/Pg` (or the first `/Pg` found among its descendants). `None` when the page
/// is not declared/resolvable — the tag tree often omits `/Pg` on grouping
/// elements, so a `None` here means "page unknown from the structure", not page
/// 0. Callers distribute the blocks onto model pages from this hint.
///
/// NOTE: the actual placement of these blocks onto their `Page`s happens in the
/// model-reconstruction driver (`Document::reconstruct_model` in `document.rs`),
/// which currently dumps the flat list on page 1; consuming this page hint there
/// (out of scope for this module) is what finishes the per-page assignment.
pub fn reconstruct_from_struct_tree_paged(
    doc: &crate::Document,
    ids: &mut IdGen,
) -> Option<Vec<(Option<usize>, Block)>> {
    let root = struct_tree_root(doc)?;
    // Collect marked-content text for every page once.
    let marked = collect_all_marked_text(doc);
    if marked.is_empty() {
        return None;
    }

    let mut walker = Walker {
        doc,
        marked: &marked,
        ids,
    };
    // The root's `/K` are the top-level structure elements. `visit` tags every
    // emitted block with the page of the *element that produced it* (its `/Pg`,
    // or the nearest enclosing one), so blocks land on their real page even when
    // a single grouping element (`/Document`) spans several pages.
    let mut paged: Vec<(Option<usize>, Block)> = Vec::new();
    for kid in kids_of(doc, root) {
        walker.visit(&kid, &mut paged, None);
    }
    // If nothing text-bearing came out, the tree was unhelpful — fall back.
    let has_text = paged.iter().any(|(_, b)| block_has_text(b));
    has_text.then_some(paged)
}

/// The `/StructTreeRoot` dictionary, if the catalog references one.
fn struct_tree_root(doc: &crate::Document) -> Option<&Dictionary> {
    let catalog = doc.catalog().ok()?;
    let root = catalog.get(b"StructTreeRoot")?;
    doc.resolve(root).as_dict()
}

/// The `/K` children of a structure element as a flat list of resolved objects
/// (a single kid, an array, or absent → empty).
fn kids_of(doc: &crate::Document, elem: &Dictionary) -> Vec<Object> {
    match elem.get(b"K").map(|k| doc.resolve(k)) {
        Some(Object::Array(items)) => items.iter().map(|i| doc.resolve(i).clone()).collect(),
        Some(other) => vec![other.clone()],
        None => Vec::new(),
    }
}

struct Walker<'a> {
    doc: &'a crate::Document,
    marked: &'a MarkedText,
    ids: &'a mut IdGen,
}

impl Walker<'_> {
    /// Visit a `/K` entry. Structure-element dictionaries map to a role; MCID
    /// integers and marked-content reference dicts are leaves bound to text via
    /// `page_hint` (the page index of the nearest enclosing `/Pg`). Each emitted
    /// block is tagged with that resolved page so the caller can place it on the
    /// right model page.
    fn visit(
        &mut self,
        node: &Object,
        out: &mut Vec<(Option<usize>, Block)>,
        page_hint: Option<usize>,
    ) {
        let Some(dict) = node.as_dict() else {
            return; // bare MCID int with no element context — handled by parents
        };
        // The element's page context (`/Pg`) overrides the inherited hint.
        let page = self.page_of(dict).or(page_hint);

        let tag = dict
            .get(b"S")
            .and_then(|o| self.doc.resolve(o).as_name())
            .map(|n| n.to_vec());
        let role = tag.as_deref().and_then(structure_role);

        match role {
            Some(StructRole::Heading(level)) => {
                let text = self.collect_text(dict, page);
                if !text.is_empty() {
                    let frame = self.element_bbox(dict);
                    out.push((page, self.heading_block(level, &text, frame)));
                }
            }
            Some(StructRole::Paragraph) => {
                let text = self.collect_text(dict, page);
                if !text.is_empty() {
                    let frame = self.element_bbox(dict);
                    out.push((page, self.paragraph_block(&text, frame)));
                }
            }
            Some(StructRole::List) => {
                if let Some(block) = self.list_block(dict, page) {
                    out.push((page, block));
                }
            }
            Some(StructRole::Table) => {
                if let Some(block) = self.table_block(dict, page) {
                    out.push((page, block));
                }
            }
            Some(StructRole::Figure) => {
                // No raster binding here; recurse for any caption text.
                for kid in kids_of(self.doc, dict) {
                    self.visit(&kid, out, page);
                }
            }
            // Grouping element (Document/Sect/Div/…) or a stray cell/row/item
            // outside its container: recurse.
            _ => {
                for kid in kids_of(self.doc, dict) {
                    self.visit(&kid, out, page);
                }
            }
        }
    }

    /// Resolve a structure element's `/Pg` to a page index (0-based).
    fn page_of(&self, elem: &Dictionary) -> Option<usize> {
        let page_ids = self.doc.page_ids().ok()?;
        pg_page_index(elem, &page_ids)
    }

    /// Gather all text bound to a leaf element (and its descendants) by walking
    /// its `/K` MCID references against the marked-content map.
    fn collect_text(&self, elem: &Dictionary, page: Option<usize>) -> String {
        let mut s = String::new();
        self.gather_text(elem, page, &mut s);
        s.trim().to_string()
    }

    fn gather_text(&self, elem: &Dictionary, page: Option<usize>, out: &mut String) {
        let page = self.page_of(elem).or(page);
        match elem.get(b"K").map(|k| self.doc.resolve(k)) {
            Some(Object::Integer(mcid)) => self.push_mcid(page, *mcid, out),
            Some(Object::Array(items)) => {
                for item in items {
                    match self.doc.resolve(item) {
                        Object::Integer(mcid) => self.push_mcid(page, *mcid, out),
                        Object::Dictionary(d) => self.gather_text(d, page, out),
                        _ => {}
                    }
                }
            }
            Some(Object::Dictionary(d)) => self.gather_text(d, page, out),
            _ => {}
        }
    }

    fn push_mcid(&self, page: Option<usize>, mcid: i64, out: &mut String) {
        let Some(page) = page else { return };
        if let Some(text) = self.marked.get(&(page, mcid)) {
            let t = text.trim();
            if t.is_empty() {
                return;
            }
            if !out.is_empty() && !out.ends_with(char::is_whitespace) {
                out.push(' ');
            }
            out.push_str(t);
        }
    }

    /// The element's placement box from its `/Layout` `/BBox` attribute, if any.
    fn element_bbox(&self, elem: &Dictionary) -> Option<Rect> {
        layout_bbox(elem, &|o| self.doc.resolve(o))
    }

    fn paragraph_block(&mut self, text: &str, frame: Option<Rect>) -> Block {
        Block {
            id: self.ids.mint(),
            frame,
            rotation: Rotation::D0,
            kind: BlockKind::Paragraph(text_paragraph(text)),
        }
    }

    fn heading_block(&mut self, level: u8, text: &str, frame: Option<Rect>) -> Block {
        Block {
            id: self.ids.mint(),
            frame,
            rotation: Rotation::D0,
            kind: BlockKind::Heading(Heading {
                level: level.clamp(1, 6),
                para: text_paragraph(text),
            }),
        }
    }

    /// Build a [`List`] from an `/L` element: each `/LI` (or its `/LBody`)
    /// becomes a [`ListItem`].
    fn list_block(&mut self, elem: &Dictionary, page: Option<usize>) -> Option<Block> {
        let page = self.page_of(elem).or(page);
        let mut items: Vec<ListItem> = Vec::new();
        for kid in kids_of(self.doc, elem) {
            let Some(d) = kid.as_dict() else { continue };
            let tag = d.get(b"S").and_then(|o| self.doc.resolve(o).as_name());
            if tag == Some(b"LI".as_slice()) {
                let text = self.collect_text(d, page);
                if text.is_empty() {
                    continue;
                }
                items.push(ListItem {
                    blocks: vec![Block {
                        id: self.ids.mint(),
                        frame: None,
                        rotation: Rotation::D0,
                        kind: BlockKind::Paragraph(text_paragraph(&text)),
                    }],
                    level: 0,
                });
            }
        }
        if items.is_empty() {
            return None;
        }
        // `/ListNumbering` (ISO 32000-1 §14.8.5.2, `/List` owner) decides ordered
        // vs unordered and the marker glyph/format; absent ⇒ unordered default.
        let (ordered, marker) = list_numbering(elem, &|o| self.doc.resolve(o))
            .unwrap_or((false, ListMarker::default()));
        let frame = self.element_bbox(elem);
        Some(Block {
            id: self.ids.mint(),
            frame,
            rotation: Rotation::D0,
            kind: BlockKind::List(List {
                ordered,
                marker,
                items,
            }),
        })
    }

    /// Build a [`Table`] from a `/Table` element: each `/TR` is a row, each
    /// `/TD`/`/TH` a cell.
    fn table_block(&mut self, elem: &Dictionary, page: Option<usize>) -> Option<Block> {
        let page = self.page_of(elem).or(page);
        let mut rows: Vec<Row> = Vec::new();
        let mut max_cols = 0usize;
        for tr in kids_of(self.doc, elem) {
            let Some(tr_dict) = tr.as_dict() else {
                continue;
            };
            if self.doc.resolve_name(tr_dict, b"S") != Some(b"TR".to_vec()) {
                continue;
            }
            let mut cells: Vec<Cell> = Vec::new();
            for td in kids_of(self.doc, tr_dict) {
                let Some(td_dict) = td.as_dict() else {
                    continue;
                };
                let tag = self.doc.resolve_name(td_dict, b"S");
                if tag.as_deref() != Some(b"TD") && tag.as_deref() != Some(b"TH") {
                    continue;
                }
                let text = self.collect_text(td_dict, page);
                // `/ColSpan`/`/RowSpan` (`/Table` owner) — without them tagged
                // tables come out as ragged 0-width grids.
                let (col_span, row_span) = cell_spans(td_dict, &|o| self.doc.resolve(o));
                let cell_frame = self.element_bbox(td_dict);
                cells.push(Cell {
                    blocks: vec![Block {
                        id: self.ids.mint(),
                        frame: cell_frame,
                        rotation: Rotation::D0,
                        kind: BlockKind::Paragraph(text_paragraph(&text)),
                    }],
                    col_span,
                    row_span,
                    shading: None,
                    vertical_align: None,
                });
            }
            if cells.is_empty() {
                continue;
            }
            // Logical column count honours horizontal spans (Σ col_span), so a
            // row of two cells where one spans 2 columns is a 3-wide grid.
            let row_cols: usize = cells.iter().map(|c| c.col_span.max(1) as usize).sum();
            max_cols = max_cols.max(row_cols);
            rows.push(Row {
                cells,
                height: None,
            });
        }
        if rows.is_empty() {
            return None;
        }
        let frame = self.element_bbox(elem);
        Some(Block {
            id: self.ids.mint(),
            frame,
            rotation: Rotation::D0,
            kind: BlockKind::Table(Table {
                rows,
                col_widths: vec![0.0; max_cols],
                border: crate::model::BorderStyle {
                    width: 1.0,
                    color: [0.0, 0.0, 0.0],
                },
            }),
        })
    }
}

/// A small extension on [`Document`](crate::Document) for tag-tree walking.
trait ResolveName {
    fn resolve_name(&self, dict: &Dictionary, key: &[u8]) -> Option<Vec<u8>>;
}
impl ResolveName for crate::Document {
    fn resolve_name(&self, dict: &Dictionary, key: &[u8]) -> Option<Vec<u8>> {
        dict.get(key)
            .map(|o| self.resolve(o))
            .and_then(Object::as_name)
            .map(|n| n.to_vec())
    }
}

/// Build a single-run paragraph from plain text (default char style).
fn text_paragraph(text: &str) -> Paragraph {
    let runs = if text.trim().is_empty() {
        Vec::new()
    } else {
        vec![Inline::Run(InlineRun {
            text: text.trim().to_string(),
            style: char_style(&TextStyle::default(), 12.0),
            source_index: None,
        })]
    };
    Paragraph {
        style: ParagraphStyle::default(),
        style_ref: None,
        runs,
    }
}

/// Whether a block (recursively) carries any text — used to decide if the
/// struct-tree walk was fruitful.
fn block_has_text(block: &Block) -> bool {
    match &block.kind {
        BlockKind::Paragraph(p) => paragraph_has_text(p),
        BlockKind::Heading(h) => paragraph_has_text(&h.para),
        BlockKind::List(l) => l.items.iter().any(|i| i.blocks.iter().any(block_has_text)),
        BlockKind::Table(t) => t
            .rows
            .iter()
            .any(|r| r.cells.iter().any(|c| c.blocks.iter().any(block_has_text))),
        _ => false,
    }
}

fn paragraph_has_text(p: &Paragraph) -> bool {
    p.runs.iter().any(|r| match r {
        Inline::Run(run) => !run.text.trim().is_empty(),
        _ => false,
    })
}

// ── marked-content text collection ──────────────────────────────────────────

/// Collect `(page_index, MCID) → text` for every page by scanning each page's
/// content stream for `BDC … EMC` ranges that carry an `/MCID`.
fn collect_all_marked_text(doc: &crate::Document) -> MarkedText {
    let mut out = MarkedText::new();
    for page_no in 1..=doc.page_count() as u32 {
        let Ok(content) = doc.page_content(page_no) else {
            continue;
        };
        let decoders = doc.page_font_decoders(page_no);
        collect_page_marked_text((page_no - 1) as usize, &content, &decoders, &mut out);
    }
    out
}

/// Scan one page's content stream, accumulating text per active `/MCID`. The
/// `BDC`/`BMC` … `EMC` nesting is tracked on a stack; `Tj`/`TJ` text is added to
/// the innermost MCID range in effect (the standard "current marked-content
/// sequence"). `Tf` selects the active font decoder.
fn collect_page_marked_text(
    page: usize,
    content: &[u8],
    decoders: &FontDecoders,
    out: &mut MarkedText,
) {
    let Ok(ops) = parse_content(content) else {
        return;
    };
    let mut mc_stack: Vec<Option<i64>> = Vec::new();
    let mut font: Option<&TextDecoder> = None;
    let default = TextDecoder::winansi();

    for op in &ops {
        match op.operator.as_slice() {
            b"BDC" => mc_stack.push(mcid_of(op)),
            b"BMC" => mc_stack.push(None),
            b"EMC" => {
                mc_stack.pop();
            }
            b"Tf" => {
                font = op
                    .operands
                    .first()
                    .and_then(Object::as_name)
                    .and_then(|name| decoders.get(name));
            }
            b"Tj" | b"TJ" | b"'" | b"\"" => {
                let Some(mcid) = mc_stack.iter().rev().find_map(|m| *m) else {
                    continue;
                };
                let decoder = font.unwrap_or(&default);
                let text = decode_show(&op.operands, decoder);
                if text.is_empty() {
                    continue;
                }
                out.entry((page, mcid)).or_default().push_str(&text);
            }
            _ => {}
        }
    }
}

/// The `/MCID` integer of a `BDC` operation (`/Tag <<… /MCID n …>> BDC` or
/// `/Tag /Props BDC` with a named property dict we can't resolve → `None`).
fn mcid_of(op: &crate::content::Operation) -> Option<i64> {
    // Operands: `/Tag` then either a property dict or a name. We want the MCID
    // from an inline dict.
    op.operands.iter().find_map(|o| match o {
        Object::Dictionary(d) => d.get(b"MCID").and_then(Object::as_i64),
        _ => None,
    })
}

/// Decode the text operands of a show operator with the active font decoder.
fn decode_show(operands: &[Object], decoder: &TextDecoder) -> String {
    let mut text = String::new();
    for operand in operands {
        match operand {
            Object::String(bytes, _) => text.push_str(&decoder.decode(bytes)),
            Object::Array(items) => {
                for item in items {
                    if let Object::String(bytes, _) = item {
                        text.push_str(&decoder.decode(bytes));
                    }
                }
            }
            _ => {}
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_standard_structure_types() {
        assert_eq!(structure_role(b"P"), Some(StructRole::Paragraph));
        assert_eq!(structure_role(b"H1"), Some(StructRole::Heading(1)));
        assert_eq!(structure_role(b"H3"), Some(StructRole::Heading(3)));
        assert_eq!(structure_role(b"H"), Some(StructRole::Heading(1)));
        assert_eq!(structure_role(b"L"), Some(StructRole::List));
        assert_eq!(structure_role(b"LI"), Some(StructRole::ListItem));
        assert_eq!(structure_role(b"Table"), Some(StructRole::Table));
        assert_eq!(structure_role(b"TR"), Some(StructRole::TableRow));
        assert_eq!(structure_role(b"TD"), Some(StructRole::TableCell));
        assert_eq!(structure_role(b"TH"), Some(StructRole::TableCell));
        assert_eq!(structure_role(b"Figure"), Some(StructRole::Figure));
    }

    #[test]
    fn grouping_and_unknown_tags_have_no_role() {
        assert_eq!(structure_role(b"Document"), None);
        assert_eq!(structure_role(b"Sect"), None);
        assert_eq!(structure_role(b"Div"), None);
        assert_eq!(structure_role(b"Span"), None);
        assert_eq!(structure_role(b"Bogus"), None);
    }

    #[test]
    fn mcid_text_accumulates_per_marked_range() {
        // `/P <</MCID 0>> BDC (Hello) Tj EMC  /P <</MCID 1>> BDC (World) Tj EMC`
        let content =
            b"/P <</MCID 0>> BDC BT (Hello) Tj ET EMC /P <</MCID 1>> BDC BT (World) Tj ET EMC";
        let mut out = MarkedText::new();
        let decoders = FontDecoders::new();
        collect_page_marked_text(0, content, &decoders, &mut out);
        assert_eq!(out.get(&(0, 0)).map(String::as_str), Some("Hello"));
        assert_eq!(out.get(&(0, 1)).map(String::as_str), Some("World"));
    }

    #[test]
    fn text_outside_any_mcid_is_ignored() {
        let content = b"BT (loose) Tj ET";
        let mut out = MarkedText::new();
        collect_page_marked_text(0, content, &FontDecoders::new(), &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn text_paragraph_is_empty_for_blank() {
        assert!(text_paragraph("   ").runs.is_empty());
        assert_eq!(text_paragraph("hi").runs.len(), 1);
    }

    // ── structure-element attribute lowering (`/A`) ─────────────────────────

    /// Identity resolver: the hand-built dictionaries below hold no indirect
    /// references, so resolution is a no-op.
    fn ident(o: &Object) -> &Object {
        o
    }

    fn name(s: &str) -> Object {
        Object::Name(s.as_bytes().to_vec())
    }

    /// A structure element with a single attribute dict `<</O owner …entries>>`
    /// in its `/A`.
    fn elem_with_attr(owner: &str, entries: &[(&str, Object)]) -> Dictionary {
        let mut attr = Dictionary::new();
        attr.set(b"O".to_vec(), name(owner));
        for (k, v) in entries {
            attr.set(k.as_bytes().to_vec(), v.clone());
        }
        let mut elem = Dictionary::new();
        elem.set(b"A".to_vec(), Object::Dictionary(attr));
        elem
    }

    #[test]
    fn cell_spans_read_colspan_and_rowspan() {
        // A `/TD` carrying `/ColSpan 2 /RowSpan 3` (the `/Table` owner).
        let td = elem_with_attr(
            "Table",
            &[
                ("ColSpan", Object::Integer(2)),
                ("RowSpan", Object::Integer(3)),
            ],
        );
        assert_eq!(cell_spans(&td, &ident), (2, 3));
    }

    #[test]
    fn cell_spans_default_to_one_when_absent_or_invalid() {
        // No `/A` at all.
        assert_eq!(cell_spans(&Dictionary::new(), &ident), (1, 1));
        // Present but zero / negative ⇒ clamp to the 1 default.
        let td = elem_with_attr(
            "Table",
            &[
                ("ColSpan", Object::Integer(0)),
                ("RowSpan", Object::Integer(-4)),
            ],
        );
        assert_eq!(cell_spans(&td, &ident), (1, 1));
    }

    #[test]
    fn attribute_value_honours_owner_array_and_wildcard() {
        // `/A [ <</O /Layout /BBox …>> <</O /Table /ColSpan 2>> ]` — the value
        // must come from the dict whose `/O` matches the requested owner.
        let mut layout = Dictionary::new();
        layout.set(b"O".to_vec(), name("Layout"));
        layout.set(
            b"BBox".to_vec(),
            Object::Array(vec![
                Object::Integer(1),
                Object::Integer(2),
                Object::Integer(3),
                Object::Integer(4),
            ]),
        );
        let mut table = Dictionary::new();
        table.set(b"O".to_vec(), name("Table"));
        table.set(b"ColSpan".to_vec(), Object::Integer(2));
        let mut elem = Dictionary::new();
        elem.set(
            b"A".to_vec(),
            Object::Array(vec![Object::Dictionary(layout), Object::Dictionary(table)]),
        );
        assert_eq!(
            attribute_value(&elem, b"Table", b"ColSpan", &ident).and_then(Object::as_i64),
            Some(2)
        );
        // A `/Layout`-owned `/BBox` is *not* returned for a `/Table` query.
        assert!(attribute_value(&elem, b"Table", b"BBox", &ident).is_none());
        // An owner-less dict acts as a wildcard.
        let mut bare = Dictionary::new();
        bare.set(b"ColSpan".to_vec(), Object::Integer(5));
        let mut e2 = Dictionary::new();
        e2.set(b"A".to_vec(), Object::Dictionary(bare));
        assert_eq!(
            attribute_value(&e2, b"Table", b"ColSpan", &ident).and_then(Object::as_i64),
            Some(5)
        );
    }

    #[test]
    fn list_numbering_maps_kinds() {
        let l = |kind: &str| elem_with_attr("List", &[("ListNumbering", name(kind))]);
        assert_eq!(
            list_numbering(&l("Decimal"), &ident),
            Some((true, ListMarker::Decimal))
        );
        assert_eq!(
            list_numbering(&l("UpperRoman"), &ident),
            Some((true, ListMarker::UpperRoman))
        );
        assert_eq!(
            list_numbering(&l("LowerAlpha"), &ident),
            Some((true, ListMarker::LowerAlpha))
        );
        assert_eq!(
            list_numbering(&l("Disc"), &ident),
            Some((false, ListMarker::Bullet('•')))
        );
        assert_eq!(
            list_numbering(&l("None"), &ident),
            Some((false, ListMarker::default()))
        );
        // Absent attribute ⇒ no override.
        assert_eq!(list_numbering(&Dictionary::new(), &ident), None);
    }

    #[test]
    fn layout_bbox_reads_rect_and_normalizes() {
        // `[llx lly urx ury]` ⇒ lower-left + width/height.
        let e = elem_with_attr(
            "Layout",
            &[(
                "BBox",
                Object::Array(vec![
                    Object::Real(72.0),
                    Object::Real(700.0),
                    Object::Real(300.0),
                    Object::Real(750.0),
                ]),
            )],
        );
        let r = layout_bbox(&e, &ident).expect("bbox");
        assert_eq!((r.x, r.y, r.w, r.h), (72.0, 700.0, 228.0, 50.0));
        // Reversed corners normalize to a positive-extent rect.
        let e2 = elem_with_attr(
            "Layout",
            &[(
                "BBox",
                Object::Array(vec![
                    Object::Integer(300),
                    Object::Integer(750),
                    Object::Integer(72),
                    Object::Integer(700),
                ]),
            )],
        );
        let r2 = layout_bbox(&e2, &ident).expect("bbox");
        assert_eq!((r2.x, r2.y, r2.w, r2.h), (72.0, 700.0, 228.0, 50.0));
        // Wrong arity ⇒ None.
        let e3 = elem_with_attr(
            "Layout",
            &[("BBox", Object::Array(vec![Object::Integer(1)]))],
        );
        assert!(layout_bbox(&e3, &ident).is_none());
    }

    // ── end-to-end: a real tagged document through the walker ───────────────

    /// Assemble a raw PDF from `(obj number, body)` parts. `Document::open`
    /// locates objects by scanning and the catalog via `trailer /Root`, so the
    /// dummy xref offsets need not be accurate (mirrors `document.rs`'s helper).
    fn raw_pdf(objects: &[(u32, String)]) -> Vec<u8> {
        let mut out = String::from("%PDF-1.7\n");
        for (num, body) in objects {
            out.push_str(&format!("{num} 0 obj\n{body}\nendobj\n"));
        }
        out.push_str(
            "xref\n0 1\n0000000000 65535 f \ntrailer\n<< /Root 1 0 R >>\nstartxref\n0\n%%EOF",
        );
        out.into_bytes()
    }

    /// A 2-page tagged document. Page 1 (obj 8) carries a `/Table` with a single
    /// `/TR` of two `/TD`s — the first spanning 2 columns (`/ColSpan 2`) and
    /// carrying a `/Layout /BBox` — plus an ordered `/L` (`/ListNumbering
    /// /Decimal`). Page 2 (obj 9) carries a single `/P` whose `/Pg` points at it.
    fn tagged_doc() -> crate::Document {
        // Page-1 content: MCID 0/1 = the two table cells, MCID 2 = the list item.
        let c1 = "BT /F1 12 Tf \
                  /TD <</MCID 0>> BDC (A) Tj EMC \
                  /TD <</MCID 1>> BDC (B) Tj EMC \
                  /LI <</MCID 2>> BDC (one) Tj EMC ET";
        // Page-2 content: MCID 0 = the paragraph text.
        let c2 = "BT /F1 12 Tf /P <</MCID 0>> BDC (second page) Tj EMC ET";
        let pdf = raw_pdf(&[
            (
                1,
                "<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 10 0 R >>".into(),
            ),
            (2, "<< /Type /Pages /Kids [8 0 R 9 0 R] /Count 2 >>".into()),
            (
                6,
                "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
            ),
            (
                8,
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 800] \
                 /Resources << /Font << /F1 6 0 R >> >> /Contents 18 0 R >>"
                    .into(),
            ),
            (
                9,
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 800] \
                 /Resources << /Font << /F1 6 0 R >> >> /Contents 19 0 R >>"
                    .into(),
            ),
            (
                18,
                format!("<< /Length {} >> stream\n{c1}\nendstream", c1.len()),
            ),
            (
                19,
                format!("<< /Length {} >> stream\n{c2}\nendstream", c2.len()),
            ),
            // StructTreeRoot → /Document (obj 11).
            (10, "<< /Type /StructTreeRoot /K 11 0 R >>".into()),
            (
                11,
                "<< /Type /StructElem /S /Document /K [12 0 R 16 0 R 20 0 R] >>".into(),
            ),
            // Table (obj 12) → TR (13) → TD col-span-2 (14, with /Pg page 1) + TD (15).
            (12, "<< /S /Table /Pg 8 0 R /K 13 0 R >>".into()),
            (13, "<< /S /TR /K [14 0 R 15 0 R] >>".into()),
            (
                14,
                "<< /S /TD /Pg 8 0 R /A << /O /Table /ColSpan 2 >> /K 0 >>".into(),
            ),
            (15, "<< /S /TD /Pg 8 0 R /K 1 >>".into()),
            // List (obj 16, ordered Decimal) → LI (17).
            (
                16,
                "<< /S /L /Pg 8 0 R /A << /O /List /ListNumbering /Decimal >> /K 17 0 R >>".into(),
            ),
            (17, "<< /S /LI /Pg 8 0 R /K 2 >>".into()),
            // Paragraph on page 2 (obj 20), with a /Layout /BBox.
            (
                20,
                "<< /S /P /Pg 9 0 R /A << /O /Layout /BBox [72 700 300 750] >> /K 0 >>".into(),
            ),
        ]);
        crate::Document::open(&pdf).expect("valid tagged PDF")
    }

    #[test]
    fn tagged_table_cell_colspan_is_lowered_and_grid_aligned() {
        let doc = tagged_doc();
        let mut ids = IdGen::default();
        let blocks = reconstruct_from_struct_tree(&doc, &mut ids).expect("tag tree blocks");
        let table = blocks
            .iter()
            .find_map(|b| match &b.kind {
                BlockKind::Table(t) => Some(t),
                _ => None,
            })
            .expect("a table block");
        let row = &table.rows[0];
        assert_eq!(row.cells.len(), 2, "two physical cells");
        assert_eq!(row.cells[0].col_span, 2, "first cell spans two columns");
        assert_eq!(row.cells[1].col_span, 1, "second cell is single");
        // Logical grid width honours the span: 2 + 1 = 3 columns.
        assert_eq!(table.col_widths.len(), 3, "grid is 3 columns wide");
    }

    #[test]
    fn tagged_list_decimal_numbering_is_ordered() {
        let doc = tagged_doc();
        let mut ids = IdGen::default();
        let blocks = reconstruct_from_struct_tree(&doc, &mut ids).expect("tag tree blocks");
        let list = blocks
            .iter()
            .find_map(|b| match &b.kind {
                BlockKind::List(l) => Some(l),
                _ => None,
            })
            .expect("a list block");
        assert!(list.ordered, "/ListNumbering /Decimal ⇒ ordered list");
        assert_eq!(list.marker, ListMarker::Decimal);
    }

    #[test]
    fn tagged_paragraph_bbox_sets_block_frame() {
        let doc = tagged_doc();
        let mut ids = IdGen::default();
        let blocks = reconstruct_from_struct_tree(&doc, &mut ids).expect("tag tree blocks");
        let para = blocks
            .iter()
            .find(|b| matches!(b.kind, BlockKind::Paragraph(_)))
            .expect("a paragraph block");
        let frame = para.frame.expect("paragraph frame from /BBox");
        assert_eq!(
            (frame.x, frame.y, frame.w, frame.h),
            (72.0, 700.0, 228.0, 50.0)
        );
    }

    #[test]
    fn tagged_block_page_assignment_follows_pg() {
        let doc = tagged_doc();
        let mut ids = IdGen::default();
        let paged =
            reconstruct_from_struct_tree_paged(&doc, &mut ids).expect("paged tag tree blocks");
        // The table + list elements declare `/Pg 8 0 R` (page index 0); the
        // paragraph declares `/Pg 9 0 R` (page index 1).
        let para_page = paged
            .iter()
            .find(|(_, b)| matches!(b.kind, BlockKind::Paragraph(_)))
            .map(|(p, _)| *p)
            .expect("a paragraph entry");
        assert_eq!(
            para_page,
            Some(1),
            "paragraph is on the 2nd page, not page 1"
        );
        let table_page = paged
            .iter()
            .find(|(_, b)| matches!(b.kind, BlockKind::Table(_)))
            .map(|(p, _)| *p)
            .expect("a table entry");
        assert_eq!(table_page, Some(0), "table is on the 1st page");
    }

    #[test]
    fn pg_resolves_to_zero_based_page_index() {
        // Page object ids in document order; `/Pg 7 0 R` is the 2nd page → idx 1.
        let page_ids: Vec<crate::object::ObjectId> = vec![(4, 0), (7, 0), (9, 0)];
        let mut elem = Dictionary::new();
        elem.set(b"Pg".to_vec(), Object::Reference((7, 0)));
        assert_eq!(pg_page_index(&elem, &page_ids), Some(1));
        // `/Pg` pointing at the third page → idx 2.
        let mut e2 = Dictionary::new();
        e2.set(b"Pg".to_vec(), Object::Reference((9, 0)));
        assert_eq!(pg_page_index(&e2, &page_ids), Some(2));
        // Absent / non-matching `/Pg` ⇒ None (page unknown, not page 0).
        assert_eq!(pg_page_index(&Dictionary::new(), &page_ids), None);
        let mut e3 = Dictionary::new();
        e3.set(b"Pg".to_vec(), Object::Reference((99, 0)));
        assert_eq!(pg_page_index(&e3, &page_ids), None);
    }
}
