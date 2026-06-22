//! Multilingual OCR: load every available rec model from a models dir + the shared DBNet detector.
//! Default: automatic per-line script selection (printed). Pass a model name (e.g. `latin_hw`) to
//! force a specific recognizer — that's how handwriting is invoked.
//! Usage: ocr_auto <models_dir> <page.png> [model]

use gigapdf_ocr_rten::OcrEngine;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 3 {
        eprintln!("usage: ocr_auto <models_dir> <page.png> [model]   (e.g. model=latin_hw for handwriting)");
        std::process::exit(2);
    }
    let eng = OcrEngine::load_models_dir(&a[1])?;
    eprintln!("{} rec model(s) loaded", eng.rec_count());
    let img = image::open(&a[2])?.to_rgb8();
    // Optional 3rd arg: force a specific recognizer (handwriting = "latin_hw"); else auto-select.
    let lines = match a.get(3) {
        Some(model) => eng.recognize_page_with(&img, model)?,
        None => eng.recognize_page(&img)?,
    };
    eprintln!("{} line(s) detected", lines.len());
    for l in &lines {
        println!("[{:.2}|{}] {}", l.confidence, l.model, l.text);
    }
    Ok(())
}
