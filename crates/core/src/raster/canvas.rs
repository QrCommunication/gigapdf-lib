//! An RGBA framebuffer with anti-aliased polygon filling — zero dependencies.
//!
//! Filling uses the classic scanline algorithm with 4× vertical supersampling
//! and exact horizontal coverage, supporting both non-zero and even-odd winding
//! (the two PDF fill rules, `f` and `f*`). Device space here is top-left origin,
//! y down — callers map PDF user space (bottom-left, y up) before filling.

/// A straight edge in a flattened path, in device pixels.
#[derive(Debug, Clone, Copy)]
pub struct Edge {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

/// A separable PDF blend mode (ISO 32000-1 §11.3.5). Each acts channel-by-channel
/// on the backdrop `b` and source `s` colours (both `0.0..=1.0`); `Normal` simply
/// returns the source. Non-separable modes (Hue/Saturation/Color/Luminosity) are
/// approximated as `Normal` — they need the whole colour, not a per-channel rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BlendMode {
    #[default]
    Normal,
    Multiply,
    Screen,
    Overlay,
    Darken,
    Lighten,
    ColorDodge,
    ColorBurn,
    HardLight,
    SoftLight,
    Difference,
    Exclusion,
}

impl BlendMode {
    /// Parse a `/BM` name (an ExtGState blend-mode entry). Unknown names — and the
    /// non-separable modes — fall back to `Normal`.
    pub fn from_name(name: &[u8]) -> BlendMode {
        match name {
            b"Multiply" => BlendMode::Multiply,
            b"Screen" => BlendMode::Screen,
            b"Overlay" => BlendMode::Overlay,
            b"Darken" => BlendMode::Darken,
            b"Lighten" => BlendMode::Lighten,
            b"ColorDodge" => BlendMode::ColorDodge,
            b"ColorBurn" => BlendMode::ColorBurn,
            b"HardLight" => BlendMode::HardLight,
            b"SoftLight" => BlendMode::SoftLight,
            b"Difference" => BlendMode::Difference,
            b"Exclusion" => BlendMode::Exclusion,
            _ => BlendMode::Normal,
        }
    }

    /// Apply the blend rule to one channel pair (`b` backdrop, `s` source), both in
    /// `0.0..=1.0`, returning the blended channel value.
    fn apply(self, b: f64, s: f64) -> f64 {
        match self {
            BlendMode::Normal => s,
            BlendMode::Multiply => b * s,
            BlendMode::Screen => b + s - b * s,
            BlendMode::Overlay => hard_light(s, b),
            BlendMode::Darken => b.min(s),
            BlendMode::Lighten => b.max(s),
            BlendMode::ColorDodge => {
                if b <= 0.0 {
                    0.0
                } else if s >= 1.0 {
                    1.0
                } else {
                    (b / (1.0 - s)).min(1.0)
                }
            }
            BlendMode::ColorBurn => {
                if b >= 1.0 {
                    1.0
                } else if s <= 0.0 {
                    0.0
                } else {
                    1.0 - ((1.0 - b) / s).min(1.0)
                }
            }
            BlendMode::HardLight => hard_light(b, s),
            BlendMode::SoftLight => {
                if s <= 0.5 {
                    b - (1.0 - 2.0 * s) * b * (1.0 - b)
                } else {
                    let d = if b <= 0.25 {
                        ((16.0 * b - 12.0) * b + 4.0) * b
                    } else {
                        b.sqrt()
                    };
                    b + (2.0 * s - 1.0) * (d - b)
                }
            }
            BlendMode::Difference => (b - s).abs(),
            BlendMode::Exclusion => b + s - 2.0 * b * s,
        }
    }
}

fn hard_light(b: f64, s: f64) -> f64 {
    if s <= 0.5 {
        b * (2.0 * s)
    } else {
        let s2 = 2.0 * s - 1.0;
        b + s2 - b * s2
    }
}

/// A per-pixel clip coverage mask in device space (`0.0..=1.0` per pixel). A pixel
/// painted through the mask has its coverage multiplied by the mask value, so an
/// arbitrary path clip (`W`/`W*`) and a soft mask (`/SMask`) are both just buffers
/// to multiply. Intersecting two clips multiplies their buffers.
#[derive(Debug, Clone)]
pub struct ClipMask {
    pub width: u32,
    pub height: u32,
    /// `width*height` coverage values, row-major top-to-bottom.
    pub cover: Vec<f32>,
}

impl ClipMask {
    /// A mask that admits everything (coverage 1.0 everywhere).
    pub fn full(width: u32, height: u32) -> ClipMask {
        ClipMask {
            width,
            height,
            cover: vec![1.0; (width as usize) * (height as usize)],
        }
    }

    /// Rasterize `edges` (a flattened path in device space) into a fresh coverage
    /// mask using the given winding rule — the per-pixel area covered by the path.
    pub fn from_edges(width: u32, height: u32, edges: &[Edge], even_odd: bool) -> ClipMask {
        let mut mask = ClipMask {
            width,
            height,
            cover: vec![0.0; (width as usize) * (height as usize)],
        };
        rasterize_coverage(edges, width, height, even_odd, &mut |px, py, cov| {
            let i = (py as usize) * (width as usize) + px as usize;
            mask.cover[i] = (mask.cover[i] + cov as f32).min(1.0);
        });
        mask
    }

    /// Intersect with another mask of the same size (per-pixel product). A pixel
    /// passes only where *both* masks admit it — exactly the PDF clip semantics
    /// when a `W` is issued inside an already-clipped region.
    pub fn intersect(&self, other: &ClipMask) -> ClipMask {
        let mut cover = self.cover.clone();
        if other.width == self.width && other.height == self.height {
            for (c, &o) in cover.iter_mut().zip(other.cover.iter()) {
                *c *= o;
            }
        }
        ClipMask {
            width: self.width,
            height: self.height,
            cover,
        }
    }

    /// Coverage admitted at `(x, y)` (0.0 outside the buffer).
    #[inline]
    pub(crate) fn at(&self, x: i32, y: i32) -> f64 {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return 0.0;
        }
        self.cover[(y as usize) * (self.width as usize) + x as usize] as f64
    }
}

/// An RGBA8 framebuffer (row-major, top-to-bottom).
#[derive(Debug, Clone)]
pub struct Canvas {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl Canvas {
    /// A new canvas filled with opaque white.
    pub fn new(width: u32, height: u32) -> Canvas {
        Canvas {
            width,
            height,
            pixels: vec![0xFF; (width as usize) * (height as usize) * 4],
        }
    }

    /// Encode the canvas as a PNG.
    pub fn to_png(&self) -> Vec<u8> {
        super::png::encode_png(self.width, self.height, &self.pixels)
    }

    /// Alpha-blend `color` (`[r, g, b]`, 0..=255) into pixel `(x, y)` with the
    /// given coverage `alpha` (0.0..=1.0), compositing with a separable `mode`.
    /// The blended colour `B(backdrop, source)` is computed per channel, then
    /// mixed with the backdrop by the coverage `alpha` (the source is opaque, so
    /// the Porter-Duff result reduces to `lerp(backdrop, B, alpha)`).
    /// `mode == Normal` is plain source-over.
    pub(crate) fn blend_mode(
        &mut self,
        x: i32,
        y: i32,
        color: [u8; 3],
        alpha: f64,
        mode: BlendMode,
    ) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 || alpha <= 0.0 {
            return;
        }
        let a = alpha.clamp(0.0, 1.0);
        let idx = ((y as usize) * (self.width as usize) + x as usize) * 4;
        for (c, &src) in color.iter().enumerate() {
            let dst = self.pixels[idx + c] as f64 / 255.0;
            let blended = if mode == BlendMode::Normal {
                src as f64 / 255.0
            } else {
                mode.apply(dst, src as f64 / 255.0)
            };
            self.pixels[idx + c] = ((blended * a + dst * (1.0 - a)) * 255.0).round() as u8;
        }
        // Keep the framebuffer opaque (white paper background).
        self.pixels[idx + 3] = 0xFF;
    }

    /// Fill the polygon described by `edges` with `color`, using non-zero or
    /// even-odd winding. `edges` may contain several sub-paths.
    pub fn fill(&mut self, edges: &[Edge], color: [u8; 3], even_odd: bool) {
        self.fill_alpha(edges, color, even_odd, 1.0);
    }

    /// Like [`fill`](Self::fill) but scales every pixel's coverage by a constant
    /// `alpha` (`0.0..=1.0`) — used to honour an annotation's `/CA` (non-stroking
    /// opacity) when painting its appearance. `alpha == 1.0` is identical to
    /// [`fill`](Self::fill).
    pub fn fill_alpha(&mut self, edges: &[Edge], color: [u8; 3], even_odd: bool, alpha: f64) {
        self.fill_ext(edges, color, even_odd, alpha, None, BlendMode::Normal);
    }

    /// The full fill path: rasterize `edges` (non-zero or even-odd) and composite
    /// `color` with a constant `alpha`, an optional clip mask `clip` (per-pixel
    /// coverage multiplier — the active `W`/`W*` clip and/or a soft mask), and a
    /// separable blend `mode`. [`fill_alpha`](Self::fill_alpha) and
    /// [`fill`](Self::fill) are this with no clip and `Normal`.
    pub fn fill_ext(
        &mut self,
        edges: &[Edge],
        color: [u8; 3],
        even_odd: bool,
        alpha: f64,
        clip: Option<&ClipMask>,
        mode: BlendMode,
    ) {
        let alpha = alpha.clamp(0.0, 1.0);
        if edges.is_empty() || alpha <= 0.0 {
            return;
        }
        let width = self.width;
        let height = self.height;
        rasterize_coverage(edges, width, height, even_odd, &mut |px, py, cov| {
            let mut a = cov * alpha;
            if let Some(c) = clip {
                a *= c.at(px, py);
            }
            if a > 0.0 {
                self.blend_mode(px, py, color, a, mode);
            }
        });
    }
}

/// Rasterize the polygon `edges` (non-zero or even-odd winding) and invoke `emit`
/// once per touched pixel with its `(px, py, coverage)` — `coverage` in `0.0..=1.0`
/// (4× vertical supersampling, exact horizontal coverage). This is the shared
/// scanline core behind solid fills, alpha fills, and clip-mask rasterization.
fn rasterize_coverage(
    edges: &[Edge],
    width: u32,
    height: u32,
    even_odd: bool,
    emit: &mut dyn FnMut(i32, i32, f64),
) {
    const SS: usize = 4; // vertical sub-samples per pixel row
    let mut min_y = f64::INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for e in edges {
        min_y = min_y.min(e.y0.min(e.y1));
        max_y = max_y.max(e.y0.max(e.y1));
    }
    let y_start = (min_y.floor().max(0.0)) as i32;
    let y_end = (max_y.ceil().min(height as f64)) as i32;

    // Per-pixel-row horizontal coverage accumulator.
    let mut coverage = vec![0.0f64; width as usize];

    for py in y_start..y_end {
        for c in coverage.iter_mut() {
            *c = 0.0;
        }
        for sub in 0..SS {
            let sy = py as f64 + (sub as f64 + 0.5) / SS as f64;
            // Gather edge crossings at this sub-scanline.
            let mut crossings: Vec<(f64, i32)> = Vec::new();
            for e in edges {
                let (mut ax, mut ay, mut bx, mut by) = (e.x0, e.y0, e.x1, e.y1);
                if ay == by {
                    continue; // horizontal edge contributes no crossing
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
                let x = ax + t * (bx - ax);
                crossings.push((x, dir));
            }
            if crossings.len() < 2 {
                continue;
            }
            crossings.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

            // Walk spans where the winding rule says "inside".
            let mut winding = 0i32;
            for pair in crossings.windows(2) {
                let (x_left, dir) = pair[0];
                let x_right = pair[1].0;
                winding += dir;
                let inside = if even_odd {
                    winding % 2 != 0
                } else {
                    winding != 0
                };
                if inside {
                    add_span_coverage(&mut coverage, x_left, x_right, 1.0 / SS as f64);
                }
            }
        }
        for (px, &cov) in coverage.iter().enumerate() {
            if cov > 0.0 {
                emit(px as i32, py, cov.min(1.0));
            }
        }
    }
}

/// Accumulate horizontal coverage for the span `[x_left, x_right)` weighted by
/// `weight`, with exact partial coverage at the two end pixels.
fn add_span_coverage(coverage: &mut [f64], x_left: f64, x_right: f64, weight: f64) {
    if x_right <= x_left {
        return;
    }
    let lo = x_left.max(0.0);
    let hi = x_right.min(coverage.len() as f64);
    if hi <= lo {
        return;
    }
    let mut px = lo.floor() as usize;
    while (px as f64) < hi {
        let cell_left = px as f64;
        let cell_right = cell_left + 1.0;
        let covered = hi.min(cell_right) - lo.max(cell_left);
        if covered > 0.0 {
            coverage[px] += covered * weight;
        }
        px += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect_edges(x: f64, y: f64, w: f64, h: f64) -> Vec<Edge> {
        vec![
            Edge {
                x0: x,
                y0: y,
                x1: x + w,
                y1: y,
            },
            Edge {
                x0: x + w,
                y0: y,
                x1: x + w,
                y1: y + h,
            },
            Edge {
                x0: x + w,
                y0: y + h,
                x1: x,
                y1: y + h,
            },
            Edge {
                x0: x,
                y0: y + h,
                x1: x,
                y1: y,
            },
        ]
    }

    #[test]
    fn fills_a_solid_rectangle() {
        let mut canvas = Canvas::new(20, 20);
        canvas.fill(&rect_edges(4.0, 4.0, 12.0, 12.0), [255, 0, 0], false);
        // A pixel well inside the rectangle is fully red.
        let idx = (10 * 20 + 10) * 4;
        assert_eq!(&canvas.pixels[idx..idx + 3], &[255, 0, 0]);
        // A pixel outside stays white.
        let outside = (20 + 1) * 4;
        assert_eq!(&canvas.pixels[outside..outside + 3], &[255, 255, 255]);
    }

    #[test]
    fn even_odd_leaves_a_hole() {
        // Outer 16x16 box with an inner 8x8 box → even-odd ring, hollow centre.
        let mut canvas = Canvas::new(20, 20);
        let mut edges = rect_edges(2.0, 2.0, 16.0, 16.0);
        edges.extend(rect_edges(6.0, 6.0, 8.0, 8.0));
        canvas.fill(&edges, [0, 0, 0], true);
        let center = (10 * 20 + 10) * 4;
        assert_eq!(&canvas.pixels[center..center + 3], &[255, 255, 255], "hole");
        let ring = (3 * 20 + 10) * 4;
        assert_eq!(&canvas.pixels[ring..ring + 3], &[0, 0, 0], "ring filled");
    }
}
