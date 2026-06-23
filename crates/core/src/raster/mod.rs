//! Zero-dependency PDF page rasterizer.
//!
//! Built to replace external renderers (PDFium/MuPDF) entirely: a content-stream
//! interpreter ([`render`]) paints vector graphics into an anti-aliased RGBA
//! [`Canvas`], which exports a spec-valid [`png`]. Text-glyph and image slices
//! build on the same canvas and fill engine.

pub mod avif;
pub mod canvas;
pub mod colorspace;
pub mod gif;
pub mod jpeg;
pub mod mesh;
pub mod png;
pub mod png_decode;
pub mod render;
pub mod resize;
pub mod vp8;
pub mod webp;

pub use canvas::{BlendMode, Canvas, ClipMask};
pub use png::encode_png;
pub use png_decode::decode_png;
pub use render::{render_content, render_content_into, render_content_into_ctx};
pub use resize::resize_rgba;
