//! Full-page OCR pipeline probe: detect text lines (DBNet) + recognize each (PaddleOCR rec),
//! all via RTen. Usage: ocr_probe <det.rten> <rec.rten> <dict.txt> <page.png>

use gigapdf_ocr_rten::OcrEngine;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 5 {
        eprintln!("usage: ocr_probe <det.rten> <rec.rten> <dict.txt> <page.png>");
        std::process::exit(2);
    }
    let eng = OcrEngine::load(&a[1], &a[2], &a[3])?;
    let img = image::open(&a[4])?.to_rgb8();
    let lines = eng.recognize_page(&img)?;
    eprintln!("{} line(s) detected", lines.len());
    for l in &lines {
        println!(
            "[{:.2}|{}] ({},{})-({},{}) {}",
            l.confidence, l.model, l.bbox.x0, l.bbox.y0, l.bbox.x1, l.bbox.y1, l.text
        );
    }
    Ok(())
}
