//! Markdown → unified editable [`Document`](crate::model::Document) importer.
//!
//! A small, dependency-free **CommonMark-ish** parser that lowers the common
//! constructs straight into the model tree (the same target the HTML/Office/text
//! importers build), so Markdown becomes a first-class editable source format:
//!
//! - ATX headings `#`..`######` → [`Heading`] (level 1..=6), sized and bold so
//!   they render distinctly in the model→PDF fallback.
//! - Paragraphs (blank-line separated) with inline styling.
//! - Lists: `-`/`*`/`+` (unordered) and `1.`/`1)` (ordered), nesting by indent.
//! - Inline: `**bold**`/`__bold__`, `*italic*`/`_italic_`, `` `code` ``,
//!   `[text](url)` links.
//! - Fenced code blocks ```` ``` ```` / `~~~` → monospace paragraphs (verbatim).
//! - Block quotes `>` → indented paragraphs.
//! - Thematic breaks `---`/`***`/`___` → a thin full-width rule (a [`Shape`]).
//! - GFM pipe tables `| a | b |` with a `---|---` separator row → [`Table`].
//!
//! Constructs outside this set (raw inline HTML, footnotes, reference links,
//! setext headings) are not specially handled — their source text flows through
//! as plain paragraph content rather than being dropped.

use crate::convert::style::Generic;
use crate::model::{
    Block, BlockKind, Blockquote, Cell, CharStyle, CodeBlock, Document, Heading, Inline, InlineRun,
    LinkTarget, List, ListItem, ListMarker, Page, Paragraph, Row, Section, Table,
};

/// Body font size used for paragraph runs (points). Mirrors the model→PDF
/// fallback's default so spacing stays predictable.
const BODY_PT: f64 = 11.0;
/// Monospace size for inline `code` and fenced code blocks (points).
const CODE_PT: f64 = 10.0;

/// Per-level heading sizes (index 0 ⇒ `#`/h1 … index 5 ⇒ `######`/h6), points.
const HEADING_PT: [f64; 6] = [24.0, 20.0, 16.0, 14.0, 12.0, 11.0];

/// Markdown text → [`Document`]: one section / one page of flow blocks. Never
/// fails — unrecognized syntax degrades to plain paragraph text.
pub fn md_to_model(md: &str) -> Document {
    let lines: Vec<&str> = md.lines().collect();
    let blocks = parse_blocks(&lines);
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

/// Parse a slice of source lines into top-level [`Block`]s.
fn parse_blocks(lines: &[&str]) -> Vec<Block> {
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
            out.push(heading_block(level, &text));
            i += 1;
            continue;
        }

        // GFM pipe table: current line looks like a table row and the next is a
        // delimiter row (`---|:--:|...`).
        if looks_like_table_row(raw) && lines.get(i + 1).is_some_and(|n| is_table_delimiter(n)) {
            let (table, consumed) = parse_table(&lines[i..]);
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
            out.push(blockquote_block(parse_blocks(&refs)));
            continue;
        }

        // List: a run of items each introduced by a bullet (`-`/`*`/`+`) or an
        // ordinal (`N.`/`N)`). Mixed markers split into separate lists.
        if list_item_marker(raw).is_some() {
            let (list, consumed) = parse_list(&lines[i..]);
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
            {
                break;
            }
            if !para_text.is_empty() {
                para_text.push(' ');
            }
            para_text.push_str(t);
            i += 1;
        }
        out.push(paragraph_block(parse_inlines(&para_text)));
    }
    out
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
fn heading_block(level: u8, text: &str) -> Block {
    let size = HEADING_PT[(level.clamp(1, 6) - 1) as usize];
    let mut runs = parse_inlines(text);
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
fn parse_list(lines: &[&str]) -> (Block, usize) {
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
            blocks: vec![paragraph_block(parse_inlines(content))],
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
        }),
        ..Block::default()
    };
    (block, i)
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
fn parse_table(lines: &[&str]) -> (Block, usize) {
    let header = split_table_row(lines[0]);
    let ncols = header.len().max(1);
    let mut rows: Vec<Row> = Vec::new();

    rows.push(make_row(&header, ncols, true));
    let mut consumed = 2; // header + delimiter
    let mut i = 2;
    while i < lines.len() {
        let l = lines[i];
        if l.trim().is_empty() || !looks_like_table_row(l) {
            break;
        }
        let cells = split_table_row(l);
        rows.push(make_row(&cells, ncols, false));
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
fn make_row(cells: &[String], ncols: usize, header: bool) -> Row {
    let mut out = Vec::with_capacity(ncols);
    for c in 0..ncols {
        let text = cells.get(c).map(String::as_str).unwrap_or("");
        let mut runs = parse_inlines(text);
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
/// `**`/`__` (bold), `*`/`_` (italic), `` ` `` (code) and `[t](url)` links.
/// Unmatched markers are treated as literal characters.
fn parse_inlines(text: &str) -> Vec<Inline> {
    let mut runs = Vec::new();
    inline_into(text, body_style(), &mut runs);
    if runs.is_empty() {
        runs.push(Inline::Run(InlineRun {
            text: String::new(),
            style: body_style(),
            source_index: None,
        }));
    }
    runs
}

/// Tokenize `text` under the active `style`, pushing runs/links into `out`.
/// Emphasis recurses with the bold/italic flag toggled; code switches to the
/// monospace style.
fn inline_into(text: &str, style: CharStyle, out: &mut Vec<Inline>) {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let mut buf = String::new();

    let flush = |buf: &mut String, out: &mut Vec<Inline>, style: &CharStyle| {
        if !buf.is_empty() {
            out.push(Inline::Run(InlineRun {
                text: std::mem::take(buf),
                style: style.clone(),
                source_index: None,
            }));
        }
    };

    while i < chars.len() {
        let c = chars[i];

        // Backslash escape: the next char is literal.
        if c == '\\' && i + 1 < chars.len() {
            buf.push(chars[i + 1]);
            i += 2;
            continue;
        }

        // Inline code: `…` (verbatim, no nested markup).
        if c == '`' {
            if let Some(end) = find_char(&chars, i + 1, '`') {
                flush(&mut buf, out, &style);
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

        // Strong emphasis: `**` or `__`.
        if (c == '*' || c == '_') && chars.get(i + 1) == Some(&c) {
            let delim = [c, c];
            if let Some(end) = find_seq(&chars, i + 2, &delim) {
                flush(&mut buf, out, &style);
                let inner: String = chars[i + 2..end].iter().collect();
                let mut s = style.clone();
                s.bold = true;
                inline_into(&inner, s, out);
                i = end + 2;
                continue;
            }
        }

        // Emphasis: `*` or `_`.
        if c == '*' || c == '_' {
            if let Some(end) = find_char(&chars, i + 1, c) {
                if end > i + 1 {
                    flush(&mut buf, out, &style);
                    let inner: String = chars[i + 1..end].iter().collect();
                    let mut s = style.clone();
                    s.italic = true;
                    inline_into(&inner, s, out);
                    i = end + 1;
                    continue;
                }
            }
        }

        // Link: `[text](url)`.
        if c == '[' {
            if let Some(close) = find_char(&chars, i + 1, ']') {
                if chars.get(close + 1) == Some(&'(') {
                    if let Some(paren) = find_char(&chars, close + 2, ')') {
                        flush(&mut buf, out, &style);
                        let label: String = chars[i + 1..close].iter().collect();
                        let url: String = chars[close + 2..paren].iter().collect();
                        let mut children = Vec::new();
                        inline_into(&label, style.clone(), &mut children);
                        out.push(Inline::Link {
                            href: LinkTarget::Url(url.trim().to_string()),
                            children,
                        });
                        i = paren + 1;
                        continue;
                    }
                }
            }
        }

        buf.push(c);
        i += 1;
    }
    flush(&mut buf, out, &style);
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
}
