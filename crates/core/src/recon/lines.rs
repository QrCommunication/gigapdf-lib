//! Stage 1 — **line grouping**. Cluster a page's text runs into reading-order
//! lines: sort top→bottom (PDF *Y up*, so descending `y`) then left→right, and
//! start a new line whenever the vertical centre jumps by more than
//! `0.6 × median font size` (a baseline-band tolerance).
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
    /// The concatenated text of the line (runs joined by single spaces, unless a
    /// run already ends/starts with whitespace).
    pub fn text(&self) -> String {
        let mut s = String::new();
        for run in &self.runs {
            let t = run.text.trim();
            if t.is_empty() {
                continue;
            }
            if !s.is_empty()
                && !s.ends_with(char::is_whitespace)
                && !run.text.starts_with(char::is_whitespace)
            {
                s.push(' ');
            }
            s.push_str(t);
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
}

/// Group runs into [`ReconLine`]s. Runs are first ordered top→bottom then
/// left→right; a new line begins when the centre-to-centre gap exceeds the
/// baseline tolerance (`0.6 ×` the larger of the run / current-line height).
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
    let mut row_center = f64::INFINITY;
    let mut row_height = 0.0f64;

    for run in items {
        let c = run.center_y();
        let tol = run.h.max(row_height).max(1.0) * 0.6;
        if lines.is_empty() || (row_center - c).abs() > tol {
            lines.push(ReconLine {
                x: run.x,
                y: run.y,
                w: run.w,
                h: run.h,
                runs: vec![run.clone()],
            });
            row_center = c;
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
            row_height = row_height.max(run.h);
        }
    }

    // Within each line keep the runs left→right (they were inserted in that
    // order already by the global sort, but a later-added run from a band can
    // arrive out of order — re-sort defensively).
    for line in &mut lines {
        line.runs
            .sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(core::cmp::Ordering::Equal));
    }
    lines
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
}
