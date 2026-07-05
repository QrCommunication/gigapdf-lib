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
//! │                                           Image / Shape / TextBox / Sheet / Slide /
//! │                                           CodeBlock / Blockquote / HorizontalRule)
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
pub use sheet::{
    CellValue, DiagonalBorder, MergeRange, Sheet, SheetBlock, SheetCell, SheetRow,
};
pub use slide::{Placeholder, PlaceholderRole, Slide, SlideBlock};
pub use style::{
    Align, CellVAlign, CharStyle, Direction, LineHeight, NamedStyle, ParaBorder, ParagraphStyle,
    StyleId, StyleTable, TabAlign, TabStop, VAlign,
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
    /// Review comments / annotations imported from the source document (DOCX
    /// `word/comments.xml`, ODT `office:annotation`). Each is anchored in the
    /// body by a matching [`Inline::CommentRef`] (by [`Comment::id`]); HTML
    /// renders them as an endnote-style list. Empty ⇒ none.
    pub comments: Vec<Comment>,
    /// Foot-/endnotes imported from the source document (DOCX
    /// `word/footnotes.xml`/`word/endnotes.xml`, RTF `{\footnote …}`). Each is
    /// anchored in the body by a matching [`Inline::FootnoteRef`] (by
    /// [`Footnote::id`]); renderers synthesize the superscript call marker and
    /// a notes section at the bottom. Empty ⇒ none (additive — documents
    /// without notes serialize exactly as before).
    pub footnotes: Vec<Footnote>,
}

/// A foot-/endnote: its document-unique `id` (matched by an
/// [`Inline::FootnoteRef`] anchor in the body) and its body as a full block
/// list (paragraphs, lists, …), mirroring the [`Comment`] pattern but keeping
/// the note's structure instead of flattened text. The display ordinal is the
/// note's 1-based position in [`Document::footnotes`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Footnote {
    pub id: String,
    pub blocks: Vec<Block>,
}

/// A review comment / annotation: its document-unique `id` (matched by an
/// [`Inline::CommentRef`] anchor in the body), `author`, raw `date` text (kept
/// verbatim — ISO-8601 / W3CDTF as the source wrote it, `""` ⇒ absent), and the
/// flattened plain-text body. Maps DOCX `w:comment` (`w:id`/`w:author`/`w:date`)
/// and ODF `office:annotation` (`office:name`/`dc:creator`/`dc:date`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Comment {
    pub id: String,
    pub author: String,
    pub date: String,
    pub text: String,
}

/// Document-level metadata.
///
/// `title`/`author`/`subject`/`keywords`/`lang` are the core five. The remaining
/// fields mirror the extra OOXML core/extended properties
/// (`docProps/core.xml` + `app.xml`) and ODF `meta.xml` properties so an Office
/// import round-trips them. They are stored as raw strings — empty (`""`) means
/// absent, matching the project convention; dates keep their source ISO-8601 /
/// W3CDTF text (no date type is introduced).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DocMeta {
    pub title: Option<String>,
    pub author: Option<String>,
    pub subject: Option<String>,
    pub keywords: Vec<String>,
    /// BCP-47 language tag (e.g. `"en"`, `"fr-FR"`).
    pub lang: Option<String>,
    /// Free-text description / abstract (`dc:description`).
    pub description: String,
    /// Creation timestamp, raw ISO-8601 / W3CDTF text (`dcterms:created` /
    /// `meta:creation-date`).
    pub created: String,
    /// Last-modification timestamp, raw ISO-8601 / W3CDTF text
    /// (`dcterms:modified` / `dc:date`).
    pub modified: String,
    /// Name of the last person to modify the document (`cp:lastModifiedBy`).
    pub last_modified_by: String,
    /// Revision number, kept verbatim as text (`cp:revision`).
    pub revision: String,
    /// Generating application (OOXML `app.xml` `<Application>`).
    pub application: String,
    /// Company the document belongs to (OOXML `app.xml` `<Company>`).
    pub company: String,
    /// Producer / generator string (ODF `meta:generator`).
    pub generator: String,
    /// Editing-cycle count, kept verbatim as text (ODF `meta:editing-cycles`).
    pub editing_cycles: String,
}

/// A document section: one page geometry, optional running header/footer
/// (themselves block lists), and the section's pages.
///
/// Beyond the default running [`header`](Self::header)/[`footer`](Self::footer),
/// a section may carry **first-page** and **even-page** variants (DOCX
/// `w:titlePg` + `w:headerReference w:type="first"`, `w:evenAndOddHeaders` +
/// `w:type="even"`; ODT `style:header-first` / left-page master styles).
/// `None` ⇒ the variant is not distinct (the default applies everywhere);
/// `Some(vec![])` ⇒ the variant exists but is **blank** (Word's "different
/// first page" with an empty first-page header). All four are additive: never
/// serialized when `None`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Section {
    pub geometry: PageGeometry,
    pub header: Option<Vec<Block>>,
    pub footer: Option<Vec<Block>>,
    /// Distinct first-page header (`w:titlePg`).
    pub header_first: Option<Vec<Block>>,
    /// Distinct first-page footer (`w:titlePg`).
    pub footer_first: Option<Vec<Block>>,
    /// Distinct even-page header (`w:evenAndOddHeaders`).
    pub header_even: Option<Vec<Block>>,
    /// Distinct even-page footer (`w:evenAndOddHeaders`).
    pub footer_even: Option<Vec<Block>>,
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
    /// A preformatted code block: verbatim text with an optional language hint
    /// (the info-string of a Markdown fence). Rendered monospaced/preformatted.
    CodeBlock(CodeBlock),
    /// A block quotation: a nested block list, set off (indented + a left rule)
    /// from the surrounding flow. Composes recursively like [`TextBox`].
    Blockquote(Blockquote),
    /// A thematic break (horizontal rule) — a semantic divider between blocks.
    HorizontalRule,
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
    /// An anchor marking where a review [`Comment`] (matched by `id`) attaches in
    /// the run flow. Carries no text of its own: HTML renders it as a small
    /// superscript reference marker linking to the comment body; exporters with
    /// no comment syntax (Markdown/RTF) and plain-text emit nothing for it.
    CommentRef {
        id: String,
    },
    /// An anchor marking where a foot-/endnote (matched by `id` against
    /// [`Document::footnotes`]) is referenced in the run flow. Carries no text
    /// of its own: renderers synthesize the superscript call marker (the note's
    /// 1-based ordinal) and the note body renders in a notes section, mirroring
    /// [`CommentRef`](Self::CommentRef).
    FootnoteRef {
        id: String,
    },
}

/// A styled span of text. `source_index` optionally records the originating
/// top-level text-run ordinals in the source page (the index space of
/// `Document::page_text_elements` / `Document::replace_text_run`), for
/// round-tripping back to the exact content-stream operators.
///
/// A run holds **all** its source ordinals, in content order: reconstruction
/// coalesces adjacent same-style PDF runs into one editable span (see
/// `recon::paragraphs::coalesce_runs`), and every constituent stays addressable.
/// `None` means the run has no page source (synthesized spacing, imported
/// content, form-XObject text). Serialized as the legacy scalar `source_index`
/// (the first ordinal, back-compat) **plus** the full `source_indices` array.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct InlineRun {
    pub text: String,
    pub style: CharStyle,
    pub source_index: Option<Vec<usize>>,
}

impl InlineRun {
    /// The first originating run ordinal — the legacy single-index view of
    /// [`source_index`](Self::source_index).
    pub fn first_source_index(&self) -> Option<usize> {
        self.source_index.as_ref().and_then(|v| v.first().copied())
    }
}

/// An ordered or unordered list.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct List {
    pub ordered: bool,
    pub marker: ListMarker,
    pub items: Vec<ListItem>,
    /// Start number for ordered lists (HTML `<ol start="N">`, DOCX `w:lvlOverride`
    /// `w:startOverride`, Markdown implicit). `1` = default (first item).
    pub start: u32,
    /// Ordinal suffix for **ordered** markers: the punctuation Word/ODF append
    /// after the number — DOCX `w:lvlText` `"%1."` vs `"%1)"`, ODF
    /// `style:num-suffix`. `None` ⇒ the conventional `"."` (the pre-existing
    /// behaviour, so the JSON stays retro-compatible: the field is only
    /// serialized when set). Ignored for unordered lists.
    pub marker_suffix: Option<char>,
    /// Per-depth marker styles, indexed by [`ListItem::level`] (entry 0 = the
    /// outermost depth). Empty ⇒ unresolved: the table-wide
    /// [`marker`](Self::marker)/[`marker_suffix`](Self::marker_suffix) apply to
    /// every depth (the pre-existing behaviour, so the field is additive).
    /// Populated by the DOCX importer from `numbering.xml`'s multi-level
    /// definitions so a nested list keeps `1. / a) / i.` per depth.
    pub levels: Vec<ListLevelStyle>,
    /// Ordinal **prefix** for ordered markers — the opening punctuation of a
    /// `"(a)"` / `"(1)"` template (DOCX `w:lvlText` `"(%1)"`, ODF
    /// `style:num-prefix`, reconstruction's paren-both shape). `None` ⇒ no
    /// prefix (the field is only serialized when set).
    pub marker_prefix: Option<String>,
}

/// One depth's marker style in a multi-level [`List`]: the marker (its variant
/// is the number format; [`ListMarker::Bullet`] ⇒ an unordered depth), the
/// ordinal suffix (`None` ⇒ the conventional `'.'`), and the depth's first
/// ordinal (`w:start`, 1 by default).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ListLevelStyle {
    pub marker: ListMarker,
    /// Ordinal suffix for an ordered depth; `None` ⇒ `'.'`.
    pub suffix: Option<char>,
    /// First ordinal of the depth (1 by default). Meaningless for bullets.
    pub start: u32,
}

impl Default for ListLevelStyle {
    fn default() -> Self {
        Self {
            marker: ListMarker::default(),
            suffix: None,
            start: 1,
        }
    }
}

impl ListLevelStyle {
    /// Whether this depth numbers its items (any non-bullet marker).
    pub fn is_ordered(&self) -> bool {
        !matches!(self.marker, ListMarker::Bullet(_))
    }
}

impl List {
    /// The effective ordinal suffix: [`marker_suffix`](Self::marker_suffix),
    /// defaulting to `'.'`.
    pub fn suffix(&self) -> char {
        self.marker_suffix.unwrap_or('.')
    }
}

/// One list item: a list of blocks at a given nesting `level`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ListItem {
    pub blocks: Vec<Block>,
    pub level: u8,
    /// The item's **literal** marker label (`•`, `◦`, `1.`, `a)`), preserved
    /// verbatim rather than regenerated from the list-wide
    /// [`marker`](List::marker). Additive (`None` ⇒ the marker is regenerated
    /// semantically, the pre-existing behaviour, so the JSON stays
    /// retro-compatible: only serialized when set).
    ///
    /// Two independent flows populate and consume it — they never coexist in one
    /// model instance:
    ///  * **write** (geometric recon → tagged PDF): [`crate::recon`]'s list
    ///    builder captures the *painted* marker glyph run here, carrying its
    ///    [`source_index`](InlineRun::source_index) so the tagger can bind it to
    ///    a `/Lbl` structure element (the reconstruction reads that `/Lbl` back).
    ///  * **read** (tagged PDF → text): the struct-tree reconstruction stores the
    ///    `/Lbl` text here so `to_text`/exporters emit the *exact* nested marker
    ///    (`1. / a) / ◦`) instead of a flattened list-wide scheme.
    pub label: Option<InlineRun>,
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
    /// Whether this row is a table **header row** (column titles), as opposed to
    /// a body row. Maps directly to DOCX `w:trPr/w:tblHeader`, the HTML
    /// `<thead>`/`<th>` distinction, and ODF `table:table-header-rows`. `false`
    /// (the default) ⇒ an ordinary body row. Recon marks a leading row when it is
    /// visually a header (e.g. entirely bold). Per-cell column-header semantics
    /// (`<th scope>` inside the body) are not modelled here — this is a per-**row**
    /// flag covering the dominant case.
    pub is_header: bool,
}

/// A table cell: block content plus span and optional background shading.
#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    pub blocks: Vec<Block>,
    pub col_span: u16,
    pub row_span: u16,
    /// RGB background, components `0.0..=1.0`. `None` = no shading.
    pub shading: Option<[f64; 3]>,
    /// Vertical alignment of the cell's content within its box. `None` ⇒ the
    /// format default (`Top` for word-processing/ODF table cells). Round-trips
    /// DOCX `w:tcPr/w:vAlign`, ODF `style:table-cell-properties@style:vertical-align`,
    /// and the PPTX/ODP slide-table cell anchor.
    pub vertical_align: Option<CellVAlign>,
    /// Per-cell borders `[top, right, bottom, left]`, mirroring
    /// [`ParagraphStyle::borders`] (same [`ParaBorder`] leaf: width in points,
    /// line style keyword, RGB colour). All-`None` (the default) ⇒ no per-cell
    /// regime: the table-wide [`Table::border`] applies as before. When **any**
    /// side is set, this cell declares its own border regime: a `Some` side
    /// draws that border, a `None` side draws **nothing** (an explicit "no line
    /// here", not a fall-through to the table default). Round-trips DOCX
    /// `w:tcPr/w:tcBorders`, PPTX `a:tcPr/a:lnT`/`lnR`/`lnB`/`lnL`, and ODF
    /// per-cell `fo:border-*`. Additive: never serialized when all-`None`.
    pub borders: [Option<ParaBorder>; 4],
}

impl Cell {
    /// Whether this cell declares its own border regime (any side set).
    pub fn has_borders(&self) -> bool {
        self.borders.iter().any(Option::is_some)
    }
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            blocks: Vec::new(),
            col_span: 1,
            row_span: 1,
            shading: None,
            vertical_align: None,
            borders: [None, None, None, None],
        }
    }
}

/// A reference to an image blob in the [`ResourceTable`]. `resource` is the
/// content-hash key (see [`ResourceTable::images`]).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ImageRef {
    pub resource: u64,
    pub alt: Option<String>,
    /// The image's authored display size `(width, height)` in points — DOCX
    /// inline `wp:extent@cx/@cy` (EMU÷12700), ODF `svg:width`/`svg:height`.
    /// `None` ⇒ no authored size survived (exporters fall back to a default
    /// extent; a framed block image uses its [`Block::frame`] instead).
    /// Additive: never serialized when unset.
    pub size_pt: Option<(f64, f64)>,
    /// Source-rectangle crop `[left, top, right, bottom]` as inset fractions of
    /// the full bitmap, each `0.0..=1.0` (DrawingML `a:srcRect@l/t/r/b`,
    /// thousandths-of-a-percent ÷ 100000). `[0.1, 0.0, 0.2, 0.0]` cuts 10 % off
    /// the left and 20 % off the right. `None` = uncropped. Additive.
    pub crop: Option<[f64; 4]>,
}

/// One colour stop of a [`Gradient`]: its position along the gradient axis
/// (`0.0..=1.0`), RGB colour (`0.0..=1.0` components), and opacity `alpha`
/// (`1.0` = opaque). Maps DrawingML `a:gs@pos` (thousandths-of-a-percent) with
/// `a:srgbClr` + `a:alpha`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GradientStop {
    pub pos: f64,
    pub color: [f64; 3],
    pub alpha: f64,
}

/// A linear gradient fill: ordered colour stops plus the gradient axis angle in
/// **degrees clockwise from the positive X axis** (DrawingML `a:lin@ang`,
/// 60000ths of a degree — `0` = left→right, `90` = top→bottom). Carried
/// *alongside* the flat RGB fill (which stays the first stop's colour) so
/// consumers unaware of gradients keep rendering the previous solid
/// approximation.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Gradient {
    pub stops: Vec<GradientStop>,
    pub angle_deg: f64,
}

/// An outer drop shadow (DrawingML `a:effectLst/a:outerShdw`): offset
/// `dx_pt`/`dy_pt` (points, +X right / +Y **down**, matching the CSS
/// `box-shadow` convention), Gaussian `blur_pt` radius, RGB colour, and
/// opacity `alpha` (`1.0` = opaque).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shadow {
    pub dx_pt: f64,
    pub dy_pt: f64,
    pub blur_pt: f64,
    pub color: [f64; 3],
    pub alpha: f64,
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
    /// Gradient fill (PPTX/ODP `a:gradFill`). When set it **primes over**
    /// [`fill`](Self::fill), which keeps the first stop's colour as the flat
    /// fallback for gradient-unaware consumers. Additive: never serialized
    /// when unset.
    pub fill_gradient: Option<Gradient>,
    /// Outer drop shadow (`a:effectLst/a:outerShdw`). Additive.
    pub shadow: Option<Shadow>,
}

/// A free-floating text box. Holds a list of blocks (paragraphs, lists, nested
/// tables…) so it composes with the rest of the model uniformly.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TextBox {
    pub blocks: Vec<Block>,
}

/// A preformatted code block: a single string of source `code` (newlines kept
/// verbatim) and an optional `lang` hint (e.g. `"rust"`), used to drive
/// monospaced/preformatted rendering and the Markdown fence info-string.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CodeBlock {
    /// The language identifier of the fence info-string, if any.
    pub lang: Option<String>,
    /// The verbatim block contents (literal newlines preserved).
    pub code: String,
}

/// A block quotation: a nested list of blocks set off from the surrounding flow.
/// Composes recursively (a quote may contain paragraphs, lists, nested quotes…).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Blockquote {
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
    /// A named in-document anchor (a DOCX bookmark / HTML fragment name) that
    /// did not — or could not yet — resolve to a page. Keeps the anchor name
    /// **typed** instead of a `"#name"` URL placeholder.
    Anchor(String),
    /// A link action with no URL/page/anchor equivalent, kept as a raw textual
    /// summary instead of being dropped — e.g. a PDF `GoToR:report.pdf`
    /// (remote go-to), `Named:NextPage`, or `Launch:app.exe`.
    Other(String),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_kind_default_is_empty_paragraph() {
        match BlockKind::default() {
            BlockKind::Paragraph(p) => assert!(p.runs.is_empty()),
            other => panic!("expected default Paragraph, got {other:?}"),
        }
    }

    #[test]
    fn link_target_default_is_empty_url() {
        assert_eq!(LinkTarget::default(), LinkTarget::Url(String::new()));
    }
}
