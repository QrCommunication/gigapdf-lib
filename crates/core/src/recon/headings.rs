//! Stage 4 — **heading promotion**. A paragraph that is *short* (1–2 lines) and
//! visually prominent — its font size exceeds `1.15 × the body median`, or it is
//! bold and short — is reclassified as a [`Heading`]. The font-size ratio over
//! the body median is bucketed into a level `1..=6` (bigger ⇒ lower level
//! number ⇒ more important).
//!
//! Promotion operates on an already-built paragraph [`Block`] so the frame, runs
//! and alignment carry over unchanged; only the kind changes.

use crate::model::{Block, BlockKind, Heading};

/// The font-size multiple above the body median at which a short line becomes a
/// heading.
const HEADING_RATIO: f64 = 1.15;

/// Promote `block` to a heading when it qualifies; otherwise return it as-is.
/// `body` is the document body font size.
pub fn promote(block: Block, body: f64) -> Block {
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
    let bold_subhead = bold && size >= body * 0.98;
    if !(big || bold_subhead) {
        return block;
    }
    let level = level_for(size, body);
    Block {
        kind: BlockKind::Heading(Heading {
            level,
            para: para.clone(),
        }),
        ..block
    }
}

/// Bucket the size ratio over the body median into a heading level `1..=6`.
/// A line at/below body size (a bold-only subheading) falls through to level 6.
fn level_for(size: f64, body: f64) -> u8 {
    let ratio = if body > 0.0 { size / body } else { 1.0 };
    if ratio >= 2.0 {
        1
    } else if ratio >= 1.7 {
        2
    } else if ratio >= 1.45 {
        3
    } else if ratio >= 1.25 {
        4
    } else if ratio >= HEADING_RATIO {
        5
    } else {
        6
    }
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
        // 24pt over a 12pt body → ratio 2.0 → level 1.
        let block = promote(para_block("Chapter Title", 24.0, false, 0), 12.0);
        match block.kind {
            BlockKind::Heading(h) => assert_eq!(h.level, 1),
            _ => panic!("expected heading"),
        }
    }

    #[test]
    fn body_size_paragraph_stays_a_paragraph() {
        let block = promote(para_block("ordinary body line", 12.0, false, 0), 12.0);
        assert!(matches!(block.kind, BlockKind::Paragraph(_)));
    }

    #[test]
    fn bold_short_line_is_a_minor_heading() {
        let block = promote(para_block("Subsection", 12.0, true, 0), 12.0);
        match block.kind {
            BlockKind::Heading(h) => assert_eq!(h.level, 6),
            _ => panic!("expected heading for bold short line"),
        }
    }

    #[test]
    fn long_large_paragraph_is_not_a_heading() {
        // Large font but 3 lines (2 breaks) → too long to be a heading.
        let block = promote(para_block("big but long", 20.0, false, 2), 12.0);
        assert!(matches!(block.kind, BlockKind::Paragraph(_)));
    }

    #[test]
    fn size_buckets_map_to_levels() {
        assert_eq!(level_for(24.0, 12.0), 1); // 2.0×
        assert_eq!(level_for(21.0, 12.0), 2); // 1.75×
        assert_eq!(level_for(18.0, 12.0), 3); // 1.5×
        assert_eq!(level_for(15.0, 12.0), 4); // 1.25×
        assert_eq!(level_for(14.0, 12.0), 5); // ~1.17×
    }
}
