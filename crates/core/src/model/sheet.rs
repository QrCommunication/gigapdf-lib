//! Spreadsheet content for the unified editable document model.
//!
//! A [`SheetBlock`] holds one or more [`Sheet`]s; each is a grid of typed
//! [`SheetCell`]s with optional merge ranges and per-column widths. This is the
//! editable counterpart of the XLSX/ODS reconstruction path — typed values, not
//! a rasterised table.

use crate::model::style::{Align, CharStyle};
use crate::model::BorderStyle;

/// A block of spreadsheet content: one or more named sheets.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SheetBlock {
    pub sheets: Vec<Sheet>,
}

/// A single named worksheet.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Sheet {
    pub name: String,
    pub rows: Vec<SheetRow>,
    pub merges: Vec<MergeRange>,
    /// Per-column widths in points; shorter than the widest row ⇒ defaults.
    pub col_widths: Vec<f64>,
}

/// One row of cells, with an optional fixed height (points).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SheetRow {
    pub cells: Vec<SheetCell>,
    /// Fixed row height in points. `None` ⇒ the suite's default/auto height.
    pub height: Option<f64>,
}

/// A single cell: a typed value plus optional number format, fill, and style.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SheetCell {
    pub value: CellValue,
    /// The cell's formula expression without the leading `=` (e.g. `"SUM(A1:A9)"`),
    /// as authored in the source (`<f>` in XLSX). `None` ⇒ a literal cell. The
    /// cached evaluated result is kept in [`value`](SheetCell::value) for display.
    pub formula: Option<String>,
    /// Spreadsheet number format code (e.g. `"0.00"`, `"yyyy-mm-dd"`).
    pub number_format: Option<String>,
    /// RGB cell background, components `0.0..=1.0`. `None` = no fill.
    pub fill: Option<[f64; 3]>,
    pub style: CharStyle,
    /// Cell border (all four edges). `None` ⇒ no border.
    pub border: Option<BorderStyle>,
    /// Horizontal text alignment. `None` ⇒ the suite's default (general).
    pub align: Option<Align>,
    /// Wrap text within the cell. `false` ⇒ no wrapping (default).
    pub wrap: bool,
}

/// A cell's typed value.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum CellValue {
    #[default]
    Empty,
    Text(String),
    Number(f64),
    Bool(bool),
}

/// An inclusive merged-cell rectangle, `(r0, c0)`..=`(r1, c1)`, zero-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MergeRange {
    pub r0: usize,
    pub c0: usize,
    pub r1: usize,
    pub c1: usize,
}
