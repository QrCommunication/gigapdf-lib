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
/// in points. `tag_blocks`, when `Some`, is the already-built block list from a
/// `/StructTreeRoot` walk and is used verbatim (the author tagged the document).
#[allow(clippy::too_many_arguments)]
pub fn reconstruct_page(
    mut text_runs: Vec<ReconRun>,
    vpaths: &[VectorPath],
    image_refs: &[PlacedImageRef],
    geom: (f64, f64, f64, f64),
    ids: &mut IdGen,
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

    // 1. Lines, then 2. reading-order columns over those lines.
    let lines = lines::group_into_lines(&text_runs);
    let body = body_font_size(&text_runs, 12.0);
    let order = columns::reading_order(&lines);

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
    let blocks = resolve_text_blocks(blocks, &lines, body, ids, |x, y, w, h| {
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
    for img in image_refs {
        out.push(Block {
            id: ids.mint(),
            frame: Some(frame_top_down(img.x, img.y, img.w, img.h, x0, y0, page_h)),
            rotation: Rotation::D0,
            kind: BlockKind::Image(ImageRef {
                resource: img.resource,
                alt: None,
            }),
        });
    }

    out
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
fn resolve_text_blocks(
    blocks: Vec<Block>,
    lines: &[lines::ReconLine],
    body: f64,
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
        let text_blocks = lower_text_group(&group, body, ids, to_frame);
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
/// paragraphs, with headings and lists promoted out of them.
fn lower_text_group(
    group: &[&lines::ReconLine],
    body: f64,
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
                for para_lines in paragraphs::split_paragraphs(&lines, body) {
                    let block = paragraphs::build_paragraph(&para_lines, ids, to_frame);
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
}
