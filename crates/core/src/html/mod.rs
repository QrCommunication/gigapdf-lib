//! A zero-dependency HTML + CSS → PDF rendering engine.
//!
//! This replaces a headless-browser dependency (Chromium/Playwright) for the
//! common HTML→PDF case — structured documents, invoices, reports, templates —
//! with a real in-engine pipeline: parse HTML to a DOM, parse and cascade CSS,
//! lay out a block/inline box tree using **real font metrics**, and paint to PDF
//! with **fonts resolved against the full Google-Fonts catalogue** (downloaded
//! by the host and embedded), so the output font is identical or the nearest
//! match — never a generic substitute.
//!
//! Inline `<script>`s are executed before layout by the built-in
//! zero-dependency JavaScript engine ([`crate::js`]) with DOM bindings, so
//! script-driven content is rendered — no headless browser. CSS flexbox/grid
//! are progressively added; block, inline and table flow are supported.

pub mod bidi;
pub mod css;
pub mod diagram;
pub mod dom;
pub mod layout;
pub mod model;
pub mod page;
pub mod paint;

pub use model::to_model;
pub use page::{page_size, Margins, RenderOptions};
pub use paint::{
    needed_fonts, needed_fonts_with, needed_resources, render, render_with, FontRequest,
    ProvidedFont, ResourceNeed,
};
