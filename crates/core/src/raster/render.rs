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

/// The shape of a PDF shading (ISO 32000-1 §8.7.4.5): an axial (type 2) or radial
/// (type 3) gradient. Coordinates are in the shading's own space; the renderer
/// maps device pixels back into this space to evaluate the gradient parameter `t`.
#[derive(Debug, Clone)]
pub enum ShadingKind {
    /// Axial: the gradient runs along the segment `(x0,y0)→(x1,y1)`.
    Axial {
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
    },
    /// Radial: between circle `(x0,y0,r0)` and circle `(x1,y1,r1)`.
    Radial {
        x0: f64,
        y0: f64,
        r0: f64,
        x1: f64,
        y1: f64,
        r1: f64,
    },
}

/// A resolved shading ready to paint: its geometry, a pre-sampled 256-entry RGB
/// colour ramp across `t ∈ [0,1]` (the `/Function` already evaluated), the two
/// `/Extend` flags, and the matrix mapping shading space to the device.
#[derive(Debug, Clone)]
pub struct Shading {
    /// Gradient geometry, in shading space.
    pub kind: ShadingKind,
    /// 256 RGB triples sampled across the colour function (`t = i/255`).
    pub ramp: Vec<[u8; 3]>,
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
    /// Simple-font character-code → glyph-id map, resolved from the PDF
    /// `/Encoding` (base + `/Differences`) against the program's own charset.
    /// Primary glyph-selection path for simple fonts whose program has no usable
    /// Unicode cmap (notably subset CFF, where `gid_for_unicode` is unavailable).
    /// `None` falls back to the code→Unicode→cmap path.
    pub code_to_gid: Option<BTreeMap<u32, u16>>,
    /// Composite-font CID → glyph-id map from a non-identity `/CIDToGIDMap`
    /// stream. `None` means identity (CID is the glyph id), the common case.
    pub cid_to_gid: Option<Vec<u16>>,
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
}

/// Per-page image XObjects, keyed by resource name (as used by `Do`).
pub type RenderImages = BTreeMap<Vec<u8>, RenderImage>;

/// Blit an image XObject into the canvas. The image fills the unit square in
/// user space, mapped to the device by `ctm` then `base`; we inverse-map each
/// device pixel to a texel so up- and down-scaling both work without gaps.
/// `global_alpha` (`0.0..=1.0`) scales the image's own per-pixel alpha — used to
/// honour an annotation appearance's `/CA` opacity. An optional `clip` (active
/// `W` clip ∩ soft mask) further modulates each pixel, and `blend` selects the
/// compositing mode.
#[allow(clippy::too_many_arguments)]
fn blit_image_clipped(
    canvas: &mut Canvas,
    image: &RenderImage,
    ctm: &Matrix,
    base: &Matrix,
    global_alpha: f64,
    clip: Option<&ClipMask>,
    blend: BlendMode,
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
            let color = [image.rgba[idx], image.rgba[idx + 1], image.rgba[idx + 2]];
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
    );
}

/// The full content-stream rasterizer: like [`render_content_into`] but resolves
/// nested resources through `ctx` — form XObjects (`Do`), axial/radial shadings
/// (`sh` and shading-pattern fills), and ExtGState blend modes + soft masks
/// (`gs`). `depth` is the current form-nesting depth (cycle/recursion guard);
/// `init_clip` seeds the graphics-state clip (a parent form's `/BBox` + active
/// clip when recursing).
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

    for op in &operations {
        let n = nums(op);
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
                    state.fill = resolve_set_color(ctx, state.fill_cs.as_deref(), &n)
                        .unwrap_or(state.fill);
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
                commit_pending_clip(&mut state, &subpaths, &mut pending_clip, canvas);
                subpaths.clear();
            }
            b"S" | b"s" => {
                let lw = device_scale(&state.ctm, &base) * state.line_width;
                let clip = state.paint_clip();
                canvas.fill_ext(
                    &stroke_edges(&subpaths, lw),
                    state.stroke,
                    false,
                    global_alpha,
                    clip.as_ref(),
                    state.blend,
                );
                commit_pending_clip(&mut state, &subpaths, &mut pending_clip, canvas);
                subpaths.clear();
            }
            b"n" => {
                commit_pending_clip(&mut state, &subpaths, &mut pending_clip, canvas);
                subpaths.clear();
            }

            // Paint a shading directly across the current clip region.
            b"sh" => {
                if let Some(Object::Name(name)) = op.operands.first() {
                    if let Some(mut shading) = ctx.shading(name) {
                        // `sh` paints in the current user space → device.
                        shading.to_device = state.ctm.then(&base);
                        let clip = state.paint_clip();
                        paint_shading(canvas, &shading, clip.as_ref(), global_alpha, state.blend);
                    }
                }
            }

            b"Do" => {
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
                        );
                    } else if depth < crate::content::MAX_FORM_DEPTH {
                        if let Some(form) = ctx.form_xobject(name) {
                            draw_form(canvas, &form, &state, &base, global_alpha, depth);
                        }
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
                    );
                }
            }
            b"TJ" => {
                if let (Some(f), Some(Object::Array(items))) = (font, op.operands.first()) {
                    let clip = state.paint_clip();
                    for item in items {
                        if let Object::String(bytes, _) = item {
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
                            );
                        } else if let Some(adj) = item.as_f64() {
                            let dx = -adj / 1000.0 * font_size * h_scale;
                            tm = Matrix::translate(dx, 0.0).then(&tm);
                        }
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
fn draw_form(
    canvas: &mut Canvas,
    form: &FormXObject,
    state: &GState,
    base: &Matrix,
    global_alpha: f64,
    depth: usize,
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
    );
}

/// Render a text-show string: for each character code, look up its glyph and
/// fill the outline, advancing the text matrix by the glyph's width.
/// `global_alpha` scales the fill coverage (annotation `/CA` opacity); `clip`
/// modulates per pixel (active `W` clip ∩ soft mask) and `blend` selects the mode.
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
) {
    let mut i = 0;
    while i < bytes.len() {
        let (code, consumed): (u32, usize) = if font.two_byte && i + 1 < bytes.len() {
            (((bytes[i] as u32) << 8) | bytes[i + 1] as u32, 2)
        } else {
            (bytes[i] as u32, 1)
        };
        let code_bytes = &bytes[i..i + consumed];
        i += consumed;

        let mut advance = size * 0.5; // fallback width
        if let Some(ttf) = &font.program {
            let upem = ttf.units_per_em();
            let gid = if font.two_byte {
                // Composite font: the 2-byte code is the CID. Map CID → glyph id
                // through a non-identity `/CIDToGIDMap` when present, else the
                // CID is the glyph id directly (the Identity case).
                match &font.cid_to_gid {
                    Some(map) => map.get(code as usize).copied().unwrap_or(0),
                    None => code as u16,
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
                        let (ux, uy) = tm.apply(gx * s * h_scale, gy * s);
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
            advance = ttf.advance_width(gid) / upem * size;
        }

        let mut step = advance + char_spacing;
        if consumed == 1 && code == 32 {
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
    let Some(inv) = invert(&shading.to_device) else {
        return;
    };
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
        paint_shading(canvas, &shading, Some(&clip), global_alpha * state.fill_alpha, state.blend);
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
}
