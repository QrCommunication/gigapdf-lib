//! Document outline — bookmarks / table of contents (ISO 32000-1 §12.3.3).
//!
//! The catalog's `/Outlines` dictionary roots a doubly-linked tree of items,
//! each with `/Title`, `/Parent`, `/Prev`, `/Next`, `/First`, `/Last`, `/Count`
//! and a destination (`/Dest` or a `/A /GoTo` action). The tree walk and the
//! (re)builder live on `Document`; this module is the flattened view a caller
//! sees.

/// A flattened outline (bookmark) entry.
#[derive(Debug, Clone, PartialEq)]
pub struct OutlineItem {
    /// The bookmark label.
    pub title: String,
    /// Nesting depth, `0` for a top-level item.
    pub level: usize,
    /// 1-based destination page, when it resolves to one.
    pub page: Option<u32>,
}
