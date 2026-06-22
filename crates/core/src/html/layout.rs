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
    Align, AlignItems, Display, FloatSide, Justify, Len, Position, Style, Stylesheet, VAlign,
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
        self.boxes
            .iter()
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
        let mut i = start;
        while i < self.out.len() {
            if fragment_outside(&self.out[i].frag, rx0, ry0, rx1, ry1) {
                self.out.remove(i);
            } else {
                i += 1;
            }
        }
    }
}

/// Translate one fragment by `(dx, dy)` in place.
fn shift_fragment(frag: &mut Fragment, dx: f64, dy: f64) {
    match frag {
        Fragment::Text { x, y, .. }
        | Fragment::Rect { x, y, .. }
        | Fragment::Image { x, y, .. }
        | Fragment::Svg { x, y, .. } => {
            *x += dx;
            *y += dy;
        }
    }
}

/// True if a fragment's bounding box lies entirely outside `[x0,x1)×[y0,y1)`.
/// Text height is approximated from its font size.
fn fragment_outside(frag: &Fragment, x0: f64, y0: f64, x1: f64, y1: f64) -> bool {
    let (fx0, fy0, fx1, fy1) = match frag {
        Fragment::Text { x, y, style, .. } => (*x, *y, *x, *y + style.font_size),
        Fragment::Rect { x, y, w, h, .. }
        | Fragment::Image { x, y, w, h, .. }
        | Fragment::Svg { x, y, w, h, .. } => (*x, *y, *x + *w, *y + *h),
    };
    // No overlap with the clip rect on either axis ⇒ fully outside.
    fx1 < x0 || fx0 > x1 || fy1 < y0 || fy0 > y1
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
                }
                if let Node::Element(e) = child {
                    let st = self.style_of(e, parent_style, ancestors);
                    if st.display == Display::ListItem {
                        list_index += 1;
                    }
                    if st.page_break_before {
                        y = self.break_to_next_page(y);
                    }
                    // `position: relative` lays out in flow, then shifts its
                    // fragments by `inset` (its normal space is preserved).
                    let start = self.out.len();
                    y = self.block(e, &st, parent_style, x, avail_w, y, ancestors, list_index);
                    if st.position == Position::Relative {
                        let (dx, dy) = self.relative_offset(&st, avail_w);
                        self.translate_range(start, dx, dy);
                    }
                    if st.z_index != 0 {
                        self.stamp_z(start, st.z_index);
                    }
                    if st.page_break_after {
                        y = self.break_to_next_page(y);
                    }
                }
            } else {
                inline_run.push(child);
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
        self.flow_lines_floated(&items, x, avail_w, y, style.align, style.text_indent, &floats)
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
        if el.tag == "table" {
            return self.table(el, style, x, avail_w, y, ancestors);
        }
        if style.display == Display::Flex {
            return self.flex(el, style, x, avail_w, y, ancestors);
        }
        if style.display == Display::Grid {
            return self.grid(el, style, x, avail_w, y, ancestors);
        }

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
                h: style.min_height.unwrap_or(self.cb.h),
            };
        }
        let children_start = self.out.len();
        cy = self.block_children(
            &el.children,
            style,
            content_x,
            content_w,
            cy,
            &new_ancestors,
        );

        cy += p.bottom + b.bottom;
        let mut box_h = (cy - box_top).max(0.1);
        if let Some(mh) = style.min_height {
            box_h = box_h.max(mh); // `height` / `min-height`
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
        // suppresses the paint but the box still occupies its space. The
        // background is a fill-only rect; borders are drawn per-side so
        // `border-bottom`/`border-left` (etc.) keep their own width and colour.
        let any_border = b.top + b.bottom + b.left + b.right > 0.0;
        if !style.hidden && (style.background.is_some() || any_border) {
            if style.background.is_some() {
                self.out.push(Abs {
                    z: 0,
                    zi: 0,
                    frag: Fragment::Rect {
                        x: box_x,
                        y: box_top,
                        w: box_w,
                        h: box_h,
                        fill: style.background,
                        stroke: None,
                        stroke_w: 0.0,
                        opacity: style.opacity,
                    },
                });
            }
            if any_border {
                self.emit_border_edges(
                    box_x,
                    box_top,
                    box_w,
                    box_h,
                    [b.top, b.right, b.bottom, b.left],
                    &style.border_color_edges,
                    style.opacity,
                );
            }
        }

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
        self.flow_lines(&items, x, avail_w, y, style.align, style.text_indent)
    }

    fn collect_inline(
        &mut self,
        node: &Node,
        parent_style: &Style,
        ancestors: &[&Element],
        out: &mut Vec<InlineItem>,
    ) {
        match node {
            Node::Text(t) => out.push(InlineItem {
                text: parent_style.text_transform.apply(t),
                style: parent_style.clone(),
                media: None,
            }),
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
    ) {
        if line.is_empty() {
            *y += default_line_height(&Style::default());
            return;
        }
        let line_h = line
            .iter()
            .map(|w| w.style.font_size * w.style.line_height.max(1.0))
            .fold(0.0_f64, f64::max);
        let extra = (line_avail - line_w).max(0.0);
        let (mut cx, gap_extra) = match align {
            Align::Left => (line_x, 0.0),
            Align::Right => (line_x + extra, 0.0),
            Align::Center => (line_x + extra / 2.0, 0.0),
            Align::Justify => {
                let gaps = line.iter().filter(|w| w.space_after).count().max(1);
                (line_x, if last { 0.0 } else { extra / gaps as f64 })
            }
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
                    // `vertical-align: super|sub` raises/lowers the run's baseline
                    // within the line (negative = up). Width/advance are unchanged.
                    self.out.push(Abs {
                        z: 1,
                        zi: 0,
                        frag: Fragment::Text {
                            x: cx,
                            y: *y + w.style.valign_shift,
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

    /// Break inline items into lines and emit positioned text/images.
    /// `indent` (`text-indent`) shifts and shortens the first line only.
    fn flow_lines(
        &mut self,
        items: &[InlineItem],
        x: f64,
        avail_w: f64,
        mut y: f64,
        align: Align,
        indent: f64,
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
                self.emit_line(&line, line_w, &mut y, true, line_x, line_avail, align, space_w);
                line.clear();
                line_w = 0.0;
                first_line = false;
                i += 1;
                continue;
            }
            let add = w.w + if line.is_empty() { 0.0 } else { space_w };
            if !line.is_empty() && line_w + add > line_avail {
                self.emit_line(&line, line_w, &mut y, false, line_x, line_avail, align, space_w);
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
        self.emit_line(&line, line_w, &mut y, true, line_x, line_avail, align, space_w);
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
                self.emit_line(&line, line_w, &mut y, true, line_x, line_avail, align, space_w);
                line.clear();
                line_w = 0.0;
                first_line = false;
                i += 1;
                continue;
            }
            let add = w.w + if line.is_empty() { 0.0 } else { space_w };
            if !line.is_empty() && line_w + add > line_avail {
                self.emit_line(&line, line_w, &mut y, false, line_x, line_avail, align, space_w);
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
        self.emit_line(&line, line_w, &mut y, true, line_x, line_avail, align, space_w);
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
                    },
                });
            }

            // Per-side borders. In collapse mode draw top + left always, bottom
            // only when the cell reaches the last row, right only when it reaches
            // the last column — so interior edges (shared with the next cell down
            // / right) are drawn exactly once. Separate mode draws all four.
            let bw = &pl.border_width;
            let bc = &pl.border_color_edges;
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
            self.emit_border_edges(cell_x, top, cw, cell_h, sides, bc, pl.opacity);
        }

        acc_y + style.margin.bottom
    }

    /// Emit up to four per-side border rules for the box `(x, y, w, h)` as thin
    /// filled rectangles (top-down). `widths`/`colors` are `[top, right, bottom,
    /// left]`. Filled rects (rather than a single stroked rect) give exact
    /// per-side placement, width and colour — the only way to honour
    /// `border-bottom: 2pt` without also thickening the other three sides.
    #[allow(clippy::too_many_arguments)]
    fn emit_border_edges(
        &mut self,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        widths: [f64; 4],
        colors: &[[f64; 3]; 4],
        opacity: f64,
    ) {
        // (rect x, y, w, h) for each present side; corners overlap so the frame
        // joins cleanly (acceptable: same colour, opaque overlap).
        let push = |out: &mut Vec<Abs>, rx: f64, ry: f64, rw: f64, rh: f64, c: [f64; 3]| {
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
                },
            });
        };
        let [wt, wr, wb, wl] = widths;
        if wt > 0.0 {
            push(&mut self.out, x, y, w, wt, colors[0]); // top
        }
        if wb > 0.0 {
            push(&mut self.out, x, y + h - wb, w, wb, colors[2]); // bottom
        }
        if wl > 0.0 {
            push(&mut self.out, x, y, wl, h, colors[3]); // left
        }
        if wr > 0.0 {
            push(&mut self.out, x + w - wr, y, wr, h, colors[1]); // right
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
        let any_explicit = items.iter().any(|it| self.style_of(it, style, na).width.is_some());
        if !style.flex_wrap || !any_explicit {
            return vec![items.to_vec()];
        }
        let gap = style.gap_col;
        let mut lines: Vec<Vec<&Element>> = Vec::new();
        let mut cur: Vec<&Element> = Vec::new();
        let mut used = 0.0;
        for it in items {
            let st = self.style_of(it, style, na);
            let w = match st.width {
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

        let mut ws: Vec<f64> = Vec::with_capacity(n);
        let mut grows: Vec<f64> = Vec::with_capacity(n);
        let mut any_explicit = false;
        for it in items {
            let st = self.style_of(it, style, na);
            grows.push(st.flex_grow.max(0.0));
            match st.width {
                Some(Len::Pt(w)) => {
                    ws.push(w.max(0.0));
                    any_explicit = true;
                }
                Some(Len::Percent(pc)) => {
                    ws.push(avail * pc / 100.0);
                    any_explicit = true;
                }
                None => ws.push(f64::NAN), // resolved below
            }
        }
        let total_grow: f64 = grows.iter().sum();

        let (offset, extra_gap) = if any_explicit {
            // Items with no explicit width share the leftover equally as basis.
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
            let mut free = (avail - ws.iter().sum::<f64>()).max(0.0);
            if total_grow > 0.0 {
                for (w, g) in ws.iter_mut().zip(&grows) {
                    *w += free * g / total_grow;
                }
                free = 0.0;
            }
            justify_offsets(style.justify, free, n)
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
        if let Some(h) = style.min_height {
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

    /// A grid with a fixed column count (`grid-template-columns`). Children fill
    /// equal-width cells left-to-right, wrapping every `cols` items; each row's
    /// height is its tallest cell. No spanning or named lines.
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
        let col_w = content_w / cols as f64;
        let mut y_cursor = y + b.top + p.top;

        for row in items.chunks(cols) {
            let row_top = y_cursor;
            let mut row_bottom = row_top;
            for (c, it) in row.iter().enumerate() {
                let istyle = self.style_of(it, style, &na);
                let nca = push_ancestor(&na, it);
                let ip = &istyle.padding;
                let ib = &istyle.border_width;
                let cx = content_x + c as f64 * col_w;
                let cy = self.block_children(
                    &it.children,
                    &istyle,
                    cx + ip.left + ib.left,
                    (col_w - ip.left - ip.right - ib.left - ib.right).max(1.0),
                    row_top + ip.top + ib.top,
                    &nca,
                );
                row_bottom = row_bottom.max(cy + ip.bottom + ib.bottom);
            }
            for (c, it) in row.iter().enumerate() {
                let istyle = self.style_of(it, style, &na);
                self.paint_item_box(
                    &istyle,
                    content_x + c as f64 * col_w,
                    row_top,
                    col_w,
                    row_bottom - row_top,
                );
            }
            y_cursor = row_bottom;
        }
        y_cursor + p.bottom + b.bottom + style.margin.bottom
    }

    /// Paint a flex/grid item's background + border as a single rect spanning
    /// its cell (z=0, behind the item's own content).
    fn paint_item_box(&mut self, istyle: &Style, x: f64, y: f64, w: f64, h: f64) {
        let has_border = istyle.border_width.top > 0.0;
        if istyle.background.is_some() || has_border {
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
                },
            });
        }
    }

    fn style_of(&self, el: &Element, parent: &Style, ancestors: &[&Element]) -> Style {
        self.sheet.computed(el, parent, ancestors)
    }
}

fn default_line_height(style: &Style) -> f64 {
    style.font_size * style.line_height.max(1.0)
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
            Fragment::Text { x, y, style, text } => {
                let p = page_of(y);
                ensure(&mut pages, p);
                pages[p].push(Fragment::Text {
                    x,
                    y: local_y(y, p),
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
            } => {
                // Split the rect across the page bands it covers.
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
                    });
                    top = seg_bottom + 0.001;
                }
            }
        }
    }
    if pages.is_empty() {
        pages.push(Vec::new());
    }
    pages
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
        let layout = run(
            "<table>\
             <tr><td rowspan=\"2\">Span</td><td>B0</td></tr>\
             <tr><td>C1</td></tr></table>",
        );
        let span = cell_x(&layout, "Span");
        let b0 = cell_x(&layout, "B0");
        let c1 = cell_x(&layout, "C1");
        // Spanning cell anchors column 0 (left edge).
        assert!((span - CELL_INSET).abs() < 1.0, "rowspan cell at col 0 ({span})");
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
        let layout = run(
            "<table>\
             <tr><td rowspan=\"2\">A</td><td>B0</td></tr>\
             <tr><td>C1</td><td>D1</td></tr></table>",
        );
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
        let layout = run(
            "<table>\
             <tr><td rowspan=\"2\" style=\"background:#cccccc\">S</td><td>B0</td></tr>\
             <tr><td>C1</td></tr></table>",
        );
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
        assert!(one_row > 1.0, "the two rows are vertically separated ({one_row})");
        // The spanning rect must be taller than a single row (it covers two).
        assert!(
            sh > one_row + 1.0,
            "spanning cell rect ({sh}) is taller than one row ({one_row})"
        );
        // And it starts at (or above) row 0's content baseline area.
        assert!(sy <= b0_y, "spanning rect starts at/above row 0 ({sy} vs {b0_y})");
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
        let short = run(
            "<table>\
             <tr><td rowspan=\"2\">x</td><td>B0</td></tr>\
             <tr><td>C1</td></tr>\
             <tr><td>R2L</td><td>R2R</td></tr></table>",
        );
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
        let layout = run(
            r#"<table><tr><th style="background:#cccccc">Head</th></tr></table>"#,
        );
        let page = &layout.pages[0];
        let grey = [0.8, 0.8, 0.8];
        let bg_idx = page.iter().position(|f| {
            matches!(f, Fragment::Rect { fill: Some(c), .. } if *c == grey)
        });
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
        let layout = run(
            "<table style=\"border-collapse:collapse\">\
               <tr><td>a</td><td>b</td></tr>\
               <tr><td>c</td><td>d</td></tr></table>",
        );
        // Equal 2-col grid over avail 540 ⇒ boundary at 36 + 270 = 306.
        let boundary = 36.0 + 270.0;
        let verticals: Vec<_> = rects(&layout)
            .into_iter()
            // thin vertical rules straddling the interior boundary
            .filter(|(rx, _, rw, rh, fill)| {
                fill.is_some()
                    && *rw < 4.0
                    && *rh > 4.0
                    && (*rx - boundary).abs() < 2.5
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
        let find = |t: &str| {
            xy.iter()
                .find(|(_, _, s)| s == t)
                .map(|(x, y, _)| (*x, *y))
        };
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
        assert!(bul_x > one_x, "nested bullet indented (bul={bul_x}, top={one_x})");
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
        let layout = run(
            r#"<p><span>base</span><span style="vertical-align:5px">up</span></p>"#,
        );
        let (_, base_y) = run_metrics(&layout, "base").expect("base run");
        let (_, up_y) = run_metrics(&layout, "up").expect("raised run");
        assert!(up_y < base_y, "explicit length raised the run ({up_y} < {base_y})");
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
}
