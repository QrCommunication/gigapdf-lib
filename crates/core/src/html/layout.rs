//! Box-tree layout: turn a styled DOM into positioned fragments across pages.
//!
//! Implements a real (if pragmatic) CSS visual formatting model: the block
//! formatting context stacks block boxes vertically honouring the box model
//! (margin / border / padding / background), and the inline formatting context
//! flows text + inline boxes into line boxes, breaking lines using **actual font
//! metrics** supplied by [`Measure`] (the paint layer plugs in embedded Google
//! fonts). Lists get markers, tables lay cells side-by-side, and the whole flow
//! is sliced into pages with backgrounds/borders split across page bands.

use super::css::{
    Align, AlignItems, BorderStyle, Clear, CssGradient, Direction, Display, FloatSide, Justify,
    Len, Position, Style, Stylesheet, TrackSize, VAlign,
};
use super::dom::{Element, Node};
use crate::svg::SvgImage;

/// Text-measurement hook. The paint layer implements this over the embedded
/// TrueType fonts (real advance widths); [`AverageMeasure`] is the fallback.
pub trait Measure {
    /// Advance width of `text` in points for the given computed style.
    fn width(&self, text: &str, style: &Style) -> f64;
}

/// A positioned output fragment in absolute top-down points (pre-pagination).
// `Text` legitimately carries the full computed `Style`, so it's larger than the
// geometric variants; these fragments are transient render output, so the size
// asymmetry is acceptable rather than boxing every text run.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum Fragment {
    Text {
        x: f64,
        y: f64,
        /// Measured advance width of the run in points. Gives the run a real
        /// horizontal extent so `overflow: hidden|clip` can cut text that
        /// straddles a box edge (without it a run would be a zero-width point and
        /// never register as overflowing).
        w: f64,
        style: Style,
        text: String,
    },
    Rect {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        fill: Option<[f64; 3]>,
        stroke: Option<[f64; 3]>,
        stroke_w: f64,
        /// `opacity` (0..=1) applied to the fill and stroke.
        opacity: f64,
        /// `border-radius` **horizontal** corner radii `[tl, tr, br, bl]` in
        /// points. All `0` (the common case) ⇒ the painter emits a plain
        /// rectangle, byte-for-byte as before; any non-zero radius ⇒ the
        /// fill/stroke follow a rounded contour (real Bézier corners).
        radius: [f64; 4],
        /// `border-radius` **vertical** corner radii `[tl, tr, br, bl]` in points.
        /// Equal to `radius` for circular corners (the default); differing values
        /// give elliptical corners. Ignored when `radius` is all-zero.
        radius_v: [f64; 4],
        /// Optional drop shadow painted *behind* this rect (offset + blur).
        /// `None` ⇒ nothing extra is drawn.
        shadow: Option<super::css::BoxShadow>,
    },
    Image {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        src: String,
    },
    /// A vector SVG placed at `(x, y)` with size `w×h` (top-down), drawn as
    /// native PDF paths (not rasterized) by the paint layer.
    Svg {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        image: SvgImage,
    },
    /// One styled border side, drawn by the paint layer according to its
    /// [`BorderStyle`]: a `dashed`/`dotted` run of segments, a `double` pair of
    /// thin lines. `(x, y, w, h)` is the side's *band* (top-down): a horizontal
    /// side is a `w`-wide strip `h = width` tall; a vertical side is an `h`-tall
    /// strip `w = width` wide. `Solid` sides never become this fragment — they
    /// stay plain filled [`Fragment::Rect`]s, so existing output is byte-identical.
    Border {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        /// `true` for top/bottom sides (run left→right), `false` for left/right
        /// (run top→bottom). Tells the painter which way dashes march.
        horizontal: bool,
        width: f64,
        color: [f64; 3],
        style: BorderStyle,
        opacity: f64,
    },
    /// A CSS gradient background filling the box `(x, y, w, h)` (top-down):
    /// `linear`/`radial` as a true PDF shading clipped to the box, `conic` as a
    /// fan of flat-coloured vector sectors. A box with no gradient never produces
    /// this fragment, so the flat-fill path is unchanged.
    Gradient {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        gradient: CssGradient,
        opacity: f64,
    },
    /// `inner` clipped to `rect` (`[x, y, w, h]`, top-down) — the painter wraps
    /// it in `q … re W n … Q`, so content straddling an `overflow: hidden|clip`
    /// box edge is actually cut (ISO 32000-1 §8.5.4). Nesting (`Clipped` inside
    /// `Clipped`) intersects the clips.
    Clipped {
        rect: [f64; 4],
        inner: Box<Fragment>,
    },
}

impl Fragment {
    /// A throwaway zero-area fragment, used only as a `mem::replace` placeholder
    /// while re-wrapping a fragment in [`Fragment::Clipped`] (never painted).
    fn placeholder() -> Fragment {
        Fragment::Rect {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
            fill: None,
            stroke: None,
            stroke_w: 0.0,
            opacity: 0.0,
            radius: [0.0; 4],
            radius_v: [0.0; 4],
            shadow: None,
        }
    }
}

#[derive(Debug, Clone)]
struct Abs {
    /// z=0 backgrounds/borders, z=1 content (text/images) — paint order within
    /// a stacking level.
    z: u8,
    /// CSS `z-index` stacking order (higher paints later). Positioned subtrees
    /// stamp their `z-index` here so they paint above/below in-flow content
    /// (which stays at 0).
    zi: i32,
    frag: Fragment,
}

/// The laid-out document: fragments grouped per page (top-down points).
#[derive(Debug, Clone)]
pub struct Layout {
    pub pages: Vec<Vec<Fragment>>,
    pub page_w: f64,
    pub page_h: f64,
}

/// The page box plus the content insets (per-side margins), in points.
#[derive(Debug, Clone, Copy)]
pub struct Frame {
    pub page_w: f64,
    pub page_h: f64,
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
    pub left: f64,
}

/// Lay out `nodes` onto pages of `page_w`×`page_h` with a uniform `margin`
/// around the content area (convenience wrapper over [`layout_document_framed`]).
pub fn layout_document(
    nodes: &[Node],
    sheet: &Stylesheet,
    measure: &dyn Measure,
    page_w: f64,
    page_h: f64,
    margin: f64,
) -> Layout {
    layout_document_framed(
        nodes,
        sheet,
        measure,
        &Frame {
            page_w,
            page_h,
            top: margin,
            right: margin,
            bottom: margin,
            left: margin,
        },
    )
}

/// Lay out `nodes` into the content box described by `frame` (per-side margins),
/// paginating the body between `frame.top` and `page_h - frame.bottom`.
pub fn layout_document_framed(
    nodes: &[Node],
    sheet: &Stylesheet,
    measure: &dyn Measure,
    frame: &Frame,
) -> Layout {
    let content_w = (frame.page_w - frame.left - frame.right).max(1.0);
    let page_cb = Cb {
        x: frame.left,
        y: frame.top,
        w: content_w,
        h: (frame.page_h - frame.top - frame.bottom).max(1.0),
    };
    let mut flow = Flow {
        out: Vec::new(),
        m: measure,
        sheet,
        page_h: frame.page_h,
        top: frame.top,
        bottom: frame.bottom,
        page_cb,
        cb: page_cb,
        flow_cb: page_cb,
        floats: FloatCtx::default(),
    };
    // Find <body> if present, else lay out the whole forest.
    let roots = find_body(nodes).unwrap_or(nodes);
    let root_style = Style {
        display: Display::Block,
        ..Style::default()
    };
    let mut y = frame.top;
    y = flow.block_children(roots, &root_style, frame.left, content_w, y, &[]);
    let _ = y;

    Layout {
        pages: paginate(flow.out, frame.page_h, frame.top, frame.bottom),
        page_w: frame.page_w,
        page_h: frame.page_h,
    }
}

fn find_body(nodes: &[Node]) -> Option<&[Node]> {
    for n in nodes {
        if let Node::Element(e) = n {
            if e.tag == "body" {
                return Some(&e.children);
            }
            if e.tag == "html" {
                if let Some(b) = find_body(&e.children) {
                    return Some(b);
                }
            }
        }
    }
    None
}

/// A containing-block rectangle (the reference box absolute children resolve
/// their `inset` against), in absolute top-down points.
#[derive(Debug, Clone, Copy)]
struct Cb {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

/// A placed float box: its side and the band `[top, bottom)` it occupies, plus
/// the inline `width` it steals from that band. Inline lines overlapping the
/// band are narrowed (and left-floats also shift the line start right).
#[derive(Debug, Clone, Copy)]
struct FloatBox {
    left: bool,
    top: f64,
    bottom: f64,
    width: f64,
}

/// The active floats inside the current block container. Reset per container so
/// floats don't leak across block boundaries (a pragmatic clearing model).
#[derive(Debug, Clone, Default)]
struct FloatCtx {
    boxes: Vec<FloatBox>,
}

/// A CSS-grid item's resolved cell: which item (index into the item list) sits
/// at which 0-based `(row, col)` of a fixed-column `display: grid`, and how many
/// columns/rows it spans (`col_span`/`row_span` ≥ 1). (Distinct from the
/// `<table>` `GridCell`, which models colspan/rowspan cells.)
#[derive(Debug, Clone, Copy)]
struct GridPlace {
    item: usize,
    row: usize,
    col: usize,
    col_span: usize,
    row_span: usize,
}

impl FloatCtx {
    /// Left and right inline insets to apply to a line spanning `[y, y+h)`:
    /// the summed widths of left- and right-floats overlapping that band.
    fn insets(&self, y: f64, h: f64) -> (f64, f64) {
        let (mut l, mut r) = (0.0, 0.0);
        let line_bottom = y + h;
        for f in &self.boxes {
            // Overlap test (a line touching the float's band is affected).
            if f.top < line_bottom && y < f.bottom {
                if f.left {
                    l += f.width;
                } else {
                    r += f.width;
                }
            }
        }
        (l, r)
    }

    /// The lowest bottom among placed floats (for clearing after a block).
    fn max_bottom(&self) -> f64 {
        self.boxes.iter().map(|f| f.bottom).fold(0.0_f64, f64::max)
    }

    /// The lowest bottom among floats on the side(s) selected by `clear`, for
    /// honouring `clear: left|right|both` on a block: the block's top is pushed
    /// down to at least this value. `Clear::None` yields `0.0` (no clearance).
    fn clear_bottom(&self, clear: Clear) -> f64 {
        self.boxes
            .iter()
            .filter(|f| match clear {
                Clear::Left => f.left,
                Clear::Right => !f.left,
                Clear::Both => true,
                Clear::None => false,
            })
            .map(|f| f.bottom)
            .fold(0.0_f64, f64::max)
    }
}

struct Flow<'a> {
    out: Vec<Abs>,
    m: &'a dyn Measure,
    sheet: &'a Stylesheet,
    /// Page height (for resolving `page-break-*` to the next page boundary).
    page_h: f64,
    /// Content-area top inset (page `margin-top`); the body band starts here.
    top: f64,
    /// Content-area bottom inset (page `margin-bottom`).
    bottom: f64,
    /// The page content box (margins applied) — the containing block for
    /// `position: fixed` (and the initial containing block for `absolute`).
    page_cb: Cb,
    /// The current containing block for `position: absolute` (the nearest
    /// positioned ancestor's content box). Saved/restored around positioned
    /// blocks.
    cb: Cb,
    /// The **enclosing block container's** content box (the nearest block
    /// ancestor, positioned or not). Used to clamp `position: sticky` offsets so
    /// a sticky box can't leave its parent — the static, scroll-free model. Every
    /// `block` updates this to its own content box around its children (its height
    /// taken from a declared `height`/`min-height`, else the remaining page band).
    flow_cb: Cb,
    /// Floats active in the current block container (narrow inline lines that
    /// overlap their vertical band). Saved/restored per container.
    floats: FloatCtx,
}

impl Flow<'_> {
    /// Advance `y` to the start of the next page (for `page-break-*: always`).
    /// A `y` already at a page boundary is left unchanged.
    fn break_to_next_page(&self, y: f64) -> f64 {
        let content_h = (self.page_h - self.top - self.bottom).max(1.0);
        let rel = (y - self.top).max(0.0);
        let next_k = (rel / content_h - 1e-9).ceil().max(0.0);
        self.top + next_k * content_h
    }

    /// Whether a block occupying `[top_y, bottom_y]` (absolute, flow-space)
    /// spans more than one page band — i.e. a page boundary cuts through it.
    /// Used by `page-break-inside: avoid`.
    fn crosses_page_break(&self, top_y: f64, bottom_y: f64) -> bool {
        let content_h = (self.page_h - self.top - self.bottom).max(1.0);
        let page_of = |y: f64| ((y - self.top).max(0.0) / content_h).floor() as i64;
        // A zero-height block can't be "cut"; only flag genuine spans.
        bottom_y - top_y > 0.05 && page_of(top_y) != page_of((bottom_y - 0.05).max(top_y))
    }

    /// Shift every fragment emitted at `self.out[start..]` by `(dx, dy)`
    /// (used to realise `position: relative|absolute|fixed` offsets after a
    /// subtree was laid out in place).
    fn translate_range(&mut self, start: usize, dx: f64, dy: f64) {
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        for a in &mut self.out[start..] {
            shift_fragment(&mut a.frag, dx, dy);
        }
    }

    /// Stamp `zi` (CSS `z-index`) on every fragment at `self.out[start..]` so a
    /// positioned subtree paints as one stacking unit.
    fn stamp_z(&mut self, start: usize, zi: i32) {
        if zi == 0 {
            return;
        }
        for a in &mut self.out[start..] {
            a.zi = zi;
        }
    }

    /// Drop fragments at `self.out[start..]` that fall entirely outside the clip
    /// `rect` (a pragmatic `overflow: hidden|clip` — whole fragments are culled
    /// rather than pixel-clipped, since the paint layer has no clip primitive).
    fn clip_range(&mut self, start: usize, rect: Cb) {
        let rx0 = rect.x;
        let ry0 = rect.y;
        let rx1 = rect.x + rect.w;
        let ry1 = rect.y + rect.h;
        let clip = [rect.x, rect.y, rect.w, rect.h];
        let mut i = start;
        while i < self.out.len() {
            let frag = &self.out[i].frag;
            if fragment_outside(frag, rx0, ry0, rx1, ry1) {
                // Fully outside the box ⇒ drop it (cheaper than clipping to empty).
                self.out.remove(i);
            } else if fragment_inside(frag, rx0, ry0, rx1, ry1) {
                // Fully inside ⇒ no clip needed; leave the bytes untouched.
                i += 1;
            } else {
                // Straddles an edge ⇒ wrap in `Clipped` so the painter emits a
                // real `W n` clip. Wrapping an already-`Clipped` fragment nests
                // the clips, which the painter intersects (handles nested boxes).
                let inner = std::mem::replace(&mut self.out[i].frag, Fragment::placeholder());
                self.out[i].frag = Fragment::Clipped {
                    rect: clip,
                    inner: Box::new(inner),
                };
                i += 1;
            }
        }
    }
}

/// Translate one fragment by `(dx, dy)` in place.
pub(crate) fn shift_fragment(frag: &mut Fragment, dx: f64, dy: f64) {
    match frag {
        Fragment::Text { x, y, .. }
        | Fragment::Rect { x, y, .. }
        | Fragment::Image { x, y, .. }
        | Fragment::Svg { x, y, .. }
        | Fragment::Border { x, y, .. }
        | Fragment::Gradient { x, y, .. } => {
            *x += dx;
            *y += dy;
        }
        Fragment::Clipped { rect, inner } => {
            // Move both the clip window and the clipped content together, so a
            // relatively-positioned subtree keeps its clip aligned.
            rect[0] += dx;
            rect[1] += dy;
            shift_fragment(inner, dx, dy);
        }
    }
}

/// True if a fragment's bounding box lies entirely outside `[x0,x1)×[y0,y1)`.
/// Text height is approximated from its font size.
fn fragment_outside(frag: &Fragment, x0: f64, y0: f64, x1: f64, y1: f64) -> bool {
    let (fx0, fy0, fx1, fy1) = fragment_bbox(frag);
    // No overlap with the clip rect on either axis ⇒ fully outside.
    fx1 < x0 || fx0 > x1 || fy1 < y0 || fy0 > y1
}

/// True when `frag` lies entirely within the rect — such a fragment needs no
/// real clip (it is already inside the `overflow` box), so `clip_range` leaves
/// it bare and only wraps the ones straddling an edge.
fn fragment_inside(frag: &Fragment, x0: f64, y0: f64, x1: f64, y1: f64) -> bool {
    let (fx0, fy0, fx1, fy1) = fragment_bbox(frag);
    fx0 >= x0 && fx1 <= x1 && fy0 >= y0 && fy1 <= y1
}

/// The fragment's axis-aligned bounding box `(x0, y0, x1, y1)` in top-down
/// points. A `Text` run is a thin vertical extent (the existing convention); a
/// `Clipped` fragment reports its inner box (the clip only shrinks it).
pub(crate) fn fragment_bbox(frag: &Fragment) -> (f64, f64, f64, f64) {
    match frag {
        Fragment::Text { x, y, w, style, .. } => (*x, *y, *x + *w, *y + style.font_size),
        Fragment::Rect { x, y, w, h, .. }
        | Fragment::Image { x, y, w, h, .. }
        | Fragment::Svg { x, y, w, h, .. }
        | Fragment::Border { x, y, w, h, .. }
        | Fragment::Gradient { x, y, w, h, .. } => (*x, *y, *x + *w, *y + *h),
        Fragment::Clipped { inner, .. } => fragment_bbox(inner),
    }
}

/// An atomic inline item for line breaking.
/// Inline replaced content laid out as a box on the line: a raster image
/// (`w, h, src`) or a vector SVG (`w, h, image`).
#[derive(Clone)]
enum Media {
    Raster(f64, f64, String),
    Svg(f64, f64, SvgImage),
}

impl Media {
    /// The reserved inline-box width in points.
    fn width(&self) -> f64 {
        match self {
            Media::Raster(w, ..) | Media::Svg(w, ..) => *w,
        }
    }
}

struct InlineItem {
    text: String,
    style: Style,
    /// Replaced content (`<img>` / inline `<svg>`) laid out as an inline box.
    media: Option<Media>,
}

/// One atom on a line: a text token or a replaced box, its measured width, and
/// whether a collapsible space follows it.
struct Word {
    text: String,
    style: Style,
    w: f64,
    media: Option<Media>,
    space_after: bool,
}

/// A unit of a multi-column block's flow, as the column placer distributes it:
/// either a single block-level child or a maximal run of inline-level siblings
/// (which lay out together as one inline formatting context).
enum FlowUnit<'a> {
    /// A block-level child element, with its 1-based list index (for markers).
    Block { el: &'a Element, list_index: usize },
    /// A contiguous run of inline-level nodes (text + inline elements).
    Inline(Vec<&'a Node>),
}

/// Partition a multi-column block's children into [`FlowUnit`]s: maximal runs of
/// inline-level content interleaved with block-level children, mirroring the
/// block/inline classification of [`Flow::block_children`] (whitespace-only text
/// between blocks is dropped, `display:none` is skipped, and list-item indices
/// honour an enclosing `<ol start>`). Out-of-flow children (float / absolute /
/// fixed) are treated as block units so they still lay out; their precise
/// out-of-flow behaviour inside a column is not modelled (a pragmatic choice —
/// floats/absolutes are rare inside multi-column text).
fn flow_units<'a>(
    children: &'a [Node],
    parent_style: &Style,
    ancestors: &[&Element],
    flow: &Flow,
) -> Vec<FlowUnit<'a>> {
    let mut units: Vec<FlowUnit<'a>> = Vec::new();
    let mut inline_run: Vec<&'a Node> = Vec::new();
    let mut list_index = list_start_offset(ancestors);

    for child in children {
        let is_block = match child {
            Node::Text(t) => {
                if t.trim().is_empty() {
                    continue; // collapse whitespace between blocks
                }
                false
            }
            Node::Element(e) => {
                let st = flow.style_of(e, parent_style, ancestors);
                if st.display == Display::None {
                    continue;
                }
                matches!(
                    st.display,
                    Display::Block
                        | Display::ListItem
                        | Display::Table
                        | Display::TableRow
                        | Display::Flex
                        | Display::Grid
                )
            }
        };

        if is_block {
            if !inline_run.is_empty() {
                units.push(FlowUnit::Inline(std::mem::take(&mut inline_run)));
            }
            if let Node::Element(e) = child {
                let st = flow.style_of(e, parent_style, ancestors);
                if st.display == Display::ListItem {
                    list_index += 1;
                }
                units.push(FlowUnit::Block {
                    el: e,
                    list_index,
                });
            }
        } else {
            inline_run.push(child);
        }
    }
    if !inline_run.is_empty() {
        units.push(FlowUnit::Inline(inline_run));
    }
    units
}

impl Flow<'_> {
    /// Lay out the children of a block container, partitioning runs of
    /// inline-level content into inline formatting contexts. Returns the bottom
    /// `y`.
    fn block_children(
        &mut self,
        children: &[Node],
        parent_style: &Style,
        x: f64,
        avail_w: f64,
        mut y: f64,
        ancestors: &[&Element],
    ) -> f64 {
        // Each block container establishes a fresh float context: floats placed
        // inside it don't leak into sibling/parent containers.
        let saved_floats = std::mem::take(&mut self.floats);

        let mut inline_run: Vec<&Node> = Vec::new();
        // `<ol start="N">` makes the first item count from N (default 1), so the
        // pre-increment counter starts at N-1. Plain lists count from 1.
        let mut list_index = list_start_offset(ancestors);
        // Bottom margin of the previous in-flow block, kept so the next block's
        // top margin can collapse against it (CSS adjacent-sibling margin
        // collapsing: the gap is `max(prev.bottom, next.top)`, not their sum).
        // Reset to `None` whenever inline content interrupts the block flow.
        let mut prev_block_bottom: Option<f64> = None;

        for child in children {
            // Out-of-flow children (float / absolute / fixed) are placed without
            // disturbing the normal-flow `y`. Detect them before the block/inline
            // partition so an inline-`display` floated/positioned box still works.
            if let Node::Element(e) = child {
                let st = self.style_of(e, parent_style, ancestors);
                if st.display == Display::None {
                    continue;
                }
                if st.float != FloatSide::None {
                    if !inline_run.is_empty() {
                        y = self.inline_context_f(
                            &inline_run,
                            parent_style,
                            x,
                            avail_w,
                            y,
                            ancestors,
                        );
                        inline_run.clear();
                    }
                    self.place_float(e, &st, x, avail_w, y, ancestors);
                    continue;
                }
                if matches!(st.position, Position::Absolute | Position::Fixed) {
                    self.place_positioned(e, &st, ancestors);
                    continue;
                }
            }

            let is_block = match child {
                Node::Text(t) => {
                    if t.trim().is_empty() {
                        continue; // collapse whitespace between blocks
                    }
                    false
                }
                Node::Element(e) => {
                    let st = self.style_of(e, parent_style, ancestors);
                    matches!(
                        st.display,
                        Display::Block
                            | Display::ListItem
                            | Display::Table
                            | Display::TableRow
                            | Display::Flex
                            | Display::Grid
                    )
                }
            };

            if is_block {
                if !inline_run.is_empty() {
                    y = self.inline_context_f(&inline_run, parent_style, x, avail_w, y, ancestors);
                    inline_run.clear();
                    prev_block_bottom = None; // inline content broke the adjacency
                }
                if let Node::Element(e) = child {
                    let st = self.style_of(e, parent_style, ancestors);
                    if st.display == Display::ListItem {
                        list_index += 1;
                    }
                    // `clear: left|right|both` — drop this block below the
                    // preceding floats on the chosen side(s) before placing it.
                    // Clearance establishes a hard boundary, so it suppresses
                    // margin collapsing against the previous sibling.
                    let cleared = if st.clear != Clear::None {
                        let clear_to = self.floats.clear_bottom(st.clear);
                        if clear_to > y {
                            y = clear_to;
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    // Collapse this block's top margin against the previous
                    // block's bottom margin: pull `y` up by the overlap so the
                    // visible gap becomes `max(prev.bottom, this.top)`. Skipped
                    // when clearance already moved the block down.
                    if !cleared {
                        if let Some(prev_bottom) = prev_block_bottom {
                            y -= prev_bottom.min(st.margin.top).max(0.0);
                        }
                    }
                    if st.page_break_before {
                        y = self.break_to_next_page(y);
                    }
                    // `position: relative` lays out in flow, then shifts its
                    // fragments by `inset` (its normal space is preserved).
                    let mut start = self.out.len();
                    let y_before = y;
                    y = self.block(e, &st, parent_style, x, avail_w, y, ancestors, list_index);
                    // `page-break-inside: avoid` — if the block straddled a page
                    // boundary yet fits within one page, discard it and re-lay
                    // from the next page top so it stays intact.
                    // Both `relative` and `sticky` lay out in flow and are then
                    // shifted by `inset`, so neither should be re-laid by the
                    // `page-break-inside: avoid` reflow (which is for in-flow
                    // boxes that haven't been offset yet).
                    let in_flow_offset =
                        matches!(st.position, Position::Relative | Position::Sticky);
                    if st.page_break_inside_avoid
                        && !in_flow_offset
                        && self.crosses_page_break(y_before, y)
                    {
                        let height = y - y_before;
                        let content_h = (self.page_h - self.top - self.bottom).max(1.0);
                        if height <= content_h {
                            self.out.truncate(start);
                            let moved = self.break_to_next_page(y_before);
                            if moved > y_before {
                                start = self.out.len();
                                y = self.block(
                                    e, &st, parent_style, x, avail_w, moved, ancestors, list_index,
                                );
                            }
                        }
                    }
                    if st.position == Position::Relative {
                        let (dx, dy) = self.relative_offset(&st, avail_w);
                        self.translate_range(start, dx, dy);
                    } else if st.position == Position::Sticky {
                        // `sticky` ≈ `relative`, but the offset is clamped so the
                        // box never leaves its containing block (the static,
                        // scroll-free approximation). Bound the shift by how far
                        // the box's edges may move within `self.cb`.
                        let (dx, dy) = self.relative_offset(&st, avail_w);
                        let (dx, dy) =
                            self.clamp_sticky_offset(dx, dy, x, y_before, avail_w, y - y_before);
                        self.translate_range(start, dx, dy);
                    }
                    if st.z_index != 0 {
                        self.stamp_z(start, st.z_index);
                    }
                    if st.page_break_after {
                        y = self.break_to_next_page(y);
                        prev_block_bottom = None;
                    } else {
                        prev_block_bottom = Some(st.margin.bottom);
                    }
                }
            } else {
                inline_run.push(child);
                prev_block_bottom = None;
            }
        }
        if !inline_run.is_empty() {
            y = self.inline_context_f(&inline_run, parent_style, x, avail_w, y, ancestors);
        }
        // Clear past any floats that extend below the in-flow content, so the
        // container fully contains its floats (matches `overflow`/clearfix).
        y = y.max(self.floats.max_bottom());

        self.floats = saved_floats;
        y
    }

    /// `position: relative` offset in points from `top`/`left` (falling back to
    /// the negated `bottom`/`right` when only those are set), resolved against
    /// the containing width/height.
    fn relative_offset(&self, st: &Style, avail_w: f64) -> (f64, f64) {
        let resolve = |len: Len, base: f64| match len {
            Len::Pt(p) => p,
            Len::Percent(pc) => base * pc / 100.0,
        };
        let dx = match (st.inset[3], st.inset[1]) {
            (Some(l), _) => resolve(l, avail_w),
            (None, Some(r)) => -resolve(r, avail_w),
            _ => 0.0,
        };
        let dy = match (st.inset[0], st.inset[2]) {
            (Some(t), _) => resolve(t, self.cb.h),
            (None, Some(b)) => -resolve(b, self.cb.h),
            _ => 0.0,
        };
        (dx, dy)
    }

    /// Clamp a `position: sticky` shift `(dx, dy)` so the box — laid out at
    /// `(box_x, box_y)` with size `box_w × box_h` (`box_w` = `avail_w`) — stays
    /// inside its containing block `self.cb` after translation. This is the
    /// static, scroll-free model of stickiness: the box may shift up to where its
    /// edge meets the container edge, then no further. With a box that already
    /// fills (or overflows) the container on an axis, the clamp pins that axis to
    /// `0` (no room to move), exactly like a sticky element with nowhere to go.
    fn clamp_sticky_offset(
        &self,
        dx: f64,
        dy: f64,
        box_x: f64,
        box_y: f64,
        box_w: f64,
        box_h: f64,
    ) -> (f64, f64) {
        // Clamp against the enclosing block container (`flow_cb`), the nearest
        // block ancestor — that is the box a static-page sticky element may not
        // escape. Per axis, clamp the requested shift `d` so the box
        // `[bstart, bend]` stays within `[cstart, cend]`: the shift that hugs the
        // start edge is `cstart - bstart` (≤ 0, the box starts inside) and the one
        // that hugs the end edge is `cend - bend`. If the box is bigger than the
        // container the two cross over → no slack, pin to 0.
        let cb = self.flow_cb;
        let clamp_axis = |d: f64, bstart: f64, bend: f64, cstart: f64, cend: f64| -> f64 {
            let min_d = cstart - bstart;
            let max_d = cend - bend;
            if min_d > max_d {
                0.0
            } else {
                d.clamp(min_d, max_d)
            }
        };
        let cdx = clamp_axis(dx, box_x, box_x + box_w, cb.x, cb.x + cb.w);
        let cdy = clamp_axis(dy, box_y, box_y + box_h, cb.y, cb.y + cb.h);
        (cdx, cdy)
    }

    /// Place an out-of-flow `position: absolute|fixed` element: lay its subtree
    /// out at the origin of its containing block, then translate it to the
    /// position resolved from `inset`. Fixed resolves against the page box,
    /// absolute against the current containing block. Does not affect flow `y`.
    fn place_positioned(&mut self, el: &Element, st: &Style, ancestors: &[&Element]) {
        let cb = if st.position == Position::Fixed {
            self.page_cb
        } else {
            self.cb
        };
        // Resolve width: explicit `width`, else left+right insets pin both
        // edges, else shrink to the containing block.
        let resolve = |len: Len, base: f64| match len {
            Len::Pt(p) => p,
            Len::Percent(pc) => base * pc / 100.0,
        };
        let left = st.inset[3].map(|l| resolve(l, cb.w));
        let right = st.inset[1].map(|r| resolve(r, cb.w));
        let top = st.inset[0].map(|t| resolve(t, cb.h));
        let bottom = st.inset[2].map(|b| resolve(b, cb.h));

        let box_w = match (st.width, left, right) {
            (Some(len), ..) => resolve(len, cb.w),
            (None, Some(l), Some(r)) => (cb.w - l - r).max(1.0),
            _ => cb.w,
        };
        // Lay the subtree out at the containing block's top-left, in isolation
        // from the surrounding float context.
        let saved_floats = std::mem::take(&mut self.floats);
        let saved_cb = self.cb;
        self.cb = Cb {
            x: cb.x,
            y: cb.y,
            w: box_w,
            h: cb.h,
        };
        let start = self.out.len();
        // Treat it as a block (its own formatting context).
        let bstyle = Style {
            display: Display::Block,
            position: Position::Static,
            float: FloatSide::None,
            ..st.clone()
        };
        let bottom_y = self.block(el, &bstyle, st, cb.x, box_w, cb.y, ancestors, 0);
        let laid_h = bottom_y - cb.y;

        // Final top-left from insets (default: the containing block origin).
        let final_x = match (left, right) {
            (Some(l), _) => cb.x + l,
            (None, Some(r)) => cb.x + cb.w - r - box_w,
            _ => cb.x,
        };
        let final_y = match (top, bottom) {
            (Some(t), _) => cb.y + t,
            (None, Some(b)) => cb.y + cb.h - b - laid_h,
            _ => cb.y,
        };
        self.translate_range(start, final_x - cb.x, final_y - cb.y);
        // Absolutely-positioned content stacks above in-flow content by default.
        self.stamp_z(start, if st.z_index != 0 { st.z_index } else { 1 });

        self.cb = saved_cb;
        self.floats = saved_floats;
    }

    /// Place a `float: left|right` box: lay it out as a block sized to its
    /// `width` (or shrink-to-fit fallback) at the appropriate edge of the
    /// content box, then register its band so following inline lines wrap.
    fn place_float(
        &mut self,
        el: &Element,
        st: &Style,
        x: f64,
        avail_w: f64,
        y: f64,
        ancestors: &[&Element],
    ) {
        let left = st.float == FloatSide::Left;
        // Width: explicit `width` else a third of the line (a pragmatic
        // shrink-to-fit that keeps room for the wrapping text).
        let box_w = match st.width {
            Some(Len::Pt(w)) => w,
            Some(Len::Percent(pc)) => avail_w * pc / 100.0,
            None => (avail_w / 3.0).max(1.0),
        }
        .min(avail_w);

        // Existing same-side floats overlapping `y` stack inward.
        let (l_in, r_in) = self.floats.insets(y, 1.0);
        let box_x = if left {
            x + l_in
        } else {
            x + avail_w - r_in - box_w
        };

        let start = self.out.len();
        let bstyle = Style {
            display: Display::Block,
            float: FloatSide::None,
            position: Position::Static,
            ..st.clone()
        };
        let bottom_y = self.block(el, &bstyle, st, box_x, box_w, y, ancestors, 0);

        if st.z_index != 0 {
            self.stamp_z(start, st.z_index);
        }
        self.floats.boxes.push(FloatBox {
            left,
            top: y,
            bottom: bottom_y.max(y + 0.1),
            width: box_w,
        });
    }

    /// Lay out an inline run, applying the active floats so lines wrap around
    /// them. Falls back to the plain inline context when no floats are active.
    fn inline_context_f(
        &mut self,
        nodes: &[&Node],
        style: &Style,
        x: f64,
        avail_w: f64,
        y: f64,
        ancestors: &[&Element],
    ) -> f64 {
        if self.floats.boxes.is_empty() {
            return self.inline_context(nodes, style, x, avail_w, y, ancestors);
        }
        let mut items = Vec::new();
        for n in nodes {
            self.collect_inline(n, style, ancestors, &mut items);
        }
        let floats = self.floats.clone();
        self.flow_lines_floated(
            &items,
            x,
            avail_w,
            y,
            style.align,
            style.text_indent,
            &floats,
            style.direction,
        )
    }

    /// If `el` is a Mermaid flowchart container, lay the diagram out within
    /// `avail_w`, push its vector + label fragments at the block's origin, and
    /// return the `y` past it (top margin + diagram + bottom margin). Returns
    /// `None` when `el` isn't a renderable Mermaid block, so the caller renders
    /// it normally.
    fn try_mermaid_block(
        &mut self,
        el: &Element,
        style: &Style,
        x: f64,
        avail_w: f64,
        y: f64,
    ) -> Option<f64> {
        let diagram =
            super::diagram::try_build(el, style, avail_w.max(1.0), self.m)?;

        // Honour the block's own vertical margins; centre the diagram in the
        // available width when it's narrower (mermaid renders centred).
        let top = y + style.margin.top;
        let dx = x + ((avail_w - diagram.width).max(0.0)) / 2.0;

        // The vector geometry (boxes + edges + arrow-heads) as one Svg fragment.
        self.out.push(Abs {
            z: 1,
            zi: 0,
            frag: Fragment::Svg {
                x: dx,
                y: top,
                w: diagram.width,
                h: diagram.height,
                image: diagram.image,
            },
        });

        // Centred labels on top (node titles + edge labels). Each label centre
        // `(cx, cy)` is diagram-local; convert to absolute and back off by half
        // the measured text width / ascent so the run is visually centred.
        for label in &diagram.labels {
            let mut lstyle = style.clone();
            lstyle.font_size = label.font_size;
            lstyle.bold = label.bold;
            lstyle.font_weight = if label.bold { 700 } else { 400 };
            lstyle.italic = false;
            lstyle.underline = false;
            lstyle.strike = false;
            lstyle.color = [0.13, 0.13, 0.13];
            let tw = self.m.width(&label.text, &lstyle);
            // Text is positioned at its top-left; the paint layer draws from the
            // baseline at `y + 0.8·font_size`, so lift the box by ~0.8·fs to
            // centre the glyphs on `cy`.
            let lx = dx + label.cx - tw / 2.0;
            let ly = top + label.cy - label.font_size * 0.8;
            self.out.push(Abs {
                z: 1,
                zi: 0,
                frag: Fragment::Text {
                    x: lx,
                    y: ly,
                    w: tw,
                    style: lstyle,
                    text: label.text.clone(),
                },
            });
        }

        Some(top + diagram.height + style.margin.bottom)
    }

    #[allow(clippy::too_many_arguments)]
    fn block(
        &mut self,
        el: &Element,
        style: &Style,
        _parent_style: &Style,
        x: f64,
        avail_w: f64,
        mut y: f64,
        ancestors: &[&Element],
        list_index: usize,
    ) -> f64 {
        if style.display == Display::None {
            return y;
        }
        // Mermaid flowchart blocks (`<pre class="mermaid">`, `<div class="mermaid">`,
        // `<pre><code class="language-mermaid">`) render as native vector diagrams.
        // If the block is a Mermaid container whose source parses as a flowchart,
        // emit the diagram and consume the block; otherwise fall through and the
        // block renders exactly as before.
        if let Some(next_y) = self.try_mermaid_block(el, style, x, avail_w, y) {
            return next_y;
        }
        if el.tag == "table" {
            return self.table(el, style, x, avail_w, y, ancestors);
        }
        if style.display == Display::Flex {
            return self.flex(el, style, x, avail_w, y, ancestors);
        }
        if style.display == Display::Grid {
            return self.grid(el, style, x, avail_w, y, ancestors);
        }
        // A normal block with `column-count`/`columns` > 1 lays its flow content
        // out as equal-width balanced columns (newspaper/newsletter flow).
        if style.column_count > 1 {
            return self.columns(el, style, x, avail_w, y, ancestors);
        }

        let m = &style.margin;
        let p = &style.padding;
        let b = &style.border_width;

        y += m.top;
        let box_top = y;
        let resolve_w = |len: Len| match len {
            Len::Pt(w) => w,
            Len::Percent(pc) => avail_w * pc / 100.0,
        };
        let mut box_w = match style.width {
            // `box-sizing: border-box` → `width` already includes padding+border.
            Some(Len::Pt(w)) if style.border_box => w,
            Some(Len::Pt(w)) => w + p.left + p.right + b.left + b.right,
            Some(Len::Percent(pc)) => avail_w * pc / 100.0,
            None => avail_w - m.left - m.right,
        };
        if let Some(mw) = style.max_width {
            box_w = box_w.min(resolve_w(mw));
        }
        if let Some(mw) = style.min_width {
            box_w = box_w.max(resolve_w(mw));
        }
        // `margin: 0 auto` (or `margin-left/right: auto`) on a fixed-width block
        // centres it in the available width: the auto side(s) split the free
        // space. With only one auto side, that side pushes the box to the
        // opposite edge. Falls back to the normal `x + margin-left` otherwise.
        let box_x = if style.width.is_some() && (style.margin_left_auto || style.margin_right_auto) {
            let free = (avail_w - box_w).max(0.0);
            match (style.margin_left_auto, style.margin_right_auto) {
                (true, true) => x + free / 2.0,
                (true, false) => x + (free - m.right).max(0.0),
                (false, true) => x + m.left,
                (false, false) => x + m.left,
            }
        } else {
            x + m.left
        };
        let content_x = box_x + b.left + p.left;
        let content_w = (box_w - b.left - b.right - p.left - p.right).max(1.0);

        let mut cy = y + b.top + p.top;

        // Marker for list items (honours `list-style-type`). `ancestors` ends at
        // the enclosing list container, so its `<ol>`/`<ul>` depth drives the
        // default bullet glyph (disc → circle → square) when unspecified.
        if style.display == Display::ListItem {
            if let Some(marker) = list_marker(
                style,
                list_marker_ordered(ancestors),
                list_index,
                list_nesting_depth(ancestors),
            ) {
                let mstyle = style.clone();
                let mw = self.m.width(&marker, &mstyle);
                self.out.push(Abs {
                    z: 1,
                    zi: 0,
                    frag: Fragment::Text {
                        x: content_x - mw - 4.0,
                        y: cy,
                        w: mw,
                        style: mstyle,
                        text: marker,
                    },
                });
            }
        }

        let new_ancestors = push_ancestor(ancestors, el);
        // A positioned box becomes the containing block for descendant
        // `position: absolute` elements. Save/restore the previous one.
        let establishes_cb = style.position != Position::Static;
        let saved_cb = self.cb;
        if establishes_cb {
            self.cb = Cb {
                x: content_x,
                y: cy,
                w: content_w,
                h: style.height.or(style.min_height).unwrap_or(self.cb.h),
            };
        }
        // This block is the enclosing block container for its children: record
        // its content box so a `position: sticky` child clamps within it. Height
        // comes from a declared `height`/`min-height`, else the remaining page
        // band from the content top down to the page bottom.
        let saved_flow_cb = self.flow_cb;
        self.flow_cb = Cb {
            x: content_x,
            y: cy,
            w: content_w,
            h: style
                .height
                .or(style.min_height)
                .unwrap_or((self.page_h - self.bottom - cy).max(1.0)),
        };
        let children_start = self.out.len();
        cy = self.block_children(
            &el.children,
            style,
            content_x,
            content_w,
            cy,
            &new_ancestors,
        );
        self.flow_cb = saved_flow_cb;

        cy += p.bottom + b.bottom;
        // A definite `height` caps the box (content overflows — and is clipped
        // under `overflow: hidden`); without it the box grows to its content.
        // `min-height` is only a floor on whichever applies.
        let mut box_h = style.height.unwrap_or((cy - box_top).max(0.1));
        if let Some(mh) = style.min_height {
            box_h = box_h.max(mh);
        }

        // `overflow: hidden|clip` — cull descendant fragments outside the
        // padding box (the visible content area, including padding).
        if style.overflow_clip {
            let clip = Cb {
                x: box_x + b.left,
                y: box_top + b.top,
                w: (box_w - b.left - b.right).max(0.0),
                h: (box_h - b.top - b.bottom).max(0.0),
            };
            self.clip_range(children_start, clip);
        }
        if establishes_cb {
            self.cb = saved_cb;
        }

        // Background + border behind the content (z=0). `visibility: hidden`
        // suppresses the paint but the box still occupies its space. Borders are
        // drawn per-side so `border-bottom`/`border-left` (etc.) keep their own
        // width and colour; `border-radius`, `box-shadow`, `linear-gradient`
        // backgrounds and `dashed`/`dotted`/`double` border styles are honoured
        // by the shared decoration helper.
        self.emit_box_decoration(style, box_x, box_top, box_w, box_h);

        box_top + box_h + m.bottom
    }

    /// Lay out a run of inline nodes into line boxes; returns the bottom `y`.
    fn inline_context(
        &mut self,
        nodes: &[&Node],
        style: &Style,
        x: f64,
        avail_w: f64,
        y: f64,
        ancestors: &[&Element],
    ) -> f64 {
        let mut items = Vec::new();
        for n in nodes {
            self.collect_inline(n, style, ancestors, &mut items);
        }
        self.flow_lines(&items, x, avail_w, y, style.align, style.text_indent, style.direction)
    }

    fn collect_inline(
        &mut self,
        node: &Node,
        parent_style: &Style,
        ancestors: &[&Element],
        out: &mut Vec<InlineItem>,
    ) {
        match node {
            Node::Text(t) => {
                // A bare text node has no box of its own: its highlight may only
                // come from an enclosing *inline* element (`<mark>`, a styled
                // `<span>`), never from a block-ish container — whose own
                // `background` is painted as a box (`push_box_background`) and is a
                // non-inherited property. So drop the container's background unless
                // the immediate parent is a true inline element. This keeps inline
                // highlights working while leaving block/cell backgrounds to their
                // own box (no duplicate rect behind the text).
                let mut style = parent_style.clone();
                if parent_style.display != Display::Inline {
                    style.background = None;
                }
                out.push(InlineItem {
                    text: parent_style.text_transform.apply(t),
                    style,
                    media: None,
                });
            }
            Node::Element(e) => {
                let st = self.style_of(e, parent_style, ancestors);
                if st.display == Display::None {
                    return;
                }
                if e.tag == "br" {
                    out.push(InlineItem {
                        text: "\n".into(),
                        style: st,
                        media: None,
                    });
                    return;
                }
                // Inline <svg> → native vector box (sized by width/height or viewBox).
                if e.tag == "svg" {
                    if let Some(img) = crate::svg::from_element(e) {
                        let w = e
                            .attr("width")
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(img.width.max(1.0));
                        let h = e
                            .attr("height")
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(img.height.max(1.0));
                        out.push(InlineItem {
                            text: String::new(),
                            style: st,
                            media: Some(Media::Svg(w, h, img)),
                        });
                    }
                    return;
                }
                if e.tag == "img" {
                    let w = e
                        .attr("width")
                        .and_then(|v| v.parse::<f64>().ok())
                        .unwrap_or(64.0);
                    let h = e
                        .attr("height")
                        .and_then(|v| v.parse::<f64>().ok())
                        .unwrap_or(64.0);
                    let src = e.attr("src").unwrap_or_default().to_string();
                    // A `data:image/svg+xml` source renders as native vector, not a bitmap.
                    let media = crate::svg::parse_data_uri(&src)
                        .map(|img| Media::Svg(w, h, img))
                        .unwrap_or(Media::Raster(w, h, src));
                    out.push(InlineItem {
                        text: String::new(),
                        style: st,
                        media: Some(media),
                    });
                    return;
                }
                let na = push_ancestor(ancestors, e);
                for c in &e.children {
                    self.collect_inline(c, &st, &na, out);
                }
            }
        }
    }

    /// Tokenise inline items into measured [`Word`]s for line breaking,
    /// honouring `white-space: pre` (newlines kept) and `letter-spacing`
    /// (added to each token's measured width).
    fn build_words(&self, items: &[InlineItem]) -> Vec<Word> {
        let mut words: Vec<Word> = Vec::new();
        let push_text = |words: &mut Vec<Word>, text: String, style: &Style, space: bool| {
            let base = self.m.width(&text, style);
            // `letter-spacing` widens the token by one step per character.
            let ls = style.letter_spacing * text.chars().count().max(1) as f64;
            words.push(Word {
                w: base + ls,
                text,
                style: style.clone(),
                media: None,
                space_after: space,
            });
        };
        for it in items {
            if let Some(m) = &it.media {
                words.push(Word {
                    text: String::new(),
                    style: it.style.clone(),
                    w: m.width(),
                    media: Some(m.clone()),
                    space_after: true,
                });
                continue;
            }
            if it.style.pre {
                // Preserve whitespace: split only on newlines, keep runs.
                for (i, seg) in it.text.split('\n').enumerate() {
                    if i > 0 {
                        words.push(Word {
                            text: "\n".into(),
                            style: it.style.clone(),
                            w: 0.0,
                            media: None,
                            space_after: false,
                        });
                    }
                    if !seg.is_empty() {
                        push_text(&mut words, seg.to_string(), &it.style, false);
                    }
                }
                continue;
            }
            let normalized = collapse_ws(&it.text);
            for token in normalized.split(' ') {
                if token == "\n" {
                    words.push(Word {
                        text: "\n".into(),
                        style: it.style.clone(),
                        w: 0.0,
                        media: None,
                        space_after: false,
                    });
                    continue;
                }
                if token.is_empty() {
                    continue;
                }
                push_text(&mut words, token.to_string(), &it.style, true);
            }
        }
        words
    }

    /// Emit one line of words at vertical position `*y`, aligned within
    /// `[line_x, line_x + line_avail)`, then advance `*y` by the line height.
    /// `space_w` is the inter-word space; `word_extra` is added at each space on
    /// top of it (used by `word-spacing`). `last` suppresses justification.
    ///
    /// `dir` is the inline base direction. With [`Direction::Ltr`] (the default,
    /// and the only value reachable for any existing document) the original
    /// left-to-right placement runs verbatim. With [`Direction::Rtl`] the same
    /// words — still in logical order — are placed from the right edge leftward:
    /// the first logical box sits flush right and each subsequent box advances to
    /// its left. The glyphs inside a run keep the order the font produced (the PDF
    /// text object draws them from the box's left corner), which is correct for a
    /// purely-RTL run; mixed-direction reordering (the full bidi algorithm) is out
    /// of scope.
    #[allow(clippy::too_many_arguments)]
    fn emit_line(
        &mut self,
        line: &[&Word],
        line_w: f64,
        y: &mut f64,
        last: bool,
        line_x: f64,
        line_avail: f64,
        align: Align,
        space_w: f64,
        dir: Direction,
    ) {
        if line.is_empty() {
            *y += default_line_height(&Style::default());
            return;
        }
        let line_h = line
            .iter()
            .map(|w| w.style.font_size * w.style.line_height.max(1.0))
            .fold(0.0_f64, f64::max);
        // Resolve direction-relative alignment (`start`/`end`) to a physical edge.
        let align = align.resolve(dir);
        let extra = (line_avail - line_w).max(0.0);

        if dir == Direction::Rtl {
            self.emit_line_rtl(line, y, last, line_x, line_avail, align, space_w, extra);
            *y += line_h;
            return;
        }

        // ── LTR (default): unchanged left-to-right placement ──
        let (mut cx, gap_extra) = match align {
            Align::Left => (line_x, 0.0),
            Align::Right => (line_x + extra, 0.0),
            Align::Center => (line_x + extra / 2.0, 0.0),
            Align::Justify => {
                let gaps = line.iter().filter(|w| w.space_after).count().max(1);
                (line_x, if last { 0.0 } else { extra / gaps as f64 })
            }
            // `Start`/`End` are resolved above; nothing reaches here.
            Align::Start | Align::End => (line_x, 0.0),
        };
        for w in line.iter() {
            match &w.media {
                Some(Media::Raster(iw, ih, src)) => {
                    self.out.push(Abs {
                        z: 1,
                        zi: 0,
                        frag: Fragment::Image {
                            x: cx,
                            y: *y,
                            w: *iw,
                            h: *ih,
                            src: src.clone(),
                        },
                    });
                    cx += iw + space_w;
                }
                Some(Media::Svg(iw, ih, image)) => {
                    self.out.push(Abs {
                        z: 1,
                        zi: 0,
                        frag: Fragment::Svg {
                            x: cx,
                            y: *y,
                            w: *iw,
                            h: *ih,
                            image: image.clone(),
                        },
                    });
                    cx += iw + space_w;
                }
                None => {
                    // Inline highlight: a run carrying its own `background`
                    // (`<mark>`, or a span with `background-color`) paints a filled
                    // rectangle behind the glyphs at z=0. `background` is a
                    // non-inherited property reset on every child, so a block's
                    // background never reaches here — only the run's own highlight
                    // does, and adjacent highlighted words form a continuous band.
                    push_run_highlight(&mut self.out, cx, *y, w);
                    // `vertical-align: super|sub` raises/lowers the run's baseline
                    // within the line (negative = up). Width/advance are unchanged.
                    self.out.push(Abs {
                        z: 1,
                        zi: 0,
                        frag: Fragment::Text {
                            x: cx,
                            y: *y + w.style.valign_shift,
                            w: w.w,
                            style: w.style.clone(),
                            text: w.text.clone(),
                        },
                    });
                    cx += w.w
                        + if w.space_after {
                            space_w + gap_extra + w.style.word_spacing
                        } else {
                            0.0
                        };
                }
            }
        }
        *y += line_h;
    }

    /// Place one already-broken line right-to-left. Words stay in logical order;
    /// `cx_right` tracks the right edge of the next box and walks leftward. Each
    /// box is emitted at `cx_right - content_width` (its physical left corner),
    /// then `cx_right` recedes by the box's advance (content + trailing space, plus
    /// `word-spacing`/justification at a real inter-word space). The starting right
    /// edge encodes the resolved alignment, mirroring the LTR `cx` computation.
    #[allow(clippy::too_many_arguments)]
    fn emit_line_rtl(
        &mut self,
        line: &[&Word],
        y: &f64,
        last: bool,
        line_x: f64,
        line_avail: f64,
        align: Align,
        space_w: f64,
        extra: f64,
    ) {
        let right = line_x + line_avail;
        // The right edge of the line's content block. `Right` (the RTL default)
        // hugs the trailing/right edge; `Left` pushes the block to the leading/left
        // edge so its right side sits `extra` in from the right; `Center` splits.
        // `Justify` keeps the block flush right and spreads the slack into gaps.
        let (mut cx_right, gap_extra) = match align {
            Align::Right | Align::Justify => (right, 0.0),
            Align::Left => (right - extra, 0.0),
            Align::Center => (right - extra / 2.0, 0.0),
            Align::Start | Align::End => (right, 0.0),
        };
        let gap_extra = if matches!(align, Align::Justify) && !last {
            let gaps = line.iter().filter(|w| w.space_after).count().max(1);
            extra / gaps as f64
        } else {
            gap_extra
        };
        for w in line.iter() {
            // The visible content width placed for this box (text run or media);
            // the trailing inter-word space is advance-only, not drawn.
            let content = match &w.media {
                Some(m) => m.width(),
                None => w.w,
            };
            let x = cx_right - content;
            match &w.media {
                Some(Media::Raster(iw, ih, src)) => {
                    self.out.push(Abs {
                        z: 1,
                        zi: 0,
                        frag: Fragment::Image {
                            x,
                            y: *y,
                            w: *iw,
                            h: *ih,
                            src: src.clone(),
                        },
                    });
                    cx_right = x - space_w;
                }
                Some(Media::Svg(iw, ih, image)) => {
                    self.out.push(Abs {
                        z: 1,
                        zi: 0,
                        frag: Fragment::Svg {
                            x,
                            y: *y,
                            w: *iw,
                            h: *ih,
                            image: image.clone(),
                        },
                    });
                    cx_right = x - space_w;
                }
                None => {
                    // Inline highlight behind the glyphs (see the LTR path).
                    push_run_highlight(&mut self.out, x, *y, w);
                    self.out.push(Abs {
                        z: 1,
                        zi: 0,
                        frag: Fragment::Text {
                            x,
                            y: *y + w.style.valign_shift,
                            w: w.w,
                            style: w.style.clone(),
                            text: w.text.clone(),
                        },
                    });
                    cx_right = x
                        - if w.space_after {
                            space_w + gap_extra + w.style.word_spacing
                        } else {
                            0.0
                        };
                }
            }
        }
    }

    /// Break inline items into lines and emit positioned text/images.
    /// `indent` (`text-indent`) shifts and shortens the first line only.
    #[allow(clippy::too_many_arguments)]
    fn flow_lines(
        &mut self,
        items: &[InlineItem],
        x: f64,
        avail_w: f64,
        mut y: f64,
        align: Align,
        indent: f64,
        dir: Direction,
    ) -> f64 {
        let words = self.build_words(items);
        let mut line: Vec<&Word> = Vec::new();
        let mut line_w = 0.0;
        let space_w = self.m.width(" ", &Style::default());

        // The first line uses a reduced budget `avail_w - indent` and starts at
        // `x + indent`; every subsequent line spans the full width at `x`.
        let mut first_line = true;
        let line_geom = |first: bool| -> (f64, f64) {
            if first {
                (x + indent, (avail_w - indent).max(1.0))
            } else {
                (x, avail_w)
            }
        };

        let mut i = 0;
        while i < words.len() {
            let (line_x, line_avail) = line_geom(first_line);
            let w = &words[i];
            if w.text == "\n" {
                self.emit_line(
                    &line, line_w, &mut y, true, line_x, line_avail, align, space_w, dir,
                );
                line.clear();
                line_w = 0.0;
                first_line = false;
                i += 1;
                continue;
            }
            let add = w.w + if line.is_empty() { 0.0 } else { space_w };
            if !line.is_empty() && line_w + add > line_avail {
                self.emit_line(
                    &line, line_w, &mut y, false, line_x, line_avail, align, space_w, dir,
                );
                line.clear();
                first_line = false;
                // Re-evaluate the same word on the fresh line.
                line.push(w);
                line_w = w.w;
            } else {
                line.push(w);
                line_w += add;
            }
            i += 1;
        }
        let (line_x, line_avail) = line_geom(first_line);
        self.emit_line(
            &line, line_w, &mut y, true, line_x, line_avail, align, space_w, dir,
        );
        y
    }

    /// Like [`flow_lines`] but narrows each line by the active `floats` for that
    /// line's vertical band, so inline text wraps around floated boxes. Left
    /// floats shift the line start right; both sides shrink the available width.
    #[allow(clippy::too_many_arguments)]
    fn flow_lines_floated(
        &mut self,
        items: &[InlineItem],
        x: f64,
        avail_w: f64,
        mut y: f64,
        align: Align,
        indent: f64,
        floats: &FloatCtx,
        dir: Direction,
    ) -> f64 {
        let words = self.build_words(items);
        let space_w = self.m.width(" ", &Style::default());
        let mut line: Vec<&Word> = Vec::new();
        let mut line_w = 0.0;
        let mut first_line = true;

        // Per-line geometry at the current `y`: shrink/shift by float insets,
        // then apply `text-indent` to the first line.
        let geom = |this: &Self, y: f64, first: bool| -> (f64, f64) {
            let line_h = this
                .m
                .width("x", &Style::default())
                .max(default_line_height(&Style::default()));
            let (l, r) = floats.insets(y, line_h);
            let ind = if first { indent } else { 0.0 };
            (x + l + ind, (avail_w - l - r - ind).max(1.0))
        };

        let mut i = 0;
        while i < words.len() {
            let (line_x, line_avail) = geom(self, y, first_line);
            let w = &words[i];
            if w.text == "\n" {
                self.emit_line(
                    &line, line_w, &mut y, true, line_x, line_avail, align, space_w, dir,
                );
                line.clear();
                line_w = 0.0;
                first_line = false;
                i += 1;
                continue;
            }
            let add = w.w + if line.is_empty() { 0.0 } else { space_w };
            if !line.is_empty() && line_w + add > line_avail {
                self.emit_line(
                    &line, line_w, &mut y, false, line_x, line_avail, align, space_w, dir,
                );
                line.clear();
                first_line = false;
                line.push(w);
                line_w = w.w;
            } else {
                line.push(w);
                line_w += add;
            }
            i += 1;
        }
        let (line_x, line_avail) = geom(self, y, first_line);
        self.emit_line(
            &line, line_w, &mut y, true, line_x, line_avail, align, space_w, dir,
        );
        y
    }

    /// Pragmatic table layout. Column widths come from a `<colgroup>`/`<col>`
    /// set or the first row's per-cell `width`, normalised to fit `avail_w`
    /// (fixed-layout style); columns with no declared width share the remainder
    /// equally, so a table that declares nothing keeps **equal** columns. Cells
    /// are placed onto a grid honouring both `colspan` and `rowspan`
    /// ([`build_grid`]): a `colspan` cell covers the summed width of its columns,
    /// a `rowspan` cell covers the summed height of its rows and reserves those
    /// columns so the rows below shift their cells past it. A simple row's height
    /// is its tallest 1-row cell; a `rowspan` cell that is taller than the rows
    /// it covers grows them (deficit spread over the spanned rows).
    fn table(
        &mut self,
        el: &Element,
        style: &Style,
        x: f64,
        avail_w: f64,
        mut y: f64,
        ancestors: &[&Element],
    ) -> f64 {
        y += style.margin.top;
        let table_top = y;
        let na = push_ancestor(ancestors, el);
        let rows = collect_rows(el);

        // Place every cell on the grid (colspan + rowspan), then resolve column
        // widths and prefix-sum them so a cell is positioned by its start column.
        let (grid, ncols) = build_grid(&rows);
        let col_w = self.resolve_col_widths(el, style, &rows, &na, avail_w, ncols);
        let mut cum_x = Vec::with_capacity(col_w.len() + 1);
        let mut acc = 0.0;
        cum_x.push(0.0);
        for w in &col_w {
            acc += w;
            cum_x.push(acc);
        }
        // Width spanning columns `[start, start+span)`, clamped to the grid.
        let span_geom = |start: usize, span: usize| -> (f64, f64) {
            let s = start.min(col_w.len());
            let e = (start + span.max(1)).min(col_w.len());
            (cum_x[s], (cum_x[e] - cum_x[s]).max(1.0))
        };

        let collapse = style.border_collapse;
        let n_rows = rows.len();

        // Per-placed-cell record carried from the measure pass to placement.
        struct Placed {
            start: usize,
            col_span: usize,
            row: usize,
            row_span: usize,
            frag_lo: usize,
            frag_hi: usize,
            /// Content height (top of content area → bottom of content area),
            /// excluding the cell's own border but including padding.
            content_h: f64,
            /// Absolute top of the cell's content fragments as emitted in the
            /// measure pass (= provisional row top + border.top + padding.top).
            /// Placement translates from here to the final content top.
            prov_content_top: f64,
            /// Top padding (final content top = final cell top + border.top +
            /// padding.top).
            pad_top: f64,
            background: Option<[f64; 3]>,
            border_width: super::css::Edges,
            border_color_edges: [[f64; 3]; 4],
            border_style_edges: [BorderStyle; 4],
            vertical_align: VAlign,
            opacity: f64,
        }

        // Provisional per-row top (set as we walk rows) and per-row height
        // (seeded by 1-row cells; grown later by rowspan deficits).
        let mut row_top = vec![table_top; n_rows];
        let mut row_h = vec![0.1f64; n_rows];
        let mut placed: Vec<Placed> = Vec::with_capacity(grid.len());

        // Measure pass: lay each cell's content out once, at its anchor row's
        // provisional top. Horizontal placement is final (column x never moves);
        // the vertical position is corrected by a translate once row heights are
        // resolved. `y` tracks the provisional top of the current row.
        let mut gi = 0usize;
        for r in 0..n_rows {
            row_top[r] = y;
            let mut single_row_h = 0.1f64;
            while gi < grid.len() && grid[gi].row == r {
                let gc = &grid[gi];
                gi += 1;
                let cstyle = self.style_of(gc.el, style, &na);
                let (dx, cw) = span_geom(gc.col, gc.col_span);
                let cx = x + dx;
                let nca = push_ancestor(&na, gc.el);
                let p = &cstyle.padding;
                let bw = cstyle.border_width;
                let content_top = y + p.top + bw.top;
                let frag_lo = self.out.len();
                let mut cy = self.block_children(
                    &gc.el.children,
                    &cstyle,
                    cx + p.left + bw.left,
                    (cw - p.left - p.right).max(1.0),
                    content_top,
                    &nca,
                );
                let frag_hi = self.out.len();
                cy += p.bottom;
                let content_h = (cy - content_top).max(0.0) + p.top;
                // Total cell height (content + both borders) the rows it spans
                // must accommodate.
                let cell_h = content_h + bw.top + bw.bottom;
                if gc.row_span <= 1 {
                    single_row_h = single_row_h.max(cell_h);
                }
                placed.push(Placed {
                    start: gc.col,
                    col_span: gc.col_span,
                    row: r,
                    row_span: gc.row_span,
                    frag_lo,
                    frag_hi,
                    content_h,
                    prov_content_top: content_top,
                    pad_top: p.top,
                    background: cstyle.background,
                    border_width: bw,
                    border_color_edges: cstyle.border_color_edges,
                    border_style_edges: cstyle.border_style_edges,
                    vertical_align: cstyle.vertical_align,
                    opacity: cstyle.opacity,
                });
            }
            row_h[r] = single_row_h;
            y += single_row_h;
        }

        // Resolve rowspan deficits: a cell spanning rows `[r, r+rs)` must fit in
        // their summed height; spread any shortfall evenly over those rows.
        for pl in &placed {
            if pl.row_span <= 1 {
                continue;
            }
            let end = (pl.row + pl.row_span).min(n_rows);
            let span_rows = end - pl.row;
            if span_rows == 0 {
                continue;
            }
            let have: f64 = row_h[pl.row..end].iter().sum();
            let need = pl.content_h + pl.border_width.top + pl.border_width.bottom;
            let deficit = need - have;
            if deficit > 0.05 {
                let add = deficit / span_rows as f64;
                for h in &mut row_h[pl.row..end] {
                    *h += add;
                }
            }
        }

        // Recompute the final row tops from the resolved heights.
        let mut acc_y = table_top;
        for r in 0..n_rows {
            row_top[r] = acc_y;
            acc_y += row_h[r];
        }

        // Placement pass: correct each cell vertically (translate from its
        // provisional top to the final one), apply `vertical-align`, then emit
        // the background and per-side borders over the cell's full merged rect.
        for pl in &placed {
            let (dx, cw) = span_geom(pl.start, pl.col_span);
            let cell_x = x + dx;
            let top = row_top[pl.row];
            let end_row = (pl.row + pl.row_span).min(n_rows).max(pl.row + 1);
            // Merged-cell height = sum of the rows it spans.
            let cell_h: f64 = row_h[pl.row..end_row].iter().sum::<f64>().max(0.1);

            // Translate the cell's content from its provisional top to its final
            // top, then add the `vertical-align` slack so a short cell sits
            // middle/bottom within a row sized by a taller peer or stretched by a
            // rowspan. Final content top = cell top + border.top + padding.top.
            let avail_content = (cell_h - pl.border_width.top - pl.border_width.bottom).max(0.0);
            let slack = (avail_content - pl.content_h).max(0.0);
            let valign_shift = match pl.vertical_align {
                VAlign::Top => 0.0,
                VAlign::Middle => slack / 2.0,
                VAlign::Bottom => slack,
            };
            let final_content_top = top + pl.border_width.top + pl.pad_top;
            let dy = (final_content_top - pl.prov_content_top) + valign_shift;
            if dy.abs() > 0.05 {
                for a in &mut self.out[pl.frag_lo..pl.frag_hi] {
                    shift_fragment(&mut a.frag, 0.0, dy);
                }
            }

            if let Some(fill) = pl.background {
                self.out.push(Abs {
                    z: 0,
                    zi: 0,
                    frag: Fragment::Rect {
                        x: cell_x,
                        y: top,
                        w: cw,
                        h: cell_h,
                        fill: Some(fill),
                        stroke: None,
                        stroke_w: 0.0,
                        opacity: pl.opacity,
                        radius: [0.0; 4],
                        radius_v: [0.0; 4],
                        shadow: None,
                    },
                });
            }

            // Per-side borders. In collapse mode draw top + left always, bottom
            // only when the cell reaches the last row, right only when it reaches
            // the last column — so interior edges (shared with the next cell down
            // / right) are drawn exactly once. Separate mode draws all four.
            let bw = &pl.border_width;
            let bc = &pl.border_color_edges;
            let bs = &pl.border_style_edges;
            let reaches_last_col = pl.start + pl.col_span.max(1) >= ncols.max(1);
            let reaches_last_row = end_row >= n_rows;
            let sides = if collapse {
                [
                    bw.top,
                    if reaches_last_col { bw.right } else { 0.0 },
                    if reaches_last_row { bw.bottom } else { 0.0 },
                    bw.left,
                ]
            } else {
                [bw.top, bw.right, bw.bottom, bw.left]
            };
            self.emit_border_edges(cell_x, top, cw, cell_h, sides, bc, bs, pl.opacity);
        }

        acc_y + style.margin.bottom
    }

    /// Emit a box's background at z=0: the solid `background` colour (when set)
    /// as a filled rect, then any `linear-gradient` painted over it as a
    /// [`Fragment::Gradient`] (CSS layers `background-image` above
    /// `background-color`). A box with neither emits nothing — the flat-fill path
    /// for ordinary boxes is unchanged. `radius`/`shadow` ride on the solid rect
    /// so a rounded card with a shadow is drawn correctly; the gradient overlay
    /// inherits the radius via its own clip.
    /// Emit one shadow-only rect per *extra* `box-shadow` layer (the 2nd…Nth),
    /// each carrying just its shadow (no fill/stroke), behind the box. They are
    /// pushed deepest-first so the earlier-listed layer paints on top (CSS
    /// back-to-front order). No-op when there are no extra layers, so the common
    /// single-shadow path is untouched.
    #[allow(clippy::too_many_arguments)]
    fn push_extra_shadow_layers(
        &mut self,
        style: &Style,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        radius: [f64; 4],
        radius_v: [f64; 4],
    ) {
        for layer in style.box_shadow_extra.iter().rev() {
            self.out.push(Abs {
                z: 0,
                zi: 0,
                frag: Fragment::Rect {
                    x,
                    y,
                    w,
                    h,
                    fill: None,
                    stroke: None,
                    stroke_w: 0.0,
                    opacity: style.opacity,
                    radius,
                    radius_v,
                    shadow: Some(*layer),
                },
            });
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn push_box_background(
        &mut self,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        style: &Style,
        radius: [f64; 4],
        radius_v: [f64; 4],
        shadow: Option<super::css::BoxShadow>,
    ) {
        // When the box has a shadow but no solid fill, still emit a (fill-less)
        // rect so the shadow is painted behind the box.
        if style.background.is_some() || shadow.is_some() {
            self.out.push(Abs {
                z: 0,
                zi: 0,
                frag: Fragment::Rect {
                    x,
                    y,
                    w,
                    h,
                    fill: style.background,
                    stroke: None,
                    stroke_w: 0.0,
                    opacity: style.opacity,
                    radius,
                    radius_v,
                    shadow,
                },
            });
        }
        if let Some(grad) = &style.background_gradient {
            self.out.push(Abs {
                z: 0,
                zi: 0,
                frag: Fragment::Gradient {
                    x,
                    y,
                    w,
                    h,
                    gradient: grad.clone(),
                    opacity: style.opacity,
                },
            });
        }
    }

    /// Emit a block box's background + border decoration at z=0, honouring
    /// `border-radius`, `box-shadow`, `linear-gradient` backgrounds and per-side
    /// `dashed`/`dotted`/`double` border styles. Shared by the normal `block`
    /// path and the multi-column block so both decorate identically.
    ///
    /// Behaviour is chosen to keep the square-corner, solid-border case
    /// byte-for-byte unchanged:
    /// * **No radius** ⇒ a plain fill rect for the background (plus a gradient
    ///   overlay if any) and per-side border rules via [`emit_border_edges`]
    ///   (the existing path; supports asymmetric `border-bottom: 2pt`,
    ///   dashed/dotted/double sides, etc.).
    /// * **Rounded + uniform solid border** (all four sides the same width &
    ///   colour, all `Solid`, no gradient) ⇒ one rounded fill rect carrying the
    ///   stroke, so background and border both follow the rounded contour.
    /// * **Rounded + asymmetric/styled border** ⇒ a rounded fill rect for the
    ///   background (correct), with the per-side borders still drawn via
    ///   [`emit_border_edges`] as a documented best-effort (mixing rounded
    ///   corners with differing/styled per-side borders is rare and has no clean
    ///   rectangular decomposition).
    fn emit_box_decoration(&mut self, style: &Style, x: f64, y: f64, w: f64, h: f64) {
        if style.hidden {
            return;
        }
        let b = &style.border_width;
        let any_border = b.top + b.bottom + b.left + b.right > 0.0;
        let any_bg = style.background.is_some() || style.background_gradient.is_some();
        let radius = clamp_radius(style.border_radius, w, h);
        let radius_v = clamp_radius(style.border_radius_v, w, h);
        let rounded = radius.iter().any(|r| *r > 0.0) || radius_v.iter().any(|r| *r > 0.0);
        let shadow = style.box_shadow;
        // Nothing to paint and no shadow ⇒ emit nothing (unchanged).
        if !any_border && !any_bg && shadow.is_none() && style.box_shadow_extra.is_empty() {
            return;
        }

        // Extra `box-shadow` layers (2nd…Nth) paint *behind* the topmost one (CSS
        // back-to-front): emit a shadow-only rect per layer first, so they stack
        // under the box. The topmost layer rides on the background/uniform rect
        // below (keeping the single-shadow path unchanged).
        self.push_extra_shadow_layers(style, x, y, w, h, radius, radius_v);

        // A rounded box can collapse the background + a uniform SOLID border into
        // a single rounded fill+stroke rect (so both follow the contour). Only
        // when there's no gradient overlay (which paints its own box).
        let all_solid = style.border_style_edges.iter().all(|s| *s == BorderStyle::Solid);
        let uniform_border = any_border
            && all_solid
            && (b.top - b.right).abs() < 1e-6
            && (b.top - b.bottom).abs() < 1e-6
            && (b.top - b.left).abs() < 1e-6
            && {
                let c = style.border_color_edges;
                c[0] == c[1] && c[0] == c[2] && c[0] == c[3]
            };

        if rounded && uniform_border && style.background_gradient.is_none() {
            // One rounded rect with both fill and stroke (carrying the shadow).
            self.out.push(Abs {
                z: 0,
                zi: 0,
                frag: Fragment::Rect {
                    x,
                    y,
                    w,
                    h,
                    fill: style.background,
                    stroke: Some(style.border_color_edges[0]),
                    stroke_w: b.top.max(0.0),
                    opacity: style.opacity,
                    radius,
                    radius_v,
                    shadow,
                },
            });
            return;
        }

        // Background (solid + gradient overlay), rounded when a radius is set,
        // carrying the shadow on its solid rect.
        self.push_box_background(x, y, w, h, style, radius, radius_v, shadow);
        if any_border {
            // Per-side borders — solid sides are exact filled rects; styled sides
            // become dash/dot/double bands. With a radius set this is the
            // asymmetric/styled best-effort fallback (corners stay square).
            self.emit_border_edges(
                x,
                y,
                w,
                h,
                [b.top, b.right, b.bottom, b.left],
                &style.border_color_edges,
                &style.border_style_edges,
                style.opacity,
            );
        }
    }

    /// Emit up to four per-side border rules for the box `(x, y, w, h)` (top-down).
    /// `widths`/`colors`/`styles` are `[top, right, bottom, left]`. A `Solid` side
    /// (the default) is a thin filled rectangle — exact per-side placement, width
    /// and colour, the only way to honour `border-bottom: 2pt` without thickening
    /// the other three sides — and is byte-identical to the pre-style behaviour.
    /// A `Dashed`/`Dotted`/`Double` side becomes a [`Fragment::Border`] band the
    /// paint layer expands into segments / parallel lines.
    #[allow(clippy::too_many_arguments)]
    fn emit_border_edges(
        &mut self,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        widths: [f64; 4],
        colors: &[[f64; 3]; 4],
        styles: &[BorderStyle; 4],
        opacity: f64,
    ) {
        // A `Solid` side: the legacy filled rectangle (corners overlap so the
        // frame joins cleanly — acceptable: same colour, opaque overlap).
        let push_solid = |out: &mut Vec<Abs>, rx: f64, ry: f64, rw: f64, rh: f64, c: [f64; 3]| {
            out.push(Abs {
                z: 0,
                zi: 0,
                frag: Fragment::Rect {
                    x: rx,
                    y: ry,
                    w: rw,
                    h: rh,
                    fill: Some(c),
                    stroke: None,
                    stroke_w: 0.0,
                    opacity,
                    radius: [0.0; 4],
                    radius_v: [0.0; 4],
                    shadow: None,
                },
            });
        };
        // A styled side: a band the painter turns into dashes / parallel lines.
        #[allow(clippy::too_many_arguments)]
        let push_styled = |out: &mut Vec<Abs>,
                           rx: f64,
                           ry: f64,
                           rw: f64,
                           rh: f64,
                           c: [f64; 3],
                           horizontal: bool,
                           width: f64,
                           style: BorderStyle| {
            out.push(Abs {
                z: 0,
                zi: 0,
                frag: Fragment::Border {
                    x: rx,
                    y: ry,
                    w: rw,
                    h: rh,
                    horizontal,
                    width,
                    color: c,
                    style,
                    opacity,
                },
            });
        };
        // Dispatch one side to the solid or styled emitter by its style.
        #[allow(clippy::too_many_arguments)]
        let emit_side = |out: &mut Vec<Abs>,
                         rx: f64,
                         ry: f64,
                         rw: f64,
                         rh: f64,
                         c: [f64; 3],
                         horizontal: bool,
                         width: f64,
                         style: BorderStyle| {
            match style {
                BorderStyle::Solid => push_solid(out, rx, ry, rw, rh, c),
                _ => push_styled(out, rx, ry, rw, rh, c, horizontal, width, style),
            }
        };
        let [wt, wr, wb, wl] = widths;
        let [st, sr, sb, sl] = *styles;
        if wt > 0.0 {
            emit_side(&mut self.out, x, y, w, wt, colors[0], true, wt, st); // top
        }
        if wb > 0.0 {
            emit_side(&mut self.out, x, y + h - wb, w, wb, colors[2], true, wb, sb); // bottom
        }
        if wl > 0.0 {
            emit_side(&mut self.out, x, y, wl, h, colors[3], false, wl, sl); // left
        }
        if wr > 0.0 {
            emit_side(&mut self.out, x + w - wr, y, wr, h, colors[1], false, wr, sr); // right
        }
    }

    /// Resolve the table's column widths (length `ncols`) to absolute points
    /// summing to `avail_w`. Declared widths come first from `<col>` elements
    /// (honouring `span`), else from the first row's per-cell `width`. Columns
    /// without a declared width split the remaining space equally; if every
    /// column is declared the widths are scaled proportionally to fit `avail_w`
    /// (browser fixed-layout). With nothing declared this yields equal columns.
    fn resolve_col_widths(
        &self,
        table: &Element,
        style: &Style,
        rows: &[&Element],
        na: &[&Element],
        avail_w: f64,
        ncols: usize,
    ) -> Vec<f64> {
        if ncols == 0 {
            return Vec::new();
        }
        let equal = avail_w / ncols as f64;
        let mut decl: Vec<Option<f64>> = vec![None; ncols];

        // Source 1: <colgroup>/<col> declarations (each <col span="N">).
        let cols = collect_cols(table);
        if !cols.is_empty() {
            let mut ci = 0usize;
            for c in cols {
                if ci >= ncols {
                    break;
                }
                let span = cell_colspan(c); // reads `span`/`colspan`
                let w = col_declared_width(c, avail_w);
                for k in 0..span.max(1) {
                    if ci + k < ncols {
                        // A multi-column <col> applies its width per column.
                        decl[ci + k] = w;
                    }
                }
                ci += span.max(1);
            }
        } else if let Some(first) = rows.first() {
            // Source 2: per-cell width on the first row's cells. A colspan cell
            // distributes its declared width equally over the columns it covers.
            let mut ci = 0usize;
            for cell in collect_cells(first) {
                if ci >= ncols {
                    break;
                }
                let span = cell_colspan(cell);
                let cstyle = self.style_of(cell, style, na);
                let w = cstyle.width.map(|len| match len {
                    Len::Pt(pt) => pt.max(0.0),
                    Len::Percent(pc) => avail_w * pc / 100.0,
                });
                if let Some(total) = w {
                    let per = total / span.max(1) as f64;
                    for k in 0..span.max(1) {
                        if ci + k < ncols {
                            decl[ci + k] = Some(per);
                        }
                    }
                }
                ci += span.max(1);
            }
        }

        let declared_sum: f64 = decl.iter().filter_map(|d| *d).sum();
        let undeclared = decl.iter().filter(|d| d.is_none()).count();

        if undeclared == 0 {
            // All columns declared: scale to fit avail_w (fixed-layout). Guard a
            // zero/degenerate sum by falling back to equal columns.
            if declared_sum > 0.0 {
                let scale = avail_w / declared_sum;
                decl.iter().map(|d| d.unwrap_or(equal) * scale).collect()
            } else {
                vec![equal; ncols]
            }
        } else {
            // Undeclared columns share whatever space the declared ones leave.
            let fill = ((avail_w - declared_sum).max(0.0)) / undeclared as f64;
            decl.iter().map(|d| d.unwrap_or(fill)).collect()
        }
    }

    /// A flex container. Supports `flex-direction` (row | column),
    /// `justify-content` (both axes), `flex-grow`, `flex-wrap`, `order`, and
    /// `align-items`/`align-self` (cross-axis). Shrinking is not modelled.
    fn flex(
        &mut self,
        el: &Element,
        style: &Style,
        x: f64,
        avail_w: f64,
        mut y: f64,
        ancestors: &[&Element],
    ) -> f64 {
        y += style.margin.top;
        let m = &style.margin;
        let p = &style.padding;
        let b = &style.border_width;
        let na = push_ancestor(ancestors, el);

        let mut items: Vec<&Element> = el
            .children
            .iter()
            .filter_map(|n| match n {
                Node::Element(e) => Some(e),
                _ => None,
            })
            .filter(|e| self.style_of(e, style, &na).display != Display::None)
            .collect();
        if items.is_empty() {
            return y + style.margin.bottom;
        }
        // `order` reorders items for layout (stable; ties keep document order).
        items.sort_by_key(|e| self.style_of(e, style, &na).order);
        // `row-reverse` / `column-reverse` run the main axis from the far end:
        // reversing the placement order achieves that for every downstream axis
        // / wrap / justify-content path.
        if style.flex_reverse {
            items.reverse();
        }

        let content_x = x + m.left + b.left + p.left;
        let content_w = (avail_w - m.left - m.right - b.left - b.right - p.left - p.right).max(1.0);
        let row_top = y + b.top + p.top;

        let row_bottom = if style.flex_column {
            self.flex_column_axis(&items, style, content_x, content_w, row_top, &na)
        } else {
            self.flex_row_axis(&items, style, content_x, content_w, row_top, &na)
        };

        row_bottom + p.bottom + b.bottom + style.margin.bottom
    }

    /// The cross-axis alignment used for a flex item: its `align-self` if set,
    /// else the container's `align-items`.
    fn item_align(&self, item_style: &Style, container: &Style) -> AlignItems {
        item_style.align_self.unwrap_or(container.align_items)
    }

    /// Horizontal flex: resolve item main-axis widths and place them
    /// left-to-right. With `flex-wrap` the items break into successive flex
    /// lines whenever their explicit widths overflow `content_w`. Returns the
    /// bottom `y` of the last line.
    fn flex_row_axis(
        &mut self,
        items: &[&Element],
        style: &Style,
        content_x: f64,
        content_w: f64,
        row_top: f64,
        na: &[&Element],
    ) -> f64 {
        // Break items into flex lines. Wrap only applies when widths are
        // explicit (the fill model always fits by construction).
        let lines = self.flex_wrap_lines(items, style, content_w, na);
        let mut y = row_top;
        let row_gap = style.gap_row;
        for (li, line) in lines.iter().enumerate() {
            if li > 0 {
                y += row_gap;
            }
            y = self.flex_row_line(line, style, content_x, content_w, y, na);
        }
        y
    }

    /// Partition flex items into lines for `flex-wrap`. Without wrap (or with no
    /// explicit widths) every item stays on a single line.
    fn flex_wrap_lines<'b>(
        &self,
        items: &[&'b Element],
        style: &Style,
        content_w: f64,
        na: &[&Element],
    ) -> Vec<Vec<&'b Element>> {
        let any_explicit = items.iter().any(|it| {
            let st = self.style_of(it, style, na);
            st.flex_basis.is_some() || st.width.is_some()
        });
        if !style.flex_wrap || !any_explicit {
            return vec![items.to_vec()];
        }
        let gap = style.gap_col;
        let mut lines: Vec<Vec<&Element>> = Vec::new();
        let mut cur: Vec<&Element> = Vec::new();
        let mut used = 0.0;
        for it in items {
            let st = self.style_of(it, style, na);
            // Wrap decision uses the flex base size (basis, else width).
            let w = match st.flex_basis.or(st.width) {
                Some(Len::Pt(w)) => w.max(0.0),
                Some(Len::Percent(pc)) => content_w * pc / 100.0,
                None => 0.0,
            };
            let add = if cur.is_empty() { w } else { w + gap };
            if !cur.is_empty() && used + add > content_w + 0.01 {
                lines.push(std::mem::take(&mut cur));
                used = w;
            } else {
                used += add;
            }
            cur.push(it);
        }
        if !cur.is_empty() {
            lines.push(cur);
        }
        lines
    }

    /// Lay out a single flex line of items at `row_top`, applying the width
    /// model, `justify-content` (main axis) and `align-items`/`align-self`
    /// (cross axis). Returns the line's bottom `y`.
    fn flex_row_line(
        &mut self,
        items: &[&Element],
        style: &Style,
        content_x: f64,
        content_w: f64,
        row_top: f64,
        na: &[&Element],
    ) -> f64 {
        let n = items.len();
        if n == 0 {
            return row_top;
        }
        let gap = style.gap_col;
        let gaps_w = gap * (n.saturating_sub(1)) as f64;
        let avail = (content_w - gaps_w).max(0.0);

        // Each item's resolved main-axis basis. The basis is `flex-basis` when
        // set, else `width` (both resolve % against `avail`); `NaN` marks an
        // `auto`/`content` basis to be filled from the leftover. `flex-grow` and
        // `flex-shrink` factors are collected alongside for free-space handling.
        let mut ws: Vec<f64> = Vec::with_capacity(n);
        let mut grows: Vec<f64> = Vec::with_capacity(n);
        let mut shrinks: Vec<f64> = Vec::with_capacity(n);
        let mut any_explicit = false;
        let resolve_len = |len: Len| match len {
            Len::Pt(w) => w.max(0.0),
            Len::Percent(pc) => avail * pc / 100.0,
        };
        for it in items {
            let st = self.style_of(it, style, na);
            grows.push(st.flex_grow.max(0.0));
            shrinks.push(st.flex_shrink.max(0.0));
            // `flex-basis` takes precedence over `width` for the main size.
            match st.flex_basis.or(st.width) {
                Some(len) => {
                    let base = resolve_len(len);
                    ws.push(base);
                    // A `flex-basis: 0` (e.g. from `flex: 3`) is NOT a fixed
                    // width: it means "start at 0, then grow". Treating it as
                    // explicit would let auto-basis siblings eat the leftover
                    // before grow runs. Only a genuinely sized basis (or width)
                    // makes the line use the explicit-width model.
                    if base > 0.01 {
                        any_explicit = true;
                    }
                }
                None => ws.push(f64::NAN), // resolved below
            }
        }
        let total_grow: f64 = grows.iter().sum();

        let (offset, extra_gap) = if any_explicit {
            // Items with an `auto` basis share the leftover equally as their base.
            let known: f64 = ws.iter().filter(|w| !w.is_nan()).sum();
            let unknown = ws.iter().filter(|w| w.is_nan()).count();
            let fill = if unknown > 0 {
                (avail - known).max(0.0) / unknown as f64
            } else {
                0.0
            };
            for w in ws.iter_mut() {
                if w.is_nan() {
                    *w = fill;
                }
            }
            let mut free = avail - ws.iter().sum::<f64>();
            if free > 0.0 && total_grow > 0.0 {
                // Positive free space: grow flexible items by their grow factor.
                for (w, g) in ws.iter_mut().zip(&grows) {
                    *w += free * g / total_grow;
                }
                free = 0.0;
            } else if free < 0.0 {
                // Overflow: shrink items weighted by `flex-shrink × basis` (the
                // CSS scaled-shrink factor), so wider/more-shrinkable items give
                // up proportionally more. Items reaching 0 stop contributing.
                shrink_to_fit(&mut ws, &shrinks, -free);
                free = (avail - ws.iter().sum::<f64>()).max(0.0);
            }
            justify_offsets(style.justify, free.max(0.0), n)
        } else {
            // Fill model: weight = 1 + grow, sums to `avail` exactly.
            let total_w: f64 = grows.iter().map(|g| 1.0 + g).sum();
            for (w, g) in ws.iter_mut().zip(&grows) {
                *w = avail * (1.0 + g) / total_w;
            }
            (0.0, 0.0)
        };

        let mut xs: Vec<f64> = Vec::with_capacity(n);
        let mut cx = content_x + offset;
        for w in &ws {
            xs.push(cx);
            cx += w + gap + extra_gap;
        }

        // Lay out each item, recording its fragment range + natural height so
        // cross-axis alignment can shift shorter items within the line band.
        let mut heights: Vec<f64> = Vec::with_capacity(n);
        let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(n);
        let mut row_bottom = row_top;
        for (i, it) in items.iter().enumerate() {
            let istyle = self.style_of(it, style, na);
            let nca = push_ancestor(na, it);
            let ip = &istyle.padding;
            let ib = &istyle.border_width;
            let start = self.out.len();
            let cy = self.block_children(
                &it.children,
                &istyle,
                xs[i] + ip.left + ib.left,
                (ws[i] - ip.left - ip.right - ib.left - ib.right).max(1.0),
                row_top + ip.top + ib.top,
                &nca,
            );
            let item_h = (cy + ip.bottom + ib.bottom - row_top).max(0.0);
            heights.push(item_h);
            ranges.push((start, self.out.len()));
            row_bottom = row_bottom.max(row_top + item_h);
        }

        let line_h = row_bottom - row_top;
        // Cross-axis alignment: stretch fills the band (no shift); start/center/
        // end position the natural-height item within it.
        for (i, it) in items.iter().enumerate() {
            let istyle = self.style_of(it, style, na);
            let dy = match self.item_align(&istyle, style) {
                AlignItems::Stretch | AlignItems::Start => 0.0,
                AlignItems::Center => (line_h - heights[i]) / 2.0,
                AlignItems::End => line_h - heights[i],
            };
            if dy.abs() > f64::EPSILON {
                let (s, e) = ranges[i];
                for a in &mut self.out[s..e] {
                    shift_fragment(&mut a.frag, 0.0, dy);
                }
            }
            // Backgrounds: stretched items fill the band, others wrap content.
            let box_h = match self.item_align(&istyle, style) {
                AlignItems::Stretch => line_h,
                _ => heights[i],
            };
            self.paint_item_box(&istyle, xs[i], row_top + dy.max(0.0), ws[i], box_h);
        }
        row_bottom
    }

    /// Vertical flex: stack items top-to-bottom. `row-gap` separates items,
    /// `align-items`/`align-self` position each item on the cross (horizontal)
    /// axis, and — when the container has an explicit height (`min_height`) with
    /// leftover space — `justify-content` distributes that space along the
    /// block axis.
    fn flex_column_axis(
        &mut self,
        items: &[&Element],
        style: &Style,
        content_x: f64,
        content_w: f64,
        row_top: f64,
        na: &[&Element],
    ) -> f64 {
        let gap = style.gap_row;
        let mut y = row_top;
        // Per-item fragment ranges so we can redistribute for justify-content.
        let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(items.len());
        for (i, it) in items.iter().enumerate() {
            if i > 0 {
                y += gap;
            }
            let istyle = self.style_of(it, style, na);
            let nca = push_ancestor(na, it);
            let im = &istyle.margin;
            let ip = &istyle.padding;
            let ib = &istyle.border_width;
            y += im.top;
            let item_top = y;
            // Cross-axis (horizontal) sizing: stretch fills the width; otherwise
            // the item is laid out at its natural content width.
            let cross = self.item_align(&istyle, style);
            let full_inner =
                (content_w - im.left - im.right - ib.left - ib.right - ip.left - ip.right).max(1.0);
            let item_w = match istyle.width {
                Some(Len::Pt(w)) => w.max(0.0),
                Some(Len::Percent(pc)) => content_w * pc / 100.0,
                None => content_w - im.left - im.right,
            };
            let (box_w, inner_w) = if cross == AlignItems::Stretch && istyle.width.is_none() {
                ((content_w - im.left - im.right).max(0.1), full_inner)
            } else {
                let bw = item_w.max(0.1);
                (bw, (bw - ib.left - ib.right - ip.left - ip.right).max(1.0))
            };
            let dx = match cross {
                AlignItems::Stretch | AlignItems::Start => 0.0,
                AlignItems::Center => (content_w - im.left - im.right - box_w) / 2.0,
                AlignItems::End => content_w - im.left - im.right - box_w,
            }
            .max(0.0);
            let start = self.out.len();
            let cy = self.block_children(
                &it.children,
                &istyle,
                content_x + im.left + dx + ib.left + ip.left,
                inner_w,
                y + ip.top + ib.top,
                &nca,
            );
            let item_bottom = cy + ip.bottom + ib.bottom;
            self.paint_item_box(
                &istyle,
                content_x + im.left + dx,
                item_top,
                box_w,
                item_bottom - item_top,
            );
            ranges.push((start, self.out.len()));
            y = item_bottom + im.bottom;
        }

        // Block-axis `justify-content`: only meaningful with an explicit
        // container height that exceeds the content. Distribute the free space.
        if let Some(h) = style.height.or(style.min_height) {
            let used = y - row_top;
            let free = h - used;
            if free > 0.01 && !items.is_empty() {
                let (offset, item_gap) = justify_offsets(style.justify, free, items.len());
                for (i, (s, e)) in ranges.iter().enumerate() {
                    let dy = offset + item_gap * i as f64;
                    if dy.abs() > f64::EPSILON {
                        for a in &mut self.out[*s..*e] {
                            shift_fragment(&mut a.frag, 0.0, dy);
                        }
                    }
                }
                return row_top + h;
            }
        }
        y
    }

    /// A CSS grid (`grid-template-columns`/`-rows`). Column widths come from the
    /// track list (`fr`, `minmax`, fixed px/pt, `%`, `auto`, `repeat`), separated
    /// by `column-gap`; rows are sized to their tallest cell unless a row track
    /// gives an explicit height, separated by `row-gap`. Items honour
    /// `grid-column`/`grid-row` start lines and `span N`. With no track list the
    /// grid falls back to equal columns (the legacy behaviour).
    fn grid(
        &mut self,
        el: &Element,
        style: &Style,
        x: f64,
        avail_w: f64,
        mut y: f64,
        ancestors: &[&Element],
    ) -> f64 {
        y += style.margin.top;
        let m = &style.margin;
        let p = &style.padding;
        let b = &style.border_width;
        let na = push_ancestor(ancestors, el);

        let items: Vec<&Element> = el
            .children
            .iter()
            .filter_map(|n| match n {
                Node::Element(e) => Some(e),
                _ => None,
            })
            .filter(|e| self.style_of(e, style, &na).display != Display::None)
            .collect();
        if items.is_empty() {
            return y + style.margin.bottom;
        }

        let cols = style.grid_columns.max(1);
        let content_x = x + m.left + b.left + p.left;
        let content_w = (avail_w - m.left - m.right - b.left - b.right - p.left - p.right).max(1.0);
        let y_cursor = y + b.top + p.top;
        let gap_col = style.gap_col.max(0.0);
        let gap_row = style.gap_row.max(0.0);

        // Resolve per-column widths from the track list (or equal columns when no
        // track list is present), then the cumulative x of each column's left
        // edge (including the column gaps that precede it).
        let col_widths = resolve_track_sizes(&style.grid_template_columns, cols, content_w, gap_col);
        let col_x: Vec<f64> = cumulative_offsets(content_x, &col_widths, gap_col);

        // Assign each item a (row, col) cell + span. Items with an explicit
        // `grid-column`/`grid-row` start line land on that line; the rest
        // auto-flow into the next free cell (left-to-right, top-to-bottom).
        let placement = self.place_grid_items(&items, style, &na, cols);
        let row_count = placement
            .iter()
            .map(|p| p.row + p.row_span)
            .max()
            .unwrap_or(0);

        // The x span (left edge → right edge) of a cell covering
        // `[col, col+span)`: from `col_x[col]` to the right edge of the last
        // spanned column, so the interior column gaps are absorbed into the cell.
        let span_x = |col: usize, span: usize| -> (f64, f64) {
            let start = col_x[col.min(cols - 1)];
            let last = (col + span).min(cols).saturating_sub(1);
            let end = col_x[last] + col_widths[last];
            (start, (end - start).max(1.0))
        };

        // Explicit row-track heights (if any), indexed by row; `None` = auto
        // (sized to the tallest cell). `auto`/`fr` rows resolve to auto here
        // (no fixed height), matching the content-sized default.
        let row_track_h = |r: usize| -> Option<f64> {
            style
                .grid_template_rows
                .get(r)
                .and_then(track_fixed_height)
        };

        // Lay each row's cells out, tracking the tallest cell so the whole row
        // shares one height. A row-spanning cell only contributes its height to
        // its LAST row (so earlier rows aren't inflated by it); single-row cells
        // size their row directly.
        let mut row_tops = vec![y_cursor; row_count];
        let mut row_bottoms = vec![y_cursor; row_count];
        // First pass: place content for single-row cells, sizing each row.
        for r in 0..row_count {
            let row_top = if r == 0 {
                y_cursor
            } else {
                row_bottoms[r - 1] + gap_row
            };
            row_tops[r] = row_top;
            let mut row_bottom = row_top + row_track_h(r).unwrap_or(0.0);
            for cell in placement.iter().filter(|p| p.row == r && p.row_span == 1) {
                let it = items[cell.item];
                let istyle = self.style_of(it, style, &na);
                let nca = push_ancestor(&na, it);
                let ip = &istyle.padding;
                let ib = &istyle.border_width;
                let (cx, cw) = span_x(cell.col, cell.col_span);
                let cy = self.block_children(
                    &it.children,
                    &istyle,
                    cx + ip.left + ib.left,
                    (cw - ip.left - ip.right - ib.left - ib.right).max(1.0),
                    row_top + ip.top + ib.top,
                    &nca,
                );
                row_bottom = row_bottom.max(cy + ip.bottom + ib.bottom);
            }
            row_bottoms[r] = row_bottom.max(row_top);
        }
        // Second pass: row-spanning cells. Lay them out within their first row's
        // top; if their content overflows the spanned rows, grow the last row so
        // the grid still contains them (and shift later rows down).
        for cell in placement.iter().filter(|p| p.row_span > 1) {
            let it = items[cell.item];
            let istyle = self.style_of(it, style, &na);
            let nca = push_ancestor(&na, it);
            let ip = &istyle.padding;
            let ib = &istyle.border_width;
            let (cx, cw) = span_x(cell.col, cell.col_span);
            let top = row_tops[cell.row];
            let cy = self.block_children(
                &it.children,
                &istyle,
                cx + ip.left + ib.left,
                (cw - ip.left - ip.right - ib.left - ib.right).max(1.0),
                top + ip.top + ib.top,
                &nca,
            );
            let needed = cy + ip.bottom + ib.bottom;
            let last_row = (cell.row + cell.row_span - 1).min(row_count - 1);
            if needed > row_bottoms[last_row] {
                let grow = needed - row_bottoms[last_row];
                // Push the last spanned row's bottom and all subsequent rows down.
                for r in last_row..row_count {
                    row_tops[r] += if r == last_row { 0.0 } else { grow };
                    row_bottoms[r] += grow;
                }
            }
        }

        // Paint each cell's box spanning its full (row × column) area.
        for cell in &placement {
            let it = items[cell.item];
            let istyle = self.style_of(it, style, &na);
            let (cx, cw) = span_x(cell.col, cell.col_span);
            let last_row = (cell.row + cell.row_span - 1).min(row_count - 1);
            let top = row_tops[cell.row];
            let bottom = row_bottoms[last_row];
            self.paint_item_box(&istyle, cx, top, cw, (bottom - top).max(0.0));
        }
        let grid_bottom = row_bottoms.last().copied().unwrap_or(y_cursor);
        grid_bottom + p.bottom + b.bottom + style.margin.bottom
    }

    /// Resolve each grid item to a `(row, col)` cell in a fixed-`cols` grid.
    ///
    /// Honours explicit `grid-column`/`grid-row` start lines (1-based, clamped to
    /// the column count); items without an explicit column auto-flow into the next
    /// free cell scanning columns then rows, and items without an explicit row but
    /// with an explicit column take the next free row in that column. A cell is
    /// "occupied" once any item is placed there, so an explicitly-placed item is
    /// skipped over by the auto-flow cursor. With no explicit placement at all the
    /// result is the plain row-major fill (identical to `chunks(cols)`).
    fn place_grid_items(
        &mut self,
        items: &[&Element],
        style: &Style,
        na: &[&Element],
        cols: usize,
    ) -> Vec<GridPlace> {
        let mut occupied: Vec<bool> = Vec::new();
        let idx = |row: usize, col: usize| row * cols + col;
        let ensure_rows = |occ: &mut Vec<bool>, rows: usize| {
            if occ.len() < rows * cols {
                occ.resize(rows * cols, false);
            }
        };
        // Is the `[col, col+span) × [row, row+row_span)` block entirely free?
        let block_free = |occ: &mut Vec<bool>, row: usize, col: usize, cs: usize, rs: usize| {
            ensure_rows(occ, row + rs);
            for r in row..row + rs {
                for c in col..col + cs {
                    if occ[idx(r, c)] {
                        return false;
                    }
                }
            }
            true
        };
        let mut cursor = 0usize; // auto-flow scan position (row-major).
        let mut out = Vec::with_capacity(items.len());

        for (i, it) in items.iter().enumerate() {
            let istyle = self.style_of(it, style, na);
            // Column span clamped so the cell never runs past the last column.
            let col_span = istyle.grid_col_span.max(1).min(cols);
            let row_span = istyle.grid_row_span.max(1);
            // Explicit column (1-based) clamped so the spanned cell fits; 0 = auto.
            let col_hint = if istyle.grid_col_start >= 1 && istyle.grid_col_start <= cols {
                Some((istyle.grid_col_start - 1).min(cols - col_span))
            } else {
                None
            };
            let row_hint = istyle.grid_row_start.checked_sub(1); // 0 = auto → None.

            let (row, col) = match (row_hint, col_hint) {
                // Both explicit: place exactly there (growing the grid as needed).
                (Some(r), Some(c)) => (r, c),
                // Column only: next free row range in that column.
                (None, Some(c)) => {
                    let mut r = 0;
                    while !block_free(&mut occupied, r, c, col_span, row_span) {
                        r += 1;
                    }
                    (r, c)
                }
                // Row only: next free column run in that row (then overflow down).
                (Some(r), None) => {
                    let mut rr = r;
                    let mut cc = 0;
                    loop {
                        if cc + col_span <= cols
                            && block_free(&mut occupied, rr, cc, col_span, row_span)
                        {
                            break (rr, cc);
                        }
                        cc += 1;
                        if cc + col_span > cols {
                            cc = 0;
                            rr += 1;
                        }
                    }
                }
                // Fully auto: advance the row-major cursor to the next free run
                // that fits the column span.
                (None, None) => loop {
                    let r = cursor / cols;
                    let c = cursor % cols;
                    cursor += 1;
                    if c + col_span <= cols && block_free(&mut occupied, r, c, col_span, row_span) {
                        break (r, c);
                    }
                },
            };
            ensure_rows(&mut occupied, row + row_span);
            for r in row..row + row_span {
                for c in col..(col + col_span).min(cols) {
                    occupied[idx(r, c)] = true;
                }
            }
            out.push(GridPlace {
                item: i,
                row,
                col,
                col_span,
                row_span,
            });
        }
        out
    }

    /// Lay a normal block's flow content out into `column_count` equal-width
    /// columns (CSS multi-column: `column-count` / `columns`, gutter from
    /// `column-gap`, falling back to a 1em gutter when unset).
    ///
    /// The block's own box (margin / border / padding / width / background) is
    /// handled exactly like [`Flow::block`]; only its *content* is split. Flow
    /// units — each top-level block child, and each maximal run of inline-level
    /// siblings — are distributed left-to-right and **height-balanced**: a
    /// measure pass lays every unit out once (at the column width) to total the
    /// content height, the target per column is `total / N`, and the place pass
    /// fills each column until adding the next unit would pass the target (then
    /// it moves to the next column, except in the last one which takes the rest).
    ///
    /// Pagination interaction: columns are emitted in absolute coordinates, so a
    /// region that overflows a single page is sliced by the y-band paginator like
    /// any other content (each column simply continues down the next page band).
    /// True column-then-page balancing across multiple pages is **not** modelled;
    /// the single-page case (the common newsletter/report block) is exact.
    fn columns(
        &mut self,
        el: &Element,
        style: &Style,
        x: f64,
        avail_w: f64,
        mut y: f64,
        ancestors: &[&Element],
    ) -> f64 {
        let m = &style.margin;
        let p = &style.padding;
        let b = &style.border_width;

        y += m.top;
        let box_top = y;
        let box_x = x + m.left;
        let resolve_w = |len: Len| match len {
            Len::Pt(w) => w,
            Len::Percent(pc) => avail_w * pc / 100.0,
        };
        // Box width, matching `block`'s sizing (incl. `box-sizing`/min/max).
        let mut box_w = match style.width {
            Some(Len::Pt(w)) if style.border_box => w,
            Some(Len::Pt(w)) => w + p.left + p.right + b.left + b.right,
            Some(Len::Percent(pc)) => avail_w * pc / 100.0,
            None => avail_w - m.left - m.right,
        };
        if let Some(mw) = style.max_width {
            box_w = box_w.min(resolve_w(mw));
        }
        if let Some(mw) = style.min_width {
            box_w = box_w.max(resolve_w(mw));
        }
        let content_x = box_x + b.left + p.left;
        let content_w = (box_w - b.left - b.right - p.left - p.right).max(1.0);
        let content_top = box_top + b.top + p.top;

        let n = style.column_count.max(1);
        // Gutter: explicit `column-gap`, else a 1em default (CSS `normal`).
        let gap = if style.gap_col > 0.0 {
            style.gap_col
        } else {
            style.font_size
        };
        // Equal column width: total minus the (n-1) gutters, split n ways.
        let col_w = ((content_w - gap * (n - 1) as f64) / n as f64).max(1.0);
        let col_x = |c: usize| content_x + c as f64 * (col_w + gap);

        let new_ancestors = push_ancestor(ancestors, el);
        // Partition the children into flow units (block child | inline run).
        let units = flow_units(&el.children, style, &new_ancestors, self);

        // ── Measure pass: lay each unit out once at the column width, record its
        // height, then discard the probe fragments (truncate `self.out`). ──
        let mut heights: Vec<f64> = Vec::with_capacity(units.len());
        for unit in &units {
            let probe_start = self.out.len();
            let bottom =
                self.lay_unit(unit, style, content_x, col_w, content_top, &new_ancestors);
            self.out.truncate(probe_start);
            heights.push((bottom - content_top).max(0.0));
        }
        let total: f64 = heights.iter().sum();
        let target = total / n as f64;

        // ── Place pass: greedily fill columns to the balanced target. Each unit
        // is re-laid at its column's running `y`; a column advances when the next
        // unit would push it past the target (never on the last column). ──
        let mut col = 0usize;
        let mut col_y = content_top;
        let mut max_bottom = content_top;
        for (unit, uh) in units.iter().zip(&heights) {
            // Move to the next column when this column already holds content and
            // adding the unit would overshoot the balanced target. Skip degenerate
            // units (height 0) so they never trigger a premature break.
            if col + 1 < n
                && col_y > content_top + 0.05
                && *uh > 0.0
                && (col_y - content_top) + *uh > target + 0.05
            {
                col += 1;
                col_y = content_top;
            }
            let bottom =
                self.lay_unit(unit, style, col_x(col), col_w, col_y, &new_ancestors);
            col_y = bottom;
            max_bottom = max_bottom.max(col_y);
        }

        let mut box_h = style
            .height
            .unwrap_or((max_bottom + p.bottom + b.bottom - box_top).max(0.1));
        if let Some(mh) = style.min_height {
            box_h = box_h.max(mh);
        }

        // Background + per-side borders behind the columns (z=0), like `block`
        // (honours `border-radius` / `box-shadow` / gradients / styled borders
        // via the shared helper).
        self.emit_box_decoration(style, box_x, box_top, box_w, box_h);

        box_top + box_h + m.bottom
    }

    /// Lay one flow unit (a block child, or a run of inline siblings) out at
    /// `(x, avail_w, y)`, returning its bottom `y`. The shared building block of
    /// the multi-column placer (measure pass + place pass), so a unit is laid out
    /// identically wherever it lands.
    fn lay_unit(
        &mut self,
        unit: &FlowUnit,
        parent_style: &Style,
        x: f64,
        avail_w: f64,
        y: f64,
        ancestors: &[&Element],
    ) -> f64 {
        match unit {
            FlowUnit::Block { el, list_index } => {
                let st = self.style_of(el, parent_style, ancestors);
                self.block(el, &st, parent_style, x, avail_w, y, ancestors, *list_index)
            }
            FlowUnit::Inline(nodes) => {
                self.inline_context_f(nodes, parent_style, x, avail_w, y, ancestors)
            }
        }
    }

    /// Paint a flex/grid item's background + border as a single rect spanning
    /// its cell (z=0, behind the item's own content). A `linear-gradient`
    /// background is layered over the solid fill (same as `block` boxes), and a
    /// rounded corner / drop shadow on the item ride on the rect.
    fn paint_item_box(&mut self, istyle: &Style, x: f64, y: f64, w: f64, h: f64) {
        let has_border = istyle.border_width.top > 0.0;
        let has_grad = istyle.background_gradient.is_some();
        if istyle.background.is_some() || has_border || has_grad || istyle.box_shadow.is_some() {
            self.out.push(Abs {
                z: 0,
                zi: 0,
                frag: Fragment::Rect {
                    x,
                    y,
                    w,
                    h: h.max(0.1),
                    fill: istyle.background,
                    stroke: if has_border {
                        Some(istyle.border_color)
                    } else {
                        None
                    },
                    stroke_w: istyle.border_width.top.max(0.0),
                    opacity: istyle.opacity,
                    // Flex/grid item boxes paint a uniform stroked rect; honour a
                    // rounded corner and drop shadow here too so a rounded flex
                    // card is correct.
                    radius: clamp_radius(istyle.border_radius, w, h.max(0.1)),
                    radius_v: clamp_radius(istyle.border_radius_v, w, h.max(0.1)),
                    shadow: istyle.box_shadow,
                },
            });
            if let Some(grad) = &istyle.background_gradient {
                self.out.push(Abs {
                    z: 0,
                    zi: 0,
                    frag: Fragment::Gradient {
                        x,
                        y,
                        w,
                        h: h.max(0.1),
                        gradient: grad.clone(),
                        opacity: istyle.opacity,
                    },
                });
            }
        }
    }

    fn style_of(&self, el: &Element, parent: &Style, ancestors: &[&Element]) -> Style {
        self.sheet.computed(el, parent, ancestors)
    }
}

fn default_line_height(style: &Style) -> f64 {
    style.font_size * style.line_height.max(1.0)
}

/// Push a text run's highlight rectangle behind its glyphs (z=0), when the run
/// carries a `background` (e.g. `<mark>` or a span with `background-color`). The
/// box spans the run's measured content width `word.w` and one line-box height,
/// so consecutive highlighted words tile into a continuous band. A run without a
/// background emits nothing, leaving the highlight-free output byte-for-byte
/// unchanged. `background` is a non-inherited property, so a block ancestor's
/// background never lands on a text run here — only the run's own highlight does.
fn push_run_highlight(out: &mut Vec<Abs>, x: f64, y: f64, word: &Word) {
    let Some(fill) = word.style.background else {
        return;
    };
    let h = word.style.font_size * word.style.line_height.max(1.0);
    out.push(Abs {
        z: 0,
        zi: 0,
        frag: Fragment::Rect {
            x,
            y,
            w: word.w.max(0.0),
            h,
            fill: Some(fill),
            stroke: None,
            stroke_w: 0.0,
            opacity: word.style.opacity,
            radius: [0.0; 4],
            radius_v: [0.0; 4],
            shadow: None,
        },
    });
}

/// Resolve a `grid-template-columns`/`-rows` track list into `cols` concrete
/// sizes (points), distributing the leftover space across `fr` tracks.
///
/// Sizing follows the documents'-eye-view of the CSS algorithm:
/// 1. Fixed (`px`/`pt`) and `%` tracks take their resolved size first.
/// 2. `auto` tracks (and `auto`/`min-content` sides of `minmax`) get an equal
///    share of whatever space remains alongside the `fr` distribution, treated
///    as a flexible weight of `1` so a bare `auto` column still gets room.
/// 3. `fr` tracks split the remaining free space in proportion to their factor;
///    a `minmax(min, max)` track is clamped between its resolved bounds.
///
/// When the track list is empty (or shorter than `cols`), the missing columns
/// fall back to equal widths — preserving the legacy equal-column grid. `total`
/// is the content size to fill; `gap` is the gutter between adjacent tracks
/// (subtracted once per interior gap before distribution).
fn resolve_track_sizes(tracks: &[TrackSize], cols: usize, total: f64, gap: f64) -> Vec<f64> {
    let cols = cols.max(1);
    let inner = (total - gap * cols.saturating_sub(1) as f64).max(0.0);

    // Fall back to equal columns when no usable track list is present.
    if tracks.is_empty() {
        return vec![inner / cols as f64; cols];
    }

    // Build a per-column track sizing, padding short lists with `auto`.
    let get = |i: usize| tracks.get(i).cloned().unwrap_or(TrackSize::Auto);

    // First pass: resolve fixed/percent minimums; collect flexible weights.
    let mut sizes = vec![0.0f64; cols];
    let mut fr_weight = vec![0.0f64; cols]; // fr factor (or 1 for auto)
    let mut fixed_total = 0.0;
    let mut total_fr = 0.0;
    for i in 0..cols {
        let (base, fr) = track_base_and_fr(&get(i), inner);
        sizes[i] = base;
        fixed_total += base;
        if fr > 0.0 {
            fr_weight[i] = fr;
            total_fr += fr;
        }
    }

    // Distribute the free space across flexible tracks by their fr weight.
    let free = (inner - fixed_total).max(0.0);
    if total_fr > 0.0 {
        for i in 0..cols {
            if fr_weight[i] > 0.0 {
                let mut add = free * fr_weight[i] / total_fr;
                // Clamp `minmax(min, max)` flexible tracks to their max bound.
                if let TrackSize::MinMax(_, max) = get(i) {
                    if let Some(cap) = track_fixed_height(&max) {
                        add = add.min((cap - sizes[i]).max(0.0));
                    }
                }
                sizes[i] += add;
            }
        }
    } else if free > 0.0 {
        // No flexible tracks but space remains: spread it equally so the grid
        // still fills its width (e.g. an all-fixed list narrower than the box).
        let share = free / cols as f64;
        for s in sizes.iter_mut() {
            *s += share;
        }
    }
    sizes
}

/// The fixed base size (points) and flexible `fr` weight of one track against a
/// container `inner` size. Fixed/percent tracks return `(size, 0)`; `fr` tracks
/// return `(0, factor)`; `auto`/intrinsic tracks return `(0, 1)` so they share
/// leftover space like a 1fr track. `minmax(min, max)` uses `min` as the base
/// and is flexible up to `max` (weight 1) — a pragmatic reading that gives the
/// track its minimum then lets it grow.
fn track_base_and_fr(track: &TrackSize, inner: f64) -> (f64, f64) {
    match track {
        TrackSize::Pt(p) => (p.max(0.0), 0.0),
        TrackSize::Percent(pc) => ((inner * pc / 100.0).max(0.0), 0.0),
        TrackSize::Fr(f) => (0.0, f.max(0.0)),
        TrackSize::Auto => (0.0, 1.0),
        TrackSize::MinMax(min, _max) => {
            // Base = resolved min (fixed/percent); the track stays flexible.
            let base = track_fixed_height(min).unwrap_or(0.0);
            (base, 1.0)
        }
    }
}

/// The fixed height (points) of a row track, or `None` for `auto`/`fr`
/// (content-sized). Used both for explicit row heights and `minmax` max bounds.
/// Percentages can't be resolved without the container height here, so they are
/// treated as auto (content-sized) for rows.
fn track_fixed_height(track: &TrackSize) -> Option<f64> {
    match track {
        TrackSize::Pt(p) => Some(p.max(0.0)),
        TrackSize::MinMax(_, max) => track_fixed_height(max),
        _ => None,
    }
}

/// Reduce `widths` in place to absorb `overflow` points, distributing the
/// reduction by the CSS scaled flex-shrink factor (`shrink × basis`) so wider
/// and more-shrinkable items give up proportionally more. Items that would go
/// below 0 are clamped to 0 and frozen, and the remaining overflow is
/// redistributed across the still-shrinkable items (bounded iteration).
fn shrink_to_fit(widths: &mut [f64], shrinks: &[f64], overflow: f64) {
    let n = widths.len();
    let mut frozen = vec![false; n];
    let mut remaining = overflow;
    // At most `n` rounds: each round freezes at least one item or finishes.
    for _ in 0..n {
        if remaining <= 0.01 {
            break;
        }
        // Total scaled-shrink weight over the still-flexible items.
        let total: f64 = (0..n)
            .filter(|&i| !frozen[i] && shrinks[i] > 0.0)
            .map(|i| shrinks[i] * widths[i])
            .sum();
        if total <= 0.0 {
            break; // nothing left can shrink (all shrink:0 or already 0-wide)
        }
        let mut any_clamped = false;
        let mut absorbed = 0.0;
        for i in 0..n {
            if frozen[i] || shrinks[i] <= 0.0 {
                continue;
            }
            let want = remaining * (shrinks[i] * widths[i]) / total;
            if want >= widths[i] {
                // Item bottoms out at 0 and freezes.
                absorbed += widths[i];
                widths[i] = 0.0;
                frozen[i] = true;
                any_clamped = true;
            } else {
                widths[i] -= want;
                absorbed += want;
            }
        }
        remaining -= absorbed;
        if !any_clamped {
            break; // a clean pass fully distributed the overflow
        }
    }
}

/// Cumulative left/top edges for `widths` starting at `origin`, inserting `gap`
/// between adjacent tracks. `offsets[i]` is the start coordinate of track `i`.
fn cumulative_offsets(origin: f64, widths: &[f64], gap: f64) -> Vec<f64> {
    let mut offsets = Vec::with_capacity(widths.len());
    let mut cur = origin;
    for (i, w) in widths.iter().enumerate() {
        if i > 0 {
            cur += gap;
        }
        offsets.push(cur);
        cur += w;
    }
    offsets
}

/// Leading offset and inter-item gap realising a `justify-content` value, given
/// the `free` main-axis space left over by `n` explicitly-sized items.
fn justify_offsets(j: Justify, free: f64, n: usize) -> (f64, f64) {
    if free <= 0.0 || n == 0 {
        return (0.0, 0.0);
    }
    match j {
        Justify::Start => (0.0, 0.0),
        Justify::Center => (free / 2.0, 0.0),
        Justify::End => (free, 0.0),
        Justify::SpaceBetween => (0.0, if n > 1 { free / (n - 1) as f64 } else { 0.0 }),
        Justify::SpaceAround => (free / (2.0 * n as f64), free / n as f64),
        // Equal gaps everywhere — `n + 1` of them (one before each item, one
        // after the last), so leading == inter-item gap.
        Justify::SpaceEvenly => {
            let gap = free / (n + 1) as f64;
            (gap, gap)
        }
    }
}

/// Collapse runs of ASCII whitespace to single spaces (normal `white-space`).
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out
}

fn push_ancestor<'a>(ancestors: &[&'a Element], el: &'a Element) -> Vec<&'a Element> {
    let mut v = ancestors.to_vec();
    v.push(el);
    v
}

fn list_marker_ordered(ancestors: &[&Element]) -> bool {
    ancestors.last().map(|e| e.tag == "ol").unwrap_or(false)
}

/// Number of `<ol>`/`<ul>` containers among the ancestors (1 for a top-level
/// list, 2 for a list nested once, …). Drives the default bullet cycle.
fn list_nesting_depth(ancestors: &[&Element]) -> usize {
    ancestors
        .iter()
        .filter(|e| e.tag == "ol" || e.tag == "ul")
        .count()
}

/// The 1-based start index for the items of the list container that immediately
/// encloses these `ancestors`, honouring `<ol start="N">` (default 1). The
/// pre-increment counter therefore begins one below the returned value.
fn list_start_offset(ancestors: &[&Element]) -> usize {
    match ancestors.last() {
        Some(e) if e.tag == "ol" => e
            .attr("start")
            .and_then(|s| s.trim().parse::<usize>().ok())
            .map(|n| n.saturating_sub(1))
            .unwrap_or(0),
        _ => 0,
    }
}

/// The marker string for a list item, honouring `list-style-type`. An unset
/// type (`Disc`, the inherited default) inside an `<ol>` becomes `decimal`; an
/// unset type inside a `<ul>` cycles disc → circle → square with nesting depth.
/// `None` ⇒ no marker (`list-style-type: none`).
fn list_marker(
    style: &Style,
    ordered_ancestor: bool,
    index: usize,
    nesting_depth: usize,
) -> Option<String> {
    use super::css::ListStyle as L;
    // Only the inherited default reacts to its context; an explicit
    // `list-style-type` is always honoured verbatim.
    let kind = if style.list_style == L::Disc {
        if ordered_ancestor {
            L::Decimal
        } else {
            // depth 1 → disc, 2 → circle, ≥3 → square.
            match nesting_depth {
                0 | 1 => L::Disc,
                2 => L::Circle,
                _ => L::Square,
            }
        }
    } else {
        style.list_style
    };
    match kind {
        L::None => None,
        L::Disc => Some("•".to_string()),
        L::Circle => Some("◦".to_string()),
        L::Square => Some("▪".to_string()),
        L::Decimal => Some(format!("{index}.")),
        L::LowerAlpha => Some(format!("{}.", alpha_marker(index, false))),
        L::UpperAlpha => Some(format!("{}.", alpha_marker(index, true))),
        L::LowerRoman => Some(format!("{}.", roman_marker(index, false))),
        L::UpperRoman => Some(format!("{}.", roman_marker(index, true))),
    }
}

/// `1 → a`, `26 → z`, `27 → aa`, … (bijective base-26).
fn alpha_marker(mut n: usize, upper: bool) -> String {
    if n == 0 {
        return "0".to_string();
    }
    let base = if upper { b'A' } else { b'a' };
    let mut out = Vec::new();
    while n > 0 {
        n -= 1;
        out.push(base + (n % 26) as u8);
        n /= 26;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// `1 → i`, `4 → iv`, `9 → ix`, … (Roman numerals; falls back to the number
/// past 3999).
fn roman_marker(n: usize, upper: bool) -> String {
    if n == 0 || n > 3999 {
        return n.to_string();
    }
    const VALUES: [(usize, &str); 13] = [
        (1000, "m"),
        (900, "cm"),
        (500, "d"),
        (400, "cd"),
        (100, "c"),
        (90, "xc"),
        (50, "l"),
        (40, "xl"),
        (10, "x"),
        (9, "ix"),
        (5, "v"),
        (4, "iv"),
        (1, "i"),
    ];
    let mut n = n;
    let mut out = String::new();
    for (val, sym) in VALUES {
        while n >= val {
            out.push_str(sym);
            n -= val;
        }
    }
    if upper {
        out.to_uppercase()
    } else {
        out
    }
}

fn collect_rows(table: &Element) -> Vec<&Element> {
    let mut rows = Vec::new();
    fn walk<'a>(el: &'a Element, rows: &mut Vec<&'a Element>) {
        for c in &el.children {
            if let Node::Element(e) = c {
                if e.tag == "tr" {
                    rows.push(e);
                } else if matches!(e.tag.as_str(), "thead" | "tbody" | "tfoot") {
                    walk(e, rows);
                }
            }
        }
    }
    walk(table, &mut rows);
    rows
}

fn collect_cells(row: &Element) -> Vec<&Element> {
    row.children
        .iter()
        .filter_map(|c| match c {
            Node::Element(e) if e.tag == "td" || e.tag == "th" => Some(e),
            _ => None,
        })
        .collect()
}

/// `<col>` elements declared under the table's `<colgroup>` children (or a
/// `<colgroup>` that itself acts as a column via its `span`, when it has no
/// `<col>` children — per HTML semantics). Returns them in document order.
fn collect_cols(table: &Element) -> Vec<&Element> {
    let mut cols = Vec::new();
    for c in &table.children {
        if let Node::Element(group) = c {
            if group.tag != "colgroup" {
                continue;
            }
            let children: Vec<&Element> = group
                .children
                .iter()
                .filter_map(|n| match n {
                    Node::Element(e) if e.tag == "col" => Some(e),
                    _ => None,
                })
                .collect();
            if children.is_empty() {
                // A childless <colgroup> spans `span` columns itself.
                cols.push(group);
            } else {
                cols.extend(children);
            }
        }
    }
    cols
}

/// Number of physical columns a cell occupies: `colspan` (cells) or `span`
/// (`<col>`/`<colgroup>`), defaulting to 1. Zero/garbage clamps to 1.
fn cell_colspan(el: &Element) -> usize {
    el.attr("colspan")
        .or_else(|| el.attr("span"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(1)
}

/// Number of rows a cell occupies via `rowspan`, defaulting to 1. Zero/garbage
/// clamps to 1. (`rowspan="0"`, the "span to the end of the row group" form, is
/// rare in the Office-generated HTML we render and is treated as 1.)
fn cell_rowspan(el: &Element) -> usize {
    el.attr("rowspan")
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(1)
}

/// One `td`/`th` resolved onto the table grid by the standard occupation
/// algorithm: cells skip columns still covered by a `rowspan` anchored in an
/// earlier row, so the physical `<td>` order maps to the right grid columns.
struct GridCell<'a> {
    el: &'a Element,
    /// Anchor row index (into the `rows` slice — rows with no cells included).
    row: usize,
    /// First column the cell occupies.
    col: usize,
    /// Columns spanned (`colspan`, clamped ≥ 1).
    col_span: usize,
    /// Rows spanned (`rowspan`, clamped ≥ 1).
    row_span: usize,
}

/// Place a table's `td`/`th` cells onto a grid honouring both `colspan` and
/// `rowspan`. Returns the placed cells in document order plus the total column
/// count. A `rowspan` cell reserves its columns for the rows below it, so the
/// next row's physical cells shift past those reserved slots (rather than
/// colliding with the spanning cell). This is the canonical HTML table model:
/// a per-column "rows still occupied" counter, decremented once per processed
/// row.
fn build_grid<'a>(rows: &[&'a Element]) -> (Vec<GridCell<'a>>, usize) {
    let mut placed: Vec<GridCell<'a>> = Vec::new();
    // `occupied[c]` = number of *remaining* rows (counting the current one) that
    // column `c` is covered by a rowspan anchored at or above the current row.
    let mut occupied: Vec<usize> = Vec::new();
    let mut ncols = 0usize;
    for (r, row) in rows.iter().enumerate() {
        let mut c = 0usize;
        for cell in collect_cells(row) {
            // Skip leading columns still covered by a rowspan from a row above.
            while c < occupied.len() && occupied[c] > 0 {
                c += 1;
            }
            let col_span = cell_colspan(cell);
            let row_span = cell_rowspan(cell);
            let end = c + col_span;
            if end > occupied.len() {
                occupied.resize(end, 0);
            }
            ncols = ncols.max(end);
            // Reserve this cell's columns for `row_span` rows (the current row
            // plus `row_span - 1` below); the end-of-row decrement turns this
            // into exactly `row_span - 1` rows of downward coverage.
            for slot in occupied[c..end].iter_mut() {
                *slot = row_span;
            }
            placed.push(GridCell {
                el: cell,
                row: r,
                col: c,
                col_span,
                row_span,
            });
            c = end;
        }
        // Consume the current row from every active rowspan's remaining count.
        for slot in occupied.iter_mut() {
            *slot = slot.saturating_sub(1);
        }
    }
    (placed, ncols)
}

/// Declared width of a `<col>`: `style="width:.."` first, then a `width=".."`
/// attribute. Percentages resolve against `avail_w`; bare numbers and `px` are
/// pixels (1px = 0.75pt), `pt` is points — matching the CSS length convention.
fn col_declared_width(col: &Element, avail_w: f64) -> Option<f64> {
    if let Some(style) = col.attr("style") {
        // Scan the inline declarations for a `width:` (ignore `min/max-width`).
        for decl in style.split(';') {
            let mut kv = decl.splitn(2, ':');
            let key = kv.next().unwrap_or("").trim();
            if key.eq_ignore_ascii_case("width") {
                if let Some(val) = kv.next() {
                    if let Some(w) = parse_table_width(val.trim(), avail_w) {
                        return Some(w);
                    }
                }
            }
        }
    }
    col.attr("width")
        .and_then(|v| parse_table_width(v.trim(), avail_w))
}

/// Parse a column width to absolute points. `%` → fraction of `avail_w`; `pt`
/// stays; `px`/bare number → pixels (×0.75). Negatives and unparseable → None.
fn parse_table_width(v: &str, avail_w: f64) -> Option<f64> {
    let v = v.trim();
    if let Some(n) = v.strip_suffix('%') {
        return n
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|p| *p >= 0.0)
            .map(|p| avail_w * p / 100.0);
    }
    if let Some(n) = v.strip_suffix("pt") {
        return n.trim().parse::<f64>().ok().filter(|p| *p >= 0.0);
    }
    if let Some(n) = v.strip_suffix("px") {
        return n
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|p| *p >= 0.0)
            .map(|p| p * 0.75);
    }
    v.parse::<f64>()
        .ok()
        .filter(|p| *p >= 0.0)
        .map(|p| p * 0.75)
}

/// Slice the absolute-positioned fragments into pages, splitting rects that
/// straddle a page boundary so backgrounds/borders stay correct.
fn paginate(mut frags: Vec<Abs>, page_h: f64, top: f64, bottom: f64) -> Vec<Vec<Fragment>> {
    // Stacking order: by CSS `z-index` first (positioned subtrees lift their
    // whole range), then backgrounds (z=0) before content (z=1) within a level.
    // A stable sort keeps insertion (document) order for equal keys.
    frags.sort_by_key(|a| (a.zi, a.z));
    let content_h = (page_h - top - bottom).max(1.0);
    let mut pages: Vec<Vec<Fragment>> = Vec::new();

    let ensure = |pages: &mut Vec<Vec<Fragment>>, idx: usize| {
        while pages.len() <= idx {
            pages.push(Vec::new());
        }
    };
    let page_of = |y_abs: f64| ((y_abs - top).max(0.0) / content_h) as usize;
    let local_y = |y_abs: f64, page: usize| top + (y_abs - top) - page as f64 * content_h;

    for a in frags {
        match a.frag {
            Fragment::Text {
                x,
                y,
                w,
                style,
                text,
            } => {
                let p = page_of(y);
                ensure(&mut pages, p);
                pages[p].push(Fragment::Text {
                    x,
                    y: local_y(y, p),
                    w,
                    style,
                    text,
                });
            }
            Fragment::Image { x, y, w, h, src } => {
                let p = page_of(y);
                ensure(&mut pages, p);
                pages[p].push(Fragment::Image {
                    x,
                    y: local_y(y, p),
                    w,
                    h,
                    src,
                });
            }
            Fragment::Svg { x, y, w, h, image } => {
                let p = page_of(y);
                ensure(&mut pages, p);
                pages[p].push(Fragment::Svg {
                    x,
                    y: local_y(y, p),
                    w,
                    h,
                    image,
                });
            }
            Fragment::Rect {
                x,
                y,
                w,
                h,
                fill,
                stroke,
                stroke_w,
                opacity,
                radius,
                radius_v,
                shadow,
            } => {
                // Does this rect fit within a single page band? Only then can the
                // rounded corners / shadow be carried faithfully; a rect that
                // straddles a page boundary is sliced into plain rectangular bands
                // (drawing half-arcs per band would be wrong), matching how the
                // background/border already degrade across a page break.
                let single_band = page_of(y) == page_of(y + h - 0.001);
                let mut top = y;
                let bottom = y + h;
                while top < bottom {
                    let p = page_of(top);
                    let band_bottom = top + (p as f64 + 1.0) * content_h;
                    let seg_bottom = bottom.min(band_bottom);
                    ensure(&mut pages, p);
                    pages[p].push(Fragment::Rect {
                        x,
                        y: local_y(top, p),
                        w,
                        h: (seg_bottom - top).max(0.1),
                        fill,
                        stroke,
                        stroke_w,
                        opacity,
                        radius: if single_band { radius } else { [0.0; 4] },
                        radius_v: if single_band { radius_v } else { [0.0; 4] },
                        shadow: if single_band { shadow } else { None },
                    });
                    top = seg_bottom + 0.001;
                }
            }
            Fragment::Border {
                x,
                y,
                w,
                h,
                horizontal,
                width,
                color,
                style,
                opacity,
            } => {
                // Split the side's band across the page bands it spans, exactly
                // like `Rect`. The painter re-derives the dash run within each
                // segment, so a tall dashed/dotted side breaks cleanly at a page
                // boundary; a horizontal side never spans bands.
                let mut top = y;
                let bottom = y + h;
                while top < bottom {
                    let p = page_of(top);
                    let band_bottom = top + (p as f64 + 1.0) * content_h;
                    let seg_bottom = bottom.min(band_bottom);
                    ensure(&mut pages, p);
                    pages[p].push(Fragment::Border {
                        x,
                        y: local_y(top, p),
                        w,
                        h: (seg_bottom - top).max(0.1),
                        horizontal,
                        width,
                        color,
                        style,
                        opacity,
                    });
                    top = seg_bottom + 0.001;
                }
            }
            Fragment::Gradient {
                x,
                y,
                w,
                h,
                gradient,
                opacity,
            } => {
                // Placed whole on the page where its top sits (like images/SVG).
                // A gradient box that straddles a page boundary is uncommon and
                // splitting one faithfully needs the full-box extent on each band;
                // single-page placement is the pragmatic, documented behaviour.
                let p = page_of(y);
                ensure(&mut pages, p);
                pages[p].push(Fragment::Gradient {
                    x,
                    y: local_y(y, p),
                    w,
                    h,
                    gradient,
                    opacity,
                });
            }
            Fragment::Clipped { rect, inner } => {
                // Like Text/Image: the clipped content keeps to one page band
                // (assigned by its top); move the clip window and content into
                // that page's local coordinates together.
                let (_, fy0, _, _) = fragment_bbox(&inner);
                let p = page_of(fy0);
                ensure(&mut pages, p);
                let mut moved = Fragment::Clipped { rect, inner };
                shift_fragment(&mut moved, 0.0, local_y(fy0, p) - fy0);
                pages[p].push(moved);
            }
        }
    }
    if pages.is_empty() {
        pages.push(Vec::new());
    }
    pages
}

/// Clamp `border-radius` corners `[tl, tr, br, bl]` so no corner exceeds half the
/// box and adjacent radii on a side never overlap (CSS §"Corner overlap": if the
/// radii on any side sum to more than that side's length, all radii scale down by
/// the same factor). Negative radii are floored at `0`. A zero box yields zeros,
/// so this is a no-op for the common square-corner case.
fn clamp_radius(r: [f64; 4], w: f64, h: f64) -> [f64; 4] {
    let mut r = [r[0].max(0.0), r[1].max(0.0), r[2].max(0.0), r[3].max(0.0)];
    if w <= 0.0 || h <= 0.0 {
        return [0.0; 4];
    }
    // Per-side sums: top = tl+tr, right = tr+br, bottom = br+bl, left = bl+tl.
    let mut f = 1.0_f64;
    let pairs = [
        (r[0] + r[1], w), // top
        (r[1] + r[2], h), // right
        (r[2] + r[3], w), // bottom
        (r[3] + r[0], h), // left
    ];
    for (sum, len) in pairs {
        if sum > 0.0 && len > 0.0 {
            f = f.min(len / sum);
        }
    }
    if f < 1.0 {
        for v in &mut r {
            *v *= f;
        }
    }
    r
}

/// A rough fallback metric (used for tests and when no embedded font matches):
/// per-glyph advance estimated from the font class. The paint layer overrides
/// this with real TrueType advance widths.
#[derive(Debug)]
pub struct AverageMeasure;

impl Measure for AverageMeasure {
    fn width(&self, text: &str, style: &Style) -> f64 {
        let per_em = if style.generic_mono { 0.6 } else { 0.5 };
        let bold_factor = if style.bold { 1.03 } else { 1.0 };
        text.chars().count() as f64 * style.font_size * per_em * bold_factor
    }
}

#[cfg(test)]
mod tests {
    use super::super::css::{collect_style_css, Stylesheet};
    use super::super::dom::parse;
    use super::*;

    fn run(html: &str) -> Layout {
        let nodes = parse(html);
        let sheet = Stylesheet::new(&collect_style_css(&nodes));
        layout_document(&nodes, &sheet, &AverageMeasure, 612.0, 792.0, 36.0)
    }

    #[test]
    fn wraps_long_text_into_multiple_lines() {
        let html = format!("<p>{}</p>", "word ".repeat(200));
        let layout = run(&html);
        let texts = layout
            .pages
            .iter()
            .flatten()
            .filter(|f| matches!(f, Fragment::Text { .. }))
            .count();
        assert!(texts > 50, "long paragraph wraps into many runs ({texts})");
    }

    #[test]
    fn paginates_tall_content() {
        let html = format!("<div>{}</div>", "<p>line</p>".repeat(120));
        let layout = run(&html);
        assert!(
            layout.pages.len() > 1,
            "tall content spans pages ({})",
            layout.pages.len()
        );
    }

    #[test]
    fn emits_background_rect_behind_text() {
        let layout = run(r#"<div style="background:#eee;padding:10pt">hello</div>"#);
        let page = &layout.pages[0];
        let rect_idx = page.iter().position(|f| matches!(f, Fragment::Rect { .. }));
        let text_idx = page.iter().position(|f| matches!(f, Fragment::Text { .. }));
        assert!(rect_idx.is_some() && text_idx.is_some());
        assert!(rect_idx < text_idx, "background paints before text");
    }

    #[test]
    fn css_page_break_before_starts_new_page() {
        let layout = run("<p>first</p><p style=\"page-break-before: always\">second</p>");
        assert!(
            layout.pages.len() >= 2,
            "page-break-before forces a new page ({})",
            layout.pages.len()
        );
        let on_p2 = layout.pages[1]
            .iter()
            .any(|f| matches!(f, Fragment::Text { text, .. } if text.contains("second")));
        assert!(on_p2, "second paragraph is on page 2");
    }

    #[test]
    fn pagebreak_tag_starts_new_page() {
        let layout = run("<p>a</p><pagebreak></pagebreak><p>b</p>");
        assert!(
            layout.pages.len() >= 2,
            "<pagebreak> forces a new page ({})",
            layout.pages.len()
        );
        let b_on_p2 = layout.pages[1]
            .iter()
            .any(|f| matches!(f, Fragment::Text { text, .. } if text == "b"));
        assert!(b_on_p2, "content after <pagebreak> is on page 2");
    }

    #[test]
    fn table_lays_cells_side_by_side() {
        let layout = run("<table><tr><td>A</td><td>B</td></tr></table>");
        let texts: Vec<_> = layout
            .pages
            .iter()
            .flatten()
            .filter_map(|f| match f {
                Fragment::Text { x, text, .. } => Some((*x, text.clone())),
                _ => None,
            })
            .collect();
        let a = texts.iter().find(|(_, t)| t == "A").unwrap().0;
        let b = texts.iter().find(|(_, t)| t == "B").unwrap().0;
        assert!(b > a, "second cell is to the right of the first");
    }

    // x of a cell's text fragment.
    fn cell_x(layout: &Layout, label: &str) -> f64 {
        layout
            .pages
            .iter()
            .flatten()
            .find_map(|f| match f {
                Fragment::Text { x, text, .. } if text == label => Some(*x),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no text fragment {label:?}"))
    }

    // Page 612pt, margins 36pt ⇒ avail_w = 540. Default `td` padding 2pt +
    // border 1pt ⇒ cell text sits 3pt inside its column, so a cell starting at
    // column x renders its text at 36 + x + 3.
    const CELL_INSET: f64 = 36.0 + 3.0;

    #[test]
    fn table_honours_colgroup_widths() {
        // Declared 400/100pt (sum 500) is scaled to fill avail_w=540 (fixed
        // layout): scale 1.08 ⇒ col[0] = 432, so cell B sits at 39 + 432 = 471,
        // far past the equal-split midpoint (39 + 270 = 309).
        let layout = run(
            "<table><colgroup><col style=\"width:400pt\"><col style=\"width:100pt\"></colgroup>\
             <tr><td>A</td><td>B</td></tr></table>",
        );
        let a = cell_x(&layout, "A");
        let b = cell_x(&layout, "B");
        assert!((a - CELL_INSET).abs() < 1.0, "first cell at left ({a})");
        assert!(
            (b - (CELL_INSET + 432.0)).abs() < 1.0,
            "cell B starts at scaled col[0] width (~471), not avail_w/2 ({b})"
        );
    }

    #[test]
    fn table_honours_percent_col_widths() {
        // 75% / 25% of 540 ⇒ col[0] = 405; cell B at 39 + 405 = 444.
        let layout = run(
            "<table><colgroup><col style=\"width:75%\"><col style=\"width:25%\"></colgroup>\
             <tr><td>A</td><td>B</td></tr></table>",
        );
        let b = cell_x(&layout, "B");
        assert!(
            (b - (CELL_INSET + 405.0)).abs() < 1.0,
            "cell B near 39 + 75%×540 = 444 ({b})"
        );
    }

    #[test]
    fn table_without_widths_keeps_equal_columns() {
        // No declared widths ⇒ equal columns (270 each): cell B at 39 + 270.
        let layout = run("<table><tr><td>A</td><td>B</td></tr></table>");
        let b = cell_x(&layout, "B");
        assert!(
            (b - (CELL_INSET + 270.0)).abs() < 1.0,
            "equal columns put B at ~309 ({b})"
        );
    }

    #[test]
    fn table_colspan_sums_column_widths() {
        // Equal 3-col grid (180 each). A colspan=2 cell covers cols 0–1 (360),
        // so "Tail" starts at column 2 ⇒ 39 + 360 = 399. Row 2 fixes the grid.
        let layout = run("<table>\
             <tr><td colspan=\"2\">Wide</td><td>Tail</td></tr>\
             <tr><td>a</td><td>b</td><td>c</td></tr></table>");
        let wide = cell_x(&layout, "Wide");
        let tail = cell_x(&layout, "Tail");
        let c = cell_x(&layout, "c");
        assert!(
            (wide - CELL_INSET).abs() < 1.0,
            "spanning cell at left ({wide})"
        );
        assert!(
            (tail - (CELL_INSET + 360.0)).abs() < 1.0,
            "Tail after 2 columns (~399), proving colspan summed ({tail})"
        );
        // Third column of row 2 aligns under "Tail" (same start column index 2).
        assert!(
            (c - tail).abs() < 1.0,
            "col 2 aligns across rows ({c} vs {tail})"
        );
    }

    // y of a cell's (first) text fragment.
    fn cell_y(layout: &Layout, label: &str) -> f64 {
        layout
            .pages
            .iter()
            .flatten()
            .find_map(|f| match f {
                Fragment::Text { y, text, .. } if text == label => Some(*y),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no text fragment {label:?}"))
    }

    #[test]
    fn table_rowspan_skips_occupied_slot_in_next_row() {
        // 2-col equal grid (270 each). The left cell of row 0 spans both rows
        // (rowspan=2), so it reserves column 0 for row 1 — the single physical
        // cell of row 1 must therefore land in *column 1*, not collide with the
        // spanning cell in column 0.
        //
        //   row0: | Span (rowspan 2) | B0 |
        //   row1: |     (reserved)   | C1 |
        let layout = run("<table>\
             <tr><td rowspan=\"2\">Span</td><td>B0</td></tr>\
             <tr><td>C1</td></tr></table>");
        let span = cell_x(&layout, "Span");
        let b0 = cell_x(&layout, "B0");
        let c1 = cell_x(&layout, "C1");
        // Spanning cell anchors column 0 (left edge).
        assert!(
            (span - CELL_INSET).abs() < 1.0,
            "rowspan cell at col 0 ({span})"
        );
        // B0 sits in column 1.
        assert!(
            (b0 - (CELL_INSET + 270.0)).abs() < 1.0,
            "B0 in column 1 (~309) ({b0})"
        );
        // C1 is pushed into column 1 (under B0), NOT column 0 where it would
        // overlap the spanning cell — this is the whole point of rowspan
        // occupation.
        assert!(
            (c1 - b0).abs() < 1.0,
            "row-1 cell skips the reserved col 0 and aligns under B0 ({c1} vs {b0})"
        );
        assert!(
            (c1 - span).abs() > 100.0,
            "row-1 cell does NOT land on the spanning cell's column ({c1} vs {span})"
        );
    }

    #[test]
    fn table_rowspan_under_counts_without_occupation() {
        // A rowspan in row 0 makes row 1 hold *more* physical cells than row 0.
        // Counting columns by a naive per-row colspan sum (max(1, 2) = 2) would
        // under-report; the real grid is 3 wide:
        //   row0: | A (rowspan 2) | B0 |        (B0 → col 1; nothing in col 2)
        //   row1: |  (reserved)   | C1 | D1 |   (C1 → col 1, D1 → col 2)
        // So D1 must sit in column 2 (the third of three equal 180-pt columns).
        let layout = run("<table>\
             <tr><td rowspan=\"2\">A</td><td>B0</td></tr>\
             <tr><td>C1</td><td>D1</td></tr></table>");
        let b0 = cell_x(&layout, "B0");
        let c1 = cell_x(&layout, "C1");
        let d1 = cell_x(&layout, "D1");
        // 3 equal columns of 180. col1 = 180, col2 = 360.
        assert!(
            (b0 - (CELL_INSET + 180.0)).abs() < 1.5,
            "B0 in column 1 of a 3-col grid (~219) ({b0})"
        );
        assert!(
            (c1 - (CELL_INSET + 180.0)).abs() < 1.5,
            "C1 aligns under B0 in column 1 ({c1})"
        );
        assert!(
            (d1 - (CELL_INSET + 360.0)).abs() < 1.5,
            "D1 in column 2 (~399), proving the grid is 3 wide ({d1})"
        );
    }

    #[test]
    fn table_rowspan_cell_covers_both_rows_vertically() {
        // The spanning cell's background rect must cover the full height of the
        // two rows it spans — i.e. be taller than either single row alone, and
        // start at the table top. A grey background makes the rect findable.
        let layout = run("<table>\
             <tr><td rowspan=\"2\" style=\"background:#cccccc\">S</td><td>B0</td></tr>\
             <tr><td>C1</td></tr></table>");
        let grey = [0.8, 0.8, 0.8];
        // The spanning cell's background fill rect.
        let span_rect = rects(&layout)
            .into_iter()
            .find(|(_, _, _, _, fill)| *fill == Some(grey))
            .expect("a grey background rect for the spanning cell");
        let (_sx, sy, _sw, sh, _) = span_rect;
        // Heights of the two simple cells in column 1 give a single-row scale.
        let b0_y = cell_y(&layout, "B0");
        let c1_y = cell_y(&layout, "C1");
        let one_row = c1_y - b0_y; // top-to-top distance ≈ row-0 height
        assert!(
            one_row > 1.0,
            "the two rows are vertically separated ({one_row})"
        );
        // The spanning rect must be taller than a single row (it covers two).
        assert!(
            sh > one_row + 1.0,
            "spanning cell rect ({sh}) is taller than one row ({one_row})"
        );
        // And it starts at (or above) row 0's content baseline area.
        assert!(
            sy <= b0_y,
            "spanning rect starts at/above row 0 ({sy} vs {b0_y})"
        );
    }

    #[test]
    fn table_tall_rowspan_stretches_the_rows_it_spans() {
        // A rowspan=2 cell whose content is much taller than the simple peers in
        // the rows it spans must push the row *below* its anchor downward (the
        // rows grow to fit). We compare the y of a cell in row 2 (outside the
        // span) with and without a tall rowspan in rows 0–1.
        let tall = "line ".repeat(40);
        let with_tall = run(&format!(
            "<table>\
             <tr><td rowspan=\"2\">{tall}</td><td>B0</td></tr>\
             <tr><td>C1</td></tr>\
             <tr><td>R2L</td><td>R2R</td></tr></table>"
        ));
        let short = run("<table>\
             <tr><td rowspan=\"2\">x</td><td>B0</td></tr>\
             <tr><td>C1</td></tr>\
             <tr><td>R2L</td><td>R2R</td></tr></table>");
        let y_tall = cell_y(&with_tall, "R2L");
        let y_short = cell_y(&short, "R2L");
        assert!(
            y_tall > y_short + 20.0,
            "a tall rowspan grows the spanned rows, pushing row 2 down \
             (tall {y_tall} vs short {y_short})"
        );
    }

    // All filled rects on page 0 as (x, y, w, h, fill, opacity).
    #[allow(clippy::type_complexity)]
    fn rects(layout: &Layout) -> Vec<(f64, f64, f64, f64, Option<[f64; 3]>)> {
        layout
            .pages
            .iter()
            .flatten()
            .filter_map(|f| match f {
                Fragment::Rect {
                    x, y, w, h, fill, ..
                } => Some((*x, *y, *w, *h, *fill)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn table_cell_border_bottom_only_draws_a_bottom_rule() {
        // A red 3pt bottom border with no other sides: expect a thin filled rect
        // hugging the cell's bottom (h≈3pt, full cell width, red), and NO 3pt
        // rule at the cell top — the old uniform-stroke path could not do this.
        let layout = run(
            r#"<table><tr><td style="border:none;border-bottom:3pt solid #ff0000">Cell</td></tr></table>"#,
        );
        let red = [1.0, 0.0, 0.0];
        let bottoms: Vec<_> = rects(&layout)
            .into_iter()
            .filter(|(_, _, _, h, fill)| *fill == Some(red) && (*h - 3.0).abs() < 0.01)
            .collect();
        assert_eq!(
            bottoms.len(),
            1,
            "exactly one 3pt-tall red rule (the bottom border): {bottoms:?}"
        );
        let (_rx, ry, rw, _rh, _) = bottoms[0];
        // It must sit at the bottom of the cell, not the top (cell starts at the
        // content-area top = 36pt).
        assert!(ry > 36.0 + 5.0, "bottom rule is below the cell top ({ry})");
        assert!(rw > 100.0, "bottom rule spans the (full-width) cell ({rw})");
        // No red rect taller than a hairline anywhere else (no spurious side).
        let red_count = rects(&layout)
            .into_iter()
            .filter(|(_, _, _, _, fill)| *fill == Some(red))
            .count();
        assert_eq!(red_count, 1, "only the bottom side is red ({red_count})");
    }

    #[test]
    fn table_header_cell_background_paints_behind_text() {
        // A grey header background must render as a fill rect at z=0 *before* the
        // header text (so text stays legible on top).
        let layout = run(r#"<table><tr><th style="background:#cccccc">Head</th></tr></table>"#);
        let page = &layout.pages[0];
        let grey = [0.8, 0.8, 0.8];
        let bg_idx = page
            .iter()
            .position(|f| matches!(f, Fragment::Rect { fill: Some(c), .. } if *c == grey));
        let text_idx = page
            .iter()
            .position(|f| matches!(f, Fragment::Text { text, .. } if text == "Head"));
        let bg = bg_idx.expect("a grey header-background rect");
        let tx = text_idx.expect("the header text");
        assert!(bg < tx, "header background paints before its text");
    }

    #[test]
    fn table_per_side_border_colors_are_distinct() {
        // border-left blue, border-bottom green → two differently-coloured rules.
        let layout = run(
            r#"<table><tr><td style="border:none;border-left:2pt solid #0000ff;border-bottom:2pt solid #00ff00">X</td></tr></table>"#,
        );
        let has = |c: [f64; 3]| {
            rects(&layout)
                .into_iter()
                .any(|(_, _, _, _, fill)| fill == Some(c))
        };
        assert!(has([0.0, 0.0, 1.0]), "blue left border present");
        assert!(has([0.0, 1.0, 0.0]), "green bottom border present");
    }

    #[test]
    fn table_vertical_align_middle_lowers_short_cell_text() {
        // Two cells in one row: the left cell wraps into several lines (tall),
        // the right cell holds one line with vertical-align:middle. The short
        // cell's text must sit *below* where top-alignment would place it.
        let long = "word ".repeat(60);
        let html = format!(
            r#"<table><tr>
                 <td style="width:80%">{long}</td>
                 <td style="width:20%;vertical-align:middle">Mid</td>
               </tr></table>"#,
        );
        let mid_top = run(&html_top_variant(&long))
            .pages
            .iter()
            .flatten()
            .find_map(|f| match f {
                Fragment::Text { y, text, .. } if text == "Mid" => Some(*y),
                _ => None,
            })
            .expect("Mid (top-aligned)");
        let mid_mid = run(&html)
            .pages
            .iter()
            .flatten()
            .find_map(|f| match f {
                Fragment::Text { y, text, .. } if text == "Mid" => Some(*y),
                _ => None,
            })
            .expect("Mid (middle-aligned)");
        assert!(
            mid_mid > mid_top + 5.0,
            "middle-aligned text is lower than top-aligned ({mid_mid} vs {mid_top})"
        );
    }

    // Same table but the short cell is top-aligned (baseline for the assertion).
    fn html_top_variant(long: &str) -> String {
        format!(
            r#"<table><tr>
                 <td style="width:80%">{long}</td>
                 <td style="width:20%;vertical-align:top">Mid</td>
               </tr></table>"#,
        )
    }

    #[test]
    fn collapsed_table_does_not_double_interior_rules() {
        // 2×2 collapsed grid. At the interior vertical grid line each row
        // contributes exactly ONE rule segment (the right cell's left edge); the
        // left cell does NOT also stroke its right edge there. So the boundary
        // carries n_rows segments (2), not 2×n_rows (4) — that's the collapse
        // dedup. Separate mode (next test) would draw both adjacent sides.
        let layout = run("<table style=\"border-collapse:collapse\">\
               <tr><td>a</td><td>b</td></tr>\
               <tr><td>c</td><td>d</td></tr></table>");
        // Equal 2-col grid over avail 540 ⇒ boundary at 36 + 270 = 306.
        let boundary = 36.0 + 270.0;
        let verticals: Vec<_> = rects(&layout)
            .into_iter()
            // thin vertical rules straddling the interior boundary
            .filter(|(rx, _, rw, rh, fill)| {
                fill.is_some() && *rw < 4.0 && *rh > 4.0 && (*rx - boundary).abs() < 2.5
            })
            .collect();
        assert_eq!(
            verticals.len(),
            2,
            "interior boundary drawn once per row (2), not doubled (4): {verticals:?}"
        );
    }

    #[test]
    fn separated_table_draws_all_four_cell_sides() {
        // With border-collapse:separate a single cell draws top/right/bottom/left
        // → at least 4 border rects (plus none shared away).
        let layout = run(
            r#"<table style="border-collapse:separate"><tr><td style="border:1pt solid #000000">x</td></tr></table>"#,
        );
        let black = [0.0, 0.0, 0.0];
        let n = rects(&layout)
            .into_iter()
            .filter(|(_, _, _, _, fill)| *fill == Some(black))
            .count();
        assert!(n >= 4, "separate mode draws all four cell sides ({n})");
    }

    #[test]
    fn flex_lays_children_in_a_row() {
        let layout = run(r#"<div style="display:flex"><div>Left</div><div>Right</div></div>"#);
        let texts: Vec<_> = layout
            .pages
            .iter()
            .flatten()
            .filter_map(|f| match f {
                Fragment::Text { x, text, .. } => Some((*x, text.clone())),
                _ => None,
            })
            .collect();
        let l = texts.iter().find(|(_, t)| t == "Left").unwrap().0;
        let r = texts.iter().find(|(_, t)| t == "Right").unwrap().0;
        assert!(
            r > l,
            "flex item 'Right' is to the right of 'Left' (l={l}, r={r})"
        );
    }

    fn text_xy(layout: &Layout) -> Vec<(f64, f64, String)> {
        layout
            .pages
            .iter()
            .flatten()
            .filter_map(|f| match f {
                Fragment::Text { x, y, text, .. } => Some((*x, *y, text.clone())),
                _ => None,
            })
            .collect()
    }

    fn text_runs(layout: &Layout) -> Vec<String> {
        layout
            .pages
            .iter()
            .flatten()
            .filter_map(|f| match f {
                Fragment::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn list_style_type_markers() {
        let roman = text_runs(&run(
            r#"<ol style="list-style-type: lower-roman"><li>a</li><li>b</li><li>c</li></ol>"#,
        ));
        for m in ["i.", "ii.", "iii."] {
            assert!(roman.iter().any(|s| s == m), "roman marker {m}: {roman:?}");
        }
        let alpha = text_runs(&run(
            r#"<ul style="list-style-type: upper-alpha"><li>x</li><li>y</li></ul>"#,
        ));
        assert!(alpha.iter().any(|s| s == "A.") && alpha.iter().any(|s| s == "B."));
        let none = text_runs(&run(r#"<ul style="list-style-type: none"><li>z</li></ul>"#));
        assert!(!none.iter().any(|s| s == "•"), "no marker: {none:?}");
        // Bare <ol> defaults to decimal, bare <ul> to a disc bullet.
        let dec = text_runs(&run("<ol><li>a</li><li>b</li></ol>"));
        assert!(dec.iter().any(|s| s == "1.") && dec.iter().any(|s| s == "2."));
        assert!(text_runs(&run("<ul><li>a</li></ul>"))
            .iter()
            .any(|s| s == "•"));
    }

    #[test]
    fn ordered_list_markers_are_left_of_their_item() {
        // `<ol><li>A<li>B</ol>` → markers "1." / "2.", each to the left of and
        // vertically aligned with its item's text.
        let xy = text_xy(&run("<ol><li>Alpha</li><li>Beta</li></ol>"));
        let find = |t: &str| xy.iter().find(|(_, _, s)| s == t).map(|(x, y, _)| (*x, *y));
        let (m1x, m1y) = find("1.").expect("marker 1.");
        let (m2x, m2y) = find("2.").expect("marker 2.");
        let (a_x, a_y) = find("Alpha").expect("item Alpha");
        let (b_x, b_y) = find("Beta").expect("item Beta");
        // Marker sits in the left gutter (before the content) on the item's line.
        assert!(m1x < a_x, "marker 1. left of item (m={m1x}, item={a_x})");
        assert!(m2x < b_x, "marker 2. left of item (m={m2x}, item={b_x})");
        assert!((m1y - a_y).abs() < 1.0, "marker 1. aligned with item line");
        assert!((m2y - b_y).abs() < 1.0, "marker 2. aligned with item line");
        // Both markers share the same left edge; the second is below the first.
        assert!((m1x - m2x).abs() < 0.5, "markers share a left edge");
        assert!(m2y > m1y, "second item below the first");
    }

    #[test]
    fn nested_ul_inside_ol_indents_its_bullet() {
        // The outer ordered item gets "1."; the inner unordered item gets a
        // bullet, indented to the right of the outer marker. The inner list is
        // at nesting depth 2 (<ol> then <ul>), so its default glyph is a circle.
        let xy = text_xy(&run("<ol><li>Outer<ul><li>Inner</li></ul></li></ol>"));
        let find = |t: &str| xy.iter().find(|(_, _, s)| s == t).map(|(x, _, _)| *x);
        let one_x = find("1.").expect("ordered marker 1.");
        let bul_x = find("◦").expect("nested bullet (circle at depth 2)");
        let inner_x = find("Inner").expect("inner item text");
        // Nested bullet is indented past the outer "1." marker, and still sits
        // left of its own item's text.
        assert!(
            bul_x > one_x,
            "nested bullet indented (bul={bul_x}, top={one_x})"
        );
        assert!(bul_x < inner_x, "bullet left of inner text");
    }

    #[test]
    fn lower_alpha_markers() {
        let alpha = text_runs(&run(
            r#"<ol style="list-style-type: lower-alpha"><li>a</li><li>b</li></ol>"#,
        ));
        assert!(
            alpha.iter().any(|s| s == "a.") && alpha.iter().any(|s| s == "b."),
            "lower-alpha a./b.: {alpha:?}"
        );
    }

    #[test]
    fn ordered_list_start_attribute() {
        // `<ol start="5">` counts 5, 6, 7…
        let runs = text_runs(&run(r#"<ol start="5"><li>a</li><li>b</li><li>c</li></ol>"#));
        for m in ["5.", "6.", "7."] {
            assert!(runs.iter().any(|s| s == m), "start=5 marker {m}: {runs:?}");
        }
        assert!(!runs.iter().any(|s| s == "1."), "no 1. when start=5");
    }

    #[test]
    fn nested_unordered_bullets_cycle_by_depth() {
        // Bare nested <ul>s cycle disc → circle → square with depth.
        let runs = text_runs(&run(
            "<ul><li>a<ul><li>b<ul><li>c</li></ul></li></ul></li></ul>",
        ));
        assert!(runs.iter().any(|s| s == "•"), "depth 1 disc: {runs:?}");
        assert!(runs.iter().any(|s| s == "◦"), "depth 2 circle: {runs:?}");
        assert!(runs.iter().any(|s| s == "▪"), "depth 3 square: {runs:?}");
    }

    #[test]
    fn width_clamps_min_and_max() {
        // width 500 but max-width 100 → the box (background rect) is clamped.
        let layout = run(r#"<div style="width:500pt;max-width:100pt;background:#eee">x</div>"#);
        let w = layout
            .pages
            .iter()
            .flatten()
            .find_map(|f| match f {
                Fragment::Rect { w, .. } => Some(*w),
                _ => None,
            })
            .expect("a background rect");
        assert!(w <= 101.0, "max-width clamps box width: {w}");
    }

    #[test]
    fn text_decoration_flags_on_runs() {
        let layout = run(r#"<p style="text-decoration: line-through overline">struck</p>"#);
        let st = layout.pages.iter().flatten().find_map(|f| match f {
            Fragment::Text { style, text, .. } if text.contains("struck") => Some(style.clone()),
            _ => None,
        });
        let st = st.expect("the text run");
        assert!(st.strike && st.overline, "line-through + overline flagged");
    }

    /// Find the (font_size, top-down y) of the first text run whose text equals
    /// `needle`, across all pages.
    fn run_metrics(layout: &Layout, needle: &str) -> Option<(f64, f64)> {
        layout.pages.iter().flatten().find_map(|f| match f {
            Fragment::Text { style, text, y, .. } if text == needle => Some((style.font_size, *y)),
            _ => None,
        })
    }

    #[test]
    fn sup_run_is_smaller_and_raised() {
        // `x<sup>2</sup>`: the superscript "2" must be a smaller font AND sit
        // higher on the page (smaller top-down y) than the base "x".
        let layout = run("<p>x<sup>2</sup></p>");
        let (base_sz, base_y) = run_metrics(&layout, "x").expect("base run");
        let (sup_sz, sup_y) = run_metrics(&layout, "2").expect("superscript run");
        assert!(
            sup_sz < base_sz,
            "superscript glyph is smaller ({sup_sz} < {base_sz})"
        );
        assert!(
            sup_y < base_y - 1.0,
            "superscript baseline is raised (top-down y {sup_y} < {base_y})"
        );
    }

    #[test]
    fn sub_run_is_smaller_and_lowered() {
        // `H<sub>2</sub>O`: the subscript "2" must be smaller AND sit lower on
        // the page (larger top-down y) than the base "H".
        let layout = run("<p>H<sub>2</sub>O</p>");
        let (base_sz, base_y) = run_metrics(&layout, "H").expect("base run");
        let (sub_sz, sub_y) = run_metrics(&layout, "2").expect("subscript run");
        assert!(
            sub_sz < base_sz,
            "subscript glyph is smaller ({sub_sz} < {base_sz})"
        );
        assert!(
            sub_y > base_y + 1.0,
            "subscript baseline is lowered (top-down y {sub_y} > {base_y})"
        );
    }

    #[test]
    fn explicit_vertical_align_length_raises_the_run() {
        // `vertical-align: 5px` (positive = up in CSS) raises the run; the
        // shifted run's top-down y is above its un-shifted sibling.
        let layout = run(r#"<p><span>base</span><span style="vertical-align:5px">up</span></p>"#);
        let (_, base_y) = run_metrics(&layout, "base").expect("base run");
        let (_, up_y) = run_metrics(&layout, "up").expect("raised run");
        assert!(
            up_y < base_y,
            "explicit length raised the run ({up_y} < {base_y})"
        );
    }

    #[test]
    fn inline_svg_becomes_a_vector_fragment() {
        let layout = run(
            r#"<p>logo <svg width="20" height="20" viewBox="0 0 10 10"><rect width="10" height="10"/></svg> here</p>"#,
        );
        let svg = layout.pages.iter().flatten().find_map(|f| match f {
            Fragment::Svg { w, h, .. } => Some((*w, *h)),
            _ => None,
        });
        assert_eq!(
            svg,
            Some((20.0, 20.0)),
            "inline <svg> → a 20×20 vector fragment"
        );
        // Surrounding text still flows as text runs.
        assert!(text_runs(&layout).iter().any(|t| t == "logo"));
    }

    #[test]
    fn text_transform_cases_rendered_text() {
        let texts = |layout: &Layout| -> String {
            layout
                .pages
                .iter()
                .flatten()
                .filter_map(|f| match f {
                    Fragment::Text { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert!(texts(&run(
            r#"<p style="text-transform: uppercase">hello world</p>"#
        ))
        .contains("HELLO"));
        assert!(texts(&run(r#"<p style="text-transform: lowercase">HELLO</p>"#)).contains("hello"));
        // `capitalize` upper-cases each word's first letter; it is inherited.
        let cap = texts(&run(
            r#"<div style="text-transform: capitalize"><span>the quick</span></div>"#,
        ));
        assert!(cap.contains("The") && cap.contains("Quick"), "got: {cap}");
    }

    #[test]
    fn text_indent_shifts_first_line_only() {
        // First line is pushed right by 40pt; wrapped lines start at the margin.
        let html = format!(r#"<p style="text-indent:40pt">{}</p>"#, "word ".repeat(80));
        let xs: Vec<f64> = run(&html)
            .pages
            .iter()
            .flatten()
            .filter_map(|f| match f {
                Fragment::Text { x, .. } => Some(*x),
                _ => None,
            })
            .collect();
        assert!(xs.len() > 2, "paragraph wrapped into several lines");
        let first_x = xs[0];
        let min_x = xs.iter().copied().fold(f64::INFINITY, f64::min);
        assert!(
            first_x > min_x + 30.0,
            "first line indented past later lines (first={first_x}, min={min_x})"
        );
    }

    #[test]
    fn flex_column_stacks_items_vertically() {
        let layout = run(
            r#"<div style="display:flex;flex-direction:column"><div>Top</div><div>Bot</div></div>"#,
        );
        let t = text_xy(&layout);
        let top = t.iter().find(|(_, _, s)| s == "Top").unwrap();
        let bot = t.iter().find(|(_, _, s)| s == "Bot").unwrap();
        assert!(
            bot.1 > top.1 && (bot.0 - top.0).abs() < 1.0,
            "column flex stacks 'Bot' below 'Top' at the same x (top={top:?}, bot={bot:?})"
        );
    }

    #[test]
    fn flex_grow_widens_the_growing_item() {
        // Item A grows (weight 4), item B does not (weight 1): A's column is wider,
        // so B starts much further right than the equal-split midpoint.
        let grow =
            run(r#"<div style="display:flex"><div style="flex:3">A</div><div>B</div></div>"#);
        let equal = run(r#"<div style="display:flex"><div>A</div><div>B</div></div>"#);
        let bx = |l: &Layout| text_xy(l).into_iter().find(|(_, _, s)| s == "B").unwrap().0;
        assert!(
            bx(&grow) > bx(&equal) + 50.0,
            "flex-grow pushes 'B' right vs equal split (grow={}, equal={})",
            bx(&grow),
            bx(&equal)
        );
    }

    #[test]
    fn grid_wraps_items_into_rows() {
        // 2 columns, 4 cells → 2 rows. Cell 3 sits below cell 1 at the same x.
        let layout = run(
            r#"<div style="display:grid;grid-template-columns:1fr 1fr"><div>C1</div><div>C2</div><div>C3</div><div>C4</div></div>"#,
        );
        let t = text_xy(&layout);
        let c1 = t.iter().find(|(_, _, s)| s == "C1").unwrap();
        let c2 = t.iter().find(|(_, _, s)| s == "C2").unwrap();
        let c3 = t.iter().find(|(_, _, s)| s == "C3").unwrap();
        assert!(c2.0 > c1.0, "C2 is right of C1 (same row)");
        assert!(
            c3.1 > c1.1 && (c3.0 - c1.0).abs() < 1.0,
            "C3 wraps below C1 in the next row (c1={c1:?}, c3={c3:?})"
        );
    }

    #[test]
    fn grid_column_start_places_item_explicitly() {
        // 2 columns. The FIRST item carries `grid-column: 2`, so it lands in the
        // right column; the auto-flowing second item fills the (free) left cell of
        // the same row. Hence "A" ends up to the RIGHT of "B" despite source order.
        let layout = run(
            r#"<div style="display:grid;grid-template-columns:1fr 1fr">
                 <div style="grid-column:2">A</div><div>B</div>
               </div>"#,
        );
        let t = text_xy(&layout);
        let a = t.iter().find(|(_, _, s)| s == "A").unwrap();
        let b = t.iter().find(|(_, _, s)| s == "B").unwrap();
        assert!(
            a.0 > b.0,
            "explicit grid-column:2 puts A right of the auto-placed B (a={a:?}, b={b:?})"
        );
        assert!((a.1 - b.1).abs() < 1.0, "A and B share the first row");
    }

    #[test]
    fn grid_row_start_places_item_on_a_later_row() {
        // An item with `grid-row: 2` drops to the second row; a plain item stays
        // on the first. So "Late" sits below "Early".
        let layout = run(
            r#"<div style="display:grid;grid-template-columns:1fr 1fr">
                 <div>Early</div><div style="grid-row:2;grid-column:1">Late</div>
               </div>"#,
        );
        let t = text_xy(&layout);
        let early = t.iter().find(|(_, _, s)| s == "Early").unwrap();
        let late = t.iter().find(|(_, _, s)| s == "Late").unwrap();
        assert!(
            late.1 > early.1,
            "grid-row:2 places 'Late' below 'Early' (early={early:?}, late={late:?})"
        );
        assert!((late.0 - early.0).abs() < 1.0, "both in column 1 (same x)");
    }

    // ── clear (CSS clear: left | right | both) ──

    #[test]
    fn clear_left_drops_block_below_a_left_float() {
        // A tall left float, then a `clear:left` block. The cleared block's top
        // must sit at/below the float's bottom; without `clear` it would start at
        // the container top (much higher). We compare the two layouts' block y.
        let floated = "<div style=\"float:left;width:80pt\">\
                       <p>f1</p><p>f2</p><p>f3</p><p>f4</p><p>f5</p></div>";
        let cleared = run(&format!(
            "<div>{floated}<div style=\"clear:left\">After</div></div>"
        ));
        let not_cleared = run(&format!(
            "<div>{floated}<div>After</div></div>"
        ));
        let y_of = |l: &Layout| text_xy(l).into_iter().find(|(_, _, s)| s == "After").unwrap().1;
        let yc = y_of(&cleared);
        let yn = y_of(&not_cleared);
        assert!(
            yc > yn + 20.0,
            "clear:left pushes 'After' well below the non-cleared baseline (cleared={yc}, baseline={yn})"
        );
    }

    #[test]
    fn clear_right_ignores_a_left_float() {
        // `clear:right` only clears RIGHT floats. Against a LEFT float it has no
        // effect, so the block stays at the same height as a non-cleared block.
        let floated = "<div style=\"float:left;width:80pt\">\
                       <p>f1</p><p>f2</p><p>f3</p><p>f4</p><p>f5</p></div>";
        let clear_right = run(&format!(
            "<div>{floated}<div style=\"clear:right\">After</div></div>"
        ));
        let not_cleared = run(&format!(
            "<div>{floated}<div>After</div></div>"
        ));
        let y_of = |l: &Layout| text_xy(l).into_iter().find(|(_, _, s)| s == "After").unwrap().1;
        assert!(
            (y_of(&clear_right) - y_of(&not_cleared)).abs() < 1.0,
            "clear:right does not clear a left float (right={}, baseline={})",
            y_of(&clear_right),
            y_of(&not_cleared)
        );
    }

    #[test]
    fn clear_both_drops_below_a_right_float() {
        // `clear:both` clears either side, so it drops below a RIGHT float too.
        let floated = "<div style=\"float:right;width:80pt\">\
                       <p>f1</p><p>f2</p><p>f3</p><p>f4</p><p>f5</p></div>";
        let cleared = run(&format!(
            "<div>{floated}<div style=\"clear:both\">After</div></div>"
        ));
        let not_cleared = run(&format!(
            "<div>{floated}<div>After</div></div>"
        ));
        let y_of = |l: &Layout| text_xy(l).into_iter().find(|(_, _, s)| s == "After").unwrap().1;
        assert!(
            y_of(&cleared) > y_of(&not_cleared) + 20.0,
            "clear:both drops below a right float (cleared={}, baseline={})",
            y_of(&cleared),
            y_of(&not_cleared)
        );
    }

    // ── multi-column (CSS column-count / columns / column-gap) ──

    #[test]
    fn column_count_splits_content_into_two_columns() {
        // Six paragraphs in a 2-column block: the first paragraphs fill the left
        // column (x near the content origin), the later ones spill into the right
        // column (a distinctly larger x). The right column starts at roughly the
        // column width + gutter further right than the left.
        let layout = run(
            r#"<div style="column-count:2">
                 <p>Alpha</p><p>Bravo</p><p>Charlie</p>
                 <p>Delta</p><p>Echo</p><p>Foxtrot</p>
               </div>"#,
        );
        let t = text_xy(&layout);
        let x_of = |s: &str| t.iter().find(|(_, _, label)| label == s).unwrap().0;

        let left_x = x_of("Alpha");
        let right_x = x_of("Foxtrot");
        // Left column hugs the content origin (page margin 36).
        assert!(
            (left_x - 36.0).abs() < 2.0,
            "first paragraph is in the left column at the content origin ({left_x})"
        );
        // The last paragraph landed in the right column — clearly further right.
        assert!(
            right_x > left_x + 100.0,
            "later content flows into the right column (left={left_x}, right={right_x})"
        );
        // Both columns sit within the content box and don't overlap horizontally.
        // content_w = 540, gap defaults to 1em (=12pt for the inherited 12pt
        // body font) ⇒ col_w = (540 - 12) / 2 = 264, right column at 36+264+12.
        assert!(
            (right_x - (36.0 + 264.0 + 12.0)).abs() < 4.0,
            "right column starts at ~col_w + gutter from the left ({right_x})"
        );
    }

    #[test]
    fn columns_balance_height_across_columns() {
        // Many short paragraphs: balancing should put roughly half in each column,
        // so the left and right columns reach comparable bottoms (neither column
        // is left almost empty). We compare the lowest y in each column.
        let mut html = String::from(r#"<div style="column-count:2">"#);
        for i in 0..12 {
            html.push_str(&format!("<p>Para number {i}</p>"));
        }
        html.push_str("</div>");
        let layout = run(&html);
        let t = text_xy(&layout);
        // Split fragments by which column their x falls into (boundary ≈ midway).
        let boundary = 36.0 + 264.0; // left col right edge
        let left_bottom = t
            .iter()
            .filter(|(x, _, _)| *x < boundary)
            .map(|(_, y, _)| *y)
            .fold(0.0_f64, f64::max);
        let right_bottom = t
            .iter()
            .filter(|(x, _, _)| *x >= boundary)
            .map(|(_, y, _)| *y)
            .fold(0.0_f64, f64::max);
        assert!(left_bottom > 36.0, "left column has content");
        assert!(right_bottom > 36.0, "right column has content");
        // Balanced: the two columns end within ~one paragraph-height of each other.
        assert!(
            (left_bottom - right_bottom).abs() < 40.0,
            "columns are height-balanced (left bottom {left_bottom}, right {right_bottom})"
        );
    }

    #[test]
    fn column_gap_widens_the_gutter_and_pushes_the_right_column() {
        // A larger `column-gap` narrows each column and shifts the right column's
        // start further right than the default gutter would.
        let wide = run(
            r#"<div style="column-count:2;column-gap:60pt">
                 <p>One</p><p>Two</p><p>Three</p><p>Four</p>
               </div>"#,
        );
        let narrow = run(
            r#"<div style="column-count:2;column-gap:4pt">
                 <p>One</p><p>Two</p><p>Three</p><p>Four</p>
               </div>"#,
        );
        let right_x = |l: &Layout| {
            // The right-column start = the largest distinct x among fragments.
            text_xy(l)
                .into_iter()
                .map(|(x, _, _)| x)
                .fold(0.0_f64, f64::max)
        };
        let rw = right_x(&wide);
        let rn = right_x(&narrow);
        // wide gutter ⇒ col_w=(540-60)/2=240, right col at 36+240+60=336.
        // narrow gutter ⇒ col_w=(540-4)/2=268, right col at 36+268+4=308.
        assert!(
            rw > rn + 20.0,
            "a wider column-gap pushes the right column further right (wide={rw}, narrow={rn})"
        );
        assert!(
            (rw - 336.0).abs() < 5.0,
            "wide-gutter right column starts near 336 ({rw})"
        );
    }

    #[test]
    fn columns_shorthand_count_is_honoured() {
        // `columns: 3` (the count form of the shorthand) yields three columns;
        // with nine short blocks the last one lands in the third column, well
        // right of the first.
        let layout = run(
            r#"<div style="columns:3">
                 <p>A1</p><p>A2</p><p>A3</p>
                 <p>B1</p><p>B2</p><p>B3</p>
                 <p>C1</p><p>C2</p><p>C3</p>
               </div>"#,
        );
        let t = text_xy(&layout);
        let xs: Vec<f64> = t.iter().map(|(x, _, _)| *x).collect();
        let min_x = xs.iter().copied().fold(f64::INFINITY, f64::min);
        let max_x = xs.iter().copied().fold(0.0_f64, f64::max);
        // Three columns over 540 with a 12pt default gutter ⇒ col_w=172,
        // third column at 36 + 2*(172+12) = 404 — far right of the first.
        assert!(
            max_x > min_x + 250.0,
            "three columns span the content width (min={min_x}, max={max_x})"
        );
        assert!(
            (min_x - 36.0).abs() < 2.0,
            "first column at the content origin ({min_x})"
        );
    }

    #[test]
    fn single_column_block_is_unchanged_no_regression() {
        // A plain block (no column-count) must lay out exactly as before: all
        // paragraphs stack in one column at the content origin, increasing y.
        let plain = run("<div><p>One</p><p>Two</p><p>Three</p></div>");
        let t = text_xy(&plain);
        assert_eq!(t.len(), 3, "three paragraphs, one each");
        // All at the same (content-origin) x and strictly increasing y.
        for (x, _, _) in &t {
            assert!((*x - 36.0).abs() < 1.0, "stacked at the content origin ({x})");
        }
        assert!(
            t[0].1 < t[1].1 && t[1].1 < t[2].1,
            "paragraphs stack top-to-bottom ({:?})",
            t.iter().map(|(_, y, _)| *y).collect::<Vec<_>>()
        );
        // `column-count:1` is explicitly a no-op (single column).
        let one = run(r#"<div style="column-count:1"><p>One</p><p>Two</p></div>"#);
        let to = text_xy(&one);
        for (x, _, _) in &to {
            assert!((*x - 36.0).abs() < 1.0, "column-count:1 stays single-column ({x})");
        }
    }

    #[test]
    fn columns_preserve_reading_order_left_then_right() {
        // The unit that fills the left column comes before, in document order,
        // the unit that opens the right column — i.e. the flow goes top-of-left
        // then top-of-right (newspaper columns), not interleaved.
        let layout = run(
            r#"<div style="column-count:2">
                 <p>First</p><p>Second</p><p>Third</p>
                 <p>Fourth</p><p>Fifth</p><p>Sixth</p>
               </div>"#,
        );
        let t = text_xy(&layout);
        let pos = |s: &str| {
            let (x, y, _) = t.iter().find(|(_, _, l)| l == s).unwrap();
            (*x, *y)
        };
        let (first_x, first_y) = pos("First");
        let (sixth_x, _sixth_y) = pos("Sixth");
        // "First" tops the left column; "Sixth" is in the right column lower than
        // where it would be if all six stacked in one column.
        assert!(first_x < sixth_x - 100.0, "First in left, Sixth in right");
        assert!(
            (first_y - 36.0).abs() < 12.0,
            "First sits at the top of the left column ({first_y})"
        );
    }

    // ── Quick-win 3: `margin: 0 auto` centres a fixed-width block ───────────

    #[test]
    fn margin_auto_centres_fixed_width_block() {
        // A 200pt block centred in the 540pt content area (page 612 − 2×36
        // margin) should start at x ≈ 36 + (540 − 200)/2 = 206.
        let layout = run(
            "<div style=\"width:200pt;margin:0 auto;background:#eee\">centered</div>",
        );
        let x = layout
            .pages
            .iter()
            .flatten()
            .find_map(|f| match f {
                Fragment::Text { x, text, .. } if text == "centered" => Some(*x),
                _ => None,
            })
            .expect("text fragment");
        assert!(
            (x - 206.0).abs() < 4.0,
            "centred block content starts near x=206 (got {x})"
        );

        // Without auto margins the same block hugs the left content edge (x≈36).
        let left = run("<div style=\"width:200pt;background:#eee\">left</div>");
        let lx = left
            .pages
            .iter()
            .flatten()
            .find_map(|f| match f {
                Fragment::Text { x, text, .. } if text == "left" => Some(*x),
                _ => None,
            })
            .expect("text fragment");
        assert!(lx < 60.0, "non-auto block stays left ({lx})");
    }

    // ── Quick-win 4: adjacent block margins collapse (max, not sum) ─────────

    #[test]
    fn adjacent_block_margins_collapse() {
        // Two stacked blocks, each with 20pt vertical margins. The gap between
        // them must be ~20pt (the larger margin), not 40pt (their sum).
        let layout = run(
            "<div style=\"margin:20pt 0;height:10pt\">A</div>\
             <div style=\"margin:20pt 0;height:10pt\">B</div>",
        );
        let ay = cell_y(&layout, "A");
        let by = cell_y(&layout, "B");
        let gap = by - ay; // baseline-to-baseline ≈ A height + collapsed margin
        // A's content (~10pt height) + 20pt collapsed margin ≈ 30pt. Without
        // collapsing it would be ~50pt. Assert it is clearly under 45.
        assert!(
            (25.0..45.0).contains(&gap),
            "collapsed gap ~30pt, not the 50pt sum (got {gap})"
        );
    }

    // ── Quick-win 5: `page-break-inside: avoid` keeps a block whole ─────────

    #[test]
    fn page_break_inside_avoid_moves_block_to_next_page() {
        // Fill most of page 1 with a tall spacer, then an `avoid` block that is
        // short enough to fit on one page but, placed in flow, would straddle
        // the page-1/page-2 boundary. It must move whole onto page 2.
        let layout = run(
            "<div style=\"height:700pt\">spacer</div>\
             <div style=\"page-break-inside:avoid;height:120pt\">keep</div>",
        );
        assert!(layout.pages.len() >= 2, "content spans pages");
        // The whole `keep` block (its text) lands on page 2, not page 1.
        let on_p1 = layout.pages[0]
            .iter()
            .any(|f| matches!(f, Fragment::Text { text, .. } if text == "keep"));
        let on_p2 = layout
            .pages
            .get(1)
            .map(|p| p.iter().any(|f| matches!(f, Fragment::Text { text, .. } if text == "keep")))
            .unwrap_or(false);
        assert!(!on_p1, "avoid block does not start on page 1");
        assert!(on_p2, "avoid block moved whole to page 2");
    }

    #[test]
    fn page_break_inside_avoid_keeps_block_that_already_fits() {
        // A short `avoid` block that fits entirely on page 1 is NOT moved.
        let layout = run(
            "<div style=\"height:50pt\">top</div>\
             <div style=\"page-break-inside:avoid;height:50pt\">keep</div>",
        );
        let on_p1 = layout.pages[0]
            .iter()
            .any(|f| matches!(f, Fragment::Text { text, .. } if text == "keep"));
        assert!(on_p1, "fitting avoid block stays on page 1");
    }

    // ── CSS grid: fr / fixed / minmax tracks, gaps, spanning ───────────────

    #[test]
    fn grid_fr_tracks_share_space_proportionally() {
        // Two columns 1fr / 3fr over a 540pt content area: col0 = 135, col1 = 405.
        // Cell B (in the 3fr column) starts at 36 + 135 = 171.
        let layout = run(
            r#"<div style="display:grid;grid-template-columns:1fr 3fr"><div>A</div><div>B</div></div>"#,
        );
        let a = cell_x(&layout, "A");
        let b = cell_x(&layout, "B");
        assert!((a - 36.0).abs() < 2.0, "A hugs the left ({a})");
        assert!(
            (b - (36.0 + 135.0)).abs() < 3.0,
            "B starts at the 1fr boundary (~171), not the equal-split 306 ({b})"
        );
    }

    #[test]
    fn grid_fixed_px_column_then_fr_fills_rest() {
        // `120pt fixed | 1fr`: col0 = 120pt, so B starts at 36 + 120 = 156, well
        // left of the equal-split midpoint (36 + 270 = 306).
        let layout = run(
            r#"<div style="display:grid;grid-template-columns:120pt 1fr"><div>A</div><div>B</div></div>"#,
        );
        let b = cell_x(&layout, "B");
        assert!(
            (b - (36.0 + 120.0)).abs() < 3.0,
            "B starts after the fixed 120pt column (~156), not 306 ({b})"
        );
    }

    #[test]
    fn grid_minmax_min_pushes_second_column() {
        // `minmax(300pt, 1fr) | 1fr`: the first track takes at least 300pt, so it
        // gets 300 (min) + half the 240 leftover = 420; B starts at 36 + 420.
        let layout = run(
            r#"<div style="display:grid;grid-template-columns:minmax(300pt,1fr) 1fr"><div>A</div><div>B</div></div>"#,
        );
        let b = cell_x(&layout, "B");
        assert!(
            b > 36.0 + 300.0,
            "minmax min (300pt) pushes B past x=336 ({b})"
        );
    }

    #[test]
    fn grid_column_gap_pushes_second_column() {
        // 1fr / 1fr with a 60pt column-gap: inner = 540 − 60 = 480, each col 240;
        // col1 starts at 36 + 240 + 60 = 336 (vs 306 with no gap).
        let gapped = run(
            r#"<div style="display:grid;grid-template-columns:1fr 1fr;column-gap:60pt"><div>A</div><div>B</div></div>"#,
        );
        let nogap = run(
            r#"<div style="display:grid;grid-template-columns:1fr 1fr"><div>A</div><div>B</div></div>"#,
        );
        let bg = cell_x(&gapped, "B");
        let bn = cell_x(&nogap, "B");
        assert!(
            bg > bn + 20.0,
            "column-gap pushes B right (gapped={bg}, nogap={bn})"
        );
    }

    #[test]
    fn grid_row_gap_separates_rows() {
        // Two rows separated by a 50pt row-gap: C3 (row 2) sits at least 50pt
        // below where it would without the gap.
        let gapped = run(
            r#"<div style="display:grid;grid-template-columns:1fr 1fr;row-gap:50pt"><div>C1</div><div>C2</div><div>C3</div><div>C4</div></div>"#,
        );
        let nogap = run(
            r#"<div style="display:grid;grid-template-columns:1fr 1fr"><div>C1</div><div>C2</div><div>C3</div><div>C4</div></div>"#,
        );
        let yg = cell_y(&gapped, "C3");
        let yn = cell_y(&nogap, "C3");
        assert!(
            yg > yn + 40.0,
            "row-gap drops C3 onto a lower row (gapped={yg}, nogap={yn})"
        );
    }

    #[test]
    fn grid_column_span_widens_a_cell() {
        // 3 columns; the first item spans 2 columns. The auto-flowing second
        // item lands in column 3 (x = 36 + 2·180 = 396), proving the spanning
        // cell occupied columns 1–2.
        let layout = run(
            r#"<div style="display:grid;grid-template-columns:1fr 1fr 1fr">
                 <div style="grid-column:span 2">Wide</div><div>Next</div>
               </div>"#,
        );
        let wide = cell_x(&layout, "Wide");
        let next = cell_x(&layout, "Next");
        assert!((wide - 36.0).abs() < 2.0, "spanning cell starts at the left ({wide})");
        assert!(
            (next - (36.0 + 360.0)).abs() < 4.0,
            "Next lands in column 3 after the 2-col span (~396), got {next}"
        );
    }

    #[test]
    fn grid_row_span_keeps_following_item_on_first_row() {
        // A row-spanning item in column 1 leaves column 2 of the first row free,
        // so the next item stays on row 1 (same y), to its right.
        let layout = run(
            r#"<div style="display:grid;grid-template-columns:1fr 1fr">
                 <div style="grid-row:span 2">Tall</div><div>Side</div><div>Below</div>
               </div>"#,
        );
        let tall = text_xy(&layout).into_iter().find(|(_, _, s)| s == "Tall").unwrap();
        let side = text_xy(&layout).into_iter().find(|(_, _, s)| s == "Side").unwrap();
        let below = text_xy(&layout).into_iter().find(|(_, _, s)| s == "Below").unwrap();
        assert!(side.0 > tall.0, "Side is right of the spanning Tall");
        assert!((side.1 - tall.1).abs() < 2.0, "Side shares the first row with Tall");
        // `Below` auto-flows into row 2 column 2 (Tall still occupies r2c1).
        assert!(below.1 > tall.1, "Below drops to a later row ({:?})", below);
        assert!(below.0 > tall.0, "Below sits in column 2 under Side");
    }

    #[test]
    fn grid_explicit_row_height_is_honoured() {
        // A first row fixed at 100pt pushes the second-row cell down by ~100pt
        // (vs an auto first row sized to one line of text, ~12pt).
        let fixed = run(
            r#"<div style="display:grid;grid-template-columns:1fr;grid-template-rows:100pt auto"><div>R1</div><div>R2</div></div>"#,
        );
        let auto = run(
            r#"<div style="display:grid;grid-template-columns:1fr"><div>R1</div><div>R2</div></div>"#,
        );
        let yf = cell_y(&fixed, "R2");
        let ya = cell_y(&auto, "R2");
        assert!(
            yf > ya + 60.0,
            "explicit 100pt first row pushes R2 well below the auto layout (fixed={yf}, auto={ya})"
        );
    }

    // ── flexbox: basis, shrink, wrap, justify, align ───────────────────────

    #[test]
    fn flex_basis_sets_initial_main_size() {
        // A with flex-basis 120pt, B fills the rest: B starts at 36 + 120 = 156.
        let layout = run(
            r#"<div style="display:flex"><div style="flex-basis:120pt">A</div><div>B</div></div>"#,
        );
        let b = cell_x(&layout, "B");
        assert!(
            (b - (36.0 + 120.0)).abs() < 4.0,
            "flex-basis fixes A at 120pt so B starts ~156 ({b})"
        );
    }

    #[test]
    fn flex_shrink_reduces_overflowing_items() {
        // Two items each basis 400pt (sum 800 > 540 content). With equal shrink
        // they shrink proportionally; B must start LEFT of its un-shrunk 400pt
        // position (36 + 400 = 436) — around the 540/2 = 270 mark.
        let layout = run(
            r#"<div style="display:flex"><div style="flex:0 1 400pt">A</div><div style="flex:0 1 400pt">B</div></div>"#,
        );
        let b = cell_x(&layout, "B");
        assert!(
            b < 36.0 + 400.0 - 50.0,
            "flex-shrink pulls B left of its 400pt basis position ({b})"
        );
        assert!(b > 36.0 + 200.0, "but B is still past the midpoint area ({b})");
    }

    #[test]
    fn flex_shrink_zero_keeps_item_at_basis() {
        // A: flex-shrink 0 basis 400pt — refuses to shrink. B (shrinkable) gives
        // up the overflow, so A still starts at the left edge and keeps its width:
        // B starts at ~36 + 400 = 436 (A unshrunk), unlike the both-shrink case.
        let layout = run(
            r#"<div style="display:flex"><div style="flex:0 0 400pt">A</div><div style="flex:0 1 400pt">B</div></div>"#,
        );
        let b = cell_x(&layout, "B");
        assert!(
            b > 36.0 + 360.0,
            "flex-shrink:0 keeps A at 400pt so B starts near 436 ({b})"
        );
    }

    #[test]
    fn flex_wrap_breaks_onto_a_second_line() {
        // Three items each basis 250pt (sum 750 > 540) with flex-wrap: the third
        // wraps below the first. Item 3 sits lower than item 1, at the same x.
        let layout = run(
            r#"<div style="display:flex;flex-wrap:wrap">
                 <div style="flex:0 0 250pt">One</div>
                 <div style="flex:0 0 250pt">Two</div>
                 <div style="flex:0 0 250pt">Three</div>
               </div>"#,
        );
        let one = text_xy(&layout).into_iter().find(|(_, _, s)| s == "One").unwrap();
        let three = text_xy(&layout).into_iter().find(|(_, _, s)| s == "Three").unwrap();
        assert!(
            three.1 > one.1 && (three.0 - one.0).abs() < 3.0,
            "Three wraps below One at the same x (one={one:?}, three={three:?})"
        );
    }

    #[test]
    fn flex_justify_content_center_offsets_items() {
        // Two fixed 100pt items, justify-content center: total content 200, free
        // 340, leading offset 170 ⇒ A starts at ~36 + 170 = 206.
        let layout = run(
            r#"<div style="display:flex;justify-content:center"><div style="flex:0 0 100pt">A</div><div style="flex:0 0 100pt">B</div></div>"#,
        );
        let a = cell_x(&layout, "A");
        assert!(
            a > 36.0 + 120.0,
            "justify-content:center pushes the first item right (~206), got {a}"
        );
    }

    #[test]
    fn flex_justify_content_space_between_pins_edges() {
        // space-between: first item at the left edge, last at the right. B (100pt
        // wide) ends at the content right edge 576, so it starts near 476.
        let layout = run(
            r#"<div style="display:flex;justify-content:space-between"><div style="flex:0 0 100pt">A</div><div style="flex:0 0 100pt">B</div></div>"#,
        );
        let a = cell_x(&layout, "A");
        let b = cell_x(&layout, "B");
        assert!((a - 36.0).abs() < 4.0, "A pinned to the left ({a})");
        assert!(b > 36.0 + 400.0, "B pushed to the right edge ({b})");
    }

    #[test]
    fn flex_justify_content_space_evenly_uses_equal_gaps() {
        // space-evenly distributes `n + 1` EQUAL gaps: the leading gap (before the
        // first item) equals the gap between items — unlike space-around, which
        // puts half-size gaps at the two ends.
        let layout = run(
            r#"<div style="display:flex;justify-content:space-evenly"><div style="flex:0 0 100pt">A</div><div style="flex:0 0 100pt">B</div></div>"#,
        );
        let a = cell_x(&layout, "A");
        let b = cell_x(&layout, "B");
        let left_gap = a - 36.0; // before A (content band starts ~36pt)
        let mid_gap = b - (a + 100.0); // between A's right edge and B (A is 100pt wide)
        assert!(left_gap > 50.0, "non-trivial leading gap ({left_gap})");
        assert!(
            (left_gap - mid_gap).abs() < 5.0,
            "equal gaps: leading {left_gap} vs inter-item {mid_gap}"
        );
    }

    #[test]
    fn flex_direction_row_reverse_swaps_item_order() {
        // row-reverse runs the main axis right-to-left, so the first DOM item (A)
        // is placed at the far (right) end — to the RIGHT of the second item (B).
        let layout = run(
            r#"<div style="display:flex;flex-direction:row-reverse"><div style="flex:0 0 100pt">A</div><div style="flex:0 0 100pt">B</div></div>"#,
        );
        assert!(
            cell_x(&layout, "A") > cell_x(&layout, "B"),
            "row-reverse puts A right of B"
        );
        // A forward row keeps A left of B (guards against reversing everything).
        let fwd = run(
            r#"<div style="display:flex"><div style="flex:0 0 100pt">A</div><div style="flex:0 0 100pt">B</div></div>"#,
        );
        assert!(
            cell_x(&fwd, "A") < cell_x(&fwd, "B"),
            "forward row keeps A left of B"
        );
    }

    #[test]
    fn flex_align_items_center_lowers_short_item() {
        // A short item next to a tall one, align-items:center. The short item's
        // text is vertically centred in the line band, so it sits LOWER than the
        // tall item's first line (which starts at the band top).
        let tall = "<div style=\"flex:0 0 200pt\"><p>t1</p><p>t2</p><p>t3</p><p>t4</p></div>";
        let layout = run(&format!(
            "<div style=\"display:flex;align-items:center\">{tall}<div style=\"flex:0 0 200pt\">short</div></div>"
        ));
        let t1 = text_xy(&layout).into_iter().find(|(_, _, s)| s == "t1").unwrap();
        let short = text_xy(&layout).into_iter().find(|(_, _, s)| s == "short").unwrap();
        assert!(
            short.1 > t1.1 + 10.0,
            "align-items:center lowers the short item below the tall item's first line (t1={t1:?}, short={short:?})"
        );
    }

    // ── RTL (right-to-left) ──────────────────────────────────────────────────
    //
    // Page 612pt, margins 36pt ⇒ content runs in [36, 576] (avail_w = 540).
    // `AverageMeasure` gives 8pt per glyph at the default 16pt font, and a space
    // is one glyph (8pt). So a 4-glyph word is 32pt wide.

    // x of the (single) text fragment whose text equals `label`.
    fn run_x(layout: &Layout, label: &str) -> f64 {
        text_xy(layout)
            .into_iter()
            .find(|(_, _, s)| s == label)
            .unwrap_or_else(|| panic!("no text fragment {label:?}"))
            .0
    }

    #[test]
    fn rtl_block_defaults_to_right_alignment() {
        // The same word in an RTL block hugs the right edge; in an LTR block it
        // starts at the left content edge (x = 36). Right edge = 576, word = 32pt,
        // so the RTL run's left corner lands near 576 - 32 = 544.
        let rtl = run_x(&run(r#"<p dir="rtl">word</p>"#), "word");
        let ltr = run_x(&run(r#"<p>word</p>"#), "word");
        assert!(
            (ltr - 36.0).abs() < 0.01,
            "LTR word starts at the left content edge (x={ltr})"
        );
        assert!(
            rtl > ltr + 400.0,
            "RTL word is right-aligned, far past the LTR position (rtl={rtl}, ltr={ltr})"
        );
        assert!(
            (rtl - (576.0 - 32.0)).abs() < 1.0,
            "RTL word's left corner sits one word-width in from the right edge (x={rtl})"
        );
    }

    #[test]
    fn rtl_inline_boxes_run_right_to_left() {
        // Two adjacent inline spans in logical order AAA then BBB. In RTL the first
        // logical box sits at the right, the next advances to its left, so
        // x(AAA) > x(BBB). The LTR control keeps source order: x(AAA) < x(BBB).
        let rtl = run(r#"<p dir="rtl"><span>AAA</span><span>BBB</span></p>"#);
        let a = run_x(&rtl, "AAA");
        let b = run_x(&rtl, "BBB");
        assert!(
            a > b,
            "RTL lays the first logical box (AAA) to the right of the next (BBB) (a={a}, b={b})"
        );

        let ltr = run(r#"<p><span>AAA</span><span>BBB</span></p>"#);
        let la = run_x(&ltr, "AAA");
        let lb = run_x(&ltr, "BBB");
        assert!(
            la < lb,
            "LTR keeps source order left-to-right (la={la}, lb={lb})"
        );
    }

    #[test]
    fn rtl_hebrew_run_placed_from_right_edge() {
        // A Hebrew word (logical order preserved in the fragment text) is placed
        // with its left corner one word-width in from the right edge. The glyphs
        // themselves are whatever the font yields — we never reorder the string.
        let layout = run(r#"<p dir="rtl">שלום</p>"#);
        let (x, _, text) = text_xy(&layout)
            .into_iter()
            .find(|(_, _, s)| s == "שלום")
            .expect("hebrew run present, text unchanged");
        assert_eq!(text, "שלום", "the run's logical text is left intact");
        // 4 Hebrew glyphs ⇒ 32pt wide ⇒ left corner ≈ 576 - 32 = 544.
        assert!(
            (x - (576.0 - 32.0)).abs() < 1.0,
            "hebrew run starts from the right edge (x={x})"
        );
    }

    #[test]
    fn rtl_via_css_direction_property() {
        // `direction: rtl` (the CSS property, not the `dir` attribute) also flips
        // the block to right alignment.
        let x = run_x(&run(r#"<p style="direction:rtl">word</p>"#), "word");
        assert!(
            x > 36.0 + 400.0,
            "CSS direction:rtl right-aligns the run (x={x})"
        );
    }

    #[test]
    fn rtl_direction_inherits_to_children() {
        // `dir` on an ancestor cascades: the inner <p> inherits rtl and right-aligns.
        let x = run_x(&run(r#"<div dir="rtl"><p>child</p></div>"#), "child");
        assert!(
            x > 36.0 + 400.0,
            "inner paragraph inherits rtl and right-aligns (x={x})"
        );
    }

    #[test]
    fn text_align_start_end_resolve_per_direction() {
        // `start`/`end` are direction-relative. In LTR: start = left (36), end =
        // right edge. In RTL: start = right edge, end = left (36).
        let ltr_start = run_x(&run(r#"<p style="text-align:start">w</p>"#), "w");
        let ltr_end = run_x(&run(r#"<p style="text-align:end">w</p>"#), "w");
        assert!(
            (ltr_start - 36.0).abs() < 0.01,
            "LTR text-align:start = left edge (x={ltr_start})"
        );
        assert!(
            ltr_end > 36.0 + 400.0,
            "LTR text-align:end = right edge (x={ltr_end})"
        );

        let rtl_start = run_x(&run(r#"<p dir="rtl" style="text-align:start">w</p>"#), "w");
        let rtl_end = run_x(&run(r#"<p dir="rtl" style="text-align:end">w</p>"#), "w");
        assert!(
            rtl_start > 36.0 + 400.0,
            "RTL text-align:start = right edge (x={rtl_start})"
        );
        assert!(
            (rtl_end - 36.0).abs() < 0.01,
            "RTL text-align:end = left edge (x={rtl_end})"
        );
    }

    #[test]
    fn rtl_explicit_left_align_pushes_to_left_edge() {
        // An explicit physical `text-align:left` inside an RTL block still lands
        // the run's left corner at the left content edge (36) — the box block is
        // pushed left, only the within-line box order is reversed.
        let x = run_x(&run(r#"<p dir="rtl" style="text-align:left">word</p>"#), "word");
        assert!(
            (x - 36.0).abs() < 0.01,
            "RTL + text-align:left puts the single run at the left edge (x={x})"
        );
    }

    #[test]
    fn ltr_default_layout_is_byte_identical() {
        // Guard: a plain LTR paragraph keeps the exact pre-RTL geometry — left
        // content edge, no direction machinery engaged.
        let layout = run(r#"<p>hello world example</p>"#);
        let first = text_xy(&layout)
            .into_iter()
            .find(|(_, _, s)| s == "hello")
            .unwrap();
        assert!(
            (first.0 - 36.0).abs() < 0.01,
            "LTR first run still starts at x=36 (got {})",
            first.0
        );
    }

    // ── border-radius / box-shadow ──────────────────────────────────────────

    /// All `Fragment::Rect` on page 0 as `(fill, stroke, radius, shadow_present)`.
    #[allow(clippy::type_complexity)]
    fn rect_decorations(
        layout: &Layout,
    ) -> Vec<(Option<[f64; 3]>, Option<[f64; 3]>, [f64; 4], bool)> {
        layout
            .pages
            .iter()
            .flatten()
            .filter_map(|f| match f {
                Fragment::Rect {
                    fill,
                    stroke,
                    radius,
                    shadow,
                    ..
                } => Some((*fill, *stroke, *radius, shadow.is_some())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn square_box_carries_zero_radius_and_no_shadow() {
        // Guard: a plain background div is unchanged — fill rect, radius all-zero,
        // no shadow (so the painter still takes the rectangular fast path).
        let layout = run(r#"<div style="background:#eee;padding:10pt">hi</div>"#);
        let rects = rect_decorations(&layout);
        let bg = rects
            .iter()
            .find(|(fill, ..)| fill.is_some())
            .expect("a background rect");
        assert_eq!(bg.2, [0.0; 4], "no radius on a plain box");
        assert!(!bg.3, "no shadow on a plain box");
    }

    #[test]
    fn mark_paints_an_inline_highlight_behind_the_text() {
        // `<mark>` (UA `background:#ffff00`) and a span with `background-color`
        // each paint a filled rect behind their glyphs; the text itself is still
        // emitted. The rect sits at z=0 (before the z=1 text in paint order).
        let layout = run(r#"<p><mark>hi</mark> <span style="background-color:#00ff00">x</span></p>"#);
        let fills: Vec<[f64; 3]> = rect_decorations(&layout)
            .into_iter()
            .filter_map(|(fill, ..)| fill)
            .collect();
        assert!(
            fills.contains(&[1.0, 1.0, 0.0]),
            "yellow <mark> highlight rect present: {fills:?}"
        );
        assert!(
            fills.contains(&[0.0, 1.0, 0.0]),
            "green span highlight rect present: {fills:?}"
        );
        let texts: Vec<String> = layout
            .pages
            .iter()
            .flatten()
            .filter_map(|f| match f {
                Fragment::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(texts.iter().any(|t| t == "hi"), "the marked text is drawn");
    }

    #[test]
    fn block_background_does_not_duplicate_behind_its_text() {
        // A block's own `background` is a non-inherited box property: it must be
        // painted exactly once (the box), never re-emitted as an inline highlight
        // behind its direct text run. Guards the non-inheritance fix.
        let layout = run(r#"<div style="background:#3366cc;padding:10pt">card</div>"#);
        let blue = rect_decorations(&layout)
            .into_iter()
            .filter(|(fill, ..)| *fill == Some([0.2, 0.4, 0.8]))
            .count();
        assert_eq!(blue, 1, "exactly one blue rect (the box), no text duplicate");
    }

    #[test]
    fn rounded_background_emits_rect_with_radius() {
        let layout =
            run(r#"<div style="background:#3366cc;border-radius:8pt;padding:10pt">card</div>"#);
        let rects = rect_decorations(&layout);
        let bg = rects
            .iter()
            .find(|(fill, ..)| *fill == Some([0.2, 0.4, 0.8]))
            .expect("the blue background rect");
        assert!(
            bg.2.iter().all(|r| (*r - 8.0).abs() < 0.01),
            "every corner is 8pt: {:?}",
            bg.2
        );
    }

    #[test]
    fn uniform_border_plus_radius_makes_one_fill_and_stroke_rect() {
        // A rounded card with a uniform 2pt border ⇒ a SINGLE rect carrying both
        // fill and stroke + radius (not a separate fill rect + 4 border edges).
        let layout = run(
            r#"<div style="background:#ffffff;border:2pt solid #000000;border-radius:10pt;padding:8pt">x</div>"#,
        );
        let rects = rect_decorations(&layout);
        let rounded: Vec<_> = rects
            .iter()
            .filter(|(_, _, radius, _)| radius.iter().any(|r| *r > 0.0))
            .collect();
        assert_eq!(rounded.len(), 1, "exactly one rounded decoration rect");
        let (fill, stroke, radius, _) = rounded[0];
        assert_eq!(*fill, Some([1.0, 1.0, 1.0]), "fill present");
        assert_eq!(
            *stroke,
            Some([0.0, 0.0, 0.0]),
            "uniform border ⇒ stroke present"
        );
        assert!(radius.iter().all(|r| (*r - 10.0).abs() < 0.01));
        // No square per-side border rects were emitted alongside it.
        let zero_radius_fills = rects
            .iter()
            .filter(|(fill, _, radius, _)| fill.is_some() && *radius == [0.0; 4])
            .count();
        assert_eq!(zero_radius_fills, 0, "no square fill/border rects remain");
    }

    #[test]
    fn box_shadow_rides_on_the_background_rect() {
        let layout = run(
            r#"<div style="background:#eeeeee;box-shadow:3pt 3pt 4pt #000000;padding:10pt">x</div>"#,
        );
        let rects = rect_decorations(&layout);
        assert!(
            rects
                .iter()
                .any(|(fill, _, _, shadow)| fill.is_some() && *shadow),
            "the background rect carries the shadow: {rects:?}"
        );
    }

    #[test]
    fn asymmetric_border_with_radius_rounds_background_only() {
        // A radius + a thicker bottom border (asymmetric) ⇒ the background still
        // rounds, but borders fall back to the square per-side path (documented
        // best-effort). We assert a rounded fill rect exists AND square border
        // edge rects also exist.
        let layout = run(
            r#"<div style="background:#ddeeff;border:1pt solid #000;border-bottom:4pt solid #f00;border-radius:6pt;padding:6pt">x</div>"#,
        );
        let rects = rect_decorations(&layout);
        // Rounded background fill (no stroke on it — borders are separate here).
        assert!(
            rects.iter().any(|(fill, stroke, radius, _)| fill.is_some()
                && stroke.is_none()
                && radius.iter().any(|r| *r > 0.0)),
            "rounded background fill present"
        );
        // At least one square red bottom border edge (radius zero).
        let red = [1.0, 0.0, 0.0];
        assert!(
            rects
                .iter()
                .any(|(fill, _, radius, _)| *fill == Some(red) && *radius == [0.0; 4]),
            "square red bottom-border edge present"
        );
    }

    #[test]
    fn clamp_radius_caps_oversized_corners() {
        // A 100pt radius on a 40×20 box scales down so adjacent radii fit the side.
        let r = clamp_radius([100.0, 100.0, 100.0, 100.0], 40.0, 20.0);
        // Limiting side is the 20pt one: tl+tr ≤ 40 and tr+br ≤ 20 ⇒ each ≤ 10.
        assert!(r.iter().all(|v| *v <= 10.0 + 1e-6), "clamped to ≤10: {r:?}");
        assert!(r.iter().all(|v| *v > 0.0), "still positive: {r:?}");
        // A modest radius that already fits is untouched.
        assert_eq!(clamp_radius([4.0, 4.0, 4.0, 4.0], 100.0, 100.0), [4.0; 4]);
        // Degenerate box ⇒ zeros.
        assert_eq!(clamp_radius([5.0; 4], 0.0, 10.0), [0.0; 4]);
        // Negatives floored.
        assert_eq!(clamp_radius([-3.0, 0.0, 0.0, 0.0], 100.0, 100.0), [0.0; 4]);
    }

    // ── styled borders (dashed/dotted/double) / linear-gradient ──────────────

    /// Page-0 `Fragment::Border` sides as `(horizontal, style)`.
    fn border_sides(layout: &Layout) -> Vec<(bool, BorderStyle)> {
        layout
            .pages
            .iter()
            .flatten()
            .filter_map(|f| match f {
                Fragment::Border {
                    horizontal, style, ..
                } => Some((*horizontal, *style)),
                _ => None,
            })
            .collect()
    }

    /// Count page-0 `Fragment::Gradient`s.
    fn gradient_count(layout: &Layout) -> usize {
        layout
            .pages
            .iter()
            .flatten()
            .filter(|f| matches!(f, Fragment::Gradient { .. }))
            .count()
    }

    #[test]
    fn solid_border_stays_plain_rects_no_border_fragment() {
        // Guard: an all-solid border emits NO styled `Border` fragments — it stays
        // the legacy filled-rect path, byte-identical to before.
        let layout =
            run(r#"<div style="border:2pt solid #000000;width:100pt;height:40pt"></div>"#);
        assert!(
            border_sides(&layout).is_empty(),
            "a solid border emits no styled Border fragments"
        );
    }

    #[test]
    fn dashed_border_emits_styled_border_fragments() {
        // A dashed border on all sides ⇒ four `Border` bands (the painter splits
        // each into dash segments).
        let layout =
            run(r#"<div style="border:2pt dashed #000000;width:100pt;height:40pt"></div>"#);
        let sides = border_sides(&layout);
        assert_eq!(sides.len(), 4, "one Border band per dashed side: {sides:?}");
        assert!(
            sides.iter().all(|(_, s)| *s == BorderStyle::Dashed),
            "every band is dashed"
        );
        // Top/bottom are horizontal; left/right vertical.
        assert_eq!(sides.iter().filter(|(h, _)| *h).count(), 2, "2 horizontal");
        assert_eq!(sides.iter().filter(|(h, _)| !*h).count(), 2, "2 vertical");
    }

    #[test]
    fn one_dotted_side_emits_one_border_band() {
        let layout = run(
            r#"<div style="border-bottom:3pt dotted #000000;width:90pt;height:30pt"></div>"#,
        );
        let sides = border_sides(&layout);
        assert_eq!(sides.len(), 1, "exactly one styled side");
        assert_eq!(sides[0], (true, BorderStyle::Dotted), "horizontal dotted");
    }

    #[test]
    fn linear_gradient_background_emits_a_gradient_fragment() {
        let layout = run(
            r#"<div style="background:linear-gradient(90deg,#ff0000,#0000ff);width:100pt;height:40pt"></div>"#,
        );
        assert_eq!(
            gradient_count(&layout),
            1,
            "a gradient background emits one Gradient fragment"
        );
        // A pure gradient sets no solid background rect.
        let solid_fills = rect_decorations(&layout)
            .into_iter()
            .filter(|(fill, ..)| fill.is_some())
            .count();
        assert_eq!(solid_fills, 0, "no solid fill rect for a gradient-only box");
    }

    #[test]
    fn gradient_over_solid_color_emits_both() {
        // `background-color` fallback + a gradient overlay ⇒ a solid rect AND a
        // gradient fragment (CSS layers the image above the colour).
        let layout = run(
            r#"<div style="background-color:#112233;background-image:linear-gradient(red,blue);width:80pt;height:30pt"></div>"#,
        );
        assert_eq!(gradient_count(&layout), 1, "one gradient overlay");
        assert!(
            rect_decorations(&layout)
                .iter()
                .any(|(fill, ..)| *fill == Some([17.0 / 255.0, 34.0 / 255.0, 51.0 / 255.0])),
            "the solid colour fallback rect is still present"
        );
    }

    #[test]
    fn radial_gradient_background_emits_a_gradient_fragment() {
        let layout = run(
            r#"<div style="background:radial-gradient(#ff0000,#0000ff);width:100pt;height:40pt"></div>"#,
        );
        assert_eq!(gradient_count(&layout), 1, "radial emits one Gradient");
    }

    #[test]
    fn conic_gradient_background_emits_a_gradient_fragment() {
        let layout = run(
            r#"<div style="background:conic-gradient(from 0deg,#ff0000,#00ff00,#0000ff);width:60pt;height:60pt"></div>"#,
        );
        assert_eq!(gradient_count(&layout), 1, "conic emits one Gradient");
    }

    // ── elliptical border-radius / multi-layer box-shadow / sticky ───────────

    /// Page-0 `Fragment::Rect` decorations including the vertical radii and
    /// whether a shadow rides on each: `(fill, radius_h, radius_v, shadow)`.
    #[allow(clippy::type_complexity)]
    fn rect_radii(layout: &Layout) -> Vec<(Option<[f64; 3]>, [f64; 4], [f64; 4], bool)> {
        layout
            .pages
            .iter()
            .flatten()
            .filter_map(|f| match f {
                Fragment::Rect {
                    fill,
                    radius,
                    radius_v,
                    shadow,
                    ..
                } => Some((*fill, *radius, *radius_v, shadow.is_some())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn elliptical_radius_reaches_the_background_rect() {
        // `border-radius: 12pt / 4pt` ⇒ the decoration rect carries distinct
        // horizontal (12) and vertical (4) radii.
        let layout = run(
            r#"<div style="background:#3366cc;border-radius:12pt / 4pt;padding:6pt">x</div>"#,
        );
        let bg = rect_radii(&layout)
            .into_iter()
            .find(|(fill, ..)| *fill == Some([0.2, 0.4, 0.8]))
            .expect("the blue rounded rect");
        assert!(bg.1.iter().all(|r| (*r - 12.0).abs() < 0.01), "h=12: {:?}", bg.1);
        assert!(bg.2.iter().all(|r| (*r - 4.0).abs() < 0.01), "v=4: {:?}", bg.2);
    }

    #[test]
    fn circular_radius_keeps_h_equal_v_on_the_rect() {
        // Guard: a plain circular radius keeps `radius == radius_v` (the painter's
        // byte-identical circular path).
        let layout =
            run(r#"<div style="background:#3366cc;border-radius:8pt;padding:6pt">x</div>"#);
        let bg = rect_radii(&layout)
            .into_iter()
            .find(|(fill, ..)| *fill == Some([0.2, 0.4, 0.8]))
            .expect("the blue rounded rect");
        assert_eq!(bg.1, bg.2, "circular corners: h == v");
        assert!(bg.1.iter().all(|r| (*r - 8.0).abs() < 0.01));
    }

    #[test]
    fn multi_layer_box_shadow_emits_one_extra_shadow_rect() {
        // Two shadow layers ⇒ the topmost rides on the background rect AND one
        // extra shadow-only rect (fill-less, shadow present) is emitted behind it.
        let layout = run(
            r#"<div style="background:#eeeeee;box-shadow:1pt 1pt 2pt #000000, 5pt 5pt 8pt #888888;padding:8pt">x</div>"#,
        );
        let rects = rect_radii(&layout);
        // Background rect with the topmost shadow.
        assert!(
            rects.iter().any(|(fill, _, _, shadow)| fill.is_some() && *shadow),
            "background rect carries the topmost shadow"
        );
        // Exactly one extra shadow-only rect (no fill, shadow present).
        let extra = rects
            .iter()
            .filter(|(fill, _, _, shadow)| fill.is_none() && *shadow)
            .count();
        assert_eq!(extra, 1, "one extra shadow-only rect for the 2nd layer");
    }

    #[test]
    fn single_box_shadow_emits_no_extra_shadow_rect() {
        // Guard: a single shadow rides on the background rect with NO extra
        // shadow-only rect — the common path is unchanged.
        let layout = run(
            r#"<div style="background:#eeeeee;box-shadow:3pt 3pt 4pt #000000;padding:8pt">x</div>"#,
        );
        let extra = rect_radii(&layout)
            .iter()
            .filter(|(fill, _, _, shadow)| fill.is_none() && *shadow)
            .count();
        assert_eq!(extra, 0, "no extra shadow-only rect for a single shadow");
    }

    #[test]
    fn sticky_offset_is_clamped_to_the_containing_block() {
        // A sticky child asked to shift far down is clamped so it can't leave its
        // parent's content box. The parent is a fixed-height container; the child
        // requests `top: 9999pt` (way past the container). After clamping the
        // child's text must remain within the parent's content band, i.e. its
        // top stays ≤ the parent's content bottom.
        let html = r#"<div style="height:60pt;padding:0">
            <p style="position:sticky;top:9999pt;margin:0">sticky</p>
        </div>"#;
        let layout = run(html);
        let sticky_y = text_xy(&layout)
            .into_iter()
            .find(|(_, _, s)| s == "sticky")
            .map(|(_, y, _)| y)
            .expect("the sticky run");
        // The container starts at the top margin (36) and is 60pt tall, so its
        // content bottom is ≈ 96. The clamp must keep the run inside that band —
        // it certainly must NOT have flown ~9999pt down.
        assert!(
            sticky_y < 100.0,
            "sticky shift clamped within the container (y={sticky_y}, not ~9999)"
        );
        assert!(sticky_y >= 36.0, "still within/after the container top (y={sticky_y})");
    }

    #[test]
    fn sticky_with_room_applies_the_offset() {
        // With slack inside the container, a modest sticky offset DOES move the
        // box (it behaves like relative until it hits the container edge).
        let base = run(
            r#"<div style="height:200pt"><p style="margin:0">x</p></div>"#,
        );
        let shifted = run(
            r#"<div style="height:200pt"><p style="position:sticky;top:20pt;margin:0">x</p></div>"#,
        );
        let y0 = text_xy(&base).into_iter().find(|(_, _, s)| s == "x").unwrap().1;
        let y1 = text_xy(&shifted).into_iter().find(|(_, _, s)| s == "x").unwrap().1;
        assert!(
            (y1 - y0 - 20.0).abs() < 0.5,
            "sticky with room shifts by ~20pt (y0={y0}, y1={y1})"
        );
    }
}
