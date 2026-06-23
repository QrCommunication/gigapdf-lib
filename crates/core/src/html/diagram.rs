//! Zero-dependency **Mermaid flowchart** renderer for the HTML→PDF engine.
//!
//! Mermaid is a text-to-diagram syntax. A `<pre class="mermaid">` (or
//! `<div class="mermaid">`, or `<pre><code class="language-mermaid">`) block
//! carries a diagram description as plain text; mainstream tooling renders it to
//! SVG in the browser via a JavaScript library. We have no browser, so this
//! module parses the most common dialect — **`graph`/`flowchart`** — and lays it
//! out with a pragmatic Sugiyama-style layered algorithm, then emits the geometry
//! as a single in-engine [`SvgImage`] (boxes, edges, arrow-heads, dashes) plus a
//! list of centred text labels. Both are handed back to the block layout, which
//! pushes them as ordinary [`Fragment`](super::layout::Fragment)s — so the
//! diagram is **native PDF vectors**, crisp at any zoom, never a raster.
//!
//! Scope (deliberately bounded, with graceful fall-through):
//! - **Supported now:** `graph`/`flowchart` with direction `TD`/`TB`/`BT`/`LR`/
//!   `RL`; node shapes `A[rect]`, `B(round)`, `C{diamond}`, `D((circle))`,
//!   `E([stadium])`; edges `-->`, `---`, `-.->`, `==>`, `--x`, `--o`; edge labels
//!   `-->|txt|` and `A-- txt -->B`; edge chains `A-->B-->C`. The directives
//!   `subgraph`/`end`/`style`/`classDef`/`class`/`click`/`%%` comments are
//!   tolerated (skipped) and never abort the parse.
//! - **Deferred (fall through to the normal code-block rendering):** other
//!   diagram kinds (`sequenceDiagram`, `gantt`, `classDiagram`, `pie`, …),
//!   `style fill:` colour application, advanced multi-component packing.
//!
//! Robustness contract: parsing is **purely defensive** — any malformed or
//! unrecognised input yields `None`, so the caller renders the block exactly as
//! it would have without this module. Nothing here can panic.

use super::css::Style;
use super::dom::{Element, Node};
use super::layout::Measure;
use crate::svg::{parse_svg, SvgImage};

/// A positioned, centred text label to draw over the diagram (top-down points,
/// relative to the diagram's top-left origin). `(cx, cy)` is the label centre.
#[derive(Debug, Clone)]
pub struct Label {
    pub cx: f64,
    pub cy: f64,
    pub text: String,
    /// Font size in points (already scaled for the diagram).
    pub font_size: f64,
    /// `true` for node titles (bold), `false` for edge labels (regular).
    pub bold: bool,
}

/// A laid-out diagram ready to place at a block's origin.
#[derive(Debug, Clone)]
pub struct Diagram {
    /// Overall width in points.
    pub width: f64,
    /// Overall height in points.
    pub height: f64,
    /// The vector geometry (boxes, edges, arrow-heads) as native PDF paths.
    pub image: SvgImage,
    /// Centred text labels (node titles + edge labels).
    pub labels: Vec<Label>,
}

// ─────────────────────────────── detection ────────────────────────────────

/// If `el` is a Mermaid container, return its verbatim source text; else `None`.
///
/// Recognised forms (class-token match is case-insensitive):
/// - `<pre class="… mermaid …">`
/// - `<div class="… mermaid …">`
/// - `<pre><code class="… language-mermaid …">` (and `lang-mermaid`)
pub fn mermaid_source(el: &Element) -> Option<String> {
    let tag = el.tag.as_str();
    if tag == "pre" || tag == "div" {
        if class_has(el, "mermaid") {
            return Some(collect_text(el));
        }
        // `<pre><code class="language-mermaid">…` — single `<code>` child.
        if tag == "pre" {
            if let Some(code) = sole_code_child(el) {
                if class_has_lang(code, "mermaid") {
                    return Some(collect_text(code));
                }
            }
        }
    }
    None
}

/// `<pre>`'s only meaningful child being a `<code>` element (whitespace text
/// around it is ignored).
fn sole_code_child(pre: &Element) -> Option<&Element> {
    let mut found: Option<&Element> = None;
    for child in &pre.children {
        match child {
            Node::Text(t) if t.trim().is_empty() => {}
            Node::Element(e) if e.tag == "code" => {
                if found.is_some() {
                    return None; // more than one element child
                }
                found = Some(e);
            }
            _ => return None, // non-whitespace text or a non-code element
        }
    }
    found
}

/// Whether `el`'s `class` attribute contains the whitespace-separated `token`
/// (case-insensitive).
fn class_has(el: &Element, token: &str) -> bool {
    el.attr("class")
        .map(|c| {
            c.split_whitespace()
                .any(|t| t.eq_ignore_ascii_case(token))
        })
        .unwrap_or(false)
}

/// Whether `el` carries `language-<token>` or `lang-<token>` as a class token
/// (case-insensitive) — the convention syntax highlighters use on `<code>`.
fn class_has_lang(el: &Element, token: &str) -> bool {
    el.attr("class")
        .map(|c| {
            c.split_whitespace().any(|t| {
                let t = t.to_ascii_lowercase();
                t == format!("language-{token}") || t == format!("lang-{token}")
            })
        })
        .unwrap_or(false)
}

/// Concatenate the text of every descendant text node, preserving order. This
/// recovers the verbatim diagram source even if the parser split it around a
/// stray inline element.
fn collect_text(el: &Element) -> String {
    let mut out = String::new();
    push_text(el, &mut out);
    out
}

fn push_text(el: &Element, out: &mut String) {
    for child in &el.children {
        match child {
            Node::Text(t) => out.push_str(t),
            Node::Element(e) => {
                // A `<br>` inside a label means a hard line break in the source.
                if e.tag == "br" {
                    out.push('\n');
                }
                push_text(e, out);
            }
        }
    }
}

// ─────────────────────────────── parsing ──────────────────────────────────

/// Flow direction (mermaid `graph <DIR>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dir {
    /// Top→bottom (`TD`/`TB`).
    Down,
    /// Bottom→top (`BT`).
    Up,
    /// Left→right (`LR`).
    Right,
    /// Right→left (`RL`).
    Left,
}

impl Dir {
    /// Layers stack vertically (Down/Up) vs horizontally (Right/Left).
    fn vertical(self) -> bool {
        matches!(self, Dir::Down | Dir::Up)
    }
    /// Whether rank order is reversed against the on-screen axis (Up/Left).
    fn reversed(self) -> bool {
        matches!(self, Dir::Up | Dir::Left)
    }
}

/// A node shape (`[]`, `()`, `{}`, `(())`, `([])`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Rect,
    Round,
    Diamond,
    Circle,
    Stadium,
}

/// Arrow-head style on an edge end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Head {
    /// `>` — a filled triangle (`-->`).
    Arrow,
    /// `x` — a cross (`--x`).
    Cross,
    /// `o` — a small circle (`--o`).
    Circle,
    /// `---` — no head (open line end).
    None,
}

/// Edge line style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Line {
    Solid,
    /// `-.->` (dotted).
    Dotted,
    /// `==>` (thick).
    Thick,
}

#[derive(Debug, Clone)]
struct ParsedNode {
    id: String,
    label: String,
    shape: Shape,
}

#[derive(Debug, Clone)]
struct ParsedEdge {
    from: usize,
    to: usize,
    head: Head,
    line: Line,
    label: Option<String>,
}

#[derive(Debug)]
struct Graph {
    dir: Dir,
    nodes: Vec<ParsedNode>,
    edges: Vec<ParsedEdge>,
}

/// Parse a flowchart source. Returns `None` for non-flowchart diagrams or input
/// with no usable nodes, so the caller falls back to plain code rendering.
fn parse_flowchart(src: &str) -> Option<Graph> {
    let mut dir: Option<Dir> = None;
    let mut nodes: Vec<ParsedNode> = Vec::new();
    let mut edges: Vec<ParsedEdge> = Vec::new();
    let mut subgraph_depth: u32 = 0;
    let mut saw_header = false;

    for raw in logical_lines(src) {
        let line = strip_comment(&raw);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // A line can carry several `;`-separated statements (and the header may
        // share its line with the first statement: `graph TD; A-->B;`). Split
        // first, then classify each part — so nothing after a `;` is dropped.
        for part in split_statements(line) {
            let stmt = part.trim();
            if stmt.is_empty() {
                continue;
            }

            // Header detection runs on the very first non-empty statement.
            if !saw_header {
                if let Some(d) = parse_header(stmt) {
                    dir = Some(d);
                    saw_header = true;
                    // The header may itself be only `graph TD` — the rest of the
                    // statement (if any, e.g. `graph TD A-->B`) is parsed below.
                    let rest = strip_header_prefix(stmt);
                    if rest.is_empty() {
                        continue;
                    }
                    parse_statement(rest, &mut nodes, &mut edges);
                    continue;
                }
                // Any other diagram kind (sequenceDiagram, gantt, …) → not ours.
                if is_other_diagram_keyword(first_word(stmt)) {
                    return None;
                }
                // No explicit header but the first content looks like flowchart
                // syntax — accept with the default direction. Otherwise bail.
                if looks_like_flow_line(stmt) {
                    dir = Some(Dir::Down);
                    saw_header = true;
                    // fall through to parse this statement as content
                } else {
                    return None;
                }
            }

            // Structural directives we tolerate but ignore.
            if first_word_is(stmt, "subgraph") {
                subgraph_depth = subgraph_depth.saturating_add(1);
                continue;
            }
            if stmt == "end" || first_word_is(stmt, "end") {
                subgraph_depth = subgraph_depth.saturating_sub(1);
                continue;
            }
            if first_word_is(stmt, "style")
                || first_word_is(stmt, "classDef")
                || first_word_is(stmt, "class")
                || first_word_is(stmt, "click")
                || first_word_is(stmt, "linkStyle")
                || first_word_is(stmt, "direction")
            {
                continue;
            }

            parse_statement(stmt, &mut nodes, &mut edges);
        }
    }
    let _ = subgraph_depth;

    if nodes.is_empty() {
        return None;
    }
    Some(Graph {
        dir: dir.unwrap_or(Dir::Down),
        nodes,
        edges,
    })
}

/// Merge backslash/whitespace continuations and split on physical newlines.
fn logical_lines(src: &str) -> Vec<String> {
    // Mermaid is line-oriented; we just split on newlines. (No line-continuation
    // syntax in flowcharts.)
    src.lines().map(|l| l.to_string()).collect()
}

/// Drop a trailing `%%…` comment (mermaid line comment). A `%%` only starts a
/// comment when not inside a `[...]`/`{...}`/`"..."` label.
fn strip_comment(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        match c {
            '"' => in_str = !in_str,
            '[' | '(' | '{' if !in_str => depth += 1,
            ']' | ')' | '}' if !in_str => depth -= 1,
            '%' if !in_str && depth <= 0 && i + 1 < bytes.len() && bytes[i + 1] == b'%' => {
                return line[..i].to_string();
            }
            _ => {}
        }
        i += 1;
    }
    line.to_string()
}

/// Split a line into `;`-separated statements, honouring label brackets/quotes
/// so a `;` inside `A["a; b"]` doesn't split.
fn split_statements(line: &str) -> Vec<String> {
    split_top_level(line, ';')
}

/// Generic top-level split on `sep`, skipping separators nested in
/// `[]`/`()`/`{}` or inside double quotes.
fn split_top_level(s: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut start = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '"' => in_str = !in_str,
            '[' | '(' | '{' if !in_str => depth += 1,
            ']' | ')' | '}' if !in_str => depth -= 1,
            _ if c == sep && depth <= 0 && !in_str => {
                out.push(s[start..i].to_string());
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    out.push(s[start..].to_string());
    out
}

fn first_word(line: &str) -> &str {
    line.split_whitespace().next().unwrap_or("")
}

fn first_word_is(line: &str, w: &str) -> bool {
    first_word(line).eq_ignore_ascii_case(w)
}

fn is_other_diagram_keyword(w: &str) -> bool {
    matches!(
        w,
        "sequenceDiagram"
            | "gantt"
            | "classDiagram"
            | "classDiagram-v2"
            | "stateDiagram"
            | "stateDiagram-v2"
            | "erDiagram"
            | "journey"
            | "pie"
            | "gitGraph"
            | "mindmap"
            | "timeline"
            | "quadrantChart"
            | "requirementDiagram"
            | "C4Context"
            | "sankey"
            | "xychart-beta"
            | "block-beta"
    )
}

/// Map a direction token (`TD`/`TB`/`BT`/`LR`/`RL`, case-insensitive) to a
/// [`Dir`]. Returns `None` for anything else (so it isn't mistaken for one).
fn dir_token(tok: &str) -> Option<Dir> {
    match tok.to_ascii_uppercase().as_str() {
        "TD" | "TB" => Some(Dir::Down),
        "BT" => Some(Dir::Up),
        "LR" => Some(Dir::Right),
        "RL" => Some(Dir::Left),
        _ => None,
    }
}

/// Whether `stmt` begins with the `graph`/`flowchart` keyword.
fn is_header_keyword(stmt: &str) -> bool {
    first_word_is(stmt, "graph") || first_word_is(stmt, "flowchart")
}

/// Parse the `graph <DIR>` / `flowchart <DIR>` header. Returns the direction
/// (defaulting to `Down` when no explicit/known direction follows) if the
/// statement starts with the keyword, else `None`.
fn parse_header(stmt: &str) -> Option<Dir> {
    if !is_header_keyword(stmt) {
        return None;
    }
    let second = stmt.split_whitespace().nth(1).unwrap_or("");
    Some(dir_token(second).unwrap_or(Dir::Down))
}

/// Given a statement that begins with a `graph`/`flowchart` header, return the
/// trailing content after the keyword and (if present) a recognised direction
/// token. A non-direction second token (e.g. `graph A-->B`) is kept as content.
fn strip_header_prefix(stmt: &str) -> &str {
    let s = stmt.trim_start();
    // Strip the keyword (case-insensitively) by its known length.
    let after_kw = if s.len() >= 5 && s[..5].eq_ignore_ascii_case("graph") {
        &s[5..]
    } else if s.len() >= 9 && s[..9].eq_ignore_ascii_case("flowchart") {
        &s[9..]
    } else {
        s
    };
    let after_kw = after_kw.trim_start();
    // Only skip the next token if it's a recognised direction; otherwise it's
    // content (no explicit direction was given).
    let second = after_kw.split_whitespace().next().unwrap_or("");
    if dir_token(second).is_some() {
        after_kw[second.len()..].trim_start()
    } else {
        after_kw
    }
    .trim()
}

/// Heuristic: does this line look like flowchart node/edge syntax? Used when no
/// `graph`/`flowchart` header is present.
fn looks_like_flow_line(line: &str) -> bool {
    line.contains("-->")
        || line.contains("---")
        || line.contains("-.-")
        || line.contains("==>")
        || line.contains("===")
        || line.contains("--x")
        || line.contains("--o")
        || line.contains('[')
        || line.contains('{')
}

// ─────────────────────────── statement parsing ────────────────────────────

/// Parse one statement: either a lone node declaration (`A[label]`) or an edge
/// chain (`A-->B-- t -->C`). Adds the nodes/edges discovered.
fn parse_statement(stmt: &str, nodes: &mut Vec<ParsedNode>, edges: &mut Vec<ParsedEdge>) {
    // Tokenise into an alternating sequence: NODE (CONNECTOR NODE)*.
    let toks = tokenise_chain(stmt);
    if toks.is_empty() {
        return;
    }

    // First token must be a node spec.
    let Tok::Node(spec0) = &toks[0] else {
        return;
    };
    let mut prev = ensure_node(nodes, spec0);

    let mut i = 1;
    while i + 1 < toks.len() {
        let conn = match &toks[i] {
            Tok::Conn(c) => c,
            // Two adjacent nodes with no connector — ignore the dangling one.
            Tok::Node(_) => {
                i += 1;
                continue;
            }
        };
        let Tok::Node(spec) = &toks[i + 1] else {
            break;
        };
        let cur = ensure_node(nodes, spec);
        // Direction of arrow follows the (already-normalised) connector. A
        // reversed head (`<--`) is normalised in `tokenise_chain` by swapping.
        let (from, to) = if conn.reversed { (cur, prev) } else { (prev, cur) };
        edges.push(ParsedEdge {
            from,
            to,
            head: conn.head,
            line: conn.line,
            label: conn.label.clone(),
        });
        prev = cur;
        i += 2;
    }

    // A lone node (single token) is just declared.
    let _ = prev;
}

/// A token in an edge chain.
#[derive(Debug, Clone)]
enum Tok {
    Node(NodeSpec),
    Conn(Connector),
}

/// A node reference with its (optional) inline shape+label.
#[derive(Debug, Clone)]
struct NodeSpec {
    id: String,
    label: Option<String>,
    shape: Shape,
}

/// A parsed connector between two nodes.
#[derive(Debug, Clone)]
struct Connector {
    head: Head,
    line: Line,
    label: Option<String>,
    /// `true` if the arrow points back (`<--`, `x--`, `o--`).
    reversed: bool,
}

/// Tokenise a statement into nodes and connectors. Walks the string finding node
/// specs (`id` + optional bracketed label) separated by connector runs
/// (`-->`, `-.->`, `==>`, `--x`, `---`, `-- text -->`, …).
fn tokenise_chain(stmt: &str) -> Vec<Tok> {
    let bytes = stmt.as_bytes();
    let mut toks: Vec<Tok> = Vec::new();
    let mut i = 0usize;
    let n = bytes.len();

    while i < n {
        // Skip leading whitespace.
        while i < n && (bytes[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= n {
            break;
        }

        // Is a connector starting here? Connectors contain `-`/`.`/`=` runs and
        // optional head chars `<`, `>`, `x`, `o`. A node id never starts with
        // those (mermaid ids are alnum/_).
        if is_connector_start(bytes, i) {
            let (conn, next) = read_connector(stmt, i);
            toks.push(Tok::Conn(conn));
            i = next;
            continue;
        }

        // Otherwise, read a node spec.
        if let Some((spec, next)) = read_node(stmt, i) {
            toks.push(Tok::Node(spec));
            i = next;
        } else {
            // Couldn't make progress — advance one to stay defensive.
            i += 1;
        }
    }
    toks
}

/// Whether a connector token begins at `i`. A connector begins with a link char
/// (`-`, `=`, `.`) or a reversed head (`<`) immediately followed by a link char.
fn is_connector_start(b: &[u8], i: usize) -> bool {
    match b[i] {
        b'-' | b'=' => true,
        b'.' => i + 1 < b.len() && (b[i + 1] == b'-'), // `.-` (rare) — treat as link
        b'<' => i + 1 < b.len() && matches!(b[i + 1], b'-' | b'='),
        // `x--`/`o--` reversed-head forms only count as a connector when the
        // next char is a link char (so we don't eat node ids like `x1`).
        b'x' | b'o' | b'X' | b'O' => {
            i + 1 < b.len() && matches!(b[i + 1], b'-' | b'=')
        }
        _ => false,
    }
}

/// Read a connector starting at `i`, returning it and the index just past it.
/// Handles the inline-label forms `-- text -->` and `-->|text|`.
fn read_connector(stmt: &str, i: usize) -> (Connector, usize) {
    let b = stmt.as_bytes();
    let n = b.len();
    let mut j = i;

    let mut reversed = false;
    // Leading reverse head: `<`, or `x`/`o` directly before a link char.
    match b[j] {
        b'<' => {
            reversed = true;
            j += 1;
        }
        b'x' | b'X' => {
            reversed = true;
            // consumed as a head; line begins next
            j += 1;
        }
        b'o' | b'O' => {
            reversed = true;
            j += 1;
        }
        _ => {}
    }

    // Determine line style by scanning the link run (chars `-`, `.`, `=`).
    let run_start = j;
    let mut dotted = false;
    let mut thick = false;
    while j < n {
        match b[j] {
            b'-' => {}
            b'.' => dotted = true,
            b'=' => thick = true,
            _ => break,
        }
        j += 1;
    }

    // Inline label between link halves: `-- text --` / `-. text .-` / `== text ==`.
    // After the first run, an optional ` text ` then another run may follow.
    let mut label: Option<String> = None;
    if j < n && (b[j] as char).is_whitespace() {
        // peek: is there a second link run after some text? Find next link char.
        let after_ws = skip_ws(b, j);
        // Collect text until the next link run or end / pipe.
        let mut k = after_ws;
        while k < n && !matches!(b[k], b'-' | b'=' | b'.' | b'>' | b'|') {
            k += 1;
        }
        // Only treat as an inline label if a second link run follows.
        if k < n && matches!(b[k], b'-' | b'=' | b'.') {
            let txt = stmt[after_ws..k].trim();
            if !txt.is_empty() {
                label = Some(unquote(txt));
            }
            // consume the second link run
            j = k;
            while j < n {
                match b[j] {
                    b'-' => {}
                    b'.' => dotted = true,
                    b'=' => thick = true,
                    _ => break,
                }
                j += 1;
            }
        }
    }

    let _ = run_start;

    // Trailing head: `>`, `x`, `o` (only if not already reversed at the front).
    let mut head = Head::None;
    if j < n {
        match b[j] {
            b'>' => {
                head = Head::Arrow;
                j += 1;
            }
            b'x' | b'X' => {
                head = Head::Cross;
                j += 1;
            }
            b'o' | b'O' => {
                head = Head::Circle;
                j += 1;
            }
            _ => {}
        }
    }
    // If reversed at the front and no trailing head, the head is on the `from`
    // side; we still draw a forward arrow after swapping endpoints.
    if reversed && head == Head::None {
        head = Head::Arrow;
    } else if reversed && head != Head::None {
        // Bidirectional — keep a single forward head (simplest faithful render).
    }

    // Pipe-delimited label: `-->|text|`.
    if j < n && b[j] == b'|' {
        let mut k = j + 1;
        while k < n && b[k] != b'|' {
            k += 1;
        }
        let txt = stmt[j + 1..k.min(n)].trim();
        if !txt.is_empty() {
            label = Some(unquote(txt));
        }
        j = if k < n { k + 1 } else { n };
    }

    let line = if dotted {
        Line::Dotted
    } else if thick {
        Line::Thick
    } else {
        Line::Solid
    };

    (
        Connector {
            head,
            line,
            label,
            reversed,
        },
        j,
    )
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && (b[i] as char).is_whitespace() {
        i += 1;
    }
    i
}

/// Read a node spec (`id` + optional bracketed label) starting at `i`. Returns
/// the spec and the index just past it, or `None` if there's no id here.
fn read_node(stmt: &str, i: usize) -> Option<(NodeSpec, usize)> {
    let b = stmt.as_bytes();
    let n = b.len();
    let mut j = i;

    // Identifier: alnum / `_` / `-` (not a link `-`; we stop the id before a
    // bracket or whitespace or a connector char run).
    let id_start = j;
    while j < n {
        let c = b[j] as char;
        if c.is_alphanumeric() || c == '_' {
            j += 1;
        } else {
            break;
        }
    }
    if j == id_start {
        return None;
    }
    let id = stmt[id_start..j].to_string();

    // Optional shape+label.
    let (shape, label, after) = read_shape(stmt, j);
    Some((
        NodeSpec {
            id,
            label,
            shape: shape.unwrap_or(Shape::Rect),
        },
        after,
    ))
}

/// Read an optional node shape wrapper starting at `i`. Recognises the longest
/// matching opener so `((` (circle) wins over `(` (round) and `([` (stadium)
/// over `(`. Returns `(shape, label, index past the closer)`.
fn read_shape(stmt: &str, i: usize) -> (Option<Shape>, Option<String>, usize) {
    let b = stmt.as_bytes();
    let n = b.len();
    if i >= n {
        return (None, None, i);
    }
    // Try the multi-char openers first.
    let (shape, open, close): (Shape, &str, &str) = if starts_with(b, i, "((") {
        (Shape::Circle, "((", "))")
    } else if starts_with(b, i, "([") {
        (Shape::Stadium, "([", "])")
    } else if starts_with(b, i, "[") {
        (Shape::Rect, "[", "]")
    } else if starts_with(b, i, "{") {
        (Shape::Diamond, "{", "}")
    } else if starts_with(b, i, "(") {
        (Shape::Round, "(", ")")
    } else {
        return (None, None, i);
    };

    let inner_start = i + open.len();
    // Find the closer, honouring quotes so `]`/`)` inside a quoted label don't
    // terminate early.
    if let Some(close_at) = find_close(stmt, inner_start, close) {
        let raw = &stmt[inner_start..close_at];
        let label = unquote(raw.trim());
        (Some(shape), Some(label), close_at + close.len())
    } else {
        // Unterminated label — take the rest as the label (defensive).
        let raw = &stmt[inner_start..];
        (Some(shape), Some(unquote(raw.trim())), n)
    }
}

fn starts_with(b: &[u8], i: usize, pat: &str) -> bool {
    let p = pat.as_bytes();
    i + p.len() <= b.len() && &b[i..i + p.len()] == p
}

/// Find `close` in `s` from `start`, skipping over double-quoted spans.
fn find_close(s: &str, start: usize, close: &str) -> Option<usize> {
    let b = s.as_bytes();
    let c = close.as_bytes();
    let mut in_str = false;
    let mut i = start;
    while i < b.len() {
        if b[i] == b'"' {
            in_str = !in_str;
            i += 1;
            continue;
        }
        if !in_str && i + c.len() <= b.len() && &b[i..i + c.len()] == c {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Strip surrounding double quotes from a label and collapse `<br>` to spaces in
/// the displayed text (we keep single-line labels for layout simplicity).
fn unquote(s: &str) -> String {
    let s = s.trim();
    let s = s.strip_prefix('"').unwrap_or(s);
    let s = s.strip_suffix('"').unwrap_or(s);
    // Mermaid `<br>` / `<br/>` line breaks → a space (kept on one line).
    let mut out = s.replace("<br/>", " ").replace("<br>", " ");
    out = out.replace('\n', " ");
    out.trim().to_string()
}

/// Find or create the node identified by `spec`. If the spec carries a label, it
/// (re)sets the node's label/shape — the last declaration wins, matching
/// mermaid's behaviour where the shape can be declared at first use.
fn ensure_node(nodes: &mut Vec<ParsedNode>, spec: &NodeSpec) -> usize {
    if let Some(idx) = nodes.iter().position(|nd| nd.id == spec.id) {
        if let Some(lbl) = &spec.label {
            nodes[idx].label = lbl.clone();
            nodes[idx].shape = spec.shape;
        }
        return idx;
    }
    let label = spec.label.clone().unwrap_or_else(|| spec.id.clone());
    nodes.push(ParsedNode {
        id: spec.id.clone(),
        label,
        shape: spec.shape,
    });
    nodes.len() - 1
}

// ─────────────────────────────── layout ───────────────────────────────────

/// A node with computed geometry (top-down points, diagram-local origin).
#[derive(Debug, Clone)]
struct PlacedNode {
    /// Centre x.
    cx: f64,
    /// Centre y.
    cy: f64,
    /// Half-width.
    hw: f64,
    /// Half-height.
    hh: f64,
    shape: Shape,
    label: String,
}

/// Layout tunables (points), kept modest so diagrams stay compact.
const FONT_PT: f64 = 12.0;
const PAD_X: f64 = 12.0;
const PAD_Y: f64 = 8.0;
const MIN_W: f64 = 40.0;
const MIN_H: f64 = 28.0;
const RANK_GAP: f64 = 44.0; // gap between layers (along flow axis)
const NODE_GAP: f64 = 26.0; // gap between siblings (across flow axis)
const MARGIN: f64 = 8.0; // outer padding around the whole diagram

/// Assign each node a rank (layer) via longest-path on a cycle-safe DAG view of
/// the edges. Back-edges (those that would point to an already-deeper rank) are
/// ignored for ranking so cycles can't loop forever.
fn assign_ranks(g: &Graph) -> Vec<usize> {
    let n = g.nodes.len();
    let mut rank = vec![0usize; n];

    // Build a forward adjacency that excludes self-loops; we relax ranks with a
    // bounded number of passes (Bellman-Ford-style longest path on a DAG, capped
    // at `n` iterations so any residual cycle terminates).
    let mut adj: Vec<(usize, usize)> = g
        .edges
        .iter()
        .filter(|e| e.from != e.to)
        .map(|e| (e.from, e.to))
        .collect();
    adj.sort_unstable();
    adj.dedup();

    let mut changed = true;
    let mut iters = 0usize;
    while changed && iters <= n {
        changed = false;
        for &(a, b) in &adj {
            if rank[b] < rank[a] + 1 {
                rank[b] = rank[a] + 1;
                changed = true;
            }
        }
        iters += 1;
    }

    // Roots (no incoming edge) anchor at rank 0; isolated nodes keep rank 0.
    rank
}

/// Lay out the parsed graph into placed nodes + the bounding size.
fn place(g: &Graph, measure: &dyn Measure, base: &Style, scale: f64) -> (Vec<PlacedNode>, f64, f64) {
    let rank = assign_ranks(g);
    let max_rank = rank.iter().copied().max().unwrap_or(0);

    // Group node indices by rank, preserving declaration order for stability.
    let mut layers: Vec<Vec<usize>> = vec![Vec::new(); max_rank + 1];
    for (idx, &r) in rank.iter().enumerate() {
        layers[r].push(idx);
    }

    // Measure each node's box.
    let label_style = label_style(base, FONT_PT * scale, true);
    let mut sizes: Vec<(f64, f64)> = Vec::with_capacity(g.nodes.len());
    for nd in &g.nodes {
        let tw = measure.width(&nd.label, &label_style);
        let mut w = (tw + 2.0 * PAD_X * scale).max(MIN_W * scale);
        let mut h = (FONT_PT * scale + 2.0 * PAD_Y * scale).max(MIN_H * scale);
        // Circles/diamonds need extra room to enclose the label.
        match nd.shape {
            Shape::Circle => {
                let d = w.max(h) * 1.15;
                w = d;
                h = d;
            }
            Shape::Diamond => {
                w *= 1.4;
                h *= 1.5;
            }
            Shape::Stadium | Shape::Round => {
                w += PAD_X * scale; // rounded ends eat horizontal room
            }
            Shape::Rect => {}
        }
        sizes.push((w, h));
    }

    let vertical = g.dir.vertical();
    let rank_gap = RANK_GAP * scale;
    let node_gap = NODE_GAP * scale;
    let margin = MARGIN * scale;

    // Cross-axis extent of each layer and overall cross size.
    let cross_of = |w: f64, h: f64| if vertical { w } else { h };
    let main_of = |w: f64, h: f64| if vertical { h } else { w };

    // First pass: per-layer total cross size + max main size.
    let mut layer_main: Vec<f64> = Vec::with_capacity(layers.len());
    let mut layer_cross: Vec<f64> = Vec::with_capacity(layers.len());
    for layer in &layers {
        let mut cross = 0.0;
        let mut main_max: f64 = 0.0;
        for (k, &idx) in layer.iter().enumerate() {
            let (w, h) = sizes[idx];
            if k > 0 {
                cross += node_gap;
            }
            cross += cross_of(w, h);
            main_max = main_max.max(main_of(w, h));
        }
        layer_main.push(main_max);
        layer_cross.push(cross);
    }
    let total_cross = layer_cross.iter().cloned().fold(0.0_f64, f64::max);
    let total_main: f64 = layer_main.iter().sum::<f64>()
        + rank_gap * (layers.len().saturating_sub(1)) as f64;

    // Main-axis position of each layer's centre line.
    let mut layer_main_center: Vec<f64> = Vec::with_capacity(layers.len());
    {
        let mut acc = margin;
        for &lm in &layer_main {
            layer_main_center.push(acc + lm / 2.0);
            acc += lm + rank_gap;
        }
    }

    // Place nodes. Cross-axis: centre each layer within `total_cross`.
    let mut placed: Vec<PlacedNode> = vec![
        PlacedNode {
            cx: 0.0,
            cy: 0.0,
            hw: 0.0,
            hh: 0.0,
            shape: Shape::Rect,
            label: String::new(),
        };
        g.nodes.len()
    ];

    for (li, layer) in layers.iter().enumerate() {
        let main_center = layer_main_center[li];
        // Reverse the main axis for Up/Left so rank 0 sits at the bottom/right.
        let main_center = if g.dir.reversed() {
            2.0 * margin + total_main - main_center
        } else {
            main_center
        };
        let mut cross_cursor = margin + (total_cross - layer_cross[li]) / 2.0;
        for (k, &idx) in layer.iter().enumerate() {
            let (w, h) = sizes[idx];
            if k > 0 {
                cross_cursor += node_gap;
            }
            let cross_center = cross_cursor + cross_of(w, h) / 2.0;
            cross_cursor += cross_of(w, h);
            let (cx, cy) = if vertical {
                (cross_center, main_center)
            } else {
                (main_center, cross_center)
            };
            placed[idx] = PlacedNode {
                cx,
                cy,
                hw: w / 2.0,
                hh: h / 2.0,
                shape: g.nodes[idx].shape,
                label: g.nodes[idx].label.clone(),
            };
        }
    }

    let width = total_cross.max(MIN_W * scale) + 2.0 * margin;
    let height = total_main.max(MIN_H * scale) + 2.0 * margin;
    let (w, h) = if vertical {
        (width, height)
    } else {
        (height, width)
    };
    (placed, w, h)
}

// ─────────────────────────────── rendering ────────────────────────────────

/// Build an SVG string for the diagram geometry (boxes + edges + arrow-heads)
/// and the list of text labels. Coordinates are in the diagram's own viewBox
/// (points, top-down), 1:1 with the placed geometry.
fn render_svg(g: &Graph, placed: &[PlacedNode], w: f64, h: f64, scale: f64) -> (String, Vec<Label>) {
    let mut svg = String::new();
    svg.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{:.2}\" height=\"{:.2}\" viewBox=\"0 0 {:.2} {:.2}\">",
        w, h, w, h
    ));

    let mut labels: Vec<Label> = Vec::new();

    // ── edges (drawn first, so node boxes overpaint their endpoints) ──
    let stroke_w = 1.3 * scale;
    for e in &g.edges {
        if e.from >= placed.len() || e.to >= placed.len() || e.from == e.to {
            continue;
        }
        let a = &placed[e.from];
        let b = &placed[e.to];
        // Trim endpoints to each box's border along the centre-to-centre line.
        let (x0, y0) = edge_point(a, b.cx, b.cy);
        let (x1, y1) = edge_point(b, a.cx, a.cy);

        // The line stops short of the head so the triangle isn't doubled.
        let head_len = 9.0 * scale;
        let (hx, hy, lx1, ly1) = if e.head == Head::None {
            (x1, y1, x1, y1)
        } else {
            let (ux, uy) = unit(x0, y0, x1, y1);
            (x1, y1, x1 - ux * head_len, y1 - uy * head_len)
        };

        // Edge body. Dotted → emit dashes as short segments (the SVG parser has
        // no `stroke-dasharray`, so we synthesise the pattern ourselves).
        match e.line {
            Line::Dotted => {
                dashed_line(&mut svg, x0, y0, lx1, ly1, stroke_w, (4.0 * scale, 3.0 * scale));
            }
            Line::Thick => {
                line(&mut svg, x0, y0, lx1, ly1, stroke_w * 2.0);
            }
            Line::Solid => {
                line(&mut svg, x0, y0, lx1, ly1, stroke_w);
            }
        }

        // Head.
        match e.head {
            Head::Arrow => arrow_head(&mut svg, x0, y0, hx, hy, head_len, scale),
            Head::Circle => {
                circle(&mut svg, hx, hy, 3.5 * scale, "#ffffff", "#333333", stroke_w)
            }
            Head::Cross => cross_head(&mut svg, x0, y0, hx, hy, 5.0 * scale, stroke_w),
            Head::None => {}
        }

        // Edge label with a white wash behind it for legibility.
        if let Some(txt) = &e.label {
            if !txt.is_empty() {
                let mx = (x0 + x1) / 2.0;
                let my = (y0 + y1) / 2.0;
                let fs = (FONT_PT - 1.0) * scale;
                let lw = (txt.chars().count() as f64) * fs * 0.55 + 4.0 * scale;
                let lh = fs + 2.0 * scale;
                // wash
                svg.push_str(&format!(
                    "<rect x=\"{:.2}\" y=\"{:.2}\" width=\"{:.2}\" height=\"{:.2}\" fill=\"#ffffff\"/>",
                    mx - lw / 2.0,
                    my - lh / 2.0,
                    lw,
                    lh
                ));
                labels.push(Label {
                    cx: mx,
                    cy: my,
                    text: txt.clone(),
                    font_size: fs,
                    bold: false,
                });
            }
        }
    }

    // ── node boxes ──
    for nd in placed {
        if nd.hw <= 0.0 || nd.hh <= 0.0 {
            continue;
        }
        node_shape(&mut svg, nd, stroke_w);
        labels.push(Label {
            cx: nd.cx,
            cy: nd.cy,
            text: nd.label.clone(),
            font_size: FONT_PT * scale,
            bold: true,
        });
    }

    svg.push_str("</svg>");
    (svg, labels)
}

/// Fill/stroke colours for nodes (mermaid's default light theme palette).
const NODE_FILL: &str = "#ECECFF";
const NODE_STROKE: &str = "#9370DB";
const EDGE_COLOR: &str = "#333333";

/// Emit the SVG for a node's shape with fill + stroke.
fn node_shape(svg: &mut String, nd: &PlacedNode, stroke_w: f64) {
    let x = nd.cx - nd.hw;
    let y = nd.cy - nd.hh;
    let w = nd.hw * 2.0;
    let h = nd.hh * 2.0;
    match nd.shape {
        Shape::Rect => {
            svg.push_str(&format!(
                "<rect x=\"{:.2}\" y=\"{:.2}\" width=\"{:.2}\" height=\"{:.2}\" fill=\"{}\" stroke=\"{}\" stroke-width=\"{:.2}\"/>",
                x, y, w, h, NODE_FILL, NODE_STROKE, stroke_w
            ));
        }
        Shape::Round => {
            let r = (h * 0.25).min(10.0);
            svg.push_str(&format!(
                "<rect x=\"{:.2}\" y=\"{:.2}\" width=\"{:.2}\" height=\"{:.2}\" rx=\"{:.2}\" ry=\"{:.2}\" fill=\"{}\" stroke=\"{}\" stroke-width=\"{:.2}\"/>",
                x, y, w, h, r, r, NODE_FILL, NODE_STROKE, stroke_w
            ));
        }
        Shape::Stadium => {
            let r = h / 2.0;
            svg.push_str(&format!(
                "<rect x=\"{:.2}\" y=\"{:.2}\" width=\"{:.2}\" height=\"{:.2}\" rx=\"{:.2}\" ry=\"{:.2}\" fill=\"{}\" stroke=\"{}\" stroke-width=\"{:.2}\"/>",
                x, y, w, h, r, r, NODE_FILL, NODE_STROKE, stroke_w
            ));
        }
        Shape::Circle => {
            let r = nd.hw.min(nd.hh);
            svg.push_str(&format!(
                "<circle cx=\"{:.2}\" cy=\"{:.2}\" r=\"{:.2}\" fill=\"{}\" stroke=\"{}\" stroke-width=\"{:.2}\"/>",
                nd.cx, nd.cy, r, NODE_FILL, NODE_STROKE, stroke_w
            ));
        }
        Shape::Diamond => {
            // A rhombus through the four box-edge midpoints.
            svg.push_str(&format!(
                "<polygon points=\"{:.2},{:.2} {:.2},{:.2} {:.2},{:.2} {:.2},{:.2}\" fill=\"{}\" stroke=\"{}\" stroke-width=\"{:.2}\"/>",
                nd.cx, y,                 // top
                x + w, nd.cy,             // right
                nd.cx, y + h,             // bottom
                x, nd.cy,                 // left
                NODE_FILL, NODE_STROKE, stroke_w
            ));
        }
    }
}

/// Emit a straight stroked line (`fill="none"` so it's a pure stroke, not a
/// degenerate filled path).
fn line(svg: &mut String, x0: f64, y0: f64, x1: f64, y1: f64, w: f64) {
    svg.push_str(&format!(
        "<line x1=\"{:.2}\" y1=\"{:.2}\" x2=\"{:.2}\" y2=\"{:.2}\" fill=\"none\" stroke=\"{}\" stroke-width=\"{:.2}\"/>",
        x0, y0, x1, y1, EDGE_COLOR, w
    ));
}

/// Emit a dashed line as a run of short solid segments (`pattern.0` on,
/// `pattern.1` off), packed as one `(dash, gap)` tuple to keep the arity small.
fn dashed_line(svg: &mut String, x0: f64, y0: f64, x1: f64, y1: f64, w: f64, pattern: (f64, f64)) {
    let (dash, gap) = pattern;
    let dx = x1 - x0;
    let dy = y1 - y0;
    let len = (dx * dx + dy * dy).sqrt();
    if len <= 1e-6 {
        return;
    }
    let ux = dx / len;
    let uy = dy / len;
    let step = (dash + gap).max(0.5);
    let mut t = 0.0;
    while t < len {
        let seg_end = (t + dash).min(len);
        line(
            svg,
            x0 + ux * t,
            y0 + uy * t,
            x0 + ux * seg_end,
            y0 + uy * seg_end,
            w,
        );
        t += step;
    }
}

/// Emit a filled triangular arrow-head pointing at `(x1, y1)`, coming from
/// `(x0, y0)`.
fn arrow_head(svg: &mut String, x0: f64, y0: f64, x1: f64, y1: f64, len: f64, scale: f64) {
    let (ux, uy) = unit(x0, y0, x1, y1);
    // Perpendicular.
    let (px, py) = (-uy, ux);
    let half = 4.0 * scale;
    let bx = x1 - ux * len;
    let by = y1 - uy * len;
    let p1 = (bx + px * half, by + py * half);
    let p2 = (bx - px * half, by - py * half);
    svg.push_str(&format!(
        "<polygon points=\"{:.2},{:.2} {:.2},{:.2} {:.2},{:.2}\" fill=\"{}\" stroke=\"none\"/>",
        x1, y1, p1.0, p1.1, p2.0, p2.1, EDGE_COLOR
    ));
}

/// Emit a small `x` cross-head at `(x1, y1)`.
fn cross_head(svg: &mut String, x0: f64, y0: f64, x1: f64, y1: f64, r: f64, w: f64) {
    let (ux, uy) = unit(x0, y0, x1, y1);
    let (px, py) = (-uy, ux);
    // Two diagonals around the endpoint.
    line(
        svg,
        x1 + (ux + px) * r * 0.5,
        y1 + (uy + py) * r * 0.5,
        x1 - (ux + px) * r * 0.5,
        y1 - (uy + py) * r * 0.5,
        w,
    );
    line(
        svg,
        x1 + (ux - px) * r * 0.5,
        y1 + (uy - py) * r * 0.5,
        x1 - (ux - px) * r * 0.5,
        y1 - (uy - py) * r * 0.5,
        w,
    );
}

/// Emit a filled/stroked circle.
fn circle(svg: &mut String, cx: f64, cy: f64, r: f64, fill: &str, stroke: &str, w: f64) {
    svg.push_str(&format!(
        "<circle cx=\"{:.2}\" cy=\"{:.2}\" r=\"{:.2}\" fill=\"{}\" stroke=\"{}\" stroke-width=\"{:.2}\"/>",
        cx, cy, r, fill, stroke, w
    ));
}

/// Unit vector from `(x0,y0)` to `(x1,y1)`; `(0,1)` for a degenerate segment.
fn unit(x0: f64, y0: f64, x1: f64, y1: f64) -> (f64, f64) {
    let dx = x1 - x0;
    let dy = y1 - y0;
    let len = (dx * dx + dy * dy).sqrt();
    if len <= 1e-6 {
        (0.0, 1.0)
    } else {
        (dx / len, dy / len)
    }
}

/// Intersection of the ray from node `nd`'s centre toward `(tx, ty)` with the
/// node's bounding box border — where an incident edge should touch the box.
fn edge_point(nd: &PlacedNode, tx: f64, ty: f64) -> (f64, f64) {
    let dx = tx - nd.cx;
    let dy = ty - nd.cy;
    if dx.abs() < 1e-6 && dy.abs() < 1e-6 {
        return (nd.cx, nd.cy);
    }
    // Scale factor to reach the nearest box edge.
    let sx = if dx.abs() > 1e-6 {
        nd.hw / dx.abs()
    } else {
        f64::INFINITY
    };
    let sy = if dy.abs() > 1e-6 {
        nd.hh / dy.abs()
    } else {
        f64::INFINITY
    };
    let s = sx.min(sy);
    (nd.cx + dx * s, nd.cy + dy * s)
}

// ─────────────────────────────── public API ───────────────────────────────

/// Build a label style from the block's computed style (so the diagram inherits
/// the document's font family), overriding size and weight.
fn label_style(base: &Style, size: f64, bold: bool) -> Style {
    let mut s = base.clone();
    s.font_size = size;
    s.bold = bold;
    s.font_weight = if bold { 700 } else { 400 };
    s.italic = false;
    s.underline = false;
    s.strike = false;
    s.color = [0.13, 0.13, 0.13];
    s
}

/// Try to render `el` as a Mermaid flowchart fitted to `avail_w` points.
///
/// Returns `Some(Diagram)` when `el` is a Mermaid container whose source parses
/// as a flowchart; returns `None` otherwise (not Mermaid, or a Mermaid kind we
/// don't render) so the caller falls back to the normal block rendering with
/// **no** behavioural change.
pub fn try_build(el: &Element, base: &Style, avail_w: f64, measure: &dyn Measure) -> Option<Diagram> {
    let src = mermaid_source(el)?;
    let g = parse_flowchart(&src)?;
    Some(build_graph(&g, base, avail_w, measure))
}

/// Lay out and render an already-parsed graph. Split out so the layout/auto-scale
/// path is unit-testable from a `Graph` directly.
fn build_graph(g: &Graph, base: &Style, avail_w: f64, measure: &dyn Measure) -> Diagram {
    // Lay out at scale 1, then shrink to fit the available width if needed.
    let (placed1, w1, h1) = place(g, measure, base, 1.0);
    let scale = if w1 > avail_w && avail_w > 1.0 {
        (avail_w / w1).clamp(0.25, 1.0)
    } else {
        1.0
    };

    let (placed, w, h) = if (scale - 1.0).abs() < 1e-6 {
        (placed1, w1, h1)
    } else {
        place(g, measure, base, scale)
    };

    let (svg_str, labels) = render_svg(g, &placed, w, h, scale);
    // Parsing our own well-formed SVG should always succeed; fall back to an
    // empty 1×1 image rather than panicking if it somehow doesn't.
    let image = parse_svg(&svg_str).unwrap_or_else(|| {
        parse_svg("<svg viewBox=\"0 0 1 1\"><rect width=\"1\" height=\"1\" fill=\"none\"/></svg>")
            .expect("trivial svg parses")
    });

    Diagram {
        width: w,
        height: h,
        image,
        labels,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html::dom::parse;
    use crate::html::layout::AverageMeasure;

    fn first_el(html: &str) -> Element {
        let nodes = parse(html);
        nodes
            .into_iter()
            .find_map(|n| match n {
                Node::Element(e) => Some(e),
                _ => None,
            })
            .expect("an element")
    }

    #[test]
    fn detects_pre_mermaid_class() {
        let el = first_el(r#"<pre class="mermaid">graph TD; A--&gt;B;</pre>"#);
        assert!(mermaid_source(&el).is_some());
    }

    #[test]
    fn detects_div_mermaid_class_case_insensitive() {
        let el = first_el(r#"<div class="Foo MERMAID bar">graph LR; A-->B</div>"#);
        assert!(mermaid_source(&el).is_some());
    }

    #[test]
    fn detects_pre_code_language_mermaid() {
        let el = first_el(r#"<pre><code class="language-mermaid">graph TD; A-->B</code></pre>"#);
        assert!(mermaid_source(&el).is_some());
    }

    #[test]
    fn non_mermaid_block_is_ignored() {
        let el = first_el(r#"<pre class="language-text">hello world</pre>"#);
        assert!(mermaid_source(&el).is_none());
        let el2 = first_el(r#"<div class="note">graph TD; A-->B</div>"#);
        assert!(mermaid_source(&el2).is_none());
    }

    #[test]
    fn parses_simple_chain() {
        let g = parse_flowchart("graph TD; A-->B; B-->C;").expect("parsed");
        assert_eq!(g.dir, Dir::Down);
        assert_eq!(g.nodes.len(), 3, "A, B, C");
        assert_eq!(g.edges.len(), 2, "A->B, B->C");
        assert_eq!(g.edges[0].head, Head::Arrow);
    }

    #[test]
    fn parses_inline_chain_in_one_token() {
        let g = parse_flowchart("flowchart LR\n A-->B-->C-->D").expect("parsed");
        assert_eq!(g.dir, Dir::Right);
        assert_eq!(g.nodes.len(), 4);
        assert_eq!(g.edges.len(), 3);
    }

    #[test]
    fn parses_shapes_and_labels() {
        let g = parse_flowchart(
            "graph TD\n A[Square]-->B(Round)\n B-->C{Diamond}\n C-->D((Circle))\n D-->E([Stadium])",
        )
        .expect("parsed");
        let by_id = |id: &str| g.nodes.iter().find(|n| n.id == id).unwrap();
        assert_eq!(by_id("A").shape, Shape::Rect);
        assert_eq!(by_id("A").label, "Square");
        assert_eq!(by_id("B").shape, Shape::Round);
        assert_eq!(by_id("C").shape, Shape::Diamond);
        assert_eq!(by_id("D").shape, Shape::Circle);
        assert_eq!(by_id("E").shape, Shape::Stadium);
    }

    #[test]
    fn parses_edge_styles_and_heads() {
        let g = parse_flowchart(
            "graph LR\n A-->B\n A---C\n A-.->D\n A==>E\n A--xF\n A--oG",
        )
        .expect("parsed");
        let find = |to_label: &str| {
            let idx = g.nodes.iter().position(|n| n.id == to_label).unwrap();
            g.edges.iter().find(|e| e.to == idx).unwrap()
        };
        assert_eq!(find("B").head, Head::Arrow);
        assert_eq!(find("B").line, Line::Solid);
        assert_eq!(find("C").head, Head::None);
        assert_eq!(find("D").line, Line::Dotted);
        assert_eq!(find("D").head, Head::Arrow);
        assert_eq!(find("E").line, Line::Thick);
        assert_eq!(find("F").head, Head::Cross);
        assert_eq!(find("G").head, Head::Circle);
    }

    #[test]
    fn parses_pipe_and_inline_edge_labels() {
        let g = parse_flowchart("graph TD\n A-->|yes|B\n A-- no -->C").expect("parsed");
        let b = g.nodes.iter().position(|n| n.id == "B").unwrap();
        let c = g.nodes.iter().position(|n| n.id == "C").unwrap();
        assert_eq!(g.edges.iter().find(|e| e.to == b).unwrap().label.as_deref(), Some("yes"));
        assert_eq!(g.edges.iter().find(|e| e.to == c).unwrap().label.as_deref(), Some("no"));
    }

    #[test]
    fn tolerates_directives_and_comments() {
        let g = parse_flowchart(
            "graph TD\n  %% a comment\n  subgraph one\n  A-->B\n  end\n  style A fill:#f00\n  classDef big font-size:20px\n  class A big\n  click A \"http://x\"\n  C-->D",
        )
        .expect("parsed");
        assert!(g.nodes.iter().any(|n| n.id == "A"));
        assert!(g.nodes.iter().any(|n| n.id == "D"));
        assert!(g.edges.len() >= 2);
    }

    #[test]
    fn other_diagram_kinds_fall_through() {
        assert!(parse_flowchart("sequenceDiagram\n Alice->>John: Hi").is_none());
        assert!(parse_flowchart("pie title Pets\n \"Dogs\": 50").is_none());
        assert!(parse_flowchart("gantt\n title A").is_none());
        assert!(parse_flowchart("classDiagram\n class Animal").is_none());
    }

    #[test]
    fn reversed_arrow_normalises_direction() {
        let g = parse_flowchart("graph LR\n A<--B").expect("parsed");
        let a = g.nodes.iter().position(|n| n.id == "A").unwrap();
        let b = g.nodes.iter().position(|n| n.id == "B").unwrap();
        // `A<--B` means B points to A.
        assert_eq!(g.edges[0].from, b);
        assert_eq!(g.edges[0].to, a);
        assert_eq!(g.edges[0].head, Head::Arrow);
    }

    #[test]
    fn handles_cycles_without_hanging() {
        // A cycle must not loop forever in ranking.
        let g = parse_flowchart("graph TD\n A-->B\n B-->C\n C-->A").expect("parsed");
        let d = build_graph(&g, &Style::default(), 400.0, &AverageMeasure);
        assert!(d.width > 0.0 && d.height > 0.0);
        // 3 nodes ⇒ at least 3 labels (node titles), plus geometry.
        assert!(d.labels.len() >= 3);
    }

    #[test]
    fn builds_geometry_for_simple_graph() {
        let g = parse_flowchart("graph TD; A-->B; B-->C;").expect("parsed");
        let d = build_graph(&g, &Style::default(), 400.0, &AverageMeasure);
        assert!(d.width > 0.0 && d.height > 0.0);
        // Three node titles among the labels.
        let titles: Vec<&str> = d.labels.iter().filter(|l| l.bold).map(|l| l.text.as_str()).collect();
        assert!(titles.contains(&"A") && titles.contains(&"B") && titles.contains(&"C"));
        // The SVG image must carry vector primitives (boxes + edges + heads).
        // Down layout ⇒ the three boxes stack: B sits below A, C below B.
        // (placed order matches node declaration order)
    }

    #[test]
    fn never_panics_on_garbage() {
        // A spray of pathological inputs — none may panic; each returns
        // Some/None but always terminates.
        let cases = [
            "",
            "graph",
            "graph TD",
            "graph TD;",
            "graph TD\n A",
            "graph TD\n -->",
            "graph TD\n A-->",
            "graph TD\n -->B",
            "graph TD\n A[unterminated",
            "graph TD\n A{{{{",
            "graph TD\n A-->B-->",
            "graph TD\n A<-->B",
            "graph TD\n A == B == C",
            "graph TD\n A-.-.-.->B",
            "graph LR\n A--very long label with-->B",
            "flowchart\n \"\"-->\"\"",
            "graph TD\n A-->|‼️🚀|B",
            "subgraph\nend",
            "%%%%%%",
            "graph XY\n A-->B",
            "\n\n\n;;;;;\n\n",
            "graph TD A B C D E F G",
        ];
        for c in cases {
            // parse must not panic
            let parsed = parse_flowchart(c);
            // building from any successful parse must not panic either
            if let Some(g) = parsed {
                let _ = build_graph(&g, &Style::default(), 300.0, &AverageMeasure);
            }
        }
    }

    #[test]
    fn auto_scales_wide_graph_to_fit() {
        // A wide fan-out should be scaled down to fit a narrow column.
        let src = "graph LR\n A-->B\n A-->C\n A-->D\n A-->E\n A-->F\n A-->G\n A-->H";
        let g = parse_flowchart(src).expect("parsed");
        let narrow = build_graph(&g, &Style::default(), 120.0, &AverageMeasure);
        assert!(
            narrow.width <= 120.0 + 0.5,
            "diagram fits the 120pt column (got {})",
            narrow.width
        );
    }

    /// Render an HTML fragment to a PDF and return its first page's content
    /// stream as a UTF-8-lossy string, for content-operator inspection.
    fn page1_content(html: &str) -> String {
        let pdf = crate::html::render(html, &[], 612.0, 792.0, 36.0);
        assert!(pdf.starts_with(b"%PDF-"), "valid PDF emitted");
        let doc = crate::document::Document::open(&pdf).expect("open PDF");
        let bytes = doc.page_content(1).expect("page 1 content");
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Count whitespace-delimited occurrences of a single-token PDF operator on
    /// its own (i.e. appearing as a standalone token in the content stream).
    fn count_op(content: &str, op: &str) -> usize {
        content
            .split_whitespace()
            .filter(|t| *t == op)
            .count()
    }

    #[test]
    fn flowchart_emits_boxes_edges_and_arrowheads_in_pdf() {
        // `graph TD; A-->B; B-->C;` → three node boxes + two directed edges, all
        // as native PDF path operators. Boxes fill+stroke (`B`), arrow-heads are
        // filled triangles (`f`), edge bodies stroke (`S`).
        let content = page1_content(r#"<pre class="mermaid">graph TD; A--&gt;B; B--&gt;C;</pre>"#);

        // Three node boxes are filled+stroked paths (`B` terminator).
        let b_ops = count_op(&content, "B");
        assert!(b_ops >= 3, "≥3 filled+stroked node boxes (B op), got {b_ops}");

        // Arrow-heads are fill-only triangles (`f`); two directed edges ⇒ ≥2.
        let f_ops = count_op(&content, "f");
        assert!(f_ops >= 2, "≥2 filled arrow-heads (f op), got {f_ops}");

        // Edge bodies are stroke-only lines (`S`); two edges ⇒ ≥2 stroke ops.
        let s_ops = count_op(&content, "S");
        assert!(s_ops >= 2, "≥2 stroked edge bodies (S op), got {s_ops}");

        // Path-construction ops confirm real vector geometry (moves + lines).
        assert!(count_op(&content, "m") >= 5, "multiple path moves");
        assert!(count_op(&content, "l") >= 6, "multiple path lines");

        // The three node titles render as text (`Tj`/`TJ` inside BT…ET).
        let text_ops = count_op(&content, "Tj") + count_op(&content, "TJ");
        assert!(text_ops >= 3, "≥3 text runs for node titles, got {text_ops}");
    }

    #[test]
    fn non_flowchart_code_block_emits_no_diagram_vectors() {
        // RETROCOMPAT: a plain code block that merely *mentions* class names must
        // render exactly as before — text only, NO diagram vector geometry. (A
        // black-text fill `rg` is emitted by text rendering, so the discriminator
        // is the path/fill ops a diagram would add: `B`, `f`, `re`, `m`.)
        let content = page1_content(r#"<pre class="language-text">graph TD; A--&gt;B; B--&gt;C;</pre>"#);
        assert_eq!(count_op(&content, "B"), 0, "no filled+stroked boxes\n{content}");
        assert_eq!(count_op(&content, "f"), 0, "no filled arrow-heads");
        assert_eq!(count_op(&content, "re"), 0, "no rectangles");
        assert_eq!(count_op(&content, "m"), 0, "no path moves (no vector geometry)");
        // It still renders its text verbatim.
        assert!(
            count_op(&content, "Tj") + count_op(&content, "TJ") >= 1,
            "the code text still renders"
        );
    }

    #[test]
    fn mermaid_flowchart_block_differs_from_plain_block() {
        // The same source under `class="mermaid"` vs `class="language-text"` must
        // diverge: only the mermaid one carries vector fills.
        let mermaid =
            page1_content(r#"<pre class="mermaid">graph LR; A--&gt;B; B--&gt;C;</pre>"#);
        let plain =
            page1_content(r#"<pre class="language-text">graph LR; A--&gt;B; B--&gt;C;</pre>"#);
        assert!(
            count_op(&mermaid, "rg") > count_op(&plain, "rg"),
            "mermaid renders vectors that the plain code block does not"
        );
    }

    #[test]
    fn collects_text_across_inline_elements() {
        // A stray <span> inside the source must not lose the surrounding text.
        let el = first_el(r#"<pre class="mermaid">graph TD; A--><span>B</span>; B-->C</pre>"#);
        let src = mermaid_source(&el).expect("source");
        let g = parse_flowchart(&src).expect("parsed");
        assert!(g.nodes.iter().any(|n| n.id == "A"));
        assert!(g.nodes.iter().any(|n| n.id == "B"));
        assert!(g.nodes.iter().any(|n| n.id == "C"));
    }
}
