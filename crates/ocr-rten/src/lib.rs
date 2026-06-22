//! Server-side OCR engine: PaddleOCR PP-OCR models (text **detection** DBNet + **recognition**
//! SVTR/CRNN) run through **RTen** — a pure-Rust ONNX engine, no C++ dependency. This replaces
//! the legacy hand-trained CRNN; it carries the ML deps and runs host-side, while the lean
//! pure-std `core`/`wasm` crates call it via an endpoint.

use std::path::Path;

use image::imageops::FilterType;
use image::RgbImage;
use rten::Model;
use rten_tensor::prelude::*;
use rten_tensor::NdTensor;

const REC_H: usize = 48; // PP-OCRv4 recognition input height
const DET_MAX_SIDE: u32 = 960; // cap the detection input's long side
const DET_BIN_THRESH: f32 = 0.3; // DBNet probability-map binarization threshold

/// Language manifest: (display name, models-dir subdirectory, RTL?). PaddleOCR PP-OCRv3/v4 rec
/// models (shared DBNet detector) + our own Hebrew model (PaddleOCR has none). One DBNet detector
/// covers every script; only the recognizer + dict vary. Detection model is shared.
pub const REC_MODELS: &[(&str, &str, bool)] = &[
    ("ar", "arabic_PP-OCRv3_rec", true),   // Arabic — RTL
    ("he", "hebrew", true),                // Hebrew — our model (PaddleOCR ships none), RTL
    ("zh", "ch_PP-OCRv4_rec", false),      // Simplified Chinese (+ Latin + digits)
    ("zh_tw", "chinese_cht_PP-OCRv3_rec", false), // Traditional Chinese
    ("cyrillic", "cyrillic_PP-OCRv3_rec", false), // Russian/Ukrainian/…
    ("devanagari", "devanagari_PP-OCRv3_rec", false), // Hindi/Marathi/…
    ("en", "en_PP-OCRv4_rec", false),      // English
    ("ja", "japan_PP-OCRv3_rec", false),   // Japanese
    ("kn", "ka_PP-OCRv3_rec", false),      // Kannada
    ("ko", "korean_PP-OCRv3_rec", false),  // Korean
    ("latin", "latin_PP-OCRv3_rec", false), // French/German/Spanish/… (Latin script)
    ("ta", "ta_PP-OCRv3_rec", false),      // Tamil
    ("te", "te_PP-OCRv3_rec", false),      // Telugu
];

/// An axis-aligned text box in original-image pixel coordinates.
#[derive(Clone, Copy, Debug)]
pub struct BBox {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}

/// One recognized line: its box, decoded text, mean per-step confidence, and which rec model won.
#[derive(Clone, Debug)]
pub struct Line {
    pub bbox: BBox,
    pub text: String,
    pub confidence: f32,
    pub model: String,
}

/// A loaded recognition model: its CTC charlist plus a right-to-left flag (Hebrew/Arabic).
pub struct RecModel {
    pub name: String,
    model: Model,
    /// CTC charlist: index 0 = blank, 1..=N = dict chars, last = space.
    chars: Vec<String>,
    /// True for RTL scripts — the CTC output is in visual L→R order, so we reverse to logical.
    pub rtl: bool,
}

/// A shared (language-agnostic) DBNet detector plus one or more recognition models. With several
/// recs loaded, lines are routed by **confidence-based script selection** (highest mean CTC logit).
pub struct OcrEngine {
    det: Model,
    recs: Vec<RecModel>,
}

fn load_charlist(dict: impl AsRef<Path>) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let raw = std::fs::read_to_string(dict)?;
    let mut chars = Vec::with_capacity(raw.lines().count() + 2);
    chars.push(String::new()); // 0 = blank
    chars.extend(raw.lines().map(str::to_string)); // 1..=N
    chars.push(" ".to_string()); // trailing space (use_space_char)
    Ok(chars)
}

impl OcrEngine {
    /// Build an engine with only the (shared) detection model; add rec models with `add_rec`.
    pub fn new(det: impl AsRef<Path>) -> Result<OcrEngine, Box<dyn std::error::Error>> {
        Ok(OcrEngine { det: Model::load_file(det.as_ref())?, recs: Vec::new() })
    }

    /// Register a recognition model (its dict + RTL flag) under `name`.
    pub fn add_rec(
        &mut self,
        name: impl Into<String>,
        rec: impl AsRef<Path>,
        dict: impl AsRef<Path>,
        rtl: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.recs.push(RecModel {
            name: name.into(),
            model: Model::load_file(rec.as_ref())?,
            chars: load_charlist(dict)?,
            rtl,
        });
        Ok(())
    }

    /// Load a whole models directory laid out as `det.rten` + `<subdir>/{model.rten,dict.txt}`
    /// per [`REC_MODELS`]. Missing languages are skipped, so any available subset works.
    pub fn load_models_dir(dir: impl AsRef<Path>) -> Result<OcrEngine, Box<dyn std::error::Error>> {
        let dir = dir.as_ref();
        let mut e = OcrEngine::new(dir.join("det.rten"))?;
        for (name, subdir, rtl) in REC_MODELS {
            let rec = dir.join(subdir).join("model.rten");
            let dict = dir.join(subdir).join("dict.txt");
            if rec.exists() && dict.exists() {
                e.add_rec(*name, rec, dict, *rtl)?;
            }
        }
        if e.recs.is_empty() {
            return Err("no recognition models found in models dir".into());
        }
        Ok(e)
    }

    /// Number of loaded recognition models.
    pub fn rec_count(&self) -> usize {
        self.recs.len()
    }

    /// Convenience: detector + a single LTR recognition model (back-compat with the probes).
    pub fn load(
        det: impl AsRef<Path>,
        rec: impl AsRef<Path>,
        dict: impl AsRef<Path>,
    ) -> Result<OcrEngine, Box<dyn std::error::Error>> {
        let mut e = OcrEngine::new(det)?;
        e.add_rec("default", rec, dict, false)?;
        Ok(e)
    }

    /// Full page → recognized lines (detect, recognize each via best-confidence rec, reading order).
    pub fn recognize_page(&self, img: &RgbImage) -> Result<Vec<Line>, Box<dyn std::error::Error>> {
        let mut boxes = self.detect(img)?;
        boxes.sort_by_key(|b| (b.y0 / 10, b.x0)); // top-to-bottom, then left-to-right
        let mut out = Vec::with_capacity(boxes.len());
        for b in boxes {
            let crop = image::imageops::crop_imm(img, b.x0, b.y0, b.x1 - b.x0, b.y1 - b.y0).to_image();
            let (text, confidence, model) = self.recognize_line_auto(&crop)?;
            if !text.trim().is_empty() {
                out.push(Line { bbox: b, text, confidence, model });
            }
        }
        Ok(out)
    }

    /// Recognize one cropped line, auto-selecting the rec model with the highest confidence.
    pub fn recognize_line_auto(
        &self,
        line: &RgbImage,
    ) -> Result<(String, f32, String), Box<dyn std::error::Error>> {
        let (mut best, mut best_conf, mut best_name) = (String::new(), f32::NEG_INFINITY, "");
        for m in &self.recs {
            let (text, conf) = self.decode(m, line)?;
            if conf > best_conf {
                (best, best_conf, best_name) = (text, conf, m.name.as_str());
            }
        }
        Ok((best, best_conf.max(0.0), best_name.to_string()))
    }

    /// Run one rec model on a cropped line → (text, mean confidence). Reverses RTL output to logical.
    fn decode(&self, m: &RecModel, line: &RgbImage) -> Result<(String, f32), Box<dyn std::error::Error>> {
        let (w0, h0) = (line.width().max(1) as f32, line.height().max(1) as f32);
        let new_w = ((REC_H as f32) * w0 / h0).round().max(1.0) as u32;
        let resized = image::imageops::resize(line, new_w, REC_H as u32, FilterType::Triangle);
        let w = new_w as usize;
        let mut data = vec![0f32; 3 * REC_H * w];
        for y in 0..REC_H {
            for x in 0..w {
                let px = resized.get_pixel(x as u32, y as u32);
                for c in 0..3 {
                    data[c * REC_H * w + y * w + x] = (px[c] as f32 / 255.0 - 0.5) / 0.5;
                }
            }
        }
        let input: NdTensor<f32, 4> = NdTensor::from_data([1, 3, REC_H, w], data);
        let logits: NdTensor<f32, 3> = m.model.run_one((&input).into(), None)?.try_into()?;
        let (t_len, n_cls) = (logits.shape()[1], logits.shape()[2]);
        let mut prev = usize::MAX;
        let (mut chars_out, mut conf_sum) = (Vec::<&str>::new(), 0f32);
        for t in 0..t_len {
            let (mut bi, mut bv) = (0usize, f32::NEG_INFINITY);
            for c in 0..n_cls {
                let v = logits[[0, t, c]];
                if v > bv {
                    (bv, bi) = (v, c);
                }
            }
            conf_sum += bv;
            if bi != prev && bi != 0 {
                if let Some(ch) = m.chars.get(bi) {
                    chars_out.push(ch);
                }
            }
            prev = bi;
        }
        // RTL: the model emits glyphs in visual L→R order; reverse the token sequence to logical.
        if m.rtl {
            chars_out.reverse();
        }
        Ok((chars_out.concat(), conf_sum / t_len.max(1) as f32))
    }

    /// DBNet text detection → axis-aligned line boxes (original-image coords).
    pub fn detect(&self, img: &RgbImage) -> Result<Vec<BBox>, Box<dyn std::error::Error>> {
        let (ow, oh) = (img.width(), img.height());
        // Resize: long side ≤ DET_MAX_SIDE, both dims rounded to a multiple of 32.
        let ratio = (DET_MAX_SIDE as f32 / ow.max(oh) as f32).min(1.0);
        let round32 = |v: f32| ((v / 32.0).round().max(1.0) as u32) * 32;
        let (nw, nh) = (round32(ow as f32 * ratio), round32(oh as f32 * ratio));
        let resized = image::imageops::resize(img, nw, nh, FilterType::Triangle);
        // ImageNet normalization (PaddleOCR detection).
        const MEAN: [f32; 3] = [0.485, 0.456, 0.406];
        const STD: [f32; 3] = [0.229, 0.224, 0.225];
        let (w, h) = (nw as usize, nh as usize);
        let mut data = vec![0f32; 3 * h * w];
        for y in 0..h {
            for x in 0..w {
                let px = resized.get_pixel(x as u32, y as u32);
                for c in 0..3 {
                    data[c * h * w + y * w + x] = (px[c] as f32 / 255.0 - MEAN[c]) / STD[c];
                }
            }
        }
        let input: NdTensor<f32, 4> = NdTensor::from_data([1, 3, h, w], data);
        let prob: NdTensor<f32, 4> = self.det.run_one((&input).into(), None)?.try_into()?;
        // prob shape [1,1,h,w] — binarize then connected-component boxes.
        let (ph, pw) = (prob.shape()[2], prob.shape()[3]);
        let mut bin = vec![false; ph * pw];
        for y in 0..ph {
            for x in 0..pw {
                bin[y * pw + x] = prob[[0, 0, y, x]] > DET_BIN_THRESH;
            }
        }
        // Scale prob-map coords back to the original image.
        let (sx, sy) = (ow as f32 / pw as f32, oh as f32 / ph as f32);
        let mut boxes = Vec::new();
        for (mut x0, mut y0, mut x1, mut y1) in connected_boxes(&bin, pw, ph) {
            // Unclip: DBNet shrinks regions, so expand by ~30% of box height.
            let pad = (((y1 - y0) as f32) * 0.3).round() as i32;
            x0 = (x0 as i32 - pad).max(0) as usize;
            y0 = (y0 as i32 - pad).max(0) as usize;
            x1 = ((x1 as i32 + pad) as usize).min(pw - 1);
            y1 = ((y1 as i32 + pad) as usize).min(ph - 1);
            let b = BBox {
                x0: (x0 as f32 * sx) as u32,
                y0: (y0 as f32 * sy) as u32,
                x1: ((x1 + 1) as f32 * sx).ceil().min(ow as f32) as u32,
                y1: ((y1 + 1) as f32 * sy).ceil().min(oh as f32) as u32,
            };
            if b.x1 > b.x0 + 2 && b.y1 > b.y0 + 2 {
                boxes.push(b);
            }
        }
        Ok(boxes)
    }
}

/// Connected components (4-connectivity) of a binary mask → their bounding boxes
/// `(x0,y0,x1,y1)` inclusive. Tiny components (< 6 px) are dropped as noise.
fn connected_boxes(bin: &[bool], w: usize, h: usize) -> Vec<(usize, usize, usize, usize)> {
    let mut seen = vec![false; w * h];
    let mut out = Vec::new();
    let mut stack = Vec::new();
    for sy in 0..h {
        for sx in 0..w {
            let s = sy * w + sx;
            if !bin[s] || seen[s] {
                continue;
            }
            let (mut x0, mut y0, mut x1, mut y1, mut n) = (sx, sy, sx, sy, 0usize);
            seen[s] = true;
            stack.push((sx, sy));
            while let Some((x, y)) = stack.pop() {
                n += 1;
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
                let mut push = |nx: usize, ny: usize, st: &mut Vec<(usize, usize)>| {
                    let i = ny * w + nx;
                    if bin[i] && !seen[i] {
                        seen[i] = true;
                        st.push((nx, ny));
                    }
                };
                if x > 0 {
                    push(x - 1, y, &mut stack);
                }
                if x + 1 < w {
                    push(x + 1, y, &mut stack);
                }
                if y > 0 {
                    push(x, y - 1, &mut stack);
                }
                if y + 1 < h {
                    push(x, y + 1, &mut stack);
                }
            }
            if n >= 6 {
                out.push((x0, y0, x1, y1));
            }
        }
    }
    out
}
