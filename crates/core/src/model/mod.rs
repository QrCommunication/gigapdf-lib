//! Unified editable document model — the format-neutral tree that every
//! importer (PDF, DOCX, XLSX, PPTX, HTML, …) lowers *into* and every exporter
//! reconstructs *from*.
//!
//! **Phase 0 (this module): the types and a stable JSON round-trip only.** No
//! importers, exporters, reconstruction, or edit operations live here yet — this
//! is the foundation those later phases build on.
//!
//! ## Shape
//!
//! ```text
//! Document
//! ├─ meta:      DocMeta                 (title / author / … / lang)
//! ├─ styles:    StyleTable              (named paragraph + char styles)
//! ├─ sections:  Vec<Section>            (geometry + optional header/footer + pages)
//! │              └─ pages: Vec<Page>    (a list of Blocks; `absolute` layout flag)
//! │                          └─ blocks: Vec<Block>
//! │                                      └─ kind: BlockKind
//! │                                          (Paragraph / Heading / List / Table /
//! │                                           Image / Shape / TextBox / Sheet / Slide)
//! ├─ outline:   Vec<OutlineNode>        (bookmarks tree → page index)
//! └─ resources: ResourceTable           (content-addressed image blobs)
//! ```
//!
//! Measurements are **PDF points** (`f64`); colours are RGB `[f64; 3]` in
//! `0.0..=1.0`. Reused leaves: [`geom`]/[`style`] (with
//! [`Generic`](crate::convert::style::Generic)), [`PathSeg`](crate::content::vector::PathSeg)
//! for vector shapes.

pub mod edit;
pub mod geom;
pub mod json;
pub mod sheet;
pub mod slide;
pub mod style;

pub use edit::{apply_ops, parse_ops, BlockAddr, ModelOp, StylePatch};
pub use geom::{Margins, PageGeometry, Rect, Rotation};
pub use sheet::{CellValue, MergeRange, Sheet, SheetBlock, SheetCell, SheetRow};
pub use slide::{Placeholder, PlaceholderRole, Slide, SlideBlock};
pub use style::{
    Align, CharStyle, LineHeight, NamedStyle, ParagraphStyle, StyleId, StyleTable, VAlign,
};

use crate::content::vector::PathSeg;

/// A complete document.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Document {
    pub meta: DocMeta,
    pub styles: StyleTable,
    pub sections: Vec<Section>,
    pub outline: Vec<OutlineNode>,
    pub resources: ResourceTable,
}

/// Document-level metadata.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DocMeta {
    pub title: Option<String>,
    pub author: Option<String>,
    pub subject: Option<String>,
    pub keywords: Vec<String>,
    /// BCP-47 language tag (e.g. `"en"`, `"fr-FR"`).
    pub lang: Option<String>,
}

/// A document section: one page geometry, optional running header/footer
/// (themselves block lists), and the section's pages.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Section {
    pub geometry: PageGeometry,
    pub header: Option<Vec<Block>>,
    pub footer: Option<Vec<Block>>,
    pub pages: Vec<Page>,
}

/// A single page: a list of blocks. When `absolute` is set the blocks are
/// positioned by their [`Block::frame`] (slide / form layout); otherwise they
/// flow top-to-bottom (prose).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Page {
    pub blocks: Vec<Block>,
    pub absolute: bool,
}

/// A block-level node: an optional placement frame + rotation, plus its kind.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub id: BlockId,
    /// Absolute placement box (used when the containing [`Page`] is `absolute`,
    /// or for floated boxes). `None` ⇒ flow layout.
    pub frame: Option<Rect>,
    pub rotation: Rotation,
    pub kind: BlockKind,
}

impl Default for Block {
    fn default() -> Self {
        Self {
            id: BlockId::default(),
            frame: None,
            rotation: Rotation::default(),
            kind: BlockKind::Paragraph(Paragraph::default()),
        }
    }
}

/// A stable per-document block identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub struct BlockId(pub u64);

/// The payload of a [`Block`].
#[derive(Debug, Clone, PartialEq)]
pub enum BlockKind {
    Paragraph(Paragraph),
    Heading(Heading),
    List(List),
    Table(Table),
    Image(ImageRef),
    Shape(Shape),
    TextBox(TextBox),
    Sheet(sheet::SheetBlock),
    Slide(slide::SlideBlock),
}

impl Default for BlockKind {
    fn default() -> Self {
        BlockKind::Paragraph(Paragraph::default())
    }
}

/// A paragraph: its own style (optionally referencing a named style) and a run
/// of inline content.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Paragraph {
    pub style: ParagraphStyle,
    /// Named style this paragraph derives from, if any.
    pub style_ref: Option<StyleId>,
    pub runs: Vec<Inline>,
}

/// A heading (`level` 1..=6) wrapping a paragraph.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Heading {
    pub level: u8,
    pub para: Paragraph,
}

/// Inline (within-paragraph) content.
#[derive(Debug, Clone, PartialEq)]
pub enum Inline {
    Run(InlineRun),
    LineBreak,
    Image(ImageRef),
    Link {
        href: LinkTarget,
        children: Vec<Inline>,
    },
}

/// A styled span of text. `source_index` optionally records the originating
/// run's index in the source document (for round-tripping back to the exact
/// content-stream operator).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct InlineRun {
    pub text: String,
    pub style: CharStyle,
    pub source_index: Option<usize>,
}

/// An ordered or unordered list.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct List {
    pub ordered: bool,
    pub marker: ListMarker,
    pub items: Vec<ListItem>,
}

/// One list item: a list of blocks at a given nesting `level`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ListItem {
    pub blocks: Vec<Block>,
    pub level: u8,
}

/// The bullet/number style for a [`List`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListMarker {
    Bullet(char),
    Decimal,
    LowerAlpha,
    UpperAlpha,
    LowerRoman,
    UpperRoman,
}

impl Default for ListMarker {
    fn default() -> Self {
        ListMarker::Bullet('•')
    }
}

/// A table: rows of cells, explicit column widths (points), and a border style.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Table {
    pub rows: Vec<Row>,
    pub col_widths: Vec<f64>,
    pub border: BorderStyle,
}

/// A table row with optional fixed height (points).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Row {
    pub cells: Vec<Cell>,
    pub height: Option<f64>,
}

/// A table cell: block content plus span and optional background shading.
#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    pub blocks: Vec<Block>,
    pub col_span: u16,
    pub row_span: u16,
    /// RGB background, components `0.0..=1.0`. `None` = no shading.
    pub shading: Option<[f64; 3]>,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            blocks: Vec::new(),
            col_span: 1,
            row_span: 1,
            shading: None,
        }
    }
}

/// A reference to an image blob in the [`ResourceTable`]. `resource` is the
/// content-hash key (see [`ResourceTable::images`]).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ImageRef {
    pub resource: u64,
    pub alt: Option<String>,
}

/// A vector shape: a path (reusing [`PathSeg`]) with fill/stroke styling. Stroke
/// width and the `dash` pattern are in points.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Shape {
    pub segments: Vec<PathSeg>,
    /// RGB fill, components `0.0..=1.0`. `None` = unfilled.
    pub fill: Option<[f64; 3]>,
    /// RGB stroke, components `0.0..=1.0`. `None` = no stroke.
    pub stroke: Option<[f64; 3]>,
    pub stroke_width: f64,
    pub dash: Vec<f64>,
}

/// A free-floating text box. Holds a list of blocks (paragraphs, lists, nested
/// tables…) so it composes with the rest of the model uniformly.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TextBox {
    pub blocks: Vec<Block>,
}

/// A bookmark/outline entry pointing at a page index, with nested children.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct OutlineNode {
    pub title: String,
    /// Zero-based page index within the document's flattened page sequence.
    pub page: usize,
    pub children: Vec<OutlineNode>,
}

/// Content-addressed image store. The `u64` key is a content hash referenced by
/// [`ImageRef::resource`]; identical bytes are stored once.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ResourceTable {
    pub images: std::collections::BTreeMap<u64, ImageResource>,
}

/// A stored image blob and its format tag (e.g. `"png"`, `"jpeg"`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ImageResource {
    pub bytes: Vec<u8>,
    pub format: String,
}

/// The destination of a hyperlink.
#[derive(Debug, Clone, PartialEq)]
pub enum LinkTarget {
    /// An external URL.
    Url(String),
    /// An internal jump to a zero-based page index.
    Page(usize),
}

impl Default for LinkTarget {
    fn default() -> Self {
        LinkTarget::Url(String::new())
    }
}

/// A table/cell border. The default is a hairline-less `0pt` black border
/// (i.e. "no border" until widened).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BorderStyle {
    pub width: f64,
    pub color: [f64; 3],
}
