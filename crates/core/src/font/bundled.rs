//! A single compact, permissively-licensed font compiled into the engine as the
//! **universal last-resort fallback** for HTML→PDF / Office→PDF rendering.
//!
//! The engine is zero-network: normally the host downloads the requested family
//! from Google Fonts and hands the bytes back (see [`crate::html::ProvidedFont`]).
//! When the host provides *no* matching font — offline, or an unknown family —
//! rendering would otherwise fall back to rough character-width estimates and
//! base-14 standard fonts (WinAnsi-only, generic metrics). This bundled face
//! gives **real, selectable glyphs and real advance widths** in that case,
//! without any network access.
//!
//! The face is **Liberation Sans Regular** (SIL Open Font License 1.1): a
//! metric-compatible substitute for Arial/Helvetica — the default sans for most
//! HTML — so layout stays faithful. Only the regular weight is bundled to keep
//! the WASM binary small; bold is synthesised (faux-bold) and italic by shear in
//! the paint layer. Host-provided and Google fonts always take precedence; this
//! is consulted only when nothing else matches.
//!
//! License text: `bundled/LICENSE-LiberationSans.txt` (kept alongside the font,
//! as OFL §2 requires). The font is bundled unmodified under its original name.

/// The bundled fallback font program (TrueType, `glyf` outlines + `cmap`).
///
/// Liberation Sans Regular, SIL OFL 1.1. Suitable for both metric measurement
/// (parse via [`crate::font::truetype::TrueTypeFont::parse`]) and PDF embedding
/// (via [`crate::document::Document::embed_truetype_font`]).
pub const FALLBACK_TTF: &[u8] = include_bytes!("bundled/LiberationSans-Regular.ttf");

/// The family name used when embedding the bundled fallback. Deliberately
/// generic so it reads as a substitute rather than claiming a specific family.
pub const FALLBACK_FAMILY: &str = "Fallback Sans";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::font::truetype::TrueTypeFont;

    #[test]
    fn bundled_font_parses_with_real_metrics() {
        let ttf = TrueTypeFont::parse(FALLBACK_TTF).expect("bundled fallback font parses");
        assert!(ttf.units_per_em() > 0.0);
        // It maps common Latin and gives non-zero advances (real glyphs, not a
        // glyphless stub).
        let gid = ttf.gid_for_unicode('A' as u32).expect("'A' is mapped");
        assert!(gid != 0, "'A' resolves to a real glyph id");
        assert!(ttf.advance_width(gid) > 0.0, "'A' has a real advance width");
    }
}
