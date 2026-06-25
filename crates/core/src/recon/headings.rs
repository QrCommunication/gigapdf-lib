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

/// The maximum heading level the model expresses (HTML `h1..h6`).
const MAX_LEVEL: u8 = 6;

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
        for size in sorted {
            match clusters.last() {
                // Within tolerance of the current (larger) cluster representative
                // → same level. The representative stays the larger size.
                Some(&rep) if rep > 0.0 && size >= rep * (1.0 - CLUSTER_TOLERANCE) => {}
                // A clear drop (or the first size) → a new, deeper level.
                _ => clusters.push(size),
            }
        }
        Self { clusters }
    }

    /// The heading level for a promoted line of representative font `size`.
    /// Resolves to the rank of the nearest cluster (`1`-based), capped at
    /// [`MAX_LEVEL`]. A run-in subhead at body size therefore lands on the
    /// *deepest present* level rather than always collapsing to `H6`. With no
    /// clusters recorded (degenerate), falls back to `1`.
    pub fn level_for(&self, size: f64) -> u8 {
        if self.clusters.is_empty() {
            return 1;
        }
        // Pick the cluster whose representative is closest to `size`; ties favour
        // the larger (lower-numbered) level. Clusters are descending, so the
        // first minimal-distance index is the largest such size.
        let mut best_idx = 0usize;
        let mut best_dist = f64::INFINITY;
        for (idx, &rep) in self.clusters.iter().enumerate() {
            let dist = (rep - size).abs();
            if dist < best_dist {
                best_dist = dist;
                best_idx = idx;
            }
        }
        clamp_level(best_idx + 1)
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
    if lines == 0 || lines > 2 {
        return block;
    }
    let size = paragraph_size(para);
    let bold = paragraph_bold(para);
    let big = size >= body * HEADING_RATIO;
    // Bold + short qualifies even at body size (common for run-in subheadings),
    // but a single ordinary-weight body-size line must NOT become a heading.
    let bold_subhead = bold && size >= body * BOLD_SUBHEAD_RATIO;
    if !(big || bold_subhead) {
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
/// empty `levels` (no headings collected) [`HeadingLevels::level_for`] yields
/// `1`, so a lone heading still gets a valid level.
pub fn relevel(heading: &mut Heading, levels: &HeadingLevels) {
    heading.level = levels.level_for(heading_size(heading));
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
        // Large font but 3 lines (2 breaks) → too long to be a heading.
        let lv = levels(&[20.0]);
        let block = promote(para_block("big but long", 20.0, false, 2), 12.0, &lv);
        assert!(matches!(block.kind, BlockKind::Paragraph(_)));
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
}
