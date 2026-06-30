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

// ───────────────────────────── raster op (ROP2) ─────────────────────────────

/// Binary raster operations (`SetROP2` mix modes). GDI combines the *pen/brush*
/// colour `S` with the existing *destination* colour `D` per pixel; only the
/// 16 binary ops apply to vector drawing (the ternary ROPs are for BitBlt).
/// Channels are mixed independently as 8-bit values; `R2_COPYPEN` is the default
/// "just paint the source" behaviour the rest of this module assumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rop2 {
    Black,       // R2_BLACK (1): 0
    NotMergePen, // R2_NOTMERGEPEN (2): !(S | D)
    MaskNotPen,  // R2_MASKNOTPEN (3): D & !S
    NotCopyPen,  // R2_NOTCOPYPEN (4): !S
    MaskPenNot,  // R2_MASKPENNOT (5): S & !D
    Not,         // R2_NOT (6): !D
    XorPen,      // R2_XORPEN (7): S ^ D
    NotMaskPen,  // R2_NOTMASKPEN (8): !(S & D)
    MaskPen,     // R2_MASKPEN (9): S & D
    NotXorPen,   // R2_NOTXORPEN (10): !(S ^ D)
    Nop,         // R2_NOP (11): D
    MergeNotPen, // R2_MERGENOTPEN (12): D | !S
    CopyPen,     // R2_COPYPEN (13): S  (default)
    MergePenNot, // R2_MERGEPENNOT (14): S | !D
    MergePen,    // R2_MERGEPEN (15): S | D
    White,       // R2_WHITE (16): 1
}

impl Rop2 {
    /// Map a GDI `R2_*` constant (1..=16) to a [`Rop2`]; unknown ⇒ `CopyPen`.
    pub fn from_u32(v: u32) -> Rop2 {
        match v {
            1 => Rop2::Black,
            2 => Rop2::NotMergePen,
            3 => Rop2::MaskNotPen,
            4 => Rop2::NotCopyPen,
            5 => Rop2::MaskPenNot,
            6 => Rop2::Not,
            7 => Rop2::XorPen,
            8 => Rop2::NotMaskPen,
            9 => Rop2::MaskPen,
            10 => Rop2::NotXorPen,
            11 => Rop2::Nop,
            12 => Rop2::MergeNotPen,
            14 => Rop2::MergePenNot,
            15 => Rop2::MergePen,
            16 => Rop2::White,
            _ => Rop2::CopyPen, // 13 and anything unexpected
        }
    }

    /// `true` when this op ignores the source entirely (`R2_NOP` leaves the
    /// destination untouched). Such ops don't anti-alias meaningfully, so the
    /// rasterizer can short-circuit a `R2_NOP` fill/stroke as a no-op.
    pub fn is_nop(self) -> bool {
        matches!(self, Rop2::Nop)
    }

    /// Combine one 8-bit source channel `s` with the destination channel `d`
    /// per this binary op (bit-for-bit, as GDI mixes raster ops).
    fn mix_channel(self, s: u8, d: u8) -> u8 {
        match self {
            Rop2::Black => 0,
            Rop2::NotMergePen => !(s | d),
            Rop2::MaskNotPen => d & !s,
            Rop2::NotCopyPen => !s,
            Rop2::MaskPenNot => s & !d,
            Rop2::Not => !d,
            Rop2::XorPen => s ^ d,
            Rop2::NotMaskPen => !(s & d),
            Rop2::MaskPen => s & d,
            Rop2::NotXorPen => !(s ^ d),
            Rop2::Nop => d,
            Rop2::MergeNotPen => d | !s,
            Rop2::CopyPen => s,
            Rop2::MergePenNot => s | !d,
            Rop2::MergePen => s | d,
            Rop2::White => 0xFF,
        }
    }
}

// ───────────────────────────── canvas ─────────────────────────────

/// A transparent RGBA8 framebuffer (row-major, top-to-bottom) with source-over
/// compositing. The active binary raster op (`rop2`) governs how a painted
/// source colour is mixed into the destination before alpha compositing.
#[derive(Debug, Clone)]
pub struct Canvas {
    pub width: u32,
    pub height: u32,
    /// `width*height*4` bytes, RGBA, premultiplied-free straight alpha.
    pub pixels: Vec<u8>,
    /// The binary raster op applied to vector paint (`SetROP2`).
    pub rop2: Rop2,
}

impl Canvas {
    /// A fully transparent canvas of `width` × `height` (default `R2_COPYPEN`).
    pub fn new(width: u32, height: u32) -> Canvas {
        Canvas {
            width,
            height,
            pixels: vec![0u8; (width as usize) * (height as usize) * 4],
            rop2: Rop2::CopyPen,
        }
    }

    /// Composite straight-alpha `src` over pixel `(x, y)` scaled by coverage
    /// `cov` (`0.0..=1.0`). Out-of-bounds and zero-coverage are no-ops. When the
    /// active `rop2` is not the default `R2_COPYPEN`, the source RGB is first
    /// mixed with the existing destination RGB through the raster op, and the
    /// result is composited at the source's alpha·coverage (so `R2_NOT` /
    /// `R2_XORPEN` / … draw their mixed colour with proper edge anti-aliasing).
    pub fn blend(&mut self, x: i32, y: i32, src: Rgba, cov: f64) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        if self.rop2.is_nop() {
            return;
        }
        let sa = (src.a as f64 / 255.0) * cov.clamp(0.0, 1.0);
        if sa <= 0.0 {
            return;
        }
        let idx = ((y as usize) * (self.width as usize) + x as usize) * 4;
        // Apply the binary raster op to the RGB channels against the current
        // destination, yielding the effective source colour for this pixel.
        let src = if self.rop2 == Rop2::CopyPen {
            src
        } else {
            Rgba {
                r: self.rop2.mix_channel(src.r, self.pixels[idx]),
                g: self.rop2.mix_channel(src.g, self.pixels[idx + 1]),
                b: self.rop2.mix_channel(src.b, self.pixels[idx + 2]),
                a: src.a,
            }
        };
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

/// A polygon interior as a per-pixel coverage mask over its device bounding box.
/// Used to clip hatch lines and pattern tiles to the exact shape outline.
struct PolyMask {
    x0: i32,
    y0: i32,
    w: usize,
    h: usize,
    cov: Vec<f64>,
}

impl PolyMask {
    /// Coverage `0.0..=1.0` at device pixel `(px, py)`, `0.0` outside the box.
    fn coverage(&self, px: i32, py: i32) -> f64 {
        if px < self.x0 || py < self.y0 {
            return 0.0;
        }
        let ix = (px - self.x0) as usize;
        let iy = (py - self.y0) as usize;
        if ix >= self.w || iy >= self.h {
            return 0.0;
        }
        self.cov[iy * self.w + ix]
    }
}

/// Build the interior coverage mask of `polys` (device pixels) under the GDI
/// fill rule, bounded to `width`×`height`. Returns `None` when the polygons
/// don't cover any pixel.
fn build_poly_mask(
    polys: &[Vec<Pt>],
    width: u32,
    height: u32,
    alternate: bool,
) -> Option<PolyMask> {
    let mut edges: Vec<Edge> = Vec::new();
    let (mut min_x, mut min_y) = (f64::INFINITY, f64::INFINITY);
    let (mut max_x, mut max_y) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
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
            min_x = min_x.min(a.x);
            min_y = min_y.min(a.y);
            max_x = max_x.max(a.x);
            max_y = max_y.max(a.y);
        }
    }
    if edges.is_empty() || !min_x.is_finite() {
        return None;
    }
    let x0 = (min_x.floor().max(0.0)) as i32;
    let y0 = (min_y.floor().max(0.0)) as i32;
    let x1 = (max_x.ceil().min(width as f64)) as i32;
    let y1 = (max_y.ceil().min(height as f64)) as i32;
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let w = (x1 - x0) as usize;
    let h = (y1 - y0) as usize;
    let mut cov = vec![0.0f64; w * h];
    rasterize_coverage(&edges, width, height, alternate, &mut |px, py, c| {
        let ix = px - x0;
        let iy = py - y0;
        if ix >= 0 && iy >= 0 && (ix as usize) < w && (iy as usize) < h {
            cov[(iy as usize) * w + ix as usize] = c;
        }
    });
    Some(PolyMask { x0, y0, w, h, cov })
}

/// Fill `polys` with a GDI hatch brush: the shape interior stays transparent and
/// the `HS_*` line pattern is painted in `color`, clipped to the shape's
/// coverage mask (anti-aliased where the pattern meets the outline). The hatch
/// pitch and line thickness are fixed device-pixel cosmetics matching GDI's look.
fn fill_hatch(
    canvas: &mut Canvas,
    polys: &[Vec<Pt>],
    color: Rgba,
    hatch: HatchStyle,
    alternate: bool,
    clip: Option<&ClipRect>,
) {
    if color.a == 0 {
        return;
    }
    let Some(mask) = build_poly_mask(polys, canvas.width, canvas.height, alternate) else {
        return;
    };
    // Pitch: 8 device px between lines, ~1 px line — the classic GDI hatch.
    const PITCH: i32 = 8;
    let x0 = mask.x0;
    let y0 = mask.y0;
    let x1 = x0 + mask.w as i32;
    let y1 = y0 + mask.h as i32;
    let horiz = matches!(hatch, HatchStyle::Horizontal | HatchStyle::Cross);
    let vert = matches!(hatch, HatchStyle::Vertical | HatchStyle::Cross);
    let fdiag = matches!(hatch, HatchStyle::FDiagonal | HatchStyle::DiagCross);
    let bdiag = matches!(hatch, HatchStyle::BDiagonal | HatchStyle::DiagCross);
    for py in y0..y1 {
        for px in x0..x1 {
            // A pixel lies on the hatch when it sits on any enabled line family.
            let on_line = (horiz && py.rem_euclid(PITCH) == 0)
                || (vert && px.rem_euclid(PITCH) == 0)
                || (fdiag && (px - py).rem_euclid(PITCH) == 0) // "╲"
                || (bdiag && (px + py).rem_euclid(PITCH) == 0); // "╱"
            if !on_line {
                continue;
            }
            let cov = mask.coverage(px, py);
            if cov <= 0.0 {
                continue;
            }
            if let Some(c) = clip {
                if !c.contains(px, py) {
                    continue;
                }
            }
            canvas.blend(px, py, color, cov);
        }
    }
}

/// Fill `polys` with a GDI pattern brush: the decoded `tile` DIB is tiled across
/// the shape interior (origin at the device 0,0 grid), clipped to the shape's
/// coverage mask. Each source texel composites at the mask coverage so the
/// pattern's edge follows the outline.
fn fill_pattern(
    canvas: &mut Canvas,
    polys: &[Vec<Pt>],
    tile: &Dib,
    alternate: bool,
    clip: Option<&ClipRect>,
) {
    if tile.width == 0 || tile.height == 0 {
        return;
    }
    let Some(mask) = build_poly_mask(polys, canvas.width, canvas.height, alternate) else {
        return;
    };
    let x0 = mask.x0;
    let y0 = mask.y0;
    let x1 = x0 + mask.w as i32;
    let y1 = y0 + mask.h as i32;
    for py in y0..y1 {
        for px in x0..x1 {
            let cov = mask.coverage(px, py);
            if cov <= 0.0 {
                continue;
            }
            if let Some(c) = clip {
                if !c.contains(px, py) {
                    continue;
                }
            }
            // Tile the pattern against the device-pixel origin.
            let sx = px.rem_euclid(tile.width as i32) as u32;
            let sy = py.rem_euclid(tile.height as i32) as u32;
            canvas.blend(px, py, tile.at(sx, sy), cov);
        }
    }
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

/// Brush fill styles (`BS_*`). `Hatched` carries one of the six `HS_*` line
/// patterns (painted as a real scanline overlay); `Pattern` carries an optional
/// decoded DIB tiled across the fill (falling back to a mid-grey solid when the
/// pattern bits couldn't be decoded).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrushStyle {
    Solid,
    Null,
    Hatched,
    Pattern,
}

/// The six GDI hatch line patterns (`HS_*`), drawn as a thin-line overlay in the
/// brush colour over the (transparent) shape interior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HatchStyle {
    Horizontal, // HS_HORIZONTAL (0): ─────
    Vertical,   // HS_VERTICAL (1):   │││││
    FDiagonal,  // HS_FDIAGONAL (2):  ╲╲╲╲╲ (top-left → bottom-right)
    BDiagonal,  // HS_BDIAGONAL (3):  ╱╱╱╱╱ (bottom-left → top-right)
    Cross,      // HS_CROSS (4):      ┼┼┼┼┼
    DiagCross,  // HS_DIAGCROSS (5):  ╳╳╳╳╳
}

impl HatchStyle {
    /// Map a GDI `HS_*` constant to a [`HatchStyle`]; unknown ⇒ `Horizontal`.
    pub fn from_u32(v: u32) -> HatchStyle {
        match v {
            0 => HatchStyle::Horizontal,
            1 => HatchStyle::Vertical,
            2 => HatchStyle::FDiagonal,
            3 => HatchStyle::BDiagonal,
            4 => HatchStyle::Cross,
            5 => HatchStyle::DiagCross,
            _ => HatchStyle::Horizontal,
        }
    }
}

/// A GDI brush: style + colour, with the hatch pattern (for `Hatched`) and an
/// optional decoded tile (for `Pattern`). Cloneable but not `Copy` because the
/// pattern tile owns a pixel buffer.
#[derive(Debug, Clone)]
pub struct Brush {
    pub style: BrushStyle,
    pub color: Rgba,
    /// The `HS_*` line pattern when `style == Hatched`.
    pub hatch: HatchStyle,
    /// The decoded pattern tile when `style == Pattern` and the DIB was readable.
    pub pattern: Option<Dib>,
}

impl Brush {
    /// A solid brush of `color`.
    pub fn solid(color: Rgba) -> Brush {
        Brush {
            style: BrushStyle::Solid,
            color,
            hatch: HatchStyle::Horizontal,
            pattern: None,
        }
    }

    /// A hatched brush of `color` with line pattern `hatch`.
    pub fn hatched(color: Rgba, hatch: HatchStyle) -> Brush {
        Brush {
            style: BrushStyle::Hatched,
            color,
            hatch,
            pattern: None,
        }
    }

    /// A pattern brush from a decoded tile (or a mid-grey solid fallback when
    /// `tile` is `None`).
    pub fn pattern(tile: Option<Dib>) -> Brush {
        Brush {
            style: BrushStyle::Pattern,
            color: Rgba::rgb(128, 128, 128),
            hatch: HatchStyle::Horizontal,
            pattern: tile,
        }
    }

    pub fn white() -> Brush {
        Brush::solid(Rgba::rgb(255, 255, 255))
    }

    pub fn null() -> Brush {
        Brush {
            style: BrushStyle::Null,
            color: Rgba::TRANSPARENT,
            hatch: HatchStyle::Horizontal,
            pattern: None,
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
    /// A region as its full list of disjoint rectangles (logical units). A
    /// simple region is one rectangle; a complex region keeps every scan
    /// rectangle so its **union** (not just the bounding box) can be filled.
    Region(Vec<LogRect>),
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

impl LogRect {
    /// The smallest rectangle covering both `self` and `other`.
    pub fn union(&self, other: &LogRect) -> LogRect {
        LogRect {
            left: self.left.min(other.left),
            top: self.top.min(other.top),
            right: self.right.max(other.right),
            bottom: self.bottom.max(other.bottom),
        }
    }
}

/// The bounding rectangle of a list of region rectangles (logical units), or
/// `None` for an empty region.
pub fn region_bbox(rects: &[LogRect]) -> Option<LogRect> {
    let mut it = rects.iter();
    let first = *it.next()?;
    Some(it.fold(first, |acc, r| acc.union(r)))
}

/// DIB stretch interpolation (`SetStretchBltMode`): `Nearest` for the COLORONCOLOR
/// / "delete excess lines" modes, `Bilinear` for HALFTONE (smooth resampling).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StretchMode {
    Nearest,
    Bilinear,
}

impl StretchMode {
    /// Map a GDI `SetStretchBltMode` constant to a [`StretchMode`]:
    /// `BLACKONWHITE`/`WHITEONBLACK`/`COLORONCOLOR` (1/2/3) → nearest;
    /// `HALFTONE` (4) → bilinear; unknown ⇒ nearest.
    pub fn from_u32(v: u32) -> StretchMode {
        match v {
            4 => StretchMode::Bilinear, // HALFTONE / STRETCH_HALFTONE
            _ => StretchMode::Nearest,
        }
    }
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
    /// DIB stretch interpolation (`SetStretchBltMode`).
    pub stretch_mode: StretchMode,
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
            stretch_mode: StretchMode::Nearest,
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

    /// Set the active clip rectangle to the device-space bounds of a **logical**
    /// rectangle, mapped through the current transform. GDI `SelectClipRgn` /
    /// `ExtSelectClipRgn` with `RGN_COPY`: the new clip *replaces* any prior clip.
    /// All four logical corners are transformed so a rotated/flipped world
    /// transform still yields a correct axis-aligned device clip. A `None`
    /// rectangle clears the clip (no clipping).
    pub fn set_clip_logrect(&mut self, rect: Option<LogRect>) {
        self.clip = rect.map(|r| self.logrect_to_cliprect(r));
    }

    /// Map a logical rectangle to a device-pixel [`ClipRect`] (axis-aligned
    /// bounds of its four transformed corners), clamped to the canvas.
    pub fn logrect_to_cliprect(&self, r: LogRect) -> ClipRect {
        let corners = [
            self.to_device(r.left, r.top),
            self.to_device(r.right, r.top),
            self.to_device(r.right, r.bottom),
            self.to_device(r.left, r.bottom),
        ];
        let mut min_x = f64::INFINITY;
        let mut min_y = f64::INFINITY;
        let mut max_x = f64::NEG_INFINITY;
        let mut max_y = f64::NEG_INFINITY;
        for c in corners {
            min_x = min_x.min(c.x);
            min_y = min_y.min(c.y);
            max_x = max_x.max(c.x);
            max_y = max_y.max(c.y);
        }
        let cw = self.canvas.width as i32;
        let ch = self.canvas.height as i32;
        ClipRect {
            left: (min_x.floor() as i32).clamp(0, cw),
            top: (min_y.floor() as i32).clamp(0, ch),
            right: (max_x.ceil() as i32).clamp(0, cw),
            bottom: (max_y.ceil() as i32).clamp(0, ch),
        }
    }

    /// Fill `polys` (logical) with the current brush, then stroke their outline
    /// with the current pen — the shape primitive shared by Rectangle / Ellipse /
    /// Polygon / Pie / Chord.
    pub fn fill_and_stroke(&mut self, polys: &[Vec<Pt>]) {
        self.fill_with_current_brush(polys);
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

    /// Fill `polys` (device pixels) with the current brush, honouring its style:
    /// solid → flat colour; hatched → the `HS_*` line overlay; pattern → the
    /// tiled DIB (mid-grey fallback). `Null` paints nothing.
    pub fn fill_with_current_brush(&mut self, polys: &[Vec<Pt>]) {
        match self.cur_brush.style {
            BrushStyle::Null => {}
            BrushStyle::Solid => fill_polygons(
                &mut self.canvas,
                polys,
                self.cur_brush.color,
                self.poly_fill_alternate,
                self.clip.as_ref(),
            ),
            BrushStyle::Hatched => {
                let color = self.cur_brush.color;
                let hatch = self.cur_brush.hatch;
                fill_hatch(
                    &mut self.canvas,
                    polys,
                    color,
                    hatch,
                    self.poly_fill_alternate,
                    self.clip.as_ref(),
                );
            }
            BrushStyle::Pattern => {
                if let Some(tile) = self.cur_brush.pattern.clone() {
                    fill_pattern(
                        &mut self.canvas,
                        polys,
                        &tile,
                        self.poly_fill_alternate,
                        self.clip.as_ref(),
                    );
                } else {
                    // No decodable tile → mid-grey solid (legacy fallback).
                    fill_polygons(
                        &mut self.canvas,
                        polys,
                        self.cur_brush.color,
                        self.poly_fill_alternate,
                        self.clip.as_ref(),
                    );
                }
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

    /// Bilinearly sample the source at continuous coordinates `(u, v)` in texel
    /// units (texel centres at `+0.5`), clamping to the edge. Used by the
    /// HALFTONE stretch mode for smooth up/down-scaling.
    pub fn sample_bilinear(&self, u: f64, v: f64) -> Rgba {
        if self.width == 0 || self.height == 0 {
            return Rgba::TRANSPARENT;
        }
        // Texel-centre convention: sample point (u,v) sits between integer texels.
        let fx = (u - 0.5).clamp(0.0, (self.width - 1) as f64);
        let fy = (v - 0.5).clamp(0.0, (self.height - 1) as f64);
        let x0 = fx.floor() as u32;
        let y0 = fy.floor() as u32;
        let x1 = (x0 + 1).min(self.width - 1);
        let y1 = (y0 + 1).min(self.height - 1);
        let tx = fx - x0 as f64;
        let ty = fy - y0 as f64;
        let c00 = self.at(x0, y0);
        let c10 = self.at(x1, y0);
        let c01 = self.at(x0, y1);
        let c11 = self.at(x1, y1);
        let lerp = |a: u8, b: u8, t: f64| a as f64 * (1.0 - t) + b as f64 * t;
        let mix = |a: u8, b: u8, c: u8, d: u8| -> u8 {
            let top = lerp(a, b, tx);
            let bot = lerp(c, d, tx);
            (top * (1.0 - ty) + bot * ty).round().clamp(0.0, 255.0) as u8
        };
        Rgba {
            r: mix(c00.r, c10.r, c01.r, c11.r),
            g: mix(c00.g, c10.g, c01.g, c11.g),
            b: mix(c00.b, c10.b, c01.b, c11.b),
            a: mix(c00.a, c10.a, c01.a, c11.a),
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
/// (device pixels), clipped to the canvas and any active clip rectangle. `mode`
/// selects the resampling kernel — [`StretchMode::Nearest`] (the default
/// COLORONCOLOR/“delete excess lines” behaviour) or [`StretchMode::Bilinear`]
/// (HALFTONE smooth scaling, `SetStretchBltMode`). Used by all the
/// StretchDIBits/StretchBlt records.
#[allow(clippy::too_many_arguments)]
pub fn blit_dib(
    canvas: &mut Canvas,
    dib: &Dib,
    dx: f64,
    dy: f64,
    dw: f64,
    dh: f64,
    clip: Option<&ClipRect>,
    mode: StretchMode,
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
            // Map device pixel back to normalized source coordinate (0..1).
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
            let texel = match mode {
                StretchMode::Nearest => {
                    let sx = (u * dib.width as f64).floor() as u32;
                    let sy = (v * dib.height as f64).floor() as u32;
                    dib.at(sx.min(dib.width - 1), sy.min(dib.height - 1))
                }
                StretchMode::Bilinear => {
                    // Continuous texel coordinate (texel centres at +0.5).
                    dib.sample_bilinear(u * dib.width as f64, v * dib.height as f64)
                }
            };
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A device-space rectangle polygon (CW), for fills/masks in tests.
    fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<Pt> {
        vec![
            Pt { x: x0, y: y0 },
            Pt { x: x1, y: y0 },
            Pt { x: x1, y: y1 },
            Pt { x: x0, y: y1 },
        ]
    }

    fn px(c: &Canvas, x: i32, y: i32) -> Rgba {
        let i = ((y as usize) * (c.width as usize) + x as usize) * 4;
        Rgba {
            r: c.pixels[i],
            g: c.pixels[i + 1],
            b: c.pixels[i + 2],
            a: c.pixels[i + 3],
        }
    }

    // ── ROP2 (#176/#180) ─────────────────────────────────────────────────────

    #[test]
    fn rop2_from_u32_maps_known_modes() {
        assert_eq!(Rop2::from_u32(1), Rop2::Black);
        assert_eq!(Rop2::from_u32(6), Rop2::Not);
        assert_eq!(Rop2::from_u32(7), Rop2::XorPen);
        assert_eq!(Rop2::from_u32(9), Rop2::MaskPen);
        assert_eq!(Rop2::from_u32(11), Rop2::Nop);
        assert_eq!(Rop2::from_u32(13), Rop2::CopyPen);
        assert_eq!(Rop2::from_u32(16), Rop2::White);
        // Unknown → CopyPen.
        assert_eq!(Rop2::from_u32(999), Rop2::CopyPen);
    }

    #[test]
    fn rop2_black_white_force_constant_colour() {
        // R2_BLACK paints black regardless of source.
        let mut c = Canvas::new(4, 4);
        c.rop2 = Rop2::Black;
        c.blend(1, 1, Rgba::rgb(200, 100, 50), 1.0);
        assert_eq!(px(&c, 1, 1), Rgba::rgb(0, 0, 0));
        // R2_WHITE paints white regardless of source.
        let mut c = Canvas::new(4, 4);
        c.rop2 = Rop2::White;
        c.blend(1, 1, Rgba::rgb(10, 20, 30), 1.0);
        assert_eq!(px(&c, 1, 1), Rgba::rgb(255, 255, 255));
    }

    #[test]
    fn rop2_not_inverts_destination() {
        // Start with a known opaque destination, then R2_NOT paints !D.
        let mut c = Canvas::new(2, 2);
        c.put(0, 0, Rgba::rgb(10, 20, 30));
        c.rop2 = Rop2::Not;
        c.blend(0, 0, Rgba::rgb(255, 255, 255), 1.0); // source ignored
        let got = px(&c, 0, 0);
        assert_eq!((got.r, got.g, got.b), (245, 235, 225)); // !10,!20,!30
    }

    #[test]
    fn rop2_xor_combines_source_and_dest() {
        let mut c = Canvas::new(2, 2);
        c.put(0, 0, Rgba::rgb(0xF0, 0x0F, 0xAA));
        c.rop2 = Rop2::XorPen;
        c.blend(0, 0, Rgba::rgb(0x0F, 0xF0, 0x55), 1.0);
        let got = px(&c, 0, 0);
        assert_eq!((got.r, got.g, got.b), (0xFF, 0xFF, 0xFF));
    }

    #[test]
    fn rop2_nop_leaves_pixel_untouched() {
        let mut c = Canvas::new(2, 2);
        c.put(0, 0, Rgba::rgb(7, 8, 9));
        c.rop2 = Rop2::Nop;
        c.blend(0, 0, Rgba::rgb(255, 0, 0), 1.0);
        assert_eq!(px(&c, 0, 0), Rgba::rgb(7, 8, 9));
    }

    #[test]
    fn rop2_maskpen_ands_channels() {
        let mut c = Canvas::new(2, 2);
        c.put(0, 0, Rgba::rgb(0xF0, 0xFF, 0x0F));
        c.rop2 = Rop2::MaskPen; // S & D
        c.blend(0, 0, Rgba::rgb(0x3C, 0x0F, 0xFF), 1.0);
        let got = px(&c, 0, 0);
        assert_eq!((got.r, got.g, got.b), (0x30, 0x0F, 0x0F));
    }

    // ── hatch brushes (#177) ─────────────────────────────────────────────────

    #[test]
    fn hatch_style_from_u32() {
        assert_eq!(HatchStyle::from_u32(0), HatchStyle::Horizontal);
        assert_eq!(HatchStyle::from_u32(3), HatchStyle::BDiagonal);
        assert_eq!(HatchStyle::from_u32(5), HatchStyle::DiagCross);
        assert_eq!(HatchStyle::from_u32(42), HatchStyle::Horizontal);
    }

    #[test]
    fn horizontal_hatch_paints_lines_not_solid() {
        // 32×32 fill; horizontal hatch should paint rows 0,8,16,24 only.
        let mut c = Canvas::new(32, 32);
        fill_hatch(
            &mut c,
            &[rect(0.0, 0.0, 32.0, 32.0)],
            Rgba::rgb(0, 0, 0),
            HatchStyle::Horizontal,
            true,
            None,
        );
        // A hatch row is painted.
        assert!(px(&c, 10, 8).a > 0, "row 8 should carry a hatch line");
        // A non-hatch row stays transparent (the interior is NOT solid-filled).
        assert_eq!(px(&c, 10, 4).a, 0, "row 4 between lines must be empty");
        assert_eq!(px(&c, 10, 12).a, 0, "row 12 between lines must be empty");
    }

    #[test]
    fn cross_hatch_paints_both_axes() {
        let mut c = Canvas::new(24, 24);
        fill_hatch(
            &mut c,
            &[rect(0.0, 0.0, 24.0, 24.0)],
            Rgba::rgb(0, 0, 0),
            HatchStyle::Cross,
            true,
            None,
        );
        assert!(px(&c, 8, 0).a > 0, "vertical line at x=8");
        assert!(px(&c, 0, 8).a > 0, "horizontal line at y=8");
        assert_eq!(px(&c, 3, 3).a, 0, "interior between lines empty");
    }

    #[test]
    fn hatch_is_clipped_to_polygon() {
        // A small triangle: hatch must not paint outside it.
        let mut c = Canvas::new(40, 40);
        let tri = vec![
            Pt { x: 0.0, y: 0.0 },
            Pt { x: 16.0, y: 0.0 },
            Pt { x: 0.0, y: 16.0 },
        ];
        fill_hatch(
            &mut c,
            &[tri],
            Rgba::rgb(0, 0, 0),
            HatchStyle::Horizontal,
            true,
            None,
        );
        // Far corner outside the triangle: never painted.
        assert_eq!(px(&c, 39, 39).a, 0, "outside the triangle stays clear");
    }

    // ── pattern brushes (#177) ───────────────────────────────────────────────

    #[test]
    fn pattern_brush_tiles_decoded_dib() {
        // 2×2 tile: TL red, TR green, BL blue, BR white (top-down RGBA).
        let tile = Dib {
            width: 2,
            height: 2,
            rgba: vec![
                255, 0, 0, 255, // (0,0) red
                0, 255, 0, 255, // (1,0) green
                0, 0, 255, 255, // (0,1) blue
                255, 255, 255, 255, // (1,1) white
            ],
        };
        let mut c = Canvas::new(4, 4);
        fill_pattern(&mut c, &[rect(0.0, 0.0, 4.0, 4.0)], &tile, true, None);
        // The pattern tiles against the device origin: (0,0)=red, (1,0)=green,
        // (0,1)=blue, (2,2)=red again (period 2).
        assert_eq!(px(&c, 0, 0), Rgba::rgb(255, 0, 0));
        assert_eq!(px(&c, 1, 0), Rgba::rgb(0, 255, 0));
        assert_eq!(px(&c, 0, 1), Rgba::rgb(0, 0, 255));
        assert_eq!(px(&c, 2, 2), Rgba::rgb(255, 0, 0));
        assert_eq!(px(&c, 3, 3), Rgba::rgb(255, 255, 255));
    }

    // ── stretch mode / bilinear (#178) ───────────────────────────────────────

    #[test]
    fn stretch_mode_from_u32() {
        assert_eq!(StretchMode::from_u32(1), StretchMode::Nearest);
        assert_eq!(StretchMode::from_u32(3), StretchMode::Nearest);
        assert_eq!(StretchMode::from_u32(4), StretchMode::Bilinear);
        assert_eq!(StretchMode::from_u32(7), StretchMode::Nearest);
    }

    #[test]
    fn bilinear_blends_between_texels_nearest_does_not() {
        // 2×1 source: black | white. Upscale 4× wide.
        let src = Dib {
            width: 2,
            height: 1,
            rgba: vec![0, 0, 0, 255, 255, 255, 255, 255],
        };
        // Nearest: a hard step (only 0 or 255 appear).
        let mut near = Canvas::new(8, 1);
        blit_dib(
            &mut near,
            &src,
            0.0,
            0.0,
            8.0,
            1.0,
            None,
            StretchMode::Nearest,
        );
        for x in 0..8 {
            let v = px(&near, x, 0).r;
            assert!(v == 0 || v == 255, "nearest must be hard-edged, got {v}");
        }
        // Bilinear: intermediate greys appear somewhere across the gradient.
        let mut bil = Canvas::new(8, 1);
        blit_dib(
            &mut bil,
            &src,
            0.0,
            0.0,
            8.0,
            1.0,
            None,
            StretchMode::Bilinear,
        );
        let mid_grey = (0..8).any(|x| {
            let v = px(&bil, x, 0).r;
            v > 0 && v < 255
        });
        assert!(mid_grey, "bilinear should produce intermediate values");
    }

    #[test]
    fn sample_bilinear_clamps_to_edges() {
        let d = Dib {
            width: 2,
            height: 1,
            rgba: vec![0, 0, 0, 255, 255, 255, 255, 255],
        };
        // Far left → first texel (black); far right → last texel (white).
        assert_eq!(d.sample_bilinear(0.0, 0.5).r, 0);
        assert_eq!(d.sample_bilinear(2.0, 0.5).r, 255);
        // Exactly between the two texel centres (x=1.0) → ~mid grey.
        let mid = d.sample_bilinear(1.0, 0.5).r;
        assert!((120..=135).contains(&mid), "midpoint grey ~127, got {mid}");
    }

    // ── region union (#179/#182) ─────────────────────────────────────────────

    #[test]
    fn logrect_union_and_region_bbox() {
        let a = LogRect {
            left: 0.0,
            top: 0.0,
            right: 10.0,
            bottom: 10.0,
        };
        let b = LogRect {
            left: 20.0,
            top: 5.0,
            right: 30.0,
            bottom: 25.0,
        };
        let u = a.union(&b);
        assert_eq!((u.left, u.top, u.right, u.bottom), (0.0, 0.0, 30.0, 25.0));
        let bbox = region_bbox(&[a, b]).unwrap();
        assert_eq!((bbox.left, bbox.right, bbox.bottom), (0.0, 30.0, 25.0));
        assert!(region_bbox(&[]).is_none());
    }

    #[test]
    fn region_union_fills_two_disjoint_rects_not_their_bbox() {
        // Two separated rects: the GAP between them must stay empty (proving we
        // fill the union, not the bounding box).
        let mut c = Canvas::new(40, 12);
        let polys = vec![rect(0.0, 0.0, 10.0, 10.0), rect(30.0, 0.0, 40.0, 10.0)];
        fill_polygons(&mut c, &polys, Rgba::rgb(0, 0, 0), true, None);
        assert!(px(&c, 5, 5).a > 0, "left rect filled");
        assert!(px(&c, 35, 5).a > 0, "right rect filled");
        assert_eq!(px(&c, 20, 5).a, 0, "gap between rects must be empty");
    }

    // ── clip rect from logical rect (#175) ───────────────────────────────────

    #[test]
    fn set_clip_logrect_bounds_paint() {
        let mut g = Gdi::new(20, 20, Affine::identity());
        // Clip to the logical rect (4,4)-(12,12); paint a full-canvas rect.
        g.set_clip_logrect(Some(LogRect {
            left: 4.0,
            top: 4.0,
            right: 12.0,
            bottom: 12.0,
        }));
        let poly = rect(0.0, 0.0, 20.0, 20.0);
        let color = Rgba::rgb(0, 0, 0);
        let clip = g.clip;
        fill_polygons(&mut g.canvas, &[poly], color, true, clip.as_ref());
        assert!(px(&g.canvas, 8, 8).a > 0, "inside the clip painted");
        assert_eq!(px(&g.canvas, 1, 1).a, 0, "outside the clip clipped away");
        assert_eq!(px(&g.canvas, 15, 15).a, 0, "outside the clip clipped away");
        // Clearing the clip removes the bound.
        g.set_clip_logrect(None);
        assert!(g.clip.is_none());
    }

    // ── enum converters: exhaustive arms ─────────────────────────────────────

    #[test]
    fn rop2_from_u32_covers_every_arm() {
        let expect = [
            (1, Rop2::Black),
            (2, Rop2::NotMergePen),
            (3, Rop2::MaskNotPen),
            (4, Rop2::NotCopyPen),
            (5, Rop2::MaskPenNot),
            (6, Rop2::Not),
            (7, Rop2::XorPen),
            (8, Rop2::NotMaskPen),
            (9, Rop2::MaskPen),
            (10, Rop2::NotXorPen),
            (11, Rop2::Nop),
            (12, Rop2::MergeNotPen),
            (13, Rop2::CopyPen),
            (14, Rop2::MergePenNot),
            (15, Rop2::MergePen),
            (16, Rop2::White),
            (0, Rop2::CopyPen),
            (17, Rop2::CopyPen),
        ];
        for (v, want) in expect {
            assert_eq!(Rop2::from_u32(v), want, "from_u32({v})");
        }
        assert!(Rop2::Nop.is_nop());
        assert!(!Rop2::CopyPen.is_nop());
    }

    #[test]
    fn rop2_mix_channel_every_variant() {
        let s = 0b1100_1010u8;
        let d = 0b1010_0110u8;
        let cases = [
            (Rop2::Black, 0u8),
            (Rop2::NotMergePen, !(s | d)),
            (Rop2::MaskNotPen, d & !s),
            (Rop2::NotCopyPen, !s),
            (Rop2::MaskPenNot, s & !d),
            (Rop2::Not, !d),
            (Rop2::XorPen, s ^ d),
            (Rop2::NotMaskPen, !(s & d)),
            (Rop2::MaskPen, s & d),
            (Rop2::NotXorPen, !(s ^ d)),
            (Rop2::Nop, d),
            (Rop2::MergeNotPen, d | !s),
            (Rop2::CopyPen, s),
            (Rop2::MergePenNot, s | !d),
            (Rop2::MergePen, s | d),
            (Rop2::White, 0xFF),
        ];
        for (op, want) in cases {
            assert_eq!(op.mix_channel(s, d), want, "{op:?}");
        }
    }

    #[test]
    fn stretch_mode_from_u32_all_arms() {
        assert_eq!(StretchMode::from_u32(4), StretchMode::Bilinear);
        assert_eq!(StretchMode::from_u32(1), StretchMode::Nearest);
        assert_eq!(StretchMode::from_u32(3), StretchMode::Nearest);
        assert_eq!(StretchMode::from_u32(99), StretchMode::Nearest);
    }

    #[test]
    fn pen_style_from_u32_all_arms() {
        assert_eq!(PenStyle::from_u32(0), PenStyle::Solid);
        assert_eq!(PenStyle::from_u32(1), PenStyle::Dash);
        assert_eq!(PenStyle::from_u32(2), PenStyle::Dot);
        assert_eq!(PenStyle::from_u32(3), PenStyle::DashDot);
        assert_eq!(PenStyle::from_u32(4), PenStyle::DashDotDot);
        assert_eq!(PenStyle::from_u32(5), PenStyle::Null);
        assert_eq!(PenStyle::from_u32(6), PenStyle::InsideFrame);
        assert_eq!(PenStyle::from_u32(7), PenStyle::Solid); // unknown
        assert_eq!(PenStyle::from_u32(0x1_0001), PenStyle::Dash); // masked to low nibble
    }

    #[test]
    fn hatch_style_from_u32_all_arms() {
        assert_eq!(HatchStyle::from_u32(0), HatchStyle::Horizontal);
        assert_eq!(HatchStyle::from_u32(1), HatchStyle::Vertical);
        assert_eq!(HatchStyle::from_u32(2), HatchStyle::FDiagonal);
        assert_eq!(HatchStyle::from_u32(3), HatchStyle::BDiagonal);
        assert_eq!(HatchStyle::from_u32(4), HatchStyle::Cross);
        assert_eq!(HatchStyle::from_u32(5), HatchStyle::DiagCross);
        assert_eq!(HatchStyle::from_u32(42), HatchStyle::Horizontal); // unknown
    }

    // ── Affine transform ─────────────────────────────────────────────────────

    #[test]
    fn affine_identity_apply_and_concat() {
        let id = Affine::identity();
        let p = id.apply(3.0, 7.0);
        assert_eq!((p.x, p.y), (3.0, 7.0));
        // Translate then scale, via concat (self ∘ other = apply other first).
        let scale = Affine {
            m11: 2.0,
            m12: 0.0,
            m21: 0.0,
            m22: 2.0,
            dx: 0.0,
            dy: 0.0,
        };
        let translate = Affine {
            m11: 1.0,
            m12: 0.0,
            m21: 0.0,
            m22: 1.0,
            dx: 5.0,
            dy: 1.0,
        };
        let composed = scale.concat(&translate); // translate first, then scale
        let q = composed.apply(1.0, 1.0); // (1+5, 1+1)*2 = (12, 4)
        assert_eq!((q.x, q.y), (12.0, 4.0));
    }

    // ── region bbox ──────────────────────────────────────────────────────────

    #[test]
    fn region_bbox_unions_rects_and_handles_empty() {
        assert!(region_bbox(&[]).is_none());
        let rects = [
            LogRect {
                left: 5.0,
                top: 5.0,
                right: 10.0,
                bottom: 8.0,
            },
            LogRect {
                left: 1.0,
                top: 2.0,
                right: 7.0,
                bottom: 20.0,
            },
        ];
        let bb = region_bbox(&rects).expect("union");
        assert_eq!(
            (bb.left, bb.top, bb.right, bb.bottom),
            (1.0, 2.0, 10.0, 20.0)
        );
    }

    // ── DIB decoding: each bpp path ──────────────────────────────────────────

    /// Build a BITMAPINFOHEADER (40 bytes) + appended `body`.
    fn bih(w: i32, h: i32, bpp: u16, compression: u32, body: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&40u32.to_le_bytes()); // biSize
        v.extend_from_slice(&w.to_le_bytes());
        v.extend_from_slice(&h.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes()); // planes
        v.extend_from_slice(&bpp.to_le_bytes());
        v.extend_from_slice(&compression.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes()); // image size
        v.extend_from_slice(&[0u8; 16]); // ppm x/y, clrUsed, clrImportant
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn decode_packed_dib_rejects_malformed() {
        assert!(decode_packed_dib(&[]).is_none());
        assert!(decode_packed_dib(&[0u8; 3]).is_none());
        // header_size < 40
        assert!(decode_packed_dib(&12u32.to_le_bytes()).is_none());
        // zero width
        assert!(decode_packed_dib(&bih(0, 1, 24, 0, &[])).is_none());
        // zero height
        assert!(decode_packed_dib(&bih(1, 0, 24, 0, &[])).is_none());
    }

    #[test]
    fn decode_packed_dib_24bpp_bottom_up() {
        // 2×1, 24bpp BI_RGB. Row padded to 4 bytes. BGR order. Bottom-up.
        // Pixel0 = blue(255,0,0 BGR=0,0,255), Pixel1 = green.
        let row = [0u8, 0, 255, /*p0 BGR*/ 0, 255, 0 /*p1 BGR*/, 0, 0]; // +2 pad → 8 bytes
        let dib = decode_packed_dib(&bih(2, 1, 24, 0, &row)).expect("24bpp");
        assert_eq!((dib.width, dib.height), (2, 1));
        assert_eq!(dib.at(0, 0), Rgba::rgb(255, 0, 0));
        assert_eq!(dib.at(1, 0), Rgba::rgb(0, 255, 0));
    }

    #[test]
    fn decode_packed_dib_32bpp() {
        // 1×1 32bpp BGRA.
        let body = [10u8, 20, 30, 255]; // B,G,R,A
        let dib = decode_packed_dib(&bih(1, 1, 32, 0, &body)).expect("32bpp");
        assert_eq!(dib.at(0, 0), Rgba::rgb(30, 20, 10));
    }

    #[test]
    fn decode_packed_dib_8bpp_palette() {
        // 1×1 8bpp: palette of 256 BGRA quads precedes 4-byte-padded pixel row.
        let mut body = Vec::new();
        // palette[0] = red (BGRA), rest zero.
        body.extend_from_slice(&[0u8, 0, 255, 0]);
        body.extend_from_slice(&vec![0u8; 255 * 4]);
        body.extend_from_slice(&[0u8, 0, 0, 0]); // pixel index 0, padded row
        let dib = decode_packed_dib(&bih(1, 1, 8, 0, &body)).expect("8bpp");
        assert_eq!(dib.at(0, 0), Rgba::rgb(255, 0, 0));
    }

    #[test]
    fn dib_sample_bilinear_clamps_and_empty() {
        let dib = Dib {
            width: 0,
            height: 0,
            rgba: vec![],
        };
        assert_eq!(dib.sample_bilinear(0.5, 0.5), Rgba::TRANSPARENT);
        let dib = Dib {
            width: 2,
            height: 1,
            rgba: vec![0, 0, 0, 255, 255, 255, 255, 255],
        };
        // Sampling well outside clamps to edge texels.
        let left = dib.sample_bilinear(-5.0, 0.5);
        let right = dib.sample_bilinear(10.0, 0.5);
        assert_eq!(left, Rgba::rgb(0, 0, 0));
        assert_eq!(right, Rgba::rgb(255, 255, 255));
        // Out-of-range at() returns transparent.
        assert_eq!(dib.at(9, 9), Rgba::TRANSPARENT);
    }
}
