//! Spreadsheet content for the unified editable document model.
//!
//! A [`SheetBlock`] holds one or more [`Sheet`]s; each is a grid of typed
//! [`SheetCell`]s with optional merge ranges and per-column widths. This is the
//! editable counterpart of the XLSX/ODS reconstruction path — typed values, not
//! a rasterised table.

use crate::model::style::{Align, CellVAlign, CharStyle, Direction};
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
    /// Frozen panes as `(rows, cols)`: the number of leading rows and columns
    /// pinned while the rest of the sheet scrolls (XLSX `<pane ySplit xSplit
    /// state="frozen">`). `(1, 0)` freezes the header row. `None` ⇒ no freeze.
    /// Additive: never serialized when unset.
    pub freeze: Option<(u32, u32)>,
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
    /// Vertical text alignment within the cell. `None` ⇒ the suite's default
    /// (`Bottom` for spreadsheet cells, per the OOXML `CT_CellAlignment@vertical`
    /// default). Round-trips XLSX `xf/alignment@vertical` and ODS
    /// `style:table-cell-properties@style:vertical-align`.
    pub vertical_align: Option<CellVAlign>,
    /// Wrap text within the cell. `false` ⇒ no wrapping (default).
    pub wrap: bool,
    /// Cell hyperlink target (an external URL, or an in-workbook `#Sheet!A1`
    /// location). `None` ⇒ no link.
    pub hyperlink: Option<String>,
    /// Text rotation in **signed degrees counter-clockwise** (`+45` slants
    /// up-right, `-45` down-right), from XLSX `alignment@textRotation` (whose
    /// `0..=90` maps to `+0..=+90` and `91..=180` to `-1..=-90`; the vertical
    /// marker `255` is not modelled). `None` ⇒ horizontal. Additive.
    pub rotation_deg: Option<i32>,
    /// Diagonal border line(s) across the cell (XLSX `border/diagonal` with
    /// `@diagonalUp`/`@diagonalDown`). `None` ⇒ none. Additive.
    pub diagonal: Option<DiagonalBorder>,
    /// Base reading direction of the cell's text, from XLSX
    /// `alignment@readingOrder` (`2` ⇒ right-to-left, `1` ⇒ left-to-right, `0`
    /// / absent ⇒ context-dependent) and ODS
    /// `style:table-cell-properties@style:writing-mode` (`rl-tb` ⇒ RTL). `None`
    /// ⇒ unspecified (reads left-to-right). Reuses the model [`Direction`] enum.
    /// Additive: never serialized when unset.
    pub direction: Option<Direction>,
}

/// A diagonal cell border: which diagonal(s) are drawn and the line to draw
/// them with. `up` runs bottom-left → top-right (XLSX `@diagonalUp`), `down`
/// top-left → bottom-right (`@diagonalDown`); both may be set (an X).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct DiagonalBorder {
    pub up: bool,
    pub down: bool,
    pub border: BorderStyle,
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
