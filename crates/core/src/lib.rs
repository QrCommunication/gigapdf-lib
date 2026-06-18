//! gigapdf-core — a document engine with **no third-party PDF/Office/image
//! library**.
//!
//! Copyright 2025 Rony Licha / QR Communication. Licensed under the PolyForm
//! Noncommercial License 1.0.0 (see `LICENSE`).
//!
//! The PDF/Office/image stack — lexer, parser, `FlateDecode` inflate, content
//! editor, serializer, rasteriser, fonts, OCR and the format conversions — is
//! pure `std` and compiles to `wasm32` directly. Two subsystems delegate to
//! audited crates by design (see `THIRD-PARTY-LICENSES.md`): cryptography uses
//! RustCrypto, and the HTML→PDF inline-script path uses the Boa JS engine.
//!
//! Editing model: a PDF page's content is a flat list of drawing operators
//! (`Tj` text, `Do` image, `re`/`f` shapes). The engine parses that stream,
//! edits the targeted operator, and re-encodes — so the background is
//! preserved by construction and original glyphs never leak.
//!
//! ## Module map (built bottom-up)
//! - [`object`] — the PDF object model (ISO 32000-1 §7.3).
//! - [`error`]  — engine error type (hand-written, no `thiserror`).
//! - `lexer`    — byte-level tokenizer (next).
//! - `parser`   — tokens → objects, xref (classic + xref streams).
//! - `filters`  — FlateDecode / inflate (RFC 1950/1951).
//! - `content`  — content-stream operators + locate/edit.
//! - `document` — high-level open/inspect/edit/save.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod annot;
pub mod content;
pub mod convert;
pub mod crypto;
pub mod document;
pub mod error;
pub mod filters;
pub mod font;
pub mod form;
pub mod html;
pub mod js;
pub mod lexer;
pub mod link;
pub mod model;
pub mod object;
pub mod ocg;
pub mod outline;
pub mod parser;
pub mod raster;
pub mod security;
pub mod serialize;
pub mod sign;
pub mod svg;

pub use annot::Annotation;
pub use content::{Bounds, ContentElement, ElementKind, Operation, TextLine, TextRun};
pub use convert::{ConvPage, PlacedImage, PlacedShape, PlacedText};
pub use document::{
    Attachment, Document, EmbeddedFontInfo, ImageElementInfo, SearchMatch, TextElementInfo,
    TextLayerRun,
};
pub use error::{EngineError, Result};
pub use form::{FieldKind, FormField};
pub use lexer::{Lexer, Token};
pub use link::{Link, LinkTarget};
pub use object::{Dictionary, Object, ObjectId, Stream, StringKind};
pub use ocg::Layer;
pub use outline::OutlineItem;
pub use parser::Parser;
pub use raster::ocr::{OcrResult, OcrWord};
