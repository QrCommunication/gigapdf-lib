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
//!   [`RestyleBlock`](ModelOp::RestyleBlock).
//! - table cell: [`SetCellText`](ModelOp::SetCellText).
//! - sheet cell: [`SetSheetCell`](ModelOp::SetSheetCell).
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
use crate::model::{Block, BlockId, BlockKind, CellValue, Document, Inline, InlineRun, Page};

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
        let mut text: Option<String> = None;
        let mut style: Option<StylePatch> = None;
        let mut block: Option<Block> = None;
        let mut value: Option<CellValue> = None;

        self.object(|r, k| {
            match k {
                "op" => name = Some(r.string()?),
                "addr" => addr = Some(r.addr()?),
                "to" => to = Some(r.addr()?),
                "run" => run = Some(r.usize()?),
                "row" => row = Some(r.usize()?),
                "col" => col = Some(r.usize()?),
                "sheet" => sheet = Some(r.usize()?),
                "text" => text = Some(r.string()?),
                "style" => style = Some(r.style_patch()?),
                "block" => block = Some(r.block()?),
                "value" => value = Some(r.cell_value()?),
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
            _ => None,
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::style::CharStyle;
    use crate::model::{Cell, Row};
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
