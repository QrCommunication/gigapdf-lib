//! Optional Content Groups — PDF layers (ISO 32000 §8.11).
//!
//! A layer is an OCG dictionary (`<< /Type /OCG /Name (...) >>`) registered in
//! the catalog's `/OCProperties /OCGs` array. The default configuration `/D`
//! drives the viewer state: an OCG listed in `/OFF` is **hidden**, one listed in
//! `/Locked` is **locked** (the user can't toggle it), and `/Order` gives the
//! layers-panel order. Showing/hiding and locking/unlocking therefore reduce to
//! adding or removing the OCG's reference from those arrays — exactly the
//! eye/lock toggles the editor UI exposes, now persisted in the PDF.

/// A PDF optional-content layer (calque).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layer {
    /// Object number of the OCG dictionary — the stable id used by the
    /// visibility/lock/remove operations.
    pub id: u32,
    /// Human-readable layer name (`/Name`).
    pub name: String,
    /// Visible in the default configuration (i.e. **not** in `/D /OFF`).
    pub visible: bool,
    /// Locked in the default configuration (in `/D /Locked`).
    pub locked: bool,
    /// Position in the layers panel (`/D /Order`, else discovery order).
    pub order: usize,
}
