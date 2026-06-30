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
    /// A style discriminant for grouping markers of the *same* numbering scheme
    /// (so `1.`/`2.` group together but `1.` and `a.` do not). Bullets all share
    /// one key — their glyph variety doesn't break a coherent bullet list.
    fn style_key(&self) -> u8 {
        match self {
            Marker::Bullet(_) => 0,
            Marker::Decimal => 1,
            Marker::LowerAlpha => 2,
            Marker::UpperAlpha => 3,
            Marker::LowerRoman => 4,
            Marker::UpperRoman => 5,
        }
    }
}

/// An ordered marker's numbering delimiter — `.` or `)`. `1.` and `1)` are
/// different list FORMATS and must not merge into one run; bullets carry `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delim {
    Dot,
    Paren,
}

/// The full classification of a line's leading marker: its scheme, its
/// delimiter (ordered only), and its **ordinal value** (1-based; `None` for
/// bullets). This is the unit ordinal-sequence validation reasons over.
#[derive(Debug, Clone, PartialEq)]
struct MarkerInfo {
    marker: Marker,
    delim: Option<Delim>,
    ordinal: Option<u32>,
}

impl MarkerInfo {
    /// Two markers share a list FORMAT when their scheme key and delimiter match.
    /// Bullets share a format regardless of glyph; `1.` ≠ `1)` ≠ `a.`.
    fn same_format(&self, other: &MarkerInfo) -> bool {
        self.marker.style_key() == other.marker.style_key() && self.delim == other.delim
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
///
/// A candidate list run is not taken on the strength of its markers alone:
/// before it is emitted it passes through [`classify_list_run`], which requires
/// the markers to form a coherent **ordinal sequence** (consecutive/monotonic in
/// one format, plausible first ordinal) — so numeric sentences, citations and
/// stray section numbers (`12. Smith et al.`) fall back to prose instead of
/// becoming phantom lists.
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
                push_run(&mut segments, std::mem::take(&mut run), run_is_list);
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
                push_run(&mut segments, std::mem::take(&mut run), run_is_list);
                run_is_list = false;
            }
            run.push(line);
        }
    }
    if !run.is_empty() {
        push_run(&mut segments, run, run_is_list);
    }
    segments
}

/// Emit a finished run as one or more segments. Prose passes straight through; a
/// candidate list run is validated/sub-split by [`classify_list_run`] (which may
/// yield a mix of [`Segment::List`] and [`Segment::Prose`] when only part of the
/// run is a genuine ordinal sequence).
fn push_run<'a>(segments: &mut Vec<Segment<'a>>, lines: Vec<&'a ReconLine>, is_list: bool) {
    if !is_list {
        if !lines.is_empty() {
            coalesce_prose(segments, lines);
        }
        return;
    }
    for seg in classify_list_run(lines) {
        match seg {
            Segment::Prose(p) => coalesce_prose(segments, p),
            list => segments.push(list),
        }
    }
}

/// Append prose lines, merging into the preceding [`Segment::Prose`] if one is
/// adjacent — so a rejected phantom-list run rejoins surrounding prose as a
/// single paragraph group rather than splintering paragraph reconstruction.
fn coalesce_prose<'a>(segments: &mut Vec<Segment<'a>>, mut lines: Vec<&'a ReconLine>) {
    if lines.is_empty() {
        return;
    }
    if let Some(Segment::Prose(prev)) = segments.last_mut() {
        prev.append(&mut lines);
    } else {
        segments.push(Segment::Prose(lines));
    }
}

/// Validate (and, on a same-level format change, sub-split) a candidate list run.
///
/// The run is a contiguous block of marker lines plus their wrapped continuation
/// lines (the only markerless lines [`split_lists`] keeps inside a list run). It
/// is partitioned into **format-coherent sub-runs**: a marker whose FORMAT
/// (scheme + delimiter) differs from the sub-run's *base* format **at the same
/// nesting level** starts a new sub-run, while a differently-formatted marker at
/// a **deeper** indent is legitimate nesting and stays (so `build_list` keeps the
/// sub-list). Each sub-run is then accepted as a [`Segment::List`] only if its
/// base-level markers pass [`is_ordinal_sequence`]; otherwise it degrades to
/// [`Segment::Prose`].
fn classify_list_run<'a>(lines: Vec<&'a ReconLine>) -> Vec<Segment<'a>> {
    // Bucket marker start-Xs into nesting levels using each line's own height as
    // the tolerance (mirrors `build_list`'s `body`-tolerance bucketing — which is
    // unavailable here, `split_lists` taking no body size).
    let levels = run_levels(&lines);

    let mut out: Vec<Segment> = Vec::new();
    let mut sub: Vec<&ReconLine> = Vec::new();
    // The base (shallowest) marker info + level of the sub-run being built.
    let mut base: Option<(MarkerInfo, u8)> = None;

    for &line in &lines {
        let info = classify_marker(&line.text());
        let level = levels.get(&line_key(line)).copied();
        match (&info, level) {
            (Some(info), Some(level)) => {
                match &base {
                    Some((base_info, base_level))
                        if level <= *base_level && !info.same_format(base_info) =>
                    {
                        // Format change at/above the base level → break the run.
                        out.push(classify_sub_run(std::mem::take(&mut sub), &levels));
                        base = Some((info.clone(), level));
                    }
                    Some((_, base_level)) if level < *base_level => {
                        // A *shallower* marker of a compatible format becomes the
                        // new base (the list's true outer level).
                        base = Some((info.clone(), level));
                    }
                    None => base = Some((info.clone(), level)),
                    _ => {}
                }
                sub.push(line);
            }
            // A continuation line (markerless) stays with the current sub-run.
            _ => sub.push(line),
        }
    }
    if !sub.is_empty() {
        out.push(classify_sub_run(sub, &levels));
    }
    out
}

/// Decide whether one format-coherent sub-run is a genuine list ([`Segment::List`])
/// or a phantom ([`Segment::Prose`]). Bullets are always lists (no ordinal
/// requirement, keeping the existing `detect_marker` guards); an ordered sub-run
/// must pass [`is_ordinal_sequence`] over its **base-level** markers.
fn classify_sub_run<'a>(
    lines: Vec<&'a ReconLine>,
    levels: &std::collections::HashMap<u64, u8>,
) -> Segment<'a> {
    let markers: Vec<(MarkerInfo, u8)> = lines
        .iter()
        .filter_map(|l| {
            let info = classify_marker(&l.text())?;
            let level = levels.get(&line_key(l)).copied()?;
            Some((info, level))
        })
        .collect();
    let Some((first, _)) = markers.first().cloned() else {
        return Segment::Prose(lines);
    };
    // The list's own level is the shallowest marker present; deeper markers are
    // nested sub-list items (validated through their own base ordinal run).
    let base_level = markers.iter().map(|(_, lvl)| *lvl).min().unwrap_or(0);
    // Bullets: a coherent list with no ordinal demand (one bullet is still a list).
    if !first.marker.ordered() {
        return Segment::List(lines);
    }
    // Ordered: validate the ordinal run formed by the base-level markers only
    // (deeper markers belong to nested sub-lists, validated by their own base).
    let base_ords: Vec<u32> = markers
        .iter()
        .filter(|(_, lvl)| *lvl == base_level)
        .filter_map(|(info, _)| info.ordinal)
        .collect();
    let total_markers = markers.len();
    if is_ordinal_sequence(&base_ords, total_markers) {
        Segment::List(lines)
    } else {
        Segment::Prose(lines)
    }
}

/// Whether `ords` (the base-level ordinals of an ordered sub-run, in reading
/// order) form a coherent list sequence:
///
/// * **Monotonic with bounded gaps** — strictly increasing, each step in
///   `1..=GAP_TOL` (so `1 2 3` and `1 2 4` pass, `5 1 9` and a duplicate `1 1`
///   fail). A small gap tolerance absorbs an OCR-missed item without admitting
///   arbitrary jumps.
/// * **Plausible first ordinal** — starts at `1` (`1`/`a`/`i`/`A`/`I`), *or*
///   starts mid-stream only with corroboration: ≥ 3 items all stepping by exactly
///   `1` (a list continued from an earlier column/page). This is what rejects a
///   lone `12. Smith et al.` or a stray `7.` line.
///
/// A **lone** ordered marker (`total_markers == 1`) is never a list — a single
/// `1.` paragraph is prose. (Unordered single bullets are handled before here.)
fn is_ordinal_sequence(ords: &[u32], total_markers: usize) -> bool {
    /// Largest accepted gap between consecutive ordinals (absorbs one missed item).
    const GAP_TOL: u32 = 2;
    // A lone ordered marker, or markers we couldn't value, is not a list.
    if total_markers <= 1 || ords.len() < 2 {
        return false;
    }
    // Strictly increasing with bounded positive gaps.
    let monotonic = ords
        .windows(2)
        .all(|w| w[1] > w[0] && w[1] - w[0] <= GAP_TOL);
    if !monotonic {
        return false;
    }
    // Plausible start: at 1, or a corroborated mid-list continuation.
    if ords[0] == 1 {
        return true;
    }
    ords.len() >= 3 && ords.windows(2).all(|w| w[1] - w[0] == 1)
}

/// A stable identity for a line within one run — its `(x, y)` packed into a key
/// for the per-line level map. Lines in a reconstruction group have distinct
/// baselines, so this never collides within a run.
fn line_key(line: &ReconLine) -> u64 {
    let x = (line.x * 16.0).round() as i64 as u64;
    let y = (line.y * 16.0).round() as i64 as u64;
    x.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(y)
}

/// Bucket the **marker** lines of a run into nesting levels (`0` = shallowest),
/// keyed by [`line_key`]. Uses each line's height as the bucket tolerance — the
/// body size is unavailable in [`split_lists`] — which is enough to separate a
/// clear sub-indent (a tab/indent, well over one line height) from per-glyph
/// jitter. Markerless lines are absent from the map (they inherit context).
fn run_levels(lines: &[&ReconLine]) -> std::collections::HashMap<u64, u8> {
    let mut marker_xs: Vec<f64> = lines
        .iter()
        .filter(|l| detect_marker(&l.text()).is_some())
        .map(|l| l.x)
        .collect();
    marker_xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));

    // A representative tolerance: the median marker line height (fallback 1.0).
    let tol = lines
        .iter()
        .filter(|l| detect_marker(&l.text()).is_some())
        .map(|l| l.h)
        .fold(0.0_f64, f64::max)
        .max(1.0);

    let mut bucket_reps: Vec<f64> = Vec::new();
    for &x in &marker_xs {
        match bucket_reps.last() {
            Some(&rep) if (x - rep).abs() <= tol => {}
            _ => bucket_reps.push(x),
        }
    }

    let mut map = std::collections::HashMap::new();
    for l in lines {
        if detect_marker(&l.text()).is_none() {
            continue;
        }
        let lvl = bucket_reps
            .iter()
            .position(|&rep| (l.x - rep).abs() <= tol)
            .unwrap_or(0);
        map.insert(line_key(l), lvl.min(u8::MAX as usize) as u8);
    }
    map
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

        ..Default::default()
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
                    blocks: vec![paragraph_block(runs, line.rotation(), ids)],
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
                    blocks: vec![paragraph_block(runs, line.rotation(), ids)],
                    level: *level,
                });
            }
        }
    }

    Some(Block {
        id: ids.mint(),
        frame: Some(to_frame(x0, y0, x1 - x0, y1 - y0)),
        // A rotated list (every item drawn on an angled baseline) lowers with
        // that rotation; an ordinary upright list stays `Rotation::D0`.
        rotation: super::lines::lines_rotation(items),
        kind: BlockKind::List(list),
    })
}

/// Wrap a run of [`Inline`]s into a default-styled paragraph [`Block`] carrying
/// `rotation` (the orientation of the list item's source line, so a rotated
/// list item lowers rotated rather than flattened upright).
fn paragraph_block(runs: Vec<Inline>, rotation: Rotation, ids: &mut IdGen) -> Block {
    Block {
        id: ids.mint(),
        frame: None,
        rotation,
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

/// Classify a line's leading marker into its scheme + delimiter + ordinal value
/// — the unit ordinal-sequence validation works on. Bullets get `ordinal:None`;
/// ordered markers parse their label (`12.` → 12, `c)` → 3, `iv.` → 4).
fn classify_marker(text: &str) -> Option<MarkerInfo> {
    let marker = detect_marker(text)?;
    if let Marker::Bullet(_) = marker {
        return Some(MarkerInfo {
            marker,
            delim: None,
            ordinal: None,
        });
    }
    let t = text.trim_start();
    let token: String = t.chars().take_while(|c| !c.is_whitespace()).collect();
    let delim = match token.chars().last() {
        Some('.') => Delim::Dot,
        Some(')') => Delim::Paren,
        _ => return None,
    };
    let label = &token[..token.len() - 1];
    let ordinal = marker_ordinal(&marker, label);
    Some(MarkerInfo {
        marker,
        delim: Some(delim),
        ordinal,
    })
}

/// The 1-based ordinal value of an ordered marker's `label`:
/// decimal digits → the number; a single letter → its position in the alphabet
/// (`a`/`A` = 1 … `z`/`Z` = 26); a roman numeral → its decoded value. `None`
/// when the label can't be valued (out-of-range decimal, malformed roman).
fn marker_ordinal(marker: &Marker, label: &str) -> Option<u32> {
    match marker {
        Marker::Decimal => label.parse::<u32>().ok(),
        Marker::LowerAlpha | Marker::UpperAlpha => {
            let c = label.chars().next()?.to_ascii_lowercase();
            if c.is_ascii_lowercase() {
                Some((c as u32) - ('a' as u32) + 1)
            } else {
                None
            }
        }
        Marker::LowerRoman | Marker::UpperRoman => roman_value(&label.to_ascii_lowercase()),
        Marker::Bullet(_) => None,
    }
}

/// Decode a (lowercase) roman numeral to its integer value with the standard
/// subtractive rule (`iv` = 4, `ix` = 9). Returns `None` on any non-roman glyph.
fn roman_value(label: &str) -> Option<u32> {
    fn digit(c: char) -> Option<i64> {
        Some(match c {
            'i' => 1,
            'v' => 5,
            'x' => 10,
            'l' => 50,
            'c' => 100,
            'd' => 500,
            'm' => 1000,
            _ => return None,
        })
    }
    if label.is_empty() {
        return None;
    }
    let vals: Vec<i64> = label.chars().map(digit).collect::<Option<_>>()?;
    // Left-to-right: a digit smaller than the one after it is subtractive.
    // `i64` keeps the running total well clear of any unsigned underflow.
    let mut total: i64 = 0;
    for i in 0..vals.len() {
        if i + 1 < vals.len() && vals[i] < vals[i + 1] {
            total -= vals[i];
        } else {
            total += vals[i];
        }
    }
    (total > 0 && total <= u32::MAX as i64).then_some(total as u32)
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

    /// Count the [`Segment::List`] segments produced for a set of lines.
    fn list_segments(segs: &[Segment]) -> usize {
        segs.iter()
            .filter(|s| matches!(s, Segment::List(_)))
            .count()
    }

    /// `1. 2. 3.` on consecutive lines is a coherent ordinal sequence → one
    /// ordered list of three items.
    #[test]
    fn consecutive_decimals_form_one_ordered_list() {
        let runs = vec![
            run("1. first", 72.0, 700.0),
            run("2. second", 72.0, 686.0),
            run("3. third", 72.0, 672.0),
        ];
        let lines = group_into_lines(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let segs = split_lists(&refs);
        assert_eq!(segs.len(), 1, "one segment");
        assert!(matches!(segs[0], Segment::List(_)), "and it is a list");

        let mut ids = IdGen::default();
        let block = build_list(&refs, 12.0, &mut ids, Rect::new).unwrap();
        let BlockKind::List(list) = block.kind else {
            panic!("expected list");
        };
        assert!(list.ordered, "decimals → ordered");
        assert_eq!(list.marker, ListMarker::Decimal);
        assert_eq!(list.items.len(), 3);
    }

    /// An ordered list that does NOT start at 1 (`3. 4. 5.`) is still a coherent
    /// list — a continuation from an earlier column/page — because ≥ 3 items step
    /// by exactly 1 (gap #75, sub-item 8). The single-item / uncorroborated cases
    /// that must stay prose are covered by the lone-marker tests above.
    #[test]
    fn ordered_list_starting_above_one_is_a_continuation() {
        let runs = vec![
            run("3. third", 72.0, 700.0),
            run("4. fourth", 72.0, 686.0),
            run("5. fifth", 72.0, 672.0),
        ];
        let lines = group_into_lines(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let segs = split_lists(&refs);
        assert_eq!(list_segments(&segs), 1, "3 4 5 is a continuation list");
        let mut ids = IdGen::default();
        let block = build_list(&refs, 12.0, &mut ids, Rect::new).unwrap();
        let BlockKind::List(list) = block.kind else {
            panic!("expected list");
        };
        assert!(list.ordered);
        assert_eq!(list.items.len(), 3, "all three items kept");
    }

    /// `1. 2. 4.` (a single missed item) is still a coherent ordered list, but
    /// `5. 1. 9.` (non-monotonic) is rejected as prose — the ordinal-sequence
    /// guard tolerates a small gap, not arbitrary jumps.
    #[test]
    fn gap_tolerated_but_non_monotonic_rejected() {
        let ok = vec![
            run("1. a", 72.0, 700.0),
            run("2. b", 72.0, 686.0),
            run("4. c", 72.0, 672.0),
        ];
        let lines = group_into_lines(&ok);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let segs = split_lists(&refs);
        assert_eq!(list_segments(&segs), 1, "1 2 4 is a list (gap tolerated)");

        let bad = vec![
            run("5. a", 72.0, 700.0),
            run("1. b", 72.0, 686.0),
            run("9. c", 72.0, 672.0),
        ];
        let lines = group_into_lines(&bad);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let segs = split_lists(&refs);
        assert_eq!(list_segments(&segs), 0, "5 1 9 is not a list (scrambled)");
        assert!(segs.iter().all(|s| matches!(s, Segment::Prose(_))));
    }

    /// A lone `12. Smith et al.` followed by an ordinary (non-marker) line is a
    /// citation, not a list — a single ordered marker with no sequel is prose.
    #[test]
    fn lone_ordered_marker_with_following_prose_is_not_a_list() {
        let runs = vec![
            run("12. Smith et al. study the matter", 72.0, 700.0),
            run("and conclude something entirely else", 72.0, 686.0),
        ];
        let lines = group_into_lines(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let segs = split_lists(&refs);
        assert_eq!(list_segments(&segs), 0, "a lone citation marker is prose");
        // The two lines fall through as prose (and coalesce into one group).
        assert!(segs.iter().all(|s| matches!(s, Segment::Prose(_))));
    }

    /// A bare `5.` numbered-sentence opener with no second ordinal is prose, not
    /// a one-item ordered list.
    #[test]
    fn lone_decimal_paragraph_is_prose() {
        let runs = vec![run(
            "5. This sentence merely starts with a number.",
            72.0,
            700.0,
        )];
        let lines = group_into_lines(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let segs = split_lists(&refs);
        assert_eq!(list_segments(&segs), 0);
        assert!(matches!(segs[0], Segment::Prose(_)));
    }

    /// A price like `$5.99` never reads as a marker (the `$` prefix and the
    /// trailing digit mean no `digit.`/`digit)` token), so a price line is prose.
    #[test]
    fn price_line_is_not_a_list() {
        assert_eq!(detect_marker("$5.99 per unit"), None);
        assert_eq!(classify_marker("$5.99 per unit"), None);
        let runs = vec![
            run("$5.99 per unit", 72.0, 700.0),
            run("$9.49 in bulk", 72.0, 686.0),
        ];
        let lines = group_into_lines(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let segs = split_lists(&refs);
        assert_eq!(list_segments(&segs), 0, "prices are prose, not a list");
    }

    /// Mixing `1.` and `a)` at the same indent must not collapse into one list:
    /// the format change breaks the run, and neither lone fragment is a list.
    #[test]
    fn mixed_decimal_and_alpha_paren_not_merged() {
        let runs = vec![run("1. first", 72.0, 700.0), run("a) second", 72.0, 686.0)];
        let lines = group_into_lines(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let segs = split_lists(&refs);
        // No single list contains both markers.
        for seg in &segs {
            if let Segment::List(lines) = seg {
                assert!(
                    lines.len() < 2,
                    "a `1.` and an `a)` must not share one list"
                );
            }
        }
        // Two lone, differently-formatted markers → neither is a list.
        assert_eq!(list_segments(&segs), 0);
    }

    /// A genuine `a) b) c)` run is a coherent alpha sequence → one ordered list
    /// with the lower-alpha marker.
    #[test]
    fn alpha_paren_sequence_is_an_ordered_list() {
        let runs = vec![
            run("a) apples", 72.0, 700.0),
            run("b) bananas", 72.0, 686.0),
            run("c) cherries", 72.0, 672.0),
        ];
        let lines = group_into_lines(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let segs = split_lists(&refs);
        assert_eq!(segs.len(), 1);
        assert!(matches!(segs[0], Segment::List(_)));

        let mut ids = IdGen::default();
        let block = build_list(&refs, 12.0, &mut ids, Rect::new).unwrap();
        let BlockKind::List(list) = block.kind else {
            panic!("expected list");
        };
        assert!(list.ordered);
        assert_eq!(list.marker, ListMarker::LowerAlpha);
        assert_eq!(list.items.len(), 3);
    }

    /// Unordered bullets need no ordinal validation: `- x` / `- y` stay a list.
    #[test]
    fn unordered_dash_bullets_stay_a_list() {
        let runs = vec![run("- alpha", 72.0, 700.0), run("- beta", 72.0, 686.0)];
        let lines = group_into_lines(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let segs = split_lists(&refs);
        assert_eq!(segs.len(), 1);
        assert!(matches!(segs[0], Segment::List(_)));
        let Segment::List(list_lines) = &segs[0] else {
            panic!();
        };
        assert_eq!(list_lines.len(), 2);
    }

    /// A single standalone bullet is still a valid one-item list (unlike a lone
    /// ordered marker, which is prose).
    #[test]
    fn single_standalone_bullet_is_a_list() {
        let runs = vec![run("- the only item", 72.0, 700.0)];
        let lines = group_into_lines(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let segs = split_lists(&refs);
        assert_eq!(segs.len(), 1);
        assert!(
            matches!(segs[0], Segment::List(_)),
            "a lone bullet is a list"
        );

        let mut ids = IdGen::default();
        let block = build_list(&refs, 12.0, &mut ids, Rect::new).unwrap();
        let BlockKind::List(list) = block.kind else {
            panic!("expected list");
        };
        assert!(!list.ordered);
        assert_eq!(list.items.len(), 1);
    }

    /// A real ordered list with a nested ordinal sub-sequence at a deeper indent
    /// survives validation: the outer `1. 2.` and the inner `a. b.` are each
    /// their own coherent ordinal run, kept as one nested list.
    #[test]
    fn nested_ordinal_sub_sequence_survives_validation() {
        let runs = vec![
            run("1. Top one", 72.0, 700.0),  // level 0 decimal
            run("a. Sub one", 108.0, 686.0), // level 1 alpha (own run)
            run("b. Sub two", 108.0, 672.0), // level 1 alpha
            run("2. Top two", 72.0, 658.0),  // level 0 decimal
        ];
        let lines = group_into_lines(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let segs = split_lists(&refs);
        assert_eq!(segs.len(), 1, "nesting stays one list segment");
        assert!(matches!(segs[0], Segment::List(_)));

        let mut ids = IdGen::default();
        let block = build_list(&refs, 12.0, &mut ids, Rect::new).unwrap();
        let BlockKind::List(list) = block.kind else {
            panic!("expected list");
        };
        assert_eq!(list.items.len(), 4, "four items, two nested");
        let levels: Vec<u8> = list.items.iter().map(|i| i.level).collect();
        assert_eq!(levels, vec![0, 1, 1, 0]);
    }

    /// A roman-numeral ordered sequence (`i. ii. iii.`) is decoded to 1,2,3 and
    /// accepted as a coherent list.
    #[test]
    fn lower_roman_sequence_is_an_ordered_list() {
        assert_eq!(roman_value("iv"), Some(4));
        assert_eq!(roman_value("ix"), Some(9));
        let runs = vec![
            run("ii. second", 72.0, 700.0),
            run("iii. third", 72.0, 686.0),
            run("iv. fourth", 72.0, 672.0),
        ];
        let lines = group_into_lines(&runs);
        let refs: Vec<&ReconLine> = lines.iter().collect();
        let segs = split_lists(&refs);
        // ii, iii, iv = 2,3,4 — strictly +1 over ≥3 items → corroborated mid-list.
        assert_eq!(
            list_segments(&segs),
            1,
            "ii iii iv is an ordered roman list"
        );
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
