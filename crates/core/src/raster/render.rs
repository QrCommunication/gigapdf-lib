//! Vector content rasterizer: interpret a page's content stream (paths, fills,
//! strokes, colours, CTM) and paint into a [`Canvas`]. Zero dependencies.
//!
//! This is slice 1 of the renderer — vector graphics only. Text glyphs and
//! images are drawn by later slices on top of the same canvas and fill engine.
//! Clipping (`W`) is currently ignored (paths over-paint rather than clip).

use std::collections::BTreeMap;

use super::canvas::{BlendMode, Canvas, ClipMask, Edge};
use crate::content::{parse_content, Operation, PageMatrix as Matrix};
use crate::font::cmap::TextDecoder;
use crate::font::GlyphSource;
use crate::object::Object;

/// One vertex of a mesh-shading triangle: a position in the shading's own
/// coordinate space plus its already-resolved device-RGB colour. The colour is
/// baked at parse time (the `/ColorSpace` / `/Function` is evaluated by the
/// document, exactly like the axial/radial ramp) so the rasterizer stays free of
/// the object graph and only interpolates.
#[derive(Debug, Clone, Copy)]
pub struct MeshVertex {
    /// X in shading space.
    pub x: f64,
    /// Y in shading space.
    pub y: f64,
    /// Resolved device-RGB colour at this vertex.
    pub color: [u8; 3],
}

/// The shape of a PDF shading (ISO 32000-1 §8.7.4.5): a function-based (type 1)
/// shading, an axial (type 2) gradient, a radial (type 3) gradient, or a mesh
/// (types 4–7) reduced to a list of Gouraud-shaded triangles. Coordinates are in
/// the shading's own space; for function-based/axial/radial the renderer maps
/// device pixels back into this space to evaluate the colour, while a mesh maps
/// each triangle *forward* to device space and barycentric-interpolates the
/// per-vertex colours.
#[derive(Debug, Clone)]
pub enum ShadingKind {
    /// Function-based (type 1): a 2-in→N-out `/Function` colours each point of a
    /// 2-D `/Domain`. The colour grid is pre-sampled into [`Shading::func_grid`]
    /// (a `grid_w × grid_h` lattice over the domain); `inv_matrix` maps a point
    /// in the shading's target coordinate space back into domain space (the
    /// inverse of the shading's `/Matrix`). A point outside the domain is not
    /// painted.
    Function {
        /// `/Domain` `[x0 x1 y0 y1]` in domain space.
        domain: [f64; 4],
        /// Columns of the pre-sampled colour grid (x axis of the domain).
        grid_w: usize,
        /// Rows of the pre-sampled colour grid (y axis of the domain).
        grid_h: usize,
        /// Shading-target space → domain space (inverse of `/Matrix`).
        inv_matrix: Matrix,
    },
    /// Axial: the gradient runs along the segment `(x0,y0)→(x1,y1)`.
    Axial { x0: f64, y0: f64, x1: f64, y1: f64 },
    /// Radial: between circle `(x0,y0,r0)` and circle `(x1,y1,r1)`.
    Radial {
        x0: f64,
        y0: f64,
        r0: f64,
        x1: f64,
        y1: f64,
        r1: f64,
    },
    /// Mesh (types 4–7): a flat list of Gouraud triangles. Free-form (4) and
    /// lattice (5) Gouraud meshes contribute their triangles directly; Coons (6)
    /// and tensor (7) patches are subdivided into a triangle grid by the parser.
    /// Each triple of [`MeshVertex`] is one triangle (`triangles.len()` is a
    /// multiple of 3).
    Mesh { triangles: Vec<MeshVertex> },
}

/// A resolved shading ready to paint: its geometry, a pre-sampled 256-entry RGB
/// colour ramp across `t ∈ [0,1]` (the `/Function` already evaluated), the two
/// `/Extend` flags, and the matrix mapping shading space to the device.
#[derive(Debug, Clone)]
pub struct Shading {
    /// Gradient geometry, in shading space.
    pub kind: ShadingKind,
    /// 256 RGB triples sampled across the colour function (`t = i/255`). Used by
    /// axial/radial; empty for function-based (type 1, see `func_grid`) and mesh.
    pub ramp: Vec<[u8; 3]>,
    /// Pre-sampled `grid_w × grid_h` RGB colour grid over the 2-D `/Domain` of a
    /// function-based (type 1) shading, row-major (`y * grid_w + x`). Empty for
    /// every other shading kind.
    pub func_grid: Vec<[u8; 3]>,
    /// `[extend_start, extend_end]` — paint beyond `t<0` / `t>1`.
    pub extend: [bool; 2],
    /// Maps shading space → device pixels (composed by the caller).
    pub to_device: Matrix,
}

impl Shading {
    /// Look up the ramp colour for a `t` in `[0,1]`.
    fn color_at(&self, t: f64) -> [u8; 3] {
        let n = self.ramp.len();
        if n == 0 {
            return [0, 0, 0];
        }
        let idx = ((t.clamp(0.0, 1.0) * (n - 1) as f64).round() as usize).min(n - 1);
        self.ramp[idx]
    }
}

/// A form XObject (`/Subtype /Form`) resolved for rasterization: its content
/// bytes, `/Matrix`, `/BBox`, the fonts and images of its own `/Resources`, and a
/// child resource context for any forms/shadings *it* invokes. Returned by
/// [`ResourceCtx::form_xobject`] and reused for type-1 tiling patterns.
//
// `Debug` is hand-written (below): the `ctx` field is a `Box<dyn ResourceCtx>`,
// which has no `Debug` bound, so the field is summarised rather than printed.
// The lifetime `'a` ties the child context to the provider that produced it
// (typically the document borrow), so a nested form can resolve its own names.
pub struct FormXObject<'a> {
    /// Decoded content stream of the form.
    pub content: Vec<u8>,
    /// The form's `/Matrix` (form space → the space where `Do` was invoked).
    pub matrix: Matrix,
    /// The form's `/BBox` `[x0 y0 x1 y1]` (a clip on the form's own marks).
    pub bbox: [f64; 4],
    /// Fonts of the form's own `/Resources`.
    pub fonts: RenderFonts,
    /// Images of the form's own `/Resources`.
    pub images: RenderImages,
    /// Resource context for nested resources of *this* form.
    pub ctx: Box<dyn ResourceCtx + 'a>,
}

impl std::fmt::Debug for FormXObject<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FormXObject")
            .field("content_len", &self.content.len())
            .field("matrix", &self.matrix)
            .field("bbox", &self.bbox)
            .field("fonts", &self.fonts.len())
            .field("images", &self.images.len())
            .finish_non_exhaustive()
    }
}

/// Parameters pulled from an ExtGState (`gs`) that affect compositing: a constant
/// non-stroking alpha (`/ca`), a separable blend mode (`/BM`), and an optional
/// soft mask resolved to a full-page device-resolution alpha buffer (`/SMask`).
#[derive(Debug, Clone, Default)]
pub struct ExtGStateParams {
    /// `/ca` non-stroking constant alpha, if present.
    pub fill_alpha: Option<f64>,
    /// `/BM` blend mode (defaults to Normal).
    pub blend: BlendMode,
    /// `/SMask` resolved to per-device-pixel alpha (`None` = `/None`, no mask).
    pub soft_mask: Option<ClipMask>,
}

/// Resolves the named resources a content stream refers to (form XObjects,
/// shadings, patterns, ExtGStates) into ready-to-render data. Implemented by the
/// document, which owns the object graph; the rasterizer stays object-graph-free.
///
/// Every method is keyed by the resource *name* used in the stream (e.g. the `Fm0`
/// in `/Fm0 Do`) and resolved against the **current** resources scope — a nested
/// form's context resolves names against that form's `/Resources`.
pub trait ResourceCtx {
    /// Resolve a `/Form` XObject named `name` (for the `Do` operator).
    fn form_xobject(&self, name: &[u8]) -> Option<FormXObject<'_>>;
    /// Resolve a shading named `name` in the `/Shading` sub-dictionary (`sh`).
    fn shading(&self, name: &[u8]) -> Option<Shading>;
    /// Resolve a shading **pattern** (PatternType 2) named `name`, returning the
    /// shading already carrying its pattern matrix in `to_device` *relative to the
    /// page base* (the caller composes the base in). For `scn`/`SCN`.
    fn pattern_shading(&self, name: &[u8]) -> Option<Shading>;
    /// Resolve a **tiling** pattern (PatternType 1) named `name` as a form to draw
    /// once across the filled area (a single tile, not infinitely repeated).
    fn tiling_pattern(&self, name: &[u8]) -> Option<FormXObject<'_>>;
    /// Resolve an ExtGState named `name` (the `gs` operator).
    fn ext_gstate(&self, name: &[u8]) -> Option<ExtGStateParams>;
    /// Resolve a *named* colour space (`cs`/`CS` operand) from the current
    /// `/Resources /ColorSpace` dictionary and convert `comps` to device RGB.
    /// Returns `None` when the name isn't a colour space the document can resolve
    /// (the interpreter then falls back to an arity-based Device interpretation).
    /// Device-space names (`DeviceGray`/`DeviceRGB`/`DeviceCMYK`) are resolved
    /// here too so a `cs /DeviceCMYK … scn` path is exact.
    fn resolve_color(&self, name: &[u8], comps: &[f64]) -> Option<[u8; 3]>;
    /// Decode an inline image (the `raw` bytes the content parser captured for a
    /// `BI`…`EI` operation: everything after `BI` up to and including `EI`) into a
    /// ready-to-blit [`RenderImage`], reusing the document's image pipeline.
    /// The default returns `None` (contexts that don't own the object graph can't
    /// decode); the document-backed context overrides it. ISO 32000-1 §8.9.7.
    fn inline_image(&self, _raw: &[u8]) -> Option<RenderImage> {
        None
    }

    /// Whether the optional-content (layer) referenced by a `/OC … BDC` marked
    /// sequence is **visible** in the default configuration (ISO 32000-1 §8.11).
    /// `prop` is the second operand of `/OC <prop> BDC`: a `/Name` keyed in the
    /// current `/Resources /Properties` (resolving to an OCG or OCMD), or an
    /// inline OCG/OCMD dictionary. A `false` return makes the rasterizer skip
    /// every painting operator up to the matching `EMC`.
    ///
    /// The default returns `true` (contexts without an object graph can't tell
    /// what's hidden, so they show everything) — the prior behaviour.
    fn oc_marked_visible(&self, _prop: &Object) -> bool {
        true
    }

    /// Whether an XObject named `name` (in the current `/Resources /XObject`) is
    /// **visible** given its own `/OC` entry (ISO 32000-1 §8.11.3.3). A `Do`
    /// targeting a hidden XObject paints nothing. The default returns `true`.
    fn oc_xobject_visible(&self, _name: &[u8]) -> bool {
        true
    }
}

/// A resource context that resolves nothing — used by [`render_content_into`],
/// which keeps the simple (page-content-only, no nested forms/shadings) behaviour
/// for callers that don't supply a document-backed [`ResourceCtx`].
#[derive(Debug)]
pub struct NoResources;

impl ResourceCtx for NoResources {
    fn form_xobject(&self, _name: &[u8]) -> Option<FormXObject<'_>> {
        None
    }
    fn shading(&self, _name: &[u8]) -> Option<Shading> {
        None
    }
    fn pattern_shading(&self, _name: &[u8]) -> Option<Shading> {
        None
    }
    fn tiling_pattern(&self, _name: &[u8]) -> Option<FormXObject<'_>> {
        None
    }
    fn ext_gstate(&self, _name: &[u8]) -> Option<ExtGStateParams> {
        None
    }
    fn resolve_color(&self, _name: &[u8], _comps: &[f64]) -> Option<[u8; 3]> {
        None
    }
}

/// A font ready to render: its embedded glyph program (TrueType or CFF, when
/// available) and the code→Unicode decoder used to pick glyphs for simple fonts.
#[derive(Debug, Clone)]
pub struct RenderFont {
    /// The parsed embedded glyph program, if the font embeds one.
    pub program: Option<GlyphSource>,
    /// Code→Unicode decoder (drives glyph lookup for simple fonts).
    pub decoder: TextDecoder,
    /// Composite (Type0) font: 2-byte codes, glyph id taken as the CID.
    pub two_byte: bool,
    /// Composite-font `/Encoding` CMap mapping a 2-byte code → CID (a predefined
    /// CJK CMap or an embedded CMap stream). `None` ⇒ Identity-H (code == CID),
    /// the common case. Applied before `cid_to_gid` resolves CID → glyph id.
    pub code_to_cid: Option<crate::font::cmap::Cmap>,
    /// Simple-font character-code → glyph-id map, resolved from the PDF
    /// `/Encoding` (base + `/Differences`) against the program's own charset.
    /// Primary glyph-selection path for simple fonts whose program has no usable
    /// Unicode cmap (notably subset CFF, where `gid_for_unicode` is unavailable).
    /// `None` falls back to the code→Unicode→cmap path.
    pub code_to_gid: Option<BTreeMap<u32, u16>>,
    /// Composite-font CID → glyph-id map from a non-identity `/CIDToGIDMap`
    /// stream. `None` means identity (CID is the glyph id), the common case.
    pub cid_to_gid: Option<Vec<u16>>,
    /// `/Type3` font payload: each glyph is a PDF content stream in `/CharProcs`,
    /// drawn under `/FontMatrix`. `Some` only for Type3 fonts, which carry no
    /// `program`; [`show_text`] then executes the per-code glyph procedure
    /// instead of filling an outline. `None` for all other font types.
    pub type3: Option<Type3Glyphs>,
}

/// A `/Type3` font's drawable glyph descriptions (ISO 32000-1 §9.6.5). Unlike
/// every other font kind, a Type3 glyph is **not** an outline in an embedded
/// program: it's a small PDF content stream (`/CharProcs`) positioned by the
/// font's `/FontMatrix` and selected by `/Encoding` `/Differences`. The fields
/// are fully owned (no document borrow) so a [`RenderFont`] stays `'static` and
/// `Clone`; [`show_text`] runs each proc through the same content-stream
/// rasterizer used for page content and form XObjects.
#[derive(Debug, Clone)]
pub struct Type3Glyphs {
    /// The font's `/FontMatrix` `[a b c d e f]` (glyph space → text space).
    pub font_matrix: [f64; 6],
    /// Character code → decoded `/CharProcs` content stream. A code absent here
    /// (no `/Differences` entry, or a name with no matching proc) draws nothing
    /// but still advances by its `/Widths` entry.
    pub char_procs: BTreeMap<u32, Vec<u8>>,
    /// Fonts of the font's `/Resources` (a glyph proc may set `Tf` and show
    /// nested text). Empty when the font declares no `/Resources /Font`.
    pub fonts: RenderFonts,
    /// Images of the font's `/Resources` (a glyph proc may `Do` an image).
    pub images: RenderImages,
}

/// Per-page render fonts, keyed by font resource name (as used by `Tf`).
pub type RenderFonts = BTreeMap<Vec<u8>, RenderFont>;

/// A decoded image XObject ready to blit (8-bit RGBA, row-major top-to-bottom).
#[derive(Debug, Clone)]
pub struct RenderImage {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// `width*height*4` RGBA bytes.
    pub rgba: Vec<u8>,
    /// `/ImageMask true`: a 1-bit stencil. The RGB samples are ignored and the
    /// *current fill colour* of the graphics state is painted through the
    /// unmasked pixels (their alpha is the stencil coverage). `false` for an
    /// ordinary image whose own RGB is painted.
    pub stencil: bool,
}

/// Per-page image XObjects, keyed by resource name (as used by `Do`).
pub type RenderImages = BTreeMap<Vec<u8>, RenderImage>;

/// Blit an image XObject into the canvas. The image fills the unit square in
/// user space, mapped to the device by `ctm` then `base`; we inverse-map each
/// device pixel to a texel so up- and down-scaling both work without gaps.
/// `global_alpha` (`0.0..=1.0`) scales the image's own per-pixel alpha — used to
/// honour an annotation appearance's `/CA` opacity. An optional `clip` (active
/// `W` clip ∩ soft mask) further modulates each pixel, and `blend` selects the
/// compositing mode. For an `/ImageMask` stencil (`image.stencil`), `fill` is the
/// current fill colour painted through the unmasked pixels (the texel RGB is
/// ignored); for an ordinary image `fill` is unused.
#[allow(clippy::too_many_arguments)]
fn blit_image_clipped(
    canvas: &mut Canvas,
    image: &RenderImage,
    ctm: &Matrix,
    base: &Matrix,
    global_alpha: f64,
    clip: Option<&ClipMask>,
    blend: BlendMode,
    fill: [u8; 3],
) {
    if image.width == 0 || image.height == 0 {
        return;
    }
    let m = ctm.then(base); // (u, v) in [0,1]² → device
    let [a, b, c, d, e, f] = m.0;
    let det = a * d - b * c;
    if det.abs() < 1e-12 {
        return;
    }

    let corners = [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)].map(|(u, v)| m.apply(u, v));
    let min_x = corners
        .iter()
        .map(|p| p.0)
        .fold(f64::INFINITY, f64::min)
        .floor()
        .max(0.0) as i32;
    let max_x = corners
        .iter()
        .map(|p| p.0)
        .fold(f64::NEG_INFINITY, f64::max)
        .ceil()
        .min(canvas.width as f64) as i32;
    let min_y = corners
        .iter()
        .map(|p| p.1)
        .fold(f64::INFINITY, f64::min)
        .floor()
        .max(0.0) as i32;
    let max_y = corners
        .iter()
        .map(|p| p.1)
        .fold(f64::NEG_INFINITY, f64::max)
        .ceil()
        .min(canvas.height as f64) as i32;

    let (w, h) = (image.width as usize, image.height as usize);
    for py in min_y..max_y {
        for px in min_x..max_x {
            let dx = px as f64 + 0.5 - e;
            let dy = py as f64 + 0.5 - f;
            let u = (d * dx - c * dy) / det;
            let v = (-b * dx + a * dy) / det;
            if !(0.0..1.0).contains(&u) || !(0.0..1.0).contains(&v) {
                continue;
            }
            let col = ((u * w as f64) as usize).min(w - 1);
            // Image row 0 is the top of the image, which is v = 1.
            let row = (((1.0 - v) * h as f64) as usize).min(h - 1);
            let idx = (row * w + col) * 4;
            // A stencil mask paints the current fill colour through its unmasked
            // pixels; an ordinary image paints its own RGB texel.
            let color = if image.stencil {
                fill
            } else {
                [image.rgba[idx], image.rgba[idx + 1], image.rgba[idx + 2]]
            };
            let mut alpha = image.rgba[idx + 3] as f64 / 255.0 * global_alpha;
            if let Some(cmask) = clip {
                alpha *= cmask.at(px, py);
            }
            canvas.blend_mode(px, py, color, alpha, blend);
        }
    }
}

#[derive(Clone)]
struct GState {
    ctm: Matrix,
    fill: [u8; 3],
    stroke: [u8; 3],
    line_width: f64,
    /// Active clip mask (per-pixel coverage). `None` = unclipped.
    clip: Option<ClipMask>,
    /// Current separable blend mode (from an ExtGState `/BM`).
    blend: BlendMode,
    /// Constant non-stroking alpha (`/ca`), `1.0` = opaque.
    fill_alpha: f64,
    /// Active soft mask as a device-resolution alpha buffer (`/SMask`).
    soft_mask: Option<ClipMask>,
    /// Name of the fill colour's shading pattern, if the fill colour space is
    /// `/Pattern` and `scn` named a pattern (axial/radial or tiling).
    fill_pattern: Option<Vec<u8>>,
    /// Name of the current *non-stroking* colour space set by `cs`, used to
    /// resolve `sc`/`scn` operands through [`ResourceCtx::resolve_color`] (named
    /// Separation/DeviceN/Indexed/ICCBased/Lab spaces). `None` = device space
    /// implied by `g`/`rg`/`k` or by `sc`/`scn` arity.
    fill_cs: Option<Vec<u8>>,
    /// Name of the current *stroking* colour space set by `CS` (for `SC`/`SCN`).
    stroke_cs: Option<Vec<u8>>,
}

impl GState {
    fn new(ctm: Matrix) -> Self {
        GState {
            ctm,
            fill: [0, 0, 0],
            stroke: [0, 0, 0],
            line_width: 1.0,
            clip: None,
            blend: BlendMode::Normal,
            fill_alpha: 1.0,
            soft_mask: None,
            fill_pattern: None,
            fill_cs: None,
            stroke_cs: None,
        }
    }

    /// The combined per-pixel clip for painting: the active clip ∩ soft mask
    /// (`None` when neither is set, so callers can skip the multiply fast-path).
    fn paint_clip(&self) -> Option<ClipMask> {
        match (&self.clip, &self.soft_mask) {
            (Some(c), Some(m)) => Some(c.intersect(m)),
            (Some(c), None) => Some(c.clone()),
            (None, Some(m)) => Some(m.clone()),
            (None, None) => None,
        }
    }
}

fn nums(op: &Operation) -> Vec<f64> {
    op.operands.iter().filter_map(Object::as_f64).collect()
}

fn cmyk_to_rgb(c: f64, m: f64, y: f64, k: f64) -> [u8; 3] {
    let r = (1.0 - c) * (1.0 - k);
    let g = (1.0 - m) * (1.0 - k);
    let b = (1.0 - y) * (1.0 - k);
    [
        (r * 255.0).round() as u8,
        (g * 255.0).round() as u8,
        (b * 255.0).round() as u8,
    ]
}

fn gray(v: f64) -> [u8; 3] {
    let g = (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    [g, g, g]
}

/// Resolve an `sc`/`scn`/`SC`/`SCN` colour. When a named colour space is active
/// (`cs_name`), resolve through the document (`ResourceCtx`) so Separation,
/// DeviceN, Indexed, ICCBased and Lab spaces convert correctly. Otherwise — or
/// when the name can't be resolved — fall back to the Device space implied by
/// the operand count (1=Gray, 3=RGB, 4=CMYK). `None` only when neither path
/// yields a colour (caller keeps the previous fill/stroke).
fn resolve_set_color(
    ctx: &dyn ResourceCtx,
    cs_name: Option<&[u8]>,
    comps: &[f64],
) -> Option<[u8; 3]> {
    if let Some(name) = cs_name {
        if let Some(rgb) = ctx.resolve_color(name, comps) {
            return Some(rgb);
        }
    }
    match comps.len() {
        1 => Some(gray(comps[0])),
        3 => Some(rgb(comps[0], comps[1], comps[2])),
        4 => Some(cmyk_to_rgb(comps[0], comps[1], comps[2], comps[3])),
        _ => None,
    }
}

fn rgb(r: f64, g: f64, b: f64) -> [u8; 3] {
    [
        (r.clamp(0.0, 1.0) * 255.0).round() as u8,
        (g.clamp(0.0, 1.0) * 255.0).round() as u8,
        (b.clamp(0.0, 1.0) * 255.0).round() as u8,
    ]
}

/// Flatten a cubic Bézier into line segments, appending device points.
fn flatten_cubic(
    p0: (f64, f64),
    p1: (f64, f64),
    p2: (f64, f64),
    p3: (f64, f64),
    to_device: &dyn Fn(f64, f64) -> (f64, f64),
    out: &mut Vec<(f64, f64)>,
) {
    const STEPS: usize = 16;
    for i in 1..=STEPS {
        let t = i as f64 / STEPS as f64;
        let mt = 1.0 - t;
        let x = mt * mt * mt * p0.0
            + 3.0 * mt * mt * t * p1.0
            + 3.0 * mt * t * t * p2.0
            + t * t * t * p3.0;
        let y = mt * mt * mt * p0.1
            + 3.0 * mt * mt * t * p1.1
            + 3.0 * mt * t * t * p2.1
            + t * t * t * p3.1;
        out.push(to_device(x, y));
    }
}

fn subpath_edges(subpaths: &[Vec<(f64, f64)>]) -> Vec<Edge> {
    let mut edges = Vec::new();
    for sub in subpaths {
        if sub.len() < 2 {
            continue;
        }
        for i in 0..sub.len() {
            let (x0, y0) = sub[i];
            let (x1, y1) = sub[(i + 1) % sub.len()]; // implicit close for fill
            edges.push(Edge { x0, y0, x1, y1 });
        }
    }
    edges
}

/// Build fill edges for a stroked path: a quad per segment (no joins/caps yet).
fn stroke_edges(subpaths: &[Vec<(f64, f64)>], width: f64) -> Vec<Edge> {
    let half = (width / 2.0).max(0.35);
    let mut edges = Vec::new();
    for sub in subpaths {
        for seg in sub.windows(2) {
            let (x0, y0) = seg[0];
            let (x1, y1) = seg[1];
            let (dx, dy) = (x1 - x0, y1 - y0);
            let len = (dx * dx + dy * dy).sqrt();
            if len < 1e-6 {
                continue;
            }
            let (nx, ny) = (-dy / len * half, dx / len * half);
            let quad = [
                (x0 + nx, y0 + ny),
                (x1 + nx, y1 + ny),
                (x1 - nx, y1 - ny),
                (x0 - nx, y0 - ny),
            ];
            for i in 0..4 {
                edges.push(Edge {
                    x0: quad[i].0,
                    y0: quad[i].1,
                    x1: quad[(i + 1) % 4].0,
                    y1: quad[(i + 1) % 4].1,
                });
            }
        }
    }
    edges
}

/// Rasterize a decoded content stream onto a fresh canvas of `width × height`
/// device pixels. `base` maps page user space (origin bottom-left, y up) to the
/// device (origin top-left, y down). `fonts` provides glyph outlines per font
/// resource name; an empty map renders vector graphics only.
pub fn render_content(
    content: &[u8],
    width: u32,
    height: u32,
    base: Matrix,
    fonts: &RenderFonts,
    images: &RenderImages,
) -> Canvas {
    let mut canvas = Canvas::new(width, height);
    render_content_into(&mut canvas, content, base, fonts, images, 1.0);
    canvas
}

/// Rasterize a decoded content stream onto an **existing** canvas, painting over
/// whatever is already there. `base` maps the stream's user space to the device;
/// `global_alpha` (`0.0..=1.0`) scales every paint operation's coverage/alpha so
/// a caller can honour a constant opacity (an annotation appearance's `/CA`).
///
/// This is the simple entry point: it resolves no nested resources, so form
/// XObjects (`Do`), shadings (`sh`/pattern fills) and ExtGState soft masks are
/// not drawn. Use [`render_content_into_ctx`] with a document-backed
/// [`ResourceCtx`] for full fidelity.
pub fn render_content_into(
    canvas: &mut Canvas,
    content: &[u8],
    base: Matrix,
    fonts: &RenderFonts,
    images: &RenderImages,
    global_alpha: f64,
) {
    render_content_into_ctx(
        canvas,
        content,
        base,
        fonts,
        images,
        global_alpha,
        &NoResources,
        0,
        None,
        false,
        &[],
    );
}

/// The full content-stream rasterizer: like [`render_content_into`] but resolves
/// nested resources through `ctx` — form XObjects (`Do`), axial/radial shadings
/// (`sh` and shading-pattern fills), and ExtGState blend modes + soft masks
/// (`gs`). `depth` is the current form-nesting depth (cycle/recursion guard);
/// `init_clip` seeds the graphics-state clip (a parent form's `/BBox` + active
/// clip when recursing).
///
/// When `skip_text` is true, the text-showing operators (`Tj`, `'`, `"`, `TJ`)
/// paint nothing: all text-state bookkeeping (the text matrix advances driven by
/// `'`/`"`/`Td`/`Tm`/…) still runs, but the glyph fills are suppressed. Vector,
/// shading, image and pattern painting are unaffected. The flag is threaded into
/// nested form XObjects and tiling patterns so the whole content tree honours it.
///
/// `excluded` is an optional per-operation mask (indexed by this stream's parsed
/// op position): when `excluded[i]` is true, the **painting** of op `i` is
/// suppressed — fills/strokes/shadings/images/text — while all state bookkeeping
/// still runs (so positions of following content are unchanged). It applies to
/// the **top-level** stream only (nested forms/patterns are passed an empty mask,
/// since unified element op ranges are top-level), and generalises `skip_text` to
/// arbitrary element op ranges. An empty slice means "exclude nothing".
#[allow(clippy::too_many_arguments)]
pub fn render_content_into_ctx(
    canvas: &mut Canvas,
    content: &[u8],
    base: Matrix,
    fonts: &RenderFonts,
    images: &RenderImages,
    global_alpha: f64,
    ctx: &dyn ResourceCtx,
    depth: usize,
    init_clip: Option<&ClipMask>,
    skip_text: bool,
    excluded: &[bool],
) {
    let global_alpha = global_alpha.clamp(0.0, 1.0);
    if global_alpha <= 0.0 {
        return;
    }
    let operations = match parse_content(content) {
        Ok(ops) => ops,
        Err(_) => return,
    };

    let mut state = GState::new(Matrix::IDENTITY);
    state.clip = init_clip.cloned();
    let mut stack: Vec<GState> = Vec::new();

    // Path being constructed, in device space, split into sub-paths.
    let mut subpaths: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut cur_user = (0.0, 0.0);
    // Pending clip armed by `W`/`W*`, applied at the next path-painting operator.
    let mut pending_clip: Option<bool> = None; // Some(even_odd)

    // Text state.
    let mut tm = Matrix::IDENTITY;
    let mut tlm = Matrix::IDENTITY;
    let mut font: Option<&RenderFont> = None;
    let mut font_size = 0.0f64;
    let mut leading = 0.0f64;
    let mut char_spacing = 0.0f64;
    let mut word_spacing = 0.0f64;
    let mut h_scale = 1.0f64;

    // Optional-content (layer) visibility stack (ISO 32000-1 §8.11.3.2). Each
    // open marked-content sequence (`BDC`/`BMC`) pushes whether it is *visible*:
    // a `/OC` sequence whose group is OFF pushes `false`, every other marked
    // sequence pushes `true`. `oc_hidden` counts the currently-open hidden
    // frames, so painting is suppressed whenever **any** enclosing `/OC` is off
    // (nested layers: an inner ON inside an outer OFF stays hidden).
    let mut oc_stack: Vec<bool> = Vec::new();
    let mut oc_hidden: usize = 0usize;

    for (op_index, op) in operations.iter().enumerate() {
        let n = nums(op);
        // When this op falls inside an excluded element's range, suppress its
        // painting (like `skip_text`, but for arbitrary op ranges) while still
        // running all graphics/text-state bookkeeping below. An enclosing hidden
        // optional-content layer suppresses painting just the same.
        let op_excluded = excluded.get(op_index).copied().unwrap_or(false);
        let skip_paint = op_excluded || oc_hidden > 0;
        match op.operator.as_slice() {
            b"q" => stack.push(state.clone()),
            b"Q" => {
                if let Some(s) = stack.pop() {
                    state = s;
                }
            }
            b"cm" if n.len() == 6 => {
                state.ctm = Matrix::new(n[0], n[1], n[2], n[3], n[4], n[5]).then(&state.ctm);
            }
            b"w" if !n.is_empty() => state.line_width = n[0],
            // The device-colour operators also reset the current colour space to
            // their device space (ISO 32000-1 §8.6.8), clearing any named `cs`.
            b"rg" if n.len() == 3 => {
                state.fill = rgb(n[0], n[1], n[2]);
                state.fill_pattern = None;
                state.fill_cs = None;
            }
            b"RG" if n.len() == 3 => {
                state.stroke = rgb(n[0], n[1], n[2]);
                state.stroke_cs = None;
            }
            b"g" if !n.is_empty() => {
                state.fill = gray(n[0]);
                state.fill_pattern = None;
                state.fill_cs = None;
            }
            b"G" if !n.is_empty() => {
                state.stroke = gray(n[0]);
                state.stroke_cs = None;
            }
            b"k" if n.len() == 4 => {
                state.fill = cmyk_to_rgb(n[0], n[1], n[2], n[3]);
                state.fill_pattern = None;
                state.fill_cs = None;
            }
            b"K" if n.len() == 4 => {
                state.stroke = cmyk_to_rgb(n[0], n[1], n[2], n[3]);
                state.stroke_cs = None;
            }
            // Non-stroking colour space: remember the name so `sc`/`scn` can
            // resolve it. `/Pattern` flips the fill into pattern mode; any other
            // space resets the colour to its initial value (black for the device
            // spaces; the spec's "initial value" otherwise).
            b"cs" => {
                state.fill_pattern = None;
                let name = op.operands.first().and_then(Object::as_name);
                if name == Some(b"Pattern".as_slice()) {
                    state.fill_cs = None;
                } else {
                    state.fill_cs = name.map(|n| n.to_vec());
                    state.fill = [0, 0, 0];
                }
            }
            // Stroking colour space.
            b"CS" => {
                let name = op.operands.first().and_then(Object::as_name);
                if name == Some(b"Pattern".as_slice()) {
                    state.stroke_cs = None;
                } else {
                    state.stroke_cs = name.map(|n| n.to_vec());
                    state.stroke = [0, 0, 0];
                }
            }
            // Set the fill colour from components, or name a pattern (last operand
            // is a `/Name` when the colour space is `/Pattern`).
            b"sc" | b"scn" => {
                if let Some(Object::Name(name)) = op.operands.last() {
                    state.fill_pattern = Some(name.clone());
                } else {
                    state.fill_pattern = None;
                    state.fill =
                        resolve_set_color(ctx, state.fill_cs.as_deref(), &n).unwrap_or(state.fill);
                }
            }
            b"SC" | b"SCN" => {
                if op.operands.last().and_then(Object::as_name).is_none() {
                    state.stroke = resolve_set_color(ctx, state.stroke_cs.as_deref(), &n)
                        .unwrap_or(state.stroke);
                }
            }
            b"gs" => {
                if let Some(Object::Name(name)) = op.operands.first() {
                    if let Some(params) = ctx.ext_gstate(name) {
                        state.blend = params.blend;
                        if let Some(a) = params.fill_alpha {
                            state.fill_alpha = a.clamp(0.0, 1.0);
                        }
                        // `/SMask /None` clears the mask; a mask replaces it.
                        state.soft_mask = params.soft_mask;
                    }
                }
            }
            b"m" if n.len() == 2 => {
                cur_user = (n[0], n[1]);
                let ctm = state.ctm;
                subpaths.push(vec![device(&ctm, &base, n[0], n[1])]);
            }
            b"l" if n.len() == 2 => {
                cur_user = (n[0], n[1]);
                let ctm = state.ctm;
                if let Some(last) = subpaths.last_mut() {
                    last.push(device(&ctm, &base, n[0], n[1]));
                }
            }
            b"c" | b"v" | b"y" => {
                let ctm = state.ctm;
                let to_dev = |x: f64, y: f64| device(&ctm, &base, x, y);
                let (p1, p2, p3) = bezier_control_points(op.operator.as_slice(), &n, cur_user);
                if let (Some(p1), Some(p2), Some(p3)) = (p1, p2, p3) {
                    if let Some(last) = subpaths.last_mut() {
                        flatten_cubic(cur_user, p1, p2, p3, &to_dev, last);
                    }
                    cur_user = p3;
                }
            }
            b"re" if n.len() == 4 => {
                let (x, y, w, h) = (n[0], n[1], n[2], n[3]);
                let ctm = state.ctm;
                subpaths.push(vec![
                    device(&ctm, &base, x, y),
                    device(&ctm, &base, x + w, y),
                    device(&ctm, &base, x + w, y + h),
                    device(&ctm, &base, x, y + h),
                ]);
                cur_user = (x, y);
            }
            // `W`/`W*` arm a clip from the current path; it takes effect at the
            // next path-painting op (which may be `n`, a no-op paint).
            b"W" => pending_clip = Some(false),
            b"W*" => pending_clip = Some(true),
            b"f" | b"F" | b"f*" | b"b" | b"b*" | b"B" | b"B*" => {
                let even_odd = op.operator.ends_with(b"*");
                let clip = state.paint_clip();
                // Excluded element: skip the actual fill/stroke paint, but still
                // commit the clip and clear the path so the stream stays correct.
                if !skip_paint {
                    if let Some(pat) = state.fill_pattern.clone() {
                        paint_pattern_fill(
                            canvas,
                            &subpaths,
                            even_odd,
                            &pat,
                            &state,
                            &base,
                            global_alpha,
                            ctx,
                            depth,
                            skip_text,
                        );
                    } else {
                        canvas.fill_ext(
                            &subpath_edges(&subpaths),
                            state.fill,
                            even_odd,
                            global_alpha * state.fill_alpha,
                            clip.as_ref(),
                            state.blend,
                        );
                    }
                    if matches!(op.operator.as_slice(), b"b" | b"b*" | b"B" | b"B*") {
                        let lw = device_scale(&state.ctm, &base) * state.line_width;
                        canvas.fill_ext(
                            &stroke_edges(&subpaths, lw),
                            state.stroke,
                            false,
                            global_alpha,
                            clip.as_ref(),
                            state.blend,
                        );
                    }
                }
                commit_pending_clip(&mut state, &subpaths, &mut pending_clip, canvas);
                subpaths.clear();
            }
            b"S" | b"s" => {
                let lw = device_scale(&state.ctm, &base) * state.line_width;
                let clip = state.paint_clip();
                if !skip_paint {
                    canvas.fill_ext(
                        &stroke_edges(&subpaths, lw),
                        state.stroke,
                        false,
                        global_alpha,
                        clip.as_ref(),
                        state.blend,
                    );
                }
                commit_pending_clip(&mut state, &subpaths, &mut pending_clip, canvas);
                subpaths.clear();
            }
            b"n" => {
                commit_pending_clip(&mut state, &subpaths, &mut pending_clip, canvas);
                subpaths.clear();
            }

            // Paint a shading directly across the current clip region.
            b"sh" if !skip_paint => {
                if let Some(Object::Name(name)) = op.operands.first() {
                    if let Some(mut shading) = ctx.shading(name) {
                        // `sh` paints in the current user space → device.
                        shading.to_device = state.ctm.then(&base);
                        let clip = state.paint_clip();
                        paint_shading(canvas, &shading, clip.as_ref(), global_alpha, state.blend);
                    }
                }
            }

            // A `Do` whose XObject carries an `/OC` that is OFF (ISO 32000-1
            // §8.11.3.3) paints nothing, exactly like an enclosing hidden `/OC`
            // marked sequence — applies to both image and form XObjects.
            b"Do"
                if !skip_paint
                    && op
                        .operands
                        .first()
                        .and_then(Object::as_name)
                        .is_none_or(|name| ctx.oc_xobject_visible(name)) =>
            {
                if let Some(Object::Name(name)) = op.operands.first() {
                    if let Some(image) = images.get(name) {
                        let clip = state.paint_clip();
                        blit_image_clipped(
                            canvas,
                            image,
                            &state.ctm,
                            &base,
                            global_alpha * state.fill_alpha,
                            clip.as_ref(),
                            state.blend,
                            state.fill,
                        );
                    } else if depth < crate::content::MAX_FORM_DEPTH {
                        if let Some(form) = ctx.form_xobject(name) {
                            draw_form(canvas, &form, &state, &base, global_alpha, depth, skip_text);
                        }
                    }
                }
            }

            // Inline image (`BI`…`ID`<data>`EI`, §8.9.7): the content parser stored
            // the captured body as this op's single string operand. Decode it
            // through the same pipeline as `/Image` XObjects and blit it over the
            // unit square, exactly like a `Do` image. An `/IM true` stencil paints
            // the current fill colour through its mask.
            b"BI" if !skip_paint => {
                if let Some(Object::String(raw, _)) = op.operands.first() {
                    if let Some(image) = ctx.inline_image(raw) {
                        let clip = state.paint_clip();
                        blit_image_clipped(
                            canvas,
                            &image,
                            &state.ctm,
                            &base,
                            global_alpha * state.fill_alpha,
                            clip.as_ref(),
                            state.blend,
                            state.fill,
                        );
                    }
                }
            }

            // ── text ──
            b"BT" => {
                tm = Matrix::IDENTITY;
                tlm = Matrix::IDENTITY;
            }
            b"Tf" => {
                if let Some(Object::Name(name)) = op.operands.first() {
                    font = fonts.get(name);
                }
                if let Some(&sz) = n.last() {
                    font_size = sz;
                }
            }
            b"Td" if n.len() == 2 => {
                tlm = Matrix::translate(n[0], n[1]).then(&tlm);
                tm = tlm;
            }
            b"TD" if n.len() == 2 => {
                leading = -n[1];
                tlm = Matrix::translate(n[0], n[1]).then(&tlm);
                tm = tlm;
            }
            b"Tm" if n.len() == 6 => {
                tlm = Matrix::new(n[0], n[1], n[2], n[3], n[4], n[5]);
                tm = tlm;
            }
            b"T*" => {
                tlm = Matrix::translate(0.0, -leading).then(&tlm);
                tm = tlm;
            }
            b"TL" if !n.is_empty() => leading = n[0],
            b"Tc" if !n.is_empty() => char_spacing = n[0],
            b"Tw" if !n.is_empty() => word_spacing = n[0],
            b"Tz" if !n.is_empty() => h_scale = n[0] / 100.0,
            b"Tj" | b"'" | b"\"" => {
                if op.operator.as_slice() != b"Tj" {
                    // ' and " move to the next line first; " also sets spacing.
                    if op.operator.as_slice() == b"\"" && op.operands.len() >= 3 {
                        word_spacing = op.operands[0].as_f64().unwrap_or(word_spacing);
                        char_spacing = op.operands[1].as_f64().unwrap_or(char_spacing);
                    }
                    tlm = Matrix::translate(0.0, -leading).then(&tlm);
                    tm = tlm;
                }
                if let (Some(f), Some(Object::String(bytes, _))) = (font, op.operands.last()) {
                    if !skip_text && !skip_paint {
                        show_text(
                            canvas,
                            f,
                            font_size,
                            &mut tm,
                            &state.ctm,
                            &base,
                            state.fill,
                            char_spacing,
                            word_spacing,
                            h_scale,
                            global_alpha * state.fill_alpha,
                            state.paint_clip().as_ref(),
                            state.blend,
                            bytes,
                            ctx,
                            depth,
                        );
                    }
                }
            }
            b"TJ" => {
                if let (Some(f), Some(Object::Array(items))) = (font, op.operands.first()) {
                    let clip = state.paint_clip();
                    for item in items {
                        if let Object::String(bytes, _) = item {
                            if !skip_text && !skip_paint {
                                show_text(
                                    canvas,
                                    f,
                                    font_size,
                                    &mut tm,
                                    &state.ctm,
                                    &base,
                                    state.fill,
                                    char_spacing,
                                    word_spacing,
                                    h_scale,
                                    global_alpha * state.fill_alpha,
                                    clip.as_ref(),
                                    state.blend,
                                    bytes,
                                    ctx,
                                    depth,
                                );
                            }
                        } else if let Some(adj) = item.as_f64() {
                            let dx = -adj / 1000.0 * font_size * h_scale;
                            tm = Matrix::translate(dx, 0.0).then(&tm);
                        }
                    }
                }
            }

            // ── marked content (optional-content visibility, §8.11.3.2) ──
            // `BDC`/`BMC` open a sequence; `EMC` closes the innermost one. A
            // `/OC <prop> BDC` gates the enclosed operators on the referenced
            // group's visibility — push `false` (hidden) when the group is OFF.
            // Every other marked sequence (`/Span … BDC`, `BMC`) pushes `true`,
            // so the stack stays balanced and a hidden ancestor keeps its
            // descendants hidden regardless of their own tag.
            b"BDC" | b"BMC" => {
                let visible = if op.operator.as_slice() == b"BDC"
                    && op.operands.first().and_then(Object::as_name) == Some(b"OC".as_slice())
                {
                    op.operands
                        .get(1)
                        .is_none_or(|prop| ctx.oc_marked_visible(prop))
                } else {
                    true
                };
                oc_stack.push(visible);
                if !visible {
                    oc_hidden += 1;
                }
            }
            b"EMC" => {
                if let Some(visible) = oc_stack.pop() {
                    if !visible {
                        oc_hidden = oc_hidden.saturating_sub(1);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Commit a pending `W`/`W*` clip (armed earlier) using the current path, at a
/// path-painting operator. The new clip intersects the existing one (PDF clips
/// only shrink). Clears `pending`.
fn commit_pending_clip(
    state: &mut GState,
    subpaths: &[Vec<(f64, f64)>],
    pending: &mut Option<bool>,
    canvas: &Canvas,
) {
    let Some(even_odd) = pending.take() else {
        return;
    };
    let edges = subpath_edges(subpaths);
    if edges.is_empty() {
        return;
    }
    let mask = ClipMask::from_edges(canvas.width, canvas.height, &edges, even_odd);
    state.clip = Some(match &state.clip {
        Some(existing) => existing.intersect(&mask),
        None => mask,
    });
}

/// Draw a form XObject: compose its `/Matrix` onto the invoking CTM, clip to its
/// `/BBox`, and recurse through the content renderer with the form's own
/// resources and child context. The composed `/BBox` ∩ active clip is seeded as
/// the child's initial clip so the form's marks stay inside its box. `depth+1`
/// bounds nesting (cycle guard).
#[allow(clippy::too_many_arguments)]
fn draw_form(
    canvas: &mut Canvas,
    form: &FormXObject,
    state: &GState,
    base: &Matrix,
    global_alpha: f64,
    depth: usize,
    skip_text: bool,
) {
    // Form space → invoking user space → device base. The form draws in this
    // composed coordinate system, so we fold it into a new base for the recursion.
    let form_ctm = form.matrix.then(&state.ctm);
    let form_base = form_ctm.then(base);

    // Clip to the form's /BBox (its corners mapped through the form CTM), then
    // intersect with the currently active clip + soft mask.
    let [x0, y0, x1, y1] = form.bbox;
    let corners =
        [(x0, y0), (x1, y0), (x1, y1), (x0, y1)].map(|(x, y)| device(&form_ctm, base, x, y));
    let bbox_edges: Vec<Edge> = (0..4)
        .map(|i| {
            let (ax, ay) = corners[i];
            let (bx, by) = corners[(i + 1) % 4];
            Edge {
                x0: ax,
                y0: ay,
                x1: bx,
                y1: by,
            }
        })
        .collect();
    let bbox_mask = ClipMask::from_edges(canvas.width, canvas.height, &bbox_edges, false);
    let clip = match state.paint_clip() {
        Some(active) => active.intersect(&bbox_mask),
        None => bbox_mask,
    };

    render_content_into_ctx(
        canvas,
        &form.content,
        form_base,
        &form.fonts,
        &form.images,
        global_alpha * state.fill_alpha,
        form.ctx.as_ref(),
        depth + 1,
        Some(&clip),
        skip_text,
        // Exclusion masks address top-level element op ranges only; nested form
        // content carries no such index, so it always paints.
        &[],
    );
}

/// Render a text-show string: for each character code, look up its glyph and
/// fill the outline, advancing the text matrix by the glyph's width.
/// `global_alpha` scales the fill coverage (annotation `/CA` opacity); `clip`
/// modulates per pixel (active `W` clip ∩ soft mask) and `blend` selects the mode.
/// `ctx`/`depth` are forwarded to the content-stream rasterizer for `/Type3`
/// glyph procedures (whose marks may invoke nested form/shading resources).
#[allow(clippy::too_many_arguments)]
fn show_text(
    canvas: &mut Canvas,
    font: &RenderFont,
    size: f64,
    tm: &mut Matrix,
    ctm: &Matrix,
    base: &Matrix,
    fill: [u8; 3],
    char_spacing: f64,
    word_spacing: f64,
    h_scale: f64,
    global_alpha: f64,
    clip: Option<&ClipMask>,
    blend: BlendMode,
    bytes: &[u8],
    ctx: &dyn ResourceCtx,
    depth: usize,
) {
    // `/Type3` fonts draw each glyph as a content stream, not an outline — split
    // off to the dedicated path that executes the `/CharProcs` procedure under
    // `FontMatrix · Tm · CTM · base`.
    if let Some(type3) = &font.type3 {
        show_text_type3(
            canvas,
            font,
            type3,
            size,
            tm,
            ctm,
            base,
            fill,
            char_spacing,
            word_spacing,
            h_scale,
            global_alpha,
            clip,
            blend,
            bytes,
            ctx,
            depth,
        );
        return;
    }
    // Vertical writing mode (ISO 32000-1 §9.4.4): a composite font whose
    // `/Encoding` CMap is `*-V`/`Identity-V`/`/WMode 1`. Each glyph is offset from
    // the pen by its `/W2`/`/DW2` position vector (so it centres on the vertical
    // baseline) and the pen steps top-to-bottom by the vertical displacement `w1y`
    // (≤ 0) rather than rightward by `w0`. Horizontal fonts keep the unchanged path.
    let vertical = font.decoder.wmode() == 1;
    let mut i = 0;
    while i < bytes.len() {
        let (code, consumed): (u32, usize) = if font.two_byte && i + 1 < bytes.len() {
            (((bytes[i] as u32) << 8) | bytes[i + 1] as u32, 2)
        } else {
            (bytes[i] as u32, 1)
        };
        let code_bytes = &bytes[i..i + consumed];
        i += consumed;

        // Composite-font CID for this code: map code → CID through the `/Encoding`
        // CMap when it isn't `Identity-H` (a predefined CJK or embedded CMap), else
        // the code *is* the CID. This CID keys both the `/W` width table and the
        // `/CIDToGIDMap` glyph lookup below. (Unused for simple fonts.)
        let cid = match &font.code_to_cid {
            Some(cmap) => cmap.cid(code as u16).unwrap_or(code as u16) as u32,
            None => code,
        };

        // Vertical mode: place the glyph so its **vertical** origin (origin 1) sits
        // at the pen — shift the outline by `-v` (the position vector from origin 0
        // to origin 1) — and step the pen by `w1y` afterwards. `(w1y, vx, vy)` come
        // back in user-space points (already font-size scaled). Both offsets are 0
        // for a horizontal font, leaving the path below unchanged.
        let (v_w1y, off_x, off_y) = if vertical {
            let (w1y, vx, vy) = font.decoder.vertical_metric(code as u16, size);
            (w1y, -vx * h_scale, -vy)
        } else {
            (0.0, 0.0, 0.0)
        };

        // Authoritative pen advance from the PDF width table (`/Widths` keyed by
        // code for simple fonts, `/W`+`/DW` keyed by **CID** for composite ones).
        // ISO 32000-1 §9.2.4 makes these widths the displacement of record; the
        // embedded program's own metric is only a fallback, and a subset CFF that
        // omits the charstring width operand can otherwise report 0 and collapse
        // whole words. `None` → no width table → use the program's `advance_width`
        // (filled below), else the 0.5-em guess.
        let width_key = if font.two_byte { cid } else { code };
        let dict_advance = font
            .decoder
            .widths
            .as_ref()
            .map(|w| w.advance(width_key) * size / 1000.0);

        let mut advance = dict_advance.unwrap_or(size * 0.5); // fallback width
        if let Some(ttf) = &font.program {
            let upem = ttf.units_per_em();
            let gid = if font.two_byte {
                // Composite font: map CID → glyph id through a non-identity
                // `/CIDToGIDMap` when present, else the CID is the glyph id
                // directly (the Identity case).
                match &font.cid_to_gid {
                    Some(map) => map.get(cid as usize).copied().unwrap_or(0),
                    None => cid as u16,
                }
            } else if let Some(gid) = font
                .code_to_gid
                .as_ref()
                .and_then(|m| m.get(&code).copied())
            {
                // Simple font: PDF `/Encoding` → charset glyph id (handles subset
                // CFF, whose program carries no Unicode cmap).
                gid
            } else {
                let decoded = font.decoder.decode(code_bytes);
                let scalar = decoded.chars().next().map(|c| c as u32).unwrap_or(code);
                ttf.gid_for_unicode(scalar).unwrap_or(0)
            };
            let s = size / upem;
            // Accumulate ALL contours of the glyph into one edge set and fill
            // ONCE. Filling each contour separately painted glyph counters (the
            // holes in O, e, a, 0, 8, B…) solid, because an isolated inner
            // contour has no outer contour to subtract from. TrueType/CFF
            // outlines wind their outer and inner contours in opposite
            // directions, so a single non-zero fill carves the counters out.
            //
            // Skip glyph id 0 (`.notdef`): an unresolved code must render as
            // *nothing*, never the notdef box — drawing it would litter the page
            // with tofu wherever a code didn't map (e.g. a glyph absent from a
            // subset). The cursor still advances by the notdef width below.
            let mut edges = Vec::new();
            let polygons = if gid == 0 {
                Vec::new()
            } else {
                ttf.glyph_polygons(gid)
            };
            for poly in polygons {
                if poly.len() < 3 {
                    continue;
                }
                let dev: Vec<(f64, f64)> = poly
                    .iter()
                    .map(|&(gx, gy)| {
                        // `off_x`/`off_y` are 0 horizontally; in vertical mode they
                        // shift the glyph by `-v` so origin 1 lands at the pen.
                        let (ux, uy) = tm.apply(gx * s * h_scale + off_x, gy * s + off_y);
                        device(ctm, base, ux, uy)
                    })
                    .collect();
                for k in 0..dev.len() {
                    let (x0, y0) = dev[k];
                    let (x1, y1) = dev[(k + 1) % dev.len()];
                    edges.push(Edge { x0, y0, x1, y1 });
                }
            }
            if !edges.is_empty() {
                canvas.fill_ext(&edges, fill, false, global_alpha, clip, blend);
            }
            // The PDF width table wins when present (already in `advance`); only
            // fall back to the program's own glyph metric when the font carries
            // no `/Widths`/`/W`.
            if dict_advance.is_none() {
                advance = ttf.advance_width(gid) / upem * size;
            }
        }

        if vertical {
            // Vertical pen step: the glyph's vertical displacement `w1y` (≤ 0)
            // plus `Tc`/`Tw` applied along the text-space y-axis. `Tw` only acts on
            // the single-byte code 32 (never produced by a 2-byte composite font,
            // so it is inert here, matching the spec).
            let mut step = v_w1y - char_spacing;
            if consumed == 1 && code == 32 {
                step -= word_spacing;
            }
            *tm = Matrix::translate(0.0, step).then(tm);
        } else {
            let mut step = advance + char_spacing;
            if consumed == 1 && code == 32 {
                step += word_spacing;
            }
            *tm = Matrix::translate(step * h_scale, 0.0).then(tm);
        }
    }
}

/// Draw a `/Type3` text-show string (ISO 32000-1 §9.6.5). Each character code is
/// a glyph whose description is a PDF content stream in `/CharProcs`, drawn in
/// **glyph space** mapped to text space by the font's `/FontMatrix`. For each
/// code we compose the glyph render matrix
/// `FontMatrix · scale(fontSize·h_scale, fontSize) · Tm · CTM` and run the proc
/// through the same content-stream rasterizer used for page content and form
/// XObjects (so its fills, strokes, colours, images and even nested text all
/// honour the engine's full machinery). `d0`/`d1` glyph-metric operators set the
/// width/bbox and are no-ops for painting (the interpreter simply ignores the
/// unknown operators). The pen advances by the `/Widths` value transformed
/// through the `/FontMatrix` into text space — never the fixed 1000-unit divisor
/// the outline path uses, because a Type3 `/FontMatrix` is arbitrary.
#[allow(clippy::too_many_arguments)]
fn show_text_type3(
    canvas: &mut Canvas,
    font: &RenderFont,
    type3: &Type3Glyphs,
    size: f64,
    tm: &mut Matrix,
    ctm: &Matrix,
    base: &Matrix,
    _fill: [u8; 3],
    char_spacing: f64,
    word_spacing: f64,
    h_scale: f64,
    global_alpha: f64,
    clip: Option<&ClipMask>,
    blend: BlendMode,
    bytes: &[u8],
    ctx: &dyn ResourceCtx,
    depth: usize,
) {
    let _ = blend; // glyph procs carry their own blend state; nothing to seed.
    let fm = Matrix(type3.font_matrix);
    // The width table for a Type3 font holds raw glyph-space advances; convert to
    // text space via the FontMatrix linear part (`(w,0)` displacement), then by
    // the font size. Falls back to half-em in text space when no `/Widths`.
    let width_table = font.decoder.widths.as_ref();
    for &byte in bytes {
        let code = byte as u32;

        // Glyph-space advance → text-space x displacement. `FontMatrix·(w,0)`
        // gives the per-glyph vector in text space; we take its magnitude in the
        // text x direction (a) since text advances horizontally.
        let glyph_w = width_table.map(|w| w.advance(code));
        let advance = match glyph_w {
            Some(w) => fm.0[0] * w * size,
            None => size * 0.5,
        };

        if let Some(content) = type3.char_procs.get(&code) {
            // Glyph space → text space (FontMatrix), scaled by the font size and
            // horizontal scaling, then through the text matrix and CTM. The proc
            // is rendered with this composition as its `base` (user→device map),
            // intersecting the active clip so the glyph stays inside any W clip.
            let glyph_to_text = fm.then(&Matrix::new(size * h_scale, 0.0, 0.0, size, 0.0, 0.0));
            let glyph_base = glyph_to_text.then(tm).then(ctm).then(base);
            if depth < crate::content::MAX_FORM_DEPTH {
                render_content_into_ctx(
                    canvas,
                    content,
                    glyph_base,
                    &type3.fonts,
                    &type3.images,
                    global_alpha,
                    ctx,
                    depth + 1,
                    clip,
                    false,
                    &[],
                );
            }
        }

        let mut step = advance + char_spacing;
        if code == 32 {
            step += word_spacing;
        }
        *tm = Matrix::translate(step * h_scale, 0.0).then(tm);
    }
}

/// Invert an affine matrix `[a b c d e f]`. Returns `None` if it's singular.
fn invert(m: &Matrix) -> Option<Matrix> {
    let [a, b, c, d, e, f] = m.0;
    let det = a * d - b * c;
    if det.abs() < 1e-12 {
        return None;
    }
    let inv = 1.0 / det;
    let ia = d * inv;
    let ib = -b * inv;
    let ic = -c * inv;
    let id = a * inv;
    // Translation: -(e,f) mapped through the inverse linear part.
    let ie = -(e * ia + f * ic);
    let if_ = -(e * ib + f * id);
    Some(Matrix::new(ia, ib, ic, id, ie, if_))
}

/// The `[x_min, y_min, x_max, y_max]` device pixel bounds where a clip admits
/// anything; `None`/empty → the whole canvas. Lets shading/pattern paints scan
/// only the relevant region instead of every pixel.
fn clip_bounds(clip: Option<&ClipMask>, width: u32, height: u32) -> (i32, i32, i32, i32) {
    match clip {
        Some(c) => {
            let (mut x0, mut y0, mut x1, mut y1) = (width as i32, height as i32, 0i32, 0i32);
            for y in 0..height as i32 {
                for x in 0..width as i32 {
                    if c.at(x, y) > 0.0 {
                        x0 = x0.min(x);
                        y0 = y0.min(y);
                        x1 = x1.max(x + 1);
                        y1 = y1.max(y + 1);
                    }
                }
            }
            if x1 <= x0 || y1 <= y0 {
                (0, 0, 0, 0)
            } else {
                (x0, y0, x1, y1)
            }
        }
        None => (0, 0, width as i32, height as i32),
    }
}

/// Evaluate the gradient parameter `t ∈ [0,1]` (or out of range) for a point in
/// shading space, plus whether it should paint given the `/Extend` flags. Returns
/// `(t_clamped, paint?)`.
fn shading_param(kind: &ShadingKind, x: f64, y: f64, extend: [bool; 2]) -> (f64, bool) {
    match *kind {
        ShadingKind::Axial { x0, y0, x1, y1 } => {
            let dx = x1 - x0;
            let dy = y1 - y0;
            let len2 = dx * dx + dy * dy;
            if len2 < 1e-12 {
                return (0.0, false);
            }
            let t = ((x - x0) * dx + (y - y0) * dy) / len2;
            extended(t, extend)
        }
        ShadingKind::Radial {
            x0,
            y0,
            r0,
            x1,
            y1,
            r1,
        } => {
            // Find the largest s ∈ [0,1] (with Extend) such that the point lies on
            // circle C(s): centre lerp((x0,y0),(x1,y1),s), radius lerp(r0,r1,s).
            // Solve |P - C(s)| = R(s) → quadratic a s² + b s + c = 0.
            let cdx = x1 - x0;
            let cdy = y1 - y0;
            let dr = r1 - r0;
            let px = x - x0;
            let py = y - y0;
            let a = cdx * cdx + cdy * cdy - dr * dr;
            let b = 2.0 * (px * cdx + py * cdy + r0 * dr);
            let c = px * px + py * py - r0 * r0;
            // Prefer the larger root (paints the circle nearer the end colour on
            // top), but only if its radius is non-negative and it paints under the
            // Extend flags.
            let mut best: Option<f64> = None;
            let mut consider = |s: f64| {
                if r0 + s * dr < 0.0 {
                    return; // negative-radius circle doesn't exist
                }
                if !extended(s, extend).1 {
                    return; // outside [0,1] and not extended on that side
                }
                if best.map(|prev| s > prev).unwrap_or(true) {
                    best = Some(s);
                }
            };
            if a.abs() < 1e-9 {
                if b.abs() > 1e-12 {
                    consider(-c / b);
                }
            } else {
                let disc = b * b - 4.0 * a * c;
                if disc >= 0.0 {
                    let sq = disc.sqrt();
                    consider((-b + sq) / (2.0 * a));
                    consider((-b - sq) / (2.0 * a));
                }
            }
            match best {
                Some(s) => extended(s, extend),
                None => (0.0, false),
            }
        }
        // Mesh shadings are painted triangle-by-triangle (forward-mapped), and
        // function-based shadings sample a 2-D colour grid directly; neither uses
        // this 1-D gradient-parameter inverse map.
        ShadingKind::Mesh { .. } | ShadingKind::Function { .. } => (0.0, false),
    }
}

/// Clamp `t` into `[0,1]` honouring the two `/Extend` flags: a `t<0` paints the
/// start colour only if `extend[0]`, `t>1` paints the end colour only if
/// `extend[1]`. Returns `(t_clamped, paint?)`.
fn extended(t: f64, extend: [bool; 2]) -> (f64, bool) {
    if t < 0.0 {
        (0.0, extend[0])
    } else if t > 1.0 {
        (1.0, extend[1])
    } else {
        (t, true)
    }
}

/// Paint a shading across the clip region: for each device pixel admitted by
/// `clip`, map it back into shading space, evaluate the gradient colour, and
/// blend it through the clip coverage. Used by `sh` (clip = active clip) and by
/// shading-pattern fills (clip = the painted path ∩ active clip).
fn paint_shading(
    canvas: &mut Canvas,
    shading: &Shading,
    clip: Option<&ClipMask>,
    global_alpha: f64,
    blend: BlendMode,
) {
    // Mesh shadings (types 4–7) are painted by forward-mapping each Gouraud
    // triangle to device space and barycentric-interpolating its colours, so they
    // take a separate path from the axial/radial per-pixel inverse map.
    if let ShadingKind::Mesh { triangles } = &shading.kind {
        paint_mesh(canvas, triangles, &shading.to_device, clip, global_alpha, blend);
        return;
    }
    let Some(inv) = invert(&shading.to_device) else {
        return;
    };
    // Function-based shadings (type 1) also inverse-map each pixel into the
    // shading's space, but then index the pre-sampled 2-D `/Domain` colour grid
    // instead of evaluating a 1-D gradient parameter.
    if let ShadingKind::Function {
        domain,
        grid_w,
        grid_h,
        inv_matrix,
    } = &shading.kind
    {
        paint_function(
            canvas,
            &shading.func_grid,
            *domain,
            *grid_w,
            *grid_h,
            &inv,
            inv_matrix,
            clip,
            global_alpha,
            blend,
        );
        return;
    }
    let (x0, y0, x1, y1) = clip_bounds(clip, canvas.width, canvas.height);
    for py in y0..y1 {
        for px in x0..x1 {
            let cov = match clip {
                Some(c) => c.at(px, py),
                None => 1.0,
            };
            if cov <= 0.0 {
                continue;
            }
            let (sx, sy) = inv.apply(px as f64 + 0.5, py as f64 + 0.5);
            let (t, paint) = shading_param(&shading.kind, sx, sy, shading.extend);
            if !paint {
                continue;
            }
            let color = shading.color_at(t);
            canvas.blend_mode(px, py, color, cov * global_alpha, blend);
        }
    }
}

/// Paint a function-based (type 1) shading. For each device pixel admitted by
/// `clip`, map it back into the shading's target space (`dev_inv`), then into the
/// function's domain space (`inv_matrix` = inverse of `/Matrix`). Pixels whose
/// domain point lies inside `domain` are coloured from the pre-sampled `grid`
/// (`grid_w × grid_h`, row-major) and blended; points outside the domain are
/// left untouched. The grid lookup is nearest-cell — a cheap, deterministic
/// stand-in for re-evaluating the `/Function` per pixel.
#[allow(clippy::too_many_arguments)]
fn paint_function(
    canvas: &mut Canvas,
    grid: &[[u8; 3]],
    domain: [f64; 4],
    grid_w: usize,
    grid_h: usize,
    dev_inv: &Matrix,
    inv_matrix: &Matrix,
    clip: Option<&ClipMask>,
    global_alpha: f64,
    blend: BlendMode,
) {
    let [dx0, dx1, dy0, dy1] = domain;
    let span_x = dx1 - dx0;
    let span_y = dy1 - dy0;
    if grid_w == 0 || grid_h == 0 || grid.len() < grid_w * grid_h {
        return;
    }
    let (x0, y0, x1, y1) = clip_bounds(clip, canvas.width, canvas.height);
    for py in y0..y1 {
        for px in x0..x1 {
            let cov = match clip {
                Some(c) => c.at(px, py),
                None => 1.0,
            };
            if cov <= 0.0 {
                continue;
            }
            // Device → shading-target space → domain space.
            let (sx, sy) = dev_inv.apply(px as f64 + 0.5, py as f64 + 0.5);
            let (u, v) = inv_matrix.apply(sx, sy);
            // Reject points outside the (possibly reversed) domain rectangle.
            if u < dx0.min(dx1) || u > dx0.max(dx1) || v < dy0.min(dy1) || v > dy0.max(dy1) {
                continue;
            }
            // Map the domain point to a nearest grid cell.
            let fx = if span_x.abs() < 1e-12 {
                0.0
            } else {
                (u - dx0) / span_x
            };
            let fy = if span_y.abs() < 1e-12 {
                0.0
            } else {
                (v - dy0) / span_y
            };
            let gx =
                ((fx * (grid_w - 1) as f64).round() as i64).clamp(0, grid_w as i64 - 1) as usize;
            let gy =
                ((fy * (grid_h - 1) as f64).round() as i64).clamp(0, grid_h as i64 - 1) as usize;
            let color = grid[gy * grid_w + gx];
            canvas.blend_mode(px, py, color, cov * global_alpha, blend);
        }
    }
}

/// Paint a mesh shading: forward-map every Gouraud triangle from shading space to
/// device pixels and fill it with barycentric-interpolated vertex colours, honouring
/// the clip coverage and global/blend compositing. Triangles are taken three vertices
/// at a time (Coons/tensor patches have already been tessellated by the parser).
fn paint_mesh(
    canvas: &mut Canvas,
    triangles: &[MeshVertex],
    to_device: &Matrix,
    clip: Option<&ClipMask>,
    global_alpha: f64,
    blend: BlendMode,
) {
    for tri in triangles.chunks_exact(3) {
        // Map the three vertices into device space; their colours travel unchanged.
        let p: [(f64, f64); 3] = [
            to_device.apply(tri[0].x, tri[0].y),
            to_device.apply(tri[1].x, tri[1].y),
            to_device.apply(tri[2].x, tri[2].y),
        ];
        fill_gouraud_triangle(canvas, &p, tri, clip, global_alpha, blend);
    }
}

/// Fill one device-space triangle `p[0..3]` (with the matching `verts` colours)
/// using barycentric interpolation. Pixels whose centre lies inside the triangle
/// are blended with the interpolated colour scaled by the clip coverage. A
/// degenerate (zero-area) triangle paints nothing.
fn fill_gouraud_triangle(
    canvas: &mut Canvas,
    p: &[(f64, f64); 3],
    verts: &[MeshVertex],
    clip: Option<&ClipMask>,
    global_alpha: f64,
    blend: BlendMode,
) {
    // Signed area ×2 (the barycentric denominator). Near-zero ⇒ no fill.
    let (ax, ay) = p[0];
    let (bx, by) = p[1];
    let (cx, cy) = p[2];
    let denom = (by - cy) * (ax - cx) + (cx - bx) * (ay - cy);
    if denom.abs() < 1e-9 {
        return;
    }
    let inv_denom = 1.0 / denom;
    // Device-pixel bounding box of the triangle, clamped to the canvas.
    let min_x = ax.min(bx).min(cx).floor().max(0.0) as i32;
    let max_x = ax.max(bx).max(cx).ceil().min(canvas.width as f64) as i32;
    let min_y = ay.min(by).min(cy).floor().max(0.0) as i32;
    let max_y = ay.max(by).max(cy).ceil().min(canvas.height as f64) as i32;
    for py in min_y..max_y {
        for px in min_x..max_x {
            let cov = match clip {
                Some(c) => c.at(px, py),
                None => 1.0,
            };
            if cov <= 0.0 {
                continue;
            }
            let sx = px as f64 + 0.5;
            let sy = py as f64 + 0.5;
            // Barycentric weights of the pixel centre w.r.t. the triangle.
            let w0 = ((by - cy) * (sx - cx) + (cx - bx) * (sy - cy)) * inv_denom;
            let w1 = ((cy - ay) * (sx - cx) + (ax - cx) * (sy - cy)) * inv_denom;
            let w2 = 1.0 - w0 - w1;
            // A small epsilon admits edge/shared-vertex pixels so neighbouring
            // triangles tile without leaving background-coloured seams.
            const EPS: f64 = -1e-6;
            if w0 < EPS || w1 < EPS || w2 < EPS {
                continue;
            }
            let lerp = |i: usize| {
                w0 * verts[0].color[i] as f64
                    + w1 * verts[1].color[i] as f64
                    + w2 * verts[2].color[i] as f64
            };
            let color = [
                lerp(0).round().clamp(0.0, 255.0) as u8,
                lerp(1).round().clamp(0.0, 255.0) as u8,
                lerp(2).round().clamp(0.0, 255.0) as u8,
            ];
            canvas.blend_mode(px, py, color, cov * global_alpha, blend);
        }
    }
}

/// Fill the current path with a `/Pattern` colour: build a coverage mask from the
/// path, intersect with the active clip, and either paint a shading pattern
/// (PatternType 2) across it or stamp a tiling pattern (PatternType 1) form
/// clipped to it. Falls back to nothing if the pattern can't be resolved.
#[allow(clippy::too_many_arguments)]
fn paint_pattern_fill(
    canvas: &mut Canvas,
    subpaths: &[Vec<(f64, f64)>],
    even_odd: bool,
    name: &[u8],
    state: &GState,
    base: &Matrix,
    global_alpha: f64,
    ctx: &dyn ResourceCtx,
    depth: usize,
    skip_text: bool,
) {
    let path_mask = ClipMask::from_edges(
        canvas.width,
        canvas.height,
        &subpath_edges(subpaths),
        even_odd,
    );
    let clip = match state.paint_clip() {
        Some(active) => active.intersect(&path_mask),
        None => path_mask,
    };

    if let Some(mut shading) = ctx.pattern_shading(name) {
        // The pattern's matrix (already in `to_device` relative to the page base)
        // composes with `base`: shading space → page user space → device.
        shading.to_device = shading.to_device.then(base);
        paint_shading(
            canvas,
            &shading,
            Some(&clip),
            global_alpha * state.fill_alpha,
            state.blend,
        );
        return;
    }
    if depth < crate::content::MAX_FORM_DEPTH {
        if let Some(form) = ctx.tiling_pattern(name) {
            // Stamp the tile once: its /Matrix maps pattern space to the page base
            // (not the current CTM). Clip it to the filled path.
            let form_ctm = form.matrix;
            let form_base = form_ctm.then(base);
            render_content_into_ctx(
                canvas,
                &form.content,
                form_base,
                &form.fonts,
                &form.images,
                global_alpha * state.fill_alpha,
                form.ctx.as_ref(),
                depth + 1,
                Some(&clip),
                skip_text,
                &[],
            );
        }
    }
}

fn device(ctm: &Matrix, base: &Matrix, x: f64, y: f64) -> (f64, f64) {
    let (ux, uy) = ctm.apply(x, y);
    base.apply(ux, uy)
}

/// Approximate device-space length of a unit user-space vector, for line width.
fn device_scale(ctm: &Matrix, base: &Matrix) -> f64 {
    let o = device(ctm, base, 0.0, 0.0);
    let p = device(ctm, base, 1.0, 0.0);
    let q = device(ctm, base, 0.0, 1.0);
    let sx = ((p.0 - o.0).powi(2) + (p.1 - o.1).powi(2)).sqrt();
    let sy = ((q.0 - o.0).powi(2) + (q.1 - o.1).powi(2)).sqrt();
    (sx + sy) / 2.0
}

type Pt = (f64, f64);
/// The three control points of a cubic (`P1`, `P2`, `P3`), each present or not.
type Cubic = (Option<Pt>, Option<Pt>, Option<Pt>);

fn bezier_control_points(operator: &[u8], n: &[f64], cur: Pt) -> Cubic {
    match operator {
        b"c" if n.len() == 6 => (Some((n[0], n[1])), Some((n[2], n[3])), Some((n[4], n[5]))),
        // v: first control point = current point.
        b"v" if n.len() == 4 => (Some(cur), Some((n[0], n[1])), Some((n[2], n[3]))),
        // y: second control point = end point.
        b"y" if n.len() == 4 => (Some((n[0], n[1])), Some((n[2], n[3])), Some((n[2], n[3]))),
        _ => (None, None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Page user space → device for a 200pt-tall page at scale 1 (y-flip).
    fn base() -> Matrix {
        Matrix::new(1.0, 0.0, 0.0, -1.0, 0.0, 200.0)
    }

    #[test]
    fn fills_a_red_rectangle_from_content() {
        // User-space rect (50,50)-(150,130) → device rows 70..150, cols 50..150.
        let canvas = render_content(
            b"1 0 0 rg 50 50 100 80 re f",
            200,
            200,
            base(),
            &RenderFonts::new(),
            &RenderImages::new(),
        );
        let idx = (110 * 200 + 100) * 4;
        assert_eq!(
            &canvas.pixels[idx..idx + 3],
            &[255, 0, 0],
            "interior is red"
        );
        assert_eq!(&canvas.pixels[0..3], &[255, 255, 255], "corner stays white");
    }

    #[test]
    fn blits_an_image_over_the_unit_square() {
        // 2×2 image: red, green (top) / blue, white (bottom).
        let image = RenderImage {
            width: 2,
            height: 2,
            rgba: vec![
                255, 0, 0, 255, 0, 255, 0, 255, // top row
                0, 0, 255, 255, 255, 255, 255, 255, // bottom row
            ],
            stencil: false,
        };
        let mut images = RenderImages::new();
        images.insert(b"Im0".to_vec(), image);
        // Scale the unit square to a 100×100 box at the origin, then draw it.
        let canvas = render_content(
            b"100 0 0 100 0 0 cm /Im0 Do",
            100,
            100,
            Matrix::new(1.0, 0.0, 0.0, -1.0, 0.0, 100.0),
            &RenderFonts::new(),
            &images,
        );
        let at = |x: usize, y: usize| {
            let i = (y * 100 + x) * 4;
            [canvas.pixels[i], canvas.pixels[i + 1], canvas.pixels[i + 2]]
        };
        assert_eq!(at(10, 10), [255, 0, 0], "top-left texel is red");
        assert_eq!(at(90, 90), [255, 255, 255], "bottom-right texel is white");
        assert_eq!(at(90, 10), [0, 255, 0], "top-right texel is green");
    }

    #[test]
    fn stencil_image_paints_fill_colour_through_alpha() {
        // A 2×2 stencil (`stencil: true`): top-left opaque (alpha 255), the rest
        // transparent (alpha 0). The RGB samples are placeholders — the painted
        // pixel must take the *current fill colour* (set blue via `rg`), and the
        // transparent pixels must stay white paper.
        let image = RenderImage {
            width: 2,
            height: 2,
            rgba: vec![
                0, 0, 0, 255, 0, 0, 0, 0, // top row: paint, skip
                0, 0, 0, 0, 0, 0, 0, 0, // bottom row: skip, skip
            ],
            stencil: true,
        };
        let mut images = RenderImages::new();
        images.insert(b"Im0".to_vec(), image);
        let canvas = render_content(
            b"0 0 1 rg 100 0 0 100 0 0 cm /Im0 Do",
            100,
            100,
            Matrix::new(1.0, 0.0, 0.0, -1.0, 0.0, 100.0),
            &RenderFonts::new(),
            &images,
        );
        let at = |x: usize, y: usize| {
            let i = (y * 100 + x) * 4;
            [canvas.pixels[i], canvas.pixels[i + 1], canvas.pixels[i + 2]]
        };
        assert_eq!(
            at(25, 25),
            [0, 0, 255],
            "painted stencil pixel is the fill blue"
        );
        assert_eq!(
            at(75, 25),
            [255, 255, 255],
            "transparent stencil pixel stays white"
        );
        assert_eq!(
            at(75, 75),
            [255, 255, 255],
            "transparent stencil pixel stays white"
        );
    }

    #[test]
    fn cmyk_and_gray_fills() {
        // Pure cyan via k, then a mid-gray box.
        let canvas = render_content(
            b"1 0 0 0 k 10 10 40 40 re f 0.5 g 100 100 40 40 re f",
            200,
            200,
            base(),
            &RenderFonts::new(),
            &RenderImages::new(),
        );
        let cyan = ((200 - 30) * 200 + 30) * 4; // inside first box
        assert_eq!(&canvas.pixels[cyan..cyan + 3], &[0, 255, 255]);
        let g = ((200 - 120) * 200 + 120) * 4;
        assert_eq!(canvas.pixels[g], 128);
    }

    // ── optional-content (layer) visibility enforcement (issue #54) ──

    /// A `ResourceCtx` that answers only the optional-content questions, from a
    /// fixed name→visible table (everything else resolves to nothing). Names
    /// absent from the table default to **visible** — matching a stream that
    /// references no layer.
    struct OcStub {
        /// `/OC <name> BDC` property names that are visible.
        props: std::collections::BTreeMap<Vec<u8>, bool>,
        /// `Do` XObject names whose `/OC` is visible.
        xobjects: std::collections::BTreeMap<Vec<u8>, bool>,
    }

    impl ResourceCtx for OcStub {
        fn form_xobject(&self, _name: &[u8]) -> Option<FormXObject<'_>> {
            None
        }
        fn shading(&self, _name: &[u8]) -> Option<Shading> {
            None
        }
        fn pattern_shading(&self, _name: &[u8]) -> Option<Shading> {
            None
        }
        fn tiling_pattern(&self, _name: &[u8]) -> Option<FormXObject<'_>> {
            None
        }
        fn ext_gstate(&self, _name: &[u8]) -> Option<ExtGStateParams> {
            None
        }
        fn resolve_color(&self, _name: &[u8], _comps: &[f64]) -> Option<[u8; 3]> {
            None
        }
        fn oc_marked_visible(&self, prop: &Object) -> bool {
            match prop.as_name() {
                Some(name) => self.props.get(name).copied().unwrap_or(true),
                None => true,
            }
        }
        fn oc_xobject_visible(&self, name: &[u8]) -> bool {
            self.xobjects.get(name).copied().unwrap_or(true)
        }
    }

    fn px(canvas: &Canvas, x: usize, y: usize) -> [u8; 3] {
        let i = (y * canvas.width as usize + x) * 4;
        [canvas.pixels[i], canvas.pixels[i + 1], canvas.pixels[i + 2]]
    }

    #[test]
    fn off_layer_marked_content_is_not_painted() {
        // Two OCGs: `oc0` ON (red box, left), `oc1` OFF (blue box, right).
        // Only the ON layer's box must be painted; the OFF layer's box is gone.
        let mut props = std::collections::BTreeMap::new();
        props.insert(b"oc0".to_vec(), true);
        props.insert(b"oc1".to_vec(), false);
        let ctx = OcStub {
            props,
            xobjects: std::collections::BTreeMap::new(),
        };
        let content = b"/OC /oc0 BDC 1 0 0 rg 10 100 40 40 re f EMC\n\
                        /OC /oc1 BDC 0 0 1 rg 150 100 40 40 re f EMC\n";
        let mut canvas = Canvas::new(200, 200);
        render_content_into_ctx(
            &mut canvas,
            content,
            base(),
            &RenderFonts::new(),
            &RenderImages::new(),
            1.0,
            &ctx,
            0,
            None,
            false,
            &[],
        );
        // Left box (ON) is red; right box (OFF) stayed white paper.
        assert_eq!(px(&canvas, 30, 80), [255, 0, 0], "ON layer box is painted");
        assert_eq!(
            px(&canvas, 170, 80),
            [255, 255, 255],
            "OFF layer box is hidden (not rasterized)"
        );
    }

    #[test]
    fn no_optional_content_paints_everything() {
        // The same two boxes but with NO `/OC` brackets: both must paint, proving
        // the enforcement is inert for ordinary (non-layered) content.
        let ctx = OcStub {
            props: std::collections::BTreeMap::new(),
            xobjects: std::collections::BTreeMap::new(),
        };
        let content = b"1 0 0 rg 10 100 40 40 re f\n0 0 1 rg 150 100 40 40 re f\n";
        let mut canvas = Canvas::new(200, 200);
        render_content_into_ctx(
            &mut canvas,
            content,
            base(),
            &RenderFonts::new(),
            &RenderImages::new(),
            1.0,
            &ctx,
            0,
            None,
            false,
            &[],
        );
        assert_eq!(px(&canvas, 30, 80), [255, 0, 0], "first box paints");
        assert_eq!(px(&canvas, 170, 80), [0, 0, 255], "second box paints");
    }

    #[test]
    fn nested_oc_hidden_when_outer_is_off() {
        // Outer layer OFF, inner layer ON: the inner content is still hidden
        // because an enclosing layer is off (visibility stack, not just the
        // innermost frame).
        let mut props = std::collections::BTreeMap::new();
        props.insert(b"outer".to_vec(), false);
        props.insert(b"inner".to_vec(), true);
        let ctx = OcStub {
            props,
            xobjects: std::collections::BTreeMap::new(),
        };
        let content = b"/OC /outer BDC\n\
                          1 0 0 rg 10 100 40 40 re f\n\
                          /OC /inner BDC 0 0 1 rg 60 100 40 40 re f EMC\n\
                        EMC\n";
        let mut canvas = Canvas::new(200, 200);
        render_content_into_ctx(
            &mut canvas,
            content,
            base(),
            &RenderFonts::new(),
            &RenderImages::new(),
            1.0,
            &ctx,
            0,
            None,
            false,
            &[],
        );
        assert_eq!(px(&canvas, 30, 80), [255, 255, 255], "outer-OFF box hidden");
        assert_eq!(
            px(&canvas, 80, 80),
            [255, 255, 255],
            "inner-ON box still hidden under an OFF ancestor"
        );
    }

    #[test]
    fn inner_on_paints_after_off_sibling_closes() {
        // `/OC /off BDC … EMC` (hidden) followed by `/OC /on BDC … EMC` (visible):
        // the `EMC` must pop the OFF frame so the next, ON layer paints. Guards
        // the stack balance (a mismatched pop would leave the page hidden).
        let mut props = std::collections::BTreeMap::new();
        props.insert(b"off".to_vec(), false);
        props.insert(b"on".to_vec(), true);
        let ctx = OcStub {
            props,
            xobjects: std::collections::BTreeMap::new(),
        };
        let content = b"/OC /off BDC 1 0 0 rg 10 100 40 40 re f EMC\n\
                        /OC /on BDC 0 0 1 rg 150 100 40 40 re f EMC\n";
        let mut canvas = Canvas::new(200, 200);
        render_content_into_ctx(
            &mut canvas,
            content,
            base(),
            &RenderFonts::new(),
            &RenderImages::new(),
            1.0,
            &ctx,
            0,
            None,
            false,
            &[],
        );
        assert_eq!(px(&canvas, 30, 80), [255, 255, 255], "OFF layer hidden");
        assert_eq!(
            px(&canvas, 170, 80),
            [0, 0, 255],
            "ON layer paints after the OFF sibling's EMC"
        );
    }

    #[test]
    fn off_xobject_do_paints_nothing() {
        // A `Do` whose XObject's `/OC` is OFF must paint nothing, even though the
        // image is present in the page image map (image-XObject branch of `Do`).
        let image = RenderImage {
            width: 1,
            height: 1,
            rgba: vec![255, 0, 0, 255],
            stencil: false,
        };
        let mut images = RenderImages::new();
        images.insert(b"Im0".to_vec(), image);
        let mut xobjects = std::collections::BTreeMap::new();
        xobjects.insert(b"Im0".to_vec(), false); // its /OC is OFF
        let ctx = OcStub {
            props: std::collections::BTreeMap::new(),
            xobjects,
        };
        let content = b"100 0 0 100 0 0 cm /Im0 Do";
        let mut canvas = Canvas::new(100, 100);
        render_content_into_ctx(
            &mut canvas,
            content,
            Matrix::new(1.0, 0.0, 0.0, -1.0, 0.0, 100.0),
            &RenderFonts::new(),
            &images,
            1.0,
            &ctx,
            0,
            None,
            false,
            &[],
        );
        assert_eq!(
            px(&canvas, 50, 50),
            [255, 255, 255],
            "OFF XObject image is not blitted"
        );

        // …and with its `/OC` ON, the same content blits the red image.
        let mut xobjects = std::collections::BTreeMap::new();
        xobjects.insert(b"Im0".to_vec(), true);
        let ctx_on = OcStub {
            props: std::collections::BTreeMap::new(),
            xobjects,
        };
        let image_on = RenderImage {
            width: 1,
            height: 1,
            rgba: vec![255, 0, 0, 255],
            stencil: false,
        };
        let mut images_on = RenderImages::new();
        images_on.insert(b"Im0".to_vec(), image_on);
        let mut canvas_on = Canvas::new(100, 100);
        render_content_into_ctx(
            &mut canvas_on,
            content,
            Matrix::new(1.0, 0.0, 0.0, -1.0, 0.0, 100.0),
            &RenderFonts::new(),
            &images_on,
            1.0,
            &ctx_on,
            0,
            None,
            false,
            &[],
        );
        assert_eq!(
            px(&canvas_on, 50, 50),
            [255, 0, 0],
            "ON XObject image blits"
        );
    }
}
