//! Hyperlink annotations (ISO 32000-1 §12.5.6.5).
//!
//! A link is an annotation with `/Subtype /Link` and a clickable `/Rect`. It
//! either runs a URI action (`/A << /S /URI /URI (…) >>`, an external link) or
//! carries a go-to destination (`/Dest` / `/A /GoTo`, a jump within the
//! document). Reading and creation live on `Document`; this module is the view
//! a caller sees.

/// Where a link points.
#[derive(Debug, Clone, PartialEq)]
pub enum LinkTarget {
    /// An external URI (web address, mailto, …).
    Uri(String),
    /// An internal jump to a 1-based page number.
    Page(u32),
    /// A link whose target could not be resolved to a URI or page.
    Unknown,
}

/// A link annotation read from a page.
#[derive(Debug, Clone)]
pub struct Link {
    /// 0-based index in the page `/Annots` array.
    pub index: usize,
    /// The clickable rectangle `[x0 y0 x1 y1]`.
    pub rect: [f64; 4],
    /// What the link points to.
    pub target: LinkTarget,
}
