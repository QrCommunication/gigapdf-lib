//! Translate SVG path data into PDF path-painting content-stream operators.
//!
//! Editors describe freeform shapes, polygons and triangles as SVG path data
//! (`M`/`L`/`C`/`Q`/`Z`…). PDF's path model is similar but Y-up and without
//! quadratic Béziers or arcs, so we:
//!
//! * flip the Y axis — the SVG origin `(0,0)` maps to PDF user-space `(ox, oy)`
//!   and SVG-Y grows downward (`X = ox + sx`, `Y = oy − sy`), matching
//!   `pdf-lib`'s `drawSvgPath({ x: ox, y: oy })`;
//! * convert quadratic Béziers to cubics (`c1 = p0 + ⅔(pc−p0)`,
//!   `c2 = p2 + ⅔(pc−p2)`);
//! * approximate elliptical arcs (`A`) by a straight segment to the endpoint —
//!   a deliberate, documented simplification rather than a panic.

use super::{num, Rgb};

/// A resolved path segment in SVG user space (cubics already expanded).
#[derive(Debug, Clone, Copy)]
pub(crate) enum Seg {
    Move(f64, f64),
    Line(f64, f64),
    Cubic(f64, f64, f64, f64, f64, f64),
    Close,
}

/// Translate SVG path data `d` into PDF path content bytes, anchored so the SVG
/// origin maps to `(ox, oy)` with the Y axis flipped. Stroked, filled or both;
/// returns an empty `Vec` when the path has no drawable segment.
pub fn svg_path_ops(
    d: &str,
    ox: f64,
    oy: f64,
    stroke: Option<Rgb>,
    fill: Option<Rgb>,
    line_width: f64,
) -> Vec<u8> {
    let segs = parse(d);
    if !segs.iter().any(|s| matches!(s, Seg::Line(..) | Seg::Cubic(..))) {
        return Vec::new(); // moveto-only / empty path draws nothing
    }

    let tx = |x: f64| ox + x;
    let ty = |y: f64| oy - y;

    let mut out = Vec::new();
    out.extend_from_slice(b"q\n");
    if let Some([r, g, b]) = fill {
        out.extend_from_slice(format!("{} {} {} rg\n", num(r), num(g), num(b)).as_bytes());
    }
    if let Some([r, g, b]) = stroke {
        out.extend_from_slice(format!("{} {} {} RG\n", num(r), num(g), num(b)).as_bytes());
    }
    out.extend_from_slice(format!("{} w\n", num(line_width)).as_bytes());
    for seg in &segs {
        match *seg {
            Seg::Move(x, y) => {
                out.extend_from_slice(format!("{} {} m\n", num(tx(x)), num(ty(y))).as_bytes());
            }
            Seg::Line(x, y) => {
                out.extend_from_slice(format!("{} {} l\n", num(tx(x)), num(ty(y))).as_bytes());
            }
            Seg::Cubic(x1, y1, x2, y2, x3, y3) => {
                out.extend_from_slice(
                    format!(
                        "{} {} {} {} {} {} c\n",
                        num(tx(x1)),
                        num(ty(y1)),
                        num(tx(x2)),
                        num(ty(y2)),
                        num(tx(x3)),
                        num(ty(y3))
                    )
                    .as_bytes(),
                );
            }
            Seg::Close => out.extend_from_slice(b"h\n"),
        }
    }
    let paint: &[u8] = match (fill.is_some(), stroke.is_some()) {
        (true, true) => b"B\n",
        (true, false) => b"f\n",
        _ => b"S\n",
    };
    out.extend_from_slice(paint);
    out.extend_from_slice(b"Q\n");
    out
}

/// Tolerant SVG-path tokenizer/parser → resolved segments. Never panics on
/// malformed input: it stops as soon as a required number can't be read.
pub(crate) fn parse(d: &str) -> Vec<Seg> {
    let bytes = d.as_bytes();
    let mut scan = Scan { bytes, pos: 0 };
    let mut segs = Vec::new();

    // Current point, current subpath start, and the previous Bézier control
    // points (for the smooth `S`/`T` reflections).
    let (mut cx, mut cy) = (0.0f64, 0.0f64);
    let (mut sx, mut sy) = (0.0f64, 0.0f64);
    let mut prev_cubic: Option<(f64, f64)> = None;
    let mut prev_quad: Option<(f64, f64)> = None;

    let mut cmd = 0u8;
    loop {
        scan.skip_seps();
        let next = match scan.peek() {
            Some(c) => c,
            None => break,
        };
        if next.is_ascii_alphabetic() {
            cmd = next;
            scan.pos += 1;
            if cmd == b'Z' || cmd == b'z' {
                segs.push(Seg::Close);
                cx = sx;
                cy = sy;
                prev_cubic = None;
                prev_quad = None;
            }
            continue;
        }
        // A bare number continues the current command (implicit repeat). After a
        // moveto, repeats are linetos.
        let rel = cmd.is_ascii_lowercase();
        match cmd.to_ascii_uppercase() {
            b'M' => {
                let Some((x, y)) = scan.pair() else { break };
                let (x, y) = if rel { (cx + x, cy + y) } else { (x, y) };
                cx = x;
                cy = y;
                sx = x;
                sy = y;
                segs.push(Seg::Move(x, y));
                cmd = if rel { b'l' } else { b'L' }; // subsequent pairs are lines
                prev_cubic = None;
                prev_quad = None;
            }
            b'L' => {
                let Some((x, y)) = scan.pair() else { break };
                let (x, y) = if rel { (cx + x, cy + y) } else { (x, y) };
                cx = x;
                cy = y;
                segs.push(Seg::Line(x, y));
                prev_cubic = None;
                prev_quad = None;
            }
            b'H' => {
                let Some(x) = scan.number() else { break };
                cx = if rel { cx + x } else { x };
                segs.push(Seg::Line(cx, cy));
                prev_cubic = None;
                prev_quad = None;
            }
            b'V' => {
                let Some(y) = scan.number() else { break };
                cy = if rel { cy + y } else { y };
                segs.push(Seg::Line(cx, cy));
                prev_cubic = None;
                prev_quad = None;
            }
            b'C' => {
                let (Some((x1, y1)), Some((x2, y2)), Some((x3, y3))) =
                    (scan.pair(), scan.pair(), scan.pair())
                else {
                    break;
                };
                let (x1, y1, x2, y2, x3, y3) = if rel {
                    (cx + x1, cy + y1, cx + x2, cy + y2, cx + x3, cy + y3)
                } else {
                    (x1, y1, x2, y2, x3, y3)
                };
                segs.push(Seg::Cubic(x1, y1, x2, y2, x3, y3));
                cx = x3;
                cy = y3;
                prev_cubic = Some((x2, y2));
                prev_quad = None;
            }
            b'S' => {
                let (Some((x2, y2)), Some((x3, y3))) = (scan.pair(), scan.pair()) else {
                    break;
                };
                let (x2, y2, x3, y3) = if rel {
                    (cx + x2, cy + y2, cx + x3, cy + y3)
                } else {
                    (x2, y2, x3, y3)
                };
                // First control = reflection of the previous cubic control.
                let (x1, y1) = match prev_cubic {
                    Some((px, py)) => (2.0 * cx - px, 2.0 * cy - py),
                    None => (cx, cy),
                };
                segs.push(Seg::Cubic(x1, y1, x2, y2, x3, y3));
                cx = x3;
                cy = y3;
                prev_cubic = Some((x2, y2));
                prev_quad = None;
            }
            b'Q' => {
                let (Some((qx, qy)), Some((x, y))) = (scan.pair(), scan.pair()) else {
                    break;
                };
                let (qx, qy, x, y) = if rel {
                    (cx + qx, cy + qy, cx + x, cy + y)
                } else {
                    (qx, qy, x, y)
                };
                let (c1, c2) = quad_to_cubic(cx, cy, qx, qy, x, y);
                segs.push(Seg::Cubic(c1.0, c1.1, c2.0, c2.1, x, y));
                cx = x;
                cy = y;
                prev_quad = Some((qx, qy));
                prev_cubic = None;
            }
            b'T' => {
                let Some((x, y)) = scan.pair() else { break };
                let (x, y) = if rel { (cx + x, cy + y) } else { (x, y) };
                let (qx, qy) = match prev_quad {
                    Some((px, py)) => (2.0 * cx - px, 2.0 * cy - py),
                    None => (cx, cy),
                };
                let (c1, c2) = quad_to_cubic(cx, cy, qx, qy, x, y);
                segs.push(Seg::Cubic(c1.0, c1.1, c2.0, c2.1, x, y));
                cx = x;
                cy = y;
                prev_quad = Some((qx, qy));
                prev_cubic = None;
            }
            b'A' => {
                // rx ry x-axis-rotation large-arc-flag sweep-flag x y. PDF has no
                // arc operator, so convert the elliptical arc to cubic Béziers
                // (SVG endpoint→centre parameterisation, split into ≤90° pieces).
                let mut p = [0.0f64; 7];
                let mut ok = true;
                for slot in p.iter_mut() {
                    match scan.number() {
                        Some(v) => *slot = v,
                        None => {
                            ok = false;
                            break;
                        }
                    }
                }
                if !ok {
                    break;
                }
                let (ex, ey) = if rel { (cx + p[5], cy + p[6]) } else { (p[5], p[6]) };
                let cubics = arc_to_cubics(cx, cy, p[0], p[1], p[2], p[3] != 0.0, p[4] != 0.0, ex, ey);
                if cubics.is_empty() {
                    segs.push(Seg::Line(ex, ey)); // degenerate radii → straight line
                } else {
                    for (c1x, c1y, c2x, c2y, x3, y3) in cubics {
                        segs.push(Seg::Cubic(c1x, c1y, c2x, c2y, x3, y3));
                    }
                }
                cx = ex;
                cy = ey;
                prev_cubic = None;
                prev_quad = None;
            }
            _ => break, // unknown command
        }
    }
    segs
}

/// Convert a quadratic Bézier `(p0, pc, p2)` to a cubic's two control points.
fn quad_to_cubic(
    p0x: f64,
    p0y: f64,
    pcx: f64,
    pcy: f64,
    p2x: f64,
    p2y: f64,
) -> ((f64, f64), (f64, f64)) {
    let c1 = (p0x + 2.0 / 3.0 * (pcx - p0x), p0y + 2.0 / 3.0 * (pcy - p0y));
    let c2 = (p2x + 2.0 / 3.0 * (pcx - p2x), p2y + 2.0 / 3.0 * (pcy - p2y));
    (c1, c2)
}

/// Signed angle from vector `u` to vector `v` (SVG arc helper).
fn arc_angle(ux: f64, uy: f64, vx: f64, vy: f64) -> f64 {
    let dot = ux * vx + uy * vy;
    let len = ((ux * ux + uy * uy) * (vx * vx + vy * vy)).sqrt();
    let mut a = if len > 0.0 { (dot / len).clamp(-1.0, 1.0).acos() } else { 0.0 };
    if ux * vy - uy * vx < 0.0 {
        a = -a;
    }
    a
}

/// Convert an SVG elliptical arc (endpoint parameterisation) to a list of cubic
/// Bézier control sets `(c1x, c1y, c2x, c2y, ex, ey)`, each spanning ≤90°. Empty
/// for degenerate radii or coincident endpoints (caller draws a line).
#[allow(clippy::too_many_arguments)]
fn arc_to_cubics(
    x1: f64,
    y1: f64,
    mut rx: f64,
    mut ry: f64,
    phi_deg: f64,
    large_arc: bool,
    sweep: bool,
    x2: f64,
    y2: f64,
) -> Vec<(f64, f64, f64, f64, f64, f64)> {
    rx = rx.abs();
    ry = ry.abs();
    if rx < 1e-9 || ry < 1e-9 || ((x1 - x2).abs() < 1e-12 && (y1 - y2).abs() < 1e-12) {
        return Vec::new();
    }
    let (sin_phi, cos_phi) = phi_deg.to_radians().sin_cos();

    // Endpoint → centre parameterisation (SVG implementation notes F.6.5).
    let dx = (x1 - x2) / 2.0;
    let dy = (y1 - y2) / 2.0;
    let x1p = cos_phi * dx + sin_phi * dy;
    let y1p = -sin_phi * dx + cos_phi * dy;

    let lambda = x1p * x1p / (rx * rx) + y1p * y1p / (ry * ry);
    if lambda > 1.0 {
        let s = lambda.sqrt();
        rx *= s;
        ry *= s;
    }

    let (rx2, ry2) = (rx * rx, ry * ry);
    let num = (rx2 * ry2 - rx2 * y1p * y1p - ry2 * x1p * x1p).max(0.0);
    let den = rx2 * y1p * y1p + ry2 * x1p * x1p;
    let mut coef = if den > 0.0 { (num / den).sqrt() } else { 0.0 };
    if large_arc == sweep {
        coef = -coef;
    }
    let cxp = coef * rx * y1p / ry;
    let cyp = -coef * ry * x1p / rx;
    let cx = cos_phi * cxp - sin_phi * cyp + (x1 + x2) / 2.0;
    let cy = sin_phi * cxp + cos_phi * cyp + (y1 + y2) / 2.0;

    let ux = (x1p - cxp) / rx;
    let uy = (y1p - cyp) / ry;
    let vx = (-x1p - cxp) / rx;
    let vy = (-y1p - cyp) / ry;
    let theta1 = arc_angle(1.0, 0.0, ux, uy);
    let mut dtheta = arc_angle(ux, uy, vx, vy);
    if !sweep && dtheta > 0.0 {
        dtheta -= std::f64::consts::TAU;
    } else if sweep && dtheta < 0.0 {
        dtheta += std::f64::consts::TAU;
    }

    // Split into ≤90° pieces; the epsilon avoids an extra piece at exact 90°×k.
    let n = ((dtheta.abs() / std::f64::consts::FRAC_PI_2) - 1e-9).ceil().max(1.0) as usize;
    let delta = dtheta / n as f64;
    let alpha = 4.0 / 3.0 * (delta / 4.0).tan();

    // A point and its tangent on the rotated ellipse at angle `a`.
    let pt = |a: f64| {
        let (sa, ca) = a.sin_cos();
        (
            cx + cos_phi * rx * ca - sin_phi * ry * sa,
            cy + sin_phi * rx * ca + cos_phi * ry * sa,
        )
    };
    let der = |a: f64| {
        let (sa, ca) = a.sin_cos();
        (
            -cos_phi * rx * sa - sin_phi * ry * ca,
            -sin_phi * rx * sa + cos_phi * ry * ca,
        )
    };

    let mut out = Vec::with_capacity(n);
    let mut t = theta1;
    for _ in 0..n {
        let t2 = t + delta;
        let (p0x, p0y) = pt(t);
        let (p3x, p3y) = pt(t2);
        let (d0x, d0y) = der(t);
        let (d3x, d3y) = der(t2);
        out.push((
            p0x + alpha * d0x,
            p0y + alpha * d0y,
            p3x - alpha * d3x,
            p3y - alpha * d3y,
            p3x,
            p3y,
        ));
        t = t2;
    }
    out
}

struct Scan<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Scan<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_seps(&mut self) {
        while let Some(c) = self.peek() {
            if c == b' ' || c == b',' || c == b'\t' || c == b'\n' || c == b'\r' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    /// Read a number (sign, digits, fraction, exponent), or `None`.
    fn number(&mut self) -> Option<f64> {
        self.skip_seps();
        let start = self.pos;
        if matches!(self.peek(), Some(b'+') | Some(b'-')) {
            self.pos += 1;
        }
        let mut digits = false;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
            digits = true;
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
                digits = true;
            }
        }
        if !digits {
            self.pos = start;
            return None;
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            let save = self.pos;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            let mut exp_digits = false;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
                exp_digits = true;
            }
            if !exp_digits {
                self.pos = save; // a stray 'e' is not part of the number
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos]).ok()?;
        text.parse::<f64>().ok()
    }

    fn pair(&mut self) -> Option<(f64, f64)> {
        let x = self.number()?;
        let y = self.number()?;
        Some((x, y))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(d: &str, ox: f64, oy: f64) -> String {
        let bytes = svg_path_ops(d, ox, oy, Some([0.0, 0.0, 0.0]), None, 1.0);
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[test]
    fn polygon_with_close_and_y_flip() {
        let out = render("M 0 0 L 10 0 L 10 10 Z", 100.0, 200.0);
        assert!(out.contains("100 200 m"), "{out}");
        assert!(out.contains("110 200 l"), "{out}");
        assert!(out.contains("110 190 l"), "{out}");
        assert!(out.contains("h\n"), "{out}");
        assert!(out.trim_end().ends_with('Q'), "{out}");
        let body = out.trim_end();
        assert!(body[..body.len() - 2].trim_end().ends_with('S'), "{out}");
    }

    #[test]
    fn cubic_is_y_flipped() {
        let out = render("M0 0 C 0 10 10 10 10 0", 0.0, 0.0);
        assert!(out.contains("0 -10 10 -10 10 0 c"), "{out}");
    }

    #[test]
    fn arc_converts_to_cubics() {
        // 90° arc → exactly one cubic ending at the endpoint.
        let q = arc_to_cubics(1.0, 0.0, 1.0, 1.0, 0.0, false, true, 0.0, 1.0);
        assert_eq!(q.len(), 1);
        assert!((q[0].4).abs() < 1e-9 && (q[0].5 - 1.0).abs() < 1e-9, "endpoint (0,1): {:?}", q[0]);

        // Large-arc (~270°) splits into three ≤90° cubics, endpoint preserved.
        let big = arc_to_cubics(1.0, 0.0, 1.0, 1.0, 0.0, true, true, 0.0, 1.0);
        assert_eq!(big.len(), 3, "270° arc → 3 cubics");
        assert!((big[2].4).abs() < 1e-9 && (big[2].5 - 1.0).abs() < 1e-9, "endpoint preserved");

        // Degenerate radius → no cubics (caller draws a straight line).
        assert!(arc_to_cubics(0.0, 0.0, 0.0, 5.0, 0.0, false, true, 3.0, 4.0).is_empty());
    }

    #[test]
    fn arc_path_emits_curves_not_a_line() {
        let out = render("M10 10 A20 20 0 0 1 30 30", 0.0, 100.0);
        assert!(out.contains(" c\n"), "the arc command emits cubic curves: {out}");
    }

    #[test]
    fn relative_commands_track_current_point() {
        let out = render("m 0 0 l 5 5", 0.0, 0.0);
        assert!(out.contains("5 -5 l"), "{out}");
    }

    #[test]
    fn quadratic_becomes_cubic() {
        let out = render("M0 0 Q 10 0 10 10", 0.0, 0.0);
        assert!(out.contains(" c\n"), "quad converted to cubic: {out}");
        // No lowercase 'q' operator should appear (that's the graphics-state op).
        assert!(!out.contains(" q\n"), "{out}");
    }

    #[test]
    fn malformed_input_never_panics() {
        let _ = svg_path_ops("M garbage", 0.0, 0.0, None, Some([0.0, 0.0, 0.0]), 1.0);
        let _ = svg_path_ops("", 0.0, 0.0, None, None, 1.0);
        let _ = svg_path_ops("ZZZ", 0.0, 0.0, None, None, 1.0);
    }
}
