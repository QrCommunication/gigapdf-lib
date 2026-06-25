//! Shared GDI rasterization primitives for the WMF/EMF interpreters.
//!
//! Everything here is self-contained and zero-dependency: a transparent RGBA8
//! framebuffer with source-over alpha compositing ([`Canvas`]), anti-aliased
//! line and thick-stroke drawing, scanline polygon filling (the two GDI fill
//! modes — alternate / winding), the GDI **graphics state** with its object
//! table (pens / brushes / fonts + stock objects), the logical→device
//! transform (window/viewport + map mode + EMF world transform), and a from
//! scratch **DIB/BMP** decoder (1/4/8/24/32 bpp + RLE4/RLE8) used by the blit
//! records.
//!
//! Device space is top-left origin, y down (the on-canvas pixel grid). Callers
//! feed *logical* coordinates and the [`Gdi`] transform maps them to device
//! pixels.

// ───────────────────────────── colour ─────────────────────────────

/// An opaque-or-not RGBA colour, channels `0..=255`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const TRANSPARENT: Rgba = Rgba {
        r: 0,
        g: 0,
        b: 0,
        a: 0,
    };

    pub const fn rgb(r: u8, g: u8, b: u8) -> Rgba {
        Rgba { r, g, b, a: 255 }
    }

    /// A Windows `COLORREF` (`0x00bbggrr`, little-endian in the file) → RGBA.
    pub fn from_colorref(c: u32) -> Rgba {
        Rgba::rgb(
            (c & 0xFF) as u8,
            ((c >> 8) & 0xFF) as u8,
            ((c >> 16) & 0xFF) as u8,
        )
    }
}

// ───────────────────────────── canvas ─────────────────────────────

/// A transparent RGBA8 framebuffer (row-major, top-to-bottom) with source-over
/// compositing.
#[derive(Debug, Clone)]
pub struct Canvas {
    pub width: u32,
    pub height: u32,
    /// `width*height*4` bytes, RGBA, premultiplied-free straight alpha.
    pub pixels: Vec<u8>,
}

impl Canvas {
    /// A fully transparent canvas of `width` × `height`.
    pub fn new(width: u32, height: u32) -> Canvas {
        Canvas {
            width,
            height,
            pixels: vec![0u8; (width as usize) * (height as usize) * 4],
        }
    }

    /// Composite straight-alpha `src` over pixel `(x, y)` scaled by coverage
    /// `cov` (`0.0..=1.0`). Out-of-bounds and zero-coverage are no-ops.
    pub fn blend(&mut self, x: i32, y: i32, src: Rgba, cov: f64) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        let sa = (src.a as f64 / 255.0) * cov.clamp(0.0, 1.0);
        if sa <= 0.0 {
            return;
        }
        let idx = ((y as usize) * (self.width as usize) + x as usize) * 4;
        let da = self.pixels[idx + 3] as f64 / 255.0;
        let out_a = sa + da * (1.0 - sa);
        if out_a <= 0.0 {
            return;
        }
        for c in 0..3 {
            let sc = [src.r, src.g, src.b][c] as f64 / 255.0;
            let dc = self.pixels[idx + c] as f64 / 255.0;
            let oc = (sc * sa + dc * da * (1.0 - sa)) / out_a;
            self.pixels[idx + c] = (oc * 255.0).round().clamp(0.0, 255.0) as u8;
        }
        self.pixels[idx + 3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
    }

    /// Overwrite pixel `(x, y)` with straight-alpha `src` (no blending) — used by
    /// the DIB blit, which replaces device pixels.
    pub fn put(&mut self, x: i32, y: i32, src: Rgba) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        let idx = ((y as usize) * (self.width as usize) + x as usize) * 4;
        self.pixels[idx] = src.r;
        self.pixels[idx + 1] = src.g;
        self.pixels[idx + 2] = src.b;
        self.pixels[idx + 3] = src.a;
    }
}

// ───────────────────────────── geometry ─────────────────────────────

/// A point in device pixels (post-transform).
#[derive(Debug, Clone, Copy)]
pub struct Pt {
    pub x: f64,
    pub y: f64,
}

/// One straight edge of a flattened polygon, in device pixels.
#[derive(Debug, Clone, Copy)]
struct Edge {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
}

/// Fill a set of closed sub-paths (`polys`, device pixels) with `color`, using
/// the GDI fill rule (`alternate` = even-odd, otherwise winding). 4× vertical
/// supersampling with exact horizontal coverage gives anti-aliased edges; an
/// optional axis-aligned `clip` rectangle bounds the output.
pub fn fill_polygons(
    canvas: &mut Canvas,
    polys: &[Vec<Pt>],
    color: Rgba,
    alternate: bool,
    clip: Option<&ClipRect>,
) {
    if color.a == 0 {
        return;
    }
    let mut edges: Vec<Edge> = Vec::new();
    for poly in polys {
        if poly.len() < 2 {
            continue;
        }
        for i in 0..poly.len() {
            let a = poly[i];
            let b = poly[(i + 1) % poly.len()];
            edges.push(Edge {
                x0: a.x,
                y0: a.y,
                x1: b.x,
                y1: b.y,
            });
        }
    }
    if edges.is_empty() {
        return;
    }
    rasterize_coverage(
        &edges,
        canvas.width,
        canvas.height,
        alternate,
        &mut |px, py, cov| {
            if let Some(c) = clip {
                if !c.contains(px, py) {
                    return;
                }
            }
            canvas.blend(px, py, color, cov);
        },
    );
}

/// Scanline coverage of `edges` → `emit(px, py, coverage)` per touched pixel.
/// Mirrors the engine's page rasterizer (4× vertical subsamples, exact
/// horizontal spans). `alternate` selects even-odd over non-zero winding.
fn rasterize_coverage(
    edges: &[Edge],
    width: u32,
    height: u32,
    alternate: bool,
    emit: &mut dyn FnMut(i32, i32, f64),
) {
    const SS: usize = 4;
    let mut min_y = f64::INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for e in edges {
        min_y = min_y.min(e.y0.min(e.y1));
        max_y = max_y.max(e.y0.max(e.y1));
    }
    if !min_y.is_finite() || !max_y.is_finite() {
        return;
    }
    let y_start = (min_y.floor().max(0.0)) as i32;
    let y_end = (max_y.ceil().min(height as f64)) as i32;
    let mut coverage = vec![0.0f64; width as usize];

    for py in y_start..y_end {
        for c in coverage.iter_mut() {
            *c = 0.0;
        }
        for sub in 0..SS {
            let sy = py as f64 + (sub as f64 + 0.5) / SS as f64;
            let mut crossings: Vec<(f64, i32)> = Vec::new();
            for e in edges {
                let (mut ax, mut ay, mut bx, mut by) = (e.x0, e.y0, e.x1, e.y1);
                if ay == by {
                    continue;
                }
                let dir = if ay < by { 1 } else { -1 };
                if dir < 0 {
                    std::mem::swap(&mut ax, &mut bx);
                    std::mem::swap(&mut ay, &mut by);
                }
                if sy < ay || sy >= by {
                    continue;
                }
                let t = (sy - ay) / (by - ay);
                crossings.push((ax + t * (bx - ax), dir));
            }
            if crossings.len() < 2 {
                continue;
            }
            crossings.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            let mut winding = 0i32;
            for pair in crossings.windows(2) {
                let (x_left, dir) = pair[0];
                let x_right = pair[1].0;
                winding += dir;
                let inside = if alternate {
                    winding % 2 != 0
                } else {
                    winding != 0
                };
                if !inside {
                    continue;
                }
                add_span(&mut coverage, x_left, x_right, 1.0 / SS as f64);
            }
        }
        for (px, &cov) in coverage.iter().enumerate() {
            if cov > 0.0 {
                emit(px as i32, py, cov.min(1.0));
            }
        }
    }
}

/// Accumulate exact horizontal coverage for the span `[x_left, x_right)` into the
/// per-row `coverage` buffer, weighted by `weight` (one sub-scanline's share).
fn add_span(coverage: &mut [f64], x_left: f64, x_right: f64, weight: f64) {
    if x_right <= x_left {
        return;
    }
    let w = coverage.len() as f64;
    let xl = x_left.max(0.0);
    let xr = x_right.min(w);
    if xr <= xl {
        return;
    }
    let mut px = xl.floor() as usize;
    let last = (xr.ceil() as usize).min(coverage.len());
    while px < last {
        let cell_l = px as f64;
        let cell_r = cell_l + 1.0;
        let covered = (xr.min(cell_r) - xl.max(cell_l)).max(0.0);
        coverage[px] += covered * weight;
        px += 1;
    }
}

/// Stroke a polyline (`pts`, device pixels) with `color` and `width` device
/// pixels. Thin lines (≤ ~1.2 px) use an anti-aliased 1-px Wu-style line; wider
/// lines are filled as quads per segment plus round joins, honouring an optional
/// `dash` pattern (device-pixel on/off lengths) and clip rectangle.
#[allow(clippy::too_many_arguments)]
pub fn stroke_polyline(
    canvas: &mut Canvas,
    pts: &[Pt],
    color: Rgba,
    width: f64,
    dash: &[f64],
    closed: bool,
    clip: Option<&ClipRect>,
) {
    if pts.len() < 2 || color.a == 0 {
        return;
    }
    let w = width.max(0.0);
    let mut segs: Vec<(Pt, Pt)> = Vec::new();
    let n = if closed { pts.len() } else { pts.len() - 1 };
    for i in 0..n {
        segs.push((pts[i], pts[(i + 1) % pts.len()]));
    }
    if dash.is_empty() || dash.iter().all(|d| *d <= 0.0) {
        for (a, b) in &segs {
            stroke_segment(canvas, *a, *b, color, w, clip);
        }
    } else {
        // Walk the dash pattern continuously across the whole polyline.
        let total: f64 = dash.iter().sum();
        if total <= 0.0 {
            for (a, b) in &segs {
                stroke_segment(canvas, *a, *b, color, w, clip);
            }
            return;
        }
        let mut dash_idx = 0usize;
        let mut dash_left = dash[0];
        let mut on = true;
        for (a, b) in &segs {
            let mut cur = *a;
            let seg_len = (b.x - a.x).hypot(b.y - a.y);
            if seg_len <= 1e-9 {
                continue;
            }
            let dx = (b.x - a.x) / seg_len;
            let dy = (b.y - a.y) / seg_len;
            let mut remaining = seg_len;
            while remaining > 1e-9 {
                let step = dash_left.min(remaining);
                let next = Pt {
                    x: cur.x + dx * step,
                    y: cur.y + dy * step,
                };
                if on {
                    stroke_segment(canvas, cur, next, color, w, clip);
                }
                cur = next;
                remaining -= step;
                dash_left -= step;
                if dash_left <= 1e-9 {
                    dash_idx = (dash_idx + 1) % dash.len();
                    dash_left = dash[dash_idx];
                    on = !on;
                }
            }
        }
    }
}

/// Stroke one segment `a→b` at `width` device pixels.
fn stroke_segment(
    canvas: &mut Canvas,
    a: Pt,
    b: Pt,
    color: Rgba,
    width: f64,
    clip: Option<&ClipRect>,
) {
    if width <= 1.2 {
        draw_line_aa(canvas, a, b, color, clip);
        return;
    }
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    let len = dx.hypot(dy);
    if len <= 1e-9 {
        // Degenerate: a dot of radius width/2.
        fill_disc(canvas, a, width / 2.0, color, clip);
        return;
    }
    let h = width / 2.0;
    let nx = -dy / len * h;
    let ny = dx / len * h;
    let quad = vec![
        Pt {
            x: a.x + nx,
            y: a.y + ny,
        },
        Pt {
            x: b.x + nx,
            y: b.y + ny,
        },
        Pt {
            x: b.x - nx,
            y: b.y - ny,
        },
        Pt {
            x: a.x - nx,
            y: a.y - ny,
        },
    ];
    fill_polygons(canvas, &[quad], color, false, clip);
    // Round caps/joins so consecutive segments connect cleanly.
    fill_disc(canvas, a, h, color, clip);
    fill_disc(canvas, b, h, color, clip);
}

/// Fill a filled disc (round join/cap) of `radius` at `center`.
fn fill_disc(canvas: &mut Canvas, center: Pt, radius: f64, color: Rgba, clip: Option<&ClipRect>) {
    if radius <= 0.0 {
        return;
    }
    const SEG: usize = 16;
    let mut poly = Vec::with_capacity(SEG);
    for i in 0..SEG {
        let t = (i as f64) / (SEG as f64) * std::f64::consts::TAU;
        poly.push(Pt {
            x: center.x + radius * t.cos(),
            y: center.y + radius * t.sin(),
        });
    }
    fill_polygons(canvas, &[poly], color, false, clip);
}

/// Anti-aliased 1-pixel line (Xiaolin-Wu style) for thin strokes.
fn draw_line_aa(canvas: &mut Canvas, a: Pt, b: Pt, color: Rgba, clip: Option<&ClipRect>) {
    let mut x0 = a.x;
    let mut y0 = a.y;
    let mut x1 = b.x;
    let mut y1 = b.y;
    let steep = (y1 - y0).abs() > (x1 - x0).abs();
    if steep {
        std::mem::swap(&mut x0, &mut y0);
        std::mem::swap(&mut x1, &mut y1);
    }
    if x0 > x1 {
        std::mem::swap(&mut x0, &mut x1);
        std::mem::swap(&mut y0, &mut y1);
    }
    let dx = x1 - x0;
    let dy = y1 - y0;
    let gradient = if dx.abs() < 1e-9 { 1.0 } else { dy / dx };

    let mut plot = |x: i32, y: i32, c: f64| {
        if c <= 0.0 {
            return;
        }
        let (px, py) = if steep { (y, x) } else { (x, y) };
        if let Some(cl) = clip {
            if !cl.contains(px, py) {
                return;
            }
        }
        canvas.blend(px, py, color, c);
    };

    let xstart = x0.round();
    let xend = x1.round();
    let mut intery = y0 + gradient * (xstart - x0);
    let mut x = xstart as i32;
    let xlast = xend as i32;
    while x <= xlast {
        let y = intery.floor();
        let f = intery - y;
        plot(x, y as i32, 1.0 - f);
        plot(x, y as i32 + 1, f);
        intery += gradient;
        x += 1;
    }
}

// ───────────────────────────── clip rect ─────────────────────────────

/// An axis-aligned device-pixel clip rectangle (GDI `IntersectClipRect` / region
/// bbox), half-open `[left, right) × [top, bottom)`.
#[derive(Debug, Clone, Copy)]
pub struct ClipRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl ClipRect {
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }
}

// ───────────────────────────── GDI objects ─────────────────────────────

/// Pen line styles (`PS_*`). Only the geometric distinction matters here:
/// `Null` draws nothing; `Solid`/dashed differ by their dash pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PenStyle {
    Solid,
    Dash,
    Dot,
    DashDot,
    DashDotDot,
    Null,
    InsideFrame,
}

impl PenStyle {
    pub fn from_u32(v: u32) -> PenStyle {
        match v & 0x0F {
            0 => PenStyle::Solid,
            1 => PenStyle::Dash,
            2 => PenStyle::Dot,
            3 => PenStyle::DashDot,
            4 => PenStyle::DashDotDot,
            5 => PenStyle::Null,
            6 => PenStyle::InsideFrame,
            _ => PenStyle::Solid,
        }
    }

    /// The on/off dash pattern in *device pixels* for a pen of `width` px (0 ⇒
    /// solid). Lengths scale loosely with width, matching GDI's cosmetic look.
    pub fn dash_pattern(self, width: f64) -> Vec<f64> {
        let u = width.max(1.0);
        match self {
            PenStyle::Dash => vec![6.0 * u, 3.0 * u],
            PenStyle::Dot => vec![1.0 * u, 2.0 * u],
            PenStyle::DashDot => vec![5.0 * u, 2.0 * u, 1.0 * u, 2.0 * u],
            PenStyle::DashDotDot => vec![5.0 * u, 2.0 * u, 1.0 * u, 2.0 * u, 1.0 * u, 2.0 * u],
            _ => Vec::new(),
        }
    }
}

/// A GDI pen: line style, logical width, colour.
#[derive(Debug, Clone, Copy)]
pub struct Pen {
    pub style: PenStyle,
    /// Width in *logical* units (scaled to device by the active transform).
    pub width: f64,
    pub color: Rgba,
}

impl Pen {
    pub fn cosmetic_black() -> Pen {
        Pen {
            style: PenStyle::Solid,
            width: 0.0,
            color: Rgba::rgb(0, 0, 0),
        }
    }
}

/// Brush fill styles (`BS_*` / `HS_*`). Pattern/hatch brushes are approximated by
/// their solid colour (a faithful enough fill for import).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrushStyle {
    Solid,
    Null,
    Hatched,
    Pattern,
}

/// A GDI brush: style + colour.
#[derive(Debug, Clone, Copy)]
pub struct Brush {
    pub style: BrushStyle,
    pub color: Rgba,
}

impl Brush {
    pub fn white() -> Brush {
        Brush {
            style: BrushStyle::Solid,
            color: Rgba::rgb(255, 255, 255),
        }
    }

    pub fn null() -> Brush {
        Brush {
            style: BrushStyle::Null,
            color: Rgba::TRANSPARENT,
        }
    }
}

/// A GDI logical font — only the metrics we use for the text fallback box.
#[derive(Debug, Clone)]
pub struct Font {
    /// Em height in logical units (`lfHeight`; negative = char height).
    pub height: f64,
    /// Average char width (`lfWidth`; 0 ⇒ derive from height).
    pub width: f64,
    /// Escapement (text angle) in tenths of a degree.
    pub escapement: i32,
    pub bold: bool,
    pub italic: bool,
}

impl Default for Font {
    fn default() -> Self {
        Font {
            height: 12.0,
            width: 0.0,
            escapement: 0,
            bold: false,
            italic: false,
        }
    }
}

/// A handle-table slot: one of the GDI object kinds.
#[derive(Debug, Clone)]
pub enum GdiObject {
    Pen(Pen),
    Brush(Brush),
    Font(Font),
    /// A region reduced to its bounding rectangle (logical units).
    Region(LogRect),
    /// An empty/placeholder slot (after delete, or a kind we don't model).
    Empty,
}

/// A rectangle in logical units.
#[derive(Debug, Clone, Copy)]
pub struct LogRect {
    pub left: f64,
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
}

// ───────────────────────────── transform ─────────────────────────────

/// A 2×3 affine transform mapping logical → device (or page → device), row form
/// `[m11 m12; m21 m22; dx dy]` so `x' = m11*x + m21*y + dx`.
#[derive(Debug, Clone, Copy)]
pub struct Affine {
    pub m11: f64,
    pub m12: f64,
    pub m21: f64,
    pub m22: f64,
    pub dx: f64,
    pub dy: f64,
}

impl Affine {
    pub fn identity() -> Affine {
        Affine {
            m11: 1.0,
            m12: 0.0,
            m21: 0.0,
            m22: 1.0,
            dx: 0.0,
            dy: 0.0,
        }
    }

    /// Apply to a point.
    pub fn apply(&self, x: f64, y: f64) -> Pt {
        Pt {
            x: self.m11 * x + self.m21 * y + self.dx,
            y: self.m12 * x + self.m22 * y + self.dy,
        }
    }

    /// `self ∘ other` (apply `other` first, then `self`).
    pub fn concat(&self, other: &Affine) -> Affine {
        Affine {
            m11: self.m11 * other.m11 + self.m21 * other.m12,
            m12: self.m12 * other.m11 + self.m22 * other.m12,
            m21: self.m11 * other.m21 + self.m21 * other.m22,
            m22: self.m12 * other.m21 + self.m22 * other.m22,
            dx: self.m11 * other.dx + self.m21 * other.dy + self.dx,
            dy: self.m12 * other.dx + self.m22 * other.dy + self.dy,
        }
    }

    /// Average linear scale factor (for converting a logical line width to
    /// device pixels), the geometric mean of the row magnitudes.
    pub fn mean_scale(&self) -> f64 {
        let sx = (self.m11 * self.m11 + self.m12 * self.m12).sqrt();
        let sy = (self.m21 * self.m21 + self.m22 * self.m22).sqrt();
        ((sx * sy).abs()).sqrt().max(1e-6)
    }
}

// ───────────────────────────── graphics state ─────────────────────────────

/// The full GDI graphics state shared by both interpreters: the object table,
/// current pen/brush/font, drawing modes, current position, the logical→device
/// transform chain, and the destination canvas-to-logical mapping (`base`, which
/// maps the metafile's logical bounds onto the device raster).
#[derive(Debug)]
pub struct Gdi {
    pub canvas: Canvas,
    /// Maps **logical bounds** → device pixels (fixed for the whole playback).
    pub base: Affine,
    // Window/viewport mapping (logical → "page"/device-ish), composed under base.
    pub win_org: Pt,
    pub win_ext: Pt,
    pub vp_org: Pt,
    pub vp_ext: Pt,
    pub map_mode: i32,
    /// EMF world transform (logical → page), identity for WMF.
    pub world: Affine,
    pub objects: Vec<GdiObject>,
    pub cur_pen: Pen,
    pub cur_brush: Brush,
    pub cur_font: Font,
    pub pos: Pt,
    pub text_color: Rgba,
    pub bk_color: Rgba,
    pub bk_opaque: bool,
    pub poly_fill_alternate: bool,
    pub clip: Option<ClipRect>,
}

impl Gdi {
    /// New state rasterizing onto a `width`×`height` device canvas, with `base`
    /// mapping logical bounds to device pixels.
    pub fn new(width: u32, height: u32, base: Affine) -> Gdi {
        Gdi {
            canvas: Canvas::new(width, height),
            base,
            win_org: Pt { x: 0.0, y: 0.0 },
            win_ext: Pt { x: 1.0, y: 1.0 },
            vp_org: Pt { x: 0.0, y: 0.0 },
            vp_ext: Pt { x: 1.0, y: 1.0 },
            map_mode: 8, // MM_ANISOTROPIC-ish default; window/ext drive scale
            world: Affine::identity(),
            objects: Vec::new(),
            cur_pen: Pen::cosmetic_black(),
            cur_brush: Brush::white(),
            cur_font: Font::default(),
            pos: Pt { x: 0.0, y: 0.0 },
            text_color: Rgba::rgb(0, 0, 0),
            bk_color: Rgba::rgb(255, 255, 255),
            bk_opaque: true,
            poly_fill_alternate: true,
            clip: None,
        }
    }

    /// The active logical→device transform: `base ∘ win/vp ∘ world`.
    pub fn transform(&self) -> Affine {
        // window→viewport scale and offset
        let sx = if self.win_ext.x != 0.0 {
            self.vp_ext.x / self.win_ext.x
        } else {
            1.0
        };
        let sy = if self.win_ext.y != 0.0 {
            self.vp_ext.y / self.win_ext.y
        } else {
            1.0
        };
        let winvp = Affine {
            m11: sx,
            m12: 0.0,
            m21: 0.0,
            m22: sy,
            dx: self.vp_org.x - self.win_org.x * sx,
            dy: self.vp_org.y - self.win_org.y * sy,
        };
        self.base.concat(&winvp).concat(&self.world)
    }

    /// Map a logical point to device pixels.
    pub fn to_device(&self, x: f64, y: f64) -> Pt {
        self.transform().apply(x, y)
    }

    /// The pen's device-pixel line width (≥ ~1 for cosmetic 0-width pens).
    pub fn pen_device_width(&self) -> f64 {
        let s = self.transform().mean_scale();
        (self.cur_pen.width * s).max(if self.cur_pen.width == 0.0 { 1.0 } else { 0.6 })
    }

    /// Insert `obj` into the first free handle slot (or append) and return its
    /// 0-based index — both interpreters track their own handle→index mapping on
    /// top of this.
    pub fn add_object(&mut self, obj: GdiObject) -> usize {
        if let Some(i) = self
            .objects
            .iter()
            .position(|o| matches!(o, GdiObject::Empty))
        {
            self.objects[i] = obj;
            i
        } else {
            self.objects.push(obj);
            self.objects.len() - 1
        }
    }

    /// Select object at `idx` as the current pen/brush/font.
    pub fn select_object(&mut self, idx: usize) {
        if let Some(obj) = self.objects.get(idx).cloned() {
            match obj {
                GdiObject::Pen(p) => self.cur_pen = p,
                GdiObject::Brush(b) => self.cur_brush = b,
                GdiObject::Font(f) => self.cur_font = f,
                _ => {}
            }
        }
    }

    pub fn delete_object(&mut self, idx: usize) {
        if let Some(slot) = self.objects.get_mut(idx) {
            *slot = GdiObject::Empty;
        }
    }

    /// Fill `polys` (logical) with the current brush, then stroke their outline
    /// with the current pen — the shape primitive shared by Rectangle / Ellipse /
    /// Polygon / Pie / Chord.
    pub fn fill_and_stroke(&mut self, polys: &[Vec<Pt>]) {
        if self.cur_brush.style != BrushStyle::Null {
            fill_polygons(
                &mut self.canvas,
                polys,
                self.cur_brush.color,
                self.poly_fill_alternate,
                self.clip.as_ref(),
            );
        }
        if self.cur_pen.style != PenStyle::Null {
            let w = self.pen_device_width();
            let dash = self.cur_pen.style.dash_pattern(w);
            for p in polys {
                stroke_polyline(
                    &mut self.canvas,
                    p,
                    self.cur_pen.color,
                    w,
                    &dash,
                    true,
                    self.clip.as_ref(),
                );
            }
        }
    }

    /// Stroke `pts` (already device pixels) with the current pen (open polyline).
    pub fn stroke_open_device(&mut self, pts: &[Pt], closed: bool) {
        if self.cur_pen.style == PenStyle::Null {
            return;
        }
        let w = self.pen_device_width();
        let dash = self.cur_pen.style.dash_pattern(w);
        stroke_polyline(
            &mut self.canvas,
            pts,
            self.cur_pen.color,
            w,
            &dash,
            closed,
            self.clip.as_ref(),
        );
    }

    /// Render a run of `glyphs` visible characters as a light **glyph strip** at
    /// logical origin `(x, y)`, honouring the current font's metrics, escapement
    /// (rotation, tenths of a degree, CCW), bold (heavier ink), and italic
    /// (shear). Text is secondary — this is the reasonable advance/box fallback,
    /// not a shaped-glyph rasterizer. `runs` is the per-cell visibility mask so a
    /// caller can skip spaces while preserving advance.
    pub fn draw_text(&mut self, x: f64, y: f64, runs: &[bool]) {
        if runs.is_empty() {
            return;
        }
        let fh = if self.cur_font.height > 0.0 {
            self.cur_font.height
        } else {
            12.0
        };
        let cw = if self.cur_font.width > 0.0 {
            self.cur_font.width
        } else {
            fh * 0.5
        };
        let ink = if self.cur_font.bold { 190 } else { 150 };
        let slant = if self.cur_font.italic { 0.22 } else { 0.0 };
        // Escapement: rotate the advance direction by `esc` tenths-of-degree CCW.
        // GDI Y is down, so a positive (CCW in logical) escapement steps the
        // baseline up-right — express the advance vector in logical units.
        let theta = -(self.cur_font.escapement as f64) * std::f64::consts::PI / 1800.0;
        let (ct, st) = (theta.cos(), theta.sin());
        let color = Rgba {
            a: ink,
            ..self.text_color
        };
        for (i, &visible) in runs.iter().enumerate() {
            if !visible {
                continue;
            }
            let ax = (i as f64) * cw;
            // Cell corners in glyph-local space (advance along +x, up is -y).
            let cell = [
                (cw * 0.1, 0.0),
                (cw * 0.85, 0.0),
                (cw * 0.85 + slant * fh, -fh * 0.9),
                (cw * 0.1 + slant * fh, -fh * 0.9),
            ];
            let poly: Vec<Pt> = cell
                .iter()
                .map(|&(lx, ly)| {
                    // Place along the (rotated) baseline, then map logical→device.
                    let gx = x + (ax + lx) * ct - ly * st;
                    let gy = y + (ax + lx) * st + ly * ct + fh * 0.9;
                    self.to_device(gx, gy)
                })
                .collect();
            fill_polygons(&mut self.canvas, &[poly], color, true, self.clip.as_ref());
        }
    }
}

// ───────────────────────────── shape helpers ─────────────────────────────

/// A device-space polygon approximation of the axis-aligned logical rect.
pub fn rect_poly(g: &Gdi, l: f64, t: f64, r: f64, b: f64) -> Vec<Pt> {
    vec![
        g.to_device(l, t),
        g.to_device(r, t),
        g.to_device(r, b),
        g.to_device(l, b),
    ]
}

/// A device-space polygon approximating the ellipse inscribed in the logical
/// rect `(l,t)-(r,b)` with `seg` segments.
pub fn ellipse_poly(g: &Gdi, l: f64, t: f64, r: f64, b: f64, seg: usize) -> Vec<Pt> {
    let cx = (l + r) / 2.0;
    let cy = (t + b) / 2.0;
    let rx = (r - l).abs() / 2.0;
    let ry = (b - t).abs() / 2.0;
    let mut poly = Vec::with_capacity(seg);
    for i in 0..seg {
        let a = (i as f64) / (seg as f64) * std::f64::consts::TAU;
        poly.push(g.to_device(cx + rx * a.cos(), cy + ry * a.sin()));
    }
    poly
}

/// A rounded-rectangle device polygon with corner radii `rw`/`rh` (logical).
pub fn round_rect_poly(g: &Gdi, l: f64, t: f64, r: f64, b: f64, rw: f64, rh: f64) -> Vec<Pt> {
    let rw = rw.abs().min((r - l).abs() / 2.0);
    let rh = rh.abs().min((b - t).abs() / 2.0);
    if rw <= 0.0 || rh <= 0.0 {
        return rect_poly(g, l, t, r, b);
    }
    const Q: usize = 6;
    let mut pts = Vec::new();
    // Corner centres.
    let corners = [
        (r - rw, t + rh, -std::f64::consts::FRAC_PI_2, 0.0), // top-right
        (r - rw, b - rh, 0.0, std::f64::consts::FRAC_PI_2),  // bottom-right
        (
            l + rw,
            b - rh,
            std::f64::consts::FRAC_PI_2,
            std::f64::consts::PI,
        ), // bottom-left
        (
            l + rw,
            t + rh,
            std::f64::consts::PI,
            std::f64::consts::PI + std::f64::consts::FRAC_PI_2,
        ), // top-left
    ];
    for (ccx, ccy, a0, a1) in corners {
        for i in 0..=Q {
            let a = a0 + (a1 - a0) * (i as f64) / (Q as f64);
            pts.push(g.to_device(ccx + rw * a.cos(), ccy + rh * a.sin()));
        }
    }
    pts
}

/// Points along the elliptical arc inscribed in logical rect `(l,t)-(r,b)` from
/// the ray through `(x1,y1)` to the ray through `(x2,y2)`, counter-clockwise
/// (GDI default), as device points. `seg` controls smoothness.
#[allow(clippy::too_many_arguments)]
pub fn arc_points(
    g: &Gdi,
    l: f64,
    t: f64,
    r: f64,
    b: f64,
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
    seg: usize,
) -> Vec<Pt> {
    let cx = (l + r) / 2.0;
    let cy = (t + b) / 2.0;
    let rx = (r - l).abs() / 2.0;
    let ry = (b - t).abs() / 2.0;
    if rx <= 0.0 || ry <= 0.0 {
        return Vec::new();
    }
    let a_start = (y1 - cy).atan2(x1 - cx);
    let mut a_end = (y2 - cy).atan2(x2 - cx);
    // GDI arcs go counter-clockwise in logical space.
    while a_end <= a_start {
        a_end += std::f64::consts::TAU;
    }
    let sweep = a_end - a_start;
    let n = seg.max(2);
    let mut pts = Vec::with_capacity(n + 1);
    for i in 0..=n {
        let a = a_start + sweep * (i as f64) / (n as f64);
        pts.push(g.to_device(cx + rx * a.cos(), cy + ry * a.sin()));
    }
    pts
}

// ───────────────────────────── DIB / BMP decoder ─────────────────────────────

/// A decoded device-independent bitmap: RGBA8 rows, top-to-bottom.
#[derive(Debug, Clone)]
pub struct Dib {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Dib {
    pub fn at(&self, x: u32, y: u32) -> Rgba {
        if x >= self.width || y >= self.height {
            return Rgba::TRANSPARENT;
        }
        let i = ((y * self.width + x) * 4) as usize;
        Rgba {
            r: self.rgba[i],
            g: self.rgba[i + 1],
            b: self.rgba[i + 2],
            a: self.rgba[i + 3],
        }
    }
}

/// Decode a **packed DIB** (BITMAPINFOHEADER followed by palette + pixel bits,
/// the form carried by WMF `StretchDIBits`/`SetDIBitsToDevice` and EMF blits).
/// Supports 1/4/8/24/32 bpp + BI_RGB and RLE4/RLE8. Returns `None` on malformed
/// input (never panics).
pub fn decode_packed_dib(data: &[u8]) -> Option<Dib> {
    if data.len() < 4 {
        return None;
    }
    let header_size = rd_u32(data, 0)? as usize;
    // Only the modern BITMAPINFOHEADER family (≥ 40). Older BITMAPCOREHEADER (12)
    // is rare in metafiles; reject rather than misparse.
    if header_size < 40 || header_size > data.len() {
        return None;
    }
    let width = rd_i32(data, 4)?;
    let height_raw = rd_i32(data, 8)?;
    let bit_count = rd_u16(data, 14)? as u32;
    let compression = rd_u32(data, 16)?;
    if width <= 0 || width > 1 << 16 {
        return None;
    }
    let top_down = height_raw < 0;
    let height = height_raw.unsigned_abs();
    if height == 0 || height > 1 << 16 {
        return None;
    }
    let w = width as u32;
    let h = height;

    let mut clr_used = rd_u32(data, 32).unwrap_or(0);
    if clr_used == 0 && bit_count <= 8 {
        clr_used = 1u32 << bit_count;
    }

    // Palette (BGRA quads) sits right after the header for ≤8 bpp.
    let palette_off = header_size;
    let palette_len = if bit_count <= 8 { clr_used as usize } else { 0 };
    let mut palette: Vec<Rgba> = Vec::with_capacity(palette_len);
    for i in 0..palette_len {
        let o = palette_off + i * 4;
        if o + 4 > data.len() {
            // Truncated palette — pad with black so indices stay valid.
            palette.push(Rgba::rgb(0, 0, 0));
            continue;
        }
        palette.push(Rgba::rgb(data[o + 2], data[o + 1], data[o]));
    }

    let bits_off = palette_off + palette_len * 4;
    if bits_off > data.len() {
        return None;
    }
    let bits = &data[bits_off..];

    let mut out = vec![0u8; (w as usize) * (h as usize) * 4];
    let store = |out: &mut [u8], x: u32, row: u32, c: Rgba| {
        // `row` is bottom-up unless top_down; flip to top-down storage.
        let y = if top_down { row } else { h - 1 - row };
        if x < w && y < h {
            let i = ((y * w + x) * 4) as usize;
            out[i] = c.r;
            out[i + 1] = c.g;
            out[i + 2] = c.b;
            out[i + 3] = c.a;
        }
    };

    match (bit_count, compression) {
        // BI_RGB uncompressed.
        (1, 0) | (4, 0) | (8, 0) => {
            let row_bytes = ((w as usize) * bit_count as usize).div_ceil(32) * 4;
            for row in 0..h {
                let ro = (row as usize) * row_bytes;
                if ro + row_bytes > bits.len() {
                    break;
                }
                let line = &bits[ro..ro + row_bytes];
                for x in 0..w {
                    let idx = read_indexed(line, x, bit_count);
                    let c = *palette.get(idx as usize).unwrap_or(&Rgba::rgb(0, 0, 0));
                    store(&mut out, x, row, c);
                }
            }
        }
        (24, 0) => {
            let row_bytes = (w as usize * 3).div_ceil(4) * 4;
            for row in 0..h {
                let ro = (row as usize) * row_bytes;
                if ro + row_bytes > bits.len() {
                    break;
                }
                for x in 0..w {
                    let p = ro + (x as usize) * 3;
                    let c = Rgba::rgb(bits[p + 2], bits[p + 1], bits[p]);
                    store(&mut out, x, row, c);
                }
            }
        }
        (32, 0) => {
            let row_bytes = w as usize * 4;
            for row in 0..h {
                let ro = (row as usize) * row_bytes;
                if ro + row_bytes > bits.len() {
                    break;
                }
                for x in 0..w {
                    let p = ro + (x as usize) * 4;
                    // BI_RGB 32bpp: bytes are B,G,R,X. Alpha byte is undefined →
                    // treat as opaque (the common DIB convention).
                    let c = Rgba::rgb(bits[p + 2], bits[p + 1], bits[p]);
                    store(&mut out, x, row, c);
                }
            }
        }
        // BI_RLE8 / BI_RLE4.
        (8, 1) => decode_rle(bits, w, h, &palette, false, top_down, &mut out),
        (4, 2) => decode_rle(bits, w, h, &palette, true, top_down, &mut out),
        _ => return None,
    }

    Some(Dib {
        width: w,
        height: h,
        rgba: out,
    })
}

/// Read the `bit_count`-bpp palette index of pixel `x` from a row `line`.
fn read_indexed(line: &[u8], x: u32, bit_count: u32) -> u32 {
    match bit_count {
        1 => {
            let byte = line.get((x / 8) as usize).copied().unwrap_or(0);
            ((byte >> (7 - (x % 8))) & 1) as u32
        }
        4 => {
            let byte = line.get((x / 2) as usize).copied().unwrap_or(0);
            if x.is_multiple_of(2) {
                (byte >> 4) as u32
            } else {
                (byte & 0x0F) as u32
            }
        }
        8 => line.get(x as usize).copied().unwrap_or(0) as u32,
        _ => 0,
    }
}

/// Decode an RLE8 (or RLE4 if `rle4`) bitstream into `out`. Bounds-checked; on a
/// malformed run it stops cleanly (partial image rather than panic).
fn decode_rle(
    bits: &[u8],
    w: u32,
    h: u32,
    palette: &[Rgba],
    rle4: bool,
    top_down: bool,
    out: &mut [u8],
) {
    let mut x = 0u32;
    let mut row = 0u32;
    let mut i = 0usize;
    let put = |out: &mut [u8], x: u32, row: u32, idx: u32| {
        let c = *palette.get(idx as usize).unwrap_or(&Rgba::rgb(0, 0, 0));
        let y = if top_down {
            row
        } else {
            h.saturating_sub(1).wrapping_sub(row)
        };
        if x < w && y < h && row < h {
            let p = ((y * w + x) * 4) as usize;
            out[p] = c.r;
            out[p + 1] = c.g;
            out[p + 2] = c.b;
            out[p + 3] = c.a;
        }
    };
    while i + 1 < bits.len() {
        let count = bits[i];
        let val = bits[i + 1];
        i += 2;
        if count > 0 {
            // Encoded run of `count` pixels of index(es) in `val`.
            for k in 0..count as u32 {
                let idx = if rle4 {
                    if k.is_multiple_of(2) {
                        (val >> 4) as u32
                    } else {
                        (val & 0x0F) as u32
                    }
                } else {
                    val as u32
                };
                put(out, x, row, idx);
                x += 1;
            }
        } else {
            // Escape.
            match val {
                0 => {
                    // End of line.
                    x = 0;
                    row += 1;
                }
                1 => break, // End of bitmap.
                2 => {
                    // Delta: next two bytes = dx, dy.
                    if i + 1 < bits.len() {
                        x = x.saturating_add(bits[i] as u32);
                        row = row.saturating_add(bits[i + 1] as u32);
                        i += 2;
                    } else {
                        break;
                    }
                }
                n => {
                    // Absolute run of `n` literal pixels.
                    let n = n as u32;
                    if rle4 {
                        let nbytes = n.div_ceil(2) as usize;
                        for k in 0..n {
                            let b = bits.get(i + (k / 2) as usize).copied().unwrap_or(0);
                            let idx = if k.is_multiple_of(2) {
                                (b >> 4) as u32
                            } else {
                                (b & 0x0F) as u32
                            };
                            put(out, x, row, idx);
                            x += 1;
                        }
                        i += nbytes;
                        if !nbytes.is_multiple_of(2) {
                            i += 1; // word alignment padding
                        }
                    } else {
                        for k in 0..n {
                            let idx = bits.get(i + k as usize).copied().unwrap_or(0) as u32;
                            put(out, x, row, idx);
                            x += 1;
                        }
                        i += n as usize;
                        if !(n as usize).is_multiple_of(2) {
                            i += 1; // word alignment padding
                        }
                    }
                }
            }
        }
    }
}

/// Blit a decoded `dib` into the device rectangle `(dx,dy)`–`(dx+dw,dy+dh)`
/// (device pixels), nearest-neighbour scaled, clipped to the canvas and any
/// active clip rectangle. Used by all the StretchDIBits/StretchBlt records.
#[allow(clippy::too_many_arguments)]
pub fn blit_dib(
    canvas: &mut Canvas,
    dib: &Dib,
    dx: f64,
    dy: f64,
    dw: f64,
    dh: f64,
    clip: Option<&ClipRect>,
) {
    if dib.width == 0 || dib.height == 0 || dw.abs() < 1e-6 || dh.abs() < 1e-6 {
        return;
    }
    let x0 = dx.min(dx + dw).floor() as i32;
    let x1 = dx.max(dx + dw).ceil() as i32;
    let y0 = dy.min(dy + dh).floor() as i32;
    let y1 = dy.max(dy + dh).ceil() as i32;
    let flip_x = dw < 0.0;
    let flip_y = dh < 0.0;
    for py in y0..y1 {
        for px in x0..x1 {
            if let Some(c) = clip {
                if !c.contains(px, py) {
                    continue;
                }
            }
            // Map device pixel back to source texel.
            let mut u = (px as f64 + 0.5 - dx.min(dx + dw)) / dw.abs();
            let mut v = (py as f64 + 0.5 - dy.min(dy + dh)) / dh.abs();
            if flip_x {
                u = 1.0 - u;
            }
            if flip_y {
                v = 1.0 - v;
            }
            if !(0.0..1.0).contains(&u) || !(0.0..1.0).contains(&v) {
                continue;
            }
            let sx = (u * dib.width as f64).floor() as u32;
            let sy = (v * dib.height as f64).floor() as u32;
            let texel = dib.at(sx.min(dib.width - 1), sy.min(dib.height - 1));
            canvas.put(px, py, texel);
        }
    }
}

// ───────────────────────────── little-endian readers ─────────────────────────

pub fn rd_u16(b: &[u8], o: usize) -> Option<u16> {
    b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]))
}

pub fn rd_i16(b: &[u8], o: usize) -> Option<i16> {
    rd_u16(b, o).map(|v| v as i16)
}

pub fn rd_u32(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

pub fn rd_i32(b: &[u8], o: usize) -> Option<i32> {
    rd_u32(b, o).map(|v| v as i32)
}
