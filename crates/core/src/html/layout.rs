//! Box-tree layout: turn a styled DOM into positioned fragments across pages.
//!
//! Implements a real (if pragmatic) CSS visual formatting model: the block
//! formatting context stacks block boxes vertically honouring the box model
//! (margin / border / padding / background), and the inline formatting context
//! flows text + inline boxes into line boxes, breaking lines using **actual font
//! metrics** supplied by [`Measure`] (the paint layer plugs in embedded Google
//! fonts). Lists get markers, tables lay cells side-by-side, and the whole flow
//! is sliced into pages with backgrounds/borders split across page bands.

use super::bidi;
use super::css::{
    Align, AlignItems, BorderStyle, Clear, CssGradient, Direction, Display, FloatSide, Justify,
    Len, ObjectFit, Position, Style, Stylesheet, TrackSize, VAlign, VShift, WhiteSpace,
    WritingMode,
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
        /// `object-fit` — how the decoded pixels map into the `w×h` box. The
        /// painter (which knows the intrinsic size once the bytes are decoded)
        /// letterboxes (`contain`), scales-and-clips (`cover`), or centres
        /// (`none`/`scale-down`) accordingly; `Fill` (the default) stretches
        /// exactly as before.
        fit: ObjectFit,
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
        /// Outer `border-radius` of the corner adjoining the band's **start**
        /// (its top/left end), `0` for a square corner. When non-zero the band
        /// has already been shortened by this amount and the painter continues
        /// the dash/dot/double pattern along the corner's quarter-arc half —
        /// so a rounded dashed border follows the curve instead of overshooting
        /// it with square bars. All-zero radii reproduce the previous output
        /// byte-for-byte.
        radius_start: f64,
        /// Outer corner radius adjoining the band's **end** (bottom/right end).
        radius_end: f64,
        /// `true` when the side sits at the box's far edge (bottom side of a
        /// horizontal band, right side of a vertical one) — orients which way
        /// the corner arcs curve.
        far: bool,
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
    /// `inner` clipped to `rect` (`[x, y, w, h]`, top-down) — the painter wraps it
    /// in `q … W n … Q`, so content straddling an `overflow: hidden|clip` box edge
    /// is actually cut (ISO 32000-1 §8.5.4). `radius`/`radius_v` are the box's
    /// corner radii (`[tl, tr, br, bl]`, all `0` for a plain rectangular clip): a
    /// non-zero corner makes the painter clip to the **rounded** contour. Nesting
    /// (`Clipped` inside `Clipped`) intersects the clips.
    Clipped {
        rect: [f64; 4],
        radius: [f64; 4],
        radius_v: [f64; 4],
        inner: Box<Fragment>,
    },
    /// `inner` painted under a CSS `transform`: `matrix` is the affine
    /// `[a, b, c, d, e, f]` in **CSS (top-down) user space**, with the
    /// transform-origin already folded in. The painter conjugates it with the
    /// page Y-flip to a PDF-space `cm` and wraps the inner in `q … cm … Q`.
    /// Nesting composes (outer × inner).
    Transformed {
        matrix: [f64; 6],
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

/// One level of a fragment's stacking-context path: `(band, z-index)`.
///
/// CSS 2.1 paint order **within** a stacking context, encoded as bands so a
/// plain tuple comparison yields the right order:
/// * [`BAND_OWN_DECOR`] — the context element's own background/borders;
/// * [`BAND_NEG`] — child stacking contexts with a negative `z-index`
///   (ascending), painted above the context's own background but under its
///   normal flow;
/// * [`BAND_FLOW`] — normal-flow content (the *implicit* terminal every plain
///   fragment gets at comparison time, see [`cmp_stack`]);
/// * [`BAND_POS`] — positioned content and child contexts with `z-index ≥ 0`
///   (ascending), painted above the flow.
///
/// A fragment's full key is the chain of such entries for every enclosing
/// stacking context (outermost first). Descendant `z-index`es therefore stay
/// **local to their context**: `z-index: 9999` inside a `z-index: 1` context
/// compares as `[(POS,1), (POS,9999)]` — under a sibling `[(POS,2)]` context
/// no matter how large the inner value is.
type StackLevel = (i8, i32);

/// Band: the stacking context's own background/borders (its "layer 1").
const BAND_OWN_DECOR: i8 = 0;
/// Band: child stacking contexts with `z-index < 0`.
const BAND_NEG: i8 = 1;
/// Band: normal-flow content (implicit terminal of every path).
const BAND_FLOW: i8 = 2;
/// Band: positioned content / child contexts with `z-index ≥ 0`.
const BAND_POS: i8 = 3;

/// The implicit terminal a path is padded with when it runs out during
/// comparison — plain in-flow content of the enclosing context.
const STACK_FLOW: StackLevel = (BAND_FLOW, 0);

#[derive(Debug, Clone)]
struct Abs {
    /// z=0 backgrounds/borders, z=1 content (text/images) — paint order within
    /// a stacking level.
    z: u8,
    /// Stacking-context path (outermost context first; empty ⇒ root normal
    /// flow). Each stacking context wraps its finished subtree via
    /// [`Flow::stamp_context`], which *prepends* its own level — so inner
    /// contexts stamp first and end up deeper in the path.
    zk: Vec<StackLevel>,
    frag: Fragment,
}

/// Lexicographic comparison of two stacking paths, padding the exhausted side
/// with the implicit [`STACK_FLOW`] terminal: a context's plain content sits
/// between its negative and its non-negative child contexts, and a context
/// entry itself orders against its siblings' flow content the same way.
fn cmp_stack(a: &[StackLevel], b: &[StackLevel]) -> std::cmp::Ordering {
    let n = a.len().max(b.len());
    for i in 0..n {
        let av = a.get(i).copied().unwrap_or(STACK_FLOW);
        let bv = b.get(i).copied().unwrap_or(STACK_FLOW);
        match av.cmp(&bv) {
            std::cmp::Ordering::Equal => continue,
            other => return other,
        }
    }
    std::cmp::Ordering::Equal
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
    // `vw`/`vh` lengths resolve against THIS page's real size for the whole
    // layout (styles are computed lazily during the walk); the guard restores
    // the reference viewport on exit. Synthetic frames (the "endless page"
    // used to measure header/footer bands) are filtered by the setter.
    let _viewport = super::css::set_viewport_for_layout(frame.page_w, frame.page_h);
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
        fixed_templates: Vec::new(),
        last_decor: None,
    };
    // Find <body> if present, else lay out the whole forest.
    let roots = find_body(nodes).unwrap_or(nodes);
    let root_style = Style {
        display: Display::Block,
        ..Style::default()
    };
    let mut y = frame.top;
    // The page (root) is the top-level block formatting context.
    y = flow.block_children(roots, &root_style, frame.left, content_w, y, &[], true);
    let _ = y;

    // `position: fixed` is viewport-attached: repeat each fixed subtree on every
    // page after the first (its template is already positioned in the page-1
    // band; shifting by whole page bands re-lands it at the same page-local
    // spot). The page count comes from the fragments actually produced.
    if !flow.fixed_templates.is_empty() {
        let content_h = page_cb.h;
        let max_bottom = flow
            .out
            .iter()
            .map(|a| fragment_bbox(&a.frag).3)
            .fold(frame.top, f64::max);
        let last_page = (((max_bottom - frame.top - 0.05).max(0.0)) / content_h) as usize;
        let templates = std::mem::take(&mut flow.fixed_templates);
        for p in 1..=last_page {
            for tpl in &templates {
                for abs in tpl {
                    let mut clone = abs.clone();
                    shift_fragment(&mut clone.frag, 0.0, p as f64 * content_h);
                    flow.out.push(clone);
                }
            }
        }
    }

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

/// Whether a block with style `st` establishes a **new block formatting
/// context** — i.e. its float context is isolated from the surrounding flow:
/// floats placed inside it neither escape to following siblings nor are its own
/// line boxes shortened by ancestor floats; it also fully contains its floats.
///
/// Per CSS this is the case for: a non-`visible` `overflow`, `display: flow-root`,
/// a float, an out-of-flow positioned box, and the formatting-root displays
/// (`table`/`flex`/`grid`, and a multi-column block) — the last group already
/// lays out through their own routines, but we still report them here so the
/// predicate is a single source of truth. As a pragmatic extension we also treat
/// any non-`static` positioned block as a BFC: `relative`/`sticky` shift their
/// fragments *after* layout, so isolating their internal floats avoids leaking
/// pre-shift float geometry into siblings.
fn establishes_bfc(st: &Style) -> bool {
    st.overflow_clip
        || st.establishes_bfc
        || st.float != FloatSide::None
        || st.position != Position::Static
        || st.column_count > 1
        || matches!(
            st.display,
            Display::Table | Display::TableRow | Display::Flex | Display::Grid
        )
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
    /// `position: fixed` subtrees, captured as already-positioned fragment
    /// templates (page-1 band coordinates). Fixed boxes are viewport-attached:
    /// once the document's page count is known, [`layout_document_framed`]
    /// re-stamps each template onto every page after the first (the same
    /// replay model as multi-page `position: sticky`).
    fixed_templates: Vec<Vec<Abs>>,
    /// The `self.out` range of the **most recent** `emit_box_decoration` call.
    /// A block emits its own decoration *after* its children, so right after
    /// `block(el)` returns this is `el`'s own background/border range — which
    /// [`Flow::stamp_context`] marks as the stacking context's "own layer"
    /// (painted before negative-z child contexts, per CSS 2.1 Appendix E).
    last_decor: Option<(usize, usize)>,
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

    /// Close a **stacking context** over the fragments at `self.out[start..]`:
    /// prepend `(band, z)` to every fragment's path so the whole subtree moves
    /// as one unit in the enclosing context while its descendants' own entries
    /// (already stamped — inner contexts finish first) stay *local* to it.
    ///
    /// `z` places the context in its parent: negative ⇒ under the parent's
    /// flow (but above the parent's own background), positive ⇒ above,
    /// `0` ⇒ in flow order (the `opacity`/`transform` trigger with `z-index:
    /// auto` — containment without lifting).
    ///
    /// The context element's **own** decoration (the most recent
    /// `emit_box_decoration` range — a block emits it after its children) is
    /// marked [`BAND_OWN_DECOR`] so it paints first inside the context,
    /// under negative-z children (CSS 2.1 Appendix E steps 1–2).
    fn stamp_context(&mut self, start: usize, z: i32) {
        let entry: StackLevel = match z.cmp(&0) {
            std::cmp::Ordering::Less => (BAND_NEG, z),
            std::cmp::Ordering::Equal => (BAND_FLOW, 0),
            std::cmp::Ordering::Greater => (BAND_POS, z),
        };
        if let Some((ds, de)) = self.last_decor {
            if ds >= start && de <= self.out.len() {
                for a in &mut self.out[ds..de] {
                    if a.zk.is_empty() {
                        a.zk.push((BAND_OWN_DECOR, 0));
                    }
                }
            }
        }
        for a in &mut self.out[start..] {
            a.zk.insert(0, entry);
        }
    }

    /// Drop fragments at `self.out[start..]` that fall entirely outside the clip
    /// `rect` (a pragmatic `overflow: hidden|clip` — whole fragments are culled
    /// rather than pixel-clipped, since the paint layer has no clip primitive).
    fn clip_range(&mut self, start: usize, rect: Cb, radius: [f64; 4], radius_v: [f64; 4]) {
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
                    radius,
                    radius_v,
                    inner: Box::new(inner),
                };
                i += 1;
            }
        }
    }

    /// Wrap every fragment at `self.out[start..]` in a [`Fragment::Transformed`]
    /// carrying `matrix` (a CSS-space affine with the transform-origin already
    /// folded in). Called after an element's box + children are emitted, so its
    /// whole subtree transforms together; nesting composes via the painter's
    /// `q … cm` stacking.
    fn transform_range(&mut self, start: usize, matrix: [f64; 6]) {
        for a in &mut self.out[start..] {
            let inner = std::mem::replace(&mut a.frag, Fragment::placeholder());
            a.frag = Fragment::Transformed {
                matrix,
                inner: Box::new(inner),
            };
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
        Fragment::Clipped { rect, inner, .. } => {
            // Move both the clip window and the clipped content together, so a
            // relatively-positioned subtree keeps its clip aligned (the corner
            // radii are size-relative, so they don't shift).
            rect[0] += dx;
            rect[1] += dy;
            shift_fragment(inner, dx, dy);
        }
        Fragment::Transformed { matrix, inner } => {
            // The inner moves by (dx, dy); conjugate the matrix by that same shift
            // (`M' = T(d)·M·T(-d)`) so the transform-origin moves with it and the
            // visual result is exactly the old one translated by (dx, dy).
            let [a, b, c, d, _, _] = *matrix;
            matrix[4] += dx - (a * dx + c * dy);
            matrix[5] += dy - (b * dx + d * dy);
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
        Fragment::Transformed { matrix, inner } => {
            // AABB of the inner box's four corners mapped through the matrix.
            let (x0, y0, x1, y1) = fragment_bbox(inner);
            let [a, b, c, d, e, f] = *matrix;
            let pts = [(x0, y0), (x1, y0), (x1, y1), (x0, y1)];
            let xs = pts.map(|(x, y)| a * x + c * y + e);
            let ys = pts.map(|(x, y)| b * x + d * y + f);
            let min = |v: [f64; 4]| v.iter().copied().fold(f64::INFINITY, f64::min);
            let max = |v: [f64; 4]| v.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            (min(xs), min(ys), max(xs), max(ys))
        }
    }
}

/// A darker shade of `c` for the shadowed side of a 3-D bevel border.
fn darken(c: [f64; 3]) -> [f64; 3] {
    c.map(|v| v * 0.55)
}

/// A lighter shade of `c` for the lit side of a 3-D bevel border.
fn lighten(c: [f64; 3]) -> [f64; 3] {
    c.map(|v| v + (1.0 - v) * 0.45)
}

/// The rightmost x reached by a fragment's bounding box (its `x + width`),
/// using `m` to measure the advance of a [`Fragment::Text`] run (which stores
/// only its origin `x`, not its width). Used to measure a float's intrinsic
/// (max-content) width: the widest right edge among the fragments produced when
/// the content is laid out at a very large available width.
fn fragment_right(frag: &Fragment, m: &dyn Measure) -> f64 {
    match frag {
        Fragment::Text { x, style, text, .. } => *x + m.width(text, style),
        Fragment::Rect { x, w, .. }
        | Fragment::Image { x, w, .. }
        | Fragment::Svg { x, w, .. }
        | Fragment::Border { x, w, .. }
        | Fragment::Gradient { x, w, .. } => *x + *w,
        // Decoration wrappers: the inner fragment carries the geometry. A clip
        // only cuts (it doesn't move content), and the float max-content path
        // never wraps its content in these — recurse for an exhaustive, sensible
        // bound. (`Transformed` ignores the affine here; transformed content is
        // not part of intrinsic float sizing.)
        Fragment::Clipped { inner, .. } | Fragment::Transformed { inner, .. } => {
            fragment_right(inner, m)
        }
    }
}

/// Whether `el` is a replaced element (`<img>` / `<svg>`): its box is sized from
/// an intrinsic width, not from laying out child content. A replaced float keeps
/// its intrinsic width rather than shrink-to-fit measuring (it has no text run
/// to measure).
fn is_replaced(el: &Element) -> bool {
    el.tag == "img" || el.tag == "svg"
}

/// The intrinsic width (points) of a replaced element's box: its `width`
/// attribute if present, else the conventional 64pt fallback used by the inline
/// replaced path. (Matches `collect_inline`'s default so a floated `<img>` is
/// sized exactly as the inline one.)
fn intrinsic_replaced_width(el: &Element) -> f64 {
    el.attr("width")
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|w| *w > 0.0)
        .unwrap_or(64.0)
}

/// An atomic inline item for line breaking.
/// Inline replaced content laid out as a box on the line: a raster image
/// (`w, h, src`) or a vector SVG (`w, h, image`).
#[derive(Clone)]
enum Media {
    Raster(f64, f64, String),
    /// An inline vector image: placed size, the parsed SVG, and its
    /// `preserveAspectRatio` (drives the viewBox→box mapping at placement).
    Svg(f64, f64, SvgImage, crate::svg::PreserveAspectRatio),
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
                units.push(FlowUnit::Block { el: e, list_index });
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
    #[allow(clippy::too_many_arguments)]
    fn block_children(
        &mut self,
        children: &[Node],
        parent_style: &Style,
        x: f64,
        avail_w: f64,
        mut y: f64,
        ancestors: &[&Element],
        // Whether the *container* whose children these are establishes a new
        // block formatting context. When `true`, floats are isolated to this
        // container (a fresh context, and the container clears past its floats at
        // the end). When `false` (a plain in-flow block container), floats are
        // shared with the parent context: floats placed here flow up to following
        // siblings, and the container does NOT clear past them.
        container_is_bfc: bool,
    ) -> f64 {
        // A BFC container starts a fresh float context (restored on exit) so its
        // floats don't leak; a non-BFC container shares the inherited one.
        let saved_floats = if container_is_bfc {
            std::mem::take(&mut self.floats)
        } else {
            FloatCtx::default()
        };

        // `position: sticky` boxes, replayed once this container's page span is
        // known (bottom of this fn). Each entry is (fragment template, start index
        // in `self.out`, natural top, target page-local Y of the box top when
        // stuck, sticks-to-bottom?, clamp dx, clamp dy). A single-page block uses
        // the static clamp; a multi-page block becomes a running header (`top`) or
        // footer (`bottom`) repeated at that edge on every page it spans (paged
        // output has no scroll).
        type StickyBox = (Vec<Abs>, usize, f64, f64, bool, f64, f64);
        let mut pending_stickies: Vec<StickyBox> = Vec::new();
        // The container's own content top (this fn's entry `y`), so a bottom-sticky
        // footer knows the first page of the block it must repeat across.
        let container_top = y;

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
                    // visible gap becomes `max(prev.bottom, this.top)`. The
                    // block's *effective* top margin includes any first-child
                    // margin that collapses through it. Skipped when clearance
                    // already moved the block down.
                    if !cleared {
                        if let Some(prev_bottom) = prev_block_bottom {
                            let eff_top =
                                st.margin.top.max(self.top_margin_escape(e, &st, ancestors));
                            y -= prev_bottom.min(eff_top).max(0.0);
                        }
                    }
                    if st.page_break_before {
                        y = self.break_to_next_page(y);
                    }
                    // `position: relative` lays out in flow, then shifts its
                    // fragments by `inset` (its normal space is preserved).
                    let mut start = self.out.len();
                    let y_before = y;
                    let child_bfc = establishes_bfc(&st);
                    y = self.block_in_flow(
                        e,
                        &st,
                        parent_style,
                        x,
                        avail_w,
                        y,
                        ancestors,
                        list_index,
                        child_bfc,
                    );
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
                                y = self.block_in_flow(
                                    e,
                                    &st,
                                    parent_style,
                                    x,
                                    avail_w,
                                    moved,
                                    ancestors,
                                    list_index,
                                    child_bfc,
                                );
                            }
                        }
                    }
                    if st.position == Position::Relative {
                        let (dx, dy) = self.relative_offset(&st, avail_w);
                        self.translate_range(start, dx, dy);
                    } else if st.position == Position::Sticky {
                        // Compute the static clamped offset (used as-is for a
                        // single-page block) and the stuck edge (used for a
                        // multi-page running header/footer). The choice needs the
                        // container's page span, so defer both to the replay below.
                        let (dx, dy) = self.relative_offset(&st, avail_w);
                        let (dx, dy) =
                            self.clamp_sticky_offset(dx, dy, x, y_before, avail_w, y - y_before);
                        let content_h = (self.page_h - self.top - self.bottom).max(1.0);
                        let height = y - y_before;
                        let resolve = |len: Len| len.resolve(content_h);
                        let stuck = if let Some(t) = st.inset[0] {
                            Some((self.top + resolve(t), false))
                        } else {
                            st.inset[2]
                                .map(|b| ((self.page_h - self.bottom) - resolve(b) - height, true))
                        };
                        match stuck {
                            Some((local_top, is_bottom)) => pending_stickies.push((
                                self.out[start..].to_vec(),
                                start,
                                y_before,
                                local_top,
                                is_bottom,
                                dx,
                                dy,
                            )),
                            // No top/bottom edge (e.g. a horizontal-only sticky):
                            // nothing to repeat, just apply the static clamp now.
                            None => self.translate_range(start, dx, dy),
                        }
                    }
                    // A stacking context forms on the classic triggers: an
                    // explicit `z-index`, `opacity < 1`, or a `transform`.
                    // `z-index: 0` here means "auto" (the two are not
                    // distinguished), so opacity/transform contexts stamp a
                    // flow-ordered entry — containment without lifting.
                    if st.z_index != 0
                        || st.own_opacity.is_some_and(|o| o < 1.0)
                        || st.transform.is_some()
                    {
                        self.stamp_context(start, st.z_index);
                    }
                    if st.page_break_after {
                        y = self.break_to_next_page(y);
                        prev_block_bottom = None;
                    } else {
                        // The block's effective trailing margin includes any
                        // last-child margin that collapsed through its bottom.
                        prev_block_bottom = Some(
                            st.margin
                                .bottom
                                .max(self.bottom_margin_escape(e, &st, ancestors)),
                        );
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
        // A BFC container fully contains its floats: clear past any that extend
        // below the in-flow content (matches `overflow`/clearfix). A non-BFC
        // container does NOT clear — the floats it placed overflow into the
        // parent's following siblings (their context is shared, restored below).
        // This runs BEFORE the sticky replay so the latter sees the final `y`.
        if container_is_bfc {
            y = y.max(self.floats.max_bottom());
        }

        // Replay `position: sticky` running headers: now that this containing
        // block's bottom (`y`) is known, a top-sticky box whose span crosses page
        // boundaries is re-emitted at `self.top + offset` on each page after the
        // one it naturally sits on, up to the container's last page. The in-flow
        // space stays reserved (the box already advanced `y`); only paint repeats.
        // (Independent of the float context — runs for BFC and non-BFC alike.)
        if !pending_stickies.is_empty() {
            let top = self.top;
            let content_h = (self.page_h - top - self.bottom).max(1.0);
            let page_at = |yy: f64| ((yy - top).max(0.0) / content_h) as usize;
            let cb_first = page_at(container_top);
            let cb_last = page_at(y);
            for (template, start, natural_top, local_top, is_bottom, dx, dy) in &pending_stickies {
                let n = (*start + template.len()).min(self.out.len());
                if cb_first == cb_last {
                    // Single-page containing block: the static clamped offset
                    // (byte-identical to the pre-running-header behaviour).
                    for frag in self.out[*start..n].iter_mut() {
                        shift_fragment(&mut frag.frag, *dx, *dy);
                    }
                    continue;
                }
                // Multi-page block ⇒ running header/footer. Place the box at its
                // stuck edge on every page the block spans: a `top` box on its
                // natural page and each one AFTER, a `bottom` (footer) box on its
                // natural page and each one BEFORE.
                let np = page_at(*natural_top);
                let (lo, hi) = if *is_bottom {
                    (cb_first, np)
                } else {
                    (np, cb_last)
                };
                for p in lo..=hi {
                    let shift = (*local_top + p as f64 * content_h) - *natural_top;
                    if p == np {
                        // Reposition the in-flow fragments onto the stuck edge.
                        for frag in self.out[*start..n].iter_mut() {
                            shift_fragment(&mut frag.frag, 0.0, shift);
                        }
                    } else {
                        for abs in template {
                            let mut clone = abs.clone();
                            shift_fragment(&mut clone.frag, 0.0, shift);
                            self.out.push(clone);
                        }
                    }
                }
            }
        }

        // Restore the parent's float context for a BFC container. A non-BFC
        // container leaves `self.floats` as-is so the floats it placed remain
        // visible to the parent's following siblings.
        if container_is_bfc {
            self.floats = saved_floats;
        }
        y
    }

    /// Lay out one in-flow block child, narrowing its box beside any float that
    /// overlaps its top so the block's background/border/content sit *beside* the
    /// float (then full width below it, since the float's band ends there).
    ///
    /// `child_bfc` is [`establishes_bfc`] for the child. The interaction with
    /// floats differs by case, and both avoid shifting text twice:
    /// - **BFC child** (`overflow`, `flow-root`, positioned, table/flex/grid…):
    ///   `block` lays its children in an isolated float context, so the ancestor
    ///   floats never reach its line boxes — narrowing the box alone is enough.
    /// - **non-BFC child** (a plain in-flow block): its `block_children` *shares*
    ///   `self.floats`, so its lines would re-subtract the same float inset
    ///   (double-narrowing). We suppress that by removing the ancestor floats for
    ///   the duration of the child's layout, then restore them and re-append any
    ///   floats the child itself placed (so those still flow to later siblings).
    #[allow(clippy::too_many_arguments)]
    fn block_in_flow(
        &mut self,
        e: &Element,
        st: &Style,
        parent_style: &Style,
        x: f64,
        avail_w: f64,
        y: f64,
        ancestors: &[&Element],
        list_index: usize,
        child_bfc: bool,
    ) -> f64 {
        // Inset from floats overlapping the block's top edge. A 1pt probe band
        // matches the inline line-level model (a line touching the float's band
        // is affected); the box is narrowed by however much float sits at `y`.
        let (l_in, r_in) = self.floats.insets(y, 1.0);
        let bx = x + l_in;
        let bw = (avail_w - l_in - r_in).max(1.0);

        if child_bfc || (l_in == 0.0 && r_in == 0.0) {
            // BFC child (self-isolating) or no float to wrap beside: lay out
            // normally at the (possibly narrowed) geometry; nothing to suppress.
            return self.block(e, st, parent_style, bx, bw, y, ancestors, list_index);
        }

        // Non-BFC child beside a float: lift the ancestor floats so the child's
        // shared-context lines aren't narrowed again, lay it out, then restore.
        let suppressed = std::mem::take(&mut self.floats);
        let out_y = self.block(e, st, parent_style, bx, bw, y, ancestors, list_index);
        // Any floats the child placed (it shares the context, so they landed in
        // `self.floats`) must survive into the parent's following siblings.
        let inner = std::mem::take(&mut self.floats);
        self.floats = suppressed;
        self.floats.boxes.extend(inner.boxes);
        out_y
    }

    /// `position: relative` offset in points from `top`/`left` (falling back to
    /// the negated `bottom`/`right` when only those are set), resolved against
    /// the containing width/height.
    fn relative_offset(&self, st: &Style, avail_w: f64) -> (f64, f64) {
        let resolve = |len: Len, base: f64| len.resolve(base);
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
        let resolve = |len: Len, base: f64| len.resolve(base);
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
        self.stamp_context(start, if st.z_index != 0 { st.z_index } else { 1 });

        // A fixed box repeats on EVERY page (it is attached to the viewport, and
        // paged output has no scroll). Capture the positioned fragments as a
        // template; the document-level pass clones it onto each later page once
        // the page count is known.
        if st.position == Position::Fixed {
            let template: Vec<Abs> = self.out[start..].to_vec();
            self.fixed_templates.push(template);
        }

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
        // A float lays out as a block in its own formatting context (so its
        // content doesn't wrap around ancestor floats and it contains its own).
        let bstyle = Style {
            display: Display::Block,
            float: FloatSide::None,
            position: Position::Static,
            establishes_bfc: true,
            ..st.clone()
        };

        // Width resolution. An explicit `width` wins. A replaced float
        // (`<img>`/`<svg>`) uses its intrinsic width (its `width` attribute, or
        // the same 64pt default as the inline path) — its content has no
        // measurable text run. Otherwise the float is **shrink-to-fit**: it sizes
        // to its content's max-content width, clamped to the available width.
        let box_w = match st.width {
            Some(Len::Pt(w)) => w,
            Some(Len::Percent(pc)) => avail_w * pc / 100.0,
            Some(Len::Calc { pt, pct }) => avail_w * pct / 100.0 + pt,
            None if is_replaced(el) => intrinsic_replaced_width(el),
            None => self.measure_float_max_content(el, &bstyle, x, y, ancestors, avail_w),
        }
        .min(avail_w)
        .max(1.0);

        // Existing same-side floats overlapping `y` stack inward.
        let (l_in, r_in) = self.floats.insets(y, 1.0);
        let box_x = if left {
            x + l_in
        } else {
            x + avail_w - r_in - box_w
        };

        let start = self.out.len();
        let bottom_y = self.block(el, &bstyle, st, box_x, box_w, y, ancestors, 0);

        if st.z_index != 0 {
            self.stamp_context(start, st.z_index);
        }
        self.floats.boxes.push(FloatBox {
            left,
            top: y,
            bottom: bottom_y.max(y + 0.1),
            width: box_w,
        });
    }

    /// Intrinsic **max-content** width of a (non-replaced) float's content, for
    /// shrink-to-fit sizing. Lays the float's block content out into `self.out`
    /// at a very large available width (so nothing wraps), measures the widest
    /// fragment right edge relative to the layout origin `x`, then **discards**
    /// those fragments (truncating `self.out` back to the saved length). The
    /// returned width is unclamped; the caller clamps it to the available width.
    ///
    /// The throwaway pass runs at the float's real `y` and origin `x` so the
    /// measured extent is content-relative; the box's `position`/`float` are
    /// already neutralised in `bstyle`, and `bstyle` establishes a BFC so this
    /// pass also runs in an isolated float context.
    fn measure_float_max_content(
        &mut self,
        el: &Element,
        bstyle: &Style,
        x: f64,
        y: f64,
        ancestors: &[&Element],
        avail_w: f64,
    ) -> f64 {
        // A band wide enough that intrinsic content never wraps. Bounded so a
        // pathological style can't overflow arithmetic.
        const HUGE_W: f64 = 1.0e6;
        self.measure_intrinsic_width(el, bstyle, x, y, ancestors, HUGE_W, avail_w)
    }

    /// Shared intrinsic-width probe: lay `el` out in a throwaway pass at
    /// `probe_w` available width and report the widest content extent reached,
    /// clamped to `[1, cap]`. `probe_w` huge ⇒ **max-content** (nothing wraps);
    /// `probe_w` ≈ 1 ⇒ **min-content** (every breakable token wraps, so the
    /// widest single token wins). The probe fully isolates itself: fragments are
    /// discarded, the float context is untouched, and any `position: fixed`
    /// template captured by the throwaway pass is dropped.
    #[allow(clippy::too_many_arguments)]
    fn measure_intrinsic_width(
        &mut self,
        el: &Element,
        bstyle: &Style,
        x: f64,
        y: f64,
        ancestors: &[&Element],
        probe_w: f64,
        cap: f64,
    ) -> f64 {
        let probe = self.out.len();
        let fixed_mark = self.fixed_templates.len();
        // Isolate the float context: a non-BFC subtree would otherwise register
        // its floats on `self.floats` and leak probe geometry into the real flow.
        let saved_floats = std::mem::take(&mut self.floats);
        // Lay the content out at the origin with the probe width.
        self.block(el, bstyle, bstyle, x, probe_w, y, ancestors, 0);
        // Widest right edge among the **content** fragments (text / images),
        // minus the origin. Box decorations (background fills, borders, gradients)
        // are skipped: laid at the huge band, a block's own full-width background
        // would otherwise read as the max-content width. Content fragments start
        // at the inner content edge (`x + border-left + padding-left`), so their
        // extent already accounts for the left padding/border.
        let content_right = self.out[probe..]
            .iter()
            .filter(|a| {
                matches!(
                    a.frag,
                    Fragment::Text { .. } | Fragment::Image { .. } | Fragment::Svg { .. }
                )
            })
            .map(|a| fragment_right(&a.frag, self.m))
            .fold(x, f64::max);
        // Discard the throwaway fragments; the real layout re-runs at the
        // clamped width.
        self.out.truncate(probe);
        self.fixed_templates.truncate(fixed_mark);
        self.floats = saved_floats;
        // The used box width adds the right padding/border (the left side is
        // already in `content_right`).
        let box_w = (content_right - x) + bstyle.padding.right + bstyle.border_width.right;
        box_w.clamp(1.0, cap.max(1.0))
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
            self.collect_inline(n, style, ancestors, avail_w, &mut items);
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

    /// The top margin that **collapses through** `el`'s top edge (CSS 2.1
    /// §8.3.1 "adjoining margins"): the largest top margin along `el`'s
    /// first-in-flow-block-child chain, or `0` when nothing collapses. Margins
    /// collapse only when nothing separates them: `el` must have no top
    /// border/padding and must not establish a new BFC; the chain stops at the
    /// first level whose child is inline content, out-of-flow, positioned or
    /// clearance-bearing. Positioned (`relative`/`sticky`) children are excluded
    /// as a pragmatic safety: their post-layout shift math reads the pre-margin
    /// flow position, so collapsing around them would move their clamp anchor.
    fn top_margin_escape(&self, el: &Element, style: &Style, ancestors: &[&Element]) -> f64 {
        if style.border_width.top > 0.0 || style.padding.top > 0.0 || establishes_bfc(style) {
            return 0.0;
        }
        let na = push_ancestor(ancestors, el);
        for n in &el.children {
            match n {
                Node::Text(t) if t.trim().is_empty() => continue,
                Node::Text(_) => return 0.0,
                Node::Element(e) => {
                    let st = self.style_of(e, style, &na);
                    if st.display == Display::None {
                        continue;
                    }
                    if st.float != FloatSide::None
                        || st.position != Position::Static
                        || st.clear != Clear::None
                        || !matches!(
                            st.display,
                            Display::Block
                                | Display::ListItem
                                | Display::Table
                                | Display::Flex
                                | Display::Grid
                        )
                    {
                        return 0.0;
                    }
                    let own = st.margin.top.max(0.0);
                    return own.max(self.top_margin_escape(e, &st, &na));
                }
            }
        }
        0.0
    }

    /// The bottom-edge counterpart of [`Flow::top_margin_escape`]: the largest
    /// bottom margin along `el`'s last-in-flow-block-child chain. Additionally
    /// gated on `el` having an automatic height — a definite `height`,
    /// `min-height` or `aspect-ratio` separates the child's bottom margin from
    /// `el`'s own (CSS: only auto heights collapse through the bottom).
    fn bottom_margin_escape(&self, el: &Element, style: &Style, ancestors: &[&Element]) -> f64 {
        if style.border_width.bottom > 0.0
            || style.padding.bottom > 0.0
            || establishes_bfc(style)
            || style.height.is_some()
            || style.min_height.is_some()
            || style.aspect_ratio.is_some()
        {
            return 0.0;
        }
        let na = push_ancestor(ancestors, el);
        for n in el.children.iter().rev() {
            match n {
                Node::Text(t) if t.trim().is_empty() => continue,
                Node::Text(_) => return 0.0,
                Node::Element(e) => {
                    let st = self.style_of(e, style, &na);
                    if st.display == Display::None {
                        continue;
                    }
                    if st.float != FloatSide::None
                        || st.position != Position::Static
                        || st.clear != Clear::None
                        || !matches!(
                            st.display,
                            Display::Block
                                | Display::ListItem
                                | Display::Table
                                | Display::Flex
                                | Display::Grid
                        )
                    {
                        return 0.0;
                    }
                    let own = st.margin.bottom.max(0.0);
                    return own.max(self.bottom_margin_escape(e, &st, &na));
                }
            }
        }
        0.0
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
        let diagram = super::diagram::try_build(el, style, avail_w.max(1.0), self.m)?;

        // Honour the block's own vertical margins; centre the diagram in the
        // available width when it's narrower (mermaid renders centred).
        let top = y + style.margin.top;
        let dx = x + ((avail_w - diagram.width).max(0.0)) / 2.0;

        // The vector geometry (boxes + edges + arrow-heads) as one Svg fragment.
        self.out.push(Abs {
            z: 1,
            zk: Vec::new(),
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
                zk: Vec::new(),
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

    /// A `writing-mode: vertical-rl | vertical-lr` block — the **rotated
    /// label** approximation. The block's content is laid out as a normal
    /// horizontal block into a strip whose *width* is the vertical box's
    /// height budget (an explicit `height`, else the content's natural
    /// max-content width), then the whole strip — box decoration included —
    /// is wrapped in a `Transformed` rotation: 90° clockwise for
    /// `vertical-rl`, 90° counter-clockwise for `vertical-lr`. Width and
    /// height swap: the strip's height becomes the box's width and vice
    /// versa, giving the narrow-and-tall geometry vertical table headers and
    /// spine labels want.
    ///
    /// Documented limits of the first level: glyphs are rotated (no upright
    /// CJK `text-orientation`), line stacking beyond the rotation is not
    /// remodelled, and the strip's own margins rotate with it (the flow
    /// advances by the rotated height only).
    #[allow(clippy::too_many_arguments)]
    fn vertical_block(
        &mut self,
        el: &Element,
        style: &Style,
        parent_style: &Style,
        x: f64,
        avail_w: f64,
        y: f64,
        ancestors: &[&Element],
        list_index: usize,
    ) -> f64 {
        let start = self.out.len();
        // Re-enter the normal path with the mode reset (breaks the recursion)
        // and the box constraints swapped: the strip has no width/height of
        // its own — its width IS the vertical box's height budget.
        let hstyle = Style {
            writing_mode: WritingMode::HorizontalTb,
            width: None,
            height: None,
            ..style.clone()
        };
        let strip_w = match style.height {
            Some(h) => h.max(1.0),
            // No explicit vertical extent: the natural (max-content) width of
            // the strip, clamped to something sane for pathological content.
            None => self
                .measure_float_max_content(el, &hstyle, x, y, ancestors, avail_w)
                .max(1.0),
        };
        let bottom = self.block(el, &hstyle, parent_style, x, strip_w, y, ancestors, list_index);
        let strip_h = (bottom - y).max(0.1);

        // Rotate the strip about its top-left, then slide it back so the
        // rotated box's top-left lands on (x, y):
        //   vertical-rl: +90° (cw), shifted right by the strip height;
        //   vertical-lr: −90° (ccw), shifted down by the strip width.
        // CSS-space affine [a, b, c, d, e, f]: p' = (a·px + c·py + e,
        // b·px + d·py + f).
        let m = match style.writing_mode {
            WritingMode::VerticalRl => [0.0, 1.0, -1.0, 0.0, x + strip_h + y, y - x],
            _ => [0.0, -1.0, 1.0, 0.0, x - y, y + strip_w + x],
        };
        self.transform_range(start, m);
        // The vertical box occupies `strip_h × strip_w`; flow resumes below it.
        y + strip_w
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
        // `writing-mode: vertical-rl | vertical-lr` — the rotated-label model:
        // lay the block out horizontally, then rotate the subtree ±90°.
        if style.writing_mode != WritingMode::HorizontalTb {
            return self.vertical_block(el, style, _parent_style, x, avail_w, y, ancestors, list_index);
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

        // Parent/first-child margin collapsing (CSS 2.1 §8.3.1): with no top
        // border/padding and no BFC boundary, the first in-flow block child's
        // top margin collapses with this block's own — the box applies
        // `max(own, escaped)` and the child flow starts `escape` above the
        // content top, so the child's own `margin-top` re-lands it exactly at
        // the content edge (no double gap).
        let top_escape = self.top_margin_escape(el, style, ancestors);
        y += m.top.max(top_escape);
        let box_top = y;
        let resolve_w = |len: Len| len.resolve(avail_w);
        let mut box_w = match style.width {
            // `box-sizing: border-box` → `width` already includes padding+border.
            Some(Len::Pt(w)) if style.border_box => w,
            Some(Len::Pt(w)) => w + p.left + p.right + b.left + b.right,
            Some(Len::Percent(pc)) => avail_w * pc / 100.0,
            // A mixed `calc()` resolves like a percentage (border-box style).
            Some(Len::Calc { pt, pct }) => avail_w * pct / 100.0 + pt,
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
        let box_x = if style.width.is_some() && (style.margin_left_auto || style.margin_right_auto)
        {
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
                    zk: Vec::new(),
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
            // The collapsed-through child re-applies its own top margin, so the
            // child flow starts `top_escape` above the content edge (zero in
            // the common non-collapsing case — identical geometry).
            cy - top_escape,
            &new_ancestors,
            // Whether this block isolates its children's floats. A plain in-flow
            // block does not (its floats flow to the parent's following siblings
            // and reach this block's own line boxes); a BFC block does.
            establishes_bfc(style),
        );
        self.flow_cb = saved_flow_cb;

        cy += p.bottom + b.bottom;
        // Parent/last-child collapsing (the bottom-edge mirror): the escaped
        // child margin leaves the box (excluded from its height) and collapses
        // with this block's own bottom margin below.
        let bottom_escape = self.bottom_margin_escape(el, style, ancestors);
        // A definite `height` caps the box (content overflows — and is clipped
        // under `overflow: hidden`). Failing that, `aspect-ratio` derives the
        // height from the resolved width (`box_w / ratio`); otherwise the box
        // grows to its content. `min-height` is only a floor on whichever applies.
        let mut box_h = match (style.height, style.aspect_ratio) {
            (Some(h), _) => h,
            (None, Some(ratio)) if ratio > 0.0 => (box_w / ratio).max(0.1),
            _ => (cy - bottom_escape - box_top).max(0.1),
        };
        // `max-height` caps the tentative height (content overflows, visible
        // unless `overflow` clips); `min-height` floors it afterwards — the
        // CSS §10.7 order, where min wins over max when they conflict.
        if let Some(mh) = style.max_height {
            box_h = box_h.min(mh);
        }
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
            // A rounded box clips its content to the curve. Clamp the corner radii
            // to the clip box; all-zero ⇒ the painter keeps the rectangular clip.
            let clampr = |r: [f64; 4]| {
                let m = (clip.w.min(clip.h) / 2.0).max(0.0);
                r.map(|v| v.clamp(0.0, m))
            };
            self.clip_range(
                children_start,
                clip,
                clampr(style.border_radius),
                clampr(style.border_radius_v),
            );
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

        // CSS `transform`: wrap this element's whole subtree (box + children) in a
        // `Transformed` fragment, pivoting about the `transform-origin` — the
        // border-box centre (`50% 50%`) unless the author moved it. Percentages
        // resolve against the border-box dimensions, lengths from its top-left.
        if let Some(raw) = style.transform {
            let resolve = |len: &Len, dim: f64| len.resolve(dim);
            let (ox, oy) = match &style.transform_origin {
                Some((x, y)) => (box_x + resolve(x, box_w), box_top + resolve(y, box_h)),
                None => (box_x + box_w / 2.0, box_top + box_h / 2.0),
            };
            let tm = super::css::transform_about_origin(raw, ox, oy);
            self.transform_range(children_start, tm);
        }

        // The trailing margin below the box is the collapsed pair (own bottom
        // margin ⊔ escaped last-child margin).
        box_top + box_h + m.bottom.max(bottom_escape)
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
            self.collect_inline(n, style, ancestors, avail_w, &mut items);
        }
        self.flow_lines(
            &items,
            x,
            avail_w,
            y,
            style.align,
            style.text_indent,
            style.direction,
        )
    }

    fn collect_inline(
        &mut self,
        node: &Node,
        parent_style: &Style,
        ancestors: &[&Element],
        avail_w: f64,
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
                            media: Some(Media::Svg(
                                w,
                                h,
                                img,
                                crate::svg::preserve_aspect_ratio_of(e),
                            )),
                        });
                    }
                    return;
                }
                if e.tag == "img" {
                    // Intrinsic size from the HTML attributes (64pt fallback).
                    let attr_w = e.attr("width").and_then(|v| v.parse::<f64>().ok());
                    let attr_h = e.attr("height").and_then(|v| v.parse::<f64>().ok());
                    let iw = attr_w.filter(|v| *v > 0.0).unwrap_or(64.0);
                    let ih = attr_h.filter(|v| *v > 0.0).unwrap_or(64.0);
                    // CSS `width`/`height` override the attributes; a single CSS
                    // dimension preserves the intrinsic aspect ratio (percentages
                    // resolve against the inline containing block width).
                    let resolve = |len: Len| len.resolve(avail_w).max(0.0);
                    let css_w = st.width.map(resolve);
                    let css_h = st.height.map(|h| h.max(0.0));
                    let (mut w, mut h) = match (css_w, css_h) {
                        (Some(cw), Some(ch)) => (cw, ch),
                        (Some(cw), None) => (cw, if iw > 0.0 { cw * ih / iw } else { ih }),
                        (None, Some(ch)) => (if ih > 0.0 { ch * iw / ih } else { iw }, ch),
                        (None, None) => (iw, ih),
                    };
                    // `max-width` clamps the used width, scaling the height to
                    // keep the (used) ratio.
                    if let Some(mw) = st.max_width.map(resolve) {
                        if w > mw && w > 0.0 {
                            h *= mw / w;
                            w = mw;
                        }
                    }
                    let src = e.attr("src").unwrap_or_default().to_string();
                    // A `data:image/svg+xml` source renders as native vector, not a bitmap.
                    let media = crate::svg::parse_data_uri_with_aspect(&src)
                        .map(|(img, par)| Media::Svg(w, h, img, par))
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
                    self.collect_inline(c, &st, &na, avail_w, out);
                }
            }
        }
    }

    /// Tokenise inline items into measured [`Word`]s for line breaking,
    /// honouring `white-space` — `pre` keeps whitespace + newlines with no
    /// wrapping, `pre-wrap` keeps them but wraps between space runs,
    /// `pre-line` collapses spaces yet preserves newlines, and `nowrap` makes
    /// each run a single unbreakable token — and `letter-spacing` (added to
    /// each token's measured width).
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
        let push_newline = |words: &mut Vec<Word>, style: &Style| {
            words.push(Word {
                text: "\n".into(),
                style: style.clone(),
                w: 0.0,
                media: None,
                space_after: false,
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
                // Preserve whitespace: split on newlines. `pre` keeps each
                // line as ONE unbreakable token (never wraps); `pre-wrap`
                // splits the line into space-preserving tokens so an over-long
                // line can still wrap between space runs.
                let wrapping = it.style.white_space == WhiteSpace::PreWrap;
                for (i, seg) in it.text.split('\n').enumerate() {
                    if i > 0 {
                        push_newline(&mut words, &it.style);
                    }
                    if seg.is_empty() {
                        continue;
                    }
                    if wrapping {
                        for tok in split_space_runs(seg) {
                            push_text(&mut words, tok.to_string(), &it.style, false);
                        }
                    } else {
                        push_text(&mut words, seg.to_string(), &it.style, false);
                    }
                }
                continue;
            }
            if it.style.white_space == WhiteSpace::PreLine {
                // Collapse runs of spaces but honour the source newlines.
                for (i, seg) in it.text.split('\n').enumerate() {
                    if i > 0 {
                        push_newline(&mut words, &it.style);
                    }
                    for token in collapse_ws(seg).split(' ') {
                        if !token.is_empty() {
                            push_text(&mut words, token.to_string(), &it.style, true);
                        }
                    }
                }
                continue;
            }
            if it.style.white_space == WhiteSpace::Nowrap {
                // The whole (collapsed) run is ONE token: the line breaker can
                // never split inside it. Breaks stay possible at boundaries
                // between differently-styled sibling runs — a pragmatic
                // subset of `nowrap`, and exactly what single-run labels and
                // `text-overflow: ellipsis` cells need.
                let collapsed = collapse_ws(&it.text);
                let trimmed = collapsed.trim();
                if !trimmed.is_empty() {
                    push_text(&mut words, trimmed.to_string(), &it.style, true);
                }
                continue;
            }
            let normalized = collapse_ws(&it.text);
            for token in normalized.split(' ') {
                if token == "\n" {
                    push_newline(&mut words, &it.style);
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

    /// Post-process measured words for the overflow properties: a text word
    /// wider than the line `budget` is either truncated with `…`
    /// (`text-overflow: ellipsis` on a `nowrap` run inside an `overflow`-
    /// clipped box) or broken into budget-sized pieces at character
    /// boundaries (`overflow-wrap: break-word` / `word-break: break-all`).
    /// Words that fit — the overwhelming common case — pass through
    /// untouched, keeping existing layouts byte-identical.
    fn fit_overlong_words(&self, words: Vec<Word>, budget: f64) -> Vec<Word> {
        let needs_fit = |w: &Word| {
            w.media.is_none()
                && w.text != "\n"
                && w.w > budget
                && (w.style.break_word
                    || (w.style.text_overflow_ellipsis
                        && w.style.white_space == WhiteSpace::Nowrap
                        && w.style.overflow_clip))
        };
        if budget <= 0.0 || !words.iter().any(needs_fit) {
            return words;
        }
        // Measured width of a token, mirroring `build_words`'s letter-spacing.
        let measure = |t: &str, style: &Style| {
            self.m.width(t, style) + style.letter_spacing * t.chars().count().max(1) as f64
        };
        let mut out: Vec<Word> = Vec::with_capacity(words.len());
        for w in words {
            if !needs_fit(&w) {
                out.push(w);
                continue;
            }
            let chars: Vec<char> = w.text.chars().collect();
            if w.style.text_overflow_ellipsis
                && w.style.white_space == WhiteSpace::Nowrap
                && w.style.overflow_clip
            {
                // Longest prefix such that `prefix + …` fits the budget.
                let mut best = "\u{2026}".to_string();
                for i in (1..chars.len()).rev() {
                    let cand: String = chars[..i].iter().collect::<String>() + "\u{2026}";
                    if measure(&cand, &w.style) <= budget {
                        best = cand;
                        break;
                    }
                }
                let bw = measure(&best, &w.style);
                out.push(Word {
                    text: best,
                    w: bw,
                    style: w.style.clone(),
                    media: None,
                    space_after: w.space_after,
                });
                continue;
            }
            // `break-word`: greedy budget-sized pieces (each ≥ 1 char so the
            // loop always advances, even against an absurdly small budget).
            let mut start = 0usize;
            while start < chars.len() {
                let mut end = start + 1;
                let mut piece: String = chars[start..end].iter().collect();
                let mut pw = measure(&piece, &w.style);
                while end < chars.len() {
                    let cand: String = chars[start..=end].iter().collect();
                    let cw = measure(&cand, &w.style);
                    if cw > budget {
                        break;
                    }
                    end += 1;
                    piece = cand;
                    pw = cw;
                }
                let last = end >= chars.len();
                out.push(Word {
                    text: piece,
                    w: pw,
                    style: w.style.clone(),
                    media: None,
                    space_after: if last { w.space_after } else { false },
                });
                start = end;
            }
        }
        out
    }

    /// Split one logical line into **visually ordered** segments under the
    /// Unicode Bidirectional Algorithm. The levels are resolved **per
    /// character over the whole line** (inter-word spaces included, so
    /// neutrals resolve across token boundaries exactly as UAX #9 requires),
    /// *then* the visual order is grouped back into word segments. A token
    /// whose characters straddle a level boundary — e.g. `world!` at the end
    /// of an RTL sentence, whose `!` resolves to the base (RTL) level — splits
    /// at that boundary so each part lands on its correct side, instead of the
    /// whole token dragging its punctuation along (the old word-granularity
    /// model, which classified a token by its first character only).
    ///
    /// Returns owned [`Word`] segments in visual (left→right) order:
    /// * a segment's `text` keeps the **logical** character order (the painter
    ///   draws glyph runs; intra-run glyph mirroring stays its concern);
    /// * `space_after` is set on the segment to the left of each inter-word
    ///   space, so split fragments of one token stay glued (no phantom gap);
    /// * replaced boxes (images/SVG) travel as opaque one-atom units.
    fn bidi_visual_segments(&self, line: &[&Word], base: u8) -> Vec<Word> {
        /// What one bidi atom stands for.
        #[derive(Clone, Copy)]
        enum Owner {
            /// The `ci`-th character (char index) of `line[word]`.
            Char { word: usize, ci: usize },
            /// An inter-word space (becomes the left segment's trailing gap).
            Space,
            /// An opaque unit box (replaced content / empty text).
            Unit(usize),
        }
        let mut atoms: Vec<String> = Vec::new();
        let mut owners: Vec<Owner> = Vec::new();
        for (wi, w) in line.iter().enumerate() {
            if w.media.is_some() || w.text.is_empty() {
                // One (empty ⇒ boundary-neutral) atom: the box rides the flow.
                atoms.push(String::new());
                owners.push(Owner::Unit(wi));
            } else {
                for (ci, c) in w.text.chars().enumerate() {
                    atoms.push(c.to_string());
                    owners.push(Owner::Char { word: wi, ci });
                }
            }
            if w.space_after && wi + 1 < line.len() {
                atoms.push(" ".to_string());
                owners.push(Owner::Space);
            }
        }
        let refs: Vec<&str> = atoms.iter().map(|s| s.as_str()).collect();
        let order = bidi::reorder(&refs, base);

        // Group the visual char walk into segments: a maximal visual run of
        // characters of the same word at consecutive (±1) char indices is one
        // segment (L2 only reverses whole runs, so such a run is a contiguous
        // logical slice — sliced in logical order).
        struct Group {
            word: usize,
            lo: usize,
            hi: usize,
            prev: usize,
        }
        let m = self.m;
        let mut segments: Vec<Word> = Vec::new();
        let mut cur: Option<Group> = None;
        let flush = |cur: &mut Option<Group>, segments: &mut Vec<Word>| {
            if let Some(g) = cur.take() {
                let w = line[g.word];
                let text: String = w.text.chars().skip(g.lo).take(g.hi - g.lo + 1).collect();
                let n_chars = (g.hi - g.lo + 1) as f64;
                let width = m.width(&text, &w.style) + w.style.letter_spacing * n_chars.max(1.0);
                segments.push(Word {
                    text,
                    style: w.style.clone(),
                    w: width,
                    media: None,
                    space_after: false,
                });
            }
        };
        for &ai in &order {
            match owners[ai] {
                Owner::Space => {
                    flush(&mut cur, &mut segments);
                    if let Some(prev) = segments.last_mut() {
                        prev.space_after = true;
                    }
                }
                Owner::Unit(wi) => {
                    flush(&mut cur, &mut segments);
                    let w = line[wi];
                    segments.push(Word {
                        text: w.text.clone(),
                        style: w.style.clone(),
                        w: w.w,
                        media: w.media.clone(),
                        space_after: false,
                    });
                }
                Owner::Char { word, ci } => match &mut cur {
                    Some(g) if g.word == word && ci.abs_diff(g.prev) == 1 => {
                        g.lo = g.lo.min(ci);
                        g.hi = g.hi.max(ci);
                        g.prev = ci;
                    }
                    _ => {
                        flush(&mut cur, &mut segments);
                        cur = Some(Group {
                            word,
                            lo: ci,
                            hi: ci,
                            prev: ci,
                        });
                    }
                },
            }
        }
        flush(&mut cur, &mut segments);
        segments
    }

    /// Emit one line of words at vertical position `*y`, aligned within
    /// `[line_x, line_x + line_avail)`, then advance `*y` by the line height.
    /// `space_w` is the inter-word space; `word_extra` is added at each space on
    /// top of it (used by `word-spacing`). `last` suppresses justification.
    ///
    /// `dir` is the inline base direction. A left-to-right line whose
    /// characters are all bidi-inert takes the original verbatim placement
    /// (byte-identical fast path). Any RTL base — or an LTR line carrying
    /// RTL/AN content or a bidi control — goes through the Unicode
    /// Bidirectional Algorithm **at character granularity over the whole line**
    /// ([`Flow::bidi_visual_segments`]): levels resolve across token
    /// boundaries, mixed tokens split at their level boundaries (so trailing
    /// punctuation lands on its correct side), and the visually-ordered
    /// segments are placed left-to-right from a start `x` that encodes the
    /// resolved alignment (the RTL default hugs the right edge). The glyphs
    /// *inside* a segment keep logical order (the painter's concern).
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

        // Resolve the line through the Unicode bidi algorithm at CHARACTER
        // granularity (spaces included) and place the visual segments. A
        // pure-LTR line with only bidi-inert characters skips all of it and
        // falls through to the original verbatim placement (byte-identical).
        let base = if dir == Direction::Rtl { 1 } else { 0 };
        let is_significant = |c: char| {
            matches!(
                bidi::bidi_class(c),
                bidi::BidiClass::R
                    | bidi::BidiClass::Al
                    | bidi::BidiClass::An
                    | bidi::BidiClass::Lre
                    | bidi::BidiClass::Rle
                    | bidi::BidiClass::Lro
                    | bidi::BidiClass::Rlo
                    | bidi::BidiClass::Pdf
                    | bidi::BidiClass::Lri
                    | bidi::BidiClass::Rli
                    | bidi::BidiClass::Fsi
                    | bidi::BidiClass::Pdi
            )
        };
        if dir == Direction::Rtl || line.iter().any(|w| w.text.chars().any(is_significant)) {
            let segments = self.bidi_visual_segments(line, base);
            let visual: Vec<&Word> = segments.iter().collect();
            self.place_line_visual(&visual, y, last, line_x, align, space_w, extra);
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
                        zk: Vec::new(),
                        frag: Fragment::Image {
                            x: cx,
                            y: *y,
                            w: *iw,
                            h: *ih,
                            src: src.clone(),
                            fit: w.style.object_fit,
                        },
                    });
                    cx += iw + space_w;
                }
                Some(Media::Svg(iw, ih, image, par)) => {
                    self.out.push(Abs {
                        z: 1,
                        zk: Vec::new(),
                        frag: svg_fragment(cx, *y, *iw, *ih, image, *par),
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
                    // The line-relative values (`middle`/`bottom`) add an offset
                    // within the line box, resolved against this line's height.
                    self.out.push(Abs {
                        z: 1,
                        zk: Vec::new(),
                        frag: Fragment::Text {
                            x: cx,
                            y: *y + w.style.valign_shift + inline_valign_delta(&w.style, line_h),
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

    /// Place one already-broken line whose word boxes are **already in visual
    /// order** (the bidi reorder ran in [`emit_line`]). Placement is always
    /// left-to-right from a start `cx` that encodes the resolved *physical*
    /// alignment, so it serves both an RTL line (default `Right` ⇒ the block
    /// hugs the right edge) and an LTR line that carries a reordered RTL run.
    ///
    /// `line` holds the boxes in visual (left→right) order; `extra` is the line's
    /// slack (`line_avail - content`). The per-box trailing inter-word advance
    /// (space + `word-spacing`, plus the justification gap) is added after each
    /// box that carries `space_after` — visually adjacent fragments of one
    /// split token stay glued — and the last box draws no trailing space.
    #[allow(clippy::too_many_arguments)]
    fn place_line_visual(
        &mut self,
        line: &[&Word],
        y: &f64,
        last: bool,
        line_x: f64,
        align: Align,
        space_w: f64,
        extra: f64,
    ) {
        // Start x of the first visual box, mirroring the LTR `cx` computation so
        // the geometry is identical for the same resolved alignment: `Left`/
        // `Justify` start at the leading edge, `Right` shifts the block right by
        // the full slack, `Center` by half. `Justify` then spreads the slack into
        // the inter-word gaps (non-last lines only).
        let mut cx = match align {
            Align::Left | Align::Justify => line_x,
            Align::Right => line_x + extra,
            Align::Center => line_x + extra / 2.0,
            // `Start`/`End` are resolved to physical edges before this call.
            Align::Start | Align::End => line_x,
        };
        let last_idx = line.len().saturating_sub(1);
        let gap_extra = if matches!(align, Align::Justify) && !last {
            // One justification gap per real inter-word space (the last box's
            // trailing space never draws, so it doesn't count).
            let gaps = line
                .iter()
                .take(last_idx)
                .filter(|w| w.space_after)
                .count()
                .max(1);
            extra / gaps as f64
        } else {
            0.0
        };
        // This line's height, for the line-relative `vertical-align` values
        // (mirrors the computation in `emit_line`).
        let line_h = line
            .iter()
            .map(|w| w.style.font_size * w.style.line_height.max(1.0))
            .fold(0.0_f64, f64::max);
        for (i, w) in line.iter().enumerate() {
            let spaced = i != last_idx && w.space_after;
            match &w.media {
                Some(Media::Raster(iw, ih, src)) => {
                    self.out.push(Abs {
                        z: 1,
                        zk: Vec::new(),
                        frag: Fragment::Image {
                            x: cx,
                            y: *y,
                            w: *iw,
                            h: *ih,
                            src: src.clone(),
                            fit: w.style.object_fit,
                        },
                    });
                    cx += iw + if spaced { space_w } else { 0.0 };
                }
                Some(Media::Svg(iw, ih, image, par)) => {
                    self.out.push(Abs {
                        z: 1,
                        zk: Vec::new(),
                        frag: svg_fragment(cx, *y, *iw, *ih, image, *par),
                    });
                    cx += iw + if spaced { space_w } else { 0.0 };
                }
                None => {
                    // Inline highlight behind the glyphs (see the LTR path).
                    push_run_highlight(&mut self.out, cx, *y, w);
                    self.out.push(Abs {
                        z: 1,
                        zk: Vec::new(),
                        frag: Fragment::Text {
                            x: cx,
                            y: *y + w.style.valign_shift + inline_valign_delta(&w.style, line_h),
                            w: w.w,
                            style: w.style.clone(),
                            text: w.text.clone(),
                        },
                    });
                    cx += w.w
                        + if spaced {
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
        // Overflow handling for words wider than the line (`break-word` split,
        // `text-overflow: ellipsis` truncation) — a no-op pass-through unless
        // such a word exists. Budgeted against the tighter (indented) line.
        let words = self.fit_overlong_words(words, (avail_w - indent.max(0.0)).max(1.0));
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
        // Overflow handling (`break-word` / ellipsis). The float insets vary
        // per line band, so the full width is the budget — pieces re-fit at
        // the actual wrap points anyway.
        let words = self.fit_overlong_words(words, (avail_w - indent.max(0.0)).max(1.0));
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
    /// (fixed-layout style); columns with no declared width are sized to their
    /// content — the remainder is split proportionally to each column's
    /// max-content width, floored at its min-content (CSS auto layout). Cells
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
        let na = push_ancestor(ancestors, el);

        // `<caption>` renders as a block above the grid (`caption-side: top`,
        // the default) or below it (`caption-side: bottom`, read off the
        // caption's computed style — the property inherits from the table).
        // Previously the caption was silently dropped — `collect_rows` only
        // walks `tr`/row groups.
        let mut bottom_captions: Vec<(&Element, Style)> = Vec::new();
        for n in &el.children {
            if let Node::Element(cap) = n {
                if cap.tag == "caption" {
                    let cstyle = self.style_of(cap, style, &na);
                    if cstyle.display != Display::None {
                        let bstyle = Style {
                            display: Display::Block,
                            ..cstyle
                        };
                        if bstyle.caption_side == super::css::CaptionSide::Bottom {
                            bottom_captions.push((cap, bstyle));
                        } else {
                            y = self.block(cap, &bstyle, style, x, avail_w, y, &na, 0);
                        }
                    }
                }
            }
        }

        let table_top = y;
        let (rows, thead_flags) = collect_rows(el);

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
            background_alpha: f64,
            border_width: super::css::Edges,
            border_color_edges: [[f64; 3]; 4],
            border_color_alpha: f64,
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
                    true, // each column is its own block formatting context
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
                    background_alpha: cstyle.background_alpha,
                    border_width: bw,
                    border_color_edges: cstyle.border_color_edges,
                    border_color_alpha: cstyle.border_color_alpha,
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

        // Recompute the final row tops from the resolved heights. With a
        // `<thead>`, a body row that would straddle a page boundary is pushed to
        // the next page top **plus the header height**, reserving the band where
        // the thead rows are re-stamped (running table header). Tables without a
        // thead keep the plain accumulation (rows may split across pages, as
        // before).
        let thead_n = thead_flags.iter().take_while(|f| **f).count();
        let header_h: f64 = row_h[..thead_n.min(n_rows)].iter().sum();
        let content_h = (self.page_h - self.top - self.bottom).max(1.0);
        // Bottom of the page band containing `yy` (flow space; a band's bottom
        // coincides with the next band's top).
        let band_bottom_of = |yy: f64| -> f64 {
            let k = ((yy - self.top).max(0.0) / content_h).floor();
            self.top + (k + 1.0) * content_h
        };
        let mut header_repeat_tops: Vec<f64> = Vec::new();
        let mut acc_y = table_top;
        for r in 0..n_rows {
            if thead_n > 0
                && thead_n < n_rows
                && r >= thead_n
                // Only rows that can actually fit under a repeated header are
                // moved; a row taller than the reserved band stays and splits.
                && header_h + row_h[r] <= content_h
                && acc_y + row_h[r] > band_bottom_of(acc_y) + 0.01
            {
                let page_top = band_bottom_of(acc_y);
                header_repeat_tops.push(page_top);
                acc_y = page_top + header_h;
            }
            row_top[r] = acc_y;
            acc_y += row_h[r];
        }

        // Placement pass: correct each cell vertically (translate from its
        // provisional top to the final one), apply `vertical-align`, then emit
        // the background and per-side borders over the cell's full merged rect.
        // Thead cells additionally contribute their (positioned) fragments to a
        // running-header template, re-stamped on every later page of the table.
        let mut header_template: Vec<Abs> = Vec::new();
        for pl in &placed {
            let (dx, cw) = span_geom(pl.start, pl.col_span);
            let cell_x = x + dx;
            let deco_lo = self.out.len();
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
                    zk: Vec::new(),
                    frag: Fragment::Rect {
                        x: cell_x,
                        y: top,
                        w: cw,
                        h: cell_h,
                        fill: Some(fill),
                        stroke: None,
                        stroke_w: 0.0,
                        opacity: (pl.opacity * pl.background_alpha).clamp(0.0, 1.0),
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
            self.emit_border_edges(
                cell_x,
                top,
                cw,
                cell_h,
                sides,
                bc,
                bs,
                (pl.opacity * pl.border_color_alpha).clamp(0.0, 1.0),
                [0.0; 4], // table cells have square corners
            );

            // A thead cell's final fragments (translated content + the
            // decorations just emitted) join the running-header template.
            if pl.row < thead_n && !header_repeat_tops.is_empty() {
                header_template.extend(self.out[pl.frag_lo..pl.frag_hi].iter().cloned());
                let deco_hi = self.out.len();
                header_template.extend(self.out[deco_lo..deco_hi].iter().cloned());
            }
        }

        // Re-stamp the thead rows at the top of every later page the table
        // spans (the space was reserved when the row tops were laid).
        for &page_top in &header_repeat_tops {
            let dy = page_top - table_top;
            for abs in &header_template {
                let mut clone = abs.clone();
                shift_fragment(&mut clone.frag, 0.0, dy);
                self.out.push(clone);
            }
        }

        // `caption-side: bottom` captions land below the grid.
        let mut end_y = acc_y;
        for (cap, bstyle) in &bottom_captions {
            end_y = self.block(cap, bstyle, style, x, avail_w, end_y, &na, 0);
        }

        end_y + style.margin.bottom
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
                zk: Vec::new(),
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
                zk: Vec::new(),
                frag: Fragment::Rect {
                    x,
                    y,
                    w,
                    h,
                    fill: style.background,
                    stroke: None,
                    stroke_w: 0.0,
                    opacity: (style.opacity * style.background_alpha).clamp(0.0, 1.0),
                    radius,
                    radius_v,
                    shadow,
                },
            });
        }
        if let Some(grad) = &style.background_gradient {
            self.out.push(Abs {
                z: 0,
                zk: Vec::new(),
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
        // Record the emitted range — including the empty one — so
        // `stamp_context` always sees the range of the LAST decoration call
        // (the stamped element's own; a block decorates after its children).
        let start = self.out.len();
        self.emit_box_decoration_inner(style, x, y, w, h);
        self.last_decor = Some((start, self.out.len()));
    }

    fn emit_box_decoration_inner(&mut self, style: &Style, x: f64, y: f64, w: f64, h: f64) {
        if style.hidden {
            return;
        }
        let b = &style.border_width;
        let any_border = b.top + b.bottom + b.left + b.right > 0.0;
        let any_bg = style.background.is_some()
            || style.background_gradient.is_some()
            || style.background_image.is_some();
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
        let all_solid = style
            .border_style_edges
            .iter()
            .all(|s| *s == BorderStyle::Solid);
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
                zk: Vec::new(),
                frag: Fragment::Rect {
                    x,
                    y,
                    w,
                    h,
                    fill: style.background,
                    stroke: Some(style.border_color_edges[0]),
                    stroke_w: b.top.max(0.0),
                    // One opacity covers both fill and stroke: take the
                    // background's alpha when there is a fill, else the border's.
                    opacity: (style.opacity
                        * if style.background.is_some() {
                            style.background_alpha
                        } else {
                            style.border_color_alpha
                        })
                    .clamp(0.0, 1.0),
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
            // become dash/dot/double bands. With a radius set, a styled side is
            // shortened by its adjoining corner radii and the painter follows
            // the corner arcs; asymmetric SOLID sides still keep square corners
            // (the uniform-solid case took the rounded fill+stroke path above).
            self.emit_border_edges(
                x,
                y,
                w,
                h,
                [b.top, b.right, b.bottom, b.left],
                &style.border_color_edges,
                &style.border_style_edges,
                (style.opacity * style.border_color_alpha).clamp(0.0, 1.0),
                radius,
            );
        }

        // `background-image: url(...)` — a raster fill over the background colour,
        // behind the content (z=0, pushed last so it sits above the colour). Fills
        // the box; `background-size`/`-position`/`-repeat` are not yet modelled.
        if let Some(src) = &style.background_image {
            self.out.push(Abs {
                z: 0,
                zk: Vec::new(),
                frag: Fragment::Image {
                    x,
                    y,
                    w,
                    h,
                    src: src.clone(),
                    // A background fills its box; `object-fit` applies to
                    // replaced elements only.
                    fit: ObjectFit::Fill,
                },
            });
        }
    }

    /// Emit up to four per-side border rules for the box `(x, y, w, h)` (top-down).
    /// `widths`/`colors`/`styles` are `[top, right, bottom, left]`. A `Solid` side
    /// (the default) is a thin filled rectangle — exact per-side placement, width
    /// and colour, the only way to honour `border-bottom: 2pt` without thickening
    /// the other three sides — and is byte-identical to the pre-style behaviour.
    /// A `Dashed`/`Dotted`/`Double` side becomes a [`Fragment::Border`] band the
    /// paint layer expands into segments / parallel lines. `radius` is the box's
    /// `border-radius` `[tl, tr, br, bl]` (clamped, circular approximation): a
    /// styled side adjoining a rounded corner is shortened by that radius and
    /// carries it on the fragment, so the painter's dashes stop where the arc
    /// begins and continue along it instead of overshooting the corner.
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
        radius: [f64; 4],
    ) {
        // A `Solid` side: the legacy filled rectangle (corners overlap so the
        // frame joins cleanly — acceptable: same colour, opaque overlap).
        let push_solid = |out: &mut Vec<Abs>, rx: f64, ry: f64, rw: f64, rh: f64, c: [f64; 3]| {
            out.push(Abs {
                z: 0,
                zk: Vec::new(),
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
        // With a rounded adjoining corner the band is shortened by the corner
        // radius (the fragment carries the radii so the painter can follow the
        // arc). A square corner (`r = 0`) emits the exact previous band.
        #[allow(clippy::too_many_arguments)]
        let push_styled = |out: &mut Vec<Abs>,
                           rx: f64,
                           ry: f64,
                           rw: f64,
                           rh: f64,
                           c: [f64; 3],
                           horizontal: bool,
                           width: f64,
                           style: BorderStyle,
                           r_start: f64,
                           r_end: f64,
                           far: bool| {
            let (rx, ry, rw, rh) = if horizontal {
                (rx + r_start, ry, (rw - r_start - r_end).max(0.01), rh)
            } else {
                (rx, ry + r_start, rw, (rh - r_start - r_end).max(0.01))
            };
            out.push(Abs {
                z: 0,
                zk: Vec::new(),
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
                    radius_start: r_start.max(0.0),
                    radius_end: r_end.max(0.0),
                    far,
                },
            });
        };
        // Dispatch one side to the solid / styled / 3-D bevel emitter. `near` is
        // true for the top and left sides (the ones lit dark for `inset`).
        #[allow(clippy::too_many_arguments)]
        let emit_side = |out: &mut Vec<Abs>,
                         rx: f64,
                         ry: f64,
                         rw: f64,
                         rh: f64,
                         c: [f64; 3],
                         horizontal: bool,
                         width: f64,
                         style: BorderStyle,
                         near: bool,
                         r_start: f64,
                         r_end: f64| {
            match style {
                BorderStyle::Solid => push_solid(out, rx, ry, rw, rh, c),
                BorderStyle::Inset | BorderStyle::Outset => {
                    // One tone per side: lit when raised==near (outset top/left,
                    // inset bottom/right), shadowed otherwise.
                    let raised = matches!(style, BorderStyle::Outset);
                    let shade = if raised == near {
                        lighten(c)
                    } else {
                        darken(c)
                    };
                    push_solid(out, rx, ry, rw, rh, shade);
                }
                BorderStyle::Groove | BorderStyle::Ridge => {
                    // Two half-width bands with opposite tones. The "outer" half is
                    // at rx/ry on a near side (top/left), the far half otherwise.
                    let ridge = matches!(style, BorderStyle::Ridge);
                    let outer_lit = ridge == near;
                    let oc = if outer_lit { lighten(c) } else { darken(c) };
                    let ic = if outer_lit { darken(c) } else { lighten(c) };
                    if horizontal {
                        let half = rh / 2.0;
                        let (oy, iy) = if near {
                            (ry, ry + half)
                        } else {
                            (ry + half, ry)
                        };
                        push_solid(out, rx, oy, rw, half, oc);
                        push_solid(out, rx, iy, rw, half, ic);
                    } else {
                        let half = rw / 2.0;
                        let (ox, ix) = if near {
                            (rx, rx + half)
                        } else {
                            (rx + half, rx)
                        };
                        push_solid(out, ox, ry, half, rh, oc);
                        push_solid(out, ix, ry, half, rh, ic);
                    }
                }
                _ => push_styled(
                    out, rx, ry, rw, rh, c, horizontal, width, style, r_start, r_end, !near,
                ),
            }
        };
        let [wt, wr, wb, wl] = widths;
        let [st, sr, sb, sl] = *styles;
        let [rtl_, rtr, rbr, rbl] = radius;
        // `near` (top + left) selects the dark side for `inset`. TRBL colour
        // indices: top=0, right=1, bottom=2, left=3. Corner radii adjoin the
        // sides as: top ↦ (tl, tr), bottom ↦ (bl, br), left ↦ (tl, bl),
        // right ↦ (tr, br) — each in the band's start→end order.
        if wt > 0.0 {
            emit_side(
                &mut self.out,
                x,
                y,
                w,
                wt,
                colors[0],
                true,
                wt,
                st,
                true,
                rtl_,
                rtr,
            );
        }
        if wb > 0.0 {
            emit_side(
                &mut self.out,
                x,
                y + h - wb,
                w,
                wb,
                colors[2],
                true,
                wb,
                sb,
                false,
                rbl,
                rbr,
            );
        }
        if wl > 0.0 {
            emit_side(
                &mut self.out,
                x,
                y,
                wl,
                h,
                colors[3],
                false,
                wl,
                sl,
                true,
                rtl_,
                rbl,
            );
        }
        if wr > 0.0 {
            emit_side(
                &mut self.out,
                x + w - wr,
                y,
                wr,
                h,
                colors[1],
                false,
                wr,
                sr,
                false,
                rtr,
                rbr,
            );
        }
    }

    /// Resolve the table's column widths (length `ncols`) to absolute points
    /// summing to `avail_w`. Declared widths come first from `<col>` elements
    /// (honouring `span`), else from the first row's per-cell `width`; if every
    /// column is declared the widths are scaled proportionally to fit `avail_w`
    /// (browser fixed-layout). Columns without a declared width are sized to
    /// their **content** (the CSS 2.1 auto table layout, approximated): the
    /// space the declared columns leave is distributed proportionally to each
    /// column's max-content width, floored at its min-content width — so a
    /// column of long text gets more room and no column drops below its widest
    /// unbreakable token. Undeclared columns with equal content still split
    /// equally (proportional shares of equal weights).
    fn resolve_col_widths(
        &mut self,
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
                    Len::Calc { pt, pct } => (avail_w * pct / 100.0 + pt).max(0.0),
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
                return decl.iter().map(|d| d.unwrap_or(equal) * scale).collect();
            }
            return vec![equal; ncols];
        }

        // Undeclared columns: content-based auto sizing. Measure each column's
        // min-content (widest unbreakable token — a ~1pt probe wraps everything)
        // and max-content (nothing wraps — a huge probe) over its single-span
        // cells; colspan cells don't pin a single column and are skipped.
        let rest = (avail_w - declared_sum).max(0.0);
        let fill = rest / undeclared as f64;
        let (grid, _) = build_grid(rows);
        let mut min_c = vec![0.0f64; ncols];
        let mut max_c = vec![0.0f64; ncols];
        let mut measured = vec![false; ncols];
        for gc in &grid {
            if gc.col_span != 1 || gc.col >= ncols || decl[gc.col].is_some() {
                continue;
            }
            let cstyle = self.style_of(gc.el, style, na);
            max_c[gc.col] = max_c[gc.col]
                .max(self.measure_intrinsic_width(gc.el, &cstyle, 0.0, 0.0, na, 1.0e6, avail_w));
            min_c[gc.col] = min_c[gc.col]
                .max(self.measure_intrinsic_width(gc.el, &cstyle, 0.0, 0.0, na, 1.0, avail_w));
            measured[gc.col] = true;
        }
        // A column with no single-span cell keeps the equal share as its weight.
        for c in 0..ncols {
            if decl[c].is_none() && !measured[c] {
                max_c[c] = fill.max(1.0);
                min_c[c] = 0.0;
            }
        }

        // Distribute `rest` over the undeclared columns proportionally to their
        // max-content, then floor at min-content: floored columns freeze and the
        // remainder is re-shared among the rest (bounded rounds). If the floors
        // alone exceed `rest` the columns keep their min-content (the table
        // overflows, as CSS does, rather than crushing a word).
        let mut w: Vec<f64> = vec![0.0; ncols];
        let mut frozen: Vec<bool> = (0..ncols).map(|c| decl[c].is_some()).collect();
        for _ in 0..ncols.max(1) {
            let pool: f64 = (0..ncols)
                .filter(|&c| !frozen[c])
                .map(|c| max_c[c].max(0.001))
                .sum();
            let left = rest
                - (0..ncols)
                    .filter(|&c| frozen[c] && decl[c].is_none())
                    .map(|c| w[c])
                    .sum::<f64>();
            let mut clamped = false;
            for c in 0..ncols {
                if frozen[c] {
                    continue;
                }
                w[c] = (left.max(0.0) * max_c[c].max(0.001) / pool).max(0.0);
                if w[c] < min_c[c] - 0.01 {
                    w[c] = min_c[c];
                    frozen[c] = true;
                    clamped = true;
                }
            }
            if !clamped {
                break;
            }
        }
        (0..ncols).map(|c| decl[c].unwrap_or(w[c])).collect()
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

    /// Partition flex items into lines for `flex-wrap`. Without wrap every item
    /// stays on a single line. The wrap decision uses each item's flex base
    /// size — `flex-basis`, else `width`, else its **intrinsic max-content
    /// width** (measured by a throwaway probe), so a row of text-sized items
    /// still wraps when their natural widths overflow the container.
    fn flex_wrap_lines<'b>(
        &mut self,
        items: &[&'b Element],
        style: &Style,
        content_w: f64,
        na: &[&Element],
    ) -> Vec<Vec<&'b Element>> {
        if !style.flex_wrap {
            return vec![items.to_vec()];
        }
        let gap = style.gap_col;
        let mut lines: Vec<Vec<&Element>> = Vec::new();
        let mut cur: Vec<&Element> = Vec::new();
        let mut used = 0.0;
        for it in items {
            let st = self.style_of(it, style, na);
            // Wrap decision uses the flex base size (basis, else width, else the
            // measured max-content width — clamped to the line, so a single item
            // wider than the container still gets a line of its own).
            let w = match st.flex_basis.or(st.width) {
                Some(Len::Pt(w)) => w.max(0.0),
                Some(Len::Percent(pc)) => content_w * pc / 100.0,
                Some(Len::Calc { pt, pct }) => (content_w * pct / 100.0 + pt).max(0.0),
                None => self.measure_float_max_content(it, &st, 0.0, 0.0, na, content_w),
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
            Len::Calc { pt, pct } => (avail * pct / 100.0 + pt).max(0.0),
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
                true, // a flex item establishes a block formatting context
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
        // The container's available main-axis (height) extent, if definite — the
        // basis for `flex-grow`/`flex-shrink` and `%` `flex-basis` on this axis.
        let avail_main = style.height.or(style.min_height);

        // Pass 1: lay each item's content out at its natural (content-stacked)
        // position, recording the metrics needed to re-flex its main size.
        struct ColItem {
            range: (usize, usize),
            natural_top: f64,
            content_h: f64,
            box_x: f64,
            box_w: f64,
            margin_top: f64,
            margin_bottom: f64,
            grow: f64,
            shrink: f64,
            basis: Option<f64>,
            istyle: Style,
        }
        let mut cols: Vec<ColItem> = Vec::with_capacity(items.len());
        let mut y = row_top;
        for (i, it) in items.iter().enumerate() {
            if i > 0 {
                y += gap;
            }
            let istyle = self.style_of(it, style, na);
            let nca = push_ancestor(na, it);
            let im = istyle.margin;
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
                Some(Len::Calc { pt, pct }) => (content_w * pct / 100.0 + pt).max(0.0),
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
                item_top + ip.top + ib.top,
                &nca,
                true, // a grid item establishes a block formatting context
            );
            let content_h = (cy + ip.bottom + ib.bottom - item_top).max(0.0);
            // The main-axis basis: `flex-basis`, else a definite `height`. `%`
            // resolves against the container's definite height (else content).
            let basis = match istyle.flex_basis {
                Some(Len::Pt(v)) => Some(v.max(0.0)),
                Some(Len::Percent(pc)) => {
                    Some(avail_main.map(|a| a * pc / 100.0).unwrap_or(content_h))
                }
                Some(Len::Calc { pt, pct }) => Some(
                    avail_main
                        .map(|a| a * pct / 100.0 + pt)
                        .unwrap_or(content_h)
                        .max(0.0),
                ),
                None => istyle.height,
            };
            cols.push(ColItem {
                range: (start, self.out.len()),
                natural_top: item_top,
                content_h,
                box_x: content_x + im.left + dx,
                box_w,
                margin_top: im.top,
                margin_bottom: im.bottom,
                grow: istyle.flex_grow.max(0.0),
                shrink: istyle.flex_shrink.max(0.0),
                basis,
                istyle,
            });
            y = item_top + content_h + im.bottom;
        }

        // Resolve each item's final main-axis height: its basis (or content),
        // then distribute the container's leftover height by `flex-grow`, or
        // shrink it back by `flex-shrink × basis` when the items overflow. Any
        // height the items don't absorb is left for `justify-content`.
        let n = cols.len();
        let mut heights: Vec<f64> = cols
            .iter()
            .map(|c| c.basis.unwrap_or(c.content_h).max(0.0))
            .collect();
        let mut leftover_for_justify = 0.0;
        if let Some(avail) = avail_main {
            let gaps = gap * n.saturating_sub(1) as f64;
            let margins: f64 = cols.iter().map(|c| c.margin_top + c.margin_bottom).sum();
            let free = avail - heights.iter().sum::<f64>() - gaps - margins;
            let total_grow: f64 = cols.iter().map(|c| c.grow).sum();
            if free > 0.01 && total_grow > 0.0 {
                for (h, c) in heights.iter_mut().zip(&cols) {
                    *h += free * c.grow / total_grow;
                }
            } else if free < -0.01 {
                let shrinks: Vec<f64> = cols.iter().map(|c| c.shrink).collect();
                shrink_to_fit(&mut heights, &shrinks, -free);
                leftover_for_justify =
                    (avail - heights.iter().sum::<f64>() - gaps - margins).max(0.0);
            } else {
                leftover_for_justify = free.max(0.0);
            }
        }

        // Pass 2: stack the items at their resolved heights — shifting each one's
        // already-placed content to its final top — and paint the item box.
        let (j_offset, j_gap) = if n > 0 {
            justify_offsets(style.justify, leftover_for_justify, n)
        } else {
            (0.0, 0.0)
        };
        let mut cursor = row_top + j_offset;
        for (i, c) in cols.iter().enumerate() {
            if i > 0 {
                cursor += gap + j_gap;
            }
            cursor += c.margin_top;
            let final_top = cursor;
            let dy = final_top - c.natural_top;
            if dy.abs() > f64::EPSILON {
                for a in &mut self.out[c.range.0..c.range.1] {
                    shift_fragment(&mut a.frag, 0.0, dy);
                }
            }
            self.paint_item_box(&c.istyle, c.box_x, final_top, c.box_w, heights[i]);
            cursor = final_top + heights[i] + c.margin_bottom;
        }
        // A definite-height container fills its height; otherwise the bottom is
        // wherever the (content-sized) items end.
        avail_main.map(|a| row_top + a).unwrap_or(cursor)
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

        // Column count: the track list, widened to fit any `grid-template-areas`
        // (so areas declared without explicit columns still get enough columns).
        let area_cols = style
            .grid_template_areas
            .iter()
            .map(|a| a.col + a.col_span)
            .max()
            .unwrap_or(0);
        let cols = style.grid_columns.max(area_cols).max(1);
        let content_x = x + m.left + b.left + p.left;
        let content_w = (avail_w - m.left - m.right - b.left - b.right - p.left - p.right).max(1.0);
        let y_cursor = y + b.top + p.top;
        let gap_col = style.gap_col.max(0.0);
        let gap_row = style.gap_row.max(0.0);

        // Assign each item a (row, col) cell + span. Items with an explicit
        // `grid-column`/`grid-row` start line land on that line; the rest
        // auto-flow into the next free cell (left-to-right, top-to-bottom).
        // (Placement is independent of the column widths, and the widths need it
        // to size `auto` tracks to their content below.)
        let placement = self.place_grid_items(&items, style, &na, cols);

        // `auto` tracks size to their content (max-content, clamped to the
        // grid), not to an equal `1fr`-like share. Measure each auto column's
        // widest single-column item with the intrinsic probe; non-auto track
        // lists skip the probe entirely (identical output to before).
        let tracks = &style.grid_template_columns;
        let has_auto_track = !tracks.is_empty()
            && (0..cols).any(|i| matches!(tracks.get(i), Some(TrackSize::Auto) | None));
        let auto_content: Vec<f64> = if has_auto_track {
            let mut content = vec![0.0f64; cols];
            for cell in placement.iter().filter(|p| p.col_span == 1) {
                if !matches!(tracks.get(cell.col), Some(TrackSize::Auto) | None) {
                    continue;
                }
                let it = items[cell.item];
                let istyle = self.style_of(it, style, &na);
                let w = self.measure_float_max_content(it, &istyle, 0.0, 0.0, &na, content_w);
                content[cell.col] = content[cell.col].max(w);
            }
            content
        } else {
            Vec::new()
        };

        // Resolve per-column widths from the track list (or equal columns when no
        // track list is present), then the cumulative x of each column's left
        // edge (including the column gaps that precede it).
        let col_widths = if has_auto_track {
            resolve_track_sizes_content(tracks, cols, content_w, gap_col, &auto_content)
        } else {
            resolve_track_sizes(tracks, cols, content_w, gap_col)
        };
        let col_x: Vec<f64> = cumulative_offsets(content_x, &col_widths, gap_col);
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

        // The grid's definite content-box height (its row track region), if the
        // container has an explicit `height`. `fr`/`%` rows need it; without it
        // they fall back to content (`auto`), the correct auto-height behaviour.
        let avail_row_h = style
            .height
            .map(|h| (h - b.top - b.bottom - p.top - p.bottom).max(0.0));

        // Explicit row-track height for row `r`, or `None` = auto (sized to the
        // tallest cell). `pt` is fixed; `%` resolves against the container's
        // definite height; `auto`/`fr` resolve to auto here (`fr` is distributed
        // in a later pass once the auto rows' content heights are known).
        let row_track_h = |r: usize| -> Option<f64> {
            match style.grid_template_rows.get(r)? {
                TrackSize::Percent(pct) => avail_row_h.map(|a| a * pct / 100.0),
                t => track_fixed_height(t),
            }
        };

        // Lay each row's cells out, tracking the tallest cell so the whole row
        // shares one height. A row-spanning cell only contributes its height to
        // its LAST row (so earlier rows aren't inflated by it); single-row cells
        // size their row directly.
        let mut row_tops = vec![y_cursor; row_count];
        let mut row_bottoms = vec![y_cursor; row_count];
        // `self.out` index at which each row's single-row-cell fragments begin —
        // so a later `fr` pass can shift a whole row's content when the row grows.
        let mut row_out_start = vec![self.out.len(); row_count];
        // First pass: place content for single-row cells, sizing each row.
        for r in 0..row_count {
            row_out_start[r] = self.out.len();
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
                    true, // a table cell establishes a block formatting context
                );
                row_bottom = row_bottom.max(cy + ip.bottom + ib.bottom);
            }
            row_bottoms[r] = row_bottom.max(row_top);
        }
        // End of the last row's single-row fragments (spanning cells append after).
        let span_start = self.out.len();
        // Second pass: row-spanning cells. Lay them out within their first row's
        // top; if their content overflows the spanned rows, grow the last row so
        // the grid still contains them (and shift later rows down).
        // `(out range, first row)` per spanning cell, so the `fr` pass can shift
        // them with their first row.
        let mut span_ranges: Vec<(usize, usize, usize)> = Vec::new();
        for cell in placement.iter().filter(|p| p.row_span > 1) {
            let it = items[cell.item];
            let istyle = self.style_of(it, style, &na);
            let nca = push_ancestor(&na, it);
            let ip = &istyle.padding;
            let ib = &istyle.border_width;
            let (cx, cw) = span_x(cell.col, cell.col_span);
            let top = row_tops[cell.row];
            let span_out_start = self.out.len();
            let cy = self.block_children(
                &it.children,
                &istyle,
                cx + ip.left + ib.left,
                (cw - ip.left - ip.right - ib.left - ib.right).max(1.0),
                top + ip.top + ib.top,
                &nca,
                true, // a table cell establishes a block formatting context
            );
            span_ranges.push((span_out_start, self.out.len(), cell.row));
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

        // `fr` rows: distribute the grid's leftover *definite* height across the
        // `fr`-sized rows. The auto rows' content heights are known now, so the
        // free space is `avail − every non-fr row − gaps`. Growing an `fr` row
        // shifts every following row — and its already-placed content — down by
        // the accumulated extra height. No definite `avail` ⇒ no `fr` distribution
        // (those rows kept their content height above, the auto-grid default).
        if let Some(avail) = avail_row_h {
            let fr_factor = |r: usize| match style.grid_template_rows.get(r) {
                Some(TrackSize::Fr(f)) if *f > 0.0 => *f,
                _ => 0.0,
            };
            let fr_sum: f64 = (0..row_count).map(fr_factor).sum();
            if fr_sum > 0.0 {
                let gaps = gap_row * row_count.saturating_sub(1) as f64;
                let non_fr: f64 = (0..row_count)
                    .filter(|&r| fr_factor(r) == 0.0)
                    .map(|r| row_bottoms[r] - row_tops[r])
                    .sum();
                let free = (avail - non_fr - gaps).max(0.0);
                let mut shift = 0.0;
                for r in 0..row_count {
                    let content_h = row_bottoms[r] - row_tops[r];
                    // `fr` rows take their share but never shrink below content.
                    let new_h = if fr_factor(r) > 0.0 {
                        (free * fr_factor(r) / fr_sum).max(content_h)
                    } else {
                        content_h
                    };
                    if shift != 0.0 {
                        let end = row_out_start.get(r + 1).copied().unwrap_or(span_start);
                        for i in row_out_start[r]..end {
                            shift_fragment(&mut self.out[i].frag, 0.0, shift);
                        }
                        for &(s, e, row) in &span_ranges {
                            if row == r {
                                for i in s..e {
                                    shift_fragment(&mut self.out[i].frag, 0.0, shift);
                                }
                            }
                        }
                    }
                    row_tops[r] += shift;
                    row_bottoms[r] = row_tops[r] + new_h;
                    shift += new_h - content_h;
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
            let mut istyle = self.style_of(it, style, na);
            // `grid-area: <name>` → place into the parent's named template area.
            if let Some(name) = istyle.grid_area_name.clone() {
                if let Some(a) = style.grid_template_areas.iter().find(|a| a.name == name) {
                    istyle.grid_col_start = a.col + 1; // template areas are 0-based
                    istyle.grid_row_start = a.row + 1;
                    istyle.grid_col_span = a.col_span;
                    istyle.grid_row_span = a.row_span;
                }
            }
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
    /// Pagination interaction: filling is **page-sequential**. Content is split
    /// into page groups first (greedy: a page holds what its `column_count`
    /// columns can take between the block's top and the page-band bottom), so
    /// every column of a page fills before anything lands on the next page —
    /// column 1 never "continues" onto page 2 while column 2 of page 1 sits
    /// empty. The **last** page group is height-balanced exactly like the
    /// single-page case (which therefore keeps its historical behaviour); the
    /// earlier groups fill each column to the band bottom.
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
        let resolve_w = |len: Len| len.resolve(avail_w);
        // Box width, matching `block`'s sizing (incl. `box-sizing`/min/max).
        let mut box_w = match style.width {
            Some(Len::Pt(w)) if style.border_box => w,
            Some(Len::Pt(w)) => w + p.left + p.right + b.left + b.right,
            Some(Len::Percent(pc)) => avail_w * pc / 100.0,
            Some(Len::Calc { pt, pct }) => avail_w * pct / 100.0 + pt,
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
            let bottom = self.lay_unit(unit, style, content_x, col_w, content_top, &new_ancestors);
            self.out.truncate(probe_start);
            heights.push((bottom - content_top).max(0.0));
        }
        // ── Page split: greedily assign units to page groups. Each page offers
        // `n` columns of `cap` height (the current band's remaining height for
        // the first page, the full band afterwards); when all `n` columns of a
        // page are full the NEXT unit opens the next page. A unit taller than a
        // whole column still goes into a fresh column (and is sliced by the
        // y-band paginator like any oversized block). ──
        let content_h = (self.page_h - self.top - self.bottom).max(1.0);
        let first_band_bottom = {
            let page = ((content_top - self.top).max(0.0) / content_h) as usize;
            self.top + (page + 1) as f64 * content_h
        };
        // Page groups: unit indices + the group's column-area top + capacity.
        let mut groups: Vec<(Vec<usize>, f64, f64)> =
            vec![(Vec::new(), content_top, (first_band_bottom - content_top).max(1.0))];
        {
            let (mut col, mut col_y) = (0usize, 0.0_f64);
            for (i, uh) in heights.iter().enumerate() {
                while col_y > 0.05 && col_y + uh > groups.last().unwrap().2 + 0.05 {
                    col += 1;
                    col_y = 0.0;
                    if col >= n {
                        col = 0;
                        let (_, prev_top, prev_cap) = *groups.last().unwrap();
                        // The next band starts at the previous band's bottom
                        // (absolute coordinates keep growing across pages).
                        groups.push((Vec::new(), prev_top + prev_cap, content_h));
                    }
                }
                groups.last_mut().unwrap().0.push(i);
                col_y += uh;
            }
        }

        // ── Place pass, per page group: greedily fill columns to the group's
        // target. Non-last pages target the full band (columns fill to the page
        // bottom); the last page balances to `group total / n` — identical to
        // the historical single-page behaviour when there is only one group.
        // Degenerate units (height 0) never trigger a premature column break. ──
        let mut max_bottom = content_top;
        let last_gi = groups.len() - 1;
        for (gi, (idxs, group_top, cap)) in groups.iter().enumerate() {
            let target = if gi == last_gi {
                idxs.iter().map(|&i| heights[i]).sum::<f64>() / n as f64
            } else {
                *cap
            };
            let mut col = 0usize;
            let mut col_y = *group_top;
            for &i in idxs {
                let uh = heights[i];
                if col + 1 < n
                    && col_y > *group_top + 0.05
                    && uh > 0.0
                    && (col_y - *group_top) + uh > target + 0.05
                {
                    col += 1;
                    col_y = *group_top;
                }
                let bottom =
                    self.lay_unit(&units[i], style, col_x(col), col_w, col_y, &new_ancestors);
                col_y = bottom;
                max_bottom = max_bottom.max(col_y);
            }
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
                zk: Vec::new(),
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
                    zk: Vec::new(),
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
/// Build the placed fragment for an inline SVG box, honouring the image's
/// `preserveAspectRatio`: `meet` letterboxes the (uniformly scaled) viewBox
/// inside the CSS box, `slice` covers the box and clips the overflow via a
/// `Clipped` wrapper, `none` keeps the plain stretch. The painter stretches
/// the viewBox onto whatever rect the fragment carries, so the adjustment is
/// purely a matter of the target rect.
fn svg_fragment(
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    image: &SvgImage,
    par: crate::svg::PreserveAspectRatio,
) -> Fragment {
    let ((dx, dy, dw, dh), clip) =
        crate::svg::place_viewport(par, image.view_box[2], image.view_box[3], x, y, w, h);
    let svg = Fragment::Svg {
        x: dx,
        y: dy,
        w: dw,
        h: dh,
        image: image.clone(),
    };
    if clip {
        Fragment::Clipped {
            rect: [x, y, w, h],
            radius: [0.0; 4],
            radius_v: [0.0; 4],
            inner: Box::new(svg),
        }
    } else {
        svg
    }
}

/// background never lands on a text run here — only the run's own highlight does.
fn push_run_highlight(out: &mut Vec<Abs>, x: f64, y: f64, word: &Word) {
    let Some(fill) = word.style.background else {
        return;
    };
    let h = word.style.font_size * word.style.line_height.max(1.0);
    out.push(Abs {
        z: 0,
        zk: Vec::new(),
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

/// Like [`resolve_track_sizes`], but sizes `auto` tracks to their **content**:
/// each `auto` track's base is its measured max-content width (`content[i]`,
/// clamped to the grid's inner width) instead of an equal `1fr`-like share.
/// Free space then goes to the `fr` tracks; when the list has **no** `fr`
/// track, the leftover stretches the `auto` tracks equally (the CSS "stretch
/// auto tracks" step), so an all-`auto` grid still fills its container.
fn resolve_track_sizes_content(
    tracks: &[TrackSize],
    cols: usize,
    total: f64,
    gap: f64,
    content: &[f64],
) -> Vec<f64> {
    let cols = cols.max(1);
    let inner = (total - gap * cols.saturating_sub(1) as f64).max(0.0);
    if tracks.is_empty() {
        return vec![inner / cols as f64; cols];
    }
    let get = |i: usize| tracks.get(i).cloned().unwrap_or(TrackSize::Auto);

    let mut sizes = vec![0.0f64; cols];
    let mut fr_weight = vec![0.0f64; cols];
    let mut is_auto = vec![false; cols];
    let mut fixed_total = 0.0;
    let mut total_fr = 0.0;
    for i in 0..cols {
        match get(i) {
            TrackSize::Auto => {
                // Content-sized: the measured max-content, capped to the grid.
                sizes[i] = content.get(i).copied().unwrap_or(0.0).min(inner).max(0.0);
                is_auto[i] = true;
                fixed_total += sizes[i];
            }
            t => {
                let (base, fr) = track_base_and_fr(&t, inner);
                sizes[i] = base;
                fixed_total += base;
                if fr > 0.0 {
                    fr_weight[i] = fr;
                    total_fr += fr;
                }
            }
        }
    }

    let free = (inner - fixed_total).max(0.0);
    if total_fr > 0.0 {
        // Flexible tracks absorb every leftover point (auto tracks stay snug).
        for i in 0..cols {
            if fr_weight[i] > 0.0 {
                let mut add = free * fr_weight[i] / total_fr;
                if let TrackSize::MinMax(_, max) = get(i) {
                    if let Some(cap) = track_fixed_height(&max) {
                        add = add.min((cap - sizes[i]).max(0.0));
                    }
                }
                sizes[i] += add;
            }
        }
    } else if free > 0.0 {
        // No `fr` track: stretch the auto tracks equally (else every track).
        let autos = is_auto.iter().filter(|a| **a).count();
        if autos > 0 {
            let share = free / autos as f64;
            for i in 0..cols {
                if is_auto[i] {
                    sizes[i] += share;
                }
            }
        } else {
            let share = free / cols as f64;
            for s in sizes.iter_mut() {
                *s += share;
            }
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

/// Split a line into alternating non-space / space runs, both kept verbatim,
/// so `white-space: pre-wrap` text can wrap **between** runs while preserving
/// every source space (each run becomes its own measured word).
fn split_space_runs(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut in_space: Option<bool> = None;
    for (i, c) in s.char_indices() {
        let sp = c == ' ' || c == '\t';
        if in_space != Some(sp) {
            if i > start {
                out.push(&s[start..i]);
            }
            start = i;
            in_space = Some(sp);
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// Extra top-down offset aligning a run inside its line box for the
/// line-relative `vertical-align` values: `top` keeps the (default) top
/// placement, `middle` centres the run's own line box within the tallest one
/// on the line, `bottom` sinks it to the line bottom. Baseline / super / sub /
/// length runs return `0.0`, keeping their placement byte-identical.
fn inline_valign_delta(style: &Style, line_h: f64) -> f64 {
    let own = style.font_size * style.line_height.max(1.0);
    match style.valign {
        VShift::Middle => ((line_h - own) / 2.0).max(0.0),
        VShift::Bottom => (line_h - own).max(0.0),
        _ => 0.0,
    }
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

/// The table's `<tr>` rows in document order, plus a parallel flag marking the
/// rows that live inside a `<thead>` (so a multi-page table can repeat them as
/// a running header at the top of each page it spans).
fn collect_rows(table: &Element) -> (Vec<&Element>, Vec<bool>) {
    let mut rows = Vec::new();
    let mut in_thead = Vec::new();
    fn walk<'a>(el: &'a Element, thead: bool, rows: &mut Vec<&'a Element>, flags: &mut Vec<bool>) {
        for c in &el.children {
            if let Node::Element(e) = c {
                if e.tag == "tr" {
                    rows.push(e);
                    flags.push(thead);
                } else if matches!(e.tag.as_str(), "thead" | "tbody" | "tfoot") {
                    walk(e, e.tag == "thead", rows, flags);
                }
            }
        }
    }
    walk(table, false, &mut rows, &mut in_thead);
    (rows, in_thead)
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
    // Stacking order: hierarchical CSS stacking contexts first (the path
    // comparison keeps descendant z-indexes local to their context), then
    // backgrounds (z=0) before content (z=1) within a level. A stable sort
    // keeps insertion (document) order for equal keys.
    frags.sort_by(|a, b| cmp_stack(&a.zk, &b.zk).then(a.z.cmp(&b.z)));
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
            Fragment::Image {
                x,
                y,
                w,
                h,
                src,
                fit,
            } => {
                let p = page_of(y);
                ensure(&mut pages, p);
                pages[p].push(Fragment::Image {
                    x,
                    y: local_y(y, p),
                    w,
                    h,
                    src,
                    fit,
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
                radius_start,
                radius_end,
                far,
            } => {
                // Split the side's band across the page bands it spans, exactly
                // like `Rect`. The painter re-derives the dash run within each
                // segment, so a tall dashed/dotted side breaks cleanly at a page
                // boundary; a horizontal side never spans bands. When a vertical
                // band splits, its start-corner radius stays with the first
                // segment and its end-corner radius with the last (the corner
                // arcs live at the band's real ends).
                let mut top = y;
                let bottom = y + h;
                let first_top = top;
                while top < bottom {
                    let p = page_of(top);
                    let band_bottom = top + (p as f64 + 1.0) * content_h;
                    let seg_bottom = bottom.min(band_bottom);
                    ensure(&mut pages, p);
                    let is_first = top <= first_top;
                    let is_last = seg_bottom >= bottom - 0.0005;
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
                        radius_start: if horizontal || is_first {
                            radius_start
                        } else {
                            0.0
                        },
                        radius_end: if horizontal || is_last {
                            radius_end
                        } else {
                            0.0
                        },
                        far,
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
            Fragment::Clipped {
                rect,
                radius,
                radius_v,
                inner,
            } => {
                // Like Text/Image: the clipped content keeps to one page band
                // (assigned by its top); move the clip window and content into
                // that page's local coordinates together.
                let (_, fy0, _, _) = fragment_bbox(&inner);
                let p = page_of(fy0);
                ensure(&mut pages, p);
                let mut moved = Fragment::Clipped {
                    rect,
                    radius,
                    radius_v,
                    inner,
                };
                shift_fragment(&mut moved, 0.0, local_y(fy0, p) - fy0);
                pages[p].push(moved);
            }
            Fragment::Transformed { matrix, inner } => {
                // Assign by the TRANSFORMED box's top, then move into that page's
                // local band (`shift_fragment` conjugates the matrix to match).
                let (ix0, iy0, ix1, iy1) = fragment_bbox(&inner);
                let [_, b, _, d, _, f] = matrix;
                let ys = [
                    b * ix0 + d * iy0 + f,
                    b * ix1 + d * iy0 + f,
                    b * ix1 + d * iy1 + f,
                    b * ix0 + d * iy1 + f,
                ];
                let fy0 = ys.iter().copied().fold(f64::INFINITY, f64::min);
                let p = page_of(fy0);
                ensure(&mut pages, p);
                let mut moved = Fragment::Transformed { matrix, inner };
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
    fn sticky_header_repeats_on_each_page_of_its_container() {
        // A `position: sticky; top: 0` header inside a container taller than one
        // page reappears at the top of every page the container spans — the
        // paged-media running-header model (there is no scroll to stick against).
        let hdr_count = |layout: &Layout, needle: &str| -> Vec<usize> {
            layout
                .pages
                .iter()
                .map(|p| {
                    p.iter()
                        .filter(
                            |f| matches!(f, Fragment::Text { text, .. } if text.contains(needle)),
                        )
                        .count()
                })
                .collect()
        };
        let sticky = run(&format!(
            r#"<div><h2 style="position:sticky;top:0">STICKYHDR</h2>{}</div>"#,
            "<p>line</p>".repeat(120)
        ));
        assert!(
            sticky.pages.len() >= 2,
            "container spans pages ({})",
            sticky.pages.len()
        );
        let per_page = hdr_count(&sticky, "STICKYHDR");
        assert!(per_page[0] >= 1, "sticky header on page 1 ({per_page:?})");
        assert!(
            per_page[1] >= 1,
            "sticky header repeats on page 2 ({per_page:?})"
        );

        // A non-sticky header appears exactly once (only on its natural page).
        let plain = run(&format!(
            r#"<div><h2>PLAINHDR</h2>{}</div>"#,
            "<p>line</p>".repeat(120)
        ));
        let plain_pp = hdr_count(&plain, "PLAINHDR");
        let total: usize = plain_pp.iter().sum();
        assert_eq!(total, 1, "non-sticky header appears once ({plain_pp:?})");

        // A `bottom: 0` sticky footer at the END of the container reappears at the
        // bottom of every EARLIER page it spans (running footer), plus its natural
        // last page.
        let footer = run(&format!(
            r#"<div>{}<div style="position:sticky;bottom:0">FOOTERMK</div></div>"#,
            "<p>line</p>".repeat(120)
        ));
        let fp = hdr_count(&footer, "FOOTERMK");
        assert!(fp.len() >= 2, "footer container spans pages ({fp:?})");
        assert!(fp[0] >= 1, "footer repeats on the first page ({fp:?})");
        assert!(
            fp[fp.len() - 1] >= 1,
            "footer is on its natural last page ({fp:?})"
        );
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
        // The equal grid is pinned via <colgroup> — undeclared columns are
        // content-sized since the CSS auto-layout fix; the colspan summing
        // under test is orthogonal to how the column widths were chosen.
        let layout = run("<table>\
             <colgroup><col style=\"width:180pt\"><col style=\"width:180pt\"><col style=\"width:180pt\"></colgroup>\
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
        //
        // Columns are pinned to 270pt each via <colgroup> — undeclared columns
        // are now sized to their content (CSS auto table layout), so the test
        // declares the equal grid its position maths assume; the rowspan
        // occupation mechanics under test are unchanged.
        let layout = run("<table>\
             <colgroup><col style=\"width:270pt\"><col style=\"width:270pt\"></colgroup>\
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
        // The equal 180pt grid is pinned via <colgroup> — undeclared columns
        // are content-sized since the CSS auto-layout fix; the occupation
        // arithmetic under test is unaffected.
        let layout = run("<table>\
             <colgroup><col style=\"width:180pt\"><col style=\"width:180pt\"><col style=\"width:180pt\"></colgroup>\
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
    fn flex_column_basis_sets_item_height() {
        // In a column flex, `flex-basis` sets the item's main (vertical) size, so a
        // 100pt-basis item pushes the next item ~100pt down (vs ~1 line of content).
        let basis = run(
            r#"<div style="display:flex;flex-direction:column"><div style="flex-basis:100pt">A</div><div>B</div></div>"#,
        );
        let plain = run(
            r#"<div style="display:flex;flex-direction:column"><div>A</div><div>B</div></div>"#,
        );
        let pushed = cell_y(&basis, "B") - cell_y(&plain, "B");
        assert!(
            pushed > 60.0,
            "flex-basis 100pt makes A ~100pt tall ({pushed}pt)"
        );
    }

    #[test]
    fn flex_column_grow_fills_definite_height() {
        // A 300pt-tall column flex with one grow item: the grow item absorbs the
        // free height, pushing the second item toward the bottom of the container.
        let grow = run(
            r#"<div style="display:flex;flex-direction:column;height:300pt"><div style="flex-grow:1">A</div><div>B</div></div>"#,
        );
        let yb = cell_y(&grow, "B");
        assert!(
            yb > 36.0 + 200.0,
            "grow item expands so B sits near the 300pt container's bottom (yb={yb})"
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
        let layout = run(r#"<div style="display:grid;grid-template-columns:1fr 1fr">
                 <div style="grid-column:2">A</div><div>B</div>
               </div>"#);
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
        let layout = run(r#"<div style="display:grid;grid-template-columns:1fr 1fr">
                 <div>Early</div><div style="grid-row:2;grid-column:1">Late</div>
               </div>"#);
        let t = text_xy(&layout);
        let early = t.iter().find(|(_, _, s)| s == "Early").unwrap();
        let late = t.iter().find(|(_, _, s)| s == "Late").unwrap();
        assert!(
            late.1 > early.1,
            "grid-row:2 places 'Late' below 'Early' (early={early:?}, late={late:?})"
        );
        assert!((late.0 - early.0).abs() < 1.0, "both in column 1 (same x)");
    }

    #[test]
    fn grid_template_areas_place_items_by_name() {
        // Areas "a a b" / "a a c": `a` is the left 2×2 block, `b` top-right,
        // `c` bottom-right. Items are listed out of order to prove name-placement.
        let layout = run(
            "<div style=\"display:grid;grid-template-columns:1fr 1fr 1fr;\
                         grid-template-areas:'a a b' 'a a c'\">\
               <div style=\"grid-area:b\">B</div>\
               <div style=\"grid-area:a\">A</div>\
               <div style=\"grid-area:c\">C</div>\
             </div>",
        );
        let t = text_xy(&layout);
        let a = t.iter().find(|(_, _, s)| s == "A").unwrap();
        let b = t.iter().find(|(_, _, s)| s == "B").unwrap();
        let c = t.iter().find(|(_, _, s)| s == "C").unwrap();
        // `b`/`c` sit in the right column → right of `a`'s left column.
        assert!(
            b.0 > a.0,
            "B (area b) right of A (area a) (a={a:?}, b={b:?})"
        );
        assert!(c.0 > a.0, "C (area c) right of A (a={a:?}, c={c:?})");
        assert!((b.0 - c.0).abs() < 1.0, "B and C share the right column");
        // `c` is one row below `b`; `a` starts on the top row with `b`.
        assert!(c.1 > b.1, "C (row 2) below B (row 1) (b={b:?}, c={c:?})");
        assert!((a.1 - b.1).abs() < 1.0, "A and B start on the top row");
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
        let not_cleared = run(&format!("<div>{floated}<div>After</div></div>"));
        let y_of = |l: &Layout| {
            text_xy(l)
                .into_iter()
                .find(|(_, _, s)| s == "After")
                .unwrap()
                .1
        };
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
        let not_cleared = run(&format!("<div>{floated}<div>After</div></div>"));
        let y_of = |l: &Layout| {
            text_xy(l)
                .into_iter()
                .find(|(_, _, s)| s == "After")
                .unwrap()
                .1
        };
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
        let not_cleared = run(&format!("<div>{floated}<div>After</div></div>"));
        let y_of = |l: &Layout| {
            text_xy(l)
                .into_iter()
                .find(|(_, _, s)| s == "After")
                .unwrap()
                .1
        };
        assert!(
            y_of(&cleared) > y_of(&not_cleared) + 20.0,
            "clear:both drops below a right float (cleared={}, baseline={})",
            y_of(&cleared),
            y_of(&not_cleared)
        );
    }

    // Smallest background rect with a fill within `tol` of `want` (the float box
    // in the float tests carries a distinctive fill so we can read its width/x).
    fn smallest_filled_rect(layout: &Layout, want: [f64; 3]) -> (f64, f64, f64, f64) {
        let tol = 0.05;
        rects(layout)
            .into_iter()
            .filter(|(.., fill)| fill.is_some_and(|c| (0..3).all(|i| (c[i] - want[i]).abs() < tol)))
            .map(|(x, y, w, h, _)| (x, y, w, h))
            .min_by(|a, b| a.2.partial_cmp(&b.2).unwrap())
            .expect("a background rect with the wanted fill")
    }

    #[test]
    fn float_without_width_shrinks_to_content() {
        // A `float:left` with no explicit `width` must size to its content's
        // max-content width, NOT the legacy `avail_w / 3` (= 540/3 = 180pt).
        // "Hi" is 2 chars ≈ 2 × 8pt = 16pt under AverageMeasure (16pt / 0.5em).
        // A grey background makes the float's own box rect findable.
        let layout =
            run(r#"<div style="float:left;background:#cccccc">Hi</div><p>beside the float</p>"#);
        let (_, _, fw, _) = smallest_filled_rect(&layout, [0.8, 0.8, 0.8]);
        assert!(
            fw > 8.0 && fw < 80.0,
            "shrink-to-fit float sizes to its ~16pt content, not avail/3=180 ({fw})"
        );
        // The text that wraps beside it therefore starts close to the left edge
        // (content origin 36 + ~16 float), far left of the old 36 + 180.
        let beside_x = text_xy(&layout)
            .into_iter()
            .find(|(_, _, s)| s == "beside")
            .expect("the wrapping word")
            .0;
        assert!(
            beside_x < 120.0,
            "text wraps just past the narrow float, not past a 180pt box ({beside_x})"
        );
    }

    #[test]
    fn float_uses_intrinsic_width_for_replaced() {
        // A floated `<img width=40>` keeps its intrinsic 40pt width (replaced
        // content is not shrink-to-fit measured). With explicit `width=120` it
        // reserves a wider band. We read the band width through where the wrapping
        // text starts (content origin 36 + the float width).
        let narrow =
            run(r#"<img src="a.png" width="40" height="20" style="float:left"><p>beside</p>"#);
        let wide =
            run(r#"<img src="a.png" width="120" height="20" style="float:left"><p>beside</p>"#);
        let beside_x = |l: &Layout| {
            text_xy(l)
                .into_iter()
                .find(|(_, _, s)| s == "beside")
                .expect("the wrapping word")
                .0
        };
        let xn = beside_x(&narrow);
        let xw = beside_x(&wide);
        assert!(
            (xn - (36.0 + 40.0)).abs() < 6.0,
            "text wraps past the intrinsic 40pt image float (≈76, got {xn})"
        );
        assert!(
            (xw - (36.0 + 120.0)).abs() < 6.0,
            "a wider replaced float reserves its intrinsic 120pt (≈156, got {xw})"
        );
    }

    #[test]
    fn block_sibling_wraps_beside_a_float_then_clears_below() {
        // A left float (80pt) followed by a normal block, then a `clear:left`
        // block. The middle block's *box* must sit beside the float (x shifted
        // right by ~80, width reduced); the cleared block drops below the float.
        let layout = run(r#"<div>
                 <div style="float:left;width:80pt">F</div>
                 <div style="background:#eeeeee">B</div>
                 <div style="clear:left;background:#cccccc">C</div>
               </div>"#);
        // Content origin is the 36pt page margin.
        let (bx, by, bw, _) = smallest_filled_rect(&layout, [0.9333, 0.9333, 0.9333]);
        assert!(
            (bx - (36.0 + 80.0)).abs() < 4.0,
            "block sibling B starts beside the 80pt float (x≈116, got {bx})"
        );
        assert!(
            bw < 540.0 - 70.0,
            "block sibling B's width is reduced by the float inset ({bw})"
        );
        let (_, cy, ..) = smallest_filled_rect(&layout, [0.8, 0.8, 0.8]);
        assert!(
            cy > by + 5.0,
            "the clear:left block C drops below the float, under B (cy={cy}, by={by})"
        );
    }

    #[test]
    fn float_affects_next_in_flow_sibling_container_but_not_a_bfc() {
        // (c) A float inside an in-flow block container still narrows a *following
        // sibling container's* content (same formatting context). But when the
        // float's container is a NEW block formatting context (`overflow:hidden`),
        // the float is contained and the sibling is full width.
        let shared = run(r#"<div><div style="float:left;width:80pt">F</div></div>
               <p style="background:#eeeeee">After</p>"#);
        let bfc = run(
            r#"<div style="overflow:hidden"><div style="float:left;width:80pt">F</div></div>
               <p style="background:#eeeeee">After</p>"#,
        );
        let x_shared = smallest_filled_rect(&shared, [0.9333, 0.9333, 0.9333]).0;
        let x_bfc = smallest_filled_rect(&bfc, [0.9333, 0.9333, 0.9333]).0;
        assert!(
            x_shared > 36.0 + 40.0,
            "float persists to the next in-flow sibling, shifting 'After' right ({x_shared})"
        );
        assert!(
            (x_bfc - 36.0).abs() < 4.0,
            "an overflow:hidden BFC contains the float, so 'After' is full width ({x_bfc})"
        );
    }

    // ── multi-column (CSS column-count / columns / column-gap) ──

    #[test]
    fn column_count_splits_content_into_two_columns() {
        // Six paragraphs in a 2-column block: the first paragraphs fill the left
        // column (x near the content origin), the later ones spill into the right
        // column (a distinctly larger x). The right column starts at roughly the
        // column width + gutter further right than the left.
        let layout = run(r#"<div style="column-count:2">
                 <p>Alpha</p><p>Bravo</p><p>Charlie</p>
                 <p>Delta</p><p>Echo</p><p>Foxtrot</p>
               </div>"#);
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
        let wide = run(r#"<div style="column-count:2;column-gap:60pt">
                 <p>One</p><p>Two</p><p>Three</p><p>Four</p>
               </div>"#);
        let narrow = run(r#"<div style="column-count:2;column-gap:4pt">
                 <p>One</p><p>Two</p><p>Three</p><p>Four</p>
               </div>"#);
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
        let layout = run(r#"<div style="columns:3">
                 <p>A1</p><p>A2</p><p>A3</p>
                 <p>B1</p><p>B2</p><p>B3</p>
                 <p>C1</p><p>C2</p><p>C3</p>
               </div>"#);
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
            assert!(
                (*x - 36.0).abs() < 1.0,
                "stacked at the content origin ({x})"
            );
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
            assert!(
                (*x - 36.0).abs() < 1.0,
                "column-count:1 stays single-column ({x})"
            );
        }
    }

    #[test]
    fn columns_preserve_reading_order_left_then_right() {
        // The unit that fills the left column comes before, in document order,
        // the unit that opens the right column — i.e. the flow goes top-of-left
        // then top-of-right (newspaper columns), not interleaved.
        let layout = run(r#"<div style="column-count:2">
                 <p>First</p><p>Second</p><p>Third</p>
                 <p>Fourth</p><p>Fifth</p><p>Sixth</p>
               </div>"#);
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
        let layout = run("<div style=\"width:200pt;margin:0 auto;background:#eee\">centered</div>");
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
        let layout = run("<div style=\"margin:20pt 0;height:10pt\">A</div>\
             <div style=\"margin:20pt 0;height:10pt\">B</div>");
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
        let layout = run("<div style=\"height:700pt\">spacer</div>\
             <div style=\"page-break-inside:avoid;height:120pt\">keep</div>");
        assert!(layout.pages.len() >= 2, "content spans pages");
        // The whole `keep` block (its text) lands on page 2, not page 1.
        let on_p1 = layout.pages[0]
            .iter()
            .any(|f| matches!(f, Fragment::Text { text, .. } if text == "keep"));
        let on_p2 = layout
            .pages
            .get(1)
            .map(|p| {
                p.iter()
                    .any(|f| matches!(f, Fragment::Text { text, .. } if text == "keep"))
            })
            .unwrap_or(false);
        assert!(!on_p1, "avoid block does not start on page 1");
        assert!(on_p2, "avoid block moved whole to page 2");
    }

    #[test]
    fn page_break_inside_avoid_keeps_block_that_already_fits() {
        // A short `avoid` block that fits entirely on page 1 is NOT moved.
        let layout = run("<div style=\"height:50pt\">top</div>\
             <div style=\"page-break-inside:avoid;height:50pt\">keep</div>");
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
        assert!(
            (wide - 36.0).abs() < 2.0,
            "spanning cell starts at the left ({wide})"
        );
        assert!(
            (next - (36.0 + 360.0)).abs() < 4.0,
            "Next lands in column 3 after the 2-col span (~396), got {next}"
        );
    }

    #[test]
    fn grid_row_span_keeps_following_item_on_first_row() {
        // A row-spanning item in column 1 leaves column 2 of the first row free,
        // so the next item stays on row 1 (same y), to its right.
        let layout = run(r#"<div style="display:grid;grid-template-columns:1fr 1fr">
                 <div style="grid-row:span 2">Tall</div><div>Side</div><div>Below</div>
               </div>"#);
        let tall = text_xy(&layout)
            .into_iter()
            .find(|(_, _, s)| s == "Tall")
            .unwrap();
        let side = text_xy(&layout)
            .into_iter()
            .find(|(_, _, s)| s == "Side")
            .unwrap();
        let below = text_xy(&layout)
            .into_iter()
            .find(|(_, _, s)| s == "Below")
            .unwrap();
        assert!(side.0 > tall.0, "Side is right of the spanning Tall");
        assert!(
            (side.1 - tall.1).abs() < 2.0,
            "Side shares the first row with Tall"
        );
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

    #[test]
    fn grid_fr_rows_split_a_definite_height() {
        // A 400pt-tall grid with `1fr 3fr` rows: row 1 = 100pt, row 2 = 300pt, so
        // R2's content sits ~100pt (the 1fr row) below R1's.
        let layout = run(
            r#"<div style="display:grid;height:400pt;grid-template-columns:1fr;grid-template-rows:1fr 3fr"><div>R1</div><div>R2</div></div>"#,
        );
        let gap = cell_y(&layout, "R2") - cell_y(&layout, "R1");
        assert!(
            (gap - 100.0).abs() < 12.0,
            "1fr row ≈ 100pt of the 400pt grid (R2 is {gap}pt below R1)"
        );
    }

    #[test]
    fn grid_percent_rows_resolve_against_definite_height() {
        // `25% auto` rows in a 400pt grid: the first row is 100pt, so R2 sits
        // ~100pt below R1. `%` rows need the container's definite height.
        let layout = run(
            r#"<div style="display:grid;height:400pt;grid-template-columns:1fr;grid-template-rows:25% auto"><div>R1</div><div>R2</div></div>"#,
        );
        let gap = cell_y(&layout, "R2") - cell_y(&layout, "R1");
        assert!(
            (gap - 100.0).abs() < 12.0,
            "25% row ≈ 100pt of the 400pt grid (R2 is {gap}pt below R1)"
        );
    }

    #[test]
    fn aspect_ratio_derives_height_from_width() {
        // A 200pt-wide box with `aspect-ratio: 2/1` is 100pt tall, so a sibling
        // below it starts ~100pt down — far below the content-sized box (~1 line).
        let ar = run(r#"<div style="width:200pt;aspect-ratio:2/1">A</div><div>B</div>"#);
        let plain = run(r#"<div style="width:200pt">A</div><div>B</div>"#);
        let pushed = cell_y(&ar, "B") - cell_y(&plain, "B");
        assert!(
            pushed > 60.0,
            "aspect-ratio box (~100pt tall) pushes B well below the plain box ({pushed}pt)"
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
        assert!(
            b > 36.0 + 200.0,
            "but B is still past the midpoint area ({b})"
        );
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
        let layout = run(r#"<div style="display:flex;flex-wrap:wrap">
                 <div style="flex:0 0 250pt">One</div>
                 <div style="flex:0 0 250pt">Two</div>
                 <div style="flex:0 0 250pt">Three</div>
               </div>"#);
        let one = text_xy(&layout)
            .into_iter()
            .find(|(_, _, s)| s == "One")
            .unwrap();
        let three = text_xy(&layout)
            .into_iter()
            .find(|(_, _, s)| s == "Three")
            .unwrap();
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
        let t1 = text_xy(&layout)
            .into_iter()
            .find(|(_, _, s)| s == "t1")
            .unwrap();
        let short = text_xy(&layout)
            .into_iter()
            .find(|(_, _, s)| s == "short")
            .unwrap();
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
        // Two adjacent RTL (Hebrew) inline spans, logical order ALEF-word then
        // BET-word. Under the bidi reorder the first logical RTL box lands at the
        // right and the next to its left, so x(first) > x(second). (Latin words
        // would instead stay LTR — see `rtl_latin_words_keep_ltr_order`.) The LTR
        // control keeps source order: x(AAA) < x(BBB).
        let rtl = run(r#"<p dir="rtl"><span>אלף</span><span>בית</span></p>"#);
        let a = run_x(&rtl, "אלף");
        let b = run_x(&rtl, "בית");
        assert!(
            a > b,
            "RTL lays the first logical box to the right of the next (a={a}, b={b})"
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
        let x = run_x(
            &run(r#"<p dir="rtl" style="text-align:left">word</p>"#),
            "word",
        );
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

    // ── Unicode bidi (UAX #9) line-level reordering ─────────────────────────

    #[test]
    fn bidi_pure_rtl_line_reverses_word_order() {
        // Three Arabic words in source order W1 W2 W3. The base is RTL, so the
        // whole line mirrors: the first logical word lands rightmost, the last
        // leftmost → x(W1) > x(W2) > x(W3).
        let layout = run(r#"<p dir="rtl">مرحبا بالعالم اليوم</p>"#);
        let x1 = run_x(&layout, "مرحبا");
        let x2 = run_x(&layout, "بالعالم");
        let x3 = run_x(&layout, "اليوم");
        assert!(
            x1 > x2 && x2 > x3,
            "pure-RTL line places logical order right-to-left (x1={x1}, x2={x2}, x3={x3})"
        );
    }

    #[test]
    fn bidi_rtl_with_embedded_latin_word_keeps_it_between_runs() {
        // RTL line with an embedded Latin word: "Arabic ABC Arabic". The Arabic
        // runs mirror (first logical word rightmost) but the Latin word stays at
        // its own level, landing between them — not naively reversed to an edge.
        let layout = run(r#"<p dir="rtl">مرحبا ABC جميل</p>"#);
        let a1 = run_x(&layout, "مرحبا");
        let lat = run_x(&layout, "ABC");
        let a2 = run_x(&layout, "جميل");
        assert!(
            a1 > lat && lat > a2,
            "embedded Latin word sits between the mirrored Arabic runs (a1={a1}, ABC={lat}, a2={a2})"
        );
    }

    #[test]
    fn bidi_rtl_with_two_latin_words_keeps_their_ltr_order() {
        // Two adjacent Latin words inside an RTL line read left-to-right among
        // themselves (their level-2 sub-run is reversed back by L2), even though
        // the surrounding Arabic is mirrored: x(ONE) < x(TWO).
        let layout = run(r#"<p dir="rtl">عرب ONE TWO نص</p>"#);
        let one = run_x(&layout, "ONE");
        let two = run_x(&layout, "TWO");
        let a_first = run_x(&layout, "عرب");
        let a_last = run_x(&layout, "نص");
        assert!(
            one < two,
            "embedded Latin words read LTR (ONE={one}, TWO={two})"
        );
        assert!(
            a_first > two && a_last < one,
            "Arabic frames the Latin run, first logical word rightmost (first={a_first}, last={a_last})"
        );
    }

    #[test]
    fn bidi_rtl_with_european_numbers_read_ltr() {
        // Each number token resolves to an LTR sub-level (W/I rules) and stays
        // an intact run — its digits are never mirrored. Between the two
        // tokens, though, sits a real space: under UAX #9 resolved at character
        // level a space flanked by numbers takes the RTL base level (N1 — EN/AN
        // act as R), so the two SEPARATE tokens flow right-to-left like any
        // other RTL words: 12 lands right of 34 (the classic bidi phone-number
        // ordering, matching browsers). The old word-granularity model dropped
        // the inter-word spaces from the algorithm, gluing both numbers into
        // one LTR run — that ordering (12 left of 34) was not spec-correct.
        let layout = run(r#"<p dir="rtl">عرب 12 34 نص</p>"#);
        let n1 = run_x(&layout, "12");
        let n2 = run_x(&layout, "34");
        assert!(
            n2 < n1,
            "separate number tokens flow RTL, digits intact (34={n2} left of 12={n1})"
        );
        // Both frame words mirror around them: first logical Arabic word
        // rightmost, last leftmost.
        let a_first = run_x(&layout, "عرب");
        let a_last = run_x(&layout, "نص");
        assert!(
            a_first > n1 && a_last < n2,
            "Arabic frames the numbers (first={a_first}, last={a_last})"
        );
    }

    #[test]
    fn bidi_ltr_base_with_embedded_hebrew_reverses_only_hebrew() {
        // LTR paragraph with an embedded Hebrew pair: "the H1 H2 end". The Latin
        // words keep their order; the two Hebrew words form an RTL sub-run that
        // reverses among themselves → x(the) < x(H2) < x(H1) < x(end).
        let layout = run(r#"<p>the שלום עולם end</p>"#);
        let the = run_x(&layout, "the");
        let h1 = run_x(&layout, "שלום");
        let h2 = run_x(&layout, "עולם");
        let end = run_x(&layout, "end");
        assert!(
            the < h2 && h2 < h1 && h1 < end,
            "only the Hebrew sub-run reverses (the={the}, H1={h1}, H2={h2}, end={end})"
        );
    }

    #[test]
    fn bidi_rtl_latin_only_stays_ltr_and_right_aligned() {
        // A line of only Latin words inside an RTL block reads left-to-right
        // (no odd level present ⇒ no reversal) but is right-aligned: AAA stays
        // left of BBB, and the block hugs the right edge.
        let layout = run(r#"<p dir="rtl">AAA BBB</p>"#);
        let a = run_x(&layout, "AAA");
        let b = run_x(&layout, "BBB");
        assert!(
            a < b,
            "Latin-only RTL keeps source order (AAA={a}, BBB={b})"
        );
        // Right-aligned: BBB's right edge ≈ 576. BBB = 3 glyphs × 8pt = 24pt wide
        // ⇒ its left corner ≈ 552, far past the left content edge (36).
        assert!(
            b > 36.0 + 400.0,
            "Latin-only RTL block is right-aligned (BBB={b})"
        );
    }

    #[test]
    fn bidi_neutral_between_opposite_strongs_takes_base_direction() {
        // LTR base: "abc / שלום" — the slash token is a neutral flanked by L
        // (left) and R (right). N2 resolves it to the base (LTR) direction, so it
        // stays with the Latin side: x(abc) < x(/) and the Hebrew word follows to
        // the right of the slash.
        let layout = run(r#"<p>abc / שלום</p>"#);
        let abc = run_x(&layout, "abc");
        let slash = run_x(&layout, "/");
        let heb = run_x(&layout, "שלום");
        assert!(
            abc < slash && slash < heb,
            "neutral takes base (LTR) direction between opposite strongs (abc={abc}, /={slash}, heb={heb})"
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
        let layout =
            run(r#"<p><mark>hi</mark> <span style="background-color:#00ff00">x</span></p>"#);
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
        assert_eq!(
            blue, 1,
            "exactly one blue rect (the box), no text duplicate"
        );
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
        let layout = run(r#"<div style="border:2pt solid #000000;width:100pt;height:40pt"></div>"#);
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
        let layout =
            run(r#"<div style="border-bottom:3pt dotted #000000;width:90pt;height:30pt"></div>"#);
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
        let layout =
            run(r#"<div style="background:#3366cc;border-radius:12pt / 4pt;padding:6pt">x</div>"#);
        let bg = rect_radii(&layout)
            .into_iter()
            .find(|(fill, ..)| *fill == Some([0.2, 0.4, 0.8]))
            .expect("the blue rounded rect");
        assert!(
            bg.1.iter().all(|r| (*r - 12.0).abs() < 0.01),
            "h=12: {:?}",
            bg.1
        );
        assert!(
            bg.2.iter().all(|r| (*r - 4.0).abs() < 0.01),
            "v=4: {:?}",
            bg.2
        );
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
            rects
                .iter()
                .any(|(fill, _, _, shadow)| fill.is_some() && *shadow),
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
        assert!(
            sticky_y >= 36.0,
            "still within/after the container top (y={sticky_y})"
        );
    }

    #[test]
    fn sticky_with_room_applies_the_offset() {
        // With slack inside the container, a modest sticky offset DOES move the
        // box (it behaves like relative until it hits the container edge).
        let base = run(r#"<div style="height:200pt"><p style="margin:0">x</p></div>"#);
        let shifted = run(
            r#"<div style="height:200pt"><p style="position:sticky;top:20pt;margin:0">x</p></div>"#,
        );
        let y0 = text_xy(&base)
            .into_iter()
            .find(|(_, _, s)| s == "x")
            .unwrap()
            .1;
        let y1 = text_xy(&shifted)
            .into_iter()
            .find(|(_, _, s)| s == "x")
            .unwrap()
            .1;
        assert!(
            (y1 - y0 - 20.0).abs() < 0.5,
            "sticky with room shifts by ~20pt (y0={y0}, y1={y1})"
        );
    }

    // ── flex-wrap with intrinsic (auto) item widths ─────────────────────────

    #[test]
    fn flex_wrap_breaks_intrinsic_width_items_onto_lines() {
        // Six auto-width text items (~96pt max-content each with the average
        // measure) inside a 300pt wrap container: their intrinsic widths
        // overflow the row, so the items must break onto several flex lines —
        // previously wrap only ever triggered with explicit widths.
        let items: String = (1..=6)
            .map(|i| format!("<div>AAAAAAAAAAAAAAA{i}</div>"))
            .collect();
        let layout = run(&format!(
            r#"<div style="display:flex;flex-wrap:wrap;width:300pt">{items}</div>"#
        ));
        let y_of = |label: &str| {
            text_xy(&layout)
                .into_iter()
                .find(|(_, _, s)| s == label)
                .unwrap_or_else(|| panic!("missing {label}"))
                .1
        };
        let y1 = y_of("AAAAAAAAAAAAAAA1");
        let y6 = y_of("AAAAAAAAAAAAAAA6");
        assert!(
            y6 > y1 + 5.0,
            "intrinsic-width items wrap onto a later line (y1={y1}, y6={y6})"
        );
        // And items on the same line stay side by side (not all stacked).
        let distinct_ys: std::collections::BTreeSet<i64> = (1..=6)
            .map(|i| (y_of(&format!("AAAAAAAAAAAAAAA{i}")) * 10.0) as i64)
            .collect();
        assert!(
            distinct_ys.len() >= 2 && distinct_ys.len() < 6,
            "several lines, several items per line ({distinct_ys:?})"
        );
    }

    // ── position: fixed repeats on every page ───────────────────────────────

    #[test]
    fn fixed_banner_repeats_on_every_page() {
        // A `position: fixed` banner is viewport-attached: in paged output it
        // must appear on EVERY page of the document, at the same page-local
        // spot — not only on page 1.
        let layout = run(&format!(
            r#"<div style="position:fixed;top:0;left:0">FIXEDBANNER</div>{}"#,
            "<p>line</p>".repeat(150)
        ));
        assert!(
            layout.pages.len() >= 3,
            "document spans at least 3 pages ({})",
            layout.pages.len()
        );
        for (pi, page) in layout.pages.iter().enumerate() {
            let n = page
                .iter()
                .filter(|f| matches!(f, Fragment::Text { text, .. } if text == "FIXEDBANNER"))
                .count();
            assert!(n >= 1, "fixed banner present on page {pi} (count={n})");
        }
        // Same page-local y on page 1 and page 2.
        let banner_y = |pi: usize| {
            layout.pages[pi]
                .iter()
                .find_map(|f| match f {
                    Fragment::Text { y, text, .. } if text == "FIXEDBANNER" => Some(*y),
                    _ => None,
                })
                .unwrap()
        };
        assert!(
            (banner_y(0) - banner_y(1)).abs() < 0.01,
            "banner sits at the same page-local y ({} vs {})",
            banner_y(0),
            banner_y(1)
        );
    }

    // ── <thead> repeats at the top of each page of a broken table ───────────

    #[test]
    fn thead_repeats_on_each_page_of_a_multipage_table() {
        let body: String = (0..60)
            .map(|i| format!("<tr><td>rowa{i}</td><td>rowb{i}</td></tr>"))
            .collect();
        let layout = run(&format!(
            "<table><thead><tr><th>HCOLA</th><th>HCOLB</th></tr></thead><tbody>{body}</tbody></table>"
        ));
        assert!(
            layout.pages.len() >= 2,
            "table spans pages ({})",
            layout.pages.len()
        );
        for (pi, page) in layout.pages.iter().enumerate() {
            let has_body_row = page
                .iter()
                .any(|f| matches!(f, Fragment::Text { text, .. } if text.starts_with("rowa")));
            if !has_body_row {
                continue;
            }
            let hdr = page
                .iter()
                .filter(|f| matches!(f, Fragment::Text { text, .. } if text == "HCOLA"))
                .count();
            assert!(hdr >= 1, "thead header repeated on page {pi}");
            // The repeated header sits ABOVE every body row on that page.
            let hdr_y = page
                .iter()
                .find_map(|f| match f {
                    Fragment::Text { y, text, .. } if text == "HCOLA" => Some(*y),
                    _ => None,
                })
                .unwrap();
            let min_row_y = page
                .iter()
                .filter_map(|f| match f {
                    Fragment::Text { y, text, .. } if text.starts_with("rowa") => Some(*y),
                    _ => None,
                })
                .fold(f64::INFINITY, f64::min);
            assert!(
                hdr_y < min_row_y,
                "header above the body rows on page {pi} (hdr={hdr_y}, row={min_row_y})"
            );
        }
    }

    // ── table auto layout: undeclared columns size to content ───────────────

    #[test]
    fn table_undeclared_columns_size_to_content() {
        // CSS auto table layout: with no declared widths, a column of long text
        // gets proportionally more room than a tiny one — and nothing overflows
        // the table band (columns still sum to the available width).
        let layout = run(
            "<table><tr><td>averyveryverylongcelltextwithlotsofcontent inside</td><td>ok</td></tr></table>",
        );
        let ok_x = cell_x(&layout, "ok");
        // Equal columns would put "ok" at ~36+270+3=309; content-proportional
        // sizing pushes it far right (the long column takes most of the 540pt).
        assert!(
            ok_x > 36.0 + 400.0,
            "long-text column got the lion's share (ok at {ok_x})"
        );
        assert!(
            ok_x < 36.0 + 540.0,
            "the short column still fits inside the table band ({ok_x})"
        );
    }

    #[test]
    fn table_undeclared_columns_floor_at_min_content() {
        // The long column's max-content dwarfs the short one, but the short
        // column may not collapse below its widest unbreakable token: its cell
        // text must still start inside the page band and render.
        let long = "word ".repeat(80);
        let layout = run(&format!(
            "<table><tr><td>{long}</td><td>unbreakabletoken</td></tr></table>"
        ));
        let x = cell_x(&layout, "unbreakabletoken");
        // min-content of the 16-char token ≈ 16×6 + padding ≈ 102pt ⇒ the
        // column starts no further right than 540-96 side; generously assert it
        // stays on the page and the token is rendered whole (not clipped away).
        assert!(
            x < 36.0 + 540.0 - 90.0,
            "short column keeps at least its min-content ({x})"
        );
    }

    // ── <img> CSS width/height/max-width ────────────────────────────────────

    /// First raster image fragment as `(w, h)`.
    fn img_size(layout: &Layout) -> (f64, f64) {
        layout
            .pages
            .iter()
            .flatten()
            .find_map(|f| match f {
                Fragment::Image { w, h, .. } => Some((*w, *h)),
                _ => None,
            })
            .expect("an image fragment")
    }

    #[test]
    fn img_css_width_scales_height_by_ratio() {
        // 400×200 intrinsic, CSS width 100pt ⇒ rendered 100×50 (ratio kept).
        let layout =
            run(r#"<p><img src="x.png" width="400" height="200" style="width:100pt"></p>"#);
        let (w, h) = img_size(&layout);
        assert!((w - 100.0).abs() < 0.01, "CSS width applied ({w})");
        assert!((h - 50.0).abs() < 0.01, "height follows the ratio ({h})");
    }

    #[test]
    fn img_css_max_width_clamps_and_keeps_ratio() {
        let layout =
            run(r#"<p><img src="x.png" width="400" height="200" style="max-width:80pt"></p>"#);
        let (w, h) = img_size(&layout);
        assert!((w - 80.0).abs() < 0.01, "max-width clamps ({w})");
        assert!((h - 40.0).abs() < 0.01, "height follows the clamp ({h})");
    }

    #[test]
    fn img_attribute_size_unchanged_without_css() {
        // No CSS sizing ⇒ the width/height attributes rule, exactly as before.
        let layout = run(r#"<p><img src="x.png" width="120" height="90"></p>"#);
        let (w, h) = img_size(&layout);
        assert!((w - 120.0).abs() < 0.01 && (h - 90.0).abs() < 0.01);
    }

    // ── parent/child margin collapsing ──────────────────────────────────────

    #[test]
    fn parent_and_first_child_top_margins_collapse() {
        // No padding/border between the div's top and its first <p>: the two
        // top margins collapse to max(10, 20) = 20 ⇒ the text sits at 36+20=56
        // (not 36+10+20=66), and the div's background hugs the child.
        let layout = run(
            r#"<div style="margin-top:10pt;background:#eeeeee"><p style="margin:20pt 0 0 0">TXT</p></div>"#,
        );
        let ty = text_xy(&layout)
            .into_iter()
            .find(|(_, _, s)| s == "TXT")
            .unwrap()
            .1;
        assert!(
            (ty - 56.0).abs() < 0.5,
            "collapsed margin = max(10,20) puts the text at ~56 ({ty})"
        );
        let bg_y = layout.pages[0]
            .iter()
            .find_map(|f| match f {
                Fragment::Rect {
                    y, fill: Some(_), ..
                } => Some(*y),
                _ => None,
            })
            .expect("the div background");
        assert!(
            (bg_y - 56.0).abs() < 0.5,
            "the box top coincides with the child's (bg at {bg_y})"
        );
    }

    #[test]
    fn padding_blocks_parent_child_margin_collapsing() {
        // 5pt of top padding separates the margins: they stack (10+5+20=35).
        let layout = run(
            r#"<div style="margin-top:10pt;padding-top:5pt"><p style="margin:20pt 0 0 0">TXT2</p></div>"#,
        );
        let ty = text_xy(&layout)
            .into_iter()
            .find(|(_, _, s)| s == "TXT2")
            .unwrap()
            .1;
        assert!(
            (ty - 71.0).abs() < 0.5,
            "padding keeps both margins (36+10+5+20=71, got {ty})"
        );
    }

    #[test]
    fn parent_and_last_child_bottom_margins_collapse() {
        // The last child's 20pt bottom margin escapes the (margin-less) div and
        // collapses with the div's own 0 ⇒ the following block sits exactly
        // where it would if the DIV itself carried margin-bottom:20pt.
        let escaped = run(
            r#"<div style="background:#eeeeee"><p style="margin:0 0 20pt 0">TB</p></div><p style="margin:0">AFTER</p>"#,
        );
        let control = run(
            r#"<div style="background:#eeeeee;margin-bottom:20pt"><p style="margin:0">TB</p></div><p style="margin:0">AFTER</p>"#,
        );
        let y_of = |l: &Layout, label: &str| {
            text_xy(l)
                .into_iter()
                .find(|(_, _, s)| s == label)
                .unwrap()
                .1
        };
        assert!(
            (y_of(&escaped, "AFTER") - y_of(&control, "AFTER")).abs() < 0.5,
            "escaped last-child margin ≡ the parent's own bottom margin ({} vs {})",
            y_of(&escaped, "AFTER"),
            y_of(&control, "AFTER")
        );
    }

    // ── grid `auto` track sizes to content ──────────────────────────────────

    #[test]
    fn grid_auto_track_hugs_its_content() {
        // `auto 1fr` with a tiny label: the first column shrinks to the label's
        // max-content width (~12pt) instead of an equal 1fr-like half, so the
        // second item starts far left of the 270pt midpoint.
        let layout = run(
            r#"<div style="display:grid;grid-template-columns:auto 1fr"><div>ab</div><div>wide</div></div>"#,
        );
        let wide_x = text_xy(&layout)
            .into_iter()
            .find(|(_, _, s)| s == "wide")
            .unwrap()
            .0;
        assert!(
            wide_x < 36.0 + 60.0,
            "auto column hugs its ~12pt content; the 1fr item starts near the left ({wide_x})"
        );
    }

    // ── <caption> renders above the table ───────────────────────────────────

    #[test]
    fn table_caption_renders_above_the_grid() {
        let layout = run("<table><caption>CAPT</caption><tr><td>CellA</td></tr></table>");
        let y_capt = text_xy(&layout)
            .into_iter()
            .find(|(_, _, s)| s == "CAPT")
            .expect("caption text is rendered")
            .1;
        let y_cell = text_xy(&layout)
            .into_iter()
            .find(|(_, _, s)| s == "CellA")
            .unwrap()
            .1;
        assert!(
            y_capt < y_cell,
            "caption sits above the first row (caption={y_capt}, cell={y_cell})"
        );
    }

    // ── bidi: line-level UAX #9 keeps punctuation on the right side ─────────

    #[test]
    fn bidi_trailing_punctuation_splits_to_the_base_side() {
        // RTL base: "שלום, world!". Per UAX #9 resolved at character level, the
        // comma stays glued to the Hebrew word (both level 1) while `world!`
        // splits: `world` is an LTR (level-2) run but its `!` resolves to the
        // RTL base level, so it must land LEFT of `world` — not stay stuck on
        // its right as the old first-char-per-token model rendered it.
        let layout = run(r#"<p dir="rtl">שלום, world!</p>"#);
        let runs = text_runs(&layout);
        assert!(
            runs.iter().any(|t| t == "שלום,"),
            "comma stays attached to the Hebrew word ({runs:?})"
        );
        assert!(
            !runs.iter().any(|t| t == "world!"),
            "the mixed token splits at its level boundary ({runs:?})"
        );
        let x_bang = run_x(&layout, "!");
        let x_world = run_x(&layout, "world");
        let x_heb = run_x(&layout, "שלום,");
        assert!(
            x_bang < x_world,
            "the `!` lands on the RTL side of `world` (!={x_bang}, world={x_world})"
        );
        assert!(
            x_world < x_heb,
            "the Hebrew run stays rightmost (world={x_world}, heb={x_heb})"
        );
    }

    // ── dashed border + border-radius: bands stop at the corner arcs ────────

    #[test]
    #[allow(clippy::type_complexity)]
    fn dashed_border_with_radius_shortens_bands_and_carries_radii() {
        let layout = run(
            r#"<div style="border:2pt dashed #000000;border-radius:8pt;width:100pt;height:40pt"></div>"#,
        );
        let bands: Vec<(f64, f64, f64, f64, bool, f64, f64, bool)> = layout
            .pages
            .iter()
            .flatten()
            .filter_map(|f| match f {
                Fragment::Border {
                    x,
                    y,
                    w,
                    h,
                    horizontal,
                    radius_start,
                    radius_end,
                    far,
                    ..
                } => Some((
                    *x,
                    *y,
                    *w,
                    *h,
                    *horizontal,
                    *radius_start,
                    *radius_end,
                    *far,
                )),
                _ => None,
            })
            .collect();
        assert_eq!(bands.len(), 4, "four styled sides ({bands:?})");
        // Box: x=36, content 100pt + 2×2pt border ⇒ outer w=104. Top band is
        // inset by the 8pt corner radius on both ends: x=44, w=88.
        let top = bands
            .iter()
            .find(|b| b.4 && !b.7)
            .expect("top horizontal band");
        assert!(
            (top.0 - 44.0).abs() < 0.01 && (top.2 - 88.0).abs() < 0.01,
            "top band inset by the corner radius (x={}, w={})",
            top.0,
            top.2
        );
        assert!(
            (top.5 - 8.0).abs() < 0.01 && (top.6 - 8.0).abs() < 0.01,
            "the band carries its corner radii ({top:?})"
        );
        // The bottom band is marked `far` so its arcs curve upward.
        assert!(
            bands.iter().any(|b| b.4 && b.7),
            "bottom band flagged far ({bands:?})"
        );
        // Square-cornered dashed borders are unchanged: zero radii everywhere.
        let square =
            run(r#"<div style="border:2pt dashed #000000;width:100pt;height:40pt"></div>"#);
        let all_zero = square
            .pages
            .iter()
            .flatten()
            .filter_map(|f| match f {
                Fragment::Border {
                    radius_start,
                    radius_end,
                    ..
                } => Some(*radius_start == 0.0 && *radius_end == 0.0),
                _ => None,
            })
            .all(|z| z);
        assert!(all_zero, "no radius ⇒ byte-identical square bands");
    }

    // ── `white-space` / overflow-word / vertical-align / max-height / vw-vh ──

    /// The distinct text-fragment baselines (rounded Y positions) on page 1 —
    /// i.e. the number of laid-out lines.
    fn line_ys(layout: &Layout) -> Vec<i64> {
        let mut ys: Vec<i64> = layout.pages[0]
            .iter()
            .filter_map(|f| match f {
                Fragment::Text { y, .. } => Some((*y * 100.0).round() as i64),
                _ => None,
            })
            .collect();
        ys.sort_unstable();
        ys.dedup();
        ys
    }

    #[test]
    fn nowrap_disables_line_breaking() {
        let long = "word ".repeat(60);
        let wrapped = run(&format!("<p>{long}</p>"));
        assert!(line_ys(&wrapped).len() > 1, "normal text wraps");
        let nowrap = run(&format!("<p style=\"white-space:nowrap\">{long}</p>"));
        assert_eq!(
            line_ys(&nowrap).len(),
            1,
            "nowrap keeps the run on a single (overflowing) line"
        );
    }

    #[test]
    fn pre_line_collapses_spaces_but_keeps_newlines() {
        let layout = run("<div style=\"white-space:pre-line\">a    b\nc</div>");
        assert_eq!(line_ys(&layout).len(), 2, "the newline forces a 2nd line");
        let texts: Vec<String> = layout.pages[0]
            .iter()
            .filter_map(|f| match f {
                Fragment::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            texts.iter().all(|t| !t.contains("    ")),
            "space runs collapsed: {texts:?}"
        );
    }

    #[test]
    fn pre_wrap_preserves_spaces_and_still_wraps() {
        // One very long space-separated line: `pre` keeps it on ONE line
        // (overflowing); `pre-wrap` wraps it.
        let long = "word ".repeat(60);
        let pre = run(&format!("<div style=\"white-space:pre\">{long}</div>"));
        assert_eq!(line_ys(&pre).len(), 1, "pre never wraps");
        let prew = run(&format!("<div style=\"white-space:pre-wrap\">{long}</div>"));
        assert!(line_ys(&prew).len() > 1, "pre-wrap wraps the long line");
        // And a space RUN survives as its own token (preserved, not collapsed).
        let spaced = run("<div style=\"white-space:pre-wrap\">a    b</div>");
        let has_space_run = spaced.pages[0].iter().any(
            |f| matches!(f, Fragment::Text { text, .. } if text.chars().all(|c| c == ' ') && text.len() >= 4),
        );
        assert!(has_space_run, "the 4-space run is kept verbatim");
    }

    #[test]
    fn break_word_splits_an_overlong_word() {
        let giant = "x".repeat(400); // ≫ one line at any measure
        let plain = run(&format!("<p>{giant}</p>"));
        assert_eq!(
            line_ys(&plain).len(),
            1,
            "an unbreakable word overflows on one line by default"
        );
        let broken = run(&format!(
            "<p style=\"overflow-wrap:break-word\">{giant}</p>"
        ));
        assert!(
            line_ys(&broken).len() > 1,
            "break-word wraps the word mid-characters ({} lines)",
            line_ys(&broken).len()
        );
        // `word-break: break-all` behaves the same for the overlong word.
        let all = run(&format!("<p style=\"word-break:break-all\">{giant}</p>"));
        assert!(line_ys(&all).len() > 1, "break-all splits too");
    }

    #[test]
    fn text_overflow_ellipsis_truncates_nowrap_clipped_text() {
        let long = "abcdefghij ".repeat(40);
        let layout = run(&format!(
            "<div style=\"overflow:hidden;white-space:nowrap;text-overflow:ellipsis\">{long}</div>"
        ));
        let texts: Vec<&String> = layout.pages[0]
            .iter()
            .filter_map(|f| match f {
                Fragment::Text { text, .. } => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(texts.len(), 1, "single truncated run");
        assert!(texts[0].ends_with('\u{2026}'), "ends with …: {:?}", texts[0]);
        // The truncated run fits the content box (page 612 − 2×36 margins).
        let w = layout.pages[0]
            .iter()
            .find_map(|f| match f {
                Fragment::Text { w, .. } => Some(*w),
                _ => None,
            })
            .unwrap();
        assert!(w <= 540.0 + 1e-6, "truncated to the line budget: {w}");
    }

    #[test]
    fn vertical_align_middle_and_bottom_offset_the_run() {
        // A big run sets the line box; the small `middle` run sinks halfway,
        // `bottom` sinks fully, `top` stays at the line top.
        let html = |va: &str| {
            format!(
                "<p style=\"font-size:20pt;line-height:1\">BIG\
                 <span style=\"font-size:10pt;line-height:1;vertical-align:{va}\">s</span></p>"
            )
        };
        let y_of = |layout: &Layout, needle: &str| -> f64 {
            layout.pages[0]
                .iter()
                .find_map(|f| match f {
                    Fragment::Text { y, text, .. } if text.contains(needle) => Some(*y),
                    _ => None,
                })
                .expect("run present")
        };
        let mid = run(&html("middle"));
        let (big_y, small_y) = (y_of(&mid, "BIG"), y_of(&mid, "s"));
        assert!(
            (small_y - big_y - 5.0).abs() < 1e-6,
            "middle: (20 − 10) / 2 = 5pt below the line top, got {}",
            small_y - big_y
        );
        let bot = run(&html("bottom"));
        assert!(
            (y_of(&bot, "s") - y_of(&bot, "BIG") - 10.0).abs() < 1e-6,
            "bottom: 20 − 10 = 10pt below the line top"
        );
        let top = run(&html("top"));
        assert!(
            (y_of(&top, "s") - y_of(&top, "BIG")).abs() < 1e-6,
            "top: no offset (the default placement)"
        );
    }

    #[test]
    fn max_height_clamps_the_box_height() {
        let layout = run(
            "<div style=\"background:#eee;max-height:30pt\">\
             <p>a</p><p>b</p><p>c</p><p>d</p><p>e</p></div>",
        );
        let rect_h = layout.pages[0]
            .iter()
            .find_map(|f| match f {
                Fragment::Rect { h, .. } => Some(*h),
                _ => None,
            })
            .expect("background rect");
        assert!(
            (rect_h - 30.0).abs() < 1e-6,
            "box height clamped to max-height, got {rect_h}"
        );
        // Content overflows visibly by default: text fragments still exist
        // below the clamped box.
        let text_count = layout.pages[0]
            .iter()
            .filter(|f| matches!(f, Fragment::Text { .. }))
            .count();
        assert_eq!(text_count, 5, "overflowing content is not culled");
    }

    #[test]
    fn transform_origin_moves_the_pivot() {
        // `scale(2)` about origin O maps a point p to `O + 2(p − O)`, so the
        // wrapped matrix's translation is exactly `−O` — a direct probe of the
        // pivot actually used.
        let matrix_of = |html: &str| -> [f64; 6] {
            run(html)
                .pages
                .iter()
                .flatten()
                .find_map(|f| match f {
                    Fragment::Transformed { matrix, .. } => Some(*matrix),
                    _ => None,
                })
                .expect("a Transformed fragment")
        };
        // Default pivot: the border-box centre (x = 36 margin + 540/2 = 306).
        let centre = matrix_of(r#"<div style="transform:scale(2)">x</div>"#);
        assert!((centre[4] + 306.0).abs() < 1e-6, "centre pivot: {centre:?}");
        // `transform-origin: 0 0` pivots on the box's top-left (x = 36).
        let tl = matrix_of(r#"<div style="transform:scale(2);transform-origin:0 0">x</div>"#);
        assert!((tl[4] + 36.0).abs() < 1e-6, "top-left pivot: {tl:?}");
        // The keyword pair in either order lands on the same corner.
        let kw = matrix_of(
            r#"<div style="transform:scale(2);transform-origin:top left">x</div>"#,
        );
        assert!((kw[4] - tl[4]).abs() < 1e-6 && (kw[5] - tl[5]).abs() < 1e-6);
    }

    #[test]
    fn caption_side_bottom_renders_below_the_grid() {
        let layout = run(
            "<table style=\"caption-side:bottom\">\
             <caption>CAPT</caption><tr><td>CellA</td></tr></table>",
        );
        let y_capt = text_xy(&layout)
            .into_iter()
            .find(|(_, _, s)| s == "CAPT")
            .expect("caption text is rendered")
            .1;
        let y_cell = text_xy(&layout)
            .into_iter()
            .find(|(_, _, s)| s == "CellA")
            .unwrap()
            .1;
        assert!(
            y_capt > y_cell,
            "caption sits below the last row (caption={y_capt}, cell={y_cell})"
        );
    }

    #[test]
    fn vh_lengths_use_the_live_page_size_during_layout() {
        // `margin-top: 50vh` on a 500pt-tall page ⇒ 250pt below the content
        // top — provable only if the REAL page height reached the cascade.
        let nodes = parse("<p style=\"margin-top:50vh;margin-bottom:0\">x</p>");
        let sheet = Stylesheet::new("");
        let layout = layout_document(&nodes, &sheet, &AverageMeasure, 400.0, 500.0, 0.0);
        let y = layout.pages[0]
            .iter()
            .find_map(|f| match f {
                Fragment::Text { y, .. } => Some(*y),
                _ => None,
            })
            .expect("text");
        assert!(
            (y - 250.0).abs() < 1e-6,
            "50vh of a 500pt page = 250pt, got {y}"
        );
    }

    // ── stacking contexts (CSS 2.1 z-index scoping) ──

    #[test]
    fn stacking_context_keeps_descendant_z_index_local() {
        // The classic case: A(z:1) contains X(z:9999); a sibling B(z:2)
        // follows. X paints above A's own flow content but UNDER B — the
        // descendant z-index never escapes its context.
        let layout = run(
            r#"<div style="position:relative;z-index:1">
                 <p>InA</p>
                 <div style="position:relative;z-index:9999"><p>DeepX</p></div>
               </div>
               <div style="position:relative;z-index:2"><p>TopB</p></div>"#,
        );
        let order: Vec<String> = layout
            .pages
            .iter()
            .flatten()
            .filter_map(|f| match f {
                Fragment::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        let pos = |s: &str| {
            order
                .iter()
                .position(|t| t.contains(s))
                .unwrap_or_else(|| panic!("{s} missing from {order:?}"))
        };
        assert!(
            pos("InA") < pos("DeepX"),
            "the z:9999 child paints above its own context's flow ({order:?})"
        );
        assert!(
            pos("DeepX") < pos("TopB"),
            "…but stays under the parent's z:2 SIBLING context ({order:?})"
        );
    }

    #[test]
    fn negative_z_index_between_grandparent_and_parent_backgrounds() {
        // G forms the stacking context (positioned, z:1, red bg); P is a plain
        // in-flow div (green bg); N (z:-1, blue bg). CSS 2.1 Appendix E: the
        // context's own background first, then negative-z children, then the
        // in-flow content — so blue sits ABOVE red but UNDER green.
        let layout = run(
            r#"<div style="position:relative;z-index:1;background:#ff0000">
                 <div style="background:#00ff00;height:40pt">
                   <div style="position:relative;z-index:-1;background:#0000ff;height:10pt">N</div>
                 </div>
               </div>"#,
        );
        let fills: Vec<[f64; 3]> = layout
            .pages
            .iter()
            .flatten()
            .filter_map(|f| match f {
                Fragment::Rect { fill: Some(c), .. } => Some(*c),
                _ => None,
            })
            .collect();
        let pos = |c: [f64; 3], name: &str| {
            fills
                .iter()
                .position(|f| {
                    (f[0] - c[0]).abs() < 0.02
                        && (f[1] - c[1]).abs() < 0.02
                        && (f[2] - c[2]).abs() < 0.02
                })
                .unwrap_or_else(|| panic!("{name} bg missing from {fills:?}"))
        };
        let red = pos([1.0, 0.0, 0.0], "grandparent/context");
        let blue = pos([0.0, 0.0, 1.0], "negative-z child");
        let green = pos([0.0, 1.0, 0.0], "in-flow parent");
        assert!(
            red < blue,
            "negative z paints above the context's own background ({fills:?})"
        );
        assert!(
            blue < green,
            "negative z paints under the parent's in-flow background ({fills:?})"
        );
    }

    // ── multi-column pagination (page-sequential fill) ──

    #[test]
    fn multicol_fills_both_page_columns_before_the_next_page() {
        // Enough paragraphs in a 2-column block to overflow one page: page 1
        // must fill BOTH its columns down to the page band before anything
        // lands on page 2 (the old behaviour let column 1 continue onto page 2
        // while page 1's column 2 sat empty).
        let mut html = String::from(r#"<div style="column-count:2">"#);
        for i in 0..120 {
            html.push_str(&format!("<p>P{i:03}</p>"));
        }
        html.push_str("</div>");
        let layout = run(&html);
        assert!(
            layout.pages.len() >= 2,
            "content spans pages ({})",
            layout.pages.len()
        );
        let boundary = 36.0 + 264.0; // left column's right edge (540pt content, 12pt gutter)
        let texts_on = |p: usize| -> Vec<(f64, f64)> {
            layout.pages[p]
                .iter()
                .filter_map(|f| match f {
                    Fragment::Text { x, y, .. } => Some((*x, *y)),
                    _ => None,
                })
                .collect()
        };
        let p1 = texts_on(0);
        let left_bottom = p1
            .iter()
            .filter(|(x, _)| *x < boundary)
            .map(|(_, y)| *y)
            .fold(0.0_f64, f64::max);
        let right_bottom = p1
            .iter()
            .filter(|(x, _)| *x >= boundary)
            .map(|(_, y)| *y)
            .fold(0.0_f64, f64::max);
        assert!(
            left_bottom > 600.0,
            "page-1 left column fills toward the band bottom ({left_bottom})"
        );
        assert!(
            right_bottom > 600.0,
            "page-1 RIGHT column fills too — before any page-2 content ({right_bottom})"
        );
        assert!(
            !texts_on(1).is_empty(),
            "the remainder lands on page 2 ({} pages)",
            layout.pages.len()
        );
    }

    // ── writing-mode (rotated-label vertical blocks) ──

    /// The transformed bounding box of a `Transformed` fragment (CSS-space).
    fn transformed_bbox(matrix: &[f64; 6], inner: &Fragment) -> (f64, f64, f64, f64) {
        let (x0, y0, x1, y1) = fragment_bbox(inner);
        let [a, b, c, d, e, f] = *matrix;
        let pts = [(x0, y0), (x1, y0), (x1, y1), (x0, y1)];
        let mut bb = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
        for (px, py) in pts {
            let tx = a * px + c * py + e;
            let ty = b * px + d * py + f;
            bb.0 = bb.0.min(tx);
            bb.1 = bb.1.min(ty);
            bb.2 = bb.2.max(tx);
            bb.3 = bb.3.max(ty);
        }
        bb
    }

    #[test]
    fn writing_mode_vertical_rl_rotates_text_into_a_tall_narrow_box() {
        let layout = run(r#"<div style="writing-mode:vertical-rl">VERTICALLABEL</div>"#);
        let (matrix, inner) = layout
            .pages
            .iter()
            .flatten()
            .find_map(|f| match f {
                Fragment::Transformed { matrix, inner }
                    if matches!(**inner, Fragment::Text { .. }) =>
                {
                    Some((*matrix, (**inner).clone()))
                }
                _ => None,
            })
            .expect("a rotated text fragment");
        // +90° (clockwise) rotation: a=0, b=1, c=−1, d=0.
        assert!(
            matrix[0].abs() < 1e-9
                && (matrix[1] - 1.0).abs() < 1e-9
                && (matrix[2] + 1.0).abs() < 1e-9
                && matrix[3].abs() < 1e-9,
            "vertical-rl is the clockwise quarter-turn ({matrix:?})"
        );
        // The rotated text occupies a NARROW-and-TALL box (height ≫ width).
        let (bx0, by0, bx1, by1) = transformed_bbox(&matrix, &inner);
        let (w, h) = (bx1 - bx0, by1 - by0);
        assert!(
            h > w * 2.0,
            "label box is tall and narrow after rotation (w={w:.1}, h={h:.1})"
        );
        // Text content survives extraction.
        assert!(
            matches!(&inner, Fragment::Text { text, .. } if text.contains("VERTICALLABEL")),
            "the label text is present in the rotated fragment"
        );
        // Top of the rotated box stays at the flow position (content top).
        assert!(by0 >= 35.0, "rotated box starts inside the content area ({by0})");
    }

    #[test]
    fn writing_mode_vertical_lr_uses_the_opposite_rotation() {
        let layout = run(r#"<div style="writing-mode:vertical-lr">LABEL</div>"#);
        let matrix = layout
            .pages
            .iter()
            .flatten()
            .find_map(|f| match f {
                Fragment::Transformed { matrix, inner }
                    if matches!(**inner, Fragment::Text { .. }) =>
                {
                    Some(*matrix)
                }
                _ => None,
            })
            .expect("a rotated text fragment");
        // −90° (counter-clockwise): a=0, b=−1, c=1, d=0.
        assert!(
            matrix[0].abs() < 1e-9
                && (matrix[1] + 1.0).abs() < 1e-9
                && (matrix[2] - 1.0).abs() < 1e-9
                && matrix[3].abs() < 1e-9,
            "vertical-lr is the counter-clockwise quarter-turn ({matrix:?})"
        );
    }

    // ── inline <svg> preserveAspectRatio ──

    #[test]
    fn inline_svg_meet_letterboxes_without_distortion() {
        // A square viewBox into a 200×100 box under the default xMidYMid meet:
        // the draw rect is the undistorted 100×100 square, centred at +50pt.
        let layout = run(
            r##"<svg width="200" height="100" viewBox="0 0 100 100"><circle cx="50" cy="50" r="40" fill="#f00"/></svg>"##,
        );
        let (x, _y, w, h) = layout
            .pages
            .iter()
            .flatten()
            .find_map(|f| match f {
                Fragment::Svg { x, y, w, h, .. } => Some((*x, *y, *w, *h)),
                _ => None,
            })
            .expect("an svg fragment");
        assert!(
            (w - 100.0).abs() < 1.0 && (h - 100.0).abs() < 1.0,
            "meet keeps the square undistorted ({w}×{h})"
        );
        assert!(
            (x - (36.0 + 50.0)).abs() < 1.5,
            "xMid centres the square in the 200pt box (x={x})"
        );
    }

    #[test]
    fn inline_svg_slice_covers_and_is_clipped_to_the_viewport() {
        let layout = run(
            r##"<svg width="200" height="100" viewBox="0 0 100 100" preserveAspectRatio="xMidYMid slice"><circle cx="50" cy="50" r="40" fill="#f00"/></svg>"##,
        );
        let (cw, ch, dw, dh) = layout
            .pages
            .iter()
            .flatten()
            .find_map(|f| match f {
                Fragment::Clipped { rect, inner, .. } => match **inner {
                    Fragment::Svg { w, h, .. } => Some((rect[2], rect[3], w, h)),
                    _ => None,
                },
                _ => None,
            })
            .expect("a clipped svg fragment");
        assert!(
            (cw - 200.0).abs() < 1.0 && (ch - 100.0).abs() < 1.0,
            "the clip window is the CSS viewport ({cw}×{ch})"
        );
        assert!(
            (dw - 200.0).abs() < 1.0 && (dh - 200.0).abs() < 1.0,
            "slice scales the square up to cover ({dw}×{dh})"
        );
    }
}
