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
    geom::Rotation, Block, BlockKind, Cell, Heading, Inline, InlineRun, List, ListItem, ListMarker,
    Paragraph, ParagraphStyle, Row, Table,
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

/// Per-`(page index, MCID)` accumulated text from a page's marked content.
type MarkedText = BTreeMap<(usize, i64), String>;

/// Walk the document's `/StructTreeRoot` into model blocks. `None` when the
/// document is not tagged, or the tree produced no text-bearing blocks (so the
/// caller uses the heuristic reconstruction instead).
pub fn reconstruct_from_struct_tree(doc: &crate::Document, ids: &mut IdGen) -> Option<Vec<Block>> {
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
    // The root's `/K` are the top-level structure elements.
    let mut blocks = Vec::new();
    for kid in kids_of(doc, root) {
        walker.visit(&kid, &mut blocks, None);
    }
    // If nothing text-bearing came out, the tree was unhelpful — fall back.
    let has_text = blocks.iter().any(block_has_text);
    has_text.then_some(blocks)
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
    /// `page_hint` (the page index of the nearest enclosing `/Pg`).
    fn visit(&mut self, node: &Object, out: &mut Vec<Block>, page_hint: Option<usize>) {
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
                    out.push(self.heading_block(level, &text));
                }
            }
            Some(StructRole::Paragraph) => {
                let text = self.collect_text(dict, page);
                if !text.is_empty() {
                    out.push(self.paragraph_block(&text));
                }
            }
            Some(StructRole::List) => {
                if let Some(block) = self.list_block(dict, page) {
                    out.push(block);
                }
            }
            Some(StructRole::Table) => {
                if let Some(block) = self.table_block(dict, page) {
                    out.push(block);
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
        let pg = elem.get(b"Pg")?;
        let Object::Reference(id) = pg else {
            return None;
        };
        self.doc.page_ids().ok()?.iter().position(|p| p == id)
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

    fn paragraph_block(&mut self, text: &str) -> Block {
        Block {
            id: self.ids.mint(),
            frame: None,
            rotation: Rotation::D0,
            kind: BlockKind::Paragraph(text_paragraph(text)),
        }
    }

    fn heading_block(&mut self, level: u8, text: &str) -> Block {
        Block {
            id: self.ids.mint(),
            frame: None,
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
        Some(Block {
            id: self.ids.mint(),
            frame: None,
            rotation: Rotation::D0,
            kind: BlockKind::List(List {
                ordered: false,
                marker: ListMarker::default(),
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
                cells.push(Cell {
                    blocks: vec![Block {
                        id: self.ids.mint(),
                        frame: None,
                        rotation: Rotation::D0,
                        kind: BlockKind::Paragraph(text_paragraph(&text)),
                    }],
                    col_span: 1,
                    row_span: 1,
                    shading: None,
                    vertical_align: None,
                });
            }
            if cells.is_empty() {
                continue;
            }
            max_cols = max_cols.max(cells.len());
            rows.push(Row {
                cells,
                height: None,
            });
        }
        if rows.is_empty() {
            return None;
        }
        Some(Block {
            id: self.ids.mint(),
            frame: None,
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
}
