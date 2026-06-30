//! Running header / footer recovery for **reconstructed** documents.
//!
//! A PDF repeats its page furniture — a running title at the top, a page number
//! / rule at the bottom — physically on *every* page. The heuristic
//! reconstruction in [`reconstruct_page`](super::reconstruct_page) faithfully
//! turns each repetition into body blocks, so without this pass the furniture
//! leaks into the prose flow (and thus into every `toDocx()`/`toMarkdown()`/…
//! export) once per page.
//!
//! [`strip_running_furniture`] runs *after* the per-page blocks are assembled
//! into [`Section`](crate::model::Section)s. For each section it:
//!
//! 1. looks at the blocks sitting in the page's **top band** (header candidates)
//!    and **bottom band** (footer candidates);
//! 2. keeps only those whose normalized text/shape signature **repeats across a
//!    majority of the section's pages** — page numbers are folded to a common
//!    signature (`Page 1`/`Page 2` ⇒ `page #`) so they still cluster;
//! 3. **removes** the matching blocks from every page's body flow, and
//! 4. **populates** [`Section::header`](crate::model::Section::header) /
//!    [`footer`](crate::model::Section::footer) with **one** representative copy
//!    (de-duplicated), preserving the structure instead of just deleting it.
//!
//! Conservatism is the rule: a single-page section, or a section whose top/bottom
//! band carries no repeated content, is left untouched (`header`/`footer` stay
//! `None`, the body is unchanged) — real first-page content is never stripped.

use crate::model::{Block, BlockKind, Inline, Section};

/// Band depth as a fraction of page height scanned for furniture at each edge.
/// 12 % comfortably covers a standard one-line running head/foot plus its rule
/// without reaching into the body text region.
const BAND_FRACTION: f64 = 0.12;

/// A signature must recur on **more than this fraction** of the section's pages
/// to count as running furniture. A strict majority keeps one-off first/last
/// page content (a cover title, a colophon) out of the header/footer.
const REPEAT_FRACTION: f64 = 0.5;

/// Detect and lift the running header/footer of every section in `sections`.
///
/// Mutates each [`Section`] in place: furniture blocks are removed from the
/// pages' bodies and a single representative copy is stored on
/// [`Section::header`] / [`Section::footer`]. Sections without repeated band
/// content are untouched.
pub fn strip_running_furniture(sections: &mut [Section]) {
    for section in sections {
        strip_section(section);
    }
}

/// Lift the header and footer of a single section (see module docs).
fn strip_section(section: &mut Section) {
    // A running header/footer is, by definition, repeated across pages; a single
    // page cannot establish "running" furniture.
    if section.pages.len() < 2 {
        return;
    }
    let page_h = section.geometry.height;
    // Bail on a non-positive or non-finite page height (no meaningful band).
    if !(page_h.is_finite() && page_h > 0.0) {
        return;
    }
    let band_h = page_h * BAND_FRACTION;
    let page_count = section.pages.len();
    // Strict majority: strictly more than half the pages must carry a signature.
    let min_repeat = (page_count as f64 * REPEAT_FRACTION).floor() as usize + 1;

    let header = lift_band(&mut section.pages, Edge::Top, band_h, page_h, min_repeat);
    let footer = lift_band(&mut section.pages, Edge::Bottom, band_h, page_h, min_repeat);

    if header.is_some() {
        section.header = header;
    }
    if footer.is_some() {
        section.footer = footer;
    }
}

/// Which page edge a band hugs (model **top-down** frames: `y = 0` is the page
/// top, `y = page_h` the bottom).
#[derive(Clone, Copy, PartialEq)]
enum Edge {
    Top,
    Bottom,
}

/// Remove the running furniture from `pages` at `edge` and return one
/// representative copy, or `None` when the band carries no repeated content.
///
/// `band_h` is the band depth at the edge, `page_h` the page height (top-down
/// frame space), and `min_repeat` the strict-majority page count a signature
/// must reach to qualify as furniture.
fn lift_band(
    pages: &mut [crate::model::Page],
    edge: Edge,
    band_h: f64,
    page_h: f64,
    min_repeat: usize,
) -> Option<Vec<Block>> {
    // 1. Per page, the signatures present in the band (a set: a signature counts
    //    once per page so a body paragraph that merely repeats a word can't
    //    inflate the tally).
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for page in pages.iter() {
        let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for block in &page.blocks {
            if !in_band(block, edge, band_h, page_h) {
                continue;
            }
            if let Some(sig) = block_signature(block) {
                seen.insert(sig);
            }
        }
        for sig in seen {
            *counts.entry(sig).or_default() += 1;
        }
    }

    // 2. Signatures that recur across a strict majority of pages = furniture.
    let furniture: std::collections::BTreeSet<String> = counts
        .into_iter()
        .filter(|&(_, n)| n >= min_repeat)
        .map(|(sig, _)| sig)
        .collect();
    if furniture.is_empty() {
        return None;
    }

    // 3. Capture one representative copy (the first page that carries the
    //    furniture, in document reading order) before stripping the rest.
    let mut representative: Option<Vec<Block>> = None;
    for page in pages.iter() {
        let found: Vec<Block> = page
            .blocks
            .iter()
            .filter(|block| {
                in_band(block, edge, band_h, page_h)
                    && block_signature(block).is_some_and(|sig| furniture.contains(&sig))
            })
            .cloned()
            .collect();
        if !found.is_empty() {
            representative = Some(found);
            break;
        }
    }

    // 4. Strip the furniture from every page's body flow.
    for page in pages.iter_mut() {
        page.blocks.retain(|block| {
            !(in_band(block, edge, band_h, page_h)
                && block_signature(block).is_some_and(|sig| furniture.contains(&sig)))
        });
    }

    representative
}

/// Whether `block` sits wholly within the `edge` band of a page of height
/// `page_h` (model **top-down** frame space). A frameless block (none in
/// practice post-reconstruction) is never furniture.
fn in_band(block: &Block, edge: Edge, band_h: f64, page_h: f64) -> bool {
    let Some(frame) = block.frame else {
        return false;
    };
    let top = frame.y;
    let bottom = frame.y + frame.h;
    // The inner edge (toward the body) is a tight hairline on purpose: widening it
    // pulls genuine body content (a page title sitting just inside the margin) into
    // the band and wrongly strips it as furniture. The band-boundary *tolerance*
    // the recon polish calls for (gap #75, sub-item 11) lives where it is safe —
    // the table rule bands, where `segment_rule_bands` already merges near-miss
    // rule intervals within an adaptive `split_gap` (`recon::tables`).
    match edge {
        // Header: the whole block lives in the top band.
        Edge::Top => top >= -0.5 && bottom <= band_h + 0.5,
        // Footer: the whole block lives in the bottom band.
        Edge::Bottom => top >= page_h - band_h - 0.5 && bottom <= page_h + 0.5,
    }
}

/// A normalized signature that clusters the *same* piece of furniture across
/// pages while folding the parts that legitimately vary (the page number).
///
/// * Text-bearing blocks → lowercased, whitespace-collapsed text with every run
///   of ASCII digits replaced by `#`, so `Page 1` / `Page 2` and a bare `1` / `2`
///   share a signature. An empty result (no letters/digits) ⇒ `None` (not
///   furniture).
/// * A [`Shape`](crate::model::Shape) (e.g. the rule under a running head) →
///   a kind-tagged, position-bucketed signature so the same rule clusters but a
///   stray graphic does not collapse onto unrelated text.
/// * An **image-only** block — a running logo, with no prose — → an image
///   signature keyed by its resource hash and bucketed vertical centre (gap #75,
///   sub-item 10), so a logo repeated on every page is lifted as furniture
///   instead of leaking into the body of every page.
/// * Other non-text blocks ⇒ `None` (never treated as furniture).
fn block_signature(block: &Block) -> Option<String> {
    match &block.kind {
        BlockKind::Shape(_) => {
            // Bucket the shape's vertical centre to ~2 pt so the same running
            // rule clusters across pages despite sub-pixel jitter.
            let frame = block.frame?;
            let cy = ((frame.y + frame.h / 2.0) / 2.0).round() as i64;
            Some(format!("shape@{cy}"))
        }
        BlockKind::Image(img) => image_signature(block, &[img.resource]),
        _ => {
            let text = normalize_text(&block_text(block));
            if !text.is_empty() {
                return Some(text);
            }
            // No prose: a header/footer laid out as a logo (an image inside a
            // paragraph/text box) still clusters by its image resource(s).
            let resources = block_image_resources(block);
            if resources.is_empty() {
                None
            } else {
                image_signature(block, &resources)
            }
        }
    }
}

/// An image furniture signature: the (sorted) image resource hashes plus the
/// block's vertical centre bucketed to ~2 pt (matching the shape signature), so
/// the same logo at the same height clusters across pages while a one-off graphic
/// does not collapse onto it. Returns `None` for a frameless block.
fn image_signature(block: &Block, resources: &[u64]) -> Option<String> {
    let frame = block.frame?;
    let cy = ((frame.y + frame.h / 2.0) / 2.0).round() as i64;
    let mut rs = resources.to_vec();
    rs.sort_unstable();
    rs.dedup();
    let list: Vec<String> = rs.iter().map(|r| r.to_string()).collect();
    Some(format!("image:{}@{cy}", list.join(",")))
}

/// Collect the image resource hashes a block carries (recursing through inline
/// runs and nested block containers), for the image-only furniture signature.
fn block_image_resources(block: &Block) -> Vec<u64> {
    let mut out = Vec::new();
    collect_block_images(block, &mut out);
    out
}

/// Append `block`'s image resource hashes to `out` (recursing into nested
/// containers and inline content).
fn collect_block_images(block: &Block, out: &mut Vec<u64>) {
    match &block.kind {
        BlockKind::Image(img) => out.push(img.resource),
        BlockKind::Paragraph(p) => collect_inline_images(&p.runs, out),
        BlockKind::Heading(h) => collect_inline_images(&h.para.runs, out),
        BlockKind::List(list) => {
            for item in &list.items {
                for b in &item.blocks {
                    collect_block_images(b, out);
                }
            }
        }
        BlockKind::Table(table) => {
            for row in &table.rows {
                for cell in &row.cells {
                    for b in &cell.blocks {
                        collect_block_images(b, out);
                    }
                }
            }
        }
        BlockKind::TextBox(tb) => {
            for b in &tb.blocks {
                collect_block_images(b, out);
            }
        }
        BlockKind::Blockquote(bq) => {
            for b in &bq.blocks {
                collect_block_images(b, out);
            }
        }
        _ => {}
    }
}

/// Append the image resource hashes inside a run of [`Inline`]s to `out`.
fn collect_inline_images(runs: &[Inline], out: &mut Vec<u64>) {
    for inline in runs {
        match inline {
            Inline::Image(img) => out.push(img.resource),
            Inline::Link { children, .. } => collect_inline_images(children, out),
            _ => {}
        }
    }
}

/// The most alphabetic content a block may carry and still have its digits
/// folded as a page number. A bare `1`, `Page 1`, `p. 1`, `1 of 9`, `- 1 -` all
/// stay under this; a numbered running *title* like `Chapter 1 Title` (≈12
/// letters) does not — so its number is **kept**, and a sequence of numbered
/// chapter headings does not collapse to one false-positive signature.
const PAGE_NUMBER_MAX_LETTERS: usize = 6;

/// The longest digit run that may be folded as a page number (gap #75, sub-item
/// 12). Page numbers are short; a longer run is a year-range, an account/ISBN, a
/// reference id — folding *those* would let two distinct numeric strings collapse
/// onto one `#` signature and be wrongly clustered as furniture, so they are kept
/// verbatim.
const PAGE_NUMBER_MAX_DIGITS: usize = 4;

/// Collapse `raw` to a signature that clusters the *same* furniture across pages:
/// ASCII-lowercased with whitespace collapsed to single spaces, and — *only when
/// the block is dominated by the number* (≤ [`PAGE_NUMBER_MAX_LETTERS`] letters,
/// the page-number case) — every **short** digit run (≤ [`PAGE_NUMBER_MAX_DIGITS`])
/// folded to `#`. A running title that merely repeats verbatim already matches
/// without folding; the letter guard keeps it from swallowing distinct numbered
/// headings, and the digit-length guard keeps a long number (year-range, id) from
/// folding into a page-number-shaped signature.
fn normalize_text(raw: &str) -> String {
    let fold_digits = raw.chars().filter(|c| c.is_alphabetic()).count() <= PAGE_NUMBER_MAX_LETTERS;
    let chars: Vec<char> = raw.chars().collect();
    let mut out = String::with_capacity(raw.len());
    let mut last_was_space = true; // trims leading whitespace
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch.is_ascii_digit() {
            // Consume the whole digit run, then decide whether to fold it.
            let start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            let run_len = i - start;
            if fold_digits && run_len <= PAGE_NUMBER_MAX_DIGITS {
                out.push('#');
            } else {
                // A long run (or a digit-rich title) keeps its digits verbatim.
                out.extend(chars[start..i].iter());
            }
            last_was_space = false;
            continue;
        }
        if ch.is_whitespace() {
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
        } else {
            out.extend(ch.to_lowercase());
            last_was_space = false;
        }
        i += 1;
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

/// The flattened text of any block kind that carries inline runs (paragraph,
/// heading, list, table, text box, blockquote …). Recurses through nested block
/// containers so a header laid out as, say, a one-row table still yields its
/// text. Non-text kinds contribute nothing.
fn block_text(block: &Block) -> String {
    let mut out = String::new();
    collect_block_text(block, &mut out);
    out
}

/// Append `block`'s flattened text to `out` (recursing into nested containers).
fn collect_block_text(block: &Block, out: &mut String) {
    match &block.kind {
        BlockKind::Paragraph(p) => append_runs(&p.runs, out),
        BlockKind::Heading(h) => append_runs(&h.para.runs, out),
        BlockKind::List(list) => {
            for item in &list.items {
                for b in &item.blocks {
                    collect_block_text(b, out);
                }
            }
        }
        BlockKind::Table(table) => {
            for row in &table.rows {
                for cell in &row.cells {
                    for b in &cell.blocks {
                        collect_block_text(b, out);
                    }
                }
            }
        }
        BlockKind::TextBox(tb) => {
            for b in &tb.blocks {
                collect_block_text(b, out);
            }
        }
        BlockKind::Blockquote(bq) => {
            for b in &bq.blocks {
                collect_block_text(b, out);
            }
        }
        BlockKind::CodeBlock(cb) => out.push_str(&cb.code),
        // Image / Shape / Sheet / Slide / HorizontalRule carry no prose.
        _ => {}
    }
}

/// Append the text of a run of [`Inline`]s to `out` (mirrors the module-private
/// `paragraph_text`, kept local so this stage owns its recursion).
fn append_runs(runs: &[Inline], out: &mut String) {
    for inline in runs {
        match inline {
            Inline::Run(run) => out.push_str(&run.text),
            Inline::LineBreak => out.push(' '),
            Inline::Link { children, .. } => append_runs(children, out),
            Inline::Image(_) => {}
            Inline::CommentRef { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        geom::PageGeometry, BlockId, BlockKind, Heading, Inline, InlineRun, Page, Paragraph, Rect,
        Section, Shape,
    };

    /// A paragraph block at a top-down `frame`.
    fn para_block(id: u64, text: &str, x: f64, y: f64, w: f64, h: f64) -> Block {
        Block {
            id: BlockId(id),
            frame: Some(Rect::new(x, y, w, h)),
            rotation: Default::default(),
            kind: BlockKind::Paragraph(Paragraph {
                runs: vec![Inline::Run(InlineRun {
                    text: text.to_string(),
                    ..Default::default()
                })],
                ..Default::default()
            }),
        }
    }

    /// A heading block at a top-down `frame`.
    fn heading_block(id: u64, text: &str, x: f64, y: f64, w: f64, h: f64) -> Block {
        Block {
            id: BlockId(id),
            frame: Some(Rect::new(x, y, w, h)),
            rotation: Default::default(),
            kind: BlockKind::Heading(Heading {
                level: 1,
                para: Paragraph {
                    runs: vec![Inline::Run(InlineRun {
                        text: text.to_string(),
                        ..Default::default()
                    })],
                    ..Default::default()
                },
            }),
        }
    }

    /// A4-ish geometry (height drives the band math).
    fn geom() -> PageGeometry {
        PageGeometry {
            width: 595.0,
            height: 842.0,
            ..Default::default()
        }
    }

    fn block_texts(blocks: &[Block]) -> Vec<String> {
        blocks.iter().map(block_text).collect()
    }

    /// A multi-page section with a running title header + a "Page N" footer →
    /// the furniture is removed from every body and lifted, once, onto the
    /// section header/footer.
    #[test]
    fn strips_running_header_and_page_number_footer() {
        // 4 pages, each: header "Annual Report" near the top, a body paragraph
        // mid-page, and a "Page N" footer near the bottom.
        let mut pages = Vec::new();
        for n in 1..=4u64 {
            let blocks = vec![
                para_block(n * 10, "Annual Report", 60.0, 20.0, 200.0, 14.0),
                para_block(
                    n * 10 + 1,
                    &format!("Body paragraph on page {n} with unique prose."),
                    60.0,
                    400.0,
                    470.0,
                    14.0,
                ),
                para_block(n * 10 + 2, &format!("Page {n}"), 280.0, 815.0, 40.0, 12.0),
            ];
            pages.push(Page {
                blocks,
                absolute: false,
            });
        }
        let mut section = Section {
            geometry: geom(),
            header: None,
            footer: None,
            pages,
        };

        strip_running_furniture(std::slice::from_mut(&mut section));

        // Header lifted exactly once, with the running title.
        let header = section.header.expect("header detected");
        assert_eq!(block_texts(&header), vec!["Annual Report"]);

        // Footer lifted exactly once, with a representative "Page N".
        let footer = section.footer.expect("footer detected");
        assert_eq!(footer.len(), 1);
        assert!(
            block_text(&footer[0]).starts_with("Page "),
            "footer is the page-number furniture, got {:?}",
            block_text(&footer[0])
        );

        // Every page body now holds ONLY its unique prose — no furniture leak.
        for (i, page) in section.pages.iter().enumerate() {
            assert_eq!(page.blocks.len(), 1, "page {i} keeps only its body block");
            let txt = block_text(&page.blocks[0]);
            assert!(
                txt.contains("Body paragraph"),
                "page {i} body preserved, got {txt:?}"
            );
            assert!(
                !txt.contains("Annual Report") && !txt.starts_with("Page "),
                "page {i} furniture stripped"
            );
        }
    }

    /// A bottom rule that repeats on every page is recognised as footer
    /// furniture and lifted alongside / instead of text.
    #[test]
    fn strips_repeated_footer_rule_shape() {
        let rule = |id: u64| Block {
            id: BlockId(id),
            frame: Some(Rect::new(60.0, 810.0, 470.0, 1.0)),
            rotation: Default::default(),
            kind: BlockKind::Shape(Shape {
                stroke: Some([0.0, 0.0, 0.0]),
                stroke_width: 1.0,
                ..Default::default()
            }),
        };
        let mut pages = Vec::new();
        for n in 1..=3u64 {
            pages.push(Page {
                blocks: vec![
                    para_block(
                        n * 10,
                        &format!("Distinct body text number {n}."),
                        60.0,
                        400.0,
                        470.0,
                        14.0,
                    ),
                    rule(n * 10 + 1),
                ],
                absolute: false,
            });
        }
        let mut section = Section {
            geometry: geom(),
            header: None,
            footer: None,
            pages,
        };

        strip_running_furniture(std::slice::from_mut(&mut section));

        let footer = section.footer.expect("footer rule detected");
        assert_eq!(footer.len(), 1);
        assert!(matches!(footer[0].kind, BlockKind::Shape(_)));
        // Bodies keep only their prose; the rule is gone from each page.
        for page in &section.pages {
            assert_eq!(page.blocks.len(), 1);
            assert!(matches!(page.blocks[0].kind, BlockKind::Paragraph(_)));
        }
    }

    /// A single-page document has no "running" furniture → nothing is touched,
    /// header/footer stay `None`, and the body (including a top-of-page title)
    /// is preserved. Guards against false positives stripping real content.
    #[test]
    fn single_page_is_untouched() {
        let section_blocks = vec![
            heading_block(1, "Real First-Page Title", 60.0, 20.0, 300.0, 24.0),
            para_block(2, "The only body paragraph.", 60.0, 400.0, 470.0, 14.0),
            para_block(3, "Page 1", 280.0, 815.0, 40.0, 12.0),
        ];
        let mut section = Section {
            geometry: geom(),
            header: None,
            footer: None,
            pages: vec![Page {
                blocks: section_blocks.clone(),
                absolute: false,
            }],
        };

        strip_running_furniture(std::slice::from_mut(&mut section));

        assert!(section.header.is_none(), "no header on a single page");
        assert!(section.footer.is_none(), "no footer on a single page");
        assert_eq!(
            section.pages[0].blocks, section_blocks,
            "single-page body is unchanged"
        );
    }

    /// A multi-page document whose top/bottom bands carry *no* repeated content
    /// (every page's heading is distinct) is left untouched — distinct
    /// per-page band content is body, not furniture.
    #[test]
    fn no_repeated_furniture_leaves_body_unchanged() {
        let mut pages = Vec::new();
        for n in 1..=3u64 {
            pages.push(Page {
                blocks: vec![
                    // Distinct top-of-page heading each page (a chapter title),
                    // not running furniture.
                    heading_block(
                        n * 10,
                        &format!("Chapter {n} Title"),
                        60.0,
                        20.0,
                        300.0,
                        24.0,
                    ),
                    para_block(
                        n * 10 + 1,
                        &format!("Body of chapter {n}."),
                        60.0,
                        400.0,
                        470.0,
                        14.0,
                    ),
                ],
                absolute: false,
            });
        }
        let original: Vec<_> = pages.iter().map(|p| p.blocks.clone()).collect();
        let mut section = Section {
            geometry: geom(),
            header: None,
            footer: None,
            pages,
        };

        strip_running_furniture(std::slice::from_mut(&mut section));

        assert!(section.header.is_none());
        assert!(section.footer.is_none());
        for (i, page) in section.pages.iter().enumerate() {
            assert_eq!(page.blocks, original[i], "page {i} body unchanged");
        }
    }

    /// Page numbers fold to a shared signature: `Page 1`/`Page 2`/`Page 3` count
    /// as the *same* furniture even though the text differs per page.
    #[test]
    fn page_numbers_normalize_to_one_signature() {
        assert_eq!(normalize_text("Page 1"), normalize_text("Page 23"));
        assert_eq!(normalize_text("Page 1"), "page #");
        assert_eq!(normalize_text("  3 "), "#");
        // Distinct prose does NOT collapse.
        assert_ne!(normalize_text("Chapter 1 Title"), normalize_text("Summary"));
    }

    #[test]
    fn digit_fold_guard_keeps_long_numbers_verbatim() {
        // Short page-number runs (≤ 4 digits) fold and cluster…
        assert_eq!(normalize_text("Page 4567"), "page #");
        assert_eq!(normalize_text("Page 4567"), normalize_text("Page 12"));
        // …but a LONG digit run is kept verbatim (a year-range, an id, an ISBN),
        // so two distinct long-number footers do NOT collapse to one signature
        // (gap #75 #12).
        assert_eq!(normalize_text("Ref 12345678"), "ref 12345678");
        assert_ne!(
            normalize_text("Ref 12345678"),
            normalize_text("Ref 87654321")
        );
    }

    #[test]
    fn strips_running_logo_image() {
        // A logo image repeated at the top of every page is running furniture even
        // though it carries no text (gap #75 #10): it is lifted to the section
        // header and removed from every body.
        let logo = |id: u64| Block {
            id: BlockId(id),
            frame: Some(Rect::new(60.0, 20.0, 120.0, 40.0)),
            rotation: Default::default(),
            kind: BlockKind::Image(crate::model::ImageRef {
                resource: 0xABCD,
                alt: None,
            }),
        };
        let mut pages = Vec::new();
        for n in 1..=3u64 {
            pages.push(Page {
                blocks: vec![
                    logo(n * 10),
                    para_block(
                        n * 10 + 1,
                        &format!("Unique body prose on page {n}."),
                        60.0,
                        400.0,
                        470.0,
                        14.0,
                    ),
                ],
                absolute: false,
            });
        }
        let mut section = Section {
            geometry: geom(),
            header: None,
            footer: None,
            pages,
        };
        strip_running_furniture(std::slice::from_mut(&mut section));

        let header = section.header.expect("logo header detected");
        assert_eq!(header.len(), 1, "one representative logo lifted");
        assert!(
            matches!(header[0].kind, BlockKind::Image(_)),
            "the lifted header is the logo image"
        );
        for (i, page) in section.pages.iter().enumerate() {
            assert_eq!(page.blocks.len(), 1, "page {i} keeps only its body block");
            assert!(block_text(&page.blocks[0]).contains("Unique body prose"));
        }
    }
}
