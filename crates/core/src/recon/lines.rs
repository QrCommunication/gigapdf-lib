//! Stage 1 — **line grouping**. Cluster a page's text runs into reading-order
//! lines: sort top→bottom (PDF *Y up*, so descending `y`) then left→right, and
//! start a new line whenever a run's vertical centre departs the line's running
//! **centroid** by more than a baseline-band tolerance.
//!
//! Two refinements keep mixed-size lines intact (issue #5):
//!
//!  - **Centroid anchor (not first-run).** A line's vertical position tracks the
//!    width-weighted centroid of its runs' centres, recomputed as runs join — not
//!    whichever run happened to be sorted first. So a line that *opens* with a
//!    superscript or a small-cap run is anchored on the dominant body text, not on
//!    that outlier, and the next body run still falls inside the band.
//!  - **Second-pass overlap merge.** After the initial banding, fragments whose
//!    vertical extent is *engulfed* by an adjacent line (a superscript/subscript
//!    footnote marker, an inline formula, a mixed-size run that got split into its
//!    own pseudo-line) are merged back into the line they belong to — while two
//!    genuinely separate body lines stay apart, gated on a vertical-overlap ratio
//!    *and* the fragment being small/partial relative to the main line.
//!
//! A [`ReconLine`] keeps its constituent runs (so later stages can read first-run
//! indent, font size, list markers, per-cell text) plus the union bounding box.

use super::{median, ReconRun};

/// A reading-order line: the runs sharing a baseline band, left→right, with the
/// union box in PDF user space (origin bottom-left).
#[derive(Debug, Clone)]
pub struct ReconLine {
    pub runs: Vec<ReconRun>,
    /// Union lower-left X.
    pub x: f64,
    /// Union lower-left Y.
    pub y: f64,
    /// Union width.
    pub w: f64,
    /// Union height.
    pub h: f64,
}

impl ReconLine {
    /// The concatenated text of the line. Adjacent runs are joined **gap-aware**
    /// (via [`runs_join`](super::runs_join)): a space is inserted only when the
    /// horizontal gap to the previous run is a real inter-word space, not when a
    /// word is split across fonts (`"ENFANT"`+`"S"` → `"ENFANTS"`, not
    /// `"ENFANT S"`). A run already carrying leading/trailing whitespace keeps it
    /// and is never double-spaced.
    pub fn text(&self) -> String {
        let mut s = String::new();
        // Previous emitted run's right edge, height, and whether its raw text
        // ended with whitespace. `None` before the first non-blank run.
        let mut prev: Option<(f64, f64, bool)> = None;
        for run in &self.runs {
            let t = run.text.trim();
            if t.is_empty() {
                continue;
            }
            if let Some((prev_right, prev_h, prev_trailing_ws)) = prev {
                // The trim dropped each run's own whitespace; a space is due when
                // either side carried one, or the gap is a real inter-word gap
                // (not a multi-font split-word butt at gap ≈ 0).
                if prev_trailing_ws
                    || run.text.starts_with(char::is_whitespace)
                    || !super::runs_join(prev_right, run.x, run.h.max(prev_h))
                {
                    s.push(' ');
                }
            }
            s.push_str(t);
            prev = Some((run.right(), run.h, run.text.ends_with(char::is_whitespace)));
        }
        s
    }

    /// The line's left edge (its first run's X in reading order).
    pub fn left(&self) -> f64 {
        self.x
    }

    /// The line's right edge.
    pub fn right(&self) -> f64 {
        self.x + self.w
    }

    /// The line's vertical centre.
    pub fn center_y(&self) -> f64 {
        self.y + self.h / 2.0
    }

    /// The line's top edge (larger Y = higher on the page).
    pub fn top(&self) -> f64 {
        self.y + self.h
    }

    /// The representative (median) font size of the line's runs.
    pub fn font_size(&self) -> f64 {
        let mut sizes: Vec<f64> = self.runs.iter().map(|r| r.size.max(1.0)).collect();
        median(&mut sizes, self.h.max(1.0))
    }

    /// Whether *any* run on the line is bold (drives heading promotion).
    pub fn is_bold(&self) -> bool {
        self.runs.iter().any(|r| r.style.bold)
    }

    /// The line's dominant baseline [`Rotation`] — the orientation a block built
    /// from this line should carry. Upright lines report
    /// [`Rotation::D0`](crate::model::geom::Rotation::D0); a vertical/rotated run
    /// drives the cardinal/free-form variant. See
    /// [`runs_rotation`](super::runs_rotation).
    pub fn rotation(&self) -> crate::model::geom::Rotation {
        super::runs_rotation(&self.runs)
    }
}

/// The dominant baseline [`Rotation`] across a group of lines (e.g. the lines of
/// one paragraph or list). Pools every line's runs so a multi-line rotated block
/// is judged as a whole. Upright groups report
/// [`Rotation::D0`](crate::model::geom::Rotation::D0). See
/// [`runs_rotation`](super::runs_rotation).
pub(crate) fn lines_rotation(lines: &[&ReconLine]) -> crate::model::geom::Rotation {
    let runs: Vec<ReconRun> = lines.iter().flat_map(|l| l.runs.iter().cloned()).collect();
    super::runs_rotation(&runs)
}

/// Group runs into [`ReconLine`]s. Runs are first ordered top→bottom then
/// left→right; a new line begins when a run's vertical centre departs the line's
/// running **width-weighted centroid** by more than the baseline tolerance
/// (`0.6 ×` the larger of the run / current-line height). A second pass then
/// merges fragments (superscripts, inline formulae, split mixed-size runs) whose
/// vertical extent is engulfed by an adjacent line back into that line, without
/// fusing two genuinely separate body lines (see [`merge_overlapping_fragments`]).
pub fn group_into_lines(runs: &[ReconRun]) -> Vec<ReconLine> {
    let mut items: Vec<&ReconRun> = runs.iter().filter(|r| !r.text.trim().is_empty()).collect();
    if items.is_empty() {
        return Vec::new();
    }
    // Top→bottom: PDF y is up, so larger centre first. Then left→right.
    items.sort_by(|a, b| {
        b.center_y()
            .partial_cmp(&a.center_y())
            .unwrap_or(core::cmp::Ordering::Equal)
            .then(a.x.partial_cmp(&b.x).unwrap_or(core::cmp::Ordering::Equal))
    });

    let mut lines: Vec<ReconLine> = Vec::new();
    // Running width-weighted centroid of the open line's run centres, plus the
    // total weight, so each added run *re-centres* the band. Anchoring on the
    // centroid (not the first run) keeps a line that opens with a superscript /
    // small-cap run banded on its dominant body text — the next body run still
    // falls inside the tolerance instead of starting a spurious new line.
    let mut centroid = f64::INFINITY;
    let mut weight_sum = 0.0f64;
    let mut row_height = 0.0f64;

    for run in items {
        let c = run.center_y();
        let tol = run.h.max(row_height).max(1.0) * 0.6;
        if lines.is_empty() || (centroid - c).abs() > tol {
            lines.push(ReconLine {
                x: run.x,
                y: run.y,
                w: run.w,
                h: run.h,
                runs: vec![run.clone()],
            });
            // Weight by width so a wide body run dominates a hair-thin marker;
            // never zero (a width-0 run still anchors a fresh line).
            let wt = run.w.max(0.01);
            centroid = c;
            weight_sum = wt;
            row_height = run.h;
        } else {
            let line = lines.last_mut().unwrap();
            line.runs.push(run.clone());
            // Union the bounds.
            let x0 = line.x.min(run.x);
            let y0 = line.y.min(run.y);
            let x1 = (line.x + line.w).max(run.x + run.w);
            let y1 = (line.y + line.h).max(run.y + run.h);
            line.x = x0;
            line.y = y0;
            line.w = x1 - x0;
            line.h = y1 - y0;
            // Fold the run into the running centroid (incremental weighted mean).
            let wt = run.w.max(0.01);
            weight_sum += wt;
            centroid += (c - centroid) * (wt / weight_sum);
            row_height = row_height.max(run.h);
        }
    }

    // Second pass: rejoin engulfed fragments to the line they belong to.
    merge_overlapping_fragments(&mut lines);

    // Within each line keep the runs left→right (they were inserted in that
    // order already by the global sort, but a later-added run from a band — or a
    // merged fragment — can arrive out of order, so re-sort defensively).
    for line in &mut lines {
        line.runs
            .sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(core::cmp::Ordering::Equal));
    }
    lines
}

/// Second banding pass: merge **fragment** lines whose vertical extent is engulfed
/// by an adjacent line back into that line. The first pass anchors on the
/// centroid, but a superscript/subscript footnote marker, an inline formula run,
/// or a mixed-size glyph can still land far enough from the body centroid to open
/// its own pseudo-line. Such a fragment belongs to the body line it overlaps.
///
/// The merge is gated so two genuinely separate body lines never fuse: a candidate
/// pair must (a) overlap vertically by at least [`MERGE_OVERLAP_RATIO`] of the
/// **shorter** line's extent — measured on the runs' real top/bottom, not a centre
/// point — *and* (b) the shorter line must be a *partial* band, meaningfully
/// smaller than the taller one (height ≤ [`FRAGMENT_HEIGHT_RATIO`] ×). Two adjacent
/// full-height body lines (equal heights, extents merely touching) satisfy neither
/// and stay apart.
///
/// Lines are processed top→bottom and a fragment is folded into its taller
/// neighbour (above or below, whichever overlaps more), so the surviving lines stay
/// in top→bottom order. The merged line's runs are concatenated; the caller
/// re-sorts each line left→right afterwards to preserve horizontal reading order.
fn merge_overlapping_fragments(lines: &mut Vec<ReconLine>) {
    if lines.len() < 2 {
        return;
    }
    // Top→bottom by the line's vertical centre (the first pass already emits in
    // this order, but a band can finish with a lower centroid than the next; sort
    // so "adjacent" is well defined and the output stays top→bottom).
    lines.sort_by(|a, b| {
        b.center_y()
            .partial_cmp(&a.center_y())
            .unwrap_or(core::cmp::Ordering::Equal)
    });

    let mut merged: Vec<ReconLine> = Vec::with_capacity(lines.len());
    for line in lines.drain(..) {
        // Try to fold `line` into the most-overlapping already-kept line. Only the
        // neighbours whose extent still overlaps are eligible; the strongest
        // overlap wins so a fragment between two lines joins the closer one.
        let mut best: Option<(usize, f64)> = None;
        for (i, kept) in merged.iter().enumerate() {
            if let Some(ratio) = fragment_overlap_ratio(kept, &line) {
                if best.map(|(_, r)| ratio > r).unwrap_or(true) {
                    best = Some((i, ratio));
                }
            }
        }
        if let Some((i, _)) = best {
            union_line(&mut merged[i], &line);
        } else {
            merged.push(line);
        }
    }
    *lines = merged;
}

/// Minimum fraction of the **shorter** line's vertical extent that must be covered
/// by the overlap for the pair to be a fragment-of-a-line (vs two separate lines).
const MERGE_OVERLAP_RATIO: f64 = 0.55;

/// A fragment must be at most this tall *relative to* the line it joins. A
/// superscript / subscript / inline marker covers only part of the body band; two
/// equal-height body lines fail this and never merge.
const FRAGMENT_HEIGHT_RATIO: f64 = 0.8;

/// If `cand` is a *fragment* engulfed by `main` (or vice-versa), return the
/// vertical-overlap ratio over the shorter extent; else `None`. Uses the runs'
/// real top/bottom (`y .. y+h`), not a centre point, so a superscript sitting in
/// the **top** portion of a body line is recognised even though their centres are
/// far apart. Symmetric in the pair: whichever line is shorter is the fragment.
fn fragment_overlap_ratio(main: &ReconLine, cand: &ReconLine) -> Option<f64> {
    let (a_bot, a_top) = (main.y, main.y + main.h);
    let (b_bot, b_top) = (cand.y, cand.y + cand.h);
    let overlap = a_top.min(b_top) - a_bot.max(b_bot);
    if overlap <= 0.0 {
        return None; // extents disjoint (or merely touching) → distinct lines.
    }
    let h_short = main.h.min(cand.h).max(1e-6);
    let h_tall = main.h.max(cand.h).max(1e-6);
    // The shorter line must be a genuine partial band, not a near-equal-height
    // neighbour, and the overlap must cover most of that short band.
    if h_short > h_tall * FRAGMENT_HEIGHT_RATIO {
        return None;
    }
    let ratio = overlap / h_short;
    (ratio >= MERGE_OVERLAP_RATIO).then_some(ratio)
}

/// Fold `src`'s runs and bounding box into `dst` (union box; runs appended — the
/// caller re-sorts left→right).
fn union_line(dst: &mut ReconLine, src: &ReconLine) {
    let x0 = dst.x.min(src.x);
    let y0 = dst.y.min(src.y);
    let x1 = (dst.x + dst.w).max(src.x + src.w);
    let y1 = (dst.y + dst.h).max(src.y + src.h);
    dst.x = x0;
    dst.y = y0;
    dst.w = x1 - x0;
    dst.h = y1 - y0;
    dst.runs.extend(src.runs.iter().cloned());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::style::TextStyle;

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
    fn two_runs_on_one_baseline_merge_into_one_line() {
        // Same y, side by side → one line, two runs, joined text.
        let runs = vec![
            run("Hello", 72.0, 700.0, 12.0),
            run("World", 140.0, 700.0, 12.0),
        ];
        let lines = group_into_lines(&runs);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].runs.len(), 2);
        assert_eq!(lines[0].text(), "Hello World");
    }

    #[test]
    fn separated_baselines_make_separate_lines_top_first() {
        // y=700 is higher on the page than y=680; line order is top→bottom.
        let runs = vec![
            run("second", 72.0, 680.0, 12.0),
            run("first", 72.0, 700.0, 12.0),
        ];
        let lines = group_into_lines(&runs);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text(), "first");
        assert_eq!(lines[1].text(), "second");
    }

    #[test]
    fn line_font_size_and_bold_are_reported() {
        let mut bold = run("Title", 72.0, 700.0, 24.0);
        bold.style.bold = true;
        let lines = group_into_lines(&[bold]);
        assert_eq!(lines.len(), 1);
        assert!((lines[0].font_size() - 24.0).abs() < 1e-9);
        assert!(lines[0].is_bold());
    }

    #[test]
    fn blank_runs_are_dropped() {
        let runs = vec![
            run("   ", 72.0, 700.0, 12.0),
            run("kept", 72.0, 680.0, 12.0),
        ];
        let lines = group_into_lines(&runs);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text(), "kept");
    }

    #[test]
    fn empty_input_yields_no_lines() {
        assert!(group_into_lines(&[]).is_empty());
    }

    // ── gap-aware spacing: multi-font words must not gain spurious spaces ──────

    /// Build a run at an explicit X with a width chosen so the next run can butt
    /// it (gap ≈ 0) or stand off it (real space). `w = len·size·0.5`.
    fn at(text: &str, x: f64, size: f64) -> ReconRun {
        let mut r = run(text, x, 700.0, size);
        r.w = text.chars().count() as f64 * size * 0.5;
        r
    }

    #[test]
    fn split_word_runs_join_without_a_spurious_space() {
        // A dense form draws "ENFANTS" as "ENFANT"+"S" from two embedded fonts
        // (the "S" butts the previous run: gap ≈ 0). Likewise "MINEURS". A real
        // inter-word gap separates the two words. Expect "ENFANTS MINEURS", never
        // "ENFANT S MINEUR S".
        let enfant = at("ENFANT", 72.0, 10.0); // x 72..102
        let s1 = at("S", 102.0, 10.0); // butts → join → "ENFANTS"
        let mineur = at("MINEUR", 130.0, 10.0); // gap 130-107 = 23 → space before
        let s2 = at("S", 160.0, 10.0); // butts → join → "MINEURS"
        let lines = group_into_lines(&[enfant, s1, mineur, s2]);
        assert_eq!(lines.len(), 1, "one baseline band → one line");
        assert_eq!(lines[0].text(), "ENFANTS MINEURS");
    }

    #[test]
    fn a_real_inter_word_gap_keeps_its_space() {
        // "DES" then "ENFANTS" with a clear gap must NOT fuse into "DESENFANTS".
        let des = at("DES", 72.0, 10.0); // x 72..87
        let enfants = at("ENFANTS", 110.0, 10.0); // gap 110-87 = 23 → space
        let lines = group_into_lines(&[des, enfants]);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text(), "DES ENFANTS");
    }

    // ── centroid anchoring + overlap-merge: superscripts / mixed sizes (#5) ──────

    /// A run with explicit y/size (so vertical extents can be controlled). `w` is
    /// proportional to text length & size, matching `at`.
    fn yat(text: &str, x: f64, y: f64, size: f64) -> ReconRun {
        let mut r = run(text, x, y, size);
        r.w = text.chars().count() as f64 * size * 0.5;
        r
    }

    #[test]
    fn line_opening_with_a_superscript_anchors_on_the_body_centroid() {
        // A footnote reference opens the line: a tiny "12" superscript sits raised
        // and in a smaller font, *before* (sorted above, by higher centre) the wide
        // body text. The band must anchor on the dominant body run — not the
        // superscript outlier — and the superscript must remain part of that line.
        //
        // Body "Theorem" at y=700 h=12 → centre 706, extent [700,712], wide run.
        // Superscript "12" at y=707 size=7 → centre 710.5, raised, narrow.
        let body = yat("Theorem", 90.0, 700.0, 12.0); // x 90..132, centre 706
        let sup = yat("12", 72.0, 707.0, 7.0); // x 72..79, centre 710.5 (sorted first)
        let lines = group_into_lines(&[body, sup]);

        assert_eq!(lines.len(), 1, "superscript opener must not split the line");
        assert_eq!(lines[0].runs.len(), 2);
        // Anchored on the body, not the superscript: the line centre is far closer
        // to the body's 706 than to the superscript's 710.5.
        let c = lines[0].center_y();
        assert!(
            (c - 706.0).abs() < (c - 710.5).abs(),
            "line centre {c} should sit near the body (706), not the superscript (710.5)"
        );
        // Horizontal order preserved: the superscript (x=72) precedes the body.
        assert_eq!(lines[0].runs[0].text, "12");
        assert_eq!(lines[0].runs[1].text, "Theorem");
    }

    #[test]
    fn a_mid_line_inline_superscript_stays_on_one_line_in_reading_order() {
        // "x" then a raised exponent "2" then "+ y" on the same body line: one line,
        // runs left→right ("x", "2", "+ y"). The raised "2" must not split off.
        let x = yat("x", 72.0, 700.0, 12.0); // centre 706
        let exp = yat("2", 80.0, 706.0, 7.0); // raised small exponent, centre 709.5
        let rest = yat("+ y", 88.0, 700.0, 12.0); // centre 706
        let lines = group_into_lines(&[x, exp, rest]);

        assert_eq!(lines.len(), 1, "inline exponent must stay on the body line");
        let order: Vec<&str> = lines[0].runs.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(order, vec!["x", "2", "+ y"], "runs kept left→right");
    }

    #[test]
    fn two_distinct_body_lines_are_not_over_merged() {
        // Two full-height body lines one leading apart: equal heights, vertical
        // extents disjoint → the overlap-merge guard must keep them separate.
        // Line A y=700 h=12 → [700,712]; line B y=684 h=12 → [684,696].
        let a = yat("first line", 72.0, 700.0, 12.0);
        let b = yat("second line", 72.0, 684.0, 12.0);
        let lines = group_into_lines(&[a, b]);

        assert_eq!(lines.len(), 2, "adjacent body lines must not merge");
        assert_eq!(lines[0].text(), "first line");
        assert_eq!(lines[1].text(), "second line");
    }

    #[test]
    fn a_normal_single_size_line_is_unchanged() {
        // A plain uniform line: three same-size runs on one baseline → one line,
        // text joined, runs left→right. (Regression guard for the no-op path.)
        let runs = vec![
            yat("the", 72.0, 700.0, 12.0),
            yat("quick", 96.0, 700.0, 12.0),
            yat("fox", 140.0, 700.0, 12.0),
        ];
        let lines = group_into_lines(&runs);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].runs.len(), 3);
        assert_eq!(lines[0].text(), "the quick fox");
    }

    #[test]
    fn the_overlap_merge_pass_rejoins_an_engulfed_fragment_keeping_runs_ordered() {
        // Direct exercise of the second pass: two lines where the shorter is a
        // partial band engulfed by the taller — a superscript run that landed in
        // its own pseudo-line. `merge_overlapping_fragments` must fold it back and
        // the surviving line must keep its runs left→right.
        //
        // Main line "body" extent [700,712] (h=12); fragment "12" extent [706,713]
        // (h=7) sits in the upper portion → engulfed. Centres differ (706 vs 709.5)
        // but the extents overlap by 6 of the fragment's 7 → ratio ≈ 0.86 ≥ 0.55,
        // and 7 ≤ 12·0.8 → the fragment is small enough to be a fragment.
        let main = ReconLine {
            x: 90.0,
            y: 700.0,
            w: 48.0,
            h: 12.0,
            runs: vec![yat("body", 90.0, 700.0, 12.0)],
        };
        let frag = ReconLine {
            x: 72.0,
            y: 706.0,
            w: 7.0,
            h: 7.0,
            runs: vec![yat("12", 72.0, 706.0, 7.0)],
        };
        assert!(
            fragment_overlap_ratio(&main, &frag).is_some(),
            "an engulfed partial fragment must be recognised"
        );
        let mut lines = vec![main, frag];
        merge_overlapping_fragments(&mut lines);
        // After the merge: re-sort left→right as group_into_lines does.
        for l in &mut lines {
            l.runs
                .sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(core::cmp::Ordering::Equal));
        }
        assert_eq!(lines.len(), 1, "the fragment must be merged into the line");
        let order: Vec<&str> = lines[0].runs.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(order, vec!["12", "body"], "merged runs kept left→right");
    }

    #[test]
    fn the_overlap_merge_pass_never_fuses_two_equal_height_body_lines() {
        // The no-over-merge guard, exercised on the second pass directly: two
        // full-height body lines, equal heights, even if their extents *touch* —
        // the "fragment must be small relative to the main line" gate (height ratio)
        // forbids the merge regardless of overlap.
        let upper = ReconLine {
            x: 72.0,
            y: 700.0,
            w: 60.0,
            h: 12.0, // extent [700,712]
            runs: vec![yat("upper line", 72.0, 700.0, 12.0)],
        };
        // Touching extent [688,700] (shares the 700 edge → zero-area overlap):
        let lower = ReconLine {
            x: 72.0,
            y: 688.0,
            w: 60.0,
            h: 12.0,
            runs: vec![yat("lower line", 72.0, 688.0, 12.0)],
        };
        assert!(
            fragment_overlap_ratio(&upper, &lower).is_none(),
            "equal-height neighbours are not fragments of each other"
        );
        let mut lines = vec![upper, lower];
        merge_overlapping_fragments(&mut lines);
        assert_eq!(lines.len(), 2, "two body lines must stay separate");
    }

    #[test]
    fn text_skips_an_interleaved_blank_run() {
        // A line whose runs include a whitespace-only run between two words: the
        // `t.is_empty()` guard in `text()` must drop it (no double space, no panic).
        let line = ReconLine {
            x: 72.0,
            y: 700.0,
            w: 120.0,
            h: 12.0,
            runs: vec![
                yat("alpha", 72.0, 700.0, 12.0),
                yat("   ", 110.0, 700.0, 12.0), // blank → continue
                yat("beta", 150.0, 700.0, 12.0),
            ],
        };
        // The blank vanishes; the real inter-word gap still yields one space.
        assert_eq!(line.text(), "alpha beta");
    }

    #[test]
    fn fragment_prefers_the_more_overlapping_of_two_kept_lines() {
        // Two tall, equal-height body lines that do NOT fuse with each other (the
        // height gate forbids it) both survive into `merged`. A short fragment then
        // overlaps BOTH — exercising the `best` update branch where a later, higher
        // ratio replaces the first match. It must fold into the stronger-overlap
        // line (the lower one here, ratio 1.0 > 0.75).
        let main_hi = ReconLine {
            x: 90.0,
            y: 702.0,
            w: 48.0,
            h: 12.0, // extent [702,714], centre 708 (processed first)
            runs: vec![yat("upper", 90.0, 702.0, 12.0)],
        };
        let main_lo = ReconLine {
            x: 90.0,
            y: 697.0,
            w: 48.0,
            h: 12.0, // extent [697,709], centre 703
            runs: vec![yat("lower", 90.0, 697.0, 12.0)],
        };
        let frag = ReconLine {
            x: 72.0,
            y: 701.0,
            w: 7.0,
            h: 4.0, // extent [701,705]
            runs: vec![yat("xx", 72.0, 701.0, 4.0)],
        };
        // Sanity: the two body lines are not fragments of each other.
        assert!(fragment_overlap_ratio(&main_hi, &main_lo).is_none());
        // Fragment overlaps the upper at 3/4 and the lower at 4/4.
        let r_hi = fragment_overlap_ratio(&main_hi, &frag).unwrap();
        let r_lo = fragment_overlap_ratio(&main_lo, &frag).unwrap();
        assert!(r_lo > r_hi, "lower line must be the stronger match");
        let mut lines = vec![main_hi, main_lo, frag];
        merge_overlapping_fragments(&mut lines);
        // Two survivors: the upper body line and the lower body line with the
        // fragment folded in.
        assert_eq!(lines.len(), 2);
        let with_frag = lines
            .iter()
            .find(|l| l.runs.iter().any(|r| r.text == "xx"))
            .expect("fragment merged into a line");
        assert!(
            with_frag.runs.iter().any(|r| r.text == "lower"),
            "fragment must join the more-overlapping (lower) line"
        );
    }
}
