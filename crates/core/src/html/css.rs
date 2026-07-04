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
///
/// `Start`/`End` are *direction-relative*: they resolve to `Left`/`Right` (or the
/// reverse) only at layout time, against the element's [`Direction`]. Keeping them
/// as distinct variants — rather than collapsing them to `Left` at parse time —
/// is what lets `text-align: start` follow `direction: rtl`. The default stays
/// `Left` (not `Start`) so every existing LTR document lays out byte-identically:
/// an absent `text-align` is `Left`, never the direction-relative path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
    Justify,
    /// `text-align: start` — the line's leading edge (left in LTR, right in RTL).
    Start,
    /// `text-align: end` — the line's trailing edge (right in LTR, left in RTL).
    End,
}

/// CSS `direction` — the inline base direction of a box, set by the `direction`
/// property or the `dir="ltr|rtl"` HTML attribute. Inherited. The default is
/// [`Direction::Ltr`], so the whole RTL machinery is dormant for every existing
/// document and the LTR layout paths are reached unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Direction {
    /// `ltr` — left-to-right (the default).
    #[default]
    Ltr,
    /// `rtl` — right-to-left (Hebrew, Arabic). Inline boxes flow from the right
    /// edge leftward; the default text alignment becomes `right`.
    Rtl,
}

impl Align {
    /// Resolve a direction-relative alignment against `dir`. `Start`/`End` map to
    /// the physical `Left`/`Right`; the physical and justify variants pass
    /// through unchanged. This is the single point where direction influences the
    /// horizontal placement of a line.
    pub fn resolve(self, dir: Direction) -> Align {
        match (self, dir) {
            (Align::Start, Direction::Ltr) => Align::Left,
            (Align::Start, Direction::Rtl) => Align::Right,
            (Align::End, Direction::Ltr) => Align::Right,
            (Align::End, Direction::Rtl) => Align::Left,
            (other, _) => other,
        }
    }
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
    /// `top` / `text-top` — the run's top hugs the line box top. Resolved to a
    /// point offset at line-assembly time (the line height isn't known during
    /// the cascade), so `Style::valign_shift` stays `0.0` for it.
    Top,
    /// `middle` — the run is centred within the line box (resolved at line
    /// assembly, like [`VShift::Top`]).
    Middle,
    /// `bottom` / `text-bottom` — the run's bottom hugs the line box bottom
    /// (resolved at line assembly).
    Bottom,
}

/// CSS `white-space` — how whitespace collapses and where lines may break.
/// The legacy [`Style::pre`] flag stays in lock-step (`Pre`/`PreWrap` ⇒ `pre`)
/// so every existing consumer of `pre` behaves exactly as before.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WhiteSpace {
    /// `normal` (the default) — collapse whitespace, wrap freely.
    #[default]
    Normal,
    /// `nowrap` — collapse whitespace, never wrap.
    Nowrap,
    /// `pre` — preserve whitespace and newlines, never wrap.
    Pre,
    /// `pre-wrap` — preserve whitespace and newlines, but wrap when needed.
    PreWrap,
    /// `pre-line` — collapse runs of spaces but preserve newlines; wraps.
    PreLine,
}

/// CSS `object-fit` — how an `<img>`'s pixels map into its layout box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ObjectFit {
    /// `fill` (the default) — stretch to the box, ignoring the aspect ratio.
    #[default]
    Fill,
    /// `contain` — largest scaled size that fits inside the box (letterboxed).
    Contain,
    /// `cover` — smallest scaled size that covers the box (clipped).
    Cover,
    /// `none` — intrinsic size, centred, clipped to the box.
    None,
    /// `scale-down` — the smaller of `none` and `contain`.
    ScaleDown,
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
    /// `sticky` — laid out in flow like `relative`, then offset by `inset`, but
    /// the shift is **clamped to the containing block** so the box never leaves
    /// its parent's content box. On a statically-paginated PDF there is no
    /// scrolling viewport to stick to, so this is the faithful static
    /// approximation: a `relative` shift bounded by the container (documented
    /// limitation — true scroll-sticky behaviour is not modelled). Still
    /// occupies its normal space.
    Sticky,
}

/// CSS `writing-mode`. The vertical modes are supported at a pragmatic first
/// level: the block's content is laid out **horizontally**, then the whole
/// subtree is rotated 90° (clockwise for `vertical-rl`, counter-clockwise for
/// `vertical-lr`) with the box's width/height swapped — the "rotated label"
/// approximation that matches the real-world uses (table headers, spine
/// labels). True vertical CJK line layout (upright glyphs, vertical metrics,
/// line stacking) is out of scope and documented as such.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WritingMode {
    /// `horizontal-tb` — normal horizontal layout (the default).
    #[default]
    HorizontalTb,
    /// `vertical-rl` — rendered as the horizontal layout rotated 90° clockwise.
    VerticalRl,
    /// `vertical-lr` — rendered rotated 90° counter-clockwise.
    VerticalLr,
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

/// How a border side's line is drawn. Only the line *style* — the width lives in
/// [`Edges`] and the colour in `border_color_edges`. `Solid` keeps the legacy
/// filled-rectangle rendering; the others change how the side is stroked. Any
/// unrecognised `border-style` keyword falls back to `Solid` (tolerant parsing),
/// so the visual is unchanged for every value we don't special-case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BorderStyle {
    /// A continuous line (the default; rendered as a filled rectangle, exactly
    /// as before this style existed).
    #[default]
    Solid,
    /// A row of dashes — long on/off segments along the side.
    Dashed,
    /// A row of dots — short square on/off segments (dash length ≈ width).
    Dotted,
    /// Two parallel thin lines with a gap between them.
    Double,
    /// 3-D bevels — top/left edges and bottom/right edges take a darker or
    /// lighter shade of the colour to fake depth. `Inset`/`Outset` shade each
    /// side as one tone; `Groove`/`Ridge` split each side into an outer and inner
    /// half with opposite tones (a carved groove / raised ridge).
    Inset,
    Outset,
    Groove,
    Ridge,
}

/// A single `box-shadow` layer, resolved to points.
///
/// The paint layer offsets the box by `(dx, dy)`, grows it by `spread` on every
/// side, and fills it in `color`. When `blur > 0` it renders a **soft edge** by
/// stacking concentric rings of decreasing alpha out to the blur radius (a
/// multi-pass box-blur approximation of a Gaussian falloff — a true blurred drop
/// shadow, not just a dimmed block). An `inset` shadow is instead painted as a
/// shadow-coloured frame *inside* the box (offset + `spread + blur` reach),
/// clipped to the box so it reads as recessed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoxShadow {
    /// Horizontal offset in points (positive → right).
    pub dx: f64,
    /// Vertical offset in points (positive → down).
    pub dy: f64,
    /// Blur radius in points. Drives the width of the soft edge (and, at `0`,
    /// gives a hard offset rectangle).
    pub blur: f64,
    /// Spread distance in points (grows the shadow rect on every side).
    pub spread: f64,
    /// Shadow colour (RGB 0..=1); any alpha in the source colour is dropped.
    pub color: [f64; 3],
    /// `inset` keyword — painted as a clipped inner frame (recessed look).
    pub inset: bool,
}

/// A CSS `text-shadow` layer: an offset (`dx`, `dy`) copy of the glyphs in
/// `color` (with `alpha`). `blur` is approximated at paint time (a small spread
/// of extra offset passes, not a true Gaussian). `text-shadow` inherits, so this
/// rides on the inherited `Style` fields.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextShadow {
    pub dx: f64,
    pub dy: f64,
    pub blur: f64,
    pub color: [f64; 3],
    pub alpha: f64,
}

/// A CSS `linear-gradient(...)` background. Stops carry an RGB colour and an
/// optional position (`0.0..=1.0`); a `None` position is spread evenly between
/// its positioned neighbours at paint time (CSS default-stop placement). The
/// `angle_deg` follows the CSS convention: `0deg` points to the top, `90deg` to
/// the right, increasing clockwise (`to right` ≡ `90deg`, `to bottom` ≡
/// `180deg`).
#[derive(Debug, Clone)]
pub struct LinearGradient {
    /// Gradient line direction in CSS degrees (`0` = upward, clockwise).
    pub angle_deg: f64,
    /// Colour stops in declaration order; `pos` is `0..=1` or `None`.
    pub stops: Vec<GradientStop>,
}

/// A CSS `radial-gradient(...)` background. Modelled as a circle centred at
/// `(cx, cy)` (fractions of the box, `0..=1`) reaching radius `r` (a fraction of
/// the box's smaller half-extent — `0.5` ≈ the `closest-side` of a square). The
/// colour ramp runs from the centre (`offset 0`) outward to `r` (`offset 1`).
/// Elliptical sizing and the `farthest-corner`/explicit-length forms collapse to
/// this circular approximation (good enough for a background fill); the stops use
/// the same auto-placement rules as the linear case.
#[derive(Debug, Clone)]
pub struct RadialGradient {
    /// Centre X as a fraction of the box width (`0..=1`, `0.5` = middle).
    pub cx: f64,
    /// Centre Y as a fraction of the box height (`0..=1`, `0.5` = middle).
    pub cy: f64,
    /// End radius as a fraction of `min(w, h) / 2` (`1.0` ≈ closest-side).
    pub r: f64,
    /// Colour stops, centre→edge.
    pub stops: Vec<GradientStop>,
}

/// A CSS `conic-gradient(...)` background: colour sweeps **angularly** around the
/// centre `(cx, cy)` (fractions of the box), starting at `from_deg` (CSS: `0deg`
/// points up, increasing clockwise) and wrapping once. There is no native PDF
/// shading for a conic sweep, so the paint layer approximates it with a fan of
/// flat-coloured triangular sectors (a vector approximation — no raster); enough
/// sectors that the banding is invisible at print resolution. Stop positions are
/// fractions of the full turn (`0..=1`), auto-placed like the other gradients.
#[derive(Debug, Clone)]
pub struct ConicGradient {
    /// Centre X as a fraction of the box width (`0..=1`).
    pub cx: f64,
    /// Centre Y as a fraction of the box height (`0..=1`).
    pub cy: f64,
    /// Sweep start angle in CSS degrees (`0` = up, clockwise).
    pub from_deg: f64,
    /// Colour stops around the turn; `pos` is a `0..=1` fraction of the turn.
    pub stops: Vec<GradientStop>,
}

/// A CSS gradient background in any of the three shapes the engine paints. The
/// `Linear` arm is the original [`LinearGradient`] and its paint path is
/// unchanged (byte-for-byte) — `Radial`/`Conic` are additive. Carried by
/// `Style::background_gradient` and `Fragment::Gradient`.
#[derive(Debug, Clone)]
pub enum CssGradient {
    Linear(LinearGradient),
    Radial(RadialGradient),
    Conic(ConicGradient),
}

/// One gradient colour stop: an RGB colour, an optional position
/// (`0.0..=1.0`, `None` = auto-placed) and the stop's own opacity. Shared by
/// linear / radial / conic.
#[derive(Debug, Clone, Copy)]
pub struct GradientStop {
    pub color: [f64; 3],
    pub pos: Option<f64>,
    /// Stop opacity, `0..=1`. `1.0` for plain colours; a `transparent` stop is
    /// **kept** with `0.0` (its colour borrowed from the nearest opaque
    /// neighbour so the ramp fades out in place rather than towards black),
    /// and an `rgba()`/`#rgba` stop carries its own alpha.
    pub alpha: f64,
}

/// A fully-resolved computed style for one element.
#[derive(Debug, Clone)]
pub struct Style {
    pub display: Display,
    pub color: [f64; 3],
    /// Alpha of `color` (0..=1) from an `rgba()`/`hsla()`/`#rgba` text colour;
    /// folded into the text's paint opacity. Inherited, like `color`.
    pub color_alpha: f64,
    pub background: Option<[f64; 3]>,
    /// Alpha of `background` (0..=1); folded into the background fill's opacity.
    pub background_alpha: f64,
    pub font_size: f64,
    pub font_family: String,
    pub generic_serif: bool,
    pub generic_mono: bool,
    pub bold: bool,
    /// Numeric `font-weight` (100–900), preserved verbatim even though the
    /// renderer currently only distinguishes bold from regular. Kept so callers
    /// (e.g. variant-aware face selection) can pick a graduated weight later;
    /// `bold` stays the convenience flag (`weight >= 600`). Inherited.
    pub font_weight: u16,
    pub italic: bool,
    pub underline: bool,
    pub align: Align,
    /// `direction` (`ltr`/`rtl`) — the inline base direction, from the CSS
    /// `direction` property or the `dir` HTML attribute. Inherited. Drives RTL
    /// block alignment, inline box ordering, and run placement. Defaults to
    /// `ltr`, keeping every existing layout unchanged.
    pub direction: Direction,
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
    /// Alpha of the border colour (0..=1) from an `rgba()`/`hsla()`/`#rgba`;
    /// folded into the border's paint opacity (applied to every side).
    pub border_color_alpha: f64,
    /// Per-side border colours in `[top, right, bottom, left]` order. Each side
    /// defaults to `border_color`; the `border-{top,right,bottom,left}[-color]`
    /// longhands override an individual side, letting a cell stroke (say) only
    /// a coloured bottom rule.
    pub border_color_edges: [[f64; 3]; 4],
    /// Per-side border line styles in `[top, right, bottom, left]` order. Each
    /// side defaults to `Solid` (the legacy filled-rectangle rendering), so a
    /// border without an explicit style — or with one we don't special-case —
    /// looks exactly as it did before. `Dashed`/`Dotted`/`Double` change only
    /// how that side is drawn, never its geometry. Not inherited.
    pub border_style_edges: [BorderStyle; 4],
    /// `background[-image]: linear-gradient(...) | radial-gradient(...) |
    /// conic-gradient(...)`. When set it paints over (or in place of) the solid
    /// `background`. `None` for the overwhelming majority of boxes, keeping the
    /// flat-fill path untouched. Not inherited.
    pub background_gradient: Option<CssGradient>,
    /// `background-image: url(...)` — the raster source (a `data:` URI or an
    /// external URL the host pre-downloads). Painted to fill the box behind its
    /// content. Not inherited.
    pub background_image: Option<String>,
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
    /// `flex-direction: row-reverse` / `column-reverse` — items run from the
    /// far end of the main axis toward the start.
    pub flex_reverse: bool,
    /// `justify-content` along the main axis.
    pub justify: Justify,
    /// `flex` / `flex-grow` factor (a flex item's share of free space).
    pub flex_grow: f64,
    /// `flex-shrink` factor (a flex item's share of overflow reduction). The CSS
    /// initial value is `1`; the `flex`/`flex-grow` longhands leave it untouched
    /// unless the `flex` shorthand provides a second number. Not inherited.
    pub flex_shrink: f64,
    /// `flex-basis` — a flex item's initial main-size before grow/shrink. `None`
    /// means `auto` (fall back to `width`, then content). Not inherited.
    pub flex_basis: Option<Len>,
    /// `grid-template-columns` → number of columns (0 = not a grid). Kept as the
    /// authoritative column COUNT; the detailed per-track sizings live in
    /// `grid_template_columns` (which always has this many entries when non-empty).
    pub grid_columns: usize,
    /// Resolved `grid-template-columns` track sizings (empty = none declared, so
    /// the grid falls back to equal columns). Length equals `grid_columns`.
    pub grid_template_columns: Vec<TrackSize>,
    /// Resolved `grid-template-rows` track sizings (empty = auto rows sized to
    /// content). Length equals `grid_rows` when non-empty.
    pub grid_template_rows: Vec<TrackSize>,
    /// `grid-column: span N` — the item spans `N` columns (1 = no span). Honoured
    /// alongside an explicit/auto start. Not inherited.
    pub grid_col_span: usize,
    /// `grid-row: span N` — the item spans `N` rows (1 = no span). Not inherited.
    pub grid_row_span: usize,
    /// `grid-template-areas` named areas declared on a grid container. Empty when
    /// none. Not inherited.
    pub grid_template_areas: Vec<GridAreaRect>,
    /// `grid-area: <name>` on a grid item — resolved against the parent's
    /// `grid_template_areas` at placement time. `None` for the numeric/default
    /// placement. Not inherited.
    pub grid_area_name: Option<String>,
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
    /// `min-height` — a minimum block height in points (the box still grows
    /// with content; this is only a floor).
    pub min_height: Option<f64>,
    /// `height` — a *definite* block height in points. Unlike `min-height` the
    /// box does **not** grow past it: taller content overflows (and is clipped
    /// when `overflow: hidden|clip`). Floored by `min_height` when both are set.
    pub height: Option<f64>,
    /// `aspect-ratio` as `width / height` (e.g. `16/9` → `1.777…`). When set and
    /// no definite `height` is given, the block's height is derived as
    /// `box_width / aspect_ratio`. `None` ⇒ height comes from content.
    pub aspect_ratio: Option<f64>,
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
    /// `writing-mode` — vertical blocks render as a horizontal layout rotated
    /// ±90° (the pragmatic "rotated label" model; see the layout note). Kept
    /// **non-inherited** here (a deliberate deviation from CSS, which inherits
    /// it) so exactly the declaring block rotates — descendants would
    /// otherwise double-rotate under the rotation approximation.
    pub writing_mode: WritingMode,
    /// `overflow: hidden|clip|scroll|auto` — clip descendants to this box.
    pub overflow_clip: bool,
    /// `display: flow-root` — establish a new block formatting context (so
    /// floats inside don't escape to siblings, and the box contains its floats)
    /// without any clipping. `overflow` non-visible establishes a BFC too, but
    /// via [`Self::overflow_clip`]; this flag is the clip-free trigger.
    pub establishes_bfc: bool,
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
    /// `clear` — drop this block below preceding floats before placing it. Not
    /// inherited.
    pub clear: Clear,
    /// Inline `vertical-align` (super/sub/length) for THIS run, before cascade
    /// resolves it to a point offset. Not inherited (resets to `baseline`).
    pub valign: VShift,
    /// Resolved super/subscript baseline offset in points, top-down: a negative
    /// value raises the run (super), a positive value lowers it (sub). Computed
    /// during cascade from the *parent* em so a shrunk glyph still shifts by the
    /// surrounding text's scale. Not inherited.
    pub valign_shift: f64,
    /// `border-radius` **horizontal** corner radii in points, `[top-left,
    /// top-right, bottom-right, bottom-left]` (the CSS clockwise order). All `0`
    /// means square corners — the default — and the box paints via the unchanged
    /// rectangular path. Not inherited.
    pub border_radius: [f64; 4],
    /// `border-radius` **vertical** corner radii in points, same corner order as
    /// [`Style::border_radius`]. Each entry defaults to its horizontal
    /// counterpart, so a circular `border-radius: 8pt` keeps `h == v` and paints
    /// exactly as before; the elliptical `a b c d / e f g h` form fills these
    /// with the vertical radii so each corner is an `h × v` arc. Not inherited.
    pub border_radius_v: [f64; 4],
    /// `box-shadow` first (topmost) layer. `None` = no shadow. Painted as a
    /// blurred offset rectangle behind the box — see [`BoxShadow`]. Kept as the
    /// single-layer field so the common one-shadow path is byte-identical. Not
    /// inherited.
    pub box_shadow: Option<BoxShadow>,
    /// Additional `box-shadow` layers (the 2nd…Nth, in source order). CSS paints
    /// shadow layers back-to-front — the first listed sits on top — so these are
    /// painted *behind* `box_shadow`. Empty for a single (or absent) shadow,
    /// keeping the one-layer path unchanged. Not inherited.
    pub box_shadow_extra: Vec<BoxShadow>,
    /// `text-shadow` layers (first = topmost). Inherited like `color`.
    pub text_shadows: Vec<TextShadow>,
    /// `transform` — the composed affine matrix `[a, b, c, d, e, f]` of the
    /// `translate`/`rotate`/`scale`/`skew`/`matrix` functions, in CSS (top-down)
    /// user units **before** the transform-origin shift (the paint layer applies
    /// the origin = box centre and the page Y-flip). `None` = no transform. Not
    /// inherited.
    pub transform: Option<[f64; 6]>,
    /// `white-space` (inherited). The legacy [`Style::pre`] flag is kept in
    /// lock-step (`Pre`/`PreWrap` ⇒ `pre = true`) for existing consumers.
    pub white_space: WhiteSpace,
    /// `overflow-wrap: break-word|anywhere` / `word-break: break-all|break-word`
    /// — an over-long word may be broken mid-word to fit the line. Inherited.
    pub break_word: bool,
    /// `text-overflow: ellipsis` — with `white-space: nowrap` + `overflow:
    /// hidden`, an over-long line is truncated with `…`. Not inherited.
    pub text_overflow_ellipsis: bool,
    /// `object-fit` on a replaced element (`<img>`). Not inherited.
    pub object_fit: ObjectFit,
    /// `max-height` — clamps the used box height in points (content overflows,
    /// visible by default). Not inherited.
    pub max_height: Option<f64>,
    /// The **full** `font-family` fallback list, in source order (names
    /// de-quoted, generic keywords included). [`Style::font_family`] stays the
    /// first entry for compatibility; the font resolver walks this list and uses
    /// the first family it actually has. Empty when no `font-family` was ever
    /// declared. Inherited.
    pub font_families: Vec<String>,
    /// CSS custom properties (`--name: value`) in scope for this element —
    /// the parent's map plus this element's own declarations (which win).
    /// `var(--name[, fallback])` substitution reads from here. Inherited.
    pub custom_props: std::collections::BTreeMap<String, String>,
    /// The element's **own** `opacity` declaration (the last one to cascade),
    /// before composition with the inherited (ancestor-effective) opacity.
    /// `None` when the element declares no opacity. [`Stylesheet::computed`]
    /// folds it multiplicatively into [`Style::opacity`] once per element, so
    /// `opacity: 0.5` under an `opacity: 0.5` parent is 0.25 effective — not
    /// 0.5 overwriting the parent. Not inherited (each element composes anew).
    pub own_opacity: Option<f64>,
    /// `counter-reset` pairs (name, start value). Consumed by the generated
    ///-content pass (`::before`/`::after` counters). Not inherited.
    pub counter_reset: Vec<(String, i64)>,
    /// `counter-increment` pairs (name, delta). Not inherited.
    pub counter_increment: Vec<(String, i64)>,
    /// `transform-origin` as `(x, y)` — each axis a length (points from the
    /// box's top-left) or a percentage of the border-box dimension (keywords
    /// map to 0% / 50% / 100%). `None` = the initial `50% 50%` (box centre),
    /// which is exactly the previous hard-coded pivot. Not inherited.
    pub transform_origin: Option<(Len, Len)>,
    /// `caption-side` — where a `<table>`'s `<caption>` box renders. Inherited
    /// (set once on the table, reaching the caption element itself).
    pub caption_side: CaptionSide,
}

/// CSS `caption-side` — table-caption placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaptionSide {
    /// `top` (the default) — the caption box renders above the table grid.
    #[default]
    Top,
    /// `bottom` — the caption box renders below the grid.
    Bottom,
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

/// CSS `clear` — push a block below preceding floats of the chosen side(s)
/// before it is placed. Not inherited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Clear {
    /// `none` (the default) — no clearance.
    #[default]
    None,
    /// `left` — drop below left floats.
    Left,
    /// `right` — drop below right floats.
    Right,
    /// `both` — drop below floats on either side.
    Both,
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
    SpaceEvenly,
}

impl Default for Style {
    fn default() -> Style {
        Style {
            display: Display::Inline,
            color: [0.0, 0.0, 0.0],
            color_alpha: 1.0,
            background: None,
            background_alpha: 1.0,
            font_size: 16.0,
            font_family: String::new(),
            generic_serif: false,
            generic_mono: false,
            bold: false,
            font_weight: 400,
            italic: false,
            underline: false,
            // CSS initial value of `text-align` is `start` (direction-relative):
            // `Start` resolves to `Left` in LTR — byte-identical to the old default
            // for every existing document — and to `Right` in RTL, giving an RTL
            // block its right alignment when no explicit `text-align` is set.
            align: Align::Start,
            direction: Direction::Ltr,
            text_transform: TextTransform::None,
            margin: Edges::default(),
            margin_left_auto: false,
            margin_right_auto: false,
            padding: Edges::default(),
            border_width: Edges::default(),
            border_color: [0.0, 0.0, 0.0],
            border_color_alpha: 1.0,
            border_color_edges: [[0.0, 0.0, 0.0]; 4],
            border_style_edges: [BorderStyle::Solid; 4],
            background_gradient: None,
            background_image: None,
            vertical_align: VAlign::Top,
            border_collapse: false,
            width: None,
            line_height: 1.2,
            pre: false,
            page_break_before: false,
            page_break_after: false,
            page_break_inside_avoid: false,
            flex_column: false,
            flex_reverse: false,
            justify: Justify::Start,
            flex_grow: 0.0,
            flex_shrink: 1.0,
            flex_basis: None,
            grid_columns: 0,
            grid_template_columns: Vec::new(),
            grid_template_rows: Vec::new(),
            grid_col_span: 1,
            grid_row_span: 1,
            grid_template_areas: Vec::new(),
            grid_area_name: None,
            strike: false,
            overline: false,
            hidden: false,
            opacity: 1.0,
            text_indent: 0.0,
            list_style: ListStyle::Disc,
            min_width: None,
            max_width: None,
            min_height: None,
            height: None,
            aspect_ratio: None,
            border_box: false,
            position: Position::Static,
            inset: [None; 4],
            z_index: 0,
            writing_mode: WritingMode::HorizontalTb,
            overflow_clip: false,
            establishes_bfc: false,
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
            clear: Clear::None,
            valign: VShift::Baseline,
            valign_shift: 0.0,
            border_radius: [0.0; 4],
            border_radius_v: [0.0; 4],
            box_shadow: None,
            box_shadow_extra: Vec::new(),
            text_shadows: Vec::new(),
            transform: None,
            white_space: WhiteSpace::Normal,
            break_word: false,
            text_overflow_ellipsis: false,
            object_fit: ObjectFit::Fill,
            max_height: None,
            font_families: Vec::new(),
            custom_props: std::collections::BTreeMap::new(),
            own_opacity: None,
            counter_reset: Vec::new(),
            counter_increment: Vec::new(),
            transform_origin: None,
            caption_side: CaptionSide::Top,
        }
    }
}

/// A CSS length: absolute points, a percentage of the container, or a mixed
/// `calc()` linear form of both.
#[derive(Debug, Clone, Copy)]
pub enum Len {
    Pt(f64),
    Percent(f64),
    /// `calc()` mixing absolute and percent terms: resolves to
    /// `pt + (pct/100)·base` against the same base the `Percent` arm uses.
    Calc { pt: f64, pct: f64 },
}

impl Len {
    /// Resolve against the containing dimension `base` (what a bare `%` would
    /// be a fraction of).
    #[inline]
    pub fn resolve(self, base: f64) -> f64 {
        match self {
            Len::Pt(p) => p,
            Len::Percent(pc) => base * pc / 100.0,
            Len::Calc { pt, pct } => pt + base * pct / 100.0,
        }
    }
}

/// A single `grid-template-columns` / `grid-template-rows` track sizing.
///
/// Models the track-list values documents actually use: a flexible `fr` share,
/// a fixed length (points), a percentage of the container, intrinsic `auto`
/// (sized to content), and `minmax(min, max)`. Anything unrecognised parses as
/// `Auto`, so an unknown function never drops the track (the grid still lays out
/// with the right column count).
#[derive(Debug, Clone)]
pub enum TrackSize {
    /// `<number>fr` — a share of the leftover space after fixed/percent tracks.
    Fr(f64),
    /// A fixed length resolved to points (`px`/`pt`/`em`/…).
    Pt(f64),
    /// `<percentage>` of the grid's content width/height.
    Percent(f64),
    /// `auto` — sized to the column's max content (rows: tallest cell).
    Auto,
    /// `minmax(min, max)` — clamp the resolved size between two track sizings.
    /// Boxed to keep the enum small (recursive variant).
    MinMax(Box<TrackSize>, Box<TrackSize>),
}

/// One named area from `grid-template-areas`: the bounding rectangle (0-based
/// row/column origin + spans) of all cells carrying that name. A child with
/// `grid-area: <name>` is placed into this rectangle.
#[derive(Debug, Clone)]
pub struct GridAreaRect {
    pub name: String,
    pub row: usize,
    pub col: usize,
    pub row_span: usize,
    pub col_span: usize,
}

// ─── selectors ──────────────────────────────────────────────────────────────

/// Tag of the synthetic elements materialising `::before`/`::after` generated
/// content (see [`apply_pseudo_elements`]). Chosen so no author selector ever
/// matches it, and filtered out of structural pseudo-class sibling counts so
/// injecting a `::before` never shifts a real child's `:first-child`/
/// `:nth-child` position.
pub(crate) const PSEUDO_ELEMENT_TAG: &str = "gp-pseudo";

/// An `[attr]` / `[attr=val]` attribute condition on a compound selector.
#[derive(Debug, Clone, PartialEq)]
struct AttrCond {
    name: String,
    /// `Some(v)` for `[attr=v]`/`[attr^=v]`/… ; `None` for bare `[attr]`
    /// (presence only). Values are stored as written (case-sensitive).
    value: Option<String>,
    /// How `value` is matched against the attribute (ignored when `value` is
    /// `None`).
    op: AttrOp,
}

/// CSS attribute-selector operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttrOp {
    /// `[a=v]` — exact match.
    Exact,
    /// `[a^=v]` — the value starts with `v`.
    Prefix,
    /// `[a$=v]` — the value ends with `v`.
    Suffix,
    /// `[a*=v]` — the value contains `v`.
    Substring,
    /// `[a~=v]` — `v` is one of the whitespace-separated words.
    Word,
    /// `[a|=v]` — the value equals `v` or starts with `v-`.
    DashPrefix,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Compound {
    tag: Option<String>,
    classes: Vec<String>,
    id: Option<String>,
    /// `[attr]` / `[attr=val]` conditions (all must hold).
    attrs: Vec<AttrCond>,
    /// Structural pseudo-classes (`:first-child`, `:nth-child(…)`, `:not(…)`,
    /// …) — all must hold. Dynamic pseudo-classes that can never apply on
    /// static paged output (`:hover`, `:focus`, …) and unrecognised ones are
    /// stored as [`PseudoClass::Unsupported`], which never matches — the rule
    /// is inert instead of over-matching every element.
    pseudo: Vec<PseudoClass>,
    /// `true` if the compound carries a pseudo-**element** (`::before`/`::after`).
    /// Such a compound never matches a real element (rather than wrongly styling
    /// the element itself); `::before`/`::after` rules with `content` are instead
    /// materialised as generated children by [`apply_pseudo_elements`].
    pseudo_element: bool,
    /// The pseudo-element's name (`before`, `after`, `first-line`, …), captured
    /// so the generated-content pass can find `::before`/`::after` rules. The
    /// legacy single-colon spellings (`:before`/`:after`) land here too.
    pseudo_element_name: Option<String>,
}

/// A structural pseudo-class we can evaluate against the DOM tree.
#[derive(Debug, Clone, PartialEq)]
enum PseudoClass {
    FirstChild,
    LastChild,
    OnlyChild,
    /// `nth-child(an + b)`, 1-based among element siblings.
    NthChild {
        a: i32,
        b: i32,
    },
    /// `nth-last-child(an + b)`, 1-based counting from the LAST sibling.
    NthLastChild {
        a: i32,
        b: i32,
    },
    /// `nth-of-type(an + b)`, 1-based among same-tag siblings.
    NthOfType {
        a: i32,
        b: i32,
    },
    FirstOfType,
    LastOfType,
    OnlyOfType,
    /// `:root` — the document root element (no parent).
    Root,
    /// `:empty` — no element children and no non-empty text.
    Empty,
    /// `:link` / `:any-link` — an `<a>`/`<area>` with an `href`. On static
    /// print output every link is unvisited, so both behave alike.
    AnyLink,
    /// `:not(<compound list>)` — matches when NONE of the inner compounds
    /// match. An inner compound carrying an [`PseudoClass::Unsupported`] (or a
    /// pseudo-element) makes the whole selector inert — never "not(false) ⇒
    /// true" over-matching on a constraint we can't evaluate.
    Not(Vec<Compound>),
    /// A dynamic or unrecognised pseudo-class (`:hover`, `:focus`, vendor
    /// extensions, malformed arguments). Never matches, per CSS's
    /// invalid-selector handling — the rule is inert instead of over-applied.
    Unsupported,
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
    /// Normal (non-`!important`) declarations, values already stripped of any
    /// importance suffix.
    decls: Vec<(String, String)>,
    /// `!important` declarations (suffix stripped). Applied in a second cascade
    /// pass so they win over every later/higher-specificity normal declaration
    /// and over a normal inline `style="…"`, per the CSS cascade origins.
    important: Vec<(String, String)>,
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
                // `[attr]`, `[attr=value]`, or an operator form `[attr^=v]` /
                // `$=` / `*=` / `~=` / `|=` (quotes optional).
                i += 1;
                let inner_start = i;
                while i < bytes.len() && bytes[i] != b']' {
                    i += 1;
                }
                let inner = &s[inner_start..i];
                if i < bytes.len() {
                    i += 1; // consume ']'
                }
                if let Some((lhs, val)) = inner.split_once('=') {
                    // The byte just before `=` may be an operator.
                    let (name, op) = match lhs.as_bytes().last() {
                        Some(b'~') => (&lhs[..lhs.len() - 1], AttrOp::Word),
                        Some(b'^') => (&lhs[..lhs.len() - 1], AttrOp::Prefix),
                        Some(b'$') => (&lhs[..lhs.len() - 1], AttrOp::Suffix),
                        Some(b'*') => (&lhs[..lhs.len() - 1], AttrOp::Substring),
                        Some(b'|') => (&lhs[..lhs.len() - 1], AttrOp::DashPrefix),
                        _ => (lhs, AttrOp::Exact),
                    };
                    c.attrs.push(AttrCond {
                        name: name.trim().to_ascii_lowercase(),
                        value: Some(val.trim().trim_matches(['"', '\'']).to_string()),
                        op,
                    });
                } else if !inner.trim().is_empty() {
                    c.attrs.push(AttrCond {
                        name: inner.trim().to_ascii_lowercase(),
                        value: None,
                        op: AttrOp::Exact,
                    });
                }
            }
            b':' => {
                i += 1;
                // `::name` is a pseudo-element — flag it and skip the name.
                let double_colon = i < bytes.len() && bytes[i] == b':';
                if double_colon {
                    i += 1;
                    c.pseudo_element = true;
                }
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-') {
                    i += 1;
                }
                let name = s[start..i].to_ascii_lowercase();
                // Optional functional `(arg)`, e.g. `nth-child(2n + 1)`. Parens
                // are balanced so `:not(:nth-child(2))` captures its full arg.
                let mut arg = "";
                if i < bytes.len() && bytes[i] == b'(' {
                    i += 1;
                    let astart = i;
                    let mut depth = 1usize;
                    while i < bytes.len() {
                        match bytes[i] {
                            b'(' => depth += 1,
                            b')' => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                        i += 1;
                    }
                    arg = &s[astart..i.min(bytes.len())];
                    if i < bytes.len() {
                        i += 1; // consume ')'
                    }
                }
                // The legacy single-colon spellings of the CSS 2.1 pseudo-
                // elements (`:before`, `:after`, …) are pseudo-elements too.
                if !double_colon && is_pseudo_element_name(&name) {
                    c.pseudo_element = true;
                }
                if c.pseudo_element && c.pseudo_element_name.is_none() {
                    c.pseudo_element_name = Some(name.clone());
                }
                // Pseudo-elements never match a real element; pseudo-classes
                // are recorded — including `Unsupported` for the dynamic /
                // unknown ones, which makes the rule inert instead of letting
                // it over-match every element.
                if !c.pseudo_element {
                    c.pseudo.push(parse_pseudo_class(&name, arg));
                }
            }
            _ => i += 1,
        }
    }
    c
}

/// The CSS 2.1 pseudo-element names that are also valid with a single colon
/// (`:before` ≡ `::before`), plus the modern ones documents write.
fn is_pseudo_element_name(name: &str) -> bool {
    matches!(
        name,
        "before"
            | "after"
            | "first-line"
            | "first-letter"
            | "marker"
            | "placeholder"
            | "selection"
            | "backdrop"
    )
}

/// Map a pseudo-class name (+ functional argument) to a [`PseudoClass`].
/// Structural classes are evaluated against the tree; everything dynamic or
/// unrecognised — `:hover`, vendor extensions, a malformed `nth-*` argument —
/// becomes [`PseudoClass::Unsupported`], which never matches (the selector is
/// inert, per CSS invalid-selector handling, instead of over-matching).
fn parse_pseudo_class(name: &str, arg: &str) -> PseudoClass {
    let nth = |arg: &str, mk: fn(i32, i32) -> PseudoClass| {
        parse_nth(arg)
            .map(|(a, b)| mk(a, b))
            .unwrap_or(PseudoClass::Unsupported)
    };
    match name {
        "first-child" => PseudoClass::FirstChild,
        "last-child" => PseudoClass::LastChild,
        "only-child" => PseudoClass::OnlyChild,
        "nth-child" => nth(arg, |a, b| PseudoClass::NthChild { a, b }),
        "nth-last-child" => nth(arg, |a, b| PseudoClass::NthLastChild { a, b }),
        "nth-of-type" => nth(arg, |a, b| PseudoClass::NthOfType { a, b }),
        "nth-last-of-type" => PseudoClass::Unsupported,
        "first-of-type" => PseudoClass::FirstOfType,
        "last-of-type" => PseudoClass::LastOfType,
        "only-of-type" => PseudoClass::OnlyOfType,
        "root" => PseudoClass::Root,
        "empty" => PseudoClass::Empty,
        "link" | "any-link" => PseudoClass::AnyLink,
        // `:not(<list>)`: parse each comma component as a compound. An inner
        // pseudo-element or an empty list can't be evaluated → Unsupported.
        "not" => {
            let inner: Vec<Compound> = arg
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(parse_compound)
                .collect();
            if inner.is_empty() || inner.iter().any(|c| c.pseudo_element) {
                PseudoClass::Unsupported
            } else {
                PseudoClass::Not(inner)
            }
        }
        // `:is()`/`:where()` with a single compound could be supported later;
        // for now they (and everything else) are unsupported → inert.
        _ => PseudoClass::Unsupported,
    }
}

/// Parse an `nth-child` argument into `(a, b)` for the `an + b` form. Accepts
/// `odd`, `even`, a bare integer (`3` → `0n + 3`), and the general `2n`, `n`,
/// `-n + 3`, `2n + 1` shapes. Returns `None` if it can't be parsed.
fn parse_nth(arg: &str) -> Option<(i32, i32)> {
    let s = arg.trim().to_ascii_lowercase();
    match s.as_str() {
        "odd" => return Some((2, 1)),
        "even" => return Some((2, 0)),
        _ => {}
    }
    if let Some(npos) = s.find('n') {
        let a = match s[..npos].trim() {
            "" | "+" => 1,
            "-" => -1,
            x => x.parse::<i32>().ok()?,
        };
        let b_str: String = s[npos + 1..]
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let b = if b_str.is_empty() {
            0
        } else {
            b_str.parse::<i32>().ok()?
        };
        Some((a, b))
    } else {
        s.parse::<i32>().ok().map(|b| (0, b))
    }
}

/// Does `el` satisfy a compound's pseudo constraints? A pseudo-element compound
/// never matches a real element; pseudo-classes are evaluated against the tree
/// (see [`pseudo_classes_ok`]).
fn pseudo_ok(compound: &Compound, el: &Element, parent: Option<&Element>) -> bool {
    if compound.pseudo_element {
        return false;
    }
    pseudo_classes_ok(compound, el, parent)
}

/// [`pseudo_ok`] without the pseudo-element bail — shared with the generated-
/// content pass, which matches an `x::before` selector against the real `x`.
/// Structural pseudo-classes are evaluated against `el`'s position among its
/// parent's element children; positional tests with no parent (a top-level
/// element) keep passing, so a rule is never dropped on the root.
fn pseudo_classes_ok(compound: &Compound, el: &Element, parent: Option<&Element>) -> bool {
    compound
        .pseudo
        .iter()
        .all(|p| pseudo_class_matches(p, el, parent))
}

/// Evaluate one pseudo-class against `el` (with its `parent`, when known).
fn pseudo_class_matches(p: &PseudoClass, el: &Element, parent: Option<&Element>) -> bool {
    // `an + b` membership for a 1-based index.
    let nth = |a: i32, b: i32, idx: i32| {
        if a == 0 {
            idx == b
        } else {
            let diff = idx - b;
            diff % a == 0 && diff / a >= 0
        }
    };
    match p {
        // Dynamic / unrecognised — never matches: the rule is inert.
        PseudoClass::Unsupported => false,
        PseudoClass::Root => parent.is_none(),
        PseudoClass::Empty => el.children.iter().all(|c| match c {
            Node::Element(_) => false,
            Node::Text(t) => t.is_empty(),
        }),
        PseudoClass::AnyLink => {
            matches!(el.tag.as_str(), "a" | "area") && el.attr("href").is_some()
        }
        PseudoClass::Not(list) => {
            // A constraint we can't evaluate inside `:not(…)` must NOT flip
            // into a match ("not(unknowable) ⇒ true" would over-apply the
            // rule); the whole selector is inert instead.
            if list
                .iter()
                .any(|c| c.pseudo.contains(&PseudoClass::Unsupported))
            {
                return false;
            }
            !list
                .iter()
                .any(|c| matches(c, el) && pseudo_classes_ok(c, el, parent))
        }
        // Positional tests need the sibling list.
        _ => {
            let Some(parent) = parent else { return true };
            let siblings: Vec<&Element> = parent
                .children
                .iter()
                .filter_map(|c| match c {
                    // Generated `::before`/`::after` children are invisible to
                    // structural pseudo-classes (they are not real siblings).
                    Node::Element(e) if e.tag != PSEUDO_ELEMENT_TAG => Some(e),
                    _ => None,
                })
                .collect();
            let count = siblings.len() as i32;
            let Some(pos) = siblings.iter().position(|e| std::ptr::eq(*e, el)) else {
                return true; // `el` not found under `parent` — don't drop the rule
            };
            let idx = (pos + 1) as i32; // 1-based
            match p {
                PseudoClass::FirstChild => idx == 1,
                PseudoClass::LastChild => idx == count,
                PseudoClass::OnlyChild => count == 1,
                PseudoClass::NthChild { a, b } => nth(*a, *b, idx),
                PseudoClass::NthLastChild { a, b } => nth(*a, *b, count - idx + 1),
                PseudoClass::NthOfType { .. }
                | PseudoClass::FirstOfType
                | PseudoClass::LastOfType
                | PseudoClass::OnlyOfType => {
                    // Position among same-tag siblings only.
                    let typed: Vec<&&Element> =
                        siblings.iter().filter(|e| e.tag == el.tag).collect();
                    let tcount = typed.len() as i32;
                    let Some(tpos) = typed.iter().position(|e| std::ptr::eq(**e, el)) else {
                        return true;
                    };
                    let tidx = (tpos + 1) as i32;
                    match p {
                        PseudoClass::NthOfType { a, b } => nth(*a, *b, tidx),
                        PseudoClass::FirstOfType => tidx == 1,
                        PseudoClass::LastOfType => tidx == tcount,
                        PseudoClass::OnlyOfType => tcount == 1,
                        _ => unreachable!("outer match narrowed to of-type variants"),
                    }
                }
                // Non-positional variants are fully handled above.
                _ => unreachable!("non-positional pseudo-classes handled above"),
            }
        }
    }
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
    // Specificity: ids*100 + (classes + attrs + pseudo-classes)*10 + (tags +
    // pseudo-elements). Combinators add none.
    let mut spec = 0u32;
    for (_, p) in &src {
        if p.id.is_some() {
            spec += 100;
        }
        spec += 10 * (p.classes.len() + p.attrs.len() + p.pseudo.len()) as u32;
        if p.tag.is_some() {
            spec += 1;
        }
        if p.pseudo_element {
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
    // Bracket/paren depth: combinators and whitespace only separate compounds at
    // depth 0, so `[class~=b]` / `:nth-child(2n + 1)` keep their `~`/spaces.
    let mut depth = 0i32;
    for ch in s.chars() {
        match ch {
            '[' | '(' => {
                depth += 1;
                cur.push(ch);
                prev_ws = false;
            }
            ']' | ')' => {
                depth = (depth - 1).max(0);
                cur.push(ch);
                prev_ws = false;
            }
            '>' | '+' | '~' if depth == 0 => {
                flush(&mut cur, &mut toks);
                // Replace a trailing descendant (from surrounding whitespace)
                // with the explicit combinator.
                if matches!(
                    toks.last(),
                    Some(SelTok::Combinator(Combinator::Descendant))
                ) {
                    toks.pop();
                }
                toks.push(SelTok::Combinator(match ch {
                    '>' => Combinator::Child,
                    '+' => Combinator::AdjacentSibling,
                    _ => Combinator::GeneralSibling,
                }));
                prev_ws = false;
            }
            c if c.is_whitespace() && depth == 0 => {
                flush(&mut cur, &mut toks);
                prev_ws = true;
            }
            c => {
                // A space between two compounds (no explicit combinator) is a
                // descendant relationship.
                if prev_ws && cur.is_empty() && matches!(toks.last(), Some(SelTok::Compound(_))) {
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

/// Parse a stylesheet body into rules. `viewport_px` is the page width in CSS px
/// (when known), used to evaluate `@media` feature queries.
fn parse_rules(css: &str, order_base: usize, viewport_px: Option<f64>) -> Vec<Rule> {
    let css = strip_comments(css);
    let mut rules = Vec::new();
    parse_rules_into(&css, order_base, &mut rules, viewport_px);
    rules
}

/// Recursive worker behind [`parse_rules`]. Appends parsed rules to `out` and
/// returns the next source order to use. Splitting this out lets a conditional
/// group rule (`@media print { … }`) recurse on its body while preserving the
/// global source order across nested and top-level rules.
fn parse_rules_into(
    css: &str,
    order_base: usize,
    out: &mut Vec<Rule>,
    viewport_px: Option<f64>,
) -> usize {
    let mut rest = css;
    let mut order = order_base;
    while let Some(brace) = rest.find('{') {
        let preamble = rest[..brace].trim();
        let after = &rest[brace + 1..];
        // Find the brace that matches THIS `{` by depth counting. A normal rule
        // body holds no braces, so depth-matching reduces to the old "first `}`";
        // a conditional group rule (`@media { .x { … } }`) needs the balanced
        // close so its nested rules aren't mistaken for a truncated declaration.
        let Some((body, consumed)) = take_balanced_block(after) else {
            break;
        };
        rest = &after[consumed..];

        if let Some(query) = preamble.strip_prefix('@') {
            // Conditional group rules (`@media …`) nest rules. Apply the inner
            // rules when the query matches the print target; otherwise drop them.
            // Other at-rules (@font-face, @keyframes, @supports, @page, …) carry
            // no plain selector rules we interpret, so they're skipped wholesale.
            let name_end = query
                .find(|c: char| c.is_whitespace() || c == '{')
                .unwrap_or(query.len());
            if query[..name_end].eq_ignore_ascii_case("media") {
                if media_query_applies(query[name_end..].trim(), viewport_px) {
                    order = parse_rules_into(body, order, out, viewport_px);
                } else {
                    order += 1;
                }
            } else {
                order += 1;
            }
            continue;
        }
        let selectors: Vec<Selector> = preamble.split(',').filter_map(parse_selector).collect();
        if selectors.is_empty() {
            continue;
        }
        let (decls, important) = split_important(parse_decls_full(body));
        out.push(Rule {
            selectors,
            decls,
            important,
            order,
        });
        order += 1;
    }
    order
}

/// Given the text immediately after an opening `{`, return the block body (text
/// up to the matching `}`) and the number of bytes consumed including that `}`.
/// Returns `None` if the brace is never closed (truncated stylesheet).
fn take_balanced_block(after: &str) -> Option<(&str, usize)> {
    let bytes = after.as_bytes();
    let mut depth = 1usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((&after[..i], i + 1));
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Decide whether an `@media` query applies to the print target. We render to
/// paged PDF, so `print` and the universal/empty queries apply; `screen`-only
/// queries do not. Comma-separated queries apply if ANY component applies
/// (CSS media query lists are a logical OR). Feature queries we can't evaluate
/// (`(max-width: …)`) are treated as applying for the matched media type so a
/// stylesheet's intended print rules aren't silently dropped.
fn media_query_applies(query: &str, viewport_px: Option<f64>) -> bool {
    let q = query.trim();
    if q.is_empty() {
        return true; // bare `@media { … }` — always-on group.
    }
    // A comma-separated query list is a logical OR.
    q.split(',')
        .any(|component| media_component_applies(component.trim(), viewport_px))
}

/// One comma component: an optional leading media type (with an `only`/`not`
/// modifier) ANDed with zero or more `(feature)` queries. The media type decides
/// print vs screen; the feature queries (`min-width`/`max-width`/`width`) are
/// evaluated against the page viewport width when it is known.
fn media_component_applies(c: &str, viewport_px: Option<f64>) -> bool {
    let lower = c.to_ascii_lowercase();
    let (negate, body) = if let Some(rest) = lower.strip_prefix("not ") {
        (true, rest)
    } else if let Some(rest) = lower.strip_prefix("only ") {
        (false, rest)
    } else {
        (false, lower.as_str())
    };
    let mut type_ok = true;
    let mut features_ok = true;
    for (i, part) in body.split(" and ").map(str::trim).enumerate() {
        if part.starts_with('(') {
            features_ok &= media_feature_applies(part, viewport_px);
        } else if i == 0 && !part.is_empty() {
            // Leading media type — `print`/`all` target paged PDF; others don't.
            type_ok = matches!(part, "print" | "all");
        }
    }
    let matched = type_ok && features_ok;
    if negate {
        !matched
    } else {
        matched
    }
}

/// Evaluate a single `(min-width: …)` / `(max-width: …)` / `(width: …)` feature
/// against the viewport width in CSS px. An unknown feature — or no viewport —
/// returns `true`, so the author's intended rules aren't silently dropped.
fn media_feature_applies(feat: &str, viewport_px: Option<f64>) -> bool {
    let Some(vp) = viewport_px else { return true };
    let inner = feat.trim_start_matches('(').trim_end_matches(')');
    let Some((name, val)) = inner.split_once(':') else {
        return true;
    };
    // `parse_len_px` yields points; an `em` here is the initial 16px font (12pt).
    // Convert points → CSS px (÷ 0.75) to compare with the px viewport.
    let Some(px) = parse_len_px(val.trim(), 12.0).map(|pt| pt / 0.75) else {
        return true;
    };
    match name.trim() {
        "min-width" => vp >= px,
        "max-width" => vp <= px,
        "width" => (vp - px).abs() < 0.5,
        _ => true, // height / orientation / etc. — only the page width is modelled
    }
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

/// A parsed `@font-face` rule carrying the data needed to register an inline web
/// font as a renderable face: the family name, the ordered list of `src` URLs
/// (each `url(...)` content — a `data:` URI is decodable, an external URL is
/// skipped by the renderer), and the declared `font-weight`/`font-style`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FontFaceRule {
    /// `font-family`, lower-cased and de-quoted (matches [`Style::font_family`]).
    pub family: String,
    /// Every `url(...)` listed in `src`, in source order (alternatives to try).
    pub srcs: Vec<String>,
    /// `font-weight` mapped to a numeric CSS weight (1–1000); `400` when absent.
    pub weight: u16,
    /// `true` when `font-style` is `italic` or `oblique`; `false` otherwise.
    pub italic: bool,
}

/// Scan `@font-face { … }` blocks and return one [`FontFaceRule`] per block,
/// capturing the family **and** the `src` URLs + `font-weight`/`font-style` so
/// an inline web font (`src: url(data:font/woff2;base64,…)`) can be decoded and
/// registered as a render face. Mirrors [`collect_font_faces`]'s walk; rules
/// without any `src` URL are still returned (with an empty `srcs`) so callers see
/// the declared family, but they contribute no face.
pub(crate) fn collect_font_face_rules(css: &str) -> Vec<FontFaceRule> {
    let css = strip_comments(css);
    let mut rules = Vec::new();
    let mut rest = css.as_str();
    while let Some(at) = rest.find("@font-face") {
        rest = &rest[at + "@font-face".len()..];
        let Some(open) = rest.find('{') else { break };
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else { break };
        let body = &after[..close];

        let mut family = String::new();
        let mut srcs: Vec<String> = Vec::new();
        let mut weight: u16 = 400;
        let mut italic = false;
        for (k, val) in parse_decls(body) {
            match k.as_str() {
                "font-family" => {
                    family = val.trim().trim_matches(['"', '\'']).to_ascii_lowercase();
                }
                "src" => srcs.extend(collect_all_css_urls(&val)),
                "font-weight" => weight = parse_font_face_weight(&val),
                "font-style" => {
                    let s = val.trim().to_ascii_lowercase();
                    italic = s.starts_with("italic") || s.starts_with("oblique");
                }
                _ => {}
            }
        }
        if !family.is_empty() {
            rules.push(FontFaceRule {
                family,
                srcs,
                weight,
                italic,
            });
        }
        rest = &after[close + 1..];
    }
    rules
}

/// Map a `@font-face` `font-weight` value to a numeric CSS weight (1–1000):
/// `normal`→400, `bold`→700, a number as-is (clamped to 1..=1000). A
/// range/list (`400 700`) takes the first token. Falls back to 400.
fn parse_font_face_weight(v: &str) -> u16 {
    let tok = v.split_whitespace().next().unwrap_or("");
    match tok.to_ascii_lowercase().as_str() {
        "" | "normal" | "lighter" => 400,
        "bold" | "bolder" => 700,
        other => other
            .parse::<u32>()
            .map(|n| n.clamp(1, 1000) as u16)
            .unwrap_or(400),
    }
}

/// Scan the author CSS for an `@page { … }` rule and reduce it to a
/// [`PageRule`]. Page-selector variants (`@page :first`, `@page landscape`)
/// are accepted; the **first** block wins per component (documents in the wild
/// declare one). Returns `None` when the stylesheet has no `@page` at all, or
/// none of its declarations resolve.
pub(crate) fn collect_page_rule(css: &str) -> Option<PageRule> {
    let css = strip_comments(css);
    let mut rule = PageRule::default();
    let mut found = false;
    let mut rest = css.as_str();
    while let Some(at) = rest.find("@page") {
        // Require a word boundary so `@page-break-x` (hypothetical) is skipped.
        let after_kw = &rest[at + "@page".len()..];
        if after_kw
            .chars()
            .next()
            .is_some_and(|c| !c.is_whitespace() && c != '{' && c != ':')
        {
            rest = after_kw;
            continue;
        }
        let Some(open) = after_kw.find('{') else { break };
        let Some((body, consumed)) = take_balanced_block(&after_kw[open + 1..]) else {
            break;
        };
        for (k, v) in parse_decls(body) {
            match k.as_str() {
                "size" if rule.size.is_none() => rule.size = parse_page_size_value(&v),
                "margin" => {
                    let e = parse_edges(&v, ROOT_EM_PT);
                    for (slot, side) in rule
                        .margins
                        .iter_mut()
                        .zip([e.top, e.right, e.bottom, e.left])
                    {
                        slot.get_or_insert(side);
                    }
                }
                "margin-top" => {
                    rule.margins[0] = parse_len_px(&v, ROOT_EM_PT).or(rule.margins[0]);
                }
                "margin-right" => {
                    rule.margins[1] = parse_len_px(&v, ROOT_EM_PT).or(rule.margins[1]);
                }
                "margin-bottom" => {
                    rule.margins[2] = parse_len_px(&v, ROOT_EM_PT).or(rule.margins[2]);
                }
                "margin-left" => {
                    rule.margins[3] = parse_len_px(&v, ROOT_EM_PT).or(rule.margins[3]);
                }
                _ => {}
            }
        }
        found = true;
        rest = &after_kw[open + 1 + consumed..];
    }
    (found && (rule.size.is_some() || rule.margins.iter().any(Option::is_some))).then_some(rule)
}

/// Resolve an `@page` `size:` value to `(width, height)` points. Forms:
/// a named paper size with an optional `landscape`/`portrait` keyword
/// (`A4 landscape`, `letter`), two lengths (`210mm 297mm`), or one length (a
/// square page, per spec). `auto` (or anything unresolvable) yields `None`.
fn parse_page_size_value(v: &str) -> Option<(f64, f64)> {
    let toks: Vec<&str> = v.split_whitespace().collect();
    if toks.is_empty() || v.trim().eq_ignore_ascii_case("auto") {
        return None;
    }
    // `<length> <length>` / a single `<length>` (square).
    let lens: Vec<f64> = toks
        .iter()
        .filter_map(|t| parse_len_px(t, ROOT_EM_PT))
        .collect();
    match lens.as_slice() {
        [w, h] if *w > 0.0 && *h > 0.0 => return Some((*w, *h)),
        [s] if *s > 0.0 && toks.len() == 1 => return Some((*s, *s)),
        _ => {}
    }
    // Named size, with the orientation keyword in either position.
    let mut name = None;
    let mut landscape = false;
    for t in &toks {
        match t.to_ascii_lowercase().as_str() {
            "landscape" => landscape = true,
            "portrait" => {}
            other => name = name.or_else(|| super::page::page_size(other)),
            // (first name token wins; `page_size` rejects unknown names)
        }
    }
    name.map(|(w, h)| if landscape { (h, w) } else { (w, h) })
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

/// Parse `prop: value; …` into pairs. Splits on `;` at paren depth 0, so a
/// `url(data:image/png;base64,…)` value (whose `;` is *inside* the function) is
/// not torn in half. A `!important` suffix is **stripped from the value** (it is
/// a cascade flag, not part of the value — left in place it made the whole
/// declaration unparseable, e.g. `parse_color("red !important")` failed and the
/// property was silently dropped); use [`parse_decls_full`] when the importance
/// flag itself is needed.
pub fn parse_decls(body: &str) -> Vec<(String, String)> {
    parse_decls_full(body)
        .into_iter()
        .map(|(k, v, _)| (k, v))
        .collect()
}

/// [`parse_decls`] plus the `!important` flag per declaration (the suffix is
/// stripped from the returned value either way).
fn parse_decls_full(body: &str) -> Vec<(String, String, bool)> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    let push = |seg: &str, out: &mut Vec<(String, String, bool)>| {
        if let Some((k, v)) = seg.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            let (v, important) = strip_important(v.trim());
            let v = v.trim().to_string();
            if !k.is_empty() && !v.is_empty() {
                out.push((k, v, important));
            }
        }
    };
    for (i, ch) in body.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = (depth - 1).max(0),
            ';' if depth == 0 => {
                push(&body[start..i], &mut out);
                start = i + 1;
            }
            _ => {}
        }
    }
    push(&body[start..], &mut out);
    out
}

/// Split a trailing `!important` off a declaration value: returns the value
/// without the flag plus whether it was present. Case-insensitive, tolerant of
/// whitespace around the `!` (`red !important`, `red!important`,
/// `red ! IMPORTANT` all match). ASCII-only lowering keeps byte offsets valid.
fn strip_important(v: &str) -> (&str, bool) {
    let lower = v.to_ascii_lowercase();
    let Some(before_word) = lower.strip_suffix("important") else {
        return (v, false);
    };
    let trimmed = before_word.trim_end();
    let Some(before_bang) = trimmed.strip_suffix('!') else {
        return (v, false);
    };
    (v[..before_bang.len()].trim_end(), true)
}

/// A property→value declaration list (importance already resolved/stripped).
type DeclList = Vec<(String, String)>;

/// Partition parsed declarations into (normal, important) pairs, preserving
/// order within each class.
fn split_important(decls: Vec<(String, String, bool)>) -> (DeclList, DeclList) {
    let mut normal = Vec::new();
    let mut important = Vec::new();
    for (k, v, imp) in decls {
        if imp {
            important.push((k, v));
        } else {
            normal.push((k, v));
        }
    }
    (normal, important)
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
                    if !attr_value_matches(actual, want, cond.op) {
                        return false;
                    }
                }
            }
        }
    }
    true
}

/// Match an attribute's `actual` value against a selector's `want` per its
/// operator. The substring/prefix/suffix forms never match an empty `want`
/// (per CSS Selectors §6.3).
fn attr_value_matches(actual: &str, want: &str, op: AttrOp) -> bool {
    match op {
        AttrOp::Exact => actual == want,
        AttrOp::Prefix => !want.is_empty() && actual.starts_with(want),
        AttrOp::Suffix => !want.is_empty() && actual.ends_with(want),
        AttrOp::Substring => !want.is_empty() && actual.contains(want),
        AttrOp::Word => actual.split_whitespace().any(|w| w == want),
        AttrOp::DashPrefix => actual == want || actual.starts_with(&format!("{want}-")),
    }
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
            // Generated `::before` children are not real siblings.
            if e.tag != PSEUDO_ELEMENT_TAG {
                out.push(e);
            }
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
    let parent = ancestors.last().copied();
    // parts[0] always matches the element itself (incl. its pseudo-classes).
    if !matches(&parts[0].1, el) || !pseudo_ok(&parts[0].1, el, parent) {
        return false;
    }
    selector_ancestry_matches(selector, el, ancestors)
}

/// Does an `x::before` / `x::after` selector target `el`? Like
/// [`selector_matches`] but the target compound's pseudo-**element** flag is
/// ignored (its pseudo-classes still apply): the generated-content pass asks
/// "does this rule generate a pseudo child ON `el`", which is precisely a
/// match of everything except the pseudo-element itself.
fn selector_matches_pseudo_target(selector: &Selector, el: &Element, ancestors: &[&Element]) -> bool {
    let parts = &selector.parts;
    let parent = ancestors.last().copied();
    if !matches(&parts[0].1, el) || !pseudo_classes_ok(&parts[0].1, el, parent) {
        return false;
    }
    selector_ancestry_matches(selector, el, ancestors)
}

/// The combinator walk over `selector.parts[1..]` (descendant / child /
/// sibling hops), shared by [`selector_matches`] and
/// [`selector_matches_pseudo_target`] — the target compound is already checked.
fn selector_ancestry_matches(selector: &Selector, el: &Element, ancestors: &[&Element]) -> bool {
    let parts = &selector.parts;
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
                let p_parent = if ai > 0 {
                    Some(ancestors[ai - 1])
                } else {
                    None
                };
                if !matches(compound, p) || !pseudo_ok(compound, p, p_parent) {
                    return false;
                }
                cur = p;
            }
            Combinator::Descendant => {
                let mut found = false;
                while ai > 0 {
                    ai -= 1;
                    let p_parent = if ai > 0 {
                        Some(ancestors[ai - 1])
                    } else {
                        None
                    };
                    if matches(compound, ancestors[ai])
                        && pseudo_ok(compound, ancestors[ai], p_parent)
                    {
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
                    Some(p) if matches(compound, p) && pseudo_ok(compound, p, Some(par)) => cur = p,
                    _ => return false,
                }
            }
            Combinator::GeneralSibling => {
                let Some(par) = sibling_parent(cur, el, parent, ancestors, ai) else {
                    return false;
                };
                let prev = preceding_siblings(cur, par);
                match prev
                    .iter()
                    .rev()
                    .find(|p| matches(compound, p) && pseudo_ok(compound, p, Some(par)))
                {
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

/// The document's `@page` rule, reduced to what the paged renderer consumes:
/// an explicit page size and per-side margins. `None`/absent components leave
/// the caller's [`RenderOptions`](super::page::RenderOptions) untouched.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PageRule {
    /// `size: A4 [landscape] | letter | <w> <h> | <len>` → `(width, height)`
    /// in points.
    pub size: Option<(f64, f64)>,
    /// `margin`/`margin-{top,right,bottom,left}` in points, `[top, right,
    /// bottom, left]` order; `None` per side keeps the caller's margin.
    pub margins: [Option<f64>; 4],
}

/// The full stylesheet context: user-agent defaults + author rules.
#[derive(Debug)]
pub struct Stylesheet {
    rules: Vec<Rule>,
    /// Family names declared by `@font-face` rules (lower-cased), so callers
    /// can tell which families the document defines locally.
    font_faces: Vec<String>,
    /// Full `@font-face` rules (family + `src` URLs + weight/style), so the
    /// renderer can decode inline `data:` font sources and register them.
    font_face_rules: Vec<FontFaceRule>,
    /// The author's `@page { size; margin }` rule, when present.
    page_rule: Option<PageRule>,
    /// Whether any rule targets `::before`/`::after` — the fast bail that keeps
    /// [`apply_pseudo_elements`] a no-op clone for the common document.
    has_generated_content: bool,
}

impl Stylesheet {
    /// Build from the author CSS collected from `<style>` blocks. `@media`
    /// feature queries (`min-width`/`max-width`) can't be evaluated without a
    /// page width, so they all apply; use [`Stylesheet::with_viewport`] when the
    /// render width is known.
    pub fn new(author_css: &str) -> Stylesheet {
        Stylesheet::with_viewport(author_css, None)
    }

    /// Build with the page viewport width in CSS px, so `@media (min-width: …)` /
    /// `(max-width: …)` queries are evaluated against it.
    pub fn with_viewport(author_css: &str, viewport_px: Option<f64>) -> Stylesheet {
        let mut rules = parse_rules(UA_CSS, 0, viewport_px);
        rules.extend(parse_rules(author_css, 100_000, viewport_px));
        let has_generated_content = rules.iter().any(|r| {
            r.selectors.iter().any(|s| {
                matches!(
                    s.parts[0].1.pseudo_element_name.as_deref(),
                    Some("before") | Some("after")
                )
            })
        });
        Stylesheet {
            rules,
            font_faces: collect_font_faces(author_css),
            font_face_rules: collect_font_face_rules(author_css),
            page_rule: collect_page_rule(author_css),
            has_generated_content,
        }
    }

    /// Family names registered via `@font-face` (lower-cased).
    pub fn font_faces(&self) -> &[String] {
        &self.font_faces
    }

    /// The author's `@page { size; margin }` rule, when the document has one.
    pub fn page_rule(&self) -> Option<&PageRule> {
        self.page_rule.as_ref()
    }

    /// The cascade-ordered declarations of every rule targeting
    /// `el::{which}` (`which` = `"before"` / `"after"`): rule normals in
    /// (specificity, order), then `!important`s in the same order. Empty when
    /// no pseudo-element rule matches `el`.
    fn pseudo_decls(&self, el: &Element, ancestors: &[&Element], which: &str) -> DeclList {
        let mut hits: Vec<(&Selector, &Rule)> = Vec::new();
        for rule in &self.rules {
            for sel in &rule.selectors {
                if sel.parts[0].1.pseudo_element_name.as_deref() == Some(which)
                    && selector_matches_pseudo_target(sel, el, ancestors)
                {
                    hits.push((sel, rule));
                    break;
                }
            }
        }
        if hits.is_empty() {
            return Vec::new();
        }
        hits.sort_by_key(|(s, r)| (s.specificity, r.order));
        let mut out: DeclList = Vec::new();
        for (_, rule) in &hits {
            out.extend(rule.decls.iter().cloned());
        }
        for (_, rule) in &hits {
            out.extend(rule.important.iter().cloned());
        }
        out
    }

    /// Full `@font-face` rules (family + `src` URLs + weight/style). The renderer
    /// decodes inline `data:` font sources from these and registers them as
    /// render faces; external `url(...)` sources are left to the host.
    pub(crate) fn font_face_rules(&self) -> &[FontFaceRule] {
        &self.font_face_rules
    }

    /// Compute the style of `el` given its inherited (parent) style and its
    /// ancestor chain (root-first, excluding `el`).
    pub fn computed(&self, el: &Element, parent: &Style, ancestors: &[&Element]) -> Style {
        let mut style = inherit(parent);

        // The `dir` HTML attribute is a presentational hint: apply it before the
        // author stylesheet and inline `style` so an explicit `direction:` rule (or
        // inline declaration) can still override it. `dir="auto"` would require the
        // Unicode bidi algorithm to detect the base direction, which is out of
        // scope, so it is ignored (the inherited direction stands).
        if let Some(dir) = el.attr("dir") {
            match dir.trim().to_ascii_lowercase().as_str() {
                "rtl" => style.direction = Direction::Rtl,
                "ltr" => style.direction = Direction::Ltr,
                _ => {}
            }
        }

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
        let inline = el.attr("style").map(parse_decls_full).map(split_important);
        // Custom properties (`--name`) are collected FIRST, over the whole
        // cascade in priority order, so a `var(--x)` reference resolves no
        // matter where the declaration sits relative to the `--x` definition
        // (per spec, custom properties are order-independent within an
        // element's cascade).
        {
            let mut collect = |decls: &[(String, String)]| {
                for (k, v) in decls {
                    if k.starts_with("--") {
                        style.custom_props.insert(k.clone(), v.clone());
                    }
                }
            };
            for (_, rule) in &hits {
                collect(&rule.decls);
            }
            if let Some((normal, _)) = &inline {
                collect(normal);
            }
            for (_, rule) in &hits {
                collect(&rule.important);
            }
            if let Some((_, important)) = &inline {
                collect(important);
            }
        }
        // Cascade in ascending priority: rule normals (by specificity/order),
        // inline normals, rule `!important`s (same specificity/order among
        // themselves), inline `!important`s — the CSS origin order for
        // author-level styles.
        for (_, rule) in &hits {
            apply_decls(&mut style, &rule.decls);
        }
        if let Some((normal, _)) = &inline {
            apply_decls(&mut style, normal);
        }
        for (_, rule) in &hits {
            apply_decls(&mut style, &rule.important);
        }
        if let Some((_, important)) = &inline {
            apply_decls(&mut style, important);
        }
        // `opacity` composes down the tree: the element's own declaration (the
        // last to cascade, if any) multiplies the inherited effective opacity —
        // a 0.5 child of a 0.5 parent paints at 0.25, never "back up" to 0.5.
        if let Some(own) = style.own_opacity {
            style.opacity = (parent.opacity * own).clamp(0.0, 1.0);
        }
        // A closed `<select>` control displays exactly ONE option — the
        // `selected` one(s), else the first — never the concatenation of every
        // `<option>`'s text. This is control semantics, not a style choice, so
        // it is decided here (where the ancestor chain is available) after the
        // cascade.
        if el.tag == "option" && !option_is_rendered(el, ancestors) {
            style.display = Display::None;
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
            // Line-relative alignments resolve at line assembly (the line
            // height isn't known here); no static shift.
            VShift::Top | VShift::Middle | VShift::Bottom => 0.0,
        };
        style
    }
}

// ─── generated content (`::before` / `::after`) ────────────────────────────

/// CSS counters in scope during the generated-content walk. Each counter name
/// holds a **stack** of `(scope_depth, value)` entries: `counter-reset` at tree
/// depth `d` pushes a new innermost counter that lives until its parent (depth
/// `d - 1`) is exited — i.e. it covers the resetting element, its descendants
/// and its following siblings, per CSS 2.1 §12.4. Nested resets of the same
/// name stack, which is what makes `counters(item, ".")` produce `1.2.1`-style
/// hierarchical numbering on nested lists.
#[derive(Debug, Default)]
struct CounterSet {
    stacks: std::collections::BTreeMap<String, Vec<(usize, i64)>>,
}

impl CounterSet {
    /// `counter-reset: name v` at tree depth `depth`.
    fn reset(&mut self, name: &str, v: i64, depth: usize) {
        self.stacks
            .entry(name.to_string())
            .or_default()
            .push((depth, v));
    }

    /// `counter-increment: name by`. With no counter of that name in scope, a
    /// fresh one is auto-created at `depth` starting from 0 (then incremented),
    /// per spec.
    fn increment(&mut self, name: &str, by: i64, depth: usize) {
        let stack = self.stacks.entry(name.to_string()).or_default();
        match stack.last_mut() {
            Some(top) => top.1 += by,
            None => stack.push((depth, by)),
        }
    }

    /// The innermost value of `name` (0 when no counter is in scope).
    fn value(&self, name: &str) -> i64 {
        self.stacks
            .get(name)
            .and_then(|s| s.last())
            .map(|(_, v)| *v)
            .unwrap_or(0)
    }

    /// All in-scope values of `name`, outermost→innermost, joined with `sep`
    /// (the `counters(name, sep)` form). `"0"` when none is in scope.
    fn join(&self, name: &str, sep: &str) -> String {
        match self.stacks.get(name) {
            Some(s) if !s.is_empty() => s
                .iter()
                .map(|(_, v)| v.to_string())
                .collect::<Vec<_>>()
                .join(sep),
            _ => "0".to_string(),
        }
    }

    /// Leaving an element at `depth`: counters scoped deeper die; counters
    /// reset AT `depth` survive for the element's following siblings.
    fn exit_depth(&mut self, depth: usize) {
        for stack in self.stacks.values_mut() {
            stack.retain(|(d, _)| *d <= depth);
        }
    }
}

/// Resolve a `content:` value to the generated text. Supports the forms real
/// documents use: quoted strings (concatenated), `attr(name)`,
/// `counter(name[, style])`, `counters(name, "sep"[, style])` (decimal
/// rendering), and the `open-quote`/`close-quote` keywords. `none`/`normal` —
/// or an unparseable value — yields `None` (no pseudo-element is generated).
/// An empty string is a **valid** result: `content: ""` still generates the
/// (empty) box, which the clearfix idiom relies on.
fn resolve_content(value: &str, el: &Element, counters: &CounterSet) -> Option<String> {
    let v = value.trim();
    if v.is_empty() || v.eq_ignore_ascii_case("none") || v.eq_ignore_ascii_case("normal") {
        return None;
    }
    let bytes = v.as_bytes();
    let mut out = String::new();
    let mut produced = false;
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'"' || c == b'\'' {
            // Quoted string with the usual `\` escapes.
            let quote = c;
            i += 1;
            while i < bytes.len() && bytes[i] != quote {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 1;
                    if bytes[i] == b'A' || bytes[i] == b'a' {
                        // `\A` (or `\a`) is the CSS newline escape.
                        out.push('\n');
                    } else {
                        // Any other escape: the character itself, verbatim
                        // (full UTF-8 scalar — never a mid-char byte slice).
                        let ch = v[i..].chars().next().unwrap_or('\u{FFFD}');
                        out.push(ch);
                        i += ch.len_utf8() - 1;
                    }
                } else {
                    // Copy one UTF-8 scalar.
                    let ch = v[i..].chars().next().unwrap_or('\u{FFFD}');
                    out.push(ch);
                    i += ch.len_utf8() - 1;
                }
                i += 1;
            }
            i = (i + 1).min(bytes.len()); // consume the closing quote
            produced = true;
            continue;
        }
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        // An identifier / function token.
        let start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'(' {
            i += 1;
        }
        let word = &v[start..i];
        if i < bytes.len() && bytes[i] == b'(' {
            // Function: capture the balanced argument.
            i += 1;
            let astart = i;
            let mut depth = 1usize;
            while i < bytes.len() {
                match bytes[i] {
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            let arg = &v[astart..i.min(bytes.len())];
            i = (i + 1).min(bytes.len());
            match word.to_ascii_lowercase().as_str() {
                "attr" => {
                    out.push_str(el.attr(arg.trim()).unwrap_or_default());
                    produced = true;
                }
                "counter" => {
                    // `counter(name[, style])` — style rendered as decimal.
                    let name = arg.split(',').next().unwrap_or(arg).trim();
                    out.push_str(&counters.value(name).to_string());
                    produced = true;
                }
                "counters" => {
                    // `counters(name, "sep"[, style])`.
                    let mut parts = arg.splitn(3, ',');
                    let name = parts.next().unwrap_or("").trim();
                    let sep = parts
                        .next()
                        .map(|s| s.trim().trim_matches(['"', '\'']).to_string())
                        .unwrap_or_default();
                    out.push_str(&counters.join(name, &sep));
                    produced = true;
                }
                _ => {} // url(...) / unknown function — contributes nothing
            }
            continue;
        }
        match word.to_ascii_lowercase().as_str() {
            "open-quote" => {
                out.push('\u{201C}');
                produced = true;
            }
            "close-quote" => {
                out.push('\u{201D}');
                produced = true;
            }
            "no-open-quote" | "no-close-quote" => produced = true,
            _ => {} // unknown keyword — contributes nothing
        }
    }
    produced.then_some(out)
}

/// Materialise `::before` / `::after` rules as synthetic children.
///
/// This is the styled-tree construction point: for every element matched by an
/// `x::before` / `x::after` rule whose `content` is not `none`, a
/// [`PSEUDO_ELEMENT_TAG`] child is inserted first / last, carrying the rule's
/// declarations as an inline `style` attribute (so the normal cascade styles
/// it, inheriting from the host element exactly like a real pseudo) and the
/// resolved `content` as its text. `counter-reset` / `counter-increment` are
/// processed document-order-wide during this walk — on the real elements and
/// on the pseudos themselves — so `counter()` / `counters()` produce the
/// standard hierarchical numbering. Documents whose stylesheet has no
/// `::before`/`::after` rule are returned as an untouched clone.
pub fn apply_pseudo_elements(nodes: &[Node], sheet: &Stylesheet) -> Vec<Node> {
    if !sheet.has_generated_content {
        return nodes.to_vec();
    }
    let root = Style::default();
    let mut counters = CounterSet::default();
    pseudo_walk_nodes(nodes, sheet, &root, &[], 0, &mut counters)
}

fn pseudo_walk_nodes(
    nodes: &[Node],
    sheet: &Stylesheet,
    parent_style: &Style,
    ancestors: &[&Element],
    depth: usize,
    counters: &mut CounterSet,
) -> Vec<Node> {
    nodes
        .iter()
        .map(|n| match n {
            Node::Text(t) => Node::Text(t.clone()),
            Node::Element(e) => Node::Element(pseudo_walk_element(
                e,
                sheet,
                parent_style,
                ancestors,
                depth,
                counters,
            )),
        })
        .collect()
}

fn pseudo_walk_element(
    el: &Element,
    sheet: &Stylesheet,
    parent_style: &Style,
    ancestors: &[&Element],
    depth: usize,
    counters: &mut CounterSet,
) -> Element {
    let style = sheet.computed(el, parent_style, ancestors);
    // A `display: none` subtree generates nothing and doesn't touch counters.
    if style.display == Display::None {
        return el.clone();
    }
    // The element's own counter properties apply before its `::before`.
    for (name, v) in &style.counter_reset {
        counters.reset(name, *v, depth);
    }
    for (name, by) in &style.counter_increment {
        counters.increment(name, *by, depth);
    }

    let mut rebuilt = Element {
        tag: el.tag.clone(),
        attrs: el.attrs.clone(),
        children: Vec::new(),
    };
    if let Some(pseudo) = make_pseudo_child(sheet, el, &style, ancestors, "before", depth, counters)
    {
        rebuilt.children.push(Node::Element(pseudo));
    }
    let chain: Vec<&Element> = ancestors.iter().copied().chain([el]).collect();
    rebuilt.children.extend(pseudo_walk_nodes(
        &el.children,
        sheet,
        &style,
        &chain,
        depth + 1,
        counters,
    ));
    if let Some(pseudo) = make_pseudo_child(sheet, el, &style, ancestors, "after", depth, counters)
    {
        rebuilt.children.push(Node::Element(pseudo));
    }
    // Descendant counter scopes die here; scopes opened AT this depth survive
    // for the following siblings.
    counters.exit_depth(depth);
    rebuilt
}

/// Build the synthetic `el::{which}` child, when a matching rule generates one.
fn make_pseudo_child(
    sheet: &Stylesheet,
    el: &Element,
    host_style: &Style,
    ancestors: &[&Element],
    which: &str,
    depth: usize,
    counters: &mut CounterSet,
) -> Option<Element> {
    let decls = sheet.pseudo_decls(el, ancestors, which);
    if decls.is_empty() {
        return None;
    }
    // The last cascaded `content` wins; without one there is no pseudo.
    let raw_content = decls
        .iter()
        .rev()
        .find(|(k, _)| k == "content")
        .map(|(_, v)| v.clone())?;
    // `var()` in the content resolves against the host's custom properties.
    let raw_content = if raw_content.contains("var(") {
        substitute_vars(&raw_content, &host_style.custom_props, 0)?
    } else {
        raw_content
    };
    // The pseudo's own counter properties apply BEFORE its content is
    // resolved (`li::before { counter-increment: item; content: counter(item) }`
    // numbers 1, 2, 3 …). The pseudo counts as one level deeper.
    for (k, v) in &decls {
        match k.as_str() {
            "counter-reset" => {
                for (name, n) in parse_counter_pairs(v, 0) {
                    counters.reset(&name, n, depth + 1);
                }
            }
            "counter-increment" => {
                for (name, n) in parse_counter_pairs(v, 1) {
                    counters.increment(&name, n, depth + 1);
                }
            }
            _ => {}
        }
    }
    let text = resolve_content(&raw_content, el, counters)?;
    // Every other declaration rides on the synthetic element as an inline
    // style, so the ordinary cascade (inheriting from the host) styles the
    // generated box — `display`, colours, margins, all of it.
    let style_attr = decls
        .iter()
        .filter(|(k, _)| !matches!(k.as_str(), "content" | "counter-reset" | "counter-increment"))
        .map(|(k, v)| format!("{k}:{v}"))
        .collect::<Vec<_>>()
        .join(";");
    let mut children = Vec::new();
    if !text.is_empty() {
        children.push(Node::Text(text));
    }
    Some(Element {
        tag: PSEUDO_ELEMENT_TAG.to_string(),
        attrs: vec![("style".to_string(), style_attr)],
        children,
    })
}

/// Whether this `<option>` is the one a closed `<select>` control displays.
///
/// The scope is the nearest ancestor `<select>` (looking through `<optgroup>`
/// and any other wrapper); an orphaned option falls back to its direct parent.
/// Within the scope, the option(s) carrying the `selected` attribute render
/// (covers `<select multiple>` too); with no `selected` anywhere, the **first**
/// option renders — matching what a browser paints for the closed control.
/// A frameless/unmatched option (defensive) renders rather than vanishing.
fn option_is_rendered(el: &Element, ancestors: &[&Element]) -> bool {
    let scope = ancestors
        .iter()
        .rev()
        .find(|a| a.tag == "select")
        .copied()
        .or_else(|| ancestors.last().copied());
    let Some(scope) = scope else {
        return true; // no parent at all — keep it visible
    };
    let mut options: Vec<&Element> = Vec::new();
    collect_options(scope, &mut options);
    if options.is_empty() {
        return true;
    }
    if options.iter().any(|o| o.attr("selected").is_some()) {
        el.attr("selected").is_some()
    } else {
        std::ptr::eq(options[0], el)
    }
}

/// Collect the `<option>` elements under `el` in tree order (descending through
/// `<optgroup>` or any other wrapper, but not *into* an option).
fn collect_options<'a>(el: &'a Element, out: &mut Vec<&'a Element>) {
    for child in &el.children {
        if let Node::Element(e) = child {
            if e.tag == "option" {
                out.push(e);
            } else {
                collect_options(e, out);
            }
        }
    }
}

/// Reset non-inherited properties; keep the inherited ones from the parent.
fn inherit(parent: &Style) -> Style {
    Style {
        // Inherited:
        color: parent.color,
        color_alpha: parent.color_alpha,
        font_size: parent.font_size,
        font_family: parent.font_family.clone(),
        generic_serif: parent.generic_serif,
        generic_mono: parent.generic_mono,
        bold: parent.bold,
        font_weight: parent.font_weight,
        italic: parent.italic,
        underline: parent.underline,
        align: parent.align,
        // `direction` is inherited (CSS), and the `dir` attribute likewise cascades
        // to descendants, so children start from the parent's base direction.
        direction: parent.direction,
        text_transform: parent.text_transform,
        line_height: parent.line_height,
        pre: parent.pre,
        // Reset:
        display: Display::Inline,
        background: None,
        background_alpha: 1.0,
        margin: Edges::default(),
        margin_left_auto: false,
        margin_right_auto: false,
        padding: Edges::default(),
        border_width: Edges::default(),
        border_color: parent.color,
        border_color_alpha: 1.0,
        // Per-side colours reset to the (resolved) text colour like
        // `border-color`; longhands repaint individual sides during cascade.
        border_color_edges: [parent.color; 4],
        // Border line style and background gradient are not inherited.
        border_style_edges: [BorderStyle::Solid; 4],
        background_gradient: None,
        background_image: None,
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
        flex_reverse: false,
        justify: Justify::Start,
        flex_grow: 0.0,
        flex_shrink: 1.0,
        flex_basis: None,
        grid_columns: 0,
        grid_template_columns: Vec::new(),
        grid_template_rows: Vec::new(),
        grid_col_span: 1,
        grid_row_span: 1,
        // `grid-template-areas` / `grid-area` are not inherited (per-element).
        grid_template_areas: Vec::new(),
        grid_area_name: None,
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
        height: None,
        aspect_ratio: None,
        border_box: false,
        position: Position::Static,
        inset: [None; 4],
        z_index: 0,
        // `writing-mode` is inherited in CSS; kept per-element here (see the
        // field doc) so only the declaring block rotates.
        writing_mode: WritingMode::HorizontalTb,
        overflow_clip: false,
        establishes_bfc: false,
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
        // `clear` is not inherited (resets to the initial value).
        clear: Clear::None,
        // `vertical-align` is not inherited; super/sub apply to the run only.
        valign: VShift::Baseline,
        valign_shift: 0.0,
        // `border-radius` / `box-shadow` are box decorations — not inherited.
        border_radius: [0.0; 4],
        border_radius_v: [0.0; 4],
        box_shadow: None,
        box_shadow_extra: Vec::new(),
        // `text-shadow` IS inherited (unlike box decorations above).
        text_shadows: parent.text_shadows.clone(),
        // `transform` is not inherited (each element transforms independently).
        transform: None,
        // Inherited (like `pre`, which mirrors it):
        white_space: parent.white_space,
        // `overflow-wrap`/`word-break` are inherited.
        break_word: parent.break_word,
        // `text-overflow` / `object-fit` / `max-height` are box properties.
        text_overflow_ellipsis: false,
        object_fit: ObjectFit::Fill,
        max_height: None,
        // The `font-family` fallback list travels with `font_family`.
        font_families: parent.font_families.clone(),
        // Custom properties inherit; a child's own `--x` overrides on top.
        custom_props: parent.custom_props.clone(),
        // `opacity` composes per element: the effective value inherits (above,
        // in `opacity`), the element's own declaration starts empty.
        own_opacity: None,
        // Counter properties apply to the element itself only.
        counter_reset: Vec::new(),
        counter_increment: Vec::new(),
        // `transform-origin` rides with `transform` (not inherited).
        transform_origin: None,
        // `caption-side` IS inherited (declared on the table, read on the
        // caption).
        caption_side: parent.caption_side,
    }
}

fn apply_decls(style: &mut Style, decls: &[(String, String)]) {
    for (k, v) in decls {
        apply_one(style, k, v);
    }
}

/// Parse a `grid-template-columns` / `grid-template-rows` value into a list of
/// per-track sizings.
///
/// Supports the forms documents use: an explicit track list
/// (`1fr 1fr 200px auto`), `repeat(N, <track-list>)` (expanded inline, with N
/// the count), `minmax(min, max)`, `fr`, fixed lengths, percentages and `auto`.
/// `none`/empty yields an empty list (the grid then falls back to equal
/// columns / content-sized rows). Unknown tokens parse as `auto`, never
/// dropping the track, so the column *count* stays faithful.
fn parse_track_list(v: &str, em: f64) -> Vec<TrackSize> {
    let v = v.trim();
    if v.is_empty() || v.eq_ignore_ascii_case("none") {
        return Vec::new();
    }
    let mut out = Vec::new();
    for tok in tokenize_track_list(v) {
        if let Some(rest) = strip_func(&tok, "repeat") {
            // `repeat(N, <track-list>)` — expand the inner list N times. A
            // keyword count (`auto-fill`/`auto-fit`) can't be sized without the
            // container, so treat it as a single repetition of the inner list.
            if let Some((count_tok, list)) = rest.split_once(',') {
                let inner = parse_track_list(list.trim(), em);
                let n = count_tok.trim().parse::<usize>().unwrap_or(1).max(1);
                for _ in 0..n {
                    out.extend(inner.iter().cloned());
                }
                continue;
            }
        }
        out.push(parse_track_size(&tok, em));
    }
    out
}

/// Parse `grid-template-areas: "a a b" "a a c" …` into the bounding rectangle of
/// each named area (CSS requires areas to be rectangular; we take the min/max
/// row+col of each name's cells). `.` marks an empty cell and is skipped.
fn parse_grid_template_areas(v: &str) -> Vec<GridAreaRect> {
    // Each quoted string is one row of area-name tokens. CSS allows single OR
    // double quotes; normalise to one delimiter before splitting.
    let normalised = v.replace('\'', "\"");
    let rows: Vec<Vec<&str>> = normalised
        .split('"')
        .enumerate()
        .filter(|(i, _)| i % 2 == 1) // odd segments are inside quotes
        .map(|(_, s)| s.split_whitespace().collect::<Vec<_>>())
        .filter(|r: &Vec<&str>| !r.is_empty())
        .collect();
    let mut out: Vec<GridAreaRect> = Vec::new();
    for (r, row) in rows.iter().enumerate() {
        for (c, name) in row.iter().enumerate() {
            if *name == "." {
                continue;
            }
            if let Some(a) = out.iter_mut().find(|a| a.name == *name) {
                // Rows/cols are scanned in increasing order, so grow the span.
                a.row_span = (r + 1 - a.row).max(a.row_span);
                a.col_span = (c + 1 - a.col).max(a.col_span);
            } else {
                out.push(GridAreaRect {
                    name: name.to_string(),
                    row: r,
                    col: c,
                    row_span: 1,
                    col_span: 1,
                });
            }
        }
    }
    out
}

/// Parse one track sizing token (already isolated by `tokenize_track_list`).
fn parse_track_size(tok: &str, em: f64) -> TrackSize {
    let t = tok.trim();
    if let Some(rest) = strip_func(t, "minmax") {
        if let Some((a, b)) = split_top_level_comma(rest) {
            return TrackSize::MinMax(
                Box::new(parse_track_size(a.trim(), em)),
                Box::new(parse_track_size(b.trim(), em)),
            );
        }
    }
    if t.eq_ignore_ascii_case("auto")
        || t.eq_ignore_ascii_case("min-content")
        || t.eq_ignore_ascii_case("max-content")
    {
        return TrackSize::Auto;
    }
    if let Some(n) = t.strip_suffix("fr") {
        if let Ok(f) = n.trim().parse::<f64>() {
            return TrackSize::Fr(f.max(0.0));
        }
    }
    if let Some(n) = t.strip_suffix('%') {
        if let Ok(p) = n.trim().parse::<f64>() {
            return TrackSize::Percent(p);
        }
    }
    if let Some(px) = parse_len_px(t, em) {
        return TrackSize::Pt(px);
    }
    // `fit-content(...)`, named-line `[…]` remnants, or anything unknown.
    TrackSize::Auto
}

/// Split a track list into top-level tokens, keeping parenthesised functions
/// (`repeat(...)`, `minmax(...)`) whole and dropping `[line-name]` blocks.
fn tokenize_track_list(v: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    let mut in_name = false; // inside a `[ … ]` line-name block
    for ch in v.chars() {
        match ch {
            '[' if depth == 0 => in_name = true,
            ']' if in_name => in_name = false,
            _ if in_name => {}
            '(' => {
                depth += 1;
                cur.push(ch);
            }
            ')' => {
                depth -= 1;
                cur.push(ch);
            }
            c if c.is_whitespace() && depth == 0 => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// If `t` is `name(<inner>)` (case-insensitive name), return `<inner>`.
fn strip_func<'a>(t: &'a str, name: &str) -> Option<&'a str> {
    let t = t.trim();
    let open = t.find('(')?;
    if t[..open].eq_ignore_ascii_case(name) {
        t.strip_suffix(')').map(|s| &s[open + 1..])
    } else {
        None
    }
}

/// Split `a, b` on the first top-level comma (ignoring commas inside nested
/// parentheses, e.g. a nested `minmax(...)`).
fn split_top_level_comma(s: &str) -> Option<(&str, &str)> {
    let mut depth = 0i32;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => return Some((&s[..i], &s[i + 1..])),
            _ => {}
        }
    }
    None
}

/// Split `a, b, c, …` on every top-level comma (commas inside nested parens —
/// e.g. an `rgb(…)` colour — stay with their part). Trimmed, empties dropped.
fn split_top_level_commas(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                let part = s[start..i].trim();
                if !part.is_empty() {
                    out.push(part);
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    let last = s[start..].trim();
    if !last.is_empty() {
        out.push(last);
    }
    out
}

/// Parse a `background[-image]` value that *is* a gradient function into a
/// [`CssGradient`]. Recognises `linear-gradient(...)`, `radial-gradient(...)` and
/// `conic-gradient(...)` (and their `repeating-*` aliases, treated as the
/// non-repeating form — the ramp still fills the box). Returns `None` for any
/// other value (a flat colour, an `url(...)`, an unknown function), leaving the
/// caller's solid-colour path in charge. The leading `var(...)` is resolved once
/// so a gradient stored in a custom-property fallback is still seen.
fn parse_css_gradient(v: &str, em: f64) -> Option<CssGradient> {
    let v = resolve_var(v);
    let v = v.trim();
    // Accept the `repeating-` prefix by stripping it: we render the single ramp.
    let bare = v.strip_prefix("repeating-").unwrap_or(v);
    if let Some(inner) = strip_func(bare, "linear-gradient") {
        return parse_linear_gradient(inner, em).map(CssGradient::Linear);
    }
    if let Some(inner) = strip_func(bare, "radial-gradient") {
        return parse_radial_gradient(inner, em).map(CssGradient::Radial);
    }
    if let Some(inner) = strip_func(bare, "conic-gradient") {
        return parse_conic_gradient(inner, em).map(CssGradient::Conic);
    }
    None
}

/// Extract the source from the first `url(...)` token in a value (quotes
/// optional). `None` when there is no `url(...)`. base64 `data:` URIs and plain
/// URLs contain no `)`, so the first `)` closes the function.
pub(crate) fn extract_css_url(v: &str) -> Option<String> {
    let lower = v.to_ascii_lowercase();
    let open = lower.find("url(")? + 4;
    let close = v[open..].find(')')? + open;
    let inner = v[open..close].trim().trim_matches(['"', '\'']).trim();
    (!inner.is_empty()).then(|| inner.to_string())
}

/// Collect **every** `url(...)` source in a value, in order. A CSS `src:`
/// declaration may list several alternatives — e.g.
/// `url(a.woff2) format('woff2'), url(b.woff) format('woff')` — so unlike
/// [`extract_css_url`] (which returns only the first) this returns all of them,
/// skipping the `format(...)`/`local(...)` tokens between them. base64 `data:`
/// URIs and plain URLs contain no `)`, so the first `)` always closes each
/// function — paren-matching stays safe.
pub(crate) fn collect_all_css_urls(v: &str) -> Vec<String> {
    let lower = v.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = lower[from..].find("url(") {
        let open = from + rel + 4;
        let Some(rel_close) = v[open..].find(')') else {
            break;
        };
        let close = open + rel_close;
        let inner = v[open..close].trim().trim_matches(['"', '\'']).trim();
        if !inner.is_empty() {
            out.push(inner.to_string());
        }
        from = close + 1;
    }
    out
}

/// Parse the inside of a `radial-gradient(...)` into a [`RadialGradient`].
///
/// The optional leading configuration (`<shape> <size> [at <position>]`) is read
/// only for its `at <position>` (centre, defaulting to the box centre) and a
/// `closest-side`/`farthest-corner`/etc. *size keyword*, which scales the end
/// radius fraction; explicit lengths/ellipse axes collapse to a circle. Whatever
/// remains are `<color> [<pos>]` stops (centre→edge). Returns `None` unless ≥ 2
/// stops resolve, leaving any solid `background` in place.
fn parse_radial_gradient(inner: &str, em: f64) -> Option<RadialGradient> {
    let parts = split_top_level_commas(inner);
    if parts.is_empty() {
        return None;
    }
    // First part is a config (not a stop) only if it carries no colour: it then
    // holds the shape/size/position keywords. Otherwise every part is a stop.
    let first_is_config = parse_color_in(parts[0]).is_none() && is_radial_config(parts[0]);
    let (cx, cy, r, stop_parts): (f64, f64, f64, &[&str]) = if first_is_config {
        let (cx, cy) = parse_at_position(parts[0]);
        (cx, cy, radial_size_fraction(parts[0]), &parts[1..])
    } else {
        (0.5, 0.5, 1.0, &parts[..])
    };
    let stops = finish_gradient_stops(
        stop_parts
            .iter()
            .filter_map(|p| parse_gradient_stop(p, em))
            .collect(),
    );
    if stops.len() < 2 {
        return None;
    }
    Some(RadialGradient { cx, cy, r, stops })
}

/// Parse the inside of a `conic-gradient(...)` into a [`ConicGradient`].
///
/// The optional leading `[from <angle>] [at <position>]` sets the sweep start
/// angle (CSS `0deg` = up, clockwise; default `0`) and centre (default box
/// centre). The remaining parts are `<color> [<pos>]` stops where `<pos>` is a
/// `%`/`0..1` fraction of the full turn (angular positions in `deg`/`turn` are
/// also accepted and normalised to a fraction). Returns `None` unless ≥ 2 stops
/// resolve.
fn parse_conic_gradient(inner: &str, em: f64) -> Option<ConicGradient> {
    let parts = split_top_level_commas(inner);
    if parts.is_empty() {
        return None;
    }
    let first_is_config = parse_color_in(parts[0]).is_none() && is_conic_config(parts[0]);
    let (cx, cy, from_deg, stop_parts): (f64, f64, f64, &[&str]) = if first_is_config {
        let (cx, cy) = parse_at_position(parts[0]);
        (cx, cy, parse_from_angle(parts[0]), &parts[1..])
    } else {
        (0.5, 0.5, 0.0, &parts[..])
    };
    let stops = finish_gradient_stops(
        stop_parts
            .iter()
            .filter_map(|p| parse_conic_stop(p, em))
            .collect(),
    );
    if stops.len() < 2 {
        return None;
    }
    Some(ConicGradient {
        cx,
        cy,
        from_deg,
        stops,
    })
}

/// True if `part` looks like a `radial-gradient` configuration token group
/// (shape/size/position keywords) rather than a colour stop.
fn is_radial_config(part: &str) -> bool {
    let p = part.trim().to_ascii_lowercase();
    p.starts_with("at ")
        || p.starts_with("circle")
        || p.starts_with("ellipse")
        || p.contains("closest-side")
        || p.contains("closest-corner")
        || p.contains("farthest-side")
        || p.contains("farthest-corner")
}

/// True if `part` looks like a `conic-gradient` configuration token group
/// (`from <angle>` and/or `at <position>`).
fn is_conic_config(part: &str) -> bool {
    let p = part.trim().to_ascii_lowercase();
    p.starts_with("from ") || p.starts_with("at ")
}

/// Map a radial size keyword to an end-radius fraction of `min(w,h)/2`.
///
/// In this circular model only `farthest-corner` reaches measurably past the
/// nearer side — `≈√2` (a square's corner) — so it gets the extended fraction.
/// Every other keyword (`closest-side` — the CSS default for circles —
/// `closest-corner`, `farthest-side`) and the absent/unknown case stop at the
/// nearer side (`1.0`); modelling their small ellipse-dependent differences isn't
/// worth it for a background fill.
fn radial_size_fraction(part: &str) -> f64 {
    if part.to_ascii_lowercase().contains("farthest-corner") {
        std::f64::consts::SQRT_2
    } else {
        1.0
    }
}

/// Read an `at <position>` clause into centre fractions `(cx, cy)` of the box
/// (`0..=1`). Supports the keyword corners/edges (`top`/`left`/`center`/…) and
/// percentage pairs (`at 25% 75%`). Defaults to the box centre `(0.5, 0.5)`.
fn parse_at_position(part: &str) -> (f64, f64) {
    let lower = part.to_ascii_lowercase();
    let Some(after) = lower.split("at ").nth(1) else {
        return (0.5, 0.5);
    };
    let mut cx = 0.5;
    let mut cy = 0.5;
    let mut pct_axis = 0; // first % → x, second % → y
    for tok in after.split_whitespace() {
        match tok {
            "left" => cx = 0.0,
            "right" => cx = 1.0,
            "top" => cy = 0.0,
            "bottom" => cy = 1.0,
            "center" | "centre" => {}
            _ => {
                if let Some(pc) = tok.strip_suffix('%').and_then(|n| n.parse::<f64>().ok()) {
                    let f = (pc / 100.0).clamp(0.0, 1.0);
                    if pct_axis == 0 {
                        cx = f;
                        pct_axis = 1;
                    } else {
                        cy = f;
                    }
                }
            }
        }
    }
    (cx.clamp(0.0, 1.0), cy.clamp(0.0, 1.0))
}

/// Read the `from <angle>` of a conic config into CSS degrees (0 = up,
/// clockwise). Absent/unparseable ⇒ `0.0`.
fn parse_from_angle(part: &str) -> f64 {
    let lower = part.to_ascii_lowercase();
    lower
        .split("from ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(parse_gradient_angle)
        .unwrap_or(0.0)
}

/// Is there a parseable colour anywhere in `part`? Used to tell a leading
/// configuration group (no colour) from the first colour stop.
fn parse_color_in(part: &str) -> Option<[f64; 3]> {
    part.split_whitespace().find_map(parse_color)
}

/// Parse one conic `<color> [<angular-pos>]` stop. The position may be a `%` /
/// bare `0..1` (a fraction of the turn) or an angle (`deg`/`turn`) which is
/// normalised to a `0..1` fraction. `None` if no colour parses. The `bool`
/// flags a `transparent` stop (see [`finish_gradient_stops`]).
fn parse_conic_stop(p: &str, _em: f64) -> Option<(GradientStop, bool)> {
    let mut color = None;
    let mut alpha = 1.0;
    let mut transparent = false;
    let mut pos = None;
    for tok in p.split_whitespace() {
        if let Some(pct) = tok.strip_suffix('%').and_then(|n| n.parse::<f64>().ok()) {
            pos = Some((pct / 100.0).clamp(0.0, 1.0));
        } else if tok.eq_ignore_ascii_case("transparent") {
            color = Some([0.0, 0.0, 0.0]);
            alpha = 0.0;
            transparent = true;
        } else if let Some((c, a)) = parse_color_alpha(tok) {
            color = Some(c);
            alpha = a;
        } else if let Some(deg) = parse_gradient_angle(tok) {
            // An explicit angle stop → fraction of the full turn.
            pos = Some((deg / 360.0).rem_euclid(1.0));
        } else if let Ok(n) = tok.parse::<f64>() {
            pos = Some(n.clamp(0.0, 1.0));
        }
    }
    color.map(|color| (GradientStop { color, pos, alpha }, transparent))
}

/// Parse the inside of a `linear-gradient(...)` into a [`LinearGradient`].
///
/// Accepts an optional leading direction — an angle (`90deg`, `0.25turn`, a bare
/// number treated as degrees) or a `to <side[ side]>` keyword — followed by two
/// or more colour stops (`<color> [<pos>]`, where `<pos>` is a `%` or `0..1`
/// number). Returns `None` unless at least two stops resolve, so a malformed or
/// non-linear value leaves any solid `background` in place. Default direction is
/// `180deg` (`to bottom`), per CSS.
fn parse_linear_gradient(inner: &str, em: f64) -> Option<LinearGradient> {
    let parts = split_top_level_commas(inner);
    if parts.is_empty() {
        return None;
    }
    // The first part is a direction only if it has no colour (a colour-less
    // angle/keyword); otherwise every part is a stop and the angle defaults.
    let (angle_deg, stop_parts): (f64, &[&str]) = match parse_gradient_direction(parts[0]) {
        Some(a) => (a, &parts[1..]),
        None => (180.0, &parts[..]),
    };
    let stops = finish_gradient_stops(
        stop_parts
            .iter()
            .filter_map(|p| parse_gradient_stop(p, em))
            .collect(),
    );
    if stops.len() < 2 {
        return None;
    }
    Some(LinearGradient { angle_deg, stops })
}

/// Parse a gradient direction token: a CSS angle (`Ndeg`/`Nturn`/`Nrad`/`Ngrad`
/// or a bare number = degrees) or a `to <side>[ <side>]` keyword. Returns the
/// angle in CSS degrees (`0` = up, clockwise), or `None` if it isn't a
/// direction (i.e. it's the first colour stop).
fn parse_gradient_direction(tok: &str) -> Option<f64> {
    let t = tok.trim();
    if let Some(sides) = t.strip_prefix("to ") {
        return Some(side_keywords_to_angle(sides.trim()));
    }
    parse_gradient_angle(t)
}

/// Convert a CSS angle literal to degrees. Supports `deg`, `grad`, `rad`, `turn`
/// and a bare number (degrees). `None` if it isn't a number-with-(optional)-unit.
fn parse_gradient_angle(t: &str) -> Option<f64> {
    let lower = t.to_ascii_lowercase();
    for (unit, to_deg) in [
        ("deg", 1.0),
        ("grad", 0.9),                         // 400grad = 360deg
        ("turn", 360.0),                       // 1turn = 360deg
        ("rad", 180.0 / std::f64::consts::PI), // radians → degrees
    ] {
        if let Some(num) = lower.strip_suffix(unit) {
            return num.trim().parse::<f64>().ok().map(|n| n * to_deg);
        }
    }
    // A bare number is degrees; reject anything else (e.g. a colour name).
    lower.parse::<f64>().ok()
}

/// `to right` ⇒ 90, `to bottom` ⇒ 180, `to left` ⇒ 270, `to top` ⇒ 0; the
/// diagonal `to <v> <h>` corners use the 45° approximations CSS rounds to for a
/// square box (good enough for our axial fill). Unknown ⇒ 180 (`to bottom`).
fn side_keywords_to_angle(sides: &str) -> f64 {
    let mut top = false;
    let mut bottom = false;
    let mut left = false;
    let mut right = false;
    for s in sides.split_whitespace() {
        match s {
            "top" => top = true,
            "bottom" => bottom = true,
            "left" => left = true,
            "right" => right = true,
            _ => {}
        }
    }
    match (top, bottom, left, right) {
        (true, _, false, false) => 0.0,
        (_, true, false, false) => 180.0,
        (false, false, false, true) => 90.0,
        (false, false, true, false) => 270.0,
        (true, _, _, true) => 45.0,  // to top right
        (_, true, _, true) => 135.0, // to bottom right
        (_, true, true, _) => 225.0, // to bottom left
        (true, _, true, _) => 315.0, // to top left
        _ => 180.0,
    }
}

/// Parse one `<color> [<position>]` gradient stop. `<position>` is a `%` or a
/// `0..1` number (other length units are ignored — the stop is then auto-placed).
/// `None` if no colour parses. The `bool` flags a `transparent` stop, whose
/// colour is later borrowed from its nearest opaque neighbour (see
/// [`finish_gradient_stops`]) so the ramp fades out in place instead of
/// towards black.
fn parse_gradient_stop(p: &str, _em: f64) -> Option<(GradientStop, bool)> {
    let mut color = None;
    let mut alpha = 1.0;
    let mut transparent = false;
    let mut pos = None;
    for tok in p.split_whitespace() {
        if let Some(pct) = tok.strip_suffix('%').and_then(|n| n.parse::<f64>().ok()) {
            pos = Some((pct / 100.0).clamp(0.0, 1.0));
        } else if tok.eq_ignore_ascii_case("transparent") {
            // Kept as a stop: black alpha-0 per spec, but we substitute the
            // neighbour's colour so the fade doesn't muddy towards grey.
            color = Some([0.0, 0.0, 0.0]);
            alpha = 0.0;
            transparent = true;
        } else if let Some((c, a)) = parse_color_alpha(tok) {
            color = Some(c);
            alpha = a;
        } else if let Ok(n) = tok.parse::<f64>() {
            // A bare 0..1 number as a position (rare but valid in our subset).
            pos = Some(n.clamp(0.0, 1.0));
        }
    }
    color.map(|color| (GradientStop { color, pos, alpha }, transparent))
}

/// Resolve `transparent` stops against their neighbours: each keeps its
/// position and `alpha = 0`, but takes the colour of the nearest non-
/// transparent stop (previous first, else next) so interpolation fades the
/// *neighbour's* colour out rather than blending towards black. An all-
/// transparent ramp keeps black (nothing better to borrow).
fn finish_gradient_stops(raw: Vec<(GradientStop, bool)>) -> Vec<GradientStop> {
    let mut out: Vec<GradientStop> = raw.iter().map(|(s, _)| *s).collect();
    for i in 0..out.len() {
        if !raw[i].1 {
            continue;
        }
        let borrowed = raw[..i]
            .iter()
            .rev()
            .chain(raw[i + 1..].iter())
            .find(|(_, t)| !t)
            .map(|(s, _)| s.color);
        if let Some(c) = borrowed {
            out[i].color = c;
        }
    }
    out
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

/// Parse a `grid-column`/`grid-row` placement into a 1-based `(start, span)`.
///
/// `start` is 0 for auto-flow, else the 1-based start line. `span` is the track
/// count the item covers (≥ 1). Supported forms:
/// - `2` → start 2, span 1
/// - `1 / 3` → start 1, end line 3 ⇒ span 2
/// - `span 2` → start 0 (auto), span 2
/// - `2 / span 3` → start 2, span 3
/// - `auto` / unknown ⇒ start 0, span 1.
fn parse_grid_placement(v: &str) -> (usize, usize) {
    let mut it = v.split('/');
    let first = it.next().unwrap_or(v).trim();
    let second = it.next().map(str::trim);

    let parse_span = |s: &str| -> Option<usize> {
        s.strip_prefix("span")
            .map(str::trim)
            .and_then(|n| n.parse::<usize>().ok())
            .map(|n| n.max(1))
    };

    // Leading `span N` (no explicit start) → auto start, N-track span.
    if let Some(span) = parse_span(first) {
        return (0, span);
    }
    let start = if first.is_empty() || first == "auto" {
        0
    } else {
        first.parse::<usize>().unwrap_or(0)
    };
    // Second component: `span N`, or an end line (`/ <m>` ⇒ span = m − start).
    let span = match second {
        Some(s) => {
            if let Some(span) = parse_span(s) {
                span
            } else if let Ok(end) = s.parse::<usize>() {
                if start >= 1 && end > start {
                    end - start
                } else {
                    1
                }
            } else {
                1
            }
        }
        None => 1,
    };
    (start, span)
}

/// Decompose the `flex` shorthand into `flex-grow`, `flex-shrink`, `flex-basis`.
///
/// CSS grammar: `none | [ <grow> <shrink>? || <basis> ]`. Per spec the shorthand
/// resets all three components; the common forms are handled:
/// - `flex: none` → `0 0 auto`
/// - `flex: auto` → `1 1 auto`
/// - `flex: initial` → `0 1 auto`
/// - `flex: <number>` (e.g. `flex: 1`) → `<n> 1 0` (basis 0, the one-value rule)
/// - `flex: <grow> <shrink>` → those two, basis `0`
/// - `flex: <grow> <basis>` / `flex: <grow> <shrink> <basis>` — a token carrying
///   a unit/`%`/`auto` is the basis; bare numbers are grow then shrink.
fn apply_flex_shorthand(style: &mut Style, v: &str) {
    let v = v.trim();
    match v {
        "none" => {
            style.flex_grow = 0.0;
            style.flex_shrink = 0.0;
            style.flex_basis = None;
            return;
        }
        "auto" => {
            style.flex_grow = 1.0;
            style.flex_shrink = 1.0;
            style.flex_basis = None;
            return;
        }
        "initial" => {
            style.flex_grow = 0.0;
            style.flex_shrink = 1.0;
            style.flex_basis = None;
            return;
        }
        _ => {}
    }

    // Reset to the shorthand's defaults, then apply the provided components.
    let mut grow = 0.0;
    let mut shrink = 1.0;
    let mut basis: Option<Len> = None;
    let mut numbers_seen = 0; // bare numbers map to grow (0th) then shrink (1st)
    let mut basis_seen = false;
    let mut saw_any_number = false;

    for tok in v.split_whitespace() {
        if tok == "auto" || tok == "content" {
            basis = None;
            basis_seen = true;
        } else if tok.ends_with('%')
            || LENGTH_UNITS
                .iter()
                .any(|u| tok.len() > u.len() && tok.ends_with(u))
        {
            basis = parse_len(tok, style.font_size);
            basis_seen = true;
        } else if let Ok(n) = tok.parse::<f64>() {
            match numbers_seen {
                0 => grow = n.max(0.0),
                1 => shrink = n.max(0.0),
                _ => {}
            }
            numbers_seen += 1;
            saw_any_number = true;
        }
    }

    // One-value numeric form (`flex: 1`): grow = n, shrink = 1, basis = 0.
    if saw_any_number && numbers_seen == 1 && !basis_seen {
        basis = Some(Len::Pt(0.0));
    }

    style.flex_grow = grow;
    style.flex_shrink = shrink;
    style.flex_basis = basis;
}

/// Apply `font-family` (a comma-separated stack): adopt the first family name
/// and infer the serif/mono generic buckets used for fallback metrics.
/// Parse a `counter-reset` / `counter-increment` value into `(name, value)`
/// pairs: a whitespace list of counter names, each optionally followed by an
/// integer. `default` is the per-name value when the integer is absent (`0`
/// for reset, `1` for increment). `none` (or nothing parseable) yields an
/// empty list.
fn parse_counter_pairs(v: &str, default: i64) -> Vec<(String, i64)> {
    let mut out: Vec<(String, i64)> = Vec::new();
    if v.trim().eq_ignore_ascii_case("none") {
        return out;
    }
    for tok in v.split_whitespace() {
        if let Ok(n) = tok.parse::<i64>() {
            // An integer adjusts the preceding counter name.
            if let Some(last) = out.last_mut() {
                last.1 = n;
            }
        } else if tok
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '-' || c == '_')
        {
            out.push((tok.to_string(), default));
        }
    }
    out
}

fn apply_font_family(style: &mut Style, v: &str) {
    // Keep the FULL fallback list (de-quoted, source order): the font resolver
    // tries each family and uses the first one it has, so `font-family:
    // "MissingFont", serif` really falls back to a serif face instead of a
    // sans-serif guess for the unknown first name.
    let names: Vec<String> = v
        .split(',')
        .map(|t| t.trim().trim_matches(['"', '\'']).to_string())
        .filter(|t| !t.is_empty())
        .collect();
    if names.is_empty() {
        return;
    }
    style.font_family = names[0].clone();
    // Generic serif/mono flags: the first name in the list that clearly
    // announces a generic wins — so `"MissingFont", serif` sets serif, while
    // `Arial, serif` stops at the (sans) Arial and keeps the old flags. The
    // single-name case is byte-identical to the previous first-name heuristic.
    let is_serif = |l: &str| l == "serif" || l.contains("times") || l.contains("georgia");
    let is_mono = |l: &str| {
        l == "monospace" || l.contains("courier") || l.contains("mono") || l.contains("consol")
    };
    let is_sans = |l: &str| {
        l == "sans-serif"
            || l.contains("arial")
            || l.contains("helvetica")
            || l.contains("verdana")
            || l.contains("tahoma")
            || l.contains("segoe")
    };
    style.generic_serif = false;
    style.generic_mono = false;
    for name in &names {
        let lower = name.to_ascii_lowercase();
        if is_serif(&lower) {
            style.generic_serif = true;
            break;
        }
        if is_mono(&lower) {
            style.generic_mono = true;
            break;
        }
        if is_sans(&lower) {
            break; // an explicit sans family: flags stay off
        }
        // An unknown concrete family: keep scanning — the next fallback name
        // may reveal the intended generic.
    }
    style.font_families = names;
}

/// Apply a `font-weight` value: store the numeric weight (named keywords map to
/// the canonical 400/700; `bolder`/`lighter` are approximated relative to the
/// current weight) and refresh the `bold` rendering flag (`weight >= 600`).
fn apply_font_weight(style: &mut Style, v: &str) {
    let weight = match v.trim() {
        "normal" => 400,
        "bold" => 700,
        // Relative keywords: step one weight band from the inherited value.
        "bolder" => (style.font_weight + 300).min(900),
        "lighter" => style.font_weight.saturating_sub(300).max(100),
        n => n
            .parse::<u16>()
            .map(|w| w.clamp(1, 1000))
            .unwrap_or(style.font_weight),
    };
    style.font_weight = weight;
    style.bold = weight >= 600;
}

/// Decompose the `font` shorthand into its longhands.
///
/// CSS grammar: `[ <style> || <variant> || <weight> || <stretch> ]? <size>[/<line-height>]? <family>`.
/// The size is the pivot: it's the first token that is a `<length>`/`<percentage>`
/// (optionally carrying `/<line-height>`); everything before it is the optional
/// style/variant/weight prefix and everything after it is the family list. The
/// `font: inherit|caption|menu|…` system/keyword forms carry no size and are left
/// to the cascade (a no-op here, which keeps the inherited font intact).
fn apply_font_shorthand(style: &mut Style, v: &str) {
    let v = v.trim();
    let tokens: Vec<&str> = v.split_whitespace().collect();
    // Locate the size token: the first token whose head (before any `/`) is a
    // genuine `<font-size>`. Crucially this must NOT match a bare unitless number
    // (e.g. the `600` weight in `font: 600 14pt Arial`) — `font-size` requires a
    // unit/`%` or an absolute size keyword, so a unitless number stays a weight.
    let size_idx = tokens
        .iter()
        .position(|t| is_font_size_token(t.split('/').next().unwrap_or(t), style.font_size));
    let Some(size_idx) = size_idx else {
        return; // no size → system font / keyword form: leave the font as-is.
    };

    // Prefix tokens (style / variant / weight). `font` resets these longhands to
    // their initial values first, then re-applies whatever the prefix specifies.
    style.italic = false;
    style.font_weight = 400;
    style.bold = false;
    for t in &tokens[..size_idx] {
        match *t {
            "italic" | "oblique" => style.italic = true,
            "normal" | "small-caps" => {} // variant/normal: nothing to model.
            _ => apply_font_weight(style, t), // `bold`/`100`…`900`/`bolder`/`lighter`.
        }
    }

    // Size (and optional `/line-height`).
    let size_tok = tokens[size_idx];
    if let Some((size_str, lh_str)) = size_tok.split_once('/') {
        if let Some(px) = font_size_px(size_str, style.font_size) {
            style.font_size = px;
        }
        apply_one(style, "line-height", lh_str);
    } else if let Some(px) = font_size_px(size_tok, style.font_size) {
        style.font_size = px;
    }

    // Family: everything after the size, rejoined (it may contain spaces/commas).
    if size_idx + 1 < tokens.len() {
        let family = tokens[size_idx + 1..].join(" ");
        apply_font_family(style, &family);
    }
}

/// Is `t` a valid `<font-size>` token? Accepts a length/percentage that carries
/// a unit or `%`, plus the absolute/relative size keywords. A bare unitless
/// number is rejected so it is not mistaken for the size in the `font` shorthand
/// (where it is actually a `font-weight`).
fn is_font_size_token(t: &str, em: f64) -> bool {
    font_size_px(t, em).is_some()
}

/// Resolve a `<font-size>` token to points: a length/percentage with a unit, or
/// an absolute/relative size keyword. Returns `None` for bare unitless numbers
/// and anything else (so the `font` shorthand's size detection stays precise).
fn font_size_px(t: &str, em: f64) -> Option<f64> {
    let t = t.trim();
    // Absolute/relative keyword sizes (CSS `<absolute-size>` / `<relative-size>`),
    // anchored to the conventional 12pt medium baseline.
    match t.to_ascii_lowercase().as_str() {
        "xx-small" => return Some(12.0 * 0.6),
        "x-small" => return Some(12.0 * 0.75),
        "small" => return Some(12.0 * 0.89),
        "medium" => return Some(12.0),
        "large" => return Some(12.0 * 1.2),
        "x-large" => return Some(12.0 * 1.5),
        "xx-large" => return Some(12.0 * 2.0),
        "smaller" => return Some(em * 0.833),
        "larger" => return Some(em * 1.2),
        _ => {}
    }
    // A length/percentage must carry a unit (or `%`); a bare number is not a
    // valid font-size and must not pivot the shorthand. Limit to the units
    // `parse_len_px` resolves so detection and resolution stay in lock-step.
    let has_unit = t.ends_with('%')
        || LENGTH_UNITS
            .iter()
            .any(|u| t.len() > u.len() && t.ends_with(u));
    if !has_unit {
        return None;
    }
    parse_len_px(t, em)
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
    // Custom properties (`--name: value`) are collected into
    // `Style::custom_props` before the cascade applies normal declarations
    // (see [`Stylesheet::computed`]); they are never properties themselves.
    if prop.starts_with("--") {
        style
            .custom_props
            .insert(prop.to_string(), value.trim().to_string());
        return;
    }
    let v = value.trim();
    // Substitute `var(--name[, fallback])` references against the custom
    // properties in scope (inherited + own). A reference that resolves to
    // nothing — unknown name with no fallback, or a reference cycle — makes the
    // declaration *invalid at computed-value time*: it is skipped, leaving the
    // inherited/previous value in place.
    let substituted;
    let v = if v.contains("var(") {
        match substitute_vars(v, &style.custom_props, 0) {
            Some(s) => {
                substituted = s;
                substituted.trim()
            }
            None => return,
        }
    } else {
        v
    };
    // `currentColor` resolves to the element's own (already-cascaded) colour, so
    // a bare `border-color: currentColor` / `background: currentColor` / … picks
    // up `color`. Substitute it as an `rgb(...)` literal before parsing. (The
    // `color` property itself is a no-op: `style.color` is the inherited value.)
    let resolved_current;
    let v = if v.eq_ignore_ascii_case("currentcolor") {
        let [r, g, b] = style.color;
        resolved_current = format!("rgb({},{},{})", r * 255.0, g * 255.0, b * 255.0);
        resolved_current.as_str()
    } else {
        v
    };
    match prop {
        "display" => {
            // `display: flow-root` is a plain block that *establishes a BFC*
            // (contains its floats, doesn't leak them to siblings).
            style.establishes_bfc = v == "flow-root";
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
            style.flex_column = v.starts_with("column");
            style.flex_reverse = v.ends_with("-reverse");
        }
        "justify-content" => {
            style.justify = match v {
                "center" => Justify::Center,
                "flex-end" | "end" | "right" => Justify::End,
                "space-between" => Justify::SpaceBetween,
                "space-around" => Justify::SpaceAround,
                "space-evenly" => Justify::SpaceEvenly,
                _ => Justify::Start,
            };
        }
        "flex-grow" => {
            style.flex_grow = v.parse().unwrap_or(0.0);
        }
        "flex-shrink" => {
            style.flex_shrink = v.parse::<f64>().unwrap_or(1.0).max(0.0);
        }
        "flex-basis" => {
            style.flex_basis = if v == "auto" || v == "content" {
                None
            } else {
                parse_len(v, style.font_size)
            };
        }
        "flex" => apply_flex_shorthand(style, v),
        "grid-template-columns" => {
            let tracks = parse_track_list(v, style.font_size);
            // Keep the column COUNT as the authoritative value (≥ 1 when a track
            // list is present) so existing equal-column behaviour is preserved
            // when no detailed sizing applies.
            style.grid_columns = tracks.len().max(1);
            style.grid_template_columns = tracks;
        }
        "grid-template-rows" => {
            let tracks = parse_track_list(v, style.font_size);
            style.grid_rows = tracks.len();
            style.grid_template_rows = tracks;
        }
        "grid-template-areas" => {
            style.grid_template_areas = parse_grid_template_areas(v);
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
            let (start, span) = parse_grid_placement(v);
            style.grid_col_start = start;
            style.grid_col_span = span;
        }
        "grid-row" | "grid-row-start" => {
            let (start, span) = parse_grid_placement(v);
            style.grid_row_start = start;
            style.grid_row_span = span;
        }
        "grid-column-end" => {
            // `grid-column-end: span N` widens an item placed by its start; a bare
            // end line is honoured only when a start is already known.
            let (_, span) = parse_grid_placement(&format!("{} / {v}", style.grid_col_start));
            style.grid_col_span = span;
        }
        "grid-row-end" => {
            let (_, span) = parse_grid_placement(&format!("{} / {v}", style.grid_row_start));
            style.grid_row_span = span;
        }
        "grid-area" => {
            // A single identifier names a `grid-template-areas` area (resolved at
            // placement against the parent grid); otherwise it is the numeric line
            // form below.
            let t = v.trim();
            let is_name = !t.contains('/')
                && !t.eq_ignore_ascii_case("auto")
                && !t.starts_with("span")
                && t.parse::<usize>().is_err()
                && t.chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
            if is_name {
                style.grid_area_name = Some(t.to_string());
                return;
            }
            // `grid-area: <row-start> / <col-start> [/ <row-end> / <col-end>]`.
            // Resolve start lines from the first two components, and a span from
            // an end component when it is a `span N` or a numeric end line.
            let parts: Vec<&str> = v.split('/').map(str::trim).collect();
            if let Some(rs) = parts.first() {
                let (s, sp) = parse_grid_placement(rs);
                style.grid_row_start = s;
                style.grid_row_span = sp;
            }
            if let Some(cs) = parts.get(1) {
                let (s, sp) = parse_grid_placement(cs);
                style.grid_col_start = s;
                style.grid_col_span = sp;
            }
            if let Some(re) = parts.get(2) {
                let (_, sp) = parse_grid_placement(&format!("{} / {re}", style.grid_row_start));
                style.grid_row_span = sp;
            }
            if let Some(ce) = parts.get(3) {
                let (_, sp) = parse_grid_placement(&format!("{} / {ce}", style.grid_col_start));
                style.grid_col_span = sp;
            }
        }
        "flex-wrap" => {
            style.flex_wrap = v == "wrap" || v == "wrap-reverse";
        }
        "flex-flow" => {
            // `flex-flow: <direction> || <wrap>` shorthand.
            for tok in v.split_whitespace() {
                match tok {
                    "column" => style.flex_column = true,
                    "column-reverse" => {
                        style.flex_column = true;
                        style.flex_reverse = true;
                    }
                    "row" => style.flex_column = false,
                    "row-reverse" => {
                        style.flex_column = false;
                        style.flex_reverse = true;
                    }
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
                // `sticky` keeps its own scheme now: in-flow like relative, but
                // its `inset` shift is clamped to the containing block at layout.
                "sticky" => Position::Sticky,
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
        "writing-mode" => {
            style.writing_mode = match v {
                "vertical-rl" | "sideways-rl" => WritingMode::VerticalRl,
                "vertical-lr" | "sideways-lr" => WritingMode::VerticalLr,
                _ => WritingMode::HorizontalTb,
            };
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
        "clear" => {
            // `clear: left|right|both` drops the block below the chosen side's
            // preceding floats before it is placed. `none` leaves it untouched.
            style.clear = match v {
                "left" => Clear::Left,
                "right" => Clear::Right,
                "both" => Clear::Both,
                _ => Clear::None,
            };
        }
        "color" => {
            if let Some((c, a)) = parse_color_alpha(v) {
                style.color = c;
                style.color_alpha = a;
            }
        }
        "background" | "background-color" | "background-image" => {
            // A `linear-`/`radial-`/`conic-gradient(...)` (with or without the
            // `background-image` longhand) is captured as a gradient that paints
            // over the box; the solid `background` is left as-is so a
            // `background-color` fallback declared alongside still shows where the
            // gradient can't. Otherwise the value is a flat colour (first token of
            // the `background` shorthand); `background-image` never sets a colour.
            if let Some(g) = parse_css_gradient(v.trim(), style.font_size) {
                style.background_gradient = Some(g);
            } else if prop != "background-image" {
                // Try the whole value first so a function colour with internal
                // spaces (`rgba(0, 0, 0, .4)`, `hsl(0 100% 50%)`) parses; fall back
                // to the first token for the `background` shorthand (`red url(…)`).
                let candidate = parse_color_alpha(v.trim())
                    .or_else(|| parse_color_alpha(v.split_whitespace().next().unwrap_or(v)));
                match candidate {
                    Some((c, a)) => {
                        style.background = Some(c);
                        style.background_alpha = a;
                    }
                    None => style.background = None,
                }
            }
            // `url(...)` raster source. `background-color` never carries one; the
            // `background` shorthand and `background-image` set (or, with no url,
            // reset) it.
            if prop != "background-color" {
                style.background_image = extract_css_url(v);
            }
        }
        "font-size" => {
            if let Some(px) = parse_len_px(v, style.font_size) {
                style.font_size = px;
            }
        }
        "font-weight" => apply_font_weight(style, v),
        "font-style" => style.italic = matches!(v, "italic" | "oblique"),
        "font" => apply_font_shorthand(style, v),
        "font-family" => apply_font_family(style, v),
        "text-align" => {
            style.align = match v {
                "center" => Align::Center,
                "right" => Align::Right,
                "justify" => Align::Justify,
                // Direction-relative: kept as `Start`/`End` and resolved against
                // `direction` at layout time (so `start` follows `rtl`).
                "start" => Align::Start,
                "end" => Align::End,
                _ => Align::Left,
            }
        }
        // `direction: ltr|rtl`. Unknown values leave the (inherited) direction
        // untouched rather than forcing `ltr`, so a typo never silently flips a
        // child back to LTR inside an RTL ancestor.
        "direction" => match v {
            "rtl" => style.direction = Direction::Rtl,
            "ltr" => style.direction = Direction::Ltr,
            _ => {}
        },
        "text-decoration" | "text-decoration-line" => {
            style.underline = v.contains("underline");
            style.strike = v.contains("line-through");
            style.overline = v.contains("overline");
        }
        "visibility" => style.hidden = v == "hidden" || v == "collapse",
        "opacity" => {
            // Recorded as the element's OWN opacity; `Stylesheet::computed`
            // composes it multiplicatively with the inherited (ancestor)
            // opacity once per element — so nested `opacity: 0.5` boxes
            // multiply (0.25 effective) instead of the child overwriting the
            // parent. A `%` value is accepted too (`50%` ⇒ 0.5).
            let parsed = v
                .strip_suffix('%')
                .and_then(|n| n.trim().parse::<f64>().ok().map(|p| p / 100.0))
                .or_else(|| v.parse::<f64>().ok());
            if let Some(o) = parsed {
                style.own_opacity = Some(o.clamp(0.0, 1.0));
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
        "min-height" => {
            style.min_height = parse_len_px(v, style.font_size);
        }
        "max-height" => {
            // `none` (the initial value) clears the clamp; a length caps the
            // used box height (content overflows, visible by default).
            style.max_height = if v == "none" {
                None
            } else {
                parse_len_px(v, style.font_size)
            };
        }
        "height" => {
            // Definite height: caps the box (content overflows) rather than just
            // flooring it like `min-height`.
            style.height = parse_len_px(v, style.font_size);
        }
        "aspect-ratio" => style.aspect_ratio = parse_aspect_ratio(v),
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
        "white-space" => {
            style.white_space = match v {
                "nowrap" => WhiteSpace::Nowrap,
                "pre" => WhiteSpace::Pre,
                // `break-spaces` preserves whitespace and wraps, like pre-wrap.
                "pre-wrap" | "break-spaces" => WhiteSpace::PreWrap,
                "pre-line" => WhiteSpace::PreLine,
                _ => WhiteSpace::Normal,
            };
            // Legacy flag, kept in lock-step: `pre`/`pre-wrap` preserve
            // whitespace (`pre-line` collapses spaces, so it is NOT `pre`).
            style.pre = matches!(style.white_space, WhiteSpace::Pre | WhiteSpace::PreWrap);
        }
        "word-break" => {
            // `break-all` (and the legacy `break-word`) allow mid-word breaks;
            // `normal`/`keep-all` do not.
            style.break_word = matches!(v, "break-all" | "break-word");
        }
        // `word-wrap` is the legacy alias of `overflow-wrap`.
        "overflow-wrap" | "word-wrap" => {
            style.break_word = matches!(v, "break-word" | "anywhere");
        }
        "text-overflow" => {
            style.text_overflow_ellipsis = v.contains("ellipsis");
        }
        "object-fit" => {
            style.object_fit = match v {
                "contain" => ObjectFit::Contain,
                "cover" => ObjectFit::Cover,
                "none" => ObjectFit::None,
                "scale-down" => ObjectFit::ScaleDown,
                _ => ObjectFit::Fill,
            };
        }
        "counter-reset" => style.counter_reset = parse_counter_pairs(v, 0),
        "counter-increment" => style.counter_increment = parse_counter_pairs(v, 1),
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
            let (w, c, vis, s) = parse_border_shorthand(v, style.font_size, style.color);
            style.border_width = Edges::all(if vis { w } else { 0.0 });
            if let Some((c, a)) = c {
                style.border_color = c;
                style.border_color_edges = [c; 4];
                style.border_color_alpha = a;
            }
            // A `solid` keyword (or none at all) restores the default; the
            // shorthand resets every side's style, matching CSS reset semantics.
            style.border_style_edges = [s.unwrap_or(BorderStyle::Solid); 4];
        }
        "border-top" | "border-right" | "border-bottom" | "border-left" => {
            let (w, c, vis, s) = parse_border_shorthand(v, style.font_size, style.color);
            let i = border_side_index(prop);
            if let Some((_, a)) = c {
                style.border_color_alpha = a;
            }
            set_border_side(
                style,
                i,
                if vis { Some(w) } else { Some(0.0) },
                c.map(|(rgb, _)| rgb),
            );
            // The side shorthand also resets THIS side's style (to the given
            // keyword, or `solid` when none was written).
            style.border_style_edges[i] = s.unwrap_or(BorderStyle::Solid);
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
            if let Some((c, a)) = parse_color_alpha(v) {
                set_border_side(style, border_side_index(prop), None, Some(c));
                style.border_color_alpha = a;
            }
        }
        // `border-style` longhands set the per-side line style. `none`/`hidden`
        // still suppress the rule (zero width) even when a width is also
        // declared; any other keyword sets the side's `BorderStyle`.
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
            } else if prop == "border-style" {
                // 1–4 keywords, TRBL fill rules (like the box shorthands).
                apply_border_style_shorthand(style, v);
            } else if let Some(s) = parse_border_style_keyword(v.trim()) {
                style.border_style_edges[border_side_index(prop)] = s;
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
            // `top`/`middle`/`bottom` (and their `text-*` cousins) shift the run
            // within its line box — the offset is resolved at line assembly, when
            // the line height is known.
            style.valign = match v {
                "super" => VShift::Super,
                "sub" => VShift::Sub,
                "top" | "text-top" => VShift::Top,
                "middle" => VShift::Middle,
                "bottom" | "text-bottom" => VShift::Bottom,
                "baseline" => VShift::Baseline,
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
        "border-radius" => {
            let (h, vert) = parse_border_radius(v, style.font_size);
            style.border_radius = h;
            style.border_radius_v = vert;
        }
        "border-top-left-radius" => set_corner_radius(style, 0, v),
        "border-top-right-radius" => set_corner_radius(style, 1, v),
        "border-bottom-right-radius" => set_corner_radius(style, 2, v),
        "border-bottom-left-radius" => set_corner_radius(style, 3, v),
        "box-shadow" => {
            let mut layers = parse_box_shadows(v, style.font_size);
            // First layer is the topmost (single-layer path unchanged); the rest
            // paint behind it. `none`/unparseable ⇒ no shadow at all.
            style.box_shadow = if layers.is_empty() {
                None
            } else {
                Some(layers.remove(0))
            };
            style.box_shadow_extra = layers;
        }
        "text-shadow" => style.text_shadows = parse_text_shadows(v, style.font_size, style.color),
        "transform" => style.transform = parse_transform(v, style.font_size),
        "transform-origin" => {
            style.transform_origin = parse_transform_origin(v, style.font_size);
        }
        "caption-side" => {
            style.caption_side = match v {
                "bottom" => CaptionSide::Bottom,
                _ => CaptionSide::Top,
            };
        }
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
/// `(width_pt, colour?, visible, style?)`. Width defaults to a hairline (1pt)
/// when a style/colour is given without a length (matching the CSS `medium`
/// initial width pragmatically); `none`/`hidden` set `visible = false`. The
/// `style?` is the recognised line-style keyword (`solid`/`dashed`/`dotted`/
/// `double`) when one appears, so `border: 1px dashed red` carries its dash.
#[allow(clippy::type_complexity)] // (width, Option<(rgb, alpha)>, visible, style)
fn parse_border_shorthand(
    v: &str,
    em: f64,
    current: [f64; 3],
) -> (f64, Option<([f64; 3], f64)>, bool, Option<BorderStyle>) {
    let mut w = 1.0;
    let mut got_w = false;
    let mut color: Option<([f64; 3], f64)> = None;
    let mut visible = true;
    let mut style = None;
    let collapsed = collapse_paren_spaces(v);
    for tok in collapsed.split_whitespace() {
        if let Some(px) = parse_len_px(tok, em) {
            w = px;
            got_w = true;
        } else if matches!(tok, "none" | "hidden") {
            visible = false;
        } else if let Some(s) = parse_border_style_keyword(tok) {
            style = Some(s);
        } else if tok.eq_ignore_ascii_case("currentcolor") {
            color = Some((current, 1.0));
        } else if let Some(ca) = parse_color_alpha(tok) {
            color = Some(ca);
        }
    }
    // `border: 0` (explicit zero length) keeps width 0 even though `visible`.
    if got_w && w <= 0.0 {
        visible = false;
    }
    (w, color, visible, style)
}

/// Map a `border-style` keyword to a [`BorderStyle`]. Only the styles we render
/// distinctly map to a non-`Solid` value; every other recognised keyword
/// (`groove`/`ridge`/`inset`/`outset`/`solid`) renders solid, and an unknown
/// token returns `None` (not a style keyword at all). `none`/`hidden` are
/// handled by the width path, not here.
fn parse_border_style_keyword(tok: &str) -> Option<BorderStyle> {
    match tok {
        "dashed" => Some(BorderStyle::Dashed),
        "dotted" => Some(BorderStyle::Dotted),
        "double" => Some(BorderStyle::Double),
        "inset" => Some(BorderStyle::Inset),
        "outset" => Some(BorderStyle::Outset),
        "groove" => Some(BorderStyle::Groove),
        "ridge" => Some(BorderStyle::Ridge),
        "solid" => Some(BorderStyle::Solid),
        _ => None,
    }
}

/// `border-style` shorthand: 1–4 keywords applied TRBL with the CSS shorthand
/// fill rules. Unknown keywords fall back to `Solid`. A `none`/`hidden` value is
/// handled by the caller (it zeroes the width); this only sets line styles.
fn apply_border_style_shorthand(style: &mut Style, v: &str) {
    let kinds: Vec<BorderStyle> = v
        .split_whitespace()
        .map(|t| parse_border_style_keyword(t).unwrap_or(BorderStyle::Solid))
        .collect();
    style.border_style_edges = match kinds.as_slice() {
        [a] => [*a, *a, *a, *a],
        [a, b] => [*a, *b, *a, *b],
        [a, b, c] => [*a, *b, *c, *b],
        [a, b, c, d] => [*a, *b, *c, *d],
        _ => return,
    };
}

/// Parse the `border-radius` shorthand into `(horizontal, vertical)` corner
/// radii, each `[top-left, top-right, bottom-right, bottom-left]` (CSS clockwise
/// order), in points.
///
/// Supports the 1–4 value forms (`r` / `tl-tr-bl tr-bl` … per CSS) **and** the
/// elliptical `a b c d / e f g h` syntax: the part before `/` fills the
/// horizontal radii and the part after fills the vertical radii (each with the
/// same 1–4 fill rules). With no `/` the box is circular and the vertical radii
/// equal the horizontal ones, so a plain `border-radius: 8pt` is byte-identical
/// to before. Percentages resolve against the font-size here (no box size is
/// available at parse time); unparseable tokens fall back to `0`, never panicking.
fn parse_border_radius(v: &str, em: f64) -> ([f64; 4], [f64; 4]) {
    let mut it = v.splitn(2, '/');
    let h_part = it.next().unwrap_or(v);
    let h = fill_radius_quad(h_part, em);
    // Elliptical form: a `/ v…` part overrides the vertical radii; otherwise the
    // vertical radii mirror the horizontal ones (circular corners).
    let vert = match it.next() {
        Some(v_part) if !v_part.trim().is_empty() => fill_radius_quad(v_part, em),
        _ => h,
    };
    (h, vert)
}

/// Expand a 1–4 length list into a `[TL, TR, BR, BL]` quad with the CSS
/// `border-radius` fill rules. Unparseable/empty ⇒ all zeros.
fn fill_radius_quad(part: &str, em: f64) -> [f64; 4] {
    let vals: Vec<f64> = part
        .split_whitespace()
        .map(|t| parse_len_px(t, em).unwrap_or(0.0).max(0.0))
        .collect();
    match vals.as_slice() {
        [a] => [*a, *a, *a, *a],
        [a, b] => [*a, *b, *a, *b],
        [a, b, c] => [*a, *b, *c, *b],
        [a, b, c, d] => [*a, *b, *c, *d],
        _ => [0.0; 4],
    }
}

/// Parse a single corner-radius longhand (e.g. `border-top-left-radius`) into its
/// `(horizontal, vertical)` radii. A lone value is circular (`h == v`); an `h v`
/// pair gives an elliptical corner. `(0, 0)` on failure.
fn parse_corner_radius(v: &str, em: f64) -> (f64, f64) {
    let mut it = v.split_whitespace();
    let h = it
        .next()
        .and_then(|t| parse_len_px(t, em))
        .unwrap_or(0.0)
        .max(0.0);
    let vert = it
        .next()
        .and_then(|t| parse_len_px(t, em))
        .unwrap_or(h)
        .max(0.0);
    (h, vert)
}

/// Apply a corner-radius longhand to corner `i` (`0=TL,1=TR,2=BR,3=BL`), setting
/// both the horizontal and vertical radii for that corner.
fn set_corner_radius(style: &mut Style, i: usize, v: &str) {
    let (h, vert) = parse_corner_radius(v, style.font_size);
    style.border_radius[i] = h;
    style.border_radius_v[i] = vert;
}

/// Parse a `box-shadow` value into all its layers, in source order (first layer
/// = topmost). `none` (or a value with no usable layer) yields an empty vec.
///
/// Comma-separated layers are split at the **top level** (commas inside an
/// `rgb()/rgba()/hsl()` colour stay with their layer), each then parsed by
/// [`parse_box_shadow_layer`]. A layer that doesn't carry the two required
/// offsets is dropped rather than mis-placed.
fn parse_box_shadows(v: &str, em: f64) -> Vec<BoxShadow> {
    let v = resolve_var(v);
    if v.trim().eq_ignore_ascii_case("none") || v.trim().is_empty() {
        return Vec::new();
    }
    split_top_level_commas(&v)
        .iter()
        .filter_map(|layer| parse_box_shadow_layer(layer, em))
        .collect()
}

/// Parse one `box-shadow` layer: `[inset] <dx> <dy> [blur] [spread] [color]` with
/// the `inset` keyword/colour in any order and the lengths in canonical order. A
/// missing colour defaults to black (the paint layer dims by `blur`). `None` if
/// fewer than the two offset lengths are present.
fn parse_box_shadow_layer(layer: &str, em: f64) -> Option<BoxShadow> {
    let layer = layer.trim();
    if layer.is_empty() || layer.eq_ignore_ascii_case("none") {
        return None;
    }
    let mut inset = false;
    let mut color: Option<[f64; 3]> = None;
    let mut lengths: Vec<f64> = Vec::new();
    for tok in layer.split_whitespace() {
        if tok.eq_ignore_ascii_case("inset") {
            inset = true;
        } else if let Some(px) = parse_len_px(tok, em) {
            lengths.push(px);
        } else if let Some(c) = parse_color(tok) {
            color = Some(c);
        }
        // unknown keywords are skipped (tolerant parsing)
    }
    // Need at least the two offsets to place a shadow.
    if lengths.len() < 2 {
        return None;
    }
    let dx = lengths[0];
    let dy = lengths[1];
    let blur = lengths.get(2).copied().unwrap_or(0.0).max(0.0);
    let spread = lengths.get(3).copied().unwrap_or(0.0);
    Some(BoxShadow {
        dx,
        dy,
        blur,
        spread,
        color: color.unwrap_or([0.0, 0.0, 0.0]),
        inset,
    })
}

/// Parse `text-shadow: <dx> <dy> [blur] [color]` layers (comma-separated, first =
/// topmost). The colour may come first or last; a missing colour falls back to
/// `current` (the element's own text colour). `none`/empty ⇒ no shadows.
fn parse_text_shadows(v: &str, em: f64, current: [f64; 3]) -> Vec<TextShadow> {
    let v = resolve_var(v);
    if v.trim().eq_ignore_ascii_case("none") || v.trim().is_empty() {
        return Vec::new();
    }
    split_top_level_commas(&v)
        .iter()
        .filter_map(|layer| parse_text_shadow_layer(layer, em, current))
        .collect()
}

/// One `text-shadow` layer: needs the two offset lengths; an optional third is
/// the blur radius. Colour (with alpha) may sit before or after the lengths.
fn parse_text_shadow_layer(layer: &str, em: f64, current: [f64; 3]) -> Option<TextShadow> {
    let layer = layer.trim();
    if layer.is_empty() || layer.eq_ignore_ascii_case("none") {
        return None;
    }
    let mut color: Option<[f64; 3]> = None;
    let mut alpha = 1.0;
    let mut lengths: Vec<f64> = Vec::new();
    // Collapse spaces inside parens so `rgba(0, 0, 0, .5)` survives tokenising.
    for tok in collapse_paren_spaces(layer).split_whitespace() {
        if let Some(px) = parse_len_px(tok, em) {
            lengths.push(px);
        } else if let Some((c, a)) = parse_color_alpha(tok) {
            color = Some(c);
            alpha = a;
        }
    }
    if lengths.len() < 2 {
        return None;
    }
    Some(TextShadow {
        dx: lengths[0],
        dy: lengths[1],
        blur: lengths.get(2).copied().unwrap_or(0.0).max(0.0),
        color: color.unwrap_or(current),
        alpha,
    })
}

/// Multiply two affine matrices `[a, b, c, d, e, f]` (a point `(x, y)` maps to
/// `(a·x + c·y + e, b·x + d·y + f)`): returns `m1 · m2` (apply `m2` first).
fn mat_mul(m1: [f64; 6], m2: [f64; 6]) -> [f64; 6] {
    let [a1, b1, c1, d1, e1, f1] = m1;
    let [a2, b2, c2, d2, e2, f2] = m2;
    [
        a1 * a2 + c1 * b2,
        b1 * a2 + d1 * b2,
        a1 * c2 + c1 * d2,
        b1 * c2 + d1 * d2,
        a1 * e2 + c1 * f2 + e1,
        b1 * e2 + d1 * f2 + f1,
    ]
}

/// Parse `transform: <function>+` into the composed affine matrix (CSS user
/// units, top-down, before the transform-origin shift). Functions compose
/// left-to-right (the leftmost is applied last to a point). `None` for
/// `none`/empty/unparsable.
fn parse_transform(v: &str, em: f64) -> Option<[f64; 6]> {
    let v = resolve_var(v);
    let t = v.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("none") {
        return None;
    }
    let mut m = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0]; // identity
    let mut any = false;
    let mut rest = t;
    while let Some(open) = rest.find('(') {
        let name = rest[..open]
            .trim()
            .trim_start_matches(|c: char| !c.is_ascii_alphabetic())
            .to_ascii_lowercase();
        let Some(close) = rest[open + 1..].find(')') else {
            break;
        };
        let args = &rest[open + 1..open + 1 + close];
        if let Some(fm) = transform_function(&name, args, em) {
            m = mat_mul(m, fm);
            any = true;
        }
        rest = &rest[open + 1 + close + 1..];
    }
    any.then_some(m)
}

/// Build the affine matrix for one `transform` function (`translate`, `rotate`,
/// `scale`, `skew`, `matrix`, and their axis variants). `None` if its arguments
/// don't parse.
fn transform_function(name: &str, args: &str, em: f64) -> Option<[f64; 6]> {
    let nums: Vec<&str> = args
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let len = |s: &str| parse_len_px(s, em);
    let num = |s: &str| s.parse::<f64>().ok();
    let rad = |s: &str| parse_gradient_angle(s).map(f64::to_radians);
    match name {
        "translate" => {
            let tx = len(nums.first()?)?;
            let ty = nums.get(1).and_then(|s| len(s)).unwrap_or(0.0);
            Some([1.0, 0.0, 0.0, 1.0, tx, ty])
        }
        "translatex" => Some([1.0, 0.0, 0.0, 1.0, len(nums.first()?)?, 0.0]),
        "translatey" => Some([1.0, 0.0, 0.0, 1.0, 0.0, len(nums.first()?)?]),
        "scale" => {
            let sx = num(nums.first()?)?;
            let sy = nums.get(1).and_then(|s| num(s)).unwrap_or(sx);
            Some([sx, 0.0, 0.0, sy, 0.0, 0.0])
        }
        "scalex" => Some([num(nums.first()?)?, 0.0, 0.0, 1.0, 0.0, 0.0]),
        "scaley" => Some([1.0, 0.0, 0.0, num(nums.first()?)?, 0.0, 0.0]),
        "rotate" => {
            let r = rad(nums.first()?)?;
            Some([r.cos(), r.sin(), -r.sin(), r.cos(), 0.0, 0.0])
        }
        "skew" => {
            let ax = rad(nums.first()?)?;
            let ay = nums.get(1).and_then(|s| rad(s)).unwrap_or(0.0);
            Some([1.0, ay.tan(), ax.tan(), 1.0, 0.0, 0.0])
        }
        "skewx" => Some([1.0, 0.0, rad(nums.first()?)?.tan(), 1.0, 0.0, 0.0]),
        "skewy" => Some([1.0, rad(nums.first()?)?.tan(), 0.0, 1.0, 0.0, 0.0]),
        "matrix" => {
            if nums.len() < 6 {
                return None;
            }
            let p: Vec<f64> = nums.iter().take(6).map(|s| num(s)).collect::<Option<_>>()?;
            Some([p[0], p[1], p[2], p[3], p[4], p[5]])
        }
        _ => None,
    }
}

/// Parse a `transform-origin` value into per-axis lengths: keywords map to
/// percentages (`left`/`top` → 0%, `center` → 50%, `right`/`bottom` → 100%),
/// lengths resolve to points from the box's top-left, `%` stays relative to
/// the border-box dimension. One value sets `x` (y defaults to `center`); the
/// two-value form accepts the keywords in either order (`top left` ≡ `left
/// top`), while plain lengths are positional `x y`. `None` (initial value /
/// unparseable) keeps the box-centre pivot.
fn parse_transform_origin(v: &str, em: f64) -> Option<(Len, Len)> {
    // A component along one axis. `swapped` marks a keyword that names the
    // OTHER axis (`top`/`bottom` in x position), so the pair can be reordered.
    fn component(tok: &str, em: f64) -> Option<(Len, bool)> {
        match tok {
            "left" => Some((Len::Percent(0.0), false)),
            "right" => Some((Len::Percent(100.0), false)),
            "top" => Some((Len::Percent(0.0), true)),
            "bottom" => Some((Len::Percent(100.0), true)),
            "center" | "centre" => Some((Len::Percent(50.0), false)),
            t => {
                if let Some(p) = t.strip_suffix('%').and_then(|n| n.trim().parse::<f64>().ok()) {
                    Some((Len::Percent(p), false))
                } else {
                    parse_len_px(t, em).map(|pt| (Len::Pt(pt), false))
                }
            }
        }
    }
    let toks: Vec<&str> = v.split_whitespace().collect();
    match toks.as_slice() {
        [x] => {
            let (cx, vertical) = component(x, em)?;
            // A lone `top`/`bottom` names the Y axis; X defaults to center.
            Some(if vertical {
                (Len::Percent(50.0), cx)
            } else {
                (cx, Len::Percent(50.0))
            })
        }
        [a, b, ..] => {
            let (ca, a_vert) = component(a, em)?;
            let (cb, _) = component(b, em)?;
            // Keyword axes may arrive in either order (`top left` ≡ `left
            // top`): a leading `top`/`bottom` — or a trailing `left`/`right` —
            // means the pair is (y, x). Positional lengths stay `x y`.
            let swapped = a_vert || matches!(*b, "left" | "right");
            Some(if swapped { (cb, ca) } else { (ca, cb) })
        }
        _ => None,
    }
}

/// Fold a transform-origin `(ox, oy)` into a raw transform matrix:
/// `T(ox, oy) · raw · T(-ox, -oy)`, so the transform pivots about `(ox, oy)`
/// (CSS user space). The paint layer then applies the page Y-flip.
pub(crate) fn transform_about_origin(raw: [f64; 6], ox: f64, oy: f64) -> [f64; 6] {
    let pos = [1.0, 0.0, 0.0, 1.0, ox, oy];
    let neg = [1.0, 0.0, 0.0, 1.0, -ox, -oy];
    mat_mul(mat_mul(pos, raw), neg)
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
    let collapsed = collapse_paren_spaces(v);
    let parsed: Vec<([f64; 3], f64)> = collapsed
        .split_whitespace()
        .filter_map(parse_color_alpha)
        .collect();
    let cols: Vec<[f64; 3]> = parsed.iter().map(|(c, _)| *c).collect();
    let edges = match cols.as_slice() {
        [a] => [*a, *a, *a, *a],
        [a, b] => [*a, *b, *a, *b],
        [a, b, c] => [*a, *b, *c, *b],
        [a, b, c, d] => [*a, *b, *c, *d],
        _ => return,
    };
    style.border_color_edges = edges;
    style.border_color = edges[0];
    // The border emit applies one opacity to every side, so take the first
    // colour's alpha as the uniform border alpha.
    if let Some((_, a)) = parsed.first() {
        style.border_color_alpha = *a;
    }
}

/// Parse `aspect-ratio` to a `width / height` factor: `16 / 9` → `1.777…`, a bare
/// `1.5` → `1.5`. The `auto` keyword (e.g. `auto 16/9`) is ignored — the ratio is
/// still used. `None` when no positive ratio is present.
fn parse_aspect_ratio(v: &str) -> Option<f64> {
    let cleaned = v.to_ascii_lowercase().replace("auto", " ");
    let nums: Vec<f64> = cleaned
        .split('/')
        .flat_map(|s| s.split_whitespace())
        .filter_map(|t| t.parse::<f64>().ok())
        .collect();
    match nums.as_slice() {
        [w] if *w > 0.0 => Some(*w),
        [w, h] if *w > 0.0 && *h > 0.0 => Some(w / h),
        _ => None,
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

std::thread_local! {
    /// The live viewport (page) size in points that `vw`/`vh` resolve against.
    /// Layout entry points install the real page geometry for the duration of a
    /// layout via [`set_viewport_for_layout`]; outside a layout the reference
    /// US-Letter viewport applies (the historical behaviour). Thread-local so
    /// concurrent renders on different threads never observe each other.
    static VIEWPORT_PT: std::cell::Cell<(f64, f64)> =
        const { std::cell::Cell::new((VIEWPORT_W_PT, VIEWPORT_H_PT)) };
}

/// Scope guard restoring the previous `vw`/`vh` viewport on drop.
pub(crate) struct ViewportGuard {
    prev: (f64, f64),
}

impl Drop for ViewportGuard {
    fn drop(&mut self) {
        VIEWPORT_PT.with(|c| c.set(self.prev));
    }
}

/// Install the real page size (points) for `vw`/`vh` resolution during a
/// layout; the returned guard restores the previous viewport when dropped.
/// Non-finite, non-positive, or absurdly large dimensions (the synthetic
/// "endless page" used to measure header/footer bands) are ignored per axis,
/// keeping the reference viewport for that axis.
pub(crate) fn set_viewport_for_layout(w: f64, h: f64) -> ViewportGuard {
    let sane = |v: f64, fallback: f64| {
        if v.is_finite() && v > 0.0 && v < 1.0e5 {
            v
        } else {
            fallback
        }
    };
    let prev = VIEWPORT_PT.with(|c| c.get());
    VIEWPORT_PT.with(|c| c.set((sane(w, prev.0), sane(h, prev.1))));
    ViewportGuard { prev }
}

/// Maximum `var()` substitution depth. A dependency chain deeper than this — in
/// practice a reference **cycle** (`--a: var(--b); --b: var(--a)`) — makes the
/// reference unresolvable, so the `var()` falls back (or invalidates the
/// declaration), per the spec's cycle handling.
const MAX_VAR_DEPTH: usize = 8;

/// Substitute every `var(--name[, fallback])` in `v` against the custom
/// properties in scope. Substituted values are themselves substituted (nested
/// references), bounded by [`MAX_VAR_DEPTH`] as the cycle guard. Returns `None`
/// when any reference is unresolvable — unknown custom property with no
/// fallback, a cycle, or a malformed `var(` — which invalidates the whole
/// declaration at computed-value time.
fn substitute_vars(
    v: &str,
    props: &std::collections::BTreeMap<String, String>,
    depth: usize,
) -> Option<String> {
    if depth > MAX_VAR_DEPTH {
        return None; // cycle (or absurd nesting) — unresolvable
    }
    if !v.contains("var(") {
        return Some(v.to_string());
    }
    let mut out = String::with_capacity(v.len());
    let mut rest = v;
    while let Some(at) = rest.find("var(") {
        // A `var(` must be a function head, not the tail of an identifier
        // (e.g. `navar(`); require a non-ident char before it.
        let is_head = at == 0
            || !rest[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !is_head {
            out.push_str(&rest[..at + 4]);
            rest = &rest[at + 4..];
            continue;
        }
        out.push_str(&rest[..at]);
        let after = &rest[at + 4..];
        // Find the matching `)` (the fallback may itself contain functions).
        let mut close = None;
        let mut par = 1usize;
        for (i, c) in after.char_indices() {
            match c {
                '(' => par += 1,
                ')' => {
                    par -= 1;
                    if par == 0 {
                        close = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let close = close?; // unterminated `var(` — invalid
        let inner = &after[..close];
        // Split `--name` from the optional fallback at the first TOP-LEVEL comma.
        let mut split = None;
        let mut par2 = 0usize;
        for (i, c) in inner.char_indices() {
            match c {
                '(' => par2 += 1,
                ')' => par2 = par2.saturating_sub(1),
                ',' if par2 == 0 => {
                    split = Some(i);
                    break;
                }
                _ => {}
            }
        }
        let (name, fallback) = match split {
            Some(i) => (inner[..i].trim(), Some(inner[i + 1..].trim())),
            None => (inner.trim(), None),
        };
        let replacement = match props.get(name) {
            // A known custom property: substitute its (recursively resolved)
            // value; if ITS references are unresolvable, fall back.
            Some(val) => match substitute_vars(val, props, depth + 1) {
                Some(s) => s,
                None => substitute_vars(fallback?, props, depth + 1)?,
            },
            None => substitute_vars(fallback?, props, depth + 1)?,
        };
        out.push_str(replacement.trim());
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    Some(out)
}

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

/// A `calc()` value mid-evaluation. Lengths reduce to a **linear form**
/// `pt + pct%·base` (absolute units fold into `pt` immediately; the percent
/// part stays symbolic until the caller supplies its base), and bare numbers
/// stay dimensionless so they can multiply/divide a length.
#[derive(Debug, Clone, Copy)]
enum CalcV {
    /// A dimensionless number — only meaningful as a `*`/`/` operand.
    Num(f64),
    /// `pt + (pct/100)·base` — a length with an unresolved percent part.
    Lin { pt: f64, pct: f64 },
}

/// Recursive-descent `calc()` expression parser: full operator chaining with
/// `*`/`/` binding tighter than `+`/`-`, parentheses, nested `calc()`, unary
/// signs, and mixed `%`/absolute operands kept in linear form. Invalid algebra
/// — `length × length`, division **by** a length, `number + length`, division
/// by zero — yields `None` so the declaration is ignored (CSS error handling).
struct CalcParser<'a> {
    s: &'a [u8],
    i: usize,
    em: f64,
}

impl<'a> CalcParser<'a> {
    fn skip_ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    /// `sum := prod (('+' | '-') prod)*`
    fn sum(&mut self) -> Option<CalcV> {
        let mut acc = self.prod()?;
        loop {
            self.skip_ws();
            let sign = match self.peek() {
                Some(b'+') => 1.0,
                Some(b'-') => -1.0,
                _ => return Some(acc),
            };
            self.i += 1;
            let rhs = self.prod()?;
            acc = match (acc, rhs) {
                (CalcV::Num(a), CalcV::Num(b)) => CalcV::Num(a + sign * b),
                (CalcV::Lin { pt: ap, pct: ac }, CalcV::Lin { pt: bp, pct: bc }) => CalcV::Lin {
                    pt: ap + sign * bp,
                    pct: ac + sign * bc,
                },
                // `number ± length` has no CSS meaning — the whole calc is invalid.
                _ => return None,
            };
        }
    }

    /// `prod := atom (('*' | '/') atom)*`
    fn prod(&mut self) -> Option<CalcV> {
        let mut acc = self.atom()?;
        loop {
            self.skip_ws();
            let mul = match self.peek() {
                Some(b'*') => true,
                Some(b'/') => false,
                _ => return Some(acc),
            };
            self.i += 1;
            let rhs = self.atom()?;
            acc = if mul {
                match (acc, rhs) {
                    (CalcV::Num(a), CalcV::Num(b)) => CalcV::Num(a * b),
                    (CalcV::Num(k), CalcV::Lin { pt, pct })
                    | (CalcV::Lin { pt, pct }, CalcV::Num(k)) => CalcV::Lin {
                        pt: pt * k,
                        pct: pct * k,
                    },
                    // `length × length` would be an area — invalid in CSS.
                    _ => return None,
                }
            } else {
                match rhs {
                    // Division by a length (or by zero) is invalid.
                    CalcV::Num(k) if k != 0.0 => match acc {
                        CalcV::Num(a) => CalcV::Num(a / k),
                        CalcV::Lin { pt, pct } => CalcV::Lin {
                            pt: pt / k,
                            pct: pct / k,
                        },
                    },
                    _ => return None,
                }
            };
        }
    }

    /// `atom := ['+'|'-'] ( '(' sum ')' | 'calc(' sum ')' | value )`
    fn atom(&mut self) -> Option<CalcV> {
        self.skip_ws();
        // Unary sign (covers `-5px`, `2 * -3px`, `-(1px + 2px)`).
        let mut sign = 1.0;
        while let Some(c @ (b'+' | b'-')) = self.peek() {
            if c == b'-' {
                sign = -sign;
            }
            self.i += 1;
            self.skip_ws();
        }
        let v = match self.peek()? {
            b'(' => self.group()?,
            b'c' | b'C' => {
                // Nested `calc( … )` — the inner expression evaluates in place.
                let rest = &self.s[self.i..];
                if rest.len() >= 5 && rest[..5].eq_ignore_ascii_case(b"calc(") {
                    self.i += 4; // leave the '(' for `group`
                    self.group()?
                } else {
                    return None;
                }
            }
            _ => self.value()?,
        };
        Some(match v {
            CalcV::Num(n) => CalcV::Num(sign * n),
            CalcV::Lin { pt, pct } => CalcV::Lin {
                pt: sign * pt,
                pct: sign * pct,
            },
        })
    }

    /// A parenthesised sub-expression: `'(' sum ')'`.
    fn group(&mut self) -> Option<CalcV> {
        debug_assert_eq!(self.peek(), Some(b'('));
        self.i += 1;
        let v = self.sum()?;
        self.skip_ws();
        if self.peek() != Some(b')') {
            return None;
        }
        self.i += 1;
        Some(v)
    }

    /// `<number> [unit | '%']` — absolute/font/viewport units resolve to points
    /// through [`parse_len_px`]; a bare number stays dimensionless.
    fn value(&mut self) -> Option<CalcV> {
        let start = self.i;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || c == b'.' {
                self.i += 1;
            } else {
                break;
            }
        }
        if self.i == start {
            return None;
        }
        let num: f64 = std::str::from_utf8(&self.s[start..self.i])
            .ok()?
            .parse()
            .ok()?;
        if self.peek() == Some(b'%') {
            self.i += 1;
            return Some(CalcV::Lin { pt: 0.0, pct: num });
        }
        let ustart = self.i;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphabetic() {
                self.i += 1;
            } else {
                break;
            }
        }
        if ustart == self.i {
            return Some(CalcV::Num(num));
        }
        let unit = std::str::from_utf8(&self.s[ustart..self.i])
            .ok()?
            .to_ascii_lowercase();
        let pt = parse_len_px(&format!("{num}{unit}"), self.em)?;
        Some(CalcV::Lin { pt, pct: 0.0 })
    }
}

/// Parse a full `calc( … )` expression to its linear form `(pt, pct)` — the
/// caller resolves the percent part against its own base (container width for
/// box lengths, `em` for font-relative contexts). `None` when the text is not
/// a `calc(...)` or the expression is invalid.
fn parse_calc_lin(v: &str, em: f64) -> Option<(f64, f64)> {
    let v = v.trim();
    // Byte-wise prefix test (a `str` slice could panic mid-char on exotic input).
    if v.len() < 5 || !v.as_bytes()[..5].eq_ignore_ascii_case(b"calc(") {
        return None;
    }
    let mut p = CalcParser {
        s: v.as_bytes(),
        i: 4, // at the opening '('
        em,
    };
    let val = p.group()?;
    p.skip_ws();
    if p.i != p.s.len() {
        return None; // trailing garbage after the closing ')'
    }
    match val {
        CalcV::Lin { pt, pct } => Some((pt, pct)),
        // A pure number is not a length (`calc(2*3)` where a length is due).
        CalcV::Num(_) => None,
    }
}

/// Evaluate a `calc(...)` down to points. The percent part resolves against
/// `em` — the same base the bare `%` branch of [`parse_len_px`] uses, since
/// this entry serves font-relative contexts.
fn parse_calc_px(v: &str, em: f64) -> Option<f64> {
    let (pt, pct) = parse_calc_lin(v, em)?;
    Some(pt + em * pct / 100.0)
}

/// CSS length units [`parse_len_px`] resolves — kept in **lock-step** with the
/// unit detection in flex-basis / font-size parsing (`%` is handled separately).
/// Absolute: `px`/`pt`/`cm`/`mm`/`in`/`pc`/`q`; font-relative: `em`/`rem`/`ex`/
/// `ch`; viewport: `vw`/`vh`.
const LENGTH_UNITS: [&str; 13] = [
    "px", "pt", "rem", "em", "vw", "vh", "cm", "mm", "in", "pc", "ex", "ch", "q",
];

/// Parse a length to absolute points (1px ≈ 0.75pt at 96dpi), resolving the
/// absolute units (`cm`/`mm`/`in`/`pc`/`q` via 1in = 72pt), `em`/`rem`/`ex`/`ch`
/// (font-relative; `ex`/`ch` ≈ 0.5em), `vw`/`vh` (reference viewport) and a basic
/// `calc()`/`var()`.
fn parse_len_px(v: &str, em: f64) -> Option<f64> {
    let resolved = resolve_var(v);
    let v = resolved.trim();
    if v.len() >= 5 && v.as_bytes()[..5].eq_ignore_ascii_case(b"calc(") {
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
    // `vw`/`vh` resolve against the LIVE page size when a layout is running
    // (installed by the layout entry via `set_viewport_for_layout`), falling
    // back to the reference US-Letter viewport outside one.
    if let Some(n) = v.strip_suffix("vw") {
        let vp_w = VIEWPORT_PT.with(|c| c.get().0);
        return n.trim().parse::<f64>().ok().map(|p| vp_w * p / 100.0);
    }
    if let Some(n) = v.strip_suffix("vh") {
        let vp_h = VIEWPORT_PT.with(|c| c.get().1);
        return n.trim().parse::<f64>().ok().map(|p| vp_h * p / 100.0);
    }
    // Absolute physical units, anchored at 1in = 72pt (= 96px).
    if let Some(n) = v.strip_suffix("cm") {
        return n.trim().parse::<f64>().ok().map(|p| p * 72.0 / 2.54);
    }
    if let Some(n) = v.strip_suffix("mm") {
        return n.trim().parse::<f64>().ok().map(|p| p * 72.0 / 25.4);
    }
    if let Some(n) = v.strip_suffix("in") {
        return n.trim().parse::<f64>().ok().map(|p| p * 72.0);
    }
    if let Some(n) = v.strip_suffix("pc") {
        return n.trim().parse::<f64>().ok().map(|p| p * 12.0);
    }
    // `ex`/`ch` have no font metrics here → the common 0.5em approximation.
    if let Some(n) = v.strip_suffix("ex") {
        return n.trim().parse::<f64>().ok().map(|p| p * em * 0.5);
    }
    if let Some(n) = v.strip_suffix("ch") {
        return n.trim().parse::<f64>().ok().map(|p| p * em * 0.5);
    }
    if let Some(n) = v.strip_suffix('q') {
        return n.trim().parse::<f64>().ok().map(|p| p * 72.0 / 101.6);
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
    // `calc()` keeps its percent part symbolic so box lengths resolve it
    // against the container (not `em`): pure-pt / pure-% forms normalise to
    // the plain variants, a genuine mix stays `Len::Calc`.
    if v.len() >= 5 && v.as_bytes()[..5].eq_ignore_ascii_case(b"calc(") {
        let (pt, pct) = parse_calc_lin(v, em)?;
        return Some(if pct == 0.0 {
            Len::Pt(pt)
        } else if pt == 0.0 {
            Len::Percent(pct)
        } else {
            Len::Calc { pt, pct }
        });
    }
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
    parse_color_alpha(v).map(|(rgb, _)| rgb)
}

/// Parse a CSS colour into `(rgb, alpha)` — channels and alpha all 0..=1. Handles
/// `#rgb[a]` / `#rrggbb[aa]`, `rgb()/rgba()`, `hsl()/hsla()` and the named
/// colours; a missing alpha defaults to fully opaque (`1.0`).
pub fn parse_color_alpha(v: &str) -> Option<([f64; 3], f64)> {
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
        // The 4-/8-digit forms carry an alpha nibble/byte (else fully opaque); a
        // malformed alpha (`#12345`) discards the whole colour (returns None).
        let a = match hex.len() {
            4 => u8::from_str_radix(&hex[3..4].repeat(2), 16).ok()? as f64 / 255.0,
            8 => u8::from_str_radix(&hex[6..8], 16).ok()? as f64 / 255.0,
            _ => 1.0,
        };
        return Some(([r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0], a));
    }
    // `rgb()` / `rgba()` — comma- or space-separated; the alpha (4th value, or
    // after a `/`) is ignored but the colour is still returned.
    if let Some(inner) = v
        .strip_prefix("rgba(")
        .or_else(|| v.strip_prefix("rgb("))
        .and_then(|s| s.strip_suffix(')'))
    {
        let normalized = inner.replace('/', " ");
        let raw: Vec<&str> = normalized
            .split([',', ' '])
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .collect();
        let rgb: Vec<f64> = raw
            .iter()
            .take(3)
            .filter_map(|n| parse_rgb_component(n))
            .collect();
        if rgb.len() == 3 {
            let a = raw.get(3).and_then(|t| parse_alpha(t)).unwrap_or(1.0);
            return Some(([rgb[0], rgb[1], rgb[2]], a));
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
            let a = parts.get(3).and_then(|t| parse_alpha(t)).unwrap_or(1.0);
            return Some((hsl_to_rgb(h, s, l), a));
        }
        return None;
    }
    named_color(&v).map(|rgb| (rgb, 1.0))
}

/// One `rgb()` channel → 0..=1. Accepts `0-255` integers/floats and `%`.
fn parse_rgb_component(t: &str) -> Option<f64> {
    if let Some(p) = t.strip_suffix('%') {
        return p
            .trim()
            .parse::<f64>()
            .ok()
            .map(|n| (n / 100.0).clamp(0.0, 1.0));
    }
    t.parse::<f64>().ok().map(|n| (n / 255.0).clamp(0.0, 1.0))
}

/// An alpha component (`rgba`/`hsla` 4th value, or `/`-alpha) → 0..=1. Accepts a
/// `0..1` number or a percentage. Unlike a colour channel it is **not** divided
/// by 255 (CSS alpha is already a fraction).
fn parse_alpha(t: &str) -> Option<f64> {
    if let Some(p) = t.strip_suffix('%') {
        return p
            .trim()
            .parse::<f64>()
            .ok()
            .map(|n| (n / 100.0).clamp(0.0, 1.0));
    }
    t.parse::<f64>().ok().map(|n| n.clamp(0.0, 1.0))
}

/// Drop whitespace **inside** parentheses so a whitespace tokeniser keeps a
/// function colour (`rgba(0, 0, 0, .5)`, `hsl(0 100% 50%)`) as one token. Spaces
/// outside parens (the real shorthand separators) are preserved.
fn collapse_paren_spaces(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut depth = 0i32;
    for c in v.chars() {
        match c {
            '(' => {
                depth += 1;
                out.push(c);
            }
            ')' => {
                depth = (depth - 1).max(0);
                out.push(c);
            }
            c if depth > 0 && c.is_whitespace() => {}
            c => out.push(c),
        }
    }
    out
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
    p.trim()
        .parse::<f64>()
        .ok()
        .map(|n| (n / 100.0).clamp(0.0, 1.0))
}

/// Convert HSL (`h` in degrees, `s`/`l` in 0..=1) to linear-free sRGB 0..=1.
fn hsl_to_rgb(h: f64, s: f64, l: f64) -> [f64; 3] {
    if s == 0.0 {
        return [l, l, l]; // achromatic (grey)
    }
    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
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
            [
                0x64 as f64 / 255.0,
                0x95 as f64 / 255.0,
                0xed as f64 / 255.0
            ],
        ));
        assert!(approx(
            parse_color("crimson").unwrap(),
            [0.863, 0.078, 0.235]
        ));
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
        assert!(approx(
            parse_color("rgba(255, 0, 0, 0.5)").unwrap(),
            [1.0, 0.0, 0.0]
        ));
        // Space-separated + slash-alpha form.
        assert!(approx(
            parse_color("rgb(0 128 0 / 50%)").unwrap(),
            [0.0, 0.502, 0.0]
        ));
        // Percentage channels.
        assert!(approx(
            parse_color("rgb(100%, 0%, 0%)").unwrap(),
            [1.0, 0.0, 0.0]
        ));
    }

    #[test]
    fn hsl_and_hsla_convert_to_rgb() {
        // hsl(0,100%,50%) = red, hsl(120,100%,50%) = green, hsl(240,…) = blue.
        assert!(approx(
            parse_color("hsl(0, 100%, 50%)").unwrap(),
            [1.0, 0.0, 0.0]
        ));
        assert!(approx(
            parse_color("hsl(120, 100%, 50%)").unwrap(),
            [0.0, 1.0, 0.0]
        ));
        assert!(approx(
            parse_color("hsl(240, 100%, 50%)").unwrap(),
            [0.0, 0.0, 1.0]
        ));
        // Saturation 0 → grey at the lightness level.
        assert!(approx(
            parse_color("hsl(0, 0%, 50%)").unwrap(),
            [0.5, 0.5, 0.5]
        ));
        // hsla keeps the colour, drops alpha.
        assert!(approx(
            parse_color("hsla(0,100%,50%,0.3)").unwrap(),
            [1.0, 0.0, 0.0]
        ));
    }

    #[test]
    fn hex_with_alpha_keeps_rgb() {
        // `#rgba` / `#rrggbbaa` — the alpha nibble/byte is validated then dropped.
        assert_eq!(parse_color("#ff0000ff"), Some([1.0, 0.0, 0.0]));
        assert_eq!(parse_color("#f00f"), Some([1.0, 0.0, 0.0]));
        assert!(approx(
            parse_color("#aabbccdd").unwrap(),
            [0.667, 0.733, 0.8]
        ));
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
    fn important_suffix_is_stripped_not_corrupting() {
        // `!important` is a cascade flag, not part of the value: previously the
        // suffix stayed glued to the value ("red !important"), the colour parse
        // failed, and the whole declaration was silently dropped.
        let nodes = parse(r#"<style>p { color: red !important; }</style><p>x</p>"#);
        let sheet = Stylesheet::new(&collect_style_css(&nodes));
        let p = nodes
            .iter()
            .find_map(|n| match n {
                Node::Element(e) if e.tag == "p" => Some(e),
                _ => None,
            })
            .unwrap();
        let style = sheet.computed(p, &Style::default(), &[]);
        assert_eq!(style.color, [1.0, 0.0, 0.0], "value survives the flag");
        // Spacing/case variants of the flag are all recognised.
        assert_eq!(strip_important("red !important"), ("red", true));
        assert_eq!(strip_important("red!important"), ("red", true));
        assert_eq!(strip_important("red ! IMPORTANT"), ("red", true));
        assert_eq!(strip_important("red"), ("red", false));
        // "important" alone (no `!`) is a legitimate value, not a flag.
        assert_eq!(strip_important("important"), ("important", false));
    }

    #[test]
    fn important_wins_the_cascade() {
        // A `!important` tag rule beats a later, more specific class rule AND a
        // normal inline style; an important inline style beats everything.
        let nodes = parse(
            r#"<style>p { color: red !important; } p.k { color: green; }</style>
               <p class="k" style="color: blue">a</p>
               <p class="k" style="color: blue !important">b</p>"#,
        );
        let sheet = Stylesheet::new(&collect_style_css(&nodes));
        let ps: Vec<&Element> = nodes
            .iter()
            .filter_map(|n| match n {
                Node::Element(e) if e.tag == "p" => Some(e),
                _ => None,
            })
            .collect();
        let a = sheet.computed(ps[0], &Style::default(), &[]);
        assert_eq!(
            a.color,
            [1.0, 0.0, 0.0],
            "rule !important beats class rule + normal inline"
        );
        let b = sheet.computed(ps[1], &Style::default(), &[]);
        assert_eq!(
            b.color,
            [0.0, 0.0, 1.0],
            "inline !important beats rule !important"
        );
    }

    /// Elements (not text) among a node's children.
    fn child_elements(el: &Element) -> Vec<&Element> {
        el.children
            .iter()
            .filter_map(|n| match n {
                Node::Element(e) => Some(e),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn select_renders_only_the_selected_option() {
        // A closed <select> shows ONE option; previously every option's text was
        // concatenated into the output.
        let nodes = parse(
            "<select><option>One</option><option selected>Two</option>\
             <option>Three</option></select>",
        );
        let sheet = Stylesheet::new("");
        let select = nodes
            .iter()
            .find_map(|n| match n {
                Node::Element(e) if e.tag == "select" => Some(e),
                _ => None,
            })
            .unwrap();
        let options = child_elements(select);
        assert_eq!(options.len(), 3);
        let displays: Vec<Display> = options
            .iter()
            .map(|o| sheet.computed(o, &Style::default(), &[select]).display)
            .collect();
        assert_eq!(displays[0], Display::None, "unselected option hidden");
        assert_ne!(displays[1], Display::None, "selected option shown");
        assert_eq!(displays[2], Display::None, "unselected option hidden");
    }

    #[test]
    fn select_without_selected_shows_the_first_option() {
        let nodes = parse("<select><option>First</option><option>Second</option></select>");
        let sheet = Stylesheet::new("");
        let select = nodes
            .iter()
            .find_map(|n| match n {
                Node::Element(e) if e.tag == "select" => Some(e),
                _ => None,
            })
            .unwrap();
        let options = child_elements(select);
        assert_eq!(options.len(), 2);
        let first = sheet.computed(options[0], &Style::default(), &[select]);
        let second = sheet.computed(options[1], &Style::default(), &[select]);
        assert_ne!(first.display, Display::None, "first option is the default");
        assert_eq!(second.display, Display::None, "others hidden");
    }

    #[test]
    fn optgroup_options_resolve_against_the_whole_select() {
        // The `selected` option lives in the SECOND optgroup: the first group's
        // options must still hide (the scope is the select, not the optgroup).
        let nodes = parse(
            "<select><optgroup><option>A</option></optgroup>\
             <optgroup><option selected>B</option></optgroup></select>",
        );
        let sheet = Stylesheet::new("");
        let select = nodes
            .iter()
            .find_map(|n| match n {
                Node::Element(e) if e.tag == "select" => Some(e),
                _ => None,
            })
            .unwrap();
        let groups = child_elements(select);
        let a = child_elements(groups[0])[0];
        let b = child_elements(groups[1])[0];
        let sa = sheet.computed(a, &Style::default(), &[select, groups[0]]);
        let sb = sheet.computed(b, &Style::default(), &[select, groups[1]]);
        assert_eq!(
            sa.display,
            Display::None,
            "unselected group-A option hidden"
        );
        assert_ne!(sb.display, Display::None, "selected group-B option shown");
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

    /// Extract the [`LinearGradient`] from a style's background gradient (panics
    /// if it is absent or a different kind) — keeps the gradient tests terse.
    fn as_linear(s: &Style) -> &LinearGradient {
        match s.background_gradient.as_ref().expect("a gradient parsed") {
            CssGradient::Linear(g) => g,
            other => panic!("expected a linear gradient, got {other:?}"),
        }
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

    // ── border-radius / box-shadow ──────────────────────────────────────────

    #[test]
    fn border_radius_defaults_to_square() {
        // No `border-radius` ⇒ all corners zero (the unchanged rectangular path).
        let s = inline_style("background:#eee");
        assert_eq!(s.border_radius, [0.0; 4]);
        assert!(s.box_shadow.is_none());
    }

    #[test]
    fn border_radius_single_value_applies_to_all_corners() {
        // `8pt` → every corner 8pt. (TL, TR, BR, BL).
        let s = inline_style("border-radius:8pt");
        assert_eq!(s.border_radius, [8.0, 8.0, 8.0, 8.0]);
        // px converts at 0.75pt/px: 16px → 12pt.
        let s = inline_style("border-radius:16px");
        assert_eq!(s.border_radius, [12.0, 12.0, 12.0, 12.0]);
    }

    #[test]
    fn border_radius_fill_rules_match_css() {
        // 2 values: TL/BR = a, TR/BL = b.
        let s = inline_style("border-radius:10pt 20pt");
        assert_eq!(s.border_radius, [10.0, 20.0, 10.0, 20.0]);
        // 3 values: TL=a, TR/BL=b, BR=c.
        let s = inline_style("border-radius:1pt 2pt 3pt");
        assert_eq!(s.border_radius, [1.0, 2.0, 3.0, 2.0]);
        // 4 values: TL, TR, BR, BL verbatim.
        let s = inline_style("border-radius:1pt 2pt 3pt 4pt");
        assert_eq!(s.border_radius, [1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn border_radius_elliptical_keeps_horizontal_radii() {
        // `h / v` form: only the horizontal radii are modelled (circular corners).
        let s = inline_style("border-radius:10pt 20pt / 5pt 6pt");
        assert_eq!(s.border_radius, [10.0, 20.0, 10.0, 20.0]);
    }

    #[test]
    fn border_radius_corner_longhands_set_one_corner_each() {
        let s = inline_style(
            "border-top-left-radius:1pt;border-top-right-radius:2pt;\
             border-bottom-right-radius:3pt;border-bottom-left-radius:4pt",
        );
        assert_eq!(s.border_radius, [1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn box_shadow_parses_offsets_blur_spread_color() {
        let s = inline_style("box-shadow:2pt 3pt 4pt 1pt #ff0000");
        let sh = s.box_shadow.expect("a shadow");
        assert_eq!(sh.dx, 2.0);
        assert_eq!(sh.dy, 3.0);
        assert_eq!(sh.blur, 4.0);
        assert_eq!(sh.spread, 1.0);
        assert_eq!(sh.color, [1.0, 0.0, 0.0]);
        assert!(!sh.inset);
    }

    #[test]
    fn box_shadow_minimal_two_offsets_only() {
        // Just the required two offsets ⇒ blur/spread default 0, colour black.
        let s = inline_style("box-shadow:4pt 4pt");
        let sh = s.box_shadow.expect("a shadow");
        assert_eq!((sh.dx, sh.dy, sh.blur, sh.spread), (4.0, 4.0, 0.0, 0.0));
        assert_eq!(sh.color, [0.0, 0.0, 0.0]);
    }

    #[test]
    fn box_shadow_inset_and_colour_in_any_order() {
        // `inset` keyword + leading colour are both tolerated.
        let s = inline_style("box-shadow:inset #00ff00 1pt 2pt 3pt");
        let sh = s.box_shadow.expect("a shadow");
        assert!(sh.inset);
        assert_eq!(sh.color, [0.0, 1.0, 0.0]);
        assert_eq!((sh.dx, sh.dy, sh.blur), (1.0, 2.0, 3.0));
    }

    #[test]
    fn box_shadow_none_and_garbage_yield_none() {
        assert!(inline_style("box-shadow:none").box_shadow.is_none());
        // A single offset is not enough to place a shadow.
        assert!(inline_style("box-shadow:5pt").box_shadow.is_none());
    }

    #[test]
    fn box_shadow_keeps_only_first_layer() {
        // Two comma-separated layers: only the first (topmost) is kept.
        let s = inline_style("box-shadow:1pt 1pt #ff0000, 9pt 9pt #0000ff");
        let sh = s.box_shadow.expect("a shadow");
        assert_eq!((sh.dx, sh.dy), (1.0, 1.0));
        assert_eq!(sh.color, [1.0, 0.0, 0.0]);
    }

    #[test]
    fn border_radius_and_box_shadow_are_not_inherited() {
        // A parent radius/shadow must not leak to a child element.
        let nodes = parse(
            r#"<div style="border-radius:10pt;box-shadow:2pt 2pt #000"><span>x</span></div>"#,
        );
        let sheet = Stylesheet::new("");
        let div = match &nodes[0] {
            super::super::dom::Node::Element(e) => e,
            _ => panic!("div"),
        };
        let parent = sheet.computed(div, &Style::default(), &[]);
        assert_eq!(parent.border_radius, [10.0; 4], "parent keeps its radius");
        assert!(parent.box_shadow.is_some());
        let span = match &div.children[0] {
            super::super::dom::Node::Element(e) => e,
            _ => panic!("span"),
        };
        let child = sheet.computed(span, &parent, &[div]);
        assert_eq!(child.border_radius, [0.0; 4], "radius is not inherited");
        assert!(child.box_shadow.is_none(), "shadow is not inherited");
    }

    // ── border-style (dashed/dotted/double) / linear-gradient ────────────────

    #[test]
    fn border_shorthand_captures_dashed_style() {
        // `border: 1px dashed red` carries width, colour AND the dash style on
        // all four sides; the default stays solid for an unstyled border.
        let s = inline_style("border:2pt dashed #ff0000");
        assert_eq!(s.border_width.top, 2.0);
        assert_eq!(s.border_color_edges, [[1.0, 0.0, 0.0]; 4]);
        assert_eq!(s.border_style_edges, [BorderStyle::Dashed; 4]);

        // A solid (or styleless) border is — and stays — Solid.
        let plain = inline_style("border:1pt solid #000000");
        assert_eq!(plain.border_style_edges, [BorderStyle::Solid; 4]);
    }

    #[test]
    fn per_side_border_styles_parse() {
        // Longhand and side-shorthand each set just their own side's style.
        let s = inline_style(
            "border:1pt solid #000000;border-bottom:1pt dotted #000000;border-left-style:double",
        );
        assert_eq!(
            s.border_style_edges[0],
            BorderStyle::Solid,
            "top stays solid"
        );
        assert_eq!(
            s.border_style_edges[2],
            BorderStyle::Dotted,
            "bottom dotted"
        );
        assert_eq!(s.border_style_edges[3], BorderStyle::Double, "left double");
    }

    #[test]
    fn border_style_shorthand_is_trbl() {
        // `border-style: dashed dotted` → top/bottom dashed, right/left dotted.
        let s = inline_style("border:1pt solid #000000;border-style:dashed dotted");
        assert_eq!(s.border_style_edges[0], BorderStyle::Dashed, "top");
        assert_eq!(s.border_style_edges[1], BorderStyle::Dotted, "right");
        assert_eq!(s.border_style_edges[2], BorderStyle::Dashed, "bottom = top");
        assert_eq!(s.border_style_edges[3], BorderStyle::Dotted, "left = right");
    }

    #[test]
    fn unknown_border_style_falls_back_to_solid() {
        // `groove` now maps to its 3-D bevel style; a value we don't recognise at
        // all (`wavy`) still falls back to Solid.
        let groove = inline_style("border:1pt groove #000000");
        assert_eq!(groove.border_style_edges, [BorderStyle::Groove; 4]);
        let unknown = inline_style("border:1pt wavy #000000");
        assert_eq!(unknown.border_style_edges, [BorderStyle::Solid; 4]);
    }

    #[test]
    fn linear_gradient_parses_angle_and_stops() {
        let s = inline_style("background:linear-gradient(90deg, #ff0000, #0000ff)");
        let g = as_linear(&s);
        assert!((g.angle_deg - 90.0).abs() < 0.01, "90deg angle");
        assert_eq!(g.stops.len(), 2, "two stops");
        assert_eq!(g.stops[0].color, [1.0, 0.0, 0.0]);
        assert_eq!(g.stops[1].color, [0.0, 0.0, 1.0]);
        // A gradient does not set a solid background colour.
        assert!(s.background.is_none(), "solid background left unset");
    }

    #[test]
    fn linear_gradient_to_side_keyword_and_positions() {
        // `to right` ≡ 90deg; a `%` position is captured per stop.
        let s = inline_style("background-image:linear-gradient(to right, red 10%, blue 90%)");
        let g = as_linear(&s);
        assert!((g.angle_deg - 90.0).abs() < 0.01, "to right = 90deg");
        assert_eq!(g.stops[0].pos, Some(0.10));
        assert_eq!(g.stops[1].pos, Some(0.90));
    }

    #[test]
    fn linear_gradient_default_direction_is_to_bottom() {
        // No leading direction ⇒ CSS default 180deg (to bottom).
        let s = inline_style("background:linear-gradient(red, blue)");
        let g = as_linear(&s);
        assert!((g.angle_deg - 180.0).abs() < 0.01, "default 180deg");
        assert_eq!(g.stops.len(), 2);
    }

    #[test]
    fn malformed_gradient_leaves_solid_background_intact() {
        // A one-stop (invalid) gradient is ignored; a separately declared solid
        // colour still applies (gradient parsing never clobbers it).
        let s = inline_style("background:#112233;background-image:linear-gradient(red)");
        assert!(
            s.background_gradient.is_none(),
            "one-stop gradient rejected"
        );
        let bg = s.background.expect("solid background preserved");
        assert!(
            approx(bg, [17.0 / 255.0, 34.0 / 255.0, 51.0 / 255.0]),
            "got {bg:?}"
        );
    }

    #[test]
    fn radial_gradient_parses_center_and_stops() {
        // Default circle, centred; two stops centre→edge.
        let s = inline_style("background:radial-gradient(#ff0000, #0000ff)");
        match s.background_gradient.as_ref().expect("radial gradient") {
            CssGradient::Radial(g) => {
                assert!(
                    (g.cx - 0.5).abs() < 1e-9 && (g.cy - 0.5).abs() < 1e-9,
                    "centred"
                );
                assert_eq!(g.stops.len(), 2);
                assert_eq!(g.stops[0].color, [1.0, 0.0, 0.0]);
                assert_eq!(g.stops[1].color, [0.0, 0.0, 1.0]);
            }
            other => panic!("expected radial, got {other:?}"),
        }
        assert!(
            s.background.is_none(),
            "gradient leaves solid background unset"
        );
    }

    #[test]
    fn radial_gradient_at_position_and_size_keyword() {
        // `circle farthest-corner at 25% 75%` → centre (0.25,0.75), r ≈ √2.
        let s = inline_style(
            "background:radial-gradient(circle farthest-corner at 25% 75%, red, blue)",
        );
        match s.background_gradient.as_ref().expect("radial") {
            CssGradient::Radial(g) => {
                assert!((g.cx - 0.25).abs() < 1e-9, "cx={}", g.cx);
                assert!((g.cy - 0.75).abs() < 1e-9, "cy={}", g.cy);
                assert!(
                    (g.r - std::f64::consts::SQRT_2).abs() < 1e-9,
                    "farthest-corner radius fraction, got {}",
                    g.r
                );
            }
            other => panic!("expected radial, got {other:?}"),
        }
    }

    #[test]
    fn conic_gradient_parses_from_angle_and_stops() {
        let s = inline_style("background:conic-gradient(from 90deg at 50% 50%, red, lime, blue)");
        match s.background_gradient.as_ref().expect("conic") {
            CssGradient::Conic(g) => {
                assert!(
                    (g.from_deg - 90.0).abs() < 1e-9,
                    "from 90deg, got {}",
                    g.from_deg
                );
                assert!((g.cx - 0.5).abs() < 1e-9 && (g.cy - 0.5).abs() < 1e-9);
                assert_eq!(g.stops.len(), 3);
            }
            other => panic!("expected conic, got {other:?}"),
        }
    }

    #[test]
    fn conic_gradient_default_from_is_zero() {
        let s = inline_style("background:conic-gradient(#000000, #ffffff)");
        match s.background_gradient.as_ref().expect("conic") {
            CssGradient::Conic(g) => {
                assert!((g.from_deg).abs() < 1e-9, "default from 0deg");
                assert!((g.cx - 0.5).abs() < 1e-9, "default centred");
            }
            other => panic!("expected conic, got {other:?}"),
        }
    }

    #[test]
    fn linear_gradient_still_byte_identical_after_generalisation() {
        // Regression guard: the linear path must keep producing a `Linear` arm
        // with the same angle/stops it always has (no accidental re-routing).
        let s = inline_style("background:linear-gradient(45deg, red, blue)");
        let g = as_linear(&s);
        assert!((g.angle_deg - 45.0).abs() < 1e-9);
        assert_eq!(g.stops.len(), 2);
    }

    #[test]
    fn elliptical_border_radius_keeps_horizontal_and_vertical() {
        // `a b c d / e f g h` — horizontal radii from the first list, vertical
        // from the second (each with TRBL fill rules).
        let s = inline_style("border-radius:10pt 20pt 30pt 40pt / 5pt 6pt 7pt 8pt");
        assert_eq!(s.border_radius, [10.0, 20.0, 30.0, 40.0]);
        assert_eq!(s.border_radius_v, [5.0, 6.0, 7.0, 8.0]);
    }

    #[test]
    fn circular_border_radius_mirrors_h_into_v() {
        // No `/` ⇒ vertical radii equal the horizontal ones (circular corners).
        let s = inline_style("border-radius:8pt");
        assert_eq!(s.border_radius, [8.0; 4]);
        assert_eq!(s.border_radius_v, [8.0; 4], "circular: v mirrors h");
    }

    #[test]
    fn elliptical_corner_longhand_takes_h_v_pair() {
        let s = inline_style("border-top-left-radius:12pt 4pt");
        assert_eq!(s.border_radius[0], 12.0);
        assert_eq!(s.border_radius_v[0], 4.0);
        // A lone value is circular for that corner.
        let s2 = inline_style("border-bottom-right-radius:9pt");
        assert_eq!(s2.border_radius[2], 9.0);
        assert_eq!(s2.border_radius_v[2], 9.0);
    }

    #[test]
    fn box_shadow_keeps_all_layers() {
        // Two layers: the first is the topmost (kept in `box_shadow`), the second
        // (and any more) land in `box_shadow_extra`, in source order.
        let s = inline_style("box-shadow:1pt 1pt 2pt #ff0000, 4pt 4pt 8pt 1pt #0000ff");
        let top = s.box_shadow.expect("first layer");
        assert_eq!((top.dx, top.dy, top.blur), (1.0, 1.0, 2.0));
        assert_eq!(top.color, [1.0, 0.0, 0.0]);
        assert_eq!(s.box_shadow_extra.len(), 1, "one extra layer");
        let extra = s.box_shadow_extra[0];
        assert_eq!(
            (extra.dx, extra.dy, extra.blur, extra.spread),
            (4.0, 4.0, 8.0, 1.0)
        );
        assert_eq!(extra.color, [0.0, 0.0, 1.0]);
    }

    #[test]
    fn single_box_shadow_has_no_extra_layers() {
        // The common one-shadow case keeps `box_shadow_extra` empty (unchanged).
        let s = inline_style("box-shadow:2pt 3pt 4pt #000000");
        assert!(s.box_shadow.is_some());
        assert!(s.box_shadow_extra.is_empty());
        // `none` clears everything.
        let n = inline_style("box-shadow:none");
        assert!(n.box_shadow.is_none() && n.box_shadow_extra.is_empty());
    }

    #[test]
    fn position_sticky_parses_as_sticky() {
        assert_eq!(inline_style("position:sticky").position, Position::Sticky);
        // The other schemes are unchanged.
        assert_eq!(
            inline_style("position:relative").position,
            Position::Relative
        );
        assert_eq!(inline_style("position:fixed").position, Position::Fixed);
        assert_eq!(inline_style("position:static").position, Position::Static);
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
        assert_eq!(
            inherit(&parent).column_count,
            0,
            "column-count does not inherit"
        );
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
        walk(
            &nodes,
            &sheet,
            &Style::default(),
            &mut Vec::new(),
            tag,
            &mut out,
        );
        out.expect("target element not found")
    }

    /// Computed `color` of every `tag` element, in document order.
    fn colors_of(html: &str, tag: &str) -> Vec<[f64; 3]> {
        let nodes = parse(html);
        let sheet = Stylesheet::new(&collect_style_css(&nodes));
        fn walk<'a>(
            nodes: &'a [Node],
            sheet: &Stylesheet,
            parent: &Style,
            chain: &mut Vec<&'a Element>,
            target: &str,
            out: &mut Vec<[f64; 3]>,
        ) {
            for n in nodes {
                if let Node::Element(e) = n {
                    let st = sheet.computed(e, parent, chain);
                    if e.tag == target {
                        out.push(st.color);
                    }
                    chain.push(e);
                    walk(&e.children, sheet, &st, chain, target, out);
                    chain.pop();
                }
            }
        }
        let mut out = Vec::new();
        walk(
            &nodes,
            &sheet,
            &Style::default(),
            &mut Vec::new(),
            tag,
            &mut out,
        );
        out
    }

    #[test]
    fn structural_pseudo_classes_select_by_sibling_position() {
        // first-child / nth-child / last-child each pick ONE li, not every li.
        let html = "<style>li{color:#000000} \
                    li:first-child{color:#ff0000} \
                    li:nth-child(2){color:#00ff00} \
                    li:last-child{color:#0000ff}</style>\
                    <ul><li>a</li><li>b</li><li>c</li></ul>";
        let cols = colors_of(html, "li");
        assert_eq!(cols.len(), 3);
        assert_eq!(cols[0], [1.0, 0.0, 0.0], "1st li is :first-child → red");
        assert_eq!(cols[1], [0.0, 1.0, 0.0], "2nd li is :nth-child(2) → green");
        assert_eq!(cols[2], [0.0, 0.0, 1.0], "3rd li is :last-child → blue");
    }

    #[test]
    fn nth_child_formula_and_only_child() {
        // nth-child(odd) hits positions 1 and 3; :only-child needs a lone child.
        let odd = "<style>li{color:#000000} li:nth-child(odd){color:#ff0000}</style>\
                   <ul><li>a</li><li>b</li><li>c</li></ul>";
        let cols = colors_of(odd, "li");
        assert_eq!(cols[0], [1.0, 0.0, 0.0], "pos 1 is odd");
        assert_eq!(cols[1], [0.0, 0.0, 0.0], "pos 2 is even → unstyled");
        assert_eq!(cols[2], [1.0, 0.0, 0.0], "pos 3 is odd");
        let only = "<style>li{color:#000000} li:only-child{color:#ff0000}</style>\
                    <ul><li>solo</li></ul>";
        assert_eq!(
            colors_of(only, "li")[0],
            [1.0, 0.0, 0.0],
            "lone li is :only-child"
        );
        let not_only = "<style>li{color:#000000} li:only-child{color:#ff0000}</style>\
                        <ul><li>a</li><li>b</li></ul>";
        assert_eq!(
            colors_of(not_only, "li")[0],
            [0.0, 0.0, 0.0],
            "two li → not :only-child"
        );
    }

    #[test]
    fn pseudo_element_selector_does_not_style_the_real_element() {
        // We don't generate ::before/::after boxes, so such a rule must NOT leak
        // onto the element itself (it used to, via the skipped `:`).
        let html = "<style>p{color:#000000} p::before{color:#ff0000}</style><p>x</p>";
        assert_eq!(
            colors_of(html, "p")[0],
            [0.0, 0.0, 0.0],
            "::before leaves <p> unstyled"
        );
        // A dynamic pseudo-class can never fire on static paged output: the
        // rule is inert (it used to over-match, painting hover styles into
        // the PDF).
        let hover = "<style>a{color:#000000} a:hover{color:#ff0000}</style><a>x</a>";
        assert_eq!(
            colors_of(hover, "a")[0],
            [0.0, 0.0, 0.0],
            ":hover rule is inert on paged output"
        );
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

        let absent = color_of_first("<style>[data-x] { color: lime }</style><p>x</p>", "p");
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

        let by_id = color_of_first("<style>#a { color: lime }</style><p id=\"a\">x</p>", "p");
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
        assert!(
            s.margin_left_auto && s.margin_right_auto,
            "0 auto → both auto"
        );
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

    // ── CSS-2 quick-win 1: @media print/screen ─────────────────────────────

    #[test]
    fn media_query_applies_selects_print() {
        // Print and the universal/empty queries apply; screen-only does not.
        assert!(media_query_applies("print", None));
        assert!(media_query_applies("all", None));
        assert!(media_query_applies("", None), "bare @media is always-on");
        assert!(!media_query_applies("screen", None));
        assert!(!media_query_applies("speech", None));
        // Comma list is an OR: applies if ANY component matches.
        assert!(media_query_applies("screen, print", None));
        assert!(!media_query_applies("screen, speech", None));
        // `only print` / `not screen` modifiers resolve to print.
        assert!(media_query_applies("only print", None));
        assert!(media_query_applies("not screen", None));
        assert!(!media_query_applies("not print", None));
        assert!(!media_query_applies("only screen", None));
        // Without a viewport, a feature-only query applies (we keep the rules).
        assert!(media_query_applies("(max-width: 600px)", None));
        assert!(media_query_applies("print and (color)", None));
    }

    #[test]
    fn media_feature_queries_evaluate_against_the_viewport() {
        // 816 ≈ a US-Letter page width in CSS px (612pt ÷ 0.75). Width features
        // compare against this page viewport.
        let wide = Some(816.0);
        assert!(
            media_query_applies("(min-width: 600px)", wide),
            "816 >= 600"
        );
        assert!(
            !media_query_applies("(max-width: 600px)", wide),
            "816 > 600"
        );
        assert!(
            media_query_applies("(max-width: 600px)", Some(400.0)),
            "400 <= 600"
        );
        // Media type AND feature must BOTH hold.
        assert!(media_query_applies("print and (min-width: 600px)", wide));
        assert!(
            !media_query_applies("print and (max-width: 600px)", wide),
            "feature fails"
        );
        assert!(
            !media_query_applies("screen and (min-width: 600px)", wide),
            "type fails"
        );
        // Unknown feature / no viewport ⇒ applies (rules not dropped).
        assert!(media_query_applies("(orientation: landscape)", wide));
        assert!(media_query_applies("(min-width: 9999px)", None));
    }

    #[test]
    fn transform_parses_each_function() {
        // translate lengths resolve to points (px · 0.75).
        let t = parse_transform("translate(8px, 16px)", 16.0).unwrap();
        assert!((t[4] - 6.0).abs() < 1e-6 && (t[5] - 12.0).abs() < 1e-6);
        // scale / rotate / matrix.
        assert_eq!(
            parse_transform("scale(2, 3)", 16.0).unwrap(),
            [2.0, 0.0, 0.0, 3.0, 0.0, 0.0]
        );
        let r = parse_transform("rotate(90deg)", 16.0).unwrap();
        assert!(r[0].abs() < 1e-9 && (r[1] - 1.0).abs() < 1e-9 && (r[2] + 1.0).abs() < 1e-9);
        assert_eq!(
            parse_transform("matrix(1, 0, 0, 1, 5, 6)", 16.0).unwrap(),
            [1.0, 0.0, 0.0, 1.0, 5.0, 6.0]
        );
        // Compose left-to-right: `scale(2) translate(10pt,0)` applies translate
        // first, so a point (0,0) → (10,0) → scaled → (20,0): e = 20.
        assert_eq!(
            parse_transform("scale(2) translate(10pt, 0)", 16.0).unwrap()[4],
            20.0
        );
        assert!(parse_transform("none", 16.0).is_none());
        assert!(parse_transform("", 16.0).is_none());
    }

    #[test]
    fn attribute_operator_selectors_match_in_the_cascade() {
        // Each operator selects the <p> by its attribute, colouring it red.
        let cases = [
            ("[class^=btn]", "class=\"btn-x\""),     // prefix
            ("[data-x$=ary]", "data-x=\"primary\""), // suffix
            ("[data-x*=im]", "data-x=\"primary\""),  // substring
            ("[class~=b]", "class=\"a b c\""),       // word in a list
            ("[lang|=en]", "lang=\"en-US\""),        // dash-prefix
        ];
        for (sel, attr) in cases {
            let html = format!("<style>{sel}{{color:red}}</style><p {attr}>x</p>");
            assert_eq!(color_of_first(&html, "p"), RED, "{sel} should match {attr}");
        }
        // Negatives: the operator must NOT over-match.
        let neg = color_of_first(
            "<style>p{color:black}[class^=btn]{color:red}</style><p class=\"nav\">x</p>",
            "p",
        );
        assert_eq!(
            neg,
            [0.0, 0.0, 0.0],
            "prefix that doesn't match leaves p black"
        );
    }

    #[test]
    fn background_image_url_is_captured() {
        let s = inline_style("background-image:url(data:image/png;base64,AAAA)");
        assert_eq!(
            s.background_image.as_deref(),
            Some("data:image/png;base64,AAAA")
        );
        // In the `background` shorthand too (quotes optional); the colour still parses.
        let sh = inline_style("background:red url('http://x/y.png')");
        assert_eq!(sh.background_image.as_deref(), Some("http://x/y.png"));
        assert_eq!(sh.background, Some([1.0, 0.0, 0.0]));
        // `background-color` never carries an image; gradients/none set no image.
        assert!(inline_style("background-color:blue")
            .background_image
            .is_none());
        assert!(inline_style("background-image:linear-gradient(red, blue)")
            .background_image
            .is_none());
    }

    #[test]
    fn media_print_rules_apply_screen_rules_drop() {
        // The print block's rule wins; the screen block is dropped entirely, so
        // the later (source-order) screen rule does NOT override the print one.
        let c = color_of_first(
            "<style>@media print { p { color: red } } \
                    @media screen { p { color: blue } }</style><p>x</p>",
            "p",
        );
        assert_eq!(
            c,
            [1.0, 0.0, 0.0],
            "print rule applies, screen rule dropped"
        );
    }

    #[test]
    fn media_nested_balanced_braces_do_not_truncate_following_rules() {
        // The `@media` body holds two nested rules; the balanced-brace scan must
        // consume the whole group so the trailing `h1` rule outside it is still
        // parsed (a naive "first }" would stop mid-group and lose `h1`).
        let css = "@media print { p { color: red } span { color: lime } } \
                   h1 { color: blue }";
        let p = color_of_first(&format!("<style>{css}</style><p>x</p>"), "p");
        let h1 = color_of_first(&format!("<style>{css}</style><h1>x</h1>"), "h1");
        assert_eq!(p, [1.0, 0.0, 0.0], "nested print rule applies");
        assert_eq!(h1, [0.0, 0.0, 1.0], "rule after the @media group survives");
    }

    // ── CSS-2 quick-win 2: font shorthand + numeric weight ─────────────────

    #[test]
    fn font_weight_keeps_numeric_value_and_bold_flag() {
        // Numeric weights are preserved verbatim; `bold` flips at >= 600.
        let w300 = inline_style("font-weight: 300");
        assert_eq!(w300.font_weight, 300);
        assert!(!w300.bold, "300 is not bold");
        let w600 = inline_style("font-weight: 600");
        assert_eq!(w600.font_weight, 600);
        assert!(w600.bold, "600 is bold");
        // Named keywords map to canonical 400/700.
        assert_eq!(inline_style("font-weight: normal").font_weight, 400);
        let bold = inline_style("font-weight: bold");
        assert_eq!(bold.font_weight, 700);
        assert!(bold.bold);
        // Relative keywords step from the (default 400) weight.
        assert_eq!(inline_style("font-weight: bolder").font_weight, 700);
        assert_eq!(inline_style("font-weight: lighter").font_weight, 100);
    }

    #[test]
    fn font_shorthand_decomposes_into_longhands() {
        // `italic bold 18pt/24pt "Times New Roman"`: style, weight, size,
        // line-height and family all land in their longhands.
        let s = inline_style("font: italic bold 18pt/24pt \"Times New Roman\"");
        assert!(s.italic, "italic from the style token");
        assert!(
            s.bold && s.font_weight == 700,
            "bold weight from the prefix"
        );
        assert!(
            (s.font_size - 18.0).abs() < 0.01,
            "size 18pt ({})",
            s.font_size
        );
        // line-height: 24pt against an 18pt font → ratio ~1.333.
        assert!(
            (s.line_height - 24.0 / 18.0).abs() < 0.05,
            "line-height 24/18 ({})",
            s.line_height
        );
        assert_eq!(s.font_family, "Times New Roman");
        assert!(s.generic_serif, "Times → serif bucket");
    }

    #[test]
    fn font_shorthand_numeric_weight_and_reset() {
        // A numeric weight in the prefix; size without line-height; sans family.
        let s = inline_style("font: 600 14pt Arial");
        assert_eq!(s.font_weight, 600);
        assert!(s.bold);
        assert!((s.font_size - 14.0).abs() < 0.01);
        assert_eq!(s.font_family, "Arial");
        assert!(
            !s.generic_serif && !s.generic_mono,
            "Arial is the sans bucket"
        );
        // `font` resets the style/weight longhands first: a plain `font: 12pt x`
        // after an italic/bold context clears them.
        let mut st = Style::default();
        apply_one(&mut st, "font-style", "italic");
        apply_one(&mut st, "font-weight", "bold");
        apply_one(&mut st, "font", "12pt serif");
        assert!(!st.italic, "font shorthand reset italic");
        assert!(
            !st.bold && st.font_weight == 400,
            "font shorthand reset weight"
        );
    }

    #[test]
    fn font_shorthand_keyword_form_is_a_noop() {
        // System/keyword forms carry no size → leave the inherited font intact.
        let mut st = Style {
            font_size: 20.0,
            bold: true,
            font_weight: 700,
            ..Style::default()
        };
        apply_one(&mut st, "font", "inherit");
        assert!((st.font_size - 20.0).abs() < 0.01, "size unchanged");
        assert!(st.bold && st.font_weight == 700, "weight unchanged");
    }

    // ── CSS-2 quick-win 4: clear parsing ───────────────────────────────────

    #[test]
    fn clear_property_parses() {
        assert_eq!(inline_style("clear: left").clear, Clear::Left);
        assert_eq!(inline_style("clear: right").clear, Clear::Right);
        assert_eq!(inline_style("clear: both").clear, Clear::Both);
        assert_eq!(inline_style("clear: none").clear, Clear::None);
        // An unknown value is treated as `none`.
        assert_eq!(inline_style("clear: inline-start").clear, Clear::None);
        // `clear` is not inherited (resets to none).
        let parent = Style {
            clear: Clear::Both,
            ..Style::default()
        };
        assert_eq!(
            inherit(&parent).clear,
            Clear::None,
            "clear does not inherit"
        );
    }

    // ── CSS-2 quick-win 3: grid-column / grid-row start lines ───────────────

    #[test]
    fn grid_line_placement_parses() {
        // `grid-column`/`grid-row` (and their `-start` longhands) store a 1-based
        // start line; `grid-area: <row> / <col>` takes the first two lines.
        assert_eq!(inline_style("grid-column: 2").grid_col_start, 2);
        assert_eq!(inline_style("grid-row: 3").grid_row_start, 3);
        assert_eq!(inline_style("grid-column-start: 4").grid_col_start, 4);
        assert_eq!(inline_style("grid-row-start: 5").grid_row_start, 5);
        let area = inline_style("grid-area: 2 / 3");
        assert_eq!(area.grid_row_start, 2, "grid-area row");
        assert_eq!(area.grid_col_start, 3, "grid-area col");
        // `span N` / `auto` resolve to 0 (auto-flow).
        assert_eq!(inline_style("grid-column: span 2").grid_col_start, 0);
        assert_eq!(inline_style("grid-column: auto").grid_col_start, 0);
        // Not inherited.
        let parent = Style {
            grid_col_start: 2,
            grid_row_start: 3,
            ..Style::default()
        };
        let child = inherit(&parent);
        assert_eq!(
            child.grid_col_start, 0,
            "grid-column-start does not inherit"
        );
        assert_eq!(child.grid_row_start, 0, "grid-row-start does not inherit");
    }

    // ── grid-template track lists ──────────────────────────────────────────

    #[test]
    fn grid_template_columns_fr_tracks_parse() {
        let s = inline_style("display: grid; grid-template-columns: 1fr 2fr 1fr");
        assert_eq!(s.grid_columns, 3, "three fr tracks ⇒ 3 columns");
        assert_eq!(s.grid_template_columns.len(), 3);
        assert!(
            matches!(s.grid_template_columns[1], TrackSize::Fr(f) if (f - 2.0).abs() < 1e-9),
            "middle track is 2fr ({:?})",
            s.grid_template_columns[1]
        );
    }

    #[test]
    fn grid_template_columns_mixed_units_parse() {
        let s = inline_style("display: grid; grid-template-columns: 200px 1fr 25%");
        assert_eq!(s.grid_columns, 3);
        // 200px → 150pt (×0.75).
        assert!(
            matches!(s.grid_template_columns[0], TrackSize::Pt(p) if (p - 150.0).abs() < 0.5),
            "200px ⇒ 150pt ({:?})",
            s.grid_template_columns[0]
        );
        assert!(matches!(s.grid_template_columns[1], TrackSize::Fr(_)));
        assert!(
            matches!(s.grid_template_columns[2], TrackSize::Percent(p) if (p - 25.0).abs() < 1e-9),
            "third track is 25% ({:?})",
            s.grid_template_columns[2]
        );
    }

    #[test]
    fn grid_template_columns_repeat_expands() {
        let s = inline_style("display: grid; grid-template-columns: repeat(3, 1fr)");
        assert_eq!(s.grid_columns, 3, "repeat(3, 1fr) ⇒ 3 columns");
        assert!(s
            .grid_template_columns
            .iter()
            .all(|t| matches!(t, TrackSize::Fr(_))));

        // Nested repeat with a 2-track inner list expands to 4 tracks.
        let s2 = inline_style("display: grid; grid-template-columns: repeat(2, 100px 1fr)");
        assert_eq!(s2.grid_columns, 4, "repeat(2, 100px 1fr) ⇒ 4 columns");
    }

    #[test]
    fn grid_template_columns_minmax_parses() {
        let s = inline_style("display: grid; grid-template-columns: minmax(100px, 1fr) auto");
        assert_eq!(s.grid_columns, 2);
        match &s.grid_template_columns[0] {
            TrackSize::MinMax(min, max) => {
                assert!(matches!(**min, TrackSize::Pt(p) if (p - 75.0).abs() < 0.5));
                assert!(matches!(**max, TrackSize::Fr(_)));
            }
            other => panic!("expected minmax, got {other:?}"),
        }
        assert!(matches!(s.grid_template_columns[1], TrackSize::Auto));
    }

    #[test]
    fn grid_template_rows_store_explicit_heights() {
        let s = inline_style("display: grid; grid-template-rows: 40pt auto 60pt");
        assert_eq!(s.grid_rows, 3);
        assert!(matches!(s.grid_template_rows[0], TrackSize::Pt(p) if (p - 40.0).abs() < 1e-9));
        assert!(matches!(s.grid_template_rows[1], TrackSize::Auto));
        assert!(matches!(s.grid_template_rows[2], TrackSize::Pt(p) if (p - 60.0).abs() < 1e-9));
    }

    // ── grid-column / grid-row span ────────────────────────────────────────

    #[test]
    fn grid_column_span_parses() {
        assert_eq!(inline_style("grid-column: span 2").grid_col_span, 2);
        assert_eq!(
            inline_style("grid-column: span 3").grid_col_start,
            0,
            "span only ⇒ auto start"
        );
        // `start / end` line form ⇒ span = end − start.
        let s = inline_style("grid-column: 1 / 3");
        assert_eq!(s.grid_col_start, 1);
        assert_eq!(s.grid_col_span, 2, "lines 1..3 span 2 columns");
        // `start / span N`.
        let s2 = inline_style("grid-row: 2 / span 3");
        assert_eq!(s2.grid_row_start, 2);
        assert_eq!(s2.grid_row_span, 3);
        // Default span is 1.
        assert_eq!(inline_style("grid-column: 2").grid_col_span, 1);
    }

    #[test]
    fn grid_area_resolves_start_and_span() {
        let s = inline_style("grid-area: 1 / 1 / 3 / span 2");
        assert_eq!(s.grid_row_start, 1);
        assert_eq!(s.grid_col_start, 1);
        assert_eq!(s.grid_row_span, 2, "rows 1..3 ⇒ span 2");
        assert_eq!(s.grid_col_span, 2, "col span 2");
    }

    // ── flex shorthand: grow / shrink / basis ──────────────────────────────

    #[test]
    fn flex_shorthand_one_value_sets_grow_and_zero_basis() {
        // `flex: 1` ⇒ grow 1, shrink 1, basis 0.
        let s = inline_style("flex: 1");
        assert!((s.flex_grow - 1.0).abs() < 1e-9);
        assert!((s.flex_shrink - 1.0).abs() < 1e-9);
        assert!(
            matches!(s.flex_basis, Some(Len::Pt(b)) if b.abs() < 1e-9),
            "basis 0"
        );
    }

    #[test]
    fn flex_shorthand_three_values_split_correctly() {
        // `flex: 2 0 120pt` ⇒ grow 2, shrink 0, basis 120pt.
        let s = inline_style("flex: 2 0 120pt");
        assert!((s.flex_grow - 2.0).abs() < 1e-9);
        assert!((s.flex_shrink - 0.0).abs() < 1e-9);
        assert!(matches!(s.flex_basis, Some(Len::Pt(b)) if (b - 120.0).abs() < 1e-9));
    }

    #[test]
    fn flex_shorthand_keywords() {
        let none = inline_style("flex: none");
        assert!((none.flex_grow).abs() < 1e-9 && (none.flex_shrink).abs() < 1e-9);
        assert!(none.flex_basis.is_none(), "none ⇒ basis auto");
        let auto = inline_style("flex: auto");
        assert!((auto.flex_grow - 1.0).abs() < 1e-9 && (auto.flex_shrink - 1.0).abs() < 1e-9);
    }

    #[test]
    fn flex_basis_and_shrink_longhands() {
        assert!(matches!(
            inline_style("flex-basis: 200px").flex_basis,
            Some(Len::Pt(p)) if (p - 150.0).abs() < 0.5
        ));
        assert!(inline_style("flex-basis: auto").flex_basis.is_none());
        assert!((inline_style("flex-shrink: 0").flex_shrink).abs() < 1e-9);
        assert!((inline_style("flex-shrink: 3").flex_shrink - 3.0).abs() < 1e-9);
    }

    #[test]
    fn flex_fields_do_not_inherit() {
        let parent = Style {
            flex_grow: 3.0,
            flex_shrink: 0.0,
            flex_basis: Some(Len::Pt(50.0)),
            grid_col_span: 4,
            grid_template_columns: vec![TrackSize::Fr(1.0)],
            ..Style::default()
        };
        let child = inherit(&parent);
        assert!((child.flex_grow).abs() < 1e-9, "flex-grow resets");
        assert!(
            (child.flex_shrink - 1.0).abs() < 1e-9,
            "flex-shrink resets to 1"
        );
        assert!(child.flex_basis.is_none(), "flex-basis resets to auto");
        assert_eq!(child.grid_col_span, 1, "grid span resets to 1");
        assert!(child.grid_template_columns.is_empty(), "track list resets");
    }

    #[test]
    fn absolute_and_relative_length_units_resolve_to_points() {
        // 1in = 72pt; cm/mm/pc/q derive from it; ex/ch ≈ 0.5em (em = 10pt here).
        // Last two confirm the existing units are unchanged.
        for (input, expected) in [
            ("1in", 72.0),
            ("2.54cm", 72.0),
            ("25.4mm", 72.0),
            ("1pc", 12.0),
            ("40q", 72.0 / 2.54), // 40q = 1cm
            ("2ex", 10.0),
            ("2ch", 10.0),
            ("96px", 72.0),
            ("10pt", 10.0),
        ] {
            let got = parse_len_px(input, 10.0).unwrap();
            assert!(
                (got - expected).abs() < 1e-6,
                "{input} → {got} (want {expected})"
            );
        }
        // A keyword that merely ends in a unit ("thin") is not a length.
        assert!(parse_len_px("thin", 10.0).is_none());
    }

    #[test]
    fn current_color_resolves_to_the_element_color() {
        let close = |a: [f64; 3], b: [f64; 3]| a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-3);

        // `border-color: currentColor` picks up the cascaded `color`.
        let mut style = Style::default();
        apply_one(&mut style, "color", "rgb(20, 120, 220)");
        apply_one(&mut style, "border-color", "currentColor");
        assert!(
            close(style.border_color, style.color),
            "border-color follows color"
        );

        // …and as a sub-token of the `border` shorthand.
        let mut s2 = Style::default();
        apply_one(&mut s2, "color", "rgb(200, 60, 10)");
        apply_one(&mut s2, "border", "2px solid currentColor");
        assert!(
            close(s2.border_color, s2.color),
            "border shorthand currentColor"
        );

        // `background: currentColor` too (case-insensitive).
        let mut s3 = Style::default();
        apply_one(&mut s3, "color", "rgb(0, 255, 0)");
        apply_one(&mut s3, "background", "CurrentColor");
        assert_eq!(
            s3.background,
            Some([0.0, 1.0, 0.0]),
            "background follows color"
        );
    }

    #[test]
    fn colour_alpha_is_parsed_from_every_form() {
        let approx = |x: f64, y: f64| (x - y).abs() < 1e-6;
        // rgba / hsla 4th value (0..1), `#rgba` nibble, `#rrggbbaa` byte.
        let (_, a) = parse_color_alpha("rgba(255, 0, 0, 0.5)").unwrap();
        assert!(approx(a, 0.5), "rgba alpha");
        let (_, a) = parse_color_alpha("hsla(0, 100%, 50%, 0.25)").unwrap();
        assert!(approx(a, 0.25), "hsla alpha");
        let (_, a) = parse_color_alpha("#00000080").unwrap();
        assert!(approx((a * 255.0).round(), 128.0), "#rrggbbaa alpha (0x80)");
        let (rgb, a) = parse_color_alpha("#0f08").unwrap();
        assert_eq!(rgb, [0.0, 1.0, 0.0], "#rgba rgb");
        assert!(approx(a, 0x88 as f64 / 255.0), "#rgba alpha nibble");
        // Opaque forms default to alpha 1.0; `parse_color` still drops it.
        assert!(
            approx(parse_color_alpha("#0a0").unwrap().1, 1.0),
            "opaque hex"
        );
        assert_eq!(
            parse_color("rgba(1,2,3,0.5)"),
            Some([1.0 / 255.0, 2.0 / 255.0, 3.0 / 255.0])
        );
    }

    #[test]
    fn colour_alpha_folds_into_the_right_style_field() {
        let approx = |x: f64, y: f64| (x - y).abs() < 1e-6;
        let mut bg = Style::default();
        apply_one(&mut bg, "background", "rgba(0, 0, 0, 0.4)");
        assert!(approx(bg.background_alpha, 0.4), "background-alpha");
        let mut text = Style::default();
        apply_one(&mut text, "color", "rgba(0, 0, 0, 0.3)");
        assert!(approx(text.color_alpha, 0.3), "color-alpha");
        let mut bd = Style::default();
        apply_one(&mut bd, "border", "1px solid rgba(0, 0, 0, 0.6)");
        assert!(
            approx(bd.border_color_alpha, 0.6),
            "border-alpha (shorthand)"
        );
        let mut bc = Style::default();
        apply_one(&mut bc, "border-color", "rgba(0, 0, 0, 0.7)");
        assert!(
            approx(bc.border_color_alpha, 0.7),
            "border-alpha (border-color)"
        );
    }

    #[test]
    fn collect_all_css_urls_captures_every_url_in_order() {
        // A `src` with two alternatives + a data: URI whose `)`-free body keeps
        // paren-matching safe; `format(...)` tokens between them are skipped.
        let v = "url(a.woff2) format('woff2'), url('b.woff') format('woff'), \
                 url(data:font/ttf;base64,AAAB) format('truetype')";
        assert_eq!(
            collect_all_css_urls(v),
            vec![
                "a.woff2".to_string(),
                "b.woff".to_string(),
                "data:font/ttf;base64,AAAB".to_string(),
            ]
        );
        // The single-url helper still returns just the first.
        assert_eq!(extract_css_url(v).as_deref(), Some("a.woff2"));
        assert!(collect_all_css_urls("none").is_empty());
    }

    #[test]
    fn collect_font_face_rules_parses_family_srcs_weight_style() {
        let css = "@font-face { font-family: 'My Font'; font-weight: bold; \
                   font-style: italic; src: url(data:font/woff2;base64,ZZ) format('woff2'), \
                   url(/fonts/my.woff); } \
                   @font-face { font-family: Plain; src: url(p.ttf); }";
        let rules = collect_font_face_rules(css);
        assert_eq!(rules.len(), 2, "two @font-face blocks");

        let a = &rules[0];
        assert_eq!(a.family, "my font", "family de-quoted + lower-cased");
        assert_eq!(a.weight, 700, "bold → 700");
        assert!(a.italic, "italic style");
        assert_eq!(
            a.srcs,
            vec![
                "data:font/woff2;base64,ZZ".to_string(),
                "/fonts/my.woff".to_string(),
            ],
            "both src url()s captured in order (data: `;` not split)"
        );

        let b = &rules[1];
        assert_eq!(b.family, "plain");
        assert_eq!(b.weight, 400, "absent font-weight defaults to 400");
        assert!(!b.italic, "absent font-style is upright");
        assert_eq!(b.srcs, vec!["p.ttf".to_string()]);
    }

    #[test]
    fn font_face_weight_mapping() {
        let rule = |decl: &str| {
            collect_font_face_rules(&format!(
                "@font-face {{ font-family: F; {decl} src: url(x.ttf); }}"
            ))
            .remove(0)
        };
        assert_eq!(rule("font-weight: normal;").weight, 400);
        assert_eq!(rule("font-weight: bold;").weight, 700);
        assert_eq!(rule("font-weight: 300;").weight, 300);
        assert_eq!(rule("font-weight: 5000;").weight, 1000, "clamped to 1000");
        assert_eq!(rule("font-weight: 400 700;").weight, 400, "range → first");
        assert_eq!(rule("").weight, 400, "absent → 400");
        assert!(rule("font-style: oblique;").italic, "oblique → italic");
    }

    #[test]
    fn stylesheet_exposes_font_face_rules() {
        let sheet = Stylesheet::new(
            "@font-face { font-family: Embedded; src: url(data:font/ttf;base64,QQ); }",
        );
        // Both the legacy name list and the full-rule accessor see the family.
        assert_eq!(sheet.font_faces(), &["embedded".to_string()]);
        let rules = sheet.font_face_rules();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].family, "embedded");
        assert_eq!(rules[0].srcs, vec!["data:font/ttf;base64,QQ".to_string()]);
    }

    // ── custom properties (`var()`) ─────────────────────────────────────────

    /// Compute the style of the first `<tag>` element in `html`.
    fn computed_of(html: &str, tag: &str) -> Style {
        fn find<'a>(nodes: &'a [Node], tag: &str) -> Option<&'a Element> {
            for n in nodes {
                if let Node::Element(e) = n {
                    if e.tag == tag {
                        return Some(e);
                    }
                    if let Some(f) = find(&e.children, tag) {
                        return Some(f);
                    }
                }
            }
            None
        }
        fn chain<'a>(nodes: &'a [Node], tag: &str, acc: &mut Vec<&'a Element>) -> bool {
            for n in nodes {
                if let Node::Element(e) = n {
                    if e.tag == tag {
                        return true;
                    }
                    acc.push(e);
                    if chain(&e.children, tag, acc) {
                        return true;
                    }
                    acc.pop();
                }
            }
            false
        }
        let nodes = parse(html);
        let sheet = Stylesheet::new(&collect_style_css(&nodes));
        let el = find(&nodes, tag).expect("element present");
        let mut ancestors = Vec::new();
        chain(&nodes, tag, &mut ancestors);
        // Resolve parent styles down the chain so inheritance is realistic.
        let mut parent = Style::default();
        let mut walked: Vec<&Element> = Vec::new();
        for a in &ancestors {
            parent = sheet.computed(a, &parent, &walked);
            walked.push(a);
        }
        sheet.computed(el, &parent, &walked)
    }

    #[test]
    fn var_resolves_from_custom_property() {
        let s = computed_of(
            "<style>div { --brand: #ff0000; color: var(--brand); }</style><div>x</div>",
            "div",
        );
        assert_eq!(s.color, [1.0, 0.0, 0.0], "var(--brand) → red");
    }

    #[test]
    fn var_falls_back_when_unknown() {
        let s = computed_of(
            "<style>div { color: var(--missing, #00ff00); }</style><div>x</div>",
            "div",
        );
        assert_eq!(s.color, [0.0, 1.0, 0.0], "fallback used");
        // No fallback → the declaration is invalid: inherited/default stands.
        let s2 = computed_of(
            "<style>div { color: var(--missing); }</style><div>x</div>",
            "div",
        );
        assert_eq!(s2.color, [0.0, 0.0, 0.0], "invalid declaration skipped");
    }

    #[test]
    fn var_inherits_down_the_tree() {
        let s = computed_of(
            "<style>div { --pad: 10pt; } p { margin-top: var(--pad); }</style>\
             <div><p>x</p></div>",
            "p",
        );
        assert!(
            (s.margin.top - 10.0).abs() < 1e-9,
            "inherited custom property resolves on the child: {}",
            s.margin.top
        );
    }

    #[test]
    fn var_in_var_resolves_nested_references() {
        let s = computed_of(
            "<style>div { --a: var(--b); --b: 24pt; font-size: var(--a); }</style><div>x</div>",
            "div",
        );
        assert!(
            (s.font_size - 24.0).abs() < 1e-9,
            "var(--a) → var(--b) → 24pt: {}",
            s.font_size
        );
    }

    #[test]
    fn var_cycle_uses_fallback_or_skips() {
        // `--a` ↔ `--b` is a cycle: the reference is unresolvable, so the
        // fallback applies…
        let s = computed_of(
            "<style>div { --a: var(--b); --b: var(--a); font-size: var(--a, 30pt); }</style>\
             <div>x</div>",
            "div",
        );
        assert!((s.font_size - 30.0).abs() < 1e-9, "cycle → fallback");
        // …and with no fallback the declaration is skipped (default stands).
        let s2 = computed_of(
            "<style>div { --a: var(--b); --b: var(--a); font-size: var(--a); }</style>\
             <div>x</div>",
            "div",
        );
        assert!(
            (s2.font_size - 16.0).abs() < 1e-9,
            "cycle without fallback → declaration skipped (16pt root default kept), got {}",
            s2.font_size
        );
    }

    #[test]
    fn var_on_root_pseudo_class_reaches_descendants() {
        // The classic `:root { --x: … }` pattern must work end-to-end.
        let s = computed_of(
            "<style>:root { --c: #0000ff; } p { color: var(--c); }</style>\
             <html><body><p>x</p></body></html>",
            "p",
        );
        assert_eq!(s.color, [0.0, 0.0, 1.0], ":root custom property inherited");
    }

    // ── `::before` / `::after` generated content ────────────────────────────

    /// Inject pseudo-elements for `html` and return the transformed forest.
    fn injected(html: &str) -> Vec<Node> {
        let nodes = parse(html);
        let sheet = Stylesheet::new(&collect_style_css(&nodes));
        apply_pseudo_elements(&nodes, &sheet)
    }

    /// Collect the visible text of a forest in document order.
    fn forest_text(nodes: &[Node]) -> String {
        fn walk(nodes: &[Node], out: &mut String) {
            for n in nodes {
                match n {
                    Node::Text(t) => out.push_str(t),
                    Node::Element(e) => {
                        if e.tag != "style" {
                            walk(&e.children, out);
                        }
                    }
                }
            }
        }
        let mut s = String::new();
        walk(nodes, &mut s);
        s
    }

    #[test]
    fn before_and_after_content_strings_are_injected() {
        let out = injected(
            "<style>p::before { content: \">> \"; } p::after { content: \" <<\"; }</style>\
             <p>mid</p>",
        );
        assert_eq!(forest_text(&out), ">> mid <<");
        // The pseudo is a synthetic child with the rule's decls as inline style.
        let p = out
            .iter()
            .find_map(|n| match n {
                Node::Element(e) if e.tag == "p" => Some(e),
                _ => None,
            })
            .expect("p");
        let first = match &p.children[0] {
            Node::Element(e) => e,
            other => panic!("expected pseudo element first, got {other:?}"),
        };
        assert_eq!(first.tag, PSEUDO_ELEMENT_TAG);
    }

    #[test]
    fn content_attr_reads_the_host_attribute() {
        let out = injected(
            "<style>a::after { content: \" (\" attr(href) \")\"; }</style>\
             <a href=\"https://x.test\">link</a>",
        );
        assert_eq!(forest_text(&out), "link (https://x.test)");
    }

    #[test]
    fn counters_number_nested_ordered_lists() {
        // The standard authored pattern: reset on each <ol>, increment per <li>.
        let out = injected(
            "<style>ol { counter-reset: item; } \
             li::before { counter-increment: item; content: counters(item, \".\") \". \"; }\
             </style>\
             <ol><li>a</li><li>b<ol><li>b1</li><li>b2</li></ol></li><li>c</li></ol>",
        );
        let text = forest_text(&out);
        for expected in ["1. a", "2. b", "2.1. b1", "2.2. b2", "3. c"] {
            assert!(
                text.contains(expected),
                "hierarchical numbering has {expected:?}: {text:?}"
            );
        }
    }

    #[test]
    fn explicit_counter_reset_and_increment_values() {
        let out = injected(
            "<style>div { counter-reset: sec 10; } \
             h2::before { counter-increment: sec 5; content: counter(sec) \" — \"; }</style>\
             <div><h2>alpha</h2><h2>beta</h2></div>",
        );
        let text = forest_text(&out);
        assert!(text.contains("15 — alpha"), "10 + 5: {text:?}");
        assert!(text.contains("20 — beta"), "15 + 5: {text:?}");
    }

    #[test]
    fn resolve_content_never_panics_on_garbage() {
        let el = Element {
            tag: "p".into(),
            ..Default::default()
        };
        let counters = CounterSet::default();
        for v in [
            "\"unterminated",
            "\"esc \\é end\"",  // escaped multibyte char
            "\"line\\Abreak\"", // \A newline escape
            "counter(",
            "counters(a",
            "attr()",
            "url(x.png) \"y\"",
            "open-quote close-quote",
            "🎉 \"emoji\"",
        ] {
            let _ = resolve_content(v, &el, &counters); // must not panic
        }
        assert_eq!(
            resolve_content("\"a\\Ab\"", &el, &counters).as_deref(),
            Some("a\nb"),
            "\\A is the CSS newline escape"
        );
        assert_eq!(
            resolve_content("\"e \\é\"", &el, &counters).as_deref(),
            Some("e é"),
            "escaped multibyte char copied verbatim"
        );
    }

    #[test]
    fn content_none_and_no_rule_generate_nothing() {
        let out = injected("<style>p::before { content: none; }</style><p>x</p>");
        assert_eq!(forest_text(&out), "x");
        // Empty-string content still generates the (empty) box — the clearfix.
        let out2 = injected("<style>p::after { content: \"\"; display: block; }</style><p>x</p>");
        let p = out2
            .iter()
            .find_map(|n| match n {
                Node::Element(e) if e.tag == "p" => Some(e),
                _ => None,
            })
            .expect("p");
        assert!(
            matches!(p.children.last(), Some(Node::Element(e)) if e.tag == PSEUDO_ELEMENT_TAG),
            "content:\"\" still injects the box"
        );
    }

    #[test]
    fn injected_pseudo_does_not_shift_first_child() {
        // A `::before` on the parent must not steal the first real child's
        // `:first-child` match.
        let out = injected(
            "<style>ul::before { content: \"L: \"; } li:first-child { color: #ff0000; }</style>\
             <ul><li>one</li><li>two</li></ul>",
        );
        let sheet = Stylesheet::new(
            "ul::before { content: \"L: \"; } li:first-child { color: #ff0000; }",
        );
        let ul = out
            .iter()
            .find_map(|n| match n {
                Node::Element(e) if e.tag == "ul" => Some(e),
                _ => None,
            })
            .expect("ul");
        let lis: Vec<&Element> = ul
            .children
            .iter()
            .filter_map(|n| match n {
                Node::Element(e) if e.tag == "li" => Some(e),
                _ => None,
            })
            .collect();
        let first = sheet.computed(lis[0], &Style::default(), &[ul]);
        let second = sheet.computed(lis[1], &Style::default(), &[ul]);
        assert_eq!(first.color, [1.0, 0.0, 0.0], "real first li still matches");
        assert_eq!(second.color, [0.0, 0.0, 0.0], "second li does not");
    }

    // ── `@page` ─────────────────────────────────────────────────────────────

    #[test]
    fn page_rule_named_size_and_orientation() {
        let rule = collect_page_rule("@page { size: A4 landscape; }").expect("rule");
        let (w, h) = rule.size.expect("size");
        assert!(w > h, "landscape is wider than tall");
        assert!((h - 595.276).abs() < 0.1 && (w - 841.890).abs() < 0.1, "A4 swapped");
        let portrait = collect_page_rule("@page { size: letter; }").unwrap();
        assert_eq!(portrait.size, Some((612.0, 792.0)));
    }

    #[test]
    fn page_rule_custom_size_lengths() {
        let rule = collect_page_rule("@page { size: 400pt 500pt; }").unwrap();
        assert_eq!(rule.size, Some((400.0, 500.0)));
        // A single length is a square page, per spec.
        let sq = collect_page_rule("@page { size: 10cm; }").unwrap();
        let (w, h) = sq.size.unwrap();
        assert!((w - h).abs() < 1e-9 && (w - 10.0 * 72.0 / 2.54).abs() < 0.01);
    }

    #[test]
    fn page_rule_margins_shorthand_and_longhands() {
        let rule = collect_page_rule("@page { margin: 1in 2in; }").unwrap();
        assert_eq!(rule.margins, [Some(72.0), Some(144.0), Some(72.0), Some(144.0)]);
        let long = collect_page_rule("@page { margin-top: 10pt; margin-left: 20pt; }").unwrap();
        assert_eq!(long.margins, [Some(10.0), None, None, Some(20.0)]);
        // No @page at all → None.
        assert!(collect_page_rule("p { color: red }").is_none());
    }

    // ── gradient `transparent` stops ────────────────────────────────────────

    #[test]
    fn transparent_gradient_stop_is_kept_with_alpha_zero() {
        let s = inline_style("background: linear-gradient(red, transparent)");
        let g = as_linear(&s);
        assert_eq!(g.stops.len(), 2, "transparent stop is NOT dropped");
        assert!((g.stops[0].alpha - 1.0).abs() < 1e-9, "red is opaque");
        assert!(g.stops[1].alpha.abs() < 1e-9, "transparent stop alpha 0");
        // The transparent stop borrows its neighbour's colour so the ramp
        // fades red out in place instead of towards black.
        assert_eq!(g.stops[1].color, [1.0, 0.0, 0.0]);
    }

    #[test]
    fn rgba_gradient_stop_carries_its_alpha() {
        let s = inline_style("background: linear-gradient(rgba(0,0,255,0.5), yellow)");
        let g = as_linear(&s);
        assert!((g.stops[0].alpha - 0.5).abs() < 1e-9, "rgba alpha kept");
        assert!((g.stops[1].alpha - 1.0).abs() < 1e-9);
    }

    // ── `white-space` / overflow properties / `object-fit` / `max-height` ──

    #[test]
    fn white_space_values_parse_and_track_the_pre_flag() {
        for (v, ws, pre) in [
            ("normal", WhiteSpace::Normal, false),
            ("nowrap", WhiteSpace::Nowrap, false),
            ("pre", WhiteSpace::Pre, true),
            ("pre-wrap", WhiteSpace::PreWrap, true),
            ("pre-line", WhiteSpace::PreLine, false),
        ] {
            let s = inline_style(&format!("white-space: {v}"));
            assert_eq!(s.white_space, ws, "white-space: {v}");
            assert_eq!(s.pre, pre, "legacy pre flag for {v}");
        }
    }

    #[test]
    fn word_break_overflow_wrap_and_text_overflow_parse() {
        assert!(inline_style("word-break: break-all").break_word);
        assert!(inline_style("overflow-wrap: break-word").break_word);
        assert!(inline_style("word-wrap: break-word").break_word, "legacy alias");
        assert!(!inline_style("word-break: keep-all").break_word);
        assert!(inline_style("text-overflow: ellipsis").text_overflow_ellipsis);
        assert!(!inline_style("text-overflow: clip").text_overflow_ellipsis);
    }

    #[test]
    fn object_fit_parses_every_mode() {
        for (v, want) in [
            ("fill", ObjectFit::Fill),
            ("contain", ObjectFit::Contain),
            ("cover", ObjectFit::Cover),
            ("none", ObjectFit::None),
            ("scale-down", ObjectFit::ScaleDown),
        ] {
            assert_eq!(inline_style(&format!("object-fit: {v}")).object_fit, want);
        }
    }

    #[test]
    fn max_height_parses_and_none_clears() {
        let s = inline_style("max-height: 100pt");
        assert_eq!(s.max_height, Some(100.0));
        assert_eq!(inline_style("max-height: none").max_height, None);
    }

    #[test]
    fn vertical_align_line_relative_values_parse() {
        assert_eq!(inline_style("vertical-align: middle").valign, VShift::Middle);
        assert_eq!(inline_style("vertical-align: top").valign, VShift::Top);
        assert_eq!(inline_style("vertical-align: bottom").valign, VShift::Bottom);
        // Lengths keep resolving to points (positive = raise).
        assert_eq!(
            inline_style("vertical-align: 3pt").valign,
            VShift::Points(3.0)
        );
    }

    // ── `:not()` / `:nth-of-type` / unknown pseudo-classes ─────────────────

    #[test]
    fn not_pseudo_class_excludes_matching_elements() {
        let html = "<style>p:not(.a) { color: #ff0000 }</style>\
                    <div><p class=\"a\">skip</p><p>hit</p></div>";
        let nodes = parse(html);
        let sheet = Stylesheet::new(&collect_style_css(&nodes));
        let div = nodes
            .iter()
            .find_map(|n| match n {
                Node::Element(e) if e.tag == "div" => Some(e),
                _ => None,
            })
            .unwrap();
        let ps: Vec<&Element> = div
            .children
            .iter()
            .filter_map(|n| match n {
                Node::Element(e) => Some(e),
                _ => None,
            })
            .collect();
        let skip = sheet.computed(ps[0], &Style::default(), &[div]);
        let hit = sheet.computed(ps[1], &Style::default(), &[div]);
        assert_eq!(skip.color, [0.0, 0.0, 0.0], ":not(.a) excludes .a");
        assert_eq!(hit.color, [1.0, 0.0, 0.0], ":not(.a) matches the other");
    }

    #[test]
    fn nth_of_type_counts_same_tag_siblings_only() {
        // <h1> then three <p>: `p:nth-of-type(2n)` must hit ONLY the 2nd <p>
        // (with nth-child semantics the h1 would shift every index by one).
        let html = "<style>p:nth-of-type(2n) { color: #ff0000 }</style>\
                    <div><h1>t</h1><p>p1</p><p>p2</p><p>p3</p></div>";
        let nodes = parse(html);
        let sheet = Stylesheet::new(&collect_style_css(&nodes));
        let div = nodes
            .iter()
            .find_map(|n| match n {
                Node::Element(e) if e.tag == "div" => Some(e),
                _ => None,
            })
            .unwrap();
        let ps: Vec<&Element> = div
            .children
            .iter()
            .filter_map(|n| match n {
                Node::Element(e) if e.tag == "p" => Some(e),
                _ => None,
            })
            .collect();
        let colors: Vec<[f64; 3]> = ps
            .iter()
            .map(|p| sheet.computed(p, &Style::default(), &[div]).color)
            .collect();
        assert_eq!(colors[0], [0.0, 0.0, 0.0], "p1 is 1st of type");
        assert_eq!(colors[1], [1.0, 0.0, 0.0], "p2 is 2nd of type → match");
        assert_eq!(colors[2], [0.0, 0.0, 0.0], "p3 is 3rd of type");
    }

    #[test]
    fn unknown_pseudo_class_makes_the_rule_inert() {
        // Standalone unknown pseudo: never matches.
        let s = computed_of(
            "<style>p:figment-of-css { color: #ff0000 }</style><p>x</p>",
            "p",
        );
        assert_eq!(s.color, [0.0, 0.0, 0.0], "unknown pseudo → inert rule");
        // Unknown pseudo INSIDE :not(…) must not flip into an over-match.
        let s2 = computed_of(
            "<style>p:not(:figment-of-css) { color: #ff0000 }</style><p>x</p>",
            "p",
        );
        assert_eq!(s2.color, [0.0, 0.0, 0.0], ":not(unknown) → inert, not true");
    }

    // ── `font-family` fallback list ─────────────────────────────────────────

    #[test]
    fn font_family_keeps_the_full_fallback_list() {
        let s = inline_style("font-family: \"PoliceInexistante\", serif");
        assert_eq!(s.font_family, "PoliceInexistante", "first name kept");
        assert_eq!(
            s.font_families,
            vec!["PoliceInexistante".to_string(), "serif".to_string()]
        );
        // The unknown first name doesn't hide the generic fallback intent.
        assert!(s.generic_serif, "serif fallback drives the generic flag");
        // A recognised sans first name stops the scan (flags stay off).
        let s2 = inline_style("font-family: Arial, serif");
        assert!(!s2.generic_serif && !s2.generic_mono);
        // Single-name behaviour is unchanged.
        let s3 = inline_style("font-family: Courier");
        assert!(s3.generic_mono);
    }

    // ── `vw`/`vh` against the live page size ────────────────────────────────

    #[test]
    fn vw_vh_resolve_against_installed_viewport() {
        // Reference viewport (no layout running): the historical values.
        assert_eq!(parse_len_px("50vw", 12.0), Some(VIEWPORT_W_PT / 2.0));
        assert_eq!(parse_len_px("50vh", 12.0), Some(VIEWPORT_H_PT / 2.0));
        {
            let _vp = set_viewport_for_layout(400.0, 1000.0);
            assert_eq!(parse_len_px("50vw", 12.0), Some(200.0), "live width");
            assert_eq!(parse_len_px("10vh", 12.0), Some(100.0), "live height");
        }
        // The guard restored the reference viewport.
        assert_eq!(parse_len_px("50vw", 12.0), Some(VIEWPORT_W_PT / 2.0));
        // An absurd (synthetic band) height is ignored, keeping the fallback.
        {
            let _vp = set_viewport_for_layout(500.0, 1.0e6);
            assert_eq!(parse_len_px("100vw", 12.0), Some(500.0));
            assert_eq!(parse_len_px("100vh", 12.0), Some(VIEWPORT_H_PT));
        }
    }

    // ── `transform-origin` / `caption-side` ────────────────────────────────

    #[test]
    fn transform_origin_parses_keywords_lengths_and_percentages() {
        let get = |v: &str| inline_style(&format!("transform-origin: {v}")).transform_origin;
        // Keywords, both orders.
        let (x, y) = get("top left").expect("parsed");
        assert!(matches!(x, Len::Percent(p) if p == 0.0), "left → 0% x");
        assert!(matches!(y, Len::Percent(p) if p == 0.0), "top → 0% y");
        let (x2, y2) = get("left top").expect("parsed");
        assert!(matches!(x2, Len::Percent(p) if p == 0.0));
        assert!(matches!(y2, Len::Percent(p) if p == 0.0));
        // One value: x set, y defaults to center; a lone vertical keyword is y.
        let (x3, y3) = get("right").unwrap();
        assert!(matches!(x3, Len::Percent(p) if p == 100.0));
        assert!(matches!(y3, Len::Percent(p) if p == 50.0));
        let (x4, y4) = get("bottom").unwrap();
        assert!(matches!(x4, Len::Percent(p) if p == 50.0));
        assert!(matches!(y4, Len::Percent(p) if p == 100.0));
        // Lengths and percentages stay positional (x then y).
        let (x5, y5) = get("10pt 25%").unwrap();
        assert!(matches!(x5, Len::Pt(p) if (p - 10.0).abs() < 1e-9));
        assert!(matches!(y5, Len::Percent(p) if p == 25.0));
        // Initial value: no declaration → None (centre pivot).
        assert!(inline_style("color:red").transform_origin.is_none());
    }

    #[test]
    fn caption_side_parses_and_inherits() {
        assert_eq!(
            inline_style("caption-side: bottom").caption_side,
            CaptionSide::Bottom
        );
        assert_eq!(
            inline_style("caption-side: top").caption_side,
            CaptionSide::Top
        );
        // Inherited: declared on the table, visible on the caption.
        let s = computed_of(
            "<style>table { caption-side: bottom }</style>\
             <table><caption>c</caption></table>",
            "caption",
        );
        assert_eq!(s.caption_side, CaptionSide::Bottom);
    }

    // ── nested opacity composition ──────────────────────────────────────────

    #[test]
    fn nested_opacity_composes_multiplicatively() {
        let s = computed_of(
            "<style>div { opacity: 0.5 } p { opacity: 0.5 }</style><div><p>x</p></div>",
            "p",
        );
        assert!(
            (s.opacity - 0.25).abs() < 1e-9,
            "0.5 × 0.5 = 0.25 effective, got {}",
            s.opacity
        );
        // A child WITHOUT its own opacity inherits the parent's effective value.
        let s2 = computed_of(
            "<style>div { opacity: 0.5 }</style><div><p>x</p></div>",
            "p",
        );
        assert!((s2.opacity - 0.5).abs() < 1e-9);
        // Within one element the LAST declaration wins (no double-compose).
        let s3 = computed_of(
            "<style>p { opacity: 0.8 } p { opacity: 0.5 }</style><p>x</p>",
            "p",
        );
        assert!(
            (s3.opacity - 0.5).abs() < 1e-9,
            "last declaration wins, not 0.4: {}",
            s3.opacity
        );
    }

    // ── calc() expression parser (chained ops, parentheses, mixed units) ──

    #[test]
    fn calc_chained_operations_with_precedence() {
        // `*` binds tighter than `+`: 10 + 2·20 + 5 = 55pt.
        let s = inline_style("margin-top: calc(10pt + 2*20pt + 5pt)");
        assert!(
            (s.margin.top - 55.0).abs() < 1e-6,
            "chained +/* with precedence: {}",
            s.margin.top
        );
    }

    #[test]
    fn calc_parentheses_and_division() {
        // (10 + 20) / 3 = 10pt — the parens beat the natural precedence.
        let s = inline_style("margin-top: calc((10pt + 20pt)/3)");
        assert!((s.margin.top - 10.0).abs() < 1e-6, "{}", s.margin.top);
    }

    #[test]
    fn calc_mixed_percent_and_points_stays_symbolic() {
        // width: calc(100% - 2*20pt + 5pt) → Len::Calc { pct: 100, pt: -35 },
        // resolved against the CONTAINER at layout time (not em).
        let s = inline_style("width: calc(100% - 2*20pt + 5pt)");
        match s.width {
            Some(Len::Calc { pt, pct }) => {
                assert!((pt + 35.0).abs() < 1e-6, "pt part: {pt}");
                assert!((pct - 100.0).abs() < 1e-6, "pct part: {pct}");
            }
            other => panic!("expected a mixed calc, got {other:?}"),
        }
        // Resolution against a 200pt base: 200 − 35 = 165.
        assert!((s.width.unwrap().resolve(200.0) - 165.0).abs() < 1e-6);
        // The (100% − 40pt)/3 form divides BOTH components.
        let thirds = inline_style("width: calc((100% - 40pt)/3)");
        match thirds.width {
            Some(Len::Calc { pt, pct }) => {
                assert!((pct - 100.0 / 3.0).abs() < 1e-6, "pct third: {pct}");
                assert!((pt + 40.0 / 3.0).abs() < 1e-6, "pt third: {pt}");
            }
            other => panic!("expected thirds calc, got {other:?}"),
        }
    }

    #[test]
    fn calc_nested_calc_and_pure_forms_normalise() {
        // Nested calc() evaluates in place; a pure-% result is a plain Percent.
        let s = inline_style("width: calc(calc(50% + 25%))");
        assert!(
            matches!(s.width, Some(Len::Percent(p)) if (p - 75.0).abs() < 1e-6),
            "nested pure-percent normalises to Len::Percent: {:?}",
            s.width
        );
        // A pure-pt result is a plain Pt.
        let s2 = inline_style("width: calc(2*(10pt + 5pt))");
        assert!(
            matches!(s2.width, Some(Len::Pt(p)) if (p - 30.0).abs() < 1e-6),
            "pure-pt normalises to Len::Pt: {:?}",
            s2.width
        );
    }

    #[test]
    fn calc_invalid_expressions_are_ignored() {
        // Division BY a length is invalid.
        assert!(inline_style("width: calc(100pt / 2pt)").width.is_none());
        // length × length would be an area — invalid.
        assert!(inline_style("width: calc(10pt * 2pt)").width.is_none());
        // Unbalanced parenthesis.
        assert!(inline_style("width: calc((10pt + 5pt)").width.is_none());
        // number + length has no meaning.
        assert!(inline_style("width: calc(5 + 10pt)").width.is_none());
        // Division by zero.
        assert!(inline_style("width: calc(10pt / 0)").width.is_none());
    }

    // ── writing-mode ──

    #[test]
    fn writing_mode_parses_vertical_variants() {
        assert_eq!(
            inline_style("writing-mode: vertical-rl").writing_mode,
            WritingMode::VerticalRl
        );
        assert_eq!(
            inline_style("writing-mode: vertical-lr").writing_mode,
            WritingMode::VerticalLr
        );
        assert_eq!(
            inline_style("writing-mode: horizontal-tb").writing_mode,
            WritingMode::HorizontalTb
        );
        // Unknown values keep the default.
        assert_eq!(
            inline_style("writing-mode: bogus").writing_mode,
            WritingMode::HorizontalTb
        );
    }
}
