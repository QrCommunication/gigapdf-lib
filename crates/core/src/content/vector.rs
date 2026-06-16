//! Vector-path extraction (ISO 32000-1 §8.5): walk a page content stream and
//! return each painted path as geometry (segments in user space) plus the
//! graphics state in effect (fill/stroke colour, line width, alpha, dash). The
//! read-side counterpart of the SVG→PDF writer in [`super::svg_path`] — it
//! drives a host editor's shape layer without a rasteriser.

use std::collections::BTreeMap;

use super::interpret::{Bounds, BoundsBuilder, Matrix};
use super::Operation;
use crate::object::Object;

/// A single path segment in page user space (origin bottom-left, Y up). PDF's
/// path model has only cubic Béziers, so the `v`/`y` shorthands are expanded to
/// `Cubic` and rectangles (`re`) to `Move`+`Line`×3+`Close`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PathSeg {
    Move(f64, f64),
    Line(f64, f64),
    /// Cubic Bézier: `(cp1x, cp1y, cp2x, cp2y, endx, endy)`.
    Cubic(f64, f64, f64, f64, f64, f64),
    Close,
}

/// One painted vector path: its geometry plus the resolved graphics state.
#[derive(Debug, Clone)]
pub struct VectorPath {
    /// 0-based index among the page's painted paths.
    pub index: usize,
    /// Bounding box over every segment point (user space), if non-empty.
    pub bounds: Option<Bounds>,
    /// The path geometry, in page user space.
    pub segments: Vec<PathSeg>,
    /// Fill colour (RGB `0..=1`) when the paint op fills; `None` for stroke-only.
    pub fill: Option<[f64; 3]>,
    /// Stroke colour (RGB `0..=1`) when the paint op strokes; `None` for fill-only.
    pub stroke: Option<[f64; 3]>,
    /// Line width (`w`) in user-space units.
    pub stroke_width: f64,
    /// Non-stroking alpha (`/ca`), `0..=1`.
    pub fill_alpha: f64,
    /// Stroking alpha (`/CA`), `0..=1`.
    pub stroke_alpha: f64,
    /// Dash pattern (`d` array), empty for a solid line.
    pub dash: Vec<f64>,
}

#[derive(Clone)]
struct GfxState {
    ctm: Matrix,
    fill: [f64; 3],
    stroke: [f64; 3],
    line_width: f64,
    dash: Vec<f64>,
    fill_alpha: f64,
    stroke_alpha: f64,
}

/// Naive subtractive CMYK → RGB: `channel = (1 − ink)(1 − black)`.
fn cmyk_to_rgb(c: f64, m: f64, y: f64, k: f64) -> [f64; 3] {
    [
        (1.0 - c) * (1.0 - k),
        (1.0 - m) * (1.0 - k),
        (1.0 - y) * (1.0 - k),
    ]
}

fn nums(op: &Operation) -> Vec<f64> {
    op.operands
        .iter()
        .filter_map(|o| match o {
            Object::Integer(i) => Some(*i as f64),
            Object::Real(r) => Some(*r),
            _ => None,
        })
        .collect()
}

fn is_paint(op: &[u8]) -> bool {
    matches!(
        op,
        b"S" | b"s" | b"f" | b"F" | b"f*" | b"B" | b"B*" | b"b" | b"b*"
    )
}
fn paints_fill(op: &[u8]) -> bool {
    matches!(op, b"f" | b"F" | b"f*" | b"B" | b"B*" | b"b" | b"b*")
}
fn paints_stroke(op: &[u8]) -> bool {
    matches!(op, b"S" | b"s" | b"B" | b"B*" | b"b" | b"b*")
}

fn path_bounds(path: &[PathSeg]) -> Option<Bounds> {
    let mut bb = BoundsBuilder::new();
    for seg in path {
        match *seg {
            PathSeg::Move(x, y) | PathSeg::Line(x, y) => bb.add(x, y),
            PathSeg::Cubic(x1, y1, x2, y2, x3, y3) => {
                bb.add(x1, y1);
                bb.add(x2, y2);
                bb.add(x3, y3);
            }
            PathSeg::Close => {}
        }
    }
    bb.build()
}

/// Extract every painted vector path from a parsed content stream. `gstate` maps
/// each `/ExtGState` resource name to its `(/ca, /CA)` alphas (each optional),
/// so `gs` operators resolve to the right fill/stroke opacity. Clip-only paths
/// (`W`/`W*` … `n`) are skipped; every fill/stroke/fill-stroke paint op emits a
/// [`VectorPath`] carrying the geometry transformed into user space by the CTM.
pub fn vector_paths_from_ops(
    operations: &[Operation],
    gstate: &BTreeMap<String, (Option<f64>, Option<f64>)>,
) -> Vec<VectorPath> {
    let mut out = Vec::new();
    let mut st = GfxState {
        ctm: Matrix::IDENTITY,
        fill: [0.0, 0.0, 0.0],
        stroke: [0.0, 0.0, 0.0],
        line_width: 1.0,
        dash: Vec::new(),
        fill_alpha: 1.0,
        stroke_alpha: 1.0,
    };
    let mut stack: Vec<GfxState> = Vec::new();

    // The path under construction (user space) + its current/subpath-start point.
    let mut path: Vec<PathSeg> = Vec::new();
    let mut cur = (0.0f64, 0.0f64);
    let mut start = (0.0f64, 0.0f64);

    for op in operations {
        let operator = op.operator.as_slice();
        let n = nums(op);
        match operator {
            b"q" => stack.push(st.clone()),
            b"Q" => {
                if let Some(s) = stack.pop() {
                    st = s;
                }
            }
            b"cm" if n.len() == 6 => {
                st.ctm = Matrix::new(n[0], n[1], n[2], n[3], n[4], n[5]).then(&st.ctm);
            }
            b"gs" => {
                if let Some(Object::Name(name)) = op.operands.first() {
                    let key = String::from_utf8_lossy(name);
                    if let Some((ca, ca_stroke)) = gstate.get(key.as_ref()) {
                        if let Some(a) = ca {
                            st.fill_alpha = *a;
                        }
                        if let Some(a) = ca_stroke {
                            st.stroke_alpha = *a;
                        }
                    }
                }
            }
            // Fill colour.
            b"rg" if n.len() == 3 => st.fill = [n[0], n[1], n[2]],
            b"g" if n.len() == 1 => st.fill = [n[0], n[0], n[0]],
            b"k" if n.len() == 4 => st.fill = cmyk_to_rgb(n[0], n[1], n[2], n[3]),
            // Stroke colour.
            b"RG" if n.len() == 3 => st.stroke = [n[0], n[1], n[2]],
            b"G" if n.len() == 1 => st.stroke = [n[0], n[0], n[0]],
            b"K" if n.len() == 4 => st.stroke = cmyk_to_rgb(n[0], n[1], n[2], n[3]),
            b"w" if !n.is_empty() => st.line_width = n[0],
            b"d" => {
                st.dash = match op.operands.first() {
                    Some(Object::Array(items)) => items
                        .iter()
                        .filter_map(|o| match o {
                            Object::Integer(i) => Some(*i as f64),
                            Object::Real(r) => Some(*r),
                            _ => None,
                        })
                        .collect(),
                    _ => Vec::new(),
                };
            }
            // Path construction (operands transformed into user space by the CTM).
            b"m" if n.len() >= 2 => {
                let p = st.ctm.apply(n[0], n[1]);
                path.push(PathSeg::Move(p.0, p.1));
                cur = p;
                start = p;
            }
            b"l" if n.len() >= 2 => {
                let p = st.ctm.apply(n[0], n[1]);
                path.push(PathSeg::Line(p.0, p.1));
                cur = p;
            }
            b"c" if n.len() >= 6 => {
                let c1 = st.ctm.apply(n[0], n[1]);
                let c2 = st.ctm.apply(n[2], n[3]);
                let e = st.ctm.apply(n[4], n[5]);
                path.push(PathSeg::Cubic(c1.0, c1.1, c2.0, c2.1, e.0, e.1));
                cur = e;
            }
            // `v`: first control point is the current point.
            b"v" if n.len() >= 4 => {
                let c2 = st.ctm.apply(n[0], n[1]);
                let e = st.ctm.apply(n[2], n[3]);
                path.push(PathSeg::Cubic(cur.0, cur.1, c2.0, c2.1, e.0, e.1));
                cur = e;
            }
            // `y`: second control point is the end point.
            b"y" if n.len() >= 4 => {
                let c1 = st.ctm.apply(n[0], n[1]);
                let e = st.ctm.apply(n[2], n[3]);
                path.push(PathSeg::Cubic(c1.0, c1.1, e.0, e.1, e.0, e.1));
                cur = e;
            }
            b"re" if n.len() >= 4 => {
                let (x, y, w, h) = (n[0], n[1], n[2], n[3]);
                let p0 = st.ctm.apply(x, y);
                let p1 = st.ctm.apply(x + w, y);
                let p2 = st.ctm.apply(x + w, y + h);
                let p3 = st.ctm.apply(x, y + h);
                path.push(PathSeg::Move(p0.0, p0.1));
                path.push(PathSeg::Line(p1.0, p1.1));
                path.push(PathSeg::Line(p2.0, p2.1));
                path.push(PathSeg::Line(p3.0, p3.1));
                path.push(PathSeg::Close);
                cur = p0;
                start = p0;
            }
            b"h" => {
                path.push(PathSeg::Close);
                cur = start;
            }
            // `n` ends the path (often after a `W` clip) without painting.
            b"n" => path.clear(),
            // `W`/`W*` set the clip from the current path but keep it for the
            // following paint op — do not reset.
            b"W" | b"W*" => {}
            _ if is_paint(operator) => {
                if path.is_empty() {
                    continue;
                }
                // `s`/`b`/`b*` close the subpath before painting.
                if matches!(operator, b"s" | b"b" | b"b*")
                    && !matches!(path.last(), Some(PathSeg::Close))
                {
                    path.push(PathSeg::Close);
                }
                let fill = paints_fill(operator).then_some(st.fill);
                let stroke = paints_stroke(operator).then_some(st.stroke);
                let bounds = path_bounds(&path);
                out.push(VectorPath {
                    index: out.len(),
                    bounds,
                    segments: std::mem::take(&mut path),
                    fill,
                    stroke,
                    stroke_width: st.line_width,
                    fill_alpha: st.fill_alpha,
                    stroke_alpha: st.stroke_alpha,
                    dash: st.dash.clone(),
                });
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::parse_content;

    fn paths(content: &[u8]) -> Vec<VectorPath> {
        let ops = parse_content(content).unwrap();
        vector_paths_from_ops(&ops, &BTreeMap::new())
    }

    #[test]
    fn filled_rectangle_is_captured_with_colour_and_bounds() {
        // Red rectangle (10,20)-(110,70), filled.
        let p = paths(b"1 0 0 rg 10 20 100 50 re f");
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].fill, Some([1.0, 0.0, 0.0]));
        assert_eq!(p[0].stroke, None);
        let b = p[0].bounds.unwrap();
        assert!((b.x - 10.0).abs() < 1e-9 && (b.y - 20.0).abs() < 1e-9);
        assert!((b.width - 100.0).abs() < 1e-9 && (b.height - 50.0).abs() < 1e-9);
    }

    #[test]
    fn stroked_line_carries_width_and_stroke_colour_only() {
        let p = paths(b"0 0 1 RG 3 w 10 10 m 90 10 l S");
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].stroke, Some([0.0, 0.0, 1.0]));
        assert_eq!(p[0].fill, None);
        assert!((p[0].stroke_width - 3.0).abs() < 1e-9);
        assert_eq!(p[0].segments.len(), 2, "Move + Line");
    }

    #[test]
    fn clip_only_path_is_not_emitted() {
        // `W n` clips without painting → no shape.
        assert!(paths(b"0 0 50 50 re W n").is_empty());
    }

    #[test]
    fn cm_transforms_geometry_into_user_space() {
        // Translate by (100, 200) then draw a unit square at the origin.
        let p = paths(b"1 0 0 1 100 200 cm 0 0 10 10 re f");
        let b = p[0].bounds.unwrap();
        assert!((b.x - 100.0).abs() < 1e-9, "x translated, got {}", b.x);
        assert!((b.y - 200.0).abs() < 1e-9, "y translated, got {}", b.y);
    }

    #[test]
    fn fill_stroke_op_reports_both_colours() {
        let p = paths(b"1 0 0 rg 0 1 0 RG 0 0 20 20 re B");
        assert_eq!(p[0].fill, Some([1.0, 0.0, 0.0]));
        assert_eq!(p[0].stroke, Some([0.0, 1.0, 0.0]));
    }
}
