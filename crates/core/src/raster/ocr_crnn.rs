//! Zero-dependency line-level OCR — a CRNN + CTC recognizer (pure `std`).
//!
//! Where [`super::ocr`] classifies one connected-component glyph at a time (which
//! fails on touching/cursive scripts and noisy scans), this module recognizes a
//! whole text **line** as a sequence — the Tesseract-4/5 paradigm:
//!
//! ```text
//! line strip (1×H×W, ink=1)
//!   → int8 conv stack + 2×2 max-pools      (reused from ocr.rs)
//!   → collapse the remaining rows           → sequence of W' feature vectors
//!   → bidirectional GRU                      → per-step context
//!   → per-step dense → logits over (alphabet + blank)
//!   → CTC greedy decode                      → text
//! ```
//!
//! The forward pass stays **pure `std` int8**: conv/pool/dense come from `ocr.rs`,
//! the GRU is plain matvec + `sigmoid`/`tanh`, and CTC greedy is argmax-collapse —
//! no ML dependency ships. A model is just a [`Crnn`] view over embedded int8
//! statics (emitted offline by `tools/train_ocr_crnn.py`, one file per script
//! group: Latin/Cyrillic/Greek, CJK, Arabic/Hebrew, Indic). [`recognize`] routes
//! each line to the model with the highest mean confidence and reverses RTL output.
//!
//! The high-level entry [`recognize_enabled`] is wired into [`super::ocr::ocr`] and
//! routes each line to the per-script models embedded via the `ocr-*` Cargo features
//! (empty by default → the caller falls back to the mono-glyph classifier); the
//! numerically-exact primitives (GRU cell, CTC decode) are unit-tested here.

use super::ocr::{
    conv2d_relu, connected_components, dense, maxpool2, sauvola_ink, OcrResult, OcrWord,
};

/// Height (rows) every line strip is normalized to before the conv stack.
pub(crate) const STRIP_H: usize = 32;

/// A normalized line strip: ink pixels (`1×STRIP_H×width`, ink=1), its `width`, and
/// the source line's `(x0, y0, x1, y1)` bounding box in image pixels.
type LineStrip = (Vec<f32>, usize, (usize, usize, usize, usize));

/// One int8 convolution layer (3×3 same-pad, ReLU), each followed by a 2×2 pool.
/// `w` is `[out][in][3][3]` (PyTorch `Conv2d.weight` order); value = `w as f32 * scale`.
pub(crate) struct ConvSpec<'a> {
    pub w: &'a [i8],
    pub scale: f32,
    pub b: &'a [f32],
    pub in_ch: usize,
    pub out_ch: usize,
}

/// One int8 GRU direction. Input weights `w*` are `[hid][in]`, recurrent `u*` are
/// `[hid][hid]` (PyTorch order); biases are f32; the hidden state stays f32.
pub(crate) struct GruSpec<'a> {
    pub wz: &'a [i8],
    pub wr: &'a [i8],
    pub wn: &'a [i8],
    pub uz: &'a [i8],
    pub ur: &'a [i8],
    pub un: &'a [i8],
    pub w_scale: f32,
    pub u_scale: f32,
    pub bz: &'a [f32],
    pub br: &'a [f32],
    pub bn: &'a [f32],
}

/// A full CRNN line recognizer — an inference-only view over embedded int8 statics.
pub(crate) struct Crnn<'a> {
    /// Input strip height; must equal [`STRIP_H`] for the trained model.
    pub h: usize,
    /// Conv layers, each followed by a 2×2 max-pool (height shrinks by 2^N).
    pub conv: &'a [ConvSpec<'a>],
    /// Sequence feature dim (= last conv `out_ch`, = GRU input size).
    pub gru_in: usize,
    /// Hidden units per GRU direction.
    pub gru_hid: usize,
    pub fwd: GruSpec<'a>,
    pub bwd: GruSpec<'a>,
    /// Output projection `[K+1][2*gru_hid]` over the alphabet + a trailing blank.
    pub fc_w: &'a [i8],
    pub fc_scale: f32,
    pub fc_b: &'a [f32],
    /// `K` characters; the CTC blank is the implicit index `K`.
    pub alphabet: &'a str,
    /// Reverse decoded order for right-to-left scripts (Arabic, Hebrew).
    pub rtl: bool,
}

#[inline]
fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

#[inline]
fn argmax(v: &[f32]) -> usize {
    let mut bi = 0usize;
    let mut bv = f32::NEG_INFINITY;
    for (i, &x) in v.iter().enumerate() {
        if x > bv {
            bv = x;
            bi = i;
        }
    }
    bi
}

/// One GRU step (int8 weights, f32 state): `h' = (1−z)⊙n + z⊙h`, with
/// `z = σ(Wz x + Uz h)`, `r = σ(Wr x + Ur h)`, `n = tanh(Wn x + Un (r⊙h))`.
fn gru_cell(g: &GruSpec, x: &[f32], hprev: &[f32], hid: usize, inn: usize) -> Vec<f32> {
    // Pre-activation of one gate row `j`, given the recurrent vector `rec`.
    let gate = |w: &[i8], u: &[i8], b: &[f32], j: usize, rec: &[f32]| -> f32 {
        let mut acc = b[j];
        let wb = j * inn;
        for i in 0..inn {
            let wv = w[wb + i] as f32;
            if wv != 0.0 {
                acc += wv * g.w_scale * x[i];
            }
        }
        let ub = j * hid;
        for k in 0..hid {
            let uv = u[ub + k] as f32;
            if uv != 0.0 {
                acc += uv * g.u_scale * rec[k];
            }
        }
        acc
    };
    let mut z = vec![0f32; hid];
    let mut r = vec![0f32; hid];
    for j in 0..hid {
        z[j] = sigmoid(gate(g.wz, g.uz, g.bz, j, hprev));
        r[j] = sigmoid(gate(g.wr, g.ur, g.br, j, hprev));
    }
    let rh: Vec<f32> = (0..hid).map(|k| r[k] * hprev[k]).collect();
    let mut hnew = vec![0f32; hid];
    for j in 0..hid {
        let n = gate(g.wn, g.un, g.bn, j, &rh).tanh();
        hnew[j] = (1.0 - z[j]) * n + z[j] * hprev[j];
    }
    hnew
}

/// CTC greedy decode: argmax per timestep → collapse runs of equal indices → drop
/// the blank (index `K`). Repeated labels separated by a blank are kept distinct.
fn ctc_greedy_decode(logits: &[Vec<f32>], alphabet: &str, rtl: bool) -> String {
    let chars: Vec<char> = alphabet.chars().collect();
    let blank = chars.len();
    let mut prev = blank; // start "at blank" so the first real label emits
    let mut s = String::new();
    for l in logits {
        let idx = argmax(l);
        if idx != prev && idx != blank && idx < chars.len() {
            s.push(chars[idx]);
        }
        prev = idx;
    }
    if rtl {
        s.chars().rev().collect()
    } else {
        s
    }
}

/// Homoglyph confusion sets: `(Latin, Cyrillic, Greek)` glyphs that render near-
/// identically (`'\0'` = the script has no member). A multi-script model can't tell
/// `A`/`Α`/`А` apart without context; [`disambiguate_line`] uses these to snap a word's
/// ambiguous letters to the script its **unambiguous** letters vote for — a lexicon-lite
/// step that removes the main residual error vs Tesseract.
const CONFUSE: &[(char, char, char)] = &[
    ('A', '\u{0410}', '\u{0391}'), ('B', '\u{0412}', '\u{0392}'), ('C', '\u{0421}', '\0'),
    ('E', '\u{0415}', '\u{0395}'), ('H', '\u{041D}', '\u{0397}'), ('I', '\u{0406}', '\u{0399}'),
    ('J', '\u{0408}', '\0'), ('K', '\u{041A}', '\u{039A}'), ('M', '\u{041C}', '\u{039C}'),
    ('N', '\0', '\u{039D}'), ('O', '\u{041E}', '\u{039F}'), ('P', '\u{0420}', '\u{03A1}'),
    ('S', '\u{0405}', '\0'), ('T', '\u{0422}', '\u{03A4}'), ('X', '\u{0425}', '\u{03A7}'),
    ('Y', '\u{0423}', '\u{03A5}'), ('Z', '\0', '\u{0396}'),
    ('a', '\u{0430}', '\u{03B1}'), ('c', '\u{0441}', '\0'), ('e', '\u{0435}', '\0'),
    ('i', '\u{0456}', '\u{03B9}'), ('j', '\u{0458}', '\0'), ('o', '\u{043E}', '\u{03BF}'),
    ('p', '\u{0440}', '\u{03C1}'), ('s', '\u{0455}', '\0'), ('x', '\u{0445}', '\u{03C7}'),
    ('y', '\u{0443}', '\0'),
];

/// Script of a character: 1 = Latin, 2 = Cyrillic, 3 = Greek, 0 = other/neutral.
fn script_of(c: char) -> u8 {
    match c as u32 {
        0x0041..=0x005A | 0x0061..=0x007A | 0x00C0..=0x024F | 0x1E00..=0x1EFF => 1,
        0x0400..=0x04FF => 2,
        0x0370..=0x03FF | 0x1F00..=0x1FFF => 3,
        _ => 0,
    }
}

/// The confusion row a glyph belongs to, if it is an ambiguous homoglyph.
fn confuse_row(c: char) -> Option<&'static (char, char, char)> {
    CONFUSE
        .iter()
        .find(|r| c == r.0 || (r.1 != '\0' && c == r.1) || (r.2 != '\0' && c == r.2))
}

/// Snap a token's ambiguous homoglyphs to the script its unambiguous letters vote for.
fn disambiguate_word(w: &str) -> String {
    let mut votes = [0usize; 4];
    for c in w.chars() {
        let s = script_of(c);
        if s != 0 && confuse_row(c).is_none() {
            votes[s as usize] += 1; // an unambiguous member of its script
        }
    }
    let target = (1usize..=3).max_by_key(|&s| votes[s]).unwrap_or(1);
    if votes[target] == 0 {
        return w.to_string(); // no script signal → leave the token untouched
    }
    w.chars()
        .map(|c| match confuse_row(c) {
            Some(r) => {
                let repr = match target { 1 => r.0, 2 => r.1, _ => r.2 };
                if repr != '\0' { repr } else { c }
            }
            None => c,
        })
        .collect()
}

/// Apply [`disambiguate_word`] to each space-separated token of a recognized line.
fn disambiguate_line(line: &str) -> String {
    line.split(' ').map(disambiguate_word).collect::<Vec<_>>().join(" ")
}

/// Recognize one normalized line strip (`1×h×w`, ink=1) → `(text, mean confidence)`.
fn recognize_line(m: &Crnn, strip: &[f32], w: usize) -> (String, f32) {
    let mut data = strip.to_vec();
    let (mut ih, mut iw) = (m.h, w);
    for layer in m.conv {
        let c = conv2d_relu(
            &data, layer.in_ch, ih, iw, layer.w, layer.scale, layer.b, layer.out_ch, 3,
        );
        let (p, oh, ow) = maxpool2(&c, layer.out_ch, ih, iw);
        data = p;
        ih = oh;
        iw = ow;
    }
    let t_len = iw;
    if t_len == 0 || ih == 0 {
        return (String::new(), 0.0);
    }
    // Collapse the remaining rows → a length-`t_len` sequence of `gru_in` features
    // (the last conv's output channels).
    let feat = m.gru_in;
    let mut seq: Vec<Vec<f32>> = Vec::with_capacity(t_len);
    for t in 0..t_len {
        let mut v = vec![0f32; feat];
        for (c, slot) in v.iter_mut().enumerate() {
            let mut acc = 0f32;
            for y in 0..ih {
                acc += data[c * ih * iw + y * iw + t];
            }
            *slot = acc / ih as f32;
        }
        seq.push(v);
    }
    // Bidirectional GRU.
    let hid = m.gru_hid;
    let mut hf = vec![vec![0f32; hid]; t_len];
    let mut h = vec![0f32; hid];
    for t in 0..t_len {
        h = gru_cell(&m.fwd, &seq[t], &h, hid, m.gru_in);
        hf[t] = h.clone();
    }
    let mut hb = vec![vec![0f32; hid]; t_len];
    let mut h = vec![0f32; hid];
    for t in (0..t_len).rev() {
        h = gru_cell(&m.bwd, &seq[t], &h, hid, m.gru_in);
        hb[t] = h.clone();
    }
    // Per-timestep projection + CTC.
    let k = m.alphabet.chars().count();
    let mut logits: Vec<Vec<f32>> = Vec::with_capacity(t_len);
    let mut conf_sum = 0f32;
    for t in 0..t_len {
        let mut ctx = hf[t].clone();
        ctx.extend_from_slice(&hb[t]);
        let l = dense(&ctx, m.fc_w, m.fc_scale, m.fc_b, k + 1, false);
        let mx = l.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        let sum: f32 = l.iter().map(|&v| (v - mx).exp()).sum();
        conf_sum += 1.0 / sum.max(1e-6); // softmax prob of the winning class
        logits.push(l);
    }
    let text = ctc_greedy_decode(&logits, m.alphabet, m.rtl);
    (text, conf_sum / t_len as f32)
}

/// Rotate a grayscale image about its centre by `angle` rad (bilinear; out-of-bounds →
/// white). Used to deskew a page before line extraction.
fn rotate_gray(gray: &[u8], w: usize, h: usize, angle: f64) -> Vec<u8> {
    let (cx, cy) = (w as f64 / 2.0, h as f64 / 2.0);
    let (s, c) = angle.sin_cos();
    let mut out = vec![255u8; w * h];
    for y in 0..h {
        for x in 0..w {
            let (dx, dy) = (x as f64 - cx, y as f64 - cy);
            let sx = cx + dx * c + dy * s; // inverse map: output → source
            let sy = cy - dx * s + dy * c;
            if sx < 0.0 || sy < 0.0 || sx >= (w - 1) as f64 || sy >= (h - 1) as f64 {
                continue; // leave white
            }
            let (x0, y0) = (sx as usize, sy as usize);
            let (fx, fy) = (sx - x0 as f64, sy - y0 as f64);
            let p = |xx: usize, yy: usize| gray[yy * w + xx] as f64;
            let top = p(x0, y0) * (1.0 - fx) + p(x0 + 1, y0) * fx;
            let bot = p(x0, y0 + 1) * (1.0 - fx) + p(x0 + 1, y0 + 1) * fx;
            out[y * w + x] = (top * (1.0 - fy) + bot * fy).round() as u8;
        }
    }
    out
}

/// Estimate text skew (radians) as the shear that maximizes the sharpness (sum of
/// squared row-sums) of the horizontal ink projection — small range, coarse step.
fn estimate_skew(ink: &[bool], w: usize, h: usize) -> f64 {
    let (mut best_angle, mut best_score) = (0.0f64, -1.0f64);
    let cx = w as f64 / 2.0;
    for i in -10i32..=10 {
        let angle = i as f64 * 0.01; // ±0.1 rad ≈ ±5.7°
        let t = angle.tan();
        let mut prof = vec![0u32; h];
        for y in 0..h {
            let row = y * w;
            for x in 0..w {
                if ink[row + x] {
                    // Shear about the centre column so a flat band stays put at t≈0
                    // (an off-centre shear + floor would spuriously concentrate it).
                    let yy = y as f64 - (x as f64 - cx) * t;
                    if yy >= 0.0 && (yy as usize) < h {
                        prof[yy as usize] += 1;
                    }
                }
            }
        }
        let score: f64 = prof.iter().map(|&v| (v as f64) * (v as f64)).sum();
        if score > best_score {
            best_score = score;
            best_angle = angle;
        }
    }
    best_angle
}

/// Drop connected components smaller than `min_px` pixels from the ink mask
/// (salt-and-pepper despeckle); glyphs are far larger and are kept.
fn despeckle(ink: &mut [bool], w: usize, h: usize, min_px: usize) {
    let (_blobs, labels) = connected_components(ink, w, h);
    let n = labels.iter().filter(|&&l| l >= 0).map(|&l| l as usize + 1).max().unwrap_or(0);
    if n == 0 {
        return;
    }
    let mut counts = vec![0usize; n];
    for &l in &labels {
        if l >= 0 {
            counts[l as usize] += 1;
        }
    }
    for (i, slot) in ink.iter_mut().enumerate() {
        if *slot && labels[i] >= 0 && counts[labels[i] as usize] < min_px {
            *slot = false;
        }
    }
}

/// Extract reading-order line strips from a grayscale page via a **horizontal
/// projection profile**: binarize → ink-count per row → contiguous row bands (small
/// intra-line gaps merged) = text lines → crop each band's ink bbox and scale to
/// height [`STRIP_H`], sampling grayscale ink intensity (dark text → 1.0) to match the
/// antialiased strips the trainer uses. Returns `(strip, width, (x0,y0,x1,y1))` in
/// image pixels. Projection bands are far more robust than per-blob center grouping,
/// which over-splits one line on ascenders/descenders/diacritics.
fn extract_line_strips(gray: &[u8], w: usize, h: usize) -> Vec<LineStrip> {
    let mut out = Vec::new();
    if w == 0 || h == 0 || gray.len() < w * h {
        return out;
    }
    // Front-end: adaptive binarization (Sauvola) → deskew (projection-variance) →
    // despeckle. Robust to uneven illumination, page tilt and salt-noise on real scans;
    // all no-ops on clean print (skew ≈ 0 ⇒ no rotation). Strips sample raw grayscale.
    let radius = (h / 50).clamp(8, 24);
    let ink0 = sauvola_ink(gray, w, h, radius, 0.34);
    let angle = estimate_skew(&ink0, w, h);
    let deskewed = if angle.abs() > 0.012 { Some(rotate_gray(gray, w, h, -angle)) } else { None };
    let gview: &[u8] = deskewed.as_deref().unwrap_or(gray);
    let mut ink = match &deskewed {
        Some(rg) => sauvola_ink(rg, w, h, radius, 0.34),
        None => ink0,
    };
    despeckle(&mut ink, w, h, 3);
    // Ink pixels per row → text-line bands.
    let mut row_ink = vec![0u32; h];
    for (y, slot) in row_ink.iter_mut().enumerate() {
        *slot = (0..w).filter(|&x| ink[y * w + x]).count() as u32;
    }
    let row_thr = 1u32.max(w as u32 / 200); // a couple of ink px qualifies a row
    let max_gap = (h / 100).max(2); // bridge small vertical gaps within a line
    let mut bands: Vec<(usize, usize)> = Vec::new();
    let mut y = 0usize;
    while y < h {
        if row_ink[y] <= row_thr {
            y += 1;
            continue;
        }
        let (y0, mut y1, mut gap) = (y, y, 0usize);
        y += 1;
        while y < h {
            if row_ink[y] > row_thr {
                y1 = y;
                gap = 0;
            } else {
                gap += 1;
                if gap > max_gap {
                    break;
                }
            }
            y += 1;
        }
        bands.push((y0, y1));
    }
    for (y0, y1) in bands {
        let bh = y1 - y0 + 1;
        if bh < 6 {
            continue; // too thin to be a text line
        }
        // Horizontal ink extent of this band.
        let (mut x0, mut x1) = (w, 0usize);
        for yy in y0..=y1 {
            let base = yy * w;
            for x in 0..w {
                if ink[base + x] {
                    x0 = x0.min(x);
                    x1 = x1.max(x);
                }
            }
        }
        if x1 < x0 {
            continue;
        }
        let lw = x1 - x0 + 1;
        let scale = STRIP_H as f64 / bh as f64;
        let sw = ((lw as f64 * scale).round() as usize).max(1);
        let mut strip = vec![0f32; STRIP_H * sw];
        for oy in 0..STRIP_H {
            for ox in 0..sw {
                let sy = y0 + ((oy as f64 + 0.5) / scale) as usize;
                let sx = x0 + ((ox as f64 + 0.5) / scale) as usize;
                if sy <= y1 && sx <= x1 && sy < h && sx < w {
                    strip[oy * sw + ox] = (255 - gview[sy * w + sx] as u16) as f32 / 255.0;
                }
            }
        }
        out.push((strip, sw, (x0, y0, x1 + 1, y1 + 1)));
    }
    out
}

/// Run the line-level recognizer over a grayscale page. Each line is routed to the
/// `models` entry with the highest mean confidence (per-script selection); one
/// [`OcrWord`] is emitted per line (box in image pixels). Empty if `models` is empty
/// (no per-script model embedded for this build) — callers fall back to
/// [`super::ocr::ocr`].
pub(crate) fn recognize(gray: &[u8], w: usize, h: usize, models: &[&Crnn]) -> OcrResult {
    let mut res = OcrResult::default();
    if models.is_empty() {
        return res;
    }
    for (strip, sw, bx) in extract_line_strips(gray, w, h) {
        let mut best = (String::new(), f32::NEG_INFINITY);
        for m in models {
            let (t, c) = recognize_line(m, &strip, sw);
            if c > best.1 {
                best = (t, c);
            }
        }
        let text = disambiguate_line(&best.0); // snap homoglyphs to a consistent script
        if !text.is_empty() {
            res.text.push_str(&text);
            res.text.push('\n');
            res.words.push(OcrWord {
                text,
                x: bx.0 as f64,
                y: bx.1 as f64,
                width: (bx.2 - bx.0) as f64,
                height: (bx.3 - bx.1) as f64,
            });
        }
    }
    res
}

/// Per-script CRNN models embedded in this build (one per enabled `ocr-*` feature).
/// Empty when none are enabled — the default build.
fn enabled_models() -> Vec<Crnn<'static>> {
    #[allow(unused_mut)]
    let mut v: Vec<Crnn<'static>> = Vec::new();
    #[cfg(feature = "ocr-alpha")]
    v.push(super::ocr_model_alpha::model());
    // future groups: ocr-cjk, ocr-arabic, ocr-deva, ocr-beng, ocr-taml
    v
}

// ── host-loaded models (runtime blob — keeps the .wasm lean) ─────────────────
// Instead of baking int8 weights into the binary (feature-gated, above), a model
// can be supplied at runtime as a compact `.gpocr` blob — the host loads it like a
// font (WASM export `gp_ocr_load_model`). The core stays ~540 KB; advanced OCR is
// opt-in at runtime. `tools/train_ocr_crnn.py` emits the matching format.

struct LoadedConv {
    w: Vec<i8>,
    scale: f32,
    b: Vec<f32>,
    in_ch: usize,
    out_ch: usize,
}

struct LoadedGru {
    wz: Vec<i8>,
    wr: Vec<i8>,
    wn: Vec<i8>,
    uz: Vec<i8>,
    ur: Vec<i8>,
    un: Vec<i8>,
    w_scale: f32,
    u_scale: f32,
    bz: Vec<f32>,
    br: Vec<f32>,
    bn: Vec<f32>,
}

/// A CRNN line model that **owns** its weights (parsed from a `.gpocr` blob) so it can
/// live in the runtime registry; it borrows itself into a transient [`Crnn`] view for
/// inference (no data copy).
struct LoadedModel {
    h: usize,
    gru_in: usize,
    gru_hid: usize,
    rtl: bool,
    alphabet: String,
    conv: Vec<LoadedConv>,
    fwd: LoadedGru,
    bwd: LoadedGru,
    fc_w: Vec<i8>,
    fc_scale: f32,
    fc_b: Vec<f32>,
}

const GPOCR_MAGIC: &[u8; 4] = b"GPO1";

/// Bounds-checked little-endian forward byte cursor.
struct Cur<'a> {
    b: &'a [u8],
    i: usize,
}
impl<'a> Cur<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.i..self.i.checked_add(n)?)?;
        self.i += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u16(&mut self) -> Option<usize> {
        let s = self.take(2)?;
        Some(u16::from_le_bytes([s[0], s[1]]) as usize)
    }
    fn u32(&mut self) -> Option<usize> {
        let s = self.take(4)?;
        Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as usize)
    }
    fn f32(&mut self) -> Option<f32> {
        let s = self.take(4)?;
        Some(f32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn i8s(&mut self, n: usize) -> Option<Vec<i8>> {
        Some(self.take(n)?.iter().map(|&b| b as i8).collect())
    }
    fn f32s(&mut self, n: usize) -> Option<Vec<f32>> {
        let s = self.take(n.checked_mul(4)?)?;
        Some((0..n).map(|k| f32::from_le_bytes([s[4 * k], s[4 * k + 1], s[4 * k + 2], s[4 * k + 3]])).collect())
    }
}

/// Build a transient [`GruSpec`] borrowing an owned direction.
fn mk_gru(g: &LoadedGru) -> GruSpec<'_> {
    GruSpec {
        wz: &g.wz, wr: &g.wr, wn: &g.wn,
        uz: &g.uz, ur: &g.ur, un: &g.un,
        w_scale: g.w_scale, u_scale: g.u_scale,
        bz: &g.bz, br: &g.br, bn: &g.bn,
    }
}

impl LoadedModel {
    /// Parse a `.gpocr` blob; None on any structural mismatch. Layout (LE): magic
    /// `GPO1`, u8 rtl, u16 h/gru_in/gru_hid, u32 alphabet_len + UTF-8, u8 n_conv,
    /// per-conv {u16 in_ch, u16 out_ch, f32 scale, i8[out·in·9] w, f32[out] b}, two GRU
    /// dirs {f32 w_scale, f32 u_scale, i8 wz/wr/wn[hid·in], i8 uz/ur/un[hid·hid],
    /// f32 bz/br/bn[hid]}, fc {f32 scale, i8[(K+1)·2hid] w, f32[K+1] b}.
    fn from_bytes(bytes: &[u8]) -> Option<LoadedModel> {
        let mut c = Cur { b: bytes, i: 0 };
        if c.take(4)? != GPOCR_MAGIC {
            return None;
        }
        let rtl = c.u8()? != 0;
        let (h, gru_in, gru_hid) = (c.u16()?, c.u16()?, c.u16()?);
        let alen = c.u32()?;
        let alphabet = core::str::from_utf8(c.take(alen)?).ok()?.to_string();
        let k = alphabet.chars().count();
        let n_conv = c.u8()? as usize;
        let mut conv = Vec::with_capacity(n_conv);
        for _ in 0..n_conv {
            let (in_ch, out_ch) = (c.u16()?, c.u16()?);
            let scale = c.f32()?;
            let w = c.i8s(out_ch.checked_mul(in_ch)?.checked_mul(9)?)?;
            let b = c.f32s(out_ch)?;
            conv.push(LoadedConv { w, scale, b, in_ch, out_ch });
        }
        let read_gru = |c: &mut Cur| -> Option<LoadedGru> {
            let (w_scale, u_scale) = (c.f32()?, c.f32()?);
            let (wi, ui) = (gru_hid.checked_mul(gru_in)?, gru_hid.checked_mul(gru_hid)?);
            Some(LoadedGru {
                wz: c.i8s(wi)?, wr: c.i8s(wi)?, wn: c.i8s(wi)?,
                uz: c.i8s(ui)?, ur: c.i8s(ui)?, un: c.i8s(ui)?,
                w_scale, u_scale,
                bz: c.f32s(gru_hid)?, br: c.f32s(gru_hid)?, bn: c.f32s(gru_hid)?,
            })
        };
        let fwd = read_gru(&mut c)?;
        let bwd = read_gru(&mut c)?;
        let fc_scale = c.f32()?;
        let fc_w = c.i8s((k + 1).checked_mul(2)?.checked_mul(gru_hid)?)?;
        let fc_b = c.f32s(k + 1)?;
        Some(LoadedModel { h, gru_in, gru_hid, rtl, alphabet, conv, fwd, bwd, fc_w, fc_scale, fc_b })
    }

    /// Recognize a page through this model (transient borrowing [`Crnn`], no copy).
    fn recognize(&self, gray: &[u8], w: usize, h: usize) -> OcrResult {
        let conv: Vec<ConvSpec> = self
            .conv
            .iter()
            .map(|c| ConvSpec { w: &c.w, scale: c.scale, b: &c.b, in_ch: c.in_ch, out_ch: c.out_ch })
            .collect();
        let crnn = Crnn {
            h: self.h, conv: &conv, gru_in: self.gru_in, gru_hid: self.gru_hid,
            fwd: mk_gru(&self.fwd), bwd: mk_gru(&self.bwd),
            fc_w: &self.fc_w, fc_scale: self.fc_scale, fc_b: &self.fc_b,
            alphabet: &self.alphabet, rtl: self.rtl,
        };
        recognize(gray, w, h, &[&crnn])
    }
}

use std::sync::RwLock;
/// Models supplied at runtime by the host (via [`load_model_from_bytes`]).
static LOADED: RwLock<Vec<LoadedModel>> = RwLock::new(Vec::new());

/// Load a `.gpocr` model blob into the runtime registry (host owns delivery, like
/// fonts). Returns false on a malformed blob. Exposed to WASM as `gp_ocr_load_model`.
pub fn load_model_from_bytes(bytes: &[u8]) -> bool {
    match LoadedModel::from_bytes(bytes) {
        Some(m) => LOADED.write().map(|mut g| g.push(m)).is_ok(),
        None => false,
    }
}

/// Drop all runtime-loaded models.
pub fn clear_models() {
    if let Ok(mut g) = LOADED.write() {
        g.clear();
    }
}

/// Run the line recognizer over the models this build can use: **runtime-loaded blobs
/// first** (`gp_ocr_load_model`), else any **feature-baked** models. Empty when none —
/// so [`super::ocr::ocr`] falls back to the mono-glyph classifier.
pub(crate) fn recognize_enabled(gray: &[u8], w: usize, h: usize) -> OcrResult {
    if let Ok(g) = LOADED.read() {
        match g.len() {
            0 => {}
            1 => return g[0].recognize(gray, w, h),
            _ => {
                // Route per line across all loaded models (build transient Crnns).
                let convs: Vec<Vec<ConvSpec>> = g
                    .iter()
                    .map(|m| {
                        m.conv
                            .iter()
                            .map(|c| ConvSpec { w: &c.w, scale: c.scale, b: &c.b, in_ch: c.in_ch, out_ch: c.out_ch })
                            .collect()
                    })
                    .collect();
                let crnns: Vec<Crnn> = g
                    .iter()
                    .zip(&convs)
                    .map(|(m, conv)| Crnn {
                        h: m.h, conv, gru_in: m.gru_in, gru_hid: m.gru_hid,
                        fwd: mk_gru(&m.fwd), bwd: mk_gru(&m.bwd),
                        fc_w: &m.fc_w, fc_scale: m.fc_scale, fc_b: &m.fc_b,
                        alphabet: &m.alphabet, rtl: m.rtl,
                    })
                    .collect();
                let refs: Vec<&Crnn> = crnns.iter().collect();
                return recognize(gray, w, h, &refs);
            }
        }
    }
    let models = enabled_models();
    if models.is_empty() {
        return OcrResult::default();
    }
    let refs: Vec<&Crnn> = models.iter().collect();
    recognize(gray, w, h, &refs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A logit vector whose argmax is `idx` (length `k+1`, blank = `k`).
    fn onehot(idx: usize, k_plus1: usize) -> Vec<f32> {
        let mut v = vec![0f32; k_plus1];
        v[idx] = 1.0;
        v
    }

    #[test]
    fn ctc_collapses_runs_and_drops_blank() {
        // alphabet "ab": indices 0=a 1=b, blank=2. [0,0,2,1,1,1,0] → "aba".
        let seq = [0, 0, 2, 1, 1, 1, 0];
        let logits: Vec<Vec<f32>> = seq.iter().map(|&i| onehot(i, 3)).collect();
        assert_eq!(ctc_greedy_decode(&logits, "ab", false), "aba");
    }

    #[test]
    fn ctc_blank_separates_repeated_labels() {
        // [a, blank, a] must stay two a's (not collapse to one).
        let logits: Vec<Vec<f32>> = [0, 2, 0].iter().map(|&i| onehot(i, 3)).collect();
        assert_eq!(ctc_greedy_decode(&logits, "ab", false), "aa");
        // …but [a, a] (no blank between) collapses to one.
        let logits2: Vec<Vec<f32>> = [0, 0].iter().map(|&i| onehot(i, 3)).collect();
        assert_eq!(ctc_greedy_decode(&logits2, "ab", false), "a");
    }

    #[test]
    fn ctc_rtl_reverses_output() {
        // [a, b] left-to-right is "ab"; right-to-left is "ba".
        let logits: Vec<Vec<f32>> = [0, 1].iter().map(|&i| onehot(i, 3)).collect();
        assert_eq!(ctc_greedy_decode(&logits, "ab", false), "ab");
        assert_eq!(ctc_greedy_decode(&logits, "ab", true), "ba");
    }

    #[test]
    fn argmax_picks_first_maximum() {
        assert_eq!(argmax(&[0.1, 0.9, 0.9, 0.2]), 1);
        assert_eq!(argmax(&[-3.0, -1.0, -2.0]), 1);
    }

    #[test]
    fn gru_cell_matches_hand_computation() {
        // in=1, hid=1, zero recurrent weights, h0=0:
        //   z = σ(0) = 0.5 ; n = tanh(64 * (1/64) * 1) = tanh(1) ≈ 0.76159
        //   h = (1-z)·n + z·0 = 0.5 · 0.76159 ≈ 0.380797
        let (wz, wr, wn) = ([0i8], [0i8], [64i8]);
        let (uz, ur, un) = ([0i8], [0i8], [0i8]);
        let (bz, br, bn) = ([0f32], [0f32], [0f32]);
        let g = GruSpec {
            wz: &wz, wr: &wr, wn: &wn,
            uz: &uz, ur: &ur, un: &un,
            w_scale: 1.0 / 64.0, u_scale: 1.0,
            bz: &bz, br: &br, bn: &bn,
        };
        let h = gru_cell(&g, &[1.0], &[0.0], 1, 1);
        assert!((h[0] - 0.380797).abs() < 1e-3, "h={}", h[0]);
    }

    #[test]
    fn extract_line_strips_normalizes_height() {
        // 40×20 white page with three black "glyph" squares on one line.
        let (w, h) = (40usize, 20usize);
        let mut gray = vec![255u8; w * h];
        for &cx in &[5usize, 16, 27] {
            for y in 5..15 {
                for x in cx..cx + 6 {
                    gray[y * w + x] = 0;
                }
            }
        }
        let strips = extract_line_strips(&gray, w, h);
        assert_eq!(strips.len(), 1, "three glyphs should group into one line");
        let (strip, sw, _bx) = &strips[0];
        assert!(*sw > 0);
        assert_eq!(strip.len(), STRIP_H * sw, "strip is STRIP_H rows tall");
        assert!(strip.contains(&1.0), "strip carries ink");
    }

    #[test]
    fn recognize_line_runs_on_a_tiny_model() {
        // A minimal but well-formed CRNN (1 conv → 1×1 GRU → 2-class head, "a").
        let c1w = [1i8; 9];
        let c1b = [0f32];
        let conv = [ConvSpec { w: &c1w, scale: 0.1, b: &c1b, in_ch: 1, out_ch: 1 }];
        let (wz, wr, wn) = ([1i8], [1i8], [1i8]);
        let (uz, ur, un) = ([1i8], [1i8], [1i8]);
        let (bz, br, bn) = ([0f32], [0f32], [0f32]);
        let mk = || GruSpec {
            wz: &wz, wr: &wr, wn: &wn,
            uz: &uz, ur: &ur, un: &un,
            w_scale: 0.1, u_scale: 0.1,
            bz: &bz, br: &br, bn: &bn,
        };
        let fc_w = [1i8; 4]; // (K+1=2) × (2*hid=2)
        let fc_b = [0f32; 2];
        let m = Crnn {
            h: STRIP_H,
            conv: &conv,
            gru_in: 1,
            gru_hid: 1,
            fwd: mk(),
            bwd: mk(),
            fc_w: &fc_w,
            fc_scale: 0.1,
            fc_b: &fc_b,
            alphabet: "a",
            rtl: false,
        };
        let sw = 8usize;
        let mut strip = vec![0f32; STRIP_H * sw];
        for y in 8..24 {
            strip[y * sw + 4] = 1.0; // a vertical ink stroke
        }
        let (text, conf) = recognize_line(&m, &strip, sw);
        assert!(conf.is_finite() && (0.0..=1.0).contains(&conf));
        assert!(text.chars().all(|c| c == 'a')); // only the single alphabet char
    }

    #[test]
    fn recognize_returns_empty_without_models() {
        let gray = vec![255u8; 40 * 20];
        let res = recognize(&gray, 40, 20, &[]);
        assert!(res.text.is_empty() && res.words.is_empty());
    }

    // ── host-loaded `.gpocr` model format ──────────────────────────────────
    /// Serialize a model to the `.gpocr` format (inverse of `from_bytes`; mirrors
    /// tools/train_ocr_crnn.py). Lives in the test module to keep prod lean.
    fn ser(m: &LoadedModel) -> Vec<u8> {
        fn pu16(o: &mut Vec<u8>, v: usize) { o.extend_from_slice(&(v as u16).to_le_bytes()); }
        fn pu32(o: &mut Vec<u8>, v: usize) { o.extend_from_slice(&(v as u32).to_le_bytes()); }
        fn pf(o: &mut Vec<u8>, v: f32) { o.extend_from_slice(&v.to_le_bytes()); }
        fn pi8(o: &mut Vec<u8>, v: &[i8]) { o.extend(v.iter().map(|&x| x as u8)); }
        fn pf32(o: &mut Vec<u8>, v: &[f32]) { for &x in v { o.extend_from_slice(&x.to_le_bytes()); } }
        let mut o = Vec::new();
        o.extend_from_slice(GPOCR_MAGIC);
        o.push(m.rtl as u8);
        pu16(&mut o, m.h); pu16(&mut o, m.gru_in); pu16(&mut o, m.gru_hid);
        pu32(&mut o, m.alphabet.len());
        o.extend_from_slice(m.alphabet.as_bytes());
        o.push(m.conv.len() as u8);
        for cv in &m.conv {
            pu16(&mut o, cv.in_ch); pu16(&mut o, cv.out_ch); pf(&mut o, cv.scale);
            pi8(&mut o, &cv.w); pf32(&mut o, &cv.b);
        }
        for g in [&m.fwd, &m.bwd] {
            pf(&mut o, g.w_scale); pf(&mut o, g.u_scale);
            for blk in [&g.wz, &g.wr, &g.wn, &g.uz, &g.ur, &g.un] { pi8(&mut o, blk); }
            for blk in [&g.bz, &g.br, &g.bn] { pf32(&mut o, blk); }
        }
        pf(&mut o, m.fc_scale); pi8(&mut o, &m.fc_w); pf32(&mut o, &m.fc_b);
        o
    }

    fn tiny_model() -> LoadedModel {
        let g = || LoadedGru {
            wz: vec![1], wr: vec![1], wn: vec![1], uz: vec![1], ur: vec![1], un: vec![1],
            w_scale: 0.1, u_scale: 0.1, bz: vec![0.0], br: vec![0.0], bn: vec![0.0],
        };
        LoadedModel {
            h: STRIP_H, gru_in: 1, gru_hid: 1, rtl: false, alphabet: "a".into(),
            conv: vec![LoadedConv { w: vec![1; 9], scale: 0.1, b: vec![0.0], in_ch: 1, out_ch: 1 }],
            fwd: g(), bwd: g(), fc_w: vec![1; 4], fc_scale: 0.1, fc_b: vec![0.0; 2],
        }
    }

    #[test]
    fn gpocr_roundtrips() {
        let p = LoadedModel::from_bytes(&ser(&tiny_model())).expect("parse");
        assert_eq!((p.h, p.gru_in, p.gru_hid, p.rtl), (STRIP_H, 1, 1, false));
        assert_eq!(p.alphabet, "a");
        assert_eq!(p.conv.len(), 1);
        assert_eq!(p.conv[0].w, vec![1i8; 9]);
        assert_eq!(p.fwd.wz, vec![1i8]);
        assert_eq!((p.fc_w.len(), p.fc_b.len()), (4, 2));
    }

    #[test]
    fn gpocr_rejects_garbage() {
        assert!(LoadedModel::from_bytes(b"nope").is_none());
        assert!(LoadedModel::from_bytes(&[]).is_none());
        assert!(!load_model_from_bytes(b"not a model")); // bad blob → registry untouched
    }

    #[test]
    fn loaded_model_recognizes_without_panic() {
        // Blank page → no line bands → empty result, but the full path must run.
        let r = tiny_model().recognize(&vec![255u8; 40 * 20], 40, 20);
        assert!(r.words.is_empty());
    }

    #[test]
    fn disambiguate_snaps_to_voted_script() {
        // 'd' is Latin-only → a Greek 'Α' (U+0391) in the same token snaps to Latin 'A'.
        assert_eq!(disambiguate_word(&format!("d{}", '\u{0391}')), "dA");
        // Cyrillic 'и' votes Cyrillic → that 'Α' snaps to Cyrillic 'А' (U+0410).
        let out = disambiguate_word(&format!("{}{}", '\u{0438}', '\u{0391}'));
        assert_eq!(out.chars().nth(1), Some('\u{0410}'));
        // Greek 'Ω' votes Greek → ambiguous 'α' stays Greek (unchanged).
        let g = format!("{}{}", '\u{03A9}', '\u{03B1}');
        assert_eq!(disambiguate_word(&g), g);
    }

    #[test]
    fn disambiguate_leaves_signal_free_tokens_and_keeps_spaces() {
        // Latin A + Greek Α: both ambiguous, no unambiguous voter → untouched.
        let s = format!("{}{}", 'A', '\u{0391}');
        assert_eq!(disambiguate_word(&s), s);
        // Per-token over a line, spaces preserved.
        assert_eq!(disambiguate_line("food bar"), "food bar");
    }

    // ── front-end: deskew / despeckle (Lever B) ────────────────────────────
    #[test]
    fn estimate_skew_recovers_tilt() {
        let (w, h) = (60usize, 40usize);
        let mut flat = vec![false; w * h];
        let mut tilt = vec![false; w * h];
        for x in 0..w {
            for dy in 0..3 {
                flat[(20 + dy) * w + x] = true;
                let y = 20 + (x as f64 * 0.05).round() as usize + dy;
                if y < h {
                    tilt[y * w + x] = true;
                }
            }
        }
        assert!(estimate_skew(&flat, w, h).abs() < 1e-9, "flat band → no skew");
        assert!((estimate_skew(&tilt, w, h) - 0.05).abs() < 0.02, "recovers ~0.05 rad tilt");
    }

    #[test]
    fn despeckle_drops_specks_keeps_glyph() {
        let (w, h) = (20usize, 20usize);
        let mut ink = vec![false; w * h];
        for y in 5..10 {
            for x in 5..10 {
                ink[y * w + x] = true; // 25-px block
            }
        }
        ink[0] = true; // isolated speck
        ink[2 * w + 15] = true; // isolated speck
        despeckle(&mut ink, w, h, 3);
        assert!(ink[7 * w + 7], "glyph block kept");
        assert!(!ink[0] && !ink[2 * w + 15], "specks removed");
    }

    #[test]
    fn rotate_gray_is_identity_at_zero() {
        let (w, h) = (10usize, 10usize);
        let mut g = vec![0u8; w * h];
        g[5 * w + 5] = 200;
        assert_eq!(rotate_gray(&g, w, h, 0.0)[5 * w + 5], 200);
    }
}
