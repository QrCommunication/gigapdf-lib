//! Stage 2 — **column detection & reading order**. A multi-column page (news,
//! academic two-up) must be read column-by-column, not row-by-row, or the text
//! interleaves into nonsense. We project the lines' horizontal extents onto the
//! X axis, find a *vertical gutter* — a band of X with no line covering it that
//! splits the content into left/right halves — and order each column's lines
//! top→bottom, left band first.
//!
//! A gutter-free page (the overwhelmingly common case) yields a single column:
//! the lines are returned in their existing top→bottom order, unchanged.

use super::lines::ReconLine;

/// Produce the reading order of `lines` as a list of indices into `lines`.
/// Single column ⇒ identity-ish (top→bottom). Multiple columns ⇒ all of the
/// left band's lines (top→bottom) before the next band's.
pub fn reading_order(lines: &[ReconLine]) -> Vec<usize> {
    if lines.len() < 2 {
        return (0..lines.len()).collect();
    }
    let bands = column_bands(lines);
    if bands.len() < 2 {
        // One column: keep the input order (already top→bottom from stage 1).
        return (0..lines.len()).collect();
    }

    let mut order: Vec<usize> = Vec::with_capacity(lines.len());
    for band in &bands {
        let mut idxs: Vec<usize> = (0..lines.len())
            .filter(|&i| band.contains(lines[i].center_x()))
            .collect();
        // Top→bottom within the column (PDF y up → larger centre first).
        idxs.sort_by(|&a, &b| {
            lines[b]
                .center_y()
                .partial_cmp(&lines[a].center_y())
                .unwrap_or(core::cmp::Ordering::Equal)
                .then(
                    lines[a]
                        .x
                        .partial_cmp(&lines[b].x)
                        .unwrap_or(core::cmp::Ordering::Equal),
                )
        });
        order.extend(idxs);
    }
    // Any line whose centre fell outside every band (shouldn't happen) trails in
    // original order so nothing is dropped.
    for i in 0..lines.len() {
        if !order.contains(&i) {
            order.push(i);
        }
    }
    order
}

impl ReconLine {
    /// Horizontal centre of the line.
    pub(crate) fn center_x(&self) -> f64 {
        self.x + self.w / 2.0
    }
}

/// A left→right contiguous column band `[lo, hi]` of X (points).
#[derive(Debug, Clone, Copy)]
struct Band {
    lo: f64,
    hi: f64,
}
impl Band {
    fn contains(&self, x: f64) -> bool {
        x >= self.lo && x <= self.hi
    }
}

/// Split the page's X span into column bands by finding gutters: gaps in the
/// union of the lines' X intervals that are wide enough (and tall enough in
/// coverage) to be real column separators rather than inter-word spacing.
fn column_bands(lines: &[ReconLine]) -> Vec<Band> {
    // Calibrate the minimum gutter width to the typography: a gutter must be
    // clearly wider than a normal space — use a few times the median line
    // height, floored so tiny pages don't over-split.
    let mut heights: Vec<f64> = lines.iter().map(|l| l.h.max(1.0)).collect();
    let h_med = super::median(&mut heights, 10.0);
    let min_gutter = (h_med * 2.0).max(18.0);

    // Build the page X span and a coverage map as sorted (start, end) intervals.
    let mut intervals: Vec<(f64, f64)> = lines.iter().map(|l| (l.x, l.x + l.w)).collect();
    intervals.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(core::cmp::Ordering::Equal));

    // Merge overlapping intervals; the gaps between merged blocks are candidate
    // gutters.
    let mut merged: Vec<(f64, f64)> = Vec::new();
    for (s, e) in intervals {
        if let Some(last) = merged.last_mut() {
            if s <= last.1 {
                last.1 = last.1.max(e);
                continue;
            }
        }
        merged.push((s, e));
    }

    let page_lo = merged.first().map(|m| m.0).unwrap_or(0.0);
    let page_hi = merged.last().map(|m| m.1).unwrap_or(0.0);

    // Each gap between merged blocks wider than `min_gutter` becomes a column
    // boundary; bands are the spans between consecutive boundaries.
    let mut boundaries: Vec<f64> = Vec::new();
    for w in merged.windows(2) {
        let gap = w[1].0 - w[0].1;
        if gap >= min_gutter {
            boundaries.push((w[0].1 + w[1].0) / 2.0);
        }
    }

    if boundaries.is_empty() {
        return vec![Band {
            lo: page_lo,
            hi: page_hi,
        }];
    }

    // A reliable multi-column layout needs both bands actually populated; a
    // single straggler shouldn't fabricate a column. Build the bands, then keep
    // the split only if every band holds ≥ 2 lines (otherwise one column).
    let mut edges = vec![page_lo];
    edges.extend(boundaries.iter().copied());
    edges.push(page_hi);
    let mut bands: Vec<Band> = edges
        .windows(2)
        .map(|w| Band { lo: w[0], hi: w[1] })
        .collect();

    let populated = |b: &Band| lines.iter().filter(|l| b.contains(l.center_x())).count();
    if bands.iter().any(|b| populated(b) < 2) {
        bands = vec![Band {
            lo: page_lo,
            hi: page_hi,
        }];
    }
    bands
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::style::TextStyle;
    use crate::recon::lines::group_into_lines;
    use crate::recon::ReconRun;

    fn run(text: &str, x: f64, y: f64) -> ReconRun {
        ReconRun {
            text: text.to_string(),
            x,
            y,
            w: 100.0,
            h: 12.0,
            size: 12.0,
            style: TextStyle::default(),
            rotation: 0.0,
            source_index: None,
            underline: false,
        }
    }

    #[test]
    fn single_column_keeps_top_to_bottom_order() {
        let runs = vec![
            run("a", 72.0, 700.0),
            run("b", 72.0, 680.0),
            run("c", 72.0, 660.0),
        ];
        let lines = group_into_lines(&runs);
        let order = reading_order(&lines);
        assert_eq!(order, vec![0, 1, 2]);
    }

    #[test]
    fn two_columns_read_left_band_fully_before_right() {
        // A real two-column layout: the columns' baselines are staggered (they do
        // not share a y across the gutter), so each line belongs to one column.
        // Left column x≈72 (a1,a2), right column x≈360 (b1,b2). Reading order
        // must drain the left band top→bottom before the right band.
        let runs = vec![
            run("a1", 72.0, 700.0),
            run("b1", 360.0, 690.0),
            run("a2", 72.0, 680.0),
            run("b2", 360.0, 670.0),
        ];
        let lines = group_into_lines(&runs);
        assert_eq!(lines.len(), 4, "staggered columns stay four separate lines");
        let order = reading_order(&lines);
        let ordered: Vec<String> = order.iter().map(|&i| lines[i].text()).collect();
        assert_eq!(ordered, vec!["a1", "a2", "b1", "b2"]);
    }

    #[test]
    fn a_single_straggler_does_not_fabricate_a_column() {
        // One far-right run on its own baseline is not enough to make a real
        // second column (a band needs ≥ 2 lines), so the page stays single
        // column and the lines keep their top→bottom order.
        let runs = vec![
            run("body one", 72.0, 700.0),
            run("body two", 72.0, 680.0),
            run("body three", 72.0, 660.0),
            run("x", 400.0, 640.0),
        ];
        let lines = group_into_lines(&runs);
        assert_eq!(lines.len(), 4);
        let order = reading_order(&lines);
        // Falls back to single column (top→bottom input order).
        assert_eq!(order, vec![0, 1, 2, 3]);
    }
}
