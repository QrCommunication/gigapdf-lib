//! PDF **structural reconstruction** — the re-editability crux (epic #74, Phase 2).
//!
//! A PDF page is a flat bag of positioned glyph runs and painted paths; it has
//! no notion of "paragraph", "heading", "list" or "table". This module turns
//! that flat geometry back into logical [`model`](crate::model) blocks so the
//! document can be *re-edited* (reflowed, restyled, exported) rather than only
//! pixel-pushed.
//!
//! ## Pipeline (one file per stage, each unit-testable)
//!
//! ```text
//! runs ─▶ tag_tree   (if /StructTreeRoot present: trust the author's tags)
//!        │ else ↓
//!        ├▶ lines      group runs sharing a baseline band
//!        ├▶ columns    detect vertical gutters → reading order
//!        ├▶ paragraphs merge lines on leading / indent / alignment
//!        ├▶ headings   promote short large-font paragraphs → Heading
//!        ├▶ lists      bullet / ordinal + hanging indent → List
//!        └▶ tables     ruling-line grid (+ borderless fallback) → Table
//! ```
//!
//! ## Coordinate space
//!
//! Reconstruction runs in **PDF user space** (origin bottom-left, *Y up*) — the
//! native space of [`ContentElement::bounds`](crate::content::ContentElement::bounds)
//! and [`page_vector_paths`](crate::Document::page_vector_paths). Each emitted
//! [`Block::frame`](crate::model::Block::frame) is flipped to the model's
//! **top-down points** (origin top-left) at the very end — matching
//! [`Document::convert_pages`](crate::Document) and every Office exporter — so a
//! host keeps exact placement for fidelity while the block tree stays editable.

pub mod columns;
pub mod headerfooter;
pub mod headings;
pub mod lines;
pub mod lists;
pub mod paragraphs;
pub mod tables;
pub mod tag_tree;

use crate::content::vector::{PathSeg, VectorPath};
use crate::content::{ContentElement, ElementKind};
use crate::convert::style::TextStyle;
use crate::model::{geom::Rotation, Block, BlockId, BlockKind, CharStyle, ImageRef, Rect, Shape};

/// A single extracted text run in **PDF user space** (origin bottom-left). This
/// is the unit every heuristic stage consumes; it bundles the geometry the
/// element walker reports with the font style recovered from the `/BaseFont`.
#[derive(Debug, Clone)]
pub struct ReconRun {
    /// Decoded text (already font-aware / `/ToUnicode`-resolved).
    pub text: String,
    /// Lower-left X of the run's bounding box, points.
    pub x: f64,
    /// Lower-left Y of the run's bounding box, points (Y increases upward).
    pub y: f64,
    /// Bounding-box width, points.
    pub w: f64,
    /// Bounding-box height ≈ glyph cap/ascent extent, points.
    pub h: f64,
    /// Effective glyph size in points (`Tf` size × CTM vertical scale).
    pub size: f64,
    /// Recovered display family + generic class + bold/italic + colour.
    pub style: TextStyle,
    /// Baseline rotation in degrees (0 for upright text).
    pub rotation: f64,
    /// Originating page text-run index, for round-tripping to the exact operator.
    pub source_index: Option<usize>,
    /// Whether a thin horizontal ruling line sits under the run's baseline — a
    /// drawn underline (PDF has no font underline flag). Set by
    /// [`mark_underlines`] before line grouping so it flows into every stage.
    pub underline: bool,
    /// Whether a thin horizontal ruling line crosses the run at mid-glyph — a
    /// drawn strikethrough (PDF has no font strike flag either). Set by
    /// [`mark_strikes`] before line grouping so it flows into every stage.
    pub strike: bool,
}

impl ReconRun {
    /// The run's vertical centre (used to band runs into lines).
    pub fn center_y(&self) -> f64 {
        self.y + self.h / 2.0
    }

    /// The run's top edge (PDF user space, larger Y = higher on the page).
    pub fn top(&self) -> f64 {
        self.y + self.h
    }

    /// The run's right edge.
    pub fn right(&self) -> f64 {
        self.x + self.w
    }
}

/// Whether two horizontally-adjacent runs (on the same baseline band) join into
/// one word **without** a synthesized space, given the previous run's right edge,
/// the current run's left edge, and a representative line height.
///
/// A dense form interleaves several embedded fonts **per word** (a Type1 face for
/// some glyphs, a CID face for others), so a single logical word arrives as
/// several runs (`"ENFANT"` + `"S"`). Joining every adjacent run with a space
/// shreds words (`"ENFANT S MINEUR S"`); never spacing fuses real words
/// (`"DESENFANTS"`). The decision is therefore **gap-based**, mirroring the
/// already-correct line builder in [`crate::content::group_lines`]:
///
///  - a tiny gap in a band around zero (`-0.5·h ..= 0.25·h`) is normal intra-word
///    kerning or a glyph drawn from another font butting the previous one → join;
///  - a clear positive gap is a real inter-word space → don't join;
///  - a large negative gap means the run wrapped back to the left margin (a new
///    visual line inside the same baseline-row cluster) → still a boundary, don't
///    join (else a wrapped opener fuses onto the previous line's tail).
///
/// `prev_right`/`cur_x` are PDF user-space X (points); `height` is a positive
/// representative font extent (the larger of the two runs' heights).
pub(crate) fn runs_join(prev_right: f64, cur_x: f64, height: f64) -> bool {
    let h = height.max(1.0);
    let gap = cur_x - prev_right;
    gap <= h * 0.25 && gap >= -h * 0.5
}

/// Median of a slice, robust to a few outliers in a way the mean is not. Empty
/// → `fallback`. Mutates (sorts) the input for an in-place selection.
pub(crate) fn median(values: &mut [f64], fallback: f64) -> f64 {
    if values.is_empty() {
        return fallback;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    }
}

/// The body (most common) font size across runs — the calibration baseline for
/// heading promotion and leading estimates. Returns `fallback` when empty.
pub(crate) fn body_font_size(runs: &[ReconRun], fallback: f64) -> f64 {
    let mut sizes: Vec<f64> = runs
        .iter()
        .filter(|r| !r.text.trim().is_empty())
        .map(|r| r.size.max(1.0))
        .collect();
    median(&mut sizes, fallback)
}

/// Build the [`ReconRun`] list for a page from its **text** content elements and
/// the per-font styles. Non-text elements and blank runs are dropped; bounds are
/// required (a run with no computable box can't be placed).
///
/// Works for both the top-level element list and the **deep** one (text reached
/// through form XObjects): a `nested` element is included for layout/display but
/// gets `source_index = None`, because its element index doesn't address a
/// top-level editable content-stream operator (the form is shared across every
/// placement). Top-level runs keep `Some(index)` for in-place round-tripping.
pub fn runs_from_elements(
    elements: &[ContentElement],
    font_styles: &std::collections::BTreeMap<String, TextStyle>,
) -> Vec<ReconRun> {
    elements
        .iter()
        .filter(|e| e.kind == ElementKind::Text)
        .filter_map(|e| {
            let b = e.bounds?;
            if e.label.trim().is_empty() {
                return None;
            }
            let mut style = e
                .font
                .as_deref()
                .and_then(|f| font_styles.get(f))
                .cloned()
                .unwrap_or_default();
            style.color = e.color;
            Some(ReconRun {
                text: e.label.clone(),
                x: b.x,
                y: b.y,
                w: b.width,
                h: b.height,
                size: e.font_size.unwrap_or(b.height).max(1.0),
                style,
                rotation: e.rotation_deg.unwrap_or(0.0),
                // Form-XObject (nested) text is display-only — not editable by a
                // top-level run index, so it carries no source index.
                source_index: if e.nested { None } else { Some(e.index) },
                // Set later by `mark_underlines` / `mark_strikes` from the page's
                // ruling lines.
                underline: false,
                strike: false,
            })
        })
        .collect()
}

/// A monotonically increasing [`BlockId`] source. One instance per
/// `reconstruct_model` call keeps ids unique and stable within a document.
#[derive(Debug, Default)]
pub struct IdGen(u64);

impl IdGen {
    /// Mint the next sequential [`BlockId`].
    pub fn mint(&mut self) -> BlockId {
        let id = BlockId(self.0);
        self.0 += 1;
        id
    }
}

/// Convert a recovered [`TextStyle`] into the model's [`CharStyle`] at `size_pt`.
/// `underline` is carried separately because the PDF font model has no underline
/// flag — it is recovered from drawn ruling lines (see [`mark_underlines`]).
pub(crate) fn char_style(style: &TextStyle, size_pt: f64) -> CharStyle {
    CharStyle {
        family: style.family.clone(),
        generic: style.generic,
        size_pt,
        bold: style.bold,
        italic: style.italic,
        underline: false,
        strike: false,
        color: style.style_color(),
        background: None,
        vertical_align: crate::model::VAlign::Baseline,
    }
}

/// [`char_style`] for a concrete [`ReconRun`], carrying its recovered
/// `underline` and `strike` flags through. Every reconstruction stage that
/// materialises an [`InlineRun`](crate::model::InlineRun) from a run uses this so
/// a drawn underline or strikethrough reaches the editable model.
pub(crate) fn run_char_style(run: &ReconRun) -> CharStyle {
    CharStyle {
        underline: run.underline,
        strike: run.strike,
        ..char_style(&run.style, run.size)
    }
}

/// Snap a baseline angle (degrees, counter-clockwise — the convention
/// [`ReconRun::rotation`] carries from the text/CTM matrix) to a model
/// [`Rotation`]. The angle is normalised to `(-180, 180]`; values within
/// [`ROT_SNAP_EPS`] of a cardinal direction collapse to the exact first-class
/// variant (so a clean `/Rotate`-style matrix stays exact), and `270°` is
/// reported as the `-90°` the matrix actually yields. Everything else becomes
/// [`Rotation::Deg`] with the normalised angle.
///
/// Near-upright text (within the epsilon of `0°`) maps to [`Rotation::D0`], so a
/// block of ordinary horizontal runs is byte-identical to before this stage
/// existed.
pub(crate) fn rotation_from_baseline_deg(deg: f64) -> Rotation {
    if !deg.is_finite() {
        return Rotation::D0;
    }
    // Normalise to (-180, 180].
    let mut a = deg % 360.0;
    if a > 180.0 {
        a -= 360.0;
    } else if a <= -180.0 {
        a += 360.0;
    }
    let near = |target: f64| (a - target).abs() <= ROT_SNAP_EPS;
    if near(0.0) {
        Rotation::D0
    } else if near(90.0) {
        Rotation::D90
    } else if near(180.0) || near(-180.0) {
        Rotation::D180
    } else if near(-90.0) {
        // A 270° CCW baseline arrives from `atan2` as -90°; both name the same
        // direction. Report the cardinal variant the exporters understand.
        Rotation::D270
    } else {
        Rotation::Deg(a)
    }
}

/// Tolerance (degrees) for snapping a baseline angle to a cardinal direction in
/// [`rotation_from_baseline_deg`]. A clean rotation matrix yields exactly
/// `0/90/180/-90`; this small band also catches the tiny float error a scaled or
/// skewed CTM introduces, without swallowing a genuine free-form angle.
const ROT_SNAP_EPS: f64 = 0.5;

/// The dominant [`Rotation`] of a slice of runs — the orientation a block built
/// from them should carry. Blank runs (no glyphs, no meaningful orientation) are
/// ignored; the **median** baseline angle of the rest is snapped via
/// [`rotation_from_baseline_deg`].
///
/// The median makes the decision robust to a stray differently-oriented run, and
/// the snap means a block whose runs are all (near-)upright reports
/// [`Rotation::D0`] — keeping the overwhelmingly common horizontal case
/// byte-identical. An empty / all-blank slice is upright.
pub(crate) fn runs_rotation(runs: &[ReconRun]) -> Rotation {
    let mut angles: Vec<f64> = runs
        .iter()
        .filter(|r| !r.text.trim().is_empty())
        .map(|r| r.rotation)
        .collect();
    if angles.is_empty() {
        return Rotation::D0;
    }
    rotation_from_baseline_deg(median(&mut angles, 0.0))
}

/// Helper on [`TextStyle`]: the run colour as the model's `Option<[f64;3]>`
/// (black / unset stays `None`). Kept here so the model layer needn't depend on
/// the conversion style helpers.
trait StyleColor {
    fn style_color(&self) -> Option<[f64; 3]>;
}
impl StyleColor for TextStyle {
    fn style_color(&self) -> Option<[f64; 3]> {
        self.color
    }
}

/// Flip a PDF-user-space box (origin bottom-left, `y` is the *lower* edge) to a
/// model top-down [`Rect`] (origin top-left), given the page top-left corner
/// (`x0`, the MediaBox left) and the page height. Mirrors `convert_pages`.
pub(crate) fn frame_top_down(
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    x0: f64,
    y0: f64,
    page_h: f64,
) -> Rect {
    Rect::new(x - x0, page_h - (y - y0) - h, w, h)
}

/// Whether a painted path is an **axis-aligned straight ruling line** — a thin
/// rectangle or a single horizontal/vertical `Line`. These are the table grid
/// candidates; curves, diagonals and big filled boxes are not.
pub(crate) fn ruling_orientation(vp: &VectorPath) -> Option<Ruling> {
    let b = vp.bounds?;
    // No curves allowed in a ruling line.
    if vp.segments.iter().any(|s| matches!(s, PathSeg::Cubic(..))) {
        return None;
    }
    let thin = 2.5_f64.max(vp.stroke_width * 1.5);
    if b.width <= thin && b.height >= thin * 2.0 {
        Some(Ruling::Vertical {
            x: b.x + b.width / 2.0,
            y0: b.y,
            y1: b.y + b.height,
        })
    } else if b.height <= thin && b.width >= thin * 2.0 {
        Some(Ruling::Horizontal {
            y: b.y + b.height / 2.0,
            x0: b.x,
            x1: b.x + b.width,
        })
    } else {
        None
    }
}

/// An axis-aligned ruling line (PDF user space), classified for table grids.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Ruling {
    Horizontal { y: f64, x0: f64, x1: f64 },
    Vertical { x: f64, y0: f64, y1: f64 },
}

/// Set [`ReconRun::underline`] for runs that have a thin **horizontal** ruling
/// line drawn just under their baseline, and return the set of path indices that
/// were consumed as underlines (so the caller does not also emit them as
/// [`Shape`] blocks — which would draw the underline twice).
///
/// PDF carries no font underline flag; an underline is a separately-painted thin
/// rectangle/line below the glyphs. A run's box bottom (`y`) sits **below** its
/// baseline by the font descent, while an underline sits just **under** the
/// baseline — so the rule can be a hair above the box bottom. A path counts as a
/// run's underline when it is a horizontal ruling whose `y` lands in the band
/// `[box_bottom − 0.30·h, box_bottom + 0.22·h]` (under the baseline, not as high
/// as a strikethrough at ~0.4–0.5·h) and overlaps at least 55 % of the run's
/// width. A single drawn rule may underline several adjacent runs (a whole
/// phrase), so it is not removed after the first match — but it is recorded once
/// as consumed.
pub(crate) fn mark_underlines(runs: &mut [ReconRun], vpaths: &[VectorPath]) -> Vec<usize> {
    // Candidate horizontal rules: (y, x0, x1, path index).
    let rules: Vec<(f64, f64, f64, usize)> = vpaths
        .iter()
        .filter_map(|vp| match ruling_orientation(vp) {
            Some(Ruling::Horizontal { y, x0, x1 }) => Some((y, x0, x1, vp.index)),
            _ => None,
        })
        .collect();
    if rules.is_empty() {
        return Vec::new();
    }

    let mut consumed = std::collections::BTreeSet::new();
    for run in runs.iter_mut() {
        if run.text.trim().is_empty() || run.w <= 0.0 {
            continue;
        }
        // Band around the run box bottom (`y`): a little below, up to ~0.22·h
        // above (the baseline sits above the box bottom by the descent; an
        // underline rides just under the baseline). Stays clear of a
        // strikethrough, which sits near mid-glyph (~0.4–0.5·h above the bottom).
        let bottom = run.y;
        let lo = bottom - run.h * 0.30;
        let hi = bottom + run.h * 0.22;
        let run_x0 = run.x;
        let run_x1 = run.x + run.w;
        for &(ry, rx0, rx1, idx) in &rules {
            if ry < lo || ry > hi {
                continue;
            }
            let overlap = (run_x1.min(rx1) - run_x0.max(rx0)).max(0.0);
            if overlap >= run.w * 0.55 {
                run.underline = true;
                consumed.insert(idx);
                break;
            }
        }
    }
    consumed.into_iter().collect()
}

/// Set [`ReconRun::strike`] for runs crossed by a thin **horizontal** ruling line
/// at **mid-glyph**, and return the path indices consumed as strikethroughs (so
/// the caller does not also emit them as [`Shape`] blocks, nor read them as table
/// grid edges — the same hygiene [`mark_underlines`] applies).
///
/// This is the underline detector applied one band higher. An underline rides
/// just under the baseline (`[box_bottom − 0.30·h, box_bottom + 0.22·h]`); a
/// strikethrough sits across the centre of the glyph body, well above the
/// baseline — so the rule's `y` must land in `[box_bottom + 0.35·h, box_bottom +
/// 0.58·h]`. The `0.35·h` floor leaves a clear gap above the underline band's
/// `0.22·h` ceiling, so a single rule is never read as both. As with underlines,
/// a path must overlap ≥ 55 % of the run's width, and one drawn rule may strike
/// several adjacent runs (a whole phrase) yet is recorded once as consumed.
pub(crate) fn mark_strikes(runs: &mut [ReconRun], vpaths: &[VectorPath]) -> Vec<usize> {
    // Candidate horizontal rules: (y, x0, x1, path index).
    let rules: Vec<(f64, f64, f64, usize)> = vpaths
        .iter()
        .filter_map(|vp| match ruling_orientation(vp) {
            Some(Ruling::Horizontal { y, x0, x1 }) => Some((y, x0, x1, vp.index)),
            _ => None,
        })
        .collect();
    if rules.is_empty() {
        return Vec::new();
    }

    let mut consumed = std::collections::BTreeSet::new();
    for run in runs.iter_mut() {
        if run.text.trim().is_empty() || run.w <= 0.0 {
            continue;
        }
        // Mid-glyph band, measured up from the run box bottom (`y`). Clear of the
        // underline band (which tops out at +0.22·h) so no rule is double-claimed.
        let bottom = run.y;
        let lo = bottom + run.h * 0.35;
        let hi = bottom + run.h * 0.58;
        let run_x0 = run.x;
        let run_x1 = run.x + run.w;
        for &(ry, rx0, rx1, idx) in &rules {
            if ry < lo || ry > hi {
                continue;
            }
            let overlap = (run_x1.min(rx1) - run_x0.max(rx0)).max(0.0);
            if overlap >= run.w * 0.55 {
                run.strike = true;
                consumed.insert(idx);
                break;
            }
        }
    }
    consumed.into_iter().collect()
}

/// Assemble all logical blocks for one page from its text runs, painted paths
/// and images. The reading order is column-major (left band first, top→bottom
/// within a band); each block keeps `frame = Some(rect)` for fidelity. Non-rule
/// shapes pass through as [`Shape`] blocks and images as [`Image`] blocks.
///
/// `geom` is `(x0, y0, page_w, page_h)`: the MediaBox origin and the page size
/// in points. `links` are the page's hyperlinks (PDF user space), used to wrap
/// covered prose runs in [`Inline::Link`](crate::model::Inline::Link).
/// `tag_blocks`, when `Some`, is the already-built block list from a
/// `/StructTreeRoot` walk and is used verbatim (the author tagged the document).
#[allow(clippy::too_many_arguments)]
pub fn reconstruct_page(
    mut text_runs: Vec<ReconRun>,
    vpaths: &[VectorPath],
    image_refs: &[PlacedImageRef],
    geom: (f64, f64, f64, f64),
    ids: &mut IdGen,
    links: &[ParaLink],
    tag_blocks: Option<Vec<Block>>,
) -> Vec<Block> {
    let (x0, y0, _page_w, page_h) = geom;
    if let Some(blocks) = tag_blocks {
        return blocks;
    }

    // 0. Recover drawn text decorations: flag runs sitting above a thin horizontal
    //    rule (underline) or crossed by one at mid-glyph (strikethrough), and
    //    collect those rules' path indices. They must neither be re-emitted as
    //    shapes (double-drawing the decoration) nor read as table grid edges
    //    (phantom rows/columns), so both sets feed the one `consumed_rule_paths`.
    let mut consumed_rule_paths: std::collections::BTreeSet<usize> =
        mark_underlines(&mut text_runs, vpaths)
            .into_iter()
            .collect();
    consumed_rule_paths.extend(mark_strikes(&mut text_runs, vpaths));

    // 1. Lines, then 2. reading-order columns over those lines. The same column
    //    layout also ranks non-line placeables (shapes, images) so they slot into
    //    the reading order by region/column/Y instead of trailing all the text.
    let lines = lines::group_into_lines(&text_runs);
    let body = body_font_size(&text_runs, 12.0);
    let layout = columns::column_layout(&lines);
    let order = layout.order_lines(&lines);

    let mut blocks: Vec<Block> = Vec::new();

    // Ruling-line tables first, so the lines they cover are not also emitted as
    // prose. A table consumes the line indices that fall inside its grid.
    let table_plan = tables::plan_tables(&lines, vpaths, &consumed_rule_paths);

    for &line_idx in &order {
        if let Some(tbl) = table_plan.take_if_starts_at(line_idx) {
            // Build the Table block from its covered lines.
            if let Some(block) = tables::build_table(&tbl, &lines, ids, |x, y, w, h| {
                frame_top_down(x, y, w, h, x0, y0, page_h)
            }) {
                blocks.push(block);
            }
            continue;
        }
        if table_plan.is_consumed(line_idx) {
            continue;
        }
        // Not in a table → paragraph / heading / list, decided over the
        // contiguous run of unconsumed lines this one begins. To keep the
        // walk simple we collect groups lazily in `flush_text` below.
        blocks.push(make_pending(line_idx));
    }

    // Resolve the pending text-line placeholders into paragraphs/headings/lists.
    // The page's MediaBox left (`x0`) anchors recovered left-indents; `links`
    // flow down so covered prose runs become `Inline::Link`.
    let blocks = resolve_text_blocks(blocks, &lines, body, x0, links, ids, |x, y, w, h| {
        frame_top_down(x, y, w, h, x0, y0, page_h)
    });

    let mut out = blocks;

    // Non-ruling shapes pass through (filled boxes, diagonals, curves…), and any
    // ruling line not consumed by a table also survives as a shape so nothing is
    // silently dropped.
    for vp in vpaths {
        if table_plan.uses_path(vp.index) || consumed_rule_paths.contains(&vp.index) {
            continue;
        }
        let Some(b) = vp.bounds else { continue };
        let segments: Vec<PathSeg> = vp
            .segments
            .iter()
            .map(|seg| match *seg {
                PathSeg::Move(x, y) => PathSeg::Move(x - x0, page_h - (y - y0)),
                PathSeg::Line(x, y) => PathSeg::Line(x - x0, page_h - (y - y0)),
                PathSeg::Cubic(a, bb, c, d, e, f) => PathSeg::Cubic(
                    a - x0,
                    page_h - (bb - y0),
                    c - x0,
                    page_h - (d - y0),
                    e - x0,
                    page_h - (f - y0),
                ),
                PathSeg::Close => PathSeg::Close,
            })
            .collect();
        out.push(Block {
            id: ids.mint(),
            frame: Some(frame_top_down(b.x, b.y, b.width, b.height, x0, y0, page_h)),
            rotation: Rotation::D0,
            kind: BlockKind::Shape(Shape {
                segments,
                fill: vp.fill,
                stroke: vp.stroke,
                stroke_width: vp.stroke_width,
                dash: vp.dash.clone(),
            }),
        });
    }

    // Images pass through as Image blocks (resource key handed by the caller).
    // Before each image is emitted, a short caption paragraph sitting directly
    // above or below it — same width, opening with a "Figure"/"Table"/… cue — is
    // lifted into the image's `alt` and removed from the prose, so a figure and
    // its legend reunite as one editable unit (the model has no separate caption
    // field). Conservative: no association without a clear cue.
    for img in image_refs {
        let frame = frame_top_down(img.x, img.y, img.w, img.h, x0, y0, page_h);
        let alt = take_caption_for(&mut out, &frame);
        out.push(Block {
            id: ids.mint(),
            frame: Some(frame),
            rotation: Rotation::D0,
            kind: BlockKind::Image(ImageRef {
                resource: img.resource,
                alt,
            }),
        });
    }

    // Interleave shapes/images into the reading order. Text and table blocks are
    // already in reading order, so their ranks are monotonic; a *stable* sort by
    // the column-aware rank keeps them in place while slotting each shape/image at
    // its region/column/Y. Single-column pages keep their text order (band is
    // always 0) and simply gain top→bottom placement of figures.
    interleave_by_reading_order(&mut out, &layout, x0, y0, page_h);

    out
}

/// Stable-sort `blocks` (model **top-down** frames) into the page's reading order
/// using `layout`. Each block's PDF-user-space `(centre_x, top)` is recovered
/// from its top-down frame to query [`ColumnLayout::rank`]; a frameless block
/// (none in practice post-resolution) sorts last so nothing is dropped.
fn interleave_by_reading_order(
    blocks: &mut [Block],
    layout: &columns::ColumnLayout,
    x0: f64,
    y0: f64,
    page_h: f64,
) {
    // Top-down frame → PDF-user-space rank key. `frame_top_down` maps a PDF box
    // (lower-left `y`, height `h`) to top-down `y' = page_h - (y - y0) - h`; invert
    // for the PDF top edge `y + h = page_h - y' + y0`, and `centre_x = x' + x0 +
    // w/2`.
    let key = |b: &Block| match b.frame {
        Some(f) => {
            let center_x = f.x + x0 + f.w / 2.0;
            let top = page_h - f.y + y0;
            (0u8, layout.rank(center_x, top))
        }
        // Frameless: trail (group `1` sorts after every framed block).
        None => (1u8, layout.rank(f64::INFINITY, f64::NEG_INFINITY)),
    };
    blocks.sort_by_key(key);
}

/// Find a caption paragraph for an image at `image_frame` (model **top-down**
/// points), remove it from `blocks`, and return its text. A candidate must be a
/// plain [`Paragraph`], lie immediately above or below the image (vertical gap
/// ≤ `CAPTION_MAX_GAP_PT`), span roughly the image's width
/// (`CAPTION_WIDTH_TOL`), and open with a caption cue
/// (`Figure`/`Fig.`/`Table`/`Tableau`/`Image`/`Illustration`). Returns `None`
/// when nothing qualifies — no figure ever loses an unrelated paragraph.
fn take_caption_for(blocks: &mut Vec<Block>, image_frame: &Rect) -> Option<String> {
    let img_top = image_frame.y;
    let img_bottom = image_frame.y + image_frame.h;
    let img_cx = image_frame.x + image_frame.w / 2.0;

    let mut best: Option<(usize, f64)> = None; // (index, vertical gap)
    for (i, b) in blocks.iter().enumerate() {
        let BlockKind::Paragraph(para) = &b.kind else {
            continue;
        };
        let Some(frame) = b.frame else { continue };
        let text = paragraph_text(para);
        if !is_caption_text(&text) {
            continue;
        }
        // Width must be comparable to the image (a caption hugs its figure, not a
        // full-measure body paragraph that merely happens to start with "Table").
        if image_frame.w <= 0.0 {
            continue;
        }
        let width_ratio = frame.w / image_frame.w;
        if !(CAPTION_WIDTH_TOL.0..=CAPTION_WIDTH_TOL.1).contains(&width_ratio) {
            continue;
        }
        // Horizontal overlap with the image (centre within the image span) guards
        // against a same-width caption belonging to a neighbouring column.
        if img_cx < frame.x - 1.0 || img_cx > frame.x + frame.w + 1.0 {
            continue;
        }
        let para_top = frame.y;
        let para_bottom = frame.y + frame.h;
        // Gap to the nearest edge: caption directly below (para_top under image
        // bottom) or directly above (para_bottom over image top).
        let gap = if para_top >= img_bottom - 1.0 {
            para_top - img_bottom
        } else if para_bottom <= img_top + 1.0 {
            img_top - para_bottom
        } else {
            continue; // overlaps the image vertically → not a caption band
        };
        if gap > CAPTION_MAX_GAP_PT {
            continue;
        }
        if best.is_none_or(|(_, g)| gap < g) {
            best = Some((i, gap));
        }
    }

    let (idx, _) = best?;
    let block = blocks.remove(idx);
    let BlockKind::Paragraph(para) = block.kind else {
        return None;
    };
    Some(paragraph_text(&para))
}

/// Maximum vertical gap (points) between an image edge and its caption.
const CAPTION_MAX_GAP_PT: f64 = 24.0;

/// Allowed caption-to-image width ratio band: a caption may be a touch narrower
/// or wider than the image it labels, but not a full-measure body paragraph.
const CAPTION_WIDTH_TOL: (f64, f64) = (0.5, 1.5);

/// Whether `text` opens with a figure/table caption cue (case-insensitive,
/// leading whitespace ignored). Matches `Figure`, `Fig.`, `Table`, `Tableau`,
/// `Image`, `Illustration` — the conservative set of unambiguous legend leads.
fn is_caption_text(text: &str) -> bool {
    let t = text.trim_start();
    if t.is_empty() {
        return false;
    }
    const CUES: [&str; 6] = ["figure", "fig.", "table", "tableau", "image", "illustration"];
    let lower = t.to_ascii_lowercase();
    CUES.iter().any(|cue| {
        lower.strip_prefix(cue).is_some_and(|rest| {
            // A real cue is followed by a separator or number, not more letters
            // ("imagery"/"tabletop" must not match).
            rest.chars()
                .next()
                .is_none_or(|c| !c.is_ascii_alphabetic())
        })
    })
}

/// The display title of a [`Heading`](crate::model::Heading): its paragraph's
/// flattened text. Used by [`reconstruct_model`](crate::Document::reconstruct_model)'s
/// heading-based outline fallback.
pub fn heading_title(heading: &crate::model::Heading) -> String {
    paragraph_text(&heading.para)
}

/// Flatten a paragraph's inline runs (text + link text) into a single string,
/// joining successive runs and rendering line breaks as spaces.
fn paragraph_text(para: &crate::model::Paragraph) -> String {
    use crate::model::Inline;
    let mut s = String::new();
    for inline in &para.runs {
        match inline {
            Inline::Run(run) => s.push_str(&run.text),
            Inline::LineBreak => s.push(' '),
            Inline::Link { children, .. } => {
                for c in children {
                    if let Inline::Run(run) = c {
                        s.push_str(&run.text);
                    }
                }
            }
            Inline::Image(_) => {}
        }
    }
    s.trim().to_string()
}

/// One flat outline entry to fold into a tree: a label, its nesting `level`
/// (`0` = top), and a zero-based destination page. The source's reading-order
/// sequence + `level` fully determines the parent/child shape.
#[derive(Debug, Clone)]
pub struct FlatOutline {
    pub title: String,
    pub level: usize,
    pub page: usize,
}

/// Fold a pre-order, `level`-tagged flat outline into the model's nested
/// [`OutlineNode`](crate::model::OutlineNode) tree.
///
/// This is the shared assembler for both outline sources in
/// [`reconstruct_model`](crate::Document::reconstruct_model): the PDF's own
/// `/Outlines` (preferred) and, as a fallback, detected document headings. A
/// node attaches under the nearest preceding node of a strictly lower level; a
/// level that jumps ahead (e.g. a stray `h3` with no `h2`) attaches at the
/// deepest currently-open level rather than being dropped, so no entry is lost.
pub fn fold_outline(flat: &[FlatOutline]) -> Vec<crate::model::OutlineNode> {
    use crate::model::OutlineNode;

    let mut roots: Vec<OutlineNode> = Vec::new();
    // Path of indices from each root down to the current insertion point, one
    // entry per open level. `stack[k]` locates the level-`k` ancestor.
    let mut stack: Vec<usize> = Vec::new();

    for item in flat {
        let node = OutlineNode {
            title: item.title.clone(),
            page: item.page,
            children: Vec::new(),
        };
        // Trim the open path so the new node hangs under a strictly shallower
        // ancestor (clamped: a deeper jump than the path can't open extra levels).
        let depth = item.level.min(stack.len());
        stack.truncate(depth);

        // Walk to the children list the node belongs in, following the path.
        let siblings = walk_to_children(&mut roots, &stack);
        siblings.push(node);
        stack.push(siblings.len() - 1);
    }
    roots
}

/// Follow `path` (a chain of child indices from the roots) to the children list
/// a new node should join. `path` is always well-formed by construction in
/// [`fold_outline`] (each index was the freshly-pushed last child of its level),
/// so every `idx` indexes a node that exists.
fn walk_to_children<'a>(
    roots: &'a mut Vec<crate::model::OutlineNode>,
    path: &[usize],
) -> &'a mut Vec<crate::model::OutlineNode> {
    let mut level = roots;
    for &idx in path {
        // `idx` is guaranteed in-range (it indexes a node pushed earlier at this
        // level); descend into its children. The unconditional reassignment keeps
        // the borrow checker happy where a conditional `break` would not.
        level = &mut level[idx].children;
    }
    level
}

/// A placed image, in PDF user space, keyed by its resource hash in the
/// document's [`ResourceTable`](crate::model::ResourceTable).
#[derive(Debug, Clone, Copy)]
pub struct PlacedImageRef {
    pub resource: u64,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// A page hyperlink mapped into the editable model: the destination plus the
/// clickable rectangle `[x0, y0, x1, y1]` in **PDF user space** (origin
/// bottom-left, same space as [`ReconRun`]). Built by the caller from
/// [`Document::page_links`](crate::Document::page_links) and matched against run
/// boxes during reconstruction so each covered run becomes an
/// [`Inline::Link`](crate::model::Inline::Link).
#[derive(Debug, Clone)]
pub struct ParaLink {
    pub target: crate::model::LinkTarget,
    pub rect: [f64; 4],
}

// ── pending text-line placeholder plumbing ──────────────────────────────────
//
// `reconstruct_page` walks the reading order once, deciding table vs text per
// line. For text lines it pushes a placeholder Block carrying just the line
// index; a second pass groups *contiguous* placeholders and lowers each group
// through paragraphs→headings→lists. Encoding the index in the BlockId of an
// otherwise-empty Paragraph keeps the first pass allocation-free and order-true.

fn make_pending(line_idx: usize) -> Block {
    Block {
        id: BlockId(PENDING_TAG | line_idx as u64),
        ..Block::default()
    }
}

/// High bit marks a placeholder; the low bits carry the source line index.
const PENDING_TAG: u64 = 1 << 63;

fn pending_line(block: &Block) -> Option<usize> {
    let raw = block.id.0;
    (raw & PENDING_TAG != 0).then_some((raw & !PENDING_TAG) as usize)
}

/// Replace runs of pending placeholders with real paragraph/heading/list blocks,
/// preserving the interleaving with already-built (table/shape) blocks.
/// `page_left` is the MediaBox left (PDF user space) and `links` the page's
/// hyperlinks; both flow into each prose paragraph's [`paragraphs::ParaContext`].
#[allow(clippy::too_many_arguments)]
fn resolve_text_blocks(
    blocks: Vec<Block>,
    lines: &[lines::ReconLine],
    body: f64,
    page_left: f64,
    links: &[ParaLink],
    ids: &mut IdGen,
    to_frame: impl Fn(f64, f64, f64, f64) -> Rect + Copy,
) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::with_capacity(blocks.len());
    let mut pending: Vec<usize> = Vec::new();

    let flush = |pending: &mut Vec<usize>, out: &mut Vec<Block>, ids: &mut IdGen| {
        if pending.is_empty() {
            return;
        }
        let group: Vec<&lines::ReconLine> = pending.iter().map(|&i| &lines[i]).collect();
        let text_blocks = lower_text_group(&group, body, page_left, links, ids, to_frame);
        out.extend(text_blocks);
        pending.clear();
    };

    for block in blocks {
        if let Some(line_idx) = pending_line(&block) {
            pending.push(line_idx);
        } else {
            flush(&mut pending, &mut out, ids);
            out.push(block);
        }
    }
    flush(&mut pending, &mut out, ids);
    out
}

/// Lower a contiguous group of text lines (already in reading order) to blocks:
/// paragraphs (with recovered spacing/indents + link-wrapped runs), with
/// headings and lists promoted out of them.
#[allow(clippy::too_many_arguments)]
fn lower_text_group(
    group: &[&lines::ReconLine],
    body: f64,
    page_left: f64,
    links: &[ParaLink],
    ids: &mut IdGen,
    to_frame: impl Fn(f64, f64, f64, f64) -> Rect + Copy,
) -> Vec<Block> {
    // First carve out lists (consecutive bullet/ordinal lines), then turn the
    // remaining line spans into paragraphs and promote headings.
    let segments = lists::split_lists(group);
    let mut out = Vec::new();
    for seg in segments {
        match seg {
            lists::Segment::List(lines) => {
                if let Some(block) = lists::build_list(&lines, body, ids, to_frame) {
                    out.push(block);
                }
            }
            lists::Segment::Prose(lines) => {
                // Group-wide geometry calibrates each paragraph's recovered
                // spacing: one leading and one body-left for the whole prose run.
                let leading = paragraphs::group_leading(&lines, body);
                let group_left = paragraphs::group_left(&lines);
                let mut prev_bottom: Option<f64> = None;
                for para_lines in paragraphs::split_paragraphs(&lines, body) {
                    let ctx = paragraphs::ParaContext {
                        body,
                        leading,
                        group_left,
                        page_left,
                        prev_bottom,
                        links,
                    };
                    let block =
                        paragraphs::build_paragraph_styled(&para_lines, &ctx, ids, to_frame);
                    // Remember this paragraph's bottom for the next one's gap.
                    prev_bottom = Some(paragraphs::paragraph_bottom(&para_lines));
                    out.push(headings::promote(block, body));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str, x: f64, y: f64, size: f64) -> ReconRun {
        ReconRun {
            text: text.to_string(),
            x,
            y,
            w: text.len() as f64 * size * 0.5,
            h: size,
            size,
            style: TextStyle::default(),
            rotation: 0.0,
            source_index: None,
            underline: false,
            strike: false,
        }
    }

    #[test]
    fn median_is_robust_to_outliers() {
        let mut v = vec![10.0, 10.0, 10.0, 200.0];
        assert_eq!(median(&mut v, 0.0), 10.0);
        let mut empty: Vec<f64> = Vec::new();
        assert_eq!(median(&mut empty, 7.0), 7.0);
    }

    /// `run` carrying a baseline rotation (degrees CCW), for the #28 helpers.
    fn run_rot(text: &str, rotation: f64) -> ReconRun {
        ReconRun {
            rotation,
            ..run(text, 0.0, 0.0, 12.0)
        }
    }

    #[test]
    fn baseline_deg_snaps_cardinals_and_keeps_free_form() {
        assert_eq!(rotation_from_baseline_deg(0.0), Rotation::D0);
        assert_eq!(rotation_from_baseline_deg(90.0), Rotation::D90);
        assert_eq!(rotation_from_baseline_deg(180.0), Rotation::D180);
        // 270° CCW == -90° from `atan2`; both name `D270`.
        assert_eq!(rotation_from_baseline_deg(-90.0), Rotation::D270);
        assert_eq!(rotation_from_baseline_deg(270.0), Rotation::D270);
        // Near-cardinal (scaled/skewed CTM float error) still snaps exact.
        assert_eq!(rotation_from_baseline_deg(90.3), Rotation::D90);
        assert_eq!(rotation_from_baseline_deg(-179.8), Rotation::D180);
        // Free-form angle survives, normalised into (-180, 180].
        assert_eq!(rotation_from_baseline_deg(30.0), Rotation::Deg(30.0));
        assert_eq!(rotation_from_baseline_deg(450.0), Rotation::D90);
        match rotation_from_baseline_deg(200.0) {
            Rotation::Deg(d) => assert!((d - -160.0).abs() < 1e-9, "got {d}"),
            other => panic!("expected Deg(-160), got {other:?}"),
        }
        // Non-finite is treated as upright (defensive).
        assert_eq!(rotation_from_baseline_deg(f64::NAN), Rotation::D0);
    }

    #[test]
    fn runs_rotation_is_d0_for_upright_or_empty() {
        assert_eq!(runs_rotation(&[]), Rotation::D0);
        let runs = vec![run_rot("a", 0.0), run_rot("b", 0.0)];
        assert_eq!(runs_rotation(&runs), Rotation::D0);
    }

    #[test]
    fn runs_rotation_takes_the_dominant_angle_ignoring_blanks() {
        // Blank runs carry no orientation and must not sway the result: three
        // 90° glyph runs + a blank → `D90`.
        let runs = vec![
            run_rot("X", 90.0),
            run_rot("   ", 0.0),
            run_rot("Y", 90.0),
            run_rot("Z", 90.0),
        ];
        assert_eq!(runs_rotation(&runs), Rotation::D90);
    }

    #[test]
    fn runs_rotation_median_resists_a_stray_run() {
        // Four rotated runs + one upright stray → median stays 90° → `D90`.
        let runs = vec![
            run_rot("a", 90.0),
            run_rot("b", 90.0),
            run_rot("c", 90.0),
            run_rot("d", 90.0),
            run_rot("stray", 0.0),
        ];
        assert_eq!(runs_rotation(&runs), Rotation::D90);
    }

    #[test]
    fn body_size_picks_the_common_size() {
        let runs = vec![
            run("title", 72.0, 700.0, 24.0),
            run("body one", 72.0, 680.0, 12.0),
            run("body two", 72.0, 660.0, 12.0),
            run("body three", 72.0, 640.0, 12.0),
        ];
        assert_eq!(body_font_size(&runs, 10.0), 12.0);
    }

    #[test]
    fn empty_page_reconstructs_to_no_blocks() {
        let mut ids = IdGen::default();
        let blocks = reconstruct_page(
            Vec::new(),
            &[],
            &[],
            (0.0, 0.0, 600.0, 800.0),
            &mut ids,
            &[],
            None,
        );
        assert!(blocks.is_empty());
    }

    #[test]
    fn tag_blocks_are_used_verbatim() {
        let mut ids = IdGen::default();
        let tagged = vec![Block::default()];
        let blocks = reconstruct_page(
            vec![run("ignored", 0.0, 0.0, 12.0)],
            &[],
            &[],
            (0.0, 0.0, 600.0, 800.0),
            &mut ids,
            &[],
            Some(tagged.clone()),
        );
        assert_eq!(blocks.len(), 1);
    }

    #[test]
    fn frame_flip_is_top_down() {
        // A run with lower-left at y=700 on an 800-pt page, height 12, sits 88pt
        // from the top (800 - 700 - 12).
        let r = frame_top_down(72.0, 700.0, 100.0, 12.0, 0.0, 0.0, 800.0);
        assert!((r.x - 72.0).abs() < 1e-9);
        assert!((r.y - 88.0).abs() < 1e-9);
        assert!((r.w - 100.0).abs() < 1e-9 && (r.h - 12.0).abs() < 1e-9);
    }

    // ── end-to-end: build a real PDF via the lib's own API, then reconstruct ──

    use crate::convert::build::{PdfBuilder, StdFont};
    use crate::model::BlockKind as BK;

    /// Open a builder's bytes as a `Document`.
    fn open(pdf: Vec<u8>) -> crate::Document {
        crate::Document::open(&pdf).expect("valid PDF")
    }

    /// Every top-level block of the first page of a reconstructed document.
    fn page0_blocks(doc: &crate::model::Document) -> &[Block] {
        doc.sections
            .first()
            .and_then(|s| s.pages.first())
            .map(|p| p.blocks.as_slice())
            .unwrap_or(&[])
    }

    fn count_kind(blocks: &[Block], pred: impl Fn(&BK) -> bool) -> usize {
        blocks.iter().filter(|b| pred(&b.kind)).count()
    }

    #[test]
    fn two_body_paragraphs_reconstruct_to_two_paragraph_blocks() {
        let mut b = PdfBuilder::new();
        let page = b.add_page(612.0, 792.0);
        let body = StdFont::Helvetica;
        // Paragraph 1: two tight lines near the top.
        b.text(
            page,
            72.0,
            100.0,
            12.0,
            "The first paragraph opens here",
            body,
            [0.0; 3],
        );
        b.text(
            page,
            72.0,
            116.0,
            12.0,
            "and continues on a second line.",
            body,
            [0.0; 3],
        );
        // A blank-line gap, then paragraph 2.
        b.text(
            page,
            72.0,
            160.0,
            12.0,
            "The second paragraph starts fresh",
            body,
            [0.0; 3],
        );
        b.text(
            page,
            72.0,
            176.0,
            12.0,
            "with its own pair of lines too.",
            body,
            [0.0; 3],
        );

        let doc = open(b.finish()).reconstruct_model();
        let blocks = page0_blocks(&doc);
        let paras = count_kind(blocks, |k| matches!(k, BK::Paragraph(_)));
        assert_eq!(
            paras, 2,
            "two body paragraphs → two Paragraph blocks, got {blocks:?}"
        );
        // No spurious headings/lists/tables in plain prose.
        assert_eq!(count_kind(blocks, |k| matches!(k, BK::Heading(_))), 0);
        assert_eq!(count_kind(blocks, |k| matches!(k, BK::List(_))), 0);
        assert_eq!(count_kind(blocks, |k| matches!(k, BK::Table(_))), 0);
    }

    #[test]
    fn a_bulleted_list_reconstructs_to_a_list_with_stripped_markers() {
        let mut b = PdfBuilder::new();
        let page = b.add_page(612.0, 792.0);
        let f = StdFont::Helvetica;
        b.text(page, 72.0, 100.0, 12.0, "- Apples", f, [0.0; 3]);
        b.text(page, 72.0, 116.0, 12.0, "- Bananas", f, [0.0; 3]);
        b.text(page, 72.0, 132.0, 12.0, "- Cherries", f, [0.0; 3]);

        let doc = open(b.finish()).reconstruct_model();
        let blocks = page0_blocks(&doc);
        let lists: Vec<&crate::model::List> = blocks
            .iter()
            .filter_map(|b| match &b.kind {
                BK::List(l) => Some(l),
                _ => None,
            })
            .collect();
        assert_eq!(
            lists.len(),
            1,
            "the three bullets form one List, got {blocks:?}"
        );
        let list = lists[0];
        assert_eq!(list.items.len(), 3, "three list items");
        // First item text has its bullet stripped.
        let first = match &list.items[0].blocks[0].kind {
            BK::Paragraph(p) => match p.runs.first() {
                Some(crate::model::Inline::Run(r)) => r.text.clone(),
                _ => String::new(),
            },
            _ => String::new(),
        };
        assert_eq!(first, "Apples", "marker stripped from item text");
    }

    #[test]
    fn a_bordered_table_reconstructs_to_a_table_with_rows_and_cells() {
        let mut b = PdfBuilder::new();
        let page = b.add_page(612.0, 792.0);
        let f = StdFont::Helvetica;
        // A 2×2 grid drawn as thin ruling lines (top-down builder coords): rows
        // at y=100,124,148; columns at x=72,200,328. Each is a thin filled bar.
        let black = Some([0.0, 0.0, 0.0]);
        for &y in &[100.0, 124.0, 148.0] {
            b.rect(page, 72.0, y, 256.0, 0.6, None, black); // horizontal rule
        }
        for &x in &[72.0, 200.0, 328.0] {
            b.rect(page, x, 100.0, 0.6, 48.0, None, black); // vertical rule
        }
        // Cell text inside each of the four cells.
        b.text(page, 80.0, 106.0, 11.0, "Name", f, [0.0; 3]);
        b.text(page, 208.0, 106.0, 11.0, "Age", f, [0.0; 3]);
        b.text(page, 80.0, 130.0, 11.0, "Alice", f, [0.0; 3]);
        b.text(page, 208.0, 130.0, 11.0, "30", f, [0.0; 3]);

        let doc = open(b.finish()).reconstruct_model();
        let blocks = page0_blocks(&doc);
        let tables: Vec<&crate::model::Table> = blocks
            .iter()
            .filter_map(|b| match &b.kind {
                BK::Table(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(
            tables.len(),
            1,
            "the ruled grid → one Table, got {blocks:?}"
        );
        let t = tables[0];
        assert_eq!(t.rows.len(), 2, "two rows");
        assert!(
            t.rows.iter().all(|r| r.cells.len() == 2),
            "two columns per row"
        );
        assert!(t.border.width > 0.0, "ruled table has a widened border");
    }

    #[test]
    fn a_large_font_line_reconstructs_to_a_heading() {
        let mut b = PdfBuilder::new();
        let page = b.add_page(612.0, 792.0);
        let body = StdFont::Helvetica;
        // A large title (24pt) above several 12pt body lines: the body median is
        // 12, so the 24pt line (2.0×) promotes to a level-1 Heading.
        b.text(page, 72.0, 80.0, 24.0, "Document Title", body, [0.0; 3]);
        b.text(
            page,
            72.0,
            130.0,
            12.0,
            "First body line of the document.",
            body,
            [0.0; 3],
        );
        b.text(
            page,
            72.0,
            146.0,
            12.0,
            "Second body line follows along.",
            body,
            [0.0; 3],
        );
        b.text(
            page,
            72.0,
            162.0,
            12.0,
            "Third body line wraps it up here.",
            body,
            [0.0; 3],
        );

        let doc = open(b.finish()).reconstruct_model();
        let blocks = page0_blocks(&doc);
        let heading_levels: Vec<u8> = blocks
            .iter()
            .filter_map(|b| match &b.kind {
                BK::Heading(h) => Some(h.level),
                _ => None,
            })
            .collect();
        assert_eq!(
            heading_levels.len(),
            1,
            "exactly one heading, got {blocks:?}"
        );
        assert_eq!(heading_levels[0], 1, "24pt over 12pt body → level 1");
        // The body lines remain paragraph(s), not headings.
        assert!(count_kind(blocks, |k| matches!(k, BK::Paragraph(_))) >= 1);
    }

    #[test]
    fn an_empty_page_pdf_reconstructs_without_panicking() {
        let mut b = PdfBuilder::new();
        b.add_page(612.0, 792.0);
        let doc = open(b.finish()).reconstruct_model();
        // One section, one page, no blocks.
        assert_eq!(doc.sections.len(), 1);
        assert_eq!(doc.sections[0].pages.len(), 1);
        assert!(page0_blocks(&doc).is_empty());
    }

    // ── underline recovery (drawn rule under a run) ──────────────────────────

    /// A thin horizontal rule directly under a run's baseline flags it underlined
    /// and is consumed (so it isn't also re-emitted as a shape).
    #[test]
    fn mark_underlines_flags_a_run_above_a_thin_horizontal_rule() {
        use crate::content::vector::{PathSeg, VectorPath};
        use crate::content::Bounds;
        // A run at lower-left (72,700), 100 wide, 12 tall (baseline ≈ y=700).
        let mut runs = vec![run("underlined", 72.0, 700.0, 12.0)];
        runs[0].w = 100.0;
        // A 0.6pt-tall rule just under the baseline, spanning the run width.
        let rule = VectorPath {
            index: 5,
            bounds: Some(Bounds {
                x: 72.0,
                y: 698.4,
                width: 100.0,
                height: 0.6,
            }),
            segments: vec![PathSeg::Move(72.0, 698.7), PathSeg::Line(172.0, 698.7)],
            fill: None,
            stroke: Some([0.0, 0.0, 0.0]),
            stroke_width: 0.6,
            fill_alpha: 1.0,
            stroke_alpha: 1.0,
            dash: Vec::new(),
        };
        let consumed = mark_underlines(&mut runs, &[rule]);
        assert!(runs[0].underline, "run over a thin rule is underlined");
        assert_eq!(
            consumed,
            vec![5],
            "the rule path is consumed, not drawn twice"
        );
    }

    /// A rule far from any baseline (or not overlapping) leaves runs un-underlined.
    #[test]
    fn mark_underlines_ignores_unrelated_rules() {
        use crate::content::vector::{PathSeg, VectorPath};
        use crate::content::Bounds;
        let mut runs = vec![run("plain", 72.0, 700.0, 12.0)];
        runs[0].w = 100.0;
        // A rule 200pt below the run — not an underline of it.
        let rule = VectorPath {
            index: 1,
            bounds: Some(Bounds {
                x: 72.0,
                y: 500.0,
                width: 100.0,
                height: 0.6,
            }),
            segments: vec![PathSeg::Move(72.0, 500.0), PathSeg::Line(172.0, 500.0)],
            fill: None,
            stroke: Some([0.0, 0.0, 0.0]),
            stroke_width: 0.6,
            fill_alpha: 1.0,
            stroke_alpha: 1.0,
            dash: Vec::new(),
        };
        let consumed = mark_underlines(&mut runs, &[rule]);
        assert!(!runs[0].underline);
        assert!(consumed.is_empty());
    }

    // ── strikethrough recovery (drawn rule across mid-glyph) ─────────────────

    /// A thin horizontal rule crossing a run at mid-glyph flags it struck through
    /// and is consumed (so it isn't also re-emitted as a shape).
    #[test]
    fn mark_strikes_flags_a_run_crossed_at_mid_glyph() {
        use crate::content::vector::{PathSeg, VectorPath};
        use crate::content::Bounds;
        // A run at lower-left (72,700), 100 wide, 12 tall.
        let mut runs = vec![run("struck", 72.0, 700.0, 12.0)];
        runs[0].w = 100.0;
        // A 0.6pt-tall rule at ~0.45·h above the box bottom (y ≈ 700 + 5.4).
        let rule = VectorPath {
            index: 7,
            bounds: Some(Bounds {
                x: 72.0,
                y: 705.1,
                width: 100.0,
                height: 0.6,
            }),
            segments: vec![PathSeg::Move(72.0, 705.4), PathSeg::Line(172.0, 705.4)],
            fill: None,
            stroke: Some([0.0, 0.0, 0.0]),
            stroke_width: 0.6,
            fill_alpha: 1.0,
            stroke_alpha: 1.0,
            dash: Vec::new(),
        };
        let consumed = mark_strikes(&mut runs, &[rule]);
        assert!(runs[0].strike, "run crossed at mid-glyph is struck through");
        assert!(
            !runs[0].underline,
            "a strike rule must not also flag underline"
        );
        assert_eq!(
            consumed,
            vec![7],
            "the rule path is consumed, not drawn twice"
        );
    }

    /// The underline and strikethrough bands do not overlap: an underline rule
    /// (just under the baseline) is never read as a strike, and a strike rule
    /// (mid-glyph) is never read as an underline. Same rule geometry, two passes.
    #[test]
    fn underline_and_strike_bands_are_disjoint() {
        use crate::content::vector::{PathSeg, VectorPath};
        use crate::content::Bounds;
        let thin = |index: usize, y: f64| VectorPath {
            index,
            bounds: Some(Bounds {
                x: 72.0,
                y: y - 0.3,
                width: 100.0,
                height: 0.6,
            }),
            segments: vec![PathSeg::Move(72.0, y), PathSeg::Line(172.0, y)],
            fill: None,
            stroke: Some([0.0, 0.0, 0.0]),
            stroke_width: 0.6,
            fill_alpha: 1.0,
            stroke_alpha: 1.0,
            dash: Vec::new(),
        };
        // Underline rule just under the baseline (≈ box bottom): strike must ignore it.
        let mut r1 = vec![run("x", 72.0, 700.0, 12.0)];
        r1[0].w = 100.0;
        let under = thin(1, 700.1);
        assert!(
            mark_strikes(&mut r1, &[under]).is_empty(),
            "strike ignores the underline band"
        );
        assert!(!r1[0].strike);

        // Strike rule at mid-glyph: underline must ignore it.
        let mut r2 = vec![run("x", 72.0, 700.0, 12.0)];
        r2[0].w = 100.0;
        let strike = thin(2, 705.4);
        assert!(
            mark_underlines(&mut r2, &[strike]).is_empty(),
            "underline ignores the strike band"
        );
        assert!(!r2[0].underline);
    }

    /// A struck-through run reaches the editable model: `pageBlocks` surfaces a
    /// paragraph whose inline run carries `style.strike = true`. Mirrors the
    /// underline e2e test, but the bar is drawn one band higher (mid-glyph) so it
    /// is recovered as a strikethrough, not an underline.
    #[test]
    fn page_blocks_expose_a_struck_through_run() {
        let mut b = PdfBuilder::new();
        let page = b.add_page(612.0, 792.0);
        let black = [0.0, 0.0, 0.0];
        let body = StdFont::Helvetica;

        // Three tightly-spaced 12pt body lines; the third is struck through by a
        // thin rule drawn across its mid-glyph (≈ baseline − 0.35·size, i.e. a few
        // points *above* the underline position of the same line).
        b.text(
            page,
            72.0,
            140.0,
            12.0,
            "Plain opening line of body text.",
            body,
            black,
        );
        b.text(
            page,
            72.0,
            156.0,
            12.0,
            "A second body line continues on.",
            body,
            black,
        );
        b.text(
            page,
            72.0,
            172.0,
            12.0,
            "Struck-through closing line now.",
            body,
            black,
        );
        // Underline of this line would sit at top-down ≈ 182.2 (just under the
        // baseline ≈ 181.6); the strike rides mid-glyph, ≈ 4–5pt higher.
        b.rect(page, 72.0, 177.0, 150.0, 0.6, None, Some(black));

        let doc = open(b.finish());
        let blocks = doc.page_blocks(1);
        assert!(
            !blocks.is_empty(),
            "page_blocks returns the reconstructed blocks"
        );

        let any_strike = blocks.iter().any(|bl| match &bl.kind {
            BK::Paragraph(p) => para_has_strike(p),
            BK::Heading(h) => para_has_strike(&h.para),
            _ => false,
        });
        assert!(
            any_strike,
            "the rule across the line flags a struck-through run"
        );
    }

    /// Whether any run in a paragraph is flagged struck through.
    fn para_has_strike(p: &crate::model::Paragraph) -> bool {
        p.runs.iter().any(|inl| match inl {
            crate::model::Inline::Run(r) => r.style.strike,
            _ => false,
        })
    }

    // ── end-to-end: `page_blocks` exposes the recognised structure (typed) ───

    /// The SDK's `pageBlocks` (→ `Document::page_blocks`) must surface the
    /// reconstruction so a thin editor can render it 1:1: a **bold heading** with
    /// a level, body runs carrying **bold**, a **drawn underline** flagged on its
    /// run, and a **ruled table** with rows of cells.
    #[test]
    fn page_blocks_expose_bold_heading_underline_and_table() {
        let mut b = PdfBuilder::new();
        let page = b.add_page(612.0, 792.0);
        let black = [0.0, 0.0, 0.0];
        let body = StdFont::Helvetica;
        let bold = StdFont::HelveticaBold;

        // A bold 24pt title (→ heading level 1 over a 12pt body). It is isolated
        // by a wide gap above three tightly-spaced body lines, so the leading
        // estimate reflects the body and the title breaks off as its own block.
        b.text(page, 72.0, 70.0, 24.0, "Quarterly Report", bold, black);
        // Body paragraph: three 12pt lines at a regular 16pt leading. The third is
        // underlined by a thin rule drawn just under its baseline.
        b.text(
            page,
            72.0,
            140.0,
            12.0,
            "Plain opening line of body text.",
            body,
            black,
        );
        b.text(
            page,
            72.0,
            156.0,
            12.0,
            "A second body line continues on.",
            body,
            black,
        );
        b.text(
            page,
            72.0,
            172.0,
            12.0,
            "Underlined closing line here now.",
            body,
            black,
        );
        // Underline rule under the third line: builder baseline ≈ top + size*0.8,
        // i.e. top-down 172 + 9.6 ≈ 181.6; draw a 0.6pt bar a hair below.
        b.rect(page, 72.0, 182.2, 150.0, 0.6, None, Some(black));

        // A 2×2 ruled table lower on the page.
        for &y in &[300.0, 324.0, 348.0] {
            b.rect(page, 72.0, y, 256.0, 0.6, None, Some(black));
        }
        for &x in &[72.0, 200.0, 328.0] {
            b.rect(page, x, 300.0, 0.6, 48.0, None, Some(black));
        }
        b.text(page, 80.0, 306.0, 11.0, "Name", body, black);
        b.text(page, 208.0, 306.0, 11.0, "Total", body, black);
        b.text(page, 80.0, 330.0, 11.0, "Alice", body, black);
        b.text(page, 208.0, 330.0, 11.0, "42", body, black);

        let doc = open(b.finish());
        let blocks = doc.page_blocks(1);
        assert!(
            !blocks.is_empty(),
            "page_blocks returns the reconstructed blocks"
        );

        // 1) A heading with a recovered level.
        let heading = blocks
            .iter()
            .find_map(|b| match &b.kind {
                BK::Heading(h) => Some(h),
                _ => None,
            })
            .expect("the large bold title is exposed as a Heading");
        assert!(
            heading.level >= 1 && heading.level <= 6,
            "heading carries a level"
        );
        // Its run is bold (recovered from the BaseFont name).
        let head_bold = first_run_style(&heading.para)
            .map(|s| s.bold)
            .unwrap_or(false);
        assert!(head_bold, "the heading run is flagged bold");

        // 2) Some paragraph run is flagged underlined (the drawn rule).
        let any_underline = blocks.iter().any(|b| match &b.kind {
            BK::Paragraph(p) => para_has_underline(p),
            BK::Heading(h) => para_has_underline(&h.para),
            _ => false,
        });
        assert!(
            any_underline,
            "the rule under the second line flags an underlined run"
        );

        // 3) A table with rows of cells whose content is reachable.
        let table = blocks
            .iter()
            .find_map(|b| match &b.kind {
                BK::Table(t) => Some(t),
                _ => None,
            })
            .expect("the ruled grid is exposed as a Table");
        assert_eq!(table.rows.len(), 2, "two body rows");
        assert!(
            table.rows.iter().all(|r| r.cells.len() == 2),
            "two cells per row"
        );
        // Cells carry editable block content (a paragraph with a run).
        let cell_text = cell_first_text(&table.rows[0].cells[0]);
        assert!(!cell_text.is_empty(), "a cell exposes its text run");
    }

    /// First run's [`CharStyle`] of a paragraph, if any.
    fn first_run_style(p: &crate::model::Paragraph) -> Option<&crate::model::CharStyle> {
        p.runs.iter().find_map(|i| match i {
            crate::model::Inline::Run(r) => Some(&r.style),
            _ => None,
        })
    }

    /// Whether any run in a paragraph is flagged underlined.
    fn para_has_underline(p: &crate::model::Paragraph) -> bool {
        p.runs.iter().any(|i| match i {
            crate::model::Inline::Run(r) => r.style.underline,
            _ => false,
        })
    }

    /// The text of a table cell's first paragraph run.
    fn cell_first_text(c: &crate::model::Cell) -> String {
        c.blocks
            .iter()
            .find_map(|b| match &b.kind {
                BK::Paragraph(p) => p.runs.iter().find_map(|i| match i {
                    crate::model::Inline::Run(r) => Some(r.text.clone()),
                    _ => None,
                }),
                _ => None,
            })
            .unwrap_or_default()
    }

    // ── #1 hyperlink recovery (e2e through `page_blocks`) ─────────────────────

    /// A URI link annotation over a run's box surfaces in `pageBlocks` as an
    /// `Inline::Link` carrying the URL, wrapping the covered run. Mirrors the
    /// other `page_blocks_expose_*` e2e tests: build a real PDF, attach the link
    /// in PDF user space (the runs' own coordinate space), reconstruct.
    #[test]
    fn page_blocks_expose_a_hyperlinked_run() {
        let mut b = PdfBuilder::new();
        let page = b.add_page(612.0, 792.0);
        let body = StdFont::Helvetica;
        // A single body line near the top. Top-down y=140, size 12 → in PDF user
        // space the glyph box spans roughly y ∈ [640, 652], x from 72.
        b.text(
            page,
            72.0,
            140.0,
            12.0,
            "Visit our documentation site",
            body,
            [0.0; 3],
        );

        let mut doc = open(b.finish());
        // Link rect over the run (PDF user space, generous band around the box).
        doc.add_uri_link(1, [70.0, 637.0, 320.0, 655.0], "https://example.com/docs")
            .expect("add link");

        let blocks = doc.page_blocks(1);
        assert!(!blocks.is_empty(), "page_blocks returns the blocks");

        // Find an Inline::Link anywhere in the page's paragraphs/headings.
        let mut found: Option<(crate::model::LinkTarget, String)> = None;
        for bl in &blocks {
            let para = match &bl.kind {
                BK::Paragraph(p) => Some(p),
                BK::Heading(h) => Some(&h.para),
                _ => None,
            };
            let Some(p) = para else { continue };
            for inl in &p.runs {
                if let crate::model::Inline::Link { href, children } = inl {
                    let text = children
                        .iter()
                        .find_map(|c| match c {
                            crate::model::Inline::Run(r) => Some(r.text.clone()),
                            _ => None,
                        })
                        .unwrap_or_default();
                    found = Some((href.clone(), text));
                }
            }
        }
        let (href, text) = found.expect("a hyperlinked run is exposed as Inline::Link");
        assert_eq!(
            href,
            crate::model::LinkTarget::Url("https://example.com/docs".to_string()),
            "the link carries its URL"
        );
        assert!(
            text.contains("documentation"),
            "the link wraps the covered run text, got {text:?}"
        );
    }

    // ── outline folding (#4) ────────────────────────────────────────────────

    fn flat(title: &str, level: usize, page: usize) -> FlatOutline {
        FlatOutline {
            title: title.to_string(),
            level,
            page,
        }
    }

    #[test]
    fn fold_outline_builds_a_nested_tree() {
        // h1 / (h2, h2 / h3) / h1 → two roots, the first with two children, the
        // second child carrying a grandchild.
        let tree = fold_outline(&[
            flat("Chapter 1", 0, 0),
            flat("Section 1.1", 1, 1),
            flat("Section 1.2", 1, 2),
            flat("Sub 1.2.1", 2, 2),
            flat("Chapter 2", 0, 5),
        ]);
        assert_eq!(tree.len(), 2, "two top-level chapters");
        assert_eq!(tree[0].title, "Chapter 1");
        assert_eq!(tree[0].page, 0);
        assert_eq!(tree[0].children.len(), 2, "chapter 1 has two sections");
        assert_eq!(tree[0].children[1].title, "Section 1.2");
        assert_eq!(
            tree[0].children[1].children.len(),
            1,
            "section 1.2 nests a subsection"
        );
        assert_eq!(tree[0].children[1].children[0].title, "Sub 1.2.1");
        assert_eq!(tree[1].title, "Chapter 2");
        assert_eq!(tree[1].page, 5);
        assert!(tree[1].children.is_empty());
    }

    #[test]
    fn fold_outline_keeps_a_level_jump_instead_of_dropping_it() {
        // A stray deep level with no intermediate parent must still be retained
        // (clamped under the deepest open level), never silently lost.
        let tree = fold_outline(&[flat("Top", 0, 0), flat("Way Deep", 5, 1)]);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].children.len(), 1, "deep entry attaches, not dropped");
        assert_eq!(tree[0].children[0].title, "Way Deep");
    }

    #[test]
    fn fold_outline_empty_is_empty() {
        assert!(fold_outline(&[]).is_empty());
    }

    // ── figure captions (#9) ────────────────────────────────────────────────

    #[test]
    fn caption_cue_detection_is_conservative() {
        assert!(is_caption_text("Figure 3: a flowchart"));
        assert!(is_caption_text("  Fig. 2 — overview"));
        assert!(is_caption_text("Table 1"));
        assert!(is_caption_text("Tableau 4 : résultats"));
        assert!(is_caption_text("Illustration 7"));
        // Must not fire on words that merely start with a cue substring.
        assert!(!is_caption_text("Imagery of the coast"));
        assert!(!is_caption_text("Tabletop layout"));
        assert!(!is_caption_text("Ordinary body sentence."));
        assert!(!is_caption_text(""));
    }

    fn caption_para_block(text: &str, frame: Rect) -> Block {
        use crate::model::{Inline, InlineRun, Paragraph};
        Block {
            id: BlockId(0),
            frame: Some(frame),
            rotation: Rotation::D0,
            kind: BlockKind::Paragraph(Paragraph {
                runs: vec![Inline::Run(InlineRun {
                    text: text.to_string(),
                    ..InlineRun::default()
                })],
                ..Paragraph::default()
            }),
        }
    }

    #[test]
    fn caption_directly_below_an_image_is_lifted_out() {
        // Image occupies y∈[100,200] (top-down), the caption sits just below it,
        // same width and horizontally aligned.
        let image_frame = Rect::new(50.0, 100.0, 200.0, 100.0);
        let mut blocks = vec![
            caption_para_block("Body paragraph far above", Rect::new(50.0, 10.0, 400.0, 12.0)),
            caption_para_block("Figure 1: the diagram", Rect::new(55.0, 205.0, 190.0, 12.0)),
        ];
        let alt = take_caption_for(&mut blocks, &image_frame);
        assert_eq!(alt.as_deref(), Some("Figure 1: the diagram"));
        assert_eq!(blocks.len(), 1, "the caption paragraph is removed");
        assert_eq!(
            match &blocks[0].kind {
                BlockKind::Paragraph(p) => paragraph_text(p),
                _ => String::new(),
            },
            "Body paragraph far above",
            "the unrelated body paragraph stays"
        );
    }

    #[test]
    fn full_width_paragraph_starting_with_table_is_not_a_caption() {
        // "Table of contents" body line at full measure must NOT be absorbed: too
        // wide relative to the image.
        let image_frame = Rect::new(50.0, 100.0, 120.0, 80.0);
        let mut blocks = vec![caption_para_block(
            "Table of historical events from 1900",
            Rect::new(50.0, 185.0, 500.0, 12.0),
        )];
        let alt = take_caption_for(&mut blocks, &image_frame);
        assert!(alt.is_none(), "a full-measure paragraph is not a caption");
        assert_eq!(blocks.len(), 1, "nothing removed");
    }

    #[test]
    fn distant_caption_is_not_associated() {
        // Right cue and width, but far below the image (gap ≫ CAPTION_MAX_GAP_PT).
        let image_frame = Rect::new(50.0, 100.0, 200.0, 100.0);
        let mut blocks = vec![caption_para_block(
            "Figure 9: detached",
            Rect::new(55.0, 400.0, 190.0, 12.0),
        )];
        assert!(take_caption_for(&mut blocks, &image_frame).is_none());
        assert_eq!(blocks.len(), 1);
    }

    // ── reading-order interleave of shapes (wave R7, objective #3) ────────────

    /// A non-ruling shape (a filled box) sitting *between* two paragraphs by its Y
    /// must be emitted between them in reading order, not appended after all the
    /// text. Single-column page: text order is unchanged, the figure slots in.
    #[test]
    fn a_shape_between_two_paragraphs_is_interleaved_by_y() {
        use crate::content::vector::{PathSeg, VectorPath};
        use crate::content::Bounds;

        // Two single-column paragraphs, a wide vertical gap between them; a filled
        // box (not a thin rule → survives as a Shape) sits in that gap.
        let runs = vec![
            run("First paragraph line one.", 72.0, 700.0, 12.0),
            run("First paragraph line two.", 72.0, 684.0, 12.0),
            run("Second paragraph way down.", 72.0, 560.0, 12.0),
            run("Second paragraph continues.", 72.0, 544.0, 12.0),
        ];
        // A 120×60 filled box centred in the gap (PDF user space y≈600..660).
        let box_shape = VectorPath {
            index: 0,
            bounds: Some(Bounds {
                x: 72.0,
                y: 600.0,
                width: 120.0,
                height: 60.0,
            }),
            segments: vec![
                PathSeg::Move(72.0, 600.0),
                PathSeg::Line(192.0, 600.0),
                PathSeg::Line(192.0, 660.0),
                PathSeg::Line(72.0, 660.0),
                PathSeg::Close,
            ],
            fill: Some([0.8, 0.8, 0.8]),
            stroke: None,
            stroke_width: 0.0,
            fill_alpha: 1.0,
            stroke_alpha: 1.0,
            dash: Vec::new(),
        };

        let mut ids = IdGen::default();
        let blocks = reconstruct_page(
            runs,
            std::slice::from_ref(&box_shape),
            &[],
            (0.0, 0.0, 612.0, 792.0),
            &mut ids,
            &[],
            None,
        );

        // Expect three blocks: para, shape, para — in that vertical order.
        let kinds: Vec<&str> = blocks
            .iter()
            .map(|b| match &b.kind {
                BK::Paragraph(_) => "para",
                BK::Shape(_) => "shape",
                BK::Heading(_) => "heading",
                BK::Image(_) => "image",
                _ => "other",
            })
            .collect();
        let shape_pos = kinds.iter().position(|&k| k == "shape");
        let first_para = kinds.iter().position(|&k| k == "para");
        let last_para = kinds.iter().rposition(|&k| k == "para");
        assert!(
            shape_pos.is_some(),
            "the filled box survives as a Shape, got {kinds:?}"
        );
        let (shape_pos, first_para, last_para) =
            (shape_pos.unwrap(), first_para.unwrap(), last_para.unwrap());
        assert!(
            first_para < shape_pos && shape_pos < last_para,
            "the shape is read between the two paragraphs, got {kinds:?}"
        );
    }
}
