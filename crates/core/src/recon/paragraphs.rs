//! Stage 3 — **paragraph assembly**. Merge a column's lines into paragraphs.
//! Within one paragraph the lines are stacked at a regular leading; a new
//! paragraph begins when:
//!   * the inter-line gap exceeds `1.5 × leading` (a blank-line break), or
//!   * a line is indented like a first line (its left jumps right of the body
//!     left by more than a small threshold), or
//!   * the alignment visibly changes (a centred line after a flush-left block).
//!
//! [`ParagraphStyle::align`] is derived from how the lines' starts and ends vary
//! against the block's left/right margins (centred ⇒ both insets symmetric;
//! right ⇒ ends align, starts ragged; justify ⇒ both edges flush on the inner
//! lines).

use super::lines::ReconLine;
use super::{median, run_char_style, IdGen, ParaLink, ReconRun};
use crate::model::{
    Align, Block, BlockKind, Inline, InlineRun, LineHeight, Paragraph, ParagraphStyle, Rect, VAlign,
};

/// Geometry + decoration context a paragraph needs to recover its
/// [`ParagraphStyle`] spacing/indents and to wrap link runs.
///
/// All coordinates are **PDF user space** (origin bottom-left, *Y up*) — the
/// same space the [`ReconLine`] runs live in. Defaults (`page_left = 0`, empty
/// `links`, `prev_bottom = None`, `leading = 0`) reproduce the legacy
/// no-spacing behaviour, so the back-compat [`build_paragraph`] wrapper can lean
/// on `ParaContext::default()`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ParaContext<'a> {
    /// Document body font size (points) — calibrates super/sub detection.
    pub body: f64,
    /// Estimated body leading for the prose group this paragraph belongs to
    /// (points). `0` ⇒ leading not known ⇒ `line_height` left `Normal`.
    pub leading: f64,
    /// The prose **group's** left margin (min line-left across the whole group),
    /// so a first-line indent is measured against the body, not just this
    /// paragraph's own (possibly already-indented) lines.
    pub group_left: f64,
    /// The page's MediaBox left edge (points): `indent_left_pt` is the block's
    /// left margin relative to this, so a host re-lays the paragraph in the same
    /// horizontal position.
    pub page_left: f64,
    /// Bottom edge (PDF user-space `y`, lower = further down) of the previous
    /// emitted paragraph in this group, or `None` for the first. Drives
    /// `space_before_pt` from the inter-paragraph gap.
    pub prev_bottom: Option<f64>,
    /// Hyperlink rectangles (already mapped to the model target), in PDF user
    /// space. A run whose box overlaps a rect is wrapped in [`Inline::Link`].
    pub links: &'a [ParaLink],
}

/// Split a group of reading-order lines into paragraphs (each a `Vec` of line
/// references). `body` is the document body font size, used to estimate leading.
pub fn split_paragraphs<'a>(lines: &[&'a ReconLine], body: f64) -> Vec<Vec<&'a ReconLine>> {
    if lines.is_empty() {
        return Vec::new();
    }
    // Estimate the body leading from the median line-to-line vertical step (a
    // paragraph split is judged on the group's own leading; a single-line group
    // never splits, so the document fallback is irrelevant here → `0.0`).
    let leading = estimate_leading(lines, body, 0.0);
    let body_left = block_left(lines);

    let mut paras: Vec<Vec<&ReconLine>> = vec![vec![lines[0]]];
    for pair in lines.windows(2) {
        let (prev, cur) = (pair[0], pair[1]);
        // Vertical gap between baselines (top of cur minus top of prev, going
        // down the page → prev.top - cur.top is the step).
        let step = (prev.center_y() - cur.center_y()).abs();
        let big_gap = step > leading * 1.5;
        // First-line indent: the line starts clearly right of the block left.
        let indent = cur.left() - body_left > (body * 0.6).max(6.0);
        // Alignment change: a centred line breaking a flush-left run (or back).
        let align_break = align_of_line(prev, body_left, block_right(lines))
            != align_of_line(cur, body_left, block_right(lines))
            && (is_centered(cur, body_left, block_right(lines))
                || is_centered(prev, body_left, block_right(lines)));

        if big_gap || indent || align_break {
            paras.push(vec![cur]);
        } else {
            paras.last_mut().unwrap().push(cur);
        }
    }
    paras
}

/// Build one [`Block::Paragraph`] from a paragraph's lines with no recovered
/// spacing/indents and no link wrapping — the back-compat entry point. The runs
/// of every line are concatenated into inline runs separated by
/// [`Inline::LineBreak`]; the frame is the union of the lines' boxes (flipped to
/// top-down by `to_frame`).
pub fn build_paragraph(
    para: &[&ReconLine],
    ids: &mut IdGen,
    to_frame: impl Fn(f64, f64, f64, f64) -> Rect,
) -> Block {
    build_paragraph_styled(para, &ParaContext::default(), ids, to_frame)
}

/// Build a [`Block::Paragraph`] **with recovered [`ParagraphStyle`]** (line
/// height, first-line/left indents, space-before) and **link-wrapped runs**,
/// using the geometry/decoration carried in `ctx`.
///
/// Spacing recovery (all PDF points):
/// - `line_height = Multiple(leading / body_size)` when a multi-line paragraph
///   has a measurable leading distinct from a single-spaced body.
/// - `first_line_pt` = the first line's left minus the body left, when the
///   opener is indented to the right of the rest of the group.
/// - `indent_left_pt` = the block's left margin minus the page's left edge.
/// - `space_before_pt` = the gap from the previous paragraph's bottom to this
///   paragraph's top, beyond one line of leading (the normal single step).
pub(crate) fn build_paragraph_styled(
    para: &[&ReconLine],
    ctx: &ParaContext,
    ids: &mut IdGen,
    to_frame: impl Fn(f64, f64, f64, f64) -> Rect,
) -> Block {
    let (x, y, w, h) = union_box(para);
    let left = block_left(para);
    let right = block_right(para);

    // Dominant baseline + body size of each line, to flag super/sub runs that
    // sit smaller-and-raised (or smaller-and-lowered) relative to their line.
    let mut runs: Vec<Inline> = Vec::new();
    for (i, line) in para.iter().enumerate() {
        if i > 0 {
            runs.push(Inline::LineBreak);
        }
        let (line_base, line_size) = line_baseline_and_size(line);
        // Right edge / height of the previous emitted run *on this line*, to
        // decide a synthesized inter-word space. `None` at each line start (the
        // `LineBreak` already separates lines).
        let mut prev: Option<(f64, f64, &str)> = None;
        for r in &line.runs {
            let t = r.text.trim();
            if t.is_empty() {
                continue;
            }
            // A dense form splits one word across embedded fonts ("ENFANT"+"S");
            // joining every run with a space shreds words. Insert a space only
            // when the horizontal gap to the previous run is a real inter-word
            // gap (gap-aware, mirroring `content::group_lines`), and neither side
            // already carries its own whitespace.
            if let Some((prev_right, prev_h, prev_raw)) = prev {
                if !prev_raw.ends_with(char::is_whitespace)
                    && !r.text.starts_with(char::is_whitespace)
                    && !super::runs_join(prev_right, r.x, r.h.max(prev_h))
                {
                    runs.push(Inline::Run(InlineRun {
                        text: " ".to_string(),
                        style: run_char_style(r),
                        source_index: None,
                    }));
                }
            }
            let mut style = run_char_style(r);
            style.vertical_align = vertical_align_of(r, line_base, line_size);
            let inline = Inline::Run(InlineRun {
                text: r.text.clone(),
                style,
                source_index: r.source_index,
            });
            // Wrap the run in a Link when its box overlaps a link rect.
            match link_for_run(r, ctx.links) {
                Some(href) => runs.push(Inline::Link {
                    href,
                    children: vec![inline],
                }),
                None => runs.push(inline),
            }
            prev = Some((r.right(), r.h, &r.text));
        }
    }

    let align = paragraph_align(para, left, right);
    let style = paragraph_style(para, ctx, align, left, right, y, h);
    // Coalesce adjacent `Inline::Run`s that share the same character style
    // (family, size, bold/italic/underline/strike, color, background) into a
    // single run. PDF often splits a single logical sentence into dozens of
    // runs — one per glyph cluster or per embedded-font fragment — and without
    // this pass each becomes its own `<w:r>` in the DOCX, making the document
    // impossible to edit (every word/letter a separate run). Merging them here
    // means the exported Word/ODT/HTML has clean, contiguous styled spans.
    let runs = coalesce_runs(runs);
    let paragraph = Paragraph {
        style,
        style_ref: None,
        runs,
    };
    Block {
        id: ids.mint(),
        frame: Some(to_frame(x, y, w, h)),
        // Honour a rotated/vertical baseline: an in-page rotated run (its
        // text/CTM matrix angled even on an un-rotated page) lowers to a block
        // carrying that rotation, instead of being flattened upright. Upright
        // prose stays `Rotation::D0` — byte-identical to before.
        rotation: super::lines::lines_rotation(para),
        kind: BlockKind::Paragraph(paragraph),
    }
}

/// Assemble the [`ParagraphStyle`] for a paragraph from its geometry and the
/// surrounding [`ParaContext`]. `y`/`h` are the paragraph's union box bottom and
/// height in PDF user space (`top = y + h`).
fn paragraph_style(
    para: &[&ReconLine],
    ctx: &ParaContext,
    align: Align,
    left: f64,
    right: f64,
    y: f64,
    h: f64,
) -> ParagraphStyle {
    let _ = right;
    let body = ctx.body.max(1.0);

    // Line height: only meaningful for multi-line paragraphs whose leading is
    // known and visibly different from single spacing. Encode as a multiple of
    // the body size so the figure is resolution-independent.
    let line_height = if para.len() >= 2 && ctx.leading > 0.0 {
        let mult = ctx.leading / body;
        // Round-trip-friendly: leave Normal when it's within 3% of 1.0× (the
        // font's own single spacing) to avoid spurious 1.00× annotations.
        if (mult - 1.0).abs() > 0.03 {
            LineHeight::Multiple(round2(mult))
        } else {
            LineHeight::Normal
        }
    } else {
        LineHeight::Normal
    };

    // First-line indent: the opener sits to the right of the group's body left
    // (the same jump that triggered a paragraph split). Measured against the
    // group left, not this block's own left (which equals the indented opener).
    let first_line_pt = {
        let opener_left = para.first().map(|l| l.left()).unwrap_or(left);
        let indent = opener_left - ctx.group_left;
        let threshold = (body * 0.6).max(6.0);
        if indent > threshold {
            round2(indent)
        } else {
            0.0
        }
    };

    // Left indent: the block's left margin relative to the page's left edge.
    let indent_left_pt = {
        let indent = left - ctx.page_left;
        if indent > 1.0 {
            round2(indent)
        } else {
            0.0
        }
    };

    // Space-before: the inter-paragraph gap beyond a normal single line step.
    // `prev_bottom` and our `top` are PDF user-space Y (larger = higher), so the
    // visual gap is `prev_bottom - top`; subtract one line of leading (the step
    // that would exist even with no extra spacing).
    let space_before_pt = match ctx.prev_bottom {
        Some(prev_bottom) if ctx.leading > 0.0 => {
            let top = y + h;
            let gap = prev_bottom - top - (ctx.leading - body).max(0.0);
            if gap > body * 0.25 {
                round2(gap)
            } else {
                0.0
            }
        }
        _ => 0.0,
    };

    ParagraphStyle {
        align,
        space_before_pt,
        space_after_pt: 0.0,
        indent_left_pt,
        indent_right_pt: 0.0,
        first_line_pt,
        line_height,
        ..Default::default()
    }
}

/// The line's dominant baseline (`y`, PDF user space) and body font size,
/// derived from its **largest** runs — the run set that defines the main text
/// of the line, against which smaller raised/lowered runs read as super/sub.
fn line_baseline_and_size(line: &ReconLine) -> (f64, f64) {
    // Body size = the line's representative (median) run size.
    let mut sizes: Vec<f64> = line.runs.iter().map(|r| r.size.max(1.0)).collect();
    let body_size = median(&mut sizes, line.h.max(1.0));
    // Dominant baseline = the median box-bottom of runs at/above ~0.85× the body
    // size (the main-text runs), so a single small super/sub run can't drag it.
    let mut bases: Vec<f64> = line
        .runs
        .iter()
        .filter(|r| r.size >= body_size * 0.85)
        .map(|r| r.y)
        .collect();
    let base = median(&mut bases, line.y);
    (base, body_size)
}

/// Classify a run as super/sub/baseline against its line's dominant baseline and
/// body size. A super/subscript run is **distinctly smaller** (≤ 0.75× the body
/// size) **and** offset vertically: its box bottom rides clearly above the
/// baseline (superscript) or sits clearly below it (subscript). The vertical
/// offset must exceed a fraction of the body size so ordinary baseline jitter
/// (kerning, hinting) is not misread.
fn vertical_align_of(run: &ReconRun, line_base: f64, body_size: f64) -> VAlign {
    if body_size <= 0.0 || run.size > body_size * 0.75 {
        return VAlign::Baseline;
    }
    // Offset of this run's box bottom from the line baseline (PDF Y up: positive
    // = higher on the page = raised).
    let offset = run.y - line_base;
    let threshold = body_size * 0.20;
    if offset > threshold {
        VAlign::Super
    } else if offset < -threshold {
        VAlign::Sub
    } else {
        VAlign::Baseline
    }
}

/// The link target whose rectangle overlaps a run's box, if any. Overlap is
/// generous (centre-in or ≥ 50 % box coverage) so a link rect drawn a little
/// tighter or looser than the glyph extent still claims the run.
fn link_for_run(run: &ReconRun, links: &[ParaLink]) -> Option<crate::model::LinkTarget> {
    if links.is_empty() || run.w <= 0.0 {
        return None;
    }
    let rx0 = run.x;
    let rx1 = run.x + run.w;
    let ry0 = run.y;
    let ry1 = run.y + run.h;
    let cx = (rx0 + rx1) / 2.0;
    let cy = (ry0 + ry1) / 2.0;
    for link in links {
        let [lx0, ly0, lx1, ly1] = link.rect;
        let (lx0, lx1) = (lx0.min(lx1), lx0.max(lx1));
        let (ly0, ly1) = (ly0.min(ly1), ly0.max(ly1));
        // Centre of the run inside the rect → claimed.
        let center_in = cx >= lx0 && cx <= lx1 && cy >= ly0 && cy <= ly1;
        // Or a healthy box overlap (handles a rect tighter than the glyph box).
        let ox = (rx1.min(lx1) - rx0.max(lx0)).max(0.0);
        let oy = (ry1.min(ly1) - ry0.max(ly0)).max(0.0);
        let area = (rx1 - rx0).max(0.01) * (ry1 - ry0).max(0.01);
        let covered = ox * oy >= area * 0.5;
        if center_in || covered {
            return Some(link.target.clone());
        }
    }
    None
}

/// Coalesce adjacent `Inline::Run` entries that share the same `CharStyle` into
/// a single run, so the exported document has clean contiguous spans instead of
/// one run per PDF glyph fragment. `LineBreak`, `Image` and `Link` entries act
/// as hard boundaries — runs are never merged across them.
///
/// Two styles are "the same" when family, size, bold, italic, underline, strike,
/// color and background all match. `vertical_align` (super/sub) is also compared
/// so a superscript span stays separate from the baseline body.
pub(crate) fn coalesce_runs(runs: Vec<Inline>) -> Vec<Inline> {
    let mut out: Vec<Inline> = Vec::with_capacity(runs.len());
    for r in runs {
        match r {
            Inline::Run(run) => {
                // Try to merge into the previous run if it's a plain Run with a
                // compatible style. Two styles are "compatible" when family,
                // bold/italic/underline/strike, color, background and
                // vertical_align all match, AND the font sizes are within 0.5pt
                // (PDF float precision jitter). The merged run keeps the first
                // run's exact style so the span stays stable downstream.
                let merge = out.last_mut().and_then(|last| {
                    if let Inline::Run(prev) = last {
                        let same = prev.style.family == run.style.family
                            && prev.style.generic == run.style.generic
                            && prev.style.bold == run.style.bold
                            && prev.style.italic == run.style.italic
                            && prev.style.underline == run.style.underline
                            && prev.style.strike == run.style.strike
                            && prev.style.color == run.style.color
                            && prev.style.background == run.style.background
                            && prev.style.vertical_align == run.style.vertical_align
                            && (prev.style.size_pt - run.style.size_pt).abs() < 0.5;
                        (same
                            && prev.source_index.is_none() == run.source_index.is_none())
                            .then_some(prev)
                    } else {
                        None
                    }
                });
                match merge {
                    Some(prev) => prev.text.push_str(&run.text),
                    None => out.push(Inline::Run(run)),
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Round to 2 decimals — keeps recovered spacing/indents tidy (and JSON stable)
/// without pretending to sub-point precision the heuristic doesn't have.
fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// The leading (line-to-line step) of a whole prose group — exposed so the
/// caller can build a [`ParaContext`] whose `leading` reflects the group, not a
/// single paragraph (a one-line paragraph has no internal step to measure).
///
/// When the group is too short to measure its own leading (a single-line group),
/// it falls back to `doc_leading` — the **document/page** median leading (gap #75,
/// sub-item 9) — instead of a fixed `1.2 × body`, so an isolated heading/caption
/// inherits the spacing the rest of the page actually uses. Pass `0.0` for
/// `doc_leading` to keep the legacy `1.2 × body` fallback.
pub(crate) fn group_leading(lines: &[&ReconLine], body: f64, doc_leading: f64) -> f64 {
    estimate_leading(lines, body, doc_leading)
}

/// The **document/page** body leading: the median baseline step across *all* the
/// page's lines. Robust by construction — the large cross-group / cross-column
/// gaps are a minority and the median ignores them, leaving the ordinary
/// line-to-line step. Used as the per-group fallback (see [`group_leading`]) so a
/// single-line group inherits the real document leading. Falls back to
/// `1.2 × body` only when the page itself has no measurable step.
pub(crate) fn document_leading(lines: &[&ReconLine], body: f64) -> f64 {
    // Order by vertical position so consecutive steps reflect real adjacency.
    let mut ys: Vec<f64> = lines.iter().map(|l| l.center_y()).collect();
    ys.sort_by(|a, b| b.partial_cmp(a).unwrap_or(core::cmp::Ordering::Equal));
    let mut steps: Vec<f64> = ys
        .windows(2)
        .map(|w| (w[0] - w[1]).abs())
        .filter(|s| *s > 0.0)
        .collect();
    if steps.is_empty() {
        return body * 1.2;
    }
    median(&mut steps, body * 1.2).max(body).max(1.0)
}

/// The group's left margin (min line-left) — exposed for [`ParaContext::group_left`]
/// so a first-line indent is measured against the whole group's body.
pub(crate) fn group_left(lines: &[&ReconLine]) -> f64 {
    block_left(lines)
}

/// A paragraph's union-box bottom edge (PDF user-space `y`, lower = further down
/// the page) — fed to the next paragraph's [`ParaContext::prev_bottom`] to
/// recover inter-paragraph `space_before`.
pub(crate) fn paragraph_bottom(para: &[&ReconLine]) -> f64 {
    let (_, y, _, _) = union_box(para);
    y
}

/// Estimate body leading from consecutive line steps. When the group has no
/// measurable step (a single line), fall back to `doc_leading` (the document
/// leading) if positive, else to `1.2 × body`. Floored at the body size.
fn estimate_leading(lines: &[&ReconLine], body: f64, doc_leading: f64) -> f64 {
    let mut steps: Vec<f64> = lines
        .windows(2)
        .map(|w| (w[0].center_y() - w[1].center_y()).abs())
        .filter(|s| *s > 0.0)
        .collect();
    let fallback = if doc_leading > 0.0 {
        doc_leading
    } else {
        body * 1.2
    };
    let med = median(&mut steps, fallback);
    med.max(body).max(1.0)
}

/// The block's left margin = the minimum line left (robust enough for indent
/// detection; a first-line-indented opener sits to the right of this).
fn block_left(lines: &[&ReconLine]) -> f64 {
    lines.iter().map(|l| l.left()).fold(f64::INFINITY, f64::min)
}

/// The block's right margin = the maximum line right.
fn block_right(lines: &[&ReconLine]) -> f64 {
    lines
        .iter()
        .map(|l| l.right())
        .fold(f64::NEG_INFINITY, f64::max)
}

fn union_box(lines: &[&ReconLine]) -> (f64, f64, f64, f64) {
    let mut x0 = f64::INFINITY;
    let mut y0 = f64::INFINITY;
    let mut x1 = f64::NEG_INFINITY;
    let mut y1 = f64::NEG_INFINITY;
    for l in lines {
        x0 = x0.min(l.x);
        y0 = y0.min(l.y);
        x1 = x1.max(l.x + l.w);
        y1 = y1.max(l.y + l.h);
    }
    (x0, y0, x1 - x0, y1 - y0)
}

/// Per-line alignment classification against the block margins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineAlign {
    Left,
    Center,
    Right,
}

fn is_centered(line: &ReconLine, left: f64, right: f64) -> bool {
    matches!(align_of_line(line, left, right), LineAlign::Center)
}

fn align_of_line(line: &ReconLine, left: f64, right: f64) -> LineAlign {
    let tol = (line.h * 0.5).max(3.0);
    let lead = line.left() - left; // inset from left margin
    let trail = right - line.right(); // inset from right margin
    if lead > tol && (lead - trail).abs() <= tol.max(lead * 0.4) {
        LineAlign::Center
    } else if lead > tol && trail <= tol {
        LineAlign::Right
    } else {
        LineAlign::Left
    }
}

/// Derive the paragraph alignment from how its lines sit between the block
/// margins. The catch handled here: the *widest* line defines both margins, so
/// it always looks flush on both sides — alignment must be read from the
/// *inset* lines (those that don't reach a margin).
///
/// - **Center** — the inset lines are mostly symmetric (lead ≈ trail).
/// - **Right** — the inset lines hug the right margin (ragged left).
/// - **Justify** — ≥ 3 lines and every non-last line reaches *both* margins
///   (so the inset is only on the final line). The ≥ 3 floor avoids mistaking a
///   two-line centred/ragged block (whose one "inner" line is the margin setter)
///   for justified text.
/// - **Left** otherwise.
fn paragraph_align(lines: &[&ReconLine], left: f64, right: f64) -> Align {
    if lines.is_empty() {
        return Align::Left;
    }
    let tol = (lines[0].h * 0.5).max(3.0);
    // Inset lines: those not flush against *both* margins (i.e. not a margin
    // setter). These are what reveal the alignment.
    let inset: Vec<&&ReconLine> = lines
        .iter()
        .filter(|l| l.left() - left > tol || right - l.right() > tol)
        .collect();

    if !inset.is_empty() {
        let centered = inset
            .iter()
            .filter(|l| align_of_line(l, left, right) == LineAlign::Center)
            .count();
        let right_hug = inset
            .iter()
            .filter(|l| align_of_line(l, left, right) == LineAlign::Right)
            .count();
        if centered * 2 >= inset.len() && centered > 0 {
            return Align::Center;
        }
        if right_hug == inset.len() {
            return Align::Right;
        }
    }

    // Justify: ≥3 lines, every non-last line flush to both margins.
    if lines.len() >= 3 {
        let inner_full = lines[..lines.len() - 1]
            .iter()
            .all(|l| right - l.right() <= tol && l.left() - left <= tol);
        if inner_full {
            return Align::Justify;
        }
    }
    Align::Left
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::style::TextStyle;
    use crate::recon::lines::group_into_lines;
    use crate::recon::ReconRun;

    fn run(text: &str, x: f64, y: f64, w: f64) -> ReconRun {
        ReconRun {
            text: text.to_string(),
            x,
            y,
            w,
            h: 12.0,
            size: 12.0,
            style: TextStyle::default(),
            rotation: 0.0,
            source_index: None,
            underline: false,
            strike: false,
        }
    }

    fn lines_of(runs: &[ReconRun]) -> Vec<ReconLine> {
        group_into_lines(runs)
    }

    /// A run carrying a baseline `rotation` (degrees CCW), used to exercise the
    /// run-level rotation lowering (#28).
    fn run_rot(text: &str, x: f64, y: f64, w: f64, rotation: f64) -> ReconRun {
        ReconRun {
            rotation,
            ..run(text, x, y, w)
        }
    }

    /// Lower a single line of runs to one paragraph block (the path the page
    /// reconstructor takes for an isolated rotated label).
    fn paragraph_of(runs: &[ReconRun]) -> Block {
        let lines = lines_of(runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let mut ids = IdGen::default();
        build_paragraph_styled(&refs, &ParaContext::default(), &mut ids, Rect::new)
    }

    #[test]
    fn document_leading_is_robust_to_group_gaps() {
        // Mostly 16-pt steps with two large between-group voids; the median ignores
        // the outliers and recovers the real document leading (gap #75 #9).
        let runs = vec![
            run("a", 72.0, 700.0, 20.0),
            run("b", 72.0, 684.0, 20.0),
            run("c", 72.0, 668.0, 20.0),
            // big void (new block)
            run("d", 72.0, 560.0, 20.0),
            run("e", 72.0, 544.0, 20.0),
            // another void
            run("f", 72.0, 420.0, 20.0),
        ];
        let lines = lines_of(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let lead = document_leading(&refs, 12.0);
        assert!((lead - 16.0).abs() < 1.0, "document leading ≈ 16, got {lead}");
    }

    #[test]
    fn single_line_group_inherits_document_leading() {
        // A one-line group cannot measure its own leading. With a positive
        // `doc_leading` it inherits it (gap #75 #9); with `0.0` it keeps the legacy
        // `1.2 × body` fallback.
        let runs = vec![run("lonely heading", 72.0, 700.0, 90.0)];
        let lines = lines_of(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let body = 12.0;
        assert_eq!(group_leading(&refs, body, 18.0), 18.0, "inherits document leading");
        let legacy = group_leading(&refs, body, 0.0);
        assert!(
            (legacy - body * 1.2).abs() < 1e-9,
            "no document leading → 1.2×body, got {legacy}"
        );
        // The fallback is still floored at the body size.
        assert_eq!(group_leading(&refs, body, 5.0), body, "floored at body size");
    }

    #[test]
    fn upright_paragraph_stays_d0() {
        use crate::model::geom::Rotation;
        // A plain horizontal line must remain `Rotation::D0` — the rotation
        // stage is a no-op for upright text (byte-identical to before #28).
        let block = paragraph_of(&[run("upright text", 72.0, 700.0, 110.0)]);
        assert_eq!(block.rotation, Rotation::D0);
    }

    #[test]
    fn rotated_90_run_lowers_to_a_d90_block() {
        use crate::model::geom::Rotation;
        // A 90° CCW baseline (a vertical label drawn on an un-rotated page)
        // lowers to a paragraph carrying `Rotation::D90`, not flattened upright.
        let block = paragraph_of(&[run_rot("Vertical", 72.0, 700.0, 80.0, 90.0)]);
        assert_eq!(block.rotation, Rotation::D90);
        // The text content is preserved.
        match &block.kind {
            BlockKind::Paragraph(p) => assert!(p.runs.iter().any(|r| matches!(
                r,
                Inline::Run(InlineRun { text, .. }) if text == "Vertical"
            ))),
            other => panic!("expected paragraph, got {other:?}"),
        }
    }

    #[test]
    fn rotated_180_run_lowers_to_a_d180_block() {
        use crate::model::geom::Rotation;
        let block = paragraph_of(&[run_rot("Upside down", 72.0, 700.0, 110.0, 180.0)]);
        assert_eq!(block.rotation, Rotation::D180);
    }

    #[test]
    fn rotated_270_run_lowers_to_a_d270_block() {
        use crate::model::geom::Rotation;
        // A 270° CCW baseline arrives from the matrix `atan2` as -90°; the
        // lowering reports it as the cardinal `D270`.
        let block = paragraph_of(&[run_rot("Sideways", 72.0, 700.0, 80.0, -90.0)]);
        assert_eq!(block.rotation, Rotation::D270);
    }

    #[test]
    fn arbitrary_angle_run_lowers_to_a_free_form_rotation() {
        use crate::model::geom::Rotation;
        // A non-cardinal baseline (a diagonal stamp) carries through as the
        // free-form `Deg` variant with its angle.
        let block = paragraph_of(&[run_rot("Diagonal", 72.0, 700.0, 90.0, 30.0)]);
        match block.rotation {
            Rotation::Deg(d) => assert!((d - 30.0).abs() < 1e-9, "got {d}"),
            other => panic!("expected Deg(30), got {other:?}"),
        }
    }

    #[test]
    fn near_cardinal_angle_snaps_to_the_exact_variant() {
        use crate::model::geom::Rotation;
        // A scaled/skewed CTM yields an angle a hair off 90°; it must still snap
        // to the exact `D90` (so cardinal cases stay first-class).
        let block = paragraph_of(&[run_rot("Almost 90", 72.0, 700.0, 80.0, 90.2)]);
        assert_eq!(block.rotation, Rotation::D90);
    }

    #[test]
    fn tight_lines_are_one_paragraph() {
        // Three lines, regular ~14pt leading → a single paragraph.
        let runs = vec![
            run("line one here", 72.0, 700.0, 120.0),
            run("line two here", 72.0, 686.0, 120.0),
            run("line three end", 72.0, 672.0, 110.0),
        ];
        let lines = lines_of(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let paras = split_paragraphs(&refs, 12.0);
        assert_eq!(paras.len(), 1, "tight lines = one paragraph");
        assert_eq!(paras[0].len(), 3);
    }

    #[test]
    fn a_big_vertical_gap_breaks_paragraphs() {
        // Two blocks separated by a blank line (gap ~30pt ≫ 1.5×leading).
        let runs = vec![
            run("para one a", 72.0, 700.0, 100.0),
            run("para one b", 72.0, 686.0, 100.0),
            run("para two a", 72.0, 640.0, 100.0),
            run("para two b", 72.0, 626.0, 100.0),
        ];
        let lines = lines_of(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let paras = split_paragraphs(&refs, 12.0);
        assert_eq!(paras.len(), 2, "blank-line gap splits into two paragraphs");
    }

    #[test]
    fn first_line_indent_starts_a_new_paragraph() {
        let runs = vec![
            run("flush left line", 72.0, 700.0, 110.0),
            run("indented opener", 100.0, 686.0, 110.0),
        ];
        let lines = lines_of(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let paras = split_paragraphs(&refs, 12.0);
        assert_eq!(paras.len(), 2, "first-line indent breaks paragraph");
    }

    #[test]
    fn centered_lines_yield_center_align() {
        // A two-line centered block: a wide line sets the block margins, and a
        // narrower line inset symmetrically inside it reads as centred. (Single
        // lines have no margin reference and stay Left by design.)
        let runs = vec![
            run("a wide centered heading line", 100.0, 700.0, 300.0), // x 100..400
            run("shorter centered line", 175.0, 686.0, 150.0), // x 175..325, inset ~75 each side
        ];
        let lines = lines_of(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let mut ids = IdGen::default();
        let block = build_paragraph(&refs, &mut ids, Rect::new);
        let BlockKind::Paragraph(p) = block.kind else {
            panic!("expected paragraph");
        };
        assert_eq!(p.style.align, Align::Center);
    }

    #[test]
    fn full_width_inner_lines_yield_justify_align() {
        // A justified block: the inner lines all reach both margins (left x=72,
        // right x=372), and only the last line is short (ends well inside the
        // right margin). With ≥3 lines and every non-last line flush both sides,
        // the alignment reads as Justify.
        let runs = vec![
            run("inner line one stretched full", 72.0, 700.0, 300.0), // 72..372
            run("inner line two stretched full", 72.0, 686.0, 300.0), // 72..372
            run("inner line three stretch full", 72.0, 672.0, 300.0), // 72..372
            run("last short tail", 72.0, 658.0, 120.0),               // 72..192 (ragged right)
        ];
        let lines = lines_of(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let mut ids = IdGen::default();
        let block = build_paragraph(&refs, &mut ids, Rect::new);
        let BlockKind::Paragraph(p) = block.kind else {
            panic!("expected paragraph");
        };
        assert_eq!(p.style.align, Align::Justify);
    }

    #[test]
    fn paragraph_block_carries_runs_and_linebreaks() {
        let runs = vec![
            run("alpha", 72.0, 700.0, 60.0),
            run("beta", 72.0, 686.0, 60.0),
        ];
        let lines = lines_of(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let mut ids = IdGen::default();
        let block = build_paragraph(&refs, &mut ids, Rect::new);
        let BlockKind::Paragraph(p) = block.kind else {
            panic!("expected paragraph");
        };
        // alpha, LineBreak, beta
        assert_eq!(p.runs.len(), 3);
        assert!(matches!(p.runs[1], Inline::LineBreak));
    }

    // ── #10 super/sub recovery ───────────────────────────────────────────────

    /// A run with the full geometry the heuristic reads (size + raised/lowered
    /// baseline). `sized(text, x, y, w, size)`.
    fn sized(text: &str, x: f64, y: f64, w: f64, size: f64) -> ReconRun {
        ReconRun {
            text: text.to_string(),
            x,
            y,
            w,
            h: size,
            size,
            style: TextStyle::default(),
            rotation: 0.0,
            source_index: None,
            underline: false,
            strike: false,
        }
    }

    /// The first inline run's [`VAlign`], reading through any `Link` wrapper.
    fn nth_run_valign(p: &Paragraph, n: usize) -> Option<crate::model::VAlign> {
        p.runs
            .iter()
            .filter_map(|i| match i {
                Inline::Run(r) => Some(r.style.vertical_align),
                _ => None,
            })
            .nth(n)
    }

    #[test]
    fn a_small_raised_run_is_superscript() {
        // Two runs on one baseline band: 12pt body at y=700, then a 7pt run
        // raised ~4pt (y=704) — "x²"-style. The small+raised run reads as Super,
        // the body run stays Baseline. They share a line (centres within band).
        let body = sized("E = mc", 72.0, 700.0, 60.0, 12.0);
        let sup = sized("2", 132.0, 704.0, 6.0, 7.0); // 7/12 ≈ 0.58 ≤ 0.75, +4 > 0.2·12
        let lines = lines_of(&[body, sup]);
        // The two runs must share a single line for the per-line baseline to apply.
        assert_eq!(lines.len(), 1, "body + superscript group into one line");
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let ctx = ParaContext {
            body: 12.0,
            ..ParaContext::default()
        };
        let mut ids = IdGen::default();
        let block = build_paragraph_styled(&refs, &ctx, &mut ids, Rect::new);
        let BlockKind::Paragraph(p) = block.kind else {
            panic!("expected paragraph");
        };
        assert_eq!(
            nth_run_valign(&p, 0),
            Some(crate::model::VAlign::Baseline),
            "the body run stays on the baseline"
        );
        assert_eq!(
            nth_run_valign(&p, 1),
            Some(crate::model::VAlign::Super),
            "the small raised run is a superscript"
        );
    }

    #[test]
    fn a_small_lowered_run_is_subscript() {
        // "H₂O": the 7pt "2" sits ~3pt below the 12pt body baseline.
        let h = sized("H", 72.0, 700.0, 10.0, 12.0);
        let two = sized("2", 82.0, 696.5, 6.0, 7.0); // -3.5 < -0.2·12
        let o = sized("O", 88.0, 700.0, 10.0, 12.0);
        let lines = lines_of(&[h, two, o]);
        assert_eq!(lines.len(), 1, "subscript groups with its body run");
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let ctx = ParaContext {
            body: 12.0,
            ..ParaContext::default()
        };
        let mut ids = IdGen::default();
        let block = build_paragraph_styled(&refs, &ctx, &mut ids, Rect::new);
        let BlockKind::Paragraph(p) = block.kind else {
            panic!("expected paragraph");
        };
        // Order left→right: H (base), 2 (sub), O (base).
        assert_eq!(nth_run_valign(&p, 0), Some(crate::model::VAlign::Baseline));
        assert_eq!(
            nth_run_valign(&p, 1),
            Some(crate::model::VAlign::Sub),
            "the small lowered run is a subscript"
        );
        assert_eq!(nth_run_valign(&p, 2), Some(crate::model::VAlign::Baseline));
    }

    #[test]
    fn same_size_baseline_jitter_is_not_super_sub() {
        // A full-size run a couple of points off baseline must NOT be tagged:
        // super/sub requires the run to be distinctly *smaller* than the body.
        let a = sized("normal", 72.0, 700.0, 60.0, 12.0);
        let b = sized("text", 134.0, 702.0, 30.0, 12.0); // same size, +2pt
        let lines = lines_of(&[a, b]);
        assert_eq!(lines.len(), 1);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let ctx = ParaContext {
            body: 12.0,
            ..ParaContext::default()
        };
        let mut ids = IdGen::default();
        let block = build_paragraph_styled(&refs, &ctx, &mut ids, Rect::new);
        let BlockKind::Paragraph(p) = block.kind else {
            panic!("expected paragraph");
        };
        // After coalescing, two same-style runs merge into one — verify the
        // merged run is baseline (not super/sub) and carries both texts.
        assert_eq!(
            nth_run_valign(&p, 0),
            Some(crate::model::VAlign::Baseline),
            "merged same-size run is not a super/subscript"
        );
        assert!(p.runs.iter().any(|r| match r {
            crate::model::Inline::Run(ir) => ir.text.contains("normal") && ir.text.contains("text"),
            _ => false,
        }), "coalesced run carries both texts");
    }

    // ── #2 paragraph spacing / indents ───────────────────────────────────────

    #[test]
    fn indented_double_spaced_paragraph_populates_style() {
        // A 3-line paragraph at ~24pt leading (2× a 12pt body), its first line
        // indented +30pt, and the block left 100 (page left 72 → 28pt left
        // indent). `build_paragraph_styled` must recover all of these.
        let runs = vec![
            run("indented opening line here", 130.0, 700.0, 200.0), // first line indented (left 130)
            run("second line flush at block", 100.0, 676.0, 200.0), // body left = 100
            run("third line also flush left", 100.0, 652.0, 200.0),
        ];
        let lines = lines_of(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let body = 12.0;
        let leading = group_leading(&refs, body, 0.0); // ≈ 24
        let group_left = group_left(&refs); // = 100
        let ctx = ParaContext {
            body,
            leading,
            group_left,
            page_left: 72.0,
            prev_bottom: None,
            links: &[],
        };
        let mut ids = IdGen::default();
        let block = build_paragraph_styled(&refs, &ctx, &mut ids, Rect::new);
        let BlockKind::Paragraph(p) = block.kind else {
            panic!("expected paragraph");
        };
        let st = &p.style;
        // Line height ≈ 24/12 = 2.0× (well above the 1.0× Normal threshold).
        match st.line_height {
            LineHeight::Multiple(m) => {
                assert!((m - 2.0).abs() < 0.1, "≈2.0× leading, got {m}")
            }
            other => panic!("expected Multiple leading, got {other:?}"),
        }
        // First-line indent ≈ 30pt (130 − 100), well over the (12·0.6)=7.2 floor.
        assert!(
            (st.first_line_pt - 30.0).abs() < 1.0,
            "first-line indent ≈ 30, got {}",
            st.first_line_pt
        );
        // Left indent = block left (100) − page left (72) = 28pt.
        assert!(
            (st.indent_left_pt - 28.0).abs() < 1.0,
            "left indent ≈ 28, got {}",
            st.indent_left_pt
        );
    }

    #[test]
    fn gap_before_a_paragraph_recovers_space_before() {
        // A single body paragraph whose top sits 40pt below the previous
        // paragraph's bottom, at a 14pt body leading. The extra gap beyond one
        // line step is recovered as space_before.
        let runs = vec![run("a fresh paragraph body", 72.0, 600.0, 200.0)];
        let lines = lines_of(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        // Previous paragraph bottom 40pt above this para's top (top = 600+12=612).
        let ctx = ParaContext {
            body: 12.0,
            leading: 14.0,
            group_left: 72.0,
            page_left: 72.0,
            prev_bottom: Some(612.0 + 40.0),
            links: &[],
        };
        let mut ids = IdGen::default();
        let block = build_paragraph_styled(&refs, &ctx, &mut ids, Rect::new);
        let BlockKind::Paragraph(p) = block.kind else {
            panic!("expected paragraph");
        };
        // gap = prev_bottom - top - (leading-body) = 652 - 612 - 2 = 38.
        assert!(
            (p.style.space_before_pt - 38.0).abs() < 1.0,
            "space_before ≈ 38, got {}",
            p.style.space_before_pt
        );
    }

    #[test]
    fn plain_single_spaced_paragraph_leaves_style_quiet() {
        // A tight, flush-left, single-spaced paragraph at the page margin and no
        // preceding paragraph: every recovered field stays at its quiet default
        // (no spurious 1.0× leading, no phantom indents).
        let runs = vec![
            run("flush left line one", 72.0, 700.0, 150.0),
            run("flush left line two", 72.0, 688.0, 150.0), // ~12pt leading = 1.0× body
        ];
        let lines = lines_of(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let body = 12.0;
        let ctx = ParaContext {
            body,
            leading: group_leading(&refs, body, 0.0),
            group_left: group_left(&refs),
            page_left: 72.0,
            prev_bottom: None,
            links: &[],
        };
        let mut ids = IdGen::default();
        let block = build_paragraph_styled(&refs, &ctx, &mut ids, Rect::new);
        let BlockKind::Paragraph(p) = block.kind else {
            panic!("expected paragraph");
        };
        let st = &p.style;
        assert_eq!(st.line_height, LineHeight::Normal, "1.0× leading ⇒ Normal");
        assert_eq!(st.first_line_pt, 0.0, "no first-line indent");
        assert_eq!(st.indent_left_pt, 0.0, "at the page margin");
        assert_eq!(st.space_before_pt, 0.0, "no preceding paragraph");
    }

    // ── #1 link wrapping (run inside a link rect) ────────────────────────────

    #[test]
    fn a_run_under_a_link_rect_is_wrapped_in_link() {
        use crate::model::LinkTarget;
        // One body line; a link rect covers the whole run's box (PDF user space).
        let runs = vec![run("click me", 72.0, 700.0, 80.0)]; // box x 72..152, y 700..712
        let lines = lines_of(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let links = [ParaLink {
            target: LinkTarget::Url("https://example.com".into()),
            rect: [70.0, 698.0, 156.0, 714.0],
        }];
        let ctx = ParaContext {
            body: 12.0,
            links: &links,
            ..ParaContext::default()
        };
        let mut ids = IdGen::default();
        let block = build_paragraph_styled(&refs, &ctx, &mut ids, Rect::new);
        let BlockKind::Paragraph(p) = block.kind else {
            panic!("expected paragraph");
        };
        let link = p.runs.iter().find_map(|i| match i {
            Inline::Link { href, children } => Some((href.clone(), children.clone())),
            _ => None,
        });
        let (href, children) = link.expect("the covered run is wrapped in a Link");
        assert_eq!(href, LinkTarget::Url("https://example.com".into()));
        assert!(
            matches!(children.first(), Some(Inline::Run(r)) if r.text.trim() == "click me"),
            "the link wraps the run it covers"
        );
    }

    // ── gap-aware spacing inside a paragraph (split-word multi-font runs) ─────

    /// Flatten a paragraph's inline runs (incl. link children) into one string.
    fn flatten(p: &Paragraph) -> String {
        fn walk(inls: &[Inline], out: &mut String) {
            for i in inls {
                match i {
                    Inline::Run(r) => out.push_str(&r.text),
                    Inline::LineBreak => out.push(' '),
                    Inline::Link { children, .. } => walk(children, out),
                    Inline::Image(_) => {}
                    Inline::CommentRef { .. } => {}
                }
            }
        }
        let mut s = String::new();
        walk(&p.runs, &mut s);
        s
    }

    #[test]
    fn split_word_runs_get_no_spurious_space_but_real_gaps_keep_theirs() {
        // A dense form splits one word across embedded fonts: "relatif" arrives as
        // "rel"+"at"+"if" with each piece butting the previous (gap ≈ 0). A clear
        // gap separates "relatif" from "au". Expect "relatif au", never
        // "rel at if au" (and never "relatifau").
        let rel = run("rel", 72.0, 700.0, 18.0); // x 72..90
        let at = run("at", 90.0, 700.0, 12.0); // butts → join
        let iff = run("if", 102.0, 700.0, 12.0); // butts → join → "relatif"
        let au = run("au", 130.0, 700.0, 12.0); // gap 130-114 = 16 → space
        let lines = lines_of(&[rel, at, iff, au]);
        assert_eq!(lines.len(), 1, "one baseline band → one line");
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let ctx = ParaContext {
            body: 12.0,
            ..ParaContext::default()
        };
        let mut ids = IdGen::default();
        let block = build_paragraph_styled(&refs, &ctx, &mut ids, Rect::new);
        let BlockKind::Paragraph(p) = block.kind else {
            panic!("expected paragraph");
        };
        assert_eq!(flatten(&p), "relatif au");
    }

    #[test]
    fn a_run_outside_every_link_rect_is_not_wrapped() {
        use crate::model::LinkTarget;
        let runs = vec![run("plain text", 72.0, 700.0, 80.0)];
        let lines = lines_of(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        // A link rect far from the run (200pt below) must not claim it.
        let links = [ParaLink {
            target: LinkTarget::Url("https://example.com".into()),
            rect: [72.0, 498.0, 152.0, 512.0],
        }];
        let ctx = ParaContext {
            body: 12.0,
            links: &links,
            ..ParaContext::default()
        };
        let mut ids = IdGen::default();
        let block = build_paragraph_styled(&refs, &ctx, &mut ids, Rect::new);
        let BlockKind::Paragraph(p) = block.kind else {
            panic!("expected paragraph");
        };
        assert!(
            !p.runs.iter().any(|i| matches!(i, Inline::Link { .. })),
            "a run outside every rect stays a plain Run"
        );
    }
}
