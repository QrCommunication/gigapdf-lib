//! A practical CSS engine: parse stylesheets and inline `style`, match
//! selectors (tag / `.class` / `#id` / `*`, descendant combinator), cascade by
//! specificity + source order, and resolve inherited properties into a
//! [`Style`] per element. Covers the box-model, typography and colour
//! properties documents actually use; unknown properties are ignored, never
//! fatal.

use super::dom::{Element, Node};

/// CSS `display`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Display {
    #[default]
    Block,
    Inline,
    InlineBlock,
    ListItem,
    Table,
    TableRow,
    TableCell,
    /// `display: flex` / `inline-flex` (a basic horizontal row).
    Flex,
    /// `display: grid` / `inline-grid` (fixed-column-count grid).
    Grid,
    None,
}

/// CSS `text-align`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
    Justify,
}

/// CSS `position`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Position {
    /// `static` — normal flow (the default).
    #[default]
    Static,
    /// `relative` — laid out in flow, then shifted by `inset` (still occupies
    /// its normal space).
    Relative,
    /// `absolute` — removed from flow, positioned against the nearest
    /// positioned ancestor's content box (the containing block).
    Absolute,
    /// `fixed` — removed from flow, positioned against the page box.
    Fixed,
}

/// CSS `align-items` / `align-self` cross-axis alignment (basic flex).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AlignItems {
    /// `stretch` — items fill the cross dimension (the default).
    #[default]
    Stretch,
    /// `flex-start` / `start`.
    Start,
    /// `center`.
    Center,
    /// `flex-end` / `end`.
    End,
}

/// Four-sided lengths in points.
#[derive(Debug, Clone, Copy, Default)]
pub struct Edges {
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
    pub left: f64,
}

impl Edges {
    fn all(v: f64) -> Edges {
        Edges {
            top: v,
            right: v,
            bottom: v,
            left: v,
        }
    }
}

/// A fully-resolved computed style for one element.
#[derive(Debug, Clone)]
pub struct Style {
    pub display: Display,
    pub color: [f64; 3],
    pub background: Option<[f64; 3]>,
    pub font_size: f64,
    pub font_family: String,
    pub generic_serif: bool,
    pub generic_mono: bool,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub align: Align,
    /// `text-transform` applied to rendered text (inherited).
    pub text_transform: TextTransform,
    pub margin: Edges,
    pub padding: Edges,
    pub border_width: Edges,
    pub border_color: [f64; 3],
    pub width: Option<Len>,
    pub line_height: f64,
    pub pre: bool,
    /// `page-break-before: always` / `break-before: page` — start a new page
    /// before this block.
    pub page_break_before: bool,
    /// `page-break-after: always` / `break-after: page` — start a new page
    /// after this block.
    pub page_break_after: bool,
    /// `flex-direction: column` (else row).
    pub flex_column: bool,
    /// `justify-content` along the main axis.
    pub justify: Justify,
    /// `flex` / `flex-grow` factor (a flex item's share of free space).
    pub flex_grow: f64,
    /// `grid-template-columns` → number of columns (0 = not a grid).
    pub grid_columns: usize,
    // ── decorations / visibility (inherited) ──
    /// `text-decoration: line-through` — struck-through text.
    pub strike: bool,
    /// `text-decoration: overline`.
    pub overline: bool,
    /// `visibility: hidden` — occupies space but isn't painted.
    pub hidden: bool,
    /// `opacity` (0..=1, inherited) — alpha applied to fills and text.
    pub opacity: f64,
    /// `text-indent` of the first line, in points (inherited).
    pub text_indent: f64,
    /// `list-style-type` for list-item markers (inherited).
    pub list_style: ListStyle,
    // ── box sizing (not inherited) ──
    /// `min-width` / `max-width` clamps on the box width.
    pub min_width: Option<Len>,
    pub max_width: Option<Len>,
    /// `height` / `min-height` — a minimum block height in points.
    pub min_height: Option<f64>,
    /// `box-sizing: border-box` — `width` includes padding + border.
    pub border_box: bool,
    // ── positioning (not inherited) ──
    /// `position` scheme.
    pub position: Position,
    /// `top`, `right`, `bottom`, `left` offsets (each optional), in points or
    /// percentages of the containing block.
    pub inset: [Option<Len>; 4],
    /// `z-index` paint order (higher paints later/on top). 0 by default.
    pub z_index: i32,
    /// `overflow: hidden|clip|scroll|auto` — clip descendants to this box.
    pub overflow_clip: bool,
    // ── flex extras (not inherited) ──
    /// `flex-wrap: wrap|wrap-reverse` — allow flex lines to wrap.
    pub flex_wrap: bool,
    /// `align-items` on a flex container (cross-axis alignment of items).
    pub align_items: AlignItems,
    /// `align-self` on a flex item (overrides the container's `align-items`).
    pub align_self: Option<AlignItems>,
    /// `order` — visual reordering of flex items (lower comes first).
    pub order: i32,
    // ── grid extras (not inherited) ──
    /// `grid-template-rows` → explicit row count (0 = auto-flow rows).
    pub grid_rows: usize,
    /// `row-gap` / `gap` — vertical gutter between grid/flex tracks (points).
    pub gap_row: f64,
    /// `column-gap` / `gap` — horizontal gutter between tracks (points).
    pub gap_col: f64,
    /// `grid-column` start (1-based; 0 = auto-flow). Basic line placement.
    pub grid_col_start: usize,
    /// `grid-row` start (1-based; 0 = auto-flow).
    pub grid_row_start: usize,
    // ── typography extras ──
    /// `letter-spacing` added between characters (points, inherited).
    pub letter_spacing: f64,
    /// `word-spacing` added at spaces (points, inherited).
    pub word_spacing: f64,
    /// `float` direction, if any — floated boxes are taken beside inline flow.
    pub float: FloatSide,
}

/// CSS `float` side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FloatSide {
    /// `none` (the default).
    #[default]
    None,
    Left,
    Right,
}

/// `list-style-type` marker styles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ListStyle {
    /// `disc` (default for `ul`): •
    #[default]
    Disc,
    /// `circle`: ◦
    Circle,
    /// `square`: ▪
    Square,
    /// `decimal` (default for `ol`): 1. 2. 3.
    Decimal,
    /// `lower-alpha` / `lower-latin`: a. b. c.
    LowerAlpha,
    /// `upper-alpha` / `upper-latin`: A. B. C.
    UpperAlpha,
    /// `lower-roman`: i. ii. iii.
    LowerRoman,
    /// `upper-roman`: I. II. III.
    UpperRoman,
    /// `none`: no marker.
    None,
}

/// `text-transform` — how text is cased when rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextTransform {
    #[default]
    None,
    /// `uppercase`.
    Upper,
    /// `lowercase`.
    Lower,
    /// `capitalize` — first letter of each word.
    Capitalize,
}

impl TextTransform {
    /// Apply the transform to `s` (ASCII-aware; passes other bytes through).
    pub fn apply(self, s: &str) -> String {
        match self {
            TextTransform::None => s.to_string(),
            TextTransform::Upper => s.to_uppercase(),
            TextTransform::Lower => s.to_lowercase(),
            TextTransform::Capitalize => {
                let mut out = String::with_capacity(s.len());
                let mut at_word_start = true;
                for c in s.chars() {
                    if c.is_whitespace() {
                        at_word_start = true;
                        out.push(c);
                    } else if at_word_start {
                        out.extend(c.to_uppercase());
                        at_word_start = false;
                    } else {
                        out.push(c);
                    }
                }
                out
            }
        }
    }
}

/// `justify-content` values supported by the basic flex/grid layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Justify {
    #[default]
    Start,
    Center,
    End,
    SpaceBetween,
    SpaceAround,
}

impl Default for Style {
    fn default() -> Style {
        Style {
            display: Display::Inline,
            color: [0.0, 0.0, 0.0],
            background: None,
            font_size: 16.0,
            font_family: String::new(),
            generic_serif: false,
            generic_mono: false,
            bold: false,
            italic: false,
            underline: false,
            align: Align::Left,
            text_transform: TextTransform::None,
            margin: Edges::default(),
            padding: Edges::default(),
            border_width: Edges::default(),
            border_color: [0.0, 0.0, 0.0],
            width: None,
            line_height: 1.2,
            pre: false,
            page_break_before: false,
            page_break_after: false,
            flex_column: false,
            justify: Justify::Start,
            flex_grow: 0.0,
            grid_columns: 0,
            strike: false,
            overline: false,
            hidden: false,
            opacity: 1.0,
            text_indent: 0.0,
            list_style: ListStyle::Disc,
            min_width: None,
            max_width: None,
            min_height: None,
            border_box: false,
            position: Position::Static,
            inset: [None; 4],
            z_index: 0,
            overflow_clip: false,
            flex_wrap: false,
            align_items: AlignItems::Stretch,
            align_self: None,
            order: 0,
            grid_rows: 0,
            gap_row: 0.0,
            gap_col: 0.0,
            grid_col_start: 0,
            grid_row_start: 0,
            letter_spacing: 0.0,
            word_spacing: 0.0,
            float: FloatSide::None,
        }
    }
}

/// A CSS length: absolute points or a percentage of the container.
#[derive(Debug, Clone, Copy)]
pub enum Len {
    Pt(f64),
    Percent(f64),
}

// ─── selectors ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
struct Compound {
    tag: Option<String>,
    classes: Vec<String>,
    id: Option<String>,
}

#[derive(Debug, Clone)]
struct Selector {
    /// Ancestor-to-target compound chain (descendant combinator).
    parts: Vec<Compound>,
    specificity: u32,
}

#[derive(Debug, Clone)]
struct Rule {
    selectors: Vec<Selector>,
    decls: Vec<(String, String)>,
    order: usize,
}

fn parse_compound(s: &str) -> Compound {
    let mut c = Compound::default();
    let mut chars = s.char_indices().peekable();
    let bytes = s.as_bytes();
    let mut i = 0;
    // Optional leading tag / '*'.
    if i < bytes.len() && (bytes[i].is_ascii_alphabetic() || bytes[i] == b'*') {
        let start = i;
        while i < bytes.len()
            && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-' || bytes[i] == b'*')
        {
            i += 1;
        }
        let t = &s[start..i];
        if t != "*" {
            c.tag = Some(t.to_ascii_lowercase());
        }
    }
    let _ = &mut chars;
    while i < bytes.len() {
        match bytes[i] {
            b'.' => {
                i += 1;
                let start = i;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-' || bytes[i] == b'_')
                {
                    i += 1;
                }
                c.classes.push(s[start..i].to_string());
            }
            b'#' => {
                i += 1;
                let start = i;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-' || bytes[i] == b'_')
                {
                    i += 1;
                }
                c.id = Some(s[start..i].to_string());
            }
            _ => i += 1,
        }
    }
    c
}

fn parse_selector(s: &str) -> Option<Selector> {
    let parts: Vec<Compound> = s.split_whitespace().map(parse_compound).collect();
    if parts.is_empty() {
        return None;
    }
    // Specificity: ids*100 + classes*10 + tags.
    let mut spec = 0u32;
    for p in &parts {
        if p.id.is_some() {
            spec += 100;
        }
        spec += 10 * p.classes.len() as u32;
        if p.tag.is_some() {
            spec += 1;
        }
    }
    Some(Selector {
        parts,
        specificity: spec,
    })
}

/// Parse a stylesheet body into rules.
fn parse_rules(css: &str, order_base: usize) -> Vec<Rule> {
    let css = strip_comments(css);
    let mut rules = Vec::new();
    let mut rest = css.as_str();
    let mut order = order_base;
    while let Some(brace) = rest.find('{') {
        let sel_part = rest[..brace].trim();
        let after = &rest[brace + 1..];
        let close = match after.find('}') {
            Some(c) => c,
            None => break,
        };
        let body = &after[..close];
        rest = &after[close + 1..];

        // Skip at-rules (@media, @font-face, …) — not yet interpreted.
        if sel_part.starts_with('@') {
            order += 1;
            continue;
        }
        let selectors: Vec<Selector> = sel_part.split(',').filter_map(parse_selector).collect();
        if selectors.is_empty() {
            continue;
        }
        rules.push(Rule {
            selectors,
            decls: parse_decls(body),
            order,
        });
        order += 1;
    }
    rules
}

/// Scan `@font-face { font-family: <name>; … }` blocks and return the declared
/// family names (lower-cased, de-quoted, de-duplicated, in source order).
fn collect_font_faces(css: &str) -> Vec<String> {
    let css = strip_comments(css);
    let mut names = Vec::new();
    let mut rest = css.as_str();
    while let Some(at) = rest.find("@font-face") {
        rest = &rest[at + "@font-face".len()..];
        let Some(open) = rest.find('{') else { break };
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else { break };
        let body = &after[..close];
        for (k, val) in parse_decls(body) {
            if k == "font-family" {
                let name = val.trim().trim_matches(['"', '\'']).to_ascii_lowercase();
                if !name.is_empty() && !names.contains(&name) {
                    names.push(name);
                }
            }
        }
        rest = &after[close + 1..];
    }
    names
}

fn strip_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        rest = rest[start + 2..]
            .find("*/")
            .map(|e| &rest[start + 2 + e + 2..])
            .unwrap_or("");
    }
    out.push_str(rest);
    out
}

/// Parse `prop: value; …` into pairs.
pub fn parse_decls(body: &str) -> Vec<(String, String)> {
    body.split(';')
        .filter_map(|d| {
            let (k, v) = d.split_once(':')?;
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim().to_string();
            if k.is_empty() || v.is_empty() {
                None
            } else {
                Some((k, v))
            }
        })
        .collect()
}

// ─── cascade ──────────────────────────────────────────────────────────────

fn matches(compound: &Compound, el: &Element) -> bool {
    if let Some(t) = &compound.tag {
        if &el.tag != t {
            return false;
        }
    }
    if let Some(id) = &compound.id {
        if el.attr("id") != Some(id.as_str()) {
            return false;
        }
    }
    if !compound.classes.is_empty() {
        let class_attr = el.attr("class").unwrap_or_default();
        let have: Vec<&str> = class_attr.split_whitespace().collect();
        if !compound.classes.iter().all(|c| have.contains(&c.as_str())) {
            return false;
        }
    }
    true
}

/// Does `selector` match `el` given its ancestor chain (root-first)?
fn selector_matches(selector: &Selector, el: &Element, ancestors: &[&Element]) -> bool {
    let parts = &selector.parts;
    // The last compound must match the element itself.
    if !matches(parts.last().unwrap(), el) {
        return false;
    }
    // Remaining compounds must match ancestors in order (descendant combinator).
    let mut ai = ancestors.len();
    for compound in parts[..parts.len() - 1].iter().rev() {
        let mut found = false;
        while ai > 0 {
            ai -= 1;
            if matches(compound, ancestors[ai]) {
                found = true;
                break;
            }
        }
        if !found {
            return false;
        }
    }
    true
}

/// The full stylesheet context: user-agent defaults + author rules.
#[derive(Debug)]
pub struct Stylesheet {
    rules: Vec<Rule>,
    /// Family names declared by `@font-face` rules (lower-cased), so callers
    /// can tell which families the document defines locally.
    font_faces: Vec<String>,
}

impl Stylesheet {
    /// Build from the author CSS collected from `<style>` blocks.
    pub fn new(author_css: &str) -> Stylesheet {
        let mut rules = parse_rules(UA_CSS, 0);
        rules.extend(parse_rules(author_css, 100_000));
        Stylesheet {
            rules,
            font_faces: collect_font_faces(author_css),
        }
    }

    /// Family names registered via `@font-face` (lower-cased).
    pub fn font_faces(&self) -> &[String] {
        &self.font_faces
    }

    /// Compute the style of `el` given its inherited (parent) style and its
    /// ancestor chain (root-first, excluding `el`).
    pub fn computed(&self, el: &Element, parent: &Style, ancestors: &[&Element]) -> Style {
        let mut style = inherit(parent);

        // Gather matching declarations, ordered by (specificity, source order).
        let mut hits: Vec<(&Selector, &Rule)> = Vec::new();
        for rule in &self.rules {
            for sel in &rule.selectors {
                if selector_matches(sel, el, ancestors) {
                    hits.push((sel, rule));
                    break;
                }
            }
        }
        hits.sort_by_key(|(s, r)| (s.specificity, r.order));
        for (_, rule) in hits {
            apply_decls(&mut style, &rule.decls);
        }
        // Inline `style="…"` wins over everything.
        if let Some(inline) = el.attr("style") {
            apply_decls(&mut style, &parse_decls(inline));
        }
        // Tag-driven defaults the UA sheet can't express as inheritance.
        style
    }
}

/// Reset non-inherited properties; keep the inherited ones from the parent.
fn inherit(parent: &Style) -> Style {
    Style {
        // Inherited:
        color: parent.color,
        font_size: parent.font_size,
        font_family: parent.font_family.clone(),
        generic_serif: parent.generic_serif,
        generic_mono: parent.generic_mono,
        bold: parent.bold,
        italic: parent.italic,
        underline: parent.underline,
        align: parent.align,
        text_transform: parent.text_transform,
        line_height: parent.line_height,
        pre: parent.pre,
        // Reset:
        display: Display::Inline,
        background: None,
        margin: Edges::default(),
        padding: Edges::default(),
        border_width: Edges::default(),
        border_color: parent.color,
        width: None,
        page_break_before: false,
        page_break_after: false,
        flex_column: false,
        justify: Justify::Start,
        flex_grow: 0.0,
        grid_columns: 0,
        // Inherited:
        strike: parent.strike,
        overline: parent.overline,
        hidden: parent.hidden,
        opacity: parent.opacity,
        text_indent: parent.text_indent,
        list_style: parent.list_style,
        // Inherited:
        letter_spacing: parent.letter_spacing,
        word_spacing: parent.word_spacing,
        // Reset:
        min_width: None,
        max_width: None,
        min_height: None,
        border_box: false,
        position: Position::Static,
        inset: [None; 4],
        z_index: 0,
        overflow_clip: false,
        flex_wrap: false,
        align_items: AlignItems::Stretch,
        align_self: None,
        order: 0,
        grid_rows: 0,
        gap_row: 0.0,
        gap_col: 0.0,
        grid_col_start: 0,
        grid_row_start: 0,
        float: FloatSide::None,
    }
}

fn apply_decls(style: &mut Style, decls: &[(String, String)]) {
    for (k, v) in decls {
        apply_one(style, k, v);
    }
}

/// Count the columns declared by `grid-template-columns`.
///
/// Supports the two common spellings: an explicit track list
/// (`1fr 1fr 200px` → 3) and the `repeat(N, …)` shorthand (→ N). Any other
/// value yields a single column so the grid still lays out as one stack.
fn parse_grid_columns(v: &str) -> usize {
    if let Some(rest) = v.strip_prefix("repeat(") {
        if let Some(n) = rest
            .split(',')
            .next()
            .and_then(|s| s.trim().parse::<usize>().ok())
        {
            return n.max(1);
        }
    }
    v.split_whitespace()
        .filter(|t| !t.is_empty())
        .count()
        .max(1)
}

/// Parse a `grid-column`/`grid-row` placement into a 1-based start line.
/// Supports a bare line number and `<n> / <m>` (we keep the start) and the
/// `span N` form (treated as auto: 0). `auto` / unknown ⇒ 0 (auto-flow).
fn parse_grid_line(v: &str) -> usize {
    let first = v.split('/').next().unwrap_or(v).trim();
    if first.is_empty() || first == "auto" || first.starts_with("span") {
        return 0;
    }
    first.parse::<usize>().unwrap_or(0)
}

/// Parse an `align-items` / `align-self` keyword.
fn parse_align_items(v: &str) -> AlignItems {
    match v {
        "flex-start" | "start" | "self-start" => AlignItems::Start,
        "center" => AlignItems::Center,
        "flex-end" | "end" | "self-end" => AlignItems::End,
        _ => AlignItems::Stretch,
    }
}

fn apply_one(style: &mut Style, prop: &str, value: &str) {
    let v = value.trim();
    match prop {
        "display" => {
            style.display = match v {
                "none" => Display::None,
                "inline" => Display::Inline,
                "inline-block" => Display::InlineBlock,
                "list-item" => Display::ListItem,
                "table" => Display::Table,
                "table-row" => Display::TableRow,
                "table-cell" => Display::TableCell,
                "flex" | "inline-flex" => Display::Flex,
                "grid" | "inline-grid" => Display::Grid,
                _ => Display::Block,
            }
        }
        "flex-direction" => {
            // Only the axis matters for our basic flex: row (default) vs column.
            style.flex_column = v.starts_with("column");
        }
        "justify-content" => {
            style.justify = match v {
                "center" => Justify::Center,
                "flex-end" | "end" | "right" => Justify::End,
                "space-between" => Justify::SpaceBetween,
                "space-around" | "space-evenly" => Justify::SpaceAround,
                _ => Justify::Start,
            };
        }
        "flex-grow" => {
            style.flex_grow = v.parse().unwrap_or(0.0);
        }
        "flex" => {
            // `flex: <grow> [shrink] [basis]` — the first number is the grow
            // factor. The `none`/`auto`/`initial` keywords map to sane grows.
            style.flex_grow = match v {
                "none" | "initial" | "0" => 0.0,
                "auto" => 1.0,
                _ => v
                    .split_whitespace()
                    .next()
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(1.0),
            };
        }
        "grid-template-columns" => {
            style.grid_columns = parse_grid_columns(v);
        }
        "grid-template-rows" => {
            style.grid_rows = parse_grid_columns(v);
        }
        "gap" | "grid-gap" => {
            // `gap: <row> [col]` — one value sets both, two split row/col.
            let parts: Vec<f64> = v
                .split_whitespace()
                .filter_map(|t| parse_len_px(t, style.font_size))
                .collect();
            match parts.as_slice() {
                [a] => {
                    style.gap_row = *a;
                    style.gap_col = *a;
                }
                [a, b, ..] => {
                    style.gap_row = *a;
                    style.gap_col = *b;
                }
                _ => {}
            }
        }
        "row-gap" | "grid-row-gap" => {
            style.gap_row = parse_len_px(v, style.font_size).unwrap_or(0.0);
        }
        "column-gap" | "grid-column-gap" => {
            style.gap_col = parse_len_px(v, style.font_size).unwrap_or(0.0);
        }
        "grid-column" | "grid-column-start" => {
            style.grid_col_start = parse_grid_line(v);
        }
        "grid-row" | "grid-row-start" => {
            style.grid_row_start = parse_grid_line(v);
        }
        "grid-area" => {
            // `grid-area: <row> / <col> [/ …]` — take the first two lines.
            let mut it = v.split('/');
            style.grid_row_start = it.next().map(parse_grid_line).unwrap_or(0);
            style.grid_col_start = it.next().map(parse_grid_line).unwrap_or(0);
        }
        "flex-wrap" => {
            style.flex_wrap = v == "wrap" || v == "wrap-reverse";
        }
        "flex-flow" => {
            // `flex-flow: <direction> || <wrap>` shorthand.
            for tok in v.split_whitespace() {
                match tok {
                    "column" | "column-reverse" => style.flex_column = true,
                    "row" | "row-reverse" => style.flex_column = false,
                    "wrap" | "wrap-reverse" => style.flex_wrap = true,
                    "nowrap" => style.flex_wrap = false,
                    _ => {}
                }
            }
        }
        "align-items" => style.align_items = parse_align_items(v),
        "align-self" => {
            style.align_self = if v == "auto" {
                None
            } else {
                Some(parse_align_items(v))
            };
        }
        "order" => {
            style.order = v.parse().unwrap_or(0);
        }
        "position" => {
            style.position = match v {
                "relative" => Position::Relative,
                "absolute" => Position::Absolute,
                "fixed" => Position::Fixed,
                "sticky" => Position::Relative, // approximated as relative
                _ => Position::Static,
            };
        }
        "top" => style.inset[0] = parse_len(v, style.font_size),
        "right" => style.inset[1] = parse_len(v, style.font_size),
        "bottom" => style.inset[2] = parse_len(v, style.font_size),
        "left" => style.inset[3] = parse_len(v, style.font_size),
        "z-index" => {
            style.z_index = v.parse().unwrap_or(0);
        }
        "overflow" | "overflow-x" | "overflow-y" => {
            // Any non-visible overflow clips descendants to the box.
            if matches!(v, "hidden" | "clip" | "scroll" | "auto") {
                style.overflow_clip = true;
            } else if v == "visible" {
                style.overflow_clip = false;
            }
        }
        "letter-spacing" => {
            style.letter_spacing = if v == "normal" {
                0.0
            } else {
                parse_len_px(v, style.font_size).unwrap_or(0.0)
            };
        }
        "word-spacing" => {
            style.word_spacing = if v == "normal" {
                0.0
            } else {
                parse_len_px(v, style.font_size).unwrap_or(0.0)
            };
        }
        "float" => {
            // `float: left|right` registers the side; the inline formatter flows
            // surrounding text around the float box. `none` leaves block flow.
            style.float = match v {
                "left" => FloatSide::Left,
                "right" => FloatSide::Right,
                _ => FloatSide::None,
            };
        }
        "color" => {
            if let Some(c) = parse_color(v) {
                style.color = c;
            }
        }
        "background" | "background-color" => {
            style.background = parse_color(v.split_whitespace().next().unwrap_or(v));
        }
        "font-size" => {
            if let Some(px) = parse_len_px(v, style.font_size) {
                style.font_size = px;
            }
        }
        "font-weight" => {
            style.bold = matches!(v, "bold" | "bolder" | "600" | "700" | "800" | "900");
        }
        "font-style" => style.italic = matches!(v, "italic" | "oblique"),
        "font-family" => {
            let first = v
                .split(',')
                .next()
                .unwrap_or(v)
                .trim()
                .trim_matches(['"', '\'']);
            style.font_family = first.to_string();
            let lower = first.to_ascii_lowercase();
            style.generic_serif =
                lower == "serif" || lower.contains("times") || lower.contains("georgia");
            style.generic_mono = lower == "monospace"
                || lower.contains("courier")
                || lower.contains("mono")
                || lower.contains("consol");
        }
        "text-align" => {
            style.align = match v {
                "center" => Align::Center,
                "right" => Align::Right,
                "justify" => Align::Justify,
                _ => Align::Left,
            }
        }
        "text-decoration" | "text-decoration-line" => {
            style.underline = v.contains("underline");
            style.strike = v.contains("line-through");
            style.overline = v.contains("overline");
        }
        "visibility" => style.hidden = v == "hidden" || v == "collapse",
        "opacity" => {
            if let Ok(o) = v.parse::<f64>() {
                style.opacity = o.clamp(0.0, 1.0);
            }
        }
        "text-indent" => style.text_indent = parse_len_px(v, style.font_size).unwrap_or(0.0),
        "list-style-type" | "list-style" => {
            // `list-style` shorthand: scan tokens for a known type keyword.
            for tok in v.split_whitespace() {
                let s = match tok {
                    "disc" => ListStyle::Disc,
                    "circle" => ListStyle::Circle,
                    "square" => ListStyle::Square,
                    "decimal" => ListStyle::Decimal,
                    "lower-alpha" | "lower-latin" => ListStyle::LowerAlpha,
                    "upper-alpha" | "upper-latin" => ListStyle::UpperAlpha,
                    "lower-roman" => ListStyle::LowerRoman,
                    "upper-roman" => ListStyle::UpperRoman,
                    "none" => ListStyle::None,
                    _ => continue,
                };
                style.list_style = s;
                break;
            }
        }
        "min-width" => style.min_width = parse_len(v, style.font_size),
        "max-width" => style.max_width = parse_len(v, style.font_size),
        "height" | "min-height" => {
            style.min_height = parse_len_px(v, style.font_size);
        }
        "box-sizing" => style.border_box = v == "border-box",
        "text-transform" => {
            style.text_transform = match v {
                "uppercase" => TextTransform::Upper,
                "lowercase" => TextTransform::Lower,
                "capitalize" => TextTransform::Capitalize,
                _ => TextTransform::None,
            };
        }
        "line-height" => {
            if let Ok(n) = v.parse::<f64>() {
                style.line_height = n;
            } else if let Some(px) = parse_len_px(v, style.font_size) {
                style.line_height = px / style.font_size;
            }
        }
        "white-space" => style.pre = v.starts_with("pre"),
        "page-break-before" | "break-before" => {
            style.page_break_before =
                matches!(v, "always" | "page" | "left" | "right" | "recto" | "verso");
        }
        "page-break-after" | "break-after" => {
            style.page_break_after =
                matches!(v, "always" | "page" | "left" | "right" | "recto" | "verso");
        }
        "margin" => style.margin = parse_edges(v, style.font_size),
        "margin-top" => style.margin.top = parse_len_px(v, style.font_size).unwrap_or(0.0),
        "margin-right" => style.margin.right = parse_len_px(v, style.font_size).unwrap_or(0.0),
        "margin-bottom" => style.margin.bottom = parse_len_px(v, style.font_size).unwrap_or(0.0),
        "margin-left" => style.margin.left = parse_len_px(v, style.font_size).unwrap_or(0.0),
        "padding" => style.padding = parse_edges(v, style.font_size),
        "padding-top" => style.padding.top = parse_len_px(v, style.font_size).unwrap_or(0.0),
        "padding-right" => style.padding.right = parse_len_px(v, style.font_size).unwrap_or(0.0),
        "padding-bottom" => style.padding.bottom = parse_len_px(v, style.font_size).unwrap_or(0.0),
        "padding-left" => style.padding.left = parse_len_px(v, style.font_size).unwrap_or(0.0),
        "border" | "border-width" => {
            // `border: 1px solid #ccc` — take first length + a colour if present.
            let mut w = 1.0;
            for tok in v.split_whitespace() {
                if let Some(px) = parse_len_px(tok, style.font_size) {
                    w = px;
                } else if let Some(c) = parse_color(tok) {
                    style.border_color = c;
                }
            }
            style.border_width = Edges::all(w);
        }
        "border-color" => {
            if let Some(c) = parse_color(v) {
                style.border_color = c;
            }
        }
        "width" => style.width = parse_len(v, style.font_size),
        _ => {}
    }
}

fn parse_edges(v: &str, em: f64) -> Edges {
    let parts: Vec<f64> = v
        .split_whitespace()
        .map(|t| parse_len_px(t, em).unwrap_or(0.0))
        .collect();
    match parts.as_slice() {
        [a] => Edges::all(*a),
        [a, b] => Edges {
            top: *a,
            bottom: *a,
            left: *b,
            right: *b,
        },
        [a, b, c] => Edges {
            top: *a,
            right: *b,
            left: *b,
            bottom: *c,
        },
        [a, b, c, d] => Edges {
            top: *a,
            right: *b,
            bottom: *c,
            left: *d,
        },
        _ => Edges::default(),
    }
}

/// Reference viewport (US-Letter content) used to resolve `vw`/`vh` lengths
/// when no live page size is threaded into the cascade. Width 612pt, height
/// 792pt — approximate but consistent with the default page box.
const VIEWPORT_W_PT: f64 = 612.0;
const VIEWPORT_H_PT: f64 = 792.0;
/// Assumed root font size (1rem) in points.
const ROOT_EM_PT: f64 = 12.0;

/// Strip a single level of `var(--name[, fallback])`, yielding the fallback (the
/// declared behaviour when the custom property is unknown — custom properties
/// are not tracked). Returns the inner text unchanged when there is no `var(`.
fn resolve_var(v: &str) -> String {
    let v = v.trim();
    if let Some(rest) = v.strip_prefix("var(") {
        if let Some(inner) = rest.strip_suffix(')') {
            // `var(--name, fallback)` → fallback; `var(--name)` → empty.
            return inner
                .split_once(',')
                .map(|(_, fb)| fb.trim().to_string())
                .unwrap_or_default();
        }
    }
    v.to_string()
}

/// Evaluate a basic `calc(A op B)` with `+ - * /` over two point-resolved
/// operands; `None` if it is not a `calc(...)` or cannot be reduced to points.
fn parse_calc_px(v: &str, em: f64) -> Option<f64> {
    let inner = v.trim().strip_prefix("calc(")?.strip_suffix(')')?.trim();
    // Find a top-level binary operator (single operation only; no nesting).
    for op in ['+', '-', '*', '/'] {
        // Skip a leading sign so `-5px + 1px` still splits on the real `+`.
        if let Some(pos) = inner[1..].find(op).map(|i| i + 1) {
            let (a, b) = inner.split_at(pos);
            let b = &b[1..];
            let (a, b) = (a.trim(), b.trim());
            return match op {
                '+' => Some(parse_len_px(a, em)? + parse_len_px(b, em)?),
                '-' => Some(parse_len_px(a, em)? - parse_len_px(b, em)?),
                '*' => {
                    // One side must be a unitless multiplier.
                    let an = a.parse::<f64>().ok();
                    let bn = b.parse::<f64>().ok();
                    match (an, bn) {
                        (Some(k), None) => Some(k * parse_len_px(b, em)?),
                        (None, Some(k)) => Some(parse_len_px(a, em)? * k),
                        _ => None,
                    }
                }
                '/' => Some(parse_len_px(a, em)? / b.parse::<f64>().ok()?),
                _ => None,
            };
        }
    }
    None
}

/// Parse a length to absolute points (1px ≈ 0.75pt at 96dpi), resolving `em`,
/// `rem`, `vw`/`vh` (reference viewport) and a basic `calc()`/`var()`.
fn parse_len_px(v: &str, em: f64) -> Option<f64> {
    let resolved = resolve_var(v);
    let v = resolved.trim();
    if v.starts_with("calc(") {
        return parse_calc_px(v, em);
    }
    if let Some(n) = v.strip_suffix("px") {
        return n.trim().parse::<f64>().ok().map(|p| p * 0.75);
    }
    if let Some(n) = v.strip_suffix("pt") {
        return n.trim().parse::<f64>().ok();
    }
    if let Some(n) = v.strip_suffix("rem") {
        return n.trim().parse::<f64>().ok().map(|p| p * ROOT_EM_PT);
    }
    if let Some(n) = v.strip_suffix("em") {
        return n.trim().parse::<f64>().ok().map(|p| p * em);
    }
    if let Some(n) = v.strip_suffix("vw") {
        return n.trim().parse::<f64>().ok().map(|p| VIEWPORT_W_PT * p / 100.0);
    }
    if let Some(n) = v.strip_suffix("vh") {
        return n.trim().parse::<f64>().ok().map(|p| VIEWPORT_H_PT * p / 100.0);
    }
    if let Some(n) = v.strip_suffix('%') {
        // Percent of font size only makes sense for line-height/font-size here.
        return n.trim().parse::<f64>().ok().map(|p| em * p / 100.0);
    }
    v.parse::<f64>().ok().map(|p| p * 0.75)
}

fn parse_len(v: &str, em: f64) -> Option<Len> {
    let resolved = resolve_var(v);
    let v = resolved.trim();
    if let Some(n) = v.strip_suffix('%') {
        return n.trim().parse::<f64>().ok().map(Len::Percent);
    }
    // `vw` maps to a percentage of the container width (closer to spec for
    // box widths than the fixed reference viewport `parse_len_px` would use).
    if let Some(n) = v.strip_suffix("vw") {
        return n.trim().parse::<f64>().ok().map(Len::Percent);
    }
    parse_len_px(v, em).map(Len::Pt)
}

/// Parse `#rgb`, `#rrggbb`, `rgb(…)` and the common named colours.
pub fn parse_color(v: &str) -> Option<[f64; 3]> {
    let v = v.trim().to_ascii_lowercase();
    if let Some(hex) = v.strip_prefix('#') {
        let (r, g, b) = match hex.len() {
            3 => (
                u8::from_str_radix(&hex[0..1].repeat(2), 16).ok()?,
                u8::from_str_radix(&hex[1..2].repeat(2), 16).ok()?,
                u8::from_str_radix(&hex[2..3].repeat(2), 16).ok()?,
            ),
            6 => (
                u8::from_str_radix(&hex[0..2], 16).ok()?,
                u8::from_str_radix(&hex[2..4], 16).ok()?,
                u8::from_str_radix(&hex[4..6], 16).ok()?,
            ),
            _ => return None,
        };
        return Some([r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0]);
    }
    if let Some(inner) = v.strip_prefix("rgb(").and_then(|s| s.strip_suffix(')')) {
        let nums: Vec<f64> = inner
            .split(',')
            .filter_map(|n| n.trim().parse::<f64>().ok())
            .collect();
        if nums.len() >= 3 {
            return Some([nums[0] / 255.0, nums[1] / 255.0, nums[2] / 255.0]);
        }
    }
    let named = match v.as_str() {
        "black" => [0.0, 0.0, 0.0],
        "white" => [1.0, 1.0, 1.0],
        "red" => [1.0, 0.0, 0.0],
        "green" => [0.0, 0.5, 0.0],
        "lime" => [0.0, 1.0, 0.0],
        "blue" => [0.0, 0.0, 1.0],
        "gray" | "grey" => [0.5, 0.5, 0.5],
        "silver" => [0.75, 0.75, 0.75],
        "lightgray" | "lightgrey" => [0.83, 0.83, 0.83],
        "navy" => [0.0, 0.0, 0.5],
        "orange" => [1.0, 0.647, 0.0],
        "yellow" => [1.0, 1.0, 0.0],
        "purple" => [0.5, 0.0, 0.5],
        "teal" => [0.0, 0.5, 0.5],
        "maroon" => [0.5, 0.0, 0.0],
        "transparent" => return None,
        _ => return None,
    };
    Some(named)
}

/// The minimal user-agent stylesheet (tag defaults). Sizes in points.
const UA_CSS: &str = "
body { display: block; font-size: 12pt; line-height: 1.3; color: #000; }
div, p, section, article, header, footer, nav, main, ul, ol, li, blockquote, figure, figcaption, table, form, fieldset { display: block; }
p { margin-top: 8pt; margin-bottom: 8pt; }
h1 { display: block; font-size: 24pt; font-weight: bold; margin-top: 14pt; margin-bottom: 10pt; }
h2 { display: block; font-size: 20pt; font-weight: bold; margin-top: 12pt; margin-bottom: 8pt; }
h3 { display: block; font-size: 16pt; font-weight: bold; margin-top: 10pt; margin-bottom: 7pt; }
h4 { display: block; font-size: 13pt; font-weight: bold; margin-top: 9pt; margin-bottom: 6pt; }
h5 { display: block; font-size: 12pt; font-weight: bold; margin-top: 8pt; margin-bottom: 5pt; }
h6 { display: block; font-size: 11pt; font-weight: bold; margin-top: 8pt; margin-bottom: 5pt; }
b, strong { font-weight: bold; }
i, em { font-style: italic; }
u { text-decoration: underline; }
a { color: #0645ad; text-decoration: underline; }
ul, ol { margin-top: 8pt; margin-bottom: 8pt; padding-left: 30pt; }
li { display: list-item; }
blockquote { margin-left: 30pt; margin-top: 8pt; margin-bottom: 8pt; }
pre { display: block; white-space: pre; font-family: monospace; margin-top: 8pt; margin-bottom: 8pt; }
code, kbd, samp { font-family: monospace; }
table { display: table; }
tr { display: table-row; }
td, th { display: table-cell; padding: 2pt; border: 1pt solid #c0c0c0; }
th { font-weight: bold; }
hr { display: block; margin-top: 8pt; margin-bottom: 8pt; border: 1pt solid #808080; }
small { font-size: 10pt; }
pagebreak, page-break { display: block; page-break-after: always; }
head, script, style, title, meta, link, base, noscript { display: none; }
";

/// Collect author CSS from every `<style>` element in the tree.
pub fn collect_style_css(nodes: &[Node]) -> String {
    let mut css = String::new();
    collect_css_into(nodes, &mut css);
    css
}

fn collect_css_into(nodes: &[Node], css: &mut String) {
    for n in nodes {
        if let Node::Element(e) = n {
            if e.tag == "style" {
                for c in &e.children {
                    if let Node::Text(t) = c {
                        css.push_str(t);
                        css.push('\n');
                    }
                }
            } else {
                collect_css_into(&e.children, css);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::dom::parse;
    use super::*;

    #[test]
    fn colors_parse() {
        assert_eq!(parse_color("#fff"), Some([1.0, 1.0, 1.0]));
        assert_eq!(parse_color("#ff0000"), Some([1.0, 0.0, 0.0]));
        assert_eq!(parse_color("rgb(0,128,0)").map(|c| c[1] > 0.4), Some(true));
        assert!(parse_color("blue").is_some());
    }

    #[test]
    fn cascade_specificity_and_inline() {
        let nodes = parse(
            r#"<style>p { color: red; } p.k { color: green; }</style>
               <p class="k" style="color:#0000ff">hi</p>"#,
        );
        let sheet = Stylesheet::new(&collect_style_css(&nodes));
        let p = nodes
            .iter()
            .find_map(|n| match n {
                Node::Element(e) if e.tag == "p" => Some(e),
                _ => None,
            })
            .unwrap();
        let style = sheet.computed(p, &Style::default(), &[]);
        // Inline blue wins over the class-green and tag-red rules.
        assert_eq!(style.color, [0.0, 0.0, 1.0]);
    }

    #[test]
    fn ua_defaults_make_h1_big_and_bold() {
        let nodes = parse("<h1>Title</h1>");
        let sheet = Stylesheet::new("");
        let h1 = nodes
            .iter()
            .find_map(|n| match n {
                Node::Element(e) => Some(e),
                _ => None,
            })
            .unwrap();
        let style = sheet.computed(h1, &Style::default(), &[]);
        assert!(style.bold);
        assert!(style.font_size > 16.0);
        assert_eq!(style.display, Display::Block);
    }

    #[test]
    fn inheritance_passes_color_down() {
        let parent = Style {
            color: [1.0, 0.0, 0.0],
            ..Style::default()
        };
        let child = inherit(&parent);
        assert_eq!(child.color, [1.0, 0.0, 0.0], "color inherits");
        assert_eq!(child.margin.top, 0.0, "margin does not inherit");
    }
}
