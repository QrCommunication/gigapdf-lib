//! Editing operations on the unified [`Document`] model.
//!
//! A small, serializable [`ModelOp`] command set lets a host (the SDK / WASM
//! layer) mutate any document — whatever format it was lowered *from* — by
//! addressing its tree positionally. Blocks are addressed by a stable
//! [`BlockAddr`] = `(section, page, index)` triple; that mirrors the existing
//! content-stream API's positional `(page, index)` convention and is robust to
//! how importers assign [`BlockId`](super::BlockId)s (which need not be globally
//! unique). Out-of-range addresses are **no-ops** — [`apply_ops`] never panics.
//!
//! ## Operations
//!
//! - run level: [`SetRunText`](ModelOp::SetRunText),
//!   [`RestyleRun`](ModelOp::RestyleRun) (patch only the provided fields),
//!   [`InsertRun`](ModelOp::InsertRun), [`DeleteRun`](ModelOp::DeleteRun).
//! - block level: [`InsertBlock`](ModelOp::InsertBlock),
//!   [`DeleteBlock`](ModelOp::DeleteBlock), [`MoveBlock`](ModelOp::MoveBlock)
//!   (reorder or relocate across pages), [`SetBlockText`](ModelOp::SetBlockText)
//!   (replace a paragraph/heading's text wholesale),
//!   [`RestyleBlock`](ModelOp::RestyleBlock),
//!   [`SetBlockFrame`](ModelOp::SetBlockFrame) /
//!   [`SetBlockRotation`](ModelOp::SetBlockRotation) (place / rotate a block in
//!   absolute coordinates).
//! - paragraph formatting: [`SetParagraphStyle`](ModelOp::SetParagraphStyle) —
//!   a patch of optional fields (alignment, indents, spacing, leading) onto the
//!   addressed paragraph/heading/text-box's [`ParagraphStyle`].
//! - list level: [`SetListLevel`](ModelOp::SetListLevel),
//!   [`SetListMarker`](ModelOp::SetListMarker),
//!   [`SetListOrdered`](ModelOp::SetListOrdered) — on a [`List`] block.
//! - table cell: [`SetCellText`](ModelOp::SetCellText),
//!   [`SetCellShading`](ModelOp::SetCellShading) (per-cell background).
//! - table geometry: [`SetRowHeight`](ModelOp::SetRowHeight),
//!   [`SetColWidth`](ModelOp::SetColWidth),
//!   [`SetTableBorder`](ModelOp::SetTableBorder).
//! - table structure: [`InsertTableRow`](ModelOp::InsertTableRow),
//!   [`DeleteTableRow`](ModelOp::DeleteTableRow),
//!   [`InsertTableColumn`](ModelOp::InsertTableColumn),
//!   [`DeleteTableColumn`](ModelOp::DeleteTableColumn),
//!   [`SetCellSpan`](ModelOp::SetCellSpan) — these keep the column geometry
//!   (`col_widths` + per-cell spans) coherent.
//! - sheet cell: [`SetSheetCell`](ModelOp::SetSheetCell).
//! - sheet structure: [`InsertSheetRow`](ModelOp::InsertSheetRow),
//!   [`DeleteSheetRow`](ModelOp::DeleteSheetRow),
//!   [`InsertSheetColumn`](ModelOp::InsertSheetColumn),
//!   [`DeleteSheetColumn`](ModelOp::DeleteSheetColumn) — these shift cells and
//!   adjust merge ranges.
//!
//! ## JSON
//!
//! Each op is a tagged object `{ "op": "<name>", … }`; [`ModelOp::from_json`]
//! parses one and [`parse_ops`] parses a JSON array of them. The hand-rolled
//! parser mirrors [`model::json`](super::json)'s conventions (no serde): a
//! single private scanner with `ws`/`peek`/`string`/`number`/`array`/`object`.
//! Examples:
//!
//! ```json
//! { "op": "setRunText", "addr": [0,0,2], "run": 0, "text": "Hello" }
//! { "op": "restyleRun", "addr": [0,0,2], "run": 0,
//!   "style": { "bold": true, "size_pt": 14, "color": [1,0,0] } }
//! { "op": "insertBlock", "addr": [0,0,1], "block": { "kind": { "t":"paragraph", … } } }
//! { "op": "moveBlock", "addr": [0,0,3], "to": [0,1,0] }
//! { "op": "setSheetCell", "addr": [0,0,0], "sheet": 0, "row": 2, "col": 1,
//!   "value": { "t":"number", "v": 42 } }
//! ```

use crate::convert::style::Generic;
use crate::model::geom::{Rect, Rotation};
use crate::model::{
    Block, BlockId, BlockKind, BorderStyle, Cell, CellValue, Document, Inline, InlineRun, List,
    ListMarker, MergeRange, Page, Row, Sheet, SheetCell, SheetRow, Table,
};

/// A positional block address: `(section, page-in-section, block index)`,
/// all zero-based. The triple is stable for a given tree snapshot and survives
/// JSON round-trips of the model (unlike importer-assigned ids).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BlockAddr {
    pub section: usize,
    pub page: usize,
    pub index: usize,
}

impl BlockAddr {
    pub fn new(section: usize, page: usize, index: usize) -> Self {
        Self {
            section,
            page,
            index,
        }
    }
}

/// A subset of [`CharStyle`](crate::model::style::CharStyle) fields to patch onto
/// a run or block. Every field is optional: `None` leaves the existing value
/// untouched, so a restyle op only changes what it names.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StylePatch {
    pub family: Option<String>,
    pub generic: Option<Generic>,
    pub size_pt: Option<f64>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub underline: Option<bool>,
    pub strike: Option<bool>,
    /// `Some(Some(rgb))` sets the colour, `Some(None)` clears it (→ default
    /// black), `None` leaves it unchanged.
    pub color: Option<Option<[f64; 3]>>,
}

impl StylePatch {
    /// True when this patch carries no field (a no-op restyle).
    fn is_empty(&self) -> bool {
        self.family.is_none()
            && self.generic.is_none()
            && self.size_pt.is_none()
            && self.bold.is_none()
            && self.italic.is_none()
            && self.underline.is_none()
            && self.strike.is_none()
            && self.color.is_none()
    }

    /// Apply this patch in place to a character style.
    fn apply(&self, style: &mut crate::model::style::CharStyle) {
        if let Some(f) = &self.family {
            style.family = f.clone();
        }
        if let Some(g) = self.generic {
            style.generic = g;
        }
        if let Some(s) = self.size_pt {
            style.size_pt = s;
        }
        if let Some(b) = self.bold {
            style.bold = b;
        }
        if let Some(i) = self.italic {
            style.italic = i;
        }
        if let Some(u) = self.underline {
            style.underline = u;
        }
        if let Some(s) = self.strike {
            style.strike = s;
        }
        if let Some(c) = self.color {
            style.color = c;
        }
    }
}

/// A subset of [`ParagraphStyle`](crate::model::style::ParagraphStyle) fields to
/// patch onto a paragraph/heading/text-box. Every field is optional: `None`
/// leaves the existing value untouched, so the op only changes what it names.
/// This is the paragraph-formatting counterpart of [`StylePatch`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ParaPatch {
    pub align: Option<crate::model::style::Align>,
    pub indent_left_pt: Option<f64>,
    pub indent_right_pt: Option<f64>,
    pub first_line_pt: Option<f64>,
    pub space_before_pt: Option<f64>,
    pub space_after_pt: Option<f64>,
    pub line_height: Option<crate::model::style::LineHeight>,
}

impl ParaPatch {
    /// True when this patch carries no field (a no-op).
    fn is_empty(&self) -> bool {
        self.align.is_none()
            && self.indent_left_pt.is_none()
            && self.indent_right_pt.is_none()
            && self.first_line_pt.is_none()
            && self.space_before_pt.is_none()
            && self.space_after_pt.is_none()
            && self.line_height.is_none()
    }

    /// Apply this patch in place to a paragraph style.
    fn apply(&self, style: &mut crate::model::style::ParagraphStyle) {
        if let Some(a) = self.align {
            style.align = a;
        }
        if let Some(v) = self.indent_left_pt {
            style.indent_left_pt = v;
        }
        if let Some(v) = self.indent_right_pt {
            style.indent_right_pt = v;
        }
        if let Some(v) = self.first_line_pt {
            style.first_line_pt = v;
        }
        if let Some(v) = self.space_before_pt {
            style.space_before_pt = v;
        }
        if let Some(v) = self.space_after_pt {
            style.space_after_pt = v;
        }
        if let Some(lh) = self.line_height {
            style.line_height = lh;
        }
    }
}

/// A single editing command against a [`Document`] model.
#[derive(Debug, Clone, PartialEq)]
pub enum ModelOp {
    /// Set the text of the `run`-th [`InlineRun`] of the addressed block.
    SetRunText {
        addr: BlockAddr,
        run: usize,
        text: String,
    },
    /// Patch the style of the `run`-th [`InlineRun`] of the addressed block.
    RestyleRun {
        addr: BlockAddr,
        run: usize,
        style: StylePatch,
    },
    /// Insert a new text run at `run` (clamped to the run count) in the
    /// addressed block, with the given text and optional style patch.
    InsertRun {
        addr: BlockAddr,
        run: usize,
        text: String,
        style: StylePatch,
    },
    /// Delete the `run`-th [`InlineRun`] of the addressed block.
    DeleteRun { addr: BlockAddr, run: usize },
    /// Insert a block at the address's `index` (clamped to the page's block
    /// count) on its page.
    InsertBlock { addr: BlockAddr, block: Block },
    /// Delete the addressed block.
    DeleteBlock { addr: BlockAddr },
    /// Move the addressed block to `to` (reorder within a page or relocate to
    /// another page/section). `to.index` is clamped to the destination page's
    /// block count after removal.
    MoveBlock { addr: BlockAddr, to: BlockAddr },
    /// Replace the addressed paragraph/heading's content with a single run of
    /// `text`, keeping the first run's style if present.
    SetBlockText { addr: BlockAddr, text: String },
    /// Patch the style of **every** run in the addressed block (paragraph,
    /// heading, or text box).
    RestyleBlock { addr: BlockAddr, style: StylePatch },
    /// Set the text of a cell in the addressed [`Table`](crate::model::Table)
    /// block: replace the cell's content with one paragraph of `text`.
    SetCellText {
        addr: BlockAddr,
        row: usize,
        col: usize,
        text: String,
    },
    /// Set the typed value of a cell in the addressed
    /// [`Sheet`](crate::model::Sheet) of a [`SheetBlock`](crate::model::SheetBlock).
    SetSheetCell {
        addr: BlockAddr,
        sheet: usize,
        row: usize,
        col: usize,
        value: CellValue,
    },

    // ── paragraph formatting ──────────────────────────────────────────────────
    /// Patch the [`ParagraphStyle`](crate::model::style::ParagraphStyle) of the
    /// addressed paragraph/heading/text-box (alignment, indents, spacing,
    /// leading). Only the patch's named fields change. No-op on other block
    /// kinds, on an out-of-range address, or when the patch is empty.
    SetParagraphStyle { addr: BlockAddr, patch: ParaPatch },

    // ── list editing ──────────────────────────────────────────────────────────
    /// Set the nesting `level` of **every** item in the addressed
    /// [`List`](crate::model::List). No-op on non-list blocks.
    SetListLevel { addr: BlockAddr, level: u8 },
    /// Set the bullet/number [`marker`](crate::model::ListMarker) of the
    /// addressed list. No-op on non-list blocks.
    SetListMarker {
        addr: BlockAddr,
        marker: ListMarker,
    },
    /// Set whether the addressed list is `ordered` (numbered) or not. No-op on
    /// non-list blocks.
    SetListOrdered { addr: BlockAddr, ordered: bool },

    // ── absolute block placement ──────────────────────────────────────────────
    /// Set the addressed block's absolute placement [`frame`](crate::model::Block::frame)
    /// (move / resize). The rectangle is in PDF points (lower-left origin).
    SetBlockFrame { addr: BlockAddr, rect: Rect },
    /// Rotate the addressed block by `deg` degrees counter-clockwise. The four
    /// cardinal angles map to the exact [`Rotation`](crate::model::Rotation)
    /// variants; any other value becomes [`Rotation::Deg`].
    SetBlockRotation { addr: BlockAddr, deg: f64 },

    // ── per-cell shading ──────────────────────────────────────────────────────
    /// Set the background shading of the `(row, col)` cell in the addressed
    /// [`Table`](crate::model::Table). `Some(rgb)` sets the colour, `None`
    /// clears it. The cell is addressed by its position in `rows[row].cells`.
    /// Out-of-range ⇒ no-op.
    SetCellShading {
        addr: BlockAddr,
        row: usize,
        col: usize,
        color: Option<[f64; 3]>,
    },

    // ── table geometry ────────────────────────────────────────────────────────
    /// Set the fixed `height` (points) of row `row` in the addressed table.
    /// Out-of-range ⇒ no-op.
    SetRowHeight {
        addr: BlockAddr,
        row: usize,
        height: f64,
    },
    /// Set the `width` (points) of grid-column `col` in the addressed table,
    /// growing `col_widths` up to the logical column count if needed. A `col`
    /// at or past the logical column count ⇒ no-op.
    SetColWidth {
        addr: BlockAddr,
        col: usize,
        width: f64,
    },
    /// Set the [`border`](crate::model::Table::border) of the addressed table.
    /// No-op on non-table blocks.
    SetTableBorder {
        addr: BlockAddr,
        border: BorderStyle,
    },

    // ── structural table editing ──────────────────────────────────────────────
    /// Insert an empty row into the addressed [`Table`](crate::model::Table) at
    /// `at` (clamped to the row count). The new row is filled with one empty
    /// single-column cell per logical column so it spans the table's full width.
    InsertTableRow { addr: BlockAddr, at: usize },
    /// Delete row `at` from the addressed table. Cells in earlier rows whose
    /// `row_span` reaches across `at` are shrunk by one so the grid stays
    /// rectangular. Out-of-range ⇒ no-op.
    DeleteTableRow { addr: BlockAddr, at: usize },
    /// Insert a column at grid-index `at` (clamped to the column count) into the
    /// addressed table: a fresh width is added to `col_widths`, and every row
    /// gains a cell at that boundary — or, when `at` falls inside a spanning
    /// cell, that cell's `col_span` is widened so the new column passes through
    /// the merge.
    InsertTableColumn { addr: BlockAddr, at: usize },
    /// Delete the column at grid-index `at` from the addressed table: its
    /// `col_widths` entry is removed and, per row, the cell covering that grid
    /// column is removed (span 1) or shrunk (span > 1). Out-of-range ⇒ no-op.
    DeleteTableColumn { addr: BlockAddr, at: usize },
    /// Set the `(col_span, row_span)` of the `(row, col)` cell in the addressed
    /// table. Both spans are clamped to at least 1. The cell is addressed by its
    /// position in `rows[row].cells` (not by grid column). Out-of-range ⇒ no-op.
    SetCellSpan {
        addr: BlockAddr,
        row: usize,
        col: usize,
        col_span: u16,
        row_span: u16,
    },

    // ── structural sheet editing ──────────────────────────────────────────────
    /// Insert an empty row into the addressed [`Sheet`](crate::model::Sheet) of a
    /// [`SheetBlock`](crate::model::SheetBlock) at `at` (clamped). Merge ranges at
    /// or below `at` shift down by one row.
    InsertSheetRow {
        addr: BlockAddr,
        sheet: usize,
        at: usize,
    },
    /// Delete row `at` from the addressed sheet, dropping cells in that row and
    /// shifting lower rows up. Merge ranges are adjusted (shrunk, shifted, or
    /// dropped when they collapse). Out-of-range ⇒ no-op.
    DeleteSheetRow {
        addr: BlockAddr,
        sheet: usize,
        at: usize,
    },
    /// Insert a column at index `at` (clamped) into the addressed sheet: every
    /// row gains an empty cell at `at`, `col_widths` gains a slot, and merge
    /// ranges at or right of `at` shift right by one column.
    InsertSheetColumn {
        addr: BlockAddr,
        sheet: usize,
        at: usize,
    },
    /// Delete column `at` from the addressed sheet, dropping that cell in every
    /// row and shifting the rest left. `col_widths` and merge ranges are adjusted
    /// (shrunk, shifted, or dropped). Out-of-range ⇒ no-op.
    DeleteSheetColumn {
        addr: BlockAddr,
        sheet: usize,
        at: usize,
    },
}

/// Apply `ops` to `doc` in order. Out-of-range addresses are silently skipped.
/// Returns the number of ops that took effect (mutated the document).
pub fn apply_ops(doc: &mut Document, ops: &[ModelOp]) -> usize {
    let mut applied = 0;
    for op in ops {
        if apply_one(doc, op) {
            applied += 1;
        }
    }
    applied
}

/// Resolve a [`BlockAddr`] to the destination page mutably (no bounds on the
/// block index — that is checked by the caller).
fn page_mut<'a>(doc: &'a mut Document, addr: &BlockAddr) -> Option<&'a mut Page> {
    doc.sections.get_mut(addr.section)?.pages.get_mut(addr.page)
}

/// Resolve a [`BlockAddr`] to the addressed block mutably.
fn block_mut<'a>(doc: &'a mut Document, addr: &BlockAddr) -> Option<&'a mut Block> {
    page_mut(doc, addr)?.blocks.get_mut(addr.index)
}

/// The mutable run vector of a paragraph/heading/text-box block, if it has one.
/// (Text boxes expose their first paragraph's runs.)
fn block_runs_mut(block: &mut Block) -> Option<&mut Vec<Inline>> {
    match &mut block.kind {
        BlockKind::Paragraph(p) => Some(&mut p.runs),
        BlockKind::Heading(h) => Some(&mut h.para.runs),
        BlockKind::TextBox(tb) => tb.blocks.first_mut().and_then(block_runs_mut),
        _ => None,
    }
}

/// The mutable [`ParagraphStyle`](crate::model::style::ParagraphStyle) of a
/// paragraph/heading/text-box block, if it has one. (Text boxes expose their
/// first paragraph's style — mirroring [`block_runs_mut`].)
fn block_para_style_mut(block: &mut Block) -> Option<&mut crate::model::style::ParagraphStyle> {
    match &mut block.kind {
        BlockKind::Paragraph(p) => Some(&mut p.style),
        BlockKind::Heading(h) => Some(&mut h.para.style),
        BlockKind::TextBox(tb) => tb.blocks.first_mut().and_then(block_para_style_mut),
        _ => None,
    }
}

/// The `n`-th [`InlineRun`] within an inline list (skipping non-run inlines,
/// counting only `Inline::Run`).
fn nth_inline_run(runs: &mut [Inline], n: usize) -> Option<&mut InlineRun> {
    runs.iter_mut()
        .filter_map(|i| match i {
            Inline::Run(r) => Some(r),
            _ => None,
        })
        .nth(n)
}

fn apply_one(doc: &mut Document, op: &ModelOp) -> bool {
    match op {
        ModelOp::SetRunText { addr, run, text } => {
            let Some(block) = block_mut(doc, addr) else {
                return false;
            };
            let Some(runs) = block_runs_mut(block) else {
                return false;
            };
            match nth_inline_run(runs, *run) {
                Some(r) => {
                    r.text = text.clone();
                    true
                }
                None => false,
            }
        }
        ModelOp::RestyleRun { addr, run, style } => {
            if style.is_empty() {
                return false;
            }
            let Some(block) = block_mut(doc, addr) else {
                return false;
            };
            let Some(runs) = block_runs_mut(block) else {
                return false;
            };
            match nth_inline_run(runs, *run) {
                Some(r) => {
                    style.apply(&mut r.style);
                    true
                }
                None => false,
            }
        }
        ModelOp::InsertRun {
            addr,
            run,
            text,
            style,
        } => {
            let Some(block) = block_mut(doc, addr) else {
                return false;
            };
            let Some(runs) = block_runs_mut(block) else {
                return false;
            };
            // Inherit style from the run currently at/around the insertion point,
            // then apply the patch on top.
            let mut new_run = InlineRun {
                text: text.clone(),
                style: nearest_run_style(runs, *run),
                source_index: None,
            };
            style.apply(&mut new_run.style);
            // Translate the run index (over `Inline::Run`s) to a position in the
            // mixed inline vector.
            let pos = inline_pos_for_run(runs, *run);
            runs.insert(pos, Inline::Run(new_run));
            true
        }
        ModelOp::DeleteRun { addr, run } => {
            let Some(block) = block_mut(doc, addr) else {
                return false;
            };
            let Some(runs) = block_runs_mut(block) else {
                return false;
            };
            match run_inline_index(runs, *run) {
                Some(pos) => {
                    runs.remove(pos);
                    true
                }
                None => false,
            }
        }
        ModelOp::InsertBlock { addr, block } => {
            let Some(page) = page_mut(doc, addr) else {
                return false;
            };
            let pos = addr.index.min(page.blocks.len());
            page.blocks.insert(pos, block.clone());
            true
        }
        ModelOp::DeleteBlock { addr } => {
            let Some(page) = page_mut(doc, addr) else {
                return false;
            };
            if addr.index < page.blocks.len() {
                page.blocks.remove(addr.index);
                true
            } else {
                false
            }
        }
        ModelOp::MoveBlock { addr, to } => move_block(doc, addr, to),
        ModelOp::SetBlockText { addr, text } => {
            let Some(block) = block_mut(doc, addr) else {
                return false;
            };
            set_block_text(block, text)
        }
        ModelOp::RestyleBlock { addr, style } => {
            if style.is_empty() {
                return false;
            }
            let Some(block) = block_mut(doc, addr) else {
                return false;
            };
            let Some(runs) = block_runs_mut(block) else {
                return false;
            };
            let mut touched = false;
            for inline in runs.iter_mut() {
                if let Inline::Run(r) = inline {
                    style.apply(&mut r.style);
                    touched = true;
                }
            }
            touched
        }
        ModelOp::SetCellText {
            addr,
            row,
            col,
            text,
        } => {
            let Some(block) = block_mut(doc, addr) else {
                return false;
            };
            let BlockKind::Table(table) = &mut block.kind else {
                return false;
            };
            let Some(r) = table.rows.get_mut(*row) else {
                return false;
            };
            let Some(cell) = r.cells.get_mut(*col) else {
                return false;
            };
            cell.blocks = vec![paragraph_block(text)];
            true
        }
        ModelOp::SetSheetCell {
            addr,
            sheet,
            row,
            col,
            value,
        } => {
            let Some(block) = block_mut(doc, addr) else {
                return false;
            };
            let BlockKind::Sheet(sb) = &mut block.kind else {
                return false;
            };
            let Some(sh) = sb.sheets.get_mut(*sheet) else {
                return false;
            };
            // Grow rows/cells on demand so a host can write into a sparse grid.
            if *row >= sh.rows.len() {
                sh.rows.resize_with(*row + 1, Default::default);
            }
            let r = &mut sh.rows[*row];
            if *col >= r.cells.len() {
                r.cells.resize_with(*col + 1, Default::default);
            }
            r.cells[*col].value = value.clone();
            true
        }
        ModelOp::SetParagraphStyle { addr, patch } => {
            if patch.is_empty() {
                return false;
            }
            let Some(block) = block_mut(doc, addr) else {
                return false;
            };
            match block_para_style_mut(block) {
                Some(style) => {
                    patch.apply(style);
                    true
                }
                None => false,
            }
        }
        ModelOp::SetListLevel { addr, level } => with_list(doc, addr, |l| {
            for item in &mut l.items {
                item.level = *level;
            }
            true
        }),
        ModelOp::SetListMarker { addr, marker } => with_list(doc, addr, |l| {
            l.marker = *marker;
            true
        }),
        ModelOp::SetListOrdered { addr, ordered } => with_list(doc, addr, |l| {
            l.ordered = *ordered;
            true
        }),
        ModelOp::SetBlockFrame { addr, rect } => {
            let Some(block) = block_mut(doc, addr) else {
                return false;
            };
            block.frame = Some(*rect);
            true
        }
        ModelOp::SetBlockRotation { addr, deg } => {
            let Some(block) = block_mut(doc, addr) else {
                return false;
            };
            block.rotation = rotation_from_degrees(*deg);
            true
        }
        ModelOp::SetCellShading {
            addr,
            row,
            col,
            color,
        } => with_table(doc, addr, |t| {
            let Some(r) = t.rows.get_mut(*row) else {
                return false;
            };
            let Some(cell) = r.cells.get_mut(*col) else {
                return false;
            };
            cell.shading = *color;
            true
        }),
        ModelOp::SetRowHeight { addr, row, height } => with_table(doc, addr, |t| {
            let Some(r) = t.rows.get_mut(*row) else {
                return false;
            };
            r.height = Some(*height);
            true
        }),
        ModelOp::SetColWidth { addr, col, width } => with_table(doc, addr, |t| {
            let cols = table_columns(t);
            if *col >= cols {
                return false;
            }
            // Grow `col_widths` up to the logical column count so a sparse table
            // becomes addressable at `col`, then set the named width.
            if t.col_widths.len() <= *col {
                let seed = default_col_width(t);
                t.col_widths.resize(cols, seed);
            }
            t.col_widths[*col] = *width;
            true
        }),
        ModelOp::SetTableBorder { addr, border } => with_table(doc, addr, |t| {
            t.border = *border;
            true
        }),
        ModelOp::InsertTableRow { addr, at } => {
            with_table(doc, addr, |t| insert_table_row(t, *at))
        }
        ModelOp::DeleteTableRow { addr, at } => {
            with_table(doc, addr, |t| delete_table_row(t, *at))
        }
        ModelOp::InsertTableColumn { addr, at } => {
            with_table(doc, addr, |t| insert_table_column(t, *at))
        }
        ModelOp::DeleteTableColumn { addr, at } => {
            with_table(doc, addr, |t| delete_table_column(t, *at))
        }
        ModelOp::SetCellSpan {
            addr,
            row,
            col,
            col_span,
            row_span,
        } => with_table(doc, addr, |t| {
            let Some(r) = t.rows.get_mut(*row) else {
                return false;
            };
            let Some(cell) = r.cells.get_mut(*col) else {
                return false;
            };
            cell.col_span = (*col_span).max(1);
            cell.row_span = (*row_span).max(1);
            true
        }),
        ModelOp::InsertSheetRow { addr, sheet, at } => {
            with_sheet(doc, addr, *sheet, |s| insert_sheet_row(s, *at))
        }
        ModelOp::DeleteSheetRow { addr, sheet, at } => {
            with_sheet(doc, addr, *sheet, |s| delete_sheet_row(s, *at))
        }
        ModelOp::InsertSheetColumn { addr, sheet, at } => {
            with_sheet(doc, addr, *sheet, |s| insert_sheet_column(s, *at))
        }
        ModelOp::DeleteSheetColumn { addr, sheet, at } => {
            with_sheet(doc, addr, *sheet, |s| delete_sheet_column(s, *at))
        }
    }
}

/// Run `f` on the [`Table`] at `addr`, or return `false` when the address does
/// not resolve to a table block.
fn with_table(doc: &mut Document, addr: &BlockAddr, f: impl FnOnce(&mut Table) -> bool) -> bool {
    let Some(block) = block_mut(doc, addr) else {
        return false;
    };
    let BlockKind::Table(table) = &mut block.kind else {
        return false;
    };
    f(table)
}

/// Run `f` on the `sheet`-th [`Sheet`] of the [`SheetBlock`] at `addr`, or
/// return `false` when the address does not resolve to that sheet.
fn with_sheet(
    doc: &mut Document,
    addr: &BlockAddr,
    sheet: usize,
    f: impl FnOnce(&mut Sheet) -> bool,
) -> bool {
    let Some(block) = block_mut(doc, addr) else {
        return false;
    };
    let BlockKind::Sheet(sb) = &mut block.kind else {
        return false;
    };
    let Some(sh) = sb.sheets.get_mut(sheet) else {
        return false;
    };
    f(sh)
}

/// Run `f` on the [`List`] at `addr`, or return `false` when the address does
/// not resolve to a list block.
fn with_list(doc: &mut Document, addr: &BlockAddr, f: impl FnOnce(&mut List) -> bool) -> bool {
    let Some(block) = block_mut(doc, addr) else {
        return false;
    };
    let BlockKind::List(list) = &mut block.kind else {
        return false;
    };
    f(list)
}

/// Map a CCW degree value to a [`Rotation`]: the four cardinal angles become the
/// exact first-class variants (so the common `/Rotate` cases stay exact), any
/// other value becomes [`Rotation::Deg`].
fn rotation_from_degrees(deg: f64) -> Rotation {
    match deg {
        0.0 => Rotation::D0,
        90.0 => Rotation::D90,
        180.0 => Rotation::D180,
        270.0 => Rotation::D270,
        d => Rotation::Deg(d),
    }
}

/// Move the block at `from` to `to`, clamping the destination index. A move
/// onto an out-of-range source/destination page is a no-op.
fn move_block(doc: &mut Document, from: &BlockAddr, to: &BlockAddr) -> bool {
    // Validate source.
    let Some(src_page) = page_mut(doc, from) else {
        return false;
    };
    if from.index >= src_page.blocks.len() {
        return false;
    }
    // Validate destination page exists before detaching the block.
    if doc
        .sections
        .get(to.section)
        .and_then(|s| s.pages.get(to.page))
        .is_none()
    {
        return false;
    }
    let block = page_mut(doc, from)
        .expect("source page re-resolves")
        .blocks
        .remove(from.index);
    let dst_page = page_mut(doc, to).expect("destination page validated above");
    let pos = to.index.min(dst_page.blocks.len());
    dst_page.blocks.insert(pos, block);
    true
}

/// Replace a paragraph/heading/text-box block's content with a single run of
/// `text`, preserving the style of its first run when present.
fn set_block_text(block: &mut Block, text: &str) -> bool {
    let Some(runs) = block_runs_mut(block) else {
        return false;
    };
    let style = runs
        .iter()
        .find_map(|i| match i {
            Inline::Run(r) => Some(r.style.clone()),
            _ => None,
        })
        .unwrap_or_default();
    *runs = vec![Inline::Run(InlineRun {
        text: text.to_string(),
        style,
        source_index: None,
    })];
    true
}

/// A fresh paragraph block holding one run of `text`.
fn paragraph_block(text: &str) -> Block {
    use crate::model::Paragraph;
    Block {
        id: BlockId::default(),
        frame: None,
        rotation: crate::model::geom::Rotation::default(),
        kind: BlockKind::Paragraph(Paragraph {
            runs: vec![Inline::Run(InlineRun {
                text: text.to_string(),
                ..InlineRun::default()
            })],
            ..Paragraph::default()
        }),
    }
}

// ───────────────────────── table geometry ─────────────────────────────────────
//
// A table is a grid: `col_widths[i]` is the width of grid-column `i`, and each
// `Row::cells` entry occupies a contiguous run of grid-columns of length
// `col_span`. The logical column count is `max` over rows of `Σ col_span`,
// matching `convert::export_model::table_col_count`. The helpers below keep
// `col_widths` and the per-cell spans coherent across structural edits.

/// The table's logical grid-column count: the widest row's summed `col_span`,
/// never below `col_widths.len()`.
fn table_columns(table: &Table) -> usize {
    let from_rows = table
        .rows
        .iter()
        .map(row_columns)
        .max()
        .unwrap_or(0);
    from_rows.max(table.col_widths.len())
}

/// The number of grid-columns a row covers: the sum of its cells' spans (each at
/// least 1).
fn row_columns(row: &Row) -> usize {
    row.cells.iter().map(|c| c.col_span.max(1) as usize).sum()
}

/// A representative column width to seed a newly inserted column: the mean of the
/// existing widths, or a sensible default when the table has none yet.
fn default_col_width(table: &Table) -> f64 {
    /// Fallback width (points) when a table carries no explicit `col_widths`.
    const FALLBACK_COL_WIDTH: f64 = 72.0;
    if table.col_widths.is_empty() {
        FALLBACK_COL_WIDTH
    } else {
        table.col_widths.iter().sum::<f64>() / table.col_widths.len() as f64
    }
}

/// Insert an empty, full-width row at `at` (clamped). The row gets one empty
/// single-column cell per logical column so the grid stays rectangular.
fn insert_table_row(table: &mut Table, at: usize) -> bool {
    let cols = table_columns(table).max(1);
    let at = at.min(table.rows.len());
    let row = Row {
        cells: vec![Cell::default(); cols],
        height: None,
        // An inserted blank row is a body row.
        is_header: false,
    };
    table.rows.insert(at, row);
    true
}

/// Delete row `at`. Cells in earlier rows whose `row_span` extends across the
/// deleted row are shrunk by one so they no longer over-cover the grid.
fn delete_table_row(table: &mut Table, at: usize) -> bool {
    if at >= table.rows.len() {
        return false;
    }
    table.rows.remove(at);
    // Earlier rows can carry a vertical span that reached into `at`; shrink any
    // whose span crossed the removed row.
    for (ri, row) in table.rows.iter_mut().enumerate() {
        if ri >= at {
            break; // only rows above the deletion point could span across it
        }
        let from_top = at - ri; // 1-based distance from this row down to `at`
        for cell in &mut row.cells {
            if (cell.row_span as usize) > from_top {
                cell.row_span -= 1;
            }
        }
    }
    true
}

/// Insert a grid-column at `at` (clamped to the column count). Adds a width to
/// `col_widths` and, per row, either splits in a fresh empty cell at the column
/// boundary or widens the spanning cell the new column passes through.
fn insert_table_column(table: &mut Table, at: usize) -> bool {
    let cols = table_columns(table);
    let at = at.min(cols);
    let width = default_col_width(table);
    // Keep `col_widths` addressable up to the current column count before
    // inserting, so the new slot lands at `at` regardless of prior sparseness.
    if table.col_widths.len() < cols {
        table.col_widths.resize(cols, width);
    }
    table.col_widths.insert(at.min(table.col_widths.len()), width);

    for row in &mut table.rows {
        insert_row_column(row, at);
    }
    true
}

/// Insert one grid-column at `at` within a single row: widen the cell that
/// *straddles* `at`, or splice an empty single-column cell at the boundary.
fn insert_row_column(row: &mut Row, at: usize) {
    let mut grid = 0usize;
    for (ci, cell) in row.cells.iter_mut().enumerate() {
        let span = cell.col_span.max(1) as usize;
        if at > grid && at < grid + span {
            // `at` falls strictly inside this cell's span → the column passes
            // through the merge, so the cell simply gets one column wider.
            cell.col_span += 1;
            return;
        }
        if at == grid {
            row.cells.insert(ci, Cell::default());
            return;
        }
        grid += span;
    }
    // `at` is at or past the row's right edge → append an empty cell.
    row.cells.push(Cell::default());
}

/// Delete the grid-column at `at`. Removes its `col_widths` entry and, per row,
/// removes the covering cell (span 1) or shrinks it (span > 1).
fn delete_table_column(table: &mut Table, at: usize) -> bool {
    if at >= table_columns(table) {
        return false;
    }
    if at < table.col_widths.len() {
        table.col_widths.remove(at);
    }
    for row in &mut table.rows {
        delete_row_column(row, at);
    }
    true
}

/// Remove one grid-column at `at` within a single row.
fn delete_row_column(row: &mut Row, at: usize) {
    let mut grid = 0usize;
    for ci in 0..row.cells.len() {
        let span = row.cells[ci].col_span.max(1) as usize;
        if at >= grid && at < grid + span {
            if span > 1 {
                row.cells[ci].col_span -= 1;
            } else {
                row.cells.remove(ci);
            }
            return;
        }
        grid += span;
    }
    // `at` past this row's columns → nothing to remove in this row.
}

// ───────────────────────── sheet geometry ─────────────────────────────────────
//
// A sheet is a dense grid of `SheetCell`s with separate `merges` (inclusive
// rectangles) and `col_widths`. Row/column edits shift cells and re-map the
// merge ranges; a merge that collapses to nothing is dropped.

/// Insert an empty row at `at` (clamped); merges at or below `at` shift down.
fn insert_sheet_row(sheet: &mut Sheet, at: usize) -> bool {
    let at = at.min(sheet.rows.len());
    sheet.rows.insert(at, SheetRow::default());
    for m in &mut sheet.merges {
        if m.r0 >= at {
            m.r0 += 1;
        }
        if m.r1 >= at {
            m.r1 += 1;
        }
    }
    true
}

/// Delete row `at`, shifting lower rows up and re-mapping merges (shrink the ones
/// that span `at`, shift the ones below it, drop the ones that collapse).
fn delete_sheet_row(sheet: &mut Sheet, at: usize) -> bool {
    if at >= sheet.rows.len() {
        return false;
    }
    sheet.rows.remove(at);
    sheet.merges.retain_mut(|m| remap_delete(m.r0_r1_mut(), at));
    true
}

/// Insert an empty column at `at` (clamped) in every row; `col_widths` and
/// merges shift to match.
fn insert_sheet_column(sheet: &mut Sheet, at: usize) -> bool {
    let cols = sheet_columns(sheet);
    let at = at.min(cols);
    for row in &mut sheet.rows {
        let pos = at.min(row.cells.len());
        row.cells.insert(pos, SheetCell::default());
    }
    if at <= sheet.col_widths.len() {
        // Only widen `col_widths` when `at` lands within (or just past) the
        // tracked widths; leave a sparse tail sparse.
        sheet.col_widths.insert(at, 0.0);
    }
    for m in &mut sheet.merges {
        if m.c0 >= at {
            m.c0 += 1;
        }
        if m.c1 >= at {
            m.c1 += 1;
        }
    }
    true
}

/// Delete column `at` from every row, shifting the rest left; `col_widths` and
/// merges shift to match (collapsing merges are dropped).
fn delete_sheet_column(sheet: &mut Sheet, at: usize) -> bool {
    if at >= sheet_columns(sheet) {
        return false;
    }
    for row in &mut sheet.rows {
        if at < row.cells.len() {
            row.cells.remove(at);
        }
    }
    if at < sheet.col_widths.len() {
        sheet.col_widths.remove(at);
    }
    sheet.merges.retain_mut(|m| remap_delete(m.c0_c1_mut(), at));
    true
}

/// The sheet's column count: the widest row's cell count, never below
/// `col_widths.len()`.
fn sheet_columns(sheet: &Sheet) -> usize {
    sheet
        .rows
        .iter()
        .map(|r| r.cells.len())
        .max()
        .unwrap_or(0)
        .max(sheet.col_widths.len())
}

/// Re-map an inclusive `(lo, hi)` merge span across the deletion of line `at`.
/// Returns `false` when the span collapses to nothing (caller should drop it).
fn remap_delete((lo, hi): (&mut usize, &mut usize), at: usize) -> bool {
    if *hi < at {
        // Wholly before the deleted line → unaffected.
        return true;
    }
    if *lo > at {
        // Wholly after → shift both ends up by one.
        *lo -= 1;
        *hi -= 1;
        return true;
    }
    // The deleted line lies within `[lo, hi]`. A single-line span collapses;
    // otherwise the span loses exactly one line. Every line below `at` (inside
    // the span) slides up, so the high end always decrements; `lo` is already
    // ≤ `at`, so it stays put (a deletion of the span's top line just lets the
    // next line become the new top, keeping `lo`).
    if *lo == *hi {
        return false;
    }
    *hi -= 1;
    true
}

impl MergeRange {
    /// Mutable references to the row endpoints `(r0, r1)`.
    fn r0_r1_mut(&mut self) -> (&mut usize, &mut usize) {
        (&mut self.r0, &mut self.r1)
    }
    /// Mutable references to the column endpoints `(c0, c1)`.
    fn c0_c1_mut(&mut self) -> (&mut usize, &mut usize) {
        (&mut self.c0, &mut self.c1)
    }
}

/// The style to inherit for a run inserted at run-position `n`: the style of the
/// run that currently occupies that slot, else the previous run, else default.
fn nearest_run_style(runs: &[Inline], n: usize) -> crate::model::style::CharStyle {
    let only_runs: Vec<&InlineRun> = runs
        .iter()
        .filter_map(|i| match i {
            Inline::Run(r) => Some(r),
            _ => None,
        })
        .collect();
    if only_runs.is_empty() {
        return crate::model::style::CharStyle::default();
    }
    let idx = n.min(only_runs.len() - 1);
    only_runs[idx].style.clone()
}

/// Position in the mixed inline vector at which to insert so the new run becomes
/// the `n`-th `Inline::Run`. Past the end ⇒ append.
fn inline_pos_for_run(runs: &[Inline], n: usize) -> usize {
    let mut seen = 0;
    for (i, inline) in runs.iter().enumerate() {
        if matches!(inline, Inline::Run(_)) {
            if seen == n {
                return i;
            }
            seen += 1;
        }
    }
    runs.len()
}

/// The mixed-vector index of the `n`-th `Inline::Run`, if it exists.
fn run_inline_index(runs: &[Inline], n: usize) -> Option<usize> {
    let mut seen = 0;
    for (i, inline) in runs.iter().enumerate() {
        if matches!(inline, Inline::Run(_)) {
            if seen == n {
                return Some(i);
            }
            seen += 1;
        }
    }
    None
}

/// Parse a JSON **array** of operations. Returns the ops that parsed; a
/// malformed array (or a non-array) yields an empty vector. Individual ops that
/// fail to parse are skipped — `parse_ops("[]")` is the empty identity batch.
pub fn parse_ops(s: &str) -> Vec<ModelOp> {
    let mut p = OpReader::new(s.as_bytes());
    p.ops().unwrap_or_default()
}

impl ModelOp {
    /// Parse a single op object from JSON, or `None` on malformed input.
    pub fn from_json(s: &str) -> Option<ModelOp> {
        let mut p = OpReader::new(s.as_bytes());
        let op = p.op()?;
        p.ws();
        if p.i == p.b.len() {
            Some(op)
        } else {
            None
        }
    }
}

// ───────────────────────── JSON reader ────────────────────────────────────────
//
// A self-contained scanner mirroring `model::json::Reader` (no serde). It only
// needs the subset required to read the op envelope: whitespace, strings,
// numbers, booleans, null, arrays and objects.

struct OpReader<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> OpReader<'a> {
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
        let n = self.number()?;
        if n.fract() == 0.0 && n >= 0.0 && n <= usize::MAX as f64 {
            Some(n as usize)
        } else {
            None
        }
    }

    /// `[ item (, item)* ]`; empty `[]` → empty vec.
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

    /// Iterate `{ "k": <v>, … }`, calling `member(self, key)` per key (the
    /// callback consumes the value). Empty `{}` allowed.
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

    /// An optional RGB triple: `null` → `Some(None)`, `[r,g,b]` → `Some(Some)`.
    fn opt_rgb(&mut self) -> Option<Option<[f64; 3]>> {
        if self.peek()? == b'n' {
            self.null()?;
            Some(None)
        } else {
            let v = self.array(OpReader::number)?;
            if v.len() == 3 {
                Some(Some([v[0], v[1], v[2]]))
            } else {
                None
            }
        }
    }

    /// A plain `[r, g, b]` triple (no `null` form).
    fn rgb(&mut self) -> Option<[f64; 3]> {
        let v = self.array(OpReader::number)?;
        if v.len() == 3 {
            Some([v[0], v[1], v[2]])
        } else {
            None
        }
    }

    /// A `{ "x","y","w","h" }` rectangle (any missing field defaults to 0).
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

    /// A `{ "width", "color":[r,g,b] }` border (mirrors `model::json`).
    fn border(&mut self) -> Option<BorderStyle> {
        let mut b = BorderStyle::default();
        self.object(|rd, k| {
            match k {
                "width" => b.width = rd.number()?,
                "color" => b.color = rd.rgb()?,
                _ => return None,
            }
            Some(())
        })?;
        Some(b)
    }

    /// A `LineHeight` tagged object: `{ "t":"normal"|"multiple"|"points", "v"? }`
    /// (mirrors `model::json::line_height`).
    fn line_height(&mut self) -> Option<crate::model::style::LineHeight> {
        use crate::model::style::LineHeight;
        let mut tag: Option<String> = None;
        let mut v: Option<f64> = None;
        self.object(|rd, k| {
            match k {
                "t" => tag = Some(rd.string()?),
                "v" => v = Some(rd.number()?),
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

    /// A `ListMarker` tagged object: `{ "t":"bullet"|"decimal"|… , "v"? }`
    /// (mirrors `model::json::list_marker`).
    fn list_marker(&mut self) -> Option<ListMarker> {
        let mut tag: Option<String> = None;
        let mut v: Option<String> = None;
        self.object(|rd, k| {
            match k {
                "t" => tag = Some(rd.string()?),
                "v" => v = Some(rd.string()?),
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

    /// A paragraph-style patch object — only the fields that are present are set.
    fn para_patch(&mut self) -> Option<ParaPatch> {
        let mut pp = ParaPatch::default();
        self.object(|r, k| {
            match k {
                "align" => pp.align = Some(parse_align_tag(&r.string()?)?),
                "indent_left" => pp.indent_left_pt = Some(r.number()?),
                "indent_right" => pp.indent_right_pt = Some(r.number()?),
                "first_line" => pp.first_line_pt = Some(r.number()?),
                "space_before" => pp.space_before_pt = Some(r.number()?),
                "space_after" => pp.space_after_pt = Some(r.number()?),
                "line_height" => pp.line_height = Some(r.line_height()?),
                _ => return None,
            }
            Some(())
        })?;
        Some(pp)
    }

    // ── op envelope ───────────────────────────────────────────────────────────

    /// Parse the top-level `[ <op>, … ]` array.
    fn ops(&mut self) -> Option<Vec<ModelOp>> {
        let ops = self.array(OpReader::op)?;
        self.ws();
        if self.i == self.b.len() {
            Some(ops)
        } else {
            None
        }
    }

    /// A 3-element `[section, page, index]` block address.
    fn addr(&mut self) -> Option<BlockAddr> {
        let v = self.array(OpReader::usize)?;
        if v.len() == 3 {
            Some(BlockAddr::new(v[0], v[1], v[2]))
        } else {
            None
        }
    }

    /// A style patch object — only the fields that are present are set.
    fn style_patch(&mut self) -> Option<StylePatch> {
        let mut sp = StylePatch::default();
        self.object(|r, k| {
            match k {
                "family" => sp.family = Some(r.string()?),
                "generic" => sp.generic = Some(parse_generic_tag(&r.string()?)?),
                "size_pt" => sp.size_pt = Some(r.number()?),
                "bold" => sp.bold = Some(r.bool()?),
                "italic" => sp.italic = Some(r.bool()?),
                "underline" => sp.underline = Some(r.bool()?),
                "strike" => sp.strike = Some(r.bool()?),
                "color" => sp.color = Some(r.opt_rgb()?),
                _ => return None,
            }
            Some(())
        })?;
        Some(sp)
    }

    /// A `CellValue` tagged object: `{ "t":"empty"|"text"|"number"|"bool", … }`.
    fn cell_value(&mut self) -> Option<CellValue> {
        let mut tag: Option<String> = None;
        let mut text: Option<String> = None;
        let mut number: Option<f64> = None;
        let mut boolean: Option<bool> = None;
        self.object(|r, k| {
            match k {
                "t" => tag = Some(r.string()?),
                "v" => {
                    // `v` is polymorphic over the tag; peek to decide.
                    match r.peek()? {
                        b'"' => text = Some(r.string()?),
                        b't' | b'f' => boolean = Some(r.bool()?),
                        _ => number = Some(r.number()?),
                    }
                }
                _ => return None,
            }
            Some(())
        })?;
        match tag.as_deref()? {
            "empty" => Some(CellValue::Empty),
            "text" => Some(CellValue::Text(text.unwrap_or_default())),
            "number" => Some(CellValue::Number(number?)),
            "bool" => Some(CellValue::Bool(boolean?)),
            _ => None,
        }
    }

    /// A `Block` value, delegating to the model's JSON block reader so insert
    /// ops accept the exact same block shape `Document::to_json` emits.
    fn block(&mut self) -> Option<Block> {
        // Slice out the balanced `{ … }` object and hand it to the model reader.
        self.ws();
        let start = self.i;
        self.skip_value()?;
        let raw = std::str::from_utf8(&self.b[start..self.i]).ok()?;
        crate::model::json::block_from_json(raw)
    }

    /// Advance the cursor past one JSON value (object/array/string/number/
    /// literal) without interpreting it — used to capture a `Block` subobject.
    fn skip_value(&mut self) -> Option<()> {
        match self.peek()? {
            b'{' => self.skip_braced(b'{', b'}'),
            b'[' => self.skip_braced(b'[', b']'),
            b'"' => self.string().map(|_| ()),
            b't' => self.lit(b"true"),
            b'f' => self.lit(b"false"),
            b'n' => self.null(),
            _ => self.number().map(|_| ()),
        }
    }

    /// Skip a balanced `open`/`close` run, respecting nested strings.
    fn skip_braced(&mut self, open: u8, close: u8) -> Option<()> {
        self.eat(open)?;
        let mut depth = 1usize;
        while depth > 0 {
            let c = *self.b.get(self.i)?;
            self.i += 1;
            match c {
                b'"' => {
                    // Consume the rest of the string (escapes included).
                    loop {
                        let s = *self.b.get(self.i)?;
                        self.i += 1;
                        if s == b'\\' {
                            self.i += 1;
                        } else if s == b'"' {
                            break;
                        }
                    }
                }
                x if x == open => depth += 1,
                x if x == close => depth -= 1,
                _ => {}
            }
        }
        Some(())
    }

    /// Parse one tagged op object `{ "op": "<name>", … }`.
    fn op(&mut self) -> Option<ModelOp> {
        let mut name: Option<String> = None;
        let mut addr: Option<BlockAddr> = None;
        let mut to: Option<BlockAddr> = None;
        let mut run: Option<usize> = None;
        let mut row: Option<usize> = None;
        let mut col: Option<usize> = None;
        let mut sheet: Option<usize> = None;
        let mut at: Option<usize> = None;
        let mut col_span: Option<usize> = None;
        let mut row_span: Option<usize> = None;
        let mut text: Option<String> = None;
        let mut style: Option<StylePatch> = None;
        let mut block: Option<Block> = None;
        let mut value: Option<CellValue> = None;
        let mut patch: Option<ParaPatch> = None;
        let mut level: Option<usize> = None;
        let mut marker: Option<ListMarker> = None;
        let mut ordered: Option<bool> = None;
        let mut rect: Option<Rect> = None;
        let mut deg: Option<f64> = None;
        let mut color: Option<Option<[f64; 3]>> = None;
        let mut height: Option<f64> = None;
        let mut width: Option<f64> = None;
        let mut border: Option<BorderStyle> = None;

        self.object(|r, k| {
            match k {
                "op" => name = Some(r.string()?),
                "addr" => addr = Some(r.addr()?),
                "to" => to = Some(r.addr()?),
                "run" => run = Some(r.usize()?),
                "row" => row = Some(r.usize()?),
                "col" => col = Some(r.usize()?),
                "sheet" => sheet = Some(r.usize()?),
                "at" => at = Some(r.usize()?),
                "col_span" => col_span = Some(r.usize()?),
                "row_span" => row_span = Some(r.usize()?),
                "text" => text = Some(r.string()?),
                "style" => style = Some(r.style_patch()?),
                "block" => block = Some(r.block()?),
                "value" => value = Some(r.cell_value()?),
                "patch" => patch = Some(r.para_patch()?),
                "level" => level = Some(r.usize()?),
                "marker" => marker = Some(r.list_marker()?),
                "ordered" => ordered = Some(r.bool()?),
                "rect" => rect = Some(r.rect()?),
                "deg" => deg = Some(r.number()?),
                "color" => color = Some(r.opt_rgb()?),
                "height" => height = Some(r.number()?),
                "width" => width = Some(r.number()?),
                "border" => border = Some(r.border()?),
                _ => return None,
            }
            Some(())
        })?;

        let addr = addr?;
        let style = style.unwrap_or_default();
        match name.as_deref()? {
            "setRunText" => Some(ModelOp::SetRunText {
                addr,
                run: run?,
                text: text?,
            }),
            "restyleRun" => Some(ModelOp::RestyleRun {
                addr,
                run: run?,
                style,
            }),
            "insertRun" => Some(ModelOp::InsertRun {
                addr,
                run: run?,
                text: text?,
                style,
            }),
            "deleteRun" => Some(ModelOp::DeleteRun { addr, run: run? }),
            "insertBlock" => Some(ModelOp::InsertBlock {
                addr,
                block: block?,
            }),
            "deleteBlock" => Some(ModelOp::DeleteBlock { addr }),
            "moveBlock" => Some(ModelOp::MoveBlock { addr, to: to? }),
            "setBlockText" => Some(ModelOp::SetBlockText { addr, text: text? }),
            "restyleBlock" => Some(ModelOp::RestyleBlock { addr, style }),
            "setCellText" => Some(ModelOp::SetCellText {
                addr,
                row: row?,
                col: col?,
                text: text?,
            }),
            "setSheetCell" => Some(ModelOp::SetSheetCell {
                addr,
                sheet: sheet?,
                row: row?,
                col: col?,
                value: value?,
            }),
            "insertTableRow" => Some(ModelOp::InsertTableRow { addr, at: at? }),
            "deleteTableRow" => Some(ModelOp::DeleteTableRow { addr, at: at? }),
            "insertTableColumn" => Some(ModelOp::InsertTableColumn { addr, at: at? }),
            "deleteTableColumn" => Some(ModelOp::DeleteTableColumn { addr, at: at? }),
            "setCellSpan" => Some(ModelOp::SetCellSpan {
                addr,
                row: row?,
                col: col?,
                col_span: u16_from(col_span?)?,
                row_span: u16_from(row_span?)?,
            }),
            "insertSheetRow" => Some(ModelOp::InsertSheetRow {
                addr,
                sheet: sheet?,
                at: at?,
            }),
            "deleteSheetRow" => Some(ModelOp::DeleteSheetRow {
                addr,
                sheet: sheet?,
                at: at?,
            }),
            "insertSheetColumn" => Some(ModelOp::InsertSheetColumn {
                addr,
                sheet: sheet?,
                at: at?,
            }),
            "deleteSheetColumn" => Some(ModelOp::DeleteSheetColumn {
                addr,
                sheet: sheet?,
                at: at?,
            }),
            "setParagraphStyle" => Some(ModelOp::SetParagraphStyle {
                addr,
                patch: patch?,
            }),
            "setListLevel" => Some(ModelOp::SetListLevel {
                addr,
                level: u8_from(level?)?,
            }),
            "setListMarker" => Some(ModelOp::SetListMarker {
                addr,
                marker: marker?,
            }),
            "setListOrdered" => Some(ModelOp::SetListOrdered {
                addr,
                ordered: ordered?,
            }),
            "setBlockFrame" => Some(ModelOp::SetBlockFrame { addr, rect: rect? }),
            "setBlockRotation" => Some(ModelOp::SetBlockRotation { addr, deg: deg? }),
            "setCellShading" => Some(ModelOp::SetCellShading {
                addr,
                row: row?,
                col: col?,
                color: color?,
            }),
            "setRowHeight" => Some(ModelOp::SetRowHeight {
                addr,
                row: row?,
                height: height?,
            }),
            "setColWidth" => Some(ModelOp::SetColWidth {
                addr,
                col: col?,
                width: width?,
            }),
            "setTableBorder" => Some(ModelOp::SetTableBorder {
                addr,
                border: border?,
            }),
            _ => None,
        }
    }
}

/// Narrow a JSON-parsed `usize` to the model's `u8`, rejecting overflow.
fn u8_from(n: usize) -> Option<u8> {
    u8::try_from(n).ok()
}

/// Narrow a JSON-parsed `usize` span to the model's `u16`, rejecting overflow.
fn u16_from(n: usize) -> Option<u16> {
    u16::try_from(n).ok()
}

/// Parse the model's `generic` tag (mirrors `model::json::parse_generic`).
fn parse_generic_tag(s: &str) -> Option<Generic> {
    match s {
        "sans" => Some(Generic::Sans),
        "serif" => Some(Generic::Serif),
        "mono" => Some(Generic::Mono),
        _ => None,
    }
}

/// Parse the model's `align` tag (mirrors `model::json::parse_align`).
fn parse_align_tag(s: &str) -> Option<crate::model::style::Align> {
    use crate::model::style::Align;
    match s {
        "left" => Some(Align::Left),
        "center" => Some(Align::Center),
        "right" => Some(Align::Right),
        "justify" => Some(Align::Justify),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::style::{Align, CharStyle, LineHeight, ParagraphStyle};
    use crate::model::{Cell, ListItem, Row};
    use crate::model::{Heading, Paragraph, Section, Sheet, SheetBlock, Table};

    fn run(text: &str) -> Inline {
        Inline::Run(InlineRun {
            text: text.to_string(),
            ..InlineRun::default()
        })
    }

    fn para_block(runs: Vec<Inline>) -> Block {
        Block {
            kind: BlockKind::Paragraph(Paragraph {
                runs,
                ..Paragraph::default()
            }),
            ..Block::default()
        }
    }

    /// A one-section, one-page document with the given blocks.
    fn doc_with(blocks: Vec<Block>) -> Document {
        Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks,
                    absolute: false,
                }],
                ..Section::default()
            }],
            ..Document::default()
        }
    }

    fn first_block(doc: &Document) -> &Block {
        &doc.sections[0].pages[0].blocks[0]
    }

    fn run_texts(block: &Block) -> Vec<String> {
        match &block.kind {
            BlockKind::Paragraph(p) => p
                .runs
                .iter()
                .filter_map(|i| match i {
                    Inline::Run(r) => Some(r.text.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    #[test]
    fn set_run_text_changes_targeted_run() {
        let mut doc = doc_with(vec![para_block(vec![run("alpha"), run("beta")])]);
        let n = apply_ops(
            &mut doc,
            &[ModelOp::SetRunText {
                addr: BlockAddr::new(0, 0, 0),
                run: 1,
                text: "BETA".into(),
            }],
        );
        assert_eq!(n, 1);
        assert_eq!(run_texts(first_block(&doc)), vec!["alpha", "BETA"]);
    }

    #[test]
    fn out_of_range_ops_are_no_ops() {
        let mut doc = doc_with(vec![para_block(vec![run("only")])]);
        let before = doc.clone();
        let n = apply_ops(
            &mut doc,
            &[
                // Section out of range.
                ModelOp::SetRunText {
                    addr: BlockAddr::new(5, 0, 0),
                    run: 0,
                    text: "x".into(),
                },
                // Page out of range.
                ModelOp::SetRunText {
                    addr: BlockAddr::new(0, 9, 0),
                    run: 0,
                    text: "x".into(),
                },
                // Block index out of range.
                ModelOp::DeleteBlock {
                    addr: BlockAddr::new(0, 0, 7),
                },
                // Run index out of range.
                ModelOp::DeleteRun {
                    addr: BlockAddr::new(0, 0, 0),
                    run: 9,
                },
            ],
        );
        assert_eq!(n, 0);
        assert_eq!(doc, before, "no-op ops must not mutate the document");
    }

    #[test]
    fn restyle_run_patches_only_named_fields() {
        let mut doc = doc_with(vec![para_block(vec![Inline::Run(InlineRun {
            text: "x".into(),
            style: CharStyle {
                size_pt: 10.0,
                bold: false,
                family: "Times".into(),
                ..CharStyle::default()
            },
            source_index: None,
        })])]);
        apply_ops(
            &mut doc,
            &[ModelOp::RestyleRun {
                addr: BlockAddr::new(0, 0, 0),
                run: 0,
                style: StylePatch {
                    bold: Some(true),
                    color: Some(Some([1.0, 0.0, 0.0])),
                    ..StylePatch::default()
                },
            }],
        );
        let BlockKind::Paragraph(p) = &first_block(&doc).kind else {
            panic!()
        };
        let Inline::Run(r) = &p.runs[0] else { panic!() };
        assert!(r.style.bold, "bold patched");
        assert_eq!(r.style.color, Some([1.0, 0.0, 0.0]), "color patched");
        assert_eq!(r.style.size_pt, 10.0, "size untouched");
        assert_eq!(r.style.family, "Times", "family untouched");
    }

    #[test]
    fn insert_and_delete_run() {
        let mut doc = doc_with(vec![para_block(vec![run("a"), run("c")])]);
        apply_ops(
            &mut doc,
            &[ModelOp::InsertRun {
                addr: BlockAddr::new(0, 0, 0),
                run: 1,
                text: "b".into(),
                style: StylePatch::default(),
            }],
        );
        assert_eq!(run_texts(first_block(&doc)), vec!["a", "b", "c"]);
        apply_ops(
            &mut doc,
            &[ModelOp::DeleteRun {
                addr: BlockAddr::new(0, 0, 0),
                run: 0,
            }],
        );
        assert_eq!(run_texts(first_block(&doc)), vec!["b", "c"]);
    }

    #[test]
    fn insert_delete_and_move_block() {
        let mut doc = doc_with(vec![para_block(vec![run("first")])]);
        // Add a second page so we can move across pages.
        doc.sections[0].pages.push(Page {
            blocks: Vec::new(),
            absolute: false,
        });
        apply_ops(
            &mut doc,
            &[ModelOp::InsertBlock {
                addr: BlockAddr::new(0, 0, 1),
                block: para_block(vec![run("second")]),
            }],
        );
        assert_eq!(doc.sections[0].pages[0].blocks.len(), 2);
        // Move block index 1 of page 0 to page 1.
        let moved = apply_ops(
            &mut doc,
            &[ModelOp::MoveBlock {
                addr: BlockAddr::new(0, 0, 1),
                to: BlockAddr::new(0, 1, 0),
            }],
        );
        assert_eq!(moved, 1);
        assert_eq!(doc.sections[0].pages[0].blocks.len(), 1);
        assert_eq!(doc.sections[0].pages[1].blocks.len(), 1);
        assert_eq!(
            run_texts(&doc.sections[0].pages[1].blocks[0]),
            vec!["second"]
        );
        // Delete the remaining block on page 0.
        apply_ops(
            &mut doc,
            &[ModelOp::DeleteBlock {
                addr: BlockAddr::new(0, 0, 0),
            }],
        );
        assert!(doc.sections[0].pages[0].blocks.is_empty());
    }

    #[test]
    fn set_block_text_and_restyle_block() {
        let mut doc = doc_with(vec![Block {
            kind: BlockKind::Heading(Heading {
                level: 1,
                para: Paragraph {
                    runs: vec![run("old "), run("title")],
                    ..Paragraph::default()
                },
            }),
            ..Block::default()
        }]);
        apply_ops(
            &mut doc,
            &[ModelOp::SetBlockText {
                addr: BlockAddr::new(0, 0, 0),
                text: "New Title".into(),
            }],
        );
        let BlockKind::Heading(h) = &first_block(&doc).kind else {
            panic!()
        };
        assert_eq!(h.para.runs.len(), 1);
        let Inline::Run(r) = &h.para.runs[0] else {
            panic!()
        };
        assert_eq!(r.text, "New Title");

        apply_ops(
            &mut doc,
            &[ModelOp::RestyleBlock {
                addr: BlockAddr::new(0, 0, 0),
                style: StylePatch {
                    italic: Some(true),
                    ..StylePatch::default()
                },
            }],
        );
        let BlockKind::Heading(h) = &first_block(&doc).kind else {
            panic!()
        };
        let Inline::Run(r) = &h.para.runs[0] else {
            panic!()
        };
        assert!(r.style.italic);
    }

    #[test]
    fn set_table_cell_text() {
        let table = Table {
            rows: vec![Row {
                cells: vec![Cell::default(), Cell::default()],
                height: None,
                is_header: false,
            }],
            col_widths: vec![100.0, 100.0],
            ..Table::default()
        };
        let mut doc = doc_with(vec![Block {
            kind: BlockKind::Table(table),
            ..Block::default()
        }]);
        let n = apply_ops(
            &mut doc,
            &[ModelOp::SetCellText {
                addr: BlockAddr::new(0, 0, 0),
                row: 0,
                col: 1,
                text: "cell!".into(),
            }],
        );
        assert_eq!(n, 1);
        let BlockKind::Table(t) = &first_block(&doc).kind else {
            panic!()
        };
        assert_eq!(run_texts(&t.rows[0].cells[1].blocks[0]), vec!["cell!"]);
    }

    // ── structural table ops ──────────────────────────────────────────────────

    /// A `c×r` table of empty single-column cells with equal column widths.
    fn grid_table(rows: usize, cols: usize) -> Table {
        Table {
            rows: (0..rows)
                .map(|_| Row {
                    cells: vec![Cell::default(); cols],
                    height: None,
                    is_header: false,
                })
                .collect(),
            col_widths: vec![100.0; cols],
            ..Table::default()
        }
    }

    fn table_block(table: Table) -> Block {
        Block {
            kind: BlockKind::Table(table),
            ..Block::default()
        }
    }

    fn get_table(doc: &Document) -> &Table {
        let BlockKind::Table(t) = &first_block(doc).kind else {
            panic!("expected a table block")
        };
        t
    }

    fn run_one(doc: &mut Document, op: ModelOp) -> usize {
        apply_ops(doc, &[op])
    }

    #[test]
    fn insert_table_row_adds_full_width_row_at_index() {
        let mut doc = doc_with(vec![table_block(grid_table(2, 3))]);
        let n = run_one(
            &mut doc,
            ModelOp::InsertTableRow {
                addr: BlockAddr::new(0, 0, 0),
                at: 1,
            },
        );
        assert_eq!(n, 1);
        let t = get_table(&doc);
        assert_eq!(t.rows.len(), 3, "one more row");
        assert_eq!(t.rows[1].cells.len(), 3, "new row spans all 3 columns");
        assert!(
            t.rows[1].cells.iter().all(|c| c.col_span == 1),
            "new cells are single-column"
        );
    }

    #[test]
    fn insert_table_row_clamps_past_end() {
        let mut doc = doc_with(vec![table_block(grid_table(2, 2))]);
        run_one(
            &mut doc,
            ModelOp::InsertTableRow {
                addr: BlockAddr::new(0, 0, 0),
                at: 99,
            },
        );
        assert_eq!(get_table(&doc).rows.len(), 3, "clamped to append");
    }

    #[test]
    fn delete_table_row_removes_row_and_shrinks_crossing_rowspans() {
        let mut table = grid_table(3, 2);
        // Make the top-left cell span all 3 rows.
        table.rows[0].cells[0].row_span = 3;
        let mut doc = doc_with(vec![table_block(table)]);
        let n = run_one(
            &mut doc,
            ModelOp::DeleteTableRow {
                addr: BlockAddr::new(0, 0, 0),
                at: 1,
            },
        );
        assert_eq!(n, 1);
        let t = get_table(&doc);
        assert_eq!(t.rows.len(), 2, "one fewer row");
        assert_eq!(
            t.rows[0].cells[0].row_span, 2,
            "span across the deleted row shrinks 3→2"
        );
    }

    #[test]
    fn delete_table_row_out_of_range_is_no_op() {
        let mut doc = doc_with(vec![table_block(grid_table(2, 2))]);
        let before = doc.clone();
        let n = run_one(
            &mut doc,
            ModelOp::DeleteTableRow {
                addr: BlockAddr::new(0, 0, 0),
                at: 9,
            },
        );
        assert_eq!(n, 0);
        assert_eq!(doc, before);
    }

    #[test]
    fn insert_table_column_adds_width_and_cell_per_row() {
        let mut doc = doc_with(vec![table_block(grid_table(2, 3))]);
        let n = run_one(
            &mut doc,
            ModelOp::InsertTableColumn {
                addr: BlockAddr::new(0, 0, 0),
                at: 1,
            },
        );
        assert_eq!(n, 1);
        let t = get_table(&doc);
        assert_eq!(t.col_widths.len(), 4, "one more column width");
        assert_eq!(table_columns(t), 4, "logical columns now 4");
        for row in &t.rows {
            assert_eq!(row.cells.len(), 4, "every row gains a cell");
        }
    }

    #[test]
    fn insert_table_column_inside_span_widens_that_cell() {
        let mut table = grid_table(1, 3);
        // One row whose single cell spans all 3 columns.
        table.rows[0].cells = vec![Cell {
            col_span: 3,
            ..Cell::default()
        }];
        let mut doc = doc_with(vec![table_block(table)]);
        run_one(
            &mut doc,
            ModelOp::InsertTableColumn {
                addr: BlockAddr::new(0, 0, 0),
                at: 1, // strictly inside the span
            },
        );
        let t = get_table(&doc);
        assert_eq!(t.col_widths.len(), 4);
        assert_eq!(t.rows[0].cells.len(), 1, "still a single (wider) cell");
        assert_eq!(t.rows[0].cells[0].col_span, 4, "span widened 3→4");
        assert_eq!(table_columns(t), 4);
    }

    #[test]
    fn delete_table_column_removes_width_and_shrinks_spans() {
        // Row 0: one cell spanning 2 cols, then a single cell  → 3 columns.
        // Row 1: three single cells                            → 3 columns.
        let mut table = grid_table(2, 3);
        table.rows[0].cells = vec![
            Cell {
                col_span: 2,
                ..Cell::default()
            },
            Cell::default(),
        ];
        let mut doc = doc_with(vec![table_block(table)]);
        let n = run_one(
            &mut doc,
            ModelOp::DeleteTableColumn {
                addr: BlockAddr::new(0, 0, 0),
                at: 0, // covered by the span cell in row 0, a single cell in row 1
            },
        );
        assert_eq!(n, 1);
        let t = get_table(&doc);
        assert_eq!(t.col_widths.len(), 2, "3→2 column widths");
        assert_eq!(
            t.rows[0].cells[0].col_span, 1,
            "spanning cell shrinks 2→1"
        );
        assert_eq!(t.rows[0].cells.len(), 2, "row 0 keeps both cells");
        assert_eq!(t.rows[1].cells.len(), 2, "row 1 drops a single cell");
        assert_eq!(table_columns(t), 2);
    }

    #[test]
    fn delete_table_column_out_of_range_is_no_op() {
        let mut doc = doc_with(vec![table_block(grid_table(2, 2))]);
        let before = doc.clone();
        let n = run_one(
            &mut doc,
            ModelOp::DeleteTableColumn {
                addr: BlockAddr::new(0, 0, 0),
                at: 5,
            },
        );
        assert_eq!(n, 0);
        assert_eq!(doc, before);
    }

    #[test]
    fn set_cell_span_clamps_to_at_least_one() {
        let mut doc = doc_with(vec![table_block(grid_table(2, 2))]);
        run_one(
            &mut doc,
            ModelOp::SetCellSpan {
                addr: BlockAddr::new(0, 0, 0),
                row: 0,
                col: 0,
                col_span: 2,
                row_span: 0, // clamps to 1
            },
        );
        let t = get_table(&doc);
        assert_eq!(t.rows[0].cells[0].col_span, 2);
        assert_eq!(t.rows[0].cells[0].row_span, 1, "0 clamped to 1");
    }

    #[test]
    fn structural_table_ops_on_non_table_are_no_ops() {
        let mut doc = doc_with(vec![para_block(vec![run("not a table")])]);
        let before = doc.clone();
        let n = apply_ops(
            &mut doc,
            &[
                ModelOp::InsertTableRow {
                    addr: BlockAddr::new(0, 0, 0),
                    at: 0,
                },
                ModelOp::DeleteTableColumn {
                    addr: BlockAddr::new(0, 0, 0),
                    at: 0,
                },
            ],
        );
        assert_eq!(n, 0);
        assert_eq!(doc, before);
    }

    // ── structural sheet ops ──────────────────────────────────────────────────

    fn sheet_block(sheet: Sheet) -> Block {
        Block {
            kind: BlockKind::Sheet(SheetBlock {
                sheets: vec![sheet],
            }),
            ..Block::default()
        }
    }

    /// A dense `rows × cols` sheet of text cells `"r,c"`, with equal col widths.
    fn dense_sheet(rows: usize, cols: usize) -> Sheet {
        Sheet {
            name: "S".into(),
            rows: (0..rows)
                .map(|r| SheetRow {
                    cells: (0..cols)
                        .map(|c| SheetCell {
                            value: CellValue::Text(format!("{r},{c}")),
                            ..SheetCell::default()
                        })
                        .collect(),
                    ..Default::default()
                })
                .collect(),
            merges: Vec::new(),
            col_widths: vec![50.0; cols],
        }
    }

    fn get_sheet(doc: &Document) -> &Sheet {
        let BlockKind::Sheet(sb) = &first_block(doc).kind else {
            panic!("expected a sheet block")
        };
        &sb.sheets[0]
    }

    fn cell_text(sheet: &Sheet, r: usize, c: usize) -> String {
        match &sheet.rows[r].cells[c].value {
            CellValue::Text(s) => s.clone(),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn insert_sheet_row_shifts_cells_and_merges() {
        let mut sheet = dense_sheet(2, 2);
        sheet.merges.push(MergeRange {
            r0: 1,
            c0: 0,
            r1: 1,
            c1: 1,
        });
        let mut doc = doc_with(vec![sheet_block(sheet)]);
        let n = run_one(
            &mut doc,
            ModelOp::InsertSheetRow {
                addr: BlockAddr::new(0, 0, 0),
                sheet: 0,
                at: 1,
            },
        );
        assert_eq!(n, 1);
        let s = get_sheet(&doc);
        assert_eq!(s.rows.len(), 3, "one more row");
        assert!(s.rows[1].cells.is_empty(), "inserted row is empty");
        assert_eq!(cell_text(s, 2, 0), "1,0", "old row 1 pushed to index 2");
        assert_eq!(s.merges[0].r0, 2, "merge shifted down");
        assert_eq!(s.merges[0].r1, 2);
    }

    #[test]
    fn delete_sheet_row_shifts_up_and_drops_collapsed_merge() {
        let mut sheet = dense_sheet(3, 2);
        // Single-row merge on row 1 (collapses on delete) + a 2-row merge
        // (rows 0..=1, shrinks to a single row).
        sheet.merges.push(MergeRange {
            r0: 1,
            c0: 0,
            r1: 1,
            c1: 1,
        });
        sheet.merges.push(MergeRange {
            r0: 0,
            c0: 0,
            r1: 1,
            c1: 0,
        });
        let mut doc = doc_with(vec![sheet_block(sheet)]);
        let n = run_one(
            &mut doc,
            ModelOp::DeleteSheetRow {
                addr: BlockAddr::new(0, 0, 0),
                sheet: 0,
                at: 1,
            },
        );
        assert_eq!(n, 1);
        let s = get_sheet(&doc);
        assert_eq!(s.rows.len(), 2);
        assert_eq!(cell_text(s, 1, 0), "2,0", "row 2 shifted up to index 1");
        assert_eq!(s.merges.len(), 1, "single-row merge dropped");
        assert_eq!((s.merges[0].r0, s.merges[0].r1), (0, 0), "2-row merge → 1");
    }

    #[test]
    fn insert_sheet_column_shifts_cells_widths_and_merges() {
        let mut sheet = dense_sheet(2, 2);
        sheet.merges.push(MergeRange {
            r0: 0,
            c0: 1,
            r1: 1,
            c1: 1,
        });
        let mut doc = doc_with(vec![sheet_block(sheet)]);
        let n = run_one(
            &mut doc,
            ModelOp::InsertSheetColumn {
                addr: BlockAddr::new(0, 0, 0),
                sheet: 0,
                at: 1,
            },
        );
        assert_eq!(n, 1);
        let s = get_sheet(&doc);
        assert_eq!(s.rows[0].cells.len(), 3, "row gains a cell");
        assert_eq!(s.col_widths.len(), 3, "width slot added");
        assert_eq!(cell_text(s, 0, 2), "0,1", "old col 1 pushed to index 2");
        assert_eq!(s.merges[0].c0, 2, "merge shifted right");
        assert_eq!(s.merges[0].c1, 2);
    }

    #[test]
    fn delete_sheet_column_shifts_left_and_remaps_merge() {
        let mut sheet = dense_sheet(2, 3);
        sheet.merges.push(MergeRange {
            r0: 0,
            c0: 1,
            r1: 0,
            c1: 2,
        });
        let mut doc = doc_with(vec![sheet_block(sheet)]);
        let n = run_one(
            &mut doc,
            ModelOp::DeleteSheetColumn {
                addr: BlockAddr::new(0, 0, 0),
                sheet: 0,
                at: 1,
            },
        );
        assert_eq!(n, 1);
        let s = get_sheet(&doc);
        assert_eq!(s.rows[0].cells.len(), 2, "row loses a cell");
        assert_eq!(s.col_widths.len(), 2);
        assert_eq!(cell_text(s, 0, 1), "0,2", "old col 2 shifted to index 1");
        assert_eq!(
            (s.merges[0].c0, s.merges[0].c1),
            (1, 1),
            "2-col merge (1..=2) → single col 1"
        );
    }

    #[test]
    fn delete_sheet_column_out_of_range_is_no_op() {
        let mut doc = doc_with(vec![sheet_block(dense_sheet(2, 2))]);
        let before = doc.clone();
        let n = run_one(
            &mut doc,
            ModelOp::DeleteSheetColumn {
                addr: BlockAddr::new(0, 0, 0),
                sheet: 0,
                at: 9,
            },
        );
        assert_eq!(n, 0);
        assert_eq!(doc, before);
    }

    #[test]
    fn parse_structural_table_ops_from_json() {
        let ops = parse_ops(
            r#"[
                { "op":"insertTableRow", "addr":[0,0,0], "at":1 },
                { "op":"insertTableColumn", "addr":[0,0,0], "at":0 },
                { "op":"setCellSpan", "addr":[0,0,0], "row":0, "col":0, "col_span":2, "row_span":3 },
                { "op":"deleteTableRow", "addr":[0,0,0], "at":0 },
                { "op":"deleteTableColumn", "addr":[0,0,0], "at":0 }
            ]"#,
        );
        assert_eq!(ops.len(), 5);
        assert_eq!(
            ops[2],
            ModelOp::SetCellSpan {
                addr: BlockAddr::new(0, 0, 0),
                row: 0,
                col: 0,
                col_span: 2,
                row_span: 3,
            }
        );
    }

    #[test]
    fn parse_structural_sheet_ops_from_json() {
        let ops = parse_ops(
            r#"[
                { "op":"insertSheetRow", "addr":[0,0,0], "sheet":0, "at":2 },
                { "op":"deleteSheetColumn", "addr":[0,0,0], "sheet":1, "at":0 }
            ]"#,
        );
        assert_eq!(ops.len(), 2);
        assert_eq!(
            ops[0],
            ModelOp::InsertSheetRow {
                addr: BlockAddr::new(0, 0, 0),
                sheet: 0,
                at: 2,
            }
        );
        assert_eq!(
            ops[1],
            ModelOp::DeleteSheetColumn {
                addr: BlockAddr::new(0, 0, 0),
                sheet: 1,
                at: 0,
            }
        );
    }

    #[test]
    fn structural_table_op_survives_json_round_trip_end_to_end() {
        // A 2×2 table → JSON → apply insertTableColumn via parsed op → still
        // coherent (3 logical columns, every row 3 cells).
        let model = doc_with(vec![table_block(grid_table(2, 2))]);
        let json = model.to_json();
        let mut reparsed = Document::from_json(&json).expect("round-trips");
        let ops = parse_ops(r#"[{ "op":"insertTableColumn", "addr":[0,0,0], "at":2 }]"#);
        assert_eq!(apply_ops(&mut reparsed, &ops), 1);
        let t = get_table(&reparsed);
        assert_eq!(table_columns(t), 3);
        assert!(t.rows.iter().all(|r| r.cells.len() == 3));
        // And it re-serialises stably.
        let again = Document::from_json(&reparsed.to_json()).expect("re-round-trips");
        assert_eq!(again, reparsed);
    }

    #[test]
    fn set_sheet_cell_grows_grid() {
        let mut doc = doc_with(vec![Block {
            kind: BlockKind::Sheet(SheetBlock {
                sheets: vec![Sheet {
                    name: "S".into(),
                    ..Sheet::default()
                }],
            }),
            ..Block::default()
        }]);
        let n = apply_ops(
            &mut doc,
            &[ModelOp::SetSheetCell {
                addr: BlockAddr::new(0, 0, 0),
                sheet: 0,
                row: 2,
                col: 1,
                value: CellValue::Number(42.0),
            }],
        );
        assert_eq!(n, 1);
        let BlockKind::Sheet(sb) = &first_block(&doc).kind else {
            panic!()
        };
        assert_eq!(sb.sheets[0].rows.len(), 3);
        assert_eq!(sb.sheets[0].rows[2].cells[1].value, CellValue::Number(42.0));
    }

    #[test]
    fn parse_empty_ops_is_identity() {
        let mut doc = doc_with(vec![para_block(vec![run("x")])]);
        let before = doc.clone();
        let ops = parse_ops("[]");
        assert!(ops.is_empty());
        assert_eq!(apply_ops(&mut doc, &ops), 0);
        assert_eq!(doc, before);
    }

    #[test]
    fn parse_single_set_run_text_op() {
        let op =
            ModelOp::from_json(r#"{ "op":"setRunText", "addr":[0,0,2], "run":1, "text":"Hi" }"#)
                .expect("parses");
        assert_eq!(
            op,
            ModelOp::SetRunText {
                addr: BlockAddr::new(0, 0, 2),
                run: 1,
                text: "Hi".into(),
            }
        );
    }

    #[test]
    fn parse_ops_array_round_trips_through_apply() {
        let mut doc = doc_with(vec![para_block(vec![run("a"), run("b")])]);
        let ops = parse_ops(
            r#"[
                { "op":"setRunText", "addr":[0,0,0], "run":0, "text":"A" },
                { "op":"restyleRun", "addr":[0,0,0], "run":1,
                  "style": { "bold": true, "size_pt": 20, "generic": "serif", "color": [0,0,1] } }
            ]"#,
        );
        assert_eq!(ops.len(), 2);
        assert_eq!(apply_ops(&mut doc, &ops), 2);
        assert_eq!(run_texts(first_block(&doc)), vec!["A", "b"]);
        let BlockKind::Paragraph(p) = &first_block(&doc).kind else {
            panic!()
        };
        let Inline::Run(r) = &p.runs[1] else { panic!() };
        assert!(r.style.bold);
        assert_eq!(r.style.size_pt, 20.0);
        assert_eq!(r.style.generic, Generic::Serif);
        assert_eq!(r.style.color, Some([0.0, 0.0, 1.0]));
    }

    #[test]
    fn parse_insert_block_op_with_model_block_json() {
        // The block payload is exactly what the model's block serializer emits,
        // so the op accepts whatever `Document::to_json` produced.
        let block_json = crate::model::json::block_to_json(&para_block(vec![run("injected")]));
        let json = format!(r#"{{ "op":"insertBlock", "addr":[0,0,0], "block":{block_json} }}"#);
        let op = ModelOp::from_json(&json).expect("parses insertBlock");
        let mut doc = doc_with(vec![para_block(vec![run("existing")])]);
        assert_eq!(apply_ops(&mut doc, &[op]), 1);
        assert_eq!(doc.sections[0].pages[0].blocks.len(), 2);
        assert_eq!(
            run_texts(&doc.sections[0].pages[0].blocks[0]),
            vec!["injected"]
        );
    }

    // ── paragraph formatting ops ──────────────────────────────────────────────

    fn para_style(doc: &Document) -> &ParagraphStyle {
        match &first_block(doc).kind {
            BlockKind::Paragraph(p) => &p.style,
            BlockKind::Heading(h) => &h.para.style,
            _ => panic!("expected a paragraph/heading block"),
        }
    }

    #[test]
    fn set_paragraph_style_patches_only_named_fields() {
        let mut doc = doc_with(vec![Block {
            kind: BlockKind::Paragraph(Paragraph {
                style: ParagraphStyle {
                    align: Align::Left,
                    indent_left_pt: 5.0,
                    line_height: LineHeight::Normal,
                    ..ParagraphStyle::default()
                },
                runs: vec![run("x")],
                ..Paragraph::default()
            }),
            ..Block::default()
        }]);
        let n = run_one(
            &mut doc,
            ModelOp::SetParagraphStyle {
                addr: BlockAddr::new(0, 0, 0),
                patch: ParaPatch {
                    align: Some(Align::Center),
                    indent_right_pt: Some(12.0),
                    first_line_pt: Some(-9.0),
                    space_before_pt: Some(6.0),
                    space_after_pt: Some(3.0),
                    line_height: Some(LineHeight::Multiple(1.5)),
                    ..ParaPatch::default()
                },
            },
        );
        assert_eq!(n, 1);
        let s = para_style(&doc);
        assert_eq!(s.align, Align::Center, "align patched");
        assert_eq!(s.indent_right_pt, 12.0, "indent_right patched");
        assert_eq!(s.first_line_pt, -9.0, "first_line patched");
        assert_eq!(s.space_before_pt, 6.0, "space_before patched");
        assert_eq!(s.space_after_pt, 3.0, "space_after patched");
        assert_eq!(s.line_height, LineHeight::Multiple(1.5), "line_height patched");
        assert_eq!(s.indent_left_pt, 5.0, "indent_left untouched");
    }

    #[test]
    fn set_paragraph_style_on_heading_targets_inner_paragraph() {
        let mut doc = doc_with(vec![Block {
            kind: BlockKind::Heading(Heading {
                level: 1,
                para: Paragraph {
                    runs: vec![run("Title")],
                    ..Paragraph::default()
                },
            }),
            ..Block::default()
        }]);
        run_one(
            &mut doc,
            ModelOp::SetParagraphStyle {
                addr: BlockAddr::new(0, 0, 0),
                patch: ParaPatch {
                    align: Some(Align::Right),
                    ..ParaPatch::default()
                },
            },
        );
        assert_eq!(para_style(&doc).align, Align::Right);
    }

    #[test]
    fn set_paragraph_style_empty_patch_and_non_paragraph_are_no_ops() {
        // Empty patch → no-op.
        let mut doc = doc_with(vec![para_block(vec![run("x")])]);
        let before = doc.clone();
        assert_eq!(
            run_one(
                &mut doc,
                ModelOp::SetParagraphStyle {
                    addr: BlockAddr::new(0, 0, 0),
                    patch: ParaPatch::default(),
                }
            ),
            0
        );
        assert_eq!(doc, before, "empty patch must not mutate");
        // Non-paragraph block (table) → no-op.
        let mut doc = doc_with(vec![table_block(grid_table(1, 1))]);
        let before = doc.clone();
        assert_eq!(
            run_one(
                &mut doc,
                ModelOp::SetParagraphStyle {
                    addr: BlockAddr::new(0, 0, 0),
                    patch: ParaPatch {
                        align: Some(Align::Center),
                        ..ParaPatch::default()
                    },
                }
            ),
            0
        );
        assert_eq!(doc, before);
    }

    // ── list ops ──────────────────────────────────────────────────────────────

    fn list_block(ordered: bool, marker: ListMarker, levels: &[u8]) -> Block {
        Block {
            kind: BlockKind::List(List {
                ordered,
                marker,
                items: levels
                    .iter()
                    .map(|&level| ListItem {
                        blocks: vec![para_block(vec![run("item")])],
                        level,
                    })
                    .collect(),
            
            ..Default::default()
}),
            ..Block::default()
        }
    }

    fn get_list(doc: &Document) -> &List {
        let BlockKind::List(l) = &first_block(doc).kind else {
            panic!("expected a list block")
        };
        l
    }

    #[test]
    fn set_list_level_sets_every_item_level() {
        let mut doc = doc_with(vec![list_block(false, ListMarker::default(), &[0, 1, 0])]);
        let n = run_one(
            &mut doc,
            ModelOp::SetListLevel {
                addr: BlockAddr::new(0, 0, 0),
                level: 2,
            },
        );
        assert_eq!(n, 1);
        assert!(
            get_list(&doc).items.iter().all(|it| it.level == 2),
            "every item's level set to 2"
        );
    }

    #[test]
    fn set_list_marker_and_ordered() {
        let mut doc = doc_with(vec![list_block(false, ListMarker::Bullet('•'), &[0])]);
        run_one(
            &mut doc,
            ModelOp::SetListMarker {
                addr: BlockAddr::new(0, 0, 0),
                marker: ListMarker::Decimal,
            },
        );
        run_one(
            &mut doc,
            ModelOp::SetListOrdered {
                addr: BlockAddr::new(0, 0, 0),
                ordered: true,
            },
        );
        let l = get_list(&doc);
        assert_eq!(l.marker, ListMarker::Decimal, "marker → decimal");
        assert!(l.ordered, "ordered → true");
    }

    #[test]
    fn list_ops_on_non_list_are_no_ops() {
        let mut doc = doc_with(vec![para_block(vec![run("not a list")])]);
        let before = doc.clone();
        let n = apply_ops(
            &mut doc,
            &[
                ModelOp::SetListLevel {
                    addr: BlockAddr::new(0, 0, 0),
                    level: 1,
                },
                ModelOp::SetListOrdered {
                    addr: BlockAddr::new(0, 0, 0),
                    ordered: true,
                },
            ],
        );
        assert_eq!(n, 0);
        assert_eq!(doc, before);
    }

    // ── absolute block placement ops ──────────────────────────────────────────

    #[test]
    fn set_block_frame_places_the_block() {
        let mut doc = doc_with(vec![para_block(vec![run("x")])]);
        let n = run_one(
            &mut doc,
            ModelOp::SetBlockFrame {
                addr: BlockAddr::new(0, 0, 0),
                rect: Rect::new(10.0, 20.0, 100.0, 50.0),
            },
        );
        assert_eq!(n, 1);
        assert_eq!(first_block(&doc).frame, Some(Rect::new(10.0, 20.0, 100.0, 50.0)));
    }

    #[test]
    fn set_block_rotation_snaps_cardinals_and_keeps_arbitrary() {
        // Cardinal → first-class variant.
        let mut doc = doc_with(vec![para_block(vec![run("x")])]);
        run_one(
            &mut doc,
            ModelOp::SetBlockRotation {
                addr: BlockAddr::new(0, 0, 0),
                deg: 90.0,
            },
        );
        assert_eq!(first_block(&doc).rotation, Rotation::D90);
        // Arbitrary → Deg.
        run_one(
            &mut doc,
            ModelOp::SetBlockRotation {
                addr: BlockAddr::new(0, 0, 0),
                deg: 33.0,
            },
        );
        assert_eq!(first_block(&doc).rotation, Rotation::Deg(33.0));
    }

    #[test]
    fn set_block_frame_out_of_range_is_no_op() {
        let mut doc = doc_with(vec![para_block(vec![run("x")])]);
        let before = doc.clone();
        let n = run_one(
            &mut doc,
            ModelOp::SetBlockFrame {
                addr: BlockAddr::new(0, 0, 9),
                rect: Rect::new(1.0, 2.0, 3.0, 4.0),
            },
        );
        assert_eq!(n, 0);
        assert_eq!(doc, before);
    }

    // ── table shading & geometry ops ──────────────────────────────────────────

    #[test]
    fn set_cell_shading_sets_and_clears() {
        let mut doc = doc_with(vec![table_block(grid_table(1, 2))]);
        run_one(
            &mut doc,
            ModelOp::SetCellShading {
                addr: BlockAddr::new(0, 0, 0),
                row: 0,
                col: 1,
                color: Some([0.9, 0.9, 0.9]),
            },
        );
        assert_eq!(get_table(&doc).rows[0].cells[1].shading, Some([0.9, 0.9, 0.9]));
        // Clear it again.
        run_one(
            &mut doc,
            ModelOp::SetCellShading {
                addr: BlockAddr::new(0, 0, 0),
                row: 0,
                col: 1,
                color: None,
            },
        );
        assert_eq!(get_table(&doc).rows[0].cells[1].shading, None);
    }

    #[test]
    fn set_cell_shading_out_of_range_is_no_op() {
        let mut doc = doc_with(vec![table_block(grid_table(1, 1))]);
        let before = doc.clone();
        let n = run_one(
            &mut doc,
            ModelOp::SetCellShading {
                addr: BlockAddr::new(0, 0, 0),
                row: 5,
                col: 0,
                color: Some([0.0, 0.0, 0.0]),
            },
        );
        assert_eq!(n, 0);
        assert_eq!(doc, before);
    }

    #[test]
    fn set_row_height() {
        let mut doc = doc_with(vec![table_block(grid_table(2, 2))]);
        let n = run_one(
            &mut doc,
            ModelOp::SetRowHeight {
                addr: BlockAddr::new(0, 0, 0),
                row: 1,
                height: 24.0,
            },
        );
        assert_eq!(n, 1);
        assert_eq!(get_table(&doc).rows[1].height, Some(24.0));
    }

    #[test]
    fn set_col_width_sets_and_grows_sparse_widths() {
        // A 3-column table with NO explicit col_widths → SetColWidth must grow it.
        let table = Table {
            rows: vec![Row {
                cells: vec![Cell::default(); 3],
                height: None,
                is_header: false,
            }],
            col_widths: Vec::new(),
            ..Table::default()
        };
        let mut doc = doc_with(vec![table_block(table)]);
        let n = run_one(
            &mut doc,
            ModelOp::SetColWidth {
                addr: BlockAddr::new(0, 0, 0),
                col: 2,
                width: 80.0,
            },
        );
        assert_eq!(n, 1);
        let t = get_table(&doc);
        assert_eq!(t.col_widths.len(), 3, "widths grown to the column count");
        assert_eq!(t.col_widths[2], 80.0, "target width set");
    }

    #[test]
    fn set_col_width_out_of_range_is_no_op() {
        let mut doc = doc_with(vec![table_block(grid_table(1, 2))]);
        let before = doc.clone();
        let n = run_one(
            &mut doc,
            ModelOp::SetColWidth {
                addr: BlockAddr::new(0, 0, 0),
                col: 5,
                width: 99.0,
            },
        );
        assert_eq!(n, 0);
        assert_eq!(doc, before);
    }

    #[test]
    fn set_table_border() {
        let mut doc = doc_with(vec![table_block(grid_table(1, 1))]);
        let n = run_one(
            &mut doc,
            ModelOp::SetTableBorder {
                addr: BlockAddr::new(0, 0, 0),
                border: BorderStyle {
                    width: 1.5,
                    color: [0.0, 0.0, 1.0],
                },
            },
        );
        assert_eq!(n, 1);
        let t = get_table(&doc);
        assert_eq!(t.border.width, 1.5);
        assert_eq!(t.border.color, [0.0, 0.0, 1.0]);
    }

    #[test]
    fn table_geometry_ops_on_non_table_are_no_ops() {
        let mut doc = doc_with(vec![para_block(vec![run("x")])]);
        let before = doc.clone();
        let n = apply_ops(
            &mut doc,
            &[
                ModelOp::SetRowHeight {
                    addr: BlockAddr::new(0, 0, 0),
                    row: 0,
                    height: 10.0,
                },
                ModelOp::SetTableBorder {
                    addr: BlockAddr::new(0, 0, 0),
                    border: BorderStyle::default(),
                },
            ],
        );
        assert_eq!(n, 0);
        assert_eq!(doc, before);
    }

    // ── JSON parsing of the new ops ───────────────────────────────────────────

    #[test]
    fn parse_new_ops_from_json() {
        let ops = parse_ops(
            r#"[
                { "op":"setParagraphStyle", "addr":[0,0,0],
                  "patch": { "align":"center", "indent_left":18, "first_line":-9,
                             "space_after":6, "line_height": {"t":"multiple","v":1.5} } },
                { "op":"setListLevel", "addr":[0,0,0], "level":2 },
                { "op":"setListMarker", "addr":[0,0,0], "marker": {"t":"decimal"} },
                { "op":"setListOrdered", "addr":[0,0,0], "ordered":true },
                { "op":"setBlockFrame", "addr":[0,0,0], "rect": {"x":1,"y":2,"w":3,"h":4} },
                { "op":"setBlockRotation", "addr":[0,0,0], "deg":270 },
                { "op":"setCellShading", "addr":[0,0,0], "row":0, "col":1, "color":[0.5,0.5,0.5] },
                { "op":"setCellShading", "addr":[0,0,0], "row":0, "col":1, "color":null },
                { "op":"setRowHeight", "addr":[0,0,0], "row":0, "height":24 },
                { "op":"setColWidth", "addr":[0,0,0], "col":1, "width":80 },
                { "op":"setTableBorder", "addr":[0,0,0], "border": {"width":1,"color":[0,0,0]} }
            ]"#,
        );
        assert_eq!(ops.len(), 11);
        assert_eq!(
            ops[0],
            ModelOp::SetParagraphStyle {
                addr: BlockAddr::new(0, 0, 0),
                patch: ParaPatch {
                    align: Some(Align::Center),
                    indent_left_pt: Some(18.0),
                    first_line_pt: Some(-9.0),
                    space_after_pt: Some(6.0),
                    line_height: Some(LineHeight::Multiple(1.5)),
                    ..ParaPatch::default()
                },
            }
        );
        assert_eq!(
            ops[1],
            ModelOp::SetListLevel {
                addr: BlockAddr::new(0, 0, 0),
                level: 2,
            }
        );
        assert_eq!(
            ops[5],
            ModelOp::SetBlockRotation {
                addr: BlockAddr::new(0, 0, 0),
                deg: 270.0,
            }
        );
        assert_eq!(
            ops[6],
            ModelOp::SetCellShading {
                addr: BlockAddr::new(0, 0, 0),
                row: 0,
                col: 1,
                color: Some([0.5, 0.5, 0.5]),
            }
        );
        assert_eq!(
            ops[7],
            ModelOp::SetCellShading {
                addr: BlockAddr::new(0, 0, 0),
                row: 0,
                col: 1,
                color: None,
            }
        );
    }

    #[test]
    fn new_op_survives_json_round_trip_end_to_end() {
        // Apply a parsed paragraph-style op against a model that round-trips
        // through JSON, then re-serialise to prove stability.
        let model = doc_with(vec![para_block(vec![run("hello")])]);
        let mut reparsed = Document::from_json(&model.to_json()).expect("round-trips");
        let ops = parse_ops(
            r#"[{ "op":"setParagraphStyle", "addr":[0,0,0],
                 "patch": { "align":"justify", "indent_left":24 } }]"#,
        );
        assert_eq!(apply_ops(&mut reparsed, &ops), 1);
        assert_eq!(para_style(&reparsed).align, Align::Justify);
        assert_eq!(para_style(&reparsed).indent_left_pt, 24.0);
        let again = Document::from_json(&reparsed.to_json()).expect("re-round-trips");
        assert_eq!(again, reparsed);
    }

    // ── end-to-end: a real PDF → model → edit → export ────────────────────────

    /// Build a one-page PDF with a single text line, via the lib's own builder.
    fn build_pdf_with_line(text: &str) -> Vec<u8> {
        use crate::convert::build::{PdfBuilder, StdFont};
        let mut b = PdfBuilder::new();
        let page = b.add_page(612.0, 792.0);
        b.text(page, 72.0, 100.0, 12.0, text, StdFont::Helvetica, [0.0; 3]);
        b.finish()
    }

    /// The first paragraph block's concatenated run text, across a reconstructed
    /// document's pages.
    fn first_paragraph_text(doc: &Document) -> Option<String> {
        for section in &doc.sections {
            for page in &section.pages {
                for block in &page.blocks {
                    if let BlockKind::Paragraph(p) = &block.kind {
                        let s: String = p
                            .runs
                            .iter()
                            .filter_map(|i| match i {
                                Inline::Run(r) => Some(r.text.as_str()),
                                _ => None,
                            })
                            .collect();
                        return Some(s);
                    }
                }
            }
        }
        None
    }

    #[test]
    fn reconstruct_model_from_real_pdf_has_a_paragraph() {
        let pdf = build_pdf_with_line("Hello reconstruction");
        let model = crate::Document::open(&pdf)
            .expect("valid PDF")
            .reconstruct_model();
        let text = first_paragraph_text(&model).expect("a paragraph block");
        assert!(
            text.contains("Hello"),
            "reconstructed paragraph should carry the source text, got {text:?}"
        );
    }

    #[test]
    fn json_round_trip_is_stable_after_edit() {
        let pdf = build_pdf_with_line("Original line");
        let mut model = crate::Document::open(&pdf)
            .expect("valid PDF")
            .reconstruct_model();
        // Edit the first run of the first paragraph block.
        let addr = first_paragraph_addr(&model).expect("a paragraph block");
        let n = apply_ops(
            &mut model,
            &[ModelOp::SetBlockText {
                addr,
                text: "Edited line".into(),
            }],
        );
        assert_eq!(n, 1);
        assert_eq!(first_paragraph_text(&model).as_deref(), Some("Edited line"));
        // to_json → from_json must reproduce the edited model exactly.
        let json = model.to_json();
        let reparsed = Document::from_json(&json).expect("round-trips");
        assert_eq!(reparsed, model);
    }

    /// The address of the first paragraph block in a (reconstructed) document.
    fn first_paragraph_addr(doc: &Document) -> Option<BlockAddr> {
        for (si, section) in doc.sections.iter().enumerate() {
            for (pi, page) in section.pages.iter().enumerate() {
                for (bi, block) in page.blocks.iter().enumerate() {
                    if matches!(block.kind, BlockKind::Paragraph(_)) {
                        return Some(BlockAddr::new(si, pi, bi));
                    }
                }
            }
        }
        None
    }

    #[test]
    fn docx_from_model_is_a_zip() {
        let model = doc_with(vec![para_block(vec![run("export me")])]);
        let bytes = crate::convert::export_model::docx_from_model(&model);
        assert!(
            bytes.starts_with(b"PK\x03\x04"),
            "DOCX must be a ZIP (PK\\x03\\x04)"
        );
    }

    #[test]
    fn pdf_from_model_starts_with_pdf_header() {
        let model = doc_with(vec![para_block(vec![run("export me")])]);
        let bytes = crate::convert::project::pdf_from_model(&model);
        assert!(bytes.starts_with(b"%PDF"), "must begin with %PDF");
    }
}
