//! Document conversion: PDF → editable Office formats and text/HTML.
//!
//! The exporters reconstruct **real, editable content** — positioned text
//! boxes, re-embedded images and shape outlines — not a rasterized page image.
//! This mirrors how an office suite imports a PDF: each show-text run becomes a
//! placed text frame, each image XObject a placed picture. The container half
//! (ZIP) lives in [`zip`]; the per-format XML in [`office`].
//!
//! Coordinates in the data model below are **top-down, origin top-left, in PDF
//! points** (1 pt = 1/72"). The PDF→top-down Y flip is done once during
//! extraction (see `Document::convert_pages`), so every exporter consumes the
//! same already-normalized geometry.

pub mod build;
pub mod csv_import;
pub mod export_model;
pub mod grids;
pub mod import;
pub mod md_import;
pub mod office;
pub mod office_import;
pub mod pdfa;
pub mod project;
pub mod reverse;
pub mod rtf;
pub mod srgb_icc;
pub mod style;
pub mod table;
pub mod tagged;
pub mod web;
pub mod zip;

pub use import::{
    csv_to_model, html_to_model, image_to_model, md_to_model, office_to_model, rtf_to_model,
    txt_to_model,
};
pub use style::{Generic, TextStyle};

/// A text run placed on a page (top-left origin, points).
#[derive(Debug, Clone)]
pub struct PlacedText {
    /// Decoded, font-aware text of the run.
    pub text: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    /// Recovered font family / weight / style / colour.
    pub style: TextStyle,
}

/// A real image XObject from the PDF, re-encoded to PNG and placed on a page.
#[derive(Debug, Clone)]
pub struct PlacedImage {
    /// PNG-encoded bytes of the actual embedded image (not a page render).
    pub png: Vec<u8>,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// A vector path placed on a page, carrying both its geometry and the resolved
/// paint state recovered from the PDF graphics state. The exporters draw the
/// `segments` as a real path (top-down, origin top-left, points) when present;
/// otherwise they fall back to the bounding rectangle (`x`/`y`/`width`/`height`),
/// which always describes the same box. Frames, table rules and separators thus
/// keep their actual fill/stroke colours instead of a hardcoded grey rectangle.
#[derive(Debug, Clone)]
pub struct PlacedShape {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    /// Path geometry in top-down points (origin top-left), already Y-flipped from
    /// PDF user space. Empty ⇒ the exporter emits the bounding rectangle instead.
    pub segments: Vec<crate::content::vector::PathSeg>,
    /// Fill colour (RGB `0..=1`) when the path is filled; `None` ⇒ no fill.
    pub fill: Option<[f64; 3]>,
    /// Stroke colour (RGB `0..=1`) when the path is stroked; `None` ⇒ no stroke.
    pub stroke: Option<[f64; 3]>,
    /// Stroke width in points.
    pub stroke_width: f64,
    /// Non-stroking (fill) alpha, `0..=1`.
    pub fill_alpha: f64,
    /// Stroking alpha, `0..=1`.
    pub stroke_alpha: f64,
    /// Dash pattern (point lengths); empty ⇒ solid line.
    pub dash: Vec<f64>,
}

impl Default for PlacedShape {
    fn default() -> Self {
        PlacedShape {
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 0.0,
            segments: Vec::new(),
            fill: None,
            stroke: None,
            stroke_width: 1.0,
            fill_alpha: 1.0,
            stroke_alpha: 1.0,
            dash: Vec::new(),
        }
    }
}

/// One page's editable content in top-down points.
#[derive(Debug, Clone, Default)]
pub struct ConvPage {
    pub width: f64,
    pub height: f64,
    pub texts: Vec<PlacedText>,
    pub images: Vec<PlacedImage>,
    pub shapes: Vec<PlacedShape>,
}

/// Standard Base64 (RFC 4648) of `data` — for embedding images as `data:` URIs
/// in the HTML export, and for handing decoded attachment/stream bytes to the
/// WASM host as JSON. Zero-dependency.
pub fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::base64;

    #[test]
    fn base64_matches_rfc_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }
}
