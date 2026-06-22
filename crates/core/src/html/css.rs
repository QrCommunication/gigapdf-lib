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

/// CSS `vertical-align` for table cells — how the cell content box is placed
/// inside the (taller) row box. Only the table-cell values are modelled; the
/// inline values (`text-top`, `super`, …) collapse to the nearest of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VAlign {
    /// `top` — content hugs the top of the cell. This is the initial value for
    /// our single-line model (a single-line cell fills its row, so top is
    /// indistinguishable from middle/baseline there — keeps existing layouts
    /// byte-identical).
    #[default]
    Top,
    /// `middle` — content centred vertically (what Office emits for most
    /// invoice cells with multi-line neighbours).
    Middle,
    /// `bottom` / `baseline` — content hugs the bottom of the cell.
    Bottom,
}

/// Inline `vertical-align` for super/subscript text. The table-cell values are
/// modelled separately by [`VAlign`]; this carries the baseline shift that moves
/// a run up (super) or down (sub) relative to the surrounding text. The cascade
/// resolves it to an absolute point offset in `Style::valign_shift` using the
/// *parent* font-size, so a shrunk `<sup>` glyph still lifts by the right amount.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum VShift {
    /// `baseline` (the default) — no vertical shift.
    #[default]
    Baseline,
    /// `super` — raise by ~⅓ of the parent em.
    Super,
    /// `sub` — lower by ~⅕ of the parent em.
    Sub,
    /// An explicit `vertical-align: <length>` or `<percentage>` already resolved
    /// to points (positive = raise the run, matching CSS). Percentages resolve
    /// against the element's own font-size at parse time.
    Points(f64),
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
    /// `margin-left: auto` — the left margin absorbs free space. With its right
    /// counterpart this centres a fixed-width block horizontally. Not inherited.
    pub margin_left_auto: bool,
    /// `margin-right: auto`. Not inherited.
    pub margin_right_auto: bool,
    pub padding: Edges,
    pub border_width: Edges,
    /// Single border colour (the `border`/`border-color` shorthand). Kept for
    /// the block/flex/grid uniform-border paths; per-side colours live in
    /// `border_color_edges`.
    pub border_color: [f64; 3],
    /// Per-side border colours in `[top, right, bottom, left]` order. Each side
    /// defaults to `border_color`; the `border-{top,right,bottom,left}[-color]`
    /// longhands override an individual side, letting a cell stroke (say) only
    /// a coloured bottom rule.
    pub border_color_edges: [[f64; 3]; 4],
    /// `vertical-align` of table-cell content within its (taller) row box.
    pub vertical_align: VAlign,
    /// `border-collapse: collapse` — adjacent table-cell borders share a single
    /// rule instead of each cell drawing its own (Office tables default to
    /// collapse, giving the clean single-line grid of an invoice).
    pub border_collapse: bool,
    pub width: Option<Len>,
    pub line_height: f64,
    pub pre: bool,
    /// `page-break-before: always` / `break-before: page` — start a new page
    /// before this block.
    pub page_break_before: bool,
    /// `page-break-after: always` / `break-after: page` — start a new page
    /// after this block.
    pub page_break_after: bool,
    /// `page-break-inside: avoid` / `break-inside: avoid` — keep this block on a
    /// single page when it fits, pushing it to the next page rather than letting
    /// a page boundary cut through it. Not inherited.
    pub page_break_inside_avoid: bool,
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
    /// `column-gap` / `gap` — horizontal gutter between tracks (points). Shared
    /// by grid/flex gutters and the multi-column gutter (`columns`/`column-gap`).
    pub gap_col: f64,
    /// `column-count` / `columns` — number of equal-width columns the block's
    /// flow content is split into (`0`/`1` = a single normal column, i.e. not a
    /// multi-column block). Not inherited.
    pub column_count: usize,
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
    /// Inline `vertical-align` (super/sub/length) for THIS run, before cascade
    /// resolves it to a point offset. Not inherited (resets to `baseline`).
    pub valign: VShift,
    /// Resolved super/subscript baseline offset in points, top-down: a negative
    /// value raises the run (super), a positive value lowers it (sub). Computed
    /// during cascade from the *parent* em so a shrunk glyph still shifts by the
    /// surrounding text's scale. Not inherited.
    pub valign_shift: f64,
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
            margin_left_auto: false,
            margin_right_auto: false,
            padding: Edges::default(),
            border_width: Edges::default(),
            border_color: [0.0, 0.0, 0.0],
            border_color_edges: [[0.0, 0.0, 0.0]; 4],
            vertical_align: VAlign::Top,
            border_collapse: false,
            width: None,
            line_height: 1.2,
            pre: false,
            page_break_before: false,
            page_break_after: false,
            page_break_inside_avoid: false,
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
            column_count: 0,
            grid_col_start: 0,
            grid_row_start: 0,
            letter_spacing: 0.0,
            word_spacing: 0.0,
            float: FloatSide::None,
            valign: VShift::Baseline,
            valign_shift: 0.0,
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

/// An `[attr]` / `[attr=val]` attribute condition on a compound selector.
#[derive(Debug, Clone)]
struct AttrCond {
    name: String,
    /// `Some(v)` for `[attr=v]` (exact match); `None` for bare `[attr]`
    /// (presence only). Values are stored as written (case-sensitive).
    value: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct Compound {
    tag: Option<String>,
    classes: Vec<String>,
    id: Option<String>,
    /// `[attr]` / `[attr=val]` conditions (all must hold).
    attrs: Vec<AttrCond>,
}

/// How a compound relates to the one before it in the selector chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Combinator {
    /// (whitespace) descendant: an ancestor must match.
    Descendant,
    /// `>` child: the immediate parent must match.
    Child,
    /// `+` adjacent sibling: the immediately preceding sibling must match.
    AdjacentSibling,
    /// `~` general sibling: some preceding sibling must match.
    GeneralSibling,
}

#[derive(Debug, Clone)]
struct Selector {
    /// Target-first-to-ancestor chain: `parts[0]` matches the element itself,
    /// each later part carries the combinator linking it to the previous one
    /// and is checked against the appropriate relative (parent / ancestor /
    /// sibling). Storing the chain target-first lets matching walk outward.
    parts: Vec<(Combinator, Compound)>,
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
            b'[' => {
                // `[attr]` or `[attr=value]` / `[attr="value"]`. Other operators
                // (`~=`, `^=`, …) are not modelled: we keep the bare-name
                // presence test, which is a safe over-match (better than dropping
                // the rule).
                i += 1;
                let inner_start = i;
                while i < bytes.len() && bytes[i] != b']' {
                    i += 1;
                }
                let inner = &s[inner_start..i];
                if i < bytes.len() {
                    i += 1; // consume ']'
                }
                if let Some((name, val)) = inner.split_once('=') {
                    let name = name.trim_end_matches(['~', '^', '$', '*', '|']).trim();
                    c.attrs.push(AttrCond {
                        name: name.to_ascii_lowercase(),
                        value: Some(val.trim().trim_matches(['"', '\'']).to_string()),
                    });
                } else if !inner.trim().is_empty() {
                    c.attrs.push(AttrCond {
                        name: inner.trim().to_ascii_lowercase(),
                        value: None,
                    });
                }
            }
            _ => i += 1,
        }
    }
    c
}

fn parse_selector(s: &str) -> Option<Selector> {
    // Tokenize into compound strings and combinator symbols. A `>`/`+`/`~`
    // between compounds sets the relationship of the FOLLOWING compound to the
    // one before it; whitespace alone means descendant.
    let toks = tokenize_selector(s);
    // Build the source-order chain (ancestor → target) of (incoming-combinator,
    // compound) pairs. The first compound has no incoming combinator.
    let mut src: Vec<(Combinator, Compound)> = Vec::new();
    let mut pending = Combinator::Descendant; // ignored for the first compound
    for tok in toks {
        match tok {
            SelTok::Combinator(c) => pending = c,
            SelTok::Compound(text) => {
                src.push((pending, parse_compound(&text)));
                pending = Combinator::Descendant;
            }
        }
    }
    if src.is_empty() {
        return None;
    }
    // Specificity: ids*100 + (classes + attrs)*10 + tags. Combinators add none.
    let mut spec = 0u32;
    for (_, p) in &src {
        if p.id.is_some() {
            spec += 100;
        }
        spec += 10 * (p.classes.len() + p.attrs.len()) as u32;
        if p.tag.is_some() {
            spec += 1;
        }
    }
    // Re-order target-first: the LAST source compound matches the element; each
    // earlier compound carries the combinator that, in source order, preceded
    // the compound on its right. Walking target-first, that is the combinator
    // stored on the compound immediately to the right in the source chain.
    let mut parts: Vec<(Combinator, Compound)> = Vec::with_capacity(src.len());
    for i in (0..src.len()).rev() {
        let combinator = if i + 1 < src.len() {
            src[i + 1].0
        } else {
            Combinator::Descendant // target itself: relationship is unused
        };
        parts.push((combinator, src[i].1.clone()));
    }
    Some(Selector {
        parts,
        specificity: spec,
    })
}

/// A token in a complex selector: a compound (`div.x#y[a]`) or a combinator.
enum SelTok {
    Compound(String),
    Combinator(Combinator),
}

/// Split a complex selector into compound/combinator tokens. Whitespace is a
/// descendant combinator unless it merely surrounds an explicit `>`/`+`/`~`.
fn tokenize_selector(s: &str) -> Vec<SelTok> {
    let mut toks: Vec<SelTok> = Vec::new();
    let mut cur = String::new();
    let mut prev_ws = false;
    let flush = |cur: &mut String, toks: &mut Vec<SelTok>| {
        if !cur.is_empty() {
            toks.push(SelTok::Compound(std::mem::take(cur)));
        }
    };
    for ch in s.chars() {
        match ch {
            '>' | '+' | '~' => {
                flush(&mut cur, &mut toks);
                // Replace a trailing descendant (from surrounding whitespace)
                // with the explicit combinator.
                if matches!(toks.last(), Some(SelTok::Combinator(Combinator::Descendant))) {
                    toks.pop();
                }
                toks.push(SelTok::Combinator(match ch {
                    '>' => Combinator::Child,
                    '+' => Combinator::AdjacentSibling,
                    _ => Combinator::GeneralSibling,
                }));
                prev_ws = false;
            }
            c if c.is_whitespace() => {
                flush(&mut cur, &mut toks);
                prev_ws = true;
            }
            c => {
                // A space between two compounds (no explicit combinator) is a
                // descendant relationship.
                if prev_ws
                    && cur.is_empty()
                    && matches!(toks.last(), Some(SelTok::Compound(_)))
                {
                    toks.push(SelTok::Combinator(Combinator::Descendant));
                }
                prev_ws = false;
                cur.push(c);
            }
        }
    }
    flush(&mut cur, &mut toks);
    toks
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
    for cond in &compound.attrs {
        match el.attr(&cond.name) {
            None => return false,
            Some(actual) => {
                if let Some(want) = &cond.value {
                    if actual != want.as_str() {
                        return false;
                    }
                }
            }
        }
    }
    true
}

/// Preceding element siblings of `el` (source order) given its parent's
/// children. Text nodes are skipped, so `a + b` ignores whitespace between
/// tags. Identity is by pointer (the `ancestors`/`el` references are live
/// borrows into the same tree).
fn preceding_siblings<'a>(el: &Element, parent: &'a Element) -> Vec<&'a Element> {
    let mut out: Vec<&Element> = Vec::new();
    for child in &parent.children {
        if let Node::Element(e) = child {
            if std::ptr::eq(e, el) {
                break;
            }
            out.push(e);
        }
    }
    out
}

/// Does `selector` match `el` given its ancestor chain (root-first, excluding
/// `el`)? Handles descendant / child / adjacent-sibling / general-sibling
/// combinators. `selector.parts` is target-first: `parts[0]` is `el`, and each
/// later part's combinator describes how it relates to the part before it.
fn selector_matches(selector: &Selector, el: &Element, ancestors: &[&Element]) -> bool {
    let parts = &selector.parts;
    // parts[0] always matches the element itself.
    if !matches(&parts[0].1, el) {
        return false;
    }
    let parent = ancestors.last().copied();
    // `cur` tracks the element currently anchored; `ai` the next ancestor index
    // available for descendant/child hops (ancestors are root-first).
    let mut cur: &Element = el;
    let mut ai = ancestors.len();
    for (combinator, compound) in parts[1..].iter() {
        match combinator {
            Combinator::Child => {
                // Immediate parent of `cur` must match. The parent is the
                // ancestor just below the current `ai` cursor.
                if ai == 0 {
                    return false;
                }
                ai -= 1;
                let p = ancestors[ai];
                if !matches(compound, p) {
                    return false;
                }
                cur = p;
            }
            Combinator::Descendant => {
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
                cur = ancestors[ai];
            }
            Combinator::AdjacentSibling => {
                // The element immediately before `cur` under the same parent.
                let Some(par) = sibling_parent(cur, el, parent, ancestors, ai) else {
                    return false;
                };
                let prev = preceding_siblings(cur, par);
                match prev.last() {
                    Some(p) if matches(compound, p) => cur = p,
                    _ => return false,
                }
            }
            Combinator::GeneralSibling => {
                let Some(par) = sibling_parent(cur, el, parent, ancestors, ai) else {
                    return false;
                };
                let prev = preceding_siblings(cur, par);
                match prev.iter().rev().find(|p| matches(compound, p)) {
                    Some(p) => cur = p,
                    None => return false,
                }
            }
        }
    }
    true
}

/// The parent element of `cur` for a sibling combinator. When `cur` is still
/// `el`, the parent is `ancestors.last()` (`parent`); after one or more
/// descendant/child hops, it is the ancestor just below the `ai` cursor.
fn sibling_parent<'a>(
    cur: &Element,
    el: &Element,
    parent: Option<&'a Element>,
    ancestors: &[&'a Element],
    ai: usize,
) -> Option<&'a Element> {
    if std::ptr::eq(cur, el) {
        parent
    } else if ai > 0 {
        Some(ancestors[ai - 1])
    } else {
        None
    }
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
        // Resolve the inline super/subscript shift to a top-down point offset.
        // Keyword shifts use the *parent* em so a shrunk `<sup>` glyph still
        // lifts by the surrounding text's scale; explicit lengths are kept as-is.
        // Top-down sign: negative raises (super), positive lowers (sub).
        let parent_em = parent.font_size;
        style.valign_shift = match style.valign {
            VShift::Baseline => 0.0,
            VShift::Super => -parent_em * 0.33,
            VShift::Sub => parent_em * 0.20,
            VShift::Points(p) => -p, // CSS positive = up ⇒ negative top-down
        };
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
        margin_left_auto: false,
        margin_right_auto: false,
        padding: Edges::default(),
        border_width: Edges::default(),
        border_color: parent.color,
        // Per-side colours reset to the (resolved) text colour like
        // `border-color`; longhands repaint individual sides during cascade.
        border_color_edges: [parent.color; 4],
        // `vertical-align` is not inherited (resets to the initial value).
        vertical_align: VAlign::Top,
        // `border-collapse` IS inherited so it can be set once on the <table>
        // and reach every cell.
        border_collapse: parent.border_collapse,
        width: None,
        page_break_before: false,
        page_break_after: false,
        page_break_inside_avoid: false,
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
        column_count: 0,
        grid_col_start: 0,
        grid_row_start: 0,
        float: FloatSide::None,
        // `vertical-align` is not inherited; super/sub apply to the run only.
        valign: VShift::Baseline,
        valign_shift: 0.0,
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

/// Parse the column count from a `columns` shorthand (`column-width ||
/// column-count`). The shorthand mixes a length (the column *width*) and a bare
/// integer (the column *count*) in either order; we model the count only, so we
/// take the first bare integer token. A sole `column-width` (no count) and the
/// `auto` keyword leave the count unset (`0`) — we can't derive a count from a
/// width without the container width, so the block stays single-column.
fn parse_columns_shorthand(v: &str) -> usize {
    for tok in v.split_whitespace() {
        // A bare unitless integer is the column-count; anything with a unit is
        // the column-width (ignored), and `auto` carries no count.
        if let Ok(n) = tok.parse::<usize>() {
            return n;
        }
    }
    0
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
            // `normal` (the initial value) keeps the default gutter (0 here);
            // multi-column layout falls back to a 1em gutter when unset.
            style.gap_col = if v == "normal" {
                0.0
            } else {
                parse_len_px(v, style.font_size).unwrap_or(0.0)
            };
        }
        "column-count" => {
            // `column-count: <integer>`; `auto` (or a non-integer) ⇒ 0 (none).
            style.column_count = v.parse::<usize>().unwrap_or(0);
        }
        "columns" => {
            // `columns: <column-width> || <column-count>` — we model the count.
            style.column_count = parse_columns_shorthand(v);
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
        "page-break-inside" | "break-inside" => {
            style.page_break_inside_avoid = matches!(v, "avoid" | "avoid-page");
        }
        "margin" => {
            style.margin = parse_edges(v, style.font_size);
            // Detect `auto` on the horizontal sides so a fixed-width block can
            // centre. The shorthand maps 1→all, 2→(v,h), 3→(t,h,b), 4→(t,r,b,l);
            // the left/right tokens are where horizontal `auto` lands.
            let toks: Vec<&str> = v.split_whitespace().collect();
            let (l_auto, r_auto) = match toks.as_slice() {
                [a] => (*a == "auto", *a == "auto"),
                [_, h] | [_, h, _] => (*h == "auto", *h == "auto"),
                [_, r, _, l] => (*l == "auto", *r == "auto"),
                _ => (false, false),
            };
            style.margin_left_auto = l_auto;
            style.margin_right_auto = r_auto;
        }
        "margin-top" => style.margin.top = parse_len_px(v, style.font_size).unwrap_or(0.0),
        "margin-right" => {
            style.margin_right_auto = v == "auto";
            style.margin.right = parse_len_px(v, style.font_size).unwrap_or(0.0);
        }
        "margin-bottom" => style.margin.bottom = parse_len_px(v, style.font_size).unwrap_or(0.0),
        "margin-left" => {
            style.margin_left_auto = v == "auto";
            style.margin.left = parse_len_px(v, style.font_size).unwrap_or(0.0);
        }
        "padding" => style.padding = parse_edges(v, style.font_size),
        "padding-top" => style.padding.top = parse_len_px(v, style.font_size).unwrap_or(0.0),
        "padding-right" => style.padding.right = parse_len_px(v, style.font_size).unwrap_or(0.0),
        "padding-bottom" => style.padding.bottom = parse_len_px(v, style.font_size).unwrap_or(0.0),
        "padding-left" => style.padding.left = parse_len_px(v, style.font_size).unwrap_or(0.0),
        "border" | "border-width" => {
            // `border: 1px solid #ccc` — take first length + a colour if present.
            // A bare `border-width` carries only the length(s); `none`/`hidden`
            // zero the side(s).
            let (w, c, vis) = parse_border_shorthand(v, style.font_size);
            style.border_width = Edges::all(if vis { w } else { 0.0 });
            if let Some(c) = c {
                style.border_color = c;
                style.border_color_edges = [c; 4];
            }
        }
        "border-top" | "border-right" | "border-bottom" | "border-left" => {
            let (w, c, vis) = parse_border_shorthand(v, style.font_size);
            let i = border_side_index(prop);
            set_border_side(style, i, if vis { Some(w) } else { Some(0.0) }, c);
        }
        "border-top-width" | "border-right-width" | "border-bottom-width" | "border-left-width" => {
            let i = border_side_index(prop);
            let w = parse_len_px(v, style.font_size).unwrap_or(0.0);
            set_border_side(style, i, Some(w), None);
        }
        "border-color" => {
            // `border-color` accepts 1–4 colours (TRBL like the box shorthands).
            apply_border_color_shorthand(style, v);
        }
        "border-top-color" | "border-right-color" | "border-bottom-color" | "border-left-color" => {
            if let Some(c) = parse_color(v) {
                set_border_side(style, border_side_index(prop), None, Some(c));
            }
        }
        // `border-style` longhands carry no geometry in our model, but `none`/
        // `hidden` must suppress the rule even when a width is also declared.
        "border-style"
        | "border-top-style"
        | "border-right-style"
        | "border-bottom-style"
        | "border-left-style" => {
            if matches!(v, "none" | "hidden") {
                match prop {
                    "border-style" => style.border_width = Edges::default(),
                    _ => set_border_side(style, border_side_index(prop), Some(0.0), None),
                }
            }
        }
        "vertical-align" => {
            // Table-cell box alignment (kept) — the inline super/sub values map
            // to `top` here since a single-line cell fills its row.
            style.vertical_align = match v {
                "middle" => VAlign::Middle,
                "bottom" | "text-bottom" => VAlign::Bottom,
                _ => VAlign::Top,
            };
            // Inline baseline shift for super/subscript text. Lengths/percentages
            // resolve against the run's own font-size; `%` is positive-up like CSS.
            style.valign = match v {
                "super" => VShift::Super,
                "sub" => VShift::Sub,
                "baseline" | "top" | "middle" | "bottom" | "text-top" | "text-bottom" => {
                    VShift::Baseline
                }
                _ => {
                    if let Some(p) = v
                        .strip_suffix('%')
                        .and_then(|n| n.trim().parse::<f64>().ok())
                    {
                        VShift::Points(style.font_size * p / 100.0)
                    } else if let Some(pt) = parse_len_px(v, style.font_size) {
                        VShift::Points(pt)
                    } else {
                        VShift::Baseline
                    }
                }
            };
        }
        "border-collapse" => style.border_collapse = v == "collapse",
        "width" => style.width = parse_len(v, style.font_size),
        _ => {}
    }
}

/// `[top, right, bottom, left]` index for a `border-{side}*` property.
fn border_side_index(prop: &str) -> usize {
    if prop.contains("-top") {
        0
    } else if prop.contains("-right") {
        1
    } else if prop.contains("-bottom") {
        2
    } else {
        3 // -left
    }
}

/// Parse a `border` / `border-{side}` shorthand value into
/// `(width_pt, colour?, visible)`. Width defaults to a hairline (1pt) when a
/// style/colour is given without a length (matching the CSS `medium` initial
/// width pragmatically); `none`/`hidden` set `visible = false`.
fn parse_border_shorthand(v: &str, em: f64) -> (f64, Option<[f64; 3]>, bool) {
    let mut w = 1.0;
    let mut got_w = false;
    let mut color = None;
    let mut visible = true;
    for tok in v.split_whitespace() {
        if let Some(px) = parse_len_px(tok, em) {
            w = px;
            got_w = true;
        } else if matches!(tok, "none" | "hidden") {
            visible = false;
        } else if let Some(c) = parse_color(tok) {
            color = Some(c);
        }
        // other style keywords (solid/dashed/dotted/double/…) carry no geometry
    }
    // `border: 0` (explicit zero length) keeps width 0 even though `visible`.
    if got_w && w <= 0.0 {
        visible = false;
    }
    (w, color, visible)
}

/// Set one side's border width and/or colour, keeping `border_color` (the
/// shorthand colour) in step when a single side is the only one styled.
fn set_border_side(style: &mut Style, i: usize, width: Option<f64>, color: Option<[f64; 3]>) {
    if let Some(w) = width {
        match i {
            0 => style.border_width.top = w,
            1 => style.border_width.right = w,
            2 => style.border_width.bottom = w,
            _ => style.border_width.left = w,
        }
    }
    if let Some(c) = color {
        style.border_color_edges[i] = c;
        style.border_color = c;
    }
}

/// `border-color` shorthand: 1–4 colours applied TRBL (with the CSS
/// shorthand fill rules), painting every per-side colour.
fn apply_border_color_shorthand(style: &mut Style, v: &str) {
    let cols: Vec<[f64; 3]> = v.split_whitespace().filter_map(parse_color).collect();
    let edges = match cols.as_slice() {
        [a] => [*a, *a, *a, *a],
        [a, b] => [*a, *b, *a, *b],
        [a, b, c] => [*a, *b, *c, *b],
        [a, b, c, d] => [*a, *b, *c, *d],
        _ => return,
    };
    style.border_color_edges = edges;
    style.border_color = edges[0];
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
        return n
            .trim()
            .parse::<f64>()
            .ok()
            .map(|p| VIEWPORT_W_PT * p / 100.0);
    }
    if let Some(n) = v.strip_suffix("vh") {
        return n
            .trim()
            .parse::<f64>()
            .ok()
            .map(|p| VIEWPORT_H_PT * p / 100.0);
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
        // `#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa`. The alpha nibble/byte is
        // parsed for validity but dropped — the paint layer has no per-fill
        // alpha here, so an opaque RGB is the faithful approximation (the
        // colour is kept rather than the whole declaration being discarded).
        let (r, g, b) = match hex.len() {
            3 | 4 => (
                u8::from_str_radix(&hex[0..1].repeat(2), 16).ok()?,
                u8::from_str_radix(&hex[1..2].repeat(2), 16).ok()?,
                u8::from_str_radix(&hex[2..3].repeat(2), 16).ok()?,
            ),
            6 | 8 => (
                u8::from_str_radix(&hex[0..2], 16).ok()?,
                u8::from_str_radix(&hex[2..4], 16).ok()?,
                u8::from_str_radix(&hex[4..6], 16).ok()?,
            ),
            _ => return None,
        };
        // For the 4-/8-digit forms, validate the alpha component too (so a
        // malformed `#12345` still returns None rather than a half-parsed RGB).
        if hex.len() == 4 {
            u8::from_str_radix(&hex[3..4].repeat(2), 16).ok()?;
        } else if hex.len() == 8 {
            u8::from_str_radix(&hex[6..8], 16).ok()?;
        }
        return Some([r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0]);
    }
    // `rgb()` / `rgba()` — comma- or space-separated; the alpha (4th value, or
    // after a `/`) is ignored but the colour is still returned.
    if let Some(inner) = v
        .strip_prefix("rgba(")
        .or_else(|| v.strip_prefix("rgb("))
        .and_then(|s| s.strip_suffix(')'))
    {
        let nums: Vec<f64> = inner
            .replace('/', " ")
            .split([',', ' '])
            .filter(|t| !t.trim().is_empty())
            .filter_map(|n| parse_rgb_component(n.trim()))
            .collect();
        if nums.len() >= 3 {
            return Some([nums[0], nums[1], nums[2]]);
        }
        return None;
    }
    // `hsl()` / `hsla()` — hue in degrees, saturation/lightness in `%`. Alpha
    // (4th value, or after `/`) ignored.
    if let Some(inner) = v
        .strip_prefix("hsla(")
        .or_else(|| v.strip_prefix("hsl("))
        .and_then(|s| s.strip_suffix(')'))
    {
        let normalized = inner.replace('/', " ");
        let parts: Vec<&str> = normalized
            .split([',', ' '])
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .collect();
        if parts.len() >= 3 {
            let h = parse_angle_deg(parts[0])?;
            let s = parse_percent_unit(parts[1])?;
            let l = parse_percent_unit(parts[2])?;
            return Some(hsl_to_rgb(h, s, l));
        }
        return None;
    }
    named_color(&v)
}

/// One `rgb()` channel → 0..=1. Accepts `0-255` integers/floats and `%`.
fn parse_rgb_component(t: &str) -> Option<f64> {
    if let Some(p) = t.strip_suffix('%') {
        return p.trim().parse::<f64>().ok().map(|n| (n / 100.0).clamp(0.0, 1.0));
    }
    t.parse::<f64>().ok().map(|n| (n / 255.0).clamp(0.0, 1.0))
}

/// An HSL hue angle in degrees (bare number or `deg`), normalised to [0,360).
fn parse_angle_deg(t: &str) -> Option<f64> {
    let t = t.strip_suffix("deg").unwrap_or(t).trim();
    let mut a = t.parse::<f64>().ok()? % 360.0;
    if a < 0.0 {
        a += 360.0;
    }
    Some(a)
}

/// A `%` value → 0..=1 (used for HSL saturation/lightness).
fn parse_percent_unit(t: &str) -> Option<f64> {
    let p = t.strip_suffix('%')?;
    p.trim().parse::<f64>().ok().map(|n| (n / 100.0).clamp(0.0, 1.0))
}

/// Convert HSL (`h` in degrees, `s`/`l` in 0..=1) to linear-free sRGB 0..=1.
fn hsl_to_rgb(h: f64, s: f64, l: f64) -> [f64; 3] {
    if s == 0.0 {
        return [l, l, l]; // achromatic (grey)
    }
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    let hk = h / 360.0;
    let hue = |mut t: f64| {
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 1.0 / 2.0 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    [hue(hk + 1.0 / 3.0), hue(hk), hue(hk - 1.0 / 3.0)]
}

/// The full CSS named-colour table (147 keywords + `transparent`), lower-cased.
/// `transparent` and unknown names return `None` so the property is dropped.
fn named_color(name: &str) -> Option<[f64; 3]> {
    // Stored as 0xRRGGBB for compactness; expanded to 0..=1 floats on hit.
    let rgb: u32 = match name {
        "transparent" => return None,
        "aliceblue" => 0xf0f8ff,
        "antiquewhite" => 0xfaebd7,
        "aqua" | "cyan" => 0x00ffff,
        "aquamarine" => 0x7fffd4,
        "azure" => 0xf0ffff,
        "beige" => 0xf5f5dc,
        "bisque" => 0xffe4c4,
        "black" => 0x000000,
        "blanchedalmond" => 0xffebcd,
        "blue" => 0x0000ff,
        "blueviolet" => 0x8a2be2,
        "brown" => 0xa52a2a,
        "burlywood" => 0xdeb887,
        "cadetblue" => 0x5f9ea0,
        "chartreuse" => 0x7fff00,
        "chocolate" => 0xd2691e,
        "coral" => 0xff7f50,
        "cornflowerblue" => 0x6495ed,
        "cornsilk" => 0xfff8dc,
        "crimson" => 0xdc143c,
        "darkblue" => 0x00008b,
        "darkcyan" => 0x008b8b,
        "darkgoldenrod" => 0xb8860b,
        "darkgray" | "darkgrey" => 0xa9a9a9,
        "darkgreen" => 0x006400,
        "darkkhaki" => 0xbdb76b,
        "darkmagenta" => 0x8b008b,
        "darkolivegreen" => 0x556b2f,
        "darkorange" => 0xff8c00,
        "darkorchid" => 0x9932cc,
        "darkred" => 0x8b0000,
        "darksalmon" => 0xe9967a,
        "darkseagreen" => 0x8fbc8f,
        "darkslateblue" => 0x483d8b,
        "darkslategray" | "darkslategrey" => 0x2f4f4f,
        "darkturquoise" => 0x00ced1,
        "darkviolet" => 0x9400d3,
        "deeppink" => 0xff1493,
        "deepskyblue" => 0x00bfff,
        "dimgray" | "dimgrey" => 0x696969,
        "dodgerblue" => 0x1e90ff,
        "firebrick" => 0xb22222,
        "floralwhite" => 0xfffaf0,
        "forestgreen" => 0x228b22,
        "fuchsia" | "magenta" => 0xff00ff,
        "gainsboro" => 0xdcdcdc,
        "ghostwhite" => 0xf8f8ff,
        "gold" => 0xffd700,
        "goldenrod" => 0xdaa520,
        "gray" | "grey" => 0x808080,
        "green" => 0x008000,
        "greenyellow" => 0xadff2f,
        "honeydew" => 0xf0fff0,
        "hotpink" => 0xff69b4,
        "indianred" => 0xcd5c5c,
        "indigo" => 0x4b0082,
        "ivory" => 0xfffff0,
        "khaki" => 0xf0e68c,
        "lavender" => 0xe6e6fa,
        "lavenderblush" => 0xfff0f5,
        "lawngreen" => 0x7cfc00,
        "lemonchiffon" => 0xfffacd,
        "lightblue" => 0xadd8e6,
        "lightcoral" => 0xf08080,
        "lightcyan" => 0xe0ffff,
        "lightgoldenrodyellow" => 0xfafad2,
        "lightgray" | "lightgrey" => 0xd3d3d3,
        "lightgreen" => 0x90ee90,
        "lightpink" => 0xffb6c1,
        "lightsalmon" => 0xffa07a,
        "lightseagreen" => 0x20b2aa,
        "lightskyblue" => 0x87cefa,
        "lightslategray" | "lightslategrey" => 0x778899,
        "lightsteelblue" => 0xb0c4de,
        "lightyellow" => 0xffffe0,
        "lime" => 0x00ff00,
        "limegreen" => 0x32cd32,
        "linen" => 0xfaf0e6,
        "maroon" => 0x800000,
        "mediumaquamarine" => 0x66cdaa,
        "mediumblue" => 0x0000cd,
        "mediumorchid" => 0xba55d3,
        "mediumpurple" => 0x9370db,
        "mediumseagreen" => 0x3cb371,
        "mediumslateblue" => 0x7b68ee,
        "mediumspringgreen" => 0x00fa9a,
        "mediumturquoise" => 0x48d1cc,
        "mediumvioletred" => 0xc71585,
        "midnightblue" => 0x191970,
        "mintcream" => 0xf5fffa,
        "mistyrose" => 0xffe4e1,
        "moccasin" => 0xffe4b5,
        "navajowhite" => 0xffdead,
        "navy" => 0x000080,
        "oldlace" => 0xfdf5e6,
        "olive" => 0x808000,
        "olivedrab" => 0x6b8e23,
        "orange" => 0xffa500,
        "orangered" => 0xff4500,
        "orchid" => 0xda70d6,
        "palegoldenrod" => 0xeee8aa,
        "palegreen" => 0x98fb98,
        "paleturquoise" => 0xafeeee,
        "palevioletred" => 0xdb7093,
        "papayawhip" => 0xffefd5,
        "peachpuff" => 0xffdab9,
        "peru" => 0xcd853f,
        "pink" => 0xffc0cb,
        "plum" => 0xdda0dd,
        "powderblue" => 0xb0e0e6,
        "purple" => 0x800080,
        "rebeccapurple" => 0x663399,
        "red" => 0xff0000,
        "rosybrown" => 0xbc8f8f,
        "royalblue" => 0x4169e1,
        "saddlebrown" => 0x8b4513,
        "salmon" => 0xfa8072,
        "sandybrown" => 0xf4a460,
        "seagreen" => 0x2e8b57,
        "seashell" => 0xfff5ee,
        "sienna" => 0xa0522d,
        "silver" => 0xc0c0c0,
        "skyblue" => 0x87ceeb,
        "slateblue" => 0x6a5acd,
        "slategray" | "slategrey" => 0x708090,
        "snow" => 0xfffafa,
        "springgreen" => 0x00ff7f,
        "steelblue" => 0x4682b4,
        "tan" => 0xd2b48c,
        "teal" => 0x008080,
        "thistle" => 0xd8bfd8,
        "tomato" => 0xff6347,
        "turquoise" => 0x40e0d0,
        "violet" => 0xee82ee,
        "wheat" => 0xf5deb3,
        "white" => 0xffffff,
        "whitesmoke" => 0xf5f5f5,
        "yellow" => 0xffff00,
        "yellowgreen" => 0x9acd32,
        _ => return None,
    };
    Some([
        ((rgb >> 16) & 0xff) as f64 / 255.0,
        ((rgb >> 8) & 0xff) as f64 / 255.0,
        (rgb & 0xff) as f64 / 255.0,
    ])
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
u, ins { text-decoration: underline; }
s, strike, del { text-decoration: line-through; }
sup { font-size: 0.75em; vertical-align: super; }
sub { font-size: 0.75em; vertical-align: sub; }
mark { background: #ffff00; }
a { color: #0645ad; text-decoration: underline; }
ul, ol { margin-top: 8pt; margin-bottom: 8pt; padding-left: 30pt; }
li { display: list-item; }
blockquote { margin-left: 30pt; margin-top: 8pt; margin-bottom: 8pt; }
pre { display: block; white-space: pre; font-family: monospace; margin-top: 8pt; margin-bottom: 8pt; }
code, kbd, samp { font-family: monospace; }
table { display: table; border-collapse: collapse; }
tr { display: table-row; }
td, th { display: table-cell; padding: 2pt; border: 1pt solid #c0c0c0; vertical-align: top; }
th { font-weight: bold; text-align: center; }
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

    // Helper: are two colours equal within a small tolerance?
    fn approx(a: [f64; 3], b: [f64; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 0.01)
    }

    #[test]
    fn extended_named_colors_are_not_dropped() {
        // Quick-win 1: previously-unknown names now resolve instead of being
        // discarded (which made the property fall back to black/no-fill).
        assert!(approx(
            parse_color("cornflowerblue").unwrap(),
            [0x64 as f64 / 255.0, 0x95 as f64 / 255.0, 0xed as f64 / 255.0],
        ));
        assert!(approx(parse_color("crimson").unwrap(), [0.863, 0.078, 0.235]));
        assert!(approx(parse_color("gold").unwrap(), [1.0, 0.843, 0.0]));
        assert!(approx(parse_color("darkblue").unwrap(), [0.0, 0.0, 0.545]));
        assert_eq!(parse_color("rebeccapurple"), Some([0.4, 0.2, 0.6]));
        // Case-insensitive + cyan/magenta/fuchsia/aqua aliases.
        assert_eq!(parse_color("CornflowerBlue"), parse_color("cornflowerblue"));
        assert_eq!(parse_color("cyan"), parse_color("aqua"));
        // Genuinely unknown name still drops.
        assert_eq!(parse_color("notacolor"), None);
    }

    #[test]
    fn rgba_keeps_colour_and_ignores_alpha() {
        // `rgba(...)` returns the RGB (alpha dropped, but the colour is kept).
        assert!(approx(parse_color("rgba(255, 0, 0, 0.5)").unwrap(), [1.0, 0.0, 0.0]));
        // Space-separated + slash-alpha form.
        assert!(approx(parse_color("rgb(0 128 0 / 50%)").unwrap(), [0.0, 0.502, 0.0]));
        // Percentage channels.
        assert!(approx(parse_color("rgb(100%, 0%, 0%)").unwrap(), [1.0, 0.0, 0.0]));
    }

    #[test]
    fn hsl_and_hsla_convert_to_rgb() {
        // hsl(0,100%,50%) = red, hsl(120,100%,50%) = green, hsl(240,…) = blue.
        assert!(approx(parse_color("hsl(0, 100%, 50%)").unwrap(), [1.0, 0.0, 0.0]));
        assert!(approx(parse_color("hsl(120, 100%, 50%)").unwrap(), [0.0, 1.0, 0.0]));
        assert!(approx(parse_color("hsl(240, 100%, 50%)").unwrap(), [0.0, 0.0, 1.0]));
        // Saturation 0 → grey at the lightness level.
        assert!(approx(parse_color("hsl(0, 0%, 50%)").unwrap(), [0.5, 0.5, 0.5]));
        // hsla keeps the colour, drops alpha.
        assert!(approx(parse_color("hsla(0,100%,50%,0.3)").unwrap(), [1.0, 0.0, 0.0]));
    }

    #[test]
    fn hex_with_alpha_keeps_rgb() {
        // `#rgba` / `#rrggbbaa` — the alpha nibble/byte is validated then dropped.
        assert_eq!(parse_color("#ff0000ff"), Some([1.0, 0.0, 0.0]));
        assert_eq!(parse_color("#f00f"), Some([1.0, 0.0, 0.0]));
        assert!(approx(parse_color("#aabbccdd").unwrap(), [0.667, 0.733, 0.8]));
        // A malformed 5-digit hex is still rejected.
        assert_eq!(parse_color("#12345"), None);
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

    // Compute the style of the first element from an inline `style` string.
    fn inline_style(decls: &str) -> Style {
        let mut s = Style::default();
        for (k, v) in parse_decls(decls) {
            apply_one(&mut s, &k, &v);
        }
        s
    }

    #[test]
    fn per_side_border_widths_and_colors_parse() {
        let s = inline_style(
            "border:none;border-bottom:3pt solid #ff0000;border-left:2pt solid #0000ff",
        );
        assert_eq!(s.border_width.top, 0.0, "border:none zeroed the top");
        assert_eq!(s.border_width.right, 0.0);
        assert_eq!(s.border_width.bottom, 3.0, "border-bottom width");
        assert_eq!(s.border_width.left, 2.0, "border-left width");
        // TRBL colour edges: bottom=red, left=blue, others stay default black.
        assert_eq!(s.border_color_edges[2], [1.0, 0.0, 0.0]);
        assert_eq!(s.border_color_edges[3], [0.0, 0.0, 1.0]);
    }

    #[test]
    fn border_shorthand_syncs_all_four_color_edges() {
        let s = inline_style("border:1pt solid #00ff00");
        assert_eq!(s.border_width.top, 1.0);
        assert_eq!(s.border_color, [0.0, 1.0, 0.0]);
        assert_eq!(s.border_color_edges, [[0.0, 1.0, 0.0]; 4]);
    }

    #[test]
    fn border_color_shorthand_is_trbl() {
        let s = inline_style("border-color:#ff0000 #00ff00 #0000ff");
        assert_eq!(s.border_color_edges[0], [1.0, 0.0, 0.0], "top");
        assert_eq!(s.border_color_edges[1], [0.0, 1.0, 0.0], "right");
        assert_eq!(s.border_color_edges[2], [0.0, 0.0, 1.0], "bottom");
        assert_eq!(s.border_color_edges[3], [0.0, 1.0, 0.0], "left = right");
    }

    #[test]
    fn border_style_none_suppresses_a_declared_width() {
        // width then style:none → side suppressed (CSS: a none side has no rule).
        let s = inline_style("border-bottom-width:4pt;border-bottom-style:none");
        assert_eq!(s.border_width.bottom, 0.0);
    }

    #[test]
    fn multi_column_properties_parse() {
        // `column-count` sets the count; `column-gap` sets the gutter (reusing
        // gap_col); `auto`/`normal` leave the defaults.
        assert_eq!(inline_style("column-count:3").column_count, 3);
        assert_eq!(inline_style("column-count:auto").column_count, 0);
        let g = inline_style("column-count:2;column-gap:24pt");
        assert_eq!(g.column_count, 2);
        assert_eq!(g.gap_col, 24.0);
        assert_eq!(inline_style("column-gap:normal").gap_col, 0.0);
        // `columns` shorthand: the integer token is the count, a length is the
        // (ignored) column-width, in either order.
        assert_eq!(inline_style("columns:3").column_count, 3);
        assert_eq!(inline_style("columns:200px 2").column_count, 2);
        assert_eq!(inline_style("columns:2 200px").column_count, 2);
        // A sole column-width (no count) leaves the block single-column.
        assert_eq!(inline_style("columns:200px").column_count, 0);
        assert_eq!(inline_style("columns:auto").column_count, 0);
        // `column-count` is not inherited.
        let parent = Style {
            column_count: 3,
            ..Style::default()
        };
        assert_eq!(inherit(&parent).column_count, 0, "column-count does not inherit");
    }

    #[test]
    fn vertical_align_and_collapse_parse() {
        assert_eq!(
            inline_style("vertical-align:middle").vertical_align,
            VAlign::Middle
        );
        assert_eq!(
            inline_style("vertical-align:bottom").vertical_align,
            VAlign::Bottom
        );
        assert!(inline_style("border-collapse:collapse").border_collapse);
        assert!(!inline_style("border-collapse:separate").border_collapse);
    }

    #[test]
    fn inline_vertical_align_super_sub_parse() {
        assert_eq!(inline_style("vertical-align:super").valign, VShift::Super);
        assert_eq!(inline_style("vertical-align:sub").valign, VShift::Sub);
        assert_eq!(
            inline_style("vertical-align:baseline").valign,
            VShift::Baseline
        );
        // A length resolves to points (positive-up retained in the variant).
        match inline_style("font-size:20pt;vertical-align:4pt").valign {
            VShift::Points(p) => assert!((p - 4.0).abs() < 0.5, "4pt → ~4 points ({p})"),
            other => panic!("expected Points, got {other:?}"),
        }
    }

    #[test]
    fn cascade_resolves_super_sub_shift_from_parent_em() {
        // <sup> inside a 20pt parent: UA shrinks it to 15pt, and the shift is
        // computed from the PARENT em (20pt), top-down negative = raised.
        let nodes = parse("<p style=\"font-size:20pt\">x<sup>2</sup></p>");
        let p = nodes
            .iter()
            .find_map(|n| match n {
                Node::Element(e) if e.tag == "p" => Some(e),
                _ => None,
            })
            .unwrap();
        let sheet = Stylesheet::new("");
        let pstyle = sheet.computed(p, &Style::default(), &[]);
        let sup = p
            .children
            .iter()
            .find_map(|n| match n {
                Node::Element(e) if e.tag == "sup" => Some(e),
                _ => None,
            })
            .unwrap();
        let sup_style = sheet.computed(sup, &pstyle, &[p]);
        assert!(
            sup_style.font_size < pstyle.font_size,
            "superscript shrinks ({} < {})",
            sup_style.font_size,
            pstyle.font_size
        );
        // Parent em 20pt × 0.33 ≈ 6.6, raised ⇒ negative.
        assert!(
            sup_style.valign_shift < -3.0,
            "superscript raised from parent em (shift {})",
            sup_style.valign_shift
        );
    }

    #[test]
    fn ua_table_defaults_collapse_and_header_center() {
        // <table> defaults to collapse and inherits it to its <th>, which also
        // defaults to bold + centred text.
        let nodes = parse("<table><tr><th>H</th></tr></table>");
        let sheet = Stylesheet::new("");
        let table = nodes
            .iter()
            .find_map(|n| match n {
                Node::Element(e) if e.tag == "table" => Some(e),
                _ => None,
            })
            .unwrap();
        let tstyle = sheet.computed(table, &Style::default(), &[]);
        assert!(tstyle.border_collapse, "table defaults to border-collapse");

        // Resolve the <th> with the table as ancestor/parent (collapse inherits).
        let tr = match &table.children[0] {
            Node::Element(e) => e,
            _ => panic!(),
        };
        let th = match &tr.children[0] {
            Node::Element(e) => e,
            _ => panic!(),
        };
        let th_style = sheet.computed(th, &tstyle, &[table, tr]);
        assert!(th_style.bold, "th is bold");
        assert_eq!(th_style.align, Align::Center, "th text-align is center");
        assert!(th_style.border_collapse, "collapse inherited into the cell");
    }

    // ── Quick-win 2: combinators + attribute selectors ─────────────────────

    /// Compute the colour of the first element with `tag` in the tree, threading
    /// the full ancestor chain so descendant/child/sibling combinators resolve.
    fn color_of_first(html: &str, tag: &str) -> [f64; 3] {
        let nodes = parse(html);
        let sheet = Stylesheet::new(&collect_style_css(&nodes));
        fn walk<'a>(
            nodes: &'a [Node],
            sheet: &Stylesheet,
            parent: &Style,
            chain: &mut Vec<&'a Element>,
            target: &str,
            out: &mut Option<[f64; 3]>,
        ) {
            for n in nodes {
                if let Node::Element(e) = n {
                    let st = sheet.computed(e, parent, chain);
                    if e.tag == target && out.is_none() {
                        *out = Some(st.color);
                    }
                    chain.push(e);
                    walk(&e.children, sheet, &st, chain, target, out);
                    chain.pop();
                }
            }
        }
        let mut out = None;
        walk(&nodes, &sheet, &Style::default(), &mut Vec::new(), tag, &mut out);
        out.expect("target element not found")
    }

    const RED: [f64; 3] = [1.0, 0.0, 0.0];
    const GREEN_LIME: [f64; 3] = [0.0, 1.0, 0.0];

    #[test]
    fn child_combinator_matches_only_direct_child() {
        // `div > p` colours a direct child <p> but NOT a grandchild <p>.
        let direct = color_of_first(
            "<style>div > p { color: lime }</style><div><p>x</p></div>",
            "p",
        );
        assert_eq!(direct, GREEN_LIME, "direct child matched");

        let nested = color_of_first(
            "<style>div > p { color: lime }</style><div><span><p>x</p></span></div>",
            "p",
        );
        assert_eq!(nested, [0.0, 0.0, 0.0], "grandchild not matched by `>`");

        // The descendant combinator still matches the grandchild (non-regression).
        let desc = color_of_first(
            "<style>div p { color: lime }</style><div><span><p>x</p></span></div>",
            "p",
        );
        assert_eq!(desc, GREEN_LIME, "descendant combinator still works");
    }

    #[test]
    fn adjacent_sibling_combinator() {
        // `h2 + p` colours the <p> immediately after an <h2> (whitespace/text
        // between tags is ignored).
        let matched = color_of_first(
            "<style>h2 + p { color: red }</style><div><h2>t</h2> <p>x</p></div>",
            "p",
        );
        assert_eq!(matched, RED, "adjacent sibling matched");

        // A <p> NOT immediately after an <h2> is not matched.
        let not_adjacent = color_of_first(
            "<style>h2 + p { color: red }</style><div><h2>t</h2><span>s</span><p>x</p></div>",
            "p",
        );
        assert_eq!(not_adjacent, [0.0, 0.0, 0.0], "non-adjacent not matched");
    }

    #[test]
    fn general_sibling_combinator() {
        // `h2 ~ p` colours ANY <p> that follows an <h2> under the same parent.
        let matched = color_of_first(
            "<style>h2 ~ p { color: red }</style><div><h2>t</h2><span>s</span><p>x</p></div>",
            "p",
        );
        assert_eq!(matched, RED, "general sibling matched across a non-sibling");

        // A <p> BEFORE the <h2> is not matched.
        let before = color_of_first(
            "<style>h2 ~ p { color: red }</style><div><p>x</p><h2>t</h2></div>",
            "p",
        );
        assert_eq!(before, [0.0, 0.0, 0.0], "preceding sibling not matched");
    }

    #[test]
    fn attribute_selectors_presence_and_value() {
        // `[data-x]` presence test.
        let present = color_of_first(
            "<style>[data-x] { color: lime }</style><p data-x=\"1\">x</p>",
            "p",
        );
        assert_eq!(present, GREEN_LIME, "[data-x] presence matched");

        let absent = color_of_first(
            "<style>[data-x] { color: lime }</style><p>x</p>",
            "p",
        );
        assert_eq!(absent, [0.0, 0.0, 0.0], "missing attribute not matched");

        // `[type=text]` exact-value test (quoted or not).
        let exact = color_of_first(
            "<style>input[type=text] { color: red }</style><input type=\"text\">",
            "input",
        );
        assert_eq!(exact, RED, "[type=text] value matched");

        let wrong = color_of_first(
            "<style>input[type=text] { color: red }</style><input type=\"password\">",
            "input",
        );
        assert_eq!(wrong, [0.0, 0.0, 0.0], "wrong value not matched");
    }

    #[test]
    fn combinators_do_not_break_class_or_id_selectors() {
        // Non-regression: plain class + id selectors still resolve.
        let by_class = color_of_first(
            "<style>.hl { color: red }</style><p class=\"hl\">x</p>",
            "p",
        );
        assert_eq!(by_class, RED, "class selector still works");

        let by_id = color_of_first(
            "<style>#a { color: lime }</style><p id=\"a\">x</p>",
            "p",
        );
        assert_eq!(by_id, GREEN_LIME, "id selector still works");

        // Compound child: `.box > p.note` exercises classes on both sides.
        let compound = color_of_first(
            "<style>.box > p.note { color: red }</style><div class=\"box\"><p class=\"note\">x</p></div>",
            "p",
        );
        assert_eq!(compound, RED, "compound child combinator matched");
    }

    // ── Quick-win 3: margin auto parsing ───────────────────────────────────

    #[test]
    fn margin_auto_flags_parse() {
        // `margin: 0 auto` sets both horizontal-auto flags (centring intent).
        let s = inline_style("margin: 0 auto");
        assert!(s.margin_left_auto && s.margin_right_auto, "0 auto → both auto");
        // Longhands.
        assert!(inline_style("margin-left: auto").margin_left_auto);
        assert!(inline_style("margin-right: auto").margin_right_auto);
        // 4-value form: `margin: 0 auto 0 5pt` → right auto, left fixed.
        let four = inline_style("margin: 0 auto 0 5pt");
        assert!(four.margin_right_auto && !four.margin_left_auto);
        assert_eq!(four.margin.left, 5.0);
        // A plain numeric margin sets neither flag.
        let fixed = inline_style("margin: 10pt");
        assert!(!fixed.margin_left_auto && !fixed.margin_right_auto);
    }

    // ── Quick-win 5: page-break-inside parsing ─────────────────────────────

    #[test]
    fn page_break_inside_avoid_parses() {
        assert!(inline_style("page-break-inside: avoid").page_break_inside_avoid);
        assert!(inline_style("break-inside: avoid").page_break_inside_avoid);
        assert!(!inline_style("page-break-inside: auto").page_break_inside_avoid);
    }
}
