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
    let mut prims = Vec::new();
    walk(
        &svg.children,
        Mat::identity(),
        Paint::root(),
        &ids,
        &grads,
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
            "g" | "a" | "svg" => walk(&e.children, ctm, paint, ids, grads, out, depth),
            "rect" => push(out, rect_segs(e), ctm, paint, furl, grads),
            "circle" => {
                let r = attr_f(e, "r");
                push(
                    out,
                    ellipse_segs(attr_f(e, "cx"), attr_f(e, "cy"), r, r),
                    ctm,
                    paint,
                    furl,
                    grads,
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
                grads,
            ),
            "line" => push(out, line_segs(e), ctm, paint, furl, grads),
            "polyline" => push(out, poly_segs(e, false), ctm, paint, furl, grads),
            "polygon" => push(out, poly_segs(e, true), ctm, paint, furl, grads),
            "path" => push(
                out,
                e.attr("d").map(parse_path_d).unwrap_or_default(),
                ctm,
                paint,
                furl,
                grads,
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
/// invisible: empty geometry, or no fill and no stroke). `fill_url` is a gradient
/// id from `fill="url(#id)"`, resolved against `grads`.
fn push(
    out: &mut Vec<Prim>,
    segs: Vec<Seg>,
    ctm: Mat,
    paint: Paint,
    fill_url: Option<&str>,
    grads: &Grads,
) {
    if segs.is_empty() {
        return;
    }
    let segs: Vec<Seg> = segs.iter().map(|s| transform_seg(s, &ctm)).collect();
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
}
