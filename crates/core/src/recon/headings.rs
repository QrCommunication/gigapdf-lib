//! Stage 4 — **heading promotion**. A paragraph that is *short* (1–2 lines) and
//! visually prominent — its font size exceeds `1.15 × the body median`, or it is
//! bold and short — is reclassified as a [`Heading`]. The heading **level**
//! (`1..=6`, bigger ⇒ lower number ⇒ more important) is decided by *clustering*
//! the distinct heading-candidate font sizes actually present in the document,
//! not by fixed global size ratios: the largest cluster maps to `H1`, the next
//! to `H2`, and so on, monotonically and with **no skipped levels** for sizes
//! that are present.
//!
//! Promotion operates on an already-built paragraph [`Block`] so the frame, runs
//! and alignment carry over unchanged; only the kind changes.
//!
//! The cluster map ([`HeadingLevels`]) is precomputed once from the page's lines
//! (see [`HeadingLevels::from_lines`]) and threaded into [`promote`]: it gives a
//! page a *stable* hierarchy regardless of how many distinct heading sizes it
//! happens to use. Fixed ratio buckets are why heading levels used to skip (a
//! 24/18/14-over-11pt document mapping to H1/H3/H4) and why a heading only
//! ~1.1× the body always collapsed to H6.
//!
//! ## Document-wide leveling
//!
//! Clustering a *single page* in isolation makes the **same visual size** land
//! on different levels across pages: a page whose largest heading is 14 pt maps
//! 14 pt to `H1`, while a page that also has a 24 pt title maps the same 14 pt
//! to `H2`. To give a multi-page document a *coherent* outline, the same
//! clustering primitives are reused over the **whole document**: collect every
//! already-promoted heading's representative size (see [`heading_size`]), build
//! one [`HeadingLevels`] from that set ([`HeadingLevels::from_sizes`]), and
//! re-stamp each heading's level from it ([`relevel`]). A 14 pt heading then
//! resolves to the *same* level on every page. The per-page promotion gate
//! ([`promote`]) is unchanged — it still decides *which* lines are headings,
//! relative to each page's own body — only the level *assignment* becomes
//! document-wide. A one-page document collects exactly the sizes its single
//! page already clustered, so its levels are identical to the pre-pass result.

use crate::model::{Block, BlockKind, Heading};
use crate::recon::lines::ReconLine;

/// The font-size multiple above the body median at which a short line becomes a
/// heading.
const HEADING_RATIO: f64 = 1.15;

/// A bold short line qualifies as a heading even at (≈) body size — common for
/// run-in subheadings. This is the floor for the *bold* path.
const BOLD_SUBHEAD_RATIO: f64 = 0.98;

/// Two heading sizes within this relative tolerance belong to the **same** level
/// (e.g. 23.9pt and 24pt are the same heading rank). Sizes that differ by more
/// than this open a new, deeper level. `0.06` ⇒ within 6 %.
const CLUSTER_TOLERANCE: f64 = 0.06;

/// How much larger than the next tier a **one-shot** top size must be before
/// the document-wide leveling reads it as a cover *title* that skews the scale
/// (and stops it consuming the `H1` rank). Well beyond [`CLUSTER_TOLERANCE`],
/// so two adjacent real tiers never trip it.
const TITLE_DEMOTION_RATIO: f64 = 1.25;

/// The maximum heading level the model expresses (HTML `h1..h6`).
const MAX_LEVEL: u8 = 6;

/// The most visual lines a heading may span (gap #75, sub-item 5). A title set in
/// a large font commonly wraps to two or three lines, so the `big`-path promotion
/// must tolerate up to three; a block longer than this is prose, never a heading.
const MAX_HEADING_LINES: usize = 3;

/// A page's heading-size hierarchy: the distinct font sizes used by heading
/// candidates, clustered and ranked so each maps to a stable level `1..=6`.
///
/// Built once per page from its lines, then consulted by [`promote`] for every
/// paragraph. The largest cluster is `H1`; deeper clusters increment the level,
/// capped at [`MAX_LEVEL`]. Because the ranks come from the sizes *present*
/// (not fixed ratios), the levels are monotonic and never skip: three distinct
/// sizes always map to exactly `1, 2, 3`.
#[derive(Debug, Clone, Default)]
pub struct HeadingLevels {
    /// Cluster representative sizes, **descending** (largest first). Index `i`
    /// is heading level `i + 1` (saturated at [`MAX_LEVEL`]). Empty when the
    /// page has no heading candidates.
    clusters: Vec<f64>,
    /// How many candidate sizes each cluster absorbed, parallel to `clusters`.
    /// Per-page leveling ignores it; the **document-wide** pass ([`relevel`])
    /// reads it to spot a one-shot cover title that would otherwise skew the
    /// whole scale (see [`Self::one_shot_cover_title`]).
    counts: Vec<usize>,
}

impl HeadingLevels {
    /// Cluster the heading-candidate sizes found on `lines` (calibrated against
    /// the page `body` size) into ranked levels. Lines that are neither big
    /// enough nor bold-short are ignored, so body prose never dilutes the
    /// hierarchy.
    pub fn from_lines(lines: &[ReconLine], body: f64) -> Self {
        let sizes = lines.iter().filter_map(|line| {
            let size = line.font_size();
            is_candidate_size(size, line.is_bold(), body).then_some(size)
        });
        Self::from_sizes(sizes)
    }

    /// Build the hierarchy from an iterator of candidate sizes. Sizes are
    /// clustered by relative proximity, then ranked largest-first.
    ///
    /// This is the shared clustering primitive: [`from_lines`](Self::from_lines)
    /// feeds it a single page's candidate sizes, while a document-wide pass feeds
    /// it every page's already-promoted heading sizes (collected via
    /// [`heading_size`]) so one hierarchy covers the whole document. Fed the same
    /// set, it returns the same ranks — a one-page document therefore clusters
    /// identically whether built per-page or document-wide.
    pub fn from_sizes(sizes: impl IntoIterator<Item = f64>) -> Self {
        // Unique-ish, descending: collect, sort, then 1-D gap-split so that
        // sizes within `CLUSTER_TOLERANCE` of the open cluster's representative
        // collapse to one level and a clear drop starts the next, deeper level.
        let mut sorted: Vec<f64> = sizes.into_iter().filter(|s| s.is_finite()).collect();
        sorted.sort_by(|a, b| b.partial_cmp(a).unwrap_or(core::cmp::Ordering::Equal));

        let mut clusters: Vec<f64> = Vec::new();
        let mut counts: Vec<usize> = Vec::new();
        for size in sorted {
            match clusters.last() {
                // Within tolerance of the current (larger) cluster representative
                // → same level. The representative stays the larger size.
                Some(&rep) if rep > 0.0 && size >= rep * (1.0 - CLUSTER_TOLERANCE) => {
                    if let Some(n) = counts.last_mut() {
                        *n += 1;
                    }
                }
                // A clear drop (or the first size) → a new, deeper level.
                _ => {
                    clusters.push(size);
                    counts.push(1);
                }
            }
        }
        Self { clusters, counts }
    }

    /// The heading level for a promoted line of representative font `size`.
    /// Resolves to the rank of the nearest cluster (`1`-based), capped at
    /// [`MAX_LEVEL`]. A run-in subhead at body size therefore lands on the
    /// *deepest present* level rather than always collapsing to `H6`. With no
    /// clusters recorded (degenerate), falls back to `1`.
    pub fn level_for(&self, size: f64) -> u8 {
        match self.nearest_cluster(size) {
            Some(idx) => clamp_level(idx + 1),
            None => 1,
        }
    }

    /// The index of the cluster whose representative is closest to `size`;
    /// ties favour the larger (lower-numbered) level. Clusters are descending,
    /// so the first minimal-distance index is the largest such size. `None`
    /// when no clusters were recorded (degenerate).
    fn nearest_cluster(&self, size: f64) -> Option<usize> {
        if self.clusters.is_empty() {
            return None;
        }
        let mut best_idx = 0usize;
        let mut best_dist = f64::INFINITY;
        for (idx, &rep) in self.clusters.iter().enumerate() {
            let dist = (rep - size).abs();
            if dist < best_dist {
                best_dist = dist;
                best_idx = idx;
            }
        }
        Some(best_idx)
    }

    /// Whether the **document-wide** hierarchy shows the cover-title pathology:
    /// the top cluster holds exactly ONE heading in the whole document (a
    /// one-shot cover/title page), it dwarfs the next tier (≥
    /// [`TITLE_DEMOTION_RATIO`]×), and at least two tiers exist below it with a
    /// *recurrent* first one — the document's real `H1`s. Ranking the one-shot
    /// as `H1` would push every real heading a level down (`H1` → `H2`…); the
    /// document-wide leveling (see [`relevel`]) instead treats it as a *title*
    /// and hands `H1` to the largest recurrent tier. All four conditions are
    /// required — an ordinary document whose single main title tops one tier of
    /// subheads keeps the historic `title = H1, subhead = H2` reading.
    fn one_shot_cover_title(&self) -> bool {
        self.clusters.len() >= 3
            && self.counts.first().copied().unwrap_or(0) == 1
            && self.counts.get(1).copied().unwrap_or(0) >= 2
            && self.clusters[0] >= self.clusters[1] * TITLE_DEMOTION_RATIO
    }

    /// [`level_for`](Self::level_for) with the document-wide renormalization:
    /// under the [`one_shot_cover_title`](Self::one_shot_cover_title) pathology
    /// the lone giant does not consume a rank — the largest *recurrent* tier is
    /// `H1` (the giant itself reports `1` too, the closest the six-level model
    /// has to a "Title"). Everywhere else this is exactly `level_for`.
    fn document_level_for(&self, size: f64) -> u8 {
        let Some(idx) = self.nearest_cluster(size) else {
            return 1;
        };
        if self.one_shot_cover_title() {
            clamp_level(idx.max(1))
        } else {
            clamp_level(idx + 1)
        }
    }
}

/// Whether a line of representative font `size` (and boldness `bold`) is a
/// heading **candidate** — i.e. it would survive [`promote`]'s gate on size
/// alone. Mirrors the `big || bold_subhead` test so the cluster set and the
/// promotion decision agree exactly. (Line length is checked later in
/// [`promote`]; size/weight is what defines a heading *tier*.)
fn is_candidate_size(size: f64, bold: bool, body: f64) -> bool {
    let big = size >= body * HEADING_RATIO;
    let bold_subhead = bold && size >= body * BOLD_SUBHEAD_RATIO;
    big || bold_subhead
}

/// Clamp a 1-based rank to the `1..=MAX_LEVEL` range.
fn clamp_level(rank: usize) -> u8 {
    rank.clamp(1, MAX_LEVEL as usize) as u8
}

/// Promote `block` to a heading when it qualifies; otherwise return it as-is.
/// `body` is the document body font size and `levels` the page's clustered
/// heading hierarchy (see [`HeadingLevels`]).
pub fn promote(block: Block, body: f64, levels: &HeadingLevels) -> Block {
    let BlockKind::Paragraph(para) = &block.kind else {
        return block;
    };
    let lines = line_count(para);
    if lines == 0 || lines > MAX_HEADING_LINES {
        return block;
    }
    let size = paragraph_size(para);
    let bold = paragraph_bold(para);
    let big = size >= body * HEADING_RATIO;
    // Bold + short qualifies even at body size (common for run-in subheadings),
    // but a single ordinary-weight body-size line must NOT become a heading.
    let bold_subhead = bold && size >= body * BOLD_SUBHEAD_RATIO;
    // Promotion gate (gap #75, sub-items 5 & 6):
    //  - the `big` (clearly larger-font) path may span up to `MAX_HEADING_LINES`:
    //    a genuine title wraps to two or three lines at heading size and must
    //    still be recognised, not collapsed back to a paragraph (sub-item 5);
    //  - the weaker bold-at-body-size path is gated to a **single isolated line**
    //    (sub-item 6, the bold-subhead isolation guard): a multi-line all-bold
    //    block is emphasized body text (a bold lead paragraph), not a heading.
    let qualifies = big || (bold_subhead && lines == 1);
    if !qualifies {
        return block;
    }
    let level = levels.level_for(size);
    Block {
        kind: BlockKind::Heading(Heading {
            level,
            para: para.clone(),
        }),
        ..block
    }
}

/// The representative font size of an already-promoted [`Heading`] — the same
/// "largest run size" rule [`promote`] used to pick the heading's level. Used by
/// a document-wide pass to collect every heading's size into one
/// [`HeadingLevels`] (so the same visual size maps to the same level on every
/// page) and then re-stamp levels via [`relevel`].
pub fn heading_size(heading: &Heading) -> f64 {
    paragraph_size(&heading.para)
}

/// Re-stamp `heading`'s [`level`](Heading::level) from a **document-wide**
/// `levels` map (typically built from [`heading_size`] over every page's
/// headings). The heading's content is untouched; only its rank changes, so a
/// heading of a given size resolves to the same level wherever it appears.
///
/// Pairs with [`heading_size`] to make heading levels consistent across pages
/// without re-running promotion: the per-page [`promote`] gate already chose
/// *which* blocks are headings; this only harmonizes their *levels*. With an
/// empty `levels` (no headings collected) the lookup yields `1`, so a lone
/// heading still gets a valid level. When the collected set shows a one-shot
/// cover title dwarfing a recurrent hierarchy, the giant is read as a *title*
/// instead of consuming `H1` — the largest recurrent tier keeps the top rank
/// (see [`HeadingLevels::one_shot_cover_title`]); per-page leveling is
/// untouched.
pub fn relevel(heading: &mut Heading, levels: &HeadingLevels) {
    heading.level = levels.document_level_for(heading_size(heading));
}

/// The least distinct pages a same-text/same-position heading must appear on
/// before it is read as repeated page chrome rather than a real heading.
const REPEATED_HEADING_MIN_PAGES: usize = 3;

/// Position quantum (points) for the repeated-heading key — a running header
/// re-draws at the same coordinates give or take sub-point jitter.
const REPEATED_HEADING_QUANTUM: f64 = 4.0;

/// Demote **running-header artifacts** promoted as headings: a heading whose
/// exact text sits at the same position on [`REPEATED_HEADING_MIN_PAGES`]+
/// distinct pages is repeated page chrome (a large-font running header the
/// header/footer stripper missed), not document structure — kept as a heading
/// it would pollute the outline with one `H1` per page. Each such block is
/// demoted back to the plain [`Paragraph`](crate::model::BlockKind::Paragraph)
/// it wraps; nothing else changes (frame, runs and style carry over).
///
/// Page-number-bearing headers differ in text page-to-page and are (correctly)
/// left alone — this pass only ever removes *identical* repeats, so a genuine
/// heading can never match: a document does not repeat the same section title
/// at the same coordinates on three pages.
///
/// Self-contained and document-wide by design: the caller hands the same
/// `sections` slice `reconstruct_model`'s releveling pass walks. (Wiring into
/// that pass lives with the `document.rs` owner; until then the behaviour is
/// exercised directly.)
pub fn demote_repeated_headings(sections: &mut [crate::model::Section]) {
    use std::collections::{BTreeSet, HashMap};

    // First pass: how many distinct pages each (text, position) heading key
    // appears on, page ordinals counted across all sections.
    let mut seen: HashMap<(String, i64, i64), BTreeSet<usize>> = HashMap::new();
    let mut page_no = 0usize;
    for section in sections.iter() {
        for page in &section.pages {
            for block in &page.blocks {
                if let (BlockKind::Heading(h), Some(frame)) = (&block.kind, block.frame) {
                    if let Some(key) = repeated_heading_key(h, &frame) {
                        seen.entry(key).or_default().insert(page_no);
                    }
                }
            }
            page_no += 1;
        }
    }
    seen.retain(|_, pages| pages.len() >= REPEATED_HEADING_MIN_PAGES);
    if seen.is_empty() {
        return;
    }

    // Second pass: demote every occurrence of a repeated key.
    for section in sections.iter_mut() {
        for page in &mut section.pages {
            for block in &mut page.blocks {
                let demoted = match (&block.kind, block.frame) {
                    (BlockKind::Heading(h), Some(frame)) => repeated_heading_key(h, &frame)
                        .filter(|key| seen.contains_key(key))
                        .map(|_| h.para.clone()),
                    _ => None,
                };
                if let Some(para) = demoted {
                    block.kind = BlockKind::Paragraph(para);
                }
            }
        }
    }
}

/// The repeat-detection key of a heading: its flattened text (trimmed,
/// case-folded) plus its quantized frame position. `None` for an empty title —
/// nothing to match on.
fn repeated_heading_key(
    heading: &Heading,
    frame: &crate::model::Rect,
) -> Option<(String, i64, i64)> {
    let text = super::heading_title(heading).trim().to_lowercase();
    if text.is_empty() {
        return None;
    }
    let q = REPEATED_HEADING_QUANTUM;
    Some((
        text,
        (frame.x / q).round() as i64,
        (frame.y / q).round() as i64,
    ))
}

/// Number of visual lines in a paragraph = 1 + the count of explicit line breaks.
fn line_count(para: &crate::model::Paragraph) -> usize {
    use crate::model::Inline;
    let breaks = para
        .runs
        .iter()
        .filter(|r| matches!(r, Inline::LineBreak))
        .count();
    let has_text = para
        .runs
        .iter()
        .any(|r| matches!(r, Inline::Run(_) | Inline::Image(_) | Inline::Link { .. }));
    if !has_text {
        0
    } else {
        breaks + 1
    }
}

/// The representative font size of a paragraph = the largest run size (a heading
/// line is sized by its dominant glyphs).
fn paragraph_size(para: &crate::model::Paragraph) -> f64 {
    use crate::model::Inline;
    para.runs
        .iter()
        .filter_map(|r| match r {
            Inline::Run(run) => Some(run.style.size_pt),
            _ => None,
        })
        .fold(0.0_f64, f64::max)
}

/// Whether every text run in the paragraph is bold.
fn paragraph_bold(para: &crate::model::Paragraph) -> bool {
    use crate::model::Inline;
    let mut any = false;
    for r in &para.runs {
        if let Inline::Run(run) = r {
            any = true;
            if !run.style.bold {
                return false;
            }
        }
    }
    any
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        geom::Rotation, Align, BlockId, CharStyle, Inline, InlineRun, Paragraph, ParagraphStyle,
    };

    /// A `HeadingLevels` built directly from the given candidate sizes
    /// (descending ranks), for tests that promote a single block.
    fn levels(sizes: &[f64]) -> HeadingLevels {
        HeadingLevels::from_sizes(sizes.iter().copied())
    }

    fn para_block(text: &str, size: f64, bold: bool, breaks: usize) -> Block {
        let mut runs = vec![Inline::Run(InlineRun {
            text: text.to_string(),
            style: CharStyle {
                size_pt: size,
                bold,
                ..CharStyle::default()
            },
            source_index: None,
        })];
        for _ in 0..breaks {
            runs.push(Inline::LineBreak);
            runs.push(Inline::Run(InlineRun {
                text: "more".into(),
                style: CharStyle {
                    size_pt: size,
                    bold,
                    ..CharStyle::default()
                },
                source_index: None,
            }));
        }
        Block {
            id: BlockId(0),
            frame: None,
            rotation: Rotation::D0,
            kind: BlockKind::Paragraph(Paragraph {
                style: ParagraphStyle {
                    align: Align::Left,
                    ..ParagraphStyle::default()
                },
                style_ref: None,
                runs,
            }),
        }
    }

    #[test]
    fn large_short_line_becomes_heading_level_one() {
        // 24pt is the only heading size over a 12pt body → its single cluster is H1.
        let lv = levels(&[24.0]);
        let block = promote(para_block("Chapter Title", 24.0, false, 0), 12.0, &lv);
        match block.kind {
            BlockKind::Heading(h) => assert_eq!(h.level, 1),
            _ => panic!("expected heading"),
        }
    }

    #[test]
    fn body_size_paragraph_stays_a_paragraph() {
        let lv = levels(&[]);
        let block = promote(para_block("ordinary body line", 12.0, false, 0), 12.0, &lv);
        assert!(matches!(block.kind, BlockKind::Paragraph(_)));
    }

    #[test]
    fn long_large_paragraph_is_not_a_heading() {
        // Large font but 4 lines (3 breaks) → past `MAX_HEADING_LINES`, so it is
        // prose, not a heading, however large the font.
        let lv = levels(&[20.0]);
        let block = promote(para_block("big but long", 20.0, false, 3), 12.0, &lv);
        assert!(matches!(block.kind, BlockKind::Paragraph(_)));
    }

    #[test]
    fn three_line_large_title_is_promoted() {
        // A title set in a clearly larger font (20pt over a 12pt body) wraps to
        // three lines (2 breaks) — still a heading (gap #75 #5), not collapsed back
        // to a paragraph by the old 2-line cap.
        let lv = levels(&[20.0]);
        let block = promote(
            para_block("A rather long chapter title", 20.0, false, 2),
            12.0,
            &lv,
        );
        match block.kind {
            BlockKind::Heading(h) => assert_eq!(h.level, 1),
            _ => panic!("a 3-line large title should be a heading"),
        }
    }

    #[test]
    fn multiline_bold_body_is_not_a_heading() {
        // A two-line all-bold block at body size is emphasized body text, not a
        // heading: the bold-subhead path is gated to a single isolated line
        // (gap #75 #6, the bold-subhead isolation guard).
        let lv = levels(&[]);
        let block = promote(para_block("bold lead spanning", 12.0, true, 1), 12.0, &lv);
        assert!(
            matches!(block.kind, BlockKind::Paragraph(_)),
            "multi-line bold body is not a heading"
        );
        // The single-line bold subhead still promotes (the run-in subheading case).
        let lv1 = levels(&[12.0]);
        let one = promote(para_block("Bold subhead", 12.0, true, 0), 12.0, &lv1);
        assert!(
            matches!(one.kind, BlockKind::Heading(_)),
            "one-line bold subhead promotes"
        );
    }

    // ── clustering of the distinct heading sizes present ─────────────────────

    #[test]
    fn three_distinct_sizes_map_to_monotonic_levels() {
        // 24/18/14pt headings over an 11pt body. Fixed ratio buckets would give
        // 1/3/4 (skipping 2); clustering the *present* sizes gives 1/2/3.
        let lv = levels(&[24.0, 18.0, 14.0]);
        assert_eq!(lv.level_for(24.0), 1);
        assert_eq!(lv.level_for(18.0), 2);
        assert_eq!(lv.level_for(14.0), 3);
    }

    #[test]
    fn heading_just_above_body_is_detected_and_not_h6() {
        // A heading only ~1.15× the body (12.65pt over 11pt) must be a heading,
        // and — being the only/largest present heading tier — H1, never H6.
        let body = 11.0;
        let size = 12.65; // exactly 1.15 × body
        let lv = levels(&[size]);
        let block = promote(para_block("Run-in lead", size, true, 0), body, &lv);
        match block.kind {
            BlockKind::Heading(h) => {
                assert_eq!(h.level, 1, "1.15× heading must not collapse to H6")
            }
            _ => panic!("a 1.15× line should be a heading"),
        }
    }

    #[test]
    fn single_heading_size_is_one_consistent_level() {
        // Several headings, all the same size → one cluster → one level for all.
        let lv = levels(&[16.0, 16.0, 16.0]);
        assert_eq!(lv.level_for(16.0), 1);
        // A near-identical size (within tolerance) shares the level.
        assert_eq!(lv.level_for(15.8), 1);
    }

    #[test]
    fn nearby_sizes_collapse_into_one_level() {
        // 24.0 and 23.5 differ by ~2 % (< 6 % tolerance) → same level; 18 is the
        // next, deeper level. So only two ranks despite three distinct sizes.
        let lv = levels(&[24.0, 23.5, 18.0]);
        assert_eq!(lv.level_for(24.0), 1);
        assert_eq!(lv.level_for(23.5), 1);
        assert_eq!(lv.level_for(18.0), 2);
    }

    #[test]
    fn bold_run_in_subhead_lands_on_the_deepest_present_level() {
        // A bold ≈body-size run-in subhead alongside a 24pt H1: the subhead is
        // its own (smallest) cluster → the deeper level, not forced to H6.
        let body = 12.0;
        let lv = levels(&[24.0, 12.0]); // 12.0 enters via the bold path at the caller
        assert_eq!(lv.level_for(24.0), 1);
        assert_eq!(lv.level_for(12.0), 2);
        let block = promote(para_block("Subsection", body, true, 0), body, &lv);
        match block.kind {
            BlockKind::Heading(h) => assert_eq!(h.level, 2),
            _ => panic!("expected heading for bold short line"),
        }
    }

    #[test]
    fn lone_bold_body_size_subhead_is_a_single_level_heading() {
        // With no larger headings present, a lone bold body-size run-in subhead
        // is the only tier → a heading at the single present level (1), not H6.
        let body = 12.0;
        let lv = levels(&[12.0]);
        let block = promote(para_block("Subsection", body, true, 0), body, &lv);
        match block.kind {
            BlockKind::Heading(h) => assert_eq!(h.level, 1),
            _ => panic!("expected heading for lone bold short line"),
        }
    }

    #[test]
    fn levels_are_capped_at_six() {
        // Eight distinct descending sizes → ranks saturate at H6 for the deepest.
        let lv = levels(&[40.0, 34.0, 28.0, 24.0, 20.0, 17.0, 14.0, 12.5]);
        assert_eq!(lv.level_for(40.0), 1);
        assert_eq!(lv.level_for(17.0), 6);
        assert_eq!(lv.level_for(14.0), 6);
        assert_eq!(lv.level_for(12.5), 6);
    }

    #[test]
    fn empty_hierarchy_is_harmless() {
        // No candidates recorded → a degenerate lookup returns level 1 (and the
        // promote gate would normally reject body text anyway).
        let lv = levels(&[]);
        assert_eq!(lv.level_for(20.0), 1);
    }

    #[test]
    fn from_lines_ignores_body_prose() {
        use crate::recon::ReconRun;

        // One 24pt title line + several 12pt body lines over a 12pt body. Only
        // the title is a candidate, so the hierarchy has a single level.
        let mk = |text: &str, size: f64, bold: bool| ReconLine {
            runs: vec![ReconRun {
                text: text.into(),
                x: 0.0,
                y: 0.0,
                w: text.len() as f64 * size * 0.5,
                h: size,
                size,
                style: crate::convert::style::TextStyle {
                    bold,
                    ..crate::convert::style::TextStyle::default()
                },
                rotation: 0.0,
                source_index: None,
                underline: false,
                strike: false,
            }],
            x: 0.0,
            y: 0.0,
            w: text.len() as f64 * size * 0.5,
            h: size,
        };
        let lines = vec![
            mk("Document Title", 24.0, false),
            mk("First body line", 12.0, false),
            mk("Second body line", 12.0, false),
        ];
        let lv = HeadingLevels::from_lines(&lines, 12.0);
        assert_eq!(lv.level_for(24.0), 1, "the lone title is H1");
        // The body size never entered the hierarchy: there is exactly one cluster.
        assert_eq!(lv.clusters.len(), 1);
    }

    // ── document-wide leveling primitives (heading_size + relevel) ───────────

    /// The `Heading` a promotion produced for a block of one font size.
    fn heading_block(text: &str, size: f64) -> Heading {
        match promote(para_block(text, size, false, 0), 12.0, &levels(&[size])).kind {
            BlockKind::Heading(h) => h,
            _ => panic!("expected the block to promote to a heading"),
        }
    }

    #[test]
    fn heading_size_is_the_dominant_run_size() {
        // `heading_size` mirrors the promotion rule (largest run size), so the
        // document-wide collector sees the same size promotion ranked on.
        let h = heading_block("Title", 24.0);
        assert_eq!(heading_size(&h), 24.0);
    }

    #[test]
    fn relevel_restamps_from_the_document_wide_map() {
        // A 14pt heading promoted on a page that knew only 14pt was H1 per-page.
        let mut h = heading_block("Subhead", 14.0);
        assert_eq!(h.level, 1, "per-page, the lone 14pt heading is H1");
        // Document-wide a 24pt title also exists → the 14pt heading is H2.
        let doc = levels(&[24.0, 14.0]);
        relevel(&mut h, &doc);
        assert_eq!(h.level, 2, "document-wide, 14pt sits below the 24pt title");
    }

    #[test]
    fn relevel_is_consistent_across_pages_for_one_size() {
        // The crux of the document-wide pass: the SAME visual size must resolve to
        // the SAME level regardless of which page it came from. Page 1 had a 24pt
        // title + a 14pt subhead (so 14pt was H2 there); page 2 had only a 14pt
        // heading (H1 there). After re-leveling against the doc-wide set, both
        // 14pt headings are H2 and below the 24pt title.
        let doc = levels(&[24.0, 14.0, 14.0]);
        let mut page1_sub = heading_block("Page 1 subhead", 14.0);
        let mut page2_head = heading_block("Page 2 heading", 14.0);
        let mut title = heading_block("Page 1 title", 24.0);
        relevel(&mut page1_sub, &doc);
        relevel(&mut page2_head, &doc);
        relevel(&mut title, &doc);
        assert_eq!(title.level, 1, "the 24pt title is H1");
        assert_eq!(page1_sub.level, page2_head.level, "same size ⇒ same level");
        assert_eq!(
            page2_head.level, 2,
            "14pt is H2 everywhere, below the title"
        );
    }

    #[test]
    fn relevel_with_empty_map_falls_back_to_level_one() {
        // Degenerate: no sizes collected (e.g. no headings) ⇒ a valid level, not a
        // panic. (The document-wide caller skips an empty set, but `relevel` is
        // still defined.)
        let mut h = heading_block("Lonely", 20.0);
        relevel(&mut h, &levels(&[]));
        assert_eq!(h.level, 1);
    }

    #[test]
    fn document_wide_set_clusters_like_a_single_page_for_one_page() {
        // No-regression invariant: a one-page document collects exactly the sizes
        // its single page already clustered, so the document-wide hierarchy equals
        // the per-page one — identical levels for every size.
        let page = levels(&[24.0, 18.0, 14.0]);
        let doc = levels(&[24.0, 18.0, 14.0]); // same set a 1-page collect yields
        for size in [24.0, 18.0, 14.0] {
            assert_eq!(page.level_for(size), doc.level_for(size));
        }
    }

    // ── V3c fix 6b: a one-shot cover title must not skew the whole scale ─────

    /// Relevel a heading of `size` against the document-wide `doc` map.
    fn releveled(size: f64, doc: &HeadingLevels) -> u8 {
        let mut h = heading_block("h", size);
        relevel(&mut h, doc);
        h.level
    }

    #[test]
    fn one_shot_cover_title_hands_h1_to_the_recurrent_tier() {
        // A single 36pt cover title over three 24pt chapter heads and two 18pt
        // section heads. Rank-as-collected would make the chapters H2 and the
        // sections H3; the document-wide leveling reads the lone giant as a
        // *title* (reported H1 — the closest the model has) and renormalizes:
        // the recurrent 24pt tier IS the document's H1.
        let doc = levels(&[36.0, 24.0, 24.0, 24.0, 18.0, 18.0]);
        assert_eq!(releveled(24.0, &doc), 1, "recurrent chapters are H1");
        assert_eq!(releveled(18.0, &doc), 2, "sections follow at H2");
        assert_eq!(releveled(36.0, &doc), 1, "the cover title reports H1");
        // Per-page leveling (promotion) is untouched by the renormalization.
        assert_eq!(doc.level_for(36.0), 1);
        assert_eq!(doc.level_for(24.0), 2);
        assert_eq!(doc.level_for(18.0), 3);
    }

    #[test]
    fn ordinary_hierarchies_never_renormalize() {
        // A normal document: one main title over one tier of recurrent
        // subheads — the historic reading (title H1, subheads H2) holds.
        let two_tiers = levels(&[24.0, 14.0, 14.0]);
        assert_eq!(releveled(24.0, &two_tiers), 1);
        assert_eq!(releveled(14.0, &two_tiers), 2);
        // Three tiers, every size a one-shot (nothing recurrent below the
        // top): collected ranks stand — 1/2/3.
        let all_lone = levels(&[36.0, 24.0, 18.0]);
        assert_eq!(releveled(36.0, &all_lone), 1);
        assert_eq!(releveled(24.0, &all_lone), 2);
        assert_eq!(releveled(18.0, &all_lone), 3);
        // The top size barely exceeds the next tier (< 1.25×): not a scale-
        // skewing giant, ranks stand.
        let mild = levels(&[26.0, 24.0, 24.0, 18.0, 18.0]);
        assert_eq!(releveled(24.0, &mild), 2);
        assert_eq!(releveled(26.0, &mild), 1);
        // A recurrent TOP tier is a real H1 tier, never a title.
        let recurrent_top = levels(&[36.0, 36.0, 24.0, 24.0, 18.0]);
        assert_eq!(releveled(36.0, &recurrent_top), 1);
        assert_eq!(releveled(24.0, &recurrent_top), 2);
    }

    // ── V3c fix 6a: repeated running-header headings are demoted ─────────────

    use crate::model::{Page, Rect, Section};

    /// A heading block with a frame, as the reconstruction produces.
    fn framed_heading(text: &str, size: f64, frame: Rect) -> Block {
        let mut block = para_block(text, size, false, 0);
        let BlockKind::Paragraph(para) = block.kind else {
            unreachable!()
        };
        block.kind = BlockKind::Heading(Heading { level: 1, para });
        block.frame = Some(frame);
        block
    }

    fn section_of_pages(pages: Vec<Vec<Block>>) -> Section {
        Section {
            geometry: crate::model::geom::PageGeometry::default(),
            header: None,
            footer: None,
            pages: pages
                .into_iter()
                .map(|blocks| Page {
                    blocks,
                    absolute: false,
                })
                .collect(),
            ..Default::default()
        }
    }

    fn heading_count(sections: &[Section]) -> usize {
        sections
            .iter()
            .flat_map(|s| &s.pages)
            .flat_map(|p| &p.blocks)
            .filter(|b| matches!(b.kind, BlockKind::Heading(_)))
            .count()
    }

    #[test]
    fn a_running_header_on_three_pages_is_demoted_to_paragraphs() {
        // The same large-font "ACME CORP" at the same spot on every page — page
        // chrome that survived the header stripper — plus one genuine title.
        let chrome = || framed_heading("ACME CORP", 16.0, Rect::new(72.0, 30.0, 200.0, 18.0));
        let mut sections = vec![section_of_pages(vec![
            vec![
                chrome(),
                framed_heading("Introduction", 24.0, Rect::new(72.0, 120.0, 300.0, 26.0)),
            ],
            vec![chrome()],
            vec![chrome()],
        ])];
        demote_repeated_headings(&mut sections);
        assert_eq!(
            heading_count(&sections),
            1,
            "only the genuine title stays a heading"
        );
        // The demoted chrome keeps its content as a plain paragraph.
        assert!(matches!(
            &sections[0].pages[1].blocks[0].kind,
            BlockKind::Paragraph(p)
                if matches!(p.runs.first(), Some(crate::model::Inline::Run(r)) if r.text == "ACME CORP")
        ));
    }

    #[test]
    fn a_twice_repeated_heading_is_not_chrome() {
        // Two occurrences (a title repeated on a part opener) stay headings —
        // the demotion needs three distinct pages.
        let repeat = || framed_heading("Part One", 20.0, Rect::new(72.0, 100.0, 250.0, 22.0));
        let mut sections = vec![section_of_pages(vec![vec![repeat()], vec![repeat()]])];
        demote_repeated_headings(&mut sections);
        assert_eq!(heading_count(&sections), 2, "two repeats keep their rank");
    }

    #[test]
    fn same_text_at_different_positions_is_not_chrome() {
        // "Summary" as a genuine section head at different heights on three
        // pages: same text, different position → not a running header.
        let mut sections = vec![section_of_pages(vec![
            vec![framed_heading("Summary", 18.0, Rect::new(72.0, 120.0, 200.0, 20.0))],
            vec![framed_heading("Summary", 18.0, Rect::new(72.0, 400.0, 200.0, 20.0))],
            vec![framed_heading("Summary", 18.0, Rect::new(72.0, 650.0, 200.0, 20.0))],
        ])];
        demote_repeated_headings(&mut sections);
        assert_eq!(heading_count(&sections), 3, "moving headings are structure");
    }
}
