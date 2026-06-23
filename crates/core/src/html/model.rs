//! HTML → unified editable [`Document`](crate::model::Document) model.
//!
//! The companion of the HTML→PDF renderer ([`super::paint`]): instead of laying
//! the DOM out to PDF, [`to_model`] walks the same parsed [`Node`] tree —
//! cascading the same [`Stylesheet`] for each element — and lowers it into the
//! format-neutral [`Document`] tree (paragraphs, headings, lists, tables, inline
//! runs / links / images). The blocks are *flow* blocks (`frame = None`); a
//! single section/page holds them.
//!
//! Structure is recovered from the computed [`Display`] (block / list-item /
//! table) and the tag (`h1`..`h6` → [`Heading`], `a` → link, `img`/`ul`/`ol`/
//! `table` …), matching how the layout engine groups boxes — so the model and
//! the rendered output agree on what is a block, a list, a table or inline text.

use super::css::{Display, ListStyle, Style, Stylesheet};
use super::dom::{Element, Node};
use crate::convert::style::Generic;
use crate::model::{
    Block, BlockKind, Cell, CharStyle, Document, Heading, Inline, InlineRun, LinkTarget, List,
    ListItem, ListMarker, Page, Paragraph, Row, Section, Table,
};

/// Lower a parsed HTML node forest (with its cascaded `sheet`) into a
/// [`Document`]: one section/page of flow blocks. Inline-level content that
/// appears directly at the document level is wrapped in an implicit paragraph.
pub fn to_model(nodes: &[Node], sheet: &Stylesheet) -> Document {
    let root = Style::default();
    let mut blocks = Vec::new();
    let mut pending: Vec<Inline> = Vec::new();
    walk_blocks(nodes, sheet, &root, &[], &mut blocks, &mut pending);
    flush_paragraph(&mut pending, &mut blocks);

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

/// Walk a sibling list, classifying each node as a block (recurse / emit) or as
/// inline content (accumulated into `pending` until the next block flushes it).
fn walk_blocks(
    nodes: &[Node],
    sheet: &Stylesheet,
    parent: &Style,
    ancestors: &[&Element],
    out: &mut Vec<Block>,
    pending: &mut Vec<Inline>,
) {
    for node in nodes {
        match node {
            Node::Text(t) => push_text(pending, t, parent),
            Node::Element(el) => {
                let style = sheet.computed(el, parent, ancestors);
                match style.display {
                    Display::None => {}
                    Display::Inline | Display::InlineBlock => {
                        // Inline element: collect its inline content in place.
                        collect_inline_into(el, sheet, &style, ancestors, pending);
                    }
                    _ => {
                        // Block-level: flush any pending inline run as a paragraph,
                        // then emit this element's block(s).
                        flush_paragraph(pending, out);
                        emit_block(el, sheet, &style, ancestors, out);
                    }
                }
            }
        }
    }
}

/// Emit a block-level element as one or more model blocks: heading (`h1`..`h6`),
/// list (`ul`/`ol` or any `list-item`), table, image, or a generic block that
/// recurses into its children (collecting child inline runs into a paragraph).
fn emit_block(
    el: &Element,
    sheet: &Stylesheet,
    style: &Style,
    ancestors: &[&Element],
    out: &mut Vec<Block>,
) {
    let mut chain = ancestors.to_vec();
    chain.push(el);

    if let Some(level) = heading_level(&el.tag) {
        let runs = inline_children(el, sheet, style, &chain);
        out.push(block(BlockKind::Heading(Heading {
            level,
            para: Paragraph {
                runs,
                ..Paragraph::default()
            },
        })));
        return;
    }

    match el.tag.as_str() {
        "img" => {
            if let Some(b) = image_inline(el).map(|inline| match inline {
                Inline::Image(img) => BlockKind::Image(img),
                _ => unreachable!(),
            }) {
                out.push(block(b));
            }
        }
        "ul" | "ol" => out.push(emit_list(el, sheet, style, &chain)),
        "table" => out.push(block(BlockKind::Table(emit_table(
            el, sheet, style, &chain,
        )))),
        _ => {
            // A bare list-item outside a `ul`/`ol`, or any other block: if it is
            // a list-item, wrap it; otherwise recurse into a flow of blocks +
            // a trailing inline paragraph.
            if style.display == Display::ListItem {
                let synthetic_marker = ListMarker::Bullet(marker_char(style.list_style));
                let ordered = matches!(
                    style.list_style,
                    ListStyle::Decimal
                        | ListStyle::LowerAlpha
                        | ListStyle::UpperAlpha
                        | ListStyle::LowerRoman
                        | ListStyle::UpperRoman
                );
                out.push(block(BlockKind::List(List {
                    ordered,
                    marker: synthetic_marker,
                    items: vec![list_item(el, sheet, style, &chain, 0)],
                })));
                return;
            }
            let mut child_blocks = Vec::new();
            let mut pending = Vec::new();
            walk_blocks(
                &el.children,
                sheet,
                style,
                &chain,
                &mut child_blocks,
                &mut pending,
            );
            flush_paragraph(&mut pending, &mut child_blocks);
            // A generic block (`div`/`section`/`p`/…) contributes its blocks
            // directly to the parent flow.
            out.extend(child_blocks);
        }
    }
}

/// Build a [`List`] from a `ul`/`ol`: its marker style from the list-style /
/// tag, and one [`ListItem`] per `li` child (other children are skipped).
fn emit_list(el: &Element, sheet: &Stylesheet, style: &Style, ancestors: &[&Element]) -> Block {
    let ordered = el.tag == "ol";
    let marker = if ordered {
        ListMarker::Decimal
    } else {
        ListMarker::Bullet(marker_char(style.list_style))
    };
    let mut items = Vec::new();
    for child in &el.children {
        if let Node::Element(li) = child {
            if li.tag == "li" {
                let li_style = sheet.computed(li, style, ancestors);
                items.push(list_item(li, sheet, &li_style, ancestors, 0));
            }
        }
    }
    block(BlockKind::List(List {
        ordered,
        marker,
        items,
    }))
}

/// Build one [`ListItem`] at nesting `level`: the item's inline content becomes a
/// paragraph, and nested `ul`/`ol`/block children become further blocks.
fn list_item(
    li: &Element,
    sheet: &Stylesheet,
    style: &Style,
    ancestors: &[&Element],
    level: u8,
) -> ListItem {
    let mut chain = ancestors.to_vec();
    chain.push(li);
    let mut blocks = Vec::new();
    let mut pending = Vec::new();
    walk_blocks(
        &li.children,
        sheet,
        style,
        &chain,
        &mut blocks,
        &mut pending,
    );
    // The item's own inline text leads the item as a paragraph.
    if !pending.is_empty() {
        let para = block(BlockKind::Paragraph(Paragraph {
            runs: std::mem::take(&mut pending),
            ..Paragraph::default()
        }));
        blocks.insert(0, para);
    }
    ListItem { blocks, level }
}

/// Build a [`Table`] from a `table`: every `tr` (anywhere under it) becomes a
/// [`Row`], every `td`/`th` a [`Cell`] honouring `colspan`/`rowspan` and the
/// computed background shading.
fn emit_table(el: &Element, sheet: &Stylesheet, style: &Style, ancestors: &[&Element]) -> Table {
    let mut rows = Vec::new();
    collect_rows(el, sheet, style, ancestors, &mut rows);
    Table {
        rows,
        col_widths: Vec::new(),
        border: crate::model::BorderStyle::default(),
    }
}

/// Recursively find `tr` elements (through `thead`/`tbody`/`tfoot` wrappers) and
/// turn each into a model [`Row`].
fn collect_rows(
    el: &Element,
    sheet: &Stylesheet,
    parent: &Style,
    ancestors: &[&Element],
    rows: &mut Vec<Row>,
) {
    let mut chain = ancestors.to_vec();
    chain.push(el);
    for child in &el.children {
        if let Node::Element(c) = child {
            let cs = sheet.computed(c, parent, &chain);
            if c.tag == "tr" || cs.display == Display::TableRow {
                rows.push(table_row(c, sheet, &cs, &chain));
            } else if matches!(c.tag.as_str(), "thead" | "tbody" | "tfoot") {
                collect_rows(c, sheet, &cs, &chain, rows);
            }
        }
    }
}

/// Build one [`Row`] from a `tr`: a [`Cell`] per `td`/`th` child.
fn table_row(tr: &Element, sheet: &Stylesheet, style: &Style, ancestors: &[&Element]) -> Row {
    let mut chain = ancestors.to_vec();
    chain.push(tr);
    let mut cells = Vec::new();
    for child in &tr.children {
        if let Node::Element(td) = child {
            let cs = sheet.computed(td, style, &chain);
            if matches!(td.tag.as_str(), "td" | "th") || cs.display == Display::TableCell {
                cells.push(table_cell(td, sheet, &cs, &chain));
            }
        }
    }
    Row {
        cells,
        height: None,
    }
}

/// Build one [`Cell`] from a `td`/`th`: its block + inline content, the
/// `colspan`/`rowspan` attributes, and the computed `background` as shading.
fn table_cell(td: &Element, sheet: &Stylesheet, style: &Style, ancestors: &[&Element]) -> Cell {
    let mut chain = ancestors.to_vec();
    chain.push(td);
    let mut blocks = Vec::new();
    let mut pending = Vec::new();
    walk_blocks(
        &td.children,
        sheet,
        style,
        &chain,
        &mut blocks,
        &mut pending,
    );
    if !pending.is_empty() {
        let para = block(BlockKind::Paragraph(Paragraph {
            runs: std::mem::take(&mut pending),
            ..Paragraph::default()
        }));
        blocks.insert(0, para);
    }
    let col_span = td
        .attr("colspan")
        .and_then(|v| v.trim().parse::<u16>().ok())
        .unwrap_or(1)
        .max(1);
    let row_span = td
        .attr("rowspan")
        .and_then(|v| v.trim().parse::<u16>().ok())
        .unwrap_or(1)
        .max(1);
    Cell {
        blocks,
        col_span,
        row_span,
        shading: style.background,
    }
}

/// The inline runs of a block element: its child inline content collapsed into
/// an [`Inline`] vector (text, `<a>` links, `<img>`, `<br>`, nested spans).
fn inline_children(
    el: &Element,
    sheet: &Stylesheet,
    style: &Style,
    ancestors: &[&Element],
) -> Vec<Inline> {
    let mut out = Vec::new();
    for child in &el.children {
        match child {
            Node::Text(t) => push_text(&mut out, t, style),
            Node::Element(c) => {
                let cs = sheet.computed(c, style, ancestors);
                collect_inline_into(c, sheet, &cs, ancestors, &mut out);
            }
        }
    }
    out
}

/// Append an inline element's content to `out`: `<br>` → line break, `<img>` →
/// image, `<a href>` → a [`Inline::Link`], else its text children styled by `cs`.
fn collect_inline_into(
    el: &Element,
    sheet: &Stylesheet,
    cs: &Style,
    ancestors: &[&Element],
    out: &mut Vec<Inline>,
) {
    match el.tag.as_str() {
        "br" => out.push(Inline::LineBreak),
        "img" => {
            if let Some(img) = image_inline(el) {
                out.push(img);
            }
        }
        "a" => {
            let mut chain = ancestors.to_vec();
            chain.push(el);
            let children = inline_children(el, sheet, cs, &chain);
            let href = el.attr("href").unwrap_or("").to_string();
            out.push(Inline::Link {
                href: LinkTarget::Url(href),
                children,
            });
        }
        _ => {
            let mut chain = ancestors.to_vec();
            chain.push(el);
            for child in &el.children {
                match child {
                    Node::Text(t) => push_text(out, t, cs),
                    Node::Element(c) => {
                        let ccs = sheet.computed(c, cs, &chain);
                        if ccs.display == Display::None {
                            continue;
                        }
                        collect_inline_into(c, sheet, &ccs, &chain, out);
                    }
                }
            }
        }
    }
}

/// An `<img>` as an [`Inline::Image`] referencing a content-hash resource keyed
/// by its `src` (the bytes themselves live with the host; here we record the
/// reference and `alt`). Returns `None` when `src` is absent.
fn image_inline(el: &Element) -> Option<Inline> {
    let src = el.attr("src").filter(|s| !s.trim().is_empty())?;
    Some(Inline::Image(crate::model::ImageRef {
        resource: fnv1a(src.as_bytes()),
        alt: el.attr("alt").map(|s| s.to_string()),
    }))
}

/// Push a text run with the given computed `style`, applying `text-transform`,
/// collapsing runs of whitespace for non-`pre` text, and coalescing with the
/// previous run when the style matches. Whitespace-only text in flow is dropped.
fn push_text(out: &mut Vec<Inline>, text: &str, style: &Style) {
    let transformed = style.text_transform.apply(text);
    let normalized = if style.pre {
        transformed
    } else {
        collapse_ws(&transformed)
    };
    if normalized.is_empty() {
        return;
    }
    let cs = char_style(style);
    if let Some(Inline::Run(last)) = out.last_mut() {
        if last.style == cs {
            last.text.push_str(&normalized);
            return;
        }
    }
    out.push(Inline::Run(InlineRun {
        text: normalized,
        style: cs,
        source_index: None,
    }));
}

/// Collapse internal runs of ASCII/Unicode whitespace to single spaces, keeping
/// a single leading/trailing space when present (so word boundaries between
/// adjacent inline elements are preserved).
fn collapse_ws(s: &str) -> String {
    if s.trim().is_empty() {
        return if s.is_empty() {
            String::new()
        } else {
            " ".to_string()
        };
    }
    let leading = s.starts_with(|c: char| c.is_whitespace());
    let trailing = s.ends_with(|c: char| c.is_whitespace());
    let mut out = String::with_capacity(s.len());
    if leading {
        out.push(' ');
    }
    out.push_str(&s.split_whitespace().collect::<Vec<_>>().join(" "));
    if trailing {
        out.push(' ');
    }
    out
}

/// Map a computed [`Style`] to a model [`CharStyle`] (family/generic, size,
/// bold/italic/underline/strike, colour).
fn char_style(style: &Style) -> CharStyle {
    let generic = if style.generic_mono {
        Generic::Mono
    } else if style.generic_serif {
        Generic::Serif
    } else {
        Generic::Sans
    };
    CharStyle {
        family: style.font_family.clone(),
        generic,
        size_pt: style.font_size,
        bold: style.bold,
        italic: style.italic,
        underline: style.underline,
        strike: style.strike,
        color: Some(style.color),
        background: None,
        vertical_align: crate::model::VAlign::Baseline,
    }
}

/// Flush accumulated inline runs as a [`Paragraph`] block (no-op when empty).
fn flush_paragraph(pending: &mut Vec<Inline>, out: &mut Vec<Block>) {
    if pending.is_empty() {
        return;
    }
    // Drop a paragraph that is only whitespace runs.
    let has_content = pending.iter().any(|i| match i {
        Inline::Run(r) => !r.text.trim().is_empty(),
        _ => true,
    });
    if !has_content {
        pending.clear();
        return;
    }
    out.push(block(BlockKind::Paragraph(Paragraph {
        runs: std::mem::take(pending),
        ..Paragraph::default()
    })));
}

/// A default-framed flow [`Block`] wrapping `kind`.
fn block(kind: BlockKind) -> Block {
    Block {
        kind,
        ..Block::default()
    }
}

/// `h1`..`h6` → heading level 1..=6.
fn heading_level(tag: &str) -> Option<u8> {
    match tag {
        "h1" => Some(1),
        "h2" => Some(2),
        "h3" => Some(3),
        "h4" => Some(4),
        "h5" => Some(5),
        "h6" => Some(6),
        _ => None,
    }
}

/// The bullet glyph for a CSS [`ListStyle`] (unordered marker styles).
fn marker_char(ls: ListStyle) -> char {
    match ls {
        ListStyle::Circle => '\u{25E6}', // ◦
        ListStyle::Square => '\u{25AA}', // ▪
        _ => '\u{2022}',                 // •
    }
}

/// 64-bit FNV-1a hash — a stable, dependency-free resource key for image `src`.
fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html::css::{collect_style_css, Stylesheet};
    use crate::html::dom;
    use crate::model::BlockKind;

    fn model_of(html: &str) -> Document {
        let nodes = dom::parse(html);
        let sheet = Stylesheet::new(&collect_style_css(&nodes));
        to_model(&nodes, &sheet)
    }

    fn blocks(doc: &Document) -> &[Block] {
        &doc.sections[0].pages[0].blocks
    }

    #[test]
    fn heading_paragraph_and_list() {
        let doc = model_of("<h1>T</h1><p>body</p><ul><li>a</li><li>b</li></ul>");
        let b = blocks(&doc);
        assert_eq!(b.len(), 3, "heading + paragraph + list");

        match &b[0].kind {
            BlockKind::Heading(h) => {
                assert_eq!(h.level, 1);
                assert_eq!(run_text(&h.para), "T");
            }
            other => panic!("expected heading, got {other:?}"),
        }
        match &b[1].kind {
            BlockKind::Paragraph(p) => assert_eq!(run_text(p), "body"),
            other => panic!("expected paragraph, got {other:?}"),
        }
        match &b[2].kind {
            BlockKind::List(l) => {
                assert!(!l.ordered);
                assert_eq!(l.items.len(), 2, "two list items");
            }
            other => panic!("expected list, got {other:?}"),
        }
    }

    #[test]
    fn bold_run_and_link() {
        let doc = model_of("<p>plain <b>bold</b> <a href=\"http://x\">link</a></p>");
        let b = blocks(&doc);
        let p = match &b[0].kind {
            BlockKind::Paragraph(p) => p,
            other => panic!("expected paragraph, got {other:?}"),
        };
        // A bold run exists.
        assert!(
            p.runs
                .iter()
                .any(|i| matches!(i, Inline::Run(r) if r.style.bold && r.text.contains("bold"))),
            "bold run present"
        );
        // A link wraps the anchor text.
        assert!(
            p.runs.iter().any(|i| matches!(i, Inline::Link { .. })),
            "link present"
        );
    }

    #[test]
    fn table_rows_and_cells() {
        let doc = model_of(
            "<table><tr><td>A</td><td>B</td></tr><tr><td colspan=\"2\">C</td></tr></table>",
        );
        let b = blocks(&doc);
        let t = match &b[0].kind {
            BlockKind::Table(t) => t,
            other => panic!("expected table, got {other:?}"),
        };
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.rows[0].cells.len(), 2);
        assert_eq!(t.rows[1].cells[0].col_span, 2);
    }

    /// Concatenate a paragraph's plain run text (for assertions).
    fn run_text(p: &Paragraph) -> String {
        let mut s = String::new();
        collect_run_text(&p.runs, &mut s);
        s.trim().to_string()
    }

    fn collect_run_text(runs: &[Inline], out: &mut String) {
        for inline in runs {
            match inline {
                Inline::Run(r) => out.push_str(&r.text),
                Inline::Link { children, .. } => collect_run_text(children, out),
                _ => {}
            }
        }
    }
}
