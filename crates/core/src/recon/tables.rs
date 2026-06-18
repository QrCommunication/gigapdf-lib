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
use super::{char_style, median, ruling_orientation, IdGen, Ruling};
use crate::content::vector::VectorPath;
use crate::model::{
    geom::Rotation, Block, BlockKind, BorderStyle, Cell, Paragraph, ParagraphStyle, Rect, Row,
    Table,
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
pub fn plan_tables(lines: &[ReconLine], vpaths: &[VectorPath]) -> TablePlan {
    let mut plan = TablePlan::default();
    if lines.is_empty() {
        return plan;
    }

    if let Some(t) = plan_ruled(lines, vpaths) {
        plan.tables.push(t);
    }

    // Borderless fallback over lines not already covered.
    let claimed: BTreeSet<usize> = plan
        .tables
        .iter()
        .flat_map(|t| t.covered_lines.iter().copied())
        .collect();
    let free: Vec<usize> = (0..lines.len()).filter(|i| !claimed.contains(i)).collect();
    if let Some(t) = plan_borderless(lines, &free) {
        plan.tables.push(t);
    }
    plan
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
                styles[r][c] = Some(char_style(&run.style, run.size));
            }
        }
    }

    // Column widths from the edges.
    let col_widths: Vec<f64> = table.cols.windows(2).map(|w| (w[1] - w[0]).abs()).collect();

    let mut rows: Vec<Row> = Vec::with_capacity(n_rows);
    for r in 0..n_rows {
        // Row edges are top→bottom (descending Y); height is the gap.
        let height = (table.rows[r] - table.rows[r + 1]).abs();
        let mut cells: Vec<Cell> = Vec::with_capacity(n_cols);
        for c in 0..n_cols {
            // Empty interior cells stay empty cells (an unfilled span renders as
            // blank), so the grid shape is preserved rather than collapsed.
            let text = std::mem::take(&mut grid[r][c]);
            let style = styles[r][c].take();
            cells.push(make_cell(text, style, ids));
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

fn make_cell(text: String, style: Option<crate::model::CharStyle>, ids: &mut IdGen) -> Cell {
    use crate::model::{Inline, InlineRun};
    let runs = if text.is_empty() {
        Vec::new()
    } else {
        vec![Inline::Run(InlineRun {
            text,
            style: style.unwrap_or_default(),
            source_index: None,
        })]
    };
    let para = Block {
        id: ids.mint(),
        frame: None,
        rotation: Rotation::D0,
        kind: BlockKind::Paragraph(Paragraph {
            style: ParagraphStyle::default(),
            style_ref: None,
            runs,
        }),
    };
    Cell {
        blocks: vec![para],
        col_span: 1,
        row_span: 1,
        shading: None,
    }
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

/// Plan a ruled table from horizontal + vertical ruling lines. Needs ≥ 2 column
/// edges and ≥ 2 row edges to form at least one cell.
fn plan_ruled(lines: &[ReconLine], vpaths: &[VectorPath]) -> Option<PlannedTable> {
    let mut h_rules: Vec<(f64, f64, f64)> = Vec::new(); // (y, x0, x1)
    let mut v_rules: Vec<(f64, f64, f64)> = Vec::new(); // (x, y0, y1)
    let mut used_paths: BTreeSet<usize> = BTreeSet::new();

    for vp in vpaths {
        match ruling_orientation(vp) {
            Some(Ruling::Horizontal { y, x0, x1 }) => {
                h_rules.push((y, x0, x1));
                used_paths.insert(vp.index);
            }
            Some(Ruling::Vertical { x, y0, y1 }) => {
                v_rules.push((x, y0, y1));
                used_paths.insert(vp.index);
            }
            None => {}
        }
    }
    if h_rules.len() < 2 || v_rules.len() < 2 {
        return None;
    }

    let cols = cluster_edges(v_rules.iter().map(|r| r.0));
    let rows_asc = cluster_edges(h_rules.iter().map(|r| r.0));
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

    let start_line = *covered.iter().min()?;
    Some(PlannedTable {
        cols,
        rows,
        ruled: true,
        covered_lines: covered,
        used_paths,
        start_line,
    })
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

/// Plan a borderless table from the free lines. Only promotes when ≥ 2 rows
/// share ≥ 2 consistent column anchors, so prose is never turned into a table.
fn plan_borderless(lines: &[ReconLine], free: &[usize]) -> Option<PlannedTable> {
    if free.len() < 2 {
        return None;
    }

    // Column anchors from run start-x across the free lines.
    let mut xs: Vec<f64> = Vec::new();
    let mut heights: Vec<f64> = Vec::new();
    for &i in free {
        for r in &lines[i].runs {
            xs.push(r.x);
            heights.push(r.h.max(1.0));
        }
    }
    if xs.len() < 4 {
        return None;
    }
    let h_med = median(&mut heights, 10.0);
    let col_gap = (h_med * 2.0).max(16.0);
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
    let mut anchors: Vec<f64> = vec![xs[0]];
    for &x in &xs[1..] {
        if x - *anchors.last().unwrap() > col_gap {
            anchors.push(x);
        }
    }
    if anchors.len() < 2 {
        return None; // single column ⇒ prose, not a table
    }

    // Count, per free line, how many distinct anchors its runs hit. A table row
    // hits ≥ 2 anchors; a prose line hits 1.
    let anchor_of = |x: f64| -> usize {
        anchors
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                (x - **a)
                    .abs()
                    .partial_cmp(&(x - **b).abs())
                    .unwrap_or(core::cmp::Ordering::Equal)
            })
            .map(|(i, _)| i)
            .unwrap_or(0)
    };
    let mut row_lines: Vec<usize> = Vec::new();
    for &i in free {
        let mut hit: BTreeSet<usize> = BTreeSet::new();
        for r in &lines[i].runs {
            hit.insert(anchor_of(r.x));
        }
        if hit.len() >= 2 {
            row_lines.push(i);
        }
    }
    if row_lines.len() < 2 {
        return None; // need ≥ 2 tabular rows
    }

    // Build column edges midway between anchors (extend out by half a gap at the
    // ends), and row edges from the tabular lines' vertical extents.
    let mut cols: Vec<f64> = Vec::with_capacity(anchors.len() + 1);
    cols.push(anchors[0] - col_gap / 2.0);
    for w in anchors.windows(2) {
        cols.push((w[0] + w[1]) / 2.0);
    }
    cols.push(*anchors.last().unwrap() + col_gap * 4.0);

    // Row edges: above the top line and below the bottom line, plus midpoints.
    let mut centers: Vec<f64> = row_lines.iter().map(|&i| lines[i].center_y()).collect();
    centers.sort_by(|a, b| b.partial_cmp(a).unwrap_or(core::cmp::Ordering::Equal)); // desc
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
        let plan = plan_tables(&lines, &paths);
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
    fn prose_is_not_promoted_to_a_table() {
        // Single-column left-aligned prose must stay out of the table planner.
        let runs = vec![
            run("First line of body text", 72.0, 700.0, 150.0),
            run("Second line of body text", 72.0, 686.0, 150.0),
            run("Third line of the body", 72.0, 672.0, 150.0),
        ];
        let lines = group_into_lines(&runs);
        let plan = plan_tables(&lines, &[]);
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
        let plan = plan_tables(&lines, &[]);
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
}
