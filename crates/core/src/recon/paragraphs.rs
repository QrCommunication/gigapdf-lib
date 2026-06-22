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
use super::{median, run_char_style, IdGen};
use crate::model::{
    geom::Rotation, Align, Block, BlockKind, Inline, InlineRun, Paragraph, ParagraphStyle, Rect,
};

/// Split a group of reading-order lines into paragraphs (each a `Vec` of line
/// references). `body` is the document body font size, used to estimate leading.
pub fn split_paragraphs<'a>(lines: &[&'a ReconLine], body: f64) -> Vec<Vec<&'a ReconLine>> {
    if lines.is_empty() {
        return Vec::new();
    }
    // Estimate the body leading from the median line-to-line vertical step.
    let leading = estimate_leading(lines, body);
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

/// Build one [`Block::Paragraph`] from a paragraph's lines. The runs of every
/// line are concatenated into inline runs separated by [`Inline::LineBreak`];
/// the frame is the union of the lines' boxes (flipped to top-down by `to_frame`).
pub fn build_paragraph(
    para: &[&ReconLine],
    ids: &mut IdGen,
    to_frame: impl Fn(f64, f64, f64, f64) -> Rect,
) -> Block {
    let (x, y, w, h) = union_box(para);
    let left = block_left(para);
    let right = block_right(para);

    let mut runs: Vec<Inline> = Vec::new();
    for (i, line) in para.iter().enumerate() {
        if i > 0 {
            runs.push(Inline::LineBreak);
        }
        for r in &line.runs {
            let t = r.text.trim();
            if t.is_empty() {
                continue;
            }
            runs.push(Inline::Run(InlineRun {
                text: r.text.clone(),
                style: run_char_style(r),
                source_index: r.source_index,
            }));
        }
    }

    let align = paragraph_align(para, left, right);
    let paragraph = Paragraph {
        style: ParagraphStyle {
            align,
            ..ParagraphStyle::default()
        },
        style_ref: None,
        runs,
    };
    Block {
        id: ids.mint(),
        frame: Some(to_frame(x, y, w, h)),
        rotation: Rotation::D0,
        kind: BlockKind::Paragraph(paragraph),
    }
}

/// Estimate body leading from consecutive line steps; fall back to `1.2 × body`.
fn estimate_leading(lines: &[&ReconLine], body: f64) -> f64 {
    let mut steps: Vec<f64> = lines
        .windows(2)
        .map(|w| (w[0].center_y() - w[1].center_y()).abs())
        .filter(|s| *s > 0.0)
        .collect();
    let med = median(&mut steps, body * 1.2);
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
}
