//! Zero-dependency OCR — a Tesseract-style pipeline over a rasterized page.
//!
//! Stages: Otsu binarization → connected-component blobs → group into reading
//! lines → segment words by gaps → normalize each blob to the model's input and
//! classify it with the embedded CNN ([`super::ocr_model`], trained offline on
//! EMNIST handwriting + synthetic glyphs from thousands of fonts). Pure `std`:
//! the forward pass is a couple of int8 convolutions + max-pools + dense layers.
//!
//! Honest scope: strong on clean machine print and decent on tidy handwriting
//! (EMNIST-grade); noisy scans, dense layouts and unseen scripts are harder —
//! retrain `tools/train_ocr_cnn.py` with more data to improve, no runtime change.

use super::ocr_model as m;

/// A recognized word with its bounding box in **image pixels** (top-left origin).
#[derive(Debug, Clone)]
pub struct OcrWord {
    pub text: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// OCR output: the reconstructed text and the located words.
#[derive(Debug, Clone, Default)]
pub struct OcrResult {
    pub text: String,
    pub words: Vec<OcrWord>,
}

/// 2-D convolution, stride 1, zero-pad `(k-1)/2`, int8 weights + f32 bias, ReLU.
/// `w` is laid out `[out][in][ky][kx]` (PyTorch `Conv2d.weight` order); the
/// dequantized value is `w[..] as f32 * scale`. Output keeps the input H×W.
#[allow(clippy::too_many_arguments)]
/// A quantized weight element. Lets the conv/GRU/FC primitives run over **`i8`** (the lean
/// feature-baked models) or **`f32`** (full-precision host-loaded `.gpocr` blobs) from one
/// implementation. For `W = i8` this monomorphizes to byte-for-byte the previous code, so the
/// mono-glyph and feature-baked paths are unchanged; `f32` host models skip the lossy int8
/// quantization that collapsed recurrent non-Latin recognizers.
pub(crate) trait Qw: Copy {
    fn to_f32(self) -> f32;
}
impl Qw for i8 {
    #[inline]
    fn to_f32(self) -> f32 {
        self as f32
    }
}
impl Qw for f32 {
    #[inline]
    fn to_f32(self) -> f32 {
        self
    }
}

pub(crate) fn conv2d_relu<W: Qw>(
    inp: &[f32],
    in_ch: usize,
    ih: usize,
    iw: usize,
    w: &[W],
    scale: f32,
    bias: &[f32],
    out_ch: usize,
    k: usize,
) -> Vec<f32> {
    let pad = (k - 1) / 2;
    let mut out = vec![0f32; out_ch * ih * iw];
    for o in 0..out_ch {
        let b = bias[o];
        for y in 0..ih {
            for x in 0..iw {
                let mut acc = b;
                for i in 0..in_ch {
                    let wbase = (o * in_ch + i) * k * k;
                    let ibase = i * ih * iw;
                    for ky in 0..k {
                        let sy = y + ky;
                        if sy < pad || sy - pad >= ih {
                            continue;
                        }
                        let row = ibase + (sy - pad) * iw;
                        let wrow = wbase + ky * k;
                        for kx in 0..k {
                            let sx = x + kx;
                            if sx < pad || sx - pad >= iw {
                                continue;
                            }
                            let pix = inp[row + (sx - pad)];
                            if pix != 0.0 {
                                acc += pix * w[wrow + kx].to_f32() * scale;
                            }
                        }
                    }
                }
                out[o * ih * iw + y * iw + x] = acc.max(0.0);
            }
        }
    }
    out
}

/// 2×2 max-pool, stride 2 (floor). Returns `(data, out_h, out_w)`, channel-major.
pub(crate) fn maxpool2(inp: &[f32], ch: usize, h: usize, w: usize) -> (Vec<f32>, usize, usize) {
    let (oh, ow) = (h / 2, w / 2);
    let mut out = vec![0f32; ch * oh * ow];
    for c in 0..ch {
        let ibase = c * h * w;
        let obase = c * oh * ow;
        for y in 0..oh {
            for x in 0..ow {
                let (y0, x0) = (2 * y, 2 * x);
                let a = inp[ibase + y0 * w + x0];
                let b = inp[ibase + y0 * w + x0 + 1];
                let c2 = inp[ibase + (y0 + 1) * w + x0];
                let d = inp[ibase + (y0 + 1) * w + x0 + 1];
                out[obase + y * ow + x] = a.max(b).max(c2).max(d);
            }
        }
    }
    (out, oh, ow)
}

/// Fully-connected layer; `w` is `[out][in]` (PyTorch `Linear.weight`), int8 +
/// f32 bias. Applies ReLU when `relu` is set.
pub(crate) fn dense<W: Qw>(inp: &[f32], w: &[W], scale: f32, bias: &[f32], out: usize, relu: bool) -> Vec<f32> {
    let n = inp.len();
    let mut o = vec![0f32; out];
    for (j, slot) in o.iter_mut().enumerate() {
        let mut acc = bias[j];
        let wbase = j * n;
        for (i, &x) in inp.iter().enumerate() {
            if x != 0.0 {
                acc += x * w[wbase + i].to_f32() * scale;
            }
        }
        *slot = if relu { acc.max(0.0) } else { acc };
    }
    o
}

/// Classify a normalized `SIZE*SIZE` input (ink=1) as `1×SIZE×SIZE` → class
/// index + softmax confidence, via the embedded CNN.
fn classify(input: &[f32]) -> (usize, f32) {
    let c1 = conv2d_relu(
        input,
        1,
        m::SIZE,
        m::SIZE,
        &m::C1_W,
        m::C1_SCALE,
        &m::C1_B,
        m::C1_OUT,
        m::KERNEL,
    );
    let (p1, h1, w1) = maxpool2(&c1, m::C1_OUT, m::SIZE, m::SIZE);
    let c2 = conv2d_relu(
        &p1,
        m::C1_OUT,
        h1,
        w1,
        &m::C2_W,
        m::C2_SCALE,
        &m::C2_B,
        m::C2_OUT,
        m::KERNEL,
    );
    let (flat, _h2, _w2) = maxpool2(&c2, m::C2_OUT, h1, w1); // channel-major == FLAT
    let fc1 = dense(&flat, &m::F1_W, m::F1_SCALE, &m::F1_B, m::FC1, true);
    let logits = dense(&fc1, &m::F2_W, m::F2_SCALE, &m::F2_B, m::CLASSES, false);
    let (mut best, mut best_v) = (0usize, f32::NEG_INFINITY);
    for (c, &l) in logits.iter().enumerate() {
        if l > best_v {
            best_v = l;
            best = c;
        }
    }
    // Softmax confidence of the winner (numerically stable).
    let sum: f32 = logits.iter().map(|&l| (l - best_v).exp()).sum();
    (best, 1.0 / sum.max(1e-6))
}

/// Class index → character.
fn label(index: usize) -> char {
    m::LABELS.chars().nth(index).unwrap_or('?')
}

/// Otsu's global threshold over an 8-bit grayscale image.
pub(crate) fn otsu_threshold(gray: &[u8]) -> u8 {
    let mut hist = [0u32; 256];
    for &g in gray {
        hist[g as usize] += 1;
    }
    let total = gray.len() as f64;
    let sum: f64 = (0..256).map(|i| i as f64 * hist[i] as f64).sum();
    // Track the *plateau* of maximum between-class variance, not just its first
    // index: a perfectly bimodal image (empty bins between the two peaks) gives
    // a constant max over a whole range, and the first edge would exclude the
    // dark mode under `g < thresh`. Returning the plateau midpoint separates them.
    let (mut sum_b, mut w_b, mut max_var) = (0.0, 0.0, -1.0);
    let (mut t_lo, mut t_hi) = (128usize, 128usize);
    for (t, &count) in hist.iter().enumerate() {
        w_b += count as f64;
        if w_b == 0.0 {
            continue;
        }
        let w_f = total - w_b;
        if w_f == 0.0 {
            break;
        }
        sum_b += t as f64 * count as f64;
        let m_b = sum_b / w_b;
        let m_f = (sum - sum_b) / w_f;
        let var = w_b * w_f * (m_b - m_f) * (m_b - m_f);
        if var > max_var {
            max_var = var;
            t_lo = t;
            t_hi = t;
        } else if var >= max_var {
            // var == max_var (not strictly greater) → extend the plateau.
            t_hi = t;
        }
    }
    ((t_lo + t_hi) / 2) as u8
}

/// Sauvola adaptive binarization → ink mask (`true` = ink/dark). A per-pixel threshold
/// `t(x,y) = m·(1 + k·(s/R − 1))` over a local window (mean `m`, std `s`, `R=128`)
/// computed in O(1) per pixel via integral images of the sum and sum-of-squares.
/// Robust to uneven illumination / grey backgrounds where the global Otsu threshold
/// collapses (scanned/photographed documents). `radius` ≈ half the window (px).
pub(crate) fn sauvola_ink(gray: &[u8], w: usize, h: usize, radius: usize, k: f64) -> Vec<bool> {
    let iw = w + 1;
    // Integral images (prefix sums) of g and g² — u64 holds 255²·(2³²) comfortably.
    let mut isum = vec![0u64; iw * (h + 1)];
    let mut isq = vec![0u64; iw * (h + 1)];
    for y in 0..h {
        let (mut row_s, mut row_q) = (0u64, 0u64);
        for x in 0..w {
            let g = gray[y * w + x] as u64;
            row_s += g;
            row_q += g * g;
            isum[(y + 1) * iw + (x + 1)] = isum[y * iw + (x + 1)] + row_s;
            isq[(y + 1) * iw + (x + 1)] = isq[y * iw + (x + 1)] + row_q;
        }
    }
    let r = radius.max(1) as i64;
    let mut ink = vec![false; w * h];
    for y in 0..h {
        for x in 0..w {
            let x0 = (x as i64 - r).max(0) as usize;
            let y0 = (y as i64 - r).max(0) as usize;
            let x1 = ((x as i64 + r) as usize).min(w - 1);
            let y1 = ((y as i64 + r) as usize).min(h - 1);
            let area = ((x1 - x0 + 1) * (y1 - y0 + 1)) as f64;
            let rect = |img: &[u64]| -> f64 {
                (img[(y1 + 1) * iw + (x1 + 1)] + img[y0 * iw + x0]
                    - img[y0 * iw + (x1 + 1)]
                    - img[(y1 + 1) * iw + x0]) as f64
            };
            let mean = rect(&isum) / area;
            let var = (rect(&isq) / area - mean * mean).max(0.0);
            let t = mean * (1.0 + k * (var.sqrt() / 128.0 - 1.0));
            ink[y * w + x] = (gray[y * w + x] as f64) < t;
        }
    }
    ink
}

/// Coarse test for uneven illumination: the spread of mean brightness across a 4×4
/// grid of cells. A large spread ⇒ shadows / glare / paper gradient (photographed or
/// poorly-scanned pages) ⇒ worth flat-fielding; a small spread ⇒ already uniform
/// (clean scan/print) ⇒ skip, so the easy case is never perturbed.
pub(crate) fn illumination_is_uneven(gray: &[u8], w: usize, h: usize) -> bool {
    let (gx, gy) = (4usize, 4usize);
    let (mut lo, mut hi) = (u32::MAX, 0u32);
    for cy in 0..gy {
        for cx in 0..gx {
            let (x0, y0) = (cx * w / gx, cy * h / gy);
            let (x1, y1) = (((cx + 1) * w / gx).min(w), ((cy + 1) * h / gy).min(h));
            let (mut sum, mut n) = (0u64, 0u64);
            for y in y0..y1 {
                for x in x0..x1 {
                    sum += gray[y * w + x] as u64;
                    n += 1;
                }
            }
            if n == 0 {
                continue;
            }
            let mean = (sum / n) as u32;
            lo = lo.min(mean);
            hi = hi.max(mean);
        }
    }
    hi.saturating_sub(lo) > 35
}

/// Flat-field illumination correction: divide each pixel by its **local background**
/// (a large-window mean — the slowly-varying illumination field, since sparse text
/// averages out) and rescale to 8-bit, so shadows / glare / paper-gradients flatten to
/// a uniform bright background while local text contrast is preserved. O(1) per pixel
/// via an integral image (same trick as [`sauvola_ink`]). Near-identity on uniform
/// pages (bg/bg ≈ 1), so it only meaningfully changes unevenly-lit input — run it
/// behind [`illumination_is_uneven`] to stay byte-for-byte on clean scans.
pub(crate) fn normalize_illumination(gray: &[u8], w: usize, h: usize) -> Vec<u8> {
    let radius = (w.min(h) / 10).clamp(12, 64);
    let iw = w + 1;
    let mut isum = vec![0u64; iw * (h + 1)];
    for y in 0..h {
        let mut row = 0u64;
        for x in 0..w {
            row += gray[y * w + x] as u64;
            isum[(y + 1) * iw + (x + 1)] = isum[y * iw + (x + 1)] + row;
        }
    }
    let r = radius as i64;
    let mut out = vec![0u8; w * h];
    for y in 0..h {
        for x in 0..w {
            let x0 = (x as i64 - r).max(0) as usize;
            let y0 = (y as i64 - r).max(0) as usize;
            let x1 = ((x as i64 + r) as usize).min(w - 1);
            let y1 = ((y as i64 + r) as usize).min(h - 1);
            let area = ((x1 - x0 + 1) * (y1 - y0 + 1)) as u64;
            let s = isum[(y1 + 1) * iw + (x1 + 1)] + isum[y0 * iw + x0]
                - isum[y0 * iw + (x1 + 1)]
                - isum[(y1 + 1) * iw + x0];
            let bg = (s / area).max(1) as f32; // local paper / illumination level
            let v = gray[y * w + x] as f32 / bg * 255.0; // flat-field: local-white → 255
            out[y * w + x] = v.min(255.0) as u8;
        }
    }
    out
}

pub(crate) struct Blob {
    pub(crate) x0: usize,
    pub(crate) y0: usize,
    pub(crate) x1: usize,
    pub(crate) y1: usize,
}

impl Blob {
    pub(crate) fn w(&self) -> usize {
        self.x1 - self.x0 + 1
    }
    pub(crate) fn h(&self) -> usize {
        self.y1 - self.y0 + 1
    }
    pub(crate) fn cy(&self) -> f64 {
        (self.y0 + self.y1) as f64 / 2.0
    }
}

/// 8-connected components of the ink mask, returned as labelled bounding boxes.
/// `labels[i]` is the component id of pixel `i` (`-1` = background).
pub(crate) fn connected_components(ink: &[bool], w: usize, h: usize) -> (Vec<Blob>, Vec<i32>) {
    let mut labels = vec![-1i32; w * h];
    let mut blobs = Vec::new();
    let mut stack = Vec::new();
    for start in 0..w * h {
        if !ink[start] || labels[start] != -1 {
            continue;
        }
        let id = blobs.len() as i32;
        labels[start] = id;
        stack.push(start);
        let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0usize, 0usize);
        while let Some(p) = stack.pop() {
            let (px, py) = (p % w, p / w);
            x0 = x0.min(px);
            y0 = y0.min(py);
            x1 = x1.max(px);
            y1 = y1.max(py);
            for dy in -1i32..=1 {
                for dx in -1i32..=1 {
                    let nx = px as i32 + dx;
                    let ny = py as i32 + dy;
                    if nx < 0 || ny < 0 || nx >= w as i32 || ny >= h as i32 {
                        continue;
                    }
                    let q = ny as usize * w + nx as usize;
                    if ink[q] && labels[q] == -1 {
                        labels[q] = id;
                        stack.push(q);
                    }
                }
            }
        }
        blobs.push(Blob { x0, y0, x1, y1 });
    }
    (blobs, labels)
}

/// Normalize a blob's ink into a `SIZE*SIZE` input vector (scaled to `INK_BOX`,
/// centered, ink=1), sampling the component's own pixels (inverse mapping).
fn normalize(blob: &Blob, labels: &[i32], id: i32, w: usize, input: &mut [f32]) {
    input.iter_mut().for_each(|v| *v = 0.0);
    let (bw, bh) = (blob.w(), blob.h());
    let scale = m::INK_BOX as f64 / bw.max(bh) as f64;
    let (tw, th) = (
        (bw as f64 * scale).round().max(1.0),
        (bh as f64 * scale).round().max(1.0),
    );
    let ox = (m::SIZE as f64 - tw) / 2.0;
    let oy = (m::SIZE as f64 - th) / 2.0;
    for oyp in 0..th as usize {
        for oxp in 0..tw as usize {
            // Inverse map output cell → source blob pixel.
            let sx = blob.x0 + ((oxp as f64 + 0.5) / scale) as usize;
            let sy = blob.y0 + ((oyp as f64 + 0.5) / scale) as usize;
            if sx <= blob.x1 && sy <= blob.y1 && labels[sy * w + sx] == id {
                let dx = (ox as usize + oxp).min(m::SIZE - 1);
                let dy = (oy as usize + oyp).min(m::SIZE - 1);
                input[dy * m::SIZE + dx] = 1.0;
            }
        }
    }
}

/// Run OCR on an 8-bit grayscale image. `dark_text` true = ink is darker than
/// background (the usual scan); false inverts.
pub fn ocr(gray: &[u8], w: usize, h: usize) -> OcrResult {
    if w == 0 || h == 0 || gray.len() < w * h {
        return OcrResult::default();
    }
    // Front-end restoration, stage 1 — auto-crop: if the input is a *photo* of a page (a
    // document quadrilateral on a contrasting background), find its four corners and warp
    // it head-on to an axis-aligned rectangle. No-op on already-cropped scans/pages.
    let rectified = super::dewarp::rectify_document(gray, w, h);
    let (gray, w, h): (&[u8], usize, usize) = match &rectified {
        Some((g, ow, oh)) => (g, *ow, *oh),
        None => (gray, w, h),
    };
    // Stage 2 — illumination: flatten uneven lighting (shadows/glare) so the CRNN line
    // strips (which sample raw grayscale) and the mono-glyph binarization both see a
    // uniform bright page. Gated by `illumination_is_uneven` → byte-for-byte unchanged on
    // clean scans/print, so it can only help the hard case.
    let restored;
    let gray: &[u8] = if illumination_is_uneven(gray, w, h) {
        restored = normalize_illumination(gray, w, h);
        &restored
    } else {
        gray
    };
    // Line-level CRNN first when a per-script model is embedded (ocr-* features);
    // with none embedded this returns empty and we use the mono-glyph pipeline below.
    let crnn = super::ocr_crnn::recognize_enabled(gray, w, h);
    if !crnn.words.is_empty() {
        return crnn;
    }
    let thresh = otsu_threshold(gray);
    let ink: Vec<bool> = gray.iter().map(|&g| g < thresh).collect();
    let (blobs, labels) = connected_components(&ink, w, h);

    // Keep glyph-sized components; drop speckle and full-width rules/borders.
    let heights: Vec<usize> = blobs.iter().map(Blob::h).filter(|&v| v >= 4).collect();
    let med_h = median(&heights).max(6.0);
    let mut glyphs: Vec<usize> = (0..blobs.len())
        .filter(|&i| {
            let b = &blobs[i];
            let (bw, bh) = (b.w(), b.h());
            bh >= 4 && bh as f64 <= med_h * 3.0 && bw <= w * 3 / 4 && bw * bh >= 8
        })
        .collect();
    if glyphs.is_empty() {
        return OcrResult::default();
    }

    // Group into reading lines (top→bottom) by vertical centre.
    glyphs.sort_by(|&a, &b| blobs[a].cy().partial_cmp(&blobs[b].cy()).unwrap());
    let line_tol = med_h * 0.6;
    let mut lines: Vec<Vec<usize>> = Vec::new();
    let mut row_cy = f64::NEG_INFINITY;
    for &g in &glyphs {
        let cy = blobs[g].cy();
        if lines.is_empty() || (cy - row_cy).abs() > line_tol {
            lines.push(vec![g]);
            row_cy = cy;
        } else {
            lines.last_mut().unwrap().push(g);
        }
    }

    let mut input = vec![0f32; m::SIZE * m::SIZE];
    let mut result = OcrResult::default();
    let space_gap = med_h * 0.6;
    for line in &mut lines {
        line.sort_by(|&a, &b| blobs[a].x0.cmp(&blobs[b].x0));
        let mut word = String::new();
        let mut wx0 = f64::MAX;
        let mut wy0 = f64::MAX;
        let mut wx1 = 0.0f64;
        let mut wy1 = 0.0f64;
        let mut prev_x1: Option<usize> = None;
        let flush = |result: &mut OcrResult,
                     word: &mut String,
                     wx0: &mut f64,
                     wy0: &mut f64,
                     wx1: &mut f64,
                     wy1: &mut f64| {
            if !word.is_empty() {
                result.text.push_str(word);
                result.text.push(' ');
                result.words.push(OcrWord {
                    text: std::mem::take(word),
                    x: *wx0,
                    y: *wy0,
                    width: *wx1 - *wx0,
                    height: *wy1 - *wy0,
                });
                *wx0 = f64::MAX;
                *wy0 = f64::MAX;
                *wx1 = 0.0;
                *wy1 = 0.0;
            }
        };
        for &g in line.iter() {
            let b = &blobs[g];
            if let Some(px1) = prev_x1 {
                if b.x0 as f64 - px1 as f64 > space_gap {
                    flush(
                        &mut result,
                        &mut word,
                        &mut wx0,
                        &mut wy0,
                        &mut wx1,
                        &mut wy1,
                    );
                }
            }
            normalize(b, &labels, g as i32, w, &mut input);
            let (cls, _conf) = classify(&input);
            word.push(label(cls));
            wx0 = wx0.min(b.x0 as f64);
            wy0 = wy0.min(b.y0 as f64);
            wx1 = wx1.max(b.x1 as f64 + 1.0);
            wy1 = wy1.max(b.y1 as f64 + 1.0);
            prev_x1 = Some(b.x1);
        }
        flush(
            &mut result,
            &mut word,
            &mut wx0,
            &mut wy0,
            &mut wx1,
            &mut wy1,
        );
        // Newline between lines.
        if result.text.ends_with(' ') {
            result.text.pop();
        }
        result.text.push('\n');
    }
    result
}

pub(crate) fn median(values: &[usize]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v: Vec<usize> = values.to_vec();
    v.sort_unstable();
    let mid = v.len() / 2;
    if v.len().is_multiple_of(2) {
        (v[mid - 1] + v[mid]) as f64 / 2.0
    } else {
        v[mid] as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn otsu_separates_bimodal() {
        // Half black, half white → threshold between the modes.
        let mut g = vec![20u8; 50];
        g.extend(vec![230u8; 50]);
        let t = otsu_threshold(&g);
        assert!(t > 20 && t < 230);
    }

    #[test]
    fn sauvola_beats_global_threshold_on_uneven_illumination() {
        // Strong left→right illumination gradient (255 → ~105). "Text" is one pixel
        // locally ~60 darker than its background; a plain dark-background pixel must
        // NOT be ink. A single global threshold can't do both — Sauvola's local one can.
        let (w, h) = (40usize, 24usize);
        let mut gray = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w {
                gray[y * w + x] = (255 - x as i32 * 150 / (w as i32 - 1)).clamp(0, 255) as u8;
            }
        }
        let (tx, plain) = (34usize, 38usize);
        gray[12 * w + tx] = gray[12 * w + tx].saturating_sub(60); // local-dark "text"

        let ink = sauvola_ink(&gray, w, h, 6, 0.34);
        assert!(ink[12 * w + tx], "Sauvola flags the locally-dark text pixel");
        assert!(!ink[12 * w + plain], "Sauvola leaves the dark-background pixel as bg");
        // The global Otsu threshold misclassifies that same background pixel as ink.
        assert!(gray[12 * w + plain] < otsu_threshold(&gray), "global Otsu over-inks the dark bg");
    }

    #[test]
    fn illumination_normalization_flattens_gradient_preserving_text() {
        // Strong top→bottom illumination gradient (bright 240 → dark ~90) over uniform
        // paper, plus one locally-dark "text" pixel. Flat-fielding must equalize paper
        // across the gradient (shadows removed) while keeping the text clearly darker
        // than its surrounding paper — exactly what rescues a phone-photo of a page.
        let (w, h) = (60usize, 60usize);
        let mut gray = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w {
                gray[y * w + x] = (240 - y as i32 * 150 / (h as i32 - 1)).clamp(0, 255) as u8;
            }
        }
        let (tx, ty) = (30usize, 45usize);
        gray[ty * w + tx] = gray[ty * w + tx].saturating_sub(70); // local-dark "text"

        assert!(illumination_is_uneven(&gray, w, h), "the gradient must register as uneven");
        let out = normalize_illumination(&gray, w, h);
        // Paper at the bright top and dark bottom flattens to near-equal bright values.
        let (top, bot) = (out[5 * w + 30] as i32, out[55 * w + 30] as i32);
        assert!((top - bot).abs() < 25, "flat-field equalizes paper across the gradient (top={top} bot={bot})");
        assert!(top > 200 && bot > 200, "paper flattens toward white (top={top} bot={bot})");
        // The text pixel stays markedly darker than the surrounding flattened paper.
        let text = out[ty * w + tx] as i32;
        assert!(text < bot - 40, "text contrast preserved (text={text} paper≈{bot})");
    }

    #[test]
    fn illumination_normalization_skipped_on_uniform_page() {
        // A clean uniform page is NOT flagged uneven → the front-end is a no-op (no
        // train/inference skew introduced on the easy case).
        let (w, h) = (40usize, 40usize);
        let mut gray = vec![245u8; w * h];
        gray[20 * w + 20] = 40; // a glyph pixel
        assert!(!illumination_is_uneven(&gray, w, h), "uniform page must not be flagged uneven");
    }

    #[test]
    fn connected_components_counts_blobs() {
        // Two separate 2x2 ink squares on a 10x4 canvas.
        let (w, h) = (10usize, 4usize);
        let mut ink = vec![false; w * h];
        for (y, x) in [
            (1, 1),
            (1, 2),
            (2, 1),
            (2, 2),
            (1, 6),
            (1, 7),
            (2, 6),
            (2, 7),
        ] {
            ink[y * w + x] = true;
        }
        let (blobs, _) = connected_components(&ink, w, h);
        assert_eq!(blobs.len(), 2);
        assert_eq!(blobs[0].w(), 2);
    }

    #[test]
    fn model_dimensions_are_consistent() {
        // Conv weights: [out][in][k][k]; conv1 has a single input channel.
        assert_eq!(m::C1_W.len(), m::C1_OUT * m::KERNEL * m::KERNEL);
        assert_eq!(m::C2_W.len(), m::C2_OUT * m::C1_OUT * m::KERNEL * m::KERNEL);
        // Dense weights: [out][in].
        assert_eq!(m::F1_W.len(), m::FC1 * m::FLAT);
        assert_eq!(m::F2_W.len(), m::CLASSES * m::FC1);
        // Biases.
        assert_eq!(m::C1_B.len(), m::C1_OUT);
        assert_eq!(m::C2_B.len(), m::C2_OUT);
        assert_eq!(m::F1_B.len(), m::FC1);
        assert_eq!(m::F2_B.len(), m::CLASSES);
        // Two 2× pools shrink SIZE→SIZE/4; FLAT is channel-major over that map.
        assert_eq!(m::FLAT, m::C2_OUT * (m::SIZE / 4) * (m::SIZE / 4));
        assert_eq!(m::LABELS.chars().count(), m::CLASSES);
    }

    #[test]
    fn classify_returns_valid_class() {
        // A blank input must still yield an in-range class index + finite conf.
        let input = vec![0f32; m::SIZE * m::SIZE];
        let (cls, conf) = classify(&input);
        assert!(cls < m::CLASSES);
        assert!(conf.is_finite() && (0.0..=1.0).contains(&conf));
    }
}
