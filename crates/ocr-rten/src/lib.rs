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

/// An axis-aligned text box in original-image pixel coordinates.
#[derive(Clone, Copy, Debug)]
pub struct BBox {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}

/// One recognized line: its box, decoded text, and mean per-step confidence.
#[derive(Clone, Debug)]
pub struct Line {
    pub bbox: BBox,
    pub text: String,
    pub confidence: f32,
}

/// A loaded PaddleOCR detection+recognition pair plus its CTC character list.
pub struct OcrEngine {
    det: Model,
    rec: Model,
    /// CTC charlist: index 0 = blank, 1..=N = dict chars, last = space.
    chars: Vec<String>,
}

impl OcrEngine {
    /// Load a detection model, a recognition model and the recognition dictionary.
    pub fn load(
        det: impl AsRef<Path>,
        rec: impl AsRef<Path>,
        dict: impl AsRef<Path>,
    ) -> Result<OcrEngine, Box<dyn std::error::Error>> {
        let dict_chars = std::fs::read_to_string(dict)?;
        let mut chars = Vec::with_capacity(dict_chars.lines().count() + 2);
        chars.push(String::new()); // 0 = blank
        chars.extend(dict_chars.lines().map(str::to_string));
        chars.push(" ".to_string()); // trailing space (use_space_char)
        Ok(OcrEngine {
            det: Model::load_file(det.as_ref())?,
            rec: Model::load_file(rec.as_ref())?,
            chars,
        })
    }

    /// Full page → recognized lines (detect text regions, recognize each, top-to-bottom).
    pub fn recognize_page(&self, img: &RgbImage) -> Result<Vec<Line>, Box<dyn std::error::Error>> {
        let mut boxes = self.detect(img)?;
        // Reading order: top-to-bottom, then left-to-right.
        boxes.sort_by_key(|b| (b.y0 / 10, b.x0));
        let mut out = Vec::with_capacity(boxes.len());
        for b in boxes {
            let crop = image::imageops::crop_imm(img, b.x0, b.y0, b.x1 - b.x0, b.y1 - b.y0).to_image();
            let (text, confidence) = self.recognize_line(&crop)?;
            if !text.trim().is_empty() {
                out.push(Line { bbox: b, text, confidence });
            }
        }
        Ok(out)
    }

    /// Recognize one already-cropped text line.
    pub fn recognize_line(&self, line: &RgbImage) -> Result<(String, f32), Box<dyn std::error::Error>> {
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
        let logits: NdTensor<f32, 3> = self.rec.run_one((&input).into(), None)?.try_into()?;
        let (t_len, n_cls) = (logits.shape()[1], logits.shape()[2]);
        let mut prev = usize::MAX;
        let (mut text, mut conf_sum) = (String::new(), 0f32);
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
                if let Some(ch) = self.chars.get(bi) {
                    text.push_str(ch);
                }
            }
            prev = bi;
        }
        Ok((text, conf_sum / t_len.max(1) as f32))
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
