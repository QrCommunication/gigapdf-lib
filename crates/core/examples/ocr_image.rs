//! OCR a PNG image to plain text using the engine's recognizer — the benchmark
//! entry point used by `tools/ocr/bench.py` to compare against Tesseract on
//! identical images.
//!
//! With a per-script feature it uses the line-level CRNN; with none it uses the
//! mono-glyph classifier:
//!
//! ```text
//! cargo run -q --release -p gigapdf-core --features ocr-alpha --example ocr_image -- line.png
//! ```
use std::io::Read;

fn main() {
    // Optional host-supplied line-OCR model (the lean-wasm path): load a `.gpocr`
    // blob named by $GIGAPDF_OCR_MODEL before recognizing — exactly what a host does
    // via `gp_ocr_load_model`. With none set, OCR uses the mono-glyph classifier.
    if let Ok(p) = std::env::var("GIGAPDF_OCR_MODEL") {
        match std::fs::read(&p) {
            Ok(blob) => eprintln!(
                "ocr model {p}: loaded={}",
                gigapdf_core::raster::ocr_crnn::load_model_from_bytes(&blob)
            ),
            Err(e) => eprintln!("ocr model {p}: {e}"),
        }
    }
    let path = match std::env::args().nth(1) {
        Some(p) => p,
        None => {
            eprintln!("usage: ocr_image <image.png>");
            std::process::exit(2);
        }
    };
    let mut bytes = Vec::new();
    std::fs::File::open(&path)
        .and_then(|mut f| f.read_to_end(&mut bytes))
        .expect("read image");
    let img = gigapdf_core::raster::decode_png(&bytes).expect("decode PNG");
    let (w, h) = (img.width as usize, img.height as usize);
    // RGBA → 8-bit luminance (BT.601), the input `ocr` expects.
    let mut gray = vec![0u8; w * h];
    for (i, px) in img.rgba.chunks_exact(4).enumerate() {
        let (r, g, b) = (px[0] as u32, px[1] as u32, px[2] as u32);
        gray[i] = ((r * 299 + g * 587 + b * 114) / 1000) as u8;
    }
    print!("{}", gigapdf_core::raster::ocr::ocr(&gray, w, h).text);
}
