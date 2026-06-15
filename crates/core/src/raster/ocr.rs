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
fn conv2d_relu(
    inp: &[f32],
    in_ch: usize,
    ih: usize,
    iw: usize,
    w: &[i8],
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
                                acc += pix * (w[wrow + kx] as f32) * scale;
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
fn maxpool2(inp: &[f32], ch: usize, h: usize, w: usize) -> (Vec<f32>, usize, usize) {
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
fn dense(inp: &[f32], w: &[i8], scale: f32, bias: &[f32], out: usize, relu: bool) -> Vec<f32> {
    let n = inp.len();
    let mut o = vec![0f32; out];
    for (j, slot) in o.iter_mut().enumerate() {
        let mut acc = bias[j];
        let wbase = j * n;
        for (i, &x) in inp.iter().enumerate() {
            if x != 0.0 {
                acc += x * (w[wbase + i] as f32) * scale;
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
fn otsu_threshold(gray: &[u8]) -> u8 {
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

struct Blob {
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
}

impl Blob {
    fn w(&self) -> usize {
        self.x1 - self.x0 + 1
    }
    fn h(&self) -> usize {
        self.y1 - self.y0 + 1
    }
    fn cy(&self) -> f64 {
        (self.y0 + self.y1) as f64 / 2.0
    }
}

/// 8-connected components of the ink mask, returned as labelled bounding boxes.
/// `labels[i]` is the component id of pixel `i` (`-1` = background).
fn connected_components(ink: &[bool], w: usize, h: usize) -> (Vec<Blob>, Vec<i32>) {
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

fn median(values: &[usize]) -> f64 {
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
