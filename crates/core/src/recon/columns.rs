//! Stage 2 — **column detection & reading order**. A multi-column page (news,
//! academic two-up) must be read column-by-column, not row-by-row, or the text
//! interleaves into nonsense. We project the lines' horizontal extents onto the
//! X axis, find the *vertical gutters* — bands of X that almost no body line
//! covers — and order each column's lines top→bottom, left band first.
//!
//! Three refinements over a naïve projection (epic #74 wave R7, hardened for #5):
//!
//! 1. **Gutter detection tolerates gutter-spanning lines.** The fatal failure of
//!    a plain whitespace projection is that *one* wide line crossing the gutter —
//!    a heading, a pull-quote, a figure caption, a stray run — bridges the two
//!    columns' X spans and collapses the page to a single column, interleaving
//!    the bodies into garbage. So the gutter is taken from a **robust majority**
//!    of the lines, not a unanimous vote: a body line far wider than the typical
//!    column line (≥ `SPAN_WIDTH_MULT`× the median body width, and a real share of
//!    the measure — `SPAN_MIN_WIDTH_FRAC`) is set aside as a probable spanner
//!    *before* projecting, so a handful of wide lines no longer veto an otherwise
//!    empty corridor. The gutter is then the empty band in a coverage
//!    step-function over the *narrow* lines, located directly rather than by
//!    merging intervals where a single bridge would weld two columns into one. A
//!    genuinely sparse column keeps its ordinary-width lines, so it is never
//!    mistaken for a gutter (see `column_bands`).
//! 2. **Full-width and gutter-spanning lines become region breaks, not column
//!    members.** A title/banner covering ≥ [`FULL_WIDTH_FRAC`] of the measure —
//!    *and* any narrower body line that still straddles a detected gutter — is
//!    folded back in **at its Y** as a separator: everything above it is read
//!    (column-major) before it, everything below after it, and the line itself is
//!    ranked **after every column of its region** (deterministically, not by the
//!    accident of which band its centre lands in). A cross-column heading is
//!    therefore read first, then the left column, then the right; a mid-page
//!    spanning figure splits the column flow around it.
//! 3. **N columns, uneven fills.** The split generalises to any number of
//!    gutters, and a column that holds only a line or two no longer collapses the
//!    whole layout — the split stands as long as ≥ 2 bands are clearly populated
//!    (see [`column_bands`]).
//!
//! A gutter-free page (the overwhelmingly common case) yields a single column:
//! the lines are returned in their existing top→bottom order, unchanged.

use super::lines::ReconLine;

/// A line counts as **full width** (a title/banner spanning the measure, not a
/// column body line) when it covers at least this fraction of the content width.
const FULL_WIDTH_FRAC: f64 = 0.80;

/// A body line is set aside as a probable **gutter spanner** (a heading,
/// pull-quote, wide caption or stray run too narrow to be caught by
/// [`FULL_WIDTH_FRAC`] yet still crossing the corridor) when it is at least this
/// multiple of the *median body-line width* — and meaningfully wide in absolute
/// terms (see [`SPAN_MIN_WIDTH_FRAC`]). Removing these before the gutter
/// projection is what makes detection robust to the classic "one wide line
/// bridges the gutter and collapses two columns into one" failure, **without**
/// eroding a genuinely sparse column (whose lines are of ordinary width and so
/// are kept). The spanners are folded back in as region separators afterwards.
const SPAN_WIDTH_MULT: f64 = 1.8;

/// Floor on a gutter spanner's width as a fraction of the content measure: a line
/// barely wider than its neighbours on a narrow-bodied page is not a spanner. A
/// spanner must cover a real share of the page to plausibly cross a gutter.
const SPAN_MIN_WIDTH_FRAC: f64 = 0.45;

/// A line is treated as **spanning** a gutter (hence a region break, not a column
/// member) when it overlaps two or more bands by at least this many points each —
/// a margin that ignores a glyph that merely grazes a boundary but catches any
/// line genuinely straddling the corridor.
const SPAN_MARGIN: f64 = 4.0;

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

    /// How many column bands the X interval `[x0, x1]` substantially overlaps
    /// (each by ≥ [`SPAN_MARGIN`] points). A normal column line overlaps exactly
    /// one band; a gutter-spanning line (a heading, a wide figure caption) two or
    /// more — that is how a sub-full-width line is still recognised as a region
    /// break rather than wrongly folded into one column.
    fn bands_overlapped(&self, x0: f64, x1: f64) -> usize {
        if self.bands.len() < 2 {
            return 1;
        }
        let mut n = 0;
        for w in self.bands.windows(2) {
            let lo = x0.max(w[0]);
            let hi = x1.min(w[1]);
            if hi - lo >= SPAN_MARGIN {
                n += 1;
            }
        }
        n
    }

    /// Whether `line` acts as a **region separator**: a full-width title/banner
    /// (≥ [`FULL_WIDTH_FRAC`] of the band span) *or* a narrower line that still
    /// straddles a gutter (overlaps ≥ 2 bands). Only meaningful on a multi-column
    /// page; on a single column nothing separates regions.
    fn line_is_separator(&self, line: &ReconLine) -> bool {
        if !self.is_multi_column() {
            return false;
        }
        let span = (self.bands.last().copied().unwrap_or(0.0)
            - self.bands.first().copied().unwrap_or(0.0))
        .max(1.0);
        line.w >= span * FULL_WIDTH_FRAC || self.bands_overlapped(line.x, line.x + line.w) >= 2
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

    /// The reading-order key for a whole **line**. Identical to
    /// [`rank`](ColumnLayout::rank) for an ordinary column line, but a region
    /// **separator** (a full-width or gutter-spanning line) is forced into a
    /// sentinel band past every real column so it sorts *after* all columns of
    /// its region — deterministically, instead of relying on the accident of
    /// which band its centre happens to fall in.
    fn line_rank(&self, line: &ReconLine) -> (usize, usize, OrderedF64) {
        let (region, band, ydist) = self.rank(line.center_x(), line.top());
        if self.line_is_separator(line) {
            // `bands.len()` is one past the largest real band index, so a
            // separator outranks every column of its region while keeping the
            // region- and Y-ordering intact.
            (region, self.bands.len(), ydist)
        } else {
            (region, band, ydist)
        }
    }

    /// Order `lines` by [`line_rank`](ColumnLayout::line_rank). A stable sort over
    /// the input indices: ties (same region, band and Y) keep input order.
    pub fn order_lines(&self, lines: &[ReconLine]) -> Vec<usize> {
        // Fast path: a gutter-free, separator-free page keeps the stage-1 order
        // (already top→bottom) with no work.
        if !self.is_multi_column() && self.separators.is_empty() {
            return (0..lines.len()).collect();
        }
        let mut idxs: Vec<usize> = (0..lines.len()).collect();
        idxs.sort_by(|&a, &b| self.line_rank(&lines[a]).cmp(&self.line_rank(&lines[b])));
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

    // Full-width lines (titles/banners) are set aside up front: they must neither
    // dominate the gutter projection nor be assigned to one column. The remaining
    // *body* lines are what the gutters are found from — but, crucially, body
    // lines that still straddle the gutter are now **tolerated** by the detector
    // (a robust majority, not a unanimous vote) rather than collapsing it.
    let is_full_width = |l: &ReconLine| l.w >= content_w * FULL_WIDTH_FRAC;
    let body: Vec<&ReconLine> = lines.iter().filter(|l| !is_full_width(l)).collect();

    let bands = column_bands(&body, content_lo, content_hi);

    // Build the provisional layout so separator classification can use the final
    // band edges (a separator is a full-width *or* a gutter-spanning line, and the
    // latter can only be known once the bands exist).
    let mut layout = ColumnLayout {
        bands,
        separators: Vec::new(),
        page_top,
    };

    // Region separators: every line that acts as a full-width banner or that
    // straddles a detected gutter, recorded at its top edge. Only meaningful when
    // the body actually split into columns — `line_is_separator` returns `false`
    // for a single-column page, so a wide paragraph line on a one-column page is
    // not turned into a spurious region break.
    let mut separators: Vec<f64> = lines
        .iter()
        .filter(|l| layout.line_is_separator(l))
        .map(|l| l.top())
        .collect();
    separators.sort_by(|a, b| b.partial_cmp(a).unwrap_or(core::cmp::Ordering::Equal));
    separators.dedup_by(|a, b| (*a - *b).abs() < f64::EPSILON);
    layout.separators = separators;

    layout
}

/// Split the body's X span into column bands by finding gutters: vertical bands
/// that **almost no** body line covers — wide enough to be real column
/// separators rather than inter-word spacing — using a robust-majority test so a
/// few gutter-spanning lines do not weld two columns into one.
/// `content_lo`/`content_hi` bound the page measure so the outer bands reach the
/// page edges even when the body stops short.
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

    // Robustness to gutter-spanning lines: a body line far wider than the typical
    // column line (and a real share of the measure) is a probable spanner — a
    // heading or pull-quote too narrow for [`FULL_WIDTH_FRAC`] yet still crossing
    // the corridor. Project gutters from the *narrow* lines only, so one such line
    // can no longer weld two columns into one. A genuinely sparse column keeps its
    // ordinary-width lines and therefore still registers as ink between gutters.
    let content_w = (content_hi - content_lo).max(1.0);
    let mut widths: Vec<f64> = body.iter().map(|l| l.w.max(1.0)).collect();
    let w_med = super::median(&mut widths, content_w);
    let span_w = (w_med * SPAN_WIDTH_MULT).max(content_w * SPAN_MIN_WIDTH_FRAC);
    let narrow: Vec<&&ReconLine> = body.iter().filter(|l| l.w < span_w).collect();
    // If filtering would leave too little to judge a gutter, fall back to the full
    // body (the page is probably all wide lines = single column anyway).
    let proj: &[&&ReconLine] = if narrow.len() >= 2 { &narrow } else { &[] };
    let proj_lines: Vec<&ReconLine> = if proj.is_empty() {
        body.to_vec()
    } else {
        proj.iter().map(|l| **l).collect()
    };

    // Coverage step-function over X: +1 at each projection line's left edge, −1 at
    // its right edge. Sweeping the sorted events yields, for every X segment
    // between consecutive event positions, how many lines cover it. A gutter is a
    // maximal **empty** run (coverage 0) interior to the ink span; spanners having
    // been removed, an empty corridor is no longer bridged by a single wide line.
    let mut events: Vec<(f64, i32)> = Vec::with_capacity(proj_lines.len() * 2);
    for l in &proj_lines {
        events.push((l.x, 1));
        events.push((l.x + l.w, -1));
    }
    events.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(core::cmp::Ordering::Equal));

    let ink_lo = proj_lines.iter().map(|l| l.x).fold(f64::INFINITY, f64::min);
    let ink_hi = proj_lines
        .iter()
        .map(|l| l.x + l.w)
        .fold(f64::NEG_INFINITY, f64::max);
    let page_lo = ink_lo.min(content_lo);
    let page_hi = ink_hi.max(content_hi);

    // Walk the segments, accumulating coverage, and grow a maximal empty run
    // (coverage 0) until ink resumes. An empty run strictly *interior* to the ink
    // span (content on both sides) and at least `min_gutter` wide is a gutter; its
    // midpoint is the column boundary. The leading/trailing empty runs (page
    // margins) are not interior, so they never become gutters.
    let mut boundaries: Vec<f64> = Vec::new();
    let mut coverage: i32 = 0;
    let mut gutter_start: Option<f64> = None;
    for w in events.windows(2) {
        coverage += w[0].1;
        let seg_lo = w[0].0;
        if coverage == 0 {
            // Start (or continue) an empty run at the first empty segment that is
            // interior — i.e. begins at or after the first ink position.
            if gutter_start.is_none() && seg_lo >= ink_lo {
                gutter_start = Some(seg_lo);
            }
        } else if let Some(start) = gutter_start.take() {
            // Ink resumed: close the run at this segment's start. Interior by
            // construction (ink follows), so test only its width.
            if seg_lo - start >= min_gutter {
                boundaries.push((start + seg_lo) / 2.0);
            }
        }
        // A run still open at the final event runs into the trailing margin (no
        // ink to the right) and is therefore not a gutter.
    }

    if boundaries.is_empty() {
        return vec![page_lo, page_hi];
    }

    let mut edges = vec![page_lo];
    edges.extend(boundaries.iter().copied());
    edges.push(page_hi);

    // A reliable multi-column layout needs at least two **clearly populated**
    // columns (≥ 2 body lines centred in them). A single straggler band must not
    // fabricate a column, but a genuine N-column page with one sparse column must
    // survive, so we gate on the *count* of well-populated bands, not on every
    // band. Gutter-spanning lines are centred near a boundary; whichever band
    // their centre lands in, the gate still needs two *other* lines per surviving
    // column, so an outlier cannot prop up a column on its own.
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
        // Single-gutter fallback (gap #75, sub-item 4). A two-column page whose
        // second column is *short* (a sidebar, a stub, a one-line note) has only
        // one well-populated band, so the symmetric ≥2/≥2 gate would wrongly
        // collapse it to one column. When there is exactly one clear gutter, accept
        // the split if one side is well-populated (≥ 2 lines) and the sparse side's
        // line(s) sit **beside** it — their baselines fall within the dominant
        // column's Y span. That vertical coexistence is what tells a genuine
        // adjacent column from the lone straggler the ≥2 gate rightly rejects (a
        // stray run that merely lands above or below the body, not beside it).
        if boundaries.len() == 1 {
            if let Some(edges) = single_gutter_split(body, &edges, h_med) {
                return edges;
            }
        }
        return vec![page_lo, page_hi];
    }

    edges
}

/// The robust single-gutter two-column fallback (see [`column_bands`]). `edges`
/// is the candidate `[lo, mid, hi]`; returns it unchanged when the split is
/// trustworthy on sparse evidence (one well-populated column plus a sparse column
/// sitting beside it), else `None` to fall back to a single column. `tol` is one
/// line height of vertical slack.
fn single_gutter_split(body: &[&ReconLine], edges: &[f64], tol: f64) -> Option<Vec<f64>> {
    let (lo, mid, hi) = (edges[0], edges[1], *edges.last()?);
    let band_ys = |a: f64, b: f64| -> Vec<f64> {
        body.iter()
            .filter(|l| {
                let cx = l.center_x();
                cx >= a && cx < b
            })
            .map(|l| l.center_y())
            .collect::<Vec<_>>()
    };
    let left = band_ys(lo, mid);
    let right = band_ys(mid, hi);
    // The dominant (more populated) band anchors the column; the other is the
    // candidate sparse column.
    let (dom, sparse) = if left.len() >= right.len() {
        (&left, &right)
    } else {
        (&right, &left)
    };
    if dom.len() < 2 || sparse.is_empty() {
        return None;
    }
    let dom_lo = dom.iter().cloned().fold(f64::INFINITY, f64::min);
    let dom_hi = dom.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let beside = sparse
        .iter()
        .all(|&y| y >= dom_lo - tol && y <= dom_hi + tol);
    beside.then(|| edges.to_vec())
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
    fn two_columns_with_a_short_right_column_still_splits() {
        // A real two-column page whose right column is a single line that sits
        // *beside* the left column (its baseline within the left column's Y span).
        // The symmetric ≥2/≥2 gate alone would collapse this; the single-gutter
        // fallback (gap #75 #4) keeps the split — distinct from the straggler case
        // (a lone run *below* the body), which still stays single column.
        let runs = vec![
            run("left one", 72.0, 700.0),
            run("right note", 360.0, 690.0), // lone right line, beside the left column
            run("left two", 72.0, 680.0),
            run("left three", 72.0, 660.0),
        ];
        let lines = group_into_lines(&runs);
        let order = reading_order(&lines);
        let ordered: Vec<String> = order.iter().map(|&i| lines[i].text()).collect();
        assert_eq!(
            ordered,
            vec!["left one", "left two", "left three", "right note"],
            "short right column still splits and is read after the left",
        );
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

    #[test]
    fn sub_full_width_heading_bridging_gutter_keeps_two_columns() {
        // The regression this issue targets: a heading that bridges the gutter but
        // is **not** wide enough to trip `FULL_WIDTH_FRAC` (here 290 pt over a
        // 408 pt measure ≈ 0.71 < 0.80). A naïve projection merges the left and
        // right column X spans through it and collapses the page to one column,
        // interleaving the bodies. Robust detection sets the wide bridge aside,
        // keeps the gutter, reads the heading first, then the whole left column
        // top→bottom, then the whole right column — never interleaved.
        let runs = vec![
            // Heading bridges columns (x 72 → 362, width 290) above everything.
            run_w("HEADING", 72.0, 740.0, 290.0),
            // Left column x=72 w=120 ([72,192]); right column x=360 w=120
            // ([360,480]); staggered baselines so each run stays its own line.
            run_w("left one", 72.0, 700.0, 120.0),
            run_w("right one", 360.0, 690.0, 120.0),
            run_w("left two", 72.0, 680.0, 120.0),
            run_w("right two", 360.0, 670.0, 120.0),
            run_w("left three", 72.0, 660.0, 120.0),
            run_w("right three", 360.0, 650.0, 120.0),
        ];
        let lines = group_into_lines(&runs);
        let layout = column_layout(&lines);
        assert!(
            layout.is_multi_column(),
            "the sub-full-width bridge must not collapse the two columns"
        );
        let order = reading_order(&lines);
        let ordered: Vec<String> = order.iter().map(|&i| lines[i].text()).collect();
        assert_eq!(
            ordered,
            vec![
                "HEADING",
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
    fn genuine_single_column_with_one_wide_line_stays_single() {
        // A one-column page whose lines vary in width (a wide line among narrower
        // ones — a long paragraph line, a sub-heading). There is no gutter at all,
        // so the page must stay single column and keep its top→bottom order; the
        // wide line must not be mistaken for a column separator that reorders the
        // flow.
        let runs = vec![
            run_w("a normal body line", 72.0, 700.0, 300.0),
            run_w("a much wider full paragraph line here", 72.0, 680.0, 430.0),
            run_w("another body line", 72.0, 660.0, 280.0),
            run_w("short", 72.0, 640.0, 90.0),
            run_w("final body line", 72.0, 620.0, 260.0),
        ];
        let lines = group_into_lines(&runs);
        let layout = column_layout(&lines);
        assert!(
            !layout.is_multi_column(),
            "no gutter exists, so the page must stay single column"
        );
        let order = reading_order(&lines);
        assert_eq!(order, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn mid_page_spanning_figure_splits_the_column_flow() {
        // Two-column body interrupted by a wide figure caption that straddles the
        // gutter halfway down (again sub-`FULL_WIDTH_FRAC`: 290 pt of a 408 pt
        // measure). The flow must split around it: top block's left then right
        // column, then the figure, then the bottom block's left then right column —
        // not one interleaved stream.
        let runs = vec![
            // Top region, two columns.
            run_w("topL1", 72.0, 760.0, 120.0),
            run_w("topR1", 360.0, 745.0, 120.0),
            run_w("topL2", 72.0, 730.0, 120.0),
            run_w("topR2", 360.0, 715.0, 120.0),
            // Wide figure caption bridging the gutter mid-page (x 72 → 362).
            run_w("FIGURE", 72.0, 690.0, 290.0),
            // Bottom region, two columns.
            run_w("botL1", 72.0, 660.0, 120.0),
            run_w("botR1", 360.0, 645.0, 120.0),
            run_w("botL2", 72.0, 630.0, 120.0),
            run_w("botR2", 360.0, 615.0, 120.0),
        ];
        let lines = group_into_lines(&runs);
        let layout = column_layout(&lines);
        assert!(layout.is_multi_column(), "the body splits into two columns");
        let order = reading_order(&lines);
        let ordered: Vec<String> = order.iter().map(|&i| lines[i].text()).collect();
        assert_eq!(
            ordered,
            vec!["topL1", "topL2", "topR1", "topR2", "FIGURE", "botL1", "botL2", "botR1", "botR2",],
        );
    }
}
