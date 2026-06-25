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

/// Open an optional-content marked-content sequence (ISO 32000-1 §8.11.3.2):
/// `/OC /<res_name> BDC`. `res_name` is the resource name (e.g. `OC0`) the
/// OCG is registered under in the page's `/Resources /Properties`; a viewer
/// gates every operator until the matching [`end_ops`](end_ops) (`EMC`) on the
/// referenced group's visibility. `res_name` is an engine-generated `OC{n}`
/// token (ASCII, no delimiters), so it is emitted verbatim after the `/`.
#[must_use]
pub fn begin_ops(res_name: &[u8]) -> Vec<u8> {
    let mut ops = Vec::with_capacity(res_name.len() + 9);
    ops.extend_from_slice(b"/OC /");
    ops.extend_from_slice(res_name);
    ops.extend_from_slice(b" BDC\n");
    ops
}

/// Close the innermost optional-content marked-content sequence: `EMC`
/// (ISO 32000-1 §8.11.3.2). Pairs one-for-one with [`begin_ops`](begin_ops),
/// keeping the `BDC`/`EMC` nesting balanced.
#[must_use]
pub fn end_ops() -> Vec<u8> {
    b"EMC\n".to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn begin_ops_wraps_resource_name_as_oc_property() {
        assert_eq!(begin_ops(b"OC0"), b"/OC /OC0 BDC\n");
        assert_eq!(begin_ops(b"OC12"), b"/OC /OC12 BDC\n");
    }

    #[test]
    fn end_ops_is_bare_emc() {
        assert_eq!(end_ops(), b"EMC\n");
    }
}
