//! Zero-dependency PDF page rasterizer.
//!
//! Built to replace external renderers (PDFium/MuPDF) entirely: a content-stream
//! interpreter ([`render`]) paints vector graphics into an anti-aliased RGBA
//! [`Canvas`], which exports a spec-valid [`png`]. Text-glyph and image slices
//! build on the same canvas and fill engine.

pub mod canvas;
pub mod ocr;
pub mod ocr_model;
pub mod png;
pub mod png_decode;
pub mod render;

pub use canvas::Canvas;
pub use png::encode_png;
pub use render::render_content;
