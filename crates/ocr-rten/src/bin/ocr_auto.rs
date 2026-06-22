//! Multilingual OCR: load every available rec model from a models dir + the shared DBNet detector,
//! recognize a page with automatic per-line script selection (highest-confidence model).
//! Usage: ocr_auto <models_dir> <page.png>

use gigapdf_ocr_rten::OcrEngine;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 3 {
        eprintln!("usage: ocr_auto <models_dir> <page.png>");
        std::process::exit(2);
    }
    let eng = OcrEngine::load_models_dir(&a[1])?;
    eprintln!("{} rec model(s) loaded", eng.rec_count());
    let img = image::open(&a[2])?.to_rgb8();
    let lines = eng.recognize_page(&img)?;
    eprintln!("{} line(s) detected", lines.len());
    for l in &lines {
        println!("[{:.2}|{}] {}", l.confidence, l.model, l.text);
    }
    Ok(())
}
