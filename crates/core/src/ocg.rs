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

// ─── render-time visibility (issue #54) ─────────────────────────────────────
//
// Reading the default configuration `/OCProperties /D` (§8.11.4.3) and an
// optional-content **membership** dictionary `/OCMD` (§8.11.2.2) reduces to two
// object-graph-free decisions, factored here so both the reader (which resolves
// the references) and its tests share one source of truth:
//   * the **base** default visibility of an OCG from `/BaseState`, and
//   * the **policy** that combines several OCGs' visibilities into the single
//     visible/hidden verdict an `/OCMD /P` (or the `/OC` of an XObject) yields.

/// An `/OCMD` visibility **policy** (`/P`, ISO 32000-1 §8.11.2.2, Table 99):
/// how the member OCGs' individual ON/OFF states combine into one verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OcPolicy {
    /// `/AnyOn` — visible if **any** member OCG is ON (the default).
    #[default]
    AnyOn,
    /// `/AllOn` — visible only if **all** member OCGs are ON.
    AllOn,
    /// `/AnyOff` — visible if **any** member OCG is OFF.
    AnyOff,
    /// `/AllOff` — visible only if **all** member OCGs are OFF.
    AllOff,
}

impl OcPolicy {
    /// Parse a `/P` policy name; an unknown or absent name yields the `/AnyOn`
    /// default (ISO 32000-1 §8.11.2.2: `/P` defaults to `AnyOn`).
    #[must_use]
    pub fn from_name(name: Option<&[u8]>) -> Self {
        match name {
            Some(b"AllOn") => OcPolicy::AllOn,
            Some(b"AnyOff") => OcPolicy::AnyOff,
            Some(b"AllOff") => OcPolicy::AllOff,
            _ => OcPolicy::AnyOn,
        }
    }
}

/// Resolve an `/OCMD` (or an XObject's `/OC` membership) to a single visible
/// verdict: combine the member OCGs' visibilities (`members[i]` = "OCG i is ON")
/// under `policy` (ISO 32000-1 §8.11.2.2).
///
/// An **empty** membership (no resolvable OCGs) is treated as **visible** — the
/// spec says an OCMD with no groups, or whose `/OCGs` is absent, imposes no
/// visibility constraint, so the content shows.
#[must_use]
pub fn ocmd_visible(members: &[bool], policy: OcPolicy) -> bool {
    if members.is_empty() {
        return true;
    }
    match policy {
        OcPolicy::AnyOn => members.iter().any(|&on| on),
        OcPolicy::AllOn => members.iter().all(|&on| on),
        OcPolicy::AnyOff => members.iter().any(|&on| !on),
        OcPolicy::AllOff => members.iter().all(|&on| !on),
    }
}

/// Default per-OCG visibility implied by the configuration's `/BaseState`
/// (ISO 32000-1 §8.11.4.3, Table 101): `/ON` (the default) ⇒ groups start
/// **visible** unless listed in `/OFF`; `/OFF` ⇒ groups start **hidden** unless
/// listed in `/ON`. Returns the starting visibility before the `/ON`/`/OFF`
/// overrides are applied.
#[must_use]
pub fn base_state_visible(base_state: Option<&[u8]>) -> bool {
    // Only an explicit `/OFF` flips the default; anything else (incl. `/ON` and
    // unknown values) keeps groups visible by default.
    base_state != Some(b"OFF")
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

    #[test]
    fn oc_policy_parses_with_anyon_default() {
        assert_eq!(OcPolicy::from_name(Some(b"AnyOn")), OcPolicy::AnyOn);
        assert_eq!(OcPolicy::from_name(Some(b"AllOn")), OcPolicy::AllOn);
        assert_eq!(OcPolicy::from_name(Some(b"AnyOff")), OcPolicy::AnyOff);
        assert_eq!(OcPolicy::from_name(Some(b"AllOff")), OcPolicy::AllOff);
        // Absent or unknown ⇒ AnyOn (the spec default).
        assert_eq!(OcPolicy::from_name(None), OcPolicy::AnyOn);
        assert_eq!(OcPolicy::from_name(Some(b"Bogus")), OcPolicy::AnyOn);
        assert_eq!(OcPolicy::default(), OcPolicy::AnyOn);
    }

    #[test]
    fn ocmd_policies_combine_member_states() {
        // [on, off]
        let mixed = [true, false];
        assert!(ocmd_visible(&mixed, OcPolicy::AnyOn)); // some ON
        assert!(!ocmd_visible(&mixed, OcPolicy::AllOn)); // not all ON
        assert!(ocmd_visible(&mixed, OcPolicy::AnyOff)); // some OFF
        assert!(!ocmd_visible(&mixed, OcPolicy::AllOff)); // not all OFF

        // [on, on]
        let both_on = [true, true];
        assert!(ocmd_visible(&both_on, OcPolicy::AnyOn));
        assert!(ocmd_visible(&both_on, OcPolicy::AllOn));
        assert!(!ocmd_visible(&both_on, OcPolicy::AnyOff));
        assert!(!ocmd_visible(&both_on, OcPolicy::AllOff));

        // [off, off]
        let both_off = [false, false];
        assert!(!ocmd_visible(&both_off, OcPolicy::AnyOn));
        assert!(!ocmd_visible(&both_off, OcPolicy::AllOn));
        assert!(ocmd_visible(&both_off, OcPolicy::AnyOff));
        assert!(ocmd_visible(&both_off, OcPolicy::AllOff));
    }

    #[test]
    fn ocmd_empty_membership_is_visible() {
        // No constraining groups ⇒ content shows, under every policy.
        for p in [
            OcPolicy::AnyOn,
            OcPolicy::AllOn,
            OcPolicy::AnyOff,
            OcPolicy::AllOff,
        ] {
            assert!(ocmd_visible(&[], p));
        }
    }

    #[test]
    fn base_state_off_flips_default_to_hidden() {
        assert!(base_state_visible(None)); // default /ON
        assert!(base_state_visible(Some(b"ON")));
        assert!(!base_state_visible(Some(b"OFF")));
        assert!(base_state_visible(Some(b"Weird"))); // unknown ⇒ visible
    }
}
