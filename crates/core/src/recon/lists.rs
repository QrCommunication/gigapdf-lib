//! Stage 5 — **list detection**. A line whose first run is a bullet
//! (`• ‣ ◦ - – *`) or an ordinal (`1.` `1)` `a.` `IV.` …) followed by a hanging
//! indent is a [`ListItem`]; consecutive items sharing an indent collapse into a
//! single [`List`], and the marker glyph is stripped out into [`List::marker`].
//!
//! The module splits a prose group into alternating [`Segment::List`] /
//! [`Segment::Prose`] spans (so `reconstruct_page` lowers each appropriately),
//! then builds the [`List`] block from the item lines.

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

/// Split a reading-order group into list / prose segments.
pub fn split_lists<'a>(lines: &[&'a ReconLine]) -> Vec<Segment<'a>> {
    let mut segments: Vec<Segment> = Vec::new();
    let mut run: Vec<&ReconLine> = Vec::new();
    let mut run_is_list = false;

    for &line in lines {
        let is_item = detect_marker(&line.text()).is_some();
        if run.is_empty() {
            run.push(line);
            run_is_list = is_item;
            continue;
        }
        if is_item == run_is_list {
            run.push(line);
        } else {
            segments.push(finish(std::mem::take(&mut run), run_is_list));
            run.push(line);
            run_is_list = is_item;
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

/// Build a [`List`] block from item lines (each line begins a list item). The
/// marker is stripped from each item's text; the list's `ordered`/`marker` come
/// from the first item. `body` is unused beyond future indent-nesting hooks.
pub fn build_list(
    items: &[&ReconLine],
    _body: f64,
    ids: &mut IdGen,
    to_frame: impl Fn(f64, f64, f64, f64) -> Rect,
) -> Option<Block> {
    if items.is_empty() {
        return None;
    }
    let first_marker = detect_marker(&items[0].text())?;
    let mut list = List {
        ordered: first_marker.ordered(),
        marker: first_marker.to_list_marker(),
        items: Vec::new(),
    };

    let mut x0 = f64::INFINITY;
    let mut y0 = f64::INFINITY;
    let mut x1 = f64::NEG_INFINITY;
    let mut y1 = f64::NEG_INFINITY;

    for line in items {
        x0 = x0.min(line.x);
        y0 = y0.min(line.y);
        x1 = x1.max(line.x + line.w);
        y1 = y1.max(line.y + line.h);

        let stripped = strip_marker(&line.text());
        // Build the item paragraph from the line's runs, dropping the marker
        // glyph from the leading run.
        let mut runs: Vec<Inline> = Vec::new();
        let mut remaining = stripped.as_str();
        for r in &line.runs {
            if remaining.is_empty() {
                break;
            }
            let t = r.text.trim();
            if t.is_empty() {
                continue;
            }
            // Emit the portion of this run that overlaps the stripped text.
            let take = item_run_text(t, &mut remaining);
            if take.is_empty() {
                continue;
            }
            runs.push(Inline::Run(InlineRun {
                text: take,
                style: run_char_style(r),
                source_index: r.source_index,
            }));
        }
        if runs.is_empty() && !stripped.is_empty() {
            // Marker and text were one run: emit the stripped text directly.
            let style = items
                .first()
                .and_then(|l| l.runs.first())
                .map(run_char_style)
                .unwrap_or_default();
            runs.push(Inline::Run(InlineRun {
                text: stripped.clone(),
                style,
                source_index: None,
            }));
        }
        let para = Block {
            id: ids.mint(),
            frame: None,
            rotation: Rotation::D0,
            kind: BlockKind::Paragraph(Paragraph {
                style: ParagraphStyle::default(),
                style_ref: None,
                runs,
            }),
        };
        list.items.push(ListItem {
            blocks: vec![para],
            level: 0,
        });
    }

    Some(Block {
        id: ids.mint(),
        frame: Some(to_frame(x0, y0, x1 - x0, y1 - y0)),
        rotation: Rotation::D0,
        kind: BlockKind::List(list),
    })
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
}
