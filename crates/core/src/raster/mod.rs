//! Zero-dependency PDF page rasterizer.
//!
//! Built to replace external renderers (PDFium/MuPDF) entirely: a content-stream
//! interpreter ([`render`]) paints vector graphics into an anti-aliased RGBA
//! [`Canvas`], which exports a spec-valid [`png`]. Text-glyph and image slices
//! build on the same canvas and fill engine.

pub mod canvas;
pub mod ocr;
pub mod ocr_crnn;
pub mod ocr_model;
// Per-script CRNN line models (feature-gated; files emitted by tools/train_ocr_crnn.py).
#[cfg(feature = "ocr-alpha")]
pub mod ocr_model_alpha;
pub mod gif;
pub mod jpeg;
pub mod png;
pub mod png_decode;
pub mod render;
pub mod avif;
pub mod resize;
pub mod vp8;
pub mod webp;

pub use canvas::Canvas;
pub use png::encode_png;
pub use png_decode::decode_png;
pub use render::render_content;
pub use resize::resize_rgba;
