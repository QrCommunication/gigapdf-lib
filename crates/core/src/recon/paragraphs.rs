//! Stage 3 — **paragraph assembly**. Merge a column's lines into paragraphs.
//! Within one paragraph the lines are stacked at a regular leading; a new
//! paragraph begins when:
//!   * the inter-line gap exceeds `1.5 × leading` (a blank-line break), or
//!   * a line is indented like a first line (its left jumps right of the
//!     **previous line's** left by more than a small threshold — judged
//!     line-to-line so an entirely indented span, a quote block, stays one
//!     paragraph instead of re-splitting on every line), or
//!   * the alignment visibly changes (a centred line after a flush-left block).
//!
//! [`ParagraphStyle::align`] is derived from how the lines' starts and ends vary
//! against the block's left/right margins (centred ⇒ both insets symmetric;
//! right ⇒ ends align, starts ragged; justify ⇒ both edges flush on the inner
//! lines).

use super::lines::ReconLine;
use super::{median, run_char_style, IdGen, ParaLink, ReconRun};
use crate::model::{
    Align, Block, BlockKind, Inline, InlineRun, LineHeight, ParaBorder, Paragraph, ParagraphStyle,
    Rect, VAlign,
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
    /// The prose **group's** right margin (max line-right across the whole
    /// group) — with [`group_left`](Self::group_left) it is the body measure
    /// [`line_break_softness`] judges word-wrap against (a deliberately short
    /// block must not self-satisfy the ratio through its own widest line).
    /// `0` (the default) ⇒ unknown: the paragraph's own box stands in.
    pub group_right: f64,
    /// The page's MediaBox left edge (points): `indent_left_pt` is the block's
    /// left margin relative to this, so a host re-lays the paragraph in the same
    /// horizontal position.
    pub page_left: f64,
    /// Bottom edge (PDF user-space `y`, lower = further down) of the previous
    /// emitted paragraph in this group, or `None` for the first. Drives
    /// `space_before_pt` from the inter-paragraph gap.
    pub prev_bottom: Option<f64>,
    /// Top edge (PDF user-space `y`, higher = further up) of the **next**
    /// paragraph in this group, or `None` for the last. Drives `space_after_pt`
    /// from the gap to the following block, symmetric to `prev_bottom`.
    pub next_top: Option<f64>,
    /// Hyperlink rectangles (already mapped to the model target), in PDF user
    /// space. A run whose box overlaps a rect is wrapped in [`Inline::Link`].
    pub links: &'a [ParaLink],
}

/// Split a group of reading-order lines into paragraphs (each a `Vec` of line
/// references). `body` is the document body font size and `doc_leading` the
/// page-wide median line step (see [`document_leading`]; pass `0.0` when
/// unknown — the reference then falls back to `body`).
///
/// **Geometric coherence** (the v1.19.2 hole, lib-side): the historic rule
/// judged the gap against the *group's own* leading only — a two-line group's
/// single step *is* its leading, so `step > 1.5 × step` could never split and a
/// footer line 250 pt away fused with a header line into one "paragraph". The
/// split now triggers when any of these hold between consecutive lines:
///
/// 1. `step > 1.5 × reference` — the reference is the group's **own regular
///    pace** (the median of its gaps) when the group is metronomic (≥ 3 lines,
///    every gap within ±20 % of their median: a deliberately loose-set
///    paragraph on a dense page keeps its lines), and otherwise
///    `min(group_leading, max(doc_leading, body))` — irregular statistics can
///    only *tighten* the document reference, never inflate it past what the
///    page actually uses;
/// 2. `step > 2.5 × line font size` — an absolute ceiling: no paragraph stacks
///    lines further apart than 2.5× their own glyph size (this keeps splitting
///    a "regular" far-apart lattice — footer/header never fuse, whatever the
///    pace says);
/// 3. the lines' horizontal spans overlap by **less than 20 %** of the narrower
///    one — vertically adjacent but side-by-side content (columns, disjoint
///    form fields) is never one paragraph.
///
/// The **indent** break is judged against the *previous line*, not the group's
/// min-left: a sudden rightward jump of the line start is an indented opener
/// (new paragraph), while an entirely indented span (a quote block) or a
/// centred paragraph no longer re-splits on every line. A return to flush
/// (negative jump) is a boundary too — except straight after a lone indented
/// opener, the classic first-line-indent pattern (opener indented, continuation
/// flush = the SAME paragraph; `paragraph_style` reads that very shape back as
/// `first_line_pt`). The mirror shape — a flush **lone** opener whose following
/// lines all continue at one deeper column (a bibliography entry, a definition)
/// — is a **hanging indent** and stays one paragraph too, confirmed by
/// look-ahead so a genuine indented opener still splits (`paragraph_style`
/// reads the hanging shape back as a *negative* `first_line_pt`).
///
/// Output invariant: within a returned paragraph every line is within the
/// ceiling of its neighbour and overlaps it horizontally; the block frame
/// (union of its lines, computed by the builder) therefore hugs real prose.
pub fn split_paragraphs<'a>(
    lines: &[&'a ReconLine],
    body: f64,
    doc_leading: f64,
) -> Vec<Vec<&'a ReconLine>> {
    if lines.is_empty() {
        return Vec::new();
    }
    // The document reference: the page-wide leading when known, else the body
    // size. A metronomic group (≥ 3 lines, all gaps within ±20 % of their
    // median) is judged on its OWN pace, unclamped — a legitimate 16 pt-leaded
    // paragraph on a 10 pt-dense page must not shatter line by line (the
    // ceiling and disjoint guards below still split far-apart/side-by-side
    // content). An irregular group's own median step may only *lower* the
    // reference (a tight-set group splits on smaller gaps); a group of 2 lines
    // has no internal statistics at all — its single step IS
    // `estimate_leading`'s median — so its own figure is ignored and the
    // document reference alone judges the gap.
    let doc_ref = doc_leading.max(body).max(1.0);
    let reference = if lines.len() >= 3 {
        match regular_step(lines) {
            Some(step) => step,
            None => estimate_leading(lines, body, doc_leading).min(doc_ref),
        }
    } else {
        doc_ref
    };
    let body_left = block_left(lines);
    let indent_threshold = (body * 0.6).max(6.0);

    let mut paras: Vec<Vec<&ReconLine>> = vec![vec![lines[0]]];
    for i in 1..lines.len() {
        let (prev, cur) = (lines[i - 1], lines[i]);
        // Vertical gap between baselines (top of cur minus top of prev, going
        // down the page → prev.top - cur.top is the step).
        let step = (prev.center_y() - cur.center_y()).abs();
        let big_gap = step > reference * 1.5;
        // Absolute ceiling: no paragraph stacks lines further apart than 2.5×
        // their own glyph size, whatever the (possibly inflated) leading says.
        let ceiling = step > 2.5 * line_font_size(prev).max(line_font_size(cur));
        // Side-by-side content: consecutive prose lines of one paragraph share
        // their horizontal band; less than 20 % overlap (of the narrower line)
        // means columns / disjoint fields, never one flow.
        let disjoint = !lines_overlap_x(prev, cur);
        // Indent break, judged against the PREVIOUS line (see the doc above):
        // a sudden rightward jump opens an indented paragraph; a return to
        // flush closes an indented span — unless the indented "span" is a lone
        // opener (the classic first-line indent, which the continuation
        // belongs to).
        let left_delta = cur.left() - prev.left();
        let indent = if left_delta > indent_threshold {
            // A rightward jump normally opens an indented paragraph — EXCEPT
            // the **hanging-indent** shape (bibliographies, definition lists):
            // a flush LONE opener whose following lines all continue at one
            // deeper column. Judged by look-ahead: the next line must confirm
            // the continuation column at a normal step, so a genuine indented
            // opener (whose next line returns flush, or which stands alone)
            // still splits exactly as before.
            let lone_opener = paras.last().is_some_and(|p| p.len() == 1);
            let modest_hang = left_delta <= body * 6.0;
            let confirmed = lines.get(i + 1).is_some_and(|next| {
                (next.left() - cur.left()).abs() <= indent_threshold
                    && (cur.center_y() - next.center_y()).abs() <= reference * 1.5
                    && lines_overlap_x(cur, next)
            });
            !(lone_opener && modest_hang && confirmed)
        } else if left_delta < -indent_threshold {
            paras.last().is_some_and(|p| p.len() > 1)
        } else {
            false
        };
        // Alignment change: a centred line breaking a flush-left run (or back).
        let align_break = align_of_line(prev, body_left, block_right(lines))
            != align_of_line(cur, body_left, block_right(lines))
            && (is_centered(cur, body_left, block_right(lines))
                || is_centered(prev, body_left, block_right(lines)));

        // Font-size tier crossing: a heading-sized line abutting a body-sized one
        // (a heading and the paragraph it introduces) is two blocks even when the
        // gap is regular — otherwise the whole thing coalesces into one block that
        // heading promotion then stamps as a single heading, gluing the body onto
        // the title. Mirrors [`headings::is_heading_size`] so the boundary lands
        // exactly where a heading is later detected. Size only — never bold — so a
        // bold phrase inside a body paragraph cannot split it line by line.
        let size_tier_break = crate::recon::headings::is_heading_size(line_font_size(prev), body)
            != crate::recon::headings::is_heading_size(line_font_size(cur), body);

        if big_gap || ceiling || disjoint || indent || align_break || size_tier_break {
            paras.push(vec![cur]);
        } else {
            paras.last_mut().unwrap().push(cur);
        }
    }
    paras
}

/// The group's own **regular pace**: `Some(median gap)` when the group has at
/// least two inter-line gaps and every gap sits within ±20 % of their median —
/// a metronomic stack, the strongest signal that the leading is intentional,
/// however loose it looks against the page median. `None` when the pace is
/// irregular (or the group too short to have two gaps): the caller then falls
/// back to the document-clamped reference. The absolute `ceiling` and
/// `disjoint` guards in [`split_paragraphs`] stay active regardless, so a
/// "regular" far-apart lattice (stacked footers, form rows) still splits.
fn regular_step(lines: &[&ReconLine]) -> Option<f64> {
    let gaps: Vec<f64> = lines
        .windows(2)
        .map(|w| (w[0].center_y() - w[1].center_y()).abs())
        .collect();
    if gaps.len() < 2 {
        return None;
    }
    let mut sorted = gaps.clone();
    let med = median(&mut sorted, 0.0);
    if med <= 0.0 {
        return None;
    }
    gaps.iter()
        .all(|g| (g - med).abs() <= med * 0.20)
        .then_some(med.max(1.0))
}

/// A line's representative font size: the median of its runs' sizes, falling
/// back to the line height (≈ glyph extent) when it has no runs.
fn line_font_size(line: &ReconLine) -> f64 {
    let mut sizes: Vec<f64> = line.runs.iter().map(|r| r.size.max(1.0)).collect();
    median(&mut sizes, line.h.max(1.0)).max(1.0)
}

/// Whether two lines' horizontal spans overlap by at least 20 % of the
/// narrower one. Degenerate (zero-width) lines count as overlapping so a lone
/// mark never shatters its paragraph.
fn lines_overlap_x(a: &ReconLine, b: &ReconLine) -> bool {
    let overlap = a.right().min(b.right()) - a.left().max(b.left());
    let narrow = a.w.min(b.w);
    if narrow <= 0.0 {
        return true;
    }
    overlap >= narrow * 0.20
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
///   opener is indented to the right of the rest of the group; **negative**
///   for the hanging-indent shape (flush opener, indented continuations), with
///   `indent_left_pt` then anchored to the continuation column (the DOCX
///   convention).
/// - `indent_left_pt` = the block's left margin minus the page's left edge.
/// - `space_before_pt` = the gap from the previous paragraph's bottom to this
///   paragraph's top, beyond one line of leading (the normal single step).
/// - `space_after_pt` = the mirror gap down to the next paragraph's top
///   ([`ParaContext::next_top`]), beyond one line of leading.
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
                source_index: r.source_index.map(|i| vec![i]),
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
    let mut style = paragraph_style(para, ctx, align, left, right, y, h);
    // Wrap-vs-deliberate classification of each emitted `LineBreak`, judged
    // against the enclosing group's measure when the caller supplied one
    // (`ParaContext::group_left..group_right`); a default context falls back
    // to the paragraph's own box. Additive: the breaks themselves and the
    // runs/`source_index` seam are untouched.
    let (m_left, m_right) = if ctx.group_right > ctx.group_left {
        (ctx.group_left, ctx.group_right)
    } else {
        (left, right)
    };
    style.soft_breaks = line_break_softness(para, style.align, m_left, m_right);
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

    // First-line indent (positive) or hanging indent (negative). The positive
    // case: the opener sits to the right of the group's body left (the same
    // jump that triggered a paragraph split), measured against the group left,
    // not this block's own left (which equals the indented opener). The
    // **hanging** case (bibliographies, definition lists): the opener sits
    // flush and every continuation line agrees on one deeper column — encoded
    // the DOCX way: `indent_left` is the continuation column and a *negative*
    // `first_line` pulls the opener back out of it.
    let opener_left = para.first().map(|l| l.left()).unwrap_or(left);
    let threshold = (body * 0.6).max(6.0);
    let hang = hanging_indent(para, opener_left, threshold);
    let (first_line_pt, block_left) = match hang {
        Some(cont_left) => (round2(-(cont_left - opener_left)), cont_left),
        None => {
            let indent = opener_left - ctx.group_left;
            let first = if indent > threshold {
                round2(indent)
            } else {
                0.0
            };
            (first, left)
        }
    };

    // Left indent: the block's left margin relative to the page's left edge —
    // for a hanging paragraph that margin is the continuation column (the
    // opener juts out of it by `first_line_pt`).
    let indent_left_pt = {
        let indent = block_left - ctx.page_left;
        if indent > 1.0 {
            round2(indent)
        } else {
            0.0
        }
    };

    // A hanging paragraph's continuation lines all sit inset from the block
    // left (set by the jutting opener), which the per-line classifier misreads
    // as a right-aligned block. The flush-left opener disproves that reading.
    let align = if hang.is_some() && align == Align::Right {
        Align::Left
    } else {
        align
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

    // Space-after: the mirror figure, from this paragraph's bottom to the next
    // paragraph's top (`ctx.next_top`). Note the recovered geometry is reported
    // on BOTH sides of a gap (the next paragraph sees the same gap as its
    // `space_before_pt`) — attribution is the exporters' concern.
    let space_after_pt = match ctx.next_top {
        Some(next_top) if ctx.leading > 0.0 => {
            let gap = y - next_top - (ctx.leading - body).max(0.0);
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
        space_after_pt,
        indent_left_pt,
        indent_right_pt: 0.0,
        first_line_pt,
        line_height,
        direction: para_direction(para),
        ..Default::default()
    }
}

/// Unicode strong-script heuristic for the paragraph's base direction: `Rtl`
/// when **more than 60 %** of the paragraph's strong (letter) characters sit in
/// the right-to-left blocks — Hebrew `U+0590..=U+05FF`, Arabic
/// `U+0600..=U+06FF` + Supplement `U+0750..=U+077F` + Presentation Forms
/// `U+FB50..=U+FDFF` / `U+FE70..=U+FEFF`. Digits, punctuation and whitespace
/// are direction-neutral and never counted, so a Latin document (or a mostly
/// Latin paragraph quoting a few RTL words) stays `None` (LTR).
fn para_direction(para: &[&ReconLine]) -> Option<crate::model::Direction> {
    let (mut rtl, mut strong) = (0usize, 0usize);
    for line in para {
        for run in &line.runs {
            for c in run.text.chars() {
                if is_strong_rtl(c) {
                    rtl += 1;
                    strong += 1;
                } else if c.is_alphabetic() {
                    strong += 1;
                }
            }
        }
    }
    (strong > 0 && rtl * 100 > strong * 60).then_some(crate::model::Direction::Rtl)
}

/// Whether `c` belongs to the strong right-to-left Unicode blocks (Hebrew +
/// Arabic incl. supplement and presentation forms). See [`para_direction`].
fn is_strong_rtl(c: char) -> bool {
    matches!(
        c as u32,
        0x0590..=0x05FF | 0x0600..=0x06FF | 0x0750..=0x077F | 0xFB50..=0xFDFF | 0xFE70..=0xFEFF
    )
}

/// Detect the **hanging-indent** shape of a multi-line paragraph: the opener
/// sits at the block's left edge and every continuation line agrees on one
/// column deeper than `threshold` to its right. Returns that continuation
/// column, or `None` when the shape doesn't hold (a straggler line, a positive
/// first-line indent, a single line). Conservative by construction — any
/// disagreement between continuation lines vetoes the reading.
fn hanging_indent(para: &[&ReconLine], opener_left: f64, threshold: f64) -> Option<f64> {
    if para.len() < 2 {
        return None;
    }
    let conts = &para[1..];
    let cont_left = conts.iter().map(|l| l.left()).fold(f64::INFINITY, f64::min);
    if cont_left - opener_left <= threshold {
        return None;
    }
    conts
        .iter()
        .all(|l| (l.left() - cont_left).abs() <= threshold)
        .then_some(cont_left)
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
///
/// The merged run keeps **every** constituent's source ordinal (its
/// `source_index` vec is the concatenation, in content order) — the first entry
/// remains the legacy single-index view. Losing the tail indices would make a
/// coalesced span address only its first PDF run (edits/removals of the rest
/// would silently miss).
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
                        (same && prev.source_index.is_none() == run.source_index.is_none())
                            .then_some(prev)
                    } else {
                        None
                    }
                });
                match merge {
                    Some(prev) => {
                        prev.text.push_str(&run.text);
                        // Merge only happens when both sides are sourced or both
                        // unsourced (guard above) — append the newcomer's
                        // ordinals so the span addresses all its PDF runs.
                        if let (Some(pv), Some(cv)) = (&mut prev.source_index, run.source_index) {
                            pv.extend(cv);
                        }
                    }
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

/// The group's right margin (max line-right) — exposed for
/// [`ParaContext::group_right`] so [`line_break_softness`] judges wrap against
/// the whole group's measure, not one paragraph's own box.
pub(crate) fn group_right(lines: &[&ReconLine]) -> f64 {
    block_right(lines)
}

/// A paragraph's union-box bottom edge (PDF user-space `y`, lower = further down
/// the page) — fed to the next paragraph's [`ParaContext::prev_bottom`] to
/// recover inter-paragraph `space_before`.
pub(crate) fn paragraph_bottom(para: &[&ReconLine]) -> f64 {
    let (_, y, _, _) = union_box(para);
    y
}

/// A paragraph's union-box **top** edge (PDF user-space `y + h`) — fed to the
/// previous paragraph's [`ParaContext::next_top`] to recover inter-paragraph
/// `space_after` (the mirror of [`paragraph_bottom`]).
pub(crate) fn paragraph_top(para: &[&ReconLine]) -> f64 {
    let (_, y, _, h) = union_box(para);
    y + h
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

// ── line-break softness (wrap vs deliberate break) ──────────────────────────

/// Classify each of a paragraph's line breaks as **soft** (`true` — the break
/// is a consequence of word-wrap, so an editor/exporter may re-flow across it)
/// or **hard** (`false` — the line ends short on purpose: a poem verse, an
/// address line, a heading fragment).
///
/// Entry `k` describes the break between line `k` and line `k + 1`; the vector
/// therefore has `para.len() - 1` entries (empty for a single-line paragraph).
/// A break is soft when the line it closes ran (nearly) the full body width —
/// its right edge reaches ≥ 85 % of the measure — or when the whole paragraph
/// is justified (every inner line is stretched flush by definition, so its
/// physical right edge *is* the wrap signal). `left`/`right` are the **body
/// measure** to judge wrap against — the enclosing column/prose-group margins,
/// not the paragraph's own box (a deliberately short block, an address, would
/// otherwise self-satisfy the ratio through its own widest line).
///
/// This is the reliable-signal producer for the reflowable-export work: the
/// emitted [`Inline::LineBreak`]s are **unchanged** (the editor seam depends on
/// the exact lines/runs structure), and the flags are not yet persisted on the
/// model — every candidate carrier (`ParagraphStyle`, `Paragraph`) is built
/// exhaustively by the Office importers, so the additive field lands together
/// with those call sites in a later wave.
pub fn line_break_softness(
    para: &[&ReconLine],
    align: Align,
    left: f64,
    right: f64,
) -> Vec<bool> {
    if para.len() < 2 {
        return Vec::new();
    }
    let measure = right - left;
    para[..para.len() - 1]
        .iter()
        .map(|line| {
            if align == Align::Justify {
                return true;
            }
            measure > 0.0 && line.right() - left >= measure * 0.85
        })
        .collect()
}

// ── paragraph borders & background (drawn box decorations) ──────────────────

/// One painted page path, pre-classified for paragraph border/background
/// correlation. All coordinates are **PDF user space**. The caller filters the
/// page's vector paths first — table grid paths and consumed underline/strike
/// rules never become decoration candidates (no double-claiming).
#[derive(Debug, Clone, Copy)]
pub(crate) struct PageDecor {
    /// The source `VectorPath::index`, so a claimed decoration can be consumed
    /// (not re-emitted as a standalone [`crate::model::Shape`] block).
    pub index: usize,
    pub shape: DecorShape,
}

/// The geometry of a decoration candidate: an axis-aligned rectangle (filled
/// and/or stroked), or a thin ruling line (one border side).
#[derive(Debug, Clone, Copy)]
pub(crate) enum DecorShape {
    /// An axis-aligned rectangle path (`x0 < x1`, `y0 < y1`).
    Box {
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
        fill: Option<[f64; 3]>,
        stroke: Option<[f64; 3]>,
        stroke_width: f64,
    },
    /// A thin horizontal ruling line at `y`, spanning `x0..x1`.
    HRule {
        y: f64,
        x0: f64,
        x1: f64,
        color: [f64; 3],
        width: f64,
    },
    /// A thin vertical ruling line at `x`, spanning `y0..y1`.
    VRule {
        x: f64,
        y0: f64,
        y1: f64,
        color: [f64; 3],
        width: f64,
    },
}

/// Recovered box decorations of one paragraph: its shading, its per-side
/// borders (`[top, right, bottom, left]`, all set or none — a partial surround
/// is never claimed), and the path indices consumed by the claim.
pub(crate) struct ParaDecorations {
    pub background: Option<[f64; 3]>,
    pub borders: Option<[Option<ParaBorder>; 4]>,
    pub claimed: Vec<usize>,
}

/// How far a decoration may extend beyond the paragraph's text box and still
/// count as *its* frame (the padding of an encadré). Anything looser belongs
/// to a larger region (a page background, a box around several paragraphs) and
/// is left alone — the conservative stance: at the slightest doubt, claim
/// nothing.
fn decor_hug_margin(body: f64) -> f64 {
    (body * 1.5).max(12.0)
}

/// Whether a fill colour is (near-)white — a white rectangle under text is a
/// masking artifact, not shading, and must not become `background`.
fn near_white(c: [f64; 3]) -> bool {
    c.iter().all(|v| *v >= 0.99)
}

/// Correlate a paragraph's lines with the page's drawn boxes/rules (fix: the
/// CERFA encadré). Recovers:
///
/// * **`background`** — a *filled*, non-white rectangle covering ≥ 90 % of the
///   paragraph's box while hugging it (no side extends past the text by more
///   than [`decor_hug_margin`]). The topmost-painted match wins (painter's
///   order — it is the one actually visible behind the text).
/// * **`borders`** — a *stroked* rectangle hugging all four sides, or four
///   individual ruling lines (top/bottom spanning ≥ 85 % of the width within
///   the hug band, left/right the same for the height). All four sides or
///   nothing: a partial rule set stays ordinary shapes.
///
/// Guards, per the conservative brief: a rectangle that extends beyond the
/// paragraph on any side by more than the hug margin (page backgrounds, a box
/// around *several* paragraphs) is never claimed; table-owned paths were
/// filtered out by the caller before this ever runs. `claimed` returns exactly
/// the path indices this paragraph consumed so the page builder can drop them
/// from the standalone-shape pass (the same hygiene as underline/strike rules
/// — a claimed decoration must not draw twice).
pub(crate) fn paragraph_decorations(
    para: &[&ReconLine],
    decors: &[PageDecor],
    body: f64,
) -> ParaDecorations {
    let mut out = ParaDecorations {
        background: None,
        borders: None,
        claimed: Vec::new(),
    };
    if para.is_empty() || decors.is_empty() {
        return out;
    }
    let (px, py, pw, ph) = union_box(para);
    let (px1, py1) = (px + pw, py + ph);
    let area = pw * ph;
    if area <= 0.0 {
        return out;
    }
    let hug = decor_hug_margin(body);
    // A box "hugs" the paragraph when no edge strays past the text box by more
    // than the padding margin (it may sit inside by a hair — glyph boxes
    // overshoot their ink).
    let hugs = |x0: f64, y0: f64, x1: f64, y1: f64| {
        x0 >= px - hug && y0 >= py - hug && x1 <= px1 + hug && y1 <= py1 + hug
    };

    // Background: the topmost-painted (last, painter's order) filled rect that
    // covers ≥ 90 % of the paragraph box and hugs it.
    let mut background: Option<(usize, [f64; 3])> = None;
    // Border box: the topmost-painted stroked rect hugging all four sides.
    let mut border_box: Option<(usize, [f64; 3], f64)> = None;
    for d in decors {
        let DecorShape::Box {
            x0,
            y0,
            x1,
            y1,
            fill,
            stroke,
            stroke_width,
        } = d.shape
        else {
            continue;
        };
        if !hugs(x0, y0, x1, y1) {
            continue;
        }
        let cover_w = (x1.min(px1) - x0.max(px)).max(0.0);
        let cover_h = (y1.min(py1) - y0.max(py)).max(0.0);
        let covers = cover_w * cover_h >= area * 0.90;
        if let Some(f) = fill {
            if covers && !near_white(f) {
                background = Some((d.index, f));
            }
        }
        if let Some(s) = stroke {
            // A border outline must trace the paragraph's frame: every side
            // within the hug band of its edge (covering alone is not enough —
            // a small stroked swatch inside the text must not become a frame).
            let traces = (x0 - px).abs() <= hug
                && (x1 - px1).abs() <= hug
                && (y0 - py).abs() <= hug
                && (y1 - py1).abs() <= hug;
            if traces {
                border_box = Some((d.index, s, stroke_width));
            }
        }
    }
    if let Some((idx, fill)) = background {
        out.background = Some(fill);
        out.claimed.push(idx);
    }
    if let Some((idx, color, width)) = border_box {
        let side = ParaBorder {
            width_pt: width.max(0.5),
            style: "solid".to_string(),
            color,
        };
        out.borders = Some([
            Some(side.clone()),
            Some(side.clone()),
            Some(side.clone()),
            Some(side),
        ]);
        if !out.claimed.contains(&idx) {
            out.claimed.push(idx);
        }
        return out;
    }

    // Four individual rules forming the frame: a top + bottom rule spanning the
    // paragraph's width and a left + right rule spanning its height, each within
    // the hug band of its side. All four or nothing.
    let mut top: Option<(usize, [f64; 3], f64)> = None;
    let mut bottom: Option<(usize, [f64; 3], f64)> = None;
    let mut leftr: Option<(usize, [f64; 3], f64)> = None;
    let mut rightr: Option<(usize, [f64; 3], f64)> = None;
    for d in decors {
        match d.shape {
            DecorShape::HRule {
                y,
                x0,
                x1,
                color,
                width,
            } => {
                // The rule must span the paragraph's width without straying far
                // past either end (a page-wide separator is not this frame).
                let span = (x1.min(px1) - x0.max(px)).max(0.0);
                if span < pw * 0.85 || x0 < px - hug || x1 > px1 + hug {
                    continue;
                }
                if (y - py1).abs() <= hug {
                    top = Some((d.index, color, width));
                } else if (y - py).abs() <= hug {
                    bottom = Some((d.index, color, width));
                }
            }
            DecorShape::VRule {
                x,
                y0,
                y1,
                color,
                width,
            } => {
                let span = (y1.min(py1) - y0.max(py)).max(0.0);
                if span < ph * 0.85 || y0 < py - hug || y1 > py1 + hug {
                    continue;
                }
                if (x - px).abs() <= hug {
                    leftr = Some((d.index, color, width));
                } else if (x - px1).abs() <= hug {
                    rightr = Some((d.index, color, width));
                }
            }
            DecorShape::Box { .. } => {}
        }
    }
    if let (Some(t), Some(b), Some(l), Some(r)) = (top, bottom, leftr, rightr) {
        let side = |(_, color, width): (usize, [f64; 3], f64)| {
            Some(ParaBorder {
                width_pt: width.max(0.5),
                style: "solid".to_string(),
                color,
            })
        };
        out.borders = Some([side(t), side(r), side(b), side(l)]);
        for idx in [t.0, r.0, b.0, l.0] {
            if !out.claimed.contains(&idx) {
                out.claimed.push(idx);
            }
        }
    }
    out
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
        assert!(
            (lead - 16.0).abs() < 1.0,
            "document leading ≈ 16, got {lead}"
        );
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
        assert_eq!(
            group_leading(&refs, body, 18.0),
            18.0,
            "inherits document leading"
        );
        let legacy = group_leading(&refs, body, 0.0);
        assert!(
            (legacy - body * 1.2).abs() < 1e-9,
            "no document leading → 1.2×body, got {legacy}"
        );
        // The fallback is still floored at the body size.
        assert_eq!(
            group_leading(&refs, body, 5.0),
            body,
            "floored at body size"
        );
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
        let paras = split_paragraphs(&refs, 12.0, 14.0);
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
        let paras = split_paragraphs(&refs, 12.0, 14.0);
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
        let paras = split_paragraphs(&refs, 12.0, 14.0);
        assert_eq!(paras.len(), 2, "first-line indent breaks paragraph");
    }

    // ── geometric-coherence hardening (v1.19.2 hole, lib-side) ──────────────

    #[test]
    fn two_far_apart_lines_never_fuse_into_one_paragraph() {
        // The historic hole: a 2-line group's single step IS its own median
        // leading, so `step > 1.5 × own_leading` could never fire and a footer
        // line 250 pt below a header fused into one "paragraph". Judged against
        // the document leading (14 pt here), the 250 pt gap must split.
        let runs = vec![
            run("header line", 72.0, 700.0, 120.0),
            run("footer line", 72.0, 450.0, 120.0), // 250 pt below
        ];
        let lines = lines_of(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let paras = split_paragraphs(&refs, 12.0, 14.0);
        assert_eq!(paras.len(), 2, "250 pt apart = two paragraphs");
        // Even with NO document leading (0.0 → body fallback) the absolute
        // ceiling (2.5 × font size) still splits.
        let paras = split_paragraphs(&refs, 12.0, 0.0);
        assert_eq!(paras.len(), 2, "ceiling splits even without doc leading");
    }

    #[test]
    fn two_normally_leaded_lines_still_fuse() {
        // Regression guard for the hardening: ordinary 14 pt-leading prose
        // (body 12) must keep merging into a single paragraph.
        let runs = vec![
            run("first prose line", 72.0, 700.0, 140.0),
            run("second prose line", 72.0, 686.0, 140.0),
        ];
        let lines = lines_of(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let paras = split_paragraphs(&refs, 12.0, 14.0);
        assert_eq!(paras.len(), 1, "normal leading = one paragraph");
        assert_eq!(paras[0].len(), 2);
    }

    #[test]
    fn side_by_side_lines_without_horizontal_overlap_split() {
        // Two vertically adjacent lines in disjoint horizontal bands (columns,
        // disjoint form fields) share no prose flow: < 20 % overlap ⇒ split,
        // whatever the vertical step says.
        let runs = vec![
            run("left column line", 72.0, 700.0, 100.0), // x 72..172
            run("right column line", 300.0, 686.0, 100.0), // x 300..400, no overlap
        ];
        let lines = lines_of(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let paras = split_paragraphs(&refs, 12.0, 14.0);
        assert_eq!(paras.len(), 2, "no horizontal overlap = two paragraphs");
    }

    #[test]
    fn a_short_last_line_keeps_overlapping_its_paragraph() {
        // A ragged-right closer fully inside the previous line's band overlaps
        // ≥ 20 % of its own (narrower) width — it must NOT split.
        let runs = vec![
            run("a long full body line of text", 72.0, 700.0, 250.0),
            run("short tail", 72.0, 686.0, 60.0),
        ];
        let lines = lines_of(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let paras = split_paragraphs(&refs, 12.0, 14.0);
        assert_eq!(paras.len(), 1, "ragged last line stays in the paragraph");
    }

    // ── fix a1: indent judged against the PREVIOUS line ─────────────────────

    #[test]
    fn an_indented_quote_span_stays_one_paragraph() {
        // A fully indented span (a quote block) inside a flush group: the old
        // rule compared every line to the GROUP's min-left, so each indented
        // line re-triggered the indent break and the quote shattered line by
        // line. Judged line-to-line, the split fires once entering the quote
        // (+30 pt jump), never inside it (delta 0), and once leaving it (the
        // multi-line indented run is not a lone first-line opener).
        let runs = vec![
            run("body flush line one", 72.0, 700.0, 150.0),
            run("body flush line two", 72.0, 686.0, 150.0),
            run("quoted line one xx", 102.0, 672.0, 150.0),
            run("quoted line two xx", 102.0, 658.0, 150.0),
            run("quoted line three ", 102.0, 644.0, 150.0),
            run("body resumes here x", 72.0, 630.0, 150.0),
        ];
        let lines = lines_of(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let paras = split_paragraphs(&refs, 12.0, 14.0);
        assert_eq!(paras.len(), 3, "body / quote / body — the quote is ONE paragraph");
        assert_eq!(paras[0].len(), 2, "flush body before the quote");
        assert_eq!(paras[1].len(), 3, "the 3 indented lines hold together");
        assert_eq!(paras[2].len(), 1, "flush body after the quote reopens");
    }

    #[test]
    fn a_first_line_indented_opener_keeps_its_continuation() {
        // The classic first-line indent: the opener sits +30 pt right, the
        // continuation returns flush. The negative jump straight after a LONE
        // indented opener is NOT a boundary — the three lines are one
        // paragraph, and `paragraph_style` reads the very same shape back as
        // `first_line_pt` (a1 keeps split and style recovery coherent).
        let runs = vec![
            run("indented opening line", 102.0, 700.0, 150.0),
            run("flush continuation aa", 72.0, 686.0, 150.0),
            run("flush continuation bb", 72.0, 672.0, 150.0),
        ];
        let lines = lines_of(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let body = 12.0;
        let paras = split_paragraphs(&refs, body, 14.0);
        assert_eq!(paras.len(), 1, "first-line indent = one paragraph");
        assert_eq!(paras[0].len(), 3);
        // Coherence with the style recovery: the opener's indent is exactly
        // what `paragraph_style` reports as the first-line indent.
        let ctx = ParaContext {
            body,
            leading: group_leading(&refs, body, 14.0),
            group_left: group_left(&refs),
            page_left: 72.0,
            prev_bottom: None,
            next_top: None,
            links: &[],
            ..Default::default()
        };
        let mut ids = IdGen::default();
        let block = build_paragraph_styled(&paras[0], &ctx, &mut ids, Rect::new);
        let BlockKind::Paragraph(p) = block.kind else {
            panic!("expected paragraph");
        };
        assert!(
            (p.style.first_line_pt - 30.0).abs() < 1.0,
            "first-line indent ≈ 30, got {}",
            p.style.first_line_pt
        );
    }

    #[test]
    fn a_sudden_indent_jump_still_opens_a_paragraph() {
        // Regression guard: the indent break must still fire on a genuine
        // rightward jump against the previous line (an indented opener after
        // flush prose).
        let runs = vec![
            run("flush body line one x", 72.0, 700.0, 150.0),
            run("flush body line two x", 72.0, 686.0, 150.0),
            run("indented new opener xx", 102.0, 672.0, 150.0),
        ];
        let lines = lines_of(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let paras = split_paragraphs(&refs, 12.0, 14.0);
        assert_eq!(paras.len(), 2, "the +30 pt jump opens a new paragraph");
        assert_eq!(paras[1].len(), 1);
    }

    // ── fix a3: a metronomic group is judged on its own pace ─────────────────

    #[test]
    fn a_regular_loose_leaded_group_on_a_dense_page_stays_one_paragraph() {
        // A legitimate 16 pt-leaded paragraph on a page whose median leading is
        // 10 pt: the old reference clamped the group to the page
        // (`min(16, 10) = 10`), so every 16 pt step read as a blank-line break
        // and the paragraph shattered line by line. A metronomic group (all
        // gaps within ±20 % of their median) is now judged on ITS pace.
        let runs = vec![
            run("loose leaded line one", 72.0, 700.0, 150.0),
            run("loose leaded line two", 72.0, 684.0, 150.0),
            run("loose leaded line three", 72.0, 668.0, 150.0),
            run("loose leaded line four", 72.0, 652.0, 150.0),
        ];
        let lines = lines_of(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        // body 10, page median leading 10 — a dense page.
        let paras = split_paragraphs(&refs, 10.0, 10.0);
        assert_eq!(paras.len(), 1, "regular 16 pt pace = one paragraph");
        assert_eq!(paras[0].len(), 4);
    }

    #[test]
    fn a_regular_far_apart_lattice_still_splits_on_the_ceiling() {
        // The v1.19.2 guard survives fix a3: a *regular* lattice of far-apart
        // lines (stacked footers / form rows at a metronomic 100 pt step) must
        // NOT ride the own-pace exception into one paragraph — the absolute
        // ceiling (2.5 × font size) still splits it.
        let runs = vec![
            run("far line one", 72.0, 700.0, 120.0),
            run("far line two", 72.0, 600.0, 120.0),
            run("far line three", 72.0, 500.0, 120.0),
        ];
        let lines = lines_of(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let paras = split_paragraphs(&refs, 12.0, 14.0);
        assert_eq!(paras.len(), 3, "the ceiling splits a regular far-apart stack");
    }

    #[test]
    fn an_irregular_group_keeps_the_document_clamped_reference() {
        // Irregular gaps (14, 46, 14) disqualify the own-pace exception: the
        // blank-line break inside the group still splits it in two, exactly as
        // before fix a3.
        let runs = vec![
            run("para one a", 72.0, 700.0, 100.0),
            run("para one b", 72.0, 686.0, 100.0),
            run("para two a", 72.0, 640.0, 100.0),
            run("para two b", 72.0, 626.0, 100.0),
        ];
        let lines = lines_of(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let paras = split_paragraphs(&refs, 12.0, 14.0);
        assert_eq!(paras.len(), 2, "irregular pace → document reference → split");
    }

    // ── fix a6: recovered spacing/indents reach the pageBlocks JSON ──────────

    #[test]
    fn recovered_indent_and_spacing_serialize_into_block_json() {
        // The pageBlocks JSON (`model::json::block_to_json`, the exact shape
        // `gp_page_blocks_json` streams per block) must carry the recovered
        // `indent_left_pt` / `space_before_pt` so a host can re-lay the
        // paragraph. Block left 100 on a page whose MediaBox left is 72
        // → indent 28; a 40 pt gap over a 14 pt leading → space_before 38.
        let runs = vec![run("an indented spaced paragraph", 100.0, 600.0, 200.0)];
        let lines = lines_of(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let ctx = ParaContext {
            body: 12.0,
            leading: 14.0,
            group_left: 100.0,
            page_left: 72.0,
            prev_bottom: Some(612.0 + 40.0),
            next_top: None,
            links: &[],
            ..Default::default()
        };
        let mut ids = IdGen::default();
        let block = build_paragraph_styled(&refs, &ctx, &mut ids, Rect::new);
        let json = crate::model::json::block_to_json(&block);
        assert!(
            json.contains("\"indent_left_pt\":28"),
            "indent_left_pt serialized: {json}"
        );
        assert!(
            json.contains("\"space_before_pt\":38"),
            "space_before_pt serialized: {json}"
        );
    }

    // ── coalesce_runs keeps every source ordinal ─────────────────────────────

    /// Bare same-style run with a given source ordinal.
    fn sourced_inline(text: &str, idx: usize) -> Inline {
        Inline::Run(InlineRun {
            text: text.to_string(),
            style: crate::model::CharStyle {
                size_pt: 12.0,
                ..Default::default()
            },
            source_index: Some(vec![idx]),
        })
    }

    #[test]
    fn coalesce_accumulates_all_source_indices() {
        // Three adjacent same-style sourced runs merge into ONE run carrying
        // ALL their ordinals in order (first = legacy view); an unsourced run
        // (synthesized space) never merges into a sourced one.
        let merged = coalesce_runs(vec![
            sourced_inline("rel", 5),
            sourced_inline("at", 9),
            sourced_inline("if", 12),
        ]);
        assert_eq!(merged.len(), 1);
        let Inline::Run(r) = &merged[0] else {
            panic!("expected run");
        };
        assert_eq!(r.text, "relatif");
        assert_eq!(r.source_index, Some(vec![5, 9, 12]));
        assert_eq!(r.first_source_index(), Some(5), "legacy first-index view");

        let kept = coalesce_runs(vec![
            sourced_inline("a", 1),
            Inline::Run(InlineRun {
                text: " ".to_string(),
                style: crate::model::CharStyle {
                    size_pt: 12.0,
                    ..Default::default()
                },
                source_index: None,
            }),
            sourced_inline("b", 2),
        ]);
        assert_eq!(
            kept.len(),
            3,
            "an unsourced spacer never merges with sourced neighbours"
        );
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
        assert!(
            p.runs.iter().any(|r| match r {
                crate::model::Inline::Run(ir) =>
                    ir.text.contains("normal") && ir.text.contains("text"),
                _ => false,
            }),
            "coalesced run carries both texts"
        );
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
            next_top: None,
            links: &[],
            ..Default::default()
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
            next_top: None,
            links: &[],
            ..Default::default()
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
            next_top: None,
            links: &[],
            ..Default::default()
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
                    Inline::CommentRef { .. } | Inline::FootnoteRef { .. } => {}
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

    // ── V3c fix 1: hanging indent (negative first_line + continuation left) ──

    #[test]
    fn hanging_indent_stays_one_paragraph_and_recovers_negative_first_line() {
        // A bibliography-style entry: the opener flush at x=72, both
        // continuation lines at x=100. The split must keep the three lines as
        // ONE paragraph (the +28 pt jump is a hanging continuation, not an
        // indented opener), and the style must encode the DOCX convention:
        // indent_left = the continuation column (100 − 72 = 28 over the page
        // left) and first_line = −28 pulls the opener back out.
        let runs = vec![
            run("Doe, J. (2021). A rather long", 72.0, 700.0, 200.0),
            run("reference title that wraps on", 100.0, 686.0, 200.0),
            run("two continuation lines here.", 100.0, 672.0, 195.0),
        ];
        let lines = lines_of(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let body = 12.0;
        let paras = split_paragraphs(&refs, body, 14.0);
        assert_eq!(paras.len(), 1, "hanging entry = one paragraph");
        assert_eq!(paras[0].len(), 3);

        let ctx = ParaContext {
            body,
            leading: group_leading(&refs, body, 14.0),
            group_left: group_left(&refs),
            page_left: 72.0,
            prev_bottom: None,
            next_top: None,
            links: &[],
            ..Default::default()
        };
        let mut ids = IdGen::default();
        let block = build_paragraph_styled(&paras[0], &ctx, &mut ids, Rect::new);
        let BlockKind::Paragraph(p) = block.kind else {
            panic!("expected paragraph");
        };
        assert!(
            (p.style.first_line_pt - -28.0).abs() < 1.0,
            "hanging first_line ≈ −28, got {}",
            p.style.first_line_pt
        );
        assert!(
            (p.style.indent_left_pt - 28.0).abs() < 1.0,
            "indent_left anchored to the continuation column ≈ 28, got {}",
            p.style.indent_left_pt
        );
        assert_ne!(
            p.style.align,
            Align::Right,
            "the inset continuations must not misread as right-aligned"
        );
    }

    #[test]
    fn negative_first_line_survives_the_block_json() {
        // The pageBlocks JSON must transport the hanging indent's negative
        // first_line verbatim (no clamping, no drop).
        let runs = vec![
            run("Flush hanging opener line", 72.0, 700.0, 200.0),
            run("continuation over here aa", 100.0, 686.0, 200.0),
            run("continuation over here bb", 100.0, 672.0, 200.0),
        ];
        let lines = lines_of(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let ctx = ParaContext {
            body: 12.0,
            leading: 14.0,
            group_left: 72.0,
            page_left: 72.0,
            prev_bottom: None,
            next_top: None,
            links: &[],
            ..Default::default()
        };
        let mut ids = IdGen::default();
        let block = build_paragraph_styled(&refs, &ctx, &mut ids, Rect::new);
        let json = crate::model::json::block_to_json(&block);
        assert!(
            json.contains("\"first_line_pt\":-28"),
            "negative first_line serialized as-is: {json}"
        );
    }

    #[test]
    fn a_two_line_flush_then_indented_pair_still_splits() {
        // Regression guard for the hanging look-ahead: without a confirming
        // third line, [flush, indented] keeps the historic reading (an indented
        // opener starts its own paragraph).
        let runs = vec![
            run("flush left line", 72.0, 700.0, 110.0),
            run("indented opener", 100.0, 686.0, 110.0),
        ];
        let lines = lines_of(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let paras = split_paragraphs(&refs, 12.0, 14.0);
        assert_eq!(paras.len(), 2, "no third line to confirm a hanging shape");
    }

    // ── V3c fix 3: space_after from the gap to the next paragraph ────────────

    #[test]
    fn gap_after_a_paragraph_recovers_space_after() {
        // Mirror of the space_before test: a paragraph whose bottom sits 40 pt
        // above the next paragraph's top, at 14 pt leading over a 12 pt body.
        // gap = bottom − next_top − (leading − body) = 600 − 560 − 2 = 38.
        let runs = vec![run("a closing paragraph body", 72.0, 600.0, 200.0)];
        let lines = lines_of(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let ctx = ParaContext {
            body: 12.0,
            leading: 14.0,
            group_left: 72.0,
            page_left: 72.0,
            prev_bottom: None,
            next_top: Some(600.0 - 40.0),
            links: &[],
            ..Default::default()
        };
        let mut ids = IdGen::default();
        let block = build_paragraph_styled(&refs, &ctx, &mut ids, Rect::new);
        let BlockKind::Paragraph(p) = block.kind else {
            panic!("expected paragraph");
        };
        assert!(
            (p.style.space_after_pt - 38.0).abs() < 1.0,
            "space_after ≈ 38, got {}",
            p.style.space_after_pt
        );
        // A single-step follow-on (no extra gap) stays quiet: at a 14 pt
        // leading over 12 pt line boxes, the next line's TOP sits just
        // `leading − body` = 2 pt under this paragraph's bottom.
        let quiet = ParaContext {
            next_top: Some(600.0 - (14.0 - 12.0)),
            ..ctx
        };
        let block = build_paragraph_styled(&refs, &quiet, &mut ids, Rect::new);
        let BlockKind::Paragraph(p) = block.kind else {
            panic!("expected paragraph");
        };
        assert_eq!(p.style.space_after_pt, 0.0, "a plain line step is no gap");
    }

    // ── V3c fix 4: line-break softness (wrap vs deliberate break) ────────────

    /// Softness flags for a hand-built stack of lines with the given widths,
    /// judged against a `measure`-wide body column starting at x = 72.
    fn softness_of(widths: &[f64], measure: f64, align: Align) -> Vec<bool> {
        let runs: Vec<ReconRun> = widths
            .iter()
            .enumerate()
            .map(|(i, w)| run("line text", 72.0, 700.0 - 14.0 * i as f64, *w))
            .collect();
        let lines = lines_of(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        line_break_softness(&refs, align, 72.0, 72.0 + measure)
    }

    #[test]
    fn justified_paragraph_breaks_are_all_soft() {
        // Four justified lines → three breaks, every one a wrap artifact.
        let flags = softness_of(&[300.0, 300.0, 300.0, 120.0], 300.0, Align::Justify);
        assert_eq!(flags, vec![true, true, true]);
    }

    #[test]
    fn short_address_lines_break_hard() {
        // Three deliberately short lines (an address block) inside a 468 pt
        // body column: every line ends well inside 85 % of the measure, so
        // each break is a deliberate one — never re-flowed away.
        let flags = softness_of(&[150.0, 90.0, 110.0], 468.0, Align::Left);
        assert_eq!(flags, vec![false, false]);
    }

    #[test]
    fn mixed_wrap_and_short_lines_classify_per_break() {
        // A full-measure first line wraps (soft), a short second line before
        // the closer is a deliberate break (hard).
        let flags = softness_of(&[300.0, 140.0, 295.0], 300.0, Align::Left);
        assert_eq!(flags, vec![true, false]);
        // Single-line paragraph → no breaks to classify.
        assert!(softness_of(&[200.0], 300.0, Align::Left).is_empty());
    }

    /// M1 wiring: `build_paragraph_styled` now ATTACHES the softness flags to
    /// `ParagraphStyle::soft_breaks`, judged against the group measure carried
    /// by `ParaContext::group_right` — one flag per emitted `LineBreak`.
    #[test]
    fn build_paragraph_attaches_soft_breaks() {
        let runs = [
            run("a full-measure wrapping line xx", 72.0, 700.0, 300.0),
            run("short verse", 72.0, 686.0, 120.0),
            run("the closing line of the stanza", 72.0, 672.0, 260.0),
        ];
        let lines = lines_of(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let ctx = ParaContext {
            body: 12.0,
            leading: 14.0,
            group_left: 72.0,
            group_right: 372.0,
            page_left: 72.0,
            links: &[],
            ..Default::default()
        };
        let mut ids = IdGen::default();
        let block = build_paragraph_styled(&refs, &ctx, &mut ids, Rect::new);
        let BlockKind::Paragraph(p) = block.kind else {
            panic!("expected paragraph");
        };
        let breaks = p
            .runs
            .iter()
            .filter(|i| matches!(i, Inline::LineBreak))
            .count();
        assert_eq!(breaks, 2, "two emitted LineBreaks (unchanged)");
        assert_eq!(
            p.style.soft_breaks,
            vec![true, false],
            "full-measure line wraps (soft), the short verse breaks hard"
        );
    }

    // ── M1 fix 1: Unicode strong-script direction heuristic ──────────────────

    /// A paragraph dominated by Hebrew/Arabic strong letters reads RTL; Latin
    /// prose (and RTL below the 60 % bar) stays unspecified.
    #[test]
    fn paragraph_direction_heuristic_flags_rtl_only() {
        let build = |text: &str| {
            let runs = [run(text, 72.0, 700.0, 200.0)];
            let lines = lines_of(&runs);
            let refs: Vec<&ReconLine> = lines.iter().collect();
            let mut ids = IdGen::default();
            let block =
                build_paragraph_styled(&refs, &ParaContext::default(), &mut ids, Rect::new);
            match block.kind {
                BlockKind::Paragraph(p) => p.style.direction,
                other => panic!("expected paragraph, got {other:?}"),
            }
        };
        assert_eq!(
            build("שלום עולם — ברוכים הבאים"),
            Some(crate::model::Direction::Rtl),
            "Hebrew-dominated paragraph reads RTL"
        );
        assert_eq!(
            build("مرحبا بالعالم"),
            Some(crate::model::Direction::Rtl),
            "Arabic-dominated paragraph reads RTL"
        );
        assert_eq!(
            build("Plain latin prose, digits 123 and punctuation."),
            None,
            "Latin prose stays unspecified (LTR)"
        );
        assert_eq!(
            build("mostly latin words quoting שלום once here"),
            None,
            "a short RTL quote below 60 % never flips the paragraph"
        );
    }

    // ── V3c fix 2: paragraph borders & background from drawn boxes ───────────

    /// A paragraph of two body lines spanning x 72..372, y 672..712.
    fn boxed_para_refs(runs_out: &mut Vec<ReconRun>) -> (f64, f64, f64, f64) {
        runs_out.push(run("first framed body line goes here", 72.0, 700.0, 300.0));
        runs_out.push(run("second framed body line closes.", 72.0, 686.0, 300.0));
        (72.0, 686.0, 372.0, 712.0)
    }

    fn boxd(index: usize, x0: f64, y0: f64, x1: f64, y1: f64, fill: Option<[f64; 3]>, stroke: Option<[f64; 3]>) -> PageDecor {
        PageDecor {
            index,
            shape: DecorShape::Box {
                x0,
                y0,
                x1,
                y1,
                fill,
                stroke,
                stroke_width: 1.0,
            },
        }
    }

    #[test]
    fn a_stroked_box_around_a_paragraph_becomes_four_borders() {
        let mut runs = Vec::new();
        let (x0, y0, x1, y1) = boxed_para_refs(&mut runs);
        let lines = lines_of(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        // The encadré: a stroked rect 4 pt outside the text box on every side.
        let decors = [boxd(
            7,
            x0 - 4.0,
            y0 - 4.0,
            x1 + 4.0,
            y1 + 4.0,
            None,
            Some([0.0, 0.0, 0.0]),
        )];
        let d = paragraph_decorations(&refs, &decors, 12.0);
        let borders = d.borders.expect("all four borders recovered");
        assert!(borders.iter().all(Option::is_some), "four sides set");
        assert_eq!(d.claimed, vec![7], "the box path is consumed");
        assert!(d.background.is_none(), "no fill → no shading");
    }

    #[test]
    fn a_gray_filled_rect_behind_a_paragraph_becomes_background() {
        let mut runs = Vec::new();
        let (x0, y0, x1, y1) = boxed_para_refs(&mut runs);
        let lines = lines_of(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let decors = [boxd(
            3,
            x0 - 2.0,
            y0 - 2.0,
            x1 + 2.0,
            y1 + 2.0,
            Some([0.85, 0.85, 0.85]),
            None,
        )];
        let d = paragraph_decorations(&refs, &decors, 12.0);
        assert_eq!(d.background, Some([0.85, 0.85, 0.85]));
        assert_eq!(d.claimed, vec![3]);
        assert!(d.borders.is_none(), "no stroke → no borders");
    }

    #[test]
    fn white_page_wide_or_partial_rects_claim_nothing() {
        let mut runs = Vec::new();
        let (x0, y0, x1, y1) = boxed_para_refs(&mut runs);
        let lines = lines_of(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let decors = [
            // A white mask under the text: never shading.
            boxd(1, x0, y0, x1, y1, Some([1.0, 1.0, 1.0]), None),
            // A page-wide background: strays far past the hug margin.
            boxd(2, 0.0, 0.0, 612.0, 792.0, Some([0.9, 0.9, 0.9]), None),
            // A partial swatch covering only a corner of the paragraph.
            boxd(4, x0, y0, x0 + 60.0, y0 + 10.0, Some([0.8, 0.8, 0.8]), None),
            // A stroked box around something else entirely.
            boxd(5, x0, y1 + 100.0, x1, y1 + 160.0, None, Some([0.0; 3])),
        ];
        let d = paragraph_decorations(&refs, &decors, 12.0);
        assert!(d.background.is_none(), "no background claimed");
        assert!(d.borders.is_none(), "no borders claimed");
        assert!(d.claimed.is_empty(), "nothing consumed");
    }

    #[test]
    fn four_ruling_lines_forming_a_frame_become_borders() {
        let mut runs = Vec::new();
        let (x0, y0, x1, y1) = boxed_para_refs(&mut runs);
        let lines = lines_of(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let black = [0.0, 0.0, 0.0];
        let rule_h = |index, y| PageDecor {
            index,
            shape: DecorShape::HRule {
                y,
                x0: x0 - 2.0,
                x1: x1 + 2.0,
                color: black,
                width: 0.6,
            },
        };
        let rule_v = |index, x| PageDecor {
            index,
            shape: DecorShape::VRule {
                x,
                y0: y0 - 2.0,
                y1: y1 + 2.0,
                color: black,
                width: 0.6,
            },
        };
        // All four sides → borders; drop one side → nothing (all-or-nothing).
        let full = [
            rule_h(10, y1 + 3.0),
            rule_h(11, y0 - 3.0),
            rule_v(12, x0 - 3.0),
            rule_v(13, x1 + 3.0),
        ];
        let d = paragraph_decorations(&refs, &full, 12.0);
        assert!(
            d.borders.is_some_and(|b| b.iter().all(Option::is_some)),
            "four rules → four borders"
        );
        assert_eq!(d.claimed.len(), 4, "all four rules consumed");

        let partial = [rule_h(10, y1 + 3.0), rule_h(11, y0 - 3.0)];
        let d = paragraph_decorations(&refs, &partial, 12.0);
        assert!(d.borders.is_none(), "top+bottom only is not a frame");
        assert!(d.claimed.is_empty());
    }
}
