//! Document outline — bookmarks / table of contents (ISO 32000-1 §12.3.3).
//!
//! The catalog's `/Outlines` dictionary roots a doubly-linked tree of items,
//! each with `/Title`, `/Parent`, `/Prev`, `/Next`, `/First`, `/Last`, `/Count`
//! and a destination (`/Dest` or a `/A /GoTo` action). The tree walk and the
//! (re)builder live on `Document`; this module is the flattened view a caller
//! sees.

/// A flattened outline (bookmark) entry — label, nesting depth, and the
/// destination + display attributes a host editor needs to recreate it.
#[derive(Debug, Clone, PartialEq)]
pub struct OutlineItem {
    /// The bookmark label.
    pub title: String,
    /// Nesting depth, `0` for a top-level item.
    pub level: usize,
    /// 1-based destination page, when it resolves to one.
    pub page: Option<u32>,
    /// `/F` flag bit 2 — the label is drawn bold.
    pub bold: bool,
    /// `/F` flag bit 1 — the label is drawn italic.
    pub italic: bool,
    /// `/C` RGB label colour (`0..=1` per channel; black when absent).
    pub color: [f64; 3],
    /// Destination fit type, lowercased (`"xyz"`, `"fit"`, `"fith"`, `"fitv"`,
    /// `"fitr"`, `"fitb"`…), or empty when the destination doesn't resolve.
    pub dest_kind: String,
    /// `/XYZ` top-left X (when `dest_kind == "xyz"`).
    pub dest_x: Option<f64>,
    /// `/XYZ` top-left Y (when `dest_kind == "xyz"`).
    pub dest_y: Option<f64>,
    /// `/XYZ` magnification (`0`/absent = inherit; when `dest_kind == "xyz"`).
    pub dest_zoom: Option<f64>,
}
