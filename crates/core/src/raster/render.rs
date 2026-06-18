//! Vector content rasterizer: interpret a page's content stream (paths, fills,
//! strokes, colours, CTM) and paint into a [`Canvas`]. Zero dependencies.
//!
//! This is slice 1 of the renderer — vector graphics only. Text glyphs and
//! images are drawn by later slices on top of the same canvas and fill engine.
//! Clipping (`W`) is currently ignored (paths over-paint rather than clip).

use std::collections::BTreeMap;

use super::canvas::{Canvas, Edge};
use crate::content::{parse_content, Operation, PageMatrix as Matrix};
use crate::font::cmap::TextDecoder;
use crate::font::GlyphSource;
use crate::object::Object;

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
fn blit_image(canvas: &mut Canvas, image: &RenderImage, ctm: &Matrix, base: &Matrix) {
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
            let alpha = image.rgba[idx + 3] as f64 / 255.0;
            canvas.blend(px, py, color, alpha);
        }
    }
}

#[derive(Clone)]
struct GState {
    ctm: Matrix,
    fill: [u8; 3],
    stroke: [u8; 3],
    line_width: f64,
}

impl GState {
    fn new(ctm: Matrix) -> Self {
        GState {
            ctm,
            fill: [0, 0, 0],
            stroke: [0, 0, 0],
            line_width: 1.0,
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
    let operations = match parse_content(content) {
        Ok(ops) => ops,
        Err(_) => return canvas,
    };

    let mut state = GState::new(Matrix::IDENTITY);
    let mut stack: Vec<GState> = Vec::new();

    // Path being constructed, in device space, split into sub-paths.
    let mut subpaths: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut cur_user = (0.0, 0.0);

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
            b"rg" if n.len() == 3 => state.fill = rgb(n[0], n[1], n[2]),
            b"RG" if n.len() == 3 => state.stroke = rgb(n[0], n[1], n[2]),
            b"g" if !n.is_empty() => state.fill = gray(n[0]),
            b"G" if !n.is_empty() => state.stroke = gray(n[0]),
            b"k" if n.len() == 4 => state.fill = cmyk_to_rgb(n[0], n[1], n[2], n[3]),
            b"K" if n.len() == 4 => state.stroke = cmyk_to_rgb(n[0], n[1], n[2], n[3]),
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
            b"f" | b"F" | b"f*" | b"b" | b"b*" | b"B" | b"B*" => {
                let even_odd = op.operator.ends_with(b"*");
                canvas.fill(&subpath_edges(&subpaths), state.fill, even_odd);
                if matches!(op.operator.as_slice(), b"b" | b"b*" | b"B" | b"B*") {
                    let lw = device_scale(&state.ctm, &base) * state.line_width;
                    canvas.fill(&stroke_edges(&subpaths, lw), state.stroke, false);
                }
                subpaths.clear();
            }
            b"S" | b"s" => {
                let lw = device_scale(&state.ctm, &base) * state.line_width;
                canvas.fill(&stroke_edges(&subpaths, lw), state.stroke, false);
                subpaths.clear();
            }
            b"n" => subpaths.clear(),

            b"Do" => {
                if let Some(Object::Name(name)) = op.operands.first() {
                    if let Some(image) = images.get(name) {
                        blit_image(&mut canvas, image, &state.ctm, &base);
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
                        &mut canvas,
                        f,
                        font_size,
                        &mut tm,
                        &state.ctm,
                        &base,
                        state.fill,
                        char_spacing,
                        word_spacing,
                        h_scale,
                        bytes,
                    );
                }
            }
            b"TJ" => {
                if let (Some(f), Some(Object::Array(items))) = (font, op.operands.first()) {
                    for item in items {
                        if let Object::String(bytes, _) = item {
                            show_text(
                                &mut canvas,
                                f,
                                font_size,
                                &mut tm,
                                &state.ctm,
                                &base,
                                state.fill,
                                char_spacing,
                                word_spacing,
                                h_scale,
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
    canvas
}

/// Render a text-show string: for each character code, look up its glyph and
/// fill the outline, advancing the text matrix by the glyph's width.
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
                code as u16 // Identity CIDToGIDMap (common Type0 case)
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
            let mut edges = Vec::new();
            for poly in ttf.glyph_polygons(gid) {
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
                canvas.fill(&edges, fill, false);
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
