//! Character- and paragraph-level styling, plus a named style table, for the
//! unified editable document model.
//!
//! The [`Generic`] font class is **reused** from the Office exporters
//! ([`crate::convert::style::Generic`]) so a single notion of serif/sans/mono
//! flows through extraction, the model, and reconstruction. Sizes and spacing
//! are in **PDF points** (`f64`).

use crate::convert::style::Generic;
use std::collections::BTreeMap;

/// Horizontal paragraph alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
    Justify,
}

/// Run vertical alignment relative to the baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VAlign {
    #[default]
    Baseline,
    Super,
    Sub,
}

/// Vertical alignment of a cell's content within its box, for table cells
/// ([`Cell`](crate::model::Cell)) and spreadsheet cells
/// ([`SheetCell`](crate::model::SheetCell)). Distinct from [`VAlign`], which is
/// run-level superscript/subscript.
///
/// Used as `Option<CellVAlign>`: `None` ⇒ the format's default — `Top` for
/// word-processing/ODF table cells (DOCX `w:vAlign` default, ODF
/// `style:vertical-align` default), `Bottom` for spreadsheet cells (the OOXML
/// `CT_CellAlignment@vertical` default). [`Default`] is [`Top`](CellVAlign::Top),
/// matching the table-cell convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CellVAlign {
    #[default]
    Top,
    Middle,
    Bottom,
}

/// Line height (leading) policy for a paragraph.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum LineHeight {
    /// The font's natural leading.
    #[default]
    Normal,
    /// A multiple of the font size (e.g. `1.5` for 150%).
    Multiple(f64),
    /// A fixed leading in points.
    Points(f64),
}

/// A single run's character style. `family` is the display family name; the
/// [`generic`](CharStyle::generic) class is the portable fallback when that
/// family is not installed.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CharStyle {
    /// Display family name (e.g. "Helvetica", "Times New Roman").
    pub family: String,
    /// Portable fallback class (serif / sans / mono).
    pub generic: Generic,
    /// Font size in points.
    pub size_pt: f64,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    /// RGB fill colour, components `0.0..=1.0`. `None` = default (black).
    pub color: Option<[f64; 3]>,
    /// RGB text-highlight / run background, components `0.0..=1.0`. `None` = no
    /// highlight. Mirrors a word-processor's text highlight (`w:highlight`/
    /// `w:shd` in DOCX, `fo:background-color` in ODF).
    pub background: Option<[f64; 3]>,
    pub vertical_align: VAlign,
}

impl CharStyle {
    /// Whether two styles are close enough to coalesce adjacent runs into one:
    /// same family/generic, bold/italic/underline/strike, color, background,
    /// vertical-align, and font size within 0.5pt. A `size_pt` of 0 (unset /
    /// inherited) matches any size so an inherited-style run coalesces with an
    /// explicit one. This prevents the "every word is a separate run" problem
    /// that plagues imports from formats with per-run style inheritance.
    pub fn is_compatible_with(&self, other: &CharStyle) -> bool {
        let same_text_style = self.family == other.family
            && self.generic == other.generic
            && self.bold == other.bold
            && self.italic == other.italic
            && self.underline == other.underline
            && self.strike == other.strike
            && self.color == other.color
            && self.background == other.background
            && self.vertical_align == other.vertical_align;
        if !same_text_style {
            return false;
        }
        if self.size_pt == 0.0 || other.size_pt == 0.0 {
            return true;
        }
        (self.size_pt - other.size_pt).abs() < 0.5
    }
}

/// One side of a paragraph border (points + colour + style), mirroring the
/// table `BorderSide` but owned by the model so exporters can emit it directly.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ParaBorder {
    /// Border width in points.
    pub width_pt: f64,
    /// Border style: `solid`, `dashed`, `dotted`, `double`, `none`.
    pub style: String,
    /// RGB colour, components `0.0..=1.0`.
    pub color: [f64; 3],
}

/// Paragraph-level formatting: alignment, spacing, indentation, and leading.
/// Spacing and indents are in points.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ParagraphStyle {
    pub align: Align,
    pub space_before_pt: f64,
    pub space_after_pt: f64,
    pub indent_left_pt: f64,
    pub indent_right_pt: f64,
    /// First-line indent (positive) or hanging indent (negative), in points.
    pub first_line_pt: f64,
    pub line_height: LineHeight,
    /// Paragraph background shading (RGB `0..=1`). `None` = no shading.
    pub background: Option<[f64; 3]>,
    /// Per-side paragraph borders `[top, right, bottom, left]`. `width_pt == 0`
    /// means no border on that side.
    pub borders: [Option<ParaBorder>; 4],
    /// `keep-with-next`: keep this paragraph on the same page as the next.
    pub keep_with_next: bool,
    /// `keep-together`: prevent splitting this paragraph across pages.
    pub keep_together: bool,
}

/// A named style (paragraph + character defaults), optionally derived from
/// another named style via `based_on`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NamedStyle {
    pub para: ParagraphStyle,
    pub char_: CharStyle,
    pub based_on: Option<StyleId>,
}

/// A stable identifier for a named style (e.g. `"Heading1"`, `"Normal"`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct StyleId(pub String);

/// The document's table of named styles, keyed by [`StyleId`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StyleTable {
    pub named: BTreeMap<StyleId, NamedStyle>,
}
