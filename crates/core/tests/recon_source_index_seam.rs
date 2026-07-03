//! Cross-walk seam test: every `source_index` a reconstructed block run carries
//! MUST resolve, in `page_text_elements`'s index space, to the very text it was
//! built from. This is the editor contract (`pageBlocks` ↔ `replaceText` /
//! `removeElement`): the two walks live in different functions
//! (`recon::runs_from_elements` vs `Document::page_text_elements`) and any
//! future divergence in their counting — blank runs, nested runs, interposed
//! images/shapes — is caught here on a fixture that *forces* the historic
//! failure mode (non-text elements interleaved between text runs, which used to
//! shift `ContentElement::index` away from the text-run ordinal).

use std::collections::BTreeMap;

use gigapdf_core::model::{Block, BlockKind, Inline};
use gigapdf_core::{Document, ElementKind};

/// A 1×1 RGB PNG (red) — the smallest raster the engine embeds as an XObject.
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
    0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
    0x77, 0x53, 0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8,
    0xcf, 0xc0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0xc9, 0xfe, 0x92, 0xef, 0x00, 0x00, 0x00,
    0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

fn fixture(name: &str) -> Vec<u8> {
    let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("../../fixtures");
    path.push(name);
    std::fs::read(&path).unwrap_or_else(|_| panic!("missing fixture {}", path.display()))
}

/// Collect one `(source ordinal, run text)` pair per index a run carries,
/// descending into links.
fn collect_sourced(runs: &[Inline], out: &mut Vec<(usize, String)>) {
    for inline in runs {
        match inline {
            Inline::Run(r) => {
                if let Some(indices) = &r.source_index {
                    for &idx in indices {
                        out.push((idx, r.text.clone()));
                    }
                }
            }
            Inline::Link { children, .. } => collect_sourced(children, out),
            _ => {}
        }
    }
}

/// Walk a block tree (paragraphs, headings, lists, tables) collecting every
/// sourced run.
fn walk(block: &Block, out: &mut Vec<(usize, String)>) {
    match &block.kind {
        BlockKind::Paragraph(p) => collect_sourced(&p.runs, out),
        BlockKind::Heading(h) => collect_sourced(&h.para.runs, out),
        BlockKind::List(l) => {
            for item in &l.items {
                for b in &item.blocks {
                    walk(b, out);
                }
            }
        }
        BlockKind::Table(t) => {
            for row in &t.rows {
                for cell in &row.cells {
                    for b in &cell.blocks {
                        walk(b, out);
                    }
                }
            }
        }
        _ => {}
    }
}

#[test]
fn block_source_indices_resolve_page_text_elements() {
    let mut doc = Document::open(&fixture("simple.pdf")).expect("open fixture");
    let black = [0.0, 0.0, 0.0];

    // Interleave shapes and an image BETWEEN text runs. Each rectangle / image
    // is a non-text ContentElement, so the unified element index of every
    // later text run drifts away from its text-run ordinal — the exact
    // divergence the seam must be immune to.
    doc.add_text_standard(1, 72.0, 700.0, 12.0, "SEAM-AAA1", "Helvetica", black, 1.0, 0.0)
        .unwrap();
    doc.add_rectangle(
        1,
        72.0,
        650.0,
        120.0,
        20.0,
        Some(black),
        Some([0.9, 0.9, 0.9]),
        1.0,
        1.0,
    )
    .unwrap();
    doc.add_text_standard(1, 72.0, 600.0, 12.0, "SEAM-BBB2", "Helvetica", black, 1.0, 0.0)
        .unwrap();
    doc.add_image(1, TINY_PNG, 72.0, 545.0, 40.0, 40.0, 1.0)
        .unwrap();
    doc.add_text_standard(1, 72.0, 500.0, 12.0, "SEAM-CCC3", "Helvetica", black, 1.0, 0.0)
        .unwrap();
    doc.add_rectangle(1, 300.0, 452.0, 90.0, 16.0, Some(black), None, 1.0, 1.0)
        .unwrap();
    doc.add_text_standard(1, 72.0, 400.0, 12.0, "SEAM-DDD4", "Helvetica", black, 1.0, 0.0)
        .unwrap();

    // Guard against a vacuous test: on this page the unified element index of
    // at least one text run MUST differ from its text-run ordinal (non-text
    // elements are interposed). If this ever fails the fixture no longer
    // exercises the seam.
    let elements = doc.page_elements(1).expect("page elements");
    let mut ordinal = 0usize;
    let mut diverged = false;
    for e in &elements {
        if e.kind != ElementKind::Text {
            continue;
        }
        if e.index != ordinal {
            diverged = true;
        }
        ordinal += 1;
    }
    assert!(
        diverged,
        "fixture must interpose non-text elements so the two index spaces differ"
    );

    // The editing-side view: text-run ordinal → text.
    let by_ordinal: BTreeMap<usize, String> = doc
        .page_text_elements(1)
        .into_iter()
        .filter(|e| e.index != usize::MAX)
        .map(|e| (e.index, e.text))
        .collect();

    // The reconstruction-side view: every sourced block run.
    let blocks = doc.page_blocks(1);
    let mut pairs: Vec<(usize, String)> = Vec::new();
    for b in &blocks {
        walk(b, &mut pairs);
    }
    assert!(
        pairs.len() >= 4,
        "expected at least the four SEAM runs, got {}",
        pairs.len()
    );

    // THE seam invariant: every index a block run carries resolves to the very
    // text it was built from.
    for (idx, run_text) in &pairs {
        let source = by_ordinal.get(idx).unwrap_or_else(|| {
            panic!("source_index {idx} has no page_text_elements entry (run {run_text:?})")
        });
        let source = source.trim();
        assert!(
            !source.is_empty() && run_text.contains(source),
            "source_index {idx} resolves {source:?}, not part of run {run_text:?}"
        );
    }

    // And the four probes resolve EXACTLY (isolated single-run paragraphs):
    // under the pre-fix semantics (unified element index) each probe after an
    // interposed shape/image resolved a shifted, arbitrary run.
    for probe in ["SEAM-AAA1", "SEAM-BBB2", "SEAM-CCC3", "SEAM-DDD4"] {
        let (idx, _) = pairs
            .iter()
            .find(|(_, t)| t.contains(probe))
            .unwrap_or_else(|| panic!("no block run carries {probe}"));
        assert_eq!(
            by_ordinal.get(idx).map(|s| s.trim()),
            Some(probe),
            "{probe} must resolve its own text element"
        );
    }
}
