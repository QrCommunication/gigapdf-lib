//! Heuristic table reconstruction: turn a page's positioned text runs back into
//! a row/column grid for spreadsheet export.
//!
//! A PDF has no notion of a cell — only glyphs at coordinates. We recover the
//! implicit grid: cluster runs into **rows** by vertical position and into
//! **columns** by horizontal start, then drop each run into `grid[row][col]`.
//! This handles regular tabular layouts (the "the PDF is a table" case); merged
//! and multi-line cells are approximated, not perfectly reconstructed.

use super::PlacedText;

/// Median of a slice (used to calibrate clustering tolerances; robust to a few
/// oversized title runs in a way the mean is not). Empty → fallback.
fn median(values: &mut [f64], fallback: f64) -> f64 {
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

/// Reconstruct a row/column grid from a page's text runs (top-down points). Each
/// inner `Vec<String>` is a row, left→right; missing cells are empty strings.
pub fn reconstruct(texts: &[PlacedText]) -> Vec<Vec<String>> {
    if texts.is_empty() {
        return Vec::new();
    }

    let mut heights: Vec<f64> = texts.iter().map(|t| t.height.max(1.0)).collect();
    let h_med = median(&mut heights, 10.0);
    let row_tol = h_med * 0.7;
    // A column break needs clearly more whitespace than inter-word spacing, so
    // prose lines collapse into a single column (every line lands in column A —
    // the document's text), while genuinely separated table columns still split.
    let col_gap = (h_med * 2.0).max(16.0);

    // ── Column anchors: cluster run start-x, split when the sorted gap exceeds
    //    col_gap. Each column is represented by its left edge (cluster min).
    let mut xs: Vec<f64> = texts.iter().map(|t| t.x).collect();
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
    let mut columns: Vec<f64> = vec![xs[0]];
    for &x in &xs[1..] {
        if x - *columns.last().unwrap() > col_gap {
            columns.push(x);
        }
    }
    let column_of = |x: f64| -> usize {
        columns
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

    // ── Rows: sort runs by vertical centre, then greedily band them.
    let mut runs: Vec<&PlacedText> = texts.iter().collect();
    runs.sort_by(|a, b| {
        let (ca, cb) = (a.y + a.height / 2.0, b.y + b.height / 2.0);
        ca.partial_cmp(&cb)
            .unwrap_or(core::cmp::Ordering::Equal)
            .then(a.x.partial_cmp(&b.x).unwrap_or(core::cmp::Ordering::Equal))
    });

    let mut grid: Vec<Vec<String>> = Vec::new();
    let mut row_center = f64::NEG_INFINITY;
    for run in runs {
        let center = run.y + run.height / 2.0;
        if grid.is_empty() || center - row_center > row_tol {
            grid.push(vec![String::new(); columns.len()]);
            row_center = center;
        }
        let col = column_of(run.x);
        let cell = grid.last_mut().unwrap();
        let text = run.text.trim();
        if cell[col].is_empty() {
            cell[col] = text.to_string();
        } else {
            // Multiple runs in one cell (e.g., split words): join with a space.
            cell[col].push(' ');
            cell[col].push_str(text);
        }
    }
    grid
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(text: &str, x: f64, y: f64) -> PlacedText {
        PlacedText {
            text: text.to_string(),
            x,
            y,
            width: 40.0,
            height: 10.0,
            style: crate::convert::TextStyle::default(),
        }
    }

    #[test]
    fn reconstructs_a_2x2_grid() {
        // Two rows (y≈100, y≈120), two columns (x≈50, x≈200).
        let texts = vec![
            cell("Name", 50.0, 100.0),
            cell("Age", 200.0, 100.0),
            cell("Alice", 50.0, 120.0),
            cell("30", 200.0, 120.0),
        ];
        let grid = reconstruct(&texts);
        assert_eq!(grid.len(), 2, "two rows");
        assert_eq!(grid[0], vec!["Name".to_string(), "Age".to_string()]);
        assert_eq!(grid[1], vec!["Alice".to_string(), "30".to_string()]);
    }

    #[test]
    fn aligns_columns_across_rows_with_jitter() {
        // Slight x jitter within a column must still land in the same column.
        let texts = vec![
            cell("A", 50.0, 100.0),
            cell("B", 201.5, 100.0),
            cell("C", 49.2, 130.0),
            cell("D", 199.8, 130.0),
        ];
        let grid = reconstruct(&texts);
        assert_eq!(grid.len(), 2);
        assert_eq!(grid[0].len(), 2, "exactly two columns despite jitter");
        assert_eq!(grid[0][0], "A");
        assert_eq!(grid[1][1], "D");
    }

    #[test]
    fn prose_lines_collapse_to_a_single_column() {
        // Left-aligned paragraph: same x, increasing y → one column, one row
        // per line. The document's text is preserved (not just tables).
        let texts = vec![
            cell("First line of the document", 72.0, 100.0),
            cell("Second line continues here", 72.0, 115.0),
            cell("Third and final line", 72.0, 130.0),
        ];
        let grid = reconstruct(&texts);
        assert_eq!(grid.len(), 3, "one row per text line");
        assert!(grid.iter().all(|r| r.len() == 1), "single column for prose");
        assert_eq!(grid[1][0], "Second line continues here");
    }

    #[test]
    fn empty_input_yields_empty_grid() {
        assert!(reconstruct(&[]).is_empty());
    }

    #[test]
    fn median_empty_returns_fallback() {
        // The fallback arm is only reachable directly (reconstruct never calls
        // median with an empty slice on a non-empty page).
        assert_eq!(median(&mut [], 42.0), 42.0);
        // Odd and even counts pick the middle / average-of-two.
        assert_eq!(median(&mut [3.0, 1.0, 2.0], 0.0), 2.0);
        assert_eq!(median(&mut [4.0, 2.0], 0.0), 3.0);
    }

    #[test]
    fn multiple_runs_in_one_cell_join_with_space() {
        // Two runs at the same position land in the same cell → joined by space.
        let texts = vec![cell("Hello", 50.0, 100.0), cell("World", 50.0, 100.0)];
        let grid = reconstruct(&texts);
        assert_eq!(grid.len(), 1);
        assert_eq!(grid[0][0], "Hello World");
    }
}
