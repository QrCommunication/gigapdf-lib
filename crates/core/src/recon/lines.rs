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
}
