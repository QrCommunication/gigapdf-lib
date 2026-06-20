//! Zero-dependency document rectification — the "auto-crop a photo of a page" front-end.
//!
//! When a page is *photographed* (rather than flatbed-scanned) the document is a bright
//! quadrilateral sitting at an angle on a darker, cluttered background. Feeding that whole
//! frame to the recognizer wastes resolution on the background and the perspective skew
//! warps the text lines. This module finds the document's four corners and warps it back to
//! an axis-aligned rectangle — the classic "scanner app" crop — so the rest of the OCR
//! pipeline sees a clean, head-on page.
//!
//! Pipeline (all pure `std`): bright-region mask → largest connected component = the sheet →
//! its four extreme corners → perspective homography (8×8 DLT solve) → bilinear inverse warp.
//! Gated: only fires when a *distinct* quad (clearly smaller than the frame, plausibly
//! rectangular) is found, so already-cropped scans pass through untouched.

use super::ocr::{connected_components, otsu_threshold};

/// Four corners in image pixels, ordered **TL, TR, BR, BL** (clockwise from top-left).
type Quad = [[f64; 2]; 4];

/// Detect the document quadrilateral: threshold to a bright-region mask (the sheet is lighter
/// than its surroundings), take the largest connected component, and read its four extreme
/// corners (min/max of x±y — exact for a convex quad). Returns `None` when no plausible
/// document is present: the bright region is missing, fills almost the whole frame (already a
/// tight scan — nothing to crop), or is too small/degenerate to be a page.
pub(crate) fn detect_document_quad(gray: &[u8], w: usize, h: usize) -> Option<Quad> {
    if w < 16 || h < 16 || gray.len() < w * h {
        return None;
    }
    // Bright mask: above Otsu (the sheet) — `connected_components` treats `true` as foreground.
    let thr = otsu_threshold(gray);
    let bright: Vec<bool> = gray.iter().map(|&g| g > thr).collect();
    let (blobs, labels) = connected_components(&bright, w, h);
    if blobs.is_empty() {
        return None;
    }
    // Largest component by pixel count (not bbox — robust to a stray bright speck in a corner).
    let mut count = vec![0usize; blobs.len()];
    for &l in &labels {
        if l >= 0 {
            count[l as usize] += 1;
        }
    }
    let (doc, &px) = count.iter().enumerate().max_by_key(|(_, &c)| c)?;
    let frac = px as f64 / (w * h) as f64;
    // Too small ⇒ not the page; ~full frame ⇒ already cropped (leave it alone).
    if !(0.18..=0.90).contains(&frac) {
        return None;
    }
    let id = doc as i32;
    // Four extreme corners of the (convex) sheet: TL=min(x+y), BR=max(x+y), TR=max(x−y), BL=min(x−y).
    let (mut tl, mut br, mut tr, mut bl) = ((f64::MAX, 0.0), (f64::MIN, 0.0), (f64::MIN, 0.0), (f64::MAX, 0.0));
    let (mut tlp, mut brp, mut trp, mut blp) = ([0.0; 2], [0.0; 2], [0.0; 2], [0.0; 2]);
    for y in 0..h {
        for x in 0..w {
            if labels[y * w + x] != id {
                continue;
            }
            let (xf, yf) = (x as f64, y as f64);
            let (sum, diff) = (xf + yf, xf - yf);
            if sum < tl.0 {
                tl = (sum, 0.0);
                tlp = [xf, yf];
            }
            if sum > br.0 {
                br = (sum, 0.0);
                brp = [xf, yf];
            }
            if diff > tr.0 {
                tr = (diff, 0.0);
                trp = [xf, yf];
            }
            if diff < bl.0 {
                bl = (diff, 0.0);
                blp = [xf, yf];
            }
        }
    }
    let quad = [tlp, trp, brp, blp];
    if quad_is_degenerate(&quad, w, h) {
        return None;
    }
    Some(quad)
}

/// Reject quads that aren't plausibly a rectangular page: near-zero side lengths, or a shape
/// so close to the full axis-aligned frame that warping would be a no-op (and risks amplifying
/// noise). Keeps genuine tilted-photo quads.
fn quad_is_degenerate(q: &Quad, w: usize, h: usize) -> bool {
    let side = |a: [f64; 2], b: [f64; 2]| ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt();
    let (top, right, bot, left) = (side(q[0], q[1]), side(q[1], q[2]), side(q[2], q[3]), side(q[3], q[0]));
    let min_side = (w.min(h) as f64) * 0.2;
    if top < min_side || right < min_side || bot < min_side || left < min_side {
        return true;
    }
    // How far the corners sit from the exact image rectangle, relative to its size. A quad that
    // is essentially the full frame (all corners within ~3% of the image corners) → no skew to fix.
    let corner_off = (q[0][0] + q[0][1])
        + ((w - 1) as f64 - q[1][0]) + q[1][1]
        + ((w - 1) as f64 - q[2][0]) + ((h - 1) as f64 - q[2][1])
        + q[3][0] + ((h - 1) as f64 - q[3][1]);
    corner_off < 0.03 * (w + h) as f64
}

/// Solve the perspective homography `H` (3×3, `h[8]=1`) mapping `from[i]` → `to[i]` for the four
/// correspondences, via the 8×8 Direct-Linear-Transform system with partial-pivot Gaussian
/// elimination. `None` if the system is singular (collinear points). To warp by *inverse*
/// mapping, call with `from = destination rectangle`, `to = source quad`.
pub(crate) fn solve_homography(from: &Quad, to: &Quad) -> Option<[f64; 9]> {
    let mut a = [[0.0f64; 9]; 8]; // augmented [8×8 | b]
    for i in 0..4 {
        let (u, v) = (from[i][0], from[i][1]);
        let (x, y) = (to[i][0], to[i][1]);
        a[2 * i] = [u, v, 1.0, 0.0, 0.0, 0.0, -u * x, -v * x, x];
        a[2 * i + 1] = [0.0, 0.0, 0.0, u, v, 1.0, -u * y, -v * y, y];
    }
    // Gaussian elimination with partial pivoting.
    for col in 0..8 {
        let piv = (col..8).max_by(|&r1, &r2| a[r1][col].abs().total_cmp(&a[r2][col].abs()))?;
        if a[piv][col].abs() < 1e-9 {
            return None;
        }
        a.swap(col, piv);
        let d = a[col][col];
        for c in col..9 {
            a[col][c] /= d;
        }
        for r in 0..8 {
            if r != col {
                let f = a[r][col];
                if f != 0.0 {
                    for c in col..9 {
                        a[r][c] -= f * a[col][c];
                    }
                }
            }
        }
    }
    let mut hm = [0.0f64; 9];
    for i in 0..8 {
        hm[i] = a[i][8];
    }
    hm[8] = 1.0;
    Some(hm)
}

/// Bilinear inverse warp: for each output pixel `(x,y)`, map through `h` (output→source) and
/// sample the source grayscale. Out-of-bounds samples read white (255 = paper), so a slightly
/// loose crop frames the page on white rather than black.
pub(crate) fn warp_perspective(gray: &[u8], w: usize, h: usize, hm: &[f64; 9], ow: usize, oh: usize) -> Vec<u8> {
    let mut out = vec![255u8; ow * oh];
    for y in 0..oh {
        for x in 0..ow {
            let (xf, yf) = (x as f64, y as f64);
            let denom = hm[6] * xf + hm[7] * yf + hm[8];
            if denom.abs() < 1e-12 {
                continue;
            }
            let sx = (hm[0] * xf + hm[1] * yf + hm[2]) / denom;
            let sy = (hm[3] * xf + hm[4] * yf + hm[5]) / denom;
            if sx < 0.0 || sy < 0.0 || sx > (w - 1) as f64 || sy > (h - 1) as f64 {
                continue;
            }
            let (x0, y0) = (sx.floor() as usize, sy.floor() as usize);
            let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
            let (fx, fy) = (sx - x0 as f64, sy - y0 as f64);
            let p = |xx: usize, yy: usize| gray[yy * w + xx] as f64;
            let top = p(x0, y0) * (1.0 - fx) + p(x1, y0) * fx;
            let bot = p(x0, y1) * (1.0 - fx) + p(x1, y1) * fx;
            out[y * ow + x] = (top * (1.0 - fy) + bot * fy).round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

/// Detect a photographed document and rectify it to a head-on rectangle. Returns the new
/// `(gray, width, height)`, or `None` when no distinct document quad is present (already a tidy
/// scan/page) — callers then keep the original image. Output size is the quad's own estimated
/// dimensions (longer of each opposing side pair), so no resolution is invented.
pub(crate) fn rectify_document(gray: &[u8], w: usize, h: usize) -> Option<(Vec<u8>, usize, usize)> {
    let q = detect_document_quad(gray, w, h)?;
    let dist = |a: [f64; 2], b: [f64; 2]| ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt();
    let ow = dist(q[0], q[1]).max(dist(q[3], q[2])).round() as usize;
    let oh = dist(q[0], q[3]).max(dist(q[1], q[2])).round() as usize;
    if ow < 16 || oh < 16 || ow > 4 * w || oh > 4 * h {
        return None;
    }
    // Destination rectangle corners (TL, TR, BR, BL); inverse-map output→source ⇒ from = rect.
    let rect: Quad = [[0.0, 0.0], [(ow - 1) as f64, 0.0], [(ow - 1) as f64, (oh - 1) as f64], [0.0, (oh - 1) as f64]];
    let hm = solve_homography(&rect, &q)?;
    Some((warp_perspective(gray, w, h, &hm, ow, oh), ow, oh))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn homography_maps_corners_exactly() {
        // A known perspective: unit-ish rectangle → an irregular quad. Each source corner must
        // map onto its target corner (the defining property of the solved homography).
        let from: Quad = [[0.0, 0.0], [100.0, 0.0], [100.0, 80.0], [0.0, 80.0]];
        let to: Quad = [[10.0, 12.0], [95.0, 5.0], [110.0, 90.0], [4.0, 78.0]];
        let hm = solve_homography(&from, &to).expect("non-singular");
        for i in 0..4 {
            let (u, v) = (from[i][0], from[i][1]);
            let d = hm[6] * u + hm[7] * v + hm[8];
            let x = (hm[0] * u + hm[1] * v + hm[2]) / d;
            let y = (hm[3] * u + hm[4] * v + hm[5]) / d;
            assert!((x - to[i][0]).abs() < 1e-6 && (y - to[i][1]).abs() < 1e-6, "corner {i} maps wrong");
        }
    }

    #[test]
    fn detects_tilted_bright_page_on_dark_background() {
        // Dark background (40) with a brighter (230) tilted quadrilateral "page". The detected
        // corners must land near the painted ones.
        let (w, h) = (120usize, 100usize);
        let mut gray = vec![40u8; w * h];
        let page: Quad = [[22.0, 14.0], [104.0, 24.0], [96.0, 88.0], [14.0, 78.0]];
        // Scanline-fill the convex quad.
        for y in 0..h {
            let mut xs = vec![];
            for e in 0..4 {
                let (a, b) = (page[e], page[(e + 1) % 4]);
                let (y0, y1) = (a[1], b[1]);
                if (y as f64 - y0) * (y as f64 - y1) <= 0.0 && (y1 - y0).abs() > 1e-6 {
                    xs.push(a[0] + (b[0] - a[0]) * (y as f64 - y0) / (y1 - y0));
                }
            }
            if xs.len() >= 2 {
                xs.sort_by(|p, q| p.total_cmp(q));
                for x in (xs[0].ceil() as usize)..=(xs[xs.len() - 1].floor() as usize).min(w - 1) {
                    gray[y * w + x] = 230;
                }
            }
        }
        let q = detect_document_quad(&gray, w, h).expect("page detected");
        for i in 0..4 {
            assert!((q[i][0] - page[i][0]).abs() <= 4.0 && (q[i][1] - page[i][1]).abs() <= 4.0,
                "corner {i} {:?} vs {:?}", q[i], page[i]);
        }
        // And it rectifies to a plausible upright rectangle.
        let (_out, ow, oh) = rectify_document(&gray, w, h).expect("rectified");
        assert!(ow > 60 && oh > 40, "rectified size {ow}x{oh}");
    }

    #[test]
    fn full_frame_scan_is_left_alone() {
        // A page that already fills the frame (uniform bright) has no distinct quad to crop.
        let (w, h) = (80usize, 60usize);
        let gray = vec![245u8; w * h];
        assert!(detect_document_quad(&gray, w, h).is_none(), "no crop on a full-frame scan");
        assert!(rectify_document(&gray, w, h).is_none());
    }
}
