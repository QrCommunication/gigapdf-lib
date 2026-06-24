//! Zero-dependency SVG → PDF **vector** parser.
//!
//! Parses a standalone SVG document (or an inline `<svg>` subtree) into a flat
//! list of vector primitives — paths and the basic shapes, with fill / stroke /
//! width / opacity — resolved into the SVG **viewBox** coordinate system (group
//! and element `transform`s baked into the coordinates). The PDF emission lives
//! in [`crate::document::Document::draw_svg_image`], which maps that viewBox onto
//! a placement box and writes **native PDF path operators** — so SVG stays crisp
//! at any zoom rather than being rasterized.
//!
//! Supported: `<svg viewBox width height>`, `<g>`, `<rect>` (incl. `rx`/`ry`
//! rounded corners), `<circle>`, `<ellipse>`, `<line>`, `<polyline>`,
//! `<polygon>`, `<path>` (the full `d` grammar via [`crate::content::svg_path`]),
//! and `<text>` / `<tspan>` (positioned, anchored, filled — glyph outlines are
//! traced into vector paths via the bundled font, so SVG labels stay crisp like
//! every other primitive); presentation attributes + inline `style`: `fill`,
//! `stroke`, `stroke-width`, `opacity`, `fill-opacity`, `stroke-opacity`
//! (`none` honoured); `transform`
//! (`translate`/`scale`/`rotate`/`matrix`/`skewX`/`skewY`).

use crate::content::svg_path::{parse as parse_path_d, Seg};
use crate::content::Rgb;
use crate::font::bundled::{self, Base14};
use crate::font::truetype::TrueTypeFont;
use crate::html::css::parse_color;
use crate::html::dom::{self, Element, Node};

/// A 2×3 affine `[a b c d e f]` mapping `(x,y) → (a·x+c·y+e, b·x+d·y+f)`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Mat {
    a: f64,
    b: f64,
    c: f64,
    d: f64,
    e: f64,
    f: f64,
}

impl Mat {
    fn identity() -> Self {
        Self {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: 0.0,
            f: 0.0,
        }
    }
    fn translate(x: f64, y: f64) -> Self {
        Self {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: x,
            f: y,
        }
    }
    fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }
    /// `self ∘ other` — `other` is applied first, then `self`.
    fn then(&self, o: &Mat) -> Mat {
        Mat {
            a: self.a * o.a + self.c * o.b,
            b: self.b * o.a + self.d * o.b,
            c: self.a * o.c + self.c * o.d,
            d: self.b * o.c + self.d * o.d,
            e: self.a * o.e + self.c * o.f + self.e,
            f: self.b * o.e + self.d * o.f + self.f,
        }
    }
    /// Geometric-mean scale factor, used to scale stroke widths.
    fn scale_hint(&self) -> f64 {
        (self.a * self.d - self.b * self.c).abs().sqrt().max(1e-6)
    }
    /// Inverse affine, or `None` when (near-)singular. Used to map output-space
    /// points back into a (possibly rotated/skewed) pattern lattice.
    fn inverse(&self) -> Option<Mat> {
        let det = self.a * self.d - self.b * self.c;
        if det.abs() < 1e-12 {
            return None;
        }
        let inv = 1.0 / det;
        Some(Mat {
            a: self.d * inv,
            b: -self.b * inv,
            c: -self.c * inv,
            d: self.a * inv,
            e: (self.c * self.f - self.d * self.e) * inv,
            f: (self.b * self.e - self.a * self.f) * inv,
        })
    }
}

/// Inherited paint state while walking the SVG tree.
#[derive(Debug, Clone, Copy)]
struct Paint {
    fill: Option<Rgb>,
    stroke: Option<Rgb>,
    stroke_w: f64,
    fill_opacity: f64,
    stroke_opacity: f64,
}

impl Paint {
    /// SVG initial values: black fill, no stroke, 1px stroke width, opaque.
    fn root() -> Self {
        Self {
            fill: Some([0.0, 0.0, 0.0]),
            stroke: None,
            stroke_w: 1.0,
            fill_opacity: 1.0,
            stroke_opacity: 1.0,
        }
    }
}

/// A resolved fill: a flat colour or a gradient (coords already in prim space).
#[derive(Debug, Clone)]
pub(crate) enum Fill {
    Solid([f64; 3]),
    Gradient(Gradient),
}

/// A gradient resolved into the primitive's coordinate space.
#[derive(Debug, Clone)]
pub(crate) struct Gradient {
    pub(crate) kind: GradKind,
    pub(crate) stops: Vec<GradStop>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum GradKind {
    Linear {
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
    },
    Radial {
        cx: f64,
        cy: f64,
        r: f64,
        fx: f64,
        fy: f64,
    },
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct GradStop {
    pub(crate) offset: f64,
    pub(crate) rgb: [f64; 3],
    pub(crate) alpha: f64,
}

/// A gradient definition as written, before resolution to a primitive.
#[derive(Debug, Clone)]
struct RawGrad {
    is_linear: bool,
    user_space: bool, // gradientUnits = userSpaceOnUse (else objectBoundingBox)
    transform: Mat,
    href: Option<String>,
    stops: Vec<GradStop>,
    x1: Option<f64>,
    y1: Option<f64>,
    x2: Option<f64>,
    y2: Option<f64>,
    cx: Option<f64>,
    cy: Option<f64>,
    r: Option<f64>,
    fx: Option<f64>,
    fy: Option<f64>,
}

type Grads = std::collections::BTreeMap<String, RawGrad>;

/// A `<pattern>` tile definition as written, before resolution to a target shape.
///
/// `pattern_units` controls how `x`/`y`/`width`/`height` are read: the default
/// `objectBoundingBox` treats them as fractions of the filled shape's bounding
/// box, `userSpaceOnUse` as plain user-space lengths. `content_user_space`
/// (`patternContentUnits`, default `userSpaceOnUse`) controls the child geometry:
/// when it is `objectBoundingBox` the children are scaled by the bbox size.
#[derive(Debug, Clone)]
struct RawPattern {
    pattern_units_obb: bool,  // patternUnits = objectBoundingBox (default true)
    content_user_space: bool, // patternContentUnits = userSpaceOnUse (default true)
    href: Option<String>,
    view_box: Option<[f64; 4]>,
    x: Option<f64>,
    y: Option<f64>,
    width: Option<f64>,
    height: Option<f64>,
    /// `patternTransform` (matrix/translate/scale/rotate/skew), in user space —
    /// applied to the tile lattice so tiles can be rotated/skewed, not just
    /// axis-aligned. `None` ⇔ identity.
    transform: Option<Mat>,
    children: Vec<Node>,
}

type Pats = std::collections::BTreeMap<String, RawPattern>;

/// One drawable primitive: path segments in viewBox coordinates plus its paint.
#[derive(Debug, Clone)]
pub struct Prim {
    pub(crate) segs: Vec<Seg>,
    pub(crate) fill: Option<Fill>,
    pub(crate) stroke: Option<Rgb>,
    pub(crate) stroke_w: f64,
    pub(crate) fill_opacity: f64,
    pub(crate) stroke_opacity: f64,
}

/// A parsed SVG ready to place onto a page (see `Document::draw_svg_image`).
#[derive(Debug, Clone)]
pub struct SvgImage {
    /// Intrinsic width (from `width`, else the viewBox width).
    pub width: f64,
    /// Intrinsic height (from `height`, else the viewBox height).
    pub height: f64,
    /// `[min_x, min_y, w, h]` user-space box the primitives live in.
    pub(crate) view_box: [f64; 4],
    pub(crate) prims: Vec<Prim>,
}

/// Parse SVG markup. Returns `None` if there's no `<svg>` or it has no drawable
/// content.
pub fn parse_svg(src: &str) -> Option<SvgImage> {
    let nodes = dom::parse(src);
    from_element(find_svg(&nodes)?)
}

/// Build an [`SvgImage`] from an already-parsed `<svg>` DOM element (used by the
/// HTML renderer for inline `<svg>`).
pub fn from_element(svg: &Element) -> Option<SvgImage> {
    let vb = svg.attr("viewbox").and_then(parse_view_box);
    let attr_w = svg.attr("width").and_then(parse_len);
    let attr_h = svg.attr("height").and_then(parse_len);
    let view_box =
        vb.unwrap_or_else(|| [0.0, 0.0, attr_w.unwrap_or(100.0), attr_h.unwrap_or(100.0)]);
    let width = attr_w.or_else(|| vb.map(|v| v[2])).unwrap_or(view_box[2]);
    let height = attr_h.or_else(|| vb.map(|v| v[3])).unwrap_or(view_box[3]);

    let mut grads = Grads::new();
    collect_gradients(&svg.children, &mut grads);
    let mut ids: std::collections::HashMap<&str, &Node> = std::collections::HashMap::new();
    collect_ids(&svg.children, &mut ids);
    let mut pats = Pats::new();
    collect_patterns(&svg.children, &mut pats);
    let mut prims = Vec::new();
    walk(
        &svg.children,
        Mat::identity(),
        Paint::root(),
        &ids,
        &grads,
        &pats,
        &mut prims,
        0,
    );
    if prims.is_empty() {
        return None;
    }
    Some(SvgImage {
        width,
        height,
        view_box,
        prims,
    })
}

fn find_svg(nodes: &[Node]) -> Option<&Element> {
    for n in nodes {
        if let Node::Element(e) = n {
            if e.tag == "svg" {
                return Some(e);
            }
            if let Some(s) = find_svg(&e.children) {
                return Some(s);
            }
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn walk(
    nodes: &[Node],
    ctm: Mat,
    paint: Paint,
    ids: &std::collections::HashMap<&str, &Node>,
    grads: &Grads,
    pats: &Pats,
    out: &mut Vec<Prim>,
    depth: u8,
) {
    for n in nodes {
        let Node::Element(e) = n else { continue };
        let ctm = match e.attr("transform") {
            Some(t) => ctm.then(&parse_transform(t)),
            None => ctm,
        };
        let paint = inherit_paint(e, paint);
        let furl = fill_url(e);
        let furl = furl.as_deref();
        match e.tag.as_str() {
            "g" | "a" | "svg" => walk(&e.children, ctm, paint, ids, grads, pats, out, depth),
            "rect" => push(out, rect_segs(e), ctm, paint, furl, ids, grads, pats, depth),
            "circle" => {
                let r = attr_f(e, "r");
                push(
                    out,
                    ellipse_segs(attr_f(e, "cx"), attr_f(e, "cy"), r, r),
                    ctm,
                    paint,
                    furl,
                    ids,
                    grads,
                    pats,
                    depth,
                );
            }
            "ellipse" => push(
                out,
                ellipse_segs(
                    attr_f(e, "cx"),
                    attr_f(e, "cy"),
                    attr_f(e, "rx"),
                    attr_f(e, "ry"),
                ),
                ctm,
                paint,
                furl,
                ids,
                grads,
                pats,
                depth,
            ),
            "line" => push(out, line_segs(e), ctm, paint, furl, ids, grads, pats, depth),
            "polyline" => push(
                out,
                poly_segs(e, false),
                ctm,
                paint,
                furl,
                ids,
                grads,
                pats,
                depth,
            ),
            "polygon" => push(
                out,
                poly_segs(e, true),
                ctm,
                paint,
                furl,
                ids,
                grads,
                pats,
                depth,
            ),
            "path" => push(
                out,
                e.attr("d").map(parse_path_d).unwrap_or_default(),
                ctm,
                paint,
                furl,
                ids,
                grads,
                pats,
                depth,
            ),
            "text" => walk_text(e, ctm, paint, out),
            "use" => {
                // `<use href="#id" x y>` renders the referenced subtree, offset by
                // (x, y); the target then applies its own transform/paint. The
                // depth guard breaks cyclic references.
                let href = e
                    .attr("href")
                    .or_else(|| e.attr("xlink:href"))
                    .unwrap_or("");
                let id = href.trim().strip_prefix('#').unwrap_or("");
                if depth < 8 {
                    if let Some(target) = ids.get(id) {
                        let ctm_u = ctm.then(&Mat::translate(attr_f(e, "x"), attr_f(e, "y")));
                        walk(
                            std::slice::from_ref(*target),
                            ctm_u,
                            paint,
                            ids,
                            grads,
                            pats,
                            out,
                            depth + 1,
                        );
                    }
                }
            }
            _ => {} // <defs>/<title>/<style>/… ignored
        }
    }
}

// ── text → vector glyph outlines ────────────────────────────────────────────────
//
// SVG `<text>` is rendered by tracing each glyph's outline (from the bundled
// font) into filled vector subpaths in viewBox coordinates, then pushed as an
// ordinary `Prim`. This means the whole downstream pipeline — fill colour,
// opacity, gradient `fill="url(#…)"`, the PDF path emission in
// `Document::draw_svg_image` — applies to text exactly as it does to shapes,
// and text stays crisp at any zoom rather than being rasterized.

/// Mutable cursor while laying out text along the baseline (viewBox coords).
#[derive(Debug, Clone, Copy)]
struct TextCursor {
    x: f64,
    y: f64,
    font_size: f64,
}

/// SVG `text-anchor`: where the run's advance box sits relative to the start x.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Anchor {
    Start,
    Middle,
    End,
}

/// The bundled fallback face, parsed once. SVG text has no host-provided fonts,
/// so every family resolves to this metric-compatible sans (Liberation Sans).
/// `None` only if the embedded program failed to parse (it doesn't).
fn text_face() -> Option<&'static TrueTypeFont> {
    bundled::bundled_program_for_base14(Base14::Sans)
}

/// Render a `<text>` element (and its `<tspan>` descendants) into glyph-outline
/// primitives. The element's `transform` is already folded into `ctm` by the
/// caller; `paint` carries the inherited fill/stroke.
fn walk_text(e: &Element, ctm: Mat, paint: Paint, out: &mut Vec<Prim>) {
    let Some(face) = text_face() else { return };
    // Initial cursor from the element's own x/y (default 0), font-size from the
    // cascade (default 16, the CSS initial value).
    let mut cur = TextCursor {
        x: attr_f(e, "x"),
        y: attr_f(e, "y"),
        font_size: font_size_of(e, 16.0),
    };
    render_text_node(e, ctm, paint, face, &mut cur, out);
}

/// Recursively lay out one text container (`<text>` or `<tspan>`): apply its
/// position/style, then emit its direct text and recurse into nested `<tspan>`s.
fn render_text_node(
    e: &Element,
    ctm: Mat,
    parent_paint: Paint,
    face: &TrueTypeFont,
    cur: &mut TextCursor,
    out: &mut Vec<Prim>,
) {
    // Absolute reposition (`x`/`y`) then relative shift (`dx`/`dy`).
    if let Some(x) = e.attr("x").and_then(parse_len) {
        cur.x = x;
    }
    if let Some(y) = e.attr("y").and_then(parse_len) {
        cur.y = y;
    }
    cur.x += e.attr("dx").and_then(parse_len).unwrap_or(0.0);
    cur.y += e.attr("dy").and_then(parse_len).unwrap_or(0.0);
    cur.font_size = font_size_of(e, cur.font_size);

    let paint = inherit_paint(e, parent_paint);
    let anchor = anchor_of(e);

    // `text-anchor` shifts the whole run governed by this element. Measure the
    // total advance of this subtree's text and offset the start x accordingly.
    if anchor != Anchor::Start {
        let advance = measure_subtree(e, face, cur.font_size);
        cur.x -= match anchor {
            Anchor::Middle => advance / 2.0,
            Anchor::End => advance,
            Anchor::Start => 0.0,
        };
    }

    for child in &e.children {
        match child {
            Node::Text(t) => emit_text(t, ctm, paint, face, cur, out),
            Node::Element(c) if c.tag == "tspan" || c.tag == "text" => {
                render_text_node(c, ctm, paint, face, cur, out);
            }
            _ => {}
        }
    }
}

/// Total advance width (viewBox units, at `font_size`) of the text directly in
/// `e` plus all its `<tspan>` descendants — used to resolve `text-anchor`. A
/// nested `<tspan>` that re-anchors or repositions itself is excluded (it forms
/// its own anchored run, handled when reached).
fn measure_subtree(e: &Element, face: &TrueTypeFont, font_size: f64) -> f64 {
    let mut total = 0.0;
    for child in &e.children {
        match child {
            Node::Text(t) => total += run_advance(t, face, font_size),
            Node::Element(c) if c.tag == "tspan" => {
                if reanchors(c) {
                    continue;
                }
                let fs = font_size_of(c, font_size);
                total += measure_subtree(c, face, fs);
            }
            _ => {}
        }
    }
    total
}

/// True if a `<tspan>` starts its own anchored run (sets its own `x` or
/// `text-anchor`), so the parent's anchor measurement must skip it.
fn reanchors(e: &Element) -> bool {
    e.attr("x").and_then(parse_len).is_some() || e.attr("text-anchor").is_some()
}

/// Advance width (viewBox units) of a literal string at `font_size`, summing the
/// bundled face's per-glyph `hmtx` widths (whitespace collapsed like XML text).
fn run_advance(text: &str, face: &TrueTypeFont, font_size: f64) -> f64 {
    let upm = face.units_per_em().max(1.0);
    let scale = font_size / upm;
    let mut adv = 0.0;
    for ch in normalize_text(text).chars() {
        let gid = face.gid_for_unicode(ch as u32).unwrap_or(0);
        adv += face.advance_width(gid) * scale;
    }
    adv
}

/// Trace a literal string's glyphs into outline subpaths at the cursor, then
/// advance the cursor. Each glyph contour becomes `Move`+`Line…`+`Close` in
/// viewBox space; the font's Y-up units are flipped to SVG's Y-down. The
/// element's `ctm` is baked in, then the primitive is pushed (filled only —
/// stroking text outlines is uncommon and skipped for clarity).
fn emit_text(
    text: &str,
    ctm: Mat,
    paint: Paint,
    face: &TrueTypeFont,
    cur: &mut TextCursor,
    out: &mut Vec<Prim>,
) {
    let upm = face.units_per_em().max(1.0);
    let scale = cur.font_size / upm;
    let fill = paint.fill;
    for ch in normalize_text(text).chars() {
        let gid = face.gid_for_unicode(ch as u32).unwrap_or(0);
        let advance = face.advance_width(gid) * scale;
        // Skip drawing invisible glyphs (space, unmapped) but still advance.
        if gid != 0 || ch == ' ' {
            if let Some(rgb) = fill {
                let segs = glyph_segs(face, gid, cur.x, cur.y, scale, &ctm);
                if !segs.is_empty() {
                    out.push(Prim {
                        segs,
                        fill: Some(Fill::Solid(rgb)),
                        stroke: None,
                        stroke_w: 0.0,
                        fill_opacity: paint.fill_opacity,
                        stroke_opacity: paint.stroke_opacity,
                    });
                }
            }
        }
        cur.x += advance;
    }
}

/// Build the transformed outline of one glyph: font-unit contours → viewBox
/// coords at baseline `(bx, by)` with `scale` units/px (Y flipped from the
/// font's Y-up to SVG's Y-down), then the group `ctm` applied.
fn glyph_segs(face: &TrueTypeFont, gid: u16, bx: f64, by: f64, scale: f64, ctm: &Mat) -> Vec<Seg> {
    let mut segs = Vec::new();
    for contour in face.glyph_polygons(gid) {
        if contour.len() < 2 {
            continue;
        }
        let mut first = true;
        for &(gx, gy) in &contour {
            // Font units are Y-up; SVG is Y-down, so subtract the scaled Y.
            let px = bx + gx * scale;
            let py = by - gy * scale;
            let (tx, ty) = ctm.apply(px, py);
            if first {
                segs.push(Seg::Move(tx, ty));
                first = false;
            } else {
                segs.push(Seg::Line(tx, ty));
            }
        }
        segs.push(Seg::Close);
    }
    segs
}

/// Collapse XML whitespace runs in text content to single spaces (SVG renders
/// `<text>` with the default `xml:space` — runs of whitespace collapse).
fn normalize_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out
}

/// Resolve `font-size` for a text element from its attribute or inline `style`,
/// falling back to the inherited value.
fn font_size_of(e: &Element, inherited: f64) -> f64 {
    if let Some(v) = e.attr("font-size").and_then(parse_len) {
        return v;
    }
    if let Some(style) = e.attr("style") {
        for (k, v) in parse_style(style) {
            if k == "font-size" {
                if let Some(s) = parse_len(&v) {
                    return s;
                }
            }
        }
    }
    inherited
}

/// Resolve `text-anchor` from the attribute or inline `style` (default `start`).
fn anchor_of(e: &Element) -> Anchor {
    let raw = e.attr("text-anchor").map(str::to_string).or_else(|| {
        e.attr("style").and_then(|s| {
            parse_style(s)
                .into_iter()
                .find(|(k, _)| k == "text-anchor")
                .map(|(_, v)| v)
        })
    });
    match raw.as_deref().map(str::trim) {
        Some("middle") => Anchor::Middle,
        Some("end") => Anchor::End,
        _ => Anchor::Start,
    }
}

/// Bake the transform into the segments and record the primitive (skipping the
/// invisible: empty geometry, or no fill and no stroke). `fill_url` is a paint
/// server id from `fill="url(#id)"`, resolved against `grads` (gradients) or
/// `pats` (`<pattern>` tiles).
#[allow(clippy::too_many_arguments)]
fn push(
    out: &mut Vec<Prim>,
    segs: Vec<Seg>,
    ctm: Mat,
    paint: Paint,
    fill_url: Option<&str>,
    ids: &std::collections::HashMap<&str, &Node>,
    grads: &Grads,
    pats: &Pats,
    depth: u8,
) {
    if segs.is_empty() {
        return;
    }
    let segs: Vec<Seg> = segs.iter().map(|s| transform_seg(s, &ctm)).collect();

    // A `fill="url(#id)"` may reference a `<pattern>`: tile its child content
    // across this shape, clipped to the shape's actual outline (the contour,
    // even-odd), then optionally stroke the outline. Falls back to the inherited
    // solid fill if the pattern can't be resolved (empty / cyclic / zero tile).
    // `depth` breaks pattern cycles (a pattern that, directly or via nesting,
    // references itself).
    if let Some(id) = fill_url {
        if pats.contains_key(id) && depth < MAX_PATTERN_DEPTH {
            let bbox = segs_bbox(&segs);
            let shape = shape_outline(&segs);
            let tiled = resolve_pattern(id, pats, ids, grads, &shape, bbox, &ctm, depth);
            if let Some(tiles) = tiled {
                out.extend(tiles);
                if let Some(stroke) = paint.stroke {
                    out.push(Prim {
                        segs,
                        fill: None,
                        stroke: Some(stroke),
                        stroke_w: paint.stroke_w * ctm.scale_hint(),
                        fill_opacity: paint.fill_opacity,
                        stroke_opacity: paint.stroke_opacity,
                    });
                }
                return;
            }
            // Unresolved pattern → fall through to a plain fill below.
        }
    }

    let fill = match fill_url {
        Some(id) => resolve_gradient(id, grads, segs_bbox(&segs), &ctm)
            .map(Fill::Gradient)
            .or_else(|| paint.fill.map(Fill::Solid)),
        None => paint.fill.map(Fill::Solid),
    };
    if fill.is_none() && paint.stroke.is_none() {
        return;
    }
    out.push(Prim {
        segs,
        fill,
        stroke: paint.stroke,
        stroke_w: paint.stroke_w * ctm.scale_hint(),
        fill_opacity: paint.fill_opacity,
        stroke_opacity: paint.stroke_opacity,
    });
}

/// Axis-aligned bounding box `[min_x, min_y, max_x, max_y]` of path segments.
fn segs_bbox(segs: &[Seg]) -> [f64; 4] {
    let (mut nx, mut ny, mut xx, mut xy) = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    let mut upd = |x: f64, y: f64| {
        nx = nx.min(x);
        ny = ny.min(y);
        xx = xx.max(x);
        xy = xy.max(y);
    };
    for s in segs {
        match *s {
            Seg::Move(x, y) | Seg::Line(x, y) => upd(x, y),
            Seg::Cubic(a, b, c, d, e, f) => {
                upd(a, b);
                upd(c, d);
                upd(e, f);
            }
            Seg::Close => {}
        }
    }
    if nx > xx {
        [0.0, 0.0, 0.0, 0.0]
    } else {
        [nx, ny, xx, xy]
    }
}

fn transform_seg(s: &Seg, m: &Mat) -> Seg {
    match *s {
        Seg::Move(x, y) => {
            let (x, y) = m.apply(x, y);
            Seg::Move(x, y)
        }
        Seg::Line(x, y) => {
            let (x, y) = m.apply(x, y);
            Seg::Line(x, y)
        }
        Seg::Cubic(x1, y1, x2, y2, x3, y3) => {
            let (x1, y1) = m.apply(x1, y1);
            let (x2, y2) = m.apply(x2, y2);
            let (x3, y3) = m.apply(x3, y3);
            Seg::Cubic(x1, y1, x2, y2, x3, y3)
        }
        Seg::Close => Seg::Close,
    }
}

// ── shapes → segments (local, untransformed, SVG Y-down) ────────────────────────

/// Control-point factor for approximating a quarter ellipse with one cubic.
const KAPPA: f64 = 0.552_284_749_830_793_4;

fn rect_segs(e: &Element) -> Vec<Seg> {
    let (x, y, w, h) = (
        attr_f(e, "x"),
        attr_f(e, "y"),
        attr_f(e, "width"),
        attr_f(e, "height"),
    );
    if w <= 0.0 || h <= 0.0 {
        return Vec::new();
    }
    // `rx`/`ry` default to each other; clamp to half the side.
    let mut rx = e.attr("rx").and_then(parse_len);
    let mut ry = e.attr("ry").and_then(parse_len);
    if rx.is_none() {
        rx = ry;
    }
    if ry.is_none() {
        ry = rx;
    }
    let rx = rx.unwrap_or(0.0).clamp(0.0, w / 2.0);
    let ry = ry.unwrap_or(0.0).clamp(0.0, h / 2.0);
    if rx <= 0.0 || ry <= 0.0 {
        return vec![
            Seg::Move(x, y),
            Seg::Line(x + w, y),
            Seg::Line(x + w, y + h),
            Seg::Line(x, y + h),
            Seg::Close,
        ];
    }
    let (kx, ky) = (rx * KAPPA, ry * KAPPA);
    vec![
        Seg::Move(x + rx, y),
        Seg::Line(x + w - rx, y),
        Seg::Cubic(x + w - rx + kx, y, x + w, y + ry - ky, x + w, y + ry),
        Seg::Line(x + w, y + h - ry),
        Seg::Cubic(
            x + w,
            y + h - ry + ky,
            x + w - rx + kx,
            y + h,
            x + w - rx,
            y + h,
        ),
        Seg::Line(x + rx, y + h),
        Seg::Cubic(x + rx - kx, y + h, x, y + h - ry + ky, x, y + h - ry),
        Seg::Line(x, y + ry),
        Seg::Cubic(x, y + ry - ky, x + rx - kx, y, x + rx, y),
        Seg::Close,
    ]
}

fn ellipse_segs(cx: f64, cy: f64, rx: f64, ry: f64) -> Vec<Seg> {
    if rx <= 0.0 || ry <= 0.0 {
        return Vec::new();
    }
    let (kx, ky) = (rx * KAPPA, ry * KAPPA);
    vec![
        Seg::Move(cx + rx, cy),
        Seg::Cubic(cx + rx, cy + ky, cx + kx, cy + ry, cx, cy + ry),
        Seg::Cubic(cx - kx, cy + ry, cx - rx, cy + ky, cx - rx, cy),
        Seg::Cubic(cx - rx, cy - ky, cx - kx, cy - ry, cx, cy - ry),
        Seg::Cubic(cx + kx, cy - ry, cx + rx, cy - ky, cx + rx, cy),
        Seg::Close,
    ]
}

fn line_segs(e: &Element) -> Vec<Seg> {
    vec![
        Seg::Move(attr_f(e, "x1"), attr_f(e, "y1")),
        Seg::Line(attr_f(e, "x2"), attr_f(e, "y2")),
    ]
}

fn poly_segs(e: &Element, close: bool) -> Vec<Seg> {
    let pts = parse_points(e.attr("points").unwrap_or(""));
    if pts.len() < 2 {
        return Vec::new();
    }
    let mut s = Vec::with_capacity(pts.len() + 1);
    s.push(Seg::Move(pts[0].0, pts[0].1));
    for p in &pts[1..] {
        s.push(Seg::Line(p.0, p.1));
    }
    if close {
        s.push(Seg::Close);
    }
    s
}

// ── attribute / style parsing ───────────────────────────────────────────────────

fn inherit_paint(e: &Element, mut p: Paint) -> Paint {
    apply_presentation(&mut p, "fill", e.attr("fill"));
    apply_presentation(&mut p, "stroke", e.attr("stroke"));
    if let Some(w) = e.attr("stroke-width").and_then(parse_len) {
        p.stroke_w = w;
    }
    if let Some(o) = e.attr("opacity").and_then(parse_f64) {
        p.fill_opacity *= o.clamp(0.0, 1.0);
        p.stroke_opacity *= o.clamp(0.0, 1.0);
    }
    if let Some(o) = e.attr("fill-opacity").and_then(parse_f64) {
        p.fill_opacity = o.clamp(0.0, 1.0);
    }
    if let Some(o) = e.attr("stroke-opacity").and_then(parse_f64) {
        p.stroke_opacity = o.clamp(0.0, 1.0);
    }
    // The inline `style` attribute overrides presentation attributes.
    if let Some(style) = e.attr("style") {
        for (k, v) in parse_style(style) {
            match k.as_str() {
                "fill" => apply_presentation(&mut p, "fill", Some(&v)),
                "stroke" => apply_presentation(&mut p, "stroke", Some(&v)),
                "stroke-width" => {
                    if let Some(w) = parse_len(&v) {
                        p.stroke_w = w;
                    }
                }
                "opacity" => {
                    if let Some(o) = parse_f64(&v) {
                        p.fill_opacity *= o.clamp(0.0, 1.0);
                        p.stroke_opacity *= o.clamp(0.0, 1.0);
                    }
                }
                "fill-opacity" => {
                    if let Some(o) = parse_f64(&v) {
                        p.fill_opacity = o.clamp(0.0, 1.0);
                    }
                }
                "stroke-opacity" => {
                    if let Some(o) = parse_f64(&v) {
                        p.stroke_opacity = o.clamp(0.0, 1.0);
                    }
                }
                _ => {}
            }
        }
    }
    p
}

/// Apply a `fill`/`stroke` value: `none` clears the paint, a recognised colour
/// sets it, anything else (e.g. `url(#grad)`) leaves the inherited value.
fn apply_presentation(p: &mut Paint, which: &str, val: Option<&str>) {
    let Some(v) = val.map(str::trim) else { return };
    let resolved = if v.eq_ignore_ascii_case("none") {
        Some(None)
    } else {
        parse_color(v).map(Some)
    };
    if let Some(c) = resolved {
        if which == "fill" {
            p.fill = c;
        } else {
            p.stroke = c;
        }
    }
}

// ── gradients ───────────────────────────────────────────────────────────────────

/// The gradient id of a `fill="url(#id)"` (attribute or inline style).
fn fill_url(e: &Element) -> Option<String> {
    if let Some(u) = e.attr("fill").and_then(extract_url) {
        return Some(u);
    }
    e.attr("style").and_then(|s| {
        parse_style(s)
            .into_iter()
            .find(|(k, _)| k == "fill")
            .and_then(|(_, v)| extract_url(&v))
    })
}

fn extract_url(v: &str) -> Option<String> {
    let inner = v.trim().strip_prefix("url(")?.strip_suffix(')')?;
    Some(
        inner
            .trim()
            .trim_matches(|c| c == '"' || c == '\'')
            .trim_start_matches('#')
            .to_string(),
    )
}

fn collect_gradients(nodes: &[Node], out: &mut Grads) {
    for n in nodes {
        let Node::Element(e) = n else { continue };
        if (e.tag == "lineargradient" || e.tag == "radialgradient") && e.attr("id").is_some() {
            out.insert(e.attr("id").unwrap().to_string(), parse_raw_grad(e));
        }
        collect_gradients(&e.children, out);
    }
}

/// Map every element with an `id` to its `Node`, so `<use href="#id">` can render
/// the referenced subtree (re-entering [`walk`], which applies the target's own
/// transform/paint). First definition wins (ids are unique).
fn collect_ids<'a>(nodes: &'a [Node], out: &mut std::collections::HashMap<&'a str, &'a Node>) {
    for n in nodes {
        let Node::Element(e) = n else { continue };
        if let Some(id) = e.attr("id") {
            out.entry(id).or_insert(n);
        }
        collect_ids(&e.children, out);
    }
}

fn parse_raw_grad(e: &Element) -> RawGrad {
    let coord = |name: &str| e.attr(name).and_then(parse_grad_coord);
    RawGrad {
        is_linear: e.tag == "lineargradient",
        user_space: e
            .attr("gradientunits")
            .map(|u| u.eq_ignore_ascii_case("userSpaceOnUse"))
            .unwrap_or(false),
        transform: e
            .attr("gradienttransform")
            .map(parse_transform)
            .unwrap_or_else(Mat::identity),
        href: e
            .attr("href")
            .or_else(|| e.attr("xlink:href"))
            .map(|h| h.trim().trim_start_matches('#').to_string()),
        stops: parse_stops(e),
        x1: coord("x1"),
        y1: coord("y1"),
        x2: coord("x2"),
        y2: coord("y2"),
        cx: coord("cx"),
        cy: coord("cy"),
        r: coord("r"),
        fx: coord("fx"),
        fy: coord("fy"),
    }
}

/// A gradient coordinate: a `%` (→ fraction) or a plain number.
fn parse_grad_coord(s: &str) -> Option<f64> {
    let s = s.trim();
    match s.strip_suffix('%') {
        Some(p) => p.trim().parse::<f64>().ok().map(|v| v / 100.0),
        None => parse_len(s),
    }
}

fn parse_stops(e: &Element) -> Vec<GradStop> {
    let mut stops = Vec::new();
    for c in &e.children {
        let Node::Element(s) = c else { continue };
        if s.tag != "stop" {
            continue;
        }
        let offset = s
            .attr("offset")
            .and_then(parse_grad_coord)
            .unwrap_or(0.0)
            .clamp(0.0, 1.0);
        let mut rgb = s
            .attr("stop-color")
            .and_then(parse_color)
            .unwrap_or([0.0, 0.0, 0.0]);
        let mut alpha = s
            .attr("stop-opacity")
            .and_then(parse_f64)
            .unwrap_or(1.0)
            .clamp(0.0, 1.0);
        if let Some(style) = s.attr("style") {
            for (k, v) in parse_style(style) {
                match k.as_str() {
                    "stop-color" => {
                        if let Some(c) = parse_color(&v) {
                            rgb = c;
                        }
                    }
                    "stop-opacity" => {
                        if let Some(o) = parse_f64(&v) {
                            alpha = o.clamp(0.0, 1.0);
                        }
                    }
                    _ => {}
                }
            }
        }
        stops.push(GradStop { offset, rgb, alpha });
    }
    stops
}

/// Resolve a gradient reference to primitive-space coordinates + sorted stops.
fn resolve_gradient(id: &str, grads: &Grads, bbox: [f64; 4], ctm: &Mat) -> Option<Gradient> {
    let mut raw = grads.get(id)?.clone();
    // One level of href inheritance (stops + geometry).
    if let Some(href) = raw.href.clone() {
        if let Some(parent) = grads.get(&href) {
            if raw.stops.is_empty() {
                raw.stops = parent.stops.clone();
            }
            raw.x1 = raw.x1.or(parent.x1);
            raw.y1 = raw.y1.or(parent.y1);
            raw.x2 = raw.x2.or(parent.x2);
            raw.y2 = raw.y2.or(parent.y2);
            raw.cx = raw.cx.or(parent.cx);
            raw.cy = raw.cy.or(parent.cy);
            raw.r = raw.r.or(parent.r);
            raw.fx = raw.fx.or(parent.fx);
            raw.fy = raw.fy.or(parent.fy);
        }
    }
    if raw.stops.is_empty() {
        return None;
    }
    let [minx, miny, maxx, maxy] = bbox;
    let (bw, bh) = (maxx - minx, maxy - miny);
    let pt = |fx: f64, fy: f64| -> (f64, f64) {
        if raw.user_space {
            ctm.then(&raw.transform).apply(fx, fy)
        } else {
            (minx + fx * bw, miny + fy * bh)
        }
    };
    let kind = if raw.is_linear {
        let (x1, y1) = pt(raw.x1.unwrap_or(0.0), raw.y1.unwrap_or(0.0));
        let (x2, y2) = pt(raw.x2.unwrap_or(1.0), raw.y2.unwrap_or(0.0));
        GradKind::Linear { x1, y1, x2, y2 }
    } else {
        let cxf = raw.cx.unwrap_or(0.5);
        let cyf = raw.cy.unwrap_or(0.5);
        let (cx, cy) = pt(cxf, cyf);
        let (fx, fy) = pt(raw.fx.unwrap_or(cxf), raw.fy.unwrap_or(cyf));
        let rf = raw.r.unwrap_or(0.5);
        let r = if raw.user_space {
            rf * ctm.scale_hint()
        } else {
            rf * ((bw * bw + bh * bh) / 2.0).sqrt()
        };
        GradKind::Radial { cx, cy, r, fx, fy }
    };
    let mut stops = raw.stops;
    stops.sort_by(|a, b| {
        a.offset
            .partial_cmp(&b.offset)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Some(Gradient { kind, stops })
}

// ── patterns (tiling) ───────────────────────────────────────────────────────────

fn collect_patterns(nodes: &[Node], out: &mut Pats) {
    for n in nodes {
        let Node::Element(e) = n else { continue };
        if e.tag == "pattern" {
            if let Some(id) = e.attr("id") {
                out.insert(id.to_string(), parse_raw_pattern(e));
            }
        }
        collect_patterns(&e.children, out);
    }
}

/// `patternUnits` / `patternContentUnits` enum: true ⇔ `objectBoundingBox`.
fn units_is_obb(v: Option<&str>, default_obb: bool) -> bool {
    match v.map(str::trim) {
        Some(s) if s.eq_ignore_ascii_case("objectBoundingBox") => true,
        Some(s) if s.eq_ignore_ascii_case("userSpaceOnUse") => false,
        _ => default_obb,
    }
}

fn parse_raw_pattern(e: &Element) -> RawPattern {
    RawPattern {
        pattern_units_obb: units_is_obb(e.attr("patternunits"), true),
        content_user_space: !units_is_obb(e.attr("patterncontentunits"), false),
        href: e
            .attr("href")
            .or_else(|| e.attr("xlink:href"))
            .map(|h| h.trim().trim_start_matches('#').to_string()),
        view_box: e.attr("viewbox").and_then(parse_view_box),
        x: e.attr("x").and_then(parse_grad_coord),
        y: e.attr("y").and_then(parse_grad_coord),
        width: e.attr("width").and_then(parse_grad_coord),
        height: e.attr("height").and_then(parse_grad_coord),
        transform: e.attr("patterntransform").map(parse_transform),
        children: e.children.clone(),
    }
}

/// Resolve a `<pattern>` reference into a list of clipped, tiled primitives that
/// cover the target shape's bounding box `bbox` (in output / CTM-baked space).
///
/// `bbox` is `[min_x, min_y, max_x, max_y]`. `objectBoundingBox` tile geometry is
/// sized as a fraction of the bbox; `userSpaceOnUse` uses plain lengths scaled by
/// the CTM. One level of `href` inheritance fills in missing geometry/children.
/// Returns `None` if the tile is degenerate or the pattern paints nothing.
#[allow(clippy::too_many_arguments)]
fn resolve_pattern(
    id: &str,
    pats: &Pats,
    ids: &std::collections::HashMap<&str, &Node>,
    grads: &Grads,
    shape: &[Vec<(f64, f64)>],
    bbox: [f64; 4],
    ctm: &Mat,
    depth: u8,
) -> Option<Vec<Prim>> {
    let mut pat = pats.get(id)?.clone();
    // One level of href inheritance (children + tile geometry + viewBox).
    if let Some(href) = pat.href.clone() {
        if href != id {
            if let Some(parent) = pats.get(&href) {
                if pat.children.is_empty() {
                    pat.children = parent.children.clone();
                }
                pat.x = pat.x.or(parent.x);
                pat.y = pat.y.or(parent.y);
                pat.width = pat.width.or(parent.width);
                pat.height = pat.height.or(parent.height);
                pat.view_box = pat.view_box.or(parent.view_box);
                pat.transform = pat.transform.or(parent.transform);
            }
        }
    }
    if pat.children.is_empty() {
        return None;
    }

    let [minx, miny, maxx, maxy] = bbox;
    let (bw, bh) = (maxx - minx, maxy - miny);
    if bw <= 0.0 || bh <= 0.0 {
        return None;
    }
    // The CTM's mean scale maps userSpaceOnUse lengths into output space.
    let s = ctm.scale_hint();

    // Tile size + origin in output space (before any patternTransform).
    let (tw, th, ox, oy) = if pat.pattern_units_obb {
        (
            pat.width.unwrap_or(0.0) * bw,
            pat.height.unwrap_or(0.0) * bh,
            minx + pat.x.unwrap_or(0.0) * bw,
            miny + pat.y.unwrap_or(0.0) * bh,
        )
    } else {
        let (ox, oy) = ctm.apply(pat.x.unwrap_or(0.0), pat.y.unwrap_or(0.0));
        (
            pat.width.unwrap_or(0.0) * s,
            pat.height.unwrap_or(0.0) * s,
            ox,
            oy,
        )
    };
    if tw <= 1e-6 || th <= 1e-6 {
        return None;
    }

    // `patternTransform` acts on the pattern's user space; conjugate it into output
    // space (`pto = ctm ∘ PT ∘ ctm⁻¹`) so it composes with the already-output-space
    // tile lattice. Tiles then rotate/skew with `PT` instead of staying axis-aligned.
    let pto = match pat.transform {
        Some(pt) => match ctm.inverse() {
            Some(inv) => ctm.then(&pt).then(&inv),
            None => Mat::identity(),
        },
        None => Mat::identity(),
    };

    // Which integer cells (i, j) of the un-transformed lattice can cover the bbox?
    // Map the four bbox corners back through `pto` into lattice space, then read off
    // the (col, row) index span. Without `pto` this reduces to the old AABB walk.
    let pto_inv = pto.inverse().unwrap_or_else(Mat::identity);
    let corners = [(minx, miny), (maxx, miny), (maxx, maxy), (minx, maxy)];
    let (mut lo_c, mut hi_c, mut lo_r, mut hi_r) = (
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
    );
    for (cx, cy) in corners {
        let (lx, ly) = pto_inv.apply(cx, cy);
        let c = (lx - ox) / tw;
        let r = (ly - oy) / th;
        lo_c = lo_c.min(c);
        hi_c = hi_c.max(c);
        lo_r = lo_r.min(r);
        hi_r = hi_r.max(r);
    }
    let start_col = lo_c.floor() as i64 - 1;
    let end_col = hi_c.ceil() as i64 + 1;
    let start_row = lo_r.floor() as i64 - 1;
    let end_row = hi_r.ceil() as i64 + 1;

    // Cap the grid so a tiny tile over a huge bbox can't explode the primitive count.
    const MAX_CELLS: usize = 20_000;
    let ncols = (end_col - start_col + 1).max(0) as usize;
    let nrows = (end_row - start_row + 1).max(0) as usize;
    if ncols == 0 || nrows == 0 || ncols.saturating_mul(nrows) > MAX_CELLS {
        return None;
    }

    // Build the un-positioned tile content once (output-space, single tile at the
    // grid origin). The tile cell is `[ox, oy] … [ox+tw, oy+th]`.
    let content = build_tile_content(&pat, tw, th, s, ox, oy, ids, grads, pats, depth);
    if content.is_empty() {
        return None;
    }

    // Lay each cell out, transform its content by `pto ∘ translate(dx, dy)`, then
    // clip to the shape's actual outline (the contour, even-odd) ∩ the transformed
    // cell quad. Falling back to the bbox keeps a degenerate/empty outline working.
    let mut tiles: Vec<Prim> = Vec::new();
    for row in start_row..=end_row {
        for col in start_col..=end_col {
            let cell_dx = col as f64 * tw;
            let cell_dy = row as f64 * th;
            // The transformed cell quad (for an inexpensive bbox reject + a convex
            // pre-clip window).
            let quad = [
                pto.apply(ox + cell_dx, oy + cell_dy),
                pto.apply(ox + cell_dx + tw, oy + cell_dy),
                pto.apply(ox + cell_dx + tw, oy + cell_dy + th),
                pto.apply(ox + cell_dx, oy + cell_dy + th),
            ];
            let qb = poly_bbox(&quad);
            if qb[2] < minx || qb[0] > maxx || qb[3] < miny || qb[1] > maxy {
                continue; // cell entirely outside the shape bbox
            }
            let place = pto.then(&Mat::translate(cell_dx, cell_dy));
            for tc in &content {
                if let Some(clipped) = clip_prim_to_shape(tc, &place, &quad, shape, bbox) {
                    tiles.push(clipped);
                }
            }
        }
    }
    if tiles.is_empty() {
        None
    } else {
        Some(tiles)
    }
}

/// Walk a pattern's child content into primitives placed at the grid origin
/// `(ox, oy)` in output space. `s` is the CTM scale; `tw`/`th` the tile size.
/// `patternContentUnits=objectBoundingBox` scales child coords by the tile size.
#[allow(clippy::too_many_arguments)]
fn build_tile_content(
    pat: &RawPattern,
    tw: f64,
    th: f64,
    s: f64,
    ox: f64,
    oy: f64,
    ids: &std::collections::HashMap<&str, &Node>,
    grads: &Grads,
    pats: &Pats,
    depth: u8,
) -> Vec<Prim> {
    // Content transform: map a child's local coords into output space at the tile
    // origin. `viewBox` (if present) maps the box onto the tile size; otherwise
    // userSpaceOnUse content scales by the CTM and objectBoundingBox by tile size.
    let content_mat = if let Some([vx, vy, vw, vh]) = pat.view_box {
        if vw > 0.0 && vh > 0.0 {
            Mat::translate(ox, oy).then(&Mat {
                a: tw / vw,
                b: 0.0,
                c: 0.0,
                d: th / vh,
                e: -vx * tw / vw,
                f: -vy * th / vh,
            })
        } else {
            Mat::translate(ox, oy)
        }
    } else if pat.content_user_space {
        Mat::translate(ox, oy).then(&Mat {
            a: s,
            b: 0.0,
            c: 0.0,
            d: s,
            e: 0.0,
            f: 0.0,
        })
    } else {
        // objectBoundingBox content: fractions of the tile size.
        Mat::translate(ox, oy).then(&Mat {
            a: tw,
            b: 0.0,
            c: 0.0,
            d: th,
            e: 0.0,
            f: 0.0,
        })
    };

    // The pattern's children inherit only their own paint (SVG: pattern content
    // does not inherit from the referencing element), starting from initial. The
    // full pattern registry is threaded so a child `fill="url(#inner)"` resolves
    // (nested patterns); the `depth` guard (in `push`) breaks pattern cycles.
    let mut content = Vec::new();
    walk(
        &pat.children,
        content_mat,
        Paint::root(),
        ids,
        grads,
        pats,
        &mut content,
        depth.saturating_add(1),
    );
    content
}

/// Maximum nesting depth for pattern resolution (a pattern whose content
/// references another pattern, etc.). Also the cycle guard for self-reference.
const MAX_PATTERN_DEPTH: u8 = 6;

/// Flatten a shape's transformed segments into one polygon per subpath (output
/// space). These polygons are the shape's true outline, used to clip pattern
/// tiles to the contour (even-odd) rather than just the bounding box.
fn shape_outline(segs: &[Seg]) -> Vec<Vec<(f64, f64)>> {
    split_subpaths(segs)
        .iter()
        .map(|sub| flatten_subpath(sub, 0.0, 0.0))
        .filter(|poly| poly.len() >= 3)
        .collect()
}

/// Axis-aligned bbox `[min_x, min_y, max_x, max_y]` of a point list.
fn poly_bbox(poly: &[(f64, f64)]) -> [f64; 4] {
    let (mut nx, mut ny, mut xx, mut xy) = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    for &(x, y) in poly {
        nx = nx.min(x);
        ny = ny.min(y);
        xx = xx.max(x);
        xy = xy.max(y);
    }
    if nx > xx {
        [0.0, 0.0, 0.0, 0.0]
    } else {
        [nx, ny, xx, xy]
    }
}

/// Clip a tile primitive to (the transformed cell quad) ∩ (the shape contour).
/// Each tile subpath is flattened, placed via `place`, clipped to the convex
/// `quad` (Sutherland–Hodgman), then intersected with the shape outline via
/// [`clip_tile_to_shape`]. Empty `shape` falls back to the `bbox` rectangle so a
/// degenerate outline still tiles. Returns `None` if nothing survives.
fn clip_prim_to_shape(
    p: &Prim,
    place: &Mat,
    quad: &[(f64, f64); 4],
    shape: &[Vec<(f64, f64)>],
    bbox: [f64; 4],
) -> Option<Prim> {
    let mut out_segs: Vec<Seg> = Vec::new();
    let mut any = false;
    for sub in split_subpaths(&p.segs) {
        // Flatten this subpath (local, at the grid origin) and place it.
        let mut poly: Vec<(f64, f64)> = flatten_subpath(&sub, 0.0, 0.0)
            .into_iter()
            .map(|(x, y)| place.apply(x, y))
            .collect();
        if poly.len() < 3 {
            continue;
        }
        // Clip to the (convex) transformed cell quad so a single tile never bleeds
        // into its neighbours.
        poly = clip_polygon_convex(&poly, quad);
        if poly.len() < 3 {
            continue;
        }
        // Then intersect with the shape's actual contour.
        let pieces = if shape.is_empty() {
            let [x0, y0, x1, y1] = bbox;
            vec![clip_polygon(&poly, [x0, y0, x1, y1])]
                .into_iter()
                .filter(|pc| pc.len() >= 3)
                .collect::<Vec<_>>()
        } else {
            clip_tile_to_shape(&poly, shape, bbox)
        };
        for piece in pieces {
            if piece.len() < 3 {
                continue;
            }
            out_segs.push(Seg::Move(piece[0].0, piece[0].1));
            for pt in &piece[1..] {
                out_segs.push(Seg::Line(pt.0, pt.1));
            }
            out_segs.push(Seg::Close);
            any = true;
        }
    }
    if !any {
        return None;
    }
    Some(Prim {
        segs: out_segs,
        fill: p.fill.clone(),
        stroke: p.stroke,
        stroke_w: p.stroke_w,
        fill_opacity: p.fill_opacity,
        stroke_opacity: p.stroke_opacity,
    })
}

/// Sutherland–Hodgman clip of a polygon against a **convex** polygon window
/// given as an ordered vertex list (used for the transformed cell quad, which is
/// always convex). Returns the clipped vertices (possibly empty).
fn clip_polygon_convex(poly: &[(f64, f64)], window: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let m = window.len();
    if m < 3 {
        return poly.to_vec();
    }
    // Orientation sign of the window (CCW vs CW) so "inside" is consistent.
    let area2: f64 = (0..m)
        .map(|i| {
            let (x1, y1) = window[i];
            let (x2, y2) = window[(i + 1) % m];
            x1 * y2 - x2 * y1
        })
        .sum();
    let ccw = area2 >= 0.0;
    let side = |a: (f64, f64), b: (f64, f64), p: (f64, f64)| -> f64 {
        // > 0 ⇒ left of a→b.
        let v = (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0);
        if ccw {
            v
        } else {
            -v
        }
    };
    let intersect = |a: (f64, f64), b: (f64, f64), e0: (f64, f64), e1: (f64, f64)| -> (f64, f64) {
        let r = (b.0 - a.0, b.1 - a.1);
        let sgmt = (e1.0 - e0.0, e1.1 - e0.1);
        let denom = r.0 * sgmt.1 - r.1 * sgmt.0;
        if denom.abs() < 1e-12 {
            return b;
        }
        let t = ((e0.0 - a.0) * sgmt.1 - (e0.1 - a.1) * sgmt.0) / denom;
        (a.0 + t * r.0, a.1 + t * r.1)
    };
    let mut out = poly.to_vec();
    for i in 0..m {
        if out.len() < 3 {
            return Vec::new();
        }
        let e0 = window[i];
        let e1 = window[(i + 1) % m];
        let input = std::mem::take(&mut out);
        let n = input.len();
        for k in 0..n {
            let cur = input[k];
            let prev = input[(k + n - 1) % n];
            let cur_in = side(e0, e1, cur) >= -1e-9;
            let prev_in = side(e0, e1, prev) >= -1e-9;
            if cur_in {
                if !prev_in {
                    out.push(intersect(prev, cur, e0, e1));
                }
                out.push(cur);
            } else if prev_in {
                out.push(intersect(prev, cur, e0, e1));
            }
        }
    }
    out
}

/// Clip one (already cell-quad-clipped) tile subpath `tile` to the shape's actual
/// outline, returning the inside pieces. This is what makes a pattern fill respect
/// a `<circle>`/`<ellipse>`/`<path>`/`<polygon>` *contour* instead of just its
/// bounding box.
///
/// The tile is convex in the overwhelming majority of cases (a pattern's child
/// shapes are convex, and clipping a convex polygon to the convex cell quad keeps
/// it convex). When it is convex we clip **each shape contour against the tile**
/// (a convex window) with Sutherland–Hodgman — exact for an arbitrary (incl.
/// concave) shape contour, so `tile ∩ shape` is computed correctly with no leakage
/// into concave notches. A rare concave tile (e.g. a star glyph in the pattern)
/// falls back to the bbox-rectangle clip (no worse than before contour clipping).
fn clip_tile_to_shape(
    tile: &[(f64, f64)],
    shape: &[Vec<(f64, f64)>],
    bbox: [f64; 4],
) -> Vec<Vec<(f64, f64)>> {
    if tile.len() < 3 {
        return Vec::new();
    }
    if !is_convex(tile) {
        // Concave tile content: keep the simple bbox clip (correctness-preserving
        // fallback; contour-accurate clipping of a concave tile is not attempted).
        let [x0, y0, x1, y1] = bbox;
        let pc = clip_polygon(tile, [x0, y0, x1, y1]);
        return if pc.len() >= 3 { vec![pc] } else { Vec::new() };
    }
    // Convex tile ⇒ clip every shape contour against it. Each result is the part of
    // that contour's interior lying inside the tile; their union is `tile ∩ shape`.
    let mut pieces: Vec<Vec<(f64, f64)>> = Vec::new();
    for contour in shape {
        if contour.len() < 3 {
            continue;
        }
        let pc = clip_polygon_convex(contour, tile);
        if pc.len() >= 3 {
            pieces.push(pc);
        }
    }
    pieces
}

/// Is the polygon convex? (All cross-products of consecutive edges share a sign,
/// allowing near-collinear zeros.) Picks the exact convex-window clip path.
fn is_convex(poly: &[(f64, f64)]) -> bool {
    let n = poly.len();
    if n < 3 {
        return false;
    }
    let mut sign = 0i8;
    for i in 0..n {
        let a = poly[i];
        let b = poly[(i + 1) % n];
        let c = poly[(i + 2) % n];
        let cross = (b.0 - a.0) * (c.1 - b.1) - (b.1 - a.1) * (c.0 - b.0);
        if cross.abs() < 1e-9 {
            continue; // collinear vertex — ignore
        }
        let s = if cross > 0.0 { 1 } else { -1 };
        if sign == 0 {
            sign = s;
        } else if s != sign {
            return false;
        }
    }
    true
}

/// Split a segment list into subpaths (each starting at a `Move`).
fn split_subpaths(segs: &[Seg]) -> Vec<Vec<Seg>> {
    let mut subs: Vec<Vec<Seg>> = Vec::new();
    for s in segs {
        match s {
            Seg::Move(..) => subs.push(vec![*s]),
            other => {
                if let Some(last) = subs.last_mut() {
                    last.push(*other);
                }
            }
        }
    }
    subs
}

/// Flatten one subpath (cubics subdivided) into a polyline, applying the cell
/// offset `(dx, dy)`. `Close` is implicit (the polygon clipper treats it closed).
fn flatten_subpath(sub: &[Seg], dx: f64, dy: f64) -> Vec<(f64, f64)> {
    let mut pts: Vec<(f64, f64)> = Vec::new();
    let mut cur = (0.0, 0.0);
    for s in sub {
        match *s {
            Seg::Move(x, y) => {
                cur = (x + dx, y + dy);
                pts.push(cur);
            }
            Seg::Line(x, y) => {
                cur = (x + dx, y + dy);
                pts.push(cur);
            }
            Seg::Cubic(x1, y1, x2, y2, x3, y3) => {
                let p0 = cur;
                let p1 = (x1 + dx, y1 + dy);
                let p2 = (x2 + dx, y2 + dy);
                let p3 = (x3 + dx, y3 + dy);
                const STEPS: usize = 12;
                for k in 1..=STEPS {
                    let t = k as f64 / STEPS as f64;
                    pts.push(cubic_at(p0, p1, p2, p3, t));
                }
                cur = p3;
            }
            Seg::Close => {}
        }
    }
    pts
}

fn cubic_at(p0: (f64, f64), p1: (f64, f64), p2: (f64, f64), p3: (f64, f64), t: f64) -> (f64, f64) {
    let u = 1.0 - t;
    let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
    (
        a * p0.0 + b * p1.0 + c * p2.0 + d * p3.0,
        a * p0.1 + b * p1.1 + c * p2.1 + d * p3.1,
    )
}

/// Sutherland–Hodgman clip of a closed polygon against an axis-aligned rectangle
/// `[x0, y0, x1, y1]`. Returns the clipped polygon's vertices (possibly empty).
fn clip_polygon(poly: &[(f64, f64)], rect: [f64; 4]) -> Vec<(f64, f64)> {
    let [x0, y0, x1, y1] = rect;
    // Each edge: keep the side that is inside. `inside`/`intersect` per edge.
    let edges: [(u8, f64); 4] = [(0, x0), (1, x1), (2, y0), (3, y1)];
    let mut out: Vec<(f64, f64)> = poly.to_vec();
    for (which, val) in edges {
        if out.len() < 2 {
            return Vec::new();
        }
        let input = std::mem::take(&mut out);
        let inside = |p: &(f64, f64)| match which {
            0 => p.0 >= val, // left edge: x >= x0
            1 => p.0 <= val, // right edge: x <= x1
            2 => p.1 >= val, // bottom edge: y >= y0
            _ => p.1 <= val, // top edge: y <= y1
        };
        let intersect = |a: &(f64, f64), b: &(f64, f64)| -> (f64, f64) {
            match which {
                0 | 1 => {
                    let t = (val - a.0) / (b.0 - a.0);
                    (val, a.1 + t * (b.1 - a.1))
                }
                _ => {
                    let t = (val - a.1) / (b.1 - a.1);
                    (a.0 + t * (b.0 - a.0), val)
                }
            }
        };
        let n = input.len();
        for i in 0..n {
            let cur = input[i];
            let prev = input[(i + n - 1) % n];
            let cur_in = inside(&cur);
            let prev_in = inside(&prev);
            if cur_in {
                if !prev_in {
                    out.push(intersect(&prev, &cur));
                }
                out.push(cur);
            } else if prev_in {
                out.push(intersect(&prev, &cur));
            }
        }
    }
    out
}

fn parse_style(s: &str) -> Vec<(String, String)> {
    s.split(';')
        .filter_map(|decl| {
            let (k, v) = decl.split_once(':')?;
            Some((k.trim().to_ascii_lowercase(), v.trim().to_string()))
        })
        .collect()
}

fn parse_transform(s: &str) -> Mat {
    let mut m = Mat::identity();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && !bytes[i].is_ascii_alphabetic() {
            i += 1;
        }
        let name_start = i;
        while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
            i += 1;
        }
        let name = s[name_start..i].to_ascii_lowercase();
        while i < bytes.len() && bytes[i] != b'(' {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let arg_start = i + 1;
        while i < bytes.len() && bytes[i] != b')' {
            i += 1;
        }
        let args = parse_num_list(&s[arg_start..i.min(s.len())]);
        i += 1; // past ')'
        let t = match name.as_str() {
            "translate" => {
                Mat::translate(*args.first().unwrap_or(&0.0), *args.get(1).unwrap_or(&0.0))
            }
            "scale" => {
                let sx = *args.first().unwrap_or(&1.0);
                let sy = *args.get(1).unwrap_or(&sx);
                Mat {
                    a: sx,
                    b: 0.0,
                    c: 0.0,
                    d: sy,
                    e: 0.0,
                    f: 0.0,
                }
            }
            "rotate" => {
                let (sin, cos) = args.first().unwrap_or(&0.0).to_radians().sin_cos();
                let rot = Mat {
                    a: cos,
                    b: sin,
                    c: -sin,
                    d: cos,
                    e: 0.0,
                    f: 0.0,
                };
                if args.len() >= 3 {
                    Mat::translate(args[1], args[2])
                        .then(&rot)
                        .then(&Mat::translate(-args[1], -args[2]))
                } else {
                    rot
                }
            }
            "matrix" if args.len() == 6 => Mat {
                a: args[0],
                b: args[1],
                c: args[2],
                d: args[3],
                e: args[4],
                f: args[5],
            },
            "skewx" => Mat {
                a: 1.0,
                b: 0.0,
                c: args.first().unwrap_or(&0.0).to_radians().tan(),
                d: 1.0,
                e: 0.0,
                f: 0.0,
            },
            "skewy" => Mat {
                a: 1.0,
                b: args.first().unwrap_or(&0.0).to_radians().tan(),
                c: 0.0,
                d: 1.0,
                e: 0.0,
                f: 0.0,
            },
            _ => Mat::identity(),
        };
        m = m.then(&t);
    }
    m
}

fn parse_view_box(s: &str) -> Option<[f64; 4]> {
    let v = parse_num_list(s);
    if v.len() == 4 && v[2] > 0.0 && v[3] > 0.0 {
        Some([v[0], v[1], v[2], v[3]])
    } else {
        None
    }
}

fn parse_points(s: &str) -> Vec<(f64, f64)> {
    parse_num_list(s)
        .chunks_exact(2)
        .map(|c| (c[0], c[1]))
        .collect()
}

fn parse_num_list(s: &str) -> Vec<f64> {
    s.split(|c: char| c.is_whitespace() || c == ',')
        .filter(|t| !t.is_empty())
        .filter_map(|t| t.parse().ok())
        .collect()
}

/// Leading numeric value of a length, ignoring any unit suffix (`px`, `pt`, …).
fn parse_len(s: &str) -> Option<f64> {
    let s = s.trim();
    let end = s
        .find(|c: char| !matches!(c, '0'..='9' | '.' | '-' | '+' | 'e' | 'E'))
        .unwrap_or(s.len());
    s[..end].parse().ok()
}

fn parse_f64(s: &str) -> Option<f64> {
    s.trim().parse().ok()
}

fn attr_f(e: &Element, name: &str) -> f64 {
    e.attr(name).and_then(parse_len).unwrap_or(0.0)
}

// ── data: URI → SvgImage (so `<img src="data:image/svg+xml,…">` renders vector) ──

/// Parse an SVG `data:` URI — `data:image/svg+xml[;base64],…` (base64 or
/// percent-encoded payload). Returns `None` for non-SVG or unparsable data.
pub fn parse_data_uri(src: &str) -> Option<SvgImage> {
    let rest = src.strip_prefix("data:")?;
    let comma = rest.find(',')?;
    let meta = rest[..comma].to_ascii_lowercase();
    if !meta.contains("svg") {
        return None;
    }
    let data = &rest[comma + 1..];
    let markup = if meta.contains("base64") {
        String::from_utf8(base64_decode(data)?).ok()?
    } else {
        percent_decode(data)
    };
    parse_svg(&markup)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        // In a `data:` URI `+` is literal (not a space); only `%XX` is decoded.
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex_val(b[i + 1]), hex_val(b[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Minimal standard-alphabet base64 decoder (ignores whitespace and padding).
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &c in s.as_bytes() {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        let v = val(c)?;
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_shapes_and_viewbox() {
        let svg = r##"<svg viewBox="0 0 100 50" width="200" height="100">
            <rect x="10" y="10" width="30" height="20" fill="#ff0000"/>
            <circle cx="70" cy="25" r="15" fill="none" stroke="blue" stroke-width="2"/>
        </svg>"##;
        let img = parse_svg(svg).expect("parsed");
        assert_eq!(img.view_box, [0.0, 0.0, 100.0, 50.0]);
        assert_eq!((img.width, img.height), (200.0, 100.0));
        assert_eq!(img.prims.len(), 2, "rect + circle");
        let rect = &img.prims[0];
        match rect.fill {
            Some(Fill::Solid(c)) => assert_eq!(c, [1.0, 0.0, 0.0], "rect filled red"),
            _ => panic!("rect should have a solid red fill"),
        }
        let circ = &img.prims[1];
        assert!(
            circ.fill.is_none() && circ.stroke.is_some(),
            "circle stroked, not filled"
        );
    }

    #[test]
    fn use_element_renders_a_referenced_shape() {
        // A rect parked in <defs> is invisible until `<use>`d.
        let with_use = r##"<svg viewBox="0 0 100 100">
            <defs><rect id="r" width="10" height="10" fill="#ff0000"/></defs>
            <use href="#r" x="5" y="5"/>
        </svg>"##;
        let img = parse_svg(with_use).expect("use renders the referenced rect");
        assert_eq!(img.prims.len(), 1, "the <use> brings in the defs'd rect");
        match img.prims[0].fill {
            Some(Fill::Solid(c)) => assert_eq!(c, [1.0, 0.0, 0.0]),
            _ => panic!("referenced rect keeps its red fill"),
        }
        // Without the <use>, a defs-only rect draws nothing.
        let defs_only = r##"<svg viewBox="0 0 100 100"><defs><rect id="r" width="10" height="10"/></defs></svg>"##;
        assert!(
            parse_svg(defs_only).is_none(),
            "a defs-only rect renders nothing"
        );
    }

    #[test]
    fn svg_text_traces_glyph_outlines() {
        // `<text>` is rendered as filled vector glyph subpaths.
        let svg = r##"<svg viewBox="0 0 100 100"><text x="10" y="30" font-size="20" fill="#000000">Hi</text></svg>"##;
        let img = parse_svg(svg).expect("text renders");
        assert!(!img.prims.is_empty(), "SVG <text> traces glyph outlines");
    }

    #[test]
    fn linear_gradient_resolved_on_fill() {
        let svg = r##"<svg viewBox="0 0 100 100"><defs>
            <linearGradient id="g"><stop offset="0" stop-color="#ff0000"/><stop offset="1" stop-color="#0000ff"/></linearGradient>
            </defs><rect x="0" y="0" width="100" height="50" fill="url(#g)"/></svg>"##;
        let img = parse_svg(svg).expect("parsed");
        match &img.prims[0].fill {
            Some(Fill::Gradient(g)) => {
                assert_eq!(g.stops.len(), 2, "two stops");
                match g.kind {
                    GradKind::Linear { x1, x2, .. } => {
                        // objectBoundingBox: x1=0 → bbox min x (0), x2=1 → max x (100).
                        assert!(
                            (x1 - 0.0).abs() < 1e-6 && (x2 - 100.0).abs() < 1e-6,
                            "x1={x1} x2={x2}"
                        );
                    }
                    _ => panic!("linear gradient"),
                }
            }
            _ => panic!("rect should have a gradient fill"),
        }
    }

    #[test]
    fn group_transform_is_baked_into_coordinates() {
        // translate(100,0) shifts the rect's first Move x from 0 to 100.
        let svg = r#"<svg viewBox="0 0 200 100"><g transform="translate(100,0)">
            <rect x="0" y="0" width="10" height="10"/></g></svg>"#;
        let img = parse_svg(svg).unwrap();
        let first = img.prims[0].segs.iter().find_map(|s| match s {
            Seg::Move(x, _) => Some(*x),
            _ => None,
        });
        assert_eq!(first, Some(100.0), "translate baked into the Move");
    }

    #[test]
    fn fill_none_with_no_stroke_is_dropped() {
        let svg = r#"<svg viewBox="0 0 10 10"><rect width="10" height="10" fill="none"/></svg>"#;
        assert!(
            parse_svg(svg).is_none(),
            "invisible primitive yields no image"
        );
    }

    #[test]
    fn data_uri_percent_encoded_svg() {
        let uri = "data:image/svg+xml,%3Csvg%20viewBox%3D%220%200%2010%2010%22%3E%3Crect%20width%3D%2210%22%20height%3D%2210%22%2F%3E%3C%2Fsvg%3E";
        assert!(
            parse_data_uri(uri).is_some(),
            "percent-encoded svg data URI parses"
        );
        assert!(
            parse_data_uri("data:image/png;base64,iVBORw0K").is_none(),
            "non-svg data URI rejected"
        );
    }

    #[test]
    fn base64_decoder_basics() {
        assert_eq!(base64_decode("PHN2Zz4=").unwrap(), b"<svg>");
    }

    // ── <text> rendering ─────────────────────────────────────────────────────

    /// Min/max X of a primitive's segment coordinates (after the CTM bake).
    fn prim_x_range(p: &Prim) -> (f64, f64) {
        let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
        for s in &p.segs {
            let xs: &[f64] = match s {
                Seg::Move(x, _) | Seg::Line(x, _) => &[*x][..],
                Seg::Cubic(a, _, c, _, e, _) => &[*a, *c, *e][..],
                Seg::Close => &[][..],
            };
            for &x in xs {
                lo = lo.min(x);
                hi = hi.max(x);
            }
        }
        (lo, hi)
    }

    #[test]
    fn text_traces_glyph_outlines_as_filled_paths() {
        let svg = r##"<svg viewBox="0 0 200 50">
            <text x="10" y="40" font-size="30" fill="#ff0000">Hi</text>
        </svg>"##;
        let img = parse_svg(svg).expect("text yields an image");
        // Two visible letters → at least one filled primitive each, all red.
        assert!(!img.prims.is_empty(), "glyphs produce primitives");
        for p in &img.prims {
            match p.fill {
                Some(Fill::Solid(c)) => assert_eq!(c, [1.0, 0.0, 0.0], "text fill is red"),
                _ => panic!("text primitive should be solid-filled"),
            }
            assert!(p.stroke.is_none(), "text is filled, not stroked");
            // Glyph contours flatten to polylines: Moves/Lines/Close, no cubics.
            assert!(
                p.segs
                    .iter()
                    .any(|s| matches!(s, Seg::Line(..)) || matches!(s, Seg::Move(..))),
                "glyph contour has line segments"
            );
            assert!(
                !p.segs.iter().any(|s| matches!(s, Seg::Cubic(..))),
                "TrueType contours arrive pre-flattened (no cubics)"
            );
        }
        // Glyphs sit to the right of the start x (=10) and within the viewBox.
        let (lo, hi) = img
            .prims
            .iter()
            .map(prim_x_range)
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(l, h), (a, b)| {
                (l.min(a), h.max(b))
            });
        assert!(lo >= 9.0 && hi <= 200.0, "text laid out near x=10 (lo={lo} hi={hi})");
    }

    #[test]
    fn text_anchor_middle_centers_the_run() {
        // Same content, one start-anchored and one middle-anchored at the same x.
        let start = parse_svg(
            r#"<svg viewBox="0 0 200 50"><text x="100" y="30" font-size="20">ABC</text></svg>"#,
        )
        .unwrap();
        let middle = parse_svg(
            r#"<svg viewBox="0 0 200 50"><text x="100" y="30" font-size="20" text-anchor="middle">ABC</text></svg>"#,
        )
        .unwrap();
        let min_x = |img: &SvgImage| {
            img.prims
                .iter()
                .map(|p| prim_x_range(p).0)
                .fold(f64::INFINITY, f64::min)
        };
        let (sx, mx) = (min_x(&start), min_x(&middle));
        // The middle-anchored run starts to the LEFT of the start-anchored one by
        // ~half the run width (a few pt for "ABC" at 20px).
        assert!(mx < sx - 5.0, "middle anchor shifts left (start={sx} middle={mx})");
    }

    #[test]
    fn text_with_no_fill_draws_nothing() {
        let svg =
            r#"<svg viewBox="0 0 100 30"><text x="0" y="20" fill="none">x</text></svg>"#;
        assert!(
            parse_svg(svg).is_none(),
            "fill:none text produces no primitives"
        );
    }

    #[test]
    fn tspan_repositions_within_text() {
        let svg = r##"<svg viewBox="0 0 200 50">
            <text x="10" y="40" font-size="20" fill="#000">
                A<tspan dx="50">B</tspan>
            </text>
        </svg>"##;
        let img = parse_svg(svg).expect("tspan text parses");
        // The 'B' (after dx=50) must sit well to the right of the 'A'.
        let xs: Vec<f64> = img.prims.iter().map(|p| prim_x_range(p).0).collect();
        let max_x = xs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        assert!(max_x > 55.0, "tspan dx shifts the glyph right (max_x={max_x})");
    }

    #[test]
    fn text_transform_is_baked_into_glyph_coords() {
        // translate(100,0) shifts every glyph coordinate right by 100.
        let plain = parse_svg(
            r#"<svg viewBox="0 0 300 50"><text x="0" y="30" font-size="20">o</text></svg>"#,
        )
        .unwrap();
        let shifted = parse_svg(
            r#"<svg viewBox="0 0 300 50"><g transform="translate(100,0)"><text x="0" y="30" font-size="20">o</text></g></svg>"#,
        )
        .unwrap();
        let lo = |img: &SvgImage| {
            img.prims
                .iter()
                .map(|p| prim_x_range(p).0)
                .fold(f64::INFINITY, f64::min)
        };
        assert!(
            (lo(&shifted) - lo(&plain) - 100.0).abs() < 1.0,
            "translate baked into glyph outline"
        );
    }

    // ── <pattern> tiling ──────────────────────────────────────────────────────

    /// Distinct integer cell origins (rounded min-x, min-y) the primitives sit
    /// at — a proxy for "how many tiles were laid down".
    fn distinct_cell_origins(img: &SvgImage) -> usize {
        let mut origins: Vec<(i64, i64)> = img
            .prims
            .iter()
            .map(|p| {
                let (mut lo_x, mut lo_y) = (f64::INFINITY, f64::INFINITY);
                for s in &p.segs {
                    if let Seg::Move(x, y) | Seg::Line(x, y) = s {
                        lo_x = lo_x.min(*x);
                        lo_y = lo_y.min(*y);
                    }
                }
                (lo_x.round() as i64, lo_y.round() as i64)
            })
            .collect();
        origins.sort_unstable();
        origins.dedup();
        origins.len()
    }

    #[test]
    fn pattern_userspace_tiles_across_shape() {
        // A 40×20 rect filled by a 10×10 userSpaceOnUse pattern whose single child
        // fills the whole cell green → 4 cols × 2 rows = 8 tiles, each a clipped
        // green rect. The flat fallback would have produced a SINGLE primitive.
        let svg = r##"<svg viewBox="0 0 40 20"><defs>
            <pattern id="p" patternUnits="userSpaceOnUse" width="10" height="10">
                <rect x="0" y="0" width="10" height="10" fill="#00ff00"/>
            </pattern></defs>
            <rect x="0" y="0" width="40" height="20" fill="url(#p)"/></svg>"##;
        let img = parse_svg(svg).expect("pattern-filled rect parses");
        assert!(
            img.prims.len() > 1,
            "tiling emits several primitives, not a flat fallback (got {})",
            img.prims.len()
        );
        // Every tile is a solid green fill (the cell's own paint, not the shape's).
        for p in &img.prims {
            match p.fill {
                Some(Fill::Solid(c)) => assert_eq!(c, [0.0, 1.0, 0.0], "tile is green"),
                _ => panic!("each tile should be solid-filled"),
            }
        }
        // The tiles sit at multiple distinct cell origins covering the 40×20 box.
        assert!(
            distinct_cell_origins(&img) >= 4,
            "content repeats across distinct cells (origins={})",
            distinct_cell_origins(&img)
        );
        // All tile geometry stays within the shape's bbox (clipped to it).
        for p in &img.prims {
            for s in &p.segs {
                if let Seg::Move(x, y) | Seg::Line(x, y) = s {
                    assert!(
                        *x >= -0.01 && *x <= 40.01 && *y >= -0.01 && *y <= 20.01,
                        "tile clipped to bbox (x={x} y={y})"
                    );
                }
            }
        }
    }

    #[test]
    fn pattern_object_bounding_box_default_units() {
        // Default patternUnits=objectBoundingBox: width/height are fractions of the
        // shape bbox. 0.25×0.5 over a 40×20 rect → 10×10 tiles → 4×2 grid again.
        let svg = r##"<svg viewBox="0 0 40 20"><defs>
            <pattern id="p" width="0.25" height="0.5" patternContentUnits="objectBoundingBox">
                <rect x="0" y="0" width="0.25" height="0.5" fill="#0000ff"/>
            </pattern></defs>
            <rect x="0" y="0" width="40" height="20" fill="url(#p)"/></svg>"##;
        let img = parse_svg(svg).expect("objectBoundingBox pattern parses");
        assert!(
            img.prims.len() > 1,
            "objectBoundingBox pattern tiles (got {})",
            img.prims.len()
        );
        assert!(
            distinct_cell_origins(&img) >= 4,
            "obb tiles repeat across cells (origins={})",
            distinct_cell_origins(&img)
        );
    }

    #[test]
    fn solid_fill_unchanged_control() {
        // Control: a plain solid fill is still exactly one primitive, one fill.
        let svg = r##"<svg viewBox="0 0 40 20">
            <rect x="0" y="0" width="40" height="20" fill="#ff0000"/></svg>"##;
        let img = parse_svg(svg).expect("solid rect parses");
        assert_eq!(img.prims.len(), 1, "solid fill is a single primitive");
        match img.prims[0].fill {
            Some(Fill::Solid(c)) => assert_eq!(c, [1.0, 0.0, 0.0], "solid stays red"),
            _ => panic!("control rect should be a solid fill"),
        }
    }

    #[test]
    fn empty_pattern_falls_back_to_solid_fill() {
        // A pattern with no drawable children can't tile → the inherited solid
        // fill paints the shape (one primitive), never an empty result.
        let svg = r##"<svg viewBox="0 0 20 20"><defs>
            <pattern id="p" patternUnits="userSpaceOnUse" width="5" height="5"></pattern>
            </defs>
            <rect x="0" y="0" width="20" height="20" fill="url(#p)" stroke="#000"/></svg>"##;
        let img = parse_svg(svg).expect("empty pattern falls back");
        // Fallback path: the rect keeps its (default black) fill + the stroke.
        assert_eq!(img.prims.len(), 1, "fallback is the single shape primitive");
        assert!(
            img.prims[0].fill.is_some() && img.prims[0].stroke.is_some(),
            "fallback keeps inherited fill and the stroke"
        );
    }

    #[test]
    fn pattern_clips_to_circle_contour_not_bbox() {
        // A circle (r=20 at 50,50) filled with a fine 4×4 userSpaceOnUse pattern.
        // Tiles must be clipped to the CIRCLE, never leaking into the bbox corners
        // (e.g. near (30,30) — inside the bbox but outside the disc).
        let svg = r##"<svg viewBox="0 0 100 100"><defs>
            <pattern id="p" patternUnits="userSpaceOnUse" width="4" height="4">
                <rect x="0" y="0" width="4" height="4" fill="#00aa00"/>
            </pattern></defs>
            <circle cx="50" cy="50" r="20" fill="url(#p)"/></svg>"##;
        let img = parse_svg(svg).expect("pattern-filled circle parses");
        assert!(img.prims.len() > 1, "the circle tiles into many cells");
        // Every emitted tile vertex must lie inside the disc (a small tolerance
        // covers the polyline flattening of the circle outline).
        let r = 20.0_f64;
        let tol = 0.75_f64; // outline is flattened; allow a sub-pixel slack
        let mut max_d = 0.0_f64;
        for p in &img.prims {
            for s in &p.segs {
                if let Seg::Move(x, y) | Seg::Line(x, y) = s {
                    let d = ((x - 50.0).powi(2) + (y - 50.0).powi(2)).sqrt();
                    max_d = max_d.max(d);
                    assert!(
                        d <= r + tol,
                        "tile vertex ({x:.2},{y:.2}) leaks outside the circle (d={d:.3} > {})",
                        r + tol
                    );
                }
            }
        }
        // Sanity: tiling actually reaches close to the rim (not a tiny blob).
        assert!(
            max_d > r * 0.7,
            "tiles cover most of the disc (max_d={max_d:.2})"
        );
        // A bbox-only clip would have placed vertices out at the corners (~(30,30)
        // → d≈28). The contour clip keeps everything within ~20.
        assert!(
            max_d < r + tol,
            "no vertex sits in a bbox corner outside the disc (max_d={max_d:.2})"
        );
    }

    #[test]
    fn pattern_transform_rotates_the_tile_grid() {
        // `patternTransform="rotate(30)"` rotates the tile lattice: emitted tile
        // edges are no longer all axis-aligned. The un-transformed control (below)
        // produces only axis-aligned edges.
        let rotated = r##"<svg viewBox="0 0 60 60"><defs>
            <pattern id="p" patternUnits="userSpaceOnUse" width="10" height="10"
                     patternTransform="rotate(30)">
                <rect x="0" y="0" width="10" height="10" fill="#3366cc"/>
            </pattern></defs>
            <rect x="0" y="0" width="60" height="60" fill="url(#p)"/></svg>"##;
        let img = parse_svg(rotated).expect("patternTransform pattern parses");
        assert!(img.prims.len() > 1, "rotated pattern still tiles");
        assert!(
            has_oblique_edge(&img),
            "rotate(30) yields oblique (non-axis-aligned) tile edges"
        );

        // Control: identical pattern WITHOUT patternTransform → axis-aligned only.
        let upright = r##"<svg viewBox="0 0 60 60"><defs>
            <pattern id="p" patternUnits="userSpaceOnUse" width="10" height="10">
                <rect x="0" y="0" width="10" height="10" fill="#3366cc"/>
            </pattern></defs>
            <rect x="0" y="0" width="60" height="60" fill="url(#p)"/></svg>"##;
        let up = parse_svg(upright).expect("upright pattern parses");
        assert!(
            !has_oblique_edge(&up),
            "an un-transformed grid of axis-aligned rects has only axis-aligned edges"
        );
    }

    #[test]
    fn nested_pattern_inside_pattern_tiles() {
        // The OUTER pattern's tile child is itself filled by url(#inner): the inner
        // pattern must resolve and paint, so the inner colour appears in the output.
        let svg = r##"<svg viewBox="0 0 40 40"><defs>
            <pattern id="inner" patternUnits="userSpaceOnUse" width="5" height="5">
                <rect x="0" y="0" width="5" height="5" fill="#ff0000"/>
            </pattern>
            <pattern id="outer" patternUnits="userSpaceOnUse" width="20" height="20">
                <rect x="0" y="0" width="20" height="20" fill="url(#inner)"/>
            </pattern></defs>
            <rect x="0" y="0" width="40" height="40" fill="url(#outer)"/></svg>"##;
        let img = parse_svg(svg).expect("nested pattern parses");
        assert!(img.prims.len() > 1, "nested pattern tiles");
        // The inner pattern's red must have been resolved (not a flat fallback).
        let has_red = img
            .prims
            .iter()
            .any(|p| matches!(p.fill, Some(Fill::Solid(c)) if c == [1.0, 0.0, 0.0]));
        assert!(
            has_red,
            "the inner pattern resolves and paints red tiles inside the outer tiles"
        );
        // Every tile is the inner red (the outer tile has no other paint).
        for p in &img.prims {
            if let Some(Fill::Solid(c)) = p.fill {
                assert_eq!(c, [1.0, 0.0, 0.0], "every nested tile is the inner red");
            }
        }
    }

    #[test]
    fn pattern_self_reference_is_bounded() {
        // A pattern whose tile references ITSELF must not recurse forever; the
        // depth guard breaks the cycle and the parse terminates (with a fallback).
        let svg = r##"<svg viewBox="0 0 30 30"><defs>
            <pattern id="p" patternUnits="userSpaceOnUse" width="10" height="10">
                <rect x="0" y="0" width="10" height="10" fill="url(#p)"/>
            </pattern></defs>
            <rect x="0" y="0" width="30" height="30" fill="url(#p)" stroke="#000"/></svg>"##;
        // The assertion is simply that this returns (no stack overflow / hang).
        let _ = parse_svg(svg);
    }

    #[test]
    fn tile_gradient_keeps_per_stop_alpha() {
        // A pattern tile filled by a gradient with a semi-transparent stop: the
        // tiled primitives must carry that per-stop alpha (alpha < 1) on the
        // gradient fill, so the renderer makes the tile semi-transparent rather
        // than treating it as opaque.
        let svg = r##"<svg viewBox="0 0 40 20"><defs>
            <linearGradient id="g">
                <stop offset="0" stop-color="#ff0000" stop-opacity="1"/>
                <stop offset="1" stop-color="#0000ff" stop-opacity="0.3"/>
            </linearGradient>
            <pattern id="p" patternUnits="userSpaceOnUse" width="20" height="20">
                <rect x="0" y="0" width="20" height="20" fill="url(#g)"/>
            </pattern></defs>
            <rect x="0" y="0" width="40" height="20" fill="url(#p)"/></svg>"##;
        let img = parse_svg(svg).expect("gradient-in-pattern parses");
        assert!(img.prims.len() > 1, "the gradient pattern tiles");
        // At least one tile carries a gradient fill whose stops include alpha<1.
        let semi = img.prims.iter().any(|p| match &p.fill {
            Some(Fill::Gradient(g)) => g.stops.iter().any(|s| s.alpha < 0.999),
            _ => false,
        });
        assert!(
            semi,
            "tiled gradient preserves the 0.3 stop alpha (per-stop, not flattened away)"
        );
        // And the fully-opaque stop is preserved too (not clamped to the min).
        let has_opaque_stop = img.prims.iter().any(|p| match &p.fill {
            Some(Fill::Gradient(g)) => g.stops.iter().any(|s| s.alpha > 0.999),
            _ => false,
        });
        assert!(has_opaque_stop, "the opaque stop is also preserved");
    }

    /// True if any primitive has a segment edge that is neither horizontal nor
    /// vertical (i.e. the tile grid is rotated/skewed).
    fn has_oblique_edge(img: &SvgImage) -> bool {
        for p in &img.prims {
            let mut prev: Option<(f64, f64)> = None;
            for s in &p.segs {
                let pt = match s {
                    Seg::Move(x, y) | Seg::Line(x, y) => Some((*x, *y)),
                    _ => None,
                };
                if let (Some(a), Some(b)) = (prev, pt) {
                    let dx = (b.0 - a.0).abs();
                    let dy = (b.1 - a.1).abs();
                    if dx > 1e-3 && dy > 1e-3 {
                        return true;
                    }
                }
                if let Some(q) = pt {
                    prev = Some(q);
                }
                if matches!(s, Seg::Move(..)) {
                    prev = pt;
                }
            }
        }
        false
    }
}
