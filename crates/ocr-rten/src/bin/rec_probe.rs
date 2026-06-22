//! Phase-1 de-risk: run a PaddleOCR PP-OCRv4 **recognition** model via RTen (pure-Rust ONNX)
//! on a cropped text-line image and CTC-decode it with the PaddleOCR dictionary. Proves the
//! PaddleOCR-on-RTen path end-to-end (inference, not just conversion).
//!
//! Usage: cargo run -p gigapdf-ocr-rten --bin rec_probe -- <model.rten> <dict.txt> <line.png>

use std::env;
use std::fs;

use image::imageops::FilterType;
use rten::Model;
use rten_tensor::prelude::*;
use rten_tensor::NdTensor;

const REC_H: usize = 48; // PP-OCRv4 recognition input height

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: rec_probe <model.rten> <dict.txt> <line.png>");
        std::process::exit(2);
    }
    let model = Model::load_file(&args[1])?;

    // Char list (PaddleOCR CTC convention): index 0 = blank, 1..=N = dict chars, last = space.
    let dict: Vec<String> = fs::read_to_string(&args[2])?
        .lines()
        .map(|l| l.to_string())
        .collect();
    let mut chars: Vec<String> = Vec::with_capacity(dict.len() + 2);
    chars.push(String::new()); // 0 = blank
    chars.extend(dict.iter().cloned());
    chars.push(" ".to_string()); // trailing space (use_space_char)

    // ── Preprocess: RGB, resize to height 48 (keep aspect), normalize (px/255 - 0.5)/0.5, CHW.
    let img = image::open(&args[3])?.to_rgb8();
    let (w0, h0) = (img.width() as usize, img.height() as usize);
    let new_w = ((REC_H as f32) * w0 as f32 / h0 as f32).round().max(1.0) as u32;
    let resized = image::imageops::resize(&img, new_w, REC_H as u32, FilterType::Triangle);
    let w = new_w as usize;
    let mut data = vec![0f32; 3 * REC_H * w];
    for y in 0..REC_H {
        for x in 0..w {
            let px = resized.get_pixel(x as u32, y as u32);
            for c in 0..3 {
                let v = (px[c] as f32 / 255.0 - 0.5) / 0.5;
                data[c * REC_H * w + y * w + x] = v; // CHW
            }
        }
    }
    let input: NdTensor<f32, 4> = NdTensor::from_data([1, 3, REC_H, w], data);

    // ── Inference.
    let out = model.run_one((&input).into(), None)?;
    let logits: NdTensor<f32, 3> = out.try_into()?; // [1, T, C]
    let shape = logits.shape();
    let (t_len, n_cls) = (shape[1], shape[2]);
    eprintln!("output [1,{t_len},{n_cls}]  charlist={}", chars.len());

    // ── CTC greedy decode: argmax per timestep, collapse repeats, drop blank (index 0).
    let mut prev = usize::MAX;
    let mut text = String::new();
    let mut conf_sum = 0f32;
    for t in 0..t_len {
        let (mut bi, mut bv) = (0usize, f32::NEG_INFINITY);
        for c in 0..n_cls {
            let v = logits[[0, t, c]];
            if v > bv {
                bv = v;
                bi = c;
            }
        }
        conf_sum += bv;
        if bi != prev && bi != 0 {
            if let Some(ch) = chars.get(bi) {
                text.push_str(ch);
            }
        }
        prev = bi;
    }
    println!("decoded: {text}");
    eprintln!("mean_argmax_logit={:.3}", conf_sum / t_len as f32);
    Ok(())
}
