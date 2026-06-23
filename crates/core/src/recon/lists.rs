//! Stage 5 — **list detection**. A line whose first run is a bullet
//! (`• ‣ ◦ - – *`) or an ordinal (`1.` `1)` `a.` `IV.` …) followed by a hanging
//! indent is a [`ListItem`]; consecutive items sharing an indent collapse into a
//! single [`List`], and the marker glyph is stripped out into [`List::marker`].
//!
//! The module splits a prose group into alternating [`Segment::List`] /
//! [`Segment::Prose`] spans (so `reconstruct_page` lowers each appropriately),
//! then builds the [`List`] block from the item lines.
//!
//! ## Nesting & continuation
//!
//! Two refinements recover structure a flat "one marker = one item" reading
//! misses (epic #74 reconnaissance, gap #8):
//!
//! * **Nesting level** — each item's [`ListItem::level`] is derived from its
//!   marker's hanging-indent X *relative to the list's base indent*. Marker
//!   starts are bucketed into levels `0, 1, 2…` with a tolerance of one body
//!   size, so a sub-bullet indented under its parent nests a level deeper rather
//!   than flattening to the same level.
//! * **Continuation lines** — a markerless line indented to the *text start* of
//!   the item above it (where the wrapped text would naturally align, not the
//!   marker column) is a **continuation** of that item, not a new list entry nor
//!   a prose break. It is sewn back into the preceding item.

use super::lines::ReconLine;
use super::{run_char_style, IdGen};
use crate::model::{
    geom::Rotation, Block, BlockKind, Inline, InlineRun, List, ListItem, ListMarker, Paragraph,
    ParagraphStyle, Rect,
};

/// One span of a prose group: a run of list items, or ordinary prose.
#[derive(Debug)]
pub enum Segment<'a> {
    List(Vec<&'a ReconLine>),
    Prose(Vec<&'a ReconLine>),
}

/// The marker classification of a line's leading token.
#[derive(Debug, Clone, PartialEq)]
enum Marker {
    Bullet(char),
    Decimal,
    LowerAlpha,
    UpperAlpha,
    LowerRoman,
    UpperRoman,
}

impl Marker {
    fn to_list_marker(&self) -> ListMarker {
        match self {
            Marker::Bullet(c) => ListMarker::Bullet(*c),
            Marker::Decimal => ListMarker::Decimal,
            Marker::LowerAlpha => ListMarker::LowerAlpha,
            Marker::UpperAlpha => ListMarker::UpperAlpha,
            Marker::LowerRoman => ListMarker::LowerRoman,
            Marker::UpperRoman => ListMarker::UpperRoman,
        }
    }
    fn ordered(&self) -> bool {
        !matches!(self, Marker::Bullet(_))
    }
}

/// The X (PDF user space) at which a list item's **text** begins — i.e. where a
/// wrapped continuation of that item would naturally align. The marker is the
/// leading glyph/token; the text starts just after it.
///
/// Two shapes occur in the wild: the marker is its *own* run (`•` then `Apples`,
/// each a separate operator) — then the text-start is the first non-marker run's
/// left edge; or the marker shares one run with the text (`• Apples` as a single
/// `Tj`) — then we advance the line's left by the marker token's estimated
/// width. The estimate uses the run's mean glyph advance (`w / chars`), which is
/// exact enough for the bucket tolerance.
fn item_text_left(line: &ReconLine) -> f64 {
    let Some(first) = line.runs.first() else {
        return line.x;
    };
    let first_trim = first.text.trim_start();
    // Marker is a standalone run: the text begins at the next run with content.
    if detect_marker(first_trim).is_some() && is_marker_only_run(first_trim) {
        if let Some(next) = line
            .runs
            .iter()
            .find(|r| !r.text.trim().is_empty() && !is_marker_only_run(r.text.trim_start()))
        {
            return next.x;
        }
    }
    // Marker shares the run with the text (or there is a single run): advance the
    // line's left by the marker token's estimated width.
    let token_chars = marker_token_len_chars(first_trim);
    if token_chars == 0 {
        return line.x;
    }
    let glyphs = first.text.chars().filter(|c| !c.is_whitespace()).count();
    let advance = if glyphs > 0 && first.w > 0.0 {
        first.w / glyphs as f64
    } else {
        first.h * 0.5
    };
    // Marker token + the single separating space that strip_marker would drop.
    line.x + advance * (token_chars as f64 + 1.0)
}

/// Whether a run's (already left-trimmed) text is *only* a marker token — a lone
/// bullet glyph, or an ordinal token like `1.` / `a)` with nothing after it.
/// Distinguishes a standalone-marker run from a marker-plus-text run.
fn is_marker_only_run(trimmed: &str) -> bool {
    if detect_marker(trimmed).is_none() {
        return false;
    }
    // Everything after the leading whitespace-delimited token must be blank.
    trimmed
        .split_once(char::is_whitespace)
        .map(|(_, rest)| rest.trim().is_empty())
        .unwrap_or(true)
}

/// Character count of the leading marker token of an (already left-trimmed)
/// marker line — `1` for a bullet glyph, the token length (incl. the `.`/`)`)
/// for an ordinal. `0` when there is no marker.
fn marker_token_len_chars(trimmed: &str) -> usize {
    match detect_marker(trimmed) {
        Some(Marker::Bullet(_)) => 1,
        Some(_) => trimmed.chars().take_while(|c| !c.is_whitespace()).count(),
        None => 0,
    }
}

/// Split a reading-order group into list / prose segments.
///
/// A list run starts at a marker line and absorbs every following marker line.
/// A markerless line indented at/past the **text start** of the last marker line
/// in the run is a wrapped **continuation** and stays inside the list run (it is
/// folded into its item by [`build_list`]); any other markerless line breaks the
/// run into prose.
pub fn split_lists<'a>(lines: &[&'a ReconLine]) -> Vec<Segment<'a>> {
    let mut segments: Vec<Segment> = Vec::new();
    let mut run: Vec<&ReconLine> = Vec::new();
    let mut run_is_list = false;
    // Text-start X of the most recent marker line in the current list run, plus
    // a tolerance derived from that line's height — the continuation threshold.
    let mut item_text_x = f64::INFINITY;
    let mut item_tol = 0.0_f64;

    for &line in lines {
        let is_item = detect_marker(&line.text()).is_some();
        if run.is_empty() {
            run.push(line);
            run_is_list = is_item;
            if is_item {
                item_text_x = item_text_left(line);
                item_tol = line.h.max(1.0) * 0.5;
            }
            continue;
        }
        if is_item {
            // A marker line continues a list run, or starts one out of prose.
            if !run_is_list {
                segments.push(finish(std::mem::take(&mut run), run_is_list));
                run_is_list = true;
            }
            run.push(line);
            item_text_x = item_text_left(line);
            item_tol = line.h.max(1.0) * 0.5;
        } else if run_is_list && line.x + item_tol >= item_text_x {
            // Markerless, but indented to the item's wrapped-text column → a
            // continuation line of the current item. Keep it in the list run;
            // the marker context (`item_text_x`) is unchanged.
            run.push(line);
        } else {
            // Plain prose: break the run (whatever it was) and start anew.
            if run_is_list {
                segments.push(finish(std::mem::take(&mut run), run_is_list));
                run_is_list = false;
            }
            run.push(line);
        }
    }
    if !run.is_empty() {
        segments.push(finish(run, run_is_list));
    }
    segments
}

fn finish<'a>(lines: Vec<&'a ReconLine>, is_list: bool) -> Segment<'a> {
    if is_list {
        Segment::List(lines)
    } else {
        Segment::Prose(lines)
    }
}

/// Build a [`List`] block from item + continuation lines (in reading order).
///
/// Each **marker** line opens a new [`ListItem`]; its marker is stripped and its
/// nesting [`ListItem::level`] is derived from the marker's hanging-indent X
/// (see [`indent_levels`]). A **continuation** line — markerless, indented to the
/// item's wrapped-text column (carried in by [`split_lists`]) — appends to the
/// item above it as a [`Inline::LineBreak`] plus its runs, so a wrapped bullet
/// reads as one item rather than splintering. The list's `ordered`/`marker` come
/// from the first marker line. `body` is the body font size — the bucket
/// tolerance for the level derivation.
pub fn build_list(
    items: &[&ReconLine],
    body: f64,
    ids: &mut IdGen,
    to_frame: impl Fn(f64, f64, f64, f64) -> Rect,
) -> Option<Block> {
    if items.is_empty() {
        return None;
    }
    // The list's marker is taken from its first item.
    let first_idx = items
        .iter()
        .position(|l| detect_marker(&l.text()).is_some())?;
    let first_marker = detect_marker(&items[first_idx].text())?;
    let mut list = List {
        ordered: first_marker.ordered(),
        marker: first_marker.to_list_marker(),
        items: Vec::new(),
    };

    // Bucket each marker line's start-X into a nesting level (0 = shallowest).
    // Continuation lines get `None` (they inherit their parent item's level).
    let levels = indent_levels(items, body);

    let mut x0 = f64::INFINITY;
    let mut y0 = f64::INFINITY;
    let mut x1 = f64::NEG_INFINITY;
    let mut y1 = f64::NEG_INFINITY;

    for (line, level) in items.iter().zip(levels.iter()) {
        x0 = x0.min(line.x);
        y0 = y0.min(line.y);
        x1 = x1.max(line.x + line.w);
        y1 = y1.max(line.y + line.h);

        match level {
            // Markerless continuation: fold its text into the current item.
            None => {
                if let Some(last) = list.items.last_mut() {
                    if let Some(Block {
                        kind: BlockKind::Paragraph(p),
                        ..
                    }) = last.blocks.last_mut()
                    {
                        // Join the wrapped text onto the item with a soft break.
                        p.runs.push(Inline::LineBreak);
                        p.runs.extend(line_runs(line, &line.text()));
                        continue;
                    }
                }
                // No item yet to attach to (defensive): treat as its own item.
                let runs = line_runs(line, &line.text());
                list.items.push(ListItem {
                    blocks: vec![paragraph_block(runs, ids)],
                    level: 0,
                });
            }
            // Marker line: open a new item at its nesting level.
            Some(level) => {
                let stripped = strip_marker(&line.text());
                let mut runs = line_runs(line, &stripped);
                if runs.is_empty() && !stripped.is_empty() {
                    // Marker and text were one run: emit the stripped text.
                    let style = line.runs.first().map(run_char_style).unwrap_or_default();
                    runs.push(Inline::Run(InlineRun {
                        text: stripped.clone(),
                        style,
                        source_index: None,
                    }));
                }
                list.items.push(ListItem {
                    blocks: vec![paragraph_block(runs, ids)],
                    level: *level,
                });
            }
        }
    }

    Some(Block {
        id: ids.mint(),
        frame: Some(to_frame(x0, y0, x1 - x0, y1 - y0)),
        rotation: Rotation::D0,
        kind: BlockKind::List(list),
    })
}

/// Wrap a run of [`Inline`]s into a default-styled paragraph [`Block`].
fn paragraph_block(runs: Vec<Inline>, ids: &mut IdGen) -> Block {
    Block {
        id: ids.mint(),
        frame: None,
        rotation: Rotation::D0,
        kind: BlockKind::Paragraph(Paragraph {
            style: ParagraphStyle::default(),
            style_ref: None,
            runs,
        }),
    }
}

/// Emit the inline runs of a line that overlap `text` (the line's text after any
/// marker stripping), dropping a leading marker glyph that was stripped out. Used
/// for both marker items (`text` = stripped) and continuations (`text` = full).
fn line_runs(line: &ReconLine, text: &str) -> Vec<Inline> {
    let mut runs: Vec<Inline> = Vec::new();
    let mut remaining = text;
    // Right edge / height of the previously emitted run, plus whether its raw text
    // ended with whitespace — to synthesize an inter-word space **gap-aware**: a
    // dense form splits one word across embedded fonts (joining every run with a
    // space would shred words), so a space is due only on a real inter-word gap
    // or when a run carried its own leading/trailing whitespace (the per-run trim
    // dropped it).
    let mut prev: Option<(f64, f64, bool)> = None;
    for r in &line.runs {
        if remaining.is_empty() {
            break;
        }
        let t = r.text.trim();
        if t.is_empty() {
            continue;
        }
        let take = item_run_text(t, &mut remaining);
        if take.is_empty() {
            continue;
        }
        if let Some((prev_right, prev_h, prev_trailing_ws)) = prev {
            if prev_trailing_ws
                || r.text.starts_with(char::is_whitespace)
                || !super::runs_join(prev_right, r.x, r.h.max(prev_h))
            {
                runs.push(Inline::Run(InlineRun {
                    text: " ".to_string(),
                    style: run_char_style(r),
                    source_index: None,
                }));
            }
        }
        runs.push(Inline::Run(InlineRun {
            text: take,
            style: run_char_style(r),
            source_index: r.source_index,
        }));
        prev = Some((r.right(), r.h, r.text.ends_with(char::is_whitespace)));
    }
    runs
}

/// Derive each line's nesting level by bucketing **marker** start-Xs, relative to
/// the shallowest marker's X (the list base). Markerless (continuation) lines map
/// to `None`. Marker starts within `body` of each other share a level; deeper
/// indents step to the next level `0, 1, 2…` in ascending-X order.
///
/// The bucketing is greedy over the sorted distinct marker Xs: each new X farther
/// than `body` from the current bucket's representative opens the next level.
/// This is robust to small per-glyph jitter while still separating a clear
/// sub-indent (a sub-bullet sits a tab/indent — well over one body size — in).
fn indent_levels(lines: &[&ReconLine], body: f64) -> Vec<Option<u8>> {
    let tol = body.max(1.0);
    // Distinct marker start-Xs, ascending.
    let mut marker_xs: Vec<f64> = lines
        .iter()
        .filter(|l| detect_marker(&l.text()).is_some())
        .map(|l| l.x)
        .collect();
    marker_xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));

    // Greedy buckets: representative X per level, ascending.
    let mut bucket_reps: Vec<f64> = Vec::new();
    for &x in &marker_xs {
        match bucket_reps.last() {
            Some(&rep) if (x - rep).abs() <= tol => {}
            _ => bucket_reps.push(x),
        }
    }

    // Map each marker line to the index of its nearest bucket; continuations None.
    lines
        .iter()
        .map(|l| {
            // Continuation lines (no marker) inherit their parent's level.
            detect_marker(&l.text())?;
            let lvl = bucket_reps
                .iter()
                .position(|&rep| (l.x - rep).abs() <= tol)
                // Fall back to the closest bucket if jitter put it just outside.
                .unwrap_or_else(|| {
                    bucket_reps
                        .iter()
                        .enumerate()
                        .min_by(|(_, a), (_, b)| {
                            (l.x - **a)
                                .abs()
                                .partial_cmp(&(l.x - **b).abs())
                                .unwrap_or(core::cmp::Ordering::Equal)
                        })
                        .map(|(i, _)| i)
                        .unwrap_or(0)
                });
            Some(lvl.min(u8::MAX as usize) as u8)
        })
        .collect()
}

/// Consume from `remaining` the text contributed by run text `t`, returning the
/// overlapping slice. Keeps multi-run list items (marker run + text run) intact.
fn item_run_text(t: &str, remaining: &mut &str) -> String {
    let rem = remaining.trim_start();
    // Skip whitespace already consumed.
    if let Some(stripped) = rem.strip_prefix(t) {
        *remaining = stripped;
        t.to_string()
    } else if rem.starts_with(t.trim()) {
        let tt = t.trim();
        *remaining = &rem[tt.len()..];
        tt.to_string()
    } else {
        // Run is (or contains) the marker that was stripped — skip it.
        String::new()
    }
}

/// Detect the marker of a line's leading token, if any.
fn detect_marker(text: &str) -> Option<Marker> {
    let t = text.trim_start();
    let mut chars = t.chars();
    let first = chars.next()?;

    // Single-glyph bullets.
    if matches!(first, '•' | '‣' | '◦' | '▪' | '·' | '*' | '-' | '–' | '—') {
        // For '-'/'–'/'*' require a following space so a hyphenated word isn't a
        // bullet ("e-mail" stays prose, "- item" is a bullet).
        let rest = &t[first.len_utf8()..];
        if matches!(first, '•' | '‣' | '◦' | '▪' | '·') || rest.starts_with(char::is_whitespace)
        {
            return Some(Marker::Bullet(first));
        }
    }

    // Ordinals: a token ending in '.' or ')' that is a number / single letter /
    // roman numeral.
    let token: String = t.chars().take_while(|c| !c.is_whitespace()).collect();
    let (label, term) = match token.chars().last() {
        Some('.') | Some(')') => (&token[..token.len() - 1], token.chars().last().unwrap()),
        _ => return None,
    };
    let _ = term;
    if label.is_empty() {
        return None;
    }
    if label.chars().all(|c| c.is_ascii_digit()) {
        return Some(Marker::Decimal);
    }
    if label.len() == 1 {
        let c = label.chars().next().unwrap();
        if c.is_ascii_lowercase() {
            return Some(Marker::LowerAlpha);
        }
        if c.is_ascii_uppercase() {
            return Some(Marker::UpperAlpha);
        }
    }
    if is_roman(label, true) {
        return Some(Marker::LowerRoman);
    }
    if is_roman(label, false) {
        return Some(Marker::UpperRoman);
    }
    None
}

/// Whether `label` is a roman numeral in the given case (≥ 2 chars, to avoid
/// classifying a lone `I`/`i`/`V`/`v` etc. as roman over alpha).
fn is_roman(label: &str, lower: bool) -> bool {
    if label.len() < 2 {
        return false;
    }
    let valid = if lower { "ivxlcdm" } else { "IVXLCDM" };
    label.chars().all(|c| valid.contains(c))
}

/// Strip the leading marker token (and the whitespace after it) from a line.
fn strip_marker(text: &str) -> String {
    let t = text.trim_start();
    let Some(marker) = detect_marker(t) else {
        return t.to_string();
    };
    match marker {
        Marker::Bullet(c) => t[c.len_utf8()..].trim_start().to_string(),
        _ => {
            let token_len = t
                .chars()
                .take_while(|c| !c.is_whitespace())
                .map(|c| c.len_utf8())
                .sum();
            t[token_len..].trim_start().to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::style::TextStyle;
    use crate::recon::lines::group_into_lines;
    use crate::recon::ReconRun;

    fn run(text: &str, x: f64, y: f64) -> ReconRun {
        ReconRun {
            text: text.to_string(),
            x,
            y,
            w: text.len() as f64 * 6.0,
            h: 12.0,
            size: 12.0,
            style: TextStyle::default(),
            rotation: 0.0,
            source_index: None,
            underline: false,
            strike: false,
        }
    }

    #[test]
    fn detects_bullets_and_ordinals() {
        assert_eq!(detect_marker("• item"), Some(Marker::Bullet('•')));
        assert_eq!(detect_marker("- item"), Some(Marker::Bullet('-')));
        assert_eq!(detect_marker("1. item"), Some(Marker::Decimal));
        assert_eq!(detect_marker("2) item"), Some(Marker::Decimal));
        assert_eq!(detect_marker("a. item"), Some(Marker::LowerAlpha));
        assert_eq!(detect_marker("IV. item"), Some(Marker::UpperRoman));
        assert_eq!(detect_marker("plain text"), None);
        // A hyphenated word is not a bullet.
        assert_eq!(detect_marker("e-mail address"), None);
    }

    #[test]
    fn strip_marker_removes_leading_token() {
        assert_eq!(strip_marker("• First point"), "First point");
        assert_eq!(strip_marker("1. First point"), "First point");
        assert_eq!(strip_marker("plain"), "plain");
    }

    #[test]
    fn consecutive_bullets_form_one_list_with_stripped_markers() {
        let runs = vec![
            run("• Apples", 72.0, 700.0),
            run("• Bananas", 72.0, 686.0),
            run("• Cherries", 72.0, 672.0),
        ];
        let lines = group_into_lines(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let mut ids = IdGen::default();
        let block = build_list(&refs, 12.0, &mut ids, Rect::new).unwrap();
        let BlockKind::List(list) = block.kind else {
            panic!("expected list");
        };
        assert!(!list.ordered);
        assert_eq!(list.marker, ListMarker::Bullet('•'));
        assert_eq!(list.items.len(), 3);
        // Markers stripped from item text.
        let BlockKind::Paragraph(p) = &list.items[0].blocks[0].kind else {
            panic!("item should be a paragraph");
        };
        let Inline::Run(r) = &p.runs[0] else {
            panic!("expected run");
        };
        assert_eq!(r.text, "Apples");
    }

    #[test]
    fn split_separates_list_from_prose() {
        let runs = vec![
            run("Intro paragraph here", 72.0, 720.0),
            run("1. first", 72.0, 700.0),
            run("2. second", 72.0, 686.0),
            run("Closing paragraph", 72.0, 660.0),
        ];
        let lines = group_into_lines(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let segs = split_lists(&refs);
        assert_eq!(segs.len(), 3);
        assert!(matches!(segs[0], Segment::Prose(_)));
        assert!(matches!(segs[1], Segment::List(_)));
        assert!(matches!(segs[2], Segment::Prose(_)));
    }

    #[test]
    fn empty_list_is_none() {
        let mut ids = IdGen::default();
        assert!(build_list(&[], 12.0, &mut ids, Rect::new).is_none());
    }

    /// Extract the flattened text of a list item's first paragraph block.
    fn item_text(item: &ListItem) -> String {
        let BlockKind::Paragraph(p) = &item.blocks[0].kind else {
            return String::new();
        };
        let mut s = String::new();
        for inl in &p.runs {
            match inl {
                Inline::Run(r) => s.push_str(&r.text),
                Inline::LineBreak => s.push(' '),
                _ => {}
            }
        }
        s
    }

    /// A parent bullet at x=72 with a sub-bullet indented to x=108 nests one
    /// level deeper, while same-indent items share a level.
    #[test]
    fn nested_sub_items_get_a_deeper_level() {
        // Marker glyph "•" is one run; its X is the marker column. body = 12, so
        // a 36pt indent (3×body) is a clear sub-level; the 0pt jitter shares it.
        let runs = vec![
            run("• Top one", 72.0, 700.0),   // level 0
            run("• Top two", 72.0, 686.0),   // level 0
            run("• Sub a", 108.0, 672.0),    // level 1 (indented 36pt)
            run("• Sub b", 108.0, 658.0),    // level 1
            run("• Top three", 72.0, 644.0), // back to level 0
        ];
        let lines = group_into_lines(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let mut ids = IdGen::default();
        let block = build_list(&refs, 12.0, &mut ids, Rect::new).unwrap();
        let BlockKind::List(list) = block.kind else {
            panic!("expected list");
        };
        assert_eq!(list.items.len(), 5, "five marker lines → five items");
        let levels: Vec<u8> = list.items.iter().map(|i| i.level).collect();
        assert_eq!(
            levels,
            vec![0, 0, 1, 1, 0],
            "sub-bullets nest a level deeper, others stay at base"
        );
    }

    /// Three indent tiers (72 / 108 / 144) bucket into levels 0, 1, 2.
    #[test]
    fn three_indent_tiers_bucket_into_levels() {
        let runs = vec![
            run("• A", 72.0, 700.0),
            run("• B", 108.0, 686.0),
            run("• C", 144.0, 672.0),
        ];
        let lines = group_into_lines(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let mut ids = IdGen::default();
        let block = build_list(&refs, 12.0, &mut ids, Rect::new).unwrap();
        let BlockKind::List(list) = block.kind else {
            panic!("expected list");
        };
        let levels: Vec<u8> = list.items.iter().map(|i| i.level).collect();
        assert_eq!(levels, vec![0, 1, 2]);
    }

    /// A wrapped continuation line (markerless, indented to the item's text
    /// column) folds into the preceding item — not a new item, not prose.
    #[test]
    fn continuation_line_folds_into_previous_item() {
        // Marker glyph at x=72; "• First" text begins ≈ x=84 (one glyph + space).
        // The continuation sits at x=84 — the wrapped-text column — so it belongs
        // to item 1. A second bullet follows.
        let runs = vec![
            run("• First item text", 72.0, 700.0),
            run("wraps onto a second line", 84.0, 686.0), // continuation of item 1
            run("• Second item", 72.0, 672.0),
        ];
        let lines = group_into_lines(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        // split_lists keeps the continuation inside the list segment.
        let segs = split_lists(&refs);
        assert_eq!(segs.len(), 1, "one list segment, no prose break");
        assert!(matches!(segs[0], Segment::List(_)));

        let mut ids = IdGen::default();
        let block = build_list(&refs, 12.0, &mut ids, Rect::new).unwrap();
        let BlockKind::List(list) = block.kind else {
            panic!("expected list");
        };
        assert_eq!(
            list.items.len(),
            2,
            "the wrapped line folds in → two items, not three"
        );
        let first = item_text(&list.items[0]);
        assert!(
            first.contains("First item text") && first.contains("wraps onto a second line"),
            "continuation text joined into item 1: {first:?}"
        );
        assert_eq!(item_text(&list.items[1]), "Second item");
    }

    /// A markerless line indented at the *marker* column (left margin), not the
    /// text column, is NOT a continuation — it breaks the list into prose.
    #[test]
    fn unindented_markerless_line_breaks_the_list() {
        let runs = vec![
            run("• Bullet one", 72.0, 700.0),
            run("• Bullet two", 72.0, 686.0),
            run("A new prose paragraph here", 72.0, 660.0), // at left margin → prose
        ];
        let lines = group_into_lines(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let segs = split_lists(&refs);
        assert_eq!(segs.len(), 2, "list then prose");
        assert!(matches!(segs[0], Segment::List(_)));
        assert!(matches!(segs[1], Segment::Prose(_)));
        // The list segment holds exactly the two bullets.
        let Segment::List(list_lines) = &segs[0] else {
            panic!();
        };
        assert_eq!(list_lines.len(), 2);
    }

    /// A continuation line attached to a *nested* sub-item keeps that item's
    /// deeper level (folding doesn't reset nesting).
    #[test]
    fn continuation_on_nested_item_preserves_its_level() {
        let runs = vec![
            run("• Parent", 72.0, 700.0),                   // level 0
            run("• Child item that is long", 108.0, 686.0), // level 1
            run("continuing the child line", 120.0, 672.0), // continuation of child
        ];
        let lines = group_into_lines(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let mut ids = IdGen::default();
        let block = build_list(&refs, 12.0, &mut ids, Rect::new).unwrap();
        let BlockKind::List(list) = block.kind else {
            panic!("expected list");
        };
        assert_eq!(list.items.len(), 2, "parent + child, continuation folded");
        assert_eq!(list.items[0].level, 0);
        assert_eq!(list.items[1].level, 1, "nested child stays level 1");
        assert!(
            item_text(&list.items[1]).contains("continuing the child line"),
            "continuation joined the nested child"
        );
    }
}
