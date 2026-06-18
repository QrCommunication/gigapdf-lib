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
    pub vertical_align: VAlign,
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
