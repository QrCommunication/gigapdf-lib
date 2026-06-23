//! Stage 6 — **table reconstruction**. Two paths:
//!
//! 1. **Ruled tables.** Axis-aligned ruling lines (from
//!    [`page_vector_paths`](crate::Document::page_vector_paths), classified by
//!    [`ruling_orientation`](crate::recon::ruling_orientation)) define a cell
//!    grid: the distinct vertical rules give the column edges, the horizontal
//!    rules the row edges. Each text run is dropped into the cell its centre
//!    falls in; empty interior cells become spans of their left neighbour.
//! 2. **Borderless fallback.** With no rules, cluster run start-x into column
//!    anchors and band rows by baseline — but only **promote** to a table when
//!    ≥ 2 rows share ≥ 2 consistent column anchors, so ordinary prose (which
//!    lands in a single column) is never mistaken for a table.
//!
//! [`plan_tables`] decides, up front, which lines and which painted paths each
//! table consumes; `reconstruct_page` then skips those when emitting prose and
//! shapes, and calls [`build_table`] to materialise the [`Table`] block.

use std::collections::BTreeSet;

use super::lines::ReconLine;
use super::{median, ruling_orientation, run_char_style, IdGen, Ruling};
use crate::content::vector::VectorPath;
use crate::model::{
    geom::Rotation, Align, Block, BlockKind, BorderStyle, Cell, Paragraph, ParagraphStyle, Rect,
    Row, Table,
};

/// A table the planner found: its grid edges (PDF user space), the line indices
/// it covers, the painted-path indices that drew its rules, and the line it
/// starts at (smallest covered index, in reading order).
#[derive(Debug, Clone)]
pub struct PlannedTable {
    /// Column edges (X), ascending — `cols.len() - 1` columns.
    cols: Vec<f64>,
    /// Row edges (Y), descending (top first) — `rows.len() - 1` rows.
    rows: Vec<f64>,
    /// Whether the grid came from ruling lines (`true`) or the borderless
    /// fallback (`false`); drives the emitted [`BorderStyle`].
    ruled: bool,
    covered_lines: BTreeSet<usize>,
    used_paths: BTreeSet<usize>,
    start_line: usize,
    /// Raw horizontal rule segments `(y, x0, x1)` that drew the grid (ruled only;
    /// empty for the borderless fallback). Used by [`build_table`] to detect a
    /// **missing interior rule** ⇒ a merged (spanning) cell. Keeping the segments
    /// (not just the clustered edges) is what lets us tell "edge exists but this
    /// row has no rule along it" apart from "edge exists everywhere".
    h_segs: Vec<(f64, f64, f64)>,
    /// Raw vertical rule segments `(x, y0, y1)` (ruled only; empty otherwise).
    v_segs: Vec<(f64, f64, f64)>,
    /// Per-column horizontal alignment, `cols.len() - 1` entries (one per column).
    /// Borderless tables fill this from the detected anchor type (left-edge vs
    /// right-edge clustering); ruled tables leave it `Align::Left` (their cells
    /// are placed by centre and carry no alignment evidence). Drives the cell
    /// [`ParagraphStyle::align`] so a right-aligned numeric column round-trips as
    /// right-aligned. Length is kept in lock-step with `cols` by construction.
    col_align: Vec<Align>,
}

/// The set of tables planned for a page, with helpers `reconstruct_page` uses to
/// interleave tables with prose and shapes.
#[derive(Debug, Clone, Default)]
pub struct TablePlan {
    tables: Vec<PlannedTable>,
}

impl TablePlan {
    /// The table that starts at `line_idx`, if any (cloned, no mutation).
    pub fn take_if_starts_at(&self, line_idx: usize) -> Option<PlannedTable> {
        self.tables
            .iter()
            .find(|t| t.start_line == line_idx)
            .cloned()
    }

    /// Whether `line_idx` is covered by a table but is *not* its start line (so
    /// it should be skipped, not re-emitted).
    pub fn is_consumed(&self, line_idx: usize) -> bool {
        self.tables
            .iter()
            .any(|t| t.start_line != line_idx && t.covered_lines.contains(&line_idx))
    }

    /// Whether painted path `index` was used as a table rule.
    pub fn uses_path(&self, index: usize) -> bool {
        self.tables.iter().any(|t| t.used_paths.contains(&index))
    }
}

/// Plan the page's tables from its lines and painted paths. Ruled tables take
/// precedence; the borderless fallback runs over the lines no ruled table
/// claimed.
pub fn plan_tables(
    lines: &[ReconLine],
    vpaths: &[VectorPath],
    ignore_paths: &BTreeSet<usize>,
) -> TablePlan {
    let mut plan = TablePlan::default();
    if lines.is_empty() {
        return plan;
    }

    // A ruled candidate is only accepted if it looks like a real *data* table
    // and not a **form** layout (dense rules that merely separate fields). When
    // rejected, its lines are left free so they flow back to the normal prose
    // pipeline (headings / paragraphs) instead of being swallowed by a giant,
    // mostly-empty grid. See [`passes_table_sanity`]. Two tables stacked on one
    // page each come back as their own grid (the rules are segmented into vertical
    // bands first), so each is gated and kept independently — neither is lost to a
    // fused englobing grid.
    for t in plan_ruled_all(lines, vpaths, ignore_paths) {
        if passes_table_sanity(&t, lines) {
            plan.tables.push(t);
        }
    }

    // Borderless fallback over lines not already covered (also segmented into
    // vertical regions, one grid per region).
    let claimed: BTreeSet<usize> = plan
        .tables
        .iter()
        .flat_map(|t| t.covered_lines.iter().copied())
        .collect();
    let free: Vec<usize> = (0..lines.len()).filter(|i| !claimed.contains(i)).collect();
    for t in plan_borderless_all(lines, &free) {
        if passes_table_sanity(&t, lines) {
            plan.tables.push(t);
        }
    }
    plan
}

// ── form-vs-table guardrails ─────────────────────────────────────────────────
//
// A cerfa-style **form** is drawn with many short ruling segments that fence off
// input fields. Clustered naively, those rules synthesise a huge grid (e.g.
// 15×47 on one A4 page) whose cells are mostly empty and whose labels/intro
// prose get vacuumed into cells — so the editor can no longer treat that text as
// paragraphs/headings. A genuine *data* table, by contrast, is compact, has few
// columns, and its cells are mostly filled. These geometric thresholds, all
// calibrated against real fixtures (data tables: rib 16×4 @63%, permis 4×8 @31%;
// forms: s3705 15×47 @14% / 6×16 @24%, s1106 17×16 @21% / 18×42 @7%), reject the
// form layouts while preserving the data tables.

/// Hard cap on columns: a real data table rarely exceeds a dozen; a form's
/// field fences explode well past it. Forms here have 16/42/47 columns; the two
/// genuine tables have 4 and 8. `14` leaves head-room for wide-but-real tables.
const MAX_TABLE_COLS: usize = 14;

/// Hard cap on total cells (rows × cols): an A4 page does not hold a 100+-cell
/// data table; that many cells is a form. Genuine tables here are 64 and 32
/// cells; forms are 96 / 272 / 705 / 756. `160` sits clear of both, and catches
/// a tall form that might sneak under the column cap.
const MAX_TABLE_CELLS: usize = 160;

/// Minimum fraction of cells that must carry text. Form grids are mostly empty
/// fences (forms here: 7–24 % filled); data tables are dense (31 % and 63 %).
/// `0.28` sits in the gap, with margin on both sides.
const MIN_FILL_RATIO: f64 = 0.28;

/// Whether a planned grid looks like a real **data table** rather than a
/// **form** layout. Rejects grids that are too wide, too large, or too sparse —
/// any one failure is disqualifying (a form needs only one tell). The rejected
/// grid's text returns to the prose pipeline.
fn passes_table_sanity(table: &PlannedTable, lines: &[ReconLine]) -> bool {
    let n_cols = table.cols.len().saturating_sub(1);
    let n_rows = table.rows.len().saturating_sub(1);
    if n_cols == 0 || n_rows == 0 {
        return false;
    }
    if n_cols > MAX_TABLE_COLS {
        return false;
    }
    if n_cols.saturating_mul(n_rows) > MAX_TABLE_CELLS {
        return false;
    }
    let (filled, total) = cell_fill(table, lines);
    if total == 0 {
        return false;
    }
    (filled as f64) / (total as f64) >= MIN_FILL_RATIO
}

/// Count `(cells_with_text, total_cells)` for a planned grid by dropping every
/// run's centre into its cell — the same placement [`build_table`] uses, so the
/// fill ratio reflects exactly what the materialised table would contain.
fn cell_fill(table: &PlannedTable, lines: &[ReconLine]) -> (usize, usize) {
    let n_cols = table.cols.len().saturating_sub(1);
    let n_rows = table.rows.len().saturating_sub(1);
    let total = n_cols.saturating_mul(n_rows);
    if total == 0 {
        return (0, 0);
    }
    let mut occupied = vec![false; total];
    for &li in &table.covered_lines {
        let Some(line) = lines.get(li) else { continue };
        for run in &line.runs {
            if run.text.trim().is_empty() {
                continue;
            }
            let cx = run.x + run.w / 2.0;
            let cy = run.y + run.h / 2.0;
            if let (Some(c), Some(r)) = (col_of(&table.cols, cx), row_of(&table.rows, cy)) {
                occupied[r * n_cols + c] = true;
            }
        }
    }
    (occupied.iter().filter(|&&o| o).count(), total)
}

/// Build a [`Table`] block from a planned table. Runs are dropped into the cell
/// their centre lands in; empty interior cells are merged left as spans.
pub fn build_table(
    table: &PlannedTable,
    lines: &[ReconLine],
    ids: &mut IdGen,
    to_frame: impl Fn(f64, f64, f64, f64) -> Rect,
) -> Option<Block> {
    let n_cols = table.cols.len().saturating_sub(1);
    let n_rows = table.rows.len().saturating_sub(1);
    if n_cols == 0 || n_rows == 0 {
        return None;
    }

    // Cell text accumulators.
    let mut grid: Vec<Vec<String>> = vec![vec![String::new(); n_cols]; n_rows];
    // Remember a representative char style per (row,col) for the first run seen.
    let mut styles: Vec<Vec<Option<crate::model::CharStyle>>> = vec![vec![None; n_cols]; n_rows];
    // Remember the FIRST run's `source_index` per (row,col). A cell concatenates
    // several content-stream runs into one cell run, so only one index can be
    // carried — the first non-nested run's index makes the cell addressable by
    // the editor's flat `source_index` space (cell → table block, and cell →
    // (row,col) by reverse lookup), without which a host can only address tables
    // positionally. Nested-XObject runs (`source_index = None`) never overwrite a
    // real index.
    let mut src_grid: Vec<Vec<Option<usize>>> = vec![vec![None; n_cols]; n_rows];

    for &li in &table.covered_lines {
        let Some(line) = lines.get(li) else { continue };
        for run in &line.runs {
            let cx = run.x + run.w / 2.0;
            let cy = run.y + run.h / 2.0;
            let (Some(c), Some(r)) = (col_of(&table.cols, cx), row_of(&table.rows, cy)) else {
                continue;
            };
            let t = run.text.trim();
            if t.is_empty() {
                continue;
            }
            let cell = &mut grid[r][c];
            if !cell.is_empty() {
                cell.push(' ');
            }
            cell.push_str(t);
            if styles[r][c].is_none() {
                styles[r][c] = Some(run_char_style(run));
            }
            if src_grid[r][c].is_none() {
                src_grid[r][c] = run.source_index;
            }
        }
    }

    // Column widths from the edges.
    let col_widths: Vec<f64> = table.cols.windows(2).map(|w| (w[1] - w[0]).abs()).collect();

    // ── merged-cell inference (ruled tables only) ────────────────────────────
    //
    // A grid edge in `cols`/`rows` exists because *some* rule lies along it, but a
    // **merged** cell is one where the interior rule along its boundary is
    // *missing for those particular rows/columns* (e.g. an invoice header that
    // spans two columns leaves no vertical divider in the header row). We grow a
    // span out of a slot only while the interior rule is provably **absent** — a
    // fully-ruled data table has every interior edge drawn, so nothing merges and
    // the output is byte-for-byte the previous 1×1 grid. Borderless tables carry
    // no segments, so this never fires there.
    let infer_spans = table.ruled && (!table.h_segs.is_empty() || !table.v_segs.is_empty());

    // `covered[r][c]` = this slot was absorbed by a span anchored above/left, so
    // it is not emitted as its own cell.
    let mut covered = vec![vec![false; n_cols]; n_rows];
    // Per-anchor computed spans (1×1 unless inference grows them).
    let mut span = vec![vec![(1usize, 1usize); n_cols]; n_rows];

    if infer_spans {
        for r in 0..n_rows {
            for c in 0..n_cols {
                if covered[r][c] {
                    continue;
                }
                // Grow the column span: extend right while the vertical edge
                // between the current block and the next column is absent across
                // row r's full height. Stop at the first present divider.
                let mut cspan = 1;
                while c + cspan < n_cols && !vrule_present(table, table.cols[c + cspan], r, r + 1) {
                    cspan += 1;
                }
                // Grow the row span: extend down while, for **every** column the
                // block already covers, the horizontal edge below the current
                // block is absent. A single present segment anywhere along the
                // boundary blocks the merge (keeps the grid honest).
                let mut rspan = 1;
                while r + rspan < n_rows
                    && (c..c + cspan)
                        .all(|cc| !hrule_present(table, table.rows[r + rspan], cc, cc + 1))
                {
                    rspan += 1;
                }
                span[r][c] = (cspan, rspan);
                // Mark the absorbed slots covered, and fold their text/style into
                // the anchor so nothing a span swallows is lost.
                for rr in r..r + rspan {
                    for cc in c..c + cspan {
                        if rr == r && cc == c {
                            continue;
                        }
                        covered[rr][cc] = true;
                        let absorbed = std::mem::take(&mut grid[rr][cc]);
                        if !absorbed.is_empty() {
                            let anchor = &mut grid[r][c];
                            if !anchor.is_empty() {
                                anchor.push(' ');
                            }
                            anchor.push_str(&absorbed);
                        }
                        if styles[r][c].is_none() {
                            styles[r][c] = styles[rr][cc].take();
                        }
                        if src_grid[r][c].is_none() {
                            src_grid[r][c] = src_grid[rr][cc].take();
                        }
                    }
                }
            }
        }
    }

    let mut rows: Vec<Row> = Vec::with_capacity(n_rows);
    for r in 0..n_rows {
        // Row edges are top→bottom (descending Y); height is the gap.
        let height = (table.rows[r] - table.rows[r + 1]).abs();
        let mut cells: Vec<Cell> = Vec::with_capacity(n_cols);
        for c in 0..n_cols {
            // A slot absorbed by a span anchored above/left is not emitted: the
            // row supplies fewer cells, which is exactly how the model (and the
            // DOCX/ODT exporters) express a merge — see `Cell::col_span`.
            if covered[r][c] {
                continue;
            }
            // Empty interior cells stay empty cells (an unfilled span renders as
            // blank), so the grid shape is preserved rather than collapsed.
            let text = std::mem::take(&mut grid[r][c]);
            let style = styles[r][c].take();
            let src = src_grid[r][c].take();
            let (cspan, rspan) = span[r][c];
            // Per-column alignment (borderless detection sets `Right` for numeric
            // columns); absent ⇒ left. A spanning cell takes its anchor column's
            // alignment.
            let align = table.col_align.get(c).copied().unwrap_or(Align::Left);
            cells.push(make_cell_spanned(
                text,
                style,
                src,
                cspan as u16,
                rspan as u16,
                align,
                ids,
            ));
        }
        rows.push(Row {
            cells,
            height: Some(height),
        });
    }

    // Frame = the grid extent.
    let x0 = *table.cols.first()?;
    let x1 = *table.cols.last()?;
    let y_top = *table.rows.first()?;
    let y_bot = *table.rows.last()?;
    let frame = to_frame(x0, y_bot, x1 - x0, y_top - y_bot);

    let border = if table.ruled {
        BorderStyle {
            width: 1.0,
            color: [0.0, 0.0, 0.0],
        }
    } else {
        BorderStyle::default()
    };

    Some(Block {
        id: ids.mint(),
        frame: Some(frame),
        rotation: Rotation::D0,
        kind: BlockKind::Table(Table {
            rows,
            col_widths,
            border,
        }),
    })
}

/// Materialise one [`Cell`] holding `text` (one paragraph run) with the given
/// spans. A 1×1 cell is the common case; `col_span`/`row_span` > 1 mark a merged
/// region whose absorbed slots were dropped from their rows (the merge encoding
/// the model and exporters expect). `source_index` is the first content-stream
/// run index seen in the cell (or `None` for an empty / nested-only cell), which
/// makes the cell addressable by a host's flat `source_index` space.
fn make_cell_spanned(
    text: String,
    style: Option<crate::model::CharStyle>,
    source_index: Option<usize>,
    col_span: u16,
    row_span: u16,
    align: Align,
    ids: &mut IdGen,
) -> Cell {
    use crate::model::{Inline, InlineRun};
    let runs = if text.is_empty() {
        Vec::new()
    } else {
        vec![Inline::Run(InlineRun {
            text,
            style: style.unwrap_or_default(),
            source_index,
        })]
    };
    let para = Block {
        id: ids.mint(),
        frame: None,
        rotation: Rotation::D0,
        kind: BlockKind::Paragraph(Paragraph {
            style: ParagraphStyle {
                align,
                ..ParagraphStyle::default()
            },
            style_ref: None,
            runs,
        }),
    };
    Cell {
        blocks: vec![para],
        col_span: col_span.max(1),
        row_span: row_span.max(1),
        shading: None,
    }
}

// ── interior-rule probes (drive merged-cell inference) ───────────────────────
//
// X/Y match tolerance for "a segment lies on this edge" — the same slack
// `cluster_edges` used to fuse near-equal coordinates into one edge.
const EDGE_TOL: f64 = 3.0;
// A boundary counts as **ruled** only when drawn segments cover at least this
// fraction of its length. High enough that a real interior divider (drawn full
// height/width) always qualifies, so only a *deliberately omitted* divider reads
// as absent and triggers a span. Conservative by design.
const RULE_COVER: f64 = 0.6;

/// Whether a **vertical** rule lies along `x_edge` across the Y-band between row
/// edges `r0` (top) and `r1` (bottom). Sums the coverage of every collinear
/// vertical segment (a divider can be drawn in pieces) and tests it against
/// [`RULE_COVER`] of the band height.
fn vrule_present(table: &PlannedTable, x_edge: f64, r0: usize, r1: usize) -> bool {
    let (Some(&y_top), Some(&y_bot)) = (table.rows.get(r0), table.rows.get(r1)) else {
        return false;
    };
    let band = (y_top - y_bot).abs();
    if band <= 0.0 {
        return false;
    }
    let mut covered = 0.0;
    for &(x, sy0, sy1) in &table.v_segs {
        if (x - x_edge).abs() > EDGE_TOL {
            continue;
        }
        let lo = sy0.min(sy1).max(y_bot.min(y_top));
        let hi = sy0.max(sy1).min(y_top.max(y_bot));
        if hi > lo {
            covered += hi - lo;
        }
    }
    covered >= band * RULE_COVER
}

/// Whether a **horizontal** rule lies along `y_edge` across the X-band between
/// column edges `c0` (left) and `c1` (right). Mirror of [`vrule_present`].
fn hrule_present(table: &PlannedTable, y_edge: f64, c0: usize, c1: usize) -> bool {
    let (Some(&x_lo), Some(&x_hi)) = (table.cols.get(c0), table.cols.get(c1)) else {
        return false;
    };
    let band = (x_hi - x_lo).abs();
    if band <= 0.0 {
        return false;
    }
    let mut covered = 0.0;
    for &(y, sx0, sx1) in &table.h_segs {
        if (y - y_edge).abs() > EDGE_TOL {
            continue;
        }
        let lo = sx0.min(sx1).max(x_lo.min(x_hi));
        let hi = sx0.max(sx1).min(x_lo.max(x_hi));
        if hi > lo {
            covered += hi - lo;
        }
    }
    covered >= band * RULE_COVER
}

/// Find the column index whose `[lo, hi)` contains `x` (edges ascending).
fn col_of(cols: &[f64], x: f64) -> Option<usize> {
    if cols.len() < 2 {
        return None;
    }
    if x < cols[0] - 0.5 || x > cols[cols.len() - 1] + 0.5 {
        return None;
    }
    (0..cols.len() - 1).find(|&i| x >= cols[i] - 0.5 && x <= cols[i + 1] + 0.5)
}

/// Find the row index whose `[top, bottom)` contains `y` (edges descending).
fn row_of(rows: &[f64], y: f64) -> Option<usize> {
    if rows.len() < 2 {
        return None;
    }
    if y > rows[0] + 0.5 || y < rows[rows.len() - 1] - 0.5 {
        return None;
    }
    (0..rows.len() - 1).find(|&i| y <= rows[i] + 0.5 && y >= rows[i + 1] - 0.5)
}

// ── ruled tables ─────────────────────────────────────────────────────────────

/// A ruling segment kept with the painted-path index that drew it, so each table
/// a band yields can claim exactly the paths whose rules lie inside it (a path
/// belonging to table A must not be marked used by table B).
#[derive(Clone, Copy)]
struct HSeg {
    y: f64,
    x0: f64,
    x1: f64,
    path: usize,
}
#[derive(Clone, Copy)]
struct VSeg {
    x: f64,
    y0: f64,
    y1: f64,
    path: usize,
}

/// A maximal vertical band the rules glue together — the connected component on
/// the **Y axis** that becomes one candidate table. Two ruled tables stacked on a
/// page leave a vertical gap with no rule, which splits them into two bands.
struct RuleBand {
    /// Inclusive Y-extent `[lo, hi]` (PDF user space, lo ≤ hi).
    lo: f64,
    hi: f64,
}

/// Maximum vertical gap, **as a fraction of the band's own height built so far**,
/// that two rule intervals may straddle and still join the same table. Within one
/// table the vertical rules bridge its full height into a single continuous
/// interval, so consecutive horizontal rules never open a hole; a genuine
/// inter-table gap is a clear void of rules. A tiny absolute floor
/// ([`BAND_MIN_GAP`]) keeps slivers from over-splitting.
const BAND_MIN_GAP: f64 = 6.0;

/// Plan **all** ruled tables on the page. Rules are first segmented into vertical
/// connected components (bands separated by a rule-free gap); each band that
/// still has ≥ 2 column edges and ≥ 2 row edges becomes its own [`PlannedTable`].
/// Replaces the previous single-grid planner so two tables stacked on one page are
/// no longer fused into one englobing grid (which the sanity gate then dropped,
/// losing both).
fn plan_ruled_all(
    lines: &[ReconLine],
    vpaths: &[VectorPath],
    ignore_paths: &BTreeSet<usize>,
) -> Vec<PlannedTable> {
    let mut h_rules: Vec<HSeg> = Vec::new();
    let mut v_rules: Vec<VSeg> = Vec::new();

    for vp in vpaths {
        // Skip rules already claimed as text underlines — they must not be read
        // as table grid edges (a drawn underline near a table would add a phantom
        // row/column).
        if ignore_paths.contains(&vp.index) {
            continue;
        }
        match ruling_orientation(vp) {
            Some(Ruling::Horizontal { y, x0, x1 }) => h_rules.push(HSeg {
                y,
                x0,
                x1,
                path: vp.index,
            }),
            Some(Ruling::Vertical { x, y0, y1 }) => v_rules.push(VSeg {
                x,
                y0,
                y1,
                path: vp.index,
            }),
            None => {}
        }
    }
    if h_rules.len() < 2 || v_rules.len() < 2 {
        return Vec::new();
    }

    let bands = segment_rule_bands(&h_rules, &v_rules);

    let mut out: Vec<PlannedTable> = Vec::new();
    for band in &bands {
        // Rules whose Y-extent sits inside this band (a horizontal rule is a point
        // in Y; a vertical rule is included if it overlaps the band — its whole
        // segment then drives the grid).
        let lo = band.lo - EDGE_TOL;
        let hi = band.hi + EDGE_TOL;
        let band_h: Vec<HSeg> = h_rules
            .iter()
            .copied()
            .filter(|s| s.y >= lo && s.y <= hi)
            .collect();
        let band_v: Vec<VSeg> = v_rules
            .iter()
            .copied()
            .filter(|s| s.y0.min(s.y1) <= hi && s.y0.max(s.y1) >= lo)
            .collect();
        if let Some(t) = build_ruled_band(lines, &band_h, &band_v) {
            out.push(t);
        }
    }
    out
}

/// Materialise one ruled table from the rules of a single band. Mirror of the old
/// `plan_ruled` body, but over a pre-segmented slice of rules.
fn build_ruled_band(lines: &[ReconLine], h_rules: &[HSeg], v_rules: &[VSeg]) -> Option<PlannedTable> {
    if h_rules.len() < 2 || v_rules.len() < 2 {
        return None;
    }

    let cols = cluster_edges(v_rules.iter().map(|r| r.x));
    let rows_asc = cluster_edges(h_rules.iter().map(|r| r.y));
    if cols.len() < 2 || rows_asc.len() < 2 {
        return None;
    }
    // Row edges top→bottom (descending Y).
    let mut rows: Vec<f64> = rows_asc;
    rows.reverse();

    let x_lo = *cols.first().unwrap();
    let x_hi = *cols.last().unwrap();
    let y_top = *rows.first().unwrap();
    let y_bot = *rows.last().unwrap();

    // Which lines fall inside the grid extent.
    let covered: BTreeSet<usize> = (0..lines.len())
        .filter(|&i| {
            let l = &lines[i];
            let cx = l.x + l.w / 2.0;
            let cy = l.center_y();
            cx >= x_lo - 1.0 && cx <= x_hi + 1.0 && cy <= y_top + 1.0 && cy >= y_bot - 1.0
        })
        .collect();

    // Each table owns only the paths whose rules lie in its band.
    let used_paths: BTreeSet<usize> = h_rules
        .iter()
        .map(|s| s.path)
        .chain(v_rules.iter().map(|s| s.path))
        .collect();

    let start_line = *covered.iter().min()?;
    // Ruled cells are placed by centre and carry no per-column alignment evidence
    // (the rules say where columns *are*, not how text sits in them), so default
    // every column to left — matching the prior behaviour exactly.
    let col_align = vec![Align::Left; cols.len().saturating_sub(1)];
    Some(PlannedTable {
        cols,
        rows,
        ruled: true,
        covered_lines: covered,
        used_paths,
        start_line,
        h_segs: h_rules.iter().map(|s| (s.y, s.x0, s.x1)).collect(),
        v_segs: v_rules.iter().map(|s| (s.x, s.y0, s.y1)).collect(),
        col_align,
    })
}

/// Segment the page's ruling segments into vertical connected components.
///
/// Each segment projects to a Y-interval: a horizontal rule to the point
/// `[y, y]`, a vertical rule to `[min(y0,y1), max(y0,y1)]`. We sort the intervals
/// and merge any that overlap or are separated by less than an adaptive gap; a
/// wider void of rules opens a new band. Because a single table's vertical rules
/// run its full height, they fuse all its horizontal-rule points into one
/// interval — so a fully-ruled grid stays **one** band (non-regression), while two
/// stacked tables, whose rules never bridge the gap between them, split into two.
fn segment_rule_bands(h_rules: &[HSeg], v_rules: &[VSeg]) -> Vec<RuleBand> {
    let mut intervals: Vec<(f64, f64)> = Vec::with_capacity(h_rules.len() + v_rules.len());
    for s in h_rules {
        intervals.push((s.y, s.y));
    }
    for s in v_rules {
        intervals.push((s.y0.min(s.y1), s.y0.max(s.y1)));
    }
    if intervals.is_empty() {
        return Vec::new();
    }
    intervals.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(core::cmp::Ordering::Equal));

    // Split scale = one row pitch. A single table's frame verticals normally
    // bridge its whole height (union gapless ⇒ never splits, whatever the
    // threshold); the threshold only bites if verticals are drawn piecewise, where
    // the worst intra-table hole is about one row pitch. We therefore split only on
    // a void wider than `max(BAND_MIN_GAP, 1.5 × median row pitch)` — above any
    // single-row hole, yet below the frame-to-frame whitespace separating two
    // stacked tables. Conservative by construction: a single grid never splits.
    let split_gap = BAND_MIN_GAP.max(row_pitch(h_rules) * 1.5);

    let mut bands: Vec<RuleBand> = Vec::new();
    let (mut cur_lo, mut cur_hi) = intervals[0];
    for &(lo, hi) in &intervals[1..] {
        if lo - cur_hi > split_gap {
            bands.push(RuleBand {
                lo: cur_lo,
                hi: cur_hi,
            });
            cur_lo = lo;
            cur_hi = hi;
        } else {
            cur_hi = cur_hi.max(hi);
        }
    }
    bands.push(RuleBand {
        lo: cur_lo,
        hi: cur_hi,
    });
    bands
}

/// Median spacing between consecutive distinct horizontal-rule Y positions — the
/// natural "one row" scale used to size the band-splitting gap. Falls back to a
/// small positive value when fewer than two distinct rows exist (then only
/// [`BAND_MIN_GAP`] governs the split, which is already conservative).
fn row_pitch(h_rules: &[HSeg]) -> f64 {
    let mut ys: Vec<f64> = h_rules.iter().map(|s| s.y).collect();
    ys.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
    let mut gaps: Vec<f64> = Vec::new();
    for w in ys.windows(2) {
        let g = w[1] - w[0];
        if g > EDGE_TOL {
            gaps.push(g);
        }
    }
    if gaps.is_empty() {
        return 0.0;
    }
    gaps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
    gaps[gaps.len() / 2]
}

/// Cluster a set of nearly-equal edge coordinates into distinct edges (merging
/// values within a small tolerance), returned ascending.
fn cluster_edges(values: impl Iterator<Item = f64>) -> Vec<f64> {
    let mut vs: Vec<f64> = values.collect();
    vs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
    let mut out: Vec<f64> = Vec::new();
    for v in vs {
        match out.last_mut() {
            Some(last) if (v - *last).abs() <= 3.0 => {
                *last = (*last + v) / 2.0;
            }
            _ => out.push(v),
        }
    }
    out
}

// ── borderless fallback ──────────────────────────────────────────────────────

/// Estimate the abscissa of a numeric run's **decimal separator** (the decimal
/// tab a financial column aligns on), or `None` when the run is not a decimal
/// value. This is the R10 refinement of R8: amounts with *different* decimal
/// counts (`1,5` / `12,00` / `3,75`) share neither a right edge nor a left edge
/// but DO share the X of their `,` (or `.`). Returns the X of the last decimal
/// separator so `1.234,00` aligns on its `,`, not on a thousands `.`.
///
/// The position is approximated proportionally from the run's box: a run spans
/// `[x, x + w]`, so the separator at character offset `k` of an `n`-character
/// string sits at roughly `x + w * (k / n)`. This assumes near-monospaced digit
/// advance, which holds well enough for clustering (a tolerance absorbs the
/// drift). `text` is trimmed first so leading currency/sign/space don't bias the
/// ratio — the trimmed offsets are mapped back onto the original box width.
fn decimal_sep_x(text: &str, x: f64, w: f64) -> Option<f64> {
    if w <= 0.0 {
        return None;
    }
    let trimmed = text.trim();
    let chars: Vec<char> = trimmed.chars().collect();
    let n = chars.len();
    if n < 2 {
        return None;
    }
    // European convention (matching the codebase's amounts, e.g. `1.234,00`): the
    // decimal separator is a `,` when any `,` is present — then a `.` is always a
    // thousands grouping, never the decimal point. With no `,`, a `.` may be the
    // decimal point UNLESS the value is a dot-grouped integer (≥ 2 dots, each
    // group exactly 3 digits, e.g. `1.234.567`), which carries no fractional part.
    let has_comma = chars.contains(&',');
    let dot_count = chars.iter().filter(|&&c| c == '.').count();
    // Decide which separator character is the decimal point for this run.
    let decimal_char: char = if has_comma {
        ','
    } else if dot_count >= 2 {
        // Looks like dot-grouped thousands; only a `,` could be decimal, and there
        // is none → no decimal separator.
        return None;
    } else {
        '.'
    };
    // Find the last occurrence of the decimal character that is a real decimal
    // separator: 1–3 trailing fractional digits, at least one digit before it, and
    // only currency/space/sign after the fractional digits.
    let mut sep_idx: Option<usize> = None;
    for i in (0..n).rev() {
        if chars[i] != decimal_char {
            continue;
        }
        let mut frac = 0usize;
        let mut j = i + 1;
        while j < n && chars[j].is_ascii_digit() {
            frac += 1;
            j += 1;
        }
        let tail_ok = chars[j..]
            .iter()
            .all(|&c| c.is_whitespace() || matches!(c, '€' | '$' | '%' | '£' | '-' | '+'));
        let has_lead_digit = i > 0 && chars[..i].iter().any(|c| c.is_ascii_digit());
        if (1..=3).contains(&frac) && tail_ok && has_lead_digit {
            sep_idx = Some(i);
            break;
        }
    }
    let sep_idx = sep_idx?;
    // The separator occupies offset `sep_idx`; place X at its *centre* (offset +
    // 0.5) over the trimmed span, mapped onto the original box. Leading trimmed
    // chars shift the trimmed text right within the box, but proportionally over
    // the *trimmed* length the centre estimate stays stable, so we map the ratio
    // straight onto `[x, x + w]`.
    let ratio = (sep_idx as f64 + 0.5) / n as f64;
    Some(x + w * ratio)
}

/// One detected borderless column: the abscissa its cells align on (`anchor`) and
/// *which* edge that is (`align`). A left-aligned column shares its runs' **left**
/// edge (`run.x`); a right-aligned column shares their **right** edge
/// (`run.x + run.w`) — the latter is how numeric/financial columns with a fixed
/// right edge line up. R10 adds a third family, the **decimal column**: amounts
/// with differing decimal counts (`1,5` / `12,00` / `3,75`) share neither a left
/// nor a right edge, but they share the X of their decimal separator. A decimal
/// column anchors on that separator X yet is *reported* as `Align::Right` (the
/// nearest existing semantic — the `Align` enum lives outside `recon/` and is not
/// extended here), so downstream cell styling is unchanged.
#[derive(Debug, Clone, Copy)]
struct Column {
    /// The shared edge abscissa used to *assign* runs to this column: the left
    /// edge for `Left`, the right edge for `Right` (or the decimal-separator X for
    /// a decimal column — see [`Column::decimal`]).
    anchor: f64,
    align: Align,
    /// `true` when this column was detected as a **decimal** column: `align` is
    /// still `Right` (no `Align::Decimal`), but `anchor` is the decimal-separator
    /// X and runs are matched on their own separator X, not their right edge.
    decimal: bool,
    /// A representative horizontal position used only to *order* columns and place
    /// the boundary between neighbours — the cluster's centre-of-mass X.
    center: f64,
}

/// A 1-D cluster: members fused because each sits within `gap` of the previous,
/// keeping their mean position and spread so callers can score tightness.
struct Cluster {
    /// Mean of the member values.
    mean: f64,
    /// Member values (an edge abscissa each); `len()` is the support.
    members: Vec<f64>,
}

impl Cluster {
    /// Max member distance from the mean — smaller = tighter = stronger evidence
    /// the runs deliberately share this edge.
    fn spread(&self) -> f64 {
        self.members
            .iter()
            .map(|v| (v - self.mean).abs())
            .fold(0.0, f64::max)
    }
}

/// Cluster values (each tagged with the row it came from) into 1-D groups: a new
/// cluster opens when a value sits more than `gap` past the previous. Returns the
/// clusters with their mean and members, ascending.
fn cluster_1d(values: &[f64], gap: f64) -> Vec<Cluster> {
    let mut v: Vec<f64> = values.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
    let mut out: Vec<Cluster> = Vec::new();
    for x in v {
        match out.last_mut() {
            Some(c) if x - *c.members.last().unwrap() <= gap => {
                c.members.push(x);
                c.mean = c.members.iter().sum::<f64>() / c.members.len() as f64;
            }
            _ => out.push(Cluster {
                mean: x,
                members: vec![x],
            }),
        }
    }
    out
}

/// Detect the borderless columns of a region by clustering the left edges, the
/// right edges, **and** the decimal-separator abscissae of its runs, then letting
/// each run vote for whichever it sits on most tightly. This recognises
/// right-aligned numeric columns (shared right edge, scattered left edges), mixed
/// layouts (a left-aligned label column beside an amount column), and — the R10
/// refinement — **decimal columns** whose amounts have varying decimal counts
/// (`1,5` / `12,00` / `3,75`) so they share only the X of their `,`. Pure
/// left/right-edge clustering would shatter that last case into parasitic columns.
///
/// `runs` is `(left, right, decimal_x)` per run across the region, where
/// `decimal_x` is `Some(x_of_separator)` for a numeric value with decimals (from
/// [`decimal_sep_x`]) and `None` otherwise. The gate is deliberately kept
/// downstream (≥ 2 rows hitting ≥ 2 columns); this only *proposes* columns.
fn detect_columns(runs: &[(f64, f64, Option<f64>)], col_gap: f64) -> Vec<Column> {
    if runs.len() < 2 {
        return Vec::new();
    }
    let lefts: Vec<f64> = runs.iter().map(|&(l, _, _)| l).collect();
    let rights: Vec<f64> = runs.iter().map(|&(_, r, _)| r).collect();
    let decimals: Vec<f64> = runs.iter().filter_map(|&(_, _, d)| d).collect();
    let left_clusters = cluster_1d(&lefts, col_gap);
    let right_clusters = cluster_1d(&rights, col_gap);
    // Decimal columns align on the separator, which is tighter than a right edge,
    // so cluster them on a smaller gap to avoid fusing two adjacent money columns.
    let dec_clusters = cluster_1d(&decimals, (col_gap * 0.5).max(4.0));

    /// What a candidate column aligns on. `Decimal` is reported to callers as a
    /// right-aligned column (`Align::Right`) but matches runs on their separator X.
    #[derive(PartialEq, Clone, Copy)]
    enum Kind {
        Left,
        Right,
        Decimal,
    }
    // Candidate columns from the three families. Each run is assigned to a single
    // best candidate, so overlapping candidates compete for runs rather than all
    // surviving.
    struct Cand {
        pos: f64,
        kind: Kind,
        spread: f64,
        /// Cluster support = how many edges fell in this candidate's cluster. The
        /// *primary* tiebreak when a run sits equally on two candidates: a shared
        /// right edge (or separator X) backed by many runs beats each run's own
        /// singleton left edge, so numeric columns win over the accidental per-row
        /// left-edge clusters their scattered left edges create.
        support: usize,
        // Edges of the runs that ended up choosing this candidate, used to re-fit
        // the anchor and to compute the centre.
        chosen_left: Vec<f64>,
        chosen_right: Vec<f64>,
        // Separator X of the (decimal) runs that chose this candidate; only filled
        // for `Decimal` candidates, used to re-fit the decimal anchor.
        chosen_dec: Vec<f64>,
    }
    let mk = |pos: f64, kind: Kind, spread: f64, support: usize| Cand {
        pos,
        kind,
        spread,
        support,
        chosen_left: Vec::new(),
        chosen_right: Vec::new(),
        chosen_dec: Vec::new(),
    };
    let mut cands: Vec<Cand> = Vec::new();
    for c in &left_clusters {
        cands.push(mk(c.mean, Kind::Left, c.spread(), c.members.len()));
    }
    for c in &right_clusters {
        cands.push(mk(c.mean, Kind::Right, c.spread(), c.members.len()));
    }
    for c in &dec_clusters {
        // A decimal column needs ≥ 2 amounts agreeing on the separator to be
        // evidence; a lone decimal value is not a column on its own.
        if c.members.len() >= 2 {
            cands.push(mk(c.mean, Kind::Decimal, c.spread(), c.members.len()));
        }
    }

    // Assign each run to its best candidate: the nearest left-candidate by its left
    // edge, the nearest right-candidate by its right edge, or — for a numeric run —
    // the nearest decimal-candidate by its separator X. When a run sits on two
    // candidates at (near-)equal distance, prefer a decimal match (the most
    // specific evidence for a money column), then more support, then the tighter
    // one, then left (stable).
    for &(l, r, dec) in runs {
        // (idx, dist, is_decimal, support, spread)
        let mut best: Option<(usize, f64, bool, usize, f64)> = None;
        for (idx, cand) in cands.iter().enumerate() {
            let edge = match cand.kind {
                Kind::Left => l,
                Kind::Right => r,
                // Only a numeric run (with its own separator X) can match a decimal
                // candidate; a label never lands in a decimal column.
                Kind::Decimal => match dec {
                    Some(d) => d,
                    None => continue,
                },
            };
            let dist = (edge - cand.pos).abs();
            if dist > col_gap {
                continue;
            }
            let is_dec = cand.kind == Kind::Decimal;
            let better = match best {
                None => true,
                Some((_, bd, bdec, bsup, bspr)) => {
                    if (dist - bd).abs() > 1e-6 {
                        dist < bd
                    } else if is_dec != bdec {
                        // Equal distance: the decimal-tab interpretation wins, so a
                        // money column anchors on its separator rather than fanning
                        // out across right-edge / left-edge singletons.
                        is_dec
                    } else if cand.support != bsup {
                        cand.support > bsup
                    } else {
                        cand.spread < bspr - 1e-6
                    }
                }
            };
            if better {
                best = Some((idx, dist, is_dec, cand.support, cand.spread));
            }
        }
        if let Some((idx, _, _, _, _)) = best {
            cands[idx].chosen_left.push(l);
            cands[idx].chosen_right.push(r);
            if let Some(d) = dec {
                cands[idx].chosen_dec.push(d);
            }
        }
    }

    // Keep only candidates that actually won runs, re-fit their anchor from the
    // chosen edges, and compute a centre-of-mass for ordering. A candidate that
    // lost all its runs to a coincident one drops out here, so we never emit two
    // columns for the same physical column.
    let mut columns: Vec<Column> = Vec::new();
    for cand in &cands {
        if cand.chosen_left.len() < 2 {
            // A column needs ≥ 2 stacked runs to be evidence of alignment; a lone
            // run is just a word and never anchors a column on its own.
            continue;
        }
        let is_decimal = cand.kind == Kind::Decimal;
        let anchor = match cand.kind {
            Kind::Left => cand.chosen_left.iter().sum::<f64>() / cand.chosen_left.len() as f64,
            Kind::Right => cand.chosen_right.iter().sum::<f64>() / cand.chosen_right.len() as f64,
            // Decimal columns anchor on the mean separator X of their amounts.
            Kind::Decimal => {
                if cand.chosen_dec.len() < 2 {
                    continue;
                }
                cand.chosen_dec.iter().sum::<f64>() / cand.chosen_dec.len() as f64
            }
        };
        let lo = cand
            .chosen_left
            .iter()
            .cloned()
            .fold(f64::INFINITY, f64::min);
        let hi = cand
            .chosen_right
            .iter()
            .cloned()
            .fold(f64::NEG_INFINITY, f64::max);
        let center = (lo + hi) / 2.0;
        // A decimal column reports as right-aligned (no `Align::Decimal`); the
        // other families map to their natural alignment.
        let align = match cand.kind {
            Kind::Left => Align::Left,
            Kind::Right | Kind::Decimal => Align::Right,
        };
        columns.push(Column {
            anchor,
            align,
            decimal: is_decimal,
            center,
        });
    }

    // Order columns left→right by centre and drop any whose centres collide within
    // `col_gap` (defensive — should not happen after the run-assignment contest),
    // keeping the first (already the lower-X) one.
    columns.sort_by(|a, b| {
        a.center
            .partial_cmp(&b.center)
            .unwrap_or(core::cmp::Ordering::Equal)
    });
    let mut deduped: Vec<Column> = Vec::new();
    for col in columns {
        match deduped.last() {
            Some(prev) if (col.center - prev.center).abs() <= col_gap => {}
            _ => deduped.push(col),
        }
    }
    deduped
}

/// Which detected column a run belongs to: the column whose *matching* abscissa is
/// nearest the run's corresponding one — the left edge for a `Left` column, the
/// right edge for a `Right` column, or the **decimal-separator X** for a decimal
/// column (when the run has one). Returns the index, or `None` if nothing is
/// within `col_gap` (a stray run between columns is not counted toward the row's
/// hits). `dec` is the run's separator X from [`decimal_sep_x`], or `None`.
fn column_of_run(
    columns: &[Column],
    left: f64,
    right: f64,
    dec: Option<f64>,
    col_gap: f64,
) -> Option<usize> {
    let mut best: Option<(usize, f64)> = None;
    for (i, col) in columns.iter().enumerate() {
        // A decimal column is matched on the run's separator X when the run is
        // numeric; a non-numeric run falls back to its right edge so labels still
        // resolve sensibly against the column's (separator) anchor.
        let edge = if col.decimal {
            match dec {
                Some(d) => d,
                None => right,
            }
        } else if col.align == Align::Left {
            left
        } else {
            right
        };
        let dist = (edge - col.anchor).abs();
        if dist > col_gap {
            continue;
        }
        if best.is_none_or(|(_, bd)| dist < bd) {
            best = Some((i, dist));
        }
    }
    best.map(|(i, _)| i)
}

/// Plan **all** borderless tables among the free lines. Candidate tabular rows
/// (those hitting ≥ 2 columns) are first segmented into vertical regions separated
/// by a band of prose (a large baseline gap); each region is then built into its
/// own grid with **region-local**, alignment-aware columns (see [`detect_columns`]),
/// so two stacked lists with different column layouts are not fused into one
/// englobing grid (which the sanity gate could then drop, losing both).
fn plan_borderless_all(lines: &[ReconLine], free: &[usize]) -> Vec<PlannedTable> {
    if free.len() < 2 {
        return Vec::new();
    }

    // First pass: global columns only to *identify* which free lines are tabular
    // (hit ≥ 2 columns). The grid itself is rebuilt per region below with local
    // columns, so a global mismatch between two regions can't distort either.
    // Each edge tuple is `(left, right, decimal_x)` — the third carries the X of a
    // numeric run's decimal separator so amounts with varying decimals still group.
    let mut edges: Vec<(f64, f64, Option<f64>)> = Vec::new();
    let mut heights: Vec<f64> = Vec::new();
    for &i in free {
        for r in &lines[i].runs {
            edges.push((r.x, r.x + r.w, decimal_sep_x(&r.text, r.x, r.w)));
            heights.push(r.h.max(1.0));
        }
    }
    if edges.len() < 4 {
        return Vec::new();
    }
    let h_med = median(&mut heights, 10.0);
    let col_gap = (h_med * 2.0).max(16.0);
    let global_columns = detect_columns(&edges, col_gap);
    if global_columns.len() < 2 {
        return Vec::new(); // single column ⇒ prose, not a table
    }

    let mut row_lines: Vec<usize> = Vec::new();
    for &i in free {
        let mut hit: BTreeSet<usize> = BTreeSet::new();
        for r in &lines[i].runs {
            let dec = decimal_sep_x(&r.text, r.x, r.w);
            if let Some(c) = column_of_run(&global_columns, r.x, r.x + r.w, dec, col_gap) {
                hit.insert(c);
            }
        }
        if hit.len() >= 2 {
            row_lines.push(i);
        }
    }
    if row_lines.len() < 2 {
        return Vec::new(); // need ≥ 2 tabular rows somewhere
    }

    // Segment the tabular rows into vertical regions: a baseline gap wider than a
    // few line-heights is a band of prose splitting two separate tables. Rows are
    // ordered top→bottom (descending centre-Y) before scanning the gaps.
    let row_gap = (h_med * 2.5).max(20.0);
    let mut ordered = row_lines.clone();
    ordered.sort_by(|&a, &b| {
        lines[b]
            .center_y()
            .partial_cmp(&lines[a].center_y())
            .unwrap_or(core::cmp::Ordering::Equal)
    });
    let mut regions: Vec<Vec<usize>> = Vec::new();
    let mut cur: Vec<usize> = vec![ordered[0]];
    for &i in &ordered[1..] {
        let prev = *cur.last().unwrap();
        if (lines[prev].center_y() - lines[i].center_y()).abs() > row_gap {
            regions.push(std::mem::take(&mut cur));
        }
        cur.push(i);
    }
    regions.push(cur);

    let mut out: Vec<PlannedTable> = Vec::new();
    for region in &regions {
        if let Some(t) = build_borderless_region(lines, region, h_med, col_gap) {
            out.push(t);
        }
    }
    out
}

/// Build one borderless table from a vertical region of candidate rows, using
/// **region-local**, alignment-aware columns. Re-validates the ≥ 2 rows / ≥ 2
/// columns gate so a region that no longer looks tabular on its own is dropped.
fn build_borderless_region(
    lines: &[ReconLine],
    region: &[usize],
    h_med: f64,
    col_gap: f64,
) -> Option<PlannedTable> {
    if region.len() < 2 {
        return None;
    }
    // `(left, right, decimal_x)` per run — the decimal X groups varying-decimal
    // amounts onto their shared separator (R10).
    let mut edges: Vec<(f64, f64, Option<f64>)> = Vec::new();
    for &i in region {
        for r in &lines[i].runs {
            edges.push((r.x, r.x + r.w, decimal_sep_x(&r.text, r.x, r.w)));
        }
    }
    let columns = detect_columns(&edges, col_gap);
    if columns.len() < 2 {
        return None;
    }
    // Keep only rows that hit ≥ 2 of the region's own columns.
    let mut row_lines: Vec<usize> = Vec::new();
    for &i in region {
        let mut hit: BTreeSet<usize> = BTreeSet::new();
        for r in &lines[i].runs {
            let dec = decimal_sep_x(&r.text, r.x, r.w);
            if let Some(c) = column_of_run(&columns, r.x, r.x + r.w, dec, col_gap) {
                hit.insert(c);
            }
        }
        if hit.len() >= 2 {
            row_lines.push(i);
        }
    }
    if row_lines.len() < 2 {
        return None;
    }

    // Column edges midway between adjacent columns' centres (extend out at the
    // ends). Using the centre-of-mass — not the alignment anchor — keeps the
    // boundary between a left- and a right-aligned column on the visual gap
    // between them, so every run's *centre* still lands in the right cell.
    let centers: Vec<f64> = columns.iter().map(|c| c.center).collect();
    let mut cols: Vec<f64> = Vec::with_capacity(centers.len() + 1);
    cols.push(centers[0] - col_gap / 2.0);
    for w in centers.windows(2) {
        cols.push((w[0] + w[1]) / 2.0);
    }
    cols.push(*centers.last().unwrap() + col_gap * 4.0);
    let col_align: Vec<Align> = columns.iter().map(|c| c.align).collect();

    // Row edges from the tabular rows' centres (descending Y).
    let mut centers: Vec<f64> = row_lines.iter().map(|&i| lines[i].center_y()).collect();
    centers.sort_by(|a, b| b.partial_cmp(a).unwrap_or(core::cmp::Ordering::Equal));
    let top_h = lines[row_lines[0]].h.max(h_med);
    let mut rows: Vec<f64> = Vec::with_capacity(centers.len() + 1);
    rows.push(centers[0] + top_h);
    for w in centers.windows(2) {
        rows.push((w[0] + w[1]) / 2.0);
    }
    rows.push(*centers.last().unwrap() - top_h);

    let covered: BTreeSet<usize> = row_lines.iter().copied().collect();
    let start_line = *covered.iter().min()?;
    Some(PlannedTable {
        cols,
        rows,
        ruled: false,
        covered_lines: covered,
        used_paths: BTreeSet::new(),
        start_line,
        // Borderless tables have no rules, so cell-merge inference (which proves
        // a span by a *missing* interior rule) cannot run — leave the segs empty
        // and every cell stays 1×1.
        h_segs: Vec::new(),
        v_segs: Vec::new(),
        col_align,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::vector::PathSeg;
    use crate::convert::style::TextStyle;
    use crate::recon::lines::group_into_lines;
    use crate::recon::ReconRun;

    fn run(text: &str, x: f64, y: f64, w: f64) -> ReconRun {
        ReconRun {
            text: text.to_string(),
            x,
            y,
            w,
            h: 10.0,
            size: 10.0,
            style: TextStyle::default(),
            rotation: 0.0,
            source_index: None,
            underline: false,
            strike: false,
        }
    }

    /// `run` with an explicit content-stream `source_index` (for addressability).
    fn run_src(text: &str, x: f64, y: f64, w: f64, source_index: usize) -> ReconRun {
        ReconRun {
            source_index: Some(source_index),
            ..run(text, x, y, w)
        }
    }

    /// A horizontal ruling line as a thin filled rectangle path.
    fn hrule(y: f64, x0: f64, x1: f64) -> VectorPath {
        rect_path(x0, y - 0.25, x1 - x0, 0.5)
    }
    /// A vertical ruling line as a thin filled rectangle path.
    fn vrule(x: f64, y0: f64, y1: f64) -> VectorPath {
        rect_path(x - 0.25, y0, 0.5, y1 - y0)
    }
    fn rect_path(x: f64, y: f64, w: f64, h: f64) -> VectorPath {
        use crate::content::Bounds;
        VectorPath {
            index: 0,
            bounds: Some(Bounds {
                x,
                y,
                width: w,
                height: h,
            }),
            segments: vec![
                PathSeg::Move(x, y),
                PathSeg::Line(x + w, y),
                PathSeg::Line(x + w, y + h),
                PathSeg::Line(x, y + h),
                PathSeg::Close,
            ],
            fill: Some([0.0, 0.0, 0.0]),
            stroke: None,
            stroke_width: 0.5,
            fill_alpha: 1.0,
            stroke_alpha: 1.0,
            dash: Vec::new(),
        }
    }

    #[test]
    fn ruled_grid_builds_a_table_with_rows_and_cells() {
        // 2×2 ruled grid: columns at x=50,150,250; rows at y=100,120,140.
        // Cell text placed inside each cell.
        let runs = vec![
            run("Name", 60.0, 122.0, 40.0),
            run("Age", 160.0, 122.0, 30.0),
            run("Alice", 60.0, 102.0, 40.0),
            run("30", 160.0, 102.0, 20.0),
        ];
        let lines = group_into_lines(&runs);
        let mut paths = vec![
            hrule(100.0, 50.0, 250.0),
            hrule(120.0, 50.0, 250.0),
            hrule(140.0, 50.0, 250.0),
            vrule(50.0, 100.0, 140.0),
            vrule(150.0, 100.0, 140.0),
            vrule(250.0, 100.0, 140.0),
        ];
        for (i, p) in paths.iter_mut().enumerate() {
            p.index = i;
        }
        let plan = plan_tables(&lines, &paths, &BTreeSet::new());
        let tbl = plan.take_if_starts_at(0).expect("table at line 0");
        let mut ids = IdGen::default();
        let block = build_table(&tbl, &lines, &mut ids, Rect::new).unwrap();
        let BlockKind::Table(t) = block.kind else {
            panic!("expected table");
        };
        assert_eq!(t.rows.len(), 2, "two rows");
        assert_eq!(t.rows[0].cells.len(), 2, "two columns");
        // Top row (higher Y) is "Name"/"Age".
        let cell_text = |c: &Cell| -> String {
            match &c.blocks[0].kind {
                BlockKind::Paragraph(p) => match p.runs.first() {
                    Some(crate::model::Inline::Run(r)) => r.text.clone(),
                    _ => String::new(),
                },
                _ => String::new(),
            }
        };
        assert_eq!(cell_text(&t.rows[0].cells[0]), "Name");
        assert_eq!(cell_text(&t.rows[0].cells[1]), "Age");
        assert_eq!(cell_text(&t.rows[1].cells[0]), "Alice");
        assert_eq!(cell_text(&t.rows[1].cells[1]), "30");
        // Ruled border is widened.
        assert!(t.border.width > 0.0);
        // The rule paths are marked used.
        assert!(plan.uses_path(0) && plan.uses_path(3));
    }

    #[test]
    fn cell_runs_carry_the_first_source_index() {
        // Same 2×2 ruled grid as above, but each cell's run carries a distinct
        // content-stream `source_index`. The reconstructed cell run must surface
        // that index so a host can address the cell by its flat run-index space.
        let runs = vec![
            run_src("Name", 60.0, 122.0, 40.0, 10),
            run_src("Age", 160.0, 122.0, 30.0, 11),
            run_src("Alice", 60.0, 102.0, 40.0, 12),
            run_src("30", 160.0, 102.0, 20.0, 13),
        ];
        let lines = group_into_lines(&runs);
        let mut paths = vec![
            hrule(100.0, 50.0, 250.0),
            hrule(120.0, 50.0, 250.0),
            hrule(140.0, 50.0, 250.0),
            vrule(50.0, 100.0, 140.0),
            vrule(150.0, 100.0, 140.0),
            vrule(250.0, 100.0, 140.0),
        ];
        for (i, p) in paths.iter_mut().enumerate() {
            p.index = i;
        }
        let plan = plan_tables(&lines, &paths, &BTreeSet::new());
        let tbl = plan.take_if_starts_at(0).expect("table at line 0");
        let mut ids = IdGen::default();
        let block = build_table(&tbl, &lines, &mut ids, Rect::new).unwrap();
        let BlockKind::Table(t) = block.kind else {
            panic!("expected table");
        };
        let cell_src = |c: &Cell| -> Option<usize> {
            match &c.blocks[0].kind {
                BlockKind::Paragraph(p) => match p.runs.first() {
                    Some(crate::model::Inline::Run(r)) => r.source_index,
                    _ => None,
                },
                _ => None,
            }
        };
        assert_eq!(cell_src(&t.rows[0].cells[0]), Some(10), "Name cell index");
        assert_eq!(cell_src(&t.rows[0].cells[1]), Some(11), "Age cell index");
        assert_eq!(cell_src(&t.rows[1].cells[0]), Some(12), "Alice cell index");
        assert_eq!(cell_src(&t.rows[1].cells[1]), Some(13), "30 cell index");
    }

    #[test]
    fn empty_cell_has_no_source_index() {
        // A cell with no text run carries no `source_index` (stays addressable as
        // an empty cell by its grid position, but has no flat run index).
        let runs = vec![
            run_src("Name", 60.0, 122.0, 40.0, 10),
            // top-right cell intentionally left empty
            run_src("Alice", 60.0, 102.0, 40.0, 12),
            run_src("30", 160.0, 102.0, 20.0, 13),
        ];
        let lines = group_into_lines(&runs);
        let mut paths = vec![
            hrule(100.0, 50.0, 250.0),
            hrule(120.0, 50.0, 250.0),
            hrule(140.0, 50.0, 250.0),
            vrule(50.0, 100.0, 140.0),
            vrule(150.0, 100.0, 140.0),
            vrule(250.0, 100.0, 140.0),
        ];
        for (i, p) in paths.iter_mut().enumerate() {
            p.index = i;
        }
        let plan = plan_tables(&lines, &paths, &BTreeSet::new());
        let tbl = plan.take_if_starts_at(0).expect("table at line 0");
        let mut ids = IdGen::default();
        let block = build_table(&tbl, &lines, &mut ids, Rect::new).unwrap();
        let BlockKind::Table(t) = block.kind else {
            panic!("expected table");
        };
        // Empty cell → empty runs → no index.
        let empty = &t.rows[0].cells[1];
        match &empty.blocks[0].kind {
            BlockKind::Paragraph(p) => assert!(p.runs.is_empty(), "empty cell has no run"),
            _ => panic!("expected paragraph"),
        }
    }

    #[test]
    fn prose_is_not_promoted_to_a_table() {
        // Single-column left-aligned prose must stay out of the table planner.
        let runs = vec![
            run("First line of body text", 72.0, 700.0, 150.0),
            run("Second line of body text", 72.0, 686.0, 150.0),
            run("Third line of the body", 72.0, 672.0, 150.0),
        ];
        let lines = group_into_lines(&runs);
        let plan = plan_tables(&lines, &[], &BTreeSet::new());
        assert!(plan.take_if_starts_at(0).is_none(), "prose stays prose");
    }

    #[test]
    fn borderless_grid_with_two_aligned_columns_is_a_table() {
        // Two rows, two clearly separated columns, no rules → borderless table.
        let runs = vec![
            run("Product", 72.0, 700.0, 50.0),
            run("Price", 300.0, 700.0, 40.0),
            run("Widget", 72.0, 684.0, 50.0),
            run("9.99", 300.0, 684.0, 30.0),
        ];
        let lines = group_into_lines(&runs);
        let plan = plan_tables(&lines, &[], &BTreeSet::new());
        let tbl = plan.take_if_starts_at(0).expect("borderless table");
        let mut ids = IdGen::default();
        let block = build_table(&tbl, &lines, &mut ids, Rect::new).unwrap();
        let BlockKind::Table(t) = block.kind else {
            panic!("expected table");
        };
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.rows[0].cells.len(), 2);
        // Borderless ⇒ no widened border.
        assert_eq!(t.border.width, 0.0);
    }

    #[test]
    fn sparse_form_like_ruled_grid_is_rejected() {
        // A form: many vertical + horizontal field fences make a wide grid, but
        // almost every cell is empty (one short label per row). It must NOT be
        // promoted to a giant table — the labels stay free for the prose path.
        let mut paths: Vec<VectorPath> = Vec::new();
        // 16 vertical rules at x = 50,80,…,500 (15 columns), spanning y∈[100,400].
        let xs: Vec<f64> = (0..16).map(|i| 50.0 + i as f64 * 30.0).collect();
        for &x in &xs {
            paths.push(vrule(x, 100.0, 400.0));
        }
        // 11 horizontal rules at y = 100,130,…,400 (10 rows).
        let ys: Vec<f64> = (0..11).map(|i| 100.0 + i as f64 * 30.0).collect();
        for &y in &ys {
            paths.push(hrule(y, 50.0, 500.0));
        }
        for (i, p) in paths.iter_mut().enumerate() {
            p.index = i;
        }
        // One short label in the top-left cell of each row → ~10/150 cells.
        let runs: Vec<ReconRun> = (0..10)
            .map(|r| run("x", 55.0, 105.0 + r as f64 * 30.0, 8.0))
            .collect();
        let lines = group_into_lines(&runs);
        let plan = plan_tables(&lines, &paths, &BTreeSet::new());
        assert!(
            plan.take_if_starts_at(0).is_none(),
            "a sparse {}-column form grid must not become a table",
            xs.len() - 1
        );
    }

    #[test]
    fn dense_compact_ruled_grid_is_kept() {
        // A real 2×3 data table, fully filled → survives the sanity gate.
        let mut paths = vec![
            hrule(100.0, 50.0, 350.0),
            hrule(120.0, 50.0, 350.0),
            hrule(140.0, 50.0, 350.0),
            vrule(50.0, 100.0, 140.0),
            vrule(150.0, 100.0, 140.0),
            vrule(250.0, 100.0, 140.0),
            vrule(350.0, 100.0, 140.0),
        ];
        for (i, p) in paths.iter_mut().enumerate() {
            p.index = i;
        }
        let runs = vec![
            run("A", 60.0, 122.0, 30.0),
            run("B", 160.0, 122.0, 30.0),
            run("C", 260.0, 122.0, 30.0),
            run("D", 60.0, 102.0, 30.0),
            run("E", 160.0, 102.0, 30.0),
            run("F", 260.0, 102.0, 30.0),
        ];
        let lines = group_into_lines(&runs);
        let plan = plan_tables(&lines, &paths, &BTreeSet::new());
        let tbl = plan
            .take_if_starts_at(0)
            .expect("a dense compact grid stays a table");
        assert!(passes_table_sanity(&tbl, &lines));
    }

    #[test]
    fn sanity_gate_thresholds() {
        // Build a planner table directly and probe the three rejection paths.
        let dense = PlannedTable {
            cols: vec![0.0, 10.0, 20.0],
            rows: vec![20.0, 10.0, 0.0],
            ruled: true,
            covered_lines: BTreeSet::new(),
            used_paths: BTreeSet::new(),
            start_line: 0,
            h_segs: Vec::new(),
            v_segs: Vec::new(),
            col_align: vec![Align::Left, Align::Left],
        };
        // 2×2 grid, 4 cells, all filled.
        let runs = vec![
            run("a", 1.0, 11.0, 8.0),
            run("b", 11.0, 11.0, 8.0),
            run("c", 1.0, 1.0, 8.0),
            run("d", 11.0, 1.0, 8.0),
        ];
        let lines = group_into_lines(&runs);
        let mut t = dense.clone();
        t.covered_lines = (0..lines.len()).collect();
        assert!(passes_table_sanity(&t, &lines), "dense 2×2 passes");

        // Too many columns.
        let mut wide = dense.clone();
        wide.cols = (0..=MAX_TABLE_COLS as i32 + 2)
            .map(|i| i as f64 * 5.0)
            .collect();
        wide.covered_lines = (0..lines.len()).collect();
        assert!(
            !passes_table_sanity(&wide, &lines),
            "over-wide grid rejected"
        );

        // Empty grid (no runs land in cells) → zero fill → rejected.
        let mut empty = dense.clone();
        empty.covered_lines = BTreeSet::new();
        assert!(
            !passes_table_sanity(&empty, &lines),
            "an empty grid is rejected (0% fill)"
        );
    }

    #[test]
    fn col_and_row_lookup_are_correct() {
        let cols = vec![50.0, 150.0, 250.0];
        assert_eq!(col_of(&cols, 60.0), Some(0));
        assert_eq!(col_of(&cols, 160.0), Some(1));
        assert_eq!(col_of(&cols, 500.0), None);
        // Rows descending (top first).
        let rows = vec![140.0, 120.0, 100.0];
        assert_eq!(row_of(&rows, 130.0), Some(0));
        assert_eq!(row_of(&rows, 110.0), Some(1));
        assert_eq!(row_of(&rows, 10.0), None);
    }

    /// Build a table and return its rows, for span assertions.
    fn build_rows(paths: &[VectorPath], runs: &[ReconRun]) -> Vec<Row> {
        let lines = group_into_lines(runs);
        let mut paths = paths.to_vec();
        for (i, p) in paths.iter_mut().enumerate() {
            p.index = i;
        }
        let plan = plan_tables(&lines, &paths, &BTreeSet::new());
        let tbl = plan.take_if_starts_at(0).expect("a table");
        let mut ids = IdGen::default();
        let BlockKind::Table(t) = build_table(&tbl, &lines, &mut ids, Rect::new)
            .expect("table block")
            .kind
        else {
            panic!("expected table");
        };
        t.rows
    }

    fn cell_text(c: &Cell) -> String {
        match &c.blocks[0].kind {
            BlockKind::Paragraph(p) => match p.runs.first() {
                Some(crate::model::Inline::Run(r)) => r.text.clone(),
                _ => String::new(),
            },
            _ => String::new(),
        }
    }

    #[test]
    fn header_spanning_two_columns_gets_col_span_2() {
        // 2 rows × 3 columns. Columns at x=50,150,250,350; rows at y=140,120,100.
        // The interior vertical at x=150 is drawn **only in the bottom row**
        // [100,120] — it is missing across the header band [120,140]. So the top
        // cell spans columns 0+1 (col_span 2); x=250 is full-height, capping the
        // span. The bottom row stays three 1×1 cells.
        let paths = vec![
            hrule(140.0, 50.0, 350.0),
            hrule(120.0, 50.0, 350.0),
            hrule(100.0, 50.0, 350.0),
            // Outer verticals: full height.
            vrule(50.0, 100.0, 140.0),
            vrule(350.0, 100.0, 140.0),
            // x=150 interior: ONLY bottom row → absent in the header band.
            vrule(150.0, 100.0, 120.0),
            // x=250 interior: full height → present everywhere (caps the span).
            vrule(250.0, 100.0, 140.0),
        ];
        let runs = vec![
            // Header text sits in the merged region (col 0).
            run("Billing address", 60.0, 126.0, 80.0),
            run("Qty", 260.0, 126.0, 20.0),
            run("Rue de Paris", 60.0, 106.0, 40.0),
            run("75001", 160.0, 106.0, 30.0),
            run("3", 260.0, 106.0, 10.0),
        ];
        let rows = build_rows(&paths, &runs);
        assert_eq!(rows.len(), 2, "two rows");

        // Top row: a spanning cell (col_span 2) then a 1×1 cell ⇒ 2 cells total.
        assert_eq!(rows[0].cells.len(), 2, "header row merges to 2 cells");
        assert_eq!(
            rows[0].cells[0].col_span, 2,
            "header cell spans two columns"
        );
        assert_eq!(rows[0].cells[0].row_span, 1);
        assert_eq!(cell_text(&rows[0].cells[0]), "Billing address");
        assert_eq!(rows[0].cells[1].col_span, 1, "third column not merged");
        assert_eq!(cell_text(&rows[0].cells[1]), "Qty");

        // Bottom row: three full 1×1 cells (every interior divider present).
        assert_eq!(rows[1].cells.len(), 3, "body row keeps three cells");
        assert!(rows[1]
            .cells
            .iter()
            .all(|c| c.col_span == 1 && c.row_span == 1));
    }

    #[test]
    fn cell_spanning_two_rows_gets_row_span_2() {
        // 2 rows × 2 columns. The horizontal divider at y=120 is drawn **only in
        // the right column** [150,250] — missing under the left column. So the
        // left cell spans both rows (row_span 2); the right column stays two
        // 1×1 cells. No interior vertical is missing ⇒ no column merge.
        let paths = vec![
            hrule(140.0, 50.0, 250.0),
            hrule(100.0, 50.0, 250.0),
            // Interior horizontal at y=120: ONLY the right column.
            hrule(120.0, 150.0, 250.0),
            vrule(50.0, 100.0, 140.0),
            vrule(150.0, 100.0, 140.0),
            vrule(250.0, 100.0, 140.0),
        ];
        let runs = vec![
            run("Logo", 60.0, 116.0, 40.0),
            run("Name", 160.0, 126.0, 40.0),
            run("Addr", 160.0, 106.0, 40.0),
        ];
        let rows = build_rows(&paths, &runs);
        assert_eq!(rows.len(), 2);

        // Top row: left cell row_span 2 (col 0) + right 1×1 ⇒ 2 cells.
        assert_eq!(rows[0].cells.len(), 2);
        assert_eq!(rows[0].cells[0].row_span, 2, "left cell spans two rows");
        assert_eq!(rows[0].cells[0].col_span, 1, "left cell is one column");
        assert_eq!(cell_text(&rows[0].cells[0]), "Logo");

        // Bottom row: the left slot is covered by the row span above, so the row
        // supplies only the right cell.
        assert_eq!(rows[1].cells.len(), 1, "left slot absorbed by the row span");
        assert_eq!(cell_text(&rows[1].cells[0]), "Addr");
    }

    #[test]
    fn fully_ruled_grid_keeps_unit_cells() {
        // The conservative contract: when **every** interior rule is drawn, no
        // cell merges — output is the plain 1×1 grid (guards against the merge
        // inference firing spuriously on a normal data table).
        let paths = vec![
            hrule(140.0, 50.0, 250.0),
            hrule(120.0, 50.0, 250.0),
            hrule(100.0, 50.0, 250.0),
            vrule(50.0, 100.0, 140.0),
            vrule(150.0, 100.0, 140.0),
            vrule(250.0, 100.0, 140.0),
        ];
        let runs = vec![
            run("A", 60.0, 126.0, 30.0),
            run("B", 160.0, 126.0, 30.0),
            run("C", 60.0, 106.0, 30.0),
            run("D", 160.0, 106.0, 30.0),
        ];
        let rows = build_rows(&paths, &runs);
        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert_eq!(row.cells.len(), 2, "every row keeps both cells");
            assert!(
                row.cells.iter().all(|c| c.col_span == 1 && c.row_span == 1),
                "fully-ruled grid produces only 1×1 cells"
            );
        }
    }

    /// Count the `Table` blocks emitted for a page, the way `reconstruct_page`
    /// does: walk every line, materialise the table that starts there.
    fn count_tables(lines: &[ReconLine], paths: &[VectorPath]) -> usize {
        let plan = plan_tables(lines, paths, &BTreeSet::new());
        let mut ids = IdGen::default();
        let mut n = 0;
        for li in 0..lines.len() {
            if let Some(tbl) = plan.take_if_starts_at(li) {
                if let Some(block) = build_table(&tbl, lines, &mut ids, Rect::new) {
                    if matches!(block.kind, BlockKind::Table(_)) {
                        n += 1;
                    }
                }
            }
        }
        n
    }

    #[test]
    fn two_stacked_ruled_tables_yield_two_tables() {
        // GAP #5 — two distinct ruled grids stacked on one page, separated by a
        // clear vertical void. Table A (top) at y∈[300,340], Table B (bottom) at
        // y∈[100,140]; the 160-unit gap (140 → 300) carries no rule. Before the
        // fix these fused into ONE englobing grid (then dropped by the sanity gate,
        // losing both). Now the rules are segmented into two bands ⇒ two tables.
        let mut paths = vec![
            // Table A: 2×2, columns x=50,150,250; rows y=300,320,340.
            hrule(300.0, 50.0, 250.0),
            hrule(320.0, 50.0, 250.0),
            hrule(340.0, 50.0, 250.0),
            vrule(50.0, 300.0, 340.0),
            vrule(150.0, 300.0, 340.0),
            vrule(250.0, 300.0, 340.0),
            // Table B: 2×2, columns x=50,150,250; rows y=100,120,140.
            hrule(100.0, 50.0, 250.0),
            hrule(120.0, 50.0, 250.0),
            hrule(140.0, 50.0, 250.0),
            vrule(50.0, 100.0, 140.0),
            vrule(150.0, 100.0, 140.0),
            vrule(250.0, 100.0, 140.0),
        ];
        for (i, p) in paths.iter_mut().enumerate() {
            p.index = i;
        }
        let runs = vec![
            // Table A cells.
            run("Name", 60.0, 322.0, 40.0),
            run("Age", 160.0, 322.0, 30.0),
            run("Alice", 60.0, 302.0, 40.0),
            run("30", 160.0, 302.0, 20.0),
            // Table B cells.
            run("Item", 60.0, 122.0, 40.0),
            run("Qty", 160.0, 122.0, 30.0),
            run("Pen", 60.0, 102.0, 40.0),
            run("5", 160.0, 102.0, 20.0),
        ];
        let lines = group_into_lines(&runs);

        assert_eq!(
            count_tables(&lines, &paths),
            2,
            "two stacked ruled tables must yield two Table blocks, not one fused (or zero)"
        );

        // Both bands' paths are claimed, none leak back to the shape pass.
        let plan = plan_tables(&lines, &paths, &BTreeSet::new());
        for i in 0..paths.len() {
            assert!(plan.uses_path(i), "rule path {i} should be owned by a table");
        }
    }

    #[test]
    fn single_ruled_grid_stays_one_table() {
        // Non-regression on the table *count*: a single fully-ruled grid — even a
        // tall one with several rows — must come back as exactly ONE table (its
        // frame verticals bridge the full height, so the band segmenter never
        // splits it).
        let mut paths = vec![
            hrule(100.0, 50.0, 250.0),
            hrule(130.0, 50.0, 250.0),
            hrule(160.0, 50.0, 250.0),
            hrule(190.0, 50.0, 250.0),
            vrule(50.0, 100.0, 190.0),
            vrule(150.0, 100.0, 190.0),
            vrule(250.0, 100.0, 190.0),
        ];
        for (i, p) in paths.iter_mut().enumerate() {
            p.index = i;
        }
        let runs = vec![
            run("A", 60.0, 172.0, 30.0),
            run("B", 160.0, 172.0, 30.0),
            run("C", 60.0, 142.0, 30.0),
            run("D", 160.0, 142.0, 30.0),
            run("E", 60.0, 112.0, 30.0),
            run("F", 160.0, 112.0, 30.0),
        ];
        let lines = group_into_lines(&runs);
        assert_eq!(
            count_tables(&lines, &paths),
            1,
            "a single (tall) ruled grid stays one table"
        );
    }

    #[test]
    fn two_stacked_borderless_tables_yield_two_tables() {
        // Two borderless lists stacked on one page, separated by a band of prose.
        // List A (top) at y≈700/684, list B (bottom) at y≈300/284; the ~380-unit
        // baseline void splits them into two regions, each rebuilt with its own
        // column anchors ⇒ two tables.
        let runs = vec![
            // List A.
            run("Product", 72.0, 700.0, 50.0),
            run("Price", 300.0, 700.0, 40.0),
            run("Widget", 72.0, 684.0, 50.0),
            run("9.99", 300.0, 684.0, 30.0),
            // List B (different right column position is fine — anchors are local).
            run("City", 72.0, 300.0, 50.0),
            run("Pop", 320.0, 300.0, 40.0),
            run("Paris", 72.0, 284.0, 50.0),
            run("2M", 320.0, 284.0, 30.0),
        ];
        let lines = group_into_lines(&runs);
        assert_eq!(
            count_tables(&lines, &[]),
            2,
            "two stacked borderless tables must yield two Table blocks"
        );
    }

    #[test]
    fn segment_rule_bands_splits_on_gap_not_on_row_pitch() {
        // The segmenter splits on an inter-table void but never on a normal row
        // pitch. Build raw HSeg/VSeg directly.
        let h = |y: f64| HSeg {
            y,
            x0: 50.0,
            x1: 250.0,
            path: 0,
        };
        let v = |x: f64, y0: f64, y1: f64| VSeg {
            x,
            y0,
            y1,
            path: 0,
        };

        // One grid: rows 100/120/140, verticals bridge full height → ONE band.
        let h1 = vec![h(100.0), h(120.0), h(140.0)];
        let v1 = vec![v(50.0, 100.0, 140.0), v(250.0, 100.0, 140.0)];
        assert_eq!(
            segment_rule_bands(&h1, &v1).len(),
            1,
            "a single framed grid is one band"
        );

        // Two grids: [100..140] and [300..340], 160-unit void → TWO bands.
        let h2 = vec![
            h(100.0),
            h(120.0),
            h(140.0),
            h(300.0),
            h(320.0),
            h(340.0),
        ];
        let v2 = vec![
            v(50.0, 100.0, 140.0),
            v(250.0, 100.0, 140.0),
            v(50.0, 300.0, 340.0),
            v(250.0, 300.0, 340.0),
        ];
        assert_eq!(
            segment_rule_bands(&h2, &v2).len(),
            2,
            "two grids separated by a void are two bands"
        );
    }

    // ── R8: right-aligned / decimal borderless columns ───────────────────────

    /// Build the (single) borderless table a layout produces, or `None`.
    fn borderless_table(lines: &[ReconLine]) -> Option<Table> {
        let plan = plan_tables(lines, &[], &BTreeSet::new());
        let mut ids = IdGen::default();
        for li in 0..lines.len() {
            if let Some(tbl) = plan.take_if_starts_at(li) {
                if let Some(block) = build_table(&tbl, lines, &mut ids, Rect::new) {
                    if let BlockKind::Table(t) = block.kind {
                        return Some(t);
                    }
                }
            }
        }
        None
    }

    fn cell_align(c: &Cell) -> Align {
        match c.blocks.first().map(|b| &b.kind) {
            Some(BlockKind::Paragraph(p)) => p.style.align,
            _ => Align::Left,
        }
    }

    #[test]
    fn detect_columns_separates_left_label_from_right_amount() {
        // Unit-level: a left-aligned label column (left edge x=72) and a
        // right-aligned amount column (right edge 540, scattered left edges) must
        // resolve to exactly TWO columns — Left then Right — not three shattered
        // numeric columns. Left-edge-only clustering produced four anchors here.
        let edges = vec![
            // labels: (left, right, decimal_x) — left edge fixed, varying widths,
            // no decimal separator.
            (72.0, 112.0, None),
            (72.0, 192.0, None),
            (72.0, 132.0, None),
            // amounts: right edge fixed at 540, widely varying left edge. These
            // amounts carry no fractional text in this unit test (the edges are
            // synthetic), so they stay a right-aligned column, not a decimal one.
            (490.0, 540.0, None),
            (465.0, 540.0, None),
            (520.0, 540.0, None),
        ];
        let cols = detect_columns(&edges, 20.0);
        assert_eq!(cols.len(), 2, "label + amount = 2 columns, got {cols:?}");
        assert_eq!(cols[0].align, Align::Left, "label column is left-aligned");
        assert_eq!(cols[1].align, Align::Right, "amount column is right-aligned");
        // The right column's anchor is its shared right edge.
        assert!(
            (cols[1].anchor - 540.0).abs() < 1.0,
            "right anchor ≈ 540, got {}",
            cols[1].anchor
        );
    }

    #[test]
    fn borderless_invoice_right_aligned_amounts_is_one_amount_column() {
        // GAP #7 — a borderless "Libellé … Montant" table: labels left-aligned
        // (x=72, variable widths), amounts right-aligned (right edge=540, widely
        // varying left edges so left-edge clustering would shatter the column into
        // three). Must become a 2-column table (label + amount), not 4 columns and
        // not fused prose.
        let runs = vec![
            // Header row.
            run("Libelle", 72.0, 700.0, 40.0), // right 112
            run("Montant", 490.0, 700.0, 50.0), // right 540
            // "Consulting services" 1.234,00
            run("Consulting services", 72.0, 684.0, 120.0), // right 192
            run("1.234,00", 480.0, 684.0, 60.0),            // right 540
            // "Travel" 99,50
            run("Travel", 72.0, 668.0, 40.0),  // right 112
            run("99,50", 505.0, 668.0, 35.0),  // right 540
            // "Software license" 12.345,67
            run("Software license", 72.0, 652.0, 100.0), // right 172
            run("12.345,67", 465.0, 652.0, 75.0),        // right 540
        ];
        let lines = group_into_lines(&runs);
        let table = borderless_table(&lines).expect("invoice rows form a borderless table");
        assert_eq!(
            table.rows.len(),
            4,
            "four rows (header + 3 lines), got {}",
            table.rows.len()
        );
        // Every row has exactly two cells: the amount column did not shatter.
        for (ri, row) in table.rows.iter().enumerate() {
            assert_eq!(
                row.cells.len(),
                2,
                "row {ri} must have 2 cells (label + amount), got {}",
                row.cells.len()
            );
        }
        // Labels land in column 0, amounts in column 1.
        assert_eq!(cell_text(&table.rows[1].cells[0]), "Consulting services");
        assert_eq!(cell_text(&table.rows[1].cells[1]), "1.234,00");
        assert_eq!(cell_text(&table.rows[3].cells[1]), "12.345,67");
        // Borderless ⇒ no widened border.
        assert_eq!(table.border.width, 0.0);
        // Bonus #3: the amount column is marked right-aligned; the label column is
        // left-aligned.
        assert_eq!(
            cell_align(&table.rows[1].cells[0]),
            Align::Left,
            "label cell is left-aligned"
        );
        assert_eq!(
            cell_align(&table.rows[1].cells[1]),
            Align::Right,
            "amount cell is right-aligned"
        );
    }

    #[test]
    fn borderless_three_columns_mixed_alignment() {
        // A 3-column borderless table: left-aligned label, left-aligned quantity,
        // right-aligned amount (shared right edge 540). All three must be detected
        // with the correct per-column alignment.
        let runs = vec![
            run("Item", 72.0, 700.0, 40.0),     // label, left @72
            run("Qty", 260.0, 700.0, 30.0),     // qty, left @260
            run("Total", 490.0, 700.0, 50.0),   // amount header, right 540
            run("Apples", 72.0, 684.0, 60.0),   // left @72
            run("3", 260.0, 684.0, 12.0),       // left @260
            run("1.234,00", 480.0, 684.0, 60.0), // right 540
            run("Pears", 72.0, 668.0, 50.0),    // left @72
            run("12", 260.0, 668.0, 22.0),      // left @260
            run("99,50", 505.0, 668.0, 35.0),   // right 540
        ];
        let lines = group_into_lines(&runs);
        let table = borderless_table(&lines).expect("three-column mixed table");
        assert_eq!(table.rows.len(), 3, "three rows");
        for (ri, row) in table.rows.iter().enumerate() {
            assert_eq!(row.cells.len(), 3, "row {ri} has 3 columns");
        }
        // Alignment per column on a data row.
        let data = &table.rows[1];
        assert_eq!(cell_align(&data.cells[0]), Align::Left, "label left");
        assert_eq!(cell_align(&data.cells[1]), Align::Left, "qty left");
        assert_eq!(cell_align(&data.cells[2]), Align::Right, "amount right");
        assert_eq!(cell_text(&data.cells[0]), "Apples");
        assert_eq!(cell_text(&data.cells[2]), "1.234,00");
    }

    #[test]
    fn borderless_left_aligned_table_still_detected_unchanged() {
        // Non-regression: a plain left-aligned borderless table (both columns share
        // their left edge) is detected exactly as before — two columns, both
        // left-aligned, three rows.
        let runs = vec![
            run("Name", 72.0, 700.0, 50.0),
            run("Role", 300.0, 700.0, 50.0),
            run("Alice", 72.0, 684.0, 50.0),
            run("Engineer", 300.0, 684.0, 70.0),
            run("Bob", 72.0, 668.0, 50.0),
            run("Designer", 300.0, 668.0, 70.0),
        ];
        let lines = group_into_lines(&runs);
        let table = borderless_table(&lines).expect("left-aligned borderless table");
        assert_eq!(table.rows.len(), 3, "three rows");
        for row in &table.rows {
            assert_eq!(row.cells.len(), 2, "two columns");
            for cell in &row.cells {
                assert_eq!(
                    cell_align(cell),
                    Align::Left,
                    "left-aligned columns stay left-aligned"
                );
            }
        }
        assert_eq!(cell_text(&table.rows[1].cells[0]), "Alice");
        assert_eq!(cell_text(&table.rows[1].cells[1]), "Engineer");
    }

    #[test]
    fn right_ragged_prose_is_not_promoted_to_a_table() {
        // Anti-prose: ordinary single-column body text. The left edges share x=72
        // but the right edges are ragged (justified-off prose), so NEITHER edge
        // family yields a second column — it must stay prose, never a table.
        let runs = vec![
            run("The quick brown fox jumps over", 72.0, 700.0, 180.0),
            run("the lazy dog while the sun sets", 72.0, 686.0, 188.0),
            run("slowly behind the distant hills", 72.0, 672.0, 176.0),
            run("and the evening grows quiet now", 72.0, 658.0, 184.0),
        ];
        let lines = group_into_lines(&runs);
        let plan = plan_tables(&lines, &[], &BTreeSet::new());
        assert!(
            plan.take_if_starts_at(0).is_none(),
            "single-column ragged prose stays prose"
        );
        assert!(borderless_table(&lines).is_none());
    }

    #[test]
    fn coincidental_shared_right_edge_in_prose_is_not_a_table() {
        // Stronger anti-prose: two lines happen to end at the same right edge (a
        // coincidence justified text can produce) but there is no left-aligned
        // second column and no consistent interior column — still single-column,
        // so not promoted.
        let runs = vec![
            run("First paragraph line ending here", 72.0, 700.0, 200.0), // right 272
            run("A different second line ends too", 72.0, 686.0, 200.0), // right 272
            run("Third line is a bit shorter ok", 72.0, 672.0, 170.0),
        ];
        let lines = group_into_lines(&runs);
        let plan = plan_tables(&lines, &[], &BTreeSet::new());
        assert!(
            plan.take_if_starts_at(0).is_none(),
            "a shared right edge alone is not a column"
        );
    }

    // ── R10: decimal-tab borderless columns (varying decimal counts) ─────────

    #[test]
    fn decimal_sep_x_locates_the_separator() {
        // The estimate places the separator proportionally inside the run box. For
        // a box [400, 480] (w=80), the `,` of "12,00" (offset 2 of 5) lands near the
        // middle; an integer or a trailing-dot string has no decimal separator.
        let x = decimal_sep_x("12,00", 400.0, 80.0).expect("comma is a decimal sep");
        assert!((x - 440.0).abs() < 1.0, "sep ≈ 440 (mid box), got {x}");
        // "1.234,00": the LAST separator (the comma) is the decimal point, not the
        // thousands dot.
        let x2 = decimal_sep_x("1.234,00", 400.0, 80.0).expect("last sep is the comma");
        assert!(x2 > 440.0, "decimal sep is the comma (right of centre), got {x2}");
        // Non-decimal text returns None.
        assert!(decimal_sep_x("Total", 0.0, 50.0).is_none(), "word: no separator");
        assert!(decimal_sep_x("42", 0.0, 20.0).is_none(), "integer: no separator");
        assert!(
            decimal_sep_x("End of sentence.", 0.0, 90.0).is_none(),
            "trailing dot is not a decimal separator"
        );
        assert!(
            decimal_sep_x("1.234.567", 0.0, 60.0).is_none(),
            "grouped integer (no fractional part) is not a decimal value"
        );
    }

    #[test]
    fn detect_columns_groups_varying_decimal_amounts_into_one_column() {
        // R10 unit-level: a left-aligned label column plus amounts whose DECIMAL
        // SEPARATORS align at X=500 but whose left edges (440..480, spread 40) AND
        // right edges (510..545, spread 35) both scatter beyond col_gap (20). Pure
        // R8 left/right clustering shatters the amounts into singletons (no column
        // survives); the decimal family recovers a single amount column.
        let edges_with_dec: Vec<(f64, f64, Option<f64>)> = vec![
            // labels: left edge fixed at 72, ragged right edges.
            (72.0, 110.0, None),
            (72.0, 190.0, None),
            (72.0, 130.0, None),
            // amounts "1,5" / "12,00" / "3,75"-like: separator X = 500, scattered edges.
            (480.0, 510.0, Some(500.0)),
            (450.0, 525.0, Some(500.0)),
            (440.0, 545.0, Some(500.0)),
        ];
        let cols = detect_columns(&edges_with_dec, 20.0);
        assert_eq!(cols.len(), 2, "label + decimal amount = 2 columns, got {cols:?}");
        assert_eq!(cols[0].align, Align::Left, "label column is left-aligned");
        assert!(!cols[0].decimal, "label column is not a decimal column");
        // The amount column is a decimal column anchored on the separator X (≈500),
        // reported as right-aligned (no Align::Decimal variant).
        assert!(cols[1].decimal, "amount column is detected as decimal");
        assert_eq!(cols[1].align, Align::Right, "decimal column reports as Right");
        assert!(
            (cols[1].anchor - 500.0).abs() < 1.0,
            "decimal anchor ≈ separator X 500, got {}",
            cols[1].anchor
        );

        // Proof the decimal path is what saves it: drop the separator info and the
        // SAME scattered edges no longer yield an amount column at all (R8 alone).
        let edges_no_dec: Vec<(f64, f64, Option<f64>)> =
            edges_with_dec.iter().map(|&(l, r, _)| (l, r, None)).collect();
        let r8_only = detect_columns(&edges_no_dec, 20.0);
        assert!(
            r8_only.len() < 2,
            "without the decimal family these amounts shatter (no 2nd column), got {r8_only:?}"
        );
    }

    #[test]
    fn borderless_varying_decimal_amounts_is_one_amount_column() {
        // R10 integration: a "Libellé … Montant" table where amounts have DIFFERENT
        // decimal counts (1, 2 and 3 decimals) so their separators align but their
        // left edges scatter (436..468, spread > col_gap) — exactly the case R8's
        // right-edge approximation could not hold. Must resolve to a 2-column table
        // (label + amount), with the amount column right-aligned.
        // Amounts placed so the decimal separator sits at X≈480 (8px/char widths).
        let runs = vec![
            // Header.
            run("Libelle", 72.0, 700.0, 56.0), // right 128
            run("Montant", 440.0, 700.0, 56.0), // header over the amount column
            // "5,5"  (1 decimal)   x=468 w=24  right=492
            run("Service", 72.0, 684.0, 56.0),
            run("5,5", 468.0, 684.0, 24.0),
            // "1.250,00" (2 decimals) x=436 w=64 right=500
            run("Materiel divers", 72.0, 668.0, 120.0),
            run("1.250,00", 436.0, 668.0, 64.0),
            // "99,750" (3 decimals) x=460 w=48 right=508
            run("Quantite", 72.0, 652.0, 64.0),
            run("99,750", 460.0, 652.0, 48.0),
        ];
        let lines = group_into_lines(&runs);
        let table = borderless_table(&lines).expect("decimal-aligned amounts form a table");
        assert_eq!(
            table.rows.len(),
            4,
            "four rows (header + 3 lines), got {}",
            table.rows.len()
        );
        // Each row has exactly two cells: the amount column did not shatter into
        // several decimal-count columns.
        for (ri, row) in table.rows.iter().enumerate() {
            assert_eq!(
                row.cells.len(),
                2,
                "row {ri} must have 2 cells (label + amount), got {}",
                row.cells.len()
            );
        }
        // Amounts land together in column 1.
        assert_eq!(cell_text(&table.rows[2].cells[1]), "1.250,00");
        assert_eq!(cell_text(&table.rows[3].cells[1]), "99,750");
        // The amount column is right-aligned (decimal reported as Right).
        assert_eq!(
            cell_align(&table.rows[2].cells[1]),
            Align::Right,
            "amount cell is right-aligned"
        );
        assert_eq!(
            cell_align(&table.rows[1].cells[0]),
            Align::Left,
            "label cell is left-aligned"
        );
        // Borderless ⇒ no widened border.
        assert_eq!(table.border.width, 0.0);
    }

    #[test]
    fn decimals_scattered_in_prose_are_not_promoted_to_a_table() {
        // R10 anti-prose: a single column of body text that merely *mentions*
        // numbers with decimals must not become a table. The lines share their left
        // edge (prose), there is no aligned separator forming a real column, and no
        // second column — so the decimal family must not manufacture one.
        let runs = vec![
            run("The total was 12,50 last quarter", 72.0, 700.0, 190.0),
            run("but only 3,7 in the prior period", 72.0, 686.0, 188.0),
            run("and roughly 145,99 the year before", 72.0, 672.0, 196.0),
        ];
        let lines = group_into_lines(&runs);
        let plan = plan_tables(&lines, &[], &BTreeSet::new());
        assert!(
            plan.take_if_starts_at(0).is_none(),
            "prose mentioning decimals stays prose"
        );
        assert!(
            borderless_table(&lines).is_none(),
            "no table is built from prose with stray decimals"
        );
    }
}
