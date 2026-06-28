//! Markdown → unified editable [`Document`](crate::model::Document) importer.
//!
//! A small, dependency-free **CommonMark-ish** parser that lowers the common
//! constructs straight into the model tree (the same target the HTML/Office/text
//! importers build), so Markdown becomes a first-class editable source format:
//!
//! - ATX headings `#`..`######` → [`Heading`] (level 1..=6), sized and bold so
//!   they render distinctly in the model→PDF fallback.
//! - Setext headings (`Title` underlined by `===` ⇒ h1, `---` ⇒ h2).
//! - Paragraphs (blank-line separated) with inline styling.
//! - Lists: `-`/`*`/`+` (unordered) and `1.`/`1)` (ordered), nesting by indent;
//!   GFM task-list items `- [ ]`/`- [x]` get a leading `☐`/`☑` glyph (the model
//!   has no checkbox slot on a list item).
//! - Inline: `**bold**`/`__bold__`, `*italic*`/`_italic_`, `~~strike~~` (GFM),
//!   `` `code` ``, `[text](url)` links, `![alt](url)` images.
//! - Reference links `[text][ref]`/`[text][]` resolved against `[ref]: url`
//!   definitions, and footnote references `[^id]` against `[^id]: text` defs
//!   (both collected in a first pass).
//! - Inline HTML: the common phrasing tags (`<b>`/`<strong>`, `<i>`/`<em>`,
//!   `<code>`, `<u>`, `<s>`/`<del>`, `<a href>`, `<br>`) map to runs/links;
//!   character references (`&amp;`, `&#233;`…) decode; unknown tags degrade
//!   gracefully (the tag is dropped, its text kept).
//! - Fenced code blocks ```` ``` ```` / `~~~` → monospace paragraphs (verbatim).
//! - Block quotes `>` → indented paragraphs.
//! - Thematic breaks `---`/`***`/`___` → a thin full-width rule (a [`Shape`]).
//! - GFM pipe tables `| a | b |` with a `---|---` separator row → [`Table`].
//!
//! CommonMark precedence is preserved where it matters: backslash escapes and
//! code spans still win over the other inline markers.

use crate::convert::style::Generic;
use crate::model::{
    Block, BlockKind, Blockquote, Cell, CharStyle, CodeBlock, Document, Heading, ImageRef, Inline,
    InlineRun, LinkTarget, List, ListItem, ListMarker, Page, Paragraph, Row, Section, Table,
};
use std::collections::HashMap;

/// Body font size used for paragraph runs (points). Mirrors the model→PDF
/// fallback's default so spacing stays predictable.
const BODY_PT: f64 = 11.0;
/// Monospace size for inline `code` and fenced code blocks (points).
const CODE_PT: f64 = 10.0;

/// Per-level heading sizes (index 0 ⇒ `#`/h1 … index 5 ⇒ `######`/h6), points.
const HEADING_PT: [f64; 6] = [24.0, 20.0, 16.0, 14.0, 12.0, 11.0];

/// Leading glyph for an unchecked / checked GFM task-list item (the model has no
/// boolean checkbox on a [`ListItem`], so the state is rendered as a glyph).
const TASK_UNCHECKED: char = '\u{2610}'; // ☐
const TASK_CHECKED: char = '\u{2611}'; // ☑

/// Link-reference and footnote definitions, collected in a first pass so that
/// `[text][ref]` / `[^id]` references anywhere in the document can resolve.
#[derive(Default)]
struct Defs {
    /// `[label]: url "title"` ⇒ normalized label → (url, title).
    links: HashMap<String, (String, Option<String>)>,
    /// `[^id]: text` ⇒ id → footnote text.
    footnotes: HashMap<String, String>,
}

/// Markdown text → [`Document`]: one section / one page of flow blocks. Never
/// fails — unrecognized syntax degrades to plain paragraph text.
pub fn md_to_model(md: &str) -> Document {
    let lines: Vec<&str> = md.lines().collect();
    let defs = collect_defs(&lines);
    let blocks = parse_blocks(&lines, &defs);
    Document {
        sections: vec![Section {
            geometry: crate::model::PageGeometry::default(),
            header: None,
            footer: None,
            pages: vec![Page {
                blocks,
                absolute: false,
            }],
        }],
        ..Document::default()
    }
}

/// Collect link-reference (`[label]: url "title"`) and footnote (`[^id]: text`)
/// definitions in a single forward pass over the source lines.
fn collect_defs(lines: &[&str]) -> Defs {
    let mut defs = Defs::default();
    for raw in lines {
        if let Some((id, text)) = footnote_def(raw) {
            defs.footnotes.entry(id).or_insert(text);
        } else if let Some((label, url, title)) = link_ref_def(raw) {
            defs.links.entry(label).or_insert((url, title));
        }
    }
    defs
}

/// Parse a footnote definition `[^id]: text` (label may not be empty). Returns
/// the bare id and the trimmed text.
fn footnote_def(raw: &str) -> Option<(String, String)> {
    let t = raw.trim_start();
    let inner = t.strip_prefix("[^")?;
    let close = inner.find(']')?;
    let id = &inner[..close];
    if id.is_empty() {
        return None;
    }
    let after = inner[close + 1..].strip_prefix(':')?;
    Some((id.to_string(), after.trim().to_string()))
}

/// Parse a link-reference definition `[label]: url "title"` (title optional,
/// `"…"`/`'…'`/`(…)`). Returns the normalized label, the url, and the title.
/// Footnote definitions (`[^…]`) are explicitly excluded.
fn link_ref_def(raw: &str) -> Option<(String, String, Option<String>)> {
    let t = raw.trim_start();
    let inner = t.strip_prefix('[')?;
    if inner.starts_with('^') {
        return None; // a footnote definition, not a link reference
    }
    let close = inner.find(']')?;
    let label = &inner[..close];
    if label.is_empty() {
        return None;
    }
    let after = inner[close + 1..].strip_prefix(':')?.trim();
    if after.is_empty() {
        return None;
    }
    let mut parts = after.splitn(2, char::is_whitespace);
    let url = parts.next().unwrap_or("").trim();
    if url.is_empty() {
        return None;
    }
    let title = parts
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(strip_title_quotes);
    Some((normalize_ref(label), url.to_string(), title))
}

/// Strip a single layer of surrounding `"…"`, `'…'`, or `(…)` from a link title.
fn strip_title_quotes(s: &str) -> String {
    let pairs = [('"', '"'), ('\'', '\''), ('(', ')')];
    for (open, close) in pairs {
        if let Some(rest) = s.strip_prefix(open) {
            if let Some(body) = rest.strip_suffix(close) {
                return body.to_string();
            }
        }
    }
    s.to_string()
}

/// Normalize a reference label for case-insensitive, whitespace-collapsed lookup
/// (CommonMark label matching).
fn normalize_ref(label: &str) -> String {
    label
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Parse a slice of source lines into top-level [`Block`]s.
fn parse_blocks(lines: &[&str], defs: &Defs) -> Vec<Block> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let raw = lines[i];
        let trimmed = raw.trim();

        // Blank line → paragraph separator (nothing to emit on its own).
        if trimmed.is_empty() {
            i += 1;
            continue;
        }

        // Link-reference / footnote definition lines carry no rendered content
        // (they were harvested in `collect_defs`); skip them.
        if footnote_def(raw).is_some() || link_ref_def(raw).is_some() {
            i += 1;
            continue;
        }

        // Fenced code block: ``` or ~~~ (≥3), captured verbatim until the fence
        // closes (or EOF). The text after the opening fence is the info-string;
        // its first word is the language hint.
        if let Some(fence) = code_fence(trimmed) {
            let lang = fence_info_lang(trimmed, fence.0, fence.1);
            i += 1;
            let mut code = String::new();
            while i < lines.len() {
                let l = lines[i];
                if code_fence(l.trim()).map(|f| f.0) == Some(fence.0) && l.trim().len() >= fence.1 {
                    i += 1;
                    break;
                }
                if !code.is_empty() {
                    code.push('\n');
                }
                code.push_str(l);
                i += 1;
            }
            out.push(code_block(lang, code));
            continue;
        }

        // Thematic break: a line of ≥3 of `-`, `*` or `_` (spaces allowed).
        if is_thematic_break(trimmed) {
            out.push(rule_block());
            i += 1;
            continue;
        }

        // ATX heading: 1..=6 leading `#` then a space (or end of line).
        if let Some((level, text)) = atx_heading(trimmed) {
            out.push(heading_block(level, &text, defs));
            i += 1;
            continue;
        }

        // Setext heading: a text line underlined by `===` (h1) or `---` (h2).
        // The underline must not be a thematic break or a table delimiter, and
        // the text line must be plain (not itself a structural construct).
        if let Some(level) = setext_underline(lines.get(i + 1).copied()) {
            if is_setext_text(raw) {
                out.push(heading_block(level, trimmed, defs));
                i += 2;
                continue;
            }
        }

        // GFM pipe table: current line looks like a table row and the next is a
        // delimiter row (`---|:--:|...`).
        if looks_like_table_row(raw) && lines.get(i + 1).is_some_and(|n| is_table_delimiter(n)) {
            let (table, consumed) = parse_table(&lines[i..], defs);
            out.push(table);
            i += consumed;
            continue;
        }

        // Block quote: one or more consecutive `>` lines, recursively parsed and
        // wrapped in a semantic Blockquote (so nested constructs survive a
        // round trip).
        if quote_prefix(raw).is_some() {
            let mut inner: Vec<&str> = Vec::new();
            while i < lines.len() {
                match quote_prefix(lines[i]) {
                    Some(rest) => {
                        inner.push(rest);
                        i += 1;
                    }
                    None if lines[i].trim().is_empty() => break,
                    None => break,
                }
            }
            let owned: Vec<String> = inner.iter().map(|s| s.to_string()).collect();
            let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
            out.push(blockquote_block(parse_blocks(&refs, defs)));
            continue;
        }

        // List: a run of items each introduced by a bullet (`-`/`*`/`+`) or an
        // ordinal (`N.`/`N)`). Mixed markers split into separate lists.
        if list_item_marker(raw).is_some() {
            let (list, consumed) = parse_list(&lines[i..], defs);
            out.push(list);
            i += consumed;
            continue;
        }

        // Otherwise: a paragraph — gather consecutive non-blank, non-structural
        // lines and join them with spaces (soft wraps), then parse inlines.
        let mut para_text = String::new();
        while i < lines.len() {
            let l = lines[i];
            let t = l.trim();
            if t.is_empty()
                || atx_heading(t).is_some()
                || is_thematic_break(t)
                || code_fence(t).is_some()
                || list_item_marker(l).is_some()
                || quote_prefix(l).is_some()
                || footnote_def(l).is_some()
                || link_ref_def(l).is_some()
            {
                break;
            }
            // A following setext underline turns the line we've just collected
            // into a heading — hand the underline back to the top of the loop.
            if !para_text.is_empty() && setext_underline(Some(l)).is_some() {
                break;
            }
            if !para_text.is_empty() {
                para_text.push(' ');
            }
            para_text.push_str(t);
            i += 1;
        }
        out.push(paragraph_block(parse_inlines(&para_text, defs)));
    }
    out
}

/// The heading level of a setext underline: a line of only `=` (⇒ 1) or only `-`
/// (⇒ 2), at least one marker. `None` for any other line (including `None`/EOF).
fn setext_underline(line: Option<&str>) -> Option<u8> {
    let t = line?.trim();
    if t.is_empty() {
        return None;
    }
    if t.chars().all(|c| c == '=') {
        return Some(1);
    }
    if t.chars().all(|c| c == '-') {
        return Some(2);
    }
    None
}

/// Whether `raw` may be the text line of a setext heading: a non-blank line that
/// is not itself a structural construct (heading, list, quote, fence, table…).
fn is_setext_text(raw: &str) -> bool {
    let t = raw.trim();
    !t.is_empty()
        && atx_heading(t).is_none()
        && !is_thematic_break(t)
        && code_fence(t).is_none()
        && list_item_marker(raw).is_none()
        && quote_prefix(raw).is_none()
        && !looks_like_table_row(raw)
        && footnote_def(raw).is_none()
        && link_ref_def(raw).is_none()
}

// ── block builders ──────────────────────────────────────────────────────────

/// Wrap inline runs in a body [`Paragraph`] block.
fn paragraph_block(runs: Vec<Inline>) -> Block {
    Block {
        kind: BlockKind::Paragraph(Paragraph {
            runs,
            ..Paragraph::default()
        }),
        ..Block::default()
    }
}

/// A level-`level` [`Heading`] whose single run is sized and bold.
fn heading_block(level: u8, text: &str, defs: &Defs) -> Block {
    let size = HEADING_PT[(level.clamp(1, 6) - 1) as usize];
    let mut runs = parse_inlines(text, defs);
    for inline in &mut runs {
        if let Inline::Run(r) = inline {
            r.style.size_pt = size;
            r.style.bold = true;
        }
    }
    Block {
        kind: BlockKind::Heading(Heading {
            level: level.clamp(1, 6),
            para: Paragraph {
                runs,
                ..Paragraph::default()
            },
        }),
        ..Block::default()
    }
}

/// A fenced-code-block → a semantic [`CodeBlock`]: verbatim `code` with the
/// optional language hint from the fence info-string.
fn code_block(lang: Option<String>, code: String) -> Block {
    Block {
        kind: BlockKind::CodeBlock(CodeBlock { lang, code }),
        ..Block::default()
    }
}

/// A thematic break → a semantic [`BlockKind::HorizontalRule`].
fn rule_block() -> Block {
    Block {
        kind: BlockKind::HorizontalRule,
        ..Block::default()
    }
}

/// The language hint of a fence info-string: the text after the opening fence
/// run, first whitespace-delimited word, lower-cased. `None` when absent.
fn fence_info_lang(trimmed: &str, fence_char: char, run_len: usize) -> Option<String> {
    // Skip exactly the fence run, then read the info-string's first token.
    let after: String = trimmed
        .chars()
        .skip_while(|&c| c == fence_char)
        .collect::<String>();
    let _ = run_len;
    let token = after.split_whitespace().next()?;
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

/// Wrap a list of blocks in a semantic [`Blockquote`].
fn blockquote_block(blocks: Vec<Block>) -> Block {
    Block {
        kind: BlockKind::Blockquote(Blockquote { blocks }),
        ..Block::default()
    }
}

// ── lists ───────────────────────────────────────────────────────────────────

/// A parsed list-item marker: ordered flag, nesting level from indent, and the
/// content after the marker.
struct Marker {
    ordered: bool,
    level: u8,
    rest_start: usize,
}

/// Detect a list-item marker at the start of `line`, returning the kind, the
/// indent-derived level, and the byte offset where the item content begins.
fn list_item_marker(line: &str) -> Option<Marker> {
    let indent = line.len() - line.trim_start().len();
    let level = (indent / 2) as u8; // 2 spaces per nesting level
    let t = line.trim_start();
    let mut chars = t.char_indices();
    let (_, first) = chars.next()?;

    // Unordered: `-`, `*` or `+` followed by a space.
    if matches!(first, '-' | '*' | '+') {
        if let Some((_, ' ')) = chars.next() {
            return Some(Marker {
                ordered: false,
                level,
                rest_start: indent + 2,
            });
        }
        return None;
    }

    // Ordered: one or more digits, then `.` or `)`, then a space.
    if first.is_ascii_digit() {
        let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
        let after = &t[digits..];
        let mut ac = after.chars();
        if matches!(ac.next(), Some('.') | Some(')')) && matches!(ac.next(), Some(' ')) {
            return Some(Marker {
                ordered: true,
                level,
                rest_start: indent + digits + 2,
            });
        }
    }
    None
}

/// Parse a contiguous list starting at `lines[0]`; returns the [`List`] block and
/// the number of source lines consumed. The list's ordered-ness is taken from
/// its first item; continuation lines (indented, no marker) join the prior item.
fn parse_list(lines: &[&str], defs: &Defs) -> (Block, usize) {
    let first = list_item_marker(lines[0]).expect("called on a list line");
    let ordered = first.ordered;
    let mut items: Vec<ListItem> = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        if line.trim().is_empty() {
            // A blank line ends the list unless the next line continues it.
            if lines
                .get(i + 1)
                .is_some_and(|n| list_item_marker(n).is_some())
            {
                i += 1;
                continue;
            }
            break;
        }
        let Some(m) = list_item_marker(line) else {
            // Indented continuation of the current item.
            if !items.is_empty() && (line.len() - line.trim_start().len()) >= 2 {
                append_item_text(items.last_mut().unwrap(), line.trim());
                i += 1;
                continue;
            }
            break;
        };
        if m.ordered != ordered {
            break; // a different list kind starts a new list
        }
        let content = line[m.rest_start.min(line.len())..].trim();
        items.push(ListItem {
            blocks: vec![paragraph_block(list_item_runs(content, defs))],
            level: m.level,
        });
        i += 1;
    }

    let marker = if ordered {
        ListMarker::Decimal
    } else {
        ListMarker::Bullet('•')
    };
    let block = Block {
        kind: BlockKind::List(List {
            ordered,
            marker,
            items,
        
        ..Default::default()
}),
        ..Block::default()
    };
    (block, i)
}

/// Build the inline runs of a list item, honouring a leading GFM task-list
/// marker `[ ]`/`[x]` (rendered as a `☐`/`☑` glyph since the model has no
/// checkbox slot on a [`ListItem`]). Plain items parse their content as inlines.
fn list_item_runs(content: &str, defs: &Defs) -> Vec<Inline> {
    if let Some((checked, rest)) = task_marker(content) {
        let glyph = if checked {
            TASK_CHECKED
        } else {
            TASK_UNCHECKED
        };
        let mut runs = vec![Inline::Run(InlineRun {
            text: format!("{glyph} "),
            style: body_style(),
            source_index: None,
        })];
        inline_into(rest, body_style(), defs, &mut runs);
        return runs;
    }
    parse_inlines(content, defs)
}

/// Detect a GFM task-list marker at the start of an item's content: `[ ]`, `[x]`
/// or `[X]` followed by a space (or end). Returns the checked flag and the
/// remaining content. `None` when the item is not a task-list item.
fn task_marker(content: &str) -> Option<(bool, &str)> {
    let rest = content.strip_prefix('[')?;
    let mut marks = rest.chars();
    let mark = marks.next()?;
    let checked = match mark {
        ' ' => false,
        'x' | 'X' => true,
        _ => return None,
    };
    let after = rest.get(1..)?.strip_prefix(']')?;
    // A real task marker is followed by a space or is the whole item.
    if after.is_empty() {
        Some((checked, ""))
    } else {
        Some((checked, after.strip_prefix(' ')?))
    }
}

/// Append `text` (a soft-wrapped continuation line) to a list item's first
/// paragraph, separated by a space.
fn append_item_text(item: &mut ListItem, text: &str) {
    if let Some(Block {
        kind: BlockKind::Paragraph(p),
        ..
    }) = item.blocks.first_mut()
    {
        p.runs.push(Inline::Run(InlineRun {
            text: format!(" {text}"),
            style: body_style(),
            source_index: None,
        }));
    }
}

// ── tables (GFM pipe syntax) ─────────────────────────────────────────────────

/// Parse a GFM pipe table starting at `lines[0]` (header), `lines[1]` (delimiter),
/// then body rows until a blank/non-table line. Returns the [`Table`] block and
/// lines consumed. The header row is rendered bold + shaded.
fn parse_table(lines: &[&str], defs: &Defs) -> (Block, usize) {
    let header = split_table_row(lines[0]);
    let ncols = header.len().max(1);
    let mut rows: Vec<Row> = Vec::new();

    rows.push(make_row(&header, ncols, true, defs));
    let mut consumed = 2; // header + delimiter
    let mut i = 2;
    while i < lines.len() {
        let l = lines[i];
        if l.trim().is_empty() || !looks_like_table_row(l) {
            break;
        }
        let cells = split_table_row(l);
        rows.push(make_row(&cells, ncols, false, defs));
        consumed += 1;
        i += 1;
    }

    let block = Block {
        kind: BlockKind::Table(Table {
            rows,
            col_widths: Vec::new(),
            ..Table::default()
        }),
        ..Block::default()
    };
    (block, consumed)
}

/// Build a table [`Row`] from cell texts, padded/truncated to `ncols`. Header
/// rows get bold runs and light shading.
fn make_row(cells: &[String], ncols: usize, header: bool, defs: &Defs) -> Row {
    let mut out = Vec::with_capacity(ncols);
    for c in 0..ncols {
        let text = cells.get(c).map(String::as_str).unwrap_or("");
        let mut runs = parse_inlines(text, defs);
        if header {
            for inline in &mut runs {
                if let Inline::Run(r) = inline {
                    r.style.bold = true;
                }
            }
        }
        out.push(Cell {
            blocks: vec![paragraph_block(runs)],
            shading: header.then_some([0.93, 0.93, 0.93]),
            ..Cell::default()
        });
    }
    Row {
        cells: out,
        height: None,
        // A GFM header row maps directly to the model header flag.
        is_header: header,
    }
}

/// Split a pipe-delimited table row into trimmed cell strings, tolerating the
/// optional leading/trailing `|`.
fn split_table_row(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    t.split('|').map(|c| c.trim().to_string()).collect()
}

/// A line that, after trimming, contains a `|` and isn't a fence/heading — a
/// candidate table row.
fn looks_like_table_row(line: &str) -> bool {
    let t = line.trim();
    t.contains('|') && !t.starts_with("```") && atx_heading(t).is_none()
}

/// A GFM delimiter row: only `|`, `-`, `:` and spaces, and at least one `-`.
fn is_table_delimiter(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty() && t.contains('-') && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

// ── line classifiers ────────────────────────────────────────────────────────

/// ATX heading: count leading `#` (1..=6), require a following space or EOL,
/// strip an optional trailing `#` run; returns `(level, text)`.
fn atx_heading(t: &str) -> Option<(u8, String)> {
    let hashes = t.chars().take_while(|&c| c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &t[hashes..];
    if !rest.is_empty() && !rest.starts_with(' ') {
        return None; // `#foo` is not a heading
    }
    let text = rest.trim().trim_end_matches('#').trim_end();
    Some((hashes as u8, text.to_string()))
}

/// Thematic break: ≥3 of a single marker char (`-`, `*`, `_`), spaces allowed.
fn is_thematic_break(t: &str) -> bool {
    for marker in ['-', '*', '_'] {
        let count = t.chars().filter(|&c| c == marker).count();
        let only = t.chars().all(|c| c == marker || c == ' ');
        if only && count >= 3 {
            return true;
        }
    }
    false
}

/// Code fence: returns `(fence_char, run_len)` for a line of ≥3 `` ` `` or `~`
/// (an info string after the fence is ignored).
fn code_fence(t: &str) -> Option<(char, usize)> {
    for fence in ['`', '~'] {
        let run = t.chars().take_while(|&c| c == fence).count();
        if run >= 3 {
            return Some((fence, run));
        }
    }
    None
}

/// Block-quote prefix: strip a leading `>` (and one optional following space),
/// returning the remaining text of the line. `None` if the line isn't a quote.
fn quote_prefix(line: &str) -> Option<&str> {
    let t = line.trim_start();
    let rest = t.strip_prefix('>')?;
    Some(rest.strip_prefix(' ').unwrap_or(rest))
}

// ── inline parsing ───────────────────────────────────────────────────────────

/// Parse inline Markdown in `text` into a run of [`Inline`]s, honouring
/// `**`/`__` (bold), `*`/`_` (italic), `~~` (strike), `` ` `` (code), images
/// `![alt](url)`, inline/reference/footnote links, inline HTML and character
/// references. Unmatched markers are treated as literal characters.
fn parse_inlines(text: &str, defs: &Defs) -> Vec<Inline> {
    let mut runs = Vec::new();
    inline_into(text, body_style(), defs, &mut runs);
    if runs.is_empty() {
        runs.push(Inline::Run(InlineRun {
            text: String::new(),
            style: body_style(),
            source_index: None,
        }));
    }
    runs
}

/// Push a buffered run (if non-empty) under `style`, clearing the buffer.
fn flush_run(buf: &mut String, out: &mut Vec<Inline>, style: &CharStyle) {
    if !buf.is_empty() {
        out.push(Inline::Run(InlineRun {
            text: std::mem::take(buf),
            style: style.clone(),
            source_index: None,
        }));
    }
}

/// Tokenize `text` under the active `style`, pushing runs/links/images into
/// `out`. Emphasis/strike recurse with the relevant flag set; code spans switch
/// to the monospace style; references resolve against `defs`.
fn inline_into(text: &str, style: CharStyle, defs: &Defs, out: &mut Vec<Inline>) {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let mut buf = String::new();

    while i < chars.len() {
        let c = chars[i];

        // Backslash escape: the next char is literal (CommonMark precedence).
        if c == '\\' && i + 1 < chars.len() {
            buf.push(chars[i + 1]);
            i += 2;
            continue;
        }

        // Inline code: `…` (verbatim, no nested markup) — wins over the markers.
        if c == '`' {
            if let Some(end) = find_char(&chars, i + 1, '`') {
                flush_run(&mut buf, out, &style);
                let code: String = chars[i + 1..end].iter().collect();
                out.push(Inline::Run(InlineRun {
                    text: code,
                    style: mono_style(),
                    source_index: None,
                }));
                i = end + 1;
                continue;
            }
        }

        // Image: `![alt](url "title")` (checked before `[` so the `!` binds).
        if c == '!' && chars.get(i + 1) == Some(&'[') {
            if let Some((image, next)) = try_image(&chars, i) {
                flush_run(&mut buf, out, &style);
                out.push(image);
                i = next;
                continue;
            }
        }

        // Strong emphasis: `**` or `__`.
        if (c == '*' || c == '_') && chars.get(i + 1) == Some(&c) {
            let delim = [c, c];
            if let Some(end) = find_seq(&chars, i + 2, &delim) {
                flush_run(&mut buf, out, &style);
                let inner: String = chars[i + 2..end].iter().collect();
                let mut s = style.clone();
                s.bold = true;
                inline_into(&inner, s, defs, out);
                i = end + 2;
                continue;
            }
        }

        // Strikethrough: `~~…~~` (GFM).
        if c == '~' && chars.get(i + 1) == Some(&'~') {
            if let Some(end) = find_seq(&chars, i + 2, &['~', '~']) {
                flush_run(&mut buf, out, &style);
                let inner: String = chars[i + 2..end].iter().collect();
                let mut s = style.clone();
                s.strike = true;
                inline_into(&inner, s, defs, out);
                i = end + 2;
                continue;
            }
        }

        // Emphasis: `*` or `_`.
        if c == '*' || c == '_' {
            if let Some(end) = find_char(&chars, i + 1, c) {
                if end > i + 1 {
                    flush_run(&mut buf, out, &style);
                    let inner: String = chars[i + 1..end].iter().collect();
                    let mut s = style.clone();
                    s.italic = true;
                    inline_into(&inner, s, defs, out);
                    i = end + 1;
                    continue;
                }
            }
        }

        // Link (inline `[t](url)`, reference `[t][ref]`/`[t][]`/`[t]`, or
        // footnote `[^id]`).
        if c == '[' {
            if let Some((link, next)) = try_link(&chars, i, style.clone(), defs) {
                flush_run(&mut buf, out, &style);
                out.push(link);
                i = next;
                continue;
            }
        }

        // Inline HTML: a recognized phrasing tag (`<b>`, `<a href>`, `<br>`…) or
        // a character reference (`&amp;`). Unknown tags degrade to nothing.
        if c == '<' {
            if let Some(next) = try_inline_html(&chars, i, &style, defs, &mut buf, out) {
                i = next;
                continue;
            }
        }
        if c == '&' {
            if let Some((decoded, next)) = try_entity(&chars, i) {
                buf.push_str(&decoded);
                i = next;
                continue;
            }
        }

        buf.push(c);
        i += 1;
    }
    flush_run(&mut buf, out, &style);
}

/// Try to parse an image `![alt](url "title")` starting at `start` (a `!`).
/// Returns the [`Inline::Image`] and the index just past the closing `)`.
fn try_image(chars: &[char], start: usize) -> Option<(Inline, usize)> {
    let close = find_char(chars, start + 2, ']')?;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let paren = find_char(chars, close + 2, ')')?;
    let alt: String = chars[start + 2..close].iter().collect();
    let dest: String = chars[close + 2..paren].iter().collect();
    let url = link_destination(&dest);
    let alt = alt.trim();
    Some((
        Inline::Image(ImageRef {
            resource: fnv1a(url.as_bytes()),
            alt: (!alt.is_empty()).then(|| alt.to_string()),
        }),
        paren + 1,
    ))
}

/// Try to parse a link starting at `start` (a `[`): inline `[t](url)`, reference
/// `[t][ref]`/`[t][]`/shortcut `[t]`, or a footnote `[^id]`. Returns the
/// [`Inline`] and the index just past the link. `None` if `start` is not a link.
fn try_link(
    chars: &[char],
    start: usize,
    style: CharStyle,
    defs: &Defs,
) -> Option<(Inline, usize)> {
    // Footnote reference `[^id]` → resolve to its definition text (or the bare
    // marker when undefined), rendered as a link-less run.
    if chars.get(start + 1) == Some(&'^') {
        let close = find_char(chars, start + 2, ']')?;
        let id: String = chars[start + 2..close].iter().collect();
        if id.is_empty() {
            return None;
        }
        let note = defs
            .footnotes
            .get(&id)
            .cloned()
            .unwrap_or_else(|| format!("[^{id}]"));
        let mut children = Vec::new();
        inline_into(&note, style, defs, &mut children);
        return Some((
            Inline::Link {
                href: LinkTarget::Url(format!("#fn-{id}")),
                children,
            },
            close + 1,
        ));
    }

    let close = find_char(chars, start + 1, ']')?;
    let label: String = chars[start + 1..close].iter().collect();

    // Inline link `[text](url)`.
    if chars.get(close + 1) == Some(&'(') {
        let paren = find_char(chars, close + 2, ')')?;
        let dest: String = chars[close + 2..paren].iter().collect();
        let mut children = Vec::new();
        inline_into(&label, style, defs, &mut children);
        return Some((
            Inline::Link {
                href: LinkTarget::Url(link_destination(&dest)),
                children,
            },
            paren + 1,
        ));
    }

    // Reference link `[text][ref]` / collapsed `[text][]`.
    if chars.get(close + 1) == Some(&'[') {
        let close2 = find_char(chars, close + 2, ']')?;
        let refid: String = chars[close + 2..close2].iter().collect();
        let key = if refid.trim().is_empty() {
            &label
        } else {
            &refid
        };
        let (url, _title) = defs.links.get(&normalize_ref(key))?;
        let mut children = Vec::new();
        inline_into(&label, style, defs, &mut children);
        return Some((
            Inline::Link {
                href: LinkTarget::Url(url.clone()),
                children,
            },
            close2 + 1,
        ));
    }

    // Shortcut reference `[text]` (only when `text` names a definition).
    if let Some((url, _title)) = defs.links.get(&normalize_ref(&label)) {
        let mut children = Vec::new();
        inline_into(&label, style, defs, &mut children);
        return Some((
            Inline::Link {
                href: LinkTarget::Url(url.clone()),
                children,
            },
            close + 1,
        ));
    }
    None
}

/// Extract the URL from a link/image destination `(...)` body: the first
/// whitespace-delimited token (dropping an optional `"title"`), with `<...>`
/// angle brackets stripped.
fn link_destination(dest: &str) -> String {
    let token = dest.split_whitespace().next().unwrap_or("").trim();
    token
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
        .unwrap_or(token)
        .to_string()
}

/// Try to parse a character reference `&name;`/`&#nnn;` at `start`. Returns the
/// decoded text and the index just past the `;`. `None` if it is not a valid,
/// decodable entity (so a bare `&` stays literal).
fn try_entity(chars: &[char], start: usize) -> Option<(String, usize)> {
    // A reference is short; scan a bounded window for the terminating `;`.
    let end = (start + 1..chars.len().min(start + 33)).find(|&j| chars[j] == ';')?;
    let raw: String = chars[start..=end].iter().collect();
    let decoded = crate::html::dom::decode_entities(&raw);
    // Only consume it if it actually decoded to something other than itself.
    if decoded == raw {
        return None;
    }
    Some((decoded, end + 1))
}

/// Try to handle an inline HTML construct at `start` (a `<`): a recognized
/// phrasing tag toggling style / opening a link, `<br>` ⇒ a line break, or an
/// unknown tag (dropped, content kept). Comments and bare `<` stay literal.
/// Returns the index just past the consumed construct, or `None` to treat `<`
/// literally.
fn try_inline_html(
    chars: &[char],
    start: usize,
    style: &CharStyle,
    defs: &Defs,
    buf: &mut String,
    out: &mut Vec<Inline>,
) -> Option<usize> {
    let gt = find_char(chars, start + 1, '>')?;
    let raw: String = chars[start + 1..gt].iter().collect();
    let tag = raw.trim();
    if tag.is_empty() {
        return None; // an empty `<>` — not a tag
    }

    // Self-contained void tags.
    let lower = tag.to_ascii_lowercase();
    if lower == "br" || lower == "br/" || lower.starts_with("br ") || lower.starts_with("br/") {
        flush_run(buf, out, style);
        out.push(Inline::LineBreak);
        return Some(gt + 1);
    }

    // A stray closing tag (no matching open at this level) — drop it, keep going.
    if lower.starts_with('/') {
        return Some(gt + 1);
    }

    // An anchor `<a href="url">…</a>` → a link over its (HTML-parsed) content.
    if lower == "a" || lower.starts_with("a ") {
        let href = tag_attr(tag, "href").unwrap_or_default();
        let (inner, next) = html_element_inner(chars, gt + 1, "a");
        flush_run(buf, out, style);
        let mut children = Vec::new();
        inline_into(&inner, style.clone(), defs, &mut children);
        out.push(Inline::Link {
            href: LinkTarget::Url(href),
            children,
        });
        return Some(next);
    }

    // A phrasing tag that maps to a style toggle (`b`, `strong`, `i`, `em`,
    // `code`, `u`, `s`, `del`, `strike`, `mark`).
    let name = lower.split_whitespace().next().unwrap_or("");
    if let Some(apply) = inline_tag_style(name, style) {
        let (inner, next) = html_element_inner(chars, gt + 1, name);
        flush_run(buf, out, style);
        if name == "code" {
            // Code maps to the monospace run style, verbatim text kept.
            out.push(Inline::Run(InlineRun {
                text: crate::html::dom::decode_entities(&inner),
                style: mono_style(),
                source_index: None,
            }));
        } else {
            inline_into(&inner, apply, defs, out);
        }
        return Some(next);
    }

    // Unknown tag: drop the tag itself, keep parsing the following text.
    Some(gt + 1)
}

/// The [`CharStyle`] for a recognized phrasing tag (`name` lower-cased, no
/// brackets), derived from `base`. `None` for tags with no styling meaning.
fn inline_tag_style(name: &str, base: &CharStyle) -> Option<CharStyle> {
    let mut s = base.clone();
    match name {
        "b" | "strong" => s.bold = true,
        "i" | "em" => s.italic = true,
        "u" | "ins" => s.underline = true,
        "s" | "del" | "strike" => s.strike = true,
        "code" => return Some(mono_style()),
        "mark" => s.background = Some([1.0, 1.0, 0.0]),
        _ => return None,
    }
    Some(s)
}

/// Collect the raw inner text of an HTML element whose open tag ended at `from`,
/// up to its matching `</name>` (case-insensitive); returns the inner text and
/// the index just past the close tag (or end-of-input when unterminated).
fn html_element_inner(chars: &[char], from: usize, name: &str) -> (String, usize) {
    let close = format!("</{name}");
    let close_chars: Vec<char> = close.chars().collect();
    let mut j = from;
    while j < chars.len() {
        if matches_ci(chars, j, &close_chars) {
            let inner: String = chars[from..j].iter().collect();
            // Skip to just past the closing `>`.
            let end = find_char(chars, j + close_chars.len(), '>').map_or(chars.len(), |g| g + 1);
            return (inner, end);
        }
        j += 1;
    }
    (chars[from..].iter().collect(), chars.len())
}

/// Whether `needle` matches `chars` at `at`, ASCII-case-insensitively.
fn matches_ci(chars: &[char], at: usize, needle: &[char]) -> bool {
    if at + needle.len() > chars.len() {
        return false;
    }
    chars[at..at + needle.len()]
        .iter()
        .zip(needle)
        .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

/// Read the value of attribute `attr` from an HTML start-tag body `tag` (the
/// text between `<` and `>`). Handles `name="v"`, `name='v'` and bare `name=v`.
fn tag_attr(tag: &str, attr: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let pat = format!("{attr}=");
    let pos = lower.find(&pat)? + pat.len();
    let rest = &tag[pos..];
    let value = match rest.chars().next() {
        Some('"') => rest[1..].split('"').next().unwrap_or(""),
        Some('\'') => rest[1..].split('\'').next().unwrap_or(""),
        _ => rest.split_whitespace().next().unwrap_or(""),
    };
    Some(crate::html::dom::decode_entities(value.trim()))
}

/// 64-bit FNV-1a hash — a stable, dependency-free resource key for an image
/// `src` (mirrors the HTML importer, which keys image resources by their URL).
fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

/// First index `>= from` of `needle` in `chars`, or `None`.
fn find_char(chars: &[char], from: usize, needle: char) -> Option<usize> {
    (from..chars.len()).find(|&j| chars[j] == needle)
}

/// First index `>= from` where the two-char `seq` begins in `chars`, or `None`.
fn find_seq(chars: &[char], from: usize, seq: &[char; 2]) -> Option<usize> {
    (from..chars.len().saturating_sub(1)).find(|&j| chars[j] == seq[0] && chars[j + 1] == seq[1])
}

// ── styles ───────────────────────────────────────────────────────────────────

/// Default body run style (sans, 11pt).
fn body_style() -> CharStyle {
    CharStyle {
        generic: Generic::Sans,
        size_pt: BODY_PT,
        ..CharStyle::default()
    }
}

/// Monospace run style for code (10pt).
fn mono_style() -> CharStyle {
    CharStyle {
        generic: Generic::Mono,
        size_pt: CODE_PT,
        ..CharStyle::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Top-level blocks of the single page.
    fn blocks(doc: &Document) -> &[Block] {
        &doc.sections[0].pages[0].blocks
    }

    /// Concatenate a paragraph's run text (flattening links), trimmed.
    fn para_text(p: &Paragraph) -> String {
        let mut s = String::new();
        collect(&p.runs, &mut s);
        s.trim().to_string()
    }

    fn collect(runs: &[Inline], out: &mut String) {
        for inline in runs {
            match inline {
                Inline::Run(r) => out.push_str(&r.text),
                Inline::Link { children, .. } => collect(children, out),
                Inline::LineBreak => out.push('\n'),
                Inline::Image(_) => {}
                Inline::CommentRef { .. } => {}
            }
        }
    }

    #[test]
    fn headings_levels_and_sizing() {
        let doc = md_to_model("# One\n## Two\n###### Six\n#NotAHeading");
        let b = blocks(&doc);
        match &b[0].kind {
            BlockKind::Heading(h) => {
                assert_eq!(h.level, 1);
                assert_eq!(para_text(&h.para), "One");
                if let Inline::Run(r) = &h.para.runs[0] {
                    assert_eq!(r.style.size_pt, 24.0);
                    assert!(r.style.bold);
                } else {
                    panic!("expected a run");
                }
            }
            other => panic!("expected heading, got {other:?}"),
        }
        assert!(matches!(&b[1].kind, BlockKind::Heading(h) if h.level == 2));
        assert!(matches!(&b[2].kind, BlockKind::Heading(h) if h.level == 6));
        // `#NotAHeading` (no space) is a plain paragraph.
        assert!(
            matches!(&b[3].kind, BlockKind::Paragraph(p) if para_text(p) == "#NotAHeading"),
            "got {:?}",
            b[3].kind
        );
    }

    #[test]
    fn paragraphs_join_soft_wraps_and_split_on_blank() {
        let doc = md_to_model("first line\nstill first\n\nsecond para");
        let b = blocks(&doc);
        assert_eq!(b.len(), 2);
        match &b[0].kind {
            BlockKind::Paragraph(p) => assert_eq!(para_text(p), "first line still first"),
            other => panic!("expected paragraph, got {other:?}"),
        }
        match &b[1].kind {
            BlockKind::Paragraph(p) => assert_eq!(para_text(p), "second para"),
            other => panic!("expected paragraph, got {other:?}"),
        }
    }

    #[test]
    fn inline_bold_italic_code() {
        let doc = md_to_model("a **bold** and *em* and `code` end");
        let p = match &blocks(&doc)[0].kind {
            BlockKind::Paragraph(p) => p,
            other => panic!("expected paragraph, got {other:?}"),
        };
        let has = |pred: fn(&InlineRun) -> bool| {
            p.runs
                .iter()
                .any(|i| matches!(i, Inline::Run(r) if pred(r)))
        };
        assert!(has(|r| r.style.bold && r.text == "bold"), "bold run");
        assert!(has(|r| r.style.italic && r.text == "em"), "italic run");
        assert!(
            has(|r| matches!(r.style.generic, Generic::Mono) && r.text == "code"),
            "code run"
        );
        // Underscore emphasis too.
        let doc2 = md_to_model("x __strong__ y _slant_ z");
        let p2 = match &blocks(&doc2)[0].kind {
            BlockKind::Paragraph(p) => p,
            _ => panic!(),
        };
        assert!(p2
            .runs
            .iter()
            .any(|i| matches!(i, Inline::Run(r) if r.style.bold && r.text == "strong")));
        assert!(p2
            .runs
            .iter()
            .any(|i| matches!(i, Inline::Run(r) if r.style.italic && r.text == "slant")));
    }

    #[test]
    fn links_become_link_inlines() {
        let doc = md_to_model("see [the site](https://example.com) now");
        let p = match &blocks(&doc)[0].kind {
            BlockKind::Paragraph(p) => p,
            _ => panic!(),
        };
        let link = p
            .runs
            .iter()
            .find_map(|i| match i {
                Inline::Link { href, children } => Some((href, children)),
                _ => None,
            })
            .expect("a link inline");
        assert_eq!(link.0, &LinkTarget::Url("https://example.com".into()));
        let mut s = String::new();
        collect(link.1, &mut s);
        assert_eq!(s, "the site");
    }

    #[test]
    fn unordered_and_ordered_lists() {
        let doc = md_to_model("- a\n- b\n- c");
        match &blocks(&doc)[0].kind {
            BlockKind::List(l) => {
                assert!(!l.ordered);
                assert_eq!(l.items.len(), 3);
                let item0 = match &l.items[0].blocks[0].kind {
                    BlockKind::Paragraph(p) => para_text(p),
                    _ => panic!(),
                };
                assert_eq!(item0, "a");
            }
            other => panic!("expected list, got {other:?}"),
        }
        let doc2 = md_to_model("1. one\n2. two");
        match &blocks(&doc2)[0].kind {
            BlockKind::List(l) => {
                assert!(l.ordered);
                assert_eq!(l.marker, ListMarker::Decimal);
                assert_eq!(l.items.len(), 2);
            }
            other => panic!("expected ordered list, got {other:?}"),
        }
    }

    #[test]
    fn nested_list_levels_from_indent() {
        let doc = md_to_model("- top\n  - child\n  - child2\n- top2");
        match &blocks(&doc)[0].kind {
            BlockKind::List(l) => {
                assert_eq!(l.items.len(), 4);
                assert_eq!(l.items[0].level, 0);
                assert_eq!(l.items[1].level, 1, "2-space indent ⇒ level 1");
                assert_eq!(l.items[3].level, 0);
            }
            other => panic!("expected list, got {other:?}"),
        }
    }

    #[test]
    fn fenced_code_block_is_a_code_block_verbatim() {
        let doc = md_to_model("```rust\nlet x = 1;\n**not bold**\n```");
        let cb = match &blocks(&doc)[0].kind {
            BlockKind::CodeBlock(cb) => cb,
            other => panic!("expected code block, got {other:?}"),
        };
        // The info-string language is captured.
        assert_eq!(cb.lang.as_deref(), Some("rust"));
        // Content is verbatim — markup inside is NOT interpreted.
        assert_eq!(cb.code, "let x = 1;\n**not bold**");
    }

    #[test]
    fn fenced_code_block_without_lang_has_none() {
        let doc = md_to_model("```\nplain code\n```");
        match &blocks(&doc)[0].kind {
            BlockKind::CodeBlock(cb) => {
                assert!(cb.lang.is_none());
                assert_eq!(cb.code, "plain code");
            }
            other => panic!("expected code block, got {other:?}"),
        }
    }

    #[test]
    fn thematic_break_is_a_horizontal_rule() {
        let doc = md_to_model("above\n\n---\n\nbelow");
        let b = blocks(&doc);
        assert!(
            b.iter()
                .any(|blk| matches!(&blk.kind, BlockKind::HorizontalRule)),
            "a horizontal rule exists"
        );
        // `***` and `___` are thematic breaks too.
        for src in ["***", "___", "- - -"] {
            let d = md_to_model(src);
            assert!(
                matches!(&blocks(&d)[0].kind, BlockKind::HorizontalRule),
                "`{src}` is a rule"
            );
        }
    }

    #[test]
    fn gfm_pipe_table() {
        let md = "| Name | Age |\n| --- | --- |\n| Alice | 30 |\n| Bob | 25 |";
        let doc = md_to_model(md);
        match &blocks(&doc)[0].kind {
            BlockKind::Table(t) => {
                assert_eq!(t.rows.len(), 3, "header + 2 body rows");
                assert_eq!(t.rows[0].cells.len(), 2);
                // Header cells are bold + shaded.
                assert!(t.rows[0].cells[0].shading.is_some());
                let h0 = match &t.rows[0].cells[0].blocks[0].kind {
                    BlockKind::Paragraph(p) => p,
                    _ => panic!(),
                };
                assert!(matches!(&h0.runs[0], Inline::Run(r) if r.style.bold));
                assert_eq!(para_text(h0), "Name");
                // A body cell.
                let body = match &t.rows[2].cells[1].blocks[0].kind {
                    BlockKind::Paragraph(p) => para_text(p),
                    _ => panic!(),
                };
                assert_eq!(body, "25");
            }
            other => panic!("expected table, got {other:?}"),
        }
    }

    #[test]
    fn block_quote_is_a_blockquote() {
        let doc = md_to_model("> quoted line\n> still quoted");
        match &blocks(&doc)[0].kind {
            BlockKind::Blockquote(bq) => {
                assert_eq!(bq.blocks.len(), 1, "the two lines join into one paragraph");
                match &bq.blocks[0].kind {
                    BlockKind::Paragraph(p) => {
                        assert_eq!(para_text(p), "quoted line still quoted")
                    }
                    other => panic!("expected paragraph inside quote, got {other:?}"),
                }
            }
            other => panic!("expected blockquote, got {other:?}"),
        }
    }

    #[test]
    fn nested_block_quote_round_trips_constructs() {
        // A quote containing a heading and a code block keeps both as semantic
        // children (not flattened to indented text).
        let doc = md_to_model("> # Quoted Title\n>\n> ```\n> code()\n> ```");
        let bq = match &blocks(&doc)[0].kind {
            BlockKind::Blockquote(bq) => bq,
            other => panic!("expected blockquote, got {other:?}"),
        };
        assert!(bq
            .blocks
            .iter()
            .any(|b| matches!(&b.kind, BlockKind::Heading(_))));
        assert!(bq
            .blocks
            .iter()
            .any(|b| matches!(&b.kind, BlockKind::CodeBlock(_))));
    }

    #[test]
    fn empty_input_yields_empty_page() {
        let doc = md_to_model("");
        assert!(blocks(&doc).is_empty());
    }

    #[test]
    fn realistic_document_block_kinds() {
        let md = "# Title\n\nIntro paragraph with **bold** and a [link](http://x).\n\n## Items\n\n- first\n- second\n\n```\ncode();\n```\n\n---\n\n> quoted\n\n| A | B |\n| --- | --- |\n| 1 | 2 |\n";
        let doc = md_to_model(md);
        let kinds: Vec<&str> = blocks(&doc)
            .iter()
            .map(|b| match &b.kind {
                BlockKind::Heading(_) => "heading",
                BlockKind::Paragraph(_) => "paragraph",
                BlockKind::List(_) => "list",
                BlockKind::Table(_) => "table",
                BlockKind::CodeBlock(_) => "code",
                BlockKind::Blockquote(_) => "quote",
                BlockKind::HorizontalRule => "rule",
                BlockKind::Shape(_) => "shape",
                _ => "other",
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "heading", "paragraph", "heading", "list", "code", "rule", "quote", "table"
            ]
        );
    }

    /// The first paragraph of the single page.
    fn first_para(doc: &Document) -> &Paragraph {
        match &blocks(doc)[0].kind {
            BlockKind::Paragraph(p) => p,
            other => panic!("expected paragraph, got {other:?}"),
        }
    }

    #[test]
    fn strikethrough_is_a_strike_run() {
        let doc = md_to_model("a ~~gone~~ b");
        let p = first_para(&doc);
        assert!(
            p.runs
                .iter()
                .any(|i| matches!(i, Inline::Run(r) if r.style.strike && r.text == "gone")),
            "a struck run `gone`, got {:?}",
            p.runs
        );
        // A lone `~` is literal (not a strike opener).
        let doc2 = md_to_model("1 ~ 2");
        assert_eq!(para_text(first_para(&doc2)), "1 ~ 2");
    }

    #[test]
    fn inline_image_becomes_image_inline() {
        let doc = md_to_model("see ![the logo](logo.png) here");
        let p = first_para(&doc);
        let img = p
            .runs
            .iter()
            .find_map(|i| match i {
                Inline::Image(img) => Some(img),
                _ => None,
            })
            .expect("an image inline");
        assert_eq!(img.alt.as_deref(), Some("the logo"));
        // The resource key is the URL hash (mirrors the HTML importer).
        assert_eq!(img.resource, fnv1a("logo.png".as_bytes()));
        // An external URL with a title still keeps the reference + alt.
        let doc2 = md_to_model("![pic](https://ex.com/p.png \"Title\")");
        let img2 = first_para(&doc2)
            .runs
            .iter()
            .find_map(|i| match i {
                Inline::Image(img) => Some(img),
                _ => None,
            })
            .expect("an image inline");
        assert_eq!(img2.alt.as_deref(), Some("pic"));
        assert_eq!(img2.resource, fnv1a("https://ex.com/p.png".as_bytes()));
    }

    #[test]
    fn task_list_items_get_checkbox_glyphs() {
        let doc = md_to_model("- [ ] todo\n- [x] done\n- plain");
        let l = match &blocks(&doc)[0].kind {
            BlockKind::List(l) => l,
            other => panic!("expected list, got {other:?}"),
        };
        assert_eq!(l.items.len(), 3);
        let item = |n: usize| match &l.items[n].blocks[0].kind {
            BlockKind::Paragraph(p) => para_text(p),
            _ => panic!(),
        };
        // Unchecked ⇒ ☐, checked ⇒ ☑ (the model has no checkbox slot).
        assert_eq!(item(0), "\u{2610} todo");
        assert_eq!(item(1), "\u{2611} done");
        assert_eq!(item(2), "plain", "a non-task item is untouched");
    }

    #[test]
    fn reference_link_resolves_against_definition() {
        let doc = md_to_model("see [the text][ref] now\n\n[ref]: https://example.com \"Title\"");
        let p = first_para(&doc);
        let link = p
            .runs
            .iter()
            .find_map(|i| match i {
                Inline::Link { href, children } => Some((href, children)),
                _ => None,
            })
            .expect("a resolved reference link");
        assert_eq!(link.0, &LinkTarget::Url("https://example.com".into()));
        let mut s = String::new();
        collect(link.1, &mut s);
        assert_eq!(s, "the text");
        // The definition line itself produces no rendered block.
        assert_eq!(blocks(&doc).len(), 1);
    }

    #[test]
    fn collapsed_and_shortcut_references_resolve() {
        // Collapsed `[label][]` and shortcut `[label]` both use the label as ref.
        let doc = md_to_model("[Rust][] and [Rust] rule\n\n[rust]: https://rust-lang.org");
        let p = first_para(&doc);
        let links: Vec<&LinkTarget> = p
            .runs
            .iter()
            .filter_map(|i| match i {
                Inline::Link { href, .. } => Some(href),
                _ => None,
            })
            .collect();
        assert_eq!(links.len(), 2, "both references resolved, got {links:?}");
        assert!(links
            .iter()
            .all(|h| **h == LinkTarget::Url("https://rust-lang.org".into())));
        // An undefined shortcut stays literal text.
        let doc2 = md_to_model("[undefined] here");
        assert_eq!(para_text(first_para(&doc2)), "[undefined] here");
    }

    #[test]
    fn footnote_reference_resolves_to_note_text() {
        let doc = md_to_model("text[^1] more\n\n[^1]: the footnote body");
        let p = first_para(&doc);
        let link = p
            .runs
            .iter()
            .find_map(|i| match i {
                Inline::Link { href, children } => Some((href, children)),
                _ => None,
            })
            .expect("a footnote link");
        assert_eq!(link.0, &LinkTarget::Url("#fn-1".into()));
        let mut s = String::new();
        collect(link.1, &mut s);
        assert_eq!(s, "the footnote body");
        // The footnote definition produces no rendered block.
        assert_eq!(blocks(&doc).len(), 1);
    }

    #[test]
    fn setext_headings_h1_and_h2() {
        let doc = md_to_model("Title One\n===\n\nTitle Two\n---");
        let b = blocks(&doc);
        match &b[0].kind {
            BlockKind::Heading(h) => {
                assert_eq!(h.level, 1);
                assert_eq!(para_text(&h.para), "Title One");
            }
            other => panic!("expected setext h1, got {other:?}"),
        }
        match &b[1].kind {
            BlockKind::Heading(h) => {
                assert_eq!(h.level, 2);
                assert_eq!(para_text(&h.para), "Title Two");
            }
            other => panic!("expected setext h2, got {other:?}"),
        }
        // A `---` not preceded by paragraph text is still a thematic break.
        let doc2 = md_to_model("above\n\n---\n\nbelow");
        assert!(blocks(&doc2)
            .iter()
            .any(|blk| matches!(&blk.kind, BlockKind::HorizontalRule)));
    }

    #[test]
    fn inline_html_tags_and_entities() {
        let doc = md_to_model("a <b>bold</b> &amp; <i>italic</i> end");
        let p = first_para(&doc);
        assert!(
            p.runs
                .iter()
                .any(|i| matches!(i, Inline::Run(r) if r.style.bold && r.text == "bold")),
            "a bold run from <b>, got {:?}",
            p.runs
        );
        assert!(p
            .runs
            .iter()
            .any(|i| matches!(i, Inline::Run(r) if r.style.italic && r.text == "italic")));
        // `&amp;` decodes to `&` in the flattened text.
        assert!(para_text(p).contains(" & "), "got {:?}", para_text(p));
        // `<a href>` maps to a link.
        let doc2 = md_to_model("go <a href=\"https://x.io\">there</a>");
        let link = first_para(&doc2)
            .runs
            .iter()
            .find_map(|i| match i {
                Inline::Link { href, children } => Some((href, children)),
                _ => None,
            })
            .expect("an anchor link");
        assert_eq!(link.0, &LinkTarget::Url("https://x.io".into()));
        // `<br>` becomes a line break.
        let doc3 = md_to_model("one<br>two");
        assert!(first_para(&doc3)
            .runs
            .iter()
            .any(|i| matches!(i, Inline::LineBreak)));
        // An unknown tag degrades gracefully: tag dropped, text kept.
        let doc4 = md_to_model("x <unknown>kept</unknown> y");
        assert_eq!(para_text(first_para(&doc4)), "x kept y");
    }

    #[test]
    fn code_span_still_wins_over_new_markers() {
        // Backslash escapes and code spans keep CommonMark precedence over the
        // newly-added markers (`~~`, `![`, `<`, `&`).
        let doc = md_to_model("`~~x~~ <b> &amp; ![a](b)` after");
        let p = first_para(&doc);
        assert!(
            p.runs.iter().any(|i| matches!(i, Inline::Run(r)
                if matches!(r.style.generic, Generic::Mono)
                    && r.text == "~~x~~ <b> &amp; ![a](b)")),
            "code span verbatim, got {:?}",
            p.runs
        );
        // Escaped markers stay literal (escaping the `~` and the `[`/`!` so no
        // strike, link or image is recognised).
        let doc2 = md_to_model("\\~\\~ and \\!\\[x\\](y)");
        assert_eq!(para_text(first_para(&doc2)), "~~ and ![x](y)");
    }
}
