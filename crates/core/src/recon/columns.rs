//! Stage 2 — **column detection & reading order**. A multi-column page (news,
//! academic two-up) must be read column-by-column, not row-by-row, or the text
//! interleaves into nonsense. We project the lines' horizontal extents onto the
//! X axis, find the *vertical gutters* — bands of X with no body line covering
//! them that split the content into columns — and order each column's lines
//! top→bottom, left band first.
//!
//! Two refinements over a naïve projection (epic #74, wave R7):
//!
//! 1. **Full-width lines are excluded from gutter detection.** A title, banner
//!    or wide figure caption that spans both columns would otherwise bridge the
//!    gutter in the merged X projection and collapse the page to a single
//!    column — interleaving the two columns' body text. Such lines (covering
//!    ≥ [`FULL_WIDTH_FRAC`] of the content width) are set aside, the gutters are
//!    found from the *body* lines only, then the full-width lines are folded
//!    back in **at their Y** as region separators: everything above a full-width
//!    line is read (column-major) before the line, everything below after it. A
//!    cross-column title is therefore read first, then the left column, then the
//!    right.
//! 2. **N columns, uneven fills.** The split generalises to any number of
//!    gutters, and a column that holds only a line or two no longer collapses
//!    the whole layout — the split stands as long as ≥ 2 bands are clearly
//!    populated (see [`column_bands`]).
//!
//! A gutter-free page (the overwhelmingly common case) yields a single column:
//! the lines are returned in their existing top→bottom order, unchanged.

use super::lines::ReconLine;

/// A line counts as **full width** (a title/banner spanning the measure, not a
/// column body line) when it covers at least this fraction of the content width.
const FULL_WIDTH_FRAC: f64 = 0.80;

/// Produce the reading order of `lines` as a list of indices into `lines`.
/// Single column ⇒ identity-ish (top→bottom). Multiple columns ⇒ region by
/// region (split at full-width lines), each region's bands left→right, each
/// band's lines top→bottom.
pub fn reading_order(lines: &[ReconLine]) -> Vec<usize> {
    column_layout(lines).order_lines(lines)
}

/// The recovered column structure of a page: the vertical **regions** (delimited
/// by full-width lines) and, for the body region(s), the **column bands**.
///
/// Besides line ordering ([`order`](ColumnLayout::order)) it exposes a
/// [`rank`](ColumnLayout::rank) for an arbitrary `(center_x, top_y)` point, so
/// non-line placeables (shapes, images) can be slotted into the *same*
/// reading order rather than appended after all the text.
#[derive(Debug, Clone)]
pub struct ColumnLayout {
    /// Column edges in X (points), ascending. `bands.len() - 1` columns. Always
    /// at least `[page_lo, page_hi]` (one band = single column).
    bands: Vec<f64>,
    /// Y coordinates (points) of full-width separators, **descending** (top of
    /// page first). A point's region index is the count of separators strictly
    /// above it; the separators themselves sort just before the region below.
    separators: Vec<f64>,
    /// The page's full content Y span (top edge), so rank values stay positive
    /// and monotonic top→bottom.
    page_top: f64,
}

impl ColumnLayout {
    /// Whether the page actually splits into more than one column.
    fn is_multi_column(&self) -> bool {
        self.bands.len() > 2
    }

    /// The band index (0-based, left→right) a horizontal centre falls in. Points
    /// left of the first edge map to band 0, right of the last to the last band.
    fn band_of(&self, center_x: f64) -> usize {
        // edges = bands; a point sits in band k when edges[k] ≤ x < edges[k+1].
        for k in 0..self.bands.len().saturating_sub(1) {
            if center_x < self.bands[k + 1] {
                return k;
            }
        }
        self.bands.len().saturating_sub(2).max(0)
    }

    /// The region index (0 = above every separator) a top-Y falls in. A larger
    /// Y is higher on the page, so the region index counts separators *above*
    /// the point (whose Y is greater).
    fn region_of(&self, top_y: f64) -> usize {
        self.separators.iter().filter(|&&s| s > top_y).count()
    }

    /// A monotonic reading-order key for a placeable at `(center_x, top_y)`
    /// (PDF user space). Ordering is region-major, then band (column) left→right,
    /// then top→bottom within a column. A single-column page degenerates to pure
    /// top→bottom (region 0, band 0), so nothing is reordered.
    ///
    /// The key is a tuple, compared lexicographically; the final component is the
    /// downward distance from the page top so larger-Y (higher) sorts first.
    pub fn rank(&self, center_x: f64, top_y: f64) -> (usize, usize, OrderedF64) {
        let region = self.region_of(top_y);
        // Within a region columns matter only when the page is multi-column;
        // otherwise force band 0 so a single column is strictly top→bottom.
        let band = if self.is_multi_column() {
            self.band_of(center_x)
        } else {
            0
        };
        (region, band, OrderedF64(self.page_top - top_y))
    }

    /// Order `lines` by [`rank`](ColumnLayout::rank). A stable sort over the
    /// input indices: ties (same region, band and Y) keep input order.
    pub fn order_lines(&self, lines: &[ReconLine]) -> Vec<usize> {
        // Fast path: a gutter-free, separator-free page keeps the stage-1 order
        // (already top→bottom) with no work.
        if !self.is_multi_column() && self.separators.is_empty() {
            return (0..lines.len()).collect();
        }
        let mut idxs: Vec<usize> = (0..lines.len()).collect();
        idxs.sort_by(|&a, &b| {
            self.rank(lines[a].center_x(), lines[a].top())
                .cmp(&self.rank(lines[b].center_x(), lines[b].top()))
        });
        idxs
    }
}

/// Compute the [`ColumnLayout`] of a page from its lines: detect full-width
/// separators, then column gutters over the remaining body lines.
pub fn column_layout(lines: &[ReconLine]) -> ColumnLayout {
    if lines.len() < 2 {
        return ColumnLayout {
            bands: Vec::new(),
            separators: Vec::new(),
            page_top: lines.first().map(|l| l.top()).unwrap_or(0.0),
        };
    }

    // Content X span over *all* lines (the measure a title is judged against).
    let content_lo = lines.iter().map(|l| l.x).fold(f64::INFINITY, f64::min);
    let content_hi = lines
        .iter()
        .map(|l| l.x + l.w)
        .fold(f64::NEG_INFINITY, f64::max);
    let content_w = (content_hi - content_lo).max(1.0);
    let page_top = lines
        .iter()
        .map(|l| l.top())
        .fold(f64::NEG_INFINITY, f64::max);

    // Full-width lines (titles/banners) are set aside: they must neither bridge
    // the gutter in the body projection nor be assigned to one column. The rest
    // are the body lines the gutters are found from.
    let is_full_width = |l: &ReconLine| l.w >= content_w * FULL_WIDTH_FRAC;
    let body: Vec<&ReconLine> = lines.iter().filter(|l| !is_full_width(l)).collect();

    let bands = column_bands(&body, content_lo, content_hi);

    // Separators are the full-width lines' top edges — but only meaningful when
    // the body actually splits into columns. On a single-column page a wide line
    // is just a wide paragraph line; introducing a region break there would be a
    // no-op for ordering anyway (band is forced to 0), so we still record them
    // for rank stability, costing nothing.
    let mut separators: Vec<f64> = lines
        .iter()
        .filter(|l| is_full_width(l))
        .map(|l| l.top())
        .collect();
    separators.sort_by(|a, b| b.partial_cmp(a).unwrap_or(core::cmp::Ordering::Equal));

    ColumnLayout {
        bands,
        separators,
        page_top,
    }
}

/// Split the body's X span into column bands by finding gutters: gaps in the
/// union of the body lines' X intervals wide enough to be real column
/// separators rather than inter-word spacing. `content_lo`/`content_hi` bound
/// the page measure so the outer bands reach the page edges even when the body
/// stops short.
///
/// Returns the column **edges** (ascending): `[lo, b1, b2, …, hi]` with
/// `edges.len() - 1` columns. A single column yields `[lo, hi]`.
fn column_bands(body: &[&ReconLine], content_lo: f64, content_hi: f64) -> Vec<f64> {
    // Need at least a few body lines to trust a gutter; below that, one column.
    if body.len() < 2 {
        return vec![content_lo, content_hi];
    }

    // Calibrate the minimum gutter width to the typography: a gutter must be
    // clearly wider than a normal space — a few times the median line height,
    // floored so tiny pages don't over-split.
    let mut heights: Vec<f64> = body.iter().map(|l| l.h.max(1.0)).collect();
    let h_med = super::median(&mut heights, 10.0);
    let min_gutter = (h_med * 2.0).max(18.0);

    // Merge the body lines' X intervals; gaps between merged blocks are
    // candidate gutters.
    let mut intervals: Vec<(f64, f64)> = body.iter().map(|l| (l.x, l.x + l.w)).collect();
    intervals.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(core::cmp::Ordering::Equal));
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

    let page_lo = merged.first().map(|m| m.0).unwrap_or(content_lo).min(content_lo);
    let page_hi = merged.last().map(|m| m.1).unwrap_or(content_hi).max(content_hi);

    // Each gap between merged blocks wider than `min_gutter` is a column
    // boundary; bands are the spans between consecutive boundaries.
    let mut boundaries: Vec<f64> = Vec::new();
    for w in merged.windows(2) {
        let gap = w[1].0 - w[0].1;
        if gap >= min_gutter {
            boundaries.push((w[0].1 + w[1].0) / 2.0);
        }
    }
    if boundaries.is_empty() {
        return vec![page_lo, page_hi];
    }

    let mut edges = vec![page_lo];
    edges.extend(boundaries.iter().copied());
    edges.push(page_hi);

    // A reliable multi-column layout needs at least two **clearly populated**
    // columns (≥ 2 body lines each). A single straggler band must not fabricate
    // a column, but a genuine N-column page with one sparse column must survive,
    // so we gate on the *count* of well-populated bands, not on every band.
    let populated = |lo: f64, hi: f64| {
        body.iter()
            .filter(|l| {
                let cx = l.center_x();
                cx >= lo && cx < hi
            })
            .count()
    };
    let well_populated = edges
        .windows(2)
        .filter(|w| populated(w[0], w[1]) >= 2)
        .count();
    if well_populated < 2 {
        return vec![page_lo, page_hi];
    }

    edges
}

impl ReconLine {
    /// Horizontal centre of the line.
    pub(crate) fn center_x(&self) -> f64 {
        self.x + self.w / 2.0
    }
}

/// A total-order wrapper over `f64` for use in a sortable rank tuple. PDF
/// coordinates here are finite (NaN can't reach this layer — runs without a
/// computable box are dropped upstream), so a plain `partial_cmp` with an
/// `Equal` fallback is a total order in practice.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OrderedF64(pub f64);

impl Eq for OrderedF64 {}
impl PartialOrd for OrderedF64 {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for OrderedF64 {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.0
            .partial_cmp(&other.0)
            .unwrap_or(core::cmp::Ordering::Equal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::style::TextStyle;
    use crate::recon::lines::group_into_lines;
    use crate::recon::ReconRun;

    fn run(text: &str, x: f64, y: f64) -> ReconRun {
        run_w(text, x, y, 100.0)
    }

    fn run_w(text: &str, x: f64, y: f64, w: f64) -> ReconRun {
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

    #[test]
    fn full_width_title_over_two_columns_reads_title_then_columns() {
        // A two-column body with a TITLE line spanning the full measure at the
        // top. The naïve merged-projection would bridge the gutter through the
        // title and collapse to one column, interleaving the bodies. With
        // full-width exclusion, the title is read first, then the whole left
        // column top→bottom, then the whole right column.
        let runs = vec![
            // Title spans both columns (x 72 → 472, width 400).
            run_w("TITLE", 72.0, 740.0, 400.0),
            // Left column at x≈72, right column at x≈360, staggered baselines.
            run("left one", 72.0, 700.0),
            run("right one", 360.0, 690.0),
            run("left two", 72.0, 680.0),
            run("right two", 360.0, 670.0),
            run("left three", 72.0, 660.0),
            run("right three", 360.0, 650.0),
        ];
        let lines = group_into_lines(&runs);
        let order = reading_order(&lines);
        let ordered: Vec<String> = order.iter().map(|&i| lines[i].text()).collect();
        assert_eq!(
            ordered,
            vec![
                "TITLE",
                "left one",
                "left two",
                "left three",
                "right one",
                "right two",
                "right three",
            ],
        );
    }

    #[test]
    fn three_columns_read_column_by_column() {
        // Three columns at x≈72, 252, 432. Baselines are staggered by more than
        // the line-band tolerance (≈0.6×size) so each column's runs stay distinct
        // lines (stage-1 line grouping bands by baseline alone). Each column must
        // be drained top→bottom, left band first.
        let runs = vec![
            run_w("a1", 72.0, 700.0, 80.0),
            run_w("b1", 252.0, 685.0, 80.0),
            run_w("c1", 432.0, 670.0, 80.0),
            run_w("a2", 72.0, 660.0, 80.0),
            run_w("b2", 252.0, 645.0, 80.0),
            run_w("c2", 432.0, 630.0, 80.0),
        ];
        let lines = group_into_lines(&runs);
        let order = reading_order(&lines);
        let ordered: Vec<String> = order.iter().map(|&i| lines[i].text()).collect();
        assert_eq!(ordered, vec!["a1", "a2", "b1", "b2", "c1", "c2"]);
    }

    #[test]
    fn three_columns_one_sparse_still_splits() {
        // Uneven fills: left & right columns have 2 lines each, the middle only
        // one. The split must still hold (≥ 2 well-populated columns), and the
        // sparse middle column is read in its own band order. Baselines staggered
        // past the band tolerance so no two columns merge into one line.
        let runs = vec![
            run_w("a1", 72.0, 700.0, 80.0),
            run_w("c1", 432.0, 680.0, 80.0),
            run_w("b1", 252.0, 660.0, 80.0), // lone middle line
            run_w("a2", 72.0, 640.0, 80.0),
            run_w("c2", 432.0, 620.0, 80.0),
        ];
        let lines = group_into_lines(&runs);
        let order = reading_order(&lines);
        let ordered: Vec<String> = order.iter().map(|&i| lines[i].text()).collect();
        assert_eq!(ordered, vec!["a1", "a2", "b1", "c1", "c2"]);
    }

    #[test]
    fn full_width_footer_between_partitions_two_column_regions() {
        // Two stacked two-column blocks separated by a full-width banner: the top
        // block's columns are read first, then the banner, then the bottom block's
        // columns. Proves regions partition independently around a separator.
        // Column baselines staggered past the band tolerance so each stays its own
        // line.
        let runs = vec![
            // Top region, two columns.
            run("topL1", 72.0, 760.0),
            run("topR1", 360.0, 745.0),
            run("topL2", 72.0, 730.0),
            run("topR2", 360.0, 715.0),
            // Full-width banner in the middle.
            run_w("BANNER", 72.0, 690.0, 400.0),
            // Bottom region, two columns.
            run("botL1", 72.0, 660.0),
            run("botR1", 360.0, 645.0),
            run("botL2", 72.0, 630.0),
            run("botR2", 360.0, 615.0),
        ];
        let lines = group_into_lines(&runs);
        let order = reading_order(&lines);
        let ordered: Vec<String> = order.iter().map(|&i| lines[i].text()).collect();
        assert_eq!(
            ordered,
            vec![
                "topL1", "topL2", "topR1", "topR2", "BANNER", "botL1", "botL2", "botR1", "botR2",
            ],
        );
    }

    #[test]
    fn rank_slots_a_point_into_its_region_band_and_y() {
        // A two-column page with a top title. A shape sitting in the right column
        // below the title must rank after the title and after a right-column line
        // higher than it, but before a right-column line lower than it.
        let runs = vec![
            run_w("TITLE", 72.0, 740.0, 400.0),
            run("left", 72.0, 690.0),
            run("rightHi", 360.0, 700.0),
            run("rightLo", 360.0, 640.0),
        ];
        let lines = group_into_lines(&runs);
        let layout = column_layout(&lines);
        // A shape centred in the right column at y≈670 (between the two right
        // lines). Its rank must be > rightHi and < rightLo.
        let shape_rank = layout.rank(390.0, 670.0);
        // Find the right-column line ranks.
        let right_hi = lines.iter().find(|l| l.text() == "rightHi").unwrap();
        let right_lo = lines.iter().find(|l| l.text() == "rightLo").unwrap();
        let hi_rank = layout.rank(right_hi.center_x(), right_hi.top());
        let lo_rank = layout.rank(right_lo.center_x(), right_lo.top());
        assert!(hi_rank < shape_rank, "shape sorts after the higher right line");
        assert!(shape_rank < lo_rank, "shape sorts before the lower right line");
        // And the title (region 0) outranks anything in the body region.
        let title = lines.iter().find(|l| l.text() == "TITLE").unwrap();
        let title_rank = layout.rank(title.center_x(), title.top());
        assert!(title_rank < shape_rank, "the title is read before the shape");
    }
}
